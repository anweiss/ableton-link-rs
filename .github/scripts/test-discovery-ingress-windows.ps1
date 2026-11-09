param([string]$TestBinary, [switch]$Churn)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'Requires a disposable GitHub Actions runner' }
# New-NetIPAddress disables DHCP. Use the address-only IP Helper operation so
# the runner's existing DHCP address/default route remain untouched.
Add-Type @'
using System;
using System.Net;
using System.Runtime.InteropServices;
public static class FixtureAddress {
    [StructLayout(LayoutKind.Explicit, Size = 80)]
    public struct Row {
        [FieldOffset(0)] public ushort Family;
        [FieldOffset(4)] public uint Address;
        [FieldOffset(32)] public ulong Luid;
        [FieldOffset(40)] public uint Index;
        [FieldOffset(44)] public uint PrefixOrigin;
        [FieldOffset(48)] public uint SuffixOrigin;
        [FieldOffset(52)] public uint ValidLifetime;
        [FieldOffset(56)] public uint PreferredLifetime;
        [FieldOffset(60)] public byte Prefix;
        [FieldOffset(61)] public byte SkipAsSource;
        [FieldOffset(64)] public uint DadState;
        [FieldOffset(68)] public uint ScopeId;
        [FieldOffset(72)] public long CreationTimeStamp;
    }
    [DllImport("iphlpapi.dll")] static extern void InitializeUnicastIpAddressEntry(out Row row);
    [DllImport("iphlpapi.dll")] static extern uint CreateUnicastIpAddressEntry(ref Row row);
    [DllImport("iphlpapi.dll")] static extern uint DeleteUnicastIpAddressEntry(ref Row row);
    [DllImport("iphlpapi.dll")] static extern uint ConvertInterfaceIndexToLuid(uint index, out ulong luid);
    public static void Set(uint index, string address, bool add) {
        Row row;
        InitializeUnicastIpAddressEntry(out row);
        row.Family = 2;
        row.Address = BitConverter.ToUInt32(IPAddress.Parse(address).GetAddressBytes(), 0);
        row.Index = index;
        uint lookup = ConvertInterfaceIndexToLuid(index, out row.Luid);
        if (lookup != 0) throw new System.ComponentModel.Win32Exception((int)lookup);
        row.Prefix = 24;
        row.SkipAsSource = 1;
        row.ValidLifetime = UInt32.MaxValue;
        row.PreferredLifetime = UInt32.MaxValue;
        uint error = add ? CreateUnicastIpAddressEntry(ref row) : DeleteUnicastIpAddressEntry(ref row);
        if (error != 0) throw new System.ComponentModel.Win32Exception((int)error);
    }
}
'@
function Wait-FixtureAddresses {
    $deadline = (Get-Date).AddSeconds(20)
    do {
        $addresses = @(Get-NetIPAddress -AddressFamily IPv4 | Where-Object IPAddress -like '10.42.0.*')
        if (@($addresses | Where-Object AddressState -ne Preferred).Count -eq 0) { break }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    $addresses | Format-Table IPAddress, InterfaceIndex, AddressState, ValidLifetime
    if (@($addresses | Where-Object AddressState -ne Preferred).Count) { throw 'Fixture addresses did not become usable after DAD' }
}
function Invoke-Pktmon {
    param([string[]]$Arguments)
    $output = & pktmon @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "pktmon $($Arguments -join ' ') failed: $($output -join "`n")"
    }
    $output
}
. (Join-Path $PSScriptRoot 'discovery-ingress-pktmon.ps1')
if ($Churn) {
    if ($env:LINK_154_ADAPTER_FIXTURE -ne '1') { throw 'Fixture not active' }
    $index = [int]$env:LINK_154_ADAPTER_A
    [FixtureAddress]::Set($index, '10.42.0.1', $false)
    [FixtureAddress]::Set($index, '10.42.0.1', $true)
    Wait-FixtureAddresses
    exit
}
if (!(Test-Path $TestBinary -PathType Leaf)) { throw 'Missing test binary' }
if (@(Get-NetIPAddress -AddressFamily IPv4 | Where-Object IPAddress -like '10.42.0.*').Count) {
    throw 'Refusing to alter pre-existing fixture addresses'
}
$created = @()
$switches = @()
$networks = @()
$captureStarted = $false
$captureArtifactsOwned = $false
$captureEtl = [System.IO.Path]::GetFullPath((Join-Path (Get-Location).Path '.link154-pktmon.etl'))
$captureText = [System.IO.Path]::GetFullPath((Join-Path (Get-Location).Path '.link154-pktmon.txt'))
$rule = 'ableton-link-154-fixture'
$env:LINK_154_DUPLICATE_REJECTED = ''
$evidenceMissing = $null
$evidenceVerified = $false
try {
    Get-NetAdapter | Format-Table Name, ifIndex, Status
    Get-NetIPInterface -AddressFamily IPv4 | Format-Table InterfaceAlias, InterfaceIndex, ConnectionState
    # Do not assume an Up adapter has an IPv4 interface in this compartment.
    # Provision private networks instead of changing runner transport adapters.
    if (Get-Command New-VMSwitch -ErrorAction SilentlyContinue) {
        foreach ($name in @('link154-a', 'link154-b')) {
            if (Get-VMSwitch -Name $name -ErrorAction SilentlyContinue) { throw "Existing switch $name" }
            New-VMSwitch -Name $name -SwitchType Internal | Out-Null
            $switches += $name
        }
        $a = (Get-NetAdapter -Name 'vEthernet (link154-a)').ifIndex
        $b = (Get-NetAdapter -Name 'vEthernet (link154-b)').ifIndex
    } elseif (Get-Command New-HnsNetwork -ErrorAction SilentlyContinue) {
        foreach ($suffix in @('a', 'b')) {
            $name = "link154-$suffix"
            if (Get-HnsNetwork | Where-Object Name -eq $name) { throw "Existing network $name" }
            $octet = if ($suffix -eq 'a') { 154 } else { 155 }
            $network = New-HnsNetwork -Name $name -Type NAT -AddressPrefix "10.254.$octet.0/24" -Gateway "10.254.$octet.1"
            $networks += $network
        }
        $a = (Get-NetIPAddress -IPAddress '10.254.154.1').InterfaceIndex
        $b = (Get-NetIPAddress -IPAddress '10.254.155.1').InterfaceIndex
    } else {
        throw 'Runner cannot provision isolated adapters: neither New-VMSwitch nor New-HnsNetwork is available'
    }
    $assignments = @(
        @($a, '10.42.0.1'), @($a, '10.42.0.130'), @($a, '10.42.0.9'),
        @($b, '10.42.0.129'), @($b, '10.42.0.2'), @($b, '10.42.0.9')
    )
    New-NetFirewallRule -Name $rule -DisplayName $rule -Direction Inbound -Action Allow -Protocol UDP -Program $TestBinary | Out-Null
    foreach ($entry in $assignments) {
        Write-Host "Adding $($entry[1]) to interface $($entry[0])"
        try {
            [FixtureAddress]::Set($entry[0], $entry[1], $true)
            $created += ,$entry
        } catch {
            if ($entry[0] -eq $b -and $entry[1] -eq '10.42.0.9' -and $_.Exception.InnerException.NativeErrorCode -eq 5010) {
                # These adapters were created above; changing DHCP on them
                # cannot alter the runner's physical transport. Independently
                # exercise the supported NetTCPIP provider, not just our ABI.
                try {
                    New-NetIPAddress -InterfaceIndex $b -IPAddress '10.42.0.9' -PrefixLength 24 -SkipAsSource $true -PolicyStore ActiveStore | Out-Null
                    $created += ,$entry
                    Write-Host 'NetTCPIP provider accepted the duplicate address; require full network assertions'
                } catch {
                    if ($_.FullyQualifiedErrorId -notmatch 'Windows System Error 5010,New-NetIPAddress$') { throw }
                    $env:LINK_154_DUPLICATE_REJECTED = '5010'
                    Write-Warning 'Native IP Helper and NetTCPIP both rejected duplicate assignment; duplicate network behavior remains unvalidated'
                }
            } else { throw }
        }
    }
    Wait-FixtureAddresses
    $env:LINK_154_ADAPTER_A = "$a"
    $env:LINK_154_ADAPTER_B = "$b"
    $env:LINK_154_ADAPTER_FIXTURE = '1'
    if ($env:LINK_154_DUPLICATE_REJECTED -eq '5010') {
        $evidenceMissing = 'Windows rejected the duplicate address with ERROR_OBJECT_ALREADY_EXISTS/5010; duplicate-address response endpoints and multicast egress were not exercised.'
    } else {
        foreach ($path in @($captureEtl, $captureText)) {
            if (Test-Path $path) { throw "Refusing to overwrite existing capture artifact $path" }
        }
        $captureArtifactsOwned = $true
        try {
            if (!(Get-Command pktmon -ErrorAction SilentlyContinue)) { throw 'pktmon is unavailable' }
            $componentsJson = (Invoke-Pktmon @('list', '--json') | Out-String) | ConvertFrom-Json -AsHashtable
            $adapterAName = (Get-NetAdapter -InterfaceIndex $a).Name
            $adapterBName = (Get-NetAdapter -InterfaceIndex $b).Name
            $componentsA = @(Get-PktmonComponentIds $componentsJson $a $adapterAName)
            $componentsB = @(Get-PktmonComponentIds $componentsJson $b $adapterBName)
            $knownComponents = @(Get-PktmonComponentIds -Document $componentsJson -AllComponents)
            if (!$componentsA.Count -or !$componentsB.Count) {
                throw "pktmon list JSON did not map both fixture adapters to components (A=$a/$adapterAName, B=$b/$adapterBName)"
            }
            # NIC-only capture avoids treating Winsock multicast loopback as adapter egress.
            Invoke-Pktmon @('start', '--capture', '--comp', 'nics', '--type', 'flow', '--pkt-size', '0', '--file-name', $captureEtl) | Out-Null
            $captureStarted = $true
        } catch {
            if (Test-Path $captureEtl) { $captureStarted = $true }
            $evidenceMissing = "Pktmon capture setup unsupported: $($_.Exception.Message)"
        }
    }
    $testOutput = @(& $TestBinary --ignored --exact discovery::messenger::tests::multihomed_adapter_ingress_and_churn --nocapture 2>&1)
    $testExitCode = $LASTEXITCODE
    $testOutput | ForEach-Object { Write-Output $_ }
    if ($testExitCode -ne 0) { throw "Multihomed fixture failed: $testExitCode" }
    if ($env:LINK_154_DUPLICATE_REJECTED -ne '5010') {
        $expectedResult = Get-Link154ExpectedEgress $testOutput $a $b
    }
    if ($captureStarted) {
        try {
            Invoke-Pktmon @('stop') | Out-Null
            $captureStarted = $false
            Invoke-Pktmon @('etl2txt', $captureEtl, '--out', $captureText, '--verbose', '3') | Out-Null
            $captureResult = ConvertFrom-Link154PktmonText (Get-Content $captureText) $expectedResult.Expected $componentsA $componentsB $a $b $knownComponents
            if (!$captureResult.Supported) { throw $captureResult.Reason }
            if ($captureResult.Missing.Count) {
                throw "VALID_CAPTURE_MISSING_EXPECTED_NIC_TX: no NIC-level Tx matched endpoint(s) $($captureResult.Missing -join ', ')"
            }
            if ($captureResult.Misrouted.Count) {
                throw "VALID_CAPTURE_WRONG_NIC_TX: multicast source observed on a wrong NIC: $($captureResult.Misrouted -join ', ')"
            }
            $evidenceVerified = $true
            Write-Host "Pktmon correlated exact source endpoints with NIC-level multicast Tx on both adapters: $($expectedResult.Expected.Endpoint -join ', ')."
        } catch {
            if ($_.Exception.Message -like 'VALID_CAPTURE_MISSING_EXPECTED_NIC_TX:*' -or
                $_.Exception.Message -like 'VALID_CAPTURE_WRONG_NIC_TX:*') { throw }
            $evidenceMissing = "Pktmon egress evidence unavailable: $($_.Exception.Message)"
        }
    }
} finally {
    if ($captureStarted) {
        try {
            Invoke-Pktmon @('stop') | Out-Null
        } catch {
            if (!$evidenceMissing) { $evidenceMissing = "Pktmon stop failed: $($_.Exception.Message)" }
        }
    }
    if ($captureArtifactsOwned) {
        Remove-Item $captureEtl, $captureText -Force -ErrorAction SilentlyContinue
    }
    foreach ($entry in $created) {
        [FixtureAddress]::Set($entry[0], $entry[1], $false)
    }
    if (Get-NetFirewallRule -Name $rule -ErrorAction SilentlyContinue) { Remove-NetFirewallRule -Name $rule }
    foreach ($name in $switches) { Remove-VMSwitch -Name $name -Force }
    foreach ($network in $networks) { $network | Remove-HnsNetwork }
}
if ($evidenceMissing) {
    Write-Output "::warning::LINK-154 MISSING EVIDENCE: $evidenceMissing"
    if ($env:GITHUB_STEP_SUMMARY) {
        Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value "### LINK-154 Windows adapter fixture: SKIPPED — MISSING EVIDENCE`n`n$evidenceMissing`n`nAvailable ingress/churn fixture assertions completed where topology allowed. Duplicate-address evidence was not established; this is not a pass."
    }
    # Preserve prerequisite-skip behavior: other fixture assertions may have
    # passed, but unavailable duplicate-address evidence is never a full pass.
    # A parsed capture missing expected NIC Tx throws above and remains a failure.
    exit 0
}
if ($evidenceVerified -and $env:GITHUB_STEP_SUMMARY) {
    Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value '### LINK-154 Windows adapter fixture: verified distinct duplicate-address response endpoints and exact NIC-level multicast Tx correlation on both adapters.'
}

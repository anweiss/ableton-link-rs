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
        [FieldOffset(60)] public byte Prefix;
        [FieldOffset(61)] public byte SkipAsSource;
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
        uint error = add ? CreateUnicastIpAddressEntry(ref row) : DeleteUnicastIpAddressEntry(ref row);
        if (error != 0) throw new System.ComponentModel.Win32Exception((int)error);
    }
}
'@
if ($Churn) {
    if ($env:LINK_154_ADAPTER_FIXTURE -ne '1') { throw 'Fixture not active' }
    $index = [int]$env:LINK_154_ADAPTER_A
    [FixtureAddress]::Set($index, '10.42.0.1', $false)
    [FixtureAddress]::Set($index, '10.42.0.1', $true)
    Start-Sleep -Seconds 3
    exit
}
if (!(Test-Path $TestBinary -PathType Leaf)) { throw 'Missing test binary' }
if (@(Get-NetIPAddress -AddressFamily IPv4 | Where-Object IPAddress -like '10.42.0.*').Count) {
    throw 'Refusing to alter pre-existing fixture addresses'
}
$created = @()
$switches = @()
$networks = @()
$rule = 'ableton-link-154-fixture'
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
                $env:LINK_154_DUPLICATE_REJECTED = '5010'
                Write-Warning 'Runner rejected duplicate-address setup; this is NOT evidence of duplicate-address network coverage'
            } else { throw }
        }
    }
    Start-Sleep -Seconds 3
    $env:LINK_154_ADAPTER_A = "$a"
    $env:LINK_154_ADAPTER_FIXTURE = '1'
    & $TestBinary --ignored --exact discovery::messenger::tests::multihomed_adapter_ingress_and_churn --nocapture
    if ($LASTEXITCODE -ne 0) { throw "Multihomed fixture failed: $LASTEXITCODE" }
} finally {
    foreach ($entry in $created) {
        [FixtureAddress]::Set($entry[0], $entry[1], $false)
    }
    if (Get-NetFirewallRule -Name $rule -ErrorAction SilentlyContinue) { Remove-NetFirewallRule -Name $rule }
    foreach ($name in $switches) { Remove-VMSwitch -Name $name -Force }
    foreach ($network in $networks) { $network | Remove-HnsNetwork }
}

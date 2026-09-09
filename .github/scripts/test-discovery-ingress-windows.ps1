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
    public static void Set(uint index, string address, bool add) {
        Row row;
        InitializeUnicastIpAddressEntry(out row);
        row.Family = 2;
        row.Address = BitConverter.ToUInt32(IPAddress.Parse(address).GetAddressBytes(), 0);
        row.Index = index;
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
$adapters = @(Get-NetAdapter | Where-Object Status -eq Up | Sort-Object ifIndex)
if ($adapters.Count -lt 2) { throw 'Two active, distinct network adapters are required' }
$a = $adapters[0].ifIndex
$b = $adapters[1].ifIndex
$assignments = @(
    @($a, '10.42.0.1'), @($a, '10.42.0.130'), @($a, '10.42.0.9'),
    @($b, '10.42.0.129'), @($b, '10.42.0.2'), @($b, '10.42.0.9')
)
if (@(Get-NetIPAddress -AddressFamily IPv4 | Where-Object IPAddress -like '10.42.0.*').Count) {
    throw 'Refusing to alter pre-existing fixture addresses'
}
$created = @()
$rule = 'ableton-link-154-fixture'
try {
    New-NetFirewallRule -Name $rule -DisplayName $rule -Direction Inbound -Action Allow -Protocol UDP -Program $TestBinary | Out-Null
    foreach ($entry in $assignments) {
        [FixtureAddress]::Set($entry[0], $entry[1], $true)
        $created += ,$entry
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
    Remove-NetFirewallRule -Name $rule
}

param([string]$TestBinary, [switch]$Churn)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'Requires a disposable GitHub Actions runner' }
if ($Churn) {
    if ($env:LINK_154_ADAPTER_FIXTURE -ne '1') { throw 'Fixture not active' }
    $index = [int]$env:LINK_154_ADAPTER_A
    Remove-NetIPAddress -InterfaceIndex $index -IPAddress 10.42.0.1 -Confirm:$false
    New-NetIPAddress -InterfaceIndex $index -IPAddress 10.42.0.1 -PrefixLength 24 -SkipAsSource $true | Out-Null
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
        New-NetIPAddress -InterfaceIndex $entry[0] -IPAddress $entry[1] -PrefixLength 24 -SkipAsSource $true | Out-Null
        $created += ,$entry
    }
    Start-Sleep -Seconds 3
    $env:LINK_154_ADAPTER_A = "$a"
    $env:LINK_154_ADAPTER_FIXTURE = '1'
    & $TestBinary --ignored --exact discovery::messenger::tests::multihomed_adapter_ingress_and_churn --nocapture
    if ($LASTEXITCODE -ne 0) { throw "Multihomed fixture failed: $LASTEXITCODE" }
} finally {
    foreach ($entry in $created) {
        Remove-NetIPAddress -InterfaceIndex $entry[0] -IPAddress $entry[1] -Confirm:$false
    }
    Remove-NetFirewallRule -Name $rule
}

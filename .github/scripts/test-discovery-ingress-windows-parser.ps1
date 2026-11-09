$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'discovery-ingress-pktmon.ps1')

function Assert-ParserTest([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw $Message }
}

$componentListJson = @'
{
  "Components": [
    { "ComponentId": 101, "InterfaceIndex": 11, "Name": "vEthernet (link154-a)" },
    { "ComponentId": 2, "SecondaryId": [102], "InterfaceIndex": 12, "Name": "vEthernet (link154-b)" },
    { "ComponentId": 103, "InterfaceIndex": 13, "Name": "Ethernet" }
  ]
}
'@
$componentList = $componentListJson | ConvertFrom-Json -AsHashtable
$componentsA = @(Get-PktmonComponentIds $componentList 11 'vEthernet (link154-a)')
$componentsB = @(Get-PktmonComponentIds $componentList 12 'vEthernet (link154-b)')
Assert-ParserTest ($componentsA -contains 101) 'component list failed to associate adapter A'
Assert-ParserTest ($componentsB -contains 102) 'component list failed to associate adapter B'
$knownComponents = @(Get-PktmonComponentIds -Document $componentList -AllComponents)
Assert-ParserTest ($knownComponents -contains 103 -and $knownComponents -contains 101 -and $knownComponents -contains 102) 'component list failed to retain known non-fixture NIC identities'

$nestedComponents = @'
{
  "Components": [
    { "Id": 201, "Name": "Microsoft Hyper-V Network Adapter #1", "Properties": [{ "Name": "ifIndex", "Value": 11 }] },
    { "Id": 202, "Name": "Microsoft Hyper-V Network Adapter #2", "Properties": [{ "Name": "ifIndex", "Value": 12 }] }
  ]
}
'@ | ConvertFrom-Json -AsHashtable
Assert-ParserTest (@(Get-PktmonComponentIds $nestedComponents 11 'vEthernet (link154-a)') -contains 201) 'nested ifIndex did not associate the parent component with adapter A'
Assert-ParserTest (@(Get-PktmonComponentIds $nestedComponents 12 'vEthernet (link154-b)') -contains 202) 'nested ifIndex did not associate the parent component with adapter B'
Assert-ParserTest (@(Get-PktmonComponentIds $nestedComponents 99 'unrelated alias').Count -eq 0) 'nested component mapping matched an unrelated adapter'

$testOutput = @(
    'test output',
    'LINK154_MULTICAST_TX_EXPECT endpoint=10.42.0.9:53001 ifIndex=11',
    'LINK154_MULTICAST_TX_EXPECT endpoint=10.42.0.9:53002 ifIndex=12'
)
$expectedResult = Get-Link154ExpectedEgress $testOutput 11 12
Assert-ParserTest $expectedResult.Supported 'valid expected-endpoint markers were rejected'

foreach ($invalidMarkers in @(
    @{ Lines = @(); Name = 'missing markers' },
    @{ Lines = @($testOutput[1]); Name = 'one missing marker' },
    @{ Lines = @($testOutput[1], ($testOutput[2] -replace 'ifIndex=12', 'ifIndex=11')); Name = 'duplicate interface' },
    @{ Lines = @($testOutput[1], ($testOutput[2] -replace '53002', '53001')); Name = 'duplicate endpoint' },
    @{ Lines = @($testOutput[1], ($testOutput[2] -replace '53002', '65536')); Name = 'invalid port' }
)) {
    $rejected = $false
    try { Get-Link154ExpectedEgress $invalidMarkers.Lines 11 12 | Out-Null } catch { $rejected = $true }
    Assert-ParserTest $rejected "$($invalidMarkers.Name) did not fail endpoint validation"
}

$headerA = '[00]0000.0000::2026-11-09 10:00:00.000000000 [Microsoft-Windows-PktMon] PktGroupId 100, PktNumber 1, Appearance 1, Direction Tx , Type Ethernet , Component 101, Filter 1, OriginalSize 80, LoggedSize 80'
$headerB = '[00]0000.0000::2026-11-09 10:00:00.000000001 [Microsoft-Windows-PktMon] PktGroupId 101, PktNumber 1, Appearance 1, Direction Tx , Type Ethernet , Component 102, Filter 1, OriginalSize 80, LoggedSize 80'
$packetA = '       ip: 10.42.0.9.53001 > 224.76.78.75.20808: UDP, length 52'
$packetB = '       ip: 10.42.0.9.53002 > 224.76.78.75.20808: UDP, length 52'

$positive = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest $positive.Supported 'documented Pktmon text format was not recognized'
Assert-ParserTest ($positive.Missing.Count -eq 0) 'positive capture did not correlate both endpoints to their NICs'

$ethernetA = '       AA-BB-CC-DD-EE-01 > 01-00-5E-4C-4E-4B, ethertype IPv4 (0x0800), length 94: 10.42.0.9.53001 > 224.76.78.75.20808: [udp sum ok] UDP, length 52'
$ethernetB = @(
    '       AA-BB-CC-DD-EE-02 > 01-00-5E-4C-4E-4B, ethertype IPv4 (0x0800), length 94:',
    '           10.42.0.9.53002 > 224.76.78.75.20808: [bad udp cksum 0x1234 -> 0x5678!] UDP, length 52'
)
$ethernet = ConvertFrom-Link154PktmonText (@($headerA, $ethernetA, $headerB) + $ethernetB) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest ($ethernet.Supported -and $ethernet.Missing.Count -eq 0 -and $ethernet.Misrouted.Count -eq 0) 'Ethernet inline/continuation endpoints or checksum annotations were not parsed'

$noUdp = ConvertFrom-Link154PktmonText @($headerA, '       arp: who-has 10.42.0.129 tell 10.42.0.1', $headerB, '       ip: 10.42.0.1.53000 > 10.42.0.129.80: Flags [S], seq 1, win 65535, length 0') $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest $noUdp.Supported 'valid NIC packet metadata with no UDP was treated as unavailable tooling'
Assert-ParserTest ($noUdp.Missing.Count -eq 2) 'capture without UDP failed to report both missing transmissions'

$wrongPort = ConvertFrom-Link154PktmonText @($headerA, ($packetA -replace '53001', '53099'), $headerB, $packetB) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest $wrongPort.Supported 'wrong-port capture should still be recognized as valid format'
Assert-ParserTest ($wrongPort.Missing.Count -eq 1) 'capture with wrong source port falsely matched endpoint A'

$misrouted = ConvertFrom-Link154PktmonText @(($headerA -replace 'Component 101', 'Component 102'), $packetA, ($headerB -replace 'Component 102', 'Component 101'), $packetB) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest $misrouted.Supported 'cross-adapter capture should still be recognized as valid format'
Assert-ParserTest ($misrouted.Missing.Count -eq 2) 'cross-adapter packets falsely satisfied the expected NIC mappings'
Assert-ParserTest ($misrouted.Misrouted.Count -eq 2) 'cross-adapter transmissions were not reported as routing failures'

$leaked = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB, ($headerA -replace 'Component 101', 'Component 102'), $packetA) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest $leaked.Supported 'a wrong-NIC transmission should remain a recognized capture'
Assert-ParserTest ($leaked.Missing.Count -eq 0) 'leakage control must also include both correct transmissions'
Assert-ParserTest ($leaked.Misrouted.Count -eq 1) 'an additional wrong-NIC transmission was silently accepted'

$foreignOnly = ConvertFrom-Link154PktmonText @(($headerA -replace 'Component 101', 'Component 103'), $packetA, ($headerB -replace 'Component 102', 'Component 103'), $packetB) $expectedResult.Expected $componentsA $componentsB 11 12 $knownComponents
Assert-ParserTest $foreignOnly.Supported 'known foreign-NIC traffic was treated as unavailable tooling'
Assert-ParserTest ($foreignOnly.Missing.Count -eq 2 -and $foreignOnly.Misrouted.Count -eq 2) 'foreign-NIC-only traffic failed to report both missing and misrouted transmissions'

$foreignLeak = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB, ($headerA -replace 'Component 101', 'Component 103'), $packetA) $expectedResult.Expected $componentsA $componentsB 11 12 $knownComponents
Assert-ParserTest ($foreignLeak.Supported -and $foreignLeak.Missing.Count -eq 0 -and $foreignLeak.Misrouted.Count -eq 1) 'an additional known foreign-NIC transmission was silently accepted'

$ambiguous = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB) $expectedResult.Expected @(101, 102) @(102) 11 12
Assert-ParserTest (!$ambiguous.Supported) 'overlapping NIC component mappings were accepted as evidence'

$unmapped = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB, ($headerA -replace 'Component 101', 'Component 999'), $packetA) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest (!$unmapped.Supported) 'target-source traffic on an unmapped NIC was accepted as verified evidence'

$unknownAndAbsent = ConvertFrom-Link154PktmonText @(($headerA -replace 'Component 101', 'Component 999'), $packetA) $expectedResult.Expected $componentsA $componentsB 11 12 $knownComponents
Assert-ParserTest ($unknownAndAbsent.Supported -and $unknownAndAbsent.Missing -contains '10.42.0.9:53002|12') 'unknown-NIC traffic for endpoint A masked an entirely absent endpoint B'

$unknownAndMissing = ConvertFrom-Link154PktmonText @($headerA, $packetA, ($headerA -replace 'Component 101', 'Component 999'), $packetA) $expectedResult.Expected $componentsA $componentsB 11 12 $knownComponents
Assert-ParserTest ($unknownAndMissing.Supported -and $unknownAndMissing.Missing.Count -eq 1) 'an unknown extra copy masked the other endpoint missing from a valid capture'

$unknownOnly = ConvertFrom-Link154PktmonText @(($headerA -replace 'Component 101', 'Component 999'), $packetA, $headerB, $packetB) $expectedResult.Expected $componentsA $componentsB 11 12 $knownComponents
Assert-ParserTest (!$unknownOnly.Supported) 'uncertainty limited to the only unmatched endpoint was treated as verified evidence'

$mixed = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB, ($headerA -replace 'Component 101', 'Component 102'), $packetA, ($headerA -replace 'Component 101', 'Component 999'), $packetA) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest ($mixed.Supported -and $mixed.Misrouted.Count -eq 1) 'an unmapped NIC obscured a proven wrong-fixture-NIC transmission'

$unknownHeader = $headerA -replace 'Direction Tx', 'Direction Unknown'
$stale = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $unknownHeader, $packetB) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest $stale.Supported 'one unrecognized header must not erase the other valid capture records'
Assert-ParserTest ($stale.Missing.Count -eq 1) 'an unrecognized packet header inherited the previous NIC identity'

$unsupported = ConvertFrom-Link154PktmonText @('Packet Group=1 Direction=TX Component=101', 'UDP 10.42.0.9:53001 -> 224.76.78.75:20808') $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest (!$unsupported.Supported) 'unrecognized capture format was treated as valid evidence'

Write-Host 'Windows pktmon parser tests passed: direct/nested component mapping, IP/Ethernet endpoints, checksum annotations, invalid endpoint markers, no UDP, wrong source port, cross-adapter, additional wrong-NIC Tx, foreign-NIC-only Tx, foreign-NIC leakage, ambiguous mapping, per-endpoint unknown-NIC uncertainty, stale header, unsupported format.'

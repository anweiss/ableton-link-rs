$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'discovery-ingress-pktmon.ps1')

function Assert-ParserTest([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw $Message }
}

$componentListJson = @'
{
  "Components": [
    { "ComponentId": 101, "InterfaceIndex": 11, "Name": "vEthernet (link154-a)" },
    { "ComponentId": 2, "SecondaryId": [102], "InterfaceIndex": 12, "Name": "vEthernet (link154-b)" }
  ]
}
'@
$componentList = $componentListJson | ConvertFrom-Json -AsHashtable
$componentsA = @(Get-PktmonComponentIds $componentList 11 'vEthernet (link154-a)')
$componentsB = @(Get-PktmonComponentIds $componentList 12 'vEthernet (link154-b)')
Assert-ParserTest ($componentsA -contains 101) 'component list failed to associate adapter A'
Assert-ParserTest ($componentsB -contains 102) 'component list failed to associate adapter B'

$testOutput = @(
    'test output',
    'LINK154_MULTICAST_TX_EXPECT endpoint=10.42.0.9:53001 ifIndex=11',
    'LINK154_MULTICAST_TX_EXPECT endpoint=10.42.0.9:53002 ifIndex=12'
)
$expectedResult = Get-Link154ExpectedEgress $testOutput 11 12
Assert-ParserTest $expectedResult.Supported 'valid expected-endpoint markers were rejected'

$headerA = '[00]0000.0000::2026-11-09 10:00:00.000000000 [Microsoft-Windows-PktMon] PktGroupId 100, PktNumber 1, Appearance 1, Direction Tx , Type Ethernet , Component 101, Filter 1, OriginalSize 80, LoggedSize 80'
$headerB = '[00]0000.0000::2026-11-09 10:00:00.000000001 [Microsoft-Windows-PktMon] PktGroupId 101, PktNumber 1, Appearance 1, Direction Tx , Type Ethernet , Component 102, Filter 1, OriginalSize 80, LoggedSize 80'
$packetA = '       ip: 10.42.0.9.53001 > 224.76.78.75.20808: UDP, length 52'
$packetB = '       ip: 10.42.0.9.53002 > 224.76.78.75.20808: UDP, length 52'

$positive = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest $positive.Supported 'documented Pktmon text format was not recognized'
Assert-ParserTest ($positive.Missing.Count -eq 0) 'positive capture did not correlate both endpoints to their NICs'

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

$ambiguous = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB) $expectedResult.Expected @(101, 102) @(102) 11 12
Assert-ParserTest (!$ambiguous.Supported) 'overlapping NIC component mappings were accepted as evidence'

$unmapped = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB, ($headerA -replace 'Component 101', 'Component 999'), $packetA) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest (!$unmapped.Supported) 'target-source traffic on an unmapped NIC was accepted as verified evidence'

$mixed = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $packetB, ($headerA -replace 'Component 101', 'Component 102'), $packetA, ($headerA -replace 'Component 101', 'Component 999'), $packetA) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest ($mixed.Supported -and $mixed.Misrouted.Count -eq 1) 'an unmapped NIC obscured a proven wrong-fixture-NIC transmission'

$unknownHeader = $headerA -replace 'Direction Tx', 'Direction Unknown'
$stale = ConvertFrom-Link154PktmonText @($headerA, $packetA, $headerB, $unknownHeader, $packetB) $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest $stale.Supported 'one unrecognized header must not erase the other valid capture records'
Assert-ParserTest ($stale.Missing.Count -eq 1) 'an unrecognized packet header inherited the previous NIC identity'

$unsupported = ConvertFrom-Link154PktmonText @('Packet Group=1 Direction=TX Component=101', 'UDP 10.42.0.9:53001 -> 224.76.78.75:20808') $expectedResult.Expected $componentsA $componentsB 11 12
Assert-ParserTest (!$unsupported.Supported) 'unrecognized capture format was treated as valid evidence'

Write-Host 'Windows pktmon parser tests passed: positive, wrong source port, cross-adapter, additional wrong-NIC Tx, ambiguous mapping, unmapped NIC, stale header, unsupported format.'

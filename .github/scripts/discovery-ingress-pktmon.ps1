function Get-PktmonComponentIds {
    param($Document, [int]$InterfaceIndex, [string]$AdapterName)
    $ids = [System.Collections.Generic.HashSet[int]]::new()
    $visit = {
        param($item)
        if ($null -eq $item) { return }
        if ($item -is [System.Collections.IDictionary]) {
            $itemIndex = $null
            $itemName = $null
            $itemIds = @()
            foreach ($key in $item.Keys) {
                switch -Regex ($key.ToString()) {
                    '^(InterfaceIndex|IfIndex)$' { $itemIndex = $item[$key] }
                    '^(Name|AdapterName|InterfaceName)$' { $itemName = $item[$key] }
                    '^(ComponentId|CompId|Id|SecondaryId)$' {
                        $value = $item[$key]
                        if ($value -is [System.Collections.IEnumerable] -and $value -isnot [string]) {
                            $itemIds += @($value)
                        } else {
                            $itemIds += $value
                        }
                    }
                }
            }
            $matchesInterface = $null -ne $itemIndex -and [int]$itemIndex -eq $InterfaceIndex
            $matchesName = $itemName -is [string] -and $itemName -like "*$AdapterName*"
            if ($matchesInterface -or $matchesName) {
                foreach ($itemId in $itemIds) {
                    $parsedId = 0
                    if ([int]::TryParse([string]$itemId, [ref]$parsedId)) { [void]$ids.Add($parsedId) }
                }
            }
            foreach ($value in $item.Values) { & $visit $value }
        } elseif ($item -is [System.Collections.IEnumerable] -and $item -isnot [string]) {
            foreach ($value in $item) { & $visit $value }
        }
    }
    & $visit $Document
    @($ids | Sort-Object)
}

function Get-Link154ExpectedEgress {
    param([object[]]$TestOutput, [int]$InterfaceA, [int]$InterfaceB)
    $expected = @()
    foreach ($line in $TestOutput) {
        if ("$line" -match 'LINK154_MULTICAST_TX_EXPECT endpoint=(\d{1,3}(?:\.\d{1,3}){3}):(\d+) ifIndex=(\d+)') {
            $address = $Matches[1]
            $port = 0
            $interfaceIndex = 0
            if ($address -ne '10.42.0.9' -or
                ![int]::TryParse($Matches[2], [ref]$port) -or
                ![int]::TryParse($Matches[3], [ref]$interfaceIndex) -or
                $port -lt 1 -or $port -gt 65535) {
                return [pscustomobject]@{ Supported = $false; Reason = "Malformed multicast expectation marker: $line"; Expected = @() }
            }
            $expected += [pscustomobject]@{
                Address = $address
                Port = $port
                InterfaceIndex = $interfaceIndex
                Endpoint = "${address}:$port"
            }
        }
    }
    if ($expected.Count -ne 2 -or
        @($expected | Where-Object Address -ne '10.42.0.9').Count -ne 0 -or
        @($expected | Select-Object -ExpandProperty InterfaceIndex -Unique).Count -ne 2 -or
        @($expected | Select-Object -ExpandProperty Endpoint -Unique).Count -ne 2 -or
        @($expected | Where-Object { $_.InterfaceIndex -notin @($InterfaceA, $InterfaceB) }).Count -ne 0) {
        return [pscustomobject]@{
            Supported = $false
            Reason = "Expected exactly one distinct 10.42.0.9 endpoint marker per fixture adapter; found $($expected.Count)"
            Expected = $expected
        }
    }
    [pscustomobject]@{ Supported = $true; Reason = $null; Expected = $expected }
}

function ConvertFrom-Link154PktmonText {
    param(
        [string[]]$Lines,
        [object[]]$Expected,
        [int[]]$ComponentsA,
        [int[]]$ComponentsB,
        [int]$InterfaceA,
        [int]$InterfaceB
    )
    $snapshots = 0
    $udpPackets = 0
    $matched = [System.Collections.Generic.HashSet[string]]::new()
    $packet = $null
    foreach ($line in $Lines) {
        if ("$line" -match 'PktGroupId\s+(\d+),\s*PktNumber\s+(\d+),.*Direction\s+(Tx|Rx)\s*,.*Component\s+(\d+)') {
            $packet = @{
                Group = "$($Matches[1]):$($Matches[2])"
                Direction = $Matches[3]
                Component = [int]$Matches[4]
            }
            $snapshots++
            continue
        }
        if ("$line" -match '(?:Drop:\s*)?ip:\s*(\d{1,3}(?:\.\d{1,3}){3})\.(\d+)\s*>\s*(\d{1,3}(?:\.\d{1,3}){3})\.(\d+):\s*UDP(?:,|\s)') {
            $udpPackets++
            if ($null -eq $packet -or $packet.Direction -ne 'Tx') { continue }
            $source = "$($Matches[1]):$($Matches[2])"
            $destination = "$($Matches[3]):$($Matches[4])"
            if ($destination -ne '224.76.78.75:20808') { continue }
            foreach ($endpoint in $Expected) {
                if ($source -ne $endpoint.Endpoint) { continue }
                if ($endpoint.InterfaceIndex -eq $InterfaceA) {
                    $components = $ComponentsA
                } elseif ($endpoint.InterfaceIndex -eq $InterfaceB) {
                    $components = $ComponentsB
                } else {
                    continue
                }
                if ($components -contains $packet.Component) {
                    [void]$matched.Add("$($endpoint.Endpoint)|$($endpoint.InterfaceIndex)")
                }
            }
        }
    }
    if ($snapshots -eq 0 -or $udpPackets -eq 0) {
        return [pscustomobject]@{
            Supported = $false
            Reason = 'Pktmon text did not contain the documented metadata and tcpdump-style UDP record format'
            Snapshots = $snapshots
            UdpPackets = $udpPackets
            Matched = @($matched)
        }
    }
    $required = @($Expected | ForEach-Object { "$($_.Endpoint)|$($_.InterfaceIndex)" })
    [pscustomobject]@{
        Supported = $true
        Reason = $null
        Snapshots = $snapshots
        UdpPackets = $udpPackets
        Matched = @($matched | Sort-Object)
        Missing = @($required | Where-Object { !$matched.Contains($_) })
    }
}

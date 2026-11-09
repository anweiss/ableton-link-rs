function Get-PktmonComponentIds {
    param($Document, [int]$InterfaceIndex, [string]$AdapterName, [switch]$AllComponents)
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
                    '^Properties$' {
                        foreach ($property in @($item[$key])) {
                            if ($property -is [System.Collections.IDictionary] -and
                                $property['Name'] -match '^(InterfaceIndex|IfIndex)$') {
                                $itemIndex = $property['Value']
                            }
                        }
                    }
                }
            }
            $matchesInterface = $null -ne $itemIndex -and [int]$itemIndex -eq $InterfaceIndex
            $matchesName = $itemName -is [string] -and $itemName -like "*$AdapterName*"
            if ($AllComponents -or $matchesInterface -or $matchesName) {
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
                throw "Malformed multicast expectation marker: $line"
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
        throw "Expected exactly one distinct 10.42.0.9 endpoint marker per fixture adapter; found $($expected.Count)"
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
        [int]$InterfaceB,
        [int[]]$KnownComponents = @()
    )
    if (!$ComponentsA.Count -or !$ComponentsB.Count -or $InterfaceA -eq $InterfaceB -or
        @($ComponentsA | Where-Object { $ComponentsB -contains $_ }).Count) {
        return [pscustomobject]@{
            Supported = $false
            Reason = 'Fixture NIC component mappings must be nonempty and disjoint, with distinct interface indexes'
        }
    }
    $snapshots = 0
    $udpPackets = 0
    $matched = [System.Collections.Generic.HashSet[string]]::new()
    $misrouted = [System.Collections.Generic.HashSet[string]]::new()
    $unmapped = [System.Collections.Generic.HashSet[int]]::new()
    $unverified = [System.Collections.Generic.HashSet[string]]::new()
    $packet = $null
    foreach ($line in $Lines) {
        if ("$line" -match '\bPktGroupId\b') {
            $packet = $null
            if ("$line" -match 'PktGroupId\s+(\d+),\s*PktNumber\s+(\d+),.*Direction\s+(Tx|Rx)\s*,.*Component\s+(\d+)') {
                $packet = @{
                    Direction = $Matches[3]
                    Component = [int]$Matches[4]
                }
                $snapshots++
            }
            continue
        }
        if ("$line" -match '(?<![\d.])(\d{1,3}(?:\.\d{1,3}){3})\.(\d+)\s*>\s*(\d{1,3}(?:\.\d{1,3}){3})\.(\d+):\s*(?:\[[^\]\r\n]*\]\s*)*UDP(?:,|\s)') {
            $udpPackets++
            $snapshot = $packet
            $packet = $null
            if ($null -eq $snapshot -or $snapshot.Direction -ne 'Tx') { continue }
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
                if ($components -contains $snapshot.Component) {
                    [void]$matched.Add("$($endpoint.Endpoint)|$($endpoint.InterfaceIndex)")
                } elseif ($ComponentsA -contains $snapshot.Component -or
                    $ComponentsB -contains $snapshot.Component -or
                    $KnownComponents -contains $snapshot.Component) {
                    [void]$misrouted.Add("$source|expected=$($endpoint.InterfaceIndex)|component=$($snapshot.Component)")
                } else {
                    [void]$unmapped.Add($snapshot.Component)
                    [void]$unverified.Add("$($endpoint.Endpoint)|$($endpoint.InterfaceIndex)")
                }
            }
        }
    }
    $required = @($Expected | ForEach-Object { "$($_.Endpoint)|$($_.InterfaceIndex)" })
    $unobserved = @($required | Where-Object { !$matched.Contains($_) -and !$unverified.Contains($_) })
    if ($unmapped.Count -and !$misrouted.Count -and !$unobserved.Count) {
        return [pscustomobject]@{
            Supported = $false
            Reason = "Expected multicast source appeared on unmapped NIC component(s): $(@($unmapped | Sort-Object) -join ', ')"
        }
    }
    if ($snapshots -eq 0) {
        return [pscustomobject]@{
            Supported = $false
            Reason = 'Pktmon text did not contain the documented packet metadata'
            Snapshots = $snapshots
            UdpPackets = $udpPackets
            Matched = @($matched)
        }
    }
    [pscustomobject]@{
        Supported = $true
        Reason = $null
        Snapshots = $snapshots
        UdpPackets = $udpPackets
        Matched = @($matched | Sort-Object)
        Missing = @($required | Where-Object { !$matched.Contains($_) })
        Misrouted = @($misrouted | Sort-Object)
        Unverified = @($unverified | Sort-Object)
    }
}

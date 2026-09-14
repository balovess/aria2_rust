[CmdletBinding()]
param(
    [string]$Original = $env:ARIA2_ORIGINAL_BIN,
    [string]$Current = (Join-Path $PSScriptRoot '..\target\debug\aria2c.exe'),
    [switch]$BuildCurrent,
    [switch]$KeepArtifacts
)

$ErrorActionPreference = 'Stop'

function Assert-That([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "DIFF FAILED: $Message" }
}

function Get-ObjectKeys($Value) {
    @($Value.PSObject.Properties.Name | Sort-Object)
}

function Assert-SameKeys($Left, $Right, [string]$Label) {
    $leftKeys = @(Get-ObjectKeys $Left)
    $rightKeys = @(Get-ObjectKeys $Right)
    Assert-That (($leftKeys -join '|') -eq ($rightKeys -join '|')) `
        "$Label keys differ: [$($leftKeys -join ', ')] vs [$($rightKeys -join ', ')]"
}

function Assert-StringFields($Value, [string[]]$Fields, [string]$Label) {
    foreach ($field in $Fields) {
        $item = $Value.PSObject.Properties[$field]
        Assert-That ($null -ne $item) "$Label is missing '$field'"
        Assert-That ($item.Value -is [string]) "$Label.$field is not a string"
    }
}

function Invoke-JsonRpc([string]$Base, [string]$Method, $Params) {
    $request = @{ jsonrpc = '2.0'; id = $Method; method = $Method; params = $Params }
    $body = $request | ConvertTo-Json -Depth 20 -Compress
    $response = Invoke-WebRequest -Uri "$Base/jsonrpc" -Method Post `
        -ContentType 'application/json' -Body $body -UseBasicParsing
    $response.Content | ConvertFrom-Json
}

function Invoke-XmlRpc([string]$Base, [string]$Method, [string]$ParamsXml = '') {
    $body = "<?xml version=`"1.0`"?><methodCall><methodName>$Method</methodName><params>$ParamsXml</params></methodCall>"
    $response = Invoke-WebRequest -Uri "$Base/rpc" -Method Post `
        -ContentType 'text/xml' -Body $body -UseBasicParsing
    $response.Content
}

function Invoke-WebSocketJson([string]$Base, [string]$Method, $Params) {
    $uri = [Uri]::new(($Base -replace '^http', 'ws') + '/jsonrpc')
    $socket = [Net.WebSockets.ClientWebSocket]::new()
    $cancel = [Threading.CancellationToken]::None
    try {
        $socket.ConnectAsync($uri, $cancel).GetAwaiter().GetResult()
        $request = @{ jsonrpc = '2.0'; id = $Method; method = $Method; params = $Params } |
            ConvertTo-Json -Depth 20 -Compress
        $bytes = [Text.Encoding]::UTF8.GetBytes($request)
        $socket.SendAsync([ArraySegment[byte]]::new($bytes), [Net.WebSockets.WebSocketMessageType]::Text, $true, $cancel).GetAwaiter().GetResult()
        $buffer = New-Object byte[] 65536
        $stream = [IO.MemoryStream]::new()
        do {
            $result = $socket.ReceiveAsync([ArraySegment[byte]]::new($buffer), $cancel).GetAwaiter().GetResult()
            if ($result.MessageType -eq [Net.WebSockets.WebSocketMessageType]::Close) { throw 'WebSocket closed before response' }
            $stream.Write($buffer, 0, $result.Count)
        } while (-not $result.EndOfMessage)
        [Text.Encoding]::UTF8.GetString($stream.ToArray()) | ConvertFrom-Json
    } finally {
        $socket.Dispose()
    }
}

function New-FreePort {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $port
}

function New-MinimalTorrentBase64 {
    $prefix = [Text.Encoding]::ASCII.GetBytes('d8:announce28:http://127.0.0.1:9/announce4:infod4:name8:test.bin6:lengthi1e12:piece lengthi16384e6:pieces20:')
    $pieces = [byte[]](0..19)
    $suffix = [Text.Encoding]::ASCII.GetBytes('ee')
    $bytes = [byte[]]::new($prefix.Length + $pieces.Length + $suffix.Length)
    [Array]::Copy($prefix, 0, $bytes, 0, $prefix.Length)
    [Array]::Copy($pieces, 0, $bytes, $prefix.Length, $pieces.Length)
    [Array]::Copy($suffix, 0, $bytes, $prefix.Length + $pieces.Length, $suffix.Length)
    [Convert]::ToBase64String($bytes)
}

if ([string]::IsNullOrWhiteSpace($Original)) { $Original = 'aria2c-original.exe' }
if (-not (Test-Path -LiteralPath $Original)) {
    $found = Get-Command $Original -ErrorAction SilentlyContinue
    if ($found) { $Original = $found.Source }
}
Assert-That (Test-Path -LiteralPath $Original) `
    "official aria2c not found; set ARIA2_ORIGINAL_BIN or -Original (received '$Original')"
Assert-That (Test-Path -LiteralPath $Current) "current aria2c not found at '$Current'; build it or pass -Current"

if ($BuildCurrent) {
    & cargo build -p aria2 --features standard --bin aria2c
    Assert-That ($LASTEXITCODE -eq 0) 'cargo build for current aria2c failed'
}

$root = Join-Path ([IO.Path]::GetTempPath()) ('aria2-rpc-diff-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$processes = @()

function Start-Rpc([string]$Path, [string]$Name) {
    $port = New-FreePort
    $dir = Join-Path $root $Name
    New-Item -ItemType Directory -Path $dir | Out-Null
    $stdout = Join-Path $dir 'stdout.log'
    $stderr = Join-Path $dir 'stderr.log'
    $args = @('--no-conf', '--enable-rpc=true', '--rpc-listen-address=127.0.0.1',
        "--rpc-listen-port=$port", "--dir=$dir", '--file-allocation=none')
    $process = Start-Process -FilePath $Path -ArgumentList $args -PassThru `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $script:processes += $process
    $base = "http://127.0.0.1:$port"
    for ($attempt = 0; $attempt -lt 50; $attempt++) {
        Start-Sleep -Milliseconds 100
        try {
            $version = Invoke-JsonRpc $base 'aria2.getVersion' @()
            if ($version.result) { return @{ Base = $base; Dir = $dir; Process = $process } }
        } catch { }
        if ($process.HasExited) { break }
    }
    $errorText = if (Test-Path $stderr) { Get-Content $stderr -Raw } else { '' }
    throw "$Name aria2c did not start on $base. stderr: $errorText"
}

function Compare-Success($Left, $Right, [string]$Label) {
    Assert-That ($null -eq $Left.error -and $null -eq $Right.error) "$Label unexpectedly returned an error"
    Assert-That ($Left.result.GetType().Name -eq $Right.result.GetType().Name) "$Label result types differ"
}

try {
    $originalRpc = Start-Rpc $Original 'original'
    $currentRpc = Start-Rpc $Current 'current'

    Write-Host '== JSON-RPC =='
    $left = Invoke-JsonRpc $originalRpc.Base 'aria2.getVersion' @()
    $right = Invoke-JsonRpc $currentRpc.Base 'aria2.getVersion' @()
    Compare-Success $left $right 'getVersion'
    Assert-SameKeys $left.result $right.result 'getVersion'
    Assert-That ($left.result.enabledFeatures -is [array] -and $right.result.enabledFeatures -is [array]) 'enabledFeatures must be arrays'

    $left = Invoke-JsonRpc $originalRpc.Base 'aria2.getGlobalStat' @()
    $right = Invoke-JsonRpc $currentRpc.Base 'aria2.getGlobalStat' @()
    Compare-Success $left $right 'getGlobalStat'
    Assert-SameKeys $left.result $right.result 'getGlobalStat'
    Assert-StringFields $left.result (Get-ObjectKeys $left.result) 'original getGlobalStat'
    Assert-StringFields $right.result (Get-ObjectKeys $right.result) 'current getGlobalStat'

    Write-Host '== JSON-RPC task status and field absence =='
    $uri = 'http://127.0.0.1:9/differential.bin'
    $left = Invoke-JsonRpc $originalRpc.Base 'aria2.addUri' @(@($uri), @{ pause = 'true' })
    $right = Invoke-JsonRpc $currentRpc.Base 'aria2.addUri' @(@($uri), @{ pause = 'true' })
    Compare-Success $left $right 'addUri'
    Assert-That ($left.result -is [string] -and $right.result -is [string]) 'addUri must return string GIDs'
    $leftStatus = Invoke-JsonRpc $originalRpc.Base 'aria2.tellStatus' @($left.result)
    $rightStatus = Invoke-JsonRpc $currentRpc.Base 'aria2.tellStatus' @($right.result)
    Compare-Success $leftStatus $rightStatus 'tellStatus'
    Assert-SameKeys $leftStatus.result $rightStatus.result 'tellStatus'
    foreach ($status in @($leftStatus.result, $rightStatus.result)) {
        Assert-StringFields $status @('gid', 'status', 'totalLength', 'completedLength', 'downloadSpeed', 'uploadSpeed') 'tellStatus'
    }
    $filteredOriginal = Invoke-JsonRpc $originalRpc.Base 'aria2.tellStatus' @($left.result, @('gid', 'completedPieces', 'missingPieces', 'source'))
    $filteredCurrent = Invoke-JsonRpc $currentRpc.Base 'aria2.tellStatus' @($right.result, @('gid', 'completedPieces', 'missingPieces', 'source'))
    foreach ($filtered in @($filteredOriginal, $filteredCurrent)) {
        Assert-That ($null -eq $filtered.error) 'tellStatus field projection failed'
        $filteredKeys = @(Get-ObjectKeys $filtered.result)
        Assert-That (($filteredKeys -join '|') -eq 'gid') 'tellStatus field absence projection leaked a field'
    }

    Write-Host '== Error codes =='
    $errorCases = @(
        @{ Method = 'aria2.unknownDifferentialMethod'; Params = @() },
        @{ Method = 'aria2.addUri'; Params = @() },
        @{ Method = 'aria2.tellStatus'; Params = @('0000000000000000') }
    )
    foreach ($case in $errorCases) {
        $left = Invoke-JsonRpc $originalRpc.Base $case.Method $case.Params
        $right = Invoke-JsonRpc $currentRpc.Base $case.Method $case.Params
        Assert-That ($null -ne $left.error -and $null -ne $right.error) "$($case.Method) must fail on both implementations"
        Assert-That ($left.error.code -eq $right.error.code) "$($case.Method) error codes differ: $($left.error.code) vs $($right.error.code)"
    }

    Write-Host '== XML-RPC =='
    $leftXml = Invoke-XmlRpc $originalRpc.Base 'aria2.getVersion'
    $rightXml = Invoke-XmlRpc $currentRpc.Base 'aria2.getVersion'
    Assert-That ($leftXml.Contains('<methodResponse>') -and $rightXml.Contains('<methodResponse>')) 'XML-RPC response envelope differs'
    Assert-That (($leftXml -match '<name>version</name>') -and ($rightXml -match '<name>version</name>')) 'XML-RPC getVersion lacks version member'
    $leftXml = Invoke-XmlRpc $originalRpc.Base 'aria2.addUri' '<param><value><array><data></data></array></value></param>'
    $rightXml = Invoke-XmlRpc $currentRpc.Base 'aria2.addUri' '<param><value><array><data></data></array></value></param>'
    Assert-That (($leftXml -match '<fault>') -and ($rightXml -match '<fault>')) 'XML-RPC invalid params must be faults'

    Write-Host '== WebSocket =='
    $leftWs = Invoke-WebSocketJson $originalRpc.Base 'aria2.getVersion' @()
    $rightWs = Invoke-WebSocketJson $currentRpc.Base 'aria2.getVersion' @()
    Compare-Success $leftWs $rightWs 'WebSocket getVersion'
    Assert-SameKeys $leftWs.result $rightWs.result 'WebSocket getVersion'

    Write-Host '== BitTorrent =='
    $torrent = New-MinimalTorrentBase64
    $left = Invoke-JsonRpc $originalRpc.Base 'aria2.addTorrent' @($torrent, @(), @{ pause = 'true' })
    $right = Invoke-JsonRpc $currentRpc.Base 'aria2.addTorrent' @($torrent, @(), @{ pause = 'true' })
    Compare-Success $left $right 'addTorrent'
    $leftStatus = Invoke-JsonRpc $originalRpc.Base 'aria2.tellStatus' @($left.result)
    $rightStatus = Invoke-JsonRpc $currentRpc.Base 'aria2.tellStatus' @($right.result)
    Compare-Success $leftStatus $rightStatus 'BT tellStatus'
    foreach ($status in @($leftStatus.result, $rightStatus.result)) {
        Assert-StringFields $status @('infoHash', 'pieceLength', 'numPieces', 'connections') 'BT tellStatus'
        Assert-That ($null -eq $status.PSObject.Properties['completedPieces']) 'completedPieces leaked into tellStatus'
        Assert-That ($null -eq $status.PSObject.Properties['missingPieces']) 'missingPieces leaked into tellStatus'
    }
    $leftPeers = Invoke-JsonRpc $originalRpc.Base 'aria2.getPeers' @($left.result)
    $rightPeers = Invoke-JsonRpc $currentRpc.Base 'aria2.getPeers' @($right.result)
    Compare-Success $leftPeers $rightPeers 'getPeers'
    Assert-That ($leftPeers.result -is [array] -and $rightPeers.result -is [array]) 'getPeers must return arrays'

    Write-Host 'PASS: official aria2c differential RPC harness'
} finally {
    foreach ($process in $processes) {
        if ($process -and -not $process.HasExited) { $process.Kill() }
        if ($process) { $process.Dispose() }
    }
    if (-not $KeepArtifacts -and (Test-Path $root)) { Remove-Item -LiteralPath $root -Recurse -Force }
    else { Write-Host "Artifacts: $root" }
}

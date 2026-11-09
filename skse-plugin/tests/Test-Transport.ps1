param([string]$Configuration = 'RelWithDebInfo')
$ErrorActionPreference = 'Stop'
$nativeRoot = Split-Path -Parent $PSScriptRoot
$executable = Join-Path $nativeRoot "test-bin/$Configuration/BridgeTransportTest.exe"
$endpoint = [System.Net.IPEndPoint]::new([System.Net.IPAddress]::Loopback, 19463)
$receiver = [System.Net.Sockets.UdpClient]::new($endpoint)
$receiver.Client.ReceiveTimeout = 1500
$packets = [System.Collections.Generic.List[object]]::new()
$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
try {
    $process = Start-Process -FilePath $executable -WindowStyle Hidden -PassThru
    while ($stopwatch.Elapsed.TotalSeconds -lt 12) {
        try {
            $remote = [System.Net.IPEndPoint]::new([System.Net.IPAddress]::Any, 0)
            $bytes = $receiver.Receive([ref]$remote)
            if (-not $remote.Address.Equals([System.Net.IPAddress]::Loopback)) { throw 'Non-loopback sender' }
            $raw = [System.Text.Encoding]::UTF8.GetString($bytes)
            $packet = $raw | ConvertFrom-Json
            $packets.Add([pscustomobject]@{ Seconds = $stopwatch.Elapsed.TotalSeconds; State = $packet; Raw = $raw })
        } catch [System.Net.Sockets.SocketException] {
            if ($_.Exception.SocketErrorCode -ne [System.Net.Sockets.SocketError]::TimedOut) { throw }
        }
        if ($process.HasExited) { break }
    }
    $process.WaitForExit(1000) | Out-Null
    if (-not $process.HasExited -or $process.ExitCode -ne 0) { throw 'Transport harness failed to exit normally' }
    if ($packets.Count -lt 9) { throw "Insufficient heartbeats: $($packets.Count)" }
    $lastSequence = 0
    foreach ($entry in $packets) {
        $packet = $entry.State
        $names = @($packet.PSObject.Properties.Name | Sort-Object)
        $expected = @('gameLoaded','heldLeft','heldRight','menuOpen','pid','schemaVersion','seq','type')
        if (Compare-Object $names $expected) { throw 'Unexpected protocol field set' }
        if ($packet.schemaVersion -ne 1 -or $packet.type -ne 'state' -or $packet.pid -ne $process.Id) { throw 'Invalid protocol identity' }
        if ($packet.seq -le $lastSequence) { throw 'Non-monotonic sequence' }
        if ($packet.menuOpen -isnot [bool] -or $packet.gameLoaded -isnot [bool]) { throw 'Boolean fields have incorrect types' }
        if ($packet.heldLeft -notin @('none','object','actor') -or $packet.heldRight -notin @('none','object','actor')) { throw 'Invalid held enum' }
        $lastSequence = $packet.seq
    }
    if ($packets[0].State.gameLoaded) { throw 'Initial state did not clear feedback' }
    if (-not ($packets | Where-Object { $_.State.menuOpen -and $_.State.gameLoaded -and $_.State.heldLeft -eq 'none' })) { throw 'Menu state missing' }
    $grab = @($packets | Where-Object { $_.State.heldLeft -eq 'object' -and $_.State.heldRight -eq 'actor' })
    if ($grab.Count -lt 2) { throw 'Grab state or its heartbeat missing' }
    $stale = @($packets | Where-Object { $_.Seconds -gt ($grab[0].Seconds + 2.8) -and $_.Seconds -lt 6.9 -and -not $_.State.gameLoaded })
    if ($stale.Count -lt 1) { throw 'Stale cache was not cleared' }
    if (-not ($packets | Where-Object { $_.State.heldLeft -eq 'actor' -and $_.State.heldRight -eq 'object' -and $_.State.menuOpen })) { throw 'Fresh state after expiry missing' }
    $last = $packets[$packets.Count - 1].State
    if ($last.gameLoaded -or $last.menuOpen -or $last.heldLeft -ne 'none' -or $last.heldRight -ne 'none') { throw 'Final reset did not clear all state' }
    $result = [pscustomobject]@{ Passed = $true; PacketCount = $packets.Count; ProcessId = $process.Id; FirstSeq = $packets[0].State.seq; LastSeq = $last.seq; StaleClearSeconds = $stale[0].Seconds; Packets = $packets }
    $result | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath (Join-Path $nativeRoot 'transport-test-result.json') -Encoding UTF8
    $result | Select-Object Passed, PacketCount, ProcessId, FirstSeq, LastSeq, StaleClearSeconds
} finally {
    $receiver.Dispose()
}

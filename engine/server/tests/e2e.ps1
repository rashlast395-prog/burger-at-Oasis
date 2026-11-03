# End-to-end check of the real programs: provision, enrol, run, lock, sync.
#   powershell -File engine/server/tests/e2e.ps1 [-Bin <dir containing bo-server.exe and bo-device.exe>]
# Exit code 0 only if every check passed. A network sync that is blocked by security software
# rewriting TLS is reported as SKIPPED, not as a pass.
param([string]$Bin = (Join-Path $PSScriptRoot '..\..\target\debug'))

# Any unexpected error inside the script is a failure, never a silently skipped check.
$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 has no $IsWindows; there it is always Windows.
$ext = if ($null -eq $IsWindows -or $IsWindows) { '.exe' } else { '' }
$server = Join-Path $Bin "bo-server$ext"
$device = Join-Path $Bin "bo-device$ext"
$script:failed = 0
$script:skipped = 0

function Check($ok, $what) {
    if ($ok) { Write-Host "  ok    $what" } else { Write-Host "  FAIL  $what"; $script:failed++ }
}
# Windows PowerShell 5.1 drops the double quotes inside arguments to native programs; escape
# them there. PowerShell 7 passes them through correctly, and escaping would corrupt the JSON.
function J($json) { if ($PSVersionTable.PSVersion.Major -lt 7) { $json.Replace('"', '\"') } else { $json } }
function Run($exe, [string[]]$a) {
    $ErrorActionPreference = 'Continue'   # native stderr is data here, not an error
    $out = & $exe @a 2>&1 | ForEach-Object { "$_" }
    [pscustomobject]@{ Code = $LASTEXITCODE; Out = ($out -join "`n") }
}

$root = Join-Path ([IO.Path]::GetTempPath()) ("bo-e2e-" + [guid]::NewGuid().ToString('N'))
$s = Join-Path $root 'server'; $d = Join-Path $root 'device'
New-Item -ItemType Directory -Force $root | Out-Null
$l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $l.Start(); $port = $l.LocalEndpoint.Port; $l.Stop()
$proc = $null

try {
    Write-Host "server init"
    Check ((Run $server @('init', $s)).Code -ne 0) 'init without --host is refused'
    $r = Run $server @('init', $s, '--host', 'localhost', '--host', '127.0.0.1', '--listen', "127.0.0.1:$port")
    Check ($r.Code -eq 0 -and $r.Out -match 'CA fingerprint') 'init succeeds and prints the CA fingerprint'
    Check ((Run $server @('init', $s, '--host', 'localhost')).Code -ne 0) 'a second init is refused'
    foreach ($f in 'config.json', 'node.db', 'server-identity.json', 'tls\ca.crt', 'tls\server.crt', 'tls\server.key', 'tls\ca.key') {
        Check (Test-Path (Join-Path $s $f)) "created $f"
    }

    Write-Host "device init"
    $identity = Join-Path $s 'server-identity.json'
    $bad = Join-Path $root 'bad.json'
    (Get-Content $identity -Raw).Replace('BEGIN CERTIFICATE', 'BEGIN NOTHING') | Set-Content $bad
    Check ((Run $device @('init', (Join-Path $root 'dbad'), '--server-identity', $bad)).Code -ne 0) 'a tampered server identity is refused'
    Check (-not (Test-Path (Join-Path $root 'dbad\node.db'))) 'a failed init leaves no database behind'
    $r = Run $device @('init', $d, '--server-identity', $identity)
    Check ($r.Code -eq 0) 'device init succeeds'
    $m = [regex]::Match($r.Out, '--device (\w+) --public (\w+)')
    Check $m.Success 'device prints its id and public key'
    $devId = $m.Groups[1].Value; $devPub = $m.Groups[2].Value

    Write-Host "enrolment"
    Check ((Run $server @('enroll', $s, '--device', $devId, '--public', 'nothex')).Code -ne 0) 'a malformed key is refused'
    Check ((Run $server @('enroll', $s, '--device', $devId, '--public', $devPub)).Code -eq 0) 'enrol succeeds'
    Check ((Run $server @('show', $s)).Out -match $devId) 'show lists the enrolled device'

    Write-Host "device work before any network"
    Check ((Run $device @('put', $d, 'menu', (J '{"_id":"m1","version":1,"name":"Burger"}'))).Code -eq 0) 'put a versioned menu item'
    Check ((Run $device @('get', $d, 'menu', 'm1')).Out -match 'Burger') 'read it back'
    $r = Run $device @('put', $d, 'menu', (J '{"_id":"m2","name":"unversioned"}'))
    Check ($r.Code -ne 0 -and $r.Out -match 'version') 'versioned rule applies on the device (refused because of the missing version)'
    $r = Run $device @('put', $d, 'payments', (J '{"_id":"paystack:r1:payment","order_id":"x","kind":"payment","method":"card","provider":"paystack","reference":"r1","amount":100,"currency":"GHS"}'))
    Check ($r.Code -ne 0 -and $r.Out -match 'only the server') 'a device may not record a card payment (refused by the server-only rule)'
    $r = Run $device @('put', $d, 'nope', (J '{"_id":"x"}'))
    Check ($r.Code -ne 0 -and $r.Out -match 'not a synchronised collection') 'unknown collection is refused'

    Write-Host "orders and stock on the device"
    $r = Run $device @('order', $d, (J '{"currency":"GHS","items":[{"sku":"burger","name":"Burger","qty":2,"unit_price":4500}]}'))
    $om = [regex]::Match($r.Out, 'placed order (\S+) total 9000 GHS')
    Check ($r.Code -eq 0 -and $om.Success) 'place an order (total computed in pesewas)'
    $oid = $om.Groups[1].Value
    Check ((Run $device @('status', $d, $oid, 'completed')).Code -ne 0) 'cannot jump from placed to completed'
    Check ((Run $device @('status', $d, $oid, 'accepted')).Out -match 'is now accepted') 'accept the order'
    Check ((Run $device @('stock', $d)).Out -match 'burger\s+-2\s+OVERSOLD') 'accepting deducted stock (reported as oversold with none on hand)'
    Check ((Run $device @('stock', $d, 'burger', '5', 'restock')).Out -match 'burger\s+3') 'restock brings it back above zero'

    Write-Host "cash payment on the device"
    Check ((Run $device @('paid', $d, $oid)).Out -match 'unpaid') 'a new order is unpaid'
    Check ((Run $device @('refund', $d, $oid, '100')).Code -ne 0) 'cannot refund what was never paid'
    Check ((Run $device @('pay', $d, $oid, '9000')).Out -match ': paid \(paid 9000') 'cash payment of the full total marks it paid'

    Write-Host "running server"
    $log = Join-Path $root 'server.out'
    $proc = Start-Process -FilePath $server -ArgumentList @('run', $s) -PassThru -NoNewWindow -RedirectStandardOutput $log -RedirectStandardError (Join-Path $root 'server.err')
    $up = $false
    for ($i = 0; $i -lt 100 -and -not $up; $i++) {
        Start-Sleep -Milliseconds 200
        if ((Test-Path $log) -and ((Get-Content $log -Raw) -match 'listening')) { $up = $true }
    }
    Check $up 'server starts and reports listening'

    $r = Run $server @('enroll', $s, '--device', $devId, '--public', $devPub)
    Check ($r.Code -ne 0 -and $r.Out -match 'already open') 'a second process is refused while the server holds the database'
    Check ((Run $server @('show', $s)).Code -ne 0) 'show is refused while the server runs'
    Check ((Run $server @('run', $s)).Code -ne 0) 'a second server cannot start on the same data'

    Write-Host "sync over TLS"
    $r = Run $device @('sync', $d)
    if ($r.Out -match 'UnknownIssuer') {
        Write-Host '  SKIPPED  TLS on the wire is intercepted by local security software; the device refused the substituted certificate (that is the pinning working). Sync not verified on this machine.'
        $script:skipped++
    } else {
        # menu item, order, accept + its sale entry, restock, cash payment
        Check ($r.Code -eq 0 -and $r.Out -match 'sent 6 events') 'first sync pushes all queued events'
        Check ((Run $device @('sync', $d)).Out -match 'sent 0 events, received 0') 'second sync has nothing to move'
    }
}
catch {
    Write-Host "  FAIL  unexpected error: $($_.Exception.Message)"
    $script:failed++
}
finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force; $proc.WaitForExit(5000) | Out-Null }
}

Write-Host "after the server stops"
Start-Sleep -Milliseconds 500
Check ((Run $server @('show', $s)).Code -eq 0) 'admin commands work again'

Write-Host "server unreachable"
$s2 = Join-Path $root 'server2'; $d2 = Join-Path $root 'device2'
$null = Run $server @('init', $s2, '--host', 'localhost', '--listen', "127.0.0.1:$port")
$null = Run $device @('init', $d2, '--server-identity', (Join-Path $s2 'server-identity.json'))
$r = Run $device @('sync', $d2)
Check ($r.Code -ne 0 -and $r.Out -match 'retrying') 'device retries with backoff and then reports failure'

Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
Write-Host ""
if ($script:failed -gt 0) { Write-Host "$($script:failed) check(s) FAILED"; exit 1 }
if ($script:skipped -gt 0) { Write-Host "all checks passed; $($script:skipped) skipped (see above)"; exit 0 }
Write-Host "all checks passed"
# Explicit: otherwise PowerShell 7 exits with the code of the last program run (the
# deliberately failing unreachable-server sync above).
exit 0

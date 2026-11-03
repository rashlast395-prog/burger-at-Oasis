# Smoke test of the website API against a real bo-server process.
#   powershell -File engine/server/tests/api-smoke.ps1 [-Bin <dir with bo-server>]
param([string]$Bin = (Join-Path $PSScriptRoot '..\..\target\debug'))
$ErrorActionPreference = 'Stop'
$ext = if ($null -eq $IsWindows -or $IsWindows) { '.exe' } else { '' }
$server = Join-Path $Bin "bo-server$ext"
$script:failed = 0
function Check($ok, $what) { if ($ok) { Write-Host "  ok    $what" } else { Write-Host "  FAIL  $what"; $script:failed++ } }

$root = Join-Path ([IO.Path]::GetTempPath()) ("bo-api-" + [guid]::NewGuid().ToString('N'))
$s = Join-Path $root 's'
New-Item -ItemType Directory -Force $root | Out-Null
$free = { $l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $l.Start(); $p = $l.LocalEndpoint.Port; $l.Stop(); $p }
$syncPort = & $free; $apiPort = & $free
$token = -join ((1..48) | ForEach-Object { '{0:x}' -f (Get-Random -Maximum 16) })
$base = "http://127.0.0.1:$apiPort/v1"
$auth = @{ Authorization = "Bearer $token" }

# Invoke and return status + parsed body without throwing on 4xx/5xx.
function Call($method, $path, $body = $null, $headers = $auth) {
    $req = [Net.HttpWebRequest]::Create("$base$path")
    $req.Method = $method
    foreach ($k in $headers.Keys) { $req.Headers[$k] = $headers[$k] }
    if ($null -ne $body) {
        $bytes = [Text.Encoding]::UTF8.GetBytes(($body | ConvertTo-Json -Depth 10 -Compress))
        $req.ContentType = 'application/json'
        $s2 = $req.GetRequestStream(); $s2.Write($bytes, 0, $bytes.Length); $s2.Close()
    }
    try { $resp = $req.GetResponse() } catch [Net.WebException] { $resp = $_.Exception.Response }
    $text = (New-Object IO.StreamReader($resp.GetResponseStream())).ReadToEnd()
    [pscustomobject]@{ Code = [int]$resp.StatusCode; Body = ($text | ConvertFrom-Json) }
}

$proc = $null
try {
    $ErrorActionPreference = 'Continue'   # the refusals below print to stderr on purpose
    $null = & $server init $s --host localhost --listen "127.0.0.1:$syncPort" 2>&1
    $env:BO_API_TOKEN = 'short'
    $bad = & $server run $s --api "127.0.0.1:$apiPort" 2>&1 | Out-String
    Check ($bad -match 'at least 32') 'refuses to start the API with a weak token'
    $env:BO_API_TOKEN = $token
    $bad = & $server run $s --api "0.0.0.0:$apiPort" 2>&1 | Out-String
    Check ($bad -match 'only listens on 127.0.0.1') 'refuses a non-loopback API address'
    $ErrorActionPreference = 'Stop'

    $proc = Start-Process -FilePath $server -ArgumentList @('run', $s, '--api', "127.0.0.1:$apiPort") -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $root 'out') -RedirectStandardError (Join-Path $root 'err')
    for ($i = 0; $i -lt 50; $i++) { Start-Sleep -Milliseconds 200; if ((Get-Content (Join-Path $root 'out') -Raw -ErrorAction SilentlyContinue) -match 'website API') { break } }

    Check ((Call GET '/health' $null @{}).Code -eq 401) 'no token: 401'
    Check ((Call GET '/health' $null @{ Authorization = 'Bearer wrong' }).Code -eq 401) 'wrong token: 401'
    Check ((Call GET '/health').Body.ok) 'health with token'

    $r = Call PUT '/products/classic-smash-burger' @{ name = 'Classic Smash Burger'; base_price = 1499; available = $true; category = 'burgers' }
    Check ($r.Code -eq 200 -and $r.Body.version -eq 1) 'create a product (version 1)'
    $r = Call PUT '/products/classic-smash-burger' @{ name = 'Classic Smash Burger'; base_price = 1599; available = $true; category = 'burgers' }
    Check ($r.Body.version -eq 2) 'replacing bumps the version'
    Check ((Call PUT '/products/x' @{ name = 'X'; base_price = 9.5 }).Code -eq 422) 'fractional price refused'
    $r = Call GET '/products?ids=classic-smash-burger,missing'
    Check ($r.Body.products.Count -eq 1 -and $r.Body.products[0].base_price -eq 1599) 'fetch selected products'
    Check ((Call GET '/menu').Body.products.Count -eq 1) 'menu lists products'

    Check ((Call PUT '/promos/ONCE' @{ type = 'FIXED'; value = 100; max_uses = 1; active = $true }).Code -eq 200) 'create a one-use promo'

    $order = @{ idempotency_key = 'key-0001-abcdef'; reference = 'BO-TEST1'; currency = 'GHS'; channel = 'WEB'; fulfilment = 'COLLECTION'
        customer = @{ name = 'Ama'; phone = '0240000000' }; delivery_fee = 0; discount = 100; promo_code = 'ONCE'
        items = @(@{ sku = 'classic-smash-burger'; name = 'Classic Smash Burger'; qty = 2; unit_price = 1599 }); expected_total = 3098 }
    $r = Call POST '/orders' $order
    Check ($r.Code -eq 201 -and $r.Body.order.total -eq 3098 -and $r.Body.order.status -eq 'placed' -and -not $r.Body.replayed) 'place an order'
    $id = $r.Body.order.id
    $r = Call POST '/orders' $order
    Check ($r.Code -eq 200 -and $r.Body.replayed -and $r.Body.order.id -eq $id) 'same idempotency key returns the same order'
    $order.idempotency_key = 'key-0002-abcdef'
    $r = Call POST '/orders' $order
    Check ($r.Code -eq 409 -and $r.Body.error -eq 'PROMO_EXHAUSTED') 'one-use promo cannot be used twice'
    $order.promo_code = 'NOPE'
    Check ((Call POST '/orders' $order).Body.error -eq 'PROMO_INVALID') 'unknown promo refused'
    $order.Remove('promo_code'); $order.discount = 0; $order.expected_total = 1
    Check ((Call POST '/orders' $order).Body.error -eq 'TOTAL_MISMATCH') 'a total the engine disagrees with is refused'
    $order.expected_total = 3198
    Check ((Call POST '/orders' $order).Code -eq 201) 'second order without promo'
    Check ((Call GET '/orders/by-key/key-0002-abcdef').Body.order.total -eq 3198) 'look up by idempotency key'

    Check ((Call POST "/orders/$id/status" @{ status = 'completed' }).Code -eq 409) 'cannot jump to completed'
    $r = Call POST "/orders/$id/status" @{ status = 'accepted' }
    Check ($r.Code -eq 200 -and $r.Body.order.status -eq 'accepted') 'accept the order'
    $r = Call GET "/orders/$id"
    Check ($r.Body.order.status -eq 'accepted' -and $r.Body.payment.status -eq 'unpaid') 'order with payment summary'
    $r = Call GET '/orders?status=accepted'
    Check ($r.Body.orders.Count -eq 1 -and $r.Body.orders[0].payment.status -eq 'unpaid') 'list orders by status, with payment status'
    Check ((Call POST "/orders/$id/payments" @{ method = 'card'; amount = 3098 }).Code -eq 422) 'card payments cannot be recorded by the website'
    Check ((Call POST "/orders/$id/payments" @{ method = 'cash'; amount = 0 }).Code -eq 422) 'zero cash refused'
    $r = Call POST "/orders/$id/payments" @{ method = 'cash'; amount = 3098 }
    Check ($r.Code -eq 200 -and $r.Body.payment.status -eq 'paid') 'record cash for the full total'
    Check ((Call GET '/nope').Code -eq 404) 'unknown endpoint: 404'
}
catch {
    Write-Host "  FAIL  unexpected error: $($_.Exception.Message)"
    $script:failed++
}
finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force; $proc.WaitForExit(5000) | Out-Null }
    Remove-Item Env:BO_API_TOKEN -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
if ($script:failed -gt 0) { Write-Host "$($script:failed) check(s) FAILED"; exit 1 }
Write-Host "all checks passed"
exit 0

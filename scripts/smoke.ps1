<#
.SYNOPSIS
  End-to-end smoke test against the host simulator: portal, tool list, a live
  manifest change, and the board-side range check.
.DESCRIPTION
  Starts esp32-agent-sim on a free port, waits for /healthz, then walks the flow a
  model would: tools/list -> manifest_apply -> tools/list -> tool call -> an
  out-of-range call that must be refused. Exits non-zero if any step fails.
#>
param(
  [int]$Port = 18080,
  [switch]$KeepRunning
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$log = Join-Path $repo 'sim-state\smoke.log'

New-Item -ItemType Directory -Force -Path (Split-Path $log) | Out-Null
if (Test-Path (Join-Path $repo 'sim-state\kv.json')) { Remove-Item (Join-Path $repo 'sim-state\kv.json') -Force }

Write-Host "building..." -ForegroundColor DarkGray
cargo build -p esp32-agent-sim --quiet
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$bin = Join-Path $repo 'target\debug\esp32-agent-sim.exe'
$proc = Start-Process -FilePath $bin -ArgumentList @('--port', $Port, '--host', '127.0.0.1', '--state', (Join-Path $repo 'sim-state\kv.json')) `
  -PassThru -RedirectStandardOutput $log -RedirectStandardError "$log.err" -WindowStyle Hidden

function Wait-Ready {
  for ($i = 0; $i -lt 100; $i++) {
    try {
      $r = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/healthz" -TimeoutSec 2
      if ($r.ok) { return }
    } catch { Start-Sleep -Milliseconds 120 }
  }
  throw 'simulator did not become ready'
}

function Rpc([string]$body) {
  Invoke-RestMethod -Uri "http://127.0.0.1:$Port/mcp" -Method Post -ContentType 'application/json' -Body $body -TimeoutSec 20
}

function Tool-Names($resp) {
  $resp.result.tools | ForEach-Object { $_.name }
}

$failures = 0
function Check([string]$what, [scriptblock]$test) {
  try {
    $ok = & $test
    if ($ok) { Write-Host "  PASS  $what" -ForegroundColor Green }
    else { Write-Host "  FAIL  $what" -ForegroundColor Red; $script:failures++ }
  } catch {
    Write-Host "  FAIL  $what ($($_.Exception.Message))" -ForegroundColor Red
    $script:failures++
  }
}

try {
  Wait-Ready
  Write-Host "simulator up on http://127.0.0.1:$Port" -ForegroundColor Cyan

  Check 'portal serves HTML' {
    $page = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/" -TimeoutSec 5
    $page.Content -match 'esp32-agent' -and $page.Content -match 'manifest'
  }

  Check 'healthz reports the detected devices' {
    (Invoke-RestMethod "http://127.0.0.1:$Port/healthz" -TimeoutSec 5).devices -ge 3
  }

  $before = Rpc '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
  $beforeNames = Tool-Names $before
  Write-Host ("tools before: " + ($beforeNames -join ', ')) -ForegroundColor DarkGray
  Check 'OLED tools present because the panel was detected' { $beforeNames -contains 'oled_text' }
  Check 'IMU tools absent because no MPU is wired' { -not ($beforeNames -contains 'mpu6050_read') }

  $manifest = @'
{"mode":"merge","manifest":{"tools":[
  {"name":"bench_pump","description":"Pulse the pump relay on GPIO12 for a given time",
   "params":[{"name":"ms","type":"integer","min":10,"max":5000,"unit":"ms","required":true}],
   "exec":{"type":"sequence","steps":[
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":1}},
     {"ms":0,"exec":{"type":"delay","ms":{"param":"ms"}}},
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":0}}]},
   "safety":{"max_calls_per_min":30,"confirm":false}},
  {"name":"bench_led","description":"Set the onboard RGB LED",
   "params":[{"name":"color","type":"string","required":true}],
   "exec":{"type":"rgb","pin":48,"color":{"param":"color"}}}
]}}
'@
  $applied = Rpc ('{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"manifest_apply","arguments":' + $manifest + '}}')
  Write-Host ("apply -> " + $applied.result.structuredContent.applied + ", added: " + ($applied.result.structuredContent.tools_added -join ',')) -ForegroundColor DarkGray
  Check 'manifest applied without a restart' { $applied.result.structuredContent.applied -eq $true }

  $after = Rpc '{"jsonrpc":"2.0","id":3,"method":"tools/list"}'
  $afterNames = Tool-Names $after
  Check 'the new tools are listed immediately' { ($afterNames -contains 'bench_pump') -and ($afterNames -contains 'bench_led') }

  $call = Rpc '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bench_led","arguments":{"color":"#20ff80"}}}'
  Write-Host ("led call -> " + ($call.result.content[0].text -replace "`n", ' ')) -ForegroundColor DarkGray
  Check 'the manifest tool reaches the hardware' { $call.result.isError -eq $false }

  $bad = Rpc '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bench_pump","arguments":{"ms":90000}}}'
  Write-Host ("out-of-range -> " + ($bad.result.content[0].text -replace "`n", ' ')) -ForegroundColor DarkGray
  Check 'out-of-range argument is refused on the board' {
    $bad.result.isError -eq $true -and $bad.result.content[0].text -match 'outside'
  }

  $missing = Rpc '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"lcd_text","arguments":{"text":"hi"}}}'
  Check 'a tool for absent hardware is not callable' { $missing.result.isError -eq $true }

  $schema = Rpc '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"manifest_describe","arguments":{"section":"schema"}}}'
  $exec = Rpc '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"manifest_describe","arguments":{"section":"exec"}}}'
  Check 'manifest_describe hands the model a schema and the exec types' {
    $schema.result.structuredContent.manifest_schema.properties.tools -ne $null -and
    $exec.result.structuredContent.primitives.Count -ge 8
  }

  $sessions = Invoke-RestMethod "http://127.0.0.1:$Port/api/sessions" -TimeoutSec 5
  Check 'sessions were tracked' { $sessions.sessions.Count -ge 1 }

  $tools = Invoke-RestMethod "http://127.0.0.1:$Port/api/tools" -TimeoutSec 5
  Write-Host ("portal sees " + $tools.visible + "/" + $tools.total + " tools visible") -ForegroundColor DarkGray
} finally {
  if (-not $KeepRunning) {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force; }
  }
  Write-Host "simulator log: $log" -ForegroundColor DarkGray
}

if ($failures -gt 0) {
  Write-Host "$failures check(s) failed" -ForegroundColor Red
  exit 1
}
Write-Host 'all checks passed' -ForegroundColor Green

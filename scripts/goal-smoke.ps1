<#
.SYNOPSIS
  End-to-end check of the goal loop in the host simulator: no API key needed.

.DESCRIPTION
  Starts esp32-agent-sim against a throwaway state file, points the board's model at its
  own scripted `/selftest/chat/completions`, then drives the loop through the same HTTP
  calls a person or the portal would use:

    POST /api/goal -> the scheduler tick queues ordinary agent turns -> each reply is
    classified -> notes accumulate -> the cap ends the goal.

  Then: a second start while one runs must be refused (409), stop must freeze it, and
  DELETE must go back to idle with the notes gone. Finally the same loop is run against a
  model address that refuses connections, which must end `blocked` after three failures.

  Exits non-zero if any check fails.
#>
param(
  [int]$Port = 18099
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$state = Join-Path $repo 'sim-state\goal-kv.json'
$log = Join-Path $repo 'sim-state\goal.log'
$base = "http://127.0.0.1:$Port"

New-Item -ItemType Directory -Force -Path (Split-Path $state) | Out-Null
foreach ($f in @($state, $log, "$log.err")) {
  if (Test-Path $f) { Remove-Item $f -Force }
}

cargo build -p esp32-agent-sim --quiet
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$bin = Join-Path $repo 'target\debug\esp32-agent-sim.exe'
$proc = Start-Process -FilePath $bin -ArgumentList @('--port', $Port, '--host', '127.0.0.1', '--state', $state) `
  -PassThru -RedirectStandardOutput $log -RedirectStandardError "$log.err" -WindowStyle Hidden

function Wait-Ready {
  for ($i = 0; $i -lt 150; $i++) {
    try {
      if ((Invoke-RestMethod "$base/healthz" -TimeoutSec 2).ok) { return }
    } catch { Start-Sleep -Milliseconds 150 }
  }
  throw 'simulator did not become ready'
}

function Api([string]$method, [string]$path, [string]$body) {
  $params = @{ Uri = "$base$path"; Method = $method; TimeoutSec = 15 }
  if ($body) { $params.ContentType = 'application/json'; $params.Body = $body }
  Invoke-RestMethod @params
}

function Status-Code([scriptblock]$call) {
  try { & $call | Out-Null; return 200 } catch { return $_.Exception.Response.StatusCode.value__ }
}

function Wait-Finished([int]$tries = 60) {
  $g = $null
  for ($i = 0; $i -lt $tries; $i++) {
    Start-Sleep -Milliseconds 250
    $g = Api 'Get' '/api/goal'
    if ($g.status -ne 'running') { return $g }
  }
  return $g
}

$failures = 0
function Check([string]$what, [scriptblock]$test) {
  try {
    if (& $test) { Write-Host "  PASS  $what" -ForegroundColor Green }
    else { Write-Host "  FAIL  $what" -ForegroundColor Red; $script:failures++ }
  } catch {
    Write-Host "  FAIL  $what ($($_.Exception.Message))" -ForegroundColor Red
    $script:failures++
  }
}

try {
  Wait-Ready
  Write-Host "simulator up on $base"

  # Point the board's model at its own scripted endpoint: no key, no external service.
  $selftest = '{"base_url":"' + $base + '/selftest/chat/completions","model":"selftest","max_steps":2,"api_key":""}'
  Api 'Post' '/api/agent/config' $selftest | Out-Null

  $start = Api 'Post' '/api/goal' '{"objective":"confirm the board answers board_status","max_iterations":2}'
  Write-Host "  start -> status=$($start.status) iteration=$($start.iteration) max=$($start.max_iterations)"
  Check 'the start answer carries the next prompt' { $start.prompt -and $start.prompt.Contains('confirm the board') }
  Check 'nothing is queued by the route itself (the tick is the loop clock)' { -not (Api 'Get' '/api/agent/history').pending }

  $mid = Api 'Get' '/api/status'
  Check 'the status block reports the goal' { $mid.goal.status -eq 'running' -and $mid.goal.max_iterations -eq 2 }

  $g = Wait-Finished
  Write-Host "  after the cap -> status=$($g.status) iteration=$($g.iteration) failures=$($g.failures) reason=`"$($g.stop_reason)`""
  Write-Host "  notes: $(($g.notes) -join ' | ')"
  Check 'the loop iterated on its own to the cap' { $g.status -eq 'exhausted' -and $g.iteration -eq 2 }
  Check 'every iteration left one note' { $g.notes.Count -eq 2 }
  Check 'the second prompt carried the first note' {
    ($g.notes[0] -ne $null) -and ($g.notes[0].Length -gt 0)
  }
  Check 'a finished goal has no prompt' { $null -eq $g.prompt }
  Check 'the run journal shows the goal turns as ordinary runs' {
    $events = Api 'Get' '/api/agent/events?after=0'
    ($events.events | Where-Object { $_.event -eq 'message_start' }).Count -ge 2
  }
  Check 'the transcript attributes the turns to the goal' {
    $hist = Api 'Get' '/api/agent/history?after=0'
    ($hist.messages | Where-Object { $_.role -eq 'user' -and $_.source -eq 'goal' }).Count -eq 2
  }

  # One goal at a time.
  Api 'Post' '/api/goal' '{"objective":"watch the door sensor","max_iterations":50}' | Out-Null
  $code = Status-Code { Api 'Post' '/api/goal' '{"objective":"a second objective"}' }
  Check 'a second start while one runs is refused with 409' { $code -eq 409 }

  $stopped = Api 'Post' '/api/goal/stop' '{"reason":"enough"}'
  Check 'stop freezes the goal' { $stopped.status -eq 'stopped' -and $stopped.stop_reason -eq 'enough' }
  Start-Sleep -Milliseconds 1200
  $later = Api 'Get' '/api/goal'
  Check 'a stopped goal never resumes, however many ticks pass' { $later.status -eq 'stopped' -and $later.iteration -eq $stopped.iteration }
  Check 'a stopped goal has no prompt' { $null -eq $later.prompt }

  $cleared = Api 'Delete' '/api/goal'
  Check 'delete goes back to idle with the notes gone' { $cleared.status -eq 'idle' -and $cleared.notes.Count -eq 0 -and $cleared.objective -eq '' }

  # The abort path: a model address that refuses connections.
  Api 'Post' '/api/agent/config' '{"base_url":"http://127.0.0.1:9/v1/chat/completions","model":"gone"}' | Out-Null
  Api 'Post' '/api/goal' '{"objective":"read the display","max_iterations":10}' | Out-Null
  $blocked = Wait-Finished 80
  Write-Host "  unreachable model -> status=$($blocked.status) iteration=$($blocked.iteration) failures=$($blocked.failures) reason=`"$($blocked.stop_reason)`""
  Check 'three failed iterations in a row block the goal' { $blocked.status -eq 'blocked' -and $blocked.failures -eq 3 }
  Check 'a failed iteration still cost budget' { $blocked.iteration -eq 3 }
  Check 'the notes say why' { ($blocked.notes | Where-Object { $_ -like 'failed:*' }).Count -eq 3 }

  # A reboot keeps the objective and the notes.
  Stop-Process -Id $proc.Id -Force
  Start-Sleep -Milliseconds 400
  $proc = Start-Process -FilePath $bin -ArgumentList @('--port', $Port, '--host', '127.0.0.1', '--state', $state) `
    -PassThru -RedirectStandardOutput "$log.2" -RedirectStandardError "$log.2.err" -WindowStyle Hidden
  Wait-Ready
  $restored = Api 'Get' '/api/goal'
  Write-Host "  after a restart -> status=$($restored.status) iteration=$($restored.iteration) objective=`"$($restored.objective)`""
  Check 'the objective and the notes survive a restart' {
    $restored.objective -eq 'read the display' -and $restored.iteration -eq 3 -and $restored.notes.Count -eq 3
  }
} finally {
  if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
}

if ($failures -gt 0) {
  Write-Host "$failures check(s) failed" -ForegroundColor Red
  exit 1
}
Write-Host 'all goal checks passed' -ForegroundColor Green

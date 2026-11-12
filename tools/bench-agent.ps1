# Bench verification for the board-side agent loop.
#
# Proves, on hardware, the three things the brief asks for:
#   1. a message sent over HTTP reaches the loop, the loop calls a tool, and a
#      final answer comes back (the model is the stub on the LAN, so no key);
#   2. the transcript is readable through the API the portal uses;
#   3. a scheduled task fires with no HTTP request involved.
#
#   powershell -File tools/bench-agent.ps1
param(
    [string]$Board = "192.0.2.2",
    [int]$Port = 8080,
    [string]$ModelUrl = "http://192.0.2.10:8899/v1/chat/completions",
    [int]$ScheduleSeconds = 60
)

$ErrorActionPreference = "Continue"
$base = "http://${Board}:${Port}"
$fail = 0

function Api($method, $path, $body) {
    $args = @{ Uri = "$base$path"; Method = $method; TimeoutSec = 40; UseBasicParsing = $true }
    if ($body -ne $null) {
        $args.Body = ($body | ConvertTo-Json -Compress -Depth 8)
        $args.ContentType = "application/json"
    }
    try {
        $r = Invoke-WebRequest @args
        return ($r.Content | ConvertFrom-Json)
    } catch {
        Write-Host ("  ! {0} {1} failed: {2}" -f $method, $path, $_.Exception.Message) -ForegroundColor Red
        $script:fail++
        return $null
    }
}

function Check($what, $ok, $detail) {
    if ($ok) { Write-Host ("  PASS  {0}" -f $what) -ForegroundColor Green }
    else { Write-Host ("  FAIL  {0} {1}" -f $what, $detail) -ForegroundColor Red; $script:fail++ }
}

Write-Host "board: $base   model: $ModelUrl"

# 0. the stub must be reachable from here at least
$stub = $null
try { $stub = (Invoke-WebRequest -Uri ($ModelUrl -replace "/v1/chat/completions$", "/") -TimeoutSec 5 -UseBasicParsing).Content } catch {}
Check "stub model answers on the LAN" ($stub -ne $null) "($stub)"

# 1. point the board at the stub
$cfg = Api "POST" "/api/agent/config" @{ base_url = $ModelUrl; model = "stub"; max_steps = 4; api_key = "" }
Check "agent config accepted" ($cfg -ne $null) ""

# 2. send a message, wait for the loop to answer
$before = 0
$h = Api "GET" "/api/agent/history?after=0" $null
if ($h -and $h.seq) { $before = [int]$h.seq }
$q = Api "POST" "/api/agent/message" @{ text = "What is this board doing right now?" }
Check "message queued" ($q -ne $null -and $q.queued) ""

$answered = $false
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Seconds 2
    $h = Api "GET" ("/api/agent/history?after=" + $before) $null
    if ($h -and $h.messages) {
        $final = $h.messages | Where-Object { $_.role -eq "assistant" -and $_.text } | Select-Object -Last 1
        if ($final) { $answered = $true; break }
    }
}
Check "loop produced an assistant answer" $answered ""

$h = Api "GET" "/api/agent/history?after=0" $null
$toolSeen = $false
if ($h -and $h.messages) {
    Write-Host "  transcript:" -ForegroundColor DarkGray
    foreach ($m in $h.messages) {
        $text = ($m.text -replace "`r?`n", " ")
        if ($text.Length -gt 110) { $text = $text.Substring(0, 110) + "..." }
        $extra = ""
        if ($m.tool_calls -and $m.tool_calls.Count -gt 0) { $extra = " -> " + (($m.tool_calls | ForEach-Object { $_.name }) -join ",") }
        if ($m.role -eq "tool") { $toolSeen = $true }
        Write-Host ("    {0,-9} {1}{2}" -f $m.role, $text, $extra)
    }
}
Check "the loop actually called a board tool" $toolSeen ""

# 3. a scheduled task, no HTTP trigger
$s = Api "POST" "/api/sched" @{
    kind = "task"; id = "bench60"; prompt = "scheduled bench tick: report board status in one line";
    every_s = $ScheduleSeconds; at_hhmm = ""; enabled = $true
}
Check "scheduled task accepted" ($s -ne $null) ""
$list = Api "GET" "/api/sched" $null
Check "task is listed" ($list -ne $null) ""

Write-Host ("  waiting up to {0}s for the scheduled run..." -f ($ScheduleSeconds * 2)) -ForegroundColor DarkGray
$fired = $false
$deadline = (Get-Date).AddSeconds($ScheduleSeconds * 2)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 5
    $h = Api "GET" "/api/agent/history?after=0" $null
    if ($h -and $h.messages) {
        $sched = $h.messages | Where-Object { $_.source -like "schedule:*" } | Select-Object -First 1
        if ($sched) { $fired = $true; break }
    }
}
Check "scheduled task fired without an HTTP message" $fired ""

# 4. leave the board tidy: stop the task
Api "POST" "/api/sched" @{ kind = "task"; op = "delete"; id = "bench60" } | Out-Null

Write-Host ""
if ($fail -eq 0) { Write-Host "agent loop: all checks passed" -ForegroundColor Green }
else { Write-Host ("agent loop: {0} check(s) failed" -f $fail) -ForegroundColor Red }
exit $fail

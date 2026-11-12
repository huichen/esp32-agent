#!/usr/bin/env bash
# End-to-end smoke test against the host simulator: portal, tool list, a live
# manifest change, and the board-side range check.
#
# The POSIX counterpart of scripts/smoke.ps1: same build, same simulator
# invocation, same HTTP calls, same checks. Starts esp32-agent-sim on a free
# port, waits for /healthz, then walks the flow a model would:
# tools/list -> manifest_apply -> tools/list -> tool call -> an out-of-range
# call that must be refused. Exits non-zero if any step fails.
#
#   bash scripts/smoke.sh
#   PORT=18090 bash scripts/smoke.sh
#   KEEP_RUNNING=1 bash scripts/smoke.sh     # leave the simulator up
#
# Needs cargo, curl and python3 (two checks count JSON entries).
set -u

PORT="${PORT:-18080}"
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
base="http://127.0.0.1:$PORT"
log="$repo/sim-state/smoke.log"

mkdir -p "$repo/sim-state"
rm -f "$repo/sim-state/kv.json"

echo "building..."
cargo build -p esp32-agent-sim --quiet || { echo 'cargo build failed' >&2; exit 1; }

bin="$repo/target/debug/esp32-agent-sim"
[ -x "$bin" ] || bin="$bin.exe"   # Windows, where the same script is testable
"$bin" --port "$PORT" --host 127.0.0.1 --state "$repo/sim-state/kv.json" \
  > "$log" 2> "$log.err" &
sim=$!

cleanup() {
  if [ "${KEEP_RUNNING:-0}" = "1" ]; then
    echo "simulator left running (pid $sim), log: $log"
    return
  fi
  kill "$sim" 2>/dev/null
  wait "$sim" 2>/dev/null
  echo "simulator log: $log"
}
trap cleanup EXIT

failures=0
check() {
  what=$1
  shift
  if "$@"; then
    printf '  PASS  %s\n' "$what"
  else
    printf '  FAIL  %s\n' "$what"
    failures=$((failures + 1))
  fi
}

rpc() {
  curl -sS -X POST "$base/mcp" -H 'Content-Type: application/json' -d "$1"
}

ready=0
tries=0
while [ "$tries" -lt 100 ]; do
  tries=$((tries + 1))
  if curl -sS -m 2 "$base/healthz" > /dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.12
done
if [ "$ready" -ne 1 ]; then
  echo 'simulator did not become ready' >&2
  exit 1
fi
echo "simulator up on $base"

page=$(curl -sS "$base/")
check 'portal serves HTML' bash -c 'grep -q esp32-agent <<<"$1" && grep -q manifest <<<"$1"' _ "$page"

devices=$(curl -sS "$base/healthz" | sed -n 's/.*"devices":\([0-9][0-9]*\).*/\1/p')
check 'healthz reports the detected devices' test "${devices:-0}" -ge 3

before=$(rpc '{"jsonrpc":"2.0","id":1,"method":"tools/list"}')
echo "tools before: $(grep -o '"name":"[a-z0-9_]*"' <<<"$before" | sed 's/"name":"//;s/"//' | tr '\n' ' ')"
check 'OLED tools present because the panel was detected' grep -q '"name":"oled_text"' <<<"$before"
check 'IMU tools absent because no MPU is wired' bash -c '! grep -q mpu6050_read <<<"$1"' _ "$before"

manifest='{"mode":"merge","manifest":{"tools":[
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
]}}'

applied=$(rpc '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"manifest_apply","arguments":'"$manifest"'}}')
echo "apply -> $(sed -n 's/.*"applied":\([a-z]*\).*/\1/p' <<<"$applied")"
check 'manifest applied without a restart' grep -q '"applied":true' <<<"$applied"

after=$(rpc '{"jsonrpc":"2.0","id":3,"method":"tools/list"}')
check 'the new tools are listed immediately' bash -c 'grep -q bench_pump <<<"$1" && grep -q bench_led <<<"$1"' _ "$after"

call=$(rpc '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bench_led","arguments":{"color":"#20ff80"}}}')
echo "led call -> $(tr -d '\n' <<<"$call" | cut -c1-160)"
check 'the manifest tool reaches the hardware' grep -q '"isError":false' <<<"$call"

bad=$(rpc '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bench_pump","arguments":{"ms":90000}}}')
echo "out-of-range -> $(tr -d '\n' <<<"$bad" | cut -c1-160)"
check 'out-of-range argument is refused on the board' bash -c 'grep -q "\"isError\":true" <<<"$1" && grep -q outside <<<"$1"' _ "$bad"

missing=$(rpc '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"lcd_text","arguments":{"text":"hi"}}}')
check 'a tool for absent hardware is not callable' grep -q '"isError":true' <<<"$missing"

schema=$(rpc '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"manifest_describe","arguments":{"section":"schema"}}}')
exec=$(rpc '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"manifest_describe","arguments":{"section":"exec"}}}')
primitives=$(python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d["result"]["structuredContent"].get("primitives") or []))' <<<"$exec")
check 'manifest_describe hands the model a schema and the exec types' bash -c \
  'grep -q "\"manifest_schema\"" <<<"$1" && grep -q "\"tools\"" <<<"$1" && [ "$2" -ge 8 ]' _ "$schema" "${primitives:-0}"

sessions=$(curl -sS "$base/api/sessions" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["sessions"]))')
check 'sessions were tracked' test "${sessions:-0}" -ge 1

tools=$(curl -sS "$base/api/tools")
echo "portal sees $(sed -n 's/.*"visible":\([0-9][0-9]*\).*/\1/p' <<<"$tools" | head -1)/$(sed -n 's/.*"total":\([0-9][0-9]*\).*/\1/p' <<<"$tools" | head -1) tools visible"

cleanup
trap - EXIT

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed" >&2
  exit 1
fi
echo 'all checks passed'

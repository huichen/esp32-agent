# Timed prompts and sensor rules

Two ways to give the board something to do on its own. Both feed the same queue a chat
message uses, so a task and a person cannot start two runs at once.

## Tasks

```bash
curl -sS -X POST http://<board>/api/sched -H 'Content-Type: application/json' -d '{
  "kind":"task","op":"add",
  "task":{"id":"t1","prompt":"read the BME280 and say the temperature","every_s":300}}'
```

| Field | Meaning |
|---|---|
| `after_s` | fire once, this many seconds after the task was created, then remove the task. Use it alone: a one-shot is not combined with `every_s` or `at_hhmm` |
| `every_s` | interval in seconds, **at least 10**: a scheduled prompt is a model call and the board has one agent thread, so a shorter interval is due before the last turn finished |
| `at_hhmm` | `"07:30"`, in the board's local time: `tz_offset_h` (whole hours east of UTC) shifts it, and it is settable from the portal or `POST /api/config`, so no reflash is needed. Used when `every_s` is 0 |
| `script` | a script to run instead of asking the model — costs no tokens and no call |
| `enabled` | default true |

A task has exactly one trigger. `after_s` is the one-shot: the scheduler removes the task in
the same tick that fires it, so "remind me in twenty minutes" is one call and nothing to
clean up afterwards. The agent can name its own task (`id`) when it adds one, which makes a
second add of the same name a refusal instead of a task nobody can address.

The ten-second floor on `every_s` is not politeness: with eight tasks at one second each the
board spent every wakeup on a queued turn, answered nothing else on the network, and billed a
model call every second. A `script` task has no such floor by intent — it costs no call — but
it still passes through the same validation, so a pulse loop belongs in `script` with a wait,
not in a one-second prompt.

When both `prompt` and `script` are set the script wins and the prompt stays as the record of
what the task is for. A task with neither is refused: a task that fires on schedule and does
nothing looks like a bug six hours later.

Every answer lands in the transcript tagged `source: "schedule:t1"`, so the origin of a turn
is never a guess. The agent can schedule itself with
`schedule {"action":"add","prompt":"…","every_s":300}` (`list`, `remove`, `enable`, `disable`).

### The clock

`at_hhmm` is local time, and `tz_offset_h` is the setting that makes "local" mean something:
whole hours east of UTC, `-12..=14`, changed with `POST /api/config {"tz_offset_h":8}` or the
portal's Settings tab, applied on the next tick. It used to be a build-time constant
(`MCP_TZ_OFFSET_H`), which meant moving the board to another country was a reflash.

The board has no timezone database, so a zone *name* would be a promise it cannot keep, and
half-hour and 45-minute zones cannot be expressed. A fractional value is refused rather than
rounded: silently turning `UTC+5:30` into `UTC+5` moves every daily task by half an hour, and
nothing would say so. `every_s` and `after_s` do not depend on the clock at all — they count
from the last run.

### Run a task now

`POST /api/sched/run` with `{"id":"t1"}` does not move the task's scheduled clock.
A prompt task returns `queued: true`. A script runs inline and returns `job`, `state`,
`result`, `output`, `steps`, and `ms`; a failed script also returns `error` with
`state: "failed"`. The job ID is a correlation label, not a future poll URL:
`GET /api/script/<job>` remains a retired route (404).

## Rules

```bash
curl -sS -X POST http://<board>/api/sched -H 'Content-Type: application/json' -d '{
  "kind":"rule","op":"add",
  "rule":{"id":"r1","tool":"bme280_read","path":"temperature_c","op":"gt","value":30,
          "prompt":"it is hot, check the enclosure","min_interval_s":300}}'
```

The board polls the tool, reads the JSON path, compares with `op` (`eq`, `ne`, `gt`, `lt`,
`true`) and only then queues the prompt. `min_interval_s` is the rate limit: without it a
reading hovering at the threshold would start a run every tick.

Only board tools can be polled. `fs_*` and `net_*` are refused, because reading a file or a
web page on a timer is not a sensor.

## What the scheduler can see

`at_hhmm` needs a clock, so it needs `time.synced: true` in `/api/status` — SNTP starts
after the radio is up and is the only thing that makes the wall clock real. A task whose
time has passed while the board was off fires once on boot, not once per missed interval.

Related: [scripting](scripting.md) · [agent loop](agent-loop.md) · [HTTP API](http-api.md)

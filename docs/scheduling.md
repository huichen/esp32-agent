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
| `every_s` | interval in seconds |
| `at_hhmm` | `"07:30"`, in UTC unless the firmware was built with `MCP_TZ_OFFSET_H` (`MCP_TZ_OFFSET_H=8` shifts it to UTC+8); used when `every_s` is 0 |
| `script` | a script to run instead of asking the model — costs no tokens and no call |
| `enabled` | default true |

When both `prompt` and `script` are set the script wins and the prompt stays as the record of
what the task is for. A task with neither is refused: a task that fires on schedule and does
nothing looks like a bug six hours later.

Every answer lands in the transcript tagged `source: "schedule:t1"`, so the origin of a turn
is never a guess. The agent can schedule itself with
`schedule {"action":"add","prompt":"…","every_s":300}` (`list`, `remove`, `enable`, `disable`).

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

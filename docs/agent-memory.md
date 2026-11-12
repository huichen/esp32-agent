# Memory, compression and forgetting

Three things that keep a small model useful over time: it can write down what it learned,
the transcript stops growing at a bounded cost, and a client can cut the past away on
purpose.

All three live on the board's file workspace and the agent's own state — no new tool, no
extra model call, and nothing that needs a host.

## 1. Memory is an index plus files

```text
/MEMORY.md              the index: one line per memory, each pointing at a file
/memory/<topic>.md      the memory itself, at most 2 KB (the workspace file limit)
```

The **index** is what reaches the system prompt, every run, capped at 24 lines / 2 KB:

```text
## Memory (/MEMORY.md)
… Open a topic with fs_read before relying on it …
- [pins](/memory/pins.md) — GPIO18 is the strip, active high (2026-11-11)
- [wifi](/memory/wifi.md) — the 5 GHz SSID is not visible (2026-11-10)
```

The **bodies** stay on the workspace and are opened with `fs_read` when a topic is
relevant. That split is the whole design: a prompt that carries every memory crowds out the
tool descriptions, and on a board with a few dozen kB of internal RAM the prompt is the
expensive part.

Lines that are not pointers are dropped — a heading, a paragraph, a bullet with no path.
A model that writes prose instead of an index therefore loses the budget, not the
feature: `/MEMORY.md` stays readable, and the prompt only carries what a reader can follow.

### Writing a memory

There is no memory tool, on purpose. `write` and `edit` already reach the file, and a
fourth tool that writes one file costs a definition in every request:

```json
{"name":"write","arguments":{
  "path":"/memory/pins.md",
  "text":"# pins\nGPIO18 is the WS2812 strip (8 px), active high.\nGPIO48 is the devkit LED.\n"}}
```

then one line appended to the index:

```json
{"name":"edit","arguments":{
  "path":"/MEMORY.md",
  "find":"- [wifi]",
  "replace":"- [pins](/memory/pins.md) — GPIO18 is the strip (2026-11-11)\n- [wifi]"}}
```

`GET /api/agent/prompt` shows exactly what the model will be told, memory included, so this
is checkable without a model in the loop.

## 2. Compression without a summary

When the transcript passes 80% of `agent.context_tokens`, or either hard cap (256
messages, 512 KB), the oldest exchanges are **not** handed to a model to summarise. They
are replaced by one deterministic line per message, derived from the messages themselves:

```text
## Earlier in this conversation (compressed)
- user: set the on-board LED blue, then read the temperature once
- called: board_status({})
- result: {"board_name":"ESP32-S3 breadboard…","devices":6,…}
- answer: the LED is blue, the temperature is 25.1 °C
```

| Property | Why it matters on this board |
|---|---|
| No model call | Compression must never fail, cost tokens, or depend on the network. |
| Cannot invent facts | The lines are the transcript's own text, truncated — not an interpretation. |
| Reproducible | The same history compresses the same way. |
| Bounded | One line per message, 40 lines maximum; older lines fall off the front. |

The last four exchanges survive verbatim (`COMPACT_KEEP_TURNS`), which is what keeps a task
on the rails: what was just asked, and what the last tools returned.

The index is rendered into the system prompt under *Earlier in this conversation*, with one
instruction that matters: a tool call listed there has already been made.

## 3. Forgetting

`POST /api/agent/forget {"keep": N}`

| `keep` | Effect |
|---|---|
| `0` | Drop the whole conversation — a fresh start |
| `1..n` | Keep the last `n` question-and-answer exchanges, drop everything older |

The response is `{ok, removed, kept}`, and the compressed index is cleared with it: an index
of what is no longer there is worse than none. `POST /api/agent/reset` is the same thing
with `keep: 0` and is kept for older clients.

External agents use this between tasks; the on-board agent cannot call it (it would be
editing the conversation it is in the middle of).

## What it costs

| Budget | Value |
|---|---|
| Memory index in the prompt | ≤ 2 KB, ≤ 24 lines |
| Compressed index in the prompt | ≤ 40 lines, ~90 chars each |
| Transcript kept verbatim | the last 4 exchanges, within 80% of `context_tokens` and the 512 KB / 256-message caps |
| Model calls added | none |

## Verifying it without a model

```bash
# write a memory through the normal file tools
curl -sS -X POST http://<board>:80/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
       "name":"write","arguments":{"path":"/memory/pins.md","text":"GPIO18 = strip"}}}'

# the index line
curl -sS -X POST http://<board>:80/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
       "name":"write","arguments":{
         "path":"/MEMORY.md","text":"- [pins](/memory/pins.md) — GPIO18 is the strip\n"}}}'

# what the model will be told
curl -sS http://<board>:80/api/agent/prompt | jq -r .system | sed -n '/## Memory/,/^## /p'

# forgetting
curl -sS -X POST http://<board>:80/api/agent/forget \
  -H 'Content-Type: application/json' -d '{"keep":1}'
```

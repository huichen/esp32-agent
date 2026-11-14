# Memory, compression and forgetting

Three things that keep a small model useful over time: it can write down what it learned,
the transcript stops growing at a bounded cost, and a client can cut the past away on
purpose.

All three live on the board's file workspace and the agent's own state — no new tool, no
extra model call, and nothing that needs a host.

## 1. Memory is an index plus files

```text
/MEMORY.md              the index: one line per memory, each pointing at a file
/memory/<topic>.md      the memory itself, at most 256 KiB (the workspace file limit)
```

The **index** reaches the system prompt every run, capped at 24 pointer lines / 2 KiB,
including the truncation marker:

```text
## Memory (/MEMORY.md)
… Open a topic with fs_read before relying on it …
- [pins](/memory/pins.md) — GPIO18 is the strip, active high (2026-11-11)
- [wifi](/memory/wifi.md) — the 5 GHz SSID is not visible (2026-11-10)
```

The **bodies** stay on the workspace and are opened with `fs_read` when a topic is
relevant. A prompt that carries every memory crowds out tool descriptions and costs
request heap and provider tokens. The full workspace is still in RAM, with a 4 MiB
aggregate text limit and separate metadata limits; a flash filesystem does not add heap.
See [storage](storage.md).

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
is checkable without a model in the loop. Memory-file tools report `queued: true` /
`saved: false`: the RAM edit was admitted, not confirmed on storage. Later publication
receipts control saved-hash bookkeeping; neither is a power-loss measurement.

## 2. Compression without a summary

When the transcript passes 80% of `agent.context_tokens`, or either hard cap (1,024
messages, 2 MiB of payload), the oldest exchanges are **not** handed to a model to summarise. They
are represented by deterministic excerpts derived from earlier messages, not a generated
summary:

```text
## Earlier in this conversation (compressed)
- user: set the on-board LED blue, then read the temperature once
- called: board {command: "status"}
- result: {"board_name":"ESP32-S3 breadboard…","devices":6,…}
- answer: the LED is blue, the temperature is 25.1 °C
```

| Property | Why it matters on this board |
|---|---|
| No model call | Compression must never fail, cost tokens, or depend on the network. |
| Cannot invent facts | The lines are the transcript's own text, truncated — not an interpretation. |
| Reproducible | The same history compresses the same way. |
| Bounded | At most 40 lines **and** 96 KiB of excerpt text: older lines are truncated to 2 KiB, the newest question to 32 KiB, oldest evicted first. |

Recent exchanges are selected by `keep_recent_tokens`: at most a quarter of the 80% token
trigger, further limited by the output reserve. At least one newest exchange is kept.
Four verbatim exchanges are not an invariant of the current token-based selection, and
hard byte/message trimming can remove more. Payload accounting includes text, reasoning,
serialized arguments, attachment payloads/metadata, IDs and source; an oversized newest
message can remain as an explicit exception. The 2 MiB ceiling is not a measured peak-heap
guarantee.

### Compressing on request

`POST /api/agent/compact {"keep": N}` runs the same projection before the budget is
reached. `keep` is optional: without it the call spends exactly the budget the automatic
trigger would, so a manual pass cannot leave a transcript the next turn immediately
compresses again. With it, the last `N` exchanges stay verbatim.

| Field | Meaning |
|---|---|
| `kept_turns` | exchanges left verbatim |
| `removed` | messages this pass dropped (`0` means there was nothing to drop) |
| `messages_before` / `messages_after` | message counts either side, index message included |
| `bytes_before` / `bytes_after` | payload bytes either side |
| `tokens_before` / `tokens_after` | the same estimate the trigger uses |
| `archive_lines`, `demoted_answers` | how much index the model now gets |
| `index_bytes` | bytes the index message itself adds back to the transcript |
| `stored` | whether flash accepted the shorter transcript |

The call is refused with `409` while a model turn is running: that turn is answering a
question this would remove from under it. It is a **command, not a tool the model can
call** — the model does not know where the task boundary is, and compacting its own live
context mid-turn is how an agent loses the instruction it was working from.

`stored: false` is a real state, not a rounding: the conversation is smaller in RAM but
the dropped part is still on flash, so a reboot restores it. It happens when
`persist_transcript` is off or the store queue rejected the write.

### Where the index lives

The index is a **message in the transcript**, not part of the system prompt. It sits at the
boundary between what was dropped and what is still verbatim: a `user` message whose text
begins `[Context compaction: deterministic index, no model summary …],` flagged as
`checkpoint` so nothing treats it as a question (it does not count as an exchange, is never
archived back into itself, and `/api/agent/history` marks it).

That placement is the host agent's, and it is the same choice for the same reasons:

| Why not the system prompt | What the message buys |
|---|---|
| its size would depend on how long the conversation had run, so the prompt could not be cached | the prompt is byte-identical across turns |
| it would be rebuilt from an in-RAM field, so a reboot would lose it | it travels with the transcript and is restored with it |
| the model would read the index *before* the instructions that govern it | it reads it after the questions it replaces, where it belongs |

The framing in the text is deliberate too: historical data, not instructions; reordered
rather than a timeline; a tool call listed there has already been made, so repeating it is
wasted work.

## 3. Forgetting

`POST /api/agent/forget {"keep": N}`

| `keep` | Effect |
|---|---|
| `0` | Drop the whole conversation — a fresh start |
| `1..n` | Keep the last `n` question-and-answer exchanges, drop everything older |

The response includes `{ok, removed, kept}`; `removed` and `kept` count messages, while
`keep` selects exchanges. The compressed index is cleared too. `POST /api/agent/reset`
clears the conversation like `keep: 0` (its response uses `reset: true`). Both discard the
pending question in RAM and request removal of its stored queue copy, even when keeping
some older exchanges. Both request a stored-transcript update/removal, but the response
is not flash acknowledgement; a later queued-write failure can leave the older persisted
history. See [storage](storage.md).

External agents use this between tasks; the on-board agent cannot call it (it would be
editing the conversation it is in the middle of).

## What it costs

| Budget | Value |
|---|---|
| Memory index in the prompt | ≤ 2 KiB including truncation marker, ≤ 24 pointer lines |
| Compressed index in the prompt | ≤ 40 lines **and** ≤ 96 KiB of truncated excerpts (older ≤ 2 KiB, newest question ≤ 32 KiB); separate from the history byte budget |
| Transcript kept verbatim | recent exchanges selected by the token budget, subject to the 2 MiB / 1,024-message backstops and newest-message exception |
| Saved transcript | ≤ 512 KiB of JSONL including header, newest complete exchanges/call-result pairs; no attachment payloads |
| Model calls added | none |

## Verifying it without a model

When configured, add the MCP bearer token to `/mcp` requests and the portal credential
to the REST prompt/forget requests; they are separate credentials ([security](security.md)).

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

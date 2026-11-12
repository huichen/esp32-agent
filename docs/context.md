# What the model always sees

Four layers are assembled into the system prompt on **every** turn. None of them lives in the
transcript, so they cost the same whether the conversation is one message or a thousand.

| Layer | Where it lives | What reaches the model | Written by |
|---|---|---|---|
| Standing orders | `agent.system_prompt` (NVS) | all of it, 2 KB cap | the owner, over HTTP or `shell` |
| Project instructions | `/AGENTS.md` | the first 1 KB, then a marker saying the rest is there | the owner, with `write` |
| Long-term memory | `/MEMORY.md`, an index over `/memory/<topic>.md` | the index only, 24 lines / 2 KB | the agent itself, as it learns |
| Skills | `/skills/<name>/SKILL.md` | one line each: name and description | either |

```bash
curl -sS -X POST http://<board>/api/agent/prompt -H 'Content-Type: application/json' \
  -d '{"action":"set","text":"The pump relay is on GPIO12. Never run it past two minutes."}'
curl -sS http://<board>/api/agent/prompt | jq -r .system    # what the model will receive
```

`action` is `set`, `append` or `clear`. The same slot is reachable from the board itself:
`shell {"command":"system_prompt set …"}` and `system_prompt get` to read it back.

## Memory is just files

`/MEMORY.md` is an index, one pointer per line; the memory itself is `/memory/<topic>.md`.
An entry is a fact with a date, not a narrative:

```text
- [wiring](/memory/wiring.md) — GPIO18 LED strip, GPIO48 on-board LED (2026-11-11)
```

Write one with `write /memory/pins.md "GPIO18 = LED strip"` and add the line with `edit`. The
index collector keeps only lines that carry a path, so a paragraph in `/MEMORY.md` is ignored
instead of eating the budget; the index is capped at 24 lines and the prompt says when it is
full. Delete the file and the memory is gone — there is no second store to clean up.

## Why it is split like this

Internal SRAM is kilobytes. A board cannot carry everything it knows in every request, so each
layer is an index or a prefix and the expensive part is fetched with `read` when a turn needs
it. `/api/agent/prompt` is the only honest way to check the result: it returns the assembled
text, not a summary of it.

Files take effect on the next turn — no restart, no reflash, no transcript rewrite.

Related: [memory](agent-memory.md) · [skills](agent-skills.md) · [agent loop](agent-loop.md)

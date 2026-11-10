# Instructions and skills on the board

Two things an agent needs that are neither a tool nor a line of the transcript: the
rules of *this* installation, and procedures it should follow when they apply. Both live
on the board's file workspace, so the same files serve the portal, an MCP client and the
agent itself — there is no second store to keep in sync.

| Path | Role | Cost in the prompt |
|---|---|---|
| `/AGENTS.md` | project instructions, always applied | first 1024 bytes, every turn |
| `/skills/<name>.md` | one skill per file | its `description:` line only |

The body of a skill reaches the model only when it calls `skill_read`. On a board with
roughly 300 KB of usable heap that matters twice: the prompt has to stay small, and the
tool descriptions are what make a small model useful — a prompt that carries every
procedure crowds them out.

## `/AGENTS.md`

Plain Markdown, written for a reader with no memory of your bench:

```markdown
# This bench

- GPIO12 drives the pump relay, **active low**; one second maximum, ask before pulsing.
- The BME280 at 0x76 reads 2 °C high until the case is closed.
- The portal is for the operator; do not change `wifi.*` unless asked.
```

The first 1024 bytes go into the system prompt with a marker where the cut happened, and
the model is told to read the rest with `fs_read /AGENTS.md`. Editing the file takes
effect on the next turn: no restart, no transcript rewrite.

Written from anywhere:

```bash
# the agent itself, mid-turn
fs_write {"path":"/AGENTS.md","text":"GPIO12 is the pump relay, active low."}

# a skill, written the same way
fs_write {"path":"/skills/pulse_pump.md",
          "text":"# pulse_pump\ndescription: switch the pump for a measured pulse\n1. confirm the request names a duration\n2. bench_pulse {ms}\n3. read power_read before and after\n"}
```

## Skills

One file per skill, at most 2 KB (the workspace's per-file limit):

```markdown
# deploy
description: roll the portal forward without losing the transcript
1. Read `/AGENTS.md` and confirm the board is not mid-goal.
2. `fs_write` a note to `/notes/deploy-<date>.md` first.
3. Build, flash, then `board_status` and one `bme280_read`.
Acceptance: `board_status.uptime_s` is small and both reads succeed.
```

Rules, such as they are:

- The **name is the file stem** (`deploy`), and it must be `a-z A-Z 0-9 _ -`, at most 32
  characters. It is what the model asks for and what appears in the prompt.
- `description:` is the one line the model sees before deciding. Without one, the first
  prose line is used, so a plain Markdown note still appears in the index.
- The index carries at most 12 skills; the rest are on the workspace but unnamed, and
  `skill_list` shows what is there.
- No YAML front matter, no subdirectories, no includes. A skill is a procedure, not an
  interpreter.

Write one with `skill_write` (it composes the header and refuses to overwrite by
accident). From an MCP client — where these are not tools — write the file itself, which
is the same bytes:

```bash
curl -sS -X POST http://<board>:8080/mcp -H 'Content-Type: application/json' -d \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fs_write","arguments":
    {"path":"/skills/deploy.md",
     "text":"# deploy\ndescription: roll the portal forward\n1. snapshot\n2. flash\n"}}}'
```

### No tools of their own

Deliberately. `fs_ls`, `fs_read` and `fs_write` already reach both paths, and a wrapper
per verb would add a definition to every request and one more choice for a small model —
the same reason `net_*` and `fs_*` are not MCP tools either.

| Task | How |
|---|---|
| see the skills | `fs_ls {path:"/skills"}` — the prompt already lists them by name and description |
| load one | `fs_read {path:"/skills/deploy.md"}` |
| add or change one | `fs_write {path:"/skills/deploy.md", text}` with the header above |
| remove one | the file goes away with the workspace (`fs_write` on an empty file leaves an empty skill, so an empty file is not a deletion) |
| change the instructions | `fs_write {path:"/AGENTS.md", text}` |

What you give up without the wrappers: nothing checks the name or that a `description:`
line exists. The index is forgiving (it falls back to the first prose line), so a
hand-written file still shows up — but a file named `Deploy Notes.md` is invisible
because the stem has a space. Keep names to `a-z A-Z 0-9 _ -`.

From an MCP client, the same call, since these paths are ordinary files:

```bash
curl -sS -X POST http://<board>:8080/mcp -H 'Content-Type: application/json' -d \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fs_write","arguments":
    {"path":"/skills/deploy.md",
     "text":"# deploy\ndescription: roll the portal forward\n1. snapshot\n2. flash\n"}}}'
```

## Why not just put everything in the system prompt

Because of what it displaces. The prompt is rebuilt every turn from the workspace and the
per-run budget is small: `/AGENTS.md` gets 1 KB, the skill index about 1.2 KB, and the
rest of the prompt is instructions and the transcript. A 6 KB "here is everything" prompt
would push the tool schemas into the long tail of the model's attention — the same failure
that made `manifest_describe` useless while it returned 8 KB (see the changelog).

## Related

- [agent-loop.md](agent-loop.md) — the loop contract, including `ToolHost`
- [manifest.md](manifest.md) — the other self-configuring surface (hardware, not prose)

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
agents_md {"action":"set","text":"GPIO12 is the pump relay, active low."}

# a skill, written the same way
skill_write {"name":"pulse_pump","description":"switch the pump for a measured pulse",
             "text":"1. confirm the request names a duration\n2. bench_pulse {ms}\n3. read power_read before and after"}
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

### Tools

| Tool | Arguments | Returns |
|---|---|---|
| `skill_list` | — | `{count, skills:[{name, description, bytes}]}` |
| `skill_read` | `{name}` | `{name, path, bytes, text}`; an unknown name lists the ones that exist |
| `skill_write` | `{name, text, description?, replace?}` | writes `/skills/<name>.md` with a `# name` header and the description line; **an existing skill needs `replace: true`** |
| `skill_delete` | `{name, confirm: true}` | removes the file; the index follows on the next turn |
| `agents_md` | `{action: get\|set, text?}` | the stored text, how many bytes reach the prompt, and whether the file exists |

`skill_write` composes the header itself, so a skill written this way is always
findable: the description you pass (or the `description:` line in `text`, or the first
prose line) becomes the line in the prompt index. Refusals are specific — a name outside
the charset, a file over 2 KB, or an existing skill without `replace: true` each say what
to change. `skill_delete` needs `confirm: true` because the file does not come back.

Writing one by hand is still allowed and still works — `fs_write /skills/deploy.md` with
the same header — but nothing in that path checks the name, the size, or that a
`description:` line exists.

These are **agent tools only** — they are not in the board's MCP `tools/list`, the
same way `fs_*` and `net_*` are not. An MCP client that wants to read or write them can
call `fs_ls` / `fs_read` / `fs_write` on the same paths, which is what the paths are for.

## Why not just put everything in the system prompt

Because of what it displaces. The prompt is rebuilt every turn from the workspace and the
per-run budget is small: `/AGENTS.md` gets 1 KB, the skill index about 1.2 KB, and the
rest of the prompt is instructions and the transcript. A 6 KB "here is everything" prompt
would push the tool schemas into the long tail of the model's attention — the same failure
that made `manifest_describe` useless while it returned 8 KB (see the changelog).

## Related

- [agent-loop.md](agent-loop.md) — the loop contract, including `ToolHost`
- [manifest.md](manifest.md) — the other self-configuring surface (hardware, not prose)

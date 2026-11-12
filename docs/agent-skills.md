# Instructions and skills on the board

Two things an agent needs that are neither a tool nor a line of the transcript: the
rules of *this* installation, and procedures it should follow when they apply. Both live
on the board's file workspace, so the same files serve the portal, an MCP client and the
agent itself — there is no second store to keep in sync.

| Path | Role | Cost in the prompt |
|---|---|---|
| `/AGENTS.md` | project instructions, always applied | first 1024 bytes, every turn |
| `/skills/<name>/SKILL.md` | one skill per directory, with its own files | `name: description (path)` |
| `/skills/<name>.md` | a skill that needs no other file | the same line |

The body of a skill reaches the model only when it opens the file. On a board with
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

A skill is a **directory** (`/skills/<name>/`) with a `SKILL.md`, and whatever else it
needs next to it. That is the shape the host uses, and it is the reason a skill is not
just a paragraph: a pinout, a table of registers, an acceptance script the model runs by
hand with `gpio_write` all belong to the procedure, and they are `fs_read` away.

```markdown
/skills/deploy/SKILL.md
---
name: deploy
description: roll the portal forward without losing the transcript
---
1. Read `/AGENTS.md` and confirm the board is not mid-turn.
2. `fs_write` a note to `/notes/deploy-<date>.md` first.
3. Build, flash, then `board_status` and one `bme280_read`.
The pinout to check after flashing is in `pinout.md` next to this file.

/skills/deploy/pinout.md
GPIO12 relay (active low), GPIO0 boot button, GPIO48 RGB.
```

A skill that needs nothing else can be one file: `/skills/quick.md`. Both shapes are
indexed the same way; the index line carries the path, so the model does not have to
guess which one it is looking at.

Rules, such as they are:

- The **name is the directory** (`deploy`), and it must be `a-z A-Z 0-9 _ -`, at most 32
  characters. It is what appears in the prompt and what the model asks for.
- `description:` comes from YAML front matter (`---` block), from a `description:` line,
  or from the first prose line — in that order. With none of those the skill is still
  indexed, with an empty description.
- Any other file in that directory is the skill's own material. It is not indexed and not
  injected; the procedure tells the model when to read it.
- The index carries at most 12 skills. The rest are on the workspace but unnamed.
- No includes, no scripts that run by themselves. A skill is a procedure and its
  attachments, read by an agent with the file and GPIO tools it already has.

Write one with `fs_write` — there are no dedicated tools (see below).

### No tools of their own

Deliberately. `fs_ls`, `fs_read` and `fs_write` already reach both paths, and a wrapper
per verb would add a definition to every request and one more choice for a small model.

| Task | How |
|---|---|
| see the skills | `fs_ls {path:"/skills"}` — the prompt already names each one with its path |
| load one | `fs_read {path:"/skills/deploy/SKILL.md"}`, and any file next to it |
| add or change one | `fs_write {path:"/skills/deploy/SKILL.md", text}` — parent directories are created |
| remove one | `fs_rm {path:"/skills/deploy/SKILL.md", confirm:true}`; the index drops it on the next turn |
| change the instructions | `fs_write {path:"/AGENTS.md", text}` |

What you give up without the wrappers: nothing checks a name, a header, or that a skill
directory has a `SKILL.md`. The index is forgiving (front matter, then a `description:`
line, then the first prose line), so a hand-written file still shows up — but a directory
named `Deploy Notes` is invisible, because the name has a space. Keep names to
`a-z A-Z 0-9 _ -`.

From an MCP client, the same call, since these paths are ordinary files:

```bash
curl -sS -X POST http://<board>:80/mcp -H 'Content-Type: application/json' -d \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fs_write","arguments":
    {"path":"/skills/deploy.md",
     "text":"# deploy\ndescription: roll the portal forward\n1. snapshot\n2. flash\n"}}}'
```

## Why not just put everything in the system prompt

Because of what it displaces. The prompt is rebuilt every turn from the workspace and the
per-run budget is small: `/AGENTS.md` gets 1 KB, the skill index about 1.2 KB, and the
rest of the prompt is instructions and the transcript. A 6 KB "here is everything" prompt
would push the tool schemas into the long tail of the model's attention — the same failure
that made `manifest_describe` useless while it returned 8 KB, which is why it now answers in
sections.

## Related

- [agent-loop.md](agent-loop.md) — the loop contract, including `ToolHost`
- [manifest.md](manifest.md) — the other self-configuring surface (hardware, not prose)

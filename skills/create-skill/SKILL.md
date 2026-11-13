---
name: create-skill
description: write a new skill for this board when a procedure had to be worked out
---
# create-skill

A skill is a file on the workspace: `/skills/<name>/SKILL.md`. The prompt carries one line
per skill (`name: description (path)`); the body is read only when you open it with `read`.

## The header

Every `SKILL.md` starts with the same block, and it is what the index reads:

```text
---
name: blink
description: blink the onboard LED three times
---
```

`name` repeats the directory, `description` is the one line the prompt shows; both are
required. The body is a `#` heading after the closing `---`. Without the header a file is
still indexed (a `description:` line, then the first prose line), but the header is what
marks it a skill rather than a note.

## When to write one

A procedure you would otherwise work out again: the register sequence that wakes a
display, the order of reads that avoids a bus fault.

## When not to

- A fact about the board (a pin, an address) is a memory: `/memory/<topic>.md` plus a
  pointer line in `/MEMORY.md`.
- Something a tool already does. Read its description first.

## How

1. `shell {"command":"ls /skills"}`: the name is the directory, `a-z A-Z 0-9 _ -`, 32 max.
2. `write` the file with the header above; parent directories are created.
3. `read` it back: `name` and `description` must be there and short.
4. `ls /skills` again and confirm it appears.

```text
write {"path":"/skills/blink/SKILL.md","text":"---\nname: blink\ndescription: blink the LED three times\n---\n# blink\n1. gpio_write {pin:48, level:1}\n2. delay {ms:200}\n3. gpio_write {pin:48, level:0}\n4. repeat 1-3 three times\n"}
```

## House rules

- Imperative sentences, one step per line; the reader is an agent.
- Name every tool the procedure uses, with the exact call in a fenced block.
- No secrets: no passwords, no tokens, no keys.
- Under 2 KB. A larger file cannot be written at all.
- Supporting material goes beside it (`/skills/blink/pins.md`); say when to read it.

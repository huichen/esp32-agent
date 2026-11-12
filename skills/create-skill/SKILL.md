# create-skill

description: write a new skill for this board when a procedure had to be worked out

A skill is a file on the board's workspace: `/skills/<name>/SKILL.md`. The prompt carries
one line per skill (`name: description (path)`); the body is read only when you open it
with `read`.

## When to write one

A procedure you would otherwise work out again: the register sequence that wakes a
display, the order of reads that avoids a bus fault. Write it when the next session will
need the same steps and will not remember this turn.

## When not to

- A fact about this board (a pin, an address, a wiring quirk) is a memory, not a skill:
  `/memory/<topic>.md` plus one pointer line in `/MEMORY.md`.
- A one-off, or something a tool already does. Read the tool's description first.

## How to write it

1. `ls /skills` through `shell`: the name is the directory, `a-z A-Z 0-9 _ -`, at most 32
   characters.
2. `write` the file; parent directories are created for you.
3. `read` it back: the `description:` line must be there and short, because it is the only
   part that reaches the prompt.
4. `ls /skills` again and confirm the name appears.

```text
write {"path":"/skills/blink/SKILL.md","text":"# blink\ndescription: blink the onboard LED three times\n1. gpio_write {pin:48, level:1}\n2. delay {ms:200}\n3. gpio_write {pin:48, level:0}\n4. repeat steps 1-3 three times\n"}
shell {"command":"ls /skills"}
read {"path":"/skills/blink/SKILL.md"}
```

## House rules

- Imperative sentences, one step per line; the reader is an agent.
- Name every tool the procedure uses and give the exact call in a fenced block.
- No secrets: no passwords, no tokens, no keys.
- Under 2 KB. A larger file cannot be written at all.
- Put supporting material beside the skill (`/skills/blink/pins.md`) and say when to read
  it.

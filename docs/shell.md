# `shell`: one command line

A shell, not a bag of tools with an `op` field. One call, one line, parsed like a shell:

```json
{"command": "ls /notes"}
{"command": "grep GPIO /"}
{"command": "curl https://example.com/"}
{"command": "ping -c 3 192.168.3.1"}
```

Words, quotes and `--flags` are understood. `read`, `write` and `edit` stay separate tools
because they are the three most common operations and a wrapper would only add a layer.

## The commands

| Group | Commands |
|---|---|
| Files | `ls rm cp mv grep zip unzip` |
| Reading and slicing text | `cat wc head tail cut sort uniq tr tee touch du strings od` |
| Network | `curl wget nslookup nc ping ifconfig netstat route arp` |
| Numbers and time | `sleep usleep jot crc32` |
| Looking at the board | `df free uptime date uname ps hexdump xd hostname whoami id env setenv unsetenv` |
| Hashing and encoding | `sha256sum md5sum base64enc base64dec` |
| The registry | `type which help` |
| The agent itself | `system_prompt get\|set\|append\|clear` |

Names follow NuttX's shell where one exists, so anyone who has used it can guess correctly.
`cat` is `read` under the other name — NuttX has both and both get typed. `xd` is
`hexdump`, `tail` is `head` from the other end, `which` answers exactly what `type` does.

## There is no pipeline

No pipes, no redirection, no `&&`, no `;`. One call is one command, and there is no second
command for its output to reach. A grammar with a pipeline would promise a process model
the board does not have, so instead:

- a filter names its file: `sort /log.txt`, `wc -l /log.txt`, `tail -n 5 /log.txt`,
  `cut -d , -f 2 /readings.csv`, `tr abc ABC /log.txt`;
- `tee` takes the text it would have read from a pipe as `-i`: `tee /out.txt -i hello`;
- a sequence to feed a loop comes from `jot`, not from a pipeline: `jot 5 10 -1` prints
  `10 9 8 7 6`.

That is the same absence NuttX's `nsh` has, and it is documented here rather than
half-implemented in the parser.

## What each filter means here

`wc [-l|-w|-c]` counts lines, words or bytes; no flag prints all three. `head`/`tail [-n N]`
default to 10 lines. `cut -d <char> -f <n>` is one column, fields numbered from 1,
delimiter tab by default, and it reports how many lines lacked that column instead of
inventing an empty one. `sort` is byte order (`LC_ALL=C`), `-r` reverses, `-u` collapses
duplicates; `uniq` collapses *adjacent* repeats only, as real `uniq` does. `tr` maps
literal characters one for one — `a-z` is three characters here, not a range, and the
answer says so. `strings` prints printable runs of four bytes or more (`-n` changes the
minimum). `od` is `hexdump` in octal, because that is what `od` means.

`touch <path>` creates an empty file if it is missing. There is nothing to update on an
existing one: the workspace stores text and no timestamps, so `touch` answers with
`"mtime": null` and a note rather than pretending it moved a time.

## The board looking at itself

| Command | Answers |
|---|---|
| `hostname [name]` | the interface name, from the config; setting it saves and takes effect at the next reconnect |
| `whoami` | the device name, because the board has one account and no login |
| `id` | uid 0 / gid 0: one user, no passwords, no permission bits |
| `env`, `setenv K V`, `unsetenv K` | variables that live in the `App` for this boot and nowhere else |
| `netstat` | the MCP sessions, the listen port and the counters |
| `route` | the configured gateway, or the one from the DHCP lease |
| `arp` | an empty table, and why it is empty |
| `type`, `which` | whether a name is a shell command, a tool, or nothing |

`env`/`setenv` are deliberately **not** the config's free-form keys: a shell variable is
state a session made up, not a setting, and writing it to NVS would turn a typo into a
boot-time fact. Nothing is persisted, and every one of those answers carries
`"persisted": false`.

Two of these report what the build cannot do instead of making something up:

- `netstat` has no socket table to read — this build does not expose lwIP's — so it lists
  the MCP sessions and the counters it does keep, and says that is what there is.
- `arp` returns `"available": false` with an empty list and a note: the neighbour table is
  not reachable from ESP-IDF here, so an empty table means "cannot see them", not "there
  are none".

## The workspace behind them

One writable area, 8 KB of text, persisted. `write` creates parent paths; `edit` refuses when
`find` matches zero or several times, so an ambiguous edit cannot rewrite the wrong place;
`rm` and `mv` need `confirm`. `zip` writes stored entries and returns the archive as base64
because a tool result is capped at 2 KB — `unzip` reads stored and deflated entries and checks
each CRC. `du` totals it file by file; there are no directories to total separately.

`sleep <s>` and `usleep <ms>` really wait, and the wait is capped at 10 s: one loop serves
the MCP endpoint, the portal and the agent, so an uncapped sleep would look like a hung
device. `crc32` reuses the checksum the zip writer already has, so there is one of them.

## When a command is wrong

The usage line comes from the same table the help text does, so the two cannot disagree:

```text
shell {"command":"rm /x"}          → usage: rm <path> --confirm
shell {"command":"cp /a"}          → usage: cp <from> <to>     (a missing argument says it too)
shell {"command":"sudo ls"}        → `sudo` is not a shell command, then every usage line
shell {"command":"cp -h"}          → usage: cp <from> <to>        (help answers before validation)
shell {"command":"help"}           → all 55 commands
```

`help` is generated from the command table, so a command that was added is in it, and a
command with no usage line is not possible: there is a test that walks the table and fails
on either.

## `curl` and the network

`curl <url>` is a GET; `-d <body>` makes it a POST; `-f raw` keeps the document as it arrived
instead of reducing HTML to text. The reduction is worth knowing about: tags are stripped,
block elements become newlines, common entities decoded, so a 2 KB result carries the article
and not the navigation. `wget -o <path>` saves the body to the workspace instead.

`ifconfig` reports the *configured* gateway and DNS rather than the lease, which is what
answers "did my override take effect". `ping` sends real ICMP through ESP-IDF's ping session.

Related: [scripting](scripting.md) · [HTTP API](http-api.md)

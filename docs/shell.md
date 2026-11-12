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

The declaration says what this is and how a line is written; the **index of 57 commands is
not in it**. `{"command": "shell", "arguments": {"help": true}}` returns the whole index with
a usage line per command, and `help <command>` returns one. Keeping the index in the
declaration cost about 2 KB on every step of every run, to describe a tool whose entire
interface is a single string.

## The commands

| Group | Commands |
|---|---|
| Files | `ls rm cp mv grep zip unzip` |
| Reading and slicing text | `cat echo wc head tail cut sort uniq tr tee touch du strings od jq` |
| Network | `curl wget nslookup nc ping ifconfig netstat route arp` |
| Numbers and time | `sleep usleep jot crc32` |
| Looking at the board | `df free uptime date uname ps hexdump xd hostname whoami id env setenv unsetenv` |
| Hashing and encoding | `sha256sum md5sum base64enc base64dec` |
| The registry | `type which help` |
| The agent itself | `system_prompt get\|set\|append\|clear` |

Names follow NuttX's shell where one exists, so anyone who has used it can guess correctly.
`cat` is `read` under the other name — NuttX has both and both get typed. `xd` is
`hexdump`, `tail` is `head` from the other end, `which` answers exactly what `type` does.

## Pipes and redirection

One line may carry a pipeline, and what a stage prints is what the next one reads:

```json
{"command": "ls / | grep txt"}
{"command": "cat /log.txt | grep error | wc -l"}
{"command": "echo done >> /log.txt"}
```

`< file` feeds a stage from a file, `> file` writes a stage's output truncating it first, and
`>> file` appends. All three write into the same 8 KB workspace as everything else, so a
redirect that would not fit is refused with the number of bytes it needed and **nothing is
written**.

**The stages** are the commands that read or write lines. `jq cat grep sort uniq tr cut
head tail wc strings od xd hexdump base64enc base64dec sha256sum md5sum crc32 tee` read
stdin; `curl wget ls jot du df free ps uname date uptime echo` only produce, so they may
start a pipeline but not continue one (`echo hi | ls` is refused). Every other command —
`rm`, `touch`, `sleep`, `ping`, `script` — is refused by name: a pipeline is another way
to write one command line, not a second way into the tool table.

Up to **five stages**. Each stage goes through the same handler, the same argument table and
the same credential a single command does, so `ls / | grep txt` cannot reach anything `ls`
could not, and `echo hi | rm /x` is refused exactly as `rm /x` is.

This is not bash:

- no `&&`, no `;` — a word that *is* one of them is refused (`echo a && echo b` does not run
  the second half); inside a sentence they are ordinary characters, so
  `system_prompt set the pump is on GPIO12; never past it` stays one argument;
- no `$(…)`, no backticks, no subshells — refused by name;
- no `$VAR` expansion: `$HOME` is the five characters it is;
- no background jobs: a `&` on its own is refused;
- no `xargs`: it would need a command line inside a command line, and the stage list plus
  `jot` already covers what it is usually reached for.

A trailing `|`, and a `>`/`>>`/`<` with no file name, are errors with the line that says how
a pipeline is written:

```text
cmd | cmd | …  ·  cmd > file  ·  cmd >> file  ·  cmd < file   (one line, up to 5 stages)
```

A quoted pipe is text, not an operator: `echo "a | b"` prints `a | b`.

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

`sed 's/from/to/[g]'` is **one** substitution, applied line by line: `g` replaces every
match on a line, without it only the first. `/` is the usual delimiter and `,` works too
(`|` cannot — the shell splits the command line on it before `sed` sees the script).
There are no addresses, no `d`/`p`, no `-e`, no script file: those make `sed` a language,
and the board already has one. A script that is not a substitution is refused with the
form to write instead.

`awk '{print $1, $2}'` is the field-printing subset: `$0` is the whole line, `$N` a field
(counted from 1), `$NF` the last one. Fields are whitespace-separated unless `--sep <char>`
says otherwise. No patterns, no arithmetic, no `BEGIN`/`END`, no variables — `script` is
where a condition or a loop goes, and a half-`awk` that silently printed nothing would be
the worse of the two tools.

`grep -E <pattern>` is a documented subset of a regular expression: literals, `.`, `^`, `$`,
`*`, `+`, `?`, `[...]` with ranges and `[^...]`, and `\d \w \s` (with their negations).
Groups, alternation, `{n,m}` intervals and back references are refused with the sentence
that says so — `(ERR|WARN)` is a `script` condition here, not a pattern. Without `-E` the
pattern is a literal substring, which is what it always was.

`jq [query] <path>` is a path into a JSON document, not a query language:
`.name`, `.a.b`, `.[0]`, `.a[0].b`, `.[]` and `.length`, one compact value per line, so a
`| head` downstream sees one result. `-r` prints a string without its quotes. It is the
reason `curl <board>/agent.json | jq .endpoints[0].path` works at all — a pipeline of
`grep` does not survive nested objects.

A filter with no file reads the pipe, a filter with a file ignores the pipe (as `sort a.txt`
ignores stdin in a shell), and a filter with neither is an error: it has nothing to read.
`echo` joins its words with one space; `-n` leaves the newline off. `tee <path>` writes what
it is handed and passes it on; on its own, `-i <text>` is what a pipe would have provided.

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
shell {"command":"ls / |"}         → a pipeline cannot end with `|`, then the pipeline usage
shell {"command":"sudo ls"}        → `sudo` is not a shell command, then every usage line
shell {"command":"cp -h"}          → usage: cp <from> <to>        (help answers before validation)
shell {"command":"help"}           → all 57 commands
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

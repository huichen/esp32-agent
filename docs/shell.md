# `shell`: one command line

A shell, not a bag of tools with an `op` field. One call, one line, parsed like a shell.
Each line below is a separate argument object for the `shell` tool:

```jsonl
{"command": "ls /notes"}
{"command": "grep GPIO /"}
{"command": "curl https://example.com/"}
{"command": "ping -c 3 192.168.3.1"}
```

Words, quotes and `--flags` are understood. `read`, `write` and `edit` stay separate tools
because they are the three most common operations and a wrapper would only add a layer.

The declaration says what this is and how a line is written; the **index of 57 commands is
not in it**. Calling `shell` with `{"help": true}` returns the whole index with
a usage line per command, and `help <command>` returns one. Keeping the index in the
declaration cost about 2 KB on every step of every run, to describe a tool whose entire
interface is a single string.

## The commands

| Group | Commands |
|---|---|
| Files | `ls rm cp mv grep zip unzip` |
| Reading and slicing text | `cat echo wc head tail cut sort uniq tr tee touch du strings od jq` |
| Network | `curl wget nslookup nc ping ifconfig netstat route arp` |
| Numbers and time | `sleep usleep jot crc32`, and the `time <command>` prefix |
| Looking at the board | `df free uptime date uname ps hexdump xd hostname whoami id env setenv unsetenv` |
| Hashing and encoding | `sha256sum md5sum base64enc base64dec` |
| The registry | `type which help` |
| The agent itself | `system_prompt get\|set\|append\|clear` |
| Scripts | `sh <path> [args…]` — run a board script file through the same interpreter as the `script` tool; `--name value` becomes `args.name`, the rest is `argv` |

Names follow NuttX's shell where one exists, so anyone who has used it can guess correctly.
`cat` is `read` under the other name — NuttX has both and both get typed. `xd` is
`hexdump`, `tail` is `head` from the other end, `which` answers exactly what `type` does.

## Pipes and redirection

One line may carry a pipeline, and what a stage prints is what the next one reads:

```jsonl
{"command": "ls / | grep txt"}
{"command": "cat /log.txt | grep error | wc -l"}
{"command": "echo done >> /log.txt"}
```

`< file` reads a stage's input from a file; `> file` replaces a file with the stage's output,
and `>> file` appends. Writes share the 256 KiB per-file, 4 MiB aggregate text and
path/file-count workspace budgets. A refused edit/admission restores that command's RAM
edit; it does not undo an earlier successful command in a `;` sequence. Accepted writes
report `queued: true` / `saved: false`, not completed storage publication.

Pipeline stdout has a separate 8 KiB policy, not the agent's 64 KiB tool-text ceiling.
See [storage](storage.md) for acknowledgement and rollback boundaries.

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

- no `&&` or `||` conditions;
- up to four commands separated by `;` run in order, stopping on the first error. A
  semicolon splits only when the following word names a known shell command; inside
  a sentence such as `system_prompt set the pump is on GPIO12; never past it` it is
  ordinary text. Quoted or backslash-escaped semicolons never split commands, and an
  escaped quote does not end its quoted word;
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

One RAM workspace, backed by an index and one LittleFS value per file on firmware;
configuration/secrets remain in NVS. Text is limited to 256 KiB per file and 4 MiB in total,
with at most 1,024 files and additional canonical-path/metadata limits. The old five-value
`ws1` JSON format is readable for migration, not the active capacity policy.

`write` creates parent paths; `edit` refuses zero or multiple matches; `rm` and `mv` need
`confirm`. `du` totals file text, not the heap or the physical filesystem's metadata.
`zip` writes stored ZIP32 entries and returns base64. Its current inline archive cap is
1,400 bytes **before** base64, including ZIP headers/names; admission is checked before
copying workspace payloads. `unzip` supports stored/deflated entries and checks CRCs,
with independent 3,072-byte payload/name budgets and 128 entries. These compact archive
policies are not the 64 KiB tool-result ceiling or evidence that PSRAM cannot hold more.

`sleep <s>` and `usleep <ms>` really wait. How long they may wait is not their own cap: a
wait draws on the tool call's remaining budget (`agent.tool_timeout_s`, 30 s by default),
and a request longer than what is left is refused with both numbers rather than silently
shortened. With that budget switched off (`0`) the ten-second fallback applies, because
nothing else would bound a bare `sleep 1e9`. Tool calls hold the shared App lock, so the
wait delays other callers needing that lock. `crc32` reuses the ZIP checksum.
The full persistence and parser contracts are in [storage](storage.md).

### `time <command>`

One line, measured: `time echo hi` runs the command exactly as typed after it and returns
its answer inside a wrapper with the elapsed milliseconds. The prefix measures the whole
line, so `time cat /log | grep error | wc -l` reports one figure for the pipeline rather
than one per stage.

```jsonl
{"command": "time echo hi"}
{"command": "time ls / | wc -l"}
```

```json
{"command": "time echo hi", "ms": 0, "real_s": 0.0, "result": {"text": "hi", "stdout": "hi\n"}}
```

`time` is a prefix, not a command: it has no handler and cannot be piped into. Running it
alone is a wrong line ("`time` needs a command after it"), and `time <cmd> -h` still shows
`cmd`'s usage. The number is wall time on the calling thread, which includes every wait
the command made — it is a measurement, not a CPU-time claim.

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
block elements become newlines and common entities are decoded. The complete network tool
result fits the current 64 KiB envelope, including JSON escaping and metadata; truncation
stays on UTF-8 boundaries and is marked. `wget -o <path>` saves the body to the workspace instead.

`ifconfig` reports the *configured* gateway and DNS rather than the lease, which is what
answers "did my override take effect". `ping` sends real ICMP through ESP-IDF's ping session.

Related: [scripting](scripting.md) · [HTTP API](http-api.md)

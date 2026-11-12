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
| Network | `curl wget nslookup nc ping ifconfig` |
| Looking at the board | `df free uptime date uname ps hexdump xd` |
| Hashing and encoding | `sha256sum md5sum base64enc base64dec` |
| The agent itself | `system_prompt get\|set\|append\|clear`, `help` |

Names follow NuttX's shell where one exists, so anyone who has used it can guess correctly.

## The workspace behind them

One writable area, 8 KB of text, persisted. `write` creates parent paths; `edit` refuses when
`find` matches zero or several times, so an ambiguous edit cannot rewrite the wrong place;
`rm` and `mv` need `confirm`. `zip` writes stored entries and returns the archive as base64
because a tool result is capped at 2 KB — `unzip` reads stored and deflated entries and checks
each CRC.

## When a command is wrong

The usage line comes from the same table the help text does, so the two cannot disagree:

```text
shell {"command":"rm /x"}          → usage: rm <path> --confirm
shell {"command":"sudo ls"}        → `sudo` is not a shell command, then every usage line
shell {"command":"cp -h"}          → usage: cp <from> <to>        (help answers before validation)
shell {"command":"help"}           → all 26 commands
```

## `curl` and the network

`curl <url>` is a GET; `-d <body>` makes it a POST; `-f raw` keeps the document as it arrived
instead of reducing HTML to text. The reduction is worth knowing about: tags are stripped,
block elements become newlines, common entities decoded, so a 2 KB result carries the article
and not the navigation. `wget -o <path>` saves the body to the workspace instead.

`ifconfig` reports the *configured* gateway and DNS rather than the lease, which is what
answers "did my override take effect". `ping` sends real ICMP through ESP-IDF's ping session.

Related: [scripting](scripting.md) · [HTTP API](http-api.md)

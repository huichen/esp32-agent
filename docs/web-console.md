# The console

One page, served by the board, no build step and nothing fetched from the internet: about
58 KB of HTML, CSS and JavaScript compiled into the firmware.

| URL | Serves |
|---|---|
| `http://<board>/` | the console, and `/mcp` on the same port |
| `https://<board>/` | the same routes over TLS, certificate generated on the board |

Both stay open by default. Port 80 is the way back in when a certificate is wrong, so a TLS
misconfiguration costs a URL and not the board. `mcp.plain_http: false` closes it (see
[security](security.md)); with it off, only `https://<board>/` answers.

## Panels

| Panel | What you do there |
|---|---|
| Chat (the page itself) | talk to the model; each turn shows the answer and folds its tool calls and reasoning into one line you can open |
| Devices | what the probe found, pins and addresses, `Probe devices`, and the candidates waiting to be adopted |
| Tools | every tool with its input schema, and why one is hidden |
| Agent | endpoint, model, key, protocol, thinking level, temperature, context and output size; and the schedule (timed prompts and sensor rules) |
| Files | the whole workspace as one tree; tap a file to read it |
| Terminal | one command line on the board, through the `shell` tool |
| Settings | WiFi with gateway and DNS overrides, the portal PIN, and the ring log |

The page keeps a bearer token from `POST /api/auth` and retries once when a write is refused,
so the PIN is asked for once per session and not per click.

## Terminal

The last panel is a terminal for the board, and it is the only one that does not use a REST
route: a command goes to `POST /mcp` as `tools/call` on `shell`, in one MCP session the page
keeps for as long as the tab is open. That is deliberate — it is the same door an external
agent uses, so a command typed here behaves exactly like one a model calls, including the
range checks, the disables and the tool budget. Nothing has a PTY: the board runs one
command line, answers, and returns.

| Typed | What happens |
|---|---|
| `ls /`, `cat /MEMORY.md`, `df`, `free`, `ps`, `date`, `curl … \| head -n 5` | the board's `shell` tool, see [shell](shell.md) |
| `help` | what the terminal can do, in the page |
| `clear` (or `Ctrl-L`) | empty the screen, in the page |
| `!name {json}` | any tool, e.g. `!board {"command":"chip_temp"}` or `!indicators {"command":"led_set","arguments":{"color":"#00ff00"}}` |

Up and down walk the history and stop at the line being typed rather than losing it. The
answer is printed as a person reads it — one string field (`echo`, `cat`, `jq`) prints that
string, a directory listing prints as names — and anything else prints as indented JSON,
which is the shape the tool actually returned. Errors go up in red with the board's own
sentence, and the session is dropped so the next command opens a fresh one: a reboot or an
expired `mcp.session_ttl_s` costs a command, not a reload.

The scrollback lives in the page and is capped at 400 entries. The board's RAM is not the
constraint here; a tab left open overnight is.

## Live updates

`GET /api/agent/stream` is Server-Sent Events with the host agent's vocabulary
(`message_start`, `thinking_delta`, `tool_use`, `tool_result`, `usage`, `text_delta`,
`run_end`). The page subscribes while a run is in flight and falls back to polling
`/api/agent/history` every 20 s when nothing is running, so a turn started by a schedule or
another client still shows up.

Two details that cost an afternoon each when they are wrong:

- the cursor is an **event** sequence (`?after=N`), not a history index; mixing them makes
  the first events of a turn disappear;
- every response carries `Cache-Control: no-store`, because a browser holding the previous
  `app.js` beside the new HTML renders a blank page that looks exactly like a dead board.

## The composer is measured, not asserted in prose

The bottom bar is the page's only input, and CSS can remove it without touching a single
rule anyone would think to re-read: `.attach-chip { display: flex }` outranked the UA
`[hidden] { display: none }`, so the empty chip stayed a 366 px flex item, took the
composer's one line, and pushed the field and the send button off a 390 px phone. Every
declaration said what it meant; only the rectangles were wrong.

`crates/agent-core/tests/page_layout.rs` therefore renders the shipped page in a headless
browser, in a 390 px and a 360 px iframe, and asserts on `getBoundingClientRect()` with
nothing attached, with the attachment chip shown, and after it is removed. A failure prints
the whole measurement, so it needs no screenshot to read.

It is part of `cargo test -p esp32-agent-core`, which means the gate needs a Chromium-family
browser (Chrome, Edge or Chromium). Set `ESP32_AGENT_BROWSER` to the executable when it is
not in a standard location; without one the test fails rather than skipping, because a
skipped layout test is how this bug came back.

## Screenshots

`scripts/console-shot.py` captures the console with headless Chrome/Chromium — the same
browser the layout tests measure with, found the same way (`ESP32_AGENT_BROWSER`, then the
usual install paths). It needs no dependency and no driver:

```bash
python scripts/console-shot.py --url http://192.168.3.7 --out chat.png
python scripts/console-shot.py --url http://192.168.3.7 --out chat-tall.png --size 390x1400
python scripts/console-shot.py --url 'http://192.168.3.7/?panel=devices' --out devices.png \
    --size 900x1200
```

| Flag | Meaning |
|---|---|
| `--size WxH` | viewport. Chrome's `--screenshot` captures the viewport, so a **taller** size is how you get more of the conversation |
| `--wait MS` | page time before the shot (default 4000). The console fetches its history; too short a wait captures an empty chat |
| `--scale N` | device scale factor (default 2, so the PNG is retina-sized and still reads in a README) |

The browser runs against a throwaway profile, so the user's own Chrome session is not
touched and a first-run dialog cannot end up in the picture.

### Deep links

`?panel=devices` (or `#devices`) opens that panel on load: `devices`, `tools`, `agent`,
`files`, `terminal`, `settings`. An unknown name opens nothing rather than an empty sheet.
This exists because a headless browser cannot click, and because a bookmark to "the tools
tab" should work — a screenshot of a panel is the same mechanism.

Two things a screenshot must not contain: the WiFi password (the settings panel shows
`__set__`, never the value) and the access-point name of a network you would rather not
publish. Capture the chat or Devices/Tools panels, not Settings, when the picture is going
somewhere public.

## The certificate

Generated on first boot (EC P-256, self-signed, valid 2020–2035 so a 1970 clock still works),
kept in NVS, fingerprint at `GET /api/tls` and in `/agent.json`. Browsers warn; compare the
fingerprint before clicking through.

Related: [security](security.md) · [HTTP API](http-api.md) · [troubleshooting](troubleshooting.md)

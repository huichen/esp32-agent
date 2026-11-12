# The console

One page, served by the board, no build step and nothing fetched from the internet: about
58 KB of HTML, CSS and JavaScript compiled into the firmware.

| URL | Serves |
|---|---|
| `http://<board>/` | the console, and `/mcp` on the same port |
| `https://<board>/` | the same routes over TLS, certificate generated on the board |

Both stay open. Port 80 is the way back in when a certificate is wrong, so a TLS
misconfiguration costs a URL and not the board.

## Panels

| Panel | What you do there |
|---|---|
| Chat (the page itself) | talk to the model; each turn shows the answer and folds its tool calls and reasoning into one line you can open |
| Devices | what the probe found, pins and addresses, `Probe devices`, and the candidates waiting to be adopted |
| Tools | every tool with its input schema, and why one is hidden |
| Agent | endpoint, model, key, protocol, thinking level, temperature, context and output size |
| Schedule | timed prompts and sensor rules |
| Logs | the ring buffer, newest first |
| Settings | WiFi with gateway and DNS overrides, the portal PIN, reboot, factory reset |

The page keeps a bearer token from `POST /api/auth` and retries once when a write is refused,
so the PIN is asked for once per session and not per click.

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

## The certificate

Generated on first boot (EC P-256, self-signed, valid 2020–2035 so a 1970 clock still works),
kept in NVS, fingerprint at `GET /api/tls` and in `/agent.json`. Browsers warn; compare the
fingerprint before clicking through.

Related: [security](security.md) · [HTTP API](http-api.md) · [troubleshooting](troubleshooting.md)

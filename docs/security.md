# Security: who can talk to the board

The board is on a LAN, so the credential is local and there is exactly one of it: a PIN.
`README`'s threat model and the design constraints are in [SECURITY.md](../SECURITY.md); this
page is the operating manual.

## One PIN, three callers

| Caller | Credential |
|---|---|
| Console | `POST /api/auth {"pin":"…"}` returns a bearer token the page keeps and retries with |
| Script against the API | `X-Portal-Pin: <pin>`, or the same bearer token |
| MCP client | `Authorization: Bearer <mcp.token>`; the portal PIN is accepted when no MCP token is set |

With `auth.portal_pin` set, writes and the reads that expose the transcript, the prompt, the
config and the logs require it. Status, devices, tools, schedule and `/agent.json` stay open
so monitoring does not need the secret.

Tokens live in RAM: a reboot revokes every one of them.

## TLS

Port 443 serves the same routes with a certificate the board generates on first boot (EC
P-256, self-signed, 2020–2035 so a 1970 clock still verifies). The fingerprint is at
`GET /api/tls` and inside `/agent.json` — compare it with the one the browser shows before
clicking through. Port 80 stays open on purpose: a bad certificate should cost a URL, not the
board.

Outbound TLS (model calls, `curl https://…`) verifies the server certificate against the
ESP-IDF bundle; there is no "skip verification" switch and there will not be one.

## What is deliberately not defended

- Anyone on the LAN can reach port 80 and the open GETs. The board is a bench tool, not a
  service on the internet; put it behind a reverse proxy if it must be reachable from outside.
- A PIN is a shared secret with no rotation policy and no lockout. Guessing costs the attacker
  a few requests per second against a board that answers in tens of milliseconds, so a short
  PIN is a bad PIN.
- `shell` and `script` can move pins. That is the point of the board; the PIN is what separates
  "the agent doing its job" from "a passer-by doing it".

Related: [HTTP API](http-api.md) · [web console](web-console.md) · [SECURITY.md](../SECURITY.md)

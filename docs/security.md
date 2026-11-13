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

## Tool switches

`mcp.disabled_tools` is enforced at execution, not just removed from `tools/list`.
MCP and the on-board agent share the check; scripts, family commands and `device_invoke`
check their targets too. A disabled family also refuses its legacy member names.
`read`/`fs_read`, `write`/`fs_write` and `edit`/`fs_edit` share switches. File-content
filters and script-file loading require read permission; editing, copying, moving,
`wget`, `tee`, `touch`, base64 output and redirection require the underlying write
permission. An explicit disable answers with an error rather than a successful empty result.

`minimal` and `generic` are listing choices, not disables: an otherwise enabled legacy
name remains callable. These switches govern tools, not the owner's PIN-authorized HTTP
configuration routes. A caller that can change the configuration can change the switches.

## TLS

Port 443 serves the same routes with a certificate the board generates on first boot (EC
P-256, self-signed, 2020-2035 so a 1970 clock still verifies). The fingerprint is at
`GET /api/tls` and inside `/agent.json` — compare it with the one the browser shows before
clicking through.

Port 80 stays open **by default**: a bad certificate should cost a URL, not the board.
`mcp.plain_http: false` (a checkbox in Settings, or `POST /api/config`) closes it and leaves
only https. Two consequences, both deliberate:

- the board refuses the setting when it has no certificate and says so in the log — a device
  that answers on no port is not more secure, only unreachable;
- with http off, the ways back in are the USB cable (`provision` over USB-Serial-JTAG) and a
  factory reset. Nothing on the network will get you in if the certificate stops working.

## What is deliberately not defended

- Anyone on the LAN can reach port 80 and the open GETs. The board is a bench tool, not a
  service on the internet; put it behind a reverse proxy if it must be reachable from outside.
- A PIN is a shared secret with no rotation policy and no lockout. Guessing costs the attacker
  a few requests per second against a board that answers in tens of milliseconds, so a short
  PIN is a bad PIN.
- `shell` and `script` can move pins. That is the point of the board; the PIN is what separates
  "the agent doing its job" from "a passer-by doing it".

Related: [HTTP API](http-api.md) · [web console](web-console.md) · [SECURITY.md](../SECURITY.md)

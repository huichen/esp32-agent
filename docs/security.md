# Security: who can talk to the board

The board has two separately configured credentials: `auth.portal_pin` for the console
REST API and `auth.mcp_token` for MCP. The threat model and design constraints are in
[SECURITY.md](../SECURITY.md); this page is the operating manual.

## Portal PIN and MCP token

| Caller | Credential |
|---|---|
| Console | `POST /api/auth {"pin":"…"}` returns a portal bearer token the page keeps and retries with |
| Script against REST | `X-Portal-Pin: <pin>`, `?pin=…`, or `Authorization: Bearer <portal token>`; a bearer containing the PIN is also accepted |
| MCP client | `Authorization: Bearer <auth.mcp_token>` when configured; otherwise the portal credential applies |

REST routes enforce the following when `auth.portal_pin` is nonempty:

| Surface | Portal credential required? |
|---|---|
| State-changing REST writes | Yes; `/api/auth` itself is the login route |
| `GET /api/config`, `/api/agent/config`, `/api/sched`, `/api/agent/history`, `/api/agent/prompt` | Yes |
| `GET /api/files/tree`, `/api/files/read`, `/api/attachment/<name>` | Yes |
| Status, memory pools, TLS identity, devices, tools, sessions, clients and discovery | No |
| `GET /api/logs`, `/api/agent/events`, `/api/agent/stream` | Yes |

MCP POST and DELETE share the `mcp.enabled` and credential gate: a disabled service returns
`503`. A configured `auth.mcp_token` requires a real Bearer scheme and matching token;
a bare token, another scheme or a portal credential is not a substitute. If the MCP token
is empty, MCP falls back to the configured portal credential. If both are empty, it is
open. MCP tool calls and resource reads inherit this transport gate, not individual REST
read checks.

For `EventSource`, which cannot set an authorization header, the protected GET/HEAD read
routes also accept `?token=<minted portal token>`; URL-encode the token value. This
parameter accepts only an issued, unexpired portal token, not a raw PIN, and cannot
authorize a write or MCP DELETE. The portal uses it for the run stream.

Portal bearer tokens expire after 12 hours and live in RAM, so a reboot revokes them.
The configured MCP token is a persistent secret, not one of those session tokens.
`GET /api/config` masks its secrets as `__set__`; `GET /api/agent/config` omits `api_key`
and reports `api_key_set`. A redaction placeholder is not a new credential value.

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
P-256, self-signed, fixed validity 2020-2035 so generation does not depend on the board's
unsynchronized clock). The fingerprint is at
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
- A PIN is a shared secret with no rotation policy and no lockout; a short PIN is easier
  to guess. The protection is a credential check, not rate-limited login.
- `shell` and `script` can move pins. A dedicated MCP token takes precedence over portal
  credentials on that interface. A caller allowed to reconfigure the board can also
  change tool switches and credentials.

Related: [HTTP API](http-api.md) · [web console](web-console.md) · [SECURITY.md](../SECURITY.md)

# HTTP and MCP interfaces

One port serves both the web console and the MCP endpoint (default `8080`, changeable
through `config.mcp.port`).

## 1. MCP endpoint

`POST /mcp` (changeable through `config.mcp.path`). Streamable HTTP, without SSE.

```bash
# list tools
curl -sS -X POST http://<ip>:8080/mcp \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

# call a tool
curl -sS -X POST http://<ip>:8080/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"board_status","arguments":{}}}'
```

Supported JSON-RPC methods: `initialize`, `ping`, `tools/list`, `tools/call`,
`resources/list`, `resources/read`, `resources/templates/list`, `prompts/list` (empty),
`logging/setLevel`, `completion/complete` (empty). A notification (no `id`) returns `202`
with no body. Batch arrays are handled as JSON-RPC batches.

| Header | Behaviour |
|---|---|
| `Mcp-Session-Id` (response) | Allocated on `initialize` or the first request; the client may reuse it |
| `MCP-Protocol-Version` | The server version comes back in the `initialize` response |
| `Authorization: Bearer <token>` | Required when `auth.mcp_token` is set, otherwise 401 |
| `Connection: keep-alive` | Supported; a single connection closes after 32 requests |
| `Transfer-Encoding: chunked` | Supported (Node's fetch uses it) |
| `Expect: 100-continue` | Answers 100 first, then reads the body |

`GET /mcp` → `405` (use POST); `DELETE /mcp` → end that session.

CORS is on by default (`mcp.cors`), so a browser MCP client can connect directly.

## 2. Web and configuration REST

| Method | Path | Purpose |
|---|---|---|
| GET | `/` | Console (embedded HTML/CSS/JS, no external links) |
| GET | `/healthz` | `{ok, uptime_s, devices}` |
| GET | `/api/status` | Board, memory, WiFi, MCP, device count, counters |
| GET | `/api/config` | Configuration (secrets shown as `__set__`) |
| POST | `/api/config` | Partial update (JSON of the same shape) |
| GET | `/api/devices` | Device table + unidentified addresses + buses |
| POST | `/api/devices/probe` | Probe again |
| GET | `/api/tools` | Tool table, including why anything is hidden |
| POST | `/api/tools` | `{mode: auto|generic|all, disabled: [...]}` |
| GET | `/api/manifest` | Current manifest + revision |
| POST | `/api/manifest` | `{manifest, mode?, dry_run?}` |
| GET | `/api/manifest/schema` | `manifest_schema` + `primitives`, i.e. the `schema` and `exec` sections of `manifest_describe` (an HTTP response has no 2 KB cap) |
| GET | `/api/wifi/scan` | Scan |
| POST | `/api/wifi/connect` | `{ssid, password}`, save and reconnect |
| POST | `/api/wifi/forget` | Clear the credentials |
| GET | `/api/logs?n=&since=` | Ring log |
| GET | `/api/catalog` | Driver catalog |
| GET | `/api/sessions` | MCP sessions |
| GET | `/api/clients` | Client configuration, ready to copy |
| POST | `/api/auth` | `{pin}`, check the console PIN |
| POST | `/api/reboot` | Reboot |
| POST | `/api/factory-reset` | Clear configuration and manifest, then reboot |

### Agent and orchestration

| Method | Path | Purpose |
|---|---|---|
| POST | `/api/agent/message` | `{text}` — queue a user message; answers `202` with the queued count |
| GET | `/api/agent/history?after=<n>` | Transcript entries after `n`, plus `usage` |
| POST | `/api/agent/reset` | Clear the conversation |
| GET | `/api/agent/config` | Model connection, `tick_ms`, `last_run`/`total` usage |
| POST | `/api/agent/config` | Update `base_url` / `model` / `api_key` / `tick_ms` |
| GET | `/api/agent/events?after=<seq>` | The run journal as JSON (polling) |
| GET | `/api/agent/stream?after=<seq>` | The same journal as **SSE**: replay, then follow. 503 with `Retry-After: 5` when two streams are already open |
| GET | `/api/sched` | Scheduled tasks and sensor rules |
| POST | `/api/sched` | Replace them (or set `agent.tick_ms` behaviour through `/api/agent/config`) |

`POST /selftest/chat/completions` is not part of the API: it is an OpenAI-compatible stub
that answers as a scripted model, served before the `App` lock so a self-test cannot
deadlock. It exists so the loop can be verified without an API key.

Write operations require `X-Portal-Pin` (or `?pin=`) when `auth.portal_pin` is set; reads
do not.

## 3. Resources (`resources/*`)

| URI | Content |
|---|---|
| `mcp://capabilities` | Current capabilities, visible tools, hidden tools and the capability each one lacks |
| `mcp://devices` | Device table, unidentified addresses, bus scan results |
| `mcp://devices/<id>` | One device in detail |
| `mcp://manifest` | Current manifest + revision |
| `mcp://config` | Configuration with secrets redacted |
| `mcp://logs` | Recent log lines |
| `mcp://catalog` | Driver catalog |

## 4. Tool result shape

```json
{ "jsonrpc":"2.0","id":2,
  "result": { "content":[{"type":"text","text":"{…pretty json…}"}],
              "structuredContent": { … },
              "isError": false } }
```

A failing tool still returns `result`, with `isError: true` and the message and code in the
body — so the model can see why and fix its arguments instead of hitting a protocol-level
error. Only protocol errors (unknown method, malformed JSON) use the `error` object.

## 5. Connecting a client

- **Claude Desktop / Cursor and anything else that speaks HTTP**:
  `{"mcpServers":{"esp32-agent":{"type":"http","url":"http://<ip>:8080/mcp"}}}`
- **stdio-only clients**: use a bridge such as `mcp-remote` or `mcp2serial`.
- **Your own script**: see the curl above; `/api/clients` returns a ready-made command.

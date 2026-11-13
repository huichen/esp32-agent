# HTTP and MCP interfaces

One port serves both the web console and the MCP endpoint (default `80`, changeable
through `config.mcp.port`).

Port **443** serves the same routes over TLS, with a certificate the board generated for
itself on first boot (EC P-256, stored in NVS, fingerprint at `GET /api/tls`). Both ports
share **one accept thread**, so a TLS client is served while plaintext clients wait in the
listen backlog for the duration of one request — a deliberate trade: a thread per TLS
connection does not fit in this board's internal RAM alongside the agent, and the failure
arrives as `ERR_CONNECTION_RESET` in a browser. TLS connections are not kept alive, for
the same reason.

`/agent.json` reports the scheme that answered: fetched over https its `base_url` and the
MCP `initialize` instructions both say `https://…:443`, not the plaintext port.

## 0. Read this first: `GET /agent.json`

Before anything else, fetch the map. It is written for an **external agent**, not a person:
every route with its method, what it takes, what it returns, whether it needs the portal
PIN, plus the MCP surface (transport, session header, methods, resources, a worked
example), the error shape, and the limits that apply to a caller.

```bash
curl -sS http://<ip>:80/agent.json | jq '.endpoints[] | "\(.method) \(.path) — \(.summary)"'
```

`GET /sitemap.json` is an alias. The document is generated from the same table the router
is tested against: a test calls every documented route on a real `App` and fails on a 404,
so the map cannot promise a route the firmware does not serve. The dynamic half —
`base_url`, tool count, whether a PIN or MCP token is set, the model endpoint — comes from
the running board, so the same document is correct on every unit.

## 1. MCP endpoint

`POST /mcp` (changeable through `config.mcp.path`). Streamable HTTP, without SSE.

```bash
# list tools
curl -sS -X POST http://<ip>:80/mcp \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

# call a tool
curl -sS -X POST http://<ip>:80/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"board","arguments":{"command":"status"}}}'
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
| `Connection: keep-alive` | Supported. An idle socket is parked (up to 8 of them) and dropped after 30 s; the next request on it is answered by the accept thread |
| `Transfer-Encoding: chunked` | Supported (Node's fetch uses it) |
| `Expect: 100-continue` | Answers 100 first, then reads the body |

`GET /mcp` → `405` (use POST); `DELETE /mcp` → end that session.

CORS is on by default (`mcp.cors`), so a browser MCP client can connect directly.

## 2. Web and configuration REST

| Method | Path | Purpose |
|---|---|---|
| GET | `/` | Console (embedded HTML/CSS/JS, no external links) |
| GET | `/healthz` | `{ok, uptime_s, devices}` |
| GET | `/agent.json` | The map for an external agent: every route, the MCP surface, conventions and limits (`/sitemap.json` is an alias) |
| GET | `/api/status` | Board, memory, WiFi, MCP, device count, counters, `agent.thread` — `running`, `not started`, or `did not start: <why>` — `threads[]`, each task with the stack it has left at its deepest point (`high_water_kb`), and `store`: `{pending, written, failed}` plus `writer`, `last_failure` (`{key, op, err, err_name, at_ms}` or `null`), `refused_keys` (per-key counts), `refused_other` and `space` (`{used_entries, free_entries, total_entries}`) — see [a store write was refused](troubleshooting.md#a-store-write-was-refused) |
| GET | `/api/memory` | The heap pool by pool: `{available, pools: [{pool, note, total_kb, free_kb, largest_kb, min_free_kb}]}` for internal, DMA and PSRAM (a chip that registers no IRAM heap, like the S3, leaves that row out); `available: false` with a `why` on a platform with one heap |
| GET | `/api/tls` | The board's own certificate: `{enabled, port, fingerprint, error}` |
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
| GET | `/api/files/tree` | The whole workspace in one answer: every directory (derived from the paths) and file with `size_bytes`, plus `total_bytes` / `total_max` / `file_max`. What the portal's Files tab reads |
| GET | `/api/files?path=<dir>` | One directory of the workspace, directories first (the same rows `ls` returns) |
| GET | `/api/files/read?path=<file>` | One file as text |
| GET | `/api/catalog` | Driver catalog |
| GET | `/api/sessions` | MCP sessions |
| GET | `/api/clients` | Client configuration, ready to copy |
| POST | `/api/script` | Run a script: `{source, async?}`; the answer carries `result`/`output`/`steps`, or `{id, poll}` with `async: true` ([scripting](scripting.md)) |
| GET | `/api/script/<id>` | One script job: `state` (`queued`/`running`/`done`/`failed`), `result`, `output`, `error` |
| POST | `/audio/pcm` | Play PCM as it arrives: raw PCM16LE mono at 24000 Hz, chunked or with `Content-Length`, at most 30 s, never buffered ([streaming playback](audio.md#6-streaming-playback-post-audiopcm)) |
| POST | `/av` | Play audio and draw frames from one body: `tag|u16le len|payload` records — `A` audio, `F` 512-byte 128x32 bitmap, `E` end — so a soundtrack and its animation stay in step ([sound with a picture](audio.md#8-sound-with-a-picture-post-av)) |
| GET | `/api/attachment/<name>` | The file attached to the last message, with its own mime type ([attachments](#5-attachments)) |
| POST | `/api/auth` | `{pin}`, check the console PIN |
| POST | `/api/reboot` | Reboot |
| POST | `/api/factory-reset` | Clear configuration and manifest, then reboot |

`/assets/app.js` and `/assets/style.css` are the console's two files, served separately from
the HTML so a browser can cache them; everything else the page needs is inside the single
`GET /` response.

### Agent and orchestration

| Method | Path | Purpose |
|---|---|---|
| POST | `/api/agent/message` | `{text}` — queue a user message; answers `202` with the queued count |
| GET | `/api/agent/history?after=<n>` | Transcript entries after `n`, plus `usage`. Each tool call carries its `arguments`; past 1024 bytes they arrive as a string with `arguments_elided_bytes`, so a client can say what it is not showing |
| POST | `/api/agent/reset` | Clear the conversation |
| GET | `/api/agent/config` | Model connection, `tick_ms`, `last_run`/`total` usage |
| POST | `/api/agent/config` | Update `base_url` / `model` / `api_key` / `tick_ms` |
| GET | `/api/agent/prompt` | The system prompt as the model receives it, memory and skills included ([context layers](context.md)) |
| POST | `/api/agent/prompt` | The standing orders: `{action: set\|append\|clear, text}` |
| POST | `/api/agent/forget` | `{keep: N}`, keep the last N exchanges and drop the rest ([memory](agent-memory.md)) |
| GET | `/api/agent/events?after=<seq>` | The run journal as JSON (polling) |
| GET | `/api/agent/stream?after=<seq>` | The same journal as **SSE**: replay, then follow. 503 with `Retry-After: 5` when two streams are already open |
| GET | `/api/sched` | Scheduled tasks and sensor rules |
| POST | `/api/sched` | Replace them (or set `agent.tick_ms` behaviour through `/api/agent/config`) |

`POST /selftest/chat/completions` is not part of the API: it is an OpenAI-compatible stub
that answers as a scripted model, served before the `App` lock so a self-test cannot
deadlock. It exists so the loop can be verified without an API key.

Write operations require `X-Portal-Pin` (or `?pin=`) when `auth.portal_pin` is set; reads
do not.

### Streaming audio

`POST /audio/pcm` is the one route whose body is read as it arrives instead of being
buffered: a request body is capped at 128 KB, which is 1.3 s of audio, and a tool result at
2 KB, which is 43 ms — neither can carry a song
([audio](audio.md#6-streaming-playback-post-audiopcm)). The body is raw PCM, so the headers
say what it is and how long it may last:

```bash
# 16-bit mono at 24 kHz: the board plays it while it arrives and answers when it ends
sox song.mp3 -t raw -e signed -b 16 -c 1 -r 24000 - | \
  curl -sS -X POST 'http://<ip>:80/audio/pcm' \
    -H 'Content-Type: audio/pcm;rate=24000' \
    -H 'Transfer-Encoding: chunked' \
    --data-binary @-
# {"ok":true,"rate_hz":24000,"channels":1,"bits":16,"bytes":491520,"samples":245760,"ms":10240,...}
```

`Content-Type` must start with `audio/pcm` (`415` otherwise), the PIN is required when one
is configured, and only 24000 Hz is wired, so `?rate=` accepts that value alone (`400`
otherwise). Past 30 s the upload is refused rather than truncated: send the next piece as a
second request. There is one speaker, so one upload at a time — a second waits in the
listen backlog like any other request.

`POST /av` is the same route with a picture attached: the body is a sequence of
`tag | u16le length | payload` records (`A` audio, `F` a 1-bpp frame, `E` end), consumed in
order, so the frame written in front of an audio record is on screen while that audio plays
([sound with a picture](audio.md#8-sound-with-a-picture-post-av)).
`tools/av_push.py` writes that body from a PCM file and a frame strip.

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

## 5. Attachments

A message can carry files, which the board stores and hands back — it does not show them
and does not put the bytes in the prompt:

```bash
curl -sS -X POST http://<ip>:80/api/agent/message -H 'Content-Type: application/json' -d '{
  "text": "what does this say?",
  "attachments": [{"name":"shot.png","mime":"image/png","data_b64":"iVBORw0…"}]}'
```

- At most **2** attachments, each ≤ **24 KB** decoded; more is `413`.
- The model is told a file arrived — the user message gets the line
  `[attached: shot.png, image/png, 17 kB — fetch with GET /api/attachment/shot.png]` — and
  nothing else. Only the last message's attachments are kept.
- `GET /api/attachment/<name>` serves the bytes with that mime type, and needs the PIN when
  one is set (it is one of the reads that expose the conversation).
- `image_read {name?, prompt?}` is the tool that asks a model about the file: it sends the
  image to the configured endpoint as a `data:` URL with `vision` forced on, and returns
  the model's answer (1800 bytes). It needs a configured model and refuses without one,
  so it is not a way to read a file locally.

## 6. Connecting a client

- **Claude Desktop / Cursor and anything else that speaks HTTP**:
  `{"mcpServers":{"esp32-agent":{"type":"http","url":"http://<ip>:80/mcp"}}}`
- **stdio-only clients**: use a bridge such as `mcp-remote` or `mcp2serial`.
- **Your own script**: see the curl above; `/api/clients` returns a ready-made command.

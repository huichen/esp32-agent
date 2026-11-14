# HTTP and MCP interfaces

One port serves both the web console and the MCP endpoint (default `80`, changeable
through `config.mcp.port`).

Port **443** serves the same routes over TLS, with a certificate the board generated for
itself on first boot (EC P-256, stored in NVS, fingerprint at `GET /api/tls`). Both ports
share **one accept thread** and bounded request workers. Parsing, handlers, TLS
handshakes and stream bodies run on workers, not the listener. Three ordinary workers
plus one reserved plaintext control worker reserve four 16 KiB stacks on ESP-IDF
at startup and reuse them; requests do not create pthreads. Idle plaintext sockets
occupy no worker. TLS connections are not kept alive.
See [concurrency and lock ownership](concurrency.md) for pool limits and freshness.

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

`endpoints` is the surface a client should call. Four of them now have an MCP tool that does
the same job — `/api/agent/message`, `/api/agent/history`, `/api/agent/forget` and
`/api/agent/config` are `agent_say`, `agent_history`, `agent_forget` and
`agent_config_get`/`agent_config_set` — and a caller that already speaks MCP should take the
tool. The rest have none, and that is deliberate: attachments carry bytes, the event stream
needs a socket, and minting a portal token needs the PIN, which is the one thing that must
not travel in a tool call.
`portal_endpoints` is the console's own half, and each row names the tool that
does the same job — a model that reads this document is meant to take the tool and leave
the route to the browser. The document uses a static endpoint/settings table plus
runtime values such as `base_url`, tool count, credential-set flags and model endpoint.
A route-existence test calls documented paths on a real `App`; that alone does not prove
that the map is complete or its auth, formats and budgets are current. The interface
suite in [scripts](../scripts/README.md#simulator-interface-regressions) checks those
contracts independently. The exact credential coverage is in [security](security.md).

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
| `Authorization: Bearer <token>` | POST and DELETE require `auth.mcp_token` when configured (real Bearer scheme, otherwise 401); if unset, they use the portal credential gate |
| `Connection: keep-alive` | Supported. An idle socket is parked within the six-connection shared budget and dropped after 30 s; the next request runs on a bounded worker |
| `Transfer-Encoding: chunked` | Supported (Node's fetch uses it) |
| `Expect: 100-continue` | Answers 100 first, then reads the body |

`GET /mcp` → `405` (use POST); `DELETE /mcp` → end the session named by `Mcp-Session-Id`.
POST and DELETE both return `503` when `mcp.enabled` is false and apply the same auth
check. A dedicated MCP token takes precedence over portal credentials; both empty means
no auth. See [security](security.md#portal-pin-and-mcp-token).

HTTP framing is checked before dispatch: duplicate length/transfer headers, both
`Content-Length` and `Transfer-Encoding`, invalid lengths, unsupported transfer encodings,
and incomplete headers are refused. Chunked bodies require valid hexadecimal sizes, each
chunk's CRLF, and the final blank line; trailers share the 8 KiB header budget. A malformed
or interrupted upload is not a completed recording or playback. Buffered bodies are at
most 1 MiB, held once rather than copied through another full-size receive buffer; `/audio/pcm`
and `/av` stream through a small receive buffer and apply their own media limits.

Outbound HTTP has one host-tested decoder in `http_client.rs`, used by the simulator and
firmware transports. It handles fixed-length, chunked and close-delimited replies,
including informational status lines, with 16 KiB of headers and 64 KiB of decoded body.
An oversized or torn reply is an explicit error, not a partial JSON success. The request
head is written separately from the borrowed body, avoiding a second large model-request
buffer. Network tool results also reserve metadata and JSON escaping inside the 64 KiB
agent result budget, so their truncation marker remains inside valid JSON.

CORS is on by default (`mcp.cors`), so a browser MCP client can connect directly.

## 2. REST for the console, and the tool that replaces each route

What follows exists because the console is a browser: it cannot hold an MCP session, so it
calls REST. Where a route only repeats a tool, the table says which tool, and
`/agent.json` keeps those rows out of `endpoints` for exactly that reason. The exceptions
to "call the tool instead" are the two places a browser does something a model must not
or cannot: it signs in, and it serves its own assets. Neither a restart nor a factory reset
is on that list, because neither is offered anywhere — see
[capabilities](capabilities.md).

| Method | Path | Purpose |
|---|---|---|
| GET | `/` | Console (embedded HTML/CSS/JS, no external links) |
| GET | `/agent.json` | The map for an external agent: the routes to call, the MCP surface, conventions and limits |
| GET | `/api/status` | Board, memory, WiFi, MCP, device count, counters, `agent.thread` — `running`, `not started`, or `did not start: <why>` — `threads[]`, each task with the stack it has left at its deepest point (`high_water_kb`), `memory.pools` (the heap pool by pool: `[{pool, note, total_kb, free_kb, largest_kb, min_free_kb}]`, empty on a platform that cannot split it), and `store`: `{pending, written, failed}` plus `writer`, `last_failure` (`{key, op, err, err_name, at_ms}` or `null`), `refused_keys` (per-key counts), `refused_other` and `space` (`{used_bytes, free_bytes, total_bytes}`, or `{used_entries, free_entries, total_entries, used_bytes, free_bytes, total_bytes}` on NVS) - every number and where it comes from is in [what the store reports](troubleshooting.md#what-the-store-reports) — see [a store write was refused](troubleshooting.md#a-store-write-was-refused) |
| GET | `/api/tls` | The board's own certificate: `{enabled, port, fingerprint, error}` |
| GET | `/api/config` | Configuration (secrets shown as `__set__`) |
| POST | `/api/config` | Partial update (JSON of the same shape). `tz_offset_h` takes whole hours from UTC, `-12..=14`, and is refused rather than rounded outside that |
| GET | `/api/devices` | Device table + unidentified addresses + buses |
| POST | `/api/devices/probe` | Probe again |
| GET | `/api/tools` | Tool table, including why anything is hidden |
| POST | `/api/tools` | `{mode: auto|generic|all, disabled: [...]}` — no tool changes visibility, so this one stays a route |
| GET | `/api/wifi/scan` | Scan |
| POST | `/api/wifi/connect` | `{ssid, password}`, save and reconnect |
| GET | `/api/logs?n=&since=` | Ring log |
| GET | `/api/files/tree` | The whole workspace in one answer: every directory (derived from the paths) and file with `size_bytes`, plus `total_bytes` / `total_max` / `file_max`. What the portal's Files tab reads |
| GET | `/api/files/read?path=<file>` | One file as text |
| POST | `/audio/pcm` | Play PCM as it arrives: raw PCM16LE mono at 24000 Hz, chunked or with `Content-Length`, at most 30 s, never buffered ([streaming playback](audio.md#6-streaming-playback-post-audiopcm)) |
| POST | `/av` | Play audio and draw frames from one body: `tag|u16le len|payload` records — `A` audio, `F` a 512-byte 128x32 or 1024-byte 128x64 bitmap, `E` end — in order ([sound with a picture](audio.md#8-sound-with-a-picture-post-av)) |
| GET | `/api/attachment/<name>` | The file attached to the last message, with its own mime type ([attachments](#5-attachments)) |
| POST | `/api/auth` | `{pin}`, check the console PIN |

`GET /healthz` returns public JSON health, device count and uptime. It and
`GET /api/status` use a published snapshot without acquiring App. Responses include
`snapshot`, `age_ms` and `stale`; hardware/counters may be old during a long tool,
but uptime advances and `agent.run_active` is read from live control state.

`/assets/app.js` and `/assets/style.css` are served separately from the console HTML.
All three carry `Cache-Control: no-store` so old assets are not reused with new firmware.
The console loads no external asset dependencies.

Configuration POST responses distinguish `saved` from `queued`. On the firmware's queued
backend, `queued: true` / `saved: false` means the change was admitted, not that flash
has completed. Inspect `store.pending`, `failed` and `last_failure` for later outcomes;
an accepted configuration response is not a durability acknowledgement. Successful
file-mutating tool results likewise carry `queued: true`, `saved: false` and a
`persistence_note`; they apply the RAM edit without waiting for the writer. A later
command failing does not undo an earlier admitted edit, and its error reports that
partial effect. See [storage](storage.md#workspace-publication-and-rollback).

### Agent and orchestration

| Method | Path | Purpose |
|---|---|---|
| POST | `/api/agent/message` | `{text}` — queue a user message; answers `202` with the queued count |
| GET | `/api/agent/history?after=<n>` | Transcript entries after `n`, plus `usage`. Each tool call carries its `arguments`; past 1024 bytes they arrive as a string with `arguments_elided_bytes`, so a client can say what it is not showing |
| POST | `/api/agent/reset` | Clear the conversation. `409` while a model turn is in flight: its question would be gone and its answer would arrive in a thread that no longer holds it |
| POST | `/api/agent/cancel` | Ask the running turn to stop: `{cancelling, run_active}`. Cooperative — the loop stops between model calls and between tool calls, so an answer already on the wire is dropped rather than aborted; `run_end` follows either way. `cancelling: false` means there was nothing to interrupt |
| GET | `/api/agent/config` | Model connection, `tick_ms`, `last_run`/`total` usage |
| POST | `/api/agent/config` | Update model connection, `protocol`, `reasoning`, `vision`, `temperature`, `timeout_s`, context/output/step budgets and `tick_ms`; reads omit `api_key` and report `api_key_set` |
| GET | `/api/agent/prompt` | The system prompt as the model receives it, memory and skills included ([context layers](context.md)) |
| POST | `/api/agent/prompt` | The standing orders: `{action: set\|append\|clear, text}` |
| POST | `/api/agent/forget` | `{keep: N}`, keep the last N exchanges and drop the rest ([memory](agent-memory.md)); `409` under the same rule as `reset` |
| POST | `/api/agent/compact` | Compress the conversation now instead of waiting for the token budget: `{keep?: N}` exchanges stay verbatim, the rest become archive lines; the index it produces is a message at the boundary, not part of the system prompt. `409` under the same rule as `reset`. Costs no model call — see [agent memory](agent-memory.md) |
| GET | `/api/agent/events?after=<seq>` | The run journal as JSON (polling) |
| GET | `/api/agent/stream?after=<seq>` | The same journal as **SSE**: replay, then follow. 503 with `Retry-After: 5` when two streams are already open |
| GET | `/api/sched` | Scheduled tasks and sensor rules |
| POST | `/api/sched` | Replace them (or set `agent.tick_ms` behaviour through `/api/agent/config`) |
| POST | `/api/sched/run` | `{id}` — queue a scheduled task now without moving its timer |

`POST /selftest/chat/completions` is not part of the API: it is an OpenAI-compatible stub
that answers as a scripted model, served before the `App` lock so a self-test cannot
deadlock. It exists so the loop can be verified without an API key.

State-changing REST writes require the portal credential when `auth.portal_pin` is set.
So do configuration (including `/api/agent/config`), schedule, history, prompt, workspace,
attachment, log, run events/stream and script-job reads; see the exact
[auth table](security.md#portal-pin-and-mcp-token). Protected GET/HEAD reads, including
SSE/downloads, also accept a minted portal token as a URL-encoded `?token=` value; a raw
PIN is rejected there, and this parameter cannot authorize mutations. MCP uses its
dedicated token, or the portal gate when that token is unset.

The cancellation route uses a short-lived control lock, not App. Script execution
and script/shell sleeps observe interruption at bounded checkpoints (sleep slices
are 25 ms). Driver and network waits remain cooperative, not forcibly cancelled.
Ending a turn clears the flag; cancellation is effective even with `tool_timeout_s: 0`.

Model `timeout_s` is clamped to 5–180 seconds and passed at request time to Chat,
Responses, Anthropic and `image_read`. The host transport uses one whole-request deadline;
platform DNS resolution cannot necessarily be interrupted by it. This is not a measured
firmware or real-provider latency guarantee.

Each tool call has its own budget: `agent.tool_timeout_s`, clamped to 0–300 seconds and
30 by default, with `0` meaning "no deadline". It is one deadline for the whole call, and
a nested call (a `tool(…)` inside a script, a family command, a shell stage) inherits the
outer one instead of starting a fresh budget. Commands that wait spend it rather than
carrying a cap of their own — `shell sleep`, `script sleep` and the HTTP client ask what is
left — so raising this number is what makes a longer wait possible. A call that begins
after the budget is gone is refused with JSON-RPC `-32007`; one that finishes anyway keeps
its result and adds `deadline_exceeded: {timeout_ms, elapsed_ms, note}`. This is cooperative:
it cannot interrupt work already inside a driver or a socket call, and a tool that ignores
the clock can still overrun its budget.

### Streaming audio

`POST /audio/pcm` and `POST /av` stream their bodies instead of buffering the whole upload.
Ordinary buffered requests have a 1 MiB cap, while `speaker_play.pcm_b64` has its own
**2048 decoded-byte** limit (43 ms at 24 kHz mono). The 64 KiB agent tool-result cap is a
separate context budget, not that playback-argument limit. Each streaming route accepts
at most **30 s / 1,440,000 PCM bytes**; AV also permits **512 KiB** for bitmap/framing bytes
([audio](audio.md#6-streaming-playback-post-audiopcm)). The PCM headers identify the format:

```bash
# 16-bit mono at 24 kHz: the board plays it while it arrives and answers when it ends
sox song.mp3 -t raw -e signed -b 16 -c 1 -r 24000 - | \
  curl -sS -X POST 'http://<ip>:80/audio/pcm' \
    -H 'Content-Type: audio/pcm;rate=24000' \
    -H 'Transfer-Encoding: chunked' \
    --data-binary @-
# {"ok":true,"rate_hz":24000,"channels":1,"bits":16,"bytes":491520,"samples":245760,"ms":10240,...}
```

The MIME essence must be exactly `audio/pcm` (`415` otherwise; optional parameters are
allowed), not a string that merely starts with it. The portal credential is required
when configured, and only 24000 Hz is wired, so `?rate=` accepts that value alone (`400`
otherwise). Past 30 s the upload is refused rather than truncated: send the next piece as
a second request. There is one speaker, so writes to that device remain serialized. A second upload competes for App
between chunks; ordinary workers are bounded and status/cancellation have a separate
plaintext lane. This is not an asynchronous audio mixer.

`POST /av` is the same route with a picture attached: the body is a sequence of
`tag | u16le length | payload` records (`A` audio, `F` a 1-bpp frame, `E` end), consumed in
order, so the frame written in front of an audio record is on screen while that audio plays.
The MIME essence must be exactly `application/x-esp32-av` or `application/octet-stream`.
An `E` record must have zero payload. A partial final header/payload is `400` with
`ok: false` even if HTTP `Content-Length` has been fully received; hardware failures use
`500`. A complete record boundary may end the body without `E` (`ended: false`), and bytes
after a valid `E` are ignored ([sound with a picture](audio.md#8-sound-with-a-picture-post-av)).
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
  "result": { "content":[{"type":"text","text":"{\"ok\":true}"}],
              "structuredContent": { "ok": true },
              "isError": false } }
```

A failing tool still returns `result`, with `isError: true` and the message and code in the
body — so the model can see why and fix its arguments instead of hitting a protocol-level
error. Only protocol errors (unknown method, malformed JSON) use the `error` object.

## 5. Attachments

A message can carry files. The board keeps them in RAM and serves them back; image
attachments also become image content parts when the configured model has `vision: true`:

```bash
curl -sS -X POST http://<ip>:80/api/agent/message -H 'Content-Type: application/json' -d '{
  "text": "what does this say?",
  "attachments": [{"name":"shot.png","mime":"image/png","data_b64":"iVBORw0…"}]}'
```

- At most **2** attachments, each ≤ **24 KiB** decoded; excess size is `413`. Malformed
  base64, duplicate names, wrong field types or invalid metadata are `400`. Names are
  1–128 bytes without control characters; MIME is a bare `type/subtype` of at most 128 bytes.
- Admission is all-or-nothing: a refused message preserves the previous downloads and
  queued message. A successful text-only message clears the download list. A running turn
  retains its own attachments if a newer message is queued.
- Every file adds a note such as
  `[attached: shot.png, image/png, 17 kB — fetch with GET /api/attachment/shot.png]`.
  Non-image files, or images with `vision: false`, send only that note, not their bytes.
- `GET /api/attachment/<name>` serves the latest accepted message's bytes with that MIME
  type; percent-encode the name in the URL. It needs the PIN when one is set (it exposes
  conversation data). Attachments are not saved across reboots.
- `image_read {name?, prompt?}` asks the configured vision model about an attached image,
  without requiring a camera. It uses the selected `chat`, `responses` or `anthropic`
  adapter and returns at most 1800 bytes of answer text. It refuses non-image files,
  disabled vision, oversized prompts, or a missing model/transport before making a call.
  It is a second paid model call, not local OCR.

## 6. Connecting a client

- **Claude Desktop / Cursor and anything else that speaks HTTP**:
  `{"mcpServers":{"esp32-agent":{"type":"http","url":"http://<ip>:80/mcp"}}}`
- **stdio-only clients**: use a bridge such as `mcp-remote` or `mcp2serial`.
- **Your own script**: see the curl above, or the worked example in `/agent.json` under
  `dynamic.mcp.example`.

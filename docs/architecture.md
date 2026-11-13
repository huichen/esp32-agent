# Architecture

## 1. Why three layers

Early device-side MCP projects (including the C++ firmware we learned from) put three
things in one place: the driver, the meaning of a capability, and the MCP glue. Every new
peripheral then meant editing code, compiling, reflashing and reconnecting — and the
reflash costs far more than the few dozen lines it saved.

```
┌──────────────────────────────────────────────────────────────┐
│ MCP layer   protocol.rs / app.rs / http.rs / web/            │
│   Device-agnostic: JSON-RPC, sessions, auth, capability       │
│   filtering, the web console. Written once, shared by all.    │
└───────────────▲──────────────────────────────────────────────┘
                │ compile a manifest → ToolDef (parameter schema + exec closure)
┌───────────────┴──────────────────────────────────────────────┐
│ Manifest layer   manifest.rs (JSON in the NVS `manifest` key) │
│   Declares: which devices exist, which tools they expose, how │
│   primitives compose, where the safety boundaries are.        │
│   Adding a peripheral is often a JSON edit → live, no reflash │
└───────────────▲──────────────────────────────────────────────┘
                │ instantiate by driver id
┌───────────────┴──────────────────────────────────────────────┐
│ Driver layer   drivers/*.rs (compiled into the firmware)      │
│   Only for chips that must run their own protocol,            │
│   calibration or state machine: SSD1306, BME280, …            │
└───────────────▲──────────────────────────────────────────────┘
                │ traits only
┌───────────────┴──────────────────────────────────────────────┐
│ Hardware trait   hardware.rs                                  │
│   Firmware impl (ESP-IDF) / simulator impl (virtual registers)│
└──────────────────────────────────────────────────────────────┘
```

The driver layer does not know MCP exists; the MCP layer knows no chip; the manifest layer
is the glue between them.

## 2. The path of one request

```
phone / model → TCP:80
  http.rs        parse HTTP/1.1 (keep-alive, chunked, 100-continue)
  app.rs         route: /mcp or /api/*
    /mcp POST → auth (optional Bearer) → allocate or reuse Mcp-Session-Id
              → protocol.rs parses JSON-RPC (batches included)
              → dispatch: initialize / tools/list / tools/call / resources/*
              → tools/call: resolve the name → execution checks → ToolCtx → handler
              → wrap the result as content[] + structuredContent
    /api/*    → the REST surface the console uses (status, devices, tools, manifest, logs,
                WiFi, agent)
  settle()       one place to flush at the end of a request: persist config/manifest and
                 rebuild the tool table when needed
```

One `App` behind one lock, because hardware resources are shared. Tool calls are
serialized under that lock, so a blocking hardware call delays most other callers.
`GET /api/status` and `GET /healthz` instead try the lock: if it is free they build and
publish a fresh view; otherwise they serve the last published view with `stale: true`
and its `age_ms`. One independent short mutex protects only the immutable snapshot's
`Arc` swap/clone, never hardware work or JSON construction. Publication at construction
and after routes/tools uses `status_json`; health is a projection of that same view.
Uptime is computed per request from a paired hardware/monotonic clock sample, not frozen
in the cache. See the [snapshot contract](http-api.md#2-web-and-configuration-rest).

Ordinary HTTP requests run on one accept thread; only long-lived event responses get
separate threads. The snapshot shortcut bypasses a worker's live app lock, not a long
call occupying the accept thread itself. Streaming media releases the `App` lock between
pieces/records, but occupies the accept thread for the upload. The remaining live-lock
split is not implemented.

Model calls also run **outside** the lock: a model call takes seconds, so `Agent::begin`
computes the request under the lock, `Agent::apply` consumes the reply under the lock,
and the network wait in between holds nothing.

## 3. Capability filtering: the tool list is "what this board has"

- Each device declares capabilities in `info().caps` (`oled`, `bme280`, `relay`, …).
- Each tool declares its requirements in `requires`; a manifest tool may require nothing.
- `tools/list` emits only tools whose `requires ⊆ board caps + device caps`, minus the
  names the user disabled.

So an absent OLED never shows the model a `display` tool it would then call and fail on.
`tools_mode = "generic"` collapses the device tools further into `device_invoke`, which
keeps a many-peripheral board from eating the context window.

One predicate answers both questions — offered, and why not — and every view reads it: the
model's tool array, `/mcp tools/list`, `/api/tools` and `/api/status`. They used to be
three separate constructions, and they disagreed by six tools. A tool outside
the registry (`shell`, `script`, `read`/`write`/`edit`, `image_read`) is filtered by the
same rule; `/api/tools` and `/api/status` carry the actual unmet precondition, not only
the flag. `image_read` uses an attachment and a configured vision model, not a camera,
so absence of a camera alone does not hide it.

## 4. Hot reload: what takes effect immediately

| Change | How it applies | Restart needed |
|---|---|---|
| Manifest (devices/tools/params/safety) | `settle()` rebuilds the registry; the next request uses it | ❌ |
| Tool enable/disable, tool style | same | ❌ |
| Bus pins, board profile | rebuild and re-probe | ✅ `restart_required: true` |
| WiFi credentials | `wifi_connect` reconnects at once | ❌ |
| MCP port | the main loop notices and restarts only the HTTP server | no full reboot |
| The firmware itself | serial flashing; OTA is not implemented | yes |

A driver that has already opened its bus cannot change pins in place, so a pin or profile
change reports `restart_required: true` rather than quietly keeping the old wiring.

## 5. Key trade-offs

- **Blocking HTTP, no async**: ESP-IDF's std networking is enough; MCP request volume is
  low and a single tool call is serial anyway.
- **One stack, not one thread per connection**: a request is answered on the accept
  thread — 16 KiB of stack on the board, 256 KiB on the host, because it carries the
  heaviest handler (`/api/tools` serialises every tool descriptor). Only a streaming
  response gets a thread of its own (6 KiB on the board, 512 KiB on the host), because it
  outlives the request. `MAX_CONNECTIONS = 6` caps spawned response-stream threads,
  not all open TCP sockets; the journal has a separate two-stream gate. Up to eight
  plaintext/keep-alive sockets are parked for 30 s without a stack. The firmware
  keeps flash-writing/cache-off-sensitive task stacks in internal RAM. PSRAM stacks are
  allowed for suitably constrained tasks: the DNS responder uses one. They are not a
  general replacement for internal stacks.
- **Manifests are validated on the board**: the schema is a manual for the model; what
  actually rejects a call is `check_args` plus `safety`.
- **SSE only where it pays**: `GET /mcp` still answers 405, which the spec allows. The
  agent's run journal is streamed from `/api/agent/stream`, capped at two concurrent
  streams because each holds one of the six connection slots.
- **Session state is tiny**: an id, a last-seen time and a protocol version. It is dropped
  when it expires, never persisted.

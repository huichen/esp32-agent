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
              → tools/call: look up the registry → filter visibility → ToolCtx → handler
              → wrap the result as content[] + structuredContent
    /api/*    → the REST surface the console uses (status, devices, tools, manifest, logs,
                WiFi, agent)
  settle()       one place to flush at the end of a request: persist config/manifest and
                 rebuild the tool table when needed
```

One `App` behind one lock, because the hardware really is singular (one I2C bus, one
panel). Serialising tool calls costs nothing real and removes a whole class of
concurrency bugs. HTTP connections are still served by several threads; they queue only
when they enter `App`.

The one place that had to be relaxed is the agent loop: a model call takes seconds, so it
runs **outside** the lock. `Agent::begin` computes the request under the lock,
`Agent::apply` consumes the reply under the lock, and the network wait in between holds
nothing.

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
three separate constructions, and they disagreed by six tools for months. A tool outside
the registry (`shell`, `script`, `read`/`write`/`edit`, `image_read`) is filtered by the
same rule, so "this board has no camera" reaches the model, the MCP client and the portal
at once; `/api/tools` and `/api/status` carry the sentence, not only the flag.

## 4. Hot reload: what takes effect immediately

| Change | How it applies | Restart needed |
|---|---|---|
| Manifest (devices/tools/params/safety) | `settle()` rebuilds the registry; the next request uses it | ❌ |
| Tool enable/disable, tool style | same | ❌ |
| Bus pins, board profile | rebuild and re-probe | ✅ `restart_required: true` |
| WiFi credentials | `wifi_connect` reconnects at once | ❌ |
| MCP port | the main loop notices and restarts only the HTTP server | no full reboot |
| The firmware itself | OTA / serial | yes |

A driver that has already opened its bus cannot change pins in place, so a pin or profile
change reports `restart_required: true` rather than quietly keeping the old wiring.

## 5. Key trade-offs

- **Blocking HTTP, no async**: ESP-IDF's std networking is enough; MCP request volume is
  low and a single tool call is serial anyway.
- **One stack, not one thread per connection**: a request is answered on the accept
  thread — 20 KB of stack on the board, 256 KB on the host, because it carries the
  heaviest handler (`/api/tools` serialises every tool descriptor). Only a streaming
  response gets a thread of its own (6 KB on the board, 512 KB on the host), because it
  outlives the request. At most six connections are open at once, and a parked keep-alive
  socket costs a descriptor rather than a stack (see `cfg` in `http.rs`). The board figures
  are dictated by internal RAM, which PSRAM cannot supply for stacks.
- **Manifests are validated on the board**: the schema is a manual for the model; what
  actually rejects a call is `check_args` plus `safety`.
- **SSE only where it pays**: `GET /mcp` still answers 405, which the spec allows. The
  agent's run journal is streamed from `/api/agent/stream`, capped at two concurrent
  streams because each holds one of the six connection slots.
- **Session state is tiny**: an id, a last-seen time and a protocol version. It is dropped
  when it expires, never persisted.

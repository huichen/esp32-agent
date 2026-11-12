# Agent loop on the board — frozen contract

Status: brief for implementation. Every worker implements the surface below exactly;
the supervisor integrates, builds and verifies on the bench board.

## 1. What is being built

An agent loop that runs **on the ESP32** and talks to an OpenAI-compatible chat
endpoint over HTTP or HTTPS, using the board's own tools as its tool set:

    user message / scheduled task / sensor rule
        -> queue (depth 1, coalesce)
        -> agent thread: model call -> tool calls -> tool results -> ... -> final text
        -> history (RAM + NVS), visible on the portal

## 2. Constraints that shape the design

- **One OS process, several FreeRTOS tasks.** That is enough: HTTP server threads,
  the existing radio thread, plus one agent thread and one scheduler tick. No
  process isolation; every shared structure is behind the existing `App` mutex or
  a dedicated `Mutex`.
- **One agent run at a time.** The queue holds exactly one pending message; a new
  trigger while a run is in flight *coalesces* into it (newest text wins, source
  list appended). Never two concurrent model calls.
- **TLS for `https://`, on the board.** The transport carries the scheme: plain
  `http://` over a socket (a LAN endpoint, and the board's own
  `/selftest/chat/completions`), `https://` over esp-tls with the ESP-IDF certificate
  bundle. There is no "skip verification" switch. A handshake costs roughly 40 KB of
  heap for the session, which is why the board has PSRAM and why one run still happens
  at a time. Inbound traffic — the portal and `/mcp` on port 8080 — stays HTTP;
  see `SECURITY.md` for why that is a reverse-proxy decision, not an oversight.
- **Bounded everything.** History ring (<= 256 messages / <= 512 KB), tool result
  <= 2 KB (truncated with a marker), model reply <= 8 KB, `max_steps` default 4.
- **No new ESP-IDF components.** The workspace for the file tools is an in-RAM
  map persisted as one JSON blob in NVS (`esp32mcp` namespace, key `ws`), files
  <= 2 KB, whole workspace <= 8 KB. SPIFFS can replace it later behind the same
  trait.

## 3. Modules and exact public surface

### 3.1 `crates/agent-core/src/llm.rs` (worker A)

```rust
pub trait ChatTransport {
    /// POST `body` to `url` with `headers`; return the response body.
    /// POST `body` to `url` (http:// or https://); the scheme is the transport's business.
    /// Implementation enforces its own timeout.
    fn post_json(&mut self, url: &str, headers: &[(&str, &str)], body: &str)
        -> Result<String, String>;
}

#[derive(Clone, Debug, Default)]
pub struct LlmConfig {
    pub base_url: String,   // e.g. "http://192.0.2.10:8899/v1/chat/completions"
    pub model: String,      // e.g. "stub"
    pub api_key: String,    // empty = no Authorization header
    pub max_steps: u8,      // tool iterations, default 4
    pub temperature: f32,   // default 0.2
    pub timeout_s: u32,     // default 45
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall { pub id: String, pub name: String, pub arguments: serde_json::Value }
#[derive(Clone, Debug, Default)]
pub struct ModelReply { pub content: String, pub tool_calls: Vec<ToolCall> }

/// One request/response round trip. `messages` and `tools` are already serialised
/// by the caller as OpenAI `messages` / `tools` arrays.
pub fn chat(
    transport: &mut dyn ChatTransport,
    cfg: &LlmConfig,
    messages: &serde_json::Value,
    tools: &serde_json::Value,
) -> Result<ModelReply, String>;
```

`chat` builds `{"model","messages","tools","tool_choice":"auto","temperature"}`,
sends `Content-Type: application/json` plus `Authorization: Bearer <key>` when the
key is non-empty, and parses `choices[0].message.content` (may be null) and
`choices[0].message.tool_calls[]` (`id`, `function.name`,
`function.arguments` — a JSON *string* that must be parsed; an unparsable one
becomes `{}` and is reported as a tool error, not a panic). A non-2xx body is
returned as `Err` with the status and first 200 chars of the body.

### 3.2 `crates/agent-core/src/agent.rs` (worker A)

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role { System, User, Assistant, Tool }

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub text: String,              // assistant text, or tool result text
    pub tool_calls: Vec<llm::ToolCall>, // assistant messages only
    pub tool_call_id: Option<String>,   // tool messages only
    pub source: String,            // "" | "http" | "schedule:<id>" | "sensor:<id>"
    pub at_ms: u64,
}

/// Everything the loop needs from the board, implemented by `App`.
pub trait ToolHost {
    /// OpenAI `tools` array.
    fn tool_specs(&self) -> serde_json::Value;
    /// Run one tool call; the result is JSON text (<= 2 KB enforced here).
    fn call_tool(&mut self, name: &str, arguments: &serde_json::Value) -> Result<String, String>;
    /// `/AGENTS.md`, already bounded by the host; `None` when there is none.
    fn project_instructions(&self) -> Option<String> { None }
    /// Skills the model may load on demand, as `(name, one-line description)`.
    fn skills(&self) -> Vec<(String, String)> { Vec::new() }
}

pub struct Agent {
    pub history: Vec<Message>,
    pub pending: Option<Message>,  // coalesced user message
    pub last_error: String,
    pub runs: u32,
}

impl Agent {
    pub fn new() -> Self;
    pub fn push_user(&mut self, text: &str, source: &str, now_ms: u64) -> bool; // false = empty
    pub fn messages_json(&self, after: usize) -> serde_json::Value; // {"seq":n,"messages":[...]}
    pub fn reset(&mut self);
    /// Drain `pending`, run the loop, append messages. Returns the final text.
    pub fn run_pending(
        &mut self,
        host: &mut dyn ToolHost,
        transport: &mut dyn ChatTransport,
        cfg: &LlmConfig,
        now_ms: fn() -> u64,
    ) -> Result<String, String>;
    pub fn transcript_json(&self) -> serde_json::Value;
}
```

Rules: system prompt is generated once per run and is not stored in history;
history trimming keeps the system prompt implicit and drops oldest user/assistant
pairs beyond 256 messages; every tool result text is truncated to 2048 bytes with
`"…(truncated)"`; `run_pending` with no pending message is `Ok(String::new())`.

### 3.2a The order of the tool list is part of the contract

A provider caches a request by its longest matching prefix, so the `tools` array has to
be byte-identical between two turns that changed nothing — and as stable as possible when
something does change. `ToolHost::tool_specs` therefore emits three blocks, least
volatile first, each ordered by name:

1. the board's own tools (drivers and system tools) — they change with the hardware or
the firmware;
2. `fs_*` and `net_*` — fixed for a given build;
3. manifest tools — they change whenever the model rewrites one.

Putting the manifest block last is what keeps a mid-turn `manifest_apply` from
reprocessing the whole tool list: only the tail moves. Alphabetical order inside a block
is what makes two boards with the same hardware produce the same bytes, whatever order
the probe happened to enumerate them in. `the_tool_list_is_byte_identical_for_the_same_board`
and `every_tool_reaches_the_model_in_one_shape_and_a_predictable_order` pin both halves.

### 3.3 `crates/agent-core/src/tools/fs.rs` (worker B)

```rust
pub trait Workspace {
    fn list(&self, dir: &str) -> Result<Vec<String>, String>;
    fn read(&self, path: &str) -> Result<String, String>;
    fn write(&mut self, path: &str, text: &str) -> Result<(), String>;
    fn remove(&mut self, path: &str) -> Result<(), String>;
}
pub fn specs() -> serde_json::Value;                    // tools array: fs_ls, fs_read, fs_write, fs_edit, fs_grep
pub fn call(ws: &mut dyn Workspace, name: &str, args: &serde_json::Value)
    -> Option<Result<String, String>>;                   // None = not an fs tool
```

Paths are absolute-ish (`/notes/a.txt`), `/` is the root, no `..`, no symlinks.
`fs_write` creates parent dirs implicitly. `fs_edit` requires exactly one
occurrence of `find`, otherwise it errors (that is the safety property).
`fs_grep` walks files under `path` (default `/`) with a literal substring match.

### 3.4 `crates/agent-core/src/tools/net.rs` (worker B)

`specs()` + `call(transport: &mut dyn ChatTransport, name, args)` for
`net_get {url}` and `net_post {url, body}`; response body truncated to 2 KB;
non-2xx returns the status plus body prefix as `Err`.

The transport passed here is the `App`'s **tool transport** (`App::tool_transport`),
which is a different client from the one the model call uses. That is deliberate: the
model request borrows the driver's client for the length of the turn, so a shared one
would leave `net_*` with nothing to send on — which is exactly what happened while the
simulator never attached one: every `net_get` inside a turn answered "no HTTP transport
is attached". `attach_tool_transport` is what the platform layer calls once at boot.

### 3.5 `crates/agent-core/src/sched.rs` (worker B)

```rust
pub struct ScheduledTask {
    pub id: String,        // stable, e.g. "t1"
    pub prompt: String,
    pub every_s: u32,      // 0 = use at_hhmm
    pub at_hhmm: String,   // "07:30" or ""
    pub enabled: bool,
    pub last_run_ms: u64,
}
pub struct SensorRule {
    pub id: String,
    pub tool: String,          // e.g. "buttons_read"
    pub path: String,          // JSON path inside the tool result, e.g. "boot"
    pub op: String,            // "eq" | "ne" | "gt" | "lt" | "true"
    pub value: serde_json::Value,
    pub prompt: String,
    pub min_interval_s: u32,
    pub last_fire_ms: u64,
    pub enabled: bool,
}
pub fn due(task: &ScheduledTask, now_ms: u64, now_min_of_day: u32) -> bool;
pub fn rule_matches(rule: &SensorRule, value: &serde_json::Value) -> bool;
pub fn json_path(v: &serde_json::Value, path: &str) -> Option<serde_json::Value>;
```

`due`: `every_s > 0` -> `now_ms - last_run_ms >= every_s*1000` (and never fires
when `last_run_ms == 0 && now_ms < every_s*1000`); `at_hhmm` -> fires once per day
when `now_min_of_day == parsed minute` and `now_ms - last_run_ms > 60_000`.
Both functions must be pure (no clock access) — the caller passes time in.

### 3.6 `crates/agent-core/src/app.rs` (worker C)

- `impl ToolHost for App`: `tool_specs()` = the MCP `tools/list` schemas plus
  `tools::fs::specs()` and `tools::net::specs()`; `call_tool` routes fs_* to
  `tools::fs::call`, net_* to `tools::net::call`, anything else to the existing
  `App::call_tool` and returns the text of the MCP result.
- Routes (all under the existing pin/token gate where they mutate):
  - `POST /api/agent/message {"text"}` -> `202 {"queued":true,"seq":n}`
  - `GET /api/agent/history?after=N` -> transcript JSON

## Surviving a power cut

Two mechanisms, one storage: `config.agent.persist_transcript` (default on) and the
`agent.run_in_flight` marker.

The transcript is written as JSON Lines behind a header under `agent.transcript`,
`agent.transcript.1`, ... Each chunk is 3500 bytes with an FNV-1a checksum in its header,
because ESP-IDF refuses a string value above about 4000 bytes and the firmware's NVS
adapter reads through a fixed 4096-byte buffer. A single 16 KB key would pass every host
test and persist nothing on the board.

The stored copy is lossy on purpose: text over 300 bytes, reasoning over 200 and tool
arguments over 256 are cut with the truncation marker, images are not stored at all, and
only the newest messages that fit 16 KB are kept (`dropped: N` in the header, repeated in
the prompt as "Recovered after a reboot"). A blob with a bad checksum, an unknown version,
an edited body or an orphan tool result at the tail is discarded whole and logged, so a
torn write loses the transcript rather than half-adopting it.

Resume works at step granularity. The marker is written before a run and removed when it
ends, before the final save, so a crash can lose a reply but never repeat one. At boot a
marker that survived is consumed and the transcript's last user message is queued again
with `source: "resume"`; the request then carries the tool call and the result that already
landed, so the model answers instead of re-issuing the call.

What a power cut still costs: the model output in flight (the turn is re-asked and paid for
again), a queue entry that never started, and a cut inside the window between clearing the
marker and the final save (one turn lost, never duplicated).
  - `POST /api/agent/reset` -> `{"reset":true}`
  - `GET /api/agent/config` / `POST /api/agent/config` (api_key never echoed)
  - `GET /api/sched` / `POST /api/sched` (add/update/delete one task or rule by id)
- Portal (`src/web/`): an "Agent" panel with a textarea, a send button, the
  transcript rendered as `role: text`, and a schedule table with enable/disable.
  Same vanilla-JS style as the existing panel; no new dependencies.

### 3.6a Whose endpoint and whose key

All three of `base_url`, `model` and `api_key` are **empty in a build**, and the board
cannot reach a model until whoever owns it fills them in at the portal. Nothing is
compiled in and nothing ships in the image.

That is a rule, not an accident of defaults: the endpoint and the credential belong to
the person running the board. This repository's own bench work never points a board at a
maintainer's or an assistant's model provider, and never puts that provider's key on the
device — a bench run uses the board's own `/selftest/chat/completions` endpoint, which
needs no key and never leaves the device. If a demonstration seems to need a real key,
the answer is a scripted endpoint on the board, not somebody else's credential in flash.

`the_firmware_ships_with_no_endpoint_and_no_key` in `llm.rs` pins the default, so
hard-coding an endpoint has to be a deliberate act with a failing test to justify it.

### 3.7 `firmware/src/runner.rs` (worker D, second wave)

Agent thread + `EspHttpClient` transport + NVS-backed `Workspace` + the scheduler
tick that turns due tasks and fired rules into `push_user(...)`. Publishes counters
through `/api/status`.

## 4. Acceptance for the whole feature

1. `cargo test -p esp32-agent-core` (host, includes the existing 100) and
   `cargo clippy -p esp32-agent-core -p esp32-agent-sim -- -D warnings` clean.
2. On the board: post a message, watch the loop call `board_status`, and get a
   final answer from a stub model server on the LAN.
3. A 60 s scheduled task fires with no HTTP request involved.
4. The portal shows the transcript and can toggle a task.

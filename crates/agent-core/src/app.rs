//! The application: config + manifest + devices + tools + sessions, the MCP
//! dispatcher, and the HTTP routes for the MCP endpoint and the HTML portal.
//!
//! One `App` lives behind a single mutex. The hardware is inherently shared (one
//! I2C bus, one display), so serialising tool calls costs nothing real and
//! removes a whole class of concurrency bugs.

use crate::agent::{Agent, AgentStep, ToolHost};
use crate::boards::{self, BoardProfile};
use crate::capability::CapSet;
use crate::config::Config;
use crate::device::Devices;
use crate::drivers;
use crate::error::{code, McpError, Result};
use crate::hardware::Hardware;
use crate::http::{EventSink, EventStreamGate, EventStreamSlot, Handler, Request, Response, StreamBody};
use crate::llm;
use crate::llm::{ChatTransport, LlmConfig};
use crate::logbuf::LogBuf;
use crate::manifest::{Manifest, NVS_KEY_MANIFEST};
use crate::protocol::{self, RpcIn};
use crate::sched::{ScheduledTask, SensorRule};
use crate::state::{make_session_id, ProbeReport, Runtime};
use crate::store::{self, KvStore};
use crate::tool::{ToolCtx, ToolDef, ToolRegistry};
use crate::tools as board_tools;
use crate::tools::family::Family;
use crate::tools::{fs as fs_tools, net as net_tools};
use crate::web;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub type Shared = Arc<Mutex<App>>;

/// NVS keys the agent owns. Separate keys keep a write that fails on one of them
/// from taking the others down with it.
///
/// The names themselves live in [`crate::keys`], where every key the board can produce is
/// declared, assembled and checked against NVS's 15-byte ceiling by a test. A literal
/// added here or anywhere else is a name nothing is watching.
pub const NVS_KEY_AGENT_CFG: &str = crate::keys::AGENT;
pub const NVS_KEY_SCHED: &str = crate::keys::SCHED;
pub const NVS_KEY_WORKSPACE: &str = crate::keys::WORKSPACE;
/// The transcript between boots: a header line and one message per line.
///
/// The board's NVS refuses a single string value above roughly 4000 bytes - the firmware
/// reads through a fixed 4096-byte buffer (`firmware/src/nvs_store.rs`) and ESP-IDF
/// rejects a longer write - so the blob is stored as a few values: this key holds the
/// header and the first part, `transcript.1`, `.2` ... hold the rest. The header carries
/// a checksum over everything after it, so chunks from two different writes are detected
/// instead of concatenated into a transcript that never existed.
///
/// The name is `transcript` because a key is at most [`store::KEY_MAX`] bytes; the longer
/// `agent.transcript` was refused with `ESP_ERR_NVS_KEY_TOO_LONG` on every write, so the
/// board had no transcript on flash at all. There is no migration for it, and no read of
/// the old name: nothing was ever stored under it (`crate::keys`).
pub const NVS_KEY_TRANSCRIPT: &str = crate::keys::TRANSCRIPT;
/// The most bytes one stored value may hold: the board reads NVS through a fixed
/// 4096-byte buffer (`firmware/src/nvs_store.rs`) and ESP-IDF rejects a longer write.
pub const TRANSCRIPT_CHUNK_BYTES: usize = 3500;
/// How many values a full transcript may span, and then some: a 16 KB tail needs five,
/// and the spare room is what keeps a shrinking transcript from colliding with the tail
/// of the previous one.
pub const TRANSCRIPT_CHUNKS_MAX: usize = 8;
/// Written before a run starts and removed when it ends. Its presence at boot is the
/// only evidence that a turn was cut off by a power loss rather than finished.
///
/// `agent.run_in_flight` was 19 bytes and therefore never stored; the marker existed only
/// in the RAM of the run that wrote it. Like the transcript, it needs no migration.
pub const NVS_KEY_RUN_IN_FLIGHT: &str = crate::keys::RUN_IN_FLIGHT;
/// The source a resumed turn carries, so a reply can be told apart from one the user just
/// asked for.
pub const RESUME_SOURCE: &str = "resume";

/// The file tools write into a RAM map that is persisted as one JSON blob, so the whole
/// workspace has to be small enough to serialise next to everything else.
///
/// Both numbers used to be 2 KB and 8 KB because the blob was spread over five NVS values.
/// It is a file on `storage` now, so the ceiling is the partition rather than a value: a
/// single file may be 256 KB and the workspace 4 MB, which is what makes `write` a real
/// editor instead of a note pad. The map still lives in RAM between writes, so this is also
/// a heap budget; 4 MB of text is PSRAM the board has.
pub const WORKSPACE_FILE_MAX: usize = 256 * 1024;
pub const WORKSPACE_TOTAL_MAX: usize = 4 * 1024 * 1024;
/// How the workspace blob is spread over the store.
///
/// Not one value under `ws`: ESP-IDF refuses a string above about 4000 bytes, and a
/// workspace with a few files in it is already past that. The board then logged
/// `nvs write 'ws': ESP_ERR_NVS_VALUE_TOO_LONG` while `write` answered `ok: true` and
/// `read` served the file out of RAM until the next boot - a write that looked fine and
/// silently was not. The blob is now `ws` (header: count, total length, one checksum per
/// chunk) plus `ws.1`, `ws.2`, ... as the transcript is stored, with one difference: the
/// header carries a checksum per chunk, so a rejected blob can name the value that
/// disagrees.
///
/// Five values of 3500 bytes is 17,500 bytes of serialised JSON, against
/// [`WORKSPACE_TOTAL_MAX`] of *text*: JSON escapes a newline to two bytes, so the same
/// 8 KB of Markdown weighs about 10 KB stored. The ceiling is stated here rather than left
/// to arithmetic in a log line; [`store::write_blob`] refuses the blob and says so when it
/// is passed.
pub const WORKSPACE_PLAN: store::Plan =
    store::Plan { tag: "ws1", chunk_bytes: store::CHUNK_BYTES, chunks_max: 5 };

/// The longest `sleep` / `usleep` waits this board will actually wait.
///
/// One loop serves the MCP endpoint, the portal and the agent, so a sleeping command
/// blocks everything else — a `sleep 30` would look like a hung device. The cap is what
/// keeps the command useful without letting it become an outage.
pub const MAX_SHELL_SLEEP_MS: u64 = 10_000;

/// How much of a pipeline's last stage reaches the answer.
///
/// The same reason `hexdump` and `unzip` have budgets: a tool result is about 2 KB, and a
/// pipeline can multiply what a single command would have returned.
pub const PIPE_RESULT_MAX: usize = 8192;

/// How many scheduled prompts and sensor rules the board keeps. The scheduler
/// tick walks both lists on every tick, so this is bounded like everything else.
pub const MAX_TASKS: usize = 8;
pub const MAX_RULES: usize = 8;

/// The longest message the portal may queue: the same bound the agent applies to
/// its own history, so a page can never enqueue what the loop would have to cut.
pub const AGENT_TEXT_MAX: usize = crate::agent::MAX_USER_BYTES;

/// The budget of one event stream, in one place so a test can shrink it instead of
/// waiting two minutes for a close.
#[derive(Clone, Copy, Debug)]
pub struct StreamLimits {
    /// Close after this long with no new journal event.
    pub idle: Duration,
    /// Close after this long whatever happens. `EventSource` reconnects on its own
    /// with `after=<last id>`, so a bounded stream is the design, not a workaround.
    pub total: Duration,
    /// `: ping` cadence, well under the socket and proxy write timeouts.
    pub ping: Duration,
    /// Journal poll interval while waiting for the next event.
    pub poll: Duration,
}

impl Default for StreamLimits {
    /// The frozen contract: 30 s idle, 120 s total, a ping every 15 s.
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(30),
            total: Duration::from_secs(120),
            ping: Duration::from_secs(15),
            poll: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FwMeta {
    pub name: String,
    pub version: String,
    pub build: String,
    /// The git commit the image was built from, as the build script saw it. Empty when the
    /// build did not provide one; the point is that two images differ here, which is the
    /// only cheap way to answer "is the board running what I just flashed".
    pub commit: String,
    /// When it was built, UTC, from the same source.
    pub built_at: String,
}

/// Two images of ~24 kB each is what a phone screenshot downscaled in the browser comes
/// to; anything larger does not fit the request the board can send over TLS.
pub const MAX_ATTACHMENT_BYTES: usize = 24 * 1024;
pub const MAX_ATTACHMENTS: usize = 2;

/// HTTPS on this board: the port it listens on and the SHA-256 fingerprint of the
/// certificate it made for itself. Empty `fingerprint` means no https.
#[derive(Debug, Clone, Default)]
pub struct TlsInfo {
    pub port: u16,
    /// Lower-case hex, colon-separated, as `openssl x509 -fingerprint -sha256` prints it.
    pub fingerprint: String,
    /// Why https is not up, when it is not. Shown on the portal so "no https" is a
    /// statement rather than a mystery.
    pub error: String,
}

impl TlsInfo {
    pub fn enabled(&self) -> bool {
        self.port != 0 && !self.fingerprint.is_empty()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled(),
            "port": self.port,
            "fingerprint": self.fingerprint,
            "error": self.error,
            "how_to_check": "compare this with the certificate the browser shows; it is \
                             SHA-256 over the DER certificate",
        })
    }
}

impl Default for FwMeta {
    fn default() -> Self {
        Self {
            name: protocol::SERVER_NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            commit: String::new(),
            built_at: String::new(),
            build: "host".into(),
        }
    }
}

/// What a script's arguments look like to the language.
///
/// Two shapes, because two callers exist. A map (`{"pin": 12}`) is what an HTTP client sends
/// when it knows the fields. An array is a command line, which is what a shell passes:
/// bare words become `argv`, and `--name value` or `name=value` also land in `args`.
fn script_arguments(given: Option<&serde_json::Value>) -> serde_json::Value {
    match given {
        Some(serde_json::Value::Array(items)) => {
            let mut map = serde_json::Map::new();
            let mut i = 0;
            while i < items.len() {
                let word = items[i].as_str().unwrap_or_default().to_string();
                if let Some(rest) = word.strip_prefix("--") {
                    // `--name value`, or `--name=value`.
                    if let Some((k, v)) = rest.split_once('=') {
                        map.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                    } else if let Some(next) = items.get(i + 1) {
                        map.insert(rest.to_string(), next.clone());
                        i += 1;
                    } else {
                        // A flag with no value is a boolean: `--verbose` means true.
                        map.insert(rest.to_string(), serde_json::Value::Bool(true));
                    }
                } else if let Some((k, v)) = word.split_once('=') {
                    if !k.is_empty() {
                        map.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                    }
                }
                i += 1;
            }
            // `argv` keeps every word, flags included, so a script can walk the command line
            // itself when the `--name value` convention does not fit.
            let positional: Vec<serde_json::Value> = items.clone();
            serde_json::json!({ "__argv": positional, "__args": map })
        }
        Some(other) => other.clone(),
        None => serde_json::Value::Null,
    }
}

/// One run submitted through `POST /api/script`.
///
/// The board has one agent thread, so an asynchronous run is not a second thread: it is a
/// job the platform tick picks up and finishes. That keeps the RAM cost at a string and a
/// result, and makes "async" mean "you do not wait" rather than "the board runs two things
/// at once", which a chip with kilobytes of internal SRAM cannot honour.
#[derive(Clone, Debug, Default)]
pub struct ScriptJob {
    pub id: String,
    pub source: String,
    /// `queued`, `running`, `done`, `failed`.
    pub state: String,
    pub result: serde_json::Value,
    pub output: Vec<String>,
    pub steps: u64,
    pub ms: u64,
    pub error: String,
}

pub struct App {
    pub cfg: Config,
    pub manifest: Manifest,
    pub devices: Devices,
    pub tools: ToolRegistry,
    /// Tool names a family took over: `manifest_apply` -> `manifest apply`.
    ///
    /// The handlers stay in the registry so a script written before the family existed
    /// keeps working; what changes is that the model is not shown five names where one
    /// will do, and the listing explains where each old name went.
    pub family_members: std::collections::BTreeMap<String, String>,
    pub hw: Box<dyn Hardware>,
    pub store: Box<dyn KvStore>,
    pub log: Arc<LogBuf>,
    pub runtime: Runtime,
    pub caps: CapSet,
    pub probe: ProbeReport,
    pub profile: &'static BoardProfile,
    pub profile_exact: bool,
    pub firmware: FwMeta,
    /// Set by the platform layer once the HTTP server is up.
    pub lan_url: String,
    /// Files attached to the most recent message. Replaced, never accumulated: an
    /// attachment belongs to one turn, and the heap is the scarcest thing here.
    pub attachments: Vec<crate::agent::Attachment>,
    /// The board's own certificate, when it serves https on a second port. The
    /// fingerprint is what a person compares against the browser's warning page —
    /// it is the only proof that this board, and not something on the path, answered.
    pub tls: TlsInfo,
    /// The on-board agent: transcript, the one-slot queue and the last error.
    pub agent: Agent,
    /// Where the model lives and how far one run may go. `api_key` is kept in
    /// NVS but never echoed back by the portal API.
    pub llm: LlmConfig,
    /// Scheduled prompts and sensor rules. The scheduler tick (platform layer)
    /// decides when they fire; this file only stores and edits them.
    pub tasks: Vec<ScheduledTask>,
    pub rules: Vec<SensorRule>,
    /// The workspace behind the `fs_*` tools.
    pub workspace: AppWorkspace,
    /// `setenv`/`env`/`unsetenv`: variables that live in this `App` and nowhere else.
    ///
    /// Deliberately *not* in [`Config`]: a shell variable is not a setting, it is state one
    /// session made up, and writing it to NVS would turn a typo into a boot-time fact.
    /// Nothing here is persisted, and `env` says so.
    pub shell_env: std::collections::BTreeMap<String, String>,
    /// Script runs submitted over HTTP, newest last. Bounded: this is a queue for a
    /// person or an external agent, not for anything that retries in a loop.
    pub scripts: Vec<ScriptJob>,
    /// The model-facing tool array. 51 schemas is the largest thing a step used to build,
    /// and it changes only when the manifest, the profile, the disabled set or the probe
    /// does. `RefCell` because `ToolHost::tool_specs` takes `&self`; the whole `App` is
    /// already behind a mutex, so there is no second borrow to lose.
    pub tool_specs_cache: std::cell::RefCell<Option<(u64, Value)>>,
    /// Monotonic counter for job ids; not persisted, because the queue is not either.
    pub script_seq: u64,
    /// Outbound HTTP for the `net_*` tools. The platform layer attaches it once; host
    /// tests leave it empty so nothing reaches the net. It is deliberately **not** the
    /// transport the model call uses: that one is owned by whoever drives the turn
    /// (`drive_agent`), so a `net_get` inside a turn and the model request never
    /// compete for one client — and a turn can still reach the network.
    pub tool_transport: Option<Box<dyn ChatTransport + Send>>,
    /// The transport `agent_run_once` uses when nobody drives the turn from outside.
    /// Platforms use [`drive_agent`] and pass their own, leaving this empty.
    pub model_transport: Option<Box<dyn ChatTransport + Send>>,
    /// Slots for `GET /api/agent/stream`. A stream holds an HTTP connection for its
    /// whole life, so the cap is about the pool, not about this struct.
    streams: EventStreamGate,
    /// The `Arc` this `App` lives in, set by [`App::attach_shared`]. An event stream
    /// outlives the request that opened it, so it polls the journal through this
    /// instead of borrowing `&mut App`. `Weak` on purpose: the `App` must not keep
    /// its own handle alive.
    shared: Weak<Mutex<App>>,
    session_counter: u64,
    /// Whether the platform's agent loop is running. See [`AgentThread`]: a board whose
    /// loop failed to start still answers the portal, MCP and every tool, so the only
    /// honest place to see the failure is here.
    agent_thread: AgentThread,
    /// The writes this `App` has asked for and nobody has performed yet. `store` is a
    /// [`store::QueuedStore`] over this, so no code path in here can reach flash from
    /// whatever thread happens to be running.
    store_queue: store::StoreQueue,
    /// What the platform says about the task that drains the queue.
    store_writer: StoreWriter,
}

/// Whether the thread that runs the agent loop is up.
///
/// The loop lives on a thread the platform spawns, and that spawn can fail: on this board
/// it does when 16 KB of contiguous internal RAM is not there ("could not start the agent
/// thread: Not enough space (os error 12)"). Everything else keeps working — the portal
/// opens, MCP answers, tools run — which is exactly why a line in the log was not enough:
/// from outside, a board whose own agent is dead looks like a healthy board.
///
/// The platform reports the outcome once, through [`App::agent_thread_running`] or
/// [`App::agent_thread_failed`], and both the boot log and `/api/status` read that one
/// value: a decision, not two descriptions of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AgentThread {
    /// No platform has been asked to start one yet (the simulator runs the loop on its
    /// own thread and says so; a board says so until the spawn is attempted).
    #[default]
    NotStarted,
    /// The loop is up.
    Running,
    /// The spawn failed; this is the platform's error, verbatim.
    Failed(String),
}

impl AgentThread {
    /// The string `/api/status` shows: one phrase a person or an agent can branch on.
    pub fn as_str(&self) -> String {
        match self {
            AgentThread::NotStarted => "not started".to_string(),
            AgentThread::Running => "running".to_string(),
            AgentThread::Failed(why) => format!("did not start: {why}"),
        }
    }
}

/// The task that performs the queued store writes, as the platform reports it.
///
/// It exists because the write is a flash operation with both caches disabled, which
/// makes *which task does it* a correctness question rather than a detail: a task whose
/// stack is in PSRAM cannot do it at all (the board asserts and reboots). The platform
/// therefore names its writer and says whether that task's stack turned out to be in
/// internal RAM; `/api/status.store` shows both, so "the queue is being drained by the
/// right task" is something a person can check instead of assume.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreWriter {
    /// The task's name, empty until a platform attaches one.
    pub name: String,
    /// Whether the writer's stack is in internal RAM. `None` until it has said so;
    /// `Some(false)` is a bug that must not be silent.
    pub internal_stack: Option<bool>,
}

impl StoreWriter {
    pub fn as_json(&self) -> Value {
        json!({
            "task": self.name,
            "internal_stack": self.internal_stack,
        })
    }
}

impl App {
    pub fn new(hw: Box<dyn Hardware>, store: Box<dyn KvStore>, log: Arc<LogBuf>, firmware: FwMeta) -> Self {
        let now = now_ms();
        // Every write below this line is a request: `store` is the queue, and the platform's
        // writer task is what turns a request into an `spi_flash` call. Reads pass straight
        // through (after checking the queue), which is why boot is unchanged.
        let queue = store::StoreQueue::new(store);
        let store: Box<dyn KvStore> = Box::new(store::QueuedStore::new(queue.clone()));
        let cfg = store
            .get(crate::config::NVS_KEY_CONFIG)
            .and_then(|s| Config::from_json_str(&s))
            .unwrap_or_default();
        let (mut manifest, manifest_source) = match store.get(NVS_KEY_MANIFEST) {
            Some(s) => match Manifest::from_json_str(&s) {
                Ok(m) => (m, "nvs"),
                Err(e) => {
                    log.error("manifest", format!("stored manifest is unreadable ({e}); using an empty one"));
                    (Manifest::default(), "broken")
                }
            },
            None => (Manifest::default(), "default"),
        };
        // A manifest stored by an older build may name a tool the wire will not carry.
        // It has to go before the first model call, because that is the call a repair
        // would have to travel on (see `Manifest::drop_tools_with_unusable_names`).
        let dropped_names = manifest.drop_tools_with_unusable_names();
        if !dropped_names.is_empty() {
            log.warn(
                "manifest",
                format!(
                    "dropped {} stored tool(s) whose names no model endpoint accepts: {} — \
                     re-add them with `_` instead of `.`",
                    dropped_names.len(),
                    dropped_names.join(", ")
                ),
            );
        }
        let mut runtime = Runtime::new(now, &firmware.version, &firmware.build);
        runtime.manifest_rev = 1;
        // Write the cleaned manifest back, so dropping happens once and not every boot.
        runtime.manifest_dirty = !dropped_names.is_empty();
        let mut app = Self {
            cfg,
            manifest,
            devices: Devices::new(),
            tools: ToolRegistry::new(),
            family_members: std::collections::BTreeMap::new(),
            hw,
            store,
            log: log.clone(),
            runtime,
            caps: CapSet::new(),
            probe: ProbeReport::default(),
            profile: boards::find("generic").expect("generic profile"),
            profile_exact: true,
            firmware,
            lan_url: String::new(),
            attachments: Vec::new(),
            tls: TlsInfo::default(),
            agent: Agent::new(),
            llm: LlmConfig::default(),
            tasks: Vec::new(),
            rules: Vec::new(),
            scripts: Vec::new(),
            tool_specs_cache: std::cell::RefCell::new(None),
            script_seq: 0,
            workspace: AppWorkspace::default(),
            shell_env: std::collections::BTreeMap::new(),
            tool_transport: None,
            model_transport: None,
            streams: EventStreamGate::new(),
            shared: Weak::new(),
            session_counter: 0,
            agent_thread: AgentThread::NotStarted,
            store_queue: queue,
            store_writer: StoreWriter::default(),
        };
        app.log.info(
            "boot",
            format!(
                "board={} tools_mode={} manifest={manifest_source}",
                app.cfg.board, app.cfg.mcp.tools_mode
            ),
        );
        app.rebuild();
        app.load_agent_state();
        // A freshly flashed board has an empty workspace; a factory-reset board is back on
        // the default config. Both belong to the same moment: the three skills the
        // firmware carries are installed here, and in `seed_default_skills`'s other caller.
        app.seed_default_skills();
        app
    }

    /// Hand an `App` the handle it lives in, so the event stream can poll its own
    /// journal after the request that opened it has returned.
    ///
    /// Called by [`make_handler`], which is the composition root - the one place
    /// where both halves exist. Host tests call it directly. Stored as `Weak`: the
    /// `App` must not keep its own `Arc` alive.
    pub fn attach_shared(app: &Shared) {
        app_lock(app).shared = Arc::downgrade(app);
    }

    /// How many event streams hold a pool slot right now.
    pub fn active_event_streams(&self) -> usize {
        self.streams.active()
    }

    /// The platform's agent loop is up. Logged here, so the log line and
    /// `/api/status.agent.thread` cannot disagree.
    pub fn agent_thread_running(&mut self) {
        self.agent_thread = AgentThread::Running;
        self.log.info("agent", "agent thread up");
    }

    /// The platform could not start its agent loop.
    ///
    /// Logged here for the same reason: the boot says `did not start: <why>` and
    /// `/api/status` says exactly that string, so a board whose own agent is dead cannot
    /// pass for a quiet one.
    pub fn agent_thread_failed(&mut self, why: impl Into<String>) {
        let why = why.into();
        self.log.error("agent", format!("agent thread did not start: {why}"));
        self.agent_thread = AgentThread::Failed(why);
    }

    /// What `/api/status` reports as `agent.thread`, for tests and for the portal.
    pub fn agent_thread(&self) -> &AgentThread {
        &self.agent_thread
    }

    /// The queue handle a platform's writer task drains.
    ///
    /// Handing it out is the whole mechanism: the writer keeps this and calls
    /// [`App::flush_store`] under the app lock, and nothing else in the firmware touches
    /// NVS after boot.
    pub fn store_queue(&self) -> store::StoreQueue {
        self.store_queue.clone()
    }

    /// Report the task that performs the queued writes, and whether its stack turned out
    /// to be in internal RAM.
    pub fn attach_store_writer(&mut self, name: impl Into<String>, internal_stack: bool) {
        let name = name.into();
        if internal_stack {
            self.log.info(
                "store",
                format!(
                    "store writer is `{name}`, stack in internal RAM: it is the only task that writes NVS"
                ),
            );
        } else {
            self.log.error(
                "store",
                format!(
                    "store writer `{name}` is on a stack outside internal RAM; it must not write NVS (spi_flash runs with the caches off), so the queue is left queued"
                ),
            );
        }
        self.store_writer = StoreWriter { name, internal_stack: Some(internal_stack) };
    }

    /// What `/api/status` reports as `store`.
    pub fn store_writer(&self) -> &StoreWriter {
        &self.store_writer
    }

    /// Perform every write this `App` has queued.
    ///
    /// **Only a task whose stack is in internal RAM may call this.** An NVS write is an
    /// `spi_flash` operation with both caches disabled, so a task whose stack lives in
    /// PSRAM cannot survive one: the board asserts in
    /// `spi_flash_disable_interrupts_caches_and_other_cpu (esp_task_stack_is_sane_cache_disabled())`
    /// and reboots (measured 2026-11-12, a due scheduled task, ~8 s per reboot). The
    /// firmware checks the calling task's stack before every call
    /// (`firmware/src/nvs_writer.rs`); the host tests call this directly, where there is
    /// no such thing as a flash window.
    pub fn flush_store(&self, who: &str) -> store::FlushReport {
        let report = self.store_queue.flush();
        // One line per refusal, not one line for all of them: a count says something is
        // wrong, and only the key says *what* is. `report.failed` is still the count, and
        // `/api/status.store.last_failure` carries the newest one off the wire.
        for refusal in &report.failures {
            self.log.error(
                "store",
                format!(
                    "`{who}`: `{}` was refused on `{}` at {} ms: {} ({}); the change is only in RAM",
                    refusal.key,
                    refusal.op.as_str(),
                    refusal.at_ms,
                    refusal.name,
                    refusal.code
                ),
            );
        }
        if report.written > 0 {
            self.log.info(
                "store",
                format!("wrote {} store value(s) from `{who}`: {}", report.written, report.keys.join(", ")),
            );
        }
        report
    }

    /// Re-run board resolution, manifest device instantiation, detection and the
    /// tool registry. Called at boot, after a re-probe and after a manifest edit.
    pub fn rebuild(&mut self) {
        let info = self.hw.board_info();
        let (profile, exact) = boards::resolve(&self.cfg.board, &info);
        self.profile = profile;
        self.profile_exact = exact;
        if !exact {
            self.log.warn("boot", format!("unknown board `{}`, using {}", self.cfg.board, profile.id));
        }
        if self.apply_profile_defaults() {
            self.runtime.config_dirty = true;
        }

        let started = std::time::Instant::now();
        let res = drivers::detect(&mut *self.hw, &self.cfg, profile, &self.log);
        self.devices = res.devices;
        self.probe = ProbeReport {
            buses: res.buses,
            unknown: res.unknown,
            candidates: res.candidates,
            at_ms: now_ms(),
            duration_ms: started.elapsed().as_millis() as u64,
        };

        // Manifest-declared devices that a scan would not have found (or that the
        // model pinned to explicit addresses).
        let mut declared_errors = Vec::new();
        let specs: Vec<crate::manifest::DeviceSpec> =
            self.manifest.devices.iter().filter(|d| d.enabled || d.is_enabled()).cloned().collect();
        for spec in &specs {
            if spec.driver.is_none() {
                continue;
            }
            // The manifest may declare what the probe has already adopted — that is the
            // normal case once `discovery` is `auto` and someone leaves an earlier
            // declaration in place. Declaring it again must not produce a second device:
            // on the bench that showed up as eight devices, two of everything, with the
            // copies renamed `led_set_2`. Same kind *and* same identity is the same
            // device; a genuinely second sensor at another address still gets added.
            let driver = spec.driver.as_deref().unwrap_or("");
            // Compare pins as sets: a declaration written from a candidate template says
            // `{out: 48, p0: 48}` where the probe reports `[48]`, and a raw list compare
            // then declares a second LED on the same pin. A subset is enough — declaring
            // part of what is already live cannot mean "another device".
            let pins: std::collections::BTreeSet<i32> = spec.pins.values().copied().collect();
            let already = self.devices.infos().iter().any(|i| {
                if i.kind != spec.kind && i.kind != driver {
                    return false;
                }
                match spec.address {
                    Some(a) => i.address == Some(a),
                    None if pins.is_empty() => true,
                    None => pins.iter().all(|p| i.pins.contains(p)),
                }
            });
            if already {
                self.log.info(
                    "manifest",
                    format!("`{}` is already live from the probe; the declaration is a no-op", spec.id),
                );
                continue;
            }
            match drivers::instantiate(spec, &mut *self.hw) {
                Ok(dev) => {
                    self.log.info("manifest", format!("instantiated `{}` from the manifest", spec.id));
                    // The manifest's id is the one `device_invoke` and `devices_list`
                    // use; the driver's own default (`led`, `relay`) would make two
                    // devices of one kind indistinguishable.
                    self.devices.push(Box::new(crate::device::NamedDevice::new(dev, &spec.id)));
                }
                Err(e) => {
                    self.log.warn("manifest", format!("device `{}` failed: {}", spec.id, e.message));
                    declared_errors.push(json!({ "device": spec.id, "error": e.message }));
                }
            }
        }

        // Mark which candidates are already live, so a script that adopts them does not
        // have to diff two lists itself — and so a second run is visibly a no-op.
        if !self.probe.candidates.is_empty() {
            let live: Vec<String> = self.devices.infos().iter().map(|d| d.id.clone()).collect();
            for candidate in self.probe.candidates.iter_mut() {
                let id = candidate.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                candidate["adopted"] = json!(live.contains(&id));
            }
        }

        let mut caps = CapSet::from_caps(drivers::board_caps(&mut *self.hw, &self.cfg, profile));
        caps.extend(self.devices.caps());
        self.caps = caps;

        let mut tools = ToolRegistry::new();
        tools.register_all(board_tools::always_tools());
        tools.register_all(board_tools::manifest_tools());
        tools.register(board_tools::board_family());
        tools.register(board_tools::bus_family());
        // A family is one tool with several commands; the members are still registered under
        // their old names, so a stored script keeps calling the same handler. Remembering
        // which is which is what lets the listing say "reachable as `board status`" instead
        // of dropping nine names without a word.
        let mut members: std::collections::BTreeMap<String, String> = Default::default();
        for (family, names) in [
            ("manifest", board_tools::manifest_members()),
            ("board", board_tools::board_members()),
            ("bus", board_tools::bus_members()),
        ] {
            let prefix = format!("{family}_");
            for name in names {
                let command = name.strip_prefix(&prefix).unwrap_or(&name).to_string();
                members.insert(name, format!("{family} {command}"));
            }
        }
        self.family_members = members;
        tools.register_all(self.devices.tool_defs());
        // Device tools that belong together answer to one family name: six `oled_*` become
        // `display`, four audio tools become `audio`, and the LED and the buttons become
        // `indicators`. The grouping is the board's own vocabulary — a kind of device it
        // detected — so a driver that is not in this list keeps its individual tools
        // instead of being guessed at by name.
        let mut groups: std::collections::BTreeMap<&'static str, Vec<String>> = Default::default();
        for info in self.devices.infos() {
            let Some((_, family)) = Self::DEVICE_FAMILIES.iter().find(|(kind, _)| *kind == info.kind) else {
                continue;
            };
            groups.entry(family).or_default().extend(info.tools.iter().cloned());
        }
        for (family, declared) in groups {
            // A second pass, because a family owns its commands while the old names stay in
            // the registry: `oled_text` still answers, and `display oled_text` is the same
            // handler behind one declaration.
            let subs: Vec<ToolDef> = self
                .devices
                .tool_defs()
                .into_iter()
                .filter(|d| {
                    declared.iter().any(|n| d.spec.name == *n || d.spec.name.starts_with(&format!("{n}_")))
                })
                .collect();
            if subs.len() < 2 {
                // One tool is not a family: the declaration would cost more than the tool.
                continue;
            }
            let summary = format!("{family}: {} commands", subs.len());
            for name in subs.iter().map(|d| d.spec.name.clone()).collect::<Vec<String>>() {
                self.family_members.insert(name.clone(), format!("{family} {name}"));
            }
            tools.register(Family::new(family, summary, subs).into_tool().device_tool());
        }
        // A manifest may not take a name the board already uses (validation refuses it),
        // but a manifest stored by an older build — or one applied before the hardware
        // was plugged in — can still arrive here. The board's own tool wins, and the
        // clash is logged: silently replacing `manifest_apply` would leave a board that
        // cannot repair its own manifest.
        let mut clashing: Vec<String> = Vec::new();
        for def in self.manifest.tool_defs() {
            let name = def.spec.name.clone();
            if tools.get(&name).is_some() {
                clashing.push(name);
                continue;
            }
            tools.register(def);
        }
        if !clashing.is_empty() {
            self.log.error(
                "manifest",
                format!(
                    "ignored {} manifest tool(s) that reuse a tool the board provides: {} — rename them",
                    clashing.len(),
                    clashing.join(", ")
                ),
            );
        }
        self.tools = tools;
        self.runtime.manifest_rev = self.runtime.manifest_rev.max(1);

        if !declared_errors.is_empty() {
            self.log.warn("manifest", format!("{} declared device(s) unavailable", declared_errors.len()));
        }
        self.log.info(
            "boot",
            format!(
                "detected {} device(s) in {} ms; {} tool(s), {} visible",
                self.devices.len(),
                self.probe.duration_ms,
                self.tools.len(),
                self.visible_tool_names().len()
            ),
        );
    }

    /// Fill bus/pin defaults from the board profile, never overwriting the user's.
    fn apply_profile_defaults(&mut self) -> bool {
        let mut changed = false;
        let default_sda = crate::config::I2cBusCfg::default().sda;
        let default_scl = crate::config::I2cBusCfg::default().scl;
        let bus_is_default = self.cfg.bus.i2c.len() == 1
            && self.cfg.bus.i2c[0].sda == default_sda
            && self.cfg.bus.i2c[0].scl == default_scl;
        if bus_is_default && !self.profile.i2c.is_empty() {
            let buses: Vec<crate::config::I2cBusCfg> = self
                .profile
                .i2c
                .iter()
                .enumerate()
                .map(|(i, h)| crate::config::I2cBusCfg {
                    bus: i as u8,
                    sda: h.sda,
                    scl: h.scl,
                    freq_hz: h.freq_hz,
                })
                .collect();
            if buses != self.cfg.bus.i2c {
                self.log.info(
                    "boot",
                    format!(
                        "using profile I2C wiring SDA{} SCL{}",
                        self.profile.i2c[0].sda, self.profile.i2c[0].scl
                    ),
                );
                self.cfg.bus.i2c = buses;
                changed = true;
            }
        }
        changed
    }

    // ---------------------------------------------------------------- queries

    /// Tool names that still answer but must not be advertised anywhere a model reads.
    ///
    /// They were the interface once. `board_status` and its siblings became commands of the
    /// `board` family, `i2c_*`/`gpio_*`/`adc_read` of `bus`, the manifest five of `manifest`,
    /// the device tools of `display`/`audio`/`indicators`, and `fs_read`/`net_get` were
    /// folded into `read` and `shell`. A stored script, an old transcript and an owner's
    /// habit all still use the old names, so they keep working — the prompt, `help` and
    /// `tools/list` do not mention them. This list is the compatibility column, and the test
    /// below fails if one of them turns up backticked in the prompt.
    /// Tools a person uses and a model does not.
    ///
    /// `reboot` restarts the board in the middle of a turn: the run that asked for it is the
    /// one it destroys, and a restart is one click in the portal (`POST /api/reboot`, behind
    /// the PIN) or one line from a person. So it is not offered — the prompt's "you cannot
    /// reboot the board" is the same statement in words.
    ///
    /// `factory_reset` stays offered: it needs `confirm`, it is the only way a model can get
    /// out of a manifest that has bricked the tool surface, and it is a reset of configuration
    /// rather than of the run in progress.
    const NEVER_OFFERED: &'static [&'static str] = &["reboot"];

    pub const ALIASES_ACCEPTED_BUT_NOT_ADVERTISED: &'static [&'static str] = &[
        // the board family's members
        "board_status",
        "board_config_get",
        "config_set",
        "chip_temp",
        "logs_tail",
        "reboot",
        "factory_reset",
        "delay",
        "inventory",
        // the bus family's
        "i2c_scan",
        "i2c_read",
        "i2c_write",
        "i2c_identify",
        "gpio_mode",
        "gpio_read",
        "gpio_write",
        "adc_read",
        // the manifest family's
        "manifest_get",
        "manifest_describe",
        "manifest_validate",
        "manifest_apply",
        "manifest_reset",
        // the device families'
        "oled_text",
        "oled_read",
        "oled_clear",
        "oled_rect",
        "oled_bitmap",
        "oled_contrast",
        "led_set",
        "led_blink",
        "buttons_read",
        "button_wait",
        "mic_level",
        "mic_record",
        "speaker_tone",
        "speaker_play",
        // what `read` and `shell` absorbed
        "fs_read",
        "fs_write",
        "fs_edit",
        "fs_ls",
        "fs_grep",
        "fs_rm",
        "fs_cp",
        "fs_mv",
        "net_get",
        "net_post",
        "net_dns",
        "net_tcp",
    ];

    /// Device tools that belong together, by the kind of device the board detected.
    ///
    /// A driver names its own kind, so this is the board's vocabulary rather than a list of
    /// tool names: every tool of every `ssd1306` on the board lands in `display`, and a
    /// driver that is not here keeps its individual tools. Adding a driver to a family is
    /// one line; guessing from a name prefix is what this avoids.
    const DEVICE_FAMILIES: &'static [(&'static str, &'static str)] = &[
        ("ssd1306", "display"),
        ("led", "indicators"),
        ("buttons", "indicators"),
        ("mic", "audio"),
        ("speaker", "audio"),
    ];

    /// The tools `mcp.profile = "minimal"` hides: conveniences, never capabilities.
    ///
    /// Each one has an HTTP route or a manifest equivalent, and a model that has only these
    /// removed loses no ability — it loses a shortcut.
    const MINIMAL_HIDDEN: &'static [&'static str] = &[
        "wifi_scan",
        "wifi_status",
        // `board_config_get`, `delay`, `logs_tail`, `reboot` and `factory_reset` used to sit
        // here. They belong to a family now, and a profile hides whole tools: a command is
        // a few bytes of one declaration, so there is nothing left to save by hiding it —
        // and a family that quietly drops commands costs a model a call to find out.
    ];

    /// May this tool appear in `tools/list` right now?
    pub fn is_visible(&self, name: &str) -> bool {
        self.why_hidden(name).is_none() && self.tools.get(name).is_some()
    }

    /// Why the board does not offer `name`, or `None` when it does.
    ///
    /// One function answers both questions on purpose. A list built from one rule and a
    /// reason built from another drift apart, and then the board reports "no camera" about
    /// a tool it is happily listing — which is exactly the class of bug the tool lists had
    /// before they were all built from this predicate.
    pub fn why_hidden(&self, name: &str) -> Option<String> {
        let def = self.tools.get(name)?;
        if self.cfg.mcp.disabled_tools.iter().any(|d| d == name) {
            return Some("switched off in `mcp.disabled_tools`".to_string());
        }
        // A family took this name over. The handler still answers, so the sentence says
        // where to find it rather than pretending the ability is gone.
        if let Some(reached) = self.family_members.get(name) {
            return Some(format!("reachable as `{reached}`"));
        }
        if Self::NEVER_OFFERED.contains(&name) {
            return Some("a person restarts the board, from the portal; a model does not".to_string());
        }
        // `minimal` hides conveniences, never capabilities: an HTTP route or a manifest can
        // do these, and a model rarely needs them. Discovery, manifest, GPIO and the file
        // tools stay in every profile — without them an empty board cannot rebuild itself.
        if self.cfg.mcp.profile == "minimal" && Self::MINIMAL_HIDDEN.contains(&name) {
            return Some("hidden by the `minimal` profile".to_string());
        }
        if self.cfg.mcp.tools_mode == "generic" && def.spec.device_tool {
            return Some("collapsed into `device_invoke` by `tools_mode: generic`".to_string());
        }
        if def.spec.core || def.spec.requires.is_empty() || self.cfg.mcp.tools_mode == "all" {
            return None;
        }
        // What is missing is named: "no camera on this board" is a sentence a person can
        // act on, `false` is not, and an unexplained disappearance is worse than a tool
        // that explains itself.
        let missing = self.caps.missing(&def.spec.requires);
        if missing.is_empty() {
            return None;
        }
        Some(format!("no {} on this board", missing.join(" or no ")))
    }

    /// Why one of the tools that live outside the registry is not offered.
    ///
    /// Same shape as [`why_hidden`](Self::why_hidden): `None` means "offered".
    fn extra_why_hidden(&self, name: &str) -> Option<String> {
        if self.cfg.mcp.disabled_tools.iter().any(|d| d == name) {
            return Some("switched off in `mcp.disabled_tools`".to_string());
        }
        match name {
            // A camera is a driver and a DVP bus. Without them `image_read` can only answer
            // "a camera is not available on this board", so it is not a tool the model can
            // use - it is a tool that teaches the model to waste a turn.
            "image_read" if !self.caps.contains("camera") => Some("no camera on this board".to_string()),
            _ => None,
        }
    }

    /// Every out-of-registry tool, offered or not, in the OpenAI shape the model gets.
    ///
    /// One construction feeds three readers — the offered list, the hidden list and the
    /// portal page — so none of them can invent a tool the others do not have. The groups
    /// are `shell`, `script` and the four file/image tools that need the app's own state.
    fn extra_entries() -> Vec<Value> {
        let mut out = Vec::new();
        for group in [
            // One `shell` instead of the twelve fs_*/net_* tools: same handlers, one
            // schema, one name to remember.
            json!([crate::tools::shell::openai_spec()]),
            board_tools::script_specs(),
            board_tools::agent_extra_specs(),
        ] {
            out.extend(openai_tools(&group));
        }
        out
    }

    /// The out-of-registry tools that are offered right now, in declaration order.
    ///
    /// Every view that shows tools goes through here or through `is_visible`, so the model,
    /// `/mcp tools/list` and the portal cannot disagree about what exists.
    fn extra_specs(&self) -> Vec<Value> {
        Self::extra_entries()
            .into_iter()
            .filter(|entry| self.extra_why_hidden(entry["function"]["name"].as_str().unwrap_or("")).is_none())
            .collect()
    }

    pub fn visible_tool_names(&self) -> Vec<String> {
        self.tools.iter().filter(|d| self.is_visible(&d.spec.name)).map(|d| d.spec.name.clone()).collect()
    }

    /// How many tools the model is offered, without building the list.
    ///
    /// `/api/status` needs this number on every poll, and `tool_specs()` answers with a deep
    /// clone of the whole array — an 18 KB `Value`, which is a fresh allocation for every
    /// string, object and array in it, in PSRAM, for a `filter().count()`. The predicate is
    /// the same one `tool_specs_uncached` uses, so the two cannot disagree; the test that
    /// compares `/api/status` with `ToolHost::tool_specs` would fail if they did.
    fn visible_tool_count(&self) -> usize {
        let registry = self.tools.iter().filter(|d| self.is_visible(&d.spec.name)).count();
        let extras = Self::extra_entries()
            .into_iter()
            .filter(|entry| self.extra_why_hidden(entry["function"]["name"].as_str().unwrap_or("")).is_none())
            .count();
        registry + extras
    }

    /// Every tool that exists but is not offered, with the reason — registry entries first,
    /// then the out-of-registry ones, each in name order.
    ///
    /// "The board has 47 tools" is only half a fact; this is the other half.
    pub fn hidden_tools(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .tools
            .iter()
            .filter_map(|d| {
                self.why_hidden(&d.spec.name).map(|why| json!({ "name": d.spec.name, "why": why }))
            })
            .collect();
        for entry in Self::extra_entries() {
            let name = entry["function"]["name"].as_str().unwrap_or("");
            if let Some(why) = self.extra_why_hidden(name) {
                out.push(json!({ "name": name, "why": why }));
            }
        }
        out.sort_by_key(|t| t["name"].as_str().unwrap_or("").to_string());
        out
    }

    fn tools_list_json(&self) -> Value {
        let mut tools: Vec<Value> =
            self.tools.iter().filter(|d| self.is_visible(&d.spec.name)).map(|d| d.mcp_json()).collect();
        // `fs_*`, `net_*` and `script` are not registry entries (their handlers need the
        // workspace and an HTTP client), but an MCP client can reach them, so they belong
        // in the same list — and in the same order: one alphabetical run, matching what
        // the model gets. Two views of the same set should not disagree about its shape.
        tools.extend(self.extra_mcp_specs());
        tools.sort_by_key(|t| t["name"].as_str().unwrap_or("").to_string());
        json!({ "tools": tools })
    }

    /// The MCP view of the tools that live outside the registry.
    ///
    /// Built from [`extra_specs`](Self::extra_specs), so the same enable/disable rule that
    /// decides the model's list decides this one; only the field names differ, because
    /// MCP says `inputSchema` where OpenAI says `parameters`.
    fn extra_mcp_specs(&self) -> Vec<Value> {
        self.extra_specs()
            .iter()
            .map(|tool| {
                json!({
                    "name": tool["function"]["name"],
                    "description": tool["function"]["description"],
                    "inputSchema": tool["function"]["parameters"],
                })
            })
            .collect()
    }

    /// The discovery document: the static map from `sitemap`, plus what only this board
    /// knows — its address, its firmware, how many tools it actually exposes right now.
    fn sitemap_json(&self, req: &Request) -> Value {
        let host = req
            .header("host")
            .map(|h| h.to_string())
            .unwrap_or_else(|| format!("<board>:{}", self.cfg.mcp.port));
        let tool_count = self.tool_specs().as_array().map(|a| a.len()).unwrap_or(0);
        crate::sitemap::document(json!({
            "base_url": format!("{}://{host}", req.scheme),
            "firmware": { "name": "esp32-agent", "version": crate::VERSION, "build": self.firmware.build, "commit": self.firmware.commit, "built_at": self.firmware.built_at },
            "board": { "profile": self.cfg.board, "devices": self.devices.len() },
            "mcp": crate::sitemap::mcp_json(
                &self.cfg.mcp.path,
                tool_count,
                !self.cfg.auth.mcp_token.is_empty(),
            ),
            "security": {
                "portal_pin_set": !self.cfg.auth.portal_pin.is_empty(),
                "mcp_token_set": !self.cfg.auth.mcp_token.is_empty(),
                "note": "with neither set, every route above is open on the local network",
            },
            "tls": self.tls.json(),
            "model": {
                "configured": !self.llm.base_url.is_empty() && !self.llm.model.is_empty(),
                "endpoint": self.llm.base_url,
                "name": self.llm.model,
                "how_to_set": "POST /api/agent/config {base_url, model, api_key?}",
            },
        }))
    }

    fn tools_portal_json(&self) -> Value {
        let mut tools: Vec<Value> = self
            .tools
            .iter()
            .map(|d| {
                // One predicate decides both the flag and the sentence, so the page cannot
                // say "visible" about something the model does not have.
                let why = self.why_hidden(&d.spec.name);
                let mut v = d.portal_json(why.is_none());
                if let Some(why) = why {
                    if !d.spec.requires.is_empty() {
                        v["missing"] = json!(self.caps.missing(&d.spec.requires));
                    }
                    v["why"] = json!(why);
                }
                v
            })
            .collect();
        // The agent's own tools are not in the registry (they need the workspace and the
        // HTTP client), so they never appeared here — which is why the portal could show
        // 40 tools while the model had 49 and `script` looked missing.
        for entry in Self::extra_entries() {
            let f = &entry["function"];
            let name = f["name"].as_str().unwrap_or("");
            let why = self.extra_why_hidden(name);
            // The read-only ones, by name: the specs are OpenAI-shaped and carry no
            // `mutating` flag, and a made-up one would be worse than this list.
            let read_only = matches!(name, "read" | "image_read");
            let mut v = json!({
                "name": name,
                "title": name,
                "description": f["description"],
                "category": "agent",
                "requires": [],
                "mutating": !read_only,
                "core": false,
                "visible": why.is_none(),
                "mcp": why.is_none(),
                "input_schema": f["parameters"],
            });
            if let Some(why) = why {
                v["why"] = json!(why);
            }
            tools.push(v);
        }
        // Alphabetical, one list: the order the model gets, and the order a reader expects.
        tools.sort_by_key(|t| t["name"].as_str().unwrap_or("").to_string());
        let visible = tools.iter().filter(|t| t["visible"] == json!(true)).count();
        json!({
            "tools": tools,
            "visible": visible,
            "total": tools.len(),
            "mode": self.cfg.mcp.tools_mode,
            "style_hint": if self.cfg.mcp.tools_mode == "generic" {
                "generic mode: device tools are collapsed into device_invoke to keep the prompt short"
            } else {
                "auto mode: one tool per action of every detected device"
            },
        })
    }

    pub fn status_json(&mut self) -> Value {
        let info = self.hw.board_info();
        let wifi = self.hw.wifi_status();
        let internal = self.hw.heap_internal();
        let threads = self.threads_json();
        let now = now_ms();
        // Built before the object literal: `json!` reads a brace after a `:` as a nested
        // object, so a block expression belongs in a local.
        let store_last_failure = match self.store_queue.last_failure() {
            Some(refusal) => json!({
                "key": refusal.key,
                "op": refusal.op.as_str(),
                "err": refusal.code,
                "err_name": refusal.name,
                "at_ms": refusal.at_ms,
            }),
            None => Value::Null,
        };
        let mut refused = serde_json::Map::new();
        for (key, count) in self.store_queue.refused_keys() {
            refused.insert(key, json!(count));
        }
        let store_space = match self.store_queue.space() {
            Some(space) => space.to_json(),
            None => Value::Null,
        };
        json!({
            "device_name": self.cfg.device_name,
            // The board's clock, because it decides whether https works at all: before
            // SNTP syncs, every certificate reads as "not yet valid".
            "time": {
                "unix_s": now / 1000,
                "synced": now > 1_700_000_000_000,
                "note": "before SNTP syncs, https handshakes fail and http does not",
            },
            "firmware": {
                "name": self.firmware.name,
                "version": self.firmware.version,
                "build": self.firmware.build,
                "commit": self.firmware.commit,
                "built_at": self.firmware.built_at,
            },
            "board": {
                "profile": self.profile.id,
                "profile_name": self.profile.name,
                "profile_exact": self.profile_exact,
                "soc": info.soc,
                "cores": info.cores,
                "flash_mb": info.flash_mb,
                "psram_kb": info.psram_kb,
                "mac": info.mac,
                "idf_version": info.idf_version,
                "temperature_c": info.temp_c,
            },
            "memory": {
                "free_heap_kb": info.free_heap_kb,
                "min_free_heap_kb": info.min_free_heap_kb,
                // Internal SRAM is what thread stacks, DMA and the TLS handshake come from;
                // the total above includes PSRAM and can look healthy while this is empty.
                "internal_kb": internal.map(|(free, _)| free / 1024),
                "largest_internal_kb": internal.map(|(_, largest)| largest / 1024),
            },
            // The other half of the memory picture: which pool, and how much stack each
            // task has left at its deepest point. Without this, deciding whether a stack
            // constant can come down is guesswork, and a stack that is too small shows up
            // as an assert and a reboot rather than as a number.
            "threads": threads,
            "uptime_s": self.runtime.uptime_s(now),
            "wifi": wifi,
            "setup": {
                "needed": self.setup_needed(),
                "portal_url": self.portal_url(),
            },
            "mcp": {
                "enabled": self.cfg.mcp.enabled,
                "port": self.cfg.mcp.port,
                "path": self.cfg.mcp.path,
                "url": self.mcp_url(),
                "tools_mode": self.cfg.mcp.tools_mode,
                // What the model actually receives, not the registry count: the out-of-
                // registry tools (`shell`, `script`, `read`/`write`/`edit`) are tools the
                // model can call, and leaving them out made this number disagree with
                // `/api/tools` by six. Counted rather than cloned: see `visible_tool_count`.
                "tools_visible": self.visible_tool_count(),
                "tools_hidden": self.hidden_tools(),
                "sessions": self.runtime.sessions.len(),
                "auth_required": !self.cfg.auth.mcp_token.is_empty(),
                "manifest_revision": self.runtime.manifest_rev,
                "manifest_tools": self.manifest.tool_names().len(),
            },
            "devices": {
                "count": self.devices.len(),
                "caps": self.caps.to_json(),
                "unknown_addresses": self.probe.unknown.len(),
                "last_probe_ms": self.probe.duration_ms,
            },
            "counters": self.runtime.counters_json(),
            "agent": {
                "runs": self.agent.runs,
                "history": self.agent.history.len(),
                "pending": self.agent.pending.is_some(),
                "last_error": self.agent.last_error,
                "configured": !self.llm.base_url.is_empty() && !self.llm.model.is_empty(),
                "model": self.llm.model,
                // The one field that says whether the board's own loop exists. A dead
                // thread and a quiet board are otherwise indistinguishable from here.
                "thread": self.agent_thread.as_str(),
            },
            "sched": {
                "tasks": self.tasks.len(),
                "rules": self.rules.len(),
                "enabled_tasks": self.tasks.iter().filter(|t| t.enabled).count(),
                "enabled_rules": self.rules.iter().filter(|r| r.enabled).count(),
            },
            // What is queued and who drains it. A growing `pending` means the writer task
            // is not running; `internal_stack: false` means it must not write at all.
            //
            // `last_failure` and `refused_keys` are the diagnosis a `failed` count cannot
            // give: a caller was told a change was accepted and it is not on flash, and
            // these name the key and the store's own reason (an `esp_err_t` number and
            // its name) so a board with no serial terminal attached still says why.
            "store": {
                "pending": self.store_queue.pending(),
                "written": self.store_queue.written(),
                "failed": self.store_queue.failed(),
                "writer": self.store_writer.as_json(),
                "last_failure": store_last_failure,
                "refused_keys": refused,
                "refused_other": self.store_queue.refused_other(),
                "space": store_space,
            },
            "pins": self.cfg.pins,
        })
    }

    /// The tasks the platform can enumerate, each with the stack it has left at its
    /// deepest point — the field a stack constant has to be justified against.
    ///
    /// FreeRTOS' "high-water mark" is the *lowest free* figure a task ever reported, so
    /// `high_water_kb` is headroom, not use: the peak use is the size in the source minus
    /// this. Both readings are needed to apply a rule like "≥ max(2 × peak, peak + 4 kB)",
    /// which is why the size stays in the source instead of being guessed from here.
    fn threads_json(&mut self) -> Vec<Value> {
        self.hw
            .tasks()
            .into_iter()
            .map(|t| {
                json!({
                    "name": t["name"],
                    "state": t["state"],
                    "priority": t["priority"],
                    // One decimal: a 4 KB stack's answer has to be readable as 2.1, not 2.
                    "high_water_kb": t["stack_free_bytes"]
                        .as_u64()
                        .map(|bytes| ((bytes as f64 / 1024.0) * 10.0).round() / 10.0),
                })
            })
            .collect()
    }

    /// True while the board is waiting to be told which WiFi to join.
    pub fn setup_needed(&self) -> bool {
        // Setup mode means "no credentials stored", which is what the portal and the MCP
        // instructions claim. The access point being up is a different fact: with
        // `wifi.ap_fallback` it keeps running after the board joins a network, and the
        // station is briefly down after every reconnect. Keying the answer to either of
        // those told a joined board to announce "waiting for credentials", while a board
        // that had never been provisioned could report the opposite.
        self.cfg.wifi.ssid.trim().is_empty()
    }

    /// The address a phone should open. The setup AP always answers on the
    /// ESP-IDF default 192.168.4.1; a board with a LAN address serves both.
    pub fn portal_url(&mut self) -> String {
        let wifi = self.hw.wifi_status();
        if !wifi.ip.is_empty() && wifi.ip != "0.0.0.0" {
            return format!("http://{}:{}/", wifi.ip, self.cfg.mcp.port);
        }
        if !wifi.ap_ip.is_empty() {
            return format!("http://{}:{}/", wifi.ap_ip, self.cfg.mcp.port);
        }
        format!("http://192.168.4.1:{}/", self.cfg.mcp.port)
    }

    pub fn mcp_url(&mut self) -> String {
        self.mcp_url_with("http")
    }

    /// The MCP endpoint as seen by a client that reached us over `scheme`. A board
    /// answering TLS that advertises `http://` sends the next agent down a dead path —
    /// and so does one that keeps the plaintext port in the URL.
    pub fn mcp_url_with(&mut self, scheme: &str) -> String {
        if scheme == "http" && !self.lan_url.is_empty() {
            return format!("{}{}", self.lan_url.trim_end_matches('/'), self.cfg.mcp.path);
        }
        let wifi = self.hw.wifi_status();
        let host = if !wifi.ip.is_empty() && wifi.ip != "0.0.0.0" {
            wifi.ip
        } else if !wifi.ap_ip.is_empty() {
            wifi.ap_ip
        } else {
            "<board-ip>".into()
        };
        let port = if scheme == "https" && self.tls.port != 0 { self.tls.port } else { self.cfg.mcp.port };
        format!("{scheme}://{host}:{port}{}", self.cfg.mcp.path)
    }

    /// The text shown to the model at `initialize`, generated from what is really
    /// attached so it never promises hardware that is not there.
    fn instructions(&mut self, scheme: &str) -> String {
        let mut names: Vec<String> = self.devices.infos().iter().map(|d| d.name.clone()).collect();
        if names.is_empty() {
            names.push("no peripherals detected".into());
        }
        let tools = self.visible_tool_names();
        let setup = self.setup_needed();
        let url = self.mcp_url_with(scheme);
        let soc = self.hw.board_info().soc;
        let board = self.profile.name.to_string();
        format!(
            "esp32-agent v{ver} on {board} ({soc}).\n\
             Detected hardware: {devs}.\n\
             {n} tool(s) available: {tool_list}.\n\
             Tools whose hardware is missing are not listed, so anything you can see is usable.\n\
             This board is self-configuring: use `inventory` / `i2c_identify` to find what is wired, \
             `manifest_describe` for the device+tools schema, and `manifest_apply` to add hardware and tools. \
             A manifest change takes effect immediately - no reboot and no reflash - so you can iterate: \
             apply, call the new tool, and fix the manifest if the result is wrong.\n\
             Endpoint: {url}.{setup_note}",
            ver = self.firmware.version,
            board = board,
            soc = soc,
            devs = names.join(", "),
            n = tools.len(),
            tool_list = tools.join(", "),
            url = url,
            setup_note = if setup {
                "\nThis board is in setup mode: it is broadcasting its own WiFi network and is waiting for credentials on the portal page."
            } else {
                ""
            },
        )
    }

    // ---------------------------------------------------------------- tool calls

    /// Run a board script (`script` tool).
    ///
    /// The script gets a host that calls back into this dispatcher, so `tool()` inside a
    /// script has exactly the reach of the caller: no extra capability, no bypass of
    /// `is_visible`, and `script` itself is refused to stop unbounded recursion.
    /// Submit a script for execution, returning the job.
    ///
    /// `inline` runs it now and fills the result before returning; otherwise it is queued
    /// and `run_next_script` (one per scheduler tick) finishes it. Both paths go through
    /// [`Self::run_script`], so an asynchronous run has exactly the permissions of a
    /// synchronous one and of the on-board agent: the same tool dispatch, the same
    /// `mcp.disabled_tools`, the same refusal to call `script` from inside a script.
    pub fn submit_script(&mut self, source: &str, inline: bool) -> ScriptJob {
        let id = format!("s{}", self.script_seq + 1);
        self.script_seq += 1;
        let job =
            ScriptJob { id, source: source.to_string(), state: "queued".to_string(), ..Default::default() };
        self.scripts.push(job);
        // Four is enough for a person and an external agent to each have one in flight; a
        // longer queue would only hide that the board runs one at a time anyway.
        while self.scripts.len() > 4 {
            self.scripts.remove(0);
        }
        if inline {
            let id = self.scripts.last().expect("just pushed").id.clone();
            self.run_script_job(&id);
            return self.scripts.iter().find(|j| j.id == id).cloned().unwrap_or_default();
        }
        self.scripts.last().cloned().unwrap_or_default()
    }

    /// Finish the oldest queued job, if any. Called once per tick.
    pub fn run_next_script(&mut self) {
        let next = self.scripts.iter().find(|j| j.state == "queued").map(|j| j.id.clone());
        if let Some(id) = next {
            self.run_script_job(&id);
        }
    }

    /// Look up a job by id, for `GET /api/script/<id>`.
    pub fn script_job(&self, id: &str) -> Option<ScriptJob> {
        self.scripts.iter().find(|j| j.id == id).cloned()
    }

    fn run_script_job(&mut self, id: &str) {
        let Some(index) = self.scripts.iter().position(|j| j.id == id) else {
            return;
        };
        let source = self.scripts[index].source.clone();
        self.scripts[index].state = "running".to_string();
        let started = now_ms();
        match self.run_script(&json!({ "source": source })) {
            Ok(value) => {
                let job = &mut self.scripts[index];
                job.state = "done".to_string();
                job.result = value.get("result").cloned().unwrap_or(Value::Null);
                job.output = value
                    .get("output")
                    .and_then(|o| o.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                job.steps = value.get("steps").and_then(|s| s.as_u64()).unwrap_or(0);
                job.ms = now_ms().saturating_sub(started);
                job.error.clear();
            }
            Err(e) => {
                let job = &mut self.scripts[index];
                job.state = "failed".to_string();
                job.ms = now_ms().saturating_sub(started);
                job.error = e.message;
            }
        }
    }

    fn run_script(&mut self, args: &Value) -> Result<Value> {
        // No `source` means the caller wants the reference, not a run: a model writing its
        // first script has no other way to learn the syntax, and one paragraph in the tool
        // description is not a language.
        let help_section = crate::json::opt_str(args, "section").unwrap_or_default();
        let wants_help = crate::json::opt_bool(args, "help").unwrap_or(false) || !help_section.is_empty();
        // Either inline `source` or a `path` in the workspace, the way a shell runs a file.
        let mut source = crate::json::opt_str(args, "source").unwrap_or_default();
        let path = crate::json::opt_str(args, "path").unwrap_or_default();
        if source.trim().is_empty() && !path.trim().is_empty() {
            let read = fs_tools::call(&mut self.workspace, "fs_read", &json!({ "path": path }))
                .ok_or_else(|| McpError::invalid_params(format!("no script at `{path}`")))?
                .map_err(McpError::invalid_params)?;
            // `fs_read` answers with a JSON object (`{path, text, …}`), so the script is its
            // `text` field — feeding the whole object to the parser is what made a one-line
            // script fail with "unexpected token Colon".
            source = serde_json::from_str::<Value>(&read)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(|t| t.to_string()))
                .unwrap_or(read);
        }
        if crate::json::opt_bool(args, "lint").unwrap_or(false) && !source.trim().is_empty() {
            let lint = crate::script::lint(&source);
            return Ok(json!({
                "ok": lint.ok(),
                "errors": lint.errors,
                "warnings": lint.warnings,
                "note": "lint only — nothing was run; send `source` without `lint` to execute",
            }));
        }
        if wants_help || source.trim().is_empty() {
            let section = if help_section.is_empty() { "overview".to_string() } else { help_section };
            let text = crate::script::help(&section).map_err(McpError::invalid_params)?;
            return Ok(json!({
                "section": section,
                "text": text,
                "bytes": text.len(),
                "next": "overview | builtins | examples, then send `source`",
            }));
        }
        struct Host<'a> {
            app: &'a mut App,
        }
        impl crate::script::ScriptHost for Host<'_> {
            fn call_tool(&mut self, name: &str, arguments: &Value) -> std::result::Result<Value, String> {
                if name == "script" {
                    return Err("a script may not call `script`".into());
                }
                // `App::call_tool` already returns the tool's JSON value.
                self.app.call_tool(name, arguments).map_err(|e| e.message)
            }
        }
        // Arguments for the script: `{"source": …, "args": {"pin": 12}}`. They land in the
        // global scope as `args` (a map) and `argv` (its values, in order), which is what
        // makes one script usable for several pins.
        let args = script_arguments(args.get("args"));
        let mut host = Host { app: self };
        let started = now_ms();
        match crate::script::run_with(&source, &mut host, &args) {
            Ok(outcome) => {
                self.log.info(
                    "script",
                    format!(
                        "ran in {} ms: {} step(s), {} log line(s)",
                        now_ms().saturating_sub(started),
                        outcome.steps,
                        outcome.output.len()
                    ),
                );
                Ok(json!({
                    "ok": true,
                    "result": outcome.result,
                    "output": outcome.output,
                    "steps": outcome.steps,
                }))
            }
            Err(e) => Err(McpError::invalid_params(format!("script failed: {e}"))),
        }
    }

    /// Ask the configured model about an attached image.
    ///
    /// This is a **second** model call, made inside a tool: the board has no OCR engine and
    /// no image pipeline, so the only reader it has is the model the user configured. That
    /// costs a round trip and tokens, which is why the tool description says so.
    ///
    /// It always uses the chat-completions shape: image parts are OpenAI's
    /// `image_url` blocks there, Anthropic's `image` blocks in the Messages API, and the
    /// Responses API wants input items — three shapes for one question, and this tool
    /// answers only the shape it can be sure of.
    fn image_read(&mut self, args: &Value) -> Result<Value> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("Transcribe every piece of text in this image exactly, then describe what it shows.")
            .to_string();
        let wanted = args.get("name").and_then(|v| v.as_str()).unwrap_or("");

        let attachment = match self.attachments.iter().find(|a| wanted.is_empty() || a.name == wanted) {
            Some(a) => a.clone(),
            None => {
                return Err(McpError::invalid_params(if self.attachments.is_empty() {
                    "no image attached to the last message; attach one in the portal first".to_string()
                } else {
                    format!(
                        "no attachment named `{wanted}`; the last message carried: {}",
                        self.attachments.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ")
                    )
                }))
            }
        };
        if self.llm.base_url.trim().is_empty() || self.llm.model.trim().is_empty() {
            return Err(McpError::unsupported(
                "no model configured, and this tool needs one to read an image",
            ));
        }
        let Some(transport) = self.tool_transport.as_mut() else {
            return Err(McpError::unsupported("no HTTP transport attached"));
        };

        let mut cfg = self.llm.clone();
        cfg.protocol = "chat".into();
        cfg.vision = true;
        let messages = json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": prompt },
                { "type": "image_url", "image_url": { "url": format!("data:{};base64,{}", attachment.mime, attachment.data_b64) } }
            ]
        }]);
        // No tools: this is a question about a picture, not a turn.
        let reply = crate::llm::chat(transport.as_mut(), &cfg, &messages, &json!([]))
            .map_err(|e| McpError::hardware(format!("image_read: {e}")))?;
        let text = crate::llm::truncate_bytes(&reply.content, 1800);
        Ok(json!({
            "name": attachment.name,
            "model": cfg.model,
            "text": text,
            "prompt_tokens": reply.usage.prompt_tokens,
            "completion_tokens": reply.usage.completion_tokens,
        }))
    }

    /// If this `net_*` call points at the board itself, the method, path and body to serve.
    ///
    /// Recognised hosts are loopback, the station address and the setup-AP address — that
    /// is, anything a model would write when it means "this board". The port has to match
    /// `mcp.port`, so a real second service on the LAN is untouched.
    fn self_target(&mut self, args: &Value) -> Option<(String, String, String)> {
        let url = args.get("url").and_then(|v| v.as_str())?.trim();
        let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().ok()?),
            None => (authority, if url.starts_with("https://") { 443 } else { 80 }),
        };
        if port != self.cfg.mcp.port {
            return None;
        }
        let wifi = self.hw.wifi_status();
        let mine = ["127.0.0.1", "localhost", "::1"];
        let ours = mine.contains(&host) || host == wifi.ip || host == wifi.ap_ip;
        if !ours {
            return None;
        }
        let method = if args.get("body").is_some() { "POST" } else { "GET" };
        let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Some((method.to_string(), path.to_string(), body))
    }

    /// Answer a request that was aimed at this board, without a socket.
    fn serve_self(&mut self, method: &str, path: &str, body: String) -> std::result::Result<String, String> {
        let (pure_path, query) = match path.split_once('?') {
            Some((p, q)) => (p.to_string(), q),
            None => (path.to_string(), ""),
        };
        let query: std::collections::BTreeMap<String, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let req = Request {
            method: method.to_string(),
            path: pure_path,
            query,
            headers: Default::default(),
            body: body.into_bytes(),
            peer: "self".to_string(),
            // An in-process call has no listener; it is the caller's own view of the
            // board, and http is what the self-call path represents.
            scheme: "http",
        };
        let resp = self.route(&req);
        let text = String::from_utf8_lossy(&resp.body).to_string();
        if !(200..300).contains(&resp.status) {
            return Err(format!("HTTP {}: {}", resp.status, crate::llm::body_prefix(&text, 200)));
        }
        Ok(json!({
            "url": path,
            "method": method,
            "status": resp.status,
            "in_process": true,
            "bytes": text.len(),
            "truncated": text.len() > crate::tools::net::MAX_BODY_BYTES,
            "body": crate::llm::truncate_bytes(&text, crate::tools::net::MAX_BODY_BYTES),
        })
        .to_string())
    }

    pub fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        // The script tool is the one tool that needs the dispatcher itself: `tool()` inside
        // a script calls any other tool, so it cannot live in the registry where handlers
        // only see a `ToolCtx`.
        if name == "script" {
            return self.run_script(args);
        }
        // `shell` is a router in front of the fs_*/net_* handlers, so it has to be reached
        // from both entry points: the model's tool host and the MCP `tools/call`.
        if name == "shell" {
            let text = self.run_shell(args).map_err(McpError::invalid_params)?;
            return Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "text": text })));
        }
        // Familiar names, same handlers. `curl`/`wget`/`nslookup`/`nc` are what people
        // already type; the fs_*/net_* names stay reachable from `script`.
        if let Some(usage) = crate::tools::shell::usage(name) {
            if args.get("help").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Ok(json!({ "help": format!("usage: {usage}") }));
            }
        }
        if let Some(inner) = match name {
            "read" => Some("fs_read"),
            "write" => Some("fs_write"),
            "edit" => Some("fs_edit"),
            "nslookup" => Some("net_dns"),
            "nc" => Some("net_tcp"),
            _ => None,
        } {
            let text = self
                .fs_or_net_call(inner, args)
                .map_err(|e| McpError::invalid_params(with_usage(name, e)))?;
            return Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "text": text })));
        }
        if name == "curl" || name == "wget" {
            let text =
                self.run_curl(name, args).map_err(|e| McpError::invalid_params(with_usage(name, e)))?;
            return Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "text": text })));
        }
        if name == "ifconfig" {
            return Ok(self.ifconfig_json());
        }
        if name == "ping" {
            return self.run_ping(args).map_err(McpError::invalid_params);
        }
        // `image_read` asks the model about an attachment, which means it needs the model
        // config and the transport — neither of which a `ToolCtx` carries.
        if name == "image_read" {
            return self.image_read(args);
        }
        // A name a family took over still answers. It is hidden from the listing, not
        // removed from the board, so a script or a person who learned `manifest_apply`
        // before the family existed keeps working — and the listing says where it went.
        if !self.is_visible(name) && !self.family_members.contains_key(name) {
            // A tool a person has and a model does not: the refusal has to be a sentence, not a
            // bare "no such tool". A stored script that calls `reboot` was written by somebody
            // who believed the model could restart the board; that person needs to be told
            // where a restart lives now, not left guessing at a missing name.
            if Self::NEVER_OFFERED.contains(&name) {
                return Err(McpError::forbidden(
                    "`reboot` restarts the board and ends the run that asked for it, so it is not a tool. \
                     A restart is a person's action: the portal's restart button, or `POST /api/reboot` \
                     behind the PIN. Nothing on this board reaches it through a tool call.",
                )
                .with_data(json!({ "restart_route": "/api/reboot" })));
            }
            let missing =
                self.tools.get(name).map(|d| self.caps.missing(&d.spec.requires)).unwrap_or_default();
            if missing.is_empty() {
                return Err(McpError::not_found(format!("no tool `{name}`")));
            }
            return Err(McpError::unsupported(format!(
                "tool `{name}` needs hardware that was not detected: {}",
                missing.join(", ")
            ))
            .with_data(json!({ "missing_capabilities": missing })));
        }
        let out = {
            let App {
                hw,
                devices,
                cfg,
                runtime,
                log,
                caps,
                profile,
                probe,
                tools,
                manifest,
                tasks,
                rules,
                ..
            } = self;
            let def = match tools.get(name) {
                Some(d) => d,
                None => return Err(McpError::not_found(format!("no tool `{name}`"))),
            };
            // The names the board itself provides. `manifest_apply` validates against
            // them, so a manifest cannot take over `manifest_apply` (or a driver tool
            // that a later probe will bring back) and leave the board unable to repair
            // itself.
            let taken: Vec<String> =
                tools.iter().filter(|d| !d.spec.from_manifest).map(|d| d.spec.name.clone()).collect();
            let mut ctx = ToolCtx {
                hw: &mut **hw,
                devices,
                cfg,
                runtime,
                log,
                caps,
                profile,
                probe,
                manifest,
                taken: &taken,
                tasks,
                rules,
            };
            (def.handler)(&mut ctx, args)
        };
        self.runtime.record_tool_call(name, out.is_ok());
        self.settle();
        out
    }

    /// Queue the config for the store writer.
    ///
    /// Every `persist_*` in this file works this way: the state is serialised here, queued
    /// in [`store::StoreQueue`], and written by the platform's writer task. `set` is true
    /// when the write was accepted *into the queue*; whether flash took it is reported by
    /// the writer (`/api/status.store.failed` and the `store` log line), because the caller
    /// here is very often the agent thread, which cannot touch flash at all — see
    /// [`App::flush_store`].
    fn persist_config(&mut self) {
        let json = self.cfg.to_json_string();
        if self.store.set(crate::config::NVS_KEY_CONFIG, &json) {
            self.log.info("config", "saved");
        } else {
            self.log.error("config", "the config could not even be queued for saving");
        }
        self.runtime.config_dirty = false;
    }

    fn persist_manifest(&mut self) {
        let json = self.manifest.to_json_string();
        if self.store.set(NVS_KEY_MANIFEST, &json) {
            self.runtime.manifest_rev += 1;
            self.log.info("manifest", format!("saved, revision {}", self.runtime.manifest_rev));
        } else {
            self.log.error("manifest", "the manifest could not even be queued for saving");
        }
    }

    /// Work owed after a call or a request: persist, rebuild, notice a restart.
    pub fn settle(&mut self) {
        self.invalidate_tool_specs();
        let manifest_changed = self.runtime.manifest_dirty;
        if manifest_changed {
            self.runtime.manifest_dirty = false;
            self.persist_manifest();
        }
        if self.runtime.config_dirty {
            self.persist_config();
        }
        if self.runtime.sched_dirty {
            self.runtime.sched_dirty = false;
            self.persist_sched();
        }
        if manifest_changed || self.runtime.reprobe_requested {
            self.runtime.reprobe_requested = false;
            self.rebuild();
        }
    }

    // ---------------------------------------------------------------- agent

    /// Restore the model config, the scheduler lists and the file workspace.
    ///
    /// A blob that does not parse is logged and skipped rather than fatal: a
    /// board that boots with defaults is worth more than one that refuses to.
    pub fn load_agent_state(&mut self) {
        if let Some(text) = self.store.get(NVS_KEY_AGENT_CFG) {
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => llm_apply_json(&mut self.llm, &v),
                Err(e) => self
                    .log
                    .error("agent", format!("the stored model config is unreadable ({e}); using defaults")),
            }
        }
        if let Some(text) = self.store.get(NVS_KEY_SCHED) {
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => {
                    self.tasks = v
                        .get("tasks")
                        .and_then(|t| t.as_array())
                        .map(|arr| arr.iter().filter_map(|t| task_from(t, "")).collect())
                        .unwrap_or_default();
                    self.rules = v
                        .get("rules")
                        .and_then(|r| r.as_array())
                        .map(|arr| arr.iter().filter_map(rule_from).collect())
                        .unwrap_or_default();
                }
                Err(e) => self
                    .log
                    .error("agent", format!("the stored schedule is unreadable ({e}); starting empty")),
            }
        }
        self.load_workspace();
        // Last, because resuming needs the config that decides whether anything was stored
        // at all, and the transcript a resumed turn continues from.
        self.load_transcript();
    }

    /// Restore the file workspace, migrating the one-value blob an older build wrote.
    ///
    /// Three outcomes, and the difference matters:
    ///
    /// * a blob this build wrote is taken chunk by chunk, each one checked against the
    ///   header;
    /// * a plain JSON value under `ws` is the old format: it is read, and then written back
    ///   in the chunked one. Migration happens here rather than in a version bump because
    ///   there is nothing to bump: the old value is complete on its own, so a board that
    ///   upgrades mid-life keeps every file it had;
    /// * a header or chunk that does not check out is logged and dropped whole. Half a
    ///   workspace would come back as a file list no boot ever wrote, which is the same rule
    ///   the transcript follows.
    fn load_workspace(&mut self) {
        if self.load_workspace_files() {
            return;
        }
        // Older layouts: one document (current shape) or the chunked one, each of which is
        // migrated to one-value-per-file below, so a board that upgrades mid-life keeps its
        // files. Two layouts live under this key. `ws1 ...` is the chunked one written while
        // the store could only take values; a document that starts with `{` is the current one.
        let mut peek = [0u8; 4];
        let head = match self.store.read_at(NVS_KEY_WORKSPACE, 0, &mut peek) {
            Some(n) if n > 0 => n,
            _ => return,
        };
        if head >= 4 && &peek == b"ws1 " {
            self.load_workspace_chunked();
            return;
        }
        let Some(reader) = self.store.reader(NVS_KEY_WORKSPACE) else { return };
        match AppWorkspace::from_reader(reader) {
            Ok(ws) => {
                self.workspace = ws;
                self.migrate_workspace("one document");
            }
            Err(e) => {
                // The value cannot be read by this build or any other, so leaving it in
                // flash only costs the space and hides the fact from the next boot.
                self.log.error(
                    "agent",
                    format!("the stored workspace is unreadable ({e}); starting with an empty one"),
                );
                store::clear_blob(self.store.as_mut(), NVS_KEY_WORKSPACE, WORKSPACE_PLAN);
            }
        }
    }

    /// Rewrite a workspace that was read from an older layout, and say how it went.
    fn migrate_workspace(&mut self, from: &str) {
        let (files, bytes) = (self.workspace.len(), self.workspace.bytes());
        match self.persist_workspace() {
            Ok(()) => self.log.info(
                "agent",
                format!(
                    "migrated the stored workspace from {from} to one value per file: {files} file(s), {bytes} bytes"
                ),
            ),
            // Not fatal: the old value is untouched until the new index lands, so the next boot
            // reads it again and tries once more.
            Err(e) => self.log.error(
                "agent",
                format!("the workspace was read from {from} but could not be re-written ({e})"),
            ),
        }
    }

    /// Read the workspace from its index and one value per file. `false` when there is no
    /// index, which means this board still holds an older layout.
    fn load_workspace_files(&mut self) -> bool {
        let Some(doc) = self.store.get(crate::keys::WORKSPACE_INDEX) else { return false };
        let index: std::collections::BTreeMap<String, String> = match serde_json::from_str(&doc) {
            Ok(index) => index,
            Err(e) => {
                self.log
                    .error("agent", format!("the workspace index is unreadable ({e}); no files were loaded"));
                return true;
            }
        };
        let mut ws = AppWorkspace::default();
        let mut missing = 0usize;
        let mut bytes = 0usize;
        for (path, key) in &index {
            match self.store.get(key) {
                Some(text) => {
                    bytes += text.len();
                    ws.files.insert(path.clone(), text);
                }
                None => missing += 1,
            }
        }
        if missing > 0 {
            self.log.error(
                "agent",
                format!("{} workspace file(s) named by the index are missing from the store", missing),
            );
        }
        self.log
            .info("agent", format!("workspace: {} file(s), {bytes} bytes from the index", ws.files.len()));
        ws.saved = ws.files.iter().map(|(path, text)| (path.clone(), crate::keys::fnv1a(text))).collect();
        self.workspace = ws;
        true
    }

    /// The chunked layout: a `ws1` header under `ws` and the document in `ws.1`, `ws.2`, ...
    fn load_workspace_chunked(&mut self) {
        let (text, legacy) = match store::read_blob(self.store.as_ref(), NVS_KEY_WORKSPACE, WORKSPACE_PLAN) {
            store::BlobState::Missing => return,
            store::BlobState::Stored(text) => (text, false),
            store::BlobState::Legacy(text) => (text, true),
            store::BlobState::Corrupt(why) => {
                self.log.error(
                    "agent",
                    format!("the stored workspace does not check out ({why}); starting with an empty one"),
                );
                store::clear_blob(self.store.as_mut(), NVS_KEY_WORKSPACE, WORKSPACE_PLAN);
                return;
            }
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => self.workspace = AppWorkspace::from_json(&v),
            Err(e) => {
                // The value cannot be read by this build or any other, so leaving it in
                // flash only costs the space and hides the fact from the next boot.
                self.log.error(
                    "agent",
                    format!("the stored workspace is unreadable ({e}); starting with an empty one"),
                );
                store::clear_blob(self.store.as_mut(), NVS_KEY_WORKSPACE, WORKSPACE_PLAN);
                return;
            }
        }
        if legacy {
            self.migrate_workspace("one value");
        }
    }

    // ---------------------------------------------------------------- transcript

    /// Where one chunk of the stored transcript lives. The first one owns the base key.
    fn transcript_key(index: usize) -> String {
        crate::keys::transcript(index)
    }

    /// The stored transcript, chunks joined in order, or `None` when nothing was stored.
    fn read_transcript(&self) -> Option<String> {
        let mut text = self.store.get(NVS_KEY_TRANSCRIPT)?;
        for index in 1..TRANSCRIPT_CHUNKS_MAX {
            match self.store.get(&Self::transcript_key(index)) {
                Some(part) => text.push_str(&part),
                None => break,
            }
        }
        Some(text)
    }

    /// Store the blob, split at line boundaries so no value is longer than the board's NVS
    /// will take.
    ///
    /// Chunk 0 is written first and carries the checksum, so a write cut short by a power
    /// failure leaves a header that no longer matches its body. That is detected on the
    /// next boot and the blob is discarded whole - a half-published transcript would be
    /// worse than none, because the model would be told it said things it did not.
    fn write_transcript(&mut self, text: &str) -> bool {
        // A store that takes a whole value (a file on `storage`) keeps the transcript as one
        // value. The chunking below exists because an NVS value is ~4 KB, and a tail split
        // over eight of them is a tail that fits in 28 KB — which is why the transcript could
        // never hold a real conversation. `read_transcript` reads the base key and then any
        // chunks that follow, so a whole-value write needs nothing else.
        if let Some(cap) = self.store.whole_value_bytes() {
            if text.len() <= cap {
                if !self.store.set(NVS_KEY_TRANSCRIPT, text) {
                    self.log.error("agent", "the transcript could not be written");
                    return false;
                }
                // A tail of chunks from a build that chunked it would be appended on the next
                // read, which is two conversations in one.
                for index in 1..TRANSCRIPT_CHUNKS_MAX {
                    if self.store.get(&Self::transcript_key(index)).is_none() {
                        break;
                    }
                    self.store.remove(&Self::transcript_key(index));
                }
                return true;
            }
        }
        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();
        for line in text.split_inclusive('\n') {
            if !current.is_empty() && current.len() + line.len() > TRANSCRIPT_CHUNK_BYTES {
                chunks.push(std::mem::take(&mut current));
            }
            current.push_str(line);
        }
        if !current.is_empty() {
            chunks.push(current);
        }
        if chunks.len() > TRANSCRIPT_CHUNKS_MAX {
            // Unreachable while `MAX_SAVED_BYTES` and the chunk size agree; refusing beats
            // writing a tail and leaving the rest of the previous one behind it.
            self.log.error("agent", "the transcript needs more chunks than the store allows; not saved");
            return false;
        }
        for (index, chunk) in chunks.iter().enumerate() {
            if !self.store.set(&Self::transcript_key(index), chunk) {
                self.log.error("agent", format!("could not write transcript chunk {index}"));
                return false;
            }
        }
        // A shorter transcript must not be followed by the tail of the longer one it
        // replaced, or the next read concatenates two conversations.
        for index in chunks.len()..TRANSCRIPT_CHUNKS_MAX {
            if self.store.get(&Self::transcript_key(index)).is_none() {
                break;
            }
            self.store.remove(&Self::transcript_key(index));
        }
        true
    }

    fn remove_transcript(&mut self) -> bool {
        let mut removed = self.store.remove(NVS_KEY_TRANSCRIPT);
        for index in 1..TRANSCRIPT_CHUNKS_MAX {
            if self.store.get(&Self::transcript_key(index)).is_none() {
                break;
            }
            removed = self.store.remove(&Self::transcript_key(index)) || removed;
        }
        removed
    }

    /// Keep the conversation for the next boot: the tail of the history, or nothing when
    /// there is nothing to keep.
    ///
    /// Called at every boundary of a turn - after the queued message enters the history and
    /// after each model reply - rather than only at the end. That is what makes "the tool
    /// results that landed before the power went out are still there" true: a turn can die
    /// in the middle, and only what was written before that point comes back.
    ///
    /// An empty history always clears the keys, whatever `agent.persist_transcript` says: a
    /// forget that did not reach flash would come back at the next boot, which is the worst
    /// thing this feature could do.
    pub fn persist_transcript(&mut self) -> bool {
        if self.agent.history.is_empty() {
            let had = self.remove_transcript();
            if had {
                self.log.info("agent", "the stored transcript was dropped");
            }
            return had;
        }
        if !self.cfg.agent.persist_transcript {
            return false;
        }
        let text = crate::agent::transcript_jsonl(&self.agent.history, now_ms());
        let ok = self.write_transcript(&text);
        if !ok {
            self.log.error("agent", "the transcript could not be saved; a reboot loses this conversation");
        }
        ok
    }

    /// Restore the transcript from flash and finish a turn a power cut interrupted.
    ///
    /// Two independent pieces of evidence are read here, in this order:
    ///
    /// * the transcript, which holds everything up to the last completed step. A blob that
    ///   does not check out - torn by a power failure, written by a newer build, or just not
    ///   JSON - is logged and skipped, never a panic and never half-adopted;
    /// * the in-flight marker, whose presence means a turn was running when the board lost
    ///   power. The question it was answering is queued again with the source
    ///   [`RESUME_SOURCE`], so the turn continues from the last completed step: a tool call
    ///   whose result is already in the transcript is not repeated, because the model sees
    ///   the result and has no reason to ask again.
    pub fn load_transcript(&mut self) {
        if self.cfg.agent.persist_transcript {
            if let Some(text) = self.read_transcript() {
                match crate::agent::transcript_from_jsonl(&text) {
                    Some((messages, dropped)) => {
                        let kept = messages.len();
                        self.agent.history = messages;
                        self.agent.saved_dropped = dropped;
                        self.log.info("agent", format!("restored {kept} message(s) from flash"));
                        if dropped > 0 {
                            self.log.warn(
                                "agent",
                                format!(
                                    "{dropped} older message(s) were not stored; the saved tail is capped at {} bytes",
                                    crate::agent::MAX_SAVED_BYTES
                                ),
                            );
                        }
                    }
                    None => self.log.warn(
                        "agent",
                        "the stored transcript does not check out (a write torn by a power cut looks like this); starting a new conversation",
                    ),
                }
            }
        }
        // The marker is consumed whatever the setting says: leaving one behind would resume
        // a turn from weeks ago the day persistence is switched back on.
        let Some(marker) = self.store.get(NVS_KEY_RUN_IN_FLIGHT).filter(|text| !text.is_empty()) else {
            return;
        };
        let parsed: Value = serde_json::from_str(&marker).unwrap_or(Value::Null);
        let source = parsed.get("source").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let started = parsed.get("started_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        self.store.remove(NVS_KEY_RUN_IN_FLIGHT);
        match self.agent.last_user().map(|text| text.to_string()) {
            Some(text) => {
                let extra = if self.agent.ends_on_a_tool_result() {
                    " The tool results it already had are kept, so those calls do not run again."
                } else {
                    ""
                };
                self.log.warn(
                    "agent",
                    format!(
                        "a turn from `{}` (started {started} ms after boot) was interrupted by a reboot; asking it again.{extra}",
                        clip(&source, 40)
                    ),
                );
                self.agent.push_user(&text, RESUME_SOURCE, now_ms());
            }
            None => self.log.warn(
                "agent",
                format!(
                    "a turn from `{}` was interrupted by a reboot, but the stored transcript has no question left to resume",
                    clip(&source, 40)
                ),
            ),
        }
    }

    /// Note that a run has started, so the next boot knows a turn was cut off.
    ///
    /// `source` and a timestamp, and nothing else: the transcript already holds the
    /// question, and a marker that duplicated it could disagree with it. Nothing is written
    /// when the transcript is not being kept, because a marker without a transcript has no
    /// question to resume and would only be noise in the next boot log.
    fn mark_run_in_flight(&mut self, source: &str) -> bool {
        if !self.cfg.agent.persist_transcript {
            return false;
        }
        let marker = json!({ "source": source, "started_ms": now_ms() }).to_string();
        let ok = self.store.set(NVS_KEY_RUN_IN_FLIGHT, &marker);
        if !ok {
            // Worth a line: without it a power cut mid-turn looks like a finished turn, and
            // the question is never asked again.
            self.log.warn("agent", "could not write the run-in-flight marker");
        }
        ok
    }

    fn clear_run_in_flight(&mut self) {
        self.store.remove(NVS_KEY_RUN_IN_FLIGHT);
    }

    /// Hand over the HTTP client the `net_*` tools use. The model call has its own,
    /// owned by whoever drives the turn (`drive_agent`), so a `net_*` call inside a
    /// turn and the model request never compete for one client — and, unlike a client
    /// the turn borrows, this one is still there while the turn runs.
    pub fn attach_tool_transport(&mut self, transport: Box<dyn ChatTransport + Send>) {
        self.tool_transport = Some(transport);
    }

    /// Hand over the client `agent_run_once` should call the model with.
    ///
    /// Only for a runner with no driver loop of its own (the host tests); the firmware
    /// and the simulator both use [`drive_agent`] and pass their client in there.
    pub fn attach_model_transport(&mut self, transport: Box<dyn ChatTransport + Send>) {
        self.model_transport = Some(transport);
    }

    /// Phase 1 of a turn, and the answer to "is there work": take the queued message,
    /// journal it, and hand back the request the model should be asked.
    ///
    /// Nothing here touches the network, so the caller holds the lock for a few
    /// microseconds. `None` means nothing was queued.
    pub fn agent_begin(&mut self) -> Option<AgentStep> {
        // Nothing queued is not an error, it is "no work": an idle tick calls this.
        let source = self.agent.pending.as_ref().map(|pending| pending.source.clone())?;
        let max_steps = self.llm.max_steps;
        let (context_tokens, max_output_tokens) = (self.llm.context_tokens, self.llm.max_output_tokens);
        // Before the run, not after: the marker is what tells the next boot that this turn
        // did not finish, and a marker written afterwards would only describe turns that
        // already survived.
        self.mark_run_in_flight(&source);
        let step = self.with_agent(move |agent, host| {
            agent.set_budget(context_tokens, max_output_tokens);
            agent.begin(host, now_ms, max_steps)
        });
        // The queued question is in the transcript now. Saving here is what lets the next
        // boot resume this turn instead of the one before it.
        self.persist_transcript();
        step
    }

    /// Phase 2: fold one model reply back in (journal, tool calls, transcript) and say
    /// whether to ask again.
    pub fn agent_reply(&mut self, reply: std::result::Result<llm::ModelReply, String>) -> AgentStep {
        let step = self.with_agent(move |agent, host| agent.apply(host, reply, now_ms));
        if matches!(&step, AgentStep::Done(_)) {
            // The turn is over, so a reboot from here must not ask it again. Clearing the
            // marker before the final save means the worst case is a lost reply, never a
            // reply the board repeats.
            self.clear_run_in_flight();
        }
        // Every step, not only the last one: a tool result that landed is worth more after
        // a power cut than the reply that never arrived.
        self.persist_transcript();
        step
    }

    /// Run a closure with the agent moved out of the app, so it can borrow the app
    /// itself as the tool host. It goes back afterwards, which is why the portal still
    /// sees the transcript between phases.
    fn with_agent<R>(&mut self, run: impl FnOnce(&mut Agent, &mut App) -> R) -> R {
        let mut agent = std::mem::replace(&mut self.agent, Agent::new());
        let out = run(&mut agent, self);
        self.agent = agent;
        out
    }

    /// Evaluate the schedule once and queue whatever came due.
    ///
    /// Kept here rather than in the platform layer because firing a task touches the
    /// store (last_run_ms), the tool host (sensor rules call a tool) and the agent
    /// queue. Pure time is passed in, so the host tests can drive it.
    /// Returns the sources that were queued, in order.
    pub fn sched_tick(&mut self, at_ms: u64, minute_of_day: u32) -> Vec<String> {
        let mut fired = Vec::new();
        let mut dirty = false;
        for i in 0..self.tasks.len() {
            if !self.tasks[i].enabled || !crate::sched::due(&self.tasks[i], at_ms, minute_of_day) {
                continue;
            }
            let source = format!("schedule:{}", self.tasks[i].id);
            let prompt = self.tasks[i].prompt.clone();
            let script = self.tasks[i].script.clone();
            self.tasks[i].last_run_ms = at_ms;
            dirty = true;
            if source.len() >= 64 {
                continue;
            }
            // A task either asks the model something or runs a script. The script path
            // costs no tokens and no model call, which is the reason it exists: a five
            // minute rule that pulses a relay should not pay for a chat turn.
            let script = if script.trim().is_empty() { None } else { Some(script) };
            if let Some(source_text) = script {
                let job = self.submit_script(&source_text, true);
                self.log.info("script", format!("{source} ran a script: {}", job.id));
                fired.push(source);
            } else if !prompt.trim().is_empty() {
                self.agent.push_user(&prompt, &source, at_ms);
                self.log.info("agent", format!("queued {source}"));
                fired.push(source);
            }
        }
        let mut due_rules: Vec<(usize, String, String)> = Vec::new();
        for (i, rule) in self.rules.iter().enumerate() {
            if !rule.enabled {
                continue;
            }
            let cooled = rule.last_fire_ms == 0
                || at_ms.saturating_sub(rule.last_fire_ms) >= rule.min_interval_s as u64 * 1000;
            if !cooled {
                continue;
            }
            let tool = rule.tool.clone();
            let path = rule.path.clone();
            due_rules.push((i, tool, path));
        }
        for (i, tool, path) in due_rules {
            let args = json!({});
            let value = match ToolHost::call_tool(self, &tool, &args) {
                Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
                Err(e) => {
                    self.log.warn("agent", format!("rule {} could not read `{tool}`: {e}", self.rules[i].id));
                    continue;
                }
            };
            let seen = crate::sched::json_path(&value, &path).unwrap_or(Value::Null);
            if !crate::sched::rule_matches(&self.rules[i], &seen) {
                continue;
            }
            let source = format!("sensor:{}", self.rules[i].id);
            let prompt = self.rules[i].prompt.clone();
            self.rules[i].last_fire_ms = at_ms;
            dirty = true;
            if !prompt.trim().is_empty() {
                self.agent.push_user(&prompt, &source, at_ms);
                self.log.info("agent", format!("queued {source}"));
                fired.push(source);
            }
        }
        if dirty {
            self.persist_sched();
        }
        fired
    }

    /// The model config as the portal may see it: everything except the key.
    pub fn agent_config_json(&self) -> Value {
        let mut v = llm_json(&self.llm, false);
        v["api_key_set"] = json!(!self.llm.api_key.is_empty());
        v["endpoint_ok"] =
            json!(self.llm.base_url.starts_with("http://") || self.llm.base_url.starts_with("https://"));
        // The platform tick, so the portal can show and edit it next to the model settings.
        v["tick_ms"] = json!(self.cfg.agent.tick_ms);
        v["run"] = json!({
            "runs": self.agent.runs,
            "last_error": self.agent.last_error,
            "history": self.agent.history.len(),
            "pending": self.agent.pending.is_some(),
            "busy": self.agent.pending.is_some(),
            "events": self.agent.events.len(),
            "usage_run": {
                "prompt_tokens": self.agent.usage_run.prompt_tokens,
                "completion_tokens": self.agent.usage_run.completion_tokens,
                "total_tokens": self.agent.usage_run.total(),
            },
            "usage_total": {
                "prompt_tokens": self.agent.usage_total.prompt_tokens,
                "completion_tokens": self.agent.usage_total.completion_tokens,
                "total_tokens": self.agent.usage_total.total(),
            },
        });
        v
    }

    fn persist_agent_cfg(&mut self) {
        let text = llm_json(&self.llm, true).to_string();
        if !self.store.set(NVS_KEY_AGENT_CFG, &text) {
            self.log.error("agent", "could not save the model config");
        }
    }

    fn persist_sched(&mut self) {
        let text = self.sched_json().to_string();
        if !self.store.set(NVS_KEY_SCHED, &text) {
            self.log.error("agent", "could not save the schedule");
        }
    }

    /// Queue the workspace, or say why it cannot be stored at all.
    ///
    /// The return value is the point: a blob that cannot fit the storage plan has to reach
    /// the caller as an error, because the caller is the one about to answer `ok: true` to
    /// a model or a person, and [`App::workspace_edit`] undoes the change on `Err`. What is
    /// checked here is therefore everything that is knowable before flash is touched —
    /// the chunk count, the per-value ceiling, the UTF-8 boundaries; NVS refusing a write
    /// it has room for is left to the writer task, which counts it in
    /// `/api/status.store.failed` and logs the keys.
    ///
    /// The write itself is a request (see [`App::flush_store`]): this runs on whatever
    /// thread made the change, and on this board only the store writer may touch flash.
    /// Write the workspace: one value per file, plus an index that names them.
    ///
    /// This used to write the whole document on every save. Two measurements made that
    /// untenable: the time grew with the workspace (14 s at 0.25 MB, 277 s at 2 MB, because a
    /// save rewrote all of it), and the save needed a second copy of the document on flash at
    /// the same time, which SPIFFS refused at about 2 MB with `ENOSPC` while reporting 9 MB
    /// free. Per file, a save costs one file, and nothing is ever duplicated on flash.
    ///
    /// The index goes last: a value written without an index entry is garbage the next save
    /// overwrites, while an index entry whose value is missing loses one file, not the board.
    fn persist_workspace(&mut self) -> std::result::Result<(), String> {
        let mut index: std::collections::BTreeMap<String, String> = Default::default();
        let mut written: std::collections::BTreeMap<String, u64> = Default::default();
        let mut failed: Vec<String> = Vec::new();

        for (path, text) in &self.workspace.files {
            let key = crate::keys::workspace_file(path);
            index.insert(path.clone(), key.clone());
            let hash = crate::keys::fnv1a(text);
            if self.workspace.saved.get(path) == Some(&hash) {
                written.insert(path.clone(), hash);
                continue;
            }
            if self.store.set(&key, text) {
                written.insert(path.clone(), hash);
            } else {
                // The old bytes stay: the key is derived from the path, not from the content,
                // and `publish` renames only once the new value is complete.
                failed.push(path.clone());
            }
        }

        // Values whose file is gone. Only the paths we wrote before, so a stale key from an
        // interrupted session is left to the save that follows it.
        for path in self.workspace.saved.keys() {
            if !self.workspace.files.contains_key(path) {
                self.store.remove(&crate::keys::workspace_file(path));
            }
        }

        let doc = serde_json::to_string(&index)
            .map_err(|e| format!("the workspace index could not be written: {e}"))?;
        let committed = self.store.set(crate::keys::WORKSPACE_INDEX, &doc);
        if committed {
            // The layouts this one replaced, under keys nothing reads any more: the whole
            // document (`ws`, or `ws1 ...` in its header) and its chunks. Asked for before
            // removing, so the usual save leaves them alone entirely.
            if self.store.get(NVS_KEY_WORKSPACE).is_some() {
                self.store.remove(NVS_KEY_WORKSPACE);
            }
            for chunk in 1..=WORKSPACE_PLAN.chunks_max {
                let key = crate::keys::workspace_chunk(chunk);
                if self.store.get(&key).is_none() {
                    break;
                }
                self.store.remove(&key);
            }
            self.workspace.saved = written;
        }

        if !failed.is_empty() {
            let why = format!(
                "{} workspace file(s) were not saved ({}); the rest are",
                failed.len(),
                failed.join(", ")
            );
            self.log.error("agent", why.clone());
            return Err(why);
        }
        if !committed {
            let why = "the workspace index was refused by the store, see /api/status.store";
            self.log.error("agent", why);
            return Err(why.to_string());
        }
        Ok(())
    }

    /// One change to the workspace, durable or undone.
    ///
    /// Every caller that changes a file goes through here, so that "the write failed" and
    /// "the file is there" cannot both be true. Without the rollback a failed save left the
    /// new text in RAM: `write` answered ok, `read` served the file, and the next boot came
    /// back without it - the failure was only visible in a log line nobody was reading.
    /// PSRAM free right now, in bytes, when the platform can say.
    ///
    /// Asked before a change that needs a copy of the workspace, so that running out is a
    /// refusal with numbers in it instead of the allocator's `abort()`.
    fn psram_free_bytes(&mut self) -> Option<usize> {
        self.hw.heap_pools().into_iter().find(|p| p.pool == "spiram").map(|p| p.free_kb as usize * 1024)
    }

    fn workspace_edit<T>(
        &mut self,
        change: impl FnOnce(&mut AppWorkspace) -> std::result::Result<T, String>,
    ) -> std::result::Result<T, String> {
        // The rollback below needs a copy of the whole map, so this is the moment the board can
        // run out of PSRAM. Ask first: a refused write is a sentence the caller can act on, and
        // an allocation that fails is `memory allocation of N bytes failed` followed by
        // `abort()` - a reboot, measured on this board.
        if let Some(free) = self.psram_free_bytes() {
            workspace_fits(self.workspace.bytes(), Some(free))?;
        }
        let before = self.workspace.clone();
        let outcome = change(&mut self.workspace)?;
        match self.persist_workspace() {
            Ok(()) => Ok(outcome),
            Err(e) => {
                self.workspace = before;
                Err(format!(
                    "{e}. The change was dropped, so `read` shows the file as it was before this call"
                ))
            }
        }
    }

    /// Put the skills the firmware carries into the workspace, where they are missing.
    ///
    /// Called once at boot, after the stored workspace is loaded, and again after a
    /// factory reset — the reset is exactly the moment the owner's `skills.seed_defaults`
    /// decision and any deleted file go back to what the firmware ships. Returns how many
    /// it wrote, which is 0 on every boot after the first and 0 always when the flag is
    /// off.
    pub fn seed_default_skills(&mut self) -> usize {
        if !self.cfg.skills.seed_defaults {
            return 0;
        }
        let wrote = crate::seed::seed(&mut self.workspace, self.profile.id);
        if wrote > 0 {
            self.log.info(
                "boot",
                format!(
                    "seeded {wrote} of {} built-in skill(s) into {}",
                    crate::seed::DEFAULT_SKILLS.len(),
                    crate::tools::knowledge::SKILLS_DIR
                ),
            );
            // The workspace is one persisted blob; a skill that exists only in RAM would be
            // installed again next boot, and would be missing from what the portal and an
            // MCP client read today. Seeding is best effort - there is nothing to roll back
            // to, since this is the boot that had no workspace - so a failure is a log line
            // rather than an error nobody could act on.
            if let Err(e) = self.persist_workspace() {
                self.log.error("boot", format!("the seeded skills are in RAM only: {e}"));
            }
        }
        wrote
    }

    /// Queue one user message. Returns 202 with the queue position; the run
    /// itself happens on the agent thread, so a slow model never blocks a request.
    fn agent_message(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        let mut text = body.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();

        // Attachments: a name, a mime type and base64 bytes. The board cannot show an
        // image and the model here is text-only, so what the model gets is a line naming
        // the file; the bytes are served back at `/api/attachment/<name>` for whoever can
        // use them. Refusing quietly would be worse than this: the user would attach a
        // screenshot and never learn it went nowhere.
        let mut notes: Vec<String> = Vec::new();
        if let Some(list) = body.get("attachments").and_then(|v| v.as_array()) {
            if list.len() > MAX_ATTACHMENTS {
                return Response::json_status(
                    413,
                    &json!({ "error": format!("{} attachments; the limit is {MAX_ATTACHMENTS}", list.len()) }),
                );
            }
            self.attachments.clear();
            for item in list {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("attachment").to_string();
                let mime = item
                    .get("mime")
                    .and_then(|v| v.as_str())
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let mut bytes = Vec::new();
                let encoded = item.get("data_b64").and_then(|v| v.as_str()).unwrap_or("");
                if let Err(e) = crate::util::b64_decode(encoded, &mut bytes) {
                    return Response::json_status(
                        400,
                        &json!({ "error": format!("`{name}`: base64: {}", e.message) }),
                    );
                }
                if bytes.len() > MAX_ATTACHMENT_BYTES {
                    return Response::json_status(
                        413,
                        &json!({
                            "error": format!(
                                "`{name}` is {} bytes; an attachment is limited to {MAX_ATTACHMENT_BYTES}",
                                bytes.len()
                            )
                        }),
                    );
                }
                notes.push(format!(
                    "[attached: {name}, {mime}, {} kB — fetch with GET /api/attachment/{name}]",
                    bytes.len().div_ceil(1024)
                ));
                let data_b64 = crate::util::b64_encode(&bytes);
                self.attachments.push(crate::agent::Attachment { name, mime, data_b64 });
            }
            if !notes.is_empty() {
                text.push_str("\n\n");
                text.push_str(&notes.join("\n"));
            }
        }
        if text.trim().is_empty() {
            return Response::json_status(400, &json!({ "error": "`text` is required" }));
        }
        if text.len() > AGENT_TEXT_MAX {
            return Response::json_status(
                413,
                &json!({ "error": format!("`text` is {} bytes; the limit is {AGENT_TEXT_MAX}", text.len()) }),
            );
        }
        // Kept twice on purpose: the app serves the bytes back, the message sends them.
        // Vecs over 512 bytes live in PSRAM and an attachment is capped at 24 kB.
        let attachments = self.attachments.clone();
        self.agent.vision = self.llm.vision;
        if !self.agent.push_user(&text, "http", now_ms()) {
            return Response::json_status(400, &json!({ "error": "`text` is required" }));
        }
        // Attachments belong to the message, not to the app: the request that carries
        // them and the transcript that reports them must not be able to disagree.
        if let Some(message) = self.agent.pending.as_mut() {
            message.attachments = attachments;
        }
        // The index this message will occupy once the run appends it: the queue is
        // depth 1, so a second trigger coalesces into the same slot.
        let seq = self.agent.history.len() + usize::from(self.agent.pending.is_some());
        Response::json_status(202, &json!({ "queued": true, "seq": seq }))
    }

    /// The transcript for the portal, including the one message that is queued but
    /// not yet run.
    ///
    /// [`Agent::messages_json`] only knows committed history, so a page that queued
    /// a run would show nothing until the model answered — and look like it dropped
    /// the message. The pending turn is appended here, flagged, and disappears once
    /// the run appends the real one.
    /// `GET /api/files/tree`: the whole workspace in one answer.
    ///
    /// The workspace is at most 8 KB of text, so a directory-at-a-time view costs a round
    /// trip per level for a tree that fits in one response. Every file brings its path, and
    /// the directories are the prefixes of those paths — the same rule `list` uses, so the
    /// tree and `ls` cannot disagree about what a directory is.
    fn files_tree_json(&self) -> Value {
        let mut files: Vec<(String, usize)> =
            self.workspace.paths().map(|(path, text)| (path.clone(), text.len())).collect();
        files.sort();

        let mut dirs: std::collections::BTreeSet<String> = Default::default();
        for (path, _) in &files {
            let mut prefix = String::new();
            for part in path.trim_start_matches('/').split('/') {
                prefix.push('/');
                prefix.push_str(part);
                // The last segment is the file itself, not a directory.
                if prefix.as_str() != path.as_str() {
                    dirs.insert(prefix.clone());
                }
            }
        }

        let mut entries: Vec<Value> = dirs
            .iter()
            .map(|path| {
                let name = path.rsplit('/').next().unwrap_or(path);
                json!({ "name": name, "path": path, "dir": true })
            })
            .collect();
        entries.extend(files.into_iter().map(|(path, bytes)| {
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            json!({ "name": name, "path": path, "dir": false, "size_bytes": bytes })
        }));

        json!({
            "ok": true,
            "count": entries.len(),
            "entries": entries,
            "total_bytes": self.workspace.bytes(),
            "total_max": WORKSPACE_TOTAL_MAX,
            "file_max": WORKSPACE_FILE_MAX,
        })
    }

    /// `GET /api/files?path=/dir`: one directory of the workspace, directories first.
    ///
    /// Shaped like the `ls` tool on purpose — one workspace, one meaning of `size_bytes` —
    /// plus the two totals. The workspace is a single 8 KB blob stored in NVS chunks, so
    /// "how much of it is used" is the question a person asks when a write is refused, and
    /// a per-file number cannot answer it.
    fn files_json(&self, path: &str) -> Value {
        use crate::tools::fs::Workspace as _;
        let dir = match fs_tools::normalize(path) {
            Ok(d) => d,
            Err(e) => return json!({ "ok": false, "error": e }),
        };
        let names = match self.workspace.list(&dir) {
            Ok(n) => n,
            Err(e) => return json!({ "ok": false, "path": dir, "error": e }),
        };
        let mut entries: Vec<Value> = Vec::new();
        for name in names {
            let child = fs_tools::join(&dir, &name);
            // A name that does not read as a file is a directory: the trait's contract, and
            // the same rule `ls` uses, so the two views cannot disagree.
            match self.workspace.read(&child) {
                Ok(text) => entries.push(json!({
                    "name": name, "path": child, "dir": false, "size_bytes": text.len(),
                })),
                Err(_) => entries.push(json!({ "name": name, "path": child, "dir": true })),
            }
        }
        entries.sort_by(|a, b| {
            let ad = a["dir"].as_bool().unwrap_or(false);
            let bd = b["dir"].as_bool().unwrap_or(false);
            bd.cmp(&ad).then_with(|| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")))
        });
        json!({
            "ok": true,
            "path": dir,
            "count": entries.len(),
            "entries": entries,
            "total_bytes": self.workspace.bytes(),
            "total_max": WORKSPACE_TOTAL_MAX,
            "file_max": WORKSPACE_FILE_MAX,
        })
    }

    /// `GET /api/files/read?path=/x.md`: one file, as text.
    fn file_text_json(&self, path: &str) -> Value {
        use crate::tools::fs::Workspace as _;
        let file = match fs_tools::normalize(path) {
            Ok(f) => f,
            Err(e) => return json!({ "ok": false, "error": e }),
        };
        match self.workspace.read(&file) {
            Ok(text) => json!({
                "ok": true, "path": file, "size_bytes": text.len(), "text": text,
            }),
            Err(e) => json!({ "ok": false, "path": file, "error": e }),
        }
    }

    fn agent_history_json(&self, after: usize) -> Value {
        let mut v = self.agent.messages_json(after);
        // Token accounting travels with the transcript, the way the host agent reports
        // usage beside the messages.
        v["usage"] = json!({
            "last_run": {
                "prompt_tokens": self.agent.usage_run.prompt_tokens,
                "completion_tokens": self.agent.usage_run.completion_tokens,
                "total_tokens": self.agent.usage_run.total(),
            },
            "total": {
                "prompt_tokens": self.agent.usage_total.prompt_tokens,
                "completion_tokens": self.agent.usage_total.completion_tokens,
                "total_tokens": self.agent.usage_total.total(),
            },
        });
        if let Some(pending) = self.agent.pending.as_ref() {
            let m = json!({
                "role": "user",
                "text": pending.text,
                "source": pending.source,
                "at_ms": pending.at_ms,
                "tool_calls": [],
                "pending": true,
            });
            if let Some(arr) = v["messages"].as_array_mut() {
                arr.push(m);
            }
        }
        v
    }

    /// `GET /api/agent/stream?after=<seq>`: the run journal as Server-Sent Events.
    ///
    /// The response carries a body that is written after the headers, so the
    /// connection outlives this call. The app lock is taken per poll, never for the
    /// life of the stream - that is the property the whole design is for.
    fn agent_stream(&mut self, req: &Request) -> Response {
        let after = req.query_get("after").and_then(|v| v.trim().parse().ok()).unwrap_or(0u64);
        let resp = self.agent_stream_from(after, StreamLimits::default());
        if resp.is_stream() {
            let peer = req.peer.clone();
            self.log.info("agent", format!("event stream opened for {peer} from seq {after}"));
        }
        resp
    }

    fn agent_stream_from(&self, after: u64, limits: StreamLimits) -> Response {
        // Take the slot *before* the socket commits to a long write: a third console
        // gets its 503 while the connection is still cheap to answer, and never
        // consumes one of the six pool sockets for two minutes.
        let Some(slot) = self.streams.acquire() else {
            // `Connection: close`: a refused stream must not become a socket the pool
            // has to wait out. The console reconnects on its own, or not at all.
            return Response::error_json(503, "too many event streams")
                .with_header("Connection", "close")
                // A console that retries immediately would burn the next slot as soon as
                // one frees; the streams close on their own, so tell it how long to wait.
                .with_header("Retry-After", "5");
        };
        let Some(shared) = self.shared.upgrade() else {
            // An `App` nobody handed its own handle to: the journal cannot be polled
            // after this call returns. `make_handler` always wires it; a unit test
            // has to call `App::attach_shared`.
            return Response::error_json(503, "event streams unavailable: app handle not attached");
        };
        Response::streamed(200, "text/event-stream", event_stream_body(shared, slot, after, limits))
    }

    fn agent_config_apply(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        // Both schemes are carried by the platform transport (TLS on the board via
        // esp-tls, on the host via rustls), so https:// is accepted here. Anything that
        // is not HTTP at all is still refused by name.
        if let Some(url) = body.get("base_url").and_then(|u| u.as_str()) {
            let url = url.trim();
            if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                return Response::json_status(
                    400,
                    &json!({ "error": "`base_url` must start with http:// or https:// and point at the chat completions endpoint" }),
                );
            }
            self.llm.base_url = url.to_string();
        }
        // Apply the trimmed value too, so what is stored is exactly what was checked.
        let mut patch = body.clone();
        if let Some(url) = patch.get("base_url").and_then(|u| u.as_str()) {
            let url = url.trim().to_string();
            patch["base_url"] = json!(url);
        }
        llm_apply_json(&mut self.llm, &patch);
        // The tick belongs to the device config, not the model config, but the portal
        // edits both from one form: accept it here and clamp it the way the platform
        // does, so an out-of-range value never reaches the thread.
        if let Some(ms) = body.get("tick_ms").and_then(|v| v.as_u64()) {
            let clamped = crate::config::clamp_tick_ms(ms.min(u32::MAX as u64) as u32);
            self.cfg.agent.tick_ms = clamped;
            self.runtime.config_dirty = true;
            self.log.info("agent", format!("scheduler tick is now {clamped} ms"));
        }
        self.persist_agent_cfg();
        self.log.info(
            "agent",
            format!(
                "model config: {} model={} key={}",
                clip(&self.llm.base_url, 60),
                self.llm.model,
                if self.llm.api_key.is_empty() { "none" } else { "set" }
            ),
        );
        let mut v = self.agent_config_json();
        v["saved"] = json!(true);
        Response::json(&v)
    }

    fn sched_json(&self) -> Value {
        json!({
            "now_ms": now_ms(),
            "tasks": self.tasks.iter().map(task_json).collect::<Vec<Value>>(),
            "rules": self.rules.iter().map(rule_json).collect::<Vec<Value>>(),
        })
    }

    fn next_task_id(&self) -> String {
        for n in 1..=MAX_TASKS * 4 {
            let id = format!("t{n}");
            if !self.tasks.iter().any(|t| t.id == id) {
                return id;
            }
        }
        format!("t{}", self.tasks.len() + 1)
    }

    fn next_rule_id(&self) -> String {
        for n in 1..=MAX_RULES * 4 {
            let id = format!("r{n}");
            if !self.rules.iter().any(|r| r.id == id) {
                return id;
            }
        }
        format!("r{}", self.rules.len() + 1)
    }

    /// Add, update, enable/disable or delete one task or rule by id.
    /// Fire one task immediately, the way the tick would have at its due moment.
    ///
    /// A task either asks the model something or runs a script, exactly as on the timer, so a
    /// manual run and a scheduled one cannot diverge. `last_run_ms` is deliberately not
    /// touched: "run it now" must not postpone the run that was scheduled.
    fn sched_run_now(&mut self, id: &str) -> std::result::Result<Value, String> {
        let at_ms = now_ms();
        let index =
            self.tasks.iter().position(|t| t.id == id).ok_or_else(|| format!("no scheduled task `{id}`"))?;
        let source = format!("schedule:{}", self.tasks[index].id);
        if source.len() >= 64 {
            return Err(format!("the source tag for `{id}` is too long"));
        }
        let prompt = self.tasks[index].prompt.clone();
        let script = self.tasks[index].script.clone();
        let script = if script.trim().is_empty() { None } else { Some(script) };
        let mut out = json!({ "id": id, "source": source });
        if let Some(text) = script {
            let job = self.submit_script(&text, true);
            self.log.info("script", format!("{source} ran a script on demand: {}", job.id));
            out["kind"] = json!("script");
            out["job"] = json!(job.id);
        } else if !prompt.trim().is_empty() {
            self.agent.push_user(&prompt, &source, at_ms);
            self.log.info("sched", format!("{source} queued on demand"));
            out["kind"] = json!("prompt");
            out["queued"] = json!(true);
        } else {
            return Err(format!("task `{id}` has neither a prompt nor a script"));
        }
        Ok(out)
    }

    fn sched_apply(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        let kind = body.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let op = body.get("op").and_then(|o| o.as_str()).unwrap_or("upsert");
        let id = body.get("id").and_then(|i| i.as_str()).unwrap_or("").trim().to_string();
        match (kind, op) {
            ("task", "delete") => {
                let before = self.tasks.len();
                self.tasks.retain(|t| t.id != id);
                if self.tasks.len() == before {
                    return Response::json_status(
                        404,
                        &json!({ "error": format!("no scheduled task `{id}`") }),
                    );
                }
            }
            ("task", "enable") | ("task", "disable") => {
                let want = op == "enable";
                match self.tasks.iter_mut().find(|t| t.id == id) {
                    Some(t) => t.enabled = want,
                    None => {
                        return Response::json_status(
                            404,
                            &json!({ "error": format!("no scheduled task `{id}`") }),
                        );
                    }
                }
            }
            ("task", "add") | ("task", "update") | ("task", "upsert") => {
                let src = body.get("task").unwrap_or(&body);
                let fallback = self.next_task_id();
                let task = match task_from(src, &fallback) {
                    Some(t) => t,
                    None => return Response::json_status(400, &json!({ "error": "`prompt` is required" })),
                };
                if let Some(why) = task_problem(&task) {
                    return Response::json_status(400, &json!({ "error": why }));
                }
                match self.tasks.iter().position(|t| t.id == task.id) {
                    Some(i) => {
                        // A patch that does not mention the clock keeps the old one.
                        let mut task = task;
                        if src.get("last_run_ms").is_none() {
                            task.last_run_ms = self.tasks[i].last_run_ms;
                        }
                        self.tasks[i] = task;
                    }
                    None => {
                        if self.tasks.len() >= MAX_TASKS {
                            return Response::json_status(
                                400,
                                &json!({ "error": format!("at most {MAX_TASKS} scheduled tasks") }),
                            );
                        }
                        self.tasks.push(task);
                    }
                }
            }
            ("rule", "delete") => {
                let before = self.rules.len();
                self.rules.retain(|r| r.id != id);
                if self.rules.len() == before {
                    return Response::json_status(404, &json!({ "error": format!("no sensor rule `{id}`") }));
                }
            }
            ("rule", "enable") | ("rule", "disable") => {
                let want = op == "enable";
                match self.rules.iter_mut().find(|r| r.id == id) {
                    Some(r) => r.enabled = want,
                    None => {
                        return Response::json_status(
                            404,
                            &json!({ "error": format!("no sensor rule `{id}`") }),
                        )
                    }
                }
            }
            ("rule", "add") | ("rule", "update") | ("rule", "upsert") => {
                let src = body.get("rule").unwrap_or(&body);
                let fallback = self.next_rule_id();
                let rule = match rule_from_id(src, &fallback) {
                    Some(r) => r,
                    None => return Response::json_status(400, &json!({ "error": "`tool` is required" })),
                };
                if let Some(why) = rule_problem(&rule) {
                    return Response::json_status(400, &json!({ "error": why }));
                }
                match self.rules.iter().position(|r| r.id == rule.id) {
                    Some(i) => {
                        let mut rule = rule;
                        if src.get("last_fire_ms").is_none() {
                            rule.last_fire_ms = self.rules[i].last_fire_ms;
                        }
                        self.rules[i] = rule;
                    }
                    None => {
                        if self.rules.len() >= MAX_RULES {
                            return Response::json_status(
                                400,
                                &json!({ "error": format!("at most {MAX_RULES} sensor rules") }),
                            );
                        }
                        self.rules.push(rule);
                    }
                }
            }
            _ => {
                return Response::json_status(
                    400,
                    &json!({ "error": "`kind` must be \"task\" or \"rule\" and `op` one of add/update/upsert/delete/enable/disable" }),
                )
            }
        }
        self.persist_sched();
        let mut v = self.sched_json();
        v["ok"] = json!(true);
        Response::json(&v)
    }

    /// Run the queued message, if there is one, and keep the transcript.
    ///
    /// The HTTP routes only enqueue; the agent thread in the platform layer (and
    /// the simulator) call this, which is why it is a plain method and not part of
    /// the request path.
    pub fn agent_run_once(&mut self) -> std::result::Result<String, String> {
        if self.agent.pending.is_none() {
            return Ok(String::new());
        }
        let cfg = self.llm.clone();
        // The source of the queued message, which the journal and the transcript record.
        let source = self.agent.pending.as_ref().map(|p| p.source.clone()).unwrap_or_default();
        // This runner takes the whole turn in one call (`run_pending`), so its transcript
        // write comes at the end and the marker can only say "a turn was running". A power
        // cut here resumes the last question that reached flash, which is the previous one;
        // the driver path (`agent_begin`) writes the question before the first model call,
        // and that is the path the firmware and the simulator use.
        self.mark_run_in_flight(&source);
        // The agent lives in `self` and needs `self` as its tool host, so it is
        // moved out for the duration of the run and put back afterwards.
        let mut agent = std::mem::replace(&mut self.agent, Agent::new());
        let mut transport = self.model_transport.take();
        let (out, ran) = match transport.as_mut() {
            Some(t) => (agent.run_pending(self, t.as_mut(), &cfg, now_ms), true),
            None => {
                let why = "no HTTP transport is attached for the model call, so the model cannot be reached"
                    .to_string();
                agent.last_error = why.clone();
                (Err(why), false)
            }
        };
        self.model_transport = transport;
        self.agent = agent;
        // Only a turn that actually ran ends here: a missing transport leaves the
        // message queued for a later attempt and keeps the run-in-flight marker, so the
        // next boot resumes it instead of forgetting it.
        if ran {
            self.clear_run_in_flight();
        }
        self.persist_transcript();
        match &out {
            Ok(text) if !text.is_empty() => self.log.info("agent", format!("run done: {}", clip(text, 120))),
            Ok(_) => self.log.info("agent", "run done with no text"),
            Err(e) => self.log.warn("agent", format!("run failed: {}", clip(e, 160))),
        }
        out
    }

    // ---------------------------------------------------------------- RPC

    pub fn handle_rpc(&mut self, body: &str, session: Option<&str>) -> Option<Value> {
        self.handle_rpc_with(body, session, "http")
    }

    /// As [`Self::handle_rpc`], but knowing which listener answered. `initialize` quotes
    /// an endpoint, and quoting the wrong scheme sends the next agent to a dead port.
    pub fn handle_rpc_with(&mut self, body: &str, session: Option<&str>, scheme: &str) -> Option<Value> {
        let messages = match protocol::parse(body) {
            Ok(m) => m,
            Err((id, code_n, message)) => {
                self.runtime.counters.bad_requests += 1;
                return Some(protocol::err(id, code_n, &message, None));
            }
        };
        let single = messages.len() == 1;
        let mut out = Vec::new();
        for msg in messages {
            if let Some(v) = self.handle_one(msg, session, scheme) {
                out.push(v);
            }
        }
        self.settle();
        if out.is_empty() {
            None
        } else if single {
            out.pop()
        } else {
            Some(Value::Array(out))
        }
    }

    fn handle_one(&mut self, msg: RpcIn, session: Option<&str>, scheme: &str) -> Option<Value> {
        let id = msg.id.clone();
        if msg.is_notification {
            self.runtime.counters.notifications += 1;
            if msg.method == "notifications/initialized" {
                if let Some(s) = session {
                    if let Some(entry) = self.runtime.sessions.iter_mut().find(|s2| s2.id == s) {
                        entry.initialized = true;
                    }
                }
            }
            return None;
        }
        self.runtime.counters.requests += 1;
        let result = match msg.method.as_str() {
            "initialize" => Ok(self.on_initialize(&msg.params, session, scheme)),
            "ping" => Ok(json!({})),
            "tools/list" => {
                if self.cfg.mcp.reprobe_on_list {
                    self.rebuild();
                }
                Ok(self.tools_list_json())
            }
            "tools/call" => self.on_tools_call(&msg.params),
            "resources/list" => Ok(self.resources_list()),
            "resources/read" => self.on_resources_read(&msg.params),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            "logging/setLevel" => Ok(json!({})),
            "completion/complete" => Ok(json!({ "completion": { "values": [], "total": 0 } })),
            other => Err(McpError::new(
                code::METHOD_NOT_FOUND,
                format!("method `{other}` is not supported by esp32-agent"),
            )),
        };
        match result {
            Ok(v) => Some(protocol::ok(id, v)),
            Err(e) => {
                self.log.warn("rpc", format!("{}: {}", msg.method, e.message));
                Some(protocol::err(id, e.code, &e.message, e.data))
            }
        }
    }

    fn on_initialize(&mut self, params: &Value, session: Option<&str>, scheme: &str) -> Value {
        let client = params
            .get("clientInfo")
            .and_then(|c| c.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string();
        let protocol_version = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(protocol::PROTOCOL_VERSION)
            .to_string();
        if let Some(s) = session {
            let idx = self.runtime.touch_session(s, now_ms(), &client);
            self.runtime.sessions[idx].protocol = protocol_version;
        }
        let instructions = self.instructions(scheme);
        protocol::initialize_result(&self.firmware.name, &self.firmware.version, &instructions, true)
    }

    fn on_tools_call(&mut self, params: &Value) -> Result<Value> {
        let name = params
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| McpError::invalid_params("`name` is required"))?
            .to_string();
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let started = std::time::Instant::now();
        // The tools outside the registry go through `ToolHost`, which is the path the
        // board's own agent uses, so an MCP caller and the agent share one behaviour.
        let result = if name.starts_with("fs_") || name.starts_with("net_") {
            if self.cfg.mcp.disabled_tools.iter().any(|d| d == &name) {
                Err(McpError::not_found(format!("tool `{name}` is disabled")))
            } else {
                match ToolHost::call_tool(self, &name, &args) {
                    Ok(text) => Ok(text_result(&text)),
                    Err(message) => Err(McpError::invalid_params(message)),
                }
            }
        } else {
            self.call_tool(&name, &args)
        };
        let ms = started.elapsed().as_millis();
        match result {
            Ok(v) => {
                self.log.info("tool", format!("{name} ok in {ms} ms"));
                let payload =
                    if v.is_object() && v.get("content").is_some() { v } else { protocol::tool_json(&v) };
                Ok(payload)
            }
            Err(e) => {
                self.log.warn("tool", format!("{name} failed in {ms} ms: {}", e.message));
                Ok(protocol::tool_error(&e))
            }
        }
    }

    fn resources_list(&mut self) -> Value {
        let mut resources = vec![
            json!({ "uri": "mcp://capabilities", "name": "capabilities", "mimeType": "application/json" }),
            json!({ "uri": "mcp://devices", "name": "devices", "mimeType": "application/json" }),
            json!({ "uri": "mcp://manifest", "name": "manifest", "mimeType": "application/json" }),
            json!({ "uri": "mcp://config", "name": "config", "mimeType": "application/json" }),
            json!({ "uri": "mcp://logs", "name": "logs", "mimeType": "application/json" }),
            json!({ "uri": "mcp://catalog", "name": "catalog", "mimeType": "application/json" }),
        ];
        for info in self.devices.infos() {
            resources.push(json!({
                "uri": format!("mcp://devices/{}", info.id),
                "name": info.id,
                "title": info.name,
                "mimeType": "application/json",
            }));
        }
        json!({ "resources": resources })
    }

    fn on_resources_read(&mut self, params: &Value) -> Result<Value> {
        let uri = params
            .get("uri")
            .and_then(|u| u.as_str())
            .ok_or_else(|| McpError::invalid_params("`uri` is required"))?;
        let value = match uri {
            "mcp://capabilities" => json!({
                "caps": self.caps.to_json(),
                // The same list the model gets and the same reasons the portal shows: a
                // client asking "what can this board do" must not get a third answer.
                "visible_tools": self
                    .tool_specs()
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t["function"]["name"].as_str())
                    .collect::<Vec<&str>>(),
                "hidden": self.hidden_tools(),
            }),
            "mcp://devices" => json!({
                "count": self.devices.len(),
                "devices": self.devices.infos(),
                "unidentified": self.probe.unknown,
                "buses": self.probe.buses,
            }),
            "mcp://manifest" => json!({
                "revision": self.runtime.manifest_rev,
                "manifest": self.manifest,
            }),
            "mcp://config" => self.cfg.public_json(),
            "mcp://logs" => self.log.to_json(80, 0),
            "mcp://catalog" => drivers::catalog_json(),
            other => {
                if let Some(id) = other.strip_prefix("mcp://devices/") {
                    self.devices.describe(id, &mut *self.hw)?
                } else {
                    return Err(McpError::not_found(format!("unknown resource `{other}`")));
                }
            }
        };
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        Ok(json!({
            "contents": [ { "uri": uri, "mimeType": "application/json", "text": text } ]
        }))
    }

    // ---------------------------------------------------------------- HTTP

    /// A single `tools/list` request answered without building the whole result as
    /// a [`Value`].
    ///
    /// Held as a value tree the reply for 28 tool schemas costs several times the
    /// size of the JSON it prints, and on the bench board that ran the chip out of
    /// heap and aborted it on the first `tools/list` a client sent. Serialising one
    /// tool at a time keeps the peak at one tool plus the finished text.
    ///
    /// Returns `None` for anything else — a batch, a notification, another method —
    /// so the caller can fall back to [`App::handle_rpc`].
    pub fn tools_list_body(&mut self, body: &str) -> Option<String> {
        if body.trim_start().starts_with('[') {
            // A batch answers several messages; keep it on the generic path.
            return None;
        }
        let mut parsed = protocol::parse(body).ok()?;
        if parsed.len() != 1 {
            return None;
        }
        let msg = parsed.pop()?;
        if msg.method != "tools/list" || msg.is_notification {
            return None;
        }
        self.runtime.counters.requests += 1;
        if self.cfg.mcp.reprobe_on_list {
            self.rebuild();
        }
        // One list, sorted by name across both sources, byte for byte what
        // `tools_list_json` returns. The two paths exist because one avoids building a
        // Value tree on a board with a few kB of heap; they must not disagree about order,
        // or the same client sees a different list depending on the transport.
        let mut entries: Vec<Value> =
            self.tools.iter().filter(|d| self.is_visible(&d.spec.name)).map(|d| d.mcp_json()).collect();
        entries.extend(self.extra_mcp_specs());
        entries.sort_by_key(|t| t["name"].as_str().unwrap_or("").to_string());

        let id = serde_json::to_string(&msg.id.unwrap_or(Value::Null)).ok()?;
        // Same shape `protocol::ok` produces (serde_json orders object keys).
        let mut out = String::with_capacity(512 + entries.len() * 640);
        out.push_str("{\"id\":");
        out.push_str(&id);
        out.push_str(",\"jsonrpc\":\"2.0\",\"result\":{\"tools\":[");
        let mut first = true;
        for entry in entries {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&serde_json::to_string(&entry).ok()?);
        }
        out.push_str("]}}");
        Some(out)
    }

    pub fn route(&mut self, req: &Request) -> Response {
        self.runtime.counters.http_requests += 1;
        let resp = self.route_inner(req);
        self.settle();
        resp
    }

    fn route_inner(&mut self, req: &Request) -> Response {
        let method = req.method.as_str();
        let path = req.path.as_str();
        let mcp_path = self.cfg.mcp.path.clone();

        if method == "OPTIONS" && self.cfg.mcp.cors {
            return Response::empty(204)
                .with_header("Access-Control-Allow-Origin", "*")
                .with_header("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS")
                .with_header(
                    "Access-Control-Allow-Headers",
                    "Content-Type, Authorization, Mcp-Session-Id, MCP-Protocol-Version, X-Portal-Pin",
                )
                .with_header("Access-Control-Max-Age", "600");
        }

        // What was attached to the last message, for whoever can use it: an external
        // agent, a vision model later, or a person downloading their own screenshot back.
        if let Some(name) = path.strip_prefix("/api/attachment/") {
            if let Err(r) = self.require_read(req) {
                return r;
            }
            let name = crate::http::url_decode(name);
            return match self.attachments.iter().find(|a| a.name == name) {
                Some(a) => {
                    let mut bytes = Vec::new();
                    if crate::util::b64_decode(&a.data_b64, &mut bytes).is_err() {
                        return Response::error_json(500, "attachment is not decodable");
                    }
                    Response::new(200, &a.mime, bytes).with_header("Cache-Control", "no-store")
                }
                None => Response::error_json(404, "no attachment by that name"),
            };
        }

        if path == mcp_path {
            let resp = match method {
                "POST" => self.handle_mcp_post(req),
                "GET" => Response::error_json(
                    405,
                    "this endpoint uses the POST-only Streamable HTTP transport; open / for the config page",
                ),
                "DELETE" => {
                    let sid = req.header("mcp-session-id").unwrap_or("");
                    self.runtime.sessions.retain(|s| s.id != sid);
                    Response::empty(204)
                }
                _ => Response::error_json(405, "method not allowed"),
            };
            return self.with_cors(resp);
        }

        match (method, path) {
            ("GET", "/") | ("GET", "/index.html") | ("GET", "/setup") => Response::html(web::INDEX_HTML),
            ("GET", "/assets/app.js") => Response::js(web::APP_JS),
            ("GET", "/assets/style.css") => Response::css(web::STYLE_CSS),
            ("GET", "/favicon.ico") => Response::empty(204),
            ("GET", "/healthz") => {
                let up = self.runtime.uptime_s(now_ms());
                Response::json(&json!({ "ok": true, "uptime_s": up, "devices": self.devices.len() }))
            }
            // The map an external agent reads before it touches anything. Two paths so a
            // client that guesses either name finds it.
            ("GET", "/agent.json") | ("GET", "/sitemap.json") => Response::json(&self.sitemap_json(req)),
            ("GET", "/api/tls") => Response::json(&self.tls.json()),
            ("GET", "/api/status") => {
                let v = self.status_json();
                Response::json(&v)
            }
            // The heap, pool by pool. `/api/status` carries the one number that decides
            // whether a socket or a handshake fits; this carries the split, so a change
            // that claims to free internal RAM can be checked against the pool it moved.
            ("GET", "/api/memory") => {
                let pools = self.hw.heap_pools();
                if pools.is_empty() {
                    Response::json(&json!({
                        "available": false,
                        "why": "this platform cannot split its heap by capability; `memory` in /api/status and `shell ps` are the readings it has",
                    }))
                } else {
                    Response::json(&json!({ "available": true, "pools": pools }))
                }
            }
            ("GET", "/api/config") => {
                if let Err(r) = self.require_read(req) {
                    return r;
                }
                Response::json(&self.cfg.public_json())
            }
            ("POST", "/api/config") | ("PUT", "/api/config") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.patch_config(req)
            }
            ("GET", "/api/manifest") => Response::json(&json!({
                "revision": self.runtime.manifest_rev,
                "manifest": self.manifest,
            })),
            ("POST", "/api/manifest") | ("PUT", "/api/manifest") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.apply_manifest_http(req)
            }
            ("GET", "/api/manifest/schema") => {
                // An HTTP response has no 2 KB cap, so this route hands over the part of
                // the reference needed to write a manifest in one go; the model asks for
                // the same content one section at a time (`manifest_describe`).
                let mut out = json!({});
                for section in ["schema", "exec"] {
                    match self.call_tool("manifest_describe", &json!({ "section": section })) {
                        Ok(v) => {
                            if let (Some(dst), Some(src)) = (out.as_object_mut(), v.as_object()) {
                                for (key, value) in src {
                                    if key != "section" && key != "note" {
                                        dst.insert(key.clone(), value.clone());
                                    }
                                }
                            }
                        }
                        Err(e) => return Response::json_status(500, &json!({ "error": e.message })),
                    }
                }
                Response::json(&out)
            }
            ("GET", "/api/devices") => Response::json(&json!({
                "devices": self.devices.infos(),
                "unidentified": self.probe.unknown,
                "buses": self.probe.buses,
                "candidates": self.probe.candidates,
                "discovery": self.cfg.discovery,
                "caps": self.caps.to_json(),
                "summary": drivers::summary(&self.devices),
            })),
            ("POST", "/api/devices/probe") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.rebuild();
                Response::json(&json!({
                    "ok": true,
                    "devices": self.devices.infos(),
                    "unidentified": self.probe.unknown,
                    "caps": self.caps.to_json(),
                    "took_ms": self.probe.duration_ms,
                }))
            }
            ("GET", "/api/tools") => Response::json(&self.tools_portal_json()),
            ("POST", "/api/tools") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.update_tool_selection(req)
            }
            ("GET", "/api/agent/history") => {
                if let Err(r) = self.require_read(req) {
                    return r;
                }
                let after = req.query_get("after").and_then(|v| v.parse().ok()).unwrap_or(0usize);
                Response::json(&self.agent_history_json(after))
            }
            // The run journal, with the host agent's event names. A client polls it
            // with `?after=<seq>` exactly like the transcript.
            ("GET", "/api/agent/events") => {
                let after = req.query_get("after").and_then(|v| v.parse().ok()).unwrap_or(0u64);
                Response::json(&self.agent.events_json(after))
            }
            // The same journal as a live stream: replay from `after`, then follow.
            // Contract (frozen): docs/agent-stream.md §1.
            ("GET", "/api/agent/stream") => self.agent_stream(req),
            ("POST", "/api/agent/message") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.agent_message(req)
            }
            ("POST", "/api/agent/reset") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                let (removed, kept) = self.agent.forget(0);
                self.persist_transcript();
                self.log.info("agent", "transcript cleared from the portal");
                Response::json(&json!({ "reset": true, "removed": removed, "kept": kept }))
            }
            // Forgetting with a boundary: keep the last N exchanges, or none. External
            // agents use this to start a new task without losing what is in flight.
            ("POST", "/api/agent/forget") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                let body: Value = serde_json::from_str(&req.body_str()).unwrap_or(Value::Null);
                let keep = body.get("keep").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let (removed, kept) = self.agent.forget(keep);
                // Forgetting has to reach flash, or the conversation comes back after a
                // reboot - the one outcome nobody asking for `forget` would accept.
                self.persist_transcript();
                self.log.info(
                    "agent",
                    format!("forgot {removed} message(s), kept the last {kept} toward {keep} exchange(s)"),
                );
                Response::json(&json!({
                    "ok": true,
                    "removed": removed,
                    "kept": kept,
                    "archive_cleared": true,
                }))
            }
            ("GET", "/api/agent/config") => Response::json(&self.agent_config_json()),
            // What the model is actually told, system part only. Memory, instructions and
            // the compressed index live here, and a client that cannot see them cannot
            // tell a bad prompt from a bad model.
            // Create, replace, append and clear the standing orders. The `GET` above
            // reports what the model is told; this is the one part of it a person writes.
            ("POST", "/api/agent/prompt") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                let body: Value = match req.json() {
                    Ok(v) => v,
                    Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
                };
                let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("set");
                let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
                match action {
                    "clear" => self.cfg.agent.system_prompt.clear(),
                    "append" => {
                        let current = self.cfg.agent.system_prompt.trim_end();
                        let joined = if current.is_empty() {
                            text.trim().to_string()
                        } else {
                            format!("{current}\n{}", text.trim())
                        };
                        self.cfg.agent.system_prompt =
                            joined.chars().take(crate::config::MAX_SYSTEM_PROMPT).collect();
                    }
                    "set" => {
                        self.cfg.agent.system_prompt =
                            text.chars().take(crate::config::MAX_SYSTEM_PROMPT).collect();
                    }
                    other => {
                        return Response::error_json(
                            400,
                            &format!("unknown action `{other}`; use set, append or clear"),
                        )
                    }
                }
                self.persist_config();
                Response::json(&json!({
                    "ok": true,
                    "action": action,
                    "system_prompt": self.cfg.agent.system_prompt,
                    "bytes": self.cfg.agent.system_prompt.len(),
                    "limit": crate::config::MAX_SYSTEM_PROMPT,
                }))
            }
            ("GET", "/api/agent/prompt") => {
                if let Err(r) = self.require_read(req) {
                    return r;
                }
                let text = self.agent.prompt_text(self);
                Response::json(&json!({
                    "bytes": text.len(),
                    "system": text,
                    "archive_lines": self.agent.archive.len(),
                    "history": self.agent.history.len(),
                }))
            }
            ("POST", "/api/agent/config") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.agent_config_apply(req)
            }
            ("GET", "/api/sched") => Response::json(&self.sched_json()),
            // Run a script from outside: a client writes one and wants the answer. Inline is
            // the default because that is what "call and get the result" means; `async` hands
            // back a job id for a script that loops over hardware for a while.
            ("POST", "/api/script") => {
                // A script can move pins, so it is a write whatever it does: the same PIN rule
                // as `manifest_apply`, for the same reason.
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                let body = match req.json() {
                    Ok(v) => v,
                    Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
                };
                let source = body.get("source").and_then(|s| s.as_str()).unwrap_or("").to_string();
                if source.trim().is_empty() {
                    return Response::json_status(
                        400,
                        &json!({
                            "error": "`source` is required; send {\"source\": \"<script>\"}",
                            "help": "script {\"help\": true} prints the language",
                        }),
                    );
                }
                if source.len() > crate::script::MAX_SOURCE {
                    return Response::json_status(
                        413,
                        &json!({ "error": format!("script is larger than {} bytes", crate::script::MAX_SOURCE) }),
                    );
                }
                let want_async = body.get("async").and_then(|a| a.as_bool()).unwrap_or(false);
                let job = self.submit_script(&source, !want_async);
                let mut out = json!({ "id": job.id, "state": job.state, "async": want_async });
                if want_async {
                    out["poll"] = json!(format!("/api/script/{}", job.id));
                } else {
                    out["ok"] = json!(job.state == "done");
                    out["result"] = job.result;
                    out["output"] = json!(job.output);
                    out["steps"] = json!(job.steps);
                    out["ms"] = json!(job.ms);
                    if !job.error.is_empty() {
                        out["error"] = json!(job.error);
                    }
                }
                Response::json(&out)
            }
            ("GET", path) if path.starts_with("/api/script/") => {
                let id = &path["/api/script/".len()..];
                match self.script_job(id) {
                    Some(job) => Response::json(&json!({
                        "id": job.id,
                        "state": job.state,
                        "ok": job.state == "done",
                        "result": job.result,
                        "output": job.output,
                        "steps": job.steps,
                        "ms": job.ms,
                        "error": job.error,
                    })),
                    None => Response::json_status(404, &json!({ "error": format!("no script job `{id}`") })),
                }
            }
            ("POST", "/api/sched") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.sched_apply(req)
            }
            // Fire one task now. "Wait until 06:00" is not a test, and an outside caller may
            // want the report now. It does not move the schedule: `last_run_ms` is left alone,
            // so a manual run cannot starve the timer that was set for it.
            ("POST", "/api/sched/run") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                let id = match req.json() {
                    Ok(body) => body.get("id").and_then(|i| i.as_str()).unwrap_or("").trim().to_string(),
                    Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
                };
                match self.sched_run_now(&id) {
                    Ok(v) => Response::json(&v),
                    Err(e) => Response::json_status(404, &json!({ "error": e })),
                }
            }
            ("GET", "/api/wifi/scan") => match self.hw.wifi_scan() {
                Ok(aps) => Response::json(&json!({ "networks": aps })),
                Err(e) => Response::json_status(503, &json!({ "error": e.message })),
            },
            ("POST", "/api/wifi/connect") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.wifi_connect(req)
            }
            ("POST", "/api/wifi/forget") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.cfg.wifi.ssid.clear();
                self.cfg.wifi.password.clear();
                self.runtime.config_dirty = true;
                self.persist_config();
                Response::json(&json!({ "ok": true, "note": "credentials cleared" }))
            }
            ("GET", "/api/logs") => {
                let n = req.query_get("n").and_then(|v| v.parse().ok()).unwrap_or(80usize);
                let since = req.query_get("since").and_then(|v| v.parse().ok()).unwrap_or(0u64);
                let mut v = self.log.to_json(n.min(400), since);
                v["firmware"] = json!(self.firmware.version);
                Response::json(&v)
            }
            ("GET", "/api/catalog") => Response::json(&drivers::catalog_json()),
            // The portal's Files tab: read-only, so it needs no PIN like every other read.
            // Both answers carry the same numbers the `ls` and `read` tools return, because
            // there is one workspace and one meaning of "how big is it".
            ("GET", "/api/files") => {
                if let Err(r) = self.require_read(req) {
                    return r;
                }
                let path = req.query_get("path").unwrap_or("/");
                Response::json(&self.files_json(path))
            }
            ("GET", "/api/files/tree") => {
                if let Err(r) = self.require_read(req) {
                    return r;
                }
                Response::json(&self.files_tree_json())
            }
            ("GET", "/api/files/read") => {
                if let Err(r) = self.require_read(req) {
                    return r;
                }
                let path = req.query_get("path").unwrap_or("/");
                Response::json(&self.file_text_json(path))
            }
            ("GET", "/api/sessions") => Response::json(&json!({ "sessions": self.runtime.session_json() })),
            ("GET", "/api/clients") => Response::json(&self.client_snippets()),
            ("POST", "/api/auth") => {
                let pin = req
                    .json()
                    .ok()
                    .and_then(|v| v.get("pin").and_then(|p| p.as_str()).map(|s| s.to_string()))
                    .unwrap_or_default();
                if self.cfg.auth.portal_pin.is_empty() || pin == self.cfg.auth.portal_pin {
                    // A token, not just an acknowledgement: the page keeps it and sends
                    // it as a bearer token, and so can a script. Twelve hours is long
                    // enough for a bench session and short enough that a forgotten tab
                    // on a shared network stops working the next day.
                    let token = self.runtime.issue_token(seed(), now_ms(), 12 * 3600);
                    self.log.info("auth", "portal token issued");
                    Response::json(&json!({
                        "ok": true,
                        "required": !self.cfg.auth.portal_pin.is_empty(),
                        "token": token,
                        "expires_in": 12 * 3600,
                    }))
                } else {
                    self.runtime.counters.auth_failures += 1;
                    Response::json_status(401, &json!({ "ok": false, "error": "wrong PIN" }))
                }
            }
            ("POST", "/api/reboot") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.log.warn("system", "reboot requested from the portal");
                self.runtime.restart_requested = true;
                // No `hw.reboot()` here: restarting from inside this request would cut the
                // reply off and, more importantly, drop whatever the store writer has queued
                // (its tick is up to 100 ms away, and `esp_restart` does not wait for it).
                // The platform's loop notices the flag, flushes on a stack it has checked, and
                // restarts — `/api/config` already works that way for a bus or board change.
                Response::json(&json!({ "ok": true, "note": "rebooting" }))
            }
            ("POST", "/api/factory-reset") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.cfg = Config::default();
                self.manifest = Manifest::default();
                self.runtime.config_dirty = true;
                self.runtime.manifest_dirty = true;
                self.runtime.restart_requested = true;
                // A reset also restores the two things an owner's own decisions replaced:
                // the built-in skills, wherever a file was deleted, and the flag that
                // keeps them installed. An edited skill stays the owner's.
                self.seed_default_skills();
                self.log.warn("system", "factory reset from the portal");
                Response::json(&json!({ "ok": true }))
            }
            ("GET", "/generate_204")
            | ("GET", "/hotspot-detect.html")
            | ("GET", "/connecttest.txt")
            | ("GET", "/redirect")
            | ("GET", "/ncsi.txt") => Response::redirect("/"),
            // A scripted OpenAI-compatible endpoint, for verifying the agent loop
            // without a key and without any inbound network access. Point
            // `agent.base_url` at http://<board>:8080/selftest/chat/completions and
            // the request build, tool dispatch, tool result and final answer all run
            // on the board itself. It only ever returns a canned reply, so it cannot
            // be mistaken for a model, and nothing else depends on it.
            ("POST", "/selftest/chat/completions") => Response::json(&selftest_completion(req)),
            // `/audio/pcm` is consumed before the handler whenever the server was built with
            // `make_routes`, because its body must not be buffered. Reaching this arm means
            // that composition root was not used (a unit test, say); 501 says so, where the
            // redirect a browser-only path would get would say nothing.
            ("POST", AUDIO_STREAM_PATH) => Response::error_json(
                501,
                "this server has no streaming body route: /audio/pcm needs `make_routes`, not `make_handler`",
            ),
            // The same for the sound-and-picture route; see `av_route`.
            ("POST", AV_STREAM_PATH) => Response::error_json(
                501,
                "this server has no streaming body route: /av needs `make_routes`, not `make_handler`",
            ),
            _ => {
                if path.starts_with("/api/") {
                    Response::error_json(404, &format!("no route {method} {path}"))
                } else {
                    Response::redirect("/")
                }
            }
        }
    }

    fn with_cors(&self, resp: Response) -> Response {
        if self.cfg.mcp.cors {
            resp.with_header("Access-Control-Allow-Origin", "*")
                .with_header("Access-Control-Expose-Headers", "Mcp-Session-Id")
        } else {
            resp
        }
    }

    fn require_pin(&mut self, req: &Request) -> std::result::Result<(), Response> {
        if self.cfg.auth.portal_pin.is_empty() {
            return Ok(());
        }
        if self.credential_ok(req) {
            Ok(())
        } else {
            self.runtime.counters.auth_failures += 1;
            Err(Response::json_status(401, &json!({ "error": "PIN required", "pin_required": true })))
        }
    }

    /// True when the request carries the portal PIN or a token minted from it.
    ///
    /// Three ways in, because the three callers differ: a browser keeps a token from
    /// `POST /api/auth`, a script sends `X-Portal-Pin`, and an MCP client sends a bearer
    /// token. All of them are compared here so no route can be reached by a path that
    /// forgot one of them.
    fn credential_ok(&mut self, req: &Request) -> bool {
        let pin = self.cfg.auth.portal_pin.clone();
        let direct = req
            .header("x-portal-pin")
            .map(|s| s.to_string())
            .or_else(|| req.query_get("pin").map(|s| s.to_string()))
            .unwrap_or_default();
        if !direct.is_empty() && direct == pin {
            return true;
        }
        let bearer = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if bearer.is_empty() {
            return false;
        }
        if bearer == pin {
            return true;
        }
        let now = now_ms();
        self.runtime.token_valid(&bearer, now)
    }

    /// Reads that expose secrets, the transcript or the prompt need the same credential
    /// as a write when a PIN is configured: `config` masks values but still lists the
    /// network, and the transcript is the most private thing on the board.
    fn require_read(&mut self, req: &Request) -> std::result::Result<(), Response> {
        if self.cfg.auth.portal_pin.is_empty() {
            return Ok(());
        }
        if self.credential_ok(req) {
            return Ok(());
        }
        self.runtime.counters.auth_failures += 1;
        Err(Response::json_status(401, &json!({ "error": "PIN required", "pin_required": true })))
    }

    fn handle_mcp_post(&mut self, req: &Request) -> Response {
        if !self.cfg.mcp.enabled {
            return Response::error_json(503, "MCP service is disabled in the configuration");
        }
        if !self.cfg.auth.mcp_token.is_empty() {
            let ok = req
                .header("authorization")
                .map(|a| {
                    a.trim_start_matches("Bearer ").trim_start_matches("bearer ").trim()
                        == self.cfg.auth.mcp_token
                })
                .unwrap_or(false);
            if !ok {
                self.runtime.counters.auth_failures += 1;
                return Response::json_status(401, &json!({ "error": "missing or wrong bearer token" }))
                    .with_header("WWW-Authenticate", "Bearer");
            }
        }

        let body = req.body_str();
        if body.trim().is_empty() {
            return Response::error_json(400, "empty body: send a JSON-RPC request");
        }
        let is_initialize = body.contains("\"initialize\"");
        let mut session = req.header("mcp-session-id").map(|s| s.to_string());
        let mut new_session = false;
        if session.is_none() {
            self.session_counter += 1;
            session = Some(make_session_id(seed(), self.session_counter, now_ms()));
            new_session = true;
        }
        let session_id = session.clone().unwrap_or_default();
        let peer = if req.peer.is_empty() { "?".to_string() } else { req.peer.clone() };
        self.runtime.touch_session(&session_id, now_ms(), &peer);
        self.runtime.gc_sessions(
            now_ms(),
            self.cfg.mcp.session_ttl_s as u64 * 1000,
            self.cfg.mcp.max_sessions as usize,
        );

        // `tools/list` takes the streaming path; everything else goes through the
        // generic JSON-RPC handler. See `tools_list_body` for why.
        if let Some(body) = self.tools_list_body(&body) {
            let mut resp = Response::new(200, "application/json; charset=utf-8", body);
            resp.headers.push(("Mcp-Session-Id".into(), session_id));
            if new_session {
                resp.headers.push(("X-Mcp-New-Session".into(), "1".into()));
            }
            return resp;
        }

        let reply = self.handle_rpc_with(&body, Some(&session_id), req.scheme);
        let mut resp = match reply {
            None => Response::empty(202),
            Some(v) => Response::json(&v),
        };
        resp.headers.push(("Mcp-Session-Id".into(), session_id));
        if new_session {
            resp.headers.push(("X-Mcp-New-Session".into(), "1".into()));
        }
        if is_initialize {
            resp.headers.push(("MCP-Protocol-Version".into(), protocol::PROTOCOL_VERSION.into()));
        }
        resp
    }

    fn patch_config(&mut self, req: &Request) -> Response {
        let patch = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        let before = self.cfg.clone();
        // A patch can change the profile, `tools_mode`, `disabled_tools` or `discovery`,
        // all of which decide what `tool_specs` answers — so the cached array goes first.
        self.invalidate_tool_specs();
        match self.cfg.apply_patch(&patch) {
            Ok(touched) => {
                self.runtime.config_dirty = true;
                if before.wifi != self.cfg.wifi && !self.cfg.wifi.ssid.is_empty() {
                    let ssid = self.cfg.wifi.ssid.clone();
                    let pw = self.cfg.wifi.password.clone();
                    if let Err(e) = self.hw.wifi_connect(&ssid, &pw) {
                        self.log.warn("wifi", format!("connect failed: {}", e.message));
                    }
                }
                // The I2C driver owns its pins once the bus is open, so a pin or
                // profile change cannot be applied in place. Compare the *effective*
                // config rather than trusting the list of touched keys: a patch can
                // touch `bus` and change nothing, and it can change the pins through a
                // path the touched list does not name.
                let bus_or_board = before.bus != self.cfg.bus || before.board != self.cfg.board;
                if bus_or_board {
                    self.runtime.restart_requested = true;
                    self.rebuild();
                }
                let mut v = self.status_json();
                v["touched"] = json!(touched);
                v["restart_required"] = json!(bus_or_board || before.mcp.port != self.cfg.mcp.port);
                Response::json(&v)
            }
            Err(e) => Response::json_status(400, &json!({ "error": e.message })),
        }
    }

    fn apply_manifest_http(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        let mut args = body.clone();
        if args.get("manifest").is_none() {
            // Allow posting the document itself.
            args = json!({ "manifest": body });
        }
        match self.call_tool("manifest_apply", &args) {
            Ok(v) => {
                let applied = v["applied"].as_bool().unwrap_or(false);
                let mut v = v;
                v["devices"] = json!(self.devices.infos());
                v["revision"] = json!(self.runtime.manifest_rev);
                Response::json_status(if applied { 200 } else { 422 }, &v)
            }
            Err(e) => Response::json_status(400, &json!({ "applied": false, "error": e.message })),
        }
    }

    fn update_tool_selection(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        if let Some(mode) = body.get("mode").and_then(|m| m.as_str()) {
            if matches!(mode, "auto" | "all" | "generic") {
                self.cfg.mcp.tools_mode = mode.to_string();
            }
        }
        if let Some(disabled) = body.get("disabled").and_then(|d| d.as_array()) {
            self.cfg.mcp.disabled_tools =
                disabled.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
        }
        self.runtime.config_dirty = true;
        self.persist_config();
        let mut v = self.tools_portal_json();
        v["ok"] = json!(true);
        Response::json(&v)
    }

    fn wifi_connect(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        let ssid = body.get("ssid").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let password = body.get("password").and_then(|s| s.as_str()).unwrap_or("").to_string();
        if ssid.is_empty() {
            return Response::error_json(400, "`ssid` is required");
        }
        self.cfg.wifi.ssid = ssid.clone();
        if password != "__set__" {
            self.cfg.wifi.password = password;
        }
        self.runtime.config_dirty = true;
        self.persist_config();
        let pw = self.cfg.wifi.password.clone();
        match self.hw.wifi_connect(&ssid, &pw) {
            Ok(()) => Response::json(&json!({
                "ok": true,
                "ssid": ssid,
                "note": "connecting; keep this page open and it will follow the new address",
            })),
            Err(e) => Response::json_status(500, &json!({ "ok": false, "error": e.message })),
        }
    }

    fn client_snippets(&mut self) -> Value {
        let url = self.mcp_url();
        json!({
            "url": url,
            "curl_list": format!(
                "curl -sS -X POST {url} -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}}'"
            ),
            "curl_call": format!(
                "curl -sS -X POST {url} -H 'Content-Type: application/json' -d '{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"board_status\",\"arguments\":{{}}}}}}'"
            ),
            "mcp_servers_json": {
                "mcpServers": {
                    self.firmware.name.clone(): { "type": "http", "url": url }
                }
            },
            "note": "Clients that speak the Streamable HTTP transport only need the URL; stdio-only clients need a bridge such as mcp-remote.",
        })
    }
}

// ---------------------------------------------------------------- tool host

/// The board's own tools as the model sees them: an OpenAI `tools` array.
///
/// The MCP list is `{"name","inputSchema",…}` while a chat endpoint wants
/// `{"type":"function","function":{…}}`, so the conversion happens here and the
/// two views cannot drift apart.
impl ToolHost for App {
    fn tool_specs(&self) -> Value {
        // Keyed on the settings that decide the answer, not only on an explicit
        // invalidation: `profile`, `tools_mode` and `disabled_tools` are plain fields, and
        // anything that writes them directly (a test, a future caller) must not get a stale
        // array back. The fingerprint is three short hashes against an 18 KB rebuild.
        let key = self.specs_fingerprint();
        if let Some((cached_key, cached)) = self.tool_specs_cache.borrow().as_ref() {
            if *cached_key == key {
                return cached.clone();
            }
        }
        let built = self.tool_specs_uncached();
        *self.tool_specs_cache.borrow_mut() = Some((key, built.clone()));
        built
    }

    /// `/AGENTS.md`, bounded to what a small prompt can carry.
    fn project_instructions(&self) -> Option<String> {
        crate::tools::knowledge::instructions(&self.workspace)
    }

    /// The skill index: name and one line each, never the bodies.
    /// The skill index: name, one line, and the path to read — never the bodies.
    ///
    /// Both shapes reach here: `/skills/<name>/SKILL.md` (a skill with material next to
    /// it) and `/skills/<name>.md` (a skill that is one file).
    fn custom_prompt(&self) -> Option<String> {
        Some(self.cfg.agent.system_prompt.clone())
    }

    fn skills(&self) -> Vec<crate::agent::SkillRef> {
        crate::tools::knowledge::index(&self.workspace)
            .into_iter()
            .map(|s| crate::agent::SkillRef { name: s.name, description: s.description, path: s.path })
            .collect()
    }

    /// The memory index: `/MEMORY.md`, one pointer per memory.
    fn memory_index(&self) -> Option<String> {
        crate::tools::knowledge::memory(&self.workspace)
    }

    fn call_tool(&mut self, name: &str, arguments: &Value) -> std::result::Result<String, String> {
        self.fs_or_net_call(name, arguments)
    }
}

impl App {
    /// A cheap hash of the settings that decide which tools are visible.
    fn specs_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.cfg.mcp.profile.hash(&mut h);
        self.cfg.mcp.tools_mode.hash(&mut h);
        for name in &self.cfg.mcp.disabled_tools {
            name.hash(&mut h);
        }
        self.tools.len().hash(&mut h);
        // The capabilities decide visibility too, and they are not only a function of the
        // device count: a manifest can give an existing device a new cap, and a probe can
        // retire one. Without this line the model would keep the previous array while the
        // portal showed the new one.
        self.caps.to_json().to_string().hash(&mut h);
        h.finish()
    }

    /// Drop the cached tool array. Everything that can change which tools exist calls this:
    /// a manifest apply, a config change, a probe, a reboot.
    pub fn invalidate_tool_specs(&mut self) {
        *self.tool_specs_cache.borrow_mut() = None;
    }

    /// Turn what a caller passed into `args` into the two shapes a script sees.
    ///
    /// A map is passed through. An array is a command line: bare words become `argv`, and
    /// `--name value` / `name=value` pairs also become entries of `args`, which is how a
    /// shell hands arguments to a script. Positional arguments keep their order in `argv`
    /// including the flags, so a script can read either view.
    ///
    /// The array built from scratch: one pass over the registry, one predicate.
    fn tool_specs_uncached(&self) -> Value {
        let visible: Vec<&ToolDef> = self.tools.iter().filter(|d| self.is_visible(&d.spec.name)).collect();
        let load = |def: &ToolDef| {
            json!({
                "type": "function",
                "function": {
                    "name": def.spec.name,
                    "description": def.spec.description,
                    "parameters": def.spec.input_schema,
                }
            })
        };

        // Order is part of the contract: a provider caches the request by its prefix, so
        // one reordered entry and every token after it is reprocessed. The list is
        // therefore sorted by name across every source — board tools, manifest tools and
        // the agent-only fs_*/net_*/script group — which is both stable for a given board
        // and the order a person expects to read. A manifest edit now moves the entries it
        // touches instead of everything after them, because the manifest tools are no
        // longer kept in a block at the end.
        let mut tools: Vec<Value> = visible.iter().map(|d| load(d)).collect();
        // File, HTTP and script tools gate on nothing the hardware must provide, so they
        // are not MCP tools; the model still gets them, in the same sorted list.
        tools.extend(self.extra_specs());
        tools.sort_by_key(|t| t["function"]["name"].as_str().unwrap_or("").to_string());
        Value::Array(tools)
    }

    /// The `fs_*` and `net_*` handlers by name, shared by the tool host and `shell`.
    fn fs_or_net_call(&mut self, name: &str, arguments: &Value) -> std::result::Result<String, String> {
        if name.starts_with("fs_") {
            let call = |ws: &mut AppWorkspace| {
                fs_tools::call(ws, name, arguments)
                    .ok_or_else(|| format!("`{name}` starts with fs_ but no such file tool exists"))?
            };
            let text = if matches!(name, "fs_write" | "fs_edit" | "fs_rm" | "fs_cp" | "fs_mv") {
                // A tool that changes a file answers only once the change is stored, or not at
                // all: a `write` that said ok and lost the file at the next boot is the failure
                // this branch is here to prevent.
                self.workspace_edit(call)?
            } else {
                call(&mut self.workspace)?
            };
            return Ok(text);
        }
        if name.starts_with("net_") {
            if let Some((method, path, body)) = self.self_target(arguments) {
                return self.serve_self(&method, &path, body);
            }
            let mut transport = self.tool_transport.take();
            let result = match transport.as_mut() {
                Some(t) => net_tools::call(t.as_mut(), name, arguments),
                None => {
                    Some(Err("no HTTP transport is attached for the network tools, so net_* cannot run"
                        .to_string()))
                }
            };
            self.tool_transport = transport;
            return result
                .unwrap_or_else(|| Err(format!("`{name}` starts with net_ but no such HTTP tool exists")));
        }
        match App::call_tool(self, name, arguments) {
            Ok(v) => Ok(tool_text(&v)),
            Err(e) => Err(e.message),
        }
    }
}

impl App {
    /// `shell {op, …}`: pick the command, hand the rest of the arguments to the handler.
    ///
    /// A line is a *pipeline* of one or more stages. One stage with no redirect keeps the
    /// answer shape it has always had; anything else runs through [`App::run_pipeline`].
    fn run_shell(&mut self, args: &Value) -> std::result::Result<String, String> {
        // One way in: a command line, as a shell has. No `op`, no parallel object form.
        let line = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if line.trim().is_empty() {
            // `{"help": true}` is the way to the command index now that the declaration no
            // longer carries it, so it answers as a result — not as an error a model has to
            // read as a failure.
            if args.get("help").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Ok(json!({ "help": crate::tools::shell::help_text() }).to_string());
            }
            return Err(format!("`command` is required.\n\n{}", crate::tools::shell::help_text()));
        }
        // `a; b` runs two commands in order, which is what "write a file, then read it back"
        // needs. The split ignores semicolons inside quotes, and stops at four: a line is a
        // shorthand, not a program.
        if let Some(parts) = crate::tools::shell::split_sequence(line)? {
            let mut answers = Vec::new();
            for part in parts {
                let mut inner = args.clone();
                inner["command"] = json!(part);
                answers.push(json!({
                    "command": part,
                    "result": serde_json::from_str::<Value>(&self.run_shell(&inner)?)
                        .unwrap_or(Value::Null),
                }));
            }
            return Ok(json!({ "commands": answers }).to_string());
        }
        let pipeline = crate::tools::shell::parse_pipeline(line)?;
        // `help`, `-h` and `--help` answer before anything is validated, so `cp -h` does not
        // fail for a missing argument on the way to its own usage line. In a pipeline the
        // stage that asked wins, so `ls / | grep --help` explains `grep`.
        let asking = pipeline
            .stages
            .iter()
            .find(|stage| stage.parsed.flags.contains_key("help") || stage.parsed.flags.contains_key("h"));
        let wants_help = args.get("help").and_then(|v| v.as_bool()).unwrap_or(false)
            || asking.is_some()
            || pipeline.stages.iter().any(|stage| stage.parsed.name == "help");
        if wants_help {
            let op = asking
                .map(|stage| stage.parsed.name.as_str())
                .or_else(|| pipeline.stages.first().map(|stage| stage.parsed.name.as_str()))
                .unwrap_or("");
            let text = match crate::tools::shell::usage(op) {
                Some(line) if op != "help" => {
                    format!("usage: {line}\n\n{}", crate::tools::shell::help_text())
                }
                _ => crate::tools::shell::help_text(),
            };
            return Ok(json!({ "help": text }).to_string());
        }
        match pipeline.single() {
            Some(parsed) => self.run_single(parsed, args),
            None => self.run_pipeline(&pipeline),
        }
    }

    /// One command, no pipe and no redirect: the form that existed before pipelines.
    fn run_single(
        &mut self,
        parsed: &crate::tools::shell::Parsed,
        args: &Value,
    ) -> std::result::Result<String, String> {
        let op = parsed.name.clone();
        // A missing argument is a wrong command line like any other, so it answers with the
        // usage line too: `cp` with one word should not be the one error that stays silent
        // about how the command is written.
        let mut inner = match crate::tools::shell::arguments(parsed) {
            Ok(inner) => inner,
            Err(e) => return Err(with_usage(&op, e)),
        };
        if args.get("help").and_then(|v| v.as_bool()).unwrap_or(false) {
            inner["help"] = json!(true);
        }
        let tool = crate::tools::shell::route(&op).ok_or_else(|| {
            format!("`{op}` is not a shell command.\n\n{}", crate::tools::shell::help_text())
        })?;
        let usage = crate::tools::shell::usage(&op).unwrap_or("");
        self.shell_dispatch(tool, &inner).map_err(|e| {
            if usage.is_empty() || e.contains("usage:") {
                e
            } else {
                format!("{e}\n\nusage: {usage}")
            }
        })
    }

    /// The handler behind a command, by handler name.
    ///
    /// One table: a command behaves the same whether it was typed alone or as a stage of a
    /// pipeline, because a pipeline must not be a second way round anything (the PIN, the
    /// workspace limit, the refusals).
    fn shell_dispatch(&mut self, tool: &str, inner: &Value) -> std::result::Result<String, String> {
        match tool {
            "shell_zip" => self.shell_zip(inner),
            "shell_unzip" => self.shell_unzip(inner),
            "shell_curl" => self.run_curl("curl", inner),
            "shell_wget" => self.run_curl("wget", inner),
            "shell_ping" => self.run_ping(inner).map(|v| v.to_string()),
            "shell_ifconfig" => Ok(self.ifconfig_json().to_string()),
            "shell_df" => Ok(self.shell_df()),
            "shell_free" => Ok(self.shell_free()),
            "shell_uptime" => Ok(self.shell_uptime()),
            "shell_date" => Ok(self.shell_date()),
            "shell_uname" => Ok(self.shell_uname()),
            "shell_ps" => self.shell_ps(),
            "shell_hexdump" => self.shell_hexdump(inner),
            "shell_base64" => self.shell_base64(inner),
            "shell_sha256sum" => self.shell_hash(inner, true),
            "shell_md5sum" => self.shell_hash(inner, false),
            "shell_system_prompt" => self.shell_system_prompt(inner),
            "shell_jq" => self.shell_jq(inner),
            "shell_wc" => self.shell_wc(inner),
            "shell_head" => self.shell_head(inner),
            "shell_cut" => self.shell_cut(inner),
            "shell_sort" => self.shell_sort(inner),
            "shell_uniq" => self.shell_uniq(inner),
            "shell_tr" => self.shell_tr(inner),
            "shell_sed" => self.shell_sed(inner),
            "shell_awk" => self.shell_awk(inner),
            "shell_tee" => self.shell_tee(inner),
            "shell_touch" => self.shell_touch(inner),
            "shell_du" => Ok(self.shell_du()),
            "shell_strings" => self.shell_strings(inner),
            "shell_sleep" => self.shell_sleep(inner),
            "shell_jot" => self.shell_jot(inner),
            "shell_crc32" => self.shell_crc32(inner),
            "shell_hostname" => self.shell_hostname(inner),
            "shell_whoami" => Ok(self.shell_whoami()),
            "shell_id" => Ok(self.shell_id()),
            "shell_env" => self.shell_env(inner),
            "shell_netstat" => Ok(self.shell_netstat()),
            "shell_route" => Ok(self.shell_route()),
            "shell_arp" => Ok(self.shell_arp()),
            "shell_type" => self.shell_type(inner),
            "shell_echo" => self.shell_echo(inner),
            "shell_help" => Ok(json!({ "help": crate::tools::shell::help_text() }).to_string()),
            other => self.fs_or_net_call(other, inner),
        }
    }

    /// `cmd | cmd`, `cmd > file`, `cmd < file`: the stages run in order, each one through
    /// [`App::shell_dispatch`], and the text between them is what `stdout` means.
    ///
    /// The result is one JSON object rather than a stage's own answer: a pipeline has an
    /// answer of its own (what came out of the last stage, and what was written where).
    fn run_pipeline(
        &mut self,
        pipeline: &crate::tools::shell::Pipeline,
    ) -> std::result::Result<String, String> {
        let mut stdin: Option<String> = None;
        let mut stages: Vec<Value> = Vec::new();
        let mut writes: Vec<Value> = Vec::new();

        for (index, stage) in pipeline.stages.iter().enumerate() {
            let op = stage.parsed.name.clone();
            let stdio = crate::tools::shell::pipe_stdio(&op).ok_or_else(|| {
                format!(
                    "`{op}` cannot run in a pipeline: only the commands that read or write lines \
                     can be a stage\n\nthe stages are: {}\n\nusage: {}",
                    crate::tools::shell::pipe_commands(),
                    crate::tools::shell::PIPE_USAGE
                )
            })?;
            // `< file` on the first stage is where its text comes from; on a later stage the
            // pipe is already the input and two sources would be a guess.
            if let Some(path) = &stage.stdin {
                if index > 0 {
                    return Err(format!(
                        "`<` is only for the first stage: stage {} already reads the pipe\n\nusage: {}",
                        index + 1,
                        crate::tools::shell::PIPE_USAGE
                    ));
                }
                stdin = Some(fs_tools::Workspace::read(&self.workspace, path)?);
            }
            if index > 0 && stdio == crate::tools::shell::Stdio::Produces {
                return Err(format!(
                    "`{op}` does not read stdin, so it cannot be stage {} of a pipeline \
                     (it only produces output)\n\nusage: {}",
                    index + 1,
                    crate::tools::shell::PIPE_USAGE
                ));
            }

            let mut inner = match crate::tools::shell::stage_arguments(&stage.parsed, true) {
                Ok(inner) => inner,
                Err(e) => return Err(with_usage(&op, e)),
            };
            if let Some(text) = &stdin {
                inner["stdin"] = json!(text);
            }
            let out = self.pipe_stage(&op, &inner).map_err(|e| with_usage(&op, e))?;

            let mut record = json!({ "op": op, "bytes_out": out.len() });
            match &stage.stdout {
                Some(target) => {
                    let written = self.shell_redirect(target, &out)?;
                    record["file"] = written["path"].clone();
                    record["append"] = written["append"].clone();
                    writes.push(written);
                    // A redirected stage sends nothing down the pipe, as in a shell.
                    stdin = Some(String::new());
                }
                None => stdin = Some(out),
            }
            stages.push(record);
        }

        let bytes_out = stdin.as_ref().map(|text| text.len()).unwrap_or(0);
        let mut out = stdin.unwrap_or_default();
        let truncated = out.len() > PIPE_RESULT_MAX;
        if truncated {
            out = crate::llm::truncate_bytes(&out, PIPE_RESULT_MAX);
        }
        let mut result = json!({
            "pipeline": pipeline.stages.iter().map(|s| s.parsed.name.as_str()).collect::<Vec<_>>(),
            "stages": stages,
            "stdout": out,
            "bytes_out": bytes_out,
        });
        if !writes.is_empty() {
            result["writes"] = json!(writes);
        }
        if truncated {
            result["truncated"] = json!(true);
            result["note"] =
                json!(format!("stdout was cut to {PIPE_RESULT_MAX} bytes; a tool answer is capped"));
        }
        Ok(result.to_string())
    }

    /// One stage: the handler behind it, then its answer rendered as the text a pipe carries.
    ///
    /// `cat`, `grep` and `echo` are the three whose answer is not already a line shape: `cat`
    /// with no file copies its input, `grep` with no file filters what the pipe left, and
    /// `echo` is the producer a redirect is usually written for.
    fn pipe_stage(&mut self, op: &str, inner: &Value) -> std::result::Result<String, String> {
        if op == "cat" {
            match inner.get("path").and_then(|v| v.as_str()) {
                Some(path) if !path.is_empty() => {
                    // A named file wins over the pipe, as `cat a b` does over stdin.
                }
                _ => return Ok(inner.get("stdin").and_then(|v| v.as_str()).unwrap_or("").to_string()),
            }
        }
        if op == "grep" && inner.get("path").map(|v| v.is_null()).unwrap_or(true) {
            let pattern = inner.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let text = inner.get("stdin").and_then(|v| v.as_str()).unwrap_or("");
            // The same rule as `fs_grep`: a literal substring, as typed.
            let hits: Vec<&str> = text.lines().filter(|line| line.contains(pattern)).collect();
            return Ok(hits.join("\n"));
        }
        let tool = crate::tools::shell::route(op).ok_or_else(|| {
            format!("`{op}` is not a shell command.\n\n{}", crate::tools::shell::help_text())
        })?;
        let answer = self.shell_dispatch(tool, inner)?;
        let value: Value = serde_json::from_str(&answer).unwrap_or_else(|_| json!({ "text": answer }));
        pipe_text(op, &value)
    }

    /// `> file` / `>> file`: the text goes into the workspace, refused if it cannot fit.
    fn shell_redirect(
        &mut self,
        target: &crate::tools::shell::Redirect,
        text: &str,
    ) -> std::result::Result<Value, String> {
        let existing = if target.append {
            fs_tools::Workspace::read(&self.workspace, &target.path).unwrap_or_default()
        } else {
            String::new()
        };
        let joined = if target.append { format!("{existing}{text}") } else { text.to_string() };
        if joined.len() > WORKSPACE_FILE_MAX {
            return Err(format!(
                "a redirect needs {} bytes and a file holds at most {WORKSPACE_FILE_MAX}: nothing was written",
                joined.len()
            ));
        }
        let others: usize = self.workspace.to_json()["files"]
            .as_object()
            .map(|files| {
                files
                    .iter()
                    .filter(|(path, _)| {
                        path.as_str() != fs_tools::normalize(&target.path).unwrap_or_default()
                    })
                    .map(|(_, text)| text.as_str().unwrap_or("").len())
                    .sum()
            })
            .unwrap_or(0);
        if others + joined.len() > WORKSPACE_TOTAL_MAX {
            return Err(format!(
                "a redirect needs {} bytes and the workspace holds {WORKSPACE_TOTAL_MAX}: nothing was written",
                others + joined.len()
            ));
        }
        self.workspace_edit(|ws| fs_tools::Workspace::write(ws, &target.path, &joined))?;
        Ok(json!({ "path": target.path, "bytes": joined.len(), "append": target.append }))
    }

    fn run_ping(&mut self, args: &Value) -> std::result::Result<Value, String> {
        let host = args.get("host").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if host.is_empty() {
            return Err(with_usage("ping", "`host` is required".to_string()));
        }
        let count = args
            .get("count")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(3)
            .clamp(1, 5) as u32;
        match self.hw.ping(&host, count) {
            Ok(v) => Ok(v),
            Err(e) => Err(with_usage("ping", format!("ping: {}", e.message))),
        }
    }

    /// `curl`: GET (with HTML reduced to text) or POST when `data` is given.
    /// `wget`: the same fetch, saved into the workspace under `path`.
    fn run_curl(&mut self, name: &str, args: &Value) -> std::result::Result<String, String> {
        let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if url.is_empty() {
            return Err("`url` is required".into());
        }
        let data = args.get("data").and_then(|v| v.as_str());
        let mut inner = json!({ "url": url });
        if let Some(f) = args.get("format") {
            inner["format"] = f.clone();
        }
        let fetched = match data {
            Some(body) => {
                inner["body"] = json!(body);
                self.fs_or_net_call("net_post", &inner)?
            }
            None => self.fs_or_net_call("net_get", &inner)?,
        };
        if name == "curl" {
            return Ok(fetched);
        }
        // `wget`: the body goes into a file, the result reports where.
        let parsed: Value = serde_json::from_str(&fetched).unwrap_or_else(|_| json!({ "body": fetched }));
        let body = parsed.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string();
        if body.is_empty() {
            return Err("the response carried no body to save".into());
        }
        let path = args.get("path").and_then(|p| p.as_str()).map(|p| p.to_string()).unwrap_or_else(|| {
            let host = url.split("://").nth(1).unwrap_or(&url).split('/').next().unwrap_or("download");
            let tail = url.rsplit('/').next().unwrap_or("index");
            format!("/downloads/{host}-{tail}")
        });
        let text = crate::llm::truncate_bytes(&body, crate::tools::fs::MAX_FILE_BYTES);
        let truncated = text.len() < body.len();
        self.workspace_edit(|ws| fs_tools::Workspace::write(ws, &path, &text))
            .map_err(|e| format!("could not save `{path}`: {e}"))?;
        let mut out = json!({
            "path": path,
            "bytes": text.len(),
            "truncated": truncated,
            "url": url,
        });
        if truncated {
            out["note"] = json!("the file holds the first 8192 bytes of the response");
        }
        Ok(out.to_string())
    }

    /// `sha256sum` / `md5sum`: what is in this file, in a form another machine can check.
    fn shell_hash(&mut self, args: &Value, sha: bool) -> std::result::Result<String, String> {
        let (path, text) = self.shell_input(args, if sha { "sha256sum" } else { "md5sum" })?;
        let bytes = text.into_bytes();
        let digest = if sha { crate::hash::sha256(&bytes) } else { crate::hash::md5(&bytes) };
        let key = if sha { "sha256" } else { "md5" };
        let mut out = json!({ "path": path, "bytes": bytes.len() });
        out[key] = json!(digest);
        Ok(out.to_string())
    }

    /// `system_prompt get|set|append|clear`: the owner's standing orders, from the agent.
    fn shell_system_prompt(&mut self, args: &Value) -> std::result::Result<String, String> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("get").trim().to_ascii_lowercase();
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
        match action.as_str() {
            "get" => {}
            "set" => {
                self.cfg.agent.system_prompt = text.chars().take(crate::config::MAX_SYSTEM_PROMPT).collect();
                self.persist_config();
            }
            "append" => {
                let current = self.cfg.agent.system_prompt.trim_end().to_string();
                let joined = if current.is_empty() {
                    text.trim().to_string()
                } else {
                    format!("{current}\n{}", text.trim())
                };
                self.cfg.agent.system_prompt =
                    joined.chars().take(crate::config::MAX_SYSTEM_PROMPT).collect();
                self.persist_config();
            }
            "clear" => {
                self.cfg.agent.system_prompt.clear();
                self.persist_config();
            }
            other => {
                return Err(format!(
                "unknown action `{other}`\n\nusage: system_prompt get | set <text> | append <text> | clear"
            ))
            }
        }
        Ok(json!({
            "action": action,
            "system_prompt": self.cfg.agent.system_prompt,
            "bytes": self.cfg.agent.system_prompt.len(),
            "limit": crate::config::MAX_SYSTEM_PROMPT,
        })
        .to_string())
    }

    /// `df`: how much of the workspace is used.
    fn shell_df(&self) -> String {
        let json = self.workspace.to_json();
        let files = json["files"].as_object();
        let count = files.map(|f| f.len()).unwrap_or(0);
        let bytes: usize =
            files.map(|f| f.values().filter_map(|v| v.as_str()).map(|v| v.len()).sum()).unwrap_or(0);
        // What the store behind these files can take. It was three NVS numbers (a 3 500-byte
        // chunk, five of them) and that stopped describing anything when the workspace moved to
        // a filesystem: the ceiling is the partition's free space now, and it is the number a
        // refused write is about.
        let space = self.store.space();
        let (store_total, store_used) = match space {
            Some(store::StoreSpace::Bytes { total, used }) => (Some(total), Some(used)),
            Some(store::StoreSpace::Entries { total_bytes, used_bytes, .. }) => {
                (Some(total_bytes), Some(used_bytes))
            }
            None => (None, None),
        };
        serde_json::json!({
            "files": count,
            "bytes": bytes,
            "file_limit_bytes": crate::tools::fs::MAX_FILE_BYTES,
            "workspace_limit_bytes": WORKSPACE_TOTAL_MAX,
            "free_bytes": WORKSPACE_TOTAL_MAX.saturating_sub(bytes),
            "store_total_bytes": store_total,
            "store_used_bytes": store_used,
        })
        .to_string()
    }

    /// `free`: heap, split the way this board actually splits it.
    ///
    /// The totals come first because they are what `free` prints everywhere, but on a board
    /// with PSRAM the total is the wrong number for every question that matters: a stack, a
    /// TLS record and a socket all need *internal* bytes, and those can be nearly gone while
    /// the total still reads in the megabytes. The pools name which number answers which
    /// request, so the note this used to carry ("free_kb includes PSRAM") is not needed.
    fn shell_free(&mut self) -> String {
        let info = self.hw.board_info();
        let internal = self.hw.heap_internal();
        // One flat field per pool rather than a nested object: `free` prints the answer as
        // `key value` lines (that is what a `grep dma` downstream reads), and a nested map
        // would print as one line of JSON.
        let mut out = serde_json::Map::new();
        out.insert("heap_free_kb".into(), serde_json::json!(info.free_heap_kb));
        out.insert("heap_min_kb".into(), serde_json::json!(info.min_free_heap_kb));
        out.insert("internal_kb".into(), serde_json::json!(internal.map(|(free, _)| free / 1024)));
        out.insert(
            "largest_internal_kb".into(),
            serde_json::json!(internal.map(|(_, largest)| largest / 1024)),
        );
        for pool in self.hw.heap_pools() {
            let key = |what: &str| format!("{}_{}", pool.pool, what);
            out.insert(key("total_kb"), serde_json::json!(pool.total_kb));
            out.insert(key("free_kb"), serde_json::json!(pool.free_kb));
            out.insert(key("largest_kb"), serde_json::json!(pool.largest_kb));
            out.insert(key("min_free_kb"), serde_json::json!(pool.min_free_kb));
        }
        serde_json::Value::Object(out).to_string()
    }

    fn shell_uptime(&mut self) -> String {
        let up = self.runtime.uptime_s(now_ms());
        serde_json::json!({
            "uptime_s": up,
            "uptime": format!("{}h {:02}m {:02}s", up / 3600, (up % 3600) / 60, up % 60),
            "runs": self.agent.runs,
        })
        .to_string()
    }

    fn shell_date(&mut self) -> String {
        let ms = now_ms();
        serde_json::json!({
            "unix_s": ms / 1000,
            "synced": ms > 1_700_000_000_000,
            "utc": utc_string(ms / 1000),
            "note": "before SNTP syncs this is time since boot, and https certificates read as not yet valid",
        })
        .to_string()
    }

    fn shell_uname(&mut self) -> String {
        let info = self.hw.board_info();
        serde_json::json!({
            "firmware": format!("esp32-agent {}", self.firmware.version),
            "build": self.firmware.build,
            "soc": info.soc,
            "cores": info.cores,
            "revision": info.revision,
            "flash_mb": info.flash_mb,
            "psram_kb": info.psram_kb,
            "idf": info.idf_version,
            "mac": info.mac,
            "board": self.profile.id,
        })
        .to_string()
    }

    /// `ps`: tasks with their state and remaining stack.
    fn shell_ps(&mut self) -> std::result::Result<String, String> {
        let tasks = self.hw.tasks();
        if tasks.is_empty() {
            return Err("this platform cannot enumerate tasks".into());
        }
        Ok(serde_json::json!({ "count": tasks.len(), "tasks": tasks }).to_string())
    }

    fn shell_hexdump(&mut self, args: &Value) -> std::result::Result<String, String> {
        let width = args.get("width").and_then(|v| v.as_u64()).unwrap_or(16).clamp(4, 32) as usize;
        // `od` and `hexdump` are the same walk over the same bytes; the base is the only
        // difference, and `od` is the one that prints octal.
        let octal = args.get("octal").and_then(|v| v.as_bool()).unwrap_or(false);
        let (path, text) = self.shell_input(args, if octal { "od" } else { "hexdump" })?;
        let data = text.into_bytes();
        // A tool result is 2 KB; a column per byte eats 4 (octal) or 5 (hex) bytes, so dump
        // as much as arrives whole and say what was left out.
        let lines_that_fit = 1800 / (width * if octal { 4 } else { 5 } + 12);
        let mut out = String::new();
        for (row, chunk) in data.chunks(width).enumerate() {
            if row >= lines_that_fit {
                out.push_str(&format!("… {} more bytes\n", data.len() - row * width));
                break;
            }
            if octal {
                out.push_str(&format!("{:07o}  ", row * width));
                for byte in chunk {
                    out.push_str(&format!("{byte:03o} "));
                }
            } else {
                out.push_str(&format!("{:04x}  ", row * width));
                for byte in chunk {
                    out.push_str(&format!("{byte:02x} "));
                }
            }
            out.push_str(" |");
            for byte in chunk {
                out.push(if (32..127).contains(byte) { *byte as char } else { '.' });
            }
            out.push_str("|\n");
        }
        Ok(serde_json::json!({ "path": path, "bytes": data.len(), "octal": octal, "hexdump": out })
            .to_string())
    }

    /// `base64`: encode a file to base64, or decode base64 back into one.
    fn shell_base64(&mut self, args: &Value) -> std::result::Result<String, String> {
        let decode = args.get("decode").and_then(|v| v.as_bool()).unwrap_or(false);
        let out_path = args.get("out").and_then(|v| v.as_str()).map(|s| s.to_string());
        let (path, text) = self.shell_input(args, if decode { "base64dec" } else { "base64enc" })?;
        if decode {
            let mut bytes = Vec::new();
            crate::util::b64_decode(text.trim(), &mut bytes).map_err(|e| e.message)?;
            let decoded = String::from_utf8_lossy(&bytes).to_string();
            // `-o` is where it goes; a file that was named gets its `.decoded` neighbour; text
            // arriving on a pipe has neither, and the decoded text is the answer itself.
            let target = out_path.or_else(|| path.as_str().map(|p| format!("{p}.decoded")));
            let Some(target) = target else {
                return Ok(serde_json::json!({ "text": decoded, "bytes": bytes.len() }).to_string());
            };
            self.workspace_edit(|ws| fs_tools::Workspace::write(ws, &target, &decoded))?;
            return Ok(serde_json::json!({ "path": target, "bytes": bytes.len() }).to_string());
        }
        let encoded = crate::util::b64_encode(text.as_bytes());
        if let Some(target) = out_path {
            self.workspace_edit(|ws| fs_tools::Workspace::write(ws, &target, &encoded))?;
            return Ok(serde_json::json!({ "path": target, "bytes": encoded.len() }).to_string());
        }
        Ok(serde_json::json!({ "path": path, "base64": encoded }).to_string())
    }

    /// `ifconfig`: what the board thinks its network is, including the overrides.
    fn ifconfig_json(&mut self) -> Value {
        let wifi = self.hw.wifi_status();
        let info = self.hw.board_info();
        json!({
            "interface": "wlan0",
            "state": wifi.state,
            "ip": wifi.ip,
            "netmask": self.cfg.wifi.netmask,
            "gateway": self.cfg.wifi.gateway,
            "dns": [self.cfg.wifi.dns, self.cfg.wifi.dns2],
            "ssid": wifi.ssid,
            "rssi_dbm": wifi.rssi_dbm,
            "mac": info.mac,
            "hostname": self.cfg.wifi.hostname,
            "ap": { "ssid": wifi.ap_ssid, "ip": wifi.ap_ip },
            "note": "gateway and dns show the configured overrides; empty means the DHCP lease",
        })
    }

    /// Pack `{name: text}` entries into an archive and return it as base64.
    ///
    /// Base64 rather than a file in the workspace: the workspace holds text and totals 8 kB,
    /// and a tool result is capped at 2 KB anyway. An external agent can decode the string
    /// and write a real `.zip`; the board itself only needs to hand it over.
    fn shell_zip(&mut self, args: &Value) -> std::result::Result<String, String> {
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("archive.zip");
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        if let Some(files) = args.get("files").and_then(|v| v.as_object()) {
            for (path, text) in files {
                entries.push((path.clone(), text.as_str().unwrap_or("").as_bytes().to_vec()));
            }
        }
        // `paths` packs files that are already in the workspace.
        if let Some(paths) = args.get("paths").and_then(|v| v.as_array()) {
            for path in paths.iter().filter_map(|p| p.as_str()) {
                match fs_tools::Workspace::read(&self.workspace, path) {
                    Ok(text) => entries.push((path.trim_start_matches('/').to_string(), text.into_bytes())),
                    Err(_) => return Err(format!("`{path}` is not in the workspace")),
                }
            }
        }
        if entries.is_empty() {
            return Err("nothing to pack: give `files` {name: text} or `paths` [..]".into());
        }
        let archive = crate::zip::write(&entries)?;
        if archive.len() > 1400 {
            return Err(format!(
                "the archive would be {} bytes; a tool result is limited to about 1.4 kB of base64",
                archive.len()
            ));
        }
        Ok(json!({
            "name": name,
            "bytes": archive.len(),
            "entries": entries.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "stored": true,
            "archive_b64": crate::util::b64_encode(&archive),
        })
        .to_string())
    }

    fn shell_unzip(&mut self, args: &Value) -> std::result::Result<String, String> {
        let encoded = args.get("archive_b64").and_then(|v| v.as_str()).unwrap_or("");
        if encoded.is_empty() {
            return Err("`archive_b64` is required".into());
        }
        let mut bytes = Vec::new();
        crate::util::b64_decode(encoded, &mut bytes).map_err(|e| format!("base64: {}", e.message))?;
        let entries = crate::zip::read(&bytes)?;
        let mut out = Vec::new();
        let mut budget = 1800usize;
        for (name, data) in entries {
            let text = String::from_utf8_lossy(&data).to_string();
            let clipped = text.chars().take(budget.min(600)).collect::<String>();
            budget = budget.saturating_sub(clipped.len());
            out.push(json!({
                "name": name,
                "bytes": data.len(),
                "text": clipped,
                "binary": !data.iter().all(|b| *b == b'\t' || *b == b'\n' || *b == b'\r' || (32..127).contains(b)),
            }));
        }
        Ok(json!({ "entries": out, "count": out.len() }).to_string())
    }

    // ------------------------------------------------- the text filters, one file at a time

    // A pipe would carry the text from one command to the next; this board has none, so
    // every filter below opens a file and every one of them is one call. That is the whole
    // reason these exist separately from `grep`: the model can still ask "how many lines",
    // it just has to name the file it is asking about.

    /// The `path` argument, or the error that names the command that wanted it.
    fn shell_path(args: &Value, op: &str) -> std::result::Result<String, String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if path.is_empty() {
            return Err(format!("{op}: `path` is required"));
        }
        Ok(path)
    }

    /// The text a filter works on: the file it names, or the text a pipe handed it.
    ///
    /// A named file wins over the pipe, the way `sort a.txt` ignores stdin in a shell. With
    /// neither there is nothing to read, and that is an error rather than an empty answer:
    /// a filter that silently returns nothing looks like a filter that found nothing.
    fn shell_input(&self, args: &Value, op: &str) -> std::result::Result<(Value, String), String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if !path.is_empty() {
            let text = fs_tools::Workspace::read(&self.workspace, &path)?;
            return Ok((json!(path), text));
        }
        if let Some(stdin) = args.get("stdin").and_then(|v| v.as_str()) {
            return Ok((Value::Null, stdin.to_string()));
        }
        Err(format!("{op}: nothing to read — give it a path, or feed it with `< file` or a pipe"))
    }

    /// `echo [-n] <text...>`: the text, and the newline a redirect is usually written for.
    fn shell_echo(&mut self, args: &Value) -> std::result::Result<String, String> {
        let words: Vec<&str> = args
            .get("words")
            .and_then(|v| v.as_array())
            .map(|items| items.iter().filter_map(|word| word.as_str()).collect())
            .unwrap_or_default();
        let text = words.join(" ");
        let newline = args.get("newline").and_then(|v| v.as_bool()).unwrap_or(true);
        let stdout = if newline { format!("{text}\n") } else { text.clone() };
        Ok(json!({
            "text": text,
            "stdout": stdout,
            "bytes": text.len(),
            "newline": newline,
        })
        .to_string())
    }

    /// `wc [-l|-w|-c] <path>`: lines, words or bytes, whichever was asked for.
    fn shell_wc(&mut self, args: &Value) -> std::result::Result<String, String> {
        let (path, text) = self.shell_input(args, "wc")?;
        let lines = text.lines().count();
        let words = text.split_whitespace().count();
        let bytes = text.len();
        let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = json!({ "path": path });
        match mode {
            // One flag prints one number, the way `wc -l` does.
            "lines" | "words" | "bytes" => {
                let count = match mode {
                    "lines" => lines,
                    "words" => words,
                    _ => bytes,
                };
                out["count"] = json!(count);
                out["unit"] = json!(mode);
            }
            // No flag prints all three, which is what plain `wc` answers.
            _ => {
                out["lines"] = json!(lines);
                out["words"] = json!(words);
                out["bytes"] = json!(bytes);
            }
        }
        Ok(out.to_string())
    }

    /// `head` / `tail [-n N] <path>`: the first or last `N` lines, default 10.
    fn shell_head(&mut self, args: &Value) -> std::result::Result<String, String> {
        let tail = args.get("tail").and_then(|v| v.as_bool()).unwrap_or(false);
        let (path, text) = self.shell_input(args, if tail { "tail" } else { "head" })?;
        let want = args
            .get("lines")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
            .unwrap_or(10)
            .clamp(1, 200) as usize;
        let all: Vec<&str> = text.lines().collect();
        let start = if tail { all.len().saturating_sub(want) } else { 0 };
        let end = if tail { all.len() } else { want.min(all.len()) };
        let picked = &all[start..end];
        Ok(json!({
            "path": path,
            "tail": tail,
            "lines": picked.len(),
            "total_lines": all.len(),
            "truncated": picked.len() < all.len(),
            "text": picked.join("\n"),
        })
        .to_string())
    }

    /// `cut -d <delimiter> -f <field> <path>`: one column out of every line.
    fn shell_cut(&mut self, args: &Value) -> std::result::Result<String, String> {
        let delimiter = args.get("delimiter").and_then(|v| v.as_str()).unwrap_or("\t").to_string();
        let mut chars = delimiter.chars();
        let delim = chars.next().unwrap_or('\t');
        if chars.next().is_some() {
            return Err("cut: the delimiter is one character".into());
        }
        let field = args
            .get("field")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
            .unwrap_or(1) as usize;
        if field == 0 {
            return Err("cut: fields are numbered from 1".into());
        }
        let (path, text) = self.shell_input(args, "cut")?;
        let mut out: Vec<&str> = Vec::new();
        let mut missing = 0usize;
        for line in text.lines() {
            match line.split(delim).nth(field - 1) {
                Some(value) => out.push(value),
                None => missing += 1,
            }
        }
        Ok(json!({
            "path": path,
            "delimiter": delim.to_string(),
            "field": field,
            "lines": out.len(),
            "lines_without_that_field": missing,
            "text": out.join("\n"),
        })
        .to_string())
    }

    /// `sort [-r] [-u] <path>`: byte order, like `LC_ALL=C sort`.
    fn shell_sort(&mut self, args: &Value) -> std::result::Result<String, String> {
        let reverse = args.get("reverse").and_then(|v| v.as_bool()).unwrap_or(false);
        let unique = args.get("unique").and_then(|v| v.as_bool()).unwrap_or(false);
        let (path, text) = self.shell_input(args, "sort")?;
        let mut lines: Vec<&str> = text.lines().collect();
        lines.sort_unstable();
        if reverse {
            lines.reverse();
        }
        let before = lines.len();
        if unique {
            lines.dedup();
        }
        Ok(json!({
            "path": path,
            "lines": lines.len(),
            "duplicates_removed": before - lines.len(),
            "text": lines.join("\n"),
        })
        .to_string())
    }

    /// `sed 's/from/to/[g]' [path]`: one substitution, applied line by line.
    fn shell_sed(&mut self, args: &Value) -> std::result::Result<String, String> {
        let script = args.get("script").and_then(|v| v.as_str()).unwrap_or("");
        let (from, to, global) = crate::tools::shell::parse_substitute(script)?;
        let (path, text) = self.shell_input(args, "sed")?;
        let mut lines: Vec<String> = Vec::with_capacity(text.lines().count());
        let mut changed = 0usize;
        let mut replacements = 0usize;
        for line in text.lines() {
            let hits = line.matches(from.as_str()).count();
            if hits > 0 {
                changed += 1;
                replacements += if global { hits } else { 1 };
            }
            lines.push(if global {
                line.replace(from.as_str(), &to)
            } else {
                line.replacen(from.as_str(), &to, 1)
            });
        }
        Ok(json!({
            "path": path,
            "lines": lines.len(),
            "changed_lines": changed,
            "replacements": replacements,
            "text": lines.join("\n"),
        })
        .to_string())
    }

    /// `awk '{print $1, $NF}' [path]`: the field-printing subset.
    ///
    /// Fields are whitespace-separated unless `-F` gives a separator. Nothing else is
    /// implemented — no patterns, no arithmetic, no `BEGIN` — because the board has `script`
    /// for that, and a half-`awk` would be the worse tool of the two.
    fn shell_awk(&mut self, args: &Value) -> std::result::Result<String, String> {
        use crate::tools::shell::PrintItem;
        let program = args.get("program").and_then(|v| v.as_str()).unwrap_or("");
        let items = crate::tools::shell::parse_print_program(program)?;
        let sep = args.get("sep").and_then(|v| v.as_str()).unwrap_or("");
        let (path, text) = self.shell_input(args, "awk")?;
        let mut out: Vec<String> = Vec::with_capacity(text.lines().count());
        for line in text.lines() {
            let cols: Vec<&str> =
                if sep.is_empty() { line.split_whitespace().collect() } else { line.split(sep).collect() };
            let mut parts: Vec<&str> = Vec::with_capacity(items.len());
            for item in &items {
                parts.push(match item {
                    PrintItem::Line => line,
                    PrintItem::Last => cols.last().copied().unwrap_or(""),
                    PrintItem::Field(n) => cols.get(n - 1).copied().unwrap_or(""),
                });
            }
            out.push(if sep.is_empty() { parts.join(" ") } else { parts.join(sep) });
        }
        Ok(json!({
            "path": path,
            "lines": out.len(),
            "text": out.join("\n"),
        })
        .to_string())
    }

    /// `uniq <path>`: collapse *adjacent* repeats, which is what `uniq` means.
    fn shell_uniq(&mut self, args: &Value) -> std::result::Result<String, String> {
        let (path, text) = self.shell_input(args, "uniq")?;
        let mut lines: Vec<&str> = Vec::new();
        for line in text.lines() {
            if lines.last() != Some(&line) {
                lines.push(line);
            }
        }
        Ok(json!({
            "path": path,
            "lines": lines.len(),
            "removed": text.lines().count().saturating_sub(lines.len()),
            "text": lines.join("\n"),
            "note": "adjacent repeats only, as uniq does; pipe it through `sort -u` to collapse all",
        })
        .to_string())
    }

    /// `tr <from> <to> <path>`: character for character, literal characters only.
    fn shell_tr(&mut self, args: &Value) -> std::result::Result<String, String> {
        let from = args.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("");
        if from.is_empty() {
            return Err("tr: `from` is required".into());
        }
        if to.is_empty() {
            return Err("tr: `to` is required (deleting a set is not implemented)".into());
        }
        let (path, text) = self.shell_input(args, "tr")?;
        // `tr` repeats the last character of SET2 when SET2 is shorter; ranges are *not*
        // expanded, so `a-z` is three literal characters and the result says so.
        let mut map: std::collections::BTreeMap<char, char> = std::collections::BTreeMap::new();
        let mut replacement = to.chars();
        let mut last = to.chars().last().unwrap_or(' ');
        for c in from.chars() {
            if let Some(r) = replacement.next() {
                last = r;
            }
            map.insert(c, last);
        }
        let mut out = String::with_capacity(text.len());
        let mut changed = 0usize;
        for c in text.chars() {
            match map.get(&c) {
                Some(r) => {
                    if *r != c {
                        changed += 1;
                    }
                    out.push(*r);
                }
                None => out.push(c),
            }
        }
        Ok(json!({
            "path": path,
            "from": from,
            "to": to,
            "chars_changed": changed,
            "text": out,
            "note": "literal characters only: `a-z` is three characters here, not a range",
        })
        .to_string())
    }

    /// `tee <path> [-i <text>]`: write the text and echo it. In a pipeline the text arrives
    /// on stdin; on its own it arrives as `-i`, because that is what a pipe would have been.
    fn shell_tee(&mut self, args: &Value) -> std::result::Result<String, String> {
        let path = App::shell_path(args, "tee")?;
        let flag = args.get("input").and_then(|v| v.as_str()).unwrap_or("");
        let piped = args.get("stdin").and_then(|v| v.as_str());
        let input = match piped {
            Some(text) => text,
            None => flag,
        };
        if input.is_empty() {
            return Err(
                "tee: nothing to write — in a pipeline the pipe fills it, and on its own `-i <text>` does"
                    .into(),
            );
        }
        self.workspace_edit(|ws| fs_tools::Workspace::write(ws, &path, input))?;
        Ok(json!({
            "path": path,
            "bytes": input.len(),
            "text": input,
            "source": if piped.is_some() { "stdin" } else { "-i" },
            "note": "tee: the text is both written to the file and echoed to the next stage",
        })
        .to_string())
    }

    /// `touch <path>`: create an empty file. There is no mtime to move, and it says so.
    fn shell_touch(&mut self, args: &Value) -> std::result::Result<String, String> {
        let path = App::shell_path(args, "touch")?;
        let exists = fs_tools::Workspace::read(&self.workspace, &path).is_ok();
        if !exists {
            self.workspace_edit(|ws| fs_tools::Workspace::write(ws, &path, ""))?;
        }
        Ok(json!({
            "path": path,
            "created": !exists,
            "exists": true,
            "mtime": Value::Null,
            "note": if exists {
                "the file is already there; the workspace stores text and no timestamps, so there is no mtime to update"
            } else {
                "created empty; the workspace keeps no timestamps"
            },
        })
        .to_string())
    }

    /// `du`: the workspace, file by file, biggest first.
    /// `jq [query] <path>`, or a query over a pipe's input.
    ///
    /// `-r` prints strings without their quotes, which is what a `for` loop or a `grep`
    /// downstream wants.
    fn shell_jq(&mut self, args: &Value) -> std::result::Result<String, String> {
        let raw = args.get("r").and_then(|v| v.as_bool()).unwrap_or(false);
        let query = args
            .get("positional")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap_or_default();
        // `jq .` reads the file; `jq .hits[]` with a pipe reads stdin. The query is what does
        // not exist as a file, so it is decided by looking at the first word.
        let (query_text, _) = match query.len() {
            0 => (".".to_string(), ()),
            1 => (query[0].clone(), ()),
            _ => (query[0].clone(), ()),
        };
        let (_path, text) = self.shell_input(args, "jq")?;
        crate::tools::shell::jq(&text, &query_text, raw)
    }

    fn shell_du(&self) -> String {
        let json = self.workspace.to_json();
        let mut files: Vec<(String, usize)> = json["files"]
            .as_object()
            .map(|map| {
                map.iter().map(|(path, text)| (path.clone(), text.as_str().unwrap_or("").len())).collect()
            })
            .unwrap_or_default();
        files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let total: usize = files.iter().map(|(_, bytes)| bytes).sum();
        json!({
            "count": files.len(),
            "bytes": total,
            "limit_bytes": WORKSPACE_TOTAL_MAX,
            "files": files.iter().map(|(path, bytes)| json!({ "path": path, "bytes": bytes })).collect::<Vec<_>>(),
            "note": "one storage area: there are no directories to total separately",
        })
        .to_string()
    }

    /// `strings [-n <min>] <path>`: the printable runs, shortest useful one first.
    fn shell_strings(&mut self, args: &Value) -> std::result::Result<String, String> {
        let min = args
            .get("min")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
            .unwrap_or(4)
            .clamp(2, 64) as usize;
        let (path, text) = self.shell_input(args, "strings")?;
        let data = text.into_bytes();
        let mut found: Vec<String> = Vec::new();
        let mut run = String::new();
        let mut budget = 1600usize;
        // A NUL past the end closes a run that reaches the last byte.
        for byte in data.iter().chain(std::iter::once(&0u8)) {
            if (0x20..0x7f).contains(byte) || *byte == b'\t' {
                run.push(*byte as char);
                continue;
            }
            if run.len() >= min && budget >= run.len() {
                budget -= run.len();
                found.push(std::mem::take(&mut run));
            } else {
                run.clear();
            }
        }
        let mut out = json!({
            "path": path,
            "min": min,
            "bytes": data.len(),
            "strings": found,
        });
        if found.is_empty() {
            out["note"] = json!(format!("no printable run of {min} or more bytes"));
        }
        Ok(out.to_string())
    }

    /// `sleep <s>` / `usleep <ms>`: a real wait, capped, because it blocks the one loop.
    fn shell_sleep(&mut self, args: &Value) -> std::result::Result<String, String> {
        let ms_unit = args.get("unit").and_then(|v| v.as_str()) == Some("ms");
        let op = if ms_unit { "usleep" } else { "sleep" };
        let raw = args.get("value").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let (wanted, ms) = plan_sleep(ms_unit, &raw).map_err(|e| format!("{op}: {e}"))?;
        std::thread::sleep(Duration::from_millis(ms));
        Ok(json!({
            "slept_ms": ms,
            "requested_ms": wanted,
            "clamped": ms < wanted,
            "note": "one loop serves the endpoint, the portal and the agent: the wait is capped at 10 s so it cannot look like a hang",
        })
        .to_string())
    }

    /// `jot <count> [start [step]]`: a sequence, because the shell has no `for`.
    fn shell_jot(&mut self, args: &Value) -> std::result::Result<String, String> {
        let count = arg_number(args, "count").ok_or("jot: `count` must be a number")?;
        let start = arg_number(args, "start").unwrap_or(1.0);
        let step = arg_number(args, "step").unwrap_or(1.0);
        let count = (count.round() as i64).clamp(1, 100);
        let numbers: Vec<String> =
            (0..count).map(|i| plain_number(start + step * i as f64)).collect::<Vec<_>>();
        Ok(json!({
            "count": count,
            "start": plain_number(start),
            "step": plain_number(step),
            "numbers": numbers,
            "text": numbers.join(" "),
        })
        .to_string())
    }

    /// `crc32 <path>`: the same CRC-32 the zip writer already uses, so there is one of it.
    fn shell_crc32(&mut self, args: &Value) -> std::result::Result<String, String> {
        let path = App::shell_path(args, "crc32")?;
        let bytes = fs_tools::Workspace::read(&self.workspace, &path)?.into_bytes();
        let crc = crate::zlib::crc32(&bytes);
        Ok(json!({
            "path": path,
            "bytes": bytes.len(),
            "crc32": format!("{crc:08x}"),
            "note": "CRC-32 (IEEE), the checksum a zip entry carries",
        })
        .to_string())
    }

    /// `hostname [name]`: the name the network sees, read from or written to the config.
    fn shell_hostname(&mut self, args: &Value) -> std::result::Result<String, String> {
        let wanted = args.get("name").and_then(|v| v.as_str()).map(|s| s.trim().to_string());
        match wanted {
            Some(name) if !name.is_empty() => {
                if name.len() > 32 {
                    return Err("hostname: at most 32 characters".into());
                }
                if name.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '.')) {
                    return Err("hostname: letters, digits, `-` and `.` only".into());
                }
                let previous = self.cfg.wifi.hostname.clone();
                self.cfg.wifi.hostname = name.clone();
                self.persist_config();
                Ok(json!({
                    "hostname": name,
                    "previous": previous,
                    "saved": true,
                    "note": "saved to the config; the DHCP lease carries the new name after the next reconnect",
                })
                .to_string())
            }
            _ => Ok(json!({
                "hostname": self.cfg.wifi.hostname,
                "device_name": self.cfg.device_name,
                "note": "this is the interface name; `whoami` answers with the device name",
            })
            .to_string()),
        }
    }

    /// `whoami`: the device name, because the board has one account and no login.
    fn shell_whoami(&self) -> String {
        json!({
            "whoami": self.cfg.device_name,
            "note": "one account and no login: this is the name the portal and MCP `initialize` show",
        })
        .to_string()
    }

    /// `id`: uid 0 / gid 0, and nothing more to say than that.
    fn shell_id(&self) -> String {
        json!({
            "uid": 0,
            "gid": 0,
            "user": "root",
            "groups": [0],
            "note": "one user, no passwords and no permission bits: every tool runs as uid 0",
            "device_name": self.cfg.device_name,
        })
        .to_string()
    }

    /// `env` / `setenv` / `unsetenv`: variables for this `App`, in RAM, nowhere else.
    fn shell_env(&mut self, args: &Value) -> std::result::Result<String, String> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("list");
        match action {
            "list" => Ok(json!({
                "count": self.shell_env.len(),
                "vars": self.shell_env,
                "persisted": false,
                "note": "session variables: they live in RAM until the next boot and are never written to NVS",
            })
            .to_string()),
            "set" => {
                let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if key.is_empty() {
                    return Err("setenv: `key` is required".into());
                }
                if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err("setenv: a key is letters, digits and `_`".into());
                }
                let previous = self.shell_env.insert(key.clone(), value.clone());
                Ok(json!({
                    "key": key,
                    "value": value,
                    "previous": previous,
                    "persisted": false,
                    "note": "this session only: nothing about it reaches NVS, and the config is untouched",
                })
                .to_string())
            }
            "unset" => {
                let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                if key.is_empty() {
                    return Err("unsetenv: `key` is required".into());
                }
                let existed = self.shell_env.remove(&key);
                Ok(json!({ "key": key, "removed": existed.is_some(), "persisted": false }).to_string())
            }
            other => Err(format!("env: unknown action `{other}`")),
        }
    }

    /// `netstat`: the sessions and counters this build keeps, and no invented socket table.
    fn shell_netstat(&self) -> String {
        let sessions: Vec<Value> = self
            .runtime
            .sessions
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "client": s.client,
                    "protocol": s.protocol,
                    "initialized": s.initialized,
                    "requests": s.requests,
                    "age_s": now_ms().saturating_sub(s.created_ms) / 1000,
                    "idle_s": now_ms().saturating_sub(s.last_seen_ms) / 1000,
                })
            })
            .collect();
        json!({
            "listen": {
                "enabled": self.cfg.mcp.enabled,
                "port": self.cfg.mcp.port,
                "path": self.cfg.mcp.path,
                "max_sessions": self.cfg.mcp.max_sessions,
            },
            "sessions": sessions,
            "session_count": self.runtime.sessions.len(),
            "counters": self.runtime.counters_json(),
            "sockets": Vec::<Value>::new(),
            "note": "this build does not expose lwIP's socket table, so open connections cannot be listed: sessions and counters are what there is",
        })
        .to_string()
    }

    /// `route`: the configured gateway, which is the one thing that answers "is my override on".
    fn shell_route(&mut self) -> String {
        let wifi = self.hw.wifi_status();
        let gateway = self.cfg.wifi.gateway.trim().to_string();
        json!({
            "destination": "default",
            "gateway": if gateway.is_empty() { Value::Null } else { json!(gateway) },
            "source": if gateway.is_empty() { "DHCP lease" } else { "configured" },
            "interface": "wlan0",
            "state": wifi.state,
            "ip": wifi.ip,
            "netmask": self.cfg.wifi.netmask,
            "dns": [self.cfg.wifi.dns, self.cfg.wifi.dns2],
            "note": "one interface and one default route, so this is not a table: an empty gateway means the lease's",
        })
        .to_string()
    }

    /// `arp`: an empty table and the reason, rather than an invented neighbour list.
    fn shell_arp(&self) -> String {
        json!({
            "entries": Vec::<Value>::new(),
            "count": 0,
            "available": false,
            "note": "lwIP's ARP table is not reachable from ESP-IDF on this build, so neighbours cannot be listed — this is empty because the board cannot see them, not because there are none",
        })
        .to_string()
    }

    /// `type` / `which <name>`: is this a shell command, a tool, or nothing?
    fn shell_type(&mut self, args: &Value) -> std::result::Result<String, String> {
        let wanted = args.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_ascii_lowercase();
        if wanted.is_empty() {
            return Err("type: `name` is required".into());
        }
        let which = args.get("which").and_then(|v| v.as_bool()).unwrap_or(false);
        let command = crate::tools::shell::route(&wanted);
        let registered = self.tools.get(&wanted).map(|def| def.spec.clone());
        // The tools `call_tool` dispatches by name instead of through the registry: they
        // need the app's own state (the workspace, the attachments, the model), so a
        // registry lookup alone would call `read` nothing.
        const STANDALONE: &[&str] = &["read", "write", "edit", "image_read", "script"];
        let standalone = STANDALONE.contains(&wanted.as_str());
        let is_tool = registered.is_some() || standalone;
        let kind = match (command.is_some(), is_tool) {
            (true, true) => "shell command and tool",
            (true, false) => "shell command",
            (false, true) => "tool",
            (false, false) => "nothing",
        };
        let mut out = json!({
            "name": wanted,
            "kind": kind,
            "shell_command": command.is_some(),
            "tool": is_tool,
            "registered": registered.is_some(),
            "looked_in": "the shell command table, the tool registry and the tools dispatched by name (there is no PATH on this board)",
        });
        if let Some(handler) = command {
            out["handler"] = json!(handler);
            // `which` says where it is; `type` says how to write it.
            if !which {
                out["usage"] = json!(crate::tools::shell::usage(&wanted).unwrap_or(""));
            }
        }
        if let Some(spec) = registered {
            out["title"] = json!(spec.title);
            out["category"] = json!(spec.category);
            out["description"] = json!(spec.description);
        }
        if standalone {
            out["dispatched"] = json!("by name in call_tool, because it needs the app's own state");
        }
        if kind == "nothing" {
            out["note"] =
                json!("no shell command and no tool of that name; `help` lists the commands, `tools_list` the tools");
        }
        Ok(out.to_string())
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC, from a unix time. No library: the board only needs this
/// for `date`, and a civil-time conversion is twenty lines.
fn utc_string(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (hour, minute, second) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02} {hour:02}:{minute:02}:{second:02}")
}

/// Put the command's usage line under an error, once.
///
/// A model that passed the wrong argument reads the correction where it happened; making
/// it call `--help` afterwards costs a whole turn to learn one line.
fn with_usage(name: &str, error: String) -> String {
    match crate::tools::shell::usage(name) {
        Some(line) if !error.contains("usage:") => format!("{error}\n\nusage: {line}"),
        _ => error,
    }
}

/// `sleep`/`usleep`: how long was asked for, and how long this board will wait.
///
/// Split out of the handler so the cap can be tested without a test that takes ten
/// seconds: the waiting is the one thing a test should not do to prove arithmetic.
fn plan_sleep(ms_unit: bool, raw: &str) -> std::result::Result<(u64, u64), String> {
    let value: f64 = raw.parse().map_err(|_| format!("`{raw}` is not a number"))?;
    if !value.is_finite() {
        return Err(format!("`{raw}` is not a number"));
    }
    if value < 0.0 {
        return Err("a negative wait is not a wait".to_string());
    }
    let wanted = (if ms_unit { value } else { value * 1000.0 }).round() as u64;
    Ok((wanted, wanted.min(MAX_SHELL_SLEEP_MS)))
}

/// A number argument, whether the command line handed it over as text or as a number.
fn arg_number(args: &Value, key: &str) -> Option<f64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `3` rather than `3.0`, and `1.5` rather than `1.5000000000000002`:
fn plain_number(value: f64) -> String {
    if (value - value.round()).abs() < 1e-9 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value}")
    }
}

/// The text a stage hands to the next one.
///
/// A command answers with JSON because a tool result is JSON, but a pipe carries *lines* —
/// that is what `grep`, `wc` and `sort` count downstream. This is the one place the two
/// meet, per command, and it is deliberately thin: what `ls` prints here is its file names,
/// one per line, not its whole answer with the sizes in it.
fn pipe_text(op: &str, answer: &Value) -> std::result::Result<String, String> {
    let text = |key: &str| answer.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let lines_of = |key: &str, field: Option<&str>| -> String {
        answer
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .map(|item| match field {
                        Some(name) => item[name].as_str().unwrap_or("").to_string(),
                        None => match item {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    };
    Ok(match op {
        // The filters already answer with the text they filtered.
        "cat" | "tee" | "sort" | "uniq" | "tr" | "cut" | "head" | "tail" => text("text"),
        "grep" => lines_of("matches", Some("text")),
        "wc" => match answer.get("count") {
            Some(count) => count.to_string(),
            // No mode: all three counts, as plain `wc` prints them.
            None => format!("{} {} {}", answer["lines"], answer["words"], answer["bytes"]),
        },
        "strings" => lines_of("strings", None),
        "od" | "xd" | "hexdump" => text("hexdump"),
        "base64enc" => text("base64"),
        "base64dec" => text("text"),
        "sha256sum" => text("sha256"),
        "md5sum" => text("md5"),
        "crc32" => text("crc32"),
        "jot" => lines_of("numbers", None),
        "ls" => lines_of("entries", Some("name")),
        // `bytes<TAB>path`, which is what `du` prints and what a `cut -f 2` downstream reads.
        "du" => answer
            .get("files")
            .and_then(|v| v.as_array())
            .map(|files| {
                files
                    .iter()
                    .map(|file| format!("{}\t{}", file["bytes"], file["path"].as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "ps" => lines_of("tasks", None),
        // A table-shaped answer becomes `key value` lines, which is what a grep can look at.
        "df" | "free" | "uname" => match answer.as_object() {
            Some(fields) => {
                let mut out: Vec<String> = Vec::new();
                for (key, value) in fields {
                    let shown = match value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    out.push(format!("{key} {shown}"));
                }
                out.join("\n")
            }
            None => String::new(),
        },
        "date" => text("utc"),
        "uptime" => text("uptime"),
        "echo" => text("stdout"),
        // A fetch hands its body down the pipe: that is the whole point of `curl api | jq`.
        // `-f raw` keeps the document; otherwise the same HTML-to-text reduction `curl`
        // applies to its own answer applies here, so `| grep` sees words rather than tags.
        "curl" => text("body"),
        "wget" => text("body"),
        // `jq` prints one value per line: that is exactly the text a `head` or a `grep`
        // downstream wants, and the reason `curl api | jq .x | head` is the shape people type.
        "jq" => text("text"),
        other => return Err(format!("`{other}` has no line form to hand down a pipe")),
    })
}

/// The text of an MCP tool result: its `content` text when it already has the MCP
/// shape, otherwise the JSON itself. The model gets text, not a protocol envelope.
fn tool_text(v: &Value) -> String {
    if let Some(items) = v.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = items.iter().filter_map(|i| i.get("text").and_then(|t| t.as_str())).collect();
        if !texts.is_empty() {
            return texts.join("\n");
        }
    }
    serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string())
}

/// The reverse of [`tool_text`]: a tool that answers with JSON text, shaped as an MCP
/// result so an external client sees the same envelope as the registry tools.
fn text_result(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => protocol::tool_json(&value),
        Err(_) => json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }),
    }
}

/// Accept either an OpenAI tool or an MCP tool definition from an extra tool group
/// and return the OpenAI shape the model call needs.
fn openai_tools(list: &Value) -> Vec<Value> {
    let Some(items) = list.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|t| {
            if t.get("function").is_some() {
                return Some(t.clone());
            }
            let name = t.get("name").and_then(|n| n.as_str())?;
            let schema = t
                .get("inputSchema")
                .or_else(|| t.get("parameters"))
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": t.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                    "parameters": schema,
                }
            }))
        })
        .collect()
}

/// `s` cut to `n` characters, for log lines that must stay one line.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------- json glue

/// The model config as JSON. `with_key` is for NVS only — the portal never gets it.
fn llm_json(cfg: &LlmConfig, with_key: bool) -> Value {
    let mut v = json!({
        "base_url": cfg.base_url,
        "model": cfg.model,
        "max_steps": cfg.max_steps,
        "temperature": cfg.temperature,
        "timeout_s": cfg.timeout_s,
        "reasoning": cfg.reasoning,
        "vision": cfg.vision,
        "protocol": cfg.protocol,
        "context_tokens": cfg.context_tokens,
        "max_output_tokens": cfg.max_output_tokens,
    });
    if with_key {
        v["api_key"] = json!(cfg.api_key);
    }
    v
}

/// Patch the config from a partial object; absent fields are left alone.
///
/// `api_key: "__set__"` is the masked placeholder the rest of the console already
/// uses, so a page that was filled from a GET cannot overwrite the key with it.
fn llm_apply_json(cfg: &mut LlmConfig, v: &Value) {
    if let Some(s) = v.get("base_url").and_then(|x| x.as_str()) {
        cfg.base_url = s.trim().to_string();
    }
    if let Some(s) = v.get("model").and_then(|x| x.as_str()) {
        cfg.model = s.trim().to_string();
    }
    if let Some(s) = v.get("api_key").and_then(|x| x.as_str()) {
        if s != "__set__" {
            cfg.api_key = s.trim().to_string();
        }
    }
    if let Some(n) = v.get("max_steps").and_then(|x| x.as_u64()) {
        cfg.max_steps = n.min(200) as u8;
    }
    if let Some(t) = v.get("temperature").and_then(|x| x.as_f64()) {
        cfg.temperature = t.clamp(0.0, 2.0) as f32;
    }
    if let Some(t) = v.get("timeout_s").and_then(|x| x.as_u64()) {
        cfg.timeout_s = t.clamp(5, 180) as u32;
    }
    // `off | low | medium | high`, or empty for the provider's own default. Anything else
    // is refused by name rather than forwarded as a typo the endpoint would reject.
    if let Some(r) = v.get("reasoning") {
        let text = r.as_str().unwrap_or("").trim().to_ascii_lowercase().replace("none", "off");
        cfg.reasoning = if text.is_empty() || REASONING_LEVELS.contains(&text.as_str()) {
            text
        } else {
            cfg.reasoning.clone()
        };
    }
    if let Some(b) = v.get("vision").and_then(|x| x.as_bool()) {
        cfg.vision = b;
    }
    if let Some(p) = v.get("protocol").and_then(|x| x.as_str()) {
        cfg.protocol = crate::llm::Protocol::parse(p).as_str().to_string();
    }
    // The two budgets: clamped rather than rejected, because a page that sends 0 or a
    // negative number means "I did not fill this in", and a 400 there would lose the
    // rest of the patch.
    if let Some(n) = v.get("context_tokens").and_then(|x| x.as_u64()) {
        cfg.context_tokens = n.clamp(512, 2_000_000) as u32;
    }
    if let Some(n) = v.get("max_output_tokens").and_then(|x| x.as_u64()) {
        cfg.max_output_tokens = n.clamp(64, 128_000) as u32;
    }
}

/// What `reasoning` may say. `none` is accepted as a synonym for `off` because that is
/// what DeepSeek calls it.
pub const REASONING_LEVELS: &[&str] = &["off", "low", "medium", "high"];

fn task_json(t: &ScheduledTask) -> Value {
    json!({
        "id": t.id,
        "prompt": t.prompt,
        "script": t.script,
        "every_s": t.every_s,
        "at_hhmm": t.at_hhmm,
        "enabled": t.enabled,
        "last_run_ms": t.last_run_ms,
    })
}

/// A task from a JSON object. Lenient on purpose: this also reads the NVS blob,
/// which was validated when it was written. `fallback_id` names a new one.
fn task_from(v: &Value, fallback_id: &str) -> Option<ScheduledTask> {
    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_id.to_string());
    let prompt = v.get("prompt").and_then(|p| p.as_str()).unwrap_or("").trim().to_string();
    let script = v.get("script").and_then(|s| s.as_str()).unwrap_or("").trim().to_string();
    // A task needs something to do: a prompt, a script, or both. A task with neither would
    // fire on schedule and accomplish nothing, which is the kind of silence that looks like
    // a bug six hours later.
    if id.is_empty() || (prompt.is_empty() && script.is_empty()) {
        return None;
    }
    Some(ScheduledTask {
        id,
        prompt,
        every_s: v.get("every_s").and_then(|e| e.as_u64()).unwrap_or(0).min(u32::MAX as u64) as u32,
        at_hhmm: v.get("at_hhmm").and_then(|a| a.as_str()).unwrap_or("").trim().to_string(),
        script: v.get("script").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        enabled: v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
        // A task that has never run starts its clock now, not at the epoch.
        //
        // `due()` compares against the wall clock in milliseconds, and a fresh task used to carry
        // `last_run_ms: 0` - that is 1970, so `since_last` was 1.79e12 and the task fired the
        // moment it was created (measured: asked to schedule an LED change every 120 s, the board
        // changed the LED during the same turn). Stamping the creation time makes the documented
        // promise - a task that has never run waits one full interval - true.
        last_run_ms: v.get("last_run_ms").and_then(|l| l.as_u64()).unwrap_or_else(now_ms),
    })
}

/// What makes an edited task unusable, if anything. The scheduler needs exactly
/// one trigger, and `at_hhmm` has to be a time a clock can reach.
fn task_problem(t: &ScheduledTask) -> Option<String> {
    if t.every_s == 0 && t.at_hhmm.is_empty() {
        return Some("a task needs `every_s` > 0 or `at_hhmm` (\"07:30\")".to_string());
    }
    if !t.at_hhmm.is_empty() && !valid_hhmm(&t.at_hhmm) {
        return Some(format!("`at_hhmm` must be \"HH:MM\" between 00:00 and 23:59, got \"{}\"", t.at_hhmm));
    }
    None
}

fn valid_hhmm(s: &str) -> bool {
    let Some((h, m)) = s.split_once(':') else {
        return false;
    };
    if h.len() != 2 || m.len() != 2 {
        return false;
    }
    match (h.parse::<u32>(), m.parse::<u32>()) {
        (Ok(h), Ok(m)) => h < 24 && m < 60,
        _ => false,
    }
}

fn rule_json(r: &SensorRule) -> Value {
    json!({
        "id": r.id,
        "tool": r.tool,
        "path": r.path,
        "op": r.op,
        "value": r.value,
        "prompt": r.prompt,
        "min_interval_s": r.min_interval_s,
        "last_fire_ms": r.last_fire_ms,
        "enabled": r.enabled,
    })
}

/// A rule from the NVS blob; needs an id, which the store always has.
fn rule_from(v: &Value) -> Option<SensorRule> {
    let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return None;
    }
    rule_from_id(v, &id)
}

fn rule_from_id(v: &Value, fallback_id: &str) -> Option<SensorRule> {
    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_id.to_string());
    let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    if id.is_empty() || tool.is_empty() {
        return None;
    }
    Some(SensorRule {
        id,
        tool,
        path: v.get("path").and_then(|p| p.as_str()).unwrap_or("").trim().to_string(),
        op: v.get("op").and_then(|o| o.as_str()).unwrap_or("true").trim().to_string(),
        value: v.get("value").cloned().unwrap_or(Value::Bool(true)),
        prompt: v.get("prompt").and_then(|p| p.as_str()).unwrap_or("").trim().to_string(),
        min_interval_s: v.get("min_interval_s").and_then(|m| m.as_u64()).unwrap_or(60).min(u32::MAX as u64)
            as u32,
        last_fire_ms: v.get("last_fire_ms").and_then(|l| l.as_u64()).unwrap_or(0),
        enabled: v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
    })
}

/// A rule the scheduler cannot evaluate is refused at the edge, not at 3 a.m.
fn rule_problem(r: &SensorRule) -> Option<String> {
    if !matches!(r.op.as_str(), "eq" | "ne" | "gt" | "lt" | "true") {
        return Some(format!("`op` must be one of eq/ne/gt/lt/true, got \"{}\"", r.op));
    }
    if r.prompt.is_empty() {
        return Some("`prompt` is required: it is what the model is asked when the rule fires".to_string());
    }
    if r.tool.starts_with("fs_") || r.tool.starts_with("net_") {
        // Reading the filesystem on a timer is not a sensor.
        return Some(format!("`tool` must be a board tool; `{}` is not polled", r.tool));
    }
    None
}

// ---------------------------------------------------------------- workspace

/// The agent's file workspace: an in-RAM map persisted as one JSON blob in NVS.
///
/// Deliberately small (a file is <= 2 KB, the whole workspace <= 8 KB) because it
/// shares the chip's heap with the model transcript; SPIFFS can replace it later
/// behind the same [`crate::tools::fs::Workspace`] trait.
#[derive(Default, Debug, Clone)]
pub struct AppWorkspace {
    files: std::collections::BTreeMap<String, String>,
    /// What was last written for each path, so a save rewrites only what changed.
    ///
    /// The hash is content-only: the key for a path never changes, so a failed write leaves the
    /// previous bytes in place and the hash stays wrong until the next attempt, which retries.
    saved: std::collections::BTreeMap<String, u64>,
}

/// Whether a change of `bytes` can be made with `free` bytes of PSRAM.
///
/// Two copies: the rollback keeps the map as it was, and the change builds the new text. The
/// margin is the allocator's overhead and whatever else a turn is holding. The point of asking
/// is that the failure has two shapes and only one of them is usable: a refusal here says
/// "nothing was changed", while an allocation that fails prints `memory allocation of N bytes
/// failed` and calls `abort()` - measured on this board, at a 1.25 MB workspace.
pub fn workspace_fits(bytes: usize, free: Option<usize>) -> std::result::Result<(), String> {
    let Some(free) = free else { return Ok(()) };
    let need = bytes.saturating_mul(2).saturating_add(64 * 1024);
    if free >= need {
        return Ok(());
    }
    Err(format!(
        "the file workspace is {bytes} bytes and a change needs about {need} bytes of PSRAM \
         (a rollback copy plus the new text); {free} is free, so nothing was changed"
    ))
}

impl AppWorkspace {
    /// Serialise this workspace into `out`, one file at a time.
    ///
    /// This replaces `to_json().to_string()`, which built a `serde_json::Value` copy of every
    /// file and then a second full copy as one `String`. Measured on the board: a 1.25 MB
    /// workspace asked for a 1,638,752-byte contiguous allocation, PSRAM could not give it,
    /// and Rust's allocation-failure path called `abort()` - a reboot, not an error. Streaming
    /// needs a few kilobytes however big the workspace is.
    pub fn write_json(&self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        out.write_all(b"{\"version\":1,\"files\":{")?;
        let mut first = true;
        for (path, text) in &self.files {
            if !first {
                out.write_all(b",")?;
            }
            first = false;
            // The JSON escaping of one string, straight into the sink: no intermediate copy of
            // the text, which is the whole point.
            serde_json::to_writer(&mut *out, path).map_err(|e| std::io::Error::other(e.to_string()))?;
            out.write_all(b":")?;
            serde_json::to_writer(&mut *out, text).map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        out.write_all(b"}}")
    }

    /// Parse the document [`AppWorkspace::write_json`] writes, straight into the map.
    ///
    /// `from_json(&Value)` needed the whole document as a `String`, then a `Value` of it, then
    /// the map - three copies on every boot. serde deserialises into the map directly, so the
    /// peak is the map plus the parser's own buffer.
    /// Incremental, and buffered: serde_json's reader pulls one byte at a time while it parses a
    /// string, so handing it the store's reader directly meant a file open and seek per byte.
    /// 8 KB of buffer turns that into one pull per 8 KB of document.
    pub fn from_reader(reader: impl std::io::Read) -> std::result::Result<Self, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct Doc {
            #[serde(default)]
            files: std::collections::BTreeMap<String, String>,
        }
        let mut buffered = std::io::BufReader::with_capacity(8192, reader);
        let doc: Doc = serde_json::from_reader(&mut buffered)?;
        Ok(Self { files: doc.files, saved: Default::default() })
    }

    pub fn to_json(&self) -> Value {
        let mut files = serde_json::Map::new();
        for (path, text) in &self.files {
            files.insert(path.clone(), json!(text));
        }
        json!({ "version": 1, "files": files })
    }

    pub fn from_json(v: &Value) -> Self {
        let mut ws = Self::default();
        if let Some(map) = v.get("files").and_then(|f| f.as_object()) {
            for (path, text) in map {
                if let Some(text) = text.as_str() {
                    ws.files.insert(path.clone(), text.to_string());
                }
            }
        }
        ws
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Every file, path and text: the tree view and the NVS blob both want the whole map.
    pub fn paths(&self) -> impl Iterator<Item = (&String, &String)> {
        self.files.iter()
    }

    /// Total text bytes across every file: the number [`WORKSPACE_TOTAL_MAX`] is about.
    pub fn bytes(&self) -> usize {
        self.files.values().map(|text| text.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// `/a/b` from `a//b/`, `/a/./b`; `..` is refused rather than resolved, so a tool
/// call can never climb out of the workspace.
fn normalize_path(path: &str) -> std::result::Result<String, String> {
    let mut out = String::from("/");
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("`{path}` leaves the workspace root")),
            p => {
                if !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(p);
            }
        }
    }
    Ok(out)
}

impl fs_tools::Workspace for AppWorkspace {
    fn list(&self, dir: &str) -> std::result::Result<Vec<String>, String> {
        let mut prefix = normalize_path(dir)?;
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        // Child *names*, not full paths: `fs_ls` and the `fs_grep` walk both join the
        // directory with what they get back. Returning `/bench/hello.txt` here made
        // the walker look for `/bench//bench/hello.txt` and quietly report an empty
        // search - which is exactly the kind of bug a trait shared by two sides can
        // hide while each side's own unit tests pass.
        let mut out: Vec<String> = Vec::new();
        for path in self.files.keys() {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            let child = rest.split('/').next().unwrap_or("");
            if child.is_empty() || out.iter().any(|n| n == child) {
                continue;
            }
            out.push(child.to_string());
        }
        out.sort();
        Ok(out)
    }

    fn read(&self, path: &str) -> std::result::Result<String, String> {
        let key = normalize_path(path)?;
        self.files.get(&key).cloned().ok_or_else(|| format!("no file `{key}`"))
    }

    fn write(&mut self, path: &str, text: &str) -> std::result::Result<(), String> {
        let key = normalize_path(path)?;
        if text.len() > WORKSPACE_FILE_MAX {
            return Err(format!(
                "`{key}` is {} bytes; the limit is {WORKSPACE_FILE_MAX} bytes per file",
                text.len()
            ));
        }
        let others: usize = self.files.iter().filter(|(k, _)| **k != key).map(|(_, v)| v.len()).sum();
        if others + text.len() > WORKSPACE_TOTAL_MAX {
            return Err(format!(
                "the workspace holds at most {WORKSPACE_TOTAL_MAX} bytes; remove a file first"
            ));
        }
        self.files.insert(key, text.to_string());
        Ok(())
    }

    fn remove(&mut self, path: &str) -> std::result::Result<(), String> {
        let key = normalize_path(path)?;
        match self.files.remove(&key) {
            Some(_) => Ok(()),
            None => Err(format!("no file `{key}`")),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Whether `word` appears in `haystack` as a whole word.
///
/// Substring matching is wrong here because the endpoint is fed back its own output: a
/// reply quoting `board_profile` contains `file`, which used to switch the scripted
/// model onto its file script and burn the whole step budget on the wrong tool.
fn has_word(haystack: &str, word: &str) -> bool {
    haystack.split(|c: char| !c.is_alphanumeric()).any(|token| token.eq_ignore_ascii_case(word))
}

/// The reply the self-test endpoint returns: a `board_status` tool call on the first
/// round, and a final answer quoting the tool result once one is present.
fn selftest_completion(req: &Request) -> Value {
    let body = req.json().unwrap_or(Value::Null);
    // `model: "selftest-slow"` makes this endpoint take 2.5 s to answer. It exists so a
    // bench run can measure what the rest of the board does *while* a model is
    // thinking: with the turn split into phases, the portal should not notice at all.
    if body.get("model").and_then(Value::as_str).map(|m| m.contains("slow")).unwrap_or(false) {
        std::thread::sleep(std::time::Duration::from_millis(2500));
    }
    let empty = Vec::new();
    let messages = body.get("messages").and_then(|m| m.as_array()).unwrap_or(&empty);
    let has_tool = |name: &str| {
        body.get("tools")
            .and_then(|t| t.as_array())
            .map(|tools| tools.iter().any(|t| t["function"]["name"].as_str() == Some(name)))
            .unwrap_or(false)
    };
    // Rounds are counted inside the current turn only: the request carries the whole
    // transcript, so counting every tool message would make the second user message
    // look like a continuation of the first one.
    let last_user = messages.iter().rposition(|m| m["role"].as_str() == Some("user"));
    let tool_rounds = match last_user {
        Some(i) => messages[i..].iter().filter(|m| m["role"].as_str() == Some("tool")).count(),
        None => 0,
    };
    let tool_result = match last_user {
        Some(i) if tool_rounds > 0 => messages[i..]
            .iter()
            .rev()
            .find(|m| m["role"].as_str() == Some("tool"))
            .map(|m| m["content"].as_str().unwrap_or("").to_string()),
        _ => None,
    };
    let ask = messages
        .iter()
        .rev()
        .find(|m| m["role"].as_str() == Some("user"))
        .map(|m| m["content"].as_str().unwrap_or("").to_lowercase())
        .unwrap_or_default();

    // A question about tools is answered from the specs the board actually sent, so a
    // bench run can show the agent's real tool set without a model behind it.
    let tool_names: Vec<String> = body["tools"]
        .as_array()
        .map(|tools| {
            tools.iter().filter_map(|t| t["function"]["name"].as_str().map(|s| s.to_string())).collect()
        })
        .unwrap_or_default();
    if (has_word(&ask, "tool") || ask.contains("工具")) && !tool_names.is_empty() {
        return json!({
            "id": "selftest",
            "object": "chat.completion",
            "model": "selftest",
            "usage": { "prompt_tokens": 32, "completion_tokens": 41 },
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": format!("I was handed {} tools: {}", tool_names.len(), tool_names.join(", ")),
                    "reasoning_content": "selftest: the question is about my tool set, so read it out of the request instead of calling anything",
                },
                "finish_reason": "stop",
            }],
        });
    }

    // Three scripts, chosen by what the user asked for, so one bench run can show
    // each tool family working: the board's own tools, the file workspace, and the
    // HTTP client (which talks to an external host). The file script walks write ->
    // read -> grep so the search path is exercised too, not just the two obvious ones.
    const WRITE: (&str, &str) =
        ("fs_write", r#"{"path":"/bench/hello.txt","text":"written by the self-test turn"}"#);
    const READ: (&str, &str) = ("fs_read", r#"{"path":"/bench/hello.txt"}"#);
    const EDIT: (&str, &str) =
        ("fs_edit", r#"{"path":"/bench/hello.txt","find":"self-test","replace":"edited"}"#);
    const GREP: (&str, &str) = ("fs_grep", r#"{"pattern":"written","path":"/bench"}"#);
    // The workflow the device exists for: discover what is wired, learn the manifest
    // schema, add a tool that was not there before, then call it. Six rounds, so the
    // bench has to raise agent.max_steps to run it.
    const INVENTORY: (&str, &str) = ("inventory", "{}");
    const I2C_SCAN: (&str, &str) = ("i2c_scan", "{}");
    const DESCRIBE: (&str, &str) = ("manifest_describe", "{}");
    const APPLY: (&str, &str) = (
        "manifest_apply",
        r#"{"mode":"merge","manifest":{"tools":[{"name":"bench_mcp_probe","description":"Read the boot button on GPIO0 as a level, 1 released and 0 pressed","params":[],"exec":{"type":"gpio_read","pin":0}}]}}"#,
    );
    const CALL_NEW: (&str, &str) = ("bench_mcp_probe", "{}");
    let steps: &[(&str, &str)] = if has_word(&ask, "edit") || ask.contains("改") {
        &[WRITE, EDIT, READ]
    } else if has_word(&ask, "file") || ask.contains("文件") {
        &[WRITE, READ, GREP]
    } else if has_word(&ask, "mcp") {
        &[INVENTORY, I2C_SCAN, DESCRIBE, APPLY, CALL_NEW, ("board_status", "{}")]
    } else if has_word(&ask, "net") || has_word(&ask, "http") {
        &[("net_get", r#"{"url":"http://example.com/"}"#)]
    } else {
        &[("board_status", "{}")]
    };
    let next_tool = steps.get(tool_rounds).copied();

    let message = match (next_tool, tool_result) {
        (Some((name, args)), _) if has_tool(name) => json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": format!("call_selftest_{tool_rounds}"),
                "type": "function",
                "function": { "name": name, "arguments": args }
            }]
        }),
        (Some((name, _)), _) => json!({
            "role": "assistant",
            "content": format!("selftest: `{name}` is not on the tool list, so there is nothing to demonstrate"),
        }),
        (None, Some(text)) => json!({
            "role": "assistant",
            "content": format!("selftest: the board answered with {}", body_prefix(&text, 160)),
        }),
        (None, None) => json!({ "role": "assistant", "content": "selftest: nothing to do" }),
    };
    let answered = next_tool.is_none();
    // A scripted turn still carries reasoning and usage: the point of this endpoint is
    // to exercise the same pipe a real model feeds, so leaving those empty would hide
    // the reasoning block and the token accounting from every bench run.
    let mut message = message;
    if message["role"].as_str() == Some("assistant") {
        message["reasoning_content"] = json!(format!(
            "selftest has no model behind it: this turn was scripted, {tool_rounds} tool round(s) so far"
        ));
    }
    json!({
        "id": "selftest",
        "object": "chat.completion",
        "model": "selftest",
        "usage": {
            "prompt_tokens": 40 + tool_rounds * 24,
            "completion_tokens": if answered { 12 } else { 9 },
        },
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if answered { "stop" } else { "tool_calls" },
        }],
    })
}

fn body_prefix(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        if i >= max_chars {
            out.push('\u{2026}');
            break;
        }
        out.push(c);
    }
    out
}

fn seed() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x5EED)
}

/// Wrap an `App` in the HTTP handler the server expects.
/// Run one queued agent turn against a shared `App`, holding the lock only for the
/// parts that need it.
///
/// This is the shape a platform wants: the model call - seconds of network - happens
/// with nothing locked, so the portal, `/api/status` and every other handler keep
/// answering while the model thinks. Before this split the whole turn ran inside the
/// lock and a slow model stalled the board's own UI.
///
/// Returns `None` when nothing is queued, `Some(Ok(text))` for a finished turn and
/// `Some(Err(why))` for a turn that failed (already journaled by the agent).
pub fn drive_agent(
    app: &Mutex<App>,
    transport: &mut dyn ChatTransport,
) -> Option<std::result::Result<String, String>> {
    let mut step = app_lock(app).agent_begin()?;
    loop {
        match step {
            AgentStep::Ask { messages, tools } => {
                let cfg = app_lock(app).llm.clone();
                // No lock held across this call: it is the one thing here that blocks.
                let reply = llm::chat(transport, &cfg, &messages, &tools);
                step = app_lock(app).agent_reply(reply);
            }
            AgentStep::Done(result) => return Some(result),
        }
    }
}

/// Seal the app mutex: one place for the poisoned-lock fallback, so no route can
/// turn a panic in one thread into a dead board.
fn app_lock(app: &Mutex<App>) -> std::sync::MutexGuard<'_, App> {
    match app.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Poll the journal for events newer than `after`, already framed for SSE.
///
/// Returns the frames and the highest `seq` seen, so the next poll asks only for
/// what is new. The lock is held for the scan and dropped with the guard.
fn next_sse_frames(shared: &Mutex<App>, after: u64) -> (Vec<String>, u64) {
    let app = app_lock(shared);
    let mut frames = Vec::new();
    let mut newest = after;
    for event in app.agent.events.iter().filter(|e| e.seq > after) {
        frames.push(crate::http::sse_frame(event.seq, event.kind, &event.json().to_string()));
        newest = event.seq;
    }
    (frames, newest)
}

/// The body of `GET /api/agent/stream`: replay, then follow live, on a budget.
///
/// The app lock is taken once per poll and released at the end of each statement -
/// never held across the life of the connection, so the portal, `/api/status` and
/// the agent thread itself keep working while a console is open.
fn event_stream_body(shared: Shared, slot: EventStreamSlot, after: u64, limits: StreamLimits) -> StreamBody {
    Box::new(move |sink: &mut dyn EventSink| {
        // Owned here so every exit path - idle close, total close, failed write -
        // gives the pool slot back through `Drop`.
        let _slot = slot;
        let started = Instant::now();
        let mut last_seq = after;
        let mut last_event = started;
        let mut last_ping = started;
        loop {
            let (frames, newest) = next_sse_frames(&shared, last_seq);
            if !frames.is_empty() {
                for frame in &frames {
                    sink.send(frame.as_bytes())?;
                }
                last_seq = newest;
                last_event = Instant::now();
            }
            let now = Instant::now();
            // Bounded: an idle stream, or one that has had its two minutes. Either
            // way the socket goes back to the pool and the client reconnects with
            // `after=<last id>`, which is what the contract expects.
            if now.saturating_duration_since(last_event) >= limits.idle
                || now.saturating_duration_since(started) >= limits.total
            {
                return Ok(());
            }
            if now.saturating_duration_since(last_ping) >= limits.ping {
                sink.send(crate::http::SSE_PING.as_bytes())?;
                last_ping = now;
            }
            if frames.is_empty() {
                std::thread::sleep(limits.poll);
            }
        }
    })
}

pub fn make_handler(app: Shared) -> Handler {
    // The stream endpoint polls its own journal after the request has returned, so
    // it needs the handle it lives in. Weak, so this cannot become a cycle.
    App::attach_shared(&app);
    Arc::new(move |req: &Request| {
        // The self-test model is answered *before* taking the lock. The agent thread
        // holds that lock for the whole model round trip, so an endpoint that needed
        // it too would deadlock the board against itself the first time a turn ran.
        if req.method == "POST" && req.path == "/selftest/chat/completions" {
            return Response::json(&selftest_completion(req));
        }
        // A lock-free page for the `net_*` self-test: `net_get` runs while the turn
        // holds the App lock, so any route that needs that lock would deadlock the
        // board against itself.
        if req.method == "GET" && req.path == "/selftest/ping" {
            return Response::json(
                &json!({ "ok": true, "service": "selftest-net", "purpose": "lock-free target for net_get" }),
            );
        }
        let mut guard = match app.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.route(req)
    })
}

/// Where a host streams PCM into the speaker: `POST /audio/pcm` (docs/audio.md §6).
///
/// The alternative the board had was a tool argument, and a tool result is capped at 2 KB,
/// so `speaker_play` carries 43 ms — a note, never a song. A request body has no such cap
/// and, with [`crate::http::BodySink`], no buffer either: the bytes go to the amplifier as
/// they arrive, and the speaker's own pacing is what throttles the socket.
pub const AUDIO_STREAM_PATH: &str = "/audio/pcm";

/// How much audio one upload may carry: 30 s at 24 kHz, 16-bit mono.
///
/// Not a memory budget — nothing is held — but a bound on how long one request may hold
/// the speaker, and on how long a client that never stops sending can keep it. Longer audio
/// is a second request, which is also the honest shape for a retry.
pub const AUDIO_STREAM_MAX_SECONDS: u32 = 30;
pub const AUDIO_STREAM_MAX_BYTES: usize = 24_000 * 2 * AUDIO_STREAM_MAX_SECONDS as usize;

/// The sink behind `POST /audio/pcm`: PCM16LE mono, 24 kHz, straight into I2S.
struct AudioSink {
    app: Shared,
    /// Reused across pieces. A fresh `Vec` per socket read would be heap churn on the one
    /// resource this board is short of (`docs/audio.md` §3).
    bytes: Vec<u8>,
    samples: Vec<i16>,
    /// An odd byte can end a piece; it is carried into the next one rather than played as
    /// half a sample.
    carry: Option<u8>,
    received: usize,
    played: usize,
    failed: Option<String>,
}

impl AudioSink {
    fn new(app: Shared) -> Self {
        // One second of audio of headroom on the reused buffers: the socket hands over
        // whatever it has (up to 1 KB), and a chunk never grows past that plus one byte.
        Self {
            app,
            bytes: Vec::with_capacity(1025),
            samples: Vec::with_capacity(512),
            carry: None,
            received: 0,
            played: 0,
            failed: None,
        }
    }
}

impl crate::http::BodySink for AudioSink {
    fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.received + bytes.len() > AUDIO_STREAM_MAX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("more than {AUDIO_STREAM_MAX_SECONDS} s of audio in one request"),
            ));
        }
        self.received += bytes.len();
        self.bytes.clear();
        if let Some(b) = self.carry.take() {
            self.bytes.push(b);
        }
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() % 2 == 1 {
            self.carry = self.bytes.pop();
        }
        self.samples.clear();
        self.samples.extend(self.bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
        if self.samples.is_empty() {
            return Ok(());
        }
        // The one `App` mutex is taken per piece and released again. Holding it for the
        // song would stop the portal, `/api/status` and the board's own agent thread for
        // thirty seconds; taking it per piece keeps the stall to one I2S write.
        let mut state = app_lock(&self.app);
        if let Err(e) = state.hw.i2s_out_write(&self.samples) {
            let message = e.message.clone();
            self.failed = Some(message.clone());
            return Err(std::io::Error::other(message));
        }
        self.played += self.samples.len();
        Ok(())
    }

    fn finish(self: Box<Self>, complete: bool) -> Response {
        let rate_hz = crate::drivers::audio::AMP_RATE_HZ as u64;
        let mut body = json!({
            "format": "pcm_s16le",
            "rate_hz": rate_hz,
            "channels": 1,
            "bits": 16,
            "bytes": self.received,
            "samples": self.played,
            "ms": self.played as u64 * 1000 / rate_hz,
            "limit_bytes": AUDIO_STREAM_MAX_BYTES,
            "ok": true,
        });
        if let Some(e) = self.failed {
            body["ok"] = json!(false);
            body["error"] = json!(e);
            return Response::json_status(500, &body);
        }
        if !complete {
            body["ok"] = json!(false);
            body["error"] = json!("the upload ended before its body did: the rest was not played");
            return Response::json_status(400, &body);
        }
        if self.carry.is_some() {
            body["ok"] = json!(false);
            body["error"] =
                json!("an odd number of bytes: the last byte was dropped, not played as half a sample");
            return Response::json_status(400, &body);
        }
        Response::json(&body)
    }
}

/// Decide what `POST /audio/pcm` does with a body that has not been read yet.
///
/// Everything here is answerable from the request line and the headers, which is the point:
/// a wrong content type or a missing PIN costs one exchange instead of 1.4 MB of audio, and
/// the client never has to upload a song to be told no.
fn audio_route(app: &Shared, req: &Request) -> crate::http::BodyRoute {
    use crate::http::BodyRoute;
    if req.path != AUDIO_STREAM_PATH {
        return BodyRoute::Buffer;
    }
    if req.method != "POST" {
        return BodyRoute::Answer(Response::error_json(
            405,
            "POST a body of PCM16LE mono at 24000 Hz here; there is nothing to GET",
        ));
    }
    // The bytes are played at the amplitude they carry, so the content type has to say what
    // they are: a misdirected JSON POST would otherwise be screamed into the speaker.
    let content_type = req.header("content-type").unwrap_or("").to_ascii_lowercase();
    if !content_type.starts_with("audio/pcm") {
        return BodyRoute::Answer(Response::error_json(
            415,
            "Content-Type must be audio/pcm; the body is raw PCM16LE mono at 24000 Hz (docs/audio.md)",
        ));
    }
    if let Some(rate) = req.query_get("rate") {
        if rate != "24000" {
            return BodyRoute::Answer(Response::error_json(
                400,
                "only 24000 Hz is wired: the amplifier's I2S clock is a constant (docs/audio.md)",
            ));
        }
    }
    let mut state = app_lock(app);
    if let Err(r) = state.require_pin(req) {
        return BodyRoute::Answer(r);
    }
    state.runtime.counters.http_requests += 1;
    BodyRoute::Sink(Box::new(AudioSink::new(Arc::clone(app))))
}

/// The routes this firmware answers with: the request handler, plus the routes whose
/// bodies are streamed instead of buffered.
///
/// One composition root for both, so a platform cannot wire the request handler and forget
/// that `/audio/pcm` or `/av` exists — with no body router those routes fall through to the
/// normal handler, which answers plainly that this build does not stream.
pub fn make_routes(app: Shared) -> crate::http::Routes {
    let audition = Arc::clone(&app);
    let picture = Arc::clone(&app);
    crate::http::Routes::new(make_handler(app)).with_body(Arc::new(move |req: &Request| {
        match audio_route(&audition, req) {
            crate::http::BodyRoute::Buffer => av_route(&picture, req),
            decided => decided,
        }
    }))
}

// ------------------------------------------------ POST /av: sound and picture together

/// Where a host streams a soundtrack and its animation in one body: `POST /av`
/// (docs/audio.md §8).
///
/// The bench probe drove these from two places — a raw PCM socket for the speaker and one
/// MCP tool call per frame for the OLED — because its speaker sat on a task of its own.
/// Here HTTP is a single thread, so a second connection would wait for the song to end, and
/// the alternative (a thread per stream) is the internal RAM this board does not have. One
/// body, consumed in order, keeps them in step without either: an `A` record blocks in the
/// I2S write for exactly as long as its audio lasts, so the `F` record written in front of
/// it is drawn at the right moment by construction.
pub const AV_STREAM_PATH: &str = "/av";

/// PCM bytes one `/av` body may carry: the same 30 s as `/audio/pcm`.
pub const AV_MAX_AUDIO_BYTES: usize = AUDIO_STREAM_MAX_BYTES;

/// The whole body, frames included: 30 s of audio plus half a megabyte of bitmaps, which is
/// 30 s of 128x32 at 12 fps (186 KB) with room to spare.
pub const AV_MAX_BODY_BYTES: usize = AV_MAX_AUDIO_BYTES + 512 * 1024;

/// Bytes of a record header: `tag` and a little-endian `u16` length.
pub const AV_RECORD_HEADER: usize = 3;

/// Splits an `/av` body into records: `tag (1 byte) | length (u16, little endian) | payload`.
///
/// Three tags, and nothing above three bytes of framing, so a host writes this with a
/// `sendall` loop and a struct pack rather than a JSON encoder:
///
/// * `A` — PCM16LE mono at 24 kHz, played now (the write blocks for its own duration);
/// * `F` — a 1-bpp page-format bitmap, 512 bytes for 128x32 or 1024 for 128x64, drawn now;
/// * `E` — end of stream, length 0.
///
/// The reader is separate from what the records *do* so the framing can be tested on the
/// host, split at every awkward boundary, without an `App` or a board.
struct AvReader {
    /// The record header being assembled (at most [`AV_RECORD_HEADER`] bytes).
    head: Vec<u8>,
    tag: u8,
    /// Payload bytes still to come.
    need: usize,
    payload: Vec<u8>,
    done: bool,
}

impl AvReader {
    fn new() -> Self {
        Self { head: Vec::with_capacity(AV_RECORD_HEADER), tag: 0, need: 0, payload: Vec::new(), done: false }
    }

    /// Take the bytes as they arrive; `on` is called once per complete record, in order.
    fn feed(
        &mut self,
        bytes: &[u8],
        on: &mut dyn FnMut(u8, &[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            if self.done {
                // Anything after `E` is a host bug that costs nothing to ignore.
                return Ok(());
            }
            if self.need == 0 {
                let take = (AV_RECORD_HEADER - self.head.len()).min(rest.len());
                self.head.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if self.head.len() < AV_RECORD_HEADER {
                    return Ok(());
                }
                self.tag = self.head[0];
                self.need = u16::from_le_bytes([self.head[1], self.head[2]]) as usize;
                self.head.clear();
                if !matches!(self.tag, b'A' | b'F' | b'E') {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown record tag 0x{:02x}", self.tag),
                    ));
                }
                if self.need == 0 {
                    if self.tag != b'E' {
                        // An empty audio or frame record is a host bug, not a pause.
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "empty record"));
                    }
                    self.done = true;
                    on(b'E', &[])?;
                    return Ok(());
                }
                self.payload.clear();
                continue;
            }
            let take = self.need.min(rest.len());
            self.payload.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            self.need -= take;
            if self.need == 0 {
                let tag = self.tag;
                on(tag, &self.payload)?;
                self.payload.clear();
            }
        }
        Ok(())
    }
}

/// The sink behind `POST /av`: records in, speaker and screen out, in the order written.
struct AvSink {
    app: Shared,
    reader: AvReader,
    /// Reused across records, like the audio sink's, so playback does not churn the heap.
    audio: Vec<i16>,
    audio_bytes: usize,
    samples: usize,
    frames: usize,
    received: usize,
    failed: Option<String>,
}

impl AvSink {
    fn new(app: Shared) -> Self {
        Self {
            app,
            reader: AvReader::new(),
            // 1/12 s of audio: the largest record a 12 fps host writes.
            audio: Vec::with_capacity(4_000),
            audio_bytes: 0,
            samples: 0,
            frames: 0,
            received: 0,
            failed: None,
        }
    }
}

impl crate::http::BodySink for AvSink {
    fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.received + bytes.len() > AV_MAX_BODY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("more than {AV_MAX_BODY_BYTES} bytes in one body"),
            ));
        }
        self.received += bytes.len();
        // Split the borrow: the reader needs `&mut self.reader` while the callback writes
        // to the other fields.
        let shared = Arc::clone(&self.app);
        let AvSink { reader, audio, audio_bytes, samples, frames, failed, .. } = self;
        let mut on = |tag: u8, payload: &[u8]| -> std::io::Result<()> {
            match tag {
                b'E' => Ok(()),
                b'A' => {
                    if payload.len() % 2 != 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "an audio record with an odd number of bytes",
                        ));
                    }
                    if *audio_bytes + payload.len() > AV_MAX_AUDIO_BYTES {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("more than {AUDIO_STREAM_MAX_SECONDS} s of audio in one body"),
                        ));
                    }
                    *audio_bytes += payload.len();
                    audio.clear();
                    audio.extend(payload.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
                    // One lock per record, released before the next: a song must not hold
                    // the portal, `/api/status` or the agent thread for its whole length.
                    let mut state = app_lock(&shared);
                    if let Err(e) = state.hw.i2s_out_write(audio) {
                        *failed = Some(e.message.clone());
                        return Err(std::io::Error::other(e.message));
                    }
                    *samples += audio.len();
                    Ok(())
                }
                b'F' => {
                    if payload.len() != 512 && payload.len() != 1024 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("a frame is 512 (128x32) or 1024 (128x64) bytes, not {}", payload.len()),
                        ));
                    }
                    let height = (payload.len() * 8 / 128) as i64;
                    let b64 = crate::util::b64_encode(payload);
                    // Through the tool, not around it: `oled_bitmap` is the one place that
                    // knows how a 512-byte frame lands on a 64-row panel and how the bus is
                    // addressed, and a second copy of that would drift.
                    let mut state = app_lock(&shared);
                    match state
                        .call_tool("oled_bitmap", &json!({ "data_b64": b64, "width": 128, "height": height }))
                    {
                        Ok(_) => {
                            *frames += 1;
                            Ok(())
                        }
                        Err(e) => {
                            *failed = Some(e.message.clone());
                            Err(std::io::Error::other(e.message))
                        }
                    }
                }
                other => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown record tag 0x{other:02x}"),
                )),
            }
        };
        reader.feed(bytes, &mut on)
    }

    fn finish(self: Box<Self>, complete: bool) -> Response {
        let rate_hz = crate::drivers::audio::AMP_RATE_HZ as u64;
        let mut body = json!({
            "format": "av1",
            "rate_hz": rate_hz,
            "channels": 1,
            "bits": 16,
            "bytes": self.received,
            "audio_bytes": self.audio_bytes,
            "samples": self.samples,
            "ms": self.samples as u64 * 1000 / rate_hz,
            "frames": self.frames,
            "ended": self.reader.done,
            "limit_audio_bytes": AV_MAX_AUDIO_BYTES,
            "limit_bytes": AV_MAX_BODY_BYTES,
            "ok": true,
        });
        if let Some(e) = self.failed {
            body["ok"] = json!(false);
            body["error"] = json!(e);
            return Response::json_status(500, &body);
        }
        if !complete {
            body["ok"] = json!(false);
            body["error"] = json!("the upload ended before its body did: the rest was not played");
            return Response::json_status(400, &body);
        }
        Response::json(&body)
    }
}

/// Decide what `POST /av` does with a body nobody has read yet — the same rule as
/// `/audio/pcm`: method, content type and PIN from the headers, then the sink.
fn av_route(app: &Shared, req: &Request) -> crate::http::BodyRoute {
    use crate::http::BodyRoute;
    if req.path != AV_STREAM_PATH {
        return BodyRoute::Buffer;
    }
    if req.method != "POST" {
        return BodyRoute::Answer(Response::error_json(
            405,
            "POST an AV1 body of audio and frame records here; there is nothing to GET",
        ));
    }
    let content_type = req.header("content-type").unwrap_or("").to_ascii_lowercase();
    if !(content_type.starts_with("application/x-esp32-av")
        || content_type.starts_with("application/octet-stream"))
    {
        return BodyRoute::Answer(Response::error_json(
            415,
            "Content-Type must be application/x-esp32-av (or application/octet-stream); the body is `tag|u16le len|payload` records (docs/audio.md)",
        ));
    }
    let mut state = app_lock(app);
    if let Err(r) = state.require_pin(req) {
        return BodyRoute::Answer(r);
    }
    state.runtime.counters.http_requests += 1;
    BodyRoute::Sink(Box::new(AvSink::new(Arc::clone(app))))
}

/// The tool-shaped, backticked tokens in `text`.
///
/// Deliberately a small rule, not a parser: a token between backticks that is entirely
/// lowercase letters, digits and underscores, starts with a letter and is at most 32
/// characters. That is the shape a tool name has (`board`, `read`, `shell`); a path, a JSON
/// snippet, `name: description` and prose are not it. A rule that tried to be cleverer would
/// be the thing that silently stops matching.
#[cfg(test)]
fn backticked_tool_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else { break };
        let token = &after[..close];
        rest = &after[close + 1..];
        let shaped = token.len() <= 32
            && token.chars().next().map(|c| c.is_ascii_lowercase()).unwrap_or(false)
            && token.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if shaped && !out.iter().any(|seen| seen == token) {
            out.push(token.to_string());
        }
    }
    out
}

/// Backticked tool names in `text` that the model is neither offered nor allowed to use as a
/// legacy alias. Empty means the prompt and the tool list agree.
#[cfg(test)]
fn unadvertised_tool_names(text: &str, offered: &[String]) -> Vec<String> {
    backticked_tool_tokens(text)
        .into_iter()
        .filter(|token| {
            !offered.iter().any(|name| name == token)
                && !App::ALIASES_ACCEPTED_BUT_NOT_ADVERTISED.contains(&token.as_str())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use crate::virtual_board::VirtualBoard;

    fn test_app() -> App {
        let log = LogBuf::new(256);
        App::new(Box::new(VirtualBoard::new()), Box::new(MemoryStore::new()), log, FwMeta::default())
    }

    /// An app whose workspace is empty.
    ///
    /// `test_app` boots through `App::new`, which installs the three built-in skills (see
    /// `crate::seed`). A test that counts files or bytes starts from nothing, so its
    /// arithmetic is about the command it is testing; the seeded skills have their own.
    fn empty_workspace_app() -> App {
        let mut app = test_app();
        app.workspace = AppWorkspace::default();
        app
    }

    // ------------------------------------------------- a store that outlives the App

    /// A store two `App`s can share, so "reload" means "a new `App` over the same keys",
    /// which is what a reboot looks like from the inside. The transcript feature is
    /// exactly the property that a new `App` and an old one disagree about, and a store
    /// per `App` could not express it.
    #[derive(Clone, Debug, Default)]
    struct SharedStore(
        Arc<Mutex<std::collections::BTreeMap<String, String>>>,
        // Writes that actually reached this store, as opposed to being queued for it.
        Arc<std::sync::atomic::AtomicU32>,
    );

    impl SharedStore {
        fn new() -> Self {
            Self::default()
        }

        /// How many `set`/`remove` calls this store has taken. The store writer is the only
        /// thing on a board that makes these calls, so a test that reads this without
        /// flushing first is asking "did the app write flash itself?" — the question this
        /// whole layer exists to answer with "never".
        fn writes(&self) -> u32 {
            self.1.load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Every key currently stored, in order: the transcript spans several of them.
        fn all(&self) -> Vec<String> {
            self.0.lock().map(|m| m.keys().cloned().collect()).unwrap_or_default()
        }
    }

    impl KvStore for SharedStore {
        fn get(&self, key: &str) -> Option<String> {
            self.0.lock().ok()?.get(key).cloned()
        }

        /// The board's store is a file on a filesystem now, so a blob is one value. The
        /// chunking path stays tested by `store::tests`, which drives a store that does not
        /// say this; here the point is that the big blobs go in whole.
        fn whole_value_bytes(&self) -> Option<usize> {
            Some(crate::agent::MAX_SAVED_BYTES.max(super::WORKSPACE_TOTAL_MAX))
        }

        fn set(&mut self, key: &str, value: &str) -> bool {
            match self.0.lock() {
                Ok(mut m) => {
                    m.insert(key.to_string(), value.to_string());
                    self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            }
        }

        fn remove(&mut self, key: &str) -> bool {
            match self.0.lock() {
                Ok(mut m) => {
                    self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    m.remove(key).is_some()
                }
                Err(_) => false,
            }
        }

        fn keys(&self) -> Vec<String> {
            self.all()
        }
    }

    const STUB_URL: &str = "http://127.0.0.1:8899/v1/chat/completions";

    fn app_with_log(store: &SharedStore) -> (App, Arc<LogBuf>) {
        let log = LogBuf::new(512);
        let app =
            App::new(Box::new(VirtualBoard::new()), Box::new(store.clone()), log.clone(), FwMeta::default());
        (app, log)
    }

    fn app_with(store: &SharedStore) -> App {
        app_with_log(store).0
    }

    /// The store writer's job, done at a point a test chooses: these tests read the
    /// *backing* store, and on a board only the writer task ever writes there. Calling it
    /// where a test says "it reached flash" is the point - a write that is merely queued
    /// has not reached anything yet.
    fn drain_store(app: &mut App) {
        app.flush_store("test-writer");
    }

    /// The model endpoint is a stub, and the stub is the transport: only the URL has to
    /// look real, which is the same requirement the board has.
    fn point_at_the_stub(app: &mut App) {
        app.llm.base_url = STUB_URL.to_string();
        app.llm.model = "stub".to_string();
    }

    /// A model that answers from a script and keeps every request body it was sent.
    struct Scripted {
        replies: std::collections::VecDeque<String>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Scripted {
        fn new(replies: Vec<String>) -> Self {
            Self { replies: replies.into(), seen: Arc::new(Mutex::new(Vec::new())) }
        }

        fn answer(text: &str) -> String {
            json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
        }

        fn tool_call(id: &str, name: &str, arguments: &str) -> String {
            json!({ "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [
                { "id": id, "type": "function", "function": { "name": name, "arguments": arguments } }
            ] } }] })
            .to_string()
        }
    }

    impl ChatTransport for Scripted {
        fn post_json(
            &mut self,
            _url: &str,
            _headers: &[(&str, &str)],
            body: &str,
        ) -> std::result::Result<String, String> {
            self.seen.lock().unwrap().push(body.to_string());
            self.replies.pop_front().ok_or_else(|| "scripted: nothing left".to_string())
        }
    }

    /// One turn through the same path the firmware uses: queue, begin, model, apply.
    fn run_turn(app: &Shared, model: &mut Scripted) -> String {
        let out = drive_agent(app, model).expect("a queued turn");
        out.expect("the turn finished")
    }

    /// The transcript is a file on `storage` now, so its ceiling is the store's and not a
    /// count of NVS values. What still has to hold is the *fallback*: a store that cannot take
    /// a whole blob chunks it, and a chunk has to stay under the value limit that store has.
    #[test]
    fn the_transcript_budget_fits_the_store_the_board_uses() {
        // The shipped plan is a plan, not a promise: a store that takes whole values ignores
        // it (one file), and one that does not keeps chunks below the NVS ceiling.
        assert!(TRANSCRIPT_CHUNK_BYTES <= 4000, "ESP-IDF refuses an NVS string value above ~4000 bytes");
        assert!(
            crate::agent::MAX_SAVED_BYTES > TRANSCRIPT_CHUNK_BYTES * TRANSCRIPT_CHUNKS_MAX,
            "the budget is bigger than the NVS fallback on purpose: storage is a filesystem"
        );
        assert_eq!(NVS_KEY_TRANSCRIPT, "transcript");
        assert_eq!(NVS_KEY_RUN_IN_FLIGHT, "run_in_flight");
    }

    #[test]
    fn a_conversation_survives_a_reboot_with_its_tool_results() {
        let store = SharedStore::new();
        let shared = Arc::new(Mutex::new(app_with(&store)));
        {
            let mut app = shared.lock().unwrap();
            point_at_the_stub(&mut app);
        }
        for turn in 0..3u64 {
            let mut model = Scripted::new(vec![
                Scripted::tool_call(&format!("c{turn}"), "board_status", "{}"),
                Scripted::answer(&format!("turn {turn} done")),
            ]);
            shared.lock().unwrap().agent.push_user(&format!("question {turn}"), "http", turn);
            assert_eq!(run_turn(&shared, &mut model), format!("turn {turn} done"));
        }
        // The writer flushes what the turns queued, and only then does the power go out.
        shared.lock().unwrap().flush_store("test-writer");
        let before = { shared.lock().unwrap().agent.history.clone() };
        drop(shared);

        // The power goes out here, and comes back to a new App over the same flash.
        let (app, log) = app_with_log(&store);
        assert_eq!(app.agent.history.len(), before.len(), "the transcript came back whole");
        let roles: Vec<crate::agent::Role> = app.agent.history.iter().map(|m| m.role).collect();
        assert_eq!(roles, before.iter().map(|m| m.role).collect::<Vec<_>>());

        // The tool results are the reason this exists: they are what stops the model from
        // running the same call again.
        let results: Vec<&crate::agent::Message> =
            app.agent.history.iter().filter(|m| m.role == crate::agent::Role::Tool).collect();
        assert_eq!(results.len(), 3, "one result per turn survived");
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.tool_call_id.as_deref(), Some(format!("c{i}").as_str()));
            assert!(result.text.contains("board_name"), "the result text is intact: {}", result.text);
        }
        assert_eq!(app.agent.history.last().map(|m| m.text.as_str()), Some("turn 2 done"));
        assert_eq!(app.agent.saved_dropped, 0);

        // A finished turn leaves no in-flight marker, so nothing is asked again.
        assert!(app.agent.pending.is_none(), "a finished turn is not resumed");
        assert!(
            log.tail(50, 0).iter().any(|l| l.message.contains("restored")),
            "the boot log says the transcript came back: {:?}",
            log.tail(50, 0).iter().map(|l| &l.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_large_transcript_keeps_its_tail_says_so_in_the_prompt_and_spans_several_values() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        // Far more text than the old 16 KB budget: the transcript is a file now, so the
        // question is whether it survives, not whether it fits one NVS value.
        let each = crate::agent::MAX_SAVED_TEXT_BYTES + 20;
        let pairs = crate::agent::MAX_SAVED_BYTES / (each * 2) + 10;
        for i in 0..pairs as u64 {
            app.agent.history.push(crate::agent::Message::user(
                &format!("question {i} {}", "q".repeat(each)),
                "http",
                i,
            ));
            app.agent.history.push(crate::agent::Message::assistant(
                &format!("answer {i} {}", "a".repeat(each)),
                Vec::new(),
                i,
            ));
        }
        assert!(app.persist_transcript());
        drain_store(&mut app);

        // One value, because the store says it takes whole blobs: the chunking exists for a
        // backend that cannot, and that path is tested in `store::tests`.
        let chunks = store.all().iter().filter(|key| key.starts_with(NVS_KEY_TRANSCRIPT)).count();
        assert_eq!(chunks, 1, "the whole tail is one file on the board's store: {chunks} key(s)");

        let (app, log) = app_with_log(&store);
        assert!(app.agent.saved_dropped > 0, "the tail was cut and the count is known");
        assert_eq!(
            app.agent.saved_dropped + app.agent.history.len(),
            pairs * 2,
            "every message is either kept or counted as dropped"
        );
        assert!(app.agent.history.len() < pairs * 2, "the oldest messages gave way");
        assert_eq!(
            app.agent.history.last().map(|m| m.text.starts_with(&format!("answer {}", pairs - 1))),
            Some(true)
        );
        assert!(
            log.tail(80, 0).iter().any(|l| l.level == "warn" && l.message.contains("were not stored")),
            "a cut tail is stated in the log"
        );
        // And in the prompt, which is what the model reads: silence would let it assume.
        let prompt = app.agent.prompt_text(&app);
        assert!(prompt.contains("Recovered after a reboot"), "{prompt}");
        assert!(prompt.contains(&format!("{} older one(s) were not kept", app.agent.saved_dropped)));
    }

    #[test]
    fn a_corrupt_stored_transcript_is_logged_and_the_board_starts_clean() {
        // Whatever the corruption is, the answer is the same: say so and start over.
        for bad in ["not json", "", "{\"v\":9,\"dropped\":0,\"hash\":\"0000000000000000\"}\n{}", "[]"] {
            let store = SharedStore::new();
            let mut writer = store.clone();
            assert!(writer.set(NVS_KEY_TRANSCRIPT, bad));
            let (app, log) = app_with_log(&store);
            assert!(app.agent.history.is_empty(), "`{bad}` became a transcript");
            assert!(app.agent.pending.is_none(), "nothing is resumed from a blob that is not there");
            assert!(
                log.tail(80, 0).iter().any(|l| l.level == "warn" && l.message.contains("does not check out")),
                "`{bad}` was skipped without a word"
            );
        }

        // A torn write is the same case: a good transcript cut in half by a power failure.
        let store = SharedStore::new();
        let mut writer = app_with(&store);
        writer.agent.history.push(crate::agent::Message::user("before the outage", "http", 1));
        writer.agent.history.push(crate::agent::Message::assistant("and before", Vec::new(), 2));
        assert!(writer.persist_transcript());
        writer.flush_store("test-writer");
        let good = store.get(NVS_KEY_TRANSCRIPT).expect("stored");
        let mut writer = store.clone();
        assert!(writer.set(NVS_KEY_TRANSCRIPT, &good[..good.len() - 20]));
        let (app, log) = app_with_log(&store);
        assert!(app.agent.history.is_empty(), "a torn blob is not half-adopted");
        assert!(log.tail(80, 0).iter().any(|l| l.message.contains("does not check out")));
    }

    #[test]
    fn forgetting_reaches_flash_so_the_conversation_does_not_come_back() {
        let store = SharedStore::new();
        let shared = Arc::new(Mutex::new(app_with(&store)));
        {
            let mut app = shared.lock().unwrap();
            point_at_the_stub(&mut app);
        }
        let mut model = Scripted::new(vec![Scripted::answer("noted")]);
        shared.lock().unwrap().agent.push_user("remember this", "http", 1);
        assert_eq!(run_turn(&shared, &mut model), "noted");
        shared.lock().unwrap().flush_store("test-writer");
        assert!(store.get(NVS_KEY_TRANSCRIPT).is_some(), "the turn reached flash");

        // `forget` through the same route an external agent uses.
        let resp = shared.lock().unwrap().route(&post("/api/agent/forget", "{}"));
        assert_eq!(resp.status, 200);
        assert_eq!(body_json(&resp)["removed"], json!(2));
        shared.lock().unwrap().flush_store("test-writer");
        assert!(
            store.get(NVS_KEY_TRANSCRIPT).is_none(),
            "the stored copy survived a forget, so it would come back at the next boot"
        );
        drop(shared);

        let (app, _log) = app_with_log(&store);
        assert!(app.agent.history.is_empty(), "forgetting a conversation must outlive the reboot");

        // The other spelling of the same thing: the portal's reset button.
        let store = SharedStore::new();
        let mut app = app_with(&store);
        app.agent.history.push(crate::agent::Message::user("anything", "http", 1));
        app.agent.history.push(crate::agent::Message::assistant("at all", Vec::new(), 2));
        assert!(app.persist_transcript());
        assert_eq!(app.route(&post("/api/agent/reset", "{}")).status, 200);
        drain_store(&mut app);
        assert!(store.get(NVS_KEY_TRANSCRIPT).is_none());
    }

    #[test]
    fn turning_persistence_off_keeps_the_conversation_out_of_flash() {
        let store = SharedStore::new();
        let shared = Arc::new(Mutex::new(app_with(&store)));
        {
            let mut app = shared.lock().unwrap();
            point_at_the_stub(&mut app);
            assert!(app.cfg.agent.persist_transcript, "on by default");
            app.cfg.agent.persist_transcript = false;
        }
        let mut model = Scripted::new(vec![Scripted::answer("answered anyway")]);
        shared.lock().unwrap().agent.push_user("keep this out of flash", "http", 1);
        assert_eq!(run_turn(&shared, &mut model), "answered anyway");
        assert!(store.get(NVS_KEY_TRANSCRIPT).is_none(), "a disabled transcript was written anyway");
        assert!(store.get(NVS_KEY_RUN_IN_FLIGHT).is_none(), "the marker was cleared at the end");
        drop(shared);

        let (app, _log) = app_with_log(&store);
        assert!(app.agent.history.is_empty(), "nothing is restored when the setting is off");

        // The setting is reachable from the portal's config patch, under `agent`.
        let mut app = app_with(&SharedStore::new());
        let resp = app.route(&post("/api/config", r#"{"agent":{"persist_transcript":false}}"#));
        assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
        assert!(!app.cfg.agent.persist_transcript);
        app.cfg.apply_patch(&json!({ "agent": { "persist_transcript": true } })).expect("patch");
        assert!(app.cfg.agent.persist_transcript);
    }

    #[test]
    fn an_interrupted_turn_is_queued_again_and_its_tool_result_is_not_repeated() {
        let store = SharedStore::new();
        let shared = Arc::new(Mutex::new(app_with(&store)));
        {
            let mut app = shared.lock().unwrap();
            point_at_the_stub(&mut app);
        }
        // A first turn that finished, so the transcript has a history behind it.
        let mut model = Scripted::new(vec![Scripted::answer("yes, it is up")]);
        shared.lock().unwrap().agent.push_user("is the board up?", "http", 1);
        assert_eq!(run_turn(&shared, &mut model), "yes, it is up");

        // The second turn is cut off in the middle: the model asked for a board reading,
        // the result landed and was stored, and the answer never came back. Everything here
        // is the real path - `agent_begin` writes the marker, `agent_reply` stores the step -
        // and the "power failure" is simply that no third reply ever arrives.
        {
            let mut app = shared.lock().unwrap();
            app.agent.push_user("turn the pump on for two seconds", "http", 2);
            assert!(matches!(app.agent_begin(), Some(AgentStep::Ask { .. })));
            drain_store(&mut app);
            assert!(store.get(NVS_KEY_RUN_IN_FLIGHT).is_some(), "a running turn is marked");
            let reply = llm::ModelReply {
                tool_calls: vec![llm::ToolCall {
                    id: "c1".into(),
                    name: "board_status".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            };
            assert!(matches!(app.agent_reply(Ok(reply)), AgentStep::Ask { .. }));
            drain_store(&mut app);
            assert!(
                store.get(NVS_KEY_RUN_IN_FLIGHT).is_some(),
                "a turn that is still asking is still in flight"
            );
            assert!(app.agent.ends_on_a_tool_result(), "the result landed and was not answered");
        }
        drop(shared);

        let (mut app, log) = app_with_log(&store);
        point_at_the_stub(&mut app);
        // The marker is consumed at boot, so a second reboot does not ask a third time. The
        // removal is queued like every other write, so the writer is what makes it stick.
        drain_store(&mut app);
        assert!(store.get(NVS_KEY_RUN_IN_FLIGHT).is_none(), "the marker outlived the boot that read it");
        let queued = app.agent.pending.clone().expect("the interrupted turn is queued again");
        assert_eq!(queued.source, RESUME_SOURCE);
        assert_eq!(queued.text, "turn the pump on for two seconds");
        assert!(
            log.tail(80, 0)
                .iter()
                .any(|l| l.level == "warn" && l.message.contains("interrupted by a reboot")),
            "a resumed turn is announced: {:?}",
            log.tail(80, 0).iter().map(|l| &l.message).collect::<Vec<_>>()
        );
        // The results that landed are still there, and no answer was invented for them.
        assert_eq!(app.agent.history.iter().filter(|m| m.role == crate::agent::Role::Tool).count(), 1);
        assert!(app.agent.ends_on_a_tool_result());

        // The turn continues: one model call, which already carries the call and its result.
        let shared = Arc::new(Mutex::new(app));
        let mut model = Scripted::new(vec![Scripted::answer("the pump ran for two seconds")]);
        assert_eq!(run_turn(&shared, &mut model), "the pump ran for two seconds");
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "a resumed turn is one model call, not a replay of the tool work");
        let request: Value = serde_json::from_str(&seen[0]).expect("json");
        let messages = request["messages"].as_array().expect("messages");
        let call = messages
            .iter()
            .find(|m| m["tool_calls"][0]["id"] == "c1")
            .unwrap_or_else(|| panic!("the call is missing from the resumed request: {request}"));
        let result = messages
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == "c1")
            .unwrap_or_else(|| panic!("its result is missing: {request}"));
        assert!(call["tool_calls"][0]["function"]["arguments"].as_str().unwrap_or_default().contains("{}"));
        assert!(
            result["content"].as_str().unwrap_or_default().contains("board_name"),
            "the board reading is carried: {result}"
        );
        assert_eq!(
            messages.last().map(|m| m["role"].clone()),
            Some(json!("user")),
            "the question is asked again, after the work that already happened"
        );
        assert!(messages.last().unwrap()["content"]
            .as_str()
            .unwrap_or_default()
            .contains("turn the pump on for two seconds"));
        // And the resumed turn is stored like any other, marker cleared.
        assert!(store.get(NVS_KEY_RUN_IN_FLIGHT).is_none());
        assert!(store.get(NVS_KEY_TRANSCRIPT).is_some());
    }

    #[test]
    fn a_script_runs_from_a_file_with_a_command_line() {
        let mut app = test_app();
        // A script in the workspace, the way a shell has one on disk.
        fs_tools::call(
            &mut app.workspace,
            "fs_write",
            &json!({ "path": "/scripts/pin.bs", "text": "args.pin + \"\" + str(len(argv))" }),
        )
        .expect("the fs tool exists")
        .expect("write");

        // `path` + a command line: `--pin 12` fills `args.pin`, and `argv` keeps both words.
        let out = app
            .call_tool("script", &json!({ "path": "/scripts/pin.bs", "args": ["--pin", "12"] }))
            .expect("runs");
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["result"], json!("122"), "args.pin is 12, argv has 2 words: {out}");

        // `name=value` works too, and a bare flag becomes true.
        let out =
            app.call_tool("script", &json!({ "source": "args.mode", "args": ["mode=fast"] })).expect("runs");
        assert_eq!(out["result"], json!("fast"));
        let out = app
            .call_tool("script", &json!({ "source": "args.verbose", "args": ["--verbose"] }))
            .expect("runs");
        assert_eq!(out["result"], json!(true));

        // A named map still works, and a missing path is an error that says so.
        let out =
            app.call_tool("script", &json!({ "source": "args.pin", "args": { "pin": 7 } })).expect("runs");
        assert_eq!(out["result"], json!(7));
        let err =
            app.call_tool("script", &json!({ "path": "/scripts/nope.bs" })).expect_err("no such script");
        assert!(err.message.contains("nope.bs"), "{}", err.message);
    }

    #[test]
    fn plain_http_can_be_turned_off_and_the_patch_says_so() {
        // The setting is a patch field like any other, and the reply has to show it was
        // applied: a silent no-op here would leave a board serving http while its owner
        // believes it is not.
        let mut app = test_app();
        assert!(app.cfg.mcp.plain_http, "on by default, so an upgrade does not lock anyone out");

        let resp = app.route(&post("/api/config", r#"{"mcp":{"plain_http":false}}"#));
        assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
        assert!(!app.cfg.mcp.plain_http, "the flag moved");

        let body = body_json(&resp);
        let touched = body["touched"].as_array().cloned().unwrap_or_default();
        assert!(
            touched.iter().any(|t| t == "mcp.plain_http"),
            "the reply names the field it changed: {touched:?}"
        );

        // And it survives a round trip through the stored config.
        let stored: serde_json::Value =
            serde_json::from_str(&app.cfg.to_json_string()).expect("the stored config is JSON");
        assert_eq!(stored["mcp"]["plain_http"], json!(false));

        // Turning it back on is allowed without ceremony: that is the safe direction.
        let resp = app.route(&post("/api/config", r#"{"mcp":{"plain_http":true}}"#));
        assert_eq!(resp.status, 200);
        assert!(app.cfg.mcp.plain_http);
    }

    #[test]
    fn a_script_can_be_run_over_http_inline_or_as_a_job() {
        let mut app = test_app();

        // Inline is the default: the caller gets the answer in the same response.
        let resp = app.route(&post("/api/script", r#"{"source":"1 + 2"}"#));
        assert_eq!(resp.status, 200);
        let body = body_json(&resp);
        assert_eq!(body["ok"], json!(true), "{body}");
        assert_eq!(body["result"], json!(3));
        assert_eq!(body["state"], json!("done"));

        // Asynchronous: a job id comes back, the plateform tick runs it, the result is read.
        let resp = app.route(&post(
            "/api/script",
            r#"{"source":"let t = 0\nfor x in [1,2,3] { t = t + x }\nt","async":true}"#,
        ));
        assert_eq!(resp.status, 200);
        let body = body_json(&resp);
        let id = body["id"].as_str().expect("an id").to_string();
        assert_eq!(body["state"], json!("queued"), "{body}");
        assert!(body["result"].is_null(), "nothing ran yet: {body}");

        app.run_next_script();
        let resp = app.route(&get(&format!("/api/script/{id}")));
        assert_eq!(resp.status, 200);
        let done = body_json(&resp);
        assert_eq!(done["state"], json!("done"), "{done}");
        assert_eq!(done["result"], json!(6));

        // An unknown job is a 404 that names the id.
        let resp = app.route(&get("/api/script/s999"));
        assert_eq!(resp.status, 404);
        assert!(body_json(&resp)["error"].as_str().unwrap_or("").contains("s999"));

        // A failing script says so: `ok: false` with the interpreter's line and column, so
        // a caller can tell "it ran and refused" from "it never ran".
        let resp = app.route(&post("/api/script", r#"{"source":"let = 3"}"#));
        let body = body_json(&resp);
        assert_eq!(body["ok"], json!(false));
        assert!(body["error"].as_str().unwrap_or("").contains("line"), "{body}");

        // No source at all is refused before anything is queued.
        let resp = app.route(&post("/api/script", "{}"));
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn a_scheduled_task_can_run_a_script_instead_of_a_prompt() {
        let mut app = test_app();
        // One call per task, in the shape the route documents: `kind` + `op` + `task`.
        for body in [
            r#"{"kind":"task","op":"add","task":{"id":"t1","prompt":"say hi","every_s":60,"last_run_ms":0}}"#,
            r#"{"kind":"task","op":"add","task":{"id":"t2","script":"1 + 1","every_s":60,"last_run_ms":0}}"#,
        ] {
            let resp = app.route(&post("/api/sched", body));
            assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
        }

        let fired = app.sched_tick(60_000, 0);
        assert_eq!(fired.len(), 2, "{fired:?}");
        assert!(fired.contains(&"schedule:t2".to_string()));

        // The script task ran without the model: its value is in the job, and no turn was
        // queued for it. The prompt task queued a message as before.
        let job = app.scripts.iter().find(|j| j.state == "done").expect("a finished job");
        assert_eq!(job.result, json!(2));
        assert!(app.agent.pending.is_some(), "the prompt task still queues a message");
    }

    // ------------------------------------------- who is allowed to touch flash

    /// A due task runs on the agent thread, and that thread's stack is not guaranteed to
    /// be in internal RAM — measured on the bench 2026-11-12: stack in PSRAM, a due task,
    /// `persist_sched` -> `nvs_erase_key`, `esp_task_stack_is_sane_cache_disabled()`, reboot
    /// every ~8 s, forever, because the task was still due after the reboot.
    ///
    /// So this is the constraint as an assertion rather than a comment: the tick may ask
    /// for the write, and the store must not see one. Reverting `QueuedStore` to write
    /// straight through makes `writes()` 1 here and this test fail.
    #[test]
    fn a_due_task_queues_its_write_and_the_tick_never_touches_flash() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        // Let the writer catch up with what boot queued (the seeded skills), so the counts
        // below are about the scheduled task and nothing else.
        app.flush_store("test-writer");
        let writes_before = store.writes();
        let resp = app.route(&post(
            "/api/sched",
            r#"{"kind":"task","op":"add","task":{"id":"t1","prompt":"say hi","every_s":60,"last_run_ms":0}}"#,
        ));
        assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
        assert_eq!(store.writes(), writes_before, "accepting a task must not write flash either");

        // One interval later, exactly what the agent thread's tick does on the board.
        let now = 60_000;
        let fired = app.sched_tick(now, 0);
        assert_eq!(fired, vec!["schedule:t1".to_string()]);
        assert_eq!(
            store.writes(),
            writes_before,
            "the scheduler tick runs on the agent thread; a write from there is the reboot loop"
        );
        // Read-your-writes: the queued schedule already says the task ran, so a second tick
        // cannot fire it twice while the writer has not caught up.
        let queued = app.store.get(NVS_KEY_SCHED).expect("the tick queued the schedule");
        assert!(queued.contains("\"t1\""), "{queued}");
        assert_eq!(app.store_queue().pending(), 1, "one key, however many ticks queued it");

        // The writer is what makes it durable, and it is the only thing that does.
        let report = app.flush_store("test-writer");
        assert_eq!(report.written, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.keys, vec![NVS_KEY_SCHED.to_string()]);
        assert_eq!(store.writes(), writes_before + 1);
        assert!(store.get(NVS_KEY_SCHED).expect("stored").contains("\"t1\""));
        assert_eq!(app.store_queue().pending(), 0);
        // And the next tick is not due: only true because the write was applied.
        assert!(app.sched_tick(now + 1_000, 0).is_empty());
    }

    /// The same rule for every other durable change, driven through the routes and tools a
    /// person or a model actually uses: config, file workspace and transcript.
    #[test]
    fn no_edit_writes_flash_where_it_was_made() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        // Boot queues what it has to rewrite (the seeded skills), and writes nothing.
        assert_eq!(store.writes(), 0, "boot must not write flash from the thread that runs it");

        assert_eq!(app.route(&post("/api/config", r#"{"device_name":"bench"}"#)).status, 200);
        ToolHost::call_tool(&mut app, "fs_write", &json!({ "path": "/a.txt", "text": "hello" }))
            .expect("fs_write");
        app.agent.history.push(crate::agent::Message::user("question", "http", 1));
        assert!(app.persist_transcript());
        assert_eq!(store.writes(), 0, "an edit must not reach flash from the thread that made it");

        let report = app.flush_store("test-writer");
        assert_eq!(report.failed, 0, "{:?}", report.keys);
        assert!(report.written >= 3, "{:?}", report.keys);
        for key in [crate::config::NVS_KEY_CONFIG, crate::keys::WORKSPACE_INDEX, NVS_KEY_TRANSCRIPT] {
            assert!(
                report.keys.iter().any(|queued| queued == key),
                "`{key}` was never queued: {:?}",
                report.keys
            );
        }
        assert!(store.get(crate::config::NVS_KEY_CONFIG).expect("stored").contains("bench"));
        assert_eq!(app.store_queue().pending(), 0);
        // The counters `/api/status.store` shows agree with what happened.
        let status = body_json(&app.route(&get("/api/status")));
        assert_eq!(status["store"]["written"], json!(report.written));
        assert_eq!(status["store"]["failed"], json!(0));
        assert_eq!(status["store"]["pending"], json!(0));
    }

    /// What `/api/status.store` says about the writer: a board whose writer is on the wrong
    /// kind of stack has to be visible from outside, not only in a log line.
    #[test]
    fn the_status_names_the_task_that_writes_and_whether_its_stack_is_safe() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        assert_eq!(
            body_json(&app.route(&get("/api/status")))["store"]["writer"]["internal_stack"],
            Value::Null
        );
        app.attach_store_writer("store-writer", true);
        let status = body_json(&app.route(&get("/api/status")));
        assert_eq!(status["store"]["writer"]["task"], json!("store-writer"));
        assert_eq!(status["store"]["writer"]["internal_stack"], json!(true));
    }

    /// The other half of `store.failed`: *which* key, and why. A board with no serial
    /// terminal attached has only this, so it has to be true before anything is refused and
    /// complete after.
    #[test]
    fn a_refused_write_shows_up_in_the_status_with_its_key_and_code() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        let fresh = body_json(&app.route(&get("/api/status")));
        assert_eq!(fresh["store"]["last_failure"], Value::Null, "nothing refused yet");
        assert_eq!(fresh["store"]["refused_keys"], json!({}));
        assert_eq!(fresh["store"]["refused_other"], json!(0));

        // The name that used to be here: 16 bytes, one over what NVS keeps.
        app.store.set("agent.transcript", "a transcript");
        app.flush_store("test-writer");
        assert!(store.get("agent.transcript").is_none(), "and it really did not reach the store");

        let status = body_json(&app.route(&get("/api/status")));
        assert_eq!(status["store"]["failed"], json!(1), "the count stays what it was");
        assert_eq!(status["store"]["last_failure"]["key"], json!("agent.transcript"));
        assert_eq!(status["store"]["last_failure"]["op"], json!("set"));
        assert_eq!(status["store"]["last_failure"]["err"], json!(store::ERR_KEY_TOO_LONG));
        assert_eq!(status["store"]["last_failure"]["err_name"], json!(store::ERR_KEY_TOO_LONG_NAME));
        assert!(status["store"]["last_failure"]["at_ms"].is_u64(), "{:?}", status["store"]);
        assert_eq!(status["store"]["refused_keys"]["agent.transcript"], json!(1));

        // A second refusal of the same key counts up and replaces the newest one.
        app.store.set("agent.transcript", "again");
        app.flush_store("test-writer");
        let status = body_json(&app.route(&get("/api/status")));
        assert_eq!(status["store"]["failed"], json!(2));
        assert_eq!(status["store"]["refused_keys"]["agent.transcript"], json!(2));
        // A key that fits is accepted and does not disturb the record of the one that did not.
        app.store.set("config", "{}");
        app.flush_store("test-writer");
        let status = body_json(&app.route(&get("/api/status")));
        assert_eq!(status["store"]["last_failure"]["key"], json!("agent.transcript"));
    }

    /// The length invariant itself lives in `keys.rs`, on the generator. This is the other
    /// half: the two blob plans must stay inside what that table enumerated, or a raised
    /// `chunks_max` would produce a key the invariant never looked at.
    #[test]
    fn the_blob_plans_stay_inside_the_key_table() {
        let table = crate::keys::all();
        for index in 1..=WORKSPACE_PLAN.chunks_max {
            let key = crate::keys::workspace_chunk(index);
            assert!(table.contains(&key), "`{key}` is missing from keys::all()");
            assert!(key.len() <= store::KEY_MAX, "`{key}` is {} bytes", key.len());
        }
        for index in 0..TRANSCRIPT_CHUNKS_MAX {
            let key = App::transcript_key(index);
            assert!(table.contains(&key), "`{key}` is missing from keys::all()");
            assert!(key.len() <= store::KEY_MAX, "`{key}` is {} bytes", key.len());
        }
        // The table is wider than the plans on purpose: raising either ceiling to 99 must
        // still be under the length limit, and `keys::all()` is what proves it.
        assert!(crate::keys::all().contains(&crate::keys::transcript(99)));
        assert!(crate::keys::all().contains(&crate::keys::workspace_chunk(99)));
    }

    /// The map is only worth reading if it is true.
    ///
    /// Every documented route is called here on a real `App`; a 404 means the document
    /// promises something the router does not serve. Mutations are included on purpose —
    /// this is a throwaway app, and a route that answers is exactly what is being asserted.
    /// The stream route decides on the headers, plays what arrives, and says what it
    /// refused. No buffer, no second thread, no half sample.
    #[test]
    fn the_audio_route_refuses_before_the_body_and_then_plays_it() {
        use crate::http::BodyRoute;

        let pins = crate::boards::I2sPins { bclk: 15, ws: 16, din: -1, dout: 7 };
        let shared: Shared = Arc::new(Mutex::new(test_app()));
        {
            let mut app = shared.lock().unwrap();
            // The virtual board grows a speaker only when the channels are brought up.
            app.hw.i2s_setup(None, Some(pins)).expect("amplifier");
        }
        let routes = make_routes(Arc::clone(&shared));
        let router = routes.body.as_ref().expect("a body router");

        // A JSON POST is refused from its content type alone: the body is never read, so a
        // misdirected call cannot be played as noise.
        let mut req = post(AUDIO_STREAM_PATH, "");
        req.headers.insert("content-type".into(), "application/json".into());
        match router(&req) {
            BodyRoute::Answer(r) => assert_eq!(r.status, 415),
            _ => panic!("content type must be checked before the body"),
        }

        // The rate is a constant on this board, and saying so costs nothing.
        let mut req = post(AUDIO_STREAM_PATH, "");
        req.headers.insert("content-type".into(), "audio/pcm".into());
        req.query.insert("rate".into(), "16000".into());
        match router(&req) {
            BodyRoute::Answer(r) => assert_eq!(r.status, 400),
            _ => panic!("a rate the amplifier cannot run must be refused"),
        }

        let mut req = post(AUDIO_STREAM_PATH, "");
        req.headers.insert("content-type".into(), "audio/pcm".into());
        let mut sink = match router(&req) {
            BodyRoute::Sink(s) => s,
            _ => panic!("audio/pcm must be streamed"),
        };
        // Two samples, then one byte short of a third: the odd byte is carried.
        sink.push(&[0x00, 0x10, 0x00, 0xF0]).expect("a piece");
        sink.push(&[0x7f]).expect("an odd byte is carried, not played");
        let resp = sink.finish(true);
        assert_eq!(resp.status, 400, "a body that ends mid-sample is not a song: {:?}", resp.body);
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["samples"], json!(2), "the odd byte must not be played");
        assert_eq!(body["ok"], json!(false));

        // A whole body answers 200 and reports what was played.
        let mut req = post(AUDIO_STREAM_PATH, "");
        req.headers.insert("content-type".into(), "audio/pcm".into());
        let mut sink = match router(&req) {
            BodyRoute::Sink(s) => s,
            _ => panic!("audio/pcm must be streamed"),
        };
        sink.push(&[0u8; 4800]).expect("100 ms of silence");
        let resp = sink.finish(true);
        assert_eq!(resp.status, 200, "{:?}", resp.body);
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["samples"], json!(2400));
        assert_eq!(body["ms"], json!(100));

        // The cap is the request's budget, and past it the upload stops rather than being
        // silently truncated.
        let mut req = post(AUDIO_STREAM_PATH, "");
        req.headers.insert("content-type".into(), "audio/pcm".into());
        let mut sink = match router(&req) {
            BodyRoute::Sink(s) => s,
            _ => panic!("audio/pcm must be streamed"),
        };
        assert!(sink.push(&vec![0u8; AUDIO_STREAM_MAX_BYTES]).is_ok());
        let too_much = sink.push(&[0u8; 2]);
        assert!(too_much.is_err(), "a 31st second must be refused");
        let resp = sink.finish(false);
        assert_eq!(resp.status, 400, "a cut-off upload must not read as done");
    }

    /// The framing has to survive whatever a socket hands over: a header split across three
    /// reads, a payload split across two, and two records arriving together.
    #[test]
    fn the_av_reader_reassembles_records_whatever_the_split() {
        let body = [
            b'A', 0x04, 0x00, 1, 2, 3, 4, //
            b'F', 0x02, 0x00, 9, 8, //
            b'E', 0x00, 0x00,
        ];
        for split in 0..=body.len() {
            let mut reader = AvReader::new();
            let mut got: Vec<(u8, Vec<u8>)> = Vec::new();
            let mut on = |tag: u8, payload: &[u8]| -> std::io::Result<()> {
                got.push((tag, payload.to_vec()));
                Ok(())
            };
            reader.feed(&body[..split], &mut on).expect("first half");
            reader.feed(&body[split..], &mut on).expect("second half");
            assert_eq!(got.len(), 3, "split at {split}: {got:?}");
            assert_eq!(got[0], (b'A', vec![1, 2, 3, 4]), "split at {split}");
            assert_eq!(got[1], (b'F', vec![9, 8]), "split at {split}");
            assert_eq!(got[2].0, b'E', "split at {split}");
            assert!(reader.done, "split at {split}");
        }

        // One byte at a time is the case the running header exists for.
        let mut reader = AvReader::new();
        let mut tags = Vec::new();
        let mut on = |tag: u8, _p: &[u8]| -> std::io::Result<()> {
            tags.push(tag);
            Ok(())
        };
        for b in body {
            reader.feed(&[b], &mut on).expect("one byte");
        }
        assert_eq!(tags, vec![b'A', b'F', b'E']);
    }

    #[test]
    fn the_av_reader_refuses_what_a_host_cannot_have_meant() {
        let mut reader = AvReader::new();
        let mut on = |_t: u8, _p: &[u8]| -> std::io::Result<()> { Ok(()) };
        // 0x41 is `A`; thankyou, ASCII. A JSON body starts with `{`.
        let err = reader.feed(b"{\"json\": true}", &mut on).expect_err("unknown tag");
        assert!(err.to_string().contains("unknown record tag"), "{err}");

        let mut reader = AvReader::new();
        let err = reader.feed(b"A\x00\x00", &mut on).expect_err("empty audio record");
        assert!(err.to_string().contains("empty record"), "{err}");
    }

    /// `/av` plays the audio and draws the frame written in front of it — one body, one
    /// thread, no second connection, and 512-byte frames on a panel the board calls 128x64.
    #[test]
    fn the_av_route_plays_audio_and_draws_frames_in_order() {
        use crate::http::BodyRoute;

        let pins = crate::boards::I2sPins { bclk: 15, ws: 16, din: -1, dout: 7 };
        let shared: Shared = Arc::new(Mutex::new(test_app()));
        {
            let mut app = shared.lock().unwrap();
            app.hw.i2s_setup(None, Some(pins)).expect("amplifier");
        }
        // The virtual bench has an SSD1306, so `oled_bitmap` is a real tool here.
        assert!(shared.lock().unwrap().tools.get("oled_bitmap").is_some(), "no OLED on the test bench");

        let routes = make_routes(Arc::clone(&shared));
        let router = routes.body.as_ref().expect("a body router");

        let mut req = post(AV_STREAM_PATH, "");
        req.headers.insert("content-type".into(), "application/x-esp32-av".into());
        let mut sink = match router(&req) {
            BodyRoute::Sink(s) => s,
            _ => panic!("an AV body must be streamed, never buffered"),
        };

        // Two 1/12 s intervals: the frame first (so it is on screen while its audio plays),
        // then 8000 bytes = 4000 samples of silence.
        let mut body = Vec::new();
        for _ in 0..2 {
            body.push(b'F');
            body.extend_from_slice(&512u16.to_le_bytes());
            body.extend_from_slice(&[0u8; 512]);
            body.push(b'A');
            body.extend_from_slice(&8000u16.to_le_bytes());
            body.extend_from_slice(&[0u8; 8000]);
        }
        body.push(b'E');
        body.extend_from_slice(&0u16.to_le_bytes());

        // A socket hands over whatever it has; 1000-byte pieces cut every record in half.
        for piece in body.chunks(1000) {
            sink.push(piece).expect("a piece");
        }
        let resp = sink.finish(true);
        assert_eq!(resp.status, 200, "{:?}", resp.body);
        let done: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(done["frames"], json!(2));
        assert_eq!(done["samples"], json!(8000));
        assert_eq!(done["ms"], json!(333));
        assert_eq!(done["ended"], json!(true));
        assert_eq!(done["ok"], json!(true));
    }

    /// The same rule as `/audio/pcm`: refused from the headers alone, and an odd audio
    /// record is a host bug the answer names.
    #[test]
    fn the_av_route_refuses_what_it_cannot_play() {
        use crate::http::BodyRoute;

        let shared: Shared = Arc::new(Mutex::new(test_app()));
        let routes = make_routes(Arc::clone(&shared));
        let router = routes.body.as_ref().expect("a body router");

        let mut get = post(AV_STREAM_PATH, "");
        get.method = "GET".into();
        get.headers.insert("content-type".into(), "application/x-esp32-av".into());
        match router(&get) {
            BodyRoute::Answer(r) => assert_eq!(r.status, 405),
            _ => panic!("GET has no body to stream"),
        }

        let mut json = post(AV_STREAM_PATH, "");
        json.headers.insert("content-type".into(), "application/json".into());
        match router(&json) {
            BodyRoute::Answer(r) => assert_eq!(r.status, 415),
            _ => panic!("a JSON body is not records"),
        }

        let mut req = post(AV_STREAM_PATH, "");
        req.headers.insert("content-type".into(), "application/octet-stream".into());
        let mut sink = match router(&req) {
            BodyRoute::Sink(s) => s,
            _ => panic!("octet-stream is the body every CLI sends"),
        };
        // Three bytes of audio is one sample and a half: refused, not rounded.
        let err = sink.push(b"A\x03\x00abc").expect_err("odd audio record");
        assert!(err.to_string().contains("odd number of bytes"), "{err}");

        let mut sink = match router(&req) {
            BodyRoute::Sink(s) => s,
            _ => panic!("a fresh sink"),
        };
        // A frame that is neither 128x32 nor 128x64 is not a frame.
        let err = sink.push(b"F\x10\x00aaaaaaaaaaaaaaaa").expect_err("wrong frame size");
        assert!(err.to_string().contains("512"), "{err}");
    }

    /// A board whose own agent loop is dead looked exactly like a quiet board: the only
    /// thing that said so was one line in the log, which nobody reads unless they already
    /// suspect something. `/api/status` now carries the same decision.
    #[test]
    fn the_board_says_whether_its_agent_thread_is_up() {
        let mut app = test_app();
        assert_eq!(app.status_json()["agent"]["thread"], json!("not started"));

        app.agent_thread_running();
        assert_eq!(app.status_json()["agent"]["thread"], json!("running"));

        let why = "could not start the agent thread: Not enough space (os error 12)";
        app.agent_thread_failed(why);
        assert_eq!(app.status_json()["agent"]["thread"], json!(format!("did not start: {why}")));

        // The same call wrote the boot line, so the two cannot disagree. `tail` returns the
        // newest last; the failure is the last thing this app logged.
        let last = app.log.tail(1, 0).pop().expect("a log line");
        assert_eq!(last.level, "error");
        assert_eq!(last.target, "agent");
        assert!(last.message.contains(why), "{}", last.message);
    }

    #[test]
    fn every_documented_route_actually_answers() {
        let mut app = test_app();
        let mut missing = Vec::new();
        for route in crate::sitemap::ROUTES {
            let mut req = post(route.path, "{}");
            req.method = route.method.into();
            req.headers.insert("host".into(), "board.test:8080".into());
            let resp = app.route(&req);
            if resp.status == 404 {
                missing.push(format!("{} {}", route.method, route.path));
            }
        }
        assert!(missing.is_empty(), "documented but not routed: {missing:?}");
    }

    #[test]
    fn the_discovery_document_knows_where_it_is_served() {
        let mut app = test_app();
        let mut req = get("/agent.json");
        req.headers.insert("host".into(), "192.168.3.7:8080".into());
        let resp = app.route(&req);
        assert_eq!(resp.status, 200);
        let body = String::from_utf8(resp.body.clone()).expect("json");
        let doc: Value = serde_json::from_str(&body).expect("parses");
        assert_eq!(doc["dynamic"]["base_url"], "http://192.168.3.7:8080");
        assert_eq!(doc["dynamic"]["mcp"]["path"], "/mcp");
        let endpoints = doc["endpoints"].as_array().expect("endpoints");
        assert!(endpoints.iter().any(|e| e["path"] == "/agent.json"));
    }

    /// The map is a promise about where to send the next request. Over TLS it has to
    /// say https, or an external agent connects to a port that is not answering that way.
    #[test]
    fn the_discovery_document_follows_the_listener_that_answered() {
        let mut app = test_app();
        // What the firmware sets once the 443 listener is up.
        app.tls = TlsInfo { port: 443, fingerprint: "aa:bb".into(), error: String::new() };
        let mut req = get("/agent.json");
        req.headers.insert("host".into(), "192.168.3.7".into());
        req.scheme = "https";
        let doc: Value = serde_json::from_slice(&app.route(&req).body).expect("parses");
        assert_eq!(doc["dynamic"]["base_url"], "https://192.168.3.7");

        // The MCP `initialize` response quotes the same endpoint, by the same rule.
        let mut init = Request {
            method: "POST".into(),
            path: "/mcp".into(),
            query: Default::default(),
            headers: Default::default(),
            body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#.to_vec(),
            peer: "test".into(),
            scheme: "https",
        };
        init.headers.insert("content-type".into(), "application/json".into());
        let reply: Value = serde_json::from_slice(&app.route(&init).body).expect("json");
        let text = reply["result"]["instructions"].as_str().expect("instructions");
        assert!(text.contains("https://"), "the endpoint told the client: {text}");
        assert!(!text.contains("Endpoint: http://"), "the endpoint told the client: {text}");
        // And the port that answers *that* scheme — 443, not the plaintext 80.
        assert!(
            text.contains("https://") && text.contains(":443/mcp"),
            "the endpoint told the client: {text}"
        );

        // Over plain http the same board keeps naming port 80.
        let mut plain = init.clone();
        plain.scheme = "http";
        let reply: Value = serde_json::from_slice(&app.route(&plain).body).expect("json");
        let text = reply["result"]["instructions"].as_str().expect("instructions");
        assert!(text.contains("http://") && text.contains(":80/mcp"), "the endpoint told the client: {text}");
    }

    /// A GET request; the query string is split off into `query` the way the
    /// server does it, so `?after=2` reaches the handler.
    fn get(url: &str) -> Request {
        let (path, query) = match url.split_once('?') {
            Some((p, q)) => (
                p.to_string(),
                q.split('&')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            None => (url.to_string(), Default::default()),
        };
        let mut req = post(&path, "");
        req.method = "GET".into();
        req.query = query;
        req
    }

    fn post(path: &str, body: &str) -> Request {
        Request {
            method: "POST".into(),
            path: path.into(),
            query: Default::default(),
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
            peer: "test".into(),
            scheme: "http",
        }
    }

    fn body_json(resp: &Response) -> Value {
        serde_json::from_slice(&resp.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&resp.body)))
    }

    /// The portal has to carry the agent panel: a panel whose buttons call an
    /// endpoint that does not exist is worse than no panel.
    ///
    /// The page is a conversation with everything else behind the menu, so the panel is
    /// reached through `data-panel`, and the transcript lives in `.chat` as bubbles
    /// rather than in a transcript table.
    #[test]
    fn the_portal_ships_the_agent_panel() {
        assert!(web::INDEX_HTML.contains("id=\"panel-agent\""));
        assert!(web::INDEX_HTML.contains("data-panel=\"agent\""));
        assert!(web::INDEX_HTML.contains("id=\"chat\""), "the conversation is the page");
        for needle in ["/api/agent/message", "/api/agent/history", "/api/agent/config", "/api/sched"] {
            assert!(web::APP_JS.contains(needle), "app.js never calls {needle}");
        }
        // A schedule table that cannot toggle a task does not meet the brief.
        assert!(web::APP_JS.contains("sched_disable") && web::APP_JS.contains("sched_enable"));
        // A task can be a script as well as a prompt, and the board has always accepted
        // both: the form only offered one of them.
        assert!(web::INDEX_HTML.contains("sched-script"), "the schedule form cannot take a script");
        assert!(web::APP_JS.contains("script:"), "the form never sends the script field");
        // A result belongs to the call its `tool_call_id` names, not to whichever chip is
        // still open: the arguments printed beside a result were sometimes another call's.
        assert!(web::APP_JS.contains("dataset.id === data.tool_call_id"), "results are matched positionally");
        // And while the board is silent, the page says it is still working.
        assert!(web::STYLE_CSS.contains(".dot-anim"), "no live indicator in the stylesheet");
        assert!(web::STYLE_CSS.contains(".bubble"), "bubbles are the transcript");
        assert!(web::STYLE_CSS.contains("100dvh"), "mobile height, not a guessed one");
    }

    /// The board's own agent can put work on the board's schedule: a task, on an interval
    /// or at a daily time, into the same queue a user message uses.
    #[test]
    fn the_agent_can_schedule_its_own_work() {
        let mut app = test_app();
        let added = app
            .call_tool("schedule", &json!({"action":"add","prompt":"read the sensor","every_s":60}))
            .expect("add");
        assert_eq!(added["id"], "t1");
        assert_eq!(app.tasks.len(), 1);
        assert_eq!(app.tasks[0].every_s, 60);
        assert!(app.runtime.sched_dirty || !app.tasks.is_empty());

        // A task with no trigger never fires, so it is refused instead of stored.
        let bad = app.call_tool("schedule", &json!({"action":"add","prompt":"nothing"}));
        assert!(bad.is_err(), "a task with neither every_s nor at_hhmm is a trap");

        let listed = app.call_tool("schedule", &json!({"action":"list"})).expect("list");
        assert_eq!(listed["count"], 1);

        app.call_tool("schedule", &json!({"action":"disable","id":"t1"})).expect("disable");
        assert!(!app.tasks[0].enabled);
        app.call_tool("schedule", &json!({"action":"remove","id":"t1"})).expect("remove");
        assert!(app.tasks.is_empty());
    }

    /// A fresh task waits its interval; it does not fire the moment it is created.
    ///
    /// `due()` compares `last_run_ms` with the wall clock, and a new task used to carry 0 - 1970.
    /// Against an epoch of 1.79e12 that is "due since forever", so the board did the work during
    /// the same turn that scheduled it (measured on the board: `schedule add {every_s:120}` was
    /// followed by `indicators led_set #ff0000` before the model finished answering).
    #[test]
    fn a_new_task_waits_a_full_interval() {
        let mut app = test_app();
        app.call_tool("schedule", &json!({"action":"add","id":"t1","prompt":"tick","every_s":120}))
            .expect("add");
        let task = app.tasks[0].clone();
        // The clock the scheduler really uses: milliseconds since the epoch, not uptime.
        let now = crate::app::now_ms();
        assert!(task.last_run_ms > 1_000_000_000_000, "stamped with the epoch, got {}", task.last_run_ms);
        assert!(!crate::sched::due(&task, now, 0), "fired in the turn that created it");
        assert!(!crate::sched::due(&task, now + 119_000, 0), "fired a second early");
        assert!(crate::sched::due(&task, now + 121_000, 0), "never fires after its interval");
    }

    /// Manual discovery: the drivers probe, nothing is adopted, and the board's own agent
    /// is the one that decides what the board has.
    #[test]
    fn manual_discovery_hands_the_decision_to_the_agent() {
        let mut app = test_app();
        let auto = app.devices.len();
        assert!(auto > 0, "the virtual board reports a few devices");

        app.cfg.discovery = "manual".into();
        app.rebuild();
        assert_eq!(app.devices.len(), 0, "manual discovery adopts nothing on its own");
        assert!(app.probe.candidates.len() >= auto, "the drivers still saw them");
        let candidate = app.probe.candidates[0].clone();
        assert!(candidate["adopt"]["devices"].is_array(), "a candidate must say how to adopt it");

        // The exact body a candidate hands back is a valid manifest.
        let manifest = json!({
            "version": 1,
            "devices": candidate["adopt"]["devices"].clone(),
            "tools": [],
        });
        app.call_tool("manifest_apply", &json!({"manifest": manifest})).expect("adopting a candidate works");
        assert!(app.devices.len() >= 1, "the adopted device is live");
    }

    /// The reference is what makes the language usable: without it the model guesses the
    /// syntax from one paragraph and then asks for tools that do not exist.
    #[test]
    fn the_script_tool_ships_a_reference_in_sections_that_fit_a_tool_result() {
        let mut app = test_app();
        let overview = app.call_tool("script", &json!({"help": true})).expect("help");
        assert_eq!(overview["section"], "overview");
        let text = overview["text"].as_str().expect("text");
        for needle in ["while", "let", "tool(", "LIMITS"] {
            assert!(text.contains(needle), "overview is missing {needle}");
        }
        for section in ["overview", "builtins", "examples"] {
            let doc = app.call_tool("script", &json!({"section": section})).expect("section");
            let bytes = doc["text"].as_str().unwrap().len();
            assert!(bytes < 2048, "{section} is {bytes} bytes; a tool result is capped at 2 KB");
        }
        assert!(app.call_tool("script", &json!({"section": "nope"})).is_err());
        // An empty call is a help request too, not a confusing "source required".
        assert_eq!(app.call_tool("script", &json!({})).expect("empty is help")["section"], "overview");
    }

    /// A script is how the model writes the loop the tool vocabulary cannot express — and
    /// `tool()` inside it goes through the same dispatcher, so it cannot reach further than
    /// the caller.
    /// Opening the sheet always lands on Devices, whatever was open last time.
    #[test]
    fn the_board_sheet_opens_on_devices() {
        assert!(
            crate::web::INDEX_HTML.contains("data-panel=\"devices\" aria-selected=\"true\""),
            "devices is the selected tab in the markup"
        );
        assert!(
            crate::web::APP_JS.contains("showPanel(\"devices\")"),
            "opening the sheet selects devices, it does not remember"
        );
        assert!(!crate::web::APP_JS.contains("showPanel(S.panel)"), "no restore of the last tab");
    }

    /// `minimal` is the answer to "why are there still 27 tools on an empty board": it
    /// hides the nine that are conveniences, and keeps everything an empty board needs to
    /// rebuild itself.
    #[test]
    fn the_minimal_profile_hides_shortcuts_and_keeps_the_ability_to_rebuild() {
        let mut app = test_app();
        let full = app.tool_specs().as_array().unwrap().len();
        for hidden in App::MINIMAL_HIDDEN {
            assert!(app.is_visible(hidden), "{hidden} should be visible in full");
        }

        app.cfg.mcp.profile = "minimal".into();
        let minimal = app.tool_specs().as_array().unwrap().len();
        assert_eq!(minimal + App::MINIMAL_HIDDEN.len(), full, "nothing else moved");
        for hidden in App::MINIMAL_HIDDEN {
            assert!(!app.is_visible(hidden), "{hidden} should be hidden in minimal");
        }
        // The bootstrap survives: discovery, manifest, GPIO and the file tools.
        for kept in ["board", "devices_list", "devices_probe", "bus", "manifest"] {
            assert!(app.is_visible(kept), "{kept} must survive the minimal profile");
        }
        // `fs_*`, `net_*` and `script` live outside the registry, so they are checked in the
        // model's own tool list rather than through `is_visible`.
        let names: Vec<String> = app
            .tool_specs()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap_or("").to_string())
            .collect();
        for kept in ["shell", "script"] {
            assert!(names.iter().any(|n| n == kept), "{kept} must survive the minimal profile");
        }

        // And it is a config field, not a build: one patch brings them back.
        let touched = app.cfg.apply_patch(&json!({"mcp": {"profile": "full"}})).expect("patch");
        assert!(touched.contains(&"mcp.profile".to_string()));
        assert_eq!(app.tool_specs().as_array().unwrap().len(), full);
        assert!(app.cfg.apply_patch(&json!({"mcp": {"profile": "tiny"}})).is_err());
    }

    /// Linting is how a model checks a script before it runs: `tool()` calls have side
    /// effects, so "execute and see" is the worst way to find a typo.
    #[test]
    fn the_linter_answers_before_anything_runs() {
        let mut app = test_app();

        let good = app
            .call_tool("script", &json!({"lint": true, "source": "let i = 0\nwhile i < 3 { i = i + 1 }\ni"}))
            .expect("lint");
        assert_eq!(good["ok"], true);
        assert!(good["errors"].as_array().unwrap().is_empty());

        let bad = app.call_tool("script", &json!({"lint": true, "source": "let = 3"})).expect("lint");
        assert_eq!(bad["ok"], false);
        let first = bad["errors"][0].as_str().expect("an error message");
        assert!(first.contains("line 1"), "an error without a position is a guess: {first}");
        assert!(first.contains("column"), "{first}");

        // A call to a function that does not exist is a warning, not an error: it runs and
        // fails at that line, and the model should see the typo before that happens.
        let typo = app
            .call_tool("script", &json!({"lint": true, "source": "speaker_ton({freq_hz: 440})"}))
            .expect("lint");
        assert_eq!(typo["ok"], true);
        assert!(typo["warnings"][0].as_str().unwrap().contains("speaker_ton"));
        assert!(typo["warnings"][0].as_str().unwrap().contains("builtins"));

        // And linting never runs the program: a tool call would have been logged.
        let before = app.runtime.tool_calls.len();
        app.call_tool("script", &json!({"lint": true, "source": "tool(\"reboot\", {})"})).expect("lint");
        assert_eq!(app.runtime.tool_calls.len(), before, "lint must not execute anything");
        assert!(!app.runtime.restart_requested);
    }

    #[test]
    fn a_script_can_drive_the_boards_tools() {
        let mut app = test_app();
        let out = app
            .call_tool(
                "script",
                &json!({"source": "let d = tool(\"devices_list\", {})\nlet n = d.count\nlet i = 0\nwhile i < n { i = i + 1 }\nlog(\"devices: \" + str(n))\ni"}),
            )
            .expect("script runs");
        assert_eq!(out["ok"], true);
        assert!(out["steps"].as_u64().unwrap_or(0) > 0);
        assert!(out["output"][0].as_str().unwrap().starts_with("devices: "));
        assert_eq!(out["result"], json!(app.devices.len() as i64));

        // A script that tries to recurse is refused rather than nesting until the stack dies.
        let err = app.call_tool("script", &json!({"source": "tool(\"script\", {})"}));
        assert!(err.is_err());
    }

    /// A `net_get` aimed at the board itself has to be answered, not deadlocked.
    ///
    /// The tool runs on the thread that would answer the connection, so a real socket would
    /// wait for itself: measured on the bench as a 45 s timeout. It is served in process now,
    /// and the check is the same JSON a socket would have carried.
    #[test]
    fn asking_the_board_for_its_own_healthz_answers_in_process() {
        let mut app = test_app();
        let started = std::time::Instant::now();
        // The path MCP and the on-board agent both take for `net_*` is `ToolHost`.
        let url = format!("http://127.0.0.1:{}/healthz", app.cfg.mcp.port);
        let out = ToolHost::call_tool(&mut app, "net_get", &json!({"url": url}))
            .expect("a self request is answered");
        let body: Value = serde_json::from_str(&out).expect("json");
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(1), "took {took:?}");
        assert_eq!(body["status"], 200);
        assert_eq!(body["in_process"], true);
        assert!(body["body"].as_str().unwrap().contains("ok"));

        // A different port is somebody else's service: it goes to the transport.
        let other = ToolHost::call_tool(&mut app, "net_get", &json!({"url": "http://127.0.0.1:9/healthz"}));
        assert!(other.is_err(), "port 9 is not us");
    }

    #[test]
    fn a_message_is_queued_and_reads_back_from_the_transcript() {
        let mut app = test_app();
        let resp = app.route(&post("/api/agent/message", r#"{"text":"how warm is it?"}"#));
        assert_eq!(resp.status, 202);
        let v = body_json(&resp);
        assert_eq!(v["queued"], true);
        assert_eq!(v["seq"], 1, "the index the run will append at");

        // The queued turn shows up at once, flagged, so the page is never blank
        // between "sent" and "the model answered".
        let hist = body_json(&app.route(&get("/api/agent/history")));
        let msgs = hist["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "got {hist}");
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["text"], "how warm is it?");
        assert_eq!(msgs[0]["pending"], true);
        assert_eq!(hist["seq"], 0, "nothing is committed until the loop runs");

        // `after` is a history index: three committed turns, one after index 2.
        app.agent.reset();
        for i in 0..3 {
            app.agent.history.push(crate::agent::Message::user(&format!("m{i}"), "test", 0));
        }
        let all = body_json(&app.route(&get("/api/agent/history")));
        assert_eq!(all["messages"].as_array().unwrap().len(), 3, "got {all}");
        let tail = body_json(&app.route(&get("/api/agent/history?after=2")));
        assert_eq!(tail["messages"].as_array().unwrap().len(), 1, "got {tail}");
        assert_eq!(tail["messages"][0]["text"], "m2");
        assert_eq!(tail["seq"], 3);

        let mut fresh = test_app();
        let reset = body_json(&fresh.route(&post("/api/agent/reset", "{}")));
        assert_eq!(reset["reset"], true);
        assert!(fresh.agent.history.is_empty() && fresh.agent.pending.is_none());

        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"   "}"#)).status, 400);
        assert_eq!(app.route(&post("/api/agent/message", r#"{}"#)).status, 400);
    }

    /// An attachment is carried, named to the model, and served back — nothing more and
    /// nothing less. The board has no way to show an image and this model cannot see one,
    /// so pretending otherwise would be the bug.
    #[test]
    fn an_attachment_reaches_the_model_as_a_named_file() {
        let mut app = test_app();
        let body = json!({
            "text": "what is in this photo?",
            "attachments": [{
                "name": "shot.jpg",
                "mime": "image/jpeg",
                "data_b64": "aGVsbG8=",
            }],
        });
        let resp = app.route(&post("/api/agent/message", &body.to_string()));
        assert_eq!(resp.status, 202, "{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(app.attachments.len(), 1);
        assert_eq!(app.attachments[0].data_b64, "aGVsbG8=");
        let queued = app.agent.pending.as_ref().expect("a queued turn");
        assert_eq!(queued.attachments.len(), 1, "the message carries the image");
        assert_eq!(queued.attachments[0].mime, "image/jpeg");

        // The model is told what arrived, because it cannot look at it.
        let text = app.agent.pending.as_ref().expect("a queued turn").text.clone();
        assert!(text.contains("attached: shot.jpg"), "{text}");
        assert!(text.contains("image/jpeg"), "{text}");

        // And it can be fetched back by whoever can use the bytes.
        let got = app.route(&get("/api/attachment/shot.jpg"));
        assert_eq!(got.status, 200);
        assert_eq!(got.body, b"hello");
        assert!(
            got.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v == "image/jpeg"),
            "{:?}",
            got.headers
        );

        // Too large is refused rather than truncated.
        let big = "A".repeat((MAX_ATTACHMENT_BYTES / 3 + 8) * 4);
        let resp = app.route(&post(
            "/api/agent/message",
            &json!({"text":"x","attachments":[{"name":"big.bin","mime":"x/y","data_b64":big}]}).to_string(),
        ));
        assert_eq!(resp.status, 413);
    }

    /// The standing orders are part of the system prompt, writable from the shell and
    /// from HTTP, and they disappear when cleared.
    #[test]
    fn the_standing_orders_reach_the_model_and_can_be_edited() {
        let mut app = test_app();
        ToolHost::call_tool(
            &mut app,
            "shell",
            &json!({"command": "system_prompt set the pump is on GPIO12; never run it past two minutes"}),
        )
        .expect("set");

        assert_eq!(
            ToolHost::custom_prompt(&app).as_deref(),
            Some("the pump is on GPIO12; never run it past two minutes")
        );

        // It is in the prompt the model receives, under its own heading.
        let prompt = body_json(&app.route(&get("/api/agent/prompt")));
        let system = prompt["system"].as_str().unwrap_or_default();
        assert!(system.contains("Standing orders for this board"), "{system}");
        assert!(system.contains("never run it past two minutes"), "{system}");

        // The HTTP route appends and clears, for an agent that is not on the board.
        let added = app
            .route(&post("/api/agent/prompt", r#"{"action":"append","text":"the LED is a status light"}"#));
        assert_eq!(added.status, 200);
        let value = body_json(&added);
        assert!(value["system_prompt"].as_str().unwrap().contains("status light"), "{value}");
        assert!(value["system_prompt"].as_str().unwrap().contains("GPIO12"), "{value}");

        let cleared = app.route(&post("/api/agent/prompt", r#"{"action":"clear"}"#));
        assert_eq!(cleared.status, 200);
        assert_eq!(ToolHost::custom_prompt(&app).as_deref(), Some(""));
    }

    /// Hashing is a command, and the digest is the one a `sha256sum` on the host prints.
    #[test]
    fn nsh_hashes_a_file_the_way_sha256sum_does() {
        let mut app = test_app();
        ToolHost::call_tool(&mut app, "write", &json!({"path":"/h.txt","text":"abc"})).expect("write");
        let out = ToolHost::call_tool(&mut app, "shell", &json!({"command": "sha256sum /h.txt"}))
            .expect("sha256sum");
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            value["sha256"],
            json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        let md5 =
            ToolHost::call_tool(&mut app, "shell", &json!({"command": "md5sum /h.txt"})).expect("md5sum");
        let value: Value = serde_json::from_str(&md5).unwrap();
        assert_eq!(value["md5"], json!("900150983cd24fb0d6963f7d28e17f72"));
    }

    /// A model that gets a command wrong should read the correction in the error, and
    /// `--help` should answer the same lines without running anything.
    #[test]
    fn shell_errors_carry_the_usage_line() {
        let mut app = test_app();
        let queued = |app: &mut App, args: Value| ToolHost::call_tool(app, "shell", &args);

        // Missing `confirm`: the error says how to write it.
        let err = queued(&mut app, json!({"command":"rm /x"})).unwrap_err();
        assert!(err.contains("usage: rm <path> --confirm"), "{err}");

        // An operator nobody has lists the ones that exist.
        let err = queued(&mut app, json!({"command":"sudo"})).unwrap_err();
        assert!(err.contains("ls [path]"), "{err}");

        // `help` answers without doing anything.
        let help: Value =
            serde_json::from_str(&queued(&mut app, json!({"command":"help"})).unwrap()).unwrap();
        assert!(help["help"].as_str().unwrap().contains("unzip <archive_b64>"), "{help}");
        let one: Value =
            serde_json::from_str(&queued(&mut app, json!({"command":"cp -h"})).unwrap()).unwrap();
        assert!(one["help"].as_str().unwrap().starts_with("usage: cp <from> <to>"), "{one}");

        // A standalone tool does the same.
        let err = app.call_tool("read", &json!({"path":"/nope.txt"})).unwrap_err();
        assert!(err.message.contains("usage: read <path>"), "{}", err.message);
        let help = app.call_tool("ping", &json!({"help":true})).unwrap();
        assert_eq!(help["help"], json!("usage: ping <host> [-c <count>]"));
    }

    // ---- the commands added to grow `nsh` past the original set (docs/shell.md) ----

    /// One shell command, with its JSON answer. Every command answers with JSON text, so a
    /// test reads fields instead of substrings wherever the shape is the thing under test.
    fn sh(app: &mut App, line: &str) -> Value {
        let out = ToolHost::call_tool(app, "shell", &json!({ "command": line }))
            .unwrap_or_else(|e| panic!("`{line}` failed: {e}"));
        serde_json::from_str(&out).unwrap_or_else(|_| json!({ "text": out }))
    }

    fn write_file(app: &mut App, path: &str, text: &str) {
        ToolHost::call_tool(app, "write", &json!({"path": path, "text": text})).expect("write");
    }

    /// `cat` is `read` under the name a person types, and a missing file says so with the
    /// usage line attached.
    #[test]
    fn cat_answers_what_read_answers() {
        let mut app = test_app();
        write_file(&mut app, "/note.txt", "a note\n");
        let cat = sh(&mut app, "cat /note.txt");
        assert_eq!(cat["text"], json!("a note\n"));
        let read = ToolHost::call_tool(&mut app, "read", &json!({"path": "/note.txt"})).expect("read");
        let read: Value = serde_json::from_str(&read).unwrap();
        assert_eq!(read["text"], cat["text"], "the same file, the same answer");
        let err = ToolHost::call_tool(&mut app, "shell", &json!({"command": "cat /nope"})).unwrap_err();
        assert!(err.contains("usage: cat [path]"), "{err}");
    }

    #[test]
    fn wc_counts_lines_words_and_bytes() {
        let mut app = test_app();
        write_file(&mut app, "/w.txt", "one two\nthree\n");
        let all = sh(&mut app, "wc /w.txt");
        assert_eq!(all["lines"], json!(2), "{all}");
        assert_eq!(all["words"], json!(3), "{all}");
        assert_eq!(all["bytes"], json!(14), "{all}");
        assert_eq!(sh(&mut app, "wc -l /w.txt")["count"], json!(2));
        assert_eq!(sh(&mut app, "wc -w /w.txt")["count"], json!(3));
        assert_eq!(sh(&mut app, "wc -c /w.txt")["count"], json!(14), "-c is the path too");
    }

    #[test]
    fn head_takes_the_first_lines() {
        let mut app = test_app();
        write_file(&mut app, "/h.txt", "1\n2\n3\n4\n");
        let out = sh(&mut app, "head -n 2 /h.txt");
        assert_eq!(out["text"], json!("1\n2"), "{out}");
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(sh(&mut app, "head /h.txt")["lines"], json!(4), "default keeps everything that fits");
    }

    #[test]
    fn tail_takes_the_last_lines() {
        let mut app = test_app();
        write_file(&mut app, "/t.txt", "1\n2\n3\n4\n");
        let out = sh(&mut app, "tail -n 2 /t.txt");
        assert_eq!(out["text"], json!("3\n4"), "{out}");
        assert_eq!(out["total_lines"], json!(4));
    }

    #[test]
    fn cut_takes_one_column() {
        let mut app = test_app();
        write_file(&mut app, "/c.csv", "a,1\nb,2\nc\n");
        let out = sh(&mut app, "cut -d , -f 2 /c.csv");
        assert_eq!(out["text"], json!("1\n2"), "{out}");
        assert_eq!(out["lines_without_that_field"], json!(1));
        // No `-d` means a tab, as everywhere else.
        write_file(&mut app, "/c.tsv", "x\ty\n");
        assert_eq!(sh(&mut app, "cut -f 2 /c.tsv")["text"], json!("y"));
        let err =
            ToolHost::call_tool(&mut app, "shell", &json!({"command": "cut -d , -f 0 /c.csv"})).unwrap_err();
        assert!(err.contains("numbered from 1"), "{err}");
    }

    #[test]
    fn sort_orders_and_optionally_collapses() {
        let mut app = test_app();
        write_file(&mut app, "/s.txt", "b\na\nb\n");
        assert_eq!(sh(&mut app, "sort /s.txt")["text"], json!("a\nb\nb"));
        assert_eq!(sh(&mut app, "sort -r /s.txt")["text"], json!("b\nb\na"));
        let unique = sh(&mut app, "sort -u /s.txt");
        assert_eq!(unique["text"], json!("a\nb"));
        assert_eq!(unique["duplicates_removed"], json!(1));
    }

    #[test]
    fn uniq_collapses_adjacent_repeats_only() {
        let mut app = test_app();
        write_file(&mut app, "/u.txt", "a\na\nb\na\n");
        let out = sh(&mut app, "uniq /u.txt");
        assert_eq!(out["text"], json!("a\nb\na"), "only neighbours collapse");
        assert_eq!(out["removed"], json!(1));
    }

    #[test]
    fn tr_maps_characters_one_for_one() {
        let mut app = test_app();
        write_file(&mut app, "/tr.txt", "abc\n");
        let out = sh(&mut app, "tr a x /tr.txt");
        assert_eq!(out["text"], json!("xbc\n"));
        assert_eq!(out["chars_changed"], json!(1));
        write_file(&mut app, "/lower.txt", "abc\n");
        let upper = sh(&mut app, "tr abc XYZ /lower.txt");
        assert_eq!(upper["text"], json!("XYZ\n"));
        // A range would be three characters here, and the answer says so rather than lying.
        assert!(upper["note"].as_str().unwrap().contains("not a range"), "{upper}");
    }

    /// `sed`: one substitution per line, and a script that is not one is refused rather
    /// than guessed at.
    #[test]
    fn sed_replaces_one_expression_per_line() {
        let mut app = test_app();
        write_file(&mut app, "/s.txt", "error: one\nerror: two\nfine\n");
        let first = sh(&mut app, "sed s/error/ERR/ /s.txt");
        assert_eq!(first["text"], json!("ERR: one\nERR: two\nfine"), "{first}");
        assert_eq!(first["changed_lines"], json!(2));
        assert_eq!(first["replacements"], json!(2));

        // `g` takes every match on a line; without it, only the first.
        write_file(&mut app, "/g.txt", "a a a\n");
        assert_eq!(sh(&mut app, "sed s/a/b/g /g.txt")["text"], json!("b b b"));
        assert_eq!(sh(&mut app, "sed s/a/b/ /g.txt")["text"], json!("b a a"));

        // A delimiter that is not `/` works, and a literal one can be escaped. (`|` cannot:
        // the shell splits the command line on it before `sed` ever sees the script.)
        write_file(&mut app, "/p.txt", "a/b\n");
        assert_eq!(sh(&mut app, "sed s,/,:, /p.txt")["text"], json!("a:b"), "comma delimiter");

        // The refusals are the interesting part: each names what is not implemented.
        for (script, needle) in [
            ("s/a", "both sides"),
            ("s//b/", "empty `from`"),
            ("s/a/b/x", "not a flag"),
            ("d", "not a substitution"),
        ] {
            let err =
                ToolHost::call_tool(&mut app, "shell", &json!({"command": format!("sed {script} /s.txt")}))
                    .unwrap_err();
            assert!(err.contains(needle), "`{script}` gave: {err}");
        }
    }

    /// `awk`: `$0`, `$N` and `$NF`, whitespace or `-F`; anything else is refused with the
    /// reason, because a half-`awk` that quietly returns nothing is worse than an error.
    #[test]
    fn awk_prints_fields_and_refuses_the_rest_of_the_language() {
        let mut app = test_app();
        write_file(&mut app, "/log.txt", "12:01 up 3 days\n12:02 up 3 days\n");
        let second = sh(&mut app, "awk '{print $2}' /log.txt");
        assert_eq!(second["text"], json!("up\nup"), "{second}");
        assert_eq!(second["lines"], json!(2));

        // Two fields, the last one, and the whole line.
        assert_eq!(sh(&mut app, "awk '{print $1, $NF}' /log.txt")["text"], json!("12:01 days\n12:02 days"));
        assert_eq!(sh(&mut app, "awk '{print $NF, $1}' /log.txt")["text"], json!("days 12:01\ndays 12:02"));
        assert_eq!(
            sh(&mut app, "awk '{print $0}' /log.txt")["text"],
            json!("12:01 up 3 days\n12:02 up 3 days")
        );

        // A separator, as `--sep ,`.
        write_file(&mut app, "/csv.txt", "a,1\nb,2\n");
        assert_eq!(sh(&mut app, "awk '{print $2}' --sep , /csv.txt")["text"], json!("1\n2"));

        // Out of range prints an empty field, as awk does, rather than dropping the line.
        write_file(&mut app, "/short.txt", "only\n");
        assert_eq!(sh(&mut app, "awk '{print $3}' /short.txt")["text"], json!(""));

        for (program, needle) in
            [("{print 1}", "not a field"), ("$1", "`{print ...}`"), ("{sum += $1}", "only `print`")]
        {
            let err = ToolHost::call_tool(
                &mut app,
                "shell",
                &json!({"command": format!("awk '{program}' /log.txt")}),
            )
            .unwrap_err();
            assert!(err.contains(needle), "`{program}` gave: {err}");
        }
    }

    /// `grep -E`: the regex subset, reached through the shell, with the parts outside it
    /// refused rather than matched differently.
    #[test]
    fn grep_e_matches_a_regex_and_refuses_the_rest_of_the_language() {
        let mut app = test_app();
        write_file(&mut app, "/app.log", "ERR: disk full\nok\nWARN: disk 90%\nERROR: disk\n");
        let anchored = sh(&mut app, "grep -E '^ERR' /app.log");
        assert_eq!(anchored["count"], json!(2), "{anchored}");
        assert_eq!(anchored["mode"], json!("regex"));

        // A class and a quantifier: the digits after `disk `.
        write_file(&mut app, "/pct.txt", "disk 90%\ndisk 7%\ndisk full\n");
        let digits = sh(&mut app, "grep -E 'disk [0-9]+%' /pct.txt");
        assert_eq!(digits["count"], json!(2), "{digits}");

        // Without `-E` the same pattern is a literal, and matches nothing — which is the
        // difference the flag is for.
        assert_eq!(sh(&mut app, "grep '^ERR' /app.log")["count"], json!(0));

        // `$` binds to the end of the line, and `\d` is a digit.
        assert_eq!(sh(&mut app, "grep -E 'full$' /app.log")["count"], json!(1));

        let err =
            ToolHost::call_tool(&mut app, "shell", &json!({"command": "grep -E '(ERR|WARN)' /app.log"}))
                .unwrap_err();
        assert!(err.contains("not supported"), "groups must be refused: {err}");
    }

    /// The shell declaration carries two fields, and everything the index used to say is a
    /// `help` call away.
    ///
    /// The command list was in the description, which meant 57 usage lines were sent on every
    /// step of every run — 2 KB to describe a tool whose whole interface is one string. The
    /// trade is one extra call the first time; the test makes sure both halves hold: the
    /// declaration stays small, and `help` still answers with the index.
    #[test]
    fn the_shell_declaration_is_one_string_and_a_help_flag() {
        let spec = crate::tools::shell::spec();
        let props = spec.spec.input_schema["properties"].as_object().expect("properties");
        let mut keys: Vec<&str> = props.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["command", "help"], "the declaration grew a field back");

        let mut app = test_app();
        let help =
            ToolHost::call_tool(&mut app, "shell", &json!({ "help": true })).expect("help must answer");
        let text = serde_json::to_string(&help).unwrap();
        for op in ["sed", "awk", "grep", "jq", "curl"] {
            assert!(text.contains(op), "`{op}` is missing from the index help returns");
        }
        assert!(text.len() > 1_000, "the index is still the whole index: {} bytes", text.len());
    }

    /// The prompt may only name tools the model is offered, and may not advertise an alias.
    ///
    /// This is the fourth of its family — the portal listing a tool the model did not have,
    /// an image claiming a commit it was not built from, a status route reporting a tool
    /// count nobody could reconcile — and the answer each time is the same: pin the two
    /// things together and let a test be the one that notices. The prompt told a model to
    /// call `board_status`, `fs_read` and `net_get` while the list offered `board`, `read`
    /// and `shell`.
    ///
    /// Both halves are reverse-provable: a name that is not offered makes the first
    /// assertion fail, and so does putting a deprecated alias back in backticks.
    #[test]
    fn the_prompt_names_only_tools_the_model_is_offered() {
        let mut app = test_app();
        let prompt = app.agent.prompt_text(&app).clone();
        let offered: Vec<String> = ToolHost::tool_specs(&app)
            .as_array()
            .expect("array")
            .iter()
            .map(|t| t["function"]["name"].as_str().expect("a name").to_string())
            .collect();

        let stray = unadvertised_tool_names(&prompt, &offered);
        assert!(stray.is_empty(), "the prompt names {stray:?}, which the model is not offered");

        // The aliases still answer; they are just not advertised. `reboot` in the sentence
        // "you cannot reboot the board" is prose, so only backticked tokens count.
        for token in backticked_tool_tokens(&prompt) {
            assert!(
                !App::ALIASES_ACCEPTED_BUT_NOT_ADVERTISED.contains(&token.as_str()),
                "`{token}` is a deprecated alias and must not be advertised in the prompt"
            );
        }

        // Reverse proof, without touching the build: one invented name is caught.
        let mut injected = prompt.clone();
        injected.push_str(" Then call `frobnicate`.");
        assert_eq!(unadvertised_tool_names(&injected, &offered), vec!["frobnicate".to_string()]);
    }

    /// `reboot` is not something a model is given: it would restart the board in the middle of
    /// the run that asked for it. It is not a command of `board` either, and the listing says
    /// where it went; a person still restarts from the portal, behind the PIN.
    #[test]
    fn a_model_is_not_offered_a_restart_but_a_person_still_has_one() {
        let mut app = test_app();

        // Not in the family, and asking the family for it is an unknown command, not a run.
        let help = app.call_tool("board", &json!({ "command": "help" })).unwrap();
        let commands: Vec<String> =
            help["commands"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
        assert!(!commands.contains(&"reboot".to_string()), "{commands:?}");
        let err = app.call_tool("board", &json!({ "command": "reboot" })).unwrap_err();
        assert!(err.message.contains("no command"), "{err:?}");

        // Not offered as a tool of its own either, and the reason is a sentence.
        assert!(!app.is_visible("reboot"));
        let why = app
            .hidden_tools()
            .into_iter()
            .find(|t| t["name"] == "reboot")
            .expect("`reboot` must be reported as hidden");
        assert!(why["why"].as_str().unwrap_or("").contains("person"), "{why}");

        // And it is refused, with the sentence a script author needs: this is their mistake,
        // not a model's, and a bare "no such tool" would leave them guessing.
        let err = app.call_tool("reboot", &json!({})).unwrap_err();
        assert_eq!(err.code, code::FORBIDDEN, "{err:?}");
        for needle in ["restart", "portal", "/api/reboot"] {
            assert!(err.message.contains(needle), "`{needle}` missing from: {}", err.message);
        }
        assert_eq!(err.data.as_ref().and_then(|d| d["restart_route"].as_str()), Some("/api/reboot"));

        // Through a script too: a stored `tool("reboot", {})` is exactly the caller this
        // sentence exists for.
        let scripted = app.call_tool("script", &json!({ "source": "tool(\"reboot\", {})" }));
        let text = match scripted {
            Ok(value) => value.to_string(),
            Err(error) => error.message,
        };
        assert!(text.contains("/api/reboot"), "a script must be told the same thing: {text}");

        // The portal's button is a route, not a tool: it keeps working.
        let resp = app.route(&post("/api/reboot", "{}"));
        assert_eq!(resp.status, 200, "the portal's restart must still answer");
        assert!(app.runtime.restart_requested, "the platform must be told to restart");
    }

    /// "How much RAM is free" has two answers on a board with PSRAM, and the status tool
    /// used to give only the big one: asked in a real turn, the model read `free_heap_kb`
    /// (7 864 kB, nearly all PSRAM) and answered "7 919 KB of free internal heap". The
    /// numbers a stack, a socket or a TLS record has to fit in were not in the answer at all.
    #[test]
    fn board_status_names_the_internal_ram_and_agrees_with_the_pools() {
        let mut app = test_app();
        let status = app.call_tool("board", &json!({ "command": "status" })).unwrap();

        let internal_kb =
            status["internal_kb"].as_u64().unwrap_or_else(|| panic!("no internal_kb in {status}"));
        let largest_kb = status["largest_internal_kb"]
            .as_u64()
            .unwrap_or_else(|| panic!("no largest_internal_kb in {status}"));
        assert!(
            status["free_heap_kb"].as_u64().unwrap() >= internal_kb,
            "the total cannot be smaller than the part: {status}"
        );
        assert!(largest_kb <= internal_kb, "a block cannot exceed the pool: {status}");

        // One source of truth: the same pool `/api/memory` lists, not a second measurement.
        let (free, largest) = app.hw.heap_internal().expect("the board names its pools");
        assert_eq!(internal_kb, (free / 1024) as u64, "{status}");
        assert_eq!(largest_kb, (largest / 1024) as u64, "{status}");

        // And `free` says the same thing, split by pool rather than explained in a note.
        let free_out = app.call_tool("shell", &json!({ "command": "free" })).unwrap();
        let text = free_out["stdout"].as_str().unwrap_or(&free_out.to_string()).to_string();
        assert!(text.contains("internal_8bit_free_kb"), "{text}");
        assert!(text.contains("spiram_free_kb"), "{text}");
    }

    #[test]
    fn tee_writes_the_text_it_is_given() {
        let mut app = test_app();
        let out = sh(&mut app, "tee /teed.txt -i hello");
        assert_eq!(out["text"], json!("hello"));
        assert_eq!(out["bytes"], json!(5));
        assert_eq!(sh(&mut app, "cat /teed.txt")["text"], json!("hello"));
        let err = ToolHost::call_tool(&mut app, "shell", &json!({"command": "tee /teed.txt"})).unwrap_err();
        assert!(err.contains("usage: tee <path>"), "{err}");
    }

    #[test]
    fn touch_creates_a_file_and_admits_it_has_no_mtime() {
        let mut app = test_app();
        let first = sh(&mut app, "touch /new.txt");
        assert_eq!(first["created"], json!(true));
        assert_eq!(first["mtime"], Value::Null, "the workspace keeps no timestamps: {first}");
        let second = sh(&mut app, "touch /new.txt");
        assert_eq!(second["created"], json!(false));
        assert!(second["note"].as_str().unwrap().contains("no mtime to update"), "{second}");
    }

    #[test]
    fn du_totals_the_workspace() {
        let mut app = empty_workspace_app();
        write_file(&mut app, "/a.txt", "12345");
        write_file(&mut app, "/b.txt", "123");
        let out = sh(&mut app, "du");
        assert_eq!(out["count"], json!(2), "{out}");
        assert_eq!(out["bytes"], json!(8));
        assert_eq!(out["files"][0]["path"], json!("/a.txt"), "biggest first: {out}");
        assert_eq!(out["limit_bytes"], json!(WORKSPACE_TOTAL_MAX));
    }

    #[test]
    fn strings_prints_the_printable_runs() {
        let mut app = test_app();
        write_file(&mut app, "/bin.dat", "abc\u{0}hello\u{0}world\u{0}");
        let out = sh(&mut app, "strings /bin.dat");
        assert_eq!(out["strings"], json!(["hello", "world"]), "runs of four or more: {out}");
        assert_eq!(out["min"], json!(4));
        assert_eq!(sh(&mut app, "strings -n 3 /bin.dat")["strings"], json!(["abc", "hello", "world"]));
    }

    #[test]
    fn od_is_the_octal_dump() {
        let mut app = test_app();
        write_file(&mut app, "/o.txt", "A");
        let out = sh(&mut app, "od /o.txt");
        let dump = out["hexdump"].as_str().unwrap();
        assert!(dump.starts_with("0000000  101"), "octal 101 is `A`: {dump}");
        assert_eq!(out["octal"], json!(true));
        let hex = sh(&mut app, "hexdump /o.txt");
        assert!(hex["hexdump"].as_str().unwrap().contains("41"), "{hex}");
        assert_eq!(hex["octal"], json!(false));
    }

    #[test]
    fn sleep_waits_and_says_the_wait_is_capped() {
        let mut app = test_app();
        let out = sh(&mut app, "sleep 0");
        assert_eq!(out["slept_ms"], json!(0));
        assert_eq!(out["clamped"], json!(false));
        // The cap is arithmetic, so it is tested without waiting ten seconds for it.
        assert_eq!(plan_sleep(false, "100").unwrap(), (100_000, MAX_SHELL_SLEEP_MS));
        assert!(plan_sleep(false, "5").is_ok());
        assert!(plan_sleep(true, "-1").is_err());
        let err = ToolHost::call_tool(&mut app, "shell", &json!({"command": "sleep soon"})).unwrap_err();
        assert!(err.contains("usage: sleep <seconds>"), "{err}");
    }

    #[test]
    fn usleep_waits_in_milliseconds() {
        let mut app = test_app();
        let out = sh(&mut app, "usleep 1");
        assert_eq!(out["slept_ms"], json!(1), "{out}");
        assert_eq!(out["requested_ms"], json!(1));
        assert_eq!(plan_sleep(true, "999999").unwrap(), (999_999, MAX_SHELL_SLEEP_MS));
    }

    #[test]
    fn jot_prints_a_sequence() {
        let mut app = test_app();
        assert_eq!(sh(&mut app, "jot 3")["text"], json!("1 2 3"));
        assert_eq!(sh(&mut app, "jot 3 10 -2")["text"], json!("10 8 6"), "a negative step counts down");
        assert_eq!(sh(&mut app, "jot 2 0 0.5")["text"], json!("0 0.5"));
        assert_eq!(sh(&mut app, "jot 500")["count"], json!(100), "bounded, like everything else here");
    }

    #[test]
    fn crc32_is_the_checksum_of_the_file() {
        let mut app = test_app();
        write_file(&mut app, "/crc.txt", "abc");
        let out = sh(&mut app, "crc32 /crc.txt");
        assert_eq!(out["crc32"], json!("352441c2"), "the IEEE CRC-32 of `abc`: {out}");
        assert_eq!(out["bytes"], json!(3));
    }

    #[test]
    fn hostname_reads_and_writes_the_name_the_network_sees() {
        let mut app = test_app();
        let set = sh(&mut app, "hostname bench-1");
        assert_eq!(set["hostname"], json!("bench-1"));
        assert_eq!(set["saved"], json!(true));
        assert_eq!(app.cfg.wifi.hostname, "bench-1");
        assert_eq!(sh(&mut app, "hostname")["hostname"], json!("bench-1"));
        let err =
            ToolHost::call_tool(&mut app, "shell", &json!({"command": "hostname 'two words'"})).unwrap_err();
        assert!(err.contains("usage: hostname [name]"), "{err}");
    }

    #[test]
    fn whoami_is_the_device_name() {
        let mut app = test_app();
        app.cfg.device_name = "bench board".to_string();
        let out = sh(&mut app, "whoami");
        assert_eq!(out["whoami"], json!("bench board"));
        assert!(out["note"].as_str().unwrap().contains("one account"), "{out}");
    }

    #[test]
    fn id_says_there_is_one_user_and_it_is_root() {
        let mut app = test_app();
        let out = sh(&mut app, "id");
        assert_eq!(out["uid"], json!(0));
        assert_eq!(out["gid"], json!(0));
        assert_eq!(out["groups"], json!([0]));
    }

    #[test]
    fn env_lists_the_session_variables() {
        let mut app = test_app();
        let out = sh(&mut app, "env");
        assert_eq!(out["count"], json!(0));
        assert_eq!(out["persisted"], json!(false));
        assert!(out["note"].as_str().unwrap().contains("never written to NVS"), "{out}");
    }

    /// The variables live in the `App` and nowhere else: a shell variable is state, not a
    /// setting, so writing it into the config would turn a typo into a boot-time fact.
    #[test]
    fn setenv_keeps_the_value_in_the_session_and_out_of_the_config() {
        let mut app = test_app();
        let out = sh(&mut app, "setenv MODE test");
        assert_eq!(out["key"], json!("MODE"));
        assert_eq!(out["persisted"], json!(false));
        assert_eq!(sh(&mut app, "env")["vars"]["MODE"], json!("test"));
        assert!(!app.cfg.to_json_string().contains("MODE"), "the config must not learn the variable");
        assert_eq!(app.store.get(crate::config::NVS_KEY_CONFIG).unwrap_or_default().contains("MODE"), false);
    }

    #[test]
    fn unsetenv_removes_the_variable() {
        let mut app = test_app();
        sh(&mut app, "setenv TEMP 1");
        let out = sh(&mut app, "unsetenv TEMP");
        assert_eq!(out["removed"], json!(true));
        assert_eq!(sh(&mut app, "env")["count"], json!(0));
        assert_eq!(sh(&mut app, "unsetenv TEMP")["removed"], json!(false), "twice is not an error");
    }

    /// `netstat` reports the sessions and counters this build keeps, and refuses to invent
    /// the socket table it cannot read.
    #[test]
    fn netstat_reports_sessions_and_the_listen_port() {
        let mut app = test_app();
        app.cfg.mcp.port = 8080;
        let out = sh(&mut app, "netstat");
        assert_eq!(out["listen"]["port"], json!(8080), "{out}");
        assert_eq!(out["session_count"], json!(0));
        assert!(out["sockets"].as_array().unwrap().is_empty());
        assert!(out["note"].as_str().unwrap().contains("lwIP"), "{out}");
        assert!(out["counters"]["http_requests"].is_number(), "{out}");
    }

    #[test]
    fn route_reports_the_gateway_and_where_it_came_from() {
        let mut app = test_app();
        let leased = sh(&mut app, "route");
        assert_eq!(leased["gateway"], Value::Null);
        assert_eq!(leased["source"], json!("DHCP lease"));
        app.cfg.wifi.gateway = "192.168.3.1".to_string();
        let configured = sh(&mut app, "route");
        assert_eq!(configured["gateway"], json!("192.168.3.1"));
        assert_eq!(configured["source"], json!("configured"));
    }

    /// The ARP table is not reachable from this build, and an empty table that says so is
    /// the honest answer — a made-up neighbour would be worse than nothing.
    #[test]
    fn arp_says_the_table_is_not_available() {
        let mut app = test_app();
        let out = sh(&mut app, "arp");
        assert_eq!(out["count"], json!(0));
        assert_eq!(out["available"], json!(false));
        assert!(out["entries"].as_array().unwrap().is_empty());
        assert!(out["note"].as_str().unwrap().contains("cannot see them, not because there are none"));
    }

    #[test]
    fn type_says_what_a_name_is() {
        let mut app = test_app();
        let command = sh(&mut app, "type ls");
        assert_eq!(command["kind"], json!("shell command"));
        assert_eq!(command["handler"], json!("fs_ls"));
        assert_eq!(command["usage"], json!("ls [path]"));
        let tool = sh(&mut app, "type read");
        assert_eq!(tool["kind"], json!("tool"), "read is a tool of its own: {tool}");
        assert_eq!(tool["tool"], json!(true));
        let nothing = sh(&mut app, "type sudo");
        assert_eq!(nothing["kind"], json!("nothing"));
        assert!(nothing["note"].as_str().unwrap().contains("no shell command"), "{nothing}");
    }

    #[test]
    fn which_says_where_a_name_is() {
        let mut app = test_app();
        let out = sh(&mut app, "which od");
        assert_eq!(out["kind"], json!("shell command"));
        assert_eq!(out["handler"], json!("shell_hexdump"), "od is the octal half of hexdump");
        assert_eq!(out.get("usage"), None, "which reports where, not how");
        assert!(out["looked_in"].as_str().unwrap().contains("no PATH"), "{out}");
    }

    /// `help` is generated from the same table every command routes through, so it cannot
    /// fall behind, and every command's usage line is reachable by getting it wrong.
    #[test]
    fn help_lists_every_command_and_every_usage_line_is_reachable() {
        let mut app = test_app();
        let help = sh(&mut app, "help")["help"].as_str().unwrap().to_string();
        for (op, _) in crate::tools::shell::OPS {
            let line = crate::tools::shell::usage(op).unwrap_or_else(|| panic!("`{op}` has no usage"));
            assert!(help.contains(line), "`{op}` is missing from help:\n{help}");
        }
        assert_eq!(
            help.lines().filter(|l| l.starts_with("  ")).count(),
            crate::tools::shell::OPS.len(),
            "one line per command"
        );

        // A bare command either runs or answers with its usage line — `{op}` with no
        // arguments is how a model gets there, and silence would teach it nothing.
        for (op, _) in crate::tools::shell::OPS {
            let line = crate::tools::shell::usage(op).unwrap();
            match ToolHost::call_tool(&mut app, "shell", &json!({ "command": op })) {
                Ok(_) => {}
                Err(e) => assert!(
                    e.contains(&format!("usage: {line}")),
                    "`{op}` failed without its usage line: {e}"
                ),
            }
        }
    }

    // ---- pipes and redirection ----

    /// One line, several stages: what the first prints is what the second reads.
    #[test]
    fn a_pipeline_feeds_one_stage_into_the_next() {
        let mut app = empty_workspace_app();
        write_file(&mut app, "/a.txt", "alpha\n");
        write_file(&mut app, "/b.log", "beta\n");

        let out = sh(&mut app, "ls / | grep txt");
        assert_eq!(out["pipeline"], json!(["ls", "grep"]), "{out}");
        assert_eq!(out["stdout"], json!("a.txt"), "{out}");

        // `wc` counts the lines the pipe carried, not the tree.
        assert_eq!(sh(&mut app, "ls / | wc -l")["stdout"], json!("2"));
        // Four stages, still one line. `head` hands on its lines without a trailing newline.
        let long = sh(&mut app, "ls / | sort -r | head -n 1 | wc -c");
        assert_eq!(long["stdout"], json!("5"), "`b.log` is five bytes: {long}");
    }

    /// `cat < file` is reading a file, `sort | head` is reading the pipe.
    #[test]
    fn input_redirection_reads_a_file() {
        let mut app = test_app();
        write_file(&mut app, "/in.txt", "one\ntwo\nthree\n");
        assert_eq!(sh(&mut app, "cat < /in.txt")["stdout"], json!("one\ntwo\nthree\n"));
        assert_eq!(sh(&mut app, "wc -l < /in.txt")["stdout"], json!("3"));
        assert_eq!(sh(&mut app, "sort < /in.txt | head -n 1")["stdout"], json!("one"));
        // A file that is not there is the same error it always is.
        let err = ToolHost::call_tool(&mut app, "shell", &json!({"command": "cat < /nope"})).unwrap_err();
        assert!(err.contains("/nope"), "{err}");
    }

    /// `>` truncates, `>>` appends, and both land in the workspace the file tools read.
    #[test]
    fn a_redirect_writes_and_appends() {
        let mut app = test_app();
        let out = sh(&mut app, "echo hi > /out.txt");
        assert_eq!(out["writes"][0]["path"], json!("/out.txt"), "{out}");
        assert_eq!(out["stdout"], json!(""), "a redirected stage sends nothing down the pipe");
        assert_eq!(sh(&mut app, "cat /out.txt")["text"], json!("hi\n"));
        assert_eq!(sh(&mut app, "wc -c /out.txt")["count"], json!(3));

        sh(&mut app, "echo there >> /out.txt");
        assert_eq!(sh(&mut app, "cat /out.txt")["text"], json!("hi\nthere\n"));

        // `>` starts again from nothing, and the file tools see the result.
        sh(&mut app, "echo again > /out.txt");
        assert_eq!(sh(&mut app, "cat /out.txt")["text"], json!("again\n"));
    }

    /// `grep` on the end of a pipe filters the text it is handed, by the rule `fs_grep` uses.
    #[test]
    fn grep_filters_what_the_pipe_carries() {
        let mut app = test_app();
        write_file(&mut app, "/log.txt", "boot ok\nerror: pump\nboot done\nerror: fan\n");
        assert_eq!(sh(&mut app, "cat /log.txt | grep error")["stdout"], json!("error: pump\nerror: fan"));
        assert_eq!(sh(&mut app, "cat /log.txt | grep error | wc -l")["stdout"], json!("2"));
        // Nothing matched is an empty answer, not an error.
        assert_eq!(sh(&mut app, "cat /log.txt | grep nothing")["stdout"], json!(""));
        // The same pattern, the same rule as the file tool.
        let found = sh(&mut app, "grep error /log.txt");
        assert_eq!(found["matches"].as_array().unwrap().len(), 2, "{found}");
    }

    /// A stage that cannot read stdin stops the pipeline and names itself.
    #[test]
    fn a_command_that_cannot_be_a_stage_says_so() {
        let mut app = empty_workspace_app();
        // Not a line command at all.
        let err = ToolHost::call_tool(&mut app, "shell", &json!({"command": "ls / | touch /x"})).unwrap_err();
        assert!(err.contains("`touch` cannot run in a pipeline"), "{err}");
        // A producer cannot be stage two: it has nothing to read.
        let err = ToolHost::call_tool(&mut app, "shell", &json!({"command": "echo hi | ls /"})).unwrap_err();
        assert!(err.contains("`ls` does not read stdin"), "{err}");
        // `script` is not in the table, so a pipeline is not a way to reach it.
        let err =
            ToolHost::call_tool(&mut app, "shell", &json!({"command": "echo hi | script"})).unwrap_err();
        assert!(err.contains("script"), "{err}");
        // Nothing ran, so nothing was written.
        assert!(
            app.workspace.to_json()["files"].as_object().unwrap().is_empty(),
            "a refused pipeline leaves no file"
        );
    }

    /// The stage limit is five, and it is enforced by the parser, not by hoping.
    #[test]
    fn a_pipeline_carries_five_stages_and_no_more() {
        let mut app = test_app();
        assert_eq!(sh(&mut app, "echo a | cat | cat | cat | cat")["stdout"], json!("a\n"));
        let err = ToolHost::call_tool(
            &mut app,
            "shell",
            &json!({"command": "echo a | cat | cat | cat | cat | cat"}),
        )
        .unwrap_err();
        assert!(err.contains("at most 5"), "{err}");
        assert!(err.contains("usage:"), "{err}");
    }

    /// The syntax that looks like a shell but is not one is refused, with the line that says
    /// how it *is* written — a trailing pipe included.
    #[test]
    fn broken_pipeline_syntax_is_refused_with_a_usage_line() {
        let mut app = test_app();
        for (line, needle) in [
            ("ls / |", "cannot end with `|`"),
            ("echo hi >", "needs a file name"),
            ("cat <", "needs a file name"),
            ("echo a && echo b", "&&"),
            ("echo $(date)", "$("),
        ] {
            let err = ToolHost::call_tool(&mut app, "shell", &json!({ "command": line })).unwrap_err();
            assert!(err.contains(needle), "`{line}`: {err}");
            assert!(err.contains("usage:"), "`{line}` has no usage line: {err}");
        }
        // A pipe inside quotes is one word, which is how a pattern containing one is written.
        let quoted = sh(&mut app, "echo \"a | b\"");
        assert_eq!(quoted["text"], json!("a | b"), "{quoted}");
        assert_eq!(quoted["pipeline"], Value::Null, "a quoted pipe is not a pipeline");
    }

    /// `echo` is the command a redirect is written for.
    #[test]
    fn echo_joins_its_words() {
        let mut app = test_app();
        assert_eq!(sh(&mut app, "echo hello world")["text"], json!("hello world"));
        assert_eq!(sh(&mut app, "echo hello world")["stdout"], json!("hello world\n"));
        // `-n` leaves the newline off, which is what builds a line with `>>`.
        let out = sh(&mut app, "echo -n no-newline > /n.txt");
        assert_eq!(out["writes"][0]["bytes"], json!(10), "{out}");
        assert_eq!(sh(&mut app, "cat /n.txt")["text"], json!("no-newline"));
    }

    /// `tee` in a pipeline copies and passes on; on its own `-i` is what a pipe would be.
    #[test]
    fn tee_copies_the_pipe_and_passes_it_on() {
        let mut app = test_app();
        write_file(&mut app, "/in.txt", "hello\n");
        let out = sh(&mut app, "cat /in.txt | tee /copy.txt | wc -c");
        assert_eq!(out["stdout"], json!("6"), "{out}");
        assert_eq!(sh(&mut app, "cat /copy.txt")["text"], json!("hello\n"));
        assert_eq!(sh(&mut app, "cat /copy.txt | tee /second.txt")["stdout"], json!("hello\n"));
        assert_eq!(sh(&mut app, "cat /second.txt")["text"], json!("hello\n"));
    }

    /// The 8 KB workspace is the same limit a redirect has to respect, and a refusal says
    /// how many bytes it wanted.
    #[test]
    fn a_redirect_that_would_not_fit_says_how_much_it_needed() {
        let mut app = empty_workspace_app();
        let big = "x".repeat(WORKSPACE_FILE_MAX + 10);
        let err =
            ToolHost::call_tool(&mut app, "shell", &json!({"command": format!("echo {big} > /big.txt")}))
                .unwrap_err();
        assert!(err.contains(&format!("{} bytes", WORKSPACE_FILE_MAX + 11)), "{err}");
        assert!(err.contains(&format!("at most {WORKSPACE_FILE_MAX}")), "{err}");
        assert!(app.workspace.to_json()["files"].as_object().unwrap().is_empty(), "nothing was written");
    }

    /// A stage goes through the same handler and the same table as a single command: a
    /// pipeline is a shorthand, not a second way to reach something.
    #[test]
    fn a_pipeline_is_not_a_way_round_the_rules() {
        let mut app = test_app();
        let err = ToolHost::call_tool(&mut app, "shell", &json!({"command": "echo hi | rm /x"})).unwrap_err();
        assert!(err.contains("`rm` cannot run in a pipeline"), "{err}");

        // The handler's own limits still apply: `head -n` caps at 200 lines either way.
        write_file(&mut app, "/many.txt", &"line\n".repeat(50));
        assert_eq!(sh(&mut app, "cat /many.txt | head -n 2")["stdout"], json!("line\nline"));
        // And the answer is the pipeline's, not a stage's: it says what ran and what was written.
        let piped = sh(&mut app, "cat /many.txt | wc -l > /count.txt");
        assert_eq!(piped["pipeline"], json!(["cat", "wc"]), "{piped}");
        assert_eq!(piped["writes"][0]["path"], json!("/count.txt"));
        assert_eq!(sh(&mut app, "cat /count.txt")["text"], json!("50"));
    }

    // ---- the live journal stream (docs/agent-stream.md §1) ----

    /// An `App` in its `Arc`, wired exactly as `make_handler` wires it.
    fn shared_app() -> Shared {
        let app: Shared = Arc::new(Mutex::new(test_app()));
        App::attach_shared(&app);
        app
    }

    /// One journal entry, shaped like the agent's own.
    fn journal_event(seq: u64, kind: &'static str, name: &str) -> crate::agent::Event {
        crate::agent::Event {
            seq,
            kind,
            at_ms: 1_000 + seq,
            text: String::new(),
            tool_call_id: Some("c1".into()),
            name: Some(name.into()),
            is_error: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            phase: None,
            elapsed_ms: None,
        }
    }

    /// Stands in for the socket: what the stream writes, captured.
    struct Sink(Vec<u8>);

    impl EventSink for Sink {
        fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
    }

    /// Run a streamed response to completion and return what it wrote.
    fn run_stream(resp: Response) -> String {
        let body = resp.stream.expect("a streamed response");
        let mut sink = Sink(Vec::new());
        body(&mut sink).expect("the stream ended cleanly");
        String::from_utf8_lossy(&sink.0).to_string()
    }

    /// The `id:` of every frame in a stream body, in the order it arrived. A `: ping`
    /// comment has no id and so contributes nothing.
    fn frame_ids(body: &str) -> Vec<u64> {
        body.split("\n\n")
            .filter_map(|frame| frame.lines().next())
            .filter_map(|line| line.strip_prefix("id: "))
            .filter_map(|id| id.trim().parse().ok())
            .collect()
    }

    /// Short budgets, so the bounded-ness is tested in milliseconds rather than in
    /// two minutes of waiting.
    fn limits(idle: Duration, total: Duration, ping: Duration) -> StreamLimits {
        StreamLimits { idle, total, ping, poll: Duration::from_millis(10) }
    }

    #[test]
    fn the_stream_route_answers_text_event_stream_without_a_pin() {
        let shared = shared_app();
        let resp = shared.lock().unwrap().route(&get("/api/agent/stream?after=0"));
        assert_eq!(resp.status, 200, "no pin gate, like the transcript");
        assert!(resp.is_stream(), "the body is written after the headers");
        let header = |name: &str| {
            resp.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default()
        };
        assert_eq!(header("Content-Type"), "text/event-stream");
        assert_eq!(header("Cache-Control"), "no-cache");
        assert_eq!(shared.lock().unwrap().active_event_streams(), 1);
        // Dropping the response without running the body still frees the slot.
        drop(resp);
        assert_eq!(shared.lock().unwrap().active_event_streams(), 0);
    }

    #[test]
    fn the_stream_replays_after_the_given_seq_and_then_follows_live() {
        let shared = shared_app();
        {
            let mut app = shared.lock().unwrap();
            app.agent.events.push_back(journal_event(1, "message_start", ""));
            app.agent.events.push_back(journal_event(2, "tool_use", "board_status"));
            app.agent.events.push_back(journal_event(3, "tool_result", "board_status"));
        }
        let resp = shared.lock().unwrap().agent_stream_from(
            1,
            limits(Duration::from_millis(200), Duration::from_secs(5), Duration::from_secs(10)),
        );
        // The turn keeps running while the stream is open: the next event has to
        // arrive without the client asking again. 40 ms is inside the 200 ms idle budget,
        // so the frame cannot arrive from the replay — it has to be followed live.
        let writer = shared.clone();
        let turn = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            writer.lock().unwrap().agent.events.push_back(journal_event(4, "run_end", ""));
        });
        let body = run_stream(resp);
        turn.join().unwrap();

        assert_eq!(frame_ids(&body), vec![2, 3, 4], "replay after seq 1, then live: {body}");
        assert!(body.contains("event: tool_use"), "the frame names the kind: {body}");
        assert!(body.contains("\"tool_call_id\":\"c1\""), "data is the journal JSON: {body}");
        assert!(body.contains("\"seq\":2"), "data repeats the sequence number: {body}");
    }

    #[test]
    fn a_third_event_stream_is_refused_with_503_and_the_slot_is_reusable() {
        let shared = shared_app();
        let quick = limits(Duration::from_millis(50), Duration::from_millis(50), Duration::from_secs(5));
        let first = shared.lock().unwrap().agent_stream_from(0, quick);
        let second = shared.lock().unwrap().agent_stream_from(0, quick);
        let third = shared.lock().unwrap().agent_stream_from(0, quick);
        assert_eq!(third.status, 503);
        assert!(!third.is_stream(), "a refusal is an ordinary JSON body");
        let err: Value = serde_json::from_slice(&third.body).unwrap();
        assert_eq!(err["error"], "too many event streams");
        // The refused request never took a slot, so a closed one is free again.
        drop(first);
        let fourth = shared.lock().unwrap().agent_stream_from(0, quick);
        assert_eq!(fourth.status, 200, "a closed stream gives its slot back");
        drop((second, fourth));
        assert_eq!(shared.lock().unwrap().active_event_streams(), 0);
    }

    #[test]
    fn a_quiet_stream_pings_and_then_closes_itself() {
        let shared = shared_app();
        let resp = shared.lock().unwrap().agent_stream_from(
            0,
            limits(Duration::from_millis(200), Duration::from_secs(5), Duration::from_millis(50)),
        );
        let started = Instant::now();
        let body = run_stream(resp);
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(190), "it must wait for the idle budget: {took:?}");
        assert!(took < Duration::from_secs(2), "and then let go: {took:?}");
        assert!(body.contains(": ping"), "a silent stream still says it is alive: {body}");
        assert!(frame_ids(&body).is_empty(), "an empty journal has no frames: {body}");
        assert_eq!(shared.lock().unwrap().active_event_streams(), 0);
    }

    #[test]
    fn a_busy_stream_still_ends_at_the_total_budget() {
        let shared = shared_app();
        let resp = shared.lock().unwrap().agent_stream_from(
            0,
            // Idle is generous, the total is not: a stream that never goes quiet
            // must still give the socket back so the client can reconnect.
            limits(Duration::from_secs(30), Duration::from_millis(200), Duration::from_secs(30)),
        );
        let writer = shared.clone();
        let turn = std::thread::spawn(move || {
            // Events have to outlast the 200 ms total budget, or the stream would simply
            // run out of them: 60 events at the 5 ms the poller already wakes on is the
            // shortest pace that keeps it busy past the deadline.
            for seq in 1..=60 {
                std::thread::sleep(Duration::from_millis(5));
                writer.lock().unwrap().agent.events.push_back(journal_event(seq, "tool_use", "x"));
            }
        });
        let started = Instant::now();
        let body = run_stream(resp);
        let took = started.elapsed();
        turn.join().unwrap();
        assert!(took < Duration::from_secs(2), "the total budget ends a busy stream too: {took:?}");
        assert!(!frame_ids(&body).is_empty(), "and what it saw was delivered: {body}");
        assert_eq!(shared.lock().unwrap().active_event_streams(), 0);
    }

    /// The whole path over a real socket: the headers, the chunked framing, an event
    /// that arrives while the client is still reading, and the 503 for a third
    /// stream. This is the endpoint the portal and `curl -N` will actually meet.
    #[test]
    fn a_real_socket_receives_journal_events_as_they_happen() {
        use std::io::Write;
        let app = test_app();
        let log = app.log.clone();
        let shared: Shared = Arc::new(Mutex::new(app));
        App::attach_shared(&shared);
        let server =
            crate::http::Server::start("127.0.0.1:0", make_routes(shared.clone()), log).expect("bind");
        let addr = server.local_addr();

        let mut first = std::net::TcpStream::connect(addr).unwrap();
        first.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        first
            .write_all(
                b"GET /api/agent/stream?after=0 HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n",
            )
            .unwrap();
        let head = read_until(&mut first, "\r\n\r\n");
        assert!(head.starts_with("HTTP/1.1 200 OK"), "got: {head}");
        assert!(head.contains("Content-Type: text/event-stream"), "got: {head}");
        assert!(head.contains("Cache-Control: no-cache"), "got: {head}");
        assert!(head.contains("Transfer-Encoding: chunked"), "got: {head}");
        assert!(!head.contains("Content-Length"), "a bounded stream announces no length: {head}");

        // The agent emits mid-stream; the client is already reading and must see it.
        shared.lock().unwrap().agent.events.push_back(journal_event(1, "tool_use", "board_status"));
        let body = read_until(&mut first, "id: 1");
        assert!(body.contains("event: tool_use"), "got: {body}");
        assert!(body.contains("\"name\":\"board_status\""), "got: {body}");

        // Two live streams are allowed, the third is refused on the spot.
        let mut second = std::net::TcpStream::connect(addr).unwrap();
        second
            .write_all(b"GET /api/agent/stream HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while shared.lock().unwrap().active_event_streams() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(shared.lock().unwrap().active_event_streams(), 2, "both streams hold a slot");

        let mut third = std::net::TcpStream::connect(addr).unwrap();
        third.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        third
            .write_all(b"GET /api/agent/stream HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n")
            .unwrap();
        let refused = read_until(&mut third, "too many event streams");
        assert!(refused.starts_with("HTTP/1.1 503"), "got: {refused}");
        assert!(refused.contains("too many event streams"), "got: {refused}");
        assert!(refused.contains("Connection: close"), "a refusal frees its socket: {refused}");
        assert_eq!(refused.matches("Connection:").count(), 1, "one Connection header: {refused}");
        drop((first, second, third));
        drop(server);
    }

    /// Read from a socket until `needle` shows up, the peer closes, or the read
    /// times out; return everything read.
    fn read_until(stream: &mut std::net::TcpStream, needle: &str) -> String {
        use std::io::Read;
        let mut acc = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            if String::from_utf8_lossy(&acc).contains(needle) {
                break;
            }
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => acc.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&acc).to_string()
    }

    /// The point of polling instead of blocking: an open stream must not hold the app
    /// lock, or the portal freezes for two minutes every time a console is open.
    #[test]
    fn an_open_stream_leaves_the_app_lock_free_for_other_routes() {
        let shared = shared_app();
        let resp = shared.lock().unwrap().agent_stream_from(
            0,
            limits(Duration::from_millis(500), Duration::from_secs(5), Duration::from_secs(5)),
        );
        let body = resp.stream.expect("a streamed response");
        let stream = std::thread::spawn(move || {
            let mut sink = Sink(Vec::new());
            body(&mut sink).expect("stream");
        });
        let started = Instant::now();
        for _ in 0..5 {
            let resp = shared.lock().unwrap().route(&get("/api/status"));
            assert_eq!(resp.status, 200, "the portal must still answer");
        }
        let took = started.elapsed();
        assert!(
            took < Duration::from_millis(400),
            "five routes took {took:?} while a stream was open: the poller is holding the lock"
        );
        stream.join().unwrap();
        assert_eq!(shared.lock().unwrap().active_event_streams(), 0);
    }

    #[test]
    fn the_model_key_is_never_echoed_and_https_is_accepted() {
        let mut app = test_app();
        let saved = body_json(&app.route(&post(
            "/api/agent/config",
            r#"{"base_url":"http://192.0.2.10:8899/v1/chat/completions","model":"stub","api_key":"sk-test-dummy","max_steps":6}"#,
        )));
        assert!(saved.get("api_key").is_none(), "the POST response echoed the key: {saved}");
        assert_eq!(saved["api_key_set"], true);
        let got = body_json(&app.route(&get("/api/agent/config")));
        assert!(got.get("api_key").is_none(), "GET echoed the key: {got}");
        assert_eq!(got["api_key_set"], true);
        assert_eq!(got["model"], "stub");
        assert_eq!(app.llm.max_steps, 6);
        assert_eq!(app.llm.api_key, "sk-test-dummy", "the key must still reach the model call");

        // The placeholder the rest of the console shows cannot overwrite the key.
        app.route(&post("/api/agent/config", r#"{"api_key":"__set__"}"#));
        assert_eq!(app.llm.api_key, "sk-test-dummy");

        // https is a normal endpoint, not a special case: the transport carries TLS.
        let ok = app.route(&post(
            "/api/agent/config",
            r#"{"base_url":"https://api.example.com/v1/chat/completions"}"#,
        ));
        assert_eq!(ok.status, 200, "https must be accepted: {}", String::from_utf8_lossy(&ok.body));
        assert_eq!(app.llm.base_url, "https://api.example.com/v1/chat/completions");
        assert_eq!(body_json(&ok)["endpoint_ok"], true);

        // Something that is not HTTP at all still gets a reason.
        let bad = app.route(&post("/api/agent/config", r#"{"base_url":"ftp://example.com/x"}"#));
        assert_eq!(bad.status, 400);
        assert!(String::from_utf8_lossy(&bad.body).contains("http://"));
        assert_eq!(app.llm.base_url, "https://api.example.com/v1/chat/completions");
        assert_eq!(app.route(&post("/api/agent/config", r#"{"max_steps":99}"#)).status, 200);
        assert_eq!(app.llm.max_steps, 99, "a step cap can be set, up to the sane ceiling");
        assert_eq!(app.route(&post("/api/agent/config", r#"{"max_steps":0}"#)).status, 200);
        assert_eq!(app.llm.max_steps, 0, "zero turns the cap off again");
    }

    #[test]
    fn the_agent_writes_need_the_pin_like_every_other_write() {
        let mut app = test_app();
        app.cfg.auth.portal_pin = "1234".into();
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"hi"}"#)).status, 401);
        assert_eq!(app.route(&post("/api/agent/reset", "{}")).status, 401);
        assert_eq!(app.route(&post("/api/agent/config", r#"{"model":"x"}"#)).status, 401);
        assert_eq!(app.route(&post("/api/sched", r#"{"kind":"task","op":"delete","id":"t1"}"#)).status, 401);
        // Secrets and the transcript need the same credential now: `config` names the
        // network even with values masked, and the transcript is the most private thing
        // on the board. Status, devices and the schedule stay readable.
        assert_eq!(app.route(&get("/api/agent/history")).status, 401);
        assert_eq!(app.route(&get("/api/config")).status, 401);
        assert_eq!(app.route(&get("/api/agent/prompt")).status, 401);
        assert_eq!(app.route(&get("/api/sched")).status, 200);
        assert!(app.tasks.is_empty(), "an unauthorised write got through");
        assert!(app.agent.history.is_empty());
    }

    /// Login, then use the token: the page and a script both need a way in that is not
    /// "repeat the PIN in every URL".
    #[test]
    fn a_token_from_the_pin_authorises_writes_and_reads() {
        let mut app = test_app();
        app.cfg.auth.portal_pin = "1234".into();

        let wrong = body_json(&app.route(&post("/api/auth", r#"{"pin":"9999"}"#)));
        assert_eq!(wrong["ok"], false);
        assert_eq!(app.route(&post("/api/auth", r#"{"pin":"9999"}"#)).status, 401);

        let login = body_json(&app.route(&post("/api/auth", r#"{"pin":"1234"}"#)));
        let token = login["token"].as_str().expect("token").to_string();
        assert_eq!(login["expires_in"], 12 * 3600);

        let mut write = post("/api/agent/reset", "{}");
        write.headers.insert("authorization".into(), format!("Bearer {token}"));
        assert_eq!(app.route(&write).status, 200, "the token authorises a write");

        let mut read = get("/api/config");
        read.headers.insert("authorization".into(), format!("Bearer {token}"));
        assert_eq!(app.route(&read).status, 200, "and a read that needs the credential");

        let mut forged = get("/api/config");
        forged.headers.insert("authorization".into(), "Bearer 0000000000000000".into());
        assert_eq!(app.route(&forged).status, 401, "a token it never minted is worthless");
    }

    #[test]
    fn a_task_can_be_added_toggled_and_deleted() {
        let mut app = test_app();
        let added = body_json(&app.route(&post(
            "/api/sched",
            r#"{"kind":"task","op":"add","task":{"id":"t1","prompt":"read the sensor","every_s":60}}"#,
        )));
        assert_eq!(added["ok"], true);
        assert_eq!(added["tasks"][0]["id"], "t1");
        assert_eq!(added["tasks"][0]["enabled"], true);

        let off = body_json(&app.route(&post("/api/sched", r#"{"kind":"task","op":"disable","id":"t1"}"#)));
        assert_eq!(off["tasks"][0]["enabled"], false);
        let on = body_json(&app.route(&post("/api/sched", r#"{"kind":"task","op":"enable","id":"t1"}"#)));
        assert_eq!(on["tasks"][0]["enabled"], true);

        // A task with no trigger, or with a time a clock cannot reach, is refused.
        assert_eq!(
            app.route(&post("/api/sched", r#"{"kind":"task","op":"add","task":{"id":"t2","prompt":"x"}}"#))
                .status,
            400
        );
        assert_eq!(
            app.route(&post(
                "/api/sched",
                r#"{"kind":"task","op":"add","task":{"id":"t3","prompt":"x","at_hhmm":"25:00"}}"#
            ))
            .status,
            400
        );

        let del = body_json(&app.route(&post("/api/sched", r#"{"kind":"task","op":"delete","id":"t1"}"#)));
        assert!(del["tasks"].as_array().unwrap().is_empty());
        assert_eq!(
            app.route(&post("/api/sched", r#"{"kind":"task","op":"delete","id":"gone"}"#)).status,
            404
        );
        // The edit is persisted, so a reboot keeps the schedule.
        assert!(app.store.get(NVS_KEY_SCHED).is_some(), "the schedule was never saved");
    }

    #[test]
    fn a_sensor_rule_needs_a_known_op_and_a_prompt() {
        let mut app = test_app();
        let ok = app.route(&post(
            "/api/sched",
            r#"{"kind":"rule","op":"add","rule":{"id":"r1","tool":"bme280_read","path":"temperature_c","op":"gt","value":30,"prompt":"too hot"}}"#,
        ));
        assert_eq!(ok.status, 200);
        let v = body_json(&ok);
        assert_eq!(v["rules"][0]["op"], "gt");
        assert_eq!(v["rules"][0]["value"], 30);
        assert_eq!(v["rules"][0]["min_interval_s"], 60, "a default interval keeps a rule from flooding");
        assert_eq!(
            app.route(&post("/api/sched", r#"{"kind":"rule","op":"add","rule":{"id":"r2","tool":"bme280_read","op":"approx","prompt":"x"}}"#)).status,
            400
        );
        assert_eq!(
            app.route(&post(
                "/api/sched",
                r#"{"kind":"rule","op":"add","rule":{"id":"r3","tool":"bme280_read","op":"gt"}}"#
            ))
            .status,
            400
        );
        assert_eq!(app.route(&post("/api/sched", r#"{"kind":"nonsense","op":"add"}"#)).status, 400);
    }

    #[test]
    fn the_model_tool_list_is_the_board_tools_plus_the_file_and_http_tools() {
        let app = test_app();
        let specs = app.tool_specs();
        let tools = specs.as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        // Every entry is in the shape the chat endpoint expects.
        assert!(tools.iter().all(|t| t["type"] == "function" && t["function"]["parameters"].is_object()));
        assert!(names.contains(&"board"), "got {names:?}");
        assert!(names.contains(&"display"), "the OLED's tools are behind `display`: {names:?}");
        assert!(names.contains(&"shell"), "the file and network commands are missing: {names:?}");
        // A tool whose hardware is absent is not offered, exactly as in tools/list.
        assert!(!names.contains(&"mpu6050_read"));
    }

    /// A turn has to be able to reach the network while it runs.
    ///
    /// The client the model call borrows is not the one `net_*` uses: the App owns a
    /// separate tool transport. That split existed in name only — the simulator never
    /// attached one, so every `net_get` inside a turn answered "no HTTP transport is
    /// attached" on the desk while the same firmware code worked on a board.
    #[test]
    fn a_turn_can_use_the_network_tools_while_it_is_running() {
        struct Fetching {
            urls: Arc<Mutex<Vec<String>>>,
            body: String,
        }

        impl ChatTransport for Fetching {
            fn post_json(
                &mut self,
                url: &str,
                _headers: &[(&str, &str)],
                _body: &str,
            ) -> std::result::Result<String, String> {
                self.urls.lock().unwrap().push(url.to_string());
                Ok(self.body.clone())
            }

            fn request(
                &mut self,
                _method: &str,
                url: &str,
                _headers: &[(&str, &str)],
                _body: &str,
            ) -> std::result::Result<String, String> {
                self.urls.lock().unwrap().push(url.to_string());
                Ok(self.body.clone())
            }
        }

        struct AsksForThePage {
            seen: Arc<Mutex<Vec<String>>>,
            calls: usize,
        }

        impl ChatTransport for AsksForThePage {
            fn post_json(
                &mut self,
                _url: &str,
                _headers: &[(&str, &str)],
                body: &str,
            ) -> std::result::Result<String, String> {
                self.seen.lock().unwrap().push(body.to_string());
                self.calls += 1;
                if self.calls == 1 {
                    Ok(json!({ "choices": [{ "message": {
                        "role": "assistant", "content": null, "tool_calls": [{
                            "id": "call_1", "type": "function",
                            "function": { "name": "net_get", "arguments": "{\"url\":\"http://example.com/\"}" }
                        }] } }] })
                    .to_string())
                } else {
                    Ok(json!({ "choices": [{ "message": { "role": "assistant", "content": "fetched" } }] })
                        .to_string())
                }
            }
        }

        let shared: Shared = Arc::new(Mutex::new(test_app()));
        let urls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let mut app = shared.lock().unwrap();
            app.attach_tool_transport(Box::new(Fetching {
                urls: urls.clone(),
                body: "{\"page\":\"hello\"}".to_string(),
            }));
            app.route(&post(
                "/api/agent/config",
                r#"{"base_url":"http://127.0.0.1:9/v1/chat/completions","model":"stub","api_key":""}"#,
            ));
            assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"fetch the page"}"#)).status, 202);
        }

        let mut model = AsksForThePage { seen: seen.clone(), calls: 0 };
        let outcome = drive_agent(&shared, &mut model).expect("a queued turn");
        assert!(matches!(outcome, Ok(_)), "the turn failed: {outcome:?}");

        assert_eq!(
            urls.lock().unwrap().as_slice(),
            ["http://example.com/"],
            "the tool transport was never used"
        );
        let sent = seen.lock().unwrap();
        let second: Value = serde_json::from_str(&sent[1]).unwrap();
        let tool_message = second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("the fetched body never reached the model");
        assert!(tool_message["content"].as_str().unwrap().contains("hello"), "got {tool_message}");
    }

    #[test]
    fn an_unknown_fs_or_net_tool_says_so_instead_of_reaching_the_board() {
        let mut app = test_app();
        let err = ToolHost::call_tool(&mut app, "fs_nope", &json!({})).unwrap_err();
        assert!(err.contains("fs_"), "got {err}");
        // Without an attached transport the HTTP tools refuse instead of hanging.
        let err = ToolHost::call_tool(&mut app, "net_get", &json!({ "url": "http://example.invalid/" }))
            .unwrap_err();
        assert!(err.contains("transport"), "got {err}");
        // The board's own tools return their text, not a protocol envelope.
        let text = ToolHost::call_tool(&mut app, "board_status", &json!({})).unwrap();
        assert!(text.contains("board_name"), "got {}", &text[..text.len().min(200)]);
        assert!(!text.contains("\"content\""), "the MCP envelope leaked into the prompt: {text}");
    }

    /// Every request the stub model saw: url, headers, body.
    type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>, String)>>>;

    /// A stand-in chat endpoint: the first call asks for `board_status`, the second
    /// answers. Every request is recorded so the test can check what reached the wire.
    struct StubModel {
        seen: Seen,
        calls: usize,
    }

    impl ChatTransport for StubModel {
        fn post_json(
            &mut self,
            url: &str,
            headers: &[(&str, &str)],
            body: &str,
        ) -> std::result::Result<String, String> {
            self.seen.lock().unwrap().push((
                url.to_string(),
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                body.to_string(),
            ));
            self.calls += 1;
            if self.calls == 1 {
                Ok(json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"board_status","arguments":"{}"}}
                ]}}]})
                .to_string())
            } else {
                Ok(json!({
                    "choices":[{"message":{"role":"assistant","content":"The board is fine."}}]
                })
                .to_string())
            }
        }
    }

    /// The whole seam in one test: a queued message makes the model call the board's
    /// own tools through [`ToolHost`], the result goes back as a `tool` message, and
    /// the portal ends up with the exchange in its transcript.
    #[test]
    fn the_file_tools_share_one_contract_across_ls_and_grep() {
        // `fs_ls` and the `fs_grep` walker both expect child names from
        // `Workspace::list`; the App implementation used to hand back full paths, so a
        // grep over a directory found nothing while `fs_read` on the same path worked.
        let mut app = test_app();
        let write = ToolHost::call_tool(
            &mut app,
            "fs_write",
            &json!({ "path": "/bench/note.txt", "text": "the quick brown fox" }),
        )
        .expect("write");
        assert!(write.contains("note.txt"), "{write}");

        let ls = ToolHost::call_tool(&mut app, "fs_ls", &json!({ "path": "/bench" })).expect("ls");
        assert!(ls.contains("note.txt"), "ls must name the child: {ls}");
        assert!(!ls.contains("/bench/bench/"), "ls must not double the path: {ls}");

        let grep =
            ToolHost::call_tool(&mut app, "fs_grep", &json!({ "pattern": "brown fox", "path": "/bench" }))
                .expect("grep");
        assert!(grep.contains("\"count\":1"), "grep must find the file it can read: {grep}");
    }

    /// A board that has joined a network must not tell a model it is waiting for
    /// credentials.
    ///
    /// `setup_needed` used to ask the radio: is the access point up and the station
    /// not "connected". The station reports the state `ap+sta` while genuinely
    /// joined, so `contains("connected")` was false and every board with
    /// `wifi.ap_fallback` on announced setup mode forever — in `/api/status` and in
    /// the instructions handed to each MCP client, which is the sentence a model
    /// reads before deciding what to do. The question the field answers is whether
    /// credentials exist, so it is answered from the stored configuration.
    #[test]
    fn a_joined_board_does_not_claim_it_is_in_setup_mode() {
        let mut app = test_app();
        assert!(app.setup_needed(), "a board with no SSID is in setup mode");
        assert!(
            app.instructions("http").contains("setup mode"),
            "with no credentials the instructions say so"
        );

        let resp = app.route(&post("/api/config", r#"{"wifi":{"ssid":"home"}}"#));
        assert_eq!(resp.status, 200, "the config write must be accepted");

        assert!(!app.setup_needed(), "holding an SSID means the board is not waiting for credentials");
        let status = body_json(&app.route(&get("/api/status")));
        assert_eq!(
            status["setup"]["needed"],
            json!(false),
            "/api/status must agree with the instruction text"
        );
        assert!(
            !app.instructions("http").contains("setup mode"),
            "a joined board must not be described to a model as unprovisioned"
        );
    }

    /// The scripted self-test model must not be steered by its own output.
    ///
    /// Keyword matching used to be substring based, so a reply quoting
    /// `board_profile` contained `file`: the next round switched to the file
    /// script, needed four rounds for a three-step budget, and the rest of the turn
    /// failed with "step limit reached". Matching whole words fixes it.
    #[test]
    fn a_reply_quoting_a_profile_does_not_pick_the_file_script() {
        let body = json!({
            "model": "selftest",
            "messages": [{
                "role": "user",
                "content": "board_profile returned esp32s3-xiaozhi-bread; take three small steps"
            }],
            "tools": [{ "type": "function", "function": { "name": "board_status" } }],
        })
        .to_string();
        let reply = selftest_completion(&post("/selftest/chat/completions", &body));
        assert_eq!(
            reply["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            json!("board_status"),
            "a reply about a profile must not be read as a request about files"
        );

        // The word on its own still selects the file script.
        assert!(has_word("please write a file", "file"));
        assert!(!has_word("board_profile", "file"));
        assert!(!has_word("netmask 255.255.255.0", "net"));
        assert!(has_word("fetch it over http", "http"));
    }

    /// A turn in flight must not own the app while the model is thinking.
    ///
    /// This is the whole reason the turn is split into phases: with the model call
    /// inside the lock, a slow answer stalled the portal, `/api/status` and every
    /// other handler. The assertion is on wall time, because that is what the user
    /// feels.
    #[test]
    fn a_run_in_flight_does_not_hold_the_app_lock() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        /// A model that holds the reply until the test lets go.
        ///
        /// "The model is thinking" is then a state the test observes rather than a 600 ms
        /// guess: the signal arrives from inside the model call, so there is no window in
        /// which the portal may or may not be looking at a run that is in flight.
        struct HeldModel {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl crate::llm::ChatTransport for HeldModel {
            fn post_json(
                &mut self,
                _url: &str,
                _headers: &[(&str, &str)],
                _body: &str,
            ) -> std::result::Result<String, String> {
                let _ = self.entered.send(());
                // The test releases as soon as it has measured the portal. The bounded wait
                // is what keeps a regression - the lock held across the call - a red
                // assertion instead of a hung suite: /api/status would block for this long.
                let _ = self.release.recv_timeout(Duration::from_millis(400));
                Ok(json!({
                    "choices": [{ "message": { "role": "assistant", "content": "slow answer" } }]
                })
                .to_string())
            }
        }

        let app = Arc::new(Mutex::new(test_app()));
        {
            let mut guard = app.lock().unwrap();
            guard.route(&post(
                "/api/agent/config",
                r#"{"base_url":"http://192.0.2.10:8899/v1/chat/completions","model":"slow","max_steps":3,"api_key":""}"#,
            ));
            guard.agent.push_user("how is the board?", "http", 1);
        }

        let app_for_run = app.clone();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let run = std::thread::spawn(move || {
            let mut model = HeldModel { entered: entered_tx, release: release_rx };
            drive_agent(&app_for_run, &mut model)
        });

        // The model call is in flight — observed, not waited out.
        entered.recv_timeout(Duration::from_secs(5)).expect("the run reached the model");
        let started = Instant::now();
        let resp = {
            let mut guard = app.lock().unwrap();
            guard.route(&get("/api/status"))
        };
        let waited = started.elapsed();
        assert_eq!(resp.status, 200, "the portal must still answer");
        assert!(
            waited < Duration::from_millis(200),
            "the app lock was held for {waited:?} while the model was thinking"
        );

        // The transcript is readable mid-run too: the agent is only moved out for the
        // duration of a phase, not for the whole call.
        let mid_run = {
            let mut guard = app.lock().unwrap();
            guard.route(&get("/api/agent/history?after=0"))
        };
        assert_eq!(mid_run.status, 200);

        // Let the model answer, so the turn can finish.
        let _ = release.send(());
        let outcome = run.join().expect("the run thread");
        assert!(matches!(outcome, Some(Ok(_))), "the turn still finishes");
    }

    #[test]
    fn a_queued_message_runs_the_loop_through_the_board_tools() {
        let mut app = test_app();
        let seen = Arc::new(Mutex::new(Vec::new()));
        app.attach_model_transport(Box::new(StubModel { seen: seen.clone(), calls: 0 }));
        app.route(&post(
            "/api/agent/config",
            r#"{"base_url":"http://127.0.0.1:9/v1/chat/completions","model":"stub","api_key":"sk-1"}"#,
        ));
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"is the board ok?"}"#)).status, 202);

        let answer = app.agent_run_once().expect("the run should complete");
        assert_eq!(answer, "The board is fine.");
        assert_eq!(app.agent.runs, 1);
        assert!(app.agent.last_error.is_empty(), "{0}", app.agent.last_error);

        let sent = seen.lock().unwrap();
        assert_eq!(sent.len(), 2, "one tool round, then the answer");
        let (url, headers, first) = &sent[0];
        assert_eq!(url, "http://127.0.0.1:9/v1/chat/completions");
        assert!(
            headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer sk-1"),
            "the key never reached the request: {headers:?}"
        );
        let body1: Value = serde_json::from_str(first).unwrap();
        let names: Vec<&str> = body1["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"board"), "got {names:?}");
        assert!(names.contains(&"shell"), "got {names:?}");
        assert_eq!(body1["model"], "stub");

        // The second request carries the board's answer back as a `tool` message.
        let body2: Value = serde_json::from_str(&sent[1].2).unwrap();
        let tool_msg = body2["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("the tool result was never sent back");
        assert!(tool_msg["content"].as_str().unwrap().contains("board_name"), "got {tool_msg}");

        // The transcript the portal renders is the whole exchange.
        let roles: Vec<String> =
            app.agent.history.iter().map(|m| format!("{:?}", m.role).to_lowercase()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "assistant"], "got {roles:?}");
    }

    #[test]
    fn a_queued_run_without_a_transport_fails_loudly_and_keeps_the_message() {
        let mut app = test_app();
        app.agent.push_user("hello", "http", 1);
        let err = app.agent_run_once().unwrap_err();
        assert!(err.contains("transport"), "got {err}");
        assert_eq!(app.agent.last_error, err);
        // Nothing is lost: the message can still run once a transport is attached.
        assert!(app.agent.pending.is_some());
        // With nothing queued a run is a no-op, not an error.
        app.agent.reset();
        assert_eq!(app.agent_run_once(), Ok(String::new()));
    }

    #[test]
    fn a_file_tool_round_trips_through_the_tool_host() {
        let mut app = test_app();
        ToolHost::call_tool(&mut app, "fs_write", &json!({ "path": "/notes/a.txt", "text": "hello" }))
            .expect("fs_write through the host");
        let read =
            ToolHost::call_tool(&mut app, "fs_read", &json!({ "path": "/notes/a.txt" })).expect("fs_read");
        assert!(read.contains("hello"), "got {read}");
        // The workspace is persisted as one value per file, and an index names them, so a save
        // rewrites only what changed (`persist_workspace`).
        let key = crate::keys::workspace_file("/notes/a.txt");
        let text = app.store.get(&key).expect("the file's value was never written");
        assert_eq!(text, "hello");
        let index = app.store.get(crate::keys::WORKSPACE_INDEX).expect("no index");
        assert!(index.contains("/notes/a.txt"), "the index does not name the file: {index}");
        assert!(app.store.get(NVS_KEY_WORKSPACE).is_none(), "the old document is still there");
    }

    // ------------------------------------------------- the workspace across a reboot

    /// The document is written as it is generated and parsed as it is read, so the two halves
    /// of the streaming path have to agree - and the result has to still be the JSON every
    /// other reader expects.
    #[test]
    fn the_streamed_document_round_trips_and_is_still_json() {
        use crate::tools::fs::Workspace as _;
        let mut ws = AppWorkspace::default();
        ws.write("/a.txt", "plain").expect("a file");
        ws.write("/notes/quo\"tes.md", "a \"quoted\" line\nand a backslash \\\n").expect("escaping");
        ws.write("/empty.md", "").expect("an empty file");

        let mut bytes: Vec<u8> = Vec::new();
        ws.write_json(&mut bytes).expect("streams");
        let text = String::from_utf8(bytes.clone()).expect("utf-8");

        // Same shape as `to_json`: a version and a map.
        let as_value: Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(as_value["version"], 1);
        assert_eq!(as_value["files"]["/a.txt"], "plain");

        let back = AppWorkspace::from_reader(&bytes[..]).expect("parses");
        assert_eq!(back.bytes(), ws.bytes(), "same text out as in");
        for (path, text) in ws.paths() {
            assert_eq!(&back.read(path).unwrap(), text, "{path} came back different");
        }
    }

    /// The guard that turns "PSRAM cannot give it" into a sentence: two copies of the workspace
    /// (the rollback and the change) plus a margin, against what is actually free.
    #[test]
    fn a_workspace_change_is_refused_before_psram_runs_out() {
        // 1 MB of workspace needs ~2.06 MB free; a board with 7.9 MB says yes.
        assert!(super::workspace_fits(1_000_000, Some(7_900_000)).is_ok());
        // The same board with 1 MB left says no, and says why.
        let err = super::workspace_fits(1_000_000, Some(1_000_000)).unwrap_err();
        assert!(err.contains("nothing was changed"), "{err}");
        assert!(err.contains("PSRAM"), "{err}");
        // A platform that cannot answer is not blocked: no measurement is not a refusal.
        assert!(super::workspace_fits(1_000_000, None).is_ok());
    }

    /// The space a backend reports has to say what that backend has, or the field lies.
    ///
    /// A filesystem has no entries. Reporting `free_entries: 0` for one - which is what this did
    /// - reads as "full" on a partition with 11 MB free, and `/api/status.store` is what the
    /// board's own agent reads when it wonders why a write was refused. So the assertion is
    /// geometric: which keys exist, and whether the numbers add up.
    #[test]
    fn the_space_a_backend_reports_is_in_the_units_it_has() {
        let fs = store::StoreSpace::Bytes { used: 107_177, total: 11_498_561 }.to_json();
        for entry_key in ["used_entries", "free_entries", "total_entries"] {
            assert!(fs.get(entry_key).is_none(), "a filesystem reported `{entry_key}`: \n{fs}");
        }
        assert_eq!(fs["total_bytes"], 11_498_561, "{fs}");
        assert_eq!(fs["used_bytes"], 107_177, "{fs}");
        assert_eq!(
            fs["free_bytes"].as_u64(),
            Some(11_498_561 - 107_177),
            "free must be total minus used: {fs}"
        );

        let nvs = store::StoreSpace::Entries {
            used: 534,
            free: 222,
            total: 756,
            used_bytes: 534 * 32,
            total_bytes: 756 * 32,
        }
        .to_json();
        for entry_key in ["used_entries", "free_entries", "total_entries"] {
            assert!(nvs.get(entry_key).is_some(), "NVS lost `{entry_key}`: {nvs}");
        }
        assert_eq!(nvs["free_entries"], 222, "{nvs}");
        assert_eq!(nvs["free_bytes"], 222 * 32, "{nvs}");
        assert_eq!(nvs["total_bytes"], 756 * 32, "{nvs}");
        // The same relation holds in both units, so a caller can use either.
        assert_eq!(
            nvs["free_bytes"].as_u64(),
            Some(nvs["total_bytes"].as_u64().unwrap() - nvs["used_bytes"].as_u64().unwrap()),
            "{nvs}"
        );
    }

    /// A store whose workspace writes always fail, which is what a full NVS looks like from
    /// the App's side. Everything else is delegated, so only the `ws` keys are affected.
    struct RefusingWorkspaceStore(SharedStore);

    impl KvStore for RefusingWorkspaceStore {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key)
        }

        fn set(&mut self, key: &str, value: &str) -> bool {
            // Every key the workspace is made of: the per-file values and their index.
            if key.starts_with("ws.") {
                return false;
            }
            self.0.set(key, value)
        }

        fn remove(&mut self, key: &str) -> bool {
            self.0.remove(key)
        }

        fn keys(&self) -> Vec<String> {
            self.0.keys()
        }
    }

    /// Markdown with newlines, the shape a real workspace has: JSON escapes every one of
    /// them to two bytes, which is why a workspace that fits `WORKSPACE_TOTAL_MAX` of text
    /// still needs more than one stored value.
    fn notes(bytes: usize) -> String {
        "a line of notes for the board\n".repeat(bytes / 29 + 1)
    }

    #[test]
    fn a_workspace_bigger_than_one_nvs_value_survives_a_reboot() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        app.workspace = AppWorkspace::default();
        let written = [
            ("/notes/a.md", notes(1500)),
            ("/notes/b.md", notes(1500)),
            ("/skills/two/SKILL.md", notes(1500)),
        ];
        for (path, text) in &written {
            ToolHost::call_tool(&mut app, "fs_write", &json!({ "path": path, "text": text }))
                .unwrap_or_else(|e| panic!("fs_write {path}: {e}"));
        }
        // What the writer task would have done before the reboot.
        drain_store(&mut app);

        // Three values, one per file, each past the old 4000-byte single-value ceiling, plus
        // the index that names them. Nothing whole-document exists any more.
        let mut total = 0;
        for (path, _) in &written {
            let key = crate::keys::workspace_file(path);
            let text = store.get(&key).unwrap_or_else(|| panic!("{path} has no value"));
            total += text.len();
        }
        assert!(total > store::VALUE_MAX, "{total} bytes across the files");
        assert!(store.get("ws").is_none(), "the old document is still there");
        assert!(store.get(crate::keys::WORKSPACE_INDEX).is_some(), "no index");

        // A reboot is a new App over the same keys.
        let rebooted = app_with(&store);
        for (path, text) in &written {
            let back = fs_tools::Workspace::read(&rebooted.workspace, path)
                .unwrap_or_else(|e| panic!("{path} after the reboot: {e}"));
            assert_eq!(&back, text, "{path} came back different");
        }
    }

    #[test]
    fn a_write_that_cannot_be_stored_is_an_error_and_leaves_the_file_alone() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        app.workspace = AppWorkspace::default();
        ToolHost::call_tool(&mut app, "fs_write", &json!({ "path": "/a.txt", "text": "first" }))
            .expect("the first write is stored");

        // From here the workspace cannot be stored: the case that used to answer `ok: true`.
        app.store = Box::new(RefusingWorkspaceStore(store.clone()));
        let err = ToolHost::call_tool(&mut app, "fs_write", &json!({ "path": "/a.txt", "text": "second" }))
            .expect_err("a write that cannot be stored must not answer ok");
        assert!(err.contains("not saved"), "{err}");
        assert!(err.contains("/a.txt"), "the error names the file that failed: {err}");
        assert!(err.contains("The change was dropped"), "{err}");

        // And `read` shows that: the file is what it was, not what the refused write carried.
        let read = ToolHost::call_tool(&mut app, "fs_read", &json!({ "path": "/a.txt" }))
            .expect("the file is still there");
        assert!(read.contains("first"), "{read}");
        assert!(!read.contains("second"), "{read}");
    }

    /// The chunked layout is what the board wrote while one value could not exceed ~4 KB. It is
    /// still read: an upgrade mid-life keeps every file it had, and the next save rewrites it as
    /// one document.
    #[test]
    fn a_chunked_workspace_from_an_older_build_is_read_and_rewritten() {
        let store = SharedStore::new();
        let old = json!({
            "version": 1,
            "files": {
                "/MEMORY.md": "- [pins](/memory/pins.md) - GPIO18 is the strip\n",
                "/memory/pins.md": "# pins\nGPIO18 is the WS2812 strip, active high.\n",
            }
        })
        .to_string();
        // The older shape: a `ws1` header under `ws`, the document under `ws.1`, `ws.2`, ...
        store::write_blob(&mut store.clone(), NVS_KEY_WORKSPACE, &old, WORKSPACE_PLAN)
            .expect("the chunked layout");
        assert!(store.get("ws").unwrap().starts_with("ws1 "), "the fixture is not chunked");

        let mut app = app_with(&store);
        assert_eq!(
            fs_tools::Workspace::read(&app.workspace, "/memory/pins.md").unwrap(),
            "# pins\nGPIO18 is the WS2812 strip, active high.\n",
            "every file the old value held is still there"
        );
        assert!(fs_tools::Workspace::read(&app.workspace, "/MEMORY.md").unwrap().contains("pins"));
        // The rewrite is queued like every other write; the writer makes it durable.
        drain_store(&mut app);
        assert!(store.get("ws").is_none(), "the chunked layout is still there");
        assert!(store.get("ws.1").is_none(), "the old chunk is still there");
        let index = store.get(crate::keys::WORKSPACE_INDEX).expect("no index after the migration");
        assert!(index.contains("/memory/pins.md"), "{index}");

        // A second boot reads the document, so the rewrite was stored, not served from RAM once.
        let again = app_with(&store);
        assert!(fs_tools::Workspace::read(&again.workspace, "/memory/pins.md").is_ok());
    }

    /// Half a document must not come back as a file list no boot ever wrote.
    #[test]
    fn a_torn_workspace_document_starts_empty_and_says_why() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        app.workspace = AppWorkspace::default();
        for (path, text) in [("/notes/a.md", notes(1900)), ("/notes/b.md", notes(1900))] {
            ToolHost::call_tool(&mut app, "fs_write", &json!({ "path": path, "text": text })).unwrap();
        }
        assert!(fs_tools::Workspace::read(&app.workspace, "/notes/a.md").unwrap().len() > 1000);
        drain_store(&mut app);

        // The older document layout by hand: this build writes one value per file, and the
        // older layout is still read, so a torn one is exercised where it used to live.
        let mut files = serde_json::Map::new();
        files.insert("/notes/a.md".to_string(), json!(notes(1900)));
        files.insert("/notes/b.md".to_string(), json!(notes(1900)));
        let whole = json!({ "version": 1, "files": files }).to_string();
        assert!(whole.starts_with('{'));

        // A power cut in the middle of a save leaves half a document, and no index.
        store.clone().set("ws", &whole[..whole.len() / 2]);
        store.clone().remove(crate::keys::WORKSPACE_INDEX);

        let (rebooted, log) = app_with_log(&store);
        assert!(
            fs_tools::Workspace::read(&rebooted.workspace, "/notes/a.md").is_err(),
            "half a workspace must not come back"
        );
        assert!(
            log.tail(80, 0).iter().any(|l| l.level == "error" && l.message.contains("unreadable")),
            "{:?}",
            log.tail(80, 0).iter().map(|l| &l.message).collect::<Vec<_>>()
        );
    }

    /// A save costs the files that changed, not the whole workspace.
    ///
    /// This is the measurement that made the layout change worth doing: the document layout
    /// rewrote every byte on every save (14 s at 0.25 MB, 277 s at 2 MB on the board) and needed
    /// a second copy of the whole document on flash at the same time, which SPIFFS refused at
    /// about 2 MB with `ENOSPC` while reporting 9 MB free.
    #[test]
    fn a_save_rewrites_only_the_files_that_changed() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        app.workspace = AppWorkspace::default();
        for path in ["/a.md", "/b.md", "/c.md"] {
            ToolHost::call_tool(&mut app, "fs_write", &json!({ "path": path, "text": "first" }))
                .expect("a file");
        }
        drain_store(&mut app);

        // One file changes: its value and the index, nothing else.
        let before = store.writes();
        ToolHost::call_tool(
            &mut app,
            "fs_write",
            &json!({ "path": "/b.md", "text": "second and longer than before" }),
        )
        .expect("the change");
        drain_store(&mut app);
        let written = store.writes() - before;
        assert_eq!(written, 2, "expected the changed file and the index, got {written} writes");

        // And a file that did not change is untouched: its value is still the first text.
        let key = crate::keys::workspace_file("/a.md");
        assert_eq!(store.get(&key).as_deref(), Some("first"));
    }

    /// A torn index names nothing, and says so - the per-file layout's own torn value.
    #[test]
    fn a_torn_workspace_index_names_no_files_and_says_why() {
        let store = SharedStore::new();
        store.clone().set(crate::keys::WORKSPACE_INDEX, "{\"/a.md\":\"wf0123");
        let (app, log) = app_with_log(&store);
        assert!(
            fs_tools::Workspace::read(&app.workspace, "/a.md").is_err(),
            "a torn index must not come back as files"
        );
        assert!(
            log.tail(80, 0).iter().any(|l| l.level == "error" && l.message.contains("index is unreadable")),
            "{:?}",
            log.tail(80, 0).iter().map(|l| &l.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_workspace_refuses_to_climb_out_or_grow_without_bound() {
        let mut ws = AppWorkspace::default();
        assert!(fs_tools::Workspace::write(&mut ws, "/a/b.txt", "x").is_ok());
        assert_eq!(fs_tools::Workspace::read(&ws, "/a/b.txt").unwrap(), "x");
        assert!(fs_tools::Workspace::read(&ws, "/a/../b.txt").is_err(), ".. must be refused, not resolved");
        assert!(fs_tools::Workspace::write(&mut ws, "/big.txt", &"x".repeat(WORKSPACE_FILE_MAX + 1)).is_err());
        assert_eq!(
            fs_tools::Workspace::list(&ws, "/").unwrap(),
            vec!["a".to_string()],
            "list returns child names, which is what fs_ls and the grep walker join on"
        );
        assert_eq!(
            fs_tools::Workspace::list(&ws, "/a").unwrap(),
            vec!["b.txt".to_string()],
            "the file lives one level down"
        );
        assert!(fs_tools::Workspace::remove(&mut ws, "/a/b.txt").is_ok());
        assert!(fs_tools::Workspace::remove(&mut ws, "/a/b.txt").is_err());
        // What is stored is what comes back.
        let reloaded = AppWorkspace::from_json(&ws.to_json());
        assert_eq!(reloaded.len(), ws.len());
    }

    fn tool_names(app: &mut App) -> Vec<String> {
        let v = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
        v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// The device answers `tools/list` through `tools_list_body`, which never
    /// builds the whole result as a `Value`. It has to say exactly what the value
    /// path says, or the two transports disagree about the same board.
    #[test]
    fn tools_list_body_agrees_with_the_value_path() {
        let mut app = test_app();
        let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
        let fast = app.tools_list_body(body).expect("a single tools/list takes the fast path");
        let slow = app.handle_rpc(body, None).expect("value path always answers");
        let parsed: Value = serde_json::from_str(&fast).expect("the fast body is valid JSON");
        assert_eq!(parsed["id"], json!(7));
        assert_eq!(parsed["jsonrpc"], json!("2.0"));
        assert_eq!(parsed["result"]["tools"], slow["result"]["tools"]);
    }

    #[test]
    fn tools_list_body_leaves_other_requests_to_the_generic_path() {
        let mut app = test_app();
        assert!(app.tools_list_body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#).is_none());
        assert!(app.tools_list_body(r#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#).is_none());
        assert!(app.tools_list_body(r#"{"jsonrpc":"2.0","method":"tools/list"}"#).is_none());
    }

    /// The file and network tools are reachable over MCP too, not only from the board's
    /// own agent: an outside client driving the board can read the workspace, write a
    /// skill, or fetch a page — the same reach, one implementation.
    #[test]
    fn the_file_and_network_tools_are_in_the_mcp_list_and_callable() {
        let mut app = test_app();
        let names = tool_names(&mut app);
        for needle in ["shell", "script", "read"] {
            assert!(names.contains(&needle.to_string()), "{needle} is missing from tools/list: {names:?}");
        }
        // `image_read` is the counter-example, and it is the one this test used to make
        // wrong: this board has no camera, so it is not offered, and `hidden_tools` says
        // why. A tool the board cannot carry is worse than one fewer tool.
        assert!(!names.contains(&"image_read".to_string()), "image_read without a camera: {names:?}");
        assert!(
            app.hidden_tools()
                .iter()
                .any(|t| t["name"] == "image_read" && t["why"].as_str().unwrap_or("").contains("camera")),
            "a hidden tool must say why: {:?}",
            app.hidden_tools()
        );

        // The schema is the same object the model gets, in the MCP field name.
        let list = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
        let shell = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "shell")
            .expect("shell in the list");
        assert!(shell["inputSchema"]["properties"]["command"].is_object(), "{shell}");

        // And the commands go through the same code the agent uses, over MCP.
        let write = app
            .handle_rpc(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"write","arguments":{"path":"/notes/a.txt","text":"from mcp"}}}"#,
                None,
            )
            .unwrap();
        assert_eq!(write["result"]["isError"], json!(false), "{write}");
        let read = app
            .handle_rpc(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read","arguments":{"path":"/notes/a.txt"}}}"#,
                None,
            )
            .unwrap();
        assert_eq!(read["result"]["structuredContent"]["text"], json!("from mcp"), "{read}");

        // `mcp.disabled_tools` hides a tool like any other; the commands inside `shell`
        // are not separate tools, which is the point of the consolidation.
        app.cfg.mcp.disabled_tools = vec!["script".into()];
        assert!(!tool_names(&mut app).contains(&"script".to_string()), "disabled_tools did not hide script");
        assert!(
            app.hidden_tools()
                .iter()
                .any(|t| t["name"] == "script" && t["why"].as_str().unwrap_or("").contains("disabled_tools")),
            "a disabled tool must say it was switched off: {:?}",
            app.hidden_tools()
        );
    }

    /// A family's three tables are one table: declared (the schema enum), helped (`command:
    /// help`) and dispatchable (a call that is not "no command").
    ///
    /// This is the assertion the `jq` bug needed: a command that was documented in `help`
    /// and absent from dispatch. It fails if a name is added to one list alone.
    ///
    /// It also measures what a family costs, rather than trusting the count of commands
    /// grouped: one declaration plus one `help` reply must be no larger than the
    /// declarations it replaced, so even a run that stops after a single step is not worse
    /// off. Past that, the saving the family made would leak back out of the help text.
    /// The tool takes `args`, because the interpreter always did.
    ///
    /// `App::run_script` reads `{"source":…, "args":…}` and puts it in the program's `args` and
    /// `argv` globals (`docs/scripting.md` documents both). The schema never declared the field,
    /// so a model asked to run a script *with* arguments answered that the tool "takes only
    /// `source`" - measured on the board.
    #[test]
    fn the_script_tool_declares_the_arguments_it_always_accepted() {
        let specs = super::board_tools::script_specs();
        let params = &specs[0]["function"]["parameters"]["properties"];
        assert!(params.get("args").is_some(), "the schema hides `args`: {params}");
        // And the handler passes it through, which is what a script reads as `args`.
        let mut app = test_app();
        let out = app
            .call_tool("script", &json!({ "source": "args.pin + 1", "args": { "pin": 41 } }))
            .expect("a script with arguments");
        assert_eq!(out["result"], 42, "{out}");
    }

    /// A tool with arguments that were never named cannot be written down.
    #[test]
    fn a_family_declares_helps_and_dispatches_the_same_commands() {
        let mut app = test_app();
        let names = |v: &Value| -> Vec<String> {
            v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
        };
        // Every family the board actually has, discovered from the same map the listing uses:
        // a driver that joins a family is covered here without this test being edited.
        let mut families: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for (name, reach) in &app.family_members {
            let family = reach.split(' ').next().unwrap_or("").to_string();
            families.entry(family).or_default().push(name.clone());
        }
        assert!(families.len() >= 4, "not every family formed: {families:?}");
        for (family, members) in families {
            let spec = ToolHost::tool_specs(&app)
                .as_array()
                .expect("array")
                .iter()
                .find(|t| t["function"]["name"] == family)
                .cloned()
                .unwrap_or_else(|| panic!("`{family}` is not offered"));
            let mut declared = names(&spec["function"]["parameters"]["properties"]["command"]["enum"]);
            declared.retain(|c| c != "help");

            let help = app.call_tool(&family, &json!({ "command": "help" })).unwrap();
            let helped = names(&help["commands"]);
            assert_eq!(declared, helped, "{family}: the schema and `help` disagree");

            for command in helped.iter().chain(std::iter::once(&"help".to_string())) {
                // A command that waits on the board gets the shortest wait its own schema
                // allows: `button_wait` defaults to five seconds, and all this loop proves is
                // that dispatch reaches the handler. Sitting out that default is what made
                // this test 5.05 s — the slowest thing in the suite after the ring test.
                let waits = app
                    .tools
                    .get(command.as_str())
                    .map(|def| def.spec.input_schema["properties"].get("timeout_ms").is_some())
                    .unwrap_or(false);
                // A family nests its command's arguments under `arguments`; the top level is
                // only `command` (and `help` needs nothing).
                let args = if waits {
                    json!({ "command": command, "arguments": { "timeout_ms": 10 } })
                } else {
                    json!({ "command": command })
                };
                if let Err(e) = app.call_tool(&family, &args) {
                    assert!(
                        !e.message.contains("no command"),
                        "{family}: `help` lists `{command}` but dispatch refuses it: {}",
                        e.message
                    );
                }
            }
            let err = app.call_tool(&family, &json!({ "command": "frobnicate" })).unwrap_err();
            assert!(
                err.message.contains("no command"),
                "{family}: an unknown command must be refused: {err:?}"
            );

            // What the model would have been sent for these tools, had they stayed separate.
            let replaced: usize = members
                .iter()
                .map(|name| {
                    let def = app.tools.get(name).expect("a member is still registered");
                    json!({
                        "type": "function",
                        "function": {
                            "name": def.spec.name,
                            "description": def.spec.description,
                            "parameters": def.spec.input_schema,
                        }
                    })
                    .to_string()
                    .len()
                })
                .sum();
            let declared_bytes = spec.to_string().len();
            let help_bytes = help.to_string().len();
            assert!(
                declared_bytes + help_bytes <= replaced,
                "{family}: {declared_bytes} + {help_bytes} bytes is more than the {replaced} it replaced"
            );
        }
    }

    #[test]
    fn tools_list_only_shows_detected_hardware() {
        let mut app = test_app();
        let names = tool_names(&mut app);
        assert!(names.contains(&"display".to_string()), "got {names:?}");
        assert!(names.contains(&"bme280_read".to_string()));
        assert!(names.contains(&"board".to_string()));
        assert!(!names.contains(&"mpu6050_read".to_string()));
        assert!(!names.contains(&"lcd_text".to_string()));
    }

    #[test]
    fn disabled_tools_disappear_and_missing_caps_hide_devices() {
        let mut app = test_app();
        app.cfg.mcp.disabled_tools = vec!["display".into()];
        assert!(!app.is_visible("display"));
        assert!(app
            .hidden_tools()
            .iter()
            .any(|t| t["name"] == "display" && t["why"].as_str().unwrap_or("").contains("disabled_tools")));
        assert!(!app.is_visible("mpu6050_read"));
    }

    #[test]
    fn generic_mode_collapses_device_tools() {
        let mut app = test_app();
        app.cfg.mcp.tools_mode = "generic".into();
        assert!(!app.is_visible("oled_text"), "device tools hidden");
        assert!(!app.is_visible("display"), "a device family collapses like the tools it groups");
        assert!(app.is_visible("board"));
        assert!(app.is_visible("devices_list"));
    }

    #[test]
    fn all_mode_keeps_registered_tools_visible() {
        let mut app = test_app();
        app.cfg.mcp.tools_mode = "all".into();
        // `all` skips capability filtering; it cannot invent a driver for a chip
        // that is not on the bus, so `mpu6050_read` stays absent either way.
        assert!(app.is_visible("display"));
        assert!(app.is_visible("board"));
        assert!(!app.is_visible("mpu6050_read"));
    }

    #[test]
    fn calling_an_absent_device_tool_is_a_clear_not_found() {
        let mut app = test_app();
        let err = app.call_tool("lcd_text", &json!({ "text": "hi" })).unwrap_err();
        assert_eq!(err.code, code::NOT_FOUND);
        assert!(err.message.contains("lcd_text"), "{}", err.message);
        // Once the hardware is declared, the same call reaches the driver.
        assert!(app.call_tool("devices_list", &json!({})).is_ok());
    }

    #[test]
    fn a_tool_call_reaches_the_driver_and_returns_json() {
        let mut app = test_app();
        let v = app.call_tool("bme280_read", &json!({})).unwrap();
        let t = v["temperature_c"].as_f64().unwrap();
        assert!((t - 25.0).abs() < 2.0, "temperature {t}");
    }

    #[test]
    fn notification_gets_no_body_and_requests_do() {
        let mut app = test_app();
        assert!(app.handle_rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, None).is_none());
        assert!(app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#, None).is_some());
    }

    #[test]
    fn initialize_names_the_detected_hardware() {
        let mut app = test_app();
        let v = app
            .handle_rpc(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"t"}}}"#,
                None,
            )
            .unwrap();
        let text = v["result"]["instructions"].as_str().unwrap();
        assert!(text.contains("BME280"), "instructions: {text}");
        assert!(text.contains("manifest_apply"));
        assert_eq!(v["result"]["protocolVersion"], protocol::PROTOCOL_VERSION);
    }

    #[test]
    fn changing_i2c_pins_asks_for_a_restart_instead_of_pretending() {
        let mut app = test_app();
        let resp = app
            .route(&post("/api/config", r#"{"bus":{"i2c":[{"bus":0,"sda":41,"scl":42,"freq_hz":400000}]}}"#));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["restart_required"], true, "a pin change cannot take effect in place");
        assert!(app.runtime.restart_requested, "the platform must be told to restart");
    }

    #[test]
    fn a_config_change_that_moves_no_pin_asks_for_no_restart() {
        let mut app = test_app();
        let resp = app.route(&post("/api/config", r#"{"device_name":"bench"}"#));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["restart_required"], false, "only pins, the board or the port need one");
        assert!(!app.runtime.restart_requested, "no restart for a cosmetic edit");
    }

    #[test]
    fn a_manifest_is_applied_live_and_its_tool_works() {
        let mut app = test_app();
        assert!(!app.is_visible("pin_led"));
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({
                    "manifest": {
                        "tools": [{
                            "name": "pin_led",
                            "description": "Drive the onboard LED pin",
                            "params": [{ "name": "on", "type": "boolean", "required": true }],
                            "exec": { "type": "gpio_write", "pin": 48, "level": { "param": "on" } }
                        }]
                    }
                }),
            )
            .unwrap();
        assert_eq!(v["applied"], true);
        assert!(app.is_visible("pin_led"), "no reboot, tool is live");
        let names = tool_names(&mut app);
        assert!(names.contains(&"pin_led".to_string()));

        let out = app.call_tool("pin_led", &json!({ "on": true })).unwrap();
        assert_eq!(out["level"], 1);
        assert_eq!(out["pin"], 48);
    }

    /// Every primitive in a manifest runs against the real `Hardware` trait, so each one is
    /// exercised end to end here: the JSON in the tool result is the only evidence the model
    /// gets that a pin moved, and it has to come from the board rather than from the tool's
    /// own arguments.
    #[test]
    fn every_manifest_primitive_reaches_the_virtual_board() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [
                    { "name": "read_pin", "params": [], "exec": { "type": "gpio_read", "pin": 3 } },
                    { "name": "blink_pin", "params": [],
                      "exec": { "type": "toggle", "pin": 4, "times": 2, "gap_ms": 10 } },
                    { "name": "adc_pin", "params": [], "exec": { "type": "adc", "pin": 3 } },
                    { "name": "read_reg", "params": [],
                      "exec": { "type": "i2c_read", "addr": 118, "reg": 208, "length": 1, "bus": 0 } },
                    { "name": "write_reg", "params": [],
                      "exec": { "type": "i2c_write", "addr": 68, "reg": 36, "data": [0, 0], "bus": 0 } },
                    { "name": "rgb_led", "params": [],
                      "exec": { "type": "rgb", "pin": 5, "color": "#102030", "count": 4 } },
                    { "name": "pause", "params": [], "exec": { "type": "delay", "ms": 5 } },
                    { "name": "two_step", "params": [], "exec": { "type": "sequence", "steps": [
                        { "ms": 5, "exec": { "type": "gpio_write", "pin": 6, "level": 1 } },
                        { "exec": { "type": "gpio_read", "pin": 6 } }] } },
                    { "name": "servo_pin", "params": [{ "name": "d", "type": "number", "required": true }],
                      "exec": { "type": "pwm", "pin": 8, "freq_hz": 50, "duty_pct": { "param": "d" } } }
                ] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], true, "{v}");

        // A pull-up input reads high when nothing pulls it down.
        let out = app.call_tool("read_pin", &json!({})).unwrap();
        assert_eq!(out["pin"], 3);
        assert_eq!(out["level"], 1, "the virtual pin floats high: {out}");

        let out = app.call_tool("blink_pin", &json!({})).unwrap();
        assert_eq!((out["times"].as_u64(), out["gap_ms"].as_u64()), (Some(2), Some(10)));

        let out = app.call_tool("adc_pin", &json!({})).unwrap();
        assert_eq!(out["millivolts"], 1650, "the virtual ADC's documented value: {out}");

        // The BME280 the virtual board has at 0x76 answers its id register with 0x60.
        let out = app.call_tool("read_reg", &json!({})).unwrap();
        assert_eq!(out["addr"], "0x76");
        assert_eq!(out["reg"], "0xD0");
        assert_eq!(out["hex"], "60", "{out}");
        assert_eq!(out["bytes"], json!([0x60]));

        let out = app.call_tool("write_reg", &json!({})).unwrap();
        assert_eq!(out["addr"], "0x44");
        assert_eq!(out["bytes_written"], 2);

        let out = app.call_tool("rgb_led", &json!({})).unwrap();
        assert_eq!(out["color"], "#102030");
        assert_eq!(out["pin"], 5);

        let out = app.call_tool("pause", &json!({})).unwrap();
        assert_eq!(out["slept_ms"], 5);

        // A sequence reports each step's result in order, and the delay belongs to the step
        // it was declared on.
        let out = app.call_tool("two_step", &json!({})).unwrap();
        assert_eq!(out["steps_run"], 2);
        assert_eq!(out["results"][0]["level"], 1);
        assert_eq!(out["results"][1]["level"], 1, "the pin written a moment ago reads back high");

        let out = app.call_tool("servo_pin", &json!({ "d": 50 })).unwrap();
        assert_eq!(out["freq_hz"], 50);
        assert_eq!(out["duty_pct"], 50.0);
    }

    /// The exec has its own guard for a duty cycle that arrives through a parameter, because
    /// a parameter's range is only known at call time. A 150 % duty would otherwise reach
    /// the PWM driver.
    #[test]
    fn a_duty_cycle_out_of_range_is_refused_at_call_time() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [{
                    "name": "wide_pwm", "params": [{ "name": "d", "type": "number", "required": true }],
                    "exec": { "type": "pwm", "pin": 8, "freq_hz": 50, "duty_pct": { "param": "d" } }
                }] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], true, "{v}");
        let err = app.call_tool("wide_pwm", &json!({ "d": 150 })).unwrap_err();
        assert_eq!(err.code, code::FORBIDDEN);
        assert!(err.message.contains("0..=100"), "{}", err.message);
    }

    /// The tool closures on a driver's own tools are the only path a model's call takes
    /// into it, so at least one device is driven through `tools()` rather than through
    /// `Device::call` directly: the OLED the virtual board already has.
    #[test]
    fn a_drivers_own_tools_are_reachable_by_the_name_the_model_sees() {
        let mut app = test_app();
        let out = app.call_tool("oled_text", &json!({ "text": "OK", "line": 1 })).unwrap();
        assert_eq!(out["chars"], 2, "{out}");
        let out = app.call_tool("oled_read", &json!({})).unwrap();
        assert!(out["text"].as_str().unwrap_or("").contains("OK"), "{out}");
        app.call_tool("oled_rect", &json!({ "x": 0, "y": 0, "w": 4, "h": 4 })).unwrap();
        app.call_tool("oled_bitmap", &json!({ "data_b64": crate::util::b64_encode(&vec![0u8; 1024]) }))
            .unwrap();
        app.call_tool("oled_contrast", &json!({ "value": 0x80 })).unwrap();
        app.call_tool("oled_clear", &json!({})).unwrap();
    }

    /// A manifest tool with a rate limit is the only thing standing between a looping model
    /// and a relay chattering at the speed of the network.
    #[test]
    fn a_manifest_tool_rate_limit_refuses_the_next_call() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [{
                    "name": "once_a_minute", "params": [], "safety": { "max_calls_per_min": 1 },
                    "exec": { "type": "gpio_write", "pin": 5, "level": 1 }
                }] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], true, "{v}");
        assert!(app.call_tool("once_a_minute", &json!({})).is_ok(), "the first call is allowed");
        let err = app.call_tool("once_a_minute", &json!({})).unwrap_err();
        assert_eq!(err.code, code::FORBIDDEN);
        assert!(err.message.contains("rate limited"), "{}", err.message);
    }

    #[test]
    fn an_invalid_manifest_is_rejected_with_paths_and_changes_nothing() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [{ "name": "bad name", "exec": { "type": "gpio_write", "pin": 99, "level": 1 } }] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], false);
        let issues = v["issues"].as_array().unwrap();
        assert!(issues.iter().any(|i| i["path"] == "/tools/0/name"));
        assert!(issues.iter().any(|i| i["path"] == "/tools/0/exec/pin"));
        assert!(!app.is_visible("bad name"));
    }

    /// The name a model call is allowed to carry. Not a style rule: OpenAI-compatible
    /// endpoints validate `function.name` against this class, and every request carries
    /// the whole tool list.
    fn wire_tool_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    fn wire_names(app: &App) -> Vec<String> {
        let specs = ToolHost::tool_specs(app);
        specs
            .as_array()
            .expect("tool_specs is an array")
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn dotted_pump() -> Value {
        json!({ "manifest": { "tools": [{
            "name": "pump.pulse",
            "description": "Energise the pump relay for ms milliseconds",
            "params": [{ "name": "ms", "type": "integer", "min": 10, "max": 5000, "required": true }],
            "exec": { "type": "gpio_write", "pin": 12, "level": 1 }
        }] } })
    }

    /// Every section of the reference has to arrive whole.
    ///
    /// A tool result is capped at `MAX_TOOL_RESULT_BYTES`; `manifest_describe` used to
    /// return the schema, the exec primitives, the driver catalogue, the live device dump
    /// and every example in one object. On a board the model therefore received the first
    /// ~2 KB and nothing else — it guessed `exec` types and spent its step budget doing it.
    #[test]
    fn every_describe_section_arrives_without_being_cut() {
        use crate::agent::MAX_TOOL_RESULT_BYTES;

        let mut app = test_app();
        for section in ["overview", "schema", "exec", "examples", "drivers"] {
            let text = ToolHost::call_tool(&mut app, "manifest_describe", &json!({ "section": section }))
                .unwrap_or_else(|e| panic!("section {section} failed: {e}"));
            assert!(
                text.len() <= MAX_TOOL_RESULT_BYTES,
                "section {section} is {} bytes, over the {MAX_TOOL_RESULT_BYTES} the model sees",
                text.len()
            );
            assert!(!text.contains("truncated"), "section {section} was cut: {text}");
        }
        for name in ["relay_with_safety", "servo_angle", "sensor_driver", "led_then_read_back"] {
            let text = ToolHost::call_tool(
                &mut app,
                "manifest_describe",
                &json!({ "section": "examples", "example": name }),
            )
            .unwrap();
            assert!(text.len() <= MAX_TOOL_RESULT_BYTES, "example {name} is {} bytes", text.len());
            assert!(text.contains("exec"), "example {name} arrived empty: {text}");
        }

        // The overview is the entry point: it names the sections and the examples.
        let overview = ToolHost::call_tool(&mut app, "manifest_describe", &json!({})).unwrap();
        for needle in ["exec", "schema", "relay_with_safety", "max_tools"] {
            assert!(overview.contains(needle), "the overview does not mention {needle}: {overview}");
        }

        // The driver catalogue is paged for the same reason: fourteen entries in one
        // result would be cut, so each one is asked for by name.
        let listed =
            ToolHost::call_tool(&mut app, "manifest_describe", &json!({ "section": "drivers" })).unwrap();
        let listed: Value = serde_json::from_str(&listed).unwrap();
        let kinds: Vec<String> = listed["drivers"]
            .as_array()
            .expect("a driver list")
            .iter()
            .filter_map(|d| d["kind"].as_str().map(|s| s.to_string()))
            .collect();
        assert!(kinds.len() >= 10, "the catalogue lost entries: {kinds:?}");
        assert!(listed["hint"].as_str().unwrap().contains("driver="), "{listed}");
        for kind in kinds {
            let text = ToolHost::call_tool(
                &mut app,
                "manifest_describe",
                &json!({ "section": "drivers", "driver": kind }),
            )
            .unwrap();
            assert!(text.len() <= MAX_TOOL_RESULT_BYTES, "driver {kind} is {} bytes", text.len());
            assert!(!text.contains("truncated"), "driver {kind} was cut");
        }
        // A section name that does not exist is refused with the list, not guessed at.
        let err =
            ToolHost::call_tool(&mut app, "manifest_describe", &json!({ "section": "nope" })).unwrap_err();
        assert!(err.contains("overview"), "{err}");
    }

    /// The model cannot tell a driver tool from a manifest tool from a file tool.
    ///
    /// One array, one shape, no source tag: the `category`/`core`/`mutating` fields
    /// exist for the portal and for `tools/list`, and are never sent. What differs is
    /// only the *position* — the board's tools (driver, system and manifest alike) are
    /// one alphabetical list with everything else, because the model's view is built from
    /// three sources (registry, agent-only tools, manifest) and a reader expects one order.
    #[test]
    fn every_tool_reaches_the_model_in_one_shape_and_a_predictable_order() {
        let mut app = test_app();
        app.call_tool(
            "manifest_apply",
            &json!({ "manifest": { "tools": [{
                "name": "aaa_manifest", "params": [],
                "exec": { "type": "gpio_read", "pin": 3 }
            }] } }),
        )
        .expect("manifest applied");

        let specs = ToolHost::tool_specs(&app);
        let tools = specs.as_array().expect("an array");
        let names: Vec<&str> =
            tools.iter().map(|t| t["function"]["name"].as_str().expect("a name")).collect();

        // One shape for every entry, whichever module built it.
        for tool in tools {
            let top = tool.as_object().expect("an object");
            assert_eq!(top.len(), 2, "extra fields reach the wire: {tool}");
            assert!(top.contains_key("type") && top.contains_key("function"), "{tool}");
            assert_eq!(tool["type"], "function");
            let function = tool["function"].as_object().expect("a function object");
            assert_eq!(function.len(), 3, "extra fields reach the wire: {tool}");
            for key in ["name", "description", "parameters"] {
                assert!(function.contains_key(key), "{tool}");
            }
            assert!(tool["function"]["parameters"].is_object(), "{tool}");
        }

        // One list, sorted by name, whatever produced the entry: the board's own tools,
        // the agent-only fs_*/net_*/script group, and the manifest-compiled tools all sit
        // in the same alphabetical run. Stable for a given board (the next test checks the
        // bytes), and the order a reader expects.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "the tool list is not alphabetical: {names:?}");

        for needle in ["board", "manifest", "shell", "script", "read"] {
            assert!(names.contains(&needle), "{needle} is missing: {names:?}");
        }
        // A manifest tool sorts among the rest — `aaa_manifest` is first, not last.
        assert_eq!(names.first(), Some(&"aaa_manifest"), "manifest tools are not in the same list");
    }

    /// With `discovery = auto` the probe adopts what it finds, and an older manifest may
    /// still declare the same sensor. Declaring it again must not add a second device —
    /// on the bench that produced eight devices, two of everything, with copies renamed
    /// `led_set_2`.
    #[test]
    fn a_declaration_the_probe_already_adopted_is_a_no_op() {
        let mut app = test_app();
        let before = app.devices.infos().len();
        let declared: Vec<Value> = app
            .devices
            .infos()
            .iter()
            .filter(|i| i.address.is_some() || !i.pins.is_empty())
            .map(|i| {
                json!({
                    "id": format!("again_{}", i.id),
                    "kind": i.kind,
                    "driver": i.kind,
                    "bus": 0,
                    "address": i.address,
                    "pins": {},
                })
            })
            .collect();
        if declared.is_empty() {
            return; // a board with no addressable devices has nothing to duplicate
        }
        app.call_tool("manifest_apply", &json!({ "manifest": { "devices": declared } })).expect("applied");
        assert_eq!(app.devices.infos().len(), before, "a device declared twice became two devices");
    }

    /// The portal's tool page is the only place a person can see what exists. It used to
    /// list the registry only, so the agent's own tools — and `script` in particular —
    /// looked missing while the model had them all along.
    #[test]
    fn the_portal_lists_every_tool_the_model_has() {
        let mut app = test_app();
        let body = body_json(&app.route(&get("/api/tools")));
        let tools = body["tools"].as_array().expect("tools");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().expect("a name")).collect();

        for needle in ["script", "shell", "read", "board"] {
            assert!(names.contains(&needle), "{needle} is missing from the portal: {names:?}");
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "the portal list is not alphabetical: {names:?}");

        // The model has exactly the portal's *offered* rows. The comparison is on the
        // filtered set, deliberately: comparing the two full lists would go green while the
        // page still offered a tool the board had just decided not to send.
        let model: Vec<String> = ToolHost::tool_specs(&app)
            .as_array()
            .expect("array")
            .iter()
            .map(|t| t["function"]["name"].as_str().expect("name").to_string())
            .collect();
        let offered: Vec<&str> = tools
            .iter()
            .filter(|t| t["visible"] == json!(true))
            .map(|t| t["name"].as_str().expect("a name"))
            .collect();
        for name in &model {
            assert!(offered.contains(&name.as_str()), "{name} reaches the model but not the portal");
        }
        for name in &offered {
            assert!(model.iter().any(|m| m == name), "{name} is offered in the portal but not to the model");
        }

        // Every hidden row says why, and is really absent from the model's list. This board
        // has no camera, so `image_read` is the row that proves the page can report a tool
        // it deliberately does not offer.
        let hidden: Vec<&Value> = tools.iter().filter(|t| t["visible"] != json!(true)).collect();
        assert!(!hidden.is_empty(), "no camera on this board, so a hidden row must be shown");
        for row in hidden {
            let name = row["name"].as_str().unwrap_or("");
            assert!(!row["why"].as_str().unwrap_or("").is_empty(), "hidden without a reason: {row}");
            assert!(!model.iter().any(|m| m == name), "{name} is hidden in the portal but the model has it");
        }
    }

    /// The board's two faces are one set: what the model is handed, and what
    /// `/mcp tools/list` answers an outside client. They are built by different code (the
    /// OpenAI `parameters` shape versus MCP's `inputSchema`), so nothing but this
    /// assertion keeps them from drifting apart again.
    ///
    /// Both directions are checked, and the set is dropped to 47 by a real rule in the
    /// middle: a filter that only ever adds would pass a one-way comparison.
    #[test]
    fn the_agent_and_the_mcp_endpoint_offer_the_same_tools() {
        let mut app = test_app();
        let model_names = |app: &mut App| -> Vec<String> {
            ToolHost::tool_specs(app)
                .as_array()
                .expect("array")
                .iter()
                .map(|t| t["function"]["name"].as_str().expect("name").to_string())
                .collect()
        };
        let mcp_names = |app: &mut App| -> Vec<String> {
            let list = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
            list["result"]["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .map(|t| t["name"].as_str().expect("name").to_string())
                .collect()
        };

        assert_eq!(mcp_names(&mut app), model_names(&mut app), "the two lists disagree");
        assert!(!model_names(&mut app).contains(&"image_read".to_string()), "no camera, no image tool");

        app.cfg.mcp.disabled_tools = vec!["script".into()];
        assert_eq!(mcp_names(&mut app), model_names(&mut app), "the two lists disagree after a disable");
        assert!(!mcp_names(&mut app).contains(&"script".to_string()), "the disable did not reach MCP");
    }

    /// What `/api/status` counts is what the model receives, and every tool it does not
    /// offer is named with a reason.
    ///
    /// The count used to be the registry's, which left out `shell`, `script` and the file
    /// tools — so the status page said 42 while `/api/tools` said 48. A number that
    /// disagrees with another number is worse than no number: it sends a person looking for
    /// a bug in the wrong place.
    #[test]
    fn the_status_count_is_the_tool_list_the_model_gets() {
        let mut app = test_app();
        let status = body_json(&app.route(&get("/api/status")));
        let model = ToolHost::tool_specs(&app).as_array().expect("array").len();
        assert_eq!(status["mcp"]["tools_visible"], json!(model), "{status}");

        let hidden = status["mcp"]["tools_hidden"].as_array().expect("a hidden list");
        // Every tool that exists is either offered or explained, never neither.
        let registry = app.tools.len();
        let extras = App::extra_entries().len();
        assert_eq!(
            model + hidden.len(),
            registry + extras,
            "{} + {} != {registry} + {extras}",
            model,
            hidden.len()
        );
        for row in hidden {
            assert!(!row["why"].as_str().unwrap_or("").is_empty(), "hidden without a reason: {row}");
            assert!(!row["name"].as_str().unwrap_or("").is_empty(), "hidden without a name: {row}");
        }
    }

    /// The heap pool by pool, when the platform can split it, and a plain "cannot" when it
    /// cannot. The split is what a memory change is checked against: "internal RAM went up"
    /// is only good news if the pool that went up is the one the change was about.
    #[test]
    fn the_memory_route_splits_the_heap_or_says_it_cannot() {
        let mut app = test_app();
        let split = body_json(&app.route(&get("/api/memory")));
        assert_eq!(split["available"], json!(true), "{split}");
        let pools = split["pools"].as_array().expect("pools");
        assert!(pools.len() >= 2, "the virtual board has an internal and a PSRAM pool");
        for pool in pools {
            for key in ["pool", "note", "total_kb", "free_kb", "largest_kb", "min_free_kb"] {
                assert!(!pool[key].is_null(), "{} is missing {key}: {pool}", pool["pool"]);
            }
            assert!(
                pool["largest_kb"].as_u64().unwrap_or(0) <= pool["free_kb"].as_u64().unwrap_or(0),
                "the largest block cannot exceed the free total: {pool}"
            );
        }

        // A platform with one heap answers without pretending to have four.
        let mut bare = App::new(
            Box::new(crate::hardware::NullHardware),
            Box::new(SharedStore::new()),
            LogBuf::new(64),
            FwMeta::default(),
        );
        let one = body_json(&bare.route(&get("/api/memory")));
        assert_eq!(one["available"], json!(false), "{one}");
        assert!(one["why"].as_str().unwrap_or("").contains("cannot split"), "{one}");
    }

    /// Every thread in `/api/status` carries the stack it has left at its deepest point —
    /// the reading that decides whether a stack constant can come down. Without it the
    /// answer is "guess and watch for a reboot".
    #[test]
    fn status_reports_each_threads_stack_headroom() {
        let mut app = test_app();
        let status = body_json(&app.route(&get("/api/status")));
        let threads = status["threads"].as_array().expect("threads");
        assert_eq!(threads.len(), 3, "the virtual board reports three tasks: {status}");
        assert_eq!(threads[0]["name"], json!("main"));
        // 2048 bytes free is 2.0 kB, and the number is a float so a 4 KB stack can say 2.1.
        assert_eq!(threads[0]["high_water_kb"], json!(2.0));
        for t in threads {
            assert!(t["high_water_kb"].as_f64().is_some(), "every thread needs a high-water reading: {t}");
        }
    }

    /// A device the board does not have is not a tool, and the sentence explains it.
    ///
    /// `image_read` is the live case: a camera is a driver on a DVP bus, and a board with
    /// neither can only answer "a camera is not available on this board".
    #[test]
    fn a_tool_with_no_device_behind_it_is_hidden_and_says_why() {
        let mut app = test_app();
        assert!(!tool_names(&mut app).contains(&"image_read".to_string()));
        let why = app
            .hidden_tools()
            .into_iter()
            .find(|t| t["name"] == "image_read")
            .expect("image_read must be reported as hidden");
        assert!(why["why"].as_str().unwrap_or("").contains("camera"), "{why}");

        // Plug a camera in: the capability is the whole decision, so the tool appears while
        // the filter stays untouched. That is the reverse proof - a filter that could only
        // subtract would pass every test above.
        app.caps.insert("camera");
        assert!(
            tool_names(&mut app).contains(&"image_read".to_string()),
            "a camera must bring the tool back"
        );
        assert!(!app.hidden_tools().iter().any(|t| t["name"] == "image_read"));
    }

    /// The same board has to produce the same bytes, or the provider's prompt cache
    /// misses on every single turn.
    #[test]
    fn the_tool_list_is_byte_identical_for_the_same_board() {
        let manifest = json!({ "manifest": { "tools": [{
            "name": "bench_pulse",
            "params": [{ "name": "ms", "type": "integer", "min": 10, "max": 5000, "required": true }],
            "exec": { "type": "delay", "ms": { "param": "ms" } }
        }] } });
        let mut a = test_app();
        let mut b = test_app();
        a.call_tool("manifest_apply", &manifest).expect("apply a");
        b.call_tool("manifest_apply", &manifest).expect("apply b");

        let bytes = |app: &App| serde_json::to_string(&ToolHost::tool_specs(app)).expect("serialise");
        let first = bytes(&a);
        assert_eq!(first, bytes(&a), "two calls on one board produced different bytes");
        assert_eq!(first, bytes(&b), "two identically configured boards produced different bytes");

        // A re-probe must not reorder anything: same hardware, same bytes.
        a.call_tool("devices_probe", &json!({})).expect("probe");
        assert_eq!(first, bytes(&a), "a probe reshuffled the tool list");
    }

    /// `/AGENTS.md` and the skill index have to reach the model, or they are just files.
    ///
    /// The prompt is rebuilt every turn from the workspace, so editing the instructions
    /// or adding a skill takes effect on the next turn — no restart, no transcript
    /// rewrite, which is the only reason they are stored as files at all.
    #[test]
    fn the_instructions_and_the_skill_index_reach_the_system_prompt() {
        let mut app = test_app();
        ToolHost::call_tool(
            &mut app,
            "fs_write",
            &json!({ "path": "/AGENTS.md", "text": "Pin 12 is the pump relay, active low.\nAsk before switching it." }),
        )
        .expect("instructions written");
        ToolHost::call_tool(
            &mut app,
            "fs_write",
            &json!({ "path": "/skills/deploy/SKILL.md", "text": "---\ndescription: roll the portal forward safely\n---\n1. snapshot\n2. flash\n" }),
        )
        .expect("skill written");

        let seen = Arc::new(Mutex::new(Vec::new()));
        app.attach_model_transport(Box::new(StubModel { seen: seen.clone(), calls: 0 }));
        app.route(&post(
            "/api/agent/config",
            r#"{"base_url":"http://127.0.0.1:9/v1/chat/completions","model":"stub","api_key":""}"#,
        ));
        app.agent.push_user("what is on pin 12?", "http", 0);
        app.agent_run_once().expect("the turn should run");

        let sent = seen.lock().unwrap();
        let body: Value = serde_json::from_str(&sent[0].2).unwrap();
        let system = body["messages"][0]["content"].as_str().expect("a system message");
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(system.contains("Pin 12 is the pump relay"), "the instructions are missing: {system}");
        assert!(system.contains("roll the portal forward safely"), "the skill index is missing: {system}");
        assert!(!system.contains("1. snapshot"), "the skill *body* must not be in the prompt: {system}");
        assert!(system.contains("`read`"), "the prompt does not say how to load a skill: {system}");
        assert!(
            system.contains("/skills/deploy/SKILL.md"),
            "the index must carry the path to open, not just a name: {system}"
        );
    }

    /// A long file is carried as its head: the rest stays readable with `read`.
    #[test]
    fn a_long_agents_md_is_cut_in_the_prompt_but_still_readable() {
        let mut app = test_app();
        let long = format!("rule: {}\n", "x".repeat(1600));
        ToolHost::call_tool(&mut app, "fs_write", &json!({ "path": "/AGENTS.md", "text": long }))
            .expect("instructions written");

        let prompt = app.project_instructions().expect("instructions");
        assert!(prompt.len() < 1600, "the prompt carried the whole file: {} bytes", prompt.len());
        assert!(prompt.contains("truncated"), "{prompt}");
        // ... and the file still holds everything for `fs_read` to return.
        let text =
            ToolHost::call_tool(&mut app, "fs_read", &json!({ "path": "/AGENTS.md" })).expect("read back");
        assert!(text.len() > 1600, "the stored copy lost bytes: {}", text.len());
    }

    /// No dedicated tools for skills or instructions: they are files, and `fs_*` already
    /// reaches them. One wrapper per verb would cost a definition in every request.
    #[test]
    fn skills_and_instructions_have_no_tools_of_their_own() {
        let mut app = test_app();
        let names: Vec<String> = ToolHost::tool_specs(&app)
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(|s| s.to_string()))
            .collect();
        for absent in ["skill_list", "skill_read", "skill_write", "skill_delete", "agents_md"] {
            assert!(!names.contains(&absent.to_string()), "{absent} is back in the tool list: {names:?}");
        }
        // The file tools that do reach them are still there.
        for present in ["shell"] {
            assert!(names.contains(&present.to_string()), "{present} is missing: {names:?}");
        }

        // And the path works end to end: a skill directory written with `fs_write`,
        // material next to it, seen by the index with the path to open.
        ToolHost::call_tool(
            &mut app,
            "fs_write",
            &json!({
                "path": "/skills/pins/SKILL.md",
                "text": "---\ndescription: GPIO map for the bench\n---\n12 relay\n"
            }),
        )
        .expect("skill written");
        ToolHost::call_tool(
            &mut app,
            "fs_write",
            &json!({ "path": "/skills/pins/notes.md", "text": "relay is active low\n" }),
        )
        .expect("supporting file written");
        let index = app.skills();
        let pins = index
            .iter()
            .find(|s| s.name == "pins")
            .unwrap_or_else(|| panic!("the bench skill is not in the index: {index:?}"));
        assert_eq!(pins.description, "GPIO map for the bench");
        assert_eq!(pins.path, "/skills/pins/SKILL.md");

        let read = ToolHost::call_tool(&mut app, "fs_read", &json!({ "path": "/skills/pins/SKILL.md" }))
            .expect("fs_read");
        assert!(read.contains("12 relay"), "{read}");
        let sibling = ToolHost::call_tool(&mut app, "fs_read", &json!({ "path": "/skills/pins/notes.md" }))
            .expect("fs_read sibling");
        assert!(sibling.contains("active low"), "{sibling}");
    }

    /// A flat file is still a skill: the shape for one that needs nothing else.
    #[test]
    fn a_one_file_skill_still_lands_in_the_index() {
        let mut app = test_app();
        ToolHost::call_tool(
            &mut app,
            "fs_write",
            &json!({ "path": "/skills/quick.md", "text": "# quick\ndescription: one-file skill\nbody\n" }),
        )
        .expect("skill written");
        let index = app.skills();
        let quick = index
            .iter()
            .find(|s| s.name == "quick")
            .unwrap_or_else(|| panic!("the flat skill is not in the index: {index:?}"));
        assert_eq!(quick.path, "/skills/quick.md");
        assert_eq!(quick.description, "one-file skill");
    }

    /// Deleting the SKILL.md is how a skill leaves the prompt, which is why the workspace
    /// needed a delete at all.
    #[test]
    fn deleting_a_skill_file_removes_it_from_the_index() {
        let mut app = test_app();
        ToolHost::call_tool(
            &mut app,
            "fs_write",
            &json!({ "path": "/skills/pins/SKILL.md", "text": "---\ndescription: GPIO map\n---\n12 relay\n" }),
        )
        .unwrap();
        assert!(app.skills().iter().any(|s| s.name == "pins"));

        let gone = ToolHost::call_tool(
            &mut app,
            "fs_rm",
            &json!({ "path": "/skills/pins/SKILL.md", "confirm": true }),
        )
        .expect("fs_rm");
        assert!(gone.contains("removed"), "{gone}");
        assert!(
            !app.skills().iter().any(|s| s.name == "pins"),
            "the index still lists a deleted skill: {:?}",
            app.skills()
        );
        // Only the named file went: the skills the firmware seeded are still indexed.
        assert_eq!(app.skills().len(), crate::seed::DEFAULT_SKILLS.len(), "{:?}", app.skills());
    }

    /// The seed is an install, not a rewrite, and it runs on every boot: a path that already
    /// holds text is left alone, so an owner's edit survives a power cut. (The other half -
    /// a deleted skill coming back - is `seed::tests::a_skill_deleted_on_the_board_is_seeded_again`,
    /// which needs one skill and no app.)
    #[test]
    fn a_reboot_keeps_the_owners_edit_to_a_seeded_skill() {
        let store = SharedStore::new();
        {
            let mut app = app_with(&store);
            assert_eq!(app.skills().len(), crate::seed::DEFAULT_SKILLS.len());
            ToolHost::call_tool(
                &mut app,
                "fs_edit",
                &json!({
                    "path": "/skills/create-skill/SKILL.md",
                    "find": "description: write a new skill for this board when a procedure had to be worked out",
                    "replace": "description: our own house style for skills"
                }),
            )
            .expect("the owner edits the skill");
            // The owner's edit is queued; the writer flushes it, and then the power goes out.
            drain_store(&mut app);
        }

        // The power goes out, and comes back to a new App over the same flash.
        let (mut app, log) = app_with_log(&store);
        let index = app.skills();
        assert_eq!(index.len(), crate::seed::DEFAULT_SKILLS.len(), "{index:?}");
        let skill = index.iter().find(|s| s.name == "create-skill").expect("it is indexed");
        assert_eq!(skill.description, "our own house style for skills", "the edit was overwritten");
        assert!(
            !log.tail(50, 0).iter().any(|l| l.message.contains("seeded")),
            "nothing was installed over an existing skill: {:?}",
            log.tail(50, 0).iter().map(|l| &l.message).collect::<Vec<_>>()
        );
        // A second boot over a complete workspace writes nothing at all.
        assert_eq!(app.seed_default_skills(), 0);
    }

    /// `generic` is the lean mode: per-device tools fold into `device_invoke`, so a board
    /// with six peripherals spends six definitions instead of twenty.
    #[test]
    fn generic_mode_folds_device_tools_and_can_be_set_from_a_config_patch() {
        let mut app = test_app();
        assert!(app.is_visible("bme280_read"), "the per-device tool starts visible");
        assert!(app.is_visible("device_invoke"));

        let v =
            app.call_tool("config_set", &json!({ "patch": { "mcp": { "tools_mode": "generic" } } })).unwrap();
        assert!(v["touched"].as_array().unwrap().iter().any(|t| t == "mcp.tools_mode"), "{v}");

        for folded in ["bme280_read", "sht3x_read", "power_read", "oled_text"] {
            assert!(
                !app.is_visible(folded),
                "{folded} survived generic mode (device_tool={:?}, mode={})",
                app.tools.get(folded).map(|d| d.spec.device_tool),
                app.cfg.mcp.tools_mode
            );
        }
        assert!(app.is_visible("device_invoke"), "the way in must stay");
        // ... and it still reaches the device.
        let out = app
            .call_tool("device_invoke", &json!({ "device": "bme280", "tool": "bme280_read" }))
            .expect("device_invoke");
        assert!(out["temperature_c"].is_number(), "{out}");
    }

    /// A typo in the mode used to be ignored, which left the board on the old value and
    /// the caller believing it had changed.
    #[test]
    fn an_unknown_tools_mode_is_refused() {
        let mut app = test_app();
        let err = app
            .call_tool("config_set", &json!({ "patch": { "mcp": { "tools_mode": "generik" } } }))
            .unwrap_err();
        assert!(err.message.contains("auto, generic or all"), "{err:?}");
        assert_eq!(app.cfg.mcp.tools_mode, "auto", "the value changed anyway");
    }

    /// The devkit's own peripherals are opt-out: a board that wants the smallest possible
    /// surface comes up with the bus, GPIO and manifest tools, and has its LED and button
    /// back by declaring them — which is also how a different devkit starts small.
    #[test]
    fn a_minimal_board_starts_without_the_devkits_own_peripherals() {
        let mut app = test_app();
        assert!(app.is_visible("indicators"), "the profile's LED and buttons are there by default");

        // Through the config path a person or the agent would use.
        app.call_tool("config_set", &json!({ "patch": { "auto_peripherals": false } })).unwrap();
        for gone in ["led_set", "led_blink", "buttons_read", "button_wait"] {
            assert!(!app.is_visible(gone), "{gone} survived a minimal start");
        }
        assert!(app.is_visible("bus"), "the primitives must stay");
        assert!(app.is_visible("manifest"), "self-configuration must stay");

        // Declaring them in the manifest brings the tools back — the whole point.
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "devices": [
                    { "id": "rgb", "kind": "led", "driver": "led", "pins": { "out": 48 },
                      "options": { "kind": "ws2812" } },
                    { "id": "boot", "kind": "buttons", "driver": "buttons", "pins": { "boot": 0 } }
                ] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], true, "{v}");
        assert!(app.is_visible("indicators"), "the declared LED did not appear");
        assert!(app.is_visible("indicators"), "the declared button did not appear");
        let out = app.call_tool("led_set", &json!({ "color": "#010203" })).unwrap();
        assert_eq!(out["color"], json!("#010203"), "{out}");
    }

    /// I2S cannot be probed, so the tools exist exactly when the pins are wired: the
    /// devkit profile has no microphone, the xiaozhi wiring has both.
    #[test]
    fn the_microphone_and_amplifier_exist_only_when_the_i2s_pins_are_wired() {
        let mut app = test_app();
        assert!(!app.is_visible("mic_level"), "the devkit profile has no I2S");

        app.cfg.board = "esp32s3-xiaozhi-bread".into();
        app.rebuild();
        // Both I2S devices are there, so the `audio` family is; its commands are the four
        // tools, and `audio help` is what names them.
        assert!(app.is_visible("audio"), "the microphone and amplifier must bring `audio`");
        let help = app.call_tool("audio", &json!({ "command": "help" })).unwrap();
        let commands: Vec<String> =
            help["commands"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
        for tool in ["mic_level", "mic_record", "speaker_tone", "speaker_play"] {
            assert!(commands.contains(&tool.to_string()), "{tool} missing: {commands:?}");
        }

        // The capabilities ride on the devices, so a board that cannot bring I2S up never
        // claims them (that was the bench bug this replaces).
        let inv = app.call_tool("inventory", &json!({})).unwrap();
        let caps: Vec<String> =
            inv["capabilities"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
        assert!(caps.contains(&"mic".to_string()) && caps.contains(&"speaker".to_string()), "{caps:?}");

        let level = app.call_tool("mic_level", &json!({ "ms": 50 })).unwrap();
        assert!(level["dbfs"].is_number(), "the level meter read nothing: {level}");

        let rec = app.call_tool("mic_record", &json!({ "samples": 64 })).unwrap();
        assert_eq!(rec["samples"], json!(64), "{rec}");
        // The board records WAV: the result says so, and the payload is header + samples.
        assert_eq!(rec["format"], json!("wav"), "a recording states its format: {rec}");
        assert_eq!(rec["wav_bytes"], json!(44 + 64 * 2), "44-byte header, 16-bit mono: {rec}");
        let mut wave = Vec::new();
        crate::util::b64_decode(rec["wav_b64"].as_str().unwrap(), &mut wave).expect("the payload is base64");
        assert_eq!(&wave[0..4], b"RIFF", "the header travels with the samples");
        assert!(
            rec["wav_b64"].as_str().unwrap().len() <= crate::agent::MAX_TOOL_RESULT_BYTES,
            "a tool result is capped at {} bytes",
            crate::agent::MAX_TOOL_RESULT_BYTES
        );

        let tone = app.call_tool("speaker_tone", &json!({ "freq_hz": 440, "ms": 50 })).unwrap();
        assert_eq!(tone["ok"], json!(true), "{tone}");
        assert!(tone["samples"].as_u64().unwrap() >= 400, "{tone}");
    }

    /// Two WS2812 outputs (the devkit's own on 48 and the xiaozhi strip on 18) both want
    /// `led_set`; without distinct names the second silently replaced the first in the
    /// registry, and `devices_list` still claimed both were reachable.
    #[test]
    fn two_devices_of_one_kind_get_distinct_tool_names() {
        let mut app = test_app();
        assert!(app.is_visible("indicators"), "the devkit's LED starts inside `indicators`");

        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "devices": [
                    { "id": "lamp", "kind": "led", "driver": "led", "pins": { "out": 18 },
                      "options": { "kind": "ws2812", "count": "8" } }
                ] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], true, "{v}");
        // The second strip's tools are suffixed, and the family is what carries both.
        let help = app.call_tool("indicators", &json!({ "command": "help" })).unwrap();
        let commands: Vec<String> =
            help["commands"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
        assert!(commands.contains(&"led_set".to_string()), "{commands:?}");
        assert!(commands.contains(&"led_set_2".to_string()), "the strip has no tool: {commands:?}");

        // The two views agree: `devices_list` names the same suffixed tool.
        let inv = app.call_tool("devices_list", &json!({})).unwrap();
        let ids: Vec<String> = inv["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap_or("?").to_string())
            .collect();
        let lamp = inv["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == "lamp")
            .unwrap_or_else(|| panic!("no lamp device: {ids:?}"));
        let tools: Vec<String> =
            lamp["tools"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
        assert!(tools.contains(&"led_set_2".to_string()), "{tools:?}");

        // ... and the second one is answered by the strip, not refused as unknown.
        let out = app.call_tool("led_set_2", &json!({ "color": "#102030" })).unwrap();
        assert_eq!(out["ok"], json!(true), "{out}");
    }

    #[test]
    fn a_manifest_cannot_shadow_a_board_tool() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [{
                    "name": "manifest_apply", "params": [],
                    "exec": { "type": "delay", "ms": 1 }
                }] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], false, "a manifest tool took `manifest_apply`: {v}");
        let issue = v["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == "/tools/0/name")
            .expect("an issue on the name");
        assert!(issue["message"].as_str().unwrap().contains("manifest_apply"), "{issue}");
        assert!(issue["hint"].as_str().unwrap().contains("my_"), "the hint must offer a rename: {issue}");
        // The board kept its own tool.
        let v = app.call_tool("manifest_apply", &json!({ "manifest": { "tools": [] } })).unwrap();
        assert_eq!(v["applied"], true, "{v}");
    }

    /// The `manifest_` prefix is not reserved — only a name the board actually uses is.
    #[test]
    fn a_manifest_may_use_the_manifest_prefix_but_not_a_taken_name() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [{
                    "name": "bme280_read", "params": [],
                    "exec": { "type": "delay", "ms": 1 }
                }] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], false, "a manifest tool took a driver tool: {v}");

        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [{
                    "name": "manifest_helper", "params": [],
                    "exec": { "type": "delay", "ms": 1 }
                }] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], true, "a free name with the prefix must be allowed: {v}");
        assert!(app.is_visible("manifest_helper"));
    }

    /// A manifest stored by an older build is not re-validated on boot, so the rebuild
    /// has to refuse the clash itself — the board's own tool wins.
    #[test]
    fn a_stale_manifest_cannot_take_a_board_tool_either() {
        let mut app = test_app();
        app.manifest = crate::manifest::Manifest::from_json_str(
            r#"{"tools":[{"name":"manifest_apply","params":[],"exec":{"type":"delay","ms":1}}]}"#,
        )
        .expect("a manifest");
        app.rebuild();
        // The board's own `manifest` tool wins over the manifest entry of the same name, and
        // the five old command names stay reachable through it.
        assert!(app.is_visible("manifest"), "the built-in disappeared on rebuild");
        let v = app.call_tool("manifest_apply", &json!({ "manifest": { "tools": [] } })).unwrap();
        assert_eq!(v["applied"], true, "the built-in no longer applies: {v}");
    }

    /// A manifest must not be able to name a tool the endpoint will refuse.
    ///
    /// `apply` is where the refusal has to happen: a name that reaches the wire fails
    /// the *model call*, and every later call with it, so the board cannot even ask the
    /// model to fix the manifest. Remove the charset check and this assertion fails.
    #[test]
    fn a_dotted_tool_name_is_refused_by_name_and_charset() {
        let mut app = test_app();
        let v = app.call_tool("manifest_apply", &dotted_pump()).unwrap();
        assert_eq!(v["applied"], false, "`pump.pulse` must not be applied: {v}");
        let issues = v["issues"].as_array().expect("issues");
        let issue = issues
            .iter()
            .find(|i| i["path"] == "/tools/0/name")
            .unwrap_or_else(|| panic!("no /tools/0/name issue: {v}"));
        let message = issue["message"].as_str().unwrap_or_default();
        let hint = issue["hint"].as_str().unwrap_or_default();
        assert!(message.contains("pump.pulse"), "the offender is not named: {message}");
        for needle in ["a-z", "`_`", "`-`", "led_set"] {
            assert!(hint.contains(needle), "the charset is not spelled out ({needle}): {hint}");
        }
        assert!(!app.is_visible("pump.pulse"), "the refused tool still answers");
    }

    /// A refused manifest leaves nothing behind: the list, a call and a model round
    /// all still work. Turn the refusal into a pass-through and the wire carries
    /// `pump.pulse`, which is what fails here.
    #[test]
    fn a_refused_manifest_poisons_nothing() {
        let mut app = test_app();
        let refused = app.call_tool("manifest_apply", &dotted_pump()).unwrap();
        assert_eq!(refused["applied"], false);

        // tools/list still answers, and the board's own tools are still in it.
        let list = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
        let listed = list["result"]["tools"].as_array().expect("tools array");
        assert!(listed.iter().any(|t| t["name"] == "board"), "tools/list lost the built-ins");

        // A normal call still reaches the hardware.
        assert!(app.call_tool("board_status", &json!({})).is_ok(), "the board stopped answering");

        // And a model round still happens. The recorded request is the wire payload
        // the endpoint would have refused.
        let seen = Arc::new(Mutex::new(Vec::new()));
        app.attach_model_transport(Box::new(StubModel { seen: seen.clone(), calls: 0 }));
        app.route(&post(
            "/api/agent/config",
            r#"{"base_url":"http://127.0.0.1:9/v1/chat/completions","model":"stub","api_key":""}"#,
        ));
        app.agent.push_user("is the board ok?", "http", 0);
        let answer = app.agent_run_once().expect("the model call must still go out");
        assert_eq!(answer, "The board is fine.");

        for (_, _, body) in seen.lock().unwrap().iter() {
            let sent: Value = serde_json::from_str(body).unwrap();
            for tool in sent["tools"].as_array().expect("tools array") {
                let name = tool["function"]["name"].as_str().unwrap_or_default();
                assert!(wire_tool_name(name), "`{name}` would fail the model call: {body}");
            }
        }
    }

    /// Everything the board can put on the wire, manifest tools and built-ins alike.
    #[test]
    fn every_tool_name_on_the_wire_is_one_an_openai_endpoint_accepts() {
        let mut app = test_app();
        let refused = app.call_tool("manifest_apply", &dotted_pump()).unwrap();
        assert_eq!(refused["applied"], false);
        let applied = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [{
                    "name": "pump_pulse",
                    "params": [{ "name": "ms", "type": "integer", "required": true }],
                    "exec": { "type": "gpio_write", "pin": 12, "level": 1 }
                }] } }),
            )
            .unwrap();
        assert_eq!(applied["applied"], true);

        let names = wire_names(&app);
        assert!(names.contains(&"pump_pulse".to_string()), "got {names:?}");
        assert!(names.contains(&"board".to_string()), "got {names:?}");
        for name in &names {
            assert!(wire_tool_name(name), "`{name}` would be refused by the endpoint: {names:?}");
        }
    }

    /// The shape error has to be actionable: deepseek-flash sent the tools array as the
    /// whole document, got `schema mismatch: invalid type: sequence, expected a map`
    /// back, and spent six steps guessing. Put that wording back and this fails.
    #[test]
    fn a_manifest_of_the_wrong_shape_is_told_which_shape_it_needs() {
        let mut app = test_app();

        let err = app
            .call_tool("manifest_validate", &json!({ "manifest": [{ "name": "pump_pulse" }] }))
            .unwrap_err();
        let text = err.message;
        for needle in ["must be a JSON object", "devices", "tools", "pump_pulse"] {
            assert!(text.contains(needle), "the expected shape is not in the error ({needle}): {text}");
        }

        let err = app
            .call_tool("manifest_validate", &json!({ "manifest": { "tools": [{ "name": 7, "exec": {} }] } }))
            .unwrap_err();
        assert!(err.message.contains("/tools/0/name"), "no field path: {}", err.message);
        assert!(err.message.contains("devices") && err.message.contains("tools"), "{}", err.message);

        let err = app.call_tool("manifest_validate", &json!({ "manifest": { "tool": [] } })).unwrap_err();
        assert!(err.message.contains("/tool"), "a typo in a field name must be named: {}", err.message);

        // A document of the right shape still validates.
        let ok = app
            .call_tool(
                "manifest_validate",
                &json!({ "manifest": { "tools": [{
                    "name": "pump_pulse", "params": [],
                    "exec": { "type": "gpio_write", "pin": 12, "level": 1 } }] } }),
            )
            .unwrap();
        assert_eq!(ok["valid"], true, "{ok}");
    }

    #[test]
    fn safety_limits_are_enforced_on_the_board() {
        let mut app = test_app();
        app.call_tool(
            "manifest_apply",
            &json!({ "manifest": { "tools": [{
                "name": "servo_set",
                "params": [{ "name": "deg", "type": "number", "required": true, "min": 0, "max": 180 }],
                "exec": { "type": "pwm", "pin": 7, "freq_hz": 50, "duty_pct": { "param": "deg" } },
                "safety": { "min": 5, "max": 175 }
            }] } }),
        )
        .unwrap();
        // Schema range would allow 180; the safety range does not.
        let err = app.call_tool("servo_set", &json!({ "deg": 180 })).unwrap_err();
        assert_eq!(err.code, code::FORBIDDEN);
        assert!(err.message.contains("safety range"));
        assert!(app.call_tool("servo_set", &json!({ "deg": 90 })).is_ok());
    }

    #[test]
    fn confirm_flag_is_required_for_dangerous_tools() {
        let mut app = test_app();
        app.call_tool(
            "manifest_apply",
            &json!({ "manifest": { "tools": [{
                "name": "relay_pulse",
                "params": [],
                "exec": { "type": "gpio_write", "pin": 5, "level": 1 },
                "safety": { "confirm": true }
            }] } }),
        )
        .unwrap();
        let err = app.call_tool("relay_pulse", &json!({})).unwrap_err();
        assert_eq!(err.code, code::FORBIDDEN);
        assert!(app.call_tool("relay_pulse", &json!({ "confirm": true })).is_ok());
    }

    #[test]
    fn manifest_survives_a_restart_through_the_store() {
        let log = LogBuf::new(64);
        let hw = Box::new(VirtualBoard::new());
        let store: Box<dyn KvStore> = Box::new(crate::store::FileStore::open(
            std::env::temp_dir().join(format!("esp32-agent-test-{}.json", std::process::id())),
        ));
        let mut app = App::new(hw, store, log.clone(), FwMeta::default());
        app.call_tool(
            "manifest_apply",
            &json!({ "manifest": { "tools": [{
                "name": "p_keep", "params": [],
                "exec": { "type": "gpio_read", "pin": 3 } }] } }),
        )
        .unwrap();
        assert_eq!(app.runtime.manifest_rev, 2, "one for boot, one for the save");
        let saved = app.manifest.to_json_string();
        let reloaded = Manifest::from_json_str(&saved).unwrap();
        assert_eq!(reloaded.tool_names(), vec!["p_keep".to_string()]);
    }

    #[test]
    fn config_patch_persists_masks_secrets_and_never_reboots() {
        let mut app = test_app();
        app.cfg.auth.mcp_token = "secret".into();
        assert_eq!(app.cfg.public_json()["auth"]["mcp_token"], "__set__");
        let req = Request {
            method: "POST".into(),
            path: "/api/config".into(),
            query: Default::default(),
            headers: Default::default(),
            body: br#"{"device_name":"bench","mcp":{"tools_mode":"auto"}}"#.to_vec(),
            peer: "test".into(),
            scheme: "http",
        };
        let resp = app.route(&req);
        assert_eq!(resp.status, 200);
        assert_eq!(app.cfg.device_name, "bench");
        assert!(!app.runtime.config_dirty);
        assert!(!app.runtime.restart_requested);
    }

    #[test]
    fn bearer_token_is_enforced_on_the_mcp_endpoint() {
        let mut app = test_app();
        app.cfg.auth.mcp_token = "tok".into();
        let mut req = Request {
            method: "POST".into(),
            path: "/mcp".into(),
            query: Default::default(),
            headers: Default::default(),
            body: br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_vec(),
            peer: "test".into(),
            scheme: "http",
        };
        assert_eq!(app.route(&req).status, 401);
        req.headers.insert("authorization".into(), "Bearer tok".into());
        let resp = app.route(&req);
        assert_eq!(resp.status, 200);
        assert!(resp.headers.iter().any(|(k, _)| k == "Mcp-Session-Id"));
    }

    #[test]
    fn portal_pin_gates_writes_but_not_reads() {
        let mut app = test_app();
        app.cfg.auth.portal_pin = "1234".into();
        let write = Request {
            method: "POST".into(),
            path: "/api/config".into(),
            query: Default::default(),
            headers: Default::default(),
            body: br#"{"device_name":"x"}"#.to_vec(),
            peer: "t".into(),
            scheme: "http",
        };
        assert_eq!(app.route(&write).status, 401);
        let read = Request { method: "GET".into(), path: "/api/status".into(), ..write.clone() };
        assert_eq!(app.route(&read).status, 200);
    }

    #[test]
    fn settings_mode_generic_is_accepted_and_rejected() {
        let mut app = test_app();
        let req = Request {
            method: "POST".into(),
            path: "/api/tools".into(),
            query: Default::default(),
            headers: Default::default(),
            body: br#"{"mode":"generic"}"#.to_vec(),
            peer: "t".into(),
            scheme: "http",
        };
        assert_eq!(app.route(&req).status, 200);
        assert_eq!(app.cfg.mcp.tools_mode, "generic");
    }

    #[test]
    fn device_invoke_cannot_bypass_a_manifest_tools_confirmation() {
        // A driver-mode manifest tool declares `confirm`; the generic entry point
        // must reach the same verdict, or the write happens unconfirmed.
        let mut app = test_app();
        app.cfg.mcp.tools_mode = "all".into();
        let applied = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "tools": [{
                    "name": "climate_read",
                    "description": "blocking read, needs an explicit confirmation",
                    "params": [],
                    "exec": { "type": "driver", "driver": "bme280", "action": "bme280_read" },
                    "safety": { "confirm": true }
                }] } }),
            )
            .unwrap();
        assert_eq!(applied["applied"], true, "apply rejected: {applied}");
        let err = app
            .call_tool(
                "device_invoke",
                &json!({ "device": "bme280", "tool": "bme280_read", "arguments": {} }),
            )
            .unwrap_err();
        assert_eq!(err.code, code::FORBIDDEN, "got {}", err.message);
        assert!(err.message.contains("confirm"), "{}", err.message);
        // With the confirmation the same call goes through the policy and works.
        assert!(app
            .call_tool(
                "device_invoke",
                &json!({ "device": "bme280", "tool": "bme280_read", "arguments": { "confirm": true } }),
            )
            .is_ok());
    }

    #[test]
    fn device_invoke_rejects_out_of_range_arguments_before_touching_hardware() {
        let log = LogBuf::new(64);
        let board = crate::virtual_board::VirtualBoard::new();
        let calls = board.hw_calls.clone();
        let mut app = App::new(Box::new(board), Box::new(MemoryStore::new()), log, FwMeta::default());
        app.cfg.mcp.tools_mode = "all".into();
        app.call_tool(
            "manifest_apply",
            &json!({ "manifest": { "tools": [{
                "name": "climate_window",
                "params": [{ "name": "ms", "type": "integer", "min": 10, "max": 5000, "required": true }],
                "exec": { "type": "driver", "driver": "bme280", "action": "bme280_read" }
            }] } }),
        )
        .unwrap();
        let before = calls.load(std::sync::atomic::Ordering::SeqCst);
        let err = app
            .call_tool(
                "device_invoke",
                &json!({ "device": "bme280", "tool": "bme280_read", "arguments": { "ms": 90000 } }),
            )
            .unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS, "got {}", err.message);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "the board must not be touched when the arguments are rejected"
        );
    }

    #[test]
    fn a_manifest_tool_for_an_absent_device_is_not_listed() {
        let mut app = test_app();
        app.call_tool(
            "manifest_apply",
            &json!({ "manifest": { "tools": [{
                "name": "imu_read",
                "params": [],
                "exec": { "type": "driver", "driver": "mpu6050", "action": "mpu6050_read" }
            }] } }),
        )
        .unwrap();
        assert!(!app.is_visible("imu_read"), "no IMU on this board");
        assert!(!tool_names(&mut app).contains(&"imu_read".to_string()));
        // A tool for a device that *is* present stays listed.
        app.call_tool(
            "manifest_apply",
            &json!({ "mode": "merge", "manifest": { "tools": [{
                "name": "climate_read",
                "params": [],
                "exec": { "type": "driver", "driver": "bme280", "action": "bme280_read" }
            }] } }),
        )
        .unwrap();
        assert!(tool_names(&mut app).contains(&"climate_read".to_string()));
    }

    #[test]
    fn resources_expose_the_manifest_and_per_device_detail() {
        let mut app = test_app();
        let list = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#, None).unwrap();
        let uris: Vec<String> = list["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap().to_string())
            .collect();
        assert!(uris.contains(&"mcp://manifest".to_string()));
        assert!(uris.contains(&"mcp://devices/bme280".to_string()));

        let read = app
            .handle_rpc(
                r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"mcp://devices/bme280"}}"#,
                None,
            )
            .unwrap();
        let text = read["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(text.contains("BME280"));
    }

    #[test]
    fn i2c_identify_reports_candidates_for_a_known_address() {
        let mut app = test_app();
        let v = app.call_tool("i2c_identify", &json!({ "addr": 0x76 })).unwrap();
        assert!(v["candidates"].as_array().unwrap().iter().any(|c| c["driver"] == "bme280"));
        let v = app.call_tool("i2c_identify", &json!({ "addr": 0x3C })).unwrap();
        assert!(v["candidates"].as_array().unwrap().iter().any(|c| c["driver"] == "ssd1306"));
    }

    #[test]
    fn manifest_describe_carries_a_schema_primitives_and_examples() {
        // One section per call: the whole reference does not fit a tool result, so it is
        // split rather than truncated (the model used to receive 2 KB of an 8 KB answer).
        let mut app = test_app();
        let schema = app.call_tool("manifest_describe", &json!({ "section": "schema" })).unwrap();
        assert!(schema["manifest_schema"]["properties"]["tools"].is_object());
        let exec = app.call_tool("manifest_describe", &json!({ "section": "exec" })).unwrap();
        assert!(exec["primitives"].as_array().unwrap().len() >= 8);
        let examples = app.call_tool("manifest_describe", &json!({ "section": "examples" })).unwrap();
        assert!(examples["names"].as_array().unwrap().iter().any(|n| n == "relay_with_safety"));
        let drivers = app.call_tool("manifest_describe", &json!({ "section": "drivers" })).unwrap();
        assert!(drivers["drivers"].as_array().unwrap().iter().any(|d| d["kind"] == "bme280"));
        let one =
            app.call_tool("manifest_describe", &json!({ "section": "drivers", "driver": "bme280" })).unwrap();
        assert_eq!(one["driver"]["kind"], "bme280");
        // The overview is what a first call gets, and it points at the rest.
        let overview = app.call_tool("manifest_describe", &json!({})).unwrap();
        assert_eq!(overview["section"], "overview");
        assert!(overview["shape"]["tools"].is_array());
    }

    #[test]
    fn a_declared_manifest_device_is_instantiated() {
        let mut app = test_app();
        // The virtual board has the BME280 at 0x76; remove nothing, but declare an
        // OLED at 0x3D which is *not* on the bus: instantiation must fail softly.
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({ "manifest": { "devices": [
                    { "id": "board", "kind": "display", "driver": "ssd1306", "bus": 0, "address": 61, "enabled": true }
                ], "tools": [] } }),
            )
            .unwrap();
        assert_eq!(v["applied"], true);
        assert!(app.devices.infos().iter().any(|d| d.id == "ssd1306" || d.id == "ssd1306_2"));
    }

    #[test]
    fn forgetting_through_http_keeps_the_tail_and_reports_it() {
        let mut app = test_app();
        for i in 0..3 {
            app.agent.history.push(crate::agent::Message::user(&format!("q{i}"), "http", 10 * i));
            app.agent.history.push(crate::agent::Message::assistant("a", Vec::new(), 10 * i + 1));
        }
        let resp = app.route(&post("/api/agent/forget", r#"{"keep":1}"#));
        assert_eq!(resp.status, 200);
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["removed"], 4);
        assert_eq!(body["kept"], 2);
        assert_eq!(app.agent.history.len(), 2);
        assert_eq!(app.agent.history[0].text, "q2");

        // No `keep` means none: the whole conversation goes.
        let resp = app.route(&post("/api/agent/forget", "{}"));
        assert_eq!(resp.status, 200);
        assert!(app.agent.history.is_empty());
    }

    #[test]
    fn the_prompt_endpoint_shows_memory_and_the_compressed_view() {
        let mut app = test_app();
        use crate::tools::fs::Workspace as _;
        app.workspace
            .write("/MEMORY.md", "- [pins](/memory/pins.md) — GPIO18 is the strip\n")
            .expect("write");
        app.agent.archive.push("called: board_status({})".into());
        let resp = app.route(&get("/api/agent/prompt"));
        assert_eq!(resp.status, 200);
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        let system = body["system"].as_str().expect("system");
        assert!(system.contains("## Memory (/MEMORY.md)"), "{system}");
        assert!(system.contains("/memory/pins.md"), "{system}");
        assert!(system.contains("Earlier in this conversation"), "{system}");
        assert_eq!(body["archive_lines"], 1);
    }

    /// The portal's Files tab reads the workspace through two routes, and reports the budget
    /// the `write` tool enforces. A listing without the total would leave "the write was
    /// refused" with nothing to compare against.
    /// A family used to say only "`command: help` lists what each takes", so every turn began
    /// with a `help` call: one request, one round trip, before the call the model meant to make.
    /// The description now names the commands, generated from the same list `help` prints.
    #[test]
    fn every_family_names_its_commands_in_its_description() {
        let app = test_app();
        let mut families: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for label in app.family_members.values() {
            let Some((family, command)) = label.split_once(' ') else { continue };
            families.entry(family.to_string()).or_default().push(command.to_string());
        }
        assert!(families.len() >= 3, "the board and its families: {families:?}");

        let specs = app.tool_specs();
        let specs = specs.as_array().cloned().unwrap_or_default();
        let mut checked = 0;
        for (family, commands) in families {
            // Only what the model is actually offered: the declarative side is the one that
            // costs a round trip.
            let Some(spec) = specs.iter().find(|t| t["function"]["name"] == family.as_str()).cloned() else {
                continue;
            };
            checked += 1;
            let description = spec["function"]["description"].as_str().unwrap_or("");
            assert!(
                description.contains("Commands: "),
                "`{family}` does not name its commands: {description}"
            );
            for command in commands {
                assert!(
                    description.contains(&command),
                    "`{family}` hides `{command}` from its description: {description}"
                );
                // The arguments too: told only the name, the model called `led_set` with
                // `color: "green"` and was refused, which cost the second call this is for.
                assert!(
                    description.contains(&format!("{command}(")),
                    "`{family}` names `{command}` without its arguments: {description}"
                );
            }
            // And where an argument has a shape, the shape is in the line: a type is not a
            // format, and `color: string` is true of `"green"`, which the board refuses.
            if family == "indicators" {
                assert!(
                    description.contains("#rrggbb"),
                    "`indicators` does not say what a colour looks like: {description}"
                );
            }
            // A shape buried after a sentence is still a shape (`a melody, played back to back in
            // one buffer: [{hz, ms}, ...]`) - and clipping before looking for it threw it away.
            if family == "audio" {
                assert!(
                    description.contains("{hz"),
                    "`audio` does not say what a note looks like: {description}"
                );
            }
            assert!(
                !description.contains("lists what each takes"),
                "`{family}` still asks the model to spend a call on `help`: {description}"
            );
        }
        assert!(checked >= 3, "only {checked} families were visible to check");
    }

    #[test]
    fn the_files_routes_list_and_read_the_workspace() {
        use crate::tools::fs::Workspace as _;
        let mut app = test_app();
        app.workspace.write("/notes/one.md", "hello").expect("a file in a subdirectory");
        app.workspace.write("/top.md", "# title").expect("a root file");

        let resp = app.route(&get("/api/files?path=/"));
        assert_eq!(resp.status, 200, "{resp:?}");
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        // Directories first, then by name — whatever else the seeded workspace already holds.
        let dirs: Vec<&str> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["dir"] == true)
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(dirs.contains(&"notes"), "{body}");
        let positions: Vec<usize> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, e)| e["name"] == "notes" || e["name"] == "top.md")
            .map(|(i, e)| if e["name"] == "notes" { i } else { i + 100 })
            .collect();
        assert_eq!(positions.len(), 2, "{body}");
        assert!(positions[0] < positions[1], "a directory comes before a file: {body}");
        let top = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "top.md")
            .expect("top.md in the listing");
        assert_eq!(top["size_bytes"], 7, "{body}");
        assert_eq!(top["path"], "/top.md", "{body}");
        assert_eq!(body["total_max"], WORKSPACE_TOTAL_MAX, "{body}");
        assert!(body["total_bytes"].as_u64().unwrap_or(0) >= 12, "{body}");

        // A file, then a directory: the second is an error with a sentence in it.
        let resp = app.route(&get("/api/files/read?path=/top.md"));
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["text"], "# title", "{body}");
        let resp = app.route(&get("/api/files/read?path=/notes"));
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["ok"], false, "{body}");

        // `..` is refused by the same normaliser the tools use, not by a second rule here.
        let resp = app.route(&get("/api/files?path=/../etc"));
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["ok"], false, "{body}");
    }

    /// The tree: every file in one answer, and the directories derived from the paths. One
    /// request for the whole workspace, because the whole workspace is 8 KB.
    #[test]
    fn the_files_tree_lists_directories_and_files() {
        use crate::tools::fs::Workspace as _;
        let mut app = test_app();
        app.workspace.write("/notes/deep/one.md", "hello").expect("a nested file");
        app.workspace.write("/top.md", "# title").expect("a root file");

        let resp = app.route(&get("/api/files/tree"));
        assert_eq!(resp.status, 200, "{resp:?}");
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        let entries = body["entries"].as_array().expect("entries");
        let find = |path: &str| {
            entries
                .iter()
                .find(|e| e["path"] == path)
                .unwrap_or_else(|| panic!("{path} is missing from {body}"))
                .clone()
        };

        // A directory that holds only a directory is still a directory.
        for dir in ["/notes", "/notes/deep"] {
            assert_eq!(find(dir)["dir"], true, "{dir}: {body}");
            assert_eq!(find(dir)["size_bytes"], Value::Null, "a directory has no size");
        }
        let file = find("/notes/deep/one.md");
        assert_eq!(file["dir"], false, "{body}");
        assert_eq!(file["name"], "one.md", "the name is the last segment");
        assert_eq!(file["size_bytes"], 5, "{body}");
        assert_eq!(find("/top.md")["size_bytes"], 7, "{body}");

        // Nothing invented: a file's own path is not also listed as a directory.
        assert!(
            !entries.iter().any(|e| e["path"] == "/top.md" && e["dir"] == true),
            "a file became a directory: {body}"
        );
        assert_eq!(body["total_max"], WORKSPACE_TOTAL_MAX, "{body}");
    }
}

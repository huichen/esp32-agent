//! The application: config + manifest + devices + tools + sessions, the MCP
//! dispatcher, and the HTTP routes for the MCP endpoint and the HTML portal.
//!
//! Mutable tool/device state remains serialized behind App because peripherals
//! share buses. Public status and authenticated turn cancellation use independent
//! short-lived locks, so a slow tool does not block those control-plane requests.

use crate::agent::{Agent, AgentStep, ToolHost};
use crate::boards::{self, BoardProfile};
use crate::capability::CapSet;
use crate::config::Config;
use crate::device::Devices;
use crate::drivers;
use crate::error::{code, McpError, Result};
use crate::hardware::{BoardInfo, Hardware, WifiStatus};
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
/// The depth-1 queue between boots: JSON `{text, source, at_ms}`, without attachments.
/// Kept beside the transcript on the filesystem, since one question can exceed NVS's
/// single-value limit. Removed only after the question is consumed (or explicitly forgotten).
pub const NVS_KEY_PENDING_MESSAGE: &str = crate::keys::PENDING_MESSAGE;
/// The source a resumed turn carries, so a reply can be told apart from one the user just
/// asked for.
pub const RESUME_SOURCE: &str = "resume";

/// The file tools write into a RAM map with a filesystem value per published file and a
/// separate index. Per-file admission shares the file tools' limit; the aggregate cap is
/// a text-payload budget, not a measurement of the RAM needed by the map and request copies.
pub const WORKSPACE_FILE_MAX: usize = crate::tools::fs::MAX_FILE_BYTES;
pub const WORKSPACE_TOTAL_MAX: usize = 4 * 1024 * 1024;
/// Legacy `ws1` workspace chunk plan, retained for reading/migration.
///
/// This was five values of 3500 bytes (17,500 encoded JSON bytes), with checksums in
/// the `ws` header and payloads in `ws.1`, `ws.2`, ... . It replaced a single NVS
/// string that exceeded ESP-IDF's value limit. It is not the current workspace budget:
/// new saves publish an index and one filesystem value per path, and cleanup of these
/// historical chunks follows a confirmed publication.
pub const WORKSPACE_PLAN: store::Plan =
    store::Plan { tag: "ws1", chunk_bytes: store::CHUNK_BYTES, chunks_max: 5 };

/// The longest `sleep` / `usleep` waits when no tool budget is configured.
///
/// The real bound is `agent.tool_timeout_s` (30 s by default): a wait asks how much of the
/// call is left instead of using this number, so one setting governs every command that
/// waits. This cap comes back only when that deadline is switched off with `0`, because
/// nothing else would bound a bare `sleep 1e9`. Tool calls hold the shared App lock, so a
/// sleeping command delays other callers needing that lock; the bound is an execution
/// policy, independent of heap capacity.
pub const MAX_SHELL_SLEEP_MS: u64 = 10_000;

/// How much of a pipeline's last stage reaches the answer.
///
/// A pipeline can multiply intermediate output. Its 8 KiB stdout policy is deliberately
/// separate from the agent's 64 KiB tool-text ceiling and the workspace's file budgets.
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

/// One script run: what the caller asked for and what came back.
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

/// Hardware data sampled by the platform maintenance task. Public cached reads
/// never acquire the WiFi/status mutex or query the FreeRTOS task list. Sampling
/// still requires App ownership; this is not evidence of an App/WiFi lock cycle.
#[derive(Debug, Clone, Default)]
pub struct HwView {
    pub info: BoardInfo,
    pub wifi: WifiStatus,
    pub internal: Option<(u32, u32)>,
    pub threads: Vec<Value>,
}

/// The writer's last sampled store-space statistics. StoreQueue::space reads
/// cached queue statistics, not the backend. Backend I/O finishes and releases its
/// locks before the writer acknowledges under App; there is no demonstrated reverse
/// lock edge. Keep the sample to give status an explicit publication boundary.
#[derive(Debug, Clone, Default)]
pub struct StoreSpaceSample {
    pub space: Option<store::StoreSpace>,
}

impl StoreSpaceSample {
    /// Read the queue's already-cached statistics, without new backend I/O.
    pub fn sample(queue: &store::StoreQueue) -> Self {
        Self { space: queue.space() }
    }
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
    /// The running turn's image inputs. A newer queued message may replace the
    /// download list while the current model call is in flight.
    active_attachments: Option<Vec<crate::agent::Attachment>>,
    /// The board's own certificate, when it serves https on a second port. The
    /// fingerprint is what a person compares against the browser's warning page —
    /// it is the only proof that this board, and not something on the path, answered.
    pub tls: TlsInfo,
    /// The on-board agent: transcript, the one-slot queue and the last error.
    pub agent: Agent,
    /// True while `with_agent` has the `Agent` moved out of this struct.
    ///
    /// A tool handler runs *inside* that window, so anything that samples the agent for a
    /// caller (`board status`) must know whether it is looking at the real agent or at the
    /// empty placeholder that stands in for it while a turn runs.
    agent_out: bool,
    status_views: crate::status_view::StatusViews,
    run_gate: crate::run_gate::RunGate,
    /// Where the model lives and how far one run may go. `api_key` is kept in
    /// NVS but never echoed back by the portal API.
    pub llm: LlmConfig,
    /// Scheduled prompts and sensor rules. The scheduler tick (platform layer)
    /// decides when they fire; this file only stores and edits them.
    pub tasks: Vec<ScheduledTask>,
    pub rules: Vec<SensorRule>,
    /// The workspace behind the `fs_*` tools.
    pub workspace: AppWorkspace,
    /// Named media buffers in PSRAM: audio and frames too large for a tool result.
    ///
    /// Deliberately not persisted and not in the transcript — a clip is a place to put
    /// bytes between two tool calls, and `clips` reports where they actually landed.
    pub clips: crate::clips::Clips,
    /// `setenv`/`env`/`unsetenv`: variables that live in this `App` and nowhere else.
    ///
    /// Deliberately *not* in [`Config`]: a shell variable is not a setting, it is state one
    /// session made up, and writing it to NVS would turn a typo into a boot-time fact.
    /// Nothing here is persisted, and `env` says so.
    pub shell_env: std::collections::BTreeMap<String, String>,
    /// The model-facing tool array. 51 schemas is the largest thing a step used to build,
    /// and it changes only when the manifest, the profile, the disabled set or the probe
    /// does. `RefCell` because `ToolHost::tool_specs` takes `&self`; the whole `App` is
    /// already behind a mutex, so there is no second borrow to lose.
    pub tool_specs_cache: std::cell::RefCell<Option<(u64, Value)>>,
    /// Monotonic counter for script run ids; not persisted.
    pub script_seq: u64,
    /// True while the interpreter is running on this thread.
    ///
    /// The recursion guard: `tool("script", …)` is refused by the script host, and this
    /// covers the second way in — `tool("shell", {command: "sh file"})`. One flag, checked
    /// at both entrances, instead of trusting every path to remember.
    pub in_script: bool,
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
    /// Only the latest admitted workspace generation. Its receipt, not queue admission,
    /// decides when hashes can suppress a later write.
    workspace_pending: Option<WorkspaceSave>,
    /// Monotonic admissions let nested tool routers describe their own file effects without
    /// waiting for a flash writer while the caller holds the App mutex.
    workspace_admissions: u64,
    /// Values potentially left by a refused publication/cleanup, retained for a later
    /// successful index publication. Bounded with the queue's key budget before admission.
    workspace_stale_keys: std::collections::BTreeSet<String>,
    /// The hardware's view as of the last tick. See [`HwView`].
    hw_view: HwView,
    /// The store's free/used bytes as of the last drain. See [`StoreSpaceSample`].
    store_space: StoreSpaceSample,
    /// When the tool call now running must stop, from `agent.tool_timeout_s`.
    ///
    /// Nested calls inherit the outer deadline instead of starting a new one: a script that
    /// calls ten tools is one call as far as the portal is concerned, and ten fresh budgets
    /// would be ten ways to hold the board.
    tool_deadline: Option<std::time::Instant>,
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

struct WorkspaceSave {
    receipt: store::PublicationReceipt,
    hashes: std::collections::BTreeMap<String, u64>,
    changed: Vec<String>,
    cleanup: Vec<String>,
}

impl App {
    pub fn new(
        mut hw: Box<dyn Hardware>,
        store: Box<dyn KvStore>,
        log: Arc<LogBuf>,
        firmware: FwMeta,
    ) -> Self {
        // Uptime is `hw.millis()` minus this reading, so it is monotonic and independent of
        // whether SNTP ever lands. It used to be the wall clock, which read 0 until the first
        // sync and then counted from the sync instead of from the boot.
        let boot_monotonic_ms = hw.millis();
        // Every write below this line is a request: `store` is the queue, and the platform's
        // writer task is what turns a request into an `spi_flash` call. Reads pass straight
        // through (after checking the queue), which is why boot is unchanged.
        let queue = store::StoreQueue::new(store);
        let queue_sample = queue.clone();
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
        let mut runtime = Runtime::new(boot_monotonic_ms, &firmware.version, &firmware.build);
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
            active_attachments: None,
            tls: TlsInfo::default(),
            agent: Agent::new(),
            agent_out: false,
            status_views: Default::default(),
            run_gate: Default::default(),
            llm: LlmConfig::default(),
            tasks: Vec::new(),
            rules: Vec::new(),
            tool_specs_cache: std::cell::RefCell::new(None),
            script_seq: 0,
            in_script: false,
            workspace: AppWorkspace::default(),
            shell_env: std::collections::BTreeMap::new(),
            tool_transport: None,
            model_transport: None,
            streams: EventStreamGate::new(),
            shared: Weak::new(),
            session_counter: 0,
            agent_thread: AgentThread::NotStarted,
            clips: crate::clips::Clips::default(),
            store_queue: queue,
            store_writer: StoreWriter::default(),
            workspace_pending: None,
            workspace_admissions: 0,
            workspace_stale_keys: Default::default(),
            hw_view: HwView::default(),
            store_space: StoreSpaceSample::sample(&queue_sample),
            tool_deadline: None,
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
        app.refresh_hw_view();
        // The readings `board status` and `board tls` report about things a tool handler cannot
        // reach — the store's counters, the agent's own numbers, the certificate — filled once
        // at boot so the first call answers rather than returning null.
        app.sample_store();
        app.publish_agent_stats();
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
    /// Platforms use [`App::flush_shared_store`] to drain outside the app mutex and then
    /// acknowledge completed workspace generations under it. A reboot path may deliberately
    /// use [`App::flush_store`] while holding that mutex for its final batch.
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
    pub fn flush_store(&mut self, who: &str) -> store::FlushReport {
        let report = self.store_queue.flush();
        // Tests and the host simulator only: no flash window, no competing writer.
        let sample = StoreSpaceSample::sample(&self.store_queue);
        self.acknowledge_store_flush(who, &report, sample);
        report
    }

    /// Drain outside the app mutex, then settle receipts and log under a short app lock.
    /// Queue/backend access can still block during I/O; this is not a lock-free store.
    /// The same internal-stack-only precondition as `flush_store` applies, and the platform
    /// must check the calling task before using this helper.
    pub fn flush_shared_store(shared: &Shared, who: &str) -> store::FlushReport {
        let queue = app_lock(shared).store_queue();
        let report = queue.flush();
        // Sampled before the App lock is re-taken: one order, store then App.
        let sample = StoreSpaceSample::sample(&queue);
        app_lock(shared).acknowledge_store_flush(who, &report, sample);
        report
    }

    /// Complete app bookkeeping after the queue was drained, including generation-specific
    /// workspace receipts. This method does no backend writes; platforms may drain outside
    /// the app lock and then call it under a short mutable app lock.
    pub fn acknowledge_store_flush(
        &mut self,
        who: &str,
        report: &store::FlushReport,
        sample: StoreSpaceSample,
    ) {
        self.store_space = sample;
        self.acknowledge_workspace();
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
        // One capability, one name. A command a family owns is not registered on its own:
        // the family carries its own copy of every handler, so `display {command: "oled_text"}`
        // runs the same code the standalone name used to. The second spelling would be a second
        // thing to document, to disable and to keep in step, which is what the family replaced.
        //
        // `family_members` still maps every command name to the family that owns it, because
        // `disabled_tools` names a command: disabling `oled_text` keeps disabling the OLED
        // through `display` as well.
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

        // Device tools that belong together answer to one family name: six `oled_*` become
        // `display`, four audio tools become `audio`, and the LED and the buttons become
        // `indicators`. The grouping is the board's own vocabulary, a kind of device it
        // detected, so a driver that is not in this list keeps its individual tools instead of
        // being guessed at by name.
        let mut groups: std::collections::BTreeMap<&'static str, Vec<String>> = Default::default();
        for info in self.devices.infos() {
            let Some((_, family)) = Self::DEVICE_FAMILIES.iter().find(|(kind, _)| *kind == info.kind) else {
                continue;
            };
            groups.entry(family).or_default().extend(info.tools.iter().cloned());
        }
        let mut families: Vec<(&'static str, Vec<ToolDef>)> = Vec::new();
        for (family, declared) in groups {
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
            for name in subs.iter().map(|d| d.spec.name.clone()) {
                members.insert(name.clone(), format!("{family} {name}"));
            }
            families.push((family, subs));
        }

        let owned: std::collections::BTreeSet<String> = members.keys().cloned().collect();
        for def in board_tools::always_tools().into_iter().chain(board_tools::manifest_tools()) {
            if !owned.contains(&def.spec.name) {
                tools.register(def);
            }
        }
        tools.register(board_tools::board_family());
        tools.register(board_tools::bus_family());
        tools.register_all(board_tools::bitmap::tools());
        for def in self.devices.tool_defs().into_iter().chain(board_tools::media::tools(&self.devices)) {
            if !owned.contains(&def.spec.name) {
                tools.register(def);
            }
        }
        for (family, subs) in families {
            let summary = board_tools::family_summary(family)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{family}: {} commands", subs.len()));
            tools.register(Family::new(family, summary, subs).into_tool().device_tool());
        }
        self.family_members = members;
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
    /// Empty, and it stays: `reboot` used to sit here, and the answer to "why can't a model
    /// restart the board" turned out to be that nobody can — the route went with it. The
    /// list stays so the next tool whose answer is "not for a caller" has somewhere to go
    /// that is not a rule buried inside one handler.
    const NEVER_OFFERED: &'static [&'static str] = &[];

    pub const ALIASES_ACCEPTED_BUT_NOT_ADVERTISED: &'static [&'static str] = &[
        // what `read` and `shell` absorbed
        "fs_read", "fs_write", "fs_edit", "fs_ls", "fs_grep", "fs_rm", "fs_cp", "fs_mv", "net_get",
        "net_post", "net_dns", "net_tcp",
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
        // `board_config_get`, `delay` and `logs_tail` used to sit here. They belong to a family now, and a profile hides whole tools: a command is
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
        if self.ensure_enabled(name).is_err() {
            return Some("switched off in `mcp.disabled_tools`".to_string());
        }
        // A command a family owns is not a tool, so it never reaches this point: the registry
        // lookup above returns `None` first. The map is kept for `disabled_tools`, where the
        // command name is still what a person writes.
        if Self::NEVER_OFFERED.contains(&name) {
            return Some("a person's action, not a caller's".to_string());
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
        if self.ensure_enabled(name).is_err() {
            return Some("switched off in `mcp.disabled_tools`".to_string());
        }
        match name {
            "image_read" if !self.llm.vision => {
                Some("vision is disabled for the configured model".to_string())
            }
            "image_read"
                if !self.caps.contains("camera")
                    && !self.tool_attachments().iter().any(|a| a.mime.starts_with("image/")) =>
            {
                Some("no camera or image attachment on this board".to_string())
            }
            // Buffer-only operations need no peripheral. Record/play are offered
            // only when their detected microphone or playback target is present.
            "clip_record" if !self.caps.contains("mic") => {
                Some("no detected I2S microphone to record a clip".to_string())
            }
            "clip_play" if !self.caps.contains("speaker") && !self.caps.contains("oled") => {
                Some("no speaker or display to play a clip on".to_string())
            }
            _ => None,
        }
    }

    /// Every out-of-registry tool, offered or not, in the OpenAI shape the model gets.
    ///
    /// One construction feeds three readers — the offered list, the hidden list and the
    /// portal page — so none of them can invent a tool the others do not have. The groups
    /// are `shell`, `script`, the six clip tools that own PSRAM buffers, and the four
    /// file/image tools that need the app's own state.
    fn extra_entries() -> Vec<Value> {
        let mut out = Vec::new();
        for group in [
            // One `shell` instead of the twelve fs_*/net_* tools: same handlers, one
            // schema, one name to remember.
            json!([crate::tools::shell::openai_spec()]),
            board_tools::script_specs(),
            // Clips hold the media a tool result cannot carry. They are ordinary extra
            // tools, so the model, `/mcp tools/list` and the portal see the same six.
            board_tools::clip::specs(),
            board_tools::agent_extra_specs(),
            board_tools::agent_specs(),
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
                !self.cfg.auth.portal_pin.is_empty(),
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

    fn sync_run_gate(&mut self) {
        self.run_gate.set_sessions(self.runtime.sessions.len());
        self.run_gate.sync(
            &self.cfg.auth.portal_pin,
            &self.runtime.auth_tokens,
            &self.cfg.mcp.path,
            self.cfg.mcp.cors,
        );
        self.runtime.counters.auth_failures =
            self.runtime.counters.auth_failures.saturating_add(self.run_gate.take_auth_failures());
        self.runtime.counters.http_requests =
            self.runtime.counters.http_requests.saturating_add(self.run_gate.take_http_requests());
    }

    /// Only the platform maintenance task (or server construction after WiFi is
    /// up) constructs a full view. Never call on the storage writer or inside a
    /// tool: its stack and critical path must remain bounded independently.
    #[inline(never)]
    pub fn publish_status_view(&mut self) {
        self.refresh_hw_view();
        self.sync_run_gate();
        let status = self.status_json();
        let sampled_ms = self.hw.millis();
        self.status_views.publish(crate::status_view::StatusView {
            status,
            boot_ms: self.runtime.boot_ms,
            sampled_ms,
            sampled_at: Instant::now(),
        });
    }

    pub fn status_json(&mut self) -> Value {
        // Plain data from the last tick: no radio lock, no FreeRTOS task list.
        let info = self.hw_view.info.clone();
        let wifi = self.hw_view.wifi.clone();
        let internal = self.hw_view.internal;
        let threads = self.hw_view.threads.clone();
        // The clips and the pool they reserve: the other half of "where did the PSRAM go".
        let clips = self.clips.json(self.frame_bytes());
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
        let store_space = match &self.store_space.space {
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
                // The split, not just the total: a change that claims to free internal RAM
                // can be checked against the pool it moved. Empty on a platform that cannot
                // split its heap by capability.
                "pools": self.hw.heap_pools(),
            },
            // The other half of the memory picture: which pool, and how much stack each
            // task has left at its deepest point. Without this, deciding whether a stack
            // constant can come down is guesswork, and a stack that is too small shows up
            // as an assert and a reboot rather than as a number.
            "threads": threads,
            "clips": clips,
            "uptime_s": self.runtime.uptime_s(self.hw.millis()),
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
                "run_active": self.run_gate.active(),
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
        crate::hardware::threads_json(self.hw.tasks())
    }

    /// Sample the hardware's view into plain data. Called at construction and once per tick by
    /// the platform, which holds the App lock but no radio or driver lock.
    pub fn refresh_hw_view(&mut self) {
        let info = self.hw.board_info();
        let wifi = self.hw.wifi_status();
        let internal = self.hw.heap_internal();
        let threads = self.threads_json();
        self.hw_view = HwView { info, wifi, internal, threads };
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
             This board is self-configuring: `board` and `bus` take a `command` argument, and \
             `inventory`, `i2c_identify`, `describe` and `apply` are commands of `board`, `bus` \
             and `manifest`. A manifest change adds hardware and tools. \
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
    /// Submit a script for execution, returning the finished job.
    ///
    /// One path, and it waits: the callers are a tool call and a scheduled task, both of
    /// which already run on the board's one work thread. There used to be a queued variant
    /// for `POST /api/script {async:true}`; when that route went, the job pool, the poll
    /// route and the state machine it needed went with it.
    pub fn submit_script(&mut self, source: &str) -> ScriptJob {
        match self.submit_script_call(&json!({ "source": source })) {
            Ok(job) => job,
            Err(e) => ScriptJob { state: "failed".into(), error: e, ..Default::default() },
        }
    }

    /// The same thing for a full tool-arguments object, so `sh <file>` and `script {path}`
    /// share one path into the interpreter instead of two that can drift.
    pub fn submit_script_call(&mut self, args: &Value) -> std::result::Result<ScriptJob, String> {
        let id = format!("s{}", self.script_seq + 1);
        self.script_seq += 1;
        let started = now_ms();
        let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("");
        let mut job = ScriptJob { id, source: source.to_string(), ..Default::default() };
        match self.run_script(args) {
            Ok(value) => {
                job.state = "done".to_string();
                job.result = value.get("result").cloned().unwrap_or(Value::Null);
                job.output = value
                    .get("output")
                    .and_then(|o| o.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                job.steps = value.get("steps").and_then(|s| s.as_u64()).unwrap_or(0);
                job.ms = now_ms().saturating_sub(started);
                Ok(job)
            }
            Err(e) => {
                job.state = "failed".to_string();
                job.ms = now_ms().saturating_sub(started);
                job.error = e.message;
                Ok(job)
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
            let read =
                self.fs_or_net_call("fs_read", &json!({ "path": path })).map_err(McpError::invalid_params)?;
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
            fn cancelled(&self) -> bool {
                self.app.run_gate.cancelled()
            }
            fn call_tool(&mut self, name: &str, arguments: &Value) -> std::result::Result<Value, String> {
                if name == "script" {
                    return Err("a script may not call `script`".into());
                }
                // `App::call_tool` already returns the tool's JSON value.
                self.app
                    .call_tool(name, arguments)
                    .map_err(|e| crate::script::tool_error_with_arguments(self.app, name, e))
            }
        }
        // Arguments for the script: `{"source": …, "args": {"pin": 12}}`. They land in the
        // global scope as `args` (a map) and `argv` (its values, in order), which is what
        // makes one script usable for several pins.
        let args = script_arguments(args.get("args"));
        // The script runs inside this tool call, so it gets what is left of that call's
        // budget rather than a fresh one of its own.
        let budget = self.remaining_tool_ms();
        // Guard the second entrance into the interpreter (`shell sh`); the first one,
        // `tool("script")`, is refused in the host below.
        self.in_script = true;
        let mut host = Host { app: self };
        let started = now_ms();
        let outcome = crate::script::run_with_budget(&source, &mut host, &args, budget);
        host.app.in_script = false;
        match outcome {
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
                    "budget_ms": budget,
                }))
            }
            Err(e) => Err(McpError::invalid_params(format!("script failed: {e}"))),
        }
    }

    fn tool_attachments(&self) -> &[crate::agent::Attachment] {
        self.active_attachments.as_deref().unwrap_or(&self.attachments)
    }

    /// Ask the configured model about an attached image.
    ///
    /// This is a **second** model call, made inside a tool, with the configured
    /// Chat, Responses or Anthropic adapter. It costs a round trip and tokens;
    /// an uploaded image needs neither a camera driver nor an on-board OCR engine.
    fn image_read(&mut self, args: &Value) -> Result<Value> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("Transcribe every piece of text in this image exactly, then describe what it shows.");
        let wanted = args.get("name").and_then(|v| v.as_str()).unwrap_or("");

        if prompt.len() > AGENT_TEXT_MAX {
            return Err(McpError::invalid_params(format!("image prompt exceeds {AGENT_TEXT_MAX} bytes")));
        }
        if !self.llm.vision {
            return Err(McpError::unsupported("the configured model has vision disabled"));
        }
        let images = self.tool_attachments();
        let attachment = match images
            .iter()
            .find(|a| a.mime.starts_with("image/") && (wanted.is_empty() || a.name == wanted))
        {
            Some(a) => a.clone(),
            None => {
                return Err(McpError::invalid_params(
                    if !images.iter().any(|a| a.mime.starts_with("image/")) {
                        "no image attached to the last message; attach one in the portal first".to_string()
                    } else {
                        format!(
                            "no attachment named `{wanted}`; the last message carried: {}",
                            images
                                .iter()
                                .filter(|a| a.mime.starts_with("image/"))
                                .map(|a| a.name.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                ))
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

        let cfg = self.llm.clone();
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

    /// An explicit disable applies to aliases, family members and underlying file
    /// operations too; a listing profile only changes how a tool is advertised.
    fn ensure_enabled(&self, name: &str) -> Result<()> {
        crate::tool::ensure_tool_enabled_in(&self.cfg, &self.family_members, name)?;
        let dependencies: &[&str] = match name {
            "edit" | "fs_edit" | "fs_cp" => &["fs_read", "fs_write"],
            "fs_mv" => &["fs_read", "fs_write", "fs_rm"],
            "fs_grep" => &["fs_read"],
            _ => &[],
        };
        for dependency in dependencies {
            crate::tool::ensure_tool_enabled(&self.cfg, dependency)?;
        }
        Ok(())
    }

    fn workspace_admission_reply(mut value: Value) -> Value {
        if !value.is_object() {
            value = json!({ "result": value });
        }
        value["saved"] = json!(false);
        value["queued"] = json!(true);
        value["persistence_note"] = json!("applied in RAM; queue admission is not storage acknowledgement. This call did not wait for publication: `board status` reports the store's `pending`, `failed` and `last_failure`");
        value
    }

    pub fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        let _activity = self.run_gate.enter_tool(name, None);
        // A top-level call installs the configured deadline; a nested one (a script's
        // `tool(…)`, a family command, a shell stage) keeps the outer one. Ten tools inside
        // one script are one call as far as the portal is concerned.
        let outer = self.tool_deadline;
        let nested = outer.is_some();
        if outer.is_none() {
            self.tool_deadline = self.configured_tool_deadline();
        }
        let budget = self.remaining_tool_ms();
        let started = std::time::Instant::now();
        let before = self.workspace_admissions;
        // A call that starts after the budget is gone never reaches a driver: its caller has
        // already been written off as timed out, and starting hardware work now would be
        // doing it for nobody.
        let result = match (self.run_gate.cancelled(), self.remaining_tool_ms(), budget) {
            (true, _, _) => Err(McpError::timeout(format!("`{name}` did not run: the turn was interrupted"))
                .with_data(json!({
                    "tool": name,
                    "timeout_ms": budget.unwrap_or(0),
                    "interrupted": true,
                    "suggestion": "the turn was stopped from the console; ask again to continue",
                }))),
            (_, Some(0), Some(budget)) => {
                // Quote the budget the call was given, not the zero that is left of it: the
                // number a reader needs is how much time the whole call had.
                let seconds = u64::from(self.cfg.agent.tool_timeout_s).max(1);
                let message = if nested {
                    format!("`{name}` did not run: the tool call it belongs to has used its whole {seconds} s budget, so nothing is left for another step")
                } else {
                    format!("`{name}` did not run: its {seconds} s tool budget was already gone")
                };
                Err(McpError::timeout(message).with_data(json!({
                    "tool": name,
                    "timeout_ms": budget,
                    "nested": nested,
                    "suggestion": "do less in one call (a 180-frame animation is 700+ draws) or raise agent.tool_timeout_s, up to 300 s. Long media needs a background job, which the board does not have yet: see ROADMAP",
                })))
            }
            _ => self.call_tool_inner(name, args),
        };
        let elapsed = started.elapsed().as_millis() as u64;
        let result = self.note_tool_overrun(name, result, budget, elapsed);
        // Counted here, not inside the dispatch: a call that takes a fast path (a file, the
        // shell, a script, a clip) returns before `call_tool_inner`'s tail, so every one of
        // those eleven tools was invisible in `counters` and `per_tool` — a board that had
        // served 100 calls reported 70, and the missing ones were exactly the tools a caller
        // watches. An overrun counts as a failure, because that is what the caller got.
        self.runtime.record_tool_call(name, result.is_ok());
        self.settle();
        // The outer deadline comes back whatever the result was, or a refused call would
        // leave the next one with a budget that has already been spent.
        self.tool_deadline = outer;
        self.sync_run_gate();
        if self.workspace_admissions == before {
            return result;
        }
        match result {
            Ok(value) => Ok(Self::workspace_admission_reply(value)),
            Err(mut error) => {
                // A later command failing does not roll back an earlier admitted write.
                // Preserve the original error/data and make that partial effect observable.
                let data = error.data.get_or_insert_with(|| json!({}));
                if !data.is_object() {
                    *data = json!({ "original_data": data.take() });
                }
                data["workspace_admission"] = Self::workspace_admission_reply(json!({}));
                Err(error)
            }
        }
    }

    /// The deadline a top-level tool call installs, from `agent.tool_timeout_s`.
    fn configured_tool_deadline(&self) -> Option<std::time::Instant> {
        let seconds = self.cfg.agent.tool_timeout_s;
        (seconds > 0).then(|| std::time::Instant::now() + std::time::Duration::from_secs(u64::from(seconds)))
    }

    /// Milliseconds left of the tool call now running; `None` when the deadline is off.
    ///
    /// This is the number every waiting command asks for: `shell sleep`, `script sleep`
    /// and the HTTP client use what is left instead of a fixed cap of their own, which is
    /// what makes one configured timeout enough.
    pub fn remaining_tool_ms(&self) -> Option<u64> {
        // A cancelled turn gives its remaining tools no time at all: a script drawing 700
        // frames must stop at the next call, not finish the frames nobody is waiting for.
        if self.run_gate.cancelled() && self.tool_deadline.is_some() {
            return Some(0);
        }
        let deadline = self.tool_deadline?;
        let now = std::time::Instant::now();
        Some(if deadline <= now { 0 } else { (deadline - now).as_millis().min(u128::from(u64::MAX)) as u64 })
    }

    /// Whether the call now running has no time (or no permission) left to continue.
    ///
    /// A playback loop asks this between chunks: the alternative is finishing a two-minute
    /// clip for a caller that has already given up.
    pub(crate) fn out_of_time(&self) -> bool {
        self.run_gate.cancelled() || self.remaining_tool_ms() == Some(0)
    }

    /// A refusal for a route that would delete the question a running turn is answering.
    ///
    /// The turn happens with the app lock released while the model thinks, so a clear that
    /// arrives in that window is not blocked by anything: without this check it would empty
    /// the transcript, and the reply would then be appended to a thread that no longer holds
    /// its question. Waiting is the honest answer, and 409 says "try again".
    fn refuse_while_running(&self, action: &str) -> Option<Response> {
        self.run_gate.active().then(|| {
            Response::error_json(
                409,
                &format!(
                    "a model turn is running, so you cannot {action}: its question would be gone 
                     and its answer would arrive in a thread that no longer has it. Wait for the 
                     turn to finish (GET /api/agent/history stops growing) and try again"
                ),
            )
        })
    }

    /// Ask the running turn to stop at its next boundary. Returns whether there was one.
    pub fn request_cancel(&mut self) -> bool {
        if !self.run_gate.request() {
            return false;
        }

        self.log.info("agent", "cancel requested; the turn stops at its next boundary");
        true
    }

    /// Take the request, so one cancel stops one turn and cannot poison the next.
    pub fn take_cancel(&mut self) -> bool {
        self.run_gate.take_cancel()
    }

    /// Say so when a call finished after its budget, without pretending it did nothing.
    ///
    /// The work already happened (a write landed, a tone played), so this is not an error a
    /// caller can retry blindly: it is a result that carries the overrun, and the log gets a
    /// line a person can find.
    fn note_tool_overrun(
        &mut self,
        name: &str,
        result: Result<Value>,
        budget: Option<u64>,
        elapsed_ms: u64,
    ) -> Result<Value> {
        let (Ok(value), Some(budget)) = (&result, budget) else { return result };
        if elapsed_ms <= budget {
            return result;
        }
        self.log.warn("tool", format!("{name} finished after {elapsed_ms} ms, past its {budget} ms budget"));
        let note = json!({
            "timeout_ms": budget,
            "elapsed_ms": elapsed_ms,
            "note": "the call ran past `agent.tool_timeout_s`; its effects happened, and a retry is a second effect",
        });
        let mut value = value.clone();
        match value.as_object_mut() {
            Some(object) => {
                object.insert("deadline_exceeded".into(), note);
            }
            None => {
                value = json!({ "result": value, "deadline_exceeded": note });
            }
        }
        Ok(value)
    }

    fn call_tool_inner(&mut self, name: &str, args: &Value) -> Result<Value> {
        self.ensure_enabled(name)?;
        if !args.is_object() {
            return Err(McpError::invalid_params("tool arguments must be an object"));
        }
        if name.starts_with("fs_") || name.starts_with("net_") {
            let text = self.fs_or_net_call(name, args).map_err(McpError::invalid_params)?;
            return Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "text": text })));
        }
        // Generic device dispatch resolves the instance's public name before checking
        // its switch, so a stored raw alias cannot bypass a suffixed tool's disable.
        // The script tool is the one tool that needs the dispatcher itself: `tool()` inside
        // a script calls any other tool, so it cannot live in the registry where handlers
        // only see a `ToolCtx`.
        if name == "script" {
            return self.run_script(args);
        }
        // The board's own agent, for a caller that is not the model: the four tools the portal's
        // Agent panel reaches over REST. Reached here because they need `App` — the agent, the
        // model config and the store queue — and never a driver.
        match name {
            "agent_say" => return self.agent_say_tool(args),
            "agent_history" => return self.agent_history_tool(args),
            "agent_forget" => return self.agent_forget_tool(args),
            "agent_config_get" => {
                if !args.is_object() {
                    return Err(McpError::invalid_params("tool arguments must be an object"));
                }
                return Ok(self.agent_config_json());
            }
            "agent_config_set" => return self.agent_config_set_tool(args),
            _ => {}
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
        // Clips are state of this `App` (the buffers), not of a `ToolCtx`: they are reached
        // here for the same reason the file tools are.
        if name.starts_with("clip_") {
            return self.clip_tool(name, args);
        }
        // Family aliases and generic/minimal folding change the listing, not whether
        // a previously learned tool can execute. Only explicit disables and missing
        // hardware are execution refusals (the former was checked before every fast path).
        if self.tools.get(name).is_none() {
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
        if let Some(def) = self.tools.get(name) {
            if !def.spec.core && self.cfg.mcp.tools_mode != "all" {
                let missing = self.caps.missing(&def.spec.requires);
                if !missing.is_empty() {
                    return Err(McpError::unsupported(format!(
                        "tool `{name}` needs hardware that was not detected: {}",
                        missing.join(", ")
                    ))
                    .with_data(json!({ "missing_capabilities": missing })));
                }
            }
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
                family_members,
                run_gate,
                tls,
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
                families: family_members,
                gate: run_gate,
                tls,
            };
            (def.handler)(&mut ctx, args)
        };
        out
    }

    /// Take the store's own counters, without the backend lock.
    ///
    /// `pending`, `written`, `failed` and `last_failure` are the queue's own state; `space`
    /// is left out on purpose: the writer samples it independently before App
    /// acknowledgement ([`StoreSpaceSample`]), after releasing the backend lock.
    /// Queue-counter locks here are short-lived; none acquires a backend or App lock.
    fn sample_store(&mut self) {
        let queue = &self.store_queue;
        let pending = queue.pending();
        let written = queue.written();
        let failed = queue.failed();
        let last_failure = match queue.last_failure() {
            Some(refusal) => json!({
                "key": refusal.key,
                "op": refusal.op.as_str(),
                "err": refusal.code,
                "err_name": refusal.name,
                "at_ms": refusal.at_ms,
            }),
            None => Value::Null,
        };
        self.runtime.store_stats = json!({
            "pending": pending,
            "written": written,
            "failed": failed,
            "last_failure": last_failure,
        });
    }

    /// Queue the config for the store writer.
    ///
    /// Every `persist_*` in this file works this way: the state is serialised here, queued
    /// in [`store::StoreQueue`], and written by the platform's writer task. `set` is true
    /// when the write was accepted *into the queue*; whether flash took it is reported by
    /// the writer (`/api/status.store.failed` and the `store` log line), because the caller
    /// here is very often the agent thread, which cannot touch flash at all — see
    /// [`App::flush_store`].
    fn persist_config(&mut self) -> bool {
        let json = self.cfg.to_json_string();
        let queued = self.store.set(crate::config::NVS_KEY_CONFIG, &json);
        if queued {
            self.log.info("config", "queued for saving");
        } else {
            self.log.error("config", "the config could not even be queued for saving");
        }
        self.runtime.config_dirty = false;
        queued
    }

    fn persist_manifest(&mut self) {
        let json = self.manifest.to_json_string();
        if self.store.set(NVS_KEY_MANIFEST, &json) {
            self.runtime.manifest_rev += 1;
            self.log
                .info("manifest", format!("queued for saving, live revision {}", self.runtime.manifest_rev));
        } else {
            self.log.error("manifest", "the manifest could not even be queued for saving");
        }
    }

    /// Work owed after a call or a request: persist, rebuild, notice a restart.
    pub fn settle(&mut self) {
        self.invalidate_tool_specs();
        // A tool that writes (a file, the config, the schedule) answers "queued" and leaves the
        // outcome to the store writer. Nothing else samples it for the tools, so the reading is
        // taken here, on the only thread that owns both — `board status` then has an answer to
        // "did the change reach flash?" that an MCP caller can actually see.
        self.sample_store();
        // Same reason, one field over: a turn that was queued outside the agent (`/api/agent/
        // message`, a schedule) changes `pending` without another step starting.
        self.publish_agent_stats();
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
        // Peek through the streaming reader, not `get`/the queue's default `read_at`,
        // which would materialize the complete legacy document before its limits apply.
        let mut chunked = false;
        let loaded = (|| -> std::result::Result<Option<AppWorkspace>, serde_json::Error> {
            use std::io::Read as _;
            let Some(mut reader) = self.store.reader(NVS_KEY_WORKSPACE).or_else(|| {
                self.store
                    .get(NVS_KEY_WORKSPACE)
                    .map(|text| Box::new(std::io::Cursor::new(text.into_bytes())) as Box<dyn std::io::Read>)
            }) else {
                return Ok(None);
            };
            let mut peek = [0u8; 4];
            let mut head = 0;
            while head < peek.len() {
                match reader.read(&mut peek[head..]) {
                    Ok(0) => break,
                    Ok(n) => head += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(serde_json::Error::io(e)),
                }
            }
            if head == 4 && &peek == b"ws1 " {
                chunked = true;
                return Ok(None);
            }
            AppWorkspace::from_reader(std::io::Cursor::new(&peek[..head]).chain(reader)).map(Some)
        })();
        if chunked {
            self.load_workspace_chunked();
            return;
        }
        match loaded {
            Ok(Some(ws)) => {
                self.workspace = ws;
                self.migrate_workspace("one document");
            }
            other => {
                // A reader cannot distinguish absent from unreadable on every backend.
                // Look up names only on this failure path; never discard orphaned values.
                let has_workspace =
                    self.store.keys().iter().any(|key| key == NVS_KEY_WORKSPACE || key.starts_with("ws."));
                if has_workspace {
                    let error =
                        other.err().map_or_else(|| "missing workspace header".to_string(), |e| e.to_string());
                    let why =
                        format!("the stored workspace is unreadable ({error}); stored data was preserved");
                    self.log.error("agent", why.clone());
                    self.workspace.refuse_load(why);
                }
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
                    "queued workspace migration from {from} to one value per file: {files} file(s), {bytes} bytes; the old layout stays until publication succeeds"
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
        let loaded = (|| -> std::result::Result<AppWorkspace, serde_json::Error> {
            let reader = self
                .store
                .reader(crate::keys::WORKSPACE_INDEX)
                .or_else(|| {
                    self.store.get(crate::keys::WORKSPACE_INDEX).map(|text| {
                        Box::new(std::io::Cursor::new(text.into_bytes())) as Box<dyn std::io::Read>
                    })
                })
                .ok_or_else(|| {
                    serde_json::Error::io(std::io::Error::other("workspace index is absent or unreadable"))
                })?;
            // Validate the complete index, including each derived key, before accessing
            // any file content. An invalid suffix must not authorize reads from other keys.
            let index = workspace_json::index(reader)?;
            let mut ws = AppWorkspace::default();
            for (path, key) in index {
                let reader = self
                    .store
                    .reader(&key)
                    .or_else(|| {
                        self.store.get(&key).map(|text| {
                            Box::new(std::io::Cursor::new(text.into_bytes())) as Box<dyn std::io::Read>
                        })
                    })
                    .ok_or_else(|| {
                        serde_json::Error::io(std::io::Error::other(
                            "indexed workspace file is absent or unreadable",
                        ))
                    })?;
                let allowance = WORKSPACE_FILE_MAX.min(WORKSPACE_TOTAL_MAX - ws.bytes());
                let text = workspace_json::text(reader, allowance)?;
                ws.insert_checked(&path, std::borrow::Cow::Owned(text), false)
                    .map_err(<serde_json::Error as serde::de::Error>::custom)?;
            }
            ws.saved = ws.files.iter().map(|(path, text)| (path.clone(), crate::keys::fnv1a(text))).collect();
            Ok(ws)
        })();
        match loaded {
            Ok(ws) => {
                self.log.info(
                    "agent",
                    format!("workspace: {} file(s), {} bytes from the index", ws.len(), ws.bytes()),
                );
                self.workspace = ws;
                true
            }
            Err(e) => {
                if !self.store.keys().iter().any(|key| key == crate::keys::WORKSPACE_INDEX) {
                    return false;
                }
                let why = format!("the workspace index is unreadable ({e}); no files were loaded; stored data was preserved");
                self.log.error("agent", why.clone());
                self.workspace.refuse_load(why);
                true
            }
        }
    }

    /// The chunked layout: a `ws1` header under `ws` and the document in `ws.1`, `ws.2`, ...
    fn load_workspace_chunked(&mut self) {
        let loaded = (|| -> std::result::Result<(AppWorkspace, bool), String> {
            // The historical header/chunks each fit an NVS value. Stage only that bounded
            // snapshot before reusing the shared checksum validation, never an unbounded `get`.
            let mut snapshot = store::MemoryStore::new();
            let reader = self.store.reader(NVS_KEY_WORKSPACE).ok_or("workspace header is unreadable")?;
            let header = workspace_json::text(reader, store::VALUE_MAX).map_err(|e| e.to_string())?;
            let count = if header.starts_with("ws1 ") {
                let count = header
                    .split_whitespace()
                    .nth(1)
                    .and_then(|n| n.parse::<usize>().ok())
                    .ok_or("invalid workspace chunk count")?;
                if count > WORKSPACE_PLAN.chunks_max {
                    return Err("workspace chunk count exceeds its limit".to_string());
                }
                count
            } else {
                0
            };
            snapshot.set(NVS_KEY_WORKSPACE, &header);
            for chunk in 1..=count {
                let key = crate::keys::workspace_chunk(chunk);
                let reader = self.store.reader(&key).ok_or("workspace chunk is unreadable")?;
                let text = workspace_json::text(reader, store::CHUNK_BYTES).map_err(|e| e.to_string())?;
                snapshot.set(&key, &text);
            }
            let (text, legacy) = match store::read_blob(&snapshot, NVS_KEY_WORKSPACE, WORKSPACE_PLAN) {
                store::BlobState::Stored(text) => (text, false),
                store::BlobState::Legacy(text) => (text, true),
                store::BlobState::Corrupt(why) => return Err(why),
                store::BlobState::Missing => return Err("workspace header is missing".to_string()),
            };
            let ws = AppWorkspace::from_reader(text.as_bytes()).map_err(|e| e.to_string())?;
            Ok((ws, legacy))
        })();
        match loaded {
            Ok((ws, legacy)) => {
                self.workspace = ws;
                if legacy {
                    self.migrate_workspace("one value");
                }
            }
            Err(e) => {
                let why = format!("the stored workspace is unreadable ({e}); stored data was preserved");
                self.log.error("agent", why.clone());
                self.workspace.refuse_load(why);
            }
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

    /// Queue through the App, so every trigger keeps the same truncated/coalesced message
    /// in RAM and in the store queue. Queue admission is not a flash acknowledgement.
    fn queue_user(&mut self, text: &str, source: &str, at_ms: u64) -> bool {
        if !self.agent.push_user(text, source, at_ms) {
            return false;
        }
        self.persist_pending_message();
        // A queued turn that has not started is exactly what `board status` reports as
        // `pending`, and this is the moment it becomes true.
        self.publish_agent_stats();
        true
    }

    fn persist_pending_message(&mut self) -> bool {
        if !self.cfg.agent.persist_transcript {
            return false;
        }
        let Some(pending) = self.agent.pending.as_ref() else {
            return false;
        };
        let saved = json!({
            "text": pending.text,
            "source": pending.source,
            "at_ms": pending.at_ms,
        })
        .to_string();
        let ok = self.store.set(NVS_KEY_PENDING_MESSAGE, &saved);
        if !ok {
            self.log.warn("agent", "could not queue the pending user message for saving; a reboot loses it");
        }
        ok
    }

    fn clear_pending_message(&mut self) {
        // Do not enqueue an absent-key removal when persistence is off. An existing copy
        // is still removed regardless of the setting, so a forget cannot resurrect it.
        if self.store.get(NVS_KEY_PENDING_MESSAGE).is_some() && !self.store.remove(NVS_KEY_PENDING_MESSAGE) {
            self.log.warn("agent", "could not queue removal of the stored pending user message");
        }
        self.publish_agent_stats();
    }

    fn load_pending_message(&mut self) {
        let Some(saved) = self.store.get(NVS_KEY_PENDING_MESSAGE) else {
            return;
        };
        if !self.cfg.agent.persist_transcript {
            self.clear_pending_message();
            return;
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct QueuedUser {
            text: String,
            source: String,
            at_ms: u64,
        }
        let parsed =
            serde_json::from_str::<QueuedUser>(&saved).ok().filter(|message| !message.text.trim().is_empty());
        let Some(parsed) = parsed else {
            self.clear_pending_message();
            self.log.warn(
                "agent",
                "the stored pending user message does not check out (torn or foreign JSON); ignored and removed",
            );
            return;
        };
        let pending = crate::agent::Message::user(&parsed.text, &parsed.source, parsed.at_ms);
        // The transcript's final exchange wins over a stale queue copy. Text alone is
        // not an identity: a person can ask the same question again. Compare source and
        // enqueue time too, and compare the transcript's 4 KiB text projection because
        // that copy is shorter than a queued question may be. This is snapshot de-dup,
        // not a globally unique turn id or an exactly-once execution guarantee.
        let in_transcript =
            self.agent.history.iter().rfind(|m| m.role == crate::agent::Role::User).is_some_and(|last| {
                last.source == pending.source
                    && last.at_ms == pending.at_ms
                    && last.text == llm::truncate_bytes(&pending.text, crate::agent::MAX_SAVED_TEXT_BYTES)
            });
        if in_transcript {
            self.clear_pending_message();
            self.log.info("agent", "dropped a stored pending message already present in the transcript");
            return;
        }
        self.agent.pending = Some(pending);
        self.log.info("agent", "restored a queued user message from flash");
    }

    /// Restore the transcript from flash and finish a turn a power cut interrupted.
    ///
    /// Three independent pieces of evidence are read here, in this order:
    ///
    /// * the transcript, which holds everything up to the last completed step. A blob that
    ///   does not check out - torn by a power failure, written by a newer build, or just not
    ///   JSON - is logged and skipped, never a panic and never half-adopted;
    /// * the queued question, which has not started yet. The final transcript exchange
    ///   takes precedence over an identical stale queue copy; otherwise the queued question
    ///   wins over replaying an older interrupted turn in this depth-1 queue;
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
        self.load_pending_message();
        // The marker is consumed whatever the setting says: leaving one behind would resume
        // a turn from weeks ago the day persistence is switched back on.
        let Some(marker) = self.store.get(NVS_KEY_RUN_IN_FLIGHT).filter(|text| !text.is_empty()) else {
            return;
        };
        let parsed: Value = serde_json::from_str(&marker).unwrap_or(Value::Null);
        let source = parsed.get("source").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let started = parsed.get("started_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        self.store.remove(NVS_KEY_RUN_IN_FLIGHT);
        if self.agent.pending.is_some() {
            self.log.warn(
                "agent",
                format!(
                    "a turn from `{}` (started {started} ms after boot) was interrupted by a reboot; keeping the newer queued question instead of replaying the older turn",
                    clip(&source, 40)
                ),
            );
            return;
        }
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
                self.queue_user(&text, RESUME_SOURCE, now_ms());
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
        self.active_attachments = self.agent.pending.as_ref().map(|pending| pending.attachments.clone());
        let max_steps = self.llm.max_steps;
        let (context_tokens, max_output_tokens) = (self.llm.context_tokens, self.llm.max_output_tokens);
        // Before the run, not after: the marker is what tells the next boot that this turn
        // did not finish, and a marker written afterwards would only describe turns that
        // already survived.
        self.mark_run_in_flight(&source);
        self.run_gate.set_active(true);
        let step = self.with_agent(move |agent, host| {
            agent.set_budget(context_tokens, max_output_tokens);
            agent.begin(host, now_ms, max_steps)
        });
        // The queued question is in the transcript now. Queue its save before removing
        // the pending copy, and only after begin actually consumed the message. A reboot
        // between those writes can leave both copies; load_pending_message de-duplicates them.
        if step.is_some() {
            self.persist_transcript();
            self.clear_pending_message();
        }
        step
    }

    /// Phase 2: fold one model reply back in (journal, tool calls, transcript) and say
    /// whether to ask again.
    pub fn agent_reply(&mut self, reply: std::result::Result<llm::ModelReply, String>) -> AgentStep {
        let step = self.with_agent(move |agent, host| agent.apply(host, reply, now_ms));
        if matches!(&step, AgentStep::Done(_)) {
            self.active_attachments = None;
            self.run_gate.set_active(false);
            // A cancel that arrived after the final step has nothing left to stop, and must
            // not carry into the next turn (whose first boundary would otherwise stop it).
            self.run_gate.set_cancelled(false);
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
        self.agent_out = true;
        let out = run(&mut agent, self);
        self.agent = agent;
        self.agent_out = false;
        // The only place the real agent comes back, so the only place its numbers can be
        // published for a caller that is not the agent itself.
        self.publish_agent_stats();
        out
    }

    /// What the board's own agent is doing, as plain data for a caller.
    ///
    /// `run_active` and `active_tool` are not here: they come from the `RunGate`, which is
    /// live while a turn runs. Everything in this snapshot is as of the last step boundary,
    /// which is the honest reading — the numbers a turn is in the middle of producing are
    /// not finished numbers.
    fn publish_agent_stats(&mut self) {
        if self.agent_out {
            return;
        }
        let stats = {
            let a = &self.agent;
            json!({
                "runs": a.runs,
                "history": a.history.len(),
                "pending": a.pending.is_some(),
                "last_error": &a.last_error,
            })
        };
        self.runtime.agent = stats;
    }

    /// Evaluate the schedule once and queue whatever came due.
    ///
    /// Kept here rather than in the platform layer because firing a task touches the
    /// store (last_run_ms), the tool host (sensor rules call a tool) and the agent
    /// queue. Pure time is passed in, so the host tests can drive it.
    ///
    /// The local minute is derived here from `tz_offset_h` rather than handed in by the
    /// platform: the offset is a setting, and a platform that computed the minute itself
    /// would need a lock on the config before every tick and would still be the wrong
    /// place for a value the portal edits.
    /// Returns the sources that were queued, in order.
    pub fn sched_tick(&mut self, at_ms: u64) -> Vec<String> {
        let minute_of_day = crate::sched::minute_of_day(at_ms, self.cfg.tz_offset_h);
        let mut fired = Vec::new();
        let mut spent: Vec<String> = Vec::new();
        let mut dirty = false;
        for i in 0..self.tasks.len() {
            if !self.tasks[i].enabled || !crate::sched::due(&self.tasks[i], at_ms, minute_of_day) {
                continue;
            }
            let source = format!("schedule:{}", self.tasks[i].id);
            let prompt = self.tasks[i].prompt.clone();
            let script = self.tasks[i].script.clone();
            // A one-shot is done the moment it fires: "in ten seconds" is not a task the
            // caller should have to remember to remove, and a task left behind would fire
            // again on its next interval. Removed after the loop, because the loop is walking
            // these indices.
            if self.tasks[i].after_s > 0 {
                spent.push(self.tasks[i].id.clone());
            }
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
                let job = self.submit_script(&source_text);
                self.log.info("script", format!("{source} ran a script: {}", job.id));
                fired.push(source);
            } else if !prompt.trim().is_empty() {
                self.queue_user(&prompt, &source, at_ms);
                self.log.info("agent", format!("queued {source}"));
                fired.push(source);
            }
        }
        if !spent.is_empty() {
            self.tasks.retain(|t| !spent.contains(&t.id));
            for id in &spent {
                self.log.info("agent", format!("schedule: one-shot `{id}` fired and was removed"));
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
                self.queue_user(&prompt, &source, at_ms);
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

    fn persist_agent_cfg(&mut self) -> bool {
        let text = llm_json(&self.llm, true).to_string();
        let queued = self.store.set(NVS_KEY_AGENT_CFG, &text);
        if !queued {
            self.log.error("agent", "could not queue the model config for saving");
        }
        queued
    }

    fn persist_sched(&mut self) {
        let text = self.sched_json().to_string();
        if !self.store.set(NVS_KEY_SCHED, &text) {
            self.log.error("agent", "could not queue the schedule for saving");
        }
    }

    /// Admit one incremental workspace publication, not a synchronous durability promise.
    /// Files precede the index; obsolete values and legacy data are removed only after
    /// every required file and the index actually succeeds. Path-derived physical keys
    /// still mean replacements are not an all-files transaction.
    fn persist_workspace(&mut self) -> std::result::Result<(), String> {
        self.acknowledge_workspace();
        self.workspace.writable()?;
        let mut index = std::collections::BTreeMap::new();
        let mut hashes = std::collections::BTreeMap::new();
        let mut writes = Vec::new();
        let mut changed = Vec::new();
        for (path, text) in &self.workspace.files {
            let key = crate::keys::workspace_file(path);
            let hash = crate::keys::fnv1a(text);
            // A queued/in-flight newer value may differ even when this edit returns to the
            // last durable hash. Such a revert must replace that pending write too.
            let pending_matches =
                self.workspace_pending.as_ref().map_or(true, |save| save.hashes.get(path) == Some(&hash));
            if self.workspace.saved.get(path) != Some(&hash) || !pending_matches {
                writes.push((key.clone(), text.as_str()));
                changed.push(path.clone());
            }
            index.insert(path, key);
            hashes.insert(path.clone(), hash);
        }

        let mut cleanup = self.workspace_stale_keys.clone();
        cleanup.extend(self.workspace_pending.iter().flat_map(|save| save.cleanup.iter()).cloned());
        for path in self
            .workspace
            .saved
            .keys()
            .chain(self.workspace_pending.iter().flat_map(|save| save.hashes.keys()))
        {
            if !self.workspace.files.contains_key(path) {
                cleanup.insert(crate::keys::workspace_file(path));
            }
        }
        // Enumerating names avoids materializing a possibly megabyte legacy document just
        // to decide whether it needs cleanup. Check every historical chunk, including gaps.
        let legacy: std::collections::BTreeSet<String> = (1..=WORKSPACE_PLAN.chunks_max)
            .map(crate::keys::workspace_chunk)
            .chain(std::iter::once(NVS_KEY_WORKSPACE.to_string()))
            .collect();
        for key in self.store.keys() {
            if legacy.contains(&key) {
                cleanup.insert(key);
            }
        }
        // A new path may reuse an old path's physical hash; cleanup cannot unlink a key
        // named by the new index, even across such a collision between generations.
        for key in index.values() {
            cleanup.remove(key);
        }
        let prospective_stale = cleanup.len().saturating_add(writes.len()).saturating_add(
            self.workspace_stale_keys
                .iter()
                .filter(|key| !cleanup.contains(*key) && !writes.iter().any(|(write, _)| write == *key))
                .count(),
        );
        if prospective_stale > store::PENDING_KEYS_MAX {
            return Err(format!("the workspace was not saved: {prospective_stale} cleanup/dependency keys exceed the {}-key persistence budget; previous queue unchanged", store::PENDING_KEYS_MAX));
        }
        let doc = serde_json::to_string(&index)
            .map_err(|e| format!("the workspace index could not be written: {e}"))?;
        let writes_ref: Vec<(&str, &str)> = writes.iter().map(|(key, text)| (key.as_str(), *text)).collect();
        let cleanup: Vec<String> = cleanup.into_iter().collect();
        let cleanup_ref: Vec<&str> = cleanup.iter().map(String::as_str).collect();
        let receipt = self.store.publish(crate::keys::WORKSPACE_INDEX, &doc, &writes_ref, &cleanup_ref);
        let Some(receipt) = receipt else {
            let detail = self
                .store
                .last_error()
                .map_or_else(|| "the store refused a file or index request".to_string(), |error| error.name);
            let why = format!(
                "the workspace was not saved ({}): {detail}; see /api/status.store",
                changed.join(", ")
            );
            self.log.error("agent", why.clone());
            return Err(why);
        };
        self.workspace_admissions = self.workspace_admissions.wrapping_add(1);
        let previous = self.workspace_pending.replace(WorkspaceSave { receipt, hashes, changed, cleanup });
        // Admission waited for any in-flight drain. Its older receipt is now completed or
        // confirmed superseded; settle that generation without acknowledging the new one.
        if let Some(previous) = previous {
            if let Some(outcome) = previous.receipt.outcome() {
                self.apply_workspace_outcome(previous, outcome);
            }
        }
        self.acknowledge_workspace(); // Also supports a genuinely synchronous KvStore.
        Ok(())
    }

    fn acknowledge_workspace(&mut self) {
        let Some(outcome) = self.workspace_pending.as_ref().and_then(|save| save.receipt.outcome()) else {
            return;
        };
        let save = self.workspace_pending.take().expect("the receipt belongs to this generation");
        self.apply_workspace_outcome(save, outcome);
    }

    fn apply_workspace_outcome(&mut self, save: WorkspaceSave, outcome: store::PublicationOutcome) {
        match outcome {
            store::PublicationOutcome::Superseded => {}
            store::PublicationOutcome::Finished { published: true, cleanup_failed } => {
                self.workspace.saved = save.hashes;
                for path in self.workspace.saved.keys() {
                    self.workspace_stale_keys.remove(&crate::keys::workspace_file(path));
                }
                for key in save.cleanup {
                    self.workspace_stale_keys.remove(&key);
                }
                self.workspace_stale_keys.extend(cleanup_failed);
            }
            store::PublicationOutcome::Finished { published: false, .. } => {
                // Some path-addressed values may have changed even though the index did not
                // publish. Their old hashes cannot safely suppress a retry or a revert.
                for path in save.changed {
                    self.workspace.saved.remove(&path);
                    self.workspace_stale_keys.insert(crate::keys::workspace_file(&path));
                }
                self.workspace_stale_keys.extend(save.cleanup);
                self.log.error("agent", "the workspace publication failed; the RAM edit is not confirmed durable, obsolete/legacy cleanup was skipped, and a later save can retry");
            }
        }
    }

    /// PSRAM free right now, in bytes, when the platform can say.
    ///
    /// Asked before a change that needs a copy of the workspace, so that running out is a
    /// refusal with numbers in it instead of the allocator's `abort()`.
    fn psram_free_bytes(&mut self) -> Option<usize> {
        self.hw.heap_pools().into_iter().find(|p| p.pool == "spiram").map(|p| p.free_kb as usize * 1024)
    }

    /// One change admitted for persistence, or fully undone on a closure/admission error.
    /// A later backend refusal is asynchronous: RAM retains the edit, diagnostics report
    /// the failure, and a subsequent explicit save can retry it.
    fn workspace_edit<T>(
        &mut self,
        change: impl FnOnce(&mut AppWorkspace) -> std::result::Result<T, String>,
    ) -> std::result::Result<T, String> {
        // Settle an earlier completed publication before estimating or cloning the rollback;
        // otherwise a refused new admission could restore stale persistence hashes.
        self.acknowledge_workspace();
        // The rollback below needs a copy of the whole map, so this is the moment the board can
        // run out of PSRAM. Ask first: a refused write is a sentence the caller can act on, and
        // an allocation that fails is `memory allocation of N bytes failed` followed by
        // `abort()` - a reboot, measured on this board.
        if let Some(free) = self.psram_free_bytes() {
            workspace_fits(&self.workspace, Some(free))?;
        }
        let prior_receipt = self.workspace_pending.as_ref().map(|save| save.receipt.clone());
        let mut before = self.workspace.clone();
        let outcome = match change(&mut self.workspace) {
            Ok(outcome) => outcome,
            Err(e) => {
                self.workspace = before;
                self.acknowledge_workspace();
                return Err(e);
            }
        };
        match self.persist_workspace() {
            Ok(()) => Ok(outcome),
            Err(e) => {
                // The old writer may have completed after the rollback clone, and
                // persist_workspace may already have settled that receipt before refusing
                // this new admission. Undo file edits, not that durability acknowledgement
                // (or hash invalidation): otherwise a later revert can skip a required write.
                if self.workspace_pending.is_none()
                    && prior_receipt.as_ref().is_some_and(|receipt| {
                        matches!(receipt.outcome(), Some(store::PublicationOutcome::Finished { .. }))
                    })
                {
                    before.saved = std::mem::take(&mut self.workspace.saved);
                }
                self.workspace = before;
                self.acknowledge_workspace();
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
            // Seeded files use the same incremental workspace publication. A RAM-only
            // skill would be seeded again after reboot. Seeding is best effort, so an
            // admission/backend refusal is a log/store diagnostic, not a startup error.
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

        // Admit the whole candidate before replacing the download list or queued
        // message. Otherwise a rejected second attachment loses the previous image.
        let mut attachments: Vec<crate::agent::Attachment> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        if let Some(given) = body.get("attachments") {
            let Some(list) = given.as_array() else {
                return Response::error_json(400, "`attachments` must be an array");
            };
            if list.len() > MAX_ATTACHMENTS {
                return Response::json_status(
                    413,
                    &json!({ "error": format!("{} attachments; the limit is {MAX_ATTACHMENTS}", list.len()) }),
                );
            }
            for item in list {
                if !item.is_object() {
                    return Response::error_json(400, "each attachment must be an object");
                }
                for field in ["name", "mime", "data_b64"] {
                    if item.get(field).is_some_and(|v| !v.is_string()) {
                        return Response::error_json(400, &format!("attachment `{field}` must be a string"));
                    }
                }
                let name = item.get("name").and_then(Value::as_str).unwrap_or("attachment");
                if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                    return Response::error_json(
                        400,
                        "attachment name must be 1..=128 bytes without control characters",
                    );
                }
                if attachments.iter().any(|a| a.name == name) {
                    return Response::error_json(400, "attachment names must be unique");
                }
                let mime = item.get("mime").and_then(Value::as_str).unwrap_or("application/octet-stream");
                let token = |part: &str| {
                    !part.is_empty()
                        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                };
                if mime.len() > 128
                    || !mime.split_once('/').is_some_and(|(kind, subtype)| token(kind) && token(subtype))
                {
                    return Response::error_json(
                        400,
                        "attachment mime must be a bare type/subtype of at most 128 bytes",
                    );
                }
                let mut bytes = Vec::new();
                let encoded = item.get("data_b64").and_then(Value::as_str).unwrap_or("");
                if let Err(e) = crate::util::b64_decode_limit(encoded, &mut bytes, MAX_ATTACHMENT_BYTES) {
                    let status = if e.data.as_ref().is_some_and(|data| data.get("decoded_bytes").is_some()) {
                        413
                    } else {
                        400
                    };
                    return Response::json_status(
                        status,
                        &json!({ "error": format!("`{name}`: {}", e.message) }),
                    );
                }
                let mut path = String::new();
                for byte in name.bytes() {
                    if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                        path.push(byte as char);
                    } else {
                        use std::fmt::Write as _;
                        let _ = write!(path, "%{byte:02X}");
                    }
                }
                notes.push(format!(
                    "[attached: {name}, {mime}, {} kB — fetch with GET /api/attachment/{path}]",
                    bytes.len().div_ceil(1024)
                ));
                attachments.push(crate::agent::Attachment {
                    name: name.to_string(),
                    mime: mime.to_ascii_lowercase(),
                    data_b64: crate::util::b64_encode(&bytes),
                });
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
        self.agent.vision = self.llm.vision;
        if !self.queue_user(&text, "http", now_ms()) {
            return Response::json_status(400, &json!({ "error": "`text` is required" }));
        }
        // A text-only message intentionally replaces the list with an empty one.
        // The running turn keeps its own snapshot if another message is queued.
        if let Some(message) = self.agent.pending.as_mut() {
            message.attachments = attachments.clone();
        }
        self.attachments = attachments;
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
    /// This returns names and sizes, never the file bodies; the file-count and path
    /// budgets bound its metadata even with a multi-MiB workspace. Every file brings its path, and
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
        if let Err(r) = self.require_read(req) {
            return r;
        }
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
        match self.apply_agent_config(&body) {
            Ok((mut v, queued)) => {
                v["saved"] = json!(false);
                v["queued"] = json!(queued);
                v["note"] = json!("applied in RAM; queue admission is not flash acknowledgement. Check /api/status.store for pending writes and failures");
                if queued {
                    Response::json(&v)
                } else {
                    v["error"] = json!("model config was applied in RAM but could not be queued for saving");
                    Response::json_status(503, &v)
                }
            }
            Err(e) => Response::json_status(400, &json!({ "error": e.message })),
        }
    }

    /// The `agent_*` tools: the board's own model, reached by a caller that is not it.
    ///
    /// `agent_begin`/`agent_reply` do the same work over REST for the portal; these are thin
    /// wrappers so an outside agent gets the identical behaviour (the same coalescing queue,
    /// the same truncation, the same refusal while a turn runs) instead of a second
    /// implementation that drifts.
    fn agent_say_tool(&mut self, args: &Value) -> Result<Value> {
        let text = crate::json::str_arg(args, "text")?;
        let text = text.trim();
        if text.is_empty() {
            return Err(McpError::invalid_params("`text` is empty: say something"));
        }
        let replaced = self.agent.pending.is_some();
        let accepted = self.queue_user(text, "mcp", now_ms());
        if !accepted {
            return Err(McpError::internal("the message could not be queued"));
        }
        Ok(json!({
            "ok": true,
            "source": "mcp",
            "bytes": text.len(),
            // A queued turn is the answer, not a failure: the board runs it on its own thread
            // and the transcript is where the reply lands.
            "pending": true,
            "replaced_a_waiting_message": replaced,
            "next": "the board starts the turn on its own thread; `agent_history` has the answer",
        }))
    }

    fn agent_history_tool(&mut self, args: &Value) -> Result<Value> {
        let after = crate::json::opt_i64(args, "after").unwrap_or(0).max(0) as usize;
        Ok(self.agent_history_json(after))
    }

    fn agent_forget_tool(&mut self, args: &Value) -> Result<Value> {
        let keep = crate::json::opt_i64(args, "keep").unwrap_or(0).max(0) as usize;
        if self.run_gate.active() {
            return Err(McpError::invalid_params(
                "a model turn is running, so the conversation cannot be cut: its question would be gone and \
                 its answer would arrive in a thread that no longer has it. Wait for `board status` to say \
                 `agent.run_active: false`",
            ));
        }
        Ok(self.forget_conversation(keep))
    }

    fn agent_config_set_tool(&mut self, args: &Value) -> Result<Value> {
        let patch = crate::json::field(args, "patch")
            .cloned()
            .ok_or_else(|| McpError::invalid_params("`patch` is required: the fields to change"))?;
        if !patch.is_object() {
            return Err(McpError::invalid_params("`patch` must be an object"));
        }
        let (mut value, queued) = self.apply_agent_config(&patch)?;
        value["saved"] = json!(false);
        value["queued"] = json!(queued);
        if !queued {
            value["error"] = json!("applied in RAM but could not be queued for saving");
        }
        Ok(value)
    }

    /// Cut the conversation down to the last `keep` exchanges.
    ///
    /// Shared by `POST /api/agent/forget` and the `agent_forget` tool: forgetting has to reach
    /// flash or the conversation comes back after a reboot, which is the one outcome nobody
    /// asking for `forget` would accept.
    fn forget_conversation(&mut self, keep: usize) -> Value {
        let (removed, kept) = self.agent.forget(keep);
        self.clear_pending_message();
        if keep == 0 {
            self.attachments.clear();
            self.active_attachments = None;
        }
        self.persist_transcript();
        self.publish_agent_stats();
        self.log.info(
            "agent",
            format!("forgot {removed} message(s), kept the last {kept} toward {keep} exchange(s)"),
        );
        json!({ "ok": true, "removed": removed, "kept": kept, "archive_cleared": true })
    }

    /// The body of `POST /api/agent/config`, shared with the `agent_config_set` tool.
    ///
    /// One function so the portal and an outside agent cannot disagree about what is accepted,
    /// what is clamped and what is refused. Returns the settings as the caller may see them
    /// (no key) and whether the write was queued.
    fn apply_agent_config(&mut self, body: &Value) -> Result<(Value, bool)> {
        // Both schemes are carried by the platform transport (TLS on the board via esp-tls,
        // on the host via rustls), so https:// is accepted here. Anything that is not HTTP at
        // all is still refused by name.
        if let Some(url) = body.get("base_url").and_then(|u| u.as_str()) {
            let url = url.trim();
            if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(McpError::invalid_params(
                    "`base_url` must start with http:// or https:// and point at the chat completions endpoint",
                ));
            }
        }
        // Apply the trimmed value too, so what is stored is exactly what was checked.
        let mut patch = body.clone();
        if let Some(url) = patch.get("base_url").and_then(|u| u.as_str()) {
            let url = url.trim().to_string();
            patch["base_url"] = json!(url);
        }
        llm_apply_json(&mut self.llm, &patch);
        // The tick belongs to the device config, not the model config, but the portal edits
        // both from one form: accept it here and clamp it the way the platform does, so an
        // out-of-range value never reaches the thread.
        if let Some(ms) = body.get("tick_ms").and_then(|v| v.as_u64()) {
            let clamped = crate::config::clamp_tick_ms(ms.min(u32::MAX as u64) as u32);
            self.cfg.agent.tick_ms = clamped;
            self.runtime.config_dirty = true;
            self.log.info("agent", format!("scheduler tick is now {clamped} ms"));
        }
        let queued = self.persist_agent_cfg();
        self.log.info(
            "agent",
            format!(
                "model config: {} model={} key={}",
                clip(&self.llm.base_url, 60),
                self.llm.model,
                if self.llm.api_key.is_empty() { "none" } else { "set" }
            ),
        );
        Ok((self.agent_config_json(), queued))
    }

    fn sched_json(&self) -> Value {
        json!({
            "now_ms": now_ms(),
            "tasks": self.tasks.iter().map(task_json).collect::<Vec<Value>>(),
            "rules": self.rules.iter().map(rule_json).collect::<Vec<Value>>(),
        })
    }

    fn next_task_id(&self) -> String {
        next_schedule_task_id(&self.tasks)
    }

    fn next_rule_id(&self) -> String {
        next_schedule_id("r", self.rules.iter().map(|r| r.id.as_str()))
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
            let job = self.submit_script(&text);
            self.log.info("script", format!("{source} ran a script on demand: {}", job.id));
            out["kind"] = json!("script");
            out["job"] = json!(job.id);
            // submit_script has already finished. There is no retired REST job
            // poll endpoint to recover this result from, so return it now.
            out["state"] = json!(job.state);
            out["result"] = job.result;
            out["output"] = json!(job.output);
            out["steps"] = json!(job.steps);
            out["ms"] = json!(job.ms);
            if !job.error.is_empty() {
                out["error"] = json!(job.error);
            }
        } else if !prompt.trim().is_empty() {
            self.queue_user(&prompt, &source, at_ms);
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
        if body.get("tasks").is_some() || body.get("rules").is_some() {
            match self.sched_replacement(&body) {
                Ok((tasks, rules)) => {
                    self.tasks = tasks;
                    self.rules = rules;
                    self.persist_sched();
                    let mut v = self.sched_json();
                    v["ok"] = json!(true);
                    return Response::json(&v);
                }
                Err(e) => return Response::error_json(400, &e.message),
            }
        }
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
                let task = match task_from_api(src, &fallback) {
                    Ok(t) => t,
                    Err(e) => return Response::error_json(400, &e.message),
                };
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
                let rule = match rule_from_api(src, &fallback) {
                    Ok(r) => r,
                    Err(e) => return Response::error_json(400, &e.message),
                };
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

    /// Validate the advertised full replacement in temporary lists. A bad member
    /// must not erase a previously valid schedule or reset any existing clocks.
    fn sched_replacement(&self, body: &Value) -> Result<(Vec<ScheduledTask>, Vec<SensorRule>)> {
        if body.get("kind").is_some() || body.get("op").is_some() {
            return Err(McpError::invalid_params("do not mix schedule replacement with `kind`/`op`"));
        }
        let task_sources = body
            .get("tasks")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::invalid_params("replacement requires a `tasks` array"))?;
        let rule_sources = body
            .get("rules")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::invalid_params("replacement requires a `rules` array"))?;
        if task_sources.len() > MAX_TASKS || rule_sources.len() > MAX_RULES {
            return Err(McpError::invalid_params(format!(
                "at most {MAX_TASKS} scheduled tasks and {MAX_RULES} sensor rules"
            )));
        }
        let mut tasks: Vec<ScheduledTask> = Vec::with_capacity(task_sources.len());
        for src in task_sources {
            let fallback = next_schedule_id(
                "t",
                task_sources
                    .iter()
                    .filter_map(|v| v.get("id").and_then(Value::as_str))
                    .map(str::trim)
                    .chain(tasks.iter().map(|t| t.id.as_str())),
            );
            let mut task = task_from_api(src, &fallback)?;
            if tasks.iter().any(|t| t.id == task.id) {
                return Err(McpError::invalid_params(format!("duplicate task id `{}`", task.id)));
            }
            if src.get("last_run_ms").is_none() {
                if let Some(old) = self.tasks.iter().find(|t| t.id == task.id) {
                    task.last_run_ms = old.last_run_ms;
                }
            }
            tasks.push(task);
        }
        let mut rules: Vec<SensorRule> = Vec::with_capacity(rule_sources.len());
        for src in rule_sources {
            let fallback = next_schedule_id(
                "r",
                rule_sources
                    .iter()
                    .filter_map(|v| v.get("id").and_then(Value::as_str))
                    .map(str::trim)
                    .chain(rules.iter().map(|r| r.id.as_str())),
            );
            let mut rule = rule_from_api(src, &fallback)?;
            if rules.iter().any(|r| r.id == rule.id) {
                return Err(McpError::invalid_params(format!("duplicate rule id `{}`", rule.id)));
            }
            if src.get("last_fire_ms").is_none() {
                if let Some(old) = self.rules.iter().find(|r| r.id == rule.id) {
                    rule.last_fire_ms = old.last_fire_ms;
                }
            }
            rules.push(rule);
        }
        Ok((tasks, rules))
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
        let out = match self.model_transport.take() {
            Some(mut transport) => {
                self.agent.vision = cfg.vision;
                // Use the same consumption boundary as the platform driver: begin moves
                // the question into history and clears its stored queue copy before the
                // first model call, not after the entire synchronous turn has finished.
                let mut step = self.agent_begin().expect("the queued message was checked above");
                let out = loop {
                    match step {
                        AgentStep::Ask { messages, tools } => {
                            let reply = llm::chat(transport.as_mut(), &cfg, &messages, &tools);
                            step = self.agent_reply(reply);
                        }
                        AgentStep::Done(result) => break result,
                    }
                };
                self.model_transport = Some(transport);
                out
            }
            None => {
                let why = "no HTTP transport is attached for the model call, so the model cannot be reached"
                    .to_string();
                self.agent.last_error = why.clone();
                // No run started: keep (and re-persist) the queued question for a later
                // attempt rather than writing an in-flight marker for the previous turn.
                self.persist_pending_message();
                Err(why)
            }
        };
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
        let frame = match protocol::parse_frame(body) {
            Ok(m) => m,
            Err((id, code_n, message)) => {
                self.runtime.counters.bad_requests += 1;
                return Some(protocol::err(id, code_n, &message, None));
            }
        };
        let mut out = Vec::new();
        for message in frame.messages {
            match message {
                Ok(msg) => {
                    if let Some(v) = self.handle_one(msg, session, scheme) {
                        out.push(v);
                    }
                }
                Err((id, code_n, message)) => {
                    self.runtime.counters.bad_requests += 1;
                    out.push(protocol::err(id, code_n, &message, None));
                }
            }
        }
        self.settle();
        if out.is_empty() {
            None
        } else if frame.is_batch {
            Some(Value::Array(out))
        } else {
            out.pop()
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
                return None;
            }
        } else {
            self.runtime.counters.requests += 1;
        }
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
            "prompts/list" => Ok(self.prompts_list()),
            "prompts/get" => self.on_prompts_get(&msg.params),
            // A client asking the board to stop a turn. The MCP way to say what
            // `POST /api/agent/cancel` says; the gate is what makes it reachable while the
            // App lock is held by the very turn being stopped.
            "notifications/cancelled" => {
                self.request_cancel();
                Ok(json!({}))
            }
            "logging/setLevel" => Ok(json!({})),
            "completion/complete" => Ok(json!({ "completion": { "values": [], "total": 0 } })),
            other => Err(McpError::new(
                code::METHOD_NOT_FOUND,
                format!("method `{other}` is not supported by esp32-agent"),
            )),
        };
        match result {
            Ok(v) => (!msg.is_notification).then(|| protocol::ok(id, v)),
            Err(e) => {
                self.log.warn("rpc", format!("{}: {}", msg.method, e.message));
                (!msg.is_notification).then(|| protocol::err(id, e.code, &e.message, e.data))
            }
        }
    }

    /// The board's skills, as MCP prompts.
    ///
    /// A skill is a written procedure the board's model is given (the same index that reaches
    /// its system prompt), so it is exactly what an MCP prompt is: a named instruction set a
    /// client can list and fetch. Advertising `prompts/list` while answering `[]` was a
    /// capability that did nothing; answering it with the skills is the honest version, and it
    /// hands an outside agent the conventions this board was told to follow.
    fn prompts_list(&self) -> Value {
        let skills: Vec<Value> = crate::tools::knowledge::index(&self.workspace)
            .into_iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "arguments": [],
                })
            })
            .collect();
        json!({ "prompts": skills })
    }

    fn on_prompts_get(&mut self, params: &Value) -> Result<Value> {
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("").trim();
        if !crate::tools::knowledge::is_skill_name(name) {
            return Err(McpError::invalid_params(
                "`name` is required: one of the names `prompts/list` returned",
            ));
        }
        let skill =
            crate::tools::knowledge::index(&self.workspace).into_iter().find(|s| s.name == name).ok_or_else(
                || McpError::not_found(format!("no skill `{name}`; `prompts/list` has the ones that exist")),
            )?;
        // The body without its front matter: the YAML header is metadata the prompt already
        // carries as `description`, and a caller asking for a procedure wants the procedure.
        let text = crate::tools::fs::Workspace::read(&self.workspace, &skill.path)
            .map_err(|e| McpError::internal(format!("skill `{name}` could not be read: {e}")))?;
        let body = crate::tools::knowledge::split_front_matter(&text).1;
        Ok(json!({
            "description": skill.description,
            "messages": [{
                "role": "user",
                "content": { "type": "text", "text": body },
            }],
        }))
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
        let result = self.call_tool(&name, &args);
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
        self.sync_run_gate();
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
                    if let Err(r) = self.require_mcp(req) {
                        return self.with_cors(r);
                    }
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
            // The map an external agent reads before it touches anything. It is the entry
            // point, so it is a plain GET with no credential: a client that cannot read this
            // cannot discover how to authenticate either.
            ("GET", "/agent.json") => Response::json(&self.sitemap_json(req)),
            ("GET", "/api/tls") => Response::json(&self.tls.json()),
            ("GET", "/api/status") => {
                let v = self.status_json();
                Response::json(&v)
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
                if let Err(r) = self.require_read(req) {
                    return r;
                }
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
            // Stop the turn that is running. Cooperative: the loop checks between model
            // calls and between tool calls, because a request already on the wire cannot be
            // withdrawn from here. `run_end` follows either way, so a console knows the turn
            // is over instead of waiting for an answer that will not come.
            ("POST", "/api/agent/cancel") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                let cancelling = self.request_cancel();
                Response::json(&json!({ "cancelling": cancelling, "run_active": self.run_gate.active() }))
            }
            ("POST", "/api/agent/reset") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                if let Some(r) = self.refuse_while_running("clear the conversation") {
                    return r;
                }
                let (removed, kept) = self.agent.forget(0);
                self.clear_pending_message();
                self.attachments.clear();
                self.active_attachments = None;
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
                if let Some(r) = self.refuse_while_running("forget part of the conversation") {
                    return r;
                }
                let body: Value = serde_json::from_str(&req.body_str()).unwrap_or(Value::Null);
                let keep = body.get("keep").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                Response::json(&self.forget_conversation(keep))
            }
            // Compress the conversation now instead of waiting for the budget. It is the
            // same deterministic projection the trigger uses, so it costs no model call and
            // loses no fact: the dropped exchanges become archive lines. Refused mid-turn
            // for the same reason `reset` is - the turn being answered would be dropped out
            // from under the answer.
            ("POST", "/api/agent/compact") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                if let Some(r) = self.refuse_while_running("compress the conversation") {
                    return r;
                }
                let body: Value = serde_json::from_str(&req.body_str()).unwrap_or(Value::Null);
                let keep = match body.get("keep") {
                    None | Some(Value::Null) => None,
                    Some(v) => match v.as_u64() {
                        Some(n) => Some(n as usize),
                        None => return Response::error_json(400, "`keep` must be a non-negative integer"),
                    },
                };
                let report = self.agent.compact_now(keep);
                // The dropped messages must not come back after a reboot, exactly as for
                // `forget`. The archive itself lives in RAM; the transcript is what is stored.
                let stored = self.persist_transcript();
                self.log.info(
                    "agent",
                    format!(
                        "compressed the conversation: {} -> {} message(s), {} -> {} byte(s), kept {} exchange(s)",
                        report.messages_before,
                        report.messages_after,
                        report.bytes_before,
                        report.bytes_after,
                        report.kept_turns
                    ),
                );
                Response::json(&json!({
                    "ok": true,
                    "kept_turns": report.kept_turns,
                    "removed": report.removed,
                    "messages_before": report.messages_before,
                    "messages_after": report.messages_after,
                    "bytes_before": report.bytes_before,
                    "bytes_after": report.bytes_after,
                    "tokens_before": report.tokens_before,
                    "tokens_after": report.tokens_after,
                    "archive_lines": report.archive_lines,
                    "demoted_answers": report.demoted_answers,
                    "index_bytes": report.index_bytes,
                    // Freeing RAM is a different state from telling flash about it.
                    "stored": stored,
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
            ("GET", "/api/logs") => {
                if let Err(r) = self.require_read(req) {
                    return r;
                }
                let n = req.query_get("n").and_then(|v| v.parse().ok()).unwrap_or(80usize);
                let since = req.query_get("since").and_then(|v| v.parse().ok()).unwrap_or(0u64);
                let mut v = self.log.to_json(n.min(400), since);
                v["firmware"] = json!(self.firmware.version);
                Response::json(&v)
            }
            // The portal's Files tab is read-only but exposes private workspace contents.
            // Both answers carry the same numbers the `ls` and `read` tools return, because
            // there is one workspace and one meaning of "how big is it".
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
            ("GET", "/selftest/ping") => Response::json(&json!({
                "ok": true, "service": "selftest-net", "purpose": "lock-free target for net_get",
            })),
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
            // EventSource cannot set Authorization. Only a minted short-lived portal
            // token is accepted in the URL, and only for a non-mutating read.
            return matches!(req.method.as_str(), "GET" | "HEAD")
                && req.query_get("token").is_some_and(|token| self.runtime.token_valid(token, now_ms()));
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

    /// POST and DELETE share both the service switch and the bearer requirement.
    fn require_mcp(&mut self, req: &Request) -> std::result::Result<(), Response> {
        if !self.cfg.mcp.enabled {
            return Err(Response::error_json(503, "MCP service is disabled in the configuration"));
        }
        if self.cfg.auth.mcp_token.is_empty() {
            return self.require_pin(req);
        }
        let ok = req.header("authorization").and_then(|a| a.trim().split_once(' ')).is_some_and(
            |(scheme, token)| {
                scheme.eq_ignore_ascii_case("Bearer") && token.trim() == self.cfg.auth.mcp_token
            },
        );
        if ok {
            return Ok(());
        }
        self.runtime.counters.auth_failures += 1;
        Err(Response::json_status(401, &json!({ "error": "missing or wrong bearer token" }))
            .with_header("WWW-Authenticate", "Bearer"))
    }

    fn handle_mcp_post(&mut self, req: &Request) -> Response {
        if let Err(r) = self.require_mcp(req) {
            return r;
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

    fn update_tool_selection(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        if !body.is_object() {
            return Response::error_json(400, "tool selection must be an object");
        }
        let mode = match body.get("mode") {
            None => None,
            Some(Value::String(mode)) if matches!(mode.as_str(), "auto" | "all" | "generic") => {
                Some(mode.clone())
            }
            Some(_) => return Response::error_json(400, "mode must be auto, generic or all"),
        };
        let disabled = match body.get("disabled") {
            None => None,
            Some(Value::Array(names)) if names.iter().all(Value::is_string) => {
                Some(names.iter().filter_map(Value::as_str).map(str::to_string).collect())
            }
            Some(_) => return Response::error_json(400, "disabled must be an array of tool-name strings"),
        };
        // Validate the complete selection before changing either field.
        if let Some(mode) = mode {
            self.cfg.mcp.tools_mode = mode;
        }
        if let Some(disabled) = disabled {
            self.cfg.mcp.disabled_tools = disabled;
        }
        self.runtime.config_dirty = true;
        let queued = self.persist_config();
        let mut v = self.tools_portal_json();
        v["ok"] = json!(queued);
        v["applied"] = json!(true);
        v["queued"] = json!(queued);
        v["saved"] = json!(false);
        if queued {
            Response::json(&v)
        } else {
            v["error"] = json!("tool selection was applied in RAM but could not be queued for saving");
            Response::json_status(503, &v)
        }
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
        let _activity = self.run_gate.enter_tool(name, Some("agent"));
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
        self.llm.vision.hash(&mut h);
        self.tool_attachments().iter().any(|a| a.mime.starts_with("image/")).hash(&mut h);
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
        let before = self.workspace_admissions;
        let result = self.fs_or_net_call_inner(name, arguments);
        if self.workspace_admissions == before {
            return result;
        }
        match result {
            Ok(text) => {
                let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "text": text }));
                Ok(Self::workspace_admission_reply(value).to_string())
            }
            Err(mut error) => {
                const NOTE: &str = "Earlier workspace edits remain in RAM and were queued; queue admission is not storage acknowledgement. Check the store object in GET /api/status for outcomes.";
                if !error.contains(NOTE) {
                    error.push('\n');
                    error.push_str(NOTE);
                }
                Err(error)
            }
        }
    }

    fn fs_or_net_call_inner(&mut self, name: &str, arguments: &Value) -> std::result::Result<String, String> {
        self.ensure_enabled(name).map_err(|e| e.message)?;
        if !arguments.is_object() {
            return Err("tool arguments must be an object".to_string());
        }
        if name.starts_with("fs_") {
            let call = |ws: &mut AppWorkspace| {
                fs_tools::call(ws, name, arguments)
                    .ok_or_else(|| format!("`{name}` starts with fs_ but no such file tool exists"))?
            };
            let text = if matches!(name, "fs_write" | "fs_edit" | "fs_rm" | "fs_cp" | "fs_mv") {
                // Refused RAM edits/queue admission roll back here. A successful call must
                // report queued=true/saved=false, not imply that the writer has completed.
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
            // The HTTP client waits as long as the call has left, not as long as its own
            // default: one budget, and the client is one of the things that spends it.
            if let Some(left) = self.remaining_tool_ms() {
                if let Some(t) = transport.as_mut() {
                    t.set_timeout(std::time::Duration::from_millis(left.max(1)));
                }
            }
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
        // `time <command>`: the same command, plus how long it took. The prefix is taken
        // off by the parser, so what runs below is exactly what was typed after it.
        if pipeline.timed {
            let mut inner = pipeline.clone();
            inner.timed = false;
            let started = std::time::Instant::now();
            let text = match inner.single() {
                Some(parsed) => self.run_single(parsed, args)?,
                None => self.run_pipeline(&inner)?,
            };
            let ms = started.elapsed().as_millis() as u64;
            let answer = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({ "text": text }));
            return Ok(json!({
                "command": line.trim(),
                "ms": ms,
                "real_s": ms as f64 / 1000.0,
                "result": answer,
            })
            .to_string());
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
        self.ensure_enabled(&op).map_err(|e| e.message)?;
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
        self.ensure_enabled(tool).map_err(|e| e.message)?;
        match tool {
            "shell_zip" => self.shell_zip(inner),
            "shell_unzip" => self.shell_unzip(inner),
            "shell_curl" => self.run_curl("curl", inner),
            "shell_wget" => self.run_curl("wget", inner),
            "shell_ping" => self.run_ping(inner).map(|v| v.to_string()),
            "shell_ifconfig" => Ok(self.ifconfig_json().to_string()),
            // `sh <file>`: run a workspace script the way a shell runs a file, instead of
            // making the caller read it into `script {source: ...}` first.
            "shell_sh" => self.shell_sh(inner),
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
                self.ensure_enabled("fs_read").map_err(|e| e.message)?;
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
        self.ensure_enabled(op).map_err(|e| e.message)?;
        if let Some(tool) = crate::tools::shell::route(op) {
            crate::tool::ensure_tool_enabled(&self.cfg, tool).map_err(|e| e.message)?;
        }
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
        self.ensure_enabled("fs_write").map_err(|e| e.message)?;
        let existing = if target.append {
            self.ensure_enabled("fs_read").map_err(|e| e.message)?;
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
        self.ensure_enabled(name).map_err(|e| e.message)?;
        if name == "wget" {
            self.ensure_enabled("fs_write").map_err(|e| e.message)?;
        }
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
        let up = self.runtime.uptime_s(self.hw.millis());
        serde_json::json!({
            "uptime_s": up,
            "uptime": format!("{}h {:02}m {:02}s", up / 3600, (up % 3600) / 60, up % 60),
            "runs": self.agent.runs,
        })
        .to_string()
    }

    /// `date`: UTC and the board's local time.
    ///
    /// Reporting only UTC is what made a model compute "UTC+8" by hand in a reply and get
    /// the weekday from its own guess. The offset is a setting now, so the local reading is
    /// as trustworthy as the UTC one.
    fn shell_date(&mut self) -> String {
        let ms = now_ms();
        let offset_h = self.cfg.tz_offset_h;
        let local_s = (ms / 1000) as i64 + i64::from(offset_h) * 3_600;
        serde_json::json!({
            "unix_s": ms / 1000,
            "synced": ms > 1_700_000_000_000,
            "utc": utc_string(ms / 1000),
            "local": utc_string(local_s.max(0) as u64),
            "tz_offset_h": offset_h,
            "zone": if offset_h == 0 { "UTC".to_string() } else { format!("UTC{offset_h:+}") },
            "note": "before SNTP syncs these are time since boot, and https certificates read as not yet valid",
        })
        .to_string()
    }

    /// `sh <path> [args…]`: run a script file from the workspace.
    ///
    /// The `script` tool already took a `path`, but a person or a model reaches for a shell
    /// first, and `cat x.bh | …` cannot run one. Arguments become the script's `args`/`argv`,
    /// the same as `script {args: …}`.
    ///
    /// Refused inside a script: `tool("shell", {command: "sh …"})` would otherwise be a
    /// second way to recurse into the interpreter, and the inner script would draw on the
    /// outer one's time budget with nothing to stop it repeating.
    fn shell_sh(&mut self, args: &Value) -> std::result::Result<String, String> {
        if self.in_script {
            return Err(
                "a script may not run another script through `shell sh`; use `script {path: …}` instead"
                    .into(),
            );
        }
        let positional = args.get("args").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let path = positional
            .first()
            .and_then(|v| v.as_str())
            .or_else(|| args.get("path").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        if path.is_empty() {
            // The same line `shell help` prints, so the correction is in the error.
            let line = crate::tools::shell::usage("sh").unwrap_or("sh <path> [args…]");
            return Err(format!("usage: {line}"));
        }
        // Through the script tool, so the file is read, the args are wired and the budget
        // logic are exactly the ones `script {path: …}` uses.
        let rest: Vec<Value> = positional.iter().skip(1).cloned().collect();
        let mut call = json!({ "path": path });
        if !rest.is_empty() {
            call["args"] = Value::Array(rest);
        }
        let job = self.submit_script_call(&call)?;
        Ok(serde_json::json!({
            "path": path,
            "state": job.state,
            "result": job.result,
            "output": job.output,
            "steps": job.steps,
            "ms": job.ms,
            "error": if job.error.is_empty() { Value::Null } else { Value::String(job.error) },
        })
        .to_string())
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
        // Keep a compact roughly 1.8 KiB dump, independent of the global tool-text cap.
        // Octal/hex columns expand each input byte; report what this local policy omits.
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
            self.ensure_enabled("fs_write").map_err(|e| e.message)?;
            self.workspace_edit(|ws| fs_tools::Workspace::write(ws, &target, &decoded))?;
            return Ok(serde_json::json!({ "path": target, "bytes": bytes.len() }).to_string());
        }
        let encoded = crate::util::b64_encode(text.as_bytes());
        if let Some(target) = out_path {
            self.ensure_enabled("fs_write").map_err(|e| e.message)?;
            self.workspace_edit(|ws| fs_tools::Workspace::write(ws, &target, &encoded))?;
            return Ok(serde_json::json!({ "path": target, "bytes": encoded.len() }).to_string());
        }
        Ok(serde_json::json!({ "path": path, "base64": encoded }).to_string())
    }

    /// `ifconfig`: what the board thinks its network is, including the overrides.
    ///
    /// Measured first, configured second. The interface knows the netmask, gateway and
    /// resolvers it was given by DHCP; the config only knows what somebody typed. Reporting
    /// the config alone printed an empty netmask and gateway on a working DHCP board, and a
    /// second DNS slot that was `""` — three fields that looked broken and were merely
    /// never asked.
    fn ifconfig_json(&mut self) -> Value {
        let wifi = self.hw.wifi_status();
        let info = self.hw.board_info();
        let overrides = !self.cfg.wifi.gateway.trim().is_empty()
            || !self.cfg.wifi.dns.trim().is_empty()
            || !self.cfg.wifi.netmask.trim().is_empty();
        let gateway =
            if wifi.gateway.is_empty() { self.cfg.wifi.gateway.clone() } else { wifi.gateway.clone() };
        let netmask =
            if wifi.netmask.is_empty() { self.cfg.wifi.netmask.clone() } else { wifi.netmask.clone() };
        // Only real addresses. An empty slot is not a resolver, and printing it as one
        // invites a "fix the DNS" that has nothing to fix.
        let mut dns: Vec<String> = wifi.dns.iter().filter(|s| !s.is_empty()).cloned().collect();
        if dns.is_empty() {
            dns = [self.cfg.wifi.dns.clone(), self.cfg.wifi.dns2.clone()]
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .collect();
        }
        json!({
            "interface": "wlan0",
            "state": wifi.state,
            "ip": wifi.ip,
            "netmask": netmask,
            "gateway": gateway,
            "dns": dns,
            "ssid": wifi.ssid,
            "rssi_dbm": wifi.rssi_dbm,
            "channel": wifi.channel,
            "mac": if wifi.mac.is_empty() { info.mac } else { wifi.mac },
            "hostname": self.cfg.wifi.hostname,
            "ap": { "ssid": wifi.ap_ssid, "ip": wifi.ap_ip },
            "lease_or_override": if overrides { "override" } else { "dhcp lease" },
            "note": "rssi 0 and channel 0 mean not associated, not a measurement",
        })
    }

    /// Pack `{name: text}` entries into a compact inline archive and return base64.
    ///
    /// The 1,400-byte archive policy is separate from the workspace and tool-result limits.
    /// Check stored ZIP overhead before cloning any file; a large workspace export must
    /// be refused without first allocating an equally large archive and input copy.
    fn shell_zip(&mut self, args: &Value) -> std::result::Result<String, String> {
        const INLINE_MAX: usize = 1400;
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("archive.zip");
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        let mut encoded_bytes = 22usize;
        let mut append = |path: &str, text: &str| -> std::result::Result<(), String> {
            let next = encoded_bytes
                .saturating_add(76)
                .saturating_add(path.len().saturating_mul(2))
                .saturating_add(text.len());
            if next > INLINE_MAX {
                return Err(format!("the archive would be {next} bytes; zip's inline archive limit is {INLINE_MAX} bytes before base64"));
            }
            entries.push((path.to_string(), text.as_bytes().to_vec()));
            encoded_bytes = next;
            Ok(())
        };
        if let Some(files) = args.get("files").and_then(|v| v.as_object()) {
            for (path, text) in files {
                append(path, text.as_str().unwrap_or(""))?;
            }
        }
        // Borrow the existing text until admission succeeds; Workspace::read would
        // clone the entire file even when its first entry already exceeds INLINE_MAX.
        if let Some(paths) = args.get("paths").and_then(|v| v.as_array()) {
            for path in paths.iter().filter_map(|p| p.as_str()) {
                self.ensure_enabled("fs_read").map_err(|e| e.message)?;
                let canonical = AppWorkspace::file_path(path)?;
                let text = self
                    .workspace
                    .files
                    .get(&canonical)
                    .ok_or_else(|| format!("`{path}` is not in the workspace"))?;
                append(canonical.trim_start_matches('/'), text)?;
            }
        }
        if entries.is_empty() {
            return Err("nothing to pack: give `files` {name: text} or `paths` [..]".into());
        }
        let archive = crate::zip::write(&entries)?;
        debug_assert_eq!(archive.len(), encoded_bytes);
        // No `stored`: nothing is written anywhere. `zip` hands the archive back as base64 and
        // the field said otherwise, which is a claim a caller could act on and be wrong.
        Ok(json!({
            "name": name,
            "bytes": archive.len(),
            "entries": entries.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
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
            self.ensure_enabled("fs_read").map_err(|e| e.message)?;
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
        self.ensure_enabled("fs_write").map_err(|e| e.message)?;
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
        self.ensure_enabled("fs_write").map_err(|e| e.message)?;
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

    /// `sleep <s>` / `usleep <ms>`: a real wait, bounded by what the call has left.
    ///
    /// There is no separate sleep cap any more. `agent.tool_timeout_s` is the one budget a
    /// call has, so a wait asks how much of it is left instead of using a constant that
    /// disagrees with it. With the deadline switched off (`tool_timeout_s: 0`) the old
    /// ten-second cap comes back, because nothing else would bound a bare `sleep 1e9`.
    fn shell_sleep(&mut self, args: &Value) -> std::result::Result<String, String> {
        let ms_unit = args.get("unit").and_then(|v| v.as_str()) == Some("ms");
        let op = if ms_unit { "usleep" } else { "sleep" };
        let raw = args.get("value").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let budget = self.remaining_tool_ms();
        let cap = budget.unwrap_or(MAX_SHELL_SLEEP_MS);
        let (wanted, _) = plan_sleep(ms_unit, &raw, cap).map_err(|e| format!("{op}: {e}"))?;
        let source = if budget.is_some() {
            "agent.tool_timeout_s"
        } else {
            "10 s fallback (agent.tool_timeout_s is 0)"
        };
        // Refused, not shortened: a caller that asked for 20 s and waited 3 would draw the
        // wrong conclusion about whatever it was timing.
        if wanted > cap {
            return Err(format!(
                "{op}: {wanted} ms is more than the {cap} ms left of this tool call ({source}); \
                 ask for less, split the work, or raise the budget"
            ));
        }
        let end = Instant::now() + Duration::from_millis(wanted);
        while Instant::now() < end {
            if self.run_gate.cancelled() {
                return Err(format!("{op}: interrupted by the user"));
            }
            if self.remaining_tool_ms() == Some(0) {
                return Err(format!("{op}: tool deadline exceeded while waiting"));
            }
            std::thread::sleep(end.saturating_duration_since(Instant::now()).min(Duration::from_millis(25)));
        }
        Ok(json!({
            "slept_ms": wanted,
            "requested_ms": wanted,
            "budget_ms": cap,
            "budget": source,
            "clamped": false,
            "note": "waits are bounded by agent.tool_timeout_s and check interruption every 25 ms",
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
        self.ensure_enabled("fs_read").map_err(|e| e.message)?;
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
                if !self.persist_config() {
                    self.cfg.wifi.hostname = previous;
                    return Err("hostname: config could not be queued for saving; name unchanged".into());
                }
                Ok(json!({
                    "hostname": name,
                    "previous": previous,
                    "saved": false,
                    "queued": true,
                    "note": "applied in RAM and queued, not acknowledged on flash; DHCP carries the new name after reconnect. Check /api/status.store for failures",
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

/// `sleep`/`usleep`: how long was asked for, and how much of `cap_ms` that may use.
///
/// Split out of the handler so the arithmetic can be tested without a test that takes ten
/// seconds: the waiting is the one thing a test should not do to prove arithmetic.
fn plan_sleep(ms_unit: bool, raw: &str, cap_ms: u64) -> std::result::Result<(u64, u64), String> {
    let value: f64 = raw.parse().map_err(|_| format!("`{raw}` is not a number"))?;
    if !value.is_finite() {
        return Err(format!("`{raw}` is not a number"));
    }
    if value < 0.0 {
        return Err("a negative wait is not a wait".to_string());
    }
    let wanted = (if ms_unit { value } else { value * 1000.0 }).round() as u64;
    Ok((wanted, wanted.min(cap_ms)))
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
        "after_s": t.after_s,
        "every_s": t.every_s,
        "at_hhmm": t.at_hhmm,
        "enabled": t.enabled,
        "last_run_ms": t.last_run_ms,
    })
}

fn next_schedule_id<'a>(prefix: &str, ids: impl Iterator<Item = &'a str>) -> String {
    let ids: std::collections::BTreeSet<_> = ids.collect();
    // There are at most N occupied names, so one of N+1 candidates must be free.
    (1..=ids.len() + 1)
        .map(|n| format!("{prefix}{n}"))
        .find(|id| !ids.contains(id.as_str()))
        .expect("one more candidate than occupied names")
}

pub(crate) fn next_schedule_task_id(tasks: &[ScheduledTask]) -> String {
    next_schedule_id("t", tasks.iter().map(|t| t.id.as_str()))
}

fn schedule_string_fields(v: &Value, names: &[&str]) -> Result<()> {
    if !v.is_object() {
        return Err(McpError::invalid_params("schedule entries must be objects"));
    }
    for name in names {
        if v.get(*name).is_some_and(|raw| !raw.is_string()) {
            return Err(McpError::invalid_params(format!("`{name}` must be a string")));
        }
    }
    if v.get("enabled").is_some_and(|raw| !raw.is_boolean()) {
        return Err(McpError::invalid_params("`enabled` must be a boolean"));
    }
    Ok(())
}

/// Strict edge validation shared by REST and the schedule tool. Stored schedule
/// blobs keep the lenient reader below, so this does not change recovery semantics.
pub(crate) fn task_from_api(v: &Value, fallback_id: &str) -> Result<ScheduledTask> {
    schedule_string_fields(v, &["id", "prompt", "script", "at_hhmm"])?;
    let every_s = crate::json::optional_ranged_i64(v, "every_s", 0, 86_400)?.unwrap_or(0) as u32;
    let after_s = crate::json::optional_ranged_i64(v, "after_s", 0, 86_400)?.unwrap_or(0) as u32;
    crate::json::optional_ranged_u64(v, "last_run_ms", 0, u64::MAX)?;
    let mut task = task_from(v, fallback_id)
        .ok_or_else(|| McpError::invalid_params("a task requires `prompt` or `script`"))?;
    task.every_s = every_s;
    task.after_s = after_s;
    if let Some(why) = task_problem(&task) {
        return Err(McpError::invalid_params(why));
    }
    Ok(task)
}

fn rule_from_api(v: &Value, fallback_id: &str) -> Result<SensorRule> {
    schedule_string_fields(v, &["id", "tool", "path", "op", "prompt"])?;
    crate::json::optional_ranged_u64(v, "min_interval_s", 0, u32::MAX as u64)?;
    crate::json::optional_ranged_u64(v, "last_fire_ms", 0, u64::MAX)?;
    let rule = rule_from_id(v, fallback_id).ok_or_else(|| McpError::invalid_params("`tool` is required"))?;
    if let Some(why) = rule_problem(&rule) {
        return Err(McpError::invalid_params(why));
    }
    Ok(rule)
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
        after_s: v.get("after_s").and_then(|a| a.as_u64()).unwrap_or(0).min(u32::MAX as u64) as u32,
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
    if t.after_s > 0 {
        // "in ten seconds" and "every ten seconds" are different requests; picking one
        // silently is how a one-shot turns into a task that fires every ten seconds forever.
        if t.every_s > 0 || !t.at_hhmm.is_empty() {
            return Some(
                "a one-shot task takes `after_s` alone: drop `every_s` and `at_hhmm` for a task that fires once"
                    .to_string(),
            );
        }
        return None;
    }
    if t.every_s == 0 && t.at_hhmm.is_empty() {
        return Some("a task needs `after_s` > 0, `every_s` > 0 or `at_hhmm` (\"07:30\")".to_string());
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
    if h.len() != 2
        || m.len() != 2
        || !h.bytes().all(|c| c.is_ascii_digit())
        || !m.bytes().all(|c| c.is_ascii_digit())
    {
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

#[path = "workspace_json.rs"]
mod workspace_json;

/// Metadata has its own limits: empty files and long names still occupy RAM.
pub const WORKSPACE_FILES_MAX: usize = 1024;
pub const WORKSPACE_PATH_MAX: usize = 256;
pub const WORKSPACE_NAMES_MAX: usize = 128 * 1024;
/// Permit a little redundant path syntax without accepting an unbounded raw name.
const WORKSPACE_RAW_PATH_MAX: usize = 4 * WORKSPACE_PATH_MAX;
/// Conservative per-map-entry allowance for tree-node slack and allocator bookkeeping.
const WORKSPACE_ENTRY_RAM: usize = 192;

/// The agent's bounded in-RAM file map, persisted as an index and one value per file.
///
/// Text limits remain 256 KiB per file and 4 MiB in total. Logical path and entry
/// budgets are separate from the short physical keys; see `docs/storage.md`.
#[derive(Default, Debug, Clone)]
pub struct AppWorkspace {
    files: std::collections::BTreeMap<String, String>,
    /// Per-path content hashes maintained by App's incremental persistence path.
    /// This map alone does not establish that a queued write reached storage.
    saved: std::collections::BTreeMap<String, u64>,
    /// A refused restore must not be replaced by boot-time seeding or a partial edit.
    /// Repairing storage (or an explicit reset) makes the next load writable again.
    load_refused: Option<String>,
}

/// Whether a change of `workspace` can be made with `free` bytes of PSRAM.
///
/// Two copies: the rollback keeps the map as it was, and the change builds the new text. The
/// margin is the allocator's overhead and whatever else a turn is holding. The point of asking
/// is that the failure has two shapes and only one of them is usable: a refusal here says
/// "nothing was changed", while an allocation that fails prints `memory allocation of N bytes
/// failed` and calls `abort()` - measured on this board, at a 1.25 MB workspace.
pub fn workspace_fits(workspace: &AppWorkspace, free: Option<usize>) -> std::result::Result<(), String> {
    workspace.writable()?;
    let Some(free) = free else { return Ok(()) };
    let bytes = workspace.ram_bytes();
    let need = bytes.saturating_mul(2).saturating_add(64 * 1024);
    if free >= need {
        return Ok(());
    }
    Err(format!(
        "the file workspace uses an estimated {bytes} bytes of RAM and a change needs about {need} bytes of PSRAM \
         (a rollback copy, file text, names and entry overhead); {free} is free, so nothing was changed"
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

    /// Parse version-1 JSON incrementally, with bounded encoded input and decoded strings.
    ///
    /// No intermediate `Value`, full-document `String`, or unbounded parser string buffer:
    /// each path is checked before its content, and each decoded byte before allocation.
    /// Legacy documents that omit `version` or `files` retain their version-1 meaning.
    pub fn from_reader(reader: impl std::io::Read) -> std::result::Result<Self, serde_json::Error> {
        workspace_json::document(reader)
    }

    pub fn to_json(&self) -> Value {
        let mut files = serde_json::Map::new();
        for (path, text) in &self.files {
            files.insert(path.clone(), json!(text));
        }
        json!({ "version": 1, "files": files })
    }

    /// The already-materialized legacy form. Validate before copying any entry.
    /// The caller's `Value` allocation is not owned by this function.
    pub fn from_json(v: &Value) -> std::result::Result<Self, String> {
        let doc = v.as_object().ok_or("workspace document must be an object")?;
        if doc.keys().any(|field| field != "version" && field != "files") {
            return Err("unknown workspace field".to_string());
        }
        if doc.get("version").is_some_and(|version| version.as_u64() != Some(1)) {
            return Err("workspace version must be the integer 1".to_string());
        }
        let mut ws = Self::default();
        if let Some(files) = doc.get("files") {
            let map = files.as_object().ok_or("workspace files must be an object")?;
            for (path, text) in map {
                let text = text.as_str().ok_or("workspace file content must be a string")?;
                ws.insert_checked(path, std::borrow::Cow::Borrowed(text), false)?;
            }
        }
        Ok(ws)
    }

    fn file_path(path: &str) -> std::result::Result<String, String> {
        let path = normalize_path(path)?;
        if path == "/" {
            return Err("the workspace root is a directory, not a file".to_string());
        }
        Ok(path)
    }

    /// Shared admission for normal writes and both restoration formats.
    /// A restore cannot silently overwrite a canonical-name collision.
    fn check_insert(&self, path: &str, bytes: usize, overwrite: bool) -> std::result::Result<String, String> {
        self.writable()?;
        let key = Self::file_path(path)?;
        let previous = self.files.get(&key);
        if !overwrite && previous.is_some() {
            return Err(format!("duplicate canonical workspace path `{key}`"));
        }
        if previous.is_none() {
            let physical = crate::keys::workspace_file(&key);
            if self.files.keys().any(|existing| {
                Self::path_conflicts(existing, &key) || crate::keys::workspace_file(existing) == physical
            }) {
                return Err(format!("workspace path `{key}` collides with an existing file or directory"));
            }
            if self.files.len() >= WORKSPACE_FILES_MAX {
                return Err(format!("the workspace holds at most {WORKSPACE_FILES_MAX} files"));
            }
            let names = self.files.keys().map(|path| path.len()).sum::<usize>();
            if !matches!(names.checked_add(key.len()), Some(n) if n <= WORKSPACE_NAMES_MAX) {
                return Err(format!("workspace filenames hold at most {WORKSPACE_NAMES_MAX} bytes"));
            }
        }
        if bytes > WORKSPACE_FILE_MAX {
            return Err(format!(
                "`{key}` is {bytes} bytes; the limit is {WORKSPACE_FILE_MAX} bytes per file"
            ));
        }
        let others = self.bytes().saturating_sub(previous.map_or(0, String::len));
        if !matches!(others.checked_add(bytes), Some(n) if n <= WORKSPACE_TOTAL_MAX) {
            return Err(format!(
                "the workspace holds at most {WORKSPACE_TOTAL_MAX} bytes; remove a file first"
            ));
        }
        Ok(key)
    }

    fn path_conflicts(left: &str, right: &str) -> bool {
        left == right
            || left.strip_prefix(right).is_some_and(|rest| rest.starts_with('/'))
            || right.strip_prefix(left).is_some_and(|rest| rest.starts_with('/'))
    }

    fn insert_checked(
        &mut self,
        path: &str,
        text: std::borrow::Cow<'_, str>,
        overwrite: bool,
    ) -> std::result::Result<(), String> {
        let key = self.check_insert(path, text.len(), overwrite)?;
        let text = match text {
            std::borrow::Cow::Owned(text) => text,
            std::borrow::Cow::Borrowed(text) => {
                let mut copy = String::new();
                copy.try_reserve_exact(text.len()).map_err(|_| "workspace text allocation was refused")?;
                copy.push_str(text);
                copy
            }
        };
        self.files.insert(key, text);
        Ok(())
    }

    fn writable(&self) -> std::result::Result<(), String> {
        match &self.load_refused {
            Some(why) => Err(format!("the stored workspace was not loaded ({why}); writes are blocked to preserve it; repair or explicitly reset storage")),
            None => Ok(()),
        }
    }

    fn refuse_load(&mut self, why: impl Into<String>) {
        self.load_refused = Some(why.into());
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
        self.files.values().fold(0usize, |total, text| total.saturating_add(text.len()))
    }

    /// Allocation-capacity estimate, not a measured heap size. Include both maps'
    /// strings and a generous allowance per tree entry; text-only `bytes()` stays unchanged.
    pub fn ram_bytes(&self) -> usize {
        let files = self.files.iter().fold(0usize, |total, (path, text)| {
            total
                .saturating_add(path.capacity())
                .saturating_add(text.capacity())
                .saturating_add(WORKSPACE_ENTRY_RAM)
        });
        let saved = self.saved.keys().fold(0usize, |total, path| {
            total.saturating_add(path.capacity()).saturating_add(WORKSPACE_ENTRY_RAM)
        });
        files.saturating_add(saved).saturating_add(self.load_refused.as_ref().map_or(0, String::capacity))
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// `/a/b` from `a//b/`, `/a/./b`; `..` is refused rather than resolved, so a tool
/// call can never climb out of the workspace.
fn normalize_path(path: &str) -> std::result::Result<String, String> {
    // Check before normalization allocates a vector or a canonical path.
    if path.len() > WORKSPACE_RAW_PATH_MAX {
        return Err(format!("a raw workspace path holds at most {WORKSPACE_RAW_PATH_MAX} bytes"));
    }
    let path = fs_tools::normalize(path)?;
    if path.len() > WORKSPACE_PATH_MAX {
        return Err(format!("a canonical workspace path holds at most {WORKSPACE_PATH_MAX} bytes"));
    }
    Ok(path)
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
        let key = Self::file_path(path)?;
        self.files.get(&key).cloned().ok_or_else(|| format!("no file `{key}`"))
    }

    fn write(&mut self, path: &str, text: &str) -> std::result::Result<(), String> {
        self.insert_checked(path, std::borrow::Cow::Borrowed(text), true)
    }

    fn remove(&mut self, path: &str) -> std::result::Result<(), String> {
        self.writable()?;
        let key = Self::file_path(path)?;
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
    // schema, add a tool that was not there before, then call it. Six rounds also
    // verify that the next request receives the freshly published tool declaration.
    const INVENTORY: (&str, &str) = ("inventory", "{}");
    const I2C_SCAN: (&str, &str) = ("i2c_scan", "{}");
    const DESCRIBE: (&str, &str) = ("manifest_describe", "{}");
    const APPLY: (&str, &str) = (
        "manifest_apply",
        r#"{"mode":"merge","manifest":{"tools":[{"name":"bench_mcp_probe","description":"Read the boot button on GPIO0 as a level, 1 released and 0 pressed","params":[],"exec":{"type":"gpio_read","pin":0}}]}}"#,
    );
    const CALL_NEW: (&str, &str) = ("bench_mcp_probe", "{}");
    // Keep the scripted network case local too: it should not depend on public DNS
    // or Internet access merely to exercise the transport and tool result path.
    let local_url = format!("{}://{}/selftest/ping", req.scheme, req.header("host").unwrap_or("127.0.0.1"));
    let net_args = json!({"url": local_url}).to_string();
    let steps: &[(&str, &str)] = if has_word(&ask, "concurrency") {
        &[("script", r#"{"source":"sleep(20000)\n42"}"#)]
    } else if has_word(&ask, "edit") || ask.contains("改") {
        &[WRITE, EDIT, READ]
    } else if has_word(&ask, "file") || ask.contains("文件") {
        &[WRITE, READ, GREP]
    } else if has_word(&ask, "mcp") {
        &[INVENTORY, I2C_SCAN, DESCRIBE, APPLY, CALL_NEW, ("board_status", "{}")]
    } else if has_word(&ask, "net") || has_word(&ask, "http") {
        &[("net_get", &net_args)]
    } else {
        &[("board_status", "{}")]
    };
    let desired = steps.get(tool_rounds).copied();
    // The default profile advertises families and read/write/edit/shell, not the
    // old aliases. Choose only a name in the actual request; legacy-only fixtures
    // still work, and a disabled family is not bypassed via an unadvertised alias.
    let next_tool = desired.and_then(|(name, args)| {
        if has_tool(name) {
            return Some((name.to_string(), args.to_string()));
        }
        let (family, command) = match name {
            "board_status" => ("board", "status"),
            "inventory" => ("board", "inventory"),
            "i2c_scan" => ("bus", "i2c_scan"),
            "manifest_describe" => ("manifest", "describe"),
            "manifest_apply" => ("manifest", "apply"),
            "fs_write" | "fs_read" | "fs_edit" => {
                let current = match name {
                    "fs_write" => "write",
                    "fs_read" => "read",
                    _ => "edit",
                };
                return has_tool(current).then(|| (current.to_string(), args.to_string()));
            }
            "fs_grep" | "net_get" if has_tool("shell") => {
                let command = if name == "fs_grep" {
                    "grep written /bench".to_string()
                } else {
                    format!("curl {local_url}")
                };
                return Some(("shell".to_string(), json!({"command": command}).to_string()));
            }
            _ => return None,
        };
        let arguments = serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({}));
        has_tool(family)
            .then(|| (family.to_string(), json!({"command": command, "arguments": arguments}).to_string()))
    });

    let message = match (next_tool.as_ref(), desired, tool_result) {
        (Some((name, args)), _, _) => json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": format!("call_selftest_{tool_rounds}"),
                "type": "function",
                "function": { "name": name, "arguments": args }
            }]
        }),
        (None, Some((name, _)), _) => json!({
            "role": "assistant",
            "content": format!("selftest: `{name}` is not on the tool list, so there is nothing to demonstrate"),
        }),
        (None, None, Some(text)) => json!({
            "role": "assistant",
            "content": format!("selftest: the board answered with {}", body_prefix(&text, 160)),
        }),
        (None, None, None) => json!({ "role": "assistant", "content": "selftest: nothing to do" }),
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
                // The boundary a cancel lands on: the last answer (if any) is in, and nobody
                // is waiting on a socket yet. A cancel that arrives during the call below is
                // taken here on the next pass.
                if app_lock(app).take_cancel() {
                    step = app_lock(app)
                        .agent_reply(Err("interrupted by the user; the turn was stopped".into()));
                    continue;
                }
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
    // Called by the platform after WiFi startup. No publication in App::new,
    // store acknowledgment, tool recursion, or the 6 KB store-writer task.
    let (views, gate) = {
        let mut state = app_lock(&app);
        state.sync_run_gate();
        state.publish_status_view();
        (state.status_views.clone(), state.run_gate.clone())
    };
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
        if req.method == "GET" && matches!(req.path.as_str(), "/api/status" | "/healthz") {
            gate.note_request();
            if let Some(response) = gate.mcp_get(req) {
                return response;
            }
            return match views.current() {
                Some(view) => view.response(
                    req.path == "/healthz",
                    gate.active(),
                    gate.sessions(),
                    gate.tool_activity(),
                ),
                None => Response::error_json(503, "status has not been sampled yet"),
            };
        }
        if let Some(response) = gate.cancel_route(req, now_ms()) {
            return response;
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
/// Unlike `speaker_play`, which synthesizes the whole bounded score, this route accepts
/// PCM without retaining the complete clip or putting its samples into a tool envelope.
/// [`crate::http::BodySink`] reuses small chunk buffers; the upload has its own 30 s/byte
/// limits, and the speaker's pacing throttles the socket.
pub const AUDIO_STREAM_PATH: &str = "/audio/pcm";

/// How much audio one upload may carry: 30 s at 24 kHz, 16-bit mono.
///
/// Not a full-clip memory budget — only chunk buffers are held — but a bound on how long
/// one request may hold the speaker and how long a sending client can keep it. Longer audio
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
    if content_type.split(';').next().unwrap_or("").trim() != "audio/pcm" {
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
                if self.tag == b'E' && self.need != 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "an end record must have length 0",
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
    invalid: Option<String>,
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
            invalid: None,
        }
    }
}

impl crate::http::BodySink for AvSink {
    fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.received + bytes.len() > AV_MAX_BODY_BYTES {
            let message = format!("more than {AV_MAX_BODY_BYTES} bytes in one body");
            self.invalid = Some(message.clone());
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, message));
        }
        self.received += bytes.len();
        // Split the borrow: the reader needs `&mut self.reader` while the callback writes
        // to the other fields.
        let shared = Arc::clone(&self.app);
        let AvSink { reader, audio, audio_bytes, samples, frames, failed, invalid, .. } = self;
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
                        .call_tool("display", &json!({"command": "oled_bitmap", "arguments": { "data_b64": b64, "width": 128, "height": height }}))
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
        let result = reader.feed(bytes, &mut on);
        if let Err(err) = &result {
            if failed.is_none() {
                *invalid = Some(err.to_string());
            }
        }
        result
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
        if let Some(error) = self.invalid {
            body["ok"] = json!(false);
            body["error"] = json!(error);
            return Response::json_status(400, &body);
        }
        if !complete || !self.reader.head.is_empty() || self.reader.need != 0 {
            body["ok"] = json!(false);
            body["error"] = json!("the upload or its final AV record ended early: the rest was not played");
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
    if !matches!(
        content_type.split(';').next().unwrap_or("").trim(),
        "application/x-esp32-av" | "application/octet-stream"
    ) {
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

    // Read/control paths must work even while a tool owns the App mutex.
    #[test]
    fn cached_status_does_not_acquire_the_tool_mutex() {
        let shared: Shared = Arc::new(Mutex::new(test_app()));
        let handler = make_handler(shared.clone());
        let held = app_lock(&shared);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send((handler(&get("/api/status")), handler(&get("/healthz")))).unwrap();
        });
        let answer = rx.recv_timeout(Duration::from_secs(1));
        drop(held);
        worker.join().unwrap();
        let (status, health) = answer.expect("cached reads waited for App");
        assert_eq!(status.status, 200);
        assert_eq!(health.status, 200);
        assert_eq!(body_json(&status)["snapshot"], true);
        assert_eq!(body_json(&health)["ok"], true);
    }

    #[test]
    fn status_publication_is_explicit_and_not_on_the_storage_writer() {
        let mut app = test_app();
        assert!(app.status_views.current().is_none(), "constructor must not build JSON");
        app.attach_store_writer("test-writer", true);
        assert!(app.status_views.current().is_none(), "writer must not build status JSON");
        app.publish_status_view();
        let old = app.status_views.current().unwrap();
        app.cfg.device_name = "published-later".into();
        assert_ne!(old.status["device_name"], "published-later");
        app.publish_status_view();
        assert_eq!(app.status_views.current().unwrap().status["device_name"], "published-later");
        assert_ne!(old.status["device_name"], "published-later", "old Arc was mutated");
    }

    #[test]
    fn cached_uptime_advances_without_a_hardware_read() {
        let view = crate::status_view::StatusView {
            status: json!({"devices":{"count":5}}),
            boot_ms: 100,
            sampled_ms: 1099,
            sampled_at: Instant::now() - Duration::from_millis(10),
        };
        assert_eq!(body_json(&view.response(true, false, 0, None))["uptime_s"], 1);
        assert_eq!(body_json(&view.response(false, false, 0, None))["uptime_s"], 1);
    }

    #[test]
    fn independent_cancel_preserves_pin_and_token_authentication() {
        let mut app = test_app();
        app.cfg.auth.portal_pin = "1234".into();
        app.runtime.auth_tokens = vec![
            ("valid-test-token".into(), now_ms() + 60_000),
            ("expired-test-token".into(), now_ms().saturating_sub(1)),
        ];
        app.run_gate.set_active(true);
        let shared: Shared = Arc::new(Mutex::new(app));
        let handler = make_handler(shared.clone());
        let held = app_lock(&shared);
        let plain = post("/api/agent/cancel", "{}");
        assert_eq!(handler(&plain).status, 401);
        let mut req = plain.clone();
        req.query.insert("token".into(), "valid-test-token".into());
        assert_eq!(handler(&req).status, 401, "POST cannot use URL token");
        req = plain.clone();
        req.headers.insert("authorization".into(), "Bearer expired-test-token".into());
        assert_eq!(handler(&req).status, 401);
        req.headers.insert("authorization".into(), "Bearer valid-test-token".into());
        assert_eq!(handler(&req).status, 200);
        assert!(held.run_gate.cancelled());
        held.run_gate.set_cancelled(false);
        req = plain;
        req.headers.insert("x-portal-pin".into(), "1234".into());
        assert_eq!(handler(&req).status, 200);
        assert!(held.run_gate.cancelled());
    }

    #[test]
    fn run_gate_policy_updates_without_waiting_for_full_status_publication() {
        let shared: Shared = Arc::new(Mutex::new(test_app()));
        let handler = make_handler(shared.clone());
        {
            let mut app = app_lock(&shared);
            app.cfg.auth.portal_pin = "new-pin".into();
            app.cfg.mcp.path = "/healthz".into();
            app.sync_run_gate();
        }
        let held = app_lock(&shared);
        assert_eq!(handler(&post("/api/agent/cancel", "{}")).status, 401);
        assert_eq!(handler(&get("/healthz")).status, 405);
        assert_eq!(handler(&get("/api/status")).status, 200);
        drop(held);
    }

    #[test]
    fn clip_advertisement_matches_detected_recording_and_playback_targets() {
        let mut app = test_app();
        assert!(app.caps.contains("oled"));
        assert!(!app.caps.contains("mic"));
        assert!(app.extra_why_hidden("clip_play").is_none());
        assert!(app.extra_why_hidden("clip_record").is_some());
        app.caps = CapSet::new();
        assert!(app.extra_why_hidden("clip_play").is_some());
        app.caps.insert("mic");
        assert!(app.extra_why_hidden("clip_record").is_none());
        assert!(app.extra_why_hidden("clip_play").is_some());
        app.caps.insert("speaker");
        assert!(app.extra_why_hidden("clip_play").is_none());
    }

    #[test]
    fn manual_script_schedule_returns_its_finished_result_and_failures() {
        let mut app = test_app();
        app.tasks.push(ScheduledTask {
            id: "result-probe".into(),
            script: "print(\"scheduled\")\n1 + 2".into(),
            ..Default::default()
        });
        let done = app.sched_run_now("result-probe").unwrap();
        assert_eq!(done["state"], "done");
        assert_eq!(done["result"], 3);
        assert_eq!(done["output"], json!(["scheduled"]));
        assert!(done["steps"].as_u64().unwrap() > 0);
        app.tasks[0].script = "unknown_function()".into();
        let failed = app.sched_run_now("result-probe").unwrap();
        assert_eq!(failed["state"], "failed");
        assert!(failed["result"].is_null());
        assert!(failed["error"].as_str().unwrap().contains("unknown"));
    }

    #[test]
    fn long_script_and_shell_sleeps_can_be_interrupted_while_app_is_locked() {
        for (name, args) in
            [("shell", json!({"command":"sleep 10"})), ("script", json!({"source":"sleep(10000)\n1"}))]
        {
            let app = test_app();
            app.run_gate.set_active(true);
            let shared: Shared = Arc::new(Mutex::new(app));
            let handler = make_handler(shared.clone());
            let running = shared.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut app = app_lock(&running);
                tx.send(()).unwrap();
                app.call_tool(name, &args)
            });
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            let started = Instant::now();
            let cancel = handler(&post("/api/agent/cancel", "{}"));
            assert_eq!(body_json(&cancel)["cancelling"], true);
            let error = worker.join().unwrap().unwrap_err();
            assert!(error.message.contains("interrupted"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    #[test]
    fn concurrency_selftest_requests_a_real_interruptible_script() {
        let app = test_app();
        let request = json!({"messages":[{"role":"user","content":"concurrency regression"}],
            "tools": ToolHost::tool_specs(&app)});
        let reply = selftest_completion(&post("/selftest/chat/completions", &request.to_string()));
        let function = &reply["choices"][0]["message"]["tool_calls"][0]["function"];
        assert_eq!(function["name"], "script");
        let arguments: Value = serde_json::from_str(function["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["source"], "sleep(20000)\n42");
    }

    #[test]
    fn cancellation_still_refuses_tools_when_deadlines_are_disabled() {
        let mut app = test_app();
        app.cfg.agent.tool_timeout_s = 0;
        app.run_gate.set_cancelled(true);
        assert!(app
            .call_tool("shell", &json!({"command":"echo must-not-run"}))
            .unwrap_err()
            .message
            .contains("interrupted"));
        app.run_gate.set_active(false);
        assert!(!app.run_gate.cancelled(), "ending a turn must clear cancellation");
        app.run_gate.set_active(true);
        assert!(!app.run_gate.cancelled(), "next turn must not inherit cancellation");
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

    /// A MemoryStore two `App`s can share, so "reload" means "a new `App` over the same keys",
    /// which is what a reboot looks like from the inside. The transcript feature is
    /// exactly the property that a new `App` and an old one disagree about, and a store
    /// per `App` could not express it.
    #[derive(Clone, Debug, Default)]
    struct SharedStore(
        Arc<Mutex<MemoryStore>>,
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
            self.0.lock().map(|m| m.keys()).unwrap_or_default()
        }
    }

    impl KvStore for SharedStore {
        fn get(&self, key: &str) -> Option<String> {
            self.0.lock().ok()?.get(key)
        }

        fn read_at(&self, key: &str, offset: usize, out: &mut [u8]) -> Option<usize> {
            self.0.lock().ok()?.read_at(key, offset, out)
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
                    self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    m.set(key, value)
                }
                Err(_) => false,
            }
        }

        fn remove(&mut self, key: &str) -> bool {
            match self.0.lock() {
                Ok(mut m) => {
                    self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    m.remove(key)
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
        assert_eq!(NVS_KEY_PENDING_MESSAGE, "transcript.q");
    }

    #[test]
    fn a_queued_message_is_saved_before_a_run_and_cleared_on_begin() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        drain_store(&mut app);
        let writes_before = store.writes();
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"   "}"#)).status, 400);
        assert!(app.store.get(NVS_KEY_PENDING_MESSAGE).is_none());
        assert_eq!(
            app.route(&post("/api/agent/message", r#"{"text":"  answer after the reboot  "}"#)).status,
            202
        );
        let pending = app.agent.pending.clone().expect("queued in RAM");
        let saved = app.store.get(NVS_KEY_PENDING_MESSAGE).expect("queued for the writer");
        let value: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(value, json!({ "text": pending.text, "source": "http", "at_ms": pending.at_ms }));
        assert_eq!(pending.text, "answer after the reboot");
        assert_eq!(store.writes(), writes_before, "enqueue must not write the backend itself");
        assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none(), "admission is not durability");
        drain_store(&mut app);
        assert_eq!(store.get(NVS_KEY_PENDING_MESSAGE).as_deref(), Some(saved.as_str()));
        let writes_before_begin = store.writes();

        assert!(matches!(app.agent_begin(), Some(AgentStep::Ask { .. })));
        assert!(app.agent.pending.is_none(), "begin consumed the question");
        assert!(
            app.store.get(NVS_KEY_PENDING_MESSAGE).is_none(),
            "the consumed copy is removed from the queue"
        );
        assert_eq!(app.agent.history.last().map(|m| m.text.as_str()), Some(pending.text.as_str()));
        assert_eq!(store.writes(), writes_before_begin, "begin must not write under the App lock");
        assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_some(), "only the writer removes the stored copy");
        drain_store(&mut app);
        assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none());
        assert!(store.get(NVS_KEY_TRANSCRIPT).is_some(), "the question moved into the transcript");
    }

    #[test]
    fn a_queued_message_survives_a_reboot_and_is_answered_once() {
        struct PendingClearedModel {
            store: store::QueuedStore,
            scripted: Scripted,
        }
        impl ChatTransport for PendingClearedModel {
            fn post_json(
                &mut self,
                url: &str,
                headers: &[(&str, &str)],
                body: &str,
            ) -> std::result::Result<String, String> {
                assert!(
                    self.store.get(NVS_KEY_PENDING_MESSAGE).is_none(),
                    "the stored queue copy must be cleared before the first model call"
                );
                self.scripted.post_json(url, headers, body)
            }
        }

        // Both consumers must have the same boundary, including the synchronous runner.
        for synchronous in [false, true] {
            let store = SharedStore::new();
            let mut before = app_with(&store);
            assert_eq!(
                before.route(&post("/api/agent/message", r#"{"text":"please answer me"}"#)).status,
                202
            );
            let queued = before.agent.pending.clone().unwrap();
            drain_store(&mut before);
            drop(before);

            let (app, log) = app_with_log(&store);
            let restored = app.agent.pending.as_ref().expect("the queued question survived the reboot");
            assert_eq!(restored.text, queued.text);
            assert_eq!(restored.source, queued.source);
            assert_eq!(restored.at_ms, queued.at_ms);
            assert_eq!(
                log.tail(80, 0)
                    .iter()
                    .filter(|l| l.level == "info" && l.message == "restored a queued user message from flash")
                    .count(),
                1
            );
            let mut model = PendingClearedModel {
                store: store::QueuedStore::new(app.store_queue()),
                scripted: Scripted::new(vec![Scripted::answer("answered after reboot")]),
            };
            let seen = model.scripted.seen.clone();
            let shared = Arc::new(Mutex::new(app));
            point_at_the_stub(&mut shared.lock().unwrap());
            if synchronous {
                let mut app = shared.lock().unwrap();
                app.attach_model_transport(Box::new(model));
                assert_eq!(app.agent_run_once(), Ok("answered after reboot".to_string()));
                assert_eq!(app.agent_run_once(), Ok(String::new()), "an idle tick must not run it twice");
            } else {
                assert_eq!(drive_agent(&shared, &mut model), Some(Ok("answered after reboot".to_string())));
                assert!(drive_agent(&shared, &mut model).is_none(), "an idle tick must not run it twice");
            }
            let requests = seen.lock().unwrap();
            assert_eq!(requests.len(), 1, "one queued question means one model call, not zero or two");
            let request: Value = serde_json::from_str(&requests[0]).unwrap();
            let users: Vec<&Value> =
                request["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "user").collect();
            assert_eq!(users.len(), 1);
            assert_eq!(users[0]["content"], json!(queued.text));
            drop(requests);
            {
                let mut app = shared.lock().unwrap();
                assert_eq!(app.agent.runs, 1);
                assert_eq!(app.agent.history.len(), 2);
                drain_store(&mut app);
            }
            assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none());
            drop(shared);
            let after = app_with(&store);
            assert!(after.agent.pending.is_none(), "a finished answer must not come back as queued");
            assert_eq!(after.agent.history.last().map(|m| m.text.as_str()), Some("answered after reboot"));
        }
    }

    #[test]
    fn a_corrupt_stored_pending_message_is_removed_logged_and_does_not_break_boot() {
        for bad in [
            "not json",
            "",
            "[]",
            "null",
            r#"{"text":"torn","source":"http","at_ms":3"#,
            r#"{"text":"missing time","source":"http"}"#,
            r#"{"text":7,"source":"http","at_ms":3}"#,
            r#"{"text":"bad source","source":7,"at_ms":3}"#,
            r#"{"text":"bad time","source":"http","at_ms":-1}"#,
            r#"{"text":"bad time","source":"http","at_ms":1.5}"#,
            r#"{"text":"foreign","source":"http","at_ms":3,"version":99}"#,
            r#"{"text":"  ","source":"http","at_ms":3}"#,
        ] {
            let store = SharedStore::new();
            assert!(store.clone().set(NVS_KEY_PENDING_MESSAGE, bad));
            let (mut app, log) = app_with_log(&store);
            assert!(app.agent.pending.is_none(), "`{bad}` became a queued question");
            assert!(app.store.get(NVS_KEY_PENDING_MESSAGE).is_none(), "the bad value was not removed");
            assert_eq!(
                log.tail(80, 0)
                    .iter()
                    .filter(|l| l.level == "warn"
                        && l.message.contains("stored pending user message does not check out"))
                    .count(),
                1,
                "`{bad}` must produce one warning"
            );
            assert_eq!(app.route(&get("/api/status")).status, 200, "boot leaves a usable app");
            drain_store(&mut app);
            assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none(), "the writer must remove the bad value too");
            assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"still usable"}"#)).status, 202);
            point_at_the_stub(&mut app);
            let model = Scripted::new(vec![Scripted::answer("yes")]);
            let seen = model.seen.clone();
            app.attach_model_transport(Box::new(model));
            assert_eq!(app.agent_run_once(), Ok("yes".to_string()));
            assert_eq!(seen.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn turning_persistence_off_keeps_queued_messages_out_of_flash() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        drain_store(&mut app);
        let writes_before = store.writes();
        app.cfg.agent.persist_transcript = false;
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"RAM only"}"#)).status, 202);
        assert!(app.agent.pending.is_some(), "the question still runs in RAM");
        assert!(!app.persist_pending_message());
        assert!(app.store.get(NVS_KEY_PENDING_MESSAGE).is_none());
        assert_eq!(app.store_queue().pending(), 0, "disabled persistence admits no pending-message write");
        drain_store(&mut app);
        assert_eq!(store.writes(), writes_before, "disabled persistence writes nothing");
        assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none());

        // A copy left by an earlier configuration is not revived when retention is off.
        let store = SharedStore::new();
        let mut writer = store.clone();
        assert!(writer.set(crate::config::NVS_KEY_CONFIG, r#"{"agent":{"persist_transcript":false}}"#));
        assert!(writer
            .set(NVS_KEY_PENDING_MESSAGE, r#"{"text":"old retained question","source":"http","at_ms":7}"#));
        let (mut app, log) = app_with_log(&store);
        assert!(!app.cfg.agent.persist_transcript);
        assert!(app.agent.pending.is_none());
        assert!(app.store.get(NVS_KEY_PENDING_MESSAGE).is_none());
        assert!(!log.tail(80, 0).iter().any(|l| l.message == "restored a queued user message from flash"));
        drain_store(&mut app);
        assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none(), "do not leave a question to resurrect later");
    }

    #[test]
    fn an_oversized_pending_question_is_saved_and_restored_with_the_user_byte_cap() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        let text = "界".repeat(crate::agent::MAX_USER_BYTES);
        assert!(text.len() > crate::agent::MAX_USER_BYTES, "UTF-8 bytes, not characters");
        let expected = llm::truncate_bytes(&text, crate::agent::MAX_USER_BYTES);
        app.tasks.push(ScheduledTask {
            id: "large".to_string(),
            prompt: text.clone(),
            every_s: 1,
            ..Default::default()
        });
        assert_eq!(app.sched_tick(1_000), vec!["schedule:large"]);
        let queued = app.store.get(NVS_KEY_PENDING_MESSAGE).unwrap();
        let value: Value = serde_json::from_str(&queued).unwrap();
        let saved_text = value["text"].as_str().unwrap();
        assert_eq!(saved_text, expected);
        assert_eq!(saved_text.len(), app.agent.pending.as_ref().unwrap().text.len());
        assert!(saved_text.len() < text.len());
        // truncate_bytes appends its marker outside the payload cap; preserve that existing
        // contract instead of changing what the model sees just to save the queue.
        assert!(saved_text.len() <= crate::agent::MAX_USER_BYTES + llm::TRUNCATED_MARK.len());
        assert!(saved_text.ends_with(llm::TRUNCATED_MARK));
        assert!(queued.len() > store::VALUE_MAX, "this key must route to the filesystem, not NVS");
        drain_store(&mut app);
        drop(app);
        let app = app_with(&store);
        assert_eq!(app.agent.pending.as_ref().unwrap().text, expected);
        drop(app);

        // Defensive restoration also clamps a complete but oversized JSON value from elsewhere.
        assert!(store.clone().set(
            NVS_KEY_PENDING_MESSAGE,
            &json!({
                "text": text, "source": "http", "at_ms": 17
            })
            .to_string()
        ));
        let app = app_with(&store);
        let pending = app.agent.pending.as_ref().unwrap();
        assert_eq!(pending.text, expected);
        assert!(pending.text.len() <= crate::agent::MAX_USER_BYTES + llm::TRUNCATED_MARK.len());
        assert_eq!(pending.source, "http");
        assert_eq!(pending.at_ms, 17);
    }

    #[test]
    fn every_trigger_saves_the_coalesced_pending_question() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        app.tasks.push(ScheduledTask {
            id: "t1".to_string(),
            prompt: "scheduled".to_string(),
            every_s: 1,
            ..Default::default()
        });
        assert_eq!(app.sched_tick(1_000), vec!["schedule:t1"]);
        drain_store(&mut app);
        let saved: Value = serde_json::from_str(&store.get(NVS_KEY_PENDING_MESSAGE).unwrap()).unwrap();
        assert_eq!(saved, json!({ "text": "scheduled", "source": "schedule:t1", "at_ms": 1_000 }));

        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"portal"}"#)).status, 202);
        drain_store(&mut app);
        let saved: Value = serde_json::from_str(&store.get(NVS_KEY_PENDING_MESSAGE).unwrap()).unwrap();
        assert_eq!(saved["text"], "portal");
        assert_eq!(saved["source"], "schedule:t1,http");
        assert_eq!(saved["at_ms"], app.agent.pending.as_ref().unwrap().at_ms);

        app.rules.push(SensorRule {
            id: "r1".to_string(),
            tool: "bme280_read".to_string(),
            path: "temperature_c".to_string(),
            prompt: "sensor".to_string(),
            ..Default::default()
        });
        assert_eq!(app.sched_tick(1_001), vec!["sensor:r1"]);
        drain_store(&mut app);
        let saved: Value = serde_json::from_str(&store.get(NVS_KEY_PENDING_MESSAGE).unwrap()).unwrap();
        assert_eq!(
            saved,
            json!({ "text": "sensor", "source": "schedule:t1,http,sensor:r1", "at_ms": 1_001 })
        );

        let run = app.route(&post("/api/sched/run", r#"{"id":"t1"}"#));
        assert_eq!(run.status, 200);
        assert_eq!(body_json(&run)["queued"], true);
        drain_store(&mut app);
        let saved: Value = serde_json::from_str(&store.get(NVS_KEY_PENDING_MESSAGE).unwrap()).unwrap();
        let pending = app.agent.pending.as_ref().unwrap();
        assert_eq!(saved, json!({ "text": pending.text, "source": pending.source, "at_ms": pending.at_ms }));
        assert_eq!(pending.text, "scheduled", "newest text wins");
        assert_eq!(pending.source, "schedule:t1,http,sensor:r1", "repeated sources are not duplicated");
    }

    #[test]
    fn repeated_grow_and_compact_cycles_degrade_the_archive_instead_of_growing_it() {
        // The question this answers: is compaction idempotent, and what happens over
        // repeated grow -> compact -> grow -> compact cycles?
        //
        // Idempotent in the narrow sense: a second pass over an unchanged transcript removes
        // nothing. Not a fixed point across cycles: each cycle converts the exchanges that
        // grew into index lines, and the index has its own caps.
        //
        // What the caps take is now a policy question rather than "the oldest first": a
        // question states what was wanted and an answer is prose the model wrote, so the
        // answers are what a full index gives up first. At this scale every question from
        // every cycle survives while only the answers are dropped - which is the property
        // the eviction order exists for.
        let mut app = test_app();
        let mut rows = Vec::new();
        for cycle in 0..6u32 {
            for turn in 0..6u32 {
                let at = u64::from(cycle * 10 + turn);
                app.agent.history.push(crate::agent::Message::user(
                    &format!("cycle{cycle} question {turn}"),
                    "http",
                    at,
                ));
                app.agent.history.push(crate::agent::Message::assistant(
                    &format!("cycle{cycle} answer {turn}"),
                    vec![],
                    at,
                ));
            }
            let r = app.agent.compact_now(Some(1));
            rows.push((
                cycle,
                r.messages_before,
                r.messages_after,
                r.removed,
                r.archive_lines,
                r.bytes_after,
            ));
        }
        for (cycle, before, after, removed, lines, bytes) in &rows {
            println!(
                "cycle {cycle}: {before} -> {after} messages, {removed} dropped, {lines} archive lines, {bytes} bytes"
            );
        }

        // The transcript holds exactly the kept exchange after every cycle, so the growth is
        // 12 messages per cycle (six exchanges) and the drop is what arrived in it. The
        // first cycle starts empty, so it sees 12 before, not 14.
        for (cycle, before, after, removed, _, _) in &rows {
            assert_eq!(*after, 3, "one exchange survives each cycle, behind the index");
            assert_eq!(*before, if *cycle == 0 { 12 } else { 15 }, "index + kept exchange + six new ones");
            // What this pass drained: everything except the kept exchange. From the second
            // cycle on, the previous index is replaced rather than drained, so it is not in
            // this count — which is why cycle 0 drops one fewer.
            assert_eq!(*removed, if *cycle == 0 { 10 } else { 12 }, "cycle {cycle}");
        }
        // A second pass over the same transcript is a no-op.
        let again = app.agent.compact_now(Some(1));
        assert_eq!(again.removed, 0, "compaction of an unchanged transcript is idempotent");

        // The archive is bounded by lines and bytes, not by how many cycles ran.
        assert!(app.agent.archive.len() <= crate::agent::ARCHIVE_MAX_LINES, "{}", app.agent.archive.len());
        let bytes: usize = app.agent.archive.iter().map(|l| l.len()).sum();
        assert!(bytes <= crate::agent::ARCHIVE_MAX_BYTES, "{bytes}");

        // The policy, measured: questions outlive answers. The first cycle's question is
        // still here even though 30 exchanges have passed through the index, and the lines
        // that went are answers.
        let questions = app.agent.archive.iter().filter(|l| l.starts_with("user: ")).count();
        let answers = app.agent.archive.iter().filter(|l| l.starts_with("answer: ")).count();
        println!("final archive: {questions} questions, {answers} answers");
        assert!(
            app.agent.archive.iter().any(|l| l.contains("cycle0 question 0")),
            "a question was evicted before the answers: {:?}",
            app.agent.archive.iter().take(3).collect::<Vec<_>>()
        );
        assert!(questions > answers, "{questions} questions vs {answers} answers");
        assert!(app.agent.archive.iter().any(|l| l.contains("question 4")), "{:?}", app.agent.archive);
        // At least one row shows the archive no longer growing: that is where the loss starts.
        let lines: Vec<usize> = rows.iter().map(|r| r.4).collect();
        assert_eq!(lines.last(), Some(&crate::agent::ARCHIVE_MAX_LINES), "{lines:?}");
    }

    #[test]
    fn manual_compaction_reports_what_it_dropped_and_keeps_the_newest_exchange() {
        let mut app = test_app();
        // Four exchanges, and a tool call inside the oldest one: the archive has to carry
        // the call and its result, not just the prose.
        app.agent.history.push(crate::agent::Message::assistant(
            "",
            vec![llm::ToolCall {
                id: "c0".into(),
                name: "board".into(),
                arguments: json!({ "command": "status" }),
            }],
            0,
        ));
        app.agent.history.push(crate::agent::Message::tool_result("c0", "ok", false, 0));
        for turn in 0..4u64 {
            app.agent.history.push(crate::agent::Message::user(
                &format!("question {turn} {}", "detail ".repeat(40)),
                "http",
                turn,
            ));
            app.agent.history.push(crate::agent::Message::assistant(
                &format!("answer {turn} {}", "detail ".repeat(40)),
                vec![],
                turn,
            ));
        }
        assert_eq!(app.agent.history.len(), 10, "four exchanges plus the call/result pair");

        let compacted = app.route(&post("/api/agent/compact", r#"{"keep":1}"#));
        assert_eq!(compacted.status, 200);
        let body = body_json(&compacted);
        assert_eq!(body["ok"], true);
        assert_eq!(body["kept_turns"], 1);
        assert_eq!(body["messages_before"], 10);
        assert_eq!(body["messages_after"], 3, "one exchange survives verbatim, behind the index message");
        assert_eq!(body["removed"], 8);
        assert!(body["bytes_after"].as_u64().unwrap() < body["bytes_before"].as_u64().unwrap());
        assert!(body["tokens_after"].as_u64().unwrap() < body["tokens_before"].as_u64().unwrap());
        assert_eq!(body["stored"], true, "a compression flash never hears about comes back after a reboot");

        // What survives is the newest exchange, and it is still a valid request: the first
        // message is the question, never an orphaned tool result.
        let messages = app.agent_history_json(0)["messages"].as_array().unwrap().clone();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["checkpoint"], true, "the index leads the kept tail: {messages:?}");
        assert!(messages[0]["text"].as_str().unwrap().contains("[Context compaction:"));
        assert_eq!(messages[1]["role"], "user");
        assert!(
            messages[1]["text"].as_str().unwrap().starts_with("question 3 "),
            "the kept question survives whole: {}",
            messages[1]["text"]
        );
        assert!(
            messages[2]["text"].as_str().unwrap().starts_with("answer 3 "),
            "the kept answer survives whole"
        );

        // The dropped part is not gone silently: the tool call it contained is in the
        // index, which is what stops a model repeating a call it already made.
        assert!(body["archive_lines"].as_u64().unwrap() >= 8, "{} archive lines", body["archive_lines"]);
        assert!(app.agent.archive.iter().any(|l| l.starts_with("called: board(")), "{:?}", app.agent.archive);
        assert!(app.agent.archive.iter().any(|l| l == "result: ok"), "{:?}", app.agent.archive);
        assert!(
            app.agent.archive.iter().any(|l| l.starts_with("user: question 0")),
            "{:?}",
            app.agent.archive
        );

        // A second pass has nothing left to drop and must say so instead of inventing a
        // saving.
        let again = body_json(&app.route(&post("/api/agent/compact", "{}")));
        assert_eq!(again["removed"], 0);
        assert_eq!(again["messages_after"], 3, "the index message is not duplicated");
    }

    #[test]
    fn the_default_compaction_budget_keeps_a_resumable_tail() {
        let mut app = test_app();
        for turn in 0..3u64 {
            app.agent.history.push(crate::agent::Message::user(&format!("q{turn}"), "http", turn));
            app.agent.history.push(crate::agent::Message::assistant(&format!("a{turn}"), vec![], turn));
        }
        // No `keep`: the budget the automatic trigger would spend. Every exchange fits in
        // the default window, so a manual pass must not compress below what the trigger
        // itself would keep.
        let body = body_json(&app.route(&post("/api/agent/compact", "{}")));
        assert_eq!(body["ok"], true);
        assert_eq!(body["removed"], 0, "{} messages kept", body["messages_after"]);
        assert!(body["kept_turns"].as_u64().unwrap() >= 3, "{} kept turns", body["kept_turns"]);
    }

    #[test]
    fn compaction_refuses_a_bad_keep_and_a_turn_in_flight() {
        let mut app = test_app();
        point_at_the_stub(&mut app);
        assert_eq!(app.route(&post("/api/agent/compact", r#"{"keep":-1}"#)).status, 400);
        assert_eq!(app.route(&post("/api/agent/compact", r#"{"keep":"two"}"#)).status, 400);
        assert_eq!(app.agent.history.len(), 0, "a refused request changes nothing");
        // A turn in flight is answering a question this would drop from under it.
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"hold this"}"#)).status, 202);
        assert!(matches!(app.agent_begin(), Some(AgentStep::Ask { .. })));
        let refused = app.route(&post("/api/agent/compact", "{}"));
        assert_eq!(refused.status, 409);
        assert_eq!(app.agent.history.len(), 1, "the in-flight question survived");
    }

    #[test]
    fn manual_compaction_between_turns_leaves_a_request_that_still_starts_at_a_question() {
        let mut app = test_app();
        point_at_the_stub(&mut app);
        for turn in 0..6u64 {
            app.agent.history.push(crate::agent::Message::user(&format!("task {turn}"), "http", turn));
            app.agent.history.push(crate::agent::Message::assistant(&format!("did {turn}"), vec![], turn));
        }
        let body = body_json(&app.route(&post("/api/agent/compact", r#"{"keep":2}"#)));
        assert_eq!(body["removed"], 8);
        assert_eq!(body["messages_after"], 5, "the index plus the last two complete exchanges");
        // The conversation continues: the next question goes out with a transcript that
        // still starts at a question, and it is the one the model is being asked.
        app.agent.push_user("the task that matters", "http", 6);
        match app.agent_begin().expect("the next turn asks the model") {
            AgentStep::Ask { messages, .. } => {
                let sent = messages.as_array().unwrap();
                assert_eq!(sent[0]["role"], "system");
                assert_eq!(sent[1]["role"], "user", "no orphaned tool result leads the request");
                assert!(
                    sent[1]["content"].as_str().unwrap().contains("[Context compaction:"),
                    "the index is the first thing after the system prompt: {}",
                    sent[1]["content"]
                );
                assert_eq!(sent[2]["content"], "task 4");
                assert_eq!(sent[sent.len() - 1]["content"], "the task that matters");
            }
            AgentStep::Done(..) => panic!("the conversation should continue"),
        }
    }

    #[test]
    fn a_newer_queued_question_survives_reboot_while_an_older_turn_was_in_flight() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        point_at_the_stub(&mut app);
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"older question"}"#)).status, 202);
        assert!(matches!(app.agent_begin(), Some(AgentStep::Ask { .. })));
        assert!(matches!(
            app.agent_reply(Ok(llm::ModelReply {
                tool_calls: vec![llm::ToolCall {
                    id: "old-call".into(),
                    name: "board".into(),
                    arguments: json!({ "command": "status" })
                }],
                ..Default::default()
            })),
            AgentStep::Ask { .. }
        ));
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"newer question"}"#)).status, 202);
        let queued = app.agent.pending.clone().unwrap();
        drain_store(&mut app);
        assert!(store.get(NVS_KEY_RUN_IN_FLIGHT).is_some());
        drop(app);

        let (mut app, log) = app_with_log(&store);
        assert_eq!(app.agent.pending.as_ref().unwrap().text, queued.text);
        assert_eq!(app.agent.pending.as_ref().unwrap().source, "http");
        assert_eq!(app.agent.pending.as_ref().unwrap().at_ms, queued.at_ms);
        assert!(app.agent.ends_on_a_tool_result(), "the older turn's context is still available");
        assert!(log
            .tail(80, 0)
            .iter()
            .any(|l| l.level == "warn" && l.message.contains("keeping the newer queued question")));
        drain_store(&mut app);
        assert!(store.get(NVS_KEY_RUN_IN_FLIGHT).is_none());
        assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_some(), "restoring is not consuming");
        drop(app);

        // Even another reboot before the run starts keeps the queued question.
        let mut app = app_with(&store);
        point_at_the_stub(&mut app);
        let model = Scripted::new(vec![Scripted::answer("newer question answered")]);
        let seen = model.seen.clone();
        app.attach_model_transport(Box::new(model));
        assert_eq!(app.agent_run_once(), Ok("newer question answered".to_string()));
        assert_eq!(app.agent_run_once(), Ok(String::new()));
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1, "do not replay the older turn in a second run");
        let request: Value = serde_json::from_str(&requests[0]).unwrap();
        assert_eq!(request["messages"].as_array().unwrap().last().unwrap()["content"], "newer question");
        assert_eq!(app.agent.history.iter().filter(|m| m.role == crate::agent::Role::User).count(), 2);
        drain_store(&mut app);
        assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none());
    }

    #[test]
    fn the_final_transcript_exchange_wins_over_the_same_stored_queue_copy() {
        for text in ["same question".to_string(), "界".repeat(crate::agent::MAX_SAVED_TEXT_BYTES)] {
            for (source, at_ms, duplicate) in
                [("http", 77, true), ("http", 78, false), ("sensor:r1", 77, false)]
            {
                let store = SharedStore::new();
                let mut writer = app_with(&store);
                writer.agent.history.push(crate::agent::Message::user(&text, "http", 77));
                writer.agent.history.push(crate::agent::Message::assistant(
                    "already answered",
                    Vec::new(),
                    78,
                ));
                assert!(writer.persist_transcript());
                assert!(writer.queue_user(&text, source, at_ms));
                drain_store(&mut writer);
                drop(writer);

                let mut app = app_with(&store);
                point_at_the_stub(&mut app);
                let model = Scripted::new(vec![Scripted::answer("asked again deliberately")]);
                let seen = model.seen.clone();
                app.attach_model_transport(Box::new(model));
                if duplicate {
                    assert!(app.agent.pending.is_none(), "the transcript already contains this turn");
                    assert!(app.store.get(NVS_KEY_PENDING_MESSAGE).is_none());
                    assert_eq!(app.agent_run_once(), Ok(String::new()));
                    assert_eq!(
                        seen.lock().unwrap().len(),
                        0,
                        "do not bill for the same completed turn again"
                    );
                    assert_eq!(app.agent.history.len(), 2);
                } else {
                    let pending =
                        app.agent.pending.as_ref().expect("same text with new metadata is a new turn");
                    assert_eq!(pending.source, source);
                    assert_eq!(pending.at_ms, at_ms);
                    assert_eq!(app.agent_run_once(), Ok("asked again deliberately".to_string()));
                    assert_eq!(seen.lock().unwrap().len(), 1);
                }
                drain_store(&mut app);
                assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none());
            }
        }

        // A stale queue copy of an interrupted question is discarded too: only the existing
        // in-flight recovery queues it, with its resume source, for one model call.
        let store = SharedStore::new();
        let mut writer = app_with(&store);
        writer.agent.history.push(crate::agent::Message::user("interrupted", "http", 77));
        assert!(writer.persist_transcript());
        assert!(writer.queue_user("interrupted", "http", 77));
        assert!(writer.mark_run_in_flight("http"));
        drain_store(&mut writer);
        drop(writer);
        let mut app = app_with(&store);
        assert_eq!(app.agent.pending.as_ref().unwrap().source, RESUME_SOURCE);
        point_at_the_stub(&mut app);
        let model = Scripted::new(vec![Scripted::answer("resumed once")]);
        let seen = model.seen.clone();
        app.attach_model_transport(Box::new(model));
        assert_eq!(app.agent_run_once(), Ok("resumed once".to_string()));
        assert_eq!(app.agent_run_once(), Ok(String::new()));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unstarted_turn_keeps_its_saved_question_when_the_transport_is_missing() {
        let store = SharedStore::new();
        let mut app = app_with(&store);
        app.agent.history.push(crate::agent::Message::user("previous question", "http", 1));
        app.agent.history.push(crate::agent::Message::assistant("previous answer", Vec::new(), 2));
        assert!(app.persist_transcript());
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"not started yet"}"#)).status, 202);
        drain_store(&mut app);
        assert!(app.agent_run_once().unwrap_err().contains("transport"));
        assert_eq!(app.agent.pending.as_ref().unwrap().text, "not started yet");
        assert!(app.store.get(NVS_KEY_PENDING_MESSAGE).is_some());
        assert!(
            app.store.get(NVS_KEY_RUN_IN_FLIGHT).is_none(),
            "no turn started, so do not resume the old one"
        );
        drain_store(&mut app);
        drop(app);
        let mut app = app_with(&store);
        assert_eq!(app.agent.pending.as_ref().unwrap().text, "not started yet");
        point_at_the_stub(&mut app);
        let model = Scripted::new(vec![Scripted::answer("transport is here now")]);
        let seen = model.seen.clone();
        app.attach_model_transport(Box::new(model));
        assert_eq!(app.agent_run_once(), Ok("transport is here now".to_string()));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn forgetting_also_removes_a_question_that_has_not_started() {
        for (path, body) in
            [("/api/agent/reset", "{}"), ("/api/agent/forget", "{}"), ("/api/agent/forget", r#"{"keep":1}"#)]
        {
            for persistence in [true, false] {
                let store = SharedStore::new();
                let mut app = app_with(&store);
                app.agent.history.push(crate::agent::Message::user("past", "http", 1));
                app.agent.history.push(crate::agent::Message::assistant("answered", Vec::new(), 2));
                assert!(app.persist_transcript());
                assert_eq!(
                    app.route(&post("/api/agent/message", r#"{"text":"never start this"}"#)).status,
                    202
                );
                drain_store(&mut app);
                assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_some());
                app.cfg.agent.persist_transcript = persistence;
                let writes_before = store.writes();
                assert_eq!(app.route(&post(path, body)).status, 200);
                assert!(app.agent.pending.is_none());
                assert!(app.store.get(NVS_KEY_PENDING_MESSAGE).is_none());
                assert_eq!(
                    store.writes(),
                    writes_before,
                    "forget requests removal without writing under the App lock"
                );
                drain_store(&mut app);
                assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_none());
                drop(app);
                assert!(
                    app_with(&store).agent.pending.is_none(),
                    "a forgotten pending question must not return"
                );
            }
        }
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
                Scripted::tool_call(&format!("c{turn}"), "board", r#"{"command": "status"}"#),
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
                    name: "board".into(),
                    arguments: json!({ "command": "status" }),
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
        assert!(store.get(NVS_KEY_PENDING_MESSAGE).is_some(), "the resumed question is persisted too");
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

        // Consuming the marker must not make a second reboot lose this not-yet-started resume.
        drop(app);
        let mut app = app_with(&store);
        point_at_the_stub(&mut app);
        let restored = app.agent.pending.as_ref().expect("the resumed question survived another reboot");
        assert_eq!(restored.text, queued.text);
        assert_eq!(restored.source, queued.source);
        assert_eq!(restored.at_ms, queued.at_ms);

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
        assert!(call["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap_or_default()
            .contains("\"command\":\"status\""));
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

    /// The REST script route is gone. `script` is an MCP tool and the tool already waits
    /// for the answer, so the route only added a second, queued way to say the same thing.
    #[test]
    fn the_script_route_is_not_served_over_http() {
        let mut app = test_app();
        for (method, path) in [("POST", "/api/script"), ("GET", "/api/script/s1")] {
            let mut req = post(path, r#"{"source":"1 + 2"}"#);
            req.method = method.into();
            assert_eq!(app.route(&req).status, 404, "{method} {path} still answers");
        }
        // The tool path still runs a script, and a failure names the line it stopped on,
        // so a caller can tell "it ran and refused" from "it never ran".
        let job = app.submit_script("let = 3");
        assert_eq!(job.state, "failed");
        assert!(job.error.contains("line"), "{job:?}");
        let job = app.submit_script("1 + 2");
        assert_eq!(job.state, "done");
        assert_eq!(job.result, json!(3));
    }

    #[test]
    fn at_hhmm_fires_on_the_configured_local_clock_not_utc() {
        let mut app = test_app();
        // 2026-11-14T23:40:00Z. UTC+8 makes that 07:40 the next morning.
        let at_utc_evening = 1_794_613_200_000u64;
        assert_eq!(crate::sched::minute_of_day(at_utc_evening, 0), 23 * 60 + 40);
        assert_eq!(crate::sched::minute_of_day(at_utc_evening, 8), 7 * 60 + 40);
        assert_eq!(crate::sched::minute_of_day(at_utc_evening, -5), 18 * 60 + 40);

        let add = app.route(&post(
            "/api/sched",
            r#"{"kind":"task","op":"add","task":{"id":"morning","prompt":"good morning","at_hhmm":"07:40","last_run_ms":0}}"#,
        ));
        assert_eq!(add.status, 200, "{:?}", body_json(&add));

        // In UTC it is 23:40, so a 07:40 task is not due: the offset is what makes it fire.
        assert!(app.sched_tick(at_utc_evening).is_empty(), "fired at the UTC minute");
        app.cfg.tz_offset_h = 8;
        assert_eq!(app.sched_tick(at_utc_evening), vec!["schedule:morning".to_string()]);

        // A second tick in the same local minute does not run it twice.
        assert!(app.sched_tick(at_utc_evening + 30_000).is_empty());
    }

    #[test]
    fn the_timezone_offset_is_a_validated_setting() {
        let mut app = test_app();
        assert_eq!(app.cfg.tz_offset_h, 0, "a fresh board is UTC, not a guess");
        let ok = app.route(&post("/api/config", r#"{"tz_offset_h":8}"#));
        assert_eq!(ok.status, 200, "{:?}", body_json(&ok));
        assert_eq!(app.cfg.tz_offset_h, 8);
        assert_eq!(body_json(&app.route(&get("/api/config")))["tz_offset_h"], 8);
        // Half-hour zones are refused rather than rounded: 5.5 is not an hour offset the
        // board can honour, and silently storing 5 would shift every task by half an hour.
        for bad in [
            r#"{"tz_offset_h":5.5}"#,
            r#"{"tz_offset_h":15}"#,
            r#"{"tz_offset_h":-13}"#,
            r#"{"tz_offset_h":"+8"}"#,
        ] {
            let resp = app.route(&post("/api/config", bad));
            assert_eq!(resp.status, 400, "{bad} was accepted");
            assert_eq!(app.cfg.tz_offset_h, 8, "{bad} changed the stored offset");
        }
        assert_eq!(app.route(&post("/api/config", r#"{"tz_offset_h":-12}"#)).status, 200);
        assert_eq!(app.cfg.tz_offset_h, -12);
        assert_eq!(app.route(&post("/api/config", r#"{"tz_offset_h":14}"#)).status, 200);
    }

    #[test]
    fn ifconfig_reports_measured_network_values_and_never_an_empty_dns_slot() {
        let mut app = test_app();
        // Before any lease: nothing measured, so the config's (empty) values show and the
        // DNS list is empty rather than `["", ""]`.
        app.hw = Box::new(crate::virtual_board::VirtualBoard::new());
        let fresh = body_json(&app.route(&get("/api/workspace")));
        let _ = fresh;
        let ifconfig = app.ifconfig_json();
        assert_eq!(ifconfig["netmask"], "255.255.255.0", "the lease's mask, not the config's empty one");
        assert_eq!(ifconfig["gateway"], "192.168.3.1");
        assert_eq!(ifconfig["dns"], json!(["192.168.3.1"]));
        assert_eq!(ifconfig["channel"], 6);
        assert_eq!(ifconfig["rssi_dbm"], -48);
        assert_eq!(ifconfig["lease_or_override"], "dhcp lease");
        // The note has to distinguish "no association" from "0 dBm at the antenna".
        assert!(ifconfig["note"].as_str().unwrap().contains("not associated"));

        // A configured override is reported as an override, not silently mixed with the lease.
        app.cfg.wifi.gateway = "192.168.3.232".into();
        let override_view = app.ifconfig_json();
        assert_eq!(override_view["gateway"], "192.168.3.1", "the interface still reports what it has");
        assert_eq!(override_view["lease_or_override"], "override");
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

        let fired = app.sched_tick(60_000);
        assert_eq!(fired.len(), 2, "{fired:?}");
        assert!(fired.contains(&"schedule:t2".to_string()));

        // The script task ran without the model: no turn was queued for it, and the
        // board ran exactly one script. The prompt task queued a message as before.
        assert_eq!(app.script_seq, 1, "the script task ran once");
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
        let fired = app.sched_tick(now);
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
        assert_eq!(app.store_queue().pending(), 2, "the schedule and its pending question, once per key");

        // The writer is what makes it durable, and it is the only thing that does.
        let report = app.flush_store("test-writer");
        assert_eq!(report.written, 2);
        assert_eq!(report.failed, 0);
        assert_eq!(report.keys, vec![NVS_KEY_PENDING_MESSAGE.to_string(), NVS_KEY_SCHED.to_string()]);
        assert_eq!(store.writes(), writes_before + 2);
        assert!(store.get(NVS_KEY_SCHED).expect("stored").contains("\"t1\""));
        assert_eq!(app.store_queue().pending(), 0);
        // And the next tick is not due: only true because the write was applied.
        assert!(app.sched_tick(now + 1_000).is_empty());
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
        assert!(shared.lock().unwrap().tools.get("display").is_some(), "no OLED on the test bench");

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

        for body in [b"A\x04".as_slice(), b"A\x04\x00\x01", b"E\x01\x00\x00"] {
            let mut sink = match router(&req) {
                BodyRoute::Sink(s) => s,
                _ => panic!("a fresh sink"),
            };
            // HTTP completed, but the inner record did not (or E carried a payload).
            let _ = sink.push(body);
            let resp = sink.finish(true);
            assert_eq!(resp.status, 400, "accepted malformed AV body {body:?}");
            assert_eq!(body_json(&resp)["ok"], false);
        }
        let sink = match router(&req) {
            BodyRoute::Sink(s) => s,
            _ => panic!("a fresh sink"),
        };
        assert_eq!(sink.finish(true).status, 200, "an empty complete body has no torn record");
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
            if route.path == "/api/attachment/<name>" {
                app.attachments = vec![crate::agent::Attachment {
                    name: "fixture.txt".into(),
                    mime: "text/plain".into(),
                    data_b64: "Zml4dHVyZQ==".into(),
                }];
                req.path = "/api/attachment/fixture.txt".into();
            } else if route.path == "/api/sched/run" {
                app.tasks.push(ScheduledTask {
                    id: "map-run".into(),
                    script: "1".into(),
                    ..ScheduledTask::default()
                });
                req.body = br#"{"id":"map-run"}"#.to_vec();
            }
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
        let portal = doc["portal_endpoints"].as_array().expect("portal_endpoints");
        assert!(endpoints.iter().any(|e| e["path"] == "/agent.json"));
        // Wherever a private read is listed, its credential requirement goes with it.
        let all: Vec<&Value> = endpoints.iter().chain(portal.iter()).collect();
        for path in [
            "/api/config",
            "/api/agent/history",
            "/api/agent/prompt",
            "/api/agent/events",
            "/api/agent/stream",
            "/api/logs",
            "/api/files/tree",
        ] {
            assert!(
                all.iter().any(|e| e["method"] == "GET" && e["path"] == path && e["auth"] != "none"),
                "discovery incorrectly advertises a private read as open: {path}"
            );
        }
        app.cfg.auth.portal_pin = "1234".into();
        let doc = body_json(&app.route(&req));
        assert!(doc["dynamic"]["mcp"]["auth"].as_str().unwrap().contains("portal credential"));
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

        // The terminal tab is the one panel that reaches the board through `/mcp` rather than
        // a REST route, so the declaration, the transport and the screen all have to ship.
        assert!(web::INDEX_HTML.contains("data-panel=\"terminal\""), "the tab is missing");
        assert!(web::INDEX_HTML.contains("id=\"term-screen\""), "the screen is missing");
        assert!(web::INDEX_HTML.contains("id=\"term-input\""), "nothing to type in");
        assert!(web::APP_JS.contains("method: \"tools/call\""), "the terminal never calls a tool");
        assert!(web::APP_JS.contains("Mcp-Session-Id"), "the terminal opens a session per command");
        assert!(web::STYLE_CSS.contains(".term-screen"), "the screen has no style of its own");
    }

    #[test]
    fn raw_board_arguments_refuse_before_hardware_access() {
        use crate::hardware::{BoardInfo, PinMode};
        struct NoRawAccess;
        impl Hardware for NoRawAccess {
            fn board_info(&mut self) -> BoardInfo {
                BoardInfo::default()
            }
            fn gpio_config(&mut self, _: i32, _: PinMode) -> Result<()> {
                panic!("GPIO configured before validation")
            }
            fn gpio_read(&mut self, _: i32) -> Result<bool> {
                panic!("GPIO read before validation")
            }
            fn gpio_write(&mut self, _: i32, _: bool) -> Result<()> {
                panic!("GPIO written before validation")
            }
            fn adc_mv(&mut self, _: i32) -> Result<u32> {
                panic!("ADC read before validation")
            }
            fn i2c_scan(&mut self, _: u8) -> Result<Vec<u8>> {
                panic!("I2C scanned before validation")
            }
            fn i2c_read(&mut self, _: u8, _: u8, _: Option<u8>, _: &mut [u8]) -> Result<()> {
                panic!("I2C read before validation")
            }
            fn i2c_write(&mut self, _: u8, _: u8, _: Option<u8>, _: &[u8]) -> Result<()> {
                panic!("I2C written before validation")
            }
        }
        let mut app = test_app();
        app.hw = Box::new(NoRawAccess);
        let refused = |app: &mut App, name: &str, args: Value| {
            let error = app.call_tool("bus", &json!({ "command": name, "arguments": args })).unwrap_err();
            assert_eq!(error.code, code::INVALID_PARAMS, "{name} {args}: {}", error.message);
        };
        for name in ["gpio_read", "gpio_write", "gpio_mode", "adc_read"] {
            let mut args = json!({"level":1,"mode":"output"});
            refused(&mut app, name, args.clone());
            for pin in [
                json!(null),
                json!(true),
                json!(1.5),
                json!(1.0),
                json!("1.2"),
                json!("bad"),
                json!([]),
                json!({}),
                json!(-1),
                json!(49),
                json!(u64::MAX),
            ] {
                args["pin"] = pin;
                refused(&mut app, name, args.clone());
            }
        }
        refused(&mut app, "gpio_write", json!({"pin":12}));
        for level in [json!(null), json!(false), json!(0.5), json!("bad"), json!({}), json!(-1), json!(2)] {
            refused(&mut app, "gpio_write", json!({"pin":12,"level":level}));
        }
        refused(&mut app, "gpio_mode", json!({"pin":12}));
        for name in ["i2c_read", "i2c_write"] {
            refused(&mut app, name, json!({"data":[0]}));
            for addr in [json!(null), json!(true), json!(1.5), json!("bad"), json!({}), json!(0), json!(128)]
            {
                refused(&mut app, name, json!({"addr":addr,"data":[0]}));
            }
            for field in ["bus", "reg"] {
                for raw in [json!(null), json!(true), json!(1.5), json!("bad"), json!(-1), json!(256)] {
                    let mut args = json!({"addr":60,"data":[0]});
                    args[field] = raw;
                    refused(&mut app, name, args);
                }
            }
        }
        for raw in [json!(null), json!(true), json!(1.5), json!("1"), json!("bad"), json!(0), json!(65)] {
            refused(&mut app, "i2c_read", json!({"addr":60,"length":raw}));
        }
        for raw in [json!(null), json!(true), json!(1.5), json!("2"), json!(-1), json!(256)] {
            refused(&mut app, "i2c_write", json!({"addr":60,"data":[0,raw]}));
        }
        refused(&mut app, "i2c_write", json!({"addr":60}));
        refused(&mut app, "i2c_write", json!({"addr":60,"data":"bytes"}));
        for bus in [json!(null), json!(true), json!(1.5), json!("bad"), json!(-1), json!(4)] {
            refused(&mut app, "i2c_scan", json!({"bus":bus}));
        }
        // Numeric strings remain supported; absent optional fields keep the configured
        // bus, one-byte read length and command-only register default.
        app.hw = Box::new(VirtualBoard::new());
        app.cfg.bus.i2c[0].bus = 2;
        assert_eq!(
            app.call_tool("bus", &json!({"command": "gpio_write", "arguments": {"pin":"12","level":"1"}}))
                .unwrap()["pin"],
            12
        );
        let read =
            app.call_tool("bus", &json!({"command": "i2c_read", "arguments": {"addr":"118"}})).unwrap();
        assert_eq!(read["bus"], 2);
        assert_eq!(read["bytes"].as_array().unwrap().len(), 1);
        assert_eq!(read["reg"], Value::Null);
        let read = app
            .call_tool(
                "bus",
                &json!({"command": "i2c_read", "arguments": {"addr":"118","reg":"208","length":1,"bus":"0"}}),
            )
            .unwrap();
        assert_eq!(read["bytes"][0], 0x60);
        assert!(app
            .call_tool("bus", &json!({"command": "i2c_write", "arguments": {"addr":"60","data":[0,255]}}))
            .is_ok());
    }

    #[test]
    fn rpc_batches_keep_valid_peers_errors_and_array_responses() {
        let mut app = test_app();
        let reply = app.handle_rpc(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#, None).unwrap();
        assert_eq!(reply.as_array().unwrap().len(), 1);
        assert_eq!(reply[0]["id"], 1);
        let reply = app
            .handle_rpc(
                r#"[
            {"jsonrpc":"2.0","id":"first","method":"ping"},
            17,
            {"jsonrpc":"2.0","method":"ping"},
            {"jsonrpc":"2.0","id":{},"method":"tools/call","params":{"name":"devices_probe"}},
            {"jsonrpc":"2.0","id":null,"method":"ping"},
            {"id":"bad-version","method":"tools/call","params":{"name":"devices_probe"}}
        ]"#,
                None,
            )
            .unwrap();
        assert_eq!(reply.as_array().unwrap().len(), 5);
        assert_eq!(reply[0]["id"], "first");
        assert!(reply[0].get("result").is_some());
        assert_eq!(reply[1]["error"]["code"], code::INVALID_REQUEST);
        assert_eq!(reply[1]["id"], Value::Null);
        assert_eq!(reply[2]["error"]["code"], code::INVALID_REQUEST);
        assert_eq!(reply[2]["id"], Value::Null);
        assert_eq!(reply[3]["id"], Value::Null);
        assert!(reply[3].get("result").is_some());
        assert_eq!(reply[4]["id"], "bad-version");
        assert_eq!(reply[4]["error"]["code"], code::INVALID_REQUEST);
        assert_eq!(app.runtime.counters.bad_requests, 3);
        assert_eq!(app.runtime.counters.notifications, 1);
        assert!(!app.runtime.reprobe_requested, "invalid requests must not execute");
        assert!(app.handle_rpc(r#"[{"jsonrpc":"2.0","method":"ping"}]"#, None).is_none());
        let error = app.handle_rpc("[", None).unwrap();
        assert_eq!(error["error"]["code"], code::PARSE_ERROR);
        let error = app.handle_rpc("[]", None).unwrap();
        assert_eq!(error["error"]["code"], code::INVALID_REQUEST);
        assert!(error.is_object());
    }

    #[test]
    fn valid_rpc_notifications_execute_without_a_response() {
        let mut app = test_app();
        let notification = json!({"jsonrpc":"2.0","method":"tools/call","params":{
            "name":"schedule","arguments":{"action":"add","prompt":"notification task","every_s":86400}
        }});
        assert!(app.handle_rpc(&notification.to_string(), None).is_none());
        assert_eq!(app.tasks.len(), 1);
        assert_eq!(app.tasks[0].prompt, "notification task");
        let mut invalid = notification;
        invalid["jsonrpc"] = json!("1.0");
        invalid["params"]["arguments"]["prompt"] = json!("invalid task");
        let error = app.handle_rpc(&invalid.to_string(), None).unwrap();
        assert_eq!(error["error"]["code"], code::INVALID_REQUEST);
        assert_eq!(app.tasks.len(), 1, "invalid notifications must not execute");
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
        app.call_tool("manifest", &json!({"command": "apply", "arguments": {"manifest": manifest}}))
            .expect("adopting a candidate works");
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
            assert!(
                bytes < 2048,
                "{section} is {bytes} bytes; the reference section's compact policy is 2 KiB"
            );
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
    fn asking_the_board_for_its_own_status_answers_in_process() {
        let mut app = test_app();
        let started = std::time::Instant::now();
        // The path MCP and the on-board agent both take for `net_*` is `ToolHost`.
        let url = format!("http://127.0.0.1:{}/api/status", app.cfg.mcp.port);
        let out = ToolHost::call_tool(&mut app, "net_get", &json!({"url": url}))
            .expect("a self request is answered");
        let body: Value = serde_json::from_str(&out).expect("json");
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(1), "took {took:?}");
        assert_eq!(body["status"], 200);
        assert_eq!(body["in_process"], true);
        assert!(body["body"].as_str().unwrap().contains("device_name"));

        // A different port is somebody else's service: it goes to the transport.
        let other =
            ToolHost::call_tool(&mut app, "net_get", &json!({"url": "http://127.0.0.1:9/api/status"}));
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
        let app = test_app();
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

        let specs = ToolHost::tool_specs(&app);
        let write_spec = specs.as_array().unwrap().iter().find(|t| t["function"]["name"] == "write").unwrap();
        let description = write_spec["function"]["description"].as_str().unwrap();
        for limit in [WORKSPACE_FILE_MAX, WORKSPACE_TOTAL_MAX] {
            assert!(
                description.contains(&format!("{limit} bytes")),
                "the model gets the live budget: {description}"
            );
        }
        assert!(!description.contains("2048 bytes"), "the obsolete file budget must not be advertised");

        // Reverse proof, without touching the build: one invented name is caught.
        let mut injected = prompt.clone();
        injected.push_str(" Then call `frobnicate`.");
        assert_eq!(unadvertised_tool_names(&injected, &offered), vec!["frobnicate".to_string()]);
    }

    /// Neither a tool nor a route.
    ///
    /// A restart ends the run that asked for it, and a factory reset wipes the board the
    /// caller is standing on. The platform still restarts itself when a configuration change
    /// needs one (`runtime.restart_requested`), but nobody can ask for either over a tool, a
    /// route or the console: they are operations on the machine, not on the workload.
    #[test]
    fn nobody_can_restart_or_reset_the_board() {
        let mut app = test_app();
        for name in ["reboot", "factory_reset"] {
            assert!(app.tools.get(name).is_none(), "{name} is still registered");
            assert!(!app.is_visible(name), "{name} is still offered");
            let err = app.call_tool(name, &json!({ "confirm": true })).unwrap_err();
            assert_eq!(err.code, code::NOT_FOUND, "{name}: {err:?}");
        }
        // Not a command of `board` either, so the family door does not reach it.
        let help = app.call_tool("board", &json!({ "command": "help" })).unwrap();
        let commands: Vec<String> =
            help["commands"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
        for name in ["reboot", "factory_reset"] {
            assert!(!commands.contains(&name.to_string()), "{name} is still a command: {commands:?}");
        }
        // And the routes that did the same thing are gone with them.
        for path in ["/api/reboot", "/api/factory-reset", "/api/wifi/forget"] {
            let mut req = post(path, "{}");
            req.method = "POST".into();
            assert_eq!(app.route(&req).status, 404, "{path} still answers");
        }
        assert!(!app.runtime.restart_requested, "nothing here may ask for a restart");
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

        // One source of truth: the same pool the status document lists, not a second measurement.
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
        assert_eq!(plan_sleep(false, "100", MAX_SHELL_SLEEP_MS).unwrap(), (100_000, MAX_SHELL_SLEEP_MS));
        assert_eq!(plan_sleep(false, "5", MAX_SHELL_SLEEP_MS).unwrap(), (5_000, 5_000));
        assert!(plan_sleep(false, "5", 3_000).is_ok());
        assert!(plan_sleep(true, "-1", 3_000).is_err());
        let err = ToolHost::call_tool(&mut app, "shell", &json!({"command": "sleep soon"})).unwrap_err();
        assert!(err.contains("usage: sleep <seconds>"), "{err}");
    }

    #[test]
    fn time_reports_how_long_a_command_took() {
        let mut app = test_app();
        let out = sh(&mut app, "time echo hi");
        assert_eq!(out["command"], json!("time echo hi"));
        assert_eq!(out["result"]["text"], json!("hi"), "the command still answers: {out}");
        assert!(out["ms"].as_u64().is_some(), "{out}");
        // It measures the whole line, so a pipeline gets one figure rather than one per stage.
        let piped = sh(&mut app, "time echo a | wc -l");
        assert_eq!(piped["result"]["pipeline"], json!(["echo", "wc"]), "{piped}");
        assert_eq!(piped["result"]["stdout"], json!("1"), "{piped}");
        // A prefix with nothing after it is a wrong line, not a silent no-op.
        let err = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "time" })).unwrap_err();
        assert!(err.contains("needs a command"), "{err}");
        // And `time` is not a command of its own.
        assert!(crate::tools::shell::route("time").is_none());
    }

    #[test]
    fn sleep_waits_against_the_tool_budget_not_a_cap_of_its_own() {
        let mut app = test_app();
        // One second for this call: a two-second wait is refused with the number it had.
        app.cfg.agent.tool_timeout_s = 1;
        let err = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sleep 2" })).unwrap_err();
        assert!(err.contains("more than the"), "{err}");
        assert!(err.contains("agent.tool_timeout_s"), "{err}");
        // Switching the deadline off brings back the old ten-second bound, and says which
        // one was in force instead of leaving the caller to guess.
        app.cfg.agent.tool_timeout_s = 0;
        let out = sh(&mut app, "usleep 1");
        assert_eq!(out["slept_ms"], json!(1), "{out}");
        assert_eq!(out["budget_ms"], json!(MAX_SHELL_SLEEP_MS));
        assert!(out["budget"].as_str().unwrap_or("").contains("fallback"));
    }

    #[test]
    fn a_script_draws_on_the_budget_of_the_call_that_started_it() {
        let mut app = test_app();
        app.cfg.agent.tool_timeout_s = 2;
        let err = ToolHost::call_tool(&mut app, "script", &json!({ "source": "sleep(3000)" })).unwrap_err();
        assert!(err.contains("sleep(3000)"), "{err}");
        assert!(err.contains("budget"), "{err}");
        let out: Value = serde_json::from_str(
            &ToolHost::call_tool(&mut app, "script", &json!({ "source": "sleep(1)\n\"ok\"" })).unwrap(),
        )
        .expect("script answers JSON");
        assert_eq!(out["result"], json!("ok"), "{out}");
        assert!(out["budget_ms"].as_u64().unwrap() <= 2_000, "{out}");
    }

    #[test]
    fn a_call_that_starts_after_its_deadline_is_refused_as_a_timeout() {
        let mut app = test_app();
        app.cfg.agent.tool_timeout_s = 30;
        app.tool_deadline = Some(std::time::Instant::now() - Duration::from_millis(1));
        let err = app.call_tool("board", &json!({"command": "status", "arguments": {}})).unwrap_err();
        assert_eq!(err.code, code::TIMEOUT, "{}", err.message);
        // The message names the time the call had, not the zero that is left of it, and it
        // says where the fix is: a nested call is one step of somebody else's budget.
        assert!(err.message.contains("used its whole 30 s budget"), "{}", err.message);
        let data = err.data.clone().unwrap_or(Value::Null);
        assert_eq!(data["nested"], json!(true), "{data}");
        assert!(data["suggestion"].as_str().unwrap_or("").contains("tool_timeout_s"), "{data}");
        // A top-level call leaves no deadline behind, so the next one starts fresh.
        app.tool_deadline = None;
        assert!(app.call_tool("board", &json!({"command": "status", "arguments": {}})).is_ok());
        assert!(app.tool_deadline.is_none(), "the deadline outlived its call");
    }

    #[test]
    fn a_call_that_overruns_its_budget_reports_it_without_hiding_what_it_did() {
        let mut app = test_app();
        let value = app.note_tool_overrun("slow", Ok(json!({ "ok": true })), Some(10), 40).unwrap();
        assert_eq!(value["ok"], json!(true), "an effect that happened is still reported");
        assert_eq!(value["deadline_exceeded"]["timeout_ms"], json!(10));
        assert_eq!(value["deadline_exceeded"]["elapsed_ms"], json!(40));
        let on_budget = app.note_tool_overrun("quick", Ok(json!({ "ok": true })), Some(40), 40).unwrap();
        assert!(on_budget.get("deadline_exceeded").is_none(), "{on_budget}");
    }

    #[test]
    fn clearing_refuses_while_a_model_turn_is_in_flight() {
        let mut app = test_app();
        assert_eq!(app.route(&post("/api/agent/reset", "{}")).status, 200, "nothing is running yet");
        app.run_gate.set_active(true);
        let busy = app.route(&post("/api/agent/reset", "{}"));
        assert_eq!(busy.status, 409, "a clear would drop the question being answered");
        let body = body_json(&busy);
        assert!(body["error"].as_str().unwrap_or("").contains("model turn is running"), "{body}");
        assert_eq!(app.route(&post("/api/agent/forget", r#"{"keep":1}"#)).status, 409);
        app.run_gate.set_active(false);
        assert_eq!(app.route(&post("/api/agent/reset", "{}")).status, 200);
    }

    /// A cancel that arrives before the first model call ends the turn there, and the model is
    /// never asked: the user said stop, and a request already in flight is the only thing this
    /// cannot withdraw.
    #[test]
    fn a_cancel_stops_the_turn_at_the_next_boundary_without_calling_the_model() {
        let mut app = test_app();
        app.queue_user("play something long", "http", 1_000);
        app.run_gate.set_active(true);
        assert!(app.request_cancel(), "a running turn can be cancelled");
        assert!(app.take_cancel(), "the flag is handed to the loop exactly once");
        assert!(!app.take_cancel(), "and is gone after that");

        let shared: Shared = Arc::new(Mutex::new(app));
        let mut model = Scripted::new(vec![Scripted::answer("never asked")]);
        // Re-arm inside the run: `drive_agent` takes the flag at the Ask boundary.
        assert!(app_lock(&shared).request_cancel());
        let out = drive_agent(&shared, &mut model).expect("the queued turn still ends");
        let err = out.expect_err("a cancelled turn reports why it stopped");
        assert!(err.contains("interrupted"), "{err}");
        assert!(model.seen.lock().unwrap().is_empty(), "the model was called after the cancel");
        let finished = app_lock(&shared);
        assert!(!finished.run_gate.active(), "the turn is over");
        assert!(!finished.run_gate.cancelled(), "the flag does not leak into the next turn");
    }

    /// Tools stop when the turn is cancelled: a script mid-flight must not keep drawing.
    #[test]
    fn a_cancelled_turn_refuses_further_tool_calls() {
        let mut app = test_app();
        app.tool_deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
        app.run_gate.set_cancelled(true);
        let err = app.call_tool("board", &json!({ "command": "status" })).unwrap_err();
        assert!(err.message.contains("interrupted"), "{}", err.message);
        assert_eq!(err.data.as_ref().unwrap()["interrupted"], json!(true));
        // The cancelled call installs no deadline of its own: the next turn starts clean.
        app.run_gate.set_cancelled(false);
        assert!(app.call_tool("board", &json!({ "command": "status" })).is_ok());
    }

    #[test]
    fn cancel_is_only_accepted_while_a_turn_is_running() {
        let mut app = test_app();
        assert!(!app.request_cancel(), "nothing to interrupt");
        app.run_gate.set_active(true);
        assert!(app.request_cancel());
        // The route reports what it did, so a console can tell a no-op from a stop.
        app.run_gate.set_active(false);
        app.run_gate.set_cancelled(false);
        let idle = body_json(&app.route(&post("/api/agent/cancel", "{}")));
        assert_eq!(idle["cancelling"], json!(false), "{idle}");
        app.run_gate.set_active(true);
        let busy = body_json(&app.route(&post("/api/agent/cancel", "{}")));
        assert_eq!(busy["cancelling"], json!(true), "{busy}");
        assert_eq!(busy["run_active"], json!(true), "{busy}");
    }

    #[test]
    fn usleep_waits_in_milliseconds() {
        let mut app = test_app();
        let out = sh(&mut app, "usleep 1");
        assert_eq!(out["slept_ms"], json!(1), "{out}");
        assert_eq!(out["requested_ms"], json!(1));
        assert_eq!(plan_sleep(true, "999999", MAX_SHELL_SLEEP_MS).unwrap(), (999_999, MAX_SHELL_SLEEP_MS));
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
        assert_eq!(set["saved"], json!(false), "queue admission is not a durable acknowledgement");
        assert_eq!(set["queued"], json!(true));
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

    /// `remaining_ms` has to follow the clock, not the last thing the writer published.
    ///
    /// The board reported "259 ms left" on a 4 second tune because the number was written
    /// once per chunk and left stale in between — and worse, a queued job could leave the
    /// previous job's number behind. The state is now derived at read time, so two reads a
    /// known interval apart must differ by that interval.
    #[test]
    fn the_reported_remaining_time_follows_the_clock() {
        let mut hw = crate::virtual_board::VirtualBoard::new();
        // 1 second at 24 kHz.
        let samples = vec![0i16; 24_000];
        let job = crate::hardware::Hardware::media_submit(&mut hw, &samples, 24_000).unwrap();
        assert!(job >= 1);

        let first = crate::hardware::Hardware::media_state(&mut hw);
        assert!(first.playing, "{first:?}");
        assert_eq!(first.duration_ms, 1000);
        std::thread::sleep(std::time::Duration::from_millis(180));
        let second = crate::hardware::Hardware::media_state(&mut hw);
        let fell = first.remaining_ms.saturating_sub(second.remaining_ms);
        assert!(
            (120..=400).contains(&fell),
            "remaining went from {} to {} in 180 ms",
            first.remaining_ms,
            second.remaining_ms
        );

        // A job queued behind it must not rewrite the playing job's numbers.
        let second_job = crate::hardware::Hardware::media_submit(&mut hw, &vec![0i16; 48_000], 24_000);
        assert!(second_job.is_ok());
        let third = crate::hardware::Hardware::media_state(&mut hw);
        assert_eq!(
            third.duration_ms, 1000,
            "the queued 2 s job overwrote the 1 s job that is playing: {third:?}"
        );
    }

    #[test]
    fn shell_sh_runs_a_script_file_with_arguments() {
        let mut app = empty_workspace_app();
        write_file(&mut app, "/hello.bh", "log(\"hi \" + str(args.who))\n21 * 2");
        let raw = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh /hello.bh --who world" }))
            .expect("sh runs the file");
        let out: Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(out["state"], "done", "{out}");
        assert_eq!(out["result"], 42);
        assert_eq!(out["output"], json!(["hi world"]));
        assert_eq!(out["path"], "/hello.bh");

        // A missing file is an error about the file, not a silent no-op.
        let missing = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh /nope.bh" }));
        assert!(
            missing.is_err()
                || serde_json::from_str::<Value>(&missing.unwrap()).unwrap()["state"] == "failed"
        );
    }

    #[test]
    fn shell_sh_reports_a_failing_script_as_a_failed_job_not_a_crash() {
        let mut app = empty_workspace_app();
        write_file(&mut app, "/bad.bh", "1 +");
        let raw = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh /bad.bh" })).unwrap();
        let out: Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(out["state"], "failed");
        assert!(
            out["error"].as_str().unwrap().contains("unexpected token"),
            "the parse error travels with the job: {out}"
        );
        assert!(out["result"].is_null());
        // The interpreter is usable again afterwards: a syntax error is not a broken tool.
        write_file(&mut app, "/good.bh", "6 * 7");
        let raw = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh /good.bh" })).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["result"], 42);
    }

    #[test]
    fn shell_sh_passes_arguments_as_args_and_argv() {
        let mut app = empty_workspace_app();
        write_file(&mut app, "/args.bh", "str(args.who) + \"/\" + str(len(argv))");
        let raw =
            ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh /args.bh --who ada extra" }))
                .unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        // `--who ada` becomes one named argument; the bare word stays positional, so it lands
        // in `argv` next to the flag's value and the path is consumed as the file.
        assert_eq!(out["state"], "done", "{out}");
        assert_eq!(out["result"], "ada/3", "{out}");
    }

    #[test]
    fn shell_sh_reads_the_file_once_and_a_missing_one_names_the_path() {
        let mut app = empty_workspace_app();
        let raw = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh /ghost.bh" })).unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(out["state"], "failed");
        assert!(out["error"].as_str().unwrap().contains("/ghost.bh"), "the refusal names the path: {out}");
        // `sh` with no argument is a usage error, not a silent success.
        let err = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh" })).unwrap_err();
        assert!(err.contains("usage: sh <path>"), "{err}");
    }

    #[test]
    fn shell_sh_runs_a_script_that_calls_tools_and_returns_their_shape() {
        let mut app = empty_workspace_app();
        write_file(
            &mut app,
            "/scan.bh",
            "let d = tool(\"devices_list\", {})\nlet n = len(d.devices)\nlog(\"seen \" + str(n))\nn",
        );
        let raw = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh /scan.bh" })).unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(out["state"], "done", "{out}");
        assert!(out["result"].as_u64().is_some(), "the last expression is the result: {out}");
        assert_eq!(out["output"].as_array().unwrap().len(), 1, "{out}");
        assert!(out["output"][0].as_str().unwrap().starts_with("seen "), "{out}");
        assert!(out["steps"].as_u64().unwrap() > 0);
    }

    #[test]
    fn a_script_cannot_reach_the_interpreter_through_shell_sh() {
        let mut app = empty_workspace_app();
        write_file(&mut app, "/inner.bh", "1 + 1");
        // The second way into the interpreter: the script host refuses `script`, and this
        // is the same guard for `shell sh`.
        let out = ToolHost::call_tool(
            &mut app,
            "script",
            &json!({ "source": "tool(\"shell\", {command: \"sh /inner.bh\"})" }),
        );
        let reply = match out {
            Ok(v) => v.to_string(),
            Err(e) => e,
        };
        assert!(reply.contains("may not run another script"), "{reply}");
        // And the flag is cleared, so the next ordinary `sh` still works.
        let raw = ToolHost::call_tool(&mut app, "shell", &json!({ "command": "sh /inner.bh" })).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["result"], 2);
    }

    #[test]
    fn a_script_defers_tool_calls_and_collects_them_without_running_twice() {
        let mut app = empty_workspace_app();
        let source = r#"
let first = async_tool("schedule", {action: "add", id: "deferred-a", prompt: "x", every_s: 86400})
let queued = task_state(first)
let a = await(first)
let again = await(first)
let second = async_tool("schedule", {action: "add", id: "deferred-b", prompt: "y", every_s: 86400})
str(queued) + "/" + str(a.ok) + "/" + str(again.ok) + "/" + str(task_state(second))
"#;
        let job = app.submit_script(source);
        assert_eq!(job.state, "done", "{job:?}");
        assert_eq!(job.result, "queued/true/true/queued", "{:?}", job.output);
        // The first call ran once: a second execution would have been refused as a duplicate
        // task id, and `again.ok` would be false.
        let sched = body_json(&app.route(&get("/api/sched")));
        let ids: Vec<&str> =
            sched["tasks"].as_array().unwrap().iter().filter_map(|t| t["id"].as_str()).collect();
        assert!(ids.contains(&"deferred-a"), "{ids:?}");
        // The second was never awaited, so the drain at the end of the script ran it.
        assert!(ids.contains(&"deferred-b"), "a fire-and-forget call must still happen: {ids:?}");
        assert_eq!(ids.iter().filter(|id| **id == "deferred-a").count(), 1);
    }

    #[test]
    fn a_deferred_failure_is_reported_in_the_output_rather_than_vanishing() {
        let mut app = empty_workspace_app();
        let job = app.submit_script(
            "let h = async_tool(\"board\", {command: \"no_such_command\"})\n\"script finished\"",
        );
        // A tool error is not a script error unless it was awaited: the script's own result
        // stands, and the failure is written where a reader will see it.
        assert_eq!(job.state, "done", "{job:?}");
        assert_eq!(job.result, "script finished");
        assert!(job.output.iter().any(|l| l.contains("deferred board failed")), "{:?}", job.output);
    }

    #[test]
    fn run_now_reports_a_script_task_and_shell_sh_share_one_interpreter_path() {
        let mut app = empty_workspace_app();
        write_file(&mut app, "/count.bh", "len(argv)");
        let one = app.submit_script_call(&json!({ "path": "/count.bh", "args": ["--a", "1"] }));
        assert_eq!(one.unwrap().result, 2, "one flag pair becomes one named argument");
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

    /// A redirect shares the file/aggregate workspace limits and reports the requested
    /// size when it cannot fit.
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
        assert_eq!(resp.status, 200, "a board without a PIN keeps the stream open");
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
        let mut app = shared.lock().unwrap();
        app.cfg.auth.portal_pin = "1234".into();
        assert_eq!(app.route(&get("/api/agent/stream")).status, 401);
        assert_eq!(app.active_event_streams(), 0, "a refusal must not acquire a stream slot");
        let token = body_json(&app.route(&post("/api/auth", r#"{"pin":"1234"}"#)))["token"]
            .as_str()
            .unwrap()
            .to_string();
        let mut req = get("/api/agent/stream");
        req.query.insert("token".into(), token);
        assert_eq!(app.route(&req).status, 200, "EventSource can use a minted portal token");
        req.query.insert("token".into(), "1234".into());
        assert_eq!(app.route(&req).status, 401, "the token parameter must not accept a raw PIN");
    }

    #[test]
    fn the_stream_replays_after_the_given_seq_and_then_follows_live() {
        let shared = shared_app();
        {
            let mut app = shared.lock().unwrap();
            app.agent.events.push_back(journal_event(1, "message_start", ""));
            app.agent.events.push_back(journal_event(2, "tool_use", "board"));
            app.agent.events.push_back(journal_event(3, "tool_result", "board"));
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
        shared.lock().unwrap().agent.events.push_back(journal_event(1, "tool_use", "board"));
        let body = read_until(&mut first, "id: 1");
        assert!(body.contains("event: tool_use"), "got: {body}");
        assert!(body.contains("\"name\":\"board\""), "got: {body}");

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
        assert_eq!(saved["saved"], false, "a queued request cannot acknowledge durable storage");
        assert_eq!(saved["queued"], true);
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
        for path in ["/api/agent/events", "/api/agent/stream", "/api/logs"] {
            assert_eq!(app.route(&get(path)).status, 401, "{path} exposes conversation or board output");
        }
        assert_eq!(
            app.route(&post("/mcp", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)).status,
            401,
            "an unset MCP token must not bypass the configured portal PIN"
        );
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
        for path in ["/api/agent/events", "/api/logs"] {
            read.path = path.into();
            assert_eq!(app.route(&read).status, 200, "{path}");
        }
        let mut mcp = post("/mcp", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        mcp.headers.insert("authorization".into(), format!("Bearer {token}"));
        assert_eq!(
            app.route(&mcp).status,
            200,
            "a portal credential works when no dedicated MCP token is set"
        );
        read = get("/api/config");
        read.query.insert("token".into(), token.clone());
        assert_eq!(app.route(&read).status, 200);
        let mut query_write = post("/api/agent/reset", "{}");
        query_write.query.insert("token".into(), token);
        assert_eq!(app.route(&query_write).status, 401, "URL tokens authorize reads only");

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
    fn schedule_tool_and_rest_share_validation_limits_and_unique_ids() {
        let mut app = test_app();
        let first = app.call_tool("schedule", &json!({"action":"add","prompt":"one","every_s":60})).unwrap();
        let second = app.call_tool("schedule", &json!({"action":"add","prompt":"two","every_s":60})).unwrap();
        app.call_tool("schedule", &json!({"action":"remove","id":first["id"]})).unwrap();
        let third =
            app.call_tool("schedule", &json!({"action":"add","prompt":"three","every_s":60})).unwrap();
        assert_ne!(second["id"], third["id"]);
        for task in [
            json!({"prompt":"x"}),
            json!({"prompt":"x","at_hhmm":"25:00"}),
            json!({"prompt":"x","at_hhmm":"+1:00"}),
            json!({"prompt":"x","every_s":1.5}),
            json!({"prompt":"x","every_s":true}),
            json!({"prompt":"x","every_s":-1}),
            json!({"prompt":"x","every_s":86401}),
            json!({"prompt":"x","every_s":60,"enabled":"yes"}),
        ] {
            let before = app.tasks.clone();
            let mut tool = task.clone();
            tool["action"] = json!("add");
            assert_eq!(app.call_tool("schedule", &tool).unwrap_err().code, code::INVALID_PARAMS);
            let body = json!({"kind":"task","op":"add","task":task});
            assert_eq!(app.route(&post("/api/sched", &body.to_string())).status, 400);
            assert_eq!(app.tasks, before, "refusal must not mutate tasks");
        }
        while app.tasks.len() < MAX_TASKS {
            app.call_tool("schedule", &json!({"action":"add","prompt":"slot","every_s":60})).unwrap();
        }
        let before = app.tasks.clone();
        assert!(app.call_tool("schedule", &json!({"action":"add","prompt":"ninth","every_s":60})).is_err());
        assert_eq!(
            app.route(&post(
                "/api/sched",
                r#"{"kind":"task","task":{"id":"ninth","prompt":"x","every_s":60}}"#
            ))
            .status,
            400
        );
        assert_eq!(app.tasks, before);
        assert_eq!(
            app.tasks.iter().map(|t| &t.id).collect::<std::collections::BTreeSet<_>>().len(),
            MAX_TASKS
        );
    }

    /// A one-shot task: the caller names it, it fires once on its own clock and then it is gone
    /// — and a caller that asks for "in ten seconds" and "every ten seconds" at once is told
    /// to pick one instead of silently getting a task that repeats forever.
    #[test]
    fn a_one_shot_task_fires_once_and_removes_itself() {
        let mut app = test_app();
        let added = app
            .call_tool("schedule", &json!({"action":"add","id":"later","prompt":"say hi","after_s":5}))
            .unwrap();
        assert_eq!(added["id"], json!("later"), "the caller's id was not kept: {added}");
        assert_eq!(added["one_shot"], json!(true), "{added}");

        // The tool stamps the task's clock now, so a tick at the current instant must not fire it.
        let now = crate::app::now_ms();
        assert!(app.sched_tick(now).is_empty(), "fired in the turn that created it");
        let fired = app.sched_tick(now + 6_000);
        assert_eq!(fired, vec!["schedule:later".to_string()]);
        assert!(
            app.tasks.iter().all(|t| t.id != "later"),
            "a one-shot task stayed behind and would fire again: {:?}",
            app.tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>()
        );

        // Refusals: a duplicate id, a one-shot combined with a repeat, and an unaddressable id.
        app.call_tool("schedule", &json!({"action":"add","id":"taken","prompt":"x","every_s":60})).unwrap();
        for bad in [
            json!({"action":"add","id":"taken","prompt":"y","every_s":60}),
            json!({"action":"add","prompt":"y","after_s":30,"every_s":60}),
            json!({"action":"add","prompt":"y","after_s":30,"at_hhmm":"07:30"}),
            json!({"action":"add","prompt":"y","after_s":0}),
            json!({"action":"add","id":"two words","prompt":"y","every_s":60}),
        ] {
            let before = app.tasks.clone();
            let err = app.call_tool("schedule", &bad).unwrap_err();
            assert_eq!(err.code, code::INVALID_PARAMS, "{bad}: {err:?}");
            assert_eq!(app.tasks, before, "{bad} mutated the schedule");
        }
    }

    #[test]
    fn schedule_replacement_is_atomic_on_invalid_members_duplicates_and_caps() {
        let mut app = test_app();
        let task = json!({"id":"t1","prompt":"keep task","every_s":60});
        let rule = json!({"id":"r1","tool":"bme280_read","prompt":"keep rule"});
        let body = json!({"tasks":[task.clone()],"rules":[rule.clone()]});
        assert_eq!(app.route(&post("/api/sched", &body.to_string())).status, 200);
        let before = (app.tasks.clone(), app.rules.clone(), app.store.get(NVS_KEY_SCHED));
        for invalid in [
            json!({"tasks":[],"rules":[{"tool":"bme280_read"}]}),
            json!({"tasks":[task.clone(),task.clone()],"rules":[]}),
            json!({"tasks":[],"rules":[rule.clone(),rule.clone()]}),
            json!({"tasks":[{"prompt":"bad","at_hhmm":"24:00"}],"rules":[]}),
            json!({"tasks":[{"prompt":"bad","every_s":true}],"rules":[]}),
            json!({"tasks":[{"prompt":"bad","every_s":60,"last_run_ms":"0"}],"rules":[]}),
            json!({"tasks":[],"rules":[{"tool":"bme280_read","prompt":"bad","op":"approx"}]}),
            json!({"tasks":[],"rules":[{"tool":"bme280_read","prompt":"bad","min_interval_s":-1}]}),
            json!({"tasks":vec![task.clone();MAX_TASKS+1],"rules":[]}),
            json!({"tasks":[],"rules":vec![rule.clone();MAX_RULES+1]}),
            json!({"tasks":{},"rules":[]}),
            json!({"tasks":[]}),
            json!({"tasks":[],"rules":[],"kind":"task"}),
            json!({"tasks":[17],"rules":[]}),
        ] {
            assert_eq!(app.route(&post("/api/sched", &invalid.to_string())).status, 400, "{invalid}");
            assert_eq!((app.tasks.clone(), app.rules.clone(), app.store.get(NVS_KEY_SCHED)), before);
        }
    }

    #[test]
    fn schedule_replacement_preserves_clocks_defaults_and_existing_mutations() {
        let mut app = test_app();
        let body = json!({"tasks":[{"id":"t1","prompt":"first","every_s":60,"last_run_ms":123}],
            "rules":[{"id":"r1","tool":"bme280_read","prompt":"rule","last_fire_ms":456}]});
        assert_eq!(app.route(&post("/api/sched", &body.to_string())).status, 200);
        let replacement = json!({"tasks":[{"prompt":"generated","at_hhmm":"07:30"},
            {"id":"t1","script":"1","every_s":"60"}],
            "rules":[{"id":"r1","tool":"bme280_read","prompt":"new rule"}]});
        assert_eq!(app.route(&post("/api/sched", &replacement.to_string())).status, 200);
        assert_ne!(app.tasks[0].id, "t1", "generated ids reserve later explicit ids");
        assert!(app.tasks[0].last_run_ms > 1_000_000_000_000);
        assert_eq!(app.tasks[1].last_run_ms, 123);
        assert_eq!(app.rules[0].last_fire_ms, 456);
        assert!(app.tasks.iter().all(|t| t.enabled));
        assert!(app.rules[0].enabled);
        assert_eq!(app.rules[0].min_interval_s, 60);
        assert_eq!(app.rules[0].op, "true");
        assert_eq!(app.route(&post("/api/sched", r#"{"kind":"task","op":"disable","id":"t1"}"#)).status, 200);
        assert!(!app.tasks[1].enabled);
        let body = json!({"tasks":[{"id":"t1","prompt":"changed","every_s":60,"last_run_ms":789}],
            "rules":[{"id":"r1","tool":"bme280_read","prompt":"changed","last_fire_ms":987}]});
        assert_eq!(app.route(&post("/api/sched", &body.to_string())).status, 200);
        assert_eq!(app.tasks[0].last_run_ms, 789);
        assert_eq!(app.rules[0].last_fire_ms, 987);
        let stored: Value = serde_json::from_str(&app.store.get(NVS_KEY_SCHED).unwrap()).unwrap();
        assert_eq!(stored["tasks"][0]["last_run_ms"], 789);
        assert_eq!(app.route(&post("/api/sched", r#"{"tasks":[],"rules":[]}"#)).status, 200);
        assert!(app.tasks.is_empty() && app.rules.is_empty());
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
        let text =
            ToolHost::call_tool(&mut app, "board", &json!({"command": "status", "arguments": {}})).unwrap();
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
                    {"id":"call_1","type":"function","function":{"name":"board","arguments":"{\"command\":\"status\"}"}}
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

        // Exercise the whole canned chain against the default advertised profile.
        // The original alias-only fixture above remains a compatibility check.
        for ask in ["status", "write a file", "edit a file", "mcp"] {
            let mut app = test_app();
            let mut messages = vec![json!({"role": "user", "content": ask})];
            let mut calls = 0;
            loop {
                let request = json!({"model": "selftest", "messages": messages, "tools": app.tool_specs()});
                let reply = selftest_completion(&post("/selftest/chat/completions", &request.to_string()));
                let message = &reply["choices"][0]["message"];
                let Some(call) = message["tool_calls"].as_array().and_then(|calls| calls.first()) else {
                    assert_eq!(reply["choices"][0]["finish_reason"], "stop");
                    assert!(
                        !message["content"].as_str().unwrap_or("").contains("not on the tool list"),
                        "{ask}: {reply}"
                    );
                    break;
                };
                let name = call["function"]["name"].as_str().unwrap();
                assert!(app
                    .tool_specs()
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["function"]["name"] == name));
                let args: Value =
                    serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
                let result = app
                    .call_tool(name, &args)
                    .unwrap_or_else(|error| panic!("{ask}: {name} {args}: {error}"));
                messages.push(message.clone());
                messages
                    .push(json!({"role": "tool", "tool_call_id": call["id"], "content": result.to_string()}));
                calls += 1;
                assert!(calls <= 6, "the canned script must finish");
            }
            assert_eq!(
                calls,
                match ask {
                    "status" => 1,
                    "mcp" => 6,
                    _ => 3,
                },
                "{ask}"
            );
        }
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
        assert!(
            app.store.get(NVS_KEY_PENDING_MESSAGE).is_some(),
            "an unstarted retry re-persists the question"
        );
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

    fn workspace_document_refused(doc: &Value) {
        assert!(AppWorkspace::from_json(doc).is_err(), "Value loader accepted an invalid workspace");
        let encoded = serde_json::to_vec(doc).unwrap();
        assert!(
            AppWorkspace::from_reader(encoded.as_slice()).is_err(),
            "stream loader accepted an invalid workspace"
        );
    }

    fn workspace_snapshot(store: &SharedStore) -> std::collections::BTreeMap<String, String> {
        let memory = store.0.lock().unwrap();
        memory
            .keys()
            .into_iter()
            .filter(|key| key == "ws" || key.starts_with("ws."))
            .map(|key| {
                let value = memory.get(&key).expect("a listed key is present under the store lock");
                (key, value)
            })
            .collect()
    }

    struct WorkspaceCountedRead<R> {
        inner: R,
        bytes: std::rc::Rc<std::cell::Cell<usize>>,
        pull: usize,
    }

    impl<R: std::io::Read> std::io::Read for WorkspaceCountedRead<R> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let want = out.len().min(self.pull);
            let count = self.inner.read(&mut out[..want])?;
            self.bytes.set(self.bytes.get() + count);
            Ok(count)
        }
    }

    /// Reads without cloning values, and can refuse a key after a partial successful read.
    struct WorkspaceReadStore {
        inner: SharedStore,
        reads: Arc<Mutex<Vec<String>>>,
        failure: Option<(String, usize)>,
    }

    impl KvStore for WorkspaceReadStore {
        fn get(&self, key: &str) -> Option<String> {
            self.reads.lock().unwrap().push(key.to_string());
            self.inner.get(key)
        }
        fn set(&mut self, key: &str, value: &str) -> bool {
            self.inner.set(key, value)
        }
        fn remove(&mut self, key: &str) -> bool {
            self.inner.remove(key)
        }
        fn keys(&self) -> Vec<String> {
            self.inner.keys()
        }
        fn whole_value_bytes(&self) -> Option<usize> {
            self.inner.whole_value_bytes()
        }
        fn read_at(&self, key: &str, offset: usize, out: &mut [u8]) -> Option<usize> {
            self.reads.lock().unwrap().push(key.to_string());
            let until = self
                .failure
                .as_ref()
                .filter(|(failed, _)| failed == key)
                .map_or(usize::MAX, |(_, after)| *after);
            if offset >= until {
                return None;
            }
            let count = out.len().min(until - offset);
            self.inner.read_at(key, offset, &mut out[..count])
        }
    }

    #[test]
    fn workspace_restore_enforces_the_existing_text_budgets() {
        workspace_document_refused(
            &json!({"version": 1, "files": {"/big": "x".repeat(WORKSPACE_FILE_MAX + 1)}}),
        );
        let mut files = serde_json::Map::new();
        for n in 0..16 {
            files.insert(format!("/f{n}"), json!("x".repeat(WORKSPACE_FILE_MAX)));
        }
        let full = json!({"version": 1, "files": files});
        assert_eq!(AppWorkspace::from_json(&full).unwrap().bytes(), WORKSPACE_TOTAL_MAX);
        let encoded = serde_json::to_vec(&full).unwrap();
        assert_eq!(AppWorkspace::from_reader(encoded.as_slice()).unwrap().bytes(), WORKSPACE_TOTAL_MAX);
        let mut over = full;
        over["files"]["/one-more-byte"] = json!("x");
        workspace_document_refused(&over);
    }

    #[test]
    fn workspace_restore_and_writes_bound_names_and_file_count() {
        let mut files = serde_json::Map::new();
        for n in 0..10_000 {
            files.insert(format!("/empty-{n}"), json!(""));
        }
        workspace_document_refused(&json!({"version": 1, "files": files}));

        let mut ws = AppWorkspace::default();
        for n in 0..WORKSPACE_FILES_MAX {
            fs_tools::Workspace::write(&mut ws, &format!("/e{n:04}"), "").unwrap();
        }
        assert_eq!(ws.bytes(), 0);
        assert!(fs_tools::Workspace::write(&mut ws, "/extra", "").is_err());
        fs_tools::Workspace::write(&mut ws, "/e0000", "replacement").unwrap();
        assert_eq!(ws.len(), WORKSPACE_FILES_MAX);
        fs_tools::Workspace::remove(&mut ws, "/e0000").unwrap();
        fs_tools::Workspace::write(&mut ws, "/extra", "").unwrap();

        let mut names = AppWorkspace::default();
        for n in 0..(WORKSPACE_NAMES_MAX / WORKSPACE_PATH_MAX) {
            let path = format!("/n{n:04}{}", "p".repeat(WORKSPACE_PATH_MAX - 6));
            fs_tools::Workspace::write(&mut names, &path, "").unwrap();
        }
        assert_eq!(names.files.keys().map(String::len).sum::<usize>(), WORKSPACE_NAMES_MAX);
        assert!(fs_tools::Workspace::write(&mut names, "/extra", "").is_err());
        let mut over = names.to_json();
        over["files"]["/extra"] = json!("");
        workspace_document_refused(&over);
    }

    #[test]
    fn workspace_paths_share_normalization_and_directory_rules() {
        let mut ws = AppWorkspace::default();
        fs_tools::Workspace::write(&mut ws, " notes//./a/ ", "hello").unwrap();
        assert_eq!(fs_tools::Workspace::read(&ws, "/notes/a").unwrap(), "hello");
        assert_eq!(fs_tools::Workspace::read(&ws, "notes/a").unwrap(), "hello");
        for path in ["/", " ", "/a/../b", "/a\\b", "/a\0b"] {
            assert!(fs_tools::Workspace::write(&mut ws, path, "").is_err(), "accepted {path:?}");
            assert!(fs_tools::Workspace::read(&ws, path).is_err(), "read {path:?}");
            assert!(fs_tools::Workspace::remove(&mut ws, path).is_err(), "removed {path:?}");
        }
        assert!(fs_tools::Workspace::list(&ws, "/").is_ok());
        assert!(fs_tools::Workspace::write(&mut ws, "/notes", "file over directory").is_err());
        assert!(fs_tools::Workspace::write(&mut ws, "/notes/a/b", "directory under file").is_err());
        let enormous = format!("/{}", "p".repeat(WORKSPACE_TOTAL_MAX));
        assert!(fs_tools::Workspace::write(&mut ws, &enormous, "").is_err());
        let exact = format!("/{}", "p".repeat(WORKSPACE_PATH_MAX - 1));
        fs_tools::Workspace::write(&mut ws, &exact, "").unwrap();
        assert!(fs_tools::Workspace::write(&mut ws, &(exact + "p"), "").is_err());
        let relative = json!({"files": {"relative": "readable"}});
        assert_eq!(
            fs_tools::Workspace::read(&AppWorkspace::from_json(&relative).unwrap(), "/relative").unwrap(),
            "readable"
        );
    }

    #[test]
    fn workspace_restore_rejects_bad_shapes_and_canonical_collisions() {
        for bad in [
            json!(null),
            json!([]),
            json!({"files": null}),
            json!({"files": []}),
            json!({"files": {"/x": 7}}),
            json!({"version": "1"}),
            json!({"version": 1.0}),
            json!({"version": 2}),
            json!({"version": true}),
            json!({"unknown": {}}),
            json!({"files": {"/": "root"}}),
            json!({"files": {"relative": "hidden", "/a/../b": "hidden"}}),
            json!({"files": {"/a": "one", "a": "two"}}),
            json!({"files": {"/a/b": "one", "/a//b": "two"}}),
            json!({"files": {"/a": "file", "/a/b": "child"}}),
        ] {
            workspace_document_refused(&bad);
        }
        for raw in [
            r#"{"version":1,"version":1}"#,
            r#"{"files":{},"files":{}}"#,
            r#"{"files":{"/a":"one","/a":"two"}}"#,
            r#"{"files":{},}"#,
            r#"{"files":{"/a":"x",}}"#,
            r#"{"files":{}} {}"#,
        ] {
            assert!(AppWorkspace::from_reader(raw.as_bytes()).is_err(), "accepted {raw}");
        }
        for valid in [json!({}), json!({"version": 1}), json!({"files": {}})] {
            assert!(AppWorkspace::from_json(&valid).unwrap().is_empty());
            assert!(AppWorkspace::from_reader(valid.to_string().as_bytes()).unwrap().is_empty());
        }
    }

    #[test]
    fn workspace_stream_parser_agrees_with_json_string_decoding() {
        let raw = br#"{"files":{"/\u754c":"ASCII \" \\ \/ \b \f \n \r \t \u0000 \uD83D\uDE00"},"version":1}"#;
        let expected: Value = serde_json::from_slice(raw).unwrap();
        let counter = std::rc::Rc::new(std::cell::Cell::new(0));
        let reader = WorkspaceCountedRead { inner: &raw[..], bytes: counter, pull: 1 };
        let ws = AppWorkspace::from_reader(reader).unwrap();
        assert_eq!(
            fs_tools::Workspace::read(&ws, "/界").unwrap(),
            expected["files"]["/界"].as_str().unwrap()
        );
        let unicode = json!({"version": 1, "files": {"/日本語.md": "界 한글 😀"}});
        assert_eq!(AppWorkspace::from_reader(unicode.to_string().as_bytes()).unwrap().to_json(), unicode);
        for invalid in [
            &br#"{"files":{"/a":"\q"}}"#[..],
            &br#"{"files":{"/a":"\uD800"}}"#[..],
            &br#"{"files":{"/a":"\uDC00"}}"#[..],
            &br#"{"files":{"/a":"\uD800\u0041"}}"#[..],
            &br#"{"files":{"/a":"\uZZZZ"}}"#[..],
            &b"{\"files\":{\"/a\":\"\xff\"}}"[..],
            &b"{\"files\":{\"/a\":\"unescaped\nnewline\"}}"[..],
        ] {
            assert!(AppWorkspace::from_reader(invalid).is_err());
        }
    }

    #[test]
    fn workspace_stream_parser_stops_before_unbounded_strings_are_materialized() {
        use std::io::Read as _;
        for (prefix, limit) in [
            (br#"{"files":{"/a":""#.as_slice(), WORKSPACE_FILE_MAX),
            (br#"{"files":{""#.as_slice(), WORKSPACE_RAW_PATH_MAX),
        ] {
            let count = std::rc::Rc::new(std::cell::Cell::new(0));
            let reader = WorkspaceCountedRead {
                inner: std::io::Cursor::new(prefix).chain(std::io::repeat(b'x')),
                bytes: count.clone(),
                pull: usize::MAX,
            };
            let error = AppWorkspace::from_reader(reader).unwrap_err();
            assert!(error.to_string().contains("limit"), "{error}");
            println!(
                "decoded string limit={limit}: stopped after {} input bytes including read-ahead",
                count.get()
            );
            assert!(count.get() <= prefix.len() + limit + 1 + 8192, "drained {} bytes", count.get());
        }
        let prefix = br#"{"files":{"/a":""#;
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        let escapes = std::io::Cursor::new(br#"\u0061"#.repeat(WORKSPACE_FILE_MAX + 10_000));
        let reader = WorkspaceCountedRead {
            inner: std::io::Cursor::new(prefix).chain(escapes),
            bytes: count.clone(),
            pull: usize::MAX,
        };
        assert!(AppWorkspace::from_reader(reader).is_err());
        assert!(count.get() <= prefix.len() + 6 * (WORKSPACE_FILE_MAX + 1) + 8192);
    }

    #[test]
    fn workspace_ram_estimate_counts_empty_files_and_saved_paths() {
        let mut ws = AppWorkspace::default();
        for n in 0..WORKSPACE_FILES_MAX {
            fs_tools::Workspace::write(&mut ws, &format!("/empty-{n}"), "").unwrap();
        }
        assert_eq!(ws.bytes(), 0);
        assert!(ws.ram_bytes() >= WORKSPACE_FILES_MAX * WORKSPACE_ENTRY_RAM);
        assert!(workspace_fits(&ws, Some(64 * 1024)).is_err());
        let before = ws.ram_bytes();
        ws.saved = ws.files.keys().map(|path| (path.clone(), 0)).collect();
        assert!(ws.ram_bytes() >= before + WORKSPACE_FILES_MAX * WORKSPACE_ENTRY_RAM);
        assert!(workspace_fits(&ws, Some(2 * ws.ram_bytes() + 64 * 1024)).is_ok());
    }

    #[test]
    fn workspace_rejected_documents_survive_boot_seeding_and_later_edits() {
        for doc in [
            "{torn".to_string(),
            json!({"version": 2, "files": {}}).to_string(),
            json!({"version": 1, "files": {"/big": "x".repeat(WORKSPACE_FILE_MAX + 1)}}).to_string(),
        ] {
            let store = SharedStore::new();
            store.clone().set("ws", &doc);
            let before = workspace_snapshot(&store);
            let mut app = app_with(&store);
            assert!(app.workspace.is_empty());
            assert!(fs_tools::Workspace::write(&mut app.workspace, "/new", "no overwrite").is_err());
            assert!(fs_tools::Workspace::remove(&mut app.workspace, "/new").is_err());
            drain_store(&mut app);
            assert_eq!(workspace_snapshot(&store), before);
        }
    }

    #[test]
    fn workspace_rejected_chunked_snapshots_are_preserved() {
        let mut memory = MemoryStore::new();
        store::write_blob(&mut memory, "ws", r#"{"version":2,"files":{}}"#, WORKSPACE_PLAN).unwrap();
        for corrupt in [false, true] {
            let store = SharedStore::new();
            for key in memory.keys() {
                store.clone().set(&key, &memory.get(&key).unwrap());
            }
            if corrupt {
                store.clone().set("ws.1", "different bytes");
            }
            let before = workspace_snapshot(&store);
            let mut app = app_with(&store);
            assert!(app.workspace.load_refused.is_some());
            drain_store(&mut app);
            assert_eq!(workspace_snapshot(&store), before);
        }
    }

    #[test]
    fn workspace_index_validates_every_name_and_key_before_content_reads() {
        let good = crate::keys::workspace_file("/good");
        let cases = [
            json!({"/good": good, "/wrong": "other"}),
            json!({"/a": crate::keys::workspace_file("/a"), "a": crate::keys::workspace_file("/a")}),
            json!({"/a": crate::keys::workspace_file("/a"), "/a/b": crate::keys::workspace_file("/a/b")}),
            json!({"/a/../b": "other"}),
            json!({"/": "other"}),
        ];
        for index in cases {
            let store = SharedStore::new();
            store.clone().set(crate::keys::WORKSPACE_INDEX, &index.to_string());
            store.clone().set(&good, "must not be read before the index is valid");
            store.clone().set("other", "must never be fetched as workspace data");
            let before = workspace_snapshot(&store);
            let reads = Arc::new(Mutex::new(Vec::new()));
            let backend = WorkspaceReadStore { inner: store.clone(), reads: reads.clone(), failure: None };
            let mut app = App::new(
                Box::new(VirtualBoard::new()),
                Box::new(backend),
                LogBuf::new(64),
                FwMeta::default(),
            );
            assert!(app.workspace.load_refused.is_some());
            assert!(!reads.lock().unwrap().iter().any(|key| key == "other" || key == &good));
            drain_store(&mut app);
            assert_eq!(workspace_snapshot(&store), before);
        }
    }

    #[test]
    fn workspace_index_content_errors_preserve_the_complete_previous_index() {
        let key = crate::keys::workspace_file("/bad");
        for failure in [false, true] {
            let store = SharedStore::new();
            store.clone().set(crate::keys::WORKSPACE_INDEX, &json!({"/bad": key}).to_string());
            store.clone().set(&key, &"x".repeat(WORKSPACE_FILE_MAX + 1));
            let before = workspace_snapshot(&store);
            let backend = WorkspaceReadStore {
                inner: store.clone(),
                reads: Arc::new(Mutex::new(Vec::new())),
                failure: failure.then(|| (key.clone(), 7)),
            };
            let mut app = App::new(
                Box::new(VirtualBoard::new()),
                Box::new(backend),
                LogBuf::new(64),
                FwMeta::default(),
            );
            assert!(app.workspace.is_empty());
            assert!(app.workspace.load_refused.is_some());
            drain_store(&mut app);
            assert_eq!(workspace_snapshot(&store), before);
        }
        let store = SharedStore::new();
        let mut index = serde_json::Map::new();
        for n in 0..17 {
            let path = format!("/f{n:02}");
            let key = crate::keys::workspace_file(&path);
            index.insert(path, json!(key));
            store.clone().set(&key, &"x".repeat(if n == 16 { 1 } else { WORKSPACE_FILE_MAX }));
        }
        store.clone().set(crate::keys::WORKSPACE_INDEX, &Value::Object(index).to_string());
        let before = workspace_snapshot(&store);
        let mut app = app_with(&store);
        assert!(app.workspace.is_empty(), "must not adopt a partial over-budget index");
        assert!(app.workspace.load_refused.is_some());
        drain_store(&mut app);
        assert_eq!(workspace_snapshot(&store), before);
    }

    #[test]
    fn workspace_transient_read_refusal_preserves_the_legacy_document() {
        let store = SharedStore::new();
        store.clone().set("ws", r#"{"version":1,"files":{"/a":"keep me"}}"#);
        let before = workspace_snapshot(&store);
        let backend = WorkspaceReadStore {
            inner: store.clone(),
            reads: Arc::new(Mutex::new(Vec::new())),
            failure: Some(("ws".to_string(), 4)),
        };
        let mut app =
            App::new(Box::new(VirtualBoard::new()), Box::new(backend), LogBuf::new(64), FwMeta::default());
        assert!(app.workspace.load_refused.is_some());
        drain_store(&mut app);
        assert_eq!(workspace_snapshot(&store), before);
    }

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
        let mut ws = AppWorkspace::default();
        for file in 0..4 {
            fs_tools::Workspace::write(&mut ws, &format!("/f{file}"), &"x".repeat(250_000)).unwrap();
        }
        // 1 MB of text plus metadata needs slightly over 2.06 MB free.
        assert!(super::workspace_fits(&ws, Some(7_900_000)).is_ok());
        // The same board with 1 MB left says no, and says why.
        let err = super::workspace_fits(&ws, Some(1_000_000)).unwrap_err();
        assert!(err.contains("nothing was changed"), "{err}");
        assert!(err.contains("PSRAM"), "{err}");
        // A platform that cannot answer is not blocked: no measurement is not a refusal.
        assert!(super::workspace_fits(&ws, None).is_ok());
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

    /// A controllable write refusal, after queue admission and before publication.
    #[derive(Clone)]
    struct WorkspaceWriteStore {
        inner: SharedStore,
        refuse: Arc<Mutex<Option<String>>>,
        refuse_remove: Arc<Mutex<Option<String>>>,
        last_error: Option<store::StoreError>,
    }

    impl WorkspaceWriteStore {
        fn new() -> Self {
            let inner = SharedStore::new();
            let mut cfg = Config::default();
            cfg.skills.seed_defaults = false;
            inner.clone().set(crate::config::NVS_KEY_CONFIG, &cfg.to_json_string());
            Self {
                inner,
                refuse: Arc::new(Mutex::new(None)),
                refuse_remove: Arc::new(Mutex::new(None)),
                last_error: None,
            }
        }

        fn app(&self) -> App {
            App::new(
                Box::new(VirtualBoard::new()),
                Box::new(self.clone()),
                LogBuf::new(128),
                FwMeta::default(),
            )
        }
    }

    impl KvStore for WorkspaceWriteStore {
        fn get(&self, key: &str) -> Option<String> {
            self.inner.get(key)
        }
        fn set(&mut self, key: &str, value: &str) -> bool {
            self.last_error = None;
            if self.refuse.lock().unwrap().as_deref() == Some(key) {
                self.last_error = Some(store::StoreError { code: 42, name: "TEST_WRITE_REFUSAL".into() });
                return false;
            }
            self.inner.set(key, value)
        }
        fn remove(&mut self, key: &str) -> bool {
            self.last_error = None;
            if self.refuse_remove.lock().unwrap().as_deref() == Some(key) {
                self.last_error = Some(store::StoreError { code: 43, name: "TEST_REMOVE_REFUSAL".into() });
                return false;
            }
            self.inner.remove(key)
        }
        fn keys(&self) -> Vec<String> {
            self.inner.keys()
        }
        fn whole_value_bytes(&self) -> Option<usize> {
            self.inner.whole_value_bytes()
        }
        fn last_error(&self) -> Option<store::StoreError> {
            self.last_error.clone()
        }
    }

    #[test]
    fn a_shared_store_flush_releases_the_app_lock_and_acknowledges_only_finished_generations() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        struct SlowStore {
            inner: WorkspaceWriteStore,
            key: String,
            pause: Arc<AtomicBool>,
            entered: mpsc::Sender<()>,
            resume: mpsc::Receiver<()>,
        }
        impl KvStore for SlowStore {
            fn get(&self, key: &str) -> Option<String> {
                self.inner.get(key)
            }
            fn set(&mut self, key: &str, value: &str) -> bool {
                if key == self.key && self.pause.swap(false, Ordering::AcqRel) {
                    self.entered.send(()).unwrap();
                    self.resume.recv_timeout(Duration::from_secs(10)).expect("release blocked store");
                }
                self.inner.set(key, value)
            }
            fn remove(&mut self, key: &str) -> bool {
                self.inner.remove(key)
            }
            fn keys(&self) -> Vec<String> {
                self.inner.keys()
            }
            fn whole_value_bytes(&self) -> Option<usize> {
                self.inner.whole_value_bytes()
            }
            fn last_error(&self) -> Option<store::StoreError> {
                self.inner.last_error()
            }
        }

        for (refused, admission_refused) in [(false, false), (true, false), (false, true), (true, true)] {
            let backend = WorkspaceWriteStore::new();
            let pause = Arc::new(AtomicBool::new(false));
            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let key = crate::keys::workspace_file("/a");
            let slow = SlowStore {
                inner: backend.clone(),
                key: key.clone(),
                pause: pause.clone(),
                entered: entered_tx,
                resume: resume_rx,
            };
            let mut app =
                App::new(Box::new(VirtualBoard::new()), Box::new(slow), LogBuf::new(128), FwMeta::default());
            app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", "durable")).unwrap();
            drain_store(&mut app);
            app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", "first generation")).unwrap();
            let first = app.workspace_pending.as_ref().unwrap().receipt.clone();
            if refused {
                *backend.refuse.lock().unwrap() = Some(if admission_refused {
                    crate::keys::WORKSPACE_INDEX.to_string()
                } else {
                    key.clone()
                });
            }
            pause.store(true, Ordering::Release);
            let shared: Shared = Arc::new(Mutex::new(app));
            let writer = std::thread::spawn({
                let shared = shared.clone();
                move || App::flush_shared_store(&shared, "slow-test-writer")
            });
            if let Err(error) = entered_rx.recv_timeout(Duration::from_secs(10)) {
                let _ = resume_tx.send(());
                let _ = writer.join();
                panic!("the writer never entered the blocked store: {error}");
            }
            let mut guard = match shared.try_lock() {
                Ok(guard) => guard,
                Err(error) => {
                    let _ = resume_tx.send(());
                    let _ = writer.join();
                    panic!("the writer held the app lock during blocked backend I/O: {error}");
                }
            };
            assert_eq!(guard.workspace.saved.get("/a"), Some(&crate::keys::fnv1a("durable")));
            assert_eq!(fs_tools::Workspace::read(&guard.workspace, "/a").unwrap(), "first generation");
            assert!(first.outcome().is_none(), "the blocked save is not durable yet");
            // RAM-only work can proceed while flash is blocked, including error rollback.
            let error = guard
                .workspace_edit::<()>(|ws| {
                    fs_tools::Workspace::write(ws, "/a", "partial")?;
                    fs_tools::Workspace::write(ws, "/scratch", "partial")?;
                    Err("partial edit refused".to_string())
                })
                .unwrap_err();
            assert_eq!(error, "partial edit refused");
            assert_eq!(fs_tools::Workspace::read(&guard.workspace, "/a").unwrap(), "first generation");
            assert!(fs_tools::Workspace::read(&guard.workspace, "/scratch").is_err());

            // Keep the app lock so the older writer cannot do app bookkeeping yet. Wait on
            // the generation's receipt, not elapsed time, for the backend drain to finish.
            let finish_first = || {
                resume_tx.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while first.outcome().is_none() {
                    assert!(Instant::now() < deadline, "the released backend did not finish its publication");
                    std::thread::yield_now();
                }
                assert_eq!(
                    first.outcome(),
                    Some(store::PublicationOutcome::Finished {
                        published: !refused,
                        cleanup_failed: Vec::new()
                    })
                );
            };
            let first_saved = (!refused).then(|| crate::keys::fnv1a("first generation"));
            let next = if admission_refused {
                let mut budget = store::QueuedStore::new(guard.store_queue());
                let error = guard
                    .workspace_edit(|ws| {
                        fs_tools::Workspace::write(ws, "/a", "second generation")?;
                        // Complete the old publication after the rollback clone, then refuse
                        // the new admission. Restoring RAM must retain that completed outcome.
                        finish_first();
                        assert!(budget.set("budget", &"x".repeat(store::PENDING_BYTES_MAX - 256)));
                        Ok(())
                    })
                    .unwrap_err();
                assert!(error.contains("STORE_QUEUE_FULL"), "{error}");
                assert_eq!(fs_tools::Workspace::read(&guard.workspace, "/a").unwrap(), "first generation");
                assert_eq!(
                    guard.workspace.saved.get("/a").copied(),
                    first_saved,
                    "rollback lost a completed publication outcome"
                );
                assert!(budget.remove("budget"));
                "durable" // A revert must rewrite any value replaced before the failed index.
            } else {
                finish_first();
                "second generation"
            };
            *backend.refuse.lock().unwrap() = None;
            guard.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", next)).unwrap();
            let second = guard.workspace_pending.as_ref().unwrap().receipt.clone();
            assert_eq!(guard.workspace.saved.get("/a").copied(), first_saved);
            assert!(second.outcome().is_none());
            drop(guard);

            let report = writer.join().expect("writer completes after the app lock is released");
            assert_eq!(
                report.failed,
                if refused {
                    if admission_refused {
                        1
                    } else {
                        2
                    }
                } else {
                    0
                }
            );
            let guard = app_lock(&shared);
            assert_eq!(guard.workspace.saved.get("/a").copied(), first_saved);
            assert!(second.outcome().is_none(), "the old report cannot acknowledge a newer queued save");
            assert_eq!(
                backend.inner.get(&key).as_deref(),
                Some(if refused && !admission_refused { "durable" } else { "first generation" })
            );
            drop(guard);
            assert_eq!(App::flush_shared_store(&shared, "next-test-writer").failed, 0);
            assert_eq!(backend.inner.get(&key).as_deref(), Some(next));
            assert_eq!(app_lock(&shared).workspace.saved.get("/a"), Some(&crate::keys::fnv1a(next)));
        }
    }

    #[test]
    fn workspace_tools_acknowledge_admission_without_waiting_for_the_writer() {
        let backend = WorkspaceWriteStore::new();
        let mut app = backend.app();
        let key = crate::keys::workspace_file("/admission.txt");
        *backend.refuse.lock().unwrap() = Some(key.clone());
        let write = app.call_tool("write", &json!({ "path": "/admission.txt", "text": "one" })).unwrap();
        assert_eq!(write["queued"], true, "{write}");
        assert_eq!(write["saved"], false, "{write}");
        assert!(write["persistence_note"].as_str().unwrap().contains("not storage acknowledgement"));
        assert!(backend.inner.get(&key).is_none(), "an acknowledgement must not imply a backend write");
        assert!(app.flush_store("refusing-test-writer").failed > 0);
        assert_eq!(fs_tools::Workspace::read(&app.workspace, "/admission.txt").unwrap(), "one");
        assert!(backend.inner.get(&key).is_none());
        *backend.refuse.lock().unwrap() = None;

        for (tool, arguments) in [
            ("fs_edit", json!({ "path": "/admission.txt", "find": "one", "replace": "two" })),
            ("shell", json!({ "command": "echo three > /redirect.txt" })),
            ("shell", json!({ "command": "touch /touch.txt" })),
            ("shell", json!({ "command": "echo four | tee /tee.txt" })),
            ("shell", json!({ "command": "base64enc /admission.txt -o /encoded.txt" })),
            ("script", json!({ "source": "tool(\"write\", {path: \"/script.txt\", text: \"five\"}); 42" })),
        ] {
            let text = ToolHost::call_tool(&mut app, tool, &arguments).unwrap();
            let result: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(result["queued"], true, "{tool}: {result}");
            assert_eq!(result["saved"], false, "{tool}: {result}");
        }
        let read = app.call_tool("read", &json!({ "path": "/admission.txt" })).unwrap();
        assert!(read.get("queued").is_none() && read.get("saved").is_none(), "reads are not save requests");
        let error = app
            .call_tool("shell", &json!({ "command": "echo six > /partial.txt; cat /missing-after-write" }))
            .unwrap_err();
        assert_eq!(error.data.as_ref().unwrap()["workspace_admission"]["queued"], true);
        assert_eq!(fs_tools::Workspace::read(&app.workspace, "/partial.txt").unwrap().trim_end(), "six");
        let error = ToolHost::call_tool(
            &mut app,
            "shell",
            &json!({ "command": "echo seven > /partial.txt; cat /missing-after-write" }),
        )
        .unwrap_err();
        assert!(error.contains("Earlier workspace edits remain in RAM"), "{error}");
        drain_store(&mut app);
        assert_eq!(backend.inner.get(&key).as_deref(), Some("two"));
    }

    #[test]
    fn workspace_edit_rolls_back_a_partially_mutating_error() {
        let backend = WorkspaceWriteStore::new();
        let mut app = backend.app();
        fs_tools::Workspace::write(&mut app.workspace, "/a", "before").unwrap();
        app.persist_workspace().unwrap();
        drain_store(&mut app);
        let before = workspace_snapshot(&backend.inner);
        let err = app
            .workspace_edit::<()>(|ws| {
                fs_tools::Workspace::write(ws, "/a", "partial")?;
                fs_tools::Workspace::write(ws, "/extra", "also partial")?;
                Err("the second operation failed".to_string())
            })
            .unwrap_err();
        assert!(err.contains("second operation failed"), "{err}");
        assert_eq!(fs_tools::Workspace::read(&app.workspace, "/a").unwrap(), "before");
        assert!(fs_tools::Workspace::read(&app.workspace, "/extra").is_err());
        assert!(app.flush_store("test-writer").is_empty());
        assert_eq!(workspace_snapshot(&backend.inner), before);
    }

    #[test]
    fn workspace_failed_queued_file_write_can_be_retried() {
        let backend = WorkspaceWriteStore::new();
        let mut app = backend.app();
        let key = crate::keys::workspace_file("/a");
        app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", "old")).unwrap();
        drain_store(&mut app);
        *backend.refuse.lock().unwrap() = Some(key.clone());
        app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", "new")).unwrap();
        assert_eq!(backend.inner.get(&key).as_deref(), Some("old"), "only admission so far");
        let report = app.flush_store("test-writer");
        assert!(report.failed > 0);
        assert!(app.flush_store("test-writer").is_empty(), "a failure is not an automatic retry");
        *backend.refuse.lock().unwrap() = None;
        app.persist_workspace().unwrap();
        drain_store(&mut app);
        assert_eq!(
            backend.inner.get(&key).as_deref(),
            Some("new"),
            "failed admission hashes suppressed retry"
        );
        assert_eq!(app.workspace.saved.get("/a"), Some(&crate::keys::fnv1a("new")));
    }

    #[test]
    fn workspace_failed_index_publication_preserves_previous_indexed_files() {
        let backend = WorkspaceWriteStore::new();
        let mut app = backend.app();
        app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/obsolete", "previous")).unwrap();
        drain_store(&mut app);
        let old_index = backend.inner.get(crate::keys::WORKSPACE_INDEX).unwrap();
        let old_key = crate::keys::workspace_file("/obsolete");
        *backend.refuse.lock().unwrap() = Some(crate::keys::WORKSPACE_INDEX.to_string());
        app.workspace_edit(|ws| {
            fs_tools::Workspace::remove(ws, "/obsolete")?;
            fs_tools::Workspace::write(ws, "/new", "next")
        })
        .unwrap();
        assert!(app.flush_store("test-writer").failed > 0);
        assert_eq!(backend.inner.get(crate::keys::WORKSPACE_INDEX).as_deref(), Some(old_index.as_str()));
        assert_eq!(
            backend.inner.get(&old_key).as_deref(),
            Some("previous"),
            "a refused index cannot remove a referenced value"
        );
        assert_eq!(fs_tools::Workspace::read(&backend.app().workspace, "/obsolete").unwrap(), "previous");
        *backend.refuse.lock().unwrap() = None;
        app.persist_workspace().unwrap();
        let report = app.flush_store("test-writer");
        assert_eq!(report.failed, 0);
        assert!(backend.inner.get(&old_key).is_none());
        assert!(backend.inner.get(crate::keys::WORKSPACE_INDEX).unwrap().contains("/new"));
    }

    #[test]
    fn workspace_failed_migration_publication_preserves_legacy_data() {
        for chunked in [false, true] {
            let backend = WorkspaceWriteStore::new();
            let old = r#"{"version":1,"files":{"/a":"legacy"}}"#;
            if chunked {
                store::write_blob(&mut backend.inner.clone(), "ws", old, WORKSPACE_PLAN).unwrap();
            } else {
                backend.inner.clone().set("ws", old);
                backend.inner.clone().set("ws.1", "legacy tail");
            }
            let before = workspace_snapshot(&backend.inner);
            *backend.refuse.lock().unwrap() = Some(crate::keys::WORKSPACE_INDEX.to_string());
            let mut app = backend.app();
            app.persist_workspace().unwrap();
            assert!(app.flush_store("test-writer").failed > 0);
            for (key, value) in &before {
                assert_eq!(
                    backend.inner.get(key).as_deref(),
                    Some(value.as_str()),
                    "{key} removed before migration published"
                );
            }
            assert!(backend.inner.get(crate::keys::WORKSPACE_INDEX).is_none());
            *backend.refuse.lock().unwrap() = None;
            app.persist_workspace().unwrap();
            drain_store(&mut app);
            assert!(backend.inner.get("ws").is_none());
            assert!(backend.inner.get("ws.1").is_none());
            assert_eq!(fs_tools::Workspace::read(&backend.app().workspace, "/a").unwrap(), "legacy");
        }
    }

    #[test]
    fn workspace_pending_reverts_replace_newer_values_and_confirmed_saves_stay_incremental() {
        let backend = WorkspaceWriteStore::new();
        let mut app = backend.app();
        app.workspace_edit(|ws| {
            fs_tools::Workspace::write(ws, "/a", "durable")?;
            fs_tools::Workspace::write(ws, "/b", "unchanged")
        })
        .unwrap();
        drain_store(&mut app);
        app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", "pending")).unwrap();
        assert_eq!(
            app.workspace.saved.get("/a"),
            Some(&crate::keys::fnv1a("durable")),
            "admission did not change the saved hash"
        );
        app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", "durable")).unwrap();
        let report = app.flush_store("test-writer");
        assert_eq!(report.failed, 0);
        assert_eq!(backend.inner.get(&crate::keys::workspace_file("/a")).as_deref(), Some("durable"));
        assert!(!report.keys.contains(&crate::keys::workspace_file("/b")));
        app.persist_workspace().unwrap();
        assert_eq!(app.flush_store("test-writer").keys, vec![crate::keys::WORKSPACE_INDEX]);
    }

    #[test]
    fn workspace_failed_dependency_preserves_the_index_and_invalidates_replaced_hashes() {
        let backend = WorkspaceWriteStore::new();
        let mut app = backend.app();
        app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", "previous")).unwrap();
        drain_store(&mut app);
        let old_index = backend.inner.get(crate::keys::WORKSPACE_INDEX).unwrap();
        *backend.refuse.lock().unwrap() = Some(crate::keys::workspace_file("/b"));
        app.workspace_edit(|ws| {
            fs_tools::Workspace::write(ws, "/a", "replacement")?;
            fs_tools::Workspace::write(ws, "/b", "unpublished")
        })
        .unwrap();
        let report = app.flush_store("test-writer");
        assert_eq!(report.failed, 2, "file refusal and skipped index: {report:?}");
        assert_eq!(backend.inner.get(crate::keys::WORKSPACE_INDEX).as_deref(), Some(old_index.as_str()));
        assert!(
            !app.workspace.saved.contains_key("/a"),
            "a replaced value with no published index is not a confirmed old hash"
        );
        assert_eq!(
            backend.inner.get(&crate::keys::workspace_file("/a")).as_deref(),
            Some("replacement"),
            "path-derived keys cannot roll back all files"
        );
        *backend.refuse.lock().unwrap() = None;
        app.workspace_edit(|ws| {
            fs_tools::Workspace::write(ws, "/a", "previous")?;
            fs_tools::Workspace::remove(ws, "/b")
        })
        .unwrap();
        drain_store(&mut app);
        assert_eq!(
            backend.inner.get(&crate::keys::workspace_file("/a")).as_deref(),
            Some("previous"),
            "a revert must restore a successfully replaced dependency too"
        );
        assert_eq!(backend.inner.get(crate::keys::WORKSPACE_INDEX).as_deref(), Some(old_index.as_str()));
    }

    #[test]
    fn workspace_cleanup_failure_keeps_published_hashes_and_is_explicitly_retryable() {
        let backend = WorkspaceWriteStore::new();
        let mut app = backend.app();
        app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/obsolete", "old")).unwrap();
        drain_store(&mut app);
        let obsolete = crate::keys::workspace_file("/obsolete");
        let current = crate::keys::workspace_file("/current");
        *backend.refuse_remove.lock().unwrap() = Some(obsolete.clone());
        app.workspace_edit(|ws| {
            fs_tools::Workspace::remove(ws, "/obsolete")?;
            fs_tools::Workspace::write(ws, "/current", "new")
        })
        .unwrap();
        let report = app.flush_store("test-writer");
        assert_eq!(report.failed, 1);
        assert_eq!(report.failures[0].op, store::StoreOp::Remove);
        assert_eq!(app.workspace.saved.get("/current"), Some(&crate::keys::fnv1a("new")));
        assert!(app.workspace_stale_keys.contains(&obsolete));
        assert_eq!(backend.inner.get(&obsolete).as_deref(), Some("old"));
        assert!(!backend.inner.get(crate::keys::WORKSPACE_INDEX).unwrap().contains("/obsolete"));
        let index_at = report.keys.iter().position(|key| key == crate::keys::WORKSPACE_INDEX).unwrap();
        let cleanup_at = report.keys.iter().position(|key| key == &obsolete).unwrap();
        assert!(index_at < cleanup_at, "cleanup follows confirmed index publication");
        assert!(app.flush_store("test-writer").is_empty(), "no background retry loop");
        *backend.refuse_remove.lock().unwrap() = None;
        app.persist_workspace().unwrap();
        let retry = app.flush_store("test-writer");
        assert_eq!(retry.failed, 0);
        assert!(!retry.keys.contains(&current), "do not rewrite confirmed file content to retry cleanup");
        assert!(backend.inner.get(&obsolete).is_none());
        assert!(app.workspace_stale_keys.is_empty());
    }

    #[test]
    fn workspace_admission_refusal_restores_ram_and_the_complete_previous_queue() {
        let backend = WorkspaceWriteStore::new();
        let mut app = backend.app();
        app.workspace_edit(|ws| fs_tools::Workspace::write(ws, "/a", "prior request")).unwrap();
        let queue = app.store_queue();
        let remaining = store::PENDING_BYTES_MAX - queue.pending_bytes() - 1024;
        assert!(app.store.set("budget", &"x".repeat(remaining)));
        let before_index = app.store.get(crate::keys::WORKSPACE_INDEX).unwrap();
        let before = (queue.pending(), queue.pending_bytes());
        let error = app
            .workspace_edit(|ws| {
                fs_tools::Workspace::write(ws, "/a", &"a".repeat(4096))?;
                fs_tools::Workspace::write(ws, "/new", &"b".repeat(4096))
            })
            .unwrap_err();
        assert!(error.contains("STORE_QUEUE_FULL"), "{error}");
        assert_eq!((queue.pending(), queue.pending_bytes()), before);
        assert_eq!(app.store.get(crate::keys::WORKSPACE_INDEX).as_deref(), Some(before_index.as_str()));
        assert_eq!(app.store.get(&crate::keys::workspace_file("/a")).as_deref(), Some("prior request"));
        assert!(app.store.get(&crate::keys::workspace_file("/new")).is_none());
        assert_eq!(fs_tools::Workspace::read(&app.workspace, "/a").unwrap(), "prior request");
        assert!(fs_tools::Workspace::read(&app.workspace, "/new").is_err());
        assert!(app.workspace_pending.as_ref().unwrap().receipt.outcome().is_none());
        assert!(app.store.remove("budget"));
        drain_store(&mut app);
        assert_eq!(fs_tools::Workspace::read(&backend.app().workspace, "/a").unwrap(), "prior request");
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
        let reloaded = AppWorkspace::from_json(&ws.to_json()).expect("valid version-1 workspace");
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
    /// Every command has exactly one name, and it is the one inside its family.
    ///
    /// The mapping comes from `family_members`, so a driver that joins a family is covered
    /// without this test being edited, and a name that is both registered on its own and owned
    /// by a family fails here.
    #[test]
    fn a_family_command_has_exactly_one_name() {
        let mut app = test_app();
        assert!(
            app.family_members.len() >= 20,
            "the families did not form: {} commands",
            app.family_members.len()
        );
        let commands: Vec<(String, String)> =
            app.family_members.iter().map(|(name, reach)| (name.clone(), reach.clone())).collect();
        for (command, reach) in commands {
            let (family, word) = reach.split_once(' ').expect("a family and a command word");
            let family = family.to_string();
            let word = word.to_string();

            assert!(app.tools.get(&command).is_none(), "`{command}` is a second name for `{reach}`");
            assert!(app.why_hidden(&command).is_none(), "`{command}` is reported as hidden");
            assert!(!tool_names(&mut app).contains(&command), "`{command}` is still offered");
            let err = app.call_tool(&command, &json!({})).unwrap_err();
            assert_eq!(err.code, code::NOT_FOUND, "`{command}` still answers: {err:?}");

            // The one name works, with the arguments its own schema documents.
            let waits = app
                .tools
                .get(&word)
                .map(|def| def.spec.input_schema["properties"].get("timeout_ms").is_some())
                .unwrap_or(false);
            let args = if waits {
                json!({ "command": word, "arguments": { "timeout_ms": 10 } })
            } else {
                json!({ "command": word, "arguments": {} })
            };
            match app.call_tool(&family, &args) {
                Ok(value) => assert!(value.is_object(), "{family} {word}: {value}"),
                Err(e) => assert!(
                    !e.message.contains("no command"),
                    "`{command}` was dropped and `{family}` lost `{word}` too: {e:?}"
                ),
            }
        }
    }

    /// The published list and the code are the same set, in the reference a person reads.
    ///
    /// `a_family_declares_helps_and_dispatches_the_same_commands` pins schema, `help` and
    /// dispatch. This pins the fourth reader: `docs/capabilities.md`, which is what an owner
    /// reads before driving the board by hand. Deleting a command without touching the doc
    /// leaves a name a person will type and the board will refuse, so the four sets are
    /// checked here as well, and the spelled-out count in the row ("in seven commands") is
    /// part of the check: a count is the first thing to go stale.
    #[test]
    fn the_documented_command_lists_match_the_families() {
        const DOC: &str = include_str!("../../../docs/capabilities.md");
        const MANIFEST_DOC: &str = include_str!("../../../docs/manifest.md");
        let app = test_app();
        let mut families: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
        for (_, reach) in &app.family_members {
            let family = reach.split(' ').next().unwrap_or("").to_string();
            let word = reach.split(' ').nth(1).unwrap_or("").to_string();
            families.entry(family).or_default().push(word);
        }
        assert!(families.len() >= 4, "families did not form: {families:?}");

        for (family, mut words) in families {
            words.sort();
            // The row that describes this family, if the reference has one: `| `board` | ...`.
            let row = DOC.lines().find(|line| {
                line.starts_with(&format!("| `{family}` |")) || line.starts_with(&format!("| `{family}` | "))
            });
            if let Some(row) = row {
                for word in &words {
                    assert!(
                        row.contains(&format!("`{word}`")),
                        "docs/capabilities.md does not name `{word}` in the `{family}` row"
                    );
                }
                // "in seven commands" must agree with the schema, or the doc is off by one.
                if let Some(rest) = row.split(" in ").nth(1) {
                    if let Some((count, _)) = rest.split_once(" command") {
                        let spelled = match count.trim() {
                            "one" => 1,
                            "two" => 2,
                            "three" => 3,
                            "four" => 4,
                            "five" => 5,
                            "six" => 6,
                            "seven" => 7,
                            "eight" => 8,
                            "nine" => 9,
                            "ten" => 10,
                            other => other.parse::<usize>().unwrap_or(usize::MAX),
                        };
                        assert_eq!(
                            spelled,
                            words.len(),
                            "the `{family}` row says {count} commands and the schema has {}",
                            words.len()
                        );
                    }
                }
            } else {
                // No row of its own: every command still has to be named somewhere in the
                // reference, because a driver table is where a person finds it.
                for word in &words {
                    assert!(
                        DOC.contains(&format!("`{word}`")) || MANIFEST_DOC.contains(&format!("`{word}`")),
                        "neither docs/capabilities.md nor docs/manifest.md names `{word}`, from the `{family}` family"
                    );
                }
            }
        }
    }
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
            // The definitions are read from the builders rather than from the registry: a name in
            // `DROPPED_ALIASES` is not registered, and its declaration is exactly what the family
            // saved, so leaving it out would flatter the family instead of measuring it.
            let flat: std::collections::BTreeMap<String, ToolDef> = board_tools::always_tools()
                .into_iter()
                .chain(board_tools::manifest_tools())
                .chain(app.devices.tool_defs())
                .map(|def| (def.spec.name.clone(), def))
                .collect();
            let replaced: usize = members
                .iter()
                .filter_map(|name| flat.get(name))
                .map(|def| {
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

    /// A refusal has to name the words the schema accepts.
    ///
    /// The family keeps its commands under their inner names (`board_status`) and answers to
    /// the short ones (`status`). The refusal used to list the inner names, so a caller that
    /// retried with the word it had just been told to use was refused again — measured on the
    /// board, where `board restart` answered "the commands are board_status, board_config_get, …"
    /// while the schema offered `status` and `config_get`.
    #[test]
    fn a_refused_family_command_names_the_words_the_schema_accepts() {
        let mut app = test_app();
        let spec = ToolHost::tool_specs(&app)
            .as_array()
            .expect("array")
            .iter()
            .find(|t| t["function"]["name"] == "board")
            .cloned()
            .expect("the board family");
        let declared: Vec<String> = spec["function"]["parameters"]["properties"]["command"]["enum"]
            .as_array()
            .expect("enum")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        let err = app.call_tool("board", &json!({ "command": "restart" })).unwrap_err();
        assert!(err.message.contains("no command `restart`"), "{err:?}");
        for word in ["status", "chip_temp", "logs_tail"] {
            assert!(declared.contains(&word.to_string()), "the schema lost `{word}`: {declared:?}");
            assert!(err.message.contains(word), "the refusal does not name `{word}`: {err:?}");
        }
        for inner in ["board_status", "board_config_get"] {
            assert!(!err.message.contains(inner), "the refusal names `{inner}`: {err:?}");
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
    fn tool_disable_is_enforced_by_mcp_and_the_internal_agent() {
        for name in ["script", "shell", "write", "board_status"] {
            let mut app = test_app();
            app.cfg.mcp.disabled_tools = vec![name.into()];
            let args = match name {
                "script" => json!({"source": "1"}),
                "shell" => json!({"command": "echo hello"}),
                "write" => json!({"path": "/blocked", "text": "no"}),
                _ => json!({}),
            };
            let direct = app.call_tool(name, &args);
            assert!(
                direct.as_ref().is_err_and(|e| e.message.contains("disabled")),
                "disabled {name} ran through App: {direct:?}"
            );
            let host = ToolHost::call_tool(&mut app, name, &args);
            assert!(
                host.as_ref().is_err_and(|e| e.contains("disabled")),
                "disabled {name} ran through ToolHost: {host:?}"
            );
            let rpc = app
                .handle_rpc(
                    &json!({"jsonrpc": "2.0", "id": 1,
                "method": "tools/call", "params": {"name": name, "arguments": args}})
                    .to_string(),
                    None,
                )
                .unwrap();
            assert_eq!(rpc["result"]["isError"], true, "{name}: {rpc}");
        }

        let shared = Arc::new(Mutex::new(test_app()));
        {
            let mut app = shared.lock().unwrap();
            point_at_the_stub(&mut app);
            app.cfg.mcp.disabled_tools = vec!["write".into()];
            app.agent.push_user("try the disabled write", "http", 1);
        }
        let mut model = Scripted::new(vec![
            Scripted::tool_call("blocked", "write", r#"{"path":"/blocked","text":"no"}"#),
            Scripted::answer("the write was refused"),
        ]);
        assert_eq!(run_turn(&shared, &mut model), "the write was refused");
        let app = shared.lock().unwrap();
        let result = app.agent.history.iter().find(|m| m.tool_call_id.as_deref() == Some("blocked")).unwrap();
        assert!(result.text.contains("disabled"), "{}", result.text);
        assert!(fs_tools::Workspace::read(&app.workspace, "/blocked").is_err());
    }

    #[test]
    fn tool_disable_survives_alias_family_and_generic_dispatch() {
        let mut app = test_app();
        app.cfg.mcp.disabled_tools =
            vec!["fs_write".into(), "board_status".into(), "oled_text".into(), "bme280_read".into()];
        for (name, args) in [
            ("write", json!({"path": "/blocked", "text": "no"})),
            ("fs_write", json!({"path": "/blocked", "text": "no"})),
            ("board", json!({"command": "status"})),
            ("display", json!({"command": "oled_text", "arguments": {"text": "no"}})),
            ("device_invoke", json!({"device": "bme280", "tool": "bme280_read"})),
        ] {
            assert!(app.call_tool(name, &args).is_err(), "{name} bypassed a disabled target");
        }
        assert!(!tool_names(&mut app).contains(&"write".into()), "the alias should not be offered");
        app.cfg.mcp.disabled_tools = vec!["board".into(), "display".into()];
        assert!(app.call_tool("board", &json!({"command": "status", "arguments": {}})).is_err());
        assert!(app
            .call_tool("display", &json!({"command": "oled_text", "arguments": {"text": "no"}}))
            .is_err());
        assert!(app
            .call_tool(
                "device_invoke",
                &json!({"device": "oled", "tool": "oled_text", "arguments": {"text": "no"}})
            )
            .is_err());
        app.cfg.mcp.disabled_tools.clear();
        app.cfg.mcp.tools_mode = "generic".into();
        assert!(
            app.call_tool("device_invoke", &json!({"device": "bme280", "tool": "bme280_read"})).is_ok(),
            "generic visibility is not a disable"
        );

        let board = crate::virtual_board::VirtualBoard::new();
        let calls = board.hw_calls.clone();
        let mut app =
            App::new(Box::new(board), Box::new(MemoryStore::new()), LogBuf::new(32), FwMeta::default());
        let applied = app.call_tool("manifest", &json!({"command": "apply", "arguments": {"manifest": {"tools": [
            {"name": "wrapped_read", "exec": {"type": "driver", "driver": "bme280", "action": "bme280_read"}},
            {"name": "nested_read", "safety": {"max_calls_per_min": 1}, "exec": {"type": "sequence", "steps": [
                {"exec": {"type": "gpio_write", "pin": 12, "level": 1}},
                {"exec": {"type": "sequence", "steps": [{"exec": {"type": "gpio_read", "pin": 0}}]}}
            ]}},
            {"name": "wrapped_delay", "exec": {"type": "sequence", "steps": [
                {"exec": {"type": "gpio_write", "pin": 12, "level": 1}},
                {"ms": 1, "exec": {"type": "gpio_write", "pin": 12, "level": 0}}
            ]}}
        ]}}})).unwrap();
        assert_eq!(applied["applied"], true, "{applied}");
        for (disabled, tool) in [
            ("bme280_read", "wrapped_read"),
            ("gpio_read", "nested_read"),
            ("bus", "nested_read"),
            ("delay", "wrapped_delay"),
        ] {
            app.cfg.mcp.disabled_tools = vec![disabled.into()];
            let before = calls.load(std::sync::atomic::Ordering::SeqCst);
            let err =
                app.call_tool(tool, &json!({})).expect_err("a manifest cannot bypass its target's disable");
            assert!(err.message.contains("disabled"), "{disabled} via {tool}: {err}");
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                before,
                "a disabled later step must be found before the first hardware effect"
            );
        }
        app.cfg.mcp.disabled_tools.clear();
        assert!(
            app.call_tool("nested_read", &json!({})).is_ok(),
            "a refused preflight must not consume the rate-limit allowance"
        );
    }

    #[test]
    fn tool_disable_blocks_script_and_shell_file_bypasses() {
        let mut app = test_app();
        app.call_tool("write", &json!({"path": "/source", "text": "hello"})).unwrap();
        app.cfg.mcp.disabled_tools = vec!["write".into()];
        for command in [
            "echo no > /blocked",
            "echo no >> /blocked",
            "echo no | tee /blocked",
            "touch /blocked",
            "base64enc /source -o /blocked",
            "cp /source /blocked",
            "mv /source /blocked --confirm",
            "wget http://127.0.0.1/api/status -o /blocked",
        ] {
            let result = app.call_tool("shell", &json!({"command": command}));
            assert!(
                result.as_ref().is_err_and(|e| e.message.contains("disabled")),
                "disabled write via {command}: {result:?}"
            );
            assert!(fs_tools::Workspace::read(&app.workspace, "/blocked").is_err());
        }
        let script = app.call_tool(
            "script",
            &json!({"source": "tool(\"fs_write\", {path: \"/blocked\", text: \"no\"})"}),
        );
        assert!(script.as_ref().is_err_and(|e| e.message.contains("disabled")), "{script:?}");
        assert_eq!(fs_tools::Workspace::read(&app.workspace, "/source").unwrap(), "hello");
        app.cfg.mcp.disabled_tools = vec!["read".into()];
        for command in [
            "cat /source",
            "head /source",
            "echo no < /source",
            "base64enc /source",
            "sha256sum /source",
            "crc32 /source",
            "zip a.zip /source",
        ] {
            let result = app.call_tool("shell", &json!({"command": command}));
            assert!(
                result.as_ref().is_err_and(|e| e.message.contains("disabled")),
                "disabled read via {command}: {result:?}"
            );
        }
        let script = app.call_tool("script", &json!({"path": "/source"}));
        assert!(script.as_ref().is_err_and(|e| e.message.contains("disabled")), "{script:?}");
        assert!(ToolHost::call_tool(&mut app, "fs_read", &json!({"path": "/source"})).is_err());
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
        assert!(text.contains("manifest"));
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
                "manifest",
                &json!({"command": "apply", "arguments": {
                    "manifest": {
                        "tools": [{
                            "name": "pin_led",
                            "description": "Drive the onboard LED pin",
                            "params": [{ "name": "on", "type": "boolean", "required": true }],
                            "exec": { "type": "gpio_write", "pin": 48, "level": { "param": "on" } }
                        }]
                    }
                }}),
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
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "tools": [
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
                ] } }}),
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
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                    "name": "wide_pwm", "params": [{ "name": "d", "type": "number", "required": true }],
                    "exec": { "type": "pwm", "pin": 8, "freq_hz": 50, "duty_pct": { "param": "d" } }
                }] } }}),
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
        let out = app
            .call_tool("display", &json!({"command": "oled_text", "arguments": { "text": "OK", "line": 1 }}))
            .unwrap();
        assert_eq!(out["chars"], 2, "{out}");
        let out = app.call_tool("display", &json!({"command": "oled_read", "arguments": {}})).unwrap();
        assert!(out["text"].as_str().unwrap_or("").contains("OK"), "{out}");
        app.call_tool(
            "display",
            &json!({"command": "oled_rect", "arguments": { "x": 0, "y": 0, "w": 4, "h": 4 }}),
        )
        .unwrap();
        app.call_tool("display", &json!({"command": "oled_bitmap", "arguments": { "data_b64": crate::util::b64_encode(&vec![0u8; 1024]) }}))
            .unwrap();
        app.call_tool("display", &json!({"command": "oled_contrast", "arguments": { "value": 0x80 }}))
            .unwrap();
        app.call_tool("display", &json!({"command": "oled_clear", "arguments": {}})).unwrap();
    }

    /// A manifest tool with a rate limit is the only thing standing between a looping model
    /// and a relay chattering at the speed of the network.
    #[test]
    fn a_manifest_tool_rate_limit_refuses_the_next_call() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                    "name": "once_a_minute", "params": [], "safety": { "max_calls_per_min": 1 },
                    "exec": { "type": "gpio_write", "pin": 5, "level": 1 }
                }] } }}),
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
            .call_tool("manifest", &json!({"command": "apply", "arguments": { "manifest": { "tools": [{ "name": "bad name", "exec": { "type": "gpio_write", "pin": 99, "level": 1 } }] } }}))
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
            let text = ToolHost::call_tool(
                &mut app,
                "manifest",
                &json!({"command": "describe", "arguments": { "section": section }}),
            )
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
                "manifest",
                &json!({"command": "describe", "arguments": { "section": "examples", "example": name }}),
            )
            .unwrap();
            assert!(text.len() <= MAX_TOOL_RESULT_BYTES, "example {name} is {} bytes", text.len());
            assert!(text.contains("exec"), "example {name} arrived empty: {text}");
        }

        // The overview is the entry point: it names the sections and the examples.
        let overview =
            ToolHost::call_tool(&mut app, "manifest", &json!({"command": "describe", "arguments": {}}))
                .unwrap();
        for needle in ["exec", "schema", "relay_with_safety", "max_tools"] {
            assert!(overview.contains(needle), "the overview does not mention {needle}: {overview}");
        }

        // The driver catalogue is paged for the same reason: fourteen entries in one
        // result would be cut, so each one is asked for by name.
        let listed = ToolHost::call_tool(
            &mut app,
            "manifest",
            &json!({"command": "describe", "arguments": { "section": "drivers" }}),
        )
        .unwrap();
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
                "manifest",
                &json!({"command": "describe", "arguments": { "section": "drivers", "driver": kind }}),
            )
            .unwrap();
            assert!(text.len() <= MAX_TOOL_RESULT_BYTES, "driver {kind} is {} bytes", text.len());
            assert!(!text.contains("truncated"), "driver {kind} was cut");
        }
        // A section name that does not exist is refused with the list, not guessed at.
        let err = ToolHost::call_tool(
            &mut app,
            "manifest",
            &json!({"command": "describe", "arguments": { "section": "nope" }}),
        )
        .unwrap_err();
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
            "manifest",
            &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                "name": "aaa_manifest", "params": [],
                "exec": { "type": "gpio_read", "pin": 3 }
            }] } }}),
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
        app.call_tool(
            "manifest",
            &json!({"command": "apply", "arguments": { "manifest": { "devices": declared } }}),
        )
        .expect("applied");
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

    /// The heap pool by pool, when the platform can split it, and an empty list when it
    /// cannot. The split is what a memory change is checked against: "internal RAM went up"
    /// is only good news if the pool that went up is the one the change was about. It rides
    /// on `/api/status` because a second route for one number was a second thing to keep true.
    #[test]
    fn the_status_reports_the_heap_pool_by_pool() {
        let mut app = test_app();
        let status = body_json(&app.route(&get("/api/status")));
        let pools = status["memory"]["pools"].as_array().expect("pools");
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
        let one = body_json(&bare.route(&get("/api/status")));
        assert_eq!(one["memory"]["pools"].as_array().map(|p| p.len()), Some(0), "{one}");
    }

    /// Every thread in `/api/status` carries the stack it has left at its deepest point —
    /// the reading that decides whether a stack constant can come down. Without it the
    /// answer is "guess and watch for a reboot".
    /// Counting has to cover every tool, not only the ones that reach the registry.
    ///
    /// `call_tool_inner` returns early for the file, shell, script and clip tools, so those
    /// eleven returned before the counter was incremented: a board that had served about a
    /// hundred calls reported 70, and the names missing from `per_tool` were exactly the ones
    /// an operator watches.
    #[test]
    fn every_tool_call_reaches_the_counters() {
        let mut app = test_app();
        let calls: [(&str, Value); 5] = [
            ("write", json!({"path": "/counted.txt", "text": "one"})),
            ("read", json!({"path": "/counted.txt"})),
            ("edit", json!({"path": "/counted.txt", "find": "one", "replace": "two"})),
            ("shell", json!({"command": "echo counted"})),
            ("script", json!({"source": "1 + 1"})),
        ];
        let before = app.runtime.counters_json();
        for (name, args) in &calls {
            app.call_tool(name, args).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        }
        let after = app.runtime.counters_json();
        for (name, _) in &calls {
            assert!(
                after["per_tool"][name].as_u64().unwrap_or(0)
                    > before["per_tool"][name].as_u64().unwrap_or(0),
                "`{name}` was served but is missing from the counters: {}",
                after["per_tool"]
            );
        }
        assert_eq!(
            after["tool_calls"].as_u64().unwrap() - before["tool_calls"].as_u64().unwrap(),
            calls.len() as u64,
            "the aggregate disagrees with `per_tool`: {after}"
        );
    }

    /// The one reading a writing tool leaves open: did the queued change reach flash?
    /// `write` and `edit` answer `saved: false` and point at the store's own counters. Those
    /// lived on `/api/status` only, which is a portal route — an agent driving the board
    /// through MCP was told to check something it had no way to read.
    #[test]
    fn board_status_reports_what_the_store_owes() {
        let mut app = test_app();
        app.call_tool("write", &json!({"path": "/saved.txt", "text": "x"})).unwrap();
        let status = app.call_tool("board", &json!({"command": "status"})).unwrap();
        let store = &status["store"];
        assert!(store["pending"].is_number(), "board_status has no store reading: {status}");
        assert!(store["failed"].is_number(), "{status}");
        assert!(store["written"].is_number(), "{status}");
        assert!(
            store["pending"].as_u64().unwrap() >= 1,
            "a write that was only queued has to show as owed work: {store}"
        );
    }

    #[test]
    fn board_status_reports_the_agent_its_threads_and_the_pools() {
        // The three readings that only `/api/status` used to carry. An agent driving this
        // board over MCP has no portal route, so "is the board busy", "which tool is it in",
        // "does another stack fit" and "how much RAM can this pool still give" were all out
        // of reach.
        let mut app = test_app();
        let status = app.call_tool("board", &json!({ "command": "status" })).unwrap();

        let agent = &status["agent"];
        assert_eq!(agent["run_active"], json!(false), "an idle board is not running: {status}");
        assert_eq!(agent["pending"], json!(false), "{status}");
        // `active_tool` names the synchronous tool in flight — which, for a `board status` call,
        // is this one. It says nothing about a turn, and `run_active` is what does.
        assert_eq!(agent["active_tool"]["name"], json!("board"), "{status}");
        assert_eq!(agent["runs"], json!(app.agent.runs), "{status}");
        assert!(
            agent["history"].as_u64().unwrap_or(u64::MAX) == app.agent.history.len() as u64,
            "history disagrees with the transcript: {status}"
        );
        assert!(agent["last_error"].is_string(), "{status}");

        let threads = status["threads"].as_array().expect("threads");
        assert_eq!(threads.len(), 3, "the virtual board reports three tasks: {status}");
        assert!(threads.iter().all(|t| t["high_water_kb"].as_f64().is_some()), "{status}");

        let pools = status["memory"]["pools"].as_array().expect("memory pools");
        assert!(!pools.is_empty(), "the virtual board has pools: {status}");
        assert!(
            pools.iter().all(|p| p["pool"].is_string() && p["free_kb"].is_number()),
            "a pool has to name itself and its numbers: {status}"
        );

        // The certificate the board made for itself: a caller told to compare a fingerprint
        // needs to be able to read it.
        let tls = app.call_tool("board", &json!({ "command": "tls" })).unwrap();
        assert!(tls["enabled"].is_boolean(), "{tls}");
        assert!(tls["how_to_check"].is_string(), "{tls}");

        // A turn queued from outside the agent shows up without the board starting it: that
        // is the question "should I wait" an outside caller asks.
        app.route(&post("/api/agent/message", r#"{"text":"are you busy"}"#));
        let status = app.call_tool("board", &json!({ "command": "status" })).unwrap();
        assert_eq!(status["agent"]["pending"], json!(true), "{status}");
    }

    /// The certificate is installed by the platform *after* `App::new` returns — it creates the
    /// TLS server first — so the tool has to read it, not a copy taken at construction.
    ///
    /// It was a copy: on the board (firmware 5722ef2) `/api/tls` answered `enabled: true,
    /// port: 443` with a fingerprint while `board tls` answered `enabled: false, port: 0`,
    /// because the snapshot was the default and nothing ever replaced it.
    #[test]
    fn board_tls_reads_the_certificate_the_platform_installs_later() {
        let mut app = test_app();
        let before = app.call_tool("board", &json!({ "command": "tls" })).unwrap();
        assert_eq!(before["enabled"], json!(false), "a host App has no certificate: {before}");
        assert_eq!(before["port"], json!(0), "{before}");

        app.tls = crate::app::TlsInfo { port: 443, fingerprint: "aa:bb:cc".into(), error: String::new() };
        let after = app.call_tool("board", &json!({ "command": "tls" })).unwrap();
        assert_eq!(after["enabled"], json!(true), "the tool read a stale copy: {after}");
        assert_eq!(after["port"], json!(443), "{after}");
        assert_eq!(after["fingerprint"], json!("aa:bb:cc"), "{after}");
    }

    /// The board's own model, reached over MCP: the one class of capability that used to need
    /// the portal, a person or an HTTP client, because everything here assumed the caller *is*
    /// the model.
    #[test]
    fn an_outside_agent_can_reach_the_boards_model() {
        let mut app = test_app();

        // `agent_say`: the message waits for the turn the board starts on its own thread.
        let first = app.call_tool("agent_say", &json!({ "text": "  how warm is it  " })).unwrap();
        assert_eq!(first["ok"], json!(true), "{first}");
        assert_eq!(first["replaced_a_waiting_message"], json!(false), "{first}");
        let pending = app.agent.pending.as_ref().expect("a queued turn");
        assert_eq!(pending.text, "how warm is it", "trimmed like every other message");
        assert_eq!(pending.source, "mcp", "the transcript has to say where it came from");

        // One message waits at a time: the second call says so instead of pretending to queue
        // two turns, and an empty question is refused rather than queued as work.
        let second = app.call_tool("agent_say", &json!({ "text": "and now?" })).unwrap();
        assert_eq!(second["replaced_a_waiting_message"], json!(true), "{second}");
        assert_eq!(app.agent.pending.as_ref().unwrap().text, "and now?");
        assert!(app.call_tool("agent_say", &json!({ "text": "   " })).is_err(), "an empty message");

        // `agent_history`: the transcript, oldest first, with the cursor to poll with.
        app.agent.history.push(crate::agent::Message::user("earlier question", "http", 1));
        app.agent.history.push(crate::agent::Message::assistant("earlier answer", vec![], 2));
        let all = app.call_tool("agent_history", &json!({})).unwrap();
        assert_eq!(all["seq"], json!(2), "{all}");
        assert_eq!(all["messages"][0]["text"], json!("earlier question"), "{all}");
        // A queued question is visible before the turn starts, marked `pending`: that is how a
        // caller knows the board has it and has not answered yet.
        let tail = app.call_tool("agent_history", &json!({ "after": 1 })).unwrap();
        let entries = tail["messages"].as_array().expect("messages");
        assert_eq!(entries.len(), 2, "{tail}");
        assert_eq!(entries[1]["text"], json!("and now?"), "{tail}");
        assert_eq!(entries[1]["pending"], json!(true), "{tail}");

        // `agent_config_*`: a partial patch, and the key stays write-only in both directions.
        let before = app.call_tool("agent_config_get", &json!({})).unwrap();
        assert!(before.get("base_url").is_some(), "{before}");
        assert!(before.get("api_key").is_none(), "the key is never returned: {before}");
        let set = app
            .call_tool(
                "agent_config_set",
                &json!({ "patch": { "model": "another-model", "api_key": "__set__" } }),
            )
            .unwrap();
        assert_eq!(set["model"], json!("another-model"), "{set}");
        assert!(set.get("api_key").is_none(), "the key is write-only: {set}");
        assert_eq!(set["queued"], json!(true), "the write has to be queued for flash: {set}");
        assert_eq!(
            app.call_tool("agent_config_set", &json!({ "patch": { "base_url": "ftp://nope" } }))
                .unwrap_err()
                .code,
            code::INVALID_PARAMS,
            "a scheme the transport cannot carry is refused, as on the portal"
        );

        // `agent_forget`: the tail stays and nothing is left waiting; mid-turn it is refused,
        // because the answer that turn is producing would land in a thread that lost its question.
        let forgotten = app.call_tool("agent_forget", &json!({ "keep": 0 })).unwrap();
        assert!(forgotten["removed"].as_u64().unwrap() >= 2, "{forgotten}");
        assert_eq!(forgotten["kept"], json!(0), "{forgotten}");
        assert!(app.agent.history.is_empty(), "everything was dropped: {forgotten}");
        assert!(app.agent.pending.is_none(), "a forgotten conversation has nothing waiting");
        app.run_gate.set_active(true);
        let refused = app.call_tool("agent_forget", &json!({ "keep": 0 })).unwrap_err();
        assert!(refused.message.contains("turn is running"), "{refused:?}");
        app.run_gate.set_active(false);
    }

    /// `prompts/list` used to answer `[]` — a capability that did nothing. The board's skills
    /// are written procedures for its own model, which is what an MCP prompt is, so listing
    /// them hands an outside agent the conventions this board follows.
    #[test]
    fn the_prompts_are_the_boards_skills() {
        let mut app = test_app();
        let listed = app
            .handle_rpc(&json!({"jsonrpc":"2.0","id":1,"method":"prompts/list"}).to_string(), None)
            .expect("a reply");
        let prompts = listed["result"]["prompts"].as_array().expect("prompts");
        assert!(!prompts.is_empty(), "the board seeds skills, so a prompt exists: {listed}");
        let name = prompts[0]["name"].as_str().expect("a name").to_string();
        assert!(!prompts[0]["description"].as_str().unwrap_or("").is_empty(), "{listed}");

        let got = app
            .handle_rpc(
                &json!({"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":name}}).to_string(),
                None,
            )
            .expect("a reply");
        let text = got["result"]["messages"][0]["content"]["text"].as_str().expect("the skill body");
        assert!(!text.trim().is_empty(), "the body is the point: {got}");
        assert!(!text.starts_with("---"), "the front matter is metadata, not the procedure: {text}");

        let missing = app
            .handle_rpc(
                &json!({"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"nope"}}).to_string(),
                None,
            )
            .expect("a reply");
        assert!(missing["error"].is_object(), "an unknown prompt is an error: {missing}");
    }

    /// The MCP way to stop a turn, which used to have no handler at all: a client that sent it
    /// got `method not found` and had to fall back to `POST /api/agent/cancel`.
    #[test]
    fn a_client_can_stop_a_turn_the_mcp_way() {
        let mut app = test_app();
        app.run_gate.set_active(true);
        let reply = app.handle_rpc(
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": 7, "reason": "changed my mind" }
            })
            .to_string(),
            None,
        );
        assert!(reply.is_none(), "a notification has no reply: {reply:?}");
        assert!(app.run_gate.cancelled(), "the gate is what the loop checks between steps");
        app.run_gate.set_active(false);
    }

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
            assert!(t["core"].as_i64().is_some(), "every thread has to say which core it is on: {t}");
        }
    }

    /// A device the board does not have is not a tool, and the sentence explains it.
    /// Attachments are replaced only after all metadata, encodings and message limits pass.
    #[test]
    fn attachments_are_admitted_atomically_and_belong_to_one_message() {
        let image = json!({"name": "old.png", "mime": "image/png", "data_b64": "eA=="});
        for invalid in [
            json!({"text": "x".repeat(AGENT_TEXT_MAX + 1), "attachments": [image.clone()]}),
            json!({"text": "new", "attachments": [{"name": "new.png", "mime": "image/png", "data_b64": "eQ=="}, {"name": "bad", "data_b64": "?"}]}),
            json!({"text": "new", "attachments": [{"name": "bad", "mime": "image/png\r\nInjected: yes", "data_b64": "eA=="}]}),
            json!({"text": "new", "attachments": [image.clone(), image.clone()]}),
            json!({"text": "new", "attachments": "not an array"}),
            json!({"text": "new", "attachments": [{"name": 42, "data_b64": "eA=="}]}),
            json!({"text": "new", "attachments": [{"mime": ["image/png"], "data_b64": "eA=="}]}),
            json!({"text": "new", "attachments": [{"data_b64": false}]}),
            json!({"text": "new", "attachments": [{"name": "x".repeat(129), "data_b64": "eA=="}]}),
            json!({"text": "new", "attachments": [{"name": "big", "data_b64": crate::util::b64_encode(&vec![0; MAX_ATTACHMENT_BYTES + 1])}]}),
        ] {
            let mut app = test_app();
            assert_eq!(
                app.route(&post(
                    "/api/agent/message",
                    &json!({"text": "look", "attachments": [image.clone()]}).to_string()
                ))
                .status,
                202
            );
            let original = app.agent.pending.as_ref().unwrap().text.clone();
            let resp = app.route(&post("/api/agent/message", &invalid.to_string()));
            assert!((400..500).contains(&resp.status), "accepted invalid attachments: {}", body_json(&resp));
            assert_eq!(app.attachments.len(), 1, "refused input changed the download list");
            assert_eq!(app.attachments[0].name, "old.png", "refused input replaced the old image");
            assert_eq!(app.agent.pending.as_ref().unwrap().text, original);
        }
        let mut app = test_app();
        assert_eq!(
            app.route(&post(
                "/api/agent/message",
                &json!({"text": "look", "attachments": [image]}).to_string()
            ))
            .status,
            202
        );
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"text only"}"#)).status, 202);
        assert!(app.attachments.is_empty(), "a text-only message retained the previous download");
        assert!(
            app.agent.pending.as_ref().unwrap().attachments.is_empty(),
            "a text-only turn resent an old image"
        );

        assert_eq!(app.route(&post("/api/agent/message", &json!({"text": "look", "attachments": [{"name": "running.png", "mime": "image/png", "data_b64": "eA=="}]}).to_string())).status, 202);
        point_at_the_stub(&mut app);
        assert!(matches!(app.agent_begin(), Some(AgentStep::Ask { .. })));
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"next text-only turn"}"#)).status, 202);
        assert!(app.attachments.is_empty());
        app.tool_transport = Some(Box::new(Scripted::new(vec![Scripted::answer("the running image")])));
        assert_eq!(
            app.call_tool("image_read", &json!({})).unwrap()["name"],
            "running.png",
            "a newer queued message replaced the running turn's image"
        );
        assert!(matches!(
            app.agent_reply(Ok(llm::ModelReply { content: "done".into(), ..Default::default() })),
            AgentStep::Done(_)
        ));
        assert!(app.call_tool("image_read", &json!({})).is_err(), "a finished turn retained an active image");
        assert_eq!(app.route(&post("/api/agent/reset", "{}")).status, 200);
        assert!(app.attachments.is_empty());
    }

    #[test]
    fn image_read_uses_the_configured_adapter_without_a_camera() {
        for protocol in ["responses", "anthropic", "chat"] {
            let mut app = test_app();
            point_at_the_stub(&mut app);
            app.llm.protocol = protocol.into();
            app.attachments = vec![crate::agent::Attachment {
                name: "photo.png".into(),
                mime: "image/png".into(),
                data_b64: "eA==".into(),
            }];
            let response = match protocol {
                "responses" => json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "a photo"}]}]}).to_string(),
                "anthropic" => json!({"content": [{"type": "text", "text": "a photo"}]}).to_string(),
                _ => Scripted::answer("a photo"),
            };
            let transport = Scripted::new(vec![response]);
            let seen = transport.seen.clone();
            app.tool_transport = Some(Box::new(transport));
            let answer = app.call_tool("image_read", &json!({"prompt": "describe"})).expect(protocol);
            assert_eq!(answer["text"], "a photo");
            let body: Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
            match protocol {
                "responses" => {
                    assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
                    assert_eq!(body["input"][0]["content"][1]["image_url"], "data:image/png;base64,eA==");
                }
                "anthropic" => assert_eq!(body["messages"][0]["content"][1]["source"]["data"], "eA=="),
                _ => assert_eq!(
                    body["messages"][0]["content"][1]["image_url"]["url"],
                    "data:image/png;base64,eA=="
                ),
            }
            assert!(
                tool_names(&mut app).contains(&"image_read".into()),
                "an uploaded image does not require a camera"
            );
            app.llm.vision = false;
            assert!(app.call_tool("image_read", &json!({})).unwrap_err().message.contains("vision"));
            assert_eq!(seen.lock().unwrap().len(), 1, "disabled vision still contacted the model");
            app.llm.vision = true;
            app.attachments[0].mime = "application/pdf".into();
            assert!(app.call_tool("image_read", &json!({})).is_err(), "a PDF was sent as an image");
        }
    }

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
        a.call_tool("manifest", &json!({"command": "apply", "arguments": manifest})).expect("apply a");
        b.call_tool("manifest", &json!({"command": "apply", "arguments": manifest})).expect("apply b");

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
            app.call_tool("board", &json!({"command": "config_set", "arguments": { "patch": { "mcp": { "tools_mode": "generic" } } }})).unwrap();
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
            .call_tool("board", &json!({"command": "config_set", "arguments": { "patch": { "mcp": { "tools_mode": "generik" } } }}))
            .unwrap_err();
        assert!(err.message.contains("auto, generic or all"), "{err:?}");
        assert_eq!(app.cfg.mcp.tools_mode, "auto", "the value changed anyway");
        let before = app.cfg.clone();
        for body in [
            json!(null),
            json!([]),
            json!(42),
            json!("settings"),
            json!({"mode": "generik"}),
            json!({"mode": 12}),
            json!({"mode": "generic", "disabled": "oops"}),
            json!({"mode": "generic", "disabled": ["gpio_write", 3]}),
        ] {
            let response = app.route(&post("/api/tools", &body.to_string()));
            assert_eq!(response.status, 400, "accepted malformed selection: {body}");
            assert_eq!(app.cfg, before, "partially applied malformed selection: {body}");
        }
    }

    /// The devkit's own peripherals are opt-out: a board that wants the smallest possible
    /// surface comes up with the bus, GPIO and manifest tools, and has its LED and button
    /// back by declaring them — which is also how a different devkit starts small.
    #[test]
    fn a_minimal_board_starts_without_the_devkits_own_peripherals() {
        let mut app = test_app();
        assert!(app.is_visible("indicators"), "the profile's LED and buttons are there by default");

        // Through the config path a person or the agent would use.
        app.call_tool(
            "board",
            &json!({"command": "config_set", "arguments": { "patch": { "auto_peripherals": false } }}),
        )
        .unwrap();
        for gone in ["led_set", "led_blink", "buttons_read", "button_wait"] {
            assert!(!app.is_visible(gone), "{gone} survived a minimal start");
        }
        assert!(app.is_visible("bus"), "the primitives must stay");
        assert!(app.is_visible("manifest"), "self-configuration must stay");

        // Declaring them in the manifest brings the tools back — the whole point.
        let v = app
            .call_tool(
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "devices": [
                    { "id": "rgb", "kind": "led", "driver": "led", "pins": { "out": 48 },
                      "options": { "kind": "ws2812" } },
                    { "id": "boot", "kind": "buttons", "driver": "buttons", "pins": { "boot": 0 } }
                ] } }}),
            )
            .unwrap();
        assert_eq!(v["applied"], true, "{v}");
        assert!(app.is_visible("indicators"), "the declared LED did not appear");
        assert!(app.is_visible("indicators"), "the declared button did not appear");
        let out = app
            .call_tool("indicators", &json!({"command": "led_set", "arguments": { "color": "#010203" }}))
            .unwrap();
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
        let inv = app.call_tool("board", &json!({"command": "inventory", "arguments": {}})).unwrap();
        let caps: Vec<String> =
            inv["capabilities"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
        assert!(caps.contains(&"mic".to_string()) && caps.contains(&"speaker".to_string()), "{caps:?}");

        let level =
            app.call_tool("audio", &json!({"command": "mic_level", "arguments": { "ms": 50 }})).unwrap();
        assert!(level["dbfs"].is_number(), "the level meter read nothing: {level}");

        let rec = app
            .call_tool("audio", &json!({"command": "mic_record", "arguments": { "samples": 64 }}))
            .unwrap();
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

        let tone = app
            .call_tool(
                "audio",
                &json!({"command": "speaker_tone", "arguments": { "freq_hz": 440, "ms": 50 }}),
            )
            .unwrap();
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
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "devices": [
                    { "id": "lamp", "kind": "led", "driver": "led", "pins": { "out": 18 },
                      "options": { "kind": "ws2812", "count": "8" } }
                ] } }}),
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
        let out = app
            .call_tool("indicators", &json!({"command": "led_set_2", "arguments": { "color": "#102030" }}))
            .unwrap();
        assert_eq!(out["ok"], json!(true), "{out}");
        for tool in ["led_set_2", "led_set"] {
            let out = app
                .call_tool(
                    "device_invoke",
                    &json!({
                        "device": "lamp", "tool": tool, "arguments": {"color": "#203040"}
                    }),
                )
                .expect("both the published name and the stored raw alias reach the second strip");
            assert_eq!(out["ok"], true, "{tool}: {out}");
        }
        app.cfg.mcp.disabled_tools = vec!["led_set_2".into()];
        for tool in ["led_set_2", "led_set"] {
            let err = app
                .call_tool(
                    "device_invoke",
                    &json!({
                        "device": "lamp", "tool": tool, "arguments": {"color": "#203040"}
                    }),
                )
                .expect_err("a raw alias cannot bypass an instance's disable");
            assert!(err.message.contains("disabled"), "{tool}: {err}");
        }
        assert!(
            app.call_tool("indicators", &json!({"command": "led_set", "arguments": {"color": "#102030"}}))
                .is_ok(),
            "disabling the second strip does not disable the first"
        );
    }

    #[test]
    fn a_manifest_cannot_shadow_a_board_tool() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                    "name": "board", "params": [],
                    "exec": { "type": "delay", "ms": 1 }
                }] } }}),
            )
            .unwrap();
        assert_eq!(v["applied"], false, "a manifest tool took `board`: {v}");
        let issue = v["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == "/tools/0/name")
            .expect("an issue on the name");
        assert!(issue["message"].as_str().unwrap().contains("board"), "{issue}");
        assert!(issue["hint"].as_str().unwrap().contains("my_"), "the hint must offer a rename: {issue}");
        // The board kept its own tool.
        let v = app
            .call_tool("manifest", &json!({"command": "apply", "arguments": { "manifest": { "tools": [] } }}))
            .unwrap();
        assert_eq!(v["applied"], true, "{v}");
    }

    /// The `manifest_` prefix is not reserved — only a name the board actually uses is.
    #[test]
    fn a_manifest_may_use_the_manifest_prefix_but_not_a_taken_name() {
        let mut app = test_app();
        let v = app
            .call_tool(
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                    "name": "bme280_read", "params": [],
                    "exec": { "type": "delay", "ms": 1 }
                }] } }}),
            )
            .unwrap();
        assert_eq!(v["applied"], false, "a manifest tool took a driver tool: {v}");

        let v = app
            .call_tool(
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                    "name": "manifest_helper", "params": [],
                    "exec": { "type": "delay", "ms": 1 }
                }] } }}),
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
            r#"{"tools":[{"name":"board","params":[],"exec":{"type":"delay","ms":1}}]}"#,
        )
        .expect("a manifest");
        app.rebuild();
        // The board's own `manifest` tool wins over the manifest entry of the same name, and
        // the five old command names stay reachable through it.
        assert!(app.is_visible("manifest"), "the built-in disappeared on rebuild");
        let v = app
            .call_tool("manifest", &json!({"command": "apply", "arguments": { "manifest": { "tools": [] } }}))
            .unwrap();
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
        let v = app.call_tool("manifest", &json!({"command": "apply", "arguments": dotted_pump()})).unwrap();
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
        let refused =
            app.call_tool("manifest", &json!({"command": "apply", "arguments": dotted_pump()})).unwrap();
        assert_eq!(refused["applied"], false);

        // tools/list still answers, and the board's own tools are still in it.
        let list = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
        let listed = list["result"]["tools"].as_array().expect("tools array");
        assert!(listed.iter().any(|t| t["name"] == "board"), "tools/list lost the built-ins");

        // A normal call still reaches the hardware.
        assert!(
            app.call_tool("board", &json!({"command": "status", "arguments": {}})).is_ok(),
            "the board stopped answering"
        );

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
        let refused =
            app.call_tool("manifest", &json!({"command": "apply", "arguments": dotted_pump()})).unwrap();
        assert_eq!(refused["applied"], false);
        let applied = app
            .call_tool(
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                    "name": "pump_pulse",
                    "params": [{ "name": "ms", "type": "integer", "required": true }],
                    "exec": { "type": "gpio_write", "pin": 12, "level": 1 }
                }] } }}),
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
            .call_tool(
                "manifest",
                &json!({ "command": "validate", "arguments": { "manifest": [{ "name": "pump_pulse" }] } }),
            )
            .unwrap_err();
        let text = err.message;
        for needle in ["must be a JSON object", "devices", "tools", "pump_pulse"] {
            assert!(text.contains(needle), "the expected shape is not in the error ({needle}): {text}");
        }

        let err = app
            .call_tool("manifest", &json!({ "command": "validate", "arguments": { "manifest": { "tools": [{ "name": 7, "exec": {} }] } } }))
            .unwrap_err();
        assert!(err.message.contains("/tools/0/name"), "no field path: {}", err.message);
        assert!(err.message.contains("devices") && err.message.contains("tools"), "{}", err.message);

        let err = app
            .call_tool(
                "manifest",
                &json!({ "command": "validate", "arguments": { "manifest": { "tool": [] } } }),
            )
            .unwrap_err();
        assert!(err.message.contains("/tool"), "a typo in a field name must be named: {}", err.message);

        // A document of the right shape still validates.
        let ok = app
            .call_tool(
                "manifest",
                &json!({ "command": "validate", "arguments": { "manifest": { "tools": [{
                    "name": "pump_pulse", "params": [],
                    "exec": { "type": "gpio_write", "pin": 12, "level": 1 } }] } } }),
            )
            .unwrap();
        assert_eq!(ok["valid"], true, "{ok}");
    }

    #[test]
    fn safety_limits_are_enforced_on_the_board() {
        let mut app = test_app();
        app.call_tool(
            "manifest",
            &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                "name": "servo_set",
                "params": [{ "name": "deg", "type": "number", "required": true, "min": 0, "max": 180 }],
                "exec": { "type": "pwm", "pin": 7, "freq_hz": 50, "duty_pct": { "param": "deg" } },
                "safety": { "min": 5, "max": 175 }
            }] } }}),
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
            "manifest",
            &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                "name": "relay_pulse",
                "params": [],
                "exec": { "type": "gpio_write", "pin": 5, "level": 1 },
                "safety": { "confirm": true }
            }] } }}),
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
            "manifest",
            &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                "name": "p_keep", "params": [],
                "exec": { "type": "gpio_read", "pin": 3 } }] } }}),
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
        let sid = resp.headers.iter().find(|(k, _)| k == "Mcp-Session-Id").unwrap().1.clone();
        req.method = "DELETE".into();
        req.body.clear();
        req.headers.insert("mcp-session-id".into(), sid.clone());
        for credential in [None, Some("tok"), Some("Basic tok"), Some("Bearer wrong")] {
            req.headers.remove("authorization");
            if let Some(value) = credential {
                req.headers.insert("authorization".into(), value.into());
            }
            assert_eq!(app.route(&req).status, 401, "{credential:?} bypassed session deletion auth");
            assert!(app.runtime.sessions.iter().any(|s| s.id == sid));
        }
        req.headers.insert("authorization".into(), "bearer tok".into());
        assert_eq!(app.route(&req).status, 204);
        assert!(!app.runtime.sessions.iter().any(|s| s.id == sid));
        app.cfg.mcp.enabled = false;
        assert_eq!(app.route(&req).status, 503, "disabled MCP must refuse every mutating method");
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
                "manifest",
                &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                    "name": "climate_read",
                    "description": "blocking read, needs an explicit confirmation",
                    "params": [],
                    "exec": { "type": "driver", "driver": "bme280", "action": "bme280_read" },
                    "safety": { "confirm": true }
                }] } }}),
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
        for (name, args) in [
            ("bme280_read", json!({})),
            ("device_invoke", json!({"device": "bme280", "tool": "bme280_read"})),
        ] {
            let err = app.call_tool(name, &args).expect_err("the driver and generic views share the policy");
            assert!(err.message.contains("confirm"), "{name}: {err}");
        }
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
            "manifest",
            &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                "name": "climate_window",
                "params": [{ "name": "ms", "type": "integer", "min": 10, "max": 5000, "required": true }],
                "exec": { "type": "driver", "driver": "bme280", "action": "bme280_read" }
            }] } }}),
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
            "manifest",
            &json!({"command": "apply", "arguments": { "manifest": { "tools": [{
                "name": "imu_read",
                "params": [],
                "exec": { "type": "driver", "driver": "mpu6050", "action": "mpu6050_read" }
            }] } }}),
        )
        .unwrap();
        assert!(!app.is_visible("imu_read"), "no IMU on this board");
        assert!(!tool_names(&mut app).contains(&"imu_read".to_string()));
        // A tool for a device that *is* present stays listed.
        app.call_tool(
            "manifest",
            &json!({"command": "apply", "arguments": { "mode": "merge", "manifest": { "tools": [{
                "name": "climate_read",
                "params": [],
                "exec": { "type": "driver", "driver": "bme280", "action": "bme280_read" }
            }] } }}),
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
        let v =
            app.call_tool("bus", &json!({"command": "i2c_identify", "arguments": { "addr": 0x76 }})).unwrap();
        assert!(v["candidates"].as_array().unwrap().iter().any(|c| c["driver"] == "bme280"));
        let v =
            app.call_tool("bus", &json!({"command": "i2c_identify", "arguments": { "addr": 0x3C }})).unwrap();
        assert!(v["candidates"].as_array().unwrap().iter().any(|c| c["driver"] == "ssd1306"));
    }

    #[test]
    fn manifest_describe_carries_a_schema_primitives_and_examples() {
        // One section per call: the whole reference does not fit a tool result, so it is
        // split rather than truncated (the model used to receive 2 KB of an 8 KB answer).
        let mut app = test_app();
        let schema = app
            .call_tool("manifest", &json!({"command": "describe", "arguments": { "section": "schema" }}))
            .unwrap();
        assert!(schema["manifest_schema"]["properties"]["tools"].is_object());
        let exec = app
            .call_tool("manifest", &json!({"command": "describe", "arguments": { "section": "exec" }}))
            .unwrap();
        assert!(exec["primitives"].as_array().unwrap().len() >= 8);
        let examples = app
            .call_tool("manifest", &json!({"command": "describe", "arguments": { "section": "examples" }}))
            .unwrap();
        assert!(examples["names"].as_array().unwrap().iter().any(|n| n == "relay_with_safety"));
        let drivers = app
            .call_tool("manifest", &json!({"command": "describe", "arguments": { "section": "drivers" }}))
            .unwrap();
        assert!(drivers["drivers"].as_array().unwrap().iter().any(|d| d["kind"] == "bme280"));
        let one = app
            .call_tool(
                "manifest",
                &json!({"command": "describe", "arguments": { "section": "drivers", "driver": "bme280" }}),
            )
            .unwrap();
        assert_eq!(one["driver"]["kind"], "bme280");
        // The overview is what a first call gets, and it points at the rest.
        let overview = app.call_tool("manifest", &json!({"command": "describe", "arguments": {}})).unwrap();
        assert_eq!(overview["section"], "overview");
        assert!(overview["shape"]["tools"].is_array());
    }

    #[test]
    fn a_declared_manifest_device_is_instantiated() {
        let mut app = test_app();
        // The virtual board has the BME280 at 0x76; remove nothing, but declare an
        // OLED at 0x3D which is *not* on the bus: instantiation must fail softly.
        let v = app
            .call_tool("manifest", &json!({"command": "apply", "arguments": { "manifest": { "devices": [
                    { "id": "board", "kind": "display", "driver": "ssd1306", "bus": 0, "address": 61, "enabled": true }
                ], "tools": [] } }}))
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
        app.agent.archive.push("called: board status({})".into());
        let resp = app.route(&get("/api/agent/prompt"));
        assert_eq!(resp.status, 200);
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        let system = body["system"].as_str().expect("system");
        assert!(system.contains("## Memory (/MEMORY.md)"), "{system}");
        assert!(system.contains("/memory/pins.md"), "{system}");
        // The index is a message now, so it is absent from the prompt — and the endpoint
        // still reports how many lines it holds, which is what the console shows.
        assert!(!system.contains("Earlier in this conversation"), "{system}");
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

        let resp = app.route(&get("/api/files/tree"));
        assert_eq!(resp.status, 200, "{resp:?}");
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        // Directories first, then by name — the order the flat listing had, because the tree
        // is now the only listing: two listings of one workspace were two things to keep equal.
        let entries = body["entries"].as_array().expect("entries");
        let notes = entries.iter().position(|e| e["name"] == "notes").expect("the directory");
        assert_eq!(entries[notes]["dir"], true, "{body}");
        let top = entries.iter().position(|e| e["name"] == "top.md").expect("top.md in the listing");
        assert!(notes < top, "a directory comes before a file: {body}");
        let top = &entries[top];
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
        let resp = app.route(&get("/api/files/read?path=/../etc"));
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["ok"], false, "{body}");
    }

    /// The tree lists file metadata and directories derived from bounded canonical paths,
    /// without returning every file's contents.
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

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
use crate::goal::{Goal, GoalStatus, DEFAULT_MAX_ITERATIONS};
use crate::hardware::Hardware;
use crate::http::{EventSink, EventStreamGate, EventStreamSlot, Handler, Request, Response, StreamBody};
use crate::llm;
use crate::llm::{ChatTransport, LlmConfig};
use crate::logbuf::LogBuf;
use crate::manifest::{Manifest, NVS_KEY_MANIFEST};
use crate::protocol::{self, RpcIn};
use crate::sched::{ScheduledTask, SensorRule};
use crate::state::{make_session_id, ProbeReport, Runtime};
use crate::store::KvStore;
use crate::tool::{ToolCtx, ToolDef, ToolRegistry};
use crate::tools as board_tools;
use crate::tools::{fs as fs_tools, net as net_tools};
use crate::web;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub type Shared = Arc<Mutex<App>>;

/// NVS keys the agent owns. Separate keys keep a write that fails on one of them
/// from taking the others down with it.
pub const NVS_KEY_AGENT_CFG: &str = "agent";
pub const NVS_KEY_SCHED: &str = "sched";
pub const NVS_KEY_WORKSPACE: &str = "ws";
/// The one goal the board may be working towards, as one JSON document.
pub const NVS_KEY_GOAL: &str = "goal";

/// The source a goal iteration's turn carries. It is how a reply is attributed to the
/// goal: the queue merges the sources of everything that landed in one turn, so a
/// turn whose source is exactly this string is one that saw the goal prompt and nothing
/// else.
pub const GOAL_SOURCE: &str = "goal";

/// The longest objective accepted. The objective is copied into every iteration prompt
/// and stored in NVS, so it is bounded like the notes are.
pub const GOAL_OBJECTIVE_MAX: usize = 200;

/// How many bytes the stored goal may take. The board reads NVS through a 4096-byte
/// buffer (`firmware/src/nvs_store.rs`) and a string value above roughly 4000 bytes is
/// refused by ESP-IDF, so the document is trimmed to fit before it is written: a blob
/// the board cannot read back is a goal that disappears at the next boot.
pub const GOAL_BLOB_MAX: usize = 3500;

/// The file tools write into a RAM map that is persisted as one NVS blob, so the
/// whole workspace has to stay small enough to serialise next to everything else.
pub const WORKSPACE_FILE_MAX: usize = 2048;
pub const WORKSPACE_TOTAL_MAX: usize = 8192;

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
}

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
            build: "host".into(),
        }
    }
}

pub struct App {
    pub cfg: Config,
    pub manifest: Manifest,
    pub devices: Devices,
    pub tools: ToolRegistry,
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
    /// The objective the board is working towards on its own, and how far it got.
    /// Idle until a client posts one; the scheduler tick is what advances it.
    pub goal: Goal,
    /// The source of the turn in flight, remembered when the queue is drained because
    /// the message itself is gone by the time the reply comes back. `agent_begin` and
    /// `agent_run_once` set it; `finish_turn` reads it.
    turn_source: String,
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
}

impl App {
    pub fn new(hw: Box<dyn Hardware>, store: Box<dyn KvStore>, log: Arc<LogBuf>, firmware: FwMeta) -> Self {
        let now = now_ms();
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
            tls: TlsInfo::default(),
            agent: Agent::new(),
            llm: LlmConfig::default(),
            tasks: Vec::new(),
            rules: Vec::new(),
            workspace: AppWorkspace::default(),
            goal: idle_goal(),
            turn_source: String::new(),
            tool_transport: None,
            model_transport: None,
            streams: EventStreamGate::new(),
            shared: Weak::new(),
            session_counter: 0,
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
        tools.register_all(self.devices.tool_defs());
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

    /// The tools `mcp.profile = "minimal"` hides: conveniences, never capabilities.
    ///
    /// Each one has an HTTP route or a manifest equivalent, and a model that has only these
    /// removed loses no ability — it loses a shortcut.
    const MINIMAL_HIDDEN: &'static [&'static str] = &[
        "board_config_get",
        "delay",
        "logs_tail",
        "wifi_scan",
        "wifi_status",
        "reboot",
        "factory_reset",
        "manifest_validate",
        "manifest_reset",
    ];

    /// May this tool appear in `tools/list` right now?
    pub fn is_visible(&self, name: &str) -> bool {
        let Some(def) = self.tools.get(name) else {
            return false;
        };
        if self.cfg.mcp.disabled_tools.iter().any(|d| d == name) {
            return false;
        }
        // `minimal` hides conveniences, never capabilities: an HTTP route or a manifest can
        // do these, and a model rarely needs them. Discovery, manifest, GPIO and the file
        // tools stay in every profile — without them an empty board cannot rebuild itself.
        if self.cfg.mcp.profile == "minimal" && Self::MINIMAL_HIDDEN.contains(&name) {
            return false;
        }
        if self.cfg.mcp.tools_mode == "generic" && def.spec.device_tool {
            return false;
        }
        if def.spec.core || def.spec.requires.is_empty() {
            return true;
        }
        if self.cfg.mcp.tools_mode == "all" {
            return true;
        }
        self.caps.contains_all(&def.spec.requires)
    }

    pub fn visible_tool_names(&self) -> Vec<String> {
        self.tools.iter().filter(|d| self.is_visible(&d.spec.name)).map(|d| d.spec.name.clone()).collect()
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

    /// The MCP view of the tools that live outside the registry: `fs_*` then `net_*`.
    ///
    /// `mcp.disabled_tools` still applies; `tools_mode` does not, because none of these
    /// is a device tool.
    fn extra_mcp_specs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for group in [fs_tools::specs(), net_tools::specs(), board_tools::script_specs()] {
            for tool in openai_tools(&group) {
                let name = tool["function"]["name"].as_str().unwrap_or_default().to_string();
                if self.cfg.mcp.disabled_tools.iter().any(|d| d == &name) {
                    continue;
                }
                out.push(json!({
                    "name": name,
                    "description": tool["function"]["description"],
                    "inputSchema": tool["function"]["parameters"],
                }));
            }
        }
        out
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
            "firmware": { "name": "esp32-agent", "version": crate::VERSION },
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
                let mut v = d.portal_json(self.is_visible(&d.spec.name));
                if !v["visible"].as_bool().unwrap_or(false) && !d.spec.requires.is_empty() {
                    v["missing"] = json!(self.caps.missing(&d.spec.requires));
                }
                v
            })
            .collect();
        // The agent's own tools are not in the registry (they need the workspace and the
        // HTTP client), so they never appeared here — which is why the portal could show
        // 40 tools while the model had 49 and `script` looked missing.
        for group in [fs_tools::specs(), net_tools::specs(), board_tools::script_specs()] {
            for entry in group.as_array().into_iter().flatten() {
                let f = &entry["function"];
                let name = f["name"].as_str().unwrap_or("");
                // The read-only ones, by name: the specs are OpenAI-shaped and carry no
                // `mutating` flag, and a made-up one would be worse than this list.
                let read_only = matches!(name, "fs_ls" | "fs_read" | "fs_grep" | "net_get");
                tools.push(json!({
                    "name": name,
                    "title": name,
                    "description": f["description"],
                    "category": "agent",
                    "requires": [],
                    "mutating": !read_only,
                    "core": false,
                    "visible": true,
                    "mcp": true,
                    "input_schema": f["parameters"],
                }));
            }
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
        let now = now_ms();
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
                "tools_visible": self.visible_tool_names().len(),
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
            },
            "sched": {
                "tasks": self.tasks.len(),
                "rules": self.rules.len(),
                "enabled_tasks": self.tasks.iter().filter(|t| t.enabled).count(),
                "enabled_rules": self.rules.iter().filter(|r| r.enabled).count(),
            },
            // The console shows a goal without polling the whole document.
            "goal": {
                "status": self.goal.status.as_str(),
                "iteration": self.goal.iteration,
                "max_iterations": self.goal.max_iterations,
            },
            "pins": self.cfg.pins,
        })
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
    fn run_script(&mut self, args: &Value) -> Result<Value> {
        // No `source` means the caller wants the reference, not a run: a model writing its
        // first script has no other way to learn the syntax, and one paragraph in the tool
        // description is not a language.
        let help_section = crate::json::opt_str(args, "section").unwrap_or_default();
        let wants_help = crate::json::opt_bool(args, "help").unwrap_or(false) || !help_section.is_empty();
        let source = crate::json::opt_str(args, "source").unwrap_or_default();
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
        let mut host = Host { app: self };
        let started = now_ms();
        match crate::script::run(&source, &mut host) {
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
        if !self.is_visible(name) {
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

    fn persist_config(&mut self) {
        let json = self.cfg.to_json_string();
        if self.store.set(crate::config::NVS_KEY_CONFIG, &json) {
            self.log.info("config", "saved");
        } else {
            self.log.error("config", "could not write the config");
        }
        self.runtime.config_dirty = false;
    }

    fn persist_manifest(&mut self) {
        let json = self.manifest.to_json_string();
        if self.store.set(NVS_KEY_MANIFEST, &json) {
            self.runtime.manifest_rev += 1;
            self.log.info("manifest", format!("saved, revision {}", self.runtime.manifest_rev));
        } else {
            self.log.error("manifest", "could not write the manifest");
        }
    }

    /// Work owed after a call or a request: persist, rebuild, notice a restart.
    pub fn settle(&mut self) {
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
        if let Some(text) = self.store.get(NVS_KEY_WORKSPACE) {
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => self.workspace = AppWorkspace::from_json(&v),
                Err(e) => self.log.error("agent", format!("the stored workspace is unreadable ({e})")),
            }
        }
        // A goal survives a reboot, which is the point of storing it: the board may be
        // restarted in the middle of an objective and must not forget it. An idle goal
        // is not stored (it has no objective to store), so a missing key is the normal
        // case, not an error.
        if let Some(text) = self.store.get(NVS_KEY_GOAL) {
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => match Goal::from_json(&v) {
                    Some(goal) => {
                        self.log.info(
                            "goal",
                            format!(
                                "restored `{}` ({}, iteration {})",
                                clip(&goal.objective, 60),
                                goal.status.as_str(),
                                goal.iteration
                            ),
                        );
                        self.goal = goal;
                    }
                    None => self.goal = idle_goal(),
                },
                Err(e) => self.log.error("goal", format!("the stored goal is unreadable ({e})")),
            }
        }
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
        let max_steps = self.llm.max_steps;
        self.turn_source = self.pending_source();
        self.with_agent(move |agent, host| agent.begin(host, now_ms, max_steps))
    }

    /// Phase 2: fold one model reply back in (journal, tool calls, transcript) and say
    /// whether to ask again.
    pub fn agent_reply(&mut self, reply: std::result::Result<llm::ModelReply, String>) -> AgentStep {
        let step = self.with_agent(move |agent, host| agent.apply(host, reply, now_ms));
        if let AgentStep::Done(result) = &step {
            self.finish_turn(result);
        }
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
            self.tasks[i].last_run_ms = at_ms;
            dirty = true;
            if source.len() < 64 && !prompt.trim().is_empty() {
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
        // The goal loop, and the only place an iteration is started: one running goal,
        // nothing else waiting, so a user message or a schedule always goes first. The
        // turn this queues is what `finish_turn` reads back, and the next tick queues
        // the following iteration from the notes it wrote.
        if self.goal.status.is_active() && self.agent.pending.is_none() {
            if let Some(prompt) = self.goal.next_prompt() {
                if self.agent.push_user(&prompt, GOAL_SOURCE, at_ms) {
                    self.log.info(
                        "goal",
                        format!(
                            "queued iteration {} of {}",
                            self.goal.iteration.saturating_add(1),
                            self.goal.max_iterations
                        ),
                    );
                    fired.push(GOAL_SOURCE.to_string());
                }
            }
        }
        if dirty {
            self.persist_sched();
        }
        fired
    }

    // ---------------------------------------------------------------- goal

    /// The source of the message waiting in the queue, or empty when there is none.
    fn pending_source(&self) -> String {
        self.agent.pending.as_ref().map(|p| p.source.clone()).unwrap_or_default()
    }

    /// Fold a finished turn into the goal loop, when that turn was a goal iteration.
    ///
    /// Attribution is the source string: only a turn whose source is exactly
    /// [`GOAL_SOURCE`] saw the goal prompt and nothing else. A message that merged into
    /// the queued goal turn turns the source into a comma list, and a reply to a user,
    /// a schedule or a rule is never classified however it happens to read - which is
    /// what keeps a chat that mentions the marker from ending the goal.
    fn finish_turn(&mut self, result: &std::result::Result<String, String>) {
        if self.turn_source != GOAL_SOURCE {
            return;
        }
        self.turn_source.clear();
        // The goal may have been stopped or deleted while the turn was in flight. Its
        // reply must not revive it: `observe` on a CONTINUE reply would set `Running`
        // again, and a stopped goal must never resume.
        if !self.goal.status.is_active() {
            self.log.info("goal", "dropped an iteration reply: the goal is no longer running");
            return;
        }
        let now = now_ms();
        let status = match result {
            Ok(text) => self.goal.observe(text, now),
            Err(e) => self.goal.fail_iteration(e, now),
        };
        self.log.info(
            "goal",
            format!("iteration {} of {}: {}", self.goal.iteration, self.goal.max_iterations, status.as_str()),
        );
        self.persist_goal();
    }

    /// Write the goal out. Called on every state change, because the board can lose
    /// power between two iterations and the notes are the only memory the loop has.
    fn persist_goal(&mut self) {
        // An idle goal is nothing to remember, and `from_json` refuses a document with
        // no objective anyway; clearing the key keeps "no goal" and "an empty goal"
        // from looking different.
        if self.goal.objective.is_empty() {
            self.store.remove(NVS_KEY_GOAL);
            return;
        }
        // Notes are capped by count and by length, but 24 of them in UTF-8 can still be
        // far larger than one NVS value. The oldest give way, in RAM as well as on
        // flash, so what a reboot restores is exactly what the loop was using rather
        // than a silently shortened copy of it.
        let mut dropped = 0usize;
        let mut text = self.goal.to_json().to_string();
        while text.len() > GOAL_BLOB_MAX && !self.goal.notes.is_empty() {
            self.goal.notes.remove(0);
            dropped += 1;
            text = self.goal.to_json().to_string();
        }
        if dropped > 0 {
            self.log.warn("goal", format!("{dropped} oldest note(s) dropped to fit the stored goal in NVS"));
        }
        if !self.store.set(NVS_KEY_GOAL, &text) {
            self.log.error("goal", "could not save the goal");
        }
    }

    /// `GET /api/goal`: the stored document plus the prompt the next iteration would
    /// send, so a client can see what the board is about to be asked.
    pub fn goal_json(&self) -> Value {
        let mut v = self.goal.to_json();
        v["prompt"] = match self.goal.next_prompt() {
            Some(prompt) => json!(prompt),
            None => Value::Null,
        };
        v
    }

    /// Start a goal, replacing whatever finished goal is stored. A running goal is
    /// refused rather than replaced: one objective at a time is the whole contract, and
    /// silently dropping one half-done objective for another is worse than a 409.
    fn goal_start(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::json_status(400, &json!({ "error": format!("invalid JSON: {e}") })),
        };
        if self.goal.status.is_active() {
            return Response::json_status(
                409,
                &json!({ "error": "a goal is already running; stop or delete it first", "goal": self.goal_json() }),
            );
        }
        let objective = body.get("objective").and_then(|v| v.as_str()).unwrap_or("");
        let requested = body
            .get("max_iterations")
            .and_then(|v| v.as_u64())
            .map(|n| n.min(u64::from(u32::MAX)) as u32)
            .unwrap_or(DEFAULT_MAX_ITERATIONS);
        let objective = clip(objective.trim(), GOAL_OBJECTIVE_MAX);
        let Some(goal) = Goal::new(&objective, requested, now_ms()) else {
            return Response::json_status(400, &json!({ "error": "`objective` is required" }));
        };
        self.log.info(
            "goal",
            format!("started `{}` ({} iterations)", clip(&goal.objective, 80), goal.max_iterations),
        );
        self.goal = goal;
        self.persist_goal();
        Response::json(&self.goal_json())
    }

    /// Stop the loop. `stop` is a decision, not a pause: there is no resume, only a new
    /// goal. A goal that already finished is left as it finished - rewriting `met` into
    /// `stopped` would lose the outcome the notes explain.
    fn goal_stop(&mut self, req: &Request) -> Response {
        let reason = match req.json() {
            Ok(v) => v.get("reason").and_then(|r| r.as_str()).unwrap_or("").to_string(),
            // No body at all is a stop without a reason, not a bad request.
            Err(_) if req.body.is_empty() => String::new(),
            Err(e) => return Response::json_status(400, &json!({ "error": format!("invalid JSON: {e}") })),
        };
        if self.goal.objective.is_empty() {
            return Response::json_status(409, &json!({ "error": "there is no goal to stop" }));
        }
        if self.goal.status.is_active() {
            self.goal.stop(&reason, now_ms());
            self.persist_goal();
            self.log.info("goal", format!("stopped: {}", clip(&self.goal.stop_reason, 80)));
        }
        Response::json(&self.goal_json())
    }

    /// Forget the goal entirely, notes included, and go back to idle.
    fn goal_reset(&mut self) -> Response {
        let had = self.goal.objective.clone();
        self.goal = idle_goal();
        self.persist_goal();
        if !had.is_empty() {
            self.log.info("goal", format!("cleared `{}`", clip(&had, 80)));
        }
        Response::json(&self.goal_json())
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

    fn persist_workspace(&mut self) {
        let text = self.workspace.to_json().to_string();
        if !self.store.set(NVS_KEY_WORKSPACE, &text) {
            self.log.warn("agent", "could not save the file workspace");
        }
    }

    /// Queue one user message. Returns 202 with the queue position; the run
    /// itself happens on the agent thread, so a slow model never blocks a request.
    fn agent_message(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        let text = body.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
        if text.trim().is_empty() {
            return Response::json_status(400, &json!({ "error": "`text` is required" }));
        }
        if text.len() > AGENT_TEXT_MAX {
            return Response::json_status(
                413,
                &json!({ "error": format!("`text` is {} bytes; the limit is {AGENT_TEXT_MAX}", text.len()) }),
            );
        }
        if !self.agent.push_user(&text, "http", now_ms()) {
            return Response::json_status(400, &json!({ "error": "`text` is required" }));
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
        self.turn_source = self.pending_source();
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
        // Only a turn that actually ran is an iteration: a missing transport leaves the
        // message queued for a later attempt, and counting it would spend the goal's
        // budget twice on one prompt.
        if ran {
            self.finish_turn(&out);
        }
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
                "visible_tools": self.visible_tool_names(),
                "hidden": self.tools.iter().filter(|d| !self.is_visible(&d.spec.name))
                    .map(|d| json!({ "tool": d.spec.name, "missing": self.caps.missing(&d.spec.requires) }))
                    .collect::<Vec<Value>>(),
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
            // The autonomous loop. `GET` is open like the transcript and the
            // schedule; everything that changes the goal needs the PIN.
            ("GET", "/api/goal") => Response::json(&self.goal_json()),
            ("POST", "/api/goal") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.goal_start(req)
            }
            ("POST", "/api/goal/stop") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.goal_stop(req)
            }
            ("DELETE", "/api/goal") => {
                if let Err(r) = self.require_pin(req) {
                    return r;
                }
                self.goal_reset()
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
                self.hw.reboot();
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
        for group in [fs_tools::specs(), net_tools::specs(), board_tools::script_specs()] {
            tools.extend(openai_tools(&group));
        }
        tools.sort_by_key(|t| t["function"]["name"].as_str().unwrap_or("").to_string());
        Value::Array(tools)
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
    fn skills(&self) -> Vec<crate::agent::SkillRef> {
        crate::tools::knowledge::index(&self.workspace)
            .into_iter()
            .map(|s| crate::agent::SkillRef { name: s.name, description: s.description, path: s.path })
            .collect()
    }

    /// The memory index: `/memory.md`, one pointer per memory.
    fn memory_index(&self) -> Option<String> {
        crate::tools::knowledge::memory(&self.workspace)
    }

    fn call_tool(&mut self, name: &str, arguments: &Value) -> std::result::Result<String, String> {
        if name.starts_with("fs_") {
            let result = fs_tools::call(&mut self.workspace, name, arguments)
                .ok_or_else(|| format!("`{name}` starts with fs_ but no such file tool exists"))?;
            let text = result?;
            // Only the mutating tools dirty the stored blob.
            if matches!(name, "fs_write" | "fs_edit" | "fs_rm" | "fs_cp" | "fs_mv") {
                self.persist_workspace();
            }
            return Ok(text);
        }
        if name.starts_with("net_") {
            // A request to the board's own server cannot go out over a socket while this
            // call is holding the thread that would answer it: the accept loop is inside
            // this very tool. Answer it in process instead — the model gets the same JSON
            // a socket would have carried, in microseconds, and the board cannot deadlock
            // against itself.
            if let Some((method, path, body)) = self.self_target(arguments) {
                return self.serve_self(&method, &path, body);
            }
            // The turn's own client is elsewhere, so this one is free to be used from
            // inside a tool call — which is what lets the agent reach the network
            // mid-run instead of only between turns.
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

/// The idle goal an `App` starts with and `DELETE /api/goal` restores. Written out
/// because a goal with no objective has no other constructor: `Goal::new` refuses an
/// empty objective, which is what keeps an empty objective out of the loop.
fn idle_goal() -> Goal {
    Goal {
        objective: String::new(),
        status: GoalStatus::Idle,
        iteration: 0,
        max_iterations: DEFAULT_MAX_ITERATIONS,
        failures: 0,
        notes: Vec::new(),
        last_result: String::new(),
        stop_reason: String::new(),
        updated_ms: 0,
    }
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
}

fn task_json(t: &ScheduledTask) -> Value {
    json!({
        "id": t.id,
        "prompt": t.prompt,
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
    if id.is_empty() || prompt.is_empty() {
        return None;
    }
    Some(ScheduledTask {
        id,
        prompt,
        every_s: v.get("every_s").and_then(|e| e.as_u64()).unwrap_or(0).min(u32::MAX as u64) as u32,
        at_hhmm: v.get("at_hhmm").and_then(|a| a.as_str()).unwrap_or("").trim().to_string(),
        enabled: v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
        last_run_ms: v.get("last_run_ms").and_then(|l| l.as_u64()).unwrap_or(0),
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
}

impl AppWorkspace {
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
/// goal note quoting `board_profile` contains `file`, which used to switch the scripted
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::MAX_CONSECUTIVE_FAILURES;
    use crate::store::MemoryStore;
    use crate::virtual_board::VirtualBoard;

    fn test_app() -> App {
        let log = LogBuf::new(256);
        App::new(Box::new(VirtualBoard::new()), Box::new(MemoryStore::new()), log, FwMeta::default())
    }

    /// The map is only worth reading if it is true.
    ///
    /// Every documented route is called here on a real `App`; a 404 means the document
    /// promises something the router does not serve. Mutations are included on purpose —
    /// this is a throwaway app, and a route that answers is exactly what is being asserted.
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
        for kept in [
            "board_status",
            "devices_list",
            "devices_probe",
            "i2c_scan",
            "manifest_apply",
            "manifest_describe",
            "gpio_write",
        ] {
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
        for kept in ["fs_read", "fs_write", "net_get", "script"] {
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
            limits(Duration::from_millis(400), Duration::from_secs(5), Duration::from_secs(10)),
        );
        // The turn keeps running while the stream is open: the next event has to
        // arrive without the client asking again.
        let writer = shared.clone();
        let turn = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
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
            for seq in 1..=40 {
                std::thread::sleep(Duration::from_millis(20));
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
            crate::http::Server::start("127.0.0.1:0", make_handler(shared.clone()), log).expect("bind");
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
        assert!(names.contains(&"board_status"), "got {names:?}");
        assert!(names.contains(&"oled_text"));
        assert!(names.contains(&"fs_read"), "the file tools are missing: {names:?}");
        assert!(names.contains(&"net_get"), "the http tools are missing: {names:?}");
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
    /// Keyword matching used to be substring based, so a goal note quoting
    /// `board_profile` contained `file`: the next iteration switched to the file
    /// script, needed four rounds for a three-step budget, and every iteration after
    /// the first failed with "step limit reached". Matching whole words fixes it.
    #[test]
    fn a_note_quoting_a_profile_does_not_pick_the_file_script() {
        let body = json!({
            "model": "selftest",
            "messages": [{
                "role": "user",
                "content": "Goal: take three small steps\nNotes so far:\n1. board_profile esp32s3-xiaozhi-bread\n"
            }],
            "tools": [{ "type": "function", "function": { "name": "board_status" } }],
        })
        .to_string();
        let reply = selftest_completion(&post("/selftest/chat/completions", &body));
        assert_eq!(
            reply["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            json!("board_status"),
            "a note about a profile must not be read as a request about files"
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

        /// A model that takes 600 ms and then answers plainly.
        struct SlowModel;
        impl crate::llm::ChatTransport for SlowModel {
            fn post_json(
                &mut self,
                _url: &str,
                _headers: &[(&str, &str)],
                _body: &str,
            ) -> std::result::Result<String, String> {
                std::thread::sleep(Duration::from_millis(600));
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
        let run = std::thread::spawn(move || {
            let mut model = SlowModel;
            drive_agent(&app_for_run, &mut model)
        });

        // Let the run get as far as the model call, then act like the portal.
        std::thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        let resp = {
            let mut guard = app.lock().unwrap();
            guard.route(&get("/api/status"))
        };
        let waited = started.elapsed();
        assert_eq!(resp.status, 200, "the portal must still answer");
        assert!(
            waited < Duration::from_millis(300),
            "the app lock was held for {waited:?} while the model was thinking"
        );

        // The transcript is readable mid-run too: the agent is only moved out for the
        // duration of a phase, not for the whole call.
        let mid_run = {
            let mut guard = app.lock().unwrap();
            guard.route(&get("/api/agent/history?after=0"))
        };
        assert_eq!(mid_run.status, 200);

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
        assert!(names.contains(&"board_status"), "got {names:?}");
        assert!(names.contains(&"fs_read"), "got {names:?}");
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
        // The workspace is persisted, so the file survives a reboot.
        let blob = app.store.get(NVS_KEY_WORKSPACE).expect("the workspace was never saved");
        assert!(blob.contains("/notes/a.txt"), "got {blob}");
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
        for needle in
            ["fs_ls", "fs_read", "fs_write", "fs_rm", "fs_cp", "fs_mv", "fs_grep", "net_get", "net_post"]
        {
            assert!(names.contains(&needle.to_string()), "{needle} is missing from tools/list: {names:?}");
        }

        // The schema is the same object the model gets, in the MCP field name.
        let list = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
        let fs_read = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "fs_read")
            .expect("fs_read in the list");
        assert!(fs_read["inputSchema"]["properties"]["path"].is_object(), "{fs_read}");

        // And a call goes through the same code the agent uses.
        let write = app
            .handle_rpc(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"fs_write","arguments":{"path":"/notes/a.txt","text":"from mcp"}}}"#,
                None,
            )
            .unwrap();
        assert_eq!(write["result"]["isError"], json!(false), "{write}");
        let read = app
            .handle_rpc(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fs_read","arguments":{"path":"/notes/a.txt"}}}"#,
                None,
            )
            .unwrap();
        assert_eq!(read["result"]["structuredContent"]["text"], json!("from mcp"), "{read}");

        // `mcp.disabled_tools` hides them like any other tool.
        app.cfg.mcp.disabled_tools = vec!["fs_rm".into()];
        assert!(!tool_names(&mut app).contains(&"fs_rm".to_string()), "disabled_tools did not hide fs_rm");
    }

    #[test]
    fn tools_list_only_shows_detected_hardware() {
        let mut app = test_app();
        let names = tool_names(&mut app);
        assert!(names.contains(&"oled_text".to_string()), "got {names:?}");
        assert!(names.contains(&"bme280_read".to_string()));
        assert!(names.contains(&"board_status".to_string()));
        assert!(!names.contains(&"mpu6050_read".to_string()));
        assert!(!names.contains(&"lcd_text".to_string()));
    }

    #[test]
    fn disabled_tools_disappear_and_missing_caps_hide_devices() {
        let mut app = test_app();
        app.cfg.mcp.disabled_tools = vec!["oled_text".into()];
        assert!(!app.is_visible("oled_text"));
        assert!(app.is_visible("oled_clear"));
        assert!(!app.is_visible("mpu6050_read"));
    }

    #[test]
    fn generic_mode_collapses_device_tools() {
        let mut app = test_app();
        app.cfg.mcp.tools_mode = "generic".into();
        assert!(!app.is_visible("oled_text"), "device tools hidden");
        assert!(app.is_visible("board_status"));
        assert!(app.is_visible("devices_list"));
    }

    #[test]
    fn all_mode_keeps_registered_tools_visible() {
        let mut app = test_app();
        app.cfg.mcp.tools_mode = "all".into();
        // `all` skips capability filtering; it cannot invent a driver for a chip
        // that is not on the bus, so `mpu6050_read` stays absent either way.
        assert!(app.is_visible("oled_text"));
        assert!(app.is_visible("board_status"));
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

        for needle in ["board_status", "manifest_apply", "fs_read", "net_get", "script"] {
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

        for needle in ["script", "fs_read", "fs_write", "net_get", "board_status"] {
            assert!(names.contains(&needle), "{needle} is missing from the portal: {names:?}");
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "the portal list is not alphabetical: {names:?}");

        // The model has exactly this set: no tool is portal-only or model-only.
        let model: Vec<String> = ToolHost::tool_specs(&app)
            .as_array()
            .expect("array")
            .iter()
            .map(|t| t["function"]["name"].as_str().expect("name").to_string())
            .collect();
        // Every model tool is in the portal list ...
        for name in &model {
            assert!(names.contains(&name.as_str()), "{name} reaches the model but not the portal");
        }
        // ... and the portal adds nothing the model cannot call.
        for name in &names {
            assert!(model.iter().any(|m| m == name), "{name} is in the portal but not in the model's list");
        }
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
        assert!(system.contains("fs_read"), "the prompt does not say how to load a skill: {system}");
        assert!(
            system.contains("/skills/deploy/SKILL.md"),
            "the index must carry the path to open, not just a name: {system}"
        );
    }

    /// A long file is carried as its head: the rest stays readable with `fs_read`.
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
        for present in ["fs_ls", "fs_read", "fs_write"] {
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
        assert_eq!(index.len(), 1, "{index:?}");
        assert_eq!(index[0].name, "pins");
        assert_eq!(index[0].description, "GPIO map for the bench");
        assert_eq!(index[0].path, "/skills/pins/SKILL.md");

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
        assert_eq!(index.len(), 1, "{index:?}");
        assert_eq!(index[0].path, "/skills/quick.md");
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
        assert_eq!(app.skills().len(), 1);

        let gone = ToolHost::call_tool(
            &mut app,
            "fs_rm",
            &json!({ "path": "/skills/pins/SKILL.md", "confirm": true }),
        )
        .expect("fs_rm");
        assert!(gone.contains("removed"), "{gone}");
        assert!(app.skills().is_empty(), "the index still lists a deleted skill: {:?}", app.skills());
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
        assert!(app.is_visible("led_set"), "the profile's LED is there by default");

        // Through the config path a person or the agent would use.
        app.call_tool("config_set", &json!({ "patch": { "auto_peripherals": false } })).unwrap();
        for gone in ["led_set", "led_blink", "buttons_read", "button_wait"] {
            assert!(!app.is_visible(gone), "{gone} survived a minimal start");
        }
        assert!(app.is_visible("gpio_write"), "the primitives must stay");
        assert!(app.is_visible("manifest_apply"), "self-configuration must stay");

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
        assert!(app.is_visible("led_set"), "the declared LED did not appear");
        assert!(app.is_visible("buttons_read"), "the declared button did not appear");
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
        for tool in ["mic_level", "mic_record", "speaker_tone", "speaker_play"] {
            assert!(app.is_visible(tool), "{tool} missing: {:?}", app.visible_tool_names());
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
        assert!(rec["pcm_b64"].as_str().unwrap().len() <= 2048, "a tool result is capped at 2 KB");

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
        assert!(app.is_visible("led_set"), "the devkit's LED starts as `led_set`");

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
        assert!(app.is_visible("led_set_2"), "the strip has no tool: {:?}", app.visible_tool_names());

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
        assert!(app.is_visible("manifest_apply"), "the built-in disappeared on rebuild");
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
        assert!(listed.iter().any(|t| t["name"] == "board_status"), "tools/list lost the built-ins");

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
        assert!(names.contains(&"board_status".to_string()), "got {names:?}");
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

    // ---- the goal loop (docs/agent-goal.md) ----

    /// A model with one fixed answer, and the bodies it was sent, so a test can read the
    /// iteration prompt the loop actually built.
    struct FixedModel {
        text: String,
        fail: bool,
        seen: Seen,
    }

    impl ChatTransport for FixedModel {
        fn post_json(
            &mut self,
            _url: &str,
            _headers: &[(&str, &str)],
            body: &str,
        ) -> std::result::Result<String, String> {
            self.seen.lock().unwrap().push((String::new(), Vec::new(), body.to_string()));
            if self.fail {
                return Err("the model endpoint is unreachable".to_string());
            }
            Ok(json!({"choices":[{"message":{"role":"assistant","content": self.text}}]}).to_string())
        }
    }

    fn goal_app(text: &str, fail: bool) -> (App, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let mut app = test_app();
        app.attach_model_transport(Box::new(FixedModel { text: text.to_string(), fail, seen: seen.clone() }));
        app.route(&post(
            "/api/agent/config",
            r#"{"base_url":"http://127.0.0.1:9/v1/chat/completions","model":"stub","max_steps":3,"api_key":""}"#,
        ));
        (app, seen)
    }

    /// The loop end to end: a goal is posted, the tick queues an iteration, the reply
    /// becomes a note, the next prompt carries that note, and the cap ends the goal.
    #[test]
    fn a_goal_iterates_from_its_own_notes_and_ends_at_the_cap() {
        let (mut app, seen) = goal_app("scanned the bus: one device at 0x23", false);
        let started = body_json(
            &app.route(&post("/api/goal", r#"{"objective":"find the light sensor","max_iterations":2}"#)),
        );
        assert_eq!(started["status"], "running");
        assert_eq!(started["iteration"], 0);
        assert_eq!(started["max_iterations"], 2);
        assert!(
            started["prompt"].as_str().unwrap().contains("find the light sensor"),
            "the next prompt is part of the answer: {started}"
        );
        // The POST only starts the goal; the scheduler tick is the loop's clock.
        assert!(app.agent.pending.is_none(), "the route must not queue a turn itself");

        assert_eq!(app.sched_tick(1_000, 0), vec![GOAL_SOURCE.to_string()]);
        assert_eq!(app.agent.pending.as_ref().unwrap().source, "goal");
        // A queued iteration is one turn: further ticks must not stack a second one.
        assert!(app.sched_tick(2_000, 0).is_empty());

        app.agent_run_once().expect("the first iteration should run");
        assert_eq!(app.goal.iteration, 1);
        assert_eq!(app.goal.status, GoalStatus::Running);
        assert_eq!(app.goal.failures, 0);
        assert_eq!(app.goal.notes, vec!["scanned the bus: one device at 0x23".to_string()]);
        assert_eq!(app.goal.last_result, "scanned the bus: one device at 0x23");

        // The second prompt is built from the note the first iteration wrote.
        assert_eq!(app.sched_tick(3_000, 0), vec![GOAL_SOURCE.to_string()]);
        let prompt = app.agent.pending.as_ref().unwrap().text.clone();
        assert!(prompt.contains("1. scanned the bus"), "the note is missing: {prompt}");
        assert!(prompt.contains("Iteration 2 of 2"), "the count is missing: {prompt}");

        // No marker in the reply, so the cap ends it: exhausted, not failed.
        app.agent_run_once().expect("the second iteration should run");
        assert_eq!(app.goal.iteration, 2);
        assert_eq!(app.goal.status, GoalStatus::Exhausted);
        assert!(app.goal.stop_reason.contains('2'), "got `{}`", app.goal.stop_reason);
        assert!(app.sched_tick(4_000, 0).is_empty(), "a finished goal must queue nothing");
        assert!(app.agent.pending.is_none());

        // One model call per iteration, as ordinary agent runs.
        assert_eq!(seen.lock().unwrap().len(), 2, "one model call per iteration");

        // And the state is in NVS, notes included, because that is the loop's memory.
        let blob = app.store.get(NVS_KEY_GOAL).expect("the goal was never saved");
        let stored: Value = serde_json::from_str(&blob).unwrap();
        assert_eq!(stored["status"], "exhausted");
        assert_eq!(stored["notes"].as_array().unwrap().len(), 2, "got {stored}");
    }

    /// A marker in somebody else's reply is not a goal statement: only a turn whose
    /// source is exactly `goal` is classified, so a chat that mentions the marker - and
    /// a message that merged into a queued goal turn - leaves the goal alone.
    #[test]
    fn a_reply_from_another_source_never_ends_the_goal() {
        let (mut app, _seen) = goal_app("GOAL MET", false);
        app.route(&post("/api/goal", r#"{"objective":"arm the door rule","max_iterations":5}"#));

        // A plain chat turn that says the marker ends nothing.
        app.route(&post("/api/agent/message", r#"{"text":"say GOAL MET"}"#));
        app.agent_run_once().expect("the chat turn should run");
        assert_eq!(app.goal.iteration, 0);
        assert_eq!(app.goal.status, GoalStatus::Running);

        // A user message landing on a queued goal turn merges the sources, and the merged
        // turn counts as the user's: no iteration is spent on it.
        assert_eq!(app.sched_tick(5_000, 0), vec![GOAL_SOURCE.to_string()]);
        app.agent.push_user("and how warm is it?", "http", 5_100);
        let source = app.agent.pending.as_ref().unwrap().source.clone();
        assert!(source.contains("http") && source.contains("goal"), "got `{source}`");
        app.agent_run_once().expect("the merged turn should run");
        assert_eq!(app.goal.iteration, 0, "a merged turn is not an iteration");
        assert!(app.goal.notes.is_empty());

        // The loop is still healthy: the next tick queues a clean iteration and its reply
        // is classified, which is how a goal ends when the model says it is done.
        assert_eq!(app.sched_tick(6_000, 0), vec![GOAL_SOURCE.to_string()]);
        app.agent_run_once().expect("the goal turn should run");
        assert_eq!(app.goal.status, GoalStatus::Met);
        assert_eq!(app.goal.iteration, 1);
        assert!(app.sched_tick(7_000, 0).is_empty(), "a met goal queues nothing");
    }

    /// The path the board actually runs: `drive_agent` splits the turn around the model
    /// call, so the iteration is observed in `agent_reply`. `agent_run_once` is the test
    /// and simulator helper; a goal that only worked through it would not iterate on a
    /// real board.
    #[test]
    fn a_goal_iteration_is_observed_on_the_split_turn_path_too() {
        let app = Arc::new(Mutex::new(test_app()));
        {
            let mut guard = app.lock().unwrap();
            guard.route(&post(
                "/api/agent/config",
                r#"{"base_url":"http://127.0.0.1:9/v1/chat/completions","model":"stub","max_steps":3,"api_key":""}"#,
            ));
            guard.route(&post("/api/goal", r#"{"objective":"check the bus","max_iterations":3}"#));
            assert_eq!(guard.sched_tick(1, 0), vec![GOAL_SOURCE.to_string()]);
        }
        let mut model = FixedModel {
            text: "scanned the bus: a BME280 at 0x76".to_string(),
            fail: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let outcome = drive_agent(&app, &mut model).expect("a turn was queued");
        assert!(outcome.is_ok(), "got {outcome:?}");

        let guard = app.lock().unwrap();
        assert_eq!(guard.goal.iteration, 1, "the split path must observe the reply too");
        assert_eq!(guard.goal.notes, vec!["scanned the bus: a BME280 at 0x76".to_string()]);
        assert_eq!(guard.goal.status, GoalStatus::Running);
    }

    /// A model endpoint that is down must not burn the whole budget: three failed
    /// iterations in a row end the goal as blocked, and a failure still costs budget.
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
            .write("/memory.md", "- [pins](/memory/pins.md) — GPIO18 is the strip\n")
            .expect("write");
        app.agent.archive.push("called: board_status({})".into());
        let resp = app.route(&get("/api/agent/prompt"));
        assert_eq!(resp.status, 200);
        let body: Value = serde_json::from_slice(&resp.body).expect("json");
        let system = body["system"].as_str().expect("system");
        assert!(system.contains("## Memory (/memory.md)"), "{system}");
        assert!(system.contains("memory/pins.md"), "{system}");
        assert!(system.contains("Earlier in this conversation"), "{system}");
        assert_eq!(body["archive_lines"], 1);
    }

    #[test]
    fn three_failed_iterations_in_a_row_block_the_goal() {
        let (mut app, _seen) = goal_app("", true);
        app.route(&post("/api/goal", r#"{"objective":"read the display","max_iterations":10}"#));
        for tick in 0..MAX_CONSECUTIVE_FAILURES {
            let fired = app.sched_tick(1_000 * (u64::from(tick) + 1), 0);
            assert_eq!(fired, vec![GOAL_SOURCE.to_string()], "iteration {tick} was not queued");
            assert!(app.agent_run_once().is_err(), "the model call should fail");
        }
        assert_eq!(app.goal.failures, MAX_CONSECUTIVE_FAILURES);
        assert_eq!(app.goal.iteration, MAX_CONSECUTIVE_FAILURES);
        assert_eq!(app.goal.status, GoalStatus::Blocked);
        assert!(
            app.goal.stop_reason.contains(&MAX_CONSECUTIVE_FAILURES.to_string()),
            "the reason must name the count: `{}`",
            app.goal.stop_reason
        );
        assert!(app.goal.notes.iter().all(|n| n.starts_with("failed:")), "got {:?}", app.goal.notes);
        assert!(app.sched_tick(9_000, 0).is_empty(), "a blocked goal queues nothing");
    }

    /// A stop while a turn is in flight wins: the reply is dropped rather than allowed to
    /// set `Running` again, because a stopped goal never resumes.
    #[test]
    fn a_late_reply_does_not_revive_a_stopped_goal() {
        let mut app = test_app();
        app.route(&post("/api/goal", r#"{"objective":"watch the door","max_iterations":5}"#));
        app.route(&post("/api/goal/stop", r#"{"reason":"enough for today"}"#));
        assert_eq!(app.goal.status, GoalStatus::Stopped);

        // The turn that was already running when the stop arrived answers now.
        app.turn_source = GOAL_SOURCE.to_string();
        app.finish_turn(&Ok("one more step".to_string()));
        assert_eq!(app.goal.status, GoalStatus::Stopped, "a stopped goal must not resume");
        assert_eq!(app.goal.iteration, 0, "a dropped reply is not an iteration");
        assert!(app.goal.notes.is_empty());
        assert!(app.sched_tick(1_000, 0).is_empty());
    }

    /// The stored goal is read back at boot: an objective and its notes survive a reboot,
    /// which is the only reason the loop stores them.
    #[test]
    fn a_goal_and_its_notes_survive_a_reboot() {
        let mut app = test_app();
        app.route(&post(
            "/api/goal",
            r#"{"objective":"keep the greenhouse between 18 and 24 degrees","max_iterations":4}"#,
        ));
        app.goal.observe("read 17.9 C, opened the vent", 42);
        app.goal.observe("read 19.4 C, vent is open", 43);
        app.persist_goal();
        let blob = app.store.get(NVS_KEY_GOAL).expect("the goal was never saved");
        assert!(blob.contains("opened the vent"), "the notes must be in the blob: {blob}");

        // A board that reboots keeps the store: the new App must restore it.
        let mut rebooted = test_app();
        rebooted.store.set(NVS_KEY_GOAL, &blob);
        rebooted.load_agent_state();
        assert_eq!(rebooted.goal.objective, "keep the greenhouse between 18 and 24 degrees");
        assert_eq!(rebooted.goal.status, GoalStatus::Running);
        assert_eq!(rebooted.goal.iteration, 2);
        assert_eq!(rebooted.goal.notes.len(), 2);
        assert_eq!(rebooted.goal.notes[1], "read 19.4 C, vent is open");
        assert_eq!(rebooted.goal.max_iterations, 4);
        // Restored means runnable: the tick queues from what was read back.
        assert_eq!(rebooted.sched_tick(50, 0), vec![GOAL_SOURCE.to_string()]);

        // A blob that cannot be a goal leaves the board idle instead of refusing to boot.
        rebooted.store.set(NVS_KEY_GOAL, "{\"objective\":\"\",\"status\":\"running\"}");
        rebooted.load_agent_state();
        assert_eq!(rebooted.goal.status, GoalStatus::Idle);
        assert!(rebooted.goal.objective.is_empty());
    }

    /// The board can only read a bounded NVS value back, so the stored goal has to fit
    /// in one. What gives is the oldest notes - in RAM too, so a reboot restores exactly
    /// what the loop was using instead of a shorter copy nobody has seen.
    #[test]
    fn the_stored_goal_fits_one_nvs_value() {
        let mut app = test_app();
        app.route(&post("/api/goal", r#"{"objective":"keep the bench cool","max_iterations":40}"#));
        // A full ring of maximum-length notes: the worst case the goal can produce.
        let padding = "x".repeat(300);
        for i in 0..24 {
            app.goal.observe(&format!("note-{i:02} {padding}"), i);
        }
        app.persist_goal();

        let blob = app.store.get(NVS_KEY_GOAL).expect("the goal was never saved");
        assert!(blob.len() <= GOAL_BLOB_MAX, "{} bytes would not read back", blob.len());
        // The newest notes are the ones kept: they are what the next step needs.
        assert!(blob.contains("note-23"), "the newest note must survive");
        assert!(!blob.contains("note-00"), "the oldest note must give way");
        assert!(app.goal.notes.len() < 24, "the ring had to shrink to fit: {}", app.goal.notes.len());

        // RAM and NVS agree, so the loop after a reboot continues where this one was.
        let mut rebooted = test_app();
        rebooted.store.set(NVS_KEY_GOAL, &blob);
        rebooted.load_agent_state();
        assert_eq!(rebooted.goal.notes, app.goal.notes);
        assert_eq!(rebooted.goal.iteration, app.goal.iteration);
    }

    /// The routes: the shape, the PIN gate, the 409 on a second goal, the stop and the
    /// delete, and the block `/api/status` grows so a console can watch a goal cheaply.
    #[test]
    fn the_goal_routes_keep_one_goal_at_a_time() {
        let mut app = test_app();
        app.cfg.auth.portal_pin = "1234".into();
        assert_eq!(app.route(&post("/api/goal", r#"{"objective":"x"}"#)).status, 401);
        assert_eq!(app.route(&post("/api/goal/stop", "{}")).status, 401);
        let mut denied = post("/api/goal", "");
        denied.method = "DELETE".into();
        assert_eq!(app.route(&denied).status, 401);
        // Reading the goal is open, like the transcript and the schedule.
        assert_eq!(app.route(&get("/api/goal")).status, 200);
        app.cfg.auth.portal_pin.clear();

        assert_eq!(app.route(&post("/api/goal", r#"{"objective":"   "}"#)).status, 400);
        assert_eq!(app.route(&post("/api/goal", r#"{"objective":"x"}"#)).status, 200);
        let busy = app.route(&post("/api/goal", r#"{"objective":"another"}"#));
        assert_eq!(busy.status, 409, "one goal at a time");
        assert_eq!(body_json(&busy)["goal"]["status"], "running");

        let goal = body_json(&app.route(&get("/api/goal")));
        for key in [
            "objective",
            "status",
            "iteration",
            "max_iterations",
            "failures",
            "notes",
            "last_result",
            "stop_reason",
            "updated_ms",
            "prompt",
        ] {
            assert!(goal.get(key).is_some(), "`{key}` is missing from {goal}");
        }

        let status = body_json(&app.route(&get("/api/status")));
        assert_eq!(status["goal"]["status"], "running");
        assert_eq!(status["goal"]["iteration"], 0);
        assert_eq!(status["goal"]["max_iterations"], DEFAULT_MAX_ITERATIONS);

        let stopped = body_json(&app.route(&post("/api/goal/stop", r#"{"reason":"enough"}"#)));
        assert_eq!(stopped["status"], "stopped");
        assert_eq!(stopped["stop_reason"], "enough");
        // Stopping again is a no-op, not an error, and the reason is not rewritten.
        let again = body_json(&app.route(&post("/api/goal/stop", "")));
        assert_eq!(again["status"], "stopped");
        assert_eq!(again["stop_reason"], "enough");

        let mut wipe = post("/api/goal", "");
        wipe.method = "DELETE".into();
        let cleared = body_json(&app.route(&wipe));
        assert_eq!(cleared["status"], "idle");
        assert_eq!(cleared["objective"], "");
        assert!(cleared["notes"].as_array().unwrap().is_empty());
        assert!(cleared["prompt"].is_null());
        assert!(app.store.get(NVS_KEY_GOAL).is_none(), "a cleared goal must not stay in NVS");
        assert_eq!(app.route(&post("/api/goal/stop", "{}")).status, 409, "nothing left to stop");
    }
}

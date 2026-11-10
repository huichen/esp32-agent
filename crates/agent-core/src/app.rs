//! The application: config + manifest + devices + tools + sessions, the MCP
//! dispatcher, and the HTTP routes for the MCP endpoint and the HTML portal.
//!
//! One `App` lives behind a single mutex. The hardware is inherently shared (one
//! I2C bus, one display), so serialising tool calls costs nothing real and
//! removes a whole class of concurrency bugs.

use crate::agent::{Agent, ToolHost};
use crate::boards::{self, BoardProfile};
use crate::capability::CapSet;
use crate::config::Config;
use crate::device::Devices;
use crate::drivers;
use crate::error::{code, McpError, Result};
use crate::hardware::Hardware;
use crate::http::{Handler, Request, Response};
use crate::llm::{ChatTransport, LlmConfig};
use crate::logbuf::LogBuf;
use crate::manifest::{Manifest, NVS_KEY_MANIFEST};
use crate::protocol::{self, RpcIn};
use crate::sched::{ScheduledTask, SensorRule};
use crate::state::{make_session_id, ProbeReport, Runtime};
use crate::store::KvStore;
use crate::tool::{ToolCtx, ToolRegistry};
use crate::tools as board_tools;
use crate::tools::{fs as fs_tools, net as net_tools};
use crate::web;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Shared = Arc<Mutex<App>>;

/// NVS keys the agent owns. Separate keys keep a write that fails on one of them
/// from taking the others down with it.
pub const NVS_KEY_AGENT_CFG: &str = "agent";
pub const NVS_KEY_SCHED: &str = "sched";
pub const NVS_KEY_WORKSPACE: &str = "ws";

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

#[derive(Debug, Clone)]
pub struct FwMeta {
    pub name: String,
    pub version: String,
    pub build: String,
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
    /// Outbound HTTP for `net_*` and for the model call. The platform layer
    /// attaches it once; host tests leave it empty so nothing reaches the net.
    pub transport: Option<Box<dyn ChatTransport + Send>>,
    /// The model call moves this one out, so `transport` stays available to the
    /// `net_*` tools while a turn is in flight; sharing one instance made every
    /// `net_*` call inside the loop report that no transport was attached.
    pub model_transport: Option<Box<dyn ChatTransport + Send>>,
    session_counter: u64,
}

impl App {
    pub fn new(hw: Box<dyn Hardware>, store: Box<dyn KvStore>, log: Arc<LogBuf>, firmware: FwMeta) -> Self {
        let now = now_ms();
        let cfg = store
            .get(crate::config::NVS_KEY_CONFIG)
            .and_then(|s| Config::from_json_str(&s))
            .unwrap_or_default();
        let (manifest, manifest_source) = match store.get(NVS_KEY_MANIFEST) {
            Some(s) => match Manifest::from_json_str(&s) {
                Ok(m) => (m, "nvs"),
                Err(e) => {
                    log.error("manifest", format!("stored manifest is unreadable ({e}); using an empty one"));
                    (Manifest::default(), "broken")
                }
            },
            None => (Manifest::default(), "default"),
        };
        let mut runtime = Runtime::new(now, &firmware.version, &firmware.build);
        runtime.manifest_rev = 1;
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
            agent: Agent::new(),
            llm: LlmConfig::default(),
            tasks: Vec::new(),
            rules: Vec::new(),
            workspace: AppWorkspace::default(),
            transport: None,
            model_transport: None,
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
            let already = self
                .devices
                .infos()
                .iter()
                .any(|i| i.kind == spec.kind || spec.driver.as_deref() == Some(i.kind.as_str()));
            if already && spec.address.is_none() && spec.pins.is_empty() {
                continue;
            }
            match drivers::instantiate(spec, &mut *self.hw) {
                Ok(dev) => {
                    self.log.info("manifest", format!("instantiated `{}` from the manifest", spec.id));
                    self.devices.push(dev);
                }
                Err(e) => {
                    self.log.warn("manifest", format!("device `{}` failed: {}", spec.id, e.message));
                    declared_errors.push(json!({ "device": spec.id, "error": e.message }));
                }
            }
        }

        let mut caps = CapSet::from_caps(drivers::board_caps(&mut *self.hw, &self.cfg, profile));
        caps.extend(self.devices.caps());
        self.caps = caps;

        let mut tools = ToolRegistry::new();
        tools.register_all(board_tools::always_tools());
        tools.register_all(board_tools::manifest_tools());
        tools.register_all(self.devices.tool_defs());
        tools.register_all(self.manifest.tool_defs());
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

    /// May this tool appear in `tools/list` right now?
    pub fn is_visible(&self, name: &str) -> bool {
        let Some(def) = self.tools.get(name) else {
            return false;
        };
        if self.cfg.mcp.disabled_tools.iter().any(|d| d == name) {
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
        tools.sort_by_key(|t| t["name"].as_str().unwrap_or("").to_string());
        json!({ "tools": tools })
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
        tools.sort_by(|a, b| {
            let ka = (a["category"].as_str().unwrap_or(""), a["name"].as_str().unwrap_or(""));
            let kb = (b["category"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or(""));
            ka.cmp(&kb)
        });
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
        let now = now_ms();
        json!({
            "device_name": self.cfg.device_name,
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
            "pins": self.cfg.pins,
        })
    }

    /// True while the board is waiting to be told which WiFi to join.
    pub fn setup_needed(&mut self) -> bool {
        let wifi = self.hw.wifi_status();
        let ap = wifi.state.contains("ap");
        let sta = wifi.state.contains("connected");
        ap && !sta
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
        if !self.lan_url.is_empty() {
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
        format!("http://{host}:{}{}", self.cfg.mcp.port, self.cfg.mcp.path)
    }

    /// The text shown to the model at `initialize`, generated from what is really
    /// attached so it never promises hardware that is not there.
    fn instructions(&mut self) -> String {
        let mut names: Vec<String> = self.devices.infos().iter().map(|d| d.name.clone()).collect();
        if names.is_empty() {
            names.push("no peripherals detected".into());
        }
        let tools = self.visible_tool_names();
        let setup = self.setup_needed();
        let url = self.mcp_url();
        let soc = self.hw.board_info().soc;
        let board = self.profile.name.to_string();
        format!(
            "esp32-agent v{ver} on {board} ({soc}).\n\
             Detected hardware: {devs}.\n\
             {n} tool(s) available: {tool_list}.\n\
             Tools whose hardware is missing are not listed, so anything you can see is usable.\n\
             This board is self-configuring: use `inventory` / `i2c_identify` to find what is wired, \
             `manifest_describe` for the device+tools schema, and `manifest_apply` to add hardware and tools. \
             A manifest change takes effect immediately 鈥?no reboot and no reflash 鈥?so you can iterate: \
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

    pub fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
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
            let App { hw, devices, cfg, runtime, log, caps, profile, probe, tools, manifest, .. } = self;
            let def = match tools.get(name) {
                Some(d) => d,
                None => return Err(McpError::not_found(format!("no tool `{name}`"))),
            };
            let mut ctx =
                ToolCtx { hw: &mut **hw, devices, cfg, runtime, log, caps, profile, probe, manifest };
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
    }

    /// Hand over the HTTP client the model call and the `net_*` tools use.
    pub fn attach_transport(&mut self, transport: Box<dyn ChatTransport + Send>) {
        self.transport = Some(transport);
    }

    /// The loop's own transport. A platform that can build two cheap clients should
    /// call this too: the turn moves its transport out of `App`, so a `net_*` call
    /// inside that same turn would otherwise find none.
    pub fn attach_model_transport(&mut self, transport: Box<dyn ChatTransport + Send>) {
        self.model_transport = Some(transport);
    }

    /// Run one queued agent turn, if there is one.
    ///
    /// The transcript and the tool host both live on `App`, so the borrow has to be
    /// split here rather than at the call site: the agent is moved out, the transport
    /// is moved out, and both go back afterwards. Returns `None` when nothing is
    /// queued, so a platform thread can call this every second without cost.
    pub fn run_agent_turn(&mut self) -> Option<std::result::Result<String, String>> {
        self.agent.pending.as_ref()?;
        let mut agent = std::mem::replace(&mut self.agent, Agent::new());
        let llm = self.llm.clone();
        let mut transport = match self.model_transport.take() {
            Some(t) => Some(t),
            None => self.transport.take(),
        };
        let outcome = match transport.as_deref_mut() {
            Some(t) => agent.run_pending(self, t, &llm, now_ms),
            None => Err("no HTTP transport attached to the board".to_string()),
        };
        if self.model_transport.is_none() && transport.is_some() {
            self.model_transport = transport;
        } else {
            self.transport = transport;
        }
        self.agent = agent;
        if let Err(e) = &outcome {
            self.agent.last_error = e.clone();
        }
        Some(outcome)
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
        if dirty {
            self.persist_sched();
        }
        fired
    }

    /// The model config as the portal may see it: everything except the key.
    pub fn agent_config_json(&self) -> Value {
        let mut v = llm_json(&self.llm, false);
        v["api_key_set"] = json!(!self.llm.api_key.is_empty());
        v["endpoint_ok"] = json!(self.llm.base_url.starts_with("http://"));
        v["run"] = json!({
            "runs": self.agent.runs,
            "last_error": self.agent.last_error,
            "history": self.agent.history.len(),
            "pending": self.agent.pending.is_some(),
            "busy": self.agent.pending.is_some(),
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

    fn agent_config_apply(&mut self, req: &Request) -> Response {
        let body = match req.json() {
            Ok(v) => v,
            Err(e) => return Response::error_json(400, &format!("invalid JSON: {e}")),
        };
        // No TLS on the board: an https:// endpoint would hang until the timeout,
        // so it is refused with the reason rather than accepted and forgotten.
        if let Some(url) = body.get("base_url").and_then(|u| u.as_str()) {
            let url = url.trim();
            if url.starts_with("https://") {
                return Response::json_status(
                    400,
                    &json!({ "error": "https:// is not supported: the board has no TLS. Use an http:// endpoint (a LAN proxy is the expected setup)." }),
                );
            }
            if !url.is_empty() && !url.starts_with("http://") {
                return Response::json_status(
                    400,
                    &json!({ "error": "`base_url` must start with http:// and point at the chat completions endpoint" }),
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
        // The agent lives in `self` and needs `self` as its tool host, so it is
        // moved out for the duration of the run and put back afterwards.
        let mut agent = std::mem::replace(&mut self.agent, Agent::new());
        let mut transport = self.transport.take();
        let out = match transport.as_mut() {
            Some(t) => agent.run_pending(self, t.as_mut(), &cfg, now_ms),
            None => {
                let why = "no HTTP transport is attached, so the model cannot be reached".to_string();
                agent.last_error = why.clone();
                Err(why)
            }
        };
        self.transport = transport;
        self.agent = agent;
        match &out {
            Ok(text) if !text.is_empty() => self.log.info("agent", format!("run done: {}", clip(text, 120))),
            Ok(_) => self.log.info("agent", "run done with no text"),
            Err(e) => self.log.warn("agent", format!("run failed: {}", clip(e, 160))),
        }
        out
    }

    // ---------------------------------------------------------------- RPC

    pub fn handle_rpc(&mut self, body: &str, session: Option<&str>) -> Option<Value> {
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
            if let Some(v) = self.handle_one(msg, session) {
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

    fn handle_one(&mut self, msg: RpcIn, session: Option<&str>) -> Option<Value> {
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
            "initialize" => Ok(self.on_initialize(&msg.params, session)),
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

    fn on_initialize(&mut self, params: &Value, session: Option<&str>) -> Value {
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
        let instructions = self.instructions();
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
        let mut names: Vec<&str> =
            self.tools.iter().map(|d| d.spec.name.as_str()).filter(|n| self.is_visible(n)).collect();
        names.sort_unstable();

        let id = serde_json::to_string(&msg.id.unwrap_or(Value::Null)).ok()?;
        // Same shape `protocol::ok` produces (serde_json orders object keys).
        let mut out = String::with_capacity(512 + names.len() * 640);
        out.push_str("{\"id\":");
        out.push_str(&id);
        out.push_str(",\"jsonrpc\":\"2.0\",\"result\":{\"tools\":[");
        let mut first = true;
        for name in names {
            let def = self.tools.get(name)?;
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&serde_json::to_string(&def.mcp_json()).ok()?);
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
            ("GET", "/api/status") => {
                let v = self.status_json();
                Response::json(&v)
            }
            ("GET", "/api/config") => Response::json(&self.cfg.public_json()),
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
                let describe = self.call_tool("manifest_describe", &json!({}));
                match describe {
                    Ok(v) => Response::json(&v),
                    Err(e) => Response::json_status(500, &json!({ "error": e.message })),
                }
            }
            ("GET", "/api/devices") => Response::json(&json!({
                "devices": self.devices.infos(),
                "unidentified": self.probe.unknown,
                "buses": self.probe.buses,
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
                let after = req.query_get("after").and_then(|v| v.parse().ok()).unwrap_or(0usize);
                Response::json(&self.agent_history_json(after))
            }
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
                self.agent.reset();
                self.log.info("agent", "transcript cleared from the portal");
                Response::json(&json!({ "reset": true }))
            }
            ("GET", "/api/agent/config") => Response::json(&self.agent_config_json()),
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
                    Response::json(&json!({ "ok": true, "required": !self.cfg.auth.portal_pin.is_empty() }))
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
        let provided = req
            .header("x-portal-pin")
            .map(|s| s.to_string())
            .or_else(|| req.query_get("pin").map(|s| s.to_string()))
            .unwrap_or_default();
        if provided == self.cfg.auth.portal_pin {
            Ok(())
        } else {
            self.runtime.counters.auth_failures += 1;
            Err(Response::json_status(401, &json!({ "error": "PIN required", "pin_required": true })))
        }
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

        let reply = self.handle_rpc(&body, Some(&session_id));
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
        let mut tools: Vec<Value> = self
            .tools
            .iter()
            .filter(|d| self.is_visible(&d.spec.name))
            .map(|d| {
                json!({
                    "type": "function",
                    "function": {
                        "name": d.spec.name,
                        "description": d.spec.description,
                        "parameters": d.spec.input_schema,
                    }
                })
            })
            .collect();
        tools.sort_by_key(|t| t["function"]["name"].as_str().unwrap_or("").to_string());
        // File and HTTP tools gate on nothing the hardware must provide, so they
        // are not MCP tools; they are appended after the board's own list.
        for group in [fs_tools::specs(), net_tools::specs()] {
            tools.extend(openai_tools(&group));
        }
        Value::Array(tools)
    }

    fn call_tool(&mut self, name: &str, arguments: &Value) -> std::result::Result<String, String> {
        if name.starts_with("fs_") {
            let result = fs_tools::call(&mut self.workspace, name, arguments)
                .ok_or_else(|| format!("`{name}` starts with fs_ but no such file tool exists"))?;
            let text = result?;
            // Only the mutating tools dirty the stored blob.
            if matches!(name, "fs_write" | "fs_edit" | "fs_remove") {
                self.persist_workspace();
            }
            return Ok(text);
        }
        if name.starts_with("net_") {
            let mut transport = self.transport.take();
            let result = match transport.as_mut() {
                Some(t) => net_tools::call(t.as_mut(), name, arguments),
                None => {
                    Some(Err("no HTTP transport is attached on this build, so net_* cannot run".to_string()))
                }
            };
            self.transport = transport;
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
        cfg.max_steps = n.clamp(1, 12) as u8;
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

/// The reply the self-test endpoint returns: a `board_status` tool call on the first
/// round, and a final answer quoting the tool result once one is present.
fn selftest_completion(req: &Request) -> Value {
    let body = req.json().unwrap_or(Value::Null);
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

    // Three scripts, chosen by what the user asked for, so one bench run can show
    // each tool family working: the board's own tools, the file workspace, and the
    // HTTP client (which talks to an external host). The file script walks write ->
    // read -> grep so the search path is exercised too, not just the two obvious ones.
    const WRITE: (&str, &str) =
        ("fs_write", r#"{"path":"/bench/hello.txt","text":"written by the self-test turn"}"#);
    const READ: (&str, &str) = ("fs_read", r#"{"path":"/bench/hello.txt"}"#);
    const GREP: (&str, &str) = ("fs_grep", r#"{"pattern":"self-test","path":"/bench"}"#);
    let steps: &[(&str, &str)] = if ask.contains("file") {
        &[WRITE, READ, GREP]
    } else if ask.contains("net") || ask.contains("http") {
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
    json!({
        "id": "selftest",
        "object": "chat.completion",
        "model": "selftest",
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
pub fn make_handler(app: Shared) -> Handler {
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
    use crate::store::MemoryStore;
    use crate::virtual_board::VirtualBoard;

    fn test_app() -> App {
        let log = LogBuf::new(256);
        App::new(Box::new(VirtualBoard::new()), Box::new(MemoryStore::new()), log, FwMeta::default())
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
        }
    }

    fn body_json(resp: &Response) -> Value {
        serde_json::from_slice(&resp.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&resp.body)))
    }

    /// The portal has to carry the agent panel: a panel whose buttons call an
    /// endpoint that does not exist is worse than no panel.
    #[test]
    fn the_portal_ships_the_agent_panel() {
        assert!(web::INDEX_HTML.contains("id=\"panel-agent\""));
        assert!(web::INDEX_HTML.contains("data-tab=\"agent\""));
        for needle in ["/api/agent/message", "/api/agent/history", "/api/agent/config", "/api/sched"] {
            assert!(web::APP_JS.contains(needle), "app.js never calls {needle}");
        }
        // A schedule table that cannot toggle a task does not meet the brief.
        assert!(web::APP_JS.contains("sched_disable") && web::APP_JS.contains("sched_enable"));
        assert!(web::STYLE_CSS.contains(".transcript"));
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

    #[test]
    fn the_model_key_is_never_echoed_and_https_is_refused() {
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

        // No TLS on the board: https is refused with a reason, not accepted.
        let bad = app.route(&post(
            "/api/agent/config",
            r#"{"base_url":"https://api.example.com/v1/chat/completions"}"#,
        ));
        assert_eq!(bad.status, 400);
        assert!(String::from_utf8_lossy(&bad.body).contains("TLS"));
        assert_eq!(app.llm.base_url, "http://192.0.2.10:8899/v1/chat/completions");
        assert_eq!(app.route(&post("/api/agent/config", r#"{"max_steps":99}"#)).status, 200);
        assert_eq!(app.llm.max_steps, 12, "max_steps is clamped, not trusted");
    }

    #[test]
    fn the_agent_writes_need_the_pin_like_every_other_write() {
        let mut app = test_app();
        app.cfg.auth.portal_pin = "1234".into();
        assert_eq!(app.route(&post("/api/agent/message", r#"{"text":"hi"}"#)).status, 401);
        assert_eq!(app.route(&post("/api/agent/reset", "{}")).status, 401);
        assert_eq!(app.route(&post("/api/agent/config", r#"{"model":"x"}"#)).status, 401);
        assert_eq!(app.route(&post("/api/sched", r#"{"kind":"task","op":"delete","id":"t1"}"#)).status, 401);
        // Reads stay open, exactly as they do for the rest of the console.
        assert_eq!(app.route(&get("/api/agent/history")).status, 200);
        assert_eq!(app.route(&get("/api/agent/config")).status, 200);
        assert_eq!(app.route(&get("/api/sched")).status, 200);
        assert!(app.tasks.is_empty(), "an unauthorised write got through");
        assert!(app.agent.history.is_empty());
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

    #[test]
    fn a_queued_message_runs_the_loop_through_the_board_tools() {
        let mut app = test_app();
        let seen = Arc::new(Mutex::new(Vec::new()));
        app.attach_transport(Box::new(StubModel { seen: seen.clone(), calls: 0 }));
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
        assert!(!app.is_visible("pin.led"));
        let v = app
            .call_tool(
                "manifest_apply",
                &json!({
                    "manifest": {
                        "tools": [{
                            "name": "pin.led",
                            "description": "Drive the onboard LED pin",
                            "params": [{ "name": "on", "type": "boolean", "required": true }],
                            "exec": { "type": "gpio_write", "pin": 48, "level": { "param": "on" } }
                        }]
                    }
                }),
            )
            .unwrap();
        assert_eq!(v["applied"], true);
        assert!(app.is_visible("pin.led"), "no reboot, tool is live");
        let names = tool_names(&mut app);
        assert!(names.contains(&"pin.led".to_string()));

        let out = app.call_tool("pin.led", &json!({ "on": true })).unwrap();
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

    #[test]
    fn safety_limits_are_enforced_on_the_board() {
        let mut app = test_app();
        app.call_tool(
            "manifest_apply",
            &json!({ "manifest": { "tools": [{
                "name": "servo.set",
                "params": [{ "name": "deg", "type": "number", "required": true, "min": 0, "max": 180 }],
                "exec": { "type": "pwm", "pin": 7, "freq_hz": 50, "duty_pct": { "param": "deg" } },
                "safety": { "min": 5, "max": 175 }
            }] } }),
        )
        .unwrap();
        // Schema range would allow 180; the safety range does not.
        let err = app.call_tool("servo.set", &json!({ "deg": 180 })).unwrap_err();
        assert_eq!(err.code, code::FORBIDDEN);
        assert!(err.message.contains("safety range"));
        assert!(app.call_tool("servo.set", &json!({ "deg": 90 })).is_ok());
    }

    #[test]
    fn confirm_flag_is_required_for_dangerous_tools() {
        let mut app = test_app();
        app.call_tool(
            "manifest_apply",
            &json!({ "manifest": { "tools": [{
                "name": "relay.pulse",
                "params": [],
                "exec": { "type": "gpio_write", "pin": 5, "level": 1 },
                "safety": { "confirm": true }
            }] } }),
        )
        .unwrap();
        let err = app.call_tool("relay.pulse", &json!({})).unwrap_err();
        assert_eq!(err.code, code::FORBIDDEN);
        assert!(app.call_tool("relay.pulse", &json!({ "confirm": true })).is_ok());
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
                "name": "p.keep", "params": [],
                "exec": { "type": "gpio_read", "pin": 3 } }] } }),
        )
        .unwrap();
        assert_eq!(app.runtime.manifest_rev, 2, "one for boot, one for the save");
        let saved = app.manifest.to_json_string();
        let reloaded = Manifest::from_json_str(&saved).unwrap();
        assert_eq!(reloaded.tool_names(), vec!["p.keep".to_string()]);
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
                    "name": "climate.read",
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
                "name": "climate.window",
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
                "name": "imu.read",
                "params": [],
                "exec": { "type": "driver", "driver": "mpu6050", "action": "mpu6050_read" }
            }] } }),
        )
        .unwrap();
        assert!(!app.is_visible("imu.read"), "no IMU on this board");
        assert!(!tool_names(&mut app).contains(&"imu.read".to_string()));
        // A tool for a device that *is* present stays listed.
        app.call_tool(
            "manifest_apply",
            &json!({ "mode": "merge", "manifest": { "tools": [{
                "name": "climate.read",
                "params": [],
                "exec": { "type": "driver", "driver": "bme280", "action": "bme280_read" }
            }] } }),
        )
        .unwrap();
        assert!(tool_names(&mut app).contains(&"climate.read".to_string()));
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
    fn manifest_describe_carries_schema_primitives_and_examples() {
        let mut app = test_app();
        let v = app.call_tool("manifest_describe", &json!({})).unwrap();
        assert!(v["manifest_schema"]["properties"]["tools"].is_object());
        assert!(v["primitives"].as_array().unwrap().len() >= 8);
        assert!(v["examples"]["relay_with_safety"].is_object());
        assert!(v["drivers"].as_array().unwrap().iter().any(|d| d["kind"] == "bme280"));
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
}

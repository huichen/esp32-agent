//! The device configuration: WiFi, the MCP endpoint, tool selection, buses and
//! pins. Stored as one JSON blob in NVS (or a file on the host simulator) and
//! served/edited by the HTML page at `GET /`.
//!
//! Everything is `#[serde(default)]` with a matching `Default`, so a config
//! written by an older firmware keeps loading after an upgrade and a partial
//! JSON patch from the portal is enough to change one field.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const CONFIG_VERSION: u32 = 1;

/// Default for the flags that are on unless a board says otherwise.
fn default_true() -> bool {
    true
}

fn default_discovery() -> String {
    "auto".to_string()
}

fn default_profile() -> String {
    "full".to_string()
}
/// The name lives in `keys.rs` with every other NVS key: one place, one 15-byte check.
pub const NVS_KEY_CONFIG: &str = crate::keys::CONFIG;
pub const DEFAULT_PORT: u16 = 80;
pub const DEFAULT_PATH: &str = "/mcp";

/// Default interval of the platform scheduler tick, and therefore the default
/// queue-drain latency.
pub const DEFAULT_TICK_MS: u32 = 1000;
/// Below this the board spends its time scheduling instead of working.
pub const MIN_TICK_MS: u32 = 200;
/// Above this a queued message waits long enough to look dropped.
pub const MAX_TICK_MS: u32 = 5000;

/// Clamp a requested tick interval into the range the scheduler can honour. `0`
/// means "not set" (a partial patch or an untouched field), so it takes the
/// default rather than the floor.
pub fn clamp_tick_ms(ms: u32) -> u32 {
    if ms == 0 {
        DEFAULT_TICK_MS
    } else {
        ms.clamp(MIN_TICK_MS, MAX_TICK_MS)
    }
}

/// Default budget for one tool call, in seconds.
///
/// A tool runs on the thread that answers the portal, so an unbounded call is an outage
/// with a plausible excuse. Thirty seconds is longer than the slowest honest call this
/// board makes (`net_get` to a distant host, a 12 s score) and short enough that a hung
/// bus or a peer that never closes does not look like a dead board.
pub const DEFAULT_TOOL_TIMEOUT_S: u32 = 30;
/// Longest budget a configuration may ask for.
pub const MAX_TOOL_TIMEOUT_S: u32 = 300;

/// Clamp a configured tool budget. `0` is a deliberate "no deadline" and is kept: a
/// person debugging a slow peripheral needs the old behaviour, and the platform still
/// has its own watchdog. Any other value is at most five minutes.
pub fn clamp_tool_timeout_s(seconds: u32) -> u32 {
    seconds.min(MAX_TOOL_TIMEOUT_S)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Friendly name reported by MCP `initialize` and shown in the portal.
    pub device_name: String,
    /// Board profile id, or `"auto"` to pick one from the SoC/flash it finds.
    pub board: String,
    /// Hours east of UTC, `-12..=14`, used by `at_hhmm` schedules and by every clock
    /// reading the board reports.
    ///
    /// The board has no timezone database, so a zone name would be a promise it cannot
    /// keep. An offset is the honest unit: it is what `UTC+8` means and it is settable at
    /// runtime, which the old build-time `MCP_TZ_OFFSET_H` was not — changing it needed a
    /// reflash. Half-hour zones are not expressible; that is a real limit, not an
    /// oversight (see `docs/scheduling.md`).
    pub tz_offset_h: i32,
    /// Instantiate the board profile's own peripherals (the devkit's RGB LED and its
    /// BOOT button) at boot.
    ///
    /// `false` is the minimal start: the board comes up with the bus, GPIO and manifest
    /// tools only, and those peripherals exist once a manifest declares them — which is
    /// how a different devkit gets a small surface it grows on purpose.
    #[serde(default = "default_true")]
    pub auto_peripherals: bool,
    /// `auto` (default) adopts every device a driver recognises. `manual` probes and
    /// reports them as candidates but adopts nothing, so the board can boot with an empty
    /// device list and grow it by `manifest_apply` — the agent discovers the hardware and
    /// decides what the board has.
    #[serde(default = "default_discovery")]
    pub discovery: String,
    pub wifi: WifiCfg,
    pub mcp: McpCfg,
    pub bus: BusCfg,
    pub pins: BTreeMap<String, i32>,
    pub auth: AuthCfg,
    pub ui: UiCfg,
    pub log: LogCfg,
    /// The platform half of the agent: how often its loop wakes up.
    pub agent: AgentCfg,
    /// The skills the firmware carries, and whether to install them.
    pub skills: SkillsCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WifiCfg {
    pub ssid: String,
    /// Stored in NVS on the device. Never echoed back by `/api/config`.
    pub password: String,
    pub hostname: String,
    /// Keep trying to reconnect forever in the background.
    pub auto_reconnect: bool,
    /// Disable WiFi power save: lower latency, a little more current draw.
    pub low_latency: bool,
    /// Optional static IPv4 address; empty means DHCP.
    pub ip: String,
    pub netmask: String,
    /// Default gateway. Empty means "whatever the DHCP lease says"; a LAN box that
    /// tunnels traffic is set here.
    pub gateway: String,
    /// Primary DNS server. Empty means the DHCP lease's. A gateway that resolves
    /// through a tunnel usually needs this too: the router would otherwise answer.
    pub dns: String,
    /// Secondary DNS server; empty to leave it alone.
    pub dns2: String,
    /// Start a setup access point when the station fails to connect.
    pub ap_fallback: bool,
    /// Keep that access point up beside the station *after* the board has joined.
    ///
    /// Off by default: the AP exists to get the board onto a network, and a second radio
    /// network that nobody needs is one more thing advertising itself in the room. Once the
    /// station has an address the portal reports it and the AP is closed; the station stays.
    #[serde(default)]
    pub keep_ap: bool,
    pub ap_ssid: String,
    pub ap_password: String,
    /// Seconds without a connection before the setup AP comes up.
    pub ap_after_s: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpCfg {
    /// Master switch for the HTTP/MCP service.
    pub enabled: bool,
    /// Plaintext port. Unused when `plain_http` is false.
    pub port: u16,
    /// Whether the board also answers over plain HTTP.
    ///
    /// Turning it off leaves only https on 443. Two consequences are deliberate and worth
    /// knowing before you turn it off: the console's way back in when a certificate is wrong
    /// disappears with it, and a board whose clock is not yet synced cannot validate
    /// outbound TLS (SNTP needs the link up, not this port). Recovery without HTTP is the
    /// USB cable: `provision` over USB-Serial-JTAG, or a factory reset.
    ///
    /// The board refuses to run with both this and TLS unavailable: a device that answers on
    /// no port at all is not more secure, only bricked.
    pub plain_http: bool,
    /// Path the Streamable HTTP endpoint answers on.
    pub path: String,
    /// `auto` = only tools whose hardware was detected, `all` = every built-in
    /// tool even without the hardware (useful while developing a driver).
    pub tools_mode: String,
    /// Explicit execution refusals, shared by MCP, the on-board agent, scripts,
    /// aliases and family commands. Listing profiles do not impose this policy.
    pub disabled_tools: Vec<String>,
    /// `full` (default) or `minimal`.
    ///
    /// `minimal` hides the tools that are conveniences rather than capabilities: the ones
    /// an HTTP client or a manifest can do as well, and the ones a model rarely needs.
    /// Nothing is deleted — one field switches them back — and the tools that make an empty
    /// board rebuildable (discovery, manifest, GPIO, files) always stay.
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Maximum concurrent MCP sessions kept in memory.
    pub max_sessions: u32,
    /// A session id is forgotten after this many seconds of inactivity.
    pub session_ttl_s: u32,
    /// Advertise `GET` server-sent events on the endpoint (405 when off).
    pub sse: bool,
    /// Answer with permissive CORS headers (needed for browser-based clients).
    pub cors: bool,
    /// Re-scan buses and rebuild the tool list on every `tools/list`.
    pub reprobe_on_list: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BusCfg {
    pub i2c: Vec<I2cBusCfg>,
    /// Extra pins to try as a 1-Wire bus for DS18B20 probes.
    pub onewire_pins: Vec<i32>,
    /// Scan every configured I2C bus at boot and on `/api/devices/probe`.
    pub auto_probe: bool,
    /// Probe a small set of GPIOs for the presence of a button (pull-up test).
    pub probe_gpio: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct I2cBusCfg {
    pub bus: u8,
    pub sda: i32,
    pub scl: i32,
    pub freq_hz: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[derive(Default)]
pub struct AuthCfg {
    /// Bearer token required on the MCP endpoint when non-empty.
    pub mcp_token: String,
    /// Numeric PIN the portal asks for before showing the page when non-empty.
    pub portal_pin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiCfg {
    /// `zh` or `en`: the portal's initial language.
    pub lang: String,
    pub theme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LogCfg {
    pub ring: usize,
    pub level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentCfg {
    /// Interval of the platform scheduler tick, in milliseconds: how often the
    /// schedule is evaluated and the depth-1 queue is drained, so it is the
    /// worst-case latency between a trigger and the run that answers it.
    ///
    /// The platform reads it on every tick instead of once at boot, so a portal edit
    /// takes effect on the next tick.
    pub tick_ms: u32,
    /// Seconds one tool call may spend, as a cooperative deadline.
    ///
    /// The board runs tools on the thread that also answers the portal, so this is the
    /// promise that a stuck bus or a peer that never closes cannot turn a tool call into
    /// an outage. Tools that wait (`shell sleep`, `script sleep`, the HTTP client) ask
    /// how much is left instead of using a fixed cap of their own, and a nested call
    /// (a tool inside a script) inherits the outer budget rather than starting a new one.
    /// A call that starts after the budget is gone is refused with the JSON-RPC timeout
    /// code, and one that overruns anyway says so in its result: this cannot cancel work
    /// already inside a driver.
    ///
    /// `0` disables the deadline and restores the old fixed caps.
    pub tool_timeout_s: u32,
    /// Extra instructions appended to the system prompt, verbatim.
    ///
    /// The static prompt says how to behave on this board; this field is where the owner
    /// says what *their* board is for — "the pump on GPIO12 waters the plants, never run
    /// it longer than two minutes" — without editing firmware. Read and written from the
    /// portal, from `shell system_prompt …`, and from `GET`/`POST /api/agent/prompt`.
    pub system_prompt: String,
    /// Keep the conversation in flash, so a power cut does not restart it from nothing.
    ///
    /// On by default: a board that forgets what it was told whenever the power blinks is
    /// not an agent, and the stored tail is bounded ([`crate::agent::MAX_SAVED_BYTES`])
    /// rather than left to grow. `false` stops both the writes and the restore; the stored
    /// copy is left alone, so switching it back on brings the conversation back.
    #[serde(default = "default_true")]
    pub persist_transcript: bool,
}

/// The skills the firmware ships with, seeded into the workspace on boot.
///
/// Everything here lives on the workspace like any other file — there is no second store
/// (see [`crate::seed`]). This is only the switch: an owner who does not want the three
/// procedures in their prompt, or who has replaced them with their own, turns it off.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SkillsCfg {
    /// Write the built-in skills into `/skills` wherever the file is absent.
    ///
    /// On by default: a freshly flashed board should come up able to write a skill, find
    /// out what is wired and recover itself, without anyone pasting three files in first.
    /// A skill the owner edited is never overwritten, so leaving this on costs nothing
    /// after the first boot. A skill the owner deleted does come back on the next boot;
    /// switching this off is what makes a deletion stick.
    #[serde(default = "default_true")]
    pub seed_defaults: bool,
}

impl Default for SkillsCfg {
    fn default() -> Self {
        Self { seed_defaults: true }
    }
}

/// Long enough for a page of standing orders, short enough that every request carries it.
pub const MAX_SYSTEM_PROMPT: usize = 2048;

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            tick_ms: DEFAULT_TICK_MS,
            tool_timeout_s: DEFAULT_TOOL_TIMEOUT_S,
            system_prompt: String::new(),
            persist_transcript: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            device_name: "esp32-agent".into(),
            board: "auto".into(),
            tz_offset_h: 0,
            auto_peripherals: true,
            discovery: default_discovery(),
            wifi: WifiCfg::default(),
            mcp: McpCfg::default(),
            bus: BusCfg::default(),
            pins: BTreeMap::new(),
            auth: AuthCfg::default(),
            ui: UiCfg::default(),
            log: LogCfg::default(),
            agent: AgentCfg::default(),
            skills: SkillsCfg::default(),
        }
    }
}

impl Default for WifiCfg {
    fn default() -> Self {
        Self {
            ssid: String::new(),
            password: String::new(),
            hostname: "esp32-agent".into(),
            auto_reconnect: true,
            low_latency: true,
            ip: String::new(),
            netmask: String::new(),
            gateway: String::new(),
            dns: String::new(),
            dns2: String::new(),
            ap_fallback: true,
            keep_ap: false,
            ap_ssid: "esp32-agent-setup".into(),
            ap_password: String::new(),
            ap_after_s: 30,
        }
    }
}

impl Default for McpCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_PORT,
            plain_http: true,
            path: DEFAULT_PATH.into(),
            tools_mode: "auto".into(),
            disabled_tools: Vec::new(),
            profile: default_profile(),
            max_sessions: 4,
            session_ttl_s: 900,
            sse: false,
            cors: true,
            reprobe_on_list: false,
        }
    }
}

impl Default for BusCfg {
    fn default() -> Self {
        // Defaults suit an ESP32-S3 DevKitC-1 with the usual wiring; the board
        // profile replaces them when one is selected.
        Self {
            i2c: vec![I2cBusCfg::default()],
            onewire_pins: Vec::new(),
            auto_probe: true,
            probe_gpio: false,
        }
    }
}

impl Default for I2cBusCfg {
    fn default() -> Self {
        Self { bus: 0, sda: 8, scl: 9, freq_hz: 400_000 }
    }
}

impl Default for UiCfg {
    fn default() -> Self {
        Self { lang: "zh".into(), theme: "dark".into() }
    }
}

impl Default for LogCfg {
    fn default() -> Self {
        Self { ring: 96, level: "info".into() }
    }
}

/// The entries of `config.pins` that are settings rather than GPIO numbers.
///
/// `pins` is one map with two kinds of thing in it: a GPIO number (`led`, `button.left`,
/// `mic.bclk`) and a value a driver reads as a parameter (`oled.height`, `oled.sh1106`,
/// `ina219.shunt_mohm`). Only the first kind names a pin, and only the first kind may be
/// checked against the profile's reserved list — reading the second as a pin refuses
/// `{"oled.height": 32}`, the documented fix for a 128×32 panel, on every ESP32-S3, because
/// GPIO32 is the flash there and 64 falls outside the GPIO range. The panel is then drawn as
/// 64 rows and shows snow. These names are the ones `drivers::probe` reads out of the map;
/// anything else in `pins` is a GPIO.
pub const PIN_SETTINGS: &[&str] = &["oled.height", "oled.sh1106", "ina219.shunt_mohm"];

/// The first pin in a config patch that the board profile reserves, with the reason.
///
/// Config is persisted, so a pin that crashes the board does not crash it once: it does it at
/// every boot, and the only way back is erasing the chip over serial. Both edges that accept a
/// patch call this — `POST /api/config` (where the owner can still change what they like, but
/// not that) and the `config_set` tool.
pub fn reserved_pin_problem(profile: &crate::boards::BoardProfile, patch: &Value) -> Option<String> {
    if let Some(pins) = patch.get("pins").and_then(|v| v.as_object()) {
        for (name, value) in pins {
            if PIN_SETTINGS.contains(&name.as_str()) {
                continue;
            }
            let Some(pin) = value.as_i64() else { continue };
            if let Some(why) = profile.pin_problem(pin as i32) {
                return Some(format!("`pins.{name}`: {why}"));
            }
        }
    }
    if let Some(buses) = patch.get("bus").and_then(|b| b.get("i2c")).and_then(|v| v.as_array()) {
        for bus in buses {
            for key in ["sda", "scl"] {
                let Some(pin) = bus.get(key).and_then(|v| v.as_i64()) else { continue };
                if let Some(why) = profile.pin_problem(pin as i32) {
                    return Some(format!("`bus.i2c.{key}`: {why}"));
                }
            }
        }
    }
    if let Some(pins) = patch.get("bus").and_then(|b| b.get("onewire_pins")).and_then(|v| v.as_array()) {
        for pin in pins.iter().filter_map(|v| v.as_i64()) {
            if let Some(why) = profile.pin_problem(pin as i32) {
                // A 1-Wire reset pulse on the flash lines is the same crash by another name.
                return Some(format!("`bus.onewire_pins`: {why}"));
            }
        }
    }
    None
}

/// True when a patch would set a credential the caller would then be the only one to know.
pub fn sets_a_credential(patch: &Value) -> Option<&'static str> {
    let auth = patch.get("auth").and_then(|v| v.as_object())?;
    for key in ["portal_pin", "mcp_token"] {
        let sets = auth.get(key).and_then(|v| v.as_str()).is_some_and(|s| s != "__set__");
        if sets {
            return Some(if key == "portal_pin" { "portal_pin" } else { "mcp_token" });
        }
    }
    None
}

impl Config {
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn from_json_str(s: &str) -> Option<Self> {
        serde_json::from_str::<Config>(s).ok().map(|c| c.sanitized())
    }

    /// The JSON any public config reader may see: both WiFi passwords and the
    /// authentication secrets are replaced by a "set / not set" flag.
    pub fn public_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        v["wifi"]["password"] = json!(if self.wifi.password.is_empty() { "" } else { "__set__" });
        v["wifi"]["ap_password"] = json!(if self.wifi.ap_password.is_empty() { "" } else { "__set__" });
        v["auth"]["mcp_token"] = json!(if self.auth.mcp_token.is_empty() { "" } else { "__set__" });
        v["auth"]["portal_pin"] = json!(if self.auth.portal_pin.is_empty() { "" } else { "__set__" });
        v
    }

    /// Apply a partial JSON object (as posted by the portal) on top of `self`.
    /// Unknown keys are ignored; secrets sent as `__set__` keep the stored
    /// value, while an explicit empty string clears it.
    /// Why `plain_http: false` would leave this board unreachable, when that is knowable from
    /// the configuration alone. The firmware keeps the last TLS failure, so `None` here means
    /// "the config cannot tell", not "a certificate definitely exists".
    pub fn tls_error_hint(&self) -> Option<&'static str> {
        None
    }

    pub fn apply_patch(&mut self, patch: &Value) -> crate::error::Result<Vec<String>> {
        let mut touched = Vec::new();
        if !patch.is_object() {
            return Err(crate::error::McpError::invalid_params("config patch must be an object"));
        }
        if let Some(v) = patch.get("device_name") {
            if let Some(s) = v.as_str() {
                self.device_name = s.trim().to_string();
                touched.push("device_name".into());
            }
        }
        if let Some(v) = patch.get("board") {
            if let Some(s) = v.as_str() {
                self.board = s.trim().to_string();
                touched.push("board".into());
            }
        }
        if let Some(v) = patch.get("tz_offset_h") {
            // Refuse rather than clamp: a silent clamp turns "UTC+5:30" typed as 5.5 into a
            // board that schedules an hour and a half early, and nobody would know.
            let hours = v.as_i64().ok_or_else(|| {
                crate::error::McpError::invalid_params("tz_offset_h must be a whole number of hours")
            })?;
            if !(-12..=14).contains(&hours) {
                return Err(crate::error::McpError::invalid_params("tz_offset_h must be between -12 and 14"));
            }
            self.tz_offset_h = hours as i32;
            touched.push("tz_offset_h".into());
        }
        if let Some(v) = patch.get("agent").and_then(|a| a.get("system_prompt")).and_then(|v| v.as_str()) {
            self.agent.system_prompt = v.chars().take(MAX_SYSTEM_PROMPT).collect();
            touched.push("agent.system_prompt".into());
        }
        if let Some(b) = patch.get("auto_peripherals").and_then(|v| v.as_bool()) {
            self.auto_peripherals = b;
            touched.push("auto_peripherals".into());
        }
        if let Some(d) = patch.get("discovery").and_then(|v| v.as_str()) {
            if d != "auto" && d != "manual" {
                return Err(crate::error::McpError::invalid_params("discovery must be `auto` or `manual`"));
            }
            self.discovery = d.to_string();
            touched.push("discovery".into());
        }
        if let Some(p) = patch.get("mcp").and_then(|v| v.get("profile")).and_then(|v| v.as_str()) {
            if p != "full" && p != "minimal" {
                return Err(crate::error::McpError::invalid_params(
                    "mcp.profile must be `full` or `minimal`",
                ));
            }
            self.mcp.profile = p.to_string();
            touched.push("mcp.profile".into());
        }
        if let Some(w) = patch.get("wifi").and_then(|v| v.as_object()) {
            if let Some(s) = w.get("ssid").and_then(|v| v.as_str()) {
                self.wifi.ssid = s.trim().to_string();
                touched.push("wifi.ssid".into());
            }
            if let Some(s) = w.get("password").and_then(|v| v.as_str()) {
                if s != "__set__" {
                    self.wifi.password = s.to_string();
                    touched.push("wifi.password".into());
                }
            }
            if let Some(s) = w.get("hostname").and_then(|v| v.as_str()) {
                self.wifi.hostname = s.trim().to_string();
                touched.push("wifi.hostname".into());
            }
            if let Some(b) = w.get("auto_reconnect").and_then(|v| v.as_bool()) {
                self.wifi.auto_reconnect = b;
                touched.push("wifi.auto_reconnect".into());
            }
            if let Some(b) = w.get("low_latency").and_then(|v| v.as_bool()) {
                self.wifi.low_latency = b;
                touched.push("wifi.low_latency".into());
            }
            if let Some(b) = w.get("ap_fallback").and_then(|v| v.as_bool()) {
                self.wifi.ap_fallback = b;
                touched.push("wifi.ap_fallback".into());
            }
            if let Some(b) = w.get("keep_ap").and_then(|v| v.as_bool()) {
                self.wifi.keep_ap = b;
                touched.push("wifi.keep_ap".into());
            }
            for key in ["ip", "netmask", "gateway", "dns", "dns2", "ap_ssid"] {
                if let Some(s) = w.get(key).and_then(|v| v.as_str()) {
                    let s = s.trim().to_string();
                    match key {
                        "ip" => self.wifi.ip = s,
                        "netmask" => self.wifi.netmask = s,
                        "gateway" => self.wifi.gateway = s,
                        "dns" => self.wifi.dns = s,
                        "dns2" => self.wifi.dns2 = s,
                        "ap_ssid" => self.wifi.ap_ssid = s,
                        _ => {}
                    }
                    touched.push(format!("wifi.{key}"));
                }
            }
            if let Some(s) = w.get("ap_password").and_then(|v| v.as_str()) {
                if s != "__set__" {
                    self.wifi.ap_password = s.to_string();
                    touched.push("wifi.ap_password".into());
                }
            }
            if let Some(n) = w.get("ap_after_s").and_then(|v| v.as_u64()) {
                self.wifi.ap_after_s = n.min(600) as u32;
                touched.push("wifi.ap_after_s".into());
            }
        }
        if let Some(m) = patch.get("mcp").and_then(|v| v.as_object()) {
            if let Some(b) = m.get("enabled").and_then(|v| v.as_bool()) {
                self.mcp.enabled = b;
                touched.push("mcp.enabled".into());
            }
            if let Some(n) = m.get("port").and_then(|v| v.as_u64()) {
                self.mcp.port = n.min(65535) as u16;
                touched.push("mcp.port".into());
            }
            if let Some(b) = m.get("plain_http").and_then(|v| v.as_bool()) {
                self.mcp.plain_http = b;
                touched.push("mcp.plain_http".into());
                // Switching it on is checked where the listeners are opened (the board
                // knows whether a certificate exists there, not here), but a patch that
                // asks for https-only on a board without a certificate is worth a note in
                // the reply rather than a silent no-op at boot.
                if !b && self.tls_error_hint().is_some() {
                    touched.push("mcp.plain_http:no-certificate".into());
                }
            }
            if let Some(s) = m.get("path").and_then(|v| v.as_str()) {
                self.mcp.path = normalize_path(s);
                touched.push("mcp.path".into());
            }
            if let Some(s) = m.get("tools_mode").and_then(|v| v.as_str()) {
                // `generic` collapses the per-device tools into `device_invoke`, which is
                // the lever that makes a small board's tool list small. It used to be
                // accepted here only as `auto`/`all`, so the mode was documented but
                // unreachable from `config_set` and `/api/config` — and a typo was
                // ignored in silence.
                match s {
                    "auto" | "generic" | "all" => {
                        self.mcp.tools_mode = s.to_string();
                        touched.push("mcp.tools_mode".into());
                    }
                    other => {
                        return Err(crate::error::McpError::invalid_params(format!(
                            "`mcp.tools_mode` is `{other}`; use auto, generic or all"
                        )))
                    }
                }
            }
            if let Some(a) = m.get("disabled_tools").and_then(|v| v.as_array()) {
                self.mcp.disabled_tools = a
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect();
                touched.push("mcp.disabled_tools".into());
            }
            if let Some(b) = m.get("sse").and_then(|v| v.as_bool()) {
                self.mcp.sse = b;
                touched.push("mcp.sse".into());
            }
            if let Some(b) = m.get("cors").and_then(|v| v.as_bool()) {
                self.mcp.cors = b;
                touched.push("mcp.cors".into());
            }
            if let Some(b) = m.get("reprobe_on_list").and_then(|v| v.as_bool()) {
                self.mcp.reprobe_on_list = b;
                touched.push("mcp.reprobe_on_list".into());
            }
            if let Some(n) = m.get("max_sessions").and_then(|v| v.as_u64()) {
                self.mcp.max_sessions = n.clamp(1, 16) as u32;
                touched.push("mcp.max_sessions".into());
            }
            if let Some(n) = m.get("session_ttl_s").and_then(|v| v.as_u64()) {
                self.mcp.session_ttl_s = n.clamp(30, 86_400) as u32;
                touched.push("mcp.session_ttl_s".into());
            }
        }
        if let Some(b) = patch.get("bus").and_then(|v| v.as_object()) {
            if let Some(a) = b.get("i2c").and_then(|v| v.as_array()) {
                let mut buses = Vec::new();
                for (i, item) in a.iter().enumerate() {
                    buses.push(I2cBusCfg {
                        bus: item.get("bus").and_then(|v| v.as_u64()).unwrap_or(i as u64) as u8,
                        sda: item.get("sda").and_then(|v| v.as_i64()).unwrap_or(-1) as i32,
                        scl: item.get("scl").and_then(|v| v.as_i64()).unwrap_or(-1) as i32,
                        freq_hz: item
                            .get("freq_hz")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(400_000)
                            .clamp(10_000, 1_000_000) as u32,
                    });
                }
                self.bus.i2c = buses;
                touched.push("bus.i2c".into());
            }
            if let Some(a) = b.get("onewire_pins").and_then(|v| v.as_array()) {
                self.bus.onewire_pins = a.iter().filter_map(|v| v.as_i64()).map(|v| v as i32).collect();
                touched.push("bus.onewire_pins".into());
            }
            if let Some(v) = b.get("auto_probe").and_then(|v| v.as_bool()) {
                self.bus.auto_probe = v;
                touched.push("bus.auto_probe".into());
            }
            if let Some(v) = b.get("probe_gpio").and_then(|v| v.as_bool()) {
                self.bus.probe_gpio = v;
                touched.push("bus.probe_gpio".into());
            }
        }
        if let Some(p) = patch.get("pins").and_then(|v| v.as_object()) {
            for (k, v) in p {
                if let Some(i) = v.as_i64() {
                    self.pins.insert(k.clone(), i as i32);
                } else if v.is_null() {
                    self.pins.remove(k);
                }
            }
            touched.push("pins".into());
        }
        if let Some(a) = patch.get("auth").and_then(|v| v.as_object()) {
            if let Some(s) = a.get("mcp_token").and_then(|v| v.as_str()) {
                if s != "__set__" {
                    self.auth.mcp_token = s.trim().to_string();
                    touched.push("auth.mcp_token".into());
                }
            }
            if let Some(s) = a.get("portal_pin").and_then(|v| v.as_str()) {
                if s != "__set__" {
                    self.auth.portal_pin = s.trim().to_string();
                    touched.push("auth.portal_pin".into());
                }
            }
        }
        if let Some(u) = patch.get("ui").and_then(|v| v.as_object()) {
            if let Some(s) = u.get("lang").and_then(|v| v.as_str()) {
                self.ui.lang = s.to_string();
                touched.push("ui.lang".into());
            }
            if let Some(s) = u.get("theme").and_then(|v| v.as_str()) {
                self.ui.theme = s.to_string();
                touched.push("ui.theme".into());
            }
        }
        if let Some(l) = patch.get("log").and_then(|v| v.as_object()) {
            if let Some(n) = l.get("ring").and_then(|v| v.as_u64()) {
                self.log.ring = n.clamp(16, 1024) as usize;
                touched.push("log.ring".into());
            }
            if let Some(s) = l.get("level").and_then(|v| v.as_str()) {
                self.log.level = s.to_string();
                touched.push("log.level".into());
            }
        }
        if let Some(a) = patch.get("agent").and_then(|v| v.as_object()) {
            if let Some(n) = a.get("tick_ms").and_then(|v| v.as_u64()) {
                // The tick does not need a reboot: the platform re-reads it every tick.
                self.agent.tick_ms = clamp_tick_ms(n.min(u32::MAX as u64) as u32);
                touched.push("agent.tick_ms".into());
            }
            if let Some(n) = a.get("tool_timeout_s").and_then(|v| v.as_u64()) {
                self.agent.tool_timeout_s = clamp_tool_timeout_s(n.min(u32::MAX as u64) as u32);
                touched.push("agent.tool_timeout_s".into());
            }
            if let Some(b) = a.get("persist_transcript").and_then(|v| v.as_bool()) {
                self.agent.persist_transcript = b;
                touched.push("agent.persist_transcript".into());
            }
        }
        if let Some(s) = patch.get("skills").and_then(|v| v.as_object()) {
            // Turning it on does not seed by itself: the next boot (or a factory reset)
            // is the moment a board's workspace is written, and nothing here writes files
            // behind the caller's back.
            if let Some(b) = s.get("seed_defaults").and_then(|v| v.as_bool()) {
                self.skills.seed_defaults = b;
                touched.push("skills.seed_defaults".into());
            }
        }
        *self = self.clone().sanitized();
        Ok(touched)
    }

    /// Clamp everything that would make the firmware misbehave, and fill in the
    /// defaults the portal is allowed to leave empty.
    pub fn sanitized(mut self) -> Self {
        self.version = CONFIG_VERSION;
        if self.device_name.trim().is_empty() {
            self.device_name = "esp32-agent".into();
        }
        if self.board.trim().is_empty() {
            self.board = "auto".into();
        }
        self.mcp.path = normalize_path(&self.mcp.path);
        if self.mcp.port == 0 {
            self.mcp.port = DEFAULT_PORT;
        }
        // Clamp to the known modes. This used to rewrite everything except `all` back to
        // `auto`, which silently discarded `generic` on every config write — a patch
        // accepted the value and the next `sanitized()` threw it away.
        if !matches!(self.mcp.tools_mode.as_str(), "auto" | "generic" | "all") {
            self.mcp.tools_mode = "auto".into();
        }
        self.mcp.max_sessions = self.mcp.max_sessions.clamp(1, 16);
        self.mcp.session_ttl_s = self.mcp.session_ttl_s.clamp(30, 86_400);
        self.mcp.disabled_tools =
            self.mcp.disabled_tools.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        self.bus.i2c.retain(|b| b.sda >= 0 && b.scl >= 0 && b.sda != b.scl);
        if self.bus.i2c.is_empty() {
            self.bus.i2c = BusCfg::default().i2c;
        }
        self.log.ring = self.log.ring.clamp(16, 1024);
        self.agent.tick_ms = clamp_tick_ms(self.agent.tick_ms);
        self.agent.tool_timeout_s = clamp_tool_timeout_s(self.agent.tool_timeout_s);
        if self.ui.lang != "en" {
            self.ui.lang = "zh".into();
        }
        if self.wifi.hostname.trim().is_empty() {
            self.wifi.hostname = "esp32-agent".into();
        }
        self
    }

    /// Does this config require a restart of the WiFi/MCP stack to take effect?
    pub fn needs_network_restart(&self, other: &Config) -> bool {
        self.wifi != other.wifi || self.mcp.port != other.mcp.port || self.mcp.enabled != other.mcp.enabled
    }
}

fn normalize_path(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() {
        return DEFAULT_PATH.into();
    }
    let mut s = String::with_capacity(p.len() + 1);
    if !p.starts_with('/') {
        s.push('/');
    }
    s.push_str(p.trim_end_matches('/'));
    if s.len() > 32 {
        let mut end = 32;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let c = Config::default().sanitized();
        assert_eq!(c.mcp.port, DEFAULT_PORT);
        assert_eq!(c.mcp.path, DEFAULT_PATH);
        assert_eq!(c.board, "auto");
        // A board joins a network and stops advertising its own: `keep_ap` has to be asked
        // for, and the setup AP is only an on-ramp.
        assert!(!c.wifi.keep_ap, "the setup AP must not stay up by default");
        let mut patched = Config::default();
        patched.apply_patch(&json!({ "wifi": { "keep_ap": true } })).unwrap();
        assert!(patched.wifi.keep_ap);
        let back = Config::from_json_str(&patched.to_json_string()).expect("round trip");
        assert!(back.wifi.keep_ap, "the switch survives a reboot");
    }

    fn synthetic_secret(n: u8) -> String {
        (0..16).map(|i| char::from(b'a' + (i + n) % 26)).collect()
    }

    #[test]
    fn secrets_are_redacted_for_the_portal() {
        let mut c = Config::default();
        c.wifi.password = synthetic_secret(0);
        c.wifi.ap_password = synthetic_secret(1);
        c.auth.mcp_token = synthetic_secret(2);
        c.auth.portal_pin = synthetic_secret(3);
        let v = c.public_json();
        let serialized = v.to_string();
        for (group, field, secret) in [
            ("wifi", "password", &c.wifi.password),
            ("wifi", "ap_password", &c.wifi.ap_password),
            ("auth", "mcp_token", &c.auth.mcp_token),
            ("auth", "portal_pin", &c.auth.portal_pin),
        ] {
            assert_eq!(v[group][field], "__set__", "{group}.{field}");
            assert!(!serialized.contains(secret), "{group}.{field} leaked into public JSON");
        }
    }

    #[test]
    fn patch_keeps_stored_secret_when_masked() {
        let mut c = Config::default();
        let secret = synthetic_secret(0);
        c.wifi.password = secret.clone();
        c.apply_patch(&json!({ "wifi": { "ssid": "home", "password": "__set__" } })).unwrap();
        assert_eq!(c.wifi.ssid, "home");
        assert_eq!(c.wifi.password, secret);
    }

    #[test]
    fn public_config_roundtrip_preserves_secrets_and_explicit_empty_clears_them() {
        let mut c = Config::default();
        c.wifi.password = synthetic_secret(0);
        c.wifi.ap_password = synthetic_secret(1);
        c.auth.mcp_token = synthetic_secret(2);
        c.auth.portal_pin = synthetic_secret(3);
        let before = c.clone();
        let touched = c.apply_patch(&c.public_json()).unwrap();
        assert_eq!(c, before);
        for name in ["wifi.password", "wifi.ap_password", "auth.mcp_token", "auth.portal_pin"] {
            assert!(!touched.iter().any(|field| field == name), "a mask is not a secret write");
        }
        c.apply_patch(&json!({
            "wifi": { "password": "", "ap_password": "" },
            "auth": { "mcp_token": "", "portal_pin": "" },
        }))
        .unwrap();
        let v = c.public_json();
        for (group, field) in
            [("wifi", "password"), ("wifi", "ap_password"), ("auth", "mcp_token"), ("auth", "portal_pin")]
        {
            assert_eq!(v[group][field], "");
        }
    }

    #[test]
    fn empty_config_json_roundtrips_to_defaults() {
        let c = Config::from_json_str("{}").unwrap();
        assert_eq!(c.mcp.port, DEFAULT_PORT);
    }

    #[test]
    fn paths_are_normalized() {
        let mut c = Config::default();
        c.apply_patch(&json!({ "mcp": { "path": "api/mcp/" } })).unwrap();
        assert_eq!(c.mcp.path, "/api/mcp");

        // The 32-byte path ceiling may fall inside a UTF-8 character. Both a live
        // patch and a stored config must shorten it without panicking.
        let path = format!("{}é/", "a".repeat(30));
        c.apply_patch(&json!({ "mcp": { "path": path } })).unwrap();
        assert_eq!(c.mcp.path, format!("/{}", "a".repeat(30)));
        let stored = Config::from_json_str(&json!({ "mcp": { "path": "界".repeat(20) } }).to_string())
            .expect("a long Unicode path still loads");
        assert_eq!(stored.mcp.path, format!("/{}", "界".repeat(10)));
        assert!(stored.mcp.path.len() <= 32);
    }

    #[test]
    fn invalid_ports_and_buses_are_clamped() {
        let mut c = Config::default();
        c.apply_patch(&json!({ "mcp": { "port": 70000, "max_sessions": 999 } })).unwrap();
        assert_eq!(c.mcp.port, 65535);
        assert_eq!(c.mcp.max_sessions, 16);
    }

    #[test]
    fn the_agent_tick_defaults_to_one_second() {
        let c = Config::default().sanitized();
        assert_eq!(c.agent.tick_ms, DEFAULT_TICK_MS);
        assert_eq!(DEFAULT_TICK_MS, 1000);
        // A config stored by a firmware that predates the field still loads.
        let old = Config::from_json_str(r#"{"device_name":"lab","mcp":{"port":9000}}"#).unwrap();
        assert_eq!(old.agent.tick_ms, DEFAULT_TICK_MS);
        assert_eq!(old.mcp.port, 9000);
        // `0` means "not set", not "as fast as possible".
        assert_eq!(clamp_tick_ms(0), DEFAULT_TICK_MS);
    }

    #[test]
    fn a_tool_budget_is_clamped_and_zero_means_no_deadline() {
        let mut c = Config::default();
        assert_eq!(c.agent.tool_timeout_s, DEFAULT_TOOL_TIMEOUT_S);
        c.apply_patch(&json!({ "agent": { "tool_timeout_s": 5 } })).unwrap();
        assert_eq!(c.agent.tool_timeout_s, 5);
        c.apply_patch(&json!({ "agent": { "tool_timeout_s": 9999 } })).unwrap();
        assert_eq!(c.agent.tool_timeout_s, MAX_TOOL_TIMEOUT_S, "five minutes is the ceiling");
        assert_eq!(clamp_tool_timeout_s(0), 0, "0 is a deliberate 'no deadline', not the floor");
        let stored = Config::from_json_str(r#"{"agent":{"tool_timeout_s":9000}}"#).unwrap();
        assert_eq!(stored.agent.tool_timeout_s, MAX_TOOL_TIMEOUT_S);
        // The number reaches the portal read-back, so a person can see what is in force.
        let public = c.public_json();
        assert_eq!(public["agent"]["tool_timeout_s"], json!(MAX_TOOL_TIMEOUT_S));
    }

    #[test]
    fn a_tick_patch_is_clamped_and_survives_a_roundtrip() {
        let mut c = Config::default();
        c.apply_patch(&json!({ "agent": { "tick_ms": 500 } })).unwrap();
        assert_eq!(c.agent.tick_ms, 500);
        c.apply_patch(&json!({ "agent": { "tick_ms": 50 } })).unwrap();
        assert_eq!(c.agent.tick_ms, MIN_TICK_MS, "a 50 ms tick would starve the board");
        c.apply_patch(&json!({ "agent": { "tick_ms": 90_000 } })).unwrap();
        assert_eq!(c.agent.tick_ms, MAX_TICK_MS, "a 90 s tick reads as a dropped message");
        // Out of range at deserialization time is clamped too, not just through a patch.
        let stored = Config::from_json_str(r#"{"agent":{"tick_ms":100}}"#).unwrap();
        assert_eq!(stored.agent.tick_ms, MIN_TICK_MS);

        // The value is stored and reported as JSON, so a GET can echo what a POST took.
        c.apply_patch(&json!({ "agent": { "tick_ms": 250 } })).unwrap();
        let json = c.public_json();
        assert_eq!(json["agent"]["tick_ms"], 250);
        let back = Config::from_json_str(&c.to_json_string()).unwrap();
        assert_eq!(back.agent.tick_ms, 250);

        // And it needs no reboot: the platform loop re-reads it every tick.
        let mut faster = back.clone();
        faster.apply_patch(&json!({ "agent": { "tick_ms": 200 } })).unwrap();
        assert!(!back.needs_network_restart(&faster));
    }

    #[test]
    fn a_setting_in_the_pins_map_is_not_checked_as_a_gpio() {
        // This board is the one that bites: its panel is 128×32, GPIO32 is the flash on an
        // ESP32-S3, and 64 (the taller glass) is outside the GPIO range at all. Both of the
        // values that make the screen work used to come back as a reserved-pin refusal.
        let profile = crate::boards::find("esp32s3-xiaozhi-bread").expect("the bench profile");
        for patch in [
            json!({ "pins": { "oled.height": 32 } }),
            json!({ "pins": { "oled.height": 64 } }),
            json!({ "pins": { "oled.sh1106": 1 } }),
            json!({ "pins": { "ina219.shunt_mohm": 100 } }),
        ] {
            assert_eq!(reserved_pin_problem(profile, &patch), None, "{patch} is a setting");
        }
        // A real GPIO in the same map is still refused, so the guard keeps its teeth.
        let refused = reserved_pin_problem(profile, &json!({ "pins": { "led": 32 } }))
            .expect("GPIO32 is reserved on this board");
        assert!(refused.contains("GPIO") || refused.contains("reserved"), "{refused}");
        // And the panel height lands in the config, not just past the guard.
        let mut c = Config::default();
        c.apply_patch(&json!({ "pins": { "oled.height": 32 } })).unwrap();
        assert_eq!(c.pins.get("oled.height"), Some(&32));
    }
}

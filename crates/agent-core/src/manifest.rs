//! The manifest: hardware and tool definitions as data, applied without a reflash.
//!
//! Three layers, and this file is the middle one:
//!
//! ```text
//! drivers (compiled)   register-level code for a chip; only needed for timing,
//!                      calibration or a state machine
//!                     resolved by id
//! manifest (this file, JSON in NVS)   which devices exist and which tools expose
//!                      them; primitives (gpio/pwm/adc/i2c/rgb/delay) compose into
//!                      composite tools without any new firmware
//!                     compiled into ToolDef
//! MCP (protocol.rs, app.rs)           device-independent glue, written once
//! ```
//!
//! A model can read the schema and the driver catalogue, write a new manifest and
//! call it: the tool list is rebuilt in place, so no reboot and no reflash. Every
//! limit is enforced here, on the board, before anything touches a pin: a schema is
//! documentation, not security.

use crate::error::{McpError, Result};
use crate::hardware::PinMode;
use crate::tool::{object_schema, ToolCtx, ToolDef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const MANIFEST_VERSION: u32 = 1;
pub const NVS_KEY_MANIFEST: &str = "manifest";
pub const MAX_TOOLS: usize = 48;
pub const MAX_STEPS: usize = 8;
pub const MAX_TOTAL_DELAY_MS: u32 = 5_000;

/// A number that is either literal in the manifest or taken from a tool argument.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Num {
    Lit(f64),
    Param { param: String },
}

impl Num {
    pub fn lit(v: i64) -> Self {
        Num::Lit(v as f64)
    }

    pub fn resolve(&self, args: &Value) -> Result<f64> {
        match self {
            Num::Lit(v) => Ok(*v),
            Num::Param { param } => {
                let v = args
                    .get(param)
                    .ok_or_else(|| McpError::invalid_params(format!("`{param}` is required by this tool")))?;
                if let Some(n) = v.as_f64() {
                    Ok(n)
                } else if let Some(b) = v.as_bool() {
                    // A boolean is a level: true = 1, false = 0.
                    Ok(if b { 1.0 } else { 0.0 })
                } else if let Some(s) = v.as_str() {
                    s.trim()
                        .parse::<f64>()
                        .map_err(|_| McpError::invalid_params(format!("`{param}` must be a number")))
                } else {
                    Err(McpError::invalid_params(format!("`{param}` must be a number")))
                }
            }
        }
    }

    pub fn resolve_i32(&self, args: &Value) -> Result<i32> {
        Ok(self.resolve(args)?.round() as i32)
    }

    pub fn resolve_u32(&self, args: &Value) -> Result<u32> {
        let v = self.resolve(args)?;
        if v < 0.0 || v > u32::MAX as f64 {
            return Err(McpError::invalid_params("value out of range"));
        }
        Ok(v.round() as u32)
    }
}

/// A string that is literal or taken from a tool argument.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Text {
    Lit(String),
    Param { param: String },
}

impl Text {
    pub fn resolve(&self, args: &Value) -> Result<String> {
        match self {
            Text::Lit(s) => Ok(s.clone()),
            Text::Param { param } => args
                .get(param)
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .ok_or_else(|| McpError::invalid_params(format!("`{param}` is required by this tool"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Manifest {
    pub version: u32,
    /// Devices the manifest declares. `driver` instantiates a compiled driver;
    /// without it the entry only documents pins and binds tools.
    pub devices: Vec<DeviceSpec>,
    pub tools: Vec<ManifestTool>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self { version: MANIFEST_VERSION, devices: Vec::new(), tools: Vec::new() }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct DeviceSpec {
    pub id: String,
    /// Free-form kind: `sensor`, `display`, `actuator`, `board`.
    pub kind: String,
    /// Compiled driver id (`bme280`, `ssd1306`, ...) when a driver is available.
    pub driver: Option<String>,
    pub pins: BTreeMap<String, i32>,
    pub bus: Option<u8>,
    pub address: Option<u8>,
    pub options: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub note: String,
}

impl DeviceSpec {
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ManifestTool {
    pub name: String,
    pub title: String,
    pub description: String,
    /// Device this tool belongs to, for grouping and `driver` resolution.
    pub device: Option<String>,
    pub params: Vec<Param>,
    pub exec: Exec,
    pub safety: Safety,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub description: String,
    pub required: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: String,
    #[serde(rename = "enum")]
    pub values: Vec<String>,
    pub default: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Safety {
    /// Absolute limits enforced before the exec runs, independent of the schema.
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Refuse more than this many calls per minute.
    pub max_calls_per_min: Option<u32>,
    /// The call must carry `"confirm": true`.
    pub confirm: bool,
}

/// The primitives. Each one is a few lines against [`super::hardware::Hardware`],
/// which is why a new peripheral needs a manifest, not a firmware change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Exec {
    GpioWrite {
        pin: Num,
        level: Num,
    },
    GpioRead {
        pin: Num,
    },
    Toggle {
        pin: Num,
        times: Num,
        gap_ms: Num,
    },
    Pwm {
        pin: Num,
        freq_hz: Num,
        duty_pct: Num,
    },
    Adc {
        pin: Num,
    },
    I2cRead {
        addr: Num,
        reg: Num,
        length: Num,
        bus: Option<u8>,
    },
    I2cWrite {
        addr: Num,
        reg: Num,
        data: Value,
        bus: Option<u8>,
    },
    /// Addressable RGB: colour is `#rrggbb` or a param name.
    Rgb {
        pin: Num,
        color: Text,
        count: Option<u16>,
    },
    Delay {
        ms: Num,
    },
    /// Call a detected device's own driver tool (e.g. `bme280` `read`).
    Driver {
        driver: String,
        action: String,
    },
    Sequence {
        steps: Vec<Step>,
    },
}

impl Default for Exec {
    fn default() -> Self {
        Exec::Delay { ms: Num::lit(0) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Step {
    /// Milliseconds to wait after this step.
    pub ms: u32,
    pub exec: Exec,
}

// ------------------------------------------------------------------ validation

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    /// JSON pointer to the offending field, e.g. `/tools/2/exec/pin`.
    pub path: String,
    pub message: String,
    pub hint: String,
}

impl Issue {
    fn new(path: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { path: path.to_string(), message: message.into(), hint: hint.into() }
    }
}

/// Facts the validator needs from the running board.
pub struct ValidateCtx<'a> {
    pub caps: &'a crate::capability::CapSet,
    pub buses: Vec<u8>,
    pub drivers: Vec<String>,
    pub known_device_ids: Vec<String>,
}

impl Manifest {
    pub fn to_json_string(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn from_json_str(s: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str::<Manifest>(s)
    }

    /// Full validation. An empty vector means "safe to apply".
    pub fn validate(&self, ctx: &ValidateCtx) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.tools.len() > MAX_TOOLS {
            issues.push(Issue::new(
                "/tools",
                format!("{} tools is more than the limit of {MAX_TOOLS}", self.tools.len()),
                "Group related actions into one tool with a `mode` parameter, or use the generic tool style.",
            ));
        }

        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, dev) in self.devices.iter().enumerate() {
            let p = format!("/devices/{i}");
            if dev.id.trim().is_empty() {
                issues.push(Issue::new(
                    &format!("{p}/id"),
                    "device id is empty",
                    "Use a short id like `led` or `bme0`.",
                ));
            }
            if !is_ident(&dev.id) {
                issues.push(Issue::new(
                    &format!("{p}/id"),
                    format!("`{}` is not a valid id", dev.id),
                    "Allowed: a-z A-Z 0-9 _ - (max 32).",
                ));
            }
            if let Some(driver) = &dev.driver {
                if !ctx.drivers.iter().any(|d| d == driver) {
                    issues.push(Issue::new(
                        &format!("{p}/driver"),
                        format!("unknown driver `{driver}`"),
                        format!("Known drivers: {}", ctx.drivers.join(", ")),
                    ));
                }
            }
            if let (Some(bus), Some(addr)) = (dev.bus, dev.address) {
                if !ctx.buses.contains(&bus) {
                    issues.push(Issue::new(
                        &format!("{p}/bus"),
                        format!("I2C bus {bus} is not configured"),
                        "Add the bus under `config.bus.i2c`, or use bus 0.",
                    ));
                }
                if !(1..=127).contains(&addr) {
                    issues.push(Issue::new(
                        &format!("{p}/address"),
                        "address must be 1..=127",
                        "I2C addresses are 7-bit: 0x3C = 60.",
                    ));
                }
            }
            for (name, pin) in &dev.pins {
                if !(0..=48).contains(pin) {
                    issues.push(Issue::new(
                        &format!("{p}/pins/{name}"),
                        format!("pin {pin} is outside 0..=48"),
                        "ESP32-S3 has GPIO0..GPIO48.",
                    ));
                }
            }
        }

        for (i, tool) in self.tools.iter().enumerate() {
            let p = format!("/tools/{i}");
            if !is_tool_name(&tool.name) {
                issues.push(Issue::new(
                    &format!("{p}/name"),
                    format!("`{}` is not a valid tool name", tool.name),
                    format!(
                        "Allowed: a-z, A-Z, 0-9, `_` and `-` (max 48), e.g. `led_set`. Got `{}`. \
                         Every request carries the whole tool list, so one name outside that class \
                         fails every model call until the manifest changes.",
                        tool.name
                    ),
                ));
            }
            if let Some(first) = seen.insert(&tool.name, i) {
                issues.push(Issue::new(
                    &format!("{p}/name"),
                    format!("duplicate tool `{}` (also /tools/{first})", tool.name),
                    "Tool names must be unique.",
                ));
            }
            for (j, param) in tool.params.iter().enumerate() {
                let pp = format!("{p}/params/{j}");
                if !is_ident(&param.name) {
                    issues.push(Issue::new(
                        &format!("{pp}/name"),
                        format!("`{}` is not a valid parameter name", param.name),
                        "Allowed: a-z A-Z 0-9 _ -.",
                    ));
                }
                if !matches!(param.kind.as_str(), "integer" | "number" | "string" | "boolean") {
                    issues.push(Issue::new(
                        &format!("{pp}/type"),
                        format!("unknown type `{}`", param.kind),
                        "One of: integer, number, string, boolean.",
                    ));
                }
                if let (Some(min), Some(max)) = (param.min, param.max) {
                    if min > max {
                        issues.push(Issue::new(
                            &format!("{pp}/min"),
                            "min is greater than max",
                            "Swap the values.",
                        ));
                    }
                }
                if let Some(d) = &param.default {
                    if let Some(e) = check_value(param, d, &pp) {
                        issues.push(e);
                    }
                }
            }
            if let (Some(min), Some(max)) = (tool.safety.min, tool.safety.max) {
                if min > max {
                    issues.push(Issue::new(
                        &format!("{p}/safety/min"),
                        "safety min > max",
                        "Swap the values.",
                    ));
                }
            }
            self.validate_exec(&tool.exec, &format!("{p}/exec"), ctx, &tool.device, &mut issues);
        }
        issues
    }

    fn validate_exec(
        &self,
        exec: &Exec,
        path: &str,
        ctx: &ValidateCtx,
        device: &Option<String>,
        issues: &mut Vec<Issue>,
    ) {
        match exec {
            Exec::GpioWrite { pin, .. } | Exec::GpioRead { pin } | Exec::Adc { pin } => {
                self.check_pin(pin, &format!("{path}/pin"), issues);
            }
            Exec::Toggle { pin, times, gap_ms } => {
                self.check_pin(pin, &format!("{path}/pin"), issues);
                if let Num::Lit(t) = times {
                    if *t < 1.0 || *t > 20.0 {
                        issues.push(Issue::new(
                            &format!("{path}/times"),
                            "times must be 1..=20",
                            "Keep toggling short.",
                        ));
                    }
                }
                if let Num::Lit(ms) = gap_ms {
                    if *ms < 10.0 || *ms > 1000.0 {
                        issues.push(Issue::new(
                            &format!("{path}/gap_ms"),
                            "gap_ms must be 10..=1000",
                            "Human-visible blink rates live there.",
                        ));
                    }
                }
            }
            Exec::Pwm { pin, freq_hz, duty_pct } => {
                self.check_pin(pin, &format!("{path}/pin"), issues);
                if let Num::Lit(f) = freq_hz {
                    if *f < 1.0 || *f > 40_000.0 {
                        issues.push(Issue::new(
                            &format!("{path}/freq_hz"),
                            "freq_hz must be 1..=40000",
                            "Servos use 50, LEDs 1000-5000.",
                        ));
                    }
                }
                if let Num::Lit(d) = duty_pct {
                    if *d < 0.0 || *d > 100.0 {
                        issues.push(Issue::new(
                            &format!("{path}/duty_pct"),
                            "duty_pct must be 0..=100",
                            "Percentage, not a raw compare value.",
                        ));
                    }
                }
            }
            Exec::I2cRead { addr, reg, length, bus } => {
                self.check_addr(addr, &format!("{path}/addr"), issues);
                if let Num::Lit(r) = reg {
                    if *r < 0.0 || *r > 255.0 {
                        issues.push(Issue::new(
                            &format!("{path}/reg"),
                            "reg must be 0..=255",
                            "Register numbers are one byte.",
                        ));
                    }
                }
                if let Num::Lit(l) = length {
                    if *l < 1.0 || *l > 64.0 {
                        issues.push(Issue::new(
                            &format!("{path}/length"),
                            "length must be 1..=64",
                            "Read the datasheet, not the whole address space.",
                        ));
                    }
                }
                self.check_bus(*bus, &format!("{path}/bus"), ctx, issues);
            }
            Exec::I2cWrite { addr, reg, data, bus } => {
                self.check_addr(addr, &format!("{path}/addr"), issues);
                if let Num::Lit(r) = reg {
                    if *r < 0.0 || *r > 255.0 {
                        issues.push(Issue::new(
                            &format!("{path}/reg"),
                            "reg must be 0..=255",
                            "Register numbers are one byte.",
                        ));
                    }
                }
                if !data.is_array() && data.get("param").is_none() {
                    issues.push(Issue::new(
                        &format!("{path}/data"),
                        "data must be an array of bytes or {\"param\": \"name\"}",
                        "Example: [0x00, 0x08].",
                    ));
                }
                self.check_bus(*bus, &format!("{path}/bus"), ctx, issues);
            }
            Exec::Rgb { pin, color, .. } => {
                self.check_pin(pin, &format!("{path}/pin"), issues);
                if let Text::Lit(c) = color {
                    if crate::json::parse_color(&json!(c)).is_none() {
                        issues.push(Issue::new(
                            &format!("{path}/color"),
                            format!("`{c}` is not a colour"),
                            "Use #rrggbb, e.g. #20ff80.",
                        ));
                    }
                }
            }
            Exec::Delay { ms } => {
                if let Num::Lit(m) = ms {
                    if *m < 0.0 || *m as u32 > MAX_TOTAL_DELAY_MS {
                        issues.push(Issue::new(
                            &format!("{path}/ms"),
                            format!("ms must be 0..={MAX_TOTAL_DELAY_MS}"),
                            "A blocking tool call holds the HTTP connection.",
                        ));
                    }
                }
            }
            Exec::Driver { driver, action } => {
                if !ctx.drivers.iter().any(|d| d == driver) {
                    issues.push(Issue::new(
                        &format!("{path}/driver"),
                        format!("unknown driver `{driver}`"),
                        format!("Known drivers: {}", ctx.drivers.join(", ")),
                    ));
                }
                if action.trim().is_empty() {
                    issues.push(Issue::new(
                        &format!("{path}/action"),
                        "action is empty",
                        "Use the action name from the driver catalogue, e.g. `read`.",
                    ));
                }
                if let Some(dev) = device {
                    if !ctx.known_device_ids.iter().any(|d| d == dev) {
                        issues.push(Issue::new(path, format!("tool references device `{dev}` which was not detected"), "Call `devices_list` first, or set `devices[].driver` so the device is instantiated."));
                    }
                }
            }
            Exec::Sequence { steps } => {
                if steps.is_empty() {
                    issues.push(Issue::new(
                        &format!("{path}/steps"),
                        "a sequence needs at least one step",
                        "Add steps, or use a single primitive exec.",
                    ));
                }
                if steps.len() > MAX_STEPS {
                    issues.push(Issue::new(
                        &format!("{path}/steps"),
                        format!("at most {MAX_STEPS} steps"),
                        "Batching more than that makes one tool call too slow.",
                    ));
                }
                let mut total = 0u32;
                for (i, step) in steps.iter().enumerate() {
                    total += step.ms;
                    self.validate_exec(&step.exec, &format!("{path}/steps/{i}/exec"), ctx, device, issues);
                }
                if total > MAX_TOTAL_DELAY_MS {
                    issues.push(Issue::new(
                        &format!("{path}/steps"),
                        format!("total delay {total} ms exceeds {MAX_TOTAL_DELAY_MS} ms"),
                        "Shorten the delays.",
                    ));
                }
            }
        }
    }

    fn check_pin(&self, pin: &Num, path: &str, issues: &mut Vec<Issue>) {
        if let Num::Lit(p) = pin {
            if *p < 0.0 || *p > 48.0 {
                issues.push(Issue::new(
                    path,
                    format!("pin {p} is outside 0..=48"),
                    "ESP32-S3 has GPIO0..GPIO48; check the strapping pins too.",
                ));
            }
        }
    }

    fn check_addr(&self, addr: &Num, path: &str, issues: &mut Vec<Issue>) {
        if let Num::Lit(a) = addr {
            if *a < 1.0 || *a > 127.0 {
                issues.push(Issue::new(
                    path,
                    format!("address {a} is outside 1..=127"),
                    "I2C addresses are 7-bit: 0x76 = 118.",
                ));
            }
        }
    }

    fn check_bus(&self, bus: Option<u8>, path: &str, ctx: &ValidateCtx, issues: &mut Vec<Issue>) {
        if let Some(b) = bus {
            if !ctx.buses.contains(&b) {
                issues.push(Issue::new(
                    path,
                    format!("I2C bus {b} is not configured"),
                    "Configure it under `bus.i2c` first (a live change re-probes the bus).",
                ));
            }
        }
    }

    /// Compile every enabled tool into a registry entry, with argument limits
    /// enforced at call time.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut out = Vec::new();
        for t in &self.tools {
            if !t.enabled || t.name.trim().is_empty() {
                continue;
            }
            let spec = t.clone();
            let schema = tool_schema(t);
            // A tool that drives a compiled driver needs that driver's capability, so
            // it disappears from tools/list when the device is not detected (and
            // reappears on the next probe without another apply).
            let requires: Vec<&str> = match &t.exec {
                Exec::Driver { driver, .. } => vec![driver.as_str()],
                _ => Vec::new(),
            };
            let mut def = ToolDef::new(&t.name, &t.description)
                .title(if t.title.is_empty() { &t.name } else { &t.title })
                .category("manifest")
                .schema(schema)
                .manifest_tool()
                .requires(&requires);
            if t.exec_is_mutating() {
                def = def.mutating();
            }
            def = def.bind(move |ctx, args| run_tool(&spec, ctx, args));
            out.push(def);
        }
        out
    }

    /// Merge semantics for `manifest_apply` with `mode: "merge"`: tools and
    /// devices are added or replaced by name/id, nothing else is dropped.
    pub fn merged_with(&self, patch: &Manifest) -> Manifest {
        let mut out = self.clone();
        for dev in &patch.devices {
            match out.devices.iter_mut().find(|d| d.id == dev.id) {
                Some(slot) => *slot = dev.clone(),
                None => out.devices.push(dev.clone()),
            }
        }
        for tool in &patch.tools {
            match out.tools.iter_mut().find(|t| t.name == tool.name) {
                Some(slot) => *slot = tool.clone(),
                None => out.tools.push(tool.clone()),
            }
        }
        out.version = MANIFEST_VERSION;
        out
    }

    /// The manifest tool that wraps a driver action, if one exists.
    ///
    /// `device_invoke` must apply that tool's policy before calling the driver:
    /// otherwise the same physical action would get one verdict through its own
    /// tool and another through the generic entry point.
    pub fn driver_tool(&self, device: &str, action: &str) -> Option<&ManifestTool> {
        self.tools.iter().find(|t| {
            t.enabled
                && match &t.exec {
                    Exec::Driver { driver, action: a } => driver == device && a == action,
                    _ => false,
                }
        })
    }

    /// The names of the tools this manifest contributes.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().filter(|t| t.enabled && !t.name.trim().is_empty()).map(|t| t.name.clone()).collect()
    }

    /// Remove tools whose name the wire will not carry, and say which.
    ///
    /// [`Manifest::validate`] refuses such a name at `manifest_apply` time, so a board
    /// configured through this build cannot reach the state below. One configured
    /// before it can: the name sits in NVS, nothing re-validates it on boot, and every
    /// model call fails — including the call that would let the agent repair its own
    /// manifest. Dropping the entries at load is the only way back in, and the caller
    /// logs what went, so it is not silent.
    pub fn drop_tools_with_unusable_names(&mut self) -> Vec<String> {
        let mut dropped = Vec::new();
        self.tools.retain(|t| {
            let ok = is_tool_name(&t.name);
            if !ok {
                dropped.push(t.name.clone());
            }
            ok
        });
        dropped
    }
}

impl Exec {
    fn is_mutating(&self) -> bool {
        !matches!(self, Exec::GpioRead { .. } | Exec::Adc { .. } | Exec::I2cRead { .. } | Exec::Delay { .. })
    }
}

impl ManifestTool {
    pub fn exec_is_mutating(&self) -> bool {
        self.exec.is_mutating()
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_tool_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 48 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn check_value(param: &Param, v: &Value, path: &str) -> Option<Issue> {
    let num = v.as_f64();
    match param.kind.as_str() {
        "integer" | "number" => {
            let Some(n) = num else {
                return Some(Issue::new(path, "default must be numeric", "Give a number, e.g. 0 or 1.5."));
            };
            if let Some(min) = param.min {
                if n < min {
                    return Some(Issue::new(
                        path,
                        format!("default {n} is below min {min}"),
                        "Raise the default or the min.",
                    ));
                }
            }
            if let Some(max) = param.max {
                if n > max {
                    return Some(Issue::new(
                        path,
                        format!("default {n} is above max {max}"),
                        "Lower the default or the max.",
                    ));
                }
            }
        }
        "boolean" => {
            if !v.is_boolean() {
                return Some(Issue::new(path, "default must be true or false", ""));
            }
        }
        "string" if !v.is_string() => {
            return Some(Issue::new(path, "default must be a string", ""));
        }
        _ => {}
    }
    None
}

/// JSON Schema for a manifest tool, built from its parameter list.
fn tool_schema(t: &ManifestTool) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for p in &t.params {
        let mut schema = Map::new();
        schema.insert("type".into(), json!(p.kind));
        if !p.description.is_empty() {
            schema.insert("description".into(), json!(p.description));
        }
        if let Some(min) = p.min {
            schema.insert("minimum".into(), json!(min));
        }
        if let Some(max) = p.max {
            schema.insert("maximum".into(), json!(max));
        }
        if !p.values.is_empty() {
            schema.insert("enum".into(), json!(p.values));
        }
        if !p.unit.is_empty() {
            schema.insert("x-unit".into(), json!(p.unit));
        }
        if let Some(d) = &p.default {
            schema.insert("default".into(), d.clone());
        }
        props.insert(p.name.clone(), Value::Object(schema));
        if p.required {
            required.push(json!(p.name));
        }
    }
    let mut obj = object_schema(&[], &[]);
    obj["properties"] = Value::Object(props);
    obj["required"] = Value::Array(required);
    if t.safety.confirm {
        obj["properties"]["confirm"] =
            json!({ "type": "boolean", "description": "must be true: this action changes hardware state" });
        if let Some(a) = obj["required"].as_array_mut() {
            a.push(json!("confirm"))
        }
    }
    obj
}

/// Validate one call's arguments against the declared params, then the safety
/// limits. Everything here runs on the board before any pin is touched.
fn check_args(t: &ManifestTool, args: &Value) -> Result<()> {
    if t.safety.confirm && !crate::json::opt_bool(args, "confirm").unwrap_or(false) {
        return Err(McpError::forbidden(format!("`{}` changes hardware state: pass confirm=true", t.name)));
    }
    for p in &t.params {
        let raw = args.get(&p.name).filter(|v| !v.is_null());
        let Some(raw) = raw else {
            if p.required {
                return Err(McpError::invalid_params(format!("`{}` is required", p.name)));
            }
            continue;
        };
        match p.kind.as_str() {
            "integer" | "number" => {
                let n = match raw {
                    Value::Number(n) => n.as_f64().unwrap_or(0.0),
                    Value::String(s) => s
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| McpError::invalid_params(format!("`{}` must be a number", p.name)))?,
                    _ => return Err(McpError::invalid_params(format!("`{}` must be a number", p.name))),
                };
                if p.kind == "integer" && n.fract() != 0.0 {
                    return Err(McpError::invalid_params(format!("`{}` must be an integer", p.name)));
                }
                if let Some(min) = p.min {
                    if n < min {
                        return Err(limited(&p.name, n, p.min, p.max, p.unit.as_str()));
                    }
                }
                if let Some(max) = p.max {
                    if n > max {
                        return Err(limited(&p.name, n, p.min, p.max, p.unit.as_str()));
                    }
                }
                if let Some(min) = t.safety.min {
                    if n < min {
                        return Err(safety_refused(&t.name, &p.name, n, t.safety.min, t.safety.max, &p.unit));
                    }
                }
                if let Some(max) = t.safety.max {
                    if n > max {
                        return Err(safety_refused(&t.name, &p.name, n, t.safety.min, t.safety.max, &p.unit));
                    }
                }
            }
            "boolean" => {
                if !raw.is_boolean() && raw.as_i64().is_none() && raw.as_str().is_none() {
                    return Err(McpError::invalid_params(format!("`{}` must be a boolean", p.name)));
                }
            }
            "string" => {
                if !raw.is_string() {
                    return Err(McpError::invalid_params(format!("`{}` must be a string", p.name)));
                }
                if !p.values.is_empty() {
                    let s = raw.as_str().unwrap_or("");
                    if !p.values.iter().any(|v| v == s) {
                        return Err(McpError::invalid_params(format!(
                            "`{}` must be one of {}",
                            p.name,
                            p.values.join(", ")
                        )));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn limited(name: &str, value: f64, min: Option<f64>, max: Option<f64>, unit: &str) -> McpError {
    let range = match (min, max) {
        (Some(a), Some(b)) => format!("{a}..={b}"),
        (Some(a), None) => format!(">= {a}"),
        (None, Some(b)) => format!("<= {b}"),
        _ => "a valid range".into(),
    };
    McpError::invalid_params(format!(
        "`{name}` = {value} is outside {range}{}",
        if unit.is_empty() { String::new() } else { format!(" {unit}") }
    ))
}

fn safety_refused(
    tool: &str,
    param: &str,
    value: f64,
    min: Option<f64>,
    max: Option<f64>,
    unit: &str,
) -> McpError {
    McpError::forbidden(format!(
        "`{tool}` refused: `{param}` = {value} is outside the safety range {}{}",
        match (min, max) {
            (Some(a), Some(b)) => format!("{a}..={b}"),
            (Some(a), None) => format!(">= {a}"),
            (None, Some(b)) => format!("<= {b}"),
            _ => "declared".into(),
        },
        if unit.is_empty() { String::new() } else { format!(" {unit}") }
    ))
    .with_data(json!({ "tool": tool, "param": param, "value": value }))
}

pub(crate) fn run_tool(t: &ManifestTool, ctx: &mut ToolCtx, args: &Value) -> Result<Value> {
    check_args(t, args)?;
    // Rate limit, so a looping model cannot hammer a relay or a display.
    if let Some(limit) = t.safety.max_calls_per_min {
        let key = format!("manifest:{}", t.name);
        let now = crate::app::now_ms();
        let count = ctx.runtime.rate_hit(&key, now, 60_000);
        if count > limit as u64 {
            return Err(McpError::forbidden(format!(
                "`{}` is rate limited to {limit} calls per minute",
                t.name
            )));
        }
    }
    let out = run_exec(&t.exec, ctx, args)?;
    Ok(out)
}

fn run_exec(exec: &Exec, ctx: &mut ToolCtx, args: &Value) -> Result<Value> {
    let steps_note = |n: usize| json!({ "steps_run": n });
    match exec {
        Exec::GpioWrite { pin, level } => {
            let pin = pin.resolve_i32(args)?;
            let high = level.resolve(args)? != 0.0;
            ctx.hw.gpio_config(pin, PinMode::Output)?;
            ctx.hw.gpio_write(pin, high)?;
            Ok(json!({ "pin": pin, "level": if high { 1 } else { 0 } }))
        }
        Exec::GpioRead { pin } => {
            let pin = pin.resolve_i32(args)?;
            ctx.hw.gpio_config(pin, PinMode::InputPullup)?;
            let high = ctx.hw.gpio_read(pin)?;
            Ok(json!({ "pin": pin, "level": if high { 1 } else { 0 } }))
        }
        Exec::Toggle { pin, times, gap_ms } => {
            let pin = pin.resolve_i32(args)?;
            let n = times.resolve_u32(args)?.clamp(1, 20);
            let gap = gap_ms.resolve_u32(args)?.clamp(10, 1000);
            ctx.hw.gpio_config(pin, PinMode::Output)?;
            for _ in 0..n {
                ctx.hw.gpio_write(pin, true)?;
                ctx.hw.delay_us(gap * 1000);
                ctx.hw.gpio_write(pin, false)?;
                ctx.hw.delay_us(gap * 1000);
            }
            Ok(json!({ "pin": pin, "times": n, "gap_ms": gap }))
        }
        Exec::Pwm { pin, freq_hz, duty_pct } => {
            let pin = pin.resolve_i32(args)?;
            let freq = freq_hz.resolve_u32(args)?;
            let duty = duty_pct.resolve(args)?;
            if !(0.0..=100.0).contains(&duty) {
                return Err(McpError::forbidden("duty_pct must be 0..=100"));
            }
            ctx.hw.pwm_set(pin, freq, duty as f32)?;
            Ok(json!({ "pin": pin, "freq_hz": freq, "duty_pct": duty }))
        }
        Exec::Adc { pin } => {
            let pin = pin.resolve_i32(args)?;
            let mv = ctx.hw.adc_mv(pin)?;
            Ok(json!({ "pin": pin, "millivolts": mv }))
        }
        Exec::I2cRead { addr, reg, length, bus } => {
            let addr = addr.resolve_i32(args)? as u8;
            let reg = reg.resolve_i32(args)? as u8;
            let len = length.resolve_u32(args)?.clamp(1, 64) as usize;
            let bus = bus.unwrap_or_else(|| ctx.cfg.bus.i2c.first().map(|b| b.bus).unwrap_or(0));
            let mut buf = vec![0u8; len];
            ctx.hw.i2c_read(bus, addr, Some(reg), &mut buf)?;
            Ok(json!({ "addr": format!("0x{addr:02X}"), "reg": format!("0x{reg:02X}"),
                       "hex": crate::util::hex(&buf), "bytes": buf }))
        }
        Exec::I2cWrite { addr, reg, data, bus } => {
            let addr = addr.resolve_i32(args)? as u8;
            let reg = reg.resolve_i32(args)? as u8;
            let bytes = resolve_bytes(data, args)?;
            let bus = bus.unwrap_or_else(|| ctx.cfg.bus.i2c.first().map(|b| b.bus).unwrap_or(0));
            ctx.hw.i2c_write(bus, addr, Some(reg), &bytes)?;
            Ok(json!({ "addr": format!("0x{addr:02X}"), "bytes_written": bytes.len() }))
        }
        Exec::Rgb { pin, color, count } => {
            let pin = pin.resolve_i32(args)?;
            let c = color.resolve(args)?;
            let rgb = crate::json::parse_color(&json!(c))
                .ok_or_else(|| McpError::invalid_params("colour must be #rrggbb"))?;
            ctx.hw.ring_set(pin, rgb, count.unwrap_or(1))?;
            Ok(json!({ "pin": pin, "color": c }))
        }
        Exec::Delay { ms } => {
            let ms = ms.resolve_u32(args)?.min(MAX_TOTAL_DELAY_MS);
            ctx.hw.delay_us(ms * 1000);
            Ok(json!({ "slept_ms": ms }))
        }
        Exec::Driver { driver, action } => {
            // The device id equals the driver id unless the manifest renamed it.
            let id = driver.clone();
            let value = ctx.devices.call(&id, action, args, &mut *ctx.hw)?;
            Ok(value)
        }
        Exec::Sequence { steps } => {
            let mut results = Vec::with_capacity(steps.len());
            let mut n = 0usize;
            for step in steps {
                results.push(run_exec(&step.exec, ctx, args)?);
                n += 1;
                if step.ms > 0 {
                    ctx.hw.delay_us(step.ms.min(MAX_TOTAL_DELAY_MS) * 1000);
                }
            }
            let mut out = steps_note(n);
            out["results"] = Value::Array(results);
            Ok(out)
        }
    }
}

fn resolve_bytes(data: &Value, args: &Value) -> Result<Vec<u8>> {
    if let Some(param) = data.get("param").and_then(|p| p.as_str()) {
        let v = args.get(param).ok_or_else(|| McpError::invalid_params(format!("`{param}` is required")))?;
        return bytes_from_value(v);
    }
    bytes_from_value(data)
}

fn bytes_from_value(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Array(items) => Ok(items.iter().map(|i| i.as_u64().unwrap_or(0).min(255) as u8).collect()),
        Value::String(s) => {
            // "01 02 ff" or "0102ff"
            let cleaned = s.replace([' ', ',', ':'], "");
            if cleaned.len() % 2 != 0 {
                return Err(McpError::invalid_params("hex strings need an even number of digits"));
            }
            let mut out = Vec::with_capacity(cleaned.len() / 2);
            for i in (0..cleaned.len()).step_by(2) {
                out.push(
                    u8::from_str_radix(&cleaned[i..i + 2], 16)
                        .map_err(|_| McpError::invalid_params("invalid hex digit in data"))?,
                );
            }
            Ok(out)
        }
        Value::Number(n) => Ok(vec![n.as_u64().unwrap_or(0).min(255) as u8]),
        _ => Err(McpError::invalid_params("data must be an array of bytes")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapSet;
    use crate::error::code;

    fn ctx() -> ValidateCtx<'static> {
        ValidateCtx {
            caps: Box::leak(Box::new(CapSet::from_caps(["gpio", "i2c", "led"]))),
            buses: vec![0],
            drivers: vec!["bme280".into(), "ssd1306".into()],
            known_device_ids: vec!["bme280".into()],
        }
    }

    fn sample() -> Manifest {
        Manifest::from_json_str(
            r#"{
              "version": 1,
              "devices": [{"id":"led","kind":"actuator","pins":{"out":48}}],
              "tools": [
                {"name":"led_on","description":"Turn the LED on","enabled":true,
                 "params":[{"name":"brightness","type":"integer","min":0,"max":255,"required":true}],
                 "exec":{"type":"gpio_write","pin":48,"level":1},
                 "safety":{"max_calls_per_min":30}},
                {"name":"climate_read","device":"bme280","enabled":true,"params":[],
                 "exec":{"type":"driver","driver":"bme280","action":"bme280_read"}},
                {"name":"blink","enabled":true,"params":[],"safety":{"confirm":true},
                 "exec":{"type":"sequence","steps":[
                    {"ms":100,"exec":{"type":"gpio_write","pin":48,"level":1}},
                    {"ms":100,"exec":{"type":"gpio_write","pin":48,"level":0}}]}}
              ]
            }"#,
        )
        .expect("sample manifest parses")
    }

    #[test]
    fn driver_tool_is_found_by_device_and_action() {
        let m = Manifest::from_json_str(
            r#"{"tools":[{"name":"c_read","params":[],"exec":{"type":"driver","driver":"bme280","action":"bme280_read"},"safety":{"confirm":true}}]}"#,
        )
        .unwrap();
        assert!(m.tools[0].enabled, "enabled must default to true");
        let t = m.driver_tool("bme280", "bme280_read");
        assert!(t.is_some(), "driver_tool did not match: {:?}", m.tools[0].exec);
    }
    #[test]
    fn a_valid_manifest_has_no_issues() {
        let issues = sample().validate(&ctx());
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    #[test]
    fn bad_pins_addresses_and_names_are_reported_with_paths() {
        let m = Manifest::from_json_str(
            r#"{"tools":[{"name":"Bad Name","exec":{"type":"gpio_write","pin":99,"level":1}}],
                "devices":[{"id":"x","driver":"nope"}]}"#,
        )
        .unwrap();
        let issues = m.validate(&ctx());
        assert!(issues.iter().any(|i| i.path == "/tools/0/name"));
        assert!(issues.iter().any(|i| i.path == "/tools/0/exec/pin"));
        assert!(issues.iter().any(|i| i.path == "/devices/0/driver"));
    }

    #[test]
    fn duplicate_tool_names_are_rejected() {
        let m = Manifest::from_json_str(
            r#"{"tools":[{"name":"a","exec":{"type":"gpio_read","pin":1}},
                          {"name":"a","exec":{"type":"gpio_read","pin":2}}]}"#,
        )
        .unwrap();
        let issues = m.validate(&ctx());
        assert!(issues.iter().any(|i| i.message.contains("duplicate")));
    }

    #[test]
    fn sequence_length_and_delay_are_bounded() {
        let mut steps = Vec::new();
        for _ in 0..12 {
            steps.push(json!({"ms": 800, "exec": {"type": "gpio_write", "pin": 2, "level": 1}}));
        }
        let m = Manifest::from_json_str(
            &json!({"tools":[{"name":"x","exec":{"type":"sequence","steps":steps}}]}).to_string(),
        )
        .unwrap();
        let issues = m.validate(&ctx());
        assert!(issues.iter().any(|i| i.message.contains("at most")));
        assert!(issues.iter().any(|i| i.message.contains("total delay")));
    }

    #[test]
    fn argument_limits_and_safety_are_enforced_before_exec() {
        let t = sample().tools[0].clone();
        assert!(check_args(&t, &json!({})).is_err(), "required param missing");
        let e = check_args(&t, &json!({ "brightness": 300 })).unwrap_err();
        assert_eq!(e.code, code::INVALID_PARAMS);
        assert!(check_args(&t, &json!({ "brightness": 3.5 })).is_err(), "integer params reject fractions");
        assert!(check_args(&t, &json!({ "brightness": 10 })).is_ok());
    }

    #[test]
    fn safety_range_overrides_the_schema_range() {
        let m = Manifest::from_json_str(
            r#"{"tools":[{"name":"t","params":[{"name":"v","type":"number","required":true}],
                          "safety":{"min":0,"max":10},"exec":{"type":"pwm","pin":2,"freq_hz":50,"duty_pct":{"param":"v"}}}]}"#,
        )
        .unwrap();
        let t = m.tools[0].clone();
        let e = check_args(&t, &json!({ "v": 42 })).unwrap_err();
        assert_eq!(e.code, code::FORBIDDEN, "safety limits are separate from schema limits");
        assert!(e.message.contains("safety range"));
    }

    #[test]
    fn confirm_is_mandatory_when_declared() {
        let t = sample().tools[2].clone();
        let e = check_args(&t, &json!({})).unwrap_err();
        assert_eq!(e.code, code::FORBIDDEN);
        assert!(check_args(&t, &json!({ "confirm": true })).is_ok());
    }

    #[test]
    fn merge_adds_and_replaces_but_keeps_the_rest() {
        let base = sample();
        let patch = Manifest::from_json_str(
            r#"{"tools":[{"name":"fan_on","params":[],"exec":{"type":"gpio_write","pin":5,"level":1}}]}"#,
        )
        .unwrap();
        let merged = base.merged_with(&patch);
        assert_eq!(merged.tools.len(), 4);
        assert!(merged.tool_names().contains(&"fan_on".to_string()));
        assert!(merged.tool_names().contains(&"led_on".to_string()));
    }

    #[test]
    fn tool_schema_exposes_ranges_units_and_enums() {
        let m = Manifest::from_json_str(
            r#"{"tools":[{"name":"mode","params":[
                 {"name":"m","type":"string","enum":["fast","slow"],"required":true},
                 {"name":"speed","type":"integer","min":1,"max":10,"unit":"rpm"}],
                 "exec":{"type":"delay","ms":1}}]}"#,
        )
        .unwrap();
        let schema = tool_schema(&m.tools[0]);
        assert_eq!(schema["properties"]["m"]["enum"][0], "fast");
        assert_eq!(schema["properties"]["speed"]["x-unit"], "rpm");
        assert_eq!(schema["required"][0], "m");
    }

    #[test]
    fn i2c_write_accepts_hex_strings_and_byte_arrays() {
        let bytes = resolve_bytes(&json!("01 0a FF"), &json!({})).unwrap();
        assert_eq!(bytes, vec![1, 10, 255]);
        let bytes = resolve_bytes(&json!({"param": "d"}), &json!({ "d": [1, 2, 3] })).unwrap();
        assert_eq!(bytes, vec![1, 2, 3]);
        assert!(resolve_bytes(&json!("abc"), &json!({})).is_err());
    }

    #[test]
    fn params_can_drive_pins() {
        let pin = Num::Param { param: "pin".into() };
        assert_eq!(pin.resolve(&json!({ "pin": 7 })).unwrap(), 7.0);
        assert_eq!(pin.resolve(&json!({ "pin": "7" })).unwrap(), 7.0);
        assert!(pin.resolve(&json!({})).is_err());
    }
}

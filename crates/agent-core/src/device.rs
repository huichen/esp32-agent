//! Devices, and the mapping from a tool name to the device that owns it.

use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{ToolCtx, ToolDef, ToolHandler};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One detected peripheral.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Stable instance id used in `/api/devices` and for routing tool calls.
    pub id: String,
    /// Driver kind, e.g. `bme280`, `ssd1306`, `hcsr04`.
    pub kind: String,
    /// Human-readable name shown in the portal.
    pub name: String,
    /// `i2c`, `gpio`, `onewire`, `pwm`, `adc`, `i2s`, `spi`, `virtual`.
    pub bus: String,
    pub address: Option<u8>,
    pub pins: Vec<i32>,
    /// `probe` when a bus scan confirmed it, `config` when the user declared it.
    pub source: String,
    /// Capability ids this device provides; tools declare the same ids.
    pub caps: Vec<String>,
    /// Free-form details (chip id, geometry, register snapshot, ...).
    pub detail: Value,
    /// `ok` or `degraded`; `note` explains a degraded device.
    pub status: String,
    pub note: String,
    /// Tools this device offers, for the portal's tool list.
    pub tools: Vec<String>,
}

impl DeviceInfo {
    pub fn new(kind: &str, name: &str, bus: &str) -> Self {
        Self {
            id: kind.to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            bus: bus.to_string(),
            address: None,
            pins: Vec::new(),
            source: "probe".into(),
            caps: Vec::new(),
            detail: json!({}),
            status: "ok".into(),
            note: String::new(),
            tools: Vec::new(),
        }
    }

    pub fn with_caps(mut self, caps: &[&str]) -> Self {
        self.caps = caps.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_pins(mut self, pins: &[i32]) -> Self {
        self.pins = pins.to_vec();
        self
    }

    pub fn at(mut self, bus: u8, addr: u8) -> Self {
        self.bus = "i2c".into();
        self.address = Some(addr);
        self.detail["i2c_bus"] = json!(bus);
        self
    }
}

/// A detected device: identifies itself, publishes tools, executes them.
pub trait Device: Send {
    fn info(&self) -> DeviceInfo;

    /// Tools this device contributes. Called after detection only.
    fn tools(&self) -> Vec<ToolDef>;

    /// Execute one of [`Device::tools`]; `tool` is the full tool name.
    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value>;

    /// The value shown for the `mcp://devices/<id>` resource.
    fn describe(&mut self, hw: &mut dyn Hardware) -> Value {
        let _ = hw;
        serde_json::to_value(self.info()).unwrap_or_else(|_| json!({}))
    }
}

/// The devices found at boot, in probe order.
#[derive(Default)]
pub struct Devices {
    items: Vec<Box<dyn Device>>,
}

/// A device that carries a name from the manifest instead of the one its driver picked.
///
/// `drivers::instantiate` builds the hardware object, but the id the manifest asked for
/// (`lamp`) has to stick: two WS2812 outputs both call themselves `led` otherwise, and
/// `device_invoke` — which routes by id — could only ever reach the first of them. Tools
/// are routed by the id `Devices::tool_defs` reads from `info()`, so overriding it here is
/// enough; no driver needs to change.
pub struct NamedDevice {
    inner: Box<dyn Device>,
    info: DeviceInfo,
}

impl NamedDevice {
    pub fn new(inner: Box<dyn Device>, id: &str) -> Self {
        let mut info = inner.info();
        info.id = id.to_string();
        Self { inner, info }
    }
}

impl Device for NamedDevice {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }

    fn call(
        &mut self,
        tool: &str,
        args: &Value,
        hw: &mut dyn crate::hardware::Hardware,
    ) -> crate::error::Result<Value> {
        self.inner.call(tool, args, hw)
    }

    fn describe(&mut self, hw: &mut dyn crate::hardware::Hardware) -> Value {
        self.inner.describe(hw)
    }
}

impl std::fmt::Debug for Devices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Devices").field("count", &self.items.len()).finish()
    }
}

impl Devices {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, d: Box<dyn Device>) {
        self.items.push(d);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn infos(&self) -> Vec<DeviceInfo> {
        let names = self.unique_tool_names();
        self.items
            .iter()
            .enumerate()
            .map(|(idx, d)| {
                let mut info = d.info();
                info.tools = info
                    .tools
                    .iter()
                    .enumerate()
                    .map(|(slot, raw)| names.get(&(idx, slot)).cloned().unwrap_or_else(|| raw.clone()))
                    .collect();
                info
            })
            .collect()
    }

    /// The name each device tool will actually carry.
    ///
    /// Two WS2812 outputs (the devkit's own and a strip on another pin) and two BME280s
    /// both want `led_set` / `bme280_read`. The registry is keyed by name, so without this
    /// the second device silently replaced the first. The first keeps the plain name and
    /// the rest get `_2`, `_3`; `device_invoke` still routes by the device id, and
    /// `devices_list` shows the suffixed names so the two views agree.
    fn unique_tool_names(&self) -> std::collections::BTreeMap<(usize, usize), String> {
        let mut taken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut out = std::collections::BTreeMap::new();
        for (idx, d) in self.items.iter().enumerate() {
            for (slot, def) in d.tools().iter().enumerate() {
                let base = def.spec.name.clone();
                let mut name = base.clone();
                let mut n = 1usize;
                while taken.contains(&name) {
                    n += 1;
                    name = format!("{base}_{n}");
                }
                taken.insert(name.clone());
                out.insert((idx, slot), name);
            }
        }
        out
    }

    /// Capability ids contributed by the devices, in first-seen order.
    pub fn caps(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for info in self.infos() {
            for c in info.caps {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
        out
    }

    pub fn has_cap(&self, cap: &str) -> bool {
        self.items.iter().any(|d| d.info().caps.iter().any(|c| c == cap))
    }

    pub fn find_index(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|d| d.info().id == id)
    }

    pub fn ids(&self) -> Vec<String> {
        self.items.iter().map(|d| d.info().id).collect()
    }

    /// Every device tool, wrapped so the closure routes back to its device by
    /// id. Routing by id rather than by index keeps the registry valid when a
    /// re-probe changes the order.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let names = self.unique_tool_names();
        let mut out: Vec<ToolDef> = Vec::new();
        for (idx, d) in self.items.iter().enumerate() {
            let device_id = d.info().id;
            for (slot, def) in d.tools().into_iter().enumerate() {
                let mut def = def;
                // What the driver answers to, before any suffix: the handler routes with
                // this, while the wire sees the (possibly suffixed) name.
                let tool_name = def.spec.name.clone();
                if let Some(name) = names.get(&(idx, slot)) {
                    def.spec.name = name.clone();
                    def.spec.title = name.clone();
                }
                let id = device_id.clone();
                let handler: ToolHandler = Box::new(move |ctx: &mut ToolCtx, args: &Value| {
                    let ToolCtx { hw, devices, .. } = ctx;
                    let name = tool_name.clone();
                    devices.call(&id, &name, args, &mut **hw)
                });
                def.handler = handler;
                def.spec.device_tool = true;
                #[cfg(debug_assertions)]
                let _ = idx;
                out.push(def);
            }
        }
        out
    }

    pub fn tool_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for d in &self.items {
            for t in d.tools() {
                out.push(t.spec.name);
            }
        }
        out
    }

    pub fn call(&mut self, id: &str, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        let idx = self
            .find_index(id)
            .ok_or_else(|| McpError::not_found(format!("no device `{id}` on this board")))?;
        let d = self.items.get_mut(idx).expect("index from find_index");
        d.call(tool, args, hw)
    }

    pub fn describe(&mut self, id: &str, hw: &mut dyn Hardware) -> Result<Value> {
        let idx = self
            .find_index(id)
            .ok_or_else(|| McpError::not_found(format!("no device `{id}` on this board")))?;
        let d = self.items.get_mut(idx).expect("index from find_index");
        Ok(d.describe(hw))
    }
}

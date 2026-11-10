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
        self.items.iter().map(|d| d.info()).collect()
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
        let mut out: Vec<ToolDef> = Vec::new();
        for (idx, d) in self.items.iter().enumerate() {
            let device_id = d.info().id;
            for def in d.tools() {
                let tool_name = def.spec.name.clone();
                let id = device_id.clone();
                let mut def = def;
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

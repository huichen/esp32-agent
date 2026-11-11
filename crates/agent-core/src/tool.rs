//! Tool definitions, the call context, and the registry.
//!
//! A tool is data plus a closure. It declares the capabilities it needs, so the
//! registry can hide it when the hardware is absent, and the closure receives a
//! [`ToolCtx`] — mutable access to exactly the parts of the app a tool may touch.

use crate::boards::BoardProfile;
use crate::capability::CapSet;
use crate::config::Config;
use crate::device::Devices;
use crate::error::Result;
use crate::hardware::Hardware;
use crate::logbuf::LogBuf;
use crate::manifest::Manifest;
use crate::state::{ProbeReport, Runtime};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Everything a tool handler may use. Built once per call from the app's own
/// fields, so no handler needs to lock anything.
pub struct ToolCtx<'a> {
    pub hw: &'a mut dyn Hardware,
    pub devices: &'a mut Devices,
    pub cfg: &'a mut Config,
    pub runtime: &'a mut Runtime,
    pub log: &'a Arc<LogBuf>,
    /// Capabilities of the board plus every detected device.
    pub caps: &'a CapSet,
    pub profile: &'static BoardProfile,
    /// What the last detection pass saw on the buses.
    pub probe: &'a ProbeReport,
    /// The manifest in force. A tool that changes it sets `runtime.manifest_dirty`;
    /// the app re-validates and rebuilds the registry after the call returns.
    pub manifest: &'a mut Manifest,
}

pub type ToolHandler = Box<dyn for<'a> Fn(&mut ToolCtx<'a>, &Value) -> Result<Value> + Send + Sync>;

#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub title: String,
    pub description: String,
    /// Grouping used by the portal and `tools/list` ordering.
    pub category: String,
    pub input_schema: Value,
    /// Capability ids that must all be present for this tool to be listed.
    pub requires: Vec<String>,
    /// True when the call changes hardware state or the stored config.
    pub mutating: bool,
    /// True when the tool is part of the always-available core set.
    pub core: bool,
    /// True for tools contributed by a device driver (hidden in `generic` mode).
    pub device_tool: bool,
    /// True for tools compiled out of the manifest.
    ///
    /// They are the volatile ones: the model rewrites the manifest while it works, so
    /// they are ordered last in the model's tool list and a change moves only that tail
    /// (see `ToolHost::tool_specs`).
    pub from_manifest: bool,
}

pub struct ToolDef {
    pub spec: ToolSpec,
    pub handler: ToolHandler,
}

impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef").field("name", &self.spec.name).finish()
    }
}

impl ToolDef {
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            spec: ToolSpec {
                name: name.to_string(),
                title: name.to_string(),
                description: description.to_string(),
                category: "misc".into(),
                input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                requires: Vec::new(),
                mutating: false,
                core: false,
                device_tool: false,
                from_manifest: false,
            },
            handler: Box::new(|_, _| Err(crate::error::McpError::internal("tool has no handler bound"))),
        }
    }

    pub fn title(mut self, t: &str) -> Self {
        self.spec.title = t.to_string();
        self
    }

    pub fn category(mut self, c: &str) -> Self {
        self.spec.category = c.to_string();
        self
    }

    /// Hide this tool unless every capability is present.
    pub fn requires(mut self, caps: &[&str]) -> Self {
        self.spec.requires = caps.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn schema(mut self, schema: Value) -> Self {
        self.spec.input_schema = schema;
        self
    }

    pub fn mutating(mut self) -> Self {
        self.spec.mutating = true;
        self
    }

    pub fn core(mut self) -> Self {
        self.spec.core = true;
        self
    }

    /// Marks a tool compiled out of the manifest, which is ordered last on the wire.
    pub fn manifest_tool(mut self) -> Self {
        self.spec.from_manifest = true;
        self
    }

    /// Marks a driver-owned tool, which `tools_mode = "generic"` hides in favour
    /// of `device_invoke`.
    pub fn device_tool(mut self) -> Self {
        self.spec.device_tool = true;
        self
    }

    pub fn bind<F>(mut self, f: F) -> Self
    where
        F: for<'a> Fn(&mut ToolCtx<'a>, &Value) -> Result<Value> + Send + Sync + 'static,
    {
        self.handler = Box::new(f);
        self
    }

    /// The `tools/list` entry for this tool.
    pub fn mcp_json(&self) -> Value {
        json!({
            "name": self.spec.name,
            "title": self.spec.title,
            "description": self.spec.description,
            "inputSchema": self.spec.input_schema,
            "annotations": {
                "readOnlyHint": !self.spec.mutating,
                "category": self.spec.category,
            },
        })
    }

    /// The portal's view: everything plus why it is hidden, if it is.
    pub fn portal_json(&self, visible: bool) -> Value {
        json!({
            "name": self.spec.name,
            "title": self.spec.title,
            "description": self.spec.description,
            "category": self.spec.category,
            "requires": self.spec.requires,
            "mutating": self.spec.mutating,
            "core": self.spec.core,
            "visible": visible,
        })
    }
}

/// Object schema helper: `props!{"pin": integer, ...}` style without a macro.
pub fn object_schema(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut m = Map::new();
    for (k, v) in props {
        m.insert((*k).to_string(), v.clone());
    }
    let required: Vec<Value> = required.iter().map(|s| json!(s)).collect();
    json!({
        "type": "object",
        "properties": Value::Object(m),
        "required": required,
        "additionalProperties": false,
    })
}

pub fn integer(min: i64, max: i64, description: &str) -> Value {
    json!({ "type": "integer", "minimum": min, "maximum": max, "description": description })
}

pub fn number(min: f64, max: f64, description: &str) -> Value {
    json!({ "type": "number", "minimum": min, "maximum": max, "description": description })
}

pub fn boolean(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

pub fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

pub fn enumeration(values: &[&str], description: &str) -> Value {
    json!({ "type": "string", "enum": values, "description": description })
}

/// All tools known to the firmware, in registration order.
#[derive(Default)]
pub struct ToolRegistry {
    order: Vec<String>,
    map: BTreeMap<String, ToolDef>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, def: ToolDef) {
        let name = def.spec.name.clone();
        if self.map.insert(name.clone(), def).is_none() {
            self.order.push(name);
        }
    }

    pub fn register_all(&mut self, defs: Vec<ToolDef>) {
        for d in defs {
            self.register(d);
        }
    }

    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.map.get(name)
    }

    pub fn take(&mut self, name: &str) -> Option<ToolDef> {
        self.map.remove(name)
    }

    pub fn put_back(&mut self, def: ToolDef) {
        self.map.insert(def.spec.name.clone(), def);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Registration-order names.
    pub fn names(&self) -> &[String] {
        &self.order
    }

    pub fn iter(&self) -> impl Iterator<Item = &ToolDef> {
        self.order.iter().filter_map(move |n| self.map.get(n))
    }

    pub fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::McpError;

    #[test]
    fn schema_builder_marks_required_fields() {
        let s = object_schema(&[("pin", integer(0, 48, "GPIO number"))], &["pin"]);
        assert_eq!(s["required"][0], "pin");
        assert_eq!(s["properties"]["pin"]["minimum"], 0);
    }

    #[test]
    fn registry_keeps_order_and_replaces_duplicates() {
        let mut r = ToolRegistry::new();
        r.register(ToolDef::new("a", "first"));
        r.register(ToolDef::new("b", "second"));
        r.register(ToolDef::new("a", "replaced"));
        assert_eq!(r.names(), &["a".to_string(), "b".to_string()]);
        assert_eq!(r.len(), 2);
        assert_eq!(r.get("a").unwrap().spec.description, "replaced");
    }

    #[test]
    fn unbound_handler_reports_internal_error() {
        let d = ToolDef::new("x", "y");
        assert!(matches!(d.handler, _));
        let _ = McpError::internal("ok");
    }
}

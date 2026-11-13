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
    /// Names the board itself provides (built-in tools and driver tools). A manifest
    /// tool may not use one: `manifest_apply` would otherwise be able to replace
    /// itself, and the board would lose the tool it repairs its manifest with.
    pub taken: &'a [String],
    /// The schedule itself. A tool that changes it sets `runtime.sched_dirty`, and the
    /// app writes NVS after the call returns — the same path `POST /api/sched` uses.
    pub tasks: &'a mut Vec<crate::sched::ScheduledTask>,
    pub rules: &'a mut Vec<crate::sched::SensorRule>,
    /// Public action names and the families that own them, including suffixed instances.
    pub families: &'a BTreeMap<String, String>,
}

impl ToolCtx<'_> {
    pub fn ensure_enabled(&self, name: &str) -> Result<()> {
        ensure_tool_enabled_in(self.cfg, self.families, name)
    }
}

/// Enforce a public action's switch and the switch of the family containing it.
pub fn ensure_tool_enabled_in(cfg: &Config, families: &BTreeMap<String, String>, name: &str) -> Result<()> {
    ensure_tool_enabled(cfg, name)?;
    if let Some(family) = families.get(name).and_then(|reached| reached.split_whitespace().next()) {
        ensure_tool_enabled(cfg, family)?;
    }
    Ok(())
}

/// The explicit disable that applies to a name, including supported legacy aliases.
/// Listing profiles and generic/family folding are not execution permissions.
pub fn disabled_tool<'a>(cfg: &'a Config, name: &str) -> Option<&'a str> {
    let command = name.strip_prefix("shell_").unwrap_or(name);
    let aliases: &[&str] = match command {
        "read" | "fs_read" => &["read", "fs_read"],
        "write" | "fs_write" => &["write", "fs_write"],
        "edit" | "fs_edit" => &["edit", "fs_edit"],
        "nslookup" | "net_dns" => &["nslookup", "net_dns"],
        "nc" | "net_tcp" => &["nc", "net_tcp"],
        "net_get" => &["net_get", "curl"],
        "net_post" => &["net_post", "curl"],
        _ => &[],
    };
    cfg.mcp
        .disabled_tools
        .iter()
        .find(|disabled| {
            disabled.as_str() == name || disabled.as_str() == command || aliases.contains(&disabled.as_str())
        })
        .map(String::as_str)
}

pub fn ensure_tool_enabled(cfg: &Config, name: &str) -> Result<()> {
    match disabled_tool(cfg, name) {
        Some(disabled) => Err(crate::error::McpError::forbidden(format!(
            "tool `{name}` is disabled by `{disabled}` in `mcp.disabled_tools`"
        ))),
        None => Ok(()),
    }
}

pub type ToolHandler = Box<dyn for<'a> Fn(&mut ToolCtx<'a>, &Value) -> Result<Value> + Send + Sync>;

#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub title: String,
    /// Prose plus the schema-derived argument suffix, shared by every wire dialect.
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

impl ToolSpec {
    /// Help already has a separate arguments column; keep its prose free of the
    /// generated suffix. Schema reassignment uses this same source to replace the hint.
    pub(crate) fn prose_description(&self) -> &str {
        let arguments = argument_names(&self.input_schema);
        if arguments.is_empty() {
            &self.description
        } else {
            let suffix = format!(" · args: {arguments}");
            self.description.strip_suffix(&suffix).unwrap_or(&self.description)
        }
    }
}

/// Argument names only: required fields first, then optional ones, each in name order.
///
/// Sort explicitly rather than relying on JSON map order: descriptions are part of the
/// provider's cached tool-array prefix. No properties means no invented argument list.
pub(crate) fn argument_names(schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required = schema.get("required").and_then(Value::as_array);
    let is_required =
        |name: &str| required.is_some_and(|fields| fields.iter().any(|field| field.as_str() == Some(name)));
    let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
    names.sort_unstable_by_key(|name| (!is_required(name), *name));
    names
        .into_iter()
        .map(|name| if is_required(name) { format!("{name} (required)") } else { format!("{name}?") })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A compact description, without duplicating types, defaults or property prose.
pub(crate) fn description_with_arguments(description: &str, schema: &Value) -> String {
    let arguments = argument_names(schema);
    if arguments.is_empty() {
        description.to_string()
    } else {
        format!("{description} · args: {arguments}")
    }
}

/// App-owned tools use JSON declarations rather than registry handlers. Apply the
/// same description formatter before MCP, the portal or any provider lowers that array.
pub(crate) fn with_argument_descriptions(mut tools: Value) -> Value {
    if let Some(entries) = tools.as_array_mut() {
        for entry in entries {
            let function = &mut entry["function"];
            if let Some(description) = function["description"].as_str() {
                function["description"] =
                    json!(description_with_arguments(description, &function["parameters"]));
            }
        }
    }
    tools
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
        // Every registry/driver/manifest declaration passes here. Store one canonical
        // description so MCP, the portal and all model protocols read the same bytes.
        // Reassigning a schema replaces its old suffix rather than accumulating hints.
        self.spec.description = description_with_arguments(self.spec.prose_description(), &schema);
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
            // The declaration as the model sees it: without this the portal can say a
            // tool exists but not what it takes, which is the first question anyone asks.
            "input_schema": self.spec.input_schema,
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
    use crate::agent::ToolHost;
    use crate::app::{App, FwMeta};
    use crate::error::McpError;

    fn test_board() -> App {
        App::new(
            Box::new(crate::virtual_board::VirtualBoard::new()),
            Box::new(crate::store::MemoryStore::new()),
            LogBuf::new(64),
            FwMeta::default(),
        )
    }

    fn assert_argument_description(name: &str, description: &str, schema: &Value) {
        let Some(properties) = schema["properties"].as_object().filter(|props| !props.is_empty()) else {
            assert!(!description.contains(" · args: "), "{name} invented arguments: {description}");
            return;
        };
        let (_, arguments) = description
            .rsplit_once(" · args: ")
            .unwrap_or_else(|| panic!("{name} has no argument suffix: {description}"));
        let listed: Vec<&str> = arguments.split(", ").collect();
        for field in properties.keys() {
            let required = schema["required"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|value| value.as_str() == Some(field)));
            let expected = if required { format!("{field} (required)") } else { format!("{field}?") };
            assert!(listed.contains(&expected.as_str()), "{name} is missing `{field}`: {description}");
        }
        assert_eq!(listed.len(), properties.len(), "{name} invented or duplicated a field: {description}");
        assert!(arguments.len() <= 512, "{name} has an oversized argument suffix: {arguments}");
    }

    #[test]
    fn every_board_tool_description_names_its_schema_arguments() {
        let mut app = test_board();
        app.call_tool(
            "manifest_apply",
            &json!({"manifest": {"tools": [{
                "name": "bench_wait",
                "description": "Wait for a declared duration",
                "params": [
                    {"name": "ms", "type": "integer", "required": true, "min": 1, "max": 100},
                    {"name": "label", "type": "string"}
                ],
                "exec": {"type": "delay", "ms": {"param": "ms"}},
                "safety": {"confirm": true}
            }]}}),
        )
        .expect("manifest tool is compiled live");
        app.caps.insert("camera"); // Include the app-owned image tool without a real camera.
        for name in ["led_set", "oled_text", "bench_wait"] {
            assert!(app.tools.get(name).is_some(), "{name} is absent from the board registry");
        }
        for def in app.tools.iter() {
            assert_argument_description(&def.spec.name, &def.spec.description, &def.spec.input_schema);
            assert_eq!(def.mcp_json()["description"], def.portal_json(true)["description"]);
        }
        for mode in ["auto", "generic", "all"] {
            app.cfg.mcp.tools_mode = mode.into();
            let model = ToolHost::tool_specs(&app);
            let mcp = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
            let portal: Value = serde_json::from_slice(
                &app.route(&crate::http::Request {
                    method: "GET".into(),
                    path: "/api/tools".into(),
                    query: Default::default(),
                    headers: Default::default(),
                    body: Vec::new(),
                    peer: "test".into(),
                    scheme: "http",
                })
                .body,
            )
            .expect("portal JSON");
            for entry in model.as_array().expect("model tools") {
                let function = &entry["function"];
                let name = function["name"].as_str().expect("tool name");
                let description = function["description"].as_str().expect("description");
                assert_argument_description(name, description, &function["parameters"]);
                let mcp_entry =
                    mcp["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap();
                let portal_entry =
                    portal["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap();
                assert_eq!(function["description"], mcp_entry["description"], "{name}: MCP drifted");
                assert_eq!(function["description"], portal_entry["description"], "{name}: portal drifted");
            }
        }
        // Legacy file/network tools are reachable inside scripts even when not advertised.
        for tools in [crate::tools::fs::specs(), crate::tools::net::specs()] {
            for entry in tools.as_array().unwrap() {
                let function = &entry["function"];
                assert_argument_description(
                    function["name"].as_str().unwrap(),
                    function["description"].as_str().unwrap(),
                    &function["parameters"],
                );
            }
        }
    }

    #[test]
    fn a_schema_without_properties_invents_no_argument_suffix() {
        for schema in [json!({"type": "object"}), json!({"type": "object", "properties": {}})] {
            let def = ToolDef::new("status", "Read status").schema(schema.clone());
            assert_eq!(def.spec.description, "Read status");
            assert_eq!(def.mcp_json()["description"], "Read status");
            assert_eq!(def.portal_json(true)["description"], "Read status");
            let extra = with_argument_descriptions(json!([{"type": "function", "function": {
                "name": "status", "description": "Read status", "parameters": schema
            }}]));
            assert_eq!(extra[0]["function"]["description"], "Read status");
        }
    }

    #[test]
    fn argument_suffixes_are_compact_stable_and_replaced_with_the_schema() {
        let schema = object_schema(
            &[
                ("times", integer(1, 20, "blink count")),
                ("zeta", string("a property note that must not be copied into the suffix")),
                ("ms", integer(20, 1000, "half period")),
                ("color", string(&"#rrggbb ".repeat(1024))),
            ],
            &["zeta", "color"],
        );
        let expected = "Blink · args: color (required), zeta (required), ms?, times?";
        for _ in 0..8 {
            assert_eq!(description_with_arguments("Blink", &schema), expected);
        }
        let mut reordered = schema.clone();
        reordered["required"] = json!(["color", "zeta"]);
        assert_eq!(description_with_arguments("Blink", &reordered), expected);
        let def = ToolDef::new("blink", "Blink").schema(schema.clone()).schema(schema);
        assert_eq!(def.spec.description, expected, "reassigning a schema must not duplicate its suffix");
        let def = def.schema(object_schema(&[("text", string("message"))], &["text"]));
        assert_eq!(def.spec.description, "Blink · args: text (required)");
        assert_eq!(def.schema(json!({"type": "object"})).spec.description, "Blink");

        let mut app = test_board();
        let first = ToolHost::tool_specs(&app).to_string();
        app.invalidate_tool_specs();
        assert_eq!(first, ToolHost::tool_specs(&app).to_string(), "a cache rebuild changed the prefix");
        assert_eq!(
            first,
            ToolHost::tool_specs(&test_board()).to_string(),
            "the same board changed the prefix"
        );
    }

    #[test]
    fn all_model_protocols_preserve_the_canonical_argument_descriptions() {
        #[derive(Default)]
        struct Capture {
            body: Value,
        }
        impl crate::llm::ChatTransport for Capture {
            fn post_json(
                &mut self,
                _url: &str,
                _headers: &[(&str, &str)],
                body: &str,
            ) -> std::result::Result<String, String> {
                self.body = serde_json::from_str(body).map_err(|error| error.to_string())?;
                Ok(json!({
                    "choices": [{"message": {"content": "ok"}}],
                    "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}],
                    "content": [{"type": "text", "text": "ok"}]
                })
                .to_string())
            }
        }
        let tools = ToolHost::tool_specs(&test_board());
        for protocol in ["chat", "responses", "anthropic"] {
            let cfg = crate::llm::LlmConfig {
                base_url: "http://example.test/v1/messages".into(),
                model: "stub".into(),
                protocol: protocol.into(),
                ..crate::llm::LlmConfig::default()
            };
            let mut capture = Capture::default();
            crate::llm::chat(&mut capture, &cfg, &json!([{"role": "user", "content": "test"}]), &tools)
                .expect(protocol);
            let wire_tools = capture.body["tools"].as_array().expect("wire tools");
            assert_eq!(wire_tools.len(), tools.as_array().unwrap().len(), "{protocol} lost tools");
            for (canonical, wire) in tools.as_array().unwrap().iter().zip(wire_tools) {
                let canonical = &canonical["function"];
                let wire = if protocol == "chat" { &wire["function"] } else { wire };
                assert_eq!(wire["name"], canonical["name"], "{protocol} reordered tools");
                assert_eq!(
                    wire["description"], canonical["description"],
                    "{protocol}: {} drifted",
                    canonical["name"]
                );
            }
        }
    }

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

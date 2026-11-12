//! What an external agent may change, and through which endpoint.
//!
//! `agent.json` already lists the routes; this answers the next question a client
//! actually has: "I need to make the board do X — which field, on which call, and what
//! are the bounds?" The table is hand-written but tested against the route list, so a
//! group cannot point at an endpoint that no longer exists.
//!
//! Rules that matter to a caller and are not obvious from a field name:
//!
//! * Everything here is a mutation, so the portal PIN applies when one is configured.
//! * `api_key` and `wifi.password` are write-only: `GET` returns `__set__`, and sending
//!   `__set__` back leaves the stored value alone.
//! * Model settings live on their own endpoint because the model loop reads them per
//!   tick; board configuration is a different document with its own revision.

use serde_json::{json, Value};

/// One settable field.
pub struct Field {
    pub name: &'static str,
    pub kind: &'static str,
    pub default: &'static str,
    pub note: &'static str,
}

/// One group of fields sharing a write endpoint.
pub struct Group {
    pub group: &'static str,
    /// `METHOD /path`, matching an entry in [`crate::sitemap::ROUTES`].
    pub endpoint: &'static str,
    pub summary: &'static str,
    pub fields: &'static [Field],
}

const fn f(name: &'static str, kind: &'static str, default: &'static str, note: &'static str) -> Field {
    Field { name, kind, default, note }
}

const MODEL: &[Field] = &[
    f("base_url", "string", "", "full chat-completions URL, http:// or https://"),
    f("model", "string", "", "the provider's model id"),
    f("api_key", "string", "\"\"", "write-only; GET returns __set__ and __set__ leaves it unchanged"),
    f("max_steps", "integer", "0", "model calls per run; 0 means no limit, capped at 200"),
    f("temperature", "number", "1.0", "0.0 to 2.0"),
    f("timeout_s", "integer", "45", "5 to 180 seconds per model call"),
    f(
        "reasoning",
        "string",
        "\"\"",
        "off | low | medium | high; empty sends nothing and leaves it to the provider (sent as `reasoning_effort`)",
    ),
    f(
        "protocol",
        "string",
        "\"chat\"",
        "chat (OpenAI chat completions) | responses (OpenAI Responses API) | anthropic (Messages API)",
    ),
    f(
        "vision",
        "boolean",
        "true",
        "send attached images as content parts; false names them in the text instead, for a text-only model",
    ),
];

const BOARD: &[Field] = &[
    f("device_name", "string", "esp32-agent", "mDNS/portal name"),
    f("board", "string", "auto", "board profile id, or auto to pick from the SoC"),
    f("auto_peripherals", "boolean", "true", "instantiate the devkit's own LED and button"),
    f("wifi.ssid", "string", "", "join this network"),
    f("wifi.password", "string", "", "write-only, same __set__ rule as api_key"),
    f("wifi.hostname", "string", "esp32-agent", "DHCP hostname"),
    f(
        "wifi.gateway",
        "string",
        "\"\"",
        "default gateway; empty uses the DHCP lease. Set it to a LAN box that tunnels traffic",
    ),
    f("wifi.dns", "string", "\"\"", "primary DNS; empty uses the DHCP lease"),
    f("wifi.dns2", "string", "\"\"", "secondary DNS; empty leaves it alone"),
    f("wifi.auto_reconnect", "boolean", "true", "rejoin after a drop"),
    f("wifi.low_latency", "boolean", "true", "disable modem sleep"),
    f("wifi.ap_fallback", "boolean", "true", "keep the setup AP up beside the station"),
    f("wifi.ap_ssid", "string", "esp32-agent-setup", "setup AP name"),
    f("wifi.ap_after_s", "integer", "30", "start the setup AP after this long without a station"),
    f("wifi.ap_password", "string", "\"\"", "write-only; empty means an open setup AP"),
    f("mcp.enabled", "boolean", "true", "serve the MCP endpoint"),
    f("mcp.port", "integer", "8080", "needs a restart"),
    f("mcp.path", "string", "/mcp", "endpoint path"),
    f("mcp.tools_mode", "string", "auto", "auto | generic | all"),
    f(
        "mcp.profile",
        "string",
        "full",
        "full | minimal — minimal hides nine convenience tools, never the ones an empty board needs",
    ),
    f("mcp.disabled_tools", "string[]", "[]", "hide these tool names"),
    f("mcp.max_sessions", "integer", "4", "concurrent MCP sessions"),
    f("mcp.session_ttl_s", "integer", "900", "idle session lifetime"),
    f("mcp.cors", "boolean", "true", "CORS headers and OPTIONS handling"),
    f("mcp.reprobe_on_list", "boolean", "false", "probe the buses on every tools/list"),
    f("mcp.sse", "boolean", "false", "allow SSE responses on MCP"),
    f("bus.i2c[].sda/scl", "integer", "41/42", "bus pins; needs a restart"),
    f("bus.i2c[].freq_hz", "integer", "400000", "bus clock"),
    f("bus.onewire_pins", "integer[]", "[]", "pins to probe for 1-Wire"),
    f("bus.auto_probe", "boolean", "true", "probe on boot"),
    f("bus.probe_gpio", "boolean", "false", "also probe GPIO for known shapes"),
    f("pins", "object", "{}", "explicit pin overrides, e.g. {\"oled.height\":32}"),
    f("auth.portal_pin", "string", "\"\"", "empty = every mutation is open"),
    f("auth.mcp_token", "string", "\"\"", "empty = MCP needs no bearer token"),
    f("log.level", "string", "info", "trace | debug | info | warn | error"),
    f("log.ring", "integer", "96", "lines kept in RAM"),
    f("ui.lang", "string", "zh", "portal language"),
    f("ui.theme", "string", "dark", "portal theme"),
    f("agent.tick_ms", "integer", "1000", "how often the agent thread and scheduler tick"),
];

const SCHEDULE: &[Field] = &[
    f("tasks[].id", "string", "", "stable id"),
    f("tasks[].prompt", "string", "", "what to ask the agent"),
    f("tasks[].every_s", "integer", "0", "repeat every N seconds"),
    f("tasks[].at_hhmm", "string", "\"\"", "run at a wall-clock time, UTC"),
    f("tasks[].enabled", "boolean", "true", "pause without deleting"),
    f("rules[].device/tool", "string", "", "what to call"),
    f("rules[].path", "string", "", "which field of the result to watch"),
    f("rules[].above/below", "number", "", "threshold that fires"),
    f("rules[].min_interval_s", "integer", "", "rate limit"),
];

const GOAL: &[Field] = &[
    f("goal", "string", "", "the objective; POST starts the loop"),
    f("max_iterations", "integer", "10", "iterations before it stops"),
];

const MEMORY: &[Field] = &[f("keep", "integer", "0", "exchanges to keep; 0 drops the whole conversation")];

const TOOLS: &[Field] = &[
    f("tools/call name+arguments", "object", "", "every detected device, the manifest tools, fs_* and net_*"),
    f("manifest", "object", "", "POST /api/manifest to add devices and tools; live at once"),
    f("files", "string", "", "fs_write / fs_edit / fs_rm on /AGENTS.md and /skills/*"),
];

pub const GROUPS: &[Group] = &[
    Group {
        group: "model",
        endpoint: "POST /api/agent/config",
        summary: "Point the board's own model loop at a provider",
        fields: MODEL,
    },
    Group {
        group: "board",
        endpoint: "POST /api/config",
        summary: "Everything else that persists: network, MCP, buses, pins, auth, UI",
        fields: BOARD,
    },
    Group {
        group: "schedule",
        endpoint: "POST /api/sched",
        summary: "Recurring prompts and sensor-triggered rules; the board runs them itself",
        fields: SCHEDULE,
    },
    Group {
        group: "goal",
        endpoint: "POST /api/goal",
        summary: "A long-running objective the board works on across runs",
        fields: GOAL,
    },
    Group {
        group: "conversation",
        endpoint: "POST /api/agent/forget",
        summary: "Keep the last N exchanges or forget the whole transcript; memories live in files",
        fields: MEMORY,
    },
    Group {
        group: "actions",
        endpoint: "POST /mcp",
        summary: "Doing things rather than configuring them: devices, manifest, files, network",
        fields: TOOLS,
    },
];

/// The `settings` section of the discovery document.
pub fn json() -> Value {
    let groups: Vec<Value> = GROUPS
        .iter()
        .map(|group| {
            let fields: Vec<Value> = group
                .fields
                .iter()
                .map(|field| {
                    json!({
                        "name": field.name,
                        "type": field.kind,
                        "default": field.default,
                        "note": field.note,
                    })
                })
                .collect();
            json!({
                "group": group.group,
                "endpoint": group.endpoint,
                "summary": group.summary,
                "fields": fields,
            })
        })
        .collect();
    json!({
        "note": "Every row is a mutation: the portal PIN applies when one is configured. \
                 Secrets are write-only — GET returns __set__ and sending __set__ back keeps \
                 the stored value.",
        "groups": groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_group_points_at_a_documented_route() {
        for group in GROUPS {
            let found = crate::sitemap::ROUTES
                .iter()
                .any(|route| format!("{} {}", route.method, route.path) == group.endpoint);
            assert!(found, "{} has no matching route", group.endpoint);
            assert!(!group.fields.is_empty(), "{} lists no fields", group.group);
        }
    }

    #[test]
    fn the_model_group_documents_the_step_limit_as_unlimited_by_default() {
        let model = GROUPS.iter().find(|g| g.group == "model").expect("model group");
        let steps = model.fields.iter().find(|f| f.name == "max_steps").expect("max_steps");
        assert_eq!(steps.default, "0");
        assert!(steps.note.contains("no limit"), "{}", steps.note);
    }

    #[test]
    fn secrets_are_marked_write_only() {
        for group in GROUPS {
            for field in group.fields {
                if field.name.ends_with("api_key") || field.name.ends_with("password") {
                    assert!(
                        field.note.contains("write-only"),
                        "{} is a secret but does not say so",
                        field.name
                    );
                }
            }
        }
    }

    #[test]
    fn the_section_serialises_with_its_fields() {
        let value = json();
        let groups = value["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), GROUPS.len());
        assert!(groups[0]["fields"].as_array().unwrap().len() > 3);
    }
}

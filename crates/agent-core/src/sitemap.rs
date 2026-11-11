//! The machine-readable map of this board, for an external agent rather than a person.
//!
//! `GET /agent.json` (and its alias `/sitemap.json`) answers with every route, what it
//! takes, what it returns, the MCP surface beside it, and the conventions that apply to
//! all of them. An agent that has never seen this board should be able to read one
//! document and know how to drive it.
//!
//! The table below is the single source of truth: a test in `app.rs` walks every entry
//! and fails if the router answers `404`, so a route cannot be documented and missing,
//! and cannot be deleted without the map noticing.

use serde_json::{json, Value};

/// Who has to be allowed to call a route.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No credentials, on any configuration.
    Open,
    /// The portal PIN when `auth.portal_pin` is set (`X-Portal-Pin` header or `?pin=`),
    /// otherwise open. Every mutation on this board is in this class.
    Pin,
}

impl Auth {
    fn as_str(self) -> &'static str {
        match self {
            Auth::Open => "none",
            Auth::Pin => "portal pin, when one is configured",
        }
    }
}

/// One documented route.
pub struct Route {
    pub method: &'static str,
    pub path: &'static str,
    /// What it does, in the terms its caller thinks in.
    pub summary: &'static str,
    /// A field list for the body, or "" when there is none.
    pub body: &'static str,
    pub returns: &'static str,
    pub auth: Auth,
    /// Grouping for a client that wants to show the surface by area.
    pub tag: &'static str,
}

const fn r(
    method: &'static str,
    path: &'static str,
    tag: &'static str,
    auth: Auth,
    summary: &'static str,
    body: &'static str,
    returns: &'static str,
) -> Route {
    Route { method, path, summary, body, returns, auth, tag }
}

/// Every route this firmware serves, in the order a client should read them.
pub const ROUTES: &[Route] = &[
    // --- this document -------------------------------------------------------------
    r(
        "GET",
        "/agent.json",
        "discovery",
        Auth::Open,
        "This document: every route, the MCP surface and the conventions. Read it first",
        "",
        "the map you are reading",
    ),
    r("GET", "/sitemap.json", "discovery", Auth::Open, "Alias of /agent.json", "", "the map you are reading"),
    // --- health and identity -------------------------------------------------------
    r(
        "GET",
        "/healthz",
        "status",
        Auth::Open,
        "Cheapest liveness probe: no device work, no locks held for long",
        "",
        "{ok, uptime_s, devices}",
    ),
    r(
        "GET",
        "/api/status",
        "status",
        Auth::Open,
        "Board, memory, WiFi, MCP counters, device count, goal state",
        "",
        "a status object",
    ),
    r(
        "GET",
        "/api/devices",
        "devices",
        Auth::Open,
        "Every device the board has, plus addresses nothing claimed",
        "",
        "{devices, unidentified, buses}",
    ),
    r(
        "GET",
        "/api/tools",
        "tools",
        Auth::Open,
        "Tool table with each tool's visibility and why anything is hidden",
        "",
        "{tools, visible, total, mode}",
    ),
    r(
        "GET",
        "/api/catalog",
        "devices",
        Auth::Open,
        "Drivers this firmware knows, and which are implemented",
        "",
        "{drivers}",
    ),
    // --- configuration -------------------------------------------------------------
    r(
        "GET",
        "/api/config",
        "config",
        Auth::Open,
        "Current configuration; secrets appear as __set__, never in the clear",
        "",
        "the config object",
    ),
    r(
        "POST",
        "/api/config",
        "config",
        Auth::Pin,
        "Partial update; only the fields present change",
        "any subset of the config shape",
        "the config after the patch",
    ),
    r(
        "GET",
        "/api/manifest",
        "manifest",
        Auth::Open,
        "The live manifest and its revision",
        "",
        "{manifest, revision}",
    ),
    r(
        "POST",
        "/api/manifest",
        "manifest",
        Auth::Pin,
        "Validate and apply a manifest; live immediately, no reboot",
        "{manifest, mode?: replace|merge, dry_run?: bool}",
        "the apply report",
    ),
    r(
        "GET",
        "/api/manifest/schema",
        "manifest",
        Auth::Open,
        "The reference an author needs: document shape and the exec primitives",
        "",
        "{manifest_schema, primitives}",
    ),
    // --- hardware ------------------------------------------------------------------
    r(
        "POST",
        "/api/devices/probe",
        "devices",
        Auth::Pin,
        "Probe the buses again and rebuild the device list",
        "",
        "the new device table",
    ),
    r("GET", "/api/wifi/scan", "wifi", Auth::Open, "Scan for networks", "", "{networks}"),
    r(
        "POST",
        "/api/wifi/connect",
        "wifi",
        Auth::Pin,
        "Save credentials and join",
        "{ssid, password}",
        "{ok}",
    ),
    r("POST", "/api/wifi/forget", "wifi", Auth::Pin, "Clear the stored credentials", "", "{ok}"),
    // --- the agent -----------------------------------------------------------------
    r(
        "POST",
        "/api/agent/message",
        "agent",
        Auth::Pin,
        "Queue a user message for the on-board agent; the run happens on its own thread",
        "{text}",
        "202 and the queued count",
    ),
    r(
        "GET",
        "/api/agent/history",
        "agent",
        Auth::Open,
        "Transcript entries, optionally only those after a sequence number",
        "?after=<n>",
        "{messages, usage}",
    ),
    r("POST", "/api/agent/reset", "agent", Auth::Pin, "Clear the conversation", "", "{ok}"),
    r(
        "POST",
        "/api/agent/forget",
        "agent",
        Auth::Pin,
        "Keep the last `keep` question-and-answer exchanges and drop the rest, or drop everything with 0; the compressed index goes with it",
        "{keep?: integer}",
        "{ok, removed, kept}",
    ),
    r(
        "GET",
        "/api/agent/config",
        "agent",
        Auth::Open,
        "Model connection and run statistics; the key is never returned",
        "",
        "{base_url, model, api_key_set, tick_ms, usage}",
    ),
    r(
        "POST",
        "/api/agent/config",
        "agent",
        Auth::Pin,
        "Point the agent at a model endpoint",
        "{base_url?, model?, api_key?, tick_ms?}",
        "the new settings",
    ),
    r(
        "GET",
        "/api/agent/events",
        "agent",
        Auth::Open,
        "The run journal (polling), optionally after a sequence number",
        "?after=<seq>",
        "{events}",
    ),
    r(
        "GET",
        "/api/agent/stream",
        "agent",
        Auth::Open,
        "The same journal as server-sent events: replay, then follow",
        "?after=<seq>",
        "text/event-stream; 503 and Retry-After: 5 when two are open",
    ),
    r(
        "GET",
        "/api/agent/prompt",
        "agent",
        Auth::Open,
        "The system prompt as the model receives it: memory index and the compressed view of earlier turns included",
        "",
        "{system, bytes, archive_lines, history}",
    ),
    // --- scheduling and goals ------------------------------------------------------
    r("GET", "/api/sched", "sched", Auth::Open, "Scheduled tasks and sensor rules", "", "{tasks, rules}"),
    r(
        "POST",
        "/api/sched",
        "sched",
        Auth::Pin,
        "Replace the scheduled tasks and sensor rules",
        "{tasks, rules}",
        "the stored schedule",
    ),
    r(
        "GET",
        "/api/goal",
        "goal",
        Auth::Open,
        "The self-directed goal loop: status, iteration, notes",
        "",
        "{goal}",
    ),
    r(
        "POST",
        "/api/goal",
        "goal",
        Auth::Pin,
        "Start a goal the board works on across runs",
        "{goal, max_iterations?}",
        "{ok}",
    ),
    r("POST", "/api/goal/stop", "goal", Auth::Pin, "Stop the goal loop", "", "{ok}"),
    r("DELETE", "/api/goal", "goal", Auth::Pin, "Stop and clear the goal", "", "{ok}"),
    // --- diagnostics and recovery --------------------------------------------------
    r(
        "GET",
        "/api/logs",
        "logs",
        Auth::Open,
        "The ring log, newest last, bounded by log.ring",
        "?n=<count>&since=<seq>",
        "{lines}",
    ),
    r("GET", "/api/sessions", "mcp", Auth::Open, "Open MCP sessions", "", "{sessions}"),
    r(
        "GET",
        "/api/clients",
        "mcp",
        Auth::Open,
        "Ready-to-paste client configuration for this board",
        "",
        "{clients}",
    ),
    r(
        "POST",
        "/api/auth",
        "config",
        Auth::Open,
        "Check a portal PIN without changing anything",
        "{pin}",
        "{ok}",
    ),
    r("POST", "/api/reboot", "system", Auth::Pin, "Reboot the board", "", "{ok}"),
    r(
        "POST",
        "/api/factory-reset",
        "system",
        Auth::Pin,
        "Clear configuration and manifest, then reboot",
        "",
        "{ok}",
    ),
    r("POST", "/mcp", "mcp", Auth::Open,
      "The MCP transport itself: initialize, tools/list, tools/call, resources/*. The path is `mcp.path`, so read it from the dynamic section before you connect",
      "a JSON-RPC 2.0 message", "a JSON-RPC response, or text/event-stream when mcp.sse is on"),
    // --- the console ---------------------------------------------------------------
    r(
        "GET",
        "/",
        "web",
        Auth::Open,
        "The human portal: devices, tools, logs, configuration, agent panel; it loads the two assets below, without which the page is blank",
        "",
        "text/html",
    ),
    r(
        "GET",
        "/assets/app.js",
        "web",
        Auth::Open,
        "The portal's script; a load check that stops at the HTML misses a failure here",
        "",
        "application/javascript",
    ),
    r(
        "GET",
        "/assets/style.css",
        "web",
        Auth::Open,
        "The portal's stylesheet",
        "",
        "text/css",
    ),
    r(
        "GET",
        "/selftest/chat/completions",
        "agent",
        Auth::Open,
        "A built-in OpenAI-compatible stub, so the loop can be exercised without a model",
        "an OpenAI chat request",
        "a canned completion",
    ),
];

/// The MCP transport, described the way a client needs it.
pub fn mcp_json(path: &str, tools: usize, auth_required: bool) -> Value {
    json!({
        "path": path,
        "transport": "streamable-http, POST only",
        "protocol_versions": ["2025-06-18", "2025-03-26"],
        "auth": if auth_required {
            "Authorization: Bearer <mcp.token> is required"
        } else {
            "open; set mcp.token to require a bearer token"
        },
        "session": {
            "header": "Mcp-Session-Id",
            "how": "take it from the initialize response and send it on every later call",
            "ttl_s": "mcp.session_ttl_s",
            "max": "mcp.max_sessions",
        },
        "methods": [
            "initialize",
            "notifications/initialized",
            "ping",
            "tools/list",
            "tools/call",
            "resources/list",
            "resources/read",
            "resources/templates/list",
        ],
        "tools": tools,
        "resources": [
            "mcp://capabilities", "mcp://devices", "mcp://manifest",
            "mcp://config", "mcp://logs", "mcp://catalog", "mcp://devices/<id>"
        ],
        "example": {
            "initialize": {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                           "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                                      "clientInfo": {"name": "your-agent", "version": "0"}}},
            "call": {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                     "params": {"name": "board_status", "arguments": {}}},
        },
        "not_mcp": "the fs_* and net_* tools are on this endpoint too; the on-board model sees them as well",
    })
}

/// Build the document. `dynamic` carries what only the running board knows.
pub fn document(dynamic: Value) -> Value {
    let routed: Vec<Value> = ROUTES
        .iter()
        .map(|route| {
            json!({
                "method": route.method,
                "path": route.path,
                "tag": route.tag,
                "auth": route.auth.as_str(),
                "summary": route.summary,
                "body": if route.body.is_empty() { Value::Null } else { json!(route.body) },
                "returns": route.returns,
            })
        })
        .collect();

    json!({
        "name": "esp32-agent",
        "what": "An ESP32 board that exposes its hardware, its files and its own configuration over HTTP and MCP, and runs a model loop on top of them.",
        "read_this_first": [
            "GET /api/status for the state, GET /api/tools for what you can call",
            "Every tool call is POST /mcp with tools/call; there is no REST tool endpoint",
            "Mutations need the portal PIN when one is configured, so ask the operator before you try one",
            "A manifest edit is live immediately: there is no apply-and-reboot step",
        ],
        "conventions": {
            "content_type": "application/json; requests without a body still need Content-Length: 0 or a JSON body",
            "errors": {"shape": {"error": "<human sentence>"}, "codes": "HTTP status carries the class; 4xx is your request, 5xx is the board"},
            "ids": "JSON-RPC ids in the MCP surface; sequence numbers in the polling endpoints",
            "encoding": "UTF-8 everywhere; binary payloads are base64 in a string field",
            "limits": {
                "tool_result_bytes": 2048,
                "reply_bytes": 8192,
                "http_connections": 6,
                "event_streams": 2,
                "manifest_tools": 48,
                "sequence_steps": 8,
                "model_steps": "unlimited by default; POST /api/agent/config max_steps sets a cap (0 = off, 200 max)",
            },
        },
        "endpoints": routed,
        "settings": crate::settings::json(),
        "counts": {"endpoints": ROUTES.len()},
        "dynamic": dynamic,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_described_once_and_says_what_it_does() {
        let mut seen = std::collections::HashSet::new();
        for route in ROUTES {
            assert!(route.path.starts_with('/'), "{} is not a path", route.path);
            assert!(!route.summary.trim().is_empty(), "{} has no summary", route.path);
            assert!(
                ["GET", "POST", "PUT", "DELETE", "HEAD"].contains(&route.method),
                "{} {} uses an unexpected method",
                route.method,
                route.path
            );
            assert!(
                seen.insert((route.method, route.path)),
                "{} {} is documented twice",
                route.method,
                route.path
            );
        }
        assert!(ROUTES.len() > 25, "the surface is larger than this");
    }

    #[test]
    fn the_map_points_at_itself() {
        assert!(ROUTES.iter().any(|r| r.path == "/agent.json" && r.method == "GET"));
    }

    #[test]
    fn the_document_carries_the_routes_it_lists() {
        let doc = document(json!({"base_url": "http://board"}));
        let listed = doc["endpoints"].as_array().expect("endpoints");
        assert_eq!(listed.len(), ROUTES.len());
        assert_eq!(doc["counts"]["endpoints"], ROUTES.len());
        assert_eq!(doc["dynamic"]["base_url"], "http://board");
    }
}

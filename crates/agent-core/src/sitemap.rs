//! The machine-readable map of this board, for an external agent rather than a person.
//!
//! `GET /agent.json` answers with the routes an external agent should call, the portal's
//! own routes listed apart with the tool that replaces each, what each takes and returns,
//! the MCP surface beside them, and the conventions that apply to all of them. An agent
//! that has never seen this board should be able to read one document and know how to
//! drive it — and which of the two doors is its own.
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
    /// A portal credential when `auth.portal_pin` is set, otherwise open.
    /// This also protects reads of private conversation/configuration data.
    Pin,
    /// Dedicated MCP bearer token, or the portal credential when none is configured.
    Mcp,
}

impl Auth {
    fn as_str(self) -> &'static str {
        match self {
            Auth::Open => "none",
            Auth::Pin => "portal credential, when auth.portal_pin is configured",
            Auth::Mcp => "auth.mcp_token bearer when configured; otherwise the portal credential",
        }
    }
}

/// Which of the two surfaces a route belongs to.
///
/// The board is reachable two ways: MCP for a model, and the portal's REST routes for the
/// browser. Where a REST route only repeats a tool, saying so in the map is the difference
/// between a client that picks the supported path and one that re-implements the portal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// An external agent may call it directly: no MCP tool does the same job.
    Model,
    /// The portal's own backing route. It repeats an MCP tool, so it is listed apart from
    /// `endpoints` and names that tool: a model reading this document should call the tool.
    Portal,
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
    pub surface: Surface,
    /// For a portal route, the MCP tool that does the same job; "" for a model route.
    pub mcp: &'static str,
}

/// A route an external agent calls directly.
const fn r(
    method: &'static str,
    path: &'static str,
    tag: &'static str,
    auth: Auth,
    summary: &'static str,
    body: &'static str,
    returns: &'static str,
) -> Route {
    Route { method, path, summary, body, returns, auth, tag, surface: Surface::Model, mcp: "" }
}

/// A route that exists for the portal and repeats the named tool.
///
/// Eight arguments because the table has eight columns; collapsing them into a struct would
/// only move the same list one line down and hide the extra one that makes a route
/// portal-only.
#[allow(clippy::too_many_arguments)]
const fn p(
    method: &'static str,
    path: &'static str,
    tag: &'static str,
    auth: Auth,
    mcp: &'static str,
    summary: &'static str,
    body: &'static str,
    returns: &'static str,
) -> Route {
    Route { method, path, summary, body, returns, auth, tag, surface: Surface::Portal, mcp }
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
    // --- identity ------------------------------------------------------------------
    r(
        "POST",
        "/api/agent/prompt",
        "agent",
        Auth::Pin,
        "Set, append or clear the standing orders the model is given: {action: set|append|clear, text}",
        "{action, text}",
        "{ok, action, system_prompt, bytes, limit}",
    ),
    r(
        "GET",
        "/api/tls",
        "status",
        Auth::Open,
        "The certificate this board made for itself: fingerprint, https port, or why https is off",
        "",
        "{enabled, port, fingerprint, error}",
    ),
    p(
        "GET",
        "/api/status",
        "status",
        Auth::Open,
        "board status",
        "Board, memory, WiFi, MCP counters, device count, whether the agent thread is up, and each thread's stack headroom; the one reading `board status` does not carry is `threads`",
        "",
        "a status object",
    ),
    p(
        "GET",
        "/api/devices",
        "devices",
        Auth::Open,
        "devices_list",
        "Every device the board has, plus addresses nothing claimed",
        "",
        "{devices, unidentified, buses}",
    ),
    p(
        "GET",
        "/api/tools",
        "tools",
        Auth::Open,
        "tools/list",
        "Tool table with each tool's visibility and why anything is hidden",
        "",
        "{tools, visible, total, mode}",
    ),
    r(
        "POST", "/api/tools", "tools", Auth::Pin,
        "Change tool visibility/disabled switches and listing mode; switches are enforced on execution",
        "{disabled?: [name], mode?: auto|all|generic}",
        "{tools, visible, total, mode}",
    ),
    // --- configuration -------------------------------------------------------------
    p(
        "GET",
        "/api/config",
        "config",
        Auth::Pin,
        "board config_get",
        "Current configuration; secrets appear as __set__, never in the clear",
        "",
        "the config object",
    ),
    p(
        "POST",
        "/api/config",
        "config",
        Auth::Pin,
        "board config_set",
        "Partial update; only the fields present change",
        "any subset of the config shape",
        "the config after the patch",
    ),
    // --- hardware ------------------------------------------------------------------
    p(
        "POST",
        "/api/devices/probe",
        "devices",
        Auth::Pin,
        "devices_probe",
        "Probe the buses again and rebuild the device list",
        "",
        "the new device table",
    ),
    p("GET", "/api/wifi/scan", "wifi", Auth::Open, "wifi_scan", "Scan for networks", "", "{networks}"),
    p(
        "POST",
        "/api/wifi/connect",
        "wifi",
        Auth::Pin,
        "board config_set {wifi: {ssid, password}}",
        "Save credentials and join",
        "{ssid, password}",
        "{ok}",
    ),
    // --- the agent -----------------------------------------------------------------
    r(
        "POST",
        "/api/agent/message",
        "agent",
        Auth::Pin,
        "Queue a user message for the on-board agent; the run happens on its own thread",
        "{text, attachments?: [{name, mime, data_b64}]}",
        "202 and the queued count; attachment admission is atomic with the message",
    ),
    r(
        "GET",
        "/api/agent/history",
        "agent",
        Auth::Pin,
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
        Auth::Pin,
        "The run journal (polling), optionally after a sequence number",
        "?after=<seq>",
        "{events}",
    ),
    r(
        "GET",
        "/api/agent/stream",
        "agent",
        Auth::Pin,
        "The same journal as server-sent events: replay, then follow",
        "?after=<seq>",
        "text/event-stream; 503 and Retry-After: 5 when two are open",
    ),
    r(
        "GET",
        "/api/agent/prompt",
        "agent",
        Auth::Pin,
        "The system prompt as the model receives it: memory index and the compressed view of earlier turns included",
        "",
        "{system, bytes, archive_lines, history}",
    ),
    r(
        "GET", "/api/attachment/<name>", "agent", Auth::Pin,
        "Download an attachment from the last accepted message; URL-encode its name",
        "", "the original decoded bytes and MIME type, or 404",
    ),
    // --- workspace -----------------------------------------------------------------
    p(
        "GET", "/api/files/tree", "files", Auth::Pin,
        "shell ls",
        "List the workspace as a directory tree and flat entries", "",
        "{ok, count, entries, total_bytes, total_max, file_max}",
    ),
    p(
        "GET", "/api/files/read", "files", Auth::Pin,
        "read",
        "Read one UTF-8 workspace file", "?path=<file>",
        "{ok, path, text, size_bytes}, or {ok: false, error}",
    ),
    // --- audio ---------------------------------------------------------------------
    r(
        "POST",
        "/audio/pcm",
        "audio",
        Auth::Pin,
        "Play PCM as it arrives: the body is raw PCM16LE mono at 24000 Hz, streamed straight into the amplifier — a song, not a 43 ms tool argument (docs/audio.md)",
        "raw PCM16LE mono, 24000 Hz, chunked or Content-Length; at most 30 s",
        "{ok, rate_hz, channels, bits, bytes, samples, ms, limit_bytes}",
    ),
    r(
        "GET",
        "/api/mic/pcm",
        "audio",
        Auth::Pin,
        "The microphone as a chunked stream: audio/pcm, 16-bit mono at 16000 Hz, 20 ms per chunk. One reader at a time (a second client gets 503 and Retry-After), 120 s per connection (docs/audio.md)",
        "",
        "raw PCM16LE mono until the client leaves or the budget ends; X-Audio-Format, X-Rate-Hz, X-Channels and X-Bits in the headers",
    ),
    r(
        "POST",
        "/av",
        "audio",
        Auth::Pin,
        "Play audio and draw frames from one streamed body: `A` audio records, `F` bitmap records, `E` end — a soundtrack with its animation, in step by construction (docs/audio.md)",
        "`tag|u16le len|payload` records: `A` = PCM16LE mono 24000 Hz, `F` = 512-byte 128x32 or 1024-byte 128x64 bitmap, `E` = end",
        "{ok, audio_bytes, samples, ms, frames, ended, limit_bytes}",
    ),
    // --- scheduling ----------------------------------------------------------------
    p("GET", "/api/sched", "sched", Auth::Open, "schedule list", "Scheduled tasks and sensor rules", "", "{tasks, rules}"),
    p(
        "POST",
        "/api/sched",
        "sched",
        Auth::Pin,
        "schedule add/remove/enable/disable",
        "Replace the scheduled tasks and sensor rules",
        "{tasks, rules}",
        "the stored schedule",
    ),
    r(
        "POST", "/api/sched/run", "sched", Auth::Pin,
        "Run a scheduled task now without moving its timer", "{id}",
        "the queued prompt or script result, or 404 for an unknown id",
    ),
    // --- diagnostics and recovery --------------------------------------------------
    p(
        "GET",
        "/api/logs",
        "logs",
        Auth::Pin,
        "board logs_tail",
        "The ring log, newest last, bounded by log.ring",
        "?n=<count>&since=<seq>",
        "{lines}",
    ),
    r(
        "POST",
        "/api/auth",
        "config",
        Auth::Open,
        "Check a portal PIN and take a bearer token (12 h) in exchange; with a PIN set, every mutation and the reads that expose the transcript or the config need one",
        "{pin}",
        "{ok, token, expires_in}",
    ),
    r("POST", "/mcp", "mcp", Auth::Mcp,
      "The MCP transport itself: initialize, tools/list, tools/call, resources/*. The path is `mcp.path`, so read it from the dynamic section before you connect",
      "a JSON-RPC 2.0 message", "a JSON-RPC response, or text/event-stream when mcp.sse is on"),
    r("DELETE", "/mcp", "mcp", Auth::Mcp,
      "Close the session in Mcp-Session-Id; the same service and credential gate as POST",
      "", "204 with no body"),
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
        "POST",
        "/selftest/chat/completions",
        "agent",
        Auth::Open,
        "A built-in OpenAI-compatible stub, so the loop can be exercised without a model",
        "an OpenAI chat request",
        "a canned completion",
    ),
    r("GET", "/selftest/ping", "agent", Auth::Open,
      "Lock-free local HTTP target for the scripted network turn", "",
      "{ok, service, purpose}"),
];

/// The MCP transport, described the way a client needs it.
pub fn mcp_json(path: &str, tools: usize, dedicated_token: bool, portal_pin: bool) -> Value {
    json!({
        "path": path,
        "transport": "streamable-http, POST for JSON-RPC; DELETE to close a session; no standalone GET stream",
        "protocol_versions": ["2025-06-18", "2025-03-26"],
        "auth": if dedicated_token {
            "Authorization: Bearer <auth.mcp_token> is required"
        } else if portal_pin {
            "portal credential required: a bearer token from POST /api/auth or the portal PIN"
        } else {
            "open; set auth.mcp_token or auth.portal_pin to require credentials"
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
            "notifications/cancelled",
            "ping",
            "prompts/list",
            "prompts/get",
            "logging/setLevel",
            "completion/complete",
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
                     "params": {"name": "board", "arguments": {"command": "status"}}},
        },
        "shared_tools": "read, write, edit, shell, script and image_read are served here too; the on-board model uses the same tools and disabled switches",
    })
}

/// Build the document. `dynamic` carries what only the running board knows.
pub fn document(dynamic: Value) -> Value {
    let mut endpoints: Vec<Value> = Vec::new();
    let mut portal: Vec<Value> = Vec::new();
    for route in ROUTES {
        let mut row = json!({
            "method": route.method,
            "path": route.path,
            "tag": route.tag,
            "auth": route.auth.as_str(),
            "summary": route.summary,
            "body": if route.body.is_empty() { Value::Null } else { json!(route.body) },
            "returns": route.returns,
        });
        match route.surface {
            Surface::Model => endpoints.push(row),
            Surface::Portal => {
                row["mcp"] = json!(route.mcp);
                portal.push(row);
            }
        }
    }
    let model_count = endpoints.len();
    let portal_count = portal.len();

    json!({
        "name": "esp32-agent",
        "what": "An ESP32 board that exposes its hardware, its files and its own configuration over HTTP and MCP, and runs a model loop on top of them.",
        "read_this_first": [
            "GET /api/status for the state, GET /api/tools for what you can call",
            "Every tool call is POST /mcp with tools/call; there is no REST tool endpoint",
            "Portal writes and private reads need a portal credential when a PIN is set; MCP uses its dedicated bearer token, or falls back to that portal credential",
            "A manifest edit is live immediately: there is no apply-and-reboot step",
        ],
        "conventions": {
            "content_type": "JSON endpoints use application/json; send Content-Length or chunked framing for a body. Media routes accept their documented binary MIME types",
            "errors": {"shape": {"error": "<human sentence>"}, "codes": "HTTP status carries the class; 4xx is your request, 5xx is the board"},
            "ids": "JSON-RPC ids in the MCP surface; sequence numbers in the polling endpoints",
            "encoding": "UTF-8 JSON; attachments/tool binary fields use base64. Downloads and media streams carry raw binary bytes",
            "portal_auth": "X-Portal-Pin, ?pin=, or Authorization: Bearer <PIN|minted token>. Minted ?token= is read-only (GET/HEAD), for EventSource/downloads; a raw PIN is not a URL token",
            "persistence": "A successful write usually means RAM update plus bounded queue admission, not flash acknowledgement. Check /api/status.store.pending, failed and last_failure; pending zero alone is not success",
            "limits": {
                "tool_result_bytes": crate::agent::MAX_TOOL_RESULT_BYTES,
                "reply_bytes": crate::agent::MAX_REPLY_BYTES,
                "user_bytes": crate::agent::MAX_USER_BYTES,
                "history_bytes": crate::agent::MAX_HISTORY_BYTES,
                "history_messages": crate::agent::MAX_HISTORY_MESSAGES,
                "saved_transcript_bytes": crate::agent::MAX_SAVED_BYTES,
                "workspace_file_bytes": crate::app::WORKSPACE_FILE_MAX,
                "workspace_text_bytes": crate::app::WORKSPACE_TOTAL_MAX,
                "workspace_files": crate::app::WORKSPACE_FILES_MAX,
                "workspace_path_bytes": crate::app::WORKSPACE_PATH_MAX,
                "workspace_names_bytes": crate::app::WORKSPACE_NAMES_MAX,
                "attachment_bytes": crate::app::MAX_ATTACHMENT_BYTES,
                "attachments": crate::app::MAX_ATTACHMENTS,
                "http_body_bytes": crate::http::MAX_BODY_BYTES,
                "outbound_http_body_bytes": crate::http_client::MAX_BODY_BYTES,
                "http_connections": crate::http::MAX_CONNECTIONS,
                "event_streams": crate::http::MAX_EVENT_STREAMS,
                "manifest_tools": crate::manifest::MAX_TOOLS,
                "sequence_steps": crate::manifest::MAX_STEPS,
                "sequence_wait_ms": crate::manifest::MAX_TOTAL_DELAY_MS,
                "audio_stream_bytes": crate::app::AUDIO_STREAM_MAX_BYTES,
                "av_stream_bytes": crate::app::AV_MAX_BODY_BYTES,
                "model_steps": "unlimited by default; POST /api/agent/config max_steps sets a cap (0 = off, 200 max)",
            },
        },
        "endpoints": endpoints,
        // The portal's own routes, kept apart from `endpoints` because each one repeats a
        // tool. Naming the tool here is what stops a model from re-implementing the page.
        "portal_endpoints": portal,
        "portal_surface": "The portal at / calls these for the browser; every one has an MCP tool that does the same job, named in its `mcp` field. An agent with an MCP session should call the tool, not the route",
        "settings": crate::settings::json(),
        "counts": {"endpoints": model_count, "portal_endpoints": portal_count},
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
            match route.surface {
                Surface::Model => assert!(
                    route.mcp.is_empty(),
                    "{} {} names a tool but is not a portal route",
                    route.method,
                    route.path
                ),
                Surface::Portal => assert!(
                    !route.mcp.trim().is_empty(),
                    "{} {} is the portal's route and repeats a tool, so it must name it",
                    route.method,
                    route.path
                ),
            }
        }
        assert!(ROUTES.len() > 25, "the surface is larger than this");
        // The portal's half of the surface is what the page calls; `endpoints` must hold
        // only the routes an agent has no tool for, or the split is decoration.
        let portal = ROUTES.iter().filter(|r| r.surface == Surface::Portal).count();
        assert!(portal >= 10, "the portal still drives its own routes");
        assert!(portal < ROUTES.len() / 2, "most of the surface stays callable by an agent");
    }

    #[test]
    fn the_map_points_at_itself() {
        assert!(ROUTES.iter().any(|r| r.path == "/agent.json" && r.method == "GET"));
    }

    #[test]
    fn the_document_carries_the_routes_it_lists() {
        let doc = document(json!({"base_url": "http://board"}));
        let listed = doc["endpoints"].as_array().expect("endpoints");
        let portal = doc["portal_endpoints"].as_array().expect("portal_endpoints");
        assert_eq!(listed.len(), ROUTES.len() - portal.len());
        assert_eq!(doc["counts"]["endpoints"], listed.len());
        assert_eq!(doc["counts"]["portal_endpoints"], portal.len());
        assert_eq!(doc["dynamic"]["base_url"], "http://board");
        assert_eq!(doc["conventions"]["limits"]["tool_result_bytes"], crate::agent::MAX_TOOL_RESULT_BYTES);
        assert_eq!(doc["conventions"]["limits"]["reply_bytes"], crate::agent::MAX_REPLY_BYTES);
        let has = |rows: &Vec<Value>, method: &str, path: &str| {
            rows.iter().any(|route| route["method"] == method && route["path"] == path)
        };
        for (method, path) in [
            ("POST", "/api/tools"),
            ("POST", "/api/sched/run"),
            ("POST", "/selftest/chat/completions"),
            ("GET", "/api/attachment/<name>"),
            ("DELETE", "/mcp"),
        ] {
            assert!(has(listed, method, path), "implemented route omitted from discovery: {method} {path}");
        }
        // A route the portal owns is listed apart and names the tool that replaces it.
        for (method, path, tool) in
            [("GET", "/api/status", "board status"), ("GET", "/api/files/read", "read")]
        {
            assert!(
                !has(listed, method, path),
                "{method} {path} repeats a tool but is listed as an agent endpoint"
            );
            let row = portal
                .iter()
                .find(|route| route["method"] == method && route["path"] == path)
                .unwrap_or_else(|| panic!("portal route missing: {method} {path}"));
            assert_eq!(row["mcp"], tool);
        }
        // The routes an agent has a tool for are gone from REST, not merely unlisted.
        for path in ["/api/manifest", "/api/memory", "/healthz", "/sitemap.json", "/api/script"] {
            assert!(
                !ROUTES.iter().any(|r| r.path == path),
                "{path} has an MCP tool and must not be advertised as REST"
            );
        }
        assert!(mcp_json("/mcp", 1, false, true)["auth"].as_str().unwrap().contains("portal credential"));
        assert!(mcp_json("/mcp", 1, true, true)["auth"].as_str().unwrap().contains("auth.mcp_token"));
    }
}

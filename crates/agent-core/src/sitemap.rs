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
    r(
        "GET",
        "/api/status",
        "status",
        Auth::Open,
        "Board, memory, WiFi, MCP counters, device count, whether the agent thread is up, and each thread's stack headroom",
        "",
        "a status object",
    ),
    r(
        "GET",
        "/api/memory",
        "status",
        Auth::Open,
        "The heap pool by pool — internal, DMA, PSRAM (and IRAM where a chip registers one) — with free and largest free block each",
        "",
        "{available, pools: [{pool, note, total_kb, free_kb, largest_kb, min_free_kb}]}",
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
    r(
        "POST", "/api/tools", "tools", Auth::Pin,
        "Change tool visibility/disabled switches and listing mode; switches are enforced on execution",
        "{disabled?: [name], mode?: auto|all|generic}",
        "{tools, visible, total, mode}",
    ),
    // --- configuration -------------------------------------------------------------
    r(
        "GET",
        "/api/config",
        "config",
        Auth::Pin,
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
    r(
        "GET", "/api/files", "files", Auth::Pin,
        "List one workspace directory", "?path=<directory>",
        "{ok, path, count, entries, total_bytes, total_max, file_max}",
    ),
    r(
        "GET", "/api/files/tree", "files", Auth::Pin,
        "List the workspace as a directory tree and flat entries", "",
        "{ok, count, entries, total_bytes, total_max, file_max}",
    ),
    r(
        "GET", "/api/files/read", "files", Auth::Pin,
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
        "POST",
        "/av",
        "audio",
        Auth::Pin,
        "Play audio and draw frames from one streamed body: `A` audio records, `F` bitmap records, `E` end — a soundtrack with its animation, in step by construction (docs/audio.md)",
        "`tag|u16le len|payload` records: `A` = PCM16LE mono 24000 Hz, `F` = 512-byte 128x32 or 1024-byte 128x64 bitmap, `E` = end",
        "{ok, audio_bytes, samples, ms, frames, ended, limit_bytes}",
    ),
    // --- scheduling ----------------------------------------------------------------
    r("GET", "/api/sched", "sched", Auth::Open, "Scheduled tasks and sensor rules", "", "{tasks, rules}"),
    r(
        "POST",
        "/api/script",
        "script",
        Auth::Pin,
        "Run a script with the caller's own permissions; inline by default, or queue it",
        "{source, async?: bool}",
        "inline: {ok, result, output, steps, ms, error}; async: {id, state, poll} — GET that `poll` path for the result",
    ),
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
        "POST", "/api/sched/run", "sched", Auth::Pin,
        "Run a scheduled task now without moving its timer", "{id}",
        "the queued prompt or script result, or 404 for an unknown id",
    ),
    r(
        "GET", "/api/script/<id>", "script", Auth::Pin,
        "Poll the id returned by an asynchronous script submission", "",
        "{id, state, ok, result, output, steps, ms, error}, or 404",
    ),
    // --- diagnostics and recovery --------------------------------------------------
    r(
        "GET",
        "/api/logs",
        "logs",
        Auth::Pin,
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
        "Check a portal PIN and take a bearer token (12 h) in exchange; with a PIN set, every mutation and the reads that expose the transcript or the config need one",
        "{pin}",
        "{ok, token, expires_in}",
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
            "ping",
            "prompts/list",
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
        assert_eq!(doc["conventions"]["limits"]["tool_result_bytes"], crate::agent::MAX_TOOL_RESULT_BYTES);
        assert_eq!(doc["conventions"]["limits"]["reply_bytes"], crate::agent::MAX_REPLY_BYTES);
        for (method, path) in [
            ("POST", "/api/tools"),
            ("GET", "/api/files"),
            ("GET", "/api/files/tree"),
            ("GET", "/api/files/read"),
            ("POST", "/api/sched/run"),
            ("POST", "/selftest/chat/completions"),
            ("DELETE", "/mcp"),
            ("GET", "/api/script/<id>"),
            ("GET", "/api/attachment/<name>"),
        ] {
            assert!(
                listed.iter().any(|route| route["method"] == method && route["path"] == path),
                "implemented route omitted from discovery: {method} {path}"
            );
        }
        assert!(mcp_json("/mcp", 1, false, true)["auth"].as_str().unwrap().contains("portal credential"));
        assert!(mcp_json("/mcp", 1, true, true)["auth"].as_str().unwrap().contains("auth.mcp_token"));
    }
}

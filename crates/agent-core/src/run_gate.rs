//! Short-lived control-plane lock. Never acquires App, hardware or storage locks.
//! Cancellation can therefore reach a tool that is holding the App mutex.
use crate::http::{Request, Response};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    active: bool,
    cancelled: bool,
    pin: String,
    tokens: Vec<(String, u64)>,
    mcp_path: String,
    cors: bool,
    auth_failures: u64,
    http_requests: u64,
    sessions: usize,
}

#[derive(Clone, Default)]
pub struct RunGate(Arc<Mutex<State>>);

impl RunGate {
    pub fn sync(&self, pin: &str, tokens: &[(String, u64)], mcp_path: &str, cors: bool) {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        s.pin = pin.to_owned();
        s.tokens = tokens.to_vec();
        s.mcp_path = mcp_path.to_owned();
        s.cors = cors;
    }
    pub fn set_active(&self, active: bool) {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !s.active || !active {
            s.cancelled = false;
        }
        s.active = active;
    }
    pub fn active(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).active
    }
    pub fn set_cancelled(&self, cancelled: bool) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).cancelled = cancelled;
    }
    pub fn cancelled(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).cancelled
    }
    pub fn request(&self) -> bool {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !s.active {
            return false;
        }
        s.cancelled = true;
        true
    }
    pub fn take_cancel(&self) -> bool {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut s.cancelled)
    }
    pub fn set_sessions(&self, sessions: usize) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).sessions = sessions;
    }
    pub fn sessions(&self) -> usize {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).sessions
    }
    pub fn note_request(&self) {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        s.http_requests = s.http_requests.saturating_add(1);
    }
    pub fn take_http_requests(&self) -> u64 {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut s.http_requests)
    }
    pub fn take_auth_failures(&self) -> u64 {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut s.auth_failures)
    }
    /// Preserve configurable MCP-path precedence even while App is busy.
    pub fn mcp_get(&self, req: &Request) -> Option<Response> {
        let s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if req.path != s.mcp_path {
            return None;
        }
        let mut r = Response::error_json(
            405,
            "this endpoint uses the POST-only Streamable HTTP transport; open / for the config page",
        );
        if s.cors {
            r = r
                .with_header("Access-Control-Allow-Origin", "*")
                .with_header("Access-Control-Expose-Headers", "Mcp-Session-Id");
        }
        Some(r)
    }
    pub fn cancel_route(&self, req: &Request, now_ms: u64) -> Option<Response> {
        if req.method != "POST" || req.path != "/api/agent/cancel" {
            return None;
        }
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if req.path == s.mcp_path {
            return None;
        }
        s.http_requests = s.http_requests.saturating_add(1);
        let direct = req.header("x-portal-pin").or_else(|| req.query_get("pin"));
        let bearer = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
            .map(str::trim)
            .unwrap_or("");
        let authorized = s.pin.is_empty()
            || direct.is_some_and(|p| !p.is_empty() && p == s.pin)
            || (!bearer.is_empty()
                && (bearer == s.pin || s.tokens.iter().any(|(t, expiry)| t == bearer && *expiry > now_ms)));
        if !authorized {
            s.auth_failures = s.auth_failures.saturating_add(1);
            return Some(Response::json_status(401, &json!({"error":"PIN required", "pin_required":true})));
        }
        let cancelling = s.active;
        if cancelling {
            s.cancelled = true;
        }
        Some(Response::json(&json!({"cancelling":cancelling,"run_active":s.active})))
    }
}

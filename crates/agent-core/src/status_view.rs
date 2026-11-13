//! Immutable public status, independent of the tool/App mutex.
//!
//! The platform maintenance task is the sole periodic producer. Readers clone an
//! Arc under a tiny lock; JSON construction, serialization and destruction happen
//! outside that lock. Never publish from a small-stack storage writer or a nested
//! tool invocation.
use crate::http::Response;
use crate::run_gate::ToolActivity;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct StatusView {
    pub status: Value,
    pub boot_ms: u64,
    pub sampled_ms: u64,
    pub sampled_at: Instant,
}

impl StatusView {
    pub fn response(
        &self,
        health: bool,
        run_active: bool,
        sessions: usize,
        tool: Option<ToolActivity>,
    ) -> Response {
        let age_ms = self.sampled_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let mut body = if health {
            json!({"ok": true, "devices": self.status["devices"]["count"]})
        } else {
            self.status.clone()
        };
        if !health {
            body["agent"]["run_active"] = json!(run_active);
            body["agent"]["active_tool"] = json!(tool);
            body["mcp"]["sessions"] = json!(sessions);
        }
        body["uptime_s"] = json!(self.sampled_ms.saturating_add(age_ms).saturating_sub(self.boot_ms) / 1000);
        body["snapshot"] = json!(true);
        body["age_ms"] = json!(age_ms);
        body["stale"] = json!(age_ms > 1000);
        Response::json(&body).with_header("Cache-Control", "no-store")
    }
}

#[derive(Clone, Default)]
pub struct StatusViews(Arc<Mutex<Option<Arc<StatusView>>>>);

impl StatusViews {
    pub fn current(&self) -> Option<Arc<StatusView>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn publish(&self, view: StatusView) {
        let view = Arc::new(view);
        let previous = self.0.lock().unwrap_or_else(|p| p.into_inner()).replace(view);
        drop(previous);
    }
}

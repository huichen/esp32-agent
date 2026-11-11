//! Mutable runtime state: sessions, counters, pending actions.
//!
//! Kept out of [`crate::Config`] because it must not be persisted, and out of
//! the drivers because a driver should not know that MCP exists.

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Wall-clock epoch below which the system clock counts as unset (September 2020).
/// A board without a synced clock starts at zero, and subtracting that from a real
/// timestamp is what made `uptime_s` report decades.
const SANE_EPOCH_MS: u64 = 1_600_000_000_000;

/// What one detection pass saw, kept for `inventory` and the portal's device
/// page (including addresses nothing claimed).
#[derive(Debug, Clone, Default)]
pub struct ProbeReport {
    pub buses: Vec<Value>,
    pub unknown: Vec<Value>,
    /// Devices a driver recognised but did not adopt (`devices.discovery = manual`).
    pub candidates: Vec<Value>,
    pub at_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub id: String,
    pub created_ms: u64,
    pub last_seen_ms: u64,
    pub client: String,
    pub protocol: String,
    pub initialized: bool,
    pub requests: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Counters {
    pub requests: u64,
    pub notifications: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub auth_failures: u64,
    pub bad_requests: u64,
    pub http_requests: u64,
}

#[derive(Debug, Default)]
pub struct Runtime {
    pub boot_ms: u64,
    pub firmware: String,
    pub build: String,
    pub sessions: Vec<Session>,
    pub counters: Counters,
    /// Set by `reboot` and the portal; the main loop acts on it.
    pub restart_requested: bool,
    /// Set by `devices_probe`; the main loop re-runs detection.
    pub reprobe_requested: bool,
    /// Set when the config changed and must be written to NVS.
    pub config_dirty: bool,
    pub last_probe_ms: u64,
    pub boot_reason: String,
    pub tool_calls: BTreeMap<String, u64>,
    /// Sliding rate-limit windows: key -> (window start ms, count).
    pub rate: BTreeMap<String, (u64, u64)>,
    /// Bumped on every applied manifest; clients can use it to detect a change.
    pub manifest_rev: u64,
    /// Set when the manifest changed and the registry must be rebuilt.
    pub manifest_dirty: bool,
    /// Set when the schedule changed; `settle` writes it to NVS.
    pub sched_dirty: bool,
    /// Portal tokens minted by `POST /api/auth`: `(token, expires_at_ms)`. They live in
    /// RAM, so a reboot revokes every one of them — which is the right default for a
    /// device whose PIN can be changed from the same page.
    pub auth_tokens: Vec<(String, u64)>,
}

impl Runtime {
    pub fn new(now_ms: u64, firmware: &str, build: &str) -> Self {
        Self {
            boot_ms: now_ms,
            firmware: firmware.to_string(),
            build: build.to_string(),
            ..Default::default()
        }
    }

    pub fn uptime_s(&self, now_ms: u64) -> u64 {
        // The wall clock is unset at boot and only becomes real once SNTP lands, so
        // `now - boot` is meaningless until both ends are on the same calendar.
        // Reporting zero for a couple of seconds is honest; reporting 56 years is not.
        if self.boot_ms < SANE_EPOCH_MS || now_ms < SANE_EPOCH_MS {
            return 0;
        }
        now_ms.saturating_sub(self.boot_ms) / 1000
    }

    /// Re-anchor the uptime clock the first time a real wall clock appears.
    ///
    /// Called by the platform tick. After this the uptime counts from the moment the
    /// clock was set (a few seconds after boot) instead of from the epoch, and stays
    /// monotonic for the rest of the run. Returns `true` when it re-anchored.
    pub fn rebase_uptime(&mut self, now_ms: u64) -> bool {
        if self.boot_ms < SANE_EPOCH_MS && now_ms >= SANE_EPOCH_MS {
            self.boot_ms = now_ms;
            return true;
        }
        false
    }

    /// Find or create the session for an MCP request. Returns the index.
    pub fn touch_session(&mut self, id: &str, now_ms: u64, client: &str) -> usize {
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions[i].last_seen_ms = now_ms;
            self.sessions[i].requests += 1;
            return i;
        }
        self.sessions.push(Session {
            id: id.to_string(),
            created_ms: now_ms,
            last_seen_ms: now_ms,
            client: client.to_string(),
            protocol: String::new(),
            initialized: false,
            requests: 1,
        });
        self.sessions.len() - 1
    }

    /// Drop sessions idle longer than `ttl_ms`, keeping at most `max`.
    /// Returns the ids that were dropped.
    pub fn gc_sessions(&mut self, now_ms: u64, ttl_ms: u64, max: usize) -> Vec<String> {
        let mut dropped = Vec::new();
        self.sessions.retain(|s| {
            let keep = now_ms.saturating_sub(s.last_seen_ms) <= ttl_ms;
            if !keep {
                dropped.push(s.id.clone());
            }
            keep
        });
        while self.sessions.len() > max {
            let oldest = self.sessions.iter().enumerate().min_by_key(|(_, s)| s.last_seen_ms).map(|(i, _)| i);
            match oldest {
                Some(i) => dropped.push(self.sessions.remove(i).id),
                None => break,
            }
        }
        dropped
    }

    pub fn session_json(&self) -> Value {
        let items: Vec<Value> = self
            .sessions
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "age_s": (s.last_seen_ms.saturating_sub(s.created_ms)) / 1000,
                    "idle_s": (self.last_seen_ms_now().saturating_sub(s.last_seen_ms)) / 1000,
                    "client": s.client,
                    "protocol": s.protocol,
                    "initialized": s.initialized,
                    "requests": s.requests,
                })
            })
            .collect();
        json!(items)
    }

    fn last_seen_ms_now(&self) -> u64 {
        self.sessions.iter().map(|s| s.last_seen_ms).max().unwrap_or(self.boot_ms)
    }

    pub fn record_tool_call(&mut self, name: &str, ok: bool) {
        self.counters.tool_calls += 1;
        if !ok {
            self.counters.tool_errors += 1;
        }
        *self.tool_calls.entry(name.to_string()).or_insert(0) += 1;
    }

    /// Count one call in a fixed window and return the count including it.
    /// Used by manifest `safety.max_calls_per_min`.
    pub fn rate_hit(&mut self, key: &str, now_ms: u64, window_ms: u64) -> u64 {
        let entry = self.rate.entry(key.to_string()).or_insert((now_ms, 0));
        if now_ms.saturating_sub(entry.0) > window_ms {
            *entry = (now_ms, 0);
        }
        entry.1 += 1;
        entry.1
    }

    pub fn counters_json(&self) -> Value {
        json!({
            "requests": self.counters.requests,
            "notifications": self.counters.notifications,
            "http_requests": self.counters.http_requests,
            "tool_calls": self.counters.tool_calls,
            "tool_errors": self.counters.tool_errors,
            "auth_failures": self.counters.auth_failures,
            "bad_requests": self.counters.bad_requests,
            "per_tool": self.tool_calls,
        })
    }
}

/// Session ids only have to be unique on this device, so a counter mixed with
/// the boot time is enough — and it avoids pulling in a random number source.
pub fn make_session_id(seed: u64, counter: u64, now_ms: u64) -> String {
    let mut x = seed ^ now_ms.rotate_left(17) ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    format!("{:016x}", x)
}

impl Runtime {
    /// Mint a portal token that lives for `ttl_s`. Returns the token.
    ///
    /// Not a session cookie: the portal keeps it in `localStorage` and sends it as a
    /// bearer token, so the same string works for `curl` and for the page without a
    /// cookie jar. Old tokens are purged here rather than on a timer — the only moment
    /// the list can grow is a login.
    pub fn issue_token(&mut self, seed: u64, now_ms: u64, ttl_s: u64) -> String {
        self.auth_tokens.retain(|(_, expiry)| *expiry > now_ms);
        let token = make_session_id(seed, self.auth_tokens.len() as u64 + 7, now_ms);
        self.auth_tokens.push((token.clone(), now_ms + ttl_s * 1000));
        token
    }

    /// True when the token was minted here and has not expired.
    pub fn token_valid(&mut self, token: &str, now_ms: u64) -> bool {
        self.auth_tokens.retain(|(_, expiry)| *expiry > now_ms);
        self.auth_tokens.iter().any(|(t, _)| t == token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A board boots with the clock at zero and SNTP sets it a second later. Uptime
    /// must not report the gap between 1970 and now in the meantime, and must stay
    /// monotonic once re-anchored.
    #[test]
    fn uptime_waits_for_a_real_clock_and_then_counts_forward() {
        let mut rt = Runtime::new(0, "test", "test");
        assert_eq!(rt.uptime_s(2_500), 0, "no wall clock yet: unknown, not 56 years");
        let anchor = SANE_EPOCH_MS + 3_000;
        assert!(rt.rebase_uptime(anchor));
        assert!(!rt.rebase_uptime(anchor + 1_000), "re-anchoring is a one-off");
        assert_eq!(rt.uptime_s(anchor + 5_000), 5);
        assert!(rt.uptime_s(anchor + 9_000) > rt.uptime_s(anchor + 5_000));
    }

    #[test]
    fn uptime_is_zero_when_the_clock_jumps_backwards() {
        let rt = Runtime::new(SANE_EPOCH_MS, "test", "test");
        assert_eq!(rt.uptime_s(0), 0);
    }

    #[test]
    fn sessions_are_touched_and_expired() {
        let mut rt = Runtime::new(0, "test", "test");
        let i = rt.touch_session("abc", 1_000, "curl");
        assert_eq!(rt.sessions[i].requests, 1);
        rt.touch_session("abc", 2_000, "curl");
        assert_eq!(rt.sessions.len(), 1);
        assert_eq!(rt.sessions[0].requests, 2);

        let dropped = rt.gc_sessions(10_000_000, 5_000, 4);
        assert_eq!(dropped, vec!["abc".to_string()]);
        assert!(rt.sessions.is_empty());
    }

    #[test]
    fn session_cap_is_enforced_oldest_first() {
        let mut rt = Runtime::new(0, "t", "t");
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            rt.touch_session(name, 100 * (i as u64 + 1), "x");
        }
        let dropped = rt.gc_sessions(1_000, 60_000, 2);
        assert_eq!(dropped, vec!["a".to_string()]);
        assert_eq!(rt.sessions.len(), 2);
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = make_session_id(0x1234, 1, 1_000);
        let b = make_session_id(0x1234, 2, 1_000);
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert_eq!(a, make_session_id(0x1234, 1, 1_000));
    }

    #[test]
    fn tool_counters_track_failures() {
        let mut rt = Runtime::new(0, "t", "t");
        rt.record_tool_call("led_set", true);
        rt.record_tool_call("led_set", false);
        assert_eq!(rt.counters.tool_calls, 2);
        assert_eq!(rt.counters.tool_errors, 1);
        assert_eq!(rt.tool_calls["led_set"], 2);
    }

    #[test]
    fn rate_windows_reset_after_the_window() {
        let mut rt = Runtime::new(0, "t", "t");
        assert_eq!(rt.rate_hit("k", 1_000, 60_000), 1);
        assert_eq!(rt.rate_hit("k", 1_500, 60_000), 2);
        assert_eq!(rt.rate_hit("k", 120_000, 60_000), 1, "a new window starts over");
    }
}

//! A bounded in-memory log ring.
//!
//! The board has no console attached most of the time: the WiFi config page is
//! the only way to see what went wrong during boot or during a tool call. The
//! ring keeps the newest `capacity` lines and is served at `/api/logs`.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Level ranks, low to high. `log.level` names one of these and everything below it is
/// dropped on the way into the ring.
const TRACE: u8 = 0;
const DEBUG: u8 = 1;
const INFO: u8 = 2;
const WARN: u8 = 3;
const ERROR: u8 = 4;

/// An unknown or empty name is `info`, which is also the documented default: refusing the
/// whole log because a field was misspelled is worse than logging at the usual level.
fn rank(name: &str) -> u8 {
    match name.trim().to_ascii_lowercase().as_str() {
        "trace" => TRACE,
        "debug" => DEBUG,
        "warn" | "warning" => WARN,
        "error" => ERROR,
        _ => INFO,
    }
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub seq: u64,
    pub uptime_ms: u64,
    pub level: &'static str,
    pub target: String,
    pub message: String,
}

#[derive(Debug)]
pub struct LogBuf {
    inner: Mutex<Inner>,
    start: Instant,
    capacity: usize,
    /// The lowest level that reaches the ring: `log.level` in the configuration. Atomic
    /// because every request may set it and every line reads it without taking the lock.
    min_level: AtomicU8,
}

#[derive(Debug)]
struct Inner {
    lines: VecDeque<LogLine>,
    seq: u64,
}

impl LogBuf {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner { lines: VecDeque::with_capacity(capacity.min(1024)), seq: 0 }),
            start: Instant::now(),
            capacity: capacity.max(4),
            min_level: AtomicU8::new(INFO),
        })
    }

    /// Apply `log.level`. Before this existed the field was stored, validated, served and
    /// shown in the portal, and no line was ever filtered by it.
    pub fn set_min_level(&self, name: &str) {
        self.min_level.store(rank(name), Ordering::Relaxed);
    }

    pub fn uptime_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn push(&self, level: &'static str, target: &str, message: impl AsRef<str>) {
        if rank(level) < self.min_level.load(Ordering::Relaxed) {
            return;
        }
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.seq += 1;
        let line = LogLine {
            seq: g.seq,
            uptime_ms: self.start.elapsed().as_millis() as u64,
            level,
            target: target.to_string(),
            message: message.as_ref().to_string(),
        };
        g.lines.push_back(line);
        while g.lines.len() > self.capacity {
            g.lines.pop_front();
        }
    }

    pub fn info(&self, target: &str, message: impl AsRef<str>) {
        self.push("info", target, message);
    }

    pub fn warn(&self, target: &str, message: impl AsRef<str>) {
        self.push("warn", target, message);
    }

    pub fn error(&self, target: &str, message: impl AsRef<str>) {
        self.push("error", target, message);
    }

    pub fn tail(&self, n: usize, since_seq: u64) -> Vec<LogLine> {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut out: Vec<LogLine> = g.lines.iter().filter(|l| l.seq > since_seq).cloned().collect();
        if out.len() > n {
            out = out.split_off(out.len() - n);
        }
        out
    }

    pub fn to_json(&self, n: usize, since_seq: u64) -> Value {
        let lines: Vec<Value> = self
            .tail(n, since_seq)
            .into_iter()
            .map(|l| {
                json!({
                    "seq": l.seq,
                    "t_ms": l.uptime_ms,
                    "level": l.level,
                    "target": l.target,
                    "msg": l.message,
                })
            })
            .collect();
        json!({ "lines": lines })
    }

    pub fn clear(&self) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.lines.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_drops_oldest_and_tail_respects_seq() {
        let l = LogBuf::new(4);
        for i in 0..10 {
            l.info("t", format!("line {i}"));
        }
        let tail = l.tail(10, 0);
        assert_eq!(tail.len(), 4);
        assert_eq!(tail[3].message, "line 9");
        let after = l.tail(10, tail[2].seq);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].message, "line 9");
    }

    /// `log.level` was stored, validated, served in the portal and shown in the Settings
    /// page while every line reached the ring anyway. It filters on the way in now, and an
    /// error is never filtered out by a quieter level.
    #[test]
    fn the_ring_drops_lines_below_the_configured_level() {
        let l = LogBuf::new(16);
        l.info("boot", "an info line");
        assert_eq!(l.tail(10, 0).len(), 1, "info is the default level");

        l.set_min_level("warn");
        l.info("boot", "too quiet to keep");
        l.warn("boot", "a warning");
        l.error("boot", "an error");
        let lines = l.tail(10, 0);
        let kept: Vec<&str> = lines.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(kept, vec!["an info line", "a warning", "an error"], "{kept:?}");

        // The other direction too, and an unknown name is the documented default rather than
        // a level that silently keeps everything.
        l.set_min_level("trace");
        l.info("boot", "back on");
        assert_eq!(l.tail(1, 0)[0].message, "back on");
        // An unknown name is the documented default (`info`), not a level that silently drops
        // everything the board says.
        l.set_min_level("verbose-nonsense");
        l.info("boot", "kept, because an unknown level means info");
        assert_eq!(l.tail(1, 0)[0].message, "kept, because an unknown level means info");
    }
}

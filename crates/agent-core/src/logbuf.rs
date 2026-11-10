//! A bounded in-memory log ring.
//!
//! The board has no console attached most of the time: the WiFi config page is
//! the only way to see what went wrong during boot or during a tool call. The
//! ring keeps the newest `capacity` lines and is served at `/api/logs`.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
        })
    }

    pub fn uptime_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn push(&self, level: &'static str, target: &str, message: impl AsRef<str>) {
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
}

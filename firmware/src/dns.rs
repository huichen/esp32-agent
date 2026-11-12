//! Captive-portal DNS: every name resolves to the setup access point.
//!
//! Without this, a phone that joins `esp32-agent-setup` shows "no internet" and the
//! setup page never opens by itself. With it, any HTTP request the phone makes
//! lands on 192.168.4.1, which serves the portal.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use esp_idf_svc::sys;

pub struct MiniDns {
    stop: Arc<AtomicBool>,
}

impl MiniDns {
    /// Bind UDP/53 and answer every A query with `ip`. `port` is a parameter so
    /// the host build can be exercised without privileges.
    pub fn start(ip: Ipv4Addr, port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        // The one thread in this firmware whose stack can live in PSRAM, and the reason is
        // narrow: its whole body is `recvfrom`, an A-record reply and `send_to`, so it can
        // never be running while the flash cache is off. That is IDF's condition for an
        // external stack — with the cache off, PSRAM is not addressable, and a thread that
        // commits to NVS with its stack there asserts in
        // `esp_task_stack_is_sane_cache_disabled` and reboots the board. Every other
        // thread here writes NVS somewhere (config, manifest, transcript, provisioning),
        // which is why their stacks stay internal (`docs/troubleshooting.md`).
        //
        // `esp_pthread_set_cfg` is configured per *calling* thread and applies to whatever
        // it spawns next, so it is restored before anything else is created from here: the
        // HTTP listener is later, and a PSRAM stack for that one would be a crash waiting
        // for the next config save.
        let spawned = unsafe {
            let mut cfg = sys::esp_pthread_get_default_config();
            cfg.stack_alloc_caps = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
            cfg.stack_size = 4 * 1024;
            let asked = sys::esp_pthread_set_cfg(&cfg) == 0;
            let spawned = std::thread::Builder::new().name("dns".into()).stack_size(4 * 1024).spawn(
                move || {
                    let mut buf = [0u8; 512];
                    while !stop_thread.load(Ordering::SeqCst) {
                        let Ok((n, peer)) = socket.recv_from(&mut buf) else { continue };
                        if let Some(reply) = answer(&buf[..n], ip) {
                            let _ = socket.send_to(&reply, peer);
                        }
                    }
                },
            );
            // Restore unconditionally: a failed spawn must not leave the next thread with
            // caps PSRAM cannot keep.
            let back = sys::esp_pthread_get_default_config();
            sys::esp_pthread_set_cfg(&back);
            if !asked {
                log::warn!("dns: the stack could not be put in PSRAM; using internal RAM");
            }
            spawned
        }?;
        drop(spawned);
        Ok(Self { stop })
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for MiniDns {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Build a DNS response for one question, echoing the name and the question type.
fn answer(query: &[u8], ip: Ipv4Addr) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    // Only standard queries (QR=0) are worth answering.
    if query[2] & 0x80 != 0 {
        return None;
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    while pos < query.len() {
        let len = query[pos] as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if pos + 1 + len > query.len() {
            return None;
        }
        labels.push(&query[pos + 1..pos + 1 + len]);
        pos += 1 + len;
    }
    if pos + 4 > query.len() {
        return None;
    }
    let qtype = &query[pos..pos + 2];
    let question = &query[12..pos + 4];

    let mut out = Vec::with_capacity(question.len() + 32);
    out.extend_from_slice(&query[0..2]); // transaction id
    out.push(0x81); // response, recursion desired
    out.push(0x80); // no error, no recursion available
    out.extend_from_slice(&[0, 1]); // one question
    out.extend_from_slice(&[0, 1]); // one answer
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(question);

    // A record with a 60 s TTL. AAAA/other types get an answer too (type A), which
    // keeps clients from falling back to a slow IPv6 path.
    out.extend_from_slice(&[0xC0, 0x0C]); // pointer to the question name
    out.extend_from_slice(&[0, 1]); // type A
    out.extend_from_slice(&[0, 1]); // class IN
    out.extend_from_slice(&[0, 0, 0, 60]);
    out.extend_from_slice(&[0, 4]);
    out.extend_from_slice(&ip.octets());
    let _ = qtype;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &[u8]) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split(|b| *b == b'.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label);
        }
        q.push(0);
        q.extend_from_slice(&[0, 1, 0, 1]);
        q
    }

    #[test]
    fn answers_with_the_access_point_address() {
        let q = query(b"connectivitycheck.gstatic.com");
        let reply = answer(&q, Ipv4Addr::new(192, 168, 4, 1)).unwrap();
        assert_eq!(&reply[0..2], &[0x12, 0x34]);
        assert_eq!(&reply[2..4], &[0x81, 0x80]);
        assert_eq!(&reply[6..8], &[0, 1], "one answer");
        assert_eq!(&reply[reply.len() - 4..], &[192, 168, 4, 1]);
    }

    #[test]
    fn ignores_responses_and_short_packets() {
        assert!(answer(&[0; 8], Ipv4Addr::LOCALHOST).is_none());
        let mut q = query(b"x.test");
        q[2] |= 0x80;
        assert!(answer(&q, Ipv4Addr::LOCALHOST).is_none());
    }
}

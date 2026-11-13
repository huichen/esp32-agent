//! The simulator's model transport: HTTP/1.1 over `std::net`, with TLS for `https://`.
//!
//! The firmware does the same job with esp-tls and the ESP-IDF certificate bundle; the
//! simulator uses rustls and the webpki root store, so an `https://` endpoint can be
//! tried on the desk exactly as it will behave on the board. Plain `http://` stays
//! supported because a LAN endpoint — and the board's own `/selftest/chat/completions`
//! — needs no TLS.
//!
//! The request is written by hand for the same reason the server is: one shape, no
//! hidden retries, and the bytes on the wire are the bytes the board will send.

use esp32_agent_core::http_client::{self, Target};
use esp32_agent_core::llm::{http_error, ChatTransport};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One `POST` to an OpenAI-compatible endpoint, over plain TCP or TLS.
pub struct HostTransport {
    timeout: Duration,
}

impl HostTransport {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

/// Install the process-wide rustls crypto provider.
///
/// rustls 0.23 refuses to guess between its providers, and the first TLS connection
/// would otherwise fail at runtime with a "no process-level CryptoProvider" panic path.
/// Called once from `main`.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A connection that may or may not be encrypted; nothing above it needs to know.
enum Conn {
    Plain(TimedTcp),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TimedTcp>>),
}

/// One deadline for connecting, writing, the TLS handshake and the whole reply.
/// A peer trickling a byte before each idle timeout cannot keep a model call alive.
struct TimedTcp {
    socket: TcpStream,
    deadline: Instant,
}

impl TimedTcp {
    fn remaining(&self) -> std::io::Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "model request deadline expired"));
        }
        Ok(remaining)
    }
}

impl Read for TimedTcp {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(buf)
    }
}

impl Write for TimedTcp {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.flush()
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

fn tls(stream: TimedTcp, host: &str) -> Result<Conn, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("`{host}` is not a usable TLS server name: {e}"))?;
    let session = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| format!("TLS setup for {host} failed: {e}"))?;
    Ok(Conn::Tls(Box::new(rustls::StreamOwned::new(session, stream))))
}

impl ChatTransport for HostTransport {
    fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    fn post_json(&mut self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String> {
        self.request("POST", url, headers, body)
    }

    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<String, String> {
        let target = Target::parse(url)?;
        let body = if method.eq_ignore_ascii_case("GET") { "" } else { body };
        let head = http_client::request_head(method, &target, headers, body.len())?;
        let deadline = Instant::now() + self.timeout;
        let socket = target.connect(self.timeout)?;
        socket.set_nodelay(true).ok();
        let stream = TimedTcp { socket, deadline };
        let mut conn = if target.tls { tls(stream, &target.host)? } else { Conn::Plain(stream) };
        conn.write_all(head.as_bytes()).map_err(|e| format!("write failed: {e}"))?;
        if !body.is_empty() {
            conn.write_all(body.as_bytes()).map_err(|e| format!("write failed: {e}"))?;
        }
        conn.flush().map_err(|e| format!("flush failed: {e}"))?;

        let response = http_client::read_response(&mut conn)?;
        if (200..300).contains(&response.status) {
            Ok(response.body)
        } else {
            Err(http_error(response.status, &response.body))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_reply(response: &[u8]) -> Result<String, String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = response.to_vec();
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            socket.write_all(&response).unwrap();
        });
        let reply =
            HostTransport::new(Duration::from_secs(2)).request("GET", &format!("http://{address}/"), &[], "");
        peer.join().unwrap();
        reply
    }

    #[test]
    fn chunked_and_torn_replies_are_not_raw_successful_bodies() {
        let body = "{\"ok\":true}";
        let raw = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        );
        assert_eq!(local_reply(raw.as_bytes()).unwrap(), body);
        assert!(local_reply(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());

        // Every byte arrives before the old per-read timeout. Only a shared deadline
        // can stop this header from occupying the model thread indefinitely.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok" {
                if socket.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let started = Instant::now();
        let mut transport = HostTransport::new(Duration::from_secs(3));
        transport.set_timeout(Duration::from_millis(160));
        let result = transport.request("GET", &format!("http://{address}/"), &[], "");
        let elapsed = started.elapsed();
        peer.join().unwrap();
        assert!(result.is_err(), "a trickling response completed past its deadline: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "per-read timeouts restarted the deadline: {elapsed:?}");
    }

    #[test]
    fn schemes_ports_and_paths_are_split_the_way_the_request_needs() {
        let t = Target::parse("https://api.example.com/v1/chat/completions").unwrap();
        assert!(t.tls);
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("api.example.com", 443, "/v1/chat/completions")
        );
        assert_eq!(t.host_header(), "api.example.com");

        let t = Target::parse("http://192.0.2.10:8899/v1/chat/completions").unwrap();
        assert!(!t.tls);
        assert_eq!((t.host.as_str(), t.port), ("192.0.2.10", 8899));
        assert_eq!(t.host_header(), "192.0.2.10:8899", "a non-default port belongs in Host");

        let t = Target::parse("https://example.com").unwrap();
        assert_eq!(t.path, "/", "no path still means the root");

        assert!(Target::parse("ftp://example.com/x").is_err());
        assert!(Target::parse("example.com/x").is_err());
    }
}

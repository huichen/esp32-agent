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

use esp32_agent_core::llm::ChatTransport;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

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
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
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

struct Target {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

impl Target {
    fn parse(url: &str) -> Result<Self, String> {
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(format!("only http:// and https:// are supported: {url}"));
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                let port = port.parse::<u16>().map_err(|_| format!("`{port}` is not a port in {url}"))?;
                (host, port)
            }
            _ => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(format!("{url} has no host"));
        }
        Ok(Self { tls, host: host.to_string(), port, path: path.to_string() })
    }

    /// The `Host` header: the port is left off when it is the scheme's default.
    fn host_header(&self) -> String {
        let default = if self.tls { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn tls(stream: TcpStream, host: &str) -> Result<Conn, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("`{host}` is not a usable TLS server name: {e}"))?;
    let session = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| format!("TLS setup for {host} failed: {e}"))?;
    Ok(Conn::Tls(Box::new(rustls::StreamOwned::new(session, stream))))
}

/// Read to the end, tolerating a peer that closes without `close_notify`.
///
/// An HTTP server that answered `Connection: close` may drop the TCP socket instead of
/// finishing the TLS shutdown; rustls reports that as `UnexpectedEof` *after* all the
/// application data has arrived, and the response is still complete.
fn read_all(conn: &mut Conn) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match conn.read(&mut buf) {
            Ok(0) => return Ok(raw),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(raw),
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
}

impl ChatTransport for HostTransport {
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
        let addr = format!("{}:{}", target.host, target.port);
        let stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr} failed: {e}"))?;
        stream.set_read_timeout(Some(self.timeout)).ok();
        stream.set_write_timeout(Some(self.timeout)).ok();
        stream.set_nodelay(true).ok();
        let mut conn = if target.tls { tls(stream, &target.host)? } else { Conn::Plain(stream) };

        // GET carries no body and no framing headers, exactly like the board's transport.
        let body = if method.eq_ignore_ascii_case("GET") { "" } else { body };
        let mut head = format!("{method} {} HTTP/1.1\r\nHost: {}\r\n", target.path, target.host_header());
        if !body.is_empty() {
            head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
        }
        head.push_str("Connection: close\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        conn.write_all(head.as_bytes()).map_err(|e| format!("write failed: {e}"))?;
        if !body.is_empty() {
            conn.write_all(body.as_bytes()).map_err(|e| format!("write failed: {e}"))?;
        }
        conn.flush().map_err(|e| format!("flush failed: {e}"))?;

        let raw = read_all(&mut conn)?;
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| "response has no header block".to_string())?;
        let head_text = String::from_utf8_lossy(&raw[..split]).to_string();
        let status: u16 = head_text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let payload = String::from_utf8_lossy(&raw[split + 4..]).to_string();
        if (200..300).contains(&status) {
            Ok(payload)
        } else {
            Err(format!("HTTP {status}: {}", payload.chars().take(200).collect::<String>()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

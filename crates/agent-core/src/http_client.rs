//! The bounded HTTP/1.x wire format used by both model transports.
//!
//! TLS and sockets belong to the platform. URL/request framing and response decoding do
//! not: sharing them makes a chunked provider reply behave the same on the simulator and
//! the board. Bodies are decoded directly into one bounded buffer, never read-to-close
//! and copied a second time before the chunk framing is removed.

use std::io::{BufRead, BufReader, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub const MAX_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Target {
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        if url.len() > MAX_HEADER_BYTES || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err("the HTTP URL is too long or contains unescaped whitespace/control characters".into());
        }
        let lower = url.to_ascii_lowercase();
        let (tls, rest) = if lower.starts_with("https://") {
            (true, &url[8..])
        } else if lower.starts_with("http://") {
            (false, &url[7..])
        } else {
            return Err("the endpoint must start with http:// or https://".into());
        };
        let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..split];
        if authority.is_empty() || authority.contains('@') {
            return Err("the HTTP URL needs a host without embedded credentials".into());
        }
        if authority.starts_with('[') || authority.matches(':').count() > 1 {
            return Err("IPv6 literals are not supported; use an IPv4 address or a host name".into());
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("the HTTP URL has an invalid port".into());
                }
                let port = port.parse::<u16>().map_err(|_| "the HTTP URL has an invalid port")?;
                if port == 0 {
                    return Err("the HTTP port must be in 1..=65535".into());
                }
                (host, port)
            }
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err("the HTTP URL has no host".into());
        }
        let tail = rest[split..].split('#').next().unwrap_or("");
        let path = if tail.is_empty() {
            "/".into()
        } else if tail.starts_with('?') {
            format!("/{tail}")
        } else {
            tail.to_string()
        };
        Ok(Self { tls, host: host.into(), port, path })
    }

    pub fn host_header(&self) -> String {
        if self.port == if self.tls { 443 } else { 80 } {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Resolve once, then share a bounded connect budget across all candidates.
    /// Name resolution itself is controlled by the platform's resolver.
    pub fn connect(&self, timeout: Duration) -> Result<TcpStream, String> {
        let started = Instant::now();
        let addrs = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| format!("could not resolve `{}`: {e}", self.host_header()))?;
        let mut last = format!("`{}` resolved to no address", self.host_header());
        for addr in addrs {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(format!("connecting to `{}` timed out", self.host_header()));
            }
            match TcpStream::connect_timeout(&addr, remaining) {
                Ok(stream) => return Ok(stream),
                Err(e) => last = format!("connecting to {addr} failed: {e}"),
            }
        }
        Err(last)
    }
}

fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Only the request head: write the borrowed body separately, so a 2 MiB history is
/// not copied into a second outgoing buffer merely to concatenate it with headers.
pub fn request_head(
    method: &str,
    target: &Target,
    headers: &[(&str, &str)],
    body_len: usize,
) -> Result<String, String> {
    if method.is_empty() || !method.bytes().all(token_byte) {
        return Err("invalid HTTP method".into());
    }
    let get = method.eq_ignore_ascii_case("GET");
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        target.path,
        target.host_header()
    );
    if !get {
        head.push_str(&format!("Content-Length: {body_len}\r\n"));
        if body_len > 0 && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            head.push_str("Content-Type: application/json\r\n");
        }
    }
    for &(name, value) in headers {
        if name.is_empty()
            || !name.bytes().all(token_byte)
            || value.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f)
        {
            return Err("invalid HTTP request header".into());
        }
        if ["host", "connection", "content-length", "transfer-encoding"]
            .iter()
            .any(|k| name.eq_ignore_ascii_case(k))
        {
            continue; // framing is owned here, never by a caller
        }
        if head.len().saturating_add(name.len()).saturating_add(value.len()).saturating_add(6)
            > MAX_HEADER_BYTES
        {
            return Err("the request headers are too large".into());
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    if head.len() > MAX_HEADER_BYTES {
        return Err("the request headers are too large".into());
    }
    Ok(head)
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Read one complete reply. A fixed or chunked reply ends at its own boundary, not
/// at a later connection close; a short or oversized reply is an error, never silent
/// truncation that might happen to look like a successful JSON document.
pub fn read_response(reader: impl Read) -> Result<Response, String> {
    let mut reader = BufReader::with_capacity(1024, reader);
    let mut header_bytes = 0;
    for interim in 0..=8 {
        let status_line = header_line(&mut reader, &mut header_bytes)?;
        let mut parts = status_line.split_whitespace();
        let version = parts.next().unwrap_or("");
        let code = parts.next().unwrap_or("");
        let status = code
            .parse::<u16>()
            .ok()
            .filter(|s| (100..=599).contains(s) && code.len() == 3)
            .filter(|_| matches!(version, "HTTP/1.0" | "HTTP/1.1"))
            .ok_or_else(|| "unexpected HTTP status line".to_string())?;
        let mut length = None;
        let mut chunked = false;
        let mut transfer_seen = false;
        loop {
            let line = header_line(&mut reader, &mut header_bytes)?;
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').ok_or_else(|| "invalid reply header".to_string())?;
            if name.is_empty() || !name.bytes().all(token_byte) {
                return Err("invalid reply header name".into());
            }
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("invalid or duplicate reply content length".into());
                }
                length = Some(value.parse::<usize>().map_err(|_| "invalid reply content length")?);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                if transfer_seen || !value.eq_ignore_ascii_case("chunked") {
                    return Err("unsupported or duplicate reply transfer encoding".into());
                }
                transfer_seen = true;
                chunked = true;
            }
        }
        if transfer_seen && length.is_some() {
            return Err("ambiguous reply framing".into());
        }
        if status < 200 {
            if status == 101 || interim == 8 {
                return Err("unsupported informational HTTP reply".into());
            }
            continue;
        }
        if status == 204 || status == 304 {
            return Ok(Response { status, body: String::new() });
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                let line = line(&mut reader, 128)?;
                let size = line.split(';').next().unwrap_or("");
                if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("invalid reply chunk size".into());
                }
                let size = usize::from_str_radix(size, 16).map_err(|_| "invalid reply chunk size")?;
                if size == 0 {
                    let mut trailer_bytes = 0;
                    while !header_line(&mut reader, &mut trailer_bytes)?.is_empty() {}
                    break;
                }
                append_exact(&mut reader, &mut body, size)?;
                let mut end = [0; 2];
                reader
                    .read_exact(&mut end)
                    .map_err(|e| format!("the reply ended inside chunk framing: {e}"))?;
                if end != *b"\r\n" {
                    return Err("missing CRLF after reply chunk".into());
                }
            }
        } else if let Some(length) = length {
            append_exact(&mut reader, &mut body, length)?;
        } else {
            let mut buf = [0; 512];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        reserve(&mut body, n)?;
                        body.extend_from_slice(&buf[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // A close-delimited TLS response may omit close_notify. Only a
                    // genuine EOF is a close; timeout/reset errors must not become OK.
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(format!("reading the reply failed: {e}")),
                }
            }
        }
        let body = match String::from_utf8(body) {
            Ok(text) => text,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        return Ok(Response { status, body });
    }
    unreachable!("the informational response count is bounded")
}

fn header_line(reader: &mut impl BufRead, total: &mut usize) -> Result<String, String> {
    let remaining = MAX_HEADER_BYTES.saturating_sub(*total);
    if remaining < 2 {
        return Err("the reply headers exceed their byte budget".into());
    }
    let text = line(reader, remaining - 2)?;
    *total += text.len() + 2;
    Ok(text)
}

fn line(reader: &mut impl BufRead, max: usize) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(max.min(128));
    (&mut *reader)
        .take((max + 2) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|e| format!("reading the reply failed: {e}"))?;
    if !bytes.ends_with(b"\r\n") {
        return Err("the reply has an incomplete or oversized CRLF line".into());
    }
    bytes.truncate(bytes.len() - 2);
    String::from_utf8(bytes).map_err(|_| "invalid UTF-8 in reply headers".into())
}

fn reserve(body: &mut Vec<u8>, size: usize) -> Result<(), String> {
    if size > MAX_BODY_BYTES.saturating_sub(body.len()) {
        return Err(format!("the reply body exceeds {MAX_BODY_BYTES} bytes"));
    }
    let needed = body.len() + size;
    if needed > body.capacity() {
        let capacity = needed.max(body.capacity().saturating_mul(2)).min(MAX_BODY_BYTES);
        body.try_reserve_exact(capacity - body.len()).map_err(|_| "not enough memory for the reply")?;
    }
    Ok(())
}

fn append_exact(reader: &mut impl Read, body: &mut Vec<u8>, size: usize) -> Result<(), String> {
    reserve(body, size)?;
    let start = body.len();
    body.resize(start + size, 0);
    reader.read_exact(&mut body[start..]).map_err(|e| format!("the reply body is incomplete: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Pieces<'a> {
        data: &'a [u8],
        size: usize,
    }
    impl Read for Pieces<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.size).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn urls_keep_queries_drop_fragments_and_refuse_request_injection() {
        let t = Target::parse("HTTPS://example.com:8443?x=1#local").unwrap();
        assert!(t.tls);
        assert_eq!(t.host_header(), "example.com:8443");
        assert_eq!(t.path, "/?x=1");
        assert_eq!(Target::parse("http://example.com/a?b=2#c").unwrap().path, "/a?b=2");
        for url in [
            "ftp://x/",
            "http:///",
            "http://x:0/",
            "http://x:65536/",
            "http://x:no/",
            "http://user@x/",
            "http://x/a\r\nX:bad",
            "http://[::1]/",
        ] {
            assert!(Target::parse(url).is_err(), "accepted {url:?}");
        }
    }

    #[test]
    fn request_heads_do_not_copy_bodies_or_duplicate_framing() {
        let target = Target::parse("https://example.com/v1").unwrap();
        let head = request_head(
            "POST",
            &target,
            &[("Content-Type", "application/json"), ("Host", "wrong"), ("Content-Length", "7")],
            2 * 1024 * 1024,
        )
        .unwrap();
        assert!(head.len() < 256);
        assert!(head.contains("Content-Length: 2097152\r\n"));
        assert_eq!(head.matches("Content-Type:").count(), 1);
        assert!(!head.contains("wrong"));
        let get = request_head("GET", &target, &[], 999).unwrap();
        assert!(!get.contains("Content-Length"));
        assert!(request_head("BAD\r\n", &target, &[], 0).is_err());
        assert!(request_head("POST", &target, &[("Authorization", "ok\r\nX:bad")], 0).is_err());
    }

    #[test]
    fn framed_replies_decode_identically_at_every_socket_split() {
        let body = "{\"text\":\"한글日本語\"}";
        let fixed = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let chunked = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n{:x};foo=bar\r\n{body}\r\n0\r\nX-Trailer: yes\r\n\r\n", body.len());
        let closed = format!("HTTP/1.0 200 OK\r\n\r\n{body}");
        for raw in [fixed, chunked, closed] {
            for size in [1, 2, 7, 512, 1024] {
                let response = read_response(Pieces { data: raw.as_bytes(), size }).unwrap();
                assert_eq!(response.status, 200);
                assert_eq!(response.body, body);
            }
        }
    }

    #[test]
    fn torn_ambiguous_and_malformed_replies_are_errors() {
        for raw in [
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nxXX0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: notchunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            "HTTP/1.1 200 OK\r\nContent-Length: garbage\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nfffffffffffffffff\r\n",
            "HTTP/1.1 200 OK\r\nHeader: incomplete",
            "HTTP/1.1 999 Bad\r\n\r\n",
        ] {
            assert!(read_response(Cursor::new(raw)).is_err(), "accepted {raw:?}");
        }
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"))
            }
        }
        let mut partial = Cursor::new(b"HTTP/1.1 200 OK\r\n\r\npartial").chain(Failing);
        assert!(read_response(&mut partial).is_err(), "a timeout must not publish a partial body");
    }

    #[test]
    fn reply_limits_are_enforced_before_a_declared_body_is_read() {
        let large = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            MAX_BODY_BYTES + 1,
            "x".repeat(MAX_BODY_BYTES + 1)
        );
        let mut source = Cursor::new(large.as_bytes());
        assert!(read_response(&mut source).is_err());
        assert!(source.position() <= 1024, "oversized body was read before refusal");
        let header = format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "x".repeat(MAX_HEADER_BYTES));
        assert!(read_response(Cursor::new(header)).is_err());
        let edge = format!("HTTP/1.0 200 OK\r\n\r\n{}", "x".repeat(MAX_BODY_BYTES));
        assert_eq!(read_response(Cursor::new(edge)).unwrap().body.len(), MAX_BODY_BYTES);
        let oversized = format!("HTTP/1.0 200 OK\r\n\r\n{}", "x".repeat(MAX_BODY_BYTES + 1));
        assert!(read_response(Cursor::new(oversized)).is_err());
    }

    #[test]
    fn informational_and_bodyless_replies_end_at_the_right_boundary() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: /x\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(read_response(Cursor::new(raw)).unwrap().body, "{}");
        for status in [204, 304] {
            let raw = format!("HTTP/1.1 {status} Empty\r\nContent-Length: 999999\r\n\r\n");
            let response = read_response(Cursor::new(raw)).unwrap();
            assert_eq!(response.status, status);
            assert!(response.body.is_empty());
        }
    }
}

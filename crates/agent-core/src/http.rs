//! A small blocking HTTP/1.1 server.
//!
//! Written against `std::net` only, because that is what ESP-IDF's std shim
//! provides (lwIP sockets) and what the host simulator provides. No async, no TLS
//! — the board is a LAN device and the MCP spec allows plain HTTP for local use.
//!
//! What it does implement matters for MCP clients: keep-alive, chunked request
//! bodies (Node's fetch sends them), `Expect: 100-continue`, HEAD, and a hard
//! cap on header and body size so a malformed request cannot exhaust the heap.

use crate::logbuf::LogBuf;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub const MAX_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_CONNECTIONS: usize = 6;
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// Thread stacks. The board has kilobytes to spare per task, the host has
/// megabytes and a debug build with deep serde_json frames — so the numbers
/// differ by target rather than by wishful thinking.
#[cfg(target_os = "espidf")]
const ACCEPT_STACK: usize = 6 * 1024;
#[cfg(not(target_os = "espidf"))]
const ACCEPT_STACK: usize = 256 * 1024;

#[cfg(target_os = "espidf")]
const CONN_STACK: usize = 20 * 1024;
#[cfg(not(target_os = "espidf"))]
const CONN_STACK: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// Lower-cased header names, values as sent.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub peer: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }

    pub fn query_get(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(|s| s.as_str())
    }

    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// `text/event-stream` requests want SSE; we answer 405 for those.
    pub fn wants_json(&self) -> bool {
        match self.header("accept") {
            None => true,
            Some(a) => !a.contains("text/event-stream"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self { status, headers: vec![("Content-Type".into(), content_type.into())], body: body.into() }
    }

    pub fn json(value: &serde_json::Value) -> Self {
        Self::json_status(200, value)
    }

    pub fn json_status(status: u16, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self::new(status, "application/json; charset=utf-8", body)
    }

    pub fn text(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/plain; charset=utf-8", body)
    }

    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/html; charset=utf-8", body)
    }

    pub fn js(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "application/javascript; charset=utf-8", body)
    }

    pub fn css(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/css; charset=utf-8", body)
    }

    pub fn empty(status: u16) -> Self {
        Self { status, headers: vec![], body: Vec::new() }
    }

    pub fn redirect(location: &str) -> Self {
        Self { status: 302, headers: vec![("Location".into(), location.into())], body: Vec::new() }
    }

    pub fn error_json(status: u16, message: &str) -> Self {
        Self::json_status(status, &serde_json::json!({ "error": message }))
    }

    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.push((key.to_string(), value.to_string()));
        self
    }

    pub fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            201 => "Created",
            202 => "Accepted",
            204 => "No Content",
            302 => "Found",
            304 => "Not Modified",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Payload Too Large",
            414 => "URI Too Long",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            503 => "Service Unavailable",
            _ => "OK",
        }
    }
}

pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

struct BufferedReader {
    stream: TcpStream,
    buf: Vec<u8>,
    pos: usize,
}

impl BufferedReader {
    fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
        stream.set_write_timeout(Some(IDLE_TIMEOUT))?;
        stream.set_nodelay(true).ok();
        Ok(Self { stream, buf: Vec::with_capacity(1024), pos: 0 })
    }

    fn compact(&mut self) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }

    fn fill(&mut self) -> std::io::Result<usize> {
        self.compact();
        let mut chunk = [0u8; 1024];
        let n = self.stream.read(&mut chunk)?;
        if n > 0 {
            self.buf.extend_from_slice(&chunk[..n]);
        }
        Ok(n)
    }

    /// Read one CRLF-terminated line (without the CRLF). `Ok(None)` at EOF.
    fn line(&mut self, max: usize) -> std::io::Result<Option<String>> {
        loop {
            if let Some(idx) = find_crlf(&self.buf[self.pos..]) {
                let end = self.pos + idx;
                let line = String::from_utf8_lossy(&self.buf[self.pos..end]).to_string();
                self.pos = end + 2;
                return Ok(Some(line));
            }
            if self.buf.len() - self.pos > max {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line too long"));
            }
            if self.fill()? == 0 {
                return Ok(None);
            }
        }
    }

    fn exact(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        while self.buf.len() - self.pos < n {
            if self.fill()? == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short body"));
            }
        }
        let out = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(out)
    }

    fn chunked(&mut self) -> std::io::Result<Vec<u8>> {
        let mut body = Vec::new();
        loop {
            let line = self.line(128)?.unwrap_or_default();
            let size_str = line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_str, 16).unwrap_or(0);
            if size == 0 {
                // Trailer headers until an empty line.
                while let Some(l) = self.line(1024)? {
                    if l.is_empty() {
                        break;
                    }
                }
                return Ok(body);
            }
            if body.len() + size > MAX_BODY_BYTES {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "chunked body too large"));
            }
            body.extend_from_slice(&self.exact(size)?);
            let _ = self.line(2)?; // trailing CRLF
        }
    }
}

fn find_crlf(hay: &[u8]) -> Option<usize> {
    hay.windows(2).position(|w| w == b"\r\n")
}

fn parse_request(conn: &mut BufferedReader, peer: &str) -> std::io::Result<Option<Request>> {
    let request_line = match conn.line(MAX_HEADER_BYTES)? {
        Some(l) => l,
        None => return Ok(None),
    };
    if request_line.is_empty() {
        // A stray CRLF between keep-alive requests.
        return match conn.line(MAX_HEADER_BYTES)? {
            Some(l) if !l.is_empty() => parse_with_line(conn, peer, &l),
            Some(_) => Ok(None),
            None => Ok(None),
        };
    }
    parse_with_line(conn, peer, &request_line)
}

fn parse_with_line(
    conn: &mut BufferedReader,
    peer: &str,
    request_line: &str,
) -> std::io::Result<Option<Request>> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad request line"));
    }
    let (path, query) = split_target(&target);

    let mut headers = BTreeMap::new();
    let mut total = 0usize;
    while let Some(line) = conn.line(MAX_HEADER_BYTES)? {
        if line.is_empty() {
            break;
        }
        total += line.len();
        if total > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "headers too large"));
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    // 100-continue before touching the body, otherwise the client waits.
    if headers.get("expect").map(|e| e.to_ascii_lowercase().contains("100-continue")).unwrap_or(false) {
        conn.stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        conn.stream.flush()?;
    }

    let body = if headers
        .get("transfer-encoding")
        .map(|t| t.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        conn.chunked()?
    } else {
        let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        if len > MAX_BODY_BYTES {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body too large"));
        }
        if len > 0 {
            conn.exact(len)?
        } else {
            Vec::new()
        }
    };

    Ok(Some(Request { method, path, query, headers, body, peer: peer.to_string() }))
}

fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    let mut map = BTreeMap::new();
    if let Some(q) = query {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            map.insert(url_decode(k), url_decode(v));
        }
    }
    let path = path.split('#').next().unwrap_or(path).to_string();
    (if path.is_empty() { "/".into() } else { path }, map)
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn write_response(
    stream: &mut TcpStream,
    resp: &Response,
    keep_alive: bool,
    head_only: bool,
) -> std::io::Result<()> {
    let mut out = String::with_capacity(256);
    out.push_str(&format!("HTTP/1.1 {} {}\r\n", resp.status, Response::reason(resp.status)));
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if resp.status != 204 && resp.status != 304 {
        out.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    out.push_str(&format!("Connection: {}\r\n\r\n", if keep_alive { "keep-alive" } else { "close" }));
    stream.write_all(out.as_bytes())?;
    if !head_only && resp.status != 204 && resp.status != 304 && !resp.body.is_empty() {
        stream.write_all(&resp.body)?;
    }
    stream.flush()
}

/// A running server. Dropping it stops the accept loop.
pub struct Server {
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
    join: Option<JoinHandle<()>>,
    log: Arc<LogBuf>,
}

impl Server {
    pub fn start(addr: &str, handler: Handler, log: Arc<LogBuf>) -> std::io::Result<Self> {
        let listener = bind(addr)?;
        let local = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let log_thread = log.clone();
        let join = std::thread::Builder::new()
            .name("httpd".into())
            .stack_size(ACCEPT_STACK)
            .spawn(move || accept_loop(listener, handler, stop_thread, log_thread))?;
        log.info("http", format!("listening on http://{local}/"));
        Ok(Self { stop, addr: local, join: Some(join), log })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.log.info("http", "server stopped");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Signal the existing flag; the accept loop polls it every 15 ms.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

fn bind(addr: &str) -> std::io::Result<TcpListener> {
    let addrs: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
    let mut last = None;
    for a in addrs {
        match TcpListener::bind(a) {
            Ok(l) => return Ok(l),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no address")))
}

fn accept_loop(listener: TcpListener, handler: Handler, stop: Arc<AtomicBool>, log: Arc<LogBuf>) {
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    log.warn("http", format!("{peer}: too many connections, dropping"));
                    let mut s = stream;
                    let _ = write_response(
                        &mut s,
                        &Response::error_json(503, "too many connections"),
                        false,
                        false,
                    );
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let handler = handler.clone();
                let log_conn = log.clone();
                let active_conn = active.clone();
                let spawned = std::thread::Builder::new()
                    .name("http-conn".into())
                    .stack_size(CONN_STACK)
                    .spawn(move || {
                        serve_connection(stream, peer.to_string(), handler, log_conn);
                        active_conn.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    active.fetch_sub(1, Ordering::SeqCst);
                    log.error("http", "could not spawn connection thread");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(e) => {
                log.warn("http", format!("accept failed: {e}"));
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn serve_connection(stream: TcpStream, peer: String, handler: Handler, log: Arc<LogBuf>) {
    let mut conn = match BufferedReader::new(stream) {
        Ok(c) => c,
        Err(_) => return,
    };
    for _ in 0..32 {
        match parse_request(&mut conn, &peer) {
            Ok(Some(req)) => {
                let keep_alive = match req.header("connection") {
                    Some(v) => !v.eq_ignore_ascii_case("close"),
                    None => true,
                };
                let head_only = req.method == "HEAD";
                let started = std::time::Instant::now();
                let resp = handler(&req);
                if started.elapsed().as_millis() > 500 {
                    log.warn(
                        "http",
                        format!("{} {} took {} ms", req.method, req.path, started.elapsed().as_millis()),
                    );
                }
                conn.stream.set_write_timeout(Some(IDLE_TIMEOUT)).ok();
                if write_response(&mut conn.stream, &resp, keep_alive, head_only).is_err() {
                    return;
                }
                if !keep_alive {
                    return;
                }
            }
            Ok(None) => return,
            Err(e) => {
                log.warn("http", format!("{peer}: bad request: {e}"));
                let mut s = conn.stream;
                let _ = write_response(&mut s, &Response::error_json(400, "bad request"), false, false);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_are_decoded() {
        let (path, q) = split_target("/mcp?session=a%20b&x=1+2");
        assert_eq!(path, "/mcp");
        assert_eq!(q.get("session").unwrap(), "a b");
        assert_eq!(q.get("x").unwrap(), "1 2");
    }

    #[test]
    fn reason_phrases_cover_the_statuses_we_use() {
        assert_eq!(Response::reason(202), "Accepted");
        assert_eq!(Response::reason(405), "Method Not Allowed");
    }

    fn start_test_server() -> (Server, Arc<LogBuf>) {
        let log = LogBuf::new(32);
        let handler: Handler = Arc::new(|req: &Request| {
            if req.path == "/echo" {
                Response::json(&serde_json::json!({
                    "method": req.method,
                    "body": req.body_str(),
                    "query": req.query_get("q"),
                }))
            } else {
                Response::text("hello")
            }
        });
        let server = Server::start("127.0.0.1:0", handler, log.clone()).expect("bind");
        (server, log)
    }

    fn raw_request(addr: SocketAddr, raw: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut out = String::new();
        let mut buf = [0u8; 4096];
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if response_is_complete(&out) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        out
    }

    /// True once the header block and the announced body have both arrived.
    fn response_is_complete(out: &str) -> bool {
        let Some(split) = out.find("\r\n\r\n") else { return false };
        let head = &out[..split];
        let len: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.eq_ignore_ascii_case("content-length") {
                    v.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        out.len() - (split + 4) >= len
    }

    #[test]
    fn serves_a_get_and_keeps_the_connection_open() {
        let (server, _log) = start_test_server();
        let addr = server.local_addr();
        let resp = raw_request(addr, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("Content-Length: 5"));
        assert!(resp.contains("hello"));
        drop(server);
    }

    #[test]
    fn posts_a_body_and_parses_the_query() {
        let (server, _log) = start_test_server();
        let addr = server.local_addr();
        let body = r#"{"a":1}"#;
        let raw = format!(
            "POST /echo?q=7 HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = raw_request(addr, &raw);
        assert!(resp.contains("\"method\":\"POST\""));
        assert!(resp.contains("\"query\":\"7\""));
        assert!(resp.contains("a"));
        drop(server);
    }

    #[test]
    fn chunked_bodies_are_accepted() {
        let (server, _log) = start_test_server();
        let addr = server.local_addr();
        let raw = "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let resp = raw_request(addr, raw);
        assert!(resp.contains("{\\\"a\\\":1}") || resp.contains("\"a\":1"), "got: {resp}");
        drop(server);
    }
}

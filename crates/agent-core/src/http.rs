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
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub const MAX_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_CONNECTIONS: usize = 6;
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// Sockets accepted but not yet sending. A browser warming connections can hold several,
/// and they cost one descriptor each instead of a thread stack.
const MAX_PENDING: usize = 8;
/// How long a silent socket may sit in that list before it is closed.
const PENDING_TTL: Duration = Duration::from_secs(30);

/// How many sockets a long-lived event stream may hold.
///
/// A stream keeps one pool slot for its whole life, so this is a slice of
/// `MAX_CONNECTIONS` rather than a number of its own: the portal, MCP and
/// `/api/status` share those six sockets, and the board has to keep answering
/// while a console is open. Two streams is already a third of the pool.
pub const MAX_EVENT_STREAMS: usize = 2;

// The board answers HTTP from `MAX_CONNECTIONS` sockets and streams take a slice of
// that pool, never the pool itself: with the portal, MCP and `/api/status` sharing
// six sockets, two streams is already a third of them. Asserted at compile time so
// a smaller pool or a larger cap cannot pass review silently.
const _: () = assert!(MAX_EVENT_STREAMS * 2 < MAX_CONNECTIONS);

/// The comment heart-beat an event stream writes between events. It keeps a
/// proxy (and the socket write timeout) from declaring a quiet stream dead.
pub const SSE_PING: &str = ": ping\n\n";

/// One Server-Sent Events frame: `id:`, `event:`, `data:` and the blank line that
/// terminates it.
///
/// `data` must already be a single line (the JSON `Event::json()` renders is);
/// a raw newline inside a field would split the frame and confuse `EventSource`.
pub fn sse_frame(id: u64, event: &str, data: &str) -> String {
    debug_assert!(!event.contains('\n') && !data.contains('\n'), "an SSE field must be one line");
    format!("id: {id}\nevent: {event}\ndata: {data}\n\n")
}

/// Where a [`Response`] with a `stream` body writes its frames.
///
/// The HTTP layer implements this over the socket (chunked); a test implements it
/// over a `Vec`. Both see bytes, so what is tested is what the board sends.
pub trait EventSink {
    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()>;
}

/// The body of a streamed response: called once the headers are on the wire, and
/// allowed to keep the connection for as long as its own budget says.
pub type StreamBody = Box<dyn FnOnce(&mut dyn EventSink) -> std::io::Result<()> + Send>;

/// The cap on concurrent event streams, shared by everything that opens one.
///
/// A slot is taken before the socket commits to a long write, so a third client is
/// refused while its request is still cheap to answer with a 503.
#[derive(Clone, Default)]
pub struct EventStreamGate {
    taken: Arc<AtomicUsize>,
}

impl EventStreamGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many streams hold a slot right now.
    pub fn active(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }

    /// Take a slot, or `None` when [`MAX_EVENT_STREAMS`] are already open.
    pub fn acquire(&self) -> Option<EventStreamSlot> {
        let mut cur = self.taken.load(Ordering::SeqCst);
        loop {
            if cur >= MAX_EVENT_STREAMS {
                return None;
            }
            match self.taken.compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(EventStreamSlot { gate: self.clone() }),
                Err(now) => cur = now,
            }
        }
    }
}

/// One held stream slot. Dropping it gives the slot back, whatever ended the
/// stream — the idle budget, the total budget, a failed write or a dropped
/// response that never ran its body.
pub struct EventStreamSlot {
    gate: EventStreamGate,
}

impl Drop for EventStreamSlot {
    fn drop(&mut self) {
        self.gate.taken.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Thread stacks. The board has kilobytes to spare per task, the host has
/// megabytes and a debug build with deep serde_json frames — so the numbers
/// differ by target rather than by wishful thinking.
#[cfg(target_os = "espidf")]
// Requests are served on the accept thread — one stack instead of one per connection — so
// this stack carries the heaviest handler. `/api/tools` serialises 41 tool descriptors and
// overflowed a 6 kB stack with a `***ERROR*** A stack overflow in task pthread` and a
// reboot; 16 kB is what it takes with room to spare. The extra 4 kB is the TLS handshake
// (mbedTLS), which runs on this thread too when the board serves https — a second thread
// for it does not fit in internal RAM alongside the agent.
const ACCEPT_STACK: usize = 20 * 1024;
/// One request at a time may run per thread, and only this many threads at once: the stack
/// comes from internal SRAM, which is the scarcest thing on this board.
#[cfg(target_os = "espidf")]
const REQUEST_STACK: usize = 12 * 1024;
#[cfg(not(target_os = "espidf"))]
const REQUEST_STACK: usize = 128 * 1024;
const MAX_REQUEST_THREADS: usize = 3;
#[cfg(not(target_os = "espidf"))]
const ACCEPT_STACK: usize = 256 * 1024;

#[cfg(target_os = "espidf")]
// 10 kB per connection could not be paid for out of the ~40 kB the board has left once
// the radio is up, and six of them are what a browser opening the portal asks for. The
// handlers are a request parse plus a JSON body; the body itself is built on the heap.
const CONN_STACK: usize = 6 * 1024;
#[cfg(not(target_os = "espidf"))]
const CONN_STACK: usize = 512 * 1024;

/// How long a client may take to finish its handshake. Short, because the loop is
/// serialised: one silent socket must not hold every other client.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// `http` or `https` — set by the listener, not by the client. A self-describing
    /// board that answers over TLS must not tell an agent to use `http://`.
    pub scheme: &'static str,
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

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// A body that keeps writing after the headers are sent, for
    /// `text/event-stream`. A response either has `body` or `stream`; when a
    /// stream is present the connection is spent on it and closed afterwards.
    pub stream: Option<StreamBody>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .field("streaming", &self.stream.is_some())
            .finish()
    }
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.into(),
            stream: None,
        }
    }

    /// A response whose body is written by `body` once the headers are out.
    ///
    /// `Content-Type: text/event-stream` and `Cache-Control: no-cache` are the
    /// contract; the transfer framing is the HTTP layer's business.
    pub fn streamed(status: u16, content_type: &str, body: StreamBody) -> Self {
        Self {
            status,
            headers: vec![
                ("Content-Type".into(), content_type.into()),
                ("Cache-Control".into(), "no-cache".into()),
            ],
            body: Vec::new(),
            stream: Some(body),
        }
    }

    /// True when this response keeps the connection after its headers.
    pub fn is_stream(&self) -> bool {
        self.stream.is_some()
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
        // No caching: the console's HTML and its two assets change together in a firmware
        // build, and a browser that keeps the old script against the new document renders a
        // blank page. That looks exactly like "the site is down" and costs a flash cycle to
        // disprove, so the three files are always revalidated.
        Self::new(200, "text/html; charset=utf-8", body).with_header("Cache-Control", "no-store")
    }

    pub fn js(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "application/javascript; charset=utf-8", body).with_header("Cache-Control", "no-store")
    }

    pub fn css(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/css; charset=utf-8", body).with_header("Cache-Control", "no-store")
    }

    pub fn empty(status: u16) -> Self {
        Self { status, headers: vec![], body: Vec::new(), stream: None }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".into(), location.into())],
            body: Vec::new(),
            stream: None,
        }
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

/// Anything that can carry an HTTP connection.
///
/// The server does not care whether the bytes travel over a plain socket or a TLS session:
/// it reads a request, writes a response, and sets two timeouts. Keeping that in a trait is
/// what lets HTTPS be a second listener rather than a second server.
pub trait Stream: std::io::Read + std::io::Write + Send {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()>;
    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()>;
    fn set_nodelay(&self, on: bool) -> std::io::Result<()>;
}

impl Stream for TcpStream {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, d)
    }
    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_write_timeout(self, d)
    }
    fn set_nodelay(&self, on: bool) -> std::io::Result<()> {
        TcpStream::set_nodelay(self, on)
    }
}

impl Stream for Box<dyn Stream> {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        (**self).set_read_timeout(d)
    }
    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        (**self).set_write_timeout(d)
    }
    fn set_nodelay(&self, on: bool) -> std::io::Result<()> {
        (**self).set_nodelay(on)
    }
}

struct BufferedReader<S> {
    stream: S,
    buf: Vec<u8>,
    pos: usize,
}

impl<S: Stream> BufferedReader<S> {
    /// A reader that already holds the bytes read while the socket was still in the
    /// waiting list.
    fn with_prefix(stream: S, prefix: Vec<u8>) -> std::io::Result<Self> {
        stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
        stream.set_write_timeout(Some(IDLE_TIMEOUT))?;
        stream.set_nodelay(true).ok();
        Ok(Self { stream, buf: prefix, pos: 0 })
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
            body.reserve(size.min(MAX_BODY_BYTES));
            body.extend_from_slice(&self.exact(size)?);
            let _ = self.line(2)?; // trailing CRLF
        }
    }
}

fn find_crlf(hay: &[u8]) -> Option<usize> {
    hay.windows(2).position(|w| w == b"\r\n")
}

fn parse_request<S: Stream>(
    conn: &mut BufferedReader<S>,
    peer: &str,
    scheme: &'static str,
) -> std::io::Result<Option<Request>> {
    let request_line = match conn.line(MAX_HEADER_BYTES)? {
        Some(l) => l,
        None => return Ok(None),
    };
    if request_line.is_empty() {
        // A stray CRLF between keep-alive requests.
        return match conn.line(MAX_HEADER_BYTES)? {
            Some(l) if !l.is_empty() => parse_with_line(conn, peer, &l, scheme),
            Some(_) => Ok(None),
            None => Ok(None),
        };
    }
    parse_with_line(conn, peer, &request_line, scheme)
}

fn parse_with_line<S: Stream>(
    conn: &mut BufferedReader<S>,
    peer: &str,
    request_line: &str,
    scheme: &'static str,
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

    Ok(Some(Request { method, path, query, headers, body, peer: peer.to_string(), scheme }))
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

pub fn url_decode(s: &str) -> String {
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

fn write_response<S: std::io::Write>(
    stream: &mut S,
    resp: &Response,
    keep_alive: bool,
    head_only: bool,
) -> std::io::Result<()> {
    // A handler may pin the connection shut by sending its own `Connection: close`
    // (the event-stream refusal does: a refused stream must not park a pool socket).
    // Then that header is the one that goes out, exactly once.
    let explicit_close = resp
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"));
    let keep_alive = keep_alive && !explicit_close;
    let mut out = String::with_capacity(256);
    out.push_str(&format!("HTTP/1.1 {} {}\r\n", resp.status, Response::reason(resp.status)));
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if resp.status != 204 && resp.status != 304 {
        out.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    if !explicit_close {
        out.push_str(&format!("Connection: {}\r\n", if keep_alive { "keep-alive" } else { "close" }));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes())?;
    if !head_only && resp.status != 204 && resp.status != 304 && !resp.body.is_empty() {
        stream.write_all(&resp.body)?;
    }
    stream.flush()
}

/// A socket as an [`EventSink`]: every `send` becomes one chunk.
struct ChunkedSink<'a, S: std::io::Write> {
    stream: &'a mut S,
}

impl<S: std::io::Write> EventSink for ChunkedSink<'_, S> {
    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.stream.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
        self.stream.write_all(bytes)?;
        self.stream.write_all(b"\r\n")?;
        // Flush per frame: an event that sits in a buffer is an event the console
        // does not see, and this is the one route where latency is the product.
        self.stream.flush()
    }
}

/// Write a streamed response: headers, then whatever the body writes, then the
/// chunked terminator.
///
/// No `Content-Length` can exist ahead of time and the stream ends when it decides
/// to, so the body is chunked and the connection closed afterwards. `EventSource`
/// reconnects on its own with `after=<last id>`; that is the contract's design, not
/// a failure mode.
fn write_stream_response<S: std::io::Write>(
    stream: &mut S,
    resp: &Response,
    body: StreamBody,
    head_only: bool,
) -> std::io::Result<()> {
    let mut out = String::with_capacity(256);
    out.push_str(&format!("HTTP/1.1 {} {}\r\n", resp.status, Response::reason(resp.status)));
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Transfer-Encoding: chunked\r\n");
    // One stream per socket: the connection is spent, and saying so keeps a client
    // from trying to reuse it after the body ends.
    out.push_str("Connection: close\r\n\r\n");
    stream.write_all(out.as_bytes())?;
    stream.flush()?;
    if head_only {
        return Ok(());
    }
    let result = {
        let mut sink = ChunkedSink { stream };
        body(&mut sink)
    };
    // Terminate the body even when the stream reported an error: a clean end beats
    // a truncated one for a client that is about to reconnect anyway.
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
    result
}

/// A running server. Dropping it stops the accept loop.
/// What to do with a freshly accepted TCP connection on the TLS listener.
///
/// The platform owns the handshake: on the board it is esp-tls, on the host a rustls
/// server. `None` means the handshake failed and the socket is dropped — a stranger
/// speaking the wrong protocol must never reach the request parser.
pub type Handshake = Arc<dyn Fn(TcpStream) -> Option<Box<dyn Stream>> + Send + Sync>;

pub struct Server {
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
    scheme: &'static str,
    join: Option<JoinHandle<()>>,
    log: Arc<LogBuf>,
}

impl Server {
    /// Plain http only.
    pub fn start(addr: &str, handler: Handler, log: Arc<LogBuf>) -> std::io::Result<Self> {
        let http = bind(addr)?;
        let local = http.local_addr()?;
        http.set_nonblocking(true)?;
        Self::spawn(vec![http], handler, log, "http", local)
    }

    /// https only, on one port.
    ///
    /// The same accept thread and the same inline handshake as [`Server::start_with_tls`],
    /// with the plaintext socket simply absent. Used when `mcp.plain_http` is false; the
    /// board calls it only with a certificate in hand, because with none it would listen on
    /// nothing at all.
    pub fn start_tls_only(
        tls_addr: &str,
        handler: Handler,
        log: Arc<LogBuf>,
        handshake: Handshake,
    ) -> std::io::Result<Self> {
        let tls = bind(tls_addr)?;
        let local = tls.local_addr()?;
        tls.set_nonblocking(true)?;
        log.info("http", format!("listening on https://{}/ (http is off)", local));
        Self::spawn_with(vec![(tls, Some(handshake))], handler, log, "http", local)
    }

    /// http and https on two ports, served by **one** accept thread.
    ///
    /// That is not a shortcut, it is the only shape that fits. A TLS connection needs a
    /// stack before its handshake can start; six of those at once (what a browser asks
    /// for) do not fit in this board's internal RAM, and the failure arrives as
    /// ERR_CONNECTION_RESET. One listener thread that does the handshake inline costs a
    /// single stack and nothing per connection. The consequence is honest and bounded:
    /// while one TLS client is being served, another waits in the listen backlog for the
    /// duration of one request.
    pub fn start_with_tls(
        http_addr: &str,
        tls_addr: &str,
        handler: Handler,
        log: Arc<LogBuf>,
        handshake: Handshake,
    ) -> std::io::Result<Self> {
        let http = bind(http_addr)?;
        let local = http.local_addr()?;
        http.set_nonblocking(true)?;
        let tls = bind(tls_addr)?;
        tls.set_nonblocking(true)?;
        log.info("http", format!("listening on https://{}/", tls.local_addr()?));
        let mut listeners = vec![(http, None), (tls, Some(handshake))];
        // Keep the plaintext listener first: it is the one that must never starve.
        listeners.sort_by_key(|(_, tls)| tls.is_some());
        Self::spawn_with(listeners, handler, log, "http", local)
    }

    fn spawn(
        listeners: Vec<TcpListener>,
        handler: Handler,
        log: Arc<LogBuf>,
        scheme: &'static str,
        local: SocketAddr,
    ) -> std::io::Result<Self> {
        let listeners = listeners.into_iter().map(|l| (l, None)).collect();
        Self::spawn_with(listeners, handler, log, scheme, local)
    }

    fn spawn_with(
        listeners: Vec<(TcpListener, Option<Handshake>)>,
        handler: Handler,
        log: Arc<LogBuf>,
        scheme: &'static str,
        local: SocketAddr,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let log_thread = log.clone();
        let join = std::thread::Builder::new()
            .name("httpd".into())
            .stack_size(ACCEPT_STACK)
            .spawn(move || accept_loop(listeners, handler, stop_thread, log_thread))?;
        log.info("http", format!("listening on {scheme}://{local}/"));
        Ok(Self { stop, addr: local, scheme, join: Some(join), log })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("{}://{}", self.scheme, self.addr)
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

fn accept_loop(
    listeners: Vec<(TcpListener, Option<Handshake>)>,
    handler: Handler,
    stop: Arc<AtomicBool>,
    log: Arc<LogBuf>,
) {
    let active = Arc::new(AtomicUsize::new(0));
    // Requests are answered on this thread — one stack instead of one per connection — but
    // only once a socket actually has something to say. A browser opens idle sockets to
    // warm the connection (and keeps them alive between page loads); reading one of those
    // inline would park every other client for the whole read timeout, which on the bench
    // is indistinguishable from the board being dead. So accepted sockets wait in this
    // list until `peek` reports data.
    let mut pending: Vec<(TcpStream, String, std::time::Instant, Vec<u8>)> = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        for (listener, tls) in &listeners {
            loop {
                match listener.accept() {
                    Ok((stream, peer)) => match tls {
                        // A TLS socket cannot be probed for readiness without speaking
                        // TLS, so it is served here and now.
                        Some(handshake) => serve_tls(stream, peer, handshake, &handler, &log, &active),
                        None => {
                            if stream.set_nonblocking(true).is_err() {
                                continue;
                            }
                            if pending.len() >= MAX_PENDING {
                                log.warn("http", format!("{peer}: {MAX_PENDING} sockets waiting, dropping"));
                                continue;
                            }
                            pending.push((stream, peer.to_string(), std::time::Instant::now(), Vec::new()));
                        }
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log.warn("http", format!("accept failed: {e}"));
                        std::thread::sleep(Duration::from_millis(50));
                        break;
                    }
                }
            }
        }

        let now = std::time::Instant::now();
        let mut index = 0;
        while index < pending.len() {
            // Bytes left over from the previous request on this socket count as ready:
            // a pipelined request must not wait for more input that will never come.
            let state = if !pending[index].3.is_empty() {
                Ready::Data(std::mem::take(&mut pending[index].3))
            } else {
                // A non-blocking read is the readiness probe: `TcpStream::peek` is not
                // available on this platform's std, and treating its error as "closed"
                // dropped every connection the moment it was accepted. The bytes read here
                // are handed to the parser instead of being thrown away.
                let mut first = [0u8; 1024];
                match pending[index].0.read(&mut first) {
                    Ok(0) => Ready::Closed,
                    Ok(n) => Ready::Data(first[..n].to_vec()),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ready::Idle,
                    Err(_) => Ready::Closed,
                }
            };
            let expired = now.duration_since(pending[index].2) > PENDING_TTL;
            match state {
                Ready::Data(prefix) => {
                    let (stream, peer, _, _) = pending.remove(index);
                    stream.set_nonblocking(false).ok();
                    // A request is served on its own thread, never here. Serving it inline is
                    // what wedged the board: one slow handler - a model call, a 64 KB fetch -
                    // held the accept loop, and every other client got a connection reset
                    // with nothing in the log to say why. The board now answers or resets,
                    // but it always keeps accepting.
                    //
                    // The cost is the stack, so it is bounded twice: a request thread only
                    // starts while fewer than MAX_REQUEST_THREADS are running, and a refused
                    // spawn drops that one connection instead of falling back to inline.
                    if active.load(Ordering::SeqCst) >= MAX_REQUEST_THREADS {
                        log.warn(
                            "http",
                            format!("{peer}: {MAX_REQUEST_THREADS} requests in flight, dropping"),
                        );
                        index += 1;
                        continue;
                    }
                    let handler2 = handler.clone();
                    let log2 = log.clone();
                    let active2 = active.clone();
                    let peer2 = peer.clone();
                    let spawned = std::thread::Builder::new()
                        .name("http-req".into())
                        .stack_size(REQUEST_STACK)
                        .spawn(move || {
                            active2.fetch_add(1, Ordering::SeqCst);
                            match serve_until_stream(stream, prefix, &peer2, "http", true, &handler2, &log2) {
                                // Keep-alive ends with the thread: the browser reconnects, and
                                // a parked socket is not worth an unbounded table here.
                                Serve::KeepAlive(stream, rest) => {
                                    // Keep-alive ends with the thread: the socket is
                                    // closed rather than parked, so a browser reconnects and
                                    // the table stays bounded.
                                    drop((stream, rest));
                                }
                                Serve::Done | Serve::Failed => {}
                                Serve::Stream(conn, resp, body, head_only) => {
                                    spawn_stream(conn, resp, body, head_only, &log2, &active2);
                                }
                            }
                            active2.fetch_sub(1, Ordering::SeqCst);
                        });
                    if spawned.is_err() {
                        log.warn("http", format!("{peer}: no stack for a request thread, dropping"));
                    }
                    index += 1;
                    continue;
                }
                Ready::Closed => {
                    pending.remove(index);
                }
                Ready::Idle => {
                    if expired {
                        pending.remove(index);
                    } else {
                        index += 1;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Hand a streaming response to a thread of its own, so the listener can accept again.
fn spawn_stream<S: Stream + 'static>(
    mut conn: BufferedReader<S>,
    resp: Response,
    body: StreamBody,
    head_only: bool,
    log: &Arc<LogBuf>,
    active: &Arc<AtomicUsize>,
) {
    if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
        log.warn("http", "too many streams, dropping");
        return;
    }
    active.fetch_add(1, Ordering::SeqCst);
    let log_conn = log.clone();
    let active_conn = active.clone();
    let spawned =
        std::thread::Builder::new().name("http-stream".into()).stack_size(CONN_STACK).spawn(move || {
            let _ = write_stream_response(&mut conn.stream, &resp, body, head_only);
            drop(log_conn);
            active_conn.fetch_sub(1, Ordering::SeqCst);
        });
    if spawned.is_err() {
        active.fetch_sub(1, Ordering::SeqCst);
        log.error("http", "could not spawn stream thread");
    }
}

/// Serve one TLS connection on the accept thread: handshake, one request, close.
fn serve_tls(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    handshake: &Handshake,
    handler: &Handler,
    log: &Arc<LogBuf>,
    active: &Arc<AtomicUsize>,
) {
    // Blocking mode is what a handshake wants. The read timeout is short because this
    // thread is also the plaintext listener: a client that connects and says nothing must
    // not hold everybody else for the full idle timeout.
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(TLS_HANDSHAKE_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IDLE_TIMEOUT)).ok();
    stream.set_nodelay(true).ok();
    let conn = match handshake(stream) {
        Some(c) => c,
        None => {
            // A browser opens speculative TLS connections and drops the ones it does not
            // need; a phone on the LAN does the same when a page is refreshed. Logging
            // each one at warn fills the ring with the same line and buries the failures
            // that matter. Count them, say it the first few times, then once a minute.
            let n = TLS_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
            let now = (crate::app::now_ms() / 1000) as u32;
            let last = TLS_FAIL_AT_S.load(Ordering::Relaxed);
            if n <= 3 || now.saturating_sub(last) > 60 {
                TLS_FAIL_AT_S.store(now, Ordering::Relaxed);
                log.warn("http", format!("{peer}: tls handshake failed ({n} since boot)"));
            }
            return;
        }
    };
    let peer = peer.to_string();
    // No keep-alive: one request per TLS connection, because each one occupies the
    // listener while it lasts.
    match serve_until_stream(conn, Vec::new(), &peer, "https", false, handler, log) {
        Serve::Done | Serve::Failed | Serve::KeepAlive(..) => {}
        Serve::Stream(conn, resp, body, head_only) => {
            spawn_stream(conn, resp, body, head_only, log, active);
        }
    }
}

/// Handshake failures since boot, and when the last one was logged. A busy client can
/// produce dozens of them per page load; see `serve_tls`.
static TLS_FAILS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
// Seconds, not milliseconds: the xtensa target has no 64-bit atomics.
static TLS_FAIL_AT_S: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Whether an accepted socket has anything for us yet.
enum Ready {
    Data(Vec<u8>),
    Idle,
    Closed,
}

/// What serving one connection came to.
enum Serve<S> {
    /// Every request was answered; the socket is closed.
    Done,
    /// The socket died mid-request.
    Failed,
    /// The response streams, so the socket moves to a thread of its own.
    Stream(BufferedReader<S>, Response, StreamBody, bool),
    /// One request answered, the client may send another: hand the socket back to the
    /// waiting list instead of parking the accept thread in a second read. Chrome keeps
    /// six warm connections open and would otherwise hold the whole server.
    KeepAlive(S, Vec<u8>),
}

impl<S: Stream> BufferedReader<S> {
    /// The stream plus any bytes already read past the request that was just answered.
    fn into_parts(mut self) -> (S, Vec<u8>) {
        let rest = self.buf.split_off(self.pos.min(self.buf.len()));
        (self.stream, rest)
    }
}

/// Serve **one** request. The caller decides what happens to the socket afterwards.
fn serve_until_stream<S: Stream>(
    stream: S,
    prefix: Vec<u8>,
    peer: &str,
    scheme: &'static str,
    allow_keep_alive: bool,
    handler: &Handler,
    log: &Arc<LogBuf>,
) -> Serve<S> {
    let mut conn = match BufferedReader::with_prefix(stream, prefix) {
        Ok(c) => c,
        Err(_) => return Serve::Failed,
    };
    match parse_request(&mut conn, peer, scheme) {
        Ok(Some(req)) => {
            // A listener may refuse keep-alive outright. TLS does: its connections own
            // a stack each, and holding one open for an idle browser socket costs internal
            // RAM that the next handshake needs.
            let keep_alive = allow_keep_alive
                && match req.header("connection") {
                    Some(v) => !v.eq_ignore_ascii_case("close"),
                    None => true,
                };
            let head_only = req.method == "HEAD";
            let started = std::time::Instant::now();
            let mut resp = handler(&req);
            if started.elapsed().as_millis() > 500 {
                log.warn(
                    "http",
                    format!("{} {} took {} ms", req.method, req.path, started.elapsed().as_millis()),
                );
            }
            conn.stream.set_write_timeout(Some(IDLE_TIMEOUT)).ok();
            if let Some(body) = resp.stream.take() {
                // The handler has returned, so whatever lock it took is released:
                // the stream polls the app from here on, without holding it.
                return Serve::Stream(conn, resp, body, head_only);
            }
            if write_response(&mut conn.stream, &resp, keep_alive, head_only).is_err() {
                return Serve::Failed;
            }
            if !keep_alive {
                return Serve::Done;
            }
            let (stream, rest) = conn.into_parts();
            Serve::KeepAlive(stream, rest)
        }
        Ok(None) => Serve::Done,
        Err(e) => {
            log.warn("http", format!("{peer}: bad request: {e}"));
            let mut s = conn.stream;
            let _ = write_response(&mut s, &Response::error_json(400, "bad request"), false, false);
            Serve::Failed
        }
    }
}

#[allow(dead_code)]
fn serve_connection(stream: TcpStream, peer: String, handler: Handler, log: Arc<LogBuf>) {
    if let Serve::Stream(mut conn, resp, body, head_only) =
        serve_until_stream(stream, Vec::new(), &peer, "http", true, &handler, &log)
    {
        let _ = write_stream_response(&mut conn.stream, &resp, body, head_only);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The tests write raw requests; the library itself only needs `Read`.
    use std::io::Write;

    /// The portal's three files must not be cached: an HTML that changed and a script that
    /// did not is a blank page, which reads as "the site is down".
    #[test]
    fn the_portal_assets_are_never_cached() {
        for resp in [Response::html("x"), Response::js("x"), Response::css("x")] {
            let cache = resp.headers.iter().find(|(k, _)| k == "Cache-Control");
            assert_eq!(cache.map(|(_, v)| v.as_str()), Some("no-store"), "{resp:?}");
        }
    }

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

    /// A socket that connects and says nothing must not hold up anybody else.
    ///
    /// This is the failure that made the portal unreachable: the board accepted a warm-up
    /// connection from a browser, sat in a blocking read on it, and every later request
    /// timed out with no log line to explain it. The server now waits for the first byte
    /// before serving, so a silent socket is just a descriptor.
    #[test]
    fn an_idle_socket_does_not_block_a_real_request() {
        let (server, _log) = start_test_server();
        let addr = server.local_addr();

        let idle = TcpStream::connect(addr).expect("connect");
        // No bytes: this is what a browser's preconnect looks like.
        std::thread::sleep(Duration::from_millis(120));

        let started = std::time::Instant::now();
        let resp = raw_request(addr, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got {resp:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "an idle socket delayed a real request by {:?}",
            started.elapsed()
        );

        drop(idle);
        drop(server);
    }

    /// A keep-alive socket must not hold the server.
    ///
    /// Chrome keeps six warm connections open and reuses them; if the accept thread waits
    /// inside one for the next request, every other client times out — the portal "does not
    /// open" while it is being loaded. The connection goes back to the waiting list
    /// instead, and the next client is served in the meantime.
    #[test]
    fn a_kept_alive_socket_does_not_hold_the_server() {
        let (server, _log) = start_test_server();
        let addr = server.local_addr();

        let mut warm = TcpStream::connect(addr).expect("connect");
        warm.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        warm.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut first = [0u8; 128];
        let n = warm.read(&mut first).expect("first response");
        assert!(String::from_utf8_lossy(&first[..n]).starts_with("HTTP/1.1 200"));

        // The warm socket is now idle and stays open, exactly like a browser's.
        let started = std::time::Instant::now();
        let resp = raw_request(addr, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got {resp:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a kept-alive socket delayed another client by {:?}",
            started.elapsed()
        );

        drop(warm);
        drop(server);
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

    // ---- event streams ----

    /// One parsed `text/event-stream` frame.
    #[derive(Debug, PartialEq)]
    struct Frame {
        id: u64,
        event: String,
        data: String,
    }

    /// Parse the frames out of an event-stream body, the way `EventSource` does:
    /// `field: value` lines, a blank line ends the frame, a line starting with `:`
    /// is a comment and belongs to no frame.
    fn parse_frames(body: &str) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut id = 0u64;
        let mut event = String::new();
        let mut data = String::new();
        let mut open = false;
        for line in body.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                if open {
                    frames.push(Frame {
                        id,
                        event: std::mem::take(&mut event),
                        data: std::mem::take(&mut data),
                    });
                    open = false;
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // comment / heart-beat
            }
            let Some((field, value)) = line.split_once(": ") else { continue };
            match field {
                "id" => id = value.parse().unwrap_or(0),
                "event" => event = value.to_string(),
                "data" => data.push_str(value),
                _ => {}
            }
            open = true;
        }
        frames
    }

    fn test_frame(seq: u64) -> String {
        let data = serde_json::json!({ "seq": seq, "event": "tool_use", "tool_call_id": "c1" }).to_string();
        sse_frame(seq, "tool_use", &data)
    }

    #[test]
    fn an_sse_frame_parses_back_to_the_seq_kind_and_json_it_was_built_from() {
        let frame = test_frame(42);
        assert_eq!(
            frame,
            "id: 42\nevent: tool_use\ndata: {\"event\":\"tool_use\",\"seq\":42,\"tool_call_id\":\"c1\"}\n\n"
        );
        let frames = parse_frames(&format!("{}{}{}", test_frame(1), SSE_PING, test_frame(7)));
        assert_eq!(frames.len(), 2, "the ping is a comment, not a frame: {frames:?}");
        assert_eq!(frames[0].id, 1);
        assert_eq!(frames[0].event, "tool_use");
        assert_eq!(frames[1].id, 7);
        let data: serde_json::Value = serde_json::from_str(&frames[1].data).expect("data is JSON");
        assert_eq!(data["seq"], 7);
        assert_eq!(data["event"], "tool_use");
        assert_eq!(data["tool_call_id"], "c1");
    }

    #[test]
    fn event_stream_slots_are_capped_and_given_back_when_dropped() {
        let gate = EventStreamGate::new();
        let first = gate.acquire().expect("first slot");
        let second = gate.acquire().expect("second slot");
        assert_eq!(gate.active(), 2);
        assert!(gate.acquire().is_none(), "a third stream must be refused");
        drop(first);
        assert_eq!(gate.active(), 1);
        let again = gate.acquire().expect("the freed slot is reusable");
        assert_eq!(gate.active(), 2);
        drop((second, again));
        assert_eq!(gate.active(), 0, "every slot comes back");
    }

    /// A streamed response cannot know its length, so it is chunked, it says
    /// `Connection: close`, and the socket is released when the body returns.
    #[test]
    fn a_streamed_response_is_chunked_without_a_content_length() {
        let log = LogBuf::new(32);
        let handler: Handler = Arc::new(|req: &Request| {
            if req.path == "/stream" {
                let body: StreamBody = Box::new(|sink: &mut dyn EventSink| {
                    sink.send(test_frame(1).as_bytes())?;
                    sink.send(SSE_PING.as_bytes())?;
                    sink.send(test_frame(2).as_bytes())?;
                    Ok(())
                });
                Response::streamed(200, "text/event-stream", body)
            } else {
                Response::text("hello")
            }
        });
        let server = Server::start("127.0.0.1:0", handler, log).expect("bind");
        let addr = server.local_addr();
        let raw = raw_request_until_close(
            addr,
            "GET /stream?after=0 HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n",
        );
        assert!(raw.starts_with("HTTP/1.1 200 OK"), "got: {raw}");
        assert!(raw.contains("Content-Type: text/event-stream"));
        assert!(raw.contains("Cache-Control: no-cache"));
        assert!(raw.contains("Transfer-Encoding: chunked"));
        assert!(raw.contains("Connection: close"));
        assert!(!raw.contains("Content-Length"), "a stream has no length up front: {raw}");
        let frames = parse_frames(&dechunk(&raw));
        assert_eq!(frames.len(), 2, "both frames arrived: {raw}");
        assert_eq!((frames[0].id, frames[1].id), (1, 2));
        drop(server);
    }

    /// A HEAD on a stream sends the headers and closes without a body.
    #[test]
    fn a_head_on_a_stream_sends_headers_and_no_frames() {
        let log = LogBuf::new(32);
        let handler: Handler = Arc::new(|_req: &Request| {
            let body: StreamBody = Box::new(|sink: &mut dyn EventSink| sink.send(test_frame(1).as_bytes()));
            Response::streamed(200, "text/event-stream", body)
        });
        let server = Server::start("127.0.0.1:0", handler, log).expect("bind");
        let raw = raw_request_until_close(
            server.local_addr(),
            "HEAD /api/agent/stream HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert!(raw.starts_with("HTTP/1.1 200 OK"), "got: {raw}");
        assert!(!raw.contains("id: 1"), "no frames for HEAD: {raw}");
        drop(server);
    }

    /// Read until the server closes the connection - the only way a chunked body
    /// with no announced length ends.
    fn raw_request_until_close(addr: SocketAddr, raw: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// Undo `Transfer-Encoding: chunked`.
    fn dechunk(raw: &str) -> String {
        let Some(split) = raw.find("\r\n\r\n") else { return String::new() };
        let mut rest = &raw[split + 4..];
        let mut out = String::new();
        while let Some((line, tail)) = rest.split_once("\r\n") {
            let size = usize::from_str_radix(line.trim(), 16).unwrap_or(0);
            rest = tail;
            if size == 0 || rest.len() < size {
                break;
            }
            out.push_str(&rest[..size]);
            rest = rest.get(size + 2..).unwrap_or("");
        }
        out
    }
}

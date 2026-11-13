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
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub const MAX_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_CONNECTIONS: usize = 6;
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// At most four request/handshake/stream stacks, shared by both listeners.
/// Idle keep-alive sockets hold connection slots but never worker stacks.
const MAX_REQUEST_WORKERS: usize = 4;
// Reserve one plaintext control worker; ordinary/TLS/SSE work uses at most three.
const MAX_NORMAL_WORKERS: usize = MAX_REQUEST_WORKERS - 1;
const _: () = assert!(MAX_REQUEST_WORKERS <= MAX_CONNECTIONS);
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
// Keep the existing listener allocation until its reduced readiness-only workload
// has been measured on hardware. Parsing, handlers and TLS now run on workers.
const ACCEPT_STACK: usize = 16 * 1024;
#[cfg(not(target_os = "espidf"))]
const ACCEPT_STACK: usize = 256 * 1024;

#[cfg(target_os = "espidf")]
// A worker parses, runs the handler (or TLS handshake), and writes its response.
// Four stacks cost at most 64 KiB; hardware headroom must be measured on the board.
const CONN_STACK: usize = 16 * 1024;
#[cfg(not(target_os = "espidf"))]
const CONN_STACK: usize = 512 * 1024;

/// How long a client may take to finish its handshake without occupying a worker forever.
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
            415 => "Unsupported Media Type",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            503 => "Service Unavailable",
            _ => "OK",
        }
    }
}

pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

/// A request body read as it arrives, instead of being buffered into `Request::body`.
///
/// A buffered body is a `Vec` in RAM, and [`MAX_BODY_BYTES`] is what the board can pay for
/// a manifest or a config patch. An upload is not that: PCM audio at 24 kHz is 48 KB/s, a
/// minute of it is 2.9 MB, and the board's free internal RAM is counted in kilobytes
/// (`docs/audio.md`). A sink takes the bytes in arrival order — the speaker's own pacing
/// blocks the reader, so the socket is throttled by the hardware rather than by a buffer —
/// and answers when the body ends.
pub trait BodySink: Send {
    /// One piece of the body, in order. `Err` stops the upload and is answered as-is.
    fn push(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    /// The body ended. `complete` is false when the client hung up mid-body, which a
    /// sink must not report as success: half a song is not a song.
    fn finish(self: Box<Self>, complete: bool) -> Response;
}

/// What a route does with a body nobody has read yet.
pub enum BodyRoute {
    /// Not this route: buffer the body and call the request handler, as before.
    Buffer,
    /// Read it here, into this sink. The body never becomes a `Vec`, and the connection
    /// closes once the sink has answered.
    Sink(Box<dyn BodySink>),
    /// Refused before a byte of body was read — auth, method, content type. Answering here
    /// is what makes a refusal cost one header exchange instead of a whole song.
    Answer(Response),
}

/// Decides, from the request line and the headers alone, whether a route wants its body
/// streamed. It runs before the body is touched, which is the only moment the decision can
/// still be cheap.
pub type BodyRouter = Arc<dyn Fn(&Request) -> BodyRoute + Send + Sync>;

/// Everything a server answers with: the request handler, and the routes whose bodies are
/// consumed as they arrive.
///
/// One struct rather than a second constructor per listener: a server that streams uploads
/// must do so on http *and* https, and the alternative was a twin of every `start` method
/// with an extra argument.
pub struct Routes {
    pub request: Handler,
    /// `None` means no route streams its body, which is the right shape for a unit test
    /// that only exercises request/response.
    pub body: Option<BodyRouter>,
}

impl Routes {
    pub fn new(request: Handler) -> Self {
        Self { request, body: None }
    }

    pub fn with_body(mut self, body: BodyRouter) -> Self {
        self.body = Some(body);
        self
    }
}

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
                if idx > max {
                    return Err(invalid_data("line too long"));
                }
                let end = self.pos + idx;
                let line = String::from_utf8_lossy(&self.buf[self.pos..end]).to_string();
                self.pos = end + 2;
                return Ok(Some(line));
            }
            if self.buf.len() - self.pos > max {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line too long"));
            }
            if self.fill()? == 0 {
                if self.buf.len() != self.pos {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "unfinished line"));
                }
                return Ok(None);
            }
        }
    }

    fn required_line(&mut self, max: usize) -> std::io::Result<String> {
        self.line(max)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "incomplete HTTP framing"))
    }

    fn exact(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        // Only the destination holds the full body. Keeping it in `self.buf` and then
        // copying it doubled a large request's peak heap for no useful work.
        let mut out = Vec::new();
        out.try_reserve_exact(n).map_err(|_| invalid_data("not enough memory for the body"))?;
        self.stream_exact(n, &mut |bytes| {
            out.extend_from_slice(bytes);
            Ok(())
        })?;
        Ok(out)
    }

    fn chunk_size(&mut self) -> std::io::Result<usize> {
        let line = self.required_line(128)?;
        let size = line.split(';').next().unwrap_or("");
        if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid_data("bad chunk size"));
        }
        usize::from_str_radix(size, 16).map_err(|_| invalid_data("bad chunk size"))
    }

    fn chunk_end(&mut self) -> std::io::Result<()> {
        if self.exact(2)? != b"\r\n" {
            return Err(invalid_data("missing CRLF after chunk"));
        }
        Ok(())
    }

    fn trailers(&mut self) -> std::io::Result<()> {
        let mut total = 0;
        loop {
            let line = self.required_line(1024)?;
            total += line.len() + 2;
            if total > MAX_HEADER_BYTES {
                return Err(invalid_data("trailers too large"));
            }
            if line.is_empty() {
                return Ok(());
            }
        }
    }

    fn chunked(&mut self) -> std::io::Result<Vec<u8>> {
        let mut body = Vec::new();
        loop {
            let size = self.chunk_size()?;
            if size == 0 {
                self.trailers()?;
                return Ok(body);
            }
            if size > MAX_BODY_BYTES.saturating_sub(body.len()) {
                return Err(invalid_data("chunked body too large"));
            }
            let needed = body.len() + size;
            if body.capacity() < needed {
                let capacity = needed.max(body.capacity().saturating_mul(2)).min(MAX_BODY_BYTES);
                body.try_reserve_exact(capacity - body.len())
                    .map_err(|_| invalid_data("not enough memory for the body"))?;
            }
            self.stream_exact(size, &mut |bytes| {
                body.extend_from_slice(bytes);
                Ok(())
            })?;
            self.chunk_end()?;
        }
    }

    /// Send the `100 Continue` a client is waiting for before it will send its body.
    ///
    /// It belongs to the body, not to the head: a request refused on its headers (a bad
    /// content type, a missing PIN) must not invite the client to upload a song first.
    fn expect_continue(&mut self, headers: &BTreeMap<String, String>) -> std::io::Result<()> {
        let wants =
            headers.get("expect").map(|e| e.to_ascii_lowercase().contains("100-continue")).unwrap_or(false);
        if wants {
            self.stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            self.stream.flush()?;
        }
        Ok(())
    }

    /// The whole body, for the routes that want it in one piece — every route but the
    /// ones that name a [`BodySink`]. Bounded by [`MAX_BODY_BYTES`] however it is framed.
    fn read_body(&mut self, framing: &Framing) -> std::io::Result<Vec<u8>> {
        match framing {
            Framing::Empty => Ok(Vec::new()),
            Framing::Fixed(len) if *len > MAX_BODY_BYTES => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body too large"))
            }
            Framing::Fixed(len) => self.exact(*len),
            Framing::Chunked => self.chunked(),
        }
    }

    /// The body in arrival order, never buffered: one call per piece, each piece being
    /// what the socket had. The sink decides when there has been enough.
    fn stream_body(
        &mut self,
        framing: &Framing,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        match framing {
            Framing::Empty => Ok(()),
            Framing::Fixed(len) => self.stream_exact(*len, sink),
            Framing::Chunked => loop {
                let size = self.chunk_size()?;
                if size == 0 {
                    return self.trailers();
                }
                self.stream_exact(size, sink)?;
                self.chunk_end()?;
            },
        }
    }

    fn stream_exact(
        &mut self,
        mut left: usize,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        while left > 0 {
            if self.pos == self.buf.len() && self.fill()? == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short body"));
            }
            let n = (self.buf.len() - self.pos).min(left);
            sink(&self.buf[self.pos..self.pos + n])?;
            self.pos += n;
            left -= n;
        }
        Ok(())
    }
}

/// How a request's body is delimited, decided from the headers alone.
///
/// Separate from the body itself so a route can be asked about it before a byte of it has
/// been read — the whole point of [`BodySink`].
enum Framing {
    Empty,
    Fixed(usize),
    Chunked,
}

fn invalid_data(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn framing_of(headers: &BTreeMap<String, String>) -> std::io::Result<Framing> {
    if let Some(encoding) = headers.get("transfer-encoding") {
        if headers.contains_key("content-length") {
            return Err(invalid_data("ambiguous body framing"));
        }
        if !encoding.eq_ignore_ascii_case("chunked") {
            return Err(invalid_data("unsupported transfer encoding"));
        }
        return Ok(Framing::Chunked);
    }
    let Some(length) = headers.get("content-length") else { return Ok(Framing::Empty) };
    if length.is_empty() || !length.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid_data("invalid content length"));
    }
    match length.parse::<usize>() {
        Ok(0) => Ok(Framing::Empty),
        Ok(len) => Ok(Framing::Fixed(len)),
        Err(_) => Err(invalid_data("invalid content length")),
    }
}

fn find_crlf(hay: &[u8]) -> Option<usize> {
    hay.windows(2).position(|w| w == b"\r\n")
}

fn parse_request<S: Stream>(
    conn: &mut BufferedReader<S>,
    peer: &str,
    scheme: &'static str,
) -> std::io::Result<Option<(Request, Framing)>> {
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
) -> std::io::Result<Option<(Request, Framing)>> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method.is_empty()
        || !method.bytes().all(is_token_byte)
        || target.is_empty()
        || !matches!(version, "HTTP/1.1" | "HTTP/1.0")
        || parts.next().is_some()
    {
        return Err(invalid_data("bad request line"));
    }
    let (path, query) = split_target(target);

    let mut headers = BTreeMap::new();
    let mut total = request_line.len() + 2;
    loop {
        let line = conn.required_line(MAX_HEADER_BYTES)?;
        total += line.len() + 2;
        if total > MAX_HEADER_BYTES {
            return Err(invalid_data("headers too large"));
        }
        if line.is_empty() {
            break;
        }
        let (k, v) = line.split_once(':').ok_or_else(|| invalid_data("bad header"))?;
        if k.is_empty()
            || !k.bytes().all(is_token_byte)
            || v.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f)
        {
            return Err(invalid_data("bad header"));
        }
        let name = k.to_ascii_lowercase();
        if matches!(name.as_str(), "content-length" | "transfer-encoding") && headers.contains_key(&name) {
            return Err(invalid_data("duplicate body framing header"));
        }
        headers.insert(name, v.trim().to_string());
    }

    // 100-continue and the body are both left to the caller: it is the only place that
    // knows whether this route wants the bytes buffered or streamed (`BodyRoute`).
    let framing = framing_of(&headers)?;
    Ok(Some((
        Request { method, path, query, headers, body: Vec::new(), peer: peer.to_string(), scheme },
        framing,
    )))
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
    #[cfg(test)]
    budget: WorkerBudget,
}

impl Server {
    /// Plain http only.
    pub fn start(addr: &str, routes: Routes, log: Arc<LogBuf>) -> std::io::Result<Self> {
        let http = bind(addr)?;
        let local = http.local_addr()?;
        http.set_nonblocking(true)?;
        Self::spawn(vec![http], routes, log, "http", local)
    }

    /// https only, on one port.
    ///
    /// The same bounded workers as [`Server::start_with_tls`],
    /// with the plaintext socket simply absent. Used when `mcp.plain_http` is false; the
    /// board calls it only with a certificate in hand, because with none it would listen on
    /// nothing at all.
    pub fn start_tls_only(
        tls_addr: &str,
        routes: Routes,
        log: Arc<LogBuf>,
        handshake: Handshake,
    ) -> std::io::Result<Self> {
        let tls = bind(tls_addr)?;
        let local = tls.local_addr()?;
        tls.set_nonblocking(true)?;
        log.info("http", format!("listening on https://{}/ (http is off)", local));
        Self::spawn_with(vec![(tls, Some(handshake))], routes, log, "http", local)
    }

    /// http and https on two ports, served by **one** accept thread.
    ///
    /// Both listeners share the connection budget and bounded request workers. TLS
    /// handshakes and long handlers never execute on the accept thread. Saturated
    /// connections are closed rather than allocating an unbounded number of stacks.
    pub fn start_with_tls(
        http_addr: &str,
        tls_addr: &str,
        routes: Routes,
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
        Self::spawn_with(listeners, routes, log, "http", local)
    }

    fn spawn(
        listeners: Vec<TcpListener>,
        routes: Routes,
        log: Arc<LogBuf>,
        scheme: &'static str,
        local: SocketAddr,
    ) -> std::io::Result<Self> {
        let listeners = listeners.into_iter().map(|l| (l, None)).collect();
        Self::spawn_with(listeners, routes, log, scheme, local)
    }

    fn spawn_with(
        listeners: Vec<(TcpListener, Option<Handshake>)>,
        routes: Routes,
        log: Arc<LogBuf>,
        scheme: &'static str,
        local: SocketAddr,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let log_thread = log.clone();
        let budget = WorkerBudget::default();
        let budget_thread = budget.clone();
        let join = std::thread::Builder::new()
            .name("httpd".into())
            .stack_size(ACCEPT_STACK)
            .spawn(move || accept_loop(listeners, routes, stop_thread, log_thread, budget_thread))?;
        log.info("http", format!("listening on {scheme}://{local}/"));
        Ok(Self {
            stop,
            addr: local,
            scheme,
            join: Some(join),
            log,
            #[cfg(test)]
            budget,
        })
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

/// A count follows its owner through spawn failure, panic and socket return.
struct Permit(Arc<AtomicUsize>);
impl Permit {
    fn take(counter: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        let mut count = counter.load(Ordering::SeqCst);
        while count < limit {
            match counter.compare_exchange_weak(count, count + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Self(counter.clone())),
                Err(actual) => count = actual,
            }
        }
        None
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Clone, Default)]
struct WorkerBudget {
    connections: Arc<AtomicUsize>,
    workers: Arc<AtomicUsize>,
    normal_workers: Arc<AtomicUsize>,
    control_workers: Arc<AtomicUsize>,
    #[cfg(test)]
    fail_spawn: Arc<AtomicBool>,
}

impl WorkerBudget {
    fn spawn(&self, body: impl FnOnce() + Send + 'static) -> std::io::Result<JoinHandle<()>> {
        #[cfg(test)]
        if self.fail_spawn.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other("injected request spawn failure"));
        }
        std::thread::Builder::new().name("http-request".into()).stack_size(CONN_STACK).spawn(body)
    }
}
struct PendingConnection {
    stream: TcpStream,
    peer: String,
    idle_since: std::time::Instant,
    prefix: Vec<u8>,
    _connection: Permit,
}

fn accept_loop(
    listeners: Vec<(TcpListener, Option<Handshake>)>,
    routes: Routes,
    stop: Arc<AtomicBool>,
    log: Arc<LogBuf>,
    budget: WorkerBudget,
) {
    // One bounded return channel per listener generation; old workers cannot
    // return sockets to a new server after this receiver has been dropped.
    let (returned, returns) = sync_channel(MAX_CONNECTIONS);
    let mut pending: Vec<PendingConnection> = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        while let Ok(conn) = returns.try_recv() {
            pending.push(conn);
        }
        for (listener, tls) in &listeners {
            // Bound each pass so a flood cannot starve the other listener.
            for _ in 0..MAX_CONNECTIONS {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        let Some(connection) = Permit::take(&budget.connections, MAX_CONNECTIONS) else {
                            continue; // Saturated: close without parsing or blocking writes.
                        };
                        let conn = PendingConnection {
                            stream,
                            peer: peer.to_string(),
                            idle_since: std::time::Instant::now(),
                            prefix: Vec::new(),
                            _connection: connection,
                        };
                        if let Some(handshake) = tls {
                            let _ = dispatch(
                                conn,
                                Some(handshake.clone()),
                                &routes,
                                &log,
                                &stop,
                                &budget,
                                &returned,
                            );
                        } else if conn.stream.set_nonblocking(true).is_ok() {
                            pending.push(conn);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log.warn("http", format!("accept failed: {e}"));
                        break;
                    }
                }
            }
        }
        let now = std::time::Instant::now();
        let mut index = 0;
        while index < pending.len() {
            if now.duration_since(pending[index].idle_since) > PENDING_TTL {
                pending.swap_remove(index);
                continue;
            }
            // Pipelined bytes count as ready even when the socket is now idle.
            let state = probe_request_line(&mut pending[index]);
            match state {
                Ready::Data(prefix) => {
                    let mut conn = pending.swap_remove(index);
                    conn.prefix = prefix;
                    if let Some(conn) = dispatch(conn, None, &routes, &log, &stop, &budget, &returned) {
                        // A bounded pending socket waits for a worker instead of losing
                        // a normal browser burst. Leave capacity for control workers.
                        pending.push(conn);
                        index += 1;
                    }
                }
                Ready::Closed => {
                    pending.swap_remove(index);
                }
                Ready::Idle => {
                    if now.duration_since(pending[index].idle_since) > PENDING_TTL {
                        pending.swap_remove(index);
                    } else {
                        index += 1;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Pending sockets/returns close here. Busy workers finish their existing
    // request; the stop flag prevents their next keep-alive handoff.
}

// Classification only reserves capacity: the complete request still goes through
// the normal parser, body limits and authenticated handler, never a bypass.
fn is_control_prefix(prefix: &[u8]) -> bool {
    let Some(line) = prefix.split(|b| *b == b'\n').next() else {
        return false;
    };
    let Ok(line) = std::str::from_utf8(line) else {
        return false;
    };
    let mut words = line.split_ascii_whitespace();
    let method = words.next().unwrap_or("");
    let path = words.next().unwrap_or("").split('?').next().unwrap_or("");
    matches!((method, path), ("GET", "/api/status" | "/healthz") | ("POST", "/api/agent/cancel"))
}

fn probe_request_line(conn: &mut PendingConnection) -> Ready {
    if conn.prefix.contains(&b'\n') {
        return Ready::Data(std::mem::take(&mut conn.prefix));
    }
    if conn.prefix.len() >= MAX_HEADER_BYTES {
        return Ready::Closed;
    }
    let mut first = [0u8; 1024];
    let cap = first.len().min(MAX_HEADER_BYTES - conn.prefix.len());
    match conn.stream.read(&mut first[..cap]) {
        Ok(0) => Ready::Closed,
        Ok(n) => {
            conn.prefix.extend_from_slice(&first[..n]);
            if conn.prefix.contains(&b'\n') {
                Ready::Data(std::mem::take(&mut conn.prefix))
            } else {
                Ready::Idle
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ready::Idle,
        Err(_) => Ready::Closed,
    }
}

fn dispatch(
    conn: PendingConnection,
    handshake: Option<Handshake>,
    routes: &Routes,
    log: &Arc<LogBuf>,
    stop: &Arc<AtomicBool>,
    budget: &WorkerBudget,
    returned: &SyncSender<PendingConnection>,
) -> Option<PendingConnection> {
    let control = handshake.is_none() && is_control_prefix(&conn.prefix);
    let lane = if control {
        Permit::take(&budget.control_workers, 1)
    } else {
        Permit::take(&budget.normal_workers, MAX_NORMAL_WORKERS)
    };
    let Some(lane) = lane else {
        return Some(conn);
    };
    let Some(worker) = Permit::take(&budget.workers, MAX_REQUEST_WORKERS) else {
        return Some(conn); // Keep within the bounded pending socket budget.
    };
    let routes = Routes { request: routes.request.clone(), body: routes.body.clone() };
    let log_worker = log.clone();
    let stop = stop.clone();
    let returned = returned.clone();
    let spawned = budget.spawn(move || {
        let _worker = worker;
        let _lane = lane;
        if stop.load(Ordering::SeqCst) {
            return;
        }
        conn.stream.set_nonblocking(false).ok();
        if let Some(handshake) = handshake {
            serve_tls(conn, &handshake, &routes, &log_worker, &stop);
            return;
        }
        let PendingConnection { stream, peer, prefix, _connection, .. } = conn;
        match serve_until_stream(stream, prefix, &peer, "http", true, &routes, &log_worker) {
            Serve::Done | Serve::Failed => {}
            Serve::KeepAlive(stream, prefix) => {
                if !stop.load(Ordering::SeqCst) && stream.set_nonblocking(true).is_ok() {
                    let conn = PendingConnection {
                        stream,
                        peer,
                        prefix,
                        idle_since: std::time::Instant::now(),
                        _connection,
                    };
                    // Release the worker before advertising a ready pipeline.
                    // try_send never blocks shutdown and drops failed returns.
                    drop(_worker);
                    drop(_lane);
                    let _ = returned.try_send(conn);
                }
            }
            Serve::Stream(mut conn, resp, body, head_only) => {
                // Reuse this worker and both permits for the whole stream.
                if !stop.load(Ordering::SeqCst) {
                    let _ = write_stream_response(&mut conn.stream, &resp, body, head_only);
                }
            }
        }
    });
    // std drops the closure (socket and both permits) on spawn failure.
    // Unwinding a handler/handshake/stream also drops those RAII guards.
    if spawned.is_err() {
        log.error("http", "could not spawn request thread");
    }
    None
}

/// TLS handshake/request run on one bounded worker, never on the accept thread.
fn serve_tls(
    conn: PendingConnection,
    handshake: &Handshake,
    routes: &Routes,
    log: &Arc<LogBuf>,
    stop: &AtomicBool,
) {
    let PendingConnection { stream, peer, _connection, .. } = conn;
    stream.set_read_timeout(Some(TLS_HANDSHAKE_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IDLE_TIMEOUT)).ok();
    stream.set_nodelay(true).ok();
    let conn = match handshake(stream) {
        Some(c) => c,
        None => {
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
    if stop.load(Ordering::SeqCst) {
        return;
    }
    // Preserve TLS's close-after-one-request policy: encrypted idle sockets
    // cannot use the plaintext readiness probe.
    if let Serve::Stream(mut conn, resp, body, head_only) =
        serve_until_stream(conn, Vec::new(), &peer, "https", false, routes, log)
    {
        if !stop.load(Ordering::SeqCst) {
            let _ = write_stream_response(&mut conn.stream, &resp, body, head_only);
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
    /// The response streams; the owning request worker writes it on its own stack.
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
    routes: &Routes,
    log: &Arc<LogBuf>,
) -> Serve<S> {
    let mut conn = match BufferedReader::with_prefix(stream, prefix) {
        Ok(c) => c,
        Err(_) => return Serve::Failed,
    };
    match parse_request(&mut conn, peer, scheme) {
        Ok(Some((mut req, framing))) => {
            // Asked before a byte of body is read: a route that streams its body cannot
            // be served after the body has been buffered, and a refusal costs one header
            // exchange instead of a whole song.
            if let Some(router) = routes.body.as_ref() {
                match router(&req) {
                    BodyRoute::Sink(mut sink) => {
                        let read = match conn.expect_continue(&req.headers) {
                            Ok(()) => conn.stream_body(&framing, &mut |piece| sink.push(piece)),
                            Err(e) => Err(e),
                        };
                        if let Err(e) = &read {
                            // The client hung up, or the sink refused the bytes (a cap, a
                            // dead amplifier). Either way the upload stopped early, and the
                            // answer says so — the sink decides the status.
                            log.warn("http", format!("{} {} upload stopped: {e}", req.method, req.path));
                        }
                        let resp = sink.finish(read.is_ok());
                        conn.stream.set_write_timeout(Some(IDLE_TIMEOUT)).ok();
                        let _ = write_response(&mut conn.stream, &resp, false, false);
                        // Nothing follows a body that was consumed here: the request is
                        // spent, and a client that wanted keep-alive can reconnect.
                        return Serve::Done;
                    }
                    BodyRoute::Answer(resp) => {
                        conn.stream.set_write_timeout(Some(IDLE_TIMEOUT)).ok();
                        let _ = write_response(&mut conn.stream, &resp, false, false);
                        return Serve::Done;
                    }
                    BodyRoute::Buffer => {}
                }
            }
            if let Err(e) = conn.expect_continue(&req.headers) {
                log.warn("http", format!("{peer}: could not answer 100-continue: {e}"));
                return Serve::Failed;
            }
            match conn.read_body(&framing) {
                Ok(body) => req.body = body,
                Err(e) => {
                    log.warn("http", format!("{peer}: bad body: {e}"));
                    let mut s = conn.stream;
                    let _ = write_response(&mut s, &Response::error_json(400, "bad request"), false, false);
                    return Serve::Failed;
                }
            }
            // A listener may refuse keep-alive outright. TLS retains its existing
            // close-after-one-request policy; plain sockets return to readiness polling.
            let keep_alive = allow_keep_alive
                && match req.header("connection") {
                    Some(v) => !v.eq_ignore_ascii_case("close"),
                    None => true,
                };
            let head_only = req.method == "HEAD";
            let started = std::time::Instant::now();
            let mut resp = (routes.request)(&req);
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
fn serve_connection(stream: TcpStream, peer: String, routes: Routes, log: Arc<LogBuf>) {
    if let Serve::Stream(mut conn, resp, body, head_only) =
        serve_until_stream(stream, Vec::new(), &peer, "http", true, &routes, &log)
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
        let server = Server::start("127.0.0.1:0", Routes::new(handler), log.clone()).expect("bind");
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

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(std::time::Instant::now() < deadline, "condition did not become true");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct ReleaseWorkers(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);

    impl Drop for ReleaseWorkers {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }

    #[test]
    fn a_slow_handler_does_not_block_another_response() {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release = ReleaseWorkers(gate.clone());
        let (entered, entry) = std::sync::mpsc::channel();
        let handler: Handler = Arc::new(move |req| {
            if req.path == "/slow" {
                entered.send(()).unwrap();
                let mut ready = gate.0.lock().unwrap();
                while !*ready {
                    ready = gate.1.wait(ready).unwrap();
                }
            }
            Response::text("done")
        });
        let server = Server::start("127.0.0.1:0", Routes::new(handler), LogBuf::new(16)).unwrap();
        let addr = server.local_addr();
        let slow = std::thread::spawn(move || {
            raw_request(addr, "GET /slow HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        });
        entry.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = std::time::Instant::now();
        let fast = raw_request(addr, "GET /fast HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        drop(release);
        assert!(fast.contains("done"), "{fast:?}");
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(slow.join().unwrap().contains("done"));
        wait_until(|| server.budget.workers.load(Ordering::SeqCst) == 0);
    }

    #[test]
    fn request_pool_is_bounded_and_recovers_after_saturation() {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release = ReleaseWorkers(gate.clone());
        let (entered, entry) = std::sync::mpsc::channel();
        let handler: Handler = Arc::new(move |req| {
            if req.path == "/api/status" {
                return Response::text("control-ready");
            }
            entered.send(()).unwrap();
            let mut ready = gate.0.lock().unwrap();
            while !*ready {
                ready = gate.1.wait(ready).unwrap();
            }
            Response::text("done")
        });
        let server = Server::start("127.0.0.1:0", Routes::new(handler), LogBuf::new(16)).unwrap();
        let mut clients = Vec::new();
        for _ in 0..MAX_NORMAL_WORKERS {
            let mut client = TcpStream::connect(server.local_addr()).unwrap();
            client.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
            clients.push(client);
            entry.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        assert_eq!(server.budget.workers.load(Ordering::SeqCst), MAX_NORMAL_WORKERS);
        // Even a fragmented control request line must retain its reserved lane.
        let start = std::time::Instant::now();
        let mut control = TcpStream::connect(server.local_addr()).unwrap();
        control.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        control.write_all(b"GET /api/sta").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        control.write_all(b"tus HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut response = String::new();
        control.read_to_string(&mut response).unwrap();
        assert!(response.contains("control-ready"), "{response}");
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(control);
        let mut queued = TcpStream::connect(server.local_addr()).unwrap();
        queued.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        queued.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut byte = [0u8; 1];
        let error = queued.read(&mut byte).unwrap_err();
        assert!(matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
        assert!(entry.try_recv().is_err(), "a fourth ordinary handler ran");
        drop(release);
        drop(clients);
        queued.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut queued_response = String::new();
        queued.read_to_string(&mut queued_response).unwrap();
        assert!(queued_response.contains("done"), "queued burst was lost");
        drop(queued);
        wait_until(|| {
            server.budget.workers.load(Ordering::SeqCst) == 0
                && server.budget.connections.load(Ordering::SeqCst) == 0
        });
        let recovered =
            raw_request(server.local_addr(), "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(recovered.contains("done"));
    }

    #[test]
    fn panic_and_spawn_failure_release_both_budgets() {
        let handler: Handler = Arc::new(|req| {
            assert_ne!(req.path, "/panic", "injected handler panic");
            Response::text("recovered")
        });
        let server = Server::start("127.0.0.1:0", Routes::new(handler), LogBuf::new(16)).unwrap();
        let panic_response =
            raw_request(server.local_addr(), "GET /panic HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(panic_response.is_empty());
        wait_until(|| {
            server.budget.workers.load(Ordering::SeqCst) == 0
                && server.budget.connections.load(Ordering::SeqCst) == 0
        });
        server.budget.fail_spawn.store(true, Ordering::SeqCst);
        let failed =
            raw_request(server.local_addr(), "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(failed.is_empty());
        wait_until(|| {
            !server.budget.fail_spawn.load(Ordering::SeqCst)
                && server.budget.workers.load(Ordering::SeqCst) == 0
                && server.budget.connections.load(Ordering::SeqCst) == 0
        });
        assert!(raw_request(server.local_addr(), "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .contains("recovered"));
    }

    #[test]
    fn a_pipeline_returns_to_readiness_without_reordering_or_extra_workers() {
        let (server, _) = start_test_server();
        let mut client = TcpStream::connect(server.local_addr()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client
            .write_all(
                b"GET / HTTP/1.1\r\nHost: x\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let mut result = String::new();
        client.read_to_string(&mut result).unwrap();
        assert_eq!(result.matches("HTTP/1.1 200 OK").count(), 2, "{result}");
        assert_eq!(result.matches("hello").count(), 2);
        wait_until(|| {
            server.budget.workers.load(Ordering::SeqCst) == 0
                && server.budget.connections.load(Ordering::SeqCst) == 0
        });
    }

    #[test]
    fn streaming_reuses_the_request_worker_and_releases_its_connection() {
        let handler: Handler = Arc::new(|_| {
            let request_thread = std::thread::current().id();
            Response::streamed(
                200,
                "text/event-stream",
                Box::new(move |sink| {
                    assert_eq!(
                        std::thread::current().id(),
                        request_thread,
                        "streaming must not spawn another thread for this socket"
                    );
                    sink.send(b"data: same worker\n\n")
                }),
            )
        });
        let server = Server::start("127.0.0.1:0", Routes::new(handler), LogBuf::new(16)).unwrap();
        let response = raw_request_until_close(
            server.local_addr(),
            "GET /stream HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        assert!(response.contains("data: same worker"), "{response:?}");
        wait_until(|| {
            server.budget.workers.load(Ordering::SeqCst) == 0
                && server.budget.connections.load(Ordering::SeqCst) == 0
        });
    }

    #[test]
    fn idle_sockets_share_the_connection_cap_without_taking_workers() {
        let (server, _) = start_test_server();
        let idle: Vec<_> =
            (0..MAX_CONNECTIONS).map(|_| TcpStream::connect(server.local_addr()).unwrap()).collect();
        wait_until(|| server.budget.connections.load(Ordering::SeqCst) == MAX_CONNECTIONS);
        assert_eq!(server.budget.workers.load(Ordering::SeqCst), 0);
        assert!(raw_request(server.local_addr(), "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .is_empty());
        drop(idle);
        wait_until(|| server.budget.connections.load(Ordering::SeqCst) == 0);
        assert!(raw_request(server.local_addr(), "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .contains("hello"));
    }

    #[test]
    fn stop_discards_a_busy_workers_keepalive_pipeline() {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release = ReleaseWorkers(gate.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let (entered, entry) = std::sync::mpsc::channel();
        let handler: Handler = Arc::new(move |_| {
            handler_calls.fetch_add(1, Ordering::SeqCst);
            entered.send(()).unwrap();
            let mut ready = gate.0.lock().unwrap();
            while !*ready {
                ready = gate.1.wait(ready).unwrap();
            }
            Response::text("old")
        });
        let mut server = Server::start("127.0.0.1:0", Routes::new(handler), LogBuf::new(16)).unwrap();
        let mut client = TcpStream::connect(server.local_addr()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        entry.recv_timeout(Duration::from_secs(3)).unwrap();
        server.stop();
        drop(release);
        let mut result = String::new();
        let _ = client.read_to_string(&mut result);
        wait_until(|| {
            server.budget.workers.load(Ordering::SeqCst) == 0
                && server.budget.connections.load(Ordering::SeqCst) == 0
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stalled_tls_handshake_does_not_block_plain_http_and_failure_recovers() {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release = ReleaseWorkers(gate.clone());
        let (entered, entry) = std::sync::mpsc::channel();
        let handshake: Handshake = Arc::new(move |_| {
            entered.send(()).unwrap();
            let mut ready = gate.0.lock().unwrap();
            while !*ready {
                ready = gate.1.wait(ready).unwrap();
            }
            None
        });
        let http = bind("127.0.0.1:0").unwrap();
        let tls = bind("127.0.0.1:0").unwrap();
        let local = http.local_addr().unwrap();
        let tls_addr = tls.local_addr().unwrap();
        http.set_nonblocking(true).unwrap();
        tls.set_nonblocking(true).unwrap();
        let handler: Handler = Arc::new(|_| Response::text("plain"));
        let server = Server::spawn_with(
            vec![(http, None), (tls, Some(handshake))],
            Routes::new(handler),
            LogBuf::new(16),
            "http",
            local,
        )
        .unwrap();
        let tls_client = TcpStream::connect(tls_addr).unwrap();
        entry.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = std::time::Instant::now();
        let plain = raw_request(local, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        drop(release);
        assert!(plain.contains("plain"));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(tls_client);
        wait_until(|| {
            server.budget.workers.load(Ordering::SeqCst) == 0
                && server.budget.connections.load(Ordering::SeqCst) == 0
        });
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

    struct Fixture {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Fixture {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for Fixture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Stream for Fixture {
        fn set_read_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }
        fn set_write_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }
        fn set_nodelay(&self, _: bool) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn fixture(raw: &str) -> BufferedReader<Fixture> {
        BufferedReader::with_prefix(
            Fixture { input: std::io::Cursor::new(raw.as_bytes().to_vec()), output: Vec::new() },
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn ambiguous_or_incomplete_request_headers_are_refused() {
        for raw in [
            "POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: notchunked\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 0\r\n",
            "POST / HTTP/1.1\r\nHeader Without Colon\r\n\r\n",
            "POST /\r\n\r\n",
        ] {
            assert!(parse_request(&mut fixture(raw), "test", "http").is_err(), "accepted {raw:?}");
        }
    }

    #[test]
    fn malformed_chunks_never_complete_buffered_or_streamed_uploads() {
        for body in
            ["not-hex\r\n\r\n", "1\r\nxXX0\r\n\r\n", "1\r\nx\r\n0\r\n", "1\r\nx\r\n", "ffffffffffffffff\r\n"]
        {
            assert!(fixture(body).read_body(&Framing::Chunked).is_err(), "buffer accepted {body:?}");
            assert!(
                fixture(body).stream_body(&Framing::Chunked, &mut |_| Ok(())).is_err(),
                "stream accepted {body:?}"
            );
        }
        let body = format!("0\r\nX-Trailer: {}\r\n\r\n", "x".repeat(MAX_HEADER_BYTES));
        assert!(fixture(&body).read_body(&Framing::Chunked).is_err());
        assert!(fixture(&body).stream_body(&Framing::Chunked, &mut |_| Ok(())).is_err());
    }

    #[test]
    fn a_buffered_body_keeps_only_one_full_copy() {
        let body = "x".repeat(MAX_BODY_BYTES);
        for (raw, framing) in [
            (body.clone(), Framing::Fixed(body.len())),
            (format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len()), Framing::Chunked),
        ] {
            let mut reader = fixture(&raw);
            let received = reader.read_body(&framing).unwrap();
            assert_eq!(received, body.as_bytes());
            assert!(reader.buf.len() <= 1024, "{} bytes of a duplicate body", reader.buf.len());
            assert!(received.capacity() <= MAX_BODY_BYTES);
        }
    }

    #[test]
    fn a_complete_line_still_respects_its_limit() {
        assert!(fixture("12345\r\n").line(4).is_err());
        assert!(fixture("partial").line(16).is_err());
        assert_eq!(fixture("1234\r\n").line(4).unwrap().as_deref(), Some("1234"));
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
        let server = Server::start("127.0.0.1:0", Routes::new(handler), log).expect("bind");
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
        let server = Server::start("127.0.0.1:0", Routes::new(handler), log).expect("bind");
        let raw = raw_request_until_close(
            server.local_addr(),
            "HEAD /api/agent/stream HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert!(raw.starts_with("HTTP/1.1 200 OK"), "got: {raw}");
        assert!(!raw.contains("id: 1"), "no frames for HEAD: {raw}");
        drop(server);
    }

    /// A body route is asked before a byte is read, and the bytes never become a `Vec`:
    /// that is the difference between buffering a song and playing it. The payload here is
    /// over `MAX_BODY_BYTES`, which the buffered path refuses outright.
    #[test]
    fn a_body_route_streams_past_the_buffer_cap() {
        let log = LogBuf::new(64);
        let handler: Handler = Arc::new(|_req: &Request| Response::text("buffered"));

        struct Counting(usize);
        impl BodySink for Counting {
            fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
                assert!(bytes.len() <= 1024, "a piece is one socket read, not the whole body");
                self.0 += bytes.len();
                Ok(())
            }
            fn finish(self: Box<Self>, complete: bool) -> Response {
                Response::json(&serde_json::json!({ "ok": complete, "bytes": self.0 }))
            }
        }

        let router: BodyRouter = Arc::new(|req: &Request| {
            if req.path == "/upload" && req.method == "POST" {
                BodyRoute::Sink(Box::new(Counting(0)))
            } else {
                BodyRoute::Buffer
            }
        });
        let server = Server::start("127.0.0.1:0", Routes::new(handler).with_body(router), log).expect("bind");

        let payload = "x".repeat(MAX_BODY_BYTES + 1024);
        let raw =
            format!("POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{payload}", payload.len());
        let out = raw_request(server.local_addr(), &raw);
        assert!(out.starts_with("HTTP/1.1 200 OK"), "got: {out}");
        assert!(out.contains(&format!("\"bytes\":{}", payload.len())), "got: {out}");
        assert!(out.contains("\"ok\":true"), "a whole body is a complete upload: {out}");
    }

    /// A refusal decided from the headers costs one exchange, not one upload: the client
    /// announces a megabyte and gets its answer without sending any of it.
    #[test]
    fn a_refused_upload_is_answered_before_the_body_arrives() {
        let log = LogBuf::new(64);
        let handler: Handler = Arc::new(|_req: &Request| Response::text("buffered"));
        let router: BodyRouter = Arc::new(|req: &Request| {
            if req.path == "/upload" {
                BodyRoute::Answer(Response::error_json(415, "only audio/pcm"))
            } else {
                BodyRoute::Buffer
            }
        });
        let server = Server::start("127.0.0.1:0", Routes::new(handler).with_body(router), log).expect("bind");

        // Headers only, with a body announced and never sent. A server that read the body
        // first would sit here until the idle timeout.
        let out = raw_request(
            server.local_addr(),
            "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 1000000\r\n\r\n",
        );
        assert!(out.starts_with("HTTP/1.1 415 "), "got: {out}");
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

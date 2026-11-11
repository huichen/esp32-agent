//! The platform half of the on-board agent: a socket, a clock, and one thread.
//!
//! Everything that can be tested without hardware — the loop, the transcript, the
//! tool routing, the schedule predicates — lives in `esp32_agent_core`. What is left
//! for the board is exactly three things:
//!
//! * [`EspHttpTransport`] — the single `POST` that the model call and the `net_*`
//!   tools both need, spoken as HTTP/1.1 over `std::net::TcpStream` for `http://` and
//!   over esp-tls (mbedTLS, verified against the ESP-IDF certificate bundle) for
//!   `https://`. The TLS path adds a handshake and ~40 KB of heap for the session, not
//!   a different request: the bytes after the handshake are the same ones the plain
//!   socket would have carried.
//! * [`attach`] — hands that transport to the `App`, which owns the only place the
//!   rest of the firmware looks for it.
//! * [`start`] — one thread, one tick per configured interval (`agent.tick_ms`, re-read
//!   on every tick, so a portal edit takes effect on the next tick rather than on
//!   reboot): fire what the schedule says is due, then run the queued turn if there is
//!   one. The queue is depth-1 and coalescing, so a trigger that arrives mid-run is
//!   merged into the next turn instead of starting a second model call.
//!
//! Two consequences of the core design are visible here and are deliberate, not
//! accidents:
//!
//! * A run happens under the `App` mutex, because the loop needs the `App` as its
//!   `ToolHost`. A model call therefore blocks the portal for as long as the
//!   endpoint takes to answer — the thread logs when a run starts so the bench does
//!   not read that as a hang.
//! * The scheduler's clock is [`esp32_agent_core::app::now_ms`], the same clock the
//!   rest of the firmware stamps things with. Before SNTP syncs it starts at the
//!   epoch, so `every_s` tasks work immediately while `at_hhmm` tasks only mean a
//!   wall-clock time once the clock is real; the thread says which it is at boot.
//!
//! The blocks between the `===` markers (transport, one tick, the clock) depend
//! deliberately on nothing but `std`, `log`, `esp32_agent_core` and — for the TLS
//! handshake only — `esp_idf_svc::tls`, so the HTTP framing below is the same code on
//! both sides: this crate cannot run `cargo test`, and an untested HTTP client is
//! where the bench would lose a day.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use esp32_agent_core::app::{now_ms, App, Shared};
use esp32_agent_core::config::clamp_tick_ms;
use esp32_agent_core::llm::{self, ChatTransport};
use esp32_agent_core::logbuf::LogBuf;

/// The agent thread's stack. It holds a `TcpStream`, a JSON request body and
/// `serde_json`'s recursive parser; 32 KB matches the main thread.
// Internal RAM, not PSRAM: a thread stack cannot live in PSRAM. 32 KB failed to
// allocate once WiFi and the HTTP server were up (pthread: Failed to create task),
// and this thread only marshals JSON, so 16 KB is the working size.
const STACK_BYTES: usize = 16 * 1024;

/// Before this instant (2023-11-14) the clock has never been set, so `at_hhmm`
/// cannot mean a wall-clock time yet.
const CLOCK_SET_AFTER_S: u64 = 1_700_000_000;

// ============================ transport (std + llm only) ============================

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

/// esp-tls for `https://`: mbedTLS with the certificate bundle compiled into the image.
use esp_idf_svc::tls::{Config as TlsConfig, EspTls, InternalSocket};

/// Reply headers larger than this are refused instead of buffered.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// A reply body is read up to this much. The model reply is capped at 8 KB
/// upstream in `esp32_agent_core::agent`, so this is head-room, not a target.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// `LlmConfig::timeout_s` defaults to 45; the transport keeps the value inside
/// these bounds so a misplaced config cannot make it refuse to read at all.
const DEFAULT_TIMEOUT_S: u32 = 45;
const MIN_TIMEOUT_S: u32 = 5;
const MAX_TIMEOUT_S: u32 = 300;

/// `POST` for one OpenAI-compatible chat endpoint, plain or TLS.
///
/// The timeout sits behind an `Arc<AtomicU32>` because the portal can change it
/// while the transport is already owned by the `App`: the agent thread copies the
/// configured value in on every tick instead of rebuilding the client.
pub struct EspHttpTransport {
    timeout_s: Arc<AtomicU32>,
}

impl Default for EspHttpTransport {
    fn default() -> Self {
        Self { timeout_s: Arc::new(AtomicU32::new(DEFAULT_TIMEOUT_S)) }
    }
}

impl EspHttpTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// The handle the agent thread writes `LlmConfig::timeout_s` into.
    pub fn timeout_handle(&self) -> Arc<AtomicU32> {
        self.timeout_s.clone()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(clamp_timeout(self.timeout_s.load(Ordering::Relaxed)) as u64)
    }
}

fn clamp_timeout(seconds: u32) -> u32 {
    if seconds == 0 {
        DEFAULT_TIMEOUT_S
    } else {
        seconds.clamp(MIN_TIMEOUT_S, MAX_TIMEOUT_S)
    }
}

impl ChatTransport for EspHttpTransport {
    fn post_json(&mut self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String> {
        let target = Target::parse(url)?;
        let timeout = self.timeout();
        let request = request_bytes(&target, headers, body);

        let mut conn = if target.tls {
            Conn::connect_tls(&target, timeout)?
        } else {
            Conn::connect_plain(&target, timeout)?
        };
        conn.write_all(&request)?;
        let response = read_response(&mut conn, timeout)?;
        if (200..300).contains(&response.status) {
            Ok(response.body)
        } else {
            // Status plus a short body prefix, from the same helper the host side
            // uses, so the wording is identical on both ends.
            Err(llm::http_error(response.status, &response.body))
        }
    }
}

/// The socket a reply arrives on: plain TCP, or esp-tls when the endpoint is `https`.
///
/// Both arms expose the same three operations, so [`read_response`] — the part with
/// the framing rules in it — is written once and cannot drift between the two schemes.
enum Conn {
    Plain(TcpStream),
    Tls(Box<EspTls<InternalSocket>>),
}

impl Conn {
    fn connect_plain(target: &Target, timeout: Duration) -> Result<Self, String> {
        let stream = target.connect(timeout)?;
        stream.set_read_timeout(Some(timeout)).map_err(|e| format!("could not set the read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| format!("could not set the write timeout: {e}"))?;
        // One request per connection and no interactive traffic: Nagle would only
        // add latency to the header/body split.
        let _ = stream.set_nodelay(true);
        Ok(Conn::Plain(stream))
    }

    /// A TLS session verified against the certificate bundle in the image.
    ///
    /// There is deliberately no "trust anything" switch: a board that accepts any
    /// certificate cannot tell the provider from an impostor on the same WiFi, which
    /// is the one thing TLS is here to answer. A private CA belongs in `ca_cert`.
    fn connect_tls(target: &Target, timeout: Duration) -> Result<Self, String> {
        let mut tls = EspTls::new().map_err(|e| format!("could not create the TLS session: {e}"))?;
        let cfg = TlsConfig {
            use_crt_bundle_attach: true,
            timeout_ms: timeout.as_millis().min(u32::MAX as u128) as u32,
            ..Default::default()
        };
        tls.connect(&target.host, target.port, &cfg).map_err(|e| {
            format!(
                "the TLS handshake with {} failed: {e} (board clock {}s, internal heap {} kB with a \
                 {} kB largest block)",
                target.authority(),
                unsafe { esp_idf_svc::sys::time(std::ptr::null_mut()) },
                unsafe { esp_idf_svc::sys::esp_get_free_internal_heap_size() } / 1024,
                unsafe {
                    esp_idf_svc::sys::heap_caps_get_largest_free_block(
                        esp_idf_svc::sys::MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT,
                    )
                } / 1024
            )
        })?;
        Ok(Conn::Tls(Box::new(tls)))
    }

    fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        match self {
            Conn::Plain(stream) => {
                stream.write_all(data).and_then(|()| stream.flush()).map_err(|e| format!("sending the request failed: {e}"))
            }
            Conn::Tls(tls) => tls.write_all(data).map_err(|e| format!("sending the request over TLS failed: {e}")),
        }
    }

    /// One read. Interrupted reads are retried here so every caller sees the same
    /// contract the plain socket has.
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, String> {
        match self {
            Conn::Plain(stream) => loop {
                match stream.read(buf) {
                    Ok(n) => return Ok(n),
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(read_error(&e, timeout)),
                }
            },
            Conn::Tls(tls) => tls.read(buf).map_err(|e| {
                format!("reading the reply over TLS failed: {e} (the session times out after {} s)", timeout.as_secs())
            }),
        }
    }
}

/// `http://host[:port]/path` or `https://host[:port]/path`, parsed out of the config
/// string.
#[derive(Debug)]
struct Target {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

impl Target {
    fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        let lower = url.to_ascii_lowercase();
        // Both schemes are supported; TLS is a property of this struct, not a
        // separate code path above it.
        let (tls, rest) = if lower.starts_with("https://") {
            (true, &url["https://".len()..])
        } else if lower.starts_with("http://") {
            (false, &url["http://".len()..])
        } else {
            return Err(format!(
                "the model endpoint must start with http:// or https://, got `{}`",
                llm::body_prefix(url, 80)
            ));
        };
        let default_port = if tls { 443u16 } else { 80u16 };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(format!("`{}` has no host part", llm::body_prefix(url, 80)));
        }
        if authority.starts_with('[') {
            // getaddrinfo wants the bare `::1`, so a bracketed literal would have to
            // be unwrapped here; a LAN address or a provider name is v4 or DNS, so say
            // so instead of resolving something subtly wrong.
            return Err("IPv6 literals are not supported; use an IPv4 address or a host name".to_string());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                let port = port.parse::<u16>().map_err(|_| format!("`{port}` is not a port number"))?;
                (host, port)
            }
            _ => (authority, default_port),
        };
        if host.is_empty() {
            return Err(format!("`{}` has no host part", llm::body_prefix(url, 80)));
        }
        Ok(Self { tls, host: host.to_string(), port, path: path.to_string() })
    }

    /// `host:port` for messages, and the `Host` header with the scheme's default port
    /// left off.
    fn authority(&self) -> String {
        self.host_header()
    }

    fn host_header(&self) -> String {
        let default_port = if self.tls { 443 } else { 80 };
        if self.port == default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Resolve, then connect to each candidate address, `timeout` per attempt.
    fn connect(&self, timeout: Duration) -> Result<TcpStream, String> {
        let addrs = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| format!("could not resolve `{}`: {e}", self.authority()))?;
        let mut last_error = format!("`{}` resolved to no address", self.authority());
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(stream) => return Ok(stream),
                Err(e) => last_error = format!("connecting to {addr} failed: {e}"),
            }
        }
        Err(last_error)
    }
}

/// The request head plus body as one buffer, so a multi-byte UTF-8 body is never
/// re-encoded on the way to the socket.
fn request_bytes(target: &Target, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut head = String::with_capacity(256 + body.len());
    head.push_str(&format!("POST {} HTTP/1.1\r\n", target.path));
    head.push_str(&format!("Host: {}\r\n", target.authority()));
    // One request per connection: the reply is delimited by the close as well as by
    // its own framing, which is what lets the reader stay this small.
    head.push_str("Connection: close\r\n");
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        // Never let a caller override the framing or duplicate the host.
        if matches!(lower.as_str(), "host" | "connection" | "content-length" | "transfer-encoding") {
            continue;
        }
        // A header carrying CR/LF would split the request in two; drop it rather
        // than send something the endpoint reads as two requests.
        if name.contains('\r') || name.contains('\n') || value.contains('\r') || value.contains('\n') {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

struct Response {
    status: u16,
    body: String,
}

fn read_response(conn: &mut Conn, timeout: Duration) -> Result<Response, String> {
    let mut raw: Vec<u8> = Vec::with_capacity(2048);
    let mut buf = [0u8; 512];
    let header_end = loop {
        if let Some(index) = find_bytes(&raw, b"\r\n\r\n") {
            break index + 4;
        }
        if raw.len() > MAX_HEADER_BYTES {
            return Err(format!("the reply headers exceed {MAX_HEADER_BYTES} bytes"));
        }
        match conn.read(&mut buf, timeout) {
            Ok(0) => {
                if raw.is_empty() {
                    return Err("the server closed the connection without a reply".to_string());
                }
                return Err(format!(
                    "the connection closed inside the reply headers: {}",
                    llm::body_prefix(&String::from_utf8_lossy(&raw), 120)
                ));
            }
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) => return Err(e),
        }
    };

    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default().to_string();
    let status = parse_status(&status_line)
        .ok_or_else(|| format!("unexpected HTTP status line: {}", llm::body_prefix(&status_line, 80)))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse::<usize>().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }

    let mut body = raw[header_end..].to_vec();
    if chunked {
        // `Connection: close` means the last chunk and the close agree: read to the
        // end and decode afterwards, so a chunk split across reads costs nothing.
        fill_to_end(conn, &mut body, timeout);
        body = dechunk(&body);
    } else if let Some(length) = content_length {
        if body.len() < length {
            fill_length(conn, &mut body, length, timeout)?;
        }
        body.truncate(length.min(body.len()));
    } else {
        fill_to_end(conn, &mut body, timeout);
    }
    if body.len() > MAX_BODY_BYTES {
        body.truncate(MAX_BODY_BYTES);
    }
    Ok(Response { status, body: String::from_utf8_lossy(&body).to_string() })
}

/// Read until `body` holds `length` bytes. A close before that is an error: the
/// caller asked for a specific body and a short one would parse as something else.
fn fill_length(conn: &mut Conn, body: &mut Vec<u8>, length: usize, timeout: Duration) -> Result<(), String> {
    let mut buf = [0u8; 512];
    while body.len() < length && body.len() <= MAX_BODY_BYTES {
        match conn.read(&mut buf, timeout) {
            Ok(0) => return Err(format!("the reply ended after {} of {length} bytes", body.len())),
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Read to the close, bounded. A read error after the body started is tolerated:
/// some endpoints close without a clean FIN, and what arrived is still useful.
fn fill_to_end(conn: &mut Conn, body: &mut Vec<u8>, timeout: Duration) {
    let mut buf = [0u8; 512];
    while body.len() <= MAX_BODY_BYTES {
        match conn.read(&mut buf, timeout) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(e) => {
                log::debug!("agent: body read stopped early: {e}");
                break;
            }
        }
    }
}

/// Decode `Transfer-Encoding: chunked`. A malformed tail keeps what was decoded so
/// far rather than throwing the whole reply away.
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let Some(line_end) = find_bytes(data, b"\r\n") else { break };
        let size_line = String::from_utf8_lossy(&data[..line_end]);
        let size_text = size_line.split(';').next().unwrap_or_default().trim().to_string();
        let Ok(size) = usize::from_str_radix(&size_text, 16) else { break };
        data = &data[line_end + 2..];
        if size == 0 || out.len() > MAX_BODY_BYTES {
            break;
        }
        let take = size.min(data.len());
        out.extend_from_slice(&data[..take]);
        if take < size {
            break;
        }
        data = &data[size..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    out
}

fn parse_status(line: &str) -> Option<u16> {
    let mut parts = line.split_whitespace();
    if !parts.next()?.to_ascii_uppercase().starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse::<u16>().ok()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn read_error(e: &std::io::Error, timeout: Duration) -> String {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            format!("the endpoint did not answer within {} s", timeout.as_secs())
        }
        ErrorKind::UnexpectedEof => "the server closed the connection mid-reply".to_string(),
        _ => format!("reading the reply failed: {e}"),
    }
}

// ============================ end of the transport block ============================

// ---------------------------------------------------------------- boot wiring

/// Start the agent thread; the returned handle is kept by `main` for nothing but
/// lifetime, so the signature mirrors the thread's own arguments.
///
/// Called once from `main` after the radio and the HTTP server are up: a board with
/// no address can neither reach a model nor be reached by the portal, and a thread
/// that ticks earlier would only produce connection errors.
pub fn start(
    shared: Shared,
    log: Arc<LogBuf>,
    timeout_s: Arc<AtomicU32>,
    transport: EspHttpTransport,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("mcp-agent".into())
        .stack_size(STACK_BYTES)
        // The model transport lives on this thread, not in the `App`: a turn is run in
        // phases precisely so that the model call happens with the app lock released.
        .spawn(move || run_loop(shared, log, timeout_s, transport))
        .map(|_| ())
        .map_err(|e| format!("could not start the agent thread: {e}"))
}

/// Hand the HTTP transport to the `App` and say what it will talk to.
///
/// Returns the timeout handle the thread keeps, so a portal edit takes effect on the
/// next tick instead of on the next reboot.
pub fn attach(shared: &Shared, log: &LogBuf) -> (Arc<AtomicU32>, EspHttpTransport) {
    let transport = EspHttpTransport::new();
    let handle = transport.timeout_handle();
    let mut app = lock(shared);
    handle.store(clamp_timeout(app.llm.timeout_s), Ordering::Relaxed);
    let model = app.llm.model.clone();
    let url = app.llm.base_url.clone();
    // One client stays in the `App` for the `net_*` tools.
    app.attach_tool_transport(Box::new(EspHttpTransport::new()));
    drop(app);

    log.info("agent", "HTTP transport attached to the agent for the net_* tools (http and https)");
    if url.trim().is_empty() || model.trim().is_empty() {
        log.warn(
            "agent",
            "no model configured: set base_url and model in the portal Agent panel; the thread idles until then",
        );
    } else {
        log.info("agent", format!("model `{model}` at {}", llm::body_prefix(&url, 96)));
    }
    // ...and this one belongs to the agent thread, which owns it for the model call.
    (handle, transport)
}

// ---------------------------------------------------------------- the thread

fn run_loop(
    shared: Shared,
    log: Arc<LogBuf>,
    timeout_s: Arc<AtomicU32>,
    mut transport: EspHttpTransport,
) {
    // The interval is config, not a constant: it is the queue-drain latency, so the
    // portal is allowed to trade latency against scheduling overhead without a reflash.
    let mut tick_ms = tick_interval(&shared);
    log.info("agent", format!("agent thread up: tick {tick_ms} ms, scheduler clock {}", clock_note()));
    if now_ms() / 1000 < CLOCK_SET_AFTER_S {
        log.warn(
            "agent",
            "the clock is not synced yet: `every_s` tasks work, `at_hhmm` tasks only after SNTP",
        );
    }

    loop {
        // Re-read every tick: a portal edit to `agent.tick_ms` applies to the next
        // sleep, not to the next boot.
        let configured = tick_interval(&shared);
        if configured != tick_ms {
            log.info("agent", format!("tick is now {configured} ms (queued work waits at most that long)"));
            tick_ms = configured;
        }
        std::thread::sleep(Duration::from_millis(tick_ms));
        let now = now_ms();
        let minute = minute_of_day(now);
        tick(&shared, &mut transport, now, minute, &timeout_s, &log);
    }
}

/// The configured tick under a short lock of its own, clamped the way the config layer
/// clamps it (so an out-of-range value cannot make the loop spin or stall), because what
/// the loop sleeps on has to be what the portal last stored.
fn tick_interval(shared: &Shared) -> u64 {
    let app = lock(shared);
    u64::from(clamp_tick_ms(app.cfg.agent.tick_ms))
}

// ============================ tick (App only) ============================

/// What one tick of the agent thread does: sync the model timeout, fire what came due,
/// then run the queued turn.
///
/// Kept apart from the loop for one reason: this is the part with a decision in it,
/// and it takes an `App` plus an explicit clock rather than doing its own sleeping,
/// so the same function can be driven on the host with a virtual board and a stub
/// model instead of only on the bench.
fn tick(
    shared: &Shared,
    transport: &mut EspHttpTransport,
    now: u64,
    minute: u32,
    timeout_s: &AtomicU32,
    log: &LogBuf,
) {
    // Everything that needs the transcript or the tools happens in a short phase under
    // the lock; the model call below happens with it released, so the portal keeps
    // answering while the model thinks.
    let queued = {
        let mut app = lock(shared);
        // The wall clock only becomes real when SNTP lands, a second or two in. Until
        // then `uptime_s` reports zero rather than the decades between 1970 and now;
        // this is where it gets re-anchored, once.
        if app.runtime.rebase_uptime(now) {
            log.info("agent", "wall clock set; uptime counts from this tick onwards");
        }
        // A portal edit to the timeout must reach the next request, not the next boot.
        let configured = clamp_timeout(app.llm.timeout_s);
        if timeout_s.load(Ordering::Relaxed) != configured {
            timeout_s.store(configured, Ordering::Relaxed);
            log.info("agent", format!("model timeout is now {configured} s"));
        }

        // Schedule first: a task that comes due on this tick runs on this tick.
        for source in app.sched_tick(now, minute) {
            log.info("agent", format!("scheduler queued {source}"));
        }

        // Thread stacks come from internal RAM, and the heap figure the portal shows
        // counts PSRAM. A board that cannot spawn an HTTP connection thread reports
        // nothing useful, so say the number that matters *before* it gets there.
        let internal_kb = unsafe { esp_idf_svc::sys::esp_get_free_internal_heap_size() } / 1024;
        // xtensa has no 64-bit atomics, so this counts seconds: 32 bits is 136 years.
        static LAST_WARN_S: AtomicU32 = AtomicU32::new(0);
        let now_s = (now / 1000) as u32;
        let last = LAST_WARN_S.load(Ordering::Relaxed);
        if internal_kb < 24 && now_s.saturating_sub(last) > 60 {
            LAST_WARN_S.store(now_s, Ordering::Relaxed);
            log.warn(
                "agent",
                format!(
                    "internal free heap is {internal_kb} kB: the HTTP accept thread needs ~16 kB of it, so requests and streams start failing here"
                ),
            );
        }

        app.agent.pending.as_ref().map(|pending| {
            (
                pending.source.clone(),
                app.llm.max_steps.max(1),
                clamp_timeout(app.llm.timeout_s),
            )
        })
    };

    let Some((source, steps, timeout)) = queued else {
        return;
    };
    log.info(
        "agent",
        format!("run starting (source `{source}`, <= {steps} step(s), {timeout} s per call)"),
    );
    let started = Instant::now();
    match esp32_agent_core::app::drive_agent(shared, transport) {
        Some(Ok(text)) if text.is_empty() => {
            log.info("agent", format!("run finished with no text after {} ms", started.elapsed().as_millis()))
        }
        Some(Ok(text)) => log.info(
            "agent",
            format!(
                "run finished after {} ms: {}",
                started.elapsed().as_millis(),
                llm::body_prefix(&text, 160)
            ),
        ),
        Some(Err(e)) => log.warn(
            "agent",
            format!("run failed after {} ms: {}", started.elapsed().as_millis(), llm::body_prefix(&e, 200)),
        ),
        None => {}
    }
}

// ============================ end of the tick block ============================

/// Poisoning is not fatal here: the `App` is plain data behind the mutex, and a
/// thread that panicked mid-update must not stop the agent from ever ticking again.
fn lock(shared: &Shared) -> MutexGuard<'_, App> {
    match shared.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("agent: the app lock was poisoned; continuing with its contents");
            poisoned.into_inner()
        }
    }
}

// ---------------------------------------------------------------- the clock

// ============================ clock (pure) ============================

/// Local minute of the day, `0..1440`, for `ScheduledTask::at_hhmm`.
///
/// The board has no timezone database, so the offset is a build-time constant:
/// `MCP_TZ_OFFSET_H=8` for UTC+8, unset for UTC. `every_s` tasks do not use this.
pub fn minute_of_day(now_ms: u64) -> u32 {
    let local = (now_ms / 1000) as i64 + offset_secs();
    (local.rem_euclid(86_400) / 60) as u32
}

fn offset_secs() -> i64 {
    option_env!("MCP_TZ_OFFSET_H")
        .and_then(|text| text.trim().parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(-12, 14)
        * 3600
}

fn clock_note() -> String {
    match offset_secs() {
        0 => "UTC (build with MCP_TZ_OFFSET_H to shift at_hhmm)".to_string(),
        secs => format!("UTC{:+}", secs / 3600),
    }
}

// ============================ end of the clock block ============================

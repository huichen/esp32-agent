//! Host simulator.
//!
//! Runs the same HTTP server, portal, MCP endpoint, drivers and manifest engine as
//! the firmware, against [`mcp_core::virtual_board::VirtualBoard`] instead of
//! ESP-IDF. This is how the project is developed and tested without a board on the
//! desk, and how a contributor can try a driver before flashing anything.

use esp32_agent_core::app::{drive_agent, make_handler, App, FwMeta, Shared};
use esp32_agent_core::config::clamp_tick_ms;
use esp32_agent_core::http::Server;
use esp32_agent_core::llm::ChatTransport;
use esp32_agent_core::logbuf::LogBuf;
use esp32_agent_core::store::FileStore;
use esp32_agent_core::virtual_board::{FakeKind, VirtualBoard};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Minute of the day for `at_hhmm` scheduling.
///
/// The firmware applies `MCP_TZ_OFFSET_H` at build time because a board has no timezone
/// database; the simulator runs on a host that has one and is only used for trying
/// things out, so it schedules in UTC and says so.
fn minute_of_day(now_ms: u64) -> u32 {
    ((now_ms / 1000 / 60) % 1_440) as u32
}

/// The model transport for the simulator: plain HTTP over `std::net`.
///
/// The firmware has the same thing over esp-idf's client. Without it a message queued
/// in the simulator sat `pending` forever - the transcript stayed empty and the portal's
/// agent panel could not be tried without a board on the desk.
struct HostTransport {
    timeout: Duration,
}

impl ChatTransport for HostTransport {
    fn post_json(&mut self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String> {
        let rest = url.strip_prefix("http://").ok_or_else(|| format!("only http:// is supported: {url}"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let addr = if authority.contains(':') { authority.to_string() } else { format!("{authority}:80") };
        let mut stream =
            std::net::TcpStream::connect(&addr).map_err(|e| format!("connect {addr} failed: {e}"))?;
        stream.set_read_timeout(Some(self.timeout)).ok();
        stream.set_write_timeout(Some(self.timeout)).ok();

        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).map_err(|e| format!("write failed: {e}"))?;
        stream.write_all(body.as_bytes()).map_err(|e| format!("write failed: {e}"))?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|e| format!("read failed: {e}"))?;
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

struct Args {
    host: String,
    port: u16,
    state: String,
    empty: bool,
}

fn parse_args() -> Args {
    let mut args =
        Args { host: "0.0.0.0".into(), port: 8080, state: "sim-state/kv.json".into(), empty: false };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => args.port = it.next().and_then(|v| v.parse().ok()).unwrap_or(8080),
            "--host" => args.host = it.next().unwrap_or_else(|| "0.0.0.0".into()),
            "--state" => args.state = it.next().unwrap_or_else(|| "sim-state/kv.json".into()),
            "--empty" => args.empty = true,
            "-h" | "--help" => {
                println!("usage: esp32-agent-sim [--host 0.0.0.0] [--port 8080] [--state <file>] [--empty]");
                println!();
                println!("  --empty   start with no virtual I2C devices (tests \"nothing wired\")");
                std::process::exit(0);
            }
            other => eprintln!("ignoring unknown argument `{other}`"),
        }
    }
    args
}

fn main() {
    let args = parse_args();
    let log = LogBuf::new(512);
    let store = FileStore::open(&args.state);

    let mut board = VirtualBoard::new();
    if args.empty {
        board.i2c.clear();
    } else {
        // A couple more chips than the default bench, so the portal shows variety.
        board.add(0x40, FakeKind::Ina219);
    }
    board.wifi.ap_ssid = "esp32-agent-setup".into();

    let firmware = FwMeta {
        name: "esp32-agent".into(),
        version: esp32_agent_core::VERSION.into(),
        build: format!("sim {}", env!("CARGO_PKG_VERSION")),
    };
    let app: Shared = Arc::new(Mutex::new(App::new(Box::new(board), Box::new(store), log.clone(), firmware)));

    let addr = format!("{}:{}", args.host, args.port);
    let server = match Server::start(&addr, make_handler(app.clone()), log.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not listen on {addr}: {e}");
            std::process::exit(1);
        }
    };
    {
        let mut guard = app.lock().expect("app lock");
        guard.lan_url = format!("http://127.0.0.1:{}", server.local_addr().port());
    }

    println!();
    println!("  esp32-agent simulator");
    println!("  portal   http://127.0.0.1:{}/", server.local_addr().port());
    println!("  mcp      http://127.0.0.1:{}/mcp", server.local_addr().port());
    println!("  state    {}", args.state);
    println!();
    println!("  try: curl -sS -X POST http://127.0.0.1:{}/mcp \\", server.local_addr().port());
    println!("         -H 'Content-Type: application/json' \\");
    println!("         -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}}'");
    println!();

    // The loop's period is the device config, like on the board.
    let mut tick_ms = {
        let guard = app.lock().expect("app lock");
        u64::from(clamp_tick_ms(guard.cfg.agent.tick_ms))
    };
    let mut transport = HostTransport { timeout: Duration::from_secs(45) };

    loop {
        std::thread::sleep(Duration::from_millis(tick_ms));
        let (reboot, due) = {
            let mut guard = match app.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            // The same two jobs the firmware thread does: evaluate the schedule, then
            // let whoever is waiting queue a turn.
            let now = esp32_agent_core::app::now_ms();
            let due = guard.sched_tick(now, minute_of_day(now));
            let reboot = guard.runtime.restart_requested;
            if reboot {
                guard.runtime.restart_requested = false;
            }
            tick_ms = u64::from(clamp_tick_ms(guard.cfg.agent.tick_ms));
            (reboot, due)
        };
        for source in due {
            log.info("sim", format!("scheduler queued {source}"));
        }
        if reboot {
            // A real board restarts here. The simulator keeps the process alive so
            // the browser tab and its state survive the round trip.
            log.info("sim", "restart requested; the simulator keeps running");
        }
        // Unlike the board, the simulator has no HTTP client of its own to keep for
        // `net_*`, so one transport serves both uses.
        if let Some(outcome) = drive_agent(&app, &mut transport) {
            match outcome {
                Ok(text) => log.info(
                    "sim",
                    format!("agent turn finished: {}", text.chars().take(120).collect::<String>()),
                ),
                Err(e) => log.warn("sim", format!("agent turn failed: {e}")),
            }
        }
    }
}

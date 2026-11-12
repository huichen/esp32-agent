//! Host simulator.
//!
//! Runs the same HTTP server, portal, MCP endpoint, drivers and manifest engine as
//! the firmware, against [`mcp_core::virtual_board::VirtualBoard`] instead of
//! ESP-IDF. This is how the project is developed and tested without a board on the
//! desk, and how a contributor can try a driver before flashing anything.

use esp32_agent_core::app::{drive_agent, make_routes, App, FwMeta, Shared};
use esp32_agent_core::config::clamp_tick_ms;
use esp32_agent_core::http::Server;
use esp32_agent_core::logbuf::LogBuf;
use esp32_agent_core::store::FileStore;
use esp32_agent_core::virtual_board::{FakeKind, VirtualBoard};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod net;

use net::HostTransport;

/// Minute of the day for `at_hhmm` scheduling.
///
/// The firmware applies `MCP_TZ_OFFSET_H` at build time because a board has no timezone
/// database; the simulator runs on a host that has one and is only used for trying
/// things out, so it schedules in UTC and says so.
fn minute_of_day(now_ms: u64) -> u32 {
    ((now_ms / 1000 / 60) % 1_440) as u32
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
    // Before any request: rustls 0.23 will not pick a crypto provider on its own.
    net::install_crypto_provider();
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
        commit: "sim".into(),
        built_at: "sim".into(),
    };
    let app: Shared = Arc::new(Mutex::new(App::new(Box::new(board), Box::new(store), log.clone(), firmware)));

    let addr = format!("{}:{}", args.host, args.port);
    let server = match Server::start(&addr, make_routes(app.clone()), log.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not listen on {addr}: {e}");
            std::process::exit(1);
        }
    };
    {
        let mut guard = app.lock().expect("app lock");
        guard.lan_url = format!("http://127.0.0.1:{}", server.local_addr().port());
        // The loop's client drives the model call (`drive_agent` takes it); the tools
        // need one of their own, or `net_get`/`net_post` answer "no transport". The
        // firmware has attached this since it got the agent; the simulator never did,
        // which is why a `net_*` call inside a turn failed here and worked on a board.
        guard.attach_tool_transport(Box::new(HostTransport::new(Duration::from_secs(45))));
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
    let mut transport = HostTransport::new(Duration::from_secs(45));

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
        // Unlike the board, the simulator has no separate HTTP client to keep for
        // `net_*`: the App holds the tools' transport (attached above) and the loop
        // holds the model one, which is why one `HostTransport` is not enough here.
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

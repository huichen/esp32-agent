//! Host simulator.
//!
//! Runs the same HTTP server, portal, MCP endpoint, drivers and manifest engine as
//! the firmware, against [`mcp_core::virtual_board::VirtualBoard`] instead of
//! ESP-IDF. This is how the project is developed and tested without a board on the
//! desk, and how a contributor can try a driver before flashing anything.

use esp32_agent_core::app::{make_handler, App, FwMeta, Shared};
use esp32_agent_core::http::Server;
use esp32_agent_core::logbuf::LogBuf;
use esp32_agent_core::store::FileStore;
use esp32_agent_core::virtual_board::{FakeKind, VirtualBoard};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

    loop {
        std::thread::sleep(Duration::from_millis(500));
        let reboot = {
            let mut guard = match app.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if guard.runtime.restart_requested {
                guard.runtime.restart_requested = false;
                true
            } else {
                false
            }
        };
        if reboot {
            // A real board restarts here. The simulator keeps the process alive so
            // the browser tab and its state survive the round trip.
            log.info("sim", "restart requested; the simulator keeps running");
        }
    }
}

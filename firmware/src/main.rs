//! Firmware entry point.
//!
//! Boot order matters: the config must be readable before any bus or pin is
//! touched, because it says which pins to touch. After that the app detects
//! hardware, the radio comes up (setup AP when there are no credentials, station
//! when there are), and the HTTP server serves both the portal and `/mcp` on the
//! same port. A manifest edit only rebuilds the tool registry — the server, the
//! radio and the buses keep running.

// The two platform halves of a layered design, named for what they are rather than for
// the layer above them: the agent *loop* lives in `esp32-agent-core` (and is tested on the
// host against a virtual board), while `runner` is the ESP-IDF thread that drives it and
// `console` is the USB-Serial-JTAG reader. Two files called `agent.rs` was the earlier
// shape, and it made the split look like a duplicate.
mod console;
mod dns;
mod esp_hw;
mod nvs_store;
mod runner;
mod tls_server;
mod wifi;

use esp32_agent_core::app::{make_routes, App, FwMeta, Shared, TlsInfo};
use esp32_agent_core::config::Config;
use esp32_agent_core::hardware::Hardware;
use esp32_agent_core::http::Server;
use esp32_agent_core::logbuf::LogBuf;
use esp32_agent_core::store::KvStore;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::sys;
use std::sync::{Arc, Mutex};

/// Log what the internal heap looks like at a boot milestone.
///
/// PSRAM can carry allocations but not thread stacks, and the HTTP server spawns one
/// thread per connection: when internal RAM runs out the portal does not open and the only
/// symptom is `could not spawn connection thread` in the log, long after the cause. The
/// largest block matters as much as the total — ten 10 kB fragments do not make a stack.
fn mem(label: &str) {
    unsafe {
        let free = sys::esp_get_free_internal_heap_size() as u32;
        let largest =
            sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT);
        let msg = format!(
            "{}: internal free {} kB, largest block {} kB",
            label,
            free / 1024,
            largest / 1024
        );
        log::info!("{}", msg);
    }
}

// The descriptor the bootloader prints as `Project name`. esp-idf-sys compiles the C
// side as its own CMake project (`project(libespidf)`), and IDF's own descriptor is a
// weak symbol carrying that name; this strong definition replaces it so the image says
// which firmware is actually on the chip. `CARGO_PKG_NAME` is this crate's name.
esp_idf_svc::sys::esp_app_desc!();

/// Log the partition table the chip actually booted from.
///
/// The C project esp-idf-sys generates only sees this repository's `partitions.csv`
/// because `.cargo/config.toml` hands it over, and "the file says so" is not evidence:
/// this is what asks the running chip. `esp_partition_next` releases the iterator
/// itself once the list ends, so there is no separate release here.
fn log_partitions() {
    unsafe {
        let mut it = esp_idf_svc::sys::esp_partition_find(
            esp_idf_svc::sys::esp_partition_type_t_ESP_PARTITION_TYPE_ANY,
            esp_idf_svc::sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
            std::ptr::null(),
        );
        while !it.is_null() {
            let p = esp_idf_svc::sys::esp_partition_get(it);
            if !p.is_null() {
                let label = std::ffi::CStr::from_ptr((*p).label.as_ptr()).to_string_lossy();
                log::info!(
                    "partition {label:9} type {} subtype {} offset 0x{:06x} size 0x{:06x}",
                    (*p).type_,
                    (*p).subtype,
                    (*p).address,
                    (*p).size
                );
            }
            it = esp_idf_svc::sys::esp_partition_next(it);
        }
    }
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    // The IDF main task's stack size comes from sdkconfig; until that is applied
    // reliably, run everything on a thread with an explicit stack. The reset loop
    // this fixes showed no panic message, only an immediate RTC_SW_CPU_RST.
    std::thread::Builder::new()
        .name("mcp-main".into())
        .stack_size(32 * 1024)
        .spawn(run)?
        .join()
        .map_err(|_| anyhow::anyhow!("main task panicked"))?
}

fn is_auto_board(cfg: &Config) -> bool {
    let board = cfg.board.trim();
    board.is_empty() || board.eq_ignore_ascii_case("auto")
}

/// The I2C buses to open, including one alternative when the board is `auto`.
///
/// The configured bus is always first. When the board is auto-resolved and still
/// only has the stock wiring, one more candidate from another profile of the same
/// SoC is opened: profiles for boards that differ only in where I2C was routed are
/// indistinguishable from the chip, and a second candidate costs one scan. The S3
/// has two I2C controllers, so this never exceeds what the silicon can do.
fn candidate_buses(cfg: &Config) -> Vec<(u8, i32, i32, u32)> {
    let mut buses: Vec<(u8, i32, i32, u32)> =
        cfg.bus.i2c.iter().map(|b| (b.bus, b.sda, b.scl, b.freq_hz)).collect();
    if !is_auto_board(cfg) || !cfg.bus.auto_probe || buses.is_empty() || buses.len() >= 2 {
        return buses;
    }
    let soc = option_env!("MCU").unwrap_or("esp32");
    let (sda, scl) = (buses[0].1, buses[0].2);
    let alternative = esp32_agent_core::boards::PROFILES
        .iter()
        .filter(|p| p.soc == soc)
        .flat_map(|p| p.i2c.iter())
        .find(|h| h.sda != sda || h.scl != scl)
        .copied();
    if let Some(hint) = alternative {
        buses.push((buses.len() as u8, hint.sda, hint.scl, hint.freq_hz));
    }
    buses
}

fn run() -> anyhow::Result<()> {
    log::info!("esp32-agent {} starting", esp32_agent_core::VERSION);
    mem("boot");
    log_partitions();
    // Two initialisations must happen between the C runtime and the first socket:
    // the NVS partition this firmware stores its config in, and the network stack
    // that the DNS and HTTP sockets belong to. Skipping them shows up as
    // "tcpip_send_msg_wait_sem ... Invalid mbox" the moment a socket is touched.
    // The default event loop is deliberately *not* created here: esp-idf-svc's
    // `EspSystemEventLoop::take()` owns it and fails with ESP_ERR_INVALID_STATE if
    // somebody got there first.
    unsafe {
        let rc = esp_idf_svc::sys::nvs_flash_init();
        if rc != esp_idf_svc::sys::ESP_OK {
            log::warn!("nvs_flash_init returned {rc}");
        }
        esp_idf_svc::sys::esp_netif_init();
    }

    // Taken exactly once, here: `Peripherals::take()` is a process-wide singleton,
    // so the radio thread gets the modem handed to it instead of taking its own.
    let peripherals = esp_idf_svc::hal::peripherals::Peripherals::take()?;
    // The NVS partition and the system event loop are singletons of the same kind:
    // a second `take()` fails with ESP_ERR_INVALID_STATE rather than sharing the
    // handle, and the radio thread needs both.
    let nvs = esp_idf_svc::nvs::EspDefaultNvsPartition::take()?;
    let sys_loop = esp_idf_svc::eventloop::EspSystemEventLoop::take()?;

    let mut store = nvs_store::NvsStore::open("esp32mcp", nvs.clone())?;
    // Read the config once before touching hardware: it says which pins the I2C
    // buses live on.
    let mut cfg = store
        .get(esp32_agent_core::config::NVS_KEY_CONFIG)
        .and_then(|s| Config::from_json_str(&s))
        .unwrap_or_else(|| {
            log::info!("no stored config; using defaults and the setup portal");
            Config::default()
        });

    // Build-time WiFi seed. The credentials come from environment variables the
    // build script sets locally; they are never in the repository. This is for a
    // bench unit that should join a known network without the setup portal, and
    // it is stored into NVS once so later boots behave like a manual setup.
    if cfg.wifi.ssid.is_empty() {
        if let (Some(ssid), Some(pass)) = (option_env!("AGENT_WIFI_SSID"), option_env!("AGENT_WIFI_PASS")) {
            if !ssid.is_empty() {
                cfg.wifi.ssid = ssid.to_string();
                cfg.wifi.password = pass.to_string();
                if store.set(esp32_agent_core::config::NVS_KEY_CONFIG, &cfg.to_json_string()) {
                    log::info!("seeded WiFi for `{ssid}` from the build");
                }
            }
        }
    }

    // The ring is the single largest small allocation this firmware makes, and internal
    // RAM is what thread stacks come from: 256 lines at ~150 bytes each cost ~38 kB and
    // the HTTP server could not spawn a connection thread. The configured size is the
    // one that applies; the old hardcoded 256 silently ignored it.
    let log = LogBuf::new(cfg.log.ring.clamp(16, 256));
    let buses = candidate_buses(&cfg);
    let mut hw = match esp_hw::EspHw::new(&buses) {
        Ok(h) => h,
        Err(e) => {
            log.error("boot", format!("hardware init failed: {e}"));
            return Err(e);
        }
    };
    // A profile says where I2C is wired. `board: auto` can only look at the chip, and
    // a bare DevKitC-1 is indistinguishable from the same SoC on a carrier board, so
    // the guess can name pins nothing is wired to. When one of the candidates answers
    // and the resolved profile does not own it, the board is the one that owns those
    // pins: adopt its profile, so buttons, microphone and amplifier come from the same
    // wiring as the panel. Without this the bench board reported "no devices" while a
    // 128x64 panel sat on SDA41/SCL42.
    if is_auto_board(&cfg) {
        let info = hw.board_info();
        let scans: Vec<(u8, i32, i32, usize)> = buses
            .iter()
            .map(|(bus, sda, scl, _)| (*bus, *sda, *scl, hw.i2c_scan(*bus).map(|v| v.len()).unwrap_or(0)))
            .collect();
        log::info!(
            "auto board: candidate buses {:?}",
            scans.iter().map(|(_, sda, scl, n)| format!("SDA{sda}/SCL{scl}:{n}")).collect::<Vec<_>>()
        );
        let answered = scans.iter().copied().filter(|(_, _, _, n)| *n > 0).max_by_key(|(_, _, _, n)| *n);
        if let Some((bus, sda, scl, n)) = answered {
            let (current, _) = esp32_agent_core::boards::resolve(&cfg.board, &info);
            if !current.i2c.iter().any(|h| h.sda == sda && h.scl == scl) {
                let owner = esp32_agent_core::boards::PROFILES.iter().find(|p| {
                    p.soc == info.soc && p.i2c.iter().any(|h| h.sda == sda && h.scl == scl)
                });
                if let Some(profile) = owner {
                    log::info!(
                        "I2C answered on bus {bus} (SDA {sda}, SCL {scl}): {n} device(s); adopting board profile `{}` over `{}`",
                        profile.id,
                        current.id
                    );
                    cfg.board = profile.id.to_string();
                    store.set(esp32_agent_core::config::NVS_KEY_CONFIG, &cfg.to_json_string());
                }
            }
        }
    }
    let hw: Box<dyn esp32_agent_core::hardware::Hardware> = Box::new(hw);

    // The certificate this board will serve https with. Generated here — before the
    // radio and before the clock — because it carries fixed validity dates; see
    // `tls_server::identity`. A failure is not fatal: http stays up either way.
    let (tls_identity, tls_error) = match tls_server::identity(&mut store) {
        Ok(id) => (Some(id), String::new()),
        Err(e) => {
            log.error("boot", format!("https: no certificate ({e}); http only"));
            (None, e)
        }
    };

    let firmware = FwMeta {
        name: "esp32-agent".into(),
        version: esp32_agent_core::VERSION.into(),
        build: option_env!("ESP32_AGENT_BUILD").unwrap_or("idf").into(),
        // Written by the build scripts; empty when someone builds by hand without them,
        // which is itself information.
        commit: option_env!("ESP32_AGENT_COMMIT").unwrap_or("unknown").into(),
        built_at: option_env!("ESP32_AGENT_BUILT_AT").unwrap_or("unknown").into(),
    };
    let app: Shared = Arc::new(Mutex::new(App::new(hw, Box::new(store), log.clone(), firmware)));
    mem("after devices");

    let (mut port, plain_http) = {
        let guard = app.lock().map_err(|_| anyhow::anyhow!("app lock poisoned"))?;
        (guard.cfg.mcp.port, guard.cfg.mcp.plain_http)
    };

    // The console is the provisioning path that does not cost anybody their network.
    console::start(app.clone(), log.clone());

    // Radio first: the portal is useless without an address, and the setup AP is
    // how a phone reaches it.
    wifi::start(app.clone(), log.clone(), peripherals.modem, sys_loop, nvs)?;
    // The radio thread owns tcpip: binding a socket before it is initialised trips
    // an assert in lwIP ("Invalid mbox"). Wait until the stack publishes a state.
    for _ in 0..100 {
        if wifi::status().state != "off" {
            break;
        }
        FreeRtos::delay_ms(100);
    }
    // Captive DNS only while the access point is up; harmless afterwards.
    mem("after wifi");
    let _dns = match dns::MiniDns::start(std::net::Ipv4Addr::new(192, 168, 4, 1), 53) {
        Ok(d) => Some(d),
        Err(e) => {
            log::warn!("captive DNS on udp/53 failed: {e}");
            None
        }
    };

    let addr = format!("0.0.0.0:{port}");
    // `mcp.plain_http = false` closes port 80 and leaves only https on 443. It is refused
    // when there is no certificate to serve, because a board that answers on nothing is not
    // more secure, only unreachable: the way back in would be the USB cable.
    let plain_http = if plain_http {
        true
    } else if tls_identity.is_none() {
        log.error(
            "boot",
            "mcp.plain_http=false but no certificate is available; keeping http on, or the board would answer on no port",
        );
        true
    } else {
        false
    };
    // One accept thread serves both ports. A second listener thread for TLS does not fit:
    // with it the agent thread itself failed to start ("Not enough space"), which costs the
    // whole board to gain a second URL.
    let tls_bind = format!("0.0.0.0:{}", tls_server::TLS_PORT);
    let mut server = match tls_identity {
        Some(identity) => {
            let fingerprint = identity.fingerprint.clone();
            // https-only: the same listener thread, with a single TLS socket in it.
            let started = if plain_http {
                Server::start_with_tls(
                    &addr,
                    &tls_bind,
                    make_routes(app.clone()),
                    log.clone(),
                    identity.handshake(),
                )
            } else {
                Server::start_tls_only(
                    &tls_bind,
                    make_routes(app.clone()),
                    log.clone(),
                    identity.handshake(),
                )
            };
            match started {
                Ok(s) => {
                    if let Ok(mut guard) = app.lock() {
                        guard.tls = TlsInfo {
                            port: tls_server::TLS_PORT,
                            fingerprint: fingerprint.clone(),
                            error: String::new(),
                        };
                    }
                    log::info!("https server listening on 0.0.0.0:{}", tls_server::TLS_PORT);
                    log.info("boot", format!("https on port {} (certificate {fingerprint})", tls_server::TLS_PORT));
                    s
                }
                Err(e) => {
                    if let Ok(mut guard) = app.lock() {
                        guard.tls = TlsInfo { port: 0, fingerprint: String::new(), error: format!("{e}") };
                    }
                    log.error("boot", format!("https: could not listen on {}: {e}; http only", tls_server::TLS_PORT));
                    Server::start(&addr, make_routes(app.clone()), log.clone())?
                }
            }
        }
        None => {
            let mut server = Server::start(&addr, make_routes(app.clone()), log.clone())?;
            if !tls_error.is_empty() {
                if let Ok(mut guard) = app.lock() {
                    guard.tls = TlsInfo { port: 0, fingerprint: String::new(), error: tls_error.clone() };
                }
            }
            server
        }
    };
    log.info("boot", format!("serving the portal and /mcp on port {port}"));
    // Serial as well: "the board serves" is the line a bench check looks for, and
    // the in-RAM log cannot be read until the server that carries it is up.
    log::info!("http server listening on 0.0.0.0:{port}");
    {
        let mut guard = app.lock().map_err(|_| anyhow::anyhow!("app lock poisoned"))?;
        guard.lan_url = String::new(); // resolved from the WiFi status at runtime
    }


    // The scheduler's `at_hhmm` tasks, every message timestamp and — less obviously —
    // every TLS handshake need a wall clock: the board's RTC starts at the epoch, and a
    // certificate issued in 2024 is "not yet valid" to a clock in 1970. Starting SNTP
    // before the station has an address wastes its first attempt and the default retry is
    // an hour, which is how a board ends up unable to speak https for its whole first hour
    // while plain http works. Wait for the address first.
    for _ in 0..120 {
        if !wifi::status().ip.is_empty() && wifi::status().ip != "0.0.0.0" {
            break;
        }
        FreeRtos::delay_ms(100);
    }
    //
    // `every_s` tasks work without this, so a LAN with no route to an NTP server loses only
    // the daily-time mode and https. The handle is deliberately kept for the life of
    // `run`: dropping it stops the sync.
    let _sntp = match esp_idf_svc::sntp::EspSntp::new_default() {
        Ok(sntp) => {
            log.info("agent", "sntp: started (pool.ntp.org); at_hhmm tasks use the synced clock");
            Some(sntp)
        }
        Err(e) => {
            log.warn("agent", format!("sntp: could not start ({e}); at_hhmm tasks see the unsynced clock"));
            None
        }
    };

    // The agent runs on its own thread: one tick a second for the schedule, and the
    // queued turn if there is one. The transport has to exist before the first tick,
    // because both the model call and the `net_*` tools draw on it.
    let (agent_timeout, agent_transport) = runner::attach(&app, &log);
    // A board that cannot start the agent thread is still a working board: the
    // portal, the MCP endpoint and the tools do not depend on it. Say so where it can be
    // *seen* as well as read: `App::agent_thread_*` writes the boot log line and the value
    // `/api/status.agent.thread` reports, so the two cannot drift apart.
    match runner::start(app.clone(), log.clone(), agent_timeout, agent_transport) {
        Ok(()) => {
            if let Ok(mut guard) = app.lock() {
                guard.agent_thread_running();
            }
        }
        Err(e) => {
            if let Ok(mut guard) = app.lock() {
                guard.agent_thread_failed(e);
            }
        }
    }

    loop {
        FreeRtos::delay_ms(500);
        // `App::settle()` already persisted a pending manifest and rebuilt the
        // tool list inside the request that changed it, so the only work left
        // here is the two things that really need the rest of the system to move:
        // a restart, and a changed listen port.
        let (restart, port_now) = {
            let guard = match app.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            (guard.runtime.restart_requested, guard.cfg.mcp.port)
        };
        if restart {
            log.warn("system", "restart requested");
            FreeRtos::delay_ms(200);
            esp_idf_svc::hal::reset::restart();
        }
        if port_now != port {
            log.warn("system", format!("MCP port changed to {port_now}; restarting the server only"));
            server.stop();
            server = Server::start(&format!("0.0.0.0:{port_now}"), make_routes(app.clone()), log.clone())?;
            port = port_now;
            // The https listener went down with the old server and is not rebuilt here.
            // Say so instead of leaving `/api/tls` claiming a port that no longer answers.
            if let Ok(mut guard) = app.lock() {
                if guard.tls.enabled() {
                    guard.tls = TlsInfo {
                        port: 0,
                        fingerprint: String::new(),
                        error: "https stopped when the MCP port changed; reboot to restore it".into(),
                    };
                }
            }
        }
    }
}

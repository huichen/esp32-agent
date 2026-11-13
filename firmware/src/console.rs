//! Provision WiFi credentials over the console.
//!
//! Joining the board's setup access point from a machine that is also talking to a model
//! over the network is an outage waiting to happen; the console is already connected for
//! flashing, so the same credentials go down the same wire:
//!
//! ```text
//! provision <ssid> <password>
//! ```
//!
//! The line parser lives in `esp32_agent_core::provision`, where it has host tests. This
//! file is only the plumbing: read the console, hand the line over, persist, restart.
//!
//! A DevKitC-1 exposes two USB ports and they are not the same peripheral: the UART port is
//! UART0 on GPIO43/44, while the port next to it is the chip's own USB-Serial-JTAG. The
//! bench machine is wired to the second one, and writing there lands in the USJ FIFO — a
//! reader parked on UART0 sees nothing and the host's write times out. So both are read.
//!
//! Reading does not disturb the log driver: it writes, this reads.

use esp32_agent_core::app::Shared;
use esp32_agent_core::logbuf::LogBuf;
use esp32_agent_core::provision::{parse_line, ProvisionCmd};
use esp_idf_svc::sys;
use std::sync::Arc;

/// Start the console reader. Call once, after the radio thread exists.
pub fn start(app: Shared, log: Arc<LogBuf>) {
    // A named thread, because `ps` reads the FreeRTOS name and this one answers `provision`
    // on the console. Internal RAM: provisioning stores credentials in NVS.
    let spawned =
        crate::threads::spawn(b"provision\0", 6 * 1024, crate::threads::INTERNAL, None, move || {
            read_console(app, log)
        });
    if spawned.is_err() {
        log::error!("could not start the console provisioning thread");
    }
}

fn read_console(app: Shared, log: Arc<LogBuf>) {
    // Without this the USJ read below fails outright; a bench wired to the UART port needs
    // the UART driver instead. Exactly one of the two is read: polling a UART whose driver
    // is absent logs `uart_read_bytes: uart driver error` every few milliseconds, and that
    // flood is enough to starve the HTTP server.
    let usj = install_usj();
    let uart = !usj && install_uart0();
    log.info(
        "provision",
        match (usj, uart) {
            (true, _) => "console: reading USB-Serial-JTAG",
            (false, true) => "console: reading UART0",
            (false, false) => "console: no readable port; provisioning over the console is off",
        },
    );

    let mut line: Vec<u8> = Vec::with_capacity(128);
    let mut buf = [0u8; 64];
    loop {
        let mut n = 0;
        if usj {
            n = unsafe {
                sys::usb_serial_jtag_read_bytes(
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf.len() as u32,
                    5,
                )
            };
        }
        if n <= 0 && uart {
            n = unsafe {
                sys::uart_read_bytes(0, buf.as_mut_ptr() as *mut core::ffi::c_void, buf.len() as u32, 5)
            };
        }
        if n <= 0 {
            continue;
        }
        for &byte in &buf[..n as usize] {
            match byte {
                b'\r' | 0 => {}
                b'\n' => {
                    let text = String::from_utf8_lossy(&line).to_string();
                    line.clear();
                    if handle(&app, &log, &text) {
                        return;
                    }
                }
                0x20..=0xff => {
                    // Bytes, not chars: an SSID is routinely UTF-8.
                    if line.len() < 160 {
                        line.push(byte);
                    }
                }
                _ => line.clear(),
            }
        }
    }
}

/// Bring up the UART0 driver, which `uart_read_bytes` requires. Only used when the board is
/// wired through the UART port instead of USB-Serial-JTAG.
fn install_uart0() -> bool {
    let rc = unsafe { sys::uart_driver_install(0, 256, 0, 0, std::ptr::null_mut(), 0) };
    rc == sys::ESP_OK
}

/// Bring up the USB-Serial-JTAG driver, which `usb_serial_jtag_read_bytes` requires.
fn install_usj() -> bool {
    let cfg = sys::usb_serial_jtag_driver_config_t { tx_buffer_size: 256, rx_buffer_size: 512 };
    let rc = unsafe { sys::usb_serial_jtag_driver_install(&cfg as *const _ as *mut _) };
    rc == sys::ESP_OK
}

/// Apply one line. Returns true when the board is about to restart.
fn handle(app: &Shared, log: &Arc<LogBuf>, line: &str) -> bool {
    match parse_line(line) {
        None => false,
        Some(ProvisionCmd::Bad(why)) => {
            log.warn("provision", why.to_string());
            false
        }
        Some(ProvisionCmd::Set { ssid, password }) => {
            log.info("provision", format!("credentials for `{ssid}` received on the console"));
            let saved = match app.lock() {
                Ok(mut guard) => {
                    guard.cfg.wifi.ssid = ssid;
                    guard.cfg.wifi.password = password;
                    guard.cfg.wifi.auto_reconnect = true;
                    guard.runtime.config_dirty = true;
                    guard.settle();
                    true
                }
                Err(_) => false,
            };
            if !saved {
                log.error("provision", "could not store the credentials; the lock is poisoned");
                return false;
            }
            log.info("provision", "saved; restarting to join the network");
            std::thread::sleep(std::time::Duration::from_millis(300));
            unsafe { sys::esp_restart() };
        }
    }
}

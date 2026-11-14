//! Board-level and system tools.
//!
//! These are the tools that always exist on a working board (status, GPIO, I2C,
//! WiFi) plus the ones that manage the config. Device-specific tools live with
//! their driver in [`crate::drivers`].

pub mod bitmap;
pub mod clip;
pub mod family;
pub mod fs;
pub mod knowledge;
pub mod media;
pub mod net;
pub mod regex;
pub mod shell;

use crate::capability::{cap, CapSet};
use crate::error::McpError;
use crate::hardware::PinMode;
use crate::json as j;
use crate::tool::{boolean, enumeration, integer, object_schema, string, ToolCtx, ToolDef};
use crate::tools::family::Family;
use crate::util::hex;
use serde_json::{json, Value};

pub mod manifest_tools;

pub use manifest_tools::{manifest_members, manifest_tools};

/// Refuse a pin the board profile reserves before any hardware call touches it.
///
/// `gpio_config` on the SPI flash or PSRAM lines does not return an error — it takes the core
/// that is executing down with it, and the board reboots. The profile knows which pins those
/// are (`boards::BoardProfile::pin_problem`), and this is the one place the raw bus tools ask.
fn refuse_reserved(ctx: &ToolCtx<'_>, pin: i32) -> crate::error::Result<()> {
    match ctx.profile.pin_problem(pin) {
        Some(why) => Err(McpError::invalid_params(why).with_data(json!({
            "pin": pin,
            "reserved": ctx.profile.reserved,
            "profile": ctx.profile.id,
        }))),
        None => Ok(()),
    }
}

/// Refuse the parts of a config patch that a tool call must not be able to change.
///
/// Each one is a write with no way back from the outside:
///
/// * `pins` (and a bus's SDA/SCL, and a 1-Wire probe pin) naming a pin the profile reserves.
///   The SPI flash and PSRAM lines do not report an error when they are claimed, they take the
///   board down — and the config is persisted, so the next boot does it again. That is a brick
///   with a reboot in front of it, not a bad call.
/// * `auth.portal_pin` / `auth.mcp_token`: whoever sets one is the only one who knows it. A
///   model asked to "make the board safer" can lock the owner out of their own board, and the
///   only recovery is a serial erase. The authenticated route `POST /api/config` still accepts
///   both, so the owner can set them deliberately.
fn refuse_dangerous_config(ctx: &ToolCtx<'_>, patch: &Value) -> crate::error::Result<()> {
    if let Some(why) = crate::config::reserved_pin_problem(ctx.profile, patch) {
        return Err(McpError::invalid_params(why).with_data(json!({
            "reserved": ctx.profile.reserved,
            "profile": ctx.profile.id,
        })));
    }
    if let Some(key) = crate::config::sets_a_credential(patch) {
        return Err(McpError::forbidden(format!(
            "`auth.{key}` cannot be set from a tool call: the caller would be the only one who \
             knows it, and the only way back is erasing the board. Set it from the portal, or \
             with `POST /api/config` and the current PIN"
        )));
    }
    Ok(())
}

/// Default I2C bus index for tools that omit `bus`.
fn default_bus(ctx: &mut ToolCtx) -> u8 {
    ctx.cfg.bus.i2c.first().map(|b| b.bus).unwrap_or(0)
}

pub fn board_tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "chip_temp",
            "Read the chip's own temperature sensor, in degrees Celsius. Different from a board thermometer: this is the die, which runs warmer than the room",
        )
        .title("Chip temperature")
        .category("board")
        .bind(|ctx, _args| {
            let celsius = ctx.hw.chip_temp_c();
            Ok(json!({
                "celsius": celsius,
                "available": celsius.is_some(),
                "note": if celsius.is_some() {
                    "the die temperature, not the ambient one"
                } else {
                    "this part has no on-die sensor, or the driver did not start"
                },
            }))
        }),
        ToolDef::new(
            "media_status",
            "What the background audio player is doing: the job it is playing, how much is \
             left, how many chunks it has written, what is queued and whether anything was \
             cancelled. `speaker_play {async: true}` returns before the sound starts, so this \
             is how you find out whether it finished",
        )
        .title("Background audio")
        .category("board")
        .schema(object_schema(&[], &[]))
        .bind(|ctx, _args| {
            let state = ctx.hw.media_state();
            Ok(json!({
                "playing": state.playing,
                "job": state.job,
                "queued": state.queued,
                "duration_ms": state.duration_ms,
                "remaining_ms": state.remaining_ms,
                "rate_hz": state.rate_hz,
                "chunks": state.chunks,
                "jobs_done": state.jobs_done,
                "frames_done": state.frames_done,
                "frames_total": state.frames_total,
                "max_late_ms": state.max_late_ms,
                "cancelled": state.cancelled,
                "note": if state.playing {
                    "played by the board's own task: the caller that queued it already returned"
                } else {
                    "nothing is playing; `speaker_play` without `async` waits and needs no status"
                },
            }))
        }),
        ToolDef::new(
            "media_cancel",
            "Stop the background audio player and drop what is queued. The visible stop \
             happens within one chunk (about 170 ms at 24 kHz), not instantly",
        )
        .title("Stop background audio")
        .category("board")
        .schema(object_schema(&[], &[]))
        .bind(|ctx, _args| {
            let playing = ctx.hw.media_cancel();
            Ok(json!({
                "ok": true,
                "was_playing": playing,
                "note": if playing {
                    "the player stops after the chunk it is writing"
                } else {
                    "nothing was playing"
                },
            }))
        }),
        ToolDef::new(
            "board_status",
            "Board status: chip, firmware, uptime, die temperature, heap and internal RAM, WiFi, \
             detected device count, tool counters, what the store still owes (`pending`, `failed`, \
             `last_failure`), the agent's own turn (`run_active`, `active_tool`, `runs`, `history`, \
             `pending`), every thread's stack headroom and the memory pools. The first call on an \
             unfamiliar board",
        )
        .title("Board status")
        .category("board")
        .core()
        .schema(object_schema(&[], &[]))
        .bind(|ctx, _args| {
            let info = ctx.hw.board_info();
            let wifi = ctx.hw.wifi_status();
            let internal = ctx.hw.heap_internal();
            Ok(json!({                "device_name": ctx.cfg.device_name,
                "board_profile": ctx.profile.id,
                "board_name": ctx.profile.name,
                "soc": info.soc,
                "cores": info.cores,
                "flash_mb": info.flash_mb,
                "psram_kb": info.psram_kb,
                "firmware": ctx.runtime.firmware,
                "build": ctx.runtime.build,
                "uptime_s": ctx.runtime.uptime_s(ctx.hw.millis()),
                "temperature_c": info.temp_c,
                // `free_heap_kb` is the whole heap with PSRAM in it, which on this board
                // reads in the megabytes; the internal SRAM that sockets, TLS records and
                // thread stacks come from is a different and much smaller number. Asked
                // "how much RAM is free", a model reads the first and is wrong by three
                // orders of magnitude, so both are reported and named.
                "free_heap_kb": info.free_heap_kb,
                "min_free_heap_kb": info.min_free_heap_kb,
                "internal_kb": internal.map(|(free, _)| free / 1024),
                "largest_internal_kb": internal.map(|(_, largest)| largest / 1024),
                "wifi": {
                    "state": wifi.state,
                    "ssid": wifi.ssid,
                    "ip": wifi.ip,
                    "rssi_dbm": wifi.rssi_dbm,
                },
                "detected_devices": ctx.devices.len(),
                "capabilities": ctx.caps.to_json(),
                "session_count": ctx.runtime.sessions.len(),
                "counters": ctx.runtime.counters_json(),
                // "Did the write reach flash?" — the one reading a tool that answers "queued"
                // leaves open. Sampled by `App::settle` after every call.
                "store": ctx.runtime.store_stats,
                // The board's own turn: `run_active` and `active_tool` are live (the control
                // gate is written at exactly those moments), the counters are as of the last
                // step boundary. `mcp://` callers have no `/api/status`, so this is their only
                // view of whether the board is busy.
                "agent": {
                    "run_active": ctx.gate.active(),
                    "active_tool": ctx.gate.tool_activity().map(|t| serde_json::to_value(t).unwrap_or(Value::Null)),
                    "runs": ctx.runtime.agent["runs"].clone(),
                    "history": ctx.runtime.agent["history"].clone(),
                    "pending": ctx.runtime.agent["pending"].clone(),
                    "last_error": ctx.runtime.agent["last_error"].clone(),
                },
                // The two readings that decide whether another stack or buffer fits: what each
                // thread's stack has left, and how much RAM each capability pool can still give.
                "threads": crate::hardware::threads_json(ctx.hw.tasks()),
                "memory": {
                    "pools": serde_json::to_value(ctx.hw.heap_pools()).unwrap_or(Value::Null),
                    // The sentence a caller otherwise has to invent. Measured on the bench
                    // 2026-11-14: 77 KB free with a 31 KB largest block, and the board's own
                    // agent had to add "a big DMA buffer can fail even with 84 KB nominally
                    // free" itself. A tool that hands over `free_kb` alone invites exactly that
                    // mistake.
                    "note": "free_kb is not the budget for a buffer: largest_kb is the biggest \
                             single allocation, and that is what a new thread stack or DMA buffer \
                             has to fit in. A pool is a set of capabilities, not an address range, \
                             so the pools overlap: a thread stack comes from `internal_8bit`, a \
                             buffer a peripheral reads or writes comes from `dma`, and `spiram` \
                             supplies neither.",
                },
            }))
        }),
        ToolDef::new(
            "inventory",
            "Full hardware inventory: chip, board profile, buses scanned, every detected device with its pins/address, and addresses that answered but matched no driver",
        )
        .title("Inventory")
        .category("board")
        .core()
        .schema(object_schema(&[], &[]))
        .bind(|ctx, _args| {
            let info = ctx.hw.board_info();
            let devices: Vec<Value> = ctx
                .devices
                .infos()
                .into_iter()
                .map(|d| serde_json::to_value(d).unwrap_or_else(|_| json!({})))
                .collect();
            Ok(json!({
                "chip": {
                    "soc": info.soc,
                    "cores": info.cores,
                    "revision": info.revision,
                    "flash_mb": info.flash_mb,
                    "psram_kb": info.psram_kb,
                    "mac": info.mac,
                    "idf_version": info.idf_version,
                },
                "board_profile": {
                    "id": ctx.profile.id,
                    "name": ctx.profile.name,
                    "notes": ctx.profile.notes,
                    "reserved_pins": ctx.profile.reserved,
                },
                "capabilities": ctx.caps.to_json(),
                "buses": ctx.probe.buses,
                "devices": devices,
                "unidentified": ctx.probe.unknown,
                "candidates": ctx.probe.candidates,
                "discovery": ctx.cfg.discovery,
                "declared_pins": ctx.cfg.pins,
                "board_extra": ctx.hw.inventory_extra(),
            }))
        }),
        ToolDef::new("devices_list", "Every peripheral this board detected, with the tools each one answers to, its pins, its bus and its address")
            .title("Devices")
            .category("board")
            .core()
            .schema(object_schema(&[], &[]))
            .bind(|ctx, _args| {
                let devices: Vec<Value> = ctx
                    .devices
                    .infos()
                    .into_iter()
                    .map(|d| {
                        json!({
                            "id": d.id, "kind": d.kind, "name": d.name,
                            "bus": d.bus, "address": d.address, "pins": d.pins,
                            "caps": d.caps, "status": d.status, "note": d.note,
                            "tools": d.tools,
                        })
                    })
                    .collect();
                Ok(json!({
                    "count": devices.len(),
                    "devices": devices,
                    "caps": ctx.caps.to_json(),
                    // With `devices.discovery = manual` this is the list that matters: the
                    // drivers recognised these and adopted none. `adopt` is the exact
                    // `manifest_apply` body that would take one over.
                    "candidates": ctx.probe.candidates,
                    "discovery": ctx.cfg.discovery,
                }))
            }),
        ToolDef::new("i2c_scan", "Scan an I2C bus and list the addresses that acknowledge")
            .title("I2C scan")
            .category("bus")
            .requires(&[cap::I2C])
            .schema(object_schema(&[("bus", integer(0, 3, "bus index, default 0"))], &[]))
            .bind(|ctx, args| {
                let bus = j::optional_ranged_i64(args, "bus", 0, 3)?.map(|b| b as u8).unwrap_or_else(|| default_bus(ctx));
                let addrs = ctx.hw.i2c_scan(bus)?;
                Ok(json!({
                    "bus": bus,
                    "found": addrs.len(),
                    "addresses": addrs.iter().map(|a| format!("0x{a:02X}")).collect::<Vec<String>>(),
                }))
            }),
        ToolDef::new(
            "i2c_read",
            "Read bytes from an I2C device. Works for any chip, including ones without a dedicated driver",
        )
        .title("I2C read")
        .category("bus")
        .requires(&[cap::I2C])
        .schema(object_schema(
            &[
                ("addr", integer(1, 127, "7-bit address, 0x76 = 118")),
                ("length", integer(1, 64, "bytes to read, default 1")),
                ("reg", integer(0, 255, "register byte to write before reading")),
                ("bus", integer(0, 3, "bus index, default 0")),
            ],
            &["addr"],
        ))
        .bind(|ctx, args| {
            let addr = j::required_ranged_i64(args, "addr", 1, 127)? as u8;
            let len = j::optional_ranged_u64(args, "length", 1, 64)?.unwrap_or(1) as usize;
            let reg = j::optional_ranged_i64(args, "reg", 0, 255)?.map(|r| r as u8);
            let bus = j::optional_ranged_i64(args, "bus", 0, 3)?.map(|b| b as u8).unwrap_or_else(|| default_bus(ctx));
            let mut buf = vec![0u8; len];
            ctx.hw.i2c_read(bus, addr, reg, &mut buf)?;
            Ok(json!({
                "bus": bus,
                "addr": format!("0x{addr:02X}"),
                "reg": reg.map(|r| format!("0x{r:02X}")),
                "hex": hex(&buf),
                "bytes": buf,
            }))
        }),
        ToolDef::new("i2c_write", "Write bytes to an I2C device (raw access for undriven chips)")
            .title("I2C write")
            .category("bus")
            .requires(&[cap::I2C])
            .schema(object_schema(
                &[
                    ("addr", integer(1, 127, "7-bit address")),
                    ("reg", integer(0, 255, "register byte, omitted for command-only devices")),
                    ("data", json!({ "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 255 }, "description": "bytes to write" })),
                    ("bus", integer(0, 3, "bus index, default 0")),
                ],
                &["addr", "data"],
            ))
            .mutating()
            .bind(|ctx, args| {
                let addr = j::required_ranged_i64(args, "addr", 1, 127)? as u8;
                let reg = j::optional_ranged_i64(args, "reg", 0, 255)?.map(|r| r as u8);
                let bus = j::optional_ranged_i64(args, "bus", 0, 3)?.map(|b| b as u8).unwrap_or_else(|| default_bus(ctx));
                let data: Vec<u8> = j::arr(args, "data")
                    .ok_or_else(|| McpError::invalid_params("`data` must be an array of bytes"))?
                    .iter()
                    .enumerate()
                    .map(|(i, v)| v.as_u64().and_then(|n| u8::try_from(n).ok()).ok_or_else(||
                        McpError::invalid_params(format!("`data[{i}]` must be an integer within 0..=255"))))
                    .collect::<crate::error::Result<_>>()?;
                ctx.hw.i2c_write(bus, addr, reg, &data)?;
                Ok(json!({ "ok": true, "addr": format!("0x{addr:02X}"), "bytes": data.len() }))
            }),
        ToolDef::new("gpio_read", "Read a GPIO level as a digital input (pull-up enabled)")
            .title("GPIO read")
            .category("gpio")
            .requires(&[cap::GPIO])
            .schema(object_schema(&[("pin", integer(0, 48, "GPIO number"))], &["pin"]))
            .bind(|ctx, args| {
                let pin = j::required_ranged_i64(args, "pin", 0, 48)? as i32;
                ctx.hw.gpio_config(pin, PinMode::InputPullup)?;
                let level = ctx.hw.gpio_read(pin)?;
                Ok(json!({ "pin": pin, "level": if level { 1 } else { 0 }, "high": level }))
            }),
        ToolDef::new("gpio_write", "Drive a GPIO pin high or low")
            .title("GPIO write")
            .category("gpio")
            .requires(&[cap::GPIO])
            .schema(object_schema(
                &[
                    ("pin", integer(0, 48, "GPIO number")),
                    ("level", integer(0, 1, "1 = high, 0 = low")),
                ],
                &["pin", "level"],
            ))
            .mutating()
            .bind(|ctx, args| {
                let pin = j::required_ranged_i64(args, "pin", 0, 48)? as i32;
                let level = j::required_ranged_i64(args, "level", 0, 1)? != 0;
                refuse_reserved(ctx, pin)?;
                ctx.hw.gpio_config(pin, PinMode::Output)?;
                ctx.hw.gpio_write(pin, level)?;
                Ok(json!({ "ok": true, "pin": pin, "level": if level { 1 } else { 0 } }))
            }),
        ToolDef::new("gpio_mode", "Configure a GPIO as input, input with pull-up/pull-down, or output")
            .title("GPIO mode")
            .category("gpio")
            .requires(&[cap::GPIO])
            .schema(object_schema(
                &[
                    ("pin", integer(0, 48, "GPIO number")),
                    ("mode", enumeration(&["input", "input_pullup", "input_pulldown", "output"], "pin mode")),
                ],
                &["pin", "mode"],
            ))
            .mutating()
            .bind(|ctx, args| {
                let pin = j::required_ranged_i64(args, "pin", 0, 48)? as i32;
                refuse_reserved(ctx, pin)?;
                let m = j::str_arg(args, "mode")?;
                let mode = PinMode::parse(&m)
                    .ok_or_else(|| McpError::invalid_params(format!("unknown mode `{m}`")))?;
                ctx.hw.gpio_config(pin, mode)?;
                Ok(json!({ "ok": true, "pin": pin, "mode": m }))
            }),
        ToolDef::new("adc_read", "Read an ADC pin in millivolts")
            .title("ADC read")
            .category("sensor")
            .requires(&[cap::ADC])
            .schema(object_schema(&[("pin", integer(0, 48, "ADC-capable GPIO"))], &["pin"]))
            .bind(|ctx, args| {
                let pin = j::required_ranged_i64(args, "pin", 0, 48)? as i32;
                refuse_reserved(ctx, pin)?;
                let mv = ctx.hw.adc_mv(pin)?;
                Ok(json!({ "pin": pin, "millivolts": mv, "volts": (mv as f32 / 1000.0 * 1000.0).round() / 1000.0 }))
            }),
        ToolDef::new("wifi_scan", "Scan for visible WiFi networks")
            .title("WiFi scan")
            .category("wifi")
            .requires(&[cap::WIFI])
            .schema(object_schema(&[], &[]))
            .bind(|ctx, _args| {
                let aps = ctx.hw.wifi_scan()?;
                let list: Vec<Value> = aps
                    .iter()
                    .map(|a| json!({ "ssid": a.ssid, "rssi": a.rssi, "channel": a.channel, "auth": a.auth }))
                    .collect();
                Ok(json!({ "count": list.len(), "networks": list }))
            }),
        ToolDef::new("wifi_status", "WiFi connection state, IP address and signal strength")
            .title("WiFi status")
            .category("wifi")
            .requires(&[cap::WIFI])
            .schema(object_schema(&[], &[]))
            .bind(|ctx, _args| Ok(serde_json::to_value(ctx.hw.wifi_status()).unwrap_or_else(|_| json!({})))),
        ToolDef::new("delay", "Wait on the board (keeps the model's timing loop on-device)")
            .title("Delay")
            .category("board")
            .core()
            .schema(object_schema(&[("ms", integer(1, 5000, "milliseconds to wait"))], &["ms"]))
            .bind(|ctx, args| {
                let ms = j::ranged_i64(args, "ms", 100, 1, 5000)? as u32;
                ctx.hw.delay_us(ms * 1000);
                Ok(json!({ "ok": true, "ms": ms }))
            }),
        ToolDef::new("logs_tail", "The board's ring log, newest last: level, target, sequence number and the message, for boot and for every tool that failed")
            .title("Logs")
            .category("system")
            .core()
            .schema(object_schema(&[("n", integer(1, 200, "how many lines, default 40"))], &[]))
            .bind(|ctx, args| {
                let n = j::ranged_i64(args, "n", 40, 1, 200)? as usize;
                Ok(ctx.log.to_json(n, 0))
            }),
    ]
}

pub fn system_tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "config_set",
            "Patch the device configuration: pass only the fields to change (wifi, mcp, bus, pins, ui, auth)",
        )
        .title("Set config")
        .category("system")
        .core()
        .mutating()
        .schema(object_schema(
            &[(
                "patch",
                json!({ "type": "object", "description": "partial config, same shape as config_get" }),
            )],
            &["patch"],
        ))
        .bind(|ctx, args| {
            let patch = j::field(args, "patch").cloned().unwrap_or_else(|| args.clone());
            // Two things a *tool call* may not do, both because there is no way back from them
            // for the person who owns the board:
            //
            // * a pin the profile reserves — the SPI flash/PSRAM lines take the board down, and
            //   the config is persisted, so it takes it down at every boot afterwards;
            // * the portal PIN and the MCP token — a caller that sets one locks the portal (or
            //   every MCP client) out, and the only recovery is erasing the board over serial.
            //   Both stay reachable over `POST /api/config`, which asks for the current PIN when
            //   one is set, so the owner can still do it deliberately.
            refuse_dangerous_config(ctx, &patch)?;
            let touched = ctx.cfg.apply_patch(&patch)?;
            ctx.runtime.config_dirty = true;
            ctx.log.set_min_level(&ctx.cfg.log.level);
            ctx.log.info("config", format!("patched: {}", touched.join(", ")));
            // Only what the running system cannot re-read: the listening socket, the service
            // switch and the log ring's allocation. WiFi settings are live now — the radio
            // thread reads them every tick — so restarting for them was a reboot the board did
            // not need.
            let needs_restart = touched.iter().any(|t| {
                t == "mcp.port"
                    || t == "mcp.enabled"
                    || t == "log.ring"
                    // On top of the listener that was never bound: see `patch_config`.
                    || (t == "mcp.plain_http" && ctx.cfg.mcp.plain_http)
            });
            if needs_restart {
                ctx.runtime.restart_requested = true;
            }
            // The bus pins and the pin overrides belong to drivers that are already open, and
            // the device list is what a fresh probe rebuilds.
            if touched.iter().any(|t| {
                t == "auto_peripherals" || t == "board" || t == "bus.i2c" || t == "pins"
                    || t == "bus.onewire_pins"
            }) {
                ctx.runtime.reprobe_requested = true;
            }
            Ok(json!({ "ok": true, "touched": touched, "restart_required": needs_restart }))
        }),
        ToolDef::new(
            "schedule",
            "The board's own calendar, run without a host: `action` is list, add, remove, enable or disable. \
             A task either repeats (`every_s`), runs daily at a wall-clock time (`at_hhmm`, UTC), or fires \
             once and removes itself (`after_s`). What it does is a prompt for the board's model or a small \
             `script` that costs no tokens. A fired task lands in the same queue as a user message, so the \
             board works on its own and its answer arrives in the transcript.",
        )
        .title("Schedule")
        .category("system")
        .core()
        .mutating()
        .schema(object_schema(
            &[
                ("action", enumeration(&["list", "add", "remove", "enable", "disable"], "what to do")),
                (
                    "id",
                    string("task id: `add` uses it when given (else one is derived), the others require it"),
                ),
                ("prompt", string("what to ask the board's model when it fires")),
                ("script", string("a board script to run instead of asking the model; costs no tokens")),
                ("after_s", integer(1, 86_400, "fire once this many seconds from now, then remove the task")),
                ("every_s", integer(0, 86_400, "repeat every N s; a prompt task needs 10 or more, a script task any; 0 = use at_hhmm")),
                ("at_hhmm", string("daily \"07:30\" (UTC)")),
                ("enabled", boolean("default true")),
            ],
            &["action"],
        ))
        .bind(|ctx, args| {
            let action = j::opt_str(args, "action").unwrap_or_default();
            match action.as_str() {
                "list" => Ok(json!({
                    "tasks": ctx.tasks.iter().map(|t| json!({
                        "id": t.id,
                        "prompt": t.prompt,
                        "script": t.script,
                        "after_s": t.after_s,
                        "every_s": t.every_s,
                        "at_hhmm": t.at_hhmm,
                        "enabled": t.enabled,
                    })).collect::<Vec<_>>(),
                    "count": ctx.tasks.len(),
                })),
                "add" => {
                    if ctx.tasks.len() >= crate::app::MAX_TASKS {
                        return Err(McpError::invalid_params(format!("at most {} scheduled tasks", crate::app::MAX_TASKS)));
                    }
                    // A caller that names the task keeps that name: it can then `remove` or
                    // `enable` it without reading this response first, and adding it twice is a
                    // refusal instead of a second task nobody can address. Without a name the
                    // id is derived, and stays unique after removals.
                    let id = match j::opt_str(args, "id").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
                        Some(id) => {
                            if id.len() > 32 || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
                                return Err(McpError::invalid_params(
                                    "`id` is at most 32 characters and holds no spaces",
                                ));
                            }
                            if ctx.tasks.iter().any(|t| t.id == id) {
                                return Err(McpError::invalid_params(format!(
                                    "task `{id}` already exists: use `remove`, or `add` with another id"
                                )));
                            }
                            id
                        }
                        None => crate::app::next_schedule_task_id(ctx.tasks),
                    };
                    let mut input = args.clone();
                    input["id"] = json!(id);
                    // The tool creates a new task, so its clock always starts now. An
                    // existing REST edit/replacement may instead preserve its old clock.
                    input["last_run_ms"] = json!(crate::app::now_ms());
                    let task = crate::app::task_from_api(&input, &id)?;
                    let one_shot = task.after_s > 0;
                    ctx.tasks.push(task);
                    ctx.runtime.sched_dirty = true;
                    ctx.log.info("agent", format!("schedule: task `{id}` added from a tool call"));
                    Ok(json!({ "ok": true, "id": id, "count": ctx.tasks.len(), "one_shot": one_shot }))
                }
                "remove" | "enable" | "disable" => {
                    let id = j::opt_str(args, "id").unwrap_or_default();
                    let Some(task) = ctx.tasks.iter_mut().find(|t| t.id == id) else {
                        return Err(McpError::not_found(format!("no task `{id}`")));
                    };
                    if action == "remove" {
                        ctx.tasks.retain(|t| t.id != id);
                    } else {
                        task.enabled = action == "enable";
                    }
                    ctx.runtime.sched_dirty = true;
                    Ok(json!({ "ok": true, "id": id, "count": ctx.tasks.len() }))
                }
                other => Err(McpError::invalid_params(format!(
                    "unknown action `{other}`; use list, add, remove, enable or disable"
                ))),
            }
        }),
        ToolDef::new(
            "devices_probe",
            "Re-scan the buses and rebuild the tool list; the call returns when the scan is done, \
             so there is nothing to wait for",
        )
            .title("Re-probe devices")
            .category("system")
            .core()
            .mutating()
            .schema(object_schema(&[], &[]))
            .bind(|ctx, _args| {
                ctx.runtime.reprobe_requested = true;
                Ok(json!({ "ok": true, "note": "re-probe scheduled; call devices_list again in a moment" }))
            }),
    ]
}

/// The OpenAI `tools` array for the script tool.
///
/// One entry, because the language carries the generality: a script is how the model
/// writes the loop it needs instead of asking for a new fixed tool every time.
/// The tools the model gets but MCP does not: they need the app's own state.
///
/// Names follow the commands people already run — `read`, `write`, `edit`, `curl`, `wget`,
/// `nslookup`, `nc`, `ifconfig`, `ping` — because a model knows those and a description it
/// has to read for every one of them is a description in every request.
pub fn agent_extra_specs() -> Value {
    // Reading, writing and editing a file are the three operations frequent enough to be
    // their own tools; everything else the shell does is behind `shell`, with one command
    // line as its argument.
    crate::tool::with_argument_descriptions(json!([
        {
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read one UTF-8 text file from the board workspace, which is its own filesystem: `/` holds `MEMORY.md`, `notes/`, `skills/` and `memory/`. `shell` with `ls /` lists a directory; this tool reads a file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "file path, e.g. `/notes/a.txt`" },
                        "help": { "type": "boolean", "description": "return the usage line instead of running" },
                    },
                    "required": ["path"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "write",
                "description": format!("Create or overwrite a UTF-8 text file in the board workspace, creating parent directories. Overwrites whatever is there: use `edit` to change one place. A file is limited to {} bytes and the workspace to {} bytes of text in total. The answer is queue admission, not storage: `board status` reports the store's `pending`/`failed` for the outcome.", fs::MAX_FILE_BYTES, crate::app::WORKSPACE_TOTAL_MAX),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "file path to write" },
                        "text": { "type": "string", "description": "complete new contents" },
                        "help": { "type": "boolean", "description": "return the usage line instead of running" },
                    },
                    "required": ["path", "text"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "edit",
                "description": "Replace exactly one occurrence of `find` in a file. Fails when it occurs zero times or several, so an ambiguous edit never rewrites the wrong place. The answer is queue admission, not storage: `board status` reports the store's `pending`/`failed` for the outcome.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "file path to edit" },
                        "find": { "type": "string", "description": "literal text, must occur exactly once" },
                        "replace": { "type": "string", "description": "replacement text" },
                        "help": { "type": "boolean", "description": "return the usage line instead of running" },
                    },
                    "required": ["path", "find", "replace"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "image_read",
                "description": "Read an image the user attached to their message, by asking a vision model about it. Give a `prompt` (\"transcribe the text\" for OCR, \"describe\" for a caption) and optionally `name` when more than one is attached. This costs a second model call.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "what to ask about the image; the default is a full transcription plus a description" },
                        "name": { "type": "string", "description": "which attachment, when the message carried more than one" },
                    },
                    "required": [],
                },
            },
        },
    ]))
}

pub fn script_specs() -> Value {
    crate::tool::with_argument_descriptions(json!([{
        "type": "function",
        "function": {
            "name": "script",
            "description": "Run a small script on the board — the way to write a loop or a condition the fixed tools cannot express: `let`, assignment, `if`/`else`, `while` with `break`, `for`, `fn`, numbers, strings, bools, nil, lists, maps, and `tool(name, args)` to call any other board tool. To defer a call instead of blocking on it, use `async_tool(name, args)` and collect it later with `await(handle)`, `wait_all([handles])` or `task_state(handle)` — deferred calls still run one at a time (one dispatcher, one bus) and are drained at the end of the script if never awaited. The program's result is the value of its last expression (a map is returned as a map), and `return` ends it early. `source` or `path`: a workspace file can be run instead of pasting it in. `args` is optional: a map, or a list of words, and the program reads it back as the globals `args` and `argv`, so one script can be run with different values without being rewritten. `{\"help\": true}` (or `section`: overview|builtins|examples) returns the syntax reference; `{\"lint\": true}` checks `source` without running it. Bounded by 30 s and 5 000 000 steps.",
            "parameters": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "the program, or leave it empty and give `path`"
                    },
                    "path": {
                        "type": "string",
                        "description": "run a script file from the workspace instead of pasting `source`"
                    },
                    "lint": {
                        "type": "boolean",
                        "description": "check the syntax and unknown function names without running anything"
                    },
                    "args": {
                        "type": ["object", "array"],
                        "description": "optional: a map, or a list of words; the program reads it as `args` and `argv`"
                    },
                    "help": {
                        "type": "boolean",
                        "description": "return the syntax reference instead of running anything"
                    },
                    "section": {
                        "type": "string",
                        "enum": ["overview", "builtins", "examples"],
                        "description": "which part of the reference to return"
                    }
                },
                "additionalProperties": false
            }
        }
    }]))
}

/// The board's own agent, as tools.
///
/// These are the one class of capability that an outside agent could not reach over MCP at
/// all: the portal's Agent panel did it over REST, and everything else here assumed the caller
/// *is* the model. That is backwards for the case this board is built for — an outside agent
/// driving it — where the board's model is a resource, not the caller.
///
/// They live outside the registry for the same reason `shell` and the file tools do: the
/// handling needs `App` (the agent, the model config, the store queue), not a `ToolCtx`.
pub fn agent_specs() -> Value {
    crate::tool::with_argument_descriptions(json!([
        {
            "type": "function",
            "function": {
                "name": "agent_say",
                "description": "Ask the board's own model a question and let it run: the message is queued for exactly the turn a person's message starts, and the answer arrives in the transcript (`agent_history`). Use it when the work needs the board's model and its tools rather than yours. One message waits at a time: a second call while one is waiting replaces that text and joins its source, so check `board status` (`agent.pending`) before sending.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "what to ask; bounded like any user message" },
                    },
                    "required": ["text"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "agent_history",
                "description": "The board's transcript: role, text, tool calls and their results, oldest first. Read what the board's model was asked and answered — including turns a schedule or a person started — without needing the portal.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "after": { "type": "integer", "description": "only entries at or after this sequence number, default 0" },
                    },
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "agent_forget",
                "description": "Keep the last N question-and-answer exchanges and drop the rest, or drop everything with 0. The compressed archive goes with it. Refused while a turn is running, because the turn being answered would be dropped out from under it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "keep": { "type": "integer", "description": "exchanges to keep; 0 drops the whole conversation" },
                    },
                    "required": ["keep"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "agent_config_get",
                "description": "Where the board's model lives and how one run is bounded: base_url, model, protocol, temperature, context and output budgets, max_steps, timeout, reasoning, vision. The API key is never returned, only whether one is set.",
                "parameters": { "type": "object", "properties": {} },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "agent_config_set",
                "description": "Point the board's model at a provider, or tune the run. A partial patch: only the fields present change. `api_key` is write-only and `__set__` keeps the stored one. The answer is queue admission, not flash acknowledgement — `board status` reports the store's `pending`/`failed`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "patch": {
                            "type": "object",
                            "description": "any subset of {base_url, model, api_key, max_steps, context_tokens, max_output_tokens, temperature, timeout_s, reasoning, protocol, vision, tick_ms}",
                        },
                    },
                    "required": ["patch"],
                },
            },
        }
    ]))
}

/// What a device family is for, in one sentence.
///
/// A family is declared by the drivers that join it (`display`, `indicators`, `audio`), so the
/// sentence lives here rather than in `DeviceInfo`: the family is the board's own vocabulary,
/// and "display: 6 commands" told a caller that had never seen this board nothing it could act
/// on. A family without a sentence keeps the count, which is at least true.
pub fn family_summary(family: &str) -> Option<&'static str> {
    Some(match family {
        "display" => {
            "The OLED panel: draw text, a rectangle or a whole 1-bpp bitmap, read back what is \
             there, set contrast. `oled_read` returns text rows, not pixels: a bitmap reads back \
             as `?`"
        }
        "indicators" => "The LED and the buttons: set or blink the LED, read the buttons or wait for a press",
        "audio" => {
            "The microphone and the speaker: measure the mic level, record it into a WAV result, \
             or play a tone or a short melody through the amplifier"
        }
        _ => return None,
    })
}

/// Every tool that needs no device at all.
pub fn always_tools() -> Vec<ToolDef> {
    let mut v = board_tools();
    v.extend(system_tools());
    v.push(device_invoke_tool());
    v
}

/// Call a driver tool by device id. `tools_mode = "generic"` keeps this and
/// hides the per-device tools, so a board with 20 peripherals does not spend its
/// whole context window on tool definitions.
fn device_invoke_tool() -> ToolDef {
    ToolDef::new(
        "device_invoke",
        "Call a tool of a detected device by id — `devices_list` names the ids and the tools under them",
    )
    .title("Invoke device tool")
    .category("manifest")
    .core()
    .mutating()
    .schema(object_schema(
        &[
            ("device", crate::tool::string("device id, e.g. bme280")),
            ("tool", crate::tool::string("tool name of that device, e.g. bme280_read")),
            ("arguments", json!({ "type": "object", "description": "arguments for the device tool" })),
        ],
        &["device", "tool"],
    ))
    .bind(|ctx, args| {
        let device = j::str_arg(args, "device")?;
        let tool = j::str_arg(args, "tool")?;
        let inner = j::field(args, "arguments").cloned().unwrap_or_else(|| json!({}));
        if !inner.is_object() {
            return Err(crate::error::McpError::invalid_params("device tool arguments must be an object"));
        }
        crate::device::invoke(ctx, &device, &tool, &inner)
    })
}

/// The commands of the `board` family: the tools about the board itself, rather than about
/// a bus, a pin or a device.
///
/// One list. The declaration, the `help` text and the names the family takes over are all
/// derived from it, and `board_commands()` resolves each name against the tools that declare
/// themselves — so a rename shortens the family instead of leaving a command in `help` that
/// dispatch cannot reach.
const BOARD_COMMANDS: &[&str] = &[
    "board_status",
    "board_config_get",
    "config_set",
    "chip_temp",
    "logs_tail",
    // Neither `reboot` nor `factory_reset` is a tool any more, and neither is a command
    // here: both end the run, or wipe the board, that the caller is standing on. The
    // platform still restarts itself when a configuration change needs it
    // (`runtime.restart_requested`), and the console has its own `/api/reboot` behind the
    // portal credential — an owner acting on their machine, not a caller on the workload.
    "delay",
    "inventory",
    // The certificate the board made for itself. The portal showed it on `/api/tls` and a
    // caller that drives the board over https had no way to read the fingerprint it is asked
    // to compare — the one reading that turns a browser warning into a decision.
    "board_tls",
];

/// The commands, as tools. Each one keeps the handler it already had; only the name it is
/// reached by changes.
pub fn board_commands() -> Vec<ToolDef> {
    let mut pool = always_tools();
    pool.extend(manifest_tools::board_extra_tools());
    pick(&mut pool, BOARD_COMMANDS)
}

/// The `board` family: one tool, seven commands.
pub fn board_family() -> ToolDef {
    Family::new(
        "board",
        "The board itself: status, config, chip temperature, logs, delay, inventory. \
         Nothing here reboots the board or clears its storage; `command: help` lists each one's arguments",
        board_commands(),
    )
    .into_tool()
}

/// The names the family takes over, derived from the commands themselves.
pub fn board_members() -> Vec<String> {
    board_commands().into_iter().map(|d| d.spec.name).collect()
}

/// The commands of the `bus` family: the pins and the buses, with no driver in between.
///
/// Names stay whole inside the family (`bus i2c_read`). These are the names the manifest's
/// `exec` primitives, the shell's usage text and every document already use; shortening
/// them would save a few bytes in `help` and cost the one thing a model is likely to have
/// learned before the family existed.
const BUS_COMMANDS: &[&str] = &[
    "i2c_scan",
    "i2c_read",
    "i2c_write",
    "i2c_identify",
    "gpio_mode",
    "gpio_read",
    "gpio_write",
    "adc_read",
];

/// The `bus` family: one tool, eight commands.
pub fn bus_family() -> ToolDef {
    Family::new(
        "bus",
        "Raw pins and buses, for a chip that has no driver: I2C scan/read/write/identify, GPIO mode/read/write, and one ADC pin in millivolts; `command: help` lists each one's arguments",
        bus_commands(),
    )
    .into_tool()
}

pub fn bus_commands() -> Vec<ToolDef> {
    let mut pool = always_tools();
    pool.extend(manifest_tools::board_extra_tools());
    pick(&mut pool, BUS_COMMANDS)
}

pub fn bus_members() -> Vec<String> {
    bus_commands().into_iter().map(|d| d.spec.name).collect()
}

/// Take the named tools out of `pool`, in the order `names` gives them.
///
/// A family's command order is the order a person reads; deriving it from one list means
/// the declaration, `help` and dispatch order agree without a second place to edit.
fn pick(pool: &mut Vec<ToolDef>, names: &[&str]) -> Vec<ToolDef> {
    let mut picked = Vec::with_capacity(names.len());
    for name in names {
        if let Some(at) = pool.iter().position(|d| d.spec.name == *name) {
            picked.push(pool.remove(at));
        }
    }
    picked
}

/// Capabilities satisfied by the board rather than by a device, exposed for the
/// portal so it can show why a tool is hidden.
pub fn board_caps_json(caps: &CapSet) -> Value {
    json!({ "caps": caps.to_json() })
}

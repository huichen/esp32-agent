//! Board-level and system tools.
//!
//! These are the tools that always exist on a working board (status, GPIO, I2C,
//! WiFi) plus the ones that manage the config. Device-specific tools live with
//! their driver in [`crate::drivers`].

pub mod fs;
pub mod knowledge;
pub mod net;
pub mod shell;

use crate::capability::{cap, CapSet};
use crate::error::McpError;
use crate::hardware::PinMode;
use crate::json as j;
use crate::tool::{boolean, enumeration, integer, object_schema, string, ToolCtx, ToolDef};
use crate::util::hex;
use serde_json::{json, Value};

pub mod manifest_tools;

pub use manifest_tools::manifest_tools;

/// Default I2C bus index for tools that omit `bus`.
fn default_bus(ctx: &mut ToolCtx) -> u8 {
    ctx.cfg.bus.i2c.first().map(|b| b.bus).unwrap_or(0)
}

fn now_ms(ctx: &mut ToolCtx) -> u64 {
    let hw_ms = ctx.hw.millis();
    if hw_ms > 0 {
        hw_ms
    } else {
        ctx.log.uptime_ms()
    }
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
            "board_status",
            "Board status: chip, firmware, uptime, temperature, heap, WiFi, detected device count, tool counters",
        )
        .title("Board status")
        .category("board")
        .core()
        .schema(object_schema(&[], &[]))
        .bind(|ctx, _args| {
            let info = ctx.hw.board_info();
            let wifi = ctx.hw.wifi_status();
            let now = now_ms(ctx);
            Ok(json!({
                "device_name": ctx.cfg.device_name,
                "board_profile": ctx.profile.id,
                "board_name": ctx.profile.name,
                "soc": info.soc,
                "cores": info.cores,
                "flash_mb": info.flash_mb,
                "psram_kb": info.psram_kb,
                "firmware": ctx.runtime.firmware,
                "build": ctx.runtime.build,
                "uptime_s": ctx.runtime.uptime_s(now),
                "temperature_c": info.temp_c,
                "free_heap_kb": info.free_heap_kb,
                "min_free_heap_kb": info.min_free_heap_kb,
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
        ToolDef::new("devices_list", "List the peripherals this board detected, with their capabilities")
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
                let bus = j::opt_i64(args, "bus").map(|b| b as u8).unwrap_or_else(|| default_bus(ctx));
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
            let addr = j::ranged_i64(args, "addr", 0, 1, 127)? as u8;
            let len = j::ranged_i64(args, "length", 1, 1, 64)? as usize;
            let reg = j::opt_i64(args, "reg").map(|r| r as u8);
            let bus = j::opt_i64(args, "bus").map(|b| b as u8).unwrap_or_else(|| default_bus(ctx));
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
                let addr = j::ranged_i64(args, "addr", 0, 1, 127)? as u8;
                let reg = j::opt_i64(args, "reg").map(|r| r as u8);
                let bus = j::opt_i64(args, "bus").map(|b| b as u8).unwrap_or_else(|| default_bus(ctx));
                let data: Vec<u8> = j::arr(args, "data")
                    .ok_or_else(|| McpError::invalid_params("`data` must be an array of bytes"))?
                    .iter()
                    .map(|v| v.as_u64().unwrap_or(0).min(255) as u8)
                    .collect();
                ctx.hw.i2c_write(bus, addr, reg, &data)?;
                Ok(json!({ "ok": true, "addr": format!("0x{addr:02X}"), "bytes": data.len() }))
            }),
        ToolDef::new("gpio_read", "Read a GPIO level as a digital input (pull-up enabled)")
            .title("GPIO read")
            .category("gpio")
            .requires(&[cap::GPIO])
            .schema(object_schema(&[("pin", integer(0, 48, "GPIO number"))], &["pin"]))
            .bind(|ctx, args| {
                let pin = j::ranged_i64(args, "pin", 0, 0, 48)? as i32;
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
                let pin = j::ranged_i64(args, "pin", 0, 0, 48)? as i32;
                let level = j::ranged_i64(args, "level", 0, 0, 1)? != 0;
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
                let pin = j::ranged_i64(args, "pin", 0, 0, 48)? as i32;
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
                let pin = j::ranged_i64(args, "pin", 0, 0, 48)? as i32;
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
        ToolDef::new("logs_tail", "Read the board's recent log lines (boot and tool history)")
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
            let touched = ctx.cfg.apply_patch(&patch)?;
            ctx.runtime.config_dirty = true;
            ctx.log.info("config", format!("patched: {}", touched.join(", ")));
            let needs_restart = touched
                .iter()
                .any(|t| t.starts_with("wifi") || t == "mcp.port" || t == "mcp.enabled" || t == "bus.i2c");
            if needs_restart {
                ctx.runtime.restart_requested = true;
            }
            // Turning the board's own peripherals on or off changes the device list, which
            // only a fresh probe can do.
            if touched.iter().any(|t| t == "auto_peripherals" || t == "board") {
                ctx.runtime.reprobe_requested = true;
            }
            Ok(json!({ "ok": true, "touched": touched, "restart_required": needs_restart }))
        }),
        ToolDef::new(
            "schedule",
            "Manage the board's own schedule. action=list shows every task; add creates one that re-runs a prompt on an interval (`every_s`) or at a daily time (`at_hhmm`); remove/disable/enable take an `id`. A task fires into the same queue a user message uses, so the board works on its own",
        )
        .title("Schedule")
        .category("system")
        .core()
        .mutating()
        .schema(object_schema(
            &[
                ("action", enumeration(&["list", "add", "remove", "enable", "disable"], "what to do")),
                ("id", string("task id, for remove/enable/disable")),
                ("prompt", string("what to ask the agent when it fires")),
                ("every_s", integer(0, 86_400, "repeat every N seconds; 0 = use at_hhmm")),
                ("at_hhmm", string("daily time as \"07:30\" (UTC)")),
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
                        "every_s": t.every_s,
                        "at_hhmm": t.at_hhmm,
                        "enabled": t.enabled,
                    })).collect::<Vec<_>>(),
                    "count": ctx.tasks.len(),
                })),
                "add" => {
                    let prompt = j::opt_str(args, "prompt").unwrap_or_default();
                    if prompt.trim().is_empty() {
                        return Err(McpError::invalid_params("`prompt` is required to add a task"));
                    }
                    let every_s = j::opt_i64(args, "every_s").unwrap_or(0).clamp(0, 86_400) as u32;
                    let at_hhmm = j::opt_str(args, "at_hhmm").unwrap_or_default();
                    if every_s == 0 && at_hhmm.is_empty() {
                        return Err(McpError::invalid_params(
                            "give `every_s` or `at_hhmm`; a task with neither never fires",
                        ));
                    }
                    // The id is derived, not asked for: a model inventing one twice would
                    // silently replace its own task.
                    let id = format!("t{}", ctx.tasks.len() + 1);
                    ctx.tasks.push(crate::sched::ScheduledTask {
                        id: id.clone(),
                        prompt,
                        script: j::opt_str(args, "script").unwrap_or_default(),
                        every_s,
                        at_hhmm,
                        enabled: j::opt_bool(args, "enabled").unwrap_or(true),
                        last_run_ms: 0,
                    });
                    ctx.runtime.sched_dirty = true;
                    ctx.log.info("agent", format!("schedule: task `{id}` added from a tool call"));
                    Ok(json!({ "ok": true, "id": id, "count": ctx.tasks.len() }))
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
        ToolDef::new("devices_probe", "Re-scan the buses and rebuild the tool list")
            .title("Re-probe devices")
            .category("system")
            .core()
            .mutating()
            .schema(object_schema(&[], &[]))
            .bind(|ctx, _args| {
                ctx.runtime.reprobe_requested = true;
                Ok(json!({ "ok": true, "note": "re-probe scheduled; call devices_list again in a moment" }))
            }),
        ToolDef::new("reboot", "Restart the board")
            .title("Reboot")
            .category("system")
            .core()
            .mutating()
            .schema(object_schema(&[], &[]))
            .bind(|ctx, _args| {
                ctx.log.warn("system", "reboot requested via MCP");
                ctx.runtime.restart_requested = true;
                Ok(json!({ "ok": true, "note": "restarting" }))
            }),
        ToolDef::new("factory_reset", "Erase the stored configuration and restart with defaults")
            .title("Factory reset")
            .category("system")
            .core()
            .mutating()
            .schema(object_schema(&[("confirm", boolean("must be true"))], &["confirm"]))
            .bind(|ctx, args| {
                if !j::opt_bool(args, "confirm").unwrap_or(false) {
                    return Err(McpError::invalid_params("set confirm=true to erase the configuration"));
                }
                *ctx.cfg = crate::config::Config::default();
                ctx.runtime.config_dirty = true;
                ctx.runtime.restart_requested = true;
                ctx.log.warn("system", "factory reset: configuration erased");
                Ok(json!({ "ok": true }))
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
    json!([
        {
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a text file from the board workspace. `/` is the root.",
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
                "description": "Create or overwrite a text file in the board workspace. Parent directories are created. A file is limited to 2048 bytes and the workspace to 8 KB.",
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
                "description": "Replace exactly one occurrence of `find` in a file. Fails when it occurs zero or several times, so an ambiguous edit never rewrites the wrong place.",
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
    ])
}

pub fn script_specs() -> Value {
    json!([{
        "type": "function",
        "function": {
            "name": "script",
            "description": "Run a small script on the board — the way to write a loop or a condition the fixed tools cannot express. Syntax: `let x = 1`, `x = 2`, `l[0] = 9`, `if c { } else { }`, `while c { }` with `break`, values are numbers, strings, bools, nil, lists `[1,2]` and maps `{a: 1}`; operators `+ - * / %`, `== != < <= > >=`, `and or not`; builtins `len str num type log` and `tool(name, args)` which calls any other board tool; `#` starts a comment; the last expression is the result. Send {\"lint\": true} with `source` to check it without running, {\"help\": true} (or {\"section\": \"overview\"|\"builtins\"|\"examples\"}) for the full reference. Bounded by wall clock (30 s) and, as a backstop, 5 000 000 steps; `tool(\"script\")` is refused.",
            "parameters": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "the program"
                    },
                    "lint": {
                        "type": "boolean",
                        "description": "check the syntax and unknown function names without running anything"
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
    }])
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
        "Call a tool of a detected device by id (use devices_list to see ids and tool names)",
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
        // One gate for every entry point: if a manifest tool wraps this driver
        // action, its confirm / safety / rate-limit policy is what applies. Going
        // straight to the driver here would let a write with `confirm: true` slip
        // through without the confirmation.
        if let Some(policy) = ctx.manifest.driver_tool(&device, &tool).cloned() {
            return crate::manifest::run_tool(&policy, ctx, &inner);
        }
        ctx.devices.call(&device, &tool, &inner, ctx.hw)
    })
}

/// Capabilities satisfied by the board rather than by a device, exposed for the
/// portal so it can show why a tool is hidden.
pub fn board_caps_json(caps: &CapSet) -> Value {
    json!({ "caps": caps.to_json() })
}

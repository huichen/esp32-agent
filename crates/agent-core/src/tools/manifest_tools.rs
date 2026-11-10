//! The tools a model needs to configure hardware it has not seen before:
//! read the manifest, read its schema, identify an unknown chip, apply a new
//! manifest and call the tools that appear.
//!
//! Nothing here reboots the board. `manifest_apply` validates, stores and marks
//! the registry dirty; the app rebuilds the tool list before the next request.

use crate::config::NVS_KEY_CONFIG;
use crate::drivers::{self, CATALOG};
use crate::error::McpError;
use crate::json as j;
use crate::manifest::{
    Issue, Manifest, ValidateCtx, MANIFEST_VERSION, MAX_STEPS, MAX_TOOLS, MAX_TOTAL_DELAY_MS,
};
use crate::tool::{boolean, enumeration, integer, object_schema, ToolCtx, ToolDef};
use serde_json::{json, Value};

fn validate_ctx<'b>(ctx: &mut ToolCtx<'b>) -> ValidateCtx<'b> {
    ValidateCtx {
        caps: ctx.caps,
        buses: ctx.cfg.bus.i2c.iter().map(|b| b.bus).collect(),
        drivers: CATALOG.iter().filter(|e| e.status == "implemented").map(|e| e.kind.to_string()).collect(),
        known_device_ids: ctx.devices.ids(),
    }
}

pub fn manifest_tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "manifest_get",
            "Read the device manifest: the hardware this board is configured to expose and the tools defined on top of it",
        )
        .title("Get manifest")
        .category("manifest")
        .core()
        .schema(object_schema(&[], &[]))
        .bind(|ctx, _args| {
            Ok(json!({
                "manifest": ctx.manifest,
                "tool_names": ctx.manifest.tool_names(),
                "storage_key": crate::manifest::NVS_KEY_MANIFEST,
                "note": "This is the file you can rewrite with manifest_apply; it lives in NVS and is applied without a reboot.",
            }))
        }),
        ToolDef::new(
            "manifest_describe",
            "Everything needed to write a manifest: JSON schema, primitive exec types with their fields, driver catalogue, limits, detected hardware and worked examples",
        )
        .title("Describe manifest")
        .category("manifest")
        .core()
        .schema(object_schema(&[], &[]))
        .bind(|ctx, _args| {
            let devices = ctx.devices.infos();
            let driver_actions: Vec<Value> = devices
                .iter()
                .map(|d| json!({ "device": d.id, "driver": d.kind, "actions": d.tools, "address": d.address, "pins": d.pins }))
                .collect();
            Ok(json!({
                "manifest_schema": manifest_schema(),
                "primitives": primitives_doc(),
                "drivers": drivers::catalog_json(),
                "available_driver_actions": driver_actions,
                "limits": {
                    "max_tools": MAX_TOOLS,
                    "max_sequence_steps": MAX_STEPS,
                    "max_total_delay_ms": MAX_TOTAL_DELAY_MS,
                    "pin_range": [0, 48],
                    "i2c_address_range": [1, 127],
                },
                "current": {
                    "manifest": ctx.manifest,
                    "capabilities": ctx.caps.to_json(),
                    "buses": ctx.cfg.bus.i2c,
                    "declared_pins": ctx.cfg.pins,
                    "devices": devices,
                    "unidentified_addresses": ctx.probe.unknown,
                },
                "workflow": [
                    "1. inventory / devices_list / i2c_scan to see what is really wired.",
                    "2. i2c_identify {addr} for an address no driver claimed.",
                    "3. manifest_apply {mode:\"merge\", manifest:{...}} with the new device and tools.",
                    "4. tools/list now contains the new tools (no reboot, no reflash).",
                    "5. call the new tool; if the result looks wrong, edit the manifest and apply again.",
                ],
                "examples": examples(),
            }))
        }),
        ToolDef::new(
            "manifest_apply",
            "Validate and apply a new device manifest on the running board. mode=replace swaps the whole document, mode=merge adds/replaces entries by name. Returns validation issues instead of guessing",
        )
        .title("Apply manifest")
        .category("manifest")
        .core()
        .mutating()
        .schema(object_schema(
            &[
                ("manifest", json!({ "type": "object", "description": "the manifest document, see manifest_describe" })),
                ("mode", enumeration(&["replace", "merge"], "replace (default) or merge into the current manifest")),
                ("dry_run", boolean("validate and report, change nothing")),
            ],
            &["manifest"],
        ))
        .bind(|ctx, args| {
            let value = j::field(args, "manifest")
                .cloned()
                .ok_or_else(|| McpError::invalid_params("`manifest` is required"))?;
            let mode = j::opt_str(args, "mode").unwrap_or_else(|| "replace".into());
            let dry = j::opt_bool(args, "dry_run").unwrap_or(false);
            let patch = serde_json::from_value::<Manifest>(value)
                .map_err(|e| McpError::invalid_params(format!("manifest does not match the schema: {e}")))?;
            let candidate = if mode == "merge" { ctx.manifest.merged_with(&patch) } else { patch };
            let issues = candidate.validate(&validate_ctx(ctx));
            if !issues.is_empty() {
                return Ok(json!({
                    "applied": false,
                    "issues": issues,
                    "hint": "Fix the paths listed in `issues` and call manifest_apply again; nothing was changed.",
                }));
            }
            let before = ctx.manifest.tool_names();
            let after = candidate.tool_names();
            if dry {
                return Ok(json!({
                    "applied": false,
                    "dry_run": true,
                    "would_add": after.iter().filter(|t| !before.contains(t)).collect::<Vec<_>>(),
                    "would_remove": before.iter().filter(|t| !after.contains(t)).collect::<Vec<_>>(),
                }));
            }
            *ctx.manifest = candidate;
            ctx.runtime.manifest_dirty = true;
            ctx.runtime.config_dirty = true;
            ctx.log.info("manifest", format!("applied, {} tool(s)", after.len()));
            Ok(json!({
                "applied": true,
                "revision": ctx.runtime.manifest_rev + 1,
                "tools_added": after.iter().filter(|t| !before.contains(t)).collect::<Vec<_>>(),
                "tools_removed": before.iter().filter(|t| !after.contains(t)).collect::<Vec<_>>(),
                "tools_total": after.len(),
                "restart_required": false,
                "note": "Live now. Call tools/list to see the new tools.",
            }))
        }),
        ToolDef::new(
            "manifest_validate",
            "Validate a manifest without applying it",
        )
        .title("Validate manifest")
        .category("manifest")
        .core()
        .schema(object_schema(&[("manifest", json!({ "type": "object" }))], &["manifest"]))
        .bind(|ctx, args| {
            let value = j::field(args, "manifest")
                .cloned()
                .ok_or_else(|| McpError::invalid_params("`manifest` is required"))?;
            let m = serde_json::from_value::<Manifest>(value)
                .map_err(|e| McpError::invalid_params(format!("schema mismatch: {e}")))?;
            let issues: Vec<Issue> = m.validate(&validate_ctx(ctx));
            Ok(json!({ "valid": issues.is_empty(), "issues": issues }))
        }),
        ToolDef::new(
            "manifest_reset",
            "Clear every manifest-defined device and tool, keeping the built-in ones",
        )
        .title("Reset manifest")
        .category("manifest")
        .core()
        .mutating()
        .schema(object_schema(&[("confirm", boolean("must be true"))], &["confirm"]))
        .bind(|ctx, args| {
            if !j::opt_bool(args, "confirm").unwrap_or(false) {
                return Err(McpError::invalid_params("set confirm=true to clear the manifest"));
            }
            *ctx.manifest = Manifest::default();
            ctx.runtime.manifest_dirty = true;
            ctx.runtime.config_dirty = true;
            Ok(json!({ "ok": true, "note": "manifest cleared; built-in tools remain" }))
        }),
        ToolDef::new(
            "i2c_identify",
            "Identify what answers at an I2C address: reads the usual id registers and reports candidate drivers",
        )
        .title("Identify I2C device")
        .category("bus")
        .requires(&["i2c"])
        .schema(object_schema(
            &[
                ("addr", integer(1, 127, "7-bit address")),
                ("bus", integer(0, 3, "bus index, default 0")),
            ],
            &["addr"],
        ))
        .bind(|ctx, args| {
            let addr = j::ranged_i64(args, "addr", 0, 1, 127)? as u8;
            let bus = j::opt_i64(args, "bus")
                .map(|b| b as u8)
                .unwrap_or_else(|| ctx.cfg.bus.i2c.first().map(|b| b.bus).unwrap_or(0));
            let mut probes = Vec::new();
            for (name, reg) in [("id_0xD0", 0xD0u8), ("id_0x75", 0x75), ("id_0xFE", 0xFE), ("id_0x00", 0x00)] {
                let mut buf = [0u8; 2];
                if ctx.hw.i2c_read(bus, addr, Some(reg), &mut buf).is_ok() {
                    probes.push(json!({ "register": name, "hex": crate::util::hex(&buf) }));
                }
            }
            let mut candidates: Vec<Value> = Vec::new();
            if crate::drivers::bme280::Bme280::identify(ctx.hw, bus, addr).is_some() {
                candidates.push(json!({ "driver": "bme280", "confidence": "high", "why": "chip id 0x60/0x58" }));
            }
            if crate::drivers::motion::Mpu6050::identify(ctx.hw, bus, addr).is_some() {
                candidates.push(json!({ "driver": "mpu6050", "confidence": "high", "why": "WHO_AM_I 0x68/0x70/0x71/0x73" }));
            }
            if probes.iter().any(|p| p["register"] == "id_0xFE" && p["hex"] == "5449") {
                candidates.push(json!({ "driver": "ina219", "confidence": "high", "why": "INA226 manufacturer id 0x5449" }));
            }
            if (0x48..=0x4B).contains(&addr) {
                candidates.push(json!({ "driver": "ads1115", "confidence": "medium", "why": "address range of the ADS1115" }));
            }
            if (0x3C..=0x3D).contains(&addr) {
                candidates.push(json!({ "driver": "ssd1306", "confidence": "medium", "why": "the two usual OLED addresses; these panels cannot be read back" }));
            }
            if (0x44..=0x45).contains(&addr) {
                candidates.push(json!({ "driver": "sht3x", "confidence": "medium", "why": "SHT3x address; confirm with a measurement" }));
            }
            if (0x20..=0x27).contains(&addr) || (0x38..=0x3F).contains(&addr) {
                candidates.push(json!({ "driver": "pcf8574", "confidence": "medium", "why": "PCF8574/PCF8574A address range (LCD backpack)" }));
            }
            Ok(json!({
                "bus": bus,
                "addr": format!("0x{addr:02X}"),
                "register_probes": probes,
                "candidates": candidates,
                "next": if candidates.is_empty() {
                    json!("No driver matched. Use i2c_read with the datasheet's id register, then describe the device in the manifest with primitives.")
                } else {
                    json!("Call manifest_apply with a devices[] entry naming one of these drivers, then use its tools.")
                },
            }))
        }),
        ToolDef::new(
            "board_config_get",
            "Read runtime configuration (WiFi, MCP endpoint, buses, pins, tool style) with secrets masked",
        )
        .title("Get config")
        .category("system")
        .core()
        .schema(object_schema(&[], &[]))
        .bind(|ctx, _args| {
            let mut v = ctx.cfg.public_json();
            v["storage_keys"] = json!({ "config": NVS_KEY_CONFIG, "manifest": crate::manifest::NVS_KEY_MANIFEST });
            v["manifest_version"] = json!(MANIFEST_VERSION);
            Ok(v)
        }),
    ]
}

fn manifest_schema() -> Value {
    json!({
        "type": "object",
        "required": ["tools"],
        "additionalProperties": false,
        "properties": {
            "version": { "type": "integer", "const": MANIFEST_VERSION },
            "devices": {
                "type": "array",
                "description": "Hardware declarations. A `driver` instantiates compiled code; without it the entry only documents pins for tools.",
                "items": {
                    "type": "object",
                    "required": ["id"],
                    "properties": {
                        "id": { "type": "string", "description": "short unique id, a-z0-9_-" },
                        "kind": { "type": "string", "description": "sensor | display | actuator | board" },
                        "driver": { "type": "string", "description": "bme280, ssd1306, sht3x, ina219, mpu6050, ads1115, pcf8574, ds18b20, hcsr04, servo, relay, led" },
                        "pins": { "type": "object", "additionalProperties": { "type": "integer" } },
                        "bus": { "type": "integer" },
                        "address": { "type": "integer", "minimum": 1, "maximum": 127 },
                        "options": { "type": "object", "additionalProperties": { "type": "string" } },
                        "enabled": { "type": "boolean" },
                        "note": { "type": "string" }
                    }
                }
            },
            "tools": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "exec"],
                    "properties": {
                        "name": { "type": "string", "description": "a-z0-9_-., e.g. led.set" },
                        "title": { "type": "string" },
                        "description": { "type": "string" },
                        "device": { "type": "string" },
                        "enabled": { "type": "boolean" },
                        "params": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["name", "type"],
                                "properties": {
                                    "name": { "type": "string" },
                                    "type": { "enum": ["integer", "number", "string", "boolean"] },
                                    "description": { "type": "string" },
                                    "required": { "type": "boolean" },
                                    "min": { "type": "number" },
                                    "max": { "type": "number" },
                                    "unit": { "type": "string" },
                                    "enum": { "type": "array", "items": { "type": "string" } },
                                    "default": {}
                                }
                            }
                        },
                        "safety": {
                            "type": "object",
                            "properties": {
                                "min": { "type": "number" },
                                "max": { "type": "number" },
                                "max_calls_per_min": { "type": "integer" },
                                "confirm": { "type": "boolean" }
                            },
                            "description": "Enforced on the board before the exec runs; independent of the param ranges shown to the model."
                        },
                        "exec": { "$ref": "#/definitions/exec" }
                    }
                }
            }
        }
    })
}

fn primitives_doc() -> Value {
    json!([
        { "type": "gpio_write", "fields": { "pin": "number|{\"param\":\"name\"}", "level": "0|1|{\"param\":\"name\"}" },
          "example": { "type": "gpio_write", "pin": 48, "level": 1 } },
        { "type": "gpio_read", "fields": { "pin": "number|param" }, "example": { "type": "gpio_read", "pin": 0 } },
        { "type": "toggle", "fields": { "pin": "number|param", "times": "number|param", "gap_ms": "number|param" },
          "example": { "type": "toggle", "pin": 2, "times": 3, "gap_ms": 200 } },
        { "type": "pwm", "fields": { "pin": "number|param", "freq_hz": "number|param", "duty_pct": "number|param" },
          "example": { "type": "pwm", "pin": 7, "freq_hz": 50, "duty_pct": { "param": "angle" } } },
        { "type": "adc", "fields": { "pin": "number|param" }, "example": { "type": "adc", "pin": 1 } },
        { "type": "i2c_read", "fields": { "addr": "number|param", "reg": "number|param", "length": "number|param", "bus": "optional" },
          "example": { "type": "i2c_read", "addr": 118, "reg": 247, "length": 8 } },
        { "type": "i2c_write", "fields": { "addr": "number|param", "reg": "number|param", "data": "[bytes]|hex|\"{\\\"param\\\":\\\"name\\\"}\"" },
          "example": { "type": "i2c_write", "addr": 61, "reg": 0, "data": [0x00, 0xAF] } },
        { "type": "rgb", "fields": { "pin": "number|param", "color": "#rrggbb|param", "count": "optional" },
          "example": { "type": "rgb", "pin": 38, "color": { "param": "color" } } },
        { "type": "delay", "fields": { "ms": "number|param" }, "example": { "type": "delay", "ms": 500 } },
        { "type": "driver", "fields": { "driver": "driver id", "action": "tool name of that driver" },
          "example": { "type": "driver", "driver": "bme280", "action": "bme280_read" } },
        { "type": "sequence", "fields": { "steps": "[{ms, exec}]" },
          "example": { "type": "sequence", "steps": [
              { "ms": 100, "exec": { "type": "gpio_write", "pin": 5, "level": 1 } },
              { "ms": 100, "exec": { "type": "gpio_write", "pin": 5, "level": 0 } } ] } }
    ])
}

fn examples() -> Value {
    json!({
        "relay_with_safety": {
            "devices": [{ "id": "pump", "kind": "actuator", "pins": { "out": 12 }, "note": "5 V relay board, active low" }],
            "tools": [{
                "name": "pump.set",
                "description": "Switch the pump relay. Refuses to run more than 20 times a minute.",
                "params": [{ "name": "on", "type": "boolean", "required": true }],
                "exec": { "type": "gpio_write", "pin": 12, "level": { "param": "on" } },
                "safety": { "max_calls_per_min": 20 }
            }]
        },
        "servo_angle": {
            "tools": [{
                "name": "servo.angle",
                "description": "Servo angle in degrees",
                "params": [{ "name": "deg", "type": "number", "min": 0, "max": 180, "unit": "deg", "required": true }],
                "exec": { "type": "pwm", "pin": 7, "freq_hz": 50,
                          "duty_pct": { "param": "deg" } },
                "safety": { "min": 0, "max": 180 }
            }]
        },
        "sensor_driver": {
            "devices": [{ "id": "bme280", "kind": "sensor", "driver": "bme280", "bus": 0, "address": 118 }],
            "tools": [{ "name": "climate.read", "device": "bme280", "description": "Temperature, pressure and humidity",
                        "params": [], "exec": { "type": "driver", "driver": "bme280", "action": "bme280_read" } }]
        },
        "led_then_read_back": {
            "tools": [{
                "name": "led.probe",
                "description": "Blink the LED, then report the ADC level of a light sensor",
                "params": [],
                "exec": { "type": "sequence", "steps": [
                    { "ms": 120, "exec": { "type": "rgb", "pin": 38, "color": "#ff0000" } },
                    { "ms": 0, "exec": { "type": "rgb", "pin": 38, "color": "#000000" } },
                    { "ms": 0, "exec": { "type": "adc", "pin": 1 } } ] }
            }]
        }
    })
}

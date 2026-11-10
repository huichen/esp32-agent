//! Output-side peripherals: servo, relay, addressable LED and buttons.
//!
//! None of these can announce themselves, so they come from the board profile or
//! from `pins` in the config (`relay.0`, `servo.0`, `button.0`, `led`).

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::json as j;
use crate::tool::{boolean, integer, number, object_schema, string, ToolDef};
use serde_json::{json, Value};

/// A hobby servo driven with 50 Hz PWM.
pub struct Servo {
    pub pin: i32,
    pub min_us: u32,
    pub max_us: u32,
    id: String,
    angle: f32,
}

impl Servo {
    pub fn new(pin: i32) -> Self {
        Self { pin, min_us: 500, max_us: 2500, id: "servo".into(), angle: 0.0 }
    }

    pub fn set_angle(&mut self, hw: &mut dyn Hardware, angle: f32) -> Result<()> {
        let angle = angle.clamp(0.0, 180.0);
        let period_us = 20_000.0;
        let width = self.min_us as f32 + (self.max_us - self.min_us) as f32 * angle / 180.0;
        let duty = width / period_us * 100.0;
        hw.pwm_set(self.pin, 50, duty)?;
        self.angle = angle;
        Ok(())
    }
}

impl Device for Servo {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("servo", &format!("Servo on GPIO{}", self.pin), "pwm");
        i.id = self.id.clone();
        i.pins = vec![self.pin];
        i.caps = vec!["servo".into(), "pwm".into()];
        i.source = "config".into();
        i.detail = json!({ "pulse_us": [self.min_us, self.max_us], "angle": self.angle });
        i.tools = vec!["servo_angle".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("servo_angle", "Move a hobby servo to an angle in degrees")
            .title("Servo")
            .category("gpio")
            .requires(&["servo"])
            .schema(object_schema(&[("angle", number(0.0, 180.0, "0..=180 degrees"))], &["angle"]))
            .mutating()
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "servo_angle", args, ctx.hw)
            })]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "servo_angle" => {
                let a = j::f64_arg(args, "angle")? as f32;
                self.set_angle(hw, a)?;
                Ok(json!({ "ok": true, "angle": self.angle, "pin": self.pin }))
            }
            other => Err(McpError::not_found(format!("servo has no tool `{other}`"))),
        }
    }
}

/// A relay board channel.
pub struct Relay {
    pub pin: i32,
    pub active_low: bool,
    id: String,
    on: bool,
}

impl Relay {
    pub fn new(pin: i32, active_low: bool) -> Self {
        Self { pin, active_low, id: format!("relay_p{pin}"), on: false }
    }

    pub fn set(&mut self, hw: &mut dyn Hardware, on: bool) -> Result<()> {
        hw.gpio_config(self.pin, PinMode::Output)?;
        hw.gpio_write(self.pin, if self.active_low { !on } else { on })?;
        self.on = on;
        Ok(())
    }
}

impl Device for Relay {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("relay", &format!("Relay on GPIO{}", self.pin), "gpio");
        i.id = self.id.clone();
        i.pins = vec![self.pin];
        i.caps = vec!["relay".into(), "gpio".into()];
        i.source = "config".into();
        i.detail = json!({ "active_low": self.active_low, "on": self.on });
        i.tools = vec!["relay_set".into(), "relay_pulse".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let pulse = id.clone();
        vec![
            ToolDef::new("relay_set", "Switch a relay channel on or off")
                .title("Relay")
                .category("gpio")
                .requires(&["relay"])
                .schema(object_schema(&[("on", boolean("true = energised"))], &["on"]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = id.clone();
                    ctx.devices.call(&id, "relay_set", args, ctx.hw)
                }),
            ToolDef::new("relay_pulse", "Energise a relay for a number of milliseconds, then release")
                .title("Relay pulse")
                .category("gpio")
                .requires(&["relay"])
                .schema(object_schema(&[("ms", integer(10, 10_000, "pulse length"))], &["ms"]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = pulse.clone();
                    ctx.devices.call(&id, "relay_pulse", args, ctx.hw)
                }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "relay_set" => {
                let on = j::bool_arg(args, "on")?;
                self.set(hw, on)?;
                Ok(json!({ "ok": true, "on": on, "pin": self.pin }))
            }
            "relay_pulse" => {
                let ms = j::ranged_i64(args, "ms", 100, 10, 10_000)? as u32;
                self.set(hw, true)?;
                hw.delay_us(ms * 1000);
                self.set(hw, false)?;
                Ok(json!({ "ok": true, "ms": ms, "pin": self.pin }))
            }
            other => Err(McpError::not_found(format!("relay has no tool `{other}`"))),
        }
    }
}

/// The onboard RGB LED: a WS2812 pixel, a plain GPIO LED or PWM dimming.
pub struct RgbLed {
    pub pin: i32,
    pub kind: &'static str,
    pub count: u16,
    id: String,
}

impl RgbLed {
    pub fn new(pin: i32, kind: &'static str, count: u16) -> Self {
        Self { pin, kind, count: count.max(1), id: "led".into() }
    }

    fn apply(&mut self, hw: &mut dyn Hardware, rgb: (u8, u8, u8)) -> Result<()> {
        match self.kind {
            "ws2812" => hw.ring_set(self.pin, rgb, self.count),
            "pwm" => {
                let duty = (rgb.0 as f32 + rgb.1 as f32 + rgb.2 as f32) / 3.0 / 255.0 * 100.0;
                hw.pwm_set(self.pin, 5_000, duty)
            }
            _ => {
                let on = rgb.0 > 8 || rgb.1 > 8 || rgb.2 > 8;
                hw.gpio_config(self.pin, PinMode::Output)?;
                hw.gpio_write(self.pin, on)
            }
        }
    }
}

impl Device for RgbLed {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("led", &format!("{} LED on GPIO{}", self.kind, self.pin), "gpio");
        i.id = self.id.clone();
        i.pins = vec![self.pin];
        i.caps = vec!["led".into(), "gpio".into()];
        i.detail = json!({ "kind": self.kind, "count": self.count });
        i.tools = vec!["led_set".into(), "led_blink".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let blink = id.clone();
        vec![
            ToolDef::new("led_set", "Set the LED colour (#rrggbb)")
                .title("LED colour")
                .category("gpio")
                .requires(&["led"])
                .schema(object_schema(&[("color", string("#rrggbb, or an [r,g,b] array"))], &["color"]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = id.clone();
                    ctx.devices.call(&id, "led_set", args, ctx.hw)
                }),
            ToolDef::new("led_blink", "Blink the LED a few times and leave it off")
                .title("LED blink")
                .category("gpio")
                .requires(&["led"])
                .schema(object_schema(
                    &[
                        ("color", string("#rrggbb")),
                        ("times", integer(1, 20, "blink count, default 3")),
                        ("ms", integer(20, 1000, "half period, default 120")),
                    ],
                    &["color"],
                ))
                .mutating()
                .bind(move |ctx, args| {
                    let id = blink.clone();
                    ctx.devices.call(&id, "led_blink", args, ctx.hw)
                }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "led_set" => {
                let rgb = j::color(args, "color")?;
                self.apply(hw, rgb)?;
                Ok(json!({ "ok": true, "color": format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2) }))
            }
            "led_blink" => {
                let rgb = j::color(args, "color")?;
                let times = j::ranged_i64(args, "times", 3, 1, 20)?;
                let ms = j::ranged_i64(args, "ms", 120, 20, 1000)? as u32;
                for _ in 0..times {
                    self.apply(hw, rgb)?;
                    hw.delay_us(ms * 1000);
                    self.apply(hw, (0, 0, 0))?;
                    hw.delay_us(ms * 1000);
                }
                Ok(json!({ "ok": true, "times": times, "ms": ms }))
            }
            other => Err(McpError::not_found(format!("led has no tool `{other}`"))),
        }
    }
}

/// One or more push buttons on GPIO with an internal pull-up.
pub struct Buttons {
    pins: Vec<(String, i32)>,
    id: String,
}

impl Buttons {
    pub fn new(pins: Vec<(String, i32)>) -> Self {
        Self { pins, id: "buttons".into() }
    }

    pub fn init(&self, hw: &mut dyn Hardware) -> Result<()> {
        for (_, pin) in &self.pins {
            hw.gpio_config(*pin, PinMode::InputPullup)?;
        }
        Ok(())
    }

    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<Vec<(String, bool)>> {
        let mut out = Vec::with_capacity(self.pins.len());
        for (name, pin) in &self.pins {
            out.push((name.clone(), !hw.gpio_read(*pin)?));
        }
        Ok(out)
    }
}

impl Device for Buttons {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("buttons", &format!("{} push button(s)", self.pins.len()), "gpio");
        i.id = self.id.clone();
        i.pins = self.pins.iter().map(|(_, p)| *p).collect();
        i.caps = vec!["button".into(), "gpio".into()];
        i.detail = json!({ "names": self.pins.iter().map(|(n, p)| json!([n, p])).collect::<Vec<Value>>() });
        i.tools = vec!["buttons_read".into(), "button_wait".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let wait = id.clone();
        vec![
            ToolDef::new("buttons_read", "Read all push buttons (true = pressed)")
                .title("Buttons")
                .category("gpio")
                .requires(&["button"])
                .schema(object_schema(&[], &[]))
                .bind(move |ctx, args| {
                    let id = id.clone();
                    ctx.devices.call(&id, "buttons_read", args, ctx.hw)
                }),
            ToolDef::new("button_wait", "Block until a button is pressed (board-side timing)")
                .title("Wait for button")
                .category("gpio")
                .requires(&["button"])
                .schema(object_schema(
                    &[
                        ("timeout_ms", integer(10, 60_000, "give up after this long, default 5000")),
                        ("name", string("only this button, default any")),
                    ],
                    &[],
                ))
                .bind(move |ctx, args| {
                    let id = wait.clone();
                    ctx.devices.call(&id, "button_wait", args, ctx.hw)
                }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "buttons_read" => {
                let v = self.read(hw)?;
                let mut map = serde_json::Map::new();
                for (name, pressed) in v {
                    map.insert(name, json!(pressed));
                }
                Ok(Value::Object(map))
            }
            "button_wait" => {
                let timeout = j::ranged_i64(args, "timeout_ms", 5_000, 10, 60_000)? as u32;
                let which = j::opt_str(args, "name");
                let start = hw.millis();
                loop {
                    for (name, pin) in &self.pins {
                        if which.as_ref().map(|w| w != name).unwrap_or(false) {
                            continue;
                        }
                        if !hw.gpio_read(*pin)? {
                            return Ok(json!({
                                "pressed": true,
                                "button": name,
                                "pin": pin,
                                "elapsed_ms": hw.millis().saturating_sub(start),
                            }));
                        }
                    }
                    if hw.millis().saturating_sub(start) >= timeout as u64 {
                        return Ok(json!({ "pressed": false, "button": null, "elapsed_ms": timeout }));
                    }
                    hw.delay_us(3_000);
                }
            }
            other => Err(McpError::not_found(format!("buttons have no tool `{other}`"))),
        }
    }
}

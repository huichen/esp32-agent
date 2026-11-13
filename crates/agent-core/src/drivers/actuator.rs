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
            ToolDef::new("led_set", "Set the LED colour (#rrggbb); nothing reads it back")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;
    use std::collections::VecDeque;

    /// A board with pins, PWM channels and one addressable-LED strip, and a clock that only
    /// moves when the driver waits. Nothing here re-implements the driver: the log is what
    /// the driver actually asked the hardware to do, in order.
    #[derive(Default)]
    struct Board {
        /// Pin levels the driver reads back.
        reads: VecDeque<bool>,
        read_default: bool,
        writes: Vec<(i32, bool)>,
        configs: Vec<(i32, PinMode)>,
        pwm: Vec<(i32, u32, f32)>,
        ring: Vec<(i32, (u8, u8, u8), u16)>,
        clock_us: u64,
        pwm_err: Option<McpError>,
        write_err: Option<McpError>,
        read_err: Option<McpError>,
        ring_err: Option<McpError>,
    }

    impl Hardware for Board {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, pin: i32, mode: PinMode) -> Result<()> {
            self.configs.push((pin, mode));
            Ok(())
        }

        fn gpio_read(&mut self, _pin: i32) -> Result<bool> {
            if let Some(e) = &self.read_err {
                return Err(e.clone());
            }
            Ok(self.reads.pop_front().unwrap_or(self.read_default))
        }

        fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
            if let Some(e) = &self.write_err {
                return Err(e.clone());
            }
            self.writes.push((pin, high));
            Ok(())
        }

        fn pwm_set(&mut self, pin: i32, freq_hz: u32, duty_pct: f32) -> Result<()> {
            if let Some(e) = &self.pwm_err {
                return Err(e.clone());
            }
            self.pwm.push((pin, freq_hz, duty_pct));
            Ok(())
        }

        fn ring_set(&mut self, pin: i32, rgb: (u8, u8, u8), count: u16) -> Result<()> {
            if let Some(e) = &self.ring_err {
                return Err(e.clone());
            }
            self.ring.push((pin, rgb, count));
            Ok(())
        }

        fn delay_us(&mut self, us: u32) {
            self.clock_us += us as u64;
        }

        fn millis(&mut self) -> u64 {
            self.clock_us / 1000
        }
    }

    fn near(got: f32, want: f32) -> bool {
        (got - want).abs() < 1e-4
    }

    /// A 50 Hz hobby servo: 500 us is 0 degrees, 2500 us is 180, and the 20 ms period turns
    /// the pulse width into a duty cycle. Getting this wrong strips a gearbox.
    #[test]
    fn servo_angles_become_the_datasheet_pulse_widths() {
        let mut hw = Board::default();
        let mut s = Servo::new(4);
        for (angle, duty) in [(0.0, 2.5), (90.0, 7.5), (180.0, 12.5)] {
            s.set_angle(&mut hw, angle).expect("set_angle");
            let (pin, freq, d) = *hw.pwm.last().expect("a PWM write");
            assert_eq!((pin, freq), (4, 50), "a hobby servo is 50 Hz");
            assert!(near(d, duty), "{angle} degrees is {duty}% duty, got {d}");
        }
    }

    /// An angle outside the mechanism's range is clamped, not refused: a model that asks for
    /// 200 degrees gets a servo at its end stop, and the reported angle is the real one.
    #[test]
    fn a_servo_angle_outside_the_range_is_clamped() {
        let mut hw = Board::default();
        let mut s = Servo::new(4);
        s.set_angle(&mut hw, 400.0).expect("clamped");
        assert!(near(s.angle, 180.0), "reported angle {}", s.angle);
        s.set_angle(&mut hw, -40.0).expect("clamped");
        assert!(near(s.angle, 0.0));
    }

    #[test]
    fn a_servo_that_cannot_be_driven_keeps_its_last_angle() {
        let mut hw =
            Board { pwm_err: Some(McpError::unsupported("PWM is not available")), ..Default::default() };
        let mut s = Servo::new(4);
        let err = s.set_angle(&mut hw, 90.0).expect_err("no PWM, no movement");
        assert_eq!(err.code, crate::error::code::UNSUPPORTED);
        assert!(near(s.angle, 0.0), "the reported angle must not move if the pin did not");
    }

    #[test]
    fn the_servo_tool_takes_degrees_and_reports_where_it_went() {
        let mut hw = Board::default();
        let mut s = Servo::new(4);
        let v = s.call("servo_angle", &json!({ "angle": 90 }), &mut hw).expect("angle");
        assert_eq!(v["ok"], true);
        assert_eq!(v["angle"], 90.0);
        assert_eq!(v["pin"], 4);

        let err = s.call("servo_angle", &json!({}), &mut hw).expect_err("angle is required");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        let err = s.call("servo_move", &json!({ "angle": 1 }), &mut hw).expect_err("not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);

        let info = s.info();
        assert_eq!(info.kind, "servo");
        assert_eq!(info.bus, "pwm");
        assert_eq!(info.pins, vec![4]);
        assert_eq!(info.source, "config");
        assert_eq!(info.detail["pulse_us"], json!([500, 2500]));
        assert_eq!(info.detail["angle"], 90.0);
        assert_eq!(info.tools, vec!["servo_angle"]);
    }

    /// A relay board is usually active low: the pin has to be driven *low* to energise the
    /// coil, which is the opposite of what a caller asking for `on: true` expects.
    #[test]
    fn an_active_low_relay_inverts_the_pin() {
        let mut hw = Board::default();
        let mut r = Relay::new(5, true);
        r.set(&mut hw, true).expect("on");
        r.set(&mut hw, false).expect("off");
        assert_eq!(hw.writes, vec![(5, false), (5, true)], "active low: on is a low pin");
        assert_eq!(hw.configs, vec![(5, PinMode::Output), (5, PinMode::Output)], "configured for each write");
    }

    #[test]
    fn an_active_high_relay_follows_the_request() {
        let mut hw = Board::default();
        let mut r = Relay::new(5, false);
        r.set(&mut hw, true).expect("on");
        assert_eq!(hw.writes, vec![(5, true)]);
        assert_eq!(r.info().detail["on"], true);
        assert_eq!(r.info().detail["active_low"], false);

        let err = r.call("relay_set", &json!({}), &mut hw).expect_err("on is required");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        let v = r.call("relay_set", &json!({ "on": true }), &mut hw).expect("on");
        assert_eq!(v["on"], true);
        assert_eq!(v["pin"], 5);
        let err = r.call("relay_toggle", &json!({}), &mut hw).expect_err("not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
    }

    /// The pulse has to come back off: leaving a relay energised is how a pump floods a
    /// floor. The delay between the two writes is the pulse length.
    #[test]
    fn a_relay_pulse_energises_then_releases() {
        let mut hw = Board::default();
        let mut r = Relay::new(5, true);
        let v = r.call("relay_pulse", &json!({ "ms": 250 }), &mut hw).expect("pulse");
        assert_eq!(v["ms"], 250);
        assert_eq!(hw.writes, vec![(5, false), (5, true)], "on then off, inverted by the wiring");
        assert_eq!(hw.clock_us, 250_000, "the pulse is held for the requested time");
        assert!(!r.on, "and the relay is left released");
    }

    #[test]
    fn a_pulse_shorter_than_the_minimum_is_refused() {
        let mut hw = Board::default();
        let mut r = Relay::new(5, false);
        let err = r.call("relay_pulse", &json!({ "ms": 5 }), &mut hw).expect_err("5 ms is not a pulse");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        assert_eq!(hw.writes.len(), 0, "a refused pulse must not energise the coil");
        // The default is 100 ms, which is inside the range the schema documents.
        let v = r.call("relay_pulse", &json!({}), &mut hw).expect("default pulse");
        assert_eq!(v["ms"], 100);
    }

    #[test]
    fn a_gpio_failure_leaves_the_relay_state_alone() {
        let mut hw =
            Board { write_err: Some(McpError::hardware("GPIO 5 is not writable")), ..Default::default() };
        let mut r = Relay::new(5, false);
        let err = r.set(&mut hw, true).expect_err("a pin that will not move");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(!r.on, "the reported state follows the pin, not the request");
    }

    /// A WS2812 strip takes the whole colour and the pixel count in one call.
    #[test]
    fn a_ws2812_led_sends_the_colour_to_the_strip() {
        let mut hw = Board::default();
        let mut led = RgbLed::new(4, "ws2812", 8);
        let v = led.call("led_set", &json!({ "color": "#ff8000" }), &mut hw).expect("set");
        assert_eq!(v["color"], "#ff8000");
        assert_eq!(hw.ring, vec![(4, (255, 128, 0), 8)]);
        assert_eq!(hw.writes.len(), 0, "a WS2812 is not bit-banged through gpio here");
    }

    /// A pixel count of zero would address no LEDs at all, so it is clamped to one.
    #[test]
    fn an_led_count_of_zero_still_lights_one_pixel() {
        let mut hw = Board::default();
        let mut led = RgbLed::new(4, "ws2812", 0);
        assert_eq!(led.count, 1);
        led.call("led_set", &json!({ "color": [10, 20, 30] }), &mut hw).expect("set");
        assert_eq!(hw.ring, vec![(4, (10, 20, 30), 1)]);
        assert_eq!(led.info().detail["count"], 1);
    }

    /// A plain LED has no colour: it is on above the threshold the driver uses to mean
    /// "bright enough to see", and off below it.
    #[test]
    fn a_plain_led_is_on_or_off_by_threshold() {
        let mut hw = Board::default();
        let mut led = RgbLed::new(4, "gpio", 1);
        led.call("led_set", &json!({ "color": "#090909" }), &mut hw).expect("bright");
        assert_eq!(hw.writes.last(), Some(&(4, true)));
        led.call("led_set", &json!({ "color": "#080808" }), &mut hw).expect("dim");
        assert_eq!(hw.writes.last(), Some(&(4, false)), "8 of 255 is not visible on a plain LED");
        assert_eq!(hw.configs.last(), Some(&(4, PinMode::Output)));
    }

    /// PWM dimming averages the three channels on a 5 kHz carrier, which is above the flicker
    /// most people notice.
    #[test]
    fn a_pwm_led_dims_by_the_average_of_the_channels() {
        let mut hw = Board::default();
        let mut led = RgbLed::new(4, "pwm", 1);
        led.call("led_set", &json!({ "color": "#ffffff" }), &mut hw).expect("full");
        let (pin, freq, duty) = *hw.pwm.last().expect("a PWM write");
        assert_eq!((pin, freq), (4, 5_000));
        assert!(near(duty, 100.0), "white is full duty, got {duty}");

        led.call("led_set", &json!({ "color": "#ff0000" }), &mut hw).expect("red");
        assert!(near(hw.pwm.last().expect("a write").2, 100.0 / 3.0), "one channel of three");
    }

    #[test]
    fn a_blink_ends_with_the_led_off_whatever_the_colour() {
        let mut hw = Board::default();
        let mut led = RgbLed::new(4, "gpio", 1);
        let v = led
            .call("led_blink", &json!({ "color": "#ffffff", "times": 2, "ms": 20 }), &mut hw)
            .expect("blink");
        assert_eq!(v["times"], 2);
        assert_eq!(v["ms"], 20);
        assert_eq!(
            hw.writes,
            vec![(4, true), (4, false), (4, true), (4, false)],
            "two on/off pairs, and the last thing it did was turn it off"
        );
        assert_eq!(hw.clock_us, 80_000, "four half-periods of 20 ms");

        let err = led
            .call("led_blink", &json!({ "color": "#ffffff", "times": 21 }), &mut hw)
            .expect_err("21 blinks is out of range");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
    }

    #[test]
    fn a_colour_that_is_not_a_colour_is_refused() {
        let mut hw = Board::default();
        let mut led = RgbLed::new(4, "gpio", 1);
        for bad in [json!("red"), json!("#ff00"), json!("zzzzzz")] {
            let err = led.call("led_set", &json!({ "color": bad }), &mut hw).expect_err("not a colour");
            assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        }
        assert_eq!(hw.writes.len(), 0, "a refused colour must not touch the pin");
        let err = led.call("led_fade", &json!({ "color": "#ffffff" }), &mut hw).expect_err("not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
    }

    /// No LED path can be probed, so the inventory says where it came from: the config.
    #[test]
    fn the_led_inventory_row_names_the_kind() {
        let led = RgbLed::new(4, "ws2812", 8);
        let info = led.info();
        assert_eq!(info.kind, "led");
        assert_eq!(info.bus, "gpio");
        assert_eq!(info.pins, vec![4]);
        assert!(info.name.contains("ws2812") && info.name.contains("GPIO4"), "{}", info.name);
        assert_eq!(info.detail["kind"], "ws2812");
        assert_eq!(info.tools, vec!["led_set", "led_blink"]);
    }

    /// Buttons are wired to ground with a pull-up, so a pressed button reads low and the
    /// driver inverts it. A driver that forgot the inversion would report every idle board
    /// as pressed.
    #[test]
    fn buttons_are_pressed_when_the_pin_is_low() {
        let mut hw = Board { reads: VecDeque::from(vec![true, false]), ..Default::default() };
        let buttons = Buttons::new(vec![("a".into(), 4), ("b".into(), 5)]);
        buttons.init(&mut hw).expect("init");
        assert_eq!(
            hw.configs,
            vec![(4, PinMode::InputPullup), (5, PinMode::InputPullup)],
            "every button pin is a pull-up input"
        );
        let mut buttons = buttons;
        let v = buttons.read(&mut hw).expect("read");
        assert_eq!(v, vec![("a".to_string(), false), ("b".to_string(), true)]);
    }

    #[test]
    fn the_read_tool_returns_a_map_of_pressed_buttons() {
        let mut hw = Board { reads: VecDeque::from(vec![false, true]), ..Default::default() };
        let mut buttons = Buttons::new(vec![("left".into(), 4), ("right".into(), 5)]);
        let v = buttons.call("buttons_read", &json!({}), &mut hw).expect("read");
        assert_eq!(v, json!({ "left": true, "right": false }));

        let err = buttons.call("button_press", &json!({}), &mut hw).expect_err("not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);

        let info = buttons.info();
        assert_eq!(info.kind, "buttons");
        assert_eq!(info.pins, vec![4, 5]);
        assert!(info.name.contains('2'), "{}", info.name);
        assert_eq!(info.detail["names"], json!([["left", 4], ["right", 5]]));
        assert_eq!(info.tools, vec!["buttons_read", "button_wait"]);
    }

    /// The wait reports which button was pressed and how long it took, because the model
    /// cannot see the board's clock and would otherwise invent a duration.
    #[test]
    fn waiting_for_a_button_reports_the_name_and_the_elapsed_time() {
        let mut hw = Board { reads: VecDeque::from(vec![true, false]), ..Default::default() };
        let mut buttons = Buttons::new(vec![("left".into(), 4), ("right".into(), 5)]);
        let v = buttons.call("button_wait", &json!({ "timeout_ms": 1_000 }), &mut hw).expect("wait");
        assert_eq!(v["pressed"], true);
        assert_eq!(v["button"], "right");
        assert_eq!(v["pin"], 5);
        assert_eq!(v["elapsed_ms"], 0, "it was already pressed on the first scan");
    }

    /// Asking for one button must not be satisfied by another, and the board-side loop has to
    /// give up on its own timeout rather than spin.
    #[test]
    fn waiting_for_one_named_button_polls_only_that_pin() {
        let mut hw = Board { reads: VecDeque::from(vec![true, false]), ..Default::default() };
        let mut buttons = Buttons::new(vec![("left".into(), 4), ("right".into(), 5)]);
        let v =
            buttons.call("button_wait", &json!({ "timeout_ms": 10, "name": "left" }), &mut hw).expect("wait");
        assert_eq!(v["pressed"], true);
        assert_eq!(v["button"], "left");
        assert_eq!(v["elapsed_ms"], 3, "one 3 ms poll before the press");

        let mut quiet = Board { read_default: true, ..Default::default() };
        let v = buttons.call("button_wait", &json!({ "timeout_ms": 10 }), &mut quiet).expect("wait");
        assert_eq!(v["pressed"], false);
        assert_eq!(v["button"], Value::Null);
        assert_eq!(v["elapsed_ms"], 10, "the timeout it was given");
        assert!(quiet.clock_us >= 10_000, "it actually waited");
    }

    #[test]
    fn a_timeout_outside_the_range_is_refused_before_polling() {
        let mut hw = Board { read_default: true, ..Default::default() };
        let mut buttons = Buttons::new(vec![("a".into(), 4)]);
        let err = buttons.call("button_wait", &json!({ "timeout_ms": 5 }), &mut hw).expect_err("too short");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
    }

    #[test]
    fn a_pin_that_cannot_be_read_fails_the_wait_instead_of_reporting_a_press() {
        let mut hw =
            Board { read_err: Some(McpError::hardware("GPIO 4 is not readable")), ..Default::default() };
        let mut buttons = Buttons::new(vec![("a".into(), 4)]);
        let err = buttons.call("button_wait", &json!({}), &mut hw).expect_err("a broken pin is not a press");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        let err = buttons.call("buttons_read", &json!({}), &mut hw).expect_err("nor a reading");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    /// A strip that cannot be addressed has to report the error, not swallow it: the caller
    /// asked for a colour and the LEDs are still dark.
    #[test]
    fn a_strip_that_cannot_be_addressed_reports_the_error() {
        let mut hw = Board { ring_err: Some(McpError::unsupported("no RGB")), ..Default::default() };
        let mut led = RgbLed::new(4, "ws2812", 1);
        let err = led.call("led_set", &json!({ "color": "#ffffff" }), &mut hw).expect_err("no strip");
        assert_eq!(err.code, crate::error::code::UNSUPPORTED);
    }

    /// Every tool a driver publishes is addressable by that exact name, so `tools()` and
    /// `call` cannot drift apart.
    #[test]
    fn every_published_tool_is_dispatchable() {
        let cases: Vec<(Box<dyn Device>, Vec<&str>)> = vec![
            (Box::new(Servo::new(4)), vec!["servo_angle"]),
            (Box::new(Relay::new(5, false)), vec!["relay_set", "relay_pulse"]),
            (Box::new(RgbLed::new(6, "gpio", 1)), vec!["led_set", "led_blink"]),
            (Box::new(Buttons::new(vec![("a".into(), 7)])), vec!["buttons_read", "button_wait"]),
        ];
        for (device, want) in cases {
            let names: Vec<String> = device.tools().iter().map(|t| t.spec.name.clone()).collect();
            assert_eq!(names, want);
        }
    }
}

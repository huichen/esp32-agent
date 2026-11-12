//! HC-SR04 ultrasonic distance sensor (and compatible JSN-SR04T).
//!
//! Detection cannot be passive — the module answers only when pinged — so a
//! device configured by the user is kept even when the first ping times out, and
//! reported as `degraded` with the note that explains it.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::tool::{integer, object_schema, ToolDef};
use serde_json::{json, Value};

pub struct Hcsr04 {
    pub trig: i32,
    pub echo: i32,
    id: String,
    note: String,
    status: String,
}

impl Hcsr04 {
    pub fn new(trig: i32, echo: i32) -> Self {
        Self { trig, echo, id: "hcsr04".into(), note: String::new(), status: "ok".into() }
    }

    /// Configure the pins and take one reading to decide the initial status.
    pub fn probe(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        hw.gpio_config(self.trig, PinMode::Output)?;
        hw.gpio_config(self.echo, PinMode::Input)?;
        hw.gpio_write(self.trig, false)?;
        hw.delay_us(2_000);
        match self.measure(hw) {
            Ok(cm) if cm.is_finite() && cm > 0.0 => {
                self.status = "ok".into();
                self.note = format!("first ping {cm:.1} cm");
            }
            Ok(_) => {
                self.status = "degraded".into();
                self.note = "no echo returned (nothing in range?)".into();
            }
            Err(e) => {
                self.status = "degraded".into();
                self.note = format!("no echo: {}", e.message);
            }
        }
        Ok(())
    }

    /// One ping, distance in centimetres.
    pub fn measure(&mut self, hw: &mut dyn Hardware) -> Result<f32> {
        hw.gpio_write(self.trig, false)?;
        hw.delay_us(2);
        hw.gpio_write(self.trig, true)?;
        hw.delay_us(10);
        hw.gpio_write(self.trig, false)?;

        // Wait for the echo to rise (≈ 60 ms for the datasheet's 4 m maximum).
        let mut waited = 0u32;
        while !hw.gpio_read(self.echo)? {
            hw.delay_us(5);
            waited += 5;
            if waited > 60_000 {
                return Err(McpError::hardware("HC-SR04 echo never went high"));
            }
        }
        let mut width = 0u32;
        while hw.gpio_read(self.echo)? {
            hw.delay_us(5);
            width += 5;
            if width > 40_000 {
                return Err(McpError::hardware("HC-SR04 echo stuck high"));
            }
        }
        if width < 100 {
            return Err(McpError::hardware("HC-SR04 echo pulse too short"));
        }
        // Speed of sound at 20 °C: 343 m/s -> 58 µs per centimetre round trip.
        Ok(width as f32 / 58.0)
    }
}

impl Device for Hcsr04 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "hcsr04",
            &format!("HC-SR04 distance sensor (trig GPIO{}, echo GPIO{})", self.trig, self.echo),
            "gpio",
        );
        i.id = self.id.clone();
        i.pins = vec![self.trig, self.echo];
        i.caps = vec!["hcsr04".into(), "gpio".into()];
        i.source = "config".into();
        i.status = self.status.clone();
        i.note = self.note.clone();
        i.detail = json!({ "trig": self.trig, "echo": self.echo });
        i.tools = vec!["distance_cm".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("distance_cm", "Measure distance with the HC-SR04 ultrasonic sensor")
            .title("Distance")
            .category("sensor")
            .requires(&["hcsr04"])
            .schema(object_schema(
                &[("samples", integer(1, 5, "ping this many times and report the median, default 1"))],
                &[],
            ))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "distance_cm", args, ctx.hw)
            })]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "distance_cm" => {
                let n = crate::json::ranged_i64(args, "samples", 1, 1, 5)? as usize;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    match self.measure(hw) {
                        Ok(cm) => v.push(cm),
                        Err(e) if v.is_empty() => return Err(e),
                        Err(_) => {}
                    }
                    hw.delay_us(30_000);
                }
                if v.is_empty() {
                    return Err(McpError::hardware("no echo from the HC-SR04"));
                }
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median = v[v.len() / 2];
                self.status = "ok".into();
                Ok(json!({
                    "distance_cm": (median * 10.0).round() / 10.0,
                    "samples": v.iter().map(|x| (x * 10.0).round() / 10.0).collect::<Vec<f32>>(),
                }))
            }
            other => Err(McpError::not_found(format!("hcsr04 has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;
    use std::collections::VecDeque;

    /// The echo pin is the whole interface: the driver pings and then times how long the
    /// line stays high. The fake replays a scripted pin level per read, which is how a
    /// distance becomes a number the tests can predict from the speed of sound.
    #[derive(Default)]
    struct Pins {
        echo: VecDeque<bool>,
        /// What the line reads once the script runs out. False is a sensor that stopped
        /// answering; true is one stuck high.
        rest: bool,
        reads: usize,
        trig_levels: Vec<bool>,
        configs: Vec<(i32, PinMode)>,
        config_err: Option<McpError>,
        read_err: Option<McpError>,
    }

    impl Pins {
        /// One ping whose echo stays high for `width_us` after the trigger. 58 us is a
        /// centimetre, so a scripted width is a scripted distance.
        fn ping(width_us: u32) -> VecDeque<bool> {
            let mut q = VecDeque::new();
            q.push_back(false); // the rise the driver waits for
            for _ in 0..(width_us / 5 + 1) {
                q.push_back(true); // the first one ends the wait, the rest are counted
            }
            q.push_back(false); // the fall that ends the measurement
            q
        }

        fn with(pings: Vec<VecDeque<bool>>) -> Self {
            let mut echo = VecDeque::new();
            for p in pings {
                echo.extend(p);
            }
            Self { echo, ..Default::default() }
        }
    }

    impl Hardware for Pins {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, pin: i32, mode: PinMode) -> Result<()> {
            if let Some(e) = &self.config_err {
                return Err(e.clone());
            }
            self.configs.push((pin, mode));
            Ok(())
        }

        fn gpio_read(&mut self, _pin: i32) -> Result<bool> {
            if let Some(e) = &self.read_err {
                return Err(e.clone());
            }
            self.reads += 1;
            Ok(self.echo.pop_front().unwrap_or(self.rest))
        }

        fn gpio_write(&mut self, _pin: i32, high: bool) -> Result<()> {
            self.trig_levels.push(high);
            Ok(())
        }
    }

    #[test]
    fn a_ten_centimetre_echo_is_ten_centimetres() {
        // 10 cm is 580 us of round trip at 58 us/cm.
        let mut hw = Pins { echo: Pins::ping(580), ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        let cm = d.measure(&mut hw).expect("a clean echo measures");
        assert!((cm - 10.0).abs() < 1e-4, "got {cm}");
    }

    #[test]
    fn the_trigger_is_pulsed_low_high_low() {
        let mut hw = Pins { echo: Pins::ping(580), ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        d.measure(&mut hw).expect("measure");
        assert_eq!(hw.trig_levels, vec![false, true, false], "the module needs a >=10 us high pulse");
    }

    /// An echo that never rises is the common case with nothing in front of the sensor. It
    /// must give up inside the datasheet's range rather than spin forever on hardware.
    #[test]
    fn an_echo_that_never_rises_times_out() {
        let mut hw = Pins { rest: false, ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        let err = d.measure(&mut hw).expect_err("no echo");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(err.message.contains("never went high"), "{}", err.message);
        assert_eq!(hw.reads, 12_001, "the wait is bounded at 60 ms: {} reads", hw.reads);
    }

    /// An echo held high is a wiring fault (echo shorted, or a module that latched). The
    /// width limit is what keeps that from looking like a 700 m reading.
    #[test]
    fn an_echo_stuck_high_times_out_and_is_not_a_reading() {
        let mut hw = Pins { rest: true, ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        let err = d.measure(&mut hw).expect_err("stuck echo");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(err.message.contains("stuck high"), "{}", err.message);
    }

    /// A pulse shorter than 100 us cannot be a real echo (that would be 1.7 cm, inside the
    /// module's blind spot), so it is noise and reported as such instead of as a distance.
    #[test]
    fn a_pulse_too_short_to_be_an_echo_is_refused() {
        let mut hw = Pins { echo: Pins::ping(50), ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        let err = d.measure(&mut hw).expect_err("50 us is noise");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(err.message.contains("too short"), "{}", err.message);
    }

    #[test]
    fn a_gpio_failure_reaches_the_caller() {
        let mut hw =
            Pins { read_err: Some(McpError::hardware("GPIO 5 is not readable")), ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        let err = d.measure(&mut hw).expect_err("a pin that cannot be read is not a distance");
        assert!(err.message.contains("GPIO 5"), "{}", err.message);
    }

    /// Probing drives the pins and takes one reading: a sensor that answers comes up `ok`
    /// with the distance in the note, which is how the portal shows a working part.
    #[test]
    fn probe_configures_the_pins_and_records_the_first_ping() {
        let mut hw = Pins { echo: Pins::ping(580), ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        d.probe(&mut hw).expect("probe");
        assert_eq!(hw.configs, vec![(4, PinMode::Output), (5, PinMode::Input)]);
        assert_eq!(d.status, "ok");
        assert!(d.note.contains("10.0 cm"), "{}", d.note);
        let info = d.info();
        assert_eq!(info.status, "ok");
        assert_eq!(info.note, d.note, "the note is what the portal explains the device with");
        assert_eq!(info.pins, vec![4, 5]);
    }

    /// A declared sensor that does not answer is kept and marked `degraded`: the user wired
    /// it, so forgetting it would be worse than reporting that it is silent.
    #[test]
    fn probe_keeps_a_silent_sensor_as_degraded() {
        let mut hw = Pins { rest: false, ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        d.probe(&mut hw).expect("a silent sensor is not a failed probe");
        assert_eq!(d.status, "degraded");
        assert!(d.note.contains("no echo"), "{}", d.note);
        assert!(d.note.contains("never went high"), "the reason travels with it: {}", d.note);
        assert_eq!(d.info().status, "degraded");
    }

    #[test]
    fn a_pin_that_cannot_be_configured_fails_the_probe() {
        let mut hw = Pins {
            config_err: Some(McpError::unsupported("GPIO is not available on this board")),
            ..Default::default()
        };
        let mut d = Hcsr04::new(4, 5);
        let err = d.probe(&mut hw).expect_err("no GPIO, no sensor");
        assert_eq!(err.code, crate::error::code::UNSUPPORTED);
    }

    #[test]
    fn the_tool_reports_the_distance_and_the_samples_behind_it() {
        let mut hw = Pins { echo: Pins::ping(1_160), ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        let v = d.call("distance_cm", &json!({}), &mut hw).expect("distance");
        assert_eq!(v["distance_cm"], 20.0);
        assert_eq!(v["samples"], json!([20.0]));
        assert_eq!(d.status, "ok", "a good reading clears a degraded status");
    }

    /// The median is why `samples` exists: one bad ping out of three must not move the
    /// reported distance.
    #[test]
    fn the_median_ignores_one_outlier_among_three_pings() {
        let mut hw = Pins::with(vec![Pins::ping(580), Pins::ping(2_900), Pins::ping(1_160)]);
        let mut d = Hcsr04::new(4, 5);
        let v = d.call("distance_cm", &json!({ "samples": 3 }), &mut hw).expect("distance");
        assert_eq!(v["samples"], json!([10.0, 20.0, 50.0]), "sorted for the median");
        assert_eq!(v["distance_cm"], 20.0, "the middle of 10, 20, 50");
    }

    /// A failed ping in the middle of a burst is dropped, not fatal: the remaining samples
    /// still describe the distance, which is what a noisy 5 V rail looks like.
    #[test]
    fn a_ping_that_fails_after_a_good_one_is_skipped() {
        let mut hw = Pins { echo: Pins::ping(580), rest: false, ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        let v = d.call("distance_cm", &json!({ "samples": 2 }), &mut hw).expect("one of two");
        assert_eq!(v["distance_cm"], 10.0);
        assert_eq!(v["samples"], json!([10.0]), "the failed ping contributes nothing");
    }

    #[test]
    fn a_ping_burst_with_no_echo_at_all_is_an_error() {
        let mut hw = Pins { rest: false, ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        let err = d.call("distance_cm", &json!({ "samples": 3 }), &mut hw).expect_err("nothing came back");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    #[test]
    fn the_sample_count_is_bounded() {
        let mut hw = Pins::default();
        let mut d = Hcsr04::new(4, 5);
        for bad in [0, 6] {
            let err = d.call("distance_cm", &json!({ "samples": bad }), &mut hw).expect_err("out of range");
            assert_eq!(err.code, crate::error::code::INVALID_PARAMS, "samples={bad}");
        }
        assert_eq!(hw.trig_levels.len(), 0, "a refused sample count must not ping");
    }

    #[test]
    fn the_inventory_row_names_the_part_and_its_pins() {
        let d = Hcsr04::new(4, 5);
        let info = d.info();
        assert_eq!(info.kind, "hcsr04");
        assert_eq!(info.bus, "gpio");
        assert_eq!(info.source, "config", "an ultrasonic sensor is declared, never probed");
        assert_eq!(info.id, "hcsr04");
        assert!(info.name.contains("GPIO4") && info.name.contains("GPIO5"), "{}", info.name);
        assert_eq!(info.detail, json!({ "trig": 4, "echo": 5 }));
        assert_eq!(info.tools, vec!["distance_cm"]);
    }

    #[test]
    fn the_published_tool_is_dispatchable_and_nothing_else_is() {
        let names: Vec<String> = Hcsr04::new(4, 5).tools().iter().map(|t| t.spec.name.clone()).collect();
        assert_eq!(names, vec!["distance_cm"]);
        let mut hw = Pins { echo: Pins::ping(580), ..Default::default() };
        let mut d = Hcsr04::new(4, 5);
        d.call(&names[0], &json!({}), &mut hw).expect("the published name works");
        let err = d.call("distance", &json!({}), &mut hw).expect_err("a near miss is not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
        assert!(err.message.contains("distance"), "{}", err.message);
    }
}

//! PIR motion sensor (HC-SR501 and clones) on one GPIO.
//!
//! Nothing here can be probed: a PIR output sits low when there is nothing to see, which is
//! exactly what a pin nobody drives reads with a pull-down — so `new` takes the pin and the
//! polarity as board facts and there is no `identify` to mistake silence for an answer.
//!
//! "Seconds since last motion" is a *derived* value, and an upper bound rather than a
//! measurement. The driver has no interrupt and no timer of its own, so it knows only what
//! its own polls saw: it remembers the last poll that read the pin active, and the motion
//! may have ended any time after that. The figure can therefore be too large, never too
//! small. It is also quantised by the sensor's own hold time, not by the poll rate — an
//! HC-SR501 keeps its output high for at least 2.5 s after the last movement (minutes with
//! the potentiometer turned up), so two events closer together than that cannot be told
//! apart from one long event.
//!
//! Safety is not an issue on this side: a PIR is read-only, and no tool here changes a pin.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::json as j;
use crate::tool::{integer, object_schema, ToolDef};
use serde_json::{json, Value};

/// Polling period while waiting. A PIR's own hold time is measured in seconds, so 10 ms is
/// already far finer than anything the sensor can report.
const POLL_US: u32 = 10_000;
const MIN_TIMEOUT_MS: u32 = 10;
const MAX_TIMEOUT_MS: u32 = 60_000;
const DEFAULT_TIMEOUT_MS: u32 = 5_000;

pub struct Pir {
    pub pin: i32,
    /// `true` for an HC-SR501 (output high while motion is detected); `false` for the
    /// boards and modules that drive the line low instead.
    pub active_high: bool,
    id: String,
    /// When a poll last read the pin active. `None` means no motion has been seen since the
    /// driver was built, which is different from "a long time ago".
    last_motion_ms: Option<u64>,
    last_read: Option<bool>,
}

impl Pir {
    pub fn new(pin: i32, active_high: bool) -> Self {
        Self { pin, active_high, id: "pir".into(), last_motion_ms: None, last_read: None }
    }

    /// A pull-up on an active-low sensor and a pull-down on an active-high one, so a
    /// disconnected module reads "no motion" instead of noise.
    pub fn configure(&self, hw: &mut dyn Hardware) -> Result<()> {
        let mode = if self.active_high { PinMode::InputPulldown } else { PinMode::InputPullup };
        hw.gpio_config(self.pin, mode)
    }

    fn level_is_motion(&self, high: bool) -> bool {
        high == self.active_high
    }

    /// Is there motion right now? One poll, no waiting.
    pub fn motion(&mut self, hw: &mut dyn Hardware) -> Result<bool> {
        self.configure(hw)?;
        let motion = self.level_is_motion(hw.gpio_read(self.pin)?);
        self.last_read = Some(motion);
        if motion {
            self.last_motion_ms = Some(hw.millis());
        }
        Ok(motion)
    }

    /// Poll until motion or `timeout_ms`; returns `(motion, elapsed_ms)`.
    pub fn wait(&mut self, hw: &mut dyn Hardware, timeout_ms: u32) -> Result<(bool, u32)> {
        self.configure(hw)?;
        let start = hw.millis();
        loop {
            let motion = self.level_is_motion(hw.gpio_read(self.pin)?);
            self.last_read = Some(motion);
            let elapsed = hw.millis().saturating_sub(start);
            if motion {
                self.last_motion_ms = Some(hw.millis());
                return Ok((true, elapsed as u32));
            }
            if elapsed >= timeout_ms as u64 {
                return Ok((false, elapsed as u32));
            }
            hw.delay_us(POLL_US);
        }
    }

    /// Seconds since the last poll that saw motion; `None` when nothing has been seen yet.
    /// See the module note: this is an upper bound, not a stopwatch.
    pub fn since_motion(&mut self, hw: &mut dyn Hardware) -> Result<Option<f32>> {
        if self.motion(hw)? {
            return Ok(Some(0.0));
        }
        Ok(self.last_motion_ms.map(|t| hw.millis().saturating_sub(t) as f32 / 1000.0))
    }
}

impl Device for Pir {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("pir", &format!("PIR motion sensor on GPIO{}", self.pin), "gpio");
        i.id = self.id.clone();
        i.pins = vec![self.pin];
        i.caps = vec!["pir".into(), "gpio".into()];
        i.source = "config".into();
        i.detail = json!({
            "pin": self.pin,
            "active_high": self.active_high,
            "last_read": self.last_read,
            "motion_seen": self.last_motion_ms.is_some(),
        });
        i.note = "polled: the sensor holds its output for its own retrigger time".into();
        i.tools = vec!["motion_now".into(), "motion_wait".into(), "motion_since".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let wait = id.clone();
        let since = id.clone();
        vec![
            ToolDef::new("motion_now", "Read a PIR motion sensor once")
                .title("Motion now")
                .category("sensor")
                .requires(&["pir"])
                .schema(object_schema(&[], &[]))
                .bind(move |ctx, args| {
                    let id = id.clone();
                    ctx.devices.call(&id, "motion_now", args, ctx.hw)
                }),
            ToolDef::new("motion_wait", "Wait for a PIR to report motion, up to a timeout")
                .title("Wait for motion")
                .category("sensor")
                .requires(&["pir"])
                .schema(object_schema(
                    &[(
                        "timeout_ms",
                        integer(
                            MIN_TIMEOUT_MS as i64,
                            MAX_TIMEOUT_MS as i64,
                            "give up after this long, default 5000",
                        ),
                    )],
                    &[],
                ))
                .bind(move |ctx, args| {
                    let id = wait.clone();
                    ctx.devices.call(&id, "motion_wait", args, ctx.hw)
                }),
            ToolDef::new(
                "motion_since",
                "Seconds since the last motion this driver saw; an upper bound, not a stopwatch",
            )
            .title("Since motion")
            .category("sensor")
            .requires(&["pir"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| {
                let id = since.clone();
                ctx.devices.call(&id, "motion_since", args, ctx.hw)
            }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "motion_now" => {
                let motion = self.motion(hw)?;
                let since = if motion { Some(0.0) } else { self.since_motion(hw)? };
                Ok(json!({
                    "motion": motion,
                    "pin": self.pin,
                    "active_high": self.active_high,
                    "seconds_since_motion": since.map(|s| (s * 10.0).round() / 10.0),
                }))
            }
            "motion_wait" => {
                let timeout = j::ranged_i64(
                    args,
                    "timeout_ms",
                    DEFAULT_TIMEOUT_MS as i64,
                    MIN_TIMEOUT_MS as i64,
                    MAX_TIMEOUT_MS as i64,
                )? as u32;
                let (motion, elapsed) = self.wait(hw, timeout)?;
                let since = if motion { Some(0.0) } else { self.since_motion(hw)? };
                Ok(json!({
                    "motion": motion,
                    "pin": self.pin,
                    "timeout_ms": timeout,
                    "elapsed_ms": elapsed,
                    "seconds_since_motion": since.map(|s| (s * 10.0).round() / 10.0),
                }))
            }
            "motion_since" => {
                let since = self.since_motion(hw)?;
                Ok(json!({
                    "seconds_since_motion": since.map(|s| (s * 10.0).round() / 10.0),
                    // The poll inside `since_motion` is the freshest reading there is.
                    "motion": self.last_read.unwrap_or(false),
                    "pin": self.pin,
                    "note": match since {
                        Some(_) => Value::Null,
                        None => json!("no motion has been seen since the driver was built"),
                    },
                }))
            }
            other => Err(McpError::not_found(format!("pir has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code;
    use crate::hardware::BoardInfo;

    /// A sensor whose line can be held at a level or switched on at a chosen time.
    struct FakeSensor {
        pin: i32,
        level: bool,
        motion_at_ms: Option<u64>,
        now_us: u64,
    }

    impl FakeSensor {
        fn held(level: bool) -> Self {
            Self { pin: 8, level, motion_at_ms: None, now_us: 0 }
        }

        fn motion_starts_at(ms: u64) -> Self {
            Self { pin: 8, level: false, motion_at_ms: Some(ms), now_us: 0 }
        }

        fn advance_ms(&mut self, ms: u64) {
            self.now_us += ms * 1_000;
        }
    }

    impl Hardware for FakeSensor {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, _pin: i32, _mode: PinMode) -> Result<()> {
            Ok(())
        }

        fn gpio_read(&mut self, pin: i32) -> Result<bool> {
            if pin != self.pin {
                return Err(McpError::not_found(format!("nothing on GPIO{pin}")));
            }
            Ok(match self.motion_at_ms {
                Some(at) => self.millis() >= at,
                None => self.level,
            })
        }

        fn delay_us(&mut self, us: u32) {
            self.now_us += us as u64;
        }

        fn millis(&mut self) -> u64 {
            self.now_us / 1000
        }
    }

    #[test]
    fn motion_now_reads_the_pin_and_reports_zero_while_active() {
        let mut hw = FakeSensor::held(true);
        let mut dev = Pir::new(8, true);
        let v = dev.call("motion_now", &json!({}), &mut hw).unwrap();
        assert_eq!(v["motion"], true);
        assert_eq!(v["seconds_since_motion"], 0.0);

        let mut hw = FakeSensor::held(false);
        let mut dev = Pir::new(8, true);
        let v = dev.call("motion_now", &json!({}), &mut hw).unwrap();
        assert_eq!(v["motion"], false);
        assert!(v["seconds_since_motion"].is_null(), "no motion has been seen yet");
    }

    #[test]
    fn an_active_low_sensor_is_read_the_other_way_round() {
        let mut hw = FakeSensor::held(false);
        let mut dev = Pir::new(8, false);
        let v = dev.call("motion_now", &json!({}), &mut hw).unwrap();
        assert_eq!(v["motion"], true, "a low line is motion for an active-low module");
        assert_eq!(v["active_high"], false);
    }

    #[test]
    fn wait_returns_as_soon_as_the_line_goes_active() {
        let mut hw = FakeSensor::motion_starts_at(200);
        let mut dev = Pir::new(8, true);
        let v = dev.call("motion_wait", &json!({ "timeout_ms": 5_000 }), &mut hw).unwrap();
        assert_eq!(v["motion"], true);
        let elapsed = v["elapsed_ms"].as_u64().expect("elapsed_ms is a number");
        assert!((200..500).contains(&elapsed), "waited {elapsed} ms for motion at 200 ms");
    }

    #[test]
    fn both_ends_of_the_timeout_are_accepted_and_a_silent_sensor_times_out() {
        let mut hw = FakeSensor::held(false);
        let mut dev = Pir::new(8, true);
        let v = dev.call("motion_wait", &json!({ "timeout_ms": MIN_TIMEOUT_MS }), &mut hw).unwrap();
        assert_eq!(v["motion"], false);
        assert!(v["elapsed_ms"].as_u64().unwrap() >= MIN_TIMEOUT_MS as u64);

        let v = dev.call("motion_wait", &json!({ "timeout_ms": MAX_TIMEOUT_MS }), &mut hw).unwrap();
        assert_eq!(v["motion"], false);
        assert_eq!(v["elapsed_ms"], 60_000, "the longest timeout waits the whole minute");
    }

    #[test]
    fn a_timeout_out_of_range_is_refused() {
        let mut hw = FakeSensor::held(true);
        let mut dev = Pir::new(8, true);
        for bad in [0, 9, MAX_TIMEOUT_MS + 1] {
            let err = dev.call("motion_wait", &json!({ "timeout_ms": bad }), &mut hw).unwrap_err();
            assert_eq!(err.code, code::INVALID_PARAMS);
            assert!(err.message.contains("timeout_ms"), "unexpected error: {}", err.message);
        }
    }

    #[test]
    fn seconds_since_motion_is_measured_from_the_last_active_poll() {
        let mut hw = FakeSensor::held(true);
        let mut dev = Pir::new(8, true);
        dev.call("motion_now", &json!({}), &mut hw).unwrap();

        // The movement stops; no poll runs for 2.5 s, then one reads the line quiet.
        hw.level = false;
        hw.advance_ms(2_500);
        let v = dev.call("motion_since", &json!({}), &mut hw).unwrap();
        assert_eq!(v["motion"], false);
        assert_eq!(v["seconds_since_motion"], 2.5);

        // Motion restarts, so the figure drops back to zero rather than growing.
        hw.level = true;
        let v = dev.call("motion_since", &json!({}), &mut hw).unwrap();
        assert_eq!(v["seconds_since_motion"], 0.0);
    }
}

//! Stepper motor over a STEP/DIR carrier: A4988, DRV8825, TMC2208 in STEP/DIR mode.
//!
//! Nothing on such a carrier can answer a probe: the signals are pulses in and current
//! out, there is no id register and no read-back. `new` therefore takes the pins and the
//! carrier's microstep setting as board facts, and no `identify` exists to dress up a read
//! of a floating pin as a reply.
//!
//! Timing, plainly: one step is one rising edge, and `step_us` is the period of one step
//! (the pulse is high for half of it). 1000 us therefore steps at 1 kHz, which with a
//! 200-step motor at 1/16 microstepping is about 3.2 s per revolution. There is no
//! acceleration ramp and no position feedback, so a move that loses steps is not detected —
//! a stalled stepper reports the same position as one that turned.
//!
//! Because the tool blocks while stepping, a long move holds the tool call for its whole
//! duration: [`MAX_MOVE_MS`] refuses anything that would take longer than 30 s rather than
//! leaving a caller waiting on a silent 2000-step crawl.
//!
//! Safety: the explicit stop is `stepper_release`, which drops the coil current (EN high on
//! these carriers) and lets the shaft turn by hand. A move leaves the coils energised, so
//! the motor holds its position — that holding torque is the reason a release is a separate
//! command rather than something a move does on its way out. A failed pulse stops the loop
//! immediately, which is already the safe state: a stepper only moves on an edge.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::json as j;
use crate::tool::{integer, object_schema, ToolDef};
use serde_json::{json, Value};

const DEFAULT_STEP_US: u32 = 500;
const MIN_STEP_US: u32 = 50;
const MAX_STEP_US: u32 = 5_000;
/// Largest single move, in microsteps. 10 000 microsteps is 31 revolutions at 1/16 on a
/// 200-step motor: plenty for a positioning job, small enough that a typo cannot spin for
/// minutes.
const MAX_STEPS: i64 = 10_000;
/// Largest absolute position, in microsteps, for `stepper_goto`.
const MAX_POSITION: i64 = 100_000;
const MAX_MOVE_MS: u64 = 30_000;
/// Direction setup time. A4988 needs 200 ns, DRV8825 650 ns, TMC2208 20 ns; 5 us costs
/// nothing at these step rates and covers all of them with margin.
const DIR_SETUP_US: u32 = 5;

pub struct Stepper {
    pub step: i32,
    pub dir: i32,
    /// `EN` on the carrier, active low. `None` when it is tied low there, in which case the
    /// coils can never be released from here.
    pub enable: Option<i32>,
    /// Step period in microseconds, used when a tool does not override it.
    pub step_us: u32,
    /// The carrier's microstep setting (the MS1/MS2/MS3 straps). STEP/DIR cannot change it,
    /// so this is declared, not commanded; it only converts the count into a position in
    /// degrees.
    pub microsteps: u32,
    /// Full steps per revolution of the motor itself (1.8 deg/step = 200).
    pub steps_per_rev: u32,
    id: String,
    /// Microsteps from the position this driver was constructed at.
    position: i32,
    enabled: bool,
}

impl Stepper {
    pub fn new(step: i32, dir: i32, enable: Option<i32>, microsteps: u32) -> Self {
        Self {
            step,
            dir,
            enable,
            step_us: DEFAULT_STEP_US,
            microsteps: microsteps.clamp(1, 64),
            steps_per_rev: 200,
            id: "stepper".into(),
            position: 0,
            enabled: false,
        }
    }

    pub fn position(&self) -> i32 {
        self.position
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Position in degrees, from the declared microstep setting and step count.
    pub fn angle_deg(&self) -> f32 {
        let per_rev = (self.steps_per_rev.max(1) * self.microsteps) as f32;
        self.position as f32 / per_rev * 360.0
    }

    /// Enable the coils if they were released. A disabled carrier takes no current, so
    /// stepping it would report success for a motor that never moved.
    fn ensure_enabled(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        if let Some(pin) = self.enable {
            hw.gpio_config(pin, PinMode::Output)?;
            hw.gpio_write(pin, false)?;
            self.enabled = true;
            // A4988/DRV8825 need about 1 ms after enable before the first edge counts.
            hw.delay_us(1_000);
        }
        Ok(())
    }

    fn pulse_inner(hw: &mut dyn Hardware, step: i32, half: u32, rest: u32) -> Result<()> {
        hw.gpio_write(step, true)?;
        hw.delay_us(half);
        hw.gpio_write(step, false)?;
        hw.delay_us(rest);
        Ok(())
    }

    fn pulse(&mut self, hw: &mut dyn Hardware, half: u32, rest: u32) -> Result<()> {
        let result = Self::pulse_inner(hw, self.step, half, rest);
        if result.is_err() {
            // Do not leave STEP latched high: the next move assumes it starts low, and a
            // driver that sees a level rather than an edge does not step.
            let _ = hw.gpio_write(self.step, false);
        }
        result
    }

    /// Move `steps` microsteps; negative turns the other way. Returns how many went out.
    pub fn move_steps(&mut self, hw: &mut dyn Hardware, steps: i32, step_us: u32) -> Result<i32> {
        check_budget(steps, step_us)?;
        if steps == 0 {
            return Ok(0);
        }
        self.ensure_enabled(hw)?;
        hw.gpio_config(self.step, PinMode::Output)?;
        hw.gpio_config(self.dir, PinMode::Output)?;
        hw.gpio_write(self.dir, steps < 0)?;
        hw.delay_us(DIR_SETUP_US);

        let half = (step_us / 2).max(1);
        let rest = step_us.saturating_sub(half);
        let sign = steps.signum();
        let mut done = 0i32;
        for _ in 0..steps.unsigned_abs() {
            if let Err(e) = self.pulse(hw, half, rest) {
                // No further edges: a stepper is already stopped, which is the safe state.
                // The count is credited with the pulses that completed, and the error says
                // plainly that the interrupted one is a lost step at worst.
                self.position = self.position.saturating_add(done * sign);
                return Err(e.with_data(json!({
                    "steps_done": done,
                    "position": self.position,
                    "note": "the interrupted pulse may or may not have moved the motor",
                })));
            }
            done += 1;
        }
        self.position = self.position.saturating_add(steps);
        Ok(done)
    }

    /// Move to an absolute position, in microsteps from where this driver started.
    pub fn goto(&mut self, hw: &mut dyn Hardware, target: i32, step_us: u32) -> Result<i32> {
        let delta = target.saturating_sub(self.position);
        self.move_steps(hw, delta, step_us)?;
        Ok(self.position)
    }

    /// Release the coils. Returns whether there was an enable pin to release.
    pub fn release(&mut self, hw: &mut dyn Hardware) -> Result<bool> {
        match self.enable {
            Some(pin) => {
                hw.gpio_config(pin, PinMode::Output)?;
                hw.gpio_write(pin, true)?;
                self.enabled = false;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// Refuse a move that would hold the tool call for longer than [`MAX_MOVE_MS`]. Checked
/// before any pin is touched, so a refused move leaves the motor exactly where it was.
fn check_budget(steps: i32, step_us: u32) -> Result<()> {
    let ms = steps.unsigned_abs() as u64 * step_us as u64 / 1000;
    if ms > MAX_MOVE_MS {
        return Err(McpError::invalid_params(format!(
            "a move of {steps} steps at {step_us} us per step takes about {ms} ms, above the {MAX_MOVE_MS} ms budget: ask for fewer steps or a shorter step_us"
        )));
    }
    Ok(())
}

fn ranged_i32(args: &Value, name: &str, min: i64, max: i64) -> Result<i32> {
    let v = j::i64_arg(args, name)?;
    if v < min || v > max {
        return Err(McpError::invalid_params(format!("`{name}` must be within {min}..={max}, got {v}")));
    }
    Ok(v as i32)
}

fn step_us_arg(args: &Value, default: u32) -> Result<u32> {
    Ok(j::ranged_i64(args, "step_us", default as i64, MIN_STEP_US as i64, MAX_STEP_US as i64)? as u32)
}

impl Device for Stepper {
    fn info(&self) -> DeviceInfo {
        let mut pins = vec![self.step, self.dir];
        pins.extend(self.enable);
        let mut i = DeviceInfo::new(
            "stepper",
            &format!(
                "Stepper on STEP GPIO{}, DIR GPIO{}{}",
                self.step,
                self.dir,
                self.enable.map(|p| format!(", EN GPIO{p}")).unwrap_or_default()
            ),
            "pwm",
        );
        i.id = self.id.clone();
        i.pins = pins;
        i.caps = vec!["stepper".into(), "pwm".into(), "gpio".into()];
        i.source = "config".into();
        i.detail = json!({
            "step": self.step,
            "dir": self.dir,
            "enable": self.enable,
            "step_us": self.step_us,
            "microsteps": self.microsteps,
            "steps_per_rev": self.steps_per_rev,
            "position": self.position,
            "angle_deg": (self.angle_deg() * 100.0).round() / 100.0,
            "enabled": self.enabled,
        });
        i.tools = vec!["stepper_move".into(), "stepper_goto".into(), "stepper_release".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let goto = id.clone();
        let release = id.clone();
        vec![
            ToolDef::new("stepper_move", "Turn a stepper a number of microsteps; the sign is the direction")
                .title("Stepper move")
                .category("gpio")
                .requires(&["stepper"])
                .schema(object_schema(
                    &[
                        ("steps", integer(-MAX_STEPS, MAX_STEPS, "microsteps; negative reverses")),
                        (
                            "step_us",
                            integer(
                                MIN_STEP_US as i64,
                                MAX_STEP_US as i64,
                                "step period, default the configured one",
                            ),
                        ),
                    ],
                    &["steps"],
                ))
                .mutating()
                .bind(move |ctx, args| {
                    let id = id.clone();
                    ctx.devices.call(&id, "stepper_move", args, ctx.hw)
                }),
            ToolDef::new(
                "stepper_goto",
                "Move a stepper to an absolute position, in microsteps from where the driver started",
            )
            .title("Stepper goto")
            .category("gpio")
            .requires(&["stepper"])
            .schema(object_schema(
                &[
                    ("position", integer(-MAX_POSITION, MAX_POSITION, "target microstep position")),
                    (
                        "step_us",
                        integer(
                            MIN_STEP_US as i64,
                            MAX_STEP_US as i64,
                            "step period, default the configured one",
                        ),
                    ),
                ],
                &["position"],
            ))
            .mutating()
            .bind(move |ctx, args| {
                let id = goto.clone();
                ctx.devices.call(&id, "stepper_goto", args, ctx.hw)
            }),
            ToolDef::new("stepper_release", "Drop the coil current so the shaft can be turned by hand")
                .title("Stepper release")
                .category("gpio")
                .requires(&["stepper"])
                .schema(object_schema(&[], &[]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = release.clone();
                    ctx.devices.call(&id, "stepper_release", args, ctx.hw)
                }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "stepper_move" => {
                let steps = ranged_i32(args, "steps", -MAX_STEPS, MAX_STEPS)?;
                let step_us = step_us_arg(args, self.step_us)?;
                let moved = self.move_steps(hw, steps, step_us)?;
                Ok(json!({
                    "ok": true,
                    "moved": moved * steps.signum(),
                    "position": self.position,
                    "angle_deg": (self.angle_deg() * 100.0).round() / 100.0,
                    "step_us": step_us,
                    "enabled": self.enabled,
                }))
            }
            "stepper_goto" => {
                let target = ranged_i32(args, "position", -MAX_POSITION, MAX_POSITION)?;
                let step_us = step_us_arg(args, self.step_us)?;
                let before = self.position;
                let position = self.goto(hw, target, step_us)?;
                Ok(json!({
                    "ok": true,
                    "moved": position - before,
                    "position": position,
                    "target": target,
                    "angle_deg": (self.angle_deg() * 100.0).round() / 100.0,
                    "enabled": self.enabled,
                }))
            }
            "stepper_release" => {
                let released = self.release(hw)?;
                Ok(json!({
                    "ok": true,
                    "released": released,
                    "enabled": self.enabled,
                    "position": self.position,
                    "note": if released {
                        Value::Null
                    } else {
                        json!("the carrier's EN pin is tied low: the coils cannot be released from here")
                    },
                }))
            }
            other => Err(McpError::not_found(format!("stepper has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code;
    use crate::hardware::BoardInfo;
    use std::collections::BTreeMap;

    /// A carrier that counts rising edges on STEP and can refuse one of them.
    struct FakeCarrier {
        step: i32,
        level: BTreeMap<i32, bool>,
        pulses: u32,
        fail_step_at: Option<u32>,
        now_us: u64,
    }

    impl FakeCarrier {
        fn new(step: i32) -> Self {
            Self { step, level: BTreeMap::new(), pulses: 0, fail_step_at: None, now_us: 0 }
        }

        fn level_of(&self, pin: i32) -> bool {
            self.level.get(&pin).copied().unwrap_or(false)
        }
    }

    impl Hardware for FakeCarrier {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, _pin: i32, _mode: PinMode) -> Result<()> {
            Ok(())
        }

        fn gpio_read(&mut self, _pin: i32) -> Result<bool> {
            Ok(false)
        }

        fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
            if pin == self.step && high {
                if self.fail_step_at == Some(self.pulses + 1) {
                    return Err(McpError::hardware(format!("STEP on GPIO{pin} refused")));
                }
                self.pulses += 1;
            }
            self.level.insert(pin, high);
            Ok(())
        }

        fn delay_us(&mut self, us: u32) {
            self.now_us += us as u64;
        }

        fn millis(&mut self) -> u64 {
            self.now_us / 1000
        }
    }

    #[test]
    fn a_move_pulses_once_per_step_in_the_requested_direction() {
        let mut hw = FakeCarrier::new(4);
        let mut dev = Stepper::new(4, 5, Some(6), 16);
        let v = dev.call("stepper_move", &json!({ "steps": 12 }), &mut hw).unwrap();
        assert_eq!(hw.pulses, 12, "one rising edge per microstep");
        assert_eq!(v["moved"], 12);
        assert_eq!(v["position"], 12);
        assert!(!hw.level_of(5), "forward is DIR low");
        assert!(!hw.level_of(6), "the coils were enabled: EN is active low");

        dev.call("stepper_move", &json!({ "steps": -5 }), &mut hw).unwrap();
        assert!(hw.level_of(5), "reverse is DIR high");
        assert_eq!(dev.position(), 7);
        assert_eq!(hw.pulses, 17);
    }

    #[test]
    fn both_ends_of_the_step_range_are_accepted() {
        for (steps, expected) in
            [(MAX_STEPS as i32, MAX_STEPS as i32), (-(MAX_STEPS as i32), -(MAX_STEPS as i32))]
        {
            let mut hw = FakeCarrier::new(4);
            let mut dev = Stepper::new(4, 5, None, 8);
            let v = dev.call("stepper_move", &json!({ "steps": steps, "step_us": 50 }), &mut hw).unwrap();
            assert_eq!(hw.pulses, expected.unsigned_abs(), "every step went out");
            assert_eq!(v["position"], expected);
            assert_eq!(dev.position(), expected);
        }
    }

    #[test]
    fn out_of_range_is_refused_before_anything_moves() {
        let mut hw = FakeCarrier::new(4);
        let mut dev = Stepper::new(4, 5, Some(6), 16);

        let err = dev.call("stepper_move", &json!({ "steps": MAX_STEPS + 1 }), &mut hw).unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS);
        assert!(err.message.contains("steps"), "the error names the field: {}", err.message);

        let err = dev.call("stepper_move", &json!({ "steps": 10, "step_us": 49 }), &mut hw).unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS);
        assert!(err.message.contains("step_us"));

        let err = dev.call("stepper_goto", &json!({ "position": MAX_POSITION + 1 }), &mut hw).unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS);

        assert_eq!(hw.pulses, 0, "a refused command must not step");
        assert_eq!(dev.position(), 0);
        assert!(!hw.level_of(6), "and it must not even enable the coils");
    }

    #[test]
    fn a_move_above_the_time_budget_is_refused() {
        let mut hw = FakeCarrier::new(4);
        let mut dev = Stepper::new(4, 5, None, 16);
        // 10 000 steps at 5 ms each is 50 s: refused rather than blocking the call.
        let err =
            dev.call("stepper_move", &json!({ "steps": 10_000, "step_us": 5_000 }), &mut hw).unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS);
        assert!(err.message.contains("budget"), "unexpected error: {}", err.message);
        assert_eq!(hw.pulses, 0);
    }

    #[test]
    fn a_failed_pulse_stops_the_motor_and_credits_only_what_went_out() {
        let mut hw = FakeCarrier::new(4);
        hw.fail_step_at = Some(3);
        let mut dev = Stepper::new(4, 5, None, 16);
        let err = dev.call("stepper_move", &json!({ "steps": 10 }), &mut hw).unwrap_err();
        assert!(err.message.contains("GPIO4"), "unexpected error: {}", err.message);
        assert_eq!(hw.pulses, 2, "the third edge never went out");
        assert_eq!(dev.position(), 2, "the position counts completed steps only");
        assert_eq!(err.data.as_ref().map(|d| d["steps_done"].clone()), Some(json!(2)));
        assert!(!hw.level_of(4), "STEP is not left latched high");
    }

    #[test]
    fn goto_moves_the_delta_and_release_drops_the_coils() {
        let mut hw = FakeCarrier::new(4);
        let mut dev = Stepper::new(4, 5, Some(6), 16);
        dev.call("stepper_move", &json!({ "steps": 10 }), &mut hw).unwrap();
        let v = dev.call("stepper_goto", &json!({ "position": 4 }), &mut hw).unwrap();
        assert_eq!(v["moved"], -6);
        assert_eq!(v["position"], 4);
        assert_eq!(dev.position(), 4, "an absolute target is reached, not overshot");

        let v = dev.call("stepper_release", &json!({}), &mut hw).unwrap();
        assert_eq!(v["released"], true);
        assert_eq!(v["enabled"], false);
        assert!(hw.level_of(6), "EN high drops the coil current");
        assert_eq!(dev.position(), 4, "releasing does not forget the position");

        let mut hw = FakeCarrier::new(4);
        let mut tied = Stepper::new(4, 5, None, 16);
        let v = tied.call("stepper_release", &json!({}), &mut hw).unwrap();
        assert_eq!(v["released"], false, "with EN tied low there is nothing to release");
        assert!(v["note"].as_str().is_some_and(|n| !n.is_empty()));
    }
}

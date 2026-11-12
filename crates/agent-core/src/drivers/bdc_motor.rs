//! Brushed DC motor through an H-bridge: L298N, TB6612FNG, DRV8833.
//!
//! An H-bridge has no id register and nothing to read back, so this is one of the parts
//! that cannot be probed: `new` takes the wiring as a board fact, and no `identify` exists
//! to dress up a read of a floating input as a reply. The same reasoning as the servo and
//! the relay in [`crate::drivers::actuator`].
//!
//! Two wirings are supported, and the difference decides whether a brake exists:
//!
//! * [`Wiring::PwmDir`]: PWM on the bridge enable (`ENA`) and one GPIO on the direction
//!   input (`IN1`), with `IN2` tied low. A zero duty cycle coasts; there is no brake,
//!   because shorting the winding needs `IN2` high.
//! * [`Wiring::TwoPwm`]: a PWM channel on each of `IN1` and `IN2` (TB6612, DRV8833, or an
//!   L298N with both inputs on PWM pins). One input is driven while the other is held low;
//!   both high shorts the winding and brakes the motor.
//!
//! Safety, in the order the code enforces it:
//!
//! 1. A speed outside -100..=100 % is refused before a pin is touched, so a mistyped
//!    command cannot leave a duty cycle behind.
//! 2. A direction change costs one extra write: the old duty goes to zero first, because
//!    reversing the bridge with the previous duty still applied commands a full-speed
//!    reversal for the few milliseconds the bridge takes to turn around.
//! 3. A write that fails leaves the bridge commanded to zero duty and the reported speed at
//!    0, so `motor_state` never claims "stopped" while a duty cycle is set.
//! 4. `motor_stop` is an explicit tool and nothing here times out: a bridge keeps driving
//!    the motor until the next command, which is why the tool exists at all.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::json as j;
use crate::tool::{enumeration, number, object_schema, ToolDef};
use serde_json::{json, Value};

/// How the H-bridge is wired to the SoC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wiring {
    /// One PWM pin on the bridge enable and one GPIO on the direction input.
    PwmDir { pwm: i32, dir: i32 },
    /// A PWM channel on each input; one is driven, the other held low.
    TwoPwm { in1: i32, in2: i32 },
}

impl Wiring {
    pub fn pwm_dir(pwm: i32, dir: i32) -> Self {
        Self::PwmDir { pwm, dir }
    }

    pub fn two_pwm(in1: i32, in2: i32) -> Self {
        Self::TwoPwm { in1, in2 }
    }

    /// The name used in tool results and in the device detail.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PwmDir { .. } => "pwm+dir",
            Self::TwoPwm { .. } => "two_pwm",
        }
    }

    pub fn pins(&self) -> Vec<i32> {
        match *self {
            Self::PwmDir { pwm, dir } => vec![pwm, dir],
            Self::TwoPwm { in1, in2 } => vec![in1, in2],
        }
    }
}

pub struct BdcMotor {
    pub wiring: Wiring,
    /// PWM frequency. 20 kHz is above the audible band and inside what an L298N, a TB6612
    /// and a DRV8833 all switch; a slower bridge can be given another value.
    pub freq_hz: u32,
    id: String,
    speed: f32,
    braked: bool,
}

impl BdcMotor {
    pub fn new(wiring: Wiring) -> Self {
        Self { wiring, freq_hz: 20_000, id: "motor".into(), speed: 0.0, braked: false }
    }

    pub fn with_freq(mut self, freq_hz: u32) -> Self {
        self.freq_hz = freq_hz.max(1);
        self
    }

    /// The last commanded speed in percent. A measurement is impossible: an H-bridge has no
    /// output that reports what the motor is doing.
    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn braked(&self) -> bool {
        self.braked
    }

    /// Whether this wiring can short the winding, which is all a brake is on a brushed motor.
    pub fn can_brake(&self) -> bool {
        matches!(self.wiring, Wiring::TwoPwm { .. })
    }

    fn coast(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        match self.wiring {
            Wiring::PwmDir { pwm, .. } => hw.pwm_set(pwm, self.freq_hz, 0.0),
            Wiring::TwoPwm { in1, in2 } => {
                // The driven input first: taking the idle one low while the other is still
                // driving would be a no-op on the motor but reads as a stop in a log.
                hw.pwm_set(in1, self.freq_hz, 0.0)?;
                hw.pwm_set(in2, self.freq_hz, 0.0)
            }
        }
    }

    fn short_winding(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        match self.wiring {
            Wiring::TwoPwm { in1, in2 } => {
                hw.pwm_set(in1, self.freq_hz, 100.0)?;
                hw.pwm_set(in2, self.freq_hz, 100.0)
            }
            Wiring::PwmDir { .. } => Err(McpError::unsupported(
                "this wiring has no brake: IN2 is tied low, so the motor can only coast",
            )),
        }
    }

    /// The raw write sequence for one speed, without any state bookkeeping.
    fn drive(&mut self, hw: &mut dyn Hardware, speed: f32) -> Result<()> {
        if speed == 0.0 {
            return self.coast(hw);
        }
        let magnitude = speed.abs();
        let reverse = speed < 0.0;
        match self.wiring {
            Wiring::PwmDir { pwm, dir } => {
                hw.gpio_config(dir, PinMode::Output)?;
                hw.gpio_write(dir, reverse)?;
                // A4988-style setup times do not apply here, but a bridge turns on a
                // microsecond or two after its inputs settle and the direction must be
                // stable before that.
                hw.delay_us(20);
                hw.pwm_set(pwm, self.freq_hz, magnitude)
            }
            Wiring::TwoPwm { in1, in2 } => {
                let (driven, idle) = if reverse { (in2, in1) } else { (in1, in2) };
                hw.pwm_set(idle, self.freq_hz, 0.0)?;
                hw.pwm_set(driven, self.freq_hz, magnitude)
            }
        }
    }

    fn apply(&mut self, hw: &mut dyn Hardware, speed: f32) -> Result<()> {
        if speed.signum() != self.speed.signum() {
            // The old drive comes off before the direction may change: on a stopped motor
            // that is harmless, on a running one it is what keeps the reversal from being
            // a dead short across the bridge.
            self.drive(hw, 0.0)?;
        }
        self.drive(hw, speed)
    }

    /// Command a speed, in percent of the supply, negative for reverse.
    pub fn set_speed(&mut self, hw: &mut dyn Hardware, speed: f32) -> Result<()> {
        match self.apply(hw, speed) {
            Ok(()) => {
                self.speed = speed;
                self.braked = false;
                Ok(())
            }
            Err(e) => {
                // Best effort: when the bridge itself is what failed there is nothing more
                // to write, but the state must not claim a duty cycle that was never set
                // nor one that is now unknown.
                let _ = self.drive(hw, 0.0);
                self.speed = 0.0;
                self.braked = false;
                Err(e)
            }
        }
    }

    /// Stop the motor, returning whether the winding was actually shorted. A `PwmDir` bridge
    /// can only coast, and reporting `braked: true` there would be a lie about the hardware.
    pub fn stop(&mut self, hw: &mut dyn Hardware, brake: bool) -> Result<bool> {
        let shorted = brake && self.can_brake();
        let outcome = if shorted { self.short_winding(hw) } else { self.drive(hw, 0.0) };
        match outcome {
            Ok(()) => {
                self.speed = 0.0;
                self.braked = shorted;
                Ok(shorted)
            }
            Err(e) => {
                let _ = self.drive(hw, 0.0);
                self.speed = 0.0;
                self.braked = false;
                Err(e)
            }
        }
    }
}

/// A speed argument, refused rather than clamped: a caller that asked for 120 % has a bug,
/// and silently driving 100 % hides it.
fn speed_arg(args: &Value) -> Result<f32> {
    let v = j::f64_arg(args, "speed")?;
    if v.is_nan() || !(-100.0..=100.0).contains(&v) {
        return Err(McpError::invalid_params(format!("`speed` must be within -100..=100 percent, got {v}")));
    }
    Ok(v as f32)
}

impl Device for BdcMotor {
    fn info(&self) -> DeviceInfo {
        let pins = self.wiring.pins();
        let mut i = DeviceInfo::new(
            "bdc_motor",
            &format!("Brushed DC motor ({}) on GPIO{pins:?}", self.wiring.name()),
            "pwm",
        );
        i.id = self.id.clone();
        i.pins = pins;
        i.caps = vec!["motor".into(), "pwm".into(), "gpio".into()];
        i.source = "config".into();
        i.detail = json!({
            "wiring": self.wiring.name(),
            "freq_hz": self.freq_hz,
            "speed": self.speed,
            "braked": self.braked,
            "brake_available": self.can_brake(),
        });
        i.tools = vec!["motor_speed".into(), "motor_stop".into(), "motor_state".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let stop = id.clone();
        let state = id.clone();
        vec![
            ToolDef::new(
                "motor_speed",
                "Drive a brushed DC motor; the sign is the direction and the magnitude is the duty cycle",
            )
            .title("Motor speed")
            .category("gpio")
            .requires(&["motor"])
            .schema(object_schema(
                &[("speed", number(-100.0, 100.0, "-100..=100 percent; negative reverses"))],
                &["speed"],
            ))
            .mutating()
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "motor_speed", args, ctx.hw)
            }),
            ToolDef::new("motor_stop", "Stop the motor: coast (no drive) or brake (short the winding)")
                .title("Motor stop")
                .category("gpio")
                .requires(&["motor"])
                .schema(object_schema(
                    &[(
                        "mode",
                        enumeration(&["coast", "brake"], "coast = freewheel, brake = short the winding"),
                    )],
                    &[],
                ))
                .mutating()
                .bind(move |ctx, args| {
                    let id = stop.clone();
                    ctx.devices.call(&id, "motor_stop", args, ctx.hw)
                }),
            ToolDef::new(
                "motor_state",
                "Read the last commanded motor state; an H-bridge cannot be measured",
            )
            .title("Motor state")
            .category("gpio")
            .requires(&["motor"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| {
                let id = state.clone();
                ctx.devices.call(&id, "motor_state", args, ctx.hw)
            }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "motor_speed" => {
                let speed = speed_arg(args)?;
                self.set_speed(hw, speed)?;
                Ok(json!({
                    "ok": true,
                    "speed": self.speed,
                    "direction": if self.speed < 0.0 {
                        "reverse"
                    } else if self.speed > 0.0 {
                        "forward"
                    } else {
                        "stopped"
                    },
                    "pins": self.wiring.pins(),
                }))
            }
            "motor_stop" => {
                let mode = j::opt_str(args, "mode").unwrap_or_else(|| "coast".into());
                let brake = match mode.trim().to_ascii_lowercase().as_str() {
                    "coast" => false,
                    "brake" => true,
                    other => {
                        return Err(McpError::invalid_params(format!(
                            "`mode` must be \"coast\" or \"brake\", got \"{other}\""
                        )))
                    }
                };
                let braked = self.stop(hw, brake)?;
                let note = (brake && !braked).then_some(
                    "this wiring has no brake: IN2 is tied low, so the motor coasts and spins down on its own",
                );
                Ok(json!({
                    "ok": true,
                    "speed": self.speed,
                    "mode": if braked { "brake" } else { "coast" },
                    "braked": braked,
                    "note": note,
                }))
            }
            "motor_state" => Ok(json!({
                "speed": self.speed,
                "braked": self.braked,
                "wiring": self.wiring.name(),
                "pins": self.wiring.pins(),
                "freq_hz": self.freq_hz,
            })),
            other => Err(McpError::not_found(format!("bdc_motor has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code;
    use crate::hardware::BoardInfo;
    use std::collections::BTreeMap;

    /// A bridge that remembers what it was told, and can fail one call the way a pin that
    /// is briefly unusable would.
    #[derive(Default)]
    struct FakeBridge {
        duty: BTreeMap<i32, f32>,
        level: BTreeMap<i32, bool>,
        log: Vec<(i32, f32)>,
        fail_pin: Option<i32>,
        fail_once: bool,
        now_us: u64,
    }

    impl FakeBridge {
        fn duty_of(&self, pin: i32) -> f32 {
            self.duty.get(&pin).copied().unwrap_or(0.0)
        }

        fn level_of(&self, pin: i32) -> bool {
            self.level.get(&pin).copied().unwrap_or(false)
        }
    }

    impl Hardware for FakeBridge {
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
            if self.fail_once && self.fail_pin == Some(pin) {
                self.fail_once = false;
                return Err(McpError::hardware(format!("GPIO{pin} refused the write")));
            }
            self.level.insert(pin, high);
            Ok(())
        }

        fn pwm_set(&mut self, pin: i32, _freq_hz: u32, duty_pct: f32) -> Result<()> {
            if self.fail_once && self.fail_pin == Some(pin) {
                self.fail_once = false;
                return Err(McpError::hardware(format!("PWM on GPIO{pin} failed")));
            }
            self.duty.insert(pin, duty_pct);
            self.log.push((pin, duty_pct));
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
    fn a_speed_reaches_both_inputs_of_a_two_pwm_bridge() {
        let mut hw = FakeBridge::default();
        let mut motor = BdcMotor::new(Wiring::two_pwm(4, 5));
        let v = motor.call("motor_speed", &json!({ "speed": 60 }), &mut hw).unwrap();
        assert_eq!(v["direction"], "forward");
        assert_eq!(hw.duty_of(4), 60.0, "IN1 is driven");
        assert_eq!(hw.duty_of(5), 0.0, "IN2 is held low");

        motor.call("motor_speed", &json!({ "speed": -60 }), &mut hw).unwrap();
        assert_eq!(hw.duty_of(5), 60.0, "reverse drives IN2");
        assert_eq!(hw.duty_of(4), 0.0);
        assert_eq!(motor.speed(), -60.0);
    }

    #[test]
    fn both_ends_of_the_range_are_accepted_and_the_direction_pin_follows() {
        let mut hw = FakeBridge::default();
        let mut motor = BdcMotor::new(Wiring::pwm_dir(6, 7));

        motor.call("motor_speed", &json!({ "speed": 100 }), &mut hw).unwrap();
        assert_eq!(hw.duty_of(6), 100.0);
        assert!(!hw.level_of(7), "forward is DIR low");

        motor.call("motor_speed", &json!({ "speed": -100 }), &mut hw).unwrap();
        assert_eq!(hw.duty_of(6), 100.0, "the magnitude is the duty, the sign is the pin");
        assert!(hw.level_of(7), "reverse is DIR high");
    }

    #[test]
    fn out_of_range_is_refused_before_a_pin_is_touched() {
        let mut hw = FakeBridge::default();
        let mut motor = BdcMotor::new(Wiring::two_pwm(4, 5));
        for bad in [101, -101, 150] {
            let err = motor.call("motor_speed", &json!({ "speed": bad }), &mut hw).unwrap_err();
            assert_eq!(err.code, code::INVALID_PARAMS);
            assert!(err.message.contains("speed"), "the error names the field: {}", err.message);
        }
        assert!(hw.log.is_empty(), "a refused command must not drive anything: {:?}", hw.log);
        assert_eq!(motor.speed(), 0.0);
    }

    #[test]
    fn a_failed_direction_write_leaves_the_bridge_stopped() {
        let mut hw = FakeBridge::default();
        let mut motor = BdcMotor::new(Wiring::pwm_dir(6, 7));
        motor.call("motor_speed", &json!({ "speed": 60 }), &mut hw).unwrap();
        assert_eq!(hw.duty_of(6), 60.0);

        // The reversal fails on the direction pin, after the coast that precedes it.
        hw.fail_pin = Some(7);
        hw.fail_once = true;
        let err = motor.call("motor_speed", &json!({ "speed": -40 }), &mut hw).unwrap_err();
        assert!(err.message.contains("GPIO7"), "unexpected error: {}", err.message);
        assert_eq!(hw.duty_of(6), 0.0, "a failed command must leave no duty cycle");
        assert_eq!(motor.speed(), 0.0, "and the state must not claim a speed");
    }

    #[test]
    fn a_failed_duty_write_leaves_the_bridge_stopped() {
        let mut hw = FakeBridge::default();
        let mut motor = BdcMotor::new(Wiring::two_pwm(4, 5));
        motor.call("motor_speed", &json!({ "speed": 60 }), &mut hw).unwrap();

        hw.fail_pin = Some(5);
        hw.fail_once = true;
        let err = motor.call("motor_speed", &json!({ "speed": -60 }), &mut hw).unwrap_err();
        assert!(err.message.contains("GPIO5"), "unexpected error: {}", err.message);
        assert_eq!(hw.duty_of(4), 0.0, "both inputs must end at zero duty");
        assert_eq!(hw.duty_of(5), 0.0);
        assert_eq!(motor.speed(), 0.0);
    }

    #[test]
    fn stop_brakes_only_where_the_wiring_can() {
        let mut hw = FakeBridge::default();
        let mut motor = BdcMotor::new(Wiring::two_pwm(4, 5));
        motor.call("motor_speed", &json!({ "speed": 50 }), &mut hw).unwrap();
        let v = motor.call("motor_stop", &json!({ "mode": "brake" }), &mut hw).unwrap();
        assert_eq!(v["braked"], true);
        assert_eq!(v["mode"], "brake");
        assert_eq!(hw.duty_of(4), 100.0, "both inputs high shorts the winding");
        assert_eq!(hw.duty_of(5), 100.0);

        let mut hw = FakeBridge::default();
        let mut motor = BdcMotor::new(Wiring::pwm_dir(6, 7));
        motor.call("motor_speed", &json!({ "speed": 50 }), &mut hw).unwrap();
        let v = motor.call("motor_stop", &json!({ "mode": "brake" }), &mut hw).unwrap();
        assert_eq!(v["braked"], false, "pwm+dir cannot short the winding");
        assert_eq!(v["mode"], "coast");
        assert_eq!(hw.duty_of(6), 0.0);
        assert!(v["note"].as_str().is_some_and(|n| !n.is_empty()), "the note explains why");
    }

    #[test]
    fn the_state_tool_reports_the_last_command_and_the_default_stop_coasts() {
        let mut hw = FakeBridge::default();
        let mut motor = BdcMotor::new(Wiring::two_pwm(4, 5));
        motor.call("motor_speed", &json!({ "speed": 25 }), &mut hw).unwrap();
        let v = motor.call("motor_state", &json!({}), &mut hw).unwrap();
        assert_eq!(v["speed"], 25.0);
        assert_eq!(v["wiring"], "two_pwm");

        let v = motor.call("motor_stop", &json!({}), &mut hw).unwrap();
        assert_eq!(v["mode"], "coast");
        assert_eq!(hw.duty_of(4), 0.0);
        assert_eq!(hw.duty_of(5), 0.0);
        assert_eq!(motor.speed(), 0.0);

        let err = motor.call("motor_stop", &json!({ "mode": "slam" }), &mut hw).unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS);
    }
}

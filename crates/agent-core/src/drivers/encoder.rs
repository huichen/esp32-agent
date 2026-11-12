//! Rotary encoder (KY-040 or a bare quadrature encoder) polled from two GPIOs.
//!
//! The [`Hardware`] trait has no interrupts — `gpio_read` is the only way to see a pin —
//! so this decoder polls. **Polling cannot keep up with a fast turn.** At one sample per
//! [`POLL_US`] it sees a little over 1000 edges per second, and a full detent that passes
//! between two samples is simply not seen: what the caller observes is a skipped detent
//! (the count jumps by one more than the turn produced), or no movement at all for that
//! click. It never reports a wrong direction, because only single-bit transitions are
//! counted: a sample that changed both bits at once is a missed edge and is discarded
//! rather than guessed at. A gentle turn — a person turning a knob by hand — is fine; a
//! knob flicked by a finger, or a motor-driven encoder, needs an interrupt.
//!
//! The count is kept in quadrature edges. A detent is reported once the edge count passes a
//! multiple of `edges_per_detent` (4 for a KY-040: one click per full cycle), so the
//! reported position is in clicks, which is what a caller means by "the knob moved".
//!
//! Nothing here can be probed: an encoder is two switches, and a stationary one looks
//! exactly like two pins nobody is driving. `new` takes the pins, as the servo does.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::json as j;
use crate::tool::{integer, object_schema, ToolDef};
use serde_json::{json, Value};

/// Sampling period. 200 us is the shortest gap the ESP32 GPIO read path does not make
/// pointless (a virtual-board read is far cheaper, an ESP-IDF one is a C call).
const POLL_US: u32 = 200;
const MIN_WINDOW_MS: u32 = 10;
const MAX_WINDOW_MS: u32 = 1_000;

/// The direction of one quadrature transition, as ±1, or 0 when no single edge explains it.
///
/// The pins are packed as `a << 1 | b`. The forward cycle is 00 -> 01 -> 11 -> 10 -> 00.
fn edge(prev: (bool, bool), next: (bool, bool)) -> i8 {
    let prev = ((prev.0 as u8) << 1) | prev.1 as u8;
    let next = ((next.0 as u8) << 1) | next.1 as u8;
    match (prev, next) {
        (0b00, 0b01) | (0b01, 0b11) | (0b11, 0b10) | (0b10, 0b00) => 1,
        (0b01, 0b00) | (0b11, 0b01) | (0b10, 0b11) | (0b00, 0b10) => -1,
        // No change, or both bits changed between two samples: an edge was missed and the
        // only honest answer is that this sample says nothing about the direction.
        _ => 0,
    }
}

pub struct Encoder {
    pub a: i32,
    pub b: i32,
    /// Quadrature edges per detent: 4 for a KY-040, 1 for an encoder whose own electronics
    /// already divide the count.
    pub edges_per_detent: i32,
    /// Clicks per revolution, for the reported angle only.
    pub detents_per_rev: i32,
    id: String,
    edges: i32,
    state: (bool, bool),
    last_move: i8,
}

impl Encoder {
    pub fn new(a: i32, b: i32) -> Self {
        Self {
            a,
            b,
            edges_per_detent: 4,
            detents_per_rev: 20,
            id: "encoder".into(),
            edges: 0,
            state: (false, false),
            last_move: 0,
        }
    }

    /// Position in detents (clicks) since construction or the last reset.
    pub fn detents(&self) -> i32 {
        self.edges / self.edges_per_detent.max(1)
    }

    pub fn edges(&self) -> i32 {
        self.edges
    }

    /// Position in degrees, from the declared detents per revolution.
    pub fn angle_deg(&self) -> f32 {
        self.detents() as f32 / self.detents_per_rev.max(1) as f32 * 360.0
    }

    /// Both pins as inputs with pull-ups: a bare encoder and a KY-040 both work with them,
    /// and a floating input on an encoder reads as noise.
    pub fn init(&self, hw: &mut dyn Hardware) -> Result<()> {
        hw.gpio_config(self.a, PinMode::InputPullup)?;
        hw.gpio_config(self.b, PinMode::InputPullup)
    }

    fn sample(&mut self, hw: &mut dyn Hardware) -> Result<(bool, bool)> {
        Ok((hw.gpio_read(self.a)?, hw.gpio_read(self.b)?))
    }

    /// Poll for `window_ms` and fold every edge seen into the count. Returns the number of
    /// edges counted in this window, which is also what a caller can use to tell "nothing
    /// moved" from "moved back to where it was".
    pub fn poll(&mut self, hw: &mut dyn Hardware, window_ms: u32) -> Result<i32> {
        self.init(hw)?;
        let start = hw.millis();
        let mut counted = 0i32;
        loop {
            let next = self.sample(hw)?;
            if next != self.state {
                let d = edge(self.state, next);
                if d != 0 {
                    self.edges = self.edges.saturating_add(d as i32);
                    self.last_move = d;
                    counted += 1;
                }
                // Kept even when the transition was discarded, so a missed edge costs one
                // count and not every sample after it.
                self.state = next;
            }
            if hw.millis().saturating_sub(start) >= window_ms as u64 {
                return Ok(counted);
            }
            hw.delay_us(POLL_US);
        }
    }

    /// Zero the count. The pins are sampled first, so the baseline is the knob's real
    /// position rather than whatever was last seen.
    pub fn reset(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        self.init(hw)?;
        self.state = self.sample(hw)?;
        self.edges = 0;
        self.last_move = 0;
        Ok(())
    }

    fn direction(&self) -> Value {
        match self.last_move {
            1 => json!("cw"),
            -1 => json!("ccw"),
            _ => Value::Null,
        }
    }
}

impl Device for Encoder {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "encoder",
            &format!("Rotary encoder on GPIO{} / GPIO{} (polled)", self.a, self.b),
            "gpio",
        );
        i.id = self.id.clone();
        i.pins = vec![self.a, self.b];
        i.caps = vec!["encoder".into(), "gpio".into()];
        i.source = "config".into();
        i.detail = json!({
            "a": self.a,
            "b": self.b,
            "edges_per_detent": self.edges_per_detent,
            "detents_per_rev": self.detents_per_rev,
            "detents": self.detents(),
            "edges": self.edges,
            "polled": true,
        });
        i.note = "polled: a fast turn can lose a detent".into();
        i.tools = vec!["encoder_read".into(), "encoder_reset".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let reset = id.clone();
        vec![
            ToolDef::new(
                "encoder_read",
                "Poll a rotary encoder for a short window and report the position in detents",
            )
            .title("Encoder read")
            .category("sensor")
            .requires(&["encoder"])
            .schema(object_schema(
                &[(
                    "window_ms",
                    integer(MIN_WINDOW_MS as i64, MAX_WINDOW_MS as i64, "how long to listen, default 50"),
                )],
                &[],
            ))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "encoder_read", args, ctx.hw)
            }),
            ToolDef::new("encoder_reset", "Zero the encoder count and note where the knob is now")
                .title("Encoder reset")
                .category("gpio")
                .requires(&["encoder"])
                .schema(object_schema(&[], &[]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = reset.clone();
                    ctx.devices.call(&id, "encoder_reset", args, ctx.hw)
                }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "encoder_read" => {
                let window =
                    j::ranged_i64(args, "window_ms", 50, MIN_WINDOW_MS as i64, MAX_WINDOW_MS as i64)? as u32;
                let counted = self.poll(hw, window)?;
                Ok(json!({
                    "detents": self.detents(),
                    "edges": self.edges,
                    "edges_in_window": counted,
                    "direction": self.direction(),
                    "angle_deg": (self.angle_deg() * 10.0).round() / 10.0,
                    "window_ms": window,
                }))
            }
            "encoder_reset" => {
                self.reset(hw)?;
                Ok(json!({ "ok": true, "detents": 0, "edges": 0 }))
            }
            other => Err(McpError::not_found(format!("encoder has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code;
    use crate::hardware::BoardInfo;

    /// A dial that walks a scripted sequence of (A, B) levels, one entry per sample, and
    /// then holds the last one.
    struct FakeDial {
        a: i32,
        b: i32,
        script: Vec<(bool, bool)>,
        idx: usize,
        now_us: u64,
    }

    impl FakeDial {
        fn new(script: &[(bool, bool)]) -> Self {
            Self { a: 10, b: 11, script: script.to_vec(), idx: 0, now_us: 0 }
        }
    }

    impl Hardware for FakeDial {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, _pin: i32, _mode: PinMode) -> Result<()> {
            Ok(())
        }

        fn gpio_read(&mut self, pin: i32) -> Result<bool> {
            let cur = self.script[self.idx.min(self.script.len() - 1)];
            if pin == self.a {
                Ok(cur.0)
            } else if pin == self.b {
                let v = cur.1;
                if self.idx + 1 < self.script.len() {
                    self.idx += 1;
                }
                Ok(v)
            } else {
                Err(McpError::not_found(format!("nothing on GPIO{pin}")))
            }
        }

        fn delay_us(&mut self, us: u32) {
            self.now_us += us as u64;
        }

        fn millis(&mut self) -> u64 {
            self.now_us / 1000
        }
    }

    /// The forward cycle, then stopped at 00 again.
    const FORWARD: &[(bool, bool)] =
        &[(false, false), (false, true), (true, true), (true, false), (false, false)];

    /// The same cycle the other way round.
    const BACKWARD: &[(bool, bool)] =
        &[(false, false), (true, false), (true, true), (false, true), (false, false)];

    #[test]
    fn a_full_quadrature_cycle_forward_and_back_is_one_detent_each_way() {
        let mut hw = FakeDial::new(FORWARD);
        let mut dev = Encoder::new(10, 11);
        let v = dev.call("encoder_read", &json!({ "window_ms": 10 }), &mut hw).unwrap();
        assert_eq!(v["edges"], 4, "one forward cycle is four edges");
        assert_eq!(v["edges_in_window"], 4);
        assert_eq!(v["detents"], 1, "four edges are one click on a KY-040");
        assert_eq!(v["direction"], "cw");

        let mut hw = FakeDial::new(BACKWARD);
        let mut dev = Encoder::new(10, 11);
        let v = dev.call("encoder_read", &json!({ "window_ms": 10 }), &mut hw).unwrap();
        assert_eq!(v["edges"], -4);
        assert_eq!(v["detents"], -1);
        assert_eq!(v["direction"], "ccw");

        // And the cycle back to the start leaves the count where it began.
        let mut hw = FakeDial::new(FORWARD);
        let mut dev = Encoder::new(10, 11);
        dev.call("encoder_read", &json!({ "window_ms": 10 }), &mut hw).unwrap();
        let mut hw = FakeDial::new(BACKWARD);
        dev.call("encoder_read", &json!({ "window_ms": 10 }), &mut hw).unwrap();
        assert_eq!(dev.edges(), 0);
        assert_eq!(dev.detents(), 0);
    }

    #[test]
    fn a_transition_that_changed_both_bits_is_discarded() {
        // A turn fast enough that a sample missed a state: both bits differ, and the only
        // honest reading is "one edge was lost", not a direction.
        assert_eq!(edge((false, false), (true, true)), 0);
        assert_eq!(edge((true, true), (false, false)), 0);
        assert_eq!(edge((false, true), (true, false)), 0);
        // One edge either way is not ambiguous.
        assert_eq!(edge((false, false), (false, true)), 1);
        assert_eq!(edge((false, false), (true, false)), -1);
        assert_eq!(edge((true, true), (true, true)), 0);

        let mut hw = FakeDial::new(&[(false, false), (true, true), (false, false)]);
        let mut dev = Encoder::new(10, 11);
        let v = dev.call("encoder_read", &json!({ "window_ms": 10 }), &mut hw).unwrap();
        assert_eq!(v["edges"], 0, "a missed edge is not counted as movement");
        assert_eq!(v["detents"], 0);
    }

    #[test]
    fn both_ends_of_the_window_are_accepted() {
        for window in [MIN_WINDOW_MS, MAX_WINDOW_MS] {
            let mut hw = FakeDial::new(FORWARD);
            let mut dev = Encoder::new(10, 11);
            let v = dev.call("encoder_read", &json!({ "window_ms": window }), &mut hw).unwrap();
            assert_eq!(v["window_ms"], window);
            assert_eq!(v["detents"], 1);
        }
    }

    #[test]
    fn a_window_out_of_range_is_refused() {
        let mut hw = FakeDial::new(FORWARD);
        let mut dev = Encoder::new(10, 11);
        for bad in [0, 9, 1_001, 60_000] {
            let err = dev.call("encoder_read", &json!({ "window_ms": bad }), &mut hw).unwrap_err();
            assert_eq!(err.code, code::INVALID_PARAMS);
            assert!(err.message.contains("window_ms"), "unexpected error: {}", err.message);
        }
        assert_eq!(dev.edges(), 0, "a refused read does not count anything");
    }

    #[test]
    fn reset_zeroes_the_count_and_keeps_the_current_level() {
        let mut hw = FakeDial::new(FORWARD);
        let mut dev = Encoder::new(10, 11);
        dev.call("encoder_read", &json!({ "window_ms": 10 }), &mut hw).unwrap();
        assert_eq!(dev.detents(), 1);

        let v = dev.call("encoder_reset", &json!({}), &mut hw).unwrap();
        assert_eq!(v["detents"], 0);
        assert_eq!(dev.detents(), 0);
        assert_eq!(dev.edges(), 0);

        // The dial has not moved, so a second read must not invent an edge.
        let v = dev.call("encoder_read", &json!({ "window_ms": 10 }), &mut hw).unwrap();
        assert_eq!(v["detents"], 0);
    }
}

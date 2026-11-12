//! HX711 24-bit load-cell amplifier on two GPIOs: DOUT in, PD_SCK out.
//!
//! There is no register to read and no address to scan: the part is bit-banged.
//! Presence is therefore the converter's own ready handshake — DOUT sits high
//! between conversions and falls when a result is waiting — followed by a frame
//! that a live cell produced. A frame of all zeros or all ones is what an
//! undriven line returns, and a connected cell always shows a few counts of
//! noise, so both frames are reported as "no part" instead of as a weight.
//!
//! Calibration is two numbers, exactly as on a bench scale: a tare offset in raw
//! counts and a factor in raw counts per gram. `hx711_tare` sets the first,
//! `hx711_read` with a `factor` remembers the second.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::json as j;
use crate::tool::{integer, number, object_schema, ToolDef};
use serde_json::{json, Value};

/// Extra PD_SCK pulses after the 24 data bits choose the gain and channel of the
/// *next* conversion (datasheet, gain selection table): 1 = A/128, 2 = B/32,
/// 3 = A/64.
const GAIN_PULSES: [(u8, u8); 3] = [(128, 1), (32, 2), (64, 3)];

/// How long to wait for DOUT to fall. The slowest HX711 runs at 10 conversions
/// per second, so a result is at most 100 ms away; 400 ms leaves room for a
/// conversion that started just before the call.
const READY_TIMEOUT_MS: u32 = 400;

/// The trait offers a microsecond delay only; every time in the HX711 datasheet
/// is in milliseconds, so this is the wrapper the call sites read.
fn delay_ms(hw: &mut dyn Hardware, ms: u32) {
    hw.delay_us(ms.saturating_mul(1_000));
}

/// One half of a bit period. The datasheet allows 0.2..=50 us for both PD_SCK
/// phases, so 1 us is inside the window at either sample rate and gives a ~500 kHz
/// bit clock.
// TODO(verify): timing on hardware.
fn bit_delay(hw: &mut dyn Hardware) {
    hw.delay_us(1);
}

pub struct Hx711 {
    pub dout: i32,
    pub sck: i32,
    gain: u8,
    /// Raw counts with nothing on the scale.
    offset: i32,
    /// Raw counts per gram; 0.0 until a factor has been supplied.
    counts_per_gram: f32,
    id: String,
    last: Option<i32>,
}

impl Hx711 {
    /// Read one frame; `None` when the part never signalled ready or the frame is
    /// one only an undriven line produces.
    pub fn identify(hw: &mut dyn Hardware, dout: i32, sck: i32) -> Option<i32> {
        match Self::read_raw(hw, dout, sck, 128, READY_TIMEOUT_MS) {
            Ok(raw) if raw != 0 && raw != -1 => Some(raw),
            _ => None,
        }
    }

    pub fn probe(hw: &mut dyn Hardware, dout: i32, sck: i32, id_suffix: usize) -> Result<Self> {
        let raw = Self::identify(hw, dout, sck).ok_or_else(|| {
            McpError::not_found(format!(
                "no HX711 on DOUT GPIO{dout} / SCK GPIO{sck}: no ready handshake or an empty frame"
            ))
        })?;
        Ok(Self {
            dout,
            sck,
            gain: 128,
            offset: 0,
            counts_per_gram: 0.0,
            id: if id_suffix > 1 { format!("hx711_{id_suffix}") } else { "hx711".into() },
            last: Some(raw),
        })
    }

    /// Clock out one conversion and return the signed 24-bit result.
    ///
    /// The sequence is the datasheet's figure: wait for DOUT low, then 24 clock
    /// pulses MSB first, then 1..=3 more pulses that pick the gain of the next
    /// conversion.
    fn read_raw(hw: &mut dyn Hardware, dout: i32, sck: i32, gain: u8, timeout_ms: u32) -> Result<i32> {
        let pulses = GAIN_PULSES
            .iter()
            .find(|(g, _)| *g == gain)
            .map(|(_, p)| *p)
            .ok_or_else(|| McpError::invalid_params("gain must be 128, 64 or 32"))?;
        hw.gpio_config(dout, PinMode::Input)?;
        hw.gpio_config(sck, PinMode::Output)?;
        hw.gpio_write(sck, false)?;

        let mut waited = 0u32;
        while hw.gpio_read(dout)? {
            if waited >= timeout_ms {
                return Err(McpError::hardware(format!(
                    "HX711 not ready after {timeout_ms} ms (is DOUT on GPIO{dout} driven?)"
                )));
            }
            delay_ms(hw, 1);
            waited += 1;
        }

        let mut raw: i32 = 0;
        for _ in 0..24 {
            hw.gpio_write(sck, true)?;
            bit_delay(hw);
            // The bit becomes valid on the rising edge and holds until the next one.
            raw = (raw << 1) | i32::from(hw.gpio_read(dout)?);
            hw.gpio_write(sck, false)?;
            bit_delay(hw);
        }
        // Two's complement in 24 bits.
        if raw & 0x0080_0000 != 0 {
            raw |= !0x00FF_FFFF;
        }
        for _ in 0..pulses {
            hw.gpio_write(sck, true)?;
            bit_delay(hw);
            hw.gpio_write(sck, false)?;
            bit_delay(hw);
        }
        Ok(raw)
    }

    /// Median of `samples` conversions. The median rather than the mean because a
    /// single disturbed conversion (someone touching the platform) would otherwise
    /// drag every reading.
    pub fn sample(&mut self, hw: &mut dyn Hardware, samples: u32, gain: u8) -> Result<i32> {
        let n = samples.max(1);
        let mut v = Vec::with_capacity(n as usize);
        for _ in 0..n {
            v.push(Self::read_raw(hw, self.dout, self.sck, gain, READY_TIMEOUT_MS)?);
        }
        v.sort_unstable();
        let raw = v[v.len() / 2];
        self.gain = gain;
        self.last = Some(raw);
        Ok(raw)
    }

    /// Take the current reading as the empty-scale point.
    pub fn tare(&mut self, hw: &mut dyn Hardware, samples: u32) -> Result<i32> {
        let raw = self.sample(hw, samples, self.gain)?;
        self.offset = raw;
        Ok(raw)
    }

    /// Grams for a raw reading, or `None` while the scale is uncalibrated.
    pub fn grams(&self, raw: i32) -> Option<f32> {
        if self.counts_per_gram > 0.0 {
            Some((raw - self.offset) as f32 / self.counts_per_gram)
        } else {
            None
        }
    }
}

impl Device for Hx711 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "hx711",
            &format!("HX711 load cell (DOUT GPIO{}, SCK GPIO{})", self.dout, self.sck),
            "gpio",
        );
        i.id = self.id.clone();
        i.pins = vec![self.dout, self.sck];
        i.caps = vec!["hx711".into(), "gpio".into(), "weight".into()];
        i.source = "config".into();
        i.detail = json!({
            "gain": self.gain,
            "offset": self.offset,
            "counts_per_gram": self.counts_per_gram,
            "calibrated": self.counts_per_gram > 0.0,
            "last_raw": self.last,
        });
        i.tools = vec!["hx711_read".into(), "hx711_tare".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let tare = id.clone();
        vec![
            ToolDef::new(
                "hx711_read",
                "Read the HX711 load cell: raw counts plus grams when a calibration factor is known. Pass `factor` (counts per gram) once to calibrate, after taring",
            )
            .title("Weight")
            .category("sensor")
            .requires(&["hx711"])
            .schema(object_schema(
                &[
                    ("samples", integer(1, 32, "median of this many conversions, default 4")),
                    ("gain", integer(32, 128, "gain of the next conversion: 128 (default), 64 or 32")),
                    ("factor", number(0.000_001, 1_000_000.0, "counts per gram; remembered for later reads")),
                ],
                &[],
            ))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "hx711_read", args, ctx.hw)
            }),
            ToolDef::new(
                "hx711_tare",
                "Use the current reading as the empty-scale point (zero the scale)",
            )
            .title("Tare")
            .category("sensor")
            .requires(&["hx711"])
            .schema(object_schema(
                &[("samples", integer(1, 32, "median of this many conversions, default 8"))],
                &[],
            ))
            .mutating()
            .bind(move |ctx, args| {
                let id = tare.clone();
                ctx.devices.call(&id, "hx711_tare", args, ctx.hw)
            }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "hx711_read" => {
                let samples = j::ranged_i64(args, "samples", 4, 1, 32)? as u32;
                let gain = j::ranged_i64(args, "gain", 128, 32, 128)? as u8;
                if !GAIN_PULSES.iter().any(|(g, _)| *g == gain) {
                    return Err(McpError::invalid_params(format!(
                        "gain {gain} does not exist; the HX711 offers 128, 64 and 32"
                    )));
                }
                if let Some(factor) = j::opt_f64(args, "factor") {
                    if factor <= 0.0 {
                        return Err(McpError::invalid_params("`factor` must be greater than zero"));
                    }
                    self.counts_per_gram = factor as f32;
                }
                let raw = self.sample(hw, samples, gain)?;
                let mut out = serde_json::Map::new();
                out.insert("raw".into(), json!(raw));
                out.insert("gain".into(), json!(gain));
                out.insert("samples".into(), json!(samples));
                out.insert("offset".into(), json!(self.offset));
                match self.grams(raw) {
                    Some(g) => {
                        // Rounded in f64: a weight printed as 1.3999999 makes a caller
                        // think the calibration is off.
                        let g = (f64::from(g) * 10.0).round() / 10.0;
                        out.insert("grams".into(), json!(g));
                        out.insert("calibrated".into(), json!(true));
                    }
                    None => {
                        out.insert("calibrated".into(), json!(false));
                        out.insert(
                            "note".into(),
                            json!("no calibration factor yet; pass `factor` = counts per gram"),
                        );
                    }
                }
                Ok(json!(out))
            }
            "hx711_tare" => {
                let samples = j::ranged_i64(args, "samples", 8, 1, 32)? as u32;
                let offset = self.tare(hw, samples)?;
                Ok(json!({
                    "ok": true,
                    "offset": offset,
                    "samples": samples,
                    "calibrated": self.counts_per_gram > 0.0,
                }))
            }
            other => Err(McpError::not_found(format!("hx711 has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;

    /// A load cell wired to the driver: DOUT is high until a conversion is ready,
    /// then one bit per rising edge on PD_SCK, exactly as the datasheet's figure
    /// shows. Every clock edge is recorded, which is the bit-bang sequence the
    /// tests pin down.
    struct FakeCell {
        value: i32,
        ready: bool,
        bit: usize,
        /// Set once the 24 data bits are out: the next clock pulses only choose the
        /// gain, and DOUT stays high until the next conversion is ready.
        frame_done: bool,
        dout_level: bool,
        sck_trace: Vec<bool>,
        mode_log: Vec<(i32, PinMode)>,
        fail_gpio: bool,
    }

    impl FakeCell {
        fn reporting(value: i32) -> Self {
            Self {
                value,
                ready: true,
                bit: 0,
                frame_done: false,
                dout_level: false,
                sck_trace: Vec::new(),
                mode_log: Vec::new(),
                fail_gpio: false,
            }
        }

        fn not_ready() -> Self {
            Self { ready: false, dout_level: true, ..Self::reporting(0) }
        }

        fn rising_edges(&self) -> usize {
            self.sck_trace.windows(2).filter(|w| !w[0] && w[1]).count()
        }
    }

    impl Hardware for FakeCell {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, pin: i32, mode: PinMode) -> Result<()> {
            if self.fail_gpio {
                return Err(McpError::unsupported("GPIO is not available on this board"));
            }
            self.mode_log.push((pin, mode));
            Ok(())
        }

        fn gpio_read(&mut self, _pin: i32) -> Result<bool> {
            if self.frame_done {
                // The frame is out and this fake always has the next result ready.
                self.frame_done = false;
                self.bit = 0;
                self.dout_level = false;
            }
            Ok(self.dout_level)
        }

        fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
            if pin != 4 {
                return Ok(());
            }
            let was = self.sck_trace.last().copied().unwrap_or(false);
            self.sck_trace.push(high);
            if !high || was || !self.ready {
                return Ok(());
            }
            if self.bit >= 24 {
                // 25th..27th pulse: gain selection, DOUT back high until the next
                // conversion completes.
                self.frame_done = true;
                self.dout_level = true;
                return Ok(());
            }
            self.dout_level = (self.value >> (23 - self.bit)) & 1 == 1;
            self.bit += 1;
            Ok(())
        }
    }

    fn pins() -> (i32, i32) {
        (3, 4)
    }

    fn driver() -> Hx711 {
        Hx711 { dout: 3, sck: 4, gain: 128, offset: 0, counts_per_gram: 0.0, id: "hx711".into(), last: None }
    }

    #[test]
    fn a_reading_is_24_clocks_plus_one_gain_pulse() {
        let mut cell = FakeCell::reporting(1_234_567);
        let raw = Hx711::read_raw(&mut cell, pins().0, pins().1, 128, 5).expect("a reading");
        assert_eq!(raw, 1_234_567, "the frame is MSB first and unsigned");
        assert_eq!(cell.rising_edges(), 25, "24 data bits plus 1 pulse for gain 128");
        assert_eq!(cell.sck_trace.len(), 51, "one idle-low write, then 25 high/low pairs");
        assert!(cell.sck_trace.windows(2).skip(1).all(|w| w[0] != w[1]), "the clock may not glitch");
    }

    #[test]
    fn gain_64_and_32_ask_for_three_and_two_pulses() {
        for (gain, pulses) in [(64u8, 27usize), (32u8, 26usize)] {
            let mut cell = FakeCell::reporting(0x0010_0000);
            let raw = Hx711::read_raw(&mut cell, 3, 4, gain, 5).expect("a reading");
            assert_eq!(raw, 0x0010_0000);
            assert_eq!(cell.rising_edges(), pulses, "gain {gain}");
        }
        let bad = Hx711::read_raw(&mut FakeCell::reporting(1), 3, 4, 100, 5).unwrap_err();
        assert_eq!(bad.code, crate::error::code::INVALID_PARAMS);
    }

    #[test]
    fn negative_counts_are_sign_extended_to_24_bits() {
        let mut cell = FakeCell::reporting(0x00FF_0000);
        assert_eq!(Hx711::read_raw(&mut cell, 3, 4, 128, 5).unwrap(), -65_536);
        let mut cell = FakeCell::reporting(0x007F_FFFF);
        assert_eq!(Hx711::read_raw(&mut cell, 3, 4, 128, 5).unwrap(), 8_388_607);
    }

    #[test]
    fn probe_refuses_a_frame_only_an_undriven_line_returns() {
        for value in [0x000000, 0x00FF_FFFF] {
            let mut cell = FakeCell::reporting(value);
            assert_eq!(Hx711::identify(&mut cell, 3, 4), None, "frame {value:#08x}");
            let e = Hx711::probe(&mut cell, 3, 4, 1).err().expect("the probe must refuse this frame");
            assert!(e.message.contains("no HX711"), "{}", e.message);
        }
        // A live cell always carries a little noise, and that is what proves the part.
        assert_eq!(Hx711::identify(&mut FakeCell::reporting(8_321), 3, 4), Some(8_321));
    }

    #[test]
    fn a_cell_that_never_gets_ready_times_out_instead_of_hanging() {
        let mut cell = FakeCell::not_ready();
        let e = Hx711::read_raw(&mut cell, 3, 4, 128, 3).unwrap_err();
        assert_eq!(e.code, crate::error::code::HARDWARE);
        assert!(e.message.contains("not ready"), "{}", e.message);
        assert_eq!(cell.rising_edges(), 0, "no clock before the part is ready");
    }

    #[test]
    fn tare_then_a_known_factor_turns_counts_into_grams() {
        let mut cell = FakeCell::reporting(1000);
        let mut d = driver();
        assert_eq!(d.tare(&mut cell, 4).unwrap(), 1000);
        cell.value = 1140;
        let raw = d.sample(&mut cell, 4, 128).unwrap();
        assert_eq!(raw, 1140);
        assert_eq!(d.grams(raw), None, "uncalibrated until a factor arrives");
        d.counts_per_gram = 100.0;
        assert!((d.grams(raw).unwrap() - 1.4).abs() < 1e-4);

        let out = d.call("hx711_read", &json!({ "samples": 1 }), &mut cell).unwrap();
        assert_eq!(out["raw"], 1140);
        assert_eq!(out["grams"], 1.4);
        assert_eq!(out["calibrated"], true);
    }

    #[test]
    fn tools_are_named_and_unknown_ones_are_refused() {
        let mut d = driver();
        let names: Vec<String> = d.tools().iter().map(|t| t.spec.name.clone()).collect();
        assert_eq!(names, vec!["hx711_read".to_string(), "hx711_tare".to_string()]);
        let mut cell = FakeCell::reporting(7);
        let e = d.call("hx711_weigh", &json!({}), &mut cell).unwrap_err();
        assert_eq!(e.code, crate::error::code::NOT_FOUND);
        assert!(e.message.contains("hx711"), "{}", e.message);
    }
}

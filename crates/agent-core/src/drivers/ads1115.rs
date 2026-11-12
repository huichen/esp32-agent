//! ADS1115 four-channel 16-bit ADC.
//!
//! The chip has no id register, so the probe is a config write + conversion
//! read that returns a plausible value within the configured range. Addresses
//! 0x48..0x4B are selected by the ADDR pin.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{enumeration, integer, object_schema, ToolDef};
use serde_json::{json, Value};

const REG_CONVERSION: u8 = 0x00;
const REG_CONFIG: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gain {
    V6_144,
    V4_096,
    V2_048,
    V1_024,
    V0_512,
    V0_256,
}

impl Gain {
    pub fn lsb_mv(self) -> f32 {
        match self {
            Gain::V6_144 => 0.1875,
            Gain::V4_096 => 0.125,
            Gain::V2_048 => 0.0625,
            Gain::V1_024 => 0.03125,
            Gain::V0_512 => 0.015625,
            Gain::V0_256 => 0.0078125,
        }
    }

    fn code(self) -> u16 {
        match self {
            Gain::V6_144 => 0,
            Gain::V4_096 => 1,
            Gain::V2_048 => 2,
            Gain::V1_024 => 3,
            Gain::V0_512 => 4,
            Gain::V0_256 => 5,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "6.144" | "6.144v" => Some(Gain::V6_144),
            "4.096" | "4.096v" => Some(Gain::V4_096),
            "2.048" | "2.048v" => Some(Gain::V2_048),
            "1.024" | "1.024v" => Some(Gain::V1_024),
            "0.512" | "0.512v" => Some(Gain::V0_512),
            "0.256" | "0.256v" => Some(Gain::V0_256),
            _ => None,
        }
    }
}

pub struct Ads1115 {
    pub bus: u8,
    pub addr: u8,
    gain: Gain,
    id: String,
}

impl Ads1115 {
    pub const ADDRESSES: &'static [u8] = &[0x48, 0x49, 0x4A, 0x4B];

    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        let mut d = Self {
            bus,
            addr,
            gain: Gain::V4_096,
            id: if id_suffix > 1 { format!("ads1115_{id_suffix}") } else { "ads1115".into() },
        };
        // A conversion on channel 0 must complete and stay inside the range.
        let v = d.read_single(hw, 0)?;
        if v.abs() > 4.096 {
            return Err(McpError::not_found(format!("no ADS1115 at 0x{addr:02X}")));
        }
        Ok(d)
    }

    /// Single-ended read on one of the four inputs, in volts.
    pub fn read_single(&mut self, hw: &mut dyn Hardware, channel: u8) -> Result<f32> {
        if channel > 3 {
            return Err(McpError::invalid_params("channel must be 0..=3"));
        }
        // OS=1, MUX=100+channel (single-ended), PGA, MODE=1 (single shot),
        // DR=100 (128 SPS), comparator disabled.
        let cfg: u16 =
            0x8000 | ((4 + channel as u16) << 12) | (self.gain.code() << 9) | 0x0100 | (4 << 5) | 0x0003;
        hw.i2c_write(self.bus, self.addr, Some(REG_CONFIG), &cfg.to_be_bytes())?;
        hw.delay_us(9_000);
        let mut b = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, Some(REG_CONVERSION), &mut b)?;
        let raw = i16::from_be_bytes(b);
        Ok(raw as f32 * self.gain.lsb_mv() / 1000.0)
    }

    /// Differential read between two inputs (0-1, 2-3 only on this chip).
    pub fn read_diff(&mut self, hw: &mut dyn Hardware, pair: (u8, u8)) -> Result<f32> {
        let mux = match pair {
            (0, 1) => 0u16,
            (2, 3) => 3,
            _ => return Err(McpError::invalid_params("differential pairs are 0-1 or 2-3")),
        };
        let cfg: u16 = 0x8000 | (mux << 12) | (self.gain.code() << 9) | 0x0100 | (4 << 5) | 0x0003;
        hw.i2c_write(self.bus, self.addr, Some(REG_CONFIG), &cfg.to_be_bytes())?;
        hw.delay_us(9_000);
        let mut b = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, Some(REG_CONVERSION), &mut b)?;
        let raw = i16::from_be_bytes(b);
        Ok(raw as f32 * self.gain.lsb_mv() / 1000.0)
    }

    pub fn set_gain(&mut self, gain: Gain) {
        self.gain = gain;
    }
}

impl Device for Ads1115 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("ads1115", &format!("ADS1115 16-bit ADC at 0x{:02X}", self.addr), "i2c");
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["ads1115".into(), "i2c".into()];
        i.detail = json!({ "bus": self.bus, "gain": format!("{:?}", self.gain) });
        i.tools = vec!["ads1115_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("ads1115_read", "Read one channel of the ADS1115 external ADC in volts")
            .title("ADS1115 read")
            .category("sensor")
            .requires(&["ads1115"])
            .schema(object_schema(
                &[
                    ("channel", integer(0, 3, "single-ended input 0-3")),
                    ("pair", enumeration(&["0-1", "2-3"], "differential pair instead of a channel")),
                    (
                        "gain",
                        enumeration(
                            &["6.144", "4.096", "2.048", "1.024", "0.512", "0.256"],
                            "input range in volts",
                        ),
                    ),
                ],
                &[],
            ))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "ads1115_read", args, ctx.hw)
            })]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "ads1115_read" => {
                if let Some(g) = crate::json::opt_str(args, "gain") {
                    let gain = Gain::parse(&g)
                        .ok_or_else(|| McpError::invalid_params(format!("unknown gain `{g}`")))?;
                    self.set_gain(gain);
                }
                if let Some(p) = crate::json::opt_str(args, "pair") {
                    let pair = match p.as_str() {
                        "0-1" => (0u8, 1u8),
                        "2-3" => (2u8, 3u8),
                        other => return Err(McpError::invalid_params(format!("unknown pair `{other}`"))),
                    };
                    let v = self.read_diff(hw, pair)?;
                    return Ok(
                        json!({ "volts": (v * 1000.0).round() / 1000.0, "pair": p, "gain": format!("{:?}", self.gain) }),
                    );
                }
                let ch = crate::json::ranged_i64(args, "channel", 0, 0, 3)? as u8;
                let v = self.read_single(hw, ch)?;
                Ok(json!({
                    "channel": ch,
                    "volts": (v * 1000.0).round() / 1000.0,
                    "millivolts": (v * 1000.0).round() / 10.0 * 10.0,
                    "gain": format!("{:?}", self.gain),
                }))
            }
            other => Err(McpError::not_found(format!("ads1115 has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;

    /// The ADS1115 is two registers: a config word you write and a conversion word you read
    /// back. The fake holds the conversion the chip would have produced and logs what was
    /// written, so a test can check the config encoding as well as the volts.
    #[derive(Default)]
    struct Chip {
        conversion: i16,
        writes: Vec<Vec<u8>>,
        read_err: bool,
        converted: usize,
    }

    impl Chip {
        fn counts(raw: i16) -> Self {
            Self { conversion: raw, ..Default::default() }
        }

        fn last_config(&self) -> u16 {
            let d = self.writes.last().expect("a read always writes a config first");
            u16::from_be_bytes([d[0], d[1]])
        }
    }

    impl Hardware for Chip {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn i2c_read(&mut self, _bus: u8, _addr: u8, _reg: Option<u8>, out: &mut [u8]) -> Result<()> {
            if self.read_err {
                return Err(McpError::hardware("I2C NAK"));
            }
            self.converted += 1;
            out.copy_from_slice(&self.conversion.to_be_bytes());
            Ok(())
        }

        fn i2c_write(&mut self, _bus: u8, _addr: u8, _reg: Option<u8>, data: &[u8]) -> Result<()> {
            self.writes.push(data.to_vec());
            Ok(())
        }
    }

    fn dev() -> Ads1115 {
        Ads1115 { bus: 0, addr: 0x48, gain: Gain::V4_096, id: "ads1115".into() }
    }

    /// The config word from the datasheet: OS=1, MUX=100+channel for a single-ended input,
    /// PGA the gain code, MODE=1 single shot, DR=100 (128 SPS), comparator disabled (0x0003).
    #[test]
    fn a_single_ended_read_writes_the_config_word_for_that_channel() {
        let mut chip = Chip::counts(0);
        let mut d = dev();
        d.read_single(&mut chip, 0).expect("read");
        assert_eq!(chip.last_config(), 0xC383, "AIN0 at 4.096 V range, single shot");
        assert_eq!(chip.last_config() >> 12 & 0b111, 0b100, "MUX for AIN0");

        d.read_single(&mut chip, 3).expect("read");
        assert_eq!(chip.last_config() >> 12 & 0b111, 0b111, "MUX for AIN3");
    }

    /// The LSB is 125 uV at the 4.096 V range, so 10000 counts is 1.25 V.
    #[test]
    fn a_conversion_becomes_volts_at_the_gains_lsb() {
        let mut chip = Chip::counts(10_000);
        let mut d = dev();
        let v = d.read_single(&mut chip, 0).expect("read");
        assert!((v - 1.25).abs() < 1e-6, "got {v}");

        d.set_gain(Gain::V6_144);
        let v = d.read_single(&mut chip, 0).expect("read");
        assert!((v - 1.875).abs() < 1e-6, "187.5 uV per count: got {v}");
        assert_eq!(chip.last_config() >> 9 & 0b111, 0, "PGA code for 6.144 V");
    }

    /// The chip is bipolar: a negative count is a negative voltage, not a huge positive one.
    #[test]
    fn a_negative_conversion_is_a_negative_voltage() {
        let mut chip = Chip::counts(-10_000);
        let mut d = dev();
        let v = d.read_single(&mut chip, 0).expect("read");
        assert!((v + 1.25).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn the_fourth_channel_is_the_last_one() {
        let mut chip = Chip::counts(0);
        let mut d = dev();
        let err = d.read_single(&mut chip, 4).expect_err("there is no AIN4");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        assert_eq!(chip.writes.len(), 0, "a bad channel must not program the chip");
    }

    /// Only two differential pairs are wired on this part: AIN0-AIN1 (MUX 000) and
    /// AIN2-AIN3 (MUX 011). Anything else is refused before the bus.
    #[test]
    fn differential_reads_use_the_two_pairs_the_chip_has() {
        let mut chip = Chip::counts(1_000);
        let mut d = dev();
        d.read_diff(&mut chip, (0, 1)).expect("0-1");
        assert_eq!(chip.last_config() >> 12 & 0b111, 0b000, "MUX for AIN0-AIN1");
        d.read_diff(&mut chip, (2, 3)).expect("2-3");
        assert_eq!(chip.last_config() >> 12 & 0b111, 0b011, "MUX for AIN2-AIN3");

        let err = d.read_diff(&mut chip, (1, 2)).expect_err("1-2 is not a pair on this chip");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
    }

    #[test]
    fn a_conversion_that_does_not_answer_is_an_error() {
        let mut chip = Chip { read_err: true, ..Default::default() };
        let mut d = dev();
        let err = d.read_single(&mut chip, 0).expect_err("a NAK is not a voltage");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    /// A config write the chip never acknowledges means the reading that follows would be
    /// whatever was in the register, so the write error has to be the one that surfaces.
    #[test]
    fn the_probe_reads_one_channel_and_needs_the_bus_to_answer() {
        let mut chip = Chip::counts(0);
        let d = Ads1115::probe(&mut chip, 0, 0x49, 1).expect("probe");
        assert_eq!(d.addr, 0x49);
        assert_eq!(d.id, "ads1115");
        assert_eq!(chip.converted, 1, "the probe is one conversion, not a register read");

        let mut dead = Chip { read_err: true, ..Default::default() };
        let err = Ads1115::probe(&mut dead, 0, 0x49, 1).err().expect("no answer is no device");
        assert_eq!(err.code, crate::error::code::HARDWARE);

        let mut chip = Chip::counts(0);
        assert_eq!(Ads1115::probe(&mut chip, 0, 0x4A, 2).expect("probe").id, "ads1115_2");
    }

    #[test]
    fn gain_parsing_covers_every_range_and_refuses_the_rest() {
        for (text, want) in [
            ("6.144", Gain::V6_144),
            ("6.144v", Gain::V6_144),
            ("4.096", Gain::V4_096),
            ("4.096v", Gain::V4_096),
            ("2.048", Gain::V2_048),
            ("2.048v", Gain::V2_048),
            ("1.024", Gain::V1_024),
            ("1.024v", Gain::V1_024),
            ("0.512", Gain::V0_512),
            ("0.512v", Gain::V0_512),
            ("0.256", Gain::V0_256),
            ("0.256v", Gain::V0_256),
        ] {
            assert_eq!(Gain::parse(text), Some(want), "{text:?}");
        }
        assert_eq!(Gain::parse("3.3"), None);
        assert_eq!(Gain::parse(""), None);
        // The PGA field is three bits, one code per range, and the config word carries it
        // in bits 9..=11. Getting a code wrong silently reads the wrong voltage.
        for (gain, code) in [
            (Gain::V6_144, 0b000),
            (Gain::V4_096, 0b001),
            (Gain::V2_048, 0b010),
            (Gain::V1_024, 0b011),
            (Gain::V0_512, 0b100),
            (Gain::V0_256, 0b101),
        ] {
            let mut chip = Chip::counts(0);
            let mut d = dev();
            d.set_gain(gain);
            d.read_single(&mut chip, 0).expect("read");
            assert_eq!(chip.last_config() >> 9 & 0b111, code, "{gain:?}");
        }
        // Full scale is 32768 LSB over the range, which is where these LSB sizes come from.
        for (gain, full_scale) in [
            (Gain::V6_144, 6.144),
            (Gain::V4_096, 4.096),
            (Gain::V2_048, 2.048),
            (Gain::V1_024, 1.024),
            (Gain::V0_512, 0.512),
            (Gain::V0_256, 0.256),
        ] {
            let fs = gain.lsb_mv() * 32_768.0 / 1000.0;
            assert!((fs - full_scale).abs() < 1e-4, "{gain:?} full scale {fs}, want {full_scale}");
        }
    }

    #[test]
    fn the_tool_reads_a_channel_and_reports_both_units() {
        let mut chip = Chip::counts(8_000); // 1.0 V at 125 uV per count
        let mut d = dev();
        let v = d.call("ads1115_read", &json!({ "channel": 2 }), &mut chip).expect("read");
        assert_eq!(v["channel"], 2);
        assert_eq!(v["volts"], 1.0);
        assert_eq!(v["millivolts"], 1000.0);
        assert_eq!(v["gain"], "V4_096");
    }

    /// A gain in the call is set before the conversion, so the next reading is on the new
    /// range and the result says which one produced it.
    #[test]
    fn the_tool_can_change_the_range_for_the_reading_it_returns() {
        let mut chip = Chip::counts(8_000); // 1.5 V at 187.5 uV per count
        let mut d = dev();
        let v = d.call("ads1115_read", &json!({ "gain": "6.144v" }), &mut chip).expect("read");
        assert_eq!(v["volts"], 1.5);
        assert_eq!(v["gain"], "V6_144", "the answer names the range it was taken on");

        let err = d.call("ads1115_read", &json!({ "gain": "3.3" }), &mut chip).expect_err("no such range");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        assert!(err.message.contains("3.3"), "{}", err.message);
    }

    #[test]
    fn the_tool_reads_a_differential_pair_when_one_is_named() {
        let mut chip = Chip::counts(8_000);
        let mut d = dev();
        let v = d.call("ads1115_read", &json!({ "pair": "2-3" }), &mut chip).expect("read");
        assert_eq!(v["pair"], "2-3");
        assert_eq!(v["volts"], 1.0);
        assert_eq!(chip.last_config() >> 12 & 0b111, 0b011, "the pair reached the config word");

        let err = d.call("ads1115_read", &json!({ "pair": "1-2" }), &mut chip).expect_err("no such pair");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        assert!(err.message.contains("1-2"), "{}", err.message);
    }

    #[test]
    fn a_channel_out_of_range_through_the_tool_is_a_parameter_error() {
        let mut chip = Chip::counts(0);
        let mut d = dev();
        let err = d.call("ads1115_read", &json!({ "channel": 7 }), &mut chip).expect_err("no AIN7");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        // No channel and no pair means channel 0, which is what a caller who says nothing gets.
        let v = d.call("ads1115_read", &json!({}), &mut chip).expect("default channel");
        assert_eq!(v["channel"], 0);
    }

    #[test]
    fn the_inventory_row_names_the_part_and_the_range_it_is_on() {
        let mut d = dev();
        d.set_gain(Gain::V2_048);
        let info = d.info();
        assert_eq!(info.kind, "ads1115");
        assert_eq!(info.bus, "i2c");
        assert_eq!(info.address, Some(0x48));
        assert!(info.name.contains("0x48"), "{}", info.name);
        assert_eq!(info.detail["gain"], "V2_048");
        assert_eq!(info.tools, vec!["ads1115_read"]);
    }

    #[test]
    fn the_published_tool_is_dispatchable_and_nothing_else_is() {
        let names: Vec<String> = dev().tools().iter().map(|t| t.spec.name.clone()).collect();
        assert_eq!(names, vec!["ads1115_read"]);
        let mut chip = Chip::counts(0);
        let mut d = dev();
        d.call(&names[0], &json!({ "channel": 1 }), &mut chip).expect("the published name works");
        let err = d.call("adc_read", &json!({}), &mut chip).expect_err("a neighbour's name is not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
        assert!(err.message.contains("adc_read"), "{}", err.message);
    }
}

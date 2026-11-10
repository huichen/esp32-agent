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

    #[test]
    fn config_word_encodes_channel_and_gain() {
        let channel = 2u16;
        let gain = Gain::V1_024;
        let cfg: u16 = 0x8000 | ((4 + channel) << 12) | (gain.code() << 9) | 0x0100 | (4 << 5) | 0x0003;
        assert_eq!(cfg >> 12 & 0b111, 0b110, "MUX for single-ended AIN2");
        assert_eq!(cfg >> 9 & 0b111, 3, "PGA code for 1.024 V");
        assert_eq!(cfg & 0x0100, 0x0100, "single-shot mode");
    }

    #[test]
    fn lsb_sizes() {
        assert_eq!(Gain::V6_144.lsb_mv(), 0.1875);
        assert_eq!(Gain::V0_256.lsb_mv(), 0.0078125);
    }
}

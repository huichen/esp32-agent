//! SHT30 / SHT31 / SHT35 temperature and humidity (Sensirion SHT3x family).
//!
//! All three answer at 0x44 or 0x45 and share a register map, so the driver is
//! one implementation: single-shot, high repeatability, clock stretching off,
//! CRC-8 per word.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use crate::util::crc8_poly31;
use serde_json::{json, Value};

const CMD_MEASURE: [u8; 2] = [0x2C, 0x06]; // single shot, high repeatability, no clock stretch
const CMD_SOFT_RESET: [u8; 2] = [0x30, 0xA2];
const CMD_HEATER_OFF: [u8; 2] = [0x30, 0x66];

pub struct Sht3x {
    pub bus: u8,
    pub addr: u8,
    id: String,
}

impl Sht3x {
    pub const ADDRESSES: &'static [u8] = &[0x44, 0x45];

    /// The family has no id register; a successful measurement command with a
    /// valid CRC is the strongest probe available.
    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        let mut d =
            Self { bus, addr, id: if id_suffix > 1 { format!("sht3x_{id_suffix}") } else { "sht3x".into() } };
        d.soft_reset(hw)?;
        d.measure(hw).map_err(|_| McpError::not_found(format!("no SHT3x at 0x{addr:02X}")))?;
        Ok(d)
    }

    fn soft_reset(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        hw.i2c_write(self.bus, self.addr, None, &CMD_SOFT_RESET)?;
        hw.delay_us(2_000);
        hw.i2c_write(self.bus, self.addr, None, &CMD_HEATER_OFF)
    }

    /// (temperature °C, relative humidity %)
    pub fn measure(&mut self, hw: &mut dyn Hardware) -> Result<(f32, f32)> {
        hw.i2c_write(self.bus, self.addr, None, &CMD_MEASURE)?;
        hw.delay_us(16_000); // max measurement time for high repeatability
        let mut buf = [0u8; 6];
        hw.i2c_read(self.bus, self.addr, None, &mut buf)?;
        if crc8_poly31(&buf[0..2]) != buf[2] || crc8_poly31(&buf[3..5]) != buf[5] {
            return Err(McpError::hardware("SHT3x CRC mismatch"));
        }
        let raw_t = u16::from_be_bytes([buf[0], buf[1]]) as f32;
        let raw_h = u16::from_be_bytes([buf[3], buf[4]]) as f32;
        let t = -45.0 + 175.0 * raw_t / 65535.0;
        let h = (100.0 * raw_h / 65535.0).clamp(0.0, 100.0);
        Ok((t, h))
    }
}

impl Device for Sht3x {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("sht3x", &format!("SHT3x temp/humidity at 0x{:02X}", self.addr), "i2c");
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["sht3x".into(), "i2c".into(), "temp".into(), "humidity".into()];
        i.detail = json!({ "bus": self.bus });
        i.tools = vec!["sht3x_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("sht3x_read", "Read temperature and relative humidity from the SHT3x sensor")
            .title("SHT3x read")
            .category("sensor")
            .requires(&["sht3x"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "sht3x_read", args, ctx.hw)
            })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "sht3x_read" => {
                let (t, h) = self.measure(hw)?;
                Ok(json!({
                    "temperature_c": (t * 100.0).round() / 100.0,
                    "humidity_pct": (h * 100.0).round() / 100.0,
                    "dew_point_c": (crate::drivers::bme280::dew_point(t, h) * 100.0).round() / 100.0,
                }))
            }
            other => Err(McpError::not_found(format!("sht3x has no tool `{other}`"))),
        }
    }
}

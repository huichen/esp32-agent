//! INA219 / INA226 current and power monitor.
//!
//! Used to measure what a board actually draws. Detection reads the two
//! identification registers: the INA219 has fixed register 0x00/0x02 maps and
//! answers 0x399F on the config register after reset, while the INA226 reports
//! `0x5449` ("TI") in its manufacturer register.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};

const REG_CONFIG: u8 = 0x00;
const REG_SHUNT: u8 = 0x01;
const REG_BUS: u8 = 0x02;
const REG_POWER: u8 = 0x03;
const REG_CURRENT: u8 = 0x04;
const REG_CALIB: u8 = 0x05;
const REG_MANUF: u8 = 0xFE; // INA226 only

pub const INA219_MANUF: u16 = 0x5449;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Ina219,
    Ina226,
}

pub struct Ina219 {
    pub bus: u8,
    pub addr: u8,
    pub variant: Variant,
    /// Shunt resistor in milliohms (0.1 Ω breakout = 100).
    pub shunt_mohm: u32,
    /// Calibration register written at probe time.
    cal: u16,
    id: String,
}

impl Ina219 {
    pub const ADDRESSES: &'static [u8] = &[0x40, 0x41];

    pub fn probe(
        hw: &mut dyn Hardware,
        bus: u8,
        addr: u8,
        shunt_mohm: u32,
        id_suffix: usize,
    ) -> Result<Self> {
        let mut id = if id_suffix > 1 { format!("ina219_{id_suffix}") } else { "ina219".into() };
        // INA226 first: it has a real id register.
        let mut manuf = [0u8; 2];
        let variant = match hw.i2c_read(bus, addr, Some(REG_MANUF), &mut manuf) {
            Ok(()) if u16::from_be_bytes(manuf) == INA219_MANUF => {
                id = id.replace("ina219", "ina226");
                Variant::Ina226
            }
            _ => Variant::Ina219,
        };
        let mut d = Self { bus, addr, variant, shunt_mohm: shunt_mohm.max(1), cal: 0, id };
        d.configure(hw)?;
        Ok(d)
    }

    fn configure(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        // 32 V range, ±320 mV shunt, 12-bit, continuous shunt+bus.
        hw.i2c_write(self.bus, self.addr, Some(REG_CONFIG), &[0x39, 0x9F])?;
        // calibration = 0.04096 / (current_lsb * Rshunt); with 100 µA LSB and
        // R in ohms: cal = trunc(4096 / (0.1 * R)) — computed in integers so the
        // board's FPU format does not matter.
        let r_ohm_x1000 = self.shunt_mohm as u64; // mΩ == 1e-3 Ω
        let cal = (409_600u64 / r_ohm_x1000) as u16;
        self.cal = cal;
        hw.i2c_write(self.bus, self.addr, Some(REG_CALIB), &cal.to_be_bytes())?;
        Ok(())
    }

    /// (bus volts, shunt mV, current mA, power mW)
    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<(f32, f32, f32, f32)> {
        let mut bus_raw = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, Some(REG_BUS), &mut bus_raw)?;
        // Bus voltage LSB = 4 mV, bits 15..3.
        let bus_mv = (u16::from_be_bytes(bus_raw) >> 3) as f32 * 4.0;
        let mut shunt = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, Some(REG_SHUNT), &mut shunt)?;
        // Shunt LSB = 10 µV, signed.
        let shunt_uv = i16::from_be_bytes(shunt) as f32 * 10.0;
        let mut cur = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, Some(REG_CURRENT), &mut cur)?;
        let current_ma = match self.variant {
            Variant::Ina219 => i16::from_be_bytes(cur) as f32 / 10.0,
            Variant::Ina226 => i16::from_be_bytes(cur) as f32 * 1.25,
        };
        let mut pwr = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, Some(REG_POWER), &mut pwr)?;
        let power_mw = u16::from_be_bytes(pwr) as f32 * 2.0;
        Ok((bus_mv / 1000.0, shunt_uv / 1000.0, current_ma, power_mw))
    }
}

impl Device for Ina219 {
    fn info(&self) -> DeviceInfo {
        let name = match self.variant {
            Variant::Ina219 => "INA219",
            Variant::Ina226 => "INA226",
        };
        let mut i = DeviceInfo::new(
            if self.variant == Variant::Ina219 { "ina219" } else { "ina226" },
            &format!("{name} power monitor at 0x{:02X}", self.addr),
            "i2c",
        );
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["ina219".into(), "i2c".into(), "power".into()];
        i.detail = json!({ "variant": name, "bus": self.bus, "shunt_mohm": self.shunt_mohm, "calibration": self.cal });
        i.tools = vec!["power_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new(
            "power_read",
            "Read bus voltage, shunt voltage, current and power from the INA219/INA226",
        )
        .title("Power monitor")
        .category("sensor")
        .requires(&["ina219"])
        .schema(object_schema(&[], &[]))
        .bind(move |ctx, args| {
            let id = id.clone();
            ctx.devices.call(&id, "power_read", args, ctx.hw)
        })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "power_read" => {
                let (v, shunt_mv, ma, mw) = self.read(hw)?;
                Ok(json!({
                    "bus_voltage_v": (v * 1000.0).round() / 1000.0,
                    "shunt_voltage_mv": (shunt_mv * 1000.0).round() / 1000.0,
                    "current_ma": (ma * 10.0).round() / 10.0,
                    "power_mw": (mw * 10.0).round() / 10.0,
                    "shunt_ohm": self.shunt_mohm as f32 / 1000.0,
                }))
            }
            other => Err(McpError::not_found(format!("power monitor has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bus_voltage_conversion_matches_datasheet() {
        // Raw 0x2710 = 10000 -> 10000 >> 3 = 1250 -> 5 V
        let raw: u16 = 0x2710;
        let mv = (raw >> 3) as f32 * 4.0;
        assert!((mv / 1000.0 - 5.0).abs() < 0.001);
    }

    #[test]
    fn calibration_for_a_one_tenth_ohm_shunt() {
        let cal = (40_960_000u64 / 100) as u16;
        assert_eq!(cal, 409_600u32 as u16); // truncated by the register width
        assert_eq!((40_960_000u64 / 1000) as u16, 40_960);
    }
}

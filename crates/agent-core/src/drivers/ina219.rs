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
    use super::*;
    use crate::hardware::BoardInfo;
    use std::collections::HashMap;

    /// A register file with a bus on it: the INA219 is 16-bit big-endian registers and
    /// nothing else, so a map of register to value is a faithful fake. Reads and writes are
    /// logged with their register so a test can check both the value and the address.
    #[derive(Default)]
    struct Chip {
        regs: HashMap<u8, u16>,
        reads: Vec<(u8, u8, Option<u8>)>,
        writes: Vec<(Option<u8>, Vec<u8>)>,
        /// A register that NAKs, to prove a failed read reaches the caller.
        fail_reg: Option<u8>,
    }

    impl Chip {
        fn with(regs: &[(u8, u16)]) -> Self {
            Self { regs: regs.iter().copied().collect(), ..Default::default() }
        }

        fn written(&self, reg: u8) -> Vec<u8> {
            self.writes.iter().find(|(r, _)| *r == Some(reg)).map(|(_, d)| d.clone()).unwrap_or_default()
        }
    }

    impl Hardware for Chip {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn i2c_read(&mut self, bus: u8, addr: u8, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
            if reg.is_some() && reg == self.fail_reg {
                return Err(McpError::hardware("I2C NAK"));
            }
            self.reads.push((bus, addr, reg));
            assert_eq!(out.len(), 2, "every INA register is 16 bits");
            let v = reg.and_then(|r| self.regs.get(&r).copied()).unwrap_or(0);
            out.copy_from_slice(&v.to_be_bytes());
            Ok(())
        }

        fn i2c_write(&mut self, _bus: u8, _addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
            self.writes.push((reg, data.to_vec()));
            Ok(())
        }
    }

    fn close(got: &Value, want: f64) -> bool {
        (got.as_f64().unwrap_or(f64::NAN) - want).abs() < 1e-3
    }

    /// `0x5449` is "TI" in the manufacturer register, and only the INA226 has that register.
    /// Getting this wrong means every current reading is off by a factor of 12.5.
    #[test]
    fn probe_recognises_the_ina226_from_its_manufacturer_register() {
        let mut chip = Chip::with(&[(REG_MANUF, INA219_MANUF)]);
        let d = Ina219::probe(&mut chip, 0, 0x40, 100, 1).expect("probe");
        assert_eq!(d.variant, Variant::Ina226);
        assert_eq!(d.id, "ina226", "the id follows the part, not the configured kind");
        assert_eq!(d.info().kind, "ina226");
        assert_eq!(chip.written(REG_CONFIG), vec![0x39, 0x9F], "32 V, 320 mV, 12-bit, continuous");
    }

    /// An INA219 has nothing at 0xFE, so the probe reads zeros (or NAKs) and the part is
    /// taken as an INA219 by default rather than refused: it is the common part.
    #[test]
    fn probe_falls_back_to_the_ina219_when_there_is_no_id_register() {
        let mut chip = Chip::default();
        let d = Ina219::probe(&mut chip, 0, 0x41, 100, 1).expect("probe");
        assert_eq!(d.variant, Variant::Ina219);
        assert_eq!(d.id, "ina219");

        // A NAK on the manufacturer read is not a probe failure either.
        let mut chip = Chip { fail_reg: Some(REG_MANUF), ..Default::default() };
        let d = Ina219::probe(&mut chip, 0, 0x41, 100, 1).expect("a NAK on 0xFE is still an INA219");
        assert_eq!(d.variant, Variant::Ina219);

        // And a register that answers something else is not "TI".
        let mut chip = Chip::with(&[(REG_MANUF, 0x1234)]);
        assert_eq!(Ina219::probe(&mut chip, 0, 0x40, 100, 1).expect("probe").variant, Variant::Ina219);
    }

    #[test]
    fn a_second_monitor_gets_a_numbered_id() {
        let mut chip = Chip::default();
        let d = Ina219::probe(&mut chip, 0, 0x41, 100, 2).expect("probe");
        assert_eq!(d.id, "ina219_2");
        let mut chip = Chip::with(&[(REG_MANUF, INA219_MANUF)]);
        let d = Ina219::probe(&mut chip, 0, 0x41, 100, 3).expect("probe");
        assert_eq!(d.id, "ina226_3");
    }

    /// Calibration is `0.04096 / (current_lsb * Rshunt)`, and this is the number in the
    /// datasheet's own worked example: 100 uA LSB over 0.1 ohm is 4096.
    #[test]
    fn the_calibration_register_holds_the_datasheet_value_for_a_point_one_ohm_shunt() {
        let mut chip = Chip::default();
        let d = Ina219::probe(&mut chip, 0, 0x40, 100, 1).expect("probe");
        assert_eq!(chip.written(REG_CALIB), vec![0x10, 0x00], "4096 as big-endian bytes");
        assert_eq!(d.cal, 4096);
        assert_eq!(d.info().detail["calibration"], 4096);
        assert_eq!(d.info().detail["shunt_mohm"], 100);
    }

    /// A zero shunt would divide by zero on real hardware and produce a calibration word of
    /// 0, which turns every current reading into 0. It is clamped to 1 mohm instead.
    #[test]
    fn a_zero_shunt_is_clamped_instead_of_dividing_by_zero() {
        let mut chip = Chip::default();
        let d = Ina219::probe(&mut chip, 0, 0x40, 0, 1).expect("probe");
        assert_eq!(d.shunt_mohm, 1);
        assert_eq!(d.cal, (409_600u64) as u16, "the truncation the register forces");
    }

    #[test]
    fn a_write_failure_during_setup_fails_the_probe() {
        struct Naks;
        impl Hardware for Naks {
            fn board_info(&mut self) -> BoardInfo {
                BoardInfo { soc: "fake".into(), ..Default::default() }
            }
            fn i2c_read(&mut self, _b: u8, _a: u8, _r: Option<u8>, out: &mut [u8]) -> Result<()> {
                out.fill(0);
                Ok(())
            }
            fn i2c_write(&mut self, _b: u8, _a: u8, _r: Option<u8>, _d: &[u8]) -> Result<()> {
                Err(McpError::hardware("I2C NAK on write"))
            }
        }
        let err = Ina219::probe(&mut Naks, 0, 0x40, 100, 1)
            .err()
            .expect("a part that cannot be configured is not usable");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    /// The four registers, decoded with the datasheet's LSBs: bus 4 mV/bit with the low
    /// three bits unused, shunt 10 uV/bit signed, current and power in the calibration's
    /// units. A 5 V rail drawing 10 mA is the reference reading.
    #[test]
    fn a_full_reading_decodes_all_four_registers() {
        let mut chip = Chip::with(&[
            (REG_BUS, 0x2710),   // 10000 -> 1250 * 4 mV = 5 V
            (REG_SHUNT, 0xFFF6), // -10 * 10 uV = -0.1 mV, i.e. current flowing the other way
            (REG_CURRENT, 100),  // 100 * 0.1 mA = 10 mA
            (REG_POWER, 25),     // 25 * 2 mW = 50 mW
        ]);
        let mut d = Ina219::probe(&mut chip, 0, 0x40, 100, 1).expect("probe");
        let (v, shunt_mv, ma, mw) = d.read(&mut chip).expect("read");
        assert!((v - 5.0).abs() < 1e-6, "bus voltage {v}");
        assert!((shunt_mv + 0.1).abs() < 1e-6, "shunt voltage {shunt_mv}");
        assert!((ma - 10.0).abs() < 1e-6, "current {ma}");
        assert!((mw - 50.0).abs() < 1e-6, "power {mw}");
    }

    /// The INA226's current register is 1.25 mA per bit, not the INA219's 0.1 mA. This is
    /// the only place the variant changes a number, so it gets its own case.
    #[test]
    fn the_ina226_current_register_is_scaled_by_one_and_a_quarter() {
        let mut chip = Chip::with(&[(REG_MANUF, INA219_MANUF), (REG_CURRENT, 100)]);
        let mut d = Ina219::probe(&mut chip, 0, 0x40, 100, 1).expect("probe");
        let (_, _, ma, _) = d.read(&mut chip).expect("read");
        assert!((ma - 125.0).abs() < 1e-6, "current {ma}");
    }

    #[test]
    fn a_register_that_does_not_answer_fails_the_read() {
        let mut chip = Chip { fail_reg: Some(REG_CURRENT), ..Default::default() };
        let mut d = Ina219::probe(&mut chip, 0, 0x40, 100, 1).expect("probe");
        let err = d.read(&mut chip).expect_err("a NAK must not become a measurement");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    #[test]
    fn the_tool_reports_every_register_with_its_unit() {
        let mut chip =
            Chip::with(&[(REG_BUS, 0x2710), (REG_SHUNT, 0xFFF6), (REG_CURRENT, 100), (REG_POWER, 25)]);
        let mut d = Ina219::probe(&mut chip, 0, 0x40, 100, 1).expect("probe");
        let v = d.call("power_read", &json!({}), &mut chip).expect("read");
        assert!(close(&v["bus_voltage_v"], 5.0), "{v}");
        assert!(close(&v["shunt_voltage_mv"], -0.1), "{v}");
        assert!(close(&v["current_ma"], 10.0), "{v}");
        assert!(close(&v["power_mw"], 50.0), "{v}");
        assert!(close(&v["shunt_ohm"], 0.1), "the shunt it was calibrated with: {v}");
    }

    #[test]
    fn the_inventory_row_names_the_variant_and_its_registers() {
        let mut chip = Chip::default();
        let d = Ina219::probe(&mut chip, 1, 0x41, 100, 1).expect("probe");
        let info = d.info();
        assert_eq!(info.kind, "ina219");
        assert_eq!(info.bus, "i2c");
        assert_eq!(info.address, Some(0x41));
        assert!(info.name.contains("INA219") && info.name.contains("0x41"), "{}", info.name);
        assert!(info.caps.contains(&"power".to_string()));
        assert_eq!(info.detail["variant"], "INA219");
        assert_eq!(info.detail["bus"], 1);
        assert_eq!(info.tools, vec!["power_read"]);
    }

    #[test]
    fn the_published_tool_is_dispatchable_and_nothing_else_is() {
        let mut chip = Chip::default();
        let names: Vec<String> = Ina219::probe(&mut chip, 0, 0x40, 100, 1)
            .expect("probe")
            .tools()
            .iter()
            .map(|t| t.spec.name.clone())
            .collect();
        assert_eq!(names, vec!["power_read"]);
        let mut d = Ina219::probe(&mut chip, 0, 0x40, 100, 1).expect("probe");
        d.call(&names[0], &json!({}), &mut chip).expect("the published name works");
        let err = d.call("power", &json!({}), &mut chip).expect_err("a near miss is not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
        assert!(err.message.contains("power"), "{}", err.message);
    }
}

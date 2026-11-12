//! BH1750 ambient light sensor (I2C 0x23; 0x5C when the ADDR pin is high).
//!
//! There is no identification register. Presence is established by doing what only
//! this part does with one command byte: power up, reset the data register, start a
//! continuous high-resolution measurement, and wait out the conversion time — then
//! two bytes must come back and they must not be the 0xFF 0xFF an addressed-but-idle
//! bus returns.
//!
//! Conversion: in high-resolution mode the datasheet's divider is 1.2, so the 16-bit
//! count is divided by 1.2 to give lux (1 lx per LSB after that division, ~0.83 lx
//! per raw count).

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};

const CMD_POWER_ON: u8 = 0x01;
const CMD_RESET: u8 = 0x07;
/// Continuous, high-resolution (1 lx) mode. 0x11 is its 0.5 lx twin, 0x13 the
/// low-resolution one.
const CMD_CONT_H: u8 = 0x10;

/// The datasheet's conversion time for high-resolution mode is 120 ms; 180 ms
/// covers the slower end of the tolerance.
const CONVERSION_US: u32 = 180_000;

/// Divisor for high-resolution mode (datasheet: "measurement result / 1.2").
const H_RES_DIVISOR: f32 = 1.2;

#[derive(Debug)]
pub struct Bh1750 {
    pub bus: u8,
    pub addr: u8,
    id: String,
    last: Option<(u16, f32)>,
}

impl Bh1750 {
    pub const ADDRESSES: &'static [u8] = &[0x23, 0x5C];

    /// No id register on this part, so there is nothing for an `identify` to read;
    /// [`Bh1750::probe`] is the identification and it needs a whole conversion to
    /// complete. `detect` should therefore only try 0x23/0x5C when the scan saw one
    /// of them.
    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        let mut d = Self {
            bus,
            addr,
            id: if id_suffix > 1 { format!("bh1750_{id_suffix}") } else { "bh1750".into() },
            last: None,
        };
        let not_found =
            |e: McpError| McpError::not_found(format!("no BH1750 at 0x{addr:02X}: {}", e.message));
        d.start(hw).map_err(not_found)?;
        // One measurement, so an address that acknowledges but never converts is not
        // adopted as a light sensor.
        let (raw, _lux) = d.sample(hw).map_err(not_found)?;
        if raw == 0xFFFF {
            return Err(McpError::not_found(format!(
                "no BH1750 at 0x{addr:02X}: the bus returned an all-ones frame"
            )));
        }
        Ok(d)
    }

    /// Power up, clear the data register and start continuous high-resolution mode.
    fn start(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        hw.i2c_write(self.bus, self.addr, None, &[CMD_POWER_ON])?;
        hw.i2c_write(self.bus, self.addr, None, &[CMD_RESET])?;
        hw.i2c_write(self.bus, self.addr, None, &[CMD_CONT_H])?;
        hw.delay_us(CONVERSION_US);
        Ok(())
    }

    fn sample(&mut self, hw: &mut dyn Hardware) -> Result<(u16, f32)> {
        let mut buf = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, None, &mut buf)?;
        let raw = u16::from_be_bytes(buf);
        // Rounded to 0.1 lx: the part's own accuracy is +-20 %, so more digits would
        // be noise dressed as precision.
        let lux = (raw as f32 / H_RES_DIVISOR * 10.0).round() / 10.0;
        self.last = Some((raw, lux));
        Ok((raw, lux))
    }

    /// Illuminance in lux, read from the continuous measurement already running.
    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<f32> {
        Ok(self.sample(hw)?.1)
    }

    /// The last (raw counts, lux) pair, for the device detail.
    pub fn last(&self) -> Option<(u16, f32)> {
        self.last
    }
}

impl Device for Bh1750 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("bh1750", &format!("BH1750 light sensor at 0x{:02X}", self.addr), "i2c");
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["bh1750".into(), "i2c".into(), "light".into()];
        i.detail = json!({ "chip": "BH1750", "bus": self.bus, "mode": "continuous high resolution" });
        i.tools = vec!["bh1750_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("bh1750_read", "Read the illuminance in lux from the BH1750")
            .title("BH1750 read")
            .category("sensor")
            .requires(&["bh1750"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "bh1750_read", args, ctx.hw)
            })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "bh1750_read" => {
                let (raw, lux) = self.sample(hw)?;
                Ok(json!({ "lux": lux, "raw": raw }))
            }
            other => Err(McpError::not_found(format!("bh1750 has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;
    use std::collections::VecDeque;

    /// A bus that answers from a script instead of from silicon: one entry per
    /// `i2c_read`, `Err` for a NAK. Writes are recorded, since the command bytes are
    /// the whole protocol here.
    #[derive(Default)]
    struct Fake {
        script: VecDeque<Result<Vec<u8>>>,
        writes: Vec<Vec<u8>>,
        fail_write: bool,
    }

    impl Fake {
        fn script(entries: Vec<Result<Vec<u8>>>) -> Self {
            Self { script: entries.into(), ..Default::default() }
        }

        fn commands(&self) -> Vec<u8> {
            self.writes.iter().filter_map(|w| w.first().copied()).collect()
        }
    }

    impl Hardware for Fake {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn i2c_read(&mut self, _bus: u8, _addr: u8, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
            let entry = match self.script.pop_front() {
                Some(e) => e,
                None => {
                    return Err(McpError::hardware(format!(
                        "the test script has no answer for a {}-byte read at {reg:?}",
                        out.len()
                    )))
                }
            };
            let bytes = entry?;
            assert_eq!(bytes.len(), out.len(), "scripted read has the wrong length");
            out.copy_from_slice(&bytes);
            Ok(())
        }

        fn i2c_write(&mut self, _bus: u8, _addr: u8, _reg: Option<u8>, data: &[u8]) -> Result<()> {
            if self.fail_write {
                return Err(McpError::hardware("I2C NAK on write"));
            }
            self.writes.push(data.to_vec());
            Ok(())
        }
    }

    fn dev() -> Bh1750 {
        Bh1750 { bus: 0, addr: 0x23, id: "bh1750".into(), last: None }
    }

    #[test]
    fn probe_identifies_the_part_by_a_completed_conversion() {
        // 120 counts: the datasheet's divider of 1.2 makes that exactly 100 lx.
        let mut fake = Fake::script(vec![Ok(vec![0x00, 120])]);
        let d = Bh1750::probe(&mut fake, 0, 0x23, 1).expect("probe succeeds on the script");
        assert_eq!(d.last().map(|(raw, _)| raw), Some(120));
        assert_eq!(fake.commands(), vec![CMD_POWER_ON, CMD_RESET, CMD_CONT_H]);
    }

    #[test]
    fn probe_rejects_an_all_ones_frame_that_is_no_device() {
        let mut fake = Fake::script(vec![Ok(vec![0xFF, 0xFF])]);
        let err = Bh1750::probe(&mut fake, 0, 0x5C, 1).expect_err("0xFFFF is not a measurement");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
    }

    #[test]
    fn probe_reports_an_unanswered_start_as_not_found() {
        let mut fake = Fake::script(vec![Ok(vec![0x00, 0x00])]);
        fake.fail_write = true;
        let err = Bh1750::probe(&mut fake, 0, 0x23, 1).expect_err("a NAK on power-up is no device");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
        assert!(err.message.contains("0x23"));
    }

    #[test]
    fn counts_are_divided_by_the_datasheet_divisor() {
        // BH1750 datasheet, high-resolution mode: the measurement result is divided
        // by 1.2 to give lux, so 120 counts is 100 lx and 12 counts is 10 lx.
        let mut fake = Fake::script(vec![Ok(vec![0x00, 120]), Ok(vec![0x00, 12])]);
        let mut d = dev();
        assert!((d.read(&mut fake).expect("read") - 100.0).abs() < 1e-4);
        assert!((d.read(&mut fake).expect("read") - 10.0).abs() < 1e-4);
        // The 16 bits are big endian.
        let mut fake = Fake::script(vec![Ok(vec![0x01, 0x00])]);
        let lux = d.read(&mut fake).expect("read");
        assert!((lux - 213.3).abs() < 0.1, "256 counts is {lux} lx, want 213.3");
    }

    #[test]
    fn a_normal_read_returns_lux_for_a_bright_room() {
        // 0x0BB8 = 3000 counts, i.e. 2500 lx: bright but well inside the 5461 lx
        // full scale of this mode.
        let mut fake = Fake::script(vec![Ok(vec![0x0B, 0xB8])]);
        let mut d = dev();
        assert!((d.read(&mut fake).expect("read") - 2500.0).abs() < 1e-3);
        assert_eq!(d.last(), Some((3000, 2500.0)));
    }

    #[test]
    fn a_bus_error_during_a_read_is_reported() {
        let mut fake = Fake::script(vec![Err(McpError::hardware("I2C NAK"))]);
        let mut d = dev();
        let err = d.read(&mut fake).expect_err("a NAK must not become a reading");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(d.last().is_none());
    }

    #[test]
    fn full_scale_is_the_top_of_the_range() {
        // 0xFFFF / 1.2 = 54612.5 lux, the mode's documented ceiling.
        let mut fake = Fake::script(vec![Ok(vec![0xFF, 0xFF])]);
        let mut d = dev();
        assert!((d.read(&mut fake).expect("read") - 54612.5).abs() < 0.1);
    }
}

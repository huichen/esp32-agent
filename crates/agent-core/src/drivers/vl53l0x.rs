//! VL53L0X time-of-flight distance sensor (I2C 0x29).
//!
//! Identification is the model id at 0xC0 (`0xEE`), the same byte the part answers
//! with while it is booting, so [`Vl53l0x::probe`] can use it to wait out a soft
//! reset.
//!
//! WHAT IS NOT IMPLEMENTED, and it matters on a bench: ST requires the device to be
//! loaded with its tuning-settings table (roughly eighty register writes), plus a
//! reference-SPAD configuration and the VHV and phase calibrations, before the first
//! usable measurement. That table ships with ST's API (STSW-IMG005) and is not
//! reproduced here — writing eighty register values from memory would be inventing
//! hardware facts, and a wrong one can leave the part unable to range at all.
//! Without it the device still accepts a start command and still fills its result
//! block, but `range_status` will generally not be 11 ("valid") and the millimetre
//! value must not be trusted. The device therefore reports itself as `degraded` and
//! the tool result carries `valid: false` rather than a distance dressed up as a
//! measurement.
//!
//! TODO(verify) on a bench, in order of importance:
//!   * whether a single-shot read ever returns range status 11 without the tuning
//!     settings (if not, this driver needs the table before it is useful);
//!   * the position of the range-status field inside the 0x14 block (assumed bits
//!     6:3, i.e. `(b[0] & 0x78) >> 3`, with 11 meaning valid);
//!   * the sequence-config value written for a single-shot measurement;
//!   * the soft-reset sequence (stop, start, stop) on this hardware revision.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};

const REG_SYSRANGE_START: u8 = 0x00;
const REG_SEQUENCE_CONFIG: u8 = 0x01;
const REG_INTERRUPT_CLEAR: u8 = 0x0B;
const REG_INTERRUPT_STATUS: u8 = 0x13;
/// Result block, 12 bytes; the range in millimetres is the last two (0x1E/0x1F).
const REG_RANGE_STATUS: u8 = 0x14;
const REG_MODEL_ID: u8 = 0xC0;
const REG_REVISION_ID: u8 = 0xC2;

/// What the model-id register must contain.
pub const MODEL_ID: u8 = 0xEE;

const LEN_RESULT: usize = 12;
/// Bits 2:0 of 0x13: any interrupt raised. Only the range step runs here, so any of
/// them means the measurement finished.
const INTERRUPT_MASK: u8 = 0x07;
/// Poll budget: 50 x 5 ms = 250 ms, comfortably more than a single range step.
const POLLS: usize = 50;
/// "Valid" in the range-status field.
const RANGE_VALID: u8 = 11;

#[derive(Debug, Clone, Copy)]
pub struct Measurement {
    pub distance_mm: u16,
    /// Raw range-status field, 0..=15. 11 means valid, 4 a phase failure, 1 a sigma
    /// failure; the rest are the datasheet's remaining categories.
    pub range_status: u8,
    pub valid: bool,
}

impl Measurement {
    pub fn distance_m(&self) -> f32 {
        self.distance_mm as f32 / 1000.0
    }
}

#[derive(Debug)]
pub struct Vl53l0x {
    pub bus: u8,
    pub addr: u8,
    pub revision: u8,
    id: String,
    last: Option<Measurement>,
}

impl Vl53l0x {
    pub const ADDRESSES: &'static [u8] = &[0x29];

    /// Read the model id; `None` when the address holds something else.
    pub fn identify(hw: &mut dyn Hardware, bus: u8, addr: u8) -> Option<u8> {
        let mut buf = [0u8; 1];
        hw.i2c_read(bus, addr, Some(REG_MODEL_ID), &mut buf).ok()?;
        (buf[0] == MODEL_ID).then_some(buf[0])
    }

    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        Self::identify(hw, bus, addr)
            .ok_or_else(|| McpError::not_found(format!("no VL53L0X at 0x{addr:02X}")))?;
        // Soft reset, as ST's API does it: stop, start, stop, then wait for the model
        // id to come back, which is how the part announces that it has booted.
        hw.i2c_write(bus, addr, Some(REG_SYSRANGE_START), &[0x00])?;
        hw.i2c_write(bus, addr, Some(REG_SYSRANGE_START), &[0x01])?;
        hw.delay_us(1_000);
        hw.i2c_write(bus, addr, Some(REG_SYSRANGE_START), &[0x00])?;
        for _ in 0..10 {
            if Self::identify(hw, bus, addr).is_some() {
                let mut rev = [0u8; 1];
                let revision = match hw.i2c_read(bus, addr, Some(REG_REVISION_ID), &mut rev) {
                    Ok(()) => rev[0],
                    Err(_) => 0,
                };
                return Ok(Self {
                    bus,
                    addr,
                    revision,
                    id: if id_suffix > 1 { format!("vl53l0x_{id_suffix}") } else { "vl53l0x".into() },
                    last: None,
                });
            }
            hw.delay_us(1_000);
        }
        Err(McpError::hardware("VL53L0X did not answer after a soft reset"))
    }

    /// One single-shot range measurement.
    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<Measurement> {
        // Single-shot measurement: run only the range step, then start it. Bit 0 of
        // 0x00 is the start trigger.
        hw.i2c_write(self.bus, self.addr, Some(REG_SEQUENCE_CONFIG), &[0x01])?;
        hw.i2c_write(self.bus, self.addr, Some(REG_SYSRANGE_START), &[0x01])?;
        let mut finished = false;
        for _ in 0..POLLS {
            let mut status = [0u8; 1];
            hw.i2c_read(self.bus, self.addr, Some(REG_INTERRUPT_STATUS), &mut status)?;
            if status[0] & INTERRUPT_MASK != 0 {
                finished = true;
                break;
            }
            hw.delay_us(5_000);
        }
        if !finished {
            return Err(McpError::hardware("VL53L0X did not finish a measurement"));
        }
        let mut block = [0u8; LEN_RESULT];
        hw.i2c_read(self.bus, self.addr, Some(REG_RANGE_STATUS), &mut block)?;
        // Clear the interrupt, or the next measurement starts already finished.
        hw.i2c_write(self.bus, self.addr, Some(REG_INTERRUPT_CLEAR), &[0x01])?;
        let range_status = (block[0] & 0x78) >> 3;
        let distance_mm = u16::from_be_bytes([block[10], block[11]]);
        let measurement = Measurement { distance_mm, range_status, valid: range_status == RANGE_VALID };
        self.last = Some(measurement);
        Ok(measurement)
    }

    /// The last measurement, for the device detail.
    pub fn last(&self) -> Option<Measurement> {
        self.last
    }
}

impl Device for Vl53l0x {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "vl53l0x",
            &format!("VL53L0X time-of-flight distance at 0x{:02X}", self.addr),
            "i2c",
        );
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["vl53l0x".into(), "i2c".into(), "distance".into()];
        i.detail = json!({
            "chip": "VL53L0X",
            "bus": self.bus,
            "revision": self.revision,
            "tuning_settings": false,
        });
        // Honest status: the part identifies and answers, but the tuning table ST
        // requires before a usable measurement is not loaded. See the module docs.
        i.status = "degraded".into();
        i.note = "no tuning-settings table loaded: readings may report an invalid range status".into();
        i.tools = vec!["vl53l0x_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new(
            "vl53l0x_read",
            "Measure distance with the VL53L0X in metres (check `valid` before trusting it)",
        )
        .title("VL53L0X distance")
        .category("sensor")
        .requires(&["vl53l0x"])
        .schema(object_schema(&[], &[]))
        .bind(move |ctx, args| {
            let id = id.clone();
            ctx.devices.call(&id, "vl53l0x_read", args, ctx.hw)
        })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "vl53l0x_read" => {
                let m = self.read(hw)?;
                Ok(json!({
                    "distance_m": (m.distance_m() * 1000.0).round() / 1000.0,
                    "distance_mm": m.distance_mm,
                    "range_status": m.range_status,
                    "valid": m.valid,
                }))
            }
            other => Err(McpError::not_found(format!("vl53l0x has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;
    use std::collections::VecDeque;

    /// A bus that answers from a script instead of from silicon: one entry per
    /// `i2c_read`, `Err` for a NAK. Writes are recorded, since the command flow is
    /// what the tests below are really checking.
    #[derive(Default)]
    struct Fake {
        script: VecDeque<Result<Vec<u8>>>,
        writes: Vec<(Option<u8>, Vec<u8>)>,
    }

    impl Fake {
        fn script(entries: Vec<Result<Vec<u8>>>) -> Self {
            Self { script: entries.into(), ..Default::default() }
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

        fn i2c_write(&mut self, _bus: u8, _addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
            self.writes.push((reg, data.to_vec()));
            Ok(())
        }
    }

    fn dev() -> Vl53l0x {
        Vl53l0x { bus: 0, addr: 0x29, revision: 0x10, id: "vl53l0x".into(), last: None }
    }

    /// The 12-byte result block: status byte 0x58 has the range-status field at 11
    /// (valid), and the millimetre value sits in the last two bytes.
    fn block(range_status: u8, mm: u16) -> Vec<u8> {
        let mut b = vec![0u8; LEN_RESULT];
        b[0] = range_status << 3;
        b[10] = (mm >> 8) as u8;
        b[11] = (mm & 0xFF) as u8;
        b
    }

    #[test]
    fn identify_accepts_the_model_id_and_rejects_anything_else() {
        let mut good = Fake::script(vec![Ok(vec![MODEL_ID])]);
        assert_eq!(Vl53l0x::identify(&mut good, 0, 0x29), Some(MODEL_ID));

        // 0xEE's neighbour: a different ST part on the same address family.
        let mut other = Fake::script(vec![Ok(vec![0xEA])]);
        assert_eq!(Vl53l0x::identify(&mut other, 0, 0x29), None);

        let mut silent = Fake::script(vec![Err(McpError::hardware("I2C NAK"))]);
        assert_eq!(Vl53l0x::identify(&mut silent, 0, 0x29), None);
    }

    #[test]
    fn probe_resets_and_waits_for_the_part_to_boot() {
        // identify, then the three reset writes, then identify again, then the
        // revision register.
        let mut fake = Fake::script(vec![Ok(vec![MODEL_ID]), Ok(vec![MODEL_ID]), Ok(vec![0x10])]);
        let d = Vl53l0x::probe(&mut fake, 0, 0x29, 1).expect("probe");
        assert_eq!(d.revision, 0x10);
        assert_eq!(
            fake.writes,
            vec![
                (Some(REG_SYSRANGE_START), vec![0x00]),
                (Some(REG_SYSRANGE_START), vec![0x01]),
                (Some(REG_SYSRANGE_START), vec![0x00]),
            ]
        );
    }

    #[test]
    fn probe_refuses_a_foreign_part() {
        let mut fake = Fake::script(vec![Ok(vec![0x00])]);
        let err = Vl53l0x::probe(&mut fake, 0, 0x29, 1).expect_err("no model id, no device");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
    }

    #[test]
    fn a_normal_read_returns_metres_and_the_status() {
        // 1000 mm is exactly 1.000 m; range status 11 is the datasheet's "valid".
        let mut fake = Fake::script(vec![Ok(vec![INTERRUPT_MASK]), Ok(block(11, 1000))]);
        let mut d = dev();
        let m = d.read(&mut fake).expect("read");
        assert_eq!(m.distance_mm, 1000);
        assert!((m.distance_m() - 1.0).abs() < 1e-6);
        assert_eq!(m.range_status, 11);
        assert!(m.valid);
        // The interrupt is cleared afterwards, or the next read starts finished.
        assert_eq!(fake.writes.last(), Some(&(Some(REG_INTERRUPT_CLEAR), vec![0x01])));
    }

    #[test]
    fn an_invalid_range_status_is_reported_rather_than_hidden() {
        // Status 4 is a phase failure: the number in the block is meaningless.
        let mut fake = Fake::script(vec![Ok(vec![INTERRUPT_MASK]), Ok(block(4, 8190))]);
        let mut d = dev();
        let m = d.read(&mut fake).expect("the transfer succeeded");
        assert!(!m.valid, "a phase failure must not be reported as a measurement");
        assert_eq!(m.range_status, 4);
        assert_eq!(m.distance_mm, 8190, "the raw millimetres still reach the caller");
    }

    #[test]
    fn a_measurement_that_never_finishes_times_out() {
        let mut entries: Vec<Result<Vec<u8>>> = (0..POLLS).map(|_| Ok(vec![0x00])).collect();
        entries.push(Ok(block(11, 500)));
        let mut fake = Fake::script(entries);
        let mut d = dev();
        let err = d.read(&mut fake).expect_err("no interrupt means no measurement");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(d.last().is_none());
    }

    #[test]
    fn a_bus_error_during_a_read_is_reported() {
        let mut fake = Fake::script(vec![Err(McpError::hardware("I2C NAK"))]);
        let mut d = dev();
        let err = d.read(&mut fake).expect_err("a NAK must not become a reading");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    #[test]
    fn the_millimetre_field_is_big_endian_and_saturates_at_the_top() {
        // The block's last two bytes are (0x1E) << 8 | (0x1F), and the part's maximum
        // reported range is 8190 mm, which is what the top of the field is used for.
        let mut fake = Fake::script(vec![Ok(vec![INTERRUPT_MASK]), Ok(block(11, 8190))]);
        let mut d = dev();
        assert_eq!(d.read(&mut fake).expect("read").distance_mm, 8190);
        // 1234 mm = 0x04D2: high byte at index 10, low at 11.
        let b = block(11, 1234);
        assert_eq!((b[10], b[11]), (0x04, 0xD2));
    }
}

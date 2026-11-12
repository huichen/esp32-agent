//! AHT20 / AHT10 temperature and humidity (Aosong, fixed I2C address 0x38).
//!
//! There is no identification register, so presence is established the way the
//! SHT3x driver does it: the part must answer its initialise command with a status
//! byte and then produce a conversion. A wrong chip that happens to live at 0x38
//! would have to accept two three-byte command sequences to be mistaken for one.
//!
//! The frame after a trigger is seven bytes: status, humidity (20 bits),
//! temperature (20 bits), then a CRC-8 over the first six (poly 0x31, init 0xFF —
//! the same polynomial `util::crc8_poly31` already implements for the SHT3x).
//!
//! Status byte: bit 7 is busy. A completed conversion on an AHT20 reads `0x71`
//! (busy clear, mode-copy bit set). The datasheet also calls bit 3 the calibrated
//! flag, but requiring it rejects parts that measure fine, so the driver reports
//! that bit in the device detail and does not gate on it.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use crate::util::crc8_poly31;
use serde_json::{json, Value};

/// Initialise, AHT20 variant (0xBE = initialise/calibrate).
const CMD_INIT_V20: [u8; 3] = [0xBE, 0x08, 0x00];
/// Initialise, AHT10 variant (0xE1), sent when the AHT20 command leaves the part
/// un-calibrated. The two parts answer at the same address and differ only here.
const CMD_INIT_V10: [u8; 3] = [0xE1, 0x08, 0x00];
/// Trigger a measurement: 0xAC, then the two "keep the same mode" bytes.
const CMD_TRIGGER: [u8; 3] = [0xAC, 0x33, 0x00];

const STATUS_BUSY: u8 = 0x80;
const STATUS_CALIBRATED: u8 = 0x08;

/// 7 bytes: status + 5 data + CRC. The 20-bit fields make the payload 5 bytes, not
/// 4, so the task's "6-byte read" is the six bytes the CRC covers.
const FRAME_LEN: usize = 7;

#[derive(Debug)]
pub struct Aht20 {
    pub bus: u8,
    pub addr: u8,
    /// Whether the part reported the datasheet's calibrated bit at probe time.
    pub calibrated: bool,
    /// `true` when the AHT10 initialise command was the one that worked.
    pub aht10: bool,
    id: String,
}

impl Aht20 {
    pub const ADDRESSES: &'static [u8] = &[0x38];

    /// There is no id register on this part, so there is nothing for an `identify` to
    /// read: [`Aht20::probe`] is the identification. It initialises the part and
    /// requires the status byte to come back, which a dead bus or a different chip at
    /// 0x38 cannot do.
    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        let mut d = Self {
            bus,
            addr,
            calibrated: false,
            aht10: false,
            id: if id_suffix > 1 { format!("aht20_{id_suffix}") } else { "aht20".into() },
        };
        d.init(hw)
            .map_err(|e| McpError::not_found(format!("no AHT20/AHT10 at 0x{addr:02X}: {}", e.message)))?;
        Ok(d)
    }

    fn init(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        // Datasheet: 40 ms after power-on before the first command.
        hw.delay_us(40_000);
        hw.i2c_write(self.bus, self.addr, None, &CMD_INIT_V20)?;
        hw.delay_us(10_000);
        let status = self.status(hw)?;
        if status & STATUS_CALIBRATED != 0 {
            self.calibrated = true;
            return Ok(());
        }
        // An AHT10 ignores 0xBE and answers the older initialise command.
        hw.i2c_write(self.bus, self.addr, None, &CMD_INIT_V10)?;
        hw.delay_us(10_000);
        self.aht10 = true;
        self.calibrated = self.status(hw)? & STATUS_CALIBRATED != 0;
        Ok(())
    }

    fn status(&mut self, hw: &mut dyn Hardware) -> Result<u8> {
        let mut s = [0u8; 1];
        hw.i2c_read(self.bus, self.addr, None, &mut s)?;
        Ok(s[0])
    }

    /// (temperature °C, relative humidity %). The datasheet's conversion: both fields
    /// are 20-bit fractions of full scale, humidity over 0..=100 %, temperature over
    /// -50..=150 °C.
    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<(f32, f32)> {
        hw.i2c_write(self.bus, self.addr, None, &CMD_TRIGGER)?;
        // Datasheet: a conversion needs 75 ms; 80 ms is the same number rounded to a
        // delay a scheduler can honour.
        hw.delay_us(80_000);
        let mut buf = [0u8; FRAME_LEN];
        hw.i2c_read(self.bus, self.addr, None, &mut buf)?;
        if buf[0] & STATUS_BUSY != 0 {
            return Err(McpError::hardware("AHT20 is still busy after the conversion time"));
        }
        if crc8_poly31(&buf[0..6]) != buf[6] {
            return Err(McpError::hardware("AHT20 CRC mismatch"));
        }
        Ok(decode(&buf))
    }
}

/// Decode the six bytes a CRC covers: status + the two 20-bit fields.
fn decode(buf: &[u8]) -> (f32, f32) {
    let raw_h = ((buf[1] as u32) << 12) | ((buf[2] as u32) << 4) | ((buf[3] as u32) >> 4);
    let raw_t = (((buf[3] as u32) & 0x0F) << 16) | ((buf[4] as u32) << 8) | (buf[5] as u32);
    let humidity = raw_h as f32 * 100.0 / 1_048_576.0;
    let temperature = raw_t as f32 * 200.0 / 1_048_576.0 - 50.0;
    (temperature, humidity)
}

impl Device for Aht20 {
    fn info(&self) -> DeviceInfo {
        let kind = if self.aht10 { "AHT10" } else { "AHT20" };
        let mut i = DeviceInfo::new("aht20", &format!("{kind} temp/humidity at 0x{:02X}", self.addr), "i2c");
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["aht20".into(), "i2c".into(), "temp".into(), "humidity".into()];
        i.detail = json!({ "chip": kind, "bus": self.bus, "calibrated": self.calibrated });
        i.tools = vec!["aht20_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("aht20_read", "Read temperature and relative humidity from the AHT20/AHT10 sensor")
            .title("AHT20 read")
            .category("sensor")
            .requires(&["aht20"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "aht20_read", args, ctx.hw)
            })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "aht20_read" => {
                let (t, h) = self.read(hw)?;
                Ok(json!({
                    "temperature_c": (t * 100.0).round() / 100.0,
                    "humidity_pct": (h * 100.0).round() / 100.0,
                    "dew_point_c": (crate::drivers::bme280::dew_point(t, h) * 100.0).round()
                        / 100.0,
                }))
            }
            other => Err(McpError::not_found(format!("aht20 has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;
    use std::collections::VecDeque;

    /// A bus that answers from a script instead of from silicon: one entry per
    /// `i2c_read`, `Err` for a NAK. Writes are recorded; nothing in this driver
    /// reads a write back.
    #[derive(Default)]
    struct Fake {
        script: VecDeque<Result<Vec<u8>>>,
        writes: Vec<Vec<u8>>,
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

        fn i2c_write(&mut self, _bus: u8, _addr: u8, _reg: Option<u8>, data: &[u8]) -> Result<()> {
            self.writes.push(data.to_vec());
            Ok(())
        }
    }

    /// A conversion frame with both fields at half scale: raw 0x80000 is 50 %RH and
    /// (0.5 * 200 - 50) = 50 °C. The CRC is the value `crc8_poly31` computes over the
    /// first six bytes; the polynomial itself is pinned against the Sensirion
    /// datasheet example in `util::tests`, so this test is about the frame layout.
    fn frame(status: u8, raw_h: u32, raw_t: u32) -> Vec<u8> {
        let mut f = vec![
            status,
            (raw_h >> 12) as u8,
            ((raw_h >> 4) & 0xFF) as u8,
            (((raw_h & 0x0F) << 4) | (raw_t >> 16)) as u8,
            ((raw_t >> 8) & 0xFF) as u8,
            (raw_t & 0xFF) as u8,
            0,
        ];
        f[6] = crc8_poly31(&f[0..6]);
        f
    }

    /// A device that has already probed, for the tests that script only a read.
    fn dev() -> Aht20 {
        Aht20 { bus: 0, addr: 0x38, calibrated: true, aht10: false, id: "aht20".into() }
    }

    #[test]
    fn probe_accepts_a_part_that_initialises_and_reports_calibrated() {
        let mut fake = Fake::script(vec![Ok(vec![STATUS_CALIBRATED])]);
        let d = Aht20::probe(&mut fake, 0, 0x38, 1).expect("a calibrated part probes");
        assert!(d.calibrated);
        assert!(!d.aht10, "the AHT20 command was the one that worked");
        assert_eq!(fake.writes[0], CMD_INIT_V20.to_vec());
    }

    #[test]
    fn probe_falls_back_to_the_aht10_command() {
        // First status read: calibrated bit clear. Second: the AHT10 answered.
        let mut fake = Fake::script(vec![Ok(vec![0x10]), Ok(vec![STATUS_CALIBRATED])]);
        let d = Aht20::probe(&mut fake, 0, 0x38, 1).expect("the AHT10 path probes too");
        assert!(d.aht10);
        assert!(d.calibrated);
        assert_eq!(fake.writes[1], CMD_INIT_V10.to_vec());
    }

    #[test]
    fn probe_reports_a_silent_bus_as_not_found() {
        let mut fake = Fake::script(vec![Err(McpError::hardware("I2C NAK"))]);
        let err = Aht20::probe(&mut fake, 0, 0x38, 1).expect_err("no answer is not a device");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
        assert!(err.message.contains("0x38"));
    }

    #[test]
    fn a_normal_read_converts_both_fields() {
        let mut fake = Fake::script(vec![Ok(frame(0x71, 0x80000, 0x80000))]);
        let mut d = dev();
        let (t, h) = d.read(&mut fake).expect("read");
        // Half of full scale: 50 %RH, and 0.5 * 200 - 50 = 50 °C, both exact.
        assert!((h - 50.0).abs() < 1e-4, "humidity {h}");
        assert!((t - 50.0).abs() < 1e-4, "temperature {t}");
        assert_eq!(fake.writes.last(), Some(&CMD_TRIGGER.to_vec()));
    }

    #[test]
    fn full_scale_and_zero_are_the_datasheet_ends() {
        // raw 0 is -50 °C and 0 %RH; raw 2^20-1 is just under 150 °C and 100 %RH.
        let (t0, h0) = decode(&[0, 0, 0, 0, 0, 0]);
        assert!((t0 + 50.0).abs() < 1e-6, "temperature {t0}");
        assert!(h0.abs() < 1e-6, "humidity {h0}");
        let (t1, h1) = decode(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!((t1 - 149.999_81).abs() < 0.01, "temperature {t1}");
        assert!((h1 - 99.999_9).abs() < 0.01, "humidity {h1}");
    }

    #[test]
    fn a_bad_crc_is_a_hardware_error_not_a_reading() {
        let mut bad = frame(0x71, 0x80000, 0x80000);
        bad[2] ^= 0x01; // flip a data bit, leave the CRC alone
        let mut fake = Fake::script(vec![Ok(bad)]);
        let mut d = dev();
        let err = d.read(&mut fake).expect_err("a mismatched CRC must not become a reading");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    #[test]
    fn a_busy_status_is_a_hardware_error() {
        let mut fake = Fake::script(vec![Ok(frame(STATUS_BUSY | 0x71, 0x80000, 0x80000))]);
        let mut d = dev();
        let err = d.read(&mut fake).expect_err("a busy part has no measurement yet");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    #[test]
    fn a_bus_error_during_a_read_is_reported() {
        let mut fake = Fake::script(vec![Err(McpError::hardware("I2C NAK"))]);
        let mut d = dev();
        let err = d.read(&mut fake).expect_err("a NAK must not become a reading");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }
}

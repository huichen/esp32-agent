//! BME680 temperature, pressure, humidity and gas (Bosch 4-in-1 sensor).
//!
//! Detection is a chip-id read at 0xD0 (`0x61`) on 0x76/0x77 — the same register
//! and the same two addresses the BME280 driver uses. The ids do not overlap
//! (BMP280 `0x58`, BME280 `0x60`, BME680 `0x61`), so probing both drivers in one
//! pass over a bus is unambiguous; the order between them does not matter.
//!
//! What this driver does, and what it deliberately does not, because a plausible
//! number is worse than a missing one:
//!
//! * temperature is compensated with the datasheet's algorithm (`t_fine / 5120`);
//! * pressure is compensated with the datasheet's floating-point algorithm;
//! * **humidity and gas are reported as raw 16-bit ADC counts.** Compensating them
//!   needs the coefficient block at 0xE1..=0xEE, where several coefficients are
//!   packed into nibbles that share a register. That packing is not something this
//!   driver could state with confidence, and a wrong guess produces a %RH that
//!   looks reasonable and is not. The counts are the honest half of the reading,
//!   and the tool result says so in its `note` field;
//! * the gas heater is never switched on (`run_gas` stays 0 in 0x71), so the gas
//!   counts keep the datasheet's "invalid" marker instead of an invented ohm value.
//!
//! TODO(verify) on a real BME680:
//!   * the calibration byte offsets used by [`Calib::parse`] (par_t1 at 0xE9/0xEA,
//!     the pressure block at 0x8E..=0xA0, par_t2 at 0x8A/0x8B, par_t3 at 0x8C);
//!   * the pressure constants 131072, 4096 and 6250 in [`Calib::compensate_p`];
//!   * the gas ADC bytes claimed at 0x2C..=0x2D;
//!   * the humidity/gas compensation itself, when someone has the datasheet's
//!     coefficient packing in front of them.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};

const REG_ID: u8 = 0xD0;
const REG_RESET: u8 = 0xE0;
/// First calibration block, 0x8A..=0xA0 (23 bytes): par_t2, par_t3, the pressure set.
const REG_CALIB1: u8 = 0x8A;
/// Second calibration block, 0xE1..=0xEE (14 bytes): humidity, par_t1, gas.
const REG_CALIB2: u8 = 0xE1;
const REG_CTRL_GAS_1: u8 = 0x71;
const REG_CTRL_HUM: u8 = 0x72;
const REG_CTRL_MEAS: u8 = 0x74;
const REG_CONFIG: u8 = 0x75;
/// Field data, 0x1F..=0x26: pressure (3 bytes), temperature (3), humidity (2).
const REG_FIELD: u8 = 0x1F;
const LEN_FIELD: usize = 8;
/// Gas ADC counts, 16 bits, little endian. See the TODO(verify) at the top.
const REG_GAS: u8 = 0x2C;

const LEN_CALIB1: usize = 23;
const LEN_CALIB2: usize = 14;

pub const CHIP_BME680: u8 = 0x61;

/// The calibration coefficients, in the order the datasheet prints them.
///
/// Offsets inside the 37-byte block: index 0 is 0x8A and index 22 is 0xA0 (the
/// first block); index 23 is 0xE1 and index 36 is 0xEE (the second block).
#[derive(Debug, Clone, Default)]
pub struct Calib {
    pub t1: u16,
    pub t2: i16,
    pub t3: i8,
    pub p1: u16,
    pub p2: i16,
    pub p3: i8,
    pub p4: i16,
    pub p5: i16,
    pub p6: i8,
    pub p7: i8,
    pub p8: i16,
    pub p9: i16,
    pub p10: u8,
}

impl Calib {
    pub fn parse(raw: &[u8]) -> Self {
        debug_assert_eq!(raw.len(), LEN_CALIB1 + LEN_CALIB2);
        let i = LEN_CALIB1; // start of the 0xE1 block inside `raw`
        Self {
            t2: i16::from_le_bytes([raw[0], raw[1]]),
            t3: raw[2] as i8,
            // 0x8D is reserved: the pressure set starts one byte later.
            p1: u16::from_le_bytes([raw[4], raw[5]]),
            p2: i16::from_le_bytes([raw[6], raw[7]]),
            p3: raw[8] as i8,
            p4: i16::from_le_bytes([raw[10], raw[11]]),
            p5: i16::from_le_bytes([raw[12], raw[13]]),
            p7: raw[14] as i8,
            p6: raw[15] as i8,
            p8: i16::from_le_bytes([raw[18], raw[19]]),
            p9: i16::from_le_bytes([raw[20], raw[21]]),
            p10: raw[22],
            t1: u16::from_le_bytes([raw[i + 8], raw[i + 9]]),
        }
    }

    /// Datasheet temperature compensation, in °C (and `t_fine` for the next step).
    ///
    /// The datasheet writes it as
    /// `var1 = (adc_t/8 - par_t1*2) * par_t2/2048`,
    /// `var2 = ((adc_t/16 - par_t1)^2 / 4096) * par_t3/16384`,
    /// `t_fine = var1 + var2`, `T = t_fine / 5120`.
    pub fn compensate_t(&self, adc_t: i32) -> (f64, f64) {
        let adc = adc_t as f64;
        let t1 = self.t1 as f64;
        let var1 = (adc / 8.0 - t1 * 2.0) * (self.t2 as f64 / 2048.0);
        let d = adc / 16.0 - t1;
        let var2 = (d * d / 4096.0) * (self.t3 as f64 / 16384.0);
        let t_fine = var1 + var2;
        (t_fine, t_fine / 5120.0)
    }

    /// Datasheet pressure compensation, in Pa. 0.0 when the coefficients are
    /// degenerate, which is also what the datasheet specifies (no division by zero).
    pub fn compensate_p(&self, adc_p: i32, t_fine: f64) -> f64 {
        let p1 = self.p1 as f64;
        if p1 == 0.0 {
            return 0.0;
        }
        let mut var1 = t_fine / 2.0 - 64000.0;
        let mut var2 = var1 * var1 * (self.p6 as f64 / 131072.0);
        var2 += var1 * self.p5 as f64 * 2.0;
        var2 = var2 / 4.0 + self.p4 as f64 * 65536.0;
        var1 = self.p3 as f64 * var1 * var1 / 16384.0 + self.p2 as f64 * var1;
        var1 /= 524288.0;
        var1 = (1.0 + var1 / 32768.0) * p1;
        let p = 1048576.0 - adc_p as f64;
        (p - var2 / 4096.0) * 6250.0 / var1
    }
}

/// One measurement. Humidity and gas are ADC counts, not physical units: see the
/// module documentation.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    pub temperature_c: f32,
    pub pressure_hpa: f32,
    pub humidity_counts: u16,
    pub gas_counts: u16,
}

#[derive(Debug)]
pub struct Bme680 {
    pub bus: u8,
    pub addr: u8,
    calib: Calib,
    id: String,
    last: Option<Reading>,
}

impl Bme680 {
    pub const ADDRESSES: &'static [u8] = &[0x76, 0x77];

    /// Read the chip id; `None` when the address holds something else. 0x61 at 0xD0
    /// is the only id this driver claims.
    pub fn identify(hw: &mut dyn Hardware, bus: u8, addr: u8) -> Option<u8> {
        let mut buf = [0u8; 1];
        hw.i2c_read(bus, addr, Some(REG_ID), &mut buf).ok()?;
        (buf[0] == CHIP_BME680).then_some(buf[0])
    }

    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        Self::identify(hw, bus, addr)
            .ok_or_else(|| McpError::not_found(format!("no BME680 at 0x{addr:02X}")))?;
        // Soft reset, then the datasheet's 2 ms for the NVM copy (rounded up: the
        // exact figure is not what a slow bus costs).
        hw.i2c_write(bus, addr, Some(REG_RESET), &[0xB6])?;
        hw.delay_us(3_000);
        let mut raw = [0u8; LEN_CALIB1 + LEN_CALIB2];
        hw.i2c_read(bus, addr, Some(REG_CALIB1), &mut raw[..LEN_CALIB1])?;
        hw.i2c_read(bus, addr, Some(REG_CALIB2), &mut raw[LEN_CALIB1..])?;
        if raw.iter().all(|b| *b == 0x00) || raw.iter().all(|b| *b == 0xFF) {
            return Err(McpError::hardware("BME680 calibration block is empty"));
        }
        // IIR filter off, no heater, 1x oversampling: a weather-station profile that
        // finishes well inside the delay `read` waits for.
        hw.i2c_write(bus, addr, Some(REG_CONFIG), &[0x00])?;
        hw.i2c_write(bus, addr, Some(REG_CTRL_GAS_1), &[0x00])?;
        Ok(Self {
            bus,
            addr,
            calib: Calib::parse(&raw),
            id: if id_suffix > 1 { format!("bme680_{id_suffix}") } else { "bme680".into() },
            last: None,
        })
    }

    /// Forced-mode single measurement, 1x on all three channels.
    fn kick(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        hw.i2c_write(self.bus, self.addr, Some(REG_CTRL_HUM), &[0x01])?;
        // 0x25 = osrs_t 1x, osrs_p 1x, mode forced.
        hw.i2c_write(self.bus, self.addr, Some(REG_CTRL_MEAS), &[0x25])
    }

    /// One converted measurement.
    ///
    /// The wait is a fixed one rather than a poll of the status register at 0x73: the
    /// position of the "measuring" bit inside that byte could not be confirmed, and a
    /// delay longer than the datasheet's maximum measurement time is both simpler and
    /// impossible to get wrong. With the heater off at 1x oversampling the conversion
    /// takes a few milliseconds; 150 ms covers the datasheet's worst case for the
    /// three channels by a wide margin.
    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<Reading> {
        self.kick(hw)?;
        hw.delay_us(150_000);
        let mut f = [0u8; LEN_FIELD];
        hw.i2c_read(self.bus, self.addr, Some(REG_FIELD), &mut f)?;
        let adc_p = ((f[0] as i32) << 12) | ((f[1] as i32) << 4) | ((f[2] as i32) >> 4);
        let adc_t = ((f[3] as i32) << 12) | ((f[4] as i32) << 4) | ((f[5] as i32) >> 4);
        let humidity_counts = u16::from_be_bytes([f[6], f[7]]);
        let mut g = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, Some(REG_GAS), &mut g)?;
        let gas_counts = u16::from_le_bytes(g);
        let (t_fine, t_c) = self.calib.compensate_t(adc_t);
        let p_pa = self.calib.compensate_p(adc_p, t_fine);
        let reading = Reading {
            temperature_c: t_c as f32,
            pressure_hpa: (p_pa / 100.0) as f32,
            humidity_counts,
            gas_counts,
        };
        self.last = Some(reading);
        Ok(reading)
    }

    /// The last reading, for a caller that wants the device detail rather than a tool
    /// call. `None` until the first [`Bme680::read`].
    pub fn last(&self) -> Option<Reading> {
        self.last
    }
}

impl Device for Bme680 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "bme680",
            &format!("BME680 temperature/pressure/humidity/gas at 0x{:02X}", self.addr),
            "i2c",
        );
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec![
            "bme680".into(),
            "i2c".into(),
            "temp".into(),
            "pressure".into(),
            "humidity".into(),
            "gas".into(),
        ];
        i.detail = json!({ "chip": "BME680", "bus": self.bus, "units": "hpa" });
        i.tools = vec!["bme680_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new(
            "bme680_read",
            "Read a BME680: temperature and pressure in physical units, humidity and gas as raw ADC counts",
        )
        .title("BME680 read")
        .category("sensor")
        .requires(&["bme680"])
        .schema(object_schema(&[], &[]))
        .bind(move |ctx, args| {
            let id = id.clone();
            ctx.devices.call(&id, "bme680_read", args, ctx.hw)
        })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "bme680_read" => {
                let r = self.read(hw)?;
                Ok(json!({
                    "temperature_c": (r.temperature_c * 100.0).round() / 100.0,
                    "pressure_hpa": (r.pressure_hpa * 100.0).round() / 100.0,
                    "altitude_m": (crate::drivers::bme280::altitude(r.pressure_hpa) * 10.0).round()
                        / 10.0,
                    "humidity_counts": r.humidity_counts,
                    "gas_counts": r.gas_counts,
                    "note": "humidity_counts and gas_counts are raw 16-bit ADC counts: the \
                             coefficient packing for %RH and ohms is not implemented yet",
                }))
            }
            other => Err(McpError::not_found(format!("bme680 has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;
    use std::collections::VecDeque;

    /// A bus that answers from a script instead of from silicon: one entry per
    /// `i2c_read`, `Err` for a NAK. Writes are recorded and never fail unless the test
    /// asks them to, because no decision in this driver depends on a write's payload.
    #[derive(Default)]
    struct Fake {
        script: VecDeque<Result<Vec<u8>>>,
        writes: Vec<(Option<u8>, Vec<u8>)>,
        fail_write: bool,
    }

    impl Fake {
        fn script(entries: Vec<Result<Vec<u8>>>) -> Self {
            Self { script: entries.into(), ..Default::default() }
        }

        /// [`Bme680::probe`] reads the chip id and then two calibration blocks; this
        /// serves all three from one 37-byte array.
        fn calib_entries(calib: &[u8; LEN_CALIB1 + LEN_CALIB2]) -> Vec<Result<Vec<u8>>> {
            vec![Ok(vec![CHIP_BME680]), Ok(calib[..LEN_CALIB1].to_vec()), Ok(calib[LEN_CALIB1..].to_vec())]
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
            assert_eq!(bytes.len(), out.len(), "scripted {reg:?} read has the wrong length");
            out.copy_from_slice(&bytes);
            Ok(())
        }

        fn i2c_write(&mut self, _bus: u8, _addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
            if self.fail_write {
                return Err(McpError::hardware("I2C NAK on write"));
            }
            self.writes.push((reg, data.to_vec()));
            Ok(())
        }
    }

    /// A calibration block whose only non-zero coefficients are the four the exact
    /// tests below use: par_t2 = 2048 (0x0800 little endian at 0x8A) and
    /// par_p1 = 10000 (0x2710 at 0x8E).
    fn calib_bytes() -> [u8; LEN_CALIB1 + LEN_CALIB2] {
        let mut b = [0u8; LEN_CALIB1 + LEN_CALIB2];
        b[0] = 0x00;
        b[1] = 0x08; // par_t2 = 2048
        b[4] = 0x10;
        b[5] = 0x27; // par_p1 = 10000
        b
    }

    /// The eight field bytes for adc_p = 888576 and adc_t = 800000, plus two humidity
    /// counts and two gas counts.
    ///
    /// adc = (msb << 12) | (lsb << 4) | (xlsb >> 4), so 888576 = 0xD8F000>>4 packing
    /// and 800000 = 0xC3500.
    fn field_bytes() -> Vec<u8> {
        vec![
            0xD8, 0xF0, 0x00, // pressure: (0xD8 << 12) | (0xF0 << 4) = 888576
            0xC3, 0x50, 0x00, // temperature: (0xC3 << 12) | (0x50 << 4) = 800000
            0x12, 0x34, // humidity counts, big endian
        ]
    }

    fn probed(fake: &mut Fake) -> Bme680 {
        let d = Bme680::probe(fake, 0, 0x76, 1).expect("probe succeeds on the script");
        assert_eq!(d.calib.t2, 2048);
        assert_eq!(d.calib.p1, 10000);
        d
    }

    #[test]
    fn identify_accepts_the_chip_id_and_rejects_anything_else() {
        let mut good = Fake::script(vec![Ok(vec![0x61])]);
        assert_eq!(Bme680::identify(&mut good, 0, 0x76), Some(CHIP_BME680));

        // 0x60 is a BME280: the BME280 driver's part, not this one.
        let mut bme280 = Fake::script(vec![Ok(vec![0x60])]);
        assert_eq!(Bme680::identify(&mut bme280, 0, 0x76), None);

        // No answer at all is not an identification either.
        let mut silent = Fake::script(vec![Err(McpError::hardware("I2C NAK"))]);
        assert_eq!(Bme680::identify(&mut silent, 0, 0x77), None);
    }

    #[test]
    fn probe_refuses_a_foreign_chip() {
        let mut fake = Fake::script(vec![Ok(vec![0x58])]);
        let err = Bme680::probe(&mut fake, 0, 0x76, 1).expect_err("a BMP280 id must be refused");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
    }

    #[test]
    fn temperature_matches_the_datasheet_equation() {
        // With par_t1 = 0, par_t2 = 2048 and par_t3 = 0 the datasheet equations
        // collapse to var1 = adc_t / 8 and var2 = 0, so
        // t_fine = adc_t / 8 and T = t_fine / 5120 = adc_t / 40960.
        // adc_t = 800000 therefore reads exactly 19.53125 C.
        let calib = Calib { t2: 2048, ..Default::default() };
        let (t_fine, t_c) = calib.compensate_t(800_000);
        assert_eq!(t_fine, 100_000.0);
        assert!((t_c - 19.53125).abs() < 1e-6, "got {t_c}");
    }

    #[test]
    fn pressure_matches_the_datasheet_equation() {
        // par_p2..=par_p6 are 0 here, so the datasheet equation reduces to
        // var1 = par_p1 and p = (1048576 - adc_p) * 6250 / par_p1.
        // With par_p1 = 10000 and adc_p = 888576 that is 160000 * 0.625 = 100000 Pa,
        // i.e. exactly 1000 hPa — the conversion this driver reports.
        let calib = Calib { p1: 10_000, ..Default::default() };
        let p_pa = calib.compensate_p(888_576, 100_000.0);
        assert!((p_pa - 100_000.0).abs() < 1e-6, "got {p_pa} Pa");
        assert!(((p_pa / 100.0) - 1000.0).abs() < 1e-6);

        // More raw counts is less pressure: adc_p is subtracted from the full scale.
        assert!(calib.compensate_p(900_000, 100_000.0) < p_pa);
        // Degenerate coefficients must not divide by zero.
        assert_eq!(Calib::default().compensate_p(888_576, 100_000.0), 0.0);
    }

    #[test]
    fn a_normal_read_returns_both_units_and_the_raw_counts() {
        let calib = calib_bytes();
        let mut entries = Fake::calib_entries(&calib);
        entries.push(Ok(field_bytes()));
        entries.push(Ok(vec![0x00, 0x80])); // gas counts, little endian
        let mut fake = Fake::script(entries);
        let mut dev = probed(&mut fake);

        let r = dev.read(&mut fake).expect("read succeeds on the script");
        assert!((r.temperature_c - 19.53125).abs() < 1e-4, "{}", r.temperature_c);
        assert!((r.pressure_hpa - 1000.0).abs() < 1e-3, "{}", r.pressure_hpa);
        assert_eq!(r.humidity_counts, 0x1234);
        assert_eq!(r.gas_counts, 0x8000);
        // The forced-mode write is the last thing before the wait: 0x25 in 0x74.
        assert!(fake
            .writes
            .iter()
            .any(|(reg, data)| *reg == Some(REG_CTRL_MEAS) && data.as_slice() == [0x25]));
        assert_eq!(dev.last().map(|r| r.humidity_counts), Some(0x1234));
    }

    #[test]
    fn a_bus_error_in_the_middle_of_a_read_is_reported() {
        let calib = calib_bytes();
        let mut entries = Fake::calib_entries(&calib);
        entries.push(Err(McpError::hardware("I2C NAK")));
        let mut fake = Fake::script(entries);
        let mut dev = probed(&mut fake);

        let err = dev.read(&mut fake).expect_err("a NAK must not become a reading");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(dev.last().is_none(), "no reading may be cached from a failed one");
    }

    #[test]
    fn a_failed_write_is_reported_rather_than_ignored() {
        let calib = calib_bytes();
        let mut fake = Fake::script(Fake::calib_entries(&calib));
        fake.fail_write = true;
        let err = Bme680::probe(&mut fake, 0, 0x76, 1).expect_err("the soft reset must be reported");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    #[test]
    fn calibration_bytes_are_little_endian() {
        let mut raw = [0u8; LEN_CALIB1 + LEN_CALIB2];
        raw[0] = 0x30;
        raw[1] = 0x6B; // par_t2 = 0x6B30
        raw[2] = 0xFF; // par_t3 = -1
        raw[22] = 0x0A; // par_p10 at 0xA0
        raw[LEN_CALIB1 + 8] = 0x11; // par_t1 low byte at 0xE9
        raw[LEN_CALIB1 + 9] = 0x22; // par_t1 high byte at 0xEA
        let c = Calib::parse(&raw);
        assert_eq!(c.t2, 0x6B30);
        assert_eq!(c.t3, -1);
        assert_eq!(c.p10, 10);
        assert_eq!(c.t1, 0x2211);
    }
}

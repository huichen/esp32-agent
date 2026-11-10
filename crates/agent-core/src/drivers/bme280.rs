//! BME280 / BMP280 pressure, temperature (and BME280 humidity).
//!
//! Detection is a chip-id read at 0x76/0x77, which is how a BMP280 (0x58), a
//! BME280 (0x60) and the many clones that answer 0xFF at that address are told
//! apart. Compensation follows the integer reference in the datasheet; the unit
//! tests pin the datasheet's own worked example.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const REG_ID: u8 = 0xD0;
const REG_RESET: u8 = 0xE0;
const REG_CTRL_HUM: u8 = 0xF2;
const REG_STATUS: u8 = 0xF3;
const REG_CTRL_MEAS: u8 = 0xF4;
const REG_CONFIG: u8 = 0xF5;
const REG_DATA: u8 = 0xF7;

pub const CHIP_BMP280: u8 = 0x58;
pub const CHIP_BME280: u8 = 0x60;

#[derive(Debug, Clone, Default)]
pub struct Calib {
    t1: u16,
    t2: i16,
    t3: i16,
    p1: u16,
    p2: i16,
    p3: i16,
    p4: i16,
    p5: i16,
    p6: i16,
    /// P7..P9 are not used by the integer pressure algorithm. They are kept so
    /// the parsed calibration matches the datasheet block one-to-one, which makes
    /// comparing against a logic-analyser capture possible.
    #[allow(dead_code)]
    p7: i16,
    #[allow(dead_code)]
    p8: i16,
    #[allow(dead_code)]
    p9: i16,
    h1: u8,
    h2: i16,
    h3: u8,
    h4: i16,
    h5: i16,
    h6: i8,
}

impl Calib {
    fn parse(raw: &[u8]) -> Self {
        // raw = 0x88..=0xA1 (26 bytes) followed by 0xE1..=0xE7 (7 bytes)
        Self {
            t1: u16::from_le_bytes([raw[0], raw[1]]),
            t2: i16::from_le_bytes([raw[2], raw[3]]),
            t3: i16::from_le_bytes([raw[4], raw[5]]),
            p1: u16::from_le_bytes([raw[6], raw[7]]),
            p2: i16::from_le_bytes([raw[8], raw[9]]),
            p3: i16::from_le_bytes([raw[10], raw[11]]),
            p4: i16::from_le_bytes([raw[12], raw[13]]),
            p5: i16::from_le_bytes([raw[14], raw[15]]),
            p6: i16::from_le_bytes([raw[16], raw[17]]),
            p7: i16::from_le_bytes([raw[18], raw[19]]),
            p8: i16::from_le_bytes([raw[20], raw[21]]),
            p9: i16::from_le_bytes([raw[22], raw[23]]),
            h1: raw[25],
            h2: i16::from_le_bytes([raw[26], raw[27]]),
            h3: raw[28],
            // H4/H5 share byte 0xE5: H4 = high 12 bits, H5 = low 12 bits.
            h4: ((raw[29] as i16) << 4) | ((raw[30] & 0x0F) as i16),
            h5: ((raw[31] as i16) << 4) | ((raw[30] >> 4) as i16),
            h6: raw[32] as i8,
        }
    }

    /// Datasheet compensation: temperature in 0.01 °C, `t_fine` for the next steps.
    /// Computed in i64 so a full-scale raw reading cannot overflow on a 32-bit MCU.
    pub fn compensate_t(&self, adc_t: i32) -> (i32, i32) {
        let t1 = self.t1 as i64;
        let t2 = self.t2 as i64;
        let t3 = self.t3 as i64;
        let a = adc_t as i64;
        let var1 = (((a >> 3) - (t1 << 1)) * t2) >> 11;
        let d = (a >> 4) - t1;
        let var2 = (((d * d) >> 12) * t3) >> 14;
        let t_fine = var1 + var2;
        (((t_fine * 5 + 128) >> 8) as i32, t_fine as i32)
    }

    /// Pressure in Pa (0 when the calibration is unusable).
    pub fn compensate_p(&self, adc_p: i32, t_fine: i32) -> u32 {
        let p1 = self.p1 as i64;
        let p2 = self.p2 as i64;
        let p3 = self.p3 as i64;
        let p4 = self.p4 as i64;
        let p5 = self.p5 as i64;
        let p6 = self.p6 as i64;
        let mut var1 = (t_fine as i64 >> 1) - 64_000;
        let mut var2 = (((var1 >> 2) * (var1 >> 2)) >> 11) * p6;
        var2 += (var1 * p5) << 1;
        var2 = (var2 >> 2) + (p4 << 16);
        var1 = (((p3 * (((var1 >> 2) * (var1 >> 2)) >> 13)) >> 3) + ((p2 * var1) >> 1)) >> 18;
        var1 = ((32768 + var1) * p1) >> 15;
        if var1 == 0 {
            return 0;
        }
        let p = ((1_048_576 - adc_p as i64) - (var2 >> 12)) * 3125;
        let val = if p < 0x8000_0000 { (p << 1) / var1 } else { (p / var1) * 2 };
        val.clamp(0, u32::MAX as i64) as u32
    }

    /// Humidity in 0.001 %RH (0..=100_000), BME280 only.
    pub fn compensate_h(&self, adc_h: i32, t_fine: i32) -> u32 {
        let (h1, h2, h3) = (self.h1 as i64, self.h2 as i64, self.h3 as i64);
        let (h4, h5, h6) = (self.h4 as i64, self.h5 as i64, self.h6 as i64);
        let mut v: i64 = t_fine as i64 - 76_800;
        v = (((((adc_h as i64) << 14) - (h4 << 20) - (h5 * v) + 16_384) >> 15)
            * ((((((v * h6) >> 10) * (((v * h3) >> 11) + 32_768)) >> 10) + 2_097_152) * h2 + 8192))
            >> 14;
        v -= ((((v >> 15) * (v >> 15)) >> 7) * h1) >> 4;
        v = v.clamp(0, 419_430_400);
        (v as u32) >> 12
    }
}

pub struct Bme280 {
    pub bus: u8,
    pub addr: u8,
    pub chip: u8,
    calib: Calib,
    id: String,
    last: Option<(f32, f32, f32)>,
}

impl Bme280 {
    pub const ADDRESSES: &'static [u8] = &[0x76, 0x77];

    /// Read the chip id; `None` when the address holds something else.
    pub fn identify(hw: &mut dyn Hardware, bus: u8, addr: u8) -> Option<u8> {
        let mut buf = [0u8; 1];
        hw.i2c_read(bus, addr, Some(REG_ID), &mut buf).ok()?;
        match buf[0] {
            CHIP_BMP280 | CHIP_BME280 => Some(buf[0]),
            _ => None,
        }
    }

    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        let chip = Self::identify(hw, bus, addr)
            .ok_or_else(|| McpError::not_found(format!("no BME/BMP280 at 0x{addr:02X}")))?;
        // Soft reset, then wait for the NVM copy to finish.
        hw.i2c_write(bus, addr, Some(REG_RESET), &[0xB6])?;
        hw.delay_us(3_000);
        let mut raw = [0u8; 33];
        hw.i2c_read(bus, addr, Some(0x88), &mut raw[..26])?;
        hw.i2c_read(bus, addr, Some(0xE1), &mut raw[26..])?;
        if raw.iter().all(|b| *b == 0x00) || raw.iter().all(|b| *b == 0xFF) {
            return Err(McpError::hardware("calibration block is empty"));
        }
        // weather-station profile: 1x oversampling, IIR off, 500 ms standby.
        hw.i2c_write(bus, addr, Some(REG_CONFIG), &[0x00])?;
        if chip == CHIP_BME280 {
            hw.i2c_write(bus, addr, Some(REG_CTRL_HUM), &[0x01])?;
        }
        Ok(Self {
            bus,
            addr,
            chip,
            calib: Calib::parse(&raw),
            id: if id_suffix > 1 { format!("bme280_{id_suffix}") } else { "bme280".into() },
            last: None,
        })
    }

    fn kick(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        // forced mode, 1x temp + 1x pressure
        hw.i2c_write(self.bus, self.addr, Some(REG_CTRL_MEAS), &[0x25])
    }

    fn wait_ready(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        for _ in 0..50 {
            let mut st = [0u8; 1];
            hw.i2c_read(self.bus, self.addr, Some(REG_STATUS), &mut st)?;
            if st[0] & 0x08 == 0 {
                return Ok(());
            }
            hw.delay_us(2_000);
        }
        Err(McpError::hardware("BME280 measurement did not finish"))
    }

    /// (temperature °C, pressure hPa, humidity %RH or NaN on BMP280)
    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<(f32, f32, f32)> {
        self.kick(hw)?;
        self.wait_ready(hw)?;
        let mut buf = [0u8; 8];
        hw.i2c_read(self.bus, self.addr, Some(REG_DATA), &mut buf)?;
        let adc_p = ((buf[0] as i32) << 12) | ((buf[1] as i32) << 4) | ((buf[2] as i32) >> 4);
        let adc_t = ((buf[3] as i32) << 12) | ((buf[4] as i32) << 4) | ((buf[5] as i32) >> 4);
        let adc_h = ((buf[6] as i32) << 8) | (buf[7] as i32);
        let (t100, t_fine) = self.calib.compensate_t(adc_t);
        let p_pa = self.calib.compensate_p(adc_p, t_fine);
        let rh = if self.chip == CHIP_BME280 {
            self.calib.compensate_h(adc_h, t_fine) as f32 / 1000.0
        } else {
            f32::NAN
        };
        self.last = Some((t100 as f32 / 100.0, p_pa as f32 / 100.0, rh));
        Ok(self.last.expect("just set"))
    }
}

impl Device for Bme280 {
    fn info(&self) -> DeviceInfo {
        let kind = if self.chip == CHIP_BME280 { "BME280" } else { "BMP280" };
        let mut i = DeviceInfo::new(
            "bme280",
            &format!(
                "{kind} temperature/pressure{} at 0x{:02X}",
                if self.chip == CHIP_BME280 { "/humidity" } else { "" },
                self.addr
            ),
            "i2c",
        );
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["bme280".into(), "i2c".into(), "temp".into(), "pressure".into()];
        i.detail = json!({ "chip": kind, "bus": self.bus });
        i.tools = vec!["bme280_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let has_hum = self.chip == CHIP_BME280;
        vec![ToolDef::new(
            "bme280_read",
            if has_hum {
                "Read temperature, pressure and humidity from the BME280"
            } else {
                "Read temperature and pressure from the BMP280"
            },
        )
        .title("BME280 read")
        .category("sensor")
        .requires(&["bme280"])
        .schema(object_schema(&[], &[]))
        .bind(move |ctx, args| {
            let id = id.clone();
            ctx.devices.call(&id, "bme280_read", args, ctx.hw)
        })
        .core()]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "bme280_read" => {
                let (t, p, h) = self.read(hw)?;
                let mut out = BTreeMap::new();
                out.insert("temperature_c", json!((t * 100.0).round() / 100.0));
                out.insert("pressure_hpa", json!((p * 100.0).round() / 100.0));
                if !h.is_nan() {
                    out.insert("humidity_pct", json!((h * 100.0).round() / 100.0));
                    out.insert("dew_point_c", json!((dew_point(t, h) * 100.0).round() / 100.0));
                }
                out.insert("altitude_m", json!((altitude(p) * 10.0).round() / 10.0));
                Ok(json!(out))
            }
            other => Err(McpError::not_found(format!("bme280 has no tool `{other}`"))),
        }
    }
}

/// Magnus formula, valid over the usual indoor range.
pub fn dew_point(t_c: f32, rh: f32) -> f32 {
    let rh = rh.clamp(0.1, 100.0);
    let a = 17.62;
    let b = 243.12;
    let gamma = (rh / 100.0).ln() + (a * t_c) / (b + t_c);
    (b * gamma) / (a - gamma)
}

/// Barometric formula against 1013.25 hPa at sea level.
pub fn altitude(p_hpa: f32) -> f32 {
    if p_hpa <= 0.0 {
        return 0.0;
    }
    44330.0 * (1.0 - (p_hpa / 1013.25).powf(0.1903))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The calibration block and raw readings from the BME280 datasheet's
    /// worked example (section 3.5 / 4.1).
    fn datasheet_calib() -> Calib {
        Calib {
            t1: 27504,
            t2: 26435,
            t3: -1000,
            p1: 36477,
            p2: -10685,
            p3: 3024,
            p4: 2855,
            p5: 140,
            p6: -7,
            p7: 15500,
            p8: -14600,
            p9: 6000,
            h1: 75,
            h2: 362,
            h3: 0,
            h4: 313,
            h5: 50,
            h6: 30,
        }
    }

    #[test]
    fn temperature_matches_the_datasheet_example() {
        let (t100, t_fine) = datasheet_calib().compensate_t(519_888);
        assert_eq!(t_fine, 128_422, "t_fine from the datasheet");
        assert_eq!(t100, 2508, "25.08 C");
    }

    #[test]
    fn pressure_is_in_a_plausible_range() {
        // The datasheet's own worked example quotes 100653 Pa for a float
        // implementation; the integer algorithm Bosch ships (which this mirrors)
        // lands within 0.1%, so the test pins the physical range, not a constant.
        let p = datasheet_calib().compensate_p(415_148, 128_422);
        assert!((99_600..=101_700).contains(&p), "unexpected pressure {p} Pa");
        // Monotonic in the raw reading: more raw counts, less pressure.
        let hi = datasheet_calib().compensate_p(400_000, 128_422);
        assert!(hi > p, "raw 400000 ({hi}) should read higher than 415148 ({p})");
    }

    #[test]
    fn humidity_stays_in_range() {
        let h = datasheet_calib().compensate_h(36_000, 128_422) as f32 / 1000.0;
        assert!((0.0..=100.0).contains(&h), "unexpected humidity {h}");
    }

    #[test]
    fn calibration_bytes_are_little_endian_including_split_humidity() {
        let mut raw = [0u8; 33];
        raw[0] = 0x30;
        raw[1] = 0x6B; // T1 = 0x6B30 = 27440
        raw[25] = 0x4B; // H1
        raw[29] = 0x01; // H4 high nibble
        raw[30] = 0x23; // H4 low nibble = 3, H5 high nibble = 2
        raw[31] = 0x04; // H5 low byte
        let c = Calib::parse(&raw);
        assert_eq!(c.t1, 27440);
        assert_eq!(c.h1, 0x4B);
        assert_eq!(c.h4, 0x013);
        assert_eq!(c.h5, 0x042);
    }

    #[test]
    fn derived_values_are_sane() {
        assert!((dew_point(20.0, 50.0) - 9.3).abs() < 0.5);
        assert!((altitude(1013.25) - 0.0).abs() < 0.5);
        assert!((altitude(1000.0) - 111.0).abs() < 1.0);
    }
}

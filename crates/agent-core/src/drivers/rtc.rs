//! DS3231 real-time clock (I2C 0x68), with its own temperature sensor.
//!
//! The part has no identification register and 0x68 is a crowded address — the
//! MPU-6050, the PCF8563 and the MCP7940 all answer there. Presence is therefore
//! decided by the only thing a DS3231 does on demand: its seven BCD time registers
//! must decode as a valid date and time. That is weaker evidence than a chip id, so
//! `detect` has to try the IMU first and leave the RTC last on this address.
//!
//! Register map used here: 0x00 seconds, 0x01 minutes, 0x02 hours (bit 6 = 12-hour
//! mode, bit 5 = PM in that mode), 0x03 day (1..=7, the chip's own weekday counter),
//! 0x04 date, 0x05 month (bit 7 = century), 0x06 year, 0x0F status (bit 7 = the
//! oscillator stopped since the flag was cleared), 0x11/0x12 temperature as a signed
//! byte plus two fractional bits of 0.25 degrees.
//!
//! Everything is handled as UTC: the chip stores no zone, and inventing one here
//! would put the firmware's idea of local time in two places.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{integer, object_schema, ToolDef};
use serde_json::{json, Value};

const REG_SECONDS: u8 = 0x00;
/// 0x00..=0x06 in one transaction: seconds .. year.
const LEN_TIME: usize = 7;
const REG_STATUS: u8 = 0x0F;
const REG_TEMP: u8 = 0x11;
/// Status bit 7: the oscillator stopped, so the time is not trustworthy.
const STATUS_OSF: u8 = 0x80;

/// A calendar time as the chip stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Time {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 1 = Monday .. 7 = Sunday (ISO-8601). The chip only counts 1..=7 and the user
    /// assigns what they mean; this driver assigns ISO weekdays.
    pub weekday: u8,
}

impl Time {
    /// Seconds since the Unix epoch, treating the fields as UTC.
    pub fn unix(&self) -> i64 {
        days_from_civil(self.year as i64, self.month as i64, self.day as i64) * 86_400
            + self.hour as i64 * 3_600
            + self.minute as i64 * 60
            + self.second as i64
    }
}

/// One read of the clock: the time, the same instant as Unix seconds, the die
/// temperature and whether the chip lost track of time at some point.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    pub time: Time,
    pub unix: i64,
    pub temperature_c: f32,
    pub oscillator_stopped: bool,
}

#[derive(Debug)]
pub struct Ds3231 {
    pub bus: u8,
    pub addr: u8,
    id: String,
    last: Option<Reading>,
}

impl Ds3231 {
    pub const ADDRESSES: &'static [u8] = &[0x68];

    /// There is no id register, so identification is the BCD check.
    ///
    /// A DS3231 register that a foreign chip also implements has to decode as a valid
    /// field: ten's digits above 9, an hour above 23, a month outside 1..=12 or a
    /// weekday outside 1..=7 all mean "not this part". An MPU-6050 with its registers
    /// cleared reads 0x00 everywhere, which fails the month and weekday checks, and one
    /// that is running does not produce BCD in these seven bytes.
    pub fn identify(hw: &mut dyn Hardware, bus: u8, addr: u8) -> bool {
        let mut raw = [0u8; LEN_TIME];
        if hw.i2c_read(bus, addr, Some(REG_SECONDS), &mut raw).is_err() {
            return false;
        }
        Self::decode(&raw).is_some()
    }

    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        if !Self::identify(hw, bus, addr) {
            return Err(McpError::not_found(format!(
                "no DS3231 at 0x{addr:02X}: the time registers do not decode as BCD"
            )));
        }
        Ok(Self {
            bus,
            addr,
            id: if id_suffix > 1 { format!("ds3231_{id_suffix}") } else { "rtc".into() },
            last: None,
        })
    }

    /// Decode 0x00..=0x06, or `None` when the bytes are not a valid BCD time.
    fn decode(raw: &[u8; LEN_TIME]) -> Option<Time> {
        // Bit 7 of the seconds register is the clock-halt flag, not data.
        let second = bcd(raw[0] & 0x7F)?;
        if second > 59 {
            return None;
        }
        let minute = bcd(raw[1])?;
        if minute > 59 {
            return None;
        }
        let hour = if raw[2] & 0x40 != 0 {
            // 12-hour mode: bit 5 is PM, bit 4 is the tens digit (0 or 1).
            let pm = raw[2] & 0x20 != 0;
            let h = bcd(raw[2] & 0x1F)?;
            if !(1..=12).contains(&h) {
                return None;
            }
            match (pm, h) {
                (false, 12) => 0,
                (false, h) => h,
                (true, 12) => 12,
                (true, h) => h + 12,
            }
        } else {
            let h = bcd(raw[2] & 0x3F)?;
            if h > 23 {
                return None;
            }
            h
        };
        let weekday = bcd(raw[3])?;
        if !(1..=7).contains(&weekday) {
            return None;
        }
        let day = bcd(raw[4])?;
        if !(1..=31).contains(&day) {
            return None;
        }
        let month_reg = raw[5];
        let century = month_reg & 0x80 != 0;
        let month = bcd(month_reg & 0x1F)?;
        if !(1..=12).contains(&month) {
            return None;
        }
        let year = bcd(raw[6])? as u16 + if century { 2100 } else { 2000 };
        Some(Time { year, month, day, hour, minute, second, weekday })
    }

    /// The clock, its temperature and the oscillator flag, in one call.
    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<Reading> {
        let mut raw = [0u8; LEN_TIME];
        hw.i2c_read(self.bus, self.addr, Some(REG_SECONDS), &mut raw)?;
        let time = Self::decode(&raw)
            .ok_or_else(|| McpError::hardware("DS3231 time registers do not decode as BCD"))?;
        let mut t = [0u8; 2];
        hw.i2c_read(self.bus, self.addr, Some(REG_TEMP), &mut t)?;
        // Signed whole degrees, plus the top two bits as quarter-degree fractions.
        let temperature_c = t[0] as i8 as f32 + (t[1] >> 6) as f32 * 0.25;
        let mut status = [0u8; 1];
        hw.i2c_read(self.bus, self.addr, Some(REG_STATUS), &mut status)?;
        let reading = Reading {
            time,
            unix: time.unix(),
            temperature_c,
            oscillator_stopped: status[0] & STATUS_OSF != 0,
        };
        self.last = Some(reading);
        Ok(reading)
    }

    /// Write the clock from Unix seconds (UTC) and clear the oscillator-stopped flag.
    ///
    /// Clearing OSF is the other half of setting the time: a chip whose flag is left
    /// set keeps telling every later reader that its time cannot be trusted.
    pub fn set_unix(&mut self, hw: &mut dyn Hardware, unix: i64) -> Result<Reading> {
        let (year, month, day, hour, minute, second) = civil_from_unix(unix);
        if !(2000..=2199).contains(&year) {
            return Err(McpError::invalid_params(format!(
                "unix {unix} is {year}: the DS3231 counts 2000..=2199"
            )));
        }
        let weekday = iso_weekday(unix);
        let month_reg = to_bcd(month as u8)? | if year >= 2100 { 0x80 } else { 0x00 };
        let raw = [
            to_bcd(second as u8)?,
            to_bcd(minute as u8)?,
            // 24-hour mode: bit 6 clear, so the hour is plain BCD.
            to_bcd(hour as u8)?,
            weekday,
            to_bcd(day as u8)?,
            month_reg,
            to_bcd((year % 100) as u8)?,
        ];
        hw.i2c_write(self.bus, self.addr, Some(REG_SECONDS), &raw)?;
        let mut status = [0u8; 1];
        hw.i2c_read(self.bus, self.addr, Some(REG_STATUS), &mut status)?;
        hw.i2c_write(self.bus, self.addr, Some(REG_STATUS), &[status[0] & !STATUS_OSF])?;
        self.read(hw)
    }

    /// The last reading, for the device detail.
    pub fn last(&self) -> Option<Reading> {
        self.last
    }
}

impl Device for Ds3231 {
    fn info(&self) -> DeviceInfo {
        let mut i =
            DeviceInfo::new("ds3231", &format!("DS3231 real-time clock at 0x{:02X}", self.addr), "i2c");
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["rtc".into(), "i2c".into(), "time".into(), "temp".into()];
        i.detail = json!({ "chip": "DS3231", "bus": self.bus, "zone": "UTC" });
        i.tools = vec!["rtc_read".into(), "rtc_set".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let read_id = self.id.clone();
        let set_id = self.id.clone();
        vec![
            ToolDef::new("rtc_read", "Read the DS3231 clock (UTC), its temperature and its flag")
                .title("RTC read")
                .category("sensor")
                .requires(&["rtc"])
                .schema(object_schema(&[], &[]))
                .bind(move |ctx, args| {
                    let id = read_id.clone();
                    ctx.devices.call(&id, "rtc_read", args, ctx.hw)
                }),
            ToolDef::new(
                "rtc_set",
                "Set the DS3231 clock from Unix seconds (UTC) and clear its stopped flag",
            )
            .title("RTC set")
            .category("sensor")
            .requires(&["rtc"])
            .schema(object_schema(
                &[("unix", integer(946_684_800, 7_258_118_400, "Unix seconds, UTC"))],
                &["unix"],
            ))
            .mutating()
            .bind(move |ctx, args| {
                let id = set_id.clone();
                ctx.devices.call(&id, "rtc_set", args, ctx.hw)
            }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "rtc_read" => Ok(reading_json(&self.read(hw)?)),
            "rtc_set" => {
                let unix = args
                    .get("unix")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| McpError::invalid_params("rtc_set needs `unix` as an integer"))?;
                Ok(reading_json(&self.set_unix(hw, unix)?))
            }
            other => Err(McpError::not_found(format!("rtc has no tool `{other}`"))),
        }
    }
}

fn reading_json(r: &Reading) -> Value {
    json!({
        "unix": r.unix,
        "iso": format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            r.time.year, r.time.month, r.time.day, r.time.hour, r.time.minute, r.time.second
        ),
        "year": r.time.year,
        "month": r.time.month,
        "day": r.time.day,
        "hour": r.time.hour,
        "minute": r.time.minute,
        "second": r.time.second,
        "weekday": r.time.weekday,
        "temperature_c": (r.temperature_c * 100.0).round() / 100.0,
        "oscillator_stopped": r.oscillator_stopped,
    })
}

/// One BCD byte, or `None` when a nibble is not a decimal digit.
fn bcd(byte: u8) -> Option<u8> {
    let (hi, lo) = (byte >> 4, byte & 0x0F);
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(hi * 10 + lo)
}

fn to_bcd(value: u8) -> Result<u8> {
    if value > 99 {
        return Err(McpError::internal(format!("{value} does not fit a BCD byte")));
    }
    Ok(((value / 10) << 4) | (value % 10))
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`: shifts the year to start in March so the leap
/// day is the last of the cycle, which turns the century rules into one expression.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // 0..=399
    let mp = if month > 2 { month - 3 } else { month + 9 }; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month, day)
}

/// (year, month, day, hour, minute, second) in UTC.
fn civil_from_unix(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, secs / 3_600, (secs % 3_600) / 60, secs % 60)
}

/// ISO weekday, 1 = Monday .. 7 = Sunday. 1970-01-01 was a Thursday.
fn iso_weekday(unix: i64) -> u8 {
    ((unix.div_euclid(86_400) + 3).rem_euclid(7) + 1) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;
    use std::collections::VecDeque;

    /// A bus that answers from a script instead of from silicon: one entry per
    /// `i2c_read`, `Err` for a NAK. Writes are recorded, which is how the `rtc_set`
    /// tests see the bytes that would reach the chip.
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
            if self.fail_write {
                return Err(McpError::hardware("I2C NAK on write"));
            }
            self.writes.push((reg, data.to_vec()));
            Ok(())
        }
    }

    fn dev() -> Ds3231 {
        Ds3231 { bus: 0, addr: 0x68, id: "rtc".into(), last: None }
    }

    /// 2024-01-01T12:34:56Z as the chip lays it out: BCD, 24-hour mode, weekday 1.
    fn frame_2024() -> [u8; LEN_TIME] {
        [0x56, 0x34, 0x12, 0x01, 0x01, 0x01, 0x24]
    }

    #[test]
    fn identify_accepts_a_valid_bcd_time() {
        let mut fake = Fake::script(vec![Ok(frame_2024().to_vec())]);
        assert!(Ds3231::identify(&mut fake, 0, 0x68));
    }

    #[test]
    fn identify_rejects_bytes_that_are_not_a_time() {
        // 0x6A in the seconds register: the low nibble is not a decimal digit.
        let mut fake = Fake::script(vec![Ok(vec![0x6A, 0x00, 0x00, 0x01, 0x01, 0x01, 0x24])]);
        assert!(!Ds3231::identify(&mut fake, 0, 0x68));
        // A month of 0x13 (13) is out of range.
        let mut fake = Fake::script(vec![Ok(vec![0x00, 0x00, 0x00, 0x01, 0x01, 0x13, 0x24])]);
        assert!(!Ds3231::identify(&mut fake, 0, 0x68));
        // A chip that answers but NAKs the transaction is not one either.
        let mut fake = Fake::script(vec![Err(McpError::hardware("I2C NAK"))]);
        assert!(!Ds3231::identify(&mut fake, 0, 0x68));
    }

    #[test]
    fn identify_rejects_an_all_zero_frame_like_a_cleared_mpu6050() {
        // The MPU-6050 lives at 0x68 too and reads 0x00 from these registers; the
        // weekday and month checks are what keep the two apart.
        let mut fake = Fake::script(vec![Ok(vec![0x00; 7])]);
        assert!(!Ds3231::identify(&mut fake, 0, 0x68));
    }

    #[test]
    fn probe_of_a_foreign_part_is_not_found() {
        let mut fake = Fake::script(vec![Ok(vec![0x00; 7])]);
        let err = Ds3231::probe(&mut fake, 0, 0x68, 1).expect_err("0x00 everywhere is no clock");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
    }

    #[test]
    fn a_normal_read_returns_utc_seconds_and_the_temperature() {
        // 2024-01-01T12:34:56Z is Unix 1704112496 (2024-01-01T00:00:00Z is the
        // well-known 1704067200, plus 12 h 34 min 56 s). It was a Monday, so the
        // WEEKDAY register holds 1. Temperature registers 0x1A and 0x40 are
        // 26 + 1 * 0.25 = 26.25 degrees, and status 0x00 means the clock never stopped.
        let mut fake = Fake::script(vec![Ok(frame_2024().to_vec()), Ok(vec![0x1A, 0x40]), Ok(vec![0x00])]);
        let mut d = dev();
        let r = d.read(&mut fake).expect("read");
        assert_eq!(r.unix, 1_704_112_496);
        assert_eq!(r.time.weekday, 1);
        assert!((r.temperature_c - 26.25).abs() < 1e-6, "{}", r.temperature_c);
        assert!(!r.oscillator_stopped);
        assert_eq!(d.last().map(|r| r.unix), Some(1_704_112_496));
    }

    #[test]
    fn the_oscillator_flag_is_reported() {
        let mut fake = Fake::script(vec![
            Ok(frame_2024().to_vec()),
            Ok(vec![0xF6, 0xC0]), // -10 + 3 * 0.25 = -9.25 degrees
            Ok(vec![STATUS_OSF]),
        ]);
        let mut d = dev();
        let r = d.read(&mut fake).expect("read");
        assert!(r.oscillator_stopped, "bit 7 of 0x0F must reach the caller");
        assert!((r.temperature_c + 9.25).abs() < 1e-6, "{}", r.temperature_c);
    }

    #[test]
    fn a_bus_error_during_a_read_is_reported() {
        let mut fake = Fake::script(vec![Ok(frame_2024().to_vec()), Err(McpError::hardware("I2C NAK"))]);
        let mut d = dev();
        let err = d.read(&mut fake).expect_err("a NAK must not become a reading");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(d.last().is_none());
    }

    #[test]
    fn setting_the_clock_writes_bcd_and_clears_the_stopped_flag() {
        // Status 0x80 (OSF set) is read, then written back with bit 7 clear; the
        // read-back that `set_unix` returns then serves the time, the temperature
        // and the status again.
        let mut fake = Fake::script(vec![
            Ok(vec![STATUS_OSF]),
            Ok(frame_2024().to_vec()),
            Ok(vec![0x1A, 0x40]),
            Ok(vec![0x00]),
        ]);
        let mut d = dev();
        let r = d.set_unix(&mut fake, 1_704_112_496).expect("set");
        assert_eq!(r.unix, 1_704_112_496);
        assert_eq!(fake.writes[0], (Some(REG_SECONDS), frame_2024().to_vec()));
        // Bit 7 of 0x0F cleared, nothing else touched.
        assert_eq!(fake.writes[1], (Some(REG_STATUS), vec![0x00]));
    }

    #[test]
    fn unix_to_civil_and_back_is_the_documented_anchor() {
        // 2000-01-01T00:00:00Z = 946684800 s, and that day was a Saturday (ISO 6).
        let (y, mo, d, h, mi, s) = civil_from_unix(946_684_800);
        assert_eq!((y, mo, d, h, mi, s), (2000, 1, 1, 0, 0, 0));
        assert_eq!(iso_weekday(946_684_800), 6);
        let t = Time { year: 2000, month: 1, day: 1, hour: 0, minute: 0, second: 0, weekday: 6 };
        assert_eq!(t.unix(), 946_684_800);
        // And a leap day: 2024-02-29T00:00:00Z is 1709164800 s.
        let (y, mo, d, ..) = civil_from_unix(1_709_164_800);
        assert_eq!((y, mo, d), (2024, 2, 29));
    }

    #[test]
    fn twelve_hour_mode_is_decoded_including_noon_and_midnight() {
        // Bit 6 set = 12-hour mode, bit 5 = PM. 0x72 is 12 PM, 0x52 is 12 AM and
        // 0x67 is 7 PM.
        let mut noon = frame_2024();
        noon[2] = 0x72;
        assert_eq!(Ds3231::decode(&noon).expect("valid").hour, 12);
        let mut midnight = frame_2024();
        midnight[2] = 0x52;
        assert_eq!(Ds3231::decode(&midnight).expect("valid").hour, 0);
        let mut evening = frame_2024();
        evening[2] = 0x67;
        assert_eq!(Ds3231::decode(&evening).expect("valid").hour, 19);
    }

    #[test]
    fn the_century_bit_selects_the_hundred() {
        let mut f = frame_2024();
        f[5] = 0x81; // month 1 with the century bit
                     // 0x24 in the year register is 24, so the century bit makes it 2124.
        assert_eq!(Ds3231::decode(&f).expect("valid").year, 2124);
    }

    #[test]
    fn setting_a_date_the_registers_cannot_hold_is_refused() {
        let mut fake = Fake::script(vec![]);
        let mut d = dev();
        let err = d.set_unix(&mut fake, 0).expect_err("1970 does not fit");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        assert!(fake.writes.is_empty(), "a refused set must not write anything");
    }
}

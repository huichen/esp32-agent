//! MPU-6050 / MPU-6500 six-axis IMU (accelerometer + gyroscope + die temperature).
//!
//! `WHO_AM_I` (0x75) is 0x68 for the MPU-6050 and 0x70/0x71/0x73 for some
//! compatible parts; the driver accepts 0x68 and 0x69 as addresses and treats a
//! responding 0x68/0x70/0x71/0x73 id as present.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{enumeration, object_schema, ToolDef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const REG_SMPLRT: u8 = 0x19;
const REG_CONFIG: u8 = 0x1A;
const REG_GYRO_CONFIG: u8 = 0x1B;
const REG_ACCEL_CONFIG: u8 = 0x1C;
const REG_ACCEL_XOUT_H: u8 = 0x3B;
const REG_PWR_MGMT_1: u8 = 0x6B;
const REG_WHO_AM_I: u8 = 0x75;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccelRange {
    G2,
    G4,
    G8,
    G16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GyroRange {
    Dps250,
    Dps500,
    Dps1000,
    Dps2000,
}

impl AccelRange {
    fn lsb_per_g(self) -> f32 {
        match self {
            AccelRange::G2 => 16384.0,
            AccelRange::G4 => 8192.0,
            AccelRange::G8 => 4096.0,
            AccelRange::G16 => 2048.0,
        }
    }

    fn cfg_bits(self) -> u8 {
        match self {
            AccelRange::G2 => 0,
            AccelRange::G4 => 1,
            AccelRange::G8 => 2,
            AccelRange::G16 => 3,
        }
    }
}

impl GyroRange {
    fn lsb_per_dps(self) -> f32 {
        match self {
            GyroRange::Dps250 => 131.0,
            GyroRange::Dps500 => 65.5,
            GyroRange::Dps1000 => 32.8,
            GyroRange::Dps2000 => 16.4,
        }
    }

    fn cfg_bits(self) -> u8 {
        match self {
            GyroRange::Dps250 => 0,
            GyroRange::Dps500 => 1,
            GyroRange::Dps1000 => 2,
            GyroRange::Dps2000 => 3,
        }
    }
}

pub struct Mpu6050 {
    pub bus: u8,
    pub addr: u8,
    pub who: u8,
    accel: AccelRange,
    gyro: GyroRange,
    id: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ImuReading {
    pub accel_g: (f32, f32, f32),
    pub gyro_dps: (f32, f32, f32),
    pub temp_c: f32,
}

impl Mpu6050 {
    pub const ADDRESSES: &'static [u8] = &[0x68, 0x69];

    pub fn identify(hw: &mut dyn Hardware, bus: u8, addr: u8) -> Option<u8> {
        let mut b = [0u8; 1];
        hw.i2c_read(bus, addr, Some(REG_WHO_AM_I), &mut b).ok()?;
        match b[0] {
            0x68 | 0x70 | 0x71 | 0x73 | 0x69 => Some(b[0]),
            _ => None,
        }
    }

    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        let who = Self::identify(hw, bus, addr)
            .ok_or_else(|| McpError::not_found(format!("no MPU-6050 at 0x{addr:02X}")))?;
        let mut d = Self {
            bus,
            addr,
            who,
            accel: AccelRange::G2,
            gyro: GyroRange::Dps250,
            id: if id_suffix > 1 { format!("mpu6050_{id_suffix}") } else { "mpu6050".into() },
        };
        d.init(hw)?;
        Ok(d)
    }

    fn init(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        hw.i2c_write(self.bus, self.addr, Some(REG_PWR_MGMT_1), &[0x80])?; // reset
        hw.delay_us(50_000);
        hw.i2c_write(self.bus, self.addr, Some(REG_PWR_MGMT_1), &[0x01])?; // PLL with X gyro
        hw.i2c_write(self.bus, self.addr, Some(REG_SMPLRT), &[0x04])?; // 200 Hz
        hw.i2c_write(self.bus, self.addr, Some(REG_CONFIG), &[0x03])?; // DLPF 44 Hz
        self.set_ranges(hw, self.accel, self.gyro)
    }

    pub fn set_ranges(&mut self, hw: &mut dyn Hardware, accel: AccelRange, gyro: GyroRange) -> Result<()> {
        hw.i2c_write(self.bus, self.addr, Some(REG_ACCEL_CONFIG), &[accel.cfg_bits() << 3])?;
        hw.i2c_write(self.bus, self.addr, Some(REG_GYRO_CONFIG), &[gyro.cfg_bits() << 3])?;
        self.accel = accel;
        self.gyro = gyro;
        Ok(())
    }

    pub fn read(&mut self, hw: &mut dyn Hardware) -> Result<ImuReading> {
        let mut b = [0u8; 14];
        hw.i2c_read(self.bus, self.addr, Some(REG_ACCEL_XOUT_H), &mut b)?;
        let ax = i16::from_be_bytes([b[0], b[1]]) as f32 / self.accel.lsb_per_g();
        let ay = i16::from_be_bytes([b[2], b[3]]) as f32 / self.accel.lsb_per_g();
        let az = i16::from_be_bytes([b[4], b[5]]) as f32 / self.accel.lsb_per_g();
        let t = i16::from_be_bytes([b[6], b[7]]) as f32 / 340.0 + 36.53;
        let gx = i16::from_be_bytes([b[8], b[9]]) as f32 / self.gyro.lsb_per_dps();
        let gy = i16::from_be_bytes([b[10], b[11]]) as f32 / self.gyro.lsb_per_dps();
        let gz = i16::from_be_bytes([b[12], b[13]]) as f32 / self.gyro.lsb_per_dps();
        Ok(ImuReading { accel_g: (ax, ay, az), gyro_dps: (gx, gy, gz), temp_c: t })
    }
}

impl Device for Mpu6050 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("mpu6050", &format!("MPU-6xxx IMU at 0x{:02X}", self.addr), "i2c");
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["mpu6050".into(), "i2c".into(), "motion".into()];
        i.detail = json!({ "who_am_i": format!("0x{:02X}", self.who), "bus": self.bus,
                           "accel_range_g": self.accel, "gyro_range_dps": self.gyro });
        i.tools = vec!["mpu6050_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new(
            "mpu6050_read",
            "Read accelerometer (g), gyroscope (°/s), die temperature and tilt angles",
        )
        .title("IMU read")
        .category("sensor")
        .requires(&["mpu6050"])
        .schema(object_schema(
            &[(
                "range",
                enumeration(&["2g", "4g", "8g", "16g"], "change the accelerometer range before reading"),
            )],
            &[],
        ))
        .bind(move |ctx, args| {
            let id = id.clone();
            ctx.devices.call(&id, "mpu6050_read", args, ctx.hw)
        })]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "mpu6050_read" => {
                if let Some(r) = crate::json::opt_str(args, "range") {
                    let range = match r.as_str() {
                        "2g" => AccelRange::G2,
                        "4g" => AccelRange::G4,
                        "8g" => AccelRange::G8,
                        "16g" => AccelRange::G16,
                        other => return Err(McpError::invalid_params(format!("unknown range `{other}`"))),
                    };
                    self.set_ranges(hw, range, self.gyro)?;
                    hw.delay_us(10_000);
                }
                let r = self.read(hw)?;
                let (ax, ay, az) = r.accel_g;
                let pitch = (-ax).atan2((ay * ay + az * az).sqrt()).to_degrees();
                let roll = ay.atan2(az).to_degrees();
                Ok(json!({
                    "accel_g": { "x": round2(ax), "y": round2(ay), "z": round2(az) },
                    "gyro_dps": { "x": round2(r.gyro_dps.0), "y": round2(r.gyro_dps.1), "z": round2(r.gyro_dps.2) },
                    "temperature_c": round2(r.temp_c),
                    "pitch_deg": round2(pitch),
                    "roll_deg": round2(roll),
                    "accel_range_g": self.accel,
                    "gyro_range_dps": self.gyro,
                }))
            }
            other => Err(McpError::not_found(format!("imu has no tool `{other}`"))),
        }
    }
}

fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_factors_match_the_datasheet() {
        assert_eq!(AccelRange::G2.lsb_per_g(), 16384.0);
        assert_eq!(AccelRange::G16.lsb_per_g(), 2048.0);
        assert_eq!(GyroRange::Dps250.lsb_per_dps(), 131.0);
        assert_eq!(GyroRange::Dps2000.lsb_per_dps(), 16.4);
    }

    #[test]
    fn config_bits_are_the_high_nibble() {
        assert_eq!(AccelRange::G8.cfg_bits() << 3, 0x10);
        assert_eq!(GyroRange::Dps1000.cfg_bits() << 3, 0x10);
    }
}

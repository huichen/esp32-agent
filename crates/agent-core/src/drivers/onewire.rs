//! DS18B20 (and DS18S20/DS1822) temperature probe on a bit-banged 1-Wire bus.
//!
//! 1-Wire needs a 4.7 kΩ pull-up and microsecond timing, so this driver drives a
//! pin through the [`Hardware`] GPIO API: low to write, input (released) to let
//! the pull-up raise the line. Detection is the presence pulse, confirmed by
//! reading the 64-bit ROM and checking its Dallas CRC.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::tool::{object_schema, ToolDef};
use crate::util::crc8_dallas;
use serde_json::{json, Value};

const CMD_SKIP_ROM: u8 = 0xCC;
const CMD_CONVERT: u8 = 0x44;
const CMD_READ_SCRATCH: u8 = 0xBE;
const CMD_READ_ROM: u8 = 0x33;

pub struct Ds18b20 {
    pub pin: i32,
    rom: [u8; 8],
    id: String,
}

impl Ds18b20 {
    /// Probe one pin. `Ok(None)` means "no presence pulse there", which is the
    /// normal answer for a pin with nothing attached.
    pub fn probe(hw: &mut dyn Hardware, pin: i32) -> Result<Option<Self>> {
        if !Self::reset(hw, pin)? {
            return Ok(None);
        }
        Self::write_byte(hw, pin, CMD_READ_ROM)?;
        let mut rom = [0u8; 8];
        for b in rom.iter_mut() {
            *b = Self::read_byte(hw, pin)?;
        }
        if crc8_dallas(&rom[..7]) != rom[7] {
            return Ok(None);
        }
        // Family codes: 0x28 = DS18B20, 0x10 = DS18S20, 0x22 = DS1822.
        if !matches!(rom[0], 0x28 | 0x10 | 0x22) {
            return Ok(None);
        }
        Ok(Some(Self { pin, rom, id: format!("ds18b20_p{pin}") }))
    }

    fn release(hw: &mut dyn Hardware, pin: i32) -> Result<()> {
        hw.gpio_config(pin, PinMode::InputPullup)
    }

    fn pull_low(hw: &mut dyn Hardware, pin: i32) -> Result<()> {
        hw.gpio_write(pin, false)?;
        hw.gpio_config(pin, PinMode::Output)
    }

    /// Reset pulse + presence detection.
    fn reset(hw: &mut dyn Hardware, pin: i32) -> Result<bool> {
        Self::release(hw, pin)?;
        hw.delay_us(5);
        if !hw.gpio_read(pin)? {
            return Err(McpError::hardware(format!("GPIO{pin} is held low; 1-Wire needs a pull-up")));
        }
        Self::pull_low(hw, pin)?;
        hw.delay_us(500);
        Self::release(hw, pin)?;
        hw.delay_us(70);
        let present = !hw.gpio_read(pin)?;
        hw.delay_us(430);
        Ok(present)
    }

    fn write_bit(hw: &mut dyn Hardware, pin: i32, bit: bool) -> Result<()> {
        Self::pull_low(hw, pin)?;
        if bit {
            hw.delay_us(6);
            Self::release(hw, pin)?;
            hw.delay_us(64);
        } else {
            hw.delay_us(60);
            Self::release(hw, pin)?;
            hw.delay_us(10);
        }
        Ok(())
    }

    fn read_bit(hw: &mut dyn Hardware, pin: i32) -> Result<bool> {
        Self::pull_low(hw, pin)?;
        hw.delay_us(3);
        Self::release(hw, pin)?;
        hw.delay_us(10);
        let v = hw.gpio_read(pin)?;
        hw.delay_us(53);
        Ok(v)
    }

    fn write_byte(hw: &mut dyn Hardware, pin: i32, byte: u8) -> Result<()> {
        for i in 0..8 {
            Self::write_bit(hw, pin, (byte >> i) & 1 == 1)?;
        }
        Ok(())
    }

    fn read_byte(hw: &mut dyn Hardware, pin: i32) -> Result<u8> {
        let mut v = 0u8;
        for i in 0..8 {
            if Self::read_bit(hw, pin)? {
                v |= 1 << i;
            }
        }
        Ok(v)
    }

    /// Temperature in °C. Blocks for the 750 ms conversion time.
    pub fn read_temp(&mut self, hw: &mut dyn Hardware) -> Result<f32> {
        if !Self::reset(hw, self.pin)? {
            return Err(McpError::hardware("no presence pulse (device disconnected?)"));
        }
        Self::write_byte(hw, self.pin, CMD_SKIP_ROM)?;
        Self::write_byte(hw, self.pin, CMD_CONVERT)?;
        hw.delay_us(750_000);
        if !Self::reset(hw, self.pin)? {
            return Err(McpError::hardware("no presence pulse before scratchpad read"));
        }
        Self::write_byte(hw, self.pin, CMD_SKIP_ROM)?;
        Self::write_byte(hw, self.pin, CMD_READ_SCRATCH)?;
        let mut sp = [0u8; 9];
        for b in sp.iter_mut() {
            *b = Self::read_byte(hw, self.pin)?;
        }
        if crc8_dallas(&sp[..8]) != sp[8] {
            return Err(McpError::hardware("scratchpad CRC mismatch"));
        }
        let raw = i16::from_le_bytes([sp[0], sp[1]]);
        if self.rom[0] == 0x10 {
            // DS18S20 reports 0.5 °C steps in the raw field.
            return Ok(raw as f32 / 2.0);
        }
        Ok(raw as f32 / 16.0)
    }

    pub fn rom_hex(&self) -> String {
        crate::util::hex(&self.rom)
    }
}

impl Device for Ds18b20 {
    fn info(&self) -> DeviceInfo {
        let mut i =
            DeviceInfo::new("ds18b20", &format!("DS18B20 temperature probe on GPIO{}", self.pin), "onewire");
        i.id = self.id.clone();
        i.pins = vec![self.pin];
        i.caps = vec!["ds18b20".into(), "onewire".into(), "temp".into()];
        i.detail = json!({ "rom": self.rom_hex(), "family": format!("0x{:02X}", self.rom[0]) });
        i.tools = vec!["ds18b20_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new(
            "ds18b20_read",
            "Read the DS18B20 temperature probe (blocks ~750 ms for the conversion)",
        )
        .title("DS18B20 read")
        .category("sensor")
        .requires(&["ds18b20"])
        .schema(object_schema(&[], &[]))
        .bind(move |ctx, args| {
            let id = id.clone();
            ctx.devices.call(&id, "ds18b20_read", args, ctx.hw)
        })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "ds18b20_read" => {
                let t = self.read_temp(hw)?;
                Ok(json!({
                    "temperature_c": (t * 1000.0).round() / 1000.0,
                    "sensor": "DS18B20",
                    "pin": self.pin,
                    "rom": self.rom_hex(),
                }))
            }
            other => Err(McpError::not_found(format!("ds18b20 has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::hardware::NullHardware;
    use crate::util::crc8_dallas;

    #[test]
    fn scratchpad_crc_is_verified_and_corruption_is_caught() {
        // +25.0625 °C scratchpad: 0x0191 = 401 -> 401/16. The trailing byte is
        // the Dallas/Maxim CRC this implementation computes for those 8 bytes.
        let sp = [0x91u8, 0x01, 0x4B, 0x46, 0x7F, 0xFF, 0x0C, 0x10];
        assert_eq!(crc8_dallas(&sp), 0x70);
        let raw = i16::from_le_bytes([sp[0], sp[1]]);
        assert!((raw as f32 / 16.0 - 25.0625).abs() < 1e-6);

        // A single flipped bit must change the CRC, which is what makes a
        // disconnected probe fail loudly instead of reporting a stale value.
        let mut bad = sp;
        bad[1] ^= 0x02;
        assert_ne!(crc8_dallas(&bad), 0x70);
    }

    #[test]
    fn probing_a_dead_pin_reports_no_device() {
        // NullHardware has no GPIO: the probe must report an error, not panic.
        let mut hw = NullHardware;
        assert!(super::Ds18b20::probe(&mut hw, 4).is_err());
    }
}

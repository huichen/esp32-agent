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
    use super::*;
    use crate::hardware::{BoardInfo, NullHardware};
    use std::collections::VecDeque;

    /// A 1-Wire line with a DS18B20 on the far end. 1-Wire is timing, so the fake measures
    /// how long the driver holds the line low before letting go and decodes that into the
    /// bytes it sent: the datasheet allows 1-15 us for a 1, 60 us and up for a 0, and the
    /// driver's own read slot is 3 us. That turns "the driver toggled some pins" into "the
    /// driver sent 0x33", which is the thing that has to be right for a real probe.
    #[derive(Default)]
    struct Line {
        /// Answers to every `gpio_read` in order: the reset's idle check, the presence
        /// sample, then one bit per bit slot while the slave shifts a byte out.
        answers: VecDeque<bool>,
        /// What the slave holds the line at once the script runs out.
        idle: bool,
        low: bool,
        low_delay: u32,
        /// A low pulse that has ended, in microseconds. Resolved into a bit by whatever the
        /// driver does next, because a 6 us pulse is a 1 only if no read follows it.
        pending: Option<u32>,
        bits: Vec<bool>,
        reads: usize,
        configs: Vec<PinMode>,
    }

    impl Line {
        fn scripted(answers: Vec<bool>, idle: bool) -> Self {
            Self { answers: answers.into(), idle, ..Default::default() }
        }

        /// 30 us separates the datasheet's 1 (up to 15 us) from its 0 (60 us and up); the
        /// read slot (3 us) and the reset pulse (500 us) are not bits the driver wrote.
        fn resolve(&mut self, delay: u32) {
            match delay {
                30..=200 => self.bits.push(false),
                5..=29 => self.bits.push(true),
                _ => {}
            }
        }

        /// The bytes the driver sent, LSB first, decoded from the pulse lengths.
        fn written_bytes(&self) -> Vec<u8> {
            self.bits
                .chunks(8)
                .filter(|c| c.len() == 8)
                .map(|c| c.iter().enumerate().fold(0u8, |acc, (i, b)| acc | ((*b as u8) << i)))
                .collect()
        }
    }

    impl Hardware for Line {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, _pin: i32, mode: PinMode) -> Result<()> {
            self.configs.push(mode);
            if mode == PinMode::InputPullup && self.low {
                self.pending = Some(self.low_delay);
                self.low = false;
            }
            Ok(())
        }

        fn gpio_write(&mut self, _pin: i32, high: bool) -> Result<()> {
            if !high {
                if let Some(d) = self.pending.take() {
                    self.resolve(d);
                }
                self.low = true;
                self.low_delay = 0;
            }
            Ok(())
        }

        fn gpio_read(&mut self, _pin: i32) -> Result<bool> {
            // A pulse that ends just before a read either was this read's own slot or was
            // the last bit of the byte the driver just finished sending.
            if let Some(d) = self.pending.take() {
                self.resolve(d);
            }
            self.reads += 1;
            Ok(self.answers.pop_front().unwrap_or(self.idle))
        }

        fn delay_us(&mut self, us: u32) {
            if self.low {
                self.low_delay += us;
            }
        }
    }

    fn bits_lsb(bytes: &[u8]) -> Vec<bool> {
        let mut out = Vec::new();
        for b in bytes {
            for i in 0..8 {
                out.push((b >> i) & 1 == 1);
            }
        }
        out
    }

    /// A ROM with a valid Dallas CRC. The polynomial itself is pinned against the datasheet
    /// in `util::tests`; here it only has to make a fixture the driver will accept.
    fn rom(family: u8) -> [u8; 8] {
        let mut r = [family, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x00];
        r[7] = crc8_dallas(&r[..7]);
        r
    }

    fn probe_script(r: &[u8; 8]) -> Line {
        let mut a = vec![true, false]; // line idle high, then the presence pulse
        a.extend(bits_lsb(r));
        Line::scripted(a, true)
    }

    /// +25.0625 °C is the datasheet's example: raw 0x0191, and the ninth byte is the CRC of
    /// the first eight.
    fn scratchpad() -> [u8; 9] {
        let mut sp = [0x91u8, 0x01, 0x4B, 0x46, 0x7F, 0xFF, 0x0C, 0x10, 0x00];
        sp[8] = crc8_dallas(&sp[..8]);
        sp
    }

    fn temp_script(sp: &[u8; 9]) -> Line {
        let mut a = vec![true, false, true, false]; // two resets, each with a presence pulse
        a.extend(bits_lsb(sp));
        Line::scripted(a, true)
    }

    #[test]
    fn probe_finds_a_ds18b20_and_reads_its_rom() {
        let r = rom(0x28);
        let mut line = probe_script(&r);
        let d = Ds18b20::probe(&mut line, 4).expect("probe").expect("a part answered");
        assert_eq!(d.pin, 4);
        assert_eq!(d.rom, r);
        assert_eq!(d.id, "ds18b20_p4", "the pin is part of the id, so two probes do not collide");
        assert_eq!(line.written_bytes(), vec![CMD_READ_ROM], "detection is exactly READ ROM");
        assert!(line.configs.contains(&PinMode::InputPullup), "the line is released, not driven high");
    }

    /// A pin with nothing on it sees no presence pulse. That is `Ok(None)`, not an error,
    /// because the board probes every candidate pin and one of them is always empty.
    #[test]
    fn a_pin_with_nothing_attached_is_not_a_device() {
        let mut line = Line::scripted(vec![true, true], true); // no presence pulse
        assert!(Ds18b20::probe(&mut line, 5).expect("an empty pin is not a failure").is_none());
    }

    /// The CRC is the only thing that distinguishes a shifted-out ROM from noise on a long
    /// cable, so one flipped bit has to mean "no device".
    #[test]
    fn a_rom_with_a_bad_crc_is_not_a_device() {
        let mut r = rom(0x28);
        r[3] ^= 0x01;
        let mut line = probe_script(&r);
        assert!(Ds18b20::probe(&mut line, 4).expect("probe").is_none());
    }

    /// 1-Wire has many family codes; this driver decodes the three temperature parts and
    /// refuses to call an EEPROM or an iButton a thermometer.
    #[test]
    fn only_the_temperature_families_are_accepted() {
        for family in [0x28u8, 0x10, 0x22] {
            let r = rom(family);
            let mut line = probe_script(&r);
            assert!(Ds18b20::probe(&mut line, 4).expect("probe").is_some(), "family 0x{family:02X}");
        }
        for family in [0x01u8, 0x2D, 0x00] {
            let r = rom(family);
            let mut line = probe_script(&r);
            assert!(Ds18b20::probe(&mut line, 4).expect("probe").is_none(), "family 0x{family:02X}");
        }
    }

    /// A line held low is a wiring fault (no pull-up, or a short), and resetting it would
    /// read as a device that is always present. The driver says so instead.
    #[test]
    fn a_line_held_low_is_reported_as_a_wiring_fault() {
        let mut line = Line::scripted(vec![false], false);
        let err = Ds18b20::probe(&mut line, 4).err().expect("held low is an error, not absence");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(err.message.contains("GPIO4") && err.message.contains("pull-up"), "{}", err.message);
    }

    /// The full conversion: reset, SKIP ROM, CONVERT, wait, reset, SKIP ROM, READ
    /// SCRATCHPAD, nine bytes. 0x0191 over 16 is 25.0625 °C
    #[test]
    fn a_temperature_read_sends_convert_then_reads_the_scratchpad() {
        let sp = scratchpad();
        let mut line = temp_script(&sp);
        let mut d = Ds18b20 { pin: 4, rom: rom(0x28), id: "ds18b20_p4".into() };
        let t = d.read_temp(&mut line).expect("read");
        assert!((t - 25.0625).abs() < 1e-6, "got {t}");
        assert_eq!(
            line.written_bytes(),
            vec![CMD_SKIP_ROM, CMD_CONVERT, CMD_SKIP_ROM, CMD_READ_SCRATCH],
            "0xCC 0x44 then 0xCC 0xBE: {:?}",
            line.bits
        );
    }

    /// The DS18S20 reports half-degree steps in the same field, so the same raw value means
    /// 200.5 °C on that part rather than 401 °C. The family code in the ROM is what decides.
    #[test]
    fn a_ds18s20_reading_is_half_degree_steps() {
        let sp = scratchpad();
        let mut line = temp_script(&sp);
        let mut d = Ds18b20 { pin: 4, rom: rom(0x10), id: "ds18b20_p4".into() };
        let t = d.read_temp(&mut line).expect("read");
        assert!((t - 200.5).abs() < 1e-6, "0x0191 on a DS18S20: got {t}");
    }

    #[test]
    fn a_scratchpad_with_a_bad_crc_is_not_a_temperature() {
        let mut sp = scratchpad();
        sp[0] ^= 0x04;
        let mut line = temp_script(&sp);
        let mut d = Ds18b20 { pin: 4, rom: rom(0x28), id: "ds18b20_p4".into() };
        let err = d.read_temp(&mut line).expect_err("corrupt scratchpad");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert!(err.message.contains("CRC"), "{}", err.message);
    }

    /// A probe that answered at boot and then came off the bus has to fail, not return the
    /// last reading: both resets are checked, and the second one is what a loose cable hits
    /// in the middle of a conversion.
    #[test]
    fn a_probe_that_disappears_fails_both_resets() {
        let mut gone_at_start = Line::scripted(vec![true, true], true);
        let mut d = Ds18b20 { pin: 4, rom: rom(0x28), id: "ds18b20_p4".into() };
        let err = d.read_temp(&mut gone_at_start).expect_err("no presence at all");
        assert!(err.message.contains("disconnected"), "{}", err.message);

        // Present for the convert, gone before the scratchpad read.
        let mut gone_after = Line::scripted(vec![true, false, true, true], true);
        let err = d.read_temp(&mut gone_after).expect_err("no presence before the read");
        assert!(err.message.contains("scratchpad"), "{}", err.message);
    }

    #[test]
    fn the_tool_returns_the_temperature_with_the_rom_it_came_from() {
        let r = rom(0x28);
        let sp = scratchpad();
        let mut line = temp_script(&sp);
        let mut d = Ds18b20 { pin: 4, rom: r, id: "ds18b20_p4".into() };
        let v = d.call("ds18b20_read", &json!({}), &mut line).expect("read");
        assert_eq!(v["sensor"], "DS18B20");
        assert_eq!(v["pin"], 4);
        assert_eq!(v["rom"], crate::util::hex(&r));
        // f32 in, so compare to a thousandth rather than to an exact decimal.
        let t = v["temperature_c"].as_f64().expect("a number");
        assert!((t - 25.063).abs() < 1e-6, "rounded to a thousandth: {t}");

        let err = d.call("ds18b20_reed", &json!({}), &mut line).expect_err("not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
        assert!(err.message.contains("ds18b20_reed"), "{}", err.message);
    }

    #[test]
    fn the_inventory_row_names_the_rom_and_family() {
        let d = Ds18b20 { pin: 4, rom: rom(0x28), id: "ds18b20_p4".into() };
        let info = d.info();
        assert_eq!(info.kind, "ds18b20");
        assert_eq!(info.bus, "onewire");
        assert_eq!(info.pins, vec![4]);
        assert!(info.name.contains("GPIO4"), "{}", info.name);
        assert_eq!(info.detail["family"], "0x28");
        assert_eq!(info.detail["rom"], d.rom_hex());
        assert_eq!(info.tools, vec!["ds18b20_read"]);
        assert_eq!(d.rom_hex().len(), 16, "eight bytes as hex");
    }

    #[test]
    fn the_published_tool_is_dispatchable_on_a_board_with_the_part() {
        let names: Vec<String> = Ds18b20 { pin: 4, rom: rom(0x28), id: "x".into() }
            .tools()
            .iter()
            .map(|t| t.spec.name.clone())
            .collect();
        assert_eq!(names, vec!["ds18b20_read"]);
        let mut line = temp_script(&scratchpad());
        let mut d = Ds18b20 { pin: 4, rom: rom(0x28), id: "x".into() };
        d.call(&names[0], &json!({}), &mut line).expect("the published name works");
    }

    /// A board with no GPIO cannot have a 1-Wire bus, and the probe has to say that rather
    /// than report no device: "no device" would send someone looking for a wiring fault.
    #[test]
    fn a_board_without_gpio_reports_an_error_not_absence() {
        let mut hw = NullHardware;
        let err = Ds18b20::probe(&mut hw, 4).err().expect("no GPIO is an error");
        assert_eq!(err.code, crate::error::code::UNSUPPORTED);
    }
}

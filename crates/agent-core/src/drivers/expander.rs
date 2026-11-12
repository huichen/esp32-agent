//! PCF8574 I2C port expander, and the HD44780 character LCD most people hang
//! off one (the ubiquitous "I2C 1602 backpack").
//!
//! Detection writes 0xFF to the port and reads it back: a PCF8574 with no
//! external pull-downs echoes 0xFF, while most other chips at 0x20..0x27 do not.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{boolean, integer, object_schema, string, ToolDef};
use serde_json::{json, Value};

/// Backpack wiring: P0=RS, P1=RW, P2=EN, P3=backlight, P4..P7=D4..D7.
const RS: u8 = 0x01;
const EN: u8 = 0x04;
const BL: u8 = 0x08;

pub struct Pcf8574 {
    pub bus: u8,
    pub addr: u8,
    state: u8,
    backlight: bool,
    lcd_ready: bool,
    id: String,
}

impl Pcf8574 {
    pub const ADDRESSES: &'static [u8] = &[0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27];

    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, id_suffix: usize) -> Result<Self> {
        hw.i2c_write(bus, addr, None, &[0xFF])?;
        let mut b = [0u8; 1];
        hw.i2c_read(bus, addr, None, &mut b)?;
        if b[0] != 0xFF {
            return Err(McpError::not_found(format!("no PCF8574 at 0x{addr:02X} (reads 0x{:02X})", b[0])));
        }
        Ok(Self {
            bus,
            addr,
            state: 0xFF,
            backlight: true,
            lcd_ready: false,
            id: if id_suffix > 1 { format!("pcf8574_{id_suffix}") } else { "pcf8574".into() },
        })
    }

    pub fn read_port(&mut self, hw: &mut dyn Hardware) -> Result<u8> {
        let mut b = [0u8; 1];
        hw.i2c_read(self.bus, self.addr, None, &mut b)?;
        self.state = b[0];
        Ok(b[0])
    }

    pub fn write_port(&mut self, hw: &mut dyn Hardware, value: u8) -> Result<()> {
        hw.i2c_write(self.bus, self.addr, None, &[value])?;
        self.state = value;
        Ok(())
    }

    fn pulse(&mut self, hw: &mut dyn Hardware, nibble: u8, rs: bool) -> Result<()> {
        let base = (nibble << 4) | if rs { RS } else { 0 } | if self.backlight { BL } else { 0 };
        self.write_port(hw, base)?;
        hw.delay_us(60);
        self.write_port(hw, base | EN)?;
        hw.delay_us(600);
        self.write_port(hw, base & !EN)?;
        hw.delay_us(600);
        Ok(())
    }

    fn send(&mut self, hw: &mut dyn Hardware, byte: u8, rs: bool) -> Result<()> {
        self.pulse(hw, byte >> 4, rs)?;
        self.pulse(hw, byte & 0x0F, rs)?;
        Ok(())
    }

    pub fn lcd_init(&mut self, hw: &mut dyn Hardware, lines: u8) -> Result<()> {
        self.backlight = true;
        hw.delay_us(60_000);
        for _ in 0..2 {
            self.pulse(hw, 0x03, false)?;
            hw.delay_us(5_000);
        }
        self.pulse(hw, 0x02, false)?; // 4-bit mode
        hw.delay_us(1_000);
        self.send(hw, if lines > 1 { 0x28 } else { 0x20 }, false)?; // 4-bit, 2 lines, 5x8
        self.send(hw, 0x0C, false)?; // display on, cursor off
        self.send(hw, 0x01, false)?; // clear
        hw.delay_us(2_000);
        self.send(hw, 0x06, false)?; // entry mode: increment, no shift
        self.lcd_ready = true;
        Ok(())
    }

    pub fn lcd_write(&mut self, hw: &mut dyn Hardware, line: u8, text: &str) -> Result<()> {
        if !self.lcd_ready {
            self.lcd_init(hw, 2)?;
        }
        self.send(hw, if line == 0 { 0x80 } else { 0xC0 }, false)?;
        for ch in text.chars().take(20) {
            let b = if (ch as u32) < 0x20 || (ch as u32) > 0x7E { b'?' } else { ch as u8 };
            self.send(hw, b, true)?;
        }
        Ok(())
    }

    pub fn lcd_clear(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        if !self.lcd_ready {
            self.lcd_init(hw, 2)?;
        }
        self.send(hw, 0x01, false)?;
        hw.delay_us(2_000);
        Ok(())
    }

    pub fn set_backlight(&mut self, hw: &mut dyn Hardware, on: bool) -> Result<()> {
        self.backlight = on;
        let v = self.state & !BL;
        self.write_port(hw, if on { v | BL } else { v })
    }
}

impl Device for Pcf8574 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "pcf8574",
            &format!("PCF8574 I2C expander at 0x{:02X} (LCD backpack wiring)", self.addr),
            "i2c",
        );
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["pcf8574".into(), "lcd".into(), "i2c".into()];
        i.detail = json!({ "bus": self.bus, "port": self.state, "backlight": self.backlight });
        i.tools = vec![
            "pcf8574_read".into(),
            "pcf8574_write".into(),
            "lcd_text".into(),
            "lcd_clear".into(),
            "lcd_backlight".into(),
        ];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let r = id.clone();
        let w = id.clone();
        let t = id.clone();
        let c = id.clone();
        let b = id.clone();
        vec![
            ToolDef::new("pcf8574_read", "Read the PCF8574 port (8 bits) as a hex value")
                .title("Expander read")
                .category("gpio")
                .requires(&["pcf8574"])
                .schema(object_schema(&[], &[]))
                .bind(move |ctx, args| {
                    let id = r.clone();
                    ctx.devices.call(&id, "pcf8574_read", args, ctx.hw)
                }),
            ToolDef::new("pcf8574_write", "Write all 8 pins of the PCF8574 port")
                .title("Expander write")
                .category("gpio")
                .requires(&["pcf8574"])
                .schema(object_schema(&[("value", integer(0, 255, "port value, bit 0 = P0"))], &["value"]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = w.clone();
                    ctx.devices.call(&id, "pcf8574_write", args, ctx.hw)
                }),
            ToolDef::new("lcd_text", "Write text to a HD44780 LCD connected through the expander")
                .title("LCD text")
                .category("display")
                .requires(&["lcd"])
                .schema(object_schema(
                    &[
                        ("text", string("up to 20 ASCII characters")),
                        ("line", integer(0, 3, "row, 0 or 1 on a 16x2 panel")),
                        ("init", boolean("send the 4-bit init sequence first, default false")),
                    ],
                    &["text"],
                ))
                .mutating()
                .bind(move |ctx, args| {
                    let id = t.clone();
                    ctx.devices.call(&id, "lcd_text", args, ctx.hw)
                }),
            ToolDef::new("lcd_clear", "Clear the HD44780 LCD")
                .title("LCD clear")
                .category("display")
                .requires(&["lcd"])
                .mutating()
                .bind(move |ctx, args| {
                    let id = c.clone();
                    ctx.devices.call(&id, "lcd_clear", args, ctx.hw)
                }),
            ToolDef::new("lcd_backlight", "Turn the LCD backlight on or off")
                .title("LCD backlight")
                .category("display")
                .requires(&["lcd"])
                .schema(object_schema(&[("on", boolean("backlight state"))], &["on"]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = b.clone();
                    ctx.devices.call(&id, "lcd_backlight", args, ctx.hw)
                }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "pcf8574_read" => {
                let v = self.read_port(hw)?;
                let bits: Vec<u8> = (0..8u8).map(|i| (v >> i) & 1).collect();
                Ok(json!({ "port": format!("0x{v:02X}"), "bits": bits }))
            }
            "pcf8574_write" => {
                let v = crate::json::ranged_i64(args, "value", 0, 0, 255)? as u8;
                self.write_port(hw, v)?;
                Ok(json!({ "ok": true, "port": format!("0x{v:02X}") }))
            }
            "lcd_text" => {
                let text = crate::json::str_arg(args, "text")?;
                let line = crate::json::ranged_i64(args, "line", 0, 0, 3)? as u8;
                if crate::json::opt_bool(args, "init").unwrap_or(false) || !self.lcd_ready {
                    self.lcd_init(hw, 2)?;
                }
                self.lcd_write(hw, line, &text)?;
                Ok(json!({ "ok": true, "chars": text.chars().count(), "line": line }))
            }
            "lcd_clear" => {
                self.lcd_clear(hw)?;
                Ok(json!({ "ok": true }))
            }
            "lcd_backlight" => {
                let on = crate::json::bool_arg(args, "on")?;
                self.set_backlight(hw, on)?;
                Ok(json!({ "ok": true, "backlight": on }))
            }
            other => Err(McpError::not_found(format!("expander has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;

    /// The expander answers through one byte per transfer, so the fake is a one-byte port
    /// plus a log of every transfer: the LCD is driven entirely by the order and value of
    /// writes, and that order is what these tests check.
    #[derive(Default)]
    struct Bus {
        port: u8,
        /// Every transfer, in order: `(bus, addr, reg, data)`. A read logs empty data.
        log: Vec<(u8, u8, Option<u8>, Vec<u8>)>,
        read_err: Option<McpError>,
        write_err: Option<McpError>,
    }

    impl Bus {
        fn echo(port: u8) -> Self {
            Self { port, ..Default::default() }
        }

        fn writes(&self) -> Vec<u8> {
            self.log.iter().map(|(_, _, _, d)| d[0]).collect()
        }
    }

    impl Hardware for Bus {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn i2c_read(&mut self, bus: u8, addr: u8, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
            if let Some(e) = &self.read_err {
                return Err(e.clone());
            }
            self.log.push((bus, addr, reg, Vec::new()));
            out[0] = self.port;
            Ok(())
        }

        fn i2c_write(&mut self, bus: u8, addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
            if let Some(e) = &self.write_err {
                return Err(e.clone());
            }
            self.log.push((bus, addr, reg, data.to_vec()));
            // The port latch is not the pin state: a read returns what the outside world
            // pulls the pins to, which the test scripts with `port`.
            Ok(())
        }
    }

    fn dev() -> Pcf8574 {
        Pcf8574 { bus: 1, addr: 0x27, state: 0xFF, backlight: true, lcd_ready: false, id: "pcf8574".into() }
    }

    /// The 4-bit protocol is two nibbles per byte, each latched by a low-high-low pulse on
    /// EN. Decoding the log back into `(nibble, RS)` proves the driver spoke that, which a
    /// write count cannot show.
    fn nibbles(writes: &[u8]) -> Vec<(u8, bool)> {
        assert_eq!(writes.len() % 3, 0, "every nibble is three writes: latch low, high, low");
        writes
            .chunks(3)
            .map(|c| {
                assert_eq!(c[1], c[0] | EN, "the middle write raises EN");
                assert_eq!(c[2], c[0], "the third write drops EN");
                assert_eq!(c[0] & EN, 0, "the resting state has EN low");
                ((c[0] >> 4) & 0x0F, c[0] & RS != 0)
            })
            .collect()
    }

    #[test]
    fn probe_accepts_a_port_that_echoes_ff() {
        let mut bus = Bus::echo(0xFF);
        let d = Pcf8574::probe(&mut bus, 0, 0x20, 1).expect("an echoing port is a PCF8574");
        assert_eq!(d.addr, 0x20);
        assert_eq!(d.id, "pcf8574", "a single expander has no numeric suffix");
        assert!(d.backlight, "the backpack comes up lit");
        assert!(!d.lcd_ready, "probing the port says nothing about the LCD");
        assert_eq!(bus.log[0], (0, 0x20, None, vec![0xFF]), "probe writes all ones first");
    }

    #[test]
    fn probe_numbers_the_second_expander() {
        let mut bus = Bus::echo(0xFF);
        let d = Pcf8574::probe(&mut bus, 0, 0x21, 2).expect("probe");
        assert_eq!(d.id, "pcf8574_2");
    }

    /// A chip that does not echo 0xFF is something else at that address (another backpack
    /// variant, a keypad, a sensor). Reporting it as an expander would let a write tool
    /// drive the wrong device, so the value that answered is part of the message.
    #[test]
    fn probe_refuses_a_bus_address_that_reads_something_else() {
        let mut bus = Bus::echo(0x5A);
        let err = Pcf8574::probe(&mut bus, 0, 0x23, 1).err().expect("0x5A is not this part");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
        assert!(err.message.contains("0x23"), "{}", err.message);
        assert!(err.message.contains("0x5A"), "the message carries what answered: {}", err.message);
    }

    #[test]
    fn probe_reports_a_bus_that_never_answers() {
        let mut bus = Bus { read_err: Some(McpError::hardware("I2C NAK at 0x24")), ..Default::default() };
        let err = Pcf8574::probe(&mut bus, 0, 0x24, 1).err().expect("a NAK is not a device");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    #[test]
    fn reading_the_port_returns_the_pins_and_remembers_them() {
        let mut bus = Bus::echo(0xA5);
        let mut d = dev();
        assert_eq!(d.read_port(&mut bus).expect("read"), 0xA5);
        assert_eq!(d.state, 0xA5, "the cached port follows a read, so the backlight bit is real");
        assert_eq!(bus.log[0], (1, 0x27, None, Vec::new()), "a read is a plain one-byte transfer");
    }

    #[test]
    fn writing_the_port_drives_the_pins_and_caches_the_value() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.write_port(&mut bus, 0x0F).expect("write");
        assert_eq!(bus.writes(), vec![0x0F]);
        assert_eq!(d.state, 0x0F);
        assert_eq!(d.info().detail["port"], 0x0F, "the inventory row reports the driven value");

        bus.write_err = Some(McpError::hardware("I2C NAK"));
        let err = d.write_port(&mut bus, 0x00).expect_err("a NAK must not look like success");
        assert_eq!(err.code, crate::error::code::HARDWARE);
        assert_eq!(d.state, 0x0F, "a failed write leaves the cached state alone");
    }

    /// The 4-bit initialisation sequence from the HD44780 datasheet, as the wiring defines
    /// it: two 0x03s (8-bit wake-up), one 0x02 (switch to 4-bit), then function set,
    /// display on, clear, entry mode. Any other order leaves a real panel blank.
    #[test]
    fn lcd_init_sends_the_datasheet_wake_up_then_function_and_entry_mode() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.lcd_init(&mut bus, 2).expect("init");
        let seq = nibbles(&bus.writes());
        assert_eq!(
            seq,
            vec![
                (0x03, false),
                (0x03, false),
                (0x02, false),
                (0x02, false),
                (0x08, false),
                (0x00, false),
                (0x0C, false),
                (0x00, false),
                (0x01, false),
                (0x00, false),
                (0x06, false),
            ]
        );
        assert!(d.lcd_ready);
        assert!(bus.writes().iter().all(|b| b & BL != 0), "the panel is lit throughout init");
    }

    #[test]
    fn a_one_line_panel_gets_the_one_line_function_set() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.lcd_init(&mut bus, 1).expect("init");
        let seq = nibbles(&bus.writes());
        // 0x20 instead of 0x28: 4-bit, one line, 5x8, so the second nibble is 0 and not 8.
        assert_eq!(seq[3], (0x02, false));
        assert_eq!(seq[4], (0x00, false), "a one-line panel gets function set 0x20: {seq:?}");
    }

    #[test]
    fn lcd_text_initialises_a_panel_that_was_never_initialised() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.lcd_write(&mut bus, 0, "A").expect("write");
        assert!(d.lcd_ready, "writing text brings the panel up rather than sending bytes into a blank");
        let seq = nibbles(&bus.writes());
        // Init (11 nibbles), then the DDRAM address 0x00, then 'A' as two nibbles.
        assert_eq!(seq.len(), 11 + 2 + 2);
        assert_eq!(&seq[11..], &[(0x08, false), (0x00, false), (0x04, true), (0x01, true)]);
    }

    #[test]
    fn the_second_line_is_address_c0() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.lcd_init(&mut bus, 2).expect("init");
        bus.log.clear();
        d.lcd_write(&mut bus, 1, "X").expect("write");
        let seq = nibbles(&bus.writes());
        assert_eq!(&seq[..2], &[(0x0C, false), (0x00, false)], "line 1 starts at DDRAM 0xC0");
        assert_eq!(&seq[2..], &[(0x05, true), (0x08, true)], "'X' is 0x58");
    }

    #[test]
    fn text_is_clipped_to_the_twenty_character_line() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.lcd_init(&mut bus, 2).expect("init");
        bus.log.clear();
        d.lcd_write(&mut bus, 0, "0123456789ABCDEFGHIJKLMNO").expect("write");
        let seq = nibbles(&bus.writes());
        // The address plus 20 characters, each character two nibbles.
        assert_eq!(seq.len(), 2 + 20 * 2, "a 25-character string must stop at 20");
    }

    /// The panel is an ASCII device: anything outside 0x20..0x7E would land in the CGROM
    /// as an arbitrary glyph, so it becomes '?' instead of noise on the screen.
    #[test]
    fn characters_the_panel_cannot_show_become_question_marks() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.lcd_init(&mut bus, 2).expect("init");
        bus.log.clear();
        d.lcd_write(&mut bus, 0, "e\u{301}\u{4e2d}\u{1f600}x").expect("write");
        let data: Vec<(u8, bool)> = nibbles(&bus.writes()).into_iter().filter(|(_, rs)| *rs).collect();
        let bytes: Vec<u8> = data.chunks(2).map(|c| (c[0].0 << 4) | c[1].0).collect();
        // 'e' is printable; the combining accent, the CJK character and the emoji are not.
        assert_eq!(bytes, vec![b'e', b'?', b'?', b'?', b'x'], "got {:?}", String::from_utf8_lossy(&bytes));
    }

    #[test]
    fn clearing_a_cold_panel_initialises_it_first() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.lcd_clear(&mut bus).expect("clear");
        assert!(d.lcd_ready);
        let seq = nibbles(&bus.writes());
        assert_eq!(&seq[seq.len() - 2..], &[(0x00, false), (0x01, false)], "clear is command 0x01");
    }

    /// The backlight is P3 and shares the port with RS, EN and the data nibble. Toggling it
    /// must not disturb whatever else is driven, which is why the driver keeps `state`.
    #[test]
    fn the_backlight_toggles_without_disturbing_the_other_pins() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.write_port(&mut bus, 0x0A).expect("write"); // P1 and P3 high
        d.set_backlight(&mut bus, false).expect("off");
        assert_eq!(bus.writes().last(), Some(&0x02), "P3 drops, P1 stays");
        assert_eq!(d.info().detail["backlight"], false);

        d.set_backlight(&mut bus, true).expect("on");
        assert_eq!(bus.writes().last(), Some(&0x0A), "P3 returns and P1 is untouched");
        assert_eq!(d.info().detail["backlight"], true);
    }

    #[test]
    fn the_inventory_row_names_the_part_and_its_tools() {
        let d = dev();
        let info = d.info();
        assert_eq!(info.kind, "pcf8574");
        assert_eq!(info.bus, "i2c");
        assert_eq!(info.address, Some(0x27));
        assert!(info.name.contains("0x27"), "{}", info.name);
        assert!(info.caps.contains(&"lcd".to_string()));
        assert_eq!(
            info.tools,
            vec!["pcf8574_read", "pcf8574_write", "lcd_text", "lcd_clear", "lcd_backlight"]
        );
    }

    /// Every tool a driver publishes is addressable by that exact name, so the names in
    /// `tools()` and the arms of `call` have to be the same set.
    #[test]
    fn every_published_tool_is_dispatchable() {
        let names: Vec<String> = dev().tools().iter().map(|t| t.spec.name.clone()).collect();
        assert_eq!(names.len(), 5);
        for name in &names {
            let mut hw = Bus::default();
            let args = json!({ "value": 0, "text": "x", "on": true });
            dev().call(name, &args, &mut hw).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let err = dev().call("pcf8574_rset", &json!({}), &mut Bus::default()).expect_err("not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
        assert!(err.message.contains("pcf8574_rset"), "{}", err.message);
    }

    #[test]
    fn reading_through_the_tool_returns_the_bits_not_just_the_byte() {
        let mut bus = Bus::echo(0x81);
        let mut d = dev();
        let v = d.call("pcf8574_read", &json!({}), &mut bus).expect("read");
        assert_eq!(v["port"], "0x81");
        assert_eq!(v["bits"], json!([1, 0, 0, 0, 0, 0, 0, 1]), "bit 0 is P0 and the list is LSB first");
        assert_eq!(d.state, 0x81);
    }

    #[test]
    fn writing_through_the_tool_drives_the_byte_it_was_given() {
        let mut bus = Bus::default();
        let mut d = dev();
        let v = d.call("pcf8574_write", &json!({ "value": 0x5A }), &mut bus).expect("write");
        assert_eq!(v["port"], "0x5A");
        assert_eq!(bus.writes(), vec![0x5A]);
    }

    /// The port is one byte. 256 is refused before the bus is touched, so a bad argument
    /// cannot leave the pins in a half-driven state.
    #[test]
    fn a_port_value_outside_one_byte_is_refused_before_the_bus() {
        let mut bus = Bus::default();
        let mut d = dev();
        let err = d.call("pcf8574_write", &json!({ "value": 256 }), &mut bus).expect_err("over a byte");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        assert!(bus.log.is_empty(), "a refused value must not reach the expander");
        d.call("pcf8574_write", &json!({ "value": 255 }), &mut bus).expect("255 is still a byte");
    }

    #[test]
    fn text_without_the_text_argument_is_a_parameter_error() {
        let mut bus = Bus::default();
        let mut d = dev();
        let err = d.call("lcd_text", &json!({ "line": 0 }), &mut bus).expect_err("text is required");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        assert!(err.message.contains("text"), "{}", err.message);
        assert!(bus.log.is_empty());
    }

    #[test]
    fn a_line_beyond_the_panel_is_refused() {
        let mut bus = Bus::default();
        let mut d = dev();
        let err = d.call("lcd_text", &json!({ "text": "x", "line": 4 }), &mut bus).expect_err("4 rows");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
    }

    /// `init: true` re-sends the wake-up even to a panel this driver already brought up,
    /// which is the escape hatch for a panel that was reset behind the driver's back.
    #[test]
    fn init_true_re_initialises_a_panel_that_was_already_ready() {
        let mut bus = Bus::default();
        let mut d = dev();
        d.call("lcd_text", &json!({ "text": "x" }), &mut bus).expect("first write inits");
        let after_first = bus.log.len();
        d.call("lcd_text", &json!({ "text": "y" }), &mut bus).expect("second write does not");
        let steady = bus.log.len() - after_first;
        assert_eq!(steady, 6 + 6, "address plus one character, no init: got {steady} transfers");

        let before = bus.log.len();
        d.call("lcd_text", &json!({ "text": "z", "init": true }), &mut bus).expect("forced init");
        assert_eq!(bus.log.len() - before, 33 + 12, "init again, then address and character");
    }

    #[test]
    fn the_lcd_tools_report_what_they_did() {
        let mut bus = Bus::default();
        let mut d = dev();
        let v = d.call("lcd_text", &json!({ "text": "hello", "line": 1 }), &mut bus).expect("text");
        assert_eq!(v["chars"], 5);
        assert_eq!(v["line"], 1);
        assert_eq!(d.call("lcd_clear", &json!({}), &mut bus).expect("clear")["ok"], true);
        let v = d.call("lcd_backlight", &json!({ "on": false }), &mut bus).expect("off");
        assert_eq!(v["backlight"], false);
        let err = d.call("lcd_backlight", &json!({}), &mut bus).expect_err("on is required");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
    }
}

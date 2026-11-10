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

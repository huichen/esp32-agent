//! SSD1306 / SH1106 monochrome OLED (128x64 or 128x32).
//!
//! These panels have no identification register, so detection is "something
//! acknowledges 0x3C/0x3D". Everything else — 128x32 vs 128x64, SSD1306 vs the
//! SH1106 that clones it — comes from the board config, because guessing it
//! wrong is the classic reason a panel stays dark.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::font6x8::{FONT6X8, FONT_H, FONT_W};
use crate::hardware::Hardware;
use crate::json as j;
use crate::tool::{boolean, integer, object_schema, string, ToolDef};
use serde_json::{json, Value};

const CMD: u8 = 0x00;
const DATA: u8 = 0x40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Controller {
    Ssd1306,
    Sh1106,
}

pub struct Ssd1306 {
    pub bus: u8,
    pub addr: u8,
    pub width: u8,
    pub height: u8,
    pub controller: Controller,
    fb: Vec<u8>,
    inverted: bool,
    powered: bool,
    contrast: u8,
    id: String,
}

impl Ssd1306 {
    pub const ADDRESSES: &'static [u8] = &[0x3C, 0x3D];

    /// Send the init sequence. Returns `Err` when the panel does not answer,
    /// which is how a probe distinguishes "wired" from "address in use".
    pub fn probe(
        hw: &mut dyn Hardware,
        bus: u8,
        addr: u8,
        height: u8,
        controller: Controller,
        id_suffix: usize,
    ) -> Result<Self> {
        let mut d = Self {
            bus,
            addr,
            width: 128,
            height: if height >= 64 { 64 } else { 32 },
            controller,
            fb: vec![0; 128 * (height as usize / 8).max(4)],
            inverted: false,
            powered: true,
            contrast: 0xCF,
            id: String::new(),
        };
        d.fb = vec![0; d.stride() * d.pages()];
        d.init(hw)?;
        d.id = if id_suffix > 1 { format!("ssd1306_{id_suffix}") } else { "ssd1306".into() };
        Ok(d)
    }

    fn pages(&self) -> usize {
        (self.height as usize / 8).max(1)
    }

    fn stride(&self) -> usize {
        self.width as usize
    }

    pub fn fb_len(&self) -> usize {
        self.fb.len()
    }

    pub fn framebuffer(&self) -> &[u8] {
        &self.fb
    }

    fn cmd(&mut self, hw: &mut dyn Hardware, bytes: &[u8]) -> Result<()> {
        hw.i2c_write(self.bus, self.addr, Some(CMD), bytes)
    }

    fn data(&mut self, hw: &mut dyn Hardware, bytes: &[u8]) -> Result<()> {
        hw.i2c_write(self.bus, self.addr, Some(DATA), bytes)
    }

    fn init(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        let mux = self.height - 1;
        let compins = if self.height >= 64 { 0x12 } else { 0x02 };
        let mut seq: Vec<u8> = vec![
            0xAE, // display off
            0xD5,
            0x80, // clock div
            0xA8,
            mux, // multiplex ratio
            0xD3,
            0x00, // display offset
            0x40, // start line 0
            0x8D,
            0x14, // charge pump on
            0x20,
            0x00, // memory mode: horizontal
            0xA1, // segment remap
            0xC8, // COM scan direction remapped
            0xDA,
            compins, // COM pins
            0x81,
            self.contrast, // contrast
            0xD9,
            0xF1, // pre-charge
            0xDB,
            0x40, // VCOMH
            0xA4, // display follows RAM
            0xA6, // not inverted
            0x2E, // deactivate scroll
            0xAF, // display on
        ];
        if self.controller == Controller::Sh1106 {
            // SH1106 has no charge pump command and uses a 132-column driver.
            seq.retain(|b| *b != 0x8D);
            seq.push(0xAD);
            seq.push(0x8B);
        }
        self.cmd(hw, &seq)?;
        self.clear();
        self.flush(hw)
    }

    pub fn flush(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        let pages = self.pages();
        let stride = self.stride();
        let col_offset: u8 = if self.controller == Controller::Sh1106 { 2 } else { 0 };
        for page in 0..pages {
            self.cmd(hw, &[0xB0 | page as u8, (col_offset & 0x0F), 0x10 | (col_offset >> 4)])?;
            let start = page * stride;
            let end = (start + stride).min(self.fb.len());
            let slice = self.fb[start..end].to_vec();
            self.data(hw, &slice)?;
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        for b in self.fb.iter_mut() {
            *b = 0;
        }
    }

    pub fn set_pixel(&mut self, x: i32, y: i32, on: bool) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = (y as usize / 8) * self.stride() + x as usize;
        if idx >= self.fb.len() {
            return;
        }
        let mask = 1u8 << (y % 8);
        if on {
            self.fb[idx] |= mask;
        } else {
            self.fb[idx] &= !mask;
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, on: bool) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.set_pixel(xx, yy, on);
            }
        }
    }

    /// Draw ASCII text with the built-in 6x8 font. `y` is snapped to a page
    /// boundary (multiple of 8); `wrap` breaks longer strings at the right edge.
    pub fn text(&mut self, x: i32, y: i32, text: &str, wrap: bool) -> i32 {
        let mut cx = x;
        let mut cy = y;
        let glyph_w = FONT_W as i32;
        let glyph_h = FONT_H as i32;
        for ch in text.chars() {
            if ch == '\n' {
                cx = x;
                cy += glyph_h;
                continue;
            }
            if wrap && cx + glyph_w > self.width as i32 {
                cx = 0;
                cy += glyph_h;
            }
            let idx = (ch as u32).saturating_sub(0x20) as usize;
            let glyph = if idx < FONT6X8.len() { &FONT6X8[idx] } else { &FONT6X8[0] };
            let page = (cy / 8).max(0) as usize * self.stride();
            for (col, byte) in glyph.iter().enumerate() {
                let px = cx + col as i32;
                if px < 0 || px >= self.width as i32 {
                    continue;
                }
                let i = page + px as usize;
                if i < self.fb.len() {
                    self.fb[i] |= *byte;
                }
            }
            cx += glyph_w;
        }
        cy + glyph_h
    }

    pub fn text_line(&mut self, line: i32, text: &str) {
        let y = line.clamp(0, self.pages() as i32 - 1) * 8;
        self.text(0, y, text, false);
    }

    /// Copy a 1-bpp page-format bitmap. Accepts a full-panel frame or a 128x32
    /// buffer on a 128x64 panel (drawn into the top half).
    pub fn set_bitmap(&mut self, data: &[u8], w: u32, h: u32) -> bool {
        let want = self.fb.len();
        let src_len = data.len();
        if w == 128 && h == self.height as u32 && src_len == want {
            self.fb.copy_from_slice(data);
            return true;
        }
        if w == 128 && h == 32 && src_len == 512 {
            let n = 512.min(want);
            self.fb[..n].copy_from_slice(&data[..n]);
            return true;
        }
        if src_len == want {
            self.fb.copy_from_slice(data);
            return true;
        }
        false
    }

    pub fn set_contrast(&mut self, hw: &mut dyn Hardware, value: u8) -> Result<()> {
        self.contrast = value;
        self.cmd(hw, &[0x81, value])
    }

    pub fn set_power(&mut self, hw: &mut dyn Hardware, on: bool) -> Result<()> {
        self.powered = on;
        self.cmd(hw, &[if on { 0xAF } else { 0xAE }])
    }

    pub fn set_inverted(&mut self, hw: &mut dyn Hardware, on: bool) -> Result<()> {
        self.inverted = on;
        self.cmd(hw, &[if on { 0xA7 } else { 0xA6 }])
    }

    pub fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "ssd1306",
            &format!(
                "{} {}x{} at 0x{:02X}",
                match self.controller {
                    Controller::Ssd1306 => "SSD1306",
                    Controller::Sh1106 => "SH1106",
                },
                self.width,
                self.height,
                self.addr
            ),
            "i2c",
        );
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["oled".into(), "i2c".into()];
        i.detail = json!({
            "controller": match self.controller { Controller::Ssd1306 => "ssd1306", Controller::Sh1106 => "sh1106" },
            "width": self.width,
            "height": self.height,
            "bus": self.bus,
            "framebuffer_bytes": self.fb.len(),
            "contrast": self.contrast,
        });
        i
    }
}

impl Device for Ssd1306 {
    fn info(&self) -> DeviceInfo {
        Ssd1306::info(self)
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let id_clear = id.clone();
        let id_rect = id.clone();
        let id_bitmap = id.clone();
        let id_contrast = id.clone();
        vec![
            ToolDef::new("oled_text", "Write ASCII text on the OLED using the panel's own 6x8 font")
                .title("OLED text")
                .category("display")
                .requires(&["oled"])
                .schema(object_schema(
                    &[
                        ("text", string("ASCII text, \\n starts a new line")),
                        ("line", integer(0, 7, "0-based text row (8 px tall)")),
                        ("x", integer(0, 127, "left pixel, default 0")),
                        ("clear", boolean("clear the panel first, default true")),
                    ],
                    &["text"],
                ))
                .mutating()
                .bind(move |ctx, args| {
                    let text = j::str_arg(args, "text")?;
                    let line = j::opt_i64(args, "line").unwrap_or(0) as i32;
                    let x = j::opt_i64(args, "x").unwrap_or(0) as i32;
                    let clear = j::opt_bool(args, "clear").unwrap_or(true);
                    ctx.devices.call(&id, "oled_text", &json!({
                        "text": text, "line": line, "x": x, "clear": clear
                    }), ctx.hw)
                }),
            ToolDef::new("oled_clear", "Clear the OLED panel")
                .title("OLED clear")
                .category("display")
                .requires(&["oled"])
                .mutating()
                .bind(move |ctx, args| ctx.devices.call(&id_clear, "oled_clear", args, ctx.hw)),
            ToolDef::new("oled_rect", "Draw a filled or outlined rectangle on the OLED")
                .title("OLED rectangle")
                .category("display")
                .requires(&["oled"])
                .schema(object_schema(
                    &[
                        ("x", integer(0, 127, "left")),
                        ("y", integer(0, 63, "top")),
                        ("w", integer(1, 128, "width")),
                        ("h", integer(1, 64, "height")),
                        ("fill", boolean("fill or outline, default fill")),
                        ("on", boolean("set (default) or clear the pixels")),
                    ],
                    &["x", "y", "w", "h"],
                ))
                .mutating()
                .bind(move |ctx, args| ctx.devices.call(&id_rect, "oled_rect", args, ctx.hw)),
            ToolDef::new("oled_bitmap", "Blit a 1-bit-per-pixel bitmap to the OLED")
                .title("OLED bitmap")
                .category("display")
                .requires(&["oled"])
                .schema(object_schema(
                    &[
                        ("data_b64", string("base64 of 1024 bytes (128x64) or 512 bytes (128x32), page format, bit0 = top row")),
                        ("width", integer(128, 128, "bitmap width, default 128")),
                        ("height", integer(32, 64, "bitmap height, default the panel height")),
                    ],
                    &["data_b64"],
                ))
                .mutating()
                .bind(move |ctx, args| {
                    let b64 = j::str_arg(args, "data_b64")?;
                    let w = j::opt_i64(args, "width").unwrap_or(128) as u32;
                    let h = j::opt_i64(args, "height").unwrap_or(0) as u32;
                    ctx.devices.call(&id_bitmap, "oled_bitmap", &json!({
                        "data_b64": b64, "width": w, "height": h
                    }), ctx.hw)
                }),
            ToolDef::new("oled_contrast", "Set the OLED contrast (brightness)")
                .title("OLED contrast")
                .category("display")
                .requires(&["oled"])
                .schema(object_schema(&[("value", integer(0, 255, "contrast, 0xCF is the usual default"))], &["value"]))
                .mutating()
                .bind(move |ctx, args| ctx.devices.call(&id_contrast, "oled_contrast", args, ctx.hw)),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "oled_text" => {
                let text = j::str_arg(args, "text")?;
                let line = j::ranged_i64(args, "line", 0, 0, 7)? as i32;
                let x = j::ranged_i64(args, "x", 0, 0, 127)? as i32;
                let clear = j::opt_bool(args, "clear").unwrap_or(true);
                if clear {
                    self.clear();
                }
                self.text(x, line * 8, &text, true);
                self.flush(hw)?;
                Ok(json!({ "ok": true, "chars": text.chars().count(), "line": line }))
            }
            "oled_clear" => {
                self.clear();
                self.flush(hw)?;
                Ok(json!({ "ok": true }))
            }
            "oled_rect" => {
                let x = j::ranged_i64(args, "x", 0, 0, 127)? as i32;
                let y = j::ranged_i64(args, "y", 0, 0, 63)? as i32;
                let w = j::ranged_i64(args, "w", 1, 1, 128)? as i32;
                let h = j::ranged_i64(args, "h", 1, 1, 64)? as i32;
                let fill = j::opt_bool(args, "fill").unwrap_or(true);
                let on = j::opt_bool(args, "on").unwrap_or(true);
                if fill {
                    self.fill_rect(x, y, w, h, on);
                } else {
                    for xx in x..x + w {
                        self.set_pixel(xx, y, on);
                        self.set_pixel(xx, y + h - 1, on);
                    }
                    for yy in y..y + h {
                        self.set_pixel(x, yy, on);
                        self.set_pixel(x + w - 1, yy, on);
                    }
                }
                self.flush(hw)?;
                Ok(json!({ "ok": true, "x": x, "y": y, "w": w, "h": h }))
            }
            "oled_bitmap" => {
                let b64 = j::str_arg(args, "data_b64")?;
                let mut bytes = Vec::new();
                crate::util::b64_decode(&b64, &mut bytes)?;
                let w = j::opt_i64(args, "width").unwrap_or(128) as u32;
                let h = if j::opt_i64(args, "height").unwrap_or(0) == 0 {
                    self.height as u32
                } else {
                    j::opt_i64(args, "height").unwrap_or(0) as u32
                };
                if !self.set_bitmap(&bytes, w, h) {
                    return Err(McpError::invalid_params(format!(
                        "bitmap is {} bytes; expected {} for 128x{} or 512 for 128x32",
                        bytes.len(),
                        self.fb.len(),
                        self.height
                    )));
                }
                self.flush(hw)?;
                Ok(json!({ "ok": true, "bytes": bytes.len() }))
            }
            "oled_contrast" => {
                let v = j::ranged_i64(args, "value", 0xCF, 0, 255)? as u8;
                self.set_contrast(hw, v)?;
                Ok(json!({ "ok": true, "contrast": v }))
            }
            other => Err(McpError::not_found(format!("oled has no tool `{other}`"))),
        }
    }
}

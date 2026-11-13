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

/// Internal batched draw, deliberately not a published OLED tool. `media_play` checks
/// the public rectangle/text switches before using it; a frame needs one flush, not an
/// intermediate picture for every rectangle. Generic device dispatch cannot expose it.
pub(crate) const MEDIA_FRAME_TOOL: &str = "__media_frame";
pub(crate) const MAX_FRAME_RECTS: usize = 64;

pub(crate) struct MediaFrame {
    rects: Vec<MediaRect>,
    text: Option<String>,
    line: i32,
}

struct MediaRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    fill: bool,
}

impl MediaFrame {
    /// Validate against the detected panel, not the 64-row public schema. An origin
    /// outside the panel is an error; a legal rectangle crossing its edge is clipped.
    pub(crate) fn parse(args: &Value, width: u8, height: u8) -> Result<Self> {
        if !args.is_object() {
            return Err(McpError::invalid_params("must be an object"));
        }
        let mut rects = Vec::new();
        if let Some(raw) = args.get("rects") {
            let entries =
                raw.as_array().ok_or_else(|| McpError::invalid_params("`rects` must be an array"))?;
            if entries.len() > MAX_FRAME_RECTS {
                return Err(McpError::invalid_params(format!(
                    "`rects` has {} entries; at most {MAX_FRAME_RECTS} are accepted per frame",
                    entries.len()
                )));
            }
            for (index, rect) in entries.iter().enumerate() {
                let field = |name, min, max| {
                    j::required_ranged_i64(rect, name, min, max)
                        .map(|v| v as i32)
                        .map_err(|e| McpError::invalid_params(format!("`rects[{index}]` {}", e.message)))
                };
                let x = field("x", 0, i64::from(width) - 1)?;
                let y = field("y", 0, i64::from(height) - 1)?;
                let w = field("w", 1, i64::from(width))?.min(i32::from(width) - x);
                let h = field("h", 1, i64::from(height))?.min(i32::from(height) - y);
                let fill = match rect.get("fill") {
                    None => true,
                    Some(Value::Bool(fill)) => *fill,
                    Some(_) => {
                        return Err(McpError::invalid_params(format!(
                            "`rects[{index}].fill` must be a boolean"
                        )))
                    }
                };
                rects.push(MediaRect { x, y, w, h, fill });
            }
        }
        let text = match args.get("text") {
            None => None,
            Some(Value::String(text)) if text.is_empty() => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(_) => return Err(McpError::invalid_params("`text` must be a string")),
        };
        let line = j::optional_ranged_i64(args, "line", 0, i64::from(height / 8) - 1)?.unwrap_or(0) as i32;
        if rects.is_empty() && text.is_none() {
            return Err(McpError::invalid_params("needs drawable content: non-empty `rects` or `text`"));
        }
        Ok(Self { rects, text, line })
    }

    pub(crate) fn has_rects(&self) -> bool {
        !self.rects.is_empty()
    }

    pub(crate) fn has_text(&self) -> bool {
        self.text.is_some()
    }

    pub(crate) fn arguments(&self) -> Value {
        let mut args = json!({
            "rects": self.rects.iter().map(|r| json!({
                "x": r.x, "y": r.y, "w": r.w, "h": r.h, "fill": r.fill,
            })).collect::<Vec<_>>(),
            "line": self.line,
        });
        if let Some(text) = &self.text {
            args["text"] = json!(text);
        }
        args
    }
}

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

    /// What the panel is showing, read back out of the frame buffer by matching each 6x8
    /// cell against the font it was drawn with.
    ///
    /// A bitmap is useless to a text model — `board_status` cannot answer "what is on the
    /// screen" and a base64 frame is 684 characters of nothing the model can read. This
    /// turns the pixels back into the text they came from; anything drawn by
    /// `oled_bitmap` or `oled_rect` shows up as spaces or `?`, which is the honest answer.
    pub fn read_text(&self) -> Vec<String> {
        let cols = self.width as usize / FONT_W;
        let mut out = Vec::with_capacity(self.pages());
        for page in 0..self.pages() {
            let mut line = String::with_capacity(cols);
            for col in 0..cols {
                let mut cell = [0u8; FONT_W];
                for (k, byte) in cell.iter_mut().enumerate() {
                    let x = col * FONT_W + k;
                    *byte = self.fb.get(page * self.stride() + x).copied().unwrap_or(0);
                }
                if cell.iter().all(|b| *b == 0) {
                    line.push(' ');
                    continue;
                }
                let ch = FONT6X8
                    .iter()
                    .position(|g| g[..] == cell[..])
                    .map(|i| (i as u32 + 0x20) as u8 as char)
                    .unwrap_or('?');
                line.push(ch);
            }
            out.push(line.trim_end().to_string());
        }
        out
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

    fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, fill: bool, on: bool) {
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
        // Named here as well as in `tools()`: `devices_list` and `/api/devices` are how a
        // person (and the family grouping) finds out what a detected device can do.
        i.tools = vec![
            "oled_text".into(),
            "oled_read".into(),
            "oled_clear".into(),
            "oled_rect".into(),
            "oled_bitmap".into(),
            "oled_contrast".into(),
        ];
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
        let id_read = id.clone();
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
            ToolDef::new(
                "oled_read",
                "Read back what the OLED is showing. The frame buffer is decoded with the same 6x8 font it was drawn with, so text drawn by oled_text comes back as text; pixels drawn by oled_bitmap or oled_rect read as spaces or '?'",
            )
            .title("OLED read")
            .category("display")
            .requires(&["oled"])
            .bind(move |ctx, _args| ctx.devices.call(&id_read, "oled_read", &json!({}), ctx.hw)),
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
            "oled_read" => {
                let lines = self.read_text();
                Ok(json!({
                    "ok": true,
                    "width": self.width,
                    "height": self.height,
                    "lines": lines,
                    "text": lines.join("\n").trim().to_string(),
                }))
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
                self.draw_rect(x, y, w, h, fill, on);
                self.flush(hw)?;
                Ok(json!({ "ok": true, "x": x, "y": y, "w": w, "h": h }))
            }
            MEDIA_FRAME_TOOL => {
                // Re-check before changing the framebuffer even though the media planner
                // already validated the whole call. No raw caller can create a partial
                // parameter failure inside a frame.
                let frame = MediaFrame::parse(args, self.width, self.height)?;
                self.clear();
                for rect in frame.rects {
                    self.draw_rect(rect.x, rect.y, rect.w, rect.h, rect.fill, true);
                }
                if let Some(text) = frame.text {
                    self.text(0, frame.line * 8, &text, true);
                }
                self.flush(hw)?;
                Ok(json!({ "ok": true }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;

    /// The OLED is write-only over I2C: a control byte (0x00 command, 0x40 data) and a
    /// payload. The fake logs both, which is the only way to see the init sequence and the
    /// page-addressed flush the panel needs.
    #[derive(Default)]
    struct Bus {
        log: Vec<(Option<u8>, Vec<u8>)>,
        err: Option<McpError>,
    }

    impl Bus {
        fn commands(&self) -> Vec<Vec<u8>> {
            self.log.iter().filter(|(r, _)| *r == Some(CMD)).map(|(_, d)| d.clone()).collect()
        }

        fn data_writes(&self) -> Vec<Vec<u8>> {
            self.log.iter().filter(|(r, _)| *r == Some(DATA)).map(|(_, d)| d.clone()).collect()
        }
    }

    impl Hardware for Bus {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn i2c_write(&mut self, _bus: u8, _addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
            if let Some(e) = &self.err {
                return Err(e.clone());
            }
            self.log.push((reg, data.to_vec()));
            Ok(())
        }
    }

    fn bare_panel() -> Ssd1306 {
        Ssd1306 {
            bus: 0,
            addr: 0x3c,
            width: 128,
            height: 32,
            controller: Controller::Ssd1306,
            fb: vec![0; 128 * 4],
            inverted: false,
            powered: true,
            contrast: 0xcf,
            id: "ssd1306".into(),
        }
    }

    fn panel(height: u8, controller: Controller) -> Ssd1306 {
        Ssd1306 {
            bus: 0,
            addr: 0x3c,
            width: 128,
            height,
            controller,
            fb: vec![0; 128 * (height as usize / 8)],
            inverted: false,
            powered: true,
            contrast: 0xcf,
            id: "ssd1306".into(),
        }
    }

    /// Reads one pixel out of the page-format frame buffer, the way the panel does.
    fn px(p: &Ssd1306, x: usize, y: usize) -> bool {
        let idx = (y / 8) * 128 + x;
        p.fb.get(idx).map(|b| b & (1 << (y % 8)) != 0).unwrap_or(false)
    }

    /// The init sequence has to match the panel size: the multiplex ratio is height-1 and
    /// the COM pin configuration differs between 128x64 and 128x32. Getting these two bytes
    /// wrong is why a panel stays dark.
    #[test]
    fn the_init_sequence_is_sized_to_the_panel() {
        let mut bus = Bus::default();
        let d = Ssd1306::probe(&mut bus, 0, 0x3C, 64, Controller::Ssd1306, 1).expect("probe");
        assert_eq!((d.width, d.height), (128, 64));
        assert_eq!(d.fb_len(), 128 * 8, "eight pages of 128 bytes");
        let seq = bus.commands();
        let init = &seq[0];
        assert_eq!(init[0], 0xAE, "the panel is switched off first");
        assert_eq!(init[1..3], [0xD5, 0x80], "clock divide");
        assert_eq!(init[3..5], [0xA8, 63], "multiplex ratio is height - 1");
        assert_eq!(init[14..16], [0xDA, 0x12], "COM pins for a 64-row panel");
        assert!(init.contains(&0x8D) && init.contains(&0x14), "charge pump on");
        assert_eq!(*init.last().expect("a last command"), 0xAF, "and on at the end");

        let mut bus = Bus::default();
        let d = Ssd1306::probe(&mut bus, 0, 0x3C, 32, Controller::Ssd1306, 1).expect("probe");
        assert_eq!(d.height, 32, "a 32-row panel is not stretched");
        assert_eq!(d.fb_len(), 512);
        let init = &bus.commands()[0];
        assert_eq!(init[3..5], [0xA8, 31], "mux 31 for 32 rows");
        assert_eq!(init[14..16], [0xDA, 0x02], "COM pins for a 32-row panel");
    }

    /// The SH1106 is a clone with a 132-column driver and no charge-pump command; it also
    /// sits two columns into that wider RAM, which is why every page address is offset.
    #[test]
    fn the_sh1106_gets_its_own_commands_and_a_column_offset() {
        let mut bus = Bus::default();
        let d = Ssd1306::probe(&mut bus, 0, 0x3D, 64, Controller::Sh1106, 1).expect("probe");
        let init = &bus.commands()[0];
        assert!(!init.contains(&0x8D), "the SH1106 has no charge pump command");
        assert!(init.contains(&0xAD) && init.contains(&0x8B), "it has its own DC-DC command");
        // The flush that ends the init addresses page 0 at column 2.
        let cmds = bus.commands();
        let page0 = cmds.iter().find(|c| c[0] == 0xB0).expect("a page address");
        assert_eq!(page0, &[0xB0, 0x02, 0x10], "SH1106 RAM starts at column 2");
        assert_eq!(d.info().detail["controller"], "sh1106");
    }

    /// A flush addresses each page and sends its 128 bytes. A panel that gets the pages in
    /// the wrong order shows the frame buffer shuffled.
    #[test]
    fn a_flush_addresses_every_page_and_sends_its_bytes() {
        let mut bus = Bus::default();
        let mut d = Ssd1306::probe(&mut bus, 0, 0x3C, 64, Controller::Ssd1306, 1).expect("probe");
        bus.log.clear();
        d.set_pixel(0, 9, true); // page 1, first column
        d.flush(&mut bus).expect("flush");
        let pages: Vec<Vec<u8>> = bus.commands();
        assert_eq!(pages.len(), 8, "eight pages");
        for (i, cmd) in pages.iter().enumerate() {
            assert_eq!(cmd, &[0xB0 | i as u8, 0x00, 0x10], "page {i} at column 0");
        }
        let data = bus.data_writes();
        assert_eq!(data.len(), 8);
        assert!(data.iter().all(|d| d.len() == 128), "128 columns per page");
        assert_eq!(data[1][0], 0x02, "the pixel at (0, 9) is bit 1 of page 1");
    }

    #[test]
    fn a_panel_that_does_not_answer_fails_the_probe() {
        let mut bus = Bus { err: Some(McpError::hardware("I2C NAK at 0x3C")), ..Default::default() };
        let err = Ssd1306::probe(&mut bus, 0, 0x3C, 64, Controller::Ssd1306, 1)
            .err()
            .expect("a silent panel is not a panel");
        assert_eq!(err.code, crate::error::code::HARDWARE);
    }

    #[test]
    fn a_second_panel_gets_a_numbered_id() {
        let mut bus = Bus::default();
        let d = Ssd1306::probe(&mut bus, 1, 0x3D, 32, Controller::Ssd1306, 2).expect("probe");
        assert_eq!(d.id, "ssd1306_2");
        assert_eq!(d.bus, 1);
        assert_eq!(d.addr, 0x3D);
    }

    /// Coordinates outside the panel are dropped rather than wrapped: a wrapped pixel would
    /// draw a stray dot on the other side of the screen.
    #[test]
    fn pixels_outside_the_panel_are_ignored() {
        let mut p = bare_panel();
        p.set_pixel(-1, 0, true);
        p.set_pixel(0, -1, true);
        p.set_pixel(128, 0, true);
        p.set_pixel(0, 32, true);
        assert!(p.framebuffer().iter().all(|b| *b == 0), "nothing was drawn");
        p.set_pixel(127, 31, true);
        assert!(px(&p, 127, 31), "the bottom-right corner is inside");
        p.set_pixel(127, 31, false);
        assert!(!px(&p, 127, 31), "and a pixel can be cleared again");
    }

    /// `fill_rect` and the outline path have to agree on what the rectangle's edge is, or a
    /// border drawn as an outline does not line up with the fill it surrounds.
    #[test]
    fn a_filled_and_an_outlined_rectangle_agree_on_their_edges() {
        let mut filled = bare_panel();
        filled.fill_rect(2, 1, 4, 4, true);
        for y in 1..5 {
            for x in 2..6 {
                assert!(px(&filled, x, y), "({x}, {y}) is inside the fill");
            }
        }
        assert!(!px(&filled, 1, 1), "and nothing outside it");
        assert!(!px(&filled, 6, 5));

        let mut outline = bare_panel();
        for xx in 2..6 {
            outline.set_pixel(xx, 1, true);
            outline.set_pixel(xx, 4, true);
        }
        for yy in 1..5 {
            outline.set_pixel(2, yy, true);
            outline.set_pixel(5, yy, true);
        }
        assert!(px(&outline, 2, 1) && px(&outline, 5, 4), "the corners are drawn");
        assert!(!px(&outline, 3, 2), "the middle of an outline is empty");
    }

    /// A newline moves to the next row and resets x; wrap does the same at the right edge.
    /// Both are what makes a long string readable on a 128 px panel.
    #[test]
    fn text_wraps_and_honours_newlines() {
        let mut p = panel(64, Controller::Ssd1306);
        p.text(0, 0, "AB\nCD", false);
        assert_eq!(p.read_text()[0].trim_end(), "AB");
        assert_eq!(p.read_text()[1].trim_end(), "CD");

        p.clear();
        // 21 characters of 6 px does not fit in 128 px, so the 22nd wraps.
        let long = "X".repeat(22);
        p.text(0, 0, &long, true);
        let lines = p.read_text();
        assert_eq!(lines[0].len(), 21, "21 cells of 6 px fill 126 px");
        assert_eq!(lines[1].trim_end(), "X", "the last one wrapped to the next row: {lines:?}");

        p.clear();
        p.text(0, 0, &long, false);
        let lines = p.read_text();
        assert_eq!(lines[0].len(), 21, "without wrap the extra character is clipped, not moved");
        assert_eq!(lines[1].trim_end(), "");
    }

    #[test]
    fn text_lines_are_clamped_to_the_panel() {
        let mut p = bare_panel();
        p.text_line(0, "TOP");
        p.text_line(9, "BOTTOM"); // 4 pages, so 9 clamps to the last one
        let lines = p.read_text();
        assert_eq!(lines[0].trim_end(), "TOP");
        assert_eq!(lines[3].trim_end(), "BOTTOM");
        assert_eq!(lines.len(), 4);
    }

    /// The read-back walks the same font the draw used; a round trip is the only check
    /// that matters, because a mismatch would silently report a screen that is not there.
    #[test]
    fn oled_read_returns_the_text_that_was_drawn() {
        let mut panel = bare_panel();
        panel.text(0, 0, "HELLO", true);
        let lines = panel.read_text();
        assert_eq!(lines.len(), 4, "one line per 8 px page");
        assert_eq!(lines[0].trim_end(), "HELLO", "{lines:?}");
        panel.clear();
        assert!(panel.read_text().iter().all(|l| l.trim().is_empty()), "a cleared panel reads empty");
    }

    /// Pixels that are not a font glyph cannot be read back as text, so they have to be
    /// reported as '?' rather than invented into a character.
    #[test]
    fn a_bitmap_drawn_glyph_that_is_not_text_reads_as_a_question_mark() {
        let mut p = bare_panel();
        // A full column in the first cell is not any glyph in the font.
        for byte in p.fb.iter_mut().take(FONT_W) {
            *byte = 0xFF;
        }
        assert_eq!(p.read_text()[0].chars().next(), Some('?'));
    }

    /// A full frame, a half-height frame and a bare buffer of the right size are all
    /// accepted; anything else is refused instead of drawn as garbage.
    #[test]
    fn bitmaps_are_accepted_only_at_the_sizes_the_panel_can_show() {
        let mut p = panel(64, Controller::Ssd1306);
        let full = vec![0xA5u8; 1024];
        assert!(p.set_bitmap(&full, 128, 64), "a whole frame");
        assert_eq!(p.framebuffer()[0], 0xA5);

        let top = vec![0x3Cu8; 512];
        assert!(p.set_bitmap(&top, 128, 32), "a 128x32 image on a 128x64 panel");
        assert_eq!(p.framebuffer()[511], 0x3C);
        assert_eq!(p.framebuffer()[512], 0xA5, "the bottom half is left as it was");

        assert!(p.set_bitmap(&full, 64, 64), "a buffer the size of the frame is taken as one");
        assert!(!p.set_bitmap(&[0u8; 100], 128, 64), "100 bytes is not a frame");
        // The claimed geometry does not matter when the byte count is exactly a frame: the
        // last fallback accepts it. That is deliberate (the buffer is the authority), and
        // this pins the behaviour so a change has to be a decision.
        assert!(p.set_bitmap(&vec![0u8; 1024], 128, 32), "1024 bytes is a full frame whatever it says");
    }

    #[test]
    fn the_tool_draws_text_and_clears_by_default() {
        let mut bus = Bus::default();
        let mut p = panel(64, Controller::Ssd1306);
        let v = p.call("oled_text", &json!({ "text": "HELLO", "line": 2 }), &mut bus).expect("text");
        assert_eq!(v["chars"], 5);
        assert_eq!(v["line"], 2);
        assert_eq!(p.read_text()[2].trim_end(), "HELLO", "line 2 is page 2");
        assert!(!bus.log.is_empty(), "and it was pushed to the panel");

        p.call("oled_text", &json!({ "text": "AB", "line": 0, "clear": false }), &mut bus).expect("text");
        assert_eq!(p.read_text()[2].trim_end(), "HELLO", "clear:false leaves the other line alone");
        assert_eq!(p.read_text()[0].trim_end(), "AB");

        p.call("oled_text", &json!({ "text": "CD", "line": 3, "clear": true }), &mut bus).expect("text");
        assert_eq!(p.read_text()[2].trim_end(), "", "clear wipes the panel first");
        assert_eq!(p.read_text()[3].trim_end(), "CD");

        let v = p.call("oled_read", &json!({}), &mut bus).expect("read");
        assert_eq!(v["width"], 128);
        assert_eq!(v["height"], 64);
        assert_eq!(v["text"], "CD", "only the non-blank line survives the trim: {v}");
    }

    #[test]
    fn the_tools_refuse_arguments_outside_the_panel() {
        let mut bus = Bus::default();
        let mut p = panel(64, Controller::Ssd1306);
        for args in [
            json!({}),                         // no text
            json!({ "text": "x", "line": 8 }), // past the last page
            json!({ "text": "x", "x": 200 }),  // past the right edge
        ] {
            let err = p.call("oled_text", &args, &mut bus).expect_err("out of range");
            assert_eq!(err.code, crate::error::code::INVALID_PARAMS, "{args}");
        }
        let err = p.call("oled_pixel", &json!({}), &mut bus).expect_err("not a tool");
        assert_eq!(err.code, crate::error::code::NOT_FOUND);
    }

    #[test]
    fn the_rect_tool_draws_a_filled_or_an_outlined_box() {
        let mut bus = Bus::default();
        let mut p = panel(64, Controller::Ssd1306);
        let v = p.call("oled_rect", &json!({ "x": 0, "y": 0, "w": 4, "h": 4 }), &mut bus).expect("fill");
        assert_eq!(v["w"], 4);
        assert!(px(&p, 1, 1), "the default rectangle is filled");

        p.clear();
        p.call("oled_rect", &json!({ "x": 0, "y": 0, "w": 4, "h": 4, "fill": false }), &mut bus)
            .expect("outline");
        assert!(px(&p, 0, 0) && px(&p, 3, 3), "the outline follows its corners");
        assert!(!px(&p, 1, 1), "and is hollow");

        p.call("oled_rect", &json!({ "x": 0, "y": 0, "w": 4, "h": 4, "on": false }), &mut bus)
            .expect("clear pixels");
        assert!(!px(&p, 3, 3), "the same rectangle with on:false clears instead of drawing");

        let err = p
            .call("oled_rect", &json!({ "x": 0, "y": 0, "w": 200, "h": 4 }), &mut bus)
            .expect_err("wider than the panel");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
    }

    #[test]
    fn the_bitmap_tool_decodes_base64_and_reports_the_size() {
        let mut bus = Bus::default();
        let mut p = panel(64, Controller::Ssd1306);
        let data = vec![0xFFu8; 1024];
        let v = p
            .call("oled_bitmap", &json!({ "data_b64": crate::util::b64_encode(&data) }), &mut bus)
            .expect("bitmap");
        assert_eq!(v["bytes"], 1024);
        assert_eq!(p.framebuffer()[0], 0xFF);

        let err = p
            .call("oled_bitmap", &json!({ "data_b64": crate::util::b64_encode(&[0u8; 10]) }), &mut bus)
            .expect_err("ten bytes is not a frame");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
        assert!(err.message.contains("1024"), "the message says what it wanted: {}", err.message);

        // Asking for a 128x32 bitmap on a 128x64 panel draws into the top half.
        let top = vec![0xF0u8; 512];
        let v = p
            .call(
                "oled_bitmap",
                &json!({ "data_b64": crate::util::b64_encode(&top), "height": 32 }),
                &mut bus,
            )
            .expect("a half-height bitmap");
        assert_eq!(v["bytes"], 512);
        assert_eq!(p.framebuffer()[0], 0xF0);
        assert_eq!(p.framebuffer()[512], 0xFF, "the bottom half still holds the full frame");

        // And clearing is a tool of its own.
        let v = p.call("oled_clear", &json!({}), &mut bus).expect("clear");
        assert_eq!(v["ok"], true);
        assert!(p.framebuffer().iter().all(|b| *b == 0), "the panel is blank");

        let err = p.call("oled_bitmap", &json!({ "data_b64": "!!!!" }), &mut bus).expect_err("not base64");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
    }

    /// Contrast is a register write, so the panel has to be told; the driver keeps the value
    /// so the inventory row can report it.
    #[test]
    fn contrast_power_and_inversion_reach_the_panel() {
        let mut bus = Bus::default();
        let mut p = panel(32, Controller::Ssd1306);
        let v = p.call("oled_contrast", &json!({ "value": 0x40 }), &mut bus).expect("contrast");
        assert_eq!(v["contrast"], 0x40);
        assert_eq!(bus.commands().last().expect("a command"), &vec![0x81, 0x40]);
        assert_eq!(p.info().detail["contrast"], 0x40);

        p.set_power(&mut bus, false).expect("off");
        assert_eq!(bus.commands().last().expect("a command"), &vec![0xAE]);
        p.set_power(&mut bus, true).expect("on");
        assert_eq!(bus.commands().last().expect("a command"), &vec![0xAF]);

        p.set_inverted(&mut bus, true).expect("invert");
        assert_eq!(bus.commands().last().expect("a command"), &vec![0xA7]);
        assert!(p.inverted);
        p.set_inverted(&mut bus, false).expect("normal");
        assert_eq!(bus.commands().last().expect("a command"), &vec![0xA6]);

        let err = p.call("oled_contrast", &json!({ "value": 300 }), &mut bus).expect_err("over a byte");
        assert_eq!(err.code, crate::error::code::INVALID_PARAMS);
    }

    /// A write the panel never acknowledges must reach the caller: a silent OLED looks
    /// exactly like a working one from the frame buffer's point of view.
    #[test]
    fn a_failed_write_is_reported_by_every_tool() {
        let mut bus = Bus { err: Some(McpError::hardware("I2C NAK")), ..Default::default() };
        let mut p = panel(64, Controller::Ssd1306);
        for (tool, args) in [
            ("oled_text", json!({ "text": "x" })),
            ("oled_clear", json!({})),
            ("oled_rect", json!({ "x": 0, "y": 0, "w": 1, "h": 1 })),
            ("oled_bitmap", json!({ "data_b64": crate::util::b64_encode(&vec![0u8; 1024]) })),
            ("oled_contrast", json!({ "value": 1 })),
        ] {
            let err = p.call(tool, &args, &mut bus).expect_err(tool);
            assert_eq!(err.code, crate::error::code::HARDWARE, "{tool}");
        }
    }

    #[test]
    fn the_inventory_row_names_the_controller_and_the_geometry() {
        let p = panel(64, Controller::Sh1106);
        let info = p.info();
        assert_eq!(info.kind, "ssd1306", "the driver kind does not change with the controller");
        assert_eq!(info.bus, "i2c");
        assert_eq!(info.address, Some(0x3C));
        assert!(info.name.contains("SH1106") && info.name.contains("128x64"), "{}", info.name);
        assert!(info.caps.contains(&"oled".to_string()));
        assert_eq!(info.detail["width"], 128);
        assert_eq!(info.detail["height"], 64);
        assert_eq!(info.detail["framebuffer_bytes"], 1024);
        assert_eq!(
            p.tools().iter().map(|t| t.spec.name.clone()).collect::<Vec<_>>(),
            vec!["oled_text", "oled_read", "oled_clear", "oled_rect", "oled_bitmap", "oled_contrast"]
        );
    }
}

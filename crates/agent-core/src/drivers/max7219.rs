//! MAX7219 / MAX7221 8x8 LED matrix, daisy-chainable, on SPI plus one GPIO.
//!
//! The part has no read-back: DOUT only repeats what was shifted in, for the chip
//! behind it in a chain. There is therefore nothing to detect, and presence comes
//! from the manifest or the board profile — the same rule as a servo or a relay.
//! [`Max7219::identify`] can only answer "this board has an SPI bus", and it says
//! so rather than pretending to have found a chip.
//!
//! Every frame is 16 bits per chip (register, then data), sent with chip select
//! held low for the whole refresh. In a chain the farthest chip is clocked in
//! first, because each word shifts through every chip ahead of it.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::font6x8::{FONT6X8, FONT_H, FONT_W};
use crate::hardware::{Hardware, PinMode};
use crate::json as j;
use crate::tool::{integer, object_schema, string, ToolDef};
use serde_json::{json, Value};

const REG_DIGIT0: u8 = 0x01;
const REG_DECODE: u8 = 0x09;
const REG_INTENSITY: u8 = 0x0A;
const REG_SCAN: u8 = 0x0B;
const REG_SHUTDOWN: u8 = 0x0C;
const REG_DISPLAY_TEST: u8 = 0x0F;

/// Pixels per chip in one axis.
const SIDE: usize = 8;

pub struct Max7219 {
    pub bus: u8,
    pub cs: i32,
    /// Chips in the chain; chip 0 is the one nearest the SPI master.
    count: usize,
    intensity: u8,
    /// One 8x8 frame per chip, row 0 at the top, bit 7 the leftmost pixel.
    fb: Vec<[u8; SIDE]>,
    id: String,
}

impl Max7219 {
    /// Whether this board can shift a frame out at all. A MAX7219 cannot answer,
    /// so this is a bus check and nothing more.
    pub fn identify(hw: &mut dyn Hardware, bus: u8, cs: i32) -> bool {
        if hw.gpio_config(cs, PinMode::Output).is_err() {
            return false;
        }
        if hw.gpio_write(cs, false).is_err() {
            return false;
        }
        // Register 0x00 is a no-op on every MAX7219, so this cannot light a pixel
        // even when the chain is real.
        let sent = hw.spi_transfer(bus, &[0x00, 0x00]);
        let released = hw.gpio_write(cs, true);
        matches!((sent, released), (Ok(rx), Ok(())) if rx.len() == 2)
    }

    pub fn probe(hw: &mut dyn Hardware, bus: u8, cs: i32, count: usize, id_suffix: usize) -> Result<Self> {
        if !Self::identify(hw, bus, cs) {
            return Err(McpError::not_found(format!(
                "no SPI bus for a MAX7219 on CS GPIO{cs}: the shift-out failed"
            )));
        }
        let count = count.clamp(1, 8);
        let mut d = Self {
            bus,
            cs,
            count,
            intensity: 8,
            fb: vec![[0u8; SIDE]; count],
            id: if id_suffix > 1 { format!("max7219_{id_suffix}") } else { "max7219".into() },
        };
        d.init(hw)?;
        Ok(d)
    }

    /// The datasheet's startup sequence. No decode mode: the buffer holds pixels,
    /// not digits.
    fn init(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        self.send_word(hw, REG_DISPLAY_TEST, 0x00)?;
        self.send_word(hw, REG_DECODE, 0x00)?;
        self.send_word(hw, REG_SCAN, (SIDE - 1) as u8)?;
        self.send_word(hw, REG_INTENSITY, self.intensity)?;
        self.send_word(hw, REG_SHUTDOWN, 0x01)?;
        // The chip's own RAM is undefined after power-up, so clear it explicitly.
        for row in self.fb.iter_mut() {
            *row = [0u8; SIDE];
        }
        self.flush(hw)
    }

    /// One 16-bit word in its own chip-select window, which every chip in the
    /// chain receives (broadcast).
    fn send_word(&mut self, hw: &mut dyn Hardware, reg: u8, data: u8) -> Result<()> {
        hw.gpio_config(self.cs, PinMode::Output)?;
        hw.gpio_write(self.cs, false)?;
        let rx = hw.spi_transfer(self.bus, &[reg, data]);
        // Release chip select even when the transfer failed, or the next frame
        // would be appended to the wreckage of this one.
        hw.gpio_write(self.cs, true)?;
        check_len(rx?, 2)
    }

    /// Push the whole frame buffer out in one chip-select window.
    pub fn flush(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        hw.gpio_config(self.cs, PinMode::Output)?;
        hw.gpio_write(self.cs, false)?;
        let mut frame = Vec::with_capacity(self.count * SIDE * 2);
        for row in 0..SIDE {
            for chip in (0..self.count).rev() {
                frame.push(REG_DIGIT0 + row as u8);
                frame.push(self.fb[chip][row]);
            }
        }
        let rx = hw.spi_transfer(self.bus, &frame);
        hw.gpio_write(self.cs, true)?;
        check_len(rx?, frame.len())
    }

    pub fn set_row(&mut self, hw: &mut dyn Hardware, chip: usize, row: usize, bits: u8) -> Result<()> {
        if chip >= self.count {
            return Err(McpError::invalid_params(format!(
                "this chain has {} chip(s); chip {chip} does not exist",
                self.count
            )));
        }
        if row >= SIDE {
            return Err(McpError::invalid_params(format!("row must be 0..={}, got {row}", SIDE - 1)));
        }
        self.fb[chip][row] = bits;
        self.flush(hw)
    }

    /// Replace the buffer with `8 * count` row bytes, left chip first.
    pub fn set_bitmap(&mut self, data: &[u8]) -> Result<usize> {
        let want = self.count * SIDE;
        if data.len() != want {
            return Err(McpError::invalid_params(format!(
                "a bitmap for {} chip(s) is {want} bytes (8 rows each); got {}",
                self.count,
                data.len()
            )));
        }
        for (chip, chunk) in data.chunks(SIDE).enumerate() {
            self.fb[chip].copy_from_slice(chunk);
        }
        Ok(want)
    }

    pub fn clear(&mut self) {
        for row in self.fb.iter_mut() {
            *row = [0u8; SIDE];
        }
    }

    pub fn set_pixel(&mut self, x: i32, y: i32, on: bool) {
        let width = (self.count * SIDE) as i32;
        if x < 0 || y < 0 || x >= width || y >= SIDE as i32 {
            return;
        }
        let chip = x as usize / SIDE;
        let bit = 7 - (x as usize % SIDE);
        let row = y as usize;
        if on {
            self.fb[chip][row] |= 1 << bit;
        } else {
            self.fb[chip][row] &= !(1 << bit);
        }
    }

    pub fn width(&self) -> i32 {
        (self.count * SIDE) as i32
    }

    /// Draw ASCII with the 6x8 font the OLED driver uses, at `x` pixels from the
    /// left of the chain. A glyph is 8 rows tall, so it fills the matrix height.
    pub fn text(&mut self, x: i32, text: &str) -> usize {
        let mut cx = x;
        let mut drawn = 0usize;
        for ch in text.chars() {
            let idx = (ch as u32).saturating_sub(FONT6X8_FIRST) as usize;
            let glyph = FONT6X8.get(idx).unwrap_or(&FONT6X8[0]);
            for (col, byte) in glyph.iter().enumerate() {
                for bit in 0..FONT_H {
                    if byte & (1 << bit) != 0 {
                        self.set_pixel(cx + col as i32, bit as i32, true);
                    }
                }
            }
            cx += FONT_W as i32;
            drawn += 1;
        }
        drawn
    }

    pub fn set_intensity(&mut self, hw: &mut dyn Hardware, level: u8) -> Result<()> {
        self.intensity = level.min(15);
        self.send_word(hw, REG_INTENSITY, self.intensity)
    }
}

fn check_len(rx: Vec<u8>, sent: usize) -> Result<()> {
    if rx.len() != sent {
        return Err(McpError::hardware(format!("SPI returned {} byte(s) for the {sent} sent", rx.len())));
    }
    Ok(())
}

/// `FONT6X8` starts at the space character.
const FONT6X8_FIRST: u32 = 0x20;

impl Device for Max7219 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "max7219",
            &format!("MAX7219 8x8 matrix x{} on SPI{} CS GPIO{}", self.count, self.bus, self.cs),
            "spi",
        );
        i.id = self.id.clone();
        i.pins = vec![self.cs];
        i.caps = vec!["max7219".into(), "matrix".into(), "spi".into()];
        i.source = "config".into();
        i.detail = json!({
            "chips": self.count,
            "width": self.width(),
            "height": SIDE,
            "intensity": self.intensity,
            "bus": self.bus,
        });
        i.tools = vec![
            "max7219_bitmap".into(),
            "max7219_row".into(),
            "max7219_text".into(),
            "max7219_intensity".into(),
        ];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let row_id = id.clone();
        let text_id = id.clone();
        let intensity_id = id.clone();
        vec![
            ToolDef::new(
                "max7219_bitmap",
                "Draw a bitmap on the LED matrix chain: eight row bytes per chip, left chip first, bit 7 is the leftmost pixel",
            )
            .title("Matrix bitmap")
            .category("display")
            .requires(&["max7219"])
            .schema(object_schema(
                &[
                    ("data", json!({ "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 255 }, "description": "8 bytes per chip, top row first" })),
                    ("hex", string("the same bytes as hex, spaces and commas allowed")),
                ],
                &[],
            ))
            .mutating()
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "max7219_bitmap", args, ctx.hw)
            }),
            ToolDef::new("max7219_row", "Set one 8-pixel row of one chip in the chain")
                .title("Matrix row")
                .category("display")
                .requires(&["max7219"])
                .schema(object_schema(
                    &[
                        ("row", integer(0, 7, "0 = top row")),
                        ("bits", integer(0, 255, "8 pixels, bit 7 leftmost")),
                        ("chip", integer(0, 7, "position in the chain, default 0")),
                    ],
                    &["row", "bits"],
                ))
                .mutating()
                .bind(move |ctx, args| {
                    let id = row_id.clone();
                    ctx.devices.call(&id, "max7219_row", args, ctx.hw)
                }),
            ToolDef::new(
                "max7219_text",
                "Clear the matrix and write text with the built-in 6x8 font; scroll by asking for another x",
            )
            .title("Matrix text")
            .category("display")
            .requires(&["max7219"])
            .schema(object_schema(
                &[
                    ("text", string("ASCII text, one 6-pixel character cell per glyph")),
                    ("x", integer(-64, 63, "left pixel of the first glyph, default 0")),
                ],
                &["text"],
            ))
            .mutating()
            .bind(move |ctx, args| {
                let text = j::str_arg(args, "text")?;
                let x = j::opt_i64(args, "x").unwrap_or(0) as i32;
                ctx.devices.call(&text_id, "max7219_text", &json!({ "text": text, "x": x }), ctx.hw)
            }),
            ToolDef::new("max7219_intensity", "Set the LED matrix brightness, 0 (dimmest) to 15")
                .title("Matrix brightness")
                .category("display")
                .requires(&["max7219"])
                .schema(object_schema(&[("level", integer(0, 15, "intensity register value"))], &["level"]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = intensity_id.clone();
                    ctx.devices.call(&id, "max7219_intensity", args, ctx.hw)
                }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "max7219_bitmap" => {
                let data = parse_bytes(args)?;
                let n = self.set_bitmap(&data)?;
                self.flush(hw)?;
                Ok(json!({ "ok": true, "bytes": n, "chips": self.count }))
            }
            "max7219_row" => {
                let row = j::ranged_i64(args, "row", 0, 0, 7)? as usize;
                let bits = j::ranged_i64(args, "bits", 0, 0, 255)? as u8;
                let chip = j::ranged_i64(args, "chip", 0, 0, 7)? as usize;
                self.set_row(hw, chip, row, bits)?;
                Ok(json!({ "ok": true, "chip": chip, "row": row, "bits": bits }))
            }
            "max7219_text" => {
                let text = j::str_arg(args, "text")?;
                let x = j::ranged_i64(args, "x", 0, -64, 63)? as i32;
                self.clear();
                let chars = self.text(x, &text);
                self.flush(hw)?;
                Ok(json!({
                    "ok": true,
                    "chars": chars,
                    "x": x,
                    "width": self.width(),
                    "note": if (x + (chars as i32) * FONT_W as i32) > self.width() {
                        "the text runs off the right edge; call again with a smaller x to scroll"
                    } else {
                        "drawn"
                    },
                }))
            }
            "max7219_intensity" => {
                let level = j::ranged_i64(args, "level", 8, 0, 15)? as u8;
                self.set_intensity(hw, level)?;
                Ok(json!({ "ok": true, "intensity": self.intensity }))
            }
            other => Err(McpError::not_found(format!("max7219 has no tool `{other}`"))),
        }
    }
}

/// Row bytes from either an array or a hex string, so a model can paste what a
/// tool returned without converting it.
fn parse_bytes(args: &Value) -> Result<Vec<u8>> {
    if let Some(values) = j::arr(args, "data") {
        return Ok(values.iter().map(|v| v.as_u64().unwrap_or(0).min(255) as u8).collect());
    }
    let text = j::opt_str(args, "hex").or_else(|| j::opt_str(args, "data"));
    let Some(text) = text else {
        return Err(McpError::invalid_params("send `data` (array of bytes) or `hex`"));
    };
    let digits: Vec<u8> =
        text.chars().filter(|c| c.is_ascii_hexdigit()).map(|c| c.to_digit(16).unwrap_or(0) as u8).collect();
    if digits.is_empty() || digits.len() % 2 != 0 {
        return Err(McpError::invalid_params("`hex` must hold an even number of hex digits"));
    }
    Ok(digits.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;

    const CS: i32 = 10;

    /// An SPI bus with a chip-select trace: what matters for the MAX7219 is the
    /// exact byte stream and when CS rises and falls.
    struct FakeSpi {
        cs: Vec<bool>,
        transfers: Vec<Vec<u8>>,
        /// What the chain shifts back; the driver must not depend on it.
        reply: u8,
        fail: bool,
    }

    impl FakeSpi {
        fn working() -> Self {
            Self { cs: Vec::new(), transfers: Vec::new(), reply: 0x00, fail: false }
        }

        fn without_bus() -> Self {
            Self { fail: true, ..Self::working() }
        }
    }

    impl Hardware for FakeSpi {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, _pin: i32, _mode: PinMode) -> Result<()> {
            Ok(())
        }

        fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
            if pin == CS {
                self.cs.push(high);
            }
            Ok(())
        }

        fn spi_transfer(&mut self, _bus: u8, data: &[u8]) -> Result<Vec<u8>> {
            if self.fail {
                return Err(McpError::unsupported("SPI is not available on this board"));
            }
            self.transfers.push(data.to_vec());
            Ok(vec![self.reply; data.len()])
        }
    }

    fn driver(count: usize) -> Max7219 {
        Max7219 { bus: 0, cs: CS, count, intensity: 8, fb: vec![[0u8; SIDE]; count], id: "max7219".into() }
    }

    #[test]
    fn the_startup_sequence_is_the_datasheet_one() {
        let mut hw = FakeSpi::working();
        let m = Max7219::probe(&mut hw, 0, CS, 1, 1);
        assert!(m.is_ok(), "a working SPI bus is enough to bring the part up");
        assert_eq!(hw.transfers[0], vec![0x00, 0x00], "the probe's no-op word comes first");
        assert_eq!(
            hw.transfers[1..6].to_vec(),
            vec![
                vec![REG_DISPLAY_TEST, 0x00],
                vec![REG_DECODE, 0x00],
                vec![REG_SCAN, 0x07],
                vec![REG_INTENSITY, 8],
                vec![REG_SHUTDOWN, 0x01],
            ]
        );
        // Then the undefined power-up RAM is cleared: eight zero rows, one refresh.
        assert_eq!(
            hw.transfers[6],
            vec![1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 8, 0],
            "row registers 0x01..0x08, all pixels off"
        );
        assert_eq!(hw.transfers.len(), 7);
        assert_eq!(hw.cs.first(), Some(&false), "CS is asserted first");
        assert_eq!(hw.cs.last(), Some(&true), "and released last");
    }

    #[test]
    fn a_row_write_is_register_plus_pixel_byte() {
        let mut hw = FakeSpi::working();
        let mut d = driver(1);
        d.set_row(&mut hw, 0, 0, 0xFF).unwrap();
        assert_eq!(hw.transfers.len(), 1);
        let sent = &hw.transfers[0];
        assert_eq!(sent.len(), 16, "eight rows in one transfer");
        assert_eq!(
            sent,
            &vec![
                0x01, 0xFF, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x00, 0x06, 0x00, 0x07, 0x00, 0x08,
                0x00
            ],
            "row 0 is register 0x01, and the rest are cleared rows"
        );
        assert_eq!(hw.cs, vec![false, true], "one CS window for the whole refresh");
    }

    #[test]
    fn a_chain_is_clocked_farthest_chip_first() {
        let mut hw = FakeSpi::working();
        let mut d = driver(2);
        d.fb[0][0] = 0x11;
        d.fb[1][0] = 0x22;
        d.flush(&mut hw).unwrap();
        let sent = &hw.transfers[0];
        assert_eq!(&sent[..4], &[0x01, 0x22, 0x01, 0x11], "chip 1 shifts in first");
        assert_eq!(sent.len(), 32, "8 rows x 2 chips x 2 bytes");
    }

    #[test]
    fn text_uses_the_same_font_as_the_oled() {
        let mut hw = FakeSpi::working();
        let mut d = driver(1);
        assert_eq!(d.text(0, "A"), 1);
        // FONT6X8['A'] = 7C 12 11 12 7C 00, columns with bit 0 at the top.
        assert_eq!(d.fb[0], [0x20, 0x50, 0x88, 0x88, 0xF8, 0x88, 0x88, 0x00]);
        d.flush(&mut hw).unwrap();
        assert_eq!(hw.transfers[0][0..2].to_vec(), vec![0x01, 0x20]);
        // A glyph pushed off the left edge is clipped, not wrapped. One whole cell off, so
        // no column of it lands on the panel.
        d.clear();
        d.text(-6, "A");
        assert_eq!(d.fb[0], [0x00; 8]);
        // Same for the right edge of a single chip: only the first two columns fit
        // into pixels 6 and 7.
        d.clear();
        d.text(6, "A");
        assert_eq!(d.fb[0], [0x00, 0x01, 0x02, 0x02, 0x03, 0x02, 0x02, 0x00]);
    }

    #[test]
    fn a_bitmap_of_the_wrong_size_is_refused() {
        let mut hw = FakeSpi::working();
        let mut d = driver(1);
        let e = d.call("max7219_bitmap", &json!({ "data": [1, 2, 3] }), &mut hw).unwrap_err();
        assert_eq!(e.code, crate::error::code::INVALID_PARAMS);
        assert!(e.message.contains("8 rows"), "{}", e.message);
        let out = d.call("max7219_bitmap", &json!({ "hex": "00,01 02 0304050607" }), &mut hw).unwrap();
        assert_eq!(out["bytes"], 8);
        assert_eq!(d.fb[0], [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
        let e = d.call("max7219_bitmap", &json!({ "hex": "001" }), &mut hw).unwrap_err();
        assert!(e.message.contains("even"), "{}", e.message);
    }

    #[test]
    fn without_an_spi_bus_nothing_is_detected() {
        let mut hw = FakeSpi::without_bus();
        assert!(!Max7219::identify(&mut hw, 0, CS));
        let e = Max7219::probe(&mut hw, 0, CS, 1, 1).err().expect("the probe must fail");
        assert!(e.message.contains("no SPI bus"), "{}", e.message);
        assert!(hw.transfers.is_empty());
    }

    #[test]
    fn intensity_and_row_ranges_are_checked() {
        let mut hw = FakeSpi::working();
        let mut d = driver(1);
        let out = d.call("max7219_intensity", &json!({ "level": 3 }), &mut hw).unwrap();
        assert_eq!(out["intensity"], 3);
        assert_eq!(hw.transfers[0], vec![REG_INTENSITY, 3]);
        assert!(d.call("max7219_intensity", &json!({ "level": 16 }), &mut hw).is_err());
        let e = d.call("max7219_row", &json!({ "row": 9, "bits": 1 }), &mut hw).unwrap_err();
        assert_eq!(e.code, crate::error::code::INVALID_PARAMS);
        let e = d.call("max7219_row", &json!({ "row": 0, "bits": 1, "chip": 5 }), &mut hw).unwrap_err();
        assert!(e.message.contains("chip 5"), "{}", e.message);
        let e = d.call("max7219_spin", &json!({}), &mut hw).unwrap_err();
        assert_eq!(e.code, crate::error::code::NOT_FOUND);
    }
}

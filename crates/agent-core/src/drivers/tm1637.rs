//! TM1637 four-digit 7-segment display on two GPIOs: CLK and DIO.
//!
//! Not I2C, although it looks like it: the TM1637 has its own two-wire protocol
//! (a start condition is DIO falling while CLK is high, bits go out LSB first and
//! the ninth clock is the acknowledge). That acknowledge is also the only thing
//! that can be probed — the part has no id register — so [`Tm1637::probe`]
//! reports "no device" when the module never pulls DIO low.
//!
//! The frame is the datasheet's three transfers: a data command that selects
//! automatic address increment, the four digit bytes starting at 0xC0, and a
//! display-control byte carrying the brightness.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::json as j;
use crate::tool::{enumeration, integer, number, object_schema, string, ToolDef};
use serde_json::{json, Value};

const CMD_DATA_AUTO: u8 = 0x40;
const CMD_ADDRESS_0: u8 = 0xC0;
const CMD_DISPLAY_ON: u8 = 0x88;

/// 0-9, segment bits `abcdefg` in bit 0..6, bit 7 is the decimal point.
const SEG_DIGITS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];
/// A..F, the letters a 7-segment cell can actually draw.
const SEG_LETTERS: [u8; 6] = [0x77, 0x7C, 0x39, 0x5E, 0x79, 0x71];
const SEG_MINUS: u8 = 0x40;
const SEG_BLANK: u8 = 0x00;
const SEG_DOT: u8 = 0x80;

/// One half of a bit period. The TM1637 timing table allows a half period down to
/// about 0.4 us; 5 us is the ~100 kHz clock every library uses and is still an
/// order of magnitude inside the part's limit.
// TODO(verify): timing on hardware.
fn bit_delay(hw: &mut dyn Hardware) {
    hw.delay_us(5);
}

pub struct Tm1637 {
    pub clk: i32,
    pub dio: i32,
    brightness: u8,
    segments: [u8; 4],
    id: String,
}

impl Tm1637 {
    pub fn new(clk: i32, dio: i32) -> Self {
        Self { clk, dio, brightness: 7, segments: [SEG_BLANK; 4], id: format!("tm1637_p{clk}") }
    }

    /// The module acknowledges a byte on the ninth clock, which is the only
    /// handshake available: `None` when nothing answered.
    pub fn identify(hw: &mut dyn Hardware, clk: i32, dio: i32) -> Option<()> {
        let dev = Self::new(clk, dio);
        dev.idle(hw).ok()?;
        // The data command changes nothing on screen, so a probe is not visible.
        match Self::send(hw, clk, dio, &[CMD_DATA_AUTO]) {
            Ok(true) => Some(()),
            _ => None,
        }
    }

    pub fn probe(hw: &mut dyn Hardware, clk: i32, dio: i32, id_suffix: usize) -> Result<Self> {
        Self::identify(hw, clk, dio).ok_or_else(|| {
            McpError::not_found(format!(
                "no TM1637 on CLK GPIO{clk} / DIO GPIO{dio}: the module did not acknowledge"
            ))
        })?;
        let mut dev = Self::new(clk, dio);
        dev.show(hw, [SEG_BLANK; 4])?;
        if id_suffix > 1 {
            dev.id = format!("tm1637_{id_suffix}");
        }
        Ok(dev)
    }

    fn idle(&self, hw: &mut dyn Hardware) -> Result<()> {
        hw.gpio_config(self.clk, PinMode::Output)?;
        hw.gpio_config(self.dio, PinMode::Output)?;
        hw.gpio_write(self.clk, true)?;
        hw.gpio_write(self.dio, true)?;
        Ok(())
    }

    fn start(hw: &mut dyn Hardware, clk: i32, dio: i32) -> Result<()> {
        hw.gpio_config(dio, PinMode::Output)?;
        hw.gpio_write(dio, true)?;
        hw.gpio_write(clk, true)?;
        bit_delay(hw);
        // Start: DIO falls while CLK is high.
        hw.gpio_write(dio, false)?;
        bit_delay(hw);
        Ok(())
    }

    fn stop(hw: &mut dyn Hardware, clk: i32, dio: i32) -> Result<()> {
        hw.gpio_write(clk, false)?;
        hw.gpio_write(dio, false)?;
        bit_delay(hw);
        hw.gpio_write(clk, true)?;
        bit_delay(hw);
        // Stop: DIO rises while CLK is high.
        hw.gpio_write(dio, true)?;
        bit_delay(hw);
        Ok(())
    }

    /// One byte, LSB first, followed by the ninth clock that carries the
    /// acknowledge. `false` means the module did not answer.
    fn write_byte(hw: &mut dyn Hardware, clk: i32, dio: i32, byte: u8) -> Result<bool> {
        for i in 0..8 {
            hw.gpio_write(clk, false)?;
            hw.gpio_write(dio, (byte >> i) & 1 == 1)?;
            bit_delay(hw);
            hw.gpio_write(clk, true)?;
            bit_delay(hw);
        }
        // Ninth clock: release the line so the display can pull it low.
        hw.gpio_write(clk, false)?;
        hw.gpio_config(dio, PinMode::InputPullup)?;
        bit_delay(hw);
        hw.gpio_write(clk, true)?;
        bit_delay(hw);
        let acked = !hw.gpio_read(dio)?;
        hw.gpio_write(clk, false)?;
        hw.gpio_config(dio, PinMode::Output)?;
        bit_delay(hw);
        Ok(acked)
    }

    /// One start..stop frame. Stops early on a missing acknowledge, but always
    /// leaves the bus idle.
    fn send(hw: &mut dyn Hardware, clk: i32, dio: i32, bytes: &[u8]) -> Result<bool> {
        Self::start(hw, clk, dio)?;
        let mut acked = true;
        for byte in bytes {
            if !Self::write_byte(hw, clk, dio, *byte)? {
                acked = false;
                break;
            }
        }
        Self::stop(hw, clk, dio)?;
        Ok(acked)
    }

    fn transmit(&self, hw: &mut dyn Hardware, bytes: &[u8]) -> Result<()> {
        if !Self::send(hw, self.clk, self.dio, bytes)? {
            return Err(McpError::hardware(format!(
                "TM1637 on CLK GPIO{} / DIO GPIO{} stopped acknowledging",
                self.clk, self.dio
            )));
        }
        Ok(())
    }

    /// Write all four digits: the data command, the four bytes, then the
    /// display-control byte that also sets the brightness.
    pub fn show(&mut self, hw: &mut dyn Hardware, segments: [u8; 4]) -> Result<()> {
        self.idle(hw)?;
        self.transmit(hw, &[CMD_DATA_AUTO])?;
        let mut frame = vec![CMD_ADDRESS_0];
        frame.extend_from_slice(&segments);
        self.transmit(hw, &frame)?;
        self.segments = segments;
        self.transmit(hw, &[CMD_DISPLAY_ON | (self.brightness & 0x07)])
    }

    pub fn set_brightness(&mut self, hw: &mut dyn Hardware, level: u8) -> Result<()> {
        self.brightness = level.min(7);
        self.idle(hw)?;
        self.transmit(hw, &[CMD_DISPLAY_ON | self.brightness])
    }

    pub fn segments(&self) -> [u8; 4] {
        self.segments
    }
}

impl Device for Tm1637 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "tm1637",
            &format!("TM1637 four-digit display (CLK GPIO{}, DIO GPIO{})", self.clk, self.dio),
            "gpio",
        );
        i.id = self.id.clone();
        i.pins = vec![self.clk, self.dio];
        i.caps = vec!["tm1637".into(), "sevenseg".into(), "gpio".into()];
        i.source = "config".into();
        i.detail = json!({
            "brightness": self.brightness,
            "segments": self.segments.iter().map(|b| format!("0x{b:02X}")).collect::<Vec<String>>(),
        });
        i.tools = vec!["tm1637_show".into(), "tm1637_brightness".into(), "tm1637_clear".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let bright = id.clone();
        let clear = id.clone();
        vec![
            ToolDef::new(
                "tm1637_show",
                "Show up to four characters on the TM1637 display. Send `text` or a `value` (which is right-aligned and gets a decimal point from `decimals`)",
            )
            .title("Display text")
            .category("display")
            .requires(&["tm1637"])
            .schema(object_schema(
                &[
                    ("text", string("0-9, A-F, '-', '.' for the decimal point, ':' for the colon")),
                    ("value", number(-999.0, 9999.0, "a number to show instead of text")),
                    ("decimals", integer(0, 3, "digits after the point for `value`, default 0")),
                    ("align", enumeration(&["left", "right"], "default left for text, right for a value")),
                ],
                &[],
            ))
            .mutating()
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "tm1637_show", args, ctx.hw)
            }),
            ToolDef::new("tm1637_brightness", "Set the TM1637 brightness, 0 (dimmest) to 7")
                .title("Display brightness")
                .category("display")
                .requires(&["tm1637"])
                .schema(object_schema(&[("level", integer(0, 7, "brightness level"))], &["level"]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = bright.clone();
                    ctx.devices.call(&id, "tm1637_brightness", args, ctx.hw)
                }),
            ToolDef::new("tm1637_clear", "Blank all four digits of the TM1637")
                .title("Display clear")
                .category("display")
                .requires(&["tm1637"])
                .schema(object_schema(&[], &[]))
                .mutating()
                .bind(move |ctx, args| {
                    let id = clear.clone();
                    ctx.devices.call(&id, "tm1637_clear", args, ctx.hw)
                }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "tm1637_show" => {
                let value = j::opt_f64(args, "value");
                let text = j::opt_str(args, "text");
                if value.is_none() && text.is_none() {
                    return Err(McpError::invalid_params("send `text` or `value`"));
                }
                let (text, default_right) = match (text, value) {
                    (Some(t), _) => (t, false),
                    (None, Some(v)) => {
                        let decimals = j::ranged_i64(args, "decimals", 0, 0, 3)? as usize;
                        (format!("{v:.decimals$}"), true)
                    }
                    (None, None) => unreachable!("checked above"),
                };
                let right = match j::opt_str(args, "align").as_deref() {
                    Some("right") => true,
                    Some("left") => false,
                    Some(other) => {
                        return Err(McpError::invalid_params(format!(
                            "`align` must be left or right, got `{other}`"
                        )))
                    }
                    None => default_right,
                };
                let cells = digit_cells(&text);
                if cells > 4 {
                    return Err(McpError::invalid_params(format!(
                        "`{text}` needs {cells} digits; the display has four"
                    )));
                }
                let padded = if right { format!("{}{text}", " ".repeat(4 - cells)) } else { text.clone() };
                let segments = encode(&padded);
                self.show(hw, segments)?;
                Ok(json!({
                    "ok": true,
                    "text": padded,
                    "segments": segments.iter().map(|b| format!("0x{b:02X}")).collect::<Vec<String>>(),
                    "brightness": self.brightness,
                }))
            }
            "tm1637_brightness" => {
                let level = j::ranged_i64(args, "level", 7, 0, 7)? as u8;
                self.set_brightness(hw, level)?;
                Ok(json!({ "ok": true, "brightness": self.brightness }))
            }
            "tm1637_clear" => {
                self.show(hw, [SEG_BLANK; 4])?;
                Ok(json!({ "ok": true, "brightness": self.brightness }))
            }
            other => Err(McpError::not_found(format!("tm1637 has no tool `{other}`"))),
        }
    }
}

/// Character cells a string occupies: the decimal point and the colon hang off a
/// digit instead of taking a place of their own.
pub fn digit_cells(text: &str) -> usize {
    text.chars().filter(|c| !matches!(c, '.' | ':')).count()
}

/// Segment bytes for a 4-digit module, digits left to right. A `.` sets bit 7 of
/// the digit before it; a `:` is bit 7 of digit 1 on the common clock module, which
/// is where its colon sits.
// TODO(verify): the colon bit on hardware; some modules put it on another digit.
pub fn encode(text: &str) -> [u8; 4] {
    let mut out = [SEG_BLANK; 4];
    let mut pos = 0usize;
    let mut colon = false;
    for ch in text.chars() {
        if ch == '.' {
            let at = pos.saturating_sub(1);
            out[at] |= SEG_DOT;
            continue;
        }
        if ch == ':' {
            colon = true;
            continue;
        }
        if pos >= 4 {
            break;
        }
        out[pos] = match ch {
            ' ' => SEG_BLANK,
            '-' => SEG_MINUS,
            '0'..='9' => SEG_DIGITS[(ch as u8 - b'0') as usize],
            'a'..='f' => SEG_LETTERS[(ch as u8 - b'a') as usize],
            'A'..='F' => SEG_LETTERS[(ch as u8 - b'A') as usize],
            _ => SEG_BLANK,
        };
        pos += 1;
    }
    if colon {
        out[1] |= SEG_DOT;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;

    const CLK: i32 = 5;
    const DIO: i32 = 6;

    /// A TM1637 as far as the wire is concerned: it samples DIO on every rising
    /// edge of CLK while a frame is open, groups the bytes it received into frames
    /// (start..stop) and pulls DIO low on the ninth clock. That is enough to read
    /// the driver's whole protocol back out of the pin trace.
    struct Display {
        clk_level: bool,
        dio_level: bool,
        dio_input: bool,
        in_frame: bool,
        cur: u8,
        bits: u8,
        frame: Vec<u8>,
        frames: Vec<Vec<u8>>,
        bytes: Vec<u8>,
        pending_ack: bool,
        ack: bool,
    }

    impl Display {
        fn answering() -> Self {
            Self {
                clk_level: false,
                dio_level: false,
                dio_input: false,
                in_frame: false,
                cur: 0,
                bits: 0,
                frame: Vec::new(),
                frames: Vec::new(),
                bytes: Vec::new(),
                pending_ack: false,
                ack: true,
            }
        }

        fn silent() -> Self {
            Self { ack: false, ..Self::answering() }
        }
    }

    impl Hardware for Display {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, pin: i32, mode: PinMode) -> Result<()> {
            if pin == DIO {
                self.dio_input = matches!(mode, PinMode::Input | PinMode::InputPullup);
            }
            Ok(())
        }

        fn gpio_read(&mut self, pin: i32) -> Result<bool> {
            if pin != DIO {
                return Ok(self.clk_level);
            }
            if self.dio_input {
                // The ninth clock: the display pulls DIO low when it received a byte.
                let low = self.pending_ack && self.ack;
                self.pending_ack = false;
                return Ok(!low);
            }
            Ok(self.dio_level)
        }

        fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
            if pin == CLK {
                let rising = high && !self.clk_level;
                self.clk_level = high;
                if rising && !self.dio_input && self.in_frame {
                    if self.dio_level {
                        self.cur |= 1 << self.bits;
                    }
                    self.bits += 1;
                    if self.bits == 8 {
                        self.bytes.push(self.cur);
                        self.frame.push(self.cur);
                        self.cur = 0;
                        self.bits = 0;
                        self.pending_ack = true;
                    }
                }
                return Ok(());
            }
            if pin != DIO || self.dio_input {
                return Ok(());
            }
            if self.clk_level && high != self.dio_level {
                if !high {
                    // Start: DIO falls while CLK is high.
                    self.in_frame = true;
                    self.frame.clear();
                    self.cur = 0;
                    self.bits = 0;
                } else if self.in_frame {
                    // Stop: DIO rises while CLK is high.
                    self.in_frame = false;
                    self.frames.push(std::mem::take(&mut self.frame));
                }
            }
            self.dio_level = high;
            Ok(())
        }
    }

    fn driver() -> Tm1637 {
        Tm1637 { clk: CLK, dio: DIO, brightness: 7, segments: [0; 4], id: "tm1637".into() }
    }

    #[test]
    fn a_full_refresh_is_three_frames_with_the_digits_in_the_middle() {
        let mut hw = Display::answering();
        let mut d = driver();
        d.show(&mut hw, encode("1234")).unwrap();
        assert_eq!(
            hw.frames,
            vec![vec![CMD_DATA_AUTO], vec![CMD_ADDRESS_0, 0x06, 0x5B, 0x4F, 0x66], vec![CMD_DISPLAY_ON | 7],],
            "data command, four digit bytes from 0xC0, display control"
        );
        assert_eq!(hw.bytes.len(), 7, "one byte per transfer plus the four digits");
        assert!(hw.clk_level, "the clock idles high");
    }

    #[test]
    fn digits_are_sent_lsb_first() {
        let mut hw = Display::answering();
        // '1' = 0x06: the two right-hand segments, so the first four clocks are low.
        Tm1637::send(&mut hw, CLK, DIO, &[0x06]).unwrap();
        assert_eq!(hw.bytes, vec![0x06]);
    }

    #[test]
    fn brightness_and_clear_are_single_frames() {
        let mut hw = Display::answering();
        let mut d = driver();
        d.set_brightness(&mut hw, 3).unwrap();
        assert_eq!(hw.frames.pop().unwrap(), vec![0x88 | 3]);
        d.show(&mut hw, encode("8888")).unwrap();
        hw.frames.clear();
        d.call("tm1637_clear", &json!({}), &mut hw).unwrap();
        assert_eq!(hw.frames[1], vec![CMD_ADDRESS_0, 0, 0, 0, 0], "digits blanked, display on");
        assert_eq!(d.segments(), [0; 4]);
    }

    #[test]
    fn a_module_that_does_not_acknowledge_is_not_detected() {
        let mut hw = Display::silent();
        assert_eq!(Tm1637::identify(&mut hw, CLK, DIO), None);
        let e = Tm1637::probe(&mut hw, CLK, DIO, 1).err().expect("the probe must fail");
        assert!(e.message.contains("did not acknowledge"), "{}", e.message);
        // And the bus is left idle rather than mid-frame.
        assert!(!hw.in_frame, "a failed send still emits the stop condition");
        assert!(hw.clk_level && hw.dio_level);
    }

    #[test]
    fn text_is_encoded_four_digits_wide() {
        assert_eq!(encode("12.5"), [0x06, 0x5B | SEG_DOT, 0x6D, 0x00]);
        assert_eq!(encode("-"), [SEG_MINUS, 0, 0, 0]);
        assert_eq!(encode("AB"), [0x77, 0x7C, 0, 0]);
        assert_eq!(encode("1:23")[1], 0x5B | SEG_DOT, "the colon hangs off digit 1");
        assert_eq!(digit_cells("12.5"), 3);
    }

    #[test]
    fn a_value_is_right_aligned_and_a_long_one_is_refused() {
        let mut hw = Display::answering();
        let mut d = driver();
        let out = d.call("tm1637_show", &json!({ "value": 23.5, "decimals": 1 }), &mut hw).unwrap();
        assert_eq!(out["text"], " 23.5", "three digits land in the last three cells");
        let out = d.call("tm1637_show", &json!({ "value": 42 }), &mut hw).unwrap();
        assert_eq!(out["text"], "  42", "a number is right-aligned by default");
        assert_eq!(out["segments"][3], "0x5B", "the rightmost cell holds '2'");
        let e = d.call("tm1637_show", &json!({ "value": 12345 }), &mut hw).unwrap_err();
        assert_eq!(e.code, crate::error::code::INVALID_PARAMS);
        assert!(e.message.contains("four"), "{}", e.message);
        let e = d.call("tm1637_show", &json!({}), &mut hw).unwrap_err();
        assert!(e.message.contains("text"), "{}", e.message);
        let e = d.call("tm1637_show", &json!({ "text": "1", "align": "center" }), &mut hw).unwrap_err();
        assert!(e.message.contains("align"), "{}", e.message);
        let e = d.call("tm1637_nope", &json!({}), &mut hw).unwrap_err();
        assert_eq!(e.code, crate::error::code::NOT_FOUND);
    }
}

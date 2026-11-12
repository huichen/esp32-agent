//! DHT11 / DHT22 (the AM2302 is a DHT22) temperature and humidity on one GPIO.
//!
//! The part speaks a one-wire protocol with no clock line: the host pulls the line
//! low for a start signal, releases it, and the sensor answers with a low-high
//! handshake followed by 40 bits. A bit's value *is* the width of its high pulse, so
//! reading the sensor is a measurement rather than a register read. That is why this
//! driver asks for [`Hardware::delay_us_checked`] instead of the infallible
//! `delay_us`: a board that cannot time a microsecond would sample the frame at the
//! wrong rate and return a plausible wrong number, and a refusal is the better
//! answer.
//!
//! Pulse widths are counted in one-microsecond steps and compared against the 80 us
//! handshake pulse measured in the same steps, so a step that costs more than a
//! microsecond on a given board cancels out rather than shifting the threshold.
//!
//! The part converts at most once every 2 s, and [`Dht::read`] enforces that against
//! the millisecond timestamp the caller passes in.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Start signal, DHT11: the host holds the line low for at least 18 ms.
// TODO(verify): 18 ms from the DHT11 datasheet; measure the shortest pulse this part accepts.
const START_LOW_US_DHT11: u32 = 18_000;

/// Start signal, DHT22: at least 1 ms.
// TODO(verify): 1 ms from the AM2302 datasheet; the DHT11 figure also works on a real
// DHT22, so this only shortens the read.
const START_LOW_US_DHT22: u32 = 1_000;

/// The sensor pulls the line low 20-40 us after the release, so waiting 100 steps is
/// generous. A step is one `delay_us_checked(1)` call, not a measured microsecond.
// TODO(verify): 20-40 us from both datasheets.
const RESPONSE_STEPS: u32 = 100;

/// Every pulse in the frame is shorter than 200 us: the 80 us handshake pulses, the
/// 50 us low before each bit, and the 70 us high of a one.
// TODO(verify): 80 us handshake, 50 us bit low, 70 us one-bit high, from both datasheets.
const PULSE_STEPS: u32 = 200;

/// Both datasheets put a zero at 26-28 us and a one at 70 us of high time. The frame
/// is decided by comparing a bit against the 80 us handshake pulse (`n * 5 > ruler * 3`,
/// i.e. 60 % of the handshake), which keeps that 26/70 split independent of what one
/// step costs on the board.
// TODO(verify): 26-28 us and 70 us from the datasheets; the 60 % threshold is the usual
// midpoint and wants a scope on the bench.
const ONE_IS_SIXTY_PERCENT: u32 = 3;

/// One conversion every 2 s at most, for either part.
// TODO(verify): the DHT11 datasheet asks for 1 s between reads and the AM2302 for 2 s.
// The stricter of the two is what this driver enforces, so one code path is safe for both.
pub const MIN_INTERVAL_MS: u64 = 2_000;

/// Which part is on the pin. The frame layout differs, so the kind is not a label: a
/// DHT11 frame decoded as a DHT22 comes out ten times too large.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dht11,
    Dht22,
}

impl Kind {
    /// `dht11` / `dht22`, plus the module name `am2302` for the DHT22.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dht11" => Some(Self::Dht11),
            "dht22" | "am2302" => Some(Self::Dht22),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dht11 => "dht11",
            Self::Dht22 => "dht22",
        }
    }

    /// Datasheet name, for messages a person reads.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dht11 => "DHT11",
            Self::Dht22 => "DHT22",
        }
    }

    fn start_low_us(self) -> u32 {
        match self {
            Self::Dht11 => START_LOW_US_DHT11,
            Self::Dht22 => START_LOW_US_DHT22,
        }
    }

    /// The five frame bytes decoded into `(temperature °C, humidity %RH)`.
    ///
    /// Bytes 0-1 are humidity and 2-3 temperature, either as two whole-number bytes
    /// (DHT11) or one 16-bit tenths value (DHT22), with byte 4 the checksum that
    /// [`Dht::read`] has already checked.
    fn decode(self, raw: &[u8; 5]) -> (f32, f32) {
        match self {
            // Whole degrees and whole percent. Newer DHT11 modules fill the decimal
            // bytes instead of zero, so they are added when they are not zero.
            Self::Dht11 => (raw[2] as f32 + raw[3] as f32 / 10.0, raw[0] as f32 + raw[1] as f32 / 10.0),
            // Tenths, and bit 7 of byte 2 is the sign of the temperature.
            Self::Dht22 => {
                let humidity = u16::from_be_bytes([raw[0], raw[1]]) as f32 / 10.0;
                let sign = if raw[2] & 0x80 != 0 { -1.0 } else { 1.0 };
                let temperature = u16::from_be_bytes([raw[2] & 0x7F, raw[3]]) as f32 / 10.0 * sign;
                (temperature, humidity)
            }
        }
    }
}

/// One frame, decoded.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    pub temperature_c: f32,
    pub humidity_pct: f32,
    /// The five raw bytes, so a bench can see a frame that decoded badly instead of
    /// only the number it turned into.
    pub raw: [u8; 5],
}

pub struct Dht {
    pub pin: i32,
    pub kind: Kind,
    id: String,
    /// When the previous attempt started. The sensor begins a conversion on the start
    /// pulse whether or not the frame arrives intact, so a failed attempt counts too.
    last_attempt_ms: Option<u64>,
}

impl Dht {
    pub fn new(pin: i32, kind: Kind) -> Self {
        Self { pin, kind, id: kind.as_str().to_string(), last_attempt_ms: None }
    }

    /// Build from the `options` of a manifest declaration, where `kind` is `dht11` or
    /// `dht22`. Unknown is an error rather than a default: the frame layout differs
    /// between the two, so a guess would be off by a factor of ten.
    pub fn from_options(pin: i32, options: &BTreeMap<String, String>) -> Result<Self> {
        match options.get("kind") {
            Some(raw) => match Kind::parse(raw) {
                Some(kind) => Ok(Self::new(pin, kind)),
                None => Err(McpError::invalid_params(format!(
                    "driver `dht` takes options.kind = \"dht11\" or \"dht22\", not `{raw}`"
                ))),
            },
            None => Err(McpError::invalid_params(
                "driver `dht` needs options.kind = \"dht11\" or \"dht22\": the two parts decode differently",
            )),
        }
    }

    /// Pull the line low: the start of a conversion.
    fn start_signal(&self, hw: &mut dyn Hardware) -> Result<()> {
        hw.gpio_write(self.pin, false)?;
        hw.gpio_config(self.pin, PinMode::Output)?;
        hw.delay_us_checked(self.kind.start_low_us())?;
        // Released to the pull-up: from here the sensor drives the line.
        hw.gpio_config(self.pin, PinMode::InputPullup)
    }

    /// Wait until the line reads `level`, or give up after `max_steps` steps.
    fn await_level(hw: &mut dyn Hardware, pin: i32, level: bool, max_steps: u32) -> Result<()> {
        for _ in 0..max_steps {
            if hw.gpio_read(pin)? == level {
                return Ok(());
            }
            hw.delay_us_checked(1)?;
        }
        Err(McpError::hardware(format!(
            "GPIO{pin} stayed {} for more than {max_steps} steps",
            if level { "low" } else { "high" }
        )))
    }

    /// How many steps the line stayed high. This is the measurement the whole driver
    /// rests on: the caller compares it with the 80 us handshake pulse.
    fn high_pulse(hw: &mut dyn Hardware, pin: i32, max_steps: u32) -> Result<u32> {
        let mut steps = 0;
        while hw.gpio_read(pin)? {
            if steps >= max_steps {
                return Err(McpError::hardware(format!(
                    "GPIO{pin} stayed high for more than {max_steps} steps"
                )));
            }
            hw.delay_us_checked(1)?;
            steps += 1;
        }
        Ok(steps)
    }

    fn no_answer(&self, why: &McpError) -> McpError {
        McpError::hardware(format!(
            "no answer from the {} on GPIO{}: {} (the line needs a pull-up, and the part 3.3-5 V)",
            self.kind.label(),
            self.pin,
            why.message
        ))
    }

    /// Clock one 40-bit frame out of the sensor. Checks no checksum: [`Dht::read`] does.
    fn frame(&self, hw: &mut dyn Hardware) -> Result<[u8; 5]> {
        let pin = self.pin;
        self.start_signal(hw)?;

        // Handshake: the sensor pulls the line low, holds it 80 us, then raises it for
        // 80 us. That second pulse is the ruler every bit is compared against.
        Self::await_level(hw, pin, false, RESPONSE_STEPS).map_err(|e| self.no_answer(&e))?;
        Self::await_level(hw, pin, true, PULSE_STEPS).map_err(|e| self.no_answer(&e))?;
        let ruler = Self::high_pulse(hw, pin, PULSE_STEPS).map_err(|e| self.no_answer(&e))?;
        if ruler == 0 {
            return Err(self.no_answer(&McpError::hardware("the handshake pulse was not measurable")));
        }

        let mut raw = [0u8; 5];
        for bit in 0..40usize {
            // Each bit opens with a 50 us low, then the high whose width is the value.
            Self::await_level(hw, pin, true, PULSE_STEPS).map_err(|e| self.no_answer(&e))?;
            let high = Self::high_pulse(hw, pin, PULSE_STEPS).map_err(|e| self.no_answer(&e))?;
            if high * 5 > ruler * ONE_IS_SIXTY_PERCENT {
                raw[bit / 8] |= 1 << (7 - bit % 8);
            }
        }
        Ok(raw)
    }

    /// Read one frame, subject to the sensor's 2 s conversion interval.
    ///
    /// `now_ms` is a monotonic millisecond timestamp from the caller (the board's own
    /// clock); the driver keeps the previous attempt so the rate limit lives here
    /// rather than in whichever tool happens to call it. A timestamp of 0 means the
    /// platform has no clock, and the limit is then not enforced.
    pub fn read(&mut self, hw: &mut dyn Hardware, now_ms: u64) -> Result<Reading> {
        if let Some(last) = self.last_attempt_ms {
            if now_ms > 0 && now_ms >= last && now_ms - last < MIN_INTERVAL_MS {
                let since = now_ms - last;
                let wait = MIN_INTERVAL_MS - since;
                return Err(McpError::hardware(format!(
                    "{} converts at most once every {MIN_INTERVAL_MS} ms; the last attempt was {since} ms ago, try again in {wait} ms",
                    self.kind.label()
                ))
                .with_data(json!({ "retry_in_ms": wait, "min_interval_ms": MIN_INTERVAL_MS })));
            }
        }
        if now_ms > 0 {
            self.last_attempt_ms = Some(now_ms);
        }

        let raw = self.frame(hw)?;
        let sum = raw[..4].iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
        if sum != raw[4] {
            return Err(McpError::hardware(format!(
                "{} checksum mismatch on GPIO{}: bytes {:02X} {:02X} {:02X} {:02X} sum to 0x{sum:02X} but the frame carries 0x{:02X}",
                self.kind.label(),
                self.pin,
                raw[0],
                raw[1],
                raw[2],
                raw[3],
                raw[4]
            ))
            .with_data(json!({ "raw": raw, "computed_checksum": sum, "frame_checksum": raw[4] })));
        }
        let (temperature_c, humidity_pct) = self.kind.decode(&raw);
        Ok(Reading { temperature_c, humidity_pct, raw })
    }
}

fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

impl Device for Dht {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "dht11",
            &format!("{} temperature + humidity on GPIO{}", self.kind.label(), self.pin),
            "gpio",
        );
        i.id = self.id.clone();
        i.pins = vec![self.pin];
        i.source = "config".into();
        i.caps = vec!["dht".into(), "gpio".into(), "temp".into()];
        i.detail = json!({ "kind": self.kind.as_str(), "min_interval_ms": MIN_INTERVAL_MS });
        i.tools = vec!["dht_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let description = format!(
            "Read temperature and humidity from the {} on GPIO{}. One conversion per 2 s at most (the datasheet limit); a call inside that window is refused with the remaining wait. Returns the five raw frame bytes as well, so a bad frame can be inspected instead of trusted",
            self.kind.label(),
            self.pin
        );
        vec![ToolDef::new("dht_read", &description)
            .title("DHT read")
            .category("sensor")
            .requires(&["dht"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "dht_read", args, ctx.hw)
            })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "dht_read" => {
                let now = hw.millis();
                let r = self.read(hw, now)?;
                Ok(json!({
                    "kind": self.kind.as_str(),
                    "temperature_c": round1(r.temperature_c),
                    "humidity_pct": round1(r.humidity_pct),
                    "raw": r.raw,
                    "pin": self.pin,
                    "min_interval_ms": MIN_INTERVAL_MS,
                }))
            }
            other => Err(McpError::not_found(format!("dht has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code;
    use crate::hardware::BoardInfo;

    /// A DHT that answers on a virtual clock: the start signal is observed, and the
    /// handshake plus the 40 bits are laid out in time so the driver has to measure
    /// them the way it would on a real pin. Nothing sleeps, so a test runs at once.
    struct FakeDht {
        pin: i32,
        raw: [u8; 5],
        now_us: u64,
        output: bool,
        level_out: bool,
        driving_low: bool,
        release_us: Option<u64>,
        /// `(duration_us, level)` of the answer, from the release onwards.
        segments: Vec<(u32, bool)>,
        /// A sensor with nothing on the line: the handshake never happens.
        mute: bool,
    }

    impl FakeDht {
        fn new(raw: [u8; 5]) -> Self {
            Self {
                pin: 4,
                raw,
                now_us: 0,
                output: false,
                level_out: true,
                driving_low: false,
                release_us: None,
                segments: Vec::new(),
                mute: false,
            }
        }

        /// Lay out the frame the datasheet describes: 20-40 us of idle, 80 us low,
        /// 80 us high, then per bit a 50 us low and a 26 us (zero) or 70 us (one) high.
        fn build(&mut self) {
            if self.mute {
                self.segments.clear();
                return;
            }
            let mut seg = vec![(30u32, true), (80, false), (80, true)];
            for bit in 0..40usize {
                let one = (self.raw[bit / 8] >> (7 - bit % 8)) & 1 == 1;
                seg.push((50, false));
                seg.push((if one { 70 } else { 26 }, true));
            }
            seg.push((50, false));
            self.segments = seg;
        }

        fn level(&self) -> bool {
            if self.driving_low {
                return false;
            }
            let Some(released) = self.release_us else {
                return true;
            };
            let mut t = self.now_us.saturating_sub(released);
            for (duration, level) in &self.segments {
                if t < u64::from(*duration) {
                    return *level;
                }
                t -= u64::from(*duration);
            }
            true
        }
    }

    impl Hardware for FakeDht {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, pin: i32, mode: PinMode) -> Result<()> {
            if pin != self.pin {
                return Err(McpError::not_found(format!("nothing on GPIO{pin}")));
            }
            match mode {
                PinMode::Output => {
                    self.output = true;
                    self.driving_low = !self.level_out;
                    self.release_us = None;
                }
                PinMode::InputPullup => {
                    self.output = false;
                    self.driving_low = false;
                    if self.release_us.is_none() {
                        self.release_us = Some(self.now_us);
                        self.build();
                    }
                }
                _ => self.output = false,
            }
            Ok(())
        }

        fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
            if pin != self.pin {
                return Err(McpError::not_found(format!("nothing on GPIO{pin}")));
            }
            self.level_out = high;
            if self.output {
                self.driving_low = !high;
            }
            Ok(())
        }

        fn gpio_read(&mut self, pin: i32) -> Result<bool> {
            if pin != self.pin {
                return Err(McpError::not_found(format!("nothing on GPIO{pin}")));
            }
            Ok(self.level())
        }

        /// The whole point of the driver: it refuses a board where this is not there.
        fn delay_us_checked(&mut self, us: u32) -> Result<()> {
            self.now_us += u64::from(us);
            Ok(())
        }

        fn millis(&mut self) -> u64 {
            self.now_us / 1000
        }
    }

    /// 45.6 %RH, 23.4 °C as a DHT22 writes it.
    const DHT22_FRAME: [u8; 5] = [0x01, 0xC8, 0x00, 0xEA, 0xB3];
    /// 45 %RH, 23 °C as a DHT11 writes it.
    const DHT11_FRAME: [u8; 5] = [45, 0, 23, 0, 68];

    #[test]
    fn a_dht22_frame_decodes_to_the_values_the_bytes_hold() {
        let mut hw = FakeDht::new(DHT22_FRAME);
        let mut dev = Dht::new(4, Kind::Dht22);
        let r = dev.read(&mut hw, 1_000).expect("a scripted frame decodes");
        assert!((r.temperature_c - 23.4).abs() < 1e-3, "temperature {}", r.temperature_c);
        assert!((r.humidity_pct - 45.6).abs() < 1e-3, "humidity {}", r.humidity_pct);
        assert_eq!(r.raw, DHT22_FRAME, "the raw bytes reach the caller unchanged");
    }

    #[test]
    fn a_dht11_frame_is_whole_degrees_and_whole_percent() {
        let mut hw = FakeDht::new(DHT11_FRAME);
        let mut dev = Dht::new(4, Kind::Dht11);
        let r = dev.read(&mut hw, 1_000).expect("a scripted frame decodes");
        assert!((r.temperature_c - 23.0).abs() < 1e-3, "temperature {}", r.temperature_c);
        assert!((r.humidity_pct - 45.0).abs() < 1e-3, "humidity {}", r.humidity_pct);
    }

    #[test]
    fn a_negative_temperature_uses_the_sign_bit() {
        // -10.5 °C: 105 tenths with bit 7 of the high byte set.
        let mut segments = [0u8; 5];
        segments[2] = 0x80;
        segments[3] = 0x69;
        segments[4] = segments[..4].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        let (t, _) = Kind::Dht22.decode(&segments);
        assert!((t + 10.5).abs() < 1e-3, "temperature {t}");
        // And the DHT11 never reports a negative value: byte 3 is a decimal there.
        assert_eq!(Kind::Dht11.decode(&DHT11_FRAME).0, 23.0);
    }

    #[test]
    fn a_bad_checksum_is_refused_with_the_bytes_that_arrived() {
        let mut bad = DHT22_FRAME;
        bad[4] = 0x00;
        let mut hw = FakeDht::new(bad);
        let mut dev = Dht::new(4, Kind::Dht22);
        let e = dev.read(&mut hw, 1_000).expect_err("a wrong checksum must not decode");
        assert_eq!(e.code, code::HARDWARE);
        assert!(e.message.contains("checksum mismatch"), "message: {}", e.message);
        // The bench needs the bytes and both sums, not just "it failed".
        assert!(e.message.contains("01 C8 00 EA"), "message: {}", e.message);
        assert!(e.message.contains("0xB3"), "message: {}", e.message);
        let raw = e.data.expect("the frame is carried as data")["raw"].clone();
        assert_eq!(raw[3], 0xEA);
    }

    #[test]
    fn two_conversions_inside_two_seconds_are_refused_and_then_allowed() {
        let mut hw = FakeDht::new(DHT22_FRAME);
        let mut dev = Dht::new(4, Kind::Dht22);
        dev.read(&mut hw, 1_000).expect("the first read is not limited");

        let e = dev.read(&mut hw, 1_500).expect_err("500 ms later is too soon");
        assert_eq!(e.code, code::HARDWARE);
        assert!(e.message.contains("2000 ms"), "message: {}", e.message);
        assert_eq!(e.data.expect("a wait is carried")["retry_in_ms"], 1_500);

        dev.read(&mut hw, 3_000).expect("2 s later the sensor is ready again");
    }

    #[test]
    fn a_line_that_never_answers_says_so_instead_of_decoding_garbage() {
        // Nothing released onto the line: the handshake never starts.
        let mut hw = FakeDht::new(DHT22_FRAME);
        hw.mute = true;
        let mut dev = Dht::new(4, Kind::Dht22);
        let e = dev.read(&mut hw, 1_000).expect_err("no handshake, no reading");
        assert_eq!(e.code, code::HARDWARE);
        assert!(e.message.contains("no answer"), "message: {}", e.message);
    }

    #[test]
    fn the_tool_reports_the_kind_the_raw_bytes_and_the_pin() {
        let mut hw = FakeDht::new(DHT11_FRAME);
        let mut dev = Dht::new(4, Kind::Dht11);
        let v = dev.call("dht_read", &json!({}), &mut hw).expect("a scripted frame reads");
        assert_eq!(v["kind"], "dht11");
        assert_eq!(v["temperature_c"], 23.0);
        assert_eq!(v["humidity_pct"], 45.0);
        assert_eq!(v["raw"][4], 68);
        assert_eq!(v["pin"], 4);
        assert_eq!(dev.info().caps[0], "dht");
        assert_eq!(dev.tools()[0].spec.name, "dht_read");
    }

    #[test]
    fn the_kind_comes_from_the_declaration_and_an_unknown_one_is_refused() {
        let mut options = BTreeMap::new();
        options.insert("kind".to_string(), "dht22".to_string());
        assert_eq!(Dht::from_options(4, &options).expect("a valid kind").kind, Kind::Dht22);
        options.insert("kind".to_string(), "am2302".to_string());
        assert_eq!(Dht::from_options(4, &options).expect("the module name").kind, Kind::Dht22);

        options.insert("kind".to_string(), "dht23".to_string());
        assert!(Dht::from_options(4, &options).is_err(), "an unknown kind must not guess");
        assert!(Dht::from_options(4, &BTreeMap::new()).is_err(), "a missing kind must not guess");
    }
}

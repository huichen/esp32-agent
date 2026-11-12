//! The one trait the firmware implements.
//!
//! Everything above it — MCP, HTTP, the config portal, the device drivers — is
//! portable Rust that builds and runs on a desktop with [`crate::sim`]'s virtual
//! board. A driver never touches ESP-IDF types; it only calls these methods.
//! That is what makes a new peripheral testable without hardware and keeps the
//! firmware crate down to pin/bus plumbing.

use crate::error::{McpError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoardInfo {
    /// `esp32s3`, `esp32`, `esp32c3`, ... or `sim` on the host.
    pub soc: String,
    pub cores: u32,
    pub revision: u32,
    pub flash_mb: u32,
    pub psram_kb: u32,
    pub mac: String,
    pub free_heap_kb: u32,
    pub min_free_heap_kb: u32,
    pub idf_version: String,
    /// Temperature reported by the SoC sensor, °C.
    pub temp_c: Option<f32>,
    pub firmware: String,
    pub build: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WifiStatus {
    /// `connected`, `connecting`, `disconnected`, `ap`, `ap+sta`, `off`
    pub state: String,
    pub ssid: String,
    pub ip: String,
    pub rssi_dbm: i32,
    pub channel: u8,
    pub mac: String,
    pub hostname: String,
    pub ap_ssid: String,
    pub ap_ip: String,
    pub clients: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WifiAp {
    pub ssid: String,
    pub rssi: i32,
    pub channel: u8,
    pub auth: String,
}

/// One heap pool: what a capability asks for, and how much of it is left.
///
/// A pool is a set of capability flags, not an address range — `internal_8bit` and `dma`
/// overlap on every chip — so the numbers here are answers to "how much RAM satisfies this
/// request", which is the question a new stack or a DMA buffer actually asks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeapPool {
    /// The name of the pool, as the board spells it: `internal_8bit`, `dma`, `iram_8bit`,
    /// `spiram`, ...
    pub pool: String,
    /// One clause on what the pool is for, so a reader who has never seen a heap_caps flag
    /// knows which number applies to the change they are making.
    pub note: String,
    pub total_kb: u32,
    pub free_kb: u32,
    /// The largest single allocation that fits. `free_kb` cannot answer this, and this is
    /// the number a thread stack or a TLS buffer has to fit in.
    pub largest_kb: u32,
    /// The lowest `free_kb` since boot: the peak this pool has already been through.
    pub min_free_kb: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMode {
    Input,
    InputPullup,
    InputPulldown,
    Output,
}

impl PinMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "input" | "in" => Some(Self::Input),
            "input_pullup" | "pullup" | "up" => Some(Self::InputPullup),
            "input_pulldown" | "pulldown" | "down" => Some(Self::InputPulldown),
            "output" | "out" => Some(Self::Output),
            _ => None,
        }
    }
}

/// The hardware surface. Implementations: `firmware::esp::EspHardware` (ESP-IDF)
/// and `mcp-sim::VirtualBoard` (host).
///
/// Methods a given board cannot support return [`McpError::unsupported`]; every
/// method has a default so a new board implementation only writes what it has.
pub trait Hardware: Send {
    /// Identification of the running chip and firmware.
    fn board_info(&mut self) -> BoardInfo;

    /// I2C: scan one bus and return the addresses that acknowledged.
    fn i2c_scan(&mut self, _bus: u8) -> Result<Vec<u8>> {
        Err(McpError::unsupported("I2C is not available on this board"))
    }

    /// I2C: read `out.len()` bytes, optionally starting with a register byte.
    fn i2c_read(&mut self, _bus: u8, _addr: u8, _reg: Option<u8>, _out: &mut [u8]) -> Result<()> {
        Err(McpError::unsupported("I2C is not available on this board"))
    }

    /// I2C: write bytes, optionally preceded by a register byte.
    fn i2c_write(&mut self, _bus: u8, _addr: u8, _reg: Option<u8>, _data: &[u8]) -> Result<()> {
        Err(McpError::unsupported("I2C is not available on this board"))
    }

    /// SPI: transfer `data` and return the same number of bytes shifted back.
    ///
    /// Full duplex; chip select is the caller's job (drivers drive it with
    /// `gpio_write`, so the pin is visible to a manifest too). Defaulted, so a
    /// board without SPI still compiles and a driver reports one clear error
    /// instead of carrying an unreachable branch.
    fn spi_transfer(&mut self, _bus: u8, _data: &[u8]) -> Result<Vec<u8>> {
        Err(McpError::unsupported("SPI is not available on this board"))
    }

    /// Camera: capture one frame as packed RGB888 (three bytes per pixel, row
    /// major). `opts` carries the pins and geometry declared by the manifest or
    /// the board profile. Defaulted for the same reason as `spi_transfer`.
    fn dvp_capture(&mut self, _opts: &serde_json::Value) -> Result<Vec<u8>> {
        Err(McpError::unsupported("a camera is not available on this board"))
    }

    fn gpio_config(&mut self, _pin: i32, _mode: PinMode) -> Result<()> {
        Err(McpError::unsupported("GPIO is not available on this board"))
    }

    fn gpio_read(&mut self, _pin: i32) -> Result<bool> {
        Err(McpError::unsupported("GPIO is not available on this board"))
    }

    fn gpio_write(&mut self, _pin: i32, _high: bool) -> Result<()> {
        Err(McpError::unsupported("GPIO is not available on this board"))
    }

    /// PWM on a pin (servo, buzzer, LED dimming). `duty_pct` is 0..=100.
    fn pwm_set(&mut self, _pin: i32, _freq_hz: u32, _duty_pct: f32) -> Result<()> {
        Err(McpError::unsupported("PWM is not available on this board"))
    }

    /// ADC in millivolts.
    fn adc_mv(&mut self, _pin: i32) -> Result<u32> {
        Err(McpError::unsupported("ADC is not available on this board"))
    }

    /// Set up the I2S peripherals from the board's wiring.
    ///
    /// I2S has no addressing and no id register, so nothing here can be *detected*: the
    /// pins come from the board profile or `config.pins`, and this either brings the
    /// channels up or says why not. `None` on a side means that direction is not wired.
    fn i2s_setup(
        &mut self,
        _mic: Option<crate::boards::I2sPins>,
        _amp: Option<crate::boards::I2sPins>,
    ) -> Result<()> {
        Err(McpError::unsupported("no I2S audio on this board"))
    }

    /// Read up to `out.len()` 32-bit samples from the microphone.
    ///
    /// 32-bit because that is what an INMP441 sends: 24 bits left-justified in a 32-bit
    /// slot, left channel only (measured on this board). The caller scales down to what a
    /// tool result can carry. Returns how many arrived before `timeout_ms`.
    fn i2s_mic_read(&mut self, _out: &mut [i32], _timeout_ms: u32) -> Result<usize> {
        Err(McpError::unsupported("no I2S microphone on this board"))
    }

    /// Write 16-bit mono samples to the amplifier, blocking until they are accepted.
    fn i2s_out_write(&mut self, _samples: &[i16]) -> Result<()> {
        Err(McpError::unsupported("no I2S speaker on this board"))
    }

    /// Drive a single WS2812/SK6812 pixel (or the whole strip to one colour).
    fn ring_set(&mut self, _pin: i32, _rgb: (u8, u8, u8), _count: u16) -> Result<()> {
        Err(McpError::unsupported("addressable RGB LEDs are not available on this board"))
    }

    /// Microsecond-accurate delay (1-Wire, HC-SR04, bit-banged buses).
    fn delay_us(&mut self, _us: u32) {}

    /// The same busy-wait, saying so when the platform cannot time microseconds.
    ///
    /// [`Hardware::delay_us`] predates this and is infallible: its default is a silent
    /// no-op, which is fine for "wait at least this long before reading a register" and
    /// wrong for a bus whose pulse widths *are* the data. A driver that samples a
    /// waveform therefore asks for this one, and a board that cannot honour microsecond
    /// timing refuses instead of returning a plausible wrong number.
    ///
    /// Defaulted: the default is the refusal.
    fn delay_us_checked(&mut self, _us: u32) -> Result<()> {
        Err(McpError::unsupported("this platform has no microsecond delay"))
    }

    fn millis(&mut self) -> u64 {
        0
    }

    fn wifi_status(&mut self) -> WifiStatus {
        WifiStatus { state: "off".into(), ..Default::default() }
    }

    fn wifi_scan(&mut self) -> Result<Vec<WifiAp>> {
        Err(McpError::unsupported("WiFi is not available on this board"))
    }

    fn wifi_connect(&mut self, _ssid: &str, _pw: &str) -> Result<()> {
        Err(McpError::unsupported("WiFi is not available on this board"))
    }

    /// Ask the platform to restart. On the simulator this stops the process.
    fn reboot(&mut self) {}

    /// The bus addresses the board profile says are worth probing, so a driver
    /// can be tried before a full 0x08..0x77 scan on a slow bus.
    fn hint_buses(&mut self) -> Vec<crate::config::I2cBusCfg> {
        Vec::new()
    }

    /// Extra inventory rows a board wants to publish (strapping pins, PSRAM, ...).
    fn inventory_extra(&mut self) -> Value {
        json!({})
    }

    /// Internal RAM: `(free, largest contiguous block)` in bytes.
    ///
    /// Thread stacks and DMA come from here, and PSRAM cannot supply either, so this is
    /// the number that decides whether an https handshake or a new connection thread fits.
    /// The portal reports it next to the total heap for exactly that reason.
    ///
    /// Derived from [`Hardware::heap_pools`] by default: a board that can name its pools
    /// already knows this number, and two sources for it let the one a model reads (the
    /// status tool) disagree with the one a person reads (`/api/memory`). A board with no
    /// pools at all reports `None` rather than a zero somebody would take as "no RAM".
    fn heap_internal(&mut self) -> Option<(u32, u32)> {
        self.heap_pools()
            .into_iter()
            .find(|p| p.pool == "internal_8bit" || p.pool == "internal")
            .map(|p| (p.free_kb.saturating_mul(1024), p.largest_kb.saturating_mul(1024)))
    }

    /// The chip's own temperature sensor, where the part has one. `None` is a real
    /// answer: the ESP32-C3 has no such sensor, and a board that made one up would be
    /// worse than a board that says it cannot.
    fn chip_temp_c(&mut self) -> Option<f32> {
        None
    }

    /// Running tasks with their state and remaining stack, for `shell ps`.
    ///
    /// Empty means the platform cannot enumerate them; the command says so rather than
    /// printing an empty table that looks like a board with nothing running.
    ///
    /// Each entry carries `stack_free_bytes`: the *lowest* free stack the task has had
    /// since it started, which is what FreeRTOS calls its high-water mark. It is the
    /// reading a stack constant is judged by — the stack size itself lives in the source,
    /// because FreeRTOS reports what is left of a stack, never how big it is.
    fn tasks(&mut self) -> Vec<serde_json::Value> {
        Vec::new()
    }

    /// Every heap pool the platform can describe, for `/api/memory`.
    ///
    /// Empty means "one heap, which cannot be split": that is the truth on the host, and
    /// the route says so rather than printing an empty table that reads like zero RAM.
    fn heap_pools(&mut self) -> Vec<HeapPool> {
        Vec::new()
    }

    /// ICMP echo. The default refuses: a platform that cannot send one must say so rather
    /// than report every host as unreachable, which is what a fake reply would mean.
    fn ping(&mut self, _host: &str, _count: u32) -> Result<serde_json::Value> {
        Err(McpError::unsupported("ping is not available on this platform"))
    }
}

/// Convenience: a `Hardware` that answers nothing, for tests of code paths that
/// must degrade gracefully.
#[derive(Debug, Default)]
pub struct NullHardware;

impl Hardware for NullHardware {
    fn board_info(&mut self) -> BoardInfo {
        BoardInfo { soc: "null".into(), firmware: "test".into(), ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A board that has a chip and nothing else. Every other method comes from the trait
    /// default, which is the contract this module is: a new board writes only what it has,
    /// and everything it does not have has to refuse by name instead of pretending.
    struct Bare;

    impl Hardware for Bare {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "bare".into(), ..Default::default() }
        }
    }

    fn unsupported(e: McpError) -> String {
        assert_eq!(e.code, crate::error::code::UNSUPPORTED, "{}", e.message);
        e.message
    }

    /// Each default says which capability is missing. A driver that gets a bare refusal
    /// still has to be able to name the bus it wanted, so the message is the assertion.
    #[test]
    fn every_unimplemented_method_refuses_and_names_the_missing_capability() {
        let mut hw = Bare;

        assert!(unsupported(hw.i2c_scan(0).expect_err("no i2c")).contains("I2C"));
        let mut buf = [0u8; 2];
        assert!(unsupported(hw.i2c_read(0, 0x20, None, &mut buf).expect_err("no i2c")).contains("I2C"));
        assert!(unsupported(hw.i2c_write(0, 0x20, None, &[0]).expect_err("no i2c")).contains("I2C"));
        assert!(unsupported(hw.spi_transfer(0, &[0]).expect_err("no spi")).contains("SPI"));
        assert!(unsupported(hw.dvp_capture(&json!({})).expect_err("no camera")).contains("camera"));
        assert!(unsupported(hw.gpio_config(4, PinMode::Input).expect_err("no gpio")).contains("GPIO"));
        assert!(unsupported(hw.gpio_read(4).expect_err("no gpio")).contains("GPIO"));
        assert!(unsupported(hw.gpio_write(4, true).expect_err("no gpio")).contains("GPIO"));
        assert!(unsupported(hw.pwm_set(4, 1_000, 50.0).expect_err("no pwm")).contains("PWM"));
        assert!(unsupported(hw.adc_mv(4).expect_err("no adc")).contains("ADC"));
        assert!(unsupported(hw.i2s_setup(None, None).expect_err("no i2s")).contains("I2S"));
        assert!(unsupported(hw.i2s_mic_read(&mut [0i32; 4], 10).expect_err("no mic")).contains("microphone"));
        assert!(unsupported(hw.i2s_out_write(&[0i16; 4]).expect_err("no amp")).contains("speaker"));
        assert!(unsupported(hw.ring_set(4, (1, 2, 3), 1).expect_err("no rgb")).contains("RGB"));
        assert!(unsupported(hw.wifi_scan().expect_err("no wifi")).contains("WiFi"));
        assert!(unsupported(hw.wifi_connect("ssid", "pw").expect_err("no wifi")).contains("WiFi"));
        assert!(unsupported(hw.ping("example.com", 1).expect_err("no ping")).contains("ping"));
    }

    /// The methods that answer rather than refuse: a board with no facts still has to
    /// produce a well-formed answer, because these feed the portal and the tool results.
    #[test]
    fn the_methods_that_answer_explain_themselves_rather_than_refusing() {
        let mut hw = Bare;

        // WiFi is reported as off, not as an error: an API client asking "where are we?"
        // gets an answer even from a board with no radio.
        let wifi = hw.wifi_status();
        assert_eq!(wifi.state, "off");
        assert_eq!(wifi.ssid, "", "nothing connected");
        assert_eq!(hw.hint_buses().len(), 0, "no profile hints on a bare board");
        assert_eq!(hw.inventory_extra(), json!({}), "no extra inventory rows");
        assert_eq!(hw.heap_internal(), None, "no platform heap numbers");
        assert_eq!(hw.chip_temp_c(), None, "a board without a sensor says so");
        assert_eq!(hw.tasks().len(), 0, "no task table the platform can print");
        assert_eq!(hw.millis(), 0);
        // These three are silent because there is nothing to report, not because they
        // failed: a delay on a board with no clock, a reboot that is the platform's job.
        hw.delay_us(1_000);
        hw.reboot();
        let info = hw.board_info();
        assert_eq!(info.soc, "bare");
        assert_eq!(info.firmware, "", "the default has no firmware string");
    }

    /// `NullHardware` exists so a graceful-degradation test does not have to write a fake.
    /// It has to be as bare as its name, or a test using it would silently get behaviour.
    #[test]
    fn null_hardware_answers_nothing_but_identifies_itself() {
        let mut hw = NullHardware;
        let info = hw.board_info();
        assert_eq!(info.soc, "null");
        assert_eq!(info.firmware, "test");
        assert_eq!(hw.gpio_read(0).expect_err("null has no gpio").code, crate::error::code::UNSUPPORTED);
        assert_eq!(hw.wifi_status().state, "off");
    }

    #[test]
    fn pin_mode_parses_the_spellings_a_manifest_can_carry() {
        for (text, want) in [
            ("input", PinMode::Input),
            ("IN", PinMode::Input),
            (" input_pullup ", PinMode::InputPullup),
            ("pullup", PinMode::InputPullup),
            ("up", PinMode::InputPullup),
            ("input_pulldown", PinMode::InputPulldown),
            ("pulldown", PinMode::InputPulldown),
            ("down", PinMode::InputPulldown),
            ("output", PinMode::Output),
            ("OUT", PinMode::Output),
        ] {
            assert_eq!(PinMode::parse(text), Some(want), "{text:?}");
        }
        assert_eq!(PinMode::parse("tri-state"), None, "an unknown mode is refused, not guessed");
        assert_eq!(PinMode::parse(""), None);
    }
}

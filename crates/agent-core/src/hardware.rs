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

//! The ESP-IDF implementation of [`Hardware`].
//!
//! Deliberately thin: every decision about *what* to read lives in
//! `esp32-agent-core`. This file only moves bytes on buses and pins, using the
//! ESP-IDF C API because the pins it must drive come from config at runtime
//! rather than from typed peripherals at compile time.
//!
//! All structs are built with `zeroed()` and then assigned field by field: the
//! generated bindings change their private bitfield layout between IDF versions,
//! and a struct literal breaks on that every time.
//!
//! > Build gate: `scripts/build-firmware.ps1`.

use esp32_agent_core::error::{McpError, Result};
use esp32_agent_core::hardware::{BoardInfo, Hardware, PinMode, WifiAp, WifiStatus};
use esp_idf_sys as sys;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::wifi;

const I2C_TIMEOUT_MS: i32 = 50;
const MAX_PWM_CHANNELS: u32 = 8;

pub struct EspHw {
    buses: HashMap<u8, sys::i2c_master_bus_handle_t>,
    devices: HashMap<(u8, u8), sys::i2c_master_dev_handle_t>,
    pwm: HashMap<i32, u32>,
    next_pwm_channel: u32,
    pwm_freq: u32,
    adc: sys::adc_oneshot_unit_handle_t,
    rmt: sys::rmt_channel_handle_t,
    rmt_encoder: sys::rmt_encoder_handle_t,
    boot_us: i64,
    mac: String,
    bus_pins: HashMap<u8, (i32, i32, u32)>,
}

// The handles are opaque pointers owned by ESP-IDF. They are only used from the
// App mutex, which serialises every call, so moving EspHw between threads is safe.
unsafe impl Send for EspHw {}

impl EspHw {
    pub fn new(buses: &[(u8, i32, i32, u32)]) -> anyhow::Result<Self> {
        let mut hw = Self {
            buses: HashMap::new(),
            devices: HashMap::new(),
            pwm: HashMap::new(),
            next_pwm_channel: 0,
            pwm_freq: 0,
            adc: std::ptr::null_mut(),
            rmt: std::ptr::null_mut(),
            rmt_encoder: std::ptr::null_mut(),
            boot_us: unsafe { sys::esp_timer_get_time() },
            mac: read_mac(),
            bus_pins: HashMap::new(),
        };
        for (bus, sda, scl, freq) in buses {
            if let Err(e) = hw.open_bus(*bus, *sda, *scl, *freq) {
                log::warn!("i2c bus {bus} (SDA {sda}, SCL {scl}) failed: {}", e.message);
            }
        }
        Ok(hw)
    }

    fn open_bus(&mut self, bus: u8, sda: i32, scl: i32, freq: u32) -> Result<()> {
        let mut cfg: sys::i2c_master_bus_config_t = unsafe { std::mem::zeroed() };
        cfg.i2c_port = if bus == 0 { 0 } else { 1 }; // i2c_port_t: 0 = I2C_NUM_0
        cfg.sda_io_num = sda;
        cfg.scl_io_num = scl;
        // The driver derives its SCL timing from this clock, so it must name a
        // source the chip's clock tree resolves. Zero only means "unset" and the
        // `SOC_MOD_CLK_*` values are chip specific, so ask for XTAL explicitly
        // rather than relying on the zeroed default: `SOC_I2C_CLKS` on the S3 is
        // {XTAL, RC_FAST}, and anything else fails `esp_clk_tree_src_get_freq_hz`
        // with "unknown clk src" and takes the whole bus down.
        // Assigning a union field is safe; only reading one is not, which is why
        // this is a plain assignment into the anonymous union.
        cfg.__bindgen_anon_1.clk_source = sys::soc_periph_i2c_clk_src_t_I2C_CLK_SRC_XTAL;
        cfg.glitch_ignore_cnt = 7;
        // A bare devkit header has no external pull-ups; without the internal
        // ones every probe on the bus reads as absent.
        cfg.flags.set_enable_internal_pullup(1);
        let mut handle: sys::i2c_master_bus_handle_t = std::ptr::null_mut();
        let rc = unsafe { sys::i2c_new_master_bus(&cfg, &mut handle) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("i2c_new_master_bus: {rc}")));
        }
        self.buses.insert(bus, handle);
        self.bus_pins.insert(bus, (sda, scl, freq));
        Ok(())
    }

    fn device(&mut self, bus: u8, addr: u8) -> Result<sys::i2c_master_dev_handle_t> {
        if let Some(h) = self.devices.get(&(bus, addr)) {
            return Ok(*h);
        }
        let bus_handle = *self
            .buses
            .get(&bus)
            .ok_or_else(|| McpError::hardware(format!("I2C bus {bus} is not open")))?;
        let freq = self.bus_pins.get(&bus).map(|b| b.2).unwrap_or(400_000);
        let mut cfg: sys::i2c_device_config_t = unsafe { std::mem::zeroed() };
        cfg.dev_addr_length = sys::i2c_addr_bit_len_t_I2C_ADDR_BIT_LEN_7;
        cfg.device_address = addr as u16;
        cfg.scl_speed_hz = freq;
        let mut dev: sys::i2c_master_dev_handle_t = std::ptr::null_mut();
        let rc = unsafe { sys::i2c_master_bus_add_device(bus_handle, &cfg, &mut dev) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("add_device 0x{addr:02X}: {rc}")));
        }
        self.devices.insert((bus, addr), dev);
        Ok(dev)
    }

    fn ensure_adc(&mut self) -> Result<sys::adc_oneshot_unit_handle_t> {
        if !self.adc.is_null() {
            return Ok(self.adc);
        }
        let mut cfg: sys::adc_oneshot_unit_init_cfg_t = unsafe { std::mem::zeroed() };
        cfg.unit_id = sys::adc_unit_t_ADC_UNIT_1;
        cfg.clk_src = sys::soc_periph_adc_rtc_clk_src_t_ADC_RTC_CLK_SRC_DEFAULT;
        let mut handle: sys::adc_oneshot_unit_handle_t = std::ptr::null_mut();
        let rc = unsafe { sys::adc_oneshot_new_unit(&cfg, &mut handle) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("adc_oneshot_new_unit: {rc}")));
        }
        self.adc = handle;
        Ok(handle)
    }

    fn pwm_apply(&mut self, pin: i32, freq_hz: u32, duty_pct: f32) -> Result<()> {
        let channel = match self.pwm.get(&pin) {
            Some(c) => *c,
            None => {
                let c = self.next_pwm_channel;
                if c >= MAX_PWM_CHANNELS {
                    return Err(McpError::hardware("no free LEDC channel"));
                }
                self.next_pwm_channel += 1;
                self.pwm.insert(pin, c);
                c
            }
        };
        if self.pwm_freq != freq_hz {
            let mut timer_cfg: sys::ledc_timer_config_t = unsafe { std::mem::zeroed() };
            timer_cfg.speed_mode = 0; // LEDC_LOW_SPEED_MODE
            timer_cfg.duty_resolution = 13; // LEDC_TIMER_13_BIT
            timer_cfg.timer_num = 0;
            timer_cfg.freq_hz = freq_hz;
            timer_cfg.clk_cfg = sys::soc_periph_ledc_clk_src_legacy_t_LEDC_AUTO_CLK;
            let rc = unsafe { sys::ledc_timer_config(&timer_cfg) };
            if rc != sys::ESP_OK {
                return Err(McpError::hardware(format!("ledc_timer_config: {rc}")));
            }
            self.pwm_freq = freq_hz;
        }
        let mut chan_cfg: sys::ledc_channel_config_t = unsafe { std::mem::zeroed() };
        chan_cfg.gpio_num = pin;
        chan_cfg.speed_mode = 0;
        chan_cfg.channel = channel;
        chan_cfg.timer_sel = 0;
        let rc = unsafe { sys::ledc_channel_config(&chan_cfg) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("ledc_channel_config: {rc}")));
        }
        let max = (1u32 << 13) - 1;
        let duty = ((duty_pct.clamp(0.0, 100.0) / 100.0) * max as f32) as u32;
        unsafe {
            sys::ledc_set_duty(0, channel, duty);
            sys::ledc_update_duty(0, channel);
        }
        Ok(())
    }
}

fn read_mac() -> String {
    let mut mac = [0u8; 6];
    unsafe { sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_WIFI_STA) };
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

impl Hardware for EspHw {
    fn board_info(&mut self) -> BoardInfo {
        // `esp_chip_info` is not in the generated bindings, so the SoC comes from
        // the build (MCU is set in .cargo/config.toml) and the rest from the
        // runtime heap queries that are available.
        let soc = option_env!("MCU").unwrap_or("esp32");
        let (cores, flash_mb, psram_kb) = match soc {
            "esp32s3" => (2, 16, 0), // PSRAM size is not queried: the symbol only exists when CONFIG_SPIRAM is on
            "esp32c3" => (1, 4, 0),
            "esp32c6" => (1, 8, 0),
            _ => (2, 4, 0),
        };
        BoardInfo {
            soc: soc.into(),
            cores,
            revision: 0,
            flash_mb,
            psram_kb,
            mac: self.mac.clone(),
            free_heap_kb: unsafe { sys::esp_get_free_heap_size() } / 1024,
            min_free_heap_kb: unsafe { sys::esp_get_minimum_free_heap_size() } / 1024,
            idf_version: String::from_utf8_lossy(
                unsafe { std::ffi::CStr::from_ptr(sys::esp_get_idf_version()) }.to_bytes(),
            )
            .to_string(),
            temp_c: None,
            firmware: env!("CARGO_PKG_VERSION").into(),
            build: "idf".into(),
        }
    }

    fn i2c_scan(&mut self, bus: u8) -> Result<Vec<u8>> {
        let handle = *self
            .buses
            .get(&bus)
            .ok_or_else(|| McpError::hardware(format!("I2C bus {bus} is not open")))?;
        let mut found = Vec::new();
        for addr in 0x08u8..=0x77 {
            let rc = unsafe { sys::i2c_master_probe(handle, addr as u16, I2C_TIMEOUT_MS) };
            if rc == sys::ESP_OK {
                found.push(addr);
            }
        }
        Ok(found)
    }

    fn i2c_read(&mut self, bus: u8, addr: u8, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
        let dev = self.device(bus, addr)?;
        if let Some(r) = reg {
            let rc = unsafe { sys::i2c_master_transmit(dev, &r, 1, I2C_TIMEOUT_MS) };
            if rc != sys::ESP_OK {
                return Err(McpError::hardware(format!("i2c write reg: {rc}")));
            }
        }
        let rc = unsafe { sys::i2c_master_receive(dev, out.as_mut_ptr(), out.len(), I2C_TIMEOUT_MS) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("i2c read: {rc}")));
        }
        Ok(())
    }

    fn i2c_write(&mut self, bus: u8, addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
        let dev = self.device(bus, addr)?;
        let mut buf = Vec::with_capacity(data.len() + 1);
        if let Some(r) = reg {
            buf.push(r);
        }
        buf.extend_from_slice(data);
        let rc = unsafe { sys::i2c_master_transmit(dev, buf.as_ptr(), buf.len(), I2C_TIMEOUT_MS) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("i2c write: {rc}")));
        }
        Ok(())
    }

    fn gpio_config(&mut self, pin: i32, mode: PinMode) -> Result<()> {
        let mut cfg: sys::gpio_config_t = unsafe { std::mem::zeroed() };
        cfg.pin_bit_mask = 1u64 << (pin as u32);
        match mode {
            PinMode::Input => cfg.mode = sys::gpio_mode_t_GPIO_MODE_INPUT,
            PinMode::InputPullup => {
                cfg.mode = sys::gpio_mode_t_GPIO_MODE_INPUT;
                cfg.pull_up_en = sys::gpio_pullup_t_GPIO_PULLUP_ENABLE;
            }
            PinMode::InputPulldown => {
                cfg.mode = sys::gpio_mode_t_GPIO_MODE_INPUT;
                cfg.pull_down_en = sys::gpio_pulldown_t_GPIO_PULLDOWN_ENABLE;
            }
            PinMode::Output => cfg.mode = sys::gpio_mode_t_GPIO_MODE_OUTPUT,
        }
        let rc = unsafe { sys::gpio_config(&cfg) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("gpio_config({pin}): {rc}")));
        }
        Ok(())
    }

    fn gpio_read(&mut self, pin: i32) -> Result<bool> {
        Ok(unsafe { sys::gpio_get_level(pin) } != 0)
    }

    fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
        let rc = unsafe { sys::gpio_set_level(pin, if high { 1 } else { 0 }) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("gpio_set_level({pin}): {rc}")));
        }
        Ok(())
    }

    fn pwm_set(&mut self, pin: i32, freq_hz: u32, duty_pct: f32) -> Result<()> {
        self.pwm_apply(pin, freq_hz.max(1), duty_pct)
    }

    fn adc_mv(&mut self, pin: i32) -> Result<u32> {
        let unit = self.ensure_adc()?;
        let mut chan_cfg: sys::adc_oneshot_chan_cfg_t = unsafe { std::mem::zeroed() };
        chan_cfg.atten = sys::adc_atten_t_ADC_ATTEN_DB_12;
        chan_cfg.bitwidth = sys::adc_bitwidth_t_ADC_BITWIDTH_12;
        let rc = unsafe { sys::adc_oneshot_config_channel(unit, pin as u32, &chan_cfg) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("adc_config_channel({pin}): {rc}")));
        }
        let mut raw: i32 = 0;
        let rc = unsafe { sys::adc_oneshot_read(unit, pin as u32, &mut raw) };
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("adc_read({pin}): {rc}")));
        }
        // Uncalibrated but monotonic: 12 dB attenuation is ~0..3.1 V full scale.
        Ok(((raw as u32) * 3100) / 4095)
    }

    fn ring_set(&mut self, _pin: i32, _rgb: (u8, u8, u8), _count: u16) -> Result<()> {
        // TODO(rmt): the rmt_symbol_word_t bitfield layout differs between IDF
        // versions; the WS2812 path is postponed rather than guessed.
        Err(McpError::unsupported("addressable RGB is not implemented on this firmware build"))
    }
    fn delay_us(&mut self, us: u32) {
        unsafe { sys::esp_rom_delay_us(us) };
    }

    fn millis(&mut self) -> u64 {
        (unsafe { sys::esp_timer_get_time() } - self.boot_us) as u64 / 1000
    }

    fn wifi_status(&mut self) -> WifiStatus {
        wifi::status()
    }

    fn wifi_scan(&mut self) -> Result<Vec<WifiAp>> {
        wifi::scan()
    }

    fn wifi_connect(&mut self, ssid: &str, pw: &str) -> Result<()> {
        wifi::command(wifi::WifiCmd::Connect {
            ssid: ssid.to_string(),
            password: pw.to_string(),
        })
    }

    fn reboot(&mut self) {
        log::warn!("restarting");
        unsafe { sys::esp_restart() };
    }

    fn inventory_extra(&mut self) -> Value {
        json!({
            "buses": self.bus_pins.iter().map(|(b, (sda, scl, f))| json!({
                "bus": b, "sda": sda, "scl": scl, "freq_hz": f,
            })).collect::<Vec<Value>>(),
            "pwm_channels": self.pwm,
        })
    }
}



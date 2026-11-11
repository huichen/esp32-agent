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
    /// Which pin the RMT channel is bound to; a channel serves one output.
    rmt_pin: i32,
    boot_us: i64,
    mac: String,
    bus_pins: HashMap<u8, (i32, i32, u32)>,
    /// I2S channel handles; null until `i2s_setup` brings them up.
    i2s_rx: sys::i2s_chan_handle_t,
    i2s_tx: sys::i2s_chan_handle_t,
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
            rmt_pin: -1,
            boot_us: unsafe { sys::esp_timer_get_time() },
            mac: read_mac(),
            bus_pins: HashMap::new(),
            i2s_rx: std::ptr::null_mut(),
            i2s_tx: std::ptr::null_mut(),
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

    /// Drive a WS2812 chain with the RMT bytes encoder.
    ///
    /// Zero-plus-one encoding at 10 MHz: a `1` bit is 0.9 us high / 0.3 us low and a `0`
    /// bit the other way round, i.e. 9/3 and 3/9 ticks. `rmt_new_bytes_encoder` expands
    /// each payload byte into those symbols, so the buffer is plain WS2812 colour bytes in
    /// the order the part expects: green, red, blue.
    fn ring_set(&mut self, pin: i32, rgb: (u8, u8, u8), count: u16) -> Result<()> {
        if self.rmt.is_null() || self.rmt_pin != pin {
            self.ring_channel(pin)?;
        }
        let mut bytes: Vec<u8> = Vec::with_capacity(count as usize * 3);
        for _ in 0..count.max(1) {
            bytes.push(rgb.1);
            bytes.push(rgb.0);
            bytes.push(rgb.2);
        }
        unsafe {
            let cfg: sys::rmt_transmit_config_t = std::mem::zeroed();
            let rc = sys::rmt_transmit(
                self.rmt,
                self.rmt_encoder,
                bytes.as_ptr() as *const core::ffi::c_void,
                bytes.len(),
                &cfg,
            );
            if rc != sys::ESP_OK {
                return Err(McpError::hardware(format!("rmt_transmit: {rc}")));
            }
            // ~30 us per pixel plus the latch; 100 ms covers a long strip generously.
            let _ = sys::rmt_tx_wait_all_done(self.rmt, 100);
        }
        Ok(())
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

    // ------------------------------------------------------------------ I2S audio

    /// Bring up the two I2S channels with the formats measured on this board.
    ///
    /// Microphone: 16 kHz, 32-bit slots, left channel only (the INMP441 sends 24 bits
    /// left-justified and the right slot stays zero). Amplifier: 24 kHz, 16-bit, Philips.
    /// Both are master, no MCLK pin — the boards wire BCLK/WS and one data line.
    fn i2s_setup(
        &mut self,
        mic: Option<esp32_agent_core::boards::I2sPins>,
        amp: Option<esp32_agent_core::boards::I2sPins>,
    ) -> Result<()> {
        if let Some(pins) = mic {
            self.i2s_rx = i2s_channel(true, 0, 16_000, 32, pins.bclk, pins.ws, pins.din)?;
        }
        if let Some(pins) = amp {
            self.i2s_tx = i2s_channel(false, 1, 24_000, 16, pins.bclk, pins.ws, pins.dout)?;
        }
        Ok(())
    }

    /// Read 32-bit samples.
    ///
    /// The return code is **not** the judge: on this board `i2s_channel_read` answers
    /// `ESP_ERR_TIMEOUT` while `bytes_read` already holds samples, and counting only on
    /// `ESP_OK` is what makes a working microphone read as silence. So the count decides.
    fn i2s_mic_read(&mut self, out: &mut [i32], timeout_ms: u32) -> Result<usize> {
        if self.i2s_rx.is_null() {
            return Err(McpError::unsupported("no I2S microphone on this board"));
        }
        let mut got: usize = 0;
        let bytes = out.len() * core::mem::size_of::<i32>();
        let _rc = unsafe {
            sys::i2s_channel_read(
                self.i2s_rx,
                out.as_mut_ptr() as *mut core::ffi::c_void,
                bytes,
                &mut got,
                timeout_ms,
            )
        };
        Ok(got / core::mem::size_of::<i32>())
    }

    fn i2s_out_write(&mut self, samples: &[i16]) -> Result<()> {
        if self.i2s_tx.is_null() {
            return Err(McpError::unsupported("no I2S speaker on this board"));
        }
        let mut written: usize = 0;
        let bytes = samples.len() * core::mem::size_of::<i16>();
        let rc = unsafe {
            sys::i2s_channel_write(
                self.i2s_tx,
                samples.as_ptr() as *const core::ffi::c_void,
                bytes,
                &mut written,
                1000,
            )
        };
        if written == 0 {
            return Err(McpError::hardware(format!("i2s write accepted nothing (rc {rc})")));
        }
        // A short write is normal under load; the caller only needs to know it went out.
        Ok(())
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



// ---------------------------------------------------------------- I2S helpers

/// Create and start one standard-mode I2S channel.
///
/// `bits` is the slot width: 32 for the microphone (24 bits left-justified in a 32-bit
/// slot), 16 for the amplifier. Mono, Philips, master role, no MCLK pin — that is how
/// both parts are wired on this board.
fn i2s_channel(
    rx: bool,
    port: i32,
    rate_hz: u32,
    bits: u8,
    bclk: i32,
    ws: i32,
    data: i32,
) -> Result<sys::i2s_chan_handle_t> {
    unsafe {
        let mut chan: sys::i2s_chan_config_t = std::mem::zeroed();
        chan.id = if port == 0 { sys::i2s_port_t_I2S_NUM_0 } else { sys::i2s_port_t_I2S_NUM_1 };
        chan.role = sys::i2s_role_t_I2S_ROLE_MASTER;
        chan.dma_desc_num = 4;
        chan.dma_frame_num = 320;
        let mut handle: sys::i2s_chan_handle_t = std::ptr::null_mut();
        let rc = if rx {
            sys::i2s_new_channel(&chan, std::ptr::null_mut(), &mut handle)
        } else {
            sys::i2s_new_channel(&chan, &mut handle, std::ptr::null_mut())
        };
        if rc != sys::ESP_OK || handle.is_null() {
            return Err(McpError::hardware(format!("i2s_new_channel({port}): {rc}")));
        }

        let mut cfg: sys::i2s_std_config_t = std::mem::zeroed();
        cfg.clk_cfg.sample_rate_hz = rate_hz;
        cfg.clk_cfg.clk_src = sys::soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT;
        cfg.clk_cfg.mclk_multiple = sys::i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256;
        cfg.slot_cfg.slot_mode = sys::i2s_slot_mode_t_I2S_SLOT_MODE_MONO;
        cfg.slot_cfg.slot_mask = sys::i2s_std_slot_mask_t_I2S_STD_SLOT_LEFT;
        cfg.slot_cfg.ws_width = bits as u32;
        if bits == 32 {
            cfg.slot_cfg.data_bit_width = sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_32BIT;
            cfg.slot_cfg.slot_bit_width = sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_32BIT;
        } else {
            cfg.slot_cfg.data_bit_width = sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT;
            cfg.slot_cfg.slot_bit_width = sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_16BIT;
        }
        cfg.gpio_cfg.mclk = -1;
        cfg.gpio_cfg.bclk = bclk;
        cfg.gpio_cfg.ws = ws;
        cfg.gpio_cfg.din = if rx { data } else { -1 };
        cfg.gpio_cfg.dout = if rx { -1 } else { data };

        let rc = sys::i2s_channel_init_std_mode(handle, &cfg);
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("i2s_channel_init_std_mode({port}): {rc}")));
        }
        let rc = sys::i2s_channel_enable(handle);
        if rc != sys::ESP_OK {
            return Err(McpError::hardware(format!("i2s_channel_enable({port}): {rc}")));
        }
        Ok(handle)
    }
}
// ---------------------------------------------------------------- WS2812 helpers

impl EspHw {
    /// Create the RMT transmit channel and its WS2812 byte encoder for one pin.
    ///
    /// 10 MHz resolution turns the part's 0.3/0.9 us bits into 3/9 and 9/3 ticks. A
    /// channel is bound to one GPIO, so changing the output pin rebuilds it.
    fn ring_channel(&mut self, pin: i32) -> Result<()> {
        unsafe {
            if !self.rmt.is_null() {
                let _ = sys::rmt_disable(self.rmt);
                sys::rmt_del_channel(self.rmt);
                self.rmt = std::ptr::null_mut();
            }
            let mut ch: sys::rmt_tx_channel_config_t = std::mem::zeroed();
            ch.gpio_num = pin;
            ch.clk_src = sys::soc_periph_rmt_clk_src_t_RMT_CLK_SRC_DEFAULT;
            ch.resolution_hz = 10_000_000;
            ch.mem_block_symbols = 64usize;
            ch.trans_queue_depth = 4;
            let mut handle: sys::rmt_channel_handle_t = std::ptr::null_mut();
            let rc = sys::rmt_new_tx_channel(&ch, &mut handle);
            if rc != sys::ESP_OK || handle.is_null() {
                return Err(McpError::hardware(format!("rmt_new_tx_channel({pin}): {rc}")));
            }
            let rc = sys::rmt_enable(handle);
            if rc != sys::ESP_OK {
                return Err(McpError::hardware(format!("rmt_enable: {rc}")));
            }

            let mut be: sys::rmt_bytes_encoder_config_t = std::mem::zeroed();
            be.bit0 = ws2812_symbol(3, 9);
            be.bit1 = ws2812_symbol(9, 3);
            be.flags.set_msb_first(1);
            let mut enc: sys::rmt_encoder_handle_t = std::ptr::null_mut();
            let rc = sys::rmt_new_bytes_encoder(&be, &mut enc);
            if rc != sys::ESP_OK || enc.is_null() {
                return Err(McpError::hardware(format!("rmt_new_bytes_encoder: {rc}")));
            }
            self.rmt = handle;
            self.rmt_encoder = enc;
            self.rmt_pin = pin;
            Ok(())
        }
    }
}

/// One WS2812 symbol: `high` ticks at level 1, then `low` ticks at level 0.
fn ws2812_symbol(high: u16, low: u16) -> sys::rmt_symbol_word_t {
    let mut s: sys::rmt_symbol_word_t = unsafe { std::mem::zeroed() };
    unsafe {
        s.__bindgen_anon_1.set_level0(1);
        s.__bindgen_anon_1.set_duration0(high);
        s.__bindgen_anon_1.set_level1(0);
        s.__bindgen_anon_1.set_duration1(low);
    }
    s
}
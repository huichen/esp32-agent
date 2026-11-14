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
use esp32_agent_core::hardware::{BoardInfo, Hardware, HeapPool, PinMode, WifiAp, WifiStatus};
use esp_idf_sys as sys;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::ToSocketAddrs;

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
    /// The chip's own temperature sensor, installed once. Null when the part has none
    /// (ESP32-C3) or the driver refused, which is why the reading is an `Option`.
    tsens: sys::temperature_sensor_handle_t,
    i2s_rx: sys::i2s_chan_handle_t,
    i2s_tx: sys::i2s_chan_handle_t,
    /// The background player: it owns the transmit channel's timing, and this struct only
    /// hands it buffers (see `media.rs`).
    player: std::sync::Arc<crate::media::Player>,
    /// Held while a blocking write is in flight, so the two writers cannot interleave.
    media_writing: std::sync::Mutex<()>,
    /// The panel the background player draws on, created the first time a job needs it.
    /// `None` until then, so a board with no display pays nothing.
    panel: std::sync::Mutex<Option<std::sync::Arc<crate::media::Panel>>>,
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
            tsens: std::ptr::null_mut(),
            i2s_rx: std::ptr::null_mut(),
            i2s_tx: std::ptr::null_mut(),
            player: crate::media::Player::new(),
            media_writing: std::sync::Mutex::new(()),
            panel: std::sync::Mutex::new(None),
        };
        for (bus, sda, scl, freq) in buses {
            if let Err(e) = hw.open_bus(*bus, *sda, *scl, *freq) {
                log::warn!("i2c bus {bus} (SDA {sda}, SCL {scl}) failed: {}", e.message);
            }
        }
        // The on-die temperature sensor: one install, enabled for the life of the board.
        // `board_status` used to report `temperature_c: null` on a part that has one,
        // which reads as "the firmware is broken" rather than "nothing asked the driver".
        match hw.install_temp_sensor() {
            Ok(()) => log::info!("chip temperature sensor installed"),
            Err(e) => log::warn!("chip temperature sensor: {e}"),
        }
        Ok(hw)
    }

    fn install_temp_sensor(&mut self) -> anyhow::Result<()> {
        unsafe {
            // The range is the driver's expected span, not a measurement; -10..80 covers a
            // room, a bench and a warm enclosure.
            let cfg = sys::temperature_sensor_config_t {
                range_min: -10,
                range_max: 80,
                clk_src: sys::soc_periph_temperature_sensor_clk_src_t_TEMPERATURE_SENSOR_CLK_SRC_RC_FAST,
                flags: sys::temperature_sensor_config_t__bindgen_ty_1 { allow_pd: 0 },
            };
            let mut handle: sys::temperature_sensor_handle_t = std::ptr::null_mut();
            esp_idf_svc::sys::esp!(sys::temperature_sensor_install(&cfg, &mut handle))
                .map_err(|e| anyhow::anyhow!("temperature_sensor_install: {e}"))?;
            esp_idf_svc::sys::esp!(sys::temperature_sensor_enable(handle))
                .map_err(|e| anyhow::anyhow!("temperature_sensor_enable: {e}"))?;
            self.tsens = handle;
        }
        Ok(())
    }

    /// One reading, or `None` when the driver is not there or the read fails.
    fn chip_temperature(&mut self) -> Option<f32> {
        if self.tsens.is_null() {
            return None;
        }
        let mut celsius: f32 = 0.0;
        let rc = unsafe { sys::temperature_sensor_get_celsius(self.tsens, &mut celsius) };
        if rc == 0 {
            Some(celsius)
        } else {
            None
        }
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
        let bus_handle =
            *self.buses.get(&bus).ok_or_else(|| McpError::hardware(format!("I2C bus {bus} is not open")))?;
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
    fn chip_temp_c(&mut self) -> Option<f32> {
        self.chip_temperature()
    }

    /// Running tasks: name, state, priority and the stack high-water mark.
    ///
    /// `uxTaskGetSystemState` suspends the scheduler while it copies, so it is a debug aid
    /// — which is exactly what `ps` is.
    fn tasks(&mut self) -> Vec<serde_json::Value> {
        use serde_json::json;
        unsafe {
            let count = sys::uxTaskGetNumberOfTasks();
            if count == 0 || count > 40 {
                return Vec::new();
            }
            let mut items: Vec<sys::TaskStatus_t> = Vec::with_capacity(count as usize);
            let mut total_runtime: u32 = 0;
            let filled = sys::uxTaskGetSystemState(items.as_mut_ptr(), count, &mut total_runtime);
            if filled == 0 {
                return Vec::new();
            }
            items.set_len(filled as usize);
            let labels = crate::threads::task_labels();
            items
                .iter()
                .map(|t| {
                    // Only scalar snapshot fields are lifetime-safe after the kernel
                    // releases its task-list lock. Never dereference pcTaskName or
                    // query xHandle: a request worker may have exited meanwhile.
                    let label = labels.get(&(t.xTaskNumber as u32));
                    let name = label
                        .map(|(name, _)| name.clone())
                        .unwrap_or_else(|| format!("task-{}", t.xTaskNumber));
                    let state = match t.eCurrentState {
                        0 => "running",
                        1 => "ready",
                        2 => "blocked",
                        3 => "suspended",
                        4 => "deleted",
                        _ => "?",
                    };
                    json!({
                        "name": name,
                        "state": state,
                        "priority": t.uxCurrentPriority,
                        // Bytes, not words: this port's `StackType_t` is `uint8_t`, so the
                        // high-water mark is already a byte count. Calling it "words" sent
                        // one reading of this table to the wrong conclusion.
                        "stack_free_bytes": t.usStackHighWaterMark,
                        "task_id": t.xTaskNumber,
                        // xTaskGetCoreID reports affinity, not the currently executing
                        // core. Known labels capture it from the live task itself;
                        // unknown IDF/request tasks report null, not a stale handle read.
                        "core": label.map(|(_, affinity)| *affinity),
                        "core_known": label.is_some(),
                    })
                })
                .collect()
        }
    }

    /// ICMP echo through ESP-IDF's ping session.
    ///
    /// One session per call: the driver runs its own task and reports through profiles,
    /// so this side polls `esp_ping_get_profile` until the echoes arrive or the bound runs
    /// out. Three echoes half a second apart is what a tool result can afford.
    fn ping(&mut self, host: &str, count: u32) -> Result<serde_json::Value> {
        use esp32_agent_core::error::McpError;
        use serde_json::json;

        // Resolve first: the config wants an address, and a name that does not resolve
        // should say so instead of timing out and looking like an unreachable host.
        let mut addrs = (host, 0u16)
            .to_socket_addrs()
            .map_err(|e| McpError::hardware(format!("could not resolve `{host}`: {e}")))?;
        let addr = addrs.next().ok_or_else(|| McpError::hardware(format!("`{host}` resolved to nothing")))?;
        let ip = match addr.ip() {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => return Err(McpError::unsupported("ping supports IPv4 targets only")),
        };
        let count = count.clamp(1, 5);

        unsafe {
            let mut target: sys::ip_addr_t = std::mem::zeroed();
            target.u_addr.ip4.addr = u32::from_ne_bytes(ip.octets());
            let config = sys::esp_ping_config_t {
                count,
                interval_ms: 500,
                timeout_ms: 900,
                data_size: 32,
                tos: 0,
                ttl: 64,
                target_addr: target,
                interface: 0,
                task_stack_size: 4096,
                task_prio: 2,
            };
            let callbacks: sys::esp_ping_callbacks_t = std::mem::zeroed();
            let mut handle: sys::esp_ping_handle_t = std::ptr::null_mut();
            esp_idf_svc::sys::esp!(sys::esp_ping_new_session(&config, &callbacks, &mut handle))
                .map_err(|e| McpError::hardware(format!("esp_ping_new_session: {e}")))?;
            esp_idf_svc::sys::esp!(sys::esp_ping_start(handle))
                .map_err(|e| McpError::hardware(format!("esp_ping_start: {e}")))?;

            let read_profile = |handle: sys::esp_ping_handle_t, which: sys::esp_ping_profile_t| -> u32 {
                let mut value = 0u32;
                let rc = sys::esp_ping_get_profile(
                    handle,
                    which,
                    &mut value as *mut u32 as *mut core::ffi::c_void,
                    std::mem::size_of::<u32>() as u32,
                );
                if rc == 0 {
                    value
                } else {
                    0
                }
            };

            let mut received = 0u32;
            for _ in 0..(count * 4) {
                std::thread::sleep(std::time::Duration::from_millis(250));
                received = read_profile(handle, sys::esp_ping_profile_t_ESP_PING_PROF_REPLY);
                if received >= count {
                    break;
                }
            }
            let sent = read_profile(handle, sys::esp_ping_profile_t_ESP_PING_PROF_REQUEST);
            let last_rtt = read_profile(handle, sys::esp_ping_profile_t_ESP_PING_PROF_TIMEGAP);

            let _ = sys::esp_ping_stop(handle);
            let _ = sys::esp_ping_delete_session(handle);

            Ok(json!({
                "host": host,
                "address": ip.to_string(),
                "sent": sent.max(count),
                "received": received,
                "last_rtt_ms": last_rtt,
                "reachable": received > 0,
            }))
        }
    }

    fn board_info(&mut self) -> BoardInfo {
        // `esp_chip_info` is not in the generated bindings, so the SoC comes from
        // the build (MCU is set in .cargo/config.toml) and the rest from the
        // runtime heap queries that are available.
        let soc = option_env!("MCU").unwrap_or("esp32");
        let (cores, flash_mb) = match soc {
            "esp32s3" => (2, 16),
            "esp32c3" => (1, 4),
            "esp32c6" => (1, 8),
            _ => (2, 4),
        };
        // PSRAM is only mapped when CONFIG_SPIRAM is on, and the getter only exists then.
        // Reporting 0 on a board with 8 MB was wrong in `/api/status` and in `uname`.
        let psram_kb = unsafe {
            let bytes = sys::esp_psram_get_size();
            if bytes > 0 {
                (bytes / 1024) as u32
            } else {
                0
            }
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
            temp_c: self.chip_temperature(),
            firmware: env!("CARGO_PKG_VERSION").into(),
            build: "idf".into(),
        }
    }

    fn i2c_scan(&mut self, bus: u8) -> Result<Vec<u8>> {
        let handle =
            *self.buses.get(&bus).ok_or_else(|| McpError::hardware(format!("I2C bus {bus} is not open")))?;
        let mut found = Vec::new();
        for addr in 0x08u8..=0x77 {
            let first = unsafe { sys::i2c_master_probe(handle, addr as u16, I2C_TIMEOUT_MS) };
            if first != sys::ESP_OK {
                continue;
            }
            // One ACK is not evidence on a breadboard bus: a glitch on SDA during the
            // address byte's acknowledge bit reads as a device, and the same scan then
            // reports 0x51, 0x73, 0x0D or nothing at all from one run to the next. The
            // agent builds its picture of the hardware from this list, so an address has
            // to answer twice to be reported.
            esp_idf_svc::hal::delay::FreeRtos::delay_ms(1);
            let second = unsafe { sys::i2c_master_probe(handle, addr as u16, I2C_TIMEOUT_MS) };
            if second == sys::ESP_OK {
                found.push(addr);
            }
        }
        Ok(found)
    }

    fn i2c_read(&mut self, bus: u8, addr: u8, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
        let mut last = sys::ESP_OK;
        for attempt in 0..2 {
            let dev = self.device(bus, addr)?;
            if let Some(r) = reg {
                last = unsafe { sys::i2c_master_transmit(dev, &r, 1, I2C_TIMEOUT_MS) };
                if last != sys::ESP_OK {
                    if attempt == 0 {
                        self.reset_bus(bus);
                        continue;
                    }
                    return Err(McpError::hardware(format!("i2c write reg: {last}")));
                }
            }
            last = unsafe { sys::i2c_master_receive(dev, out.as_mut_ptr(), out.len(), I2C_TIMEOUT_MS) };
            if last == sys::ESP_OK {
                return Ok(());
            }
            if attempt == 0 {
                self.reset_bus(bus);
            }
        }
        Err(McpError::hardware(format!("i2c read: {last}")))
    }

    fn i2c_write(&mut self, bus: u8, addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(data.len() + 1);
        if let Some(r) = reg {
            buf.push(r);
        }
        buf.extend_from_slice(data);
        let mut last = sys::ESP_OK;
        for attempt in 0..2 {
            let dev = self.device(bus, addr)?;
            last = unsafe { sys::i2c_master_transmit(dev, buf.as_ptr(), buf.len(), I2C_TIMEOUT_MS) };
            if last == sys::ESP_OK {
                return Ok(());
            }
            // A jumper that glitches mid-byte leaves the driver's bus state invalid
            // (`ESP_ERR_INVALID_STATE`) and then *every* later transfer fails until the
            // peripheral is reset - the screen goes dead after one bad frame on a
            // breadboard, which is exactly how this was found. Reset and try once more.
            if attempt == 0 {
                self.reset_bus(bus);
            }
        }
        Err(McpError::hardware(format!("i2c write: {last}")))
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

    /// The fallible form a bit-banged driver asks for. The ROM busy-wait *is* the
    /// microsecond timing, so this board never refuses one.
    fn delay_us_checked(&mut self, us: u32) -> Result<()> {
        self.delay_us(us);
        Ok(())
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
    /// Both are master, no MCLK pin: the boards wire BCLK/WS and one data line.
    fn i2s_setup(
        &mut self,
        mic: Option<esp32_agent_core::boards::I2sPins>,
        amp: Option<esp32_agent_core::boards::I2sPins>,
    ) -> Result<()> {
        // Every `manifest_apply` rebuilds the device list, so this runs again on a board whose
        // channels are already open. `i2s_new_channel` fails on a port in use, and a failure
        // here drops the mic and the speaker from the list until the next reboot - so an open
        // channel is left alone rather than recreated.
        if mic.is_some() && self.i2s_rx.is_null() {
            let pins = mic.unwrap();
            self.i2s_rx = i2s_channel(true, 0, 16_000, 32, pins.bclk, pins.ws, pins.din)?;
        }
        if amp.is_some() && self.i2s_tx.is_null() {
            let pins = amp.unwrap();
            self.i2s_tx = i2s_channel(false, 1, 24_000, 16, pins.bclk, pins.ws, pins.dout)?;
            // The player writes to the same channel from its own task; it needs the handle
            // and one start, both of which are idempotent so a rebuilt device list is safe.
            self.player.set_channel(self.i2s_tx);
            crate::media::start(self.player.clone());
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
        // The channel is stereo (see `i2s_channel`): 8 bytes per frame, the microphone's
        // data in the left word and zeros in the right. Read the frames and keep the left
        // word; reading it as mono moves the bit alignment half a frame and turns every
        // sample into full scale, which is exactly what the first attempt did.
        let mut got: usize = 0;
        let mut raw = vec![0i32; out.len().saturating_mul(2)];
        let _rc = unsafe {
            sys::i2s_channel_read(
                self.i2s_rx,
                raw.as_mut_ptr() as *mut core::ffi::c_void,
                raw.len() * core::mem::size_of::<i32>(),
                &mut got,
                timeout_ms,
            )
        };
        let frames = got / (2 * core::mem::size_of::<i32>());
        for i in 0..frames.min(out.len()) {
            out[i] = raw[i * 2];
        }
        Ok(frames.min(out.len()))
    }

    fn i2s_out_write(&mut self, samples: &[i16]) -> Result<()> {
        if self.i2s_tx.is_null() {
            return Err(McpError::unsupported("no I2S speaker on this board"));
        }
        // One writer at a time: the background player may be mid-chunk, and two streams
        // interleaved into one DMA ring is noise, not a race that resolves itself.
        let _busy = self.media_writing.lock().unwrap_or_else(|p| p.into_inner());
        crate::media::write_chunk(self.i2s_tx, samples)
    }

    fn media_submit(&mut self, samples: &[i16], rate_hz: u32) -> Result<u64> {
        self.player.submit(samples, rate_hz)
    }

    fn media_state(&mut self) -> esp32_agent_core::hardware::MediaState {
        self.player.state()
    }

    fn media_cancel(&mut self) -> bool {
        self.player.cancel()
    }

    /// Asynchronous audio **and** video: the frames arrive pre-rendered, so this only has to
    /// hand the player a way to reach the glass without the App lock.
    ///
    /// That way is one `Panel` (its own I2C device handle plus the bus mutex the blocking
    /// path also takes). It is created the first time a job needs it and cached: reopening a
    /// device handle per tune would be a leak with a polite name.
    fn media_submit_av(
        &mut self,
        samples: &[i16],
        rate_hz: u32,
        pictures: &[esp32_agent_core::hardware::MediaPicture],
        panel: Option<(u8, u8)>,
    ) -> Result<u64> {
        let (bus, addr) = panel
            .ok_or_else(|| McpError::invalid_params("an asynchronous frame needs a panel to draw on"))?;
        let handle = self.device(bus, addr)? as usize;
        let shared = {
            let mut slot = self.panel.lock().unwrap_or_else(|p| p.into_inner());
            slot.get_or_insert_with(|| std::sync::Arc::new(crate::media::Panel::new(handle))).clone()
        };
        let pages: Vec<(u64, Vec<u8>)> = pictures.iter().map(|p| (p.at_ms, p.page.clone())).collect();
        self.player.submit_av(samples, rate_hz, &pages, Some(shared))
    }

    fn wifi_set_ap(&mut self, open: bool) -> Result<()> {
        wifi::command(wifi::WifiCmd::SetAp { open })
            .map_err(|e| McpError::hardware(format!("{e}")))
    }

    fn wifi_scan(&mut self) -> Result<Vec<WifiAp>> {
        wifi::scan()
    }

    fn wifi_connect(&mut self, ssid: &str, pw: &str) -> Result<()> {
        wifi::command(wifi::WifiCmd::Connect { ssid: ssid.to_string(), password: pw.to_string() })
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

    /// Internal SRAM left, and the largest block a thread stack or a TLS buffer can use.
    ///
    /// The same flags as the `internal_8bit` pool, deliberately. `esp_get_free_internal_heap_size`
    /// answers a slightly larger number because it counts every internal heap region, including
    /// the IRAM-only bytes that no stack, queue or socket buffer can use. Two numbers for one
    /// question — 142 kB here and 150 kB in the status document's `memory.pools` — is how a reader concludes the report
    /// is broken; this one is the pool a caller can actually allocate from.
    fn heap_internal(&mut self) -> Option<(u32, u32)> {
        use esp_idf_svc::sys;
        let mut info: sys::multi_heap_info_t = unsafe { std::mem::zeroed() };
        unsafe { sys::heap_caps_get_info(&mut info, sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT) };
        Some((info.total_free_bytes as u32, info.largest_free_block as u32))
    }

    /// Whether *this* thread can write flash itself: its stack has to be in internal RAM.
    ///
    /// `spi_flash` runs with both caches disabled, and the CPU cannot then fetch from PSRAM, so
    /// ESP-IDF asserts that the calling task's stack is internal. A thread given a PSRAM stack
    /// (the DNS one) would abort instead. Asking is one instruction and the answer is exact.
    fn stack_is_internal(&self) -> bool {
        use esp_idf_svc::sys;
        let marker = 0u8;
        let here = &marker as *const u8 as *const core::ffi::c_void;
        !unsafe { sys::esp_ptr_external_ram(here) }
    }

    /// The largest single allocation PSRAM can satisfy, in bytes: the number a buffer has to
    /// fit in, as opposed to the total that `free_heap_kb` reports.
    fn psram_largest_free(&mut self) -> Option<usize> {
        use esp_idf_svc::sys;
        let block = unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_SPIRAM) };
        if block == 0 {
            None
        } else {
            Some(block)
        }
    }

    /// PSRAM only, deliberately: a clip must not take the internal RAM that stacks, DMA and a
    /// flash write need. A refusal is the honest answer, and the caller reports the largest
    /// block it could see.
    fn media_buffer(&mut self, len: usize) -> Option<(Vec<u8>, &'static str)> {
        use esp_idf_svc::sys;
        if len == 0 {
            return Some((Vec::new(), "psram"));
        }
        let caps = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
        let raw = unsafe { sys::heap_caps_malloc(len, caps) } as *mut u8;
        if raw.is_null() {
            return None;
        }
        // `heap_caps_malloc` and Rust's global allocator are the same heap in ESP-IDF, so a
        // `Vec` built from these parts frees the way every other `Vec` does.
        Some((unsafe { Vec::from_raw_parts(raw, 0, len) }, "psram"))
    }

    /// One tick, so the idle task on this core can run and reset the watchdog.
    ///
    /// `taskYIELD` is not enough: it hands the CPU to a task of equal priority, and the idle task
    /// is below every other one here. A delayed tick is what actually lets it run.
    fn let_others_run(&mut self) {
        esp_idf_svc::hal::delay::FreeRtos::delay_ms(1);
    }

    /// The heap, split the way ESP-IDF splits it: one row per set of capability flags.
    ///
    /// `heap_caps_get_info` answers a pool by walking every heap region that satisfies the
    /// flags, so the rows overlap on purpose — internal RAM is the DMA pool plus more, and
    /// PSRAM is a pool of its own. That overlap is the fact the old three-number reading
    /// could not express: "free heap is 8 MB" and "a thread stack does not fit" were both
    /// true at once.
    ///
    /// A pool this chip does not register (IRAM as data, on the S3) is left out rather than
    /// reported as a row of zeros, which a reader would take for "exhausted".
    fn heap_pools(&mut self) -> Vec<HeapPool> {
        use esp_idf_svc::sys;
        let caps: [(&str, u32, &str); 4] = [
            (
                "internal_8bit",
                sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT,
                "thread stacks, queues and anything that must survive with the cache off",
            ),
            (
                "dma",
                sys::MALLOC_CAP_DMA | sys::MALLOC_CAP_8BIT,
                "descriptors and buffers a peripheral reads or writes",
            ),
            ("iram_8bit", sys::MALLOC_CAP_IRAM_8BIT, "internal RAM that allows unaligned access"),
            ("spiram", sys::MALLOC_CAP_SPIRAM, "PSRAM: allocation only, no stacks, no DMA descriptors"),
        ];
        caps.iter()
            .filter_map(|(pool, flags, note)| {
                let mut info: sys::multi_heap_info_t = unsafe { std::mem::zeroed() };
                unsafe { sys::heap_caps_get_info(&mut info, *flags) };
                let total = info.total_free_bytes + info.total_allocated_bytes;
                if total == 0 {
                    return None;
                }
                Some(HeapPool {
                    pool: (*pool).into(),
                    note: (*note).into(),
                    total_kb: (total / 1024) as u32,
                    free_kb: (info.total_free_bytes / 1024) as u32,
                    largest_kb: (info.largest_free_block / 1024) as u32,
                    min_free_kb: (info.minimum_free_bytes / 1024) as u32,
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------- I2S helpers

/// Create and start one standard-mode I2S channel.
///
/// `bits` is the slot width: 32 for the microphone (24 bits left-justified in a 32-bit
/// slot), 16 for the amplifier. Mono, Philips, master role, no MCLK pin: that is how
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
        // The bench firmware that works on this hardware uses 8 descriptors of 512 frames;
        // matching it removes one variable. It matters most on transmit: a stream is written
        // in one call per socket read, and anything else on the board (an OLED frame, a lock)
        // runs between those calls. With 2 x 200 frames the ring holds 16.7 ms, which is less
        // than one 128x32 frame on the bus, so the last descriptor is replayed and the music
        // repeats fragments instead of pausing. 2 x 512 frames is 42.7 ms of slack for 4 KB
        // of internal RAM - enough for a 9 ms frame draw, and this board has no 8 KB to spare
        // (its internal heap sits at a few kilobytes once the agent and TLS are up).
        chan.dma_desc_num = 2;
        chan.dma_frame_num = if rx { 200 } else { 512 };
        // A DMA descriptor keeps being replayed, so a tone whose writer went away loops
        // forever — the amplifier screams until someone sends silence. Clearing the TX
        // buffer after each send makes an interrupted tone decay to silence instead.
        if !rx {
            chan.__bindgen_anon_1 = sys::i2s_chan_config_t__bindgen_ty_1 { auto_clear_after_cb: true };
        }
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
        cfg.slot_cfg.slot_mode = sys::i2s_slot_mode_t_I2S_SLOT_MODE_STEREO;
        cfg.slot_cfg.slot_mask = sys::i2s_std_slot_mask_t_I2S_STD_SLOT_BOTH;
        cfg.slot_cfg.ws_width = bits as u32;
        // `I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG` sets these three; a zeroed struct leaves
        // them false, which right-aligns the sample in its slot and drops the Philips bit
        // shift. On an INMP441 that reads as a line that never leaves one level.
        cfg.slot_cfg.ws_pol = false;
        cfg.slot_cfg.bit_shift = true;
        cfg.slot_cfg.left_align = true;
        cfg.slot_cfg.big_endian = false;
        cfg.slot_cfg.bit_order_lsb = false;
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
    /// Put the peripheral back into a state where it will talk again.
    ///
    /// The controller is untouched: this clears the driver's bus state and the lines, not
    /// the panel's memory, so the next frame repaints what it should.
    fn reset_bus(&mut self, bus: u8) {
        if let Some(handle) = self.buses.get(&bus) {
            unsafe {
                let _ = sys::i2c_master_bus_reset(*handle);
            }
        }
    }

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

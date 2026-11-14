//! A virtual board: the same [`Hardware`] trait the firmware implements, backed
//! by an in-memory register file.
//!
//! Two jobs. It is the host simulator (`cargo run -p mcp-sim`), so the portal and
//! the MCP endpoint can be exercised without a device; and it is the fixture the
//! driver and manifest tests run against, which is why the register contents
//! follow the datasheets (BME280 calibration words, INA226's 0x5449, ...).

use crate::error::{McpError, Result};
use crate::hardware::{BoardInfo, Hardware, HeapPool, PinMode, WifiAp, WifiStatus};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeKind {
    Ssd1306,
    Bme280,
    Sht3x,
    Ina219,
    Mpu6050,
    Ads1115,
    Pcf8574,
}

impl FakeKind {
    fn name(self) -> &'static str {
        match self {
            FakeKind::Ssd1306 => "SSD1306",
            FakeKind::Bme280 => "BME280",
            FakeKind::Sht3x => "SHT3x",
            FakeKind::Ina219 => "INA219",
            FakeKind::Mpu6050 => "MPU6050",
            FakeKind::Ads1115 => "ADS1115",
            FakeKind::Pcf8574 => "PCF8574",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FakeI2c {
    pub addr: u8,
    pub kind: FakeKind,
    regs: BTreeMap<u8, u8>,
    /// Read-after-command buffer (SHT3x, PCF8574 echo).
    pending: Vec<u8>,
    armed: bool,
    /// Writes received, so tests can assert a driver wrote what it should.
    pub writes: Vec<Vec<u8>>,
    pub reads: usize,
}

impl FakeI2c {
    pub fn new(addr: u8, kind: FakeKind) -> Self {
        let mut regs = BTreeMap::new();
        match kind {
            FakeKind::Bme280 => {
                regs.insert(0xD0, 0x60); // chip id: BME280
                                         // The datasheet's worked example calibration block.
                let calib: [u8; 24] = [
                    0x70, 0x6B, 0x43, 0x67, 0x18, 0xFC, // T1..T3
                    0x7D, 0x8E, 0x43, 0xD6, 0xD0, 0x0B, // P1..P3
                    0x27, 0x0B, 0x8C, 0x00, 0xF9, 0xFF, // P4..P6
                    0x8C, 0x3C, 0xF8, 0xC6, 0x70, 0x17, // P7..P9
                ];
                for (i, b) in calib.iter().enumerate() {
                    regs.insert(0x88 + i as u8, *b);
                }
                regs.insert(0xA1, 0x4B); // H1
                for (i, b) in [0x6A, 0x01, 0x00, 0x13, 0x29, 0x03, 0x1E].iter().enumerate() {
                    regs.insert(0xE1 + i as u8, *b);
                }
                // Raw data: pressure 415148, temperature 519888, humidity 36000.
                for (i, b) in [0x65, 0x5A, 0xC0, 0x7E, 0xED, 0x00, 0x8C, 0xA0].iter().enumerate() {
                    regs.insert(0xF7 + i as u8, *b);
                }
                regs.insert(0xF3, 0x00); // status: ready
            }
            FakeKind::Mpu6050 => {
                regs.insert(0x75, 0x68); // WHO_AM_I
                for (i, b) in
                    [0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x0D, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                        .iter()
                        .enumerate()
                {
                    regs.insert(0x3B + i as u8, *b);
                }
            }
            FakeKind::Ina219 => {
                regs.insert(0xFE, 0x54); // manufacturer id, high byte
                regs.insert(0xFF, 0x49);
                regs.insert(0x02, 0x27); // bus voltage 0x2710 -> 5.00 V
                regs.insert(0x03, 0x10);
                regs.insert(0x01, 0x00); // shunt 0
                regs.insert(0x00, 0x00);
            }
            FakeKind::Ads1115 => {
                regs.insert(0x00, 0x0C); // conversion ~ 1.5 V at 4.096 range
                regs.insert(0x01, 0xC3);
                regs.insert(0x02, 0x00);
                regs.insert(0x03, 0x80);
            }
            FakeKind::Sht3x => {}
            FakeKind::Ssd1306 => {}
            FakeKind::Pcf8574 => {}
        }
        Self { addr, kind, regs, pending: Vec::new(), armed: false, writes: Vec::new(), reads: 0 }
    }

    fn write(&mut self, reg: Option<u8>, data: &[u8]) -> Result<()> {
        self.writes.push(data.to_vec());
        match self.kind {
            FakeKind::Ssd1306 => Ok(()), // commands and pixels are accepted silently
            FakeKind::Pcf8574 => {
                // The port keeps whatever was written and echoes it back.
                let last = data.last().copied().unwrap_or(0xFF);
                self.pending = vec![last];
                Ok(())
            }
            FakeKind::Sht3x => {
                // [0x2C, 0x06] = single shot measurement
                if data.len() >= 2 && data[0] == 0x2C {
                    self.armed = true;
                } else if data.len() >= 2 && data[0] == 0x30 {
                    self.armed = false; // soft reset / heater
                }
                Ok(())
            }
            FakeKind::Bme280 => {
                if let Some(r) = reg {
                    // Ignore config writes except the forced-mode trigger.
                    if r == 0xF4 {
                        // keep the measurement data as-is
                    } else {
                        for (i, b) in data.iter().enumerate() {
                            self.regs.insert(r.wrapping_add(i as u8), *b);
                        }
                    }
                }
                Ok(())
            }
            _ => {
                if let Some(r) = reg {
                    for (i, b) in data.iter().enumerate() {
                        self.regs.insert(r.wrapping_add(i as u8), *b);
                    }
                }
                Ok(())
            }
        }
    }

    fn read(&mut self, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
        self.reads += 1;
        match self.kind {
            FakeKind::Ssd1306 => Err(McpError::hardware("SSD1306 has no readable registers")),
            FakeKind::Sht3x => {
                if !self.armed {
                    return Err(McpError::hardware("SHT3x: no measurement in flight"));
                }
                // 25.0 °C, 50 %RH with valid CRCs.
                let t: u16 = ((25.0 + 45.0) / 175.0 * 65535.0) as u16;
                let h: u16 = (50.0 / 100.0 * 65535.0) as u16;
                let tb = t.to_be_bytes();
                let hb = h.to_be_bytes();
                let mut buf = vec![tb[0], tb[1], crate::util::crc8_poly31(&tb)];
                buf.extend_from_slice(&[hb[0], hb[1], crate::util::crc8_poly31(&hb)]);
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = buf.get(i).copied().unwrap_or(0);
                }
                self.armed = false;
                Ok(())
            }
            FakeKind::Pcf8574 => {
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = self.pending.get(i).copied().unwrap_or(0xFF);
                }
                Ok(())
            }
            _ => {
                let start = reg.unwrap_or(0);
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = self.regs.get(&start.wrapping_add(i as u8)).copied().unwrap_or(0);
                }
                Ok(())
            }
        }
    }
}

pub struct VirtualBoard {
    pub i2c: Vec<FakeI2c>,
    pub gpio_out: BTreeMap<i32, bool>,
    pub gpio_in: BTreeMap<i32, bool>,
    pub pwm: BTreeMap<i32, (u32, f32)>,
    pub ring: BTreeMap<i32, (u8, u8, u8)>,
    pub adc: BTreeMap<i32, u32>,
    /// Whether the virtual I2S channels are up (set by `i2s_setup`).
    pub i2s_mic: bool,
    pub i2s_amp: bool,
    /// Samples written to the virtual amplifier, so a test can check that playback
    /// reached the hardware instead of dying in the tool.
    pub i2s_written: u64,
    pub scan: Vec<WifiAp>,
    pub wifi: WifiStatus,
    pub log: Vec<String>,
    /// Frame deadlines per job, for `media_submit_av`.
    media_pictures: std::collections::BTreeMap<u64, Vec<u64>>,
    /// Playing and queued jobs: `(job, started, duration_ms, rate_hz)`.
    media: std::collections::VecDeque<(u64, std::time::Instant, u64, u64)>,
    media_next: u64,
    media_done: u64,
    media_cancelled: bool,
    /// Every hardware access a tool performs, so a test can assert that a rejected
    /// call never reached the board.
    pub hw_calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
    start: Instant,
    /// Set when something asked for a reboot, so the simulator can exit.
    pub rebooted: bool,
}

impl Default for VirtualBoard {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualBoard {
    /// The default bench: an OLED, a BME280 and an SHT3x on the S3 devkit's I2C
    /// pins. No IMU and no expander, so the tests can prove they stay hidden.
    pub fn new() -> Self {
        let mut b = Self {
            i2c: vec![
                FakeI2c::new(0x3C, FakeKind::Ssd1306),
                FakeI2c::new(0x44, FakeKind::Sht3x),
                FakeI2c::new(0x76, FakeKind::Bme280),
            ],
            gpio_out: BTreeMap::new(),
            gpio_in: BTreeMap::new(),
            pwm: BTreeMap::new(),
            ring: BTreeMap::new(),
            adc: BTreeMap::new(),
            i2s_mic: false,
            i2s_amp: false,
            i2s_written: 0,
            scan: vec![
                WifiAp { ssid: "home-net".into(), rssi: -48, channel: 6, auth: "wpa2".into() },
                WifiAp { ssid: "lab-5g".into(), rssi: -71, channel: 44, auth: "wpa3".into() },
                WifiAp { ssid: "cafe".into(), rssi: -83, channel: 1, auth: "open".into() },
            ],
            wifi: WifiStatus {
                state: "connected".into(),
                ssid: "home-net".into(),
                ip: "127.0.0.1".into(),
                rssi_dbm: -48,
                channel: 6,
                mac: "24:0a:c4:00:00:01".into(),
                hostname: "sim".into(),
                ap_ssid: String::new(),
                ap_ip: "192.168.4.1".into(),
                clients: 0,
                // What a DHCP lease on a normal LAN looks like, so `ifconfig` has measured
                // values to report instead of falling back to the config.
                gateway: "192.168.3.1".into(),
                netmask: "255.255.255.0".into(),
                dns: vec!["192.168.3.1".into()],
            },
            log: Vec::new(),
            hw_calls: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            media: std::collections::VecDeque::new(),
            media_pictures: std::collections::BTreeMap::new(),
            media_next: 0,
            media_done: 0,
            media_cancelled: false,
            start: Instant::now(),
            rebooted: false,
        };
        b.adc.insert(1, 1650);
        b.adc.insert(2, 2048);
        b.gpio_in.insert(0, true);
        b
    }

    /// Drop the device at `addr`, so a test can simulate "not wired".
    pub fn remove_at(&mut self, addr: u8) {
        self.i2c.retain(|d| d.addr != addr);
    }

    pub fn add(&mut self, addr: u8, kind: FakeKind) {
        self.i2c.push(FakeI2c::new(addr, kind));
    }

    pub fn i2c_device(&self, addr: u8) -> Option<&FakeI2c> {
        self.i2c.iter().find(|d| d.addr == addr)
    }

    fn device_mut(&mut self, addr: u8) -> Result<&mut FakeI2c> {
        self.i2c
            .iter_mut()
            .find(|d| d.addr == addr)
            .ok_or_else(|| McpError::hardware(format!("no I2C device at 0x{addr:02X}")))
    }
}

impl Hardware for VirtualBoard {
    fn board_info(&mut self) -> BoardInfo {
        BoardInfo {
            soc: "esp32s3".into(),
            cores: 2,
            revision: 2,
            flash_mb: 16,
            psram_kb: 8192,
            mac: self.wifi.mac.clone(),
            free_heap_kb: 172,
            min_free_heap_kb: 128,
            idf_version: "sim".into(),
            temp_c: Some(41.5),
            firmware: "sim".into(),
            build: "host".into(),
        }
    }

    fn chip_temp_c(&mut self) -> Option<f32> {
        Some(41.5)
    }

    fn tasks(&mut self) -> Vec<serde_json::Value> {
        // `core` is part of the shape even here: a host has no SMP scheduling to report, so
        // every task claims core 0, and `ps` on the simulator still shows the column the
        // board fills in.
        vec![
            json!({"name": "main", "state": "running", "priority": 1, "stack_free_bytes": 2048, "core": 0}),
            json!({"name": "httpd", "state": "blocked", "priority": 5, "stack_free_bytes": 1024, "core": 0}),
            json!({"name": "agent", "state": "blocked", "priority": 4, "stack_free_bytes": 1536, "core": 0}),
        ]
    }

    /// The simulator has no flash to write, so it answers "not on this thread": every write
    /// takes the queued path, exactly as it does on a board whose calling thread has a PSRAM
    /// stack. That keeps `no_edit_writes_flash_where_it_was_made` honest instead of letting a
    /// streamed write slip past the queue on the host and not on the board.
    fn stack_is_internal(&self) -> bool {
        false
    }

    /// The host has no PSRAM; the virtual board still answers the shape, so a caller that
    /// checks before allocating exercises the same path here.
    fn psram_largest_free(&mut self) -> Option<usize> {
        Some(7_800 * 1024)
    }

    /// The virtual board's PSRAM pool, named: a test that reads `pool` sees the same string
    /// the bench board reports, so "where did the bytes land" is exercised on the host too.
    fn media_buffer(&mut self, len: usize) -> Option<(Vec<u8>, &'static str)> {
        Some((Vec::with_capacity(len), "psram"))
    }

    /// The simulator has no amplifier, so the "player" is a queue of jobs and a clock: each
    /// job plays for its own duration in turn, and the state is derived from how far the
    /// clock has run. A one-slot model would let a second submit silently replace the job
    /// that is playing, which is the bug the board had — the fake must not be able to hide it.
    fn media_submit(&mut self, samples: &[i16], rate_hz: u32) -> Result<u64> {
        let rate = rate_hz.max(1) as u64;
        let duration_ms = (samples.len() as u64).saturating_mul(1000) / rate;
        if self.media.len() >= 4 {
            return Err(crate::error::McpError::invalid_params(
                "the player already has four jobs queued; wait or cancel",
            ));
        }
        self.media_next = self.media_next.saturating_add(1);
        self.media.push_back((self.media_next, std::time::Instant::now(), duration_ms, rate));
        self.media_cancelled = false;
        Ok(self.media_next)
    }

    fn media_state(&mut self) -> crate::hardware::MediaState {
        // Retire every job whose clock has run out, in order.
        while let Some((_, started, duration_ms, _)) = self.media.front().copied() {
            if started.elapsed().as_millis() as u64 >= duration_ms {
                self.media.pop_front();
                self.media_done = self.media_done.saturating_add(1);
            } else {
                break;
            }
        }
        let mut state = crate::hardware::MediaState {
            jobs_done: self.media_done,
            cancelled: self.media_cancelled,
            queued: self.media.len().saturating_sub(1) as u64,
            ..Default::default()
        };
        let Some((job, started, duration_ms, rate_hz)) = self.media.front().copied() else {
            return state;
        };
        let elapsed = started.elapsed().as_millis() as u64;
        state.job = job;
        state.duration_ms = duration_ms;
        state.rate_hz = rate_hz as u32;
        state.playing = true;
        if let Some(deadlines) = self.media_pictures.get(&job) {
            state.frames_total = deadlines.len() as u64;
            state.frames_done = deadlines.iter().filter(|at| **at <= elapsed).count() as u64;
        }
        // Derived here, not stored: the same reason the board derives it — a number written
        // once per chunk is stale the moment it is written.
        state.remaining_ms = duration_ms.saturating_sub(elapsed);
        state.chunks = (elapsed * rate_hz / 1000) / 4096 + 1;
        state
    }

    /// The fake player keeps the frame deadlines of each job and derives how many are due
    /// from the same clock it uses for audio, so a test can see the picture track the sound
    /// without a panel.
    fn media_submit_av(
        &mut self,
        samples: &[i16],
        rate_hz: u32,
        pictures: &[crate::hardware::MediaPicture],
        _panel: Option<(u8, u8)>,
    ) -> Result<u64> {
        let job = self.media_submit(samples, rate_hz)?;
        self.media_pictures.insert(job, pictures.iter().map(|p| p.at_ms).collect());
        Ok(job)
    }

    fn media_cancel(&mut self) -> bool {
        let playing = !self.media.is_empty();
        self.media_pictures.clear();
        self.media.clear();
        if playing {
            self.media_cancelled = true;
        }
        playing
    }

    /// Two pools, because the point of the reading is that the firmware asks instead of
    /// assuming: a client sees the shape here and reads real numbers off a board. The
    /// values are fixed, like the virtual board's other memory figures.
    fn heap_pools(&mut self) -> Vec<HeapPool> {
        vec![
            HeapPool {
                pool: "internal_8bit".into(),
                note: "thread stacks and anything that must survive with the cache off".into(),
                total_kb: 320,
                free_kb: 172,
                largest_kb: 96,
                min_free_kb: 128,
            },
            HeapPool {
                pool: "spiram".into(),
                note: "PSRAM: allocation only, no stacks, no DMA descriptors".into(),
                total_kb: 8192,
                free_kb: 7930,
                largest_kb: 7800,
                min_free_kb: 7800,
            },
        ]
    }

    /// The virtual board answers for every host it is asked about: there is no stack
    /// underneath it to be unreachable.
    fn ping(&mut self, host: &str, count: u32) -> crate::error::Result<serde_json::Value> {
        let count = count.clamp(1, 5);
        Ok(json!({
            "host": host,
            "address": "203.0.113.10",
            "sent": count,
            "received": count,
            "last_rtt_ms": 1,
            "reachable": true,
            "note": "virtual board: every host answers",
        }))
    }

    fn i2c_scan(&mut self, _bus: u8) -> Result<Vec<u8>> {
        let mut addrs: Vec<u8> = self.i2c.iter().map(|d| d.addr).collect();
        addrs.sort_unstable();
        Ok(addrs)
    }

    fn i2c_read(&mut self, _bus: u8, addr: u8, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
        self.hw_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.device_mut(addr)?.read(reg, out)
    }

    fn i2c_write(&mut self, _bus: u8, addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
        self.device_mut(addr)?.write(reg, data)
    }

    fn gpio_config(&mut self, _pin: i32, mode: PinMode) -> Result<()> {
        if mode == PinMode::InputPullup && !self.gpio_in.contains_key(&_pin) {
            self.gpio_in.insert(_pin, true);
        }
        Ok(())
    }

    fn gpio_read(&mut self, pin: i32) -> Result<bool> {
        Ok(self.gpio_in.get(&pin).copied().or_else(|| self.gpio_out.get(&pin).copied()).unwrap_or(false))
    }

    fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
        self.hw_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.gpio_out.insert(pin, high);
        Ok(())
    }

    fn pwm_set(&mut self, pin: i32, freq_hz: u32, duty_pct: f32) -> Result<()> {
        self.pwm.insert(pin, (freq_hz, duty_pct));
        Ok(())
    }

    fn adc_mv(&mut self, pin: i32) -> Result<u32> {
        Ok(self.adc.get(&pin).copied().unwrap_or(1650))
    }

    /// The virtual board has I2S whenever pins are given, and the "microphone" is a 440 Hz
    /// tone with a little noise: enough for a level meter and a recording to look sane
    /// without pretending to model acoustics.
    fn i2s_setup(
        &mut self,
        mic: Option<crate::boards::I2sPins>,
        amp: Option<crate::boards::I2sPins>,
    ) -> Result<()> {
        self.i2s_mic = mic.is_some();
        self.i2s_amp = amp.is_some();
        Ok(())
    }

    fn i2s_mic_read(&mut self, out: &mut [i32], _timeout_ms: u32) -> Result<usize> {
        if !self.i2s_mic {
            return Err(McpError::unsupported("no I2S microphone on the virtual board"));
        }
        let t = self.start.elapsed().as_micros() as f32 / 1_000_000.0;
        let rate = crate::drivers::audio::MIC_RATE_HZ as f32;
        for (i, s) in out.iter_mut().enumerate() {
            let phase = 2.0 * std::f32::consts::PI * 440.0 * (t + i as f32 / rate);
            // 24 bits left-justified in 32, like the INMP441: about -20 dBFS plus a slow
            // wobble, so the level is not a constant.
            let wobble = 1.0 + 0.1 * (i as f32 * 0.01).sin();
            *s = (phase.sin() * 3280.0 * wobble) as i32 * 65_536;
        }
        Ok(out.len())
    }

    fn i2s_out_write(&mut self, samples: &[i16]) -> Result<()> {
        if !self.i2s_amp {
            return Err(McpError::unsupported("no I2S speaker on the virtual board"));
        }
        self.i2s_written = self.i2s_written.saturating_add(samples.len() as u64);
        Ok(())
    }

    fn ring_set(&mut self, pin: i32, rgb: (u8, u8, u8), _count: u16) -> Result<()> {
        self.ring.insert(pin, rgb);
        Ok(())
    }

    fn delay_us(&mut self, us: u32) {
        // Cap virtual time: the simulator never needs to wait 750 ms for a probe.
        std::thread::sleep(std::time::Duration::from_micros(us.min(2_000) as u64));
    }

    /// The virtual board can honour the fallible form: it has no timing to fail at, and a
    /// DHT read on the simulator should get as far as a missing handshake rather than a
    /// blanket refusal.
    fn delay_us_checked(&mut self, us: u32) -> Result<()> {
        self.delay_us(us);
        Ok(())
    }

    fn millis(&mut self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn wifi_status(&mut self) -> WifiStatus {
        self.wifi.clone()
    }

    fn wifi_scan(&mut self) -> Result<Vec<WifiAp>> {
        Ok(self.scan.clone())
    }

    fn wifi_connect(&mut self, ssid: &str, _pw: &str) -> Result<()> {
        self.wifi.state = "connected".into();
        self.wifi.ssid = ssid.to_string();
        self.wifi.ip = "127.0.0.1".into();
        Ok(())
    }

    fn reboot(&mut self) {
        self.rebooted = true;
        self.log.push("reboot requested".into());
    }

    fn inventory_extra(&mut self) -> Value {
        json!({
            "virtual": true,
            "i2c_devices": self.i2c.iter().map(|d| json!({
                "address": format!("0x{:02X}", d.addr),
                "kind": d.kind.name(),
                "reads": d.reads,
                "writes": d.writes.len(),
            })).collect::<Vec<Value>>(),
            "gpio_out": self.gpio_out,
            "pwm": self.pwm.iter().map(|(k, v)| json!({"pin": k, "freq": v.0, "duty": v.1})).collect::<Vec<Value>>(),
            "ring": self.ring.iter().map(|(k, v)| json!({"pin": k, "rgb": [v.0, v.1, v.2]})).collect::<Vec<Value>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lists_the_virtual_chips() {
        let mut b = VirtualBoard::new();
        assert_eq!(b.i2c_scan(0).unwrap(), vec![0x3C, 0x44, 0x76]);
        b.remove_at(0x44);
        assert_eq!(b.i2c_scan(0).unwrap(), vec![0x3C, 0x76]);
    }

    #[test]
    fn bme280_registers_reproduce_the_datasheet_reading() {
        let mut b = VirtualBoard::new();
        let mut id = [0u8; 1];
        b.i2c_read(0, 0x76, Some(0xD0), &mut id).unwrap();
        assert_eq!(id[0], 0x60);
        let mut raw = [0u8; 8];
        b.i2c_read(0, 0x76, Some(0xF7), &mut raw).unwrap();
        let adc_p = ((raw[0] as i32) << 12) | ((raw[1] as i32) << 4) | ((raw[2] as i32) >> 4);
        let adc_t = ((raw[3] as i32) << 12) | ((raw[4] as i32) << 4) | ((raw[5] as i32) >> 4);
        assert_eq!(adc_p, 415_148);
        assert_eq!(adc_t, 519_888);
    }

    #[test]
    fn sht3x_needs_a_measurement_command_first() {
        let mut b = VirtualBoard::new();
        let mut buf = [0u8; 6];
        assert!(b.i2c_read(0, 0x44, None, &mut buf).is_err());
        b.i2c_write(0, 0x44, None, &[0x2C, 0x06]).unwrap();
        b.i2c_read(0, 0x44, None, &mut buf).unwrap();
        let raw_t = u16::from_be_bytes([buf[0], buf[1]]) as f32;
        let t = -45.0 + 175.0 * raw_t / 65535.0;
        assert!((t - 25.0).abs() < 0.1, "temperature {t}");
        assert_eq!(buf[2], crate::util::crc8_poly31(&[buf[0], buf[1]]));
    }

    #[test]
    fn gpio_and_pwm_state_is_observable() {
        let mut b = VirtualBoard::new();
        b.gpio_write(48, true).unwrap();
        assert!(b.gpio_read(48).unwrap());
        b.pwm_set(7, 50, 7.5).unwrap();
        assert_eq!(b.pwm[&7], (50, 7.5));
        b.ring_set(38, (1, 2, 3), 1).unwrap();
        assert_eq!(b.ring[&38], (1, 2, 3));
    }
}

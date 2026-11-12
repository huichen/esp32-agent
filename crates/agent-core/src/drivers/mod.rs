//! Device drivers and the detection pass.
//!
//! Detection runs once at boot and again whenever the portal asks for a re-probe.
//! Its result is the single source of truth for both `tools/list` and the
//! portal's device table: a driver that was not found contributes no tools, so
//! the model never sees a capability the board does not have.
//!
//! Adding a peripheral is a three-step job, documented in
//! `docs/adding-a-device.md`: write the driver module, add it to [`CATALOG`], and
//! add its `probe` call to [`detect`].

pub mod actuator;
pub mod ads1115;
pub mod audio;
pub mod bme280;
pub mod expander;
pub mod ina219;
pub mod motion;
pub mod onewire;
pub mod ranging;
pub mod sht3x;
pub mod ssd1306;

use crate::boards::BoardProfile;
use crate::config::Config;
use crate::device::{Device, DeviceInfo, Devices};
use crate::error::Result;
use crate::hardware::Hardware;
use crate::logbuf::LogBuf;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

/// A row in the driver catalogue: what the firmware knows how to look for.
pub struct CatalogEntry {
    pub kind: &'static str,
    pub name: &'static str,
    /// How the presence of this device is established.
    pub detection: &'static str,
    /// `implemented`, `probe-only` or `planned`.
    pub status: &'static str,
    pub caps: &'static [&'static str],
    pub docs: &'static str,
}

/// Everything the firmware can detect. Kept in one place because the portal,
/// `README.md` and `docs/capabilities.md` all quote this table.
pub static CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        kind: "ssd1306",
        name: "SSD1306 / SH1106 OLED 128x32/64",
        detection: "I2C ACK at 0x3C/0x3D",
        status: "implemented",
        caps: &["oled"],
        docs: "drivers/ssd1306.rs",
    },
    CatalogEntry {
        kind: "pcf8574",
        name: "PCF8574 port expander + HD44780 LCD1602",
        detection: "write 0xFF, read back 0xFF",
        status: "implemented",
        caps: &["pcf8574", "lcd"],
        docs: "drivers/expander.rs",
    },
    CatalogEntry {
        kind: "bme280",
        name: "BME280 / BMP280 temperature, pressure, humidity",
        detection: "chip id 0x60/0x58 at 0x76/0x77",
        status: "implemented",
        caps: &["bme280"],
        docs: "drivers/bme280.rs",
    },
    CatalogEntry {
        kind: "sht3x",
        name: "SHT30/SHT31/SHT35 humidity + temperature",
        detection: "measurement with valid CRC at 0x44/0x45",
        status: "implemented",
        caps: &["sht3x"],
        docs: "drivers/sht3x.rs",
    },
    CatalogEntry {
        kind: "ina219",
        name: "INA219 / INA226 current + power monitor",
        detection: "config reset value / 0x5449 at 0x40-0x41",
        status: "implemented",
        caps: &["ina219"],
        docs: "drivers/ina219.rs",
    },
    CatalogEntry {
        kind: "mpu6050",
        name: "MPU-6050 / 6500 six-axis IMU",
        detection: "WHO_AM_I 0x68/0x70/0x71/0x73 at 0x68/0x69",
        status: "implemented",
        caps: &["mpu6050"],
        docs: "drivers/motion.rs",
    },
    CatalogEntry {
        kind: "ads1115",
        name: "ADS1115 16-bit ADC",
        detection: "in-range conversion at 0x48-0x4B",
        status: "implemented",
        caps: &["ads1115"],
        docs: "drivers/ads1115.rs",
    },
    CatalogEntry {
        kind: "ds18b20",
        name: "DS18B20 / DS18S20 1-Wire temperature probe",
        detection: "presence pulse + ROM CRC on a configured pin",
        status: "implemented",
        caps: &["ds18b20"],
        docs: "drivers/onewire.rs",
    },
    CatalogEntry {
        kind: "hcsr04",
        name: "HC-SR04 ultrasonic distance",
        detection: "first ping after the pins are configured",
        status: "implemented",
        caps: &["hcsr04"],
        docs: "drivers/ranging.rs",
    },
    CatalogEntry {
        kind: "servo",
        name: "Hobby servo (PWM)",
        detection: "declared pin",
        status: "implemented",
        caps: &["servo"],
        docs: "drivers/actuator.rs",
    },
    CatalogEntry {
        kind: "relay",
        name: "Relay board channel",
        detection: "declared pin",
        status: "implemented",
        caps: &["relay"],
        docs: "drivers/actuator.rs",
    },
    CatalogEntry {
        kind: "led",
        name: "WS2812 / plain / PWM LED",
        detection: "board profile or declared pin",
        status: "implemented",
        caps: &["led"],
        docs: "drivers/actuator.rs",
    },
    CatalogEntry {
        kind: "buttons",
        name: "Push buttons with pull-ups",
        detection: "board profile or declared pins",
        status: "implemented",
        caps: &["button"],
        docs: "drivers/actuator.rs",
    },
    CatalogEntry {
        kind: "dht11",
        name: "DHT11 / DHT22 temperature + humidity",
        detection: "not implemented yet",
        status: "planned",
        caps: &["dht"],
        docs: "docs/adding-a-device.md",
    },
    CatalogEntry {
        kind: "rc522",
        name: "MFRC522 RFID reader",
        detection: "not implemented yet",
        status: "planned",
        caps: &["rc522"],
        docs: "docs/adding-a-device.md",
    },
    CatalogEntry {
        kind: "vl53l0x",
        name: "VL53L0X time-of-flight distance",
        detection: "not implemented yet",
        status: "planned",
        caps: &["vl53l0x"],
        docs: "docs/adding-a-device.md",
    },
    CatalogEntry {
        kind: "sdcard",
        name: "SD card over SPI",
        detection: "not implemented yet",
        status: "planned",
        caps: &["sdcard"],
        docs: "docs/adding-a-device.md",
    },
    CatalogEntry {
        kind: "mic",
        name: "INMP441 / ICS-43434 I2S microphone",
        detection: "I2S pins from the board profile or config.pins",
        status: "implemented",
        caps: &["mic"],
        docs: "drivers/audio.rs",
    },
    CatalogEntry {
        kind: "speaker",
        name: "MAX98357A I2S amplifier",
        detection: "I2S pins from the board profile or config.pins",
        status: "implemented",
        caps: &["speaker"],
        docs: "drivers/audio.rs",
    },
    CatalogEntry {
        kind: "camera",
        name: "OV2640 / OV3660 camera (DVP, esp32-camera)",
        detection: "not implemented yet: needs DMA frame buffers in PSRAM and an I2C control channel",
        status: "planned",
        caps: &["camera"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "bme680",
        name: "BME680 temperature / humidity / pressure / gas",
        detection: "not implemented yet: same I2C probe as the BME280 plus the gas heater ramp",
        status: "planned",
        caps: &["bme680"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "aht20",
        name: "AHT20 / AHT10 temperature + humidity",
        detection: "not implemented yet: 0x38, read the status word after a conversion command",
        status: "planned",
        caps: &["aht20"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "bh1750",
        name: "BH1750 ambient light",
        detection: "not implemented yet: 0x23/0x5C, a one-byte command returns lux as 16 bits",
        status: "planned",
        caps: &["bh1750"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "hx711",
        name: "HX711 load cell amplifier",
        detection: "not implemented yet: a bit-banged 24-bit shift, so a driver rather than a manifest",
        status: "planned",
        caps: &["hx711"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "ds3231",
        name: "DS3231 real-time clock",
        detection: "not implemented yet: 0x68, BCD registers; SNTP covers the clock today",
        status: "planned",
        caps: &["rtc"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "bdc_motor",
        name: "L298N / TB6612 / DRV8833 brushed motor",
        detection: "not implemented yet: two PWM pins and a direction pin, with a speed limit",
        status: "planned",
        caps: &["motor"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "stepper",
        name: "A4988 / DRV8825 / TMC2208 stepper",
        detection: "not implemented yet: step/dir pulses need timing a manifest sequence cannot hold",
        status: "planned",
        caps: &["stepper"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "encoder",
        name: "Rotary encoder (KY-040 and bare)",
        detection: "not implemented yet: needs an interrupt or a polled quadrature decoder",
        status: "planned",
        caps: &["encoder"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "pir",
        name: "HC-SR501 PIR motion",
        detection:
            "not implemented yet: a digital pin; a manifest could wrap it, a debounce belongs in a driver",
        status: "planned",
        caps: &["pir"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "tm1637",
        name: "TM1637 four-digit display",
        detection: "not implemented yet: a two-wire protocol of its own, not I2C",
        status: "planned",
        caps: &["sevenseg"],
        docs: "ROADMAP.md",
    },
    CatalogEntry {
        kind: "max7219",
        name: "MAX7219 LED matrix",
        detection: "not implemented yet: SPI, with a framebuffer like the OLED driver",
        status: "planned",
        caps: &["matrix"],
        docs: "ROADMAP.md",
    },
];

pub fn catalog_json() -> Value {
    json!(CATALOG
        .iter()
        .map(|e| json!({
            "kind": e.kind,
            "name": e.name,
            "detection": e.detection,
            "status": e.status,
            "caps": e.caps,
            "source": e.docs,
        }))
        .collect::<Vec<Value>>())
}

/// An address is worth trying when the scan saw it and no driver claimed it.
fn present(scan: &[u8], claimed: &BTreeSet<u8>, addr: u8) -> bool {
    scan.contains(&addr) && !claimed.contains(&addr)
}

/// What a detection pass found, including addresses it could not identify.
pub struct DetectResult {
    pub devices: Devices,
    pub unknown: Vec<Value>,
    pub buses: Vec<Value>,
    /// Devices a driver recognised but did not adopt, because `devices.discovery` is
    /// `manual`. The agent sees them, decides, and adds the ones it wants — see
    /// [`DetectResult::devices`] for the adopted set.
    pub candidates: Vec<Value>,
}

/// Drivers a manifest can instantiate. A candidate whose kind is not here is reported
/// without an `adopt` body: it comes from the board profile (I2S audio) and declaring it
/// again would be a no-op that reads like a success.
const DECLARABLE: &[&str] = &[
    "ssd1306", "bme280", "sht3x", "ina219", "mpu6050", "ads1115", "pcf8574", "ds18b20", "hcsr04", "servo",
    "relay", "led", "buttons", "lcd",
];

/// Probe every configured bus and every declared pin.
pub fn detect(
    hw: &mut dyn Hardware,
    cfg: &Config,
    profile: &BoardProfile,
    log: &Arc<LogBuf>,
) -> DetectResult {
    let mut devices = Devices::new();
    let mut unknown = Vec::new();
    let mut buses_json = Vec::new();

    // Count how many of each kind were found, so ids get a `_2`, `_3` suffix.
    let mut seen: std::collections::BTreeMap<&'static str, usize> = Default::default();
    let mut next_suffix = |kind: &'static str| {
        let n = seen.entry(kind).or_insert(0);
        *n += 1;
        *n
    };

    for bus_cfg in &cfg.bus.i2c {
        let bus = bus_cfg.bus;
        let scan = match hw.i2c_scan(bus) {
            Ok(v) => v,
            Err(e) => {
                log.warn(
                    "probe",
                    format!(
                        "I2C bus {bus} (SDA {}, SCL {}) unusable: {}",
                        bus_cfg.sda, bus_cfg.scl, e.message
                    ),
                );
                buses_json.push(json!({
                    "bus": bus, "sda": bus_cfg.sda, "scl": bus_cfg.scl,
                    "addresses": [], "error": e.message,
                }));
                continue;
            }
        };
        log.info("probe", format!("I2C bus {bus}: {} device(s) at {:02X?}", scan.len(), scan));
        buses_json.push(json!({
            "bus": bus, "sda": bus_cfg.sda, "scl": bus_cfg.scl,
            "addresses": scan.iter().map(|a| format!("0x{a:02X}")).collect::<Vec<String>>(),
        }));

        let mut claimed: BTreeSet<u8> = BTreeSet::new();

        // Display first: 0x3C is also a legal address for a few expanders.
        for addr in ssd1306::Ssd1306::ADDRESSES {
            if !present(&scan, &claimed, *addr) {
                continue;
            }
            let height = cfg.pins.get("oled.height").copied().unwrap_or(64) as u8;
            let controller = if cfg.pins.get("oled.sh1106").copied().unwrap_or(0) != 0 {
                ssd1306::Controller::Sh1106
            } else {
                ssd1306::Controller::Ssd1306
            };
            let suffix = next_suffix("ssd1306");
            match ssd1306::Ssd1306::probe(hw, bus, *addr, height, controller, suffix) {
                Ok(d) => {
                    log.info("probe", format!("OLED at 0x{addr:02X} ({height}px)"));
                    claimed.insert(*addr);
                    devices.push(Box::new(d));
                }
                Err(e) => log.warn(
                    "probe",
                    format!("0x{addr:02X} looks like an OLED but failed to init: {}", e.message),
                ),
            }
        }

        // Temperature/pressure sensors.
        for addr in bme280::Bme280::ADDRESSES {
            if !present(&scan, &claimed, *addr) || bme280::Bme280::identify(hw, bus, *addr).is_none() {
                continue;
            }
            let suffix = next_suffix("bme280");
            match bme280::Bme280::probe(hw, bus, *addr, suffix) {
                Ok(d) => {
                    log.info("probe", format!("BME/BMP280 at 0x{addr:02X}"));
                    claimed.insert(*addr);
                    devices.push(Box::new(d));
                }
                Err(e) => log.warn("probe", format!("BME280 at 0x{addr:02X}: {}", e.message)),
            }
        }

        for addr in sht3x::Sht3x::ADDRESSES {
            if !present(&scan, &claimed, *addr) {
                continue;
            }
            let suffix = next_suffix("sht3x");
            if let Ok(d) = sht3x::Sht3x::probe(hw, bus, *addr, suffix) {
                log.info("probe", format!("SHT3x at 0x{addr:02X}"));
                claimed.insert(*addr);
                devices.push(Box::new(d));
            }
        }

        for addr in ina219::Ina219::ADDRESSES {
            if !present(&scan, &claimed, *addr) {
                continue;
            }
            let shunt = cfg.pins.get("ina219.shunt_mohm").copied().unwrap_or(100).max(1) as u32;
            let suffix = next_suffix("ina219");
            if let Ok(d) = ina219::Ina219::probe(hw, bus, *addr, shunt, suffix) {
                log.info("probe", format!("INA219/226 at 0x{addr:02X}"));
                claimed.insert(*addr);
                devices.push(Box::new(d));
            }
        }

        for addr in motion::Mpu6050::ADDRESSES {
            if !present(&scan, &claimed, *addr) || motion::Mpu6050::identify(hw, bus, *addr).is_none() {
                continue;
            }
            let suffix = next_suffix("mpu6050");
            match motion::Mpu6050::probe(hw, bus, *addr, suffix) {
                Ok(d) => {
                    log.info("probe", format!("MPU-6xxx at 0x{addr:02X}"));
                    claimed.insert(*addr);
                    devices.push(Box::new(d));
                }
                Err(e) => log.warn("probe", format!("IMU at 0x{addr:02X}: {}", e.message)),
            }
        }

        for addr in ads1115::Ads1115::ADDRESSES {
            if !present(&scan, &claimed, *addr) {
                continue;
            }
            let suffix = next_suffix("ads1115");
            if let Ok(d) = ads1115::Ads1115::probe(hw, bus, *addr, suffix) {
                log.info("probe", format!("ADS1115 at 0x{addr:02X}"));
                claimed.insert(*addr);
                devices.push(Box::new(d));
            }
        }

        for addr in expander::Pcf8574::ADDRESSES {
            if !present(&scan, &claimed, *addr) {
                continue;
            }
            let suffix = next_suffix("pcf8574");
            if let Ok(d) = expander::Pcf8574::probe(hw, bus, *addr, suffix) {
                log.info("probe", format!("PCF8574 at 0x{addr:02X}"));
                claimed.insert(*addr);
                devices.push(Box::new(d));
            }
        }

        for addr in &scan {
            if !claimed.contains(addr) {
                unknown.push(json!({
                    "bus": bus,
                    "address": format!("0x{addr:02X}"),
                    "note": "something acknowledged but no driver matched",
                }));
            }
        }
    }

    // 1-Wire probes: only on pins the user listed, because a bit-banged reset
    // pulse on an arbitrary pin is not a harmless operation.
    let mut pins: Vec<i32> = cfg.bus.onewire_pins.clone();
    pins.sort_unstable();
    pins.dedup();
    for pin in pins {
        match onewire::Ds18b20::probe(hw, pin) {
            Ok(Some(d)) => {
                log.info("probe", format!("DS18B20 on GPIO{pin}"));
                next_suffix("ds18b20");
                devices.push(Box::new(d));
            }
            Ok(None) => log.info("probe", format!("no 1-Wire device on GPIO{pin}")),
            Err(e) => log.warn("probe", format!("1-Wire probe on GPIO{pin}: {}", e.message)),
        }
    }

    // Declared GPIO peripherals.
    let pin_of = |key: &str| cfg.pins.get(key).copied();

    // Buttons and the LED are the devkit's own, so they follow `board.auto_peripherals`:
    // a minimal board starts without them and has them back by declaring the device in
    // the manifest. An explicitly configured pin (`pins["button.boot"]`, `pins["led"]`)
    // still counts as a decision and wins either way.
    let mut buttons: Vec<(String, i32)> = Vec::new();
    for (k, v) in &cfg.pins {
        if let Some(rest) = k.strip_prefix("button.") {
            if !rest.is_empty() && rest.parse::<i32>().is_ok() {
                buttons.push((format!("button{rest}"), *v));
            } else if !rest.is_empty() {
                buttons.push((rest.to_string(), *v));
            }
        }
    }
    if buttons.is_empty() && cfg.auto_peripherals {
        for b in profile.buttons {
            buttons.push((b.name.to_string(), b.pin));
        }
    }
    if !buttons.is_empty() {
        let dev = actuator::Buttons::new(buttons);
        if dev.init(hw).is_ok() {
            next_suffix("buttons");
            devices.push(Box::new(dev));
        }
    }

    // LED: an explicit pin wins over the profile, and the profile only speaks when the
    // board is allowed to bring its own peripherals up.
    let led_pin =
        pin_of("led").or_else(|| if cfg.auto_peripherals { profile.led.map(|l| l.pin) } else { None });
    if let Some(pin) = led_pin {
        let kind = if pin_of("led.pwm").unwrap_or(0) != 0 {
            "pwm"
        } else if pin_of("led.gpio").unwrap_or(0) != 0 {
            "gpio"
        } else {
            profile.led.map(|l| l.kind).unwrap_or("gpio")
        };
        let count =
            pin_of("led.count").unwrap_or(profile.led.map(|l| l.count as i32).unwrap_or(1)).max(1) as u16;
        next_suffix("led");
        devices.push(Box::new(actuator::RgbLed::new(pin, kind, count)));
    }

    // Servos and relays, one device per declared channel.
    let mut servos: Vec<i32> = Vec::new();
    let mut relays: Vec<i32> = Vec::new();
    for (k, v) in &cfg.pins {
        if k.starts_with("servo.") && k != "servo.min_us" && k != "servo.max_us" {
            servos.push(*v);
        }
        if k.starts_with("relay.") && !k.ends_with(".active_low") {
            relays.push(*v);
        }
    }
    servos.sort_unstable();
    relays.sort_unstable();
    for pin in servos {
        next_suffix("servo");
        devices.push(Box::new(actuator::Servo::new(pin)));
    }
    for pin in relays {
        let active_low = pin_of(&format!("relay.{pin}.active_low")).unwrap_or(0) != 0;
        next_suffix("relay");
        devices.push(Box::new(actuator::Relay::new(pin, active_low)));
    }

    if let (Some(trig), Some(echo)) = (pin_of("hcsr04.trig"), pin_of("hcsr04.echo")) {
        let mut dev = ranging::Hcsr04::new(trig, echo);
        if dev.probe(hw).is_ok() {
            next_suffix("hcsr04");
            devices.push(Box::new(dev));
        }
    }

    // I2S audio: nothing can be probed, so the pins come from the profile or
    // `config.pins` (`mic.bclk`/`mic.ws`/`mic.din`, `amp.bclk`/`amp.ws`/`amp.dout`), and
    // the channels either come up or the tools do not exist. The caps ride on the devices,
    // so a board that cannot bring I2S up never claims `mic`/`speaker`.
    let mic_pins = i2s_pins(cfg, profile.mic, "mic");
    let amp_pins = i2s_pins(cfg, profile.amp, "amp");
    if mic_pins.is_some() || amp_pins.is_some() {
        match hw.i2s_setup(mic_pins, amp_pins) {
            Ok(()) => {
                if mic_pins.is_some() {
                    let n = next_suffix("mic");
                    devices.push(Box::new(audio::Mic {
                        id: if n > 1 { format!("mic{n}") } else { "mic".into() },
                    }));
                }
                if amp_pins.is_some() {
                    let n = next_suffix("speaker");
                    devices.push(Box::new(audio::Speaker {
                        id: if n > 1 { format!("speaker{n}") } else { "speaker".into() },
                    }));
                }
                log.info("probe", "I2S up: microphone and/or amplifier available");
            }
            Err(e) => log.warn(
                "probe",
                format!("I2S pins are configured but the channels did not come up: {}", e.message),
            ),
        }
    }

    // `devices.discovery = manual`: the drivers still probe, but nothing is adopted on
    // the agent's behalf. What was found becomes a candidate list with the exact
    // declaration that would adopt it, so the next step is a `manifest_apply` rather than
    // a guess about which pins the driver wanted.
    let mut candidates = Vec::new();
    if cfg.discovery == "manual" {
        let withheld = devices.split_off_by_kind(|kind| !DECLARABLE.contains(&kind));
        for info in withheld.infos() {
            // The declaration is built the way `instantiate` reads it: `bus` is a number,
            // a missing address is omitted rather than sent as null, and `options` carries
            // what the probe learned (an OLED's height, an INA226's shunt).
            let mut decl = serde_json::Map::new();
            decl.insert("id".into(), json!(info.id));
            decl.insert("kind".into(), json!(info.kind));
            decl.insert("driver".into(), json!(info.kind));
            if let Some(address) = info.address {
                decl.insert("address".into(), json!(address));
            }
            let bus =
                info.detail.get("bus").and_then(|b| b.as_u64()).or_else(|| (info.bus == "i2c").then_some(0));
            if let Some(bus) = bus {
                decl.insert("bus".into(), json!(bus));
            }
            let mut options = serde_json::Map::new();
            for key in ["height", "controller", "shunt_mohm", "kind", "count"] {
                if let Some(value) = info.detail.get(key) {
                    options.insert(key.into(), json!(value.to_string().trim_matches('"')));
                }
            }
            if !options.is_empty() {
                decl.insert("options".into(), Value::Object(options));
            }
            if !info.pins.is_empty() {
                let mut pins = serde_json::Map::new();
                for (i, pin) in info.pins.iter().enumerate() {
                    pins.insert(format!("p{i}"), json!(pin));
                }
                pins.insert("out".into(), json!(info.pins[0]));
                decl.insert("pins".into(), Value::Object(pins));
            }
            let declarable = DECLARABLE.contains(&info.kind.as_str());
            candidates.push(json!({
                "id": info.id,
                "kind": info.kind,
                "name": info.name,
                "bus": info.bus,
                "address": info.address,
                "pins": info.pins,
                "caps": info.caps,
                "tools": info.tools,
                "adopt": if declarable {
                    json!({ "devices": [Value::Object(decl)] })
                } else {
                    Value::Null
                },
                "note": if declarable {
                    "manifest_apply {mode: \"merge\", manifest: adopt}"
                } else {
                    "provided by the board profile; there is nothing to declare"
                },
            }));
        }
        if !candidates.is_empty() {
            log.info(
                "probe",
                format!("{} device(s) found and left as candidates: discovery is manual", candidates.len()),
            );
        }
    }

    DetectResult { devices, unknown, buses: buses_json, candidates }
}

/// Build a device from a manifest declaration: the "a device I typed in" path.
pub fn instantiate(spec: &crate::manifest::DeviceSpec, hw: &mut dyn Hardware) -> Result<Box<dyn Device>> {
    use crate::error::McpError;
    let driver = spec.driver.as_deref().ok_or_else(|| McpError::invalid_params("device has no driver"))?;
    let bus = spec.bus.unwrap_or(0);
    let addr = spec.address;
    let pin = |name: &str| spec.pins.get(name).copied();
    let opt = |name: &str| spec.options.get(name).map(|s| s.as_str());
    let needs_addr = |a: Option<u8>| {
        a.ok_or_else(|| McpError::invalid_params(format!("driver `{driver}` needs an `address`")))
    };
    let needs_pin = |p: Option<i32>, name: &str| {
        p.ok_or_else(|| McpError::invalid_params(format!("driver `{driver}` needs pins.{name}")))
    };
    Ok(match driver {
        "ssd1306" => {
            let height = opt("height").and_then(|h| h.parse().ok()).unwrap_or(64u8);
            let controller = if opt("controller") == Some("sh1106") {
                ssd1306::Controller::Sh1106
            } else {
                ssd1306::Controller::Ssd1306
            };
            Box::new(ssd1306::Ssd1306::probe(hw, bus, needs_addr(addr)?, height, controller, 1)?)
        }
        "bme280" => Box::new(bme280::Bme280::probe(hw, bus, needs_addr(addr)?, 1)?),
        "sht3x" => Box::new(sht3x::Sht3x::probe(hw, bus, needs_addr(addr)?, 1)?),
        "ina219" => {
            let shunt = opt("shunt_mohm").and_then(|s| s.parse().ok()).unwrap_or(100u32);
            Box::new(ina219::Ina219::probe(hw, bus, needs_addr(addr)?, shunt, 1)?)
        }
        "mpu6050" => Box::new(motion::Mpu6050::probe(hw, bus, needs_addr(addr)?, 1)?),
        "ads1115" => Box::new(ads1115::Ads1115::probe(hw, bus, needs_addr(addr)?, 1)?),
        "pcf8574" => Box::new(expander::Pcf8574::probe(hw, bus, needs_addr(addr)?, 1)?),
        "ds18b20" => {
            let pin = needs_pin(pin("data").or_else(|| pin("out")), "data")?;
            match onewire::Ds18b20::probe(hw, pin)? {
                Some(d) => Box::new(d),
                None => return Err(McpError::not_found(format!("no 1-Wire device on GPIO{pin}"))),
            }
        }
        "hcsr04" => {
            let trig = needs_pin(pin("trig"), "trig")?;
            let echo = needs_pin(pin("echo"), "echo")?;
            let mut d = ranging::Hcsr04::new(trig, echo);
            d.probe(hw)?;
            Box::new(d)
        }
        "servo" => Box::new(actuator::Servo::new(needs_pin(pin("out").or_else(|| pin("pwm")), "out")?)),
        "relay" => {
            Box::new(actuator::Relay::new(needs_pin(pin("out"), "out")?, opt("active_low") == Some("true")))
        }
        "led" => Box::new(actuator::RgbLed::new(
            needs_pin(pin("out").or_else(|| pin("data")), "out")?,
            match opt("kind") {
                Some("ws2812") => "ws2812",
                Some("pwm") => "pwm",
                _ => "gpio",
            },
            opt("count").and_then(|c| c.parse().ok()).unwrap_or(1),
        )),
        // `pins` is a name→pin map, which is exactly what a button set is.
        "buttons" => {
            let mut buttons: Vec<(String, i32)> =
                spec.pins.iter().map(|(name, pin)| (name.clone(), *pin)).collect();
            buttons.sort_by(|a, b| a.0.cmp(&b.0));
            if buttons.is_empty() {
                return Err(McpError::invalid_params("driver `buttons` needs pins, e.g. {\"boot\": 0}"));
            }
            Box::new(actuator::Buttons::new(buttons))
        }
        other => {
            return Err(McpError::invalid_params(format!(
                "no compiled driver `{other}`; use primitives (gpio/pwm/adc/i2c) in the manifest instead"
            )))
        }
    })
}

/// Board-level capabilities that are not owned by a device.
///
/// `hw` is kept in the signature: a capability that needs the driver's state (rather than
/// the fact that the platform has the peripheral) reads it here, and the callers already
/// have it in hand.
pub fn board_caps(hw: &mut dyn Hardware, cfg: &Config, profile: &BoardProfile) -> Vec<String> {
    let _ = hw;
    let mut caps = Vec::new();
    // The radio exists on this build whether or not it has come up yet. Asking the
    // driver for its state at probe time hid `wifi_scan` and `wifi_status` on a boot
    // where the probe ran before the radio did — which is the common case on the board,
    // and the two tools are exactly what tell you the radio is not up. Measured on the
    // device: a fresh flash listed 43 tools instead of 45 for that reason.
    caps.push("wifi".to_string());
    caps.push("gpio".to_string());
    if !profile.adc_pins.is_empty() {
        caps.push("adc".to_string());
    }
    caps.push("pwm".to_string());
    if !cfg.bus.i2c.is_empty() {
        caps.push("i2c".to_string());
    }
    // Deliberately not advertised: `mic` and `speaker`. A profile can record the I2S
    // pins (the xiaozhi wiring does: mic 4/5/6, amp 7/15/16), but there is no INMP441 or
    // MAX98357A driver yet, so those caps would promise the model a capability that has
    // no tool behind it — and I2S has no addressing or id register, so nothing about it
    // can be probed either. They come back here when the drivers land.
    caps
}

/// The I2S pins for one direction: `config.pins` wins over the board profile, and both
/// `bclk` and `ws` must be present and non-negative for the side to count as wired.
fn i2s_pins(
    cfg: &Config,
    hint: Option<crate::boards::I2sPins>,
    prefix: &str,
) -> Option<crate::boards::I2sPins> {
    let get = |suffix: &str| cfg.pins.get(&format!("{prefix}.{suffix}")).copied();
    let bclk = get("bclk").or_else(|| hint.map(|h| h.bclk));
    let ws = get("ws").or_else(|| hint.map(|h| h.ws));
    let din = get("din").or_else(|| hint.map(|h| h.din));
    let dout = get("dout").or_else(|| hint.map(|h| h.dout));
    match (bclk, ws) {
        (Some(bclk), Some(ws)) if bclk >= 0 && ws >= 0 => {
            Some(crate::boards::I2sPins { bclk, ws, din: din.unwrap_or(-1), dout: dout.unwrap_or(-1) })
        }
        _ => None,
    }
}

/// Device counts per kind, for the portal's summary line.
pub fn summary(devices: &Devices) -> Value {
    let infos: Vec<DeviceInfo> = devices.infos();
    let mut by_kind = serde_json::Map::new();
    for i in &infos {
        let n = by_kind.get(&i.kind).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        by_kind.insert(i.kind.clone(), json!(n));
    }
    json!({
        "total": infos.len(),
        "by_kind": by_kind,
        "degraded": infos.iter().filter(|i| i.status != "ok").count(),
    })
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    /// A profile can record I2S pins (the xiaozhi wiring does: mic 4/5/6, amp 7/15/16),
    /// but nothing on an I2S bus can be identified and there is no audio driver yet. The
    /// capability must not be advertised, or the model is told about a device that has no
    /// tool behind it — which is what this board reported before the fix.
    #[test]
    fn the_audio_pins_of_a_profile_do_not_become_capabilities() {
        let mut hw = crate::virtual_board::VirtualBoard::new();
        let cfg = crate::config::Config::default();
        let profile = crate::boards::find("esp32s3-xiaozhi-bread").expect("the xiaozhi profile");
        assert!(profile.mic.is_some() && profile.amp.is_some(), "the profile records I2S pins");
        let caps = board_caps(&mut hw, &cfg, profile);
        assert!(!caps.iter().any(|c| c == "mic"), "mic advertised with no driver: {caps:?}");
        assert!(!caps.iter().any(|c| c == "speaker"), "speaker advertised with no driver: {caps:?}");
        assert!(caps.iter().any(|c| c == "gpio"), "the real caps are still there: {caps:?}");
    }
}

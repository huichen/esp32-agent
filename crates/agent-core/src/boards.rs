//! Board profiles: the pin facts that cannot be probed.
//!
//! A bus scan finds what is wired to I2C. It cannot find a WS2812 on GPIO48, or
//! know that the speaker sits on I2S with LRCK=16. A profile records those
//! facts; `board: "auto"` picks one from the SoC, flash size and MAC, and any
//! value can be overridden per pin in the config (`pins` map).

use crate::hardware::BoardInfo;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct I2cHint {
    pub sda: i32,
    pub scl: i32,
    pub freq_hz: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct I2sPins {
    pub bclk: i32,
    pub ws: i32,
    pub din: i32,
    pub dout: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LedHint {
    pub pin: i32,
    /// `ws2812` (single addressable pixel) or `gpio` (plain LED).
    pub kind: &'static str,
    pub count: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ButtonHint {
    pub pin: i32,
    pub name: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BoardProfile {
    pub id: &'static str,
    pub name: &'static str,
    /// SoC family this profile is for: `esp32`, `esp32s2`, `esp32s3`, `esp32c3`,
    /// `esp32c6`, `esp32h2`.
    pub soc: &'static str,
    pub flash_mb: u32,
    pub psram_kb: u32,
    pub i2c: &'static [I2cHint],
    pub led: Option<LedHint>,
    pub buttons: &'static [ButtonHint],
    pub mic: Option<I2sPins>,
    pub amp: Option<I2sPins>,
    pub adc_pins: &'static [i32],
    /// UART pins used by the console (never used for peripherals).
    pub reserved: &'static [i32],
    pub notes: &'static str,
}

const I2C_8_9: &[I2cHint] = &[I2cHint { sda: 8, scl: 9, freq_hz: 400_000 }];
const I2C_41_42: &[I2cHint] = &[I2cHint { sda: 41, scl: 42, freq_hz: 400_000 }];
const I2C_21_22: &[I2cHint] = &[I2cHint { sda: 21, scl: 22, freq_hz: 400_000 }];
const I2C_6_7: &[I2cHint] = &[I2cHint { sda: 6, scl: 7, freq_hz: 400_000 }];

pub static PROFILES: &[BoardProfile] = &[
    BoardProfile {
        id: "esp32s3-devkitc1-n16r8",
        name: "ESP32-S3-DevKitC-1 (N16R8)",
        soc: "esp32s3",
        flash_mb: 16,
        psram_kb: 8192,
        i2c: I2C_8_9,
        led: Some(LedHint { pin: 48, kind: "ws2812", count: 1 }),
        buttons: &[ButtonHint { pin: 0, name: "boot" }],
        mic: None,
        amp: None,
        adc_pins: &[1, 2, 3, 4, 5, 6, 7, 8, 9],
        reserved: &[19, 20, 43, 44],
        notes: "USB-Serial/JTAG on 19/20; GPIO48 is the onboard WS2812.",
    },
    BoardProfile {
        id: "esp32s3-xiaozhi-bread",
        name: "ESP32-S3 breadboard (xiaozhi wiring)",
        soc: "esp32s3",
        flash_mb: 16,
        psram_kb: 8192,
        i2c: I2C_41_42,
        led: Some(LedHint { pin: 48, kind: "ws2812", count: 1 }),
        buttons: &[
            ButtonHint { pin: 0, name: "left" },
            ButtonHint { pin: 39, name: "middle" },
            ButtonHint { pin: 40, name: "right" },
        ],
        mic: Some(I2sPins { bclk: 5, ws: 4, din: 6, dout: -1 }),
        amp: Some(I2sPins { bclk: 15, ws: 16, din: -1, dout: 7 }),
        adc_pins: &[1, 2, 3, 4, 5, 6, 7, 8, 9],
        reserved: &[19, 20, 43, 44],
        notes:
            "OLED SSD1306 on SDA41/SCL42, INMP441 mic (BCLK5/WS4/DIN6), MAX98357A amp (BCLK15/WS16/DOUT7), \
                the devkit's ws2812 on GPIO48. A strip on GPIO18 is wired on this bench but is a manifest \
                device, not part of this profile.",
    },
    BoardProfile {
        id: "esp32dev",
        name: "ESP32 DevKit v1 (WROOM-32)",
        soc: "esp32",
        flash_mb: 4,
        psram_kb: 0,
        i2c: I2C_21_22,
        led: Some(LedHint { pin: 2, kind: "gpio", count: 1 }),
        buttons: &[ButtonHint { pin: 0, name: "boot" }],
        mic: None,
        amp: None,
        adc_pins: &[32, 33, 34, 35, 36, 39],
        reserved: &[1, 3, 6, 7, 8, 9, 10, 11],
        notes: "Classic WROOM-32 dev board; GPIO2 LED is active-high.",
    },
    BoardProfile {
        id: "esp32c3-devkitm-1",
        name: "ESP32-C3-DevKitM-1",
        soc: "esp32c3",
        flash_mb: 4,
        psram_kb: 0,
        i2c: I2C_8_9,
        led: Some(LedHint { pin: 8, kind: "ws2812", count: 1 }),
        buttons: &[ButtonHint { pin: 9, name: "boot" }],
        mic: None,
        amp: None,
        adc_pins: &[0, 1, 2, 3, 4],
        reserved: &[20, 21],
        notes: "Single-core RISC-V; GPIO8 is the onboard RGB pixel.",
    },
    BoardProfile {
        id: "esp32c6-devkitc-1",
        name: "ESP32-C6-DevKitC-1",
        soc: "esp32c6",
        flash_mb: 8,
        psram_kb: 0,
        i2c: I2C_6_7,
        led: Some(LedHint { pin: 8, kind: "ws2812", count: 1 }),
        buttons: &[ButtonHint { pin: 9, name: "boot" }],
        mic: None,
        amp: None,
        adc_pins: &[0, 1, 2, 3, 4],
        reserved: &[16, 17],
        notes: "WiFi 6 + Thread/Zigbee; keep GPIO8/9 free if you use the RGB LED.",
    },
    BoardProfile {
        id: "generic",
        name: "Generic board",
        soc: "any",
        flash_mb: 4,
        psram_kb: 0,
        i2c: I2C_8_9,
        led: None,
        buttons: &[],
        mic: None,
        amp: None,
        adc_pins: &[],
        reserved: &[],
        notes: "Only the probed buses are used; set pins yourself in the portal.",
    },
];

pub fn profiles() -> &'static [BoardProfile] {
    PROFILES
}

pub fn find(id: &str) -> Option<&'static BoardProfile> {
    PROFILES.iter().find(|p| p.id == id)
}

/// Pick the profile for a running chip. Flash size narrows it down when several
/// profiles share a SoC, and the fallback is always `generic` rather than an
/// error, because a wrong guess only costs a probe.
pub fn auto(info: &BoardInfo) -> &'static BoardProfile {
    let soc = info.soc.as_str();
    let candidates: Vec<&'static BoardProfile> =
        PROFILES.iter().filter(|p| p.soc == soc && p.soc != "any").collect();
    if candidates.is_empty() {
        return find("generic").expect("generic profile exists");
    }
    if candidates.len() == 1 {
        return candidates[0];
    }
    // Same SoC, different flash: prefer the one with a matching size.
    candidates.iter().find(|p| p.flash_mb == info.flash_mb).copied().unwrap_or(candidates[0])
}

/// Resolve `board: "auto"` or an explicit id. Unknown ids fall back to the
/// auto-detected profile and are reported by the caller.
pub fn resolve(requested: &str, info: &BoardInfo) -> (&'static BoardProfile, bool) {
    let requested = requested.trim();
    if requested.is_empty() || requested.eq_ignore_ascii_case("auto") {
        return (auto(info), true);
    }
    match find(requested) {
        Some(p) => (p, true),
        None => (auto(info), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(soc: &str, flash_mb: u32) -> BoardInfo {
        BoardInfo { soc: soc.into(), flash_mb, ..Default::default() }
    }

    #[test]
    fn auto_prefers_matching_flash_size() {
        assert_eq!(auto(&info("esp32s3", 16)).id, "esp32s3-devkitc1-n16r8");
        assert_eq!(auto(&info("esp32", 4)).id, "esp32dev");
        assert_eq!(auto(&info("esp32c3", 4)).id, "esp32c3-devkitm-1");
    }

    #[test]
    fn unknown_soc_falls_back_to_generic() {
        assert_eq!(auto(&info("esp32s2", 4)).id, "generic");
    }

    #[test]
    fn resolve_reports_bad_ids() {
        let i = info("esp32s3", 16);
        let (p, ok) = resolve("esp32dev", &i);
        assert_eq!(p.id, "esp32dev");
        assert!(ok);
        let (p, ok) = resolve("nope", &i);
        assert!(!ok);
        assert_eq!(p.id, "esp32s3-devkitc1-n16r8");
    }

    /// The note has to agree with the pins the profile itself declares.
    ///
    /// `esp32s3-xiaozhi-bread` said "strip on GPIO18" while wiring its LED to 48, so `inventory`
    /// answered two different things: a device list with the ws2812 on 48, and a note naming 18.
    /// The GPIO18 strip is real — it is a manifest device on this bench, not profile wiring —
    /// and a note that does not say so sends the reader looking for a device that is not there.
    #[test]
    fn the_bench_profile_note_names_the_led_pin_it_wires() {
        let p = find("esp32s3-xiaozhi-bread").expect("the bench profile");
        let led = p.led.expect("this profile wires an LED");
        assert!(
            p.notes.contains(&format!("GPIO{}", led.pin)),
            "the note does not name GPIO{}: {}",
            led.pin,
            p.notes
        );
        assert!(
            p.notes.contains("GPIO18") && p.notes.contains("manifest"),
            "the strip on 18 is wired here and is not the profile's: {}",
            p.notes
        );
    }

    #[test]
    fn every_profile_has_a_unique_id_and_docs() {
        let mut ids: Vec<&str> = PROFILES.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n);
        assert!(PROFILES.iter().all(|p| !p.notes.is_empty()));
    }
}

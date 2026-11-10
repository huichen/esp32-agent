//! Capability ids and sets.
//!
//! A capability is a stable string such as `oled` or `bme280`. Devices publish
//! them, tools require them, and a tool whose requirements are not met never
//! reaches `tools/list`. That is the whole mechanism behind "the model only sees
//! hardware that is actually on the board".

use serde_json::{json, Value};

/// Capability ids used by the built-in drivers. A community driver may publish
/// its own id; nothing needs to be registered centrally.
pub mod cap {
    pub const WIFI: &str = "wifi";
    pub const GPIO: &str = "gpio";
    pub const ADC: &str = "adc";
    pub const PWM: &str = "pwm";
    pub const LED: &str = "led";
    pub const BUTTON: &str = "button";
    pub const I2C: &str = "i2c";
    pub const ONEWIRE: &str = "onewire";

    pub const OLED: &str = "oled";
    pub const LCD: &str = "lcd";
    pub const BME280: &str = "bme280";
    pub const SHT3X: &str = "sht3x";
    pub const INA219: &str = "ina219";
    pub const MPU6050: &str = "mpu6050";
    pub const ADS1115: &str = "ads1115";
    pub const PCF8574: &str = "pcf8574";
    pub const DS18B20: &str = "ds18b20";
    pub const HCSR04: &str = "hcsr04";
    pub const SERVO: &str = "servo";
    pub const RELAY: &str = "relay";
    pub const MIC: &str = "mic";
    pub const SPEAKER: &str = "speaker";
    pub const RTC: &str = "rtc";
    pub const SDCARD: &str = "sdcard";
}

/// An ordered, deduplicated capability set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapSet {
    items: Vec<String>,
}

impl CapSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a capability set from anything iterable.
    pub fn from_caps<I: IntoIterator<Item = S>, S: Into<String>>(it: I) -> Self {
        let mut s = Self::new();
        for c in it {
            s.insert(c);
        }
        s
    }

    pub fn insert(&mut self, cap: impl Into<String>) {
        let cap = cap.into();
        if !cap.is_empty() && !self.items.contains(&cap) {
            self.items.push(cap);
        }
    }

    pub fn extend<I: IntoIterator<Item = S>, S: Into<String>>(&mut self, it: I) {
        for c in it {
            self.insert(c);
        }
    }

    pub fn contains(&self, cap: &str) -> bool {
        self.items.iter().any(|c| c == cap)
    }

    pub fn contains_all(&self, caps: &[String]) -> bool {
        caps.iter().all(|c| self.contains(c))
    }

    pub fn missing(&self, caps: &[String]) -> Vec<String> {
        caps.iter().filter(|c| !self.contains(c)).cloned().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn to_json(&self) -> Value {
        json!(self.items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_and_membership() {
        let mut s = CapSet::new();
        s.insert("oled");
        s.insert("oled");
        s.insert("bme280");
        assert_eq!(s.len(), 2);
        assert!(s.contains("bme280"));
        assert!(!s.contains("ina219"));
    }

    #[test]
    fn contains_all_needs_every_id() {
        let s = CapSet::from_caps(["oled", "i2c"]);
        assert!(s.contains_all(&["oled".into(), "i2c".into()]));
        assert!(!s.contains_all(&["oled".into(), "bme280".into()]));
        assert_eq!(s.missing(&["oled".into(), "bme280".into()]), vec!["bme280".to_string()]);
    }
}

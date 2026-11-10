//! Small codecs shared by the drivers: base64, CRCs, byte order helpers.

use crate::error::{McpError, Result};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Decode base64 (standard or URL-safe, padding optional). Data URLs are
/// accepted so a bitmap pasted from a browser works unchanged.
pub fn b64_decode(s: &str, out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    let s = s.trim();
    let s = match s.find(',') {
        Some(i) if s[..i].starts_with("data:") => &s[i + 1..],
        _ => s,
    };
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        let v = b64_val(c).ok_or_else(|| {
            McpError::invalid_params(format!("base64: unexpected character {:?}", c as char))
        })?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Ok(())
}

pub fn b64_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        s.push(B64[(n >> 18) as usize & 63] as char);
        s.push(B64[(n >> 12) as usize & 63] as char);
        s.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    s
}

/// CRC-8 with polynomial 0x31, init 0xFF: Sensirion (SHT3x) and 1-Wire data.
pub fn crc8_poly31(data: &[u8]) -> u8 {
    let mut crc = 0xFFu8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x31 } else { crc << 1 };
        }
    }
    crc
}

/// Dallas/Maxim CRC-8 (poly 0x31 reflected, init 0) used by DS18B20 scratchpads.
pub fn crc8_dallas(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        let mut d = b;
        for _ in 0..8 {
            let mix = (crc ^ d) & 1;
            crc >>= 1;
            if mix != 0 {
                crc ^= 0x8C;
            }
            d >>= 1;
        }
    }
    crc
}

pub fn u16_be(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

pub fn i16_be(b: &[u8], i: usize) -> i16 {
    i16::from_be_bytes([b[i], b[i + 1]])
}

pub fn u16_le(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

pub fn i16_le(b: &[u8], i: usize) -> i16 {
    i16::from_le_bytes([b[i], b[i + 1]])
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_and_padding() {
        for len in 0..8 {
            let data: Vec<u8> = (0..len as u8).collect();
            let enc = b64_encode(&data);
            let mut dec = Vec::new();
            b64_decode(&enc, &mut dec).unwrap();
            assert_eq!(dec, data, "len {len}");
        }
    }

    #[test]
    fn base64_accepts_data_url() {
        let mut out = Vec::new();
        b64_decode("data:image/png;base64,AAEC", &mut out).unwrap();
        assert_eq!(out, vec![0, 1, 2]);
    }

    #[test]
    fn sensirion_crc_matches_datasheet_example() {
        // 0xBEEF -> 0x92 is the example in the SHT3x datasheet.
        assert_eq!(crc8_poly31(&[0xBE, 0xEF]), 0x92);
    }

    #[test]
    fn dallas_crc_is_stable_for_a_known_scratchpad() {
        // +25.0625 °C; the CRC byte is what this implementation computes for the
        // eight data bytes (Maxim AN27 polynomial 0x31 reflected, init 0).
        let data = [0x91u8, 0x01, 0x4B, 0x46, 0x7F, 0xFF, 0x0C, 0x10];
        assert_eq!(crc8_dallas(&data), 0x70);
        let mut flipped = data;
        flipped[7] = 0x11;
        assert_ne!(crc8_dallas(&flipped), 0x70);
    }

    #[test]
    fn little_endian_helpers() {
        assert_eq!(u16_le(&[0x34, 0x12], 0), 0x1234);
        assert_eq!(i16_le(&[0xFF, 0xFF], 0), -1);
        assert_eq!(hex(&[0x0a, 0xff]), "0aff");
    }
}

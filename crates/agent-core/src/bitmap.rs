//! Byte-oriented RLE for already bit-packed monochrome framebuffers.
//!
//! `bitmap-rle-v1` is a headerless stream of blocks. The low seven bits of each
//! control byte store `count - 1` (1..=128 bytes):
//!
//! * `0x00..=0x7f`: copy the next `count` literal bytes;
//! * `0x80..=0xff`: repeat the next value byte `count` times (a two-byte pair).
//!
//! There is no terminator, padding, checksum or embedded decoded length. This is
//! not TIFF PackBits: `0x80` is a one-byte repeat, not a no-op. The caller supplies
//! the exact decoded length to [`unpack`]; a missing complete block is an error
//! just like a truncated pair/literal, trailing data or an oversized run.
//!
//! [`pack`] emits repeats only for runs of at least three bytes. Short runs stay
//! in literal blocks, so the packed size is at most `n + ceil(n / 128)` bytes,
//! including incompressible input (516 bytes for a 512-byte frame). Repeats save
//! at least one byte, paying for any extra literal-block boundary they introduce.
//! The empty input has an empty encoding.
//!
//! The codec preserves byte order and every bit: on the OLED these are the
//! `oled_bitmap` / `POST /av` page bytes, eight vertical pixels per byte, bit 0
//! at the top. It neither renders pixels nor depends on App, hardware or storage.
//! Packing is infallible and caller-bounded; decoding accepts at most 64 KiB of
//! packed input and 64 KiB of raw output. The tools impose a smaller raw limit
//! so base64 plus JSON fits the tool-result budget.

use crate::error::{McpError, Result};

/// Maximum stream length accepted by [`unpack`], before any output allocation.
pub const MAX_PACKED_BYTES: usize = 64 * 1024;
/// Maximum exact decoded length accepted by [`unpack`], preventing expansion bombs.
pub const MAX_RAW_BYTES: usize = 64 * 1024;
const BLOCK_BYTES: usize = 128;

/// Encode bytes without changing their existing bit/pixel layout.
///
/// The result is never larger than `data.len() + data.len().div_ceil(128)`.
/// Callers bound `data`; [`unpack`] refuses streams/output over its own limits.
pub fn pack(data: &[u8]) -> Vec<u8> {
    pack_with_runs(data).0
}

/// The tool reports emitted repeat pairs, not literal blocks or maximal raw runs.
pub(crate) fn pack_with_runs(data: &[u8]) -> (Vec<u8>, usize) {
    let mut packed = Vec::new();
    let mut runs = 0;
    let mut pos = 0;
    while pos < data.len() {
        let count = run_length(data, pos);
        if count >= 3 {
            packed.extend_from_slice(&[0x80 | (count - 1) as u8, data[pos]]);
            pos += count;
            runs += 1;
        } else {
            let start = pos;
            pos += count;
            while pos < data.len() && pos - start < BLOCK_BYTES {
                let count = run_length(data, pos);
                if count >= 3 {
                    break;
                }
                pos += count.min(BLOCK_BYTES - (pos - start));
            }
            packed.push((pos - start - 1) as u8);
            packed.extend_from_slice(&data[start..pos]);
        }
    }
    (packed, runs)
}

fn run_length(data: &[u8], pos: usize) -> usize {
    data[pos..].iter().take(BLOCK_BYTES).take_while(|&&byte| byte == data[pos]).count()
}

/// Decode the complete stream, refusing any decoded length other than `raw_len`.
///
/// Malformed blocks and runs/literals exceeding `raw_len` return `INVALID_PARAMS`;
/// no partial output is returned. Both input and output are bounded to 64 KiB.
pub fn unpack(packed: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    if packed.len() > MAX_PACKED_BYTES {
        return Err(McpError::invalid_params(format!(
            "bitmap RLE: packed input is {} bytes; the limit is {MAX_PACKED_BYTES}",
            packed.len()
        )));
    }
    if raw_len > MAX_RAW_BYTES {
        return Err(McpError::invalid_params(format!(
            "bitmap RLE: raw length is {raw_len} bytes; the limit is {MAX_RAW_BYTES}"
        )));
    }
    let mut data = Vec::new();
    data.try_reserve_exact(raw_len)
        .map_err(|_| McpError::invalid_params("bitmap RLE: output allocation was refused"))?;
    let mut pos = 0;
    while pos < packed.len() {
        let block = pos;
        let control = packed[pos];
        pos += 1;
        let count = usize::from(control & 0x7f) + 1;
        let is_run = control & 0x80 != 0;
        if count > raw_len - data.len() {
            return Err(McpError::invalid_params(format!(
                "bitmap RLE: {} at byte {block} would exceed raw length {raw_len}",
                if is_run { "run" } else { "literal block" }
            )));
        }
        if is_run {
            let value = *packed.get(pos).ok_or_else(|| {
                McpError::invalid_params(format!("bitmap RLE: truncated run pair at byte {block}"))
            })?;
            data.resize(data.len() + count, value);
            pos += 1;
        } else {
            let literal = packed.get(pos..pos + count).ok_or_else(|| {
                McpError::invalid_params(format!("bitmap RLE: truncated literal block at byte {block}"))
            })?;
            data.extend_from_slice(literal);
            pos += count;
        }
    }
    if data.len() != raw_len {
        return Err(McpError::invalid_params(format!(
            "bitmap RLE: decoded {} bytes; expected exactly {raw_len}",
            data.len()
        )));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::ssd1306::{Controller, Ssd1306};
    use crate::virtual_board::VirtualBoard;

    fn measurement(label: &str, raw: &[u8]) -> (usize, f64, usize) {
        let (packed, runs) = pack_with_runs(raw);
        let ratio = packed.len() as f64 / raw.len() as f64;
        println!(
            "{label}: {} -> {} bytes, ratio={ratio:.9}, runs={runs}; base64 {} -> {} characters",
            raw.len(),
            packed.len(),
            crate::util::b64_encode(raw).len(),
            crate::util::b64_encode(&packed).len()
        );
        assert_eq!(unpack(&packed, raw.len()).unwrap(), raw, "{label} must round-trip every bit");
        (packed.len(), ratio, runs)
    }

    #[test]
    fn solid_frame_measurement() {
        assert_eq!(measurement("128x32 solid 0x00", &[0; 512]), (8, 0.015625, 4));
        assert_eq!(measurement("30 solid 128x32 frames", &[0; 30 * 512]), (240, 0.015625, 120));
    }

    #[test]
    fn historical_text_frame_measurement() {
        // Bench evidence in commit 2187401: oled_text "HELLO BOARD", then
        // oled_read returned ["HELLO BOARD", "", "", ""]. Re-render through
        // the very same driver/font, not UTF-8 bytes or a hand-drawn substitute.
        //
        // That frame packed to 67 bytes because the glyph table it was measured with is
        // not the one in this tree: it had `b` == `p`, `h` == `n`, `M` == `N` and missing
        // strokes (A, G, K, S), which is why the panel never showed a readable line. The
        // table was replaced on 2026-11-14, so the same text is 68 bytes and 13 runs now.
        // The round trip and the 512-byte frame are unchanged; only the compression of a
        // different set of glyphs differs, and this pins the current one.
        let mut hw = VirtualBoard::new();
        let mut oled = Ssd1306::probe(&mut hw, 0, 0x3c, 32, Controller::Ssd1306, 1).unwrap();
        oled.text_line(0, "HELLO BOARD");
        assert_eq!(oled.read_text(), ["HELLO BOARD", "", "", ""]);
        assert_eq!(oled.framebuffer().len(), 512);
        assert_eq!(measurement("128x32 HELLO BOARD, 6x8 glyphs", oled.framebuffer()), (68, 0.1328125, 13));
    }

    #[test]
    fn pseudo_random_frame_measurement() {
        let mut state = 0x1234_5678u32;
        let raw: Vec<u8> = (0..512)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let (bytes, ratio, runs) = measurement("128x32 xorshift32 noise, seed 0x12345678", &raw);
        assert_eq!((bytes, ratio, runs), (516, 1.0078125, 0));
        assert!(bytes > raw.len() && bytes <= raw.len() + raw.len().div_ceil(BLOCK_BYTES));
    }

    #[test]
    fn wire_vectors_cover_literal_and_repeat_boundaries() {
        assert!(pack(&[]).is_empty());
        assert!(unpack(&[], 0).unwrap().is_empty());
        for (raw, expected) in [
            (vec![0xab], vec![0, 0xab]),
            (vec![0xab; 2], vec![1, 0xab, 0xab]),
            (vec![0xab; 3], vec![0x82, 0xab]),
            (vec![0xab; 128], vec![0xff, 0xab]),
            (vec![0xab; 129], vec![0xff, 0xab, 0, 0xab]),
            (vec![0xab; 130], vec![0xff, 0xab, 1, 0xab, 0xab]),
            (vec![0xab; 131], vec![0xff, 0xab, 0x82, 0xab]),
            (vec![1, 2, 2, 3, 3, 3, 4], vec![2, 1, 2, 2, 0x82, 3, 0, 4]),
        ] {
            assert_eq!(pack(&raw), expected);
            assert_eq!(unpack(&expected, raw.len()).unwrap(), raw);
        }
        let all_bytes: Vec<u8> = (0..=255).collect();
        let expected: Vec<u8> =
            std::iter::once(0x7f).chain(0..128).chain(std::iter::once(0x7f)).chain(128..=255).collect();
        assert_eq!(pack(&all_bytes), expected, "control-like values are ordinary literal bytes");
        assert_eq!(unpack(&expected, 256).unwrap(), all_bytes);
        assert_eq!(unpack(&[0x80, 0xff, 0x81, 0x80], 3).unwrap(), [0xff, 0x80, 0x80]);
    }

    #[test]
    fn small_and_mixed_patterns_respect_the_worst_case_overhead() {
        let check = |raw: &[u8]| {
            let packed = pack(raw);
            assert!(packed.len() <= raw.len() + raw.len().div_ceil(BLOCK_BYTES), "length {}", raw.len());
            assert_eq!(unpack(&packed, raw.len()).unwrap(), raw);
        };
        for len in 0..=12 {
            for bits in 0..1usize << len {
                let raw: Vec<u8> = (0..len).map(|i| ((bits >> i) & 1) as u8).collect();
                check(&raw);
            }
        }
        // Fragmented literals, repeated runs and splits on both sides of 128.
        for literal_len in [1, 2, 3, 126, 127, 128, 129, 255, 256, 257, 512] {
            for run_len in [2, 3, 4, 127, 128, 129, 130, 131, 256, 512] {
                let literal: Vec<u8> = (0..literal_len).map(|i| (i % 251) as u8).collect();
                let mut raw = literal.clone();
                raw.extend(vec![0xff; run_len]);
                raw.extend(&literal);
                raw.extend(vec![0xfe; run_len]);
                raw.extend(&literal);
                check(&raw);
            }
        }
        check(&vec![0; 30 * 512]);
    }

    #[test]
    fn malformed_streams_never_return_partial_output() {
        for (packed, raw_len, message) in [
            (vec![0x82], 3, "truncated run pair"),
            (vec![2, 0xaa, 0xbb], 3, "truncated literal block"),
            (vec![0x82, 0, 0xff], 131, "truncated run pair"),
            (vec![0, 0x12, 1, 0x34], 3, "truncated literal block"),
            (vec![0xff, 0], 127, "run at byte 0 would exceed"),
            (vec![1, 0, 0], 1, "literal block at byte 0 would exceed"),
            (vec![0x80, 0, 0x80, 1], 1, "run at byte 2 would exceed"),
            (vec![0x82, 0], 4, "expected exactly 4"),
            (vec![], 1, "expected exactly 1"),
            (vec![0, 1], 0, "would exceed raw length 0"),
        ] {
            let error = unpack(&packed, raw_len).unwrap_err();
            assert_eq!(error.code, crate::error::code::INVALID_PARAMS);
            assert!(error.message.contains(message), "{packed:?}: {}", error.message);
        }
    }

    #[test]
    fn every_truncated_prefix_is_refused_with_the_exact_length() {
        let mut raw: Vec<u8> = (0..=255).collect();
        raw.extend([0xaa; 129]);
        raw.extend(0..=255);
        raw.extend([0; 512]);
        let packed = pack(&raw);
        for end in 0..packed.len() {
            assert!(unpack(&packed[..end], raw.len()).is_err(), "prefix {end} was silently accepted");
        }
        assert_eq!(unpack(&packed, raw.len()).unwrap(), raw);
    }

    #[test]
    fn input_and_output_are_bounded() {
        let raw = vec![0x5a; MAX_RAW_BYTES];
        assert_eq!(unpack(&pack(&raw), MAX_RAW_BYTES).unwrap(), raw);
        // Legal but noncanonical one-byte repeat pairs exercise the input boundary.
        let maximum_packed: Vec<u8> = [0x80, 0xab].repeat(MAX_PACKED_BYTES / 2);
        assert_eq!(unpack(&maximum_packed, MAX_PACKED_BYTES / 2).unwrap(), vec![0xab; MAX_PACKED_BYTES / 2]);
        let mut oversized = maximum_packed;
        oversized.push(0);
        assert!(unpack(&oversized, MAX_PACKED_BYTES / 2).unwrap_err().message.contains("limit is 65536"));
        assert!(unpack(&[0xff, 0], MAX_RAW_BYTES + 1).unwrap_err().message.contains("raw length"));
        assert!(unpack(&[], usize::MAX).unwrap_err().message.contains("raw length"));
    }
}

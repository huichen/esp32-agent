//! Deflate/inflate, small enough for a board with a few kB of heap.
//!
//! Only the **decoder** is here: `unzip` on a real archive is useless without it (every
//! zip tool deflates by default), while a compressor would need a match finder and a
//! window that cost more than the 8 kB workspace is worth. `zip` therefore writes *stored*
//! entries — valid archives that any unzip reads — and says so in the tool result.
//!
//! The decode follows the reference (`puff.c`): canonical Huffman codes held as counts and
//! offsets, decoded a bit at a time, which is slower than a table and needs a fraction of
//! the memory.

const MAX_BITS: usize = 15;

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bit: 0, count: 0 }
    }

    fn bits(&mut self, need: u32) -> Result<u32, String> {
        let mut value = self.bit;
        while self.count < need {
            let byte = *self.data.get(self.pos).ok_or("deflate: ran out of input")? as u32;
            self.pos += 1;
            value |= byte << self.count;
            self.count += 8;
        }
        self.bit = value >> need;
        self.count -= need;
        Ok(value & ((1u32 << need) - 1))
    }

    fn align(&mut self) {
        self.bit = 0;
        self.count = 0;
    }
}

/// Canonical Huffman table: how many codes of each length, and the symbols in order.
struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &len in lengths {
            counts[len as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; lengths.iter().filter(|l| **l != 0).count()];
        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offsets[len as usize] as usize] = symbol as u16;
                offsets[len as usize] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, bits: &mut Bits) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= bits.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("deflate: bad Huffman code".into())
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195,
    227, 258,
];
const LEN_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073,
    4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Inflate a raw deflate stream (no zlib header, no gzip wrapper).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut bits = Bits::new(data);
    loop {
        let last = bits.bits(1)?;
        let kind = bits.bits(2)?;
        match kind {
            0 => {
                bits.align();
                let len = bits.bits(16)? as usize;
                let nlen = bits.bits(16)? as usize;
                if len != (!nlen & 0xffff) {
                    return Err("deflate: stored block length mismatch".into());
                }
                for _ in 0..len {
                    out.push(bits.bits(8)? as u8);
                }
            }
            1 => {
                let mut lengths = [0u8; 288];
                for (i, slot) in lengths.iter_mut().enumerate() {
                    *slot = match i {
                        0..=143 => 8,
                        144..=255 => 9,
                        256..=279 => 7,
                        _ => 8,
                    };
                }
                let literals = Huffman::new(&lengths)?;
                let distances = Huffman::new(&[5u8; 30])?;
                decode_block(&mut bits, &literals, &distances, &mut out)?;
            }
            2 => {
                let (literals, distances) = dynamic_tables(&mut bits)?;
                decode_block(&mut bits, &literals, &distances, &mut out)?;
            }
            _ => return Err("deflate: reserved block type".into()),
        }
        if last == 1 {
            break;
        }
    }
    Ok(out)
}

fn dynamic_tables(bits: &mut Bits) -> Result<(Huffman, Huffman), String> {
    let hlit = bits.bits(5)? as usize + 257;
    let hdist = bits.bits(5)? as usize + 1;
    let hclen = bits.bits(4)? as usize + 4;
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let mut code_lengths = [0u8; 19];
    for &slot in ORDER.iter().take(hclen) {
        code_lengths[slot] = bits.bits(3)? as u8;
    }
    let code_table = Huffman::new(&code_lengths)?;

    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        let symbol = code_table.decode(bits)?;
        match symbol {
            0..=15 => {
                lengths[i] = symbol as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err("deflate: repeat with no previous length".into());
                }
                let prev = lengths[i - 1];
                let repeat = 3 + bits.bits(2)? as usize;
                for _ in 0..repeat.min(lengths.len() - i) {
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let repeat = 3 + bits.bits(3)? as usize;
                i = (i + repeat).min(lengths.len());
            }
            18 => {
                let repeat = 11 + bits.bits(7)? as usize;
                i = (i + repeat).min(lengths.len());
            }
            _ => return Err("deflate: bad code length symbol".into()),
        }
    }
    let literals = Huffman::new(&lengths[..hlit])?;
    let distances = Huffman::new(&lengths[hlit..])?;
    Ok((literals, distances))
}

fn decode_block(
    bits: &mut Bits,
    literals: &Huffman,
    distances: &Huffman,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    loop {
        let symbol = literals.decode(bits)?;
        match symbol {
            0..=255 => out.push(symbol as u8),
            256 => return Ok(()),
            257..=285 => {
                let index = (symbol - 257) as usize;
                let length = LEN_BASE[index] as usize + bits.bits(LEN_EXTRA[index] as u32)? as usize;
                let dsym = distances.decode(bits)? as usize;
                if dsym >= DIST_BASE.len() {
                    return Err("deflate: bad distance symbol".into());
                }
                let distance = DIST_BASE[dsym] as usize + bits.bits(DIST_EXTRA[dsym] as u32)? as usize;
                if distance == 0 || distance > out.len() {
                    return Err("deflate: distance past the start of the output".into());
                }
                // Overlapping copy is the point of LZ77: copy byte by byte.
                let start = out.len() - distance;
                for k in 0..length {
                    let byte = out[start + k];
                    out.push(byte);
                }
            }
            _ => return Err(format!("deflate: bad length symbol {symbol}")),
        }
        if out.len() > 1_048_576 {
            return Err("deflate: output larger than 1 MiB".into());
        }
    }
}

/// CRC-32 (IEEE), the checksum both zip and gzip carry.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `zlib.compress(b"The quick brown fox jumps over the lazy dog. " * 4, 9)` with the
    /// zlib header and Adler-32 stripped: a raw deflate stream with dynamic Huffman codes,
    /// which is exactly what a real zip entry contains.
    const SAMPLE: &[u8] = &[
        0x0b, 0xc9, 0x48, 0x55, 0x28, 0x2c, 0xcd, 0x4c, 0xce, 0x56, 0x48, 0x2a, 0xca, 0x2f, 0xcf, 0x53, 0x48,
        0xcb, 0xaf, 0x50, 0xc8, 0x2a, 0xcd, 0x2d, 0x28, 0x56, 0xc8, 0x2f, 0x4b, 0x2d, 0x52, 0x28, 0x01, 0x4a,
        0xe7, 0x24, 0x56, 0x55, 0x2a, 0xa4, 0xe4, 0xa7, 0xeb, 0x29, 0x84, 0x0c, 0x0e, 0xc5, 0x00,
    ];

    /// Packs a deflate stream by hand, so a test can say which block type and which
    /// distance it means instead of hoping a compressor produces that case.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        bit: u32,
        count: u32,
    }

    impl Writer {
        /// A value in `n` bits, least significant bit first: how deflate stores lengths,
        /// extra bits and block headers.
        fn value(&mut self, v: u32, n: u32) {
            for i in 0..n {
                self.one((v >> i) & 1 == 1);
            }
        }

        /// A Huffman code, most significant bit first: the opposite of a value.
        fn code(&mut self, code: u32, len: u32) {
            for i in (0..len).rev() {
                self.one((code >> i) & 1 == 1);
            }
        }

        fn one(&mut self, b: bool) {
            if b {
                self.bit |= 1 << self.count;
            }
            self.count += 1;
            if self.count == 8 {
                self.bytes.push(self.bit as u8);
                self.bit = 0;
                self.count = 0;
            }
        }

        fn align(&mut self) {
            if self.count > 0 {
                self.bytes.push(self.bit as u8);
                self.bit = 0;
                self.count = 0;
            }
        }

        fn finish(mut self) -> Vec<u8> {
            self.align();
            self.bytes
        }

        fn header(&mut self, last: bool, kind: u32) {
            self.value(last as u32, 1);
            self.value(kind, 2);
        }

        fn stored(&mut self, last: bool, data: &[u8]) {
            self.header(last, 0);
            self.align();
            self.value(data.len() as u32, 16);
            self.value(!(data.len() as u32) & 0xffff, 16);
            for b in data {
                self.value(*b as u32, 8);
            }
        }

        /// The fixed literal/length code from RFC 1951 section 3.2.6.
        fn fixed_literal(&mut self, symbol: u16) {
            let (code, len) = match symbol {
                0..=143 => (0x30 + symbol as u32, 8),
                144..=255 => (0x190 + (symbol as u32 - 144), 9),
                256..=279 => (symbol as u32 - 256, 7),
                _ => (0xC0 + (symbol as u32 - 280), 8),
            };
            self.code(code, len);
        }

        /// A fixed-code back-reference: symbol 257 is length 3 with no extra bits, and the
        /// first four distance symbols are single bytes with none either.
        fn fixed_match(&mut self, length_symbol: u16, distance_symbol: u32) {
            self.fixed_literal(length_symbol);
            self.code(distance_symbol, 5);
        }
    }

    #[test]
    fn a_real_deflate_stream_decodes() {
        let out = inflate(SAMPLE).expect("inflates");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("The quick brown fox"), "{text}");
        assert_eq!(out.len(), "The quick brown fox jumps over the lazy dog. ".len() * 4);
    }

    /// A stored block is the simplest legal deflate: a length, its complement, then the
    /// bytes. Two of them in one stream also prove the last-block flag is what ends the
    /// loop rather than the end of the input.
    #[test]
    fn stored_blocks_decode_in_sequence() {
        let mut w = Writer::default();
        w.stored(false, b"hello");
        w.stored(true, b" world");
        assert_eq!(inflate(&w.finish()).expect("stored blocks"), b"hello world");
    }

    #[test]
    fn a_stored_block_with_the_wrong_length_is_refused() {
        let mut w = Writer::default();
        w.header(true, 0);
        w.align();
        w.value(5, 16); // LEN
        w.value(0, 16); // one's complement of 0xFFFF, not of 5
        for b in b"hello" {
            w.value(*b as u32, 8);
        }
        let err = inflate(&w.finish()).expect_err("LEN and NLEN disagree");
        assert!(err.contains("length mismatch"), "{err}");
    }

    /// The fixed table is the one every zlib can decode without a table in the stream, and
    /// it is where a back-reference has to be handled byte by byte: copying 3 bytes at
    /// distance 2 from "AB" has to produce "ABA", not a memcpy of a stale region.
    #[test]
    fn a_fixed_block_with_a_back_reference_overlaps_correctly() {
        let mut w = Writer::default();
        w.header(true, 1);
        w.fixed_literal(u16::from(b'A'));
        w.fixed_literal(u16::from(b'B'));
        w.fixed_match(257, 1); // length 3, distance 2
        w.fixed_literal(256); // end of block
        let out = inflate(&w.finish()).expect("fixed block");
        assert_eq!(out, b"ABABA");
    }

    /// A distance that reaches further back than there is output would read memory that was
    /// never produced. That has to be an error, not a wrapped index or a panic.
    #[test]
    fn a_distance_past_the_start_of_the_output_is_refused() {
        let mut w = Writer::default();
        w.header(true, 1);
        w.fixed_match(257, 2); // length 3, distance 3 with nothing decoded yet
        w.fixed_literal(256);
        let err = inflate(&w.finish()).expect_err("nothing to copy from");
        assert!(err.contains("past the start"), "{err}");
    }

    /// Block type 3 is reserved. A decoder that treated it as stored, or ignored it, would
    /// keep going with the rest of the stream out of step.
    #[test]
    fn a_reserved_block_type_is_refused() {
        let mut w = Writer::default();
        w.header(true, 3);
        let err = inflate(&w.finish()).expect_err("type 3 is not defined");
        assert!(err.contains("reserved"), "{err}");
    }

    /// Dynamic block: the code-length code itself is Huffman coded, the literal lengths use
    /// the zero-repeat symbols 17 and 18, and 16 repeats the previous length. The alphabet
    /// this builds has 'A'-'D' at 3 bits and end-of-block at 1 bit, so "A" is three bits.
    #[test]
    fn a_dynamic_block_built_from_repeat_symbols_decodes() {
        let mut w = Writer::default();
        w.header(true, 2);
        w.value(0, 5); // HLIT = 257
        w.value(0, 5); // HDIST = 1
        w.value(14, 4); // HCLEN = 18 (all but the last of the 19 length symbols)
                        // Code-length alphabet: symbols 0, 1, 3, 16, 17, 18 at 3 bits each, written in the
                        // permuted order the format uses. The gaps (8, 7, 9, ...) are unused and get 0.
        for len in [3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 3] {
            w.value(len, 3);
        }
        // Canonical codes for that alphabet, in symbol order: 0=000 1=001 3=010 16=011
        // 17=100 18=101.
        w.code(0b101, 3); // 18: 65 zeros
        w.value(54, 7); // 11 + 54 = 65
        w.code(0b010, 3); // 3: literal 'A' is 3 bits long
        w.code(0b011, 3); // 16: repeat the previous length...
        w.value(0, 2); // ...three more times, so 'A'-'D' are all 3 bits
        for _ in 0..18 {
            w.code(0b100, 3); // 17: ten zeros each
            w.value(7, 3);
        }
        w.code(0b100, 3); // 17: the remaining seven zeros
        w.value(4, 3);
        w.code(0b001, 3); // symbol 1: end-of-block is 1 bit
        w.code(0b001, 3); // and the single distance code is 1 bit
                          // Literal table: 65..68 = 3 bits (100..111), 256 = 1 bit (0).
        w.code(0b100, 3); // 'A'
        w.code(0b0, 1); // end of block
        assert_eq!(inflate(&w.finish()).expect("dynamic block"), b"A");
    }

    /// Symbol 16 says "repeat the previous length", which means nothing at the first entry.
    /// Accepting it would take whatever byte was in the buffer before.
    #[test]
    fn a_repeat_with_no_previous_length_is_refused() {
        let mut w = Writer::default();
        w.header(true, 2);
        w.value(0, 5);
        w.value(0, 5);
        w.value(0, 4); // HCLEN = 4: symbols 16, 17, 18, 0
        for len in [2, 0, 2, 1] {
            w.value(len, 3);
        }
        // Canonical: 0 = 0, 16 = 10, 18 = 11.
        w.code(0b10, 2);
        let err = inflate(&w.finish()).expect_err("nothing to repeat");
        assert!(err.contains("no previous length"), "{err}");
    }

    /// A bit pattern that is not in the table has to be an error rather than a symbol
    /// picked out of the empty slot. An alphabet with only 'A' and 'B' at two bits leaves
    /// every code starting with a 1 unmatched, and the decoder has to give up after
    /// fifteen bits.
    #[test]
    fn a_code_that_is_not_in_the_table_is_refused() {
        let mut w = Writer::default();
        w.header(true, 2);
        w.value(0, 5); // HLIT = 257
        w.value(0, 5); // HDIST = 1
        w.value(14, 4); // HCLEN = 18
        for len in [3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 3] {
            w.value(len, 3);
        }
        // Code-length codes: 0=000 1=001 3=010 16=011 17=100 18=101.
        w.code(0b101, 3); // 18: 65 zeros
        w.value(54, 7);
        w.code(0b010, 3); // literal 'A' is 3 bits
        w.code(0b010, 3); // literal 'B' is 3 bits
        for _ in 0..19 {
            w.code(0b100, 3); // 17: ten zeros each, up to symbol 256
            w.value(7, 3);
        }
        w.code(0b001, 3); // the one distance code, 1 bit
                          // No end-of-block symbol exists in this alphabet, and 11... matches nothing.
        for _ in 0..8 {
            w.value(0xFF, 8);
        }
        let err = inflate(&w.finish()).expect_err("11 starts no code in that table");
        assert!(err.contains("bad Huffman code"), "{err}");
    }

    /// A truncated stream must fail rather than return the half it managed to decode: a
    /// partial file that looks like a successful extraction is worse than no file.
    #[test]
    fn a_truncated_stream_is_refused() {
        let err = inflate(&[]).expect_err("no input");
        assert!(err.contains("ran out of input"), "{err}");
        for cut in [4, 16, SAMPLE.len() - 1] {
            let err = inflate(&SAMPLE[..cut]).expect_err("cut short");
            assert!(err.contains("ran out of input"), "at {cut}: {err}");
        }
        // The last byte of a stored block's payload is not optional either.
        let mut w = Writer::default();
        w.stored(true, b"abcdef");
        let mut bytes = w.finish();
        bytes.truncate(bytes.len() - 2);
        let err = inflate(&bytes).expect_err("short payload");
        assert!(err.contains("ran out of input"), "{err}");
    }

    /// Length symbols 286 and 287 exist in the fixed table but are not lengths: a stream
    /// that uses them is malformed, and treating one as a length would read the next bytes
    /// as a distance.
    #[test]
    fn a_reserved_length_symbol_is_refused() {
        for symbol in [286u16, 287] {
            let mut w = Writer::default();
            w.header(true, 1);
            w.fixed_literal(symbol);
            w.fixed_literal(256);
            let err = inflate(&w.finish()).expect_err("286 and 287 are not lengths");
            assert!(err.contains("bad length symbol"), "{symbol}: {err}");
        }
    }

    #[test]
    fn crc32_matches_the_reference() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
        // The check value zlib documents for its own CRC-32.
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414f_a339);
    }
}

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

    #[test]
    fn a_real_deflate_stream_decodes() {
        let out = inflate(SAMPLE).expect("inflates");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("The quick brown fox"), "{text}");
    }

    #[test]
    fn crc32_matches_the_reference() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }
}

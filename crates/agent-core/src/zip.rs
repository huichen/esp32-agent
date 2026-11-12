//! A minimal ZIP reader and writer.
//!
//! `read` handles what other tools produce — stored **and** deflated entries — because an
//! archive that cannot be opened is not an archive. `write` produces *stored* entries: a
//! correct ZIP that any unzip accepts, without a compressor (the workspace is 8 kB of text,
//! so the few bytes a deflate would save are not worth the match finder).

use crate::zlib;

const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL: u32 = 0x0605_4b50;
/// Refuse anything that would not fit through a 2 KB tool result anyway, with room for
/// the base64 wrapper.
const MAX_TOTAL: usize = 3 * 1024;

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Build a ZIP from `(name, bytes)` pairs, stored.
pub fn write(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut central: Vec<(String, u32, u32, u32)> = Vec::new(); // name, crc, size, offset
    for (name, bytes) in entries {
        if name.is_empty() || name.starts_with('/') || name.contains("..") {
            return Err(format!("`{name}` is not a relative path inside the archive"));
        }
        let offset = out.len() as u32;
        let crc = zlib::crc32(bytes);
        let size = bytes.len() as u32;
        out.extend_from_slice(&LOCAL_HEADER.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes()); // compressed
        out.extend_from_slice(&size.to_le_bytes()); // uncompressed
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(bytes);
        central.push((name.clone(), crc, size, offset));
    }
    let central_start = out.len() as u32;
    for (name, crc, size, offset) in &central {
        out.extend_from_slice(&CENTRAL_HEADER.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    let central_size = out.len() as u32 - central_start;
    out.extend_from_slice(&END_OF_CENTRAL.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with central
    out.extend_from_slice(&(central.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    Ok(out)
}

/// Read every entry of a ZIP, deflated entries included.
pub fn read(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    // The end-of-central-directory record is at the tail, after a comment of unknown
    // length; scan backwards for its signature.
    let mut eocd = None;
    let floor = data.len().saturating_sub(66 * 1024);
    let mut at = data.len().saturating_sub(22);
    loop {
        if at + 4 <= data.len() && u32_at(data, at) == Some(END_OF_CENTRAL) {
            eocd = Some(at);
            break;
        }
        if at == 0 || at <= floor {
            break;
        }
        at -= 1;
    }
    let eocd = eocd.ok_or("not a zip archive: no end-of-central-directory record")?;
    let count = u16_at(data, eocd + 10).ok_or("zip: truncated directory")? as usize;
    let dir_size = u32_at(data, eocd + 12).ok_or("zip: truncated directory")? as usize;
    let dir_start = u32_at(data, eocd + 16).ok_or("zip: truncated directory")? as usize;
    if dir_start + dir_size > data.len() {
        return Err("zip: central directory is past the end of the archive".into());
    }

    let mut out = Vec::new();
    let mut cursor = dir_start;
    let mut total = 0usize;
    for _ in 0..count {
        if u32_at(data, cursor) != Some(CENTRAL_HEADER) {
            return Err("zip: bad central directory entry".into());
        }
        let method = u16_at(data, cursor + 10).ok_or("zip: truncated entry")?;
        let crc = u32_at(data, cursor + 16).ok_or("zip: truncated entry")?;
        let compressed = u32_at(data, cursor + 20).ok_or("zip: truncated entry")? as usize;
        let name_len = u16_at(data, cursor + 28).ok_or("zip: truncated entry")? as usize;
        let extra_len = u16_at(data, cursor + 30).ok_or("zip: truncated entry")? as usize;
        let comment_len = u16_at(data, cursor + 32).ok_or("zip: truncated entry")? as usize;
        let local = u32_at(data, cursor + 42).ok_or("zip: truncated entry")? as usize;
        let name = String::from_utf8_lossy(
            data.get(cursor + 46..cursor + 46 + name_len).ok_or("zip: truncated name")?,
        )
        .to_string();

        // The local header repeats the name and extra fields, so the data starts after them.
        if u32_at(data, local) != Some(LOCAL_HEADER) {
            return Err(format!("zip: bad local header for `{name}`"));
        }
        let local_name = u16_at(data, local + 26).ok_or("zip: truncated local header")? as usize;
        let local_extra = u16_at(data, local + 28).ok_or("zip: truncated local header")? as usize;
        let start = local + 30 + local_name + local_extra;
        let raw =
            data.get(start..start + compressed).ok_or_else(|| format!("zip: `{name}` runs past the end"))?;

        let bytes = match method {
            0 => raw.to_vec(),
            8 => zlib::inflate(raw).map_err(|e| format!("zip: `{name}`: {e}"))?,
            other => {
                return Err(format!("zip: `{name}` uses compression method {other}, which is not supported"))
            }
        };
        if zlib::crc32(&bytes) != crc {
            return Err(format!("zip: `{name}` failed its CRC check"));
        }
        total += bytes.len();
        if total > MAX_TOTAL {
            return Err(format!("zip: entries total more than {MAX_TOTAL} bytes"));
        }
        out.push((name, bytes));
        cursor += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_archive_reads_back() {
        let entries =
            vec![("a.txt".to_string(), b"hello".to_vec()), ("dir/b.md".to_string(), b"# title\n".to_vec())];
        let archive = write(&entries).expect("writes");
        // The signature bytes a real unzip looks for.
        assert_eq!(&archive[..4], b"PK\x03\x04");
        let read = read(&archive).expect("reads");
        assert_eq!(read, entries);
    }

    #[test]
    fn a_deflated_archive_from_elsewhere_reads() {
        // `zipfile.ZipFile(..., ZIP_DEFLATED).writestr("hello.txt", "hello hello hello hello\\n")`
        const ARCHIVE: &[u8] = &[
            0x50, 0x4b, 0x03, 0x04, 0x14, 0x00, 0x00, 0x00, 0x08, 0x00, 0xc0, 0x52, 0x6c, 0x5d, 0x00, 0x88,
            0x59, 0x0b, 0x0b, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x68, 0x65,
            0x6c, 0x6c, 0x6f, 0x2e, 0x74, 0x78, 0x74, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27,
            0xb9, 0x00, 0x50, 0x4b, 0x01, 0x02, 0x14, 0x00, 0x14, 0x00, 0x00, 0x00, 0x08, 0x00, 0xc0, 0x52,
            0x6c, 0x5d, 0x00, 0x88, 0x59, 0x0b, 0x0b, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x09, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x2e, 0x74, 0x78, 0x74, 0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x37, 0x00, 0x00, 0x00, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let entries = read(ARCHIVE).expect("reads a deflated archive");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "hello.txt");
        assert_eq!(String::from_utf8_lossy(&entries[0].1), "hello hello hello hello\n");
    }
}

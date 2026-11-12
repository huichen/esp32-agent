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
    let mut out = Vec::with_capacity(4096);
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

    /// An archive with a comment puts the end-of-directory record before the end of the
    /// file, so the reader has to scan backwards for it. A file with no such record is not
    /// a zip and has to say so instead of reading whatever is at the end.
    #[test]
    fn the_end_of_directory_record_is_found_behind_a_comment() {
        let entries = vec![("a.txt".to_string(), b"hello".to_vec())];
        let archive = write(&entries).expect("writes");
        let mut commented = archive.clone();
        commented.extend_from_slice(b"a comment");
        assert_eq!(read(&commented).expect("a comment is allowed"), entries);

        let err = read(b"PK but not really a zip, just some bytes").err().expect("not an archive");
        assert!(err.contains("no end-of-central-directory"), "{err}");
    }

    /// The header fields are the only thing that says where the entries are, so a record
    /// that points past the end, or a signature that is not there, has to be refused
    /// rather than followed.
    #[test]
    fn a_header_that_points_at_nothing_is_refused() {
        let entries = vec![("a.txt".to_string(), b"hello".to_vec())];
        let archive = write(&entries).expect("writes");
        let central = archive
            .windows(4)
            .position(|w| w == CENTRAL_HEADER.to_le_bytes())
            .expect("a central directory record");
        let eocd = archive.windows(4).position(|w| w == END_OF_CENTRAL.to_le_bytes()).expect("an end record");

        let mut past_end = archive.clone();
        past_end[eocd + 16..eocd + 20].copy_from_slice(&(archive.len() as u32 + 100).to_le_bytes());
        let err = read(&past_end).err().expect("the directory is inside the file");
        assert!(err.contains("past the end"), "{err}");

        let mut bad_entry = archive.clone();
        bad_entry[central] ^= 0xFF;
        let err = read(&bad_entry).err().expect("a central entry starts with a signature");
        assert!(err.contains("bad central directory entry"), "{err}");

        let mut bad_local = archive.clone();
        bad_local[0] ^= 0xFF;
        let err = read(&bad_local).err().expect("so does a local header");
        assert!(err.contains("bad local header"), "{err}");
    }

    /// The reader refuses an archive whose entries would not fit through a tool result
    /// anyway: unpacking 10 kB to then throw it away is not worth the heap.
    #[test]
    fn an_archive_larger_than_the_tool_result_budget_is_refused() {
        let entries = vec![("big.bin".to_string(), vec![0u8; MAX_TOTAL + 1])];
        let archive = write(&entries).expect("writes");
        let err = read(&archive).err().expect("too much to unpack");
        assert!(err.contains("more than"), "{err}");
    }

    /// A path that is absolute, empty or climbs out of the archive is not a name inside it,
    /// and a writer that accepted one would produce an archive whose extraction escapes the
    /// directory it is unpacked into.
    #[test]
    fn a_name_outside_the_archive_is_refused_when_writing() {
        for bad in ["", "/etc/passwd", "../secret", "a/../../b"] {
            let err = write(&[(bad.to_string(), b"x".to_vec())]).err().expect("not a relative name");
            assert!(err.contains("relative path"), "{bad}: {err}");
        }
    }

    /// A method this build cannot decode is refused by name rather than returning the
    /// compressed bytes as if they were the file.
    #[test]
    fn an_unsupported_compression_method_is_refused() {
        let entries = vec![("a.txt".to_string(), b"hello".to_vec())];
        let archive = write(&entries).expect("writes");
        let mut corrupt = archive.clone();
        let central = corrupt
            .windows(4)
            .position(|w| w == CENTRAL_HEADER.to_le_bytes())
            .expect("a central directory record");
        corrupt[central + 10] = 99; // the compression method the reader will see
        let err = read(&corrupt).err().expect("method 99 is not supported");
        assert!(err.contains("compression method 99"), "{err}");
    }

    /// A payload that no longer matches the CRC in the header must not be handed back as if
    /// it were the file: a silently corrupted extraction is worse than a refused one.
    #[test]
    fn a_payload_that_does_not_match_its_crc_is_refused() {
        let entries = vec![("a.txt".to_string(), b"hello".to_vec())];
        let archive = write(&entries).expect("writes");
        let name_len = u16_at(&archive, 26).expect("a name length") as usize;
        let mut corrupt = archive.clone();
        corrupt[30 + name_len] = b'j'; // one byte of the stored payload
        let err = read(&corrupt).err().expect("a corrupted entry is not a file");
        assert!(err.contains("CRC"), "{err}");
    }

    /// The same for a deflated entry: the decompressed bytes are what the CRC covers.
    #[test]
    fn a_deflated_payload_that_does_not_match_its_crc_is_refused() {
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
        let mut corrupt = ARCHIVE.to_vec();
        // The CRC field of the central directory record, changed but the payload left
        // alone: the entry decompresses, and then the check is the only thing standing
        // between the caller and a file that is not the one that was archived.
        let central = corrupt
            .windows(4)
            .position(|w| w == CENTRAL_HEADER.to_le_bytes())
            .expect("a central directory record");
        corrupt[central + 16] ^= 0xFF;
        let err = read(&corrupt).err().expect("a wrong CRC is not a file");
        assert!(err.contains("CRC"), "{err}");
    }
}

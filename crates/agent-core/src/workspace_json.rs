//! The two persisted workspace shapes, parsed without an unbounded JSON string buffer.
//!
//! This is deliberately not a general JSON parser: a version-1 workspace has only
//! `version` and `files`, and an index maps logical paths to derived physical keys.
//! Strings are decoded incrementally into a fallibly grown, byte-limited buffer.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{BufReader, Read, Take};

use super::{
    AppWorkspace, WORKSPACE_FILE_MAX, WORKSPACE_NAMES_MAX, WORKSPACE_RAW_PATH_MAX, WORKSPACE_TOTAL_MAX,
};

/// Six encoded bytes per decoded byte covers `\uXXXX`, including control characters.
/// Raw paths may contain redundant separators, but are independently bounded too.
pub(super) const DOCUMENT_MAX: usize = 6
    * (WORKSPACE_TOTAL_MAX + super::WORKSPACE_FILES_MAX * WORKSPACE_RAW_PATH_MAX)
    + 32 * super::WORKSPACE_FILES_MAX
    + 128;
pub(super) const INDEX_MAX: usize = 6 * super::WORKSPACE_FILES_MAX * WORKSPACE_RAW_PATH_MAX
    + (6 * crate::store::KEY_MAX + 8) * super::WORKSPACE_FILES_MAX
    + 2;

pub(super) fn document(reader: impl Read) -> Result<AppWorkspace, serde_json::Error> {
    Parser::new(reader, DOCUMENT_MAX).document()
}

pub(super) fn index(reader: impl Read) -> Result<BTreeMap<String, String>, serde_json::Error> {
    let mut parser = Parser::new(reader, INDEX_MAX);
    let mut entries = BTreeMap::<String, String>::new();
    let mut names = 0usize;
    parser.expect(b'{')?;
    let mut first = true;
    while let Some(path) = parser.key(&mut first, WORKSPACE_RAW_PATH_MAX)? {
        let path = AppWorkspace::file_path(&path).map_err(|e| parser.error(e))?;
        if entries.keys().any(|existing| AppWorkspace::path_conflicts(existing, &path)) {
            return Err(parser.error("duplicate or file/directory-colliding path in workspace index"));
        }
        if entries.len() >= super::WORKSPACE_FILES_MAX {
            return Err(parser.error("workspace file-count limit exceeded"));
        }
        names = names.checked_add(path.len()).ok_or_else(|| parser.error("filename size overflow"))?;
        if names > WORKSPACE_NAMES_MAX {
            return Err(parser.error("workspace filename budget exceeded"));
        }
        let key = parser.string(crate::store::KEY_MAX)?;
        if key != crate::keys::workspace_file(&path) {
            return Err(parser.error("workspace index key does not match its canonical path"));
        }
        if entries.values().any(|existing| existing == &key) {
            return Err(parser.error("workspace index contains a physical-key collision"));
        }
        entries.insert(path, key);
    }
    parser.finish()?;
    Ok(entries)
}

/// Read one indexed file without allocating its over-limit suffix. The extra byte
/// distinguishes an exactly-full file from a truncated oversized one.
pub(super) fn text(mut reader: impl Read, limit: usize) -> Result<String, serde_json::Error> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let want = chunk.len().min(limit.saturating_sub(bytes.len()) + 1);
        let count = match reader.read(&mut chunk[..want]) {
            Ok(count) => count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(serde_json::Error::io(e)),
        };
        if count == 0 {
            return String::from_utf8(bytes).map_err(|_| invalid("workspace file is not UTF-8"));
        }
        append(&mut bytes, &chunk[..count], limit)?;
    }
}

fn invalid(message: impl std::fmt::Display) -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom(message)
}

fn append(out: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), serde_json::Error> {
    let len = out.len().checked_add(bytes.len()).ok_or_else(|| invalid("string size overflow"))?;
    if len > limit {
        return Err(invalid(format!("workspace string exceeds its {limit}-byte limit")));
    }
    out.try_reserve(bytes.len()).map_err(|_| invalid("workspace string allocation was refused"))?;
    out.extend_from_slice(bytes);
    Ok(())
}

struct Parser<R: Read> {
    input: BufReader<Take<R>>,
    peeked: Option<u8>,
    read: usize,
    limit: usize,
}

impl<R: Read> Parser<R> {
    fn new(reader: R, limit: usize) -> Self {
        Self {
            input: BufReader::with_capacity(8192, reader.take(limit as u64 + 1)),
            peeked: None,
            read: 0,
            limit,
        }
    }

    fn error(&self, message: impl std::fmt::Display) -> serde_json::Error {
        invalid(format!("{message} at workspace byte {}", self.read))
    }

    fn peek(&mut self) -> Result<Option<u8>, serde_json::Error> {
        if self.peeked.is_none() {
            let mut byte = [0u8; 1];
            loop {
                match self.input.read(&mut byte) {
                    Ok(0) => return Ok(None),
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(serde_json::Error::io(e)),
                }
            }
            self.read += 1;
            if self.read > self.limit {
                return Err(self.error("encoded workspace input limit exceeded"));
            }
            self.peeked = Some(byte[0]);
        }
        Ok(self.peeked)
    }

    fn byte(&mut self) -> Result<u8, serde_json::Error> {
        self.peek()?.ok_or_else(|| self.error("unexpected end of workspace JSON"))?;
        Ok(self.peeked.take().expect("peek supplied a byte"))
    }

    fn space(&mut self) -> Result<(), serde_json::Error> {
        while matches!(self.peek()?, Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.peeked = None;
        }
        Ok(())
    }

    fn expect(&mut self, expected: u8) -> Result<(), serde_json::Error> {
        self.space()?;
        if self.byte()? != expected {
            return Err(self.error(format!("expected `{}`", expected as char)));
        }
        Ok(())
    }

    fn key(&mut self, first: &mut bool, limit: usize) -> Result<Option<String>, serde_json::Error> {
        self.space()?;
        if self.peek()? == Some(b'}') {
            self.byte()?;
            return Ok(None);
        }
        if !*first {
            self.expect(b',')?;
        }
        *first = false;
        let key = self.string(limit)?;
        self.expect(b':')?;
        Ok(Some(key))
    }

    fn hex4(&mut self) -> Result<u16, serde_json::Error> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.byte()? {
                b @ b'0'..=b'9' => b - b'0',
                b @ b'a'..=b'f' => b - b'a' + 10,
                b @ b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.error("invalid Unicode escape")),
            };
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }

    fn string(&mut self, limit: usize) -> Result<String, serde_json::Error> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            match self.byte()? {
                b'"' => return String::from_utf8(out).map_err(|_| self.error("invalid UTF-8 string")),
                b'\\' => match self.byte()? {
                    b'u' => {
                        let first = self.hex4()?;
                        let point = match first {
                            0xd800..=0xdbff => {
                                if self.byte()? != b'\\' || self.byte()? != b'u' {
                                    return Err(self.error("missing low Unicode surrogate"));
                                }
                                let second = self.hex4()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err(self.error("invalid low Unicode surrogate"));
                                }
                                0x10000 + (((first as u32 - 0xd800) << 10) | (second as u32 - 0xdc00))
                            }
                            0xdc00..=0xdfff => return Err(self.error("unpaired low Unicode surrogate")),
                            _ => first as u32,
                        };
                        let ch = char::from_u32(point).ok_or_else(|| self.error("invalid Unicode point"))?;
                        let mut encoded = [0; 4];
                        append(&mut out, ch.encode_utf8(&mut encoded).as_bytes(), limit)?;
                    }
                    escaped => {
                        let byte = match escaped {
                            b'"' | b'\\' | b'/' => escaped,
                            b'b' => 8,
                            b'f' => 12,
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            _ => return Err(self.error("invalid JSON escape")),
                        };
                        append(&mut out, &[byte], limit)?;
                    }
                },
                0..=0x1f => return Err(self.error("unescaped control character")),
                byte => append(&mut out, &[byte], limit)?,
            }
        }
    }

    fn document(mut self) -> Result<AppWorkspace, serde_json::Error> {
        let mut workspace = AppWorkspace::default();
        let (mut version, mut files) = (false, false);
        self.expect(b'{')?;
        let mut first = true;
        while let Some(field) = self.key(&mut first, 16)? {
            match field.as_str() {
                "version" if !version => {
                    version = true;
                    self.expect(b'1')?;
                    if !matches!(self.peek()?, Some(b',' | b'}' | b' ' | b'\t' | b'\r' | b'\n')) {
                        return Err(self.error("workspace version must be the integer 1"));
                    }
                }
                "files" if !files => {
                    files = true;
                    self.expect(b'{')?;
                    let mut first_file = true;
                    while let Some(path) = self.key(&mut first_file, WORKSPACE_RAW_PATH_MAX)? {
                        workspace.check_insert(&path, 0, false).map_err(|e| self.error(e))?;
                        let allowance = WORKSPACE_FILE_MAX.min(WORKSPACE_TOTAL_MAX - workspace.bytes());
                        let text = self.string(allowance)?;
                        workspace
                            .insert_checked(&path, Cow::Owned(text), false)
                            .map_err(|e| self.error(e))?;
                    }
                }
                _ => return Err(self.error("unknown or duplicate workspace field")),
            }
        }
        self.finish()?;
        Ok(workspace)
    }

    fn finish(&mut self) -> Result<(), serde_json::Error> {
        self.space()?;
        if self.peek()?.is_some() {
            return Err(self.error("trailing data after workspace JSON"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_limits_cover_escaped_keys_but_stop_unbounded_whitespace() {
        let error = Parser::new(std::io::repeat(b' '), 64).document().unwrap_err();
        assert!(error.to_string().contains("encoded workspace input limit"), "{error}");

        fn escaped(out: &mut String, text: &str) {
            use std::fmt::Write as _;
            out.push('"');
            for byte in text.bytes() {
                write!(out, "\\u{byte:04x}").unwrap();
            }
            out.push('"');
        }
        // Valid raw names can normalize down to the aggregate-name budget. Physical
        // keys can also use six-byte escapes: their encoded cost is not just KEY_MAX.
        let mut input = String::with_capacity(INDEX_MAX);
        input.push('{');
        for n in 0..super::super::WORKSPACE_FILES_MAX {
            if n > 0 {
                input.push(',');
            }
            let path = format!("/n{n:04}{}", "p".repeat(128 - 6));
            let raw = format!("{}{path}", "/".repeat(WORKSPACE_RAW_PATH_MAX - path.len()));
            escaped(&mut input, &raw);
            input.push(':');
            escaped(&mut input, &crate::keys::workspace_file(&path));
        }
        input.push('}');
        println!("fully escaped valid index: {} encoded bytes", input.len());
        assert_eq!(index(input.as_bytes()).unwrap().len(), super::super::WORKSPACE_FILES_MAX);

        let mut bytes = vec![b'x'; 8];
        let capacity = bytes.capacity();
        assert!(append(&mut bytes, b"x", 8).is_err());
        assert_eq!(bytes.len(), 8);
        assert_eq!(bytes.capacity(), capacity, "over-limit append must not reserve");
    }
}

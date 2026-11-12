//! Persistence abstraction: NVS on the ESP32, a JSON file on the host.
//!
//! On top of the [`KvStore`] trait sits the one thing every large blob on this board
//! needs: [`write_blob`] / [`read_blob`], which spread a value too long for NVS over
//! several smaller ones and check it back chunk by chunk. See [`VALUE_MAX`] for why a
//! single value cannot simply be bigger.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// The most bytes one stored value may hold.
///
/// ESP-IDF refuses a longer string with `ESP_ERR_NVS_VALUE_TOO_LONG`, and the firmware
/// reads through a fixed 4096-byte buffer (`firmware/src/nvs_store.rs`), so a longer value
/// is not a slow value: it is one that cannot be written and cannot be read back. A blob
/// bigger than this is what [`write_blob`] exists for.
pub const VALUE_MAX: usize = 4000;

/// A blob too long for one value is split into chunks of this size: below [`VALUE_MAX`]
/// with room for the NUL terminator and for whatever an NVS entry adds around a value.
pub const CHUNK_BYTES: usize = 3500;

/// How one blob is spread over values, and how to tell it from one written before it was
/// chunked.
#[derive(Debug, Clone, Copy)]
pub struct Plan {
    /// The first word of the header, and the format's version: `"ws1"` is the file
    /// workspace, first layout. A value under the base key that does not start with it
    /// was written by a build that stored the blob whole, and is read as [`BlobState::Legacy`].
    pub tag: &'static str,
    /// Bytes per value. Never more than [`VALUE_MAX`].
    pub chunk_bytes: usize,
    /// How many values the blob may span. The ceiling this implies
    /// ([`Plan::capacity`]) is what [`write_blob`] refuses to exceed.
    pub chunks_max: usize,
}

impl Plan {
    /// The most bytes the stored blob may hold: what an error message must name.
    pub const fn capacity(&self) -> usize {
        self.chunk_bytes * self.chunks_max
    }
}

/// What is under one base key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobState {
    /// Nothing was ever stored there.
    Missing,
    /// A blob this version wrote, every chunk checked against the header.
    Stored(String),
    /// One value under the base key, written before the blob was chunked: the format an
    /// older build wrote. The caller decides whether it can read it, and migrates it.
    Legacy(String),
    /// A header or a chunk that does not check out, with the reason. Nothing may be
    /// adopted from it: half a blob is worse than none.
    Corrupt(String),
}

/// Where one chunk of a blob lives. Chunk 1 owns `base.1`, because the base key itself
/// holds the header that says whether the chunks belong together.
fn chunk_key(base: &str, index: usize) -> String {
    format!("{base}.{index}")
}

/// Split at UTF-8 character boundaries: a chunk has to be a `String` on its own, so a cut
/// inside a multi-byte character would make the value unreadable rather than short.
fn split(blob: &str, chunk_bytes: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, ch) in blob.char_indices() {
        let width = ch.len_utf8();
        if used + width > chunk_bytes {
            out.push(&blob[start..index]);
            start = index;
            used = 0;
        }
        used += width;
    }
    if start < blob.len() {
        out.push(&blob[start..]);
    }
    out
}

/// The header line: the tag, how many chunks, how many bytes in total, and one checksum
/// per chunk.
///
/// The count is what makes a stale tail harmless (a shorter blob leaves older chunks
/// behind, and they are not read), and the per-chunk checksums are what make a torn write
/// visible: a chunk from another write, or one cut in half, does not match the header that
/// is supposed to describe it.
fn header_line(plan: Plan, total: usize, chunks: &[&str]) -> String {
    let mut out = format!("{} {} {} ", plan.tag, chunks.len(), total);
    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("{:016x}", crate::hash::fnv1a(chunk.as_bytes())));
    }
    out
}

/// Store `blob` under `base`, spread over as many values as it needs.
///
/// Returns how many chunks it took, or an error naming the ceiling and the size that broke
/// it. Three properties this function is responsible for, and why:
///
/// * **Nothing is written when it cannot fit.** The size is checked before the first byte
///   is stored, so a refused blob leaves the previous one exactly as it was. A caller that
///   saved half a workspace would hand back a file list no boot ever wrote.
/// * **The header is the commit point.** Chunks are written first, the header last, so a
///   power cut in between leaves a header that no longer matches its chunks: [`read_blob`]
///   then reports [`BlobState::Corrupt`] and the whole blob is dropped instead of being
///   assembled out of two different versions.
/// * **Every chunk is under [`VALUE_MAX`].** Otherwise the write "succeeds" here and the
///   NVS refuses it on the board — the failure this whole module exists to end.
pub fn write_blob(
    store: &mut dyn KvStore,
    base: &str,
    blob: &str,
    plan: Plan,
) -> std::result::Result<usize, String> {
    let chunks = split(blob, plan.chunk_bytes);
    if chunks.len() > plan.chunks_max {
        return Err(format!(
            "it is {} bytes, one stored value holds at most {} bytes and the store keeps {} of them ({} bytes in total), so it needs {} values; nothing was written",
            blob.len(),
            plan.chunk_bytes,
            plan.chunks_max,
            plan.capacity(),
            chunks.len()
        ));
    }
    if plan.chunk_bytes > VALUE_MAX {
        // A plan, not a caller, decides this; the check is here so a future plan cannot
        // quietly reintroduce a chunk the board refuses.
        return Err(format!(
            "the storage plan asks for {}-byte chunks and one value holds at most {VALUE_MAX}; nothing was written",
            plan.chunk_bytes
        ));
    }
    let header = header_line(plan, blob.len(), &chunks);
    for (index, chunk) in chunks.iter().enumerate() {
        let key = chunk_key(base, index + 1);
        if !store.set(&key, chunk) {
            return Err(format!(
                "`{key}` could not be written (chunk {} of {}, {} bytes); the stored copy is left for the next read to judge",
                index + 1,
                chunks.len(),
                chunk.len()
            ));
        }
    }
    if !store.set(base, &header) {
        return Err(format!(
            "the `{base}` header could not be written, so the {} chunk(s) are not usable; nothing was adopted",
            chunks.len()
        ));
    }
    // A blob shorter than the one it replaced must not leave that one's tail behind: the
    // count in the header already stops it being read, and removing it keeps the NVS from
    // filling up with chunks nothing points at.
    for index in chunks.len() + 1..=plan.chunks_max {
        let key = chunk_key(base, index);
        if store.get(&key).is_none() {
            break;
        }
        store.remove(&key);
    }
    Ok(chunks.len())
}

/// Read back what [`write_blob`] wrote, or say why it cannot be used.
pub fn read_blob(store: &dyn KvStore, base: &str, plan: Plan) -> BlobState {
    let Some(header) = store.get(base) else {
        return BlobState::Missing;
    };
    let Some(rest) = header.strip_prefix(plan.tag).and_then(|r| r.strip_prefix(' ')) else {
        return BlobState::Legacy(header);
    };
    let mut fields = rest.split(' ');
    let (Some(count), Some(total)) = (fields.next(), fields.next()) else {
        return BlobState::Corrupt(format!("the `{base}` header is incomplete: `{}`", clip(&header)));
    };
    let (Ok(count), Ok(total)) = (count.parse::<usize>(), total.parse::<usize>()) else {
        return BlobState::Corrupt(format!("the `{base}` header is not numbers: `{}`", clip(&header)));
    };
    if count > plan.chunks_max {
        return BlobState::Corrupt(format!(
            "the `{base}` header describes {count} values and the store keeps at most {}",
            plan.chunks_max
        ));
    }
    if count == 0 {
        // An empty blob is a header with no chunks; anything else here is a header written
        // by something that never finished.
        return if total == 0 {
            BlobState::Stored(String::new())
        } else {
            BlobState::Corrupt(format!("the `{base}` header says {total} bytes in no values at all"))
        };
    }
    let Some(checksums) = fields.next() else {
        return BlobState::Corrupt(format!("the `{base}` header lists no checksums: `{}`", clip(&header)));
    };
    let mut expected = Vec::with_capacity(count);
    for field in checksums.split(',') {
        match u64::from_str_radix(field, 16) {
            Ok(value) => expected.push(value),
            Err(_) => {
                return BlobState::Corrupt(format!("the `{base}` header has a bad checksum: `{field}`"))
            }
        }
    }
    if expected.len() != count {
        return BlobState::Corrupt(format!(
            "the `{base}` header lists {} checksum(s) for {count} values",
            expected.len()
        ));
    }
    let mut blob = String::with_capacity(total);
    for (index, want) in expected.iter().enumerate() {
        let key = chunk_key(base, index + 1);
        let Some(chunk) = store.get(&key) else {
            return BlobState::Corrupt(format!(
                "`{key}` is missing, so {} of {count} value(s) are there",
                index
            ));
        };
        let got = crate::hash::fnv1a(chunk.as_bytes());
        if got != *want {
            return BlobState::Corrupt(format!(
                "`{key}` does not match the checksum in `{base}` (a write torn by a power cut, or a chunk from another write, looks exactly like this)"
            ));
        }
        blob.push_str(&chunk);
    }
    if blob.len() != total {
        return BlobState::Corrupt(format!(
            "`{base}` says {total} bytes and its {count} value(s) hold {}",
            blob.len()
        ));
    }
    BlobState::Stored(blob)
}

/// Remove the base key and every chunk that could be under it.
pub fn clear_blob(store: &mut dyn KvStore, base: &str, plan: Plan) -> bool {
    let mut removed = store.remove(base);
    for index in 1..=plan.chunks_max {
        let key = chunk_key(base, index);
        if store.get(&key).is_none() {
            continue;
        }
        removed = store.remove(&key) || removed;
    }
    removed
}

/// The first 40 characters of a header, for a log line that should not quote a blob.
fn clip(text: &str) -> String {
    text.chars().take(40).collect()
}

/// A tiny string key-value store, which is exactly what ESP-IDF NVS is.
pub trait KvStore: Send {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&mut self, key: &str, value: &str) -> bool;
    fn remove(&mut self, key: &str) -> bool;
    fn keys(&self) -> Vec<String>;
}

/// In-memory store: used by tests and by a simulator run started without a
/// state file.
#[derive(Debug, Default)]
pub struct MemoryStore {
    map: Mutex<BTreeMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KvStore for MemoryStore {
    fn get(&self, key: &str) -> Option<String> {
        self.map.lock().ok()?.get(key).cloned()
    }

    fn set(&mut self, key: &str, value: &str) -> bool {
        match self.map.lock() {
            Ok(mut m) => {
                m.insert(key.to_string(), value.to_string());
                true
            }
            Err(_) => false,
        }
    }

    fn remove(&mut self, key: &str) -> bool {
        match self.map.lock() {
            Ok(mut m) => m.remove(key).is_some(),
            Err(_) => false,
        }
    }

    fn keys(&self) -> Vec<String> {
        match self.map.lock() {
            Ok(m) => m.keys().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// A single JSON object on disk. The simulator uses it so a config edited in the
/// browser survives a restart, the same way NVS does on the board.
#[derive(Debug)]
pub struct FileStore {
    path: PathBuf,
    map: BTreeMap<String, String>,
}

impl FileStore {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(&s).ok())
            .unwrap_or_default();
        Self { path, map }
    }

    fn flush(&self) -> bool {
        let s = serde_json::to_string_pretty(&self.map).unwrap_or_else(|_| "{}".into());
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename keeps a power cut from leaving half a file.
        let tmp = self.path.with_extension("json.tmp");
        match std::fs::write(&tmp, s) {
            Ok(()) => std::fs::rename(&tmp, &self.path).is_ok(),
            Err(_) => false,
        }
    }
}

impl KvStore for FileStore {
    fn get(&self, key: &str) -> Option<String> {
        self.map.get(key).cloned()
    }

    fn set(&mut self, key: &str, value: &str) -> bool {
        self.map.insert(key.to_string(), value.to_string());
        self.flush()
    }

    fn remove(&mut self, key: &str) -> bool {
        let had = self.map.remove(key).is_some();
        self.flush();
        had
    }

    fn keys(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan the file workspace uses, so the tests exercise the numbers that ship.
    const PLAN: Plan = Plan { tag: "ws1", chunk_bytes: CHUNK_BYTES, chunks_max: 5 };

    /// The board's rule, on the host: a value above [`VALUE_MAX`] is refused, exactly as
    /// ESP-IDF refuses it. Without this, a chunk size that only works in a `String` would
    /// pass every test here and persist nothing on the board.
    #[derive(Debug, Default)]
    struct LimitedStore {
        map: BTreeMap<String, String>,
    }

    impl KvStore for LimitedStore {
        fn get(&self, key: &str) -> Option<String> {
            self.map.get(key).cloned()
        }

        fn set(&mut self, key: &str, value: &str) -> bool {
            if value.len() > VALUE_MAX {
                return false;
            }
            self.map.insert(key.to_string(), value.to_string());
            true
        }

        fn remove(&mut self, key: &str) -> bool {
            self.map.remove(key).is_some()
        }

        fn keys(&self) -> Vec<String> {
            self.map.keys().cloned().collect()
        }
    }

    #[test]
    fn memory_store_roundtrips() {
        let mut s = MemoryStore::new();
        assert!(s.set("a", "1"));
        assert_eq!(s.get("a").as_deref(), Some("1"));
        assert!(s.remove("a"));
        assert_eq!(s.get("a"), None);
    }

    #[test]
    fn file_store_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("mcp-store-test-{}", std::process::id()));
        let path = dir.join("kv.json");
        {
            let mut s = FileStore::open(&path);
            assert!(s.set("config", "{\"a\":1}"));
        }
        let s = FileStore::open(&path);
        assert_eq!(s.get("config").as_deref(), Some("{\"a\":1}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blob_too_long_for_one_value_round_trips_through_several() {
        let mut store = LimitedStore::default();
        // Two values' worth and a bit: the case that used to write nothing at all.
        let blob: String = (0..9000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let chunks = write_blob(&mut store, "ws", &blob, PLAN).expect("the blob fits the plan");
        assert_eq!(chunks, 3, "9000 bytes over {CHUNK_BYTES}-byte values");
        assert_eq!(read_blob(&store, "ws", PLAN), BlobState::Stored(blob));
        // The base key is the header and nothing else; the data lives in `.1`, `.2`, ...
        assert!(store.get("ws").unwrap().starts_with("ws1 3 9000 "), "{:?}", store.get("ws"));
        assert_eq!(store.get("ws.1").unwrap().len(), CHUNK_BYTES);
        assert!(store.get("ws.4").is_none(), "no empty chunk is written past the end");
    }

    #[test]
    fn every_stored_value_stays_under_the_nvs_ceiling() {
        // The plan's chunk size is the reason a workspace cannot be one 16 KB value, so
        // it is checked rather than trusted.
        let mut store = LimitedStore::default();
        let blob: String = "x".repeat(PLAN.capacity());
        write_blob(&mut store, "ws", &blob, PLAN).expect("exactly the capacity fits");
        for (key, value) in &store.map {
            assert!(value.len() <= VALUE_MAX, "`{key}` holds {} bytes", value.len());
            assert!(value.len() <= PLAN.chunk_bytes, "`{key}` holds {} bytes", value.len());
        }
        // The fixture is only evidence if it refuses what the board refuses.
        assert!(
            !store.set("probe", &"x".repeat(VALUE_MAX + 1)),
            "the fixture must refuse an oversized value"
        );
    }

    #[test]
    fn a_blob_over_the_capacity_is_refused_and_writes_nothing() {
        let mut store = LimitedStore::default();
        // Something valid first: a refused write must leave it untouched.
        let small = "{\"files\":{\"/a.txt\":\"hello\"}}".to_string();
        write_blob(&mut store, "ws", &small, PLAN).expect("the first blob fits");
        let before: Vec<(String, String)> = store.map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        let too_big = "x".repeat(PLAN.capacity() + 1);
        let err = write_blob(&mut store, "ws", &too_big, PLAN).expect_err("one byte over the ceiling");
        // The message names what the caller has to change: the ceiling, the chunk size and
        // the size that broke it.
        assert!(err.contains("17501 bytes"), "{err}");
        assert!(err.contains("holds at most 3500 bytes"), "{err}");
        assert!(err.contains("17500 bytes in total"), "{err}");
        assert!(err.contains("nothing was written"), "{err}");
        let after: Vec<(String, String)> = store.map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(before, after, "a refused write must not touch any key");
        assert_eq!(read_blob(&store, "ws", PLAN), BlobState::Stored(small));
    }

    #[test]
    fn one_bad_chunk_discards_the_whole_blob() {
        let blob: String = (0..7000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let mut store = LimitedStore::default();
        write_blob(&mut store, "ws", &blob, PLAN).expect("fits");

        // A chunk from another write: same length, one byte different.
        let mut edited = store.get("ws.2").unwrap().into_bytes();
        edited[10] = if edited[10] == b'z' { b'y' } else { b'z' };
        store.map.insert("ws.2".into(), String::from_utf8(edited).unwrap());
        match read_blob(&store, "ws", PLAN) {
            BlobState::Corrupt(why) => {
                assert!(why.contains("ws.2"), "the reason must name the value: {why}");
                assert!(why.contains("does not match the checksum"), "{why}");
            }
            other => panic!("a corrupt chunk must be reported, got {other:?}"),
        }

        // A missing chunk is the same answer: never the other two joined up.
        store.map.remove("ws.2");
        match read_blob(&store, "ws", PLAN) {
            BlobState::Corrupt(why) => assert!(why.contains("is missing"), "{why}"),
            other => panic!("a missing chunk must be reported, got {other:?}"),
        }

        // A header that does not describe what is there is refused before any chunk is read.
        store.map.insert("ws".into(), "ws1 3 7000 zz".into());
        assert!(matches!(read_blob(&store, "ws", PLAN), BlobState::Corrupt(_)));

        // And a header whose count is outside the plan.
        store.map.insert("ws".into(), format!("ws1 99 7000 {}", "0".repeat(16)));
        match read_blob(&store, "ws", PLAN) {
            BlobState::Corrupt(why) => assert!(why.contains("at most 5"), "{why}"),
            other => panic!("got {other:?}"),
        }
        // A header that claims bytes it does not carry is refused as a whole.
        store.map.insert("ws".into(), "ws1 0 4000 ".into());
        match read_blob(&store, "ws", PLAN) {
            BlobState::Corrupt(why) => assert!(why.contains("no values at all"), "{why}"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_single_value_in_the_old_format_is_reported_as_legacy() {
        let mut store = LimitedStore::default();
        let old = "{\"version\":1,\"files\":{\"/a.txt\":\"hello\"}}";
        assert!(store.set("ws", old));
        assert_eq!(read_blob(&store, "ws", PLAN), BlobState::Legacy(old.to_string()));
        // Nothing at all is its own answer: "no blob" is not "an empty blob".
        assert_eq!(read_blob(&LimitedStore::default(), "ws", PLAN), BlobState::Missing);
    }

    #[test]
    fn a_shorter_blob_takes_the_tail_of_the_longer_one_with_it() {
        let mut store = LimitedStore::default();
        let long: String = "x".repeat(3 * CHUNK_BYTES);
        write_blob(&mut store, "ws", &long, PLAN).expect("fits");
        assert!(store.get("ws.3").is_some());
        write_blob(&mut store, "ws", "short", PLAN).expect("fits");
        assert!(store.get("ws.2").is_none(), "the tail of the old blob must be removed");
        assert!(store.get("ws.3").is_none());
        assert_eq!(read_blob(&store, "ws", PLAN), BlobState::Stored("short".to_string()));
    }

    #[test]
    fn a_chunk_boundary_never_splits_a_character() {
        // Multi-byte characters mean "3500 bytes" is a target, not a promise: a cut inside
        // one would make the value impossible to store as a `String`.
        let blob = "é".repeat(4000); // 2 bytes each
        let chunks = split(&blob, CHUNK_BYTES);
        assert!(chunks.iter().all(|chunk| chunk.len() <= CHUNK_BYTES));
        assert_eq!(chunks.concat(), blob);
        let mut store = LimitedStore::default();
        write_blob(&mut store, "ws", &blob, PLAN).expect("fits");
        assert_eq!(read_blob(&store, "ws", PLAN), BlobState::Stored(blob));
        // An empty blob is one header with no chunks, and reads back as empty.
        write_blob(&mut store, "ws", "", PLAN).expect("fits");
        assert_eq!(read_blob(&store, "ws", PLAN), BlobState::Stored(String::new()));
    }

    #[test]
    fn clearing_removes_the_header_and_every_chunk() {
        let mut store = LimitedStore::default();
        write_blob(&mut store, "ws", &"x".repeat(2 * CHUNK_BYTES), PLAN).expect("fits");
        assert!(clear_blob(&mut store, "ws", PLAN));
        assert!(store.map.is_empty(), "left behind: {:?}", store.map.keys().collect::<Vec<_>>());
        assert!(!clear_blob(&mut store, "ws", PLAN), "nothing left to remove");
    }
}

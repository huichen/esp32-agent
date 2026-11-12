//! Persistence abstraction: NVS on the ESP32, a JSON file on the host.
//!
//! On top of the [`KvStore`] trait sits the one thing every large blob on this board
//! needs: [`write_blob`] / [`read_blob`], which spread a value too long for NVS over
//! several smaller ones and check it back chunk by chunk. See [`VALUE_MAX`] for why a
//! single value cannot simply be bigger.
//!
//! And on top of that sits [`StoreQueue`]: writes are *asked for* by whoever changed
//! something and *performed* by one task the platform owns. That split is not tidiness.
//! An NVS write is an `spi_flash` operation, which runs with both caches disabled, so the
//! task doing it must have its stack in internal RAM. On this board some threads get a
//! PSRAM stack, and a write from one of those asserts in
//! `spi_flash_disable_interrupts_caches_and_other_cpu (esp_task_stack_is_sane_cache_disabled())`
//! and reboots the board — see the `nvs-writer` task in the firmware.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

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

/// The longest key name NVS accepts: its `NVS_KEY_NAME_MAX_SIZE` is 16 *including* the
/// terminator, so a name is at most 15 bytes (`nvs.h`).
///
/// This is not a guideline. A longer name is refused with `ESP_ERR_NVS_KEY_TOO_LONG`
/// (0x1109) and *nothing is stored*, which for a while was the state of this board's
/// transcript: `agent.transcript` is 16 bytes, so every transcript write and every
/// `agent.run_in_flight` (19 bytes) was refused while `/api/status.store.failed` climbed —
/// measured 2026-11-12, 23 refusals within 195 s of boot, `nvs write
/// \`agent.transcript\`: ESP_ERR_NVS_KEY_TOO_LONG` on the console log. The queue checks the
/// length before it reaches a backend, so a name that cannot work is reported like any
/// other refusal instead of being discovered on a board.
pub const KEY_MAX: usize = 15;

/// `ESP_ERR_NVS_KEY_TOO_LONG` (`nvs.h`: `ESP_ERR_NVS_BASE + 0x09`): the code the board
/// returns for a name over [`KEY_MAX`]. Named here so the host and the board classify that
/// refusal with the same number instead of two vocabularies.
pub const ERR_KEY_TOO_LONG: i32 = 0x1109;

/// The name that number has on the board, for `/api/status` and the log line.
pub const ERR_KEY_TOO_LONG_NAME: &str = "ESP_ERR_NVS_KEY_TOO_LONG";

/// How many distinct keys [`StoreQueue::refused_keys`] keeps. A store that refuses key
/// after key must not grow a map without bound on a board with 141 KB of internal RAM; the
/// count of the rest is kept in [`StoreQueue::refused_other`].
pub const REFUSED_KEYS_MAX: usize = 16;

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
///
/// The shape comes from [`crate::keys::chunk`], the one place a key is assembled: the
/// 15-byte limit is checked there by `keys.rs`'s test for every chunk number a blob could
/// ask for, not only for the ones a plan asks for today.
fn chunk_key(base: &str, index: usize) -> String {
    crate::keys::chunk(base, index)
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
///
/// The `App` never holds a store directly: it holds a [`QueuedStore`], so `set` and
/// `remove` mean "this must be stored" and are performed by the platform's writer task
/// (see [`StoreQueue`]). The trait itself is unchanged: an implementation of it writes
/// where and when it is called, and the platform is responsible for calling it from a
/// task that may touch flash.
pub trait KvStore: Send {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&mut self, key: &str, value: &str) -> bool;
    fn remove(&mut self, key: &str) -> bool;
    fn keys(&self) -> Vec<String>;

    /// Why the `set`/`remove` that just returned `false` was refused, if this backend can
    /// say. Read immediately after the call, by the one task that drains the queue.
    ///
    /// `false` on its own is not a diagnosis: `ESP_ERR_NVS_NOT_ENOUGH_SPACE` and
    /// `ESP_ERR_NVS_KEY_TOO_LONG` are the same `false` to a caller and need opposite fixes.
    fn last_error(&self) -> Option<StoreError> {
        None
    }

    /// How much room the backing store has, if it can be asked. Reported in
    /// `/api/status.store.space`, so "it refused a write" and "it is full" stay two
    /// different observations.
    fn space(&self) -> Option<StoreSpace> {
        None
    }
}

/// One reason a store gave for refusing an operation: the platform's numeric code and the
/// name that code has there (`esp_err_t` and `esp_err_to_name` on the board).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError {
    pub code: i32,
    pub name: String,
}

/// What one refused operation was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    Set,
    Remove,
}

impl StoreOp {
    pub fn as_str(self) -> &'static str {
        match self {
            StoreOp::Set => "set",
            StoreOp::Remove => "remove",
        }
    }
}

/// A refusal with everything needed to act on it: which key, which operation, why, and
/// when — the facts `/api/status.store.last_failure` and the `store` log line carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub key: String,
    pub op: StoreOp,
    /// The platform's code, or 0 when the backend could not say (a `false` with no
    /// reason is still a refusal and is reported as one).
    pub code: i32,
    pub name: String,
    /// Milliseconds since the queue was created: a few ms after boot on the board, which
    /// is close enough to put next to a log line's `t_ms` (both are counts from that boot).
    pub at_ms: u64,
}

/// Room left in the backing store, in the units the backend thinks in — NVS entries, which
/// are 32 bytes each and 126 per 4 KB page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreSpace {
    pub used_entries: usize,
    pub free_entries: usize,
    pub total_entries: usize,
}

/// One queued operation: `Some` is a write, `None` is a removal.
type Pending = Vec<(String, Option<String>)>;

/// What one [`StoreQueue::flush`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlushReport {
    /// Operations the store accepted.
    pub written: usize,
    /// Operations it refused, which are *not* retried: a value NVS will not take now it
    /// will not take on the next tick either, and a queue that retried forever would fill
    /// the log and hide the one line that says what is wrong.
    pub failed: usize,
    /// The keys involved, in the order they were applied.
    pub keys: Vec<String>,
    /// One entry per refusal, in the order they happened: the key, the operation and the
    /// backend's own reason. `failed` is the count of these; this is what makes the count
    /// actionable.
    pub failures: Vec<Refusal>,
}

impl FlushReport {
    pub fn is_empty(&self) -> bool {
        self.written == 0 && self.failed == 0
    }
}

/// The writes an [`App`](crate::app::App) has asked for and nobody has performed yet.
///
/// Everything that changes something durable — the config, the manifest, the schedule,
/// the file workspace, the transcript — goes through this and returns immediately. The
/// platform hands the same queue to a task whose stack is known to be in internal RAM,
/// which is the only thing that ever calls the backing [`KvStore`].
///
/// Two properties the rest of the code depends on:
///
/// * **Read-your-writes.** [`get`](Self::read) answers from the queue first, so a caller
///   that queued a write and asks for the value back sees what it just wrote. The
///   transcript's chunk bookkeeping and [`write_blob`]'s stale-tail sweep both read back
///   what they wrote, and would otherwise re-write or fail to clear.
/// * **Order.** Operations are applied in the order they were queued, because
///   [`write_blob`] writes its header last on purpose: a power cut between the chunks and
///   the header must leave a header that does not match its chunks.
#[derive(Clone)]
pub struct StoreQueue {
    inner: Arc<Mutex<Box<dyn KvStore>>>,
    pending: Arc<Mutex<Pending>>,
    written: Arc<AtomicU32>,
    failed: Arc<AtomicU32>,
    stats: Arc<Mutex<StoreStats>>,
}

/// What the queue remembers so `/api/status` can answer "which key, and why" without a
/// serial terminal. Kept apart from the counters: `failed` is a number, this is the
/// diagnosis.
#[derive(Debug)]
struct StoreStats {
    origin: std::time::Instant,
    last_failure: Option<Refusal>,
    refused_keys: BTreeMap<String, u32>,
    /// Refusals whose key did not fit [`REFUSED_KEYS_MAX`]; counted, not enumerated.
    refused_other: u32,
    space: Option<StoreSpace>,
}

impl StoreStats {
    fn new(space: Option<StoreSpace>) -> Self {
        Self {
            origin: std::time::Instant::now(),
            last_failure: None,
            refused_keys: BTreeMap::new(),
            refused_other: 0,
            space,
        }
    }

    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn note(&mut self, refusal: Refusal) {
        if let Some(count) = self.refused_keys.get_mut(&refusal.key) {
            *count += 1;
        } else if self.refused_keys.len() < REFUSED_KEYS_MAX {
            self.refused_keys.insert(refusal.key.clone(), 1);
        } else {
            self.refused_other = self.refused_other.saturating_add(1);
        }
        self.last_failure = Some(refusal);
    }
}

impl std::fmt::Debug for StoreQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreQueue")
            .field("pending", &self.pending())
            .field("written", &self.written())
            .field("failed", &self.failed())
            .finish()
    }
}

impl StoreQueue {
    pub fn new(inner: Box<dyn KvStore>) -> Self {
        // Asked once, before the store is behind a lock: a backend that can report its
        // size knows it before it has written anything, and this is the reading that
        // separates "refused" from "full".
        let space = inner.space();
        Self {
            inner: Arc::new(Mutex::new(inner)),
            pending: Arc::new(Mutex::new(Vec::new())),
            written: Arc::new(AtomicU32::new(0)),
            failed: Arc::new(AtomicU32::new(0)),
            stats: Arc::new(Mutex::new(StoreStats::new(space))),
        }
    }

    /// Note a write (`Some`) or a removal (`None`). A key queued twice keeps the position
    /// of the first time and the value of the last, so a task that rewrites the same blob
    /// every tick costs one operation, not one per tick.
    fn enqueue(&self, key: &str, value: Option<String>) -> bool {
        let mut pending = match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match pending.iter_mut().find(|(queued, _)| queued == key) {
            Some(entry) => entry.1 = value,
            None => pending.push((key.to_string(), value)),
        }
        true
    }

    /// The value `key` will have once the queue is drained: the queued one if there is
    /// one, and `None` if a removal is queued.
    fn read(&self, key: &str) -> Option<String> {
        let pending = match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some((_, queued)) = pending.iter().rev().find(|(queued, _)| queued == key) {
            return queued.clone();
        }
        drop(pending);
        match self.inner.lock() {
            Ok(inner) => inner.get(key),
            Err(poisoned) => poisoned.into_inner().get(key),
        }
    }

    /// Perform every queued operation, in order.
    ///
    /// **The caller must be on a task whose stack is in internal RAM.** See the module
    /// docs. The queue's lock is held for the whole drain so a concurrent
    /// [`read`](Self::read) can neither see a value in neither place nor be answered out
    /// of order; enqueues block for the length of a flash write, which is what keeps the
    /// next write behind the one before it.
    pub fn flush(&self) -> FlushReport {
        let mut pending = match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut report = FlushReport::default();
        if pending.is_empty() {
            return report;
        }
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut stats = match self.stats.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for (key, value) in pending.drain(..) {
            let op = match &value {
                Some(_) => StoreOp::Set,
                None => StoreOp::Remove,
            };
            let refusal = if key.len() > KEY_MAX {
                // A name the board cannot hold. Reported here, with the code the board
                // would return, so it costs a log line instead of a flash round trip that
                // ends in `false` with no reason.
                Some(Refusal {
                    key: key.clone(),
                    op,
                    code: ERR_KEY_TOO_LONG,
                    name: ERR_KEY_TOO_LONG_NAME.to_string(),
                    at_ms: stats.now_ms(),
                })
            } else {
                let ok = match &value {
                    Some(text) => inner.set(&key, text),
                    // "It was not there" is not a failure: a removal that removes nothing is
                    // what a second boot asks for. A removal the *backend* failed is, and
                    // that is the difference `last_error` makes.
                    None => {
                        let removed = inner.remove(&key);
                        removed || inner.last_error().is_none()
                    }
                };
                if ok {
                    None
                } else {
                    let why = inner.last_error();
                    Some(Refusal {
                        key: key.clone(),
                        op,
                        code: why.as_ref().map(|e| e.code).unwrap_or(0),
                        name: why.map(|e| e.name).unwrap_or_else(|| "refused".to_string()),
                        at_ms: stats.now_ms(),
                    })
                }
            };
            match refusal {
                Some(refusal) => {
                    report.failed += 1;
                    self.failed.fetch_add(1, Ordering::Relaxed);
                    report.failures.push(refusal.clone());
                    stats.note(refusal);
                }
                None => {
                    report.written += 1;
                    self.written.fetch_add(1, Ordering::Relaxed);
                }
            }
            report.keys.push(key);
        }
        // Re-read once per drain, not per operation: `nvs_get_stats` walks the pages, and
        // the number is only interesting for what the drain just did to it.
        stats.space = inner.space();
        report
    }

    /// How many writes are queued and waiting for the writer.
    pub fn pending(&self) -> usize {
        match self.pending.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Operations the store has accepted since boot.
    pub fn written(&self) -> u32 {
        self.written.load(Ordering::Relaxed)
    }

    /// Operations it has refused since boot. A number above zero is a real problem: the
    /// change the caller was told about is not on flash.
    pub fn failed(&self) -> u32 {
        self.failed.load(Ordering::Relaxed)
    }

    /// The most recent refusal: which key, which operation, the backend's code and name,
    /// and how long after the queue was created it happened. `None` until something is
    /// refused. This is what `/api/status.store.last_failure` reports.
    pub fn last_failure(&self) -> Option<Refusal> {
        match self.stats.lock() {
            Ok(stats) => stats.last_failure.clone(),
            Err(poisoned) => poisoned.into_inner().last_failure.clone(),
        }
    }

    /// Refusals per key since boot, most refused first — so a board that cannot be
    /// attached to a serial terminal still says *which* key is the problem.
    pub fn refused_keys(&self) -> Vec<(String, u32)> {
        let refused = match self.stats.lock() {
            Ok(stats) => stats.refused_keys.clone(),
            Err(poisoned) => poisoned.into_inner().refused_keys.clone(),
        };
        let mut out: Vec<(String, u32)> = refused.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Refusals for keys beyond [`REFUSED_KEYS_MAX`], counted rather than enumerated.
    pub fn refused_other(&self) -> u32 {
        match self.stats.lock() {
            Ok(stats) => stats.refused_other,
            Err(poisoned) => poisoned.into_inner().refused_other,
        }
    }

    /// Room left in the backing store, when the backend can report it.
    pub fn space(&self) -> Option<StoreSpace> {
        match self.stats.lock() {
            Ok(stats) => stats.space,
            Err(poisoned) => poisoned.into_inner().space,
        }
    }

    /// Everything currently stored, queued changes included.
    pub fn keys(&self) -> Vec<String> {
        let mut out = match self.inner.lock() {
            Ok(inner) => inner.keys(),
            Err(poisoned) => poisoned.into_inner().keys(),
        };
        let pending = match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for (key, value) in pending.iter() {
            match value {
                Some(_) if !out.contains(key) => out.push(key.clone()),
                Some(_) => {}
                None => out.retain(|stored| stored != key),
            }
        }
        out
    }
}

/// A [`KvStore`] that queues instead of writing.
///
/// This is what the `App` holds, so every existing `store.set(...)` call site means "this
/// change must be stored" and none of them can reach flash from whatever thread happens
/// to be running.
pub struct QueuedStore {
    queue: StoreQueue,
}

impl QueuedStore {
    pub fn new(queue: StoreQueue) -> Self {
        Self { queue }
    }
}

impl KvStore for QueuedStore {
    fn get(&self, key: &str) -> Option<String> {
        self.queue.read(key)
    }

    /// Always true: the write is accepted into the queue. Whether NVS takes it is known
    /// only after the writer runs, and is reported by [`StoreQueue::failed`] and the log
    /// line the writer emits — not by this return value.
    fn set(&mut self, key: &str, value: &str) -> bool {
        self.queue.enqueue(key, Some(value.to_string()))
    }

    /// Whether the key exists *now*: a queued value counts, a queued removal does not.
    fn remove(&mut self, key: &str) -> bool {
        let had = self.queue.read(key).is_some();
        self.queue.enqueue(key, None);
        had
    }

    fn keys(&self) -> Vec<String> {
        self.queue.keys()
    }
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

    // ------------------------------------------------------- the queued store

    #[test]
    fn a_queued_write_is_visible_before_it_is_written_and_on_flash_after() {
        let inner = Shared::default();
        let queue = StoreQueue::new(Box::new(inner.clone()));
        let mut store = QueuedStore::new(queue.clone());

        assert!(store.set("ws", "hello"));
        // Read-your-writes: the blob bookkeeping reads back what it just wrote.
        assert_eq!(store.get("ws").as_deref(), Some("hello"));
        assert_eq!(queue.pending(), 1);
        assert!(inner.all().is_empty(), "nothing may reach flash before the writer runs");

        let report = queue.flush();
        assert_eq!(
            report,
            FlushReport { written: 1, failed: 0, keys: vec!["ws".into()], failures: Vec::new() }
        );
        assert_eq!(inner.get("ws").as_deref(), Some("hello"));
        assert_eq!(queue.pending(), 0);
        assert_eq!(queue.written(), 1);
        // A second flush has nothing to do and does not count anything.
        assert!(queue.flush().is_empty());
        assert_eq!(queue.written(), 1);
    }

    #[test]
    fn a_queued_removal_reads_as_absent_and_then_clears_the_value() {
        let inner = Shared::default();
        let mut seed = inner.clone();
        seed.set("k", "v");
        let queue = StoreQueue::new(Box::new(inner.clone()));
        let mut store = QueuedStore::new(queue.clone());

        assert!(store.remove("k"), "the key was there");
        assert_eq!(store.get("k"), None, "a queued removal must not read back the old value");
        assert!(inner.get("k").is_some(), "and must not have removed it yet");

        queue.flush();
        assert_eq!(inner.get("k"), None);
        assert!(!store.remove("k"), "removing what is not there reports nothing");
    }

    #[test]
    fn the_order_of_a_blob_write_survives_the_queue() {
        // `write_blob` writes its header last so a torn write is detectable; a queue that
        // reordered by key (`ws` before `ws.1`) would quietly undo that.
        let inner = Shared::default();
        let queue = StoreQueue::new(Box::new(inner.clone()));
        let mut store = QueuedStore::new(queue.clone());
        write_blob(&mut store, "ws", &"x".repeat(2 * CHUNK_BYTES), PLAN).expect("fits");
        let report = queue.flush();
        assert_eq!(report.keys, vec!["ws.1".to_string(), "ws.2".to_string(), "ws".to_string()]);
        assert_eq!(read_blob(&inner, "ws", PLAN), BlobState::Stored("x".repeat(2 * CHUNK_BYTES)));
    }

    #[test]
    fn a_key_queued_twice_costs_one_write_and_keeps_the_last_value() {
        let inner = Shared::default();
        let queue = StoreQueue::new(Box::new(inner.clone()));
        let mut store = QueuedStore::new(queue.clone());
        store.set("sched", "first");
        store.set("other", "x");
        store.set("sched", "second");
        assert_eq!(queue.pending(), 2, "one entry per key");
        let report = queue.flush();
        assert_eq!(report.written, 2);
        assert_eq!(inner.get("sched").as_deref(), Some("second"));
        assert_eq!(report.keys, vec!["sched".to_string(), "other".to_string()], "first position kept");
    }

    #[test]
    fn a_store_that_refuses_a_value_is_counted_not_hidden() {
        // The board's NVS refuses a value above 4000 bytes; the queue must say so rather
        // than report a write that did not happen.
        let queue = StoreQueue::new(Box::new(LimitedStore::default()));
        let mut store = QueuedStore::new(queue.clone());
        store.set("ok", "small");
        store.set("too_long", &"x".repeat(VALUE_MAX + 1));
        let report = queue.flush();
        assert_eq!(report.written, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.keys, vec!["ok".to_string(), "too_long".to_string()]);
        assert_eq!(queue.failed(), 1);
        assert_eq!(queue.pending(), 0, "a refused write is dropped, not retried forever");
        // The count is not the diagnosis: the refusal names the key and the operation even
        // when the backend cannot say why (`LimitedStore` returns a bare `false`).
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].key, "too_long");
        assert_eq!(report.failures[0].op, StoreOp::Set);
        assert_eq!(report.failures[0].name, "refused");
        assert_eq!(queue.last_failure().map(|f| f.key), Some("too_long".to_string()));
        assert_eq!(queue.refused_keys(), vec![("too_long".to_string(), 1)]);
    }

    /// A store that refuses one key for a reason it can state, the way NVS refuses a value
    /// it has no room for. `LimitedStore` covers the bare `false`; this covers the case the
    /// board actually hits, where the answer is a code.
    #[derive(Debug, Default)]
    struct RefusingStore {
        map: BTreeMap<String, String>,
        refuse: Option<(String, i32, &'static str)>,
        fail_remove: bool,
        last: Option<StoreError>,
    }

    impl KvStore for RefusingStore {
        fn get(&self, key: &str) -> Option<String> {
            self.map.get(key).cloned()
        }

        fn set(&mut self, key: &str, value: &str) -> bool {
            self.last = None;
            if let Some((refused, code, name)) = &self.refuse {
                if refused == key {
                    self.last = Some(StoreError { code: *code, name: (*name).to_string() });
                    return false;
                }
            }
            self.map.insert(key.to_string(), value.to_string());
            true
        }

        fn remove(&mut self, key: &str) -> bool {
            self.last = None;
            if self.fail_remove {
                self.last =
                    Some(StoreError { code: 0x1105, name: "ESP_ERR_NVS_NOT_ENOUGH_SPACE".to_string() });
                return false;
            }
            self.map.remove(key).is_some()
        }

        fn keys(&self) -> Vec<String> {
            self.map.keys().cloned().collect()
        }

        fn last_error(&self) -> Option<StoreError> {
            self.last.clone()
        }
    }

    #[test]
    fn a_refused_write_is_recorded_with_its_key_and_the_stores_code() {
        let queue = StoreQueue::new(Box::new(RefusingStore {
            refuse: Some(("transcript.1".to_string(), 0x1105, "ESP_ERR_NVS_NOT_ENOUGH_SPACE")),
            ..Default::default()
        }));
        let mut store = QueuedStore::new(queue.clone());
        store.set("config", "{}");
        store.set("transcript.1", "a chunk");
        let report = queue.flush();

        assert_eq!(report.written, 1);
        assert_eq!(report.failed, 1);
        let refusal = &report.failures[0];
        assert_eq!(refusal.key, "transcript.1", "the key is the part that makes it actionable");
        assert_eq!(refusal.op, StoreOp::Set);
        assert_eq!(refusal.code, 0x1105);
        assert_eq!(refusal.name, "ESP_ERR_NVS_NOT_ENOUGH_SPACE");

        // And the same facts survive the flush, which is what `/api/status` reports.
        let last = queue.last_failure().expect("a refusal must be remembered");
        assert_eq!(last.key, "transcript.1");
        assert_eq!(last.code, 0x1105);
        assert_eq!(queue.failed(), 1);
        assert_eq!(queue.refused_keys(), vec![("transcript.1".to_string(), 1)]);
    }

    #[test]
    fn a_key_longer_than_nvs_allows_is_refused_with_the_boards_own_code() {
        // The board keeps 15 bytes of key. A 16-byte one is not stored and cannot be read
        // back, and the backend is not even asked: the refusal comes with the same code the
        // board would have returned.
        let queue = StoreQueue::new(Box::new(Shared::default()));
        let mut store = QueuedStore::new(queue.clone());
        store.set("agent.transcript", "the transcript"); // 16 bytes
        let report = queue.flush();

        assert_eq!(report.written, 0);
        assert_eq!(report.failed, 1);
        assert_eq!(report.failures[0].key, "agent.transcript");
        assert_eq!(report.failures[0].code, ERR_KEY_TOO_LONG);
        assert_eq!(report.failures[0].name, ERR_KEY_TOO_LONG_NAME);
        assert_eq!(ERR_KEY_TOO_LONG, 0x1109, "ESP_ERR_NVS_KEY_TOO_LONG in nvs.h");
        // Nothing reached the store, so a later read cannot find a value that is not there.
        assert_eq!(store.get("agent.transcript"), None);
        assert_eq!(store.keys(), Vec::<String>::new());

        // Exactly KEY_MAX bytes is accepted: the limit is 15, not 14.
        let longest = "x".repeat(KEY_MAX);
        store.set(&longest, "v");
        let report = queue.flush();
        assert_eq!(report.failed, 0, "{:?}", report.failures);
        assert_eq!(store.get(&longest).as_deref(), Some("v"));
    }

    #[test]
    fn a_refused_removal_is_reported_but_removing_a_missing_key_is_not() {
        let queue = StoreQueue::new(Box::new(RefusingStore { fail_remove: true, ..Default::default() }));
        let mut store = QueuedStore::new(queue.clone());
        store.remove("config");
        let report = queue.flush();
        assert_eq!(report.failed, 1, "a removal the store failed is a refusal");
        assert_eq!(report.failures[0].op, StoreOp::Remove);
        assert_eq!(report.failures[0].code, 0x1105);

        // A store that answers "it was not there" without an error is not refusing: that
        // is what a second boot's removals look like, and they must not be reported.
        let queue = StoreQueue::new(Box::new(MemoryStore::new()));
        let mut store = QueuedStore::new(queue.clone());
        store.remove("never_written");
        let report = queue.flush();
        assert_eq!(report.failed, 0);
        assert!(queue.last_failure().is_none());
    }

    #[test]
    fn the_refused_key_list_is_bounded_and_counts_the_rest() {
        let queue = StoreQueue::new(Box::new(LimitedStore::default()));
        let mut store = QueuedStore::new(queue.clone());
        let too_long = "x".repeat(VALUE_MAX + 1);
        // More distinct keys than the list keeps.
        // More distinct keys than the list keeps.
        for index in 0..(REFUSED_KEYS_MAX + 3) {
            store.set(&format!("k{index}"), &too_long);
        }
        let report = queue.flush();
        assert_eq!(report.failed, REFUSED_KEYS_MAX + 3);
        // … and one of them again, in a later drain: counted, not added twice.
        store.set("k0", &too_long);
        assert_eq!(queue.flush().failed, 1);
        let refused = queue.refused_keys();
        assert_eq!(refused.len(), REFUSED_KEYS_MAX, "the list must not grow without bound");
        assert_eq!(refused[0], ("k0".to_string(), 2), "most refused first, counted per key");
        assert_eq!(queue.refused_other(), 3);
    }

    #[test]
    fn a_flush_is_whole_or_it_has_not_happened() {
        // A read taken while a flush is under way must see the value in one place or the
        // other, never in neither. Single-threaded here: the queue keeps the value until
        // the inner store has taken it, which is what makes the lock ordering matter.
        let inner = Shared::default();
        let queue = StoreQueue::new(Box::new(inner.clone()));
        let mut store = QueuedStore::new(queue.clone());
        store.set("a", "1");
        assert_eq!(store.get("a").as_deref(), Some("1"));
        queue.flush();
        assert_eq!(store.get("a").as_deref(), Some("1"));
        assert_eq!(inner.get("a").as_deref(), Some("1"));
    }

    /// The test store: a map behind a mutex, cloneable so a test can look at what the
    /// writer produced while the `App` holds the queue.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<BTreeMap<String, String>>>);

    impl Shared {
        fn all(&self) -> Vec<String> {
            self.0.lock().map(|m| m.keys().cloned().collect()).unwrap_or_default()
        }
    }

    impl KvStore for Shared {
        fn get(&self, key: &str) -> Option<String> {
            self.0.lock().ok()?.get(key).cloned()
        }

        fn set(&mut self, key: &str, value: &str) -> bool {
            match self.0.lock() {
                Ok(mut m) => {
                    m.insert(key.to_string(), value.to_string());
                    true
                }
                Err(_) => false,
            }
        }

        fn remove(&mut self, key: &str) -> bool {
            match self.0.lock() {
                Ok(mut m) => m.remove(key).is_some(),
                Err(_) => false,
            }
        }

        fn keys(&self) -> Vec<String> {
            self.all()
        }
    }
}

//! [`KvStore`] over a SPIFFS file per key, and the routing that decides which keys go there.
//!
//! NVS keeps a value to about 4 KB and divides its partition into 32-byte entries, which is
//! fine for a configuration blob and wrong for the two things that grow: the transcript and
//! the file workspace. Both are files here, on the 12 MB `storage` partition, and NVS is left
//! to the configuration and the secrets.
//!
//! The split is not cosmetic. A write to either backend is an `spi_flash` operation with both
//! caches off, so it has to happen on the one task whose stack is in internal RAM — the
//! `store-writer` task in `main.rs`. That is why this type is a `KvStore` like any other and not
//! a second code path: the queue in front of it does not care which filesystem answers.

use esp32_agent_core::store::{KvStore, StoreError, StoreSpace};
use esp_idf_svc::sys;
use std::path::{Path, PathBuf};

/// Where the partition is mounted.
const MOUNT: &str = "/storage";
/// One file per key, directly under the mount point.
///
/// SPIFFS in ESP-IDF has no directories: `esp_spiffs.c`'s `vfs_spiffs_mkdir` returns `ENOTSUP`
/// (measured on this board: `Error: Not supported (os error 134)` after a 38 s format, which is
/// the format doing its job and the `mkdir` right after it failing). A flat namespace is also
/// what SPIFFS is: the key names are already unique and short.
const KV_DIR: &str = "/storage";

pub struct SpiffsStore {
    dir: PathBuf,
    last_error: Option<StoreError>,
    /// The file a sequential read is pulling from, kept open: reopening per chunk cost more than
    /// the read did.
    open: std::cell::RefCell<Option<(String, std::fs::File)>>,
}

fn error_of(code: i32, context: &str) -> StoreError {
    let name = unsafe {
        let ptr = sys::esp_err_to_name(code);
        if ptr.is_null() {
            format!("0x{code:x}")
        } else {
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    log::error!("spiffs {context}: {name}");
    StoreError { code, name }
}

/// Mount `storage`.
///
/// `format_if_mount_failed` is on: the partition is empty on a board that has just been given
/// this table, and a board that refuses to boot because a filesystem was not formatted yet is
/// worse than one that formats it once. That first format is slow - 38 s for 11.9 MB, measured
/// on this board - so a freshly flashed board takes that long before its first packet.
pub fn mount() -> anyhow::Result<SpiffsStore> {
    let label = c"storage";
    let path = c"/storage";
    let conf = sys::esp_vfs_spiffs_conf_t {
        base_path: path.as_ptr(),
        partition_label: label.as_ptr(),
        max_files: 6,
        format_if_mount_failed: true,
    };
    let rc = unsafe { sys::esp_vfs_spiffs_register(&conf) };
    if rc != sys::ESP_OK {
        // `ESP_ERR_INVALID_STATE` means it is already mounted (a restart of the app without a
        // reboot), which is not an error worth failing the boot over.
        if rc != sys::ESP_ERR_INVALID_STATE {
            return Err(anyhow::anyhow!("esp_vfs_spiffs_register: {}", error_of(rc, "mount").name));
        }
    }
    let (total, used) = capacity();
    // A power cut in the middle of a replace leaves `<key>.tmp` (the new value, not yet
    // published) or `<key>.old` (the old one, moved aside). Neither is a key. A `.old` with no
    // value beside it is the case where the publish never happened: put it back, so the last
    // good value survives the crash that interrupted it.
    let mut restored = 0;
    let mut dropped = 0;
    if let Ok(entries) = std::fs::read_dir(MOUNT) {
        let names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        for name in &names {
            if name.ends_with(".tmp") {
                let _ = std::fs::remove_file(format!("{MOUNT}/{name}"));
                dropped += 1;
            } else if let Some(key) = name.strip_suffix(".old") {
                let value = format!("{MOUNT}/{key}");
                if std::fs::rename(format!("{MOUNT}/{name}"), &value).is_ok() {
                    restored += 1;
                }
            }
        }
    }
    if dropped > 0 {
        log::warn!("storage: dropped {dropped} unpublished value(s) from an interrupted save");
    }
    if restored > 0 {
        log::warn!("storage: put back {restored} value(s) an interrupted save had moved aside");
    }
    log::info!("storage: SPIFFS on `storage`, mounted at {MOUNT}, {used} of {total} bytes used");
    Ok(SpiffsStore {
        dir: PathBuf::from(KV_DIR),
        last_error: None,
        open: std::cell::RefCell::new(None),
    })
}

/// The partition's size and what it holds, in bytes. `(0, 0)` if it cannot be asked.
fn capacity() -> (usize, usize) {
    let label = c"storage";
    let mut total: usize = 0;
    let mut used: usize = 0;
    let rc = unsafe { sys::esp_spiffs_info(label.as_ptr(), &mut total, &mut used) };
    if rc != sys::ESP_OK {
        return (0, 0);
    }
    (total, used)
}

impl SpiffsStore {
    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// Where a value is written before it is published.
    ///
    /// The suffix marks it as not-a-key, so a value left behind by a power cut is never read
    /// back as one. Keys are at most 15 bytes (`store::KEY_MAX`) and a SPIFFS name is 32, so
    /// `transcript.tmp` fits with room to spare.
    fn temp_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.tmp"))
    }

    /// Replace the value under `key`, or leave the old one exactly as it was.
    ///
    /// `std::fs::write` opens with `O_TRUNC`: the old value is gone before the first byte of
    /// the new one lands, so a write that fails half way leaves neither. Measured on this
    /// board before this function existed: a 48 KB write refused with `ENOSPC` took the
    /// workspace from 81 575 bytes to 1 004, and the refused write also left the volume in a
    /// state where it kept refusing anything above ~40 KB until it was reformatted.
    ///
    /// So the new value goes to a temporary name, is flushed to flash, and only then renamed
    /// over the original. `fsync` before `rename` matters: a rename that publishes a file
    /// whose contents are still in a cache is the same loss with a smaller window. SPIFFS
    /// implements both (`CONFIG_VFS_SUPPORT_DIR` is on), and a rename is a directory-level
    /// operation that leaves the old file readable until it completes.
    fn replace(&self, key: &str, value: &str) -> std::io::Result<()> {
        let tmp = self.temp_path(key);
        let written = (|| {
            let mut file = std::fs::File::create(&tmp)?;
            std::io::Write::write_all(&mut file, value.as_bytes())?;
            file.sync_all()
        })();
        if let Err(e) = written {
            // Half a file under a temporary name is rubbish, not a value.
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        self.publish(key, &tmp)
    }

    /// Put the file at `tmp` in place of `key`'s value, keeping the old one until it lands.
    ///
    /// SPIFFS refuses to rename onto a name that exists (`SPIFFS_ERR_CONFLICTING_NAME`, which
    /// ESP-IDF maps to EIO - measured here: the temporary file wrote and synced, the rename
    /// failed with `I/O error (os error 5)`), which is why the old value is moved aside first
    /// and put back if the publish fails. The name is empty for the length of one rename, and
    /// `mount` restores a `.old` it finds without its value beside it.
    fn publish(&self, key: &str, tmp: &Path) -> std::io::Result<()> {
        let path = self.path(key);
        let old = self.old_path(key);
        let _ = std::fs::remove_file(&old);
        let displaced = std::fs::rename(&path, &old).is_ok();
        match std::fs::rename(tmp, &path) {
            Ok(()) => {
                if displaced {
                    let _ = std::fs::remove_file(&old);
                }
                Ok(())
            }
            Err(e) => {
                if displaced {
                    let _ = std::fs::rename(&old, &path);
                }
                let _ = std::fs::remove_file(tmp);
                Err(e)
            }
        }
    }

    /// Where the previous value waits while a new one is published.
    fn old_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.old"))
    }

    /// Bytes this partition can still take, which is the honest ceiling for one value.
    fn free_bytes(&self) -> usize {
        let (total, used) = capacity();
        total.saturating_sub(used)
    }
}

impl KvStore for SpiffsStore {
    fn get(&self, key: &str) -> Option<String> {
        match std::fs::read_to_string(self.path(key)) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                log::warn!("spiffs read `{key}`: {e}");
                None
            }
        }
    }

    fn set(&mut self, key: &str, value: &str) -> bool {
        self.last_error = None;
        match self.replace(key, value) {
            Ok(()) => true,
            Err(e) => {
                // `std::io::Error` has no ESP error code; the kind says what a caller can do
                // about it, which is what `/api/status.store.last_failure` is for.
                let code = e.raw_os_error().unwrap_or(0);
                let name = format!("{e}");
                log::error!("spiffs write `{key}`: {name} (the previous value is untouched)");
                self.last_error = Some(StoreError { code, name });
                false
            }
        }
    }

    fn remove(&mut self, key: &str) -> bool {
        self.last_error = None;
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => true,
            // Not there is not a failure: the queue counts a removal it did not have to make
            // as accepted (`NvsStore::remove` says the same).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0);
                let name = format!("{e}");
                log::error!("spiffs remove `{key}`: {name}");
                self.last_error = Some(StoreError { code, name });
                false
            }
        }
    }

    /// Write the value from a callback, streaming into a temporary file and publishing it with
    /// a rename.
    ///
    /// This is the write half of the memory fix: a 4 MB workspace never exists as one `String`
    /// on the way out either. The temporary name is the same trick [`SpiffsStore::replace`]
    /// uses, so a write that fails half way leaves the old value alone.
    fn set_stream(
        &mut self,
        key: &str,
        fill: &mut dyn FnMut(&mut dyn std::io::Write) -> std::io::Result<()>,
    ) -> bool {
        self.last_error = None;
        let tmp = self.temp_path(key);
        let written = (|| -> std::io::Result<()> {
            let file = std::fs::File::create(&tmp)?;
            let mut buffered = std::io::BufWriter::with_capacity(4096, file);
            let mut paced = Paced { inner: &mut buffered, since: 0 };
            fill(&mut paced)?;
            let file = buffered.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            log::error!(
                "spiffs stream `{key}` refused: {e}; esp_spiffs_info total_bytes {} used_bytes {} free_bytes {}",
                capacity().0,
                capacity().1,
                capacity().0.saturating_sub(capacity().1)
            );
            self.last_error = Some(StoreError { code: e.raw_os_error().unwrap_or(0), name: format!("{e}") });
            return false;
        }
        match self.publish(key, &tmp) {
            Ok(()) => true,
            Err(e) => {
                log::error!(
                    "spiffs publish `{key}` refused: {e}; esp_spiffs_info total_bytes {} used_bytes {} free_bytes {}",
                    capacity().0,
                    capacity().1,
                    capacity().0.saturating_sub(capacity().1)
                );
                self.last_error =
                    Some(StoreError { code: e.raw_os_error().unwrap_or(0), name: format!("{e}") });
                false
            }
        }
    }

    /// Read a slice of the stored value without building the whole thing.
    ///
    /// A file seeks, and the handle is kept: `QueueReader` reads sequentially, and re-opening on
    /// every chunk made a 1.75 MB load slow enough to trip the task watchdog. One tick per chunk
    /// lets the scheduler run - the same watchdog, from the other side.
    fn read_at(&self, key: &str, offset: usize, buf: &mut [u8]) -> Option<usize> {
        use std::io::{Read, Seek, SeekFrom};
        let mut slot = self.open.borrow_mut();
        let reuse = matches!(&*slot, Some((k, _)) if k == key);
        if !reuse {
            let file = std::fs::File::open(self.path(key)).ok()?;
            *slot = Some((key.to_string(), file));
        }
        let (_, file) = slot.as_mut()?;
        file.seek(SeekFrom::Start(offset as u64)).ok()?;
        let read = file.read(buf).ok();
        esp_idf_svc::hal::delay::FreeRtos::delay_ms(1);
        read
    }

    fn keys(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            // A value being replaced, or one a power cut left behind: not a key.
            .filter(|name| !name.ends_with(".tmp") && !name.ends_with(".old"))
            .collect()
    }

    fn last_error(&self) -> Option<StoreError> {
        self.last_error.clone()
    }

    /// What `esp_spiffs_info()` reports, in bytes.
    ///
    /// `total` is SPIFFS' own capacity figure, not the partition size: it is
    /// `(blocks - 2) * (pages_per_block - lookup_pages) + 1` data pages times the data page size,
    /// so the 12 517 376-byte partition minus two spare blocks and the object-lookup pages. On
    /// this board that is 45 811 * 251 = 11 498 561. `used` is the pages it has written. No
    /// "free" comes from here - `StoreSpace` computes it, because the backend does not report
    /// one.
    ///
    /// Neither number promises that one allocation of that size fits: a 2.1 MB file was refused
    /// with `ENOSPC` while `free` read 9.38 MB, because SPIFFS needs somewhere to move live pages
    /// while it writes a new file and reports no figure for that.
    fn space(&self) -> Option<StoreSpace> {
        let (total, used) = capacity();
        if total == 0 {
            return None;
        }
        // Bytes and nothing else: a filesystem has no entries, and reporting zeros for them made
        // `free_entries: 0` read as "full" on a partition with 11 MB left.
        Some(StoreSpace::Bytes { used, total })
    }

    /// A file is as long as the partition allows, so a blob goes in as one value. That is the
    /// whole reason the transcript and the workspace moved here: they are the two things that
    /// outgrew a 3.5 KB chunk.
    fn whole_value_bytes(&self) -> Option<usize> {
        let (total, _) = capacity();
        if total == 0 {
            None
        } else {
            Some(self.free_bytes().max(total / 2))
        }
    }
}

/// A writer that lets the rest of the system run every few dozen kilobytes.
///
/// A 4 MB save is one long stretch of flash writes on whichever thread called it, and the task
/// watchdog fires when an idle task has not run for five seconds - measured here, from a 1.75 MB
/// workspace save. One tick per 64 KB costs about 10 ms per megabyte and keeps the board
/// answering.
struct Paced<'a> {
    inner: &'a mut dyn std::io::Write,
    since: usize,
}

impl std::io::Write for Paced<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.since += written;
        if self.since >= 64 * 1024 {
            self.since = 0;
            esp_idf_svc::hal::delay::FreeRtos::delay_ms(1);
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Two backends behind one store: big mutable blobs on the filesystem, everything else in NVS.
pub struct SplitStore {
    /// The transcript and the workspace.
    files: SpiffsStore,
    /// Configuration, manifest, the scheduler, the in-flight marker.
    nvs: crate::nvs_store::NvsStore,
}

impl SplitStore {
    pub fn new(files: SpiffsStore, nvs: crate::nvs_store::NvsStore) -> Self {
        Self { files, nvs }
    }

    /// Which backend owns a key.
    ///
    /// By name, and deliberately: these are the two keys whose size is a function of how long
    /// somebody talks to the board. Everything else has a size the board chose.
    fn is_file_key(key: &str) -> bool {
        key == "transcript"
            || key.starts_with("transcript.")
            || key == "ws"
            || key.starts_with("ws.")
    }

    fn backend(&self, key: &str) -> &dyn KvStore {
        if Self::is_file_key(key) {
            &self.files
        } else {
            &self.nvs
        }
    }

    fn backend_mut(&mut self, key: &str) -> &mut dyn KvStore {
        if Self::is_file_key(key) {
            &mut self.files
        } else {
            &mut self.nvs
        }
    }
}

impl KvStore for SplitStore {
    fn get(&self, key: &str) -> Option<String> {
        self.backend(key).get(key)
    }

    fn set(&mut self, key: &str, value: &str) -> bool {
        self.backend_mut(key).set(key, value)
    }

    fn remove(&mut self, key: &str) -> bool {
        self.backend_mut(key).remove(key)
    }

    /// Forwarded by key: without this the trait default runs, which materialises the whole
    /// value through `get` on *every* 4 KB chunk - a 1.75 MB workspace became a quadratic read
    /// that blocked the boot thread long enough for the task watchdog to reboot the board.
    fn read_at(&self, key: &str, offset: usize, buf: &mut [u8]) -> Option<usize> {
        self.backend(key).read_at(key, offset, buf)
    }

    /// Streamed writes route by key like every other operation, and this override is what
    /// makes the stream reach the filesystem at all: without it the trait default builds the
    /// whole document as one `String` first, which is the megabyte the workspace could not
    /// afford.
    fn set_stream(
        &mut self,
        key: &str,
        fill: &mut dyn FnMut(&mut dyn std::io::Write) -> std::io::Result<()>,
    ) -> bool {
        if !Self::is_file_key(key) {
            return self.nvs.set_stream(key, fill);
        }
        self.files.set_stream(key, fill)
    }

    fn keys(&self) -> Vec<String> {
        let mut out = self.nvs.keys();
        out.extend(self.files.keys());
        out
    }

    fn last_error(&self) -> Option<StoreError> {
        self.nvs.last_error().or_else(|| self.files.last_error())
    }

    /// The filesystem's numbers: it holds the things that grow, so "is the store full?" is a
    /// question about this partition. NVS's entry counts stay on `/api/status.store.nvs`.
    fn space(&self) -> Option<StoreSpace> {
        self.files.space().or_else(|| self.nvs.space())
    }

    fn whole_value_bytes(&self) -> Option<usize> {
        self.files.whole_value_bytes()
    }
}

/// `Path::exists` without the panic: `false` on a path that cannot be stat-ed.
#[allow(dead_code)]
fn exists(path: &Path) -> bool {
    path.metadata().is_ok()
}

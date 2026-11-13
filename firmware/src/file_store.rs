//! [`KvStore`] over a LittleFS file per key, and the routing that decides which keys go there.
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

use esp32_agent_core::store::{
    is_file_store_key, recover_file_backup, FileReadCache, KvStore, StoreError, StoreSpace,
};
use esp_idf_svc::sys;
use std::path::{Path, PathBuf};

/// Where the partition is mounted.
const MOUNT: &str = "/storage";
/// One file per key, directly under the mount point.
///
/// A flat namespace is our layout choice, not a LittleFS restriction: the installed
/// component implements `mkdir` through `lfs_mkdir`. Keys are already unique and short.
const KV_DIR: &str = "/storage";

pub struct FileStore {
    dir: PathBuf,
    last_error: Option<StoreError>,
    /// The file a sequential read is pulling from, kept open: reopening per chunk cost more than
    /// the read did.
    open: FileReadCache,
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
    log::error!("files {context}: {name}");
    StoreError { code, name }
}

/// Mount `storage`.
///
/// `format_if_mount_failed` is on: the partition is empty on a board that has just been given
/// this table. It also permits reformatting an unreadable volume, not just an empty one;
/// see `docs/storage.md`. The old 38 s format measurement was SPIFFS, not LittleFS.
pub fn mount() -> anyhow::Result<FileStore> {
    let label = c"storage";
    let path = c"/storage";
    let mut conf = sys::littlefs::esp_vfs_littlefs_conf_t {
        base_path: path.as_ptr(),
        partition_label: label.as_ptr(),
        partition: std::ptr::null(),
        ..Default::default()
    };
    // SPIFFS and LittleFS cannot read each other, so migration requires formatting.
    // This policy also formats any other unmountable volume; it is not restricted
    // to migration or first boot. See docs/storage.md for the recovery boundary.
    conf.set_format_if_mount_failed(1);
    conf.set_read_only(0);
    conf.set_dont_mount(0);
    conf.set_grow_on_mount(0);
    let rc = unsafe { sys::littlefs::esp_vfs_littlefs_register(&conf) };
    if rc != sys::ESP_OK {
        // `ESP_ERR_INVALID_STATE` means it is already mounted (a restart of the app without a
        // reboot), which is not an error worth failing the boot over.
        if rc != sys::ESP_ERR_INVALID_STATE {
            return Err(anyhow::anyhow!("esp_vfs_littlefs_register: {}", error_of(rc, "mount").name));
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
        let names: Vec<String> = entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        for name in &names {
            if name.ends_with(".tmp") {
                if std::fs::remove_file(format!("{MOUNT}/{name}")).is_ok() {
                    dropped += 1;
                }
            } else if let Some(key) = name.strip_suffix(".old") {
                let value = format!("{MOUNT}/{key}");
                let backup = format!("{MOUNT}/{name}");
                match recover_file_backup(Path::new(&value), Path::new(&backup)) {
                    Ok(true) => restored += 1,
                    Ok(false) => {}
                    Err(e) => log::warn!("storage: could not recover `{name}`: {e}"),
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
    log::info!("storage: LittleFS on `storage`, mounted at {MOUNT}, {used} of {total} bytes used");
    Ok(FileStore { dir: PathBuf::from(KV_DIR), last_error: None, open: FileReadCache::default() })
}

/// The partition's size and what it holds, in bytes. `(0, 0)` if it cannot be asked.
fn capacity() -> (usize, usize) {
    let label = c"storage";
    let mut total: usize = 0;
    let mut used: usize = 0;
    let rc = unsafe { sys::littlefs::esp_littlefs_info(label.as_ptr(), &mut total, &mut used) };
    if rc != sys::ESP_OK {
        return (0, 0);
    }
    (total, used)
}

impl FileStore {
    fn path(&self, key: &str) -> std::io::Result<PathBuf> {
        if !is_file_store_key(key) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file-store key"));
        }
        Ok(self.dir.join(key))
    }

    /// Where a value is written before it is published.
    ///
    /// The suffix marks it as not-a-key, so a value left behind by a power cut is never read
    /// back as one. Keys are at most 15 bytes (`store::KEY_MAX`), so
    /// `transcript.tmp` fits with room to spare.
    fn temp_path(&self, key: &str) -> std::io::Result<PathBuf> {
        self.path(key)?;
        Ok(self.dir.join(format!("{key}.tmp")))
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
    /// whose contents are still in a cache is the same loss with a smaller window. LittleFS
    /// implements both (`CONFIG_VFS_SUPPORT_DIR` is on), and a rename is a directory-level
    /// operation that leaves the old file readable until it completes.
    fn replace(&self, key: &str, value: &str) -> std::io::Result<()> {
        let tmp = self.temp_path(key)?;
        self.open.invalidate(&self.path(key)?);
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
    /// LittleFS 1.23.1 permits replacement through `lfs_rename`. Its VFS rejects
    /// either path while open, so the read cache is closed before the single rename.
    /// The old two-rename SPIFFS workaround created an unnecessary missing-value window.
    fn publish(&self, key: &str, tmp: &Path) -> std::io::Result<()> {
        let path = self.path(key)?;
        self.open.invalidate(&path);
        match std::fs::rename(tmp, &path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(tmp);
                Err(e)
            }
        }
    }

    /// Bytes this partition can still take, which is the honest ceiling for one value.
    fn free_bytes(&self) -> usize {
        let (total, used) = capacity();
        total.saturating_sub(used)
    }
}

impl KvStore for FileStore {
    fn get(&self, key: &str) -> Option<String> {
        match self.path(key).and_then(std::fs::read_to_string) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                log::warn!("files read `{key}`: {e}");
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
                log::error!("files write `{key}`: {name} (the previous value is untouched)");
                self.last_error = Some(StoreError { code, name });
                false
            }
        }
    }

    fn remove(&mut self, key: &str) -> bool {
        self.last_error = None;
        let removed = self.path(key).and_then(|path| {
            self.open.invalidate(&path);
            std::fs::remove_file(path)
        });
        match removed {
            Ok(()) => true,
            // Not there is not a failure: the queue counts a removal it did not have to make
            // as accepted (`NvsStore::remove` says the same).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0);
                let name = format!("{e}");
                log::error!("files remove `{key}`: {name}");
                self.last_error = Some(StoreError { code, name });
                false
            }
        }
    }

    /// Write the value from a callback, streaming into a temporary file and publishing it with
    /// a rename.
    ///
    /// This is the write half of the memory fix: a 4 MB workspace never exists as one `String`
    /// on the way out either. The temporary name is the same trick [`FileStore::replace`]
    /// uses, so a write that fails half way leaves the old value alone.
    fn set_stream(
        &mut self,
        key: &str,
        fill: &mut dyn FnMut(&mut dyn std::io::Write) -> std::io::Result<()>,
    ) -> bool {
        self.last_error = None;
        let tmp = match self.temp_path(key) {
            Ok(tmp) => tmp,
            Err(e) => {
                self.last_error =
                    Some(StoreError { code: e.raw_os_error().unwrap_or(0), name: e.to_string() });
                return false;
            }
        };
        if let Ok(path) = self.path(key) {
            self.open.invalidate(&path);
        }
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
                "files stream `{key}` refused: {e}; esp_littlefs_info total_bytes {} used_bytes {} free_bytes {}",
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
                    "files publish `{key}` refused: {e}; esp_littlefs_info total_bytes {} used_bytes {} free_bytes {}",
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
        let path = self.path(key).ok()?;
        let read = self.open.read_at(&path, offset, buf).ok();
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

    /// What `esp_littlefs_info()` reports, in bytes: configured block count times block
    /// size for total; `lfs_fs_size()` times block size for used. `StoreSpace` computes
    /// free as total minus used. Replacement needs both old and temporary file space;
    /// neither number guarantees one allocation. SPIFFS measurements are not LittleFS budgets.
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
    files: FileStore,
    /// Configuration, manifest, the scheduler, the in-flight marker.
    nvs: crate::nvs_store::NvsStore,
    last_error: Option<StoreError>,
}

impl SplitStore {
    pub fn new(files: FileStore, nvs: crate::nvs_store::NvsStore) -> Self {
        Self { files, nvs, last_error: None }
    }

    /// Which backend owns a key.
    ///
    /// By name, and deliberately: these are the two keys whose size is a function of how long
    /// somebody talks to the board. Everything else has a size the board chose.
    fn is_file_key(key: &str) -> bool {
        key == "transcript" || key.starts_with("transcript.") || key == "ws" || key.starts_with("ws.")
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
        let backend = self.backend_mut(key);
        let ok = backend.set(key, value);
        self.last_error = backend.last_error();
        ok
    }

    fn remove(&mut self, key: &str) -> bool {
        let backend = self.backend_mut(key);
        let ok = backend.remove(key);
        self.last_error = backend.last_error();
        ok
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
        let backend = self.backend_mut(key);
        let ok = backend.set_stream(key, fill);
        self.last_error = backend.last_error();
        ok
    }

    fn keys(&self) -> Vec<String> {
        let mut out = self.nvs.keys();
        out.extend(self.files.keys());
        out
    }

    fn last_error(&self) -> Option<StoreError> {
        self.last_error.clone()
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

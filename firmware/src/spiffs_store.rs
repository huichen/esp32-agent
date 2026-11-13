//! [`KvStore`] over a SPIFFS file per key, and the routing that decides which keys go there.
//!
//! NVS keeps a value to about 4 KB and divides its partition into 32-byte entries, which is
//! fine for a configuration blob and wrong for the two things that grow: the transcript and
//! the file workspace. Both are files here, on the 12 MB `storage` partition, and NVS is left
//! to the configuration and the secrets.
//!
//! The split is not cosmetic. A write to either backend is an `spi_flash` operation with both
//! caches off, so it has to happen on the one task whose stack is in internal RAM — the
//! `nvs-writer` task in `main.rs`. That is why this type is a `KvStore` like any other and not
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
    log::info!("storage: SPIFFS on `storage`, mounted at {MOUNT}, {used} of {total} bytes used");
    Ok(SpiffsStore { dir: PathBuf::from(KV_DIR), last_error: None })
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
        match std::fs::write(self.path(key), value.as_bytes()) {
            Ok(()) => true,
            Err(e) => {
                // `std::io::Error` has no ESP error code; the kind says what a caller can do
                // about it, which is what `/api/status.store.last_failure` is for.
                let code = e.raw_os_error().unwrap_or(0);
                let name = format!("{e}");
                log::error!("spiffs write `{key}`: {name}");
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

    fn keys(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    }

    fn last_error(&self) -> Option<StoreError> {
        self.last_error.clone()
    }

    fn space(&self) -> Option<StoreSpace> {
        let (total, used) = capacity();
        if total == 0 {
            return None;
        }
        Some(StoreSpace {
            // A filesystem has no entries: the byte numbers are the answer.
            used_entries: 0,
            free_entries: 0,
            total_entries: 0,
            total_bytes: Some(total),
            used_bytes: Some(used),
        })
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

//! [`KvStore`] over ESP-IDF NVS.
//!
//! Two keys live here: `config` (WiFi, MCP endpoint, buses, pins) and `manifest`
//! (devices and tools). Both are plain JSON strings, so a firmware update reads
//! what an older one wrote and `#[serde(default)]` fills the gaps.

use esp32_agent_core::store::{KvStore, StoreError, StoreSpace};
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys;

pub struct NvsStore {
    nvs: EspNvs<NvsDefault>,
    /// Why the last `set`/`remove` was refused, cleared at the start of each operation.
    ///
    /// `bool` alone is not a diagnosis: `ESP_ERR_NVS_NOT_ENOUGH_SPACE` and
    /// `ESP_ERR_NVS_KEY_TOO_LONG` are the same `false` to the caller and need opposite
    /// fixes. The store queue reads this straight after a `false`
    /// (`StoreQueue::flush`) and puts it on `/api/status.store.last_failure`.
    last_error: Option<StoreError>,
}

/// The board's own name for an `esp_err_t`, so the wire and the log agree.
fn error_of(e: &esp_idf_svc::sys::EspError) -> StoreError {
    let code = e.code();
    let name = unsafe {
        let ptr = sys::esp_err_to_name(code);
        if ptr.is_null() {
            format!("0x{code:x}")
        } else {
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    StoreError { code, name }
}

impl NvsStore {
    /// The partition is handed in rather than taken here. `EspDefaultNvsPartition`
    /// is a process-wide singleton: the first `take()` owns it and every later one
    /// fails with ESP_ERR_INVALID_STATE, which is what kept the radio thread from
    /// ever reaching `EspWifi::new` while this store held the only handle.
    pub fn open(namespace: &str, partition: EspDefaultNvsPartition) -> anyhow::Result<Self> {
        let nvs = EspNvs::new(partition, namespace, true)?;
        Ok(Self { nvs, last_error: None })
    }
}

impl KvStore for NvsStore {
    fn get(&self, key: &str) -> Option<String> {
        let mut buf = vec![0u8; 4096];
        match self.nvs.get_str(key, &mut buf) {
            Ok(Some(v)) => Some(v.to_string()),
            Ok(None) => None,
            Err(e) => {
                log::warn!("nvs read `{key}`: {e}");
                None
            }
        }
    }

    fn set(&mut self, key: &str, value: &str) -> bool {
        self.last_error = None;
        // esp-idf-svc 0.53.0's set_str erases first. IDF's nvs_set_str already
        // updates existing strings and keeps the old entry until the new write succeeds.
        // Calling it directly avoids losing the old configuration on an ENOSPC refusal.
        let written = (|| -> Result<(), sys::EspError> {
            if value.len() > esp32_agent_core::store::VALUE_MAX {
                return Err(sys::EspError::from(sys::ESP_ERR_NVS_VALUE_TOO_LONG).unwrap());
            }
            let key = std::ffi::CString::new(key)
                .map_err(|_| sys::EspError::from(sys::ESP_ERR_INVALID_ARG).unwrap())?;
            let value = std::ffi::CString::new(value)
                .map_err(|_| sys::EspError::from(sys::ESP_ERR_INVALID_ARG).unwrap())?;
            sys::esp!(unsafe { sys::nvs_set_str(self.nvs.handle(), key.as_ptr(), value.as_ptr()) })?;
            sys::esp!(unsafe { sys::nvs_commit(self.nvs.handle()) })?;
            Ok(())
        })();
        match written {
            Ok(()) => true,
            Err(e) => {
                log::error!("nvs write `{key}`: {e} (esp_err_t {})", e.code());
                self.last_error = Some(error_of(&e));
                false
            }
        }
    }

    fn remove(&mut self, key: &str) -> bool {
        self.last_error = None;
        match self.nvs.remove(key) {
            // `Ok(false)` is "the key was not there", not a failure: the queue counts a
            // removal it did not have to make as accepted, and only a real `Err` is a
            // refusal it will report.
            Ok(_) => true,
            Err(e) => {
                log::error!("nvs remove `{key}`: {e} (esp_err_t {})", e.code());
                self.last_error = Some(error_of(&e));
                false
            }
        }
    }

    fn keys(&self) -> Vec<String> {
        // EspNvs has no key iterator in this version; the firmware only needs the
        // two keys it defines, and both are read explicitly.
        Vec::new()
    }

    fn last_error(&self) -> Option<StoreError> {
        self.last_error.clone()
    }

    /// `nvs_get_stats` for the default partition, which is the one this store was opened
    /// on (`main.rs` takes `EspDefaultNvsPartition`).
    ///
    /// `used`, `free` and `total` are `nvs_stats_t`'s `used_entries`/`free_entries`/
    /// `total_entries`: an entry is 32 bytes and a 4 KB page holds 126 of them, so
    /// `free_entries` is what says whether a refusal is about room or about the name. The two
    /// byte figures are computed in this function from those counts - NVS reports no bytes, and
    /// `StoreSpace` says which of its numbers come from where.
    fn space(&self) -> Option<StoreSpace> {
        let mut stats = std::mem::MaybeUninit::<sys::nvs_stats_t>::uninit();
        let rc = unsafe { sys::nvs_get_stats(std::ptr::null(), stats.as_mut_ptr()) };
        if rc != sys::ESP_OK {
            return None;
        }
        let stats = unsafe { stats.assume_init() };
        Some(StoreSpace::Entries {
            used: stats.used_entries,
            free: stats.free_entries,
            total: stats.total_entries,
            // An entry is 32 bytes, so the byte answer is the entry answer times 32. Given
            // both, a caller can ask "is there room?" of either backend with one expression.
            used_bytes: stats.used_entries.saturating_mul(32),
            total_bytes: stats.total_entries.saturating_mul(32),
        })
    }
}

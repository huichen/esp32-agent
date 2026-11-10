//! [`KvStore`] over ESP-IDF NVS.
//!
//! Two keys live here: `config` (WiFi, MCP endpoint, buses, pins) and `manifest`
//! (devices and tools). Both are plain JSON strings, so a firmware update reads
//! what an older one wrote and `#[serde(default)]` fills the gaps.

use esp32_agent_core::store::KvStore;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

pub struct NvsStore {
    nvs: EspNvs<NvsDefault>,
}

impl NvsStore {
    /// The partition is handed in rather than taken here. `EspDefaultNvsPartition`
    /// is a process-wide singleton: the first `take()` owns it and every later one
    /// fails with ESP_ERR_INVALID_STATE, which is what kept the radio thread from
    /// ever reaching `EspWifi::new` while this store held the only handle.
    pub fn open(namespace: &str, partition: EspDefaultNvsPartition) -> anyhow::Result<Self> {
        let nvs = EspNvs::new(partition, namespace, true)?;
        Ok(Self { nvs })
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
        match self.nvs.set_str(key, value) {
            Ok(()) => true,
            Err(e) => {
                log::error!("nvs write `{key}`: {e}");
                false
            }
        }
    }

    fn remove(&mut self, key: &str) -> bool {
        self.nvs.remove(key).is_ok()
    }

    fn keys(&self) -> Vec<String> {
        // EspNvs has no key iterator in this version; the firmware only needs the
        // two keys it defines, and both are read explicitly.
        Vec::new()
    }
}



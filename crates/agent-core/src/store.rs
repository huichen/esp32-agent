//! Persistence abstraction: NVS on the ESP32, a JSON file on the host.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

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
}

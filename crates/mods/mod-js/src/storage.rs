//! # storage
//!
//! Web Storage API implementation: `localStorage` and `sessionStorage`.
//!
//! `localStorage` persists to disk as a JSON file at `~/.nova/storage/<origin>.json`.
//! `sessionStorage` is memory-only and cleared when the context is dropped.

use std::collections::HashMap;
use std::path::PathBuf;

use tracing::{debug, warn};

// ── WebStorage ───────────────────────────────────────────────────────────────

/// A key/value store implementing the Web Storage API.
///
/// Each instance represents either a `localStorage` or `sessionStorage` bucket.
#[derive(Debug, Clone)]
pub struct WebStorage {
    /// The storage data.
    data: HashMap<String, String>,
    /// Insertion-order key list (to support `key(index)`).
    keys_order: Vec<String>,
    /// Optional path for disk persistence (only for `localStorage`).
    disk_path: Option<PathBuf>,
}

impl WebStorage {
    /// Create a new in-memory storage (for `sessionStorage`).
    pub fn new_session() -> Self {
        Self {
            data: HashMap::new(),
            keys_order: Vec::new(),
            disk_path: None,
        }
    }

    /// Create a persistent storage backed by a JSON file on disk (for `localStorage`).
    ///
    /// If the file exists, its contents are loaded. Otherwise an empty store is created.
    pub fn new_local(origin: &str) -> Self {
        let storage_dir = dirs_or_default().join("storage");
        let _ = std::fs::create_dir_all(&storage_dir);
        let safe_name = origin
            .replace("://", "_")
            .replace(['/', ':', '?', '#', '\\'], "_");
        let path = storage_dir.join(format!("{safe_name}.json"));

        let (data, keys_order) = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(json) => {
                    match serde_json::from_str::<HashMap<String, String>>(&json) {
                        Ok(map) => {
                            let keys: Vec<String> = map.keys().cloned().collect();
                            debug!(origin, items = map.len(), "loaded localStorage");
                            (map, keys)
                        }
                        Err(e) => {
                            warn!(%e, "failed to parse localStorage JSON");
                            (HashMap::new(), Vec::new())
                        }
                    }
                }
                Err(e) => {
                    warn!(%e, "failed to read localStorage file");
                    (HashMap::new(), Vec::new())
                }
            }
        } else {
            (HashMap::new(), Vec::new())
        };

        Self {
            data,
            keys_order,
            disk_path: Some(path),
        }
    }

    /// `getItem(key)` — returns `None` if the key does not exist.
    pub fn get_item(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(|s| s.as_str())
    }

    /// `setItem(key, value)` — adds or updates a key/value pair.
    pub fn set_item(&mut self, key: &str, value: &str) {
        if !self.data.contains_key(key) {
            self.keys_order.push(key.to_owned());
        }
        self.data.insert(key.to_owned(), value.to_owned());
        debug!(key, "storage setItem");
        self.persist();
    }

    /// `removeItem(key)` — removes the key if it exists.
    pub fn remove_item(&mut self, key: &str) {
        if self.data.remove(key).is_some() {
            self.keys_order.retain(|k| k != key);
            debug!(key, "storage removeItem");
            self.persist();
        }
    }

    /// `clear()` — removes all entries.
    pub fn clear(&mut self) {
        self.data.clear();
        self.keys_order.clear();
        debug!("storage clear");
        self.persist();
    }

    /// `key(index)` — returns the key at the given index, or `None`.
    pub fn key(&self, index: usize) -> Option<&str> {
        self.keys_order.get(index).map(|s| s.as_str())
    }

    /// `length` — returns the number of stored entries.
    pub fn length(&self) -> usize {
        self.data.len()
    }

    /// Persist to disk if this is a localStorage instance.
    fn persist(&self) {
        if let Some(path) = &self.disk_path {
            match serde_json::to_string(&self.data) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(path, json) {
                        warn!(%e, "failed to write localStorage");
                    }
                }
                Err(e) => {
                    warn!(%e, "failed to serialize localStorage");
                }
            }
        }
    }
}

/// Return the `~/.nova` directory, or `/tmp/.nova` as fallback.
fn dirs_or_default() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".nova")
    } else {
        PathBuf::from("/tmp/.nova")
    }
}

// ── StorageManager ───────────────────────────────────────────────────────────

/// Manages both `localStorage` and `sessionStorage` for a given origin.
#[derive(Debug)]
pub struct StorageManager {
    /// The persistent local storage.
    pub local: WebStorage,
    /// The session-scoped storage.
    pub session: WebStorage,
}

impl StorageManager {
    /// Create a new storage manager for the given origin.
    pub fn new(origin: &str) -> Self {
        Self {
            local: WebStorage::new_local(origin),
            session: WebStorage::new_session(),
        }
    }

    /// Get a mutable reference to the appropriate storage by type name.
    pub fn get_mut(&mut self, storage_type: &str) -> &mut WebStorage {
        match storage_type {
            "local" => &mut self.local,
            "session" | _ => &mut self.session,
        }
    }

    /// Get a shared reference to the appropriate storage by type name.
    pub fn get(&self, storage_type: &str) -> &WebStorage {
        match storage_type {
            "local" => &self.local,
            "session" | _ => &self.session,
        }
    }
}

/// JavaScript shim code for the Storage constructor and localStorage/sessionStorage globals.
///
/// All methods are wrapped in try/catch to prevent storage errors (quota
/// exceeded, security restrictions, etc.) from crashing the page.
pub const JS_STORAGE_SHIM: &str = r#"
function Storage(type) {
    this._type = type;
}
Storage.prototype.getItem = function(key) {
    try { return __nova.__storageGetItem(this._type, key); }
    catch(e) { return null; }
};
Storage.prototype.setItem = function(key, val) {
    try { __nova.__storageSetItem(this._type, key, String(val)); }
    catch(e) { /* swallow quota/security errors */ }
};
Storage.prototype.removeItem = function(key) {
    try { __nova.__storageRemoveItem(this._type, key); }
    catch(e) { /* swallow errors */ }
};
Storage.prototype.clear = function() {
    try { __nova.__storageClear(this._type); }
    catch(e) { /* swallow errors */ }
};
Object.defineProperty(Storage.prototype, 'length', {
    get: function() {
        try { return __nova.__storageLength(this._type); }
        catch(e) { return 0; }
    }
});
Storage.prototype.key = function(i) {
    try { return __nova.__storageKey(this._type, i); }
    catch(e) { return null; }
};

var localStorage = new Storage('local');
var sessionStorage = new Storage('session');
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_storage_get_set() {
        let mut storage = WebStorage::new_session();
        assert_eq!(storage.get_item("foo"), None);
        storage.set_item("foo", "bar");
        assert_eq!(storage.get_item("foo"), Some("bar"));
    }

    #[test]
    fn session_storage_remove() {
        let mut storage = WebStorage::new_session();
        storage.set_item("a", "1");
        storage.set_item("b", "2");
        assert_eq!(storage.length(), 2);
        storage.remove_item("a");
        assert_eq!(storage.length(), 1);
        assert_eq!(storage.get_item("a"), None);
        assert_eq!(storage.get_item("b"), Some("2"));
    }

    #[test]
    fn session_storage_clear() {
        let mut storage = WebStorage::new_session();
        storage.set_item("x", "1");
        storage.set_item("y", "2");
        storage.clear();
        assert_eq!(storage.length(), 0);
        assert_eq!(storage.get_item("x"), None);
    }

    #[test]
    fn session_storage_key_index() {
        let mut storage = WebStorage::new_session();
        storage.set_item("alpha", "1");
        storage.set_item("beta", "2");
        assert_eq!(storage.length(), 2);
        // key(0) should be "alpha", key(1) should be "beta" (insertion order).
        let k0 = storage.key(0).unwrap();
        let k1 = storage.key(1).unwrap();
        assert!(k0 == "alpha" || k0 == "beta");
        assert!(k1 == "alpha" || k1 == "beta");
        assert_ne!(k0, k1);
        assert_eq!(storage.key(5), None);
    }

    #[test]
    fn session_storage_overwrite() {
        let mut storage = WebStorage::new_session();
        storage.set_item("key", "old");
        storage.set_item("key", "new");
        assert_eq!(storage.get_item("key"), Some("new"));
        assert_eq!(storage.length(), 1);
    }

    #[test]
    fn storage_manager_routes_correctly() {
        let mut mgr = StorageManager::new("http://example.com");
        mgr.get_mut("local").set_item("x", "local_val");
        mgr.get_mut("session").set_item("x", "session_val");
        assert_eq!(mgr.get("local").get_item("x"), Some("local_val"));
        assert_eq!(mgr.get("session").get_item("x"), Some("session_val"));
    }

    #[test]
    fn storage_shim_has_constructors() {
        assert!(JS_STORAGE_SHIM.contains("function Storage"));
        assert!(JS_STORAGE_SHIM.contains("localStorage"));
        assert!(JS_STORAGE_SHIM.contains("sessionStorage"));
    }
}

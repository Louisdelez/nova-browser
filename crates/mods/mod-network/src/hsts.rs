//! # HTTP Strict Transport Security (HSTS)
//!
//! Parses `Strict-Transport-Security` response headers, maintains an HSTS
//! store (domain -> expiry + includeSubDomains), and upgrades `http://`
//! requests to `https://` when a matching HSTS entry exists.
//!
//! The HSTS list is persisted to `~/.nova/hsts.json`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// A single HSTS entry for a domain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HstsEntry {
    /// The domain this entry applies to.
    pub domain: String,
    /// When this entry expires (seconds since UNIX epoch).
    pub expires_at: u64,
    /// Whether to include subdomains.
    pub include_sub_domains: bool,
}

/// The HSTS store — maps domains to their HSTS policy.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HstsStore {
    entries: HashMap<String, HstsEntry>,
}

impl HstsStore {
    /// Create a new empty HSTS store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the HSTS store from disk (`~/.nova/hsts.json`).
    ///
    /// Returns a new empty store if the file doesn't exist or can't be read.
    pub fn load() -> Self {
        let path = Self::storage_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(store) => {
                    info!(path = %path.display(), "loaded HSTS store from disk");
                    store
                }
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to parse HSTS store, starting fresh"
                    );
                    Self::new()
                }
            },
            Err(_) => {
                debug!("no HSTS store on disk, starting fresh");
                Self::new()
            }
        }
    }

    /// Save the HSTS store to disk (`~/.nova/hsts.json`).
    pub fn save(&self) {
        let path = Self::storage_path();
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(error = %e, "failed to create HSTS storage directory");
                return;
            }
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(error = %e, "failed to write HSTS store");
                } else {
                    debug!(path = %path.display(), entries = self.entries.len(), "saved HSTS store");
                }
            }
            Err(e) => warn!(error = %e, "failed to serialize HSTS store"),
        }
    }

    /// Get the storage path for the HSTS file.
    fn storage_path() -> PathBuf {
        dirs_next().join("hsts.json")
    }

    /// Parse a `Strict-Transport-Security` header and store the entry.
    ///
    /// Example header: `max-age=31536000; includeSubDomains`
    pub fn parse_and_store(&mut self, domain: &str, header: &str) {
        let mut max_age: Option<u64> = None;
        let mut include_sub = false;

        for part in header.split(';') {
            let part = part.trim().to_lowercase();
            if let Some(age_str) = part.strip_prefix("max-age=") {
                max_age = age_str.trim().parse::<u64>().ok();
            } else if part == "includesubdomains" {
                include_sub = true;
            }
        }

        if let Some(age) = max_age {
            let now = current_epoch_secs();
            if age == 0 {
                // max-age=0 means remove the HSTS entry.
                self.entries.remove(domain);
                debug!(domain = domain, "HSTS entry removed (max-age=0)");
            } else {
                let entry = HstsEntry {
                    domain: domain.to_string(),
                    expires_at: now + age,
                    include_sub_domains: include_sub,
                };
                info!(
                    domain = domain,
                    max_age = age,
                    include_sub_domains = include_sub,
                    "HSTS entry stored"
                );
                self.entries.insert(domain.to_string(), entry);
            }
        }
    }

    /// Check if a domain has a valid (non-expired) HSTS entry.
    ///
    /// Also checks parent domains with `includeSubDomains`.
    pub fn has_hsts(&self, domain: &str) -> bool {
        let now = current_epoch_secs();

        // Direct match.
        if let Some(entry) = self.entries.get(domain) {
            if entry.expires_at > now {
                return true;
            }
        }

        // Check parent domains with includeSubDomains.
        let parts: Vec<&str> = domain.split('.').collect();
        for i in 1..parts.len() {
            let parent = parts[i..].join(".");
            if let Some(entry) = self.entries.get(&parent) {
                if entry.expires_at > now && entry.include_sub_domains {
                    return true;
                }
            }
        }

        false
    }

    /// Upgrade a URL from `http://` to `https://` if the domain has HSTS.
    ///
    /// Returns the upgraded URL, or the original URL if no upgrade is needed.
    pub fn maybe_upgrade(&self, url: &str) -> String {
        if !url.starts_with("http://") {
            return url.to_string();
        }

        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(host) = parsed.host_str() {
                if self.has_hsts(host) {
                    let upgraded = url.replacen("http://", "https://", 1);
                    info!(
                        original = url,
                        upgraded = %upgraded,
                        "HSTS: upgrading to HTTPS"
                    );
                    return upgraded;
                }
            }
        }

        url.to_string()
    }

    /// Remove expired entries.
    pub fn evict_expired(&mut self) {
        let now = current_epoch_secs();
        self.entries.retain(|_, v| v.expires_at > now);
    }

    /// Return the number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Get the NOVA data directory (`~/.nova/`).
fn dirs_next() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".nova")
}

/// Get the current time as seconds since the UNIX epoch.
fn current_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_hsts() {
        let mut store = HstsStore::new();
        store.parse_and_store("example.com", "max-age=31536000");
        assert!(store.has_hsts("example.com"));
        assert!(!store.has_hsts("other.com"));
    }

    #[test]
    fn include_subdomains() {
        let mut store = HstsStore::new();
        store.parse_and_store("example.com", "max-age=31536000; includeSubDomains");
        assert!(store.has_hsts("example.com"));
        assert!(store.has_hsts("sub.example.com"));
        assert!(store.has_hsts("deep.sub.example.com"));
        assert!(!store.has_hsts("other.com"));
    }

    #[test]
    fn max_age_zero_removes() {
        let mut store = HstsStore::new();
        store.parse_and_store("example.com", "max-age=31536000");
        assert!(store.has_hsts("example.com"));
        store.parse_and_store("example.com", "max-age=0");
        assert!(!store.has_hsts("example.com"));
    }

    #[test]
    fn upgrade_http_to_https() {
        let mut store = HstsStore::new();
        store.parse_and_store("example.com", "max-age=31536000");

        let result = store.maybe_upgrade("http://example.com/page");
        assert_eq!(result, "https://example.com/page");

        // HTTPS URLs should not be modified.
        let result = store.maybe_upgrade("https://example.com/page");
        assert_eq!(result, "https://example.com/page");

        // Domains without HSTS should not be modified.
        let result = store.maybe_upgrade("http://other.com/page");
        assert_eq!(result, "http://other.com/page");
    }

    #[test]
    fn case_insensitive_header_parsing() {
        let mut store = HstsStore::new();
        store.parse_and_store("example.com", "Max-Age=3600; IncludeSubDomains");
        assert!(store.has_hsts("example.com"));
        assert!(store.has_hsts("sub.example.com"));
    }
}

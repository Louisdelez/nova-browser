//! # Bookmarks
//!
//! Persistent bookmarks for the NOVA browser.
//! Stored in `~/.nova/bookmarks.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// A single bookmark entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    /// The page title.
    pub title: String,
    /// The bookmark URL.
    pub url: String,
    /// When the bookmark was created (seconds since UNIX epoch).
    pub created_at: u64,
    /// Optional folder name for organizing bookmarks.
    pub folder: Option<String>,
}

/// The bookmark store — manages a persistent list of bookmarks.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BookmarkStore {
    bookmarks: Vec<Bookmark>,
}

impl BookmarkStore {
    /// Create a new empty bookmark store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load bookmarks from disk (`~/.nova/bookmarks.json`).
    pub fn load() -> Self {
        let path = Self::storage_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(store) => {
                    info!(
                        path = %path.display(),
                        "loaded bookmarks from disk"
                    );
                    store
                }
                Err(e) => {
                    warn!(error = %e, "failed to parse bookmarks, starting fresh");
                    Self::new()
                }
            },
            Err(_) => {
                debug!("no bookmarks file on disk, starting fresh");
                Self::new()
            }
        }
    }

    /// Save bookmarks to disk.
    pub fn save(&self) {
        let path = Self::storage_path();
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(error = %e, "failed to create bookmarks directory");
                return;
            }
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(error = %e, "failed to write bookmarks");
                } else {
                    debug!(count = self.bookmarks.len(), "saved bookmarks");
                }
            }
            Err(e) => warn!(error = %e, "failed to serialize bookmarks"),
        }
    }

    /// Get the storage path.
    fn storage_path() -> PathBuf {
        nova_data_dir().join("bookmarks.json")
    }

    /// Add a bookmark for the given URL and title.
    ///
    /// If a bookmark for this URL already exists, it is not duplicated.
    pub fn add(&mut self, url: &str, title: &str) {
        if self.is_bookmarked(url) {
            debug!(url = url, "URL already bookmarked, skipping");
            return;
        }

        let bookmark = Bookmark {
            title: title.to_string(),
            url: url.to_string(),
            created_at: current_epoch_secs(),
            folder: None,
        };
        info!(url = url, title = title, "bookmark added");
        self.bookmarks.push(bookmark);
        self.save();
    }

    /// Remove a bookmark by URL.
    pub fn remove(&mut self, url: &str) {
        let before = self.bookmarks.len();
        self.bookmarks.retain(|b| b.url != url);
        if self.bookmarks.len() < before {
            info!(url = url, "bookmark removed");
            self.save();
        }
    }

    /// Toggle a bookmark: add if not present, remove if present.
    ///
    /// Returns `true` if the bookmark is now present, `false` if removed.
    pub fn toggle(&mut self, url: &str, title: &str) -> bool {
        if self.is_bookmarked(url) {
            self.remove(url);
            false
        } else {
            self.add(url, title);
            true
        }
    }

    /// Check if a URL is bookmarked.
    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.bookmarks.iter().any(|b| b.url == url)
    }

    /// Get all bookmarks (most recent first).
    pub fn all(&self) -> Vec<&Bookmark> {
        let mut sorted: Vec<&Bookmark> = self.bookmarks.iter().collect();
        sorted.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        sorted
    }

    /// Get bookmarks in a specific folder.
    pub fn in_folder(&self, folder: &str) -> Vec<&Bookmark> {
        self.bookmarks
            .iter()
            .filter(|b| b.folder.as_deref() == Some(folder))
            .collect()
    }

    /// Return the total number of bookmarks.
    pub fn count(&self) -> usize {
        self.bookmarks.len()
    }
}

/// Get the NOVA data directory (`~/.nova/`).
fn nova_data_dir() -> PathBuf {
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
    fn add_and_check_bookmark() {
        let mut store = BookmarkStore::new();
        store.add("https://example.com", "Example");
        assert!(store.is_bookmarked("https://example.com"));
        assert!(!store.is_bookmarked("https://other.com"));
    }

    #[test]
    fn no_duplicate_bookmarks() {
        let mut store = BookmarkStore::new();
        store.add("https://example.com", "Example");
        store.add("https://example.com", "Example Again");
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn remove_bookmark() {
        let mut store = BookmarkStore::new();
        store.add("https://example.com", "Example");
        store.remove("https://example.com");
        assert!(!store.is_bookmarked("https://example.com"));
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn toggle_bookmark() {
        let mut store = BookmarkStore::new();
        assert!(store.toggle("https://example.com", "Example"));
        assert!(store.is_bookmarked("https://example.com"));
        assert!(!store.toggle("https://example.com", "Example"));
        assert!(!store.is_bookmarked("https://example.com"));
    }

    #[test]
    fn all_returns_recent_first() {
        let mut store = BookmarkStore::new();
        store.bookmarks.push(Bookmark {
            title: "Old".into(),
            url: "https://old.com".into(),
            created_at: 1000,
            folder: None,
        });
        store.bookmarks.push(Bookmark {
            title: "New".into(),
            url: "https://new.com".into(),
            created_at: 2000,
            folder: None,
        });
        let all = store.all();
        assert_eq!(all[0].url, "https://new.com");
        assert_eq!(all[1].url, "https://old.com");
    }
}

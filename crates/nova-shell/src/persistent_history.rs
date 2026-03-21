//! # Persistent History
//!
//! Records every page navigation to `~/.nova/history.json`.
//! Provides search and clear functionality.
//!
//! This is distinct from the per-tab `HistoryStack` (back/forward navigation).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// A single history entry recording a page visit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentHistoryEntry {
    /// The visited URL.
    pub url: String,
    /// The page title (if known).
    pub title: String,
    /// When the page was visited (seconds since UNIX epoch).
    pub visited_at: u64,
}

/// The persistent history store.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistentHistory {
    entries: Vec<PersistentHistoryEntry>,
}

impl PersistentHistory {
    /// Create a new empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load history from disk (`~/.nova/history.json`).
    pub fn load() -> Self {
        let path = Self::storage_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(store) => {
                    info!(path = %path.display(), "loaded history from disk");
                    store
                }
                Err(e) => {
                    warn!(error = %e, "failed to parse history, starting fresh");
                    Self::new()
                }
            },
            Err(_) => {
                debug!("no history file on disk, starting fresh");
                Self::new()
            }
        }
    }

    /// Save history to disk.
    pub fn save(&self) {
        let path = Self::storage_path();
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(error = %e, "failed to create history directory");
                return;
            }
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(error = %e, "failed to write history");
                } else {
                    debug!(count = self.entries.len(), "saved history");
                }
            }
            Err(e) => warn!(error = %e, "failed to serialize history"),
        }
    }

    /// Get the storage path.
    fn storage_path() -> PathBuf {
        nova_data_dir().join("history.json")
    }

    /// Record a page visit.
    pub fn record(&mut self, url: &str, title: &str) {
        let entry = PersistentHistoryEntry {
            url: url.to_string(),
            title: title.to_string(),
            visited_at: current_epoch_secs(),
        };
        debug!(url = url, "history: recorded visit");
        self.entries.push(entry);

        // Auto-save every 10 entries to avoid data loss.
        if self.entries.len() % 10 == 0 {
            self.save();
        }
    }

    /// Get all history entries (most recent first).
    pub fn all(&self) -> Vec<&PersistentHistoryEntry> {
        let mut sorted: Vec<&PersistentHistoryEntry> = self.entries.iter().collect();
        sorted.sort_by(|a, b| b.visited_at.cmp(&a.visited_at));
        sorted
    }

    /// Search history entries by URL or title substring (case-insensitive).
    pub fn search(&self, query: &str) -> Vec<&PersistentHistoryEntry> {
        let lower_query = query.to_lowercase();
        let mut results: Vec<&PersistentHistoryEntry> = self
            .entries
            .iter()
            .filter(|e| {
                e.url.to_lowercase().contains(&lower_query)
                    || e.title.to_lowercase().contains(&lower_query)
            })
            .collect();
        results.sort_by(|a, b| b.visited_at.cmp(&a.visited_at));
        results
    }

    /// Clear all history entries.
    pub fn clear(&mut self) {
        info!("history: clearing all entries");
        self.entries.clear();
        self.save();
    }

    /// Return the total number of history entries.
    pub fn count(&self) -> usize {
        self.entries.len()
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
    fn record_and_retrieve() {
        let mut history = PersistentHistory::new();
        history.record("https://example.com", "Example");
        assert_eq!(history.count(), 1);
        let all = history.all();
        assert_eq!(all[0].url, "https://example.com");
    }

    #[test]
    fn search_by_url() {
        let mut history = PersistentHistory::new();
        history.record("https://example.com", "Example");
        history.record("https://rust-lang.org", "Rust");
        let results = history.search("rust");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://rust-lang.org");
    }

    #[test]
    fn search_by_title() {
        let mut history = PersistentHistory::new();
        history.record("https://example.com", "My Example Page");
        let results = history.search("example");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn clear_history() {
        let mut history = PersistentHistory::new();
        history.record("https://example.com", "Example");
        history.record("https://rust-lang.org", "Rust");
        history.clear();
        assert_eq!(history.count(), 0);
    }

    #[test]
    fn most_recent_first() {
        let mut history = PersistentHistory::new();
        history.entries.push(PersistentHistoryEntry {
            url: "https://old.com".into(),
            title: "Old".into(),
            visited_at: 1000,
        });
        history.entries.push(PersistentHistoryEntry {
            url: "https://new.com".into(),
            title: "New".into(),
            visited_at: 2000,
        });
        let all = history.all();
        assert_eq!(all[0].url, "https://new.com");
        assert_eq!(all[1].url, "https://old.com");
    }
}

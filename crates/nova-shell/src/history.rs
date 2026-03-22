//! # History
//!
//! Per-tab back/forward navigation history for the NOVA browser.
//! Each tab maintains its own history stack.

/// A single entry in the navigation history.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    /// The URL of this history entry.
    pub url: String,
    /// The vertical scroll position when this page was last viewed.
    pub scroll_y: f32,
}

/// A stack of history entries with a current position pointer.
///
/// Supports back/forward navigation. Pushing a new URL while
/// not at the end of the stack truncates the forward history.
#[derive(Debug, Clone)]
pub struct HistoryStack {
    /// All history entries.
    entries: Vec<HistoryEntry>,
    /// Index of the current entry.
    current_index: usize,
}

impl HistoryStack {
    /// Create a new history stack with an initial URL.
    pub fn new(initial_url: &str) -> Self {
        Self {
            entries: vec![HistoryEntry {
                url: initial_url.to_string(),
                scroll_y: 0.0,
            }],
            current_index: 0,
        }
    }

    /// Push a new URL onto the history stack.
    ///
    /// If the current position is not at the end of the stack,
    /// all forward entries are discarded.
    pub fn push(&mut self, url: &str) {
        // Save current scroll position could be done by the caller
        // before pushing.

        // Truncate any forward history.
        self.entries.truncate(self.current_index + 1);

        // Add the new entry.
        self.entries.push(HistoryEntry {
            url: url.to_string(),
            scroll_y: 0.0,
        });
        self.current_index = self.entries.len() - 1;
    }

    /// Navigate back in history. Returns the entry to navigate to,
    /// or `None` if already at the beginning.
    pub fn back(&mut self) -> Option<&HistoryEntry> {
        if self.current_index > 0 {
            self.current_index -= 1;
            Some(&self.entries[self.current_index])
        } else {
            None
        }
    }

    /// Navigate forward in history. Returns the entry to navigate to,
    /// or `None` if already at the end.
    pub fn forward(&mut self) -> Option<&HistoryEntry> {
        if self.current_index + 1 < self.entries.len() {
            self.current_index += 1;
            Some(&self.entries[self.current_index])
        } else {
            None
        }
    }

    /// Check if back navigation is possible.
    pub fn can_go_back(&self) -> bool {
        self.current_index > 0
    }

    /// Check if forward navigation is possible.
    pub fn can_go_forward(&self) -> bool {
        self.current_index + 1 < self.entries.len()
    }

    /// Get the current history entry.
    pub fn current(&self) -> &HistoryEntry {
        &self.entries[self.current_index]
    }

    /// Replace the URL of the current history entry.
    ///
    /// Used by `history.replaceState()` to update the current entry
    /// without adding a new one.
    pub fn replace_current(&mut self, url: &str) {
        self.entries[self.current_index].url = url.to_string();
    }

    /// Get a mutable reference to the current history entry.
    ///
    /// Useful for updating the scroll position before navigating away.
    pub fn current_mut(&mut self) -> &mut HistoryEntry {
        &mut self.entries[self.current_index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_history_has_initial_entry() {
        let h = HistoryStack::new("http://example.com");
        assert_eq!(h.current().url, "http://example.com");
        assert!(!h.can_go_back());
        assert!(!h.can_go_forward());
    }

    #[test]
    fn push_adds_entry() {
        let mut h = HistoryStack::new("http://a.com");
        h.push("http://b.com");
        assert_eq!(h.current().url, "http://b.com");
        assert!(h.can_go_back());
        assert!(!h.can_go_forward());
    }

    #[test]
    fn back_navigates() {
        let mut h = HistoryStack::new("http://a.com");
        h.push("http://b.com");
        h.push("http://c.com");

        let entry = h.back().unwrap();
        assert_eq!(entry.url, "http://b.com");
        assert!(h.can_go_back());
        assert!(h.can_go_forward());

        let entry = h.back().unwrap();
        assert_eq!(entry.url, "http://a.com");
        assert!(!h.can_go_back());
        assert!(h.can_go_forward());

        // Can't go back further.
        assert!(h.back().is_none());
    }

    #[test]
    fn forward_navigates() {
        let mut h = HistoryStack::new("http://a.com");
        h.push("http://b.com");
        h.push("http://c.com");
        h.back();
        h.back();

        let entry = h.forward().unwrap();
        assert_eq!(entry.url, "http://b.com");

        let entry = h.forward().unwrap();
        assert_eq!(entry.url, "http://c.com");

        assert!(h.forward().is_none());
    }

    #[test]
    fn push_truncates_forward_history() {
        let mut h = HistoryStack::new("http://a.com");
        h.push("http://b.com");
        h.push("http://c.com");
        h.back(); // now at b
        h.push("http://d.com"); // should truncate c

        assert_eq!(h.current().url, "http://d.com");
        assert!(!h.can_go_forward());
        assert!(h.can_go_back());

        let entry = h.back().unwrap();
        assert_eq!(entry.url, "http://b.com");

        let entry = h.back().unwrap();
        assert_eq!(entry.url, "http://a.com");

        // c.com should be gone.
        h.forward();
        h.forward();
        assert_eq!(h.current().url, "http://d.com");
    }

    #[test]
    fn current_mut_updates_scroll() {
        let mut h = HistoryStack::new("http://a.com");
        h.current_mut().scroll_y = 150.0;
        assert_eq!(h.current().scroll_y, 150.0);
    }
}

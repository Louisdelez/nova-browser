//! # Tab Management
//!
//! Manages browser tabs for the NOVA browser. Each tab has its own
//! URL, render state, scroll position, and navigation history.

use std::time::Instant;

use nova_mod_api::RenderCommands;

use crate::history::HistoryStack;
use crate::window::{AnchorRegion, CursorRegion, FormFieldRegion, HitRegion};

/// A single browser tab.
pub struct Tab {
    /// Unique identifier for this tab.
    pub id: u64,
    /// The current URL displayed in this tab.
    pub url: String,
    /// The page title.
    pub title: String,
    /// The rendered content.
    pub render_commands: RenderCommands,
    /// Current vertical scroll offset.
    pub scroll_y: f32,
    /// Current horizontal scroll offset.
    pub scroll_x: f32,
    /// Total height of the rendered content.
    pub content_height: f32,
    /// Total width of the rendered content.
    pub content_width: f32,
    /// Clickable link regions.
    pub hit_regions: Vec<HitRegion>,
    /// Form field regions.
    pub form_fields: Vec<FormFieldRegion>,
    /// Anchor regions for #section scrolling.
    pub anchor_regions: Vec<AnchorRegion>,
    /// Regions with custom CSS cursor properties.
    pub cursor_regions: Vec<CursorRegion>,
    /// Navigation history for this tab.
    pub history: HistoryStack,

    // -- Form field interaction state --
    /// Index of the currently focused form field, or None.
    pub focused_field: Option<usize>,
    /// Cursor position (character index) within the focused text field.
    pub form_cursor_pos: usize,
    /// Text selection range in the focused form field as `(start, end)` byte offsets.
    /// When `start == end`, there is no selection. The cursor is always at `form_cursor_pos`.
    pub form_selection: Option<(usize, usize)>,
    /// Timestamp when the cursor last toggled visibility (for blinking).
    pub cursor_blink_time: Instant,
    /// Whether the cursor is currently visible (toggles every 500ms).
    pub cursor_visible: bool,

    // -- Multi-click tracking for form fields --
    /// Timestamp of the last left-click on a form field (for double/triple-click detection).
    pub last_field_click_time: Option<Instant>,
    /// Number of consecutive rapid clicks (1=single, 2=double, 3=triple).
    pub field_click_count: u32,

    // -- Select dropdown state --
    /// Whether a dropdown is open for a `<select>` field.
    pub dropdown_open: bool,
    /// Index of the form field whose dropdown is open.
    pub dropdown_field_idx: usize,
    /// Index of the currently hovered option in the dropdown.
    pub dropdown_hover_idx: Option<usize>,

    // -- Smooth scrolling --
    /// Target vertical scroll offset for smooth scroll animation.
    pub scroll_target_y: f32,
    /// Target horizontal scroll offset for smooth scroll animation.
    pub scroll_target_x: f32,

    // -- Find in Page (Ctrl+F) --
    /// Whether the find bar is visible.
    pub find_bar_visible: bool,
    /// Current search query text.
    pub find_query: String,
    /// All match positions.
    pub find_matches: Vec<FindMatch>,
    /// Index of the current highlighted match.
    pub find_current: usize,
}

/// A match found during Find in Page.
#[derive(Debug, Clone)]
pub struct FindMatch {
    /// Y coordinate of the match in page coordinates.
    pub y: f32,
    /// X coordinate of the match.
    pub x: f32,
    /// Width of the matched text.
    pub width: f32,
    /// Height of the matched text line.
    pub height: f32,
}

impl Tab {
    /// Create a new tab with the given URL and render commands.
    pub fn new(id: u64, url: &str, commands: RenderCommands) -> Self {
        Self {
            id,
            url: url.to_string(),
            title: url.to_string(),
            render_commands: commands,
            scroll_y: 0.0,
            scroll_x: 0.0,
            content_height: 0.0,
            content_width: 0.0,
            hit_regions: Vec::new(),
            form_fields: Vec::new(),
            anchor_regions: Vec::new(),
            cursor_regions: Vec::new(),
            history: HistoryStack::new(url),
            focused_field: None,
            form_cursor_pos: 0,
            form_selection: None,
            last_field_click_time: None,
            field_click_count: 0,
            cursor_blink_time: Instant::now(),
            cursor_visible: true,
            scroll_target_y: 0.0,
            scroll_target_x: 0.0,
            dropdown_open: false,
            dropdown_field_idx: 0,
            dropdown_hover_idx: None,
            find_bar_visible: false,
            find_query: String::new(),
            find_matches: Vec::new(),
            find_current: 0,
        }
    }
}

/// Manages multiple browser tabs.
pub struct TabManager {
    /// All open tabs.
    tabs: Vec<Tab>,
    /// Index of the currently active tab.
    active_tab_index: usize,
    /// Counter for generating unique tab IDs.
    next_tab_id: u64,
}

impl TabManager {
    /// Create a new tab manager with an initial tab.
    pub fn new(initial_tab: Tab) -> Self {
        Self {
            tabs: vec![initial_tab],
            active_tab_index: 0,
            next_tab_id: 1,
        }
    }

    /// Get a reference to the active tab.
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab_index]
    }

    /// Get a mutable reference to the active tab.
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab_index]
    }

    /// Create a new tab and make it active.
    ///
    /// Returns a mutable reference to the new tab.
    pub fn new_tab(&mut self, url: &str, commands: RenderCommands) -> &mut Tab {
        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let tab = Tab::new(id, url, commands);

        self.tabs.push(tab);
        self.active_tab_index = self.tabs.len() - 1;
        &mut self.tabs[self.active_tab_index]
    }

    /// Close a tab at the given index.
    ///
    /// If this is the last tab, it is not closed (the browser
    /// always keeps at least one tab open).
    /// After closing, the active index is adjusted if needed.
    pub fn close_tab(&mut self, index: usize) {
        if self.tabs.len() <= 1 {
            return; // Keep at least one tab.
        }
        if index >= self.tabs.len() {
            return;
        }

        self.tabs.remove(index);

        // Adjust active index.
        if self.active_tab_index >= self.tabs.len() {
            self.active_tab_index = self.tabs.len() - 1;
        } else if index < self.active_tab_index {
            self.active_tab_index -= 1;
        }
    }

    /// Switch to the tab at the given index.
    pub fn switch_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active_tab_index = index;
        }
    }

    /// Switch to the next tab (wraps around).
    pub fn next_tab(&mut self) {
        self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
    }

    /// Switch to the previous tab (wraps around).
    pub fn prev_tab(&mut self) {
        if self.active_tab_index == 0 {
            self.active_tab_index = self.tabs.len() - 1;
        } else {
            self.active_tab_index -= 1;
        }
    }

    /// Return the number of open tabs.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Return a reference to all tabs (for rendering the tab bar).
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// Return the active tab index.
    pub fn active_index(&self) -> usize {
        self.active_tab_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tab(url: &str) -> Tab {
        Tab::new(0, url, RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None })
    }

    #[test]
    fn new_tab_manager_has_one_tab() {
        let mgr = TabManager::new(make_tab("http://a.com"));
        assert_eq!(mgr.tab_count(), 1);
        assert_eq!(mgr.active_tab().url, "http://a.com");
    }

    #[test]
    fn new_tab_becomes_active() {
        let mut mgr = TabManager::new(make_tab("http://a.com"));
        let commands = RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None };
        mgr.new_tab("http://b.com", commands);
        assert_eq!(mgr.tab_count(), 2);
        assert_eq!(mgr.active_tab().url, "http://b.com");
        assert_eq!(mgr.active_index(), 1);
    }

    #[test]
    fn switch_tabs() {
        let mut mgr = TabManager::new(make_tab("http://a.com"));
        let commands = RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None };
        mgr.new_tab("http://b.com", commands);

        mgr.switch_to(0);
        assert_eq!(mgr.active_tab().url, "http://a.com");

        mgr.switch_to(1);
        assert_eq!(mgr.active_tab().url, "http://b.com");

        // Invalid index does nothing.
        mgr.switch_to(99);
        assert_eq!(mgr.active_tab().url, "http://b.com");
    }

    #[test]
    fn close_tab_adjusts_active() {
        let mut mgr = TabManager::new(make_tab("http://a.com"));
        let c1 = RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None };
        let c2 = RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None };
        mgr.new_tab("http://b.com", c1);
        mgr.new_tab("http://c.com", c2);

        // Active is c.com (index 2). Close b.com (index 1).
        mgr.close_tab(1);
        assert_eq!(mgr.tab_count(), 2);
        // Active index should adjust from 2 to 1.
        assert_eq!(mgr.active_tab().url, "http://c.com");
    }

    #[test]
    fn close_last_remaining_tab_does_nothing() {
        let mut mgr = TabManager::new(make_tab("http://a.com"));
        mgr.close_tab(0);
        assert_eq!(mgr.tab_count(), 1);
        assert_eq!(mgr.active_tab().url, "http://a.com");
    }

    #[test]
    fn close_active_tab_selects_previous() {
        let mut mgr = TabManager::new(make_tab("http://a.com"));
        let c1 = RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None };
        mgr.new_tab("http://b.com", c1);

        // Active is b.com (index 1). Close it.
        mgr.close_tab(1);
        assert_eq!(mgr.tab_count(), 1);
        assert_eq!(mgr.active_tab().url, "http://a.com");
    }

    #[test]
    fn next_and_prev_tab_cycle() {
        let mut mgr = TabManager::new(make_tab("http://a.com"));
        let c1 = RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None };
        let c2 = RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None };
        mgr.new_tab("http://b.com", c1);
        mgr.new_tab("http://c.com", c2);

        // At c (index 2).
        mgr.next_tab(); // wraps to 0 (a).
        assert_eq!(mgr.active_tab().url, "http://a.com");

        mgr.prev_tab(); // wraps to 2 (c).
        assert_eq!(mgr.active_tab().url, "http://c.com");

        mgr.prev_tab(); // 1 (b).
        assert_eq!(mgr.active_tab().url, "http://b.com");
    }

    #[test]
    fn active_tab_mut_modifies() {
        let mut mgr = TabManager::new(make_tab("http://a.com"));
        mgr.active_tab_mut().scroll_y = 100.0;
        assert_eq!(mgr.active_tab().scroll_y, 100.0);
    }
}

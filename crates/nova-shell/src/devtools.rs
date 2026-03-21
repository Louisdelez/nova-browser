//! # DevTools Panel
//!
//! Basic browser developer tools rendered directly into the framebuffer.
//! Provides Console, Elements, and Network tabs similar to browser DevTools.
//!
//! ## Tabs
//!
//! - **Console**: Displays captured `console.log/warn/error/info/debug` messages
//!   from the JS engine with color coding.
//! - **Elements**: Tree view of the DOM with expandable/collapsible nodes,
//!   showing tag name, id, classes, and computed styles for the selected element.
//! - **Network**: Lists network requests with URL, method, status, type, size,
//!   and duration columns with color coding by status.

use std::time::Instant;

use nova_mod_api::content::DomNode;
use nova_mod_api::Color;

use crate::renderer::Framebuffer;

// ── Constants ────────────────────────────────────────────────────────────────

/// Height of the DevTools tab bar in pixels.
const TAB_BAR_HEIGHT: f32 = 30.0;
/// Font size for DevTools content.
const DEVTOOLS_FONT_SIZE: f32 = 12.0;
/// Line height for DevTools content.
const DEVTOOLS_LINE_HEIGHT: f32 = 18.0;
/// Left padding for DevTools content.
const DEVTOOLS_PADDING: f32 = 8.0;
/// Indent width per tree level in the Elements panel.
const TREE_INDENT: f32 = 16.0;
/// Height of the resize handle at the top of DevTools.
const RESIZE_HANDLE_HEIGHT: f32 = 4.0;
/// Minimum height of the DevTools panel.
const MIN_DEVTOOLS_HEIGHT: f32 = 100.0;
/// Maximum fraction of window height DevTools can occupy.
const MAX_DEVTOOLS_FRACTION: f32 = 0.75;

// ── Colors ───────────────────────────────────────────────────────────────────

const BG_COLOR: Color = Color { r: 0.15, g: 0.15, b: 0.15, a: 1.0 };
const TAB_BG: Color = Color { r: 0.20, g: 0.20, b: 0.20, a: 1.0 };
const TAB_ACTIVE_BG: Color = Color { r: 0.28, g: 0.28, b: 0.28, a: 1.0 };
const TAB_TEXT: Color = Color { r: 0.85, g: 0.85, b: 0.85, a: 1.0 };
const TAB_ACTIVE_UNDERLINE: Color = Color { r: 0.35, g: 0.56, b: 0.96, a: 1.0 };
const TEXT_DEFAULT: Color = Color { r: 0.85, g: 0.85, b: 0.85, a: 1.0 };
const TEXT_LOG: Color = Color { r: 0.85, g: 0.85, b: 0.85, a: 1.0 };
const TEXT_WARN: Color = Color { r: 0.95, g: 0.80, b: 0.20, a: 1.0 };
const TEXT_ERROR: Color = Color { r: 0.95, g: 0.30, b: 0.30, a: 1.0 };
const TEXT_INFO: Color = Color { r: 0.40, g: 0.70, b: 0.95, a: 1.0 };
const TEXT_DEBUG: Color = Color { r: 0.60, g: 0.60, b: 0.60, a: 1.0 };
const TEXT_MUTED: Color = Color { r: 0.50, g: 0.50, b: 0.50, a: 1.0 };
const BORDER_COLOR: Color = Color { r: 0.30, g: 0.30, b: 0.30, a: 1.0 };
const RESIZE_HANDLE_COLOR: Color = Color { r: 0.35, g: 0.35, b: 0.35, a: 1.0 };
const CLEAR_BTN_BG: Color = Color { r: 0.25, g: 0.25, b: 0.25, a: 1.0 };
const SELECTED_BG: Color = Color { r: 0.22, g: 0.33, b: 0.50, a: 1.0 };
const TAG_COLOR: Color = Color { r: 0.55, g: 0.40, b: 0.85, a: 1.0 };
const ATTR_NAME_COLOR: Color = Color { r: 0.80, g: 0.55, b: 0.30, a: 1.0 };
const ATTR_VALUE_COLOR: Color = Color { r: 0.55, g: 0.75, b: 0.40, a: 1.0 };
const STATUS_2XX: Color = Color { r: 0.30, g: 0.80, b: 0.30, a: 1.0 };
const STATUS_3XX: Color = Color { r: 0.95, g: 0.80, b: 0.20, a: 1.0 };
const STATUS_4XX: Color = Color { r: 0.95, g: 0.30, b: 0.30, a: 1.0 };

// ── Data Types ───────────────────────────────────────────────────────────────

/// The active DevTools tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevToolsTab {
    /// Console output panel.
    Console,
    /// DOM elements inspector.
    Elements,
    /// Network requests panel.
    Network,
}

/// Log level for console messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Standard log output.
    Log,
    /// Warning message.
    Warn,
    /// Error message.
    Error,
    /// Informational message.
    Info,
    /// Debug message.
    Debug,
}

/// A console message captured from the JS engine.
#[derive(Debug, Clone)]
pub struct ConsoleMessage {
    /// Severity level of the message.
    pub level: LogLevel,
    /// The message text.
    pub text: String,
    /// When the message was captured.
    pub timestamp: Instant,
}

/// A network request tracked by the browser.
#[derive(Debug, Clone)]
pub struct NetworkRequest {
    /// The requested URL.
    pub url: String,
    /// HTTP method (GET, POST, etc.).
    pub method: String,
    /// HTTP status code (0 if pending/failed).
    pub status: u16,
    /// Content-Type of the response.
    pub content_type: String,
    /// Size of the response body in bytes.
    pub size: usize,
    /// Duration of the request in milliseconds.
    pub duration_ms: u64,
}

/// A flattened DOM tree node for display in the Elements panel.
#[derive(Debug, Clone)]
struct DomTreeEntry {
    /// Indentation depth (0 = root).
    depth: usize,
    /// The tag name (e.g., "div", "span", "#text").
    tag: String,
    /// The `id` attribute, if any.
    id: Option<String>,
    /// The `class` attribute, if any.
    classes: Option<String>,
    /// Text content for text nodes (truncated).
    text: Option<String>,
    /// Attributes for display.
    attributes: Vec<(String, String)>,
    /// Whether this node is expanded (has visible children).
    expanded: bool,
    /// Whether this node has children at all.
    has_children: bool,
    /// Index of this entry's first child in the flat list (for collapse/expand).
    child_count: usize,
}

/// Resource type filter for the Network tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkFilter {
    /// Show all requests.
    All,
    /// XHR / Fetch requests.
    Xhr,
    /// CSS stylesheets.
    Css,
    /// JavaScript files.
    Js,
    /// Images.
    Image,
    /// Other resource types.
    Other,
}

/// Column to sort the network table by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSortColumn {
    /// Sort by URL.
    Url,
    /// Sort by HTTP method.
    Method,
    /// Sort by status code.
    Status,
    /// Sort by content type.
    Type,
    /// Sort by response size.
    Size,
    /// Sort by request duration.
    Time,
}

// ── DevTools State ───────────────────────────────────────────────────────────

/// The DevTools panel state.
pub struct DevTools {
    /// Whether DevTools is visible.
    pub visible: bool,
    /// The currently active tab.
    pub active_tab: DevToolsTab,
    /// Console messages from JS execution.
    pub console_messages: Vec<ConsoleMessage>,
    /// Tracked network requests.
    pub network_requests: Vec<NetworkRequest>,
    /// Height of the DevTools panel in pixels.
    pub panel_height: f32,
    /// Scroll offset for the console view.
    console_scroll: f32,
    /// Scroll offset for the elements view.
    elements_scroll: f32,
    /// Scroll offset for the network view.
    network_scroll: f32,
    /// Currently selected element index in the Elements panel.
    selected_element: Option<usize>,
    /// Flattened DOM tree for the Elements panel.
    dom_entries: Vec<DomTreeEntry>,
    /// Network filter.
    network_filter: NetworkFilter,
    /// Network sort column.
    network_sort: NetworkSortColumn,
    /// Whether sort is ascending.
    network_sort_asc: bool,
    /// Whether the user is currently dragging the resize handle.
    pub resizing: bool,
}

impl DevTools {
    /// Create a new DevTools instance (hidden by default).
    pub fn new() -> Self {
        Self {
            visible: false,
            active_tab: DevToolsTab::Console,
            console_messages: Vec::new(),
            network_requests: Vec::new(),
            panel_height: 250.0,
            console_scroll: 0.0,
            elements_scroll: 0.0,
            network_scroll: 0.0,
            selected_element: None,
            dom_entries: Vec::new(),
            network_filter: NetworkFilter::All,
            network_sort: NetworkSortColumn::Url,
            network_sort_asc: true,
            resizing: false,
        }
    }

    /// Toggle DevTools visibility.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    /// Clear all console messages.
    pub fn clear_console(&mut self) {
        self.console_messages.clear();
        self.console_scroll = 0.0;
    }

    /// Add a console message.
    pub fn add_console_message(&mut self, level: LogLevel, text: String) {
        self.console_messages.push(ConsoleMessage {
            level,
            text,
            timestamp: Instant::now(),
        });
    }

    /// Parse a raw console line (e.g., "[LOG] hello") into a typed message.
    pub fn add_raw_console_line(&mut self, line: &str) {
        let (level, text) = if let Some(rest) = line.strip_prefix("[LOG] ") {
            (LogLevel::Log, rest.to_string())
        } else if let Some(rest) = line.strip_prefix("[WARN] ") {
            (LogLevel::Warn, rest.to_string())
        } else if let Some(rest) = line.strip_prefix("[ERROR] ") {
            (LogLevel::Error, rest.to_string())
        } else if let Some(rest) = line.strip_prefix("[INFO] ") {
            (LogLevel::Info, rest.to_string())
        } else if let Some(rest) = line.strip_prefix("[DEBUG] ") {
            (LogLevel::Debug, rest.to_string())
        } else {
            (LogLevel::Log, line.to_string())
        };
        self.add_console_message(level, text);
    }

    /// Add a network request.
    pub fn add_network_request(&mut self, request: NetworkRequest) {
        self.network_requests.push(request);
    }

    /// Update the DOM tree from a `DomNode`.
    pub fn update_dom(&mut self, dom: &DomNode) {
        self.dom_entries.clear();
        self.flatten_dom(dom, 0);
    }

    /// Flatten a DOM tree into displayable entries.
    fn flatten_dom(&mut self, node: &DomNode, depth: usize) {
        match node {
            DomNode::Document { children } => {
                let idx = self.dom_entries.len();
                self.dom_entries.push(DomTreeEntry {
                    depth,
                    tag: "#document".into(),
                    id: None,
                    classes: None,
                    text: None,
                    attributes: Vec::new(),
                    expanded: true,
                    has_children: !children.is_empty(),
                    child_count: 0,
                });
                for child in children {
                    self.flatten_dom(child, depth + 1);
                }
                let count = self.dom_entries.len() - idx - 1;
                self.dom_entries[idx].child_count = count;
            }
            DomNode::Element { tag, attributes, children } => {
                let id = attributes.iter()
                    .find(|(k, _)| k == "id")
                    .map(|(_, v)| v.clone());
                let classes = attributes.iter()
                    .find(|(k, _)| k == "class")
                    .map(|(_, v)| v.clone());
                // Filter out id and class from displayed attributes.
                let display_attrs: Vec<(String, String)> = attributes.iter()
                    .filter(|(k, _)| k != "id" && k != "class")
                    .cloned()
                    .collect();

                let idx = self.dom_entries.len();
                self.dom_entries.push(DomTreeEntry {
                    depth,
                    tag: tag.clone(),
                    id,
                    classes,
                    text: None,
                    attributes: display_attrs,
                    expanded: depth < 3, // Auto-expand first 3 levels.
                    has_children: !children.is_empty(),
                    child_count: 0,
                });
                for child in children {
                    self.flatten_dom(child, depth + 1);
                }
                let count = self.dom_entries.len() - idx - 1;
                self.dom_entries[idx].child_count = count;
            }
            DomNode::Text(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let display = if trimmed.len() > 60 {
                        format!("{}...", &trimmed[..57])
                    } else {
                        trimmed.to_string()
                    };
                    self.dom_entries.push(DomTreeEntry {
                        depth,
                        tag: "#text".into(),
                        id: None,
                        classes: None,
                        text: Some(display),
                        attributes: Vec::new(),
                        expanded: false,
                        has_children: false,
                        child_count: 0,
                    });
                }
            }
            DomNode::Comment(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let display = if trimmed.len() > 60 {
                        format!("{}...", &trimmed[..57])
                    } else {
                        trimmed.to_string()
                    };
                    self.dom_entries.push(DomTreeEntry {
                        depth,
                        tag: "#comment".into(),
                        id: None,
                        classes: None,
                        text: Some(display),
                        attributes: Vec::new(),
                        expanded: false,
                        has_children: false,
                        child_count: 0,
                    });
                }
            }
        }
    }

    /// Toggle expand/collapse of a DOM tree entry.
    pub fn toggle_element(&mut self, index: usize) {
        if let Some(entry) = self.dom_entries.get_mut(index) {
            if entry.has_children {
                entry.expanded = !entry.expanded;
            }
        }
    }

    /// Clamp the DevTools panel height to valid range.
    pub fn clamp_height(&mut self, window_height: f32) {
        let max_h = window_height * MAX_DEVTOOLS_FRACTION;
        self.panel_height = self.panel_height.clamp(MIN_DEVTOOLS_HEIGHT, max_h);
    }

    /// Scroll the active panel by a delta (positive = scroll down).
    pub fn scroll(&mut self, delta: f32) {
        match self.active_tab {
            DevToolsTab::Console => {
                self.console_scroll = (self.console_scroll + delta).max(0.0);
            }
            DevToolsTab::Elements => {
                self.elements_scroll = (self.elements_scroll + delta).max(0.0);
            }
            DevToolsTab::Network => {
                self.network_scroll = (self.network_scroll + delta).max(0.0);
            }
        }
    }

    /// Handle a click at the given position within the DevTools panel.
    ///
    /// `x` and `y` are in DevTools-local coordinates (0,0 = top-left of DevTools).
    /// Returns `true` if the click was consumed.
    pub fn handle_click(&mut self, x: f32, y: f32) -> bool {
        // Check resize handle.
        if y < RESIZE_HANDLE_HEIGHT {
            self.resizing = true;
            return true;
        }

        // Check tab bar clicks.
        if y >= RESIZE_HANDLE_HEIGHT && y < RESIZE_HANDLE_HEIGHT + TAB_BAR_HEIGHT {
            let tab_width = 100.0;
            let tab_idx = (x / tab_width) as usize;
            match tab_idx {
                0 => self.active_tab = DevToolsTab::Console,
                1 => self.active_tab = DevToolsTab::Elements,
                2 => self.active_tab = DevToolsTab::Network,
                _ => {}
            }
            return true;
        }

        let content_y = y - RESIZE_HANDLE_HEIGHT - TAB_BAR_HEIGHT;

        match self.active_tab {
            DevToolsTab::Console => {
                // Check "Clear" button (top-right corner of content area).
                let clear_btn_x = 8.0;
                let clear_btn_y = 0.0;
                let clear_btn_w = 50.0;
                let clear_btn_h = 20.0;
                if x >= clear_btn_x
                    && x <= clear_btn_x + clear_btn_w
                    && content_y >= clear_btn_y
                    && content_y <= clear_btn_y + clear_btn_h
                {
                    self.clear_console();
                    return true;
                }
            }
            DevToolsTab::Elements => {
                // Click on an element entry to select it.
                let entry_y = content_y + self.elements_scroll;
                let entry_idx = (entry_y / DEVTOOLS_LINE_HEIGHT) as usize;
                let visible = self.visible_dom_entries();
                if entry_idx < visible.len() {
                    let actual_idx = visible[entry_idx];
                    if self.selected_element == Some(actual_idx) {
                        self.toggle_element(actual_idx);
                    } else {
                        self.selected_element = Some(actual_idx);
                    }
                    return true;
                }
            }
            DevToolsTab::Network => {
                // Check filter bar.
                if content_y < 22.0 {
                    let filters = [
                        NetworkFilter::All,
                        NetworkFilter::Xhr,
                        NetworkFilter::Css,
                        NetworkFilter::Js,
                        NetworkFilter::Image,
                        NetworkFilter::Other,
                    ];
                    let filter_idx = (x / 60.0) as usize;
                    if filter_idx < filters.len() {
                        self.network_filter = filters[filter_idx];
                    }
                    return true;
                }
                // Check column header click for sorting.
                if content_y >= 22.0 && content_y < 22.0 + DEVTOOLS_LINE_HEIGHT {
                    let col = self.column_at_x(x);
                    if self.network_sort == col {
                        self.network_sort_asc = !self.network_sort_asc;
                    } else {
                        self.network_sort = col;
                        self.network_sort_asc = true;
                    }
                    return true;
                }
            }
        }
        false
    }

    /// Determine which network column a click x-coordinate falls in.
    fn column_at_x(&self, x: f32) -> NetworkSortColumn {
        // Column layout (approximate):
        // URL: 0..400, Method: 400..460, Status: 460..520,
        // Type: 520..620, Size: 620..700, Time: 700+
        if x < 400.0 {
            NetworkSortColumn::Url
        } else if x < 460.0 {
            NetworkSortColumn::Method
        } else if x < 520.0 {
            NetworkSortColumn::Status
        } else if x < 620.0 {
            NetworkSortColumn::Type
        } else if x < 700.0 {
            NetworkSortColumn::Size
        } else {
            NetworkSortColumn::Time
        }
    }

    /// Get the indices of visible DOM entries (respecting collapsed parents).
    fn visible_dom_entries(&self) -> Vec<usize> {
        let mut visible = Vec::new();
        let mut i = 0;
        while i < self.dom_entries.len() {
            visible.push(i);
            let entry = &self.dom_entries[i];
            if !entry.expanded && entry.has_children {
                // Skip all children.
                i += entry.child_count + 1;
            } else {
                i += 1;
            }
        }
        visible
    }

    /// Get the filtered and sorted network requests.
    fn filtered_network_requests(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.network_requests.len())
            .filter(|&i| {
                let req = &self.network_requests[i];
                match self.network_filter {
                    NetworkFilter::All => true,
                    NetworkFilter::Xhr => req.content_type.contains("json")
                        || req.content_type.contains("xml"),
                    NetworkFilter::Css => req.content_type.contains("css")
                        || req.url.ends_with(".css"),
                    NetworkFilter::Js => req.content_type.contains("javascript")
                        || req.url.ends_with(".js"),
                    NetworkFilter::Image => req.content_type.starts_with("image/")
                        || req.url.ends_with(".png")
                        || req.url.ends_with(".jpg")
                        || req.url.ends_with(".gif")
                        || req.url.ends_with(".webp")
                        || req.url.ends_with(".svg"),
                    NetworkFilter::Other => {
                        !req.content_type.contains("json")
                            && !req.content_type.contains("xml")
                            && !req.content_type.contains("css")
                            && !req.content_type.contains("javascript")
                            && !req.content_type.starts_with("image/")
                    }
                }
            })
            .collect();

        indices.sort_by(|&a, &b| {
            let req_a = &self.network_requests[a];
            let req_b = &self.network_requests[b];
            let cmp = match self.network_sort {
                NetworkSortColumn::Url => req_a.url.cmp(&req_b.url),
                NetworkSortColumn::Method => req_a.method.cmp(&req_b.method),
                NetworkSortColumn::Status => req_a.status.cmp(&req_b.status),
                NetworkSortColumn::Type => req_a.content_type.cmp(&req_b.content_type),
                NetworkSortColumn::Size => req_a.size.cmp(&req_b.size),
                NetworkSortColumn::Time => req_a.duration_ms.cmp(&req_b.duration_ms),
            };
            if self.network_sort_asc { cmp } else { cmp.reverse() }
        });

        indices
    }

    // ── Rendering ────────────────────────────────────────────────────────────

    /// Render the DevTools panel into the framebuffer.
    ///
    /// `fb` is the framebuffer to draw into.
    /// `panel_top` is the y coordinate where the DevTools panel starts.
    /// `panel_width` is the width of the panel.
    pub fn render(&self, fb: &mut Framebuffer, panel_top: f32, panel_width: f32) {
        if !self.visible {
            return;
        }

        let panel_h = self.panel_height;

        // Draw resize handle.
        fb.fill_rect(0.0, panel_top, panel_width, RESIZE_HANDLE_HEIGHT, RESIZE_HANDLE_COLOR);
        // Draw a small grip indicator.
        let grip_y = panel_top + 1.0;
        let grip_w = 30.0;
        let grip_x = (panel_width - grip_w) / 2.0;
        fb.fill_rect(grip_x, grip_y, grip_w, 2.0, TEXT_MUTED);

        // Draw background.
        let bg_top = panel_top + RESIZE_HANDLE_HEIGHT;
        fb.fill_rect(0.0, bg_top, panel_width, panel_h - RESIZE_HANDLE_HEIGHT, BG_COLOR);

        // Draw top border.
        fb.fill_rect(0.0, bg_top, panel_width, 1.0, BORDER_COLOR);

        // Draw tab bar.
        let tab_y = bg_top + 1.0;
        self.render_tab_bar(fb, tab_y, panel_width);

        // Draw content area.
        let content_top = tab_y + TAB_BAR_HEIGHT;
        let content_height = panel_h - RESIZE_HANDLE_HEIGHT - TAB_BAR_HEIGHT - 1.0;

        match self.active_tab {
            DevToolsTab::Console => {
                self.render_console(fb, content_top, panel_width, content_height);
            }
            DevToolsTab::Elements => {
                self.render_elements(fb, content_top, panel_width, content_height);
            }
            DevToolsTab::Network => {
                self.render_network(fb, content_top, panel_width, content_height);
            }
        }
    }

    /// Render the tab bar.
    fn render_tab_bar(&self, fb: &mut Framebuffer, y: f32, width: f32) {
        fb.fill_rect(0.0, y, width, TAB_BAR_HEIGHT, TAB_BG);

        let tabs = [
            (DevToolsTab::Console, "Console"),
            (DevToolsTab::Elements, "Elements"),
            (DevToolsTab::Network, "Network"),
        ];

        let tab_width = 100.0;
        for (i, (tab, label)) in tabs.iter().enumerate() {
            let tx = i as f32 * tab_width;
            let is_active = self.active_tab == *tab;

            if is_active {
                fb.fill_rect(tx, y, tab_width, TAB_BAR_HEIGHT, TAB_ACTIVE_BG);
                // Active tab underline.
                fb.fill_rect(tx, y + TAB_BAR_HEIGHT - 2.0, tab_width, 2.0, TAB_ACTIVE_UNDERLINE);
            }

            let text_x = tx + 10.0;
            let text_y = y + (TAB_BAR_HEIGHT - DEVTOOLS_FONT_SIZE) / 2.0;
            fb.draw_text(text_x, text_y, label, DEVTOOLS_FONT_SIZE, TAB_TEXT, None, None, None);
        }

        // Draw bottom border of tab bar.
        fb.fill_rect(0.0, y + TAB_BAR_HEIGHT - 1.0, width, 1.0, BORDER_COLOR);
    }

    /// Render the Console tab content.
    fn render_console(
        &self,
        fb: &mut Framebuffer,
        top: f32,
        width: f32,
        height: f32,
    ) {
        // "Clear" button.
        let btn_x = DEVTOOLS_PADDING;
        let btn_y = top + 4.0;
        let btn_w = 50.0;
        let btn_h = 18.0;
        fb.fill_rect(btn_x, btn_y, btn_w, btn_h, CLEAR_BTN_BG);
        fb.stroke_rect(btn_x, btn_y, btn_w, btn_h, BORDER_COLOR, 1.0);
        fb.draw_text(
            btn_x + 8.0,
            btn_y + 3.0,
            "Clear",
            DEVTOOLS_FONT_SIZE,
            TAB_TEXT,
            None,
            None,
            None,
        );

        // Message count indicator.
        let count_text = format!("{} messages", self.console_messages.len());
        fb.draw_text(
            btn_x + btn_w + 12.0,
            btn_y + 3.0,
            &count_text,
            DEVTOOLS_FONT_SIZE,
            TEXT_MUTED,
            None,
            None,
            None,
        );

        // Console messages.
        let messages_top = top + 26.0;
        let messages_height = height - 26.0;
        let max_lines = (messages_height / DEVTOOLS_LINE_HEIGHT) as usize;

        // Auto-scroll to bottom.
        let total_lines = self.console_messages.len();
        let start = if total_lines > max_lines {
            total_lines - max_lines
        } else {
            0
        };

        for (i, msg) in self.console_messages.iter().skip(start).enumerate() {
            let line_y = messages_top + i as f32 * DEVTOOLS_LINE_HEIGHT;
            if line_y + DEVTOOLS_LINE_HEIGHT > top + height {
                break;
            }

            // Level prefix and color.
            let (prefix, color) = match msg.level {
                LogLevel::Log => ("", TEXT_LOG),
                LogLevel::Warn => ("WARN ", TEXT_WARN),
                LogLevel::Error => ("ERR  ", TEXT_ERROR),
                LogLevel::Info => ("INFO ", TEXT_INFO),
                LogLevel::Debug => ("DBG  ", TEXT_DEBUG),
            };

            // Draw level-colored background for warnings and errors.
            match msg.level {
                LogLevel::Warn => {
                    let bg = Color { r: 0.30, g: 0.28, b: 0.10, a: 1.0 };
                    fb.fill_rect(0.0, line_y, width, DEVTOOLS_LINE_HEIGHT, bg);
                }
                LogLevel::Error => {
                    let bg = Color { r: 0.30, g: 0.12, b: 0.12, a: 1.0 };
                    fb.fill_rect(0.0, line_y, width, DEVTOOLS_LINE_HEIGHT, bg);
                }
                _ => {}
            }

            let text = format!("{prefix}{}", msg.text);
            fb.draw_text(
                DEVTOOLS_PADDING,
                line_y + 3.0,
                &text,
                DEVTOOLS_FONT_SIZE,
                color,
                None,
                None,
                None,
            );

            // Separator line.
            let sep_color = Color { r: 0.22, g: 0.22, b: 0.22, a: 1.0 };
            fb.fill_rect(0.0, line_y + DEVTOOLS_LINE_HEIGHT - 1.0, width, 1.0, sep_color);
        }
    }

    /// Render the Elements tab content.
    fn render_elements(
        &self,
        fb: &mut Framebuffer,
        top: f32,
        width: f32,
        height: f32,
    ) {
        let visible = self.visible_dom_entries();
        let max_lines = (height / DEVTOOLS_LINE_HEIGHT) as usize;
        let start_offset = (self.elements_scroll / DEVTOOLS_LINE_HEIGHT) as usize;

        for (i, &entry_idx) in visible.iter().skip(start_offset).enumerate() {
            if i >= max_lines {
                break;
            }
            let entry = &self.dom_entries[entry_idx];
            let line_y = top + i as f32 * DEVTOOLS_LINE_HEIGHT;

            // Selected highlight.
            if self.selected_element == Some(entry_idx) {
                fb.fill_rect(0.0, line_y, width, DEVTOOLS_LINE_HEIGHT, SELECTED_BG);
            }

            let indent = DEVTOOLS_PADDING + entry.depth as f32 * TREE_INDENT;

            // Expand/collapse indicator.
            if entry.has_children {
                let arrow = if entry.expanded { "v" } else { ">" };
                fb.draw_text(
                    indent - 12.0,
                    line_y + 3.0,
                    arrow,
                    DEVTOOLS_FONT_SIZE,
                    TEXT_MUTED,
                    None,
                    None,
                    None,
                );
            }

            if entry.tag == "#text" {
                // Text node — show in quotes.
                let text = format!("\"{}\"", entry.text.as_deref().unwrap_or(""));
                fb.draw_text(
                    indent,
                    line_y + 3.0,
                    &text,
                    DEVTOOLS_FONT_SIZE,
                    TEXT_MUTED,
                    None,
                    None,
                    None,
                );
            } else if entry.tag == "#comment" {
                let text = format!("<!-- {} -->", entry.text.as_deref().unwrap_or(""));
                fb.draw_text(
                    indent,
                    line_y + 3.0,
                    &text,
                    DEVTOOLS_FONT_SIZE,
                    TEXT_MUTED,
                    None,
                    None,
                    None,
                );
            } else {
                // Element node: <tag id="..." class="...">
                let mut x_pos = indent;

                // Opening bracket and tag name.
                fb.draw_text(x_pos, line_y + 3.0, "<", DEVTOOLS_FONT_SIZE, TEXT_DEFAULT, None, None, None);
                x_pos += DEVTOOLS_FONT_SIZE * 0.6;

                fb.draw_text(x_pos, line_y + 3.0, &entry.tag, DEVTOOLS_FONT_SIZE, TAG_COLOR, None, None, None);
                x_pos += entry.tag.len() as f32 * DEVTOOLS_FONT_SIZE * 0.6;

                // id attribute.
                if let Some(ref id) = entry.id {
                    fb.draw_text(x_pos, line_y + 3.0, " id", DEVTOOLS_FONT_SIZE, ATTR_NAME_COLOR, None, None, None);
                    x_pos += 3.0 * DEVTOOLS_FONT_SIZE * 0.6;
                    fb.draw_text(x_pos, line_y + 3.0, "=\"", DEVTOOLS_FONT_SIZE, TEXT_DEFAULT, None, None, None);
                    x_pos += 2.0 * DEVTOOLS_FONT_SIZE * 0.6;
                    fb.draw_text(x_pos, line_y + 3.0, id, DEVTOOLS_FONT_SIZE, ATTR_VALUE_COLOR, None, None, None);
                    x_pos += id.len() as f32 * DEVTOOLS_FONT_SIZE * 0.6;
                    fb.draw_text(x_pos, line_y + 3.0, "\"", DEVTOOLS_FONT_SIZE, TEXT_DEFAULT, None, None, None);
                    x_pos += DEVTOOLS_FONT_SIZE * 0.6;
                }

                // class attribute.
                if let Some(ref classes) = entry.classes {
                    fb.draw_text(x_pos, line_y + 3.0, " class", DEVTOOLS_FONT_SIZE, ATTR_NAME_COLOR, None, None, None);
                    x_pos += 6.0 * DEVTOOLS_FONT_SIZE * 0.6;
                    fb.draw_text(x_pos, line_y + 3.0, "=\"", DEVTOOLS_FONT_SIZE, TEXT_DEFAULT, None, None, None);
                    x_pos += 2.0 * DEVTOOLS_FONT_SIZE * 0.6;
                    fb.draw_text(x_pos, line_y + 3.0, classes, DEVTOOLS_FONT_SIZE, ATTR_VALUE_COLOR, None, None, None);
                    x_pos += classes.len() as f32 * DEVTOOLS_FONT_SIZE * 0.6;
                    fb.draw_text(x_pos, line_y + 3.0, "\"", DEVTOOLS_FONT_SIZE, TEXT_DEFAULT, None, None, None);
                    x_pos += DEVTOOLS_FONT_SIZE * 0.6;
                }

                fb.draw_text(x_pos, line_y + 3.0, ">", DEVTOOLS_FONT_SIZE, TEXT_DEFAULT, None, None, None);
            }
        }

        // If an element is selected, show basic info at the bottom.
        if let Some(sel_idx) = self.selected_element {
            if let Some(entry) = self.dom_entries.get(sel_idx) {
                if entry.tag != "#text" && entry.tag != "#comment" {
                    let info_y = top + height - 40.0;
                    fb.fill_rect(0.0, info_y, width, 40.0, TAB_BG);
                    fb.fill_rect(0.0, info_y, width, 1.0, BORDER_COLOR);

                    let mut info = format!("<{}", entry.tag);
                    if let Some(ref id) = entry.id {
                        info.push_str(&format!(" id=\"{id}\""));
                    }
                    if let Some(ref classes) = entry.classes {
                        info.push_str(&format!(" class=\"{classes}\""));
                    }
                    for (k, v) in &entry.attributes {
                        let short_v = if v.len() > 30 {
                            format!("{}...", &v[..27])
                        } else {
                            v.clone()
                        };
                        info.push_str(&format!(" {k}=\"{short_v}\""));
                    }
                    info.push('>');

                    fb.draw_text(
                        DEVTOOLS_PADDING,
                        info_y + 6.0,
                        &info,
                        DEVTOOLS_FONT_SIZE,
                        TEXT_DEFAULT,
                        None,
                        None,
                        None,
                    );

                    let children_info = format!(
                        "{} child node(s)",
                        entry.child_count
                    );
                    fb.draw_text(
                        DEVTOOLS_PADDING,
                        info_y + 22.0,
                        &children_info,
                        DEVTOOLS_FONT_SIZE,
                        TEXT_MUTED,
                        None,
                        None,
                        None,
                    );
                }
            }
        }
    }

    /// Render the Network tab content.
    fn render_network(
        &self,
        fb: &mut Framebuffer,
        top: f32,
        width: f32,
        height: f32,
    ) {
        // Filter bar.
        let filters = [
            (NetworkFilter::All, "All"),
            (NetworkFilter::Xhr, "XHR"),
            (NetworkFilter::Css, "CSS"),
            (NetworkFilter::Js, "JS"),
            (NetworkFilter::Image, "Img"),
            (NetworkFilter::Other, "Other"),
        ];

        let filter_y = top + 2.0;
        for (i, (filter, label)) in filters.iter().enumerate() {
            let fx = i as f32 * 60.0 + DEVTOOLS_PADDING;
            let is_active = self.network_filter == *filter;
            let color = if is_active { TAB_ACTIVE_UNDERLINE } else { TEXT_MUTED };
            fb.draw_text(fx, filter_y, label, DEVTOOLS_FONT_SIZE, color, None, None, None);
            if is_active {
                fb.fill_rect(fx, filter_y + DEVTOOLS_FONT_SIZE + 2.0, 40.0, 2.0, TAB_ACTIVE_UNDERLINE);
            }
        }

        // Request count.
        let filtered = self.filtered_network_requests();
        let count_text = format!("{} requests", filtered.len());
        fb.draw_text(
            width - 120.0,
            filter_y,
            &count_text,
            DEVTOOLS_FONT_SIZE,
            TEXT_MUTED,
            None,
            None,
            None,
        );

        // Column headers.
        let header_y = top + 22.0;
        fb.fill_rect(0.0, header_y, width, DEVTOOLS_LINE_HEIGHT, TAB_BG);

        let headers = [
            (DEVTOOLS_PADDING, "URL"),
            (400.0, "Method"),
            (460.0, "Status"),
            (520.0, "Type"),
            (620.0, "Size"),
            (700.0, "Time"),
        ];

        for (hx, label) in &headers {
            fb.draw_text(*hx, header_y + 3.0, label, DEVTOOLS_FONT_SIZE, TAB_TEXT, None, None, None);
        }

        // Sort indicator.
        let sort_col_x = match self.network_sort {
            NetworkSortColumn::Url => DEVTOOLS_PADDING,
            NetworkSortColumn::Method => 400.0,
            NetworkSortColumn::Status => 460.0,
            NetworkSortColumn::Type => 520.0,
            NetworkSortColumn::Size => 620.0,
            NetworkSortColumn::Time => 700.0,
        };
        let arrow = if self.network_sort_asc { " ^" } else { " v" };
        // Find the header label length to position the arrow.
        let header_label = match self.network_sort {
            NetworkSortColumn::Url => "URL",
            NetworkSortColumn::Method => "Method",
            NetworkSortColumn::Status => "Status",
            NetworkSortColumn::Type => "Type",
            NetworkSortColumn::Size => "Size",
            NetworkSortColumn::Time => "Time",
        };
        let arrow_x = sort_col_x + header_label.len() as f32 * DEVTOOLS_FONT_SIZE * 0.6;
        fb.draw_text(arrow_x, header_y + 3.0, arrow, DEVTOOLS_FONT_SIZE, TAB_ACTIVE_UNDERLINE, None, None, None);

        fb.fill_rect(0.0, header_y + DEVTOOLS_LINE_HEIGHT - 1.0, width, 1.0, BORDER_COLOR);

        // Request rows.
        let rows_top = header_y + DEVTOOLS_LINE_HEIGHT;
        let rows_height = height - 22.0 - DEVTOOLS_LINE_HEIGHT;
        let max_rows = (rows_height / DEVTOOLS_LINE_HEIGHT) as usize;

        for (i, &req_idx) in filtered.iter().enumerate() {
            if i >= max_rows {
                break;
            }
            let req = &self.network_requests[req_idx];
            let row_y = rows_top + i as f32 * DEVTOOLS_LINE_HEIGHT;

            // Alternating row background.
            if i % 2 == 1 {
                let alt_bg = Color { r: 0.17, g: 0.17, b: 0.17, a: 1.0 };
                fb.fill_rect(0.0, row_y, width, DEVTOOLS_LINE_HEIGHT, alt_bg);
            }

            // URL (truncate to fit).
            let max_url_chars = 55;
            let url_display = if req.url.len() > max_url_chars {
                format!("{}...", &req.url[..max_url_chars - 3])
            } else {
                req.url.clone()
            };
            fb.draw_text(DEVTOOLS_PADDING, row_y + 3.0, &url_display, DEVTOOLS_FONT_SIZE, TEXT_DEFAULT, None, None, None);

            // Method.
            fb.draw_text(400.0, row_y + 3.0, &req.method, DEVTOOLS_FONT_SIZE, TEXT_MUTED, None, None, None);

            // Status (color coded).
            let status_color = if req.status == 0 {
                TEXT_MUTED
            } else if req.status < 300 {
                STATUS_2XX
            } else if req.status < 400 {
                STATUS_3XX
            } else {
                STATUS_4XX
            };
            let status_text = if req.status == 0 {
                "(pending)".to_string()
            } else {
                req.status.to_string()
            };
            fb.draw_text(460.0, row_y + 3.0, &status_text, DEVTOOLS_FONT_SIZE, status_color, None, None, None);

            // Content-Type (short form).
            let type_display = short_content_type(&req.content_type);
            fb.draw_text(520.0, row_y + 3.0, &type_display, DEVTOOLS_FONT_SIZE, TEXT_MUTED, None, None, None);

            // Size.
            let size_display = format_size(req.size);
            fb.draw_text(620.0, row_y + 3.0, &size_display, DEVTOOLS_FONT_SIZE, TEXT_MUTED, None, None, None);

            // Time.
            let time_display = format_duration(req.duration_ms);
            fb.draw_text(700.0, row_y + 3.0, &time_display, DEVTOOLS_FONT_SIZE, TEXT_MUTED, None, None, None);
        }

        // Empty state.
        if filtered.is_empty() {
            let empty_y = rows_top + 20.0;
            fb.draw_text(
                DEVTOOLS_PADDING,
                empty_y,
                "No network requests recorded.",
                DEVTOOLS_FONT_SIZE,
                TEXT_MUTED,
                None,
                None,
                None,
            );
        }
    }

    /// Get the overlay rectangle for the hovered/selected element.
    ///
    /// Returns `(x, y, width, height)` in page coordinates if an element
    /// is selected in the Elements panel, or `None`.
    pub fn element_highlight_rect(&self) -> Option<(f32, f32, f32, f32)> {
        // In a full implementation, this would return the actual layout box
        // coordinates of the selected element. For now, return None since we
        // don't have a mapping from DOM entries to layout boxes yet.
        None
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Shorten a Content-Type string for display.
fn short_content_type(ct: &str) -> String {
    if ct.is_empty() {
        return "-".to_string();
    }
    // Take just the main type.
    let base = ct.split(';').next().unwrap_or(ct).trim();
    match base {
        "text/html" => "html".into(),
        "text/css" => "css".into(),
        "text/javascript" | "application/javascript" => "js".into(),
        "application/json" => "json".into(),
        "application/xml" | "text/xml" => "xml".into(),
        "image/png" => "png".into(),
        "image/jpeg" | "image/jpg" => "jpeg".into(),
        "image/gif" => "gif".into(),
        "image/webp" => "webp".into(),
        "image/svg+xml" => "svg".into(),
        "font/woff" => "woff".into(),
        "font/woff2" => "woff2".into(),
        _ => {
            // Just show the subtype.
            if let Some(sub) = base.split('/').nth(1) {
                sub.to_string()
            } else {
                base.to_string()
            }
        }
    }
}

/// Format a byte size for display (e.g., "1.2 KB", "340 B").
fn format_size(bytes: usize) -> String {
    if bytes == 0 {
        return "-".to_string();
    } else if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Format a duration in milliseconds for display.
fn format_duration(ms: u64) -> String {
    if ms == 0 {
        "-".to_string()
    } else if ms < 1000 {
        format!("{} ms", ms)
    } else {
        format!("{:.2} s", ms as f64 / 1000.0)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devtools_toggle() {
        let mut dt = DevTools::new();
        assert!(!dt.visible);
        dt.toggle();
        assert!(dt.visible);
        dt.toggle();
        assert!(!dt.visible);
    }

    #[test]
    fn console_message_add_and_clear() {
        let mut dt = DevTools::new();
        dt.add_console_message(LogLevel::Log, "hello".into());
        dt.add_console_message(LogLevel::Warn, "careful".into());
        dt.add_console_message(LogLevel::Error, "oops".into());
        assert_eq!(dt.console_messages.len(), 3);

        dt.clear_console();
        assert!(dt.console_messages.is_empty());
    }

    #[test]
    fn raw_console_line_parsing() {
        let mut dt = DevTools::new();
        dt.add_raw_console_line("[LOG] hello");
        dt.add_raw_console_line("[WARN] be careful");
        dt.add_raw_console_line("[ERROR] bad thing");
        dt.add_raw_console_line("no prefix");

        assert_eq!(dt.console_messages.len(), 4);
        assert_eq!(dt.console_messages[0].level, LogLevel::Log);
        assert_eq!(dt.console_messages[0].text, "hello");
        assert_eq!(dt.console_messages[1].level, LogLevel::Warn);
        assert_eq!(dt.console_messages[2].level, LogLevel::Error);
        assert_eq!(dt.console_messages[3].level, LogLevel::Log);
        assert_eq!(dt.console_messages[3].text, "no prefix");
    }

    #[test]
    fn network_request_add() {
        let mut dt = DevTools::new();
        dt.add_network_request(NetworkRequest {
            url: "https://example.com".into(),
            method: "GET".into(),
            status: 200,
            content_type: "text/html".into(),
            size: 1024,
            duration_ms: 150,
        });
        assert_eq!(dt.network_requests.len(), 1);
    }

    #[test]
    fn dom_flatten() {
        let mut dt = DevTools::new();
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "head".into(),
                        attributes: vec![],
                        children: vec![],
                    },
                    DomNode::Element {
                        tag: "body".into(),
                        attributes: vec![("class".into(), "main".into())],
                        children: vec![DomNode::Text("Hello".into())],
                    },
                ],
            }],
        };
        dt.update_dom(&dom);
        assert!(dt.dom_entries.len() >= 4); // document, html, head, body, "Hello"
    }

    #[test]
    fn format_helpers() {
        assert_eq!(format_size(0), "-");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(1_500_000), "1.4 MB");

        assert_eq!(format_duration(0), "-");
        assert_eq!(format_duration(150), "150 ms");
        assert_eq!(format_duration(1500), "1.50 s");

        assert_eq!(short_content_type("text/html"), "html");
        assert_eq!(short_content_type("application/javascript"), "js");
        assert_eq!(short_content_type("image/png"), "png");
        assert_eq!(short_content_type(""), "-");
    }

    #[test]
    fn network_filter() {
        let mut dt = DevTools::new();
        dt.add_network_request(NetworkRequest {
            url: "https://example.com/page".into(),
            method: "GET".into(),
            status: 200,
            content_type: "text/html".into(),
            size: 1024,
            duration_ms: 100,
        });
        dt.add_network_request(NetworkRequest {
            url: "https://example.com/style.css".into(),
            method: "GET".into(),
            status: 200,
            content_type: "text/css".into(),
            size: 512,
            duration_ms: 50,
        });
        dt.add_network_request(NetworkRequest {
            url: "https://example.com/app.js".into(),
            method: "GET".into(),
            status: 200,
            content_type: "application/javascript".into(),
            size: 2048,
            duration_ms: 75,
        });

        dt.network_filter = NetworkFilter::All;
        assert_eq!(dt.filtered_network_requests().len(), 3);

        dt.network_filter = NetworkFilter::Css;
        assert_eq!(dt.filtered_network_requests().len(), 1);

        dt.network_filter = NetworkFilter::Js;
        assert_eq!(dt.filtered_network_requests().len(), 1);
    }

    #[test]
    fn tab_click_handling() {
        let mut dt = DevTools::new();
        dt.visible = true;

        // Click on "Elements" tab (second tab, x=100..200).
        dt.handle_click(150.0, RESIZE_HANDLE_HEIGHT + 15.0);
        assert_eq!(dt.active_tab, DevToolsTab::Elements);

        // Click on "Network" tab (third tab, x=200..300).
        dt.handle_click(250.0, RESIZE_HANDLE_HEIGHT + 15.0);
        assert_eq!(dt.active_tab, DevToolsTab::Network);

        // Click on "Console" tab (first tab, x=0..100).
        dt.handle_click(50.0, RESIZE_HANDLE_HEIGHT + 15.0);
        assert_eq!(dt.active_tab, DevToolsTab::Console);
    }
}

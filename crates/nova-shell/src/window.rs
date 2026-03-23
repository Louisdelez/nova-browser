//! Window management — creates a GPU-accelerated window with wgpu.
//!
//! Opens a native window via winit, initializes a wgpu render surface,
//! and blits the software-rendered framebuffer to the screen.
//! Includes an interactive URL bar for navigation.

use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, error, info, warn};
use wgpu::util::DeviceExt;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, Modifiers, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    window::{CursorIcon, Window, WindowId},
};

use nova_core::NovaCore;
use nova_gpu::compositor::LayerTree;
use nova_gpu::vello_backend::VelloBackend;
use nova_mod_api::{Color, RenderCommands, RenderOp, TypedData, Viewport};

use crate::bookmarks::BookmarkStore;
use crate::downloads::DownloadsManager;
use crate::persistent_history::PersistentHistory;
use crate::renderer::Framebuffer;
use crate::tab::{FindMatch, Tab, TabManager};

/// Height of the URL bar area in logical pixels.
const URL_BAR_HEIGHT: f32 = 40.0;
/// Font size for URL bar text.
const URL_BAR_FONT_SIZE: f32 = 14.0;
/// Padding inside the URL bar.
const URL_BAR_PADDING: f32 = 8.0;
/// Height of the text input field inside the URL bar.
const URL_INPUT_HEIGHT: f32 = 28.0;

/// Height of the tab bar area in logical pixels.
const TAB_BAR_HEIGHT: f32 = 30.0;

/// Combined height of URL bar + tab bar (the chrome above the page).
const CHROME_HEIGHT: f32 = URL_BAR_HEIGHT + TAB_BAR_HEIGHT;

/// Pixels scrolled per mouse wheel tick.
const SCROLL_STEP: f32 = 40.0;
/// Pixels scrolled per arrow key press.
const ARROW_SCROLL_STEP: f32 = 40.0;
/// Fraction of viewport height scrolled per Page Up/Down press.
const PAGE_SCROLL_FRACTION: f32 = 0.9;
/// Interpolation factor for smooth scrolling (0.0 = no movement, 1.0 = instant).
/// Higher values make scrolling snappier; lower values make it smoother.
const SMOOTH_SCROLL_FACTOR: f32 = 0.25;
/// When the remaining scroll distance is below this threshold (in pixels),
/// snap directly to the target to avoid sub-pixel drifting.
const SMOOTH_SCROLL_SNAP_THRESHOLD: f32 = 0.5;

// ---------------------------------------------------------------------------
// Hit regions (clickable links)
// ---------------------------------------------------------------------------

/// A rectangular region on the page that is clickable (e.g. a link).
#[derive(Debug, Clone)]
pub struct HitRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub url: String,
    /// The `title` attribute text (for tooltip).
    pub title: String,
    /// The `target` attribute (e.g. "_blank", "_self"). Empty means default.
    pub target: String,
}

/// An anchor point on the page (element with an `id` attribute).
/// Used for scrolling to `#section` anchors.
#[derive(Debug, Clone)]
pub struct AnchorRegion {
    /// The `id` attribute value.
    pub id: String,
    /// Y coordinate in page coordinates.
    pub y: f32,
}

impl HitRegion {
    /// Test whether a point (in page coordinates) falls inside this region.
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

/// An interactive form field region on the page.
#[derive(Debug, Clone)]
pub struct FormFieldRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub value: String,
    pub field_type: String,
    /// The `name` attribute of the form field (used for form submission).
    name: String,
    /// The `action` URL of the parent `<form>` element.
    form_action: String,
    /// The HTTP method of the parent `<form>` element ("get" or "post").
    form_method: String,
    /// The `enctype` of the parent `<form>` element.
    form_enctype: String,
    /// The placeholder text for the field.
    pub placeholder: String,
    /// Whether the field is checked (checkbox/radio).
    pub checked: bool,
    /// Whether the field is required.
    pub required: bool,
    /// Options for `<select>` elements.
    pub options: Vec<(String, String, bool)>,
    /// Validation pattern.
    pub pattern: String,
    /// Min value for number inputs.
    pub min: String,
    /// Max value for number inputs.
    pub max: String,
    /// Max character length.
    pub maxlength: Option<usize>,
    /// Min character length.
    pub minlength: Option<usize>,
    /// Whether this field has the `autofocus` attribute.
    pub autofocus: bool,
    /// The `tabindex` attribute value (None = not specified).
    pub tabindex: Option<i32>,
    /// The `title` attribute text (for tooltip).
    pub title: String,
    /// Whether `pointer-events: none` is set on this element.
    pub pointer_events_none: bool,
}

impl FormFieldRegion {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }

    /// Validate the field value according to HTML5 validation attributes.
    ///
    /// Returns `None` if valid, or `Some(error_message)` if invalid.
    pub fn validate(&self) -> Option<String> {
        // Required check.
        if self.required && self.value.is_empty()
            && !matches!(self.field_type.as_str(), "checkbox" | "radio")
        {
            return Some("This field is required".to_string());
        }
        // Required checkbox must be checked.
        if self.required && self.field_type == "checkbox" && !self.checked {
            return Some("This checkbox is required".to_string());
        }
        // Pattern validation.
        if !self.pattern.is_empty() && !self.value.is_empty() {
            if let Ok(re) = regex::Regex::new(&format!("^(?:{})$", self.pattern)) {
                if !re.is_match(&self.value) {
                    return Some(format!("Value does not match pattern: {}", self.pattern));
                }
            }
        }
        // Min/max for number inputs.
        if self.field_type == "number" && !self.value.is_empty() {
            if let Ok(v) = self.value.parse::<f64>() {
                if !self.min.is_empty() {
                    if let Ok(min_v) = self.min.parse::<f64>() {
                        if v < min_v {
                            return Some(format!("Value must be at least {}", self.min));
                        }
                    }
                }
                if !self.max.is_empty() {
                    if let Ok(max_v) = self.max.parse::<f64>() {
                        if v > max_v {
                            return Some(format!("Value must be at most {}", self.max));
                        }
                    }
                }
            }
        }
        // Maxlength.
        if let Some(maxlen) = self.maxlength {
            if self.value.len() > maxlen {
                return Some(format!("Value must be at most {} characters", maxlen));
            }
        }
        // Minlength.
        if let Some(minlen) = self.minlength {
            if !self.value.is_empty() && self.value.len() < minlen {
                return Some(format!("Value must be at least {} characters", minlen));
            }
        }
        None
    }
}

/// State of the GPU surface once the window is created.
struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// The texture we blit our framebuffer onto.
    blit_pipeline: wgpu::RenderPipeline,
    blit_bind_group_layout: wgpu::BindGroupLayout,
    blit_sampler: wgpu::Sampler,
}

/// The browser window application.
pub struct BrowserWindow {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    framebuffer: Framebuffer,
    title: String,
    width: u32,
    height: u32,

    // -- Tab management --
    /// Manages all open tabs and their per-tab state.
    tabs: TabManager,

    // -- URL bar state --
    /// Current text in the URL bar.
    url_bar_text: String,
    /// Whether the URL bar input field has focus.
    url_bar_focused: bool,
    /// Cursor position (character index) in the URL bar text.
    url_bar_cursor: usize,
    /// Whether all text is selected (select-all on focus).
    url_bar_select_all: bool,
    /// Last known cursor position for mouse click handling.
    cursor_x: f64,
    /// Last known cursor position for mouse click handling.
    cursor_y: f64,
    /// URL to navigate to when the user presses Enter. Returned from `run()`.
    pending_navigation: Option<String>,

    // -- Navigation --
    /// Reference to the core for in-place navigation.
    core: Arc<NovaCore>,
    /// Tokio runtime handle for async calls from the winit event loop.
    tokio_handle: tokio::runtime::Handle,
    /// Viewport dimensions for navigation.
    viewport: Viewport,

    // -- Layer compositor --
    /// Layer tree for GPU compositing (enables smooth scrolling).
    layer_tree: Option<LayerTree>,
    /// Whether the content framebuffer needs to be rebuilt (dirty content).
    content_dirty: bool,

    // -- Zoom (Ctrl+Plus/Minus) --
    /// Current zoom level (1.0 = 100%).
    zoom_level: f32,
    /// Ticks remaining to display the zoom indicator overlay.
    zoom_indicator_ticks: u32,

    // -- Bookmarks --
    /// Persistent bookmark store.
    bookmark_store: BookmarkStore,

    // -- Persistent history --
    /// Persistent browsing history.
    persistent_history: PersistentHistory,

    // -- Downloads --
    /// Downloads manager.
    downloads_manager: DownloadsManager,

    // -- Vello GPU rendering --
    /// Vello GPU backend for hardware-accelerated rendering of page content.
    /// When available, this replaces the software `Framebuffer` renderer for
    /// page content (the URL bar is still drawn in software on top).
    vello_backend: VelloBackend,
    /// Whether to attempt GPU rendering (disabled if GPU init fails).
    use_gpu_rendering: bool,

    // -- Keyboard modifier state --
    /// Current keyboard modifier state (Ctrl, Alt, Shift, etc.).
    modifiers: Modifiers,

    // -- Tooltip state --
    /// Text to display as a tooltip (from `title` attribute).
    tooltip_text: String,
    /// Position of the tooltip in window coordinates.
    tooltip_x: f32,
    tooltip_y: f32,
    /// Time when the cursor started hovering over the element with a title.
    tooltip_hover_start: Option<Instant>,

    // -- Notification toast --
    /// Text to display as a brief notification overlay.
    notification_text: String,
    /// When the notification was shown (used for auto-dismiss after ~2 seconds).
    notification_time: Option<Instant>,

    // -- Page status indicators --
    /// Whether the current page returned a 404 status.
    is_404: bool,
    /// Raw HTML source of the current page (for Ctrl+U view source).
    page_source_html: Option<String>,
}

impl BrowserWindow {
    /// Create a new browser window.
    ///
    /// `initial_url` is the URL shown in the URL bar on startup.
    /// `core` is used for in-place navigation when the user enters a new URL.
    pub fn new(
        width: u32,
        height: u32,
        title: &str,
        commands: RenderCommands,
        initial_url: &str,
        core: Arc<NovaCore>,
    ) -> Self {
        let hit_regions = Self::extract_hit_regions(&commands);
        let form_fields = Self::extract_form_fields(&commands);
        let anchor_regions = Self::extract_anchor_regions(&commands);
        let content_height = Self::compute_content_height(&commands);
        let content_width = Self::compute_content_width(&commands);
        let layer_tree = Some(LayerTree::from_render_commands(&commands, width as f32, height as f32, CHROME_HEIGHT));
        let fb = Framebuffer::new(width, height);
        let tokio_handle = tokio::runtime::Handle::current();
        let viewport = Viewport {
            width: width as f32,
            height: height as f32,
            scale_factor: 1.0,
        };

        let bookmark_store = BookmarkStore::load();
        let persistent_history = PersistentHistory::load();

        let mut vello_backend = VelloBackend::new();
        vello_backend.init(1.0);

        // Create the initial tab with per-page state.
        let mut initial_tab = Tab::new(0, initial_url, commands);
        initial_tab.hit_regions = hit_regions;
        initial_tab.form_fields = form_fields;
        initial_tab.anchor_regions = anchor_regions;
        initial_tab.content_height = content_height;
        initial_tab.content_width = content_width;

        let tab_manager = TabManager::new(initial_tab);

        let mut browser = Self {
            window: None,
            gpu: None,
            framebuffer: fb,
            title: title.to_string(),
            width,
            height,
            tabs: tab_manager,
            url_bar_text: initial_url.to_string(),
            url_bar_focused: false,
            url_bar_cursor: initial_url.len(),
            url_bar_select_all: false,
            cursor_x: 0.0,
            cursor_y: 0.0,
            pending_navigation: None,
            core,
            tokio_handle,
            viewport,
            layer_tree,
            content_dirty: false,
            zoom_level: 1.0,
            zoom_indicator_ticks: 0,
            bookmark_store,
            persistent_history,
            downloads_manager: DownloadsManager::new(),
            vello_backend,
            use_gpu_rendering: false, // Disabled: Vello text rendering not yet complete; software FreeType renderer used.
            modifiers: Modifiers::default(),
            tooltip_text: String::new(),
            tooltip_x: 0.0,
            tooltip_y: 0.0,
            tooltip_hover_start: None,
            notification_text: String::new(),
            notification_time: None,
            is_404: false,
            page_source_html: None,
        };

        // Autofocus: find the first form field with autofocus and set it as focused.
        browser.apply_autofocus();

        browser
    }

    /// Set focus to the first form field with the `autofocus` attribute.
    fn apply_autofocus(&mut self) {
        let tab = self.tabs.active_tab_mut();
        if tab.focused_field.is_some() {
            return;
        }
        for (i, field) in tab.form_fields.iter().enumerate() {
            if field.autofocus && !matches!(field.field_type.as_str(), "hidden") {
                tab.focused_field = Some(i);
                tab.form_cursor_pos = field.value.len();
                break;
            }
        }
    }

    /// Find the next focusable form field index after `current_idx`, respecting `tabindex`.
    ///
    /// Elements with `tabindex=-1` are skipped. Positive tabindex values are visited
    /// first in ascending order, then `tabindex=0` and unspecified follow in document order.
    fn next_tab_field(&self, current_idx: usize) -> Option<usize> {
        let total = self.tabs.active_tab().form_fields.len();
        if total == 0 {
            return None;
        }

        // Build a tab-order sorted list of focusable field indices.
        let mut tab_order: Vec<usize> = Vec::new();

        // First: positive tabindex fields in ascending order.
        let mut positive: Vec<(i32, usize)> = Vec::new();
        // Then: tabindex=0 or unspecified in document order.
        let mut natural: Vec<usize> = Vec::new();

        for (i, field) in self.tabs.active_tab().form_fields.iter().enumerate() {
            // Skip non-tabbable fields.
            if field.tabindex == Some(-1) {
                continue;
            }
            if matches!(field.field_type.as_str(), "hidden") {
                continue;
            }
            match field.tabindex {
                Some(ti) if ti > 0 => positive.push((ti, i)),
                _ => natural.push(i), // 0 or None
            }
        }

        positive.sort_by_key(|(ti, _)| *ti);
        for (_, idx) in &positive {
            tab_order.push(*idx);
        }
        tab_order.extend(natural);

        if tab_order.is_empty() {
            return None;
        }

        // Find where current_idx is in the tab order and return the next one.
        if let Some(pos) = tab_order.iter().position(|&i| i == current_idx) {
            let next_pos = (pos + 1) % tab_order.len();
            Some(tab_order[next_pos])
        } else {
            // Current field not in tab order; return first tabbable field.
            Some(tab_order[0])
        }
    }

    // -- Hit region / content height helpers --------------------------------

    /// Extract `HitRegion`s from `RenderOp::Link` entries in the render commands.
    fn extract_hit_regions(commands: &RenderCommands) -> Vec<HitRegion> {
        commands
            .ops
            .iter()
            .filter_map(|op| {
                if let RenderOp::Link { x, y, width, height, url, title, target } = op {
                    Some(HitRegion {
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                        url: url.clone(),
                        title: title.clone(),
                        target: target.clone(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Extract anchor regions from `RenderOp::Anchor` entries in the render commands.
    fn extract_anchor_regions(commands: &RenderCommands) -> Vec<AnchorRegion> {
        commands
            .ops
            .iter()
            .filter_map(|op| {
                if let RenderOp::Anchor { id, y } = op {
                    Some(AnchorRegion {
                        id: id.clone(),
                        y: *y,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Extract form field regions from render commands.
    fn extract_form_fields(commands: &RenderCommands) -> Vec<FormFieldRegion> {
        commands.ops.iter().filter_map(|op| {
            if let RenderOp::FormField {
                x, y, width, height, value, field_type,
                name, form_action, form_method, form_enctype,
                placeholder, checked, required, options,
                pattern, min, max, maxlength, minlength,
                autofocus, tabindex, title, pointer_events_none,
            } = op {
                Some(FormFieldRegion {
                    x: *x, y: *y, width: *width, height: *height,
                    value: value.clone(), field_type: field_type.clone(),
                    name: name.clone(), form_action: form_action.clone(),
                    form_method: form_method.clone(), form_enctype: form_enctype.clone(),
                    placeholder: placeholder.clone(), checked: *checked,
                    required: *required, options: options.clone(),
                    pattern: pattern.clone(), min: min.clone(), max: max.clone(),
                    maxlength: *maxlength, minlength: *minlength,
                    autofocus: *autofocus, tabindex: *tabindex,
                    title: title.clone(), pointer_events_none: *pointer_events_none,
                })
            } else {
                None
            }
        }).collect()
    }

    /// Compute the total content height by scanning all render ops for the
    /// maximum `(y + height)` value.
    fn compute_content_height(commands: &RenderCommands) -> f32 {
        let mut max_y: f32 = 0.0;
        for op in &commands.ops {
            let bottom = match op {
                RenderOp::FillRect { y, height, .. } => y + height,
                RenderOp::DrawText { y, font_size, .. } => y + font_size * 1.2,
                RenderOp::StrokeRect { y, height, .. } => y + height,
                RenderOp::DrawImage { y, height, .. } => y + height,
                RenderOp::Link { y, height, .. } => y + height,
                _ => 0.0,
            };
            if bottom > max_y {
                max_y = bottom;
            }
        }
        max_y
    }

    // -- Scrolling helpers --------------------------------------------------

    /// Compute the total content width by scanning all render ops for the
    /// maximum `(x + width)` value.
    fn compute_content_width(commands: &RenderCommands) -> f32 {
        let mut max_x: f32 = 0.0;
        for op in &commands.ops {
            let right = match op {
                RenderOp::FillRect { x, width, .. } => x + width,
                RenderOp::DrawText { x, text, font_size, .. } => {
                    x + text.len() as f32 * font_size * 0.6
                }
                RenderOp::StrokeRect { x, width, .. } => x + width,
                RenderOp::DrawImage { x, width, .. } => x + width,
                _ => 0.0,
            };
            if right > max_x {
                max_x = right;
            }
        }
        max_x
    }

    /// Advance smooth scroll animation by one step.
    ///
    /// Returns `true` if the scroll is still animating (i.e. scroll_y or
    /// scroll_x have not yet reached their targets).
    fn tick_smooth_scroll(&mut self) -> bool {
        let tab = self.tabs.active_tab_mut();
        let mut animating = false;

        let dy = tab.scroll_target_y - tab.scroll_y;
        if dy.abs() > SMOOTH_SCROLL_SNAP_THRESHOLD {
            tab.scroll_y += dy * SMOOTH_SCROLL_FACTOR;
            animating = true;
        } else if dy.abs() > 0.0 {
            tab.scroll_y = tab.scroll_target_y;
        }

        let dx = tab.scroll_target_x - tab.scroll_x;
        if dx.abs() > SMOOTH_SCROLL_SNAP_THRESHOLD {
            tab.scroll_x += dx * SMOOTH_SCROLL_FACTOR;
            animating = true;
        } else if dx.abs() > 0.0 {
            tab.scroll_x = tab.scroll_target_x;
        }

        animating
    }

    /// Clamp `scroll_y` and `scroll_target_y` to the valid range `[0, max_scroll]`.
    fn clamp_scroll(&mut self) {
        let page_viewport = (self.height as f32 - CHROME_HEIGHT).max(0.0);
        let tab = self.tabs.active_tab_mut();
        let max_scroll = (tab.content_height - page_viewport).max(0.0);
        tab.scroll_y = tab.scroll_y.clamp(0.0, max_scroll);
        tab.scroll_target_y = tab.scroll_target_y.clamp(0.0, max_scroll);
        // Clamp horizontal scroll.
        let max_scroll_x = (tab.content_width - self.width as f32).max(0.0);
        tab.scroll_x = tab.scroll_x.clamp(0.0, max_scroll_x);
        tab.scroll_target_x = tab.scroll_target_x.clamp(0.0, max_scroll_x);
    }

    // -- Link interaction helpers -------------------------------------------

    /// Perform a hit-test at the given window coordinates, returning the URL
    /// of the link under the cursor (if any).
    ///
    /// Window coordinates are translated to page coordinates by subtracting
    /// the URL bar offset and adding the scroll offset.
    fn hit_test(&self, win_x: f64, win_y: f64) -> Option<&str> {
        // Only test if the cursor is below the chrome (URL bar + tab bar).
        if (win_y as f32) <= CHROME_HEIGHT {
            return None;
        }

        let tab = self.tabs.active_tab();
        let page_x = win_x as f32 + tab.scroll_x;
        let page_y = (win_y as f32 - CHROME_HEIGHT) + tab.scroll_y;

        for region in &tab.hit_regions {
            if region.contains(page_x, page_y) {
                return Some(&region.url);
            }
        }
        None
    }

    /// Perform a hit-test returning both the URL and target attribute.
    fn hit_test_with_target(&self, win_x: f64, win_y: f64) -> Option<(&str, &str)> {
        if (win_y as f32) <= CHROME_HEIGHT {
            return None;
        }

        let tab = self.tabs.active_tab();
        let page_x = win_x as f32 + tab.scroll_x;
        let page_y = (win_y as f32 - CHROME_HEIGHT) + tab.scroll_y;

        for region in &tab.hit_regions {
            if region.contains(page_x, page_y) {
                return Some((&region.url, &region.target));
            }
        }
        None
    }

    /// Update the cursor icon based on whether the cursor is over a link or form field.
    /// Also updates tooltip state based on `title` attributes.
    fn update_cursor(&mut self) {
        let win_x = self.cursor_x;
        let win_y = self.cursor_y;

        // Determine tooltip text from hovered element.
        let mut new_tooltip = String::new();
        if (win_y as f32) > CHROME_HEIGHT {
            let tab = self.tabs.active_tab();
            let page_x = win_x as f32 + tab.scroll_x;
            let page_y = (win_y as f32 - CHROME_HEIGHT) + tab.scroll_y;

            // Check links for title.
            for region in &tab.hit_regions {
                if region.contains(page_x, page_y) && !region.title.is_empty() {
                    new_tooltip = region.title.clone();
                    break;
                }
            }
            // Check form fields for title.
            if new_tooltip.is_empty() {
                for field in &tab.form_fields {
                    if field.contains(page_x, page_y) && !field.title.is_empty() {
                        new_tooltip = field.title.clone();
                        break;
                    }
                }
            }
        }

        if new_tooltip != self.tooltip_text {
            if new_tooltip.is_empty() {
                self.tooltip_text.clear();
                self.tooltip_hover_start = None;
            } else {
                self.tooltip_text = new_tooltip;
                self.tooltip_hover_start = Some(Instant::now());
                self.tooltip_x = win_x as f32 + 12.0;
                self.tooltip_y = win_y as f32 + 20.0;
            }
        }

        if let Some(w) = &self.window {
            if self.hit_test_no_pointer_events(win_x, win_y).is_some() {
                w.set_cursor(CursorIcon::Pointer);
            } else if (win_y as f32) > CHROME_HEIGHT {
                // Check if cursor is over a text-like form field.
                let tab = self.tabs.active_tab();
                let page_x = win_x as f32 + tab.scroll_x;
                let page_y = (win_y as f32 - CHROME_HEIGHT) + tab.scroll_y;
                let over_text_field = tab.form_fields.iter().any(|f| {
                    f.contains(page_x, page_y)
                        && !f.pointer_events_none
                        && matches!(
                            f.field_type.as_str(),
                            "text" | "password" | "email" | "number" | "search"
                                | "tel" | "url" | "date" | "textarea"
                        )
                });
                if over_text_field {
                    w.set_cursor(CursorIcon::Text);
                } else {
                    w.set_cursor(CursorIcon::Default);
                }
            } else {
                w.set_cursor(CursorIcon::Default);
            }
        }
    }

    /// Hit test for links, but also considering pointer-events on form fields.
    fn hit_test_no_pointer_events(&self, win_x: f64, win_y: f64) -> Option<&str> {
        self.hit_test(win_x, win_y)
    }

    /// Run the window event loop (blocking).
    ///
    /// Returns `Ok(Some(url))` if the user entered a URL and pressed Enter,
    /// or `Ok(None)` if the window was closed normally.
    pub fn run(mut self) -> anyhow::Result<Option<String>> {
        let event_loop = EventLoop::new()?;
        // Use WaitUntil for cursor blinking when a form field is focused;
        // otherwise fall back to Wait via the about_to_wait handler.
        event_loop.set_control_flow(ControlFlow::Wait);
        event_loop.run_app(&mut self)?;
        Ok(self.pending_navigation)
    }

    /// Initialize the wgpu surface and pipeline.
    async fn init_gpu(&mut self, window: Arc<Window>) -> anyhow::Result<()> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = instance.create_surface(window.clone())?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("no suitable GPU adapter found"))?;

        info!("GPU adapter: {}", adapter.get_info().name);

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("nova-device"),
                ..Default::default()
            }, None)
            .await?;

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Create the blit pipeline — renders a fullscreen textured quad.
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit-shader"),
            source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("blit-bind-group-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("blit-pipeline-layout"),
                bind_group_layouts: &[&bind_group_layout],
                push_constant_ranges: &[],
            });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("blit-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        self.gpu = Some(GpuState {
            surface,
            device,
            queue,
            config,
            blit_pipeline: pipeline,
            blit_bind_group_layout: bind_group_layout,
            blit_sampler: sampler,
        });

        info!("GPU initialized, surface format: {:?}", format);
        Ok(())
    }

    /// Render the full frame: URL bar + scrolled page content + scrollbar.
    ///
    /// When the Vello GPU backend is available and `use_gpu_rendering` is enabled,
    /// page content is rendered on the GPU and the resulting pixels are copied
    /// into the framebuffer. The URL bar and scrollbar are always drawn in
    /// software on top.
    ///
    /// If GPU rendering fails or is unavailable, falls back to the software
    /// `Framebuffer` renderer.
    fn rebuild_framebuffer(&mut self) {
        self.framebuffer.reset(self.width, self.height);

        // Try GPU rendering for page content.
        let mut used_gpu = false;
        if self.use_gpu_rendering {
            let gpu_pixels = self.vello_backend.render_to_texture(
                &self.tabs.active_tab().render_commands.ops,
                self.width,
                self.height,
            );

            if self.vello_backend.has_gpu() && gpu_pixels.len() == (self.width as usize * self.height as usize * 4) {
                // GPU render succeeded — copy pixels into the framebuffer.
                self.framebuffer.pixels.copy_from_slice(&gpu_pixels);
                used_gpu = true;
                debug!("Frame rendered via Vello GPU backend");
            } else if !self.vello_backend.has_gpu() {
                // GPU init failed permanently — disable GPU rendering for this session.
                info!("Vello GPU not available, falling back to software renderer permanently");
                self.use_gpu_rendering = false;
            }
        }

        if !used_gpu {
            // Software fallback: render page content with the CPU renderer.
            let tab = self.tabs.active_tab();
            self.framebuffer.render_scrolled(
                &tab.render_commands,
                CHROME_HEIGHT,
                tab.scroll_x,
                tab.scroll_y,
                tab.content_height,
            );
        }

        // Redraw form fields whose values have changed (user typed into them).
        self.draw_form_field_overlays();

        // Draw focus ring on the focused form field (if any).
        {
            let tab = self.tabs.active_tab();
            if let Some(idx) = tab.focused_field {
                if let Some(field) = tab.form_fields.get(idx) {
                    let fx = field.x - tab.scroll_x;
                    let fy = field.y + CHROME_HEIGHT - tab.scroll_y;
                    let focus_color = Color { r: 0.26, g: 0.52, b: 0.96, a: 1.0 };
                    self.framebuffer.stroke_rect(fx - 1.0, fy - 1.0, field.width + 2.0, field.height + 2.0, focus_color, 2.0);
                }
            }
        }

        // Draw text cursor in the focused text field.
        self.draw_form_field_cursor();

        // Draw select dropdown overlay (on top of everything except URL bar).
        self.draw_select_dropdown();

        // Draw find-in-page highlights (before URL bar so they appear under it).
        self.draw_find_highlights();

        // Draw the tab bar below the URL bar.
        self.draw_tab_bar();

        // Draw the URL bar on top (overwrites the top region, not affected by scroll).
        self.draw_url_bar();

        // Draw the bookmark star in the URL bar.
        self.draw_bookmark_star();

        // Draw find bar overlay.
        self.draw_find_bar();

        // Draw zoom indicator.
        self.draw_zoom_indicator();

        // Draw tooltip overlay (after a 500ms hover delay).
        self.draw_tooltip();

        // Draw notification toast overlay (if active).
        self.draw_notification_overlay();

        // Draw 404 indicator in the URL bar area.
        if self.is_404 {
            let indicator_x = self.width as f32 - URL_BAR_PADDING - 60.0;
            let indicator_y = (URL_BAR_HEIGHT - 12.0) / 2.0;
            let color_404 = Color { r: 0.9, g: 0.5, b: 0.0, a: 0.9 };
            self.framebuffer.draw_text(
                indicator_x,
                indicator_y,
                "404",
                12.0,
                color_404,
                None,
                None,
                None,
                None,
            );
        }
    }

    /// Draw a tooltip overlay if hovering over an element with a `title` attribute.
    fn draw_tooltip(&mut self) {
        if self.tooltip_text.is_empty() {
            return;
        }
        // Show tooltip only after 500ms of hovering.
        if let Some(start) = self.tooltip_hover_start {
            if start.elapsed().as_millis() < 500 {
                return;
            }
        } else {
            return;
        }

        let text = &self.tooltip_text;
        let font_size = 12.0_f32;
        let char_w = font_size * 0.6;
        let padding = 4.0_f32;
        let tip_w = (text.len() as f32 * char_w + padding * 2.0).min(self.width as f32 - 10.0);
        let tip_h = font_size + padding * 2.0;

        // Clamp position so tooltip stays on screen.
        let tx = self.tooltip_x.min(self.width as f32 - tip_w - 2.0).max(2.0);
        let ty = self.tooltip_y.min(self.height as f32 - tip_h - 2.0).max(2.0);

        // Background.
        let bg = Color { r: 1.0, g: 1.0, b: 0.88, a: 1.0 }; // light yellow
        let border = Color::rgb(0.6, 0.6, 0.6);
        self.framebuffer.fill_rect(tx, ty, tip_w, tip_h, bg);
        self.framebuffer.fill_rect(tx, ty, tip_w, 1.0, border);
        self.framebuffer.fill_rect(tx, ty + tip_h - 1.0, tip_w, 1.0, border);
        self.framebuffer.fill_rect(tx, ty, 1.0, tip_h, border);
        self.framebuffer.fill_rect(tx + tip_w - 1.0, ty, 1.0, tip_h, border);
        // Text.
        self.framebuffer.draw_text(
            tx + padding, ty + font_size + padding - 2.0,
            text, font_size, Color::BLACK,
            None, None, None, None,
        );
    }

    /// Redraw form fields that the user has modified (typed text into).
    ///
    /// Since the original `RenderOp::FormField` ops contain the initial value
    /// from the HTML, we overlay the current value on top of the field's
    /// background when it has been edited.
    fn draw_form_field_overlays(&mut self) {
        // Collect field data first to avoid borrowing issues.
        let tab = self.tabs.active_tab();
        let scroll_x = tab.scroll_x;
        let scroll_y = tab.scroll_y;
        let focused_field = tab.focused_field;
        let form_selection = tab.form_selection;
        let fields: Vec<_> = tab.form_fields.iter().enumerate().map(|(i, f)| {
            (i, f.x, f.y, f.width, f.height, f.field_type.clone(), f.value.clone(),
             f.placeholder.clone(), f.checked, f.options.clone())
        }).collect();

        for (field_idx, x, y, w, h, field_type, value, placeholder, checked, options) in fields {
            let fx = x - scroll_x;
            let fy = y + CHROME_HEIGHT - scroll_y;
            let font_size = 14.0_f32; // Default form field font size.
            let approx_char_w = font_size * 0.6;

            // Determine if this field has an active selection.
            let selection_range = if focused_field == Some(field_idx) {
                form_selection.and_then(|(a, b)| {
                    let (start, end) = (a.min(b), a.max(b));
                    if start != end { Some((start, end)) } else { None }
                })
            } else {
                None
            };

            match field_type.as_str() {
                "text" | "email" | "number" | "search" | "tel" | "url" | "date" => {
                    // Redraw the field background + border + current text.
                    let border_color = Color::rgb(0.6, 0.6, 0.6);
                    self.framebuffer.fill_rect(fx, fy, w, h, Color::WHITE);
                    // Border (top, bottom, left, right).
                    self.framebuffer.fill_rect(fx, fy, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy + h - 1.0, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy, 1.0, h, border_color);
                    self.framebuffer.fill_rect(fx + w - 1.0, fy, 1.0, h, border_color);

                    // Draw selection highlight (blue background) before the text.
                    if let Some((sel_start, sel_end)) = selection_range {
                        let start_chars = value[..sel_start.min(value.len())].chars().count();
                        let end_chars = value[..sel_end.min(value.len())].chars().count();
                        let sel_x = fx + 4.0 + start_chars as f32 * approx_char_w;
                        let sel_w = (end_chars - start_chars) as f32 * approx_char_w;
                        let sel_y = fy + (h - font_size) / 2.0;
                        self.framebuffer.fill_rect(sel_x, sel_y, sel_w, font_size, Color::rgb(0.26, 0.52, 0.96));
                    }

                    let (display_text, text_color) = if value.is_empty() {
                        (placeholder.clone(), Color::rgb(0.6, 0.6, 0.6))
                    } else if selection_range.is_some() {
                        // Selected text is drawn white on blue.
                        (value.clone(), Color::BLACK)
                    } else {
                        (value.clone(), Color::BLACK)
                    };

                    if !display_text.is_empty() {
                        let text_y = fy + font_size + (h - font_size) / 2.0 - 2.0;
                        // If there's a selection, draw text in segments: normal, selected (white), normal.
                        if let Some((sel_start, sel_end)) = selection_range {
                            let sel_start = sel_start.min(value.len());
                            let sel_end = sel_end.min(value.len());
                            // Before selection.
                            if sel_start > 0 {
                                self.framebuffer.draw_text(
                                    fx + 4.0, text_y, &value[..sel_start], font_size, Color::BLACK,
                                    None, None, None, None,
                                );
                            }
                            // Selected text (white on blue).
                            if sel_end > sel_start {
                                let sel_text_x = fx + 4.0 + value[..sel_start].chars().count() as f32 * approx_char_w;
                                self.framebuffer.draw_text(
                                    sel_text_x, text_y, &value[sel_start..sel_end], font_size, Color::WHITE,
                                    None, None, None, None,
                                );
                            }
                            // After selection.
                            if sel_end < value.len() {
                                let after_text_x = fx + 4.0 + value[..sel_end].chars().count() as f32 * approx_char_w;
                                self.framebuffer.draw_text(
                                    after_text_x, text_y, &value[sel_end..], font_size, Color::BLACK,
                                    None, None, None, None,
                                );
                            }
                        } else {
                            self.framebuffer.draw_text(
                                fx + 4.0, text_y, &display_text, font_size, text_color,
                                None, None, None, None,
                            );
                        }
                    }
                }
                "password" => {
                    let border_color = Color::rgb(0.6, 0.6, 0.6);
                    self.framebuffer.fill_rect(fx, fy, w, h, Color::WHITE);
                    self.framebuffer.fill_rect(fx, fy, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy + h - 1.0, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy, 1.0, h, border_color);
                    self.framebuffer.fill_rect(fx + w - 1.0, fy, 1.0, h, border_color);

                    // Draw selection highlight for password fields.
                    if let Some((sel_start, sel_end)) = selection_range {
                        let start_chars = value[..sel_start.min(value.len())].chars().count();
                        let end_chars = value[..sel_end.min(value.len())].chars().count();
                        let sel_x = fx + 4.0 + start_chars as f32 * approx_char_w;
                        let sel_w = (end_chars - start_chars) as f32 * approx_char_w;
                        let sel_y = fy + (h - font_size) / 2.0;
                        self.framebuffer.fill_rect(sel_x, sel_y, sel_w, font_size, Color::rgb(0.26, 0.52, 0.96));
                    }

                    let (display_text, text_color) = if value.is_empty() {
                        (placeholder.clone(), Color::rgb(0.6, 0.6, 0.6))
                    } else {
                        ("\u{2022}".repeat(value.len()), Color::BLACK)
                    };

                    if !display_text.is_empty() {
                        let text_y = fy + font_size + (h - font_size) / 2.0 - 2.0;
                        self.framebuffer.draw_text(
                            fx + 4.0, text_y, &display_text, font_size, text_color,
                            None, None, None,
                            None,
                        );
                    }
                }
                "textarea" => {
                    let border_color = Color::rgb(0.6, 0.6, 0.6);
                    self.framebuffer.fill_rect(fx, fy, w, h, Color::WHITE);
                    self.framebuffer.fill_rect(fx, fy, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy + h - 1.0, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy, 1.0, h, border_color);
                    self.framebuffer.fill_rect(fx + w - 1.0, fy, 1.0, h, border_color);

                    // Draw selection highlight for textarea.
                    if let Some((sel_start, sel_end)) = selection_range {
                        let start_chars = value[..sel_start.min(value.len())].chars().count();
                        let end_chars = value[..sel_end.min(value.len())].chars().count();
                        let sel_x = fx + 4.0 + start_chars as f32 * approx_char_w;
                        let sel_w = (end_chars - start_chars) as f32 * approx_char_w;
                        let sel_y = fy + 2.0;
                        self.framebuffer.fill_rect(sel_x, sel_y, sel_w, font_size, Color::rgb(0.26, 0.52, 0.96));
                    }

                    let (display_text, text_color) = if value.is_empty() {
                        (placeholder.clone(), Color::rgb(0.6, 0.6, 0.6))
                    } else {
                        (value.clone(), Color::BLACK)
                    };

                    if !display_text.is_empty() {
                        let text_y = fy + font_size + 2.0;
                        self.framebuffer.draw_text(
                            fx + 4.0, text_y, &display_text, font_size, text_color,
                            None, None, None,
                            None,
                        );
                    }
                }
                "file" => {
                    // Render file input: button + filename display.
                    let border_color = Color::rgb(0.6, 0.6, 0.6);
                    self.framebuffer.fill_rect(fx, fy, w, h, Color::rgb(0.95, 0.95, 0.95));
                    // Border.
                    self.framebuffer.fill_rect(fx, fy, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy + h - 1.0, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy, 1.0, h, border_color);
                    self.framebuffer.fill_rect(fx + w - 1.0, fy, 1.0, h, border_color);

                    // "Choose File" button area.
                    let btn_w = 90.0_f32.min(w * 0.4);
                    self.framebuffer.fill_rect(fx + 1.0, fy + 1.0, btn_w, h - 2.0, Color::rgb(0.88, 0.88, 0.88));
                    self.framebuffer.fill_rect(fx + btn_w, fy, 1.0, h, border_color);

                    let text_y = fy + font_size + (h - font_size) / 2.0 - 2.0;
                    self.framebuffer.draw_text(
                        fx + 6.0, text_y, "Choose File", font_size * 0.85, Color::BLACK,
                        None, None, None, None,
                    );

                    // Display filename or "No file chosen".
                    let display_text = if value.is_empty() {
                        "No file chosen".to_string()
                    } else {
                        // Show just the filename, not the full path.
                        std::path::Path::new(&value)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or(value.clone())
                    };
                    self.framebuffer.draw_text(
                        fx + btn_w + 8.0, text_y, &display_text, font_size * 0.85,
                        Color::rgb(0.3, 0.3, 0.3),
                        None, None, None, None,
                    );
                }
                "checkbox" => {
                    // Redraw checkbox with current checked state.
                    let box_size = h.min(w).min(13.0);
                    let bx = fx + (w - box_size) / 2.0;
                    let by = fy + (h - box_size) / 2.0;
                    let border_color = Color::rgb(0.6, 0.6, 0.6);
                    self.framebuffer.fill_rect(bx, by, box_size, 1.0, border_color);
                    self.framebuffer.fill_rect(bx, by + box_size - 1.0, box_size, 1.0, border_color);
                    self.framebuffer.fill_rect(bx, by, 1.0, box_size, border_color);
                    self.framebuffer.fill_rect(bx + box_size - 1.0, by, 1.0, box_size, border_color);
                    let bg = if checked { Color::rgb(0.26, 0.52, 0.96) } else { Color::WHITE };
                    self.framebuffer.fill_rect(bx + 1.0, by + 1.0, box_size - 2.0, box_size - 2.0, bg);
                    if checked {
                        self.framebuffer.draw_text(
                            bx + 1.0, by + box_size - 2.0,
                            "\u{2713}", box_size - 2.0, Color::WHITE,
                            Some(700), None, None,
                            None,
                        );
                    }
                }
                "select" => {
                    // Redraw select with current value.
                    let border_color = Color::rgb(0.6, 0.6, 0.6);
                    self.framebuffer.fill_rect(fx, fy, w, h, Color::WHITE);
                    self.framebuffer.fill_rect(fx, fy, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy + h - 1.0, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy, 1.0, h, border_color);
                    self.framebuffer.fill_rect(fx + w - 1.0, fy, 1.0, h, border_color);
                    let arrow_w = 20.0_f32.min(w * 0.15);
                    self.framebuffer.fill_rect(
                        fx + w - arrow_w, fy, arrow_w, h,
                        Color::rgb(0.93, 0.93, 0.93),
                    );
                    self.framebuffer.draw_text(
                        fx + w - arrow_w + 4.0,
                        fy + font_size + (h - font_size) / 2.0 - 2.0,
                        "\u{25BC}", font_size * 0.6,
                        Color::rgb(0.3, 0.3, 0.3),
                        None, None, None,
                        None,
                    );
                    // Display the selected option's label, or the value.
                    let display_text = if !value.is_empty() {
                        options.iter()
                            .find(|(v, _, _)| *v == value)
                            .map(|(_, label, _)| label.clone())
                            .unwrap_or(value.clone())
                    } else if !placeholder.is_empty() {
                        placeholder.clone()
                    } else {
                        "Select".to_string()
                    };
                    self.framebuffer.draw_text(
                        fx + 6.0,
                        fy + font_size + (h - font_size) / 2.0 - 2.0,
                        &display_text, font_size, Color::BLACK,
                        None, None, None,
                        None,
                    );
                }
                _ => {
                    // Other field types (radio, submit, etc.) don't need overlay redraw.
                }
            }
        }
    }

    /// Draw a blinking text cursor in the currently focused text field.
    fn draw_form_field_cursor(&mut self) {
        let tab = self.tabs.active_tab_mut();
        let idx = match tab.focused_field {
            Some(i) => i,
            None => return,
        };

        let field = match tab.form_fields.get(idx) {
            Some(f) => f,
            None => return,
        };

        // Only draw cursor for text-like fields.
        let is_text_field = matches!(
            field.field_type.as_str(),
            "text" | "password" | "email" | "number" | "search" | "tel" | "url" | "date" | "textarea"
        );
        if !is_text_field {
            return;
        }

        // Toggle cursor visibility every 500ms.
        let elapsed = tab.cursor_blink_time.elapsed();
        if elapsed.as_millis() >= 500 {
            tab.cursor_visible = !tab.cursor_visible;
            tab.cursor_blink_time = Instant::now();
        }

        if !tab.cursor_visible {
            return;
        }

        let scroll_x = tab.scroll_x;
        let scroll_y = tab.scroll_y;
        let form_cursor_pos = tab.form_cursor_pos;
        let fx = field.x - scroll_x;
        let fy = field.y + CHROME_HEIGHT - scroll_y;
        let h = field.height;
        let font_size = 14.0_f32;

        // Compute cursor x position based on character count.
        let approx_char_w = font_size * 0.6;
        let display_len = if field.field_type == "password" {
            field.value.len() // Each char is one bullet
        } else {
            field.value.len()
        };
        let cursor_char_pos = form_cursor_pos.min(display_len);
        let cursor_x = fx + 4.0 + cursor_char_pos as f32 * approx_char_w;
        let cursor_y = fy + (h - font_size) / 2.0;

        let cursor_color = Color::BLACK;
        self.framebuffer.fill_rect(cursor_x, cursor_y, 1.5, font_size, cursor_color);
    }

    /// Draw a dropdown overlay for the currently open `<select>` field.
    fn draw_select_dropdown(&mut self) {
        let tab = self.tabs.active_tab_mut();
        if !tab.dropdown_open {
            return;
        }

        let dd_idx = tab.dropdown_field_idx;
        let field = match tab.form_fields.get(dd_idx) {
            Some(f) => f,
            None => {
                tab.dropdown_open = false;
                return;
            }
        };

        let options = field.options.clone();
        if options.is_empty() {
            return;
        }

        let scroll_x = tab.scroll_x;
        let scroll_y = tab.scroll_y;
        let dropdown_hover_idx = tab.dropdown_hover_idx;
        let fx = field.x - scroll_x;
        let fy = field.y + CHROME_HEIGHT - scroll_y + field.height;
        let w = field.width;
        let option_h = 24.0_f32;
        let total_h = option_h * options.len() as f32;
        let font_size = 13.0_f32;
        let current_value = field.value.clone();

        // Dropdown background.
        let bg = Color::WHITE;
        self.framebuffer.fill_rect(fx, fy, w, total_h, bg);

        // Dropdown border.
        let border = Color::rgb(0.6, 0.6, 0.6);
        self.framebuffer.fill_rect(fx, fy, w, 1.0, border);
        self.framebuffer.fill_rect(fx, fy + total_h - 1.0, w, 1.0, border);
        self.framebuffer.fill_rect(fx, fy, 1.0, total_h, border);
        self.framebuffer.fill_rect(fx + w - 1.0, fy, 1.0, total_h, border);

        // Draw each option.
        for (i, (val, label, _selected)) in options.iter().enumerate() {
            let oy = fy + i as f32 * option_h;

            // Hover highlight.
            if Some(i) == dropdown_hover_idx {
                let hover_bg = Color::rgb(0.26, 0.52, 0.96);
                self.framebuffer.fill_rect(fx + 1.0, oy, w - 2.0, option_h, hover_bg);
            } else if *val == current_value {
                // Selected item highlight.
                let sel_bg = Color::rgb(0.9, 0.93, 1.0);
                self.framebuffer.fill_rect(fx + 1.0, oy, w - 2.0, option_h, sel_bg);
            }

            // Option text.
            let text_color = if Some(i) == dropdown_hover_idx {
                Color::WHITE
            } else {
                Color::BLACK
            };
            let text_y = oy + font_size + (option_h - font_size) / 2.0 - 2.0;
            self.framebuffer.draw_text(
                fx + 6.0, text_y, label, font_size, text_color,
                None, None, None,
                None,
            );

            // Separator line between options.
            if i < options.len() - 1 {
                let sep_color = Color::rgb(0.9, 0.9, 0.9);
                self.framebuffer.fill_rect(fx + 1.0, oy + option_h - 1.0, w - 2.0, 1.0, sep_color);
            }
        }
    }

    /// Draw the URL bar chrome at the top of the framebuffer.
    fn draw_url_bar(&mut self) {
        let w = self.width as f32;

        // -- URL bar background (light gray toolbar) --
        let toolbar_bg = Color {
            r: 0.93,
            g: 0.93,
            b: 0.93,
            a: 1.0,
        };
        self.framebuffer
            .fill_rect(0.0, 0.0, w, URL_BAR_HEIGHT, toolbar_bg);

        // -- Bottom border of toolbar --
        let border_color = Color {
            r: 0.78,
            g: 0.78,
            b: 0.78,
            a: 1.0,
        };
        self.framebuffer
            .fill_rect(0.0, URL_BAR_HEIGHT - 1.0, w, 1.0, border_color);

        // -- Input field rectangle --
        let input_x = URL_BAR_PADDING;
        let input_y = (URL_BAR_HEIGHT - URL_INPUT_HEIGHT) / 2.0;
        let input_w = w - URL_BAR_PADDING * 2.0;

        // White background for input field.
        self.framebuffer.fill_rect(
            input_x,
            input_y,
            input_w,
            URL_INPUT_HEIGHT,
            Color::WHITE,
        );

        // Border: blue if focused, gray otherwise.
        let input_border = if self.url_bar_focused {
            Color {
                r: 0.26,
                g: 0.52,
                b: 0.96,
                a: 1.0,
            }
        } else {
            Color {
                r: 0.75,
                g: 0.75,
                b: 0.75,
                a: 1.0,
            }
        };
        self.framebuffer.stroke_rect(
            input_x,
            input_y,
            input_w,
            URL_INPUT_HEIGHT,
            input_border,
            1.0,
        );

        // -- URL text --
        let text_x = input_x + 6.0;
        // Vertically center the text inside the input field.
        let text_y = input_y + (URL_INPUT_HEIGHT - URL_BAR_FONT_SIZE) / 2.0;

        let text_color = Color {
            r: 0.13,
            g: 0.13,
            b: 0.13,
            a: 1.0,
        };

        // If select-all, draw a selection highlight behind the text.
        if self.url_bar_focused && self.url_bar_select_all && !self.url_bar_text.is_empty() {
            let select_color = Color {
                r: 0.26,
                g: 0.52,
                b: 0.96,
                a: 0.3,
            };
            // Approximate text width using char count and estimated char width.
            let approx_char_w = URL_BAR_FONT_SIZE * 0.6;
            let text_pixel_w = self.url_bar_text.len() as f32 * approx_char_w;
            self.framebuffer.fill_rect(
                text_x,
                text_y - 1.0,
                text_pixel_w.min(input_w - 12.0),
                URL_BAR_FONT_SIZE + 2.0,
                select_color,
            );
        }

        self.framebuffer
            .draw_text(text_x, text_y, &self.url_bar_text, URL_BAR_FONT_SIZE, text_color, None, None, None, None);

        // -- Cursor (vertical line at the cursor position) --
        if self.url_bar_focused && !self.url_bar_select_all {
            let approx_char_w = URL_BAR_FONT_SIZE * 0.6;
            let cursor_x = text_x + (self.url_bar_cursor as f32 * approx_char_w);
            let cursor_color = Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            };
            self.framebuffer
                .fill_rect(cursor_x, text_y - 1.0, 1.0, URL_BAR_FONT_SIZE + 2.0, cursor_color);
        }
    }

    /// Upload the framebuffer to GPU and render it.
    fn render_frame(&self) {
        let gpu = match &self.gpu {
            Some(g) => g,
            None => return,
        };

        let output = match gpu.surface.get_current_texture() {
            Ok(t) => t,
            Err(e) => {
                error!("failed to get surface texture: {e}");
                return;
            }
        };

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Create a texture from the framebuffer.
        let fb_texture = gpu.device.create_texture_with_data(
            &gpu.queue,
            &wgpu::TextureDescriptor {
                label: Some("framebuffer-texture"),
                size: wgpu::Extent3d {
                    width: self.framebuffer.width,
                    height: self.framebuffer.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &self.framebuffer.pixels,
        );

        let fb_view = fb_texture.create_view(&Default::default());

        let bind_group = gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("blit-bind-group"),
                layout: &gpu.blit_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&fb_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&gpu.blit_sampler),
                    },
                ],
            });

        let mut encoder =
            gpu.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("blit-encoder"),
                });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });

            pass.set_pipeline(&gpu.blit_pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1); // fullscreen quad = 2 triangles = 6 vertices
        }

        gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }

    /// Handle keyboard input when the URL bar is focused.
    ///
    /// Returns `true` if Enter was pressed (signals the event loop to exit for navigation).
    fn handle_url_bar_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        if !self.url_bar_focused || event.state != ElementState::Pressed {
            return false;
        }

        match &event.logical_key {
            Key::Named(NamedKey::Enter) => {
                let raw = self.url_bar_text.trim().to_string();
                let url = normalize_url_or_search(&raw);
                info!("URL bar: navigating to {url}");
                self.url_bar_text = url.clone();
                self.url_bar_cursor = self.url_bar_text.len();
                self.navigate_in_place(&url);
                return false;
            }
            Key::Named(NamedKey::Escape) => {
                self.url_bar_focused = false;
                self.url_bar_select_all = false;
            }
            Key::Named(NamedKey::Backspace) => {
                if self.url_bar_select_all {
                    self.url_bar_text.clear();
                    self.url_bar_cursor = 0;
                    self.url_bar_select_all = false;
                } else if self.url_bar_cursor > 0 {
                    // Find the previous char boundary.
                    let prev = self.url_bar_text[..self.url_bar_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.url_bar_text.remove(prev);
                    self.url_bar_cursor = prev;
                }
            }
            Key::Named(NamedKey::Delete) => {
                if self.url_bar_select_all {
                    self.url_bar_text.clear();
                    self.url_bar_cursor = 0;
                    self.url_bar_select_all = false;
                } else if self.url_bar_cursor < self.url_bar_text.len() {
                    self.url_bar_text.remove(self.url_bar_cursor);
                }
            }
            Key::Named(NamedKey::ArrowLeft) => {
                self.url_bar_select_all = false;
                if self.url_bar_cursor > 0 {
                    // Move to previous char boundary.
                    self.url_bar_cursor = self.url_bar_text[..self.url_bar_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.url_bar_select_all = false;
                if self.url_bar_cursor < self.url_bar_text.len() {
                    // Move to next char boundary.
                    self.url_bar_cursor = self.url_bar_text[self.url_bar_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.url_bar_cursor + i)
                        .unwrap_or(self.url_bar_text.len());
                }
            }
            Key::Named(NamedKey::Home) => {
                self.url_bar_select_all = false;
                self.url_bar_cursor = 0;
            }
            Key::Named(NamedKey::End) => {
                self.url_bar_select_all = false;
                self.url_bar_cursor = self.url_bar_text.len();
            }
            _ => {
                // Handle text input from `event.text`.
                if let Some(text) = &event.text {
                    let s = text.as_str();
                    if !s.is_empty() && s.chars().all(|c| !c.is_control()) {
                        if self.url_bar_select_all {
                            self.url_bar_text.clear();
                            self.url_bar_cursor = 0;
                            self.url_bar_select_all = false;
                        }
                        self.url_bar_text.insert_str(self.url_bar_cursor, s);
                        self.url_bar_cursor += s.len();
                    }
                }
            }
        }
        false
    }

    /// Handle a mouse click for URL bar focus/unfocus.
    fn handle_mouse_click(&mut self, x: f64, y: f64) {
        let input_y_start = ((URL_BAR_HEIGHT - URL_INPUT_HEIGHT) / 2.0) as f64;
        let input_y_end = input_y_start + URL_INPUT_HEIGHT as f64;

        if y >= input_y_start && y <= input_y_end && x >= URL_BAR_PADDING as f64 {
            // Clicked inside the URL bar input field.
            if !self.url_bar_focused {
                // First click: focus and select all.
                self.url_bar_focused = true;
                self.url_bar_select_all = true;
                self.url_bar_cursor = self.url_bar_text.len();
            } else {
                // Already focused: position cursor based on click x.
                self.url_bar_select_all = false;
                let text_x = URL_BAR_PADDING as f64 + 6.0;
                let approx_char_w = URL_BAR_FONT_SIZE as f64 * 0.6;
                let char_pos = ((x - text_x) / approx_char_w).round().max(0.0) as usize;
                self.url_bar_cursor = char_pos.min(self.url_bar_text.len());
            }
        } else {
            // Clicked outside the URL bar.
            self.url_bar_focused = false;
            self.url_bar_select_all = false;
        }
    }

    /// Rebuild the framebuffer and request a redraw.
    fn request_redraw(&mut self) {
        self.rebuild_framebuffer();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Get the selected text range in the focused form field, ordered as `(min, max)`.
    fn form_selection_range(&self) -> Option<(usize, usize)> {
        let tab = self.tabs.active_tab();
        tab.form_selection.map(|(a, b)| (a.min(b), a.max(b)))
    }

    /// Delete the currently selected text in the focused form field.
    /// Returns `true` if text was deleted.
    fn delete_form_selection(&mut self) -> bool {
        let tab = self.tabs.active_tab();
        let sel = match tab.form_selection {
            Some((a, b)) if a != b => (a.min(b), a.max(b)),
            _ => return false,
        };
        let idx = match tab.focused_field {
            Some(i) => i,
            None => return false,
        };
        let tab = self.tabs.active_tab_mut();
        if let Some(field) = tab.form_fields.get_mut(idx) {
            field.value.drain(sel.0..sel.1);
            tab.form_cursor_pos = sel.0;
            tab.form_selection = None;
        }
        true
    }

    /// Get the text currently selected in the focused form field.
    fn get_form_selected_text(&self) -> Option<String> {
        let tab = self.tabs.active_tab();
        let idx = tab.focused_field?;
        let (start, end) = tab.form_selection.map(|(a, b)| (a.min(b), a.max(b)))?;
        if start == end {
            return None;
        }
        let field = tab.form_fields.get(idx)?;
        Some(field.value[start..end].to_string())
    }

    /// Handle keyboard input when a form field is focused.
    ///
    /// Returns `true` if the event was consumed.
    fn handle_form_field_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        let tab = self.tabs.active_tab_mut();
        let idx = match tab.focused_field {
            Some(i) => i,
            None => return false,
        };

        // Reset cursor blink on any keypress.
        tab.cursor_visible = true;
        tab.cursor_blink_time = Instant::now();

        let shift_held = self.modifiers.state().shift_key();

        // Detect Ctrl modifier from event text (control characters).
        let is_ctrl_combo = event
            .text
            .as_ref()
            .map(|t| t.as_str().chars().next().is_some_and(|c| c.is_control()))
            .unwrap_or(false);
        let key_str = match &event.logical_key {
            Key::Character(c) => Some(c.as_str()),
            _ => None,
        };

        // --- Ctrl+A: Select all ---
        if key_str == Some("a") && is_ctrl_combo {
            let tab = self.tabs.active_tab_mut();
            if let Some(field) = tab.form_fields.get(idx) {
                tab.form_selection = Some((0, field.value.len()));
                tab.form_cursor_pos = field.value.len();
            }
            return true;
        }

        // --- Ctrl+C: Copy ---
        if key_str == Some("c") && is_ctrl_combo {
            let text = self.get_form_selected_text().unwrap_or_else(|| {
                // If no selection, copy all text.
                self.tabs.active_tab().form_fields.get(idx)
                    .map(|f| f.value.clone())
                    .unwrap_or_default()
            });
            if !text.is_empty() {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Err(e) = clipboard.set_text(&text) {
                        warn!("Clipboard set_text failed: {e}");
                    }
                }
            }
            return true;
        }

        // --- Ctrl+X: Cut ---
        if key_str == Some("x") && is_ctrl_combo {
            let text = self.get_form_selected_text().unwrap_or_else(|| {
                // If no selection, cut all text.
                self.tabs.active_tab().form_fields.get(idx)
                    .map(|f| f.value.clone())
                    .unwrap_or_default()
            });
            if !text.is_empty() {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Err(e) = clipboard.set_text(&text) {
                        warn!("Clipboard set_text failed: {e}");
                    }
                }
                // Delete the selected text (or all text if no selection).
                if !self.delete_form_selection() {
                    // No selection existed — clear the entire field.
                    let tab = self.tabs.active_tab_mut();
                    if let Some(field) = tab.form_fields.get_mut(idx) {
                        field.value.clear();
                        tab.form_cursor_pos = 0;
                    }
                }
            }
            return true;
        }

        // --- Ctrl+V: Paste ---
        if key_str == Some("v") && is_ctrl_combo {
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                if let Ok(text) = clipboard.get_text() {
                    if !text.is_empty() {
                        // Delete any existing selection first.
                        self.delete_form_selection();
                        let tab = self.tabs.active_tab_mut();
                        if let Some(field) = tab.form_fields.get_mut(idx) {
                            // Enforce maxlength.
                            let insert_text = if let Some(maxlen) = field.maxlength {
                                let remaining = maxlen.saturating_sub(field.value.len());
                                if remaining == 0 {
                                    return true;
                                }
                                // Truncate paste to fit.
                                let end = text.char_indices()
                                    .take(remaining)
                                    .last()
                                    .map(|(i, c)| i + c.len_utf8())
                                    .unwrap_or(0);
                                &text[..end]
                            } else {
                                &text
                            };
                            // Filter out control characters except newlines in textarea.
                            let filtered: String = insert_text.chars()
                                .filter(|c| !c.is_control() || (field.field_type == "textarea" && *c == '\n'))
                                .collect();
                            field.value.insert_str(tab.form_cursor_pos, &filtered);
                            tab.form_cursor_pos += filtered.len();
                            tab.form_selection = None;
                        }
                    }
                }
            }
            return true;
        }

        match &event.logical_key {
            Key::Named(NamedKey::Enter) => {
                // Submit the form when Enter is pressed in a text field.
                self.submit_form(idx);
                return true;
            }
            Key::Named(NamedKey::Escape) => {
                let tab = self.tabs.active_tab_mut();
                tab.focused_field = None;
                tab.dropdown_open = false;
                tab.form_selection = None;
                return true;
            }
            Key::Named(NamedKey::Backspace) => {
                // If there's a selection, delete it.
                if self.delete_form_selection() {
                    return true;
                }
                let tab = self.tabs.active_tab_mut();
                if let Some(field) = tab.form_fields.get_mut(idx) {
                    if tab.form_cursor_pos > 0 && !field.value.is_empty() {
                        let prev = field.value[..tab.form_cursor_pos]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        field.value.remove(prev);
                        tab.form_cursor_pos = prev;
                    }
                }
                return true;
            }
            Key::Named(NamedKey::Delete) => {
                if self.delete_form_selection() {
                    return true;
                }
                let tab = self.tabs.active_tab_mut();
                if let Some(field) = tab.form_fields.get_mut(idx) {
                    if tab.form_cursor_pos < field.value.len() {
                        field.value.remove(tab.form_cursor_pos);
                    }
                }
                return true;
            }
            Key::Named(NamedKey::ArrowLeft) => {
                let tab = self.tabs.active_tab_mut();
                if let Some(field) = tab.form_fields.get(idx) {
                    let old_pos = tab.form_cursor_pos;
                    let new_pos = if old_pos > 0 {
                        field.value[..old_pos]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0)
                    } else {
                        0
                    };
                    if shift_held {
                        // Extend selection.
                        let anchor = tab.form_selection.map(|(a, _)| a).unwrap_or(old_pos);
                        tab.form_selection = Some((anchor, new_pos));
                    } else {
                        // If there was a selection, move cursor to start of selection.
                        if let Some((a, b)) = tab.form_selection {
                            let min = a.min(b);
                            tab.form_cursor_pos = min;
                            tab.form_selection = None;
                            return true;
                        }
                        tab.form_selection = None;
                    }
                    tab.form_cursor_pos = new_pos;
                }
                return true;
            }
            Key::Named(NamedKey::ArrowRight) => {
                let tab = self.tabs.active_tab_mut();
                if let Some(field) = tab.form_fields.get(idx) {
                    let old_pos = tab.form_cursor_pos;
                    let new_pos = if old_pos < field.value.len() {
                        field.value[old_pos..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| old_pos + i)
                            .unwrap_or(field.value.len())
                    } else {
                        field.value.len()
                    };
                    if shift_held {
                        let anchor = tab.form_selection.map(|(a, _)| a).unwrap_or(old_pos);
                        tab.form_selection = Some((anchor, new_pos));
                    } else {
                        // If there was a selection, move cursor to end of selection.
                        if let Some((a, b)) = tab.form_selection {
                            let max = a.max(b);
                            tab.form_cursor_pos = max;
                            tab.form_selection = None;
                            return true;
                        }
                        tab.form_selection = None;
                    }
                    tab.form_cursor_pos = new_pos;
                }
                return true;
            }
            Key::Named(NamedKey::Home) => {
                let tab = self.tabs.active_tab_mut();
                let old_pos = tab.form_cursor_pos;
                if shift_held {
                    let anchor = tab.form_selection.map(|(a, _)| a).unwrap_or(old_pos);
                    tab.form_selection = Some((anchor, 0));
                } else {
                    tab.form_selection = None;
                }
                tab.form_cursor_pos = 0;
                return true;
            }
            Key::Named(NamedKey::End) => {
                let tab = self.tabs.active_tab_mut();
                if let Some(field) = tab.form_fields.get(idx) {
                    let old_pos = tab.form_cursor_pos;
                    let end = field.value.len();
                    if shift_held {
                        let anchor = tab.form_selection.map(|(a, _)| a).unwrap_or(old_pos);
                        tab.form_selection = Some((anchor, end));
                    } else {
                        tab.form_selection = None;
                    }
                    tab.form_cursor_pos = end;
                }
                return true;
            }
            Key::Named(NamedKey::Tab) => {
                // Move focus to the next focusable form field, respecting tabindex.
                if !self.tabs.active_tab().form_fields.is_empty() {
                    let next = self.next_tab_field(idx);
                    if let Some(next_idx) = next {
                        let tab = self.tabs.active_tab_mut();
                        tab.focused_field = Some(next_idx);
                        tab.form_cursor_pos = tab.form_fields[next_idx].value.len();
                        tab.form_selection = None;
                    }
                }
                return true;
            }
            _ => {
                if let Some(text) = &event.text {
                    let s = text.as_str();
                    if !s.is_empty() && s.chars().all(|c| !c.is_control()) {
                        // Delete any existing selection first.
                        self.delete_form_selection();
                        let tab = self.tabs.active_tab_mut();
                        if let Some(field) = tab.form_fields.get_mut(idx) {
                            // Enforce maxlength.
                            if let Some(maxlen) = field.maxlength {
                                if field.value.len() + s.len() > maxlen {
                                    return true; // don't add more text
                                }
                            }
                            field.value.insert_str(tab.form_cursor_pos, s);
                            tab.form_cursor_pos += s.len();
                            tab.form_selection = None;
                        }
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Submit the form that the field at `trigger_idx` belongs to.
    ///
    /// Collects all form fields sharing the same `form_action` and builds
    /// a query string (GET) or POST body from their name/value pairs.
    /// Performs HTML5 validation before submission.
    fn submit_form(&mut self, trigger_idx: usize) {
        let tab = self.tabs.active_tab();
        let trigger = match tab.form_fields.get(trigger_idx) {
            Some(f) => f.clone(),
            None => return,
        };

        let form_action = trigger.form_action.clone();
        let form_method = trigger.form_method.clone();
        let form_enctype = trigger.form_enctype.clone();

        // Validate all fields in this form before submission.
        let mut validation_errors = Vec::new();
        for field in &tab.form_fields {
            if field.form_action == form_action && !field.name.is_empty() {
                if let Some(error) = field.validate() {
                    validation_errors.push(format!("{}: {}", field.name, error));
                }
            }
        }
        if !validation_errors.is_empty() {
            warn!("Form validation failed: {:?}", validation_errors);
            // TODO: Show validation errors visually.
            return;
        }

        // Collect all fields that belong to the same form (same action URL).
        let fields: Vec<(String, String)> = self.tabs.active_tab().form_fields.iter()
            .filter(|f| f.form_action == form_action && !f.name.is_empty())
            .filter(|f| !matches!(f.field_type.as_str(), "submit" | "button" | "reset"))
            .filter_map(|f| {
                match f.field_type.as_str() {
                    "checkbox" => {
                        if f.checked {
                            Some((f.name.clone(), if f.value.is_empty() { "on".to_string() } else { f.value.clone() }))
                        } else {
                            None // unchecked checkboxes are not submitted
                        }
                    }
                    "radio" => {
                        if f.checked {
                            Some((f.name.clone(), f.value.clone()))
                        } else {
                            None // unselected radios are not submitted
                        }
                    }
                    "file" | "hidden" => Some((f.name.clone(), f.value.clone())),
                    _ => Some((f.name.clone(), f.value.clone())),
                }
            })
            .collect();

        // Resolve form action URL against current page URL.
        let base_url = &self.url_bar_text;
        let action_url = if form_action.is_empty() {
            base_url.clone()
        } else if form_action.starts_with("http://") || form_action.starts_with("https://") {
            form_action.clone()
        } else if let Ok(base) = url::Url::parse(base_url) {
            base.join(&form_action).map(|u: url::Url| u.to_string()).unwrap_or(form_action.clone())
        } else {
            form_action.clone()
        };

        if form_method == "get" {
            // Build query string and navigate.
            let query = fields.iter()
                .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            let nav_url = if action_url.contains('?') {
                format!("{action_url}&{query}")
            } else {
                format!("{action_url}?{query}")
            };
            info!(url = %nav_url, "Form GET submission");
            self.navigate_in_place(&nav_url);
        } else if form_enctype == "multipart/form-data" {
            // POST submission with multipart form data.
            let boundary = format!("----NovaFormBoundary{:x}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis());
            let mut body = Vec::new();
            for (key, value) in &fields {
                body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n").as_bytes()
                );
            }
            body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
            let content_type = format!("multipart/form-data; boundary={boundary}");
            info!(url = %action_url, "Form POST (multipart) submission");
            self.navigate_post(&action_url, body, &content_type);
        } else {
            // POST submission with application/x-www-form-urlencoded.
            let encoded = fields.iter()
                .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            info!(url = %action_url, "Form POST (urlencoded) submission");
            self.navigate_post(
                &action_url,
                encoded.into_bytes(),
                "application/x-www-form-urlencoded",
            );
        }
    }

    /// Navigate via POST method.
    fn navigate_post(&mut self, url: &str, body: Vec<u8>, content_type: &str) {
        let core = self.core.clone();
        let viewport = self.viewport;
        let url_owned = url.to_string();
        let ct_owned = content_type.to_string();
        let handle = self.tokio_handle.clone();

        let result = std::thread::spawn(move || {
            handle.block_on(async {
                core.navigate_post(&url_owned, body, &ct_owned, viewport).await
            })
        })
        .join()
        .map_err(|_| nova_mod_api::NovaError::Internal("navigation thread panicked".into()))
        .and_then(|r| r);

        match result {
            Ok(TypedData::RenderCommands(cmds)) => {
                info!("POST navigation successful! {} render ops", cmds.ops.len());
                let tab = self.tabs.active_tab_mut();
                tab.hit_regions = Self::extract_hit_regions(&cmds);
                tab.form_fields = Self::extract_form_fields(&cmds);
                tab.focused_field = None;
                tab.form_cursor_pos = 0;
                tab.form_selection = None;
                tab.dropdown_open = false;
                tab.dropdown_hover_idx = None;
                tab.content_height = Self::compute_content_height(&cmds);
                tab.content_width = Self::compute_content_width(&cmds);
                tab.render_commands = cmds;
                tab.scroll_y = 0.0;
                tab.scroll_x = 0.0;
                tab.scroll_target_y = 0.0;
                tab.scroll_target_x = 0.0;
                tab.url = url.to_string();
                tab.title = url.to_string();

                self.url_bar_focused = false;
                self.url_bar_select_all = false;
                self.url_bar_text = url.to_string();
                self.url_bar_cursor = self.url_bar_text.len();
                if let Some(w) = &self.window {
                    w.set_title(&format!("NOVA - {}", self.url_bar_text));
                }
                self.apply_autofocus();
                self.request_redraw();
            }
            Ok(_) => {
                info!("POST navigation returned unexpected type");
                self.request_redraw();
            }
            Err(e) => {
                self.show_notification(&format!("POST navigation failed: {e}"));
                self.request_redraw();
            }
        }
    }

    // -- Find in Page -------------------------------------------------------

    /// Perform a search through all DrawText render ops and collect match positions.
    fn find_in_page(&mut self) {
        let tab = self.tabs.active_tab_mut();
        tab.find_matches.clear();
        tab.find_current = 0;

        if tab.find_query.is_empty() {
            return;
        }

        let query_lower = tab.find_query.to_lowercase();
        let approx_char_w_factor = 0.6;

        for op in &tab.render_commands.ops {
            if let RenderOp::DrawText {
                x,
                y,
                text,
                font_size,
                ..
            } = op
            {
                let text_lower = text.to_lowercase();
                let char_w = font_size * approx_char_w_factor;

                // Find all occurrences in this text.
                let mut start = 0;
                while let Some(pos) = text_lower[start..].find(&query_lower) {
                    let char_offset = start + pos;
                    let match_x = x + char_offset as f32 * char_w;
                    let match_w = query_lower.len() as f32 * char_w;

                    tab.find_matches.push(FindMatch {
                        x: match_x,
                        y: *y,
                        width: match_w,
                        height: font_size * 1.2,
                    });

                    start = char_offset + query_lower.len();
                }
            }
        }

        debug!(
            query = %tab.find_query,
            matches = tab.find_matches.len(),
            "find in page"
        );

        // Scroll to first match if any.
        let has_matches = !tab.find_matches.is_empty();
        if has_matches {
            self.scroll_to_find_match();
        }
    }

    /// Navigate to the next find match.
    fn find_next(&mut self) {
        let tab = self.tabs.active_tab_mut();
        if tab.find_matches.is_empty() {
            return;
        }
        tab.find_current = (tab.find_current + 1) % tab.find_matches.len();
        self.scroll_to_find_match();
    }

    /// Navigate to the previous find match.
    fn find_prev(&mut self) {
        let tab = self.tabs.active_tab_mut();
        if tab.find_matches.is_empty() {
            return;
        }
        if tab.find_current == 0 {
            tab.find_current = tab.find_matches.len() - 1;
        } else {
            tab.find_current -= 1;
        }
        self.scroll_to_find_match();
    }

    /// Scroll the viewport to show the current find match.
    fn scroll_to_find_match(&mut self) {
        let tab = self.tabs.active_tab_mut();
        let find_current = tab.find_current;
        if let Some(m) = tab.find_matches.get(find_current) {
            let page_viewport = (self.height as f32 - CHROME_HEIGHT).max(0.0);
            // Center the match vertically in the viewport (smooth scroll).
            tab.scroll_target_y = (m.y - page_viewport / 2.0).max(0.0);
        }
        self.clamp_scroll();
    }

    /// Draw the find bar overlay at the top-right of the window.
    fn draw_find_bar(&mut self) {
        let tab = self.tabs.active_tab();
        if !tab.find_bar_visible {
            return;
        }

        let find_query = tab.find_query.clone();
        let find_matches_len = tab.find_matches.len();
        let find_current = tab.find_current;

        let bar_w = 320.0_f32;
        let bar_h = 32.0_f32;
        let bar_x = (self.width as f32 - bar_w - 8.0).max(0.0);
        let bar_y = CHROME_HEIGHT + 4.0;

        // Background.
        let bg = Color { r: 1.0, g: 1.0, b: 1.0, a: 0.95 };
        self.framebuffer.fill_rect(bar_x, bar_y, bar_w, bar_h, bg);

        // Border.
        let border = Color { r: 0.7, g: 0.7, b: 0.7, a: 1.0 };
        self.framebuffer.stroke_rect(bar_x, bar_y, bar_w, bar_h, border, 1.0);

        // Query text.
        let text_color = Color { r: 0.1, g: 0.1, b: 0.1, a: 1.0 };
        let text_x = bar_x + 6.0;
        let text_y = bar_y + (bar_h - 12.0) / 2.0;
        self.framebuffer.draw_text(
            text_x,
            text_y,
            &find_query,
            12.0,
            text_color,
            None,
            None,
            None,
            None,
        );

        // Match count.
        let count_text = if find_matches_len == 0 {
            if find_query.is_empty() {
                String::new()
            } else {
                "0 matches".to_string()
            }
        } else {
            format!("{} of {}", find_current + 1, find_matches_len)
        };

        if !count_text.is_empty() {
            let count_x = bar_x + bar_w - 90.0;
            self.framebuffer.draw_text(
                count_x,
                text_y,
                &count_text,
                11.0,
                Color { r: 0.4, g: 0.4, b: 0.4, a: 1.0 },
                None,
                None,
                None,
                None,
            );
        }
    }

    /// Draw yellow highlight rectangles for all find matches, and orange for the current one.
    fn draw_find_highlights(&mut self) {
        let tab = self.tabs.active_tab();
        if !tab.find_bar_visible || tab.find_matches.is_empty() {
            return;
        }

        let scroll_x = tab.scroll_x;
        let scroll_y = tab.scroll_y;
        let find_current = tab.find_current;
        let matches: Vec<_> = tab.find_matches.iter().map(|m| (m.x, m.y, m.width, m.height)).collect();

        let highlight_color = Color { r: 1.0, g: 1.0, b: 0.0, a: 0.4 };
        let current_color = Color { r: 1.0, g: 0.65, b: 0.0, a: 0.5 };

        for (i, (mx, my, mw, mh)) in matches.iter().enumerate() {
            let sx = mx - scroll_x;
            let sy = my + CHROME_HEIGHT - scroll_y;
            let color = if i == find_current {
                current_color
            } else {
                highlight_color
            };
            self.framebuffer.fill_rect(sx, sy, *mw, *mh, color);
        }
    }

    // -- Zoom ---------------------------------------------------------------

    /// Set the zoom level and re-render.
    fn set_zoom(&mut self, level: f32) {
        self.zoom_level = level.clamp(0.25, 5.0);
        self.zoom_indicator_ticks = 60; // Show indicator for ~60 frames.
        info!(zoom = self.zoom_level, "zoom level changed");

        // Apply zoom by adjusting viewport dimensions and re-navigating.
        let effective_width = self.width as f32 / self.zoom_level;
        let effective_height = self.height as f32 / self.zoom_level;
        self.viewport = Viewport {
            width: effective_width,
            height: effective_height,
            scale_factor: self.zoom_level,
        };

        // Re-render the current page at the new zoom level.
        let url = self.url_bar_text.clone();
        self.navigate_in_place(&url);
    }

    /// Draw a zoom indicator overlay at the bottom-right of the window.
    fn draw_zoom_indicator(&mut self) {
        if self.zoom_indicator_ticks == 0 || (self.zoom_level - 1.0).abs() < 0.01 {
            return;
        }

        let text = format!("{}%", (self.zoom_level * 100.0).round() as u32);
        let indicator_w = 70.0_f32;
        let indicator_h = 28.0_f32;
        let ix = self.width as f32 - indicator_w - 12.0;
        let iy = self.height as f32 - indicator_h - 12.0;

        // Background.
        let bg = Color { r: 0.2, g: 0.2, b: 0.2, a: 0.8 };
        self.framebuffer.fill_rect(ix, iy, indicator_w, indicator_h, bg);

        // Text.
        let text_x = ix + (indicator_w - text.len() as f32 * 8.0) / 2.0;
        let text_y = iy + (indicator_h - 13.0) / 2.0;
        self.framebuffer.draw_text(
            text_x,
            text_y,
            &text,
            13.0,
            Color::WHITE,
            None,
            None,
            None,
            None,
        );

        self.zoom_indicator_ticks = self.zoom_indicator_ticks.saturating_sub(1);
    }

    // -- Bookmark indicator -------------------------------------------------

    // -- Tab bar rendering --------------------------------------------------

    /// Draw the tab bar below the URL bar.
    fn draw_tab_bar(&mut self) {
        let w = self.width as f32;
        let tab_bar_y = URL_BAR_HEIGHT;

        // Tab bar background.
        let tab_bar_bg = Color { r: 0.88, g: 0.88, b: 0.88, a: 1.0 };
        self.framebuffer.fill_rect(0.0, tab_bar_y, w, TAB_BAR_HEIGHT, tab_bar_bg);

        // Bottom border.
        let border_color = Color { r: 0.75, g: 0.75, b: 0.75, a: 1.0 };
        self.framebuffer.fill_rect(0.0, tab_bar_y + TAB_BAR_HEIGHT - 1.0, w, 1.0, border_color);

        let tab_count = self.tabs.tab_count();
        let active_idx = self.tabs.active_index();
        let tabs_data: Vec<(String, usize)> = self.tabs.tabs().iter().enumerate().map(|(i, t)| {
            let title = if t.title.len() > 20 {
                format!("{}...", &t.title[..17])
            } else if t.title.is_empty() {
                t.url.clone()
            } else {
                t.title.clone()
            };
            (title, i)
        }).collect();

        // Calculate tab width: max 200px, shrink to fit.
        let plus_btn_w = 30.0_f32;
        let available_w = w - plus_btn_w;
        let tab_w = (available_w / tab_count.max(1) as f32).min(200.0);
        let font_size = 12.0_f32;
        let close_btn_size = 14.0_f32;

        for (title, idx) in &tabs_data {
            let tx = *idx as f32 * tab_w;
            let ty = tab_bar_y;

            // Tab background.
            let bg = if *idx == active_idx {
                Color::WHITE
            } else {
                Color { r: 0.82, g: 0.82, b: 0.82, a: 1.0 }
            };
            self.framebuffer.fill_rect(tx, ty + 2.0, tab_w - 1.0, TAB_BAR_HEIGHT - 3.0, bg);

            // Tab separator (right edge).
            self.framebuffer.fill_rect(tx + tab_w - 1.0, ty + 4.0, 1.0, TAB_BAR_HEIGHT - 8.0, border_color);

            // Tab title text.
            let text_x = tx + 8.0;
            let text_y = ty + (TAB_BAR_HEIGHT - font_size) / 2.0;
            let max_text_w = tab_w - 8.0 - close_btn_size - 8.0;
            let max_chars = (max_text_w / (font_size * 0.6)).max(1.0) as usize;
            let display_title = if title.len() > max_chars {
                format!("{}...", &title[..max_chars.saturating_sub(3).max(1)])
            } else {
                title.clone()
            };
            let text_color = if *idx == active_idx {
                Color::BLACK
            } else {
                Color { r: 0.3, g: 0.3, b: 0.3, a: 1.0 }
            };
            self.framebuffer.draw_text(text_x, text_y, &display_title, font_size, text_color, None, None, None, None);

            // Close button (X) — only show if more than 1 tab.
            if tab_count > 1 {
                let close_x = tx + tab_w - close_btn_size - 6.0;
                let close_y = ty + (TAB_BAR_HEIGHT - close_btn_size) / 2.0;
                let close_color = Color { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
                self.framebuffer.draw_text(close_x, close_y, "\u{00D7}", close_btn_size, close_color, None, None, None, None);
            }
        }

        // "+" button to open a new tab.
        let plus_x = tabs_data.len() as f32 * tab_w;
        let plus_y = tab_bar_y + (TAB_BAR_HEIGHT - font_size) / 2.0;
        let plus_color = Color { r: 0.4, g: 0.4, b: 0.4, a: 1.0 };
        self.framebuffer.draw_text(plus_x + 8.0, plus_y, "+", 16.0, plus_color, Some(700), None, None, None);
    }

    /// Handle a click in the tab bar area. Returns `true` if the click was consumed.
    fn handle_tab_bar_click(&mut self, x: f64, y: f64) -> bool {
        let tab_bar_y = URL_BAR_HEIGHT as f64;
        let tab_bar_bottom = (URL_BAR_HEIGHT + TAB_BAR_HEIGHT) as f64;

        if y < tab_bar_y || y >= tab_bar_bottom {
            return false;
        }

        let tab_count = self.tabs.tab_count();
        let plus_btn_w = 30.0_f64;
        let available_w = self.width as f64 - plus_btn_w;
        let tab_w = (available_w / tab_count.max(1) as f64).min(200.0);

        let tabs_end_x = tab_count as f64 * tab_w;

        // Check if "+" button was clicked.
        if x >= tabs_end_x && x < tabs_end_x + plus_btn_w {
            info!("New tab button clicked");
            self.open_new_tab("about:blank");
            return true;
        }

        // Check which tab was clicked.
        if x < tabs_end_x {
            let tab_idx = (x / tab_w) as usize;
            if tab_idx < tab_count {
                // Check if close button was clicked.
                let close_btn_size = 14.0_f64;
                let close_x = tab_idx as f64 * tab_w + tab_w - close_btn_size - 6.0;
                let close_y = tab_bar_y + (TAB_BAR_HEIGHT as f64 - close_btn_size) / 2.0;

                if tab_count > 1 && x >= close_x && x < close_x + close_btn_size
                    && y >= close_y && y < close_y + close_btn_size
                {
                    info!(tab = tab_idx, "Close tab button clicked");
                    self.tabs.close_tab(tab_idx);
                    self.sync_url_bar_from_active_tab();
                    self.request_redraw();
                    return true;
                }

                // Switch to this tab.
                if tab_idx != self.tabs.active_index() {
                    info!(tab = tab_idx, "Switching to tab");
                    self.tabs.switch_to(tab_idx);
                    self.sync_url_bar_from_active_tab();
                    self.request_redraw();
                }
                return true;
            }
        }

        false
    }

    /// Open a new tab and navigate to the given URL.
    fn open_new_tab(&mut self, url: &str) {
        let commands = RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None };
        self.tabs.new_tab(url, commands);
        self.sync_url_bar_from_active_tab();

        // Navigate the new tab if it's not about:blank.
        if url != "about:blank" {
            self.navigate_in_place(url);
        } else {
            self.url_bar_focused = true;
            self.url_bar_select_all = true;
            self.request_redraw();
        }
    }

    /// Sync the URL bar text from the active tab.
    fn sync_url_bar_from_active_tab(&mut self) {
        let tab = self.tabs.active_tab();
        self.url_bar_text = tab.url.clone();
        self.url_bar_cursor = self.url_bar_text.len();
        self.url_bar_focused = false;
        self.url_bar_select_all = false;

        // Update window title.
        if let Some(w) = &self.window {
            w.set_title(&format!("NOVA - {}", self.url_bar_text));
        }
    }

    /// Draw a star icon in the URL bar (filled if bookmarked, outline otherwise).
    fn draw_bookmark_star(&mut self) {
        let star_x = self.width as f32 - URL_BAR_PADDING - 22.0;
        let star_y = (URL_BAR_HEIGHT - 14.0) / 2.0;

        let is_bookmarked = self.bookmark_store.is_bookmarked(&self.url_bar_text);
        let star_char = if is_bookmarked { "\u{2605}" } else { "\u{2606}" }; // filled vs outline star
        let color = if is_bookmarked {
            Color { r: 1.0, g: 0.8, b: 0.0, a: 1.0 } // gold
        } else {
            Color { r: 0.5, g: 0.5, b: 0.5, a: 1.0 } // gray
        };

        self.framebuffer.draw_text(star_x, star_y, star_char, 16.0, color, None, None, None, None);
    }

    /// Navigate to a new URL without closing the window.
    /// Fetches the page, re-runs the pipeline, and updates the display.
    fn navigate_in_place(&mut self, url: &str) {
        info!("Navigating in-place to: {url}");

        // Extract the fragment (anchor) from the URL, if any.
        let (base_url, fragment) = if let Some(hash_pos) = url.find('#') {
            (url[..hash_pos].to_string(), Some(url[hash_pos + 1..].to_string()))
        } else {
            (url.to_string(), None)
        };

        // Check if this is a same-page anchor navigation (only #fragment changed).
        let current_base = if let Some(hash_pos) = self.url_bar_text.find('#') {
            self.url_bar_text[..hash_pos].to_string()
        } else {
            self.url_bar_text.clone()
        };

        // If the URL is just "#section" or same base URL with different fragment,
        // scroll to the anchor without fetching.
        if (!base_url.is_empty() && base_url == current_base) || url.starts_with('#') {
            info!("Same-page anchor navigation to #{}", fragment.as_deref().unwrap_or(""));
            if let Some(ref frag) = fragment {
                self.scroll_to_anchor(frag);
            }
            // Update URL bar and history.
            let full_url = if url.starts_with('#') {
                format!("{}{}", current_base, url)
            } else {
                url.to_string()
            };
            self.url_bar_text = full_url.clone();
            self.url_bar_cursor = self.url_bar_text.len();
            self.tabs.active_tab_mut().history.push(&full_url);
            self.request_redraw();
            return;
        }

        let core = self.core.clone();
        let viewport = self.viewport;
        let url_owned = url.to_string();
        let handle = self.tokio_handle.clone();

        // Run navigation in a separate thread to avoid blocking the winit event loop
        // and to avoid the "block_on inside tokio" panic.
        let result = std::thread::spawn(move || {
            handle.block_on(async {
                core.navigate(&url_owned, viewport).await
            })
        })
        .join()
        .map_err(|_| nova_mod_api::NovaError::Internal("navigation thread panicked".into()))
        .and_then(|r| r);

        match result {
            Ok(TypedData::RenderCommands(cmds)) => {
                info!("In-place navigation successful! {} render ops", cmds.ops.len());

                // Check if JS used pushState/replaceState to set a different URL.
                let spa_push = cmds.spa_push_url.clone();
                let spa_replace = cmds.spa_replace_url.clone();
                let effective_url = spa_push.as_deref()
                    .or(spa_replace.as_deref())
                    .unwrap_or(url);

                let tab = self.tabs.active_tab_mut();
                tab.hit_regions = Self::extract_hit_regions(&cmds);
                tab.form_fields = Self::extract_form_fields(&cmds);
                tab.anchor_regions = Self::extract_anchor_regions(&cmds);
                tab.focused_field = None;
                tab.form_cursor_pos = 0;
                tab.form_selection = None;
                tab.dropdown_open = false;
                tab.dropdown_hover_idx = None;
                tab.content_height = Self::compute_content_height(&cmds);
                tab.content_width = Self::compute_content_width(&cmds);
                tab.render_commands = cmds;
                tab.scroll_y = 0.0;
                tab.scroll_x = 0.0;
                tab.scroll_target_y = 0.0;
                tab.scroll_target_x = 0.0;
                tab.url = effective_url.to_string();
                tab.title = effective_url.to_string();

                // Push or replace in navigation history depending on SPA mode.
                if spa_replace.is_some() {
                    tab.history.replace_current(effective_url);
                } else {
                    tab.history.push(effective_url);
                }

                self.url_bar_focused = false;
                self.url_bar_select_all = false;
                self.url_bar_text = effective_url.to_string();
                self.url_bar_cursor = self.url_bar_text.len();

                // Record in persistent history.
                self.persistent_history.record(effective_url, effective_url);

                // Update window title.
                if let Some(w) = &self.window {
                    w.set_title(&format!("NOVA - {}", self.url_bar_text));
                }

                // Scroll to anchor if URL has a fragment.
                if let Some(ref frag) = fragment {
                    self.scroll_to_anchor(frag);
                }

                self.apply_autofocus();
                self.request_redraw();
            }
            Ok(_) => {
                info!("Navigation returned unexpected type");
                self.request_redraw();
            }
            Err(e) => {
                self.show_notification(&format!("Navigation failed: {e}"));
                self.request_redraw();
            }
        }
    }

    /// Navigate back in history for the active tab.
    fn navigate_back(&mut self) {
        let url = {
            let tab = self.tabs.active_tab_mut();
            match tab.history.back() {
                Some(entry) => entry.url.clone(),
                None => {
                    info!("Already at the beginning of history");
                    return;
                }
            }
        };
        info!("Navigating back to: {url}");
        self.navigate_in_place_no_history(&url);
    }

    /// Navigate forward in history for the active tab.
    fn navigate_forward(&mut self) {
        let url = {
            let tab = self.tabs.active_tab_mut();
            match tab.history.forward() {
                Some(entry) => entry.url.clone(),
                None => {
                    info!("Already at the end of history");
                    return;
                }
            }
        };
        info!("Navigating forward to: {url}");
        self.navigate_in_place_no_history(&url);
    }

    /// Navigate in place without pushing to history (used by back/forward/reload).
    fn navigate_in_place_no_history(&mut self, url: &str) {
        let core = self.core.clone();
        let viewport = self.viewport;
        let url_owned = url.to_string();
        let handle = self.tokio_handle.clone();

        let result = std::thread::spawn(move || {
            handle.block_on(async {
                core.navigate(&url_owned, viewport).await
            })
        })
        .join()
        .map_err(|_| nova_mod_api::NovaError::Internal("navigation thread panicked".into()))
        .and_then(|r| r);

        match result {
            Ok(TypedData::RenderCommands(cmds)) => {
                info!("Navigation successful! {} render ops", cmds.ops.len());
                let tab = self.tabs.active_tab_mut();
                tab.hit_regions = Self::extract_hit_regions(&cmds);
                tab.form_fields = Self::extract_form_fields(&cmds);
                tab.anchor_regions = Self::extract_anchor_regions(&cmds);
                tab.focused_field = None;
                tab.form_cursor_pos = 0;
                tab.form_selection = None;
                tab.dropdown_open = false;
                tab.dropdown_hover_idx = None;
                tab.content_height = Self::compute_content_height(&cmds);
                tab.content_width = Self::compute_content_width(&cmds);
                tab.render_commands = cmds;
                tab.scroll_y = 0.0;
                tab.scroll_x = 0.0;
                tab.scroll_target_y = 0.0;
                tab.scroll_target_x = 0.0;
                tab.url = url.to_string();
                tab.title = url.to_string();

                self.url_bar_focused = false;
                self.url_bar_select_all = false;
                self.url_bar_text = url.to_string();
                self.url_bar_cursor = self.url_bar_text.len();

                self.persistent_history.record(url, url);

                if let Some(w) = &self.window {
                    w.set_title(&format!("NOVA - {}", self.url_bar_text));
                }

                self.apply_autofocus();
                self.request_redraw();
            }
            Ok(_) => {
                info!("Navigation returned unexpected type");
                self.request_redraw();
            }
            Err(e) => {
                self.show_notification(&format!("Navigation failed: {e}"));
                self.request_redraw();
            }
        }
    }

    /// Show a brief notification toast that auto-dismisses after ~2 seconds.
    fn show_notification(&mut self, text: &str) {
        info!("Notification: {text}");
        self.notification_text = text.to_string();
        self.notification_time = Some(Instant::now());
        self.request_redraw();
    }

    /// Draw the notification toast overlay (if active).
    fn draw_notification_overlay(&mut self) {
        let elapsed = match self.notification_time {
            Some(t) => t.elapsed().as_millis(),
            None => return,
        };
        if elapsed > 2000 || self.notification_text.is_empty() {
            // Auto-dismiss after 2 seconds.
            self.notification_text.clear();
            self.notification_time = None;
            return;
        }
        // Draw a semi-transparent background bar at the bottom of the window.
        let bar_h = 36.0_f32;
        let bar_y = self.height as f32 - bar_h;
        let bg_color = Color { r: 0.15, g: 0.15, b: 0.15, a: 0.85 };
        self.framebuffer.fill_rect(0.0, bar_y, self.width as f32, bar_h, bg_color);
        let text_color = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        self.framebuffer.draw_text(
            12.0,
            bar_y + 10.0,
            &self.notification_text,
            13.0,
            text_color,
            None,
            None,
            None,
            None,
        );
    }

    /// Save the current page as a PNG screenshot.
    fn save_screenshot(&mut self) {
        let w = self.width;
        let h = self.height;
        let pixels = self.framebuffer.pixels.clone();
        // Build a PNG file from the raw RGBA pixel data.
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let filename = format!("nova_screenshot_{timestamp}.png");
        let path = std::env::current_dir()
            .unwrap_or_default()
            .join(&filename);

        // Spawn encoding on a background thread so we don't block the event loop.
        let path_clone = path.clone();
        std::thread::spawn(move || {
            let file = match std::fs::File::create(&path_clone) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to create screenshot file: {e}");
                    return;
                }
            };
            let ref_file = std::io::BufWriter::new(file);
            let mut encoder = png::Encoder::new(ref_file, w, h);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&pixels);
            }
        });

        self.show_notification(&format!("Screenshot saved: {filename}"));
    }

    /// View the source HTML of the current page.
    fn view_source(&mut self) {
        if let Some(ref html) = self.page_source_html {
            let url = self.tabs.active_tab().url.clone();
            let escaped = html.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let source_page_html = format!(
                r#"<html><head><title>View Source: {url}</title>
<style>
body {{ margin: 0; padding: 16px; background: #1e1e1e; color: #d4d4d4; }}
pre {{ font-family: monospace; font-size: 13px; white-space: pre-wrap; word-wrap: break-word; line-height: 1.5; }}
</style></head>
<body><pre>{escaped}</pre></body></html>"#
            );
            let core = self.core.clone();
            let viewport = self.viewport;
            let handle = self.tokio_handle.clone();
            let source_url = format!("view-source:{url}");

            let result = std::thread::spawn(move || {
                handle.block_on(async {
                    core.pipeline.render_html_string(&source_page_html, viewport).await
                })
            })
            .join()
            .map_err(|_| nova_mod_api::NovaError::Internal("view-source thread panicked".into()))
            .and_then(|r| r);

            match result {
                Ok(TypedData::RenderCommands(cmds)) => {
                    info!("View source rendered, {} ops", cmds.ops.len());
                    self.tabs.new_tab(&source_url, cmds.clone());
                    let tab = self.tabs.active_tab_mut();
                    tab.hit_regions = Self::extract_hit_regions(&cmds);
                    tab.form_fields = Self::extract_form_fields(&cmds);
                    tab.anchor_regions = Self::extract_anchor_regions(&cmds);
                    tab.content_height = Self::compute_content_height(&cmds);
                    tab.content_width = Self::compute_content_width(&cmds);
                    tab.render_commands = cmds;

                    self.sync_url_bar_from_active_tab();
                    self.request_redraw();
                }
                _ => {
                    self.show_notification("Failed to render page source");
                }
            }
        } else {
            self.show_notification("No page source available");
        }
    }

    /// Scroll to an anchor element with the given `id`.
    fn scroll_to_anchor(&mut self, anchor_id: &str) {
        if anchor_id.is_empty() {
            // Empty fragment = scroll to top.
            let tab = self.tabs.active_tab_mut();
            tab.scroll_target_y = 0.0;
            tab.scroll_y = 0.0;
            self.clamp_scroll();
            return;
        }
        let tab = self.tabs.active_tab();
        for anchor in &tab.anchor_regions {
            if anchor.id == anchor_id {
                info!("Scrolling to anchor '{}' at y={}", anchor_id, anchor.y);
                let y = anchor.y;
                self.tabs.active_tab_mut().scroll_target_y = y;
                self.clamp_scroll();
                return;
            }
        }
        warn!("Anchor '{}' not found in page", anchor_id);
    }

    /// Reload the current page.
    fn reload(&mut self) {
        let url = self.url_bar_text.clone();
        info!("Reloading: {url}");
        self.navigate_in_place_no_history(&url);
    }
}

impl ApplicationHandler for BrowserWindow {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let notification_active = self.notification_time
            .map(|t| t.elapsed().as_millis() < 2000)
            .unwrap_or(false);

        // Smooth scroll animation: interpolate scroll_y/scroll_x toward
        // their targets each frame.
        let scroll_animating = self.tick_smooth_scroll();
        if scroll_animating {
            self.clamp_scroll();
            self.rebuild_framebuffer();
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            // Keep waking up for the next animation frame (~60fps).
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + std::time::Duration::from_millis(16),
            ));
            return;
        }

        // When a text field is focused, schedule periodic redraws for cursor blinking.
        if self.tabs.active_tab().focused_field.is_some() {
            let tab = self.tabs.active_tab_mut();
            let elapsed = tab.cursor_blink_time.elapsed();
            if elapsed.as_millis() >= 500 {
                tab.cursor_visible = !tab.cursor_visible;
                tab.cursor_blink_time = Instant::now();
                self.rebuild_framebuffer();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            // Wake up again in 500ms for the next blink toggle.
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + std::time::Duration::from_millis(500),
            ));
        } else if notification_active {
            // Wake up soon to dismiss the notification.
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + std::time::Duration::from_millis(100),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(LogicalSize::new(self.width, self.height));

        match event_loop.create_window(attrs) {
            Ok(window) => {
                let window = Arc::new(window);
                self.window = Some(window.clone());

                // Initialize GPU synchronously using pollster.
                if let Err(e) = pollster::block_on(self.init_gpu(window.clone())) {
                    error!("GPU init failed: {e}");
                    event_loop.exit();
                    return;
                }

                // Build the initial framebuffer with URL bar + page content.
                self.rebuild_framebuffer();
                window.request_redraw();
            }
            Err(e) => {
                error!("Failed to create window: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("Window closed");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                self.render_frame();
            }
            WindowEvent::Resized(new_size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.config.width = new_size.width.max(1);
                    gpu.config.height = new_size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);

                    self.width = gpu.config.width;
                    self.height = gpu.config.height;

                    // Re-clamp scroll and re-render at the new size.
                    self.clamp_scroll();
                    self.rebuild_framebuffer();

                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            // ── Mouse wheel scrolling ──────────────────────────────
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * SCROLL_STEP, -y * SCROLL_STEP),
                    MouseScrollDelta::PixelDelta(pos) => (-pos.x as f32, -pos.y as f32),
                };

                let tab = self.tabs.active_tab_mut();
                let mut changed = false;
                if dy.abs() > 0.01 {
                    tab.scroll_target_y += dy;
                    changed = true;
                }
                if dx.abs() > 0.01 {
                    tab.scroll_target_x += dx;
                    changed = true;
                }
                if changed {
                    self.clamp_scroll();
                    self.request_redraw();
                }
            }

            // ── Keyboard input ─────────────────────────────────────
            WindowEvent::KeyboardInput { event, .. } => {
                // Detect modifier keys from the event.
                let is_ctrl = event.state == ElementState::Pressed
                    && (event.logical_key == Key::Named(NamedKey::Control));
                let _ = is_ctrl; // suppress warning

                // Check for Ctrl+key shortcuts FIRST (global shortcuts).
                if event.state == ElementState::Pressed {
                    let key_str = match &event.logical_key {
                        Key::Character(c) => Some(c.as_str()),
                        _ => None,
                    };

                    // Helper: detect Ctrl modifier from the event text.
                    let is_ctrl_combo = event
                        .text
                        .as_ref()
                        .map(|t| t.as_str().chars().next().is_some_and(|c| c.is_control()))
                        .unwrap_or(false);

                    // Ctrl+T: New tab.
                    if key_str == Some("t") && is_ctrl_combo {
                        info!("Ctrl+T: Opening new tab");
                        self.open_new_tab("about:blank");
                        return;
                    }

                    // Ctrl+W: Close current tab.
                    if key_str == Some("w") && is_ctrl_combo {
                        if self.tabs.tab_count() > 1 {
                            let idx = self.tabs.active_index();
                            info!(tab = idx, "Ctrl+W: Closing tab");
                            self.tabs.close_tab(idx);
                            self.sync_url_bar_from_active_tab();
                            self.request_redraw();
                        }
                        return;
                    }

                    // Ctrl+Tab: Next tab / Ctrl+Shift+Tab: Previous tab.
                    if event.logical_key == Key::Named(NamedKey::Tab) && is_ctrl_combo {
                        // Ctrl+Shift+Tab is detected by checking if the text
                        // is a backtab or if shift is held. However, winit
                        // does not provide modifier state directly.
                        // We'll handle Ctrl+Shift+Tab via the separate
                        // Shift+Tab detection below.
                        self.tabs.next_tab();
                        self.sync_url_bar_from_active_tab();
                        self.request_redraw();
                        return;
                    }

                    // Ctrl+1-9: Switch to tab by index.
                    if is_ctrl_combo {
                        let tab_num = match key_str {
                            Some("1") => Some(0),
                            Some("2") => Some(1),
                            Some("3") => Some(2),
                            Some("4") => Some(3),
                            Some("5") => Some(4),
                            Some("6") => Some(5),
                            Some("7") => Some(6),
                            Some("8") => Some(7),
                            Some("9") => Some(self.tabs.tab_count().saturating_sub(1)), // Ctrl+9 = last tab
                            _ => None,
                        };
                        if let Some(idx) = tab_num {
                            if idx < self.tabs.tab_count() {
                                self.tabs.switch_to(idx);
                                self.sync_url_bar_from_active_tab();
                                self.request_redraw();
                                return;
                            }
                        }
                    }

                    // Ctrl+Shift+Tab: Previous tab (detected via backtab).
                    if event.logical_key == Key::Named(NamedKey::Tab) {
                        // If we get here and the Tab key has control text, it
                        // may be Ctrl+Shift+Tab. We handle it as prev tab.
                        // (This is a best-effort; actual Ctrl+Tab was handled above.)
                    }

                    // Ctrl+F: Find in page.
                    if key_str == Some("f") && !event.repeat && is_ctrl_combo {
                        let tab = self.tabs.active_tab_mut();
                        tab.find_bar_visible = !tab.find_bar_visible;
                        if !tab.find_bar_visible {
                            tab.find_query.clear();
                            tab.find_matches.clear();
                        }
                        self.request_redraw();
                        return;
                    }

                    // Ctrl+D: Toggle bookmark.
                    if key_str == Some("d") && is_ctrl_combo {
                        let url = self.url_bar_text.clone();
                        self.bookmark_store.toggle(&url, &url);
                        self.request_redraw();
                        return;
                    }

                    // Ctrl+H: Log history (future: open history panel).
                    if key_str == Some("h") && is_ctrl_combo {
                        let entries = self.persistent_history.all();
                        info!("History ({} entries):", entries.len());
                        for (i, e) in entries.iter().take(20).enumerate() {
                            info!("  [{}] {} — {}", i + 1, e.url, e.title);
                        }
                        return;
                    }

                    // Ctrl+= or Ctrl++: Zoom in.
                    if (key_str == Some("=") || key_str == Some("+")) && is_ctrl_combo {
                        let new_zoom = self.zoom_level + 0.1;
                        self.set_zoom(new_zoom);
                        return;
                    }

                    // Ctrl+-: Zoom out.
                    if key_str == Some("-") && is_ctrl_combo {
                        let new_zoom = self.zoom_level - 0.1;
                        self.set_zoom(new_zoom);
                        return;
                    }

                    // Ctrl+0: Reset zoom.
                    if key_str == Some("0") && is_ctrl_combo {
                        self.set_zoom(1.0);
                        return;
                    }

                    // Ctrl+L: Focus the URL bar and select all text.
                    if key_str == Some("l") && is_ctrl_combo {
                        self.url_bar_focused = true;
                        self.url_bar_select_all = true;
                        self.url_bar_cursor = self.url_bar_text.len();
                        // Unfocus any form field.
                        let tab = self.tabs.active_tab_mut();
                        tab.focused_field = None;
                        tab.form_selection = None;
                        self.request_redraw();
                        return;
                    }

                    // Ctrl+R: Reload current page.
                    if key_str == Some("r") && is_ctrl_combo {
                        self.reload();
                        return;
                    }

                    // Ctrl+P: Save page as PNG screenshot.
                    if key_str == Some("p") && is_ctrl_combo {
                        self.save_screenshot();
                        return;
                    }

                    // Ctrl+U: View page source.
                    if key_str == Some("u") && is_ctrl_combo {
                        self.view_source();
                        return;
                    }
                }

                // Navigation shortcuts (non-Ctrl).
                if event.state == ElementState::Pressed {
                    let alt_held = self.modifiers.state().alt_key();

                    // F5: Reload current page.
                    if event.logical_key == Key::Named(NamedKey::F5) {
                        self.reload();
                        return;
                    }

                    // F6: Focus the URL bar and select all text.
                    if event.logical_key == Key::Named(NamedKey::F6) {
                        self.url_bar_focused = true;
                        self.url_bar_select_all = true;
                        self.url_bar_cursor = self.url_bar_text.len();
                        let tab = self.tabs.active_tab_mut();
                        tab.focused_field = None;
                        tab.form_selection = None;
                        self.request_redraw();
                        return;
                    }

                    // Escape: Stop loading (no-op for now since navigation is synchronous).
                    if event.logical_key == Key::Named(NamedKey::Escape)
                        && !self.url_bar_focused
                        && !self.tabs.active_tab().find_bar_visible
                        && self.tabs.active_tab().focused_field.is_none()
                    {
                        info!("Escape pressed — stop loading (no-op: navigation is synchronous)");
                        return;
                    }

                    // Alt+Left: Navigate back.
                    if event.logical_key == Key::Named(NamedKey::ArrowLeft)
                        && alt_held
                        && !self.url_bar_focused
                        && self.tabs.active_tab().focused_field.is_none()
                    {
                        self.navigate_back();
                        return;
                    }

                    // Backspace (when not in a text field): Navigate back.
                    if event.logical_key == Key::Named(NamedKey::Backspace)
                        && !self.url_bar_focused
                        && !self.tabs.active_tab().find_bar_visible
                        && self.tabs.active_tab().focused_field.is_none()
                    {
                        self.navigate_back();
                        return;
                    }

                    // Alt+Right: Navigate forward.
                    if event.logical_key == Key::Named(NamedKey::ArrowRight)
                        && alt_held
                        && !self.url_bar_focused
                        && self.tabs.active_tab().focused_field.is_none()
                    {
                        self.navigate_forward();
                        return;
                    }

                    // Alt+Home: Navigate to home page.
                    if event.logical_key == Key::Named(NamedKey::Home)
                        && alt_held
                        && !self.url_bar_focused
                        && self.tabs.active_tab().focused_field.is_none()
                    {
                        info!("Alt+Home — navigating to home page");
                        self.navigate_in_place("about:blank");
                        return;
                    }
                }

                // Handle find bar input when it's visible.
                if self.tabs.active_tab().find_bar_visible && event.state == ElementState::Pressed {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            let tab = self.tabs.active_tab_mut();
                            tab.find_bar_visible = false;
                            tab.find_query.clear();
                            tab.find_matches.clear();
                            self.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::Enter) => {
                            self.find_next();
                            self.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::Backspace) => {
                            self.tabs.active_tab_mut().find_query.pop();
                            self.find_in_page();
                            self.request_redraw();
                            return;
                        }
                        _ => {
                            if let Some(text) = &event.text {
                                let s = text.as_str();
                                if !s.is_empty() && s.chars().all(|c| !c.is_control()) {
                                    self.tabs.active_tab_mut().find_query.push_str(s);
                                    self.find_in_page();
                                    self.request_redraw();
                                    return;
                                }
                            }
                        }
                    }
                }

                // First try the URL bar.
                if self.url_bar_focused {
                    self.handle_url_bar_key(&event);
                    self.request_redraw();
                    return;
                }

                // Handle text input for focused form fields.
                if self.tabs.active_tab().focused_field.is_some() && event.state == ElementState::Pressed {
                    if self.handle_form_field_key(&event) {
                        self.request_redraw();
                        return;
                    }
                }

                // Page scrolling (only when URL bar is not focused).
                // Updates scroll_target for smooth interpolation; the actual
                // scroll_y/scroll_x values are advanced in tick_smooth_scroll.
                if event.state == ElementState::Pressed {
                    let page_viewport = (self.height as f32 - CHROME_HEIGHT).max(0.0);
                    let tab = self.tabs.active_tab_mut();
                    let scrolled = match event.logical_key {
                        Key::Named(NamedKey::ArrowDown) => {
                            tab.scroll_target_y += ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            tab.scroll_target_y -= ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            tab.scroll_target_x += ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            tab.scroll_target_x -= ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::PageDown) => {
                            tab.scroll_target_y += page_viewport * PAGE_SCROLL_FRACTION;
                            true
                        }
                        Key::Named(NamedKey::PageUp) => {
                            tab.scroll_target_y -= page_viewport * PAGE_SCROLL_FRACTION;
                            true
                        }
                        Key::Named(NamedKey::Home) => {
                            tab.scroll_target_y = 0.0;
                            true
                        }
                        Key::Named(NamedKey::End) => {
                            tab.scroll_target_y = tab.content_height;
                            true
                        }
                        Key::Named(NamedKey::Space) => {
                            tab.scroll_target_y += page_viewport * PAGE_SCROLL_FRACTION;
                            true
                        }
                        _ => false,
                    };

                    if scrolled {
                        self.clamp_scroll();
                        self.request_redraw();
                    }
                }
            }

            // ── Cursor movement (for link hover / click handling) ──
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_x = position.x;
                self.cursor_y = position.y;
                self.update_cursor();

                // Update dropdown hover index if dropdown is open.
                {
                    let tab = self.tabs.active_tab();
                    if tab.dropdown_open {
                        let dd_idx = tab.dropdown_field_idx;
                        if let Some(field) = tab.form_fields.get(dd_idx) {
                            let fx = field.x - tab.scroll_x;
                            let fy = field.y + CHROME_HEIGHT - tab.scroll_y + field.height;
                            let w = field.width;
                            let option_h = 24.0_f32;
                            let total_h = option_h * field.options.len() as f32;

                            let mx = position.x as f32;
                            let my = position.y as f32;

                            if mx >= fx && mx < fx + w && my >= fy && my < fy + total_h {
                                let new_hover = ((my - fy) / option_h) as usize;
                                if tab.dropdown_hover_idx != Some(new_hover) {
                                    self.tabs.active_tab_mut().dropdown_hover_idx = Some(new_hover);
                                    self.request_redraw();
                                }
                            } else if tab.dropdown_hover_idx.is_some() {
                                self.tabs.active_tab_mut().dropdown_hover_idx = None;
                                self.request_redraw();
                            }
                        }
                    }
                }
            }

            // ── Mouse click ────────────────────────────────────────
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let cx = self.cursor_x;
                let cy = self.cursor_y;

                // Check if click is in the URL bar area.
                if (cy as f32) < URL_BAR_HEIGHT {
                    self.handle_mouse_click(cx, cy);
                    self.request_redraw();
                } else if self.handle_tab_bar_click(cx, cy) {
                    // Click was in the tab bar area and was handled.
                } else {
                    // Unfocus URL bar when clicking in the page area.
                    if self.url_bar_focused {
                        self.url_bar_focused = false;
                        self.url_bar_select_all = false;
                        self.request_redraw();
                    }

                    let tab = self.tabs.active_tab();
                    let page_x = cx as f32 + tab.scroll_x;
                    let page_y = (cy as f32 - CHROME_HEIGHT) + tab.scroll_y;

                    // Check if click is inside an open dropdown first.
                    {
                        let tab = self.tabs.active_tab();
                        if tab.dropdown_open {
                            let dd_idx = tab.dropdown_field_idx;
                            if let Some(field) = tab.form_fields.get(dd_idx) {
                                let fx = field.x - tab.scroll_x;
                                let fy = field.y + CHROME_HEIGHT - tab.scroll_y + field.height;
                                let w = field.width;
                                let option_h = 24.0_f32;
                                let total_h = option_h * field.options.len() as f32;

                                let win_x = cx as f32;
                                let win_y = cy as f32;

                                if win_x >= fx && win_x < fx + w && win_y >= fy && win_y < fy + total_h {
                                    // Clicked inside the dropdown — select the option.
                                    let option_idx = ((win_y - fy) / option_h) as usize;
                                    let tab = self.tabs.active_tab_mut();
                                    if let Some(field) = tab.form_fields.get_mut(dd_idx) {
                                        if option_idx < field.options.len() {
                                            let new_val = field.options[option_idx].0.clone();
                                            field.value = new_val.clone();
                                            for opt in &mut field.options {
                                                opt.2 = opt.0 == new_val;
                                            }
                                            debug!(value = %new_val, "Select option chosen");
                                        }
                                    }
                                    tab.dropdown_open = false;
                                    tab.dropdown_hover_idx = None;
                                    self.request_redraw();
                                    return; // Don't process further clicks
                                }
                            }
                            // Clicked outside the dropdown — close it.
                            let tab = self.tabs.active_tab_mut();
                            tab.dropdown_open = false;
                            tab.dropdown_hover_idx = None;
                            self.request_redraw();
                        }
                    }

                    // Check for form field clicks — focus the field.
                    // Skip fields with pointer-events: none.
                    let mut clicked_field = None;
                    for (i, field) in self.tabs.active_tab().form_fields.iter().enumerate() {
                        if field.contains(page_x, page_y) && !field.pointer_events_none {
                            clicked_field = Some(i);
                            break;
                        }
                    }
                    if let Some(idx) = clicked_field {
                        let field_type = self.tabs.active_tab().form_fields.get(idx)
                            .map(|f| f.field_type.clone())
                            .unwrap_or_default();

                        match field_type.as_str() {
                            "submit" | "button" => {
                                self.submit_form(idx);
                            }
                            "checkbox" => {
                                // Toggle checked state.
                                if let Some(field) = self.tabs.active_tab_mut().form_fields.get_mut(idx) {
                                    field.checked = !field.checked;
                                }
                                self.request_redraw();
                            }
                            "radio" => {
                                // Select this radio, deselect siblings with same name.
                                let name = self.tabs.active_tab().form_fields.get(idx)
                                    .map(|f| f.name.clone())
                                    .unwrap_or_default();
                                let form_action = self.tabs.active_tab().form_fields.get(idx)
                                    .map(|f| f.form_action.clone())
                                    .unwrap_or_default();
                                for f in &mut self.tabs.active_tab_mut().form_fields {
                                    if f.field_type == "radio" && f.name == name
                                        && f.form_action == form_action
                                    {
                                        f.checked = false;
                                    }
                                }
                                if let Some(field) = self.tabs.active_tab_mut().form_fields.get_mut(idx) {
                                    field.checked = true;
                                }
                                self.request_redraw();
                            }
                            "select" => {
                                // Open the dropdown overlay.
                                let tab = self.tabs.active_tab_mut();
                                if tab.dropdown_open && tab.dropdown_field_idx == idx {
                                    // Already open — close it.
                                    tab.dropdown_open = false;
                                    tab.dropdown_hover_idx = None;
                                } else {
                                    tab.dropdown_open = true;
                                    tab.dropdown_field_idx = idx;
                                    tab.dropdown_hover_idx = None;
                                }
                                let tab = self.tabs.active_tab_mut();
                                tab.focused_field = Some(idx);
                                self.request_redraw();
                            }
                            "file" => {
                                // Open a native file picker dialog.
                                let picked = rfd::FileDialog::new().pick_file();
                                if let Some(path) = picked {
                                    let filename = path.file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_else(|| path.display().to_string());
                                    let tab = self.tabs.active_tab_mut();
                                    if let Some(field) = tab.form_fields.get_mut(idx) {
                                        field.value = path.display().to_string();
                                    }
                                    debug!(file = %filename, "File selected via file picker");
                                }
                                self.request_redraw();
                            }
                            "hidden" => {
                                // Hidden fields cannot be focused.
                            }
                            _ => {
                                // Focus the text field and position cursor.
                                let tab = self.tabs.active_tab_mut();
                                tab.focused_field = Some(idx);
                                tab.cursor_visible = true;
                                tab.cursor_blink_time = Instant::now();
                                // Position cursor based on click x within the field.
                                let field_screen_x = tab.form_fields[idx].x - tab.scroll_x + 4.0;
                                let approx_char_w = 14.0_f32 * 0.6;
                                let click_offset = (cx as f32 - field_screen_x).max(0.0);
                                let char_pos = (click_offset / approx_char_w).round() as usize;
                                let value_len = tab.form_fields[idx].value.len();
                                tab.form_cursor_pos = char_pos.min(value_len);
                                tab.dropdown_open = false;

                                // Double-click / triple-click detection.
                                let now = Instant::now();
                                let click_count = if let Some(last_time) = tab.last_field_click_time {
                                    if now.duration_since(last_time).as_millis() < 400 {
                                        tab.field_click_count + 1
                                    } else {
                                        1
                                    }
                                } else {
                                    1
                                };
                                tab.last_field_click_time = Some(now);
                                tab.field_click_count = click_count;

                                if click_count >= 3 {
                                    // Triple-click: select all text.
                                    let vlen = tab.form_fields[idx].value.len();
                                    tab.form_selection = Some((0, vlen));
                                    tab.form_cursor_pos = vlen;
                                    tab.field_click_count = 0; // Reset to avoid quad-click confusion.
                                } else if click_count == 2 {
                                    // Double-click: select the word under the cursor.
                                    let val = &tab.form_fields[idx].value;
                                    let cursor = tab.form_cursor_pos.min(val.len());
                                    // Find word boundaries.
                                    let word_start = val[..cursor]
                                        .rfind(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
                                        .map(|i| {
                                            // Move past the delimiter.
                                            val[i..].char_indices().nth(1).map(|(ci, _)| i + ci).unwrap_or(val.len())
                                        })
                                        .unwrap_or(0);
                                    let word_end = val[cursor..]
                                        .find(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
                                        .map(|i| cursor + i)
                                        .unwrap_or(val.len());
                                    tab.form_selection = Some((word_start, word_end));
                                    tab.form_cursor_pos = word_end;
                                } else {
                                    // Single click: clear selection.
                                    tab.form_selection = None;
                                }
                            }
                        }
                        self.request_redraw();
                    } else {
                        let tab = self.tabs.active_tab_mut();
                        if tab.focused_field.is_some() || tab.dropdown_open {
                            tab.focused_field = None;
                            tab.form_selection = None;
                            tab.dropdown_open = false;
                            tab.dropdown_hover_idx = None;
                            self.request_redraw();
                        }
                    }

                    // Check for link clicks — navigate in place.
                    if let Some((url, target)) = self.hit_test_with_target(cx, cy) {
                        let url = url.to_string();
                        let target = target.to_string();
                        if target == "_blank" {
                            info!(url = %url, "target=\"_blank\" — opening in new tab");
                            self.open_new_tab(&url);
                        } else {
                            info!(url = %url, "Link clicked — navigating");
                            self.navigate_in_place(&url);
                        }
                    }
                }
            }

            // ── Middle-click on links ─────────────────────────────────
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Middle,
                ..
            } => {
                let cx = self.cursor_x;
                let cy = self.cursor_y;
                if let Some((url, _target)) = self.hit_test_with_target(cx, cy) {
                    let url = url.to_string();
                    info!(url = %url, "Middle-click on link — opening in new tab");
                    self.open_new_tab(&url);
                }
            }

            // ── Modifier key state tracking ──────────────────────────
            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers;
            }

            _ => {}
        }
    }
}

/// Percent-encode a string for use in URL query parameters.
///
/// Encodes all characters except unreserved characters (A-Z, a-z, 0-9, `-`, `_`, `.`, `~`).
/// Spaces are encoded as `+` per the `application/x-www-form-urlencoded` spec.
fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push('%');
                result.push_str(&format!("{byte:02X}"));
            }
        }
    }
    result
}

/// Normalize user input from the URL bar into a navigable URL.
///
/// If the input looks like a URL (contains "://" or "." with no spaces,
/// or is an `about:` / `file:` / `data:` URL), treat it as a URL
/// (prepending `https://` if no scheme is present).
/// Otherwise, treat it as a search query and navigate to Google Search.
fn normalize_url_or_search(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Already has a scheme.
    if trimmed.contains("://") || trimmed.starts_with("about:") || trimmed.starts_with("data:") || trimmed.starts_with("file:") {
        return trimmed.to_string();
    }

    // Looks like a domain (contains a dot, no spaces).
    if trimmed.contains('.') && !trimmed.contains(' ') {
        return format!("https://{trimmed}");
    }

    // Localhost with optional port.
    if trimmed.starts_with("localhost") && !trimmed.contains(' ') {
        return format!("http://{trimmed}");
    }

    // Otherwise, treat as a search query.
    let encoded = url_encode(trimmed);
    format!("https://www.google.com/search?q={encoded}")
}

/// WGSL shader for blitting a texture to a fullscreen quad.
const BLIT_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // Fullscreen triangle pair (two triangles covering the screen).
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );

    var out: VertexOutput;
    out.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    out.uv = uvs[vertex_index];
    return out;
}

@group(0) @binding(0) var t_diffuse: texture_2d<f32>;
@group(0) @binding(1) var s_diffuse: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(t_diffuse, s_diffuse, in.uv);
}
"#;

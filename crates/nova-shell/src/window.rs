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
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
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

/// Height of the URL bar area in logical pixels.
const URL_BAR_HEIGHT: f32 = 40.0;
/// Font size for URL bar text.
const URL_BAR_FONT_SIZE: f32 = 14.0;
/// Padding inside the URL bar.
const URL_BAR_PADDING: f32 = 8.0;
/// Height of the text input field inside the URL bar.
const URL_INPUT_HEIGHT: f32 = 28.0;

/// Pixels scrolled per mouse wheel tick.
const SCROLL_STEP: f32 = 40.0;
/// Pixels scrolled per arrow key press.
const ARROW_SCROLL_STEP: f32 = 40.0;
/// Fraction of viewport height scrolled per Page Up/Down press.
const PAGE_SCROLL_FRACTION: f32 = 0.9;

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
    render_commands: RenderCommands,
    title: String,
    width: u32,
    height: u32,

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

    // -- Scrolling state --
    /// Current vertical scroll offset in pixels (0 = top of page).
    scroll_y: f32,
    /// Current horizontal scroll offset in pixels (0 = left of page).
    scroll_x: f32,
    /// Total height of the rendered content in pixels.
    content_height: f32,
    /// Total width of the rendered content in pixels.
    content_width: f32,

    // -- Form field interaction state --
    /// Interactive form field regions extracted from `RenderOp::FormField` ops.
    form_fields: Vec<FormFieldRegion>,
    /// Index of the currently focused form field, or None.
    focused_field: Option<usize>,
    /// Cursor position (character index) within the focused text field.
    form_cursor_pos: usize,
    /// Timestamp when the cursor last toggled visibility (for blinking).
    cursor_blink_time: Instant,
    /// Whether the cursor is currently visible (toggles every 500ms).
    cursor_visible: bool,

    // -- Select dropdown state --
    /// Whether a dropdown is open for a `<select>` field.
    dropdown_open: bool,
    /// Index of the form field whose dropdown is open.
    dropdown_field_idx: usize,
    /// Index of the currently hovered option in the dropdown.
    dropdown_hover_idx: Option<usize>,

    // -- Link interaction state --
    /// Clickable link regions extracted from `RenderOp::Link` ops.
    hit_regions: Vec<HitRegion>,
    /// URL of the last clicked link (for diagnostics / future navigation).
    last_clicked_url: Option<String>,

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

    // -- Find in Page (Ctrl+F) --
    /// Whether the find bar is visible.
    find_bar_visible: bool,
    /// Current search query text.
    find_query: String,
    /// All match positions as `(render_op_index, char_offset)`.
    find_matches: Vec<FindMatch>,
    /// Index of the current highlighted match.
    find_current: usize,

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
}

/// A match found during Find in Page.
#[derive(Debug, Clone)]
struct FindMatch {
    /// Y coordinate of the match in page coordinates.
    y: f32,
    /// X coordinate of the match.
    x: f32,
    /// Width of the matched text.
    width: f32,
    /// Height of the matched text line.
    height: f32,
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
        let content_height = Self::compute_content_height(&commands);
        let content_width = Self::compute_content_width(&commands);
        let layer_tree = Some(LayerTree::from_render_commands(&commands, width as f32, height as f32, URL_BAR_HEIGHT));
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

        Self {
            window: None,
            gpu: None,
            framebuffer: fb,
            render_commands: commands,
            title: title.to_string(),
            width,
            height,
            url_bar_text: initial_url.to_string(),
            url_bar_focused: false,
            url_bar_cursor: initial_url.len(),
            url_bar_select_all: false,
            cursor_x: 0.0,
            cursor_y: 0.0,
            pending_navigation: None,
            scroll_y: 0.0,
            scroll_x: 0.0,
            content_height,
            content_width,
            form_fields,
            focused_field: None,
            form_cursor_pos: 0,
            cursor_blink_time: Instant::now(),
            cursor_visible: true,
            dropdown_open: false,
            dropdown_field_idx: 0,
            dropdown_hover_idx: None,
            hit_regions,
            last_clicked_url: None,
            core,
            tokio_handle,
            viewport,
            layer_tree,
            content_dirty: false,
            find_bar_visible: false,
            find_query: String::new(),
            find_matches: Vec::new(),
            find_current: 0,
            zoom_level: 1.0,
            zoom_indicator_ticks: 0,
            bookmark_store,
            persistent_history,
            downloads_manager: DownloadsManager::new(),
            vello_backend,
            use_gpu_rendering: false, // Disabled: Vello text rendering not yet complete; software FreeType renderer used.
        }
    }

    // -- Hit region / content height helpers --------------------------------

    /// Extract `HitRegion`s from `RenderOp::Link` entries in the render commands.
    fn extract_hit_regions(commands: &RenderCommands) -> Vec<HitRegion> {
        commands
            .ops
            .iter()
            .filter_map(|op| {
                if let RenderOp::Link { x, y, width, height, url } = op {
                    Some(HitRegion {
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                        url: url.clone(),
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

    /// Clamp `scroll_y` to the valid range `[0, max_scroll]`.
    fn clamp_scroll(&mut self) {
        let page_viewport = (self.height as f32 - URL_BAR_HEIGHT).max(0.0);
        let max_scroll = (self.content_height - page_viewport).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
        // Clamp horizontal scroll.
        let max_scroll_x = (self.content_width - self.width as f32).max(0.0);
        self.scroll_x = self.scroll_x.clamp(0.0, max_scroll_x);
    }

    // -- Link interaction helpers -------------------------------------------

    /// Perform a hit-test at the given window coordinates, returning the URL
    /// of the link under the cursor (if any).
    ///
    /// Window coordinates are translated to page coordinates by subtracting
    /// the URL bar offset and adding the scroll offset.
    fn hit_test(&self, win_x: f64, win_y: f64) -> Option<&str> {
        // Only test if the cursor is below the URL bar.
        if (win_y as f32) <= URL_BAR_HEIGHT {
            return None;
        }

        let page_x = win_x as f32 + self.scroll_x;
        let page_y = (win_y as f32 - URL_BAR_HEIGHT) + self.scroll_y;

        for region in &self.hit_regions {
            if region.contains(page_x, page_y) {
                return Some(&region.url);
            }
        }
        None
    }

    /// Update the cursor icon based on whether the cursor is over a link or form field.
    fn update_cursor(&self) {
        if let Some(w) = &self.window {
            if self.hit_test(self.cursor_x, self.cursor_y).is_some() {
                w.set_cursor(CursorIcon::Pointer);
            } else if (self.cursor_y as f32) > URL_BAR_HEIGHT {
                // Check if cursor is over a text-like form field.
                let page_x = self.cursor_x as f32 + self.scroll_x;
                let page_y = (self.cursor_y as f32 - URL_BAR_HEIGHT) + self.scroll_y;
                let over_text_field = self.form_fields.iter().any(|f| {
                    f.contains(page_x, page_y)
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
                &self.render_commands.ops,
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
            self.framebuffer.render_scrolled(
                &self.render_commands,
                URL_BAR_HEIGHT,
                self.scroll_x,
                self.scroll_y,
                self.content_height,
            );
        }

        // Redraw form fields whose values have changed (user typed into them).
        self.draw_form_field_overlays();

        // Draw focus ring on the focused form field (if any).
        if let Some(idx) = self.focused_field {
            if let Some(field) = self.form_fields.get(idx) {
                let fx = field.x - self.scroll_x;
                let fy = field.y + URL_BAR_HEIGHT - self.scroll_y;
                let focus_color = Color { r: 0.26, g: 0.52, b: 0.96, a: 1.0 };
                self.framebuffer.stroke_rect(fx - 1.0, fy - 1.0, field.width + 2.0, field.height + 2.0, focus_color, 2.0);
            }
        }

        // Draw text cursor in the focused text field.
        self.draw_form_field_cursor();

        // Draw select dropdown overlay (on top of everything except URL bar).
        self.draw_select_dropdown();

        // Draw find-in-page highlights (before URL bar so they appear under it).
        self.draw_find_highlights();

        // Draw the URL bar on top (overwrites the top region, not affected by scroll).
        self.draw_url_bar();

        // Draw the bookmark star in the URL bar.
        self.draw_bookmark_star();

        // Draw find bar overlay.
        self.draw_find_bar();

        // Draw zoom indicator.
        self.draw_zoom_indicator();
    }

    /// Redraw form fields that the user has modified (typed text into).
    ///
    /// Since the original `RenderOp::FormField` ops contain the initial value
    /// from the HTML, we overlay the current value on top of the field's
    /// background when it has been edited.
    fn draw_form_field_overlays(&mut self) {
        // Collect field data first to avoid borrowing issues.
        let fields: Vec<_> = self.form_fields.iter().enumerate().map(|(i, f)| {
            (i, f.x, f.y, f.width, f.height, f.field_type.clone(), f.value.clone(),
             f.placeholder.clone(), f.checked, f.options.clone())
        }).collect();

        for (idx, x, y, w, h, field_type, value, placeholder, checked, options) in fields {
            let fx = x - self.scroll_x;
            let fy = y + URL_BAR_HEIGHT - self.scroll_y;
            let font_size = 14.0_f32; // Default form field font size.

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

                    let (display_text, text_color) = if value.is_empty() {
                        (placeholder.clone(), Color::rgb(0.6, 0.6, 0.6))
                    } else {
                        (value.clone(), Color::BLACK)
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
                "password" => {
                    let border_color = Color::rgb(0.6, 0.6, 0.6);
                    self.framebuffer.fill_rect(fx, fy, w, h, Color::WHITE);
                    self.framebuffer.fill_rect(fx, fy, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy + h - 1.0, w, 1.0, border_color);
                    self.framebuffer.fill_rect(fx, fy, 1.0, h, border_color);
                    self.framebuffer.fill_rect(fx + w - 1.0, fy, 1.0, h, border_color);

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
        let idx = match self.focused_field {
            Some(i) => i,
            None => return,
        };

        let field = match self.form_fields.get(idx) {
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
        let elapsed = self.cursor_blink_time.elapsed();
        if elapsed.as_millis() >= 500 {
            self.cursor_visible = !self.cursor_visible;
            self.cursor_blink_time = Instant::now();
        }

        if !self.cursor_visible {
            return;
        }

        let fx = field.x - self.scroll_x;
        let fy = field.y + URL_BAR_HEIGHT - self.scroll_y;
        let h = field.height;
        let font_size = 14.0_f32;

        // Compute cursor x position based on character count.
        let approx_char_w = font_size * 0.6;
        let display_len = if field.field_type == "password" {
            field.value.len() // Each char is one bullet
        } else {
            field.value.len()
        };
        let cursor_char_pos = self.form_cursor_pos.min(display_len);
        let cursor_x = fx + 4.0 + cursor_char_pos as f32 * approx_char_w;
        let cursor_y = fy + (h - font_size) / 2.0;

        let cursor_color = Color::BLACK;
        self.framebuffer.fill_rect(cursor_x, cursor_y, 1.5, font_size, cursor_color);
    }

    /// Draw a dropdown overlay for the currently open `<select>` field.
    fn draw_select_dropdown(&mut self) {
        if !self.dropdown_open {
            return;
        }

        let field = match self.form_fields.get(self.dropdown_field_idx) {
            Some(f) => f,
            None => {
                self.dropdown_open = false;
                return;
            }
        };

        let options = field.options.clone();
        if options.is_empty() {
            return;
        }

        let fx = field.x - self.scroll_x;
        let fy = field.y + URL_BAR_HEIGHT - self.scroll_y + field.height;
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
            if Some(i) == self.dropdown_hover_idx {
                let hover_bg = Color::rgb(0.26, 0.52, 0.96);
                self.framebuffer.fill_rect(fx + 1.0, oy, w - 2.0, option_h, hover_bg);
            } else if *val == current_value {
                // Selected item highlight.
                let sel_bg = Color::rgb(0.9, 0.93, 1.0);
                self.framebuffer.fill_rect(fx + 1.0, oy, w - 2.0, option_h, sel_bg);
            }

            // Option text.
            let text_color = if Some(i) == self.dropdown_hover_idx {
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
                let url = self.url_bar_text.clone();
                info!("URL bar: navigating to {url}");
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

    /// Handle keyboard input when a form field is focused.
    ///
    /// Returns `true` if the event was consumed.
    fn handle_form_field_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        let idx = match self.focused_field {
            Some(i) => i,
            None => return false,
        };

        // Reset cursor blink on any keypress.
        self.cursor_visible = true;
        self.cursor_blink_time = Instant::now();

        match &event.logical_key {
            Key::Named(NamedKey::Enter) => {
                // Submit the form when Enter is pressed in a text field.
                self.submit_form(idx);
                return true;
            }
            Key::Named(NamedKey::Escape) => {
                self.focused_field = None;
                self.dropdown_open = false;
                return true;
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(field) = self.form_fields.get_mut(idx) {
                    if self.form_cursor_pos > 0 && !field.value.is_empty() {
                        // Find the char boundary before cursor_pos.
                        let prev = field.value[..self.form_cursor_pos]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        field.value.remove(prev);
                        self.form_cursor_pos = prev;
                    }
                }
                return true;
            }
            Key::Named(NamedKey::Delete) => {
                if let Some(field) = self.form_fields.get_mut(idx) {
                    if self.form_cursor_pos < field.value.len() {
                        field.value.remove(self.form_cursor_pos);
                    }
                }
                return true;
            }
            Key::Named(NamedKey::ArrowLeft) => {
                if self.form_cursor_pos > 0 {
                    if let Some(field) = self.form_fields.get(idx) {
                        self.form_cursor_pos = field.value[..self.form_cursor_pos]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                    }
                }
                return true;
            }
            Key::Named(NamedKey::ArrowRight) => {
                if let Some(field) = self.form_fields.get(idx) {
                    if self.form_cursor_pos < field.value.len() {
                        self.form_cursor_pos = field.value[self.form_cursor_pos..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| self.form_cursor_pos + i)
                            .unwrap_or(field.value.len());
                    }
                }
                return true;
            }
            Key::Named(NamedKey::Home) => {
                self.form_cursor_pos = 0;
                return true;
            }
            Key::Named(NamedKey::End) => {
                if let Some(field) = self.form_fields.get(idx) {
                    self.form_cursor_pos = field.value.len();
                }
                return true;
            }
            Key::Named(NamedKey::Tab) => {
                // Move focus to the next focusable form field (skip hidden, submit, button).
                if !self.form_fields.is_empty() {
                    let total = self.form_fields.len();
                    let mut next = (idx + 1) % total;
                    let mut attempts = 0;
                    while attempts < total {
                        let ft = self.form_fields[next].field_type.as_str();
                        if !matches!(ft, "hidden" | "submit" | "button" | "reset") {
                            break;
                        }
                        next = (next + 1) % total;
                        attempts += 1;
                    }
                    self.focused_field = Some(next);
                    self.form_cursor_pos = self.form_fields[next].value.len();
                }
                return true;
            }
            _ => {
                if let Some(text) = &event.text {
                    let s = text.as_str();
                    if !s.is_empty() && s.chars().all(|c| !c.is_control()) {
                        if let Some(field) = self.form_fields.get_mut(idx) {
                            // Enforce maxlength.
                            if let Some(maxlen) = field.maxlength {
                                if field.value.len() + s.len() > maxlen {
                                    return true; // don't add more text
                                }
                            }
                            field.value.insert_str(self.form_cursor_pos, s);
                            self.form_cursor_pos += s.len();
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
        let trigger = match self.form_fields.get(trigger_idx) {
            Some(f) => f.clone(),
            None => return,
        };

        let form_action = trigger.form_action.clone();
        let form_method = trigger.form_method.clone();
        let form_enctype = trigger.form_enctype.clone();

        // Validate all fields in this form before submission.
        let mut validation_errors = Vec::new();
        for field in &self.form_fields {
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
        let fields: Vec<(String, String)> = self.form_fields.iter()
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
                self.hit_regions = Self::extract_hit_regions(&cmds);
                self.form_fields = Self::extract_form_fields(&cmds);
                self.focused_field = None;
                self.form_cursor_pos = 0;
                self.dropdown_open = false;
                self.dropdown_hover_idx = None;
                self.content_height = Self::compute_content_height(&cmds);
                self.content_width = Self::compute_content_width(&cmds);
                self.render_commands = cmds;
                self.scroll_y = 0.0;
                self.scroll_x = 0.0;
                self.url_bar_focused = false;
                self.url_bar_select_all = false;
                self.url_bar_text = url.to_string();
                self.url_bar_cursor = self.url_bar_text.len();
                if let Some(w) = &self.window {
                    w.set_title(&format!("NOVA - {}", self.url_bar_text));
                }
                self.request_redraw();
            }
            Ok(_) => {
                info!("POST navigation returned unexpected type");
                self.request_redraw();
            }
            Err(e) => {
                error!("POST navigation failed: {e}");
                self.request_redraw();
            }
        }
    }

    // -- Find in Page -------------------------------------------------------

    /// Perform a search through all DrawText render ops and collect match positions.
    fn find_in_page(&mut self) {
        self.find_matches.clear();
        self.find_current = 0;

        if self.find_query.is_empty() {
            return;
        }

        let query_lower = self.find_query.to_lowercase();
        let approx_char_w_factor = 0.6;

        for op in &self.render_commands.ops {
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

                    self.find_matches.push(FindMatch {
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
            query = %self.find_query,
            matches = self.find_matches.len(),
            "find in page"
        );

        // Scroll to first match if any.
        if !self.find_matches.is_empty() {
            self.scroll_to_find_match();
        }
    }

    /// Navigate to the next find match.
    fn find_next(&mut self) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_current = (self.find_current + 1) % self.find_matches.len();
        self.scroll_to_find_match();
    }

    /// Navigate to the previous find match.
    fn find_prev(&mut self) {
        if self.find_matches.is_empty() {
            return;
        }
        if self.find_current == 0 {
            self.find_current = self.find_matches.len() - 1;
        } else {
            self.find_current -= 1;
        }
        self.scroll_to_find_match();
    }

    /// Scroll the viewport to show the current find match.
    fn scroll_to_find_match(&mut self) {
        if let Some(m) = self.find_matches.get(self.find_current) {
            let page_viewport = (self.height as f32 - URL_BAR_HEIGHT).max(0.0);
            // Center the match vertically in the viewport.
            self.scroll_y = (m.y - page_viewport / 2.0).max(0.0);
            self.clamp_scroll();
        }
    }

    /// Draw the find bar overlay at the top-right of the window.
    fn draw_find_bar(&mut self) {
        if !self.find_bar_visible {
            return;
        }

        let bar_w = 320.0_f32;
        let bar_h = 32.0_f32;
        let bar_x = (self.width as f32 - bar_w - 8.0).max(0.0);
        let bar_y = URL_BAR_HEIGHT + 4.0;

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
            &self.find_query,
            12.0,
            text_color,
            None,
            None,
            None,
            None,
        );

        // Match count.
        let count_text = if self.find_matches.is_empty() {
            if self.find_query.is_empty() {
                String::new()
            } else {
                "0 matches".to_string()
            }
        } else {
            format!("{} of {}", self.find_current + 1, self.find_matches.len())
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
        if !self.find_bar_visible || self.find_matches.is_empty() {
            return;
        }

        let highlight_color = Color { r: 1.0, g: 1.0, b: 0.0, a: 0.4 };
        let current_color = Color { r: 1.0, g: 0.65, b: 0.0, a: 0.5 };

        for (i, m) in self.find_matches.iter().enumerate() {
            let sx = m.x - self.scroll_x;
            let sy = m.y + URL_BAR_HEIGHT - self.scroll_y;
            let color = if i == self.find_current {
                current_color
            } else {
                highlight_color
            };
            self.framebuffer.fill_rect(sx, sy, m.width, m.height, color);
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
                self.hit_regions = Self::extract_hit_regions(&cmds);
                self.form_fields = Self::extract_form_fields(&cmds);
                self.focused_field = None;
                self.form_cursor_pos = 0;
                self.dropdown_open = false;
                self.dropdown_hover_idx = None;
                self.content_height = Self::compute_content_height(&cmds);
                self.content_width = Self::compute_content_width(&cmds);
                self.render_commands = cmds;
                self.scroll_y = 0.0;
                self.scroll_x = 0.0;
                self.url_bar_focused = false;
                self.url_bar_select_all = false;
                self.url_bar_text = url.to_string();
                self.url_bar_cursor = self.url_bar_text.len();

                // Record in persistent history.
                self.persistent_history.record(url, url);

                // Update window title.
                if let Some(w) = &self.window {
                    w.set_title(&format!("NOVA - {}", self.url_bar_text));
                }

                self.request_redraw();
            }
            Ok(_) => {
                info!("Navigation returned unexpected type");
                self.request_redraw();
            }
            Err(e) => {
                error!("Navigation failed: {e}");
                self.request_redraw();
            }
        }
    }
}

impl ApplicationHandler for BrowserWindow {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // When a text field is focused, schedule periodic redraws for cursor blinking.
        if self.focused_field.is_some() {
            let elapsed = self.cursor_blink_time.elapsed();
            if elapsed.as_millis() >= 500 {
                self.cursor_visible = !self.cursor_visible;
                self.cursor_blink_time = Instant::now();
                self.rebuild_framebuffer();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            // Wake up again in 500ms for the next blink toggle.
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + std::time::Duration::from_millis(500),
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

                let mut changed = false;
                if dy.abs() > 0.01 {
                    self.scroll_y += dy;
                    changed = true;
                }
                if dx.abs() > 0.01 {
                    self.scroll_x += dx;
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

                    // Ctrl+F: Find in page.
                    if key_str == Some("f") && event.repeat == false {
                        // Check for ctrl via modifiers in the text field.
                        // winit doesn't give us modifiers directly on KeyboardInput,
                        // but Ctrl+F produces the character "\x06". Also, the
                        // `text` field will be empty or control char when Ctrl is held.
                        let is_ctrl_combo = event
                            .text
                            .as_ref()
                            .map(|t| t.as_str().chars().next().is_some_and(|c| c.is_control()))
                            .unwrap_or(false);

                        if is_ctrl_combo {
                            self.find_bar_visible = !self.find_bar_visible;
                            if !self.find_bar_visible {
                                self.find_query.clear();
                                self.find_matches.clear();
                            }
                            self.request_redraw();
                            return;
                        }
                    }

                    // Ctrl+D: Toggle bookmark.
                    if key_str == Some("d") {
                        let is_ctrl_combo = event
                            .text
                            .as_ref()
                            .map(|t| t.as_str().chars().next().is_some_and(|c| c.is_control()))
                            .unwrap_or(false);

                        if is_ctrl_combo {
                            let url = self.url_bar_text.clone();
                            self.bookmark_store.toggle(&url, &url);
                            self.request_redraw();
                            return;
                        }
                    }

                    // Ctrl+H: Log history (future: open history panel).
                    if key_str == Some("h") {
                        let is_ctrl_combo = event
                            .text
                            .as_ref()
                            .map(|t| t.as_str().chars().next().is_some_and(|c| c.is_control()))
                            .unwrap_or(false);

                        if is_ctrl_combo {
                            let entries = self.persistent_history.all();
                            info!("History ({} entries):", entries.len());
                            for (i, e) in entries.iter().take(20).enumerate() {
                                info!("  [{}] {} — {}", i + 1, e.url, e.title);
                            }
                            return;
                        }
                    }

                    // Ctrl+= or Ctrl++: Zoom in.
                    if key_str == Some("=") || key_str == Some("+") {
                        let is_ctrl_combo = event
                            .text
                            .as_ref()
                            .map(|t| t.as_str().chars().next().is_some_and(|c| c.is_control()))
                            .unwrap_or(false);

                        if is_ctrl_combo {
                            let new_zoom = self.zoom_level + 0.1;
                            self.set_zoom(new_zoom);
                            return;
                        }
                    }

                    // Ctrl+-: Zoom out.
                    if key_str == Some("-") {
                        let is_ctrl_combo = event
                            .text
                            .as_ref()
                            .map(|t| t.as_str().chars().next().is_some_and(|c| c.is_control()))
                            .unwrap_or(false);

                        if is_ctrl_combo {
                            let new_zoom = self.zoom_level - 0.1;
                            self.set_zoom(new_zoom);
                            return;
                        }
                    }

                    // Ctrl+0: Reset zoom.
                    if key_str == Some("0") {
                        let is_ctrl_combo = event
                            .text
                            .as_ref()
                            .map(|t| t.as_str().chars().next().is_some_and(|c| c.is_control()))
                            .unwrap_or(false);

                        if is_ctrl_combo {
                            self.set_zoom(1.0);
                            return;
                        }
                    }
                }

                // Handle find bar input when it's visible.
                if self.find_bar_visible && event.state == ElementState::Pressed {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.find_bar_visible = false;
                            self.find_query.clear();
                            self.find_matches.clear();
                            self.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::Enter) => {
                            self.find_next();
                            self.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::Backspace) => {
                            self.find_query.pop();
                            self.find_in_page();
                            self.request_redraw();
                            return;
                        }
                        _ => {
                            if let Some(text) = &event.text {
                                let s = text.as_str();
                                if !s.is_empty() && s.chars().all(|c| !c.is_control()) {
                                    self.find_query.push_str(s);
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
                if self.focused_field.is_some() && event.state == ElementState::Pressed {
                    if self.handle_form_field_key(&event) {
                        self.request_redraw();
                        return;
                    }
                }

                // Page scrolling (only when URL bar is not focused).
                if event.state == ElementState::Pressed {
                    let page_viewport = (self.height as f32 - URL_BAR_HEIGHT).max(0.0);
                    let scrolled = match event.logical_key {
                        Key::Named(NamedKey::ArrowDown) => {
                            self.scroll_y += ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            self.scroll_y -= ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            self.scroll_x += ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            self.scroll_x -= ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::PageDown) => {
                            self.scroll_y += page_viewport * PAGE_SCROLL_FRACTION;
                            true
                        }
                        Key::Named(NamedKey::PageUp) => {
                            self.scroll_y -= page_viewport * PAGE_SCROLL_FRACTION;
                            true
                        }
                        Key::Named(NamedKey::Home) => {
                            self.scroll_y = 0.0;
                            true
                        }
                        Key::Named(NamedKey::End) => {
                            self.scroll_y = self.content_height;
                            true
                        }
                        Key::Named(NamedKey::Space) => {
                            self.scroll_y += page_viewport * PAGE_SCROLL_FRACTION;
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
                if self.dropdown_open {
                    if let Some(field) = self.form_fields.get(self.dropdown_field_idx) {
                        let fx = field.x - self.scroll_x;
                        let fy = field.y + URL_BAR_HEIGHT - self.scroll_y + field.height;
                        let w = field.width;
                        let option_h = 24.0_f32;
                        let total_h = option_h * field.options.len() as f32;

                        let mx = position.x as f32;
                        let my = position.y as f32;

                        if mx >= fx && mx < fx + w && my >= fy && my < fy + total_h {
                            let new_hover = ((my - fy) / option_h) as usize;
                            if self.dropdown_hover_idx != Some(new_hover) {
                                self.dropdown_hover_idx = Some(new_hover);
                                self.request_redraw();
                            }
                        } else if self.dropdown_hover_idx.is_some() {
                            self.dropdown_hover_idx = None;
                            self.request_redraw();
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
                } else {
                    // Unfocus URL bar when clicking in the page area.
                    if self.url_bar_focused {
                        self.url_bar_focused = false;
                        self.url_bar_select_all = false;
                        self.request_redraw();
                    }

                    let page_x = cx as f32 + self.scroll_x;
                    let page_y = (cy as f32 - URL_BAR_HEIGHT) + self.scroll_y;

                    // Check if click is inside an open dropdown first.
                    if self.dropdown_open {
                        if let Some(field) = self.form_fields.get(self.dropdown_field_idx) {
                            let fx = field.x - self.scroll_x;
                            let fy = field.y + URL_BAR_HEIGHT - self.scroll_y + field.height;
                            let w = field.width;
                            let option_h = 24.0_f32;
                            let total_h = option_h * field.options.len() as f32;

                            let win_x = cx as f32;
                            let win_y = cy as f32;

                            if win_x >= fx && win_x < fx + w && win_y >= fy && win_y < fy + total_h {
                                // Clicked inside the dropdown — select the option.
                                let option_idx = ((win_y - fy) / option_h) as usize;
                                let dd_idx = self.dropdown_field_idx;
                                if let Some(field) = self.form_fields.get_mut(dd_idx) {
                                    if option_idx < field.options.len() {
                                        let new_val = field.options[option_idx].0.clone();
                                        field.value = new_val.clone();
                                        for opt in &mut field.options {
                                            opt.2 = opt.0 == new_val;
                                        }
                                        debug!(value = %new_val, "Select option chosen");
                                    }
                                }
                                self.dropdown_open = false;
                                self.dropdown_hover_idx = None;
                                self.request_redraw();
                                return; // Don't process further clicks
                            }
                        }
                        // Clicked outside the dropdown — close it.
                        self.dropdown_open = false;
                        self.dropdown_hover_idx = None;
                        self.request_redraw();
                    }

                    // Check for form field clicks — focus the field.
                    let mut clicked_field = None;
                    for (i, field) in self.form_fields.iter().enumerate() {
                        if field.contains(page_x, page_y) {
                            clicked_field = Some(i);
                            break;
                        }
                    }
                    if let Some(idx) = clicked_field {
                        let field_type = self.form_fields.get(idx)
                            .map(|f| f.field_type.clone())
                            .unwrap_or_default();

                        match field_type.as_str() {
                            "submit" | "button" => {
                                self.submit_form(idx);
                            }
                            "checkbox" => {
                                // Toggle checked state.
                                if let Some(field) = self.form_fields.get_mut(idx) {
                                    field.checked = !field.checked;
                                }
                                self.request_redraw();
                            }
                            "radio" => {
                                // Select this radio, deselect siblings with same name.
                                let name = self.form_fields.get(idx)
                                    .map(|f| f.name.clone())
                                    .unwrap_or_default();
                                let form_action = self.form_fields.get(idx)
                                    .map(|f| f.form_action.clone())
                                    .unwrap_or_default();
                                for f in &mut self.form_fields {
                                    if f.field_type == "radio" && f.name == name
                                        && f.form_action == form_action
                                    {
                                        f.checked = false;
                                    }
                                }
                                if let Some(field) = self.form_fields.get_mut(idx) {
                                    field.checked = true;
                                }
                                self.request_redraw();
                            }
                            "select" => {
                                // Open the dropdown overlay.
                                if self.dropdown_open && self.dropdown_field_idx == idx {
                                    // Already open — close it.
                                    self.dropdown_open = false;
                                    self.dropdown_hover_idx = None;
                                } else {
                                    self.dropdown_open = true;
                                    self.dropdown_field_idx = idx;
                                    self.dropdown_hover_idx = None;
                                }
                                self.focused_field = Some(idx);
                                self.request_redraw();
                            }
                            "hidden" => {
                                // Hidden fields cannot be focused.
                            }
                            _ => {
                                // Focus the text field and position cursor.
                                self.focused_field = Some(idx);
                                self.cursor_visible = true;
                                self.cursor_blink_time = Instant::now();
                                // Position cursor based on click x within the field.
                                let field_screen_x = self.form_fields[idx].x - self.scroll_x + 4.0;
                                let approx_char_w = 14.0_f32 * 0.6;
                                let click_offset = (cx as f32 - field_screen_x).max(0.0);
                                let char_pos = (click_offset / approx_char_w).round() as usize;
                                self.form_cursor_pos = char_pos.min(self.form_fields[idx].value.len());
                                self.dropdown_open = false;
                            }
                        }
                        self.request_redraw();
                    } else {
                        if self.focused_field.is_some() || self.dropdown_open {
                            self.focused_field = None;
                            self.dropdown_open = false;
                            self.dropdown_hover_idx = None;
                            self.request_redraw();
                        }
                    }

                    // Check for link clicks — navigate in place.
                    if let Some(url) = self.hit_test(cx, cy) {
                        let url = url.to_string();
                        info!(url = %url, "Link clicked — navigating");
                        self.navigate_in_place(&url);
                    }
                }
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

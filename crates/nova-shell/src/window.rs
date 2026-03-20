//! Window management — creates a GPU-accelerated window with wgpu.
//!
//! Opens a native window via winit, initializes a wgpu render surface,
//! and blits the software-rendered framebuffer to the screen.
//! Includes an interactive URL bar for navigation.

use std::sync::Arc;

use tracing::{error, info};
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
use nova_mod_api::{Color, RenderCommands, RenderOp, TypedData, Viewport};

use crate::history::HistoryStack;
use crate::renderer::Framebuffer;
use crate::tab::{Tab, TabManager};

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
/// Width of the back navigation button.
const BACK_BUTTON_WIDTH: f32 = 30.0;
/// Width of the forward navigation button.
const FORWARD_BUTTON_WIDTH: f32 = 30.0;
/// Total height of the chrome area (URL bar + tab bar).
const CHROME_HEIGHT: f32 = URL_BAR_HEIGHT + TAB_BAR_HEIGHT;

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
    /// X position in page coordinates.
    pub x: f32,
    /// Y position in page coordinates.
    pub y: f32,
    /// Width of the hit region.
    pub width: f32,
    /// Height of the hit region.
    pub height: f32,
    /// URL this region links to.
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
    /// X position in page coordinates.
    pub x: f32,
    /// Y position in page coordinates.
    pub y: f32,
    /// Width of the form field.
    pub width: f32,
    /// Height of the form field.
    pub height: f32,
    /// Current value of the field.
    pub value: String,
    /// Type of the field (e.g., "text", "password").
    pub field_type: String,
}

impl FormFieldRegion {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
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
    /// Manages all open tabs and the active tab.
    tab_manager: TabManager,

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

    // -- Form field interaction state --
    /// Index of the currently focused form field, or None.
    focused_field: Option<usize>,

    // -- Link interaction state --
    /// URL of the last clicked link (for diagnostics / future navigation).
    last_clicked_url: Option<String>,

    // -- Navigation --
    /// Reference to the core for in-place navigation.
    core: Arc<NovaCore>,
    /// Tokio runtime handle for async calls from the winit event loop.
    tokio_handle: tokio::runtime::Handle,
    /// Viewport dimensions for navigation.
    viewport: Viewport,
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
        let fb = Framebuffer::new(width, height);
        let tokio_handle = tokio::runtime::Handle::current();
        let viewport = Viewport {
            width: width as f32,
            height: height as f32,
            scale_factor: 1.0,
        };

        let initial_tab = Tab {
            id: 0,
            url: initial_url.to_string(),
            title: initial_url.to_string(),
            render_commands: commands,
            scroll_y: 0.0,
            scroll_x: 0.0,
            content_height,
            content_width,
            hit_regions,
            form_fields,
            history: HistoryStack::new(initial_url),
        };

        let tab_manager = TabManager::new(initial_tab);

        Self {
            window: None,
            gpu: None,
            framebuffer: fb,
            title: title.to_string(),
            width,
            height,
            tab_manager,
            url_bar_text: initial_url.to_string(),
            url_bar_focused: false,
            url_bar_cursor: initial_url.len(),
            url_bar_select_all: false,
            cursor_x: 0.0,
            cursor_y: 0.0,
            pending_navigation: None,
            focused_field: None,
            last_clicked_url: None,
            core,
            tokio_handle,
            viewport,
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
            if let RenderOp::FormField { x, y, width, height, value, field_type } = op {
                Some(FormFieldRegion {
                    x: *x, y: *y, width: *width, height: *height,
                    value: value.clone(), field_type: field_type.clone(),
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

    /// Clamp scroll offsets of the active tab to valid ranges.
    fn clamp_scroll(&mut self) {
        let tab = self.tab_manager.active_tab_mut();
        let page_viewport = (self.height as f32 - CHROME_HEIGHT).max(0.0);
        let max_scroll = (tab.content_height - page_viewport).max(0.0);
        tab.scroll_y = tab.scroll_y.clamp(0.0, max_scroll);
        let max_scroll_x = (tab.content_width - self.width as f32).max(0.0);
        tab.scroll_x = tab.scroll_x.clamp(0.0, max_scroll_x);
    }

    // -- Link interaction helpers -------------------------------------------

    /// Perform a hit-test at the given window coordinates, returning the URL
    /// of the link under the cursor (if any).
    ///
    /// Window coordinates are translated to page coordinates by subtracting
    /// the URL bar offset and adding the scroll offset.
    fn hit_test(&self, win_x: f64, win_y: f64) -> Option<&str> {
        // Only test if the cursor is below the chrome area.
        if (win_y as f32) <= CHROME_HEIGHT {
            return None;
        }

        let tab = self.tab_manager.active_tab();
        let page_x = win_x as f32 + tab.scroll_x;
        let page_y = (win_y as f32 - CHROME_HEIGHT) + tab.scroll_y;

        for region in &tab.hit_regions {
            if region.contains(page_x, page_y) {
                return Some(&region.url);
            }
        }
        None
    }

    /// Update the cursor icon based on whether the cursor is over a link.
    fn update_cursor(&self) {
        if let Some(w) = &self.window {
            if self.hit_test(self.cursor_x, self.cursor_y).is_some() {
                w.set_cursor(CursorIcon::Pointer);
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

    /// Render the full frame: URL bar + tab bar + scrolled page content + scrollbar.
    fn rebuild_framebuffer(&mut self) {
        self.framebuffer.reset(self.width, self.height);

        let tab = self.tab_manager.active_tab();

        // Render page content shifted down by the chrome area and by scroll offset.
        self.framebuffer.render_scrolled(
            &tab.render_commands,
            CHROME_HEIGHT,
            tab.scroll_x,
            tab.scroll_y,
            tab.content_height,
        );

        // Draw focus ring on the focused form field (if any).
        if let Some(idx) = self.focused_field {
            let tab = self.tab_manager.active_tab();
            if let Some(field) = tab.form_fields.get(idx) {
                let fx = field.x - tab.scroll_x;
                let fy = field.y + CHROME_HEIGHT - tab.scroll_y;
                let focus_color = Color { r: 0.26, g: 0.52, b: 0.96, a: 1.0 };
                self.framebuffer.stroke_rect(fx - 1.0, fy - 1.0, field.width + 2.0, field.height + 2.0, focus_color, 2.0);
            }
        }

        // Draw the chrome on top (URL bar + tab bar, not affected by scroll).
        self.draw_url_bar();
        self.draw_tab_bar();
    }

    /// Draw the URL bar chrome at the top of the framebuffer.
    ///
    /// Includes back/forward navigation buttons before the URL input field.
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

        // -- Back / Forward buttons --
        let can_back = self.tab_manager.active_tab().history.can_go_back();
        let can_fwd = self.tab_manager.active_tab().history.can_go_forward();

        let btn_y = (URL_BAR_HEIGHT - URL_INPUT_HEIGHT) / 2.0;
        let btn_h = URL_INPUT_HEIGHT;

        // Back button.
        let back_color = if can_back {
            Color { r: 0.2, g: 0.2, b: 0.2, a: 1.0 }
        } else {
            Color { r: 0.7, g: 0.7, b: 0.7, a: 1.0 }
        };
        self.framebuffer.draw_text(
            URL_BAR_PADDING + 6.0,
            btn_y + (btn_h - URL_BAR_FONT_SIZE) / 2.0,
            "\u{25C0}",
            URL_BAR_FONT_SIZE,
            back_color,
            None, None, None,
        );

        // Forward button.
        let fwd_color = if can_fwd {
            Color { r: 0.2, g: 0.2, b: 0.2, a: 1.0 }
        } else {
            Color { r: 0.7, g: 0.7, b: 0.7, a: 1.0 }
        };
        self.framebuffer.draw_text(
            URL_BAR_PADDING + BACK_BUTTON_WIDTH + 6.0,
            btn_y + (btn_h - URL_BAR_FONT_SIZE) / 2.0,
            "\u{25B6}",
            URL_BAR_FONT_SIZE,
            fwd_color,
            None, None, None,
        );

        // -- Input field rectangle --
        let nav_buttons_width = BACK_BUTTON_WIDTH + FORWARD_BUTTON_WIDTH;
        let input_x = URL_BAR_PADDING + nav_buttons_width;
        let input_y = (URL_BAR_HEIGHT - URL_INPUT_HEIGHT) / 2.0;
        let input_w = w - URL_BAR_PADDING * 2.0 - nav_buttons_width;

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
            .draw_text(text_x, text_y, &self.url_bar_text, URL_BAR_FONT_SIZE, text_color, None, None, None);

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

    /// Draw the tab bar between the URL bar and the page content.
    fn draw_tab_bar(&mut self) {
        let w = self.width as f32;
        let tab_bar_y = URL_BAR_HEIGHT;

        // Tab bar background.
        let tab_bg = Color { r: 0.88, g: 0.88, b: 0.88, a: 1.0 };
        self.framebuffer.fill_rect(0.0, tab_bar_y, w, TAB_BAR_HEIGHT, tab_bg);

        // Bottom border.
        let border_color = Color { r: 0.75, g: 0.75, b: 0.75, a: 1.0 };
        self.framebuffer.fill_rect(0.0, tab_bar_y + TAB_BAR_HEIGHT - 1.0, w, 1.0, border_color);

        let tab_count = self.tab_manager.tab_count();
        let active_idx = self.tab_manager.active_index();
        let max_tab_width: f32 = 180.0;
        let tab_font_size: f32 = 12.0;

        // Reserve space for the "+" button.
        let plus_btn_width: f32 = 30.0;
        let available_width = w - plus_btn_width;
        let tab_width = (available_width / tab_count as f32).min(max_tab_width);

        for (i, tab) in self.tab_manager.tabs().iter().enumerate() {
            let tx = i as f32 * tab_width;
            let ty = tab_bar_y;

            // Active tab is lighter.
            if i == active_idx {
                let active_bg = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
                self.framebuffer.fill_rect(tx, ty, tab_width, TAB_BAR_HEIGHT - 1.0, active_bg);
            }

            // Tab border (right side).
            let sep = Color { r: 0.75, g: 0.75, b: 0.75, a: 1.0 };
            self.framebuffer.fill_rect(tx + tab_width - 1.0, ty + 4.0, 1.0, TAB_BAR_HEIGHT - 8.0, sep);

            // Tab title (truncated).
            let title_text = if tab.title.len() > 20 {
                format!("{}...", &tab.title[..17])
            } else {
                tab.title.clone()
            };
            let text_color = Color { r: 0.13, g: 0.13, b: 0.13, a: 1.0 };
            let text_y = ty + (TAB_BAR_HEIGHT - tab_font_size) / 2.0;
            self.framebuffer.draw_text(
                tx + 6.0, text_y, &title_text, tab_font_size, text_color, None, None, None,
            );

            // Close button (x) — only if more than one tab.
            if tab_count > 1 {
                let close_x = tx + tab_width - 18.0;
                let close_color = Color { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
                self.framebuffer.draw_text(
                    close_x, text_y, "\u{00D7}", tab_font_size, close_color, None, None, None,
                );
            }
        }

        // "+" button to add a new tab.
        let plus_x = tab_count as f32 * tab_width;
        let plus_color = Color { r: 0.4, g: 0.4, b: 0.4, a: 1.0 };
        let plus_y = tab_bar_y + (TAB_BAR_HEIGHT - 14.0) / 2.0;
        self.framebuffer.draw_text(
            plus_x + 8.0, plus_y, "+", 14.0, plus_color, None, None, None,
        );
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

    /// Handle a mouse click in the URL bar area (including back/forward buttons).
    fn handle_mouse_click(&mut self, x: f64, y: f64) {
        let input_y_start = ((URL_BAR_HEIGHT - URL_INPUT_HEIGHT) / 2.0) as f64;
        let input_y_end = input_y_start + URL_INPUT_HEIGHT as f64;

        let nav_buttons_width = (BACK_BUTTON_WIDTH + FORWARD_BUTTON_WIDTH) as f64;
        let input_x_start = URL_BAR_PADDING as f64 + nav_buttons_width;

        // Check back button click.
        if y >= input_y_start && y <= input_y_end
            && x >= URL_BAR_PADDING as f64
            && x < URL_BAR_PADDING as f64 + BACK_BUTTON_WIDTH as f64
        {
            self.navigate_back();
            return;
        }

        // Check forward button click.
        if y >= input_y_start && y <= input_y_end
            && x >= URL_BAR_PADDING as f64 + BACK_BUTTON_WIDTH as f64
            && x < input_x_start
        {
            self.navigate_forward();
            return;
        }

        if y >= input_y_start && y <= input_y_end && x >= input_x_start {
            // Clicked inside the URL bar input field.
            if !self.url_bar_focused {
                self.url_bar_focused = true;
                self.url_bar_select_all = true;
                self.url_bar_cursor = self.url_bar_text.len();
            } else {
                self.url_bar_select_all = false;
                let text_x = input_x_start + 6.0;
                let approx_char_w = URL_BAR_FONT_SIZE as f64 * 0.6;
                let char_pos = ((x - text_x) / approx_char_w).round().max(0.0) as usize;
                self.url_bar_cursor = char_pos.min(self.url_bar_text.len());
            }
        } else {
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

    /// Navigate to a new URL without closing the window.
    /// Fetches the page, re-runs the pipeline, and updates the display.
    /// Pushes the new URL onto the active tab's history stack.
    fn navigate_in_place(&mut self, url: &str) {
        self.navigate_to_url(url, true);
    }

    /// Navigate to a URL, optionally pushing to history.
    ///
    /// When `push_history` is `true`, the URL is added to the tab's
    /// history stack (normal navigation). When `false`, it is used for
    /// back/forward navigation where the history position is already set.
    fn navigate_to_url(&mut self, url: &str, push_history: bool) {
        info!("Navigating to: {url} (push_history={push_history})");

        // Save current scroll position before navigating.
        {
            let tab = self.tab_manager.active_tab_mut();
            tab.history.current_mut().scroll_y = tab.scroll_y;
        }

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

                let hit_regions = Self::extract_hit_regions(&cmds);
                let form_fields = Self::extract_form_fields(&cmds);
                let content_height = Self::compute_content_height(&cmds);
                let content_width = Self::compute_content_width(&cmds);

                let tab = self.tab_manager.active_tab_mut();
                tab.hit_regions = hit_regions;
                tab.form_fields = form_fields;
                tab.content_height = content_height;
                tab.content_width = content_width;
                tab.render_commands = cmds;
                tab.scroll_y = 0.0;
                tab.scroll_x = 0.0;
                tab.url = url.to_string();
                tab.title = url.to_string();

                if push_history {
                    tab.history.push(url);
                }

                self.focused_field = None;
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
                info!("Navigation returned unexpected type");
                self.request_redraw();
            }
            Err(e) => {
                error!("Navigation failed: {e}");
                self.request_redraw();
            }
        }
    }

    /// Navigate back in the active tab's history.
    fn navigate_back(&mut self) {
        let url = {
            let tab = self.tab_manager.active_tab_mut();
            tab.history.current_mut().scroll_y = tab.scroll_y;
            tab.history.back().map(|e| e.url.clone())
        };
        if let Some(url) = url {
            info!("Navigating back to: {url}");
            self.navigate_to_url(&url, false);
            // Restore scroll position from history.
            let scroll_y = self.tab_manager.active_tab().history.current().scroll_y;
            self.tab_manager.active_tab_mut().scroll_y = scroll_y;
            self.clamp_scroll();
            self.request_redraw();
        }
    }

    /// Navigate forward in the active tab's history.
    fn navigate_forward(&mut self) {
        let url = {
            let tab = self.tab_manager.active_tab_mut();
            tab.history.current_mut().scroll_y = tab.scroll_y;
            tab.history.forward().map(|e| e.url.clone())
        };
        if let Some(url) = url {
            info!("Navigating forward to: {url}");
            self.navigate_to_url(&url, false);
            let scroll_y = self.tab_manager.active_tab().history.current().scroll_y;
            self.tab_manager.active_tab_mut().scroll_y = scroll_y;
            self.clamp_scroll();
            self.request_redraw();
        }
    }

    /// Open a new tab and navigate to a URL.
    fn open_new_tab(&mut self, url: &str) {
        info!("Opening new tab: {url}");
        let commands = RenderCommands { ops: vec![], fonts: vec![] };
        self.tab_manager.new_tab(url, commands);
        self.focused_field = None;

        // Navigate the new tab.
        self.navigate_to_url(url, false);
    }

    /// Handle a click in the tab bar area.
    fn handle_tab_bar_click(&mut self, x: f64, _y: f64) {
        let tab_count = self.tab_manager.tab_count();
        let max_tab_width: f32 = 180.0;
        let plus_btn_width: f32 = 30.0;
        let available_width = self.width as f32 - plus_btn_width;
        let tab_width = (available_width / tab_count as f32).min(max_tab_width);

        let click_x = x as f32;

        // Check if the "+" button was clicked.
        let plus_x = tab_count as f32 * tab_width;
        if click_x >= plus_x && click_x < plus_x + plus_btn_width {
            self.open_new_tab("http://example.com");
            return;
        }

        // Check which tab was clicked.
        for i in 0..tab_count {
            let tx = i as f32 * tab_width;
            if click_x >= tx && click_x < tx + tab_width {
                // Check if the close button was clicked.
                let close_x = tx + tab_width - 18.0;
                if tab_count > 1 && click_x >= close_x && click_x < close_x + 14.0 {
                    self.tab_manager.close_tab(i);
                    self.sync_from_active_tab();
                    self.request_redraw();
                    return;
                }

                // Switch to the clicked tab.
                if i != self.tab_manager.active_index() {
                    self.tab_manager.switch_to(i);
                    self.sync_from_active_tab();
                    self.request_redraw();
                }
                return;
            }
        }
    }

    /// Sync window state from the active tab (URL bar, etc.).
    fn sync_from_active_tab(&mut self) {
        let tab = self.tab_manager.active_tab();
        self.url_bar_text = tab.url.clone();
        self.url_bar_cursor = self.url_bar_text.len();
        self.url_bar_focused = false;
        self.url_bar_select_all = false;
        self.focused_field = None;

        if let Some(w) = &self.window {
            w.set_title(&format!("NOVA - {}", tab.url));
        }
    }
}

impl ApplicationHandler for BrowserWindow {
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
                let tab = self.tab_manager.active_tab_mut();
                if dy.abs() > 0.01 {
                    tab.scroll_y += dy;
                    changed = true;
                }
                if dx.abs() > 0.01 {
                    tab.scroll_x += dx;
                    changed = true;
                }
                if changed {
                    self.clamp_scroll();
                    self.request_redraw();
                }
            }

            // ── Keyboard input ─────────────────────────────────────
            WindowEvent::KeyboardInput { event, .. } => {
                // Check for modifier-based shortcuts first.
                if event.state == ElementState::Pressed {
                    let modifiers_state = event.physical_key;
                    let _ = modifiers_state; // used by match below

                    // Ctrl+T — new tab.
                    if event.logical_key == Key::Character("t".into())
                        && event.repeat == false
                    {
                        // Check if Ctrl is held via the text field: if Ctrl is held,
                        // event.text is usually None for letter keys.
                        if event.text.is_none() || event.text.as_ref().map(|t| t.as_str()) == Some("\x14") {
                            self.open_new_tab("http://example.com");
                            return;
                        }
                    }
                    // Ctrl+W — close current tab.
                    if event.logical_key == Key::Character("w".into())
                        && event.repeat == false
                    {
                        if event.text.is_none() || event.text.as_ref().map(|t| t.as_str()) == Some("\x17") {
                            let idx = self.tab_manager.active_index();
                            self.tab_manager.close_tab(idx);
                            self.sync_from_active_tab();
                            self.request_redraw();
                            return;
                        }
                    }
                    // Ctrl+Tab — next tab (note: Tab key with Ctrl).
                    if event.logical_key == Key::Named(NamedKey::Tab) {
                        // We can't easily detect Ctrl here in winit 0.30 without
                        // ModifiersState tracking, but Tab alone in page context
                        // can be used. We'll use the text presence heuristic.
                        if event.text.is_none() {
                            self.tab_manager.next_tab();
                            self.sync_from_active_tab();
                            self.request_redraw();
                            return;
                        }
                    }

                    // Alt+Left — back.
                    if event.logical_key == Key::Named(NamedKey::ArrowLeft)
                        && event.text.is_none()
                    {
                        self.navigate_back();
                        return;
                    }
                    // Alt+Right — forward.
                    if event.logical_key == Key::Named(NamedKey::ArrowRight)
                        && event.text.is_none()
                    {
                        self.navigate_forward();
                        return;
                    }
                }

                // URL bar input.
                if self.url_bar_focused {
                    self.handle_url_bar_key(&event);
                    self.request_redraw();
                    return;
                }

                // Backspace — navigate back (when URL bar not focused).
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Backspace)
                {
                    self.navigate_back();
                    return;
                }

                // Page scrolling (only when URL bar is not focused).
                if event.state == ElementState::Pressed {
                    let page_viewport = (self.height as f32 - CHROME_HEIGHT).max(0.0);
                    let tab = self.tab_manager.active_tab_mut();
                    let scrolled = match event.logical_key {
                        Key::Named(NamedKey::ArrowDown) => {
                            tab.scroll_y += ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            tab.scroll_y -= ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            tab.scroll_x += ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            tab.scroll_x -= ARROW_SCROLL_STEP;
                            true
                        }
                        Key::Named(NamedKey::PageDown) => {
                            tab.scroll_y += page_viewport * PAGE_SCROLL_FRACTION;
                            true
                        }
                        Key::Named(NamedKey::PageUp) => {
                            tab.scroll_y -= page_viewport * PAGE_SCROLL_FRACTION;
                            true
                        }
                        Key::Named(NamedKey::Home) => {
                            tab.scroll_y = 0.0;
                            true
                        }
                        Key::Named(NamedKey::End) => {
                            tab.scroll_y = tab.content_height;
                            true
                        }
                        Key::Named(NamedKey::Space) => {
                            tab.scroll_y += page_viewport * PAGE_SCROLL_FRACTION;
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
                }
                // Check if click is in the tab bar area.
                else if (cy as f32) < CHROME_HEIGHT {
                    self.handle_tab_bar_click(cx, cy);
                }
                else {
                    // Unfocus URL bar when clicking in the page area.
                    if self.url_bar_focused {
                        self.url_bar_focused = false;
                        self.url_bar_select_all = false;
                        self.request_redraw();
                    }

                    // Check for form field clicks — focus the field.
                    let tab = self.tab_manager.active_tab();
                    let page_x = cx as f32 + tab.scroll_x;
                    let page_y = (cy as f32 - CHROME_HEIGHT) + tab.scroll_y;
                    let mut clicked_field = None;
                    for (i, field) in tab.form_fields.iter().enumerate() {
                        if field.contains(page_x, page_y) {
                            clicked_field = Some(i);
                            break;
                        }
                    }
                    if let Some(idx) = clicked_field {
                        self.focused_field = Some(idx);
                        self.request_redraw();
                    } else if self.focused_field.is_some() {
                        self.focused_field = None;
                        self.request_redraw();
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

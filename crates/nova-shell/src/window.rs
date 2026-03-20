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
struct HitRegion {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    url: String,
}

impl HitRegion {
    /// Test whether a point (in page coordinates) falls inside this region.
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

/// A scrollable container region on the page.
#[derive(Debug, Clone)]
struct ScrollRegion {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    content_height: f32,
    scroll_offset: f32,
}

impl ScrollRegion {
    /// Test whether a point (in page coordinates) falls inside this region.
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }

    /// Clamp scroll offset to valid range.
    fn clamp_scroll(&mut self) {
        let max = (self.content_height - self.height).max(0.0);
        self.scroll_offset = self.scroll_offset.clamp(0.0, max);
    }
}

/// An interactive form field region on the page.
#[derive(Debug, Clone)]
struct FormFieldRegion {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    value: String,
    field_type: String,
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
    /// Per-container scroll regions (from `ScrollContainerStart` ops).
    scroll_regions: Vec<ScrollRegion>,

    // -- Form field interaction state --
    /// Interactive form field regions extracted from `RenderOp::FormField` ops.
    form_fields: Vec<FormFieldRegion>,
    /// Index of the currently focused form field, or None.
    focused_field: Option<usize>,

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
        let scroll_regions = Self::extract_scroll_regions(&commands);
        let content_height = Self::compute_content_height(&commands);
        let content_width = Self::compute_content_width(&commands);
        let fb = Framebuffer::new(width, height);
        let tokio_handle = tokio::runtime::Handle::current();
        let viewport = Viewport {
            width: width as f32,
            height: height as f32,
            scale_factor: 1.0,
        };

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
            scroll_regions,
            form_fields,
            focused_field: None,
            hit_regions,
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

    /// Extract scroll container regions from render commands.
    fn extract_scroll_regions(commands: &RenderCommands) -> Vec<ScrollRegion> {
        commands
            .ops
            .iter()
            .filter_map(|op| {
                if let RenderOp::ScrollContainerStart {
                    x,
                    y,
                    width,
                    height,
                    content_height,
                } = op
                {
                    Some(ScrollRegion {
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                        content_height: *content_height,
                        scroll_offset: 0.0,
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

    /// Render the full frame: URL bar + scrolled page content + scrollbar.
    fn rebuild_framebuffer(&mut self) {
        self.framebuffer.reset(self.width, self.height);

        // Render page content shifted down by the URL bar and by scroll offset.
        self.framebuffer.render_scrolled(
            &self.render_commands,
            URL_BAR_HEIGHT,
            self.scroll_x,
            self.scroll_y,
            self.content_height,
        );

        // Draw focus ring on the focused form field (if any).
        if let Some(idx) = self.focused_field {
            if let Some(field) = self.form_fields.get(idx) {
                let fx = field.x - self.scroll_x;
                let fy = field.y + URL_BAR_HEIGHT - self.scroll_y;
                let focus_color = Color { r: 0.26, g: 0.52, b: 0.96, a: 1.0 };
                self.framebuffer.stroke_rect(fx - 1.0, fy - 1.0, field.width + 2.0, field.height + 2.0, focus_color, 2.0);
            }
        }

        // Draw the URL bar on top (overwrites the top region, not affected by scroll).
        self.draw_url_bar();
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
                self.scroll_regions = Self::extract_scroll_regions(&cmds);
                self.focused_field = None;
                self.content_height = Self::compute_content_height(&cmds);
                self.content_width = Self::compute_content_width(&cmds);
                self.render_commands = cmds;
                self.scroll_y = 0.0;
                self.scroll_x = 0.0;
                self.url_bar_focused = false;
                self.url_bar_select_all = false;
                self.url_bar_text = url.to_string();
                self.url_bar_cursor = self.url_bar_text.len();

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

                    // Update the viewport so subsequent navigations use the new size.
                    self.viewport.width = self.width as f32;
                    self.viewport.height = self.height as f32;

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

                // Check if the cursor is inside a scroll container.
                // If so, route the scroll event to that container instead
                // of the global scroll offset.
                let page_x = self.cursor_x as f32 + self.scroll_x;
                let page_y = (self.cursor_y as f32 - URL_BAR_HEIGHT) + self.scroll_y;
                let mut handled_by_container = false;

                if (self.cursor_y as f32) > URL_BAR_HEIGHT {
                    for region in &mut self.scroll_regions {
                        if region.contains(page_x, page_y) && region.content_height > region.height
                        {
                            if dy.abs() > 0.01 {
                                region.scroll_offset += dy;
                                region.clamp_scroll();
                                handled_by_container = true;
                            }
                            break;
                        }
                    }
                }

                if handled_by_container {
                    self.request_redraw();
                } else {
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
            }

            // ── Keyboard input ─────────────────────────────────────
            WindowEvent::KeyboardInput { event, .. } => {
                // First try the URL bar.
                if self.url_bar_focused {
                    self.handle_url_bar_key(&event);
                    self.request_redraw();
                    return;
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

                    // Check for form field clicks — focus the field.
                    let page_x = cx as f32 + self.scroll_x;
                    let page_y = (cy as f32 - URL_BAR_HEIGHT) + self.scroll_y;
                    let mut clicked_field = None;
                    for (i, field) in self.form_fields.iter().enumerate() {
                        if field.contains(page_x, page_y) {
                            clicked_field = Some(i);
                            break;
                        }
                    }
                    if let Some(idx) = clicked_field {
                        self.focused_field = Some(idx);
                        self.request_redraw();
                    } else {
                        if self.focused_field.is_some() {
                            self.focused_field = None;
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

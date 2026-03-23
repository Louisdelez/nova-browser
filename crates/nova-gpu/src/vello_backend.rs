//! Vello GPU rendering backend.
//!
//! Translates NOVA `RenderOp`s into a Vello `Scene` and renders them using
//! the GPU via wgpu. Falls back to a simple software rasterizer when GPU
//! initialisation fails (e.g. headless environments).
//!
//! # Architecture
//!
//! ```text
//! RenderOps --> VelloBackend::build_scene() --> vello::Scene
//!          --> VelloBackend::render_scene()  --> GPU (wgpu) --> RGBA pixels
//! ```
//!
//! Each `RenderOp` variant maps to a Vello drawing command:
//! - `FillRect` -> `scene.fill(Fill::NonZero, transform, color, None, &rect)`
//! - `FillRoundedRect` -> `scene.fill(... &RoundedRect::from_rect(rect, radii))`
//! - `DrawText` -> `scene.fill(... &rect)` (glyph rendering deferred to CPU for now)
//! - `DrawImage` -> `scene.draw_image(&image, transform)`
//! - `StrokeRect` -> `scene.stroke(stroke, transform, color, None, &rect)`
//! - `PushClip/PopClip` -> `scene.push_layer(...)` / `scene.pop_layer()`
//! - `PushOpacity/PopOpacity` -> `scene.push_layer(BlendMode::default(), opacity, ...)` / `scene.pop_layer()`
//! - `BoxShadow` -> fill offset rectangle with shadow color

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use tracing::{debug, error, info, warn};

use nova_mod_api::{Color, RenderOp};

use vello::kurbo::{Affine, Rect, RoundedRect, RoundedRectRadii, Stroke};
use vello::peniko::{self, Blob, Fill, ImageData};
use vello::{AaConfig, AaSupport, RenderParams, RendererOptions, Scene};

// ---------------------------------------------------------------------------
// GPU state (lazily initialised)
// ---------------------------------------------------------------------------

/// Holds the wgpu device, queue, and Vello renderer for GPU rendering.
///
/// This is created lazily on the first call to `render_to_texture` when
/// GPU is available.
struct GpuState {
    device: vello::wgpu::Device,
    queue: vello::wgpu::Queue,
    renderer: vello::Renderer,
}

// ---------------------------------------------------------------------------
// Image cache
// ---------------------------------------------------------------------------

/// Cache key for decoded images uploaded to the GPU.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ImageCacheKey {
    /// Width of the source image.
    width: u32,
    /// Height of the source image.
    height: u32,
    /// A hash of the first 64 bytes + length for fast lookup.
    data_hash: u64,
}

impl ImageCacheKey {
    fn from_pixels(width: u32, height: u32, pixels: &[u8]) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        pixels.len().hash(&mut hasher);
        // Hash first 64 bytes for speed.
        let sample = &pixels[..pixels.len().min(64)];
        sample.hash(&mut hasher);
        Self {
            width,
            height,
            data_hash: hasher.finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// VelloBackend
// ---------------------------------------------------------------------------

/// GPU rendering backend powered by Vello.
///
/// Manages a Vello `Scene`, a wgpu device/queue pair, and a `vello::Renderer`.
/// When GPU is not available, falls back to a minimal software rasteriser.
///
/// The GPU state is wrapped in a `Mutex` because `vello::Renderer` is `Send`
/// but not `Sync` (it uses `RefCell` internally). The `Mutex` ensures
/// `VelloBackend` is both `Send + Sync`, which is required because
/// `GpuCompositor` lives behind `Arc<RwLock<>>`.
pub struct VelloBackend {
    /// Whether the backend has been initialized.
    initialized: bool,
    /// Render scale factor (for HiDPI displays).
    scale_factor: f32,
    /// Lazily-created GPU state (device + queue + renderer), behind a Mutex
    /// because `vello::Renderer` is not `Sync`.
    gpu: Mutex<Option<GpuState>>,
    /// Whether we already tried and failed to init GPU.
    gpu_init_failed: bool,
    /// Image cache: avoids re-creating `ImageData` objects for the same pixels.
    image_cache: HashMap<ImageCacheKey, ImageData>,
}

// Manual Debug implementation because GpuState contains non-Debug types.
impl std::fmt::Debug for VelloBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gpu_available = self.gpu.lock().map(|g| g.is_some()).unwrap_or(false);
        f.debug_struct("VelloBackend")
            .field("initialized", &self.initialized)
            .field("scale_factor", &self.scale_factor)
            .field("gpu_available", &gpu_available)
            .field("image_cache_size", &self.image_cache.len())
            .finish()
    }
}

impl VelloBackend {
    /// Create a new Vello backend (not yet initialized).
    pub fn new() -> Self {
        info!("VelloBackend created (GPU rendering will be initialised on first use)");
        Self {
            initialized: false,
            scale_factor: 1.0,
            gpu: Mutex::new(None),
            gpu_init_failed: false,
            image_cache: HashMap::new(),
        }
    }

    /// Initialize the Vello renderer.
    ///
    /// Sets the scale factor. GPU device creation is deferred to the first
    /// `render_to_texture` call to avoid blocking startup.
    pub fn init(&mut self, scale_factor: f32) {
        self.scale_factor = scale_factor;
        self.initialized = true;
        info!(
            scale_factor,
            "VelloBackend initialized (GPU init deferred to first render)"
        );
    }

    /// Try to initialize the wgpu device and Vello renderer.
    ///
    /// Returns `true` if GPU is ready, `false` if initialization failed
    /// (will use software fallback).
    fn ensure_gpu(&mut self) -> bool {
        {
            let guard = self.gpu.lock().unwrap();
            if guard.is_some() {
                return true;
            }
        }
        if self.gpu_init_failed {
            return false;
        }

        info!("VelloBackend: initialising wgpu device for GPU rendering");

        let result = pollster::block_on(async {
            let instance = vello::wgpu::Instance::new(&vello::wgpu::InstanceDescriptor {
                backends: vello::wgpu::Backends::all(),
                ..Default::default()
            });

            let adapter = instance
                .request_adapter(&vello::wgpu::RequestAdapterOptions {
                    power_preference: vello::wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .ok()?;

            info!(
                backend = ?adapter.get_info().backend,
                name = adapter.get_info().name,
                "VelloBackend: wgpu adapter found"
            );

            let features = adapter.features();
            let limits = adapter.limits();
            let (device, queue) = adapter
                .request_device(&vello::wgpu::DeviceDescriptor {
                    label: Some("nova-vello-device"),
                    required_features: features
                        & (vello::wgpu::Features::TIMESTAMP_QUERY
                            | vello::wgpu::Features::CLEAR_TEXTURE),
                    required_limits: limits,
                    memory_hints: Default::default(),
                    trace: Default::default(),
                    experimental_features: Default::default(),
                })
                .await
                .ok()?;

            let renderer = vello::Renderer::new(
                &device,
                RendererOptions {
                    use_cpu: false,
                    antialiasing_support: AaSupport::area_only(),
                    num_init_threads: NonZeroUsize::new(1),
                    pipeline_cache: None,
                },
            )
            .ok()?;

            Some(GpuState {
                device,
                queue,
                renderer,
            })
        });

        match result {
            Some(state) => {
                info!("VelloBackend: GPU rendering ready");
                *self.gpu.lock().unwrap() = Some(state);
                true
            }
            None => {
                warn!("VelloBackend: GPU init failed, using software fallback");
                self.gpu_init_failed = true;
                false
            }
        }
    }

    /// Build a Vello `Scene` from a slice of `RenderOp`s.
    ///
    /// This is the core translation layer: each NOVA render operation is
    /// mapped to the corresponding Vello scene primitive.
    pub fn build_scene(&mut self, ops: &[RenderOp], _width: u32, _height: u32) -> Scene {
        let mut scene = Scene::new();
        let mut transform_stack: Vec<Affine> = vec![Affine::IDENTITY];

        /// Get the current transform from the stack.
        fn current_tf(stack: &[Affine]) -> Affine {
            *stack.last().unwrap_or(&Affine::IDENTITY)
        }

        for op in ops {
            match op {
                RenderOp::FillRect {
                    x,
                    y,
                    width,
                    height,
                    color,
                } => {
                    let rect = Rect::new(*x as f64, *y as f64, (*x + *width) as f64, (*y + *height) as f64);
                    let brush = nova_to_vello_color(*color);
                    scene.fill(Fill::NonZero, current_tf(&transform_stack), brush, None, &rect);
                }

                RenderOp::FillRoundedRect {
                    x,
                    y,
                    width,
                    height,
                    color,
                    radius,
                } => {
                    let rect = Rect::new(*x as f64, *y as f64, (*x + *width) as f64, (*y + *height) as f64);
                    let radii = RoundedRectRadii::new(
                        radius[0] as f64,
                        radius[1] as f64,
                        radius[2] as f64,
                        radius[3] as f64,
                    );
                    let rounded = RoundedRect::from_rect(rect, radii);
                    let brush = nova_to_vello_color(*color);
                    scene.fill(Fill::NonZero, current_tf(&transform_stack), brush, None, &rounded);
                }

                RenderOp::StrokeRect {
                    x,
                    y,
                    width,
                    height,
                    color,
                    width_px,
                } => {
                    let rect = Rect::new(*x as f64, *y as f64, (*x + *width) as f64, (*y + *height) as f64);
                    let brush = nova_to_vello_color(*color);
                    let stroke = Stroke::new(*width_px as f64);
                    scene.stroke(&stroke, current_tf(&transform_stack), brush, None, &rect);
                }

                RenderOp::DrawText {
                    x,
                    y,
                    text,
                    font_size,
                    color,
                    ..
                } => {
                    // For text rendering, we draw a colored rectangle as a placeholder.
                    // Full glyph rendering is handled by the software renderer in nova-shell.
                    // When Vello is the primary renderer, this will use scene.draw_glyphs().
                    //
                    // For now, we estimate the text bounds and fill a very thin underline
                    // to mark text position.
                    let text_width = text.len() as f64 * *font_size as f64 * 0.6;
                    let text_height = *font_size as f64 * 1.2;
                    let brush = nova_to_vello_color(*color);
                    let underline = Rect::new(
                        *x as f64,
                        *y as f64 + text_height - 1.0,
                        *x as f64 + text_width,
                        *y as f64 + text_height,
                    );
                    scene.fill(Fill::NonZero, current_tf(&transform_stack), brush, None, &underline);
                }

                RenderOp::DrawImage {
                    x,
                    y,
                    width,
                    height,
                    img_width,
                    img_height,
                    pixels,
                } => {
                    if *img_width == 0 || *img_height == 0 || pixels.is_empty() {
                        continue;
                    }

                    let cache_key = ImageCacheKey::from_pixels(*img_width, *img_height, pixels);
                    let image_data = self.image_cache.entry(cache_key).or_insert_with(|| {
                        let blob = Blob::new(Arc::new(pixels.clone()));
                        ImageData {
                            data: blob,
                            format: peniko::ImageFormat::Rgba8,
                            alpha_type: peniko::ImageAlphaType::Alpha,
                            width: *img_width,
                            height: *img_height,
                        }
                    });

                    // Compute the transform to scale the image from its native size
                    // to the destination rectangle.
                    let scale_x = *width as f64 / *img_width as f64;
                    let scale_y = *height as f64 / *img_height as f64;
                    let img_transform = current_tf(&transform_stack)
                        * Affine::translate((*x as f64, *y as f64))
                        * Affine::scale_non_uniform(scale_x, scale_y);

                    let image_ref: &ImageData = image_data;
                    scene.draw_image(image_ref, img_transform);
                }

                RenderOp::DrawTexture { .. } => {
                    // GPU texture handles are managed separately; skip for scene building.
                    debug!("VelloBackend: DrawTexture skipped (handled by compositor)");
                }

                RenderOp::PushClip {
                    x,
                    y,
                    width,
                    height,
                } => {
                    let clip_rect = Rect::new(
                        *x as f64,
                        *y as f64,
                        (*x + *width) as f64,
                        (*y + *height) as f64,
                    );
                    scene.push_clip_layer(Fill::NonZero, current_tf(&transform_stack), &clip_rect);
                }

                RenderOp::PushRoundedClip {
                    x,
                    y,
                    width,
                    height,
                    radius,
                } => {
                    let rect = Rect::new(*x as f64, *y as f64, (*x + *width) as f64, (*y + *height) as f64);
                    let radii = RoundedRectRadii::new(
                        radius[0] as f64,
                        radius[1] as f64,
                        radius[2] as f64,
                        radius[3] as f64,
                    );
                    let rounded = RoundedRect::from_rect(rect, radii);
                    scene.push_clip_layer(Fill::NonZero, current_tf(&transform_stack), &rounded);
                }

                RenderOp::PopClip => {
                    scene.pop_layer();
                }

                RenderOp::PushOpacity { opacity } => {
                    // Push a layer with reduced alpha over the full scene area.
                    // We use a very large clip rect to avoid clipping content.
                    let full_rect = Rect::new(-1e6, -1e6, 1e6, 1e6);
                    scene.push_layer(
                        Fill::NonZero,
                        peniko::BlendMode::default(),
                        *opacity,
                        current_tf(&transform_stack),
                        &full_rect,
                    );
                }

                RenderOp::PopOpacity => {
                    scene.pop_layer();
                }

                RenderOp::Translate { x, y } => {
                    let current = current_tf(&transform_stack);
                    let new_tf = current * Affine::translate((*x as f64, *y as f64));
                    if let Some(top) = transform_stack.last_mut() {
                        *top = new_tf;
                    }
                }

                RenderOp::Transform { matrix } => {
                    // Apply a 2D affine transform [a, b, c, d, e, f].
                    let m = matrix;
                    let affine = Affine::new([
                        m[0] as f64, m[1] as f64,
                        m[2] as f64, m[3] as f64,
                        m[4] as f64, m[5] as f64,
                    ]);
                    let current = current_tf(&transform_stack);
                    let new_tf = current * affine;
                    if let Some(top) = transform_stack.last_mut() {
                        *top = new_tf;
                    }
                }

                RenderOp::Save => {
                    let current = current_tf(&transform_stack);
                    transform_stack.push(current);
                }

                RenderOp::Restore => {
                    if transform_stack.len() > 1 {
                        transform_stack.pop();
                    }
                }

                RenderOp::BoxShadow {
                    x,
                    y,
                    width,
                    height,
                    color,
                    offset_x,
                    offset_y,
                    blur,
                } => {
                    let shadow_rect = Rect::new(
                        (*x + *offset_x) as f64,
                        (*y + *offset_y) as f64,
                        (*x + *offset_x + *width) as f64,
                        (*y + *offset_y + *height) as f64,
                    );
                    let brush = nova_to_vello_color(*color);

                    if *blur > 0.0 {
                        // Use Vello's blurred rounded rect for actual blur.
                        scene.draw_blurred_rounded_rect(
                            current_tf(&transform_stack),
                            shadow_rect,
                            brush,
                            0.0,
                            *blur as f64 * 0.5,
                        );
                    } else {
                        scene.fill(Fill::NonZero, current_tf(&transform_stack), brush, None, &shadow_rect);
                    }
                }

                // Sticky positioning and link/form metadata ops don't produce visual output.
                RenderOp::StickyStart { .. }
                | RenderOp::StickyEnd
                | RenderOp::FixedStart
                | RenderOp::FixedEnd
                | RenderOp::Link { .. }
                | RenderOp::FormField { .. }
                | RenderOp::Anchor { .. }
                | RenderOp::CursorHint { .. } => {}
            }
        }

        scene
    }

    /// Render a set of `RenderOp`s to an RGBA pixel buffer.
    ///
    /// Builds a Vello `Scene` from the operations, renders it on the GPU
    /// (if available), and reads back the pixel data. Falls back to a
    /// simple software rasterizer if GPU is not available.
    ///
    /// # Arguments
    /// * `ops` - The render operations to draw.
    /// * `width` - Output buffer width in pixels.
    /// * `height` - Output buffer height in pixels.
    ///
    /// # Returns
    /// RGBA pixel data (`width * height * 4` bytes).
    pub fn render_to_texture(&mut self, ops: &[RenderOp], width: u32, height: u32) -> Vec<u8> {
        debug!(
            ops = ops.len(),
            width,
            height,
            "VelloBackend::render_to_texture"
        );

        if width == 0 || height == 0 {
            return vec![];
        }

        // Build the scene from render ops.
        let scene = self.build_scene(ops, width, height);

        // Try GPU rendering.
        if self.ensure_gpu() {
            match self.render_scene_gpu(&scene, width, height) {
                Ok(pixels) => return pixels,
                Err(e) => {
                    error!("VelloBackend: GPU render failed: {e}, falling back to software");
                }
            }
        }

        // Software fallback: produce a white buffer with basic FillRect support.
        self.render_software_fallback(ops, width, height)
    }

    /// Render a Vello scene to pixels using the GPU.
    fn render_scene_gpu(
        &self,
        scene: &Scene,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, String> {
        let mut gpu_guard = self.gpu.lock().map_err(|e| format!("GPU mutex poisoned: {e}"))?;
        let gpu = gpu_guard.as_mut().ok_or("GPU not initialized")?;

        // Create the render target texture.
        let texture = gpu.device.create_texture(&vello::wgpu::TextureDescriptor {
            label: Some("nova-vello-render-target"),
            size: vello::wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: vello::wgpu::TextureDimension::D2,
            format: vello::wgpu::TextureFormat::Rgba8Unorm,
            usage: vello::wgpu::TextureUsages::STORAGE_BINDING
                | vello::wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let texture_view = texture.create_view(&vello::wgpu::TextureViewDescriptor::default());

        // Render the scene.
        let params = RenderParams {
            base_color: peniko::Color::new([1.0, 1.0, 1.0, 1.0]), // White background
            width,
            height,
            antialiasing_method: AaConfig::Area,
        };

        gpu.renderer
            .render_to_texture(&gpu.device, &gpu.queue, scene, &texture_view, &params)
            .map_err(|e| format!("Vello render failed: {e}"))?;

        // Read back pixels from GPU.
        let padded_bytes_per_row = Self::padded_bytes_per_row(width);
        let buffer_size = (padded_bytes_per_row * height as usize) as u64;

        let readback_buffer = gpu.device.create_buffer(&vello::wgpu::BufferDescriptor {
            label: Some("nova-vello-readback"),
            size: buffer_size,
            usage: vello::wgpu::BufferUsages::COPY_DST | vello::wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = gpu.device.create_command_encoder(
            &vello::wgpu::CommandEncoderDescriptor {
                label: Some("nova-vello-readback-encoder"),
            },
        );

        encoder.copy_texture_to_buffer(
            vello::wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: vello::wgpu::Origin3d::ZERO,
                aspect: vello::wgpu::TextureAspect::All,
            },
            vello::wgpu::TexelCopyBufferInfo {
                buffer: &readback_buffer,
                layout: vello::wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row as u32),
                    rows_per_image: Some(height),
                },
            },
            vello::wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        gpu.queue.submit(std::iter::once(encoder.finish()));

        // Map the buffer and read back pixels.
        let buffer_slice = readback_buffer.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        buffer_slice.map_async(vello::wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });

        let _ = gpu.device.poll(vello::wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        });

        receiver
            .recv()
            .map_err(|e| format!("readback channel error: {e}"))?
            .map_err(|e| format!("buffer map failed: {e}"))?;

        let mapped = buffer_slice.get_mapped_range();
        let unpadded_bytes_per_row = width as usize * 4;

        // Remove row padding if present.
        let mut pixels = Vec::with_capacity(unpadded_bytes_per_row * height as usize);
        for row in 0..height as usize {
            let start = row * padded_bytes_per_row;
            let end = start + unpadded_bytes_per_row;
            pixels.extend_from_slice(&mapped[start..end]);
        }

        drop(mapped);
        readback_buffer.unmap();

        debug!(
            width,
            height,
            pixel_count = pixels.len() / 4,
            "VelloBackend: GPU render complete"
        );

        Ok(pixels)
    }

    /// Compute the padded bytes per row for wgpu texture readback.
    ///
    /// wgpu requires rows to be aligned to `COPY_BYTES_PER_ROW_ALIGNMENT` (256 bytes).
    fn padded_bytes_per_row(width: u32) -> usize {
        let unpadded = width as usize * 4;
        let align = vello::wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        (unpadded + align - 1) & !(align - 1)
    }

    /// Software fallback renderer — produces an RGBA buffer from basic ops.
    fn render_software_fallback(&self, ops: &[RenderOp], width: u32, height: u32) -> Vec<u8> {
        debug!(
            ops = ops.len(),
            width,
            height,
            "VelloBackend::render_software_fallback"
        );

        let size = (width as usize) * (height as usize) * 4;
        let mut pixels = vec![255u8; size];

        // Render a simple background color if the first op is a FillRect.
        if let Some(RenderOp::FillRect { color, .. }) = ops.first() {
            let r = (color.r * 255.0) as u8;
            let g = (color.g * 255.0) as u8;
            let b = (color.b * 255.0) as u8;
            let a = (color.a * 255.0) as u8;
            for chunk in pixels.chunks_exact_mut(4) {
                chunk[0] = r;
                chunk[1] = g;
                chunk[2] = b;
                chunk[3] = a;
            }
        }

        pixels
    }

    /// Check whether the Vello backend is ready to render.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Check whether GPU rendering is available.
    pub fn has_gpu(&self) -> bool {
        self.gpu.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Get the current scale factor.
    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    /// Convert a NOVA `Color` to Vello-compatible RGBA bytes.
    ///
    /// This utility is used when building Vello scenes. Returns `[r, g, b, a]`
    /// as `u8` values.
    pub fn color_to_rgba_bytes(color: Color) -> [u8; 4] {
        [
            (color.r.clamp(0.0, 1.0) * 255.0) as u8,
            (color.g.clamp(0.0, 1.0) * 255.0) as u8,
            (color.b.clamp(0.0, 1.0) * 255.0) as u8,
            (color.a.clamp(0.0, 1.0) * 255.0) as u8,
        ]
    }

    /// Clear the image cache (e.g. on navigation).
    pub fn clear_image_cache(&mut self) {
        let count = self.image_cache.len();
        self.image_cache.clear();
        if count > 0 {
            debug!(count, "VelloBackend: image cache cleared");
        }
    }
}

impl Default for VelloBackend {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a NOVA `Color` to a Vello/peniko `Color`.
fn nova_to_vello_color(c: Color) -> peniko::Color {
    peniko::Color::new([c.r, c.g, c.b, c.a])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::Color;

    #[test]
    fn test_vello_backend_creation() {
        let backend = VelloBackend::new();
        assert!(!backend.is_initialized());
        assert_eq!(backend.scale_factor(), 1.0);
    }

    #[test]
    fn test_vello_backend_init() {
        let mut backend = VelloBackend::new();
        backend.init(2.0);
        assert!(backend.is_initialized());
        assert_eq!(backend.scale_factor(), 2.0);
    }

    #[test]
    fn test_build_scene_empty() {
        let mut backend = VelloBackend::new();
        let scene = backend.build_scene(&[], 100, 100);
        // Scene should be created without errors.
        let _ = scene;
    }

    #[test]
    fn test_build_scene_fill_rect() {
        let mut backend = VelloBackend::new();
        let ops = vec![RenderOp::FillRect {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
            color: Color::rgb(1.0, 0.0, 0.0),
        }];
        let scene = backend.build_scene(&ops, 200, 200);
        let _ = scene;
    }

    #[test]
    fn test_build_scene_rounded_rect() {
        let mut backend = VelloBackend::new();
        let ops = vec![RenderOp::FillRoundedRect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 80.0,
            color: Color::rgb(0.0, 0.5, 1.0),
            radius: [10.0, 10.0, 10.0, 10.0],
        }];
        let scene = backend.build_scene(&ops, 200, 200);
        let _ = scene;
    }

    #[test]
    fn test_build_scene_clip_and_opacity() {
        let mut backend = VelloBackend::new();
        let ops = vec![
            RenderOp::PushClip {
                x: 10.0,
                y: 10.0,
                width: 80.0,
                height: 80.0,
            },
            RenderOp::PushOpacity { opacity: 0.5 },
            RenderOp::FillRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
                color: Color::rgb(0.0, 1.0, 0.0),
            },
            RenderOp::PopOpacity,
            RenderOp::PopClip,
        ];
        let scene = backend.build_scene(&ops, 100, 100);
        let _ = scene;
    }

    #[test]
    fn test_build_scene_transform_stack() {
        let mut backend = VelloBackend::new();
        let ops = vec![
            RenderOp::Save,
            RenderOp::Translate { x: 50.0, y: 50.0 },
            RenderOp::FillRect {
                x: 0.0,
                y: 0.0,
                width: 20.0,
                height: 20.0,
                color: Color::BLACK,
            },
            RenderOp::Restore,
            RenderOp::FillRect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                color: Color::WHITE,
            },
        ];
        let scene = backend.build_scene(&ops, 100, 100);
        let _ = scene;
    }

    #[test]
    fn test_build_scene_box_shadow() {
        let mut backend = VelloBackend::new();
        let ops = vec![RenderOp::BoxShadow {
            x: 20.0,
            y: 20.0,
            width: 60.0,
            height: 40.0,
            color: Color::rgba(0.0, 0.0, 0.0, 0.3),
            offset_x: 4.0,
            offset_y: 4.0,
            blur: 8.0,
        }];
        let scene = backend.build_scene(&ops, 100, 100);
        let _ = scene;
    }

    #[test]
    fn test_build_scene_draw_image() {
        let mut backend = VelloBackend::new();
        let pixels = vec![255u8; 4 * 4 * 4]; // 4x4 white image
        let ops = vec![RenderOp::DrawImage {
            x: 10.0,
            y: 10.0,
            width: 40.0,
            height: 40.0,
            img_width: 4,
            img_height: 4,
            pixels,
        }];
        let scene = backend.build_scene(&ops, 100, 100);
        let _ = scene;
    }

    #[test]
    fn test_render_to_texture_fallback() {
        let mut backend = VelloBackend::new();
        let ops = vec![RenderOp::FillRect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
            color: Color::rgb(1.0, 0.0, 0.0),
        }];
        let pixels = backend.render_to_texture(&ops, 4, 4);
        assert_eq!(pixels.len(), 4 * 4 * 4);
        // Should be red (from software fallback).
        assert_eq!(pixels[0], 255); // R
        assert_eq!(pixels[1], 0);   // G
        assert_eq!(pixels[2], 0);   // B
        assert_eq!(pixels[3], 255); // A
    }

    #[test]
    fn test_render_to_texture_empty() {
        let mut backend = VelloBackend::new();
        let pixels = backend.render_to_texture(&[], 10, 10);
        assert_eq!(pixels.len(), 10 * 10 * 4);
        // Should be white (default).
        assert_eq!(pixels[0], 255);
        assert_eq!(pixels[1], 255);
        assert_eq!(pixels[2], 255);
        assert_eq!(pixels[3], 255);
    }

    #[test]
    fn test_color_to_rgba_bytes() {
        let color = Color::rgba(0.5, 0.25, 0.75, 1.0);
        let bytes = VelloBackend::color_to_rgba_bytes(color);
        assert_eq!(bytes[0], 127); // 0.5 * 255 ~ 127
        assert_eq!(bytes[1], 63);  // 0.25 * 255 ~ 63
        assert_eq!(bytes[2], 191); // 0.75 * 255 ~ 191
        assert_eq!(bytes[3], 255); // 1.0 * 255 = 255
    }

    #[test]
    fn test_color_to_rgba_bytes_clamping() {
        let color = Color::rgba(-0.5, 1.5, 0.0, 0.0);
        let bytes = VelloBackend::color_to_rgba_bytes(color);
        assert_eq!(bytes[0], 0);   // clamped from -0.5
        assert_eq!(bytes[1], 255); // clamped from 1.5
        assert_eq!(bytes[2], 0);
        assert_eq!(bytes[3], 0);
    }

    #[test]
    fn test_image_cache_key() {
        let key1 = ImageCacheKey::from_pixels(10, 10, &[0u8; 400]);
        let key2 = ImageCacheKey::from_pixels(10, 10, &[0u8; 400]);
        let key3 = ImageCacheKey::from_pixels(10, 10, &[1u8; 400]);
        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_render_zero_size() {
        let mut backend = VelloBackend::new();
        let pixels = backend.render_to_texture(&[], 0, 0);
        assert!(pixels.is_empty());
    }
}

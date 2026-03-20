//! Vello rendering backend (stub).
//!
//! This module provides a placeholder for future Vello GPU-accelerated
//! rendering. Currently it delegates to the software renderer. When Vello
//! is integrated, this will translate `RenderOp`s directly into Vello
//! scene primitives for GPU-native path rendering.
//!
//! # Future Architecture
//!
//! ```text
//! RenderOps ──► VelloBackend ──► vello::Scene ──► GPU (wgpu)
//! ```
//!
//! Each `RenderOp` variant maps to a Vello drawing command:
//! - `FillRect` → `scene.fill(Shape::rect(...))`
//! - `DrawText` → `scene.draw_glyphs(...)`
//! - `StrokeRect` → `scene.stroke(Shape::rect(...))`
//! - `DrawImage` → `scene.draw_image(...)`
//! - `FillRoundedRect` → `scene.fill(Shape::rounded_rect(...))`
//! - `PushClip/PopClip` → `scene.push_layer(clip)` / `scene.pop_layer()`

use tracing::{debug, info};

use nova_mod_api::{Color, RenderCommands, RenderOp};

// ---------------------------------------------------------------------------
// VelloBackend
// ---------------------------------------------------------------------------

/// Placeholder backend for Vello GPU rendering.
///
/// In the current phase this simply stores configuration for future use.
/// Once Vello is added as a dependency, this struct will hold a
/// `vello::Renderer` and manage scene building from `RenderOp`s.
#[derive(Debug)]
pub struct VelloBackend {
    /// Whether the backend has been initialized.
    initialized: bool,
    /// Render scale factor (for HiDPI displays).
    scale_factor: f32,
}

impl VelloBackend {
    /// Create a new Vello backend (not yet initialized).
    pub fn new() -> Self {
        info!("VelloBackend created (stub — software fallback active)");
        Self {
            initialized: false,
            scale_factor: 1.0,
        }
    }

    /// Initialize the Vello renderer.
    ///
    /// In the future this will create the `vello::Renderer` with the given
    /// wgpu device and queue. For now it just marks the backend as ready.
    pub fn init(&mut self, scale_factor: f32) {
        self.scale_factor = scale_factor;
        self.initialized = true;
        info!(
            scale_factor,
            "VelloBackend initialized (stub — will use software fallback)"
        );
    }

    /// Render a set of `RenderOp`s to an RGBA pixel buffer.
    ///
    /// Currently delegates to a minimal software rasterizer that produces
    /// a blank white buffer of the requested dimensions. In the future,
    /// this will build a `vello::Scene`, render it with the GPU, and read
    /// back the pixel data.
    ///
    /// # Arguments
    /// * `ops` — The render operations to draw.
    /// * `width` — Output buffer width in pixels.
    /// * `height` — Output buffer height in pixels.
    ///
    /// # Returns
    /// RGBA pixel data (`width * height * 4` bytes).
    pub fn render_to_texture(&self, ops: &[RenderOp], width: u32, height: u32) -> Vec<u8> {
        debug!(
            ops = ops.len(),
            width,
            height,
            "VelloBackend::render_to_texture (software fallback)"
        );

        // Software fallback: produce a white buffer.
        // The real software rendering is handled by Framebuffer in nova-shell.
        // This stub exists so that the compositor can request GPU-rendered
        // layer textures in the future.
        let size = (width as usize) * (height as usize) * 4;
        let mut pixels = vec![255u8; size];

        // Render a simple background color if the first op is a FillRect.
        // This gives a minimal visual indication that the backend is working.
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
}

impl Default for VelloBackend {
    fn default() -> Self {
        Self::new()
    }
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
    fn test_render_to_texture_empty() {
        let backend = VelloBackend::new();
        let pixels = backend.render_to_texture(&[], 10, 10);
        assert_eq!(pixels.len(), 10 * 10 * 4);
        // Should be white.
        assert_eq!(pixels[0], 255);
        assert_eq!(pixels[1], 255);
        assert_eq!(pixels[2], 255);
        assert_eq!(pixels[3], 255);
    }

    #[test]
    fn test_render_to_texture_with_fill() {
        let backend = VelloBackend::new();
        let ops = vec![RenderOp::FillRect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
            color: Color::rgb(1.0, 0.0, 0.0),
        }];
        let pixels = backend.render_to_texture(&ops, 4, 4);
        assert_eq!(pixels.len(), 4 * 4 * 4);
        // Should be red.
        assert_eq!(pixels[0], 255); // R
        assert_eq!(pixels[1], 0);   // G
        assert_eq!(pixels[2], 0);   // B
        assert_eq!(pixels[3], 255); // A
    }

    #[test]
    fn test_color_to_rgba_bytes() {
        let color = Color::rgba(0.5, 0.25, 0.75, 1.0);
        let bytes = VelloBackend::color_to_rgba_bytes(color);
        assert_eq!(bytes[0], 127); // 0.5 * 255 ≈ 127
        assert_eq!(bytes[1], 63);  // 0.25 * 255 ≈ 63
        assert_eq!(bytes[2], 191); // 0.75 * 255 ≈ 191
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
}

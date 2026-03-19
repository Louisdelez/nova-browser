//! Core types shared across the entire NOVA ecosystem.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a mod (e.g., "org.nova.html-parser").
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModId(pub String);

impl ModId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for ModId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Opaque handle to a GPU buffer managed by the compositor.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct GpuBufferHandle(pub Uuid);

impl GpuBufferHandle {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Opaque handle to a GPU texture managed by the compositor.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct GpuTextureHandle(pub Uuid);

impl GpuTextureHandle {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Render commands that mods submit to the GPU compositor.
#[derive(Debug, Clone)]
pub struct RenderCommands {
    pub ops: Vec<RenderOp>,
}

/// Individual render operations.
#[derive(Debug, Clone)]
pub enum RenderOp {
    /// Fill a rectangle with a solid color.
    FillRect {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
    },
    /// Draw text at a position.
    DrawText {
        x: f32,
        y: f32,
        text: String,
        font_size: f32,
        color: Color,
    },
    /// Draw an image from a GPU texture.
    DrawTexture {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        texture: GpuTextureHandle,
    },
    /// Draw a decoded image from inline RGBA pixel data.
    ///
    /// The `pixels` buffer contains `img_width * img_height * 4` bytes in
    /// row-major RGBA order. The renderer scales the source image to fill
    /// the destination rectangle `(x, y, width, height)` using
    /// nearest-neighbour sampling.
    DrawImage {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        /// Width of the source image in pixels.
        img_width: u32,
        /// Height of the source image in pixels.
        img_height: u32,
        /// Raw RGBA pixel data (length = img_width * img_height * 4).
        pixels: Vec<u8>,
    },
    /// Draw a border around a rectangle.
    StrokeRect {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
        width_px: f32,
    },
    /// Push a clip rectangle (nested clips stack).
    PushClip {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
    /// Pop the current clip rectangle.
    PopClip,
    /// Apply a translation offset.
    Translate { x: f32, y: f32 },
    /// Save the current transform state.
    Save,
    /// Restore the previous transform state.
    Restore,
    /// A clickable link region (does not render anything visible).
    ///
    /// Emitted by the painter when it encounters an `<a>` element with an `href`.
    /// The window uses these to build hit-test regions for mouse interaction.
    Link {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        url: String,
    },
}

/// RGBA color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const WHITE: Self = Self { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    pub const BLACK: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const TRANSPARENT: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    pub fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }
}

/// Viewport dimensions.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub scale_factor: f32,
}

/// Log levels for structured logging through the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

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
    /// Custom fonts fetched from `@font-face` rules.
    ///
    /// Each entry is `(family_name, font_bytes)` where `font_bytes` is the raw
    /// TTF/OTF file data. The renderer loads these fonts and uses them when a
    /// `DrawText` op references the corresponding `font_family`.
    pub fonts: Vec<(String, Vec<u8>)>,
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
        /// CSS font-weight (400 = normal, 700 = bold). None defaults to 400.
        font_weight: Option<u16>,
        /// CSS font-style ("italic", "oblique"). None defaults to normal.
        font_style: Option<String>,
        /// CSS font-family name. When set and a matching custom font has been
        /// loaded (via `@font-face`), the renderer uses that font instead of
        /// the default DejaVu Sans.
        font_family: Option<String>,
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
    /// Apply a 2D affine transform.
    ///
    /// The matrix is stored as `[a, b, c, d, e, f]` representing:
    /// ```text
    /// | a c e |
    /// | b d f |
    /// | 0 0 1 |
    /// ```
    /// This supports translate, rotate, scale, skew, and arbitrary 2D transforms.
    Transform { matrix: [f32; 6] },
    /// Mark the start of a sticky-positioned element.
    ///
    /// The renderer should clamp the element's y position so it stays visible
    /// within `[sticky_top, sticky_bottom]` relative to the viewport during scroll.
    /// `original_y` is the element's position in the document flow.
    StickyStart {
        original_y: f32,
        sticky_top: f32,
    },
    /// End of a sticky-positioned element region.
    StickyEnd,
    /// An interactive form field region.
    ///
    /// Emitted by the painter for `<input>`, `<textarea>`, `<select>` elements.
    /// The window tracks these for click-to-focus and basic text editing.
    FormField {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        /// The current value/text of the field.
        value: String,
        /// The type of form element ("text", "password", "textarea", "select", "checkbox", "radio",
        /// "email", "number", "date", "file", "hidden", "submit", "button", "reset").
        field_type: String,
        /// The `name` attribute of the form field (used for form submission).
        name: String,
        /// The `action` URL of the parent `<form>` element (empty if none).
        form_action: String,
        /// The `method` of the parent `<form>` element ("get" or "post", defaults to "get").
        form_method: String,
        /// The `enctype` of the parent `<form>` element (defaults to "application/x-www-form-urlencoded").
        form_enctype: String,
        /// The placeholder text for the field.
        placeholder: String,
        /// Whether the field is checked (for checkbox/radio).
        checked: bool,
        /// Whether the field is required (HTML5 validation).
        required: bool,
        /// Options for `<select>` elements: `(value, display_text, selected)`.
        options: Vec<(String, String, bool)>,
        /// Validation pattern (regex) from the `pattern` attribute.
        pattern: String,
        /// Minimum value for number inputs.
        min: String,
        /// Maximum value for number inputs.
        max: String,
        /// Maximum character length.
        maxlength: Option<usize>,
        /// Minimum character length.
        minlength: Option<usize>,
    },
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
    /// Fill a rectangle with rounded corners using an SDF approach.
    ///
    /// `radius` contains the corner radii in CSS order:
    /// `[top-left, top-right, bottom-right, bottom-left]`.
    FillRoundedRect {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
        /// Corner radii in pixels: [top-left, top-right, bottom-right, bottom-left].
        radius: [f32; 4],
    },
    /// Draw a box-shadow behind an element.
    ///
    /// Rendered as an offset filled rectangle with a semi-transparent color.
    /// True Gaussian blur is not implemented; this is a flat shadow.
    BoxShadow {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
        offset_x: f32,
        offset_y: f32,
        blur: f32,
    },
    /// Push an opacity layer. All subsequent render ops until the matching
    /// `PopOpacity` should have their alpha multiplied by this value.
    PushOpacity {
        opacity: f32,
    },
    /// Pop the current opacity layer, restoring the previous opacity.
    PopOpacity,
    /// Mark the start of a fixed-position element.
    ///
    /// The renderer should ignore scroll offsets for all ops between
    /// `FixedStart` and `FixedEnd`, painting them at their absolute
    /// viewport position regardless of scrolling.
    FixedStart,
    /// End of a fixed-position element region.
    FixedEnd,
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

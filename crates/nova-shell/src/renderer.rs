//! Software renderer — converts RenderCommands into a pixel buffer.
//!
//! This is a simple CPU rasterizer for phase 1.
//! It renders RenderOps into an RGBA pixel buffer that gets uploaded to a wgpu texture.
//! In the future, this will be replaced by Vello (GPU-native rendering).
//!
//! Text rendering uses `fontdue` for real TTF/OTF glyph rasterization with anti-aliasing.
//! If no font file is found, falls back to a built-in bitmap font.

use std::collections::HashMap;

use nova_mod_api::{Color, RenderCommands, RenderOp};

// ---------------------------------------------------------------------------
// Clip rectangle
// ---------------------------------------------------------------------------

/// An axis-aligned clip rectangle used to restrict rendering.
#[derive(Debug, Clone, Copy)]
struct ClipRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

// ---------------------------------------------------------------------------
// Font rendering
// ---------------------------------------------------------------------------

/// A cached rasterized glyph.
struct CachedGlyph {
    /// Glyph bitmap (coverage values 0–255), row-major.
    bitmap: Vec<u8>,
    metrics: fontdue::Metrics,
}

/// Which font variant to use for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FontVariant {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl FontVariant {
    /// Determine the variant from CSS font-weight and font-style values.
    fn from_css(weight: Option<u16>, style: Option<&str>) -> Self {
        let is_bold = weight.unwrap_or(400) >= 700;
        let is_italic = style
            .map(|s| s == "italic" || s == "oblique")
            .unwrap_or(false);
        match (is_bold, is_italic) {
            (true, true) => FontVariant::BoldItalic,
            (true, false) => FontVariant::Bold,
            (false, true) => FontVariant::Italic,
            (false, false) => FontVariant::Regular,
        }
    }
}

/// Font renderer backed by `fontdue` with support for bold/italic variants.
///
/// Loads up to 4 TTF fonts at startup (regular, bold, italic, bold-italic)
/// and rasterizes glyphs on demand, caching them in a `HashMap` keyed by
/// `(variant, char, font_size_in_tenths)`.
struct FontRenderer {
    regular: fontdue::Font,
    bold: Option<fontdue::Font>,
    italic: Option<fontdue::Font>,
    bold_italic: Option<fontdue::Font>,
    /// Cache keyed by (variant, character, font_size * 10 as u32).
    cache: HashMap<(FontVariant, char, u32), CachedGlyph>,
    /// Custom fonts loaded from `@font-face` rules, keyed by family name
    /// (case-insensitive — keys are stored lowercased).
    custom_fonts: HashMap<String, fontdue::Font>,
    /// Cache for custom font glyphs, keyed by (family_lowercase, character, font_size * 10).
    custom_cache: HashMap<(String, char, u32), CachedGlyph>,
}

impl FontRenderer {
    /// Try to create a `FontRenderer` from the bundled DejaVu Sans fonts.
    ///
    /// Loads the regular variant (required) plus bold, italic, and bold-italic
    /// variants if available. Returns `None` if the regular font is not found.
    fn new() -> Option<Self> {
        let regular_bytes = Self::find_font_bytes("DejaVuSans.ttf")?;
        let settings = fontdue::FontSettings {
            scale: 40.0, // hint at a common size for better hinting
            ..fontdue::FontSettings::default()
        };
        let regular = fontdue::Font::from_bytes(regular_bytes, settings).ok()?;

        let bold = Self::find_font_bytes("DejaVuSans-Bold.ttf")
            .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok());
        let italic = Self::find_font_bytes("DejaVuSans-Oblique.ttf")
            .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok());
        let bold_italic = Self::find_font_bytes("DejaVuSans-BoldOblique.ttf")
            .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok());

        tracing::info!(
            bold = bold.is_some(),
            italic = italic.is_some(),
            bold_italic = bold_italic.is_some(),
            "Font renderer initialized with variants"
        );

        // Debug: verify proportional metrics — 'i' should be much narrower than 'm'.
        {
            let (mi, _) = regular.rasterize('i', 16.0);
            let (mm, _) = regular.rasterize('m', 16.0);
            tracing::info!(
                i_advance = mi.advance_width,
                m_advance = mm.advance_width,
                "Font metrics at 16px: 'i' vs 'm' advance widths (proportional check)"
            );
        }

        Some(Self {
            regular,
            bold,
            italic,
            bold_italic,
            cache: HashMap::new(),
            custom_fonts: HashMap::new(),
            custom_cache: HashMap::new(),
        })
    }

    /// Load a custom font from raw TTF/OTF bytes.
    ///
    /// The font is stored under the given `family` name (lowercased for
    /// case-insensitive lookup). If parsing fails, the font is silently
    /// skipped with a warning.
    fn load_custom_font(&mut self, family: &str, data: Vec<u8>) {
        let key = family.to_lowercase();
        if self.custom_fonts.contains_key(&key) {
            return; // already loaded
        }
        let settings = fontdue::FontSettings::default();
        match fontdue::Font::from_bytes(data, settings) {
            Ok(font) => {
                tracing::info!(family = %family, "loaded custom @font-face font");
                self.custom_fonts.insert(key, font);
            }
            Err(e) => {
                tracing::warn!(family = %family, error = %e, "failed to parse @font-face font data");
            }
        }
    }

    /// Rasterize a glyph using a custom font, returning it from cache if available.
    fn rasterize_custom(&mut self, family_lower: &str, ch: char, font_size: f32) -> Option<&CachedGlyph> {
        let key = (family_lower.to_string(), ch, (font_size * 10.0).round() as u32);
        if !self.custom_cache.contains_key(&key) {
            let font = self.custom_fonts.get(family_lower)?;
            let (metrics, bitmap) = font.rasterize(ch, font_size);
            self.custom_cache.insert(key.clone(), CachedGlyph { bitmap, metrics });
        }
        self.custom_cache.get(&key)
    }

    /// Locate font bytes from well-known paths for a given filename.
    fn find_font_bytes(filename: &str) -> Option<Vec<u8>> {
        // 1. Look next to the workspace root (assets/fonts/).
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace_root = std::path::Path::new(manifest_dir)
            .parent() // crates/
            .and_then(|p| p.parent()); // workspace root

        if let Some(root) = workspace_root {
            let path = root.join("assets/fonts").join(filename);
            if let Ok(bytes) = std::fs::read(&path) {
                tracing::info!("Loaded font from {}", path.display());
                return Some(bytes);
            }
        }

        // 2. Common system paths (Linux / macOS) — only for regular.
        if filename == "DejaVuSans.ttf" {
            let system_paths: &[&str] = &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/TTF/DejaVuSans.ttf",
                "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
                "/System/Library/Fonts/Helvetica.ttc",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
                "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
                "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
                "/usr/share/fonts/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
            ];

            for path in system_paths {
                if let Ok(bytes) = std::fs::read(path) {
                    tracing::info!("Loaded system font from {path}");
                    return Some(bytes);
                }
            }
        }

        // System paths for bold/italic variants.
        let system_filename = match filename {
            "DejaVuSans-Bold.ttf" => Some("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
            "DejaVuSans-Oblique.ttf" => Some("/usr/share/fonts/truetype/dejavu/DejaVuSans-Oblique.ttf"),
            "DejaVuSans-BoldOblique.ttf" => Some("/usr/share/fonts/truetype/dejavu/DejaVuSans-BoldOblique.ttf"),
            _ => None,
        };
        if let Some(path) = system_filename {
            if let Ok(bytes) = std::fs::read(path) {
                tracing::info!("Loaded system font from {path}");
                return Some(bytes);
            }
        }

        if filename == "DejaVuSans.ttf" {
            tracing::warn!(
                "No TTF font found — text rendering will use the built-in bitmap fallback. \
                 Place a TTF font at assets/fonts/DejaVuSans.ttf for real font rendering."
            );
        }
        None
    }

    /// Look up the horizontal kerning adjustment between two characters.
    ///
    /// Returns the kern value in pixels (usually negative to tighten spacing)
    /// for the given font size. Uses the custom font if `custom_family` matches
    /// a loaded `@font-face` font, otherwise uses the appropriate built-in
    /// variant (currently always the regular font's kern table).
    fn kern(&self, left: char, right: char, font_size: f32, custom_family: &Option<String>) -> Option<f32> {
        if let Some(key) = custom_family {
            if let Some(font) = self.custom_fonts.get(key) {
                return font.horizontal_kern(left, right, font_size);
            }
        }
        self.regular.horizontal_kern(left, right, font_size)
    }

    /// Rasterize a glyph (or return it from cache) for a specific variant.
    fn rasterize(&mut self, ch: char, font_size: f32, variant: FontVariant) -> &CachedGlyph {
        let key = (variant, ch, (font_size * 10.0).round() as u32);
        // We need to get the font reference before the entry API borrows self.
        // Clone the font pointer data first.
        if !self.cache.contains_key(&key) {
            let font = match variant {
                FontVariant::Bold => self.bold.as_ref().unwrap_or(&self.regular),
                FontVariant::Italic => self.italic.as_ref().unwrap_or(&self.regular),
                FontVariant::BoldItalic => self.bold_italic.as_ref()
                    .or(self.bold.as_ref())
                    .unwrap_or(&self.regular),
                FontVariant::Regular => &self.regular,
            };
            let (metrics, bitmap) = font.rasterize(ch, font_size);
            self.cache.insert(key, CachedGlyph { bitmap, metrics });
        }
        self.cache.get(&key).unwrap()
    }
}

// ---------------------------------------------------------------------------
// Framebuffer
// ---------------------------------------------------------------------------

/// A simple software framebuffer.
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    /// RGBA pixel data, row-major, 4 bytes per pixel.
    pub pixels: Vec<u8>,
    /// Whether the pixel data has changed since the last GPU upload.
    /// Set to `true` when any rendering modifies the pixels;
    /// set to `false` after the data has been uploaded to the GPU texture.
    pub dirty: bool,
    /// Optional fontdue-based renderer (None when no font file is available).
    font_renderer: Option<FontRenderer>,
    /// Stack of clip rectangles. Rendering is restricted to the intersection of
    /// all active clip rects.
    clip_stack: Vec<ClipRect>,
    /// Extra y-offset applied by sticky positioning (reset each frame).
    translate_y_offset: f32,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let size = (width * height * 4) as usize;
        let pixels = vec![255u8; size]; // White background
        let font_renderer = FontRenderer::new();
        match &font_renderer {
            Some(_) => tracing::info!("TTF font renderer loaded successfully"),
            None => tracing::warn!("TTF font NOT found — using bitmap fallback"),
        }
        Self {
            width,
            height,
            pixels,
            dirty: true,
            font_renderer,
            clip_stack: Vec::new(),
            translate_y_offset: 0.0,
        }
    }

    /// Reset the framebuffer to a new size, reusing the font renderer.
    /// Use this instead of `new()` when rebuilding frames (avoids reloading the font).
    pub fn reset(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        let size = (width * height * 4) as usize;
        self.pixels.resize(size, 255);
        self.pixels.fill(255);
        self.dirty = true;
    }

    /// Load custom `@font-face` fonts into the font renderer.
    ///
    /// Each entry is `(family_name, font_bytes)`. Fonts that have already been
    /// loaded (same family name) are skipped. Only TTF/OTF data is accepted;
    /// fontdue will reject anything else.
    pub fn load_custom_fonts(&mut self, fonts: &[(String, Vec<u8>)]) {
        if let Some(ref mut renderer) = self.font_renderer {
            for (family, data) in fonts {
                renderer.load_custom_font(family, data.clone());
            }
        } else {
            tracing::warn!(
                "cannot load custom fonts: no font renderer available (no base font loaded)"
            );
        }
    }

    /// Measure the width of text at a given font size without rendering.
    /// Returns the width in pixels.
    pub fn measure_text_width(&mut self, text: &str, font_size: f32) -> f32 {
        if let Some(ref mut renderer) = self.font_renderer {
            let mut width: f32 = 0.0;
            let chars: Vec<char> = text.chars().collect();
            for i in 0..chars.len() {
                let glyph = renderer.rasterize(chars[i], font_size, FontVariant::Regular);
                width += glyph.metrics.advance_width as f32;
                // Apply kerning with the next character.
                if i + 1 < chars.len() {
                    if let Some(kern) = renderer.regular.horizontal_kern(chars[i], chars[i + 1], font_size) {
                        width += kern;
                    }
                }
            }
            width
        } else {
            // Fallback: monospace estimate
            let scale = font_size / 16.0;
            text.len() as f32 * 8.0 * scale
        }
    }

    /// Compute the effective clip bounds from the clip stack.
    ///
    /// Returns `(x0, y0, x1, y1)` representing the intersection of all active
    /// clip rectangles, or the full framebuffer bounds if the stack is empty.
    fn effective_clip(&self) -> (i32, i32, i32, i32) {
        if self.clip_stack.is_empty() {
            (0, 0, self.width as i32, self.height as i32)
        } else {
            let mut cx0 = 0i32;
            let mut cy0 = 0i32;
            let mut cx1 = self.width as i32;
            let mut cy1 = self.height as i32;
            for clip in &self.clip_stack {
                cx0 = cx0.max(clip.x);
                cy0 = cy0.max(clip.y);
                cx1 = cx1.min(clip.x + clip.width);
                cy1 = cy1.min(clip.y + clip.height);
            }
            (cx0, cy0, cx1, cy1)
        }
    }

    /// Clear the framebuffer with a color.
    pub fn clear(&mut self, color: Color) {
        let r = (color.r * 255.0) as u8;
        let g = (color.g * 255.0) as u8;
        let b = (color.b * 255.0) as u8;
        let a = (color.a * 255.0) as u8;
        for chunk in self.pixels.chunks_exact_mut(4) {
            chunk[0] = r;
            chunk[1] = g;
            chunk[2] = b;
            chunk[3] = a;
        }
    }

    /// Set a pixel at (x, y) with blending.
    #[inline]
    fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = ((y as u32 * self.width + x as u32) * 4) as usize;
        if idx + 3 >= self.pixels.len() {
            return;
        }

        let a = color.a;
        if a >= 1.0 {
            self.pixels[idx] = (color.r * 255.0) as u8;
            self.pixels[idx + 1] = (color.g * 255.0) as u8;
            self.pixels[idx + 2] = (color.b * 255.0) as u8;
            self.pixels[idx + 3] = 255;
        } else if a > 0.0 {
            // Alpha blend.
            let inv_a = 1.0 - a;
            let dst_r = self.pixels[idx] as f32 / 255.0;
            let dst_g = self.pixels[idx + 1] as f32 / 255.0;
            let dst_b = self.pixels[idx + 2] as f32 / 255.0;
            self.pixels[idx] = ((color.r * a + dst_r * inv_a) * 255.0) as u8;
            self.pixels[idx + 1] = ((color.g * a + dst_g * inv_a) * 255.0) as u8;
            self.pixels[idx + 2] = ((color.b * a + dst_b * inv_a) * 255.0) as u8;
            self.pixels[idx + 3] = 255;
        }
    }

    /// Blend a single coverage value from a glyph bitmap into the framebuffer.
    ///
    /// `coverage` is 0–255 (0 = fully transparent, 255 = fully opaque).
    /// The glyph colour is `color`, blended against the existing pixel.
    ///
    /// Uses gamma-correct compositing: the background is converted from sRGB
    /// to linear space, the blend is performed in linear, and the result is
    /// converted back to sRGB. This eliminates the dark fringing artefact that
    /// naive alpha blending produces on light backgrounds.
    #[inline]
    fn blend_glyph_pixel(&mut self, x: i32, y: i32, coverage: u8, color: Color) {
        if coverage == 0 || x < 0 || y < 0 {
            return;
        }
        let ux = x as u32;
        let uy = y as u32;
        if ux >= self.width || uy >= self.height {
            return;
        }

        let idx = ((uy * self.width + ux) * 4) as usize;
        if idx + 3 >= self.pixels.len() {
            return;
        }

        let alpha = (coverage as f32 / 255.0) * color.a;
        if alpha <= 0.0 {
            return;
        }

        // Read existing pixel (sRGB).
        let bg_r = self.pixels[idx] as f32 / 255.0;
        let bg_g = self.pixels[idx + 1] as f32 / 255.0;
        let bg_b = self.pixels[idx + 2] as f32 / 255.0;

        // Gamma-correct blending (approximate sRGB gamma = 2.2).
        #[inline]
        fn to_linear(v: f32) -> f32 {
            v * v
        } // simplified gamma
        #[inline]
        fn to_srgb(v: f32) -> f32 {
            v.sqrt()
        } // simplified inverse

        let r = to_srgb(to_linear(color.r) * alpha + to_linear(bg_r) * (1.0 - alpha));
        let g = to_srgb(to_linear(color.g) * alpha + to_linear(bg_g) * (1.0 - alpha));
        let b = to_srgb(to_linear(color.b) * alpha + to_linear(bg_b) * (1.0 - alpha));

        self.pixels[idx] = (r * 255.0).round().clamp(0.0, 255.0) as u8;
        self.pixels[idx + 1] = (g * 255.0).round().clamp(0.0, 255.0) as u8;
        self.pixels[idx + 2] = (b * 255.0).round().clamp(0.0, 255.0) as u8;
        self.pixels[idx + 3] = 255;
    }

    /// Fill a rectangle.
    ///
    /// Uses fast row-based operations: opaque fills use `copy_from_slice` per
    /// row (no per-pixel branch), semi-transparent fills alpha-blend in bulk.
    /// Fully transparent colours are skipped entirely.
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let x0 = x.round().max(0.0) as usize;
        let y0 = y.round().max(0.0) as usize;
        let x1 = (x + w).round().min(self.width as f32) as usize;
        let y1 = (y + h).round().min(self.height as f32) as usize;

        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let r = (color.r * 255.0).round().clamp(0.0, 255.0) as u8;
        let g = (color.g * 255.0).round().clamp(0.0, 255.0) as u8;
        let b = (color.b * 255.0).round().clamp(0.0, 255.0) as u8;
        let a = (color.a * 255.0).round().clamp(0.0, 255.0) as u8;

        let fb_w = self.width as usize;

        if a == 255 {
            // Opaque: direct write, no blending needed.
            let pixel = [r, g, b, 255u8];
            for row in y0..y1 {
                let row_start = row * fb_w * 4 + x0 * 4;
                let row_end = row * fb_w * 4 + x1 * 4;
                if row_end <= self.pixels.len() {
                    for chunk in self.pixels[row_start..row_end].chunks_exact_mut(4) {
                        chunk.copy_from_slice(&pixel);
                    }
                }
            }
        } else if a > 0 {
            // Semi-transparent: alpha blend per pixel.
            let alpha = color.a;
            let inv_alpha = 1.0 - alpha;
            for row in y0..y1 {
                for col in x0..x1 {
                    let idx = (row * fb_w + col) * 4;
                    if idx + 3 < self.pixels.len() {
                        let bg_r = self.pixels[idx] as f32 / 255.0;
                        let bg_g = self.pixels[idx + 1] as f32 / 255.0;
                        let bg_b = self.pixels[idx + 2] as f32 / 255.0;
                        self.pixels[idx] = ((color.r * alpha + bg_r * inv_alpha) * 255.0) as u8;
                        self.pixels[idx + 1] =
                            ((color.g * alpha + bg_g * inv_alpha) * 255.0) as u8;
                        self.pixels[idx + 2] =
                            ((color.b * alpha + bg_b * inv_alpha) * 255.0) as u8;
                        self.pixels[idx + 3] = 255;
                    }
                }
            }
        }
        // a == 0 → fully transparent, nothing to draw.
    }

    /// Fill a rectangle with rounded corners using an SDF-based approach.
    ///
    /// `radius` is `[top-left, top-right, bottom-right, bottom-left]` in pixels.
    /// For each pixel inside the bounding rect we compute the distance from the
    /// nearest corner arc.  Pixels inside the rounded shape are filled; pixels
    /// outside the corner arcs are skipped.
    pub fn fill_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Color,
        radius: [f32; 4],
    ) {
        // Clamp each radius so overlapping corners don't exceed half the side.
        let max_r_h = w * 0.5;
        let max_r_v = h * 0.5;
        let r_tl = radius[0].min(max_r_h).min(max_r_v).max(0.0);
        let r_tr = radius[1].min(max_r_h).min(max_r_v).max(0.0);
        let r_br = radius[2].min(max_r_h).min(max_r_v).max(0.0);
        let r_bl = radius[3].min(max_r_h).min(max_r_v).max(0.0);

        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        let x1 = (x + w).ceil() as i32;
        let y1 = (y + h).ceil() as i32;

        for py in y0..y1 {
            // Pixel center within the rect (local coordinates).
            let fy = py as f32 + 0.5 - y;
            for px in x0..x1 {
                let fx = px as f32 + 0.5 - x;

                // Determine which corner quadrant this pixel falls into and
                // check whether it is inside the rounded corner arc.
                let inside = if fx < r_tl && fy < r_tl {
                    // Top-left corner.
                    let dx = r_tl - fx;
                    let dy = r_tl - fy;
                    dx * dx + dy * dy <= r_tl * r_tl
                } else if fx > w - r_tr && fy < r_tr {
                    // Top-right corner.
                    let dx = fx - (w - r_tr);
                    let dy = r_tr - fy;
                    dx * dx + dy * dy <= r_tr * r_tr
                } else if fx > w - r_br && fy > h - r_br {
                    // Bottom-right corner.
                    let dx = fx - (w - r_br);
                    let dy = fy - (h - r_br);
                    dx * dx + dy * dy <= r_br * r_br
                } else if fx < r_bl && fy > h - r_bl {
                    // Bottom-left corner.
                    let dx = r_bl - fx;
                    let dy = fy - (h - r_bl);
                    dx * dx + dy * dy <= r_bl * r_bl
                } else {
                    // Not in any corner arc → always inside the rounded rect.
                    true
                };

                if inside {
                    self.set_pixel(px, py, color);
                }
            }
        }
    }

    /// Draw a horizontal line.
    fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Color) {
        for px in x0..x1 {
            self.set_pixel(px, y, color);
        }
    }

    /// Draw a rectangle border.
    pub fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color, line_width: f32) {
        let lw = line_width.round() as i32;
        let x0 = x.round() as i32;
        let y0 = y.round() as i32;
        let x1 = (x + w).round() as i32;
        let y1 = (y + h).round() as i32;

        // Top and bottom.
        for dy in 0..lw {
            self.hline(x0, x1, y0 + dy, color);
            self.hline(x0, x1, y1 - 1 - dy, color);
        }
        // Left and right.
        for py in y0..y1 {
            for dx in 0..lw {
                self.set_pixel(x0 + dx, py, color);
                self.set_pixel(x1 - 1 - dx, py, color);
            }
        }
    }

    /// Draw text using the fontdue renderer with glyph caching, anti-aliasing,
    /// and automatic line wrapping. Supports bold/italic via font variants.
    ///
    /// When `font_family` is `Some` and a matching custom `@font-face` font has
    /// been loaded, that font is used instead of the default DejaVu Sans.
    ///
    /// Falls back to the built-in bitmap font if no TTF font was loaded.
    pub fn draw_text(
        &mut self,
        x: f32,
        y: f32,
        text: &str,
        font_size: f32,
        color: Color,
        font_weight: Option<u16>,
        font_style: Option<&str>,
        font_family: Option<&str>,
        letter_spacing: Option<f32>,
    ) {
        if self.font_renderer.is_none() {
            tracing::warn!("Using bitmap font fallback — TTF font not loaded");
            self.draw_text_bitmap(x, y, text, font_size, color);
            return;
        }

        let variant = FontVariant::from_css(font_weight, font_style);

        // Check if we should use a custom @font-face font.
        let custom_family_key = font_family
            .map(|f| f.to_lowercase())
            .filter(|key| {
                self.font_renderer
                    .as_ref()
                    .map(|r| r.custom_fonts.contains_key(key))
                    .unwrap_or(false)
            });

        // We need to temporarily take the font renderer out of `self` so we can
        // mutably borrow both `self` (for pixel writes) and the renderer (for
        // caching). We put it back at the end.
        let mut renderer = self.font_renderer.take().unwrap();

        let fb_width = self.width as i32;

        let mut cx = x;
        let mut cy = y.round() as i32;
        let line_height = (font_size * 1.2).round() as i32;

        // Collect chars for indexed access (needed for look-ahead kerning).
        let chars: Vec<char> = text.chars().collect();

        for i in 0..chars.len() {
            let ch = chars[i];

            if ch == '\n' {
                cx = x;
                cy += line_height;
                continue;
            }

            // Rasterize: use the custom font if available, otherwise the default.
            let (metrics, bitmap) = if let Some(ref family_key) = custom_family_key {
                if let Some(glyph) = renderer.rasterize_custom(family_key, ch, font_size) {
                    (glyph.metrics, glyph.bitmap.clone())
                } else {
                    // Custom font didn't have this glyph — fall back to default.
                    let glyph = renderer.rasterize(ch, font_size, variant);
                    (glyph.metrics, glyph.bitmap.clone())
                }
            } else {
                let glyph = renderer.rasterize(ch, font_size, variant);
                (glyph.metrics, glyph.bitmap.clone())
            };

            // Line wrapping: if this glyph would exceed the framebuffer width,
            // wrap to the next line.
            if cx.round() as i32 + metrics.advance_width as i32 > fb_width && cx > x {
                cx = x;
                cy += line_height;
            }

            // Blit the glyph bitmap with subpixel horizontal positioning.
            let gx = cx.round() as i32 + metrics.xmin;
            let gy = cy - metrics.ymin; // fontdue ymin is distance from baseline up
            let bw = metrics.width;
            let bh = metrics.height;

            for row in 0..bh {
                for col in 0..bw {
                    let coverage = bitmap[row * bw + col];
                    self.blend_glyph_pixel(
                        gx + col as i32,
                        gy + row as i32,
                        coverage,
                        color,
                    );
                }
            }

            // Synthetic bold: draw glyph again offset by 1px for extra weight.
            // This makes bold text visually thicker even at small sizes.
            if matches!(variant, FontVariant::Bold | FontVariant::BoldItalic) {
                for row in 0..bh {
                    for col in 0..bw {
                        let coverage = bitmap[row * bw + col];
                        self.blend_glyph_pixel(
                            gx + col as i32 + 1, // 1px offset
                            gy + row as i32,
                            coverage / 2, // Half coverage for the extra pass
                            color,
                        );
                    }
                }
            }

            cx += metrics.advance_width;

            // Apply kerning with the next character if available.
            if i + 1 < chars.len() && chars[i + 1] != '\n' {
                if let Some(kern) = renderer.kern(ch, chars[i + 1], font_size, &custom_family_key) {
                    cx += kern;
                }
            }

            if let Some(ls) = letter_spacing {
                cx += ls;
            }
        }

        // Put the renderer back.
        self.font_renderer = Some(renderer);
    }

    /// Draw text using the built-in bitmap font (fallback).
    ///
    /// This is the original 8x16 monospace bitmap renderer, used when no TTF
    /// font is available.
    fn draw_text_bitmap(&mut self, x: f32, y: f32, text: &str, font_size: f32, color: Color) {
        let scale = (font_size / 16.0).max(0.5);
        let char_w = (8.0 * scale) as i32;
        let char_h = (16.0 * scale) as i32;
        let fb_width = self.width as i32;

        let mut cx = x.round() as i32;
        let mut cy = y.round() as i32;

        for ch in text.chars() {
            if ch == '\n' {
                cx = x.round() as i32;
                cy += char_h;
                continue;
            }

            // Line wrapping.
            if cx + char_w > fb_width && cx > x.round() as i32 {
                cx = x.round() as i32;
                cy += char_h;
            }

            if ch == ' ' {
                cx += char_w;
                continue;
            }

            let glyph = get_basic_glyph(ch);
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if bits & (1 << (7 - col)) != 0 {
                        let px = cx + (col as f32 * scale) as i32;
                        let py = cy + (row as f32 * scale) as i32;
                        self.set_pixel(px, py, color);
                        // If scaling up, fill the scaled pixel.
                        if scale > 1.0 {
                            let s = scale.ceil() as i32;
                            for dy in 0..s {
                                for dx in 0..s {
                                    self.set_pixel(px + dx, py + dy, color);
                                }
                            }
                        }
                    }
                }
            }

            cx += char_w;
        }
    }

    /// Render a full set of RenderCommands.
    pub fn render(&mut self, commands: &RenderCommands) {
        self.render_with_offset(commands, 0.0);
    }

    /// Render a full set of RenderCommands with a vertical offset applied to all operations.
    ///
    /// This is used to shift page content down to make room for the URL bar.
    pub fn render_with_offset(&mut self, commands: &RenderCommands, y_offset: f32) {
        self.render_scrolled(commands, y_offset, 0.0, 0.0, 0.0);
    }

    /// Render commands with both a static y-offset (e.g. URL bar) and a scroll offset.
    ///
    /// `y_offset` shifts content down (for chrome elements above the page).
    /// `scroll_y` shifts content up (the user has scrolled down by this many pixels).
    /// `content_height` is the total height of the rendered content (for the scrollbar).
    /// If `content_height` is 0, no scrollbar is drawn.
    pub fn render_scrolled(
        &mut self,
        commands: &RenderCommands,
        y_offset: f32,
        scroll_x: f32,
        scroll_y: f32,
        content_height: f32,
    ) {
        self.clear(Color::WHITE);
        self.clip_stack.clear();
        self.translate_y_offset = 0.0;

        // Load any custom @font-face fonts that haven't been loaded yet.
        if !commands.fonts.is_empty() {
            self.load_custom_fonts(&commands.fonts);
        }

        let sx = scroll_x;
        let fb_height = self.height as f32;

        for op in &commands.ops {
            let sy_extra = self.translate_y_offset;

            // ---- Off-screen culling ----
            // Skip ops whose vertical position is entirely above or below the
            // visible viewport. We use a generous margin (1000px below the
            // element's y and 100px past the bottom) to avoid clipping
            // tall elements or text that overflows its origin.
            if let Some(op_y) = get_op_y(op) {
                let screen_y = op_y + y_offset - scroll_y + sy_extra;
                if screen_y > fb_height + 100.0 || screen_y + 1000.0 < 0.0 {
                    continue;
                }
            }
            match op {
                RenderOp::FillRect { x, y, width, height, color } => {
                    self.fill_rect(*x - sx, *y + y_offset - scroll_y + sy_extra, *width, *height, *color);
                }
                RenderOp::DrawText { x, y, text, font_size, color, font_weight, font_style, font_family, letter_spacing } => {
                    self.draw_text(
                        *x - sx, *y + y_offset - scroll_y + sy_extra, text, *font_size, *color,
                        *font_weight, font_style.as_deref(), font_family.as_deref(), *letter_spacing,
                    );
                }
                RenderOp::StrokeRect { x, y, width, height, color, width_px } => {
                    self.stroke_rect(*x - sx, *y + y_offset - scroll_y + sy_extra, *width, *height, *color, *width_px);
                }
                RenderOp::DrawImage {
                    x, y, width, height,
                    img_width, img_height, pixels,
                } => {
                    self.draw_image(
                        *x - sx, *y + y_offset - scroll_y + sy_extra, *width, *height,
                        *img_width, *img_height, pixels,
                    );
                }
                RenderOp::PushClip { x, y, width, height } => {
                    self.clip_stack.push(ClipRect {
                        x: (*x - sx).round() as i32,
                        y: (*y + y_offset - scroll_y + sy_extra).round() as i32,
                        width: width.round() as i32,
                        height: height.round() as i32,
                    });
                }
                RenderOp::PopClip => {
                    self.clip_stack.pop();
                }
                RenderOp::FillRoundedRect { x, y, width, height, color, radius } => {
                    self.fill_rounded_rect(
                        *x - sx, *y + y_offset - scroll_y + sy_extra, *width, *height, *color, *radius,
                    );
                }
                RenderOp::BoxShadow {
                    x, y, width, height, color, offset_x, offset_y, blur: _,
                } => {
                    self.fill_rect(
                        *x + *offset_x - sx,
                        *y + *offset_y + y_offset - scroll_y + sy_extra,
                        *width,
                        *height,
                        *color,
                    );
                }
                // Sticky positioning: adjust the y-offset for subsequent ops
                // so the element sticks to the viewport during scroll.
                RenderOp::StickyStart { original_y, sticky_top } => {
                    // If the element would scroll above sticky_top in the viewport,
                    // push a translation to keep it at sticky_top.
                    let element_viewport_y = *original_y + y_offset - scroll_y;
                    if element_viewport_y < y_offset + *sticky_top {
                        let offset = (y_offset + *sticky_top) - element_viewport_y;
                        self.translate_y_offset += offset;
                    }
                }
                RenderOp::StickyEnd => {
                    self.translate_y_offset = 0.0;
                }
                // Scrollable container: clip child rendering to the container bounds.
                // Internal scroll state is tracked by the window; for now this
                // behaves like PushClip/PopClip.
                RenderOp::ScrollContainerStart { x, y, width, height, .. } => {
                    self.clip_stack.push(ClipRect {
                        x: (*x - sx).round() as i32,
                        y: (*y + y_offset - scroll_y + sy_extra).round() as i32,
                        width: width.round() as i32,
                        height: height.round() as i32,
                    });
                }
                RenderOp::ScrollContainerEnd => {
                    self.clip_stack.pop();
                }
                // Form field rendering: draw visual indicators on top of the
                // background/text that the painter already emitted.
                RenderOp::FormField { x, y, width, height, field_type, .. } => {
                    let fx = *x - sx;
                    let fy = *y + y_offset - scroll_y + sy_extra;
                    let fw = *width;
                    let fh = *height;

                    match field_type.as_str() {
                        "select" => {
                            // Draw a 1px border around the select box.
                            let border_color = Color::rgb(0.6, 0.6, 0.6);
                            self.stroke_rect(fx, fy, fw, fh, border_color, 1.0);

                            // Draw a separator line before the arrow area.
                            let arrow_area_w = 20.0;
                            let sep_x = fx + fw - arrow_area_w;
                            let sep_color = Color::rgb(0.78, 0.78, 0.78);
                            self.fill_rect(sep_x, fy + 1.0, 1.0, fh - 2.0, sep_color);

                            // Draw a light background for the arrow area.
                            let arrow_bg = Color::rgb(0.92, 0.92, 0.92);
                            self.fill_rect(sep_x + 1.0, fy + 1.0, arrow_area_w - 2.0, fh - 2.0, arrow_bg);

                            // Draw a downward triangle (▼) in the arrow area.
                            let arrow_size = 6.0_f32;
                            let arrow_cx = sep_x + arrow_area_w / 2.0;
                            let arrow_cy = fy + (fh - arrow_size * 0.6) / 2.0;
                            let arrow_color = Color::rgb(0.35, 0.35, 0.35);
                            for row in 0..=(arrow_size as i32) {
                                // Each row of the triangle is narrower.
                                let half = ((arrow_size as i32 - row) as f32 / 2.0) as i32;
                                let py = (arrow_cy + row as f32).round() as i32;
                                for col in -half..=half {
                                    let px = (arrow_cx + col as f32).round() as i32;
                                    self.set_pixel(px, py, arrow_color);
                                }
                            }
                        }
                        "checkbox" => {
                            // Draw a checkbox outline (square).
                            let size = fw.min(fh).min(16.0);
                            let cx = fx + (fw - size) / 2.0;
                            let cy = fy + (fh - size) / 2.0;
                            let outline_color = Color::rgb(0.4, 0.4, 0.4);
                            // White fill inside.
                            self.fill_rect(cx, cy, size, size, Color::WHITE);
                            self.stroke_rect(cx, cy, size, size, outline_color, 1.0);
                        }
                        "radio" => {
                            // Draw a radio circle (approximated with rounded rect).
                            let size = fw.min(fh).min(16.0);
                            let cx = fx + (fw - size) / 2.0;
                            let cy = fy + (fh - size) / 2.0;
                            let outline_color = Color::rgb(0.4, 0.4, 0.4);
                            // White fill.
                            let r = size / 2.0;
                            self.fill_rounded_rect(cx, cy, size, size, Color::WHITE, [r, r, r, r]);
                            // Border via stroke (square for now — the rounded fill gives the circle shape).
                            // Draw circle outline using scanlines.
                            let center_x = cx + r;
                            let center_y = cy + r;
                            for py_i in 0..=(size as i32) {
                                for px_i in 0..=(size as i32) {
                                    let dx = px_i as f32 - r;
                                    let dy = py_i as f32 - r;
                                    let dist = (dx * dx + dy * dy).sqrt();
                                    if dist >= r - 1.0 && dist <= r {
                                        self.set_pixel(
                                            (cx + px_i as f32).round() as i32,
                                            (cy + py_i as f32).round() as i32,
                                            outline_color,
                                        );
                                    }
                                }
                            }
                        }
                        _ => {
                            // Generic form field: just draw a subtle border.
                            let border_color = Color::rgb(0.7, 0.7, 0.7);
                            self.stroke_rect(fx, fy, fw, fh, border_color, 1.0);
                        }
                    }
                }
                // Link ops are metadata-only; they don't draw anything.
                RenderOp::Link { .. } => {}
                // Other ops will be implemented as needed.
                _ => {}
            }
        }

        // Draw vertical scrollbar if content is taller than the page area.
        let page_area_height = (self.height as f32 - y_offset).max(0.0);
        let page_area_width = self.width as f32;
        if content_height > page_area_height && content_height > 0.0 {
            self.draw_scrollbar(scroll_y, content_height, page_area_height, y_offset);
        }

        // Draw horizontal scrollbar if content is wider than the viewport.
        if page_area_width > 0.0 {
            let content_w = self.compute_content_width_from_ops(commands);
            if content_w > page_area_width {
                self.draw_horizontal_scrollbar(scroll_x, content_w, page_area_width, y_offset + page_area_height);
            }
        }
    }

    /// Compute content width from render ops (max x + width).
    fn compute_content_width_from_ops(&self, commands: &RenderCommands) -> f32 {
        let mut max_x: f32 = 0.0;
        for op in &commands.ops {
            let right = match op {
                RenderOp::FillRect { x, width, .. } => x + width,
                RenderOp::DrawText { x, text, font_size, .. } => x + text.len() as f32 * font_size * 0.6,
                RenderOp::StrokeRect { x, width, .. } => x + width,
                RenderOp::DrawImage { x, width, .. } => x + width,
                _ => 0.0,
            };
            if right > max_x { max_x = right; }
        }
        max_x
    }

    /// Draw a thin horizontal scrollbar at the bottom of the page area.
    fn draw_horizontal_scrollbar(
        &mut self,
        scroll_x: f32,
        content_width: f32,
        page_area_width: f32,
        bar_y: f32,
    ) {
        let bar_height: f32 = 8.0;
        let bar_y = bar_y - bar_height; // Draw just above the bottom edge.

        // Track.
        let track_color = Color::rgba(0.85, 0.85, 0.85, 0.6);
        self.fill_rect(0.0, bar_y, page_area_width, bar_height, track_color);

        // Thumb.
        let thumb_width = (page_area_width / content_width * page_area_width).max(20.0);
        let max_scroll = (content_width - page_area_width).max(0.0);
        let scroll_ratio = if max_scroll > 0.0 { scroll_x / max_scroll } else { 0.0 };
        let thumb_x = scroll_ratio * (page_area_width - thumb_width);

        let thumb_color = Color::rgba(0.5, 0.5, 0.5, 0.7);
        self.fill_rect(thumb_x, bar_y, thumb_width, bar_height, thumb_color);
    }

    /// Draw a thin scrollbar on the right edge of the page area.
    ///
    /// Renders a semi-transparent gray track with a proportional thumb whose
    /// position reflects the current scroll offset. The scrollbar starts below
    /// `y_offset` (e.g. the URL bar) so it does not overlap browser chrome.
    fn draw_scrollbar(
        &mut self,
        scroll_y: f32,
        content_height: f32,
        page_area_height: f32,
        y_offset: f32,
    ) {
        let bar_width: f32 = 8.0;
        let bar_x = self.width as f32 - bar_width;

        // Track: light gray, semi-transparent. Starts below the URL bar.
        let track_color = Color::rgba(0.85, 0.85, 0.85, 0.6);
        self.fill_rect(bar_x, y_offset, bar_width, page_area_height, track_color);

        // Thumb: proportional size and position.
        let thumb_height = (page_area_height / content_height * page_area_height).max(20.0);
        let max_scroll = (content_height - page_area_height).max(0.0);
        let scroll_ratio = if max_scroll > 0.0 {
            scroll_y / max_scroll
        } else {
            0.0
        };
        let thumb_y = y_offset + scroll_ratio * (page_area_height - thumb_height);

        let thumb_color = Color::rgba(0.5, 0.5, 0.5, 0.7);
        self.fill_rect(bar_x, thumb_y, bar_width, thumb_height, thumb_color);
    }

    /// Blit a decoded RGBA image into the framebuffer, scaling with
    /// nearest-neighbour sampling from the source (`img_width` x `img_height`)
    /// to the destination rectangle (`dst_w` x `dst_h` at `dst_x, dst_y`).
    ///
    /// Respects the current clip stack — pixels outside the effective clip
    /// bounds are skipped.
    pub fn draw_image(
        &mut self,
        dst_x: f32,
        dst_y: f32,
        dst_w: f32,
        dst_h: f32,
        img_width: u32,
        img_height: u32,
        pixels: &[u8],
    ) {
        let expected_len = (img_width as usize) * (img_height as usize) * 4;
        if pixels.len() < expected_len || img_width == 0 || img_height == 0 {
            return;
        }

        let (clip_x0, clip_y0, clip_x1, clip_y1) = self.effective_clip();

        let dx0 = dst_x.round() as i32;
        let dy0 = dst_y.round() as i32;
        let dx1 = (dst_x + dst_w).round() as i32;
        let dy1 = (dst_y + dst_h).round() as i32;

        let dest_w = (dx1 - dx0).max(1) as f32;
        let dest_h = (dy1 - dy0).max(1) as f32;

        for py in dy0..dy1 {
            if py < 0 || py >= self.height as i32 {
                continue;
            }
            if py < clip_y0 || py >= clip_y1 {
                continue;
            }
            // Map destination y to source y (nearest-neighbour).
            let sy = (((py - dy0) as f32 / dest_h) * img_height as f32) as u32;
            let sy = sy.min(img_height - 1);

            for px in dx0..dx1 {
                if px < 0 || px >= self.width as i32 {
                    continue;
                }
                if px < clip_x0 || px >= clip_x1 {
                    continue;
                }
                // Map destination x to source x.
                let sx = (((px - dx0) as f32 / dest_w) * img_width as f32) as u32;
                let sx = sx.min(img_width - 1);

                let src_idx = ((sy * img_width + sx) * 4) as usize;
                let r = pixels[src_idx] as f32 / 255.0;
                let g = pixels[src_idx + 1] as f32 / 255.0;
                let b = pixels[src_idx + 2] as f32 / 255.0;
                let a = pixels[src_idx + 3] as f32 / 255.0;

                self.set_pixel(px, py, Color { r, g, b, a });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Off-screen culling helper
// ---------------------------------------------------------------------------

/// Extract the Y coordinate from a `RenderOp`, if applicable.
///
/// Returns `None` for ops that are purely structural (e.g. `PopClip`,
/// `StickyEnd`, `Save`, `Restore`) — those should never be skipped.
fn get_op_y(op: &RenderOp) -> Option<f32> {
    match op {
        RenderOp::FillRect { y, .. }
        | RenderOp::DrawText { y, .. }
        | RenderOp::StrokeRect { y, .. }
        | RenderOp::DrawImage { y, .. }
        | RenderOp::FillRoundedRect { y, .. }
        | RenderOp::BoxShadow { y, .. }
        | RenderOp::FormField { y, .. }
        | RenderOp::DrawTexture { y, .. } => Some(*y),
        // Structural / state ops — never cull these.
        RenderOp::PushClip { .. }
        | RenderOp::PopClip
        | RenderOp::StickyStart { .. }
        | RenderOp::StickyEnd
        | RenderOp::ScrollContainerStart { .. }
        | RenderOp::ScrollContainerEnd
        | RenderOp::Link { .. }
        | RenderOp::Translate { .. }
        | RenderOp::Save
        | RenderOp::Restore => None,
    }
}

// ---------------------------------------------------------------------------
// Built-in bitmap font (fallback)
// ---------------------------------------------------------------------------

/// Get a basic 8x16 bitmap glyph for a character.
/// This is a tiny built-in font covering ASCII printable range.
fn get_basic_glyph(ch: char) -> [u8; 16] {
    // Very basic bitmap font — just enough to see text on screen.
    // Each byte is a row of 8 pixels (MSB = leftmost).
    match ch {
        'A' => [0x00,0x18,0x3C,0x66,0x66,0x7E,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00],
        'B' => [0x00,0x7C,0x66,0x66,0x7C,0x66,0x66,0x66,0x7C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'C' => [0x00,0x3C,0x66,0x60,0x60,0x60,0x60,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'D' => [0x00,0x78,0x6C,0x66,0x66,0x66,0x66,0x6C,0x78,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'E' => [0x00,0x7E,0x60,0x60,0x7C,0x60,0x60,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'F' => [0x00,0x7E,0x60,0x60,0x7C,0x60,0x60,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'G' => [0x00,0x3C,0x66,0x60,0x60,0x6E,0x66,0x66,0x3E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'H' => [0x00,0x66,0x66,0x66,0x7E,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'I' => [0x00,0x3C,0x18,0x18,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'J' => [0x00,0x1E,0x0C,0x0C,0x0C,0x0C,0x0C,0x6C,0x38,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'K' => [0x00,0x66,0x6C,0x78,0x70,0x78,0x6C,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'L' => [0x00,0x60,0x60,0x60,0x60,0x60,0x60,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'M' => [0x00,0x63,0x77,0x7F,0x6B,0x63,0x63,0x63,0x63,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'N' => [0x00,0x66,0x76,0x7E,0x7E,0x6E,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'O' => [0x00,0x3C,0x66,0x66,0x66,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'P' => [0x00,0x7C,0x66,0x66,0x7C,0x60,0x60,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'Q' => [0x00,0x3C,0x66,0x66,0x66,0x66,0x6E,0x3C,0x0E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'R' => [0x00,0x7C,0x66,0x66,0x7C,0x78,0x6C,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'S' => [0x00,0x3C,0x66,0x60,0x3C,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'T' => [0x00,0x7E,0x18,0x18,0x18,0x18,0x18,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'U' => [0x00,0x66,0x66,0x66,0x66,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'V' => [0x00,0x66,0x66,0x66,0x66,0x66,0x3C,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'W' => [0x00,0x63,0x63,0x63,0x6B,0x7F,0x77,0x63,0x63,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'X' => [0x00,0x66,0x66,0x3C,0x18,0x3C,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'Y' => [0x00,0x66,0x66,0x66,0x3C,0x18,0x18,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'Z' => [0x00,0x7E,0x06,0x0C,0x18,0x30,0x60,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'a' => [0x00,0x00,0x00,0x3C,0x06,0x3E,0x66,0x66,0x3E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'b' => [0x00,0x60,0x60,0x7C,0x66,0x66,0x66,0x66,0x7C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'c' => [0x00,0x00,0x00,0x3C,0x66,0x60,0x60,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'd' => [0x00,0x06,0x06,0x3E,0x66,0x66,0x66,0x66,0x3E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'e' => [0x00,0x00,0x00,0x3C,0x66,0x7E,0x60,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'f' => [0x00,0x1C,0x36,0x30,0x7C,0x30,0x30,0x30,0x30,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'g' => [0x00,0x00,0x00,0x3E,0x66,0x66,0x66,0x3E,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00],
        'h' => [0x00,0x60,0x60,0x7C,0x66,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'i' => [0x00,0x18,0x00,0x38,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'j' => [0x00,0x06,0x00,0x06,0x06,0x06,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00],
        'k' => [0x00,0x60,0x60,0x66,0x6C,0x78,0x6C,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'l' => [0x00,0x38,0x18,0x18,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'm' => [0x00,0x00,0x00,0x66,0x7F,0x7F,0x6B,0x63,0x63,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'n' => [0x00,0x00,0x00,0x7C,0x66,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'o' => [0x00,0x00,0x00,0x3C,0x66,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'p' => [0x00,0x00,0x00,0x7C,0x66,0x66,0x66,0x7C,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00],
        'q' => [0x00,0x00,0x00,0x3E,0x66,0x66,0x66,0x3E,0x06,0x06,0x00,0x00,0x00,0x00,0x00,0x00],
        'r' => [0x00,0x00,0x00,0x7C,0x66,0x60,0x60,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        's' => [0x00,0x00,0x00,0x3E,0x60,0x3C,0x06,0x06,0x7C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        't' => [0x00,0x30,0x30,0x7C,0x30,0x30,0x30,0x36,0x1C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'u' => [0x00,0x00,0x00,0x66,0x66,0x66,0x66,0x66,0x3E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'v' => [0x00,0x00,0x00,0x66,0x66,0x66,0x3C,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'w' => [0x00,0x00,0x00,0x63,0x6B,0x7F,0x7F,0x36,0x36,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'x' => [0x00,0x00,0x00,0x66,0x3C,0x18,0x3C,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'y' => [0x00,0x00,0x00,0x66,0x66,0x66,0x3E,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00],
        'z' => [0x00,0x00,0x00,0x7E,0x0C,0x18,0x30,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '0' => [0x00,0x3C,0x66,0x6E,0x76,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '1' => [0x00,0x18,0x38,0x18,0x18,0x18,0x18,0x18,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '2' => [0x00,0x3C,0x66,0x06,0x0C,0x18,0x30,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '3' => [0x00,0x3C,0x66,0x06,0x1C,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '4' => [0x00,0x0C,0x1C,0x3C,0x6C,0x7E,0x0C,0x0C,0x0C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '5' => [0x00,0x7E,0x60,0x7C,0x06,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '6' => [0x00,0x3C,0x66,0x60,0x7C,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '7' => [0x00,0x7E,0x06,0x0C,0x18,0x18,0x18,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '8' => [0x00,0x3C,0x66,0x66,0x3C,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '9' => [0x00,0x3C,0x66,0x66,0x3E,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '.' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        ',' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x18,0x18,0x30,0x00,0x00,0x00,0x00,0x00,0x00],
        ':' => [0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        ';' => [0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x18,0x18,0x30,0x00,0x00,0x00,0x00,0x00,0x00],
        '!' => [0x00,0x18,0x18,0x18,0x18,0x18,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '?' => [0x00,0x3C,0x66,0x06,0x0C,0x18,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '/' => [0x00,0x06,0x06,0x0C,0x18,0x30,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '-' => [0x00,0x00,0x00,0x00,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '_' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '(' => [0x00,0x0C,0x18,0x30,0x30,0x30,0x30,0x18,0x0C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        ')' => [0x00,0x30,0x18,0x0C,0x0C,0x0C,0x0C,0x18,0x30,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '<' => [0x00,0x06,0x0C,0x18,0x30,0x18,0x0C,0x06,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '>' => [0x00,0x60,0x30,0x18,0x0C,0x18,0x30,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '=' => [0x00,0x00,0x00,0x7E,0x00,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '+' => [0x00,0x00,0x18,0x18,0x7E,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '"' => [0x00,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '\'' => [0x00,0x18,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        // Default: small filled rectangle for unknown chars.
        _ => [0x00,0x00,0x3C,0x3C,0x3C,0x3C,0x3C,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
    }
}

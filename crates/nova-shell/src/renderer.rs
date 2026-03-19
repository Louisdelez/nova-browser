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
// Font rendering
// ---------------------------------------------------------------------------

/// A cached rasterized glyph.
struct CachedGlyph {
    /// Glyph bitmap (coverage values 0–255), row-major.
    bitmap: Vec<u8>,
    metrics: fontdue::Metrics,
}

/// Font renderer backed by `fontdue`.
///
/// Loads a TTF font at startup and rasterizes glyphs on demand, caching them
/// in a `HashMap` keyed by `(char, font_size_in_tenths)` so that different
/// sizes are cached independently while avoiding floating-point keys.
struct FontRenderer {
    font: fontdue::Font,
    /// Cache keyed by (character, font_size * 10 as u32) for sub-pixel size granularity.
    cache: HashMap<(char, u32), CachedGlyph>,
}

impl FontRenderer {
    /// Try to create a `FontRenderer` from the bundled DejaVu Sans font.
    ///
    /// Font search order:
    /// 1. `assets/fonts/DejaVuSans.ttf` relative to the workspace root
    ///    (detected via `CARGO_MANIFEST_DIR` at compile time).
    /// 2. Common system font paths.
    ///
    /// Returns `None` if no usable font is found.
    fn new() -> Option<Self> {
        let font_bytes = Self::find_font_bytes()?;
        let settings = fontdue::FontSettings::default();
        let font = fontdue::Font::from_bytes(font_bytes, settings).ok()?;
        Some(Self {
            font,
            cache: HashMap::new(),
        })
    }

    /// Locate font bytes from well-known paths.
    fn find_font_bytes() -> Option<Vec<u8>> {
        // 1. Look next to the workspace root (assets/fonts/DejaVuSans.ttf).
        //    CARGO_MANIFEST_DIR points to crates/nova-shell/ at compile time.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace_root = std::path::Path::new(manifest_dir)
            .parent() // crates/
            .and_then(|p| p.parent()); // workspace root

        if let Some(root) = workspace_root {
            let path = root.join("assets/fonts/DejaVuSans.ttf");
            if let Ok(bytes) = std::fs::read(&path) {
                tracing::info!("Loaded font from {}", path.display());
                return Some(bytes);
            }
        }

        // 2. Common system paths (Linux / macOS).
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

        tracing::warn!(
            "No TTF font found — text rendering will use the built-in bitmap fallback. \
             Place a TTF font at assets/fonts/DejaVuSans.ttf for real font rendering."
        );
        None
    }

    /// Rasterize a glyph (or return it from cache).
    fn rasterize(&mut self, ch: char, font_size: f32) -> &CachedGlyph {
        let key = (ch, (font_size * 10.0).round() as u32);
        self.cache.entry(key).or_insert_with(|| {
            let (metrics, bitmap) = self.font.rasterize(ch, font_size);
            CachedGlyph { bitmap, metrics }
        })
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
    /// Optional fontdue-based renderer (None when no font file is available).
    font_renderer: Option<FontRenderer>,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let size = (width * height * 4) as usize;
        let pixels = vec![255u8; size]; // White background
        let font_renderer = FontRenderer::new();
        Self {
            width,
            height,
            pixels,
            font_renderer,
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
    #[inline]
    fn blend_glyph_pixel(&mut self, x: i32, y: i32, coverage: u8, color: Color) {
        if coverage == 0 {
            return;
        }
        let alpha = (coverage as f32 / 255.0) * color.a;
        self.set_pixel(
            x,
            y,
            Color {
                r: color.r,
                g: color.g,
                b: color.b,
                a: alpha,
            },
        );
    }

    /// Fill a rectangle.
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let x0 = x.round() as i32;
        let y0 = y.round() as i32;
        let x1 = (x + w).round() as i32;
        let y1 = (y + h).round() as i32;

        for py in y0..y1 {
            for px in x0..x1 {
                self.set_pixel(px, py, color);
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
    /// and automatic line wrapping.
    ///
    /// Falls back to the built-in bitmap font if no TTF font was loaded.
    pub fn draw_text(&mut self, x: f32, y: f32, text: &str, font_size: f32, color: Color) {
        if self.font_renderer.is_none() {
            self.draw_text_bitmap(x, y, text, font_size, color);
            return;
        }

        // We need to temporarily take the font renderer out of `self` so we can
        // mutably borrow both `self` (for pixel writes) and the renderer (for
        // caching). We put it back at the end.
        let mut renderer = self.font_renderer.take().unwrap();

        let fb_width = self.width as i32;

        let mut cx = x.round() as i32;
        let mut cy = y.round() as i32;
        let line_height = (font_size * 1.2).round() as i32;

        for ch in text.chars() {
            if ch == '\n' {
                cx = x.round() as i32;
                cy += line_height;
                continue;
            }

            let glyph = renderer.rasterize(ch, font_size);
            let metrics = glyph.metrics;

            // Line wrapping: if this glyph would exceed the framebuffer width,
            // wrap to the next line.
            if cx + metrics.advance_width as i32 > fb_width && cx > x.round() as i32 {
                cx = x.round() as i32;
                cy += line_height;
            }

            // Blit the glyph bitmap.
            let gx = cx + metrics.xmin;
            let gy = cy - metrics.ymin; // fontdue ymin is distance from baseline up
            let bw = metrics.width;
            let bh = metrics.height;

            // Safety: we clone the bitmap slice to avoid borrow issues.
            // The bitmap is typically small (< 2 KB), so this is cheap.
            let bitmap = glyph.bitmap.clone();

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

            cx += metrics.advance_width as i32;
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
        self.render_scrolled(commands, y_offset, 0.0, 0.0);
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
        scroll_y: f32,
        content_height: f32,
    ) {
        self.clear(Color::WHITE);

        for op in &commands.ops {
            match op {
                RenderOp::FillRect { x, y, width, height, color } => {
                    self.fill_rect(*x, *y + y_offset - scroll_y, *width, *height, *color);
                }
                RenderOp::DrawText { x, y, text, font_size, color } => {
                    self.draw_text(*x, *y + y_offset - scroll_y, text, *font_size, *color);
                }
                RenderOp::StrokeRect { x, y, width, height, color, width_px } => {
                    self.stroke_rect(*x, *y + y_offset - scroll_y, *width, *height, *color, *width_px);
                }
                RenderOp::DrawImage {
                    x, y, width, height,
                    img_width, img_height, pixels,
                } => {
                    self.draw_image(
                        *x, *y + y_offset - scroll_y, *width, *height,
                        *img_width, *img_height, pixels,
                    );
                }
                // Link ops are metadata-only; they don't draw anything.
                RenderOp::Link { .. } => {}
                // Other ops will be implemented as needed.
                _ => {}
            }
        }

        // Draw the scrollbar if the content is taller than the page area.
        let page_area_height = (self.height as f32 - y_offset).max(0.0);
        if content_height > page_area_height && content_height > 0.0 {
            self.draw_scrollbar(scroll_y, content_height, page_area_height, y_offset);
        }
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
            // Map destination y to source y (nearest-neighbour).
            let sy = (((py - dy0) as f32 / dest_h) * img_height as f32) as u32;
            let sy = sy.min(img_height - 1);

            for px in dx0..dx1 {
                if px < 0 || px >= self.width as i32 {
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

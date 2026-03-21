//! # canvas
//!
//! Implements the HTML5 Canvas 2D rendering context for NOVA.
//!
//! Provides a software-rendered RGBA pixel buffer that JavaScript code can
//! draw into using the standard `CanvasRenderingContext2D` API surface.
//!
//! The pixel buffer is stored in the [`JsDomTree`] keyed by element handle,
//! and is emitted as a `DrawImage` render op during painting.

use std::f64::consts::PI;

use tracing::debug;

/// A 2D rendering context that operates on an RGBA pixel buffer.
///
/// Implements the core subset of the HTML5 CanvasRenderingContext2D API.
#[derive(Debug, Clone)]
pub struct CanvasContext2D {
    /// Width of the canvas in pixels.
    pub width: u32,
    /// Height of the canvas in pixels.
    pub height: u32,
    /// RGBA pixel data (length = width * height * 4).
    pub pixels: Vec<u8>,
    /// Current drawing state.
    state: DrawState,
    /// Saved state stack for save()/restore().
    state_stack: Vec<DrawState>,
    /// Current path segments.
    path: Vec<PathSegment>,
}

/// Drawing state that can be saved/restored.
#[derive(Debug, Clone)]
struct DrawState {
    /// Fill color as RGBA (0-255 per channel).
    fill_color: [u8; 4],
    /// Stroke color as RGBA.
    stroke_color: [u8; 4],
    /// Line width in pixels.
    line_width: f64,
    /// Font size in pixels.
    font_size: f64,
    /// Font family name.
    font_family: String,
    /// Text alignment: "left", "center", "right", "start", "end".
    text_align: String,
    /// Text baseline: "top", "middle", "alphabetic", "bottom".
    text_baseline: String,
    /// Global alpha (0.0-1.0).
    global_alpha: f64,
    /// 2D affine transform matrix [a, b, c, d, e, f].
    transform: [f64; 6],
}

impl Default for DrawState {
    fn default() -> Self {
        Self {
            fill_color: [0, 0, 0, 255],    // black
            stroke_color: [0, 0, 0, 255],   // black
            line_width: 1.0,
            font_size: 10.0,
            font_family: "sans-serif".into(),
            text_align: "start".into(),
            text_baseline: "alphabetic".into(),
            global_alpha: 1.0,
            transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], // identity
        }
    }
}

/// A segment of a path being built.
#[derive(Debug, Clone)]
enum PathSegment {
    MoveTo(f64, f64),
    LineTo(f64, f64),
    Arc {
        cx: f64,
        cy: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
    },
    ClosePath,
}

impl CanvasContext2D {
    /// Create a new canvas context with the given dimensions.
    ///
    /// The pixel buffer is initialized to fully transparent black.
    pub fn new(width: u32, height: u32) -> Self {
        let pixel_count = (width as usize) * (height as usize) * 4;
        debug!(width, height, "creating canvas 2D context");
        Self {
            width,
            height,
            pixels: vec![0; pixel_count],
            state: DrawState::default(),
            state_stack: Vec::new(),
            path: Vec::new(),
        }
    }

    // ── State ──────────────────────────────────────────────────────────────

    /// Get the fill style as a CSS color string.
    pub fn fill_style(&self) -> String {
        let [r, g, b, a] = self.state.fill_color;
        if a == 255 {
            format!("#{:02x}{:02x}{:02x}", r, g, b)
        } else {
            format!("rgba({},{},{},{})", r, g, b, a as f64 / 255.0)
        }
    }

    /// Set the fill style from a CSS color string.
    pub fn set_fill_style(&mut self, color: &str) {
        if let Some(rgba) = parse_css_color(color) {
            self.state.fill_color = rgba;
        }
    }

    /// Get the stroke style as a CSS color string.
    pub fn stroke_style(&self) -> String {
        let [r, g, b, a] = self.state.stroke_color;
        if a == 255 {
            format!("#{:02x}{:02x}{:02x}", r, g, b)
        } else {
            format!("rgba({},{},{},{})", r, g, b, a as f64 / 255.0)
        }
    }

    /// Set the stroke style from a CSS color string.
    pub fn set_stroke_style(&mut self, color: &str) {
        if let Some(rgba) = parse_css_color(color) {
            self.state.stroke_color = rgba;
        }
    }

    /// Get line width.
    pub fn line_width(&self) -> f64 {
        self.state.line_width
    }

    /// Set line width.
    pub fn set_line_width(&mut self, w: f64) {
        if w > 0.0 {
            self.state.line_width = w;
        }
    }

    /// Get the font string.
    pub fn font(&self) -> String {
        format!("{}px {}", self.state.font_size, self.state.font_family)
    }

    /// Set the font from a CSS font string (e.g., "16px Arial").
    pub fn set_font(&mut self, font: &str) {
        // Simple parser: look for a number followed by "px", rest is family.
        let font = font.trim();
        if let Some(px_pos) = font.find("px") {
            let size_str = font[..px_pos].trim();
            // The size might be preceded by style/weight keywords, take the last token.
            let size_token = size_str.rsplit_once(' ').map(|(_, s)| s).unwrap_or(size_str);
            if let Ok(size) = size_token.parse::<f64>() {
                self.state.font_size = size;
            }
            let family = font[px_pos + 2..].trim();
            if !family.is_empty() {
                self.state.font_family = family.to_string();
            }
        }
    }

    /// Get text alignment.
    pub fn text_align(&self) -> &str {
        &self.state.text_align
    }

    /// Set text alignment.
    pub fn set_text_align(&mut self, align: &str) {
        match align {
            "left" | "right" | "center" | "start" | "end" => {
                self.state.text_align = align.to_string();
            }
            _ => {}
        }
    }

    /// Get text baseline.
    pub fn text_baseline(&self) -> &str {
        &self.state.text_baseline
    }

    /// Set text baseline.
    pub fn set_text_baseline(&mut self, baseline: &str) {
        match baseline {
            "top" | "hanging" | "middle" | "alphabetic" | "ideographic" | "bottom" => {
                self.state.text_baseline = baseline.to_string();
            }
            _ => {}
        }
    }

    /// Get global alpha.
    pub fn global_alpha(&self) -> f64 {
        self.state.global_alpha
    }

    /// Set global alpha (clamped to 0.0-1.0).
    pub fn set_global_alpha(&mut self, alpha: f64) {
        self.state.global_alpha = alpha.clamp(0.0, 1.0);
    }

    // ── Save / Restore ─────────────────────────────────────────────────────

    /// Save the current drawing state onto the stack.
    pub fn save(&mut self) {
        self.state_stack.push(self.state.clone());
    }

    /// Restore the most recently saved drawing state.
    pub fn restore(&mut self) {
        if let Some(state) = self.state_stack.pop() {
            self.state = state;
        }
    }

    // ── Transform ──────────────────────────────────────────────────────────

    /// Translate the current transform.
    pub fn translate(&mut self, tx: f64, ty: f64) {
        let [a, b, c, d, e, f] = self.state.transform;
        self.state.transform = [
            a, b, c, d,
            a * tx + c * ty + e,
            b * tx + d * ty + f,
        ];
    }

    /// Rotate the current transform by an angle in radians.
    pub fn rotate(&mut self, angle: f64) {
        let cos = angle.cos();
        let sin = angle.sin();
        let [a, b, c, d, e, f] = self.state.transform;
        self.state.transform = [
            a * cos + c * sin,
            b * cos + d * sin,
            a * -sin + c * cos,
            b * -sin + d * cos,
            e, f,
        ];
    }

    /// Scale the current transform.
    pub fn scale(&mut self, sx: f64, sy: f64) {
        let [a, b, c, d, e, f] = self.state.transform;
        self.state.transform = [a * sx, b * sx, c * sy, d * sy, e, f];
    }

    /// Set the transform matrix directly.
    pub fn set_transform(&mut self, a: f64, b: f64, c: f64, d: f64, e: f64, f_val: f64) {
        self.state.transform = [a, b, c, d, e, f_val];
    }

    /// Reset the transform to identity.
    pub fn reset_transform(&mut self) {
        self.state.transform = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    }

    // ── Drawing primitives ─────────────────────────────────────────────────

    /// Fill a rectangle with the current fill style.
    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let color = self.effective_fill_color();
        self.raw_fill_rect(x, y, w, h, color);
    }

    /// Stroke a rectangle outline with the current stroke style.
    pub fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let color = self.effective_stroke_color();
        let lw = self.state.line_width;
        // Top edge
        self.raw_fill_rect(x, y, w, lw, color);
        // Bottom edge
        self.raw_fill_rect(x, y + h - lw, w, lw, color);
        // Left edge
        self.raw_fill_rect(x, y, lw, h, color);
        // Right edge
        self.raw_fill_rect(x + w - lw, y, lw, h, color);
    }

    /// Clear a rectangle to fully transparent.
    pub fn clear_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let (px, py) = self.transform_point(x, y);
        let ix = px.round() as i32;
        let iy = py.round() as i32;
        let iw = w.round() as i32;
        let ih = h.round() as i32;

        for row in iy..iy + ih {
            for col in ix..ix + iw {
                if col >= 0 && col < self.width as i32 && row >= 0 && row < self.height as i32 {
                    let idx = ((row as u32) * self.width + col as u32) as usize * 4;
                    if idx + 3 < self.pixels.len() {
                        self.pixels[idx] = 0;
                        self.pixels[idx + 1] = 0;
                        self.pixels[idx + 2] = 0;
                        self.pixels[idx + 3] = 0;
                    }
                }
            }
        }
    }

    // ── Text ───────────────────────────────────────────────────────────────

    /// Fill text at the given position.
    ///
    /// This is a simplified implementation that renders each character as a
    /// filled rectangle (glyph-level rendering would require a font rasterizer).
    pub fn fill_text(&mut self, text: &str, x: f64, y: f64) {
        let color = self.effective_fill_color();
        self.draw_text_impl(text, x, y, color);
    }

    /// Stroke text outline at the given position.
    pub fn stroke_text(&mut self, text: &str, x: f64, y: f64) {
        let color = self.effective_stroke_color();
        self.draw_text_impl(text, x, y, color);
    }

    /// Measure text width (simplified: character count * approximate char width).
    pub fn measure_text(&self, text: &str) -> f64 {
        text.len() as f64 * self.state.font_size * 0.6
    }

    // ── Path building ──────────────────────────────────────────────────────

    /// Begin a new path, clearing any existing path segments.
    pub fn begin_path(&mut self) {
        self.path.clear();
    }

    /// Move the current point to (x, y).
    pub fn move_to(&mut self, x: f64, y: f64) {
        self.path.push(PathSegment::MoveTo(x, y));
    }

    /// Add a line from the current point to (x, y).
    pub fn line_to(&mut self, x: f64, y: f64) {
        self.path.push(PathSegment::LineTo(x, y));
    }

    /// Close the current subpath.
    pub fn close_path(&mut self) {
        self.path.push(PathSegment::ClosePath);
    }

    /// Add a circular arc to the path.
    pub fn arc(&mut self, cx: f64, cy: f64, radius: f64, start_angle: f64, end_angle: f64) {
        self.path.push(PathSegment::Arc {
            cx,
            cy,
            radius,
            start_angle,
            end_angle,
        });
    }

    /// Add a rectangle to the current path.
    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        self.path.push(PathSegment::MoveTo(x, y));
        self.path.push(PathSegment::LineTo(x + w, y));
        self.path.push(PathSegment::LineTo(x + w, y + h));
        self.path.push(PathSegment::LineTo(x, y + h));
        self.path.push(PathSegment::ClosePath);
    }

    /// Fill the current path with the fill style.
    pub fn fill(&mut self) {
        let color = self.effective_fill_color();
        let segments = self.path.clone();
        self.fill_path_segments(&segments, color);
    }

    /// Stroke the current path with the stroke style.
    pub fn stroke(&mut self) {
        let color = self.effective_stroke_color();
        let lw = self.state.line_width;
        let segments = self.path.clone();
        self.stroke_path_segments(&segments, color, lw);
    }

    // ── Image data ─────────────────────────────────────────────────────────

    /// Create a new blank ImageData (returns RGBA bytes of w*h*4 zeros).
    pub fn create_image_data(w: u32, h: u32) -> Vec<u8> {
        vec![0u8; (w as usize) * (h as usize) * 4]
    }

    /// Get pixel data from a rectangle of the canvas.
    pub fn get_image_data(&self, sx: u32, sy: u32, sw: u32, sh: u32) -> Vec<u8> {
        let mut data = vec![0u8; (sw as usize) * (sh as usize) * 4];
        for row in 0..sh {
            for col in 0..sw {
                let src_x = sx + col;
                let src_y = sy + row;
                if src_x < self.width && src_y < self.height {
                    let src_idx = (src_y * self.width + src_x) as usize * 4;
                    let dst_idx = (row * sw + col) as usize * 4;
                    if src_idx + 3 < self.pixels.len() && dst_idx + 3 < data.len() {
                        data[dst_idx..dst_idx + 4]
                            .copy_from_slice(&self.pixels[src_idx..src_idx + 4]);
                    }
                }
            }
        }
        data
    }

    /// Put pixel data onto the canvas at the given position.
    pub fn put_image_data(&mut self, data: &[u8], dx: u32, dy: u32, sw: u32, sh: u32) {
        for row in 0..sh {
            for col in 0..sw {
                let dst_x = dx + col;
                let dst_y = dy + row;
                if dst_x < self.width && dst_y < self.height {
                    let src_idx = (row * sw + col) as usize * 4;
                    let dst_idx = (dst_y * self.width + dst_x) as usize * 4;
                    if src_idx + 3 < data.len() && dst_idx + 3 < self.pixels.len() {
                        self.pixels[dst_idx..dst_idx + 4]
                            .copy_from_slice(&data[src_idx..src_idx + 4]);
                    }
                }
            }
        }
    }

    /// Draw image pixels onto the canvas (simplified drawImage).
    pub fn draw_image(
        &mut self,
        img_pixels: &[u8],
        img_w: u32,
        img_h: u32,
        dx: f64,
        dy: f64,
    ) {
        let (px, py) = self.transform_point(dx, dy);
        let ix = px.round() as i32;
        let iy = py.round() as i32;
        for row in 0..img_h as i32 {
            for col in 0..img_w as i32 {
                let dst_x = ix + col;
                let dst_y = iy + row;
                if dst_x >= 0
                    && dst_x < self.width as i32
                    && dst_y >= 0
                    && dst_y < self.height as i32
                {
                    let src_idx = (row as u32 * img_w + col as u32) as usize * 4;
                    let dst_idx = (dst_y as u32 * self.width + dst_x as u32) as usize * 4;
                    if src_idx + 3 < img_pixels.len() && dst_idx + 3 < self.pixels.len() {
                        let sa = img_pixels[src_idx + 3] as f64 / 255.0 * self.state.global_alpha;
                        let inv_sa = 1.0 - sa;
                        for c in 0..3 {
                            let src = img_pixels[src_idx + c] as f64;
                            let dst = self.pixels[dst_idx + c] as f64;
                            self.pixels[dst_idx + c] = (src * sa + dst * inv_sa).round() as u8;
                        }
                        let da = self.pixels[dst_idx + 3] as f64 / 255.0;
                        let out_a = sa + da * inv_sa;
                        self.pixels[dst_idx + 3] = (out_a * 255.0).round() as u8;
                    }
                }
            }
        }
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    /// Apply the current transform to a point.
    fn transform_point(&self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.state.transform;
        (a * x + c * y + e, b * x + d * y + f)
    }

    /// Get the effective fill color with global alpha applied.
    fn effective_fill_color(&self) -> [u8; 4] {
        let [r, g, b, a] = self.state.fill_color;
        let ea = (a as f64 * self.state.global_alpha).round() as u8;
        [r, g, b, ea]
    }

    /// Get the effective stroke color with global alpha applied.
    fn effective_stroke_color(&self) -> [u8; 4] {
        let [r, g, b, a] = self.state.stroke_color;
        let ea = (a as f64 * self.state.global_alpha).round() as u8;
        [r, g, b, ea]
    }

    /// Fill a rectangle in pixel space, applying the current transform.
    fn raw_fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, color: [u8; 4]) {
        let (px, py) = self.transform_point(x, y);
        let ix = px.round() as i32;
        let iy = py.round() as i32;
        let iw = w.round() as i32;
        let ih = h.round() as i32;

        let sa = color[3] as f64 / 255.0;
        if sa <= 0.0 {
            return;
        }

        for row in iy..iy + ih {
            for col in ix..ix + iw {
                if col >= 0 && col < self.width as i32 && row >= 0 && row < self.height as i32 {
                    let idx = ((row as u32) * self.width + col as u32) as usize * 4;
                    if idx + 3 < self.pixels.len() {
                        self.blend_pixel(idx, color);
                    }
                }
            }
        }
    }

    /// Alpha-composite a color onto a pixel in the buffer.
    fn blend_pixel(&mut self, idx: usize, color: [u8; 4]) {
        let sa = color[3] as f64 / 255.0;
        if sa >= 1.0 {
            self.pixels[idx] = color[0];
            self.pixels[idx + 1] = color[1];
            self.pixels[idx + 2] = color[2];
            self.pixels[idx + 3] = 255;
        } else if sa > 0.0 {
            let inv_sa = 1.0 - sa;
            for c in 0..3 {
                let src = color[c] as f64;
                let dst = self.pixels[idx + c] as f64;
                self.pixels[idx + c] = (src * sa + dst * inv_sa).round() as u8;
            }
            let da = self.pixels[idx + 3] as f64 / 255.0;
            let out_a = sa + da * inv_sa;
            self.pixels[idx + 3] = (out_a * 255.0).round() as u8;
        }
    }

    /// Draw text (simplified block rendering per character).
    fn draw_text_impl(&mut self, text: &str, x: f64, y: f64, color: [u8; 4]) {
        let char_w = self.state.font_size * 0.6;
        let char_h = self.state.font_size;

        // Adjust x for text alignment.
        let total_width = text.len() as f64 * char_w;
        let start_x = match self.state.text_align.as_str() {
            "center" => x - total_width / 2.0,
            "right" | "end" => x - total_width,
            _ => x,
        };

        // Adjust y for text baseline.
        let start_y = match self.state.text_baseline.as_str() {
            "top" | "hanging" => y,
            "middle" => y - char_h / 2.0,
            "bottom" | "ideographic" => y - char_h,
            _ => y - char_h * 0.8, // "alphabetic" default
        };

        for (i, _ch) in text.chars().enumerate() {
            let cx = start_x + i as f64 * char_w;
            // Draw a simple rectangle per character (simplified glyph rendering).
            let glyph_w = char_w * 0.7;
            let glyph_h = char_h * 0.8;
            let glyph_x = cx + char_w * 0.15;
            let glyph_y = start_y + char_h * 0.1;
            self.raw_fill_rect(glyph_x, glyph_y, glyph_w, glyph_h, color);
        }
    }

    /// Fill path segments using a scan-line approach.
    fn fill_path_segments(&mut self, segments: &[PathSegment], color: [u8; 4]) {
        // Flatten the path to a list of points.
        let points = self.flatten_path(segments);
        if points.is_empty() {
            return;
        }

        // Find bounding box.
        let min_y = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_y = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);

        // Scanline fill (even-odd rule).
        let iy_min = min_y.floor() as i32;
        let iy_max = max_y.ceil() as i32;

        for scan_y in iy_min..=iy_max {
            let y = scan_y as f64 + 0.5;
            let mut intersections = Vec::new();

            for i in 0..points.len() {
                let j = (i + 1) % points.len();
                let (x0, y0) = points[i];
                let (x1, y1) = points[j];

                if (y0 <= y && y1 > y) || (y1 <= y && y0 > y) {
                    let t = (y - y0) / (y1 - y0);
                    let x = x0 + t * (x1 - x0);
                    intersections.push(x);
                }
            }

            intersections.sort_by(|a, b| a.partial_cmp(b).unwrap());

            for pair in intersections.chunks(2) {
                if pair.len() == 2 {
                    let x_start = pair[0].round() as i32;
                    let x_end = pair[1].round() as i32;
                    for col in x_start..=x_end {
                        if col >= 0
                            && col < self.width as i32
                            && scan_y >= 0
                            && scan_y < self.height as i32
                        {
                            let idx = (scan_y as u32 * self.width + col as u32) as usize * 4;
                            if idx + 3 < self.pixels.len() {
                                self.blend_pixel(idx, color);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Stroke path segments by drawing thick lines.
    fn stroke_path_segments(
        &mut self,
        segments: &[PathSegment],
        color: [u8; 4],
        line_width: f64,
    ) {
        let points = self.flatten_path(segments);
        if points.len() < 2 {
            return;
        }

        let half_w = line_width / 2.0;
        for i in 0..points.len() - 1 {
            let (x0, y0) = points[i];
            let (x1, y1) = points[i + 1];
            self.draw_thick_line(x0, y0, x1, y1, half_w, color);
        }
    }

    /// Draw a thick line between two points using rectangle fill.
    fn draw_thick_line(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        half_w: f64,
        color: [u8; 4],
    ) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt();
        if len < 0.001 {
            return;
        }

        // Perpendicular normal.
        let nx = -dy / len * half_w;
        let ny = dx / len * half_w;

        // Four corners of the thick line rectangle.
        let corners = [
            (x0 + nx, y0 + ny),
            (x0 - nx, y0 - ny),
            (x1 - nx, y1 - ny),
            (x1 + nx, y1 + ny),
        ];

        // Fill the quadrilateral.
        let min_y = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_y = corners.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);

        let iy_min = min_y.floor() as i32;
        let iy_max = max_y.ceil() as i32;

        for scan_y in iy_min..=iy_max {
            let y = scan_y as f64 + 0.5;
            let mut intersections = Vec::new();

            for i in 0..4 {
                let j = (i + 1) % 4;
                let (x0, y0) = corners[i];
                let (x1, y1) = corners[j];
                if (y0 <= y && y1 > y) || (y1 <= y && y0 > y) {
                    let t = (y - y0) / (y1 - y0);
                    let x = x0 + t * (x1 - x0);
                    intersections.push(x);
                }
            }

            intersections.sort_by(|a, b| a.partial_cmp(b).unwrap());

            for pair in intersections.chunks(2) {
                if pair.len() == 2 {
                    let x_start = pair[0].round() as i32;
                    let x_end = pair[1].round() as i32;
                    for col in x_start..=x_end {
                        if col >= 0
                            && col < self.width as i32
                            && scan_y >= 0
                            && scan_y < self.height as i32
                        {
                            let idx =
                                (scan_y as u32 * self.width + col as u32) as usize * 4;
                            if idx + 3 < self.pixels.len() {
                                self.blend_pixel(idx, color);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Flatten path segments into a list of (x, y) points, applying the
    /// current transform. Arcs are approximated with line segments.
    fn flatten_path(&self, segments: &[PathSegment]) -> Vec<(f64, f64)> {
        let mut points = Vec::new();
        let mut current = (0.0, 0.0);
        let mut first_point: Option<(f64, f64)> = None;

        for seg in segments {
            match seg {
                PathSegment::MoveTo(x, y) => {
                    let p = self.transform_point(*x, *y);
                    current = p;
                    first_point = Some(p);
                    points.push(p);
                }
                PathSegment::LineTo(x, y) => {
                    let p = self.transform_point(*x, *y);
                    current = p;
                    points.push(p);
                }
                PathSegment::Arc {
                    cx,
                    cy,
                    radius,
                    start_angle,
                    end_angle,
                } => {
                    // Approximate arc with line segments.
                    let steps = ((end_angle - start_angle).abs() / PI * 32.0)
                        .ceil()
                        .max(4.0) as usize;
                    for i in 0..=steps {
                        let t = i as f64 / steps as f64;
                        let angle = start_angle + t * (end_angle - start_angle);
                        let x = cx + radius * angle.cos();
                        let y = cy + radius * angle.sin();
                        let p = self.transform_point(x, y);
                        current = p;
                        if i == 0 && first_point.is_none() {
                            first_point = Some(p);
                        }
                        points.push(p);
                    }
                }
                PathSegment::ClosePath => {
                    if let Some(fp) = first_point {
                        points.push(fp);
                        current = fp;
                    }
                }
            }
        }

        points
    }
}

// ── CSS color parsing ─────────────────────────────────────────────────────────

/// Parse a CSS color string to RGBA [r, g, b, a] where each component is 0-255.
///
/// Supports:
/// - Hex: `#rgb`, `#rrggbb`, `#rrggbbaa`
/// - `rgb(r, g, b)`, `rgba(r, g, b, a)`
/// - Named colors (basic set)
pub fn parse_css_color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim();

    if s.starts_with('#') {
        return parse_hex(s);
    }

    if s.starts_with("rgba(") {
        let inner = s.trim_start_matches("rgba(").trim_end_matches(')');
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() >= 4 {
            let r = parts[0].trim().parse::<u8>().ok()?;
            let g = parts[1].trim().parse::<u8>().ok()?;
            let b = parts[2].trim().parse::<u8>().ok()?;
            let a = parts[3].trim().parse::<f64>().ok().unwrap_or(1.0);
            return Some([r, g, b, (a * 255.0).round() as u8]);
        }
    }

    if s.starts_with("rgb(") {
        let inner = s.trim_start_matches("rgb(").trim_end_matches(')');
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() >= 3 {
            let r = parts[0].trim().parse::<u8>().ok()?;
            let g = parts[1].trim().parse::<u8>().ok()?;
            let b = parts[2].trim().parse::<u8>().ok()?;
            return Some([r, g, b, 255]);
        }
    }

    // Named colors.
    match s.to_lowercase().as_str() {
        "black" => Some([0, 0, 0, 255]),
        "white" => Some([255, 255, 255, 255]),
        "red" => Some([255, 0, 0, 255]),
        "green" => Some([0, 128, 0, 255]),
        "lime" => Some([0, 255, 0, 255]),
        "blue" => Some([0, 0, 255, 255]),
        "yellow" => Some([255, 255, 0, 255]),
        "cyan" | "aqua" => Some([0, 255, 255, 255]),
        "magenta" | "fuchsia" => Some([255, 0, 255, 255]),
        "orange" => Some([255, 165, 0, 255]),
        "purple" => Some([128, 0, 128, 255]),
        "gray" | "grey" => Some([128, 128, 128, 255]),
        "silver" => Some([192, 192, 192, 255]),
        "maroon" => Some([128, 0, 0, 255]),
        "olive" => Some([128, 128, 0, 255]),
        "navy" => Some([0, 0, 128, 255]),
        "teal" => Some([0, 128, 128, 255]),
        "transparent" => Some([0, 0, 0, 0]),
        _ => None,
    }
}

/// Parse a hex color string to RGBA.
fn parse_hex(s: &str) -> Option<[u8; 4]> {
    let hex = s.trim_start_matches('#');
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
            Some([r, g, b, 255])
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some([r, g, b, 255])
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some([r, g, b, a])
        }
        _ => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_canvas_is_transparent() {
        let ctx = CanvasContext2D::new(10, 10);
        assert_eq!(ctx.pixels.len(), 400);
        assert!(ctx.pixels.iter().all(|&b| b == 0));
    }

    #[test]
    fn fill_rect_draws_pixels() {
        let mut ctx = CanvasContext2D::new(10, 10);
        ctx.set_fill_style("red");
        ctx.fill_rect(0.0, 0.0, 5.0, 5.0);

        // Top-left pixel should be red.
        assert_eq!(ctx.pixels[0], 255); // R
        assert_eq!(ctx.pixels[1], 0);   // G
        assert_eq!(ctx.pixels[2], 0);   // B
        assert_eq!(ctx.pixels[3], 255); // A

        // Pixel at (6, 6) should still be transparent.
        let idx = (6 * 10 + 6) * 4;
        assert_eq!(ctx.pixels[idx + 3], 0);
    }

    #[test]
    fn clear_rect_clears_pixels() {
        let mut ctx = CanvasContext2D::new(10, 10);
        ctx.set_fill_style("blue");
        ctx.fill_rect(0.0, 0.0, 10.0, 10.0);
        ctx.clear_rect(2.0, 2.0, 3.0, 3.0);

        // Cleared pixel should be transparent.
        let idx = (2 * 10 + 2) * 4;
        assert_eq!(ctx.pixels[idx + 3], 0);

        // Non-cleared pixel should still be blue.
        assert_eq!(ctx.pixels[0], 0);   // R
        assert_eq!(ctx.pixels[1], 0);   // G
        assert_eq!(ctx.pixels[2], 255); // B
        assert_eq!(ctx.pixels[3], 255); // A
    }

    #[test]
    fn save_restore_state() {
        let mut ctx = CanvasContext2D::new(10, 10);
        ctx.set_fill_style("red");
        ctx.save();
        ctx.set_fill_style("blue");
        assert_eq!(ctx.state.fill_color, [0, 0, 255, 255]);
        ctx.restore();
        assert_eq!(ctx.state.fill_color, [255, 0, 0, 255]);
    }

    #[test]
    fn stroke_rect_draws_outline() {
        let mut ctx = CanvasContext2D::new(20, 20);
        ctx.set_stroke_style("green");
        ctx.set_line_width(1.0);
        ctx.stroke_rect(2.0, 2.0, 10.0, 10.0);

        // Top edge pixel at (5, 2) should be green.
        let idx = (2 * 20 + 5) * 4;
        assert_eq!(ctx.pixels[idx], 0);
        assert_eq!(ctx.pixels[idx + 1], 128);
        assert_eq!(ctx.pixels[idx + 2], 0);
    }

    #[test]
    fn parse_css_colors() {
        assert_eq!(parse_css_color("#ff0000"), Some([255, 0, 0, 255]));
        assert_eq!(parse_css_color("#f00"), Some([255, 0, 0, 255]));
        assert_eq!(parse_css_color("rgb(0, 128, 255)"), Some([0, 128, 255, 255]));
        assert_eq!(
            parse_css_color("rgba(255, 0, 0, 0.5)"),
            Some([255, 0, 0, 128])
        );
        assert_eq!(parse_css_color("red"), Some([255, 0, 0, 255]));
        assert_eq!(parse_css_color("transparent"), Some([0, 0, 0, 0]));
    }

    #[test]
    fn measure_text_returns_width() {
        let ctx = CanvasContext2D::new(100, 100);
        let w = ctx.measure_text("hello");
        assert!(w > 0.0);
    }

    #[test]
    fn global_alpha_affects_fill() {
        let mut ctx = CanvasContext2D::new(10, 10);
        ctx.set_fill_style("red");
        ctx.set_global_alpha(0.5);
        ctx.fill_rect(0.0, 0.0, 5.0, 5.0);

        // Alpha should be roughly 128 (0.5 * 255).
        assert!((ctx.pixels[3] as i32 - 128).abs() <= 1);
    }

    #[test]
    fn translate_transform() {
        let mut ctx = CanvasContext2D::new(20, 20);
        ctx.set_fill_style("red");
        ctx.translate(5.0, 5.0);
        ctx.fill_rect(0.0, 0.0, 3.0, 3.0);

        // Pixel at (5, 5) should be red.
        let idx = (5 * 20 + 5) * 4;
        assert_eq!(ctx.pixels[idx], 255);
        assert_eq!(ctx.pixels[idx + 3], 255);

        // Pixel at (0, 0) should still be transparent.
        assert_eq!(ctx.pixels[3], 0);
    }

    #[test]
    fn get_put_image_data_roundtrip() {
        let mut ctx = CanvasContext2D::new(10, 10);
        ctx.set_fill_style("blue");
        ctx.fill_rect(2.0, 2.0, 4.0, 4.0);

        let data = ctx.get_image_data(2, 2, 4, 4);
        assert_eq!(data.len(), 64); // 4*4*4

        // First pixel should be blue.
        assert_eq!(data[0], 0);
        assert_eq!(data[1], 0);
        assert_eq!(data[2], 255);

        // Put it back at a different position.
        ctx.put_image_data(&data, 6, 6, 4, 4);

        let idx = (6 * 10 + 6) * 4;
        assert_eq!(ctx.pixels[idx + 2], 255); // blue
    }

    #[test]
    fn arc_path_fill() {
        let mut ctx = CanvasContext2D::new(50, 50);
        ctx.set_fill_style("red");
        ctx.begin_path();
        ctx.arc(25.0, 25.0, 10.0, 0.0, 2.0 * PI);
        ctx.fill();

        // Center pixel should be red.
        let idx = (25 * 50 + 25) * 4;
        assert_eq!(ctx.pixels[idx], 255);
        assert_eq!(ctx.pixels[idx + 3], 255);
    }

    #[test]
    fn set_font_parsing() {
        let mut ctx = CanvasContext2D::new(10, 10);
        ctx.set_font("bold 20px Arial");
        assert!((ctx.state.font_size - 20.0).abs() < 0.01);
        assert_eq!(ctx.state.font_family, "Arial");
    }
}

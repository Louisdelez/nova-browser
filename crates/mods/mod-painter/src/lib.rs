//! # mod-painter
//!
//! NOVA Mod for painting — converts a `LayoutTree` into `RenderCommands`.
//! Handles the `Paint` capability.
//!
//! Walks the layout tree and generates `RenderOp`s: `FillRect` for element
//! backgrounds and `DrawText` for text content.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use semver::Version;
use tracing::{debug, info};

use nova_mod_api::{
    capability::CapabilityType,
    content::{ContentRequest, LayoutBox, LayoutContent, StyleValue, TypedData},
    error::NovaError,
    manifest::ModManifest,
    permission::{Permission, TrustLevel},
    types::{Color, ModId, RenderCommands, RenderOp},
    CoreApi, NovaMod,
};

/// The painter mod.
pub struct PainterMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
}

impl PainterMod {
    /// Create a new `PainterMod` instance.
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.painter"),
            name: "NOVA Painter".into(),
            version: Version::new(0, 1, 0),
            description: "Generates render commands from layout tree".into(),
            capabilities: vec![CapabilityType::Paint],
            permissions: vec![Permission::GpuRender],
            dependencies: vec![CapabilityType::Layout],
            triggers: vec![],
            min_core_version: Version::new(0, 1, 0),
            trust_level: TrustLevel::Core,
        };

        Self {
            manifest,
            core: None,
        }
    }
}

impl Default for PainterMod {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NovaMod for PainterMod {
    fn manifest(&self) -> &ModManifest {
        &self.manifest
    }

    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "painter mod initializing");
        self.core = Some(core);
        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            ContentRequest::Paint { layout_tree, images } => {
                let root = match *layout_tree {
                    TypedData::LayoutTree(tree) => tree,
                    _ => {
                        return Err(NovaError::UnsupportedContent(
                            "Paint expects TypedData::LayoutTree".into(),
                        ));
                    }
                };

                debug!("painting layout tree into render commands");

                let images_map: HashMap<String, Vec<u8>> = images.into_iter().collect();

                let mut ops = Vec::new();
                paint_box(&root, &mut ops, &images_map);

                // Post-process: coalesce adjacent DrawText ops on the same line
                // into single ops. This ensures consistent word spacing because
                // the renderer draws the combined string in one pass instead of
                // positioning each word independently.
                let ops = coalesce_text_ops(ops);

                debug!(op_count = ops.len(), "painting complete");
                Ok(TypedData::RenderCommands(RenderCommands { ops, fonts: vec![] }))
            }
            other => Err(NovaError::UnsupportedContent(format!(
                "painter cannot handle request: {other:?}"
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "painter mod shutting down");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Text op coalescing
// ---------------------------------------------------------------------------

/// Merge consecutive `DrawText` ops that share the same Y coordinate, font size,
/// color, weight, style, and family into a single `DrawText` op.
///
/// Individual words are laid out by the layout engine as separate boxes, each
/// producing its own `DrawText` op. When the renderer draws them independently,
/// slight differences between layout measurement and rendering measurement
/// cause words to visually overlap or have inconsistent spacing.
///
/// By joining adjacent words into one string (with a space between them when
/// there's a gap), the renderer draws the whole line in one pass and its own
/// advance-width calculations keep the spacing consistent.
fn coalesce_text_ops(ops: Vec<RenderOp>) -> Vec<RenderOp> {
    let mut result: Vec<RenderOp> = Vec::with_capacity(ops.len());

    for op in ops {
        if let RenderOp::DrawText {
            x, y, ref text, font_size, color, ref font_weight, ref font_style, ref font_family, ref letter_spacing,
        } = op
        {
            // Try to merge with the previous DrawText if on the same line.
            let merged = match result.last() {
                Some(RenderOp::DrawText {
                    x: prev_x,
                    y: prev_y,
                    text: prev_text,
                    font_size: prev_fs,
                    color: prev_color,
                    font_weight: prev_fw,
                    font_style: prev_fst,
                    font_family: prev_ff,
                    letter_spacing: prev_ls,
                }) => {
                    // Same baseline Y (within 0.5px), same font properties, and same color.
                    (y - prev_y).abs() < 0.5
                        && (font_size - prev_fs).abs() < 0.1
                        && color_eq(&color, prev_color)
                        && font_weight == prev_fw
                        && font_style == prev_fst
                        && font_family == prev_ff
                        && letter_spacing == prev_ls
                        && x > *prev_x
                        && (x - *prev_x) < prev_text.len() as f32 * font_size + font_size * 3.0
                }
                _ => false,
            };

            if merged {
                // Pop the previous and merge.
                if let Some(RenderOp::DrawText {
                    x: prev_x,
                    y: prev_y,
                    text: prev_text,
                    font_size: prev_fs,
                    color: prev_color,
                    font_weight: prev_fw,
                    font_style: prev_fst,
                    font_family: prev_ff,
                    letter_spacing: prev_ls,
                }) = result.pop()
                {
                    let combined = if text.starts_with(' ') || prev_text.ends_with(' ') {
                        format!("{}{}", prev_text, text)
                    } else {
                        format!("{} {}", prev_text, text)
                    };
                    result.push(RenderOp::DrawText {
                        x: prev_x,
                        y: prev_y,
                        text: combined,
                        font_size: prev_fs,
                        color: prev_color,
                        font_weight: prev_fw,
                        font_style: prev_fst,
                        font_family: prev_ff,
                        letter_spacing: prev_ls,
                    });
                }
            } else {
                result.push(op);
            }
        } else {
            result.push(op);
        }
    }

    result
}

/// Compare two colors for approximate equality.
#[inline]
fn color_eq(a: &Color, b: &Color) -> bool {
    (a.r - b.r).abs() < 0.01
        && (a.g - b.g).abs() < 0.01
        && (a.b - b.b).abs() < 0.01
        && (a.a - b.a).abs() < 0.01
}

// ---------------------------------------------------------------------------
// CSS transform parsing
// ---------------------------------------------------------------------------

/// Parsed CSS transform functions.
#[derive(Debug, Clone, PartialEq)]
struct CssTransform {
    translate_x: f32,
    translate_y: f32,
    scale_x: f32,
    scale_y: f32,
    rotate_deg: f32,
}

impl Default for CssTransform {
    fn default() -> Self {
        Self { translate_x: 0.0, translate_y: 0.0, scale_x: 1.0, scale_y: 1.0, rotate_deg: 0.0 }
    }
}

impl CssTransform {
    fn has_translate(&self) -> bool {
        self.translate_x != 0.0 || self.translate_y != 0.0
    }
}

/// Parse a CSS `transform` property value.
fn parse_transform(value: &str) -> Option<CssTransform> {
    let value = value.trim();
    if value.is_empty() || value == "none" {
        return None;
    }
    let mut transform = CssTransform::default();
    let mut found_any = false;
    let mut remaining = value;
    while let Some(paren_open) = remaining.find('(') {
        let func_name = remaining[..paren_open].trim();
        let func_name = func_name.rsplit_once(|c: char| c.is_whitespace()).map(|(_, n)| n).unwrap_or(func_name);
        let after_open = &remaining[paren_open + 1..];
        let Some(paren_close) = after_open.find(')') else { break };
        let args_str = &after_open[..paren_close];
        remaining = &after_open[paren_close + 1..];
        let args: Vec<&str> = args_str.split(',').map(|s| s.trim()).collect();
        match func_name {
            "translate" => {
                if let Some(x) = args.first().and_then(|a| parse_length_px(a)) {
                    transform.translate_x += x; found_any = true;
                }
                if let Some(y) = args.get(1).and_then(|a| parse_length_px(a)) {
                    transform.translate_y += y;
                }
            }
            "translateX" => {
                if let Some(x) = args.first().and_then(|a| parse_length_px(a)) {
                    transform.translate_x += x; found_any = true;
                }
            }
            "translateY" => {
                if let Some(y) = args.first().and_then(|a| parse_length_px(a)) {
                    transform.translate_y += y; found_any = true;
                }
            }
            "scale" => {
                if let Some(sx) = args.first().and_then(|a| a.parse::<f32>().ok()) {
                    let sy = args.get(1).and_then(|a| a.parse::<f32>().ok()).unwrap_or(sx);
                    transform.scale_x *= sx; transform.scale_y *= sy; found_any = true;
                }
            }
            "rotate" => {
                if let Some(deg) = args.first().and_then(|a| parse_angle_deg(a)) {
                    transform.rotate_deg += deg; found_any = true;
                }
            }
            _ => {}
        }
    }
    if found_any { Some(transform) } else { None }
}

/// Parse a CSS angle value, returning degrees.
fn parse_angle_deg(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("deg") { v.trim().parse::<f32>().ok() }
    else if let Some(v) = s.strip_suffix("rad") { v.trim().parse::<f32>().ok().map(|r| r.to_degrees()) }
    else if let Some(v) = s.strip_suffix("turn") { v.trim().parse::<f32>().ok().map(|t| t * 360.0) }
    else { s.parse::<f32>().ok() }
}

/// Extract the CSS `transform` property from a style map.
fn extract_transform(style: &nova_mod_api::content::StyleMap) -> Option<CssTransform> {
    for (key, value) in &style.properties {
        if key == "transform" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                _ => continue,
            };
            return parse_transform(s);
        }
    }
    None
}

/// Check if this element creates a new stacking context.
///
/// A stacking context is created by elements with:
/// - `z-index` set AND `position` is not `static`
/// - `opacity` < 1.0
/// - CSS `transform` is set (non-none)
/// - `position: fixed` or `position: sticky`
fn creates_stacking_context(style: &nova_mod_api::content::StyleMap) -> bool {
    let mut has_z_index = false;
    let mut has_position = false;
    let mut has_opacity_lt_1 = false;
    let mut has_transform = false;
    let mut is_fixed_or_sticky = false;

    for (key, value) in &style.properties {
        match key.as_str() {
            "z-index" => {
                match value {
                    StyleValue::Number(n) if *n != 0.0 => has_z_index = true,
                    StyleValue::Keyword(s) | StyleValue::Str(s) => {
                        if s.trim() != "auto" {
                            has_z_index = true;
                        }
                    }
                    _ => {}
                }
            }
            "position" => {
                if let StyleValue::Keyword(k) | StyleValue::Str(k) = value {
                    let pos = k.trim();
                    if pos != "static" { has_position = true; }
                    if pos == "fixed" || pos == "sticky" { is_fixed_or_sticky = true; }
                }
            }
            "opacity" => {
                match value {
                    StyleValue::Number(n) if *n < 1.0 => has_opacity_lt_1 = true,
                    StyleValue::Str(s) | StyleValue::Keyword(s) => {
                        if let Ok(v) = s.parse::<f32>() {
                            if v < 1.0 { has_opacity_lt_1 = true; }
                        }
                    }
                    _ => {}
                }
            }
            "transform" => {
                if let StyleValue::Str(s) | StyleValue::Keyword(s) = value {
                    if s.trim() != "none" && !s.trim().is_empty() {
                        has_transform = true;
                    }
                }
            }
            _ => {}
        }
    }

    (has_z_index && has_position) || has_opacity_lt_1 || has_transform || is_fixed_or_sticky
}

/// Recursively paint a layout box into render operations.
fn paint_box(layout_box: &LayoutBox, ops: &mut Vec<RenderOp>, images: &HashMap<String, Vec<u8>>) {
    // Skip zero-sized boxes (display: none, comments, etc.).
    if layout_box.width <= 0.0 && layout_box.height <= 0.0 {
        return;
    }

    // Check for position: sticky or position: fixed and emit StickyStart/StickyEnd.
    // Fixed elements reuse the sticky mechanism — they always pin to the viewport.
    let is_sticky = is_position_sticky(&layout_box.style);
    let is_fixed = is_position_fixed(&layout_box.style);
    if is_sticky {
        let sticky_top = extract_sticky_top(&layout_box.style);
        ops.push(RenderOp::StickyStart {
            original_y: layout_box.y,
            sticky_top,
        });
    } else if is_fixed {
        let fixed_top = extract_fixed_top(&layout_box.style);
        ops.push(RenderOp::StickyStart {
            original_y: layout_box.y,
            sticky_top: fixed_top,
        });
    }

    // Check for CSS transform and wrap in Save/Translate/Restore if needed.
    let transform = extract_transform(&layout_box.style);
    let needs_translate = transform.as_ref().is_some_and(|t| t.has_translate());
    if needs_translate {
        let t = transform.as_ref().unwrap();
        ops.push(RenderOp::Save);
        ops.push(RenderOp::Translate { x: t.translate_x, y: t.translate_y });
    }

    // Determine opacity factor for this box (1.0 = fully opaque).
    let (filter_opacity, css_filter) = extract_css_filter(&layout_box.style);
    let opacity = extract_opacity(&layout_box.style) * filter_opacity;

    // Emit box-shadow before the background (painter's order: shadow → bg → content).
    if let Some((shadow_color, offset_x, offset_y, blur)) = extract_box_shadow(&layout_box.style) {
        let shadow_color = multiply_alpha(shadow_color, opacity);
        if blur > 0.0 {
            // Simulate blur with concentric expanding rects of decreasing alpha.
            let steps = (blur / 2.0).ceil().max(1.0) as i32;
            for i in 0..=steps {
                let expand = (i as f32 / steps as f32) * blur;
                let alpha_factor = 1.0 - (i as f32 / steps as f32);
                let c = Color::rgba(
                    shadow_color.r, shadow_color.g, shadow_color.b,
                    shadow_color.a * alpha_factor * 0.3,
                );
                ops.push(RenderOp::FillRect {
                    x: layout_box.x + offset_x - expand,
                    y: layout_box.y + offset_y - expand,
                    width: layout_box.width + expand * 2.0,
                    height: layout_box.height + expand * 2.0,
                    color: c,
                });
            }
        } else {
            ops.push(RenderOp::BoxShadow {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width,
                height: layout_box.height,
                color: shadow_color,
                offset_x,
                offset_y,
                blur,
            });
        }
    }

    // Paint the background if this is not a text node.
    let bg_color = if let Some(ref f) = css_filter {
        apply_filter_to_color(multiply_alpha(extract_background_color(&layout_box.style), opacity), f)
    } else {
        multiply_alpha(extract_background_color(&layout_box.style), opacity)
    };
    if bg_color.a > 0.0 {
        let radius = extract_border_radius(&layout_box.style, layout_box.width, layout_box.height);
        if radius.iter().any(|&r| r > 0.0) {
            ops.push(RenderOp::FillRoundedRect {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width,
                height: layout_box.height,
                color: bg_color,
                radius,
            });
        } else {
            ops.push(RenderOp::FillRect {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width,
                height: layout_box.height,
                color: bg_color,
            });
        }
    }

    // Paint background-image (url() or linear-gradient).
    if let Some(bg_image) = extract_background_image_value(&layout_box.style) {
        if bg_image.starts_with("url(") {
            if let Some(url) = extract_css_url(&bg_image) {
                if let Some(decoded) = images.get(&url) {
                    if decoded.len() >= 8 {
                        let iw = u32::from_le_bytes([decoded[0], decoded[1], decoded[2], decoded[3]]);
                        let ih = u32::from_le_bytes([decoded[4], decoded[5], decoded[6], decoded[7]]);
                        let px = decoded[8..].to_vec();
                        let img_w = iw as f32;
                        let img_h = ih as f32;

                        let bg_repeat = extract_background_repeat(&layout_box.style);
                        let (pos_x, pos_y) = extract_background_position(&layout_box.style);
                        let (size_w, size_h) = extract_background_size(
                            &layout_box.style, layout_box.width, layout_box.height, img_w, img_h,
                        );

                        // Calculate the positioned origin of the background image.
                        let origin_x = layout_box.x + (layout_box.width - size_w) * pos_x;
                        let origin_y = layout_box.y + (layout_box.height - size_h) * pos_y;

                        match bg_repeat {
                            "no-repeat" => {
                                ops.push(RenderOp::DrawImage {
                                    x: origin_x, y: origin_y,
                                    width: size_w, height: size_h,
                                    img_width: iw, img_height: ih, pixels: px,
                                });
                            }
                            "repeat-x" => {
                                if size_w > 0.0 {
                                    let mut cx = layout_box.x;
                                    while cx < layout_box.x + layout_box.width {
                                        ops.push(RenderOp::DrawImage {
                                            x: cx, y: origin_y,
                                            width: size_w, height: size_h,
                                            img_width: iw, img_height: ih, pixels: px.clone(),
                                        });
                                        cx += size_w;
                                    }
                                }
                            }
                            "repeat-y" => {
                                if size_h > 0.0 {
                                    let mut cy = layout_box.y;
                                    while cy < layout_box.y + layout_box.height {
                                        ops.push(RenderOp::DrawImage {
                                            x: origin_x, y: cy,
                                            width: size_w, height: size_h,
                                            img_width: iw, img_height: ih, pixels: px.clone(),
                                        });
                                        cy += size_h;
                                    }
                                }
                            }
                            _ => {
                                // "repeat" — tile in both directions
                                if size_w > 0.0 && size_h > 0.0 {
                                    let mut cy = layout_box.y;
                                    while cy < layout_box.y + layout_box.height {
                                        let mut cx = layout_box.x;
                                        while cx < layout_box.x + layout_box.width {
                                            ops.push(RenderOp::DrawImage {
                                                x: cx, y: cy,
                                                width: size_w, height: size_h,
                                                img_width: iw, img_height: ih, pixels: px.clone(),
                                            });
                                            cx += size_w;
                                        }
                                        cy += size_h;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        } else if bg_image.contains("linear-gradient(") {
            // Render linear gradient as horizontal strips.
            paint_linear_gradient(
                &bg_image, layout_box.x, layout_box.y,
                layout_box.width, layout_box.height, opacity, ops,
            );
        }
    }

    // Paint text content (including space-only nodes for inter-element spacing).
    if let LayoutContent::Text(ref text) = layout_box.content {
        if !text.is_empty() {
            let font_size = extract_font_size(&layout_box.style);
            let text_color = {
                let c = multiply_alpha(extract_text_color(&layout_box.style), opacity);
                if let Some(ref f) = css_filter { apply_filter_to_color(c, f) } else { c }
            };
            let font_weight = extract_font_weight(&layout_box.style);
            let font_style = extract_font_style(&layout_box.style);
            let font_family = extract_font_family(&layout_box.style);
            let letter_spacing = extract_letter_spacing(&layout_box.style);
            let mut transformed_text = apply_text_transform(text, &layout_box.style);
            // Apply text-overflow: ellipsis if needed.
            if has_text_overflow_ellipsis(&layout_box.style) && layout_box.width > 0.0 {
                transformed_text = truncate_with_ellipsis(&transformed_text, font_size, layout_box.width);
            }

            // Check for multi-style segment data from the layout engine.
            // Only use segments if text-transform didn't change the byte
            // length (uppercase/lowercase can shift offsets for non-ASCII).
            let has_transform = transformed_text.len() != text.len();
            let segments_str = if has_transform {
                None
            } else {
                layout_box.style.properties.iter()
                    .find(|(k, _)| k == "nova-text-segments")
                    .and_then(|(_, v)| if let StyleValue::Str(s) = v { Some(s.clone()) } else { None })
            };

            if let Some(seg_str) = segments_str {
                // Multi-style merged node: emit one DrawText per segment
                // with the correct color, weight, and x-offset.
                for seg in seg_str.split(';') {
                    let parts: Vec<&str> = seg.split(':').collect();
                    if parts.len() >= 6 {
                        let start: usize = parts[0].parse().unwrap_or(0);
                        let end: usize = parts[1].parse().unwrap_or(0);
                        let x_off: f32 = parts[2].parse().unwrap_or(0.0);
                        let seg_color_str = parts[3];
                        let seg_weight_str = parts[4];
                        let seg_texdec = parts[5];
                        // 7th field: href (may contain colons, so rejoin everything after field 6).
                        let seg_href = if parts.len() > 6 {
                            parts[6..].join(":")
                        } else {
                            String::new()
                        };
                        let has_href = !seg_href.is_empty();

                        if start < end && end <= transformed_text.len() {
                            let seg_text = &transformed_text[start..end];
                            let seg_color = if !seg_color_str.is_empty() {
                                let c = parse_color_string(seg_color_str).unwrap_or(text_color);
                                let c = multiply_alpha(c, opacity);
                                if let Some(ref f) = css_filter { apply_filter_to_color(c, f) } else { c }
                            } else {
                                text_color
                            };
                            let seg_weight = if !seg_weight_str.is_empty() {
                                match seg_weight_str {
                                    "bold" => Some(700u16),
                                    "bolder" => Some(700),
                                    "lighter" => Some(300),
                                    "normal" => Some(400),
                                    _ => seg_weight_str.parse::<u16>().ok().or(font_weight),
                                }
                            } else {
                                font_weight
                            };

                            ops.push(RenderOp::DrawText {
                                x: layout_box.x + x_off,
                                y: layout_box.y + font_size,
                                text: seg_text.to_string(),
                                font_size,
                                color: seg_color,
                                font_weight: seg_weight,
                                font_style: font_style.clone(),
                                font_family: font_family.clone(),
                                letter_spacing,
                            });

                            // Per-segment underline: draw if text-decoration
                            // says underline OR the segment is a link (links
                            // always get an underline so they remain
                            // distinguishable even when the site overrides
                            // color/decoration).
                            let seg_width = if end < transformed_text.len() {
                                estimate_text_width(seg_text, font_size)
                            } else {
                                layout_box.width - x_off
                            };
                            if seg_texdec.contains("underline") || has_href {
                                ops.push(RenderOp::FillRect {
                                    x: layout_box.x + x_off,
                                    y: layout_box.y + font_size + 2.0,
                                    width: seg_width,
                                    height: 1.0,
                                    color: seg_color,
                                });
                            }

                            // Emit a Link op for segments that carry an href
                            // so the segment is clickable.
                            if has_href {
                                ops.push(RenderOp::Link {
                                    x: layout_box.x + x_off,
                                    y: layout_box.y,
                                    width: seg_width,
                                    height: font_size + 4.0,
                                    url: seg_href,
                                });
                            }
                        }
                    }
                }
            } else {
                // Single-style text node: emit one DrawText as before.
                ops.push(RenderOp::DrawText {
                    x: layout_box.x,
                    y: layout_box.y + font_size, // baseline offset
                    text: transformed_text.clone(),
                    font_size,
                    color: text_color,
                    font_weight,
                    font_style,
                    font_family,
                    letter_spacing,
                });

                // Draw underline if text-decoration: underline is set.
                if has_text_decoration_underline(&layout_box.style) {
                    let underline_y = layout_box.y + font_size + 2.0;
                    ops.push(RenderOp::FillRect {
                        x: layout_box.x,
                        y: underline_y,
                        width: layout_box.width,
                        height: 1.0,
                        color: text_color,
                    });
                }
            }
        }
    }

    // Paint image content.
    if let LayoutContent::Image { ref src } = layout_box.content {
        if let Some(decoded) = images.get(src) {
            if decoded.len() >= 8 {
                let img_width = u32::from_le_bytes([decoded[0], decoded[1], decoded[2], decoded[3]]);
                let img_height = u32::from_le_bytes([decoded[4], decoded[5], decoded[6], decoded[7]]);
                let pixels = decoded[8..].to_vec();
                ops.push(RenderOp::DrawImage {
                    x: layout_box.x, y: layout_box.y,
                    width: layout_box.width, height: layout_box.height,
                    img_width, img_height, pixels,
                });
            }
        } else {
            ops.push(RenderOp::FillRect {
                x: layout_box.x, y: layout_box.y,
                width: layout_box.width.max(150.0), height: layout_box.height.max(80.0),
                color: Color::rgb(0.85, 0.85, 0.85),
            });
            let label = src.rsplit('/').next().unwrap_or(src);
            let label = if label.len() > 40 { format!("{}...", &label[..37]) } else { label.to_string() };
            ops.push(RenderOp::DrawText {
                x: layout_box.x + 4.0, y: layout_box.y + 16.0,
                text: format!("[img: {label}]"), font_size: 12.0,
                color: Color::rgb(0.4, 0.4, 0.4),
                font_weight: None,
                font_style: None,
                font_family: None,
                letter_spacing: None,
            });
        }
    }

    // Emit FormField op for interactive form elements.
    if let Some((field_type, value)) = extract_form_field(&layout_box.style) {
        // If this is a text input with an empty value, show placeholder text in gray.
        let placeholder = extract_placeholder(&layout_box.style);
        if value.is_empty() && !placeholder.is_empty()
            && matches!(field_type.as_str(), "text" | "email" | "search" | "url" | "tel" | "password" | "textarea")
        {
            let font_size = extract_font_size(&layout_box.style);
            let placeholder_color = multiply_alpha(Color::rgb(0.6, 0.6, 0.6), opacity);
            ops.push(RenderOp::DrawText {
                x: layout_box.x + 4.0,
                y: layout_box.y + font_size,
                text: placeholder.clone(),
                font_size,
                color: placeholder_color,
                font_weight: None,
                font_style: Some("italic".to_string()),
                font_family: None,
                letter_spacing: None,
            });
        }

        ops.push(RenderOp::FormField {
            x: layout_box.x,
            y: layout_box.y,
            width: layout_box.width,
            height: layout_box.height,
            value,
            field_type,
        });
    }

    // Render iframe placeholder.
    if let Some(iframe_src) = layout_box.style.properties.iter()
        .find(|(k, _)| k == "nova-iframe-src")
        .and_then(|(_, v)| if let StyleValue::Str(s) = v { Some(s.as_str()) } else { None })
    {
        let font_size = 11.0;
        let label = if iframe_src.len() > 60 {
            format!("[iframe: {}...]", &iframe_src[..57])
        } else {
            format!("[iframe: {iframe_src}]")
        };
        ops.push(RenderOp::DrawText {
            x: layout_box.x + 4.0,
            y: layout_box.y + font_size + 2.0,
            text: label,
            font_size,
            color: Color::rgb(0.5, 0.5, 0.5),
            font_weight: None,
            font_style: Some("italic".to_string()),
            font_family: None,
            letter_spacing: None,
        });
    }

    // Paint CSS borders — supports per-side or shorthand borders.
    let borders = extract_borders_per_side(&layout_box.style);
    let bx = layout_box.x;
    let by = layout_box.y;
    let bw = layout_box.width;
    let bh = layout_box.height;
    // Top border
    if let Some((w, color, ref style)) = borders.top {
        let c = multiply_alpha(color, opacity);
        let color = if let Some(ref f) = css_filter { apply_filter_to_color(c, f) } else { c };
        emit_border_ops(ops, bx, by, bw, w, color, style, true);
    }
    // Bottom border
    if let Some((w, color, ref style)) = borders.bottom {
        let c = multiply_alpha(color, opacity);
        let color = if let Some(ref f) = css_filter { apply_filter_to_color(c, f) } else { c };
        emit_border_ops(ops, bx, by + bh - w, bw, w, color, style, true);
    }
    // Left border
    if let Some((w, color, ref style)) = borders.left {
        let c = multiply_alpha(color, opacity);
        let color = if let Some(ref f) = css_filter { apply_filter_to_color(c, f) } else { c };
        emit_border_ops(ops, bx, by, w, bh, color, style, false);
    }
    // Right border
    if let Some((w, color, ref style)) = borders.right {
        let c = multiply_alpha(color, opacity);
        let color = if let Some(ref f) = css_filter { apply_filter_to_color(c, f) } else { c };
        emit_border_ops(ops, bx + bw - w, by, w, bh, color, style, false);
    }

    // Emit a Link op if this box has an href (i.e., it is an <a> element).
    if let Some(href) = extract_href(&layout_box.style) {
        if layout_box.width > 0.0 && layout_box.height > 0.0 {
            ops.push(RenderOp::Link {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width,
                height: layout_box.height,
                url: href,
            });
        }
    }

    // Apply CSS clip-path: inset(...) if present.
    let clip_path_inset = extract_clip_path(&layout_box.style);
    if let Some((top, right, bottom, left)) = clip_path_inset {
        ops.push(RenderOp::PushClip {
            x: layout_box.x + left,
            y: layout_box.y + top,
            width: (layout_box.width - left - right).max(0.0),
            height: (layout_box.height - top - bottom).max(0.0),
        });
    }

    // Check whether this box clips its overflow.
    let clips_overflow = has_overflow_clip(&layout_box.style);

    // Check if this is a scrollable container (overflow: auto/scroll with overflowing content).
    let is_scroll_container = is_overflow_scroll(&layout_box.style);
    let content_height = layout_box.children.iter()
        .map(|c| c.y + c.height - layout_box.y)
        .fold(0.0_f32, f32::max);
    if is_scroll_container && content_height > layout_box.height {
        ops.push(RenderOp::ScrollContainerStart {
            x: layout_box.x,
            y: layout_box.y,
            width: layout_box.width,
            height: layout_box.height,
            content_height,
        });
    } else if clips_overflow {
        ops.push(RenderOp::PushClip {
            x: layout_box.x,
            y: layout_box.y,
            width: layout_box.width,
            height: layout_box.height,
        });
    }

    // Recurse into children. Children that create stacking contexts are
    // painted in z-index order; other children paint in DOM order.
    let mut child_indices: Vec<usize> = (0..layout_box.children.len()).collect();
    child_indices.sort_by_key(|&i| layout_box.children[i].z_index);
    for i in child_indices {
        let child = &layout_box.children[i];
        let is_stacking_ctx = creates_stacking_context(&child.style);
        if is_stacking_ctx {
            // Isolate this child's painting in a save/restore block.
            ops.push(RenderOp::Save);
        }
        paint_box(child, ops, images);
        if is_stacking_ctx {
            ops.push(RenderOp::Restore);
        }
    }

    if is_scroll_container && content_height > layout_box.height {
        ops.push(RenderOp::ScrollContainerEnd);
    } else if clips_overflow {
        ops.push(RenderOp::PopClip);
    }

    if clip_path_inset.is_some() {
        ops.push(RenderOp::PopClip);
    }

    if is_sticky || is_fixed {
        ops.push(RenderOp::StickyEnd);
    }

    if needs_translate {
        ops.push(RenderOp::Restore);
    }
}

/// Check if the style requests text-overflow: ellipsis.
fn has_text_overflow_ellipsis(style: &nova_mod_api::content::StyleMap) -> bool {
    let mut has_ellipsis = false;
    let mut has_overflow_hidden = false;

    for (key, value) in &style.properties {
        match key.as_str() {
            "text-overflow" => {
                if let StyleValue::Keyword(k) | StyleValue::Str(k) = value {
                    if k.trim() == "ellipsis" {
                        has_ellipsis = true;
                    }
                }
            }
            "overflow" | "overflow-x" => {
                if let StyleValue::Keyword(k) | StyleValue::Str(k) = value {
                    if matches!(k.trim(), "hidden" | "clip") {
                        has_overflow_hidden = true;
                    }
                }
            }
            _ => {}
        }
    }

    has_ellipsis && has_overflow_hidden
}

/// Truncate text and append "..." if it exceeds the given width.
///
/// Uses character width estimation. Not exact but good enough for visible
/// truncation.
fn truncate_with_ellipsis(text: &str, font_size: f32, max_width: f32) -> String {
    let ellipsis = "...";
    let ellipsis_width = estimate_text_width(ellipsis, font_size);

    if max_width <= ellipsis_width {
        return ellipsis.to_string();
    }

    let available = max_width - ellipsis_width;
    let mut current_width = 0.0;
    let mut truncated = String::new();

    for ch in text.chars() {
        let char_width = estimate_char_width(ch, font_size);
        if current_width + char_width > available {
            truncated.push_str(ellipsis);
            return truncated;
        }
        current_width += char_width;
        truncated.push(ch);
    }

    // Text fits — no truncation needed.
    text.to_string()
}

/// Estimate the width of a single character at a given font size.
fn estimate_char_width(_ch: char, font_size: f32) -> f32 {
    // Rough approximation: average character width ≈ 0.6 * font_size.
    font_size * 0.6
}

/// Estimate the width of a string at a given font size.
fn estimate_text_width(text: &str, font_size: f32) -> f32 {
    text.chars().count() as f32 * font_size * 0.6
}

/// Return `true` if the style map requests overflow clipping
/// (`overflow: hidden`, `overflow: scroll`, or `overflow: auto`,
/// as well as the longhand `overflow-x` / `overflow-y` variants).
fn has_overflow_clip(style: &nova_mod_api::content::StyleMap) -> bool {
    for (key, value) in &style.properties {
        if key == "overflow" || key == "overflow-x" || key == "overflow-y" {
            let clip_val = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            if matches!(clip_val, "hidden" | "scroll" | "auto") {
                return true;
            }
        }
    }
    false
}

/// Return `true` if the style requests scrollable overflow (`overflow: auto` or `overflow: scroll`).
fn is_overflow_scroll(style: &nova_mod_api::content::StyleMap) -> bool {
    for (key, value) in &style.properties {
        if key == "overflow" || key == "overflow-y" {
            let val = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            if matches!(val, "scroll" | "auto") {
                return true;
            }
        }
    }
    false
}

/// Multiply a color's alpha channel by an opacity factor.
///
/// Used to apply the CSS `opacity` property to any painted color.
#[inline]
fn multiply_alpha(color: Color, opacity: f32) -> Color {
    Color { r: color.r, g: color.g, b: color.b, a: color.a * opacity }
}

/// Extract the CSS `opacity` value (0.0–1.0) from a style map.
///
/// Returns `1.0` when the property is not set.
fn extract_opacity(style: &nova_mod_api::content::StyleMap) -> f32 {
    for (key, value) in &style.properties {
        if key == "opacity" {
            match value {
                StyleValue::Number(n) => return n.clamp(0.0, 1.0),
                StyleValue::Str(s) | StyleValue::Keyword(s) => {
                    if let Ok(v) = s.parse::<f32>() {
                        return v.clamp(0.0, 1.0);
                    }
                }
                _ => {}
            }
        }
    }
    1.0
}

/// Parse a CSS `box-shadow` value and return `(color, offset_x, offset_y, blur)`.
///
/// Supports the `<offset-x> <offset-y> <blur-radius> <color>` and
/// `<offset-x> <offset-y> <color>` forms.  Returns `None` if the property
/// is not set or cannot be parsed.
fn extract_box_shadow(
    style: &nova_mod_api::content::StyleMap,
) -> Option<(Color, f32, f32, f32)> {
    for (key, value) in &style.properties {
        if key == "box-shadow" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                _ => continue,
            };
            let s = s.trim();
            if s == "none" {
                return None;
            }
            // Split into whitespace-separated tokens.
            let tokens: Vec<&str> = s.split_whitespace().collect();
            // Try to parse the first two tokens as offset_x and offset_y.
            // Remaining tokens may include blur and color.
            if tokens.len() < 2 {
                continue;
            }
            let offset_x = parse_length_px(tokens[0]).unwrap_or(0.0);
            let offset_y = parse_length_px(tokens[1]).unwrap_or(0.0);
            let mut blur = 0.0_f32;
            let mut color = Color::rgba(0.0, 0.0, 0.0, 0.5);

            // Try token[2] as blur radius, then color.
            let mut color_start = 2;
            if tokens.len() > 2 {
                if let Some(b) = parse_length_px(tokens[2]) {
                    blur = b;
                    color_start = 3;
                }
            }
            // The rest is the color string.
            if color_start < tokens.len() {
                let color_str = tokens[color_start..].join(" ");
                if let Some(c) = parse_color_string(&color_str) {
                    color = c;
                }
            }
            return Some((color, offset_x, offset_y, blur));
        }
    }
    None
}

/// Parse a CSS length value like `4px`, `0.5em`, `8`, returning pixels.
///
/// `em` and `rem` are approximated as 16 px.
fn parse_length_px(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("px") {
        v.trim().parse::<f32>().ok()
    } else if let Some(v) = s.strip_suffix("em") {
        v.trim().parse::<f32>().ok().map(|n| n * 16.0)
    } else if let Some(v) = s.strip_suffix("rem") {
        v.trim().parse::<f32>().ok().map(|n| n * 16.0)
    } else {
        s.parse::<f32>().ok()
    }
}

/// Extract the CSS `border-radius` from a style map.
///
/// Returns corner radii as `[top-left, top-right, bottom-right, bottom-left]`
/// in pixels.  Percentage values are resolved against the smaller of the
/// element's `width` and `height`.  All-zero means no rounding.
fn extract_border_radius(
    style: &nova_mod_api::content::StyleMap,
    width: f32,
    height: f32,
) -> [f32; 4] {
    for (key, value) in &style.properties {
        if key == "border-radius" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                StyleValue::Px(px) => return [*px; 4],
                StyleValue::Number(n) => return [*n; 4],
                _ => continue,
            };
            let s = s.trim();
            // Split on whitespace to support 1–4 value shorthand.
            let parts: Vec<&str> = s.split_whitespace().collect();
            let parse = |token: &str| -> f32 {
                let token = token.trim();
                if let Some(v) = token.strip_suffix('%') {
                    let pct = v.trim().parse::<f32>().unwrap_or(0.0) / 100.0;
                    // Use the shorter side so the circle doesn't overflow.
                    pct * width.min(height) * 0.5
                } else {
                    parse_length_px(token).unwrap_or(0.0)
                }
            };
            return match parts.len() {
                1 => {
                    let r = parse(parts[0]);
                    [r, r, r, r]
                }
                2 => {
                    let tl_br = parse(parts[0]);
                    let tr_bl = parse(parts[1]);
                    [tl_br, tr_bl, tl_br, tr_bl]
                }
                3 => {
                    let tl = parse(parts[0]);
                    let tr_bl = parse(parts[1]);
                    let br = parse(parts[2]);
                    [tl, tr_bl, br, tr_bl]
                }
                0 => [0.0; 4],
                _ => {
                    let tl = parse(parts[0]);
                    let tr = if parts.len() > 1 { parse(parts[1]) } else { tl };
                    let br = if parts.len() > 2 { parse(parts[2]) } else { tl };
                    let bl = if parts.len() > 3 { parse(parts[3]) } else { tr };
                    [tl, tr, br, bl]
                }
            };
        }
    }
    [0.0; 4]
}

/// Apply the CSS `text-transform` property to a text string.
///
/// Handles `uppercase`, `lowercase`, and `capitalize`.
fn apply_text_transform(text: &str, style: &nova_mod_api::content::StyleMap) -> String {
    for (key, value) in &style.properties {
        if key == "text-transform" {
            let transform = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            return match transform.trim() {
                "uppercase" => text.to_uppercase(),
                "lowercase" => text.to_lowercase(),
                "capitalize" => capitalize_words(text),
                _ => text.to_owned(),
            };
        }
    }
    text.to_owned()
}

/// Capitalize the first character of every whitespace-separated word.
fn capitalize_words(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut capitalize_next = true;
    for ch in text.chars() {
        if ch.is_whitespace() {
            capitalize_next = true;
            result.push(ch);
        } else if capitalize_next {
            for upper in ch.to_uppercase() {
                result.push(upper);
            }
            capitalize_next = false;
        } else {
            result.push(ch);
        }
    }
    result
}

/// Extract the href from a style map (set by the layout mod for `<a>` elements).
fn extract_href(style: &nova_mod_api::content::StyleMap) -> Option<String> {
    for (key, value) in &style.properties {
        if key == "href" {
            if let StyleValue::Str(url) = value {
                return Some(url.clone());
            }
        }
    }
    None
}

/// Extract the background color from a style map, defaulting to transparent.
fn extract_background_color(style: &nova_mod_api::content::StyleMap) -> Color {
    for (key, value) in &style.properties {
        if key == "background-color" || key == "background" {
            if let Some(c) = style_value_to_color(value) {
                return c;
            }
        }
    }
    Color::TRANSPARENT
}

/// Extract the text color from a style map, defaulting to black.
fn extract_text_color(style: &nova_mod_api::content::StyleMap) -> Color {
    for (key, value) in &style.properties {
        if key == "color" {
            if let Some(c) = style_value_to_color(value) {
                return c;
            }
        }
    }
    Color::BLACK
}

/// Convert a StyleValue to a Color, handling Color, Str, and Keyword variants.
fn style_value_to_color(value: &StyleValue) -> Option<Color> {
    match value {
        StyleValue::Color(c) => Some(Color::rgba(
            c.r as f32 / 255.0,
            c.g as f32 / 255.0,
            c.b as f32 / 255.0,
            c.a,
        )),
        StyleValue::Str(s) | StyleValue::Keyword(s) => parse_color_string(s),
        _ => None,
    }
}

/// Parse a CSS color string like "#eee", "rgb(0,0,238)", "blue", etc.
///
/// Also handles bare hex strings without the `#` prefix (e.g. "ff6600")
/// which can occur when colours are stored from presentational HTML attributes.
fn parse_color_string(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.starts_with('#') {
        parse_hex_color(s)
    } else if s.starts_with("rgb") {
        parse_rgb_color(s)
    } else if s.starts_with("hsl") {
        parse_hsl_color(s)
    } else {
        // Try named colour first, then fall back to bare hex (no '#' prefix).
        parse_named_color(s).or_else(|| {
            // A bare hex string must be exactly 3, 6, or 8 hex chars.
            if (s.len() == 3 || s.len() == 6 || s.len() == 8)
                && s.chars().all(|c| c.is_ascii_hexdigit())
            {
                parse_hex_color(&format!("#{s}"))
            } else {
                None
            }
        })
    }
}

/// Parse a hex colour string.
///
/// Supports `#rgb`, `#rrggbb`, and `#rrggbbaa` formats. The leading `#` is
/// stripped before parsing so callers may include it or not.
fn parse_hex_color(s: &str) -> Option<Color> {
    let hex = s.trim_start_matches('#');
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
            Some(Color::rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Color::rgba(
                r as f32 / 255.0,
                g as f32 / 255.0,
                b as f32 / 255.0,
                a as f32 / 255.0,
            ))
        }
        _ => None,
    }
}

fn parse_rgb_color(s: &str) -> Option<Color> {
    let inner = s.trim_start_matches("rgba(")
        .trim_start_matches("rgb(")
        .trim_end_matches(')');
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() >= 3 {
        let r = parts[0].trim().parse::<f32>().ok()? / 255.0;
        let g = parts[1].trim().parse::<f32>().ok()? / 255.0;
        let b = parts[2].trim().parse::<f32>().ok()? / 255.0;
        let a = if parts.len() >= 4 {
            parts[3].trim().parse::<f32>().ok().unwrap_or(1.0)
        } else {
            1.0
        };
        Some(Color::rgba(r, g, b, a))
    } else {
        None
    }
}

/// Parse an HSL/HSLA color string into a Color.
fn parse_hsl_color(s: &str) -> Option<Color> {
    let inner = s
        .trim_start_matches("hsla(")
        .trim_start_matches("hsl(")
        .trim_end_matches(')');

    let parts: Vec<&str> = if inner.contains(',') {
        inner.split(',').map(str::trim).collect()
    } else {
        let slash_parts: Vec<&str> = inner.splitn(2, '/').collect();
        let mut hsl: Vec<&str> = slash_parts[0].split_whitespace().collect();
        if slash_parts.len() > 1 {
            hsl.push(slash_parts[1].trim());
        }
        hsl
    };

    if parts.len() < 3 {
        return None;
    }

    let h = {
        let s = parts[0].trim();
        let val = if let Some(v) = s.strip_suffix("deg") {
            v.trim().parse::<f32>().ok()?
        } else if let Some(v) = s.strip_suffix("rad") {
            v.trim().parse::<f32>().ok()?.to_degrees()
        } else if let Some(v) = s.strip_suffix("turn") {
            v.trim().parse::<f32>().ok()? * 360.0
        } else {
            s.parse::<f32>().ok()?
        };
        ((val % 360.0) + 360.0) % 360.0
    };
    let s_val = parts[1].trim().trim_end_matches('%').trim().parse::<f32>().ok()? / 100.0;
    let l = parts[2].trim().trim_end_matches('%').trim().parse::<f32>().ok()? / 100.0;
    let a = if parts.len() >= 4 {
        let a_str = parts[3].trim();
        if let Some(pct) = a_str.strip_suffix('%') {
            pct.trim().parse::<f32>().unwrap_or(100.0) / 100.0
        } else {
            a_str.parse::<f32>().unwrap_or(1.0)
        }
    } else {
        1.0
    };

    // HSL to RGB conversion
    let (r, g, b) = if s_val == 0.0 {
        (l, l, l)
    } else {
        let q = if l < 0.5 { l * (1.0 + s_val) } else { l + s_val - l * s_val };
        let p = 2.0 * l - q;
        let h_norm = h / 360.0;
        let hue2rgb = |p: f32, q: f32, mut t: f32| -> f32 {
            if t < 0.0 { t += 1.0; }
            if t > 1.0 { t -= 1.0; }
            if t < 1.0 / 6.0 { return p + (q - p) * 6.0 * t; }
            if t < 1.0 / 2.0 { return q; }
            if t < 2.0 / 3.0 { return p + (q - p) * (2.0 / 3.0 - t) * 6.0; }
            p
        };
        (hue2rgb(p, q, h_norm + 1.0/3.0), hue2rgb(p, q, h_norm), hue2rgb(p, q, h_norm - 1.0/3.0))
    };

    Some(Color::rgba(r, g, b, a))
}

fn parse_named_color(s: &str) -> Option<Color> {
    match s.to_lowercase().as_str() {
        "black" => Some(Color::BLACK),
        "white" => Some(Color::WHITE),
        "red" => Some(Color::rgb(1.0, 0.0, 0.0)),
        "green" => Some(Color::rgb(0.0, 0.502, 0.0)),
        "lime" => Some(Color::rgb(0.0, 1.0, 0.0)),
        "blue" => Some(Color::rgb(0.0, 0.0, 1.0)),
        "yellow" => Some(Color::rgb(1.0, 1.0, 0.0)),
        "cyan" | "aqua" => Some(Color::rgb(0.0, 1.0, 1.0)),
        "magenta" | "fuchsia" => Some(Color::rgb(1.0, 0.0, 1.0)),
        "orange" => Some(Color::rgb(1.0, 0.647, 0.0)),
        "purple" => Some(Color::rgb(0.502, 0.0, 0.502)),
        "navy" => Some(Color::rgb(0.0, 0.0, 0.502)),
        "teal" => Some(Color::rgb(0.0, 0.502, 0.502)),
        "maroon" => Some(Color::rgb(0.502, 0.0, 0.0)),
        "olive" => Some(Color::rgb(0.502, 0.502, 0.0)),
        "silver" => Some(Color::rgb(0.753, 0.753, 0.753)),
        "gray" | "grey" => Some(Color::rgb(0.502, 0.502, 0.502)),
        "lightgray" | "lightgrey" => Some(Color::rgb(0.827, 0.827, 0.827)),
        "darkgray" | "darkgrey" => Some(Color::rgb(0.663, 0.663, 0.663)),
        "brown" => Some(Color::rgb(0.647, 0.165, 0.165)),
        "pink" => Some(Color::rgb(1.0, 0.753, 0.796)),
        "coral" => Some(Color::rgb(1.0, 0.498, 0.314)),
        "tomato" => Some(Color::rgb(1.0, 0.388, 0.278)),
        "gold" => Some(Color::rgb(1.0, 0.843, 0.0)),
        "transparent" => Some(Color::TRANSPARENT),
        _ => None,
    }
}

/// Check if the style map contains `text-decoration: underline`.
fn has_text_decoration_underline(style: &nova_mod_api::content::StyleMap) -> bool {
    for (key, value) in &style.properties {
        if key == "text-decoration" {
            match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => {
                    return k.contains("underline");
                }
                _ => {}
            }
        }
    }
    false
}

/// Extract the CSS font-weight from a style map as a numeric value.
///
/// Maps `"bold"` → `700`, `"normal"` → `400`, numeric strings to their value.
/// Returns `None` if not set (caller defaults to normal/400).
fn extract_font_weight(style: &nova_mod_api::content::StyleMap) -> Option<u16> {
    for (key, value) in &style.properties {
        if key == "font-weight" {
            match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => {
                    return match k.as_str() {
                        "bold" => Some(700),
                        "bolder" => Some(700),
                        "lighter" => Some(300),
                        "normal" => Some(400),
                        _ => k.parse::<u16>().ok(),
                    };
                }
                StyleValue::Number(n) => return Some(*n as u16),
                _ => {}
            }
        }
    }
    None
}

/// Extract the CSS `letter-spacing` from a style map.
///
/// Returns `Some(px)` if a pixel value is set, `None` for `normal` or absent.
fn extract_letter_spacing(style: &nova_mod_api::content::StyleMap) -> Option<f32> {
    for (key, value) in &style.properties {
        if key == "letter-spacing" {
            match value {
                StyleValue::Px(px) => return Some(*px),
                StyleValue::Keyword(k) | StyleValue::Str(k) => {
                    if k.trim() == "normal" {
                        return None;
                    }
                    if let Some(px) = k.strip_suffix("px").and_then(|s| s.trim().parse::<f32>().ok()) {
                        return Some(px);
                    }
                }
                StyleValue::Number(n) => return Some(*n),
                _ => {}
            }
        }
    }
    None
}

/// Extract the CSS font-style from a style map.
///
/// Returns `Some("italic")` or `Some("oblique")` if set, `None` otherwise.
fn extract_font_style(style: &nova_mod_api::content::StyleMap) -> Option<String> {
    for (key, value) in &style.properties {
        if key == "font-style" {
            match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => {
                    if k == "italic" || k == "oblique" {
                        return Some(k.clone());
                    }
                    return None; // "normal" or other
                }
                _ => {}
            }
        }
    }
    None
}

/// Extract the first CSS font-family name from a style map.
///
/// Returns the first family name (stripped of quotes) if set, `None` otherwise.
/// Generic families like `sans-serif`, `serif`, `monospace` are ignored since
/// they map to the default font.
fn extract_font_family(style: &nova_mod_api::content::StyleMap) -> Option<String> {
    for (key, value) in &style.properties {
        if key == "font-family" {
            let raw = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            // font-family can be a comma-separated list. Take the first entry.
            let first = raw.split(',').next().unwrap_or(raw).trim();
            let first = first.trim_matches(|c: char| c == '"' || c == '\'');
            // Skip generic families — the renderer already handles those.
            let generic = [
                "sans-serif", "serif", "monospace", "cursive", "fantasy",
                "system-ui", "-apple-system", "BlinkMacSystemFont",
            ];
            if generic.iter().any(|g| g.eq_ignore_ascii_case(first)) {
                return None;
            }
            if !first.is_empty() {
                return Some(first.to_string());
            }
        }
    }
    None
}

/// Emit border render ops, handling dashed/dotted styles.
///
/// For `dashed` borders, emits a series of short `FillRect` dashes.
/// For `dotted` borders, emits square dots.
/// All other styles (solid, double, groove, etc.) render as solid.
fn emit_border_ops(
    ops: &mut Vec<RenderOp>,
    x: f32, y: f32, width: f32, height: f32,
    color: Color, style: &str, is_horizontal: bool,
) {
    match style {
        "dashed" => {
            let dash_len = if is_horizontal { height.max(3.0) * 3.0 } else { width.max(3.0) * 3.0 };
            let gap_len = dash_len;
            let total = if is_horizontal { width } else { height };
            let mut offset = 0.0;
            while offset < total {
                let len = dash_len.min(total - offset);
                if is_horizontal {
                    ops.push(RenderOp::FillRect {
                        x: x + offset, y, width: len, height, color,
                    });
                } else {
                    ops.push(RenderOp::FillRect {
                        x, y: y + offset, width, height: len, color,
                    });
                }
                offset += dash_len + gap_len;
            }
        }
        "dotted" => {
            let dot_size = if is_horizontal { height.max(1.0) } else { width.max(1.0) };
            let gap = dot_size;
            let total = if is_horizontal { width } else { height };
            let mut offset = 0.0;
            while offset < total {
                if is_horizontal {
                    ops.push(RenderOp::FillRect {
                        x: x + offset, y, width: dot_size, height, color,
                    });
                } else {
                    ops.push(RenderOp::FillRect {
                        x, y: y + offset, width, height: dot_size, color,
                    });
                }
                offset += dot_size + gap;
            }
        }
        _ => {
            // solid, double, groove, ridge, etc. — render as solid.
            ops.push(RenderOp::FillRect { x, y, width, height, color });
        }
    }
}

/// Per-side border info: `(width, color, style)`.
struct BorderSides {
    top: Option<(f32, Color, String)>,
    right: Option<(f32, Color, String)>,
    bottom: Option<(f32, Color, String)>,
    left: Option<(f32, Color, String)>,
}

/// Extract per-side CSS border properties from a style map.
fn extract_borders_per_side(style: &nova_mod_api::content::StyleMap) -> BorderSides {
    // Shorthand defaults.
    let mut sh_style: Option<String> = None;
    let mut sh_width: Option<f32> = None;
    let mut sh_color: Option<Color> = None;

    // Per-side overrides.
    let mut side_style: [Option<String>; 4] = [None, None, None, None]; // top, right, bottom, left
    let mut side_width: [Option<f32>; 4] = [None; 4];
    let mut side_color: [Option<Color>; 4] = [None; 4];

    for (key, value) in &style.properties {
        let (target_style, target_width, target_color) = match key.as_str() {
            "border-style" => { sh_style = extract_keyword(value); continue; }
            "border-width" => { sh_width = extract_px_value(value); continue; }
            "border-color" => { sh_color = style_value_to_color(value); continue; }
            "border-top-style" => (&mut side_style[0], &mut side_width[0], &mut side_color[0]),
            "border-right-style" => (&mut side_style[1], &mut side_width[1], &mut side_color[1]),
            "border-bottom-style" => (&mut side_style[2], &mut side_width[2], &mut side_color[2]),
            "border-left-style" => (&mut side_style[3], &mut side_width[3], &mut side_color[3]),
            "border-top-width" => { side_width[0] = extract_px_value(value); continue; }
            "border-right-width" => { side_width[1] = extract_px_value(value); continue; }
            "border-bottom-width" => { side_width[2] = extract_px_value(value); continue; }
            "border-left-width" => { side_width[3] = extract_px_value(value); continue; }
            "border-top-color" => { side_color[0] = style_value_to_color(value); continue; }
            "border-right-color" => { side_color[1] = style_value_to_color(value); continue; }
            "border-bottom-color" => { side_color[2] = style_value_to_color(value); continue; }
            "border-left-color" => { side_color[3] = style_value_to_color(value); continue; }
            _ => continue,
        };
        *target_style = extract_keyword(value);
        let _ = (target_width, target_color); // suppress unused
    }

    let make_side = |i: usize| -> Option<(f32, Color, String)> {
        let style_val = side_style[i].as_deref().or(sh_style.as_deref()).unwrap_or("none");
        if style_val == "none" || style_val == "hidden" {
            return None;
        }
        let w = side_width[i].or(sh_width).unwrap_or(1.0);
        let c = side_color[i].or(sh_color).unwrap_or(Color::BLACK);
        if w > 0.0 { Some((w, c, style_val.to_string())) } else { None }
    };

    BorderSides {
        top: make_side(0),
        right: make_side(1),
        bottom: make_side(2),
        left: make_side(3),
    }
}

fn extract_keyword(value: &StyleValue) -> Option<String> {
    match value {
        StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.clone()),
        _ => None,
    }
}

fn extract_px_value(value: &StyleValue) -> Option<f32> {
    match value {
        StyleValue::Px(px) => Some(*px),
        StyleValue::Str(s) | StyleValue::Keyword(s) => parse_length_px(s),
        _ => None,
    }
}

/// Extract form field info from style props (set by mod-layout for form elements).
fn extract_form_field(style: &nova_mod_api::content::StyleMap) -> Option<(String, String)> {
    let mut field_type = None;
    let mut value = String::new();
    for (key, val) in &style.properties {
        if key == "nova-form-type" {
            if let StyleValue::Str(s) | StyleValue::Keyword(s) = val {
                field_type = Some(s.clone());
            }
        }
        if key == "nova-form-value" {
            if let StyleValue::Str(s) | StyleValue::Keyword(s) = val {
                value = s.clone();
            }
        }
    }
    field_type.map(|ft| (ft, value))
}

/// Extract the placeholder text from a style map (set by mod-layout for form inputs).
fn extract_placeholder(style: &nova_mod_api::content::StyleMap) -> String {
    for (key, value) in &style.properties {
        if key == "nova-placeholder" {
            if let StyleValue::Str(s) | StyleValue::Keyword(s) = value {
                return s.clone();
            }
        }
    }
    String::new()
}

/// Check if the element has `position: sticky`.
fn is_position_sticky(style: &nova_mod_api::content::StyleMap) -> bool {
    for (key, value) in &style.properties {
        if key == "position" {
            if let StyleValue::Keyword(k) | StyleValue::Str(k) = value {
                return k == "sticky";
            }
        }
    }
    false
}

/// Extract the `top` value for a sticky element (defaults to 0).
fn extract_sticky_top(style: &nova_mod_api::content::StyleMap) -> f32 {
    for (key, value) in &style.properties {
        if key == "top" {
            match value {
                StyleValue::Px(px) => return *px,
                StyleValue::Str(s) | StyleValue::Keyword(s) => {
                    if let Some(px) = parse_length_px(s) {
                        return px;
                    }
                }
                _ => {}
            }
        }
    }
    0.0
}

/// Check if the element has `position: fixed`.
fn is_position_fixed(style: &nova_mod_api::content::StyleMap) -> bool {
    for (key, value) in &style.properties {
        if key == "position" {
            if let StyleValue::Keyword(k) | StyleValue::Str(k) = value {
                return k == "fixed";
            }
        }
    }
    false
}

/// Extract the `top` value for a fixed-position element (defaults to 0).
fn extract_fixed_top(style: &nova_mod_api::content::StyleMap) -> f32 {
    for (key, value) in &style.properties {
        if key == "top" {
            match value {
                StyleValue::Px(px) => return *px,
                StyleValue::Str(s) | StyleValue::Keyword(s) => {
                    if let Some(px) = parse_length_px(s) {
                        return px;
                    }
                }
                _ => {}
            }
        }
    }
    0.0
}

/// Extract the font size from a style map, defaulting to 16px.
fn extract_font_size(style: &nova_mod_api::content::StyleMap) -> f32 {
    for (key, value) in &style.properties {
        if key == "font-size" {
            if let StyleValue::Px(px) = value {
                return *px;
            }
        }
    }
    16.0
}

/// Extract the `background-repeat` CSS value. Defaults to "repeat".
fn extract_background_repeat(style: &nova_mod_api::content::StyleMap) -> &'static str {
    for (key, value) in &style.properties {
        if key == "background-repeat" {
            let val = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            return match val.trim() {
                "no-repeat" => "no-repeat",
                "repeat-x" => "repeat-x",
                "repeat-y" => "repeat-y",
                "repeat" => "repeat",
                _ => "repeat",
            };
        }
    }
    "repeat"
}

/// Extract the `background-position` CSS value as (x_fraction, y_fraction) in 0.0-1.0.
/// Defaults to (0.0, 0.0) (top-left).
fn extract_background_position(style: &nova_mod_api::content::StyleMap) -> (f32, f32) {
    for (key, value) in &style.properties {
        if key == "background-position" {
            let val = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            let val = val.trim();
            let parts: Vec<&str> = val.split_whitespace().collect();

            let parse_pos = |s: &str| -> f32 {
                match s {
                    "left" | "top" => 0.0,
                    "center" => 0.5,
                    "right" | "bottom" => 1.0,
                    _ => {
                        if let Some(pct) = s.strip_suffix('%') {
                            pct.trim().parse::<f32>().unwrap_or(0.0) / 100.0
                        } else {
                            0.0
                        }
                    }
                }
            };

            return match parts.len() {
                1 => {
                    let v = parse_pos(parts[0]);
                    (v, v)
                }
                _ => {
                    (parse_pos(parts[0]), parse_pos(parts[1]))
                }
            };
        }
    }
    (0.0, 0.0)
}

/// Extract the `background-size` CSS value.
/// Returns the computed (width, height) in pixels for the background image.
/// Special values `cover` and `contain` are resolved against the box and image dimensions.
fn extract_background_size(
    style: &nova_mod_api::content::StyleMap,
    box_w: f32,
    box_h: f32,
    img_w: f32,
    img_h: f32,
) -> (f32, f32) {
    for (key, value) in &style.properties {
        if key == "background-size" {
            let val = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            let val = val.trim();
            match val {
                "cover" => {
                    if img_w <= 0.0 || img_h <= 0.0 { return (box_w, box_h); }
                    let ratio_w = box_w / img_w;
                    let ratio_h = box_h / img_h;
                    let scale = ratio_w.max(ratio_h);
                    return (img_w * scale, img_h * scale);
                }
                "contain" => {
                    if img_w <= 0.0 || img_h <= 0.0 { return (box_w, box_h); }
                    let ratio_w = box_w / img_w;
                    let ratio_h = box_h / img_h;
                    let scale = ratio_w.min(ratio_h);
                    return (img_w * scale, img_h * scale);
                }
                "auto" => {
                    return (img_w, img_h);
                }
                _ => {
                    let parts: Vec<&str> = val.split_whitespace().collect();
                    let parse_size = |s: &str, reference: f32, img_dim: f32| -> f32 {
                        if s == "auto" { return img_dim; }
                        if let Some(pct) = s.strip_suffix('%') {
                            return pct.trim().parse::<f32>().unwrap_or(100.0) / 100.0 * reference;
                        }
                        parse_length_px(s).unwrap_or(img_dim)
                    };
                    match parts.len() {
                        1 => {
                            let w = parse_size(parts[0], box_w, img_w);
                            let h = if img_w > 0.0 { w * img_h / img_w } else { img_h };
                            return (w, h);
                        }
                        _ => {
                            return (
                                parse_size(parts[0], box_w, img_w),
                                parse_size(parts[1], box_h, img_h),
                            );
                        }
                    }
                }
            }
        }
    }
    (img_w, img_h)
}

/// Extract `background-image` CSS value from a style map.
fn extract_background_image_value(style: &nova_mod_api::content::StyleMap) -> Option<String> {
    for (key, value) in &style.properties {
        if key == "background-image" || key == "background" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                _ => continue,
            };
            if s.contains("url(") || s.contains("gradient(") {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Extract a URL from a CSS `url(...)` value.
fn extract_css_url(value: &str) -> Option<String> {
    let idx = value.find("url(")?;
    let after = &value[idx + 4..];
    let trimmed = after.trim_start();
    let (url_str, _) = if trimmed.starts_with('"') {
        let inner = &trimmed[1..];
        let end = inner.find('"')?;
        (&inner[..end], &inner[end + 1..])
    } else if trimmed.starts_with('\'') {
        let inner = &trimmed[1..];
        let end = inner.find('\'')?;
        (&inner[..end], &inner[end + 1..])
    } else {
        let end = trimmed.find(')')?;
        (trimmed[..end].trim(), &trimmed[end..])
    };
    if url_str.is_empty() { None } else { Some(url_str.to_string()) }
}

/// Render a CSS `linear-gradient(...)` as horizontal color strips.
fn paint_linear_gradient(
    value: &str,
    x: f32, y: f32, width: f32, height: f32,
    opacity: f32,
    ops: &mut Vec<RenderOp>,
) {
    // Extract the content inside linear-gradient(...).
    let inner = if let Some(s) = value.strip_prefix("linear-gradient(") {
        s.strip_suffix(')').unwrap_or(s)
    } else if let Some(idx) = value.find("linear-gradient(") {
        let after = &value[idx + "linear-gradient(".len()..];
        after.strip_suffix(')').unwrap_or(after)
    } else {
        return;
    };

    // Parse color stops. Simple approach: split by comma, parse each as "color [position%]".
    let parts: Vec<&str> = split_gradient_args(inner);
    if parts.len() < 2 {
        return;
    }

    // Check if first part is a direction.
    let (is_vertical, color_parts) = if parts[0].starts_with("to ") || parts[0].ends_with("deg") {
        let dir = parts[0].trim();
        let horizontal = dir == "to right" || dir == "to left" || dir == "90deg" || dir == "270deg";
        (!horizontal, &parts[1..])
    } else {
        (true, &parts[..]) // default: to bottom (vertical)
    };

    // Parse color stops.
    let mut stops: Vec<(f32, Color)> = Vec::new();
    for (i, part) in color_parts.iter().enumerate() {
        let part = part.trim();
        // Try to extract percentage at the end.
        let (color_str, pct) = if let Some(pct_idx) = part.rfind('%') {
            let before_pct = part[..pct_idx].trim();
            // Find the last space before the percentage number.
            if let Some(space_idx) = before_pct.rfind(' ') {
                let num_str = &before_pct[space_idx + 1..];
                let color = &before_pct[..space_idx].trim();
                let p = num_str.parse::<f32>().unwrap_or(0.0) / 100.0;
                (*color, p)
            } else {
                (part, i as f32 / (color_parts.len() - 1).max(1) as f32)
            }
        } else {
            (part, i as f32 / (color_parts.len() - 1).max(1) as f32)
        };

        if let Some(color) = parse_color_string(color_str) {
            stops.push((pct, color));
        }
    }

    if stops.len() < 2 {
        return;
    }

    // Render strips.
    let steps = if is_vertical { height as usize } else { width as usize };
    for step in 0..steps.max(1) {
        let t = step as f32 / (steps - 1).max(1) as f32;
        let color = interpolate_gradient(&stops, t);
        let color = multiply_alpha(color, opacity);

        if is_vertical {
            ops.push(RenderOp::FillRect {
                x, y: y + step as f32, width, height: 1.0, color,
            });
        } else {
            ops.push(RenderOp::FillRect {
                x: x + step as f32, y, width: 1.0, height, color,
            });
        }
    }
}

/// Split gradient arguments respecting nested parentheses.
fn split_gradient_args(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        parts.push(s[start..].trim());
    }
    parts
}

/// Interpolate between gradient color stops at position t (0.0–1.0).
fn interpolate_gradient(stops: &[(f32, Color)], t: f32) -> Color {
    if stops.is_empty() {
        return Color::TRANSPARENT;
    }
    if t <= stops[0].0 {
        return stops[0].1;
    }
    if t >= stops[stops.len() - 1].0 {
        return stops[stops.len() - 1].1;
    }
    // Find the two stops that bracket t.
    for i in 0..stops.len() - 1 {
        let (t0, c0) = stops[i];
        let (t1, c1) = stops[i + 1];
        if t >= t0 && t <= t1 {
            let range = t1 - t0;
            let frac = if range > 0.0 { (t - t0) / range } else { 0.0 };
            return Color::rgba(
                c0.r + (c1.r - c0.r) * frac,
                c0.g + (c1.g - c0.g) * frac,
                c0.b + (c1.b - c0.b) * frac,
                c0.a + (c1.a - c0.a) * frac,
            );
        }
    }
    stops.last().unwrap().1
}

// ---------------------------------------------------------------------------
// CSS filter support
// ---------------------------------------------------------------------------

/// Represents color transform effects from CSS `filter`.
struct FilterEffect {
    grayscale: bool,
    brightness: f32,
    invert: bool,
}

/// Extract CSS filter effects from style and return an opacity multiplier
/// and color transform.
fn extract_css_filter(style: &nova_mod_api::content::StyleMap) -> (f32, Option<FilterEffect>) {
    for (key, value) in &style.properties {
        if key == "filter" {
            let s = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            if s.trim() == "none" { return (1.0, None); }

            let mut opacity_mult = 1.0_f32;
            let mut grayscale = false;
            let mut brightness = 1.0_f32;
            let mut invert = false;

            // Parse filter functions
            let mut remaining = s;
            while let Some(paren) = remaining.find('(') {
                let func = remaining[..paren].trim();
                let func = func.rsplit_once(|c: char| c.is_whitespace()).map(|(_, n)| n).unwrap_or(func);
                let after = &remaining[paren + 1..];
                let Some(close) = after.find(')') else { break };
                let arg = &after[..close];
                remaining = &after[close + 1..];

                match func {
                    "opacity" => {
                        if let Some(v) = arg.trim().strip_suffix('%') {
                            opacity_mult *= v.trim().parse::<f32>().unwrap_or(100.0) / 100.0;
                        } else if let Ok(v) = arg.trim().parse::<f32>() {
                            opacity_mult *= v;
                        }
                    }
                    "brightness" => {
                        if let Some(v) = arg.trim().strip_suffix('%') {
                            brightness = v.trim().parse::<f32>().unwrap_or(100.0) / 100.0;
                        } else if let Ok(v) = arg.trim().parse::<f32>() {
                            brightness = v;
                        }
                    }
                    "grayscale" => {
                        if let Some(v) = arg.trim().strip_suffix('%') {
                            if v.trim().parse::<f32>().unwrap_or(0.0) > 50.0 { grayscale = true; }
                        } else if let Ok(v) = arg.trim().parse::<f32>() {
                            if v > 0.5 { grayscale = true; }
                        }
                    }
                    "invert" => {
                        if let Some(v) = arg.trim().strip_suffix('%') {
                            if v.trim().parse::<f32>().unwrap_or(0.0) > 50.0 { invert = true; }
                        } else if let Ok(v) = arg.trim().parse::<f32>() {
                            if v > 0.5 { invert = true; }
                        }
                    }
                    _ => {} // blur, drop-shadow, etc. — skip for now
                }
            }

            let effect = if grayscale || brightness != 1.0 || invert {
                Some(FilterEffect { grayscale, brightness, invert })
            } else {
                None
            };
            return (opacity_mult, effect);
        }
    }
    (1.0, None)
}

/// Apply a `FilterEffect` to a color (invert, brightness, grayscale).
fn apply_filter_to_color(color: Color, filter: &FilterEffect) -> Color {
    let mut r = color.r;
    let mut g = color.g;
    let mut b = color.b;

    if filter.invert {
        r = 1.0 - r;
        g = 1.0 - g;
        b = 1.0 - b;
    }

    r *= filter.brightness;
    g *= filter.brightness;
    b *= filter.brightness;

    if filter.grayscale {
        let gray = r * 0.299 + g * 0.587 + b * 0.114;
        r = gray;
        g = gray;
        b = gray;
    }

    Color::rgba(r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0), color.a)
}

// ---------------------------------------------------------------------------
// CSS clip-path support
// ---------------------------------------------------------------------------

/// Extract a `clip-path: inset(...)` value and return `(top, right, bottom, left)` insets in px.
fn extract_clip_path(style: &nova_mod_api::content::StyleMap) -> Option<(f32, f32, f32, f32)> {
    for (key, value) in &style.properties {
        if key == "clip-path" {
            let s = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            if let Some(inner) = s.strip_prefix("inset(").and_then(|s| s.strip_suffix(')')) {
                let parts: Vec<f32> = inner.split_whitespace()
                    .filter_map(|p| p.strip_suffix("px").and_then(|n| n.parse().ok())
                        .or_else(|| p.parse().ok()))
                    .collect();
                match parts.len() {
                    1 => return Some((parts[0], parts[0], parts[0], parts[0])),
                    2 => return Some((parts[0], parts[1], parts[0], parts[1])),
                    4 => return Some((parts[0], parts[1], parts[2], parts[3])),
                    _ => {}
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::content::{LayoutBox, LayoutContent, StyleMap};

    #[test]
    fn manifest_provides_paint() {
        let m = PainterMod::new();
        assert!(m.manifest().provides(&CapabilityType::Paint));
        assert!(m.manifest().requires_permission(&Permission::GpuRender));
    }

    #[test]
    fn paint_text_generates_draw_text() {
        let layout = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 19.2,
            content: LayoutContent::Text("Hello, NOVA!".into()),
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        };

        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());

        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawText { .. })),
            "should generate a DrawText op"
        );
    }

    #[test]
    fn zero_size_box_skipped() {
        let layout = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            content: LayoutContent::Block,
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        };

        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());
        assert!(ops.is_empty());
    }

    #[test]
    fn parse_hex_color_3_char() {
        let c = parse_hex_color("#abc").unwrap();
        assert!((c.r - 0xAA as f32 / 255.0).abs() < 0.01);
        assert!((c.g - 0xBB as f32 / 255.0).abs() < 0.01);
        assert!((c.b - 0xCC as f32 / 255.0).abs() < 0.01);
    }

    #[test]
    fn parse_hex_color_6_char() {
        let c = parse_hex_color("#ff6600").unwrap();
        assert!((c.r - 1.0).abs() < 0.01);
        assert!((c.g - 0.4).abs() < 0.01);
        assert!((c.b - 0.0).abs() < 0.01);
    }

    #[test]
    fn parse_hex_color_8_char_with_alpha() {
        let c = parse_hex_color("#ff660080").unwrap();
        assert!((c.r - 1.0).abs() < 0.01);
        assert!((c.a - 128.0 / 255.0).abs() < 0.01);
    }

    #[test]
    fn parse_bare_hex_without_hash() {
        // Bare hex strings (no '#' prefix) should be handled by parse_color_string.
        let c = parse_color_string("ff6600").unwrap();
        assert!((c.r - 1.0).abs() < 0.01);
        assert!((c.g - 0.4).abs() < 0.01);
    }

    #[test]
    fn parse_named_colors_extended() {
        assert!(parse_named_color("orange").is_some());
        assert!(parse_named_color("navy").is_some());
        assert!(parse_named_color("teal").is_some());
        assert!(parse_named_color("coral").is_some());
        assert!(parse_named_color("Purple").is_some());
    }

    #[test]
    fn style_value_keyword_color() {
        // Colors stored as Keyword should be resolved.
        let val = StyleValue::Keyword("blue".into());
        let c = style_value_to_color(&val).unwrap();
        assert!((c.b - 1.0).abs() < 0.01);
    }

    #[test]
    fn extract_text_color_from_style() {
        let mut style = StyleMap::default();
        style.properties.push(("color".into(), StyleValue::Keyword("#0000ee".into())));
        let c = extract_text_color(&style);
        assert!((c.b - 0xEE as f32 / 255.0).abs() < 0.01, "link blue should be extracted");
    }

    #[test]
    fn extract_bg_color_from_str() {
        let mut style = StyleMap::default();
        style.properties.push(("background-color".into(), StyleValue::Str("orange".into())));
        let c = extract_background_color(&style);
        assert!(c.a > 0.0, "orange background should not be transparent");
        assert!((c.r - 1.0).abs() < 0.01);
    }

    // --- Phase 5 tests ---

    #[test]
    fn border_radius_uniform_generates_fill_rounded_rect() {
        let mut style = StyleMap::default();
        style.properties.push(("background-color".into(), StyleValue::Str("blue".into())));
        style.properties.push(("border-radius".into(), StyleValue::Str("8px".into())));

        let layout = LayoutBox {
            x: 10.0, y: 10.0, width: 100.0, height: 50.0,
            content: LayoutContent::Block,
            style,
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::FillRoundedRect { .. })),
            "border-radius should emit FillRoundedRect"
        );
        assert!(
            !ops.iter().any(|op| matches!(op, RenderOp::FillRect { .. })),
            "should not also emit a plain FillRect for the background"
        );
    }

    #[test]
    fn no_border_radius_generates_fill_rect() {
        let mut style = StyleMap::default();
        style.properties.push(("background-color".into(), StyleValue::Str("red".into())));

        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 100.0, height: 50.0,
            content: LayoutContent::Block,
            style,
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());
        assert!(ops.iter().any(|op| matches!(op, RenderOp::FillRect { .. })));
        assert!(!ops.iter().any(|op| matches!(op, RenderOp::FillRoundedRect { .. })));
    }

    #[test]
    fn opacity_multiplies_alpha() {
        // opacity 0.5 on a fully-opaque red background → alpha should be 0.5.
        let mut style = StyleMap::default();
        style.properties.push(("background-color".into(), StyleValue::Str("red".into())));
        style.properties.push(("opacity".into(), StyleValue::Number(0.5)));

        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 100.0, height: 50.0,
            content: LayoutContent::Block,
            style,
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());
        for op in &ops {
            if let RenderOp::FillRect { color, .. } = op {
                assert!(
                    (color.a - 0.5).abs() < 0.01,
                    "opacity 0.5 should halve the background alpha, got {}",
                    color.a
                );
            }
        }
    }

    #[test]
    fn box_shadow_emitted_before_background() {
        let mut style = StyleMap::default();
        style.properties.push(("background-color".into(), StyleValue::Str("white".into())));
        style.properties.push((
            "box-shadow".into(),
            StyleValue::Str("4px 4px 8px rgba(0,0,0,0.5)".into()),
        ));

        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 100.0, height: 50.0,
            content: LayoutContent::Block,
            style,
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());

        // With blur > 0, box-shadow is now rendered as multiple FillRect ops
        // (concentric expanding rects) before the background FillRect.
        // The first FillRect should be a shadow rect (small alpha), and the
        // background FillRect (white, alpha=1) should come after all shadow rects.
        let first_fill = ops.iter().position(|op| matches!(op, RenderOp::FillRect { .. }));
        assert!(first_fill.is_some(), "should emit FillRect ops for shadow blur");
        // The background (white, alpha ~1.0) should exist somewhere after the shadow rects.
        let bg_pos = ops.iter().position(|op| {
            if let RenderOp::FillRect { color, .. } = op {
                color.a > 0.9 && color.r > 0.9 && color.g > 0.9 && color.b > 0.9
            } else {
                false
            }
        });
        assert!(bg_pos.is_some(), "should emit a FillRect for the background");
        assert!(first_fill.unwrap() < bg_pos.unwrap(), "shadow rects must come before background");
    }

    #[test]
    fn text_transform_uppercase() {
        let mut style = StyleMap::default();
        style.properties.push(("text-transform".into(), StyleValue::Keyword("uppercase".into())));

        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 200.0, height: 20.0,
            content: LayoutContent::Text("hello world".into()),
            style,
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());
        for op in &ops {
            if let RenderOp::DrawText { text, .. } = op {
                assert_eq!(text, "HELLO WORLD");
            }
        }
    }

    #[test]
    fn text_transform_lowercase() {
        let mut style = StyleMap::default();
        style.properties.push(("text-transform".into(), StyleValue::Keyword("lowercase".into())));

        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 200.0, height: 20.0,
            content: LayoutContent::Text("HELLO WORLD".into()),
            style,
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());
        for op in &ops {
            if let RenderOp::DrawText { text, .. } = op {
                assert_eq!(text, "hello world");
            }
        }
    }

    #[test]
    fn text_transform_capitalize() {
        let result = capitalize_words("hello world foo");
        assert_eq!(result, "Hello World Foo");
    }

    #[test]
    fn border_radius_four_values() {
        let mut style = StyleMap::default();
        style.properties.push(("border-radius".into(), StyleValue::Str("4px 8px 12px 16px".into())));
        let r = extract_border_radius(&style, 200.0, 100.0);
        assert!((r[0] - 4.0).abs() < 0.01);
        assert!((r[1] - 8.0).abs() < 0.01);
        assert!((r[2] - 12.0).abs() < 0.01);
        assert!((r[3] - 16.0).abs() < 0.01);
    }

    #[test]
    fn border_radius_percent() {
        // 50% on a 100x100 box → radius = 50% * min(100,100)/2 = 25px... wait.
        // The formula: pct * width.min(height) * 0.5
        // 50% of 100 * 0.5 = 25px for a 50x50 box.
        let mut style = StyleMap::default();
        style.properties.push(("border-radius".into(), StyleValue::Str("50%".into())));
        let r = extract_border_radius(&style, 100.0, 100.0);
        // 0.5 * 100 * 0.5 = 25.0
        assert!((r[0] - 25.0).abs() < 0.01, "got {}", r[0]);
    }

    #[test]
    fn box_shadow_parse_offsets() {
        let mut style = StyleMap::default();
        style.properties.push((
            "box-shadow".into(),
            StyleValue::Str("3px 6px black".into()),
        ));
        let result = extract_box_shadow(&style);
        assert!(result.is_some());
        let (color, ox, oy, blur) = result.unwrap();
        assert!((ox - 3.0).abs() < 0.01);
        assert!((oy - 6.0).abs() < 0.01);
        assert!((blur - 0.0).abs() < 0.01);
        // "black" → rgb(0,0,0)
        assert!(color.r < 0.01 && color.g < 0.01 && color.b < 0.01);
    }

    #[test]
    fn extract_font_family_custom() {
        let mut style = StyleMap::default();
        style.properties.push((
            "font-family".into(),
            StyleValue::Str("\"Roboto\", sans-serif".into()),
        ));
        let result = extract_font_family(&style);
        assert_eq!(result, Some("Roboto".to_string()));
    }

    #[test]
    fn extract_font_family_generic_returns_none() {
        let mut style = StyleMap::default();
        style.properties.push((
            "font-family".into(),
            StyleValue::Keyword("sans-serif".into()),
        ));
        let result = extract_font_family(&style);
        assert_eq!(result, None);
    }

    #[test]
    fn extract_font_family_none_when_absent() {
        let style = StyleMap::default();
        let result = extract_font_family(&style);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_hsl_color_in_painter() {
        let c = parse_color_string("hsl(0, 100%, 50%)").unwrap();
        assert!((c.r - 1.0).abs() < 0.01); // red
        assert!(c.g < 0.01);
        assert!(c.b < 0.01);
    }

    #[test]
    fn font_family_passed_in_draw_text_op() {
        let mut style = StyleMap::default();
        style.properties.push((
            "font-family".into(),
            StyleValue::Str("\"CustomFont\", Arial, sans-serif".into()),
        ));
        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 200.0, height: 20.0,
            content: LayoutContent::Text("test".into()),
            style,
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new());
        let has_family = ops.iter().any(|op| {
            if let RenderOp::DrawText { font_family, .. } = op {
                font_family.as_deref() == Some("CustomFont")
            } else {
                false
            }
        });
        assert!(has_family, "DrawText should carry font_family = Some(\"CustomFont\")");
    }

    #[test]
    fn background_repeat_no_repeat() {
        let mut style = StyleMap::default();
        style.properties.push(("background-repeat".into(), StyleValue::Keyword("no-repeat".into())));
        assert_eq!(extract_background_repeat(&style), "no-repeat");
    }

    #[test]
    fn background_position_center() {
        let mut style = StyleMap::default();
        style.properties.push(("background-position".into(), StyleValue::Keyword("center".into())));
        let (x, y) = extract_background_position(&style);
        assert!((x - 0.5).abs() < 0.01);
        assert!((y - 0.5).abs() < 0.01);
    }

    #[test]
    fn background_size_contain() {
        let mut style = StyleMap::default();
        style.properties.push(("background-size".into(), StyleValue::Keyword("contain".into())));
        let (w, h) = extract_background_size(&style, 200.0, 100.0, 400.0, 200.0);
        // contain: min(200/400, 100/200) = 0.5, so 200x100
        assert!((w - 200.0).abs() < 0.01);
        assert!((h - 100.0).abs() < 0.01);
    }

    #[test]
    fn background_size_cover() {
        let mut style = StyleMap::default();
        style.properties.push(("background-size".into(), StyleValue::Keyword("cover".into())));
        let (w, h) = extract_background_size(&style, 200.0, 100.0, 100.0, 100.0);
        // cover: max(200/100, 100/100) = 2.0, so 200x200
        assert!((w - 200.0).abs() < 0.01);
        assert!((h - 200.0).abs() < 0.01);
    }

    #[test]
    fn text_overflow_ellipsis_truncates() {
        let result = truncate_with_ellipsis("Hello, World! This is a very long text", 16.0, 100.0);
        assert!(result.ends_with("..."), "should end with ellipsis: {result}");
        assert!(result.len() < "Hello, World! This is a very long text".len());
    }

    #[test]
    fn text_overflow_short_text_unchanged() {
        let result = truncate_with_ellipsis("Hi", 16.0, 200.0);
        assert_eq!(result, "Hi");
    }

    // --- Stacking context tests ---

    #[test]
    fn stacking_context_detected_for_opacity() {
        let mut style = StyleMap::default();
        style.properties.push(("opacity".into(), StyleValue::Number(0.5)));
        assert!(creates_stacking_context(&style));
    }

    #[test]
    fn stacking_context_not_created_for_static_z_index() {
        let mut style = StyleMap::default();
        style.properties.push(("z-index".into(), StyleValue::Number(5.0)));
        // No position set (default is static) — should NOT create stacking context.
        assert!(!creates_stacking_context(&style));
    }

    #[test]
    fn stacking_context_for_positioned_z_index() {
        let mut style = StyleMap::default();
        style.properties.push(("z-index".into(), StyleValue::Number(5.0)));
        style.properties.push(("position".into(), StyleValue::Keyword("relative".into())));
        assert!(creates_stacking_context(&style));
    }
}

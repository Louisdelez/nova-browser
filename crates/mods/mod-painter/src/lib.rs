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
            ContentRequest::Paint { layout_tree, images, canvas_pixels } => {
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
                let canvas_map: HashMap<String, (u32, u32, Vec<u8>)> = canvas_pixels
                    .into_iter()
                    .map(|(id, w, h, px)| (id, (w, h, px)))
                    .collect();

                let mut ops = Vec::new();
                paint_box(&root, &mut ops, &images_map, &canvas_map);

                debug!(op_count = ops.len(), "painting complete");
                Ok(TypedData::RenderCommands(RenderCommands { ops, fonts: vec![], spa_push_url: None, spa_replace_url: None }))
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
// CSS 2D affine transform matrix
// ---------------------------------------------------------------------------

/// A 2D affine transform matrix stored as `[a, b, c, d, e, f]`:
/// ```text
/// | a c e |
/// | b d f |
/// | 0 0 1 |
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
struct AffineMatrix([f32; 6]);

impl AffineMatrix {
    /// Identity matrix.
    fn identity() -> Self {
        Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])
    }

    /// Whether this matrix is the identity (no transform needed).
    fn is_identity(&self) -> bool {
        let m = &self.0;
        (m[0] - 1.0).abs() < 1e-6
            && m[1].abs() < 1e-6
            && m[2].abs() < 1e-6
            && (m[3] - 1.0).abs() < 1e-6
            && m[4].abs() < 1e-6
            && m[5].abs() < 1e-6
    }

    /// Multiply (compose) two matrices: self * other.
    fn multiply(&self, other: &AffineMatrix) -> AffineMatrix {
        let a = &self.0;
        let b = &other.0;
        AffineMatrix([
            a[0] * b[0] + a[2] * b[1],
            a[1] * b[0] + a[3] * b[1],
            a[0] * b[2] + a[2] * b[3],
            a[1] * b[2] + a[3] * b[3],
            a[0] * b[4] + a[2] * b[5] + a[4],
            a[1] * b[4] + a[3] * b[5] + a[5],
        ])
    }

    /// Create a translation matrix.
    fn translate(tx: f32, ty: f32) -> Self {
        Self([1.0, 0.0, 0.0, 1.0, tx, ty])
    }

    /// Create a rotation matrix from an angle in degrees.
    fn rotate_deg(deg: f32) -> Self {
        let rad = deg.to_radians();
        let (s, c) = rad.sin_cos();
        Self([c, s, -s, c, 0.0, 0.0])
    }

    /// Create a scale matrix.
    fn scale(sx: f32, sy: f32) -> Self {
        Self([sx, 0.0, 0.0, sy, 0.0, 0.0])
    }

    /// Create a skew matrix from angles in degrees.
    fn skew_deg(ax: f32, ay: f32) -> Self {
        Self([1.0, ay.to_radians().tan(), ax.to_radians().tan(), 1.0, 0.0, 0.0])
    }

    /// Create a matrix from explicit values: `matrix(a, b, c, d, e, f)`.
    fn from_values(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> Self {
        Self([a, b, c, d, e, f])
    }

    /// Transform a point (x, y) by this matrix.
    fn transform_point(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.0;
        (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
    }
}

// ---------------------------------------------------------------------------
// CSS transform parsing
// ---------------------------------------------------------------------------

/// Parse a CSS angle value, returning degrees.
fn parse_angle_deg(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("deg") { v.trim().parse::<f32>().ok() }
    else if let Some(v) = s.strip_suffix("rad") { v.trim().parse::<f32>().ok().map(|r| r.to_degrees()) }
    else if let Some(v) = s.strip_suffix("turn") { v.trim().parse::<f32>().ok().map(|t| t * 360.0) }
    else if let Some(v) = s.strip_suffix("grad") { v.trim().parse::<f32>().ok().map(|g| g * 0.9) }
    else { s.parse::<f32>().ok() }
}

/// Parse a CSS `transform` property value into a combined 2D affine matrix.
///
/// Supports all 2D transform functions:
/// - `translate(x, y)`, `translateX(x)`, `translateY(y)`
/// - `rotate(angle)` (deg, rad, turn, grad units)
/// - `scale(x, y)`, `scaleX(x)`, `scaleY(y)`
/// - `skew(x, y)`, `skewX(x)`, `skewY(y)`
/// - `matrix(a, b, c, d, e, f)`
///
/// Multiple transform functions are composed left-to-right (standard CSS order).
fn parse_transform(value: &str) -> Option<AffineMatrix> {
    let value = value.trim();
    if value.is_empty() || value == "none" {
        return None;
    }
    let mut combined = AffineMatrix::identity();
    let mut found_any = false;
    let mut remaining = value;
    while let Some(paren_open) = remaining.find('(') {
        let func_name = remaining[..paren_open].trim();
        let func_name = func_name.rsplit_once(|c: char| c.is_whitespace()).map(|(_, n)| n).unwrap_or(func_name);
        let after_open = &remaining[paren_open + 1..];
        let Some(paren_close) = after_open.find(')') else { break };
        let args_str = &after_open[..paren_close];
        remaining = &after_open[paren_close + 1..];
        // Split args by comma or whitespace (some CSS uses spaces).
        let args: Vec<&str> = args_str.split(|c: char| c == ',' || c.is_whitespace())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        let mat = match func_name {
            "translate" => {
                let x = args.first().and_then(|a| parse_length_px(a)).unwrap_or(0.0);
                let y = args.get(1).and_then(|a| parse_length_px(a)).unwrap_or(0.0);
                found_any = true;
                AffineMatrix::translate(x, y)
            }
            "translateX" => {
                let x = args.first().and_then(|a| parse_length_px(a)).unwrap_or(0.0);
                found_any = true;
                AffineMatrix::translate(x, 0.0)
            }
            "translateY" => {
                let y = args.first().and_then(|a| parse_length_px(a)).unwrap_or(0.0);
                found_any = true;
                AffineMatrix::translate(0.0, y)
            }
            "rotate" => {
                let deg = args.first().and_then(|a| parse_angle_deg(a)).unwrap_or(0.0);
                found_any = true;
                AffineMatrix::rotate_deg(deg)
            }
            "scale" => {
                let sx = args.first().and_then(|a| a.parse::<f32>().ok()).unwrap_or(1.0);
                let sy = args.get(1).and_then(|a| a.parse::<f32>().ok()).unwrap_or(sx);
                found_any = true;
                AffineMatrix::scale(sx, sy)
            }
            "scaleX" => {
                let sx = args.first().and_then(|a| a.parse::<f32>().ok()).unwrap_or(1.0);
                found_any = true;
                AffineMatrix::scale(sx, 1.0)
            }
            "scaleY" => {
                let sy = args.first().and_then(|a| a.parse::<f32>().ok()).unwrap_or(1.0);
                found_any = true;
                AffineMatrix::scale(1.0, sy)
            }
            "skew" => {
                let ax = args.first().and_then(|a| parse_angle_deg(a)).unwrap_or(0.0);
                let ay = args.get(1).and_then(|a| parse_angle_deg(a)).unwrap_or(0.0);
                found_any = true;
                AffineMatrix::skew_deg(ax, ay)
            }
            "skewX" => {
                let ax = args.first().and_then(|a| parse_angle_deg(a)).unwrap_or(0.0);
                found_any = true;
                AffineMatrix::skew_deg(ax, 0.0)
            }
            "skewY" => {
                let ay = args.first().and_then(|a| parse_angle_deg(a)).unwrap_or(0.0);
                found_any = true;
                AffineMatrix::skew_deg(0.0, ay)
            }
            "matrix" => {
                if args.len() >= 6 {
                    let vals: Vec<f32> = args.iter().filter_map(|a| a.parse::<f32>().ok()).collect();
                    if vals.len() >= 6 {
                        found_any = true;
                        AffineMatrix::from_values(vals[0], vals[1], vals[2], vals[3], vals[4], vals[5])
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        combined = combined.multiply(&mat);
    }
    if found_any && !combined.is_identity() {
        Some(combined)
    } else {
        None
    }
}

/// Parse `transform-origin` from a style map.
///
/// Returns `(origin_x, origin_y)` as pixel offsets from the element's top-left corner.
/// Defaults to `(width/2, height/2)` (i.e., `50% 50%`).
fn parse_transform_origin(style: &nova_mod_api::content::StyleMap, width: f32, height: f32) -> (f32, f32) {
    for (key, value) in &style.properties {
        if key == "transform-origin" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                _ => continue,
            };
            let parts: Vec<&str> = s.split_whitespace().collect();
            let parse_origin_component = |token: &str, dimension: f32| -> f32 {
                let token = token.trim();
                match token {
                    "left" | "top" => 0.0,
                    "center" => dimension * 0.5,
                    "right" | "bottom" => dimension,
                    _ => {
                        if let Some(pct) = token.strip_suffix('%') {
                            pct.trim().parse::<f32>().unwrap_or(50.0) / 100.0 * dimension
                        } else {
                            parse_length_px(token).unwrap_or(dimension * 0.5)
                        }
                    }
                }
            };
            let ox = parts.first().map(|t| parse_origin_component(t, width)).unwrap_or(width * 0.5);
            let oy = parts.get(1).map(|t| parse_origin_component(t, height)).unwrap_or(height * 0.5);
            return (ox, oy);
        }
    }
    (width * 0.5, height * 0.5)
}

/// Extract the CSS `transform` property from a style map and compute the final
/// affine matrix including `transform-origin`.
fn extract_transform(style: &nova_mod_api::content::StyleMap, width: f32, height: f32) -> Option<[f32; 6]> {
    for (key, value) in &style.properties {
        if key == "transform" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                _ => continue,
            };
            if let Some(mat) = parse_transform(s) {
                // Apply transform-origin: translate to origin, apply transform, translate back.
                let (ox, oy) = parse_transform_origin(style, width, height);
                let pre = AffineMatrix::translate(ox, oy);
                let post = AffineMatrix::translate(-ox, -oy);
                let final_mat = post.multiply(&mat).multiply(&pre);
                return Some(final_mat.0);
            }
        }
    }
    None
}

/// Extract the CSS `position` value from a style map.
fn extract_nova_position(style: &nova_mod_api::content::StyleMap) -> Option<String> {
    for (key, value) in &style.properties {
        if key == "nova-position" {
            if let StyleValue::Keyword(k) | StyleValue::Str(k) = value {
                return Some(k.clone());
            }
        }
    }
    None
}

/// Extract a positional offset (`nova-top`, `nova-left`, etc.) from a style map.
fn extract_nova_offset(style: &nova_mod_api::content::StyleMap, prop: &str) -> Option<f32> {
    for (key, value) in &style.properties {
        if key == prop {
            match value {
                StyleValue::Px(px) => return Some(*px),
                StyleValue::Str(s) | StyleValue::Keyword(s) => {
                    if let Some(px) = parse_length_px(s) {
                        return Some(px);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Recursively paint a layout box into render operations.
fn paint_box(layout_box: &LayoutBox, ops: &mut Vec<RenderOp>, images: &HashMap<String, Vec<u8>>, canvas_map: &HashMap<String, (u32, u32, Vec<u8>)>) {
    // Skip zero-sized boxes (display: none, comments, etc.).
    if layout_box.width <= 0.0 && layout_box.height <= 0.0 {
        return;
    }

    // empty-cells: hide — skip painting empty table cells.
    // A cell is considered empty when it has no children or all children
    // are zero-sized text nodes.
    if is_empty_cells_hide(&layout_box.style) {
        let all_empty = layout_box.children.is_empty()
            || layout_box.children.iter().all(|child| {
                matches!(&child.content, LayoutContent::Text(t) if t.trim().is_empty())
                    || (child.width <= 0.0 && child.height <= 0.0)
            });
        if all_empty {
            return;
        }
    }

    // visibility: hidden — element takes up space but is invisible.
    // We still recurse into children because a child may have visibility: visible.
    let is_hidden = is_visibility_hidden(&layout_box.style);
    if is_hidden {
        // Still paint children (they may override visibility).
        let mut child_indices: Vec<usize> = (0..layout_box.children.len()).collect();
        child_indices.sort_by_key(|&i| layout_box.children[i].z_index);
        for i in child_indices {
            paint_box(&layout_box.children[i], ops, images, canvas_map);
        }
        return;
    }

    // Check for position: sticky and emit StickyStart/StickyEnd.
    let is_sticky = is_position_sticky(&layout_box.style);
    if is_sticky {
        let sticky_top = extract_sticky_top(&layout_box.style);
        ops.push(RenderOp::StickyStart {
            original_y: layout_box.y,
            sticky_top,
        });
    }

    // Check for position: fixed and emit FixedStart/FixedEnd.
    let nova_position = extract_nova_position(&layout_box.style);
    let is_fixed = nova_position.as_deref() == Some("fixed");
    if is_fixed {
        ops.push(RenderOp::FixedStart);
    }

    // Check for position: relative — apply offset via Save/Translate/Restore.
    let is_relative = nova_position.as_deref() == Some("relative");
    let relative_offset = if is_relative {
        let top = extract_nova_offset(&layout_box.style, "nova-top").unwrap_or(0.0);
        let left = extract_nova_offset(&layout_box.style, "nova-left").unwrap_or(0.0);
        let bottom = extract_nova_offset(&layout_box.style, "nova-bottom");
        let right = extract_nova_offset(&layout_box.style, "nova-right");
        // bottom/right are the opposite of top/left
        let dy = if top != 0.0 { top } else { bottom.map(|b| -b).unwrap_or(0.0) };
        let dx = if left != 0.0 { left } else { right.map(|r| -r).unwrap_or(0.0) };
        if dx != 0.0 || dy != 0.0 {
            Some((dx, dy))
        } else {
            None
        }
    } else {
        None
    };
    if let Some((dx, dy)) = relative_offset {
        ops.push(RenderOp::Save);
        ops.push(RenderOp::Translate { x: dx, y: dy });
    }

    // Check for CSS transform and wrap in Save/Transform/Restore if needed.
    let transform_matrix = extract_transform(&layout_box.style, layout_box.width, layout_box.height);
    let has_transform = transform_matrix.is_some();
    if has_transform {
        ops.push(RenderOp::Save);
        ops.push(RenderOp::Transform { matrix: transform_matrix.unwrap() });
    }

    // Determine opacity factor for this box (1.0 = fully opaque).
    let mut opacity = extract_opacity(&layout_box.style);

    // CSS filter: extract opacity() and drop-shadow() as approximations.
    let filter_shadow = extract_filter_effects(&layout_box.style, &mut opacity);

    let has_opacity = opacity < 1.0;
    if has_opacity {
        ops.push(RenderOp::PushOpacity { opacity });
    }

    // CSS clip-path: inset() — clip the element to inset bounds.
    let clip_inset = extract_clip_path_inset(&layout_box.style, layout_box.width, layout_box.height);
    if let Some((cx, cy, cw, ch)) = clip_inset {
        ops.push(RenderOp::PushClip {
            x: layout_box.x + cx,
            y: layout_box.y + cy,
            width: cw,
            height: ch,
        });
    }

    // Emit filter drop-shadow if present (before box-shadow).
    if let Some((shadow_color, offset_x, offset_y, blur)) = &filter_shadow {
        let shadow_color = multiply_alpha(*shadow_color, opacity);
        ops.push(RenderOp::BoxShadow {
            x: layout_box.x,
            y: layout_box.y,
            width: layout_box.width,
            height: layout_box.height,
            color: shadow_color,
            offset_x: *offset_x,
            offset_y: *offset_y,
            blur: *blur,
        });
    }

    // Emit box-shadow before the background (painter's order: shadow → bg → content).
    if let Some((shadow_color, offset_x, offset_y, blur)) = extract_box_shadow(&layout_box.style) {
        let shadow_color = multiply_alpha(shadow_color, opacity);
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

    // <dialog> — draw a semi-transparent backdrop behind the dialog box.
    if matches!(layout_box.content, LayoutContent::Dialog) {
        ops.push(RenderOp::FillRect {
            x: 0.0,
            y: 0.0,
            width: 100_000.0, // large enough to cover any viewport
            height: 100_000.0,
            color: Color { r: 0.0, g: 0.0, b: 0.0, a: 0.3 },
        });
    }

    // Paint the background if this is not a text node.
    let bg_color = multiply_alpha(extract_background_color(&layout_box.style), opacity);
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

    // Paint background-image (url(), linear-gradient, radial-gradient, conic-gradient).
    if let Some(bg_image) = extract_background_image_value(&layout_box.style) {
        if bg_image.starts_with("url(") || (bg_image.contains("url(") && !bg_image.contains("gradient(")) {
            // Background image from URL.
            if let Some(url) = extract_css_url(&bg_image) {
                if let Some(decoded) = images.get(&url) {
                    if decoded.len() >= 8 {
                        let iw = u32::from_le_bytes([decoded[0], decoded[1], decoded[2], decoded[3]]);
                        let ih = u32::from_le_bytes([decoded[4], decoded[5], decoded[6], decoded[7]]);
                        let px = &decoded[8..];

                        let bg_size = extract_background_size(&layout_box.style);
                        let bg_pos = extract_background_position(&layout_box.style);
                        let bg_repeat = extract_background_repeat(&layout_box.style);

                        let (draw_w, draw_h) = compute_background_size(
                            &bg_size, layout_box.width, layout_box.height,
                            iw as f32, ih as f32,
                        );
                        let (off_x, off_y) = compute_background_position(
                            &bg_pos, layout_box.width, layout_box.height, draw_w, draw_h,
                        );

                        emit_background_image_ops(
                            ops, layout_box.x, layout_box.y,
                            layout_box.width, layout_box.height,
                            off_x, off_y, draw_w, draw_h,
                            iw, ih, px,
                            &bg_repeat,
                        );
                    }
                }
            }
        } else if bg_image.contains("radial-gradient(") {
            paint_radial_gradient(
                &bg_image, layout_box.x, layout_box.y,
                layout_box.width, layout_box.height, opacity, ops,
            );
        } else if bg_image.contains("conic-gradient(") {
            paint_conic_gradient(
                &bg_image, layout_box.x, layout_box.y,
                layout_box.width, layout_box.height, opacity, ops,
            );
        } else if bg_image.contains("linear-gradient(") {
            // Render linear gradient as strips.
            paint_linear_gradient(
                &bg_image, layout_box.x, layout_box.y,
                layout_box.width, layout_box.height, opacity, ops,
            );
        }
    }

    // Paint text content.
    if let LayoutContent::Text(ref text) = layout_box.content {
        // Skip single-space text nodes — the layout engine already handles
        // spacing via node positioning. Drawing the space character on top
        // of the positional gap would create double spacing.
        if !text.trim().is_empty() && text != " " {
            let font_size = extract_font_size(&layout_box.style);
            let text_color = multiply_alpha(extract_text_color(&layout_box.style), opacity);
            let font_weight = extract_font_weight(&layout_box.style);
            let font_style = extract_font_style(&layout_box.style);
            let font_family = extract_font_family(&layout_box.style);
            let letter_spacing = extract_letter_spacing(&layout_box.style);
            let transformed_text = apply_text_transform(text, &layout_box.style);

            // Apply text-overflow: ellipsis when the text overflows a
            // clipped container with white-space: nowrap. Approximate
            // character width using 0.6 * font_size (the renderer will
            // clip the rest via PushClip, but the ellipsis gives the user
            // a visual cue that content is truncated).
            let final_text = if should_ellipsize(&layout_box.style) {
                let approx_char_width = font_size * 0.6;
                let max_chars = (layout_box.width / approx_char_width).floor() as usize;
                if max_chars > 0 && transformed_text.chars().count() > max_chars {
                    let truncated: String = transformed_text.chars().take(max_chars.saturating_sub(1)).collect();
                    format!("{truncated}\u{2026}") // ellipsis character
                } else {
                    transformed_text.clone()
                }
            } else {
                transformed_text.clone()
            };

            // Emit text-shadow before the main text (painter's order: shadow → text).
            if let Some((sx, sy, blur, shadow_color)) = extract_text_shadow(&layout_box.style) {
                let shadow_color = multiply_alpha(shadow_color, opacity);
                ops.push(RenderOp::DrawText {
                    x: layout_box.x + sx,
                    y: layout_box.y + font_size + sy,
                    text: final_text.clone(),
                    font_size,
                    color: shadow_color,
                    font_weight,
                    font_style: font_style.clone(),
                    font_family: font_family.clone(),
                    letter_spacing,
                });
            }

            ops.push(RenderOp::DrawText {
                x: layout_box.x,
                y: layout_box.y + font_size, // baseline offset
                text: final_text.clone(),
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
                let is_dotted = has_text_decoration_dotted(&layout_box.style);
                if is_dotted {
                    // Draw dotted underline: alternating 2px on, 2px off.
                    let mut dx = 0.0;
                    while dx < layout_box.width {
                        let seg_w = (2.0_f32).min(layout_box.width - dx);
                        ops.push(RenderOp::FillRect {
                            x: layout_box.x + dx,
                            y: underline_y,
                            width: seg_w,
                            height: 1.0,
                            color: text_color,
                        });
                        dx += 4.0; // 2px drawn + 2px gap
                    }
                } else {
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
    if let LayoutContent::Image { ref src, ref alt } = layout_box.content {
        if let Some(decoded) = images.get(src) {
            if decoded.len() >= 8 {
                let img_width = u32::from_le_bytes([decoded[0], decoded[1], decoded[2], decoded[3]]);
                let img_height = u32::from_le_bytes([decoded[4], decoded[5], decoded[6], decoded[7]]);
                let pixels = decoded[8..].to_vec();

                // Apply object-fit to determine draw dimensions.
                let (draw_x, draw_y, draw_w, draw_h, needs_clip) = compute_object_fit(
                    &layout_box.style,
                    layout_box.x, layout_box.y,
                    layout_box.width, layout_box.height,
                    img_width as f32, img_height as f32,
                );

                // Check for border-radius on images — clip with rounded rect.
                let img_radius = extract_border_radius(&layout_box.style, layout_box.width, layout_box.height);
                let has_img_radius = img_radius.iter().any(|&r| r > 0.0);
                if has_img_radius {
                    ops.push(RenderOp::PushRoundedClip {
                        x: layout_box.x, y: layout_box.y,
                        width: layout_box.width, height: layout_box.height,
                        radius: img_radius,
                    });
                } else if needs_clip {
                    ops.push(RenderOp::PushClip {
                        x: layout_box.x, y: layout_box.y,
                        width: layout_box.width, height: layout_box.height,
                    });
                }

                ops.push(RenderOp::DrawImage {
                    x: draw_x, y: draw_y,
                    width: draw_w, height: draw_h,
                    img_width, img_height, pixels,
                });

                if has_img_radius || needs_clip {
                    ops.push(RenderOp::PopClip);
                }
            }
        } else {
            // Show alt text if available, otherwise show filename as fallback.
            let label = if let Some(alt_text) = alt {
                if !alt_text.is_empty() {
                    alt_text.clone()
                } else {
                    // Empty alt means decorative image — show minimal placeholder.
                    String::new()
                }
            } else {
                let fname = src.rsplit('/').next().unwrap_or(src);
                let fname = if fname.len() > 40 { format!("{}...", &fname[..37]) } else { fname.to_string() };
                format!("[img: {fname}]")
            };

            if !label.is_empty() {
                ops.push(RenderOp::FillRect {
                    x: layout_box.x, y: layout_box.y,
                    width: layout_box.width.max(150.0), height: layout_box.height.max(80.0),
                    color: Color::rgb(0.85, 0.85, 0.85),
                });
                ops.push(RenderOp::DrawText {
                    x: layout_box.x + 4.0, y: layout_box.y + 16.0,
                    text: label, font_size: 12.0,
                    color: Color::rgb(0.4, 0.4, 0.4),
                    font_weight: None,
                    font_style: None,
                    font_family: None, letter_spacing: None,
                });
            }
        }
    }

    // Paint inline SVG content.
    if let LayoutContent::InlineSvg { ref markup } = layout_box.content {
        // Check if we already rasterized this SVG (keyed by first 64 chars as a proxy).
        let svg_key = format!("__inline_svg_{}", &markup[..markup.len().min(64)]);
        if let Some(decoded) = images.get(&svg_key) {
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
            // Try rasterizing inline via resvg directly if available.
            // The pipeline will handle pre-rasterization and pass via images map.
            // As a fallback, emit a placeholder.
            ops.push(RenderOp::FillRect {
                x: layout_box.x, y: layout_box.y,
                width: layout_box.width, height: layout_box.height,
                color: Color::rgb(0.95, 0.95, 0.95),
            });
            ops.push(RenderOp::DrawText {
                x: layout_box.x + 4.0, y: layout_box.y + 16.0,
                text: "[inline SVG]".to_string(),
                font_size: 12.0,
                color: Color::rgb(0.4, 0.4, 0.4),
                font_weight: None, font_style: None, font_family: None, letter_spacing: None,
            });
        }
    }

    // Paint iframe content (placeholder with inset border).
    if let LayoutContent::Iframe { ref src, ref srcdoc } = layout_box.content {
        // White content area.
        ops.push(RenderOp::FillRect {
            x: layout_box.x + 2.0, y: layout_box.y + 2.0,
            width: layout_box.width - 4.0, height: layout_box.height - 4.0,
            color: Color::WHITE,
        });
        // 2px inset border: top/left darker, bottom/right lighter (like Chrome).
        let dark = Color::rgb(0.55, 0.55, 0.55);
        let light = Color::rgb(0.83, 0.83, 0.83);
        // Outer border.
        ops.push(RenderOp::FillRect {
            x: layout_box.x, y: layout_box.y,
            width: layout_box.width, height: 1.0, color: dark,
        });
        ops.push(RenderOp::FillRect {
            x: layout_box.x, y: layout_box.y,
            width: 1.0, height: layout_box.height, color: dark,
        });
        ops.push(RenderOp::FillRect {
            x: layout_box.x, y: layout_box.y + layout_box.height - 1.0,
            width: layout_box.width, height: 1.0, color: light,
        });
        ops.push(RenderOp::FillRect {
            x: layout_box.x + layout_box.width - 1.0, y: layout_box.y,
            width: 1.0, height: layout_box.height, color: light,
        });
        // Inner border.
        ops.push(RenderOp::FillRect {
            x: layout_box.x + 1.0, y: layout_box.y + 1.0,
            width: layout_box.width - 2.0, height: 1.0, color: Color::rgb(0.65, 0.65, 0.65),
        });
        ops.push(RenderOp::FillRect {
            x: layout_box.x + 1.0, y: layout_box.y + 1.0,
            width: 1.0, height: layout_box.height - 2.0, color: Color::rgb(0.65, 0.65, 0.65),
        });
        ops.push(RenderOp::FillRect {
            x: layout_box.x + 1.0, y: layout_box.y + layout_box.height - 2.0,
            width: layout_box.width - 2.0, height: 1.0, color: Color::rgb(0.90, 0.90, 0.90),
        });
        ops.push(RenderOp::FillRect {
            x: layout_box.x + layout_box.width - 2.0, y: layout_box.y + 1.0,
            width: 1.0, height: layout_box.height - 2.0, color: Color::rgb(0.90, 0.90, 0.90),
        });
        let label = if srcdoc.is_some() {
            "[iframe: srcdoc]".to_string()
        } else if !src.is_empty() {
            let display_src = if src.len() > 50 { format!("{}...", &src[..47]) } else { src.clone() };
            format!("[iframe: {display_src}]")
        } else {
            "[iframe]".to_string()
        };
        ops.push(RenderOp::DrawText {
            x: layout_box.x + 4.0, y: layout_box.y + 14.0,
            text: label, font_size: 11.0,
            color: Color::rgb(0.4, 0.4, 0.4),
            font_weight: None, font_style: Some("italic".to_string()), font_family: None, letter_spacing: None,
        });
    }

    // Paint <video> content.
    if let LayoutContent::Video { ref src, ref poster, ref controls } = layout_box.content {
        paint_video(
            layout_box.x, layout_box.y,
            layout_box.width, layout_box.height,
            src, poster.as_deref(), *controls,
            images, opacity, ops,
        );
    }

    // Paint <audio> content.
    if let LayoutContent::Audio { ref src, ref controls } = layout_box.content {
        paint_audio(
            layout_box.x, layout_box.y,
            layout_box.width, layout_box.height,
            src, *controls,
            opacity, ops,
        );
    }

    // ── Canvas rendering ─────────────────────────────────────────────────
    if let LayoutContent::Canvas { ref canvas_id, canvas_width, canvas_height } = layout_box.content {
        if let Some((w, h, pixels)) = canvas_map.get(canvas_id) {
            ops.push(RenderOp::DrawImage {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width,
                height: layout_box.height,
                img_width: *w,
                img_height: *h,
                pixels: pixels.clone(),
            });
        } else {
            // No pixel data yet — draw a placeholder rectangle.
            ops.push(RenderOp::FillRect {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width,
                height: layout_box.height,
                color: Color { r: 0.94, g: 0.94, b: 0.94, a: 1.0 },
            });
        }
    }

    // Paint <progress> and <meter> widgets.
    if let Some(widget_type) = extract_widget_type(&layout_box.style) {
        paint_widget(layout_box, &widget_type, ops);
    }

    // Emit FormField op for interactive form elements.
    if let Some(info) = extract_form_field(&layout_box.style) {
        // Emit visual rendering based on field type.
        paint_form_field_visual(layout_box, &info, ops);

        ops.push(RenderOp::FormField {
            x: layout_box.x,
            y: layout_box.y,
            width: layout_box.width,
            height: layout_box.height,
            value: info.value,
            field_type: info.field_type,
            name: info.name,
            form_action: info.form_action,
            form_method: info.form_method,
            form_enctype: info.form_enctype,
            placeholder: info.placeholder,
            checked: info.checked,
            required: info.required,
            options: info.options,
            pattern: info.pattern,
            min: info.min,
            max: info.max,
            maxlength: info.maxlength,
            minlength: info.minlength,
            autofocus: info.autofocus,
            tabindex: info.tabindex,
            title: info.title.clone(),
            pointer_events_none: info.pointer_events_none,
        });
    }

    // Paint CSS borders — supports per-side or shorthand borders.
    // Supports solid, dashed, and dotted border styles.
    let borders = extract_borders_per_side(&layout_box.style);
    let bx = layout_box.x;
    let by = layout_box.y;
    let bw = layout_box.width;
    let bh = layout_box.height;
    // Top border
    if let Some((w, color, style)) = borders.top {
        let color = multiply_alpha(color, opacity);
        emit_border_side(ops, bx, by, bw, w, true, color, style);
    }
    // Bottom border
    if let Some((w, color, style)) = borders.bottom {
        let color = multiply_alpha(color, opacity);
        emit_border_side(ops, bx, by + bh - w, bw, w, true, color, style);
    }
    // Left border
    if let Some((w, color, style)) = borders.left {
        let color = multiply_alpha(color, opacity);
        emit_border_side(ops, bx, by, w, bh, false, color, style);
    }
    // Right border
    if let Some((w, color, style)) = borders.right {
        let color = multiply_alpha(color, opacity);
        emit_border_side(ops, bx + bw - w, by, w, bh, false, color, style);
    }

    // Paint CSS outline — similar to border but outside the border box, no layout impact.
    if let Some((outline_w, outline_color)) = extract_outline(&layout_box.style) {
        let outline_color = multiply_alpha(outline_color, opacity);
        let ox = layout_box.x - outline_w;
        let oy = layout_box.y - outline_w;
        let ow = layout_box.width + outline_w * 2.0;
        let oh = layout_box.height + outline_w * 2.0;
        // Top
        ops.push(RenderOp::FillRect { x: ox, y: oy, width: ow, height: outline_w, color: outline_color });
        // Bottom
        ops.push(RenderOp::FillRect { x: ox, y: oy + oh - outline_w, width: ow, height: outline_w, color: outline_color });
        // Left
        ops.push(RenderOp::FillRect { x: ox, y: oy, width: outline_w, height: oh, color: outline_color });
        // Right
        ops.push(RenderOp::FillRect { x: ox + ow - outline_w, y: oy, width: outline_w, height: oh, color: outline_color });
    }

    // Paint list-style-type markers for list items.
    paint_list_marker(layout_box, ops, opacity);

    // Emit a Link op if this box has an href (i.e., it is an <a> element).
    if let Some(href) = extract_href(&layout_box.style) {
        if layout_box.width > 0.0 && layout_box.height > 0.0 {
            let link_title = extract_style_str(&layout_box.style, "nova-title");
            let link_target = extract_style_str(&layout_box.style, "nova-target");
            ops.push(RenderOp::Link {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width,
                height: layout_box.height,
                url: href,
                title: link_title,
                target: link_target,
            });
        }
    }

    // Emit an Anchor op for elements with an id attribute (for anchor scrolling).
    let element_id = extract_style_str(&layout_box.style, "nova-element-id");
    if !element_id.is_empty() {
        ops.push(RenderOp::Anchor {
            id: element_id,
            y: layout_box.y,
        });
    }

    // Check whether this box clips its overflow.
    let clips_overflow = has_overflow_clip(&layout_box.style);
    if clips_overflow {
        ops.push(RenderOp::PushClip {
            x: layout_box.x,
            y: layout_box.y,
            width: layout_box.width,
            height: layout_box.height,
        });
    }

    // Recurse into children, sorted by z_index (stable sort preserves DOM order
    // for equal z-index values, which matches CSS stacking context semantics).
    let mut child_indices: Vec<usize> = (0..layout_box.children.len()).collect();
    child_indices.sort_by_key(|&i| layout_box.children[i].z_index);
    for i in child_indices {
        paint_box(&layout_box.children[i], ops, images, canvas_map);
    }

    if clips_overflow {
        ops.push(RenderOp::PopClip);
    }

    // Pop clip-path: inset() if it was applied.
    if clip_inset.is_some() {
        ops.push(RenderOp::PopClip);
    }

    if has_opacity {
        ops.push(RenderOp::PopOpacity);
    }

    if is_sticky {
        ops.push(RenderOp::StickyEnd);
    }

    if relative_offset.is_some() {
        ops.push(RenderOp::Restore);
    }

    if is_fixed {
        ops.push(RenderOp::FixedEnd);
    }

    if has_transform {
        ops.push(RenderOp::Restore);
    }
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

/// Check if `visibility: hidden` is set on an element.
///
/// Returns `true` when the element should be invisible but still occupy space.
fn is_visibility_hidden(style: &nova_mod_api::content::StyleMap) -> bool {
    for (key, value) in &style.properties {
        if key == "visibility" {
            let val_str = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            return val_str == "hidden" || val_str == "collapse";
        }
    }
    false
}

/// Check if `empty-cells: hide` is set on a style.
///
/// Used for table cells: when set, cells with no visible content are not painted.
fn is_empty_cells_hide(style: &nova_mod_api::content::StyleMap) -> bool {
    for (key, value) in &style.properties {
        if key == "empty-cells" {
            let val_str = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            return val_str == "hide";
        }
    }
    false
}

/// Extract filter effects from the CSS `filter` property.
///
/// Since we cannot do real image processing in the software renderer, this
/// extracts only the effects we can approximate:
/// - `opacity(n)` -- modifies the opacity parameter (passed by mutable ref)
/// - `drop-shadow(x y blur color)` -- returns as a BoxShadow tuple
///
/// `blur()`, `brightness()`, `grayscale()`, etc. are parsed but skipped.
fn extract_filter_effects(
    style: &nova_mod_api::content::StyleMap,
    opacity: &mut f32,
) -> Option<(Color, f32, f32, f32)> {
    let mut shadow = None;
    for (key, value) in &style.properties {
        if key == "filter" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                _ => continue,
            };
            if s == "none" {
                return None;
            }
            // Parse individual filter functions.
            let mut remaining = s;
            while let Some(paren_start) = remaining.find('(') {
                let func_name = remaining[..paren_start].trim();
                // Extract the name (last whitespace-separated token before paren).
                let func_name = func_name.rsplit_once(|c: char| c.is_whitespace())
                    .map(|(_, name)| name)
                    .unwrap_or(func_name);
                if let Some(paren_end) = remaining[paren_start..].find(')') {
                    let args = &remaining[paren_start + 1..paren_start + paren_end];
                    match func_name {
                        "opacity" => {
                            if let Some(val) = parse_filter_number(args) {
                                *opacity *= val.clamp(0.0, 1.0);
                            }
                        }
                        "drop-shadow" => {
                            shadow = parse_drop_shadow(args);
                        }
                        // blur, brightness, grayscale, etc. -- skip (too expensive for CPU).
                        _ => {}
                    }
                    remaining = &remaining[paren_start + paren_end + 1..];
                } else {
                    break;
                }
            }
        }
    }
    shadow
}

/// Parse a filter number value (e.g. "0.8", "80%").
fn parse_filter_number(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f32>().ok().map(|v| v / 100.0)
    } else {
        s.parse::<f32>().ok()
    }
}

/// Parse a `drop-shadow(x y blur color)` argument string.
fn parse_drop_shadow(args: &str) -> Option<(Color, f32, f32, f32)> {
    let tokens: Vec<&str> = args.trim().split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }
    let offset_x = parse_length_px(tokens[0])?;
    let offset_y = parse_length_px(tokens[1])?;
    let mut blur = 0.0_f32;
    let mut color = Color::rgba(0.0, 0.0, 0.0, 0.5);
    let mut color_start = 2;
    if tokens.len() > 2 {
        if let Some(b) = parse_length_px(tokens[2]) {
            blur = b;
            color_start = 3;
        }
    }
    if color_start < tokens.len() {
        let color_str = tokens[color_start..].join(" ");
        if let Some(c) = parse_color_string(&color_str) {
            color = c;
        }
    }
    Some((color, offset_x, offset_y, blur))
}

/// Extract `clip-path: inset(...)` values.
///
/// Returns `Some((x_offset, y_offset, clipped_width, clipped_height))` relative
/// to the element's position. Only `inset()` is supported.
fn extract_clip_path_inset(
    style: &nova_mod_api::content::StyleMap,
    box_width: f32,
    box_height: f32,
) -> Option<(f32, f32, f32, f32)> {
    for (key, value) in &style.properties {
        if key == "clip-path" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                _ => continue,
            };
            let s = s.trim();
            if s == "none" {
                return None;
            }
            if let Some(inset_args) = s.strip_prefix("inset(").and_then(|s| s.strip_suffix(')')) {
                let parts: Vec<&str> = inset_args.split_whitespace().collect();
                let (top, right, bottom, left) = match parts.len() {
                    1 => {
                        let v = parse_length_px(parts[0]).unwrap_or(0.0);
                        (v, v, v, v)
                    }
                    2 => {
                        let tb = parse_length_px(parts[0]).unwrap_or(0.0);
                        let lr = parse_length_px(parts[1]).unwrap_or(0.0);
                        (tb, lr, tb, lr)
                    }
                    3 => {
                        let t = parse_length_px(parts[0]).unwrap_or(0.0);
                        let lr = parse_length_px(parts[1]).unwrap_or(0.0);
                        let b = parse_length_px(parts[2]).unwrap_or(0.0);
                        (t, lr, b, lr)
                    }
                    _ => {
                        let t = parse_length_px(parts[0]).unwrap_or(0.0);
                        let r = parse_length_px(parts[1]).unwrap_or(0.0);
                        let b = parse_length_px(parts[2]).unwrap_or(0.0);
                        let l = parse_length_px(parts[3]).unwrap_or(0.0);
                        (t, r, b, l)
                    }
                };
                let clip_x = left;
                let clip_y = top;
                let clip_w = (box_width - left - right).max(0.0);
                let clip_h = (box_height - top - bottom).max(0.0);
                return Some((clip_x, clip_y, clip_w, clip_h));
            }
        }
    }
    None
}

/// Compute draw position and size for an image based on the CSS `object-fit` property.
///
/// Returns `(draw_x, draw_y, draw_width, draw_height, needs_clip)`.
/// `needs_clip` is true for `cover` mode where the image overflows the container.
fn compute_object_fit(
    style: &nova_mod_api::content::StyleMap,
    box_x: f32,
    box_y: f32,
    box_w: f32,
    box_h: f32,
    img_w: f32,
    img_h: f32,
) -> (f32, f32, f32, f32, bool) {
    let fit = style.properties.iter()
        .find(|(k, _)| k == "object-fit")
        .and_then(|(_, v)| match v {
            StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.as_str()),
            _ => None,
        })
        .unwrap_or("fill");

    match fit {
        "contain" => {
            // Scale to fit within the box, preserving aspect ratio (letterbox).
            if img_w <= 0.0 || img_h <= 0.0 {
                return (box_x, box_y, box_w, box_h, false);
            }
            let scale = (box_w / img_w).min(box_h / img_h);
            let draw_w = img_w * scale;
            let draw_h = img_h * scale;
            let draw_x = box_x + (box_w - draw_w) / 2.0;
            let draw_y = box_y + (box_h - draw_h) / 2.0;
            (draw_x, draw_y, draw_w, draw_h, false)
        }
        "cover" => {
            // Scale to fill the box, preserving aspect ratio (clip overflow).
            if img_w <= 0.0 || img_h <= 0.0 {
                return (box_x, box_y, box_w, box_h, false);
            }
            let scale = (box_w / img_w).max(box_h / img_h);
            let draw_w = img_w * scale;
            let draw_h = img_h * scale;
            let draw_x = box_x + (box_w - draw_w) / 2.0;
            let draw_y = box_y + (box_h - draw_h) / 2.0;
            (draw_x, draw_y, draw_w, draw_h, true)
        }
        "none" => {
            // No scaling — draw at natural size, centered.
            let draw_x = box_x + (box_w - img_w) / 2.0;
            let draw_y = box_y + (box_h - img_h) / 2.0;
            (draw_x, draw_y, img_w, img_h, true)
        }
        "scale-down" => {
            // Like contain, but never scale up.
            if img_w <= 0.0 || img_h <= 0.0 {
                return (box_x, box_y, box_w, box_h, false);
            }
            let scale = (box_w / img_w).min(box_h / img_h).min(1.0);
            let draw_w = img_w * scale;
            let draw_h = img_h * scale;
            let draw_x = box_x + (box_w - draw_w) / 2.0;
            let draw_y = box_y + (box_h - draw_h) / 2.0;
            (draw_x, draw_y, draw_w, draw_h, false)
        }
        // "fill" is default — stretch to fill.
        _ => (box_x, box_y, box_w, box_h, false),
    }
}

/// Extract the CSS `outline` property (outline-width, outline-style, outline-color).
///
/// Returns `Some((width_px, color))` if a visible outline is specified.
fn extract_outline(style: &nova_mod_api::content::StyleMap) -> Option<(f32, Color)> {
    let mut width = None;
    let mut color = None;
    let mut style_val = None;

    for (key, value) in &style.properties {
        match key.as_str() {
            "outline-width" => {
                let s = match value {
                    StyleValue::Px(px) => { width = Some(*px); continue; }
                    StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                    _ => continue,
                };
                width = match s.trim() {
                    "thin" => Some(1.0),
                    "medium" => Some(3.0),
                    "thick" => Some(5.0),
                    _ => parse_length_px(s),
                };
            }
            "outline-style" => {
                let s = match value {
                    StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                    _ => continue,
                };
                style_val = Some(s.to_string());
            }
            "outline-color" => {
                if let Some(c) = style_value_to_color(value) {
                    color = Some(c);
                }
            }
            _ => {}
        }
    }

    // Only paint if there's a visible style.
    let s = style_val.as_deref().unwrap_or("none");
    if s == "none" || s == "hidden" {
        return None;
    }
    let w = width.unwrap_or(3.0); // "medium" default
    if w <= 0.0 {
        return None;
    }
    let c = color.unwrap_or(Color::BLACK);
    Some((w, c))
}

/// Paint list-style-type markers (bullets or numbers) for list items.
///
/// Looks for `list-style-type` and `nova-list-index` properties in the style
/// map and prepends the appropriate marker character before the content.
fn paint_list_marker(layout_box: &LayoutBox, ops: &mut Vec<RenderOp>, opacity: f32) {
    let mut list_style = None;
    let mut list_index = None;

    for (key, value) in &layout_box.style.properties {
        match key.as_str() {
            "list-style-type" => {
                list_style = match value {
                    StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.clone()),
                    _ => None,
                };
            }
            "nova-list-index" => {
                list_index = match value {
                    StyleValue::Number(n) => Some(*n as i32),
                    StyleValue::Px(n) => Some(*n as i32),
                    StyleValue::Keyword(k) | StyleValue::Str(k) => k.parse::<i32>().ok(),
                    _ => None,
                };
            }
            _ => {}
        }
    }

    let style_type = match list_style.as_deref() {
        Some(s) if s != "none" => s,
        _ => return,
    };

    let font_size = extract_font_size(&layout_box.style);
    let text_color = multiply_alpha(extract_text_color(&layout_box.style), opacity);
    let marker_x = layout_box.x - 20.0; // Position marker in the padding-left area.

    let marker_text = match style_type {
        "disc" => "\u{2022}".to_string(),       // bullet
        "circle" => "\u{25E6}".to_string(),      // white bullet
        "square" => "\u{25AA}".to_string(),      // black small square
        "decimal" => {
            let idx = list_index.unwrap_or(1);
            format!("{idx}.")
        }
        "lower-alpha" | "lower-latin" => {
            let idx = list_index.unwrap_or(1).max(1) as u32;
            let ch = char::from_u32('a' as u32 + (idx - 1) % 26).unwrap_or('a');
            format!("{ch}.")
        }
        "upper-alpha" | "upper-latin" => {
            let idx = list_index.unwrap_or(1).max(1) as u32;
            let ch = char::from_u32('A' as u32 + (idx - 1) % 26).unwrap_or('A');
            format!("{ch}.")
        }
        "lower-roman" => {
            let idx = list_index.unwrap_or(1).max(1);
            format!("{}.", to_roman_lower(idx))
        }
        "upper-roman" => {
            let idx = list_index.unwrap_or(1).max(1);
            format!("{}.", to_roman_lower(idx).to_uppercase())
        }
        _ => return,
    };

    ops.push(RenderOp::DrawText {
        x: marker_x,
        y: layout_box.y + font_size,
        text: marker_text,
        font_size,
        color: text_color,
        font_weight: None,
        font_style: None,
        font_family: None, letter_spacing: None,
    });
}

/// Convert a number to lowercase Roman numerals (simple, up to ~3999).
fn to_roman_lower(mut n: i32) -> String {
    let values = [(1000, "m"), (900, "cm"), (500, "d"), (400, "cd"),
                  (100, "c"), (90, "xc"), (50, "l"), (40, "xl"),
                  (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")];
    let mut result = String::new();
    for (val, sym) in &values {
        while n >= *val {
            result.push_str(sym);
            n -= val;
        }
    }
    result
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

/// Extract the CSS `letter-spacing` value from a style map.
///
/// Returns `Some(px)` when letter-spacing is explicitly set, `None` otherwise.
fn extract_letter_spacing(style: &nova_mod_api::content::StyleMap) -> Option<f32> {
    for (key, value) in &style.properties {
        if key == "letter-spacing" {
            match value {
                StyleValue::Px(px) => return Some(*px),
                StyleValue::Str(s) | StyleValue::Keyword(s) => {
                    let s = s.trim();
                    if s == "normal" {
                        return None;
                    }
                    if let Some(px) = parse_length_px(s) {
                        return Some(px);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Extract the CSS `text-shadow` value from a style map.
///
/// Returns `Some((offset_x, offset_y, blur, color))` for the first shadow,
/// or `None` if not set.
fn extract_text_shadow(style: &nova_mod_api::content::StyleMap) -> Option<(f32, f32, f32, Color)> {
    for (key, value) in &style.properties {
        if key == "text-shadow" {
            let s = match value {
                StyleValue::Str(s) | StyleValue::Keyword(s) => s.as_str(),
                _ => continue,
            };
            let s = s.trim();
            if s == "none" || s.is_empty() {
                return None;
            }
            // Parse: offset-x offset-y [blur-radius] [color]
            // Color can come before or after the lengths.
            // Try to detect color tokens vs length tokens.
            let parts: Vec<&str> = s.split(',').next().unwrap_or(s).trim().split_whitespace().collect();
            if parts.is_empty() {
                return None;
            }

            let mut lengths: Vec<f32> = Vec::new();
            let mut color_parts: Vec<&str> = Vec::new();

            for part in &parts {
                if let Some(px) = parse_length_px(part) {
                    lengths.push(px);
                } else {
                    color_parts.push(part);
                }
            }
            // Also check for rgb()/rgba() color that was split by spaces.
            if color_parts.is_empty() && parts.len() > lengths.len() {
                // Reconstruct potential color string from remaining parts.
                let color_str = parts[lengths.len()..].join(" ");
                if let Some(c) = parse_color_string(&color_str) {
                    let ox = lengths.first().copied().unwrap_or(0.0);
                    let oy = lengths.get(1).copied().unwrap_or(0.0);
                    let blur = lengths.get(2).copied().unwrap_or(0.0);
                    return Some((ox, oy, blur, c));
                }
            }
            let color_str = color_parts.join(" ");
            let color = if color_str.is_empty() {
                Color::rgba(0.0, 0.0, 0.0, 0.5)
            } else {
                parse_color_string(&color_str).unwrap_or(Color::rgba(0.0, 0.0, 0.0, 0.5))
            };
            let ox = lengths.first().copied().unwrap_or(0.0);
            let oy = lengths.get(1).copied().unwrap_or(0.0);
            let blur = lengths.get(2).copied().unwrap_or(0.0);
            return Some((ox, oy, blur, color));
        }
    }
    None
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

/// Return `true` if the style map requests text-overflow ellipsis.
///
/// This is `true` when `text-overflow: ellipsis` is set AND the element
/// (or an ancestor) has `overflow: hidden|scroll|auto` AND
/// `white-space: nowrap`.  When all three conditions are met, text that
/// overflows the box should be truncated with an ellipsis character.
fn should_ellipsize(style: &nova_mod_api::content::StyleMap) -> bool {
    let mut has_ellipsis = false;
    let mut has_clip = false;
    let mut has_nowrap = false;

    for (key, value) in &style.properties {
        let val_str = match value {
            nova_mod_api::content::StyleValue::Keyword(k) => k.as_str(),
            nova_mod_api::content::StyleValue::Str(s) => s.as_str(),
            _ => continue,
        };
        match key.as_str() {
            "text-overflow" if val_str == "ellipsis" => has_ellipsis = true,
            "overflow" | "overflow-x" => {
                if matches!(val_str, "hidden" | "scroll" | "auto") {
                    has_clip = true;
                }
            }
            "white-space" if val_str == "nowrap" => has_nowrap = true,
            _ => {}
        }
    }

    has_ellipsis && has_clip && has_nowrap
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

/// Check if the style map contains `text-decoration: ... dotted ...`.
fn has_text_decoration_dotted(style: &nova_mod_api::content::StyleMap) -> bool {
    for (key, value) in &style.properties {
        if key == "text-decoration" {
            match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => {
                    return k.contains("dotted");
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
/// Extract the CSS `font-family` value from a style map.
///
/// Returns the full comma-separated font-family string so the renderer
/// can try each candidate (including generic families like `sans-serif`,
/// `system-ui`) via fontconfig resolution.
fn extract_font_family(style: &nova_mod_api::content::StyleMap) -> Option<String> {
    for (key, value) in &style.properties {
        if key == "font-family" {
            let raw = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// The style of a CSS border side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BorderLineStyle {
    Solid,
    Dashed,
    Dotted,
    Double,
}

/// Per-side border info: `(width, color, line_style)`.
struct BorderSides {
    top: Option<(f32, Color, BorderLineStyle)>,
    right: Option<(f32, Color, BorderLineStyle)>,
    bottom: Option<(f32, Color, BorderLineStyle)>,
    left: Option<(f32, Color, BorderLineStyle)>,
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

    let make_side = |i: usize| -> Option<(f32, Color, BorderLineStyle)> {
        let style_val = side_style[i].as_deref().or(sh_style.as_deref()).unwrap_or("none");
        if style_val == "none" || style_val == "hidden" {
            return None;
        }
        let w = side_width[i].or(sh_width).unwrap_or(1.0);
        let c = side_color[i].or(sh_color).unwrap_or(Color::BLACK);
        let ls = match style_val {
            "dashed" => BorderLineStyle::Dashed,
            "dotted" => BorderLineStyle::Dotted,
            "double" => BorderLineStyle::Double,
            _ => BorderLineStyle::Solid,
        };
        if w > 0.0 { Some((w, c, ls)) } else { None }
    };

    BorderSides {
        top: make_side(0),
        right: make_side(1),
        bottom: make_side(2),
        left: make_side(3),
    }
}

/// Emit render ops for a single border side with the given style.
///
/// For `horizontal == true`, the border runs along the x-axis (top/bottom);
/// for `horizontal == false`, it runs along the y-axis (left/right).
/// `rect_w` and `rect_h` are the full rectangle dimensions of the side
/// (e.g. for a top border: width = element width, height = border thickness).
fn emit_border_side(
    ops: &mut Vec<RenderOp>,
    x: f32, y: f32,
    rect_w: f32, rect_h: f32,
    horizontal: bool,
    color: Color,
    style: BorderLineStyle,
) {
    match style {
        BorderLineStyle::Solid => {
            ops.push(RenderOp::FillRect { x, y, width: rect_w, height: rect_h, color });
        }
        BorderLineStyle::Dashed => {
            // Dashed: segments of 3*thickness on, 3*thickness off.
            let thickness = if horizontal { rect_h } else { rect_w };
            let seg_len = (thickness * 3.0).max(4.0);
            let total_len = if horizontal { rect_w } else { rect_h };
            let mut offset = 0.0;
            let mut draw = true;
            while offset < total_len {
                let len = seg_len.min(total_len - offset);
                if draw {
                    if horizontal {
                        ops.push(RenderOp::FillRect { x: x + offset, y, width: len, height: rect_h, color });
                    } else {
                        ops.push(RenderOp::FillRect { x, y: y + offset, width: rect_w, height: len, color });
                    }
                }
                offset += seg_len;
                draw = !draw;
            }
        }
        BorderLineStyle::Dotted => {
            // Dotted: square dots spaced by one dot-width apart.
            let dot_size = if horizontal { rect_h } else { rect_w };
            let dot_size = dot_size.max(1.0);
            let total_len = if horizontal { rect_w } else { rect_h };
            let mut offset = 0.0;
            while offset < total_len {
                let len = dot_size.min(total_len - offset);
                if horizontal {
                    ops.push(RenderOp::FillRect { x: x + offset, y, width: len, height: rect_h, color });
                } else {
                    ops.push(RenderOp::FillRect { x, y: y + offset, width: rect_w, height: len, color });
                }
                offset += dot_size * 2.0; // dot + gap
            }
        }
        BorderLineStyle::Double => {
            // Double: two lines with a gap equal to the line width between them.
            let thickness = if horizontal { rect_h } else { rect_w };
            let line_w = (thickness / 3.0).max(1.0);
            if horizontal {
                ops.push(RenderOp::FillRect { x, y, width: rect_w, height: line_w, color });
                ops.push(RenderOp::FillRect { x, y: y + rect_h - line_w, width: rect_w, height: line_w, color });
            } else {
                ops.push(RenderOp::FillRect { x, y, width: line_w, height: rect_h, color });
                ops.push(RenderOp::FillRect { x: x + rect_w - line_w, y, width: line_w, height: rect_h, color });
            }
        }
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

/// Extract the widget type (progress/meter) from style properties.
fn extract_widget_type(style: &nova_mod_api::content::StyleMap) -> Option<String> {
    for (key, val) in &style.properties {
        if key == "nova-widget-type" {
            if let StyleValue::Str(s) | StyleValue::Keyword(s) = val {
                return Some(s.clone());
            }
        }
    }
    None
}

/// Extract a string style property by key.
fn extract_style_str(style: &nova_mod_api::content::StyleMap, prop: &str) -> String {
    for (key, val) in &style.properties {
        if key == prop {
            if let StyleValue::Str(s) | StyleValue::Keyword(s) = val {
                return s.clone();
            }
        }
    }
    String::new()
}

/// Paint a `<progress>` or `<meter>` widget.
fn paint_widget(layout_box: &LayoutBox, widget_type: &str, ops: &mut Vec<RenderOp>) {
    let x = layout_box.x;
    let y = layout_box.y;
    let w = layout_box.width;
    let h = layout_box.height;
    let radius = [h / 2.0; 4];

    match widget_type {
        "progress" => {
            let value: f32 = extract_style_str(&layout_box.style, "nova-widget-value")
                .parse().unwrap_or(0.0);
            let max: f32 = extract_style_str(&layout_box.style, "nova-widget-max")
                .parse::<f32>().unwrap_or(1.0).max(0.001);
            let fraction = (value / max).clamp(0.0, 1.0);

            // Track background.
            ops.push(RenderOp::FillRoundedRect {
                x, y, width: w, height: h,
                color: Color::rgb(0.88, 0.88, 0.88), radius,
            });
            // Filled portion.
            let fill_w = (w * fraction).max(0.0);
            if fill_w > 0.0 {
                ops.push(RenderOp::FillRoundedRect {
                    x, y, width: fill_w, height: h,
                    color: Color::rgb(0.18, 0.55, 0.89), radius,
                });
            }
        }
        "meter" => {
            let value: f32 = extract_style_str(&layout_box.style, "nova-widget-value")
                .parse().unwrap_or(0.0);
            let min: f32 = extract_style_str(&layout_box.style, "nova-widget-min")
                .parse().unwrap_or(0.0);
            let max: f32 = extract_style_str(&layout_box.style, "nova-widget-max")
                .parse().unwrap_or(1.0);
            let low: f32 = extract_style_str(&layout_box.style, "nova-widget-low")
                .parse().unwrap_or(min);
            let high: f32 = extract_style_str(&layout_box.style, "nova-widget-high")
                .parse().unwrap_or(max);
            let optimum: f32 = extract_style_str(&layout_box.style, "nova-widget-optimum")
                .parse().unwrap_or((low + high) / 2.0);

            let range = (max - min).max(0.001);
            let fraction = ((value - min) / range).clamp(0.0, 1.0);

            // Color coding based on value vs low/high/optimum.
            let fill_color = if optimum >= low && optimum <= high {
                // Optimum is in the "ok" range.
                if value >= low && value <= high {
                    Color::rgb(0.18, 0.73, 0.30) // green — in optimal range
                } else {
                    Color::rgb(0.93, 0.79, 0.13) // yellow — outside optimal range
                }
            } else if optimum < low {
                // Lower is better.
                if value <= low { Color::rgb(0.18, 0.73, 0.30) }
                else if value <= high { Color::rgb(0.93, 0.79, 0.13) }
                else { Color::rgb(0.88, 0.22, 0.20) }
            } else {
                // Higher is better.
                if value >= high { Color::rgb(0.18, 0.73, 0.30) }
                else if value >= low { Color::rgb(0.93, 0.79, 0.13) }
                else { Color::rgb(0.88, 0.22, 0.20) }
            };

            // Track background.
            ops.push(RenderOp::FillRoundedRect {
                x, y, width: w, height: h,
                color: Color::rgb(0.88, 0.88, 0.88), radius,
            });
            // Filled portion.
            let fill_w = (w * fraction).max(0.0);
            if fill_w > 0.0 {
                ops.push(RenderOp::FillRoundedRect {
                    x, y, width: fill_w, height: h,
                    color: fill_color, radius,
                });
            }
        }
        _ => {}
    }
}

/// Form field info extracted from style properties.
struct FormFieldInfo {
    field_type: String,
    value: String,
    name: String,
    form_action: String,
    form_method: String,
    form_enctype: String,
    placeholder: String,
    checked: bool,
    required: bool,
    options: Vec<(String, String, bool)>,
    pattern: String,
    min: String,
    max: String,
    maxlength: Option<usize>,
    minlength: Option<usize>,
    autofocus: bool,
    tabindex: Option<i32>,
    title: String,
    pointer_events_none: bool,
}

/// Extract form field info from style props (set by mod-layout for form elements).
fn extract_form_field(style: &nova_mod_api::content::StyleMap) -> Option<FormFieldInfo> {
    let mut field_type = None;
    let mut value = String::new();
    let mut name = String::new();
    let mut form_action = String::new();
    let mut form_method = String::from("get");
    let mut form_enctype = String::from("application/x-www-form-urlencoded");
    let mut placeholder = String::new();
    let mut checked = false;
    let mut required = false;
    let mut options_raw = String::new();
    let mut pattern = String::new();
    let mut min = String::new();
    let mut max = String::new();
    let mut maxlength = None;
    let mut minlength = None;
    let mut autofocus = false;
    let mut tabindex = None;
    let mut title = String::new();
    let mut pointer_events_none = false;
    for (key, val) in &style.properties {
        if let StyleValue::Str(s) | StyleValue::Keyword(s) = val {
            match key.as_str() {
                "nova-form-type" => field_type = Some(s.clone()),
                "nova-form-value" => value = s.clone(),
                "nova-form-name" => name = s.clone(),
                "nova-form-action" => form_action = s.clone(),
                "nova-form-method" => form_method = s.clone(),
                "nova-form-enctype" => form_enctype = s.clone(),
                "nova-form-placeholder" => placeholder = s.clone(),
                "nova-form-checked" => checked = s == "true",
                "nova-form-required" => required = s == "true",
                "nova-form-options" => options_raw = s.clone(),
                "nova-form-pattern" => pattern = s.clone(),
                "nova-form-min" => min = s.clone(),
                "nova-form-max" => max = s.clone(),
                "nova-form-maxlength" => maxlength = s.parse().ok(),
                "nova-form-minlength" => minlength = s.parse().ok(),
                "nova-form-autofocus" => autofocus = s == "true",
                "nova-form-tabindex" => tabindex = s.parse().ok(),
                "nova-form-title" => title = s.clone(),
                "pointer-events" => pointer_events_none = s == "none",
                _ => {}
            }
        }
    }

    // Parse options for <select>.
    let options = if options_raw.is_empty() {
        vec![]
    } else {
        options_raw.split('\x02').filter_map(|entry| {
            let parts: Vec<&str> = entry.split('\x01').collect();
            if parts.len() >= 3 {
                Some((parts[0].to_string(), parts[1].to_string(), parts[2] == "1"))
            } else {
                None
            }
        }).collect()
    };

    field_type.map(|ft| FormFieldInfo {
        field_type: ft,
        value,
        name,
        form_action,
        form_method,
        form_enctype,
        placeholder,
        checked,
        required,
        options,
        pattern,
        min,
        max,
        maxlength,
        minlength,
        autofocus,
        tabindex,
        title,
        pointer_events_none,
    })
}

/// Paint visual representation of a form field based on its type.
fn paint_form_field_visual(layout_box: &LayoutBox, info: &FormFieldInfo, ops: &mut Vec<RenderOp>) {
    let x = layout_box.x;
    let y = layout_box.y;
    let w = layout_box.width;
    let h = layout_box.height;

    match info.field_type.as_str() {
        "hidden" => {
            // Hidden fields have no visual representation.
        }

        "checkbox" => {
            let box_size = h.min(w).min(13.0);
            let bx = x + (w - box_size) / 2.0;
            let by = y + (h - box_size) / 2.0;
            let border_color = Color::rgb(0.6, 0.6, 0.6);
            ops.push(RenderOp::FillRect { x: bx, y: by, width: box_size, height: 1.0, color: border_color });
            ops.push(RenderOp::FillRect { x: bx, y: by + box_size - 1.0, width: box_size, height: 1.0, color: border_color });
            ops.push(RenderOp::FillRect { x: bx, y: by, width: 1.0, height: box_size, color: border_color });
            ops.push(RenderOp::FillRect { x: bx + box_size - 1.0, y: by, width: 1.0, height: box_size, color: border_color });
            let bg = if info.checked { Color::rgb(0.26, 0.52, 0.96) } else { Color::WHITE };
            ops.push(RenderOp::FillRect { x: bx + 1.0, y: by + 1.0, width: box_size - 2.0, height: box_size - 2.0, color: bg });
            if info.checked {
                ops.push(RenderOp::DrawText {
                    x: bx + 1.0, y: by + box_size - 2.0,
                    text: "\u{2713}".to_string(),
                    font_size: box_size - 2.0,
                    color: Color::WHITE,
                    font_weight: Some(700),
                    font_style: None,
                    font_family: None, letter_spacing: None,
                });
            }
        }

        "radio" => {
            let box_size = h.min(w).min(13.0);
            let bx = x + (w - box_size) / 2.0;
            let by = y + (h - box_size) / 2.0;
            let radius = [box_size / 2.0; 4];
            ops.push(RenderOp::FillRoundedRect {
                x: bx, y: by, width: box_size, height: box_size,
                color: Color::rgb(0.6, 0.6, 0.6), radius,
            });
            let inner = box_size - 2.0;
            let inner_radius = [inner / 2.0; 4];
            ops.push(RenderOp::FillRoundedRect {
                x: bx + 1.0, y: by + 1.0, width: inner, height: inner,
                color: Color::WHITE, radius: inner_radius,
            });
            if info.checked {
                let dot = box_size * 0.4;
                let dot_radius = [dot / 2.0; 4];
                let offset = (box_size - dot) / 2.0;
                ops.push(RenderOp::FillRoundedRect {
                    x: bx + offset, y: by + offset, width: dot, height: dot,
                    color: Color::rgb(0.26, 0.52, 0.96), radius: dot_radius,
                });
            }
        }

        "submit" | "button" | "reset" => {
            // Light gray background with subtle border, matching Chrome's
            // default form button appearance (#f8f9fa bg, #dadce0 border).
            let bg_color = Color::rgb(0.973, 0.976, 0.98); // #f8f9fa
            let border_color = Color::rgb(0.855, 0.867, 0.878); // #dadce0
            let radius = [4.0; 4];
            ops.push(RenderOp::FillRoundedRect { x, y, width: w, height: h, color: bg_color, radius });
            // Draw border as four 1px rects with rounded-rect clipping approximation.
            ops.push(RenderOp::FillRoundedRect { x, y, width: w, height: 1.0, color: border_color, radius: [4.0, 4.0, 0.0, 0.0] });
            ops.push(RenderOp::FillRoundedRect { x, y: y + h - 1.0, width: w, height: 1.0, color: border_color, radius: [0.0, 0.0, 4.0, 4.0] });
            ops.push(RenderOp::FillRect { x, y: y + 1.0, width: 1.0, height: h - 2.0, color: border_color });
            ops.push(RenderOp::FillRect { x: x + w - 1.0, y: y + 1.0, width: 1.0, height: h - 2.0, color: border_color });
            let label = if info.value.is_empty() {
                match info.field_type.as_str() {
                    "submit" => "Submit",
                    "reset" => "Reset",
                    _ => "Button",
                }
            } else {
                &info.value
            };
            let font_size = extract_font_size(&layout_box.style);
            ops.push(RenderOp::DrawText {
                x: x + 6.0, y: y + font_size + (h - font_size) / 2.0 - 2.0,
                text: label.to_string(),
                font_size,
                color: Color::BLACK,
                font_weight: None,
                font_style: None,
                font_family: None, letter_spacing: None,
            });
        }

        "file" => {
            let btn_w = 90.0_f32.min(w * 0.4);
            let border_color = Color::rgb(0.6, 0.6, 0.6);
            let bg_color = Color::rgb(0.93, 0.93, 0.93);
            ops.push(RenderOp::FillRect { x, y, width: btn_w, height: h, color: bg_color });
            ops.push(RenderOp::FillRect { x, y, width: btn_w, height: 1.0, color: border_color });
            ops.push(RenderOp::FillRect { x, y: y + h - 1.0, width: btn_w, height: 1.0, color: border_color });
            ops.push(RenderOp::FillRect { x, y, width: 1.0, height: h, color: border_color });
            ops.push(RenderOp::FillRect { x: x + btn_w - 1.0, y, width: 1.0, height: h, color: border_color });
            let font_size = extract_font_size(&layout_box.style).min(12.0);
            ops.push(RenderOp::DrawText {
                x: x + 4.0, y: y + font_size + (h - font_size) / 2.0 - 2.0,
                text: "Choose File".to_string(),
                font_size,
                color: Color::BLACK,
                font_weight: None, font_style: None, font_family: None, letter_spacing: None,
            });
            let filename = if info.value.is_empty() { "No file chosen" } else { &info.value };
            ops.push(RenderOp::DrawText {
                x: x + btn_w + 6.0, y: y + font_size + (h - font_size) / 2.0 - 2.0,
                text: filename.to_string(),
                font_size,
                color: Color::rgb(0.4, 0.4, 0.4),
                font_weight: None, font_style: None, font_family: None, letter_spacing: None,
            });
        }

        "select" => {
            let border_color = Color::rgb(0.6, 0.6, 0.6);
            ops.push(RenderOp::FillRect { x, y, width: w, height: h, color: Color::WHITE });
            ops.push(RenderOp::FillRect { x, y, width: w, height: 1.0, color: border_color });
            ops.push(RenderOp::FillRect { x, y: y + h - 1.0, width: w, height: 1.0, color: border_color });
            ops.push(RenderOp::FillRect { x, y, width: 1.0, height: h, color: border_color });
            ops.push(RenderOp::FillRect { x: x + w - 1.0, y, width: 1.0, height: h, color: border_color });
            let arrow_w = 20.0_f32.min(w * 0.15);
            ops.push(RenderOp::FillRect {
                x: x + w - arrow_w, y, width: arrow_w, height: h,
                color: Color::rgb(0.93, 0.93, 0.93),
            });
            let font_size = extract_font_size(&layout_box.style);
            ops.push(RenderOp::DrawText {
                x: x + w - arrow_w + 4.0, y: y + font_size + (h - font_size) / 2.0 - 2.0,
                text: "\u{25BC}".to_string(),
                font_size: font_size * 0.6,
                color: Color::rgb(0.3, 0.3, 0.3),
                font_weight: None, font_style: None, font_family: None, letter_spacing: None,
            });
            let display_text = if !info.value.is_empty() {
                &info.value
            } else if !info.placeholder.is_empty() {
                &info.placeholder
            } else {
                "Select"
            };
            ops.push(RenderOp::DrawText {
                x: x + 6.0, y: y + font_size + (h - font_size) / 2.0 - 2.0,
                text: display_text.to_string(),
                font_size,
                color: Color::BLACK,
                font_weight: None, font_style: None, font_family: None, letter_spacing: None,
            });
        }

        "textarea" => {
            let border_color = Color::rgb(0.6, 0.6, 0.6);
            ops.push(RenderOp::FillRect { x, y, width: w, height: h, color: Color::WHITE });
            ops.push(RenderOp::FillRect { x, y, width: w, height: 1.0, color: border_color });
            ops.push(RenderOp::FillRect { x, y: y + h - 1.0, width: w, height: 1.0, color: border_color });
            ops.push(RenderOp::FillRect { x, y, width: 1.0, height: h, color: border_color });
            ops.push(RenderOp::FillRect { x: x + w - 1.0, y, width: 1.0, height: h, color: border_color });
            let sb_w = 8.0;
            ops.push(RenderOp::FillRect {
                x: x + w - sb_w - 1.0, y: y + 1.0, width: sb_w, height: h - 2.0,
                color: Color::rgb(0.92, 0.92, 0.92),
            });
            // Resize grip handle in bottom-right corner (three diagonal lines).
            let grip_color = Color::rgb(0.6, 0.6, 0.6);
            let gx = x + w - 2.0;
            let gy = y + h - 2.0;
            for i in 0..3 {
                let offset = (i as f32) * 4.0;
                // Each grip line is a small 1px diagonal represented as a short rect.
                ops.push(RenderOp::FillRect {
                    x: gx - 4.0 - offset, y: gy - 1.0,
                    width: 2.0, height: 2.0, color: grip_color,
                });
                ops.push(RenderOp::FillRect {
                    x: gx - 1.0, y: gy - 4.0 - offset,
                    width: 2.0, height: 2.0, color: grip_color,
                });
                if i > 0 {
                    ops.push(RenderOp::FillRect {
                        x: gx - 4.0 - offset + 4.0, y: gy - 4.0 - offset + 4.0,
                        width: 2.0, height: 2.0, color: grip_color,
                    });
                }
            }
            let font_size = extract_font_size(&layout_box.style);
            let display_text = if info.value.is_empty() { &info.placeholder } else { &info.value };
            if !display_text.is_empty() {
                let text_color = if info.value.is_empty() {
                    Color::rgb(0.6, 0.6, 0.6)
                } else {
                    Color::BLACK
                };
                ops.push(RenderOp::DrawText {
                    x: x + 4.0, y: y + font_size + 2.0,
                    text: display_text.to_string(),
                    font_size,
                    color: text_color,
                    font_weight: None, font_style: None, font_family: None, letter_spacing: None,
                });
            }
        }

        "password" => {
            paint_text_input_visual(layout_box, info, ops, true);
        }

        // text, email, number, date, search, tel, url and other text-like inputs
        _ => {
            paint_text_input_visual(layout_box, info, ops, false);
        }
    }

    // Draw validation error indicator (red border) for required empty fields.
    if info.required && info.value.is_empty()
        && !matches!(info.field_type.as_str(), "hidden" | "submit" | "button" | "reset" | "checkbox" | "radio")
    {
        let red = Color::rgb(0.9, 0.2, 0.2);
        ops.push(RenderOp::FillRect { x, y, width: w, height: 1.5, color: red });
        ops.push(RenderOp::FillRect { x, y: y + h - 1.5, width: w, height: 1.5, color: red });
        ops.push(RenderOp::FillRect { x, y, width: 1.5, height: h, color: red });
        ops.push(RenderOp::FillRect { x: x + w - 1.5, y, width: 1.5, height: h, color: red });
    }
}

/// Paint a text input field (text, password, email, number, date, etc.).
fn paint_text_input_visual(layout_box: &LayoutBox, info: &FormFieldInfo, ops: &mut Vec<RenderOp>, is_password: bool) {
    let x = layout_box.x;
    let y = layout_box.y;
    let w = layout_box.width;
    let h = layout_box.height;
    let border_color = Color::rgb(0.6, 0.6, 0.6);
    let font_size = extract_font_size(&layout_box.style);

    // Check for border-radius — use rounded rects when present.
    let radius = extract_border_radius(&layout_box.style, w, h);
    let has_radius = radius.iter().any(|&r| r > 0.0);

    if has_radius {
        // Rounded white background.
        ops.push(RenderOp::FillRoundedRect { x, y, width: w, height: h, color: Color::WHITE, radius });
        // Rounded border — draw a slightly larger rounded rect behind, then the white fill on top.
        // Simpler approach: draw 1px border lines clipped to corners via rounded rect overlay.
        let border_radius = radius;
        ops.push(RenderOp::FillRoundedRect {
            x: x - 1.0, y: y - 1.0, width: w + 2.0, height: h + 2.0,
            color: border_color, radius: [border_radius[0] + 1.0, border_radius[1] + 1.0, border_radius[2] + 1.0, border_radius[3] + 1.0],
        });
        ops.push(RenderOp::FillRoundedRect { x, y, width: w, height: h, color: Color::WHITE, radius });
    } else {
        // White background.
        ops.push(RenderOp::FillRect { x, y, width: w, height: h, color: Color::WHITE });
        // Border.
        ops.push(RenderOp::FillRect { x, y, width: w, height: 1.0, color: border_color });
        ops.push(RenderOp::FillRect { x, y: y + h - 1.0, width: w, height: 1.0, color: border_color });
        ops.push(RenderOp::FillRect { x, y, width: 1.0, height: h, color: border_color });
        ops.push(RenderOp::FillRect { x: x + w - 1.0, y, width: 1.0, height: h, color: border_color });
    }

    // Text content or placeholder.
    let display_text = if info.value.is_empty() {
        info.placeholder.clone()
    } else if is_password {
        "\u{2022}".repeat(info.value.len())
    } else {
        info.value.clone()
    };

    if !display_text.is_empty() {
        let text_color = if info.value.is_empty() {
            Color::rgb(0.6, 0.6, 0.6) // placeholder color
        } else {
            Color::BLACK
        };
        ops.push(RenderOp::DrawText {
            x: x + 4.0, y: y + font_size + (h - font_size) / 2.0 - 2.0,
            text: display_text,
            font_size,
            color: text_color,
            font_weight: None, font_style: None, font_family: None, letter_spacing: None,
        });
    }
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

/// Parse a CSS gradient direction (first argument) and return the angle in degrees.
///
/// CSS gradient angles: 0deg = to top, 90deg = to right, 180deg = to bottom, etc.
/// Returns `None` if the first argument is not a direction (i.e., it's a color stop).
fn parse_gradient_direction(dir: &str) -> Option<f32> {
    let dir = dir.trim();
    // Keyword directions.
    match dir {
        "to top" => return Some(0.0),
        "to right" => return Some(90.0),
        "to bottom" => return Some(180.0),
        "to left" => return Some(270.0),
        "to top right" | "to right top" => return Some(45.0),
        "to bottom right" | "to right bottom" => return Some(135.0),
        "to bottom left" | "to left bottom" => return Some(225.0),
        "to top left" | "to left top" => return Some(315.0),
        _ => {}
    }
    // Angle values: deg, rad, turn, grad.
    if dir.starts_with("to ") {
        return None; // Unknown "to ..." direction.
    }
    parse_angle_deg(dir)
}

/// Parse gradient color stops from a slice of argument strings.
///
/// Each part is something like `"red"`, `"blue 50%"`, `"#ff0 10% 30%"`.
fn parse_gradient_stops(parts: &[&str]) -> Vec<(f32, Color)> {
    let mut stops: Vec<(f32, Color)> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let part = part.trim();
        // Try to extract a percentage at the end.
        let (color_str, pct) = if let Some(pct_idx) = part.rfind('%') {
            let before_pct = part[..pct_idx].trim();
            // Find the last space before the percentage number.
            if let Some(space_idx) = before_pct.rfind(' ') {
                let num_str = &before_pct[space_idx + 1..];
                let color = before_pct[..space_idx].trim();
                let p = num_str.parse::<f32>().unwrap_or(0.0) / 100.0;
                (color, p)
            } else {
                (part, i as f32 / (parts.len() - 1).max(1) as f32)
            }
        } else if let Some(px_idx) = part.rfind("px") {
            // Support pixel positions (approximate as percentage of 100px).
            let before_px = part[..px_idx].trim();
            if let Some(space_idx) = before_px.rfind(' ') {
                let num_str = &before_px[space_idx + 1..];
                let color = before_px[..space_idx].trim();
                let p = num_str.parse::<f32>().unwrap_or(0.0) / 100.0;
                (color, p.clamp(0.0, 1.0))
            } else {
                (part, i as f32 / (parts.len() - 1).max(1) as f32)
            }
        } else {
            (part, i as f32 / (parts.len() - 1).max(1) as f32)
        };

        if let Some(color) = parse_color_string(color_str) {
            stops.push((pct, color));
        }
    }
    stops
}

/// Render a CSS `linear-gradient(...)` as pixel strips.
///
/// Supports all angle formats (`to right`, `to bottom left`, `45deg`,
/// `0.5turn`, `100grad`, etc.) and multiple color stops with positions.
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

    let parts: Vec<&str> = split_gradient_args(inner);
    if parts.len() < 2 {
        return;
    }

    // Check if the first part is a direction.
    let (angle_deg, color_parts) = if let Some(angle) = parse_gradient_direction(parts[0]) {
        (angle, &parts[1..])
    } else {
        (180.0, &parts[..]) // default: to bottom = 180deg
    };

    let stops = parse_gradient_stops(color_parts);
    if stops.len() < 2 {
        return;
    }

    // Rasterize to pixel buffer and emit as DrawImage for angled gradients.
    let w = width.ceil() as u32;
    let h = height.ceil() as u32;
    if w == 0 || h == 0 {
        return;
    }

    // For axis-aligned gradients, use fast-path strips.
    let norm_angle = ((angle_deg % 360.0) + 360.0) % 360.0;
    if (norm_angle - 180.0).abs() < 0.1 || norm_angle < 0.1 || (norm_angle - 360.0).abs() < 0.1 {
        // Vertical: to bottom (180) or to top (0/360).
        let reverse = norm_angle < 0.1 || (norm_angle - 360.0).abs() < 0.1;
        let steps = h as usize;
        for step in 0..steps.max(1) {
            let t = step as f32 / (steps - 1).max(1) as f32;
            let t = if reverse { 1.0 - t } else { t };
            let color = multiply_alpha(interpolate_gradient(&stops, t), opacity);
            ops.push(RenderOp::FillRect {
                x, y: y + step as f32, width, height: 1.0, color,
            });
        }
        return;
    }
    if (norm_angle - 90.0).abs() < 0.1 || (norm_angle - 270.0).abs() < 0.1 {
        // Horizontal: to right (90) or to left (270).
        let reverse = (norm_angle - 270.0).abs() < 0.1;
        let steps = w as usize;
        for step in 0..steps.max(1) {
            let t = step as f32 / (steps - 1).max(1) as f32;
            let t = if reverse { 1.0 - t } else { t };
            let color = multiply_alpha(interpolate_gradient(&stops, t), opacity);
            ops.push(RenderOp::FillRect {
                x: x + step as f32, y, width: 1.0, height, color,
            });
        }
        return;
    }

    // General angled gradient: rasterize to pixel buffer.
    let rad = (angle_deg - 90.0).to_radians();
    let dx = rad.cos();
    let dy = rad.sin();
    // Gradient line length = projection of the box diagonal onto the gradient direction.
    let half_w = width / 2.0;
    let half_h = height / 2.0;
    let gradient_len = (dx.abs() * half_w + dy.abs() * half_h) * 2.0;
    if gradient_len <= 0.0 { return; }

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for py in 0..h {
        for px in 0..w {
            let fx = px as f32 + 0.5 - half_w;
            let fy = py as f32 + 0.5 - half_h;
            let proj = (fx * dx + fy * dy) / gradient_len + 0.5;
            let t = proj.clamp(0.0, 1.0);
            let color = multiply_alpha(interpolate_gradient(&stops, t), opacity);
            let idx = ((py * w + px) * 4) as usize;
            pixels[idx]     = (color.r * 255.0) as u8;
            pixels[idx + 1] = (color.g * 255.0) as u8;
            pixels[idx + 2] = (color.b * 255.0) as u8;
            pixels[idx + 3] = (color.a * 255.0) as u8;
        }
    }
    ops.push(RenderOp::DrawImage {
        x, y, width, height,
        img_width: w, img_height: h, pixels,
    });
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
// Background image sizing, positioning, and tiling
// ---------------------------------------------------------------------------

/// Parsed `background-size` value.
#[derive(Debug, Clone, PartialEq)]
enum BgSize {
    /// `cover` — scale to cover the entire box.
    Cover,
    /// `contain` — scale to fit inside the box.
    Contain,
    /// Explicit size (width, height). `None` means `auto` for that dimension.
    Explicit(Option<f32>, Option<f32>),
}

/// Extract `background-size` from a style map.
fn extract_background_size(style: &nova_mod_api::content::StyleMap) -> BgSize {
    for (key, value) in &style.properties {
        if key == "background-size" {
            let s = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            let s = s.trim();
            return match s {
                "cover" => BgSize::Cover,
                "contain" => BgSize::Contain,
                "auto" => BgSize::Explicit(None, None),
                _ => {
                    let parts: Vec<&str> = s.split_whitespace().collect();
                    let parse_dim = |p: &str| -> Option<f32> {
                        if p == "auto" { None } else { parse_length_or_percent(p, 0.0) }
                    };
                    let w = parts.first().and_then(|p| parse_dim(p));
                    let h = parts.get(1).and_then(|p| parse_dim(p));
                    BgSize::Explicit(w, h)
                }
            };
        }
    }
    BgSize::Explicit(None, None) // default: auto auto
}

/// Parse a CSS length or percentage value.
///
/// Percentages are resolved against `reference` (the container dimension).
fn parse_length_or_percent(s: &str, reference: f32) -> Option<f32> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f32>().ok().map(|p| p / 100.0 * reference)
    } else {
        parse_length_px(s)
    }
}

/// Compute the drawn size of a background image given the sizing mode.
fn compute_background_size(
    bg_size: &BgSize,
    box_w: f32, box_h: f32,
    img_w: f32, img_h: f32,
) -> (f32, f32) {
    if img_w <= 0.0 || img_h <= 0.0 {
        return (box_w, box_h);
    }
    let aspect = img_w / img_h;
    match bg_size {
        BgSize::Cover => {
            let scale = (box_w / img_w).max(box_h / img_h);
            (img_w * scale, img_h * scale)
        }
        BgSize::Contain => {
            let scale = (box_w / img_w).min(box_h / img_h);
            (img_w * scale, img_h * scale)
        }
        BgSize::Explicit(w, h) => {
            match (w, h) {
                (Some(w), Some(h)) => (*w, *h),
                (Some(w), None) => (*w, *w / aspect),
                (None, Some(h)) => (*h * aspect, *h),
                (None, None) => (img_w, img_h),
            }
        }
    }
}

/// Parsed `background-position` value.
#[derive(Debug, Clone, PartialEq)]
struct BgPosition {
    x: BgPosAxis,
    y: BgPosAxis,
}

#[derive(Debug, Clone, PartialEq)]
enum BgPosAxis {
    Px(f32),
    Percent(f32),
}

impl Default for BgPosition {
    fn default() -> Self {
        Self { x: BgPosAxis::Percent(0.0), y: BgPosAxis::Percent(0.0) }
    }
}

/// Extract `background-position` from a style map.
fn extract_background_position(style: &nova_mod_api::content::StyleMap) -> BgPosition {
    for (key, value) in &style.properties {
        if key == "background-position" {
            let s = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            return parse_bg_position(s.trim());
        }
    }
    BgPosition::default()
}

fn parse_bg_position(s: &str) -> BgPosition {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.is_empty() { return BgPosition::default(); }

    let parse_axis = |token: &str| -> BgPosAxis {
        match token {
            "center" => BgPosAxis::Percent(50.0),
            "left" | "top" => BgPosAxis::Percent(0.0),
            "right" | "bottom" => BgPosAxis::Percent(100.0),
            _ => {
                if let Some(pct) = token.strip_suffix('%') {
                    BgPosAxis::Percent(pct.trim().parse().unwrap_or(0.0))
                } else {
                    BgPosAxis::Px(parse_length_px(token).unwrap_or(0.0))
                }
            }
        }
    };

    if parts.len() == 1 {
        let axis = parse_axis(parts[0]);
        let is_y_keyword = matches!(parts[0], "top" | "bottom");
        if is_y_keyword {
            BgPosition { x: BgPosAxis::Percent(50.0), y: axis }
        } else {
            BgPosition { x: axis, y: BgPosAxis::Percent(50.0) }
        }
    } else {
        BgPosition { x: parse_axis(parts[0]), y: parse_axis(parts[1]) }
    }
}

/// Compute the pixel offset for background positioning.
fn compute_background_position(
    pos: &BgPosition,
    box_w: f32, box_h: f32,
    draw_w: f32, draw_h: f32,
) -> (f32, f32) {
    let off_x = match pos.x {
        BgPosAxis::Px(px) => px,
        BgPosAxis::Percent(pct) => (pct / 100.0) * (box_w - draw_w),
    };
    let off_y = match pos.y {
        BgPosAxis::Px(px) => px,
        BgPosAxis::Percent(pct) => (pct / 100.0) * (box_h - draw_h),
    };
    (off_x, off_y)
}

/// Parsed `background-repeat` value.
#[derive(Debug, Clone, PartialEq)]
enum BgRepeat {
    Repeat,
    NoRepeat,
    RepeatX,
    RepeatY,
}

/// Extract `background-repeat` from a style map.
fn extract_background_repeat(style: &nova_mod_api::content::StyleMap) -> BgRepeat {
    for (key, value) in &style.properties {
        if key == "background-repeat" {
            let s = match value {
                StyleValue::Keyword(k) | StyleValue::Str(k) => k.as_str(),
                _ => continue,
            };
            return match s.trim() {
                "no-repeat" => BgRepeat::NoRepeat,
                "repeat-x" => BgRepeat::RepeatX,
                "repeat-y" => BgRepeat::RepeatY,
                _ => BgRepeat::Repeat,
            };
        }
    }
    BgRepeat::Repeat
}

/// Emit DrawImage ops for a background image, handling tiling.
fn emit_background_image_ops(
    ops: &mut Vec<RenderOp>,
    box_x: f32, box_y: f32,
    box_w: f32, box_h: f32,
    off_x: f32, off_y: f32,
    draw_w: f32, draw_h: f32,
    img_width: u32, img_height: u32,
    pixels: &[u8],
    repeat: &BgRepeat,
) {
    if draw_w <= 0.0 || draw_h <= 0.0 { return; }

    let tile = |start_off: f32, tile_size: f32, container_size: f32, do_repeat: bool| -> Vec<f32> {
        if !do_repeat {
            return vec![start_off];
        }
        let mut positions = Vec::new();
        // Start from the first visible tile.
        let mut pos = start_off % tile_size;
        if pos > 0.0 { pos -= tile_size; }
        while pos < container_size {
            positions.push(pos);
            pos += tile_size;
        }
        positions
    };

    let repeat_x = matches!(repeat, BgRepeat::Repeat | BgRepeat::RepeatX);
    let repeat_y = matches!(repeat, BgRepeat::Repeat | BgRepeat::RepeatY);

    let x_positions = tile(off_x, draw_w, box_w, repeat_x);
    let y_positions = tile(off_y, draw_h, box_h, repeat_y);

    for &ty in &y_positions {
        for &tx in &x_positions {
            ops.push(RenderOp::DrawImage {
                x: box_x + tx,
                y: box_y + ty,
                width: draw_w,
                height: draw_h,
                img_width,
                img_height,
                pixels: pixels.to_vec(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Radial gradient
// ---------------------------------------------------------------------------

/// Render a CSS `radial-gradient(...)` by rasterizing to a pixel buffer.
///
/// Supports:
/// - `radial-gradient(circle, red, blue)`
/// - `radial-gradient(ellipse, red, blue)`
/// - `radial-gradient(closest-side, red, blue)`
/// - `radial-gradient(farthest-corner, red, blue)`
/// - Color stops with percentage positions.
fn paint_radial_gradient(
    value: &str,
    x: f32, y: f32, width: f32, height: f32,
    opacity: f32,
    ops: &mut Vec<RenderOp>,
) {
    let inner = if let Some(idx) = value.find("radial-gradient(") {
        let after = &value[idx + "radial-gradient(".len()..];
        after.strip_suffix(')').unwrap_or(after)
    } else {
        return;
    };

    let parts: Vec<&str> = split_gradient_args(inner);
    if parts.len() < 2 { return; }

    // Parse shape/size keyword from first argument if present.
    let first = parts[0].trim().to_lowercase();
    let (is_circle, _size_keyword, color_parts) = if first == "circle"
        || first == "ellipse"
        || first.starts_with("closest-")
        || first.starts_with("farthest-")
        || first.contains("at ")
    {
        let circle = first.contains("circle");
        let size_kw = if first.starts_with("closest-side") { "closest-side" }
            else if first.starts_with("closest-corner") { "closest-corner" }
            else if first.starts_with("farthest-side") { "farthest-side" }
            else { "farthest-corner" };
        (circle, size_kw, &parts[1..])
    } else {
        (false, "farthest-corner", &parts[..])
    };

    let stops = parse_gradient_stops(color_parts);
    if stops.len() < 2 { return; }

    let w = width.ceil() as u32;
    let h = height.ceil() as u32;
    if w == 0 || h == 0 { return; }

    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let rx = if is_circle { cx.min(cy) } else { cx };
    let ry = if is_circle { cx.min(cy) } else { cy };

    if rx <= 0.0 || ry <= 0.0 { return; }

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for py in 0..h {
        for px in 0..w {
            let fx = (px as f32 + 0.5 - cx) / rx;
            let fy = (py as f32 + 0.5 - cy) / ry;
            let dist = (fx * fx + fy * fy).sqrt();
            let t = dist.clamp(0.0, 1.0);
            let color = multiply_alpha(interpolate_gradient(&stops, t), opacity);
            let idx = ((py * w + px) * 4) as usize;
            pixels[idx]     = (color.r * 255.0) as u8;
            pixels[idx + 1] = (color.g * 255.0) as u8;
            pixels[idx + 2] = (color.b * 255.0) as u8;
            pixels[idx + 3] = (color.a * 255.0) as u8;
        }
    }

    ops.push(RenderOp::DrawImage {
        x, y, width, height,
        img_width: w, img_height: h, pixels,
    });
}

// ---------------------------------------------------------------------------
// Conic gradient
// ---------------------------------------------------------------------------

/// Render a CSS `conic-gradient(...)` by rasterizing to a pixel buffer.
///
/// Supports `conic-gradient(from <angle>, color1, color2, ...)`.
fn paint_conic_gradient(
    value: &str,
    x: f32, y: f32, width: f32, height: f32,
    opacity: f32,
    ops: &mut Vec<RenderOp>,
) {
    let inner = if let Some(idx) = value.find("conic-gradient(") {
        let after = &value[idx + "conic-gradient(".len()..];
        after.strip_suffix(')').unwrap_or(after)
    } else {
        return;
    };

    let parts: Vec<&str> = split_gradient_args(inner);
    if parts.len() < 2 { return; }

    // Check for "from <angle>" prefix.
    let (start_angle_deg, color_parts) = {
        let first = parts[0].trim();
        if first.starts_with("from ") {
            let angle_str = first.strip_prefix("from ").unwrap().trim();
            // May also contain "at center" etc. — strip anything after the angle.
            let angle_token = angle_str.split_whitespace().next().unwrap_or(angle_str);
            let angle = parse_angle_deg(angle_token).unwrap_or(0.0);
            (angle, &parts[1..])
        } else {
            (0.0, &parts[..])
        }
    };

    let stops = parse_gradient_stops(color_parts);
    if stops.len() < 2 { return; }

    let w = width.ceil() as u32;
    let h = height.ceil() as u32;
    if w == 0 || h == 0 { return; }

    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let start_rad = start_angle_deg.to_radians();

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for py in 0..h {
        for px in 0..w {
            let fx = px as f32 + 0.5 - cx;
            let fy = py as f32 + 0.5 - cy;
            // atan2 gives angle from positive x-axis; CSS conic starts from top (negative y).
            let angle = (fy.atan2(fx) - start_rad + std::f32::consts::FRAC_PI_2)
                .rem_euclid(std::f32::consts::TAU);
            let t = angle / std::f32::consts::TAU;
            let color = multiply_alpha(interpolate_gradient(&stops, t), opacity);
            let idx = ((py * w + px) * 4) as usize;
            pixels[idx]     = (color.r * 255.0) as u8;
            pixels[idx + 1] = (color.g * 255.0) as u8;
            pixels[idx + 2] = (color.b * 255.0) as u8;
            pixels[idx + 3] = (color.a * 255.0) as u8;
        }
    }

    ops.push(RenderOp::DrawImage {
        x, y, width, height,
        img_width: w, img_height: h, pixels,
    });
}

// ---------------------------------------------------------------------------
// Media element painting
// ---------------------------------------------------------------------------

/// Paint a `<video>` element with a dark background, play button overlay,
/// optional poster image, and controls bar.
fn paint_video(
    x: f32, y: f32, width: f32, height: f32,
    _src: &str, poster: Option<&str>, controls: bool,
    images: &HashMap<String, Vec<u8>>,
    opacity: f32,
    ops: &mut Vec<RenderOp>,
) {
    // Dark background for the video area.
    ops.push(RenderOp::FillRect {
        x, y, width, height,
        color: multiply_alpha(Color::rgb(0.07, 0.07, 0.07), opacity),
    });

    // Poster image if available.
    let mut poster_drawn = false;
    if let Some(poster_url) = poster {
        if let Some(decoded) = images.get(poster_url) {
            if decoded.len() >= 8 {
                let iw = u32::from_le_bytes([decoded[0], decoded[1], decoded[2], decoded[3]]);
                let ih = u32::from_le_bytes([decoded[4], decoded[5], decoded[6], decoded[7]]);
                let px = decoded[8..].to_vec();
                ops.push(RenderOp::DrawImage {
                    x, y, width, height,
                    img_width: iw, img_height: ih, pixels: px,
                });
                poster_drawn = true;
            }
        }
    }

    // Controls bar height.
    let controls_h = if controls { 36.0 } else { 0.0 };

    // Play button overlay (centered triangle).
    let video_area_h = height - controls_h;
    let btn_size = 48.0_f32.min(video_area_h * 0.5).min(width * 0.3);
    let btn_cx = x + width / 2.0;
    let btn_cy = y + video_area_h / 2.0;

    // Semi-transparent circle behind the play icon.
    let circle_color = if poster_drawn {
        multiply_alpha(Color::rgba(0.0, 0.0, 0.0, 0.5), opacity)
    } else {
        multiply_alpha(Color::rgba(0.3, 0.3, 0.3, 0.6), opacity)
    };
    ops.push(RenderOp::FillRoundedRect {
        x: btn_cx - btn_size / 2.0,
        y: btn_cy - btn_size / 2.0,
        width: btn_size,
        height: btn_size,
        color: circle_color,
        radius: [btn_size / 2.0; 4],
    });

    // Play triangle (approximated with a unicode character).
    let tri_size = btn_size * 0.4;
    ops.push(RenderOp::DrawText {
        x: btn_cx - tri_size * 0.35,
        y: btn_cy + tri_size * 0.4,
        text: "\u{25B6}".to_string(), // triangle
        font_size: tri_size,
        color: multiply_alpha(Color::WHITE, opacity),
        font_weight: None,
        font_style: None,
        font_family: None, letter_spacing: None,
    });

    // Controls bar at the bottom.
    if controls {
        paint_media_controls(
            x, y + height - controls_h,
            width, controls_h,
            opacity, ops,
        );
    }
}

/// Paint an `<audio>` element as a horizontal player bar.
fn paint_audio(
    x: f32, y: f32, width: f32, height: f32,
    _src: &str, controls: bool,
    opacity: f32,
    ops: &mut Vec<RenderOp>,
) {
    if !controls {
        // Audio without controls is invisible (per spec).
        return;
    }

    // Rounded gray background (Chromium-style audio player).
    ops.push(RenderOp::FillRoundedRect {
        x, y, width, height,
        color: multiply_alpha(Color::rgb(0.94, 0.94, 0.94), opacity),
        radius: [height / 2.0; 4],
    });

    paint_media_controls(x, y, width, height, opacity, ops);
}

/// Paint the shared media controls bar (play button, progress bar, time, volume).
fn paint_media_controls(
    x: f32, y: f32, width: f32, height: f32,
    opacity: f32,
    ops: &mut Vec<RenderOp>,
) {
    let controls_bg = multiply_alpha(Color::rgba(0.15, 0.15, 0.15, 0.85), opacity);
    let text_color = multiply_alpha(Color::rgb(0.9, 0.9, 0.9), opacity);
    let bar_track = multiply_alpha(Color::rgba(1.0, 1.0, 1.0, 0.25), opacity);
    let font_size = 11.0_f32.min(height * 0.35);
    let pad = 8.0;

    // Controls background.
    ops.push(RenderOp::FillRoundedRect {
        x, y, width, height,
        color: controls_bg,
        radius: [4.0; 4],
    });

    // Play/pause button.
    let btn_x = x + pad;
    let btn_cy = y + height / 2.0;
    ops.push(RenderOp::DrawText {
        x: btn_x,
        y: btn_cy + font_size * 0.35,
        text: "\u{25B6}".to_string(), // triangle
        font_size,
        color: text_color,
        font_weight: None,
        font_style: None,
        font_family: None, letter_spacing: None,
    });

    // Time display.
    let time_text = "0:00 / 0:00";
    let time_w = time_text.len() as f32 * font_size * 0.5;
    let time_x = btn_x + font_size + pad;
    ops.push(RenderOp::DrawText {
        x: time_x,
        y: btn_cy + font_size * 0.35,
        text: time_text.to_string(),
        font_size,
        color: text_color,
        font_weight: None,
        font_style: None,
        font_family: None, letter_spacing: None,
    });

    // Progress bar (empty track).
    let prog_x = time_x + time_w + pad;
    let prog_h = 4.0;
    let vol_area = font_size + pad * 2.0; // space for volume + fullscreen
    let prog_w = (width - (prog_x - x) - vol_area).max(20.0);
    ops.push(RenderOp::FillRoundedRect {
        x: prog_x,
        y: btn_cy - prog_h / 2.0,
        width: prog_w,
        height: prog_h,
        color: bar_track,
        radius: [2.0; 4],
    });

    // Volume icon.
    let vol_x = prog_x + prog_w + pad;
    ops.push(RenderOp::DrawText {
        x: vol_x,
        y: btn_cy + font_size * 0.35,
        text: "\u{1F50A}".to_string(), // speaker icon
        font_size: font_size * 0.9,
        color: text_color,
        font_weight: None,
        font_style: None,
        font_family: None, letter_spacing: None,
    });
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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());

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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());
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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());
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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());
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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());
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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());

        let shadow_pos = ops.iter().position(|op| matches!(op, RenderOp::BoxShadow { .. }));
        let bg_pos = ops.iter().position(|op| matches!(op, RenderOp::FillRect { .. }));
        assert!(shadow_pos.is_some(), "should emit a BoxShadow op");
        assert!(bg_pos.is_some(), "should emit a FillRect for the background");
        assert!(shadow_pos.unwrap() < bg_pos.unwrap(), "shadow must come before background");
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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());
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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());
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
        // Now returns the full comma-separated string for fontconfig resolution.
        assert_eq!(result, Some("\"Roboto\", sans-serif".to_string()));
    }

    #[test]
    fn extract_font_family_generic_returns_some() {
        let mut style = StyleMap::default();
        style.properties.push((
            "font-family".into(),
            StyleValue::Keyword("sans-serif".into()),
        ));
        let result = extract_font_family(&style);
        // Generic families are now passed through for fontconfig resolution.
        assert_eq!(result, Some("sans-serif".to_string()));
    }

    #[test]
    fn extract_font_family_none_when_absent() {
        let style = StyleMap::default();
        let result = extract_font_family(&style);
        assert_eq!(result, None);
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
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());
        let has_family = ops.iter().any(|op| {
            if let RenderOp::DrawText { font_family, .. } = op {
                font_family.is_some()
            } else {
                false
            }
        });
        assert!(has_family, "DrawText should carry font_family");
    }

    // --- Sprint 4 tests ---

    #[test]
    fn parse_gradient_direction_keywords() {
        assert_eq!(parse_gradient_direction("to top"), Some(0.0));
        assert_eq!(parse_gradient_direction("to right"), Some(90.0));
        assert_eq!(parse_gradient_direction("to bottom"), Some(180.0));
        assert_eq!(parse_gradient_direction("to left"), Some(270.0));
        assert_eq!(parse_gradient_direction("to top right"), Some(45.0));
        assert_eq!(parse_gradient_direction("to bottom left"), Some(225.0));
    }

    #[test]
    fn parse_gradient_direction_angles() {
        assert!((parse_gradient_direction("45deg").unwrap() - 45.0).abs() < 0.01);
        assert!((parse_gradient_direction("0.5turn").unwrap() - 180.0).abs() < 0.01);
        // A color like "red" should return None (not a direction).
        assert!(parse_gradient_direction("red").is_none());
    }

    #[test]
    fn parse_gradient_stops_basic() {
        let stops = parse_gradient_stops(&["red", "blue"]);
        assert_eq!(stops.len(), 2);
        assert!((stops[0].0 - 0.0).abs() < 0.01);
        assert!((stops[1].0 - 1.0).abs() < 0.01);
    }

    #[test]
    fn parse_gradient_stops_with_positions() {
        let stops = parse_gradient_stops(&["red 10%", "blue 90%"]);
        assert_eq!(stops.len(), 2);
        assert!((stops[0].0 - 0.1).abs() < 0.01);
        assert!((stops[1].0 - 0.9).abs() < 0.01);
    }

    #[test]
    fn radial_gradient_emits_draw_image() {
        let mut ops = Vec::new();
        paint_radial_gradient(
            "radial-gradient(circle, red, blue)",
            0.0, 0.0, 100.0, 100.0, 1.0, &mut ops,
        );
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawImage { .. })),
            "radial gradient should emit DrawImage"
        );
    }

    #[test]
    fn conic_gradient_emits_draw_image() {
        let mut ops = Vec::new();
        paint_conic_gradient(
            "conic-gradient(from 0deg, red, blue)",
            0.0, 0.0, 100.0, 100.0, 1.0, &mut ops,
        );
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawImage { .. })),
            "conic gradient should emit DrawImage"
        );
    }

    #[test]
    fn background_size_cover() {
        let (w, h) = compute_background_size(&BgSize::Cover, 200.0, 100.0, 50.0, 50.0);
        // cover: scale to fill 200x100 from 50x50 → scale=4 → 200x200
        assert!((w - 200.0).abs() < 0.1);
        assert!((h - 200.0).abs() < 0.1);
    }

    #[test]
    fn background_size_contain() {
        let (w, h) = compute_background_size(&BgSize::Contain, 200.0, 100.0, 50.0, 50.0);
        // contain: scale to fit 200x100 from 50x50 → scale=2 → 100x100
        assert!((w - 100.0).abs() < 0.1);
        assert!((h - 100.0).abs() < 0.1);
    }

    #[test]
    fn background_position_center() {
        let pos = parse_bg_position("center");
        assert_eq!(pos.x, BgPosAxis::Percent(50.0));
        assert_eq!(pos.y, BgPosAxis::Percent(50.0));
    }

    #[test]
    fn background_position_top_left() {
        let pos = parse_bg_position("left top");
        assert_eq!(pos.x, BgPosAxis::Percent(0.0));
        assert_eq!(pos.y, BgPosAxis::Percent(0.0));
    }

    #[test]
    fn background_repeat_no_repeat_single_tile() {
        let mut ops = Vec::new();
        let pixels = vec![255u8; 4 * 10 * 10]; // 10x10 RGBA
        emit_background_image_ops(
            &mut ops, 0.0, 0.0, 200.0, 200.0,
            50.0, 50.0, 10.0, 10.0,
            10, 10, &pixels,
            &BgRepeat::NoRepeat,
        );
        // no-repeat: exactly one DrawImage.
        assert_eq!(ops.len(), 1);
        if let RenderOp::DrawImage { x, y, .. } = &ops[0] {
            assert!((*x - 50.0).abs() < 0.1);
            assert!((*y - 50.0).abs() < 0.1);
        } else {
            panic!("expected DrawImage");
        }
    }

    #[test]
    fn background_repeat_tiles() {
        let mut ops = Vec::new();
        let pixels = vec![255u8; 4 * 10 * 10];
        emit_background_image_ops(
            &mut ops, 0.0, 0.0, 30.0, 30.0,
            0.0, 0.0, 10.0, 10.0,
            10, 10, &pixels,
            &BgRepeat::Repeat,
        );
        // 30/10 = 3 tiles in each direction → 9 ops.
        assert_eq!(ops.len(), 9);
    }

    #[test]
    fn angled_linear_gradient_emits_draw_image() {
        let mut ops = Vec::new();
        paint_linear_gradient(
            "linear-gradient(45deg, red, blue)",
            0.0, 0.0, 100.0, 100.0, 1.0, &mut ops,
        );
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawImage { .. })),
            "45deg gradient should emit DrawImage"
        );
    }

    #[test]
    fn inline_svg_placeholder_when_not_in_images() {
        let layout = LayoutBox {
            x: 10.0, y: 10.0, width: 100.0, height: 100.0,
            content: LayoutContent::InlineSvg {
                markup: "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect fill=\"red\" width=\"100\" height=\"100\"/></svg>".into(),
            },
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());
        // Without the image in the map, should emit placeholder.
        assert!(ops.iter().any(|op| matches!(op, RenderOp::DrawText { text, .. } if text.contains("SVG"))));
    }

    #[test]
    fn inline_svg_renders_when_in_images() {
        let markup = "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect fill=\"red\" width=\"10\" height=\"10\"/></svg>";
        let key = format!("__inline_svg_{}", &markup[..markup.len().min(64)]);

        // Create fake decoded image data (10x10).
        let mut decoded = Vec::new();
        decoded.extend_from_slice(&10u32.to_le_bytes());
        decoded.extend_from_slice(&10u32.to_le_bytes());
        decoded.extend(vec![255u8; 10 * 10 * 4]);

        let mut images = HashMap::new();
        images.insert(key, decoded);

        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 100.0, height: 100.0,
            content: LayoutContent::InlineSvg { markup: markup.to_string() },
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &images, &HashMap::new());
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawImage { .. })),
            "inline SVG with image data should emit DrawImage"
        );
    }

    // --- Media element tests ---

    #[test]
    fn video_generates_background_and_play_button() {
        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 300.0, height: 150.0,
            content: LayoutContent::Video {
                src: "test.mp4".into(),
                poster: None,
                controls: false,
            },
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());

        // Should have a dark background rect.
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::FillRect { color, .. } if color.r < 0.1)),
            "video should have a dark background"
        );
        // Should have a play button text.
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawText { text, .. } if text.contains('\u{25B6}'))),
            "video should have a play button triangle"
        );
    }

    #[test]
    fn video_with_controls_generates_controls_bar() {
        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 300.0, height: 150.0,
            content: LayoutContent::Video {
                src: "test.mp4".into(),
                poster: None,
                controls: true,
            },
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());

        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawText { text, .. } if text.contains("0:00"))),
            "video with controls should have time display"
        );
    }

    #[test]
    fn audio_with_controls_generates_player_bar() {
        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 300.0, height: 40.0,
            content: LayoutContent::Audio {
                src: "test.mp3".into(),
                controls: true,
            },
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());

        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::FillRoundedRect { .. })),
            "audio should have a rounded player background"
        );
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawText { text, .. } if text.contains("0:00"))),
            "audio should have time display"
        );
    }

    #[test]
    fn audio_without_controls_generates_nothing() {
        let layout = LayoutBox {
            x: 0.0, y: 0.0, width: 300.0, height: 40.0,
            content: LayoutContent::Audio {
                src: "test.mp3".into(),
                controls: false,
            },
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        };
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &HashMap::new(), &HashMap::new());

        assert!(
            !ops.iter().any(|op| matches!(op, RenderOp::DrawText { text, .. } if text.contains("0:00"))),
            "audio without controls should not render controls"
        );
    }
}

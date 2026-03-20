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

/// Recursively paint a layout box into render operations.
fn paint_box(layout_box: &LayoutBox, ops: &mut Vec<RenderOp>, images: &HashMap<String, Vec<u8>>) {
    // Skip zero-sized boxes (display: none, comments, etc.).
    if layout_box.width <= 0.0 && layout_box.height <= 0.0 {
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

    // Check for CSS transform and wrap in Save/Translate/Restore if needed.
    let transform = extract_transform(&layout_box.style);
    let needs_translate = transform.as_ref().is_some_and(|t| t.has_translate());
    if needs_translate {
        let t = transform.as_ref().unwrap();
        ops.push(RenderOp::Save);
        ops.push(RenderOp::Translate { x: t.translate_x, y: t.translate_y });
    }

    // Determine opacity factor for this box (1.0 = fully opaque).
    let opacity = extract_opacity(&layout_box.style);

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

    // Paint text content.
    if let LayoutContent::Text(ref text) = layout_box.content {
        if !text.trim().is_empty() {
            let font_size = extract_font_size(&layout_box.style);
            let text_color = multiply_alpha(extract_text_color(&layout_box.style), opacity);
            let font_weight = extract_font_weight(&layout_box.style);
            let font_style = extract_font_style(&layout_box.style);
            let font_family = extract_font_family(&layout_box.style);
            let transformed_text = apply_text_transform(text, &layout_box.style);
            ops.push(RenderOp::DrawText {
                x: layout_box.x,
                y: layout_box.y + font_size, // baseline offset
                text: transformed_text.clone(),
                font_size,
                color: text_color,
                font_weight,
                font_style,
                font_family,
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
            });
        }
    }

    // Emit FormField op for interactive form elements.
    if let Some((field_type, value)) = extract_form_field(&layout_box.style) {
        ops.push(RenderOp::FormField {
            x: layout_box.x,
            y: layout_box.y,
            width: layout_box.width,
            height: layout_box.height,
            value,
            field_type,
        });
    }

    // Paint CSS borders if border-style is set.
    if let Some((border_width, border_color)) = extract_border(&layout_box.style) {
        if border_width > 0.0 {
            let border_color = multiply_alpha(border_color, opacity);
            ops.push(RenderOp::StrokeRect {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width,
                height: layout_box.height,
                color: border_color,
                width_px: border_width,
            });
        }
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
        paint_box(&layout_box.children[i], ops, images);
    }

    if clips_overflow {
        ops.push(RenderOp::PopClip);
    }

    if is_sticky {
        ops.push(RenderOp::StickyEnd);
    }

    if needs_translate {
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

/// Extract CSS border properties (width, color) from a style map.
///
/// Returns `Some((width_px, color))` if a visible border is declared.
fn extract_border(style: &nova_mod_api::content::StyleMap) -> Option<(f32, Color)> {
    let mut border_style: Option<String> = None;
    let mut border_width: Option<f32> = None;
    let mut border_color: Option<Color> = None;

    for (key, value) in &style.properties {
        match key.as_str() {
            "border-style" | "border-top-style" => {
                if let StyleValue::Keyword(k) | StyleValue::Str(k) = value {
                    if border_style.is_none() {
                        border_style = Some(k.clone());
                    }
                }
            }
            "border-width" | "border-top-width" => {
                if border_width.is_none() {
                    match value {
                        StyleValue::Px(px) => border_width = Some(*px),
                        StyleValue::Str(s) | StyleValue::Keyword(s) => {
                            border_width = parse_length_px(s);
                        }
                        _ => {}
                    }
                }
            }
            "border-color" | "border-top-color" => {
                if border_color.is_none() {
                    border_color = style_value_to_color(value);
                }
            }
            _ => {}
        }
    }

    let style_val = border_style.as_deref().unwrap_or("none");
    if style_val == "none" || style_val == "hidden" {
        return None;
    }

    let width = border_width.unwrap_or(1.0);
    let color = border_color.unwrap_or(Color::BLACK);
    Some((width, color))
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
}

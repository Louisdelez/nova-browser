//! # mod-painter
//!
//! NOVA Mod for painting — converts a `LayoutTree` into `RenderCommands`.
//! Handles the `Paint` capability.
//!
//! Walks the layout tree and generates `RenderOp`s: `FillRect` for element
//! backgrounds and `DrawText` for text content.

use std::sync::Arc;

use async_trait::async_trait;
use semver::Version;
use tracing::{debug, info};

use std::collections::HashMap;

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

                // Build a lookup map from src URL to decoded RGBA bytes.
                let images_map: HashMap<String, Vec<u8>> = images.into_iter().collect();

                let mut ops = Vec::new();
                paint_box(&root, &mut ops, &images_map);

                debug!(op_count = ops.len(), "painting complete");
                Ok(TypedData::RenderCommands(RenderCommands { ops }))
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

/// Recursively paint a layout box into render operations.
fn paint_box(layout_box: &LayoutBox, ops: &mut Vec<RenderOp>, images: &HashMap<String, Vec<u8>>) {
    // Skip zero-sized boxes (display: none, comments, etc.).
    if layout_box.width <= 0.0 && layout_box.height <= 0.0 {
        return;
    }

    // Paint the background if this is not a text node.
    let bg_color = extract_background_color(&layout_box.style);
    if bg_color.a > 0.0 {
        ops.push(RenderOp::FillRect {
            x: layout_box.x,
            y: layout_box.y,
            width: layout_box.width,
            height: layout_box.height,
            color: bg_color,
        });
    }

    // Paint text content.
    if let LayoutContent::Text(ref text) = layout_box.content {
        if !text.trim().is_empty() {
            let font_size = extract_font_size(&layout_box.style);
            let text_color = extract_text_color(&layout_box.style);
            ops.push(RenderOp::DrawText {
                x: layout_box.x,
                y: layout_box.y + font_size, // baseline offset
                text: text.clone(),
                font_size,
                color: text_color,
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
            // Decoded format: [width_u32_le][height_u32_le][RGBA pixels...]
            if decoded.len() >= 8 {
                let img_width =
                    u32::from_le_bytes([decoded[0], decoded[1], decoded[2], decoded[3]]);
                let img_height =
                    u32::from_le_bytes([decoded[4], decoded[5], decoded[6], decoded[7]]);
                let pixels = decoded[8..].to_vec();
                ops.push(RenderOp::DrawImage {
                    x: layout_box.x,
                    y: layout_box.y,
                    width: layout_box.width,
                    height: layout_box.height,
                    img_width,
                    img_height,
                    pixels,
                });
            }
        } else {
            // Fallback: gray placeholder when image data is not available.
            ops.push(RenderOp::FillRect {
                x: layout_box.x,
                y: layout_box.y,
                width: layout_box.width.max(150.0),
                height: layout_box.height.max(80.0),
                color: Color::rgb(0.85, 0.85, 0.85),
            });
            let label = src.rsplit('/').next().unwrap_or(src);
            let label = if label.len() > 40 {
                format!("{}...", &label[..37])
            } else {
                label.to_string()
            };
            ops.push(RenderOp::DrawText {
                x: layout_box.x + 4.0,
                y: layout_box.y + 16.0,
                text: format!("[img: {label}]"),
                font_size: 12.0,
                color: Color::rgb(0.4, 0.4, 0.4),
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

    // Recurse into children.
    for child in &layout_box.children {
        paint_box(child, ops, images);
    }
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
fn parse_color_string(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.starts_with('#') {
        parse_hex_color(s)
    } else if s.starts_with("rgb") {
        parse_rgb_color(s)
    } else {
        parse_named_color(s)
    }
}

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
        "blue" => Some(Color::rgb(0.0, 0.0, 1.0)),
        "yellow" => Some(Color::rgb(1.0, 1.0, 0.0)),
        "gray" | "grey" => Some(Color::rgb(0.502, 0.502, 0.502)),
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
        };

        let images = HashMap::new();
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &images);

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
        };

        let images = HashMap::new();
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &images);
        assert!(ops.is_empty());
    }

    #[test]
    fn paint_image_with_data() {
        // Create a 1x1 red pixel decoded image.
        let mut decoded = Vec::new();
        decoded.extend_from_slice(&1u32.to_le_bytes()); // width
        decoded.extend_from_slice(&1u32.to_le_bytes()); // height
        decoded.extend_from_slice(&[255, 0, 0, 255]);   // RGBA

        let layout = LayoutBox {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
            content: LayoutContent::Image {
                src: "https://example.com/img.png".into(),
            },
            style: StyleMap::default(),
            children: vec![],
        };

        let mut images = HashMap::new();
        images.insert("https://example.com/img.png".to_string(), decoded);

        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &images);

        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::DrawImage { .. })),
            "should generate a DrawImage op"
        );
    }

    #[test]
    fn paint_image_without_data_shows_placeholder() {
        let layout = LayoutBox {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
            content: LayoutContent::Image {
                src: "https://example.com/missing.png".into(),
            },
            style: StyleMap::default(),
            children: vec![],
        };

        let images = HashMap::new();
        let mut ops = Vec::new();
        paint_box(&layout, &mut ops, &images);

        // Should fall back to placeholder (FillRect + DrawText).
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::FillRect { .. })),
            "should generate a placeholder FillRect"
        );
    }
}

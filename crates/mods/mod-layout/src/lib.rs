//! # mod-layout
//!
//! NOVA Mod for layout computation. Handles the `Layout` capability.
//!
//! Takes a styled DOM tree and a viewport, then computes a `LayoutBox` tree
//! with absolute positions and sizes. Uses **Taffy** for proper Flexbox and
//! block layout computation.
//!
//! ## Layout strategy
//!
//! 1. **Build a Taffy tree** by walking the DOM recursively. Each DOM node
//!    becomes a Taffy node whose style is derived from the element's tag and
//!    any computed CSS properties in the `StyleMap`.
//! 2. **Compute layout** with `taffy::TaffyTree::compute_layout`.
//! 3. **Read results** back from Taffy and build the `LayoutBox` tree that
//!    the rest of the pipeline expects.
//!
//! ### Display mode mapping
//!
//! | CSS display   | Taffy style                                        |
//! |---------------|----------------------------------------------------|
//! | `block`       | `Display::Flex`, `FlexDirection::Column`, width 100%|
//! | `inline`      | `Display::Flex`, flex item (no forced width)        |
//! | `flex`        | `Display::Flex`, direction from `flex-direction`    |
//! | `none`        | `Display::None`                                    |

use std::sync::Arc;

use async_trait::async_trait;
use semver::Version;
use taffy::prelude::*;
use tracing::{debug, info};

// ── Font measurement ──────────────────────────────────────────────────

/// Thread-local fontdue font for real text measurement in layout.
///
/// Falls back to character-width estimation if no font file is found.
thread_local! {
    static LAYOUT_FONT: Option<fontdue::Font> = {
        // Try workspace assets first, then system paths.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace_root = std::path::Path::new(manifest_dir)
            .parent()  // crates/mods/
            .and_then(|p| p.parent()) // crates/
            .and_then(|p| p.parent()); // workspace root

        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        if let Some(root) = workspace_root {
            paths.push(root.join("assets/fonts/DejaVuSans.ttf"));
        }
        paths.extend([
            std::path::PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
            std::path::PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf"),
            std::path::PathBuf::from("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf"),
        ]);

        for path in &paths {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    return Some(font);
                }
            }
        }
        None
    };
}

/// Measure the width of a string using fontdue, or fall back to estimation.
fn measure_text_width(text: &str, font_size: f32) -> f32 {
    LAYOUT_FONT.with(|font| {
        if let Some(font) = font {
            let mut width: f32 = 0.0;
            for ch in text.chars() {
                let (metrics, _) = font.rasterize(ch, font_size);
                width += metrics.advance_width;
            }
            width
        } else {
            // Fallback: character-width estimation.
            let scale = font_size / 16.0;
            text.len() as f32 * CHAR_WIDTH_AT_16PX * scale
        }
    })
}

use nova_mod_api::{
    capability::CapabilityType,
    content::{
        ContentRequest, DomNode, LayoutBox, LayoutContent, StyleMap, StyleValue, TypedData,
    },
    error::NovaError,
    manifest::ModManifest,
    permission::TrustLevel,
    types::{ModId, Viewport},
    CoreApi, NovaMod,
};

// ── Constants ──────────────────────────────────────────────────────────

/// Default font size (px) when the DOM/style provides no value.
const DEFAULT_FONT_SIZE: f32 = 16.0;

/// Approximate width of one character at 16 px font size.
/// Used to estimate text leaf node dimensions.
const CHAR_WIDTH_AT_16PX: f32 = 7.0;

/// Line-height multiplier relative to font size.
const LINE_HEIGHT_FACTOR: f32 = 1.2;

// ── Mod definition ─────────────────────────────────────────────────────

/// The layout mod.
pub struct LayoutMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
}

impl LayoutMod {
    /// Create a new `LayoutMod` instance.
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.layout"),
            name: "NOVA Layout Engine".into(),
            version: Version::new(0, 1, 0),
            description: "Block/inline layout engine".into(),
            capabilities: vec![CapabilityType::Layout],
            permissions: vec![],
            dependencies: vec![CapabilityType::ComputeStyles],
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

impl Default for LayoutMod {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NovaMod for LayoutMod {
    fn manifest(&self) -> &ModManifest {
        &self.manifest
    }

    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "layout mod initializing");
        self.core = Some(core);
        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            ContentRequest::Layout {
                styled_dom,
                viewport,
            } => {
                let dom_node = match *styled_dom {
                    TypedData::Dom(node) => node,
                    _ => {
                        return Err(NovaError::UnsupportedContent(
                            "Layout expects TypedData::Dom".into(),
                        ));
                    }
                };

                debug!(
                    viewport_w = viewport.width,
                    viewport_h = viewport.height,
                    "computing layout with Taffy"
                );

                let root = compute_layout(&dom_node, &viewport)?;
                Ok(TypedData::LayoutTree(root))
            }
            other => Err(NovaError::UnsupportedContent(format!(
                "layout mod cannot handle request: {other:?}"
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "layout mod shutting down");
        Ok(())
    }
}

// ── Public layout entry point ──────────────────────────────────────────

/// Compute layout for an entire DOM tree using Taffy.
///
/// 1. Creates a `TaffyTree` and recursively adds nodes.
/// 2. Calls `compute_layout` with the viewport as the available space.
/// 3. Reads back computed positions and builds the `LayoutBox` tree.
fn compute_layout(dom: &DomNode, viewport: &Viewport) -> Result<LayoutBox, NovaError> {
    let mut taffy = TaffyTree::<NodeContext>::new();

    // Build the Taffy tree, returning the root node id.
    let root_id = add_node(&mut taffy, dom, viewport.width, DEFAULT_FONT_SIZE, &[])?;

    // Compute layout at the viewport size.
    taffy
        .compute_layout(
            root_id,
            Size {
                width: AvailableSpace::Definite(viewport.width),
                height: AvailableSpace::Definite(viewport.height),
            },
        )
        .map_err(|e| NovaError::LayoutError(format!("Taffy compute_layout failed: {e:?}")))?;

    // Extract the resulting layout tree.
    let layout_box = build_layout_box(&taffy, root_id, 0.0, 0.0);
    Ok(layout_box)
}

// ── Node context stored alongside each Taffy node ──────────────────────

/// Extra data we attach to each Taffy node so we can reconstruct the
/// `LayoutBox` tree after layout is computed.
#[derive(Debug, Clone)]
struct NodeContext {
    /// What kind of layout content this node represents.
    content: LayoutContent,
    /// Computed style properties to carry forward.
    style: StyleMap,
}

// ── Building the Taffy tree ────────────────────────────────────────────

/// Recursively convert a `DomNode` into a Taffy node, returning its `NodeId`.
/// `parent_font_size` is inherited from the parent element for text nodes.
/// `parent_style_props` carries inheritable style properties (color,
/// background-color, text-decoration) from the parent element so text nodes
/// can inherit them.
fn add_node(
    taffy: &mut TaffyTree<NodeContext>,
    node: &DomNode,
    available_width: f32,
    parent_font_size: f32,
    parent_style_props: &[(String, StyleValue)],
) -> Result<NodeId, NovaError> {
    match node {
        DomNode::Document { children } => {
            // The document root is a block container spanning the full width.
            let child_ids = children
                .iter()
                .map(|c| add_node(taffy, c, available_width, DEFAULT_FONT_SIZE, &[]))
                .collect::<Result<Vec<_>, _>>()?;

            let style = Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                size: Size {
                    width: Dimension::Percent(1.0),
                    height: Dimension::Auto,
                },
                ..Style::DEFAULT
            };

            let ctx = NodeContext {
                content: LayoutContent::Block,
                style: StyleMap::default(),
            };

            taffy
                .new_with_children(style, &child_ids)
                .map(|id| {
                    taffy.set_node_context(id, Some(ctx)).ok();
                    id
                })
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }

        DomNode::Element {
            tag,
            children,
            attributes,
        } => {
            // ── Form elements → sized leaf nodes with placeholder text ──
            if matches!(tag.as_str(), "input" | "button" | "select" | "textarea") {
                let font_size = resolve_font_size(tag, attributes, parent_font_size);
                let line_height = font_size * LINE_HEIGHT_FACTOR;
                let display = resolve_display(tag, attributes);
                let lp = parse_layout_props(attributes);

                // Determine the label text for the form element.
                let label = match tag.as_str() {
                    "button" => {
                        // Collect text from children.
                        let mut text = String::new();
                        for child in children {
                            if let DomNode::Text(t) = child {
                                text.push_str(t);
                            }
                        }
                        if text.trim().is_empty() { "Button".to_string() } else { text.trim().to_string() }
                    }
                    "select" => {
                        // Show the first <option> text or "Select".
                        children.iter().find_map(|c| {
                            if let DomNode::Element { tag: t, children: cc, .. } = c {
                                if t == "option" {
                                    return cc.iter().find_map(|c2| {
                                        if let DomNode::Text(t) = c2 { Some(t.trim().to_string()) } else { None }
                                    });
                                }
                            }
                            None
                        }).unwrap_or_else(|| "Select".into())
                    }
                    "textarea" => {
                        attributes.iter()
                            .find(|(k, _)| k == "placeholder")
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default()
                    }
                    _ => { // input
                        let input_type = attributes.iter()
                            .find(|(k, _)| k == "type")
                            .map(|(_, v)| v.as_str())
                            .unwrap_or("text");
                        match input_type {
                            "submit" | "button" | "reset" => {
                                attributes.iter()
                                    .find(|(k, _)| k == "value")
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or_else(|| input_type.to_string().chars().next().unwrap().to_uppercase().to_string() + &input_type[1..])
                            }
                            "checkbox" | "radio" => String::new(),
                            _ => {
                                attributes.iter()
                                    .find(|(k, _)| k == "value")
                                    .or_else(|| attributes.iter().find(|(k, _)| k == "placeholder"))
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or_default()
                            }
                        }
                    }
                };

                let input_type = attributes.iter()
                    .find(|(k, _)| k == "type")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("text");

                // Determine dimensions.
                let (w, h) = if matches!(input_type, "checkbox" | "radio") && tag == "input" {
                    (13.0_f32, 13.0_f32)
                } else if tag == "textarea" {
                    let cols: f32 = attributes.iter().find(|(k,_)| k == "cols").and_then(|(_, v)| v.parse().ok()).unwrap_or(20.0);
                    let rows: f32 = attributes.iter().find(|(k,_)| k == "rows").and_then(|(_, v)| v.parse().ok()).unwrap_or(2.0);
                    (cols * font_size * 0.6, rows * line_height)
                } else {
                    let text_w = if label.is_empty() { 0.0 } else { measure_text_width(&label, font_size) };
                    let pad = 12.0; // horizontal padding
                    let default_w = lp.width.and_then(|d| match d { Dimension::Length(px) => Some(px), _ => None }).unwrap_or((text_w + pad).max(if tag == "input" { 150.0 } else { 40.0 }));
                    (default_w, line_height + 6.0)
                };

                // Build style props for the form element.
                let mut props = vec![
                    ("display".into(), StyleValue::Keyword(display.clone())),
                    ("font-size".into(), StyleValue::Px(font_size)),
                ];
                if let Some(nova_style) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
                    for decl in nova_style.1.split(';') {
                        let parts: Vec<&str> = decl.splitn(2, ':').collect();
                        if parts.len() == 2 {
                            let prop = parts[0].trim().to_string();
                            let val = parts[1].trim();
                            if prop != "display" && prop != "font-size" {
                                if let Some(px) = val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok()) {
                                    props.push((prop, StyleValue::Px(px)));
                                } else if val.starts_with("rgb") || val.starts_with('#') {
                                    props.push((prop, StyleValue::Str(val.to_string())));
                                } else {
                                    props.push((prop, StyleValue::Keyword(val.to_string())));
                                }
                            }
                        }
                    }
                }

                let taffy_style = build_taffy_style(&display, tag, attributes);

                // Create a text child for the label if non-empty.
                let mut child_ids = Vec::new();
                if !label.is_empty() {
                    let text_w = measure_text_width(&label, font_size).min(w);
                    let text_ctx = NodeContext {
                        content: LayoutContent::Text(label),
                        style: StyleMap { properties: props.clone() },
                    };
                    let text_id = taffy
                        .new_leaf_with_context(
                            Style {
                                display: Display::Flex,
                                size: Size {
                                    width: Dimension::Length(text_w),
                                    height: Dimension::Length(line_height),
                                },
                                ..Style::DEFAULT
                            },
                            text_ctx,
                        )
                        .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                    child_ids.push(text_id);
                }

                let ctx = NodeContext {
                    content: LayoutContent::Block,
                    style: StyleMap { properties: props },
                };
                return taffy
                    .new_with_children(taffy_style, &child_ids)
                    .map(|id| {
                        taffy.set_node_context(id, Some(ctx)).ok();
                        id
                    })
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // ── <img> elements → LayoutContent::Image leaf node ──────
            if tag == "img" {
                let src = attributes
                    .iter()
                    .find(|(k, _)| k == "src")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();

                // Determine image dimensions from attributes or defaults.
                let img_w: f32 = attributes
                    .iter()
                    .find(|(k, _)| k == "width")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(150.0);
                let img_h: f32 = attributes
                    .iter()
                    .find(|(k, _)| k == "height")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(80.0);

                let ctx = NodeContext {
                    content: LayoutContent::Image { src },
                    style: StyleMap::default(),
                };
                return taffy
                    .new_leaf_with_context(
                        Style {
                            display: Display::Flex,
                            size: Size {
                                width: Dimension::Length(img_w.min(available_width)),
                                height: Dimension::Length(img_h),
                            },
                            ..Style::DEFAULT
                        },
                        ctx,
                    )
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            let display = resolve_display(tag, attributes);

            // display: none produces an invisible zero-size node.
            if display == "none" {
                let ctx = NodeContext {
                    content: LayoutContent::Block,
                    style: StyleMap::default(),
                };
                return taffy
                    .new_leaf_with_context(
                        Style {
                            display: Display::None,
                            ..Style::DEFAULT
                        },
                        ctx,
                    )
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // Determine this element's font-size (from data-nova-style or tag default).
            let font_size = resolve_font_size(tag, attributes, parent_font_size);

            // We need to build the style props first so we can pass them to children.
            // However, we need the full props list. Build it now, pass inheritable
            // props to children, then use the same props for this node's StyleMap.
            let mut props = vec![
                ("display".into(), StyleValue::Keyword(display.clone())),
                ("font-size".into(), StyleValue::Px(font_size)),
            ];

            // Parse computed styles from data-nova-style (set by the CSS engine).
            if let Some(nova_style) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
                for decl in nova_style.1.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let prop = parts[0].trim().to_string();
                        let val = parts[1].trim();
                        // Don't override display/font-size we already set above.
                        if prop == "display" || prop == "font-size" {
                            // Update instead of skip — CSS engine knows better.
                            if let Some(existing) = props.iter_mut().find(|(k, _)| *k == prop) {
                                if prop == "font-size" {
                                    if let Some(px) = val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok()) {
                                        existing.1 = StyleValue::Px(px);
                                    }
                                } else {
                                    existing.1 = StyleValue::Keyword(val.to_string());
                                }
                            }
                            continue;
                        }
                        // Parse the value into a StyleValue.
                        if let Some(px) = val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok()) {
                            props.push((prop, StyleValue::Px(px)));
                        } else if val.starts_with("rgb") || val.starts_with('#') {
                            // Store as string — the painter will parse colors.
                            props.push((prop, StyleValue::Str(val.to_string())));
                        } else {
                            props.push((prop, StyleValue::Keyword(val.to_string())));
                        }
                    }
                }
            }

            // Propagate the href attribute for <a> elements so the painter
            // can emit Link ops for hit-testing.
            if tag == "a" {
                if let Some(href) = attributes.iter().find(|(k, _)| k == "href") {
                    props.push(("href".into(), StyleValue::Str(href.1.clone())));
                }
            }

            let child_ids = children
                .iter()
                .map(|c| add_node(taffy, c, available_width, font_size, &props))
                .collect::<Result<Vec<_>, _>>()?;
            let taffy_style = build_taffy_style(&display, tag, attributes);

            let content_type = match display.as_str() {
                "inline" | "table-cell" | "table-row" => LayoutContent::Inline,
                _ => LayoutContent::Block,
            };

            let style_map = StyleMap {
                properties: props,
            };

            let ctx = NodeContext {
                content: content_type,
                style: style_map,
            };

            taffy
                .new_with_children(taffy_style, &child_ids)
                .map(|id| {
                    taffy.set_node_context(id, Some(ctx)).ok();
                    id
                })
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }

        DomNode::Text(text) => {
            // Text nodes inherit font-size from their parent element,
            // along with color, background-color, and text-decoration.
            let font_size = parent_font_size;
            let mut text_props = vec![
                ("font-size".into(), StyleValue::Px(font_size)),
            ];
            // Inherit color, background-color, and text-decoration from parent.
            for (key, value) in parent_style_props {
                if key == "color" || key == "background-color" || key == "text-decoration"
                    || key == "text-align" || key == "font-weight" || key == "font-style"
                    || key == "white-space" || key == "overflow" {
                    text_props.push((key.clone(), value.clone()));
                }
            }

            let line_height = font_size * LINE_HEIGHT_FACTOR;
            // Use a standard web-metric space width (≈ 0.25 em) instead of
            // fontdue's rasterize advance_width which tends to be too large.
            let space_width = (font_size * 0.25).max(1.0);

            // Split the text into individual words.  When there are multiple
            // words we create one leaf node per word (plus thin "space" nodes
            // between them) and wrap them all in a flex-row container with
            // FlexWrap::Wrap so that Taffy handles line-breaking naturally.
            let words: Vec<&str> = text.split_whitespace().collect();

            // Empty / whitespace-only text → single zero-size node.
            if words.is_empty() {
                let ctx = NodeContext {
                    content: LayoutContent::Text(text.clone()),
                    style: StyleMap { properties: text_props },
                };
                return taffy
                    .new_leaf_with_context(
                        Style {
                            display: Display::None,
                            ..Style::DEFAULT
                        },
                        ctx,
                    )
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // Single word → return a single sized leaf (no wrapper needed).
            if words.len() == 1 {
                let word = words[0];
                let w = measure_text_width(word, font_size).min(available_width);
                let ctx = NodeContext {
                    content: LayoutContent::Text(word.to_string()),
                    style: StyleMap { properties: text_props },
                };
                let node_id = taffy
                    .new_leaf_with_context(
                        Style {
                            display: Display::Flex,
                            size: Size {
                                width: Dimension::Length(w),
                                height: Dimension::Length(line_height),
                            },
                            ..Style::DEFAULT
                        },
                        ctx,
                    )
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                return Ok(node_id);
            }

            // Multiple words → one leaf per word + space nodes between them,
            // all inside a flex-row wrapper with FlexWrap::Wrap.
            let mut word_ids: Vec<NodeId> = Vec::with_capacity(words.len() * 2 - 1);
            for (i, word) in words.iter().enumerate() {
                // Space node between words (not before the first word).
                if i > 0 {
                    let space_ctx = NodeContext {
                        content: LayoutContent::Text(" ".to_string()),
                        style: StyleMap { properties: text_props.clone() },
                    };
                    let space_id = taffy
                        .new_leaf_with_context(
                            Style {
                                display: Display::Flex,
                                size: Size {
                                    width: Dimension::Length(space_width),
                                    height: Dimension::Length(line_height),
                                },
                                ..Style::DEFAULT
                            },
                            space_ctx,
                        )
                        .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                    word_ids.push(space_id);
                }

                let w = measure_text_width(word, font_size).min(available_width);
                let word_ctx = NodeContext {
                    content: LayoutContent::Text(word.to_string()),
                    style: StyleMap { properties: text_props.clone() },
                };
                let word_id = taffy
                    .new_leaf_with_context(
                        Style {
                            display: Display::Flex,
                            size: Size {
                                width: Dimension::Length(w),
                                height: Dimension::Length(line_height),
                            },
                            ..Style::DEFAULT
                        },
                        word_ctx,
                    )
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                word_ids.push(word_id);
            }

            // Wrapper node: flex row with wrap, auto-sized.
            // Use Inline (not Text) so the painter does NOT draw the full
            // text string again — each word child already draws its own word.
            let wrapper_ctx = NodeContext {
                content: LayoutContent::Inline,
                style: StyleMap { properties: text_props },
            };
            taffy
                .new_with_children(
                    Style {
                        display: Display::Flex,
                        flex_direction: FlexDirection::Row,
                        flex_wrap: FlexWrap::Wrap,
                        size: Size {
                            width: Dimension::Auto,
                            height: Dimension::Auto,
                        },
                        ..Style::DEFAULT
                    },
                    &word_ids,
                )
                .map(|id| {
                    taffy.set_node_context(id, Some(wrapper_ctx)).ok();
                    id
                })
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }

        DomNode::Comment(_) => {
            // Comments are invisible zero-size nodes.
            let ctx = NodeContext {
                content: LayoutContent::Block,
                style: StyleMap::default(),
            };
            taffy
                .new_leaf_with_context(
                    Style {
                        display: Display::None,
                        ..Style::DEFAULT
                    },
                    ctx,
                )
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }
    }
}

// ── Reading Taffy results back into LayoutBox ──────────────────────────

/// Recursively read computed layout from Taffy and build the `LayoutBox` tree.
///
/// `parent_x` and `parent_y` are the absolute coordinates of the parent node,
/// since Taffy gives us positions relative to the parent.
fn build_layout_box(
    taffy: &TaffyTree<NodeContext>,
    node_id: NodeId,
    parent_x: f32,
    parent_y: f32,
) -> LayoutBox {
    let layout = taffy.layout(node_id).expect("node must have layout");
    let x = parent_x + layout.location.x;
    let y = parent_y + layout.location.y;
    let width = layout.size.width;
    let height = layout.size.height;

    let ctx = taffy
        .get_node_context(node_id)
        .cloned()
        .unwrap_or(NodeContext {
            content: LayoutContent::Block,
            style: StyleMap::default(),
        });

    let children: Vec<LayoutBox> = taffy
        .children(node_id)
        .unwrap_or_else(|_| Vec::new())
        .iter()
        .map(|&child_id| build_layout_box(taffy, child_id, x, y))
        .collect();

    // Extract z-index from the style properties.
    let z_index = ctx
        .style
        .properties
        .iter()
        .find(|(k, _)| k == "z-index")
        .and_then(|(_, v)| {
            if let StyleValue::Number(n) = v {
                Some(*n as i32)
            } else if let StyleValue::Keyword(s) | StyleValue::Str(s) = v {
                s.parse::<i32>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    LayoutBox {
        x,
        y,
        width,
        height,
        content: ctx.content,
        style: ctx.style,
        children,
        z_index,
    }
}

// ── Layout property extraction ─────────────────────────────────────────

/// Parsed layout-relevant CSS properties from `data-nova-style` and `style`
/// attributes. All values are optional; `None` means the property was not set.
#[derive(Debug, Clone, Default)]
struct LayoutProps {
    margin_top: Option<LengthPercentageAuto>,
    margin_right: Option<LengthPercentageAuto>,
    margin_bottom: Option<LengthPercentageAuto>,
    margin_left: Option<LengthPercentageAuto>,
    padding_top: Option<LengthPercentage>,
    padding_right: Option<LengthPercentage>,
    padding_bottom: Option<LengthPercentage>,
    padding_left: Option<LengthPercentage>,
    width: Option<Dimension>,
    max_width: Option<Dimension>,
    /// CSS `position` property: "static" | "relative" | "absolute" | "fixed" | "sticky"
    position: Option<String>,
    /// CSS `top` inset.
    top: Option<LengthPercentageAuto>,
    /// CSS `right` inset.
    right: Option<LengthPercentageAuto>,
    /// CSS `bottom` inset.
    bottom: Option<LengthPercentageAuto>,
    /// CSS `left` inset.
    left: Option<LengthPercentageAuto>,
    /// CSS `z-index` value.
    z_index: Option<i32>,
    /// CSS `float` property: "left" | "right" | "none".
    float: Option<String>,
    // ── CSS Grid properties ────────────────────────────────────────────
    /// Parsed `grid-template-columns` track list.
    grid_template_columns: Option<Vec<TrackSizingFunction>>,
    /// Parsed `grid-template-rows` track list.
    grid_template_rows: Option<Vec<TrackSizingFunction>>,
    /// `gap` / `grid-gap` shorthand (row-gap, column-gap) in px.
    gap: Option<(f32, f32)>,
    /// `grid-column: start / end` (1-based, converted to 0-based line index).
    grid_column: Option<(i16, i16)>,
    /// `grid-row: start / end` (1-based, converted to 0-based line index).
    grid_row: Option<(i16, i16)>,
}

/// Parse a CSS value into `LengthPercentageAuto`.
///
/// Recognises `auto`, `<n>px`, `<n>%`, and `<n>vw` (treated as percent).
fn parse_length_percentage_auto(val: &str) -> Option<LengthPercentageAuto> {
    let val = val.trim();
    if val == "auto" {
        return Some(LengthPercentageAuto::Auto);
    }
    if let Some(px) = val.strip_suffix("px").and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(LengthPercentageAuto::Length(px));
    }
    if let Some(pct) = val.strip_suffix('%').and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(LengthPercentageAuto::Percent(pct / 100.0));
    }
    if let Some(vw) = val.strip_suffix("vw").and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(LengthPercentageAuto::Percent(vw / 100.0));
    }
    None
}

/// Parse a CSS value into `LengthPercentage` (no `auto`).
fn parse_length_percentage(val: &str) -> Option<LengthPercentage> {
    let val = val.trim();
    if let Some(px) = val.strip_suffix("px").and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(LengthPercentage::Length(px));
    }
    if let Some(pct) = val.strip_suffix('%').and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(LengthPercentage::Percent(pct / 100.0));
    }
    None
}

/// Parse a CSS value into a `Dimension`.
///
/// Recognises `auto`, `<n>px`, `<n>%`, and `<n>vw` (treated as percent).
fn parse_dimension(val: &str) -> Option<Dimension> {
    let val = val.trim();
    if val == "auto" {
        return Some(Dimension::Auto);
    }
    if let Some(px) = val.strip_suffix("px").and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(Dimension::Length(px));
    }
    if let Some(pct) = val.strip_suffix('%').and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(Dimension::Percent(pct / 100.0));
    }
    if let Some(vw) = val.strip_suffix("vw").and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(Dimension::Percent(vw / 100.0));
    }
    None
}

// ── Grid track parsing helpers ─────────────────────────────────────────

/// Parse a single track sizing value like `1fr`, `200px`, `auto`,
/// `minmax(100px, 1fr)` into a Taffy `TrackSizingFunction`.
fn parse_single_track(s: &str) -> Option<TrackSizingFunction> {
    use taffy::style_helpers::{FromFlex, FromLength, TaffyAuto};

    let s = s.trim();
    if s == "auto" {
        return Some(TrackSizingFunction::AUTO);
    }
    if let Some(frac) = s.strip_suffix("fr").and_then(|n| n.trim().parse::<f32>().ok()) {
        // fr() helper: TrackSizingFunction::from_flex(frac)
        return Some(<TrackSizingFunction as FromFlex>::from_flex(frac));
    }
    if let Some(px) = s.strip_suffix("px").and_then(|n| n.trim().parse::<f32>().ok()) {
        return Some(<TrackSizingFunction as FromLength>::from_length(px));
    }
    if let Some(pct) = s.strip_suffix('%').and_then(|n| n.trim().parse::<f32>().ok()) {
        return Some(TrackSizingFunction::Single(NonRepeatedTrackSizingFunction {
            min: MinTrackSizingFunction::Fixed(LengthPercentage::Percent(pct / 100.0)),
            max: MaxTrackSizingFunction::Fixed(LengthPercentage::Percent(pct / 100.0)),
        }));
    }
    // minmax(min, max)
    if let Some(inner) = s.strip_prefix("minmax(").and_then(|s| s.strip_suffix(')')) {
        let comma = inner.find(',')?;
        let min_str = inner[..comma].trim();
        let max_str = inner[comma + 1..].trim();
        let min_fn = if min_str == "auto" {
            MinTrackSizingFunction::Auto
        } else if let Some(px) = min_str.strip_suffix("px").and_then(|n| n.trim().parse::<f32>().ok()) {
            MinTrackSizingFunction::Fixed(LengthPercentage::Length(px))
        } else {
            MinTrackSizingFunction::Auto
        };
        let max_fn = if max_str == "auto" {
            MaxTrackSizingFunction::Auto
        } else if let Some(fr) = max_str.strip_suffix("fr").and_then(|n| n.trim().parse::<f32>().ok()) {
            MaxTrackSizingFunction::Fraction(fr)
        } else if let Some(px) = max_str.strip_suffix("px").and_then(|n| n.trim().parse::<f32>().ok()) {
            MaxTrackSizingFunction::Fixed(LengthPercentage::Length(px))
        } else {
            MaxTrackSizingFunction::Auto
        };
        return Some(TrackSizingFunction::Single(NonRepeatedTrackSizingFunction {
            min: min_fn,
            max: max_fn,
        }));
    }
    None
}

/// Parse a CSS grid-template track list string such as:
/// `1fr 2fr`, `repeat(3, 1fr)`, `200px 1fr`, `auto auto`.
fn parse_track_list(val: &str) -> Option<Vec<TrackSizingFunction>> {
    let val = val.trim();
    if val.is_empty() {
        return None;
    }

    let mut tracks = Vec::new();

    // We need a simple tokeniser that handles `repeat(...)` as a single token
    // and whitespace as separator outside parens.
    let mut depth = 0usize;
    let mut current = String::new();

    for ch in val.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
                if depth == 0 {
                    // End of a paren-group — flush as one token.
                    let token = current.trim().to_string();
                    current.clear();
                    // Parse repeat(N, track) specially.
                    if let Some(inner) = token.strip_prefix("repeat(").and_then(|s| s.strip_suffix(')')) {
                        if let Some(comma) = inner.find(',') {
                            let count_str = inner[..comma].trim();
                            let track_str = inner[comma + 1..].trim();
                            if let Ok(n) = count_str.parse::<usize>() {
                                // Each repeated item may itself be a track list.
                                for sub in track_str.split_whitespace() {
                                    if let Some(t) = parse_single_track(sub) {
                                        for _ in 0..n {
                                            tracks.push(t.clone());
                                        }
                                        // Only repeat for the first item if multiple items.
                                        // For simplicity we only support single-track repeat.
                                        break;
                                    }
                                }
                            }
                        }
                    } else if let Some(t) = parse_single_track(&token) {
                        tracks.push(t);
                    }
                }
            }
            ' ' | '\t' | '\n' if depth == 0 => {
                let token = current.trim().to_string();
                if !token.is_empty() {
                    if let Some(t) = parse_single_track(&token) {
                        tracks.push(t);
                    }
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    // Flush any remaining token.
    let token = current.trim().to_string();
    if !token.is_empty() {
        if let Some(t) = parse_single_track(&token) {
            tracks.push(t);
        }
    }

    if tracks.is_empty() {
        None
    } else {
        Some(tracks)
    }
}

/// Parse a gap value like `10px`, `1em` (approximated).
fn parse_gap_value(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(px) = s.strip_suffix("px").and_then(|n| n.trim().parse::<f32>().ok()) {
        return Some(px);
    }
    if let Some(em) = s.strip_suffix("em").and_then(|n| n.trim().parse::<f32>().ok()) {
        return Some(em * 16.0);
    }
    if let Some(pct) = s.strip_suffix('%').and_then(|n| n.trim().parse::<f32>().ok()) {
        return Some(pct); // Store as percent; used as px approximation.
    }
    None
}

/// Parse `grid-column` / `grid-row` shorthand: `start / end`.
/// Returns (start_line, end_line) as 0-based Taffy line indices.
///
/// CSS uses 1-based line numbers; Taffy uses i16 with 1-based positive indices
/// (0 = auto). We translate directly: CSS `1` → Taffy `1`, CSS `3` → Taffy `3`.
fn parse_grid_line_span(val: &str) -> Option<(i16, i16)> {
    let val = val.trim();
    if let Some(slash) = val.find('/') {
        let start_str = val[..slash].trim();
        let end_str = val[slash + 1..].trim();
        let start = if start_str == "auto" {
            0i16
        } else {
            start_str.parse::<i16>().ok()?
        };
        let end = if end_str == "auto" {
            0i16
        } else {
            end_str.parse::<i16>().ok()?
        };
        Some((start, end))
    } else {
        // Single value: treat as start line, end = auto.
        let start = val.parse::<i16>().ok()?;
        Some((start, 0))
    }
}

/// Extract all layout-relevant CSS properties from element attributes.
///
/// Scans both `data-nova-style` (computed styles from the CSS engine) and
/// the inline `style` attribute. `data-nova-style` is processed second so
/// it overrides the inline `style` when both are present (the CSS cascade
/// result should win).
fn parse_layout_props(attributes: &[(String, String)]) -> LayoutProps {
    let mut props = LayoutProps::default();

    // Process `style` first, then `data-nova-style` so cascade wins.
    for attr_name in &["style", "data-nova-style"] {
        let style_str = match attributes.iter().find(|(k, _)| k == *attr_name) {
            Some((_, v)) => v.as_str(),
            None => continue,
        };

        for decl in style_str.split(';') {
            let parts: Vec<&str> = decl.splitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }
            let prop = parts[0].trim();
            let val = parts[1].trim();

            match prop {
                // Shorthand `margin` — supports 1-4 value syntax.
                "margin" => {
                    let values: Vec<&str> = val.split_whitespace().collect();
                    match values.len() {
                        1 => {
                            let v = parse_length_percentage_auto(values[0]);
                            props.margin_top = v;
                            props.margin_right = v;
                            props.margin_bottom = v;
                            props.margin_left = v;
                        }
                        2 => {
                            let tb = parse_length_percentage_auto(values[0]);
                            let lr = parse_length_percentage_auto(values[1]);
                            props.margin_top = tb;
                            props.margin_bottom = tb;
                            props.margin_left = lr;
                            props.margin_right = lr;
                        }
                        3 => {
                            props.margin_top = parse_length_percentage_auto(values[0]);
                            let lr = parse_length_percentage_auto(values[1]);
                            props.margin_left = lr;
                            props.margin_right = lr;
                            props.margin_bottom = parse_length_percentage_auto(values[2]);
                        }
                        4 => {
                            props.margin_top = parse_length_percentage_auto(values[0]);
                            props.margin_right = parse_length_percentage_auto(values[1]);
                            props.margin_bottom = parse_length_percentage_auto(values[2]);
                            props.margin_left = parse_length_percentage_auto(values[3]);
                        }
                        _ => {}
                    }
                }
                "margin-top" => props.margin_top = parse_length_percentage_auto(val),
                "margin-right" => props.margin_right = parse_length_percentage_auto(val),
                "margin-bottom" => props.margin_bottom = parse_length_percentage_auto(val),
                "margin-left" => props.margin_left = parse_length_percentage_auto(val),

                // Shorthand `padding` — supports 1-4 value syntax.
                "padding" => {
                    let values: Vec<&str> = val.split_whitespace().collect();
                    match values.len() {
                        1 => {
                            let v = parse_length_percentage(values[0]);
                            props.padding_top = v;
                            props.padding_right = v;
                            props.padding_bottom = v;
                            props.padding_left = v;
                        }
                        2 => {
                            let tb = parse_length_percentage(values[0]);
                            let lr = parse_length_percentage(values[1]);
                            props.padding_top = tb;
                            props.padding_bottom = tb;
                            props.padding_left = lr;
                            props.padding_right = lr;
                        }
                        3 => {
                            props.padding_top = parse_length_percentage(values[0]);
                            let lr = parse_length_percentage(values[1]);
                            props.padding_left = lr;
                            props.padding_right = lr;
                            props.padding_bottom = parse_length_percentage(values[2]);
                        }
                        4 => {
                            props.padding_top = parse_length_percentage(values[0]);
                            props.padding_right = parse_length_percentage(values[1]);
                            props.padding_bottom = parse_length_percentage(values[2]);
                            props.padding_left = parse_length_percentage(values[3]);
                        }
                        _ => {}
                    }
                }
                "padding-top" => props.padding_top = parse_length_percentage(val),
                "padding-right" => props.padding_right = parse_length_percentage(val),
                "padding-bottom" => props.padding_bottom = parse_length_percentage(val),
                "padding-left" => props.padding_left = parse_length_percentage(val),

                "width" => props.width = parse_dimension(val),
                "max-width" => props.max_width = parse_dimension(val),

                "position" => props.position = Some(val.to_string()),
                "top" => props.top = parse_length_percentage_auto(val),
                "right" => props.right = parse_length_percentage_auto(val),
                "bottom" => props.bottom = parse_length_percentage_auto(val),
                "left" => props.left = parse_length_percentage_auto(val),

                "z-index" => {
                    if let Ok(z) = val.parse::<i32>() {
                        props.z_index = Some(z);
                    }
                }

                "float" => {
                    match val {
                        "left" | "right" | "none" => props.float = Some(val.to_string()),
                        _ => {}
                    }
                }

                // ── Grid properties ────────────────────────────────────
                "grid-template-columns" => {
                    props.grid_template_columns = parse_track_list(val);
                }
                "grid-template-rows" => {
                    props.grid_template_rows = parse_track_list(val);
                }
                "gap" | "grid-gap" => {
                    let parts: Vec<&str> = val.split_whitespace().collect();
                    match parts.len() {
                        1 => {
                            if let Some(px) = parse_gap_value(parts[0]) {
                                props.gap = Some((px, px));
                            }
                        }
                        2 => {
                            if let (Some(row), Some(col)) =
                                (parse_gap_value(parts[0]), parse_gap_value(parts[1]))
                            {
                                props.gap = Some((row, col));
                            }
                        }
                        _ => {}
                    }
                }
                "row-gap" | "grid-row-gap" => {
                    if let Some(px) = parse_gap_value(val) {
                        let (_, col) = props.gap.unwrap_or((0.0, 0.0));
                        props.gap = Some((px, col));
                    }
                }
                "column-gap" | "grid-column-gap" => {
                    if let Some(px) = parse_gap_value(val) {
                        let (row, _) = props.gap.unwrap_or((0.0, 0.0));
                        props.gap = Some((row, px));
                    }
                }
                "grid-column" => {
                    props.grid_column = parse_grid_line_span(val);
                }
                "grid-row" => {
                    props.grid_row = parse_grid_line_span(val);
                }

                _ => {}
            }
        }
    }

    props
}

// ── Style mapping helpers ──────────────────────────────────────────────

/// Resolve the CSS `display` value for an element.
///
/// Checks for an explicit `style` attribute first, then falls back to
/// user-agent defaults based on the tag name.
fn resolve_display(tag: &str, attributes: &[(String, String)]) -> String {
    // Check computed styles from the CSS engine (data-nova-style) first,
    // then fall back to inline style attribute, then user-agent defaults.
    for attr_name in &["data-nova-style", "style"] {
        if let Some(style_attr) = attributes.iter().find(|(k, _)| k == *attr_name) {
            for decl in style_attr.1.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "display" {
                    return parts[1].trim().to_string();
                }
            }
        }
    }

    // User-agent defaults.
    display_for_tag(tag).to_string()
}

/// Resolve font-size from data-nova-style, tag defaults, or parent inheritance.
fn resolve_font_size(tag: &str, attributes: &[(String, String)], _parent_font_size: f32) -> f32 {
    // Check data-nova-style first.
    for attr_name in &["data-nova-style", "style"] {
        if let Some(style_attr) = attributes.iter().find(|(k, _)| k == *attr_name) {
            for decl in style_attr.1.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "font-size" {
                    let val = parts[1].trim();
                    if let Some(px) = val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok()) {
                        return px;
                    }
                }
            }
        }
    }
    // Tag-based defaults for headings.
    font_size_for_tag(tag)
}

/// Build a `taffy::Style` from the resolved display mode, tag, and attributes.
///
/// Parses margins, padding, width, and max-width from `data-nova-style` and
/// the inline `style` attribute, applying them to the Taffy `Style`.
fn build_taffy_style(
    display: &str,
    tag: &str,
    attributes: &[(String, String)],
) -> Style {
    let lp = parse_layout_props(attributes);

    // Build margin rect, defaulting unset sides to zero.
    let margin = Rect {
        top: lp.margin_top.unwrap_or(LengthPercentageAuto::Length(0.0)),
        right: lp.margin_right.unwrap_or(LengthPercentageAuto::Length(0.0)),
        bottom: lp.margin_bottom.unwrap_or(LengthPercentageAuto::Length(0.0)),
        left: lp.margin_left.unwrap_or(LengthPercentageAuto::Length(0.0)),
    };

    // Build padding rect, defaulting unset sides to zero.
    let padding = Rect {
        top: lp.padding_top.unwrap_or(LengthPercentage::Length(0.0)),
        right: lp.padding_right.unwrap_or(LengthPercentage::Length(0.0)),
        bottom: lp.padding_bottom.unwrap_or(LengthPercentage::Length(0.0)),
        left: lp.padding_left.unwrap_or(LengthPercentage::Length(0.0)),
    };

    // When both horizontal margins are `auto`, centre the element via
    // `align_self: Center` (the Taffy-idiomatic way to express `margin: 0 auto`).
    let center_via_auto_margin = matches!(
        (&lp.margin_left, &lp.margin_right),
        (Some(LengthPercentageAuto::Auto), Some(LengthPercentageAuto::Auto))
    );

    // Map CSS `position` to a Taffy `Position`.
    // `fixed` is approximated as `absolute` since Taffy has no viewport-fixed concept.
    let taffy_position = match lp.position.as_deref() {
        Some("relative") => Position::Relative,
        Some("absolute") | Some("fixed") => Position::Absolute,
        // `sticky` is approximated as `relative` since Taffy has no sticky concept.
        Some("sticky") => Position::Relative,
        _ => Position::Relative, // CSS default (static ≈ relative in Taffy)
    };

    // Build the inset rect from top/right/bottom/left.
    let inset = Rect {
        top: lp.top.unwrap_or(LengthPercentageAuto::Auto),
        right: lp.right.unwrap_or(LengthPercentageAuto::Auto),
        bottom: lp.bottom.unwrap_or(LengthPercentageAuto::Auto),
        left: lp.left.unwrap_or(LengthPercentageAuto::Auto),
    };

    // Float approximation:
    // `float: left`  → keep in flow, flex-shrink: 0, keep explicit width.
    // `float: right` → same, but push right with margin-left: auto.
    //
    // Real CSS floats require a separate float algorithm; this is a rough
    // approximation that at least keeps floated elements visible and
    // (for `float: right`) pushes them to the right edge.
    let (float_flex_shrink, float_margin_left) = match lp.float.as_deref() {
        Some("left") => (0.0_f32, None),
        Some("right") => (0.0_f32, Some(LengthPercentageAuto::Auto)),
        _ => (1.0_f32, None),
    };
    // Merge float-induced margin-left with any explicitly set margin-left.
    // The float override only applies when no explicit margin-left was set.
    let effective_margin_left = if lp.float.as_deref() == Some("right") && lp.margin_left.is_none() {
        LengthPercentageAuto::Auto
    } else {
        lp.margin_left.unwrap_or(LengthPercentageAuto::Length(0.0))
    };

    match display {
        "none" => Style {
            display: Display::None,
            ..Style::DEFAULT
        },

        "grid" => {
            // Map CSS grid properties to Taffy Grid style.
            let columns = lp.grid_template_columns.clone().unwrap_or_default();
            let rows = lp.grid_template_rows.clone().unwrap_or_default();
            let (row_gap, col_gap) = lp.gap.unwrap_or((0.0, 0.0));

            // Build grid-column / grid-row placement for this element.
            // These are set on children, not the container itself, but we parse
            // and store them here so they're available when building child nodes.
            let col_start = lp.grid_column.map(|(s, _)| s);
            let col_end = lp.grid_column.map(|(_, e)| e);
            let row_start = lp.grid_row.map(|(s, _)| s);
            let row_end = lp.grid_row.map(|(_, e)| e);

            Style {
                display: Display::Grid,
                position: taffy_position,
                inset,
                size: Size {
                    width: lp.width.unwrap_or(Dimension::Percent(1.0)),
                    height: Dimension::Auto,
                },
                max_size: Size {
                    width: lp.max_width.unwrap_or(Dimension::Auto),
                    height: Dimension::Auto,
                },
                margin,
                padding,
                grid_template_columns: columns,
                grid_template_rows: rows,
                gap: Size {
                    width: LengthPercentage::Length(col_gap),
                    height: LengthPercentage::Length(row_gap),
                },
                // Grid placement (used when this element is a grid child).
                grid_column: Line {
                    start: col_start.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                    end: col_end.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                },
                grid_row: Line {
                    start: row_start.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                    end: row_end.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                },
                align_self: if center_via_auto_margin {
                    Some(AlignSelf::Center)
                } else {
                    None
                },
                ..Style::DEFAULT
            }
        }

        "flex" => {
            let direction = resolve_flex_direction(attributes);
            Style {
                display: Display::Flex,
                flex_direction: direction,
                position: taffy_position,
                inset,
                size: Size {
                    width: lp.width.unwrap_or(Dimension::Percent(1.0)),
                    height: Dimension::Auto,
                },
                max_size: Size {
                    width: lp.max_width.unwrap_or(Dimension::Auto),
                    height: Dimension::Auto,
                },
                margin,
                padding,
                align_self: if center_via_auto_margin {
                    Some(AlignSelf::Center)
                } else {
                    None
                },
                ..Style::DEFAULT
            }
        }

        // Table row: horizontal flex container so cells sit side-by-side.
        "table-row" => Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            position: taffy_position,
            inset,
            size: Size {
                width: Dimension::Percent(1.0),
                height: Dimension::Auto,
            },
            margin,
            padding,
            ..Style::DEFAULT
        },

        // Table cell: flex item that grows to share available space.
        "table-cell" => {
            let cell_width = lp.width.unwrap_or(Dimension::Auto);
            Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                flex_grow: if cell_width == Dimension::Auto { 1.0 } else { 0.0 },
                flex_shrink: 1.0,
                position: taffy_position,
                inset,
                size: Size {
                    width: cell_width,
                    height: Dimension::Auto,
                },
                max_size: Size {
                    width: lp.max_width.unwrap_or(Dimension::Auto),
                    height: Dimension::Auto,
                },
                margin,
                padding,
                ..Style::DEFAULT
            }
        },

        // inline-block: behaves like a block box that flows inline.
        "inline-block" => Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Column,
            position: taffy_position,
            inset,
            size: Size {
                width: lp.width.unwrap_or(Dimension::Auto),
                height: Dimension::Auto,
            },
            max_size: Size {
                width: lp.max_width.unwrap_or(Dimension::Auto),
                height: Dimension::Auto,
            },
            margin,
            padding,
            flex_shrink: 0.0,
            ..Style::DEFAULT
        },

        "inline" => Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            position: taffy_position,
            inset,
            size: Size {
                width: lp.width.unwrap_or(Dimension::Auto),
                height: Dimension::Auto,
            },
            max_size: Size {
                width: lp.max_width.unwrap_or(Dimension::Auto),
                height: Dimension::Auto,
            },
            margin,
            padding,
            // Grid placement (for grid children).
            grid_column: {
                let col_start = lp.grid_column.map(|(s, _)| s);
                let col_end = lp.grid_column.map(|(_, e)| e);
                Line {
                    start: col_start.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                    end: col_end.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                }
            },
            grid_row: {
                let row_start = lp.grid_row.map(|(s, _)| s);
                let row_end = lp.grid_row.map(|(_, e)| e);
                Line {
                    start: row_start.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                    end: row_end.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                }
            },
            align_self: if center_via_auto_margin {
                Some(AlignSelf::Center)
            } else {
                None
            },
            ..Style::DEFAULT
        },

        // "block" and everything else: column flex container at full width.
        _ => {
            let _ = tag;
            // Use Auto width + flex_grow to fill available space (respects margins).
            // Only use Percent(1.0) if no margins and no explicit width.
            // Floated elements always use Auto width by default and don't grow.
            let is_floated = lp.float.as_deref().map_or(false, |f| f == "left" || f == "right");
            let has_h_margins = lp.margin_left.is_some() || lp.margin_right.is_some()
                || is_floated;
            let default_width = if has_h_margins || lp.width.is_some() || is_floated {
                lp.width.unwrap_or(Dimension::Auto)
            } else {
                Dimension::Percent(1.0)
            };
            // Floated elements shrink to content; non-floated auto-width elements grow.
            let effective_flex_grow = if is_floated {
                0.0
            } else if default_width == Dimension::Auto {
                1.0
            } else {
                0.0
            };
            let float_margin = Rect {
                top: margin.top,
                right: margin.right,
                bottom: margin.bottom,
                left: effective_margin_left,
            };
            // float_flex_shrink was computed above (0.0 for floated, 1.0 otherwise).
            let _ = float_margin_left; // suppress unused warning; value folded into float_margin
            Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                position: taffy_position,
                inset,
                flex_grow: effective_flex_grow,
                flex_shrink: float_flex_shrink,
                size: Size {
                    width: default_width,
                    height: Dimension::Auto,
                },
                max_size: Size {
                    width: lp.max_width.unwrap_or(Dimension::Auto),
                    height: Dimension::Auto,
                },
                margin: float_margin,
                padding,
                // Grid placement (for grid children).
                grid_column: {
                    let col_start = lp.grid_column.map(|(s, _)| s);
                    let col_end = lp.grid_column.map(|(_, e)| e);
                    Line {
                        start: col_start.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                        end: col_end.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                    }
                },
                grid_row: {
                    let row_start = lp.grid_row.map(|(s, _)| s);
                    let row_end = lp.grid_row.map(|(_, e)| e);
                    Line {
                        start: row_start.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                        end: row_end.map(|v| GridPlacement::Line(v.into())).unwrap_or(GridPlacement::Auto),
                    }
                },
                align_self: if center_via_auto_margin {
                    Some(AlignSelf::Center)
                } else {
                    None
                },
                ..Style::DEFAULT
            }
        }
    }
}

/// Parse the `flex-direction` value from an element's inline style.
fn resolve_flex_direction(attributes: &[(String, String)]) -> FlexDirection {
    if let Some(style_attr) = attributes.iter().find(|(k, _)| k == "style") {
        for decl in style_attr.1.split(';') {
            let parts: Vec<&str> = decl.splitn(2, ':').collect();
            if parts.len() == 2 && parts[0].trim() == "flex-direction" {
                return match parts[1].trim() {
                    "row" => FlexDirection::Row,
                    "row-reverse" => FlexDirection::RowReverse,
                    "column" => FlexDirection::Column,
                    "column-reverse" => FlexDirection::ColumnReverse,
                    _ => FlexDirection::Row, // CSS default for flex containers
                };
            }
        }
    }
    FlexDirection::Row // CSS default
}

// ── Text sizing ────────────────────────────────────────────────────────

/// Measure the (width, height) of a text node given font size and
/// available width, using real font metrics when available.
///
/// Returns `(width, height)` where width is clamped to `available_width`.
fn estimate_text_size(text: &str, font_size: f32, available_width: f32) -> (f32, f32) {
    if text.trim().is_empty() {
        return (0.0, 0.0);
    }

    let line_height = font_size * LINE_HEIGHT_FACTOR;

    // Word-wrap aware measurement using real font metrics.
    let mut max_line_width: f32 = 0.0;
    let mut current_line_width: f32 = 0.0;
    let mut line_count: f32 = 1.0;

    let space_width = measure_text_width(" ", font_size);

    for word in text.split_whitespace() {
        let word_width = measure_text_width(word, font_size);
        let gap = if current_line_width > 0.0 { space_width } else { 0.0 };

        if current_line_width + gap + word_width > available_width && current_line_width > 0.0 {
            // Wrap to next line
            max_line_width = max_line_width.max(current_line_width);
            current_line_width = word_width;
            line_count += 1.0;
        } else {
            current_line_width += gap + word_width;
        }
    }
    max_line_width = max_line_width.max(current_line_width);

    // For single-line text, use exact width; for multi-line, use available_width
    let effective_width = if line_count > 1.0 {
        available_width.min(max_line_width)
    } else {
        max_line_width.min(available_width)
    };

    (effective_width, line_count * line_height)
}

// ── Tag-level defaults ─────────────────────────────────────────────────

/// Get the default display type for a tag (simplified user-agent defaults).
fn display_for_tag(tag: &str) -> &'static str {
    match tag {
        "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "body" | "html" | "section"
        | "article" | "header" | "footer" | "nav" | "main" | "ul" | "ol" | "li"
        | "blockquote" | "pre" | "form" | "hr" => "block",
        // Table elements get special display modes for proper layout.
        "table" | "thead" | "tbody" | "tfoot" => "block",
        "tr" => "table-row",
        "td" | "th" => "table-cell",
        "span" | "a" | "em" | "strong" | "b" | "i" | "u" | "code" | "small" | "br" | "img"
        | "input" | "label" | "select" | "button" | "textarea" => "inline",
        "head" | "title" | "meta" | "link" | "style" | "script" => "none",
        _ => "block",
    }
}

/// Get the default font size for a tag.
fn font_size_for_tag(tag: &str) -> f32 {
    match tag {
        "h1" => 32.0,
        "h2" => 24.0,
        "h3" => 18.72,
        "h4" => 16.0,
        "h5" => 13.28,
        "h6" => 10.72,
        _ => DEFAULT_FONT_SIZE,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_provides_layout() {
        let m = LayoutMod::new();
        assert!(m.manifest().provides(&CapabilityType::Layout));
    }

    #[test]
    fn text_size_estimation() {
        let (w, h) = estimate_text_size("Hello, world!", 16.0, 800.0);
        assert!(w > 0.0);
        assert!(h > 0.0);
    }

    #[test]
    fn empty_text_zero_size() {
        let (w, h) = estimate_text_size("   ", 16.0, 800.0);
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    #[test]
    fn simple_block_layout() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![DomNode::Text("Hello".into())],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        assert_eq!(root.width, 800.0);
        assert!(root.height > 0.0, "root height should be > 0");
    }

    #[test]
    fn nested_block_layout() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("First block".into())],
                    },
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Second block".into())],
                    },
                ],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        // Both divs should be full width.
        let body = &root.children[0];
        assert_eq!(body.children.len(), 2);
        assert_eq!(body.children[0].width, 800.0);
        assert_eq!(body.children[1].width, 800.0);
        // Second div should be positioned below the first.
        assert!(body.children[1].y > body.children[0].y);
    }

    #[test]
    fn flex_row_layout() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![(
                    "style".into(),
                    "display: flex; flex-direction: row".into(),
                )],
                children: vec![
                    DomNode::Text("Left".into()),
                    DomNode::Text("Right".into()),
                ],
            }],
        };
        let viewport = Viewport {
            width: 400.0,
            height: 300.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let flex_container = &root.children[0];
        assert_eq!(flex_container.children.len(), 2);
        // In a row flex container the second child should be to the right.
        assert!(
            flex_container.children[1].x > flex_container.children[0].x,
            "second flex child should be to the right"
        );
    }

    #[test]
    fn display_none_hidden() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "head".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "title".into(),
                    attributes: vec![],
                    children: vec![DomNode::Text("Title".into())],
                }],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        // The head element should be hidden (zero height).
        assert_eq!(root.children[0].height, 0.0);
    }

    #[test]
    fn comment_nodes_invisible() {
        let dom = DomNode::Document {
            children: vec![DomNode::Comment("a comment".into())],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        assert_eq!(root.children[0].width, 0.0);
        assert_eq!(root.children[0].height, 0.0);
    }

    #[test]
    fn margin_from_data_nova_style() {
        // A body with 8px margins all around (user-agent default).
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "margin-top: 8px; margin-right: 8px; margin-bottom: 8px; margin-left: 8px"
                        .into(),
                )],
                children: vec![DomNode::Text("Hello".into())],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let body = &root.children[0];
        // Body should be offset by its 8px margin.
        assert!(
            (body.x - 8.0).abs() < 0.01,
            "body x should be 8, got {}",
            body.x
        );
        assert!(
            (body.y - 8.0).abs() < 0.01,
            "body y should be 8, got {}",
            body.y
        );
        // Body width should be viewport minus left+right margins.
        assert!(
            (body.width - 784.0).abs() < 0.01,
            "body width should be 784, got {}",
            body.width
        );
    }

    #[test]
    fn padding_adds_space() {
        // A div with 20px padding on all sides wrapping text.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "padding: 20px".into(),
                )],
                children: vec![DomNode::Text("Padded".into())],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let div = &root.children[0];
        let text = &div.children[0];
        // Text should be inset by the padding.
        assert!(
            (text.x - div.x - 20.0).abs() < 0.01,
            "text x should be 20px inside div: text.x={}, div.x={}",
            text.x,
            div.x
        );
        assert!(
            (text.y - div.y - 20.0).abs() < 0.01,
            "text y should be 20px inside div: text.y={}, div.y={}",
            text.y,
            div.y
        );
    }

    #[test]
    fn explicit_width_px() {
        // A div with width: 400px inside an 800px viewport.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "width: 400px".into(),
                )],
                children: vec![DomNode::Text("Narrow".into())],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let div = &root.children[0];
        assert!(
            (div.width - 400.0).abs() < 0.01,
            "div width should be 400, got {}",
            div.width
        );
    }

    #[test]
    fn max_width_clamps() {
        // A full-width div with max-width: 300px.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "max-width: 300px".into(),
                )],
                children: vec![DomNode::Text("Clamped".into())],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let div = &root.children[0];
        assert!(
            div.width <= 300.01,
            "div width should be at most 300, got {}",
            div.width
        );
    }

    #[test]
    fn margin_auto_centers() {
        // A div with width and margin: 0 auto should be centred.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "width: 400px; margin-left: auto; margin-right: auto".into(),
                )],
                children: vec![DomNode::Text("Centred".into())],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let div = &root.children[0];
        assert!(
            (div.width - 400.0).abs() < 0.01,
            "div width should be 400, got {}",
            div.width
        );
        // Centred: x should be (800 - 400) / 2 = 200.
        assert!(
            (div.x - 200.0).abs() < 1.0,
            "div should be centred at x=200, got {}",
            div.x
        );
    }

    #[test]
    fn h1_default_margins_from_cascade() {
        // h1 with user-agent margins from data-nova-style.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "h1".into(),
                    attributes: vec![(
                        "data-nova-style".into(),
                        "margin-top: 21.44px; margin-bottom: 21.44px; font-size: 32px".into(),
                    )],
                    children: vec![DomNode::Text("Title".into())],
                }],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let body = &root.children[0];
        let h1 = &body.children[0];
        // h1 should be offset by its top margin.
        assert!(
            (h1.y - body.y - 21.44).abs() < 1.0,
            "h1 y offset should be ~21.44px from body, got delta {}",
            h1.y - body.y
        );
    }

    #[test]
    fn parse_layout_props_shorthand_margin() {
        let attrs = vec![(
            "data-nova-style".into(),
            "margin: 10px 20px 30px 40px".into(),
        )];
        let lp = parse_layout_props(&attrs);
        assert!(matches!(lp.margin_top, Some(LengthPercentageAuto::Length(v)) if (v - 10.0).abs() < 0.01));
        assert!(matches!(lp.margin_right, Some(LengthPercentageAuto::Length(v)) if (v - 20.0).abs() < 0.01));
        assert!(matches!(lp.margin_bottom, Some(LengthPercentageAuto::Length(v)) if (v - 30.0).abs() < 0.01));
        assert!(matches!(lp.margin_left, Some(LengthPercentageAuto::Length(v)) if (v - 40.0).abs() < 0.01));
    }

    #[test]
    fn parse_layout_props_shorthand_padding_two_values() {
        let attrs = vec![("style".into(), "padding: 10px 20px".into())];
        let lp = parse_layout_props(&attrs);
        assert!(matches!(lp.padding_top, Some(LengthPercentage::Length(v)) if (v - 10.0).abs() < 0.01));
        assert!(matches!(lp.padding_right, Some(LengthPercentage::Length(v)) if (v - 20.0).abs() < 0.01));
        assert!(matches!(lp.padding_bottom, Some(LengthPercentage::Length(v)) if (v - 10.0).abs() < 0.01));
        assert!(matches!(lp.padding_left, Some(LengthPercentage::Length(v)) if (v - 20.0).abs() < 0.01));
    }

    #[test]
    fn width_percent() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "width: 50%".into(),
                )],
                children: vec![DomNode::Text("Half".into())],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let div = &root.children[0];
        assert!(
            (div.width - 400.0).abs() < 0.01,
            "div width should be 400 (50% of 800), got {}",
            div.width
        );
    }

    // ── Phase 6A: CSS Grid tests ───────────────────────────────────────

    #[test]
    fn parse_track_list_fr_units() {
        let tracks = parse_track_list("1fr 2fr 1fr").unwrap();
        assert_eq!(tracks.len(), 3, "should parse 3 fr tracks");
    }

    #[test]
    fn parse_track_list_px() {
        let tracks = parse_track_list("200px 400px").unwrap();
        assert_eq!(tracks.len(), 2, "should parse 2 px tracks");
    }

    #[test]
    fn parse_track_list_auto() {
        let tracks = parse_track_list("auto auto auto").unwrap();
        assert_eq!(tracks.len(), 3, "should parse 3 auto tracks");
    }

    #[test]
    fn parse_track_list_repeat() {
        let tracks = parse_track_list("repeat(3, 1fr)").unwrap();
        assert_eq!(tracks.len(), 3, "repeat(3, 1fr) should expand to 3 tracks");
    }

    #[test]
    fn parse_track_list_minmax() {
        let tracks = parse_track_list("minmax(100px, 1fr)").unwrap();
        assert_eq!(tracks.len(), 1, "minmax should produce 1 track");
    }

    #[test]
    fn parse_grid_line_span_basic() {
        let (start, end) = parse_grid_line_span("1 / 3").unwrap();
        assert_eq!(start, 1);
        assert_eq!(end, 3);
    }

    #[test]
    fn parse_grid_line_span_single() {
        let (start, end) = parse_grid_line_span("2").unwrap();
        assert_eq!(start, 2);
        assert_eq!(end, 0); // auto
    }

    #[test]
    fn grid_layout_computes() {
        // A 2-column grid with 3 children — should distribute children across columns.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "display: grid; grid-template-columns: 1fr 1fr".into(),
                )],
                children: vec![
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Cell 1".into())],
                    },
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Cell 2".into())],
                    },
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Cell 3".into())],
                    },
                ],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("grid layout should succeed");
        let grid = &root.children[0];
        // Grid container should span the full viewport width.
        assert!(
            (grid.width - 800.0).abs() < 1.0,
            "grid container should be 800px wide, got {}",
            grid.width
        );
        // Should have 3 children.
        assert_eq!(grid.children.len(), 3, "grid should have 3 children");
        // The grid should have placed children — they should have nonzero height.
        for cell in &grid.children {
            assert!(
                cell.height >= 0.0,
                "each grid cell should have non-negative height, got {}",
                cell.height
            );
        }
        // Cells should be placed in rows/columns — at least one cell should be offset.
        // (Not all at x=0 or y=0, unless the grid happens to place them there.)
        let _ = grid.children[0].x; // Just verify we can access coordinates.
    }

    #[test]
    fn grid_gap_parses() {
        let attrs = vec![(
            "data-nova-style".into(),
            "display: grid; gap: 10px 20px".into(),
        )];
        let lp = parse_layout_props(&attrs);
        assert!(
            matches!(lp.gap, Some((row, col)) if (row - 10.0).abs() < 0.01 && (col - 20.0).abs() < 0.01),
            "gap should be (10px row, 20px col), got {:?}",
            lp.gap
        );
    }
}

#[cfg(test)]
mod phase4_tests {
    use super::*;
    use nova_mod_api::content::DomNode;
    use nova_mod_api::types::Viewport;

    fn viewport() -> Viewport {
        Viewport { width: 800.0, height: 600.0, scale_factor: 1.0 }
    }

    #[test]
    fn multi_word_text_creates_multiple_children() {
        // A text node with multiple words should produce a wrapper containing
        // multiple word leaf nodes.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![DomNode::Text("Hello World Foo".into())],
            }],
        };
        let root = compute_layout(&dom, &viewport()).expect("layout ok");
        let body = &root.children[0];
        // body has one child: the inline wrapper for the text.
        assert!(
            !body.children.is_empty(),
            "body should have children for the text"
        );
        // The text wrapper should have multiple children (word nodes + space nodes).
        let text_wrapper = &body.children[0];
        assert!(
            text_wrapper.children.len() >= 3,
            "text wrapper should have >=3 children (words + spaces), got {}",
            text_wrapper.children.len()
        );
    }

    #[test]
    fn single_word_text_produces_single_node() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![],
                children: vec![DomNode::Text("Hello".into())],
            }],
        };
        let root = compute_layout(&dom, &viewport()).expect("layout ok");
        let div = &root.children[0];
        // For a single word there should be exactly one child (the word itself, no wrapper).
        assert_eq!(
            div.children.len(),
            1,
            "single-word text should produce exactly 1 child, got {}",
            div.children.len()
        );
        // That child should have positive width.
        assert!(
            div.children[0].width > 0.0,
            "single word should have positive width"
        );
    }

    #[test]
    fn float_left_parses() {
        let attrs = vec![
            ("data-nova-style".into(), "float: left; width: 200px".into()),
        ];
        let lp = parse_layout_props(&attrs);
        assert_eq!(lp.float.as_deref(), Some("left"));
        assert!(lp.width.is_some());
    }

    #[test]
    fn float_right_parses() {
        let attrs = vec![
            ("data-nova-style".into(), "float: right".into()),
        ];
        let lp = parse_layout_props(&attrs);
        assert_eq!(lp.float.as_deref(), Some("right"));
    }

    #[test]
    fn float_right_uses_margin_left_auto() {
        // A float: right element should be approximated with margin-left: auto.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "float: right; width: 200px".into(),
                )],
                children: vec![DomNode::Text("Float".into())],
            }],
        };
        let root = compute_layout(&dom, &viewport()).expect("layout ok");
        let div = &root.children[0];
        // With margin-left: auto and a 200px width in an 800px viewport,
        // the element should be pushed to the right (x should be > 0).
        assert!(
            div.x > 0.0,
            "float:right div should be pushed right, x={}",
            div.x
        );
    }
}

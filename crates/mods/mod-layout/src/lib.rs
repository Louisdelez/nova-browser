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
const CHAR_WIDTH_AT_16PX: f32 = 8.0;

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
            // Special case: <center> centers its children horizontally.
            if tag == "center" {
                let child_ids = children
                    .iter()
                    .map(|c| add_node(taffy, c, available_width, parent_font_size, parent_style_props))
                    .collect::<Result<Vec<_>, _>>()?;

                let style = Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Column,
                    align_items: Some(AlignItems::Center),
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

                return taffy
                    .new_with_children(style, &child_ids)
                    .map(|id| {
                        taffy.set_node_context(id, Some(ctx)).ok();
                        id
                    })
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // Special case: <br> forces a line break (full-width zero-height block).
            if tag == "br" {
                let ctx = NodeContext {
                    content: LayoutContent::Block,
                    style: StyleMap::default(),
                };
                let style = Style {
                    display: Display::Flex,
                    size: Size {
                        width: Dimension::Percent(1.0),
                        height: Dimension::Length(0.0),
                    },
                    ..Style::DEFAULT
                };
                return taffy
                    .new_leaf_with_context(style, ctx)
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // Special case: <hr> draws a horizontal rule.
            if tag == "hr" {
                let ctx = NodeContext {
                    content: LayoutContent::Block,
                    style: StyleMap {
                        properties: vec![
                            ("display".into(), StyleValue::Keyword("block".into())),
                            ("border-bottom".into(), StyleValue::Str("1px solid #ccc".into())),
                        ],
                    },
                };
                let style = Style {
                    display: Display::Flex,
                    size: Size {
                        width: Dimension::Percent(1.0),
                        height: Dimension::Length(1.0),
                    },
                    margin: Rect {
                        top: LengthPercentageAuto::Length(8.0),
                        right: LengthPercentageAuto::Length(0.0),
                        bottom: LengthPercentageAuto::Length(8.0),
                        left: LengthPercentageAuto::Length(0.0),
                    },
                    ..Style::DEFAULT
                };
                return taffy
                    .new_leaf_with_context(style, ctx)
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // Special case: <li> gets a bullet/number marker.
            if tag == "li" {
                // Determine bullet type from parent context.
                let is_ordered = parent_style_props
                    .iter()
                    .any(|(k, v)| k == "list-style" && matches!(v, StyleValue::Keyword(s) if s == "ordered"));

                let bullet = if is_ordered { "• " } else { "• " };

                // Create bullet text node.
                let bullet_ctx = NodeContext {
                    content: LayoutContent::Text(bullet.into()),
                    style: StyleMap {
                        properties: vec![("font-size".into(), StyleValue::Px(parent_font_size))],
                    },
                };
                let bullet_style = Style {
                    display: Display::Flex,
                    size: Size {
                        width: Dimension::Length(CHAR_WIDTH_AT_16PX * 2.0 * (parent_font_size / 16.0)),
                        height: Dimension::Length(parent_font_size * LINE_HEIGHT_FACTOR),
                    },
                    flex_shrink: 0.0,
                    ..Style::DEFAULT
                };
                let bullet_id = taffy
                    .new_leaf_with_context(bullet_style, bullet_ctx)
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;

                // Build children normally.
                let mut child_ids = vec![bullet_id];
                let content_children: Vec<NodeId> = children
                    .iter()
                    .map(|c| add_node(taffy, c, available_width - 16.0, parent_font_size, parent_style_props))
                    .collect::<Result<Vec<_>, _>>()?;
                child_ids.extend(content_children);

                let li_style = Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    flex_wrap: FlexWrap::Wrap,
                    size: Size {
                        width: Dimension::Percent(1.0),
                        height: Dimension::Auto,
                    },
                    ..Style::DEFAULT
                };

                let ctx = NodeContext {
                    content: LayoutContent::Block,
                    style: StyleMap {
                        properties: vec![("display".into(), StyleValue::Keyword("list-item".into()))],
                    },
                };

                return taffy
                    .new_with_children(li_style, &child_ids)
                    .map(|id| {
                        taffy.set_node_context(id, Some(ctx)).ok();
                        id
                    })
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // Special case: table elements get flex-based table layout.
            if is_table_element(tag) {
                return add_table_node(taffy, tag, children, attributes, available_width, parent_font_size, parent_style_props);
            }

            // Special case: <img> is a replaced element with intrinsic dimensions.
            if tag == "img" {
                let src = attributes
                    .iter()
                    .find(|(k, _)| k == "src")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();

                // Parse width/height attributes (default 300x150 per HTML spec).
                let img_width = attributes
                    .iter()
                    .find(|(k, _)| k == "width")
                    .and_then(|(_, v)| v.parse::<f32>().ok())
                    .unwrap_or(300.0);
                let img_height = attributes
                    .iter()
                    .find(|(k, _)| k == "height")
                    .and_then(|(_, v)| v.parse::<f32>().ok())
                    .unwrap_or(150.0);

                let ctx = NodeContext {
                    content: LayoutContent::Image { src },
                    style: StyleMap {
                        properties: vec![
                            ("display".into(), StyleValue::Keyword("inline".into())),
                        ],
                    },
                };

                let style = Style {
                    display: Display::Flex,
                    size: Size {
                        width: Dimension::Length(img_width),
                        height: Dimension::Length(img_height),
                    },
                    ..Style::DEFAULT
                };

                return taffy
                    .new_leaf_with_context(style, ctx)
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
                                    if let Some(size) = parse_font_size_value(val) {
                                        existing.1 = StyleValue::Px(size);
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
                        } else if let Some(pt) = val.strip_suffix("pt").and_then(|s| s.trim().parse::<f32>().ok()) {
                            props.push((prop, StyleValue::Px(pt * 4.0 / 3.0)));
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

            // Build children with inline flow: consecutive inline elements
            // are wrapped in synthetic flex-row containers ("line boxes").
            let child_ids = if display != "inline" {
                build_children_with_inline_flow(taffy, children, available_width, font_size, &props)?
            } else {
                children
                    .iter()
                    .map(|c| add_node(taffy, c, available_width, font_size, &props))
                    .collect::<Result<Vec<_>, _>>()?
            };
            let taffy_style = build_taffy_style(&display, tag, attributes);

            let content_type = match display.as_str() {
                "inline" => LayoutContent::Inline,
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
                if key == "color" || key == "background-color" || key == "text-decoration" {
                    text_props.push((key.clone(), value.clone()));
                }
            }
            let ctx = NodeContext {
                content: LayoutContent::Text(text.clone()),
                style: StyleMap {
                    properties: text_props,
                },
            };

            let text_clone = text.clone();
            let style = Style {
                display: Display::Flex,
                ..Style::DEFAULT
            };

            // Use a measure function so Taffy can size the text leaf
            // according to the available space.
            taffy
                .new_leaf_with_context(style, ctx)
                .map(|id| {
                    let t = text_clone.clone();
                    let fs = font_size;
                    // We use a fixed size based on estimation since TaffyTree
                    // measure functions require the MeasureFunc approach.
                    // We pre-compute the size and set it directly.
                    let (w, h) = estimate_text_size(&t, fs, available_width);
                    taffy
                        .set_style(
                            id,
                            Style {
                                display: Display::Flex,
                                size: Size {
                                    width: Dimension::Length(w),
                                    height: Dimension::Length(h),
                                },
                                ..Style::DEFAULT
                            },
                        )
                        .ok();
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

    LayoutBox {
        x,
        y,
        width,
        height,
        content: ctx.content,
        style: ctx.style,
        children,
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
                    if let Some(size) = parse_font_size_value(val) {
                        return size;
                    }
                }
            }
        }
    }
    // Tag-based defaults for headings.
    font_size_for_tag(tag)
}

/// Parse a CSS font-size value to pixels.
/// Supports: `px`, `pt`, `em`, `rem`, `%`.
fn parse_font_size_value(val: &str) -> Option<f32> {
    let val = val.trim();
    if let Some(px) = val.strip_suffix("px").and_then(|s| s.trim().parse::<f32>().ok()) {
        Some(px)
    } else if let Some(pt) = val.strip_suffix("pt").and_then(|s| s.trim().parse::<f32>().ok()) {
        // 1pt = 4/3 px
        Some(pt * 4.0 / 3.0)
    } else if let Some(em) = val.strip_suffix("em").and_then(|s| s.trim().parse::<f32>().ok()) {
        // em relative to parent — approximate with 16px base
        Some(em * DEFAULT_FONT_SIZE)
    } else if let Some(rem) = val.strip_suffix("rem").and_then(|s| s.trim().parse::<f32>().ok()) {
        Some(rem * DEFAULT_FONT_SIZE)
    } else if let Some(pct) = val.strip_suffix('%').and_then(|s| s.trim().parse::<f32>().ok()) {
        Some(pct / 100.0 * DEFAULT_FONT_SIZE)
    } else {
        // Try plain number.
        val.parse::<f32>().ok()
    }
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
    // Lists get default left padding for indentation.
    let default_left_pad = if tag == "ul" || tag == "ol" {
        LengthPercentage::Length(24.0)
    } else {
        LengthPercentage::Length(0.0)
    };
    let padding = Rect {
        top: lp.padding_top.unwrap_or(LengthPercentage::Length(0.0)),
        right: lp.padding_right.unwrap_or(LengthPercentage::Length(0.0)),
        bottom: lp.padding_bottom.unwrap_or(LengthPercentage::Length(0.0)),
        left: lp.padding_left.unwrap_or(default_left_pad),
    };

    // When both horizontal margins are `auto`, centre the element via
    // `align_self: Center` (the Taffy-idiomatic way to express `margin: 0 auto`).
    let center_via_auto_margin = matches!(
        (&lp.margin_left, &lp.margin_right),
        (Some(LengthPercentageAuto::Auto), Some(LengthPercentageAuto::Auto))
    );

    match display {
        "none" => Style {
            display: Display::None,
            ..Style::DEFAULT
        },

        "flex" => {
            let direction = resolve_flex_direction(attributes);
            Style {
                display: Display::Flex,
                flex_direction: direction,
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

        "inline" => Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
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
            let has_h_margins = lp.margin_left.is_some() || lp.margin_right.is_some();
            let default_width = if has_h_margins || lp.width.is_some() {
                lp.width.unwrap_or(Dimension::Auto)
            } else {
                Dimension::Percent(1.0)
            };
            Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                flex_grow: if default_width == Dimension::Auto { 1.0 } else { 0.0 },
                size: Size {
                    width: default_width,
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

/// Estimate the (width, height) of a text node given font size and
/// available width.
///
/// Returns `(width, height)` where width is clamped to `available_width`.
fn estimate_text_size(text: &str, font_size: f32, available_width: f32) -> (f32, f32) {
    if text.trim().is_empty() {
        return (0.0, 0.0);
    }

    let scale = font_size / 16.0;
    let char_width = CHAR_WIDTH_AT_16PX * scale;
    let text_width = text.len() as f32 * char_width;
    let effective_width = text_width.min(available_width).max(0.0);
    let chars_per_line = (available_width / char_width).max(1.0);
    let line_count = (text.len() as f32 / chars_per_line).ceil().max(1.0);
    let line_height = font_size * LINE_HEIGHT_FACTOR;

    (effective_width, line_count * line_height)
}

// ── Inline flow: wrapping consecutive inline children ───────────────────

/// Determines if a DOM node is inline-level for flow purposes.
fn is_inline_node(node: &DomNode) -> bool {
    match node {
        DomNode::Text(_) => true,
        DomNode::Element { tag, attributes, .. } => {
            let display = resolve_display(tag, attributes);
            display == "inline"
        }
        _ => false,
    }
}

/// Build child nodes with inline flow: consecutive inline children are
/// wrapped in synthetic flex-row containers ("line boxes") so they flow
/// horizontally. Block-level children break the inline flow.
fn build_children_with_inline_flow(
    taffy: &mut TaffyTree<NodeContext>,
    children: &[DomNode],
    available_width: f32,
    font_size: f32,
    parent_props: &[(String, StyleValue)],
) -> Result<Vec<NodeId>, NovaError> {
    let mut result = Vec::new();
    let mut inline_run: Vec<&DomNode> = Vec::new();

    for child in children {
        if is_inline_node(child) {
            inline_run.push(child);
        } else {
            // Flush any pending inline run before the block element.
            if !inline_run.is_empty() {
                let line_id = wrap_inline_run(taffy, &inline_run, available_width, font_size, parent_props)?;
                result.push(line_id);
                inline_run.clear();
            }
            // Add the block element directly.
            let id = add_node(taffy, child, available_width, font_size, parent_props)?;
            result.push(id);
        }
    }

    // Flush remaining inline run.
    if !inline_run.is_empty() {
        // If the entire content is inline, just add them directly without
        // a wrapper (the parent's flex-direction handles it).
        if result.is_empty() && inline_run.len() == children.len() {
            for child in &inline_run {
                let id = add_node(taffy, child, available_width, font_size, parent_props)?;
                result.push(id);
            }
        } else {
            let line_id = wrap_inline_run(taffy, &inline_run, available_width, font_size, parent_props)?;
            result.push(line_id);
        }
    }

    Ok(result)
}

/// Wrap a run of inline nodes in a flex-row container.
fn wrap_inline_run(
    taffy: &mut TaffyTree<NodeContext>,
    nodes: &[&DomNode],
    available_width: f32,
    font_size: f32,
    parent_props: &[(String, StyleValue)],
) -> Result<NodeId, NovaError> {
    let child_ids: Vec<NodeId> = nodes
        .iter()
        .map(|n| add_node(taffy, n, available_width, font_size, parent_props))
        .collect::<Result<Vec<_>, _>>()?;

    let style = Style {
        display: Display::Flex,
        flex_direction: FlexDirection::Row,
        flex_wrap: FlexWrap::Wrap,
        size: Size {
            width: Dimension::Percent(1.0),
            height: Dimension::Auto,
        },
        ..Style::DEFAULT
    };

    let ctx = NodeContext {
        content: LayoutContent::Inline,
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

// ── Table layout (flex-based simulation) ────────────────────────────────

/// Returns `true` for HTML tags that are part of table layout.
fn is_table_element(tag: &str) -> bool {
    matches!(
        tag,
        "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" | "caption" | "colgroup" | "col"
    )
}

/// Build a Taffy node for a table-related element.
///
/// Maps table elements to flex layout:
/// - `<table>` → flex column, full width
/// - `<thead>/<tbody>/<tfoot>` → flex column (pass-through)
/// - `<tr>` → flex row
/// - `<td>/<th>` → flex item with `flex-grow: 1` (equal width)
/// - `<caption>` → block element above the table
/// - `<colgroup>/<col>` → hidden (layout hints only)
fn add_table_node(
    taffy: &mut TaffyTree<NodeContext>,
    tag: &str,
    children: &[DomNode],
    attributes: &[(String, String)],
    available_width: f32,
    parent_font_size: f32,
    parent_style_props: &[(String, StyleValue)],
) -> Result<NodeId, NovaError> {
    let font_size = resolve_font_size(tag, attributes, parent_font_size);

    // Build style properties for this node.
    let mut props = vec![
        ("font-size".into(), StyleValue::Px(font_size)),
    ];

    // Parse data-nova-style if present.
    if let Some(nova_style) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
        for decl in nova_style.1.split(';') {
            let parts: Vec<&str> = decl.splitn(2, ':').collect();
            if parts.len() == 2 {
                let prop = parts[0].trim().to_string();
                let val = parts[1].trim();
                if prop == "font-size" {
                    continue;
                }
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

    // Parse cellpadding from <table> attributes.
    let cellpadding = if tag == "table" {
        attributes
            .iter()
            .find(|(k, _)| k == "cellpadding")
            .and_then(|(_, v)| v.parse::<f32>().ok())
            .unwrap_or(1.0)
    } else {
        0.0
    };

    // Parse cellspacing from <table> attributes.
    let cellspacing = if tag == "table" {
        attributes
            .iter()
            .find(|(k, _)| k == "cellspacing")
            .and_then(|(_, v)| v.parse::<f32>().ok())
            .unwrap_or(2.0)
    } else {
        0.0
    };

    debug!(tag = tag, children = children.len(), "table layout: processing element");

    match tag {
        "table" => {
            // Parse table width attribute.
            let table_width = attributes
                .iter()
                .find(|(k, _)| k == "width")
                .and_then(|(_, v)| {
                    if let Some(pct) = v.strip_suffix('%') {
                        pct.parse::<f32>().ok().map(|p| Dimension::Percent(p / 100.0))
                    } else {
                        v.parse::<f32>().ok().map(Dimension::Length)
                    }
                })
                .unwrap_or(Dimension::Percent(1.0));

            let lp = parse_layout_props(attributes);

            let child_ids = children
                .iter()
                .map(|c| add_node(taffy, c, available_width, font_size, &props))
                .collect::<Result<Vec<_>, _>>()?;

            let style = Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                size: Size {
                    width: table_width,
                    height: Dimension::Auto,
                },
                gap: Size {
                    width: LengthPercentage::Length(cellspacing),
                    height: LengthPercentage::Length(cellspacing),
                },
                padding: Rect {
                    top: LengthPercentage::Length(cellspacing),
                    right: LengthPercentage::Length(cellspacing),
                    bottom: LengthPercentage::Length(cellspacing),
                    left: LengthPercentage::Length(cellspacing),
                },
                margin: Rect {
                    top: lp.margin_top.unwrap_or(LengthPercentageAuto::Length(0.0)),
                    right: lp.margin_right.unwrap_or(LengthPercentageAuto::Length(0.0)),
                    bottom: lp.margin_bottom.unwrap_or(LengthPercentageAuto::Length(0.0)),
                    left: lp.margin_left.unwrap_or(LengthPercentageAuto::Length(0.0)),
                },
                ..Style::DEFAULT
            };

            props.insert(0, ("display".into(), StyleValue::Keyword("table".into())));
            let ctx = NodeContext {
                content: LayoutContent::Block,
                style: StyleMap { properties: props },
            };

            taffy
                .new_with_children(style, &child_ids)
                .map(|id| {
                    taffy.set_node_context(id, Some(ctx)).ok();
                    id
                })
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }

        "thead" | "tbody" | "tfoot" => {
            // Pass-through column container.
            let child_ids = children
                .iter()
                .map(|c| add_node(taffy, c, available_width, font_size, &props))
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
                style: StyleMap { properties: props },
            };

            taffy
                .new_with_children(style, &child_ids)
                .map(|id| {
                    taffy.set_node_context(id, Some(ctx)).ok();
                    id
                })
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }

        "tr" => {
            // Row: flex row container.
            let child_ids = children
                .iter()
                .map(|c| add_node(taffy, c, available_width, font_size, &props))
                .collect::<Result<Vec<_>, _>>()?;

            // After building children, set the last non-zero-width cell
            // to flex-grow: 1 so it fills remaining space. This simulates
            // table column auto-sizing where the widest column expands.
            for &child_id in child_ids.iter().rev() {
                if let Some(ctx) = taffy.get_node_context(child_id) {
                    if matches!(ctx.content, LayoutContent::Block) {
                        let mut style = taffy.style(child_id).unwrap().clone();
                        if style.flex_grow == 0.0 && style.size.width == Dimension::Auto {
                            style.flex_grow = 1.0;
                            taffy.set_style(child_id, style).ok();
                        }
                        break;
                    }
                }
            }

            let style = Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::NoWrap,
                size: Size {
                    width: Dimension::Percent(1.0),
                    height: Dimension::Auto,
                },
                ..Style::DEFAULT
            };

            let ctx = NodeContext {
                content: LayoutContent::Block,
                style: StyleMap { properties: props },
            };

            taffy
                .new_with_children(style, &child_ids)
                .map(|id| {
                    taffy.set_node_context(id, Some(ctx)).ok();
                    id
                })
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }

        "td" | "th" => {
            // Cell: flex item that grows to fill available space.
            let child_ids = children
                .iter()
                .map(|c| add_node(taffy, c, available_width, font_size, &props))
                .collect::<Result<Vec<_>, _>>()?;

            // Parse colspan for proportional sizing.
            let colspan = attributes
                .iter()
                .find(|(k, _)| k == "colspan")
                .and_then(|(_, v)| v.parse::<f32>().ok())
                .unwrap_or(1.0);

            // Parse explicit width on cells.
            let cell_width = attributes
                .iter()
                .find(|(k, _)| k == "width")
                .and_then(|(_, v)| {
                    if let Some(pct) = v.strip_suffix('%') {
                        pct.parse::<f32>().ok().map(|p| Dimension::Percent(p / 100.0))
                    } else {
                        v.parse::<f32>().ok().map(Dimension::Length)
                    }
                });

            // Also check data-nova-style for width (inline styles get cascaded here).
            let css_width = parse_layout_props(attributes).width;
            let final_width = cell_width.or(css_width).unwrap_or(Dimension::Auto);

            // Cells size to content by default (flex-grow: 0).
            // Cells with colspan > 1 grow proportionally.
            let has_explicit_width = cell_width.is_some() || css_width.is_some();
            let flex_grow = if has_explicit_width {
                0.0
            } else if colspan > 1.0 {
                colspan
            } else {
                0.0
            };

            // Parse valign attribute.
            let valign = attributes
                .iter()
                .find(|(k, _)| k == "valign")
                .map(|(_, v)| v.as_str())
                .unwrap_or("top");

            let align_items = match valign {
                "middle" | "center" => Some(AlignItems::Center),
                "bottom" => Some(AlignItems::FlexEnd),
                _ => Some(AlignItems::FlexStart), // top (default)
            };

            // Determine cell padding.
            let cell_pad = parse_layout_props(attributes);
            let default_pad = LengthPercentage::Length(2.0);

            let style = Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                flex_grow,
                flex_shrink: 1.0,
                flex_basis: Dimension::Auto,
                size: Size {
                    width: final_width,
                    height: Dimension::Auto,
                },
                min_size: Size {
                    width: Dimension::Auto,
                    height: Dimension::Auto,
                },
                padding: Rect {
                    top: cell_pad.padding_top.unwrap_or(default_pad),
                    right: cell_pad.padding_right.unwrap_or(default_pad),
                    bottom: cell_pad.padding_bottom.unwrap_or(default_pad),
                    left: cell_pad.padding_left.unwrap_or(default_pad),
                },
                align_items,
                ..Style::DEFAULT
            };

            // Bold text for <th>.
            if tag == "th" {
                props.push(("font-weight".into(), StyleValue::Keyword("bold".into())));
            }

            let ctx = NodeContext {
                content: LayoutContent::Block,
                style: StyleMap { properties: props },
            };

            taffy
                .new_with_children(style, &child_ids)
                .map(|id| {
                    taffy.set_node_context(id, Some(ctx)).ok();
                    id
                })
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }

        "caption" => {
            // Caption: block element displayed above the table.
            let child_ids = children
                .iter()
                .map(|c| add_node(taffy, c, available_width, font_size, &props))
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
                style: StyleMap { properties: props },
            };

            taffy
                .new_with_children(style, &child_ids)
                .map(|id| {
                    taffy.set_node_context(id, Some(ctx)).ok();
                    id
                })
                .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
        }

        // colgroup / col: invisible layout hints, not rendered.
        _ => {
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

// ── Tag-level defaults ─────────────────────────────────────────────────

/// Get the default display type for a tag (simplified user-agent defaults).
fn display_for_tag(tag: &str) -> &'static str {
    match tag {
        "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "body" | "html" | "section"
        | "article" | "header" | "footer" | "nav" | "main" | "ul" | "ol" | "li"
        | "blockquote" | "pre" | "form" | "hr" => "block",
        // Table elements are handled by add_table_node, but keep them as block
        // for the display_for_tag fallback.
        "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" | "caption" => "block",
        "span" | "a" | "em" | "strong" | "b" | "i" | "u" | "code" | "small" | "img"
        | "input" | "label" | "abbr" | "cite" | "sub" | "sup" | "time" | "var"
        | "kbd" | "samp" | "mark" | "q" | "dfn" | "bdo" | "bdi" | "data"
        | "ruby" | "rt" | "rp" | "wbr" => "inline",
        "br" => "block", // <br> handled as special case in add_node
        "head" | "title" | "meta" | "link" | "style" | "script" | "noscript"
        | "template" | "datalist" => "none",
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

    #[test]
    fn table_row_cells_side_by_side() {
        // A table with one row and three cells: narrow, narrow, wide.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "table".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "tbody".into(),
                    attributes: vec![],
                    children: vec![DomNode::Element {
                        tag: "tr".into(),
                        attributes: vec![],
                        children: vec![
                            DomNode::Element {
                                tag: "td".into(),
                                attributes: vec![],
                                children: vec![DomNode::Text("1.".into())],
                            },
                            DomNode::Element {
                                tag: "td".into(),
                                attributes: vec![],
                                children: vec![DomNode::Text("^".into())],
                            },
                            DomNode::Element {
                                tag: "td".into(),
                                attributes: vec![],
                                children: vec![DomNode::Text(
                                    "A long title that should take the remaining space".into(),
                                )],
                            },
                        ],
                    }],
                }],
            }],
        };
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");

        // Dig into table > tbody > tr
        let table = &root.children[0];
        let tbody = &table.children[0];
        let tr = &tbody.children[0];

        assert_eq!(
            tr.children.len(),
            3,
            "tr should have 3 children (cells)"
        );

        let cell1 = &tr.children[0];
        let cell2 = &tr.children[1];
        let cell3 = &tr.children[2];

        // Cells should be side by side (increasing x positions).
        assert!(
            cell2.x > cell1.x,
            "cell2.x ({}) should be > cell1.x ({})",
            cell2.x,
            cell1.x
        );
        assert!(
            cell3.x > cell2.x,
            "cell3.x ({}) should be > cell2.x ({})",
            cell3.x,
            cell2.x
        );

        // The last cell should be wider than the first two (it has more content + flex-grow).
        assert!(
            cell3.width > cell1.width,
            "cell3.width ({}) should be > cell1.width ({})",
            cell3.width,
            cell1.width
        );

        // All cells should be on the same y position.
        assert!(
            (cell1.y - cell2.y).abs() < 1.0,
            "cells should be on the same row: cell1.y={}, cell2.y={}",
            cell1.y,
            cell2.y
        );
    }
}

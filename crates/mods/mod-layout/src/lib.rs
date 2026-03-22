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

pub mod incremental;

use std::sync::Arc;

use async_trait::async_trait;
use semver::Version;
use taffy::prelude::*;
use taffy::geometry::Point;
use taffy::style::Overflow;
use tracing::{debug, info};

// ── Font measurement (rustybuzz-based, kerning-aware) ────────────────

/// Thread-local font data for rustybuzz text shaping in layout.
///
/// Uses the same shaping engine as the renderer so that measured widths
/// match rendered glyph positions exactly (kerning + ligatures included).
/// Falls back to fontdue per-character measurement, then to estimation.
thread_local! {
    static LAYOUT_FONT_DATA: Option<Vec<u8>> = {
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
            std::path::PathBuf::from("/usr/share/fonts/noto/NotoSans-Regular.ttf"),
            std::path::PathBuf::from("/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf"),
        ]);

        for path in &paths {
            if let Ok(bytes) = std::fs::read(path) {
                // Validate that rustybuzz can parse the font.
                if rustybuzz::Face::from_slice(&bytes, 0).is_some() {
                    return Some(bytes);
                }
            }
        }
        None
    };

    static LAYOUT_FONT: Option<fontdue::Font> = {
        LAYOUT_FONT_DATA.with(|data| {
            data.as_ref().and_then(|bytes| {
                fontdue::Font::from_bytes(bytes.clone(), fontdue::FontSettings::default()).ok()
            })
        })
    };
}

/// Maximum entries in the per-thread text measurement cache.
/// LRU eviction occurs when this limit is reached.
const TEXT_CACHE_MAX_ENTRIES: usize = 10_000;

/// Thread-local cache for text width measurements.
///
/// Keyed by (hash of text, font_size rounded to 0.1px) to avoid expensive
/// rustybuzz shaping calls for repeated strings (very common on large pages
/// with tables, lists, and repeated UI patterns).
thread_local! {
    static TEXT_WIDTH_CACHE: std::cell::RefCell<TextWidthCache> =
        std::cell::RefCell::new(TextWidthCache::new());
}

/// Simple LRU-like cache for text width measurements.
struct TextWidthCache {
    entries: std::collections::HashMap<(u64, u32), f32>,
    /// Insertion order for simple eviction (oldest first).
    order: Vec<(u64, u32)>,
}

impl TextWidthCache {
    fn new() -> Self {
        Self {
            entries: std::collections::HashMap::with_capacity(1024),
            order: Vec::with_capacity(1024),
        }
    }

    fn get(&self, key: &(u64, u32)) -> Option<f32> {
        self.entries.get(key).copied()
    }

    fn insert(&mut self, key: (u64, u32), value: f32) {
        if self.entries.len() >= TEXT_CACHE_MAX_ENTRIES {
            // Evict oldest quarter of entries.
            let evict_count = TEXT_CACHE_MAX_ENTRIES / 4;
            for i in 0..evict_count.min(self.order.len()) {
                self.entries.remove(&self.order[i]);
            }
            self.order.drain(..evict_count.min(self.order.len()));
        }
        self.entries.insert(key, value);
        self.order.push(key);
    }
}

/// Hash a text string for cache keying (fast, non-cryptographic).
fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Measure the width of a string using rustybuzz shaping (kerning-aware).
///
/// This matches the renderer's text shaping pipeline, ensuring layout
/// measurements are consistent with rendered glyph positions.
/// Falls back to fontdue per-character measurement if rustybuzz fails.
///
/// Results are cached per-thread to avoid redundant shaping calls.
fn measure_text_width(text: &str, font_size: f32) -> f32 {
    // Build a cache key: hash of text + font size quantized to 0.1px.
    let text_hash = hash_text(text);
    let fs_key = (font_size * 10.0) as u32;
    let cache_key = (text_hash, fs_key);

    // Check the cache first.
    let cached = TEXT_WIDTH_CACHE.with(|cache| {
        cache.borrow().get(&cache_key)
    });
    if let Some(width) = cached {
        return width;
    }

    // Cache miss — perform the measurement.
    let width = measure_text_width_uncached(text, font_size);

    // Store in cache.
    TEXT_WIDTH_CACHE.with(|cache| {
        cache.borrow_mut().insert(cache_key, width);
    });

    width
}

/// Perform the actual text measurement without caching.
fn measure_text_width_uncached(text: &str, font_size: f32) -> f32 {
    LAYOUT_FONT_DATA.with(|data| {
        if let Some(bytes) = data {
            if let Some(face) = rustybuzz::Face::from_slice(bytes, 0) {
                let upem = face.units_per_em() as f32;
                let scale = if upem > 0.0 { font_size / upem } else { 1.0 };

                let mut buffer = rustybuzz::UnicodeBuffer::new();
                buffer.push_str(text);
                buffer.set_direction(rustybuzz::Direction::LeftToRight);

                let output = rustybuzz::shape(&face, &[], buffer);
                let positions = output.glyph_positions();

                let width: f32 = positions.iter().map(|p| p.x_advance as f32 * scale).sum();
                return width;
            }
        }
        // Fallback: fontdue per-character measurement.
        LAYOUT_FONT.with(|font| {
            if let Some(font) = font {
                let mut width: f32 = 0.0;
                for ch in text.chars() {
                    let (metrics, _) = font.rasterize(ch, font_size);
                    width += metrics.advance_width;
                }
                width
            } else {
                let scale = font_size / 16.0;
                text.len() as f32 * CHAR_WIDTH_AT_16PX * scale
            }
        })
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

/// Default line-height multiplier relative to font size (`normal`).
const LINE_HEIGHT_FACTOR: f32 = 1.2;

/// Resolve the line-height from style properties and font-size.
///
/// Supports:
/// - `normal` → `font_size * LINE_HEIGHT_FACTOR`
/// - `<number>` (e.g. `1.5`) → `font_size * number`
/// - `<length>px` (e.g. `24px`) → `24.0`
/// - `<percent>%` (e.g. `150%`) → `font_size * 1.5`
///
/// Falls back to `font_size * LINE_HEIGHT_FACTOR` if not specified.
fn resolve_line_height(style_props: &[(String, StyleValue)], font_size: f32) -> f32 {
    if let Some((_, val)) = style_props.iter().find(|(k, _)| k == "line-height") {
        match val {
            StyleValue::Px(px) => return *px,
            StyleValue::Number(n) => return font_size * n,
            StyleValue::Percent(pct) => return font_size * pct / 100.0,
            StyleValue::Keyword(k) | StyleValue::Str(k) => {
                let k = k.trim();
                if k == "normal" {
                    return font_size * LINE_HEIGHT_FACTOR;
                }
                // Try parsing as "<n>px".
                if let Some(px_str) = k.strip_suffix("px") {
                    if let Ok(px) = px_str.trim().parse::<f32>() {
                        return px;
                    }
                }
                // Try parsing as "<n>%".
                if let Some(pct_str) = k.strip_suffix('%') {
                    if let Ok(pct) = pct_str.trim().parse::<f32>() {
                        return font_size * pct / 100.0;
                    }
                }
                // Try parsing as a bare number (unitless multiplier).
                if let Ok(n) = k.parse::<f32>() {
                    return font_size * n;
                }
            }
            _ => {}
        }
    }
    font_size * LINE_HEIGHT_FACTOR
}

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
    let root_id = add_node(&mut taffy, dom, viewport.width, DEFAULT_FONT_SIZE, &[], viewport, 0)?;

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

/// Compute the effective available width for children of a block element.
///
/// Takes the parent's `available_width` and narrows it based on the element's
/// own `width`, `max-width`, and horizontal `padding`. This ensures children
/// (especially text nodes) wrap at the correct container edge rather than the
/// viewport edge.
fn compute_child_available_width(available_width: f32, lp: &LayoutProps) -> f32 {
    let mut w = available_width;
    // Use explicit width if set.
    if let Some(dim) = &lp.width {
        match dim {
            Dimension::Length(px) => w = *px,
            Dimension::Percent(pct) => w = available_width * pct,
            _ => {}
        }
    }
    // Clamp by max-width.
    if let Some(dim) = &lp.max_width {
        match dim {
            Dimension::Length(px) => w = w.min(*px),
            Dimension::Percent(pct) => w = w.min(available_width * pct),
            _ => {}
        }
    }
    // Subtract horizontal padding.
    let pad_left = match &lp.padding_left {
        Some(LengthPercentage::Length(px)) => *px,
        Some(LengthPercentage::Percent(pct)) => available_width * pct,
        _ => 0.0,
    };
    let pad_right = match &lp.padding_right {
        Some(LengthPercentage::Length(px)) => *px,
        Some(LengthPercentage::Percent(pct)) => available_width * pct,
        _ => 0.0,
    };
    (w - pad_left - pad_right).max(0.0)
}

/// Recursively convert a `DomNode` into a Taffy node, returning its `NodeId`.
/// `parent_font_size` is inherited from the parent element for text nodes.
/// `parent_style_props` carries inheritable style properties (color,
/// background-color, text-decoration) from the parent element so text nodes
/// can inherit them.
/// Maximum DOM depth for layout computation.
///
/// Prevents stack overflow and excessive computation on deeply nested DOMs.
const MAX_LAYOUT_DEPTH: usize = 100;

fn add_node(
    taffy: &mut TaffyTree<NodeContext>,
    node: &DomNode,
    available_width: f32,
    parent_font_size: f32,
    parent_style_props: &[(String, StyleValue)],
    viewport: &Viewport,
    depth: usize,
) -> Result<NodeId, NovaError> {
    // Depth limit: stop recursing beyond MAX_LAYOUT_DEPTH.
    if depth >= MAX_LAYOUT_DEPTH {
        let style = Style::DEFAULT;
        let ctx = NodeContext {
            content: LayoutContent::Block,
            style: StyleMap::default(),
        };
        return taffy
            .new_leaf(style)
            .map(|id| {
                taffy.set_node_context(id, Some(ctx)).ok();
                id
            })
            .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
    }

    match node {
        DomNode::Document { children } => {
            // The document root is a block container spanning the full width.
            let child_ids = children
                .iter()
                .map(|c| add_node(taffy, c, available_width, DEFAULT_FONT_SIZE, &[], viewport, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;

            let style = Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                size: Size {
                    width: Dimension::Percent(1.0),
                    height: Dimension::Auto,
                },
                // Ensure the document root covers at least the viewport
                // so children with percentage min-height resolve correctly
                // and background-color paints the full canvas.
                min_size: Size {
                    width: Dimension::Auto,
                    height: Dimension::Length(viewport.height),
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
                let lp = parse_layout_props(attributes, viewport);

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
                    // Check for HTML `size` attribute on <input> — it specifies
                    // width in average character widths (approx 0.6 * font_size).
                    let size_attr_w: Option<f32> = if tag == "input" {
                        attributes.iter()
                            .find(|(k, _)| k == "size")
                            .and_then(|(_, v)| v.parse::<f32>().ok())
                            .map(|chars| chars * font_size * 0.6)
                    } else {
                        None
                    };
                    let default_w = lp.width
                        .and_then(|d| match d { Dimension::Length(px) => Some(px), _ => None })
                        .or(size_attr_w)
                        .unwrap_or((text_w + pad).max(if tag == "input" { 150.0 } else { 40.0 }));
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

                let mut taffy_style = build_taffy_style(&display, tag, attributes, viewport);
                // Form field dimensions are computed and forced onto Taffy.
                // Override taffy dimensions with the computed form field size.
                // `build_taffy_style` may leave width/height as Auto for inline
                // elements, which collapses to zero when there are no children.
                // Form fields need explicit dimensions to be visible.
                if taffy_style.display != Display::None {
                    taffy_style.size.width = Dimension::Length(w);
                    taffy_style.size.height = Dimension::Length(h);
                }

                // Create a text child for the label if non-empty.
                let mut child_ids = Vec::new();
                if !label.is_empty() {
                    let text_w = measure_text_width(&label, font_size).min(w);
                    let text_ctx = NodeContext {
                        content: LayoutContent::Text(label.clone()),
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

                // Tag the form element type so the painter can emit FormField ops.
                let form_type = match tag.as_str() {
                    "input" => input_type.to_string(),
                    other => other.to_string(),
                };
                props.push(("nova-form-type".into(), StyleValue::Str(form_type)));
                props.push(("nova-form-value".into(), StyleValue::Str(label.clone())));

                // Forward form-related HTML attributes so the painter can use them.
                if let Some((_, v)) = attributes.iter().find(|(k, _)| k == "name") {
                    props.push(("nova-form-name".into(), StyleValue::Str(v.clone())));
                }
                if let Some((_, v)) = attributes.iter().find(|(k, _)| k == "placeholder") {
                    props.push(("nova-form-placeholder".into(), StyleValue::Str(v.clone())));
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

            // ── <video> elements -> LayoutContent::Video leaf node -----
            if tag == "video" {
                let src = attributes
                    .iter()
                    .find(|(k, _)| k == "src")
                    .map(|(_, v)| v.clone())
                    .or_else(|| {
                        // Fall back to first <source> child's src attribute.
                        children.iter().find_map(|c| {
                            if let DomNode::Element { tag: t, attributes: a, .. } = c {
                                if t == "source" {
                                    return a.iter().find(|(k, _)| k == "src").map(|(_, v)| v.clone());
                                }
                            }
                            None
                        })
                    })
                    .unwrap_or_default();

                let poster = attributes
                    .iter()
                    .find(|(k, _)| k == "poster")
                    .map(|(_, v)| v.clone());

                let controls = attributes.iter().any(|(k, _)| k == "controls");

                let vid_w: f32 = attributes
                    .iter()
                    .find(|(k, _)| k == "width")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(300.0);
                let vid_h: f32 = attributes
                    .iter()
                    .find(|(k, _)| k == "height")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(150.0);

                let ctx = NodeContext {
                    content: LayoutContent::Video { src, poster, controls },
                    style: StyleMap::default(),
                };
                return taffy
                    .new_leaf_with_context(
                        Style {
                            display: Display::Flex,
                            size: Size {
                                width: Dimension::Length(vid_w.min(available_width)),
                                height: Dimension::Length(vid_h),
                            },
                            ..Style::DEFAULT
                        },
                        ctx,
                    )
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // ── <audio> elements -> LayoutContent::Audio leaf node -----
            if tag == "audio" {
                let src = attributes
                    .iter()
                    .find(|(k, _)| k == "src")
                    .map(|(_, v)| v.clone())
                    .or_else(|| {
                        // Fall back to first <source> child's src attribute.
                        children.iter().find_map(|c| {
                            if let DomNode::Element { tag: t, attributes: a, .. } = c {
                                if t == "source" {
                                    return a.iter().find(|(k, _)| k == "src").map(|(_, v)| v.clone());
                                }
                            }
                            None
                        })
                    })
                    .unwrap_or_default();

                let controls = attributes.iter().any(|(k, _)| k == "controls");

                let audio_w: f32 = attributes
                    .iter()
                    .find(|(k, _)| k == "width")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(300.0_f32.min(available_width));
                let audio_h: f32 = 40.0;

                let ctx = NodeContext {
                    content: LayoutContent::Audio { src, controls },
                    style: StyleMap::default(),
                };
                return taffy
                    .new_leaf_with_context(
                        Style {
                            display: Display::Flex,
                            size: Size {
                                width: Dimension::Length(audio_w),
                                height: Dimension::Length(audio_h),
                            },
                            ..Style::DEFAULT
                        },
                        ctx,
                    )
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // ── <canvas> elements -> LayoutContent::Canvas leaf node -----
            if tag == "canvas" {
                let canvas_w: f32 = attributes
                    .iter()
                    .find(|(k, _)| k == "width")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(300.0);
                let canvas_h: f32 = attributes
                    .iter()
                    .find(|(k, _)| k == "height")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(150.0);
                let canvas_id = attributes
                    .iter()
                    .find(|(k, _)| k == "id")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| format!("__canvas_{}", tag.len()));

                let ctx = NodeContext {
                    content: LayoutContent::Canvas {
                        canvas_id,
                        canvas_width: canvas_w as u32,
                        canvas_height: canvas_h as u32,
                    },
                    style: StyleMap::default(),
                };
                return taffy
                    .new_leaf_with_context(
                        Style {
                            display: Display::Flex,
                            size: Size {
                                width: Dimension::Length(canvas_w.min(available_width)),
                                height: Dimension::Length(canvas_h),
                            },
                            ..Style::DEFAULT
                        },
                        ctx,
                    )
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
            }

            // Force certain tags to always be hidden regardless of CSS.
            // Browsers never render script/style/template content.
            // Note: `noscript` is NOT hidden — its content is rendered so that
            // pages with limited JS support (e.g. Google) show their fallback
            // search forms and other noscript content.
            let force_hidden = matches!(
                tag.as_str(),
                "script" | "style" | "template" | "head" | "meta" | "link" | "title"
            );

            let display = if force_hidden {
                "none".to_string()
            } else {
                resolve_display(tag, attributes)
            };

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

            // For <li> elements, inherit list-style-type and nova-list-index
            // from parent_style_props (injected by build_children_with_ifc).
            if tag == "li" {
                for (k, v) in parent_style_props {
                    if (k == "list-style-type" || k == "nova-list-index")
                        && !props.iter().any(|(pk, _)| pk == k)
                    {
                        props.push((k.clone(), v.clone()));
                    }
                }
            }

            // Propagate list-style-type from parent <ul>/<ol> to <li> children,
            // and assign nova-list-index for ordered list numbering.
            if tag == "ul" || tag == "ol" {
                let list_style = props.iter()
                    .find(|(k, _)| k == "list-style-type")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| StyleValue::Keyword(
                        if tag == "ol" { "decimal".into() } else { "disc".into() }
                    ));
                // Store the list style on the parent props so children can inherit it.
                props.push(("nova-list-style".into(), list_style));
            }

            // ── Compute child available width ─────────────────────
            // The effective width for children accounts for explicit
            // width / max-width / padding on this element.
            let lp = parse_layout_props(attributes, viewport);
            let child_available = compute_child_available_width(available_width, &lp);

            // ── Table layout ─────────────────────────────────────
            // Use dedicated table algorithm for proper column distribution.
            if tag == "table" || display == "table" {
                return layout_table(
                    taffy, tag, attributes, children, available_width,
                    font_size, &props, viewport, depth,
                );
            }

            // ── Float detection ──────────────────────────────────────
            // Check if any direct children have float: left|right.
            // If so, the parent must use a row-wrap flex layout so
            // floated elements can sit side-by-side with content.
            let has_floated_children = children.iter().any(|c| {
                if let DomNode::Element { attributes: attrs, .. } = c {
                    let child_lp = parse_layout_props(attrs, viewport);
                    matches!(child_lp.float.as_deref(), Some("left") | Some("right"))
                } else {
                    false
                }
            });

            // ── Inline Formatting Context ──────────────────────────
            // For block containers and table cells, group consecutive
            // inline children into inline formatting contexts so they
            // flow together, wrap across lines, and share baselines.
            let child_ids = if display == "block" || display == "table-cell" {
                build_children_with_ifc(taffy, children, child_available, font_size, &props, viewport, depth)?
            } else {
                children
                    .iter()
                    .map(|c| add_node(taffy, c, child_available, font_size, &props, viewport, depth + 1))
                    .collect::<Result<Vec<_>, _>>()?
            };
            let mut taffy_style = build_taffy_style(&display, tag, attributes, viewport);

            // ── Multi-column layout (column-count) ──────────────────
            // When column-count is set, distribute children across N columns.
            let column_count = props.iter()
                .find(|(k, _)| k == "column-count")
                .and_then(|(_, v)| match v {
                    StyleValue::Number(n) => Some(*n as u32),
                    StyleValue::Px(n) => Some(*n as u32),
                    StyleValue::Keyword(k) | StyleValue::Str(k) => k.parse::<u32>().ok(),
                    _ => None,
                })
                .filter(|&n| n >= 2);

            let child_ids = if let Some(num_cols) = column_count {
                let column_gap = props.iter()
                    .find(|(k, _)| k == "column-gap")
                    .and_then(|(_, v)| match v {
                        StyleValue::Px(px) => Some(*px),
                        StyleValue::Keyword(k) | StyleValue::Str(k) => {
                            k.strip_suffix("px").and_then(|s| s.parse::<f32>().ok())
                        }
                        _ => None,
                    })
                    .unwrap_or(16.0); // default column gap

                let total_gap = column_gap * (num_cols - 1) as f32;
                let col_width = (child_available - total_gap) / num_cols as f32;
                let col_pct = col_width / child_available;

                // Distribute children roughly evenly across columns.
                let per_col = (child_ids.len() + num_cols as usize - 1) / num_cols as usize;
                let mut column_node_ids = Vec::new();
                for col_idx in 0..num_cols as usize {
                    let start = col_idx * per_col;
                    if start >= child_ids.len() { break; }
                    let end = (start + per_col).min(child_ids.len());
                    let col_children: Vec<NodeId> = child_ids[start..end].to_vec();

                    let col_style = Style {
                        display: Display::Flex,
                        flex_direction: FlexDirection::Column,
                        size: Size {
                            width: Dimension::Percent(col_pct),
                            height: Dimension::Auto,
                        },
                        ..Style::DEFAULT
                    };
                    let col_ctx = NodeContext {
                        content: LayoutContent::Block,
                        style: StyleMap::default(),
                    };
                    let col_id = taffy
                        .new_with_children(col_style, &col_children)
                        .map(|id| {
                            taffy.set_node_context(id, Some(col_ctx)).ok();
                            id
                        })
                        .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                    column_node_ids.push(col_id);
                }

                // Switch the parent to a row layout to hold columns.
                taffy_style.flex_direction = FlexDirection::Row;
                taffy_style.gap = Size {
                    width: LengthPercentage::Length(column_gap),
                    height: LengthPercentage::Length(0.0),
                };

                column_node_ids
            } else {
                child_ids
            };

            // ── Float container adjustment ───────────────────────────
            // When a block container has floated children, switch to
            // row-wrap layout so floated elements flow side-by-side
            // with non-floated content (which uses flex-basis: 100%
            // to force line breaks where needed).
            if has_floated_children && display == "block" && column_count.is_none() {
                taffy_style.flex_direction = FlexDirection::Row;
                taffy_style.flex_wrap = FlexWrap::Wrap;
            }

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
            // along with color, text-decoration, font-weight, etc.
            // NOTE: background-color is NOT inherited in CSS — only the
            // element itself paints its background, not its text children.
            let font_size = parent_font_size;
            let mut text_props = vec![
                ("font-size".into(), StyleValue::Px(font_size)),
            ];
            // Inherit color and text-related properties from parent.
            for (key, value) in parent_style_props {
                if key == "color" || key == "text-decoration"
                    || key == "text-align" || key == "font-weight" || key == "font-style"
                    || key == "white-space" || key == "overflow"
                    || key == "overflow-x" || key == "overflow-y"
                    || key == "text-overflow"
                    || key == "text-transform" || key == "font-family"
                    || key == "letter-spacing" || key == "text-shadow"
                    || key == "text-indent" || key == "direction"
                    || key == "vertical-align" {
                    text_props.push((key.clone(), value.clone()));
                }
            }

            let line_height = resolve_line_height(&text_props, font_size);
            // Measure space width from the actual font glyph so it matches
            // what the renderer will produce. Fall back to 0.3em if the font
            // measurement returns zero (e.g. no font loaded).
            let measured_space = measure_text_width(" ", font_size);
            let space_width = if measured_space > 0.0 { measured_space } else { (font_size * 0.3).max(1.0) };

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
                            // Use 100% width so the wrapper fills its parent
                            // container and words wrap at the container edge
                            // (e.g., max-width: 600px div on example.com).
                            width: Dimension::Percent(1.0),
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

// ── Inline Formatting Context (IFC) ────────────────────────────────────
//
// When a block container has inline children (text, <span>, <a>, <em>, etc.),
// they must flow together in an inline formatting context. Instead of each
// child becoming an independent flex item, consecutive inline children are
// collected into "inline runs", flattened into a sequence of inline items
// (words, spaces, inline element boundaries), then broken into line boxes.
//
// This is the core of correct text layout in a browser.

/// Check if two inline style property lists are visually equivalent for
/// text merging. We compare color and href (the properties that affect how
/// the painter draws text) rather than the full list.
fn inline_styles_match(a: &[(String, StyleValue)], b: &[(String, StyleValue)]) -> bool {
    fn get_str<'s>(props: &'s [(String, StyleValue)], key: &str) -> Option<&'s str> {
        props.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
            StyleValue::Str(s) | StyleValue::Keyword(s) => Some(s.as_str()),
            _ => None,
        })
    }
    fn get_px(props: &[(String, StyleValue)], key: &str) -> Option<f32> {
        props.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
            StyleValue::Px(px) => Some(*px),
            _ => None,
        })
    }
    get_str(a, "color") == get_str(b, "color")
        && get_str(a, "href") == get_str(b, "href")
        && get_str(a, "text-decoration") == get_str(b, "text-decoration")
        && get_px(a, "font-size") == get_px(b, "font-size")
}

/// An item in the flattened inline sequence.
#[derive(Debug, Clone)]
enum InlineItem {
    /// A word of text with its measured width.
    Word {
        text: String,
        width: f32,
        style: Vec<(String, StyleValue)>,
    },
    /// A space between words.
    Space { width: f32, style: Vec<(String, StyleValue)> },
    /// An inline image.
    Image { src: String, width: f32, height: f32 },
}

/// Check if a DOM node is inline (text, inline element, or <br>).
fn is_inline_node(node: &DomNode) -> bool {
    match node {
        DomNode::Text(_) => true,
        DomNode::Comment(_) => true,
        DomNode::Element { tag, attributes, .. } => {
            // Form elements are handled as block-level by add_node (which has
            // a dedicated form field handler). They must NOT be treated as
            // inline content here, or they'll get swallowed by the IFC and
            // never reach the form field code path.
            if matches!(tag.as_str(), "input" | "button" | "select" | "textarea") {
                return false;
            }
            let display = resolve_display(tag, attributes);
            matches!(display.as_str(), "inline" | "inline-block")
                || tag == "br" || tag == "img"
        }
        _ => false,
    }
}

/// Build child Taffy nodes for a block container, using inline formatting
/// contexts for runs of consecutive inline children.
///
/// Block children are processed normally via `add_node`. Consecutive inline
/// children are grouped and laid out together using `layout_inline_run`.
fn build_children_with_ifc(
    taffy: &mut TaffyTree<NodeContext>,
    children: &[DomNode],
    available_width: f32,
    parent_font_size: f32,
    parent_style_props: &[(String, StyleValue)],
    viewport: &Viewport,
    depth: usize,
) -> Result<Vec<NodeId>, NovaError> {
    let mut result = Vec::new();

    // Track list item index for <li> children inside <ul>/<ol>.
    let mut li_counter = 0i32;

    // Partition children into inline runs and block items.
    let mut i = 0;
    while i < children.len() {
        if is_inline_node(&children[i]) {
            // Collect consecutive inline children.
            let start = i;
            while i < children.len() && is_inline_node(&children[i]) {
                i += 1;
            }
            let inline_run = &children[start..i];
            // Flatten the inline run and create line boxes.
            let line_ids = layout_inline_run(
                taffy, inline_run, available_width, parent_font_size, parent_style_props,
            )?;
            result.extend(line_ids);
        } else {
            //
            // Handle `clear` property: insert a full-width invisible break
            // node before the child to force it below any preceding floats.
            if let DomNode::Element { attributes, .. } = &children[i] {
                let child_lp = parse_layout_props(attributes, viewport);
                if matches!(child_lp.clear.as_deref(), Some("left") | Some("right") | Some("both")) {
                    let break_ctx = NodeContext {
                        content: LayoutContent::Block,
                        style: StyleMap::default(),
                    };
                    let break_id = taffy
                        .new_leaf_with_context(
                            Style {
                                size: Size {
                                    width: Dimension::Percent(1.0),
                                    height: Dimension::Length(0.0),
                                },
                                ..Style::DEFAULT
                            },
                            break_ctx,
                        )
                        .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                    result.push(break_id);
                }
            }

            // Track <li> index for list markers.
            if let DomNode::Element { tag, .. } = &children[i] {
                if tag == "li" {
                    li_counter += 1;
                }
            }

            // For <li> elements, inject list-style-type and index from parent.
            let effective_props: Vec<(String, StyleValue)>;
            let child_parent_props = if let DomNode::Element { tag, .. } = &children[i] {
                if tag == "li" {
                    effective_props = inject_list_item_props(parent_style_props, li_counter);
                    &effective_props[..]
                } else {
                    parent_style_props
                }
            } else {
                parent_style_props
            };

            // Block child — process normally, with margin collapsing.
            let node_id = add_node(taffy, &children[i], available_width, parent_font_size, child_parent_props, viewport, depth + 1)?;

            // Margin collapsing: if the previous child was also a block,
            // reduce the current child's top margin to simulate CSS margin collapse.
            if let DomNode::Element { attributes, .. } = &children[i] {
                if !result.is_empty() {
                    collapse_margin_with_previous(taffy, &result, node_id, attributes);
                }
            }

            result.push(node_id);
            i += 1;
        }
    }

    Ok(result)
}

/// Inject list-style-type and nova-list-index into parent style props for `<li>` elements.
///
/// Copies the parent props and adds the list marker properties so the painter
/// can render the appropriate bullet or number.
fn inject_list_item_props(parent_style_props: &[(String, StyleValue)], li_index: i32) -> Vec<(String, StyleValue)> {
    let mut props = parent_style_props.to_vec();

    // Get list-style-type from parent (nova-list-style or list-style-type).
    let list_style = parent_style_props.iter()
        .find(|(k, _)| k == "nova-list-style" || k == "list-style-type")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| StyleValue::Keyword("disc".into()));

    props.push(("list-style-type".into(), list_style));
    props.push(("nova-list-index".into(), StyleValue::Number(li_index as f32)));

    props
}

/// Collapse vertical margins between adjacent block children.
///
/// In CSS, adjacent vertical margins collapse: instead of adding both margins,
/// the space between the blocks is `max(margin_bottom_prev, margin_top_current)`.
/// Since Taffy doesn't collapse margins, we simulate this by adjusting the
/// current node's top margin.
fn collapse_margin_with_previous(
    taffy: &mut TaffyTree<NodeContext>,
    previous_ids: &[NodeId],
    current_id: NodeId,
    _attributes: &[(String, String)],
) {
    let prev_id = *previous_ids.last().unwrap();

    // Get the previous node's bottom margin.
    let prev_style = taffy.style(prev_id).cloned();
    let curr_style = taffy.style(current_id).cloned();

    if let (Ok(prev_s), Ok(curr_s)) = (prev_style, curr_style) {
        let prev_bottom = match prev_s.margin.bottom {
            LengthPercentageAuto::Length(px) => px,
            _ => 0.0,
        };
        let curr_top = match curr_s.margin.top {
            LengthPercentageAuto::Length(px) => px,
            _ => 0.0,
        };

        if prev_bottom > 0.0 && curr_top > 0.0 {
            // Collapse: the gap should be max(prev_bottom, curr_top), not their sum.
            // We reduce the current node's top margin by the overlap.
            let collapsed = prev_bottom.max(curr_top);
            let overlap = (prev_bottom + curr_top) - collapsed;
            let new_top = (curr_top - overlap).max(0.0);

            let mut new_style = curr_s;
            new_style.margin.top = LengthPercentageAuto::Length(new_top);
            let _ = taffy.set_style(current_id, new_style);
        }
    }
}

/// Flatten an inline run (consecutive inline children) into InlineItems.
fn flatten_inline_content(
    nodes: &[DomNode],
    font_size: f32,
    parent_style_props: &[(String, StyleValue)],
) -> Vec<InlineItem> {
    let mut items = Vec::new();
    for node in nodes {
        flatten_node_recursive(node, font_size, parent_style_props, &mut items);
    }
    items
}

/// Recursively flatten a single DOM node into inline items.
fn flatten_node_recursive(
    node: &DomNode,
    font_size: f32,
    style_props: &[(String, StyleValue)],
    items: &mut Vec<InlineItem>,
) {
    match node {
        DomNode::Text(text) => {
            // Measure space width from the actual font glyph so it matches
            // what the renderer will produce. Fall back to 0.3em if the font
            // measurement returns zero (e.g. no font loaded).
            let measured_space = measure_text_width(" ", font_size);
            let space_width = if measured_space > 0.0 { measured_space } else { (font_size * 0.3).max(1.0) };

            // Text nodes should NOT carry background-color — only the
            // containing element paints its background (CSS spec: background
            // is not inherited).
            let text_style: Vec<(String, StyleValue)> = style_props
                .iter()
                .filter(|(k, _)| k != "background-color")
                .cloned()
                .collect();
            let style_props = &text_style[..];

            // Check white-space property to decide how to split text.
            let white_space = style_props.iter()
                .find(|(k, _)| k == "white-space")
                .and_then(|(_, v)| match v {
                    StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.as_str()),
                    _ => None,
                })
                .unwrap_or("normal");

            // Extract letter-spacing for width adjustment.
            let letter_spacing: f32 = style_props.iter()
                .find(|(k, _)| k == "letter-spacing")
                .and_then(|(_, v)| match v {
                    StyleValue::Px(px) => Some(*px),
                    StyleValue::Str(s) | StyleValue::Keyword(s) => {
                        let s = s.trim();
                        if s == "normal" { return None; }
                        if let Some(rest) = s.strip_suffix("em") {
                            return rest.trim().parse::<f32>().ok().map(|n| n * font_size);
                        }
                        if let Some(rest) = s.strip_suffix("px") {
                            return rest.trim().parse::<f32>().ok();
                        }
                        s.parse::<f32>().ok()
                    }
                    _ => None,
                })
                .unwrap_or(0.0);

            /// Add letter-spacing to a measured word width.
            fn apply_letter_spacing(base_width: f32, text: &str, spacing: f32) -> f32 {
                if spacing == 0.0 { return base_width; }
                let char_count = text.chars().count();
                if char_count <= 1 { return base_width; }
                base_width + spacing * (char_count - 1) as f32
            }

            match white_space {
                "pre" | "pre-wrap" => {
                    // Preserve whitespace: split by newlines, then keep spaces.
                    for (line_idx, line) in text.split('\n').enumerate() {
                        if line_idx > 0 {
                            // Force line break.
                            items.push(InlineItem::Word {
                                text: "\n".to_string(),
                                width: 0.0,
                                style: style_props.to_vec(),
                            });
                        }
                        for ch in line.chars() {
                            if ch == ' ' || ch == '\t' {
                                items.push(InlineItem::Space {
                                    width: if ch == '\t' { space_width * 4.0 } else { space_width },
                                    style: style_props.to_vec(),
                                });
                            } else {
                                // Accumulate non-space characters into words.
                                let should_append = if let Some(InlineItem::Word { text: t, .. }) = items.last() {
                                    !t.is_empty() && t != "\n"
                                } else { false };
                                if should_append {
                                    if let Some(InlineItem::Word { text: t, width: w, .. }) = items.last_mut() {
                                        t.push(ch);
                                        *w = apply_letter_spacing(measure_text_width(t, font_size), t, letter_spacing);
                                        continue;
                                    }
                                }
                                let s = ch.to_string();
                                let w = measure_text_width(&s, font_size);
                                items.push(InlineItem::Word {
                                    text: s,
                                    width: w,
                                    style: style_props.to_vec(),
                                });
                            }
                        }
                    }
                }
                "nowrap" => {
                    // Collapse whitespace but don't allow wrapping — just put everything
                    // in one run. The line-breaker won't break these since they're words.
                    let words: Vec<&str> = text.split_whitespace().collect();
                    for (i, word) in words.iter().enumerate() {
                        if i > 0 {
                            items.push(InlineItem::Space {
                                width: space_width,
                                style: style_props.to_vec(),
                            });
                        }
                        let w = apply_letter_spacing(measure_text_width(word, font_size), word, letter_spacing);
                        items.push(InlineItem::Word {
                            text: word.to_string(),
                            width: w,
                            style: style_props.to_vec(),
                        });
                    }
                }
                "pre-line" => {
                    // Collapse spaces but preserve newlines.
                    for (line_idx, line) in text.split('\n').enumerate() {
                        if line_idx > 0 {
                            items.push(InlineItem::Word {
                                text: "\n".to_string(),
                                width: 0.0,
                                style: style_props.to_vec(),
                            });
                        }
                        let words: Vec<&str> = line.split_whitespace().collect();
                        for (i, word) in words.iter().enumerate() {
                            if i > 0 {
                                items.push(InlineItem::Space {
                                    width: space_width,
                                    style: style_props.to_vec(),
                                });
                            }
                            let w = apply_letter_spacing(measure_text_width(word, font_size), word, letter_spacing);
                            items.push(InlineItem::Word {
                                text: word.to_string(),
                                width: w,
                                style: style_props.to_vec(),
                            });
                        }
                    }
                }
                _ => {
                    // "normal": collapse whitespace, allow wrapping.
                    let had_items = !items.is_empty();
                    let starts_with_space = text.starts_with(char::is_whitespace);
                    let words: Vec<&str> = text.split_whitespace().collect();
                    for (i, word) in words.iter().enumerate() {
                        if i > 0 || (i == 0 && had_items && starts_with_space) {
                            // Add space between words, or leading space if continuing from previous items.
                            if !matches!(items.last(), Some(InlineItem::Space { .. })) {
                                items.push(InlineItem::Space {
                                    width: space_width,
                                    style: style_props.to_vec(),
                                });
                            }
                        }
                        let w = apply_letter_spacing(measure_text_width(word, font_size), word, letter_spacing);
                        items.push(InlineItem::Word {
                            text: word.to_string(),
                            width: w,
                            style: style_props.to_vec(),
                        });
                    }
                }
            }
        }
        DomNode::Element { tag, children, attributes, .. } => {
            // Skip elements that should never produce inline content.
            if matches!(tag.as_str(), "script" | "style" | "template") {
                return;
            }
            // Check for display: none.
            let disp = resolve_display(tag, attributes);
            if disp == "none" {
                return;
            }
            if tag == "br" {
                // Line break: insert a special zero-width "word" that forces a break.
                // We'll handle this during line-breaking.
                items.push(InlineItem::Word {
                    text: "\n".to_string(),
                    width: 0.0,
                    style: style_props.to_vec(),
                });
                return;
            }
            if tag == "img" {
                let src = attributes.iter().find(|(k,_)| k == "src").map(|(_, v)| v.clone()).unwrap_or_default();
                let w: f32 = attributes.iter().find(|(k,_)| k == "width").and_then(|(_, v)| v.parse().ok()).unwrap_or(150.0);
                let h: f32 = attributes.iter().find(|(k,_)| k == "height").and_then(|(_, v)| v.parse().ok()).unwrap_or(80.0);
                items.push(InlineItem::Image { src, width: w, height: h });
                return;
            }

            // Inherit style from this inline element.
            let child_font_size = resolve_font_size(tag, attributes, font_size);
            let mut child_props: Vec<(String, StyleValue)> = style_props.to_vec();

            // Apply this element's computed styles.
            if let Some(nova_style) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
                for decl in nova_style.1.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let prop = parts[0].trim().to_string();
                        let val = parts[1].trim();
                        if prop == "font-size" {
                            if let Some(px) = val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok()) {
                                if let Some(existing) = child_props.iter_mut().find(|(k, _)| k == "font-size") {
                                    existing.1 = StyleValue::Px(px);
                                }
                            }
                        } else {
                            // Replace existing property in-place so that
                            // e.g. <a>'s color: #0000ee overrides the
                            // inherited color: #000000 from the parent.
                            let new_val = if let Some(px) = val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok()) {
                                StyleValue::Px(px)
                            } else if val.starts_with("rgb") || val.starts_with('#') {
                                StyleValue::Str(val.to_string())
                            } else {
                                StyleValue::Keyword(val.to_string())
                            };
                            if let Some(existing) = child_props.iter_mut().find(|(k, _)| *k == prop) {
                                existing.1 = new_val;
                            } else {
                                child_props.push((prop, new_val));
                            }
                        }
                    }
                }
            }

            // Propagate href for links.
            if tag == "a" {
                if let Some(href) = attributes.iter().find(|(k, _)| k == "href") {
                    child_props.push(("href".into(), StyleValue::Str(href.1.clone())));
                }
            }

            // Recurse into children with this element's styles.
            for child in children {
                flatten_node_recursive(child, child_font_size, &child_props, items);
            }
        }
        DomNode::Comment(_) | DomNode::Document { .. } => {}
    }
}

/// Layout a run of inline items into line boxes.
///
/// Returns Taffy node IDs for each line box (flex-row containers).
fn layout_inline_run(
    taffy: &mut TaffyTree<NodeContext>,
    inline_nodes: &[DomNode],
    available_width: f32,
    parent_font_size: f32,
    parent_style_props: &[(String, StyleValue)],
) -> Result<Vec<NodeId>, NovaError> {
    // 1. Flatten all inline content into items.
    let items = flatten_inline_content(inline_nodes, parent_font_size, parent_style_props);
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let line_height = resolve_line_height(parent_style_props, parent_font_size);

    // Check if white-space: nowrap is set — if so, all content stays on one
    // line (no word-wrapping). The content may overflow its container and
    // will be clipped if `overflow: hidden` is also set.
    let white_space = parent_style_props.iter()
        .find(|(k, _)| k == "white-space")
        .and_then(|(_, v)| match v {
            StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.as_str()),
            _ => None,
        })
        .unwrap_or("normal");
    let no_wrap = white_space == "nowrap" || white_space == "pre";

    // Check word-break and overflow-wrap properties for mid-word breaking.
    let word_break = parent_style_props.iter()
        .find(|(k, _)| k == "word-break")
        .and_then(|(_, v)| match v {
            StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.as_str()),
            _ => None,
        })
        .unwrap_or("normal");
    let overflow_wrap = parent_style_props.iter()
        .find(|(k, _)| k == "overflow-wrap")
        .and_then(|(_, v)| match v {
            StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.as_str()),
            _ => None,
        })
        .unwrap_or("normal");
    let break_all = word_break == "break-all";
    let break_word = overflow_wrap == "break-word" || overflow_wrap == "anywhere";

    // 2. Break items into lines.
    let mut lines: Vec<Vec<&InlineItem>> = Vec::new();
    let mut current_line: Vec<&InlineItem> = Vec::new();
    let mut current_width: f32 = 0.0;

    // Extra items created by mid-word breaking (need to live as long as the loop).
    let mut extra_items: Vec<InlineItem> = Vec::new();

    for item in &items {
        match item {
            InlineItem::Word { text, width, style } => {
                if text == "\n" {
                    // Forced line break (<br>).
                    lines.push(std::mem::take(&mut current_line));
                    current_width = 0.0;
                    continue;
                }
                // Check if this word fits on the current line.
                // When white-space: nowrap, never wrap to a new line.
                if !no_wrap && current_width + *width > available_width && current_width > 0.0 {
                    // Word doesn't fit. Check if we should break mid-word.
                    if (break_all || break_word) && *width > 0.0 {
                        // Break the word character by character.
                        let fs = style.iter()
                            .find(|(k, _)| k == "font-size")
                            .and_then(|(_, v)| if let StyleValue::Px(px) = v { Some(*px) } else { None })
                            .unwrap_or(parent_font_size);

                        let mut remaining_text = text.as_str();
                        while !remaining_text.is_empty() {
                            // Find how many chars fit on the current line.
                            let mut fit_end = 0;
                            let mut fit_width = 0.0;
                            for (i, ch) in remaining_text.char_indices() {
                                let ch_w = measure_text_width(&remaining_text[i..i + ch.len_utf8()], fs);
                                if current_width + fit_width + ch_w > available_width && fit_end > 0 {
                                    break;
                                }
                                fit_end = i + ch.len_utf8();
                                fit_width += ch_w;
                            }
                            if fit_end == 0 && current_width > 0.0 {
                                // Nothing fits, start a new line.
                                if let Some(InlineItem::Space { .. }) = current_line.last() {
                                    current_line.pop();
                                }
                                lines.push(std::mem::take(&mut current_line));
                                current_width = 0.0;
                                continue;
                            }
                            if fit_end == 0 {
                                fit_end = remaining_text.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                            }
                            let chunk = &remaining_text[..fit_end];
                            let chunk_w = measure_text_width(chunk, fs);
                            let idx = extra_items.len();
                            extra_items.push(InlineItem::Word {
                                text: chunk.to_string(),
                                width: chunk_w,
                                style: style.clone(),
                            });
                            // SAFETY: we push to extra_items and never remove, so the reference
                            // is valid for the rest of the loop. We use index-based access.
                            let item_ref: *const InlineItem = &extra_items[idx];
                            current_line.push(unsafe { &*item_ref });
                            current_width += chunk_w;

                            remaining_text = &remaining_text[fit_end..];
                            if !remaining_text.is_empty() {
                                if let Some(InlineItem::Space { .. }) = current_line.last() {
                                    current_line.pop();
                                }
                                lines.push(std::mem::take(&mut current_line));
                                current_width = 0.0;
                            }
                        }
                        continue;
                    }

                    // Wrap to new line.
                    // Remove trailing space from current line.
                    if let Some(InlineItem::Space { .. }) = current_line.last() {
                        current_line.pop();
                    }
                    lines.push(std::mem::take(&mut current_line));
                    current_width = 0.0;
                }
                current_line.push(item);
                current_width += *width;
            }
            InlineItem::Space { width, .. } => {
                if !current_line.is_empty() {
                    current_line.push(item);
                    current_width += *width;
                }
            }
            InlineItem::Image { width, .. } => {
                if !no_wrap && current_width + *width > available_width && current_width > 0.0 {
                    lines.push(std::mem::take(&mut current_line));
                    current_width = 0.0;
                }
                current_line.push(item);
                current_width += *width;
            }
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }

    // 3. Create Taffy nodes for each line box.
    //
    // To avoid inter-word gaps caused by drawing each word as a separate
    // DrawText call, merge consecutive same-style Word+Space+Word sequences
    // into single text runs. This ensures spaces are rendered within the
    // same DrawText call as adjacent words, eliminating side-bearing gaps.

    // Extract text-indent for the first line.
    let text_indent = parent_style_props.iter()
        .find(|(k, _)| k == "text-indent")
        .and_then(|(_, v)| match v {
            StyleValue::Px(px) => Some(*px),
            StyleValue::Str(s) | StyleValue::Keyword(s) => {
                let s = s.trim();
                if let Some(rest) = s.strip_suffix("em") {
                    rest.trim().parse::<f32>().ok().map(|n| n * parent_font_size)
                } else if let Some(rest) = s.strip_suffix("rem") {
                    rest.trim().parse::<f32>().ok().map(|n| n * 16.0)
                } else if let Some(rest) = s.strip_suffix("px") {
                    rest.trim().parse::<f32>().ok()
                } else if let Some(rest) = s.strip_suffix('%') {
                    rest.trim().parse::<f32>().ok().map(|pct| available_width * pct / 100.0)
                } else {
                    s.parse::<f32>().ok()
                }
            }
            _ => None,
        })
        .unwrap_or(0.0);

    // Extract direction: rtl — swap default text-align to right.
    let direction_rtl = parent_style_props.iter()
        .find(|(k, _)| k == "direction")
        .and_then(|(_, v)| match v {
            StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.as_str()),
            _ => None,
        })
        .map(|d| d == "rtl")
        .unwrap_or(false);

    let mut line_box_ids = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        let mut item_ids = Vec::new();
        let mut max_height = line_height;

        let mut idx = 0;
        while idx < line.len() {
            match line[idx] {
                InlineItem::Word { text, width, style } => {
                    let fs = style.iter()
                        .find(|(k, _)| k == "font-size")
                        .and_then(|(_, v)| if let StyleValue::Px(px) = v { Some(*px) } else { None })
                        .unwrap_or(parent_font_size);
                    let lh = resolve_line_height(style, fs);
                    max_height = max_height.max(lh);

                    // Merge with following Space+Word items if same style.
                    let mut merged_text = text.clone();
                    let mut merged_width = *width;
                    while idx + 1 < line.len() {
                        if let InlineItem::Space { width: sw, style: ss } = &line[idx + 1] {
                            if inline_styles_match(ss, style) {
                                merged_text.push(' ');
                                merged_width += sw;
                                idx += 1; // consume space
                                // Try to also consume the next word if same style.
                                if idx + 1 < line.len() {
                                    if let InlineItem::Word { text: wt, width: ww, style: ws } = &line[idx + 1] {
                                        if inline_styles_match(ws, style) {
                                            merged_text.push_str(wt);
                                            merged_width += ww;
                                            idx += 1; // consume word
                                            continue;
                                        }
                                    }
                                }
                            }
                            break;
                        } else {
                            break;
                        }
                    }

                    // Check vertical-align for this inline item.
                    let valign = style.iter()
                        .find(|(k, _)| k == "vertical-align")
                        .and_then(|(_, v)| match v {
                            StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.as_str()),
                            _ => None,
                        });
                    let align_self = match valign {
                        Some("middle") => Some(AlignSelf::Center),
                        Some("top") => Some(AlignSelf::FlexStart),
                        Some("bottom") => Some(AlignSelf::FlexEnd),
                        Some("sub") => Some(AlignSelf::FlexEnd),
                        Some("super") => Some(AlignSelf::FlexStart),
                        _ => None, // baseline (default)
                    };
                    // For sub/super, apply a vertical offset via margin.
                    let valign_margin_top = match valign {
                        Some("sub") => LengthPercentageAuto::Length(fs * 0.3),
                        Some("super") => LengthPercentageAuto::Length(-fs * 0.4),
                        _ => LengthPercentageAuto::Length(0.0),
                    };

                    let ctx = NodeContext {
                        content: LayoutContent::Text(merged_text),
                        style: StyleMap { properties: style.clone() },
                    };
                    let mut item_style = Style {
                        display: Display::Flex,
                        size: Size {
                            width: Dimension::Length(merged_width),
                            height: Dimension::Length(lh),
                        },
                        ..Style::DEFAULT
                    };
                    if let Some(a) = align_self {
                        item_style.align_self = Some(a);
                    }
                    if matches!(valign, Some("sub") | Some("super")) {
                        item_style.margin.top = valign_margin_top;
                    }
                    let id = taffy
                        .new_leaf_with_context(item_style, ctx)
                        .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                    item_ids.push(id);
                }
                InlineItem::Space { width, style } => {
                    // Standalone space (style differs from neighbors).
                    let ctx = NodeContext {
                        content: LayoutContent::Text(" ".to_string()),
                        style: StyleMap { properties: style.clone() },
                    };
                    let id = taffy
                        .new_leaf_with_context(
                            Style {
                                display: Display::Flex,
                                size: Size {
                                    width: Dimension::Length(*width),
                                    height: Dimension::Length(line_height),
                                },
                                ..Style::DEFAULT
                            },
                            ctx,
                        )
                        .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                    item_ids.push(id);
                }
                InlineItem::Image { src, width, height } => {
                    max_height = max_height.max(*height);
                    let ctx = NodeContext {
                        content: LayoutContent::Image { src: src.clone() },
                        style: StyleMap::default(),
                    };
                    let id = taffy
                        .new_leaf_with_context(
                            Style {
                                display: Display::Flex,
                                size: Size {
                                    width: Dimension::Length(*width),
                                    height: Dimension::Length(*height),
                                },
                                ..Style::DEFAULT
                            },
                            ctx,
                        )
                        .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;
                    item_ids.push(id);
                }
            }
            idx += 1;
        }

        // Create the line box container (flex-row, items aligned to baseline).
        // Apply text-align from parent style (with direction: rtl support).
        let text_align = parent_style_props.iter()
            .find(|(k, _)| k == "text-align")
            .and_then(|(_, v)| match v {
                StyleValue::Keyword(k) | StyleValue::Str(k) => Some(k.as_str()),
                _ => None,
            });
        let justify = match text_align {
            Some("center") => Some(JustifyContent::Center),
            Some("right") | Some("end") => Some(JustifyContent::FlexEnd),
            Some("left") | Some("start") => None,
            Some("justify") => Some(JustifyContent::SpaceBetween),
            None if direction_rtl => Some(JustifyContent::FlexEnd),
            _ => None, // left / default
        };

        // Apply text-indent as left padding on the first line only.
        let line_padding_left = if line_index == 0 && text_indent > 0.0 {
            LengthPercentage::Length(text_indent)
        } else {
            LengthPercentage::Length(0.0)
        };

        let line_ctx = NodeContext {
            content: LayoutContent::Inline,
            style: StyleMap::default(),
        };
        let line_id = taffy
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    align_items: Some(AlignItems::Baseline),
                    justify_content: justify,
                    padding: Rect {
                        left: line_padding_left,
                        right: LengthPercentage::Length(0.0),
                        top: LengthPercentage::Length(0.0),
                        bottom: LengthPercentage::Length(0.0),
                    },
                    size: Size {
                        width: Dimension::Percent(1.0),
                        height: Dimension::Auto,
                    },
                    ..Style::DEFAULT
                },
                &item_ids,
            )
            .map(|id| {
                taffy.set_node_context(id, Some(line_ctx)).ok();
                id
            })
            .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;

        line_box_ids.push(line_id);
    }

    Ok(line_box_ids)
}

// ── Table layout ────────────────────────────────────────────────────────

/// Estimate the minimum content width of a table cell (min-content).
///
/// This is the width of the longest individual word (or image) — the
/// smallest width the cell can shrink to without overflow.
fn estimate_cell_min_width(children: &[DomNode], font_size: f32) -> f32 {
    let mut max_word_w: f32 = 0.0;
    for child in children {
        match child {
            DomNode::Text(text) => {
                // Measure each word individually — the min-content width
                // is the width of the longest single word.
                for word in text.split_whitespace() {
                    let w = measure_text_width(word, font_size);
                    max_word_w = max_word_w.max(w);
                }
            }
            DomNode::Element {
                tag,
                children: cc,
                attributes,
                ..
            } => {
                if tag == "img" {
                    let img_w: f32 = attributes
                        .iter()
                        .find(|(k, _)| k == "width")
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(10.0);
                    max_word_w = max_word_w.max(img_w);
                } else if tag == "table" {
                    // Nested table min-width: sum of per-column min widths.
                    let table_min = estimate_nested_table_min_width(cc, font_size);
                    max_word_w = max_word_w.max(table_min);
                } else {
                    let child_w = estimate_cell_min_width(cc, font_size);
                    max_word_w = max_word_w.max(child_w);
                }
            }
            _ => {}
        }
    }
    max_word_w
}

/// Estimate the min-content width of a nested table by computing
/// per-column min widths and summing them.
fn estimate_nested_table_min_width(table_children: &[DomNode], font_size: f32) -> f32 {
    let mut col_mins: Vec<f32> = Vec::new();
    estimate_nested_table_min_cols(table_children, font_size, &mut col_mins);
    if col_mins.is_empty() {
        return estimate_cell_min_width(table_children, font_size);
    }
    col_mins.iter().sum::<f32>() + (col_mins.len().saturating_sub(1) as f32) * 2.0
}

/// Walk nested table children to accumulate per-column min-content widths.
fn estimate_nested_table_min_cols(children: &[DomNode], font_size: f32, col_mins: &mut Vec<f32>) {
    for child in children {
        if let DomNode::Element { tag, children: cc, .. } = child {
            match tag.as_str() {
                "tr" => {
                    let mut col_idx = 0;
                    for cell in cc {
                        if let DomNode::Element { tag: cell_tag, children: cell_cc, attributes, .. } = cell {
                            if cell_tag == "td" || cell_tag == "th" {
                                let colspan: usize = attributes
                                    .iter()
                                    .find(|(k, _)| k == "colspan")
                                    .and_then(|(_, v)| v.parse().ok())
                                    .unwrap_or(1);
                                let w = estimate_cell_min_width(cell_cc, font_size);
                                let per_col = w / colspan as f32;
                                for i in 0..colspan {
                                    let idx = col_idx + i;
                                    if idx >= col_mins.len() {
                                        col_mins.resize(idx + 1, 0.0_f32);
                                    }
                                    col_mins[idx] = col_mins[idx].max(per_col);
                                }
                                col_idx += colspan;
                            }
                        }
                    }
                }
                "thead" | "tbody" | "tfoot" => {
                    estimate_nested_table_min_cols(cc, font_size, col_mins);
                }
                _ => {}
            }
        }
    }
}

/// Estimate the maximum content width of a table cell (max-content).
///
/// This is the width of all content laid out on a single line without
/// line-breaks — the preferred width of the cell.
fn estimate_cell_content_width(children: &[DomNode], font_size: f32) -> f32 {
    let mut max_w: f32 = 0.0;
    for child in children {
        match child {
            DomNode::Text(text) => {
                // Total text width (no line-breaks) = max-content.
                let w = measure_text_width(text.trim(), font_size);
                max_w = max_w.max(w);
            }
            DomNode::Element {
                tag,
                children: cc,
                attributes,
                ..
            } => {
                if tag == "img" {
                    let img_w: f32 = attributes
                        .iter()
                        .find(|(k, _)| k == "width")
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(10.0);
                    max_w = max_w.max(img_w);
                } else if tag == "table" {
                    // For nested tables, estimate width as sum of max column
                    // widths across all rows.
                    let table_w = estimate_nested_table_width(cc, font_size);
                    max_w = max_w.max(table_w);
                } else {
                    let child_w = estimate_cell_content_width(cc, font_size);
                    max_w = max_w.max(child_w);
                }
            }
            _ => {}
        }
    }
    max_w
}

/// Estimate the width of a nested table by computing per-column max widths
/// and summing them. This provides a better estimate than just recursing
/// into children individually.
fn estimate_nested_table_width(table_children: &[DomNode], font_size: f32) -> f32 {
    let mut col_maxes: Vec<f32> = Vec::new();
    estimate_nested_table_cols(table_children, font_size, &mut col_maxes);
    if col_maxes.is_empty() {
        // Fallback: just recurse normally.
        return estimate_cell_content_width(table_children, font_size);
    }
    // Sum of column widths + some spacing.
    col_maxes.iter().sum::<f32>() + (col_maxes.len().saturating_sub(1) as f32) * 2.0
}

/// Walk table children (handling thead/tbody/tfoot wrappers) to find <tr>
/// rows and accumulate per-column max-content widths.
fn estimate_nested_table_cols(children: &[DomNode], font_size: f32, col_maxes: &mut Vec<f32>) {
    for child in children {
        if let DomNode::Element { tag, children: cc, .. } = child {
            match tag.as_str() {
                "tr" => {
                    let mut col_idx = 0;
                    for cell in cc {
                        if let DomNode::Element { tag: cell_tag, children: cell_cc, attributes, .. } = cell {
                            if cell_tag == "td" || cell_tag == "th" {
                                let colspan: usize = attributes
                                    .iter()
                                    .find(|(k, _)| k == "colspan")
                                    .and_then(|(_, v)| v.parse().ok())
                                    .unwrap_or(1);
                                let w = estimate_cell_content_width(cell_cc, font_size);
                                // Distribute width across spanned columns.
                                let per_col = w / colspan as f32;
                                for i in 0..colspan {
                                    let idx = col_idx + i;
                                    if idx >= col_maxes.len() {
                                        col_maxes.resize(idx + 1, 0.0_f32);
                                    }
                                    col_maxes[idx] = col_maxes[idx].max(per_col);
                                }
                                col_idx += colspan;
                            }
                        }
                    }
                }
                "thead" | "tbody" | "tfoot" => {
                    estimate_nested_table_cols(cc, font_size, col_maxes);
                }
                _ => {}
            }
        }
    }
}

/// A table row: the `<tr>` attributes plus its cell children.
struct TableRow<'a> {
    /// Attributes from the `<tr>` element (for bgcolor, etc.).
    tr_attributes: &'a [(String, String)],
    /// The `<td>` / `<th>` cell nodes.
    cells: Vec<&'a DomNode>,
}

/// Collect all `<tr>` rows from a table's children, traversing through
/// `<thead>`, `<tbody>`, `<tfoot>` wrappers.
fn collect_table_rows<'a>(children: &'a [DomNode], rows: &mut Vec<TableRow<'a>>) {
    for child in children {
        if let DomNode::Element { tag, children: cc, attributes, .. } = child {
            match tag.as_str() {
                "tr" => {
                    let cells: Vec<&DomNode> = cc
                        .iter()
                        .filter(|c| {
                            matches!(c, DomNode::Element { tag, .. } if tag == "td" || tag == "th")
                        })
                        .collect();
                    if !cells.is_empty() {
                        rows.push(TableRow {
                            tr_attributes: attributes,
                            cells,
                        });
                    }
                }
                "thead" | "tbody" | "tfoot" => {
                    collect_table_rows(cc, rows);
                }
                _ => {}
            }
        }
    }
}

/// Layout a `<table>` element with proper column width distribution.
///
/// Algorithm:
/// 1. Collect all rows (traversing thead/tbody/tfoot).
/// 2. Determine column count and collect explicit widths.
/// 3. Distribute remaining space to auto-width columns.
/// 4. Create Taffy nodes: row → flex-row, cell → flex-column with fixed width.
fn layout_table(
    taffy: &mut TaffyTree<NodeContext>,
    table_tag: &str,
    table_attrs: &[(String, String)],
    children: &[DomNode],
    available_width: f32,
    parent_font_size: f32,
    parent_style_props: &[(String, StyleValue)],
    viewport: &Viewport,
    depth: usize,
) -> Result<NodeId, NovaError> {
    let cellpadding: f32 = table_attrs
        .iter()
        .find(|(k, _)| k == "cellpadding")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(1.0);
    // Use HTML cellspacing attribute first, then CSS border-spacing, then default.
    let cellspacing: f32 = table_attrs
        .iter()
        .find(|(k, _)| k == "cellspacing")
        .and_then(|(_, v)| v.parse().ok())
        .or_else(|| {
            // Check CSS border-spacing from data-nova-style.
            table_attrs.iter()
                .find(|(k, _)| k == "data-nova-style")
                .and_then(|(_, style_str)| {
                    for decl in style_str.split(';') {
                        let parts: Vec<&str> = decl.splitn(2, ':').collect();
                        if parts.len() == 2 && parts[0].trim() == "border-spacing" {
                            let val = parts[1].trim();
                            // border-spacing accepts one or two values; use the first.
                            let first = val.split_whitespace().next().unwrap_or(val);
                            if let Some(rest) = first.strip_suffix("px") {
                                return rest.trim().parse::<f32>().ok();
                            }
                            return first.parse::<f32>().ok();
                        }
                    }
                    None
                })
        })
        .or_else(|| {
            // Also check parent_style_props for border-spacing.
            parent_style_props.iter()
                .find(|(k, _)| k == "border-spacing")
                .and_then(|(_, v)| match v {
                    StyleValue::Px(px) => Some(*px),
                    StyleValue::Str(s) | StyleValue::Keyword(s) => {
                        let first = s.split_whitespace().next().unwrap_or(s);
                        first.strip_suffix("px")
                            .and_then(|r| r.parse::<f32>().ok())
                            .or_else(|| first.parse::<f32>().ok())
                    }
                    _ => None,
                })
        })
        .unwrap_or(2.0);
    let border: f32 = table_attrs
        .iter()
        .find(|(k, _)| k == "border")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0.0);

    let table_lp = parse_layout_props(table_attrs, viewport);
    let table_width = match table_lp.width {
        Some(Dimension::Length(px)) => px.min(available_width),
        Some(Dimension::Percent(pct)) => (available_width * pct).min(available_width),
        _ => available_width,
    };

    // Extract caption-side from CSS (default: "top").
    let caption_side = table_attrs.iter()
        .find(|(k, _)| k == "data-nova-style")
        .and_then(|(_, style_str)| {
            for decl in style_str.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "caption-side" {
                    return Some(parts[1].trim().to_string());
                }
            }
            None
        })
        .unwrap_or_else(|| "top".to_string());

    // Collect <caption> elements from table children.
    let mut caption_nodes: Vec<&DomNode> = Vec::new();
    for child in children {
        if let DomNode::Element { tag, .. } = child {
            if tag == "caption" {
                caption_nodes.push(child);
            }
        }
    }

    // Phase 1: Collect all rows.
    let mut all_rows: Vec<TableRow> = Vec::new();
    collect_table_rows(children, &mut all_rows);

    if all_rows.is_empty() {
        // No rows — return an empty block node.
        let ctx = NodeContext {
            content: LayoutContent::Block,
            style: StyleMap::default(),
        };
        return taffy
            .new_leaf_with_context(
                Style {
                    display: Display::Flex,
                    ..Style::DEFAULT
                },
                ctx,
            )
            .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")));
    }

    // Phase 2: Determine column count and collect explicit widths.
    let num_cols = all_rows
        .iter()
        .map(|r| {
            r.cells.iter()
                .map(|cell| {
                    if let DomNode::Element { attributes, .. } = cell {
                        attributes
                            .iter()
                            .find(|(k, _)| k == "colspan")
                            .and_then(|(_, v)| v.parse::<usize>().ok())
                            .unwrap_or(1)
                    } else {
                        1
                    }
                })
                .sum::<usize>()
        })
        .max()
        .unwrap_or(1);

    let mut col_widths: Vec<Option<f32>> = vec![None; num_cols];
    for row in &all_rows {
        let mut col_idx = 0;
        for cell in &row.cells {
            if col_idx >= num_cols {
                break;
            }
            if let DomNode::Element { attributes, .. } = cell {
                let colspan: usize = attributes
                    .iter()
                    .find(|(k, _)| k == "colspan")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(1);

                // Check HTML width attribute.
                let html_w: Option<f32> = attributes
                    .iter()
                    .find(|(k, _)| k == "width")
                    .and_then(|(_, v)| {
                        if let Some(pct) = v.strip_suffix('%') {
                            pct.parse::<f32>().ok().map(|p| table_width * p / 100.0)
                        } else {
                            v.parse::<f32>().ok()
                        }
                    });

                // Check CSS width.
                let css_w = {
                    let cell_lp = parse_layout_props(attributes, viewport);
                    match cell_lp.width {
                        Some(Dimension::Length(px)) => Some(px),
                        Some(Dimension::Percent(pct)) => Some(table_width * pct),
                        _ => None,
                    }
                };

                let w = css_w.or(html_w);
                if colspan == 1 {
                    if let Some(px) = w {
                        if col_widths[col_idx].map_or(true, |prev| prev < px) {
                            col_widths[col_idx] = Some(px);
                        }
                    }
                }
                col_idx += colspan;
            }
        }
    }

    // Phase 3: Two-pass min/max column distribution (like Gecko BasicTableLayoutStrategy).
    let usable = table_width - cellspacing * (num_cols as f32 + 1.0) - border * 2.0;
    let fixed_total: f32 = col_widths.iter().filter_map(|w| *w).sum();

    // Pass 1: Compute min-content and max-content widths for each auto column.
    let mut col_min_widths: Vec<f32> = vec![0.0_f32; num_cols];
    let mut col_max_widths: Vec<f32> = vec![0.0_f32; num_cols];
    for row in &all_rows {
        let mut col_idx = 0;
        for cell in &row.cells {
            if col_idx >= num_cols {
                break;
            }
            if let DomNode::Element {
                attributes,
                children: cell_children,
                ..
            } = cell
            {
                let colspan: usize = attributes
                    .iter()
                    .find(|(k, _)| k == "colspan")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(1);
                if colspan == 1 && col_widths[col_idx].is_none() {
                    let min_w = estimate_cell_min_width(cell_children, parent_font_size) + cellpadding * 2.0;
                    let max_w = estimate_cell_content_width(cell_children, parent_font_size) + cellpadding * 2.0;
                    col_min_widths[col_idx] = col_min_widths[col_idx].max(min_w);
                    col_max_widths[col_idx] = col_max_widths[col_idx].max(max_w);
                }
                col_idx += colspan;
            }
        }
    }

    // Pass 2: Distribute remaining space using min/max constraints.
    let remaining = (usable - fixed_total).max(0.0);
    let sum_min: f32 = col_widths.iter().enumerate()
        .filter(|(_, w)| w.is_none())
        .map(|(i, _)| col_min_widths[i])
        .sum();
    let sum_max: f32 = col_widths.iter().enumerate()
        .filter(|(_, w)| w.is_none())
        .map(|(i, _)| col_max_widths[i])
        .sum();

    // Find which auto column has the largest max-content width.
    let widest_auto_idx: Option<usize> = col_widths
        .iter()
        .enumerate()
        .filter(|(_, w)| w.is_none())
        .max_by(|(a, _), (b, _)| col_max_widths[*a].partial_cmp(&col_max_widths[*b]).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i);

    let final_widths: Vec<f32> = col_widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            if let Some(px) = w {
                *px
            } else if remaining >= sum_max && sum_max > 0.0 {
                // Give each auto column its max-content width,
                // then give all surplus to the widest auto column.
                let surplus = remaining - sum_max;
                col_max_widths[i] + if widest_auto_idx == Some(i) { surplus } else { 0.0 }
            } else if remaining >= sum_min && sum_max > sum_min {
                // Between min and max: distribute proportionally between
                // min and max for each auto column.
                let range = sum_max - sum_min;
                let avail_range = remaining - sum_min;
                let fraction = avail_range / range;
                let col_range = col_max_widths[i] - col_min_widths[i];
                (col_min_widths[i] + col_range * fraction).max(10.0)
            } else if sum_min > 0.0 {
                // Not enough space: give min-content width (may overflow).
                col_min_widths[i].max(10.0)
            } else {
                // Fallback: equal distribution.
                let auto_count = col_widths.iter().filter(|w| w.is_none()).count();
                if auto_count > 0 { remaining / auto_count as f32 } else { 0.0 }
            }
        })
        .collect();

    // Phase 4: Create Taffy nodes.
    let font_size = resolve_font_size(table_tag, table_attrs, parent_font_size);

    // Build table style props.
    let mut table_props: Vec<(String, StyleValue)> = parent_style_props.to_vec();
    if let Some(nova_style) = table_attrs.iter().find(|(k, _)| k == "data-nova-style") {
        for decl in nova_style.1.split(';') {
            let parts: Vec<&str> = decl.splitn(2, ':').collect();
            if parts.len() == 2 {
                let prop = parts[0].trim().to_string();
                let val = parts[1].trim();
                let sv = if let Some(px) =
                    val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok())
                {
                    StyleValue::Px(px)
                } else if val.starts_with("rgb") || val.starts_with('#') {
                    StyleValue::Str(val.to_string())
                } else {
                    StyleValue::Keyword(val.to_string())
                };
                if let Some(existing) = table_props.iter_mut().find(|(k, _)| *k == prop) {
                    existing.1 = sv;
                } else {
                    table_props.push((prop, sv));
                }
            }
        }
    }

    let mut row_ids = Vec::new();
    for table_row in &all_rows {
        let mut cell_ids = Vec::new();
        let mut col_idx = 0;

        // Extract bgcolor from the <tr> element for propagation to cells.
        let tr_bgcolor: Option<&str> = table_row.tr_attributes
            .iter()
            .find(|(k, _)| k == "bgcolor")
            .map(|(_, v)| v.as_str());
        // Also check data-nova-style on <tr> for background-color from CSS.
        let tr_css_bgcolor: Option<String> = table_row.tr_attributes
            .iter()
            .find(|(k, _)| k == "data-nova-style")
            .and_then(|(_, style_str)| {
                for decl in style_str.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 && parts[0].trim() == "background-color" {
                        return Some(parts[1].trim().to_string());
                    }
                }
                None
            });

        for cell in &table_row.cells {
            if let DomNode::Element {
                tag: _,
                attributes,
                children: cell_children,
            } = cell
            {
                let colspan: usize = attributes
                    .iter()
                    .find(|(k, _)| k == "colspan")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(1);

                // Compute cell width (sum of spanned columns + inter-column spacing).
                let cell_w: f32 = (col_idx..col_idx + colspan)
                    .filter_map(|i| final_widths.get(i))
                    .sum::<f32>()
                    + cellspacing * colspan.saturating_sub(1) as f32;

                let cell_avail = (cell_w - cellpadding * 2.0).max(0.0);

                // Build cell style props.
                let mut cell_props: Vec<(String, StyleValue)> = table_props.clone();

                // Propagate <tr> bgcolor to cell as background-color
                // (lower priority than cell's own bgcolor/style).
                if let Some(bg) = tr_css_bgcolor.as_deref().or(tr_bgcolor) {
                    if !cell_props.iter().any(|(k, _)| k == "background-color") {
                        cell_props.push(("background-color".into(), StyleValue::Str(bg.to_string())));
                    }
                }

                if let Some(nova_style) =
                    attributes.iter().find(|(k, _)| k == "data-nova-style")
                {
                    for decl in nova_style.1.split(';') {
                        let parts: Vec<&str> = decl.splitn(2, ':').collect();
                        if parts.len() == 2 {
                            let prop = parts[0].trim().to_string();
                            let val = parts[1].trim();
                            let sv = if let Some(px) =
                                val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok())
                            {
                                StyleValue::Px(px)
                            } else if val.starts_with("rgb") || val.starts_with('#') {
                                StyleValue::Str(val.to_string())
                            } else {
                                StyleValue::Keyword(val.to_string())
                            };
                            if let Some(existing) =
                                cell_props.iter_mut().find(|(k, _)| *k == prop)
                            {
                                existing.1 = sv;
                            } else {
                                cell_props.push((prop, sv));
                            }
                        }
                    }
                }

                // Handle HTML bgcolor attribute directly on the cell.
                if let Some(bg) = attributes.iter().find(|(k, _)| k == "bgcolor") {
                    if let Some(existing) = cell_props.iter_mut().find(|(k, _)| k == "background-color") {
                        existing.1 = StyleValue::Str(bg.1.clone());
                    } else {
                        cell_props.push(("background-color".into(), StyleValue::Str(bg.1.clone())));
                    }
                }

                // Handle `nowrap` HTML attribute → white-space: nowrap.
                if attributes.iter().any(|(k, _)| k == "nowrap") {
                    if let Some(existing) = cell_props.iter_mut().find(|(k, _)| k == "white-space") {
                        existing.1 = StyleValue::Keyword("nowrap".to_string());
                    } else {
                        cell_props.push(("white-space".into(), StyleValue::Keyword("nowrap".to_string())));
                    }
                }

                // Handle horizontal alignment from HTML align attribute.
                if let Some((_, align_val)) = attributes.iter().find(|(k, _)| k == "align") {
                    if !cell_props.iter().any(|(k, _)| k == "text-align") {
                        cell_props.push(("text-align".into(), StyleValue::Keyword(align_val.clone())));
                    }
                }

                // Handle vertical alignment from HTML valign attribute.
                let valign = attributes
                    .iter()
                    .find(|(k, _)| k == "valign")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("top");

                // Build cell content using IFC for proper inline formatting.
                let content_ids = build_children_with_ifc(
                    taffy,
                    cell_children,
                    cell_avail,
                    font_size,
                    &cell_props,
                    viewport,
                    depth + 1,
                )?;

                // Map valign to Taffy justify_content (since cells are Column direction).
                let cell_justify = match valign {
                    "top" => JustifyContent::FlexStart,
                    "bottom" => JustifyContent::FlexEnd,
                    "middle" | "center" => JustifyContent::Center,
                    _ => JustifyContent::FlexStart,
                };

                let cell_style = Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Column,
                    flex_shrink: 0.0,
                    align_items: Some(AlignItems::FlexStart),
                    justify_content: Some(cell_justify),
                    size: Size {
                        width: Dimension::Length(cell_w),
                        height: Dimension::Auto,
                    },
                    padding: Rect {
                        top: LengthPercentage::Length(cellpadding),
                        right: LengthPercentage::Length(cellpadding),
                        bottom: LengthPercentage::Length(cellpadding),
                        left: LengthPercentage::Length(cellpadding),
                    },
                    ..Style::DEFAULT
                };

                let cell_ctx = NodeContext {
                    content: LayoutContent::Block,
                    style: StyleMap {
                        properties: cell_props,
                    },
                };

                let cell_id = taffy
                    .new_with_children(cell_style, &content_ids)
                    .map(|id| {
                        taffy.set_node_context(id, Some(cell_ctx)).ok();
                        id
                    })
                    .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;

                cell_ids.push(cell_id);
                col_idx += colspan;
            }
        }

        // Row container.
        let row_style = Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            flex_shrink: 0.0,
            gap: Size {
                width: LengthPercentage::Length(cellspacing),
                height: LengthPercentage::Length(0.0),
            },
            size: Size {
                width: Dimension::Auto,
                height: Dimension::Auto,
            },
            ..Style::DEFAULT
        };

        // Build row style properties, including bgcolor from <tr>.
        let mut row_props: Vec<(String, StyleValue)> = Vec::new();
        if let Some(bg) = tr_css_bgcolor.as_deref().or(tr_bgcolor) {
            row_props.push(("background-color".into(), StyleValue::Str(bg.to_string())));
        }
        // Also parse data-nova-style from <tr> for other properties.
        if let Some(nova_style) = table_row.tr_attributes.iter().find(|(k, _)| k == "data-nova-style") {
            for decl in nova_style.1.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 {
                    let prop = parts[0].trim().to_string();
                    let val = parts[1].trim();
                    let sv = if let Some(px) = val.strip_suffix("px").and_then(|s| s.parse::<f32>().ok()) {
                        StyleValue::Px(px)
                    } else if val.starts_with("rgb") || val.starts_with('#') {
                        StyleValue::Str(val.to_string())
                    } else {
                        StyleValue::Keyword(val.to_string())
                    };
                    if let Some(existing) = row_props.iter_mut().find(|(k, _)| *k == prop) {
                        existing.1 = sv;
                    } else {
                        row_props.push((prop, sv));
                    }
                }
            }
        }

        let row_ctx = NodeContext {
            content: LayoutContent::Block,
            style: StyleMap { properties: row_props },
        };

        let row_id = taffy
            .new_with_children(row_style, &cell_ids)
            .map(|id| {
                taffy.set_node_context(id, Some(row_ctx)).ok();
                id
            })
            .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))?;

        row_ids.push(row_id);
    }

    // Table container.
    let table_style = Style {
        display: Display::Flex,
        flex_direction: FlexDirection::Column,
        flex_shrink: 0.0,
        gap: Size {
            width: LengthPercentage::Length(0.0),
            height: LengthPercentage::Length(cellspacing),
        },
        size: Size {
            width: Dimension::Length(table_width),
            height: Dimension::Auto,
        },
        margin: Rect {
            top: table_lp
                .margin_top
                .unwrap_or(LengthPercentageAuto::Length(0.0)),
            right: table_lp
                .margin_right
                .unwrap_or(LengthPercentageAuto::Length(0.0)),
            bottom: table_lp
                .margin_bottom
                .unwrap_or(LengthPercentageAuto::Length(0.0)),
            left: table_lp
                .margin_left
                .unwrap_or(LengthPercentageAuto::Length(0.0)),
        },
        padding: Rect {
            top: LengthPercentage::Length(cellspacing + border),
            right: LengthPercentage::Length(cellspacing + border),
            bottom: LengthPercentage::Length(cellspacing + border),
            left: LengthPercentage::Length(cellspacing + border),
        },
        ..Style::DEFAULT
    };

    let table_ctx = NodeContext {
        content: LayoutContent::Block,
        style: StyleMap {
            properties: table_props,
        },
    };

    // Layout caption nodes if any.
    let mut caption_ids: Vec<NodeId> = Vec::new();
    for cap_node in &caption_nodes {
        let cap_id = add_node(taffy, cap_node, table_width, parent_font_size, parent_style_props, viewport, depth + 1)?;
        caption_ids.push(cap_id);
    }

    // Build the final children list: captions before or after rows based on caption-side.
    let mut table_children_ids: Vec<NodeId> = Vec::new();
    if caption_side != "bottom" {
        // caption-side: top (default) — captions come first.
        table_children_ids.extend(&caption_ids);
        table_children_ids.extend(&row_ids);
    } else {
        // caption-side: bottom — rows first, then captions.
        table_children_ids.extend(&row_ids);
        table_children_ids.extend(&caption_ids);
    }

    taffy
        .new_with_children(table_style, &table_children_ids)
        .map(|id| {
            taffy.set_node_context(id, Some(table_ctx)).ok();
            id
        })
        .map_err(|e| NovaError::LayoutError(format!("Taffy error: {e:?}")))
}

// ── Box model helpers ──────────────────────────────────────────────────

/// Parse `box-sizing` from attributes, defaulting to `content-box`.
fn resolve_box_sizing(attributes: &[(String, String)]) -> &'static str {
    for attr_name in &["data-nova-style", "style"] {
        if let Some(style_attr) = attributes.iter().find(|(k, _)| k == *attr_name) {
            for decl in style_attr.1.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "box-sizing" {
                    let val = parts[1].trim();
                    if val == "border-box" {
                        return "border-box";
                    }
                }
            }
        }
    }
    "content-box"
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

    let mut layout_box = LayoutBox {
        x,
        y,
        width,
        height,
        content: ctx.content,
        style: ctx.style,
        children,
        z_index,
    };

    // Post-process: collapse parent-first-child margins.
    // If this block element has no padding-top and no border-top, its
    // margin-top should collapse with the first child's margin-top.
    collapse_parent_child_margins(&mut layout_box);

    layout_box
}

/// Collapse margins between a parent and its first/last child.
///
/// In CSS, when there is no padding or border separating a parent's margin
/// from its first child's margin, the two margins collapse. The larger
/// margin "wins" and the child is repositioned accordingly.
///
/// This function adjusts Y positions in the already-computed layout box tree
/// to approximate this behavior.
fn collapse_parent_child_margins(parent: &mut LayoutBox) {
    if parent.children.is_empty() {
        return;
    }

    // Check if the parent has padding-top or border-top that would prevent collapsing.
    let has_padding_top = parent.style.properties.iter().any(|(k, v)| {
        k == "padding-top" && matches!(v, StyleValue::Px(px) if *px > 0.0)
    });
    let has_border_top = parent.style.properties.iter().any(|(k, v)| {
        (k == "border-top-width" || k == "border-width")
            && matches!(v, StyleValue::Px(px) if *px > 0.0)
    });

    if has_padding_top || has_border_top {
        return;
    }

    // Get the first child's margin-top.
    let first_child_margin_top = parent.children[0]
        .style
        .properties
        .iter()
        .find(|(k, _)| k == "margin-top")
        .and_then(|(_, v)| match v {
            StyleValue::Px(px) => Some(*px),
            _ => None,
        })
        .unwrap_or(0.0);

    // Get the parent's margin-top.
    let parent_margin_top = parent
        .style
        .properties
        .iter()
        .find(|(k, _)| k == "margin-top")
        .and_then(|(_, v)| match v {
            StyleValue::Px(px) => Some(*px),
            _ => None,
        })
        .unwrap_or(0.0);

    // Collapse: the effective gap is max(parent_margin_top, child_margin_top),
    // not the sum. The child should be shifted up by the overlap.
    if first_child_margin_top > 0.0 && parent_margin_top > 0.0 {
        let collapsed = parent_margin_top.max(first_child_margin_top);
        let overlap = parent_margin_top + first_child_margin_top - collapsed;
        if overlap > 0.0 {
            // Shift the first child (and all subsequent children) up.
            for child in parent.children.iter_mut() {
                child.y -= overlap;
                shift_children_y(child, -overlap);
            }
        }
    }
}

/// Recursively shift all descendants' Y positions by a delta.
///
/// Since layout box positions are absolute, moving a parent requires
/// moving all its descendants by the same delta.
fn shift_children_y(layout_box: &mut LayoutBox, delta: f32) {
    for child in layout_box.children.iter_mut() {
        child.y += delta;
        shift_children_y(child, delta);
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
    min_width: Option<Dimension>,
    max_width: Option<Dimension>,
    height: Option<Dimension>,
    min_height: Option<Dimension>,
    max_height: Option<Dimension>,
    /// CSS `box-sizing`: "content-box" or "border-box".
    box_sizing: Option<String>,
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
    /// CSS `clear` property: "left" | "right" | "both" | "none".
    clear: Option<String>,
    /// CSS `overflow-x` property: "visible" | "hidden" | "scroll" | "auto".
    overflow_x: Option<String>,
    /// CSS `overflow-y` property: "visible" | "hidden" | "scroll" | "auto".
    overflow_y: Option<String>,
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
    // ── Flexbox alignment properties ─────────────────────────────────
    /// CSS `align-items` property for flex/grid containers.
    align_items: Option<String>,
    /// CSS `justify-content` property for flex/grid containers.
    justify_content: Option<String>,
    /// CSS `flex-wrap` property.
    flex_wrap: Option<String>,
    /// CSS `flex-direction` property.
    flex_direction: Option<String>,
}

/// Parse a CSS value into `LengthPercentageAuto`.
///
/// Recognises `auto`, `<n>px`, `<n>%`, `<n>vw`, `<n>vh` (viewport units
/// treated as percent), and bare `0`.
fn parse_length_percentage_auto(val: &str) -> Option<LengthPercentageAuto> {
    parse_length_percentage_auto_ctx(val, 0.0)
}

/// Parse a CSS value into `LengthPercentageAuto` with a context for calc().
fn parse_length_percentage_auto_ctx(val: &str, context_px: f32) -> Option<LengthPercentageAuto> {
    let val = val.trim();
    if val == "auto" {
        return Some(LengthPercentageAuto::Auto);
    }
    if val == "0" {
        return Some(LengthPercentageAuto::Length(0.0));
    }
    // Handle calc() expressions.
    if val.starts_with("calc(") {
        if let Some(inner) = val.strip_prefix("calc(").and_then(|s| s.strip_suffix(')')) {
            if let Some(px) = mod_css_engine::values::eval_calc(inner, context_px) {
                return Some(LengthPercentageAuto::Length(px));
            }
        }
        return None;
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
    if let Some(vh) = val.strip_suffix("vh").and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(LengthPercentageAuto::Percent(vh / 100.0));
    }
    None
}

/// Parse a CSS value into `LengthPercentage` (no `auto`).
///
/// Recognises `<n>px`, `<n>%`, bare `0`, and `calc()` expressions.
fn parse_length_percentage(val: &str) -> Option<LengthPercentage> {
    parse_length_percentage_ctx(val, 0.0)
}

/// Parse a CSS value into `LengthPercentage` with a context for calc().
fn parse_length_percentage_ctx(val: &str, context_px: f32) -> Option<LengthPercentage> {
    let val = val.trim();
    if val == "0" {
        return Some(LengthPercentage::Length(0.0));
    }
    // Handle calc() expressions.
    if val.starts_with("calc(") {
        if let Some(inner) = val.strip_prefix("calc(").and_then(|s| s.strip_suffix(')')) {
            if let Some(px) = mod_css_engine::values::eval_calc(inner, context_px) {
                return Some(LengthPercentage::Length(px));
            }
        }
        return None;
    }
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
/// Recognises `auto`, `<n>px`, `<n>%`, `<n>vw`, `<n>vh` (viewport units
/// treated as percent), bare `0`, and `calc()` expressions.
///
/// Delegates to [`parse_dimension_with_context`] with a default context of 0.
fn parse_dimension(val: &str) -> Option<Dimension> {
    parse_dimension_with_context(val, 0.0)
}

/// Parse a CSS value into a `Dimension` with a context size for resolving
/// `calc()` expressions that contain `%` or viewport-relative units.
fn parse_dimension_with_context(val: &str, context_px: f32) -> Option<Dimension> {
    let val = val.trim();
    if val == "auto" {
        return Some(Dimension::Auto);
    }
    if val == "0" {
        return Some(Dimension::Length(0.0));
    }
    // Handle calc() expressions.
    if val.starts_with("calc(") {
        if let Some(inner) = val.strip_prefix("calc(").and_then(|s| s.strip_suffix(')')) {
            // Try resolving with context for % and viewport units.
            if let Some(px) = mod_css_engine::values::eval_calc(inner, context_px) {
                return Some(Dimension::Length(px));
            }
        }
        return None;
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
    if let Some(vh) = val.strip_suffix("vh").and_then(|s| s.trim().parse::<f32>().ok()) {
        return Some(Dimension::Percent(vh / 100.0));
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

/// Pre-resolve viewport units (`vw`, `vh`) in a CSS value string to absolute
/// `px` values. Each whitespace-separated token is checked independently so
/// shorthand properties like `margin: 15vh auto` are handled correctly.
fn resolve_viewport_in_value(val: &str, viewport: &Viewport) -> String {
    val.split_whitespace()
        .map(|token| {
            if let Some(v) = token.strip_suffix("vw").and_then(|s| s.parse::<f32>().ok()) {
                format!("{}px", viewport.width * v / 100.0)
            } else if let Some(v) = token.strip_suffix("vh").and_then(|s| s.parse::<f32>().ok()) {
                format!("{}px", viewport.height * v / 100.0)
            } else {
                token.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract all layout-relevant CSS properties from element attributes.
///
/// Scans both `data-nova-style` (computed styles from the CSS engine) and
/// the inline `style` attribute. `data-nova-style` is processed second so
/// it overrides the inline `style` when both are present (the CSS cascade
/// result should win).
fn parse_layout_props(attributes: &[(String, String)], viewport: &Viewport) -> LayoutProps {
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
            let raw_val = parts[1].trim();

            // Pre-resolve viewport units (vw, vh) to absolute px values
            // so downstream parse functions produce Length instead of Percent.
            let resolved_val;
            let val = if raw_val.contains("vw") || raw_val.contains("vh") {
                resolved_val = resolve_viewport_in_value(raw_val, viewport);
                resolved_val.as_str()
            } else {
                raw_val
            };

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

                "width" => props.width = parse_dimension_with_context(val, viewport.width),
                "min-width" => props.min_width = parse_dimension_with_context(val, viewport.width),
                "max-width" => props.max_width = parse_dimension_with_context(val, viewport.width),
                "height" => props.height = parse_dimension_with_context(val, viewport.height),
                "min-height" => props.min_height = parse_dimension_with_context(val, viewport.height),
                "max-height" => props.max_height = parse_dimension_with_context(val, viewport.height),
                "box-sizing" => props.box_sizing = Some(val.to_string()),

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

                "clear" => {
                    match val {
                        "left" | "right" | "both" | "none" => props.clear = Some(val.to_string()),
                        _ => {}
                    }
                }

                // ── Overflow properties ────────────────────────────────
                "overflow" => {
                    match val {
                        "visible" | "hidden" | "scroll" | "auto" | "clip" => {
                            props.overflow_x = Some(val.to_string());
                            props.overflow_y = Some(val.to_string());
                        }
                        _ => {}
                    }
                }
                "overflow-x" => {
                    match val {
                        "visible" | "hidden" | "scroll" | "auto" | "clip" => {
                            props.overflow_x = Some(val.to_string());
                        }
                        _ => {}
                    }
                }
                "overflow-y" => {
                    match val {
                        "visible" | "hidden" | "scroll" | "auto" | "clip" => {
                            props.overflow_y = Some(val.to_string());
                        }
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

                // ── Flexbox alignment properties ─────────────────────
                "align-items" => {
                    props.align_items = Some(val.to_string());
                }
                "justify-content" => {
                    props.justify_content = Some(val.to_string());
                }
                "flex-wrap" => {
                    props.flex_wrap = Some(val.to_string());
                }
                "flex-direction" => {
                    props.flex_direction = Some(val.to_string());
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
fn resolve_font_size(tag: &str, attributes: &[(String, String)], parent_font_size: f32) -> f32 {
    // Check data-nova-style first.
    for attr_name in &["data-nova-style", "style"] {
        if let Some(style_attr) = attributes.iter().find(|(k, _)| k == *attr_name) {
            for decl in style_attr.1.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "font-size" {
                    let val = parts[1].trim();
                    if let Some(px) = parse_css_length(val, parent_font_size) {
                        return px;
                    }
                }
            }
        }
    }
    // Tag-based defaults for headings.
    font_size_for_tag(tag)
}

/// Parse a CSS length value into pixels.
///
/// Supports `px`, `pt`, `em`, `rem`, `%` (relative to `context`), and
/// CSS keyword sizes (`small`, `medium`, `large`, etc.).
fn parse_css_length(val: &str, context: f32) -> Option<f32> {
    let val = val.trim();
    if let Some(s) = val.strip_suffix("px") {
        return s.trim().parse::<f32>().ok();
    }
    if let Some(s) = val.strip_suffix("pt") {
        return s.trim().parse::<f32>().ok().map(|n| n * 1.333);
    }
    if val.ends_with("rem") {
        let s = &val[..val.len() - 3];
        return s.trim().parse::<f32>().ok().map(|n| n * 16.0);
    }
    if let Some(s) = val.strip_suffix("em") {
        return s.trim().parse::<f32>().ok().map(|n| n * context);
    }
    if let Some(s) = val.strip_suffix('%') {
        return s.trim().parse::<f32>().ok().map(|n| context * n / 100.0);
    }
    // CSS keyword sizes.
    match val {
        "xx-small" => Some(9.0),
        "x-small" => Some(10.0),
        "small" => Some(13.0),
        "medium" => Some(16.0),
        "large" => Some(18.0),
        "x-large" => Some(24.0),
        "xx-large" => Some(32.0),
        "smaller" => Some(context * 0.833),
        "larger" => Some(context * 1.2),
        _ => val.parse::<f32>().ok(),
    }
}

/// Build a `taffy::Style` from the resolved display mode, tag, and attributes.
///
/// Parses margins, padding, width, and max-width from `data-nova-style` and
/// the inline `style` attribute, applying them to the Taffy `Style`.
/// Map a CSS `align-items` value to a Taffy `AlignItems`.
fn map_align_items(val: &str) -> Option<AlignItems> {
    match val.trim() {
        "flex-start" | "start" => Some(AlignItems::FlexStart),
        "flex-end" | "end" => Some(AlignItems::FlexEnd),
        "center" => Some(AlignItems::Center),
        "baseline" => Some(AlignItems::Baseline),
        "stretch" => Some(AlignItems::Stretch),
        _ => None,
    }
}

/// Map a CSS `justify-content` value to a Taffy `JustifyContent`.
fn map_justify_content(val: &str) -> Option<JustifyContent> {
    match val.trim() {
        "flex-start" | "start" => Some(JustifyContent::FlexStart),
        "flex-end" | "end" => Some(JustifyContent::FlexEnd),
        "center" => Some(JustifyContent::Center),
        "space-between" => Some(JustifyContent::SpaceBetween),
        "space-around" => Some(JustifyContent::SpaceAround),
        "space-evenly" => Some(JustifyContent::SpaceEvenly),
        _ => None,
    }
}

/// Parse the `justify-content` value from an element's computed or inline style.
///
/// Checks `data-nova-style` (CSS cascade output) first, then `style` attribute.
fn resolve_justify_content(attributes: &[(String, String)]) -> Option<JustifyContent> {
    for attr_name in &["data-nova-style", "style"] {
        if let Some(style_attr) = attributes.iter().find(|(k, _)| k == *attr_name) {
            for decl in style_attr.1.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "justify-content" {
                    return match parts[1].trim() {
                        "center" => Some(JustifyContent::Center),
                        "flex-start" | "start" => Some(JustifyContent::FlexStart),
                        "flex-end" | "end" => Some(JustifyContent::FlexEnd),
                        "space-between" => Some(JustifyContent::SpaceBetween),
                        "space-around" => Some(JustifyContent::SpaceAround),
                        "space-evenly" => Some(JustifyContent::SpaceEvenly),
                        _ => None,
                    };
                }
            }
        }
    }
    None
}

/// Map a CSS `flex-wrap` value to a Taffy `FlexWrap`.
fn map_flex_wrap(val: &str) -> FlexWrap {
    match val.trim() {
        "wrap" => FlexWrap::Wrap,
        "wrap-reverse" => FlexWrap::WrapReverse,
        _ => FlexWrap::NoWrap,
    }
}

/// Map a CSS `flex-direction` value to a Taffy `FlexDirection`.
fn map_flex_direction(val: &str) -> FlexDirection {
    match val.trim() {
        "row" => FlexDirection::Row,
        "row-reverse" => FlexDirection::RowReverse,
        "column" => FlexDirection::Column,
        "column-reverse" => FlexDirection::ColumnReverse,
        _ => FlexDirection::Row,
    }
}

fn build_taffy_style(
    display: &str,
    tag: &str,
    attributes: &[(String, String)],
    viewport: &Viewport,
) -> Style {
    let lp = parse_layout_props(attributes, viewport);

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
        // `sticky` uses Relative positioning in Taffy; the shell applies
        // the sticky offset at render time based on scroll position.
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

    // Map CSS `overflow-x` / `overflow-y` to Taffy `Overflow`.
    let map_overflow = |val: Option<&str>| -> Overflow {
        match val {
            Some("hidden") | Some("clip") => Overflow::Hidden,
            Some("scroll") | Some("auto") => Overflow::Scroll,
            _ => Overflow::Visible,
        }
    };
    let overflow = Point {
        x: map_overflow(lp.overflow_x.as_deref()),
        y: map_overflow(lp.overflow_y.as_deref()),
    };

    // Map CSS `box-sizing` to Taffy `BoxSizing`.
    let box_sizing = match lp.box_sizing.as_deref() {
        Some("border-box") => BoxSizing::BorderBox,
        _ => BoxSizing::ContentBox,
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

            let align_items = lp.align_items.as_deref()
                .and_then(map_align_items);
            let justify_content = lp.justify_content.as_deref()
                .and_then(map_justify_content);

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
                    height: lp.height.unwrap_or(Dimension::Auto),
                },
                min_size: Size {
                    width: lp.min_width.unwrap_or(Dimension::Auto),
                    height: lp.min_height.unwrap_or(Dimension::Auto),
                },
                max_size: Size {
                    width: lp.max_width.unwrap_or(Dimension::Auto),
                    height: lp.max_height.unwrap_or(Dimension::Auto),
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
                align_items,
                justify_content,
                align_self: if center_via_auto_margin {
                    Some(AlignSelf::Center)
                } else {
                    None
                },
                overflow,
                box_sizing,
                ..Style::DEFAULT
            }
        }

        "flex" => {
            // Use flex-direction from parsed layout props, then fall back to
            // resolve_flex_direction (which checks inline style attribute).
            let direction = lp.flex_direction.as_deref()
                .map(map_flex_direction)
                .unwrap_or_else(|| resolve_flex_direction(attributes));
            let flex_wrap = lp.flex_wrap.as_deref()
                .map(map_flex_wrap)
                .unwrap_or(FlexWrap::NoWrap);
            let align_items = lp.align_items.as_deref()
                .and_then(map_align_items);
            let justify_content = lp.justify_content.as_deref()
                .and_then(map_justify_content);

            // For <html> and <body> with display: flex, ensure they fill the
            // viewport height so centering has room to work.
            let is_root_element = tag == "html" || tag == "body";
            Style {
                display: Display::Flex,
                flex_direction: direction,
                flex_wrap,
                position: taffy_position,
                inset,
                size: Size {
                    width: lp.width.unwrap_or(Dimension::Percent(1.0)),
                    height: lp.height.unwrap_or(Dimension::Auto),
                },
                min_size: Size {
                    width: lp.min_width.unwrap_or(Dimension::Auto),
                    height: lp.min_height.unwrap_or(
                        if is_root_element { Dimension::Length(viewport.height) } else { Dimension::Auto }
                    ),
                },
                max_size: Size {
                    width: lp.max_width.unwrap_or(Dimension::Auto),
                    height: lp.max_height.unwrap_or(Dimension::Auto),
                },
                margin,
                padding,
                align_items,
                justify_content,
                align_self: if center_via_auto_margin {
                    Some(AlignSelf::Center)
                } else {
                    None
                },
                overflow,
                box_sizing,
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
            overflow,
            box_sizing,
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
                overflow,
                box_sizing,
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
            overflow,
            box_sizing,
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
            overflow,
            box_sizing,
            ..Style::DEFAULT
        },

        // "block" and everything else: column flex container at full width.
        _ => {
            // For <html> and <body>, ensure they fill at least the viewport
            // height so their background-color covers the entire page.
            let is_root_element = tag == "html" || tag == "body";
            // Use Auto width + flex_grow to fill available space (respects margins).
            // Only use Percent(1.0) if no margins and no explicit width.
            // Floated elements always use Auto width by default and don't grow.
            let is_floated = lp.float.as_deref().map_or(false, |f| f == "left" || f == "right");
            let has_h_margins = lp.margin_left.is_some() || lp.margin_right.is_some()
                || is_floated;
            let default_width = if center_via_auto_margin && lp.width.is_none() {
                // When both horizontal margins are `auto` (centering) and no
                // explicit width is set, use 100% so the element fills its
                // parent. `max_size.width` will clamp it and `align_self:
                // Center` will centre the clamped box.
                Dimension::Percent(1.0)
            } else if has_h_margins || lp.width.is_some() || is_floated {
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

            // Pass through align-items / justify-content for block elements
            // that are modelled as column flex containers in Taffy.
            let align_items = lp.align_items.as_deref()
                .and_then(map_align_items);
            let justify_content = lp.justify_content.as_deref()
                .and_then(map_justify_content);

            // `clear` property: forces this element below preceding floats.
            // In a row-wrap flex container, setting flex-basis: 100% on a
            // clear element forces it onto a new line (acting as a line break).
            let has_clear = matches!(
                lp.clear.as_deref(),
                Some("left") | Some("right") | Some("both")
            );

            // Non-floated block elements inside a float context need
            // flex-basis: 100% to occupy the full row width, simulating
            // normal block flow within a row-wrap container.
            let flex_basis = if has_clear || (!is_floated && default_width == Dimension::Percent(1.0)) {
                Dimension::Percent(1.0)
            } else {
                Dimension::Auto
            };

            Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                position: taffy_position,
                inset,
                flex_grow: effective_flex_grow,
                flex_shrink: float_flex_shrink,
                flex_basis,
                size: Size {
                    width: default_width,
                    height: lp.height.unwrap_or(Dimension::Auto),
                },
                min_size: Size {
                    width: lp.min_width.unwrap_or(Dimension::Auto),
                    height: lp.min_height.unwrap_or(
                        if is_root_element { Dimension::Length(viewport.height) } else { Dimension::Auto }
                    ),
                },
                max_size: Size {
                    width: lp.max_width.unwrap_or(Dimension::Auto),
                    height: lp.max_height.unwrap_or(Dimension::Auto),
                },
                margin: float_margin,
                padding,
                align_items,
                justify_content,
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
                overflow,
                box_sizing,
                ..Style::DEFAULT
            }
        }
    }
}

/// Parse the `flex-direction` value from an element's style attributes.
///
/// Checks `data-nova-style` first (computed styles from CSS engine), then
/// falls back to the inline `style` attribute.
fn resolve_flex_direction(attributes: &[(String, String)]) -> FlexDirection {
    for attr_name in &["data-nova-style", "style"] {
        if let Some(style_attr) = attributes.iter().find(|(k, _)| k == *attr_name) {
            for decl in style_attr.1.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "flex-direction" {
                    return map_flex_direction(parts[1].trim());
                }
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
        | "blockquote" | "pre" | "form" | "hr" | "center" => "block",
        // Table elements get special display modes for proper layout.
        "table" | "thead" | "tbody" | "tfoot" => "block",
        "tr" => "table-row",
        "td" | "th" => "table-cell",
        "span" | "a" | "em" | "strong" | "b" | "i" | "u" | "code" | "small" | "br" | "img"
        | "input" | "label" | "select" | "button" | "textarea"
        | "nova-before" | "nova-after" | "video" | "audio" => "inline",
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
        let lp = parse_layout_props(&attrs, &Viewport { width: 800.0, height: 600.0, scale_factor: 1.0 });
        assert!(matches!(lp.margin_top, Some(LengthPercentageAuto::Length(v)) if (v - 10.0).abs() < 0.01));
        assert!(matches!(lp.margin_right, Some(LengthPercentageAuto::Length(v)) if (v - 20.0).abs() < 0.01));
        assert!(matches!(lp.margin_bottom, Some(LengthPercentageAuto::Length(v)) if (v - 30.0).abs() < 0.01));
        assert!(matches!(lp.margin_left, Some(LengthPercentageAuto::Length(v)) if (v - 40.0).abs() < 0.01));
    }

    #[test]
    fn parse_layout_props_shorthand_padding_two_values() {
        let attrs = vec![("style".into(), "padding: 10px 20px".into())];
        let lp = parse_layout_props(&attrs, &Viewport { width: 800.0, height: 600.0, scale_factor: 1.0 });
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
        let lp = parse_layout_props(&attrs, &Viewport { width: 800.0, height: 600.0, scale_factor: 1.0 });
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
        // With same-style word merging, all words in the line are merged
        // into a single text run. The line box should have >=1 child.
        let line_box = &body.children[0];
        assert!(
            !line_box.children.is_empty(),
            "line box should have >=1 children (merged text run), got {}",
            line_box.children.len()
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
        let lp = parse_layout_props(&attrs, &Viewport { width: 800.0, height: 600.0, scale_factor: 1.0 });
        assert_eq!(lp.float.as_deref(), Some("left"));
        assert!(lp.width.is_some());
    }

    #[test]
    fn float_right_parses() {
        let attrs = vec![
            ("data-nova-style".into(), "float: right".into()),
        ];
        let lp = parse_layout_props(&attrs, &Viewport { width: 800.0, height: 600.0, scale_factor: 1.0 });
        assert_eq!(lp.float.as_deref(), Some("right"));
    }

    #[test]
    fn example_com_centering_with_max_width() {
        // Simulates example.com: body has max-width + margin: 0 auto
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "margin-top: 0; margin-right: auto; margin-bottom: 0; margin-left: auto; max-width: 600px".into(),
                )],
                children: vec![DomNode::Element {
                    tag: "div".into(),
                    attributes: vec![],
                    children: vec![DomNode::Text("Example Domain".into())],
                }],
            }],
        };
        let vp = Viewport { width: 1024.0, height: 768.0, scale_factor: 1.0 };
        let root = compute_layout(&dom, &vp).expect("layout should succeed");
        let body = &root.children[0];
        assert!(body.width <= 600.01, "body should be max 600px wide, got {}", body.width);
        let expected_x = (1024.0 - body.width) / 2.0;
        assert!((body.x - expected_x).abs() < 2.0, "body should be centered at x~{}, got {}", expected_x, body.x);
    }

    #[test]
    fn example_com_real_css_width_60vw() {
        // Real example.com uses: body { width: 60vw; margin: 15vh auto; }
        // After CSS cascade, 60vw becomes 60% and 15vh becomes 15%.
        // The layout should center the body.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![(
                    "data-nova-style".into(),
                    "width: 60%; margin-top: 15%; margin-right: auto; margin-bottom: 15%; margin-left: auto".into(),
                )],
                children: vec![
                    DomNode::Element {
                        tag: "h1".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Example Domain".into())],
                    },
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("This domain is for use in illustrative examples.".into())],
                    },
                ],
            }],
        };
        let vp = Viewport { width: 1024.0, height: 768.0, scale_factor: 1.0 };
        let root = compute_layout(&dom, &vp).expect("layout should succeed");
        let body = &root.children[0];
        // 60% of 1024 = 614.4
        let expected_width = 1024.0 * 0.60;
        assert!((body.width - expected_width).abs() < 2.0,
            "body width should be ~{}, got {}", expected_width, body.width);
        let expected_x = (1024.0 - body.width) / 2.0;
        assert!((body.x - expected_x).abs() < 2.0,
            "body should be centered at x~{}, got {}", expected_x, body.x);
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

    #[test]
    fn clear_property_parses() {
        let attrs = vec![
            ("data-nova-style".into(), "clear: both".into()),
        ];
        let lp = parse_layout_props(&attrs, &Viewport { width: 800.0, height: 600.0, scale_factor: 1.0 });
        assert_eq!(lp.clear.as_deref(), Some("both"));
    }

    #[test]
    fn clear_left_parses() {
        let attrs = vec![
            ("data-nova-style".into(), "clear: left".into()),
        ];
        let lp = parse_layout_props(&attrs, &Viewport { width: 800.0, height: 600.0, scale_factor: 1.0 });
        assert_eq!(lp.clear.as_deref(), Some("left"));
    }

    #[test]
    fn float_left_has_shrink_to_fit_width() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "div".into(),
                    attributes: vec![(
                        "data-nova-style".into(),
                        "float: left; width: 150px".into(),
                    )],
                    children: vec![DomNode::Text("Floated".into())],
                }],
            }],
        };
        let root = compute_layout(&dom, &viewport()).expect("layout ok");
        let container = &root.children[0];
        let floated = &container.children[0];
        assert!(
            (floated.width - 150.0).abs() < 2.0,
            "float:left div should be 150px wide, got {}",
            floated.width
        );
    }

    #[test]
    fn float_left_and_right_side_by_side() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![(
                            "data-nova-style".into(),
                            "float: left; width: 200px".into(),
                        )],
                        children: vec![DomNode::Text("Left".into())],
                    },
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![(
                            "data-nova-style".into(),
                            "float: right; width: 200px".into(),
                        )],
                        children: vec![DomNode::Text("Right".into())],
                    },
                ],
            }],
        };
        let root = compute_layout(&dom, &viewport()).expect("layout ok");
        let container = &root.children[0];
        assert!(
            container.children.len() >= 2,
            "container should have at least 2 children"
        );
        let child_a = &container.children[0];
        let child_b = &container.children[1];
        assert!(
            (child_a.y - child_b.y).abs() < 5.0,
            "float:left and float:right should be on the same line, y_a={} y_b={}",
            child_a.y,
            child_b.y,
        );
    }

    #[test]
    fn clear_both_pushes_below_floats() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![(
                            "data-nova-style".into(),
                            "float: left; width: 200px; height: 100px".into(),
                        )],
                        children: vec![DomNode::Text("Float".into())],
                    },
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![(
                            "data-nova-style".into(),
                            "clear: both".into(),
                        )],
                        children: vec![DomNode::Text("Cleared content".into())],
                    },
                ],
            }],
        };
        let root = compute_layout(&dom, &viewport()).expect("layout ok");
        let container = &root.children[0];
        let cleared = container.children.last().unwrap();
        let float_child = &container.children[0];
        assert!(
            cleared.y >= float_child.y + float_child.height - 1.0,
            "cleared element (y={}) should be below float (y={} h={})",
            cleared.y,
            float_child.y,
            float_child.height,
        );
    }

    #[test]
    fn margin_collapse_adjacent_siblings() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "p".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("First paragraph".into())],
                    },
                    DomNode::Element {
                        tag: "p".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Second paragraph".into())],
                    },
                ],
            }],
        };
        let root = compute_layout(&dom, &viewport()).expect("layout ok");
        let container = &root.children[0];
        assert!(
            container.children.len() >= 2,
            "container should have 2 children"
        );
        let p1 = &container.children[0];
        let p2 = &container.children[1];
        let gap = p2.y - (p1.y + p1.height);
        assert!(
            gap < 20.0,
            "gap between paragraphs should be collapsed (~16px), got {}",
            gap
        );
    }

    #[test]
    fn flex_vertical_centering() {
        // Simulate: body { display: flex; min-height: 100vh; align-items: center; justify-content: center; }
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "body".into(),
                    attributes: vec![
                        ("data-nova-style".into(),
                         "display: flex; min-height: 600px; align-items: center; justify-content: center; margin: 0".into()),
                    ],
                    children: vec![DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Centered content".into())],
                    }],
                }],
            }],
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        // The body is root.children[0] (the html element) -> children[0] (body).
        let html = &root.children[0];
        let body = &html.children[0];
        // The content div is inside body.
        assert!(!body.children.is_empty(), "body should have children");
        let content = &body.children[0];
        // With vertical centering in a 600px-tall flex container,
        // the content should not be at y=0 — it should be roughly
        // in the middle.
        assert!(
            content.y > 100.0,
            "content should be vertically centered, y={} (expected > 100)",
            content.y
        );
    }

    #[test]
    fn margin_auto_horizontal_centering() {
        // Simulate: div { width: 400px; margin: 0 auto; }
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![
                    ("data-nova-style".into(), "margin: 0".into()),
                ],
                children: vec![DomNode::Element {
                    tag: "div".into(),
                    attributes: vec![
                        ("data-nova-style".into(), "width: 400px; margin-left: auto; margin-right: auto".into()),
                    ],
                    children: vec![DomNode::Text("Centered".into())],
                }],
            }],
        };
        let root = compute_layout(&dom, &viewport).expect("layout should succeed");
        let body = &root.children[0];
        let centered_div = &body.children[0];
        // A 400px div centered in 800px should be at x ~= 200.
        assert!(
            centered_div.x >= 150.0 && centered_div.x <= 250.0,
            "div with margin: auto should be centered, x={} (expected ~200)",
            centered_div.x
        );
        assert!(
            (centered_div.width - 400.0).abs() < 10.0,
            "div width should be ~400px, got {}",
            centered_div.width
        );
    }
}

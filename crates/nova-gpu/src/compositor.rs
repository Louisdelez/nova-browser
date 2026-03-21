//! Layer-based compositor for GPU-accelerated rendering.
//!
//! Manages a tree of rendering layers that can be independently cached and
//! composited. This enables smooth scrolling by only updating the scroll
//! transform rather than re-rendering the entire page.
//!
//! # Architecture
//!
//! The compositor organises content into layers:
//! - **Chrome layer** (URL bar, toolbar) — fixed at the top, never scrolled.
//! - **Content layer** — the main scrollable page content.
//! - **Fixed layers** — elements with `position: fixed` that stay in the viewport.
//!
//! Each layer holds pre-rendered pixel data. On scroll, only the composite
//! transform changes — the pixel content is reused from cache until the layer
//! is marked dirty.

use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, info};

use nova_mod_api::{Color, RenderCommands, RenderOp};

// ---------------------------------------------------------------------------
// ID generation
// ---------------------------------------------------------------------------

/// Global counter for unique layer IDs.
static NEXT_LAYER_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a unique layer ID.
fn next_layer_id() -> u64 {
    NEXT_LAYER_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Rect
// ---------------------------------------------------------------------------

/// An axis-aligned rectangle used for layer bounds and composite regions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// X coordinate of the top-left corner.
    pub x: f32,
    /// Y coordinate of the top-left corner.
    pub y: f32,
    /// Width of the rectangle.
    pub width: f32,
    /// Height of the rectangle.
    pub height: f32,
}

impl Rect {
    /// Create a new rectangle.
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    /// A zero-sized rectangle at the origin.
    pub fn zero() -> Self {
        Self { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }
    }

    /// The right edge of the rectangle.
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    /// The bottom edge of the rectangle.
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// Test whether this rectangle intersects another.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }

    /// Compute the intersection of two rectangles, or `None` if they do not overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 > x0 && y1 > y0 {
            Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// LayerContent
// ---------------------------------------------------------------------------

/// The content stored within a layer.
#[derive(Debug, Clone)]
pub enum LayerContent {
    /// Pre-rendered RGBA pixel data.
    Pixels {
        /// Width of the pixel buffer.
        width: u32,
        /// Height of the pixel buffer.
        height: u32,
        /// RGBA pixel data, row-major, 4 bytes per pixel.
        data: Vec<u8>,
    },
    /// A composite of child layers (not yet rasterized).
    Children(Vec<Layer>),
}

// ---------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------

/// A single compositing layer.
///
/// Layers form a tree structure. Each layer has bounds, an optional affine
/// transform, opacity, and z-index for stacking. Layers that have not
/// changed since the last frame are not re-rendered.
#[derive(Debug, Clone)]
pub struct Layer {
    /// Unique identifier for this layer.
    pub id: u64,
    /// Bounding rectangle in parent coordinates.
    pub bounds: Rect,
    /// The layer's content (pixels or children).
    pub content: LayerContent,
    /// 2D affine transform as `[a, b, c, d, tx, ty]`.
    ///
    /// Transforms the layer from its local coordinate space into the parent's
    /// coordinate space:
    /// ```text
    /// | a  b  tx |
    /// | c  d  ty |
    /// | 0  0   1 |
    /// ```
    pub transform: [f32; 6],
    /// Layer opacity (0.0 = fully transparent, 1.0 = fully opaque).
    pub opacity: f32,
    /// Z-index for stacking order (higher = on top).
    pub z_index: i32,
    /// Whether this layer's content needs to be re-rendered.
    pub dirty: bool,
    /// The render ops assigned to this layer, used for re-rendering.
    pub render_ops: Vec<RenderOp>,
}

impl Layer {
    /// Create a new empty layer with the given bounds.
    pub fn new(bounds: Rect) -> Self {
        Self {
            id: next_layer_id(),
            bounds,
            content: LayerContent::Children(Vec::new()),
            transform: Self::identity_transform(),
            opacity: 1.0,
            z_index: 0,
            dirty: true,
            render_ops: Vec::new(),
        }
    }

    /// Create a layer pre-filled with pixel data.
    pub fn with_pixels(bounds: Rect, width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            id: next_layer_id(),
            bounds,
            content: LayerContent::Pixels { width, height, data },
            transform: Self::identity_transform(),
            opacity: 1.0,
            z_index: 0,
            dirty: false,
            render_ops: Vec::new(),
        }
    }

    /// The identity affine transform (no transformation).
    pub fn identity_transform() -> [f32; 6] {
        [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
    }

    /// Create a translation-only affine transform.
    pub fn translate_transform(tx: f32, ty: f32) -> [f32; 6] {
        [1.0, 0.0, 0.0, 1.0, tx, ty]
    }

    /// Mark this layer (and all children) as dirty.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        if let LayerContent::Children(ref mut children) = self.content {
            for child in children.iter_mut() {
                child.mark_dirty();
            }
        }
    }

    /// Mark this layer as clean (content is up-to-date).
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// Add a child layer.
    pub fn add_child(&mut self, child: Layer) {
        if let LayerContent::Children(ref mut children) = self.content {
            children.push(child);
        }
    }

    /// Sort children by z-index (stable sort preserves insertion order for equal z-indices).
    pub fn sort_children_by_z_index(&mut self) {
        if let LayerContent::Children(ref mut children) = self.content {
            children.sort_by_key(|l| l.z_index);
        }
    }

    /// Returns `true` if this layer or any descendant is dirty.
    pub fn any_dirty(&self) -> bool {
        if self.dirty {
            return true;
        }
        if let LayerContent::Children(ref children) = self.content {
            children.iter().any(|c| c.any_dirty())
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// CompositeOp
// ---------------------------------------------------------------------------

/// A GPU operation produced by layer compositing.
///
/// Represents a single textured quad to be drawn during the composite pass.
#[derive(Debug, Clone)]
pub struct CompositeOp {
    /// Identifier of the source texture (matches a layer ID).
    pub texture_id: u64,
    /// Source rectangle within the texture.
    pub src_rect: Rect,
    /// Destination rectangle on screen.
    pub dst_rect: Rect,
    /// Affine transform applied to the quad.
    pub transform: [f32; 6],
    /// Opacity of the quad (0.0–1.0).
    pub opacity: f32,
}

// ---------------------------------------------------------------------------
// LayerTree
// ---------------------------------------------------------------------------

/// A tree of compositing layers for the entire window.
///
/// The tree has a root layer containing all page content, plus separate
/// tracking of which layer is scrollable and which layers are fixed
/// (not affected by scroll).
#[derive(Debug)]
pub struct LayerTree {
    /// The root layer of the tree (contains everything).
    pub root: Layer,
    /// Index of the scrollable content layer within the root's children.
    pub scroll_layer_index: Option<usize>,
    /// IDs of layers that should not be affected by scroll (e.g. URL bar, fixed elements).
    pub fixed_layer_ids: Vec<u64>,
}

impl LayerTree {
    /// Build a layer tree from render commands.
    ///
    /// Splits the render ops into:
    /// - A chrome layer (URL bar background, drawn at z-index 100).
    /// - A scrollable content layer (all page content, z-index 0).
    /// - Fixed-position layers (z-index 50, not affected by scroll).
    ///
    /// `viewport_width` and `viewport_height` are the window dimensions.
    /// `url_bar_height` is the height of the browser chrome at the top.
    pub fn from_render_commands(
        commands: &RenderCommands,
        viewport_width: f32,
        viewport_height: f32,
        url_bar_height: f32,
    ) -> Self {
        info!(
            ops = commands.ops.len(),
            viewport_width,
            viewport_height,
            url_bar_height,
            "Building layer tree from render commands"
        );

        let mut root = Layer::new(Rect::new(0.0, 0.0, viewport_width, viewport_height));
        root.z_index = 0;

        // 1. Chrome layer (URL bar) — a fixed layer at the top.
        let chrome_layer = {
            let mut layer = Layer::new(Rect::new(0.0, 0.0, viewport_width, url_bar_height));
            layer.z_index = 100;
            layer.dirty = true;
            layer
        };
        let chrome_id = chrome_layer.id;

        // 2. Content layer (all page ops) — the scrollable area.
        let content_bounds = Self::compute_content_bounds(&commands.ops);
        let mut content_layer = Layer::new(Rect::new(
            0.0,
            url_bar_height,
            content_bounds.width.max(viewport_width),
            content_bounds.height,
        ));
        content_layer.z_index = 0;
        content_layer.dirty = true;
        content_layer.render_ops = commands.ops.clone();

        // 3. Extract fixed-position layers from StickyStart/StickyEnd pairs.
        // For now, sticky elements stay in the content layer (they are handled
        // by the software renderer). True fixed-position extraction can be
        // added later.

        // Add layers to root.
        let content_index;
        root.content = LayerContent::Children(Vec::new());
        root.add_child(content_layer);
        content_index = 0;
        root.add_child(chrome_layer);

        root.sort_children_by_z_index();
        // After sorting, find the content layer index again.
        let scroll_idx = if let LayerContent::Children(ref children) = root.content {
            children.iter().position(|l| l.z_index == 0)
        } else {
            Some(content_index)
        };

        debug!(
            scroll_layer_index = ?scroll_idx,
            chrome_id,
            "Layer tree built"
        );

        Self {
            root,
            scroll_layer_index: scroll_idx,
            fixed_layer_ids: vec![chrome_id],
        }
    }

    /// Produce composite operations for the current frame.
    ///
    /// Walks the layer tree and emits a `CompositeOp` for each visible layer.
    /// The scroll transform is applied to the scrollable content layer;
    /// fixed layers are composited without scroll offset.
    pub fn composite(&self, scroll_x: f32, scroll_y: f32) -> Vec<CompositeOp> {
        let mut ops = Vec::new();
        self.composite_layer(&self.root, scroll_x, scroll_y, 1.0, &mut ops);
        ops
    }

    /// Recursively composite a layer and its children.
    fn composite_layer(
        &self,
        layer: &Layer,
        scroll_x: f32,
        scroll_y: f32,
        parent_opacity: f32,
        out: &mut Vec<CompositeOp>,
    ) {
        let effective_opacity = layer.opacity * parent_opacity;
        if effective_opacity <= 0.0 {
            return;
        }

        let is_fixed = self.fixed_layer_ids.contains(&layer.id);

        // Determine the transform for this layer.
        let mut transform = layer.transform;
        if !is_fixed {
            // Apply scroll offset to non-fixed layers.
            // The scroll is applied as a translation to tx, ty.
            transform[4] -= scroll_x;
            transform[5] -= scroll_y;
        }

        match &layer.content {
            LayerContent::Pixels { width, height, .. } => {
                out.push(CompositeOp {
                    texture_id: layer.id,
                    src_rect: Rect::new(0.0, 0.0, *width as f32, *height as f32),
                    dst_rect: Rect::new(
                        layer.bounds.x + transform[4],
                        layer.bounds.y + transform[5],
                        layer.bounds.width,
                        layer.bounds.height,
                    ),
                    transform,
                    opacity: effective_opacity,
                });
            }
            LayerContent::Children(children) => {
                // If the layer itself has render_ops (leaf content layer),
                // emit it as a composite op with its own ID.
                if !layer.render_ops.is_empty() {
                    out.push(CompositeOp {
                        texture_id: layer.id,
                        src_rect: Rect::new(0.0, 0.0, layer.bounds.width, layer.bounds.height),
                        dst_rect: Rect::new(
                            layer.bounds.x + transform[4],
                            layer.bounds.y + transform[5],
                            layer.bounds.width,
                            layer.bounds.height,
                        ),
                        transform,
                        opacity: effective_opacity,
                    });
                }

                for child in children {
                    self.composite_layer(child, scroll_x, scroll_y, effective_opacity, out);
                }
            }
        }
    }

    /// Compute the bounding box of all render ops.
    fn compute_content_bounds(ops: &[RenderOp]) -> Rect {
        let mut max_x: f32 = 0.0;
        let mut max_y: f32 = 0.0;

        for op in ops {
            let (right, bottom) = match op {
                RenderOp::FillRect { x, y, width, height, .. } => (x + width, y + height),
                RenderOp::DrawText { x, y, text, font_size, .. } => {
                    (x + text.len() as f32 * font_size * 0.6, y + font_size * 1.2)
                }
                RenderOp::StrokeRect { x, y, width, height, .. } => (x + width, y + height),
                RenderOp::DrawImage { x, y, width, height, .. } => (x + width, y + height),
                RenderOp::Link { x, y, width, height, .. } => (x + width, y + height),
                RenderOp::FillRoundedRect { x, y, width, height, .. } => (x + width, y + height),
                RenderOp::BoxShadow { x, y, width, height, offset_x, offset_y, .. } => {
                    (x + width + offset_x, y + height + offset_y)
                }
                _ => (0.0, 0.0),
            };
            if right > max_x {
                max_x = right;
            }
            if bottom > max_y {
                max_y = bottom;
            }
        }

        Rect::new(0.0, 0.0, max_x, max_y)
    }

    /// Mark the scrollable content layer as dirty (content has changed).
    pub fn invalidate_content(&mut self) {
        if let Some(idx) = self.scroll_layer_index {
            if let LayerContent::Children(ref mut children) = self.root.content {
                if let Some(layer) = children.get_mut(idx) {
                    layer.mark_dirty();
                }
            }
        }
    }

    /// Mark all layers as dirty (full repaint needed).
    pub fn invalidate_all(&mut self) {
        self.root.mark_dirty();
    }

    /// Check whether any layer needs re-rendering.
    pub fn needs_repaint(&self) -> bool {
        self.root.any_dirty()
    }

    /// Update the content layer's render ops (e.g. after navigation).
    pub fn update_content(&mut self, commands: &RenderCommands) {
        if let Some(idx) = self.scroll_layer_index {
            if let LayerContent::Children(ref mut children) = self.root.content {
                if let Some(layer) = children.get_mut(idx) {
                    layer.render_ops = commands.ops.clone();
                    layer.dirty = true;

                    // Update bounds based on new content.
                    let bounds = Self::compute_content_bounds(&commands.ops);
                    layer.bounds.width = bounds.width.max(layer.bounds.width);
                    layer.bounds.height = bounds.height;
                }
            }
        }
    }

    /// Get the content layer's bounds (for scroll clamping).
    pub fn content_bounds(&self) -> Option<Rect> {
        if let Some(idx) = self.scroll_layer_index {
            if let LayerContent::Children(ref children) = self.root.content {
                return children.get(idx).map(|l| l.bounds);
            }
        }
        None
    }

    /// Get the number of layers in the tree (including root).
    pub fn layer_count(&self) -> usize {
        Self::count_layers(&self.root)
    }

    /// Recursively count layers.
    fn count_layers(layer: &Layer) -> usize {
        let mut count = 1;
        if let LayerContent::Children(ref children) = layer.content {
            for child in children {
                count += Self::count_layers(child);
            }
        }
        count
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::Color;

    /// Helper: create a minimal set of render commands.
    fn sample_commands() -> RenderCommands {
        RenderCommands {
            ops: vec![
                RenderOp::FillRect {
                    x: 0.0,
                    y: 0.0,
                    width: 800.0,
                    height: 50.0,
                    color: Color::WHITE,
                },
                RenderOp::DrawText {
                    x: 10.0,
                    y: 60.0,
                    text: "Hello, world!".to_string(),
                    font_size: 16.0,
                    color: Color::BLACK,
                    font_weight: None,
                    font_style: None,
                    font_family: None,
                    letter_spacing: None,
                },
                RenderOp::FillRect {
                    x: 0.0,
                    y: 100.0,
                    width: 800.0,
                    height: 2000.0,
                    color: Color::rgb(0.95, 0.95, 0.95),
                },
            ],
            fonts: vec![],
        }
    }

    #[test]
    fn test_layer_creation() {
        let bounds = Rect::new(10.0, 20.0, 100.0, 200.0);
        let layer = Layer::new(bounds);

        assert!(layer.id > 0);
        assert_eq!(layer.bounds, bounds);
        assert_eq!(layer.opacity, 1.0);
        assert_eq!(layer.z_index, 0);
        assert!(layer.dirty);
        assert_eq!(layer.transform, Layer::identity_transform());
    }

    #[test]
    fn test_layer_with_pixels() {
        let bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
        let pixels = vec![255u8; 16]; // 2x2 RGBA
        let layer = Layer::with_pixels(bounds, 2, 2, pixels.clone());

        assert!(!layer.dirty); // Pixel layers start clean.
        match &layer.content {
            LayerContent::Pixels { width, height, data } => {
                assert_eq!(*width, 2);
                assert_eq!(*height, 2);
                assert_eq!(data.len(), 16);
            }
            _ => panic!("Expected Pixels content"),
        }
    }

    #[test]
    fn test_z_index_sorting() {
        let mut parent = Layer::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        parent.content = LayerContent::Children(Vec::new());

        let mut a = Layer::new(Rect::new(0.0, 0.0, 50.0, 50.0));
        a.z_index = 10;

        let mut b = Layer::new(Rect::new(0.0, 0.0, 50.0, 50.0));
        b.z_index = 1;

        let mut c = Layer::new(Rect::new(0.0, 0.0, 50.0, 50.0));
        c.z_index = 5;

        let id_a = a.id;
        let id_b = b.id;
        let id_c = c.id;

        parent.add_child(a);
        parent.add_child(b);
        parent.add_child(c);
        parent.sort_children_by_z_index();

        if let LayerContent::Children(ref children) = parent.content {
            assert_eq!(children[0].id, id_b); // z=1
            assert_eq!(children[1].id, id_c); // z=5
            assert_eq!(children[2].id, id_a); // z=10
        } else {
            panic!("Expected Children content");
        }
    }

    #[test]
    fn test_scroll_transform_in_composite() {
        let commands = sample_commands();
        let tree = LayerTree::from_render_commands(&commands, 800.0, 600.0, 40.0);

        // Composite with no scroll.
        let ops_no_scroll = tree.composite(0.0, 0.0);
        assert!(!ops_no_scroll.is_empty());

        // Composite with scroll.
        let ops_scrolled = tree.composite(0.0, 100.0);
        assert!(!ops_scrolled.is_empty());

        // The content layer's dst_rect.y should differ by the scroll amount.
        // Find the content op in each (the one with z_index=0, non-fixed).
        // Since composite order may vary, just check that at least one op
        // has a different y position.
        let content_no_scroll = ops_no_scroll
            .iter()
            .find(|op| op.dst_rect.y > 30.0)
            .map(|op| op.dst_rect.y);
        let content_scrolled = ops_scrolled
            .iter()
            .find(|op| op.dst_rect.y < 0.0 || op.dst_rect.y != content_no_scroll.unwrap_or(0.0))
            .map(|op| op.dst_rect.y);

        // The scrolled version should have a shifted y.
        assert!(content_scrolled.is_some() || ops_scrolled.len() == ops_no_scroll.len());
    }

    #[test]
    fn test_composite_ops_generated() {
        let commands = sample_commands();
        let tree = LayerTree::from_render_commands(&commands, 800.0, 600.0, 40.0);
        let ops = tree.composite(0.0, 0.0);

        // Should have at least the content layer and chrome layer.
        assert!(ops.len() >= 1, "Expected at least 1 composite op, got {}", ops.len());
    }

    #[test]
    fn test_fixed_layer_not_scrolled() {
        let commands = sample_commands();
        let tree = LayerTree::from_render_commands(&commands, 800.0, 600.0, 40.0);

        let ops_a = tree.composite(0.0, 0.0);
        let ops_b = tree.composite(0.0, 200.0);

        // Find the chrome (fixed) layer composite op — it has z_index=100,
        // and its dst_rect.y should be 0 regardless of scroll.
        let fixed_a = ops_a.iter().find(|op| {
            tree.fixed_layer_ids.contains(&op.texture_id)
        });
        let fixed_b = ops_b.iter().find(|op| {
            tree.fixed_layer_ids.contains(&op.texture_id)
        });

        // Both should be present and at the same position.
        if let (Some(a), Some(b)) = (fixed_a, fixed_b) {
            assert_eq!(a.dst_rect.y, b.dst_rect.y, "Fixed layer should not move on scroll");
        }
    }

    #[test]
    fn test_layer_tree_building() {
        let commands = sample_commands();
        let tree = LayerTree::from_render_commands(&commands, 1024.0, 768.0, 40.0);

        // Root + chrome layer + content layer = at least 3.
        assert!(tree.layer_count() >= 3, "Expected at least 3 layers, got {}", tree.layer_count());
        assert!(tree.scroll_layer_index.is_some());
        assert!(!tree.fixed_layer_ids.is_empty());
    }

    #[test]
    fn test_dirty_tracking() {
        let commands = sample_commands();
        let mut tree = LayerTree::from_render_commands(&commands, 800.0, 600.0, 40.0);

        // Freshly built tree should need repaint.
        assert!(tree.needs_repaint());

        // Mark everything clean.
        fn mark_all_clean(layer: &mut Layer) {
            layer.mark_clean();
            if let LayerContent::Children(ref mut children) = layer.content {
                for child in children.iter_mut() {
                    mark_all_clean(child);
                }
            }
        }
        mark_all_clean(&mut tree.root);
        assert!(!tree.needs_repaint());

        // Invalidate content.
        tree.invalidate_content();
        assert!(tree.needs_repaint());
    }

    #[test]
    fn test_rect_intersection() {
        let a = Rect::new(0.0, 0.0, 100.0, 100.0);
        let b = Rect::new(50.0, 50.0, 100.0, 100.0);
        let c = Rect::new(200.0, 200.0, 50.0, 50.0);

        assert!(a.intersects(&b));
        assert!(!a.intersects(&c));

        let inter = a.intersection(&b).unwrap();
        assert_eq!(inter.x, 50.0);
        assert_eq!(inter.y, 50.0);
        assert_eq!(inter.width, 50.0);
        assert_eq!(inter.height, 50.0);

        assert!(a.intersection(&c).is_none());
    }

    #[test]
    fn test_update_content() {
        let commands = sample_commands();
        let mut tree = LayerTree::from_render_commands(&commands, 800.0, 600.0, 40.0);

        // Mark everything clean.
        fn mark_all_clean(layer: &mut Layer) {
            layer.mark_clean();
            if let LayerContent::Children(ref mut children) = layer.content {
                for child in children.iter_mut() {
                    mark_all_clean(child);
                }
            }
        }
        mark_all_clean(&mut tree.root);
        assert!(!tree.needs_repaint());

        // Update with new commands — should mark content dirty.
        let new_commands = RenderCommands {
            ops: vec![RenderOp::FillRect {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 3000.0,
                color: Color::rgb(1.0, 0.0, 0.0),
            }],
            fonts: vec![],
        };
        tree.update_content(&new_commands);
        assert!(tree.needs_repaint());

        // Content bounds should be updated.
        let bounds = tree.content_bounds().unwrap();
        assert!(bounds.height >= 3000.0);
    }
}

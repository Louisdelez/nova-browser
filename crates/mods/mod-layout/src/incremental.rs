//! Incremental layout engine with dirty flags and caching.
//!
//! Instead of recomputing layout for the entire DOM tree on every change,
//! this module tracks which nodes are "dirty" (have changed styles or
//! structure) and caches the computed layout for clean subtrees.
//!
//! ## Design
//!
//! - [`DirtyFlags`]: Bitflags indicating what needs relayout on a node.
//! - [`DirtyTracker`]: Per-node dirty flag storage.
//! - [`LayoutCache`]: Hash-based cache mapping (tag, class, id, style, parent_width)
//!   to previously computed dimensions.
//! - [`IncrementalLayoutEngine`]: Orchestrates cache lookups and dirty-flag
//!   propagation during layout.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use tracing::{debug, trace};

use nova_mod_api::content::{DomNode, LayoutBox, LayoutContent, StyleMap, StyleValue};

// ── NodeId ──────────────────────────────────────────────────────────────────

/// A unique identifier for a node within a layout pass.
///
/// Assigned by depth-first traversal order during tree walking.
pub type NodeId = u64;

// ── DirtyFlags ──────────────────────────────────────────────────────────────

/// Bitflags indicating what kind of relayout a node needs.
pub struct DirtyFlags;

impl DirtyFlags {
    /// No relayout needed.
    pub const NONE: u8 = 0;
    /// This node's own style or content changed.
    pub const SELF_DIRTY: u8 = 1;
    /// At least one child's layout may have changed.
    pub const CHILDREN_DIRTY: u8 = 2;
    /// The entire subtree needs relayout.
    pub const SUBTREE_DIRTY: u8 = 4;
}

// ── DirtyTracker ────────────────────────────────────────────────────────────

/// Tracks per-node dirty flags for incremental relayout.
///
/// Nodes that are not present in the map are considered clean (NONE).
pub struct DirtyTracker {
    /// Dirty flags indexed by node id.
    flags: HashMap<NodeId, u8>,
}

impl DirtyTracker {
    /// Create a new, empty dirty tracker.
    pub fn new() -> Self {
        Self {
            flags: HashMap::new(),
        }
    }

    /// Mark a node as self-dirty (its own style/content changed).
    ///
    /// Also marks all ancestors as `CHILDREN_DIRTY` by convention — callers
    /// should use [`mark_ancestors_dirty`] for that.
    pub fn mark_dirty(&mut self, node_id: NodeId) {
        let entry = self.flags.entry(node_id).or_insert(DirtyFlags::NONE);
        *entry |= DirtyFlags::SELF_DIRTY;
        trace!(node_id, "marked SELF_DIRTY");
    }

    /// Mark a node as having dirty children (one or more children changed).
    pub fn mark_children_dirty(&mut self, node_id: NodeId) {
        let entry = self.flags.entry(node_id).or_insert(DirtyFlags::NONE);
        *entry |= DirtyFlags::CHILDREN_DIRTY;
        trace!(node_id, "marked CHILDREN_DIRTY");
    }

    /// Mark a node's entire subtree as dirty.
    pub fn mark_subtree_dirty(&mut self, node_id: NodeId) {
        let entry = self.flags.entry(node_id).or_insert(DirtyFlags::NONE);
        *entry |= DirtyFlags::SUBTREE_DIRTY;
        trace!(node_id, "marked SUBTREE_DIRTY");
    }

    /// Check if a node needs any kind of relayout.
    pub fn is_dirty(&self, node_id: NodeId) -> bool {
        self.flags
            .get(&node_id)
            .map(|&f| f != DirtyFlags::NONE)
            .unwrap_or(false)
    }

    /// Check if only children are dirty (node itself is clean).
    pub fn is_children_dirty_only(&self, node_id: NodeId) -> bool {
        self.flags
            .get(&node_id)
            .map(|&f| f == DirtyFlags::CHILDREN_DIRTY)
            .unwrap_or(false)
    }

    /// Get the raw flags for a node.
    pub fn get_flags(&self, node_id: NodeId) -> u8 {
        self.flags.get(&node_id).copied().unwrap_or(DirtyFlags::NONE)
    }

    /// Clear dirty flags for a node after relayout.
    pub fn clear(&mut self, node_id: NodeId) {
        self.flags.remove(&node_id);
    }

    /// Clear all dirty flags.
    pub fn clear_all(&mut self) {
        self.flags.clear();
    }

    /// Mark all tracked nodes as dirty (used for full relayout).
    pub fn mark_all_dirty(&mut self) {
        for flag in self.flags.values_mut() {
            *flag |= DirtyFlags::SUBTREE_DIRTY;
        }
    }
}

impl Default for DirtyTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ── CacheKey ────────────────────────────────────────────────────────────────

/// Cache key for layout results, based on the properties that affect layout.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CacheKey {
    /// Element tag name (e.g. "div", "p").
    pub tag: String,
    /// Element class attribute.
    pub class: String,
    /// Element id attribute.
    pub id: String,
    /// Serialized computed style string (from `data-nova-style`).
    pub style_str: String,
    /// Parent available width, quantized to integer pixels for cache stability.
    pub parent_width_px: i32,
}

impl CacheKey {
    /// Build a cache key from a DOM node and its parent's available width.
    pub fn from_node(node: &DomNode, parent_width: f32) -> Option<Self> {
        match node {
            DomNode::Element {
                tag, attributes, ..
            } => {
                let class = attributes
                    .iter()
                    .find(|(k, _)| k == "class")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let id = attributes
                    .iter()
                    .find(|(k, _)| k == "id")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let style_str = attributes
                    .iter()
                    .find(|(k, _)| k == "data-nova-style")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();

                Some(CacheKey {
                    tag: tag.clone(),
                    class,
                    id,
                    style_str,
                    parent_width_px: parent_width as i32,
                })
            }
            DomNode::Text(text) => {
                // Text nodes are keyed by their content and parent width.
                Some(CacheKey {
                    tag: "#text".into(),
                    class: String::new(),
                    id: String::new(),
                    style_str: text.clone(),
                    parent_width_px: parent_width as i32,
                })
            }
            _ => None,
        }
    }
}

// ── CachedLayout ────────────────────────────────────────────────────────────

/// Cached layout result for a node.
#[derive(Clone, Debug)]
pub struct CachedLayout {
    /// Computed width of the node.
    pub width: f32,
    /// Computed height of the node.
    pub height: f32,
    /// Full layout box subtree (including children layout).
    pub layout_box: LayoutBox,
}

// ── LayoutCache ─────────────────────────────────────────────────────────────

/// Hash-based cache for computed layout results.
///
/// Maps a [`CacheKey`] (derived from node properties and parent width) to
/// a [`CachedLayout`] containing the previously computed dimensions and
/// child positions.
pub struct LayoutCache {
    /// Cache entries indexed by key.
    entries: HashMap<CacheKey, CachedLayout>,
    /// Number of cache hits since creation or last reset.
    pub hit_count: u64,
    /// Number of cache misses since creation or last reset.
    pub miss_count: u64,
    /// Maximum number of entries before eviction.
    max_entries: usize,
}

impl LayoutCache {
    /// Create a new layout cache with the given maximum entry count.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            hit_count: 0,
            miss_count: 0,
            max_entries,
        }
    }

    /// Look up a cached layout by key.
    pub fn get(&mut self, key: &CacheKey) -> Option<&CachedLayout> {
        if self.entries.contains_key(key) {
            self.hit_count += 1;
            trace!(tag = %key.tag, "cache HIT");
            self.entries.get(key)
        } else {
            self.miss_count += 1;
            trace!(tag = %key.tag, "cache MISS");
            None
        }
    }

    /// Insert a layout result into the cache.
    ///
    /// If the cache is full, the oldest entries are evicted (simple clear strategy).
    pub fn insert(&mut self, key: CacheKey, layout: CachedLayout) {
        if self.entries.len() >= self.max_entries {
            // Simple eviction: clear half the cache.
            let to_keep = self.max_entries / 2;
            let keys: Vec<CacheKey> = self.entries.keys().skip(to_keep).cloned().collect();
            for k in keys {
                self.entries.remove(&k);
            }
            debug!(
                evicted = self.max_entries - to_keep,
                "layout cache eviction"
            );
        }
        self.entries.insert(key, layout);
    }

    /// Remove a specific entry from the cache.
    pub fn invalidate(&mut self, key: &CacheKey) {
        self.entries.remove(key);
    }

    /// Clear the entire cache.
    pub fn clear(&mut self) {
        self.entries.clear();
        debug!("layout cache cleared");
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Reset hit/miss counters.
    pub fn reset_stats(&mut self) {
        self.hit_count = 0;
        self.miss_count = 0;
    }

    /// Hit rate as a percentage (0.0 to 100.0).
    pub fn hit_rate(&self) -> f64 {
        let total = self.hit_count + self.miss_count;
        if total == 0 {
            0.0
        } else {
            (self.hit_count as f64 / total as f64) * 100.0
        }
    }
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self::new(4096)
    }
}

// ── IncrementalLayoutEngine ─────────────────────────────────────────────────

/// Engine that combines dirty tracking and caching for incremental layout.
///
/// On the first pass the entire tree is laid out and results are cached.
/// On subsequent passes only dirty subtrees are recomputed; clean subtrees
/// return their cached layout instantly.
pub struct IncrementalLayoutEngine {
    /// Layout result cache.
    pub cache: LayoutCache,
    /// Per-node dirty flags.
    pub dirty: DirtyTracker,
    /// Monotonically increasing node-id counter for the current pass.
    next_node_id: NodeId,
    /// Whether a full layout has been done (cache is populated).
    has_baseline: bool,
}

impl IncrementalLayoutEngine {
    /// Create a new incremental layout engine.
    pub fn new() -> Self {
        Self {
            cache: LayoutCache::default(),
            dirty: DirtyTracker::new(),
            next_node_id: 0,
            has_baseline: false,
        }
    }

    /// Create with a specific cache size.
    pub fn with_cache_size(max_entries: usize) -> Self {
        Self {
            cache: LayoutCache::new(max_entries),
            dirty: DirtyTracker::new(),
            next_node_id: 0,
            has_baseline: false,
        }
    }

    /// Reset the node-id counter for a new layout pass.
    fn reset_pass(&mut self) {
        self.next_node_id = 0;
    }

    /// Allocate a node id for the current pass.
    fn alloc_node_id(&mut self) -> NodeId {
        let id = self.next_node_id;
        self.next_node_id += 1;
        id
    }

    /// Invalidate a node and propagate dirty flags up to ancestors.
    ///
    /// `ancestor_ids` should be the list of ancestor node-ids from root to
    /// the parent of the invalidated node.
    pub fn invalidate_subtree(&mut self, node_id: NodeId, ancestor_ids: &[NodeId]) {
        self.dirty.mark_subtree_dirty(node_id);
        // Mark all ancestors as having dirty children.
        for &ancestor in ancestor_ids {
            self.dirty.mark_children_dirty(ancestor);
        }
        // Invalidate cache for this node (we don't have the key here, so
        // the dirty flag will cause a miss on the next pass).
    }

    /// Perform layout for a DOM node, using the cache when possible.
    ///
    /// `parent_width` is the available width from the parent container.
    /// `layout_fn` is the closure that performs actual layout computation
    /// when a cache miss occurs.
    ///
    /// Returns the layout box for the node.
    pub fn layout_node<F>(
        &mut self,
        node: &DomNode,
        parent_width: f32,
        layout_fn: F,
    ) -> LayoutBox
    where
        F: FnOnce(&DomNode, f32) -> LayoutBox,
    {
        let node_id = self.alloc_node_id();

        // On the first pass, always compute (no baseline yet).
        if !self.has_baseline {
            let result = layout_fn(node, parent_width);
            // Cache the result.
            if let Some(key) = CacheKey::from_node(node, parent_width) {
                self.cache.insert(
                    key,
                    CachedLayout {
                        width: result.width,
                        height: result.height,
                        layout_box: result.clone(),
                    },
                );
            }
            self.dirty.clear(node_id);
            return result;
        }

        // Subsequent passes: check dirty flags and cache.
        if !self.dirty.is_dirty(node_id) {
            // Node is clean — try cache.
            if let Some(key) = CacheKey::from_node(node, parent_width) {
                if let Some(cached) = self.cache.get(&key) {
                    return cached.layout_box.clone();
                }
            }
        }

        // Dirty or cache miss: recompute.
        let result = layout_fn(node, parent_width);
        if let Some(key) = CacheKey::from_node(node, parent_width) {
            self.cache.insert(
                key,
                CachedLayout {
                    width: result.width,
                    height: result.height,
                    layout_box: result.clone(),
                },
            );
        }
        self.dirty.clear(node_id);
        result
    }

    /// Mark the baseline as established (first full layout is done).
    pub fn set_baseline(&mut self) {
        self.has_baseline = true;
        self.dirty.clear_all();
        debug!(
            cache_size = self.cache.len(),
            "incremental layout baseline established"
        );
    }

    /// Check whether a baseline layout has been performed.
    pub fn has_baseline(&self) -> bool {
        self.has_baseline
    }

    /// Get cache statistics as a formatted string.
    pub fn stats(&self) -> String {
        format!(
            "cache: {} entries, {:.1}% hit rate ({} hits / {} misses)",
            self.cache.len(),
            self.cache.hit_rate(),
            self.cache.hit_count,
            self.cache.miss_count,
        )
    }
}

impl Default for IncrementalLayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::content::{LayoutBox, LayoutContent, StyleMap};

    fn make_layout_box(w: f32, h: f32) -> LayoutBox {
        LayoutBox {
            x: 0.0,
            y: 0.0,
            width: w,
            height: h,
            content: LayoutContent::Block,
            style: StyleMap::default(),
            children: vec![],
            z_index: 0,
        }
    }

    fn make_div(style: &str) -> DomNode {
        DomNode::Element {
            tag: "div".into(),
            attributes: vec![("data-nova-style".into(), style.into())],
            children: vec![DomNode::Text("Hello".into())],
        }
    }

    // ── DirtyFlags tests ────────────────────────────────────────────────────

    #[test]
    fn dirty_flags_constants() {
        assert_eq!(DirtyFlags::NONE, 0);
        assert_eq!(DirtyFlags::SELF_DIRTY, 1);
        assert_eq!(DirtyFlags::CHILDREN_DIRTY, 2);
        assert_eq!(DirtyFlags::SUBTREE_DIRTY, 4);
    }

    #[test]
    fn dirty_tracker_mark_and_check() {
        let mut tracker = DirtyTracker::new();
        assert!(!tracker.is_dirty(1));

        tracker.mark_dirty(1);
        assert!(tracker.is_dirty(1));
        assert!(!tracker.is_dirty(2));
    }

    #[test]
    fn dirty_tracker_children_dirty() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_children_dirty(10);
        assert!(tracker.is_dirty(10));
        assert!(tracker.is_children_dirty_only(10));
    }

    #[test]
    fn dirty_tracker_subtree_dirty() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_subtree_dirty(5);
        assert!(tracker.is_dirty(5));
        assert_eq!(tracker.get_flags(5), DirtyFlags::SUBTREE_DIRTY);
    }

    #[test]
    fn dirty_tracker_clear() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_dirty(1);
        assert!(tracker.is_dirty(1));
        tracker.clear(1);
        assert!(!tracker.is_dirty(1));
    }

    #[test]
    fn dirty_tracker_clear_all() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_dirty(1);
        tracker.mark_dirty(2);
        tracker.mark_dirty(3);
        tracker.clear_all();
        assert!(!tracker.is_dirty(1));
        assert!(!tracker.is_dirty(2));
        assert!(!tracker.is_dirty(3));
    }

    #[test]
    fn dirty_flag_propagation() {
        let mut tracker = DirtyTracker::new();
        // Simulate: node 5 changed, ancestors are [0, 1, 3].
        tracker.mark_dirty(5);
        tracker.mark_children_dirty(3);
        tracker.mark_children_dirty(1);
        tracker.mark_children_dirty(0);

        assert!(tracker.is_dirty(5));
        assert!(tracker.is_dirty(3));
        assert!(tracker.is_dirty(1));
        assert!(tracker.is_dirty(0));
        assert!(!tracker.is_dirty(2)); // unrelated node
    }

    // ── CacheKey tests ──────────────────────────────────────────────────────

    #[test]
    fn cache_key_from_element() {
        let node = DomNode::Element {
            tag: "div".into(),
            attributes: vec![
                ("class".into(), "container".into()),
                ("id".into(), "main".into()),
                ("data-nova-style".into(), "display: block".into()),
            ],
            children: vec![],
        };
        let key = CacheKey::from_node(&node, 800.0).unwrap();
        assert_eq!(key.tag, "div");
        assert_eq!(key.class, "container");
        assert_eq!(key.id, "main");
        assert_eq!(key.style_str, "display: block");
        assert_eq!(key.parent_width_px, 800);
    }

    #[test]
    fn cache_key_from_text() {
        let node = DomNode::Text("Hello world".into());
        let key = CacheKey::from_node(&node, 600.0).unwrap();
        assert_eq!(key.tag, "#text");
        assert_eq!(key.style_str, "Hello world");
    }

    #[test]
    fn cache_key_none_for_document() {
        let node = DomNode::Document { children: vec![] };
        assert!(CacheKey::from_node(&node, 800.0).is_none());
    }

    // ── LayoutCache tests ───────────────────────────────────────────────────

    #[test]
    fn cache_insert_and_get() {
        let mut cache = LayoutCache::new(100);
        let key = CacheKey {
            tag: "div".into(),
            class: String::new(),
            id: String::new(),
            style_str: "display: block".into(),
            parent_width_px: 800,
        };
        let layout = CachedLayout {
            width: 800.0,
            height: 100.0,
            layout_box: make_layout_box(800.0, 100.0),
        };
        cache.insert(key.clone(), layout);
        assert_eq!(cache.len(), 1);

        let hit = cache.get(&key);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().width, 800.0);
        assert_eq!(cache.hit_count, 1);
    }

    #[test]
    fn cache_miss_increments_counter() {
        let mut cache = LayoutCache::new(100);
        let key = CacheKey {
            tag: "span".into(),
            class: String::new(),
            id: String::new(),
            style_str: String::new(),
            parent_width_px: 800,
        };
        let result = cache.get(&key);
        assert!(result.is_none());
        assert_eq!(cache.miss_count, 1);
        assert_eq!(cache.hit_count, 0);
    }

    #[test]
    fn cache_eviction() {
        let mut cache = LayoutCache::new(4);
        for i in 0..6 {
            let key = CacheKey {
                tag: format!("tag{i}"),
                class: String::new(),
                id: String::new(),
                style_str: String::new(),
                parent_width_px: 800,
            };
            cache.insert(key, CachedLayout {
                width: 100.0,
                height: 50.0,
                layout_box: make_layout_box(100.0, 50.0),
            });
        }
        // After inserting 6 items into a size-4 cache, eviction should have happened.
        assert!(cache.len() <= 4);
    }

    #[test]
    fn cache_invalidate() {
        let mut cache = LayoutCache::new(100);
        let key = CacheKey {
            tag: "p".into(),
            class: String::new(),
            id: String::new(),
            style_str: String::new(),
            parent_width_px: 800,
        };
        cache.insert(key.clone(), CachedLayout {
            width: 200.0,
            height: 30.0,
            layout_box: make_layout_box(200.0, 30.0),
        });
        assert_eq!(cache.len(), 1);
        cache.invalidate(&key);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_hit_rate() {
        let mut cache = LayoutCache::new(100);
        let key = CacheKey {
            tag: "div".into(),
            class: String::new(),
            id: String::new(),
            style_str: String::new(),
            parent_width_px: 800,
        };
        cache.insert(key.clone(), CachedLayout {
            width: 100.0,
            height: 50.0,
            layout_box: make_layout_box(100.0, 50.0),
        });
        // 1 hit
        cache.get(&key);
        // 1 miss
        let miss_key = CacheKey {
            tag: "span".into(),
            class: String::new(),
            id: String::new(),
            style_str: String::new(),
            parent_width_px: 800,
        };
        cache.get(&miss_key);
        assert!((cache.hit_rate() - 50.0).abs() < 0.1);
    }

    // ── IncrementalLayoutEngine tests ───────────────────────────────────────

    #[test]
    fn engine_first_pass_computes_all() {
        let mut engine = IncrementalLayoutEngine::new();
        let node = make_div("display: block");
        let mut compute_count = 0u32;

        let result = engine.layout_node(&node, 800.0, |_n, _w| {
            compute_count += 1;
            make_layout_box(800.0, 100.0)
        });

        assert_eq!(compute_count, 1);
        assert_eq!(result.width, 800.0);
        assert_eq!(result.height, 100.0);
    }

    #[test]
    fn engine_cache_hit_after_baseline() {
        let mut engine = IncrementalLayoutEngine::new();
        let node = make_div("display: block; width: 400px");
        let mut compute_count = 0u32;

        // First pass: computes.
        engine.layout_node(&node, 800.0, |_n, _w| {
            compute_count += 1;
            make_layout_box(400.0, 100.0)
        });
        engine.set_baseline();

        // Second pass: should hit cache.
        engine.reset_pass();
        let result = engine.layout_node(&node, 800.0, |_n, _w| {
            compute_count += 1;
            make_layout_box(400.0, 100.0)
        });

        assert_eq!(compute_count, 1, "should not recompute on clean cache hit");
        assert_eq!(result.width, 400.0);
    }

    #[test]
    fn engine_dirty_node_recomputes() {
        let mut engine = IncrementalLayoutEngine::new();
        let node = make_div("display: block");

        // First pass.
        engine.layout_node(&node, 800.0, |_n, _w| {
            make_layout_box(800.0, 100.0)
        });
        engine.set_baseline();

        // Mark node 0 dirty.
        engine.dirty.mark_dirty(0);
        engine.reset_pass();

        let mut compute_count = 0u32;
        let result = engine.layout_node(&node, 800.0, |_n, _w| {
            compute_count += 1;
            make_layout_box(800.0, 200.0) // new height
        });

        assert_eq!(compute_count, 1, "dirty node should recompute");
        assert_eq!(result.height, 200.0);
    }

    #[test]
    fn engine_invalidate_subtree() {
        let mut engine = IncrementalLayoutEngine::new();
        engine.invalidate_subtree(5, &[0, 1, 3]);

        assert!(engine.dirty.is_dirty(5));
        assert!(engine.dirty.is_dirty(3));
        assert!(engine.dirty.is_dirty(1));
        assert!(engine.dirty.is_dirty(0));
    }

    #[test]
    fn engine_stats_output() {
        let engine = IncrementalLayoutEngine::new();
        let stats = engine.stats();
        assert!(stats.contains("cache:"));
        assert!(stats.contains("hit rate"));
    }

    #[test]
    fn incremental_update_preserves_cached() {
        let mut engine = IncrementalLayoutEngine::new();

        // Layout two nodes.
        let node_a = make_div("display: block; color: red");
        let node_b = make_div("display: block; color: blue");

        engine.layout_node(&node_a, 800.0, |_n, _w| make_layout_box(800.0, 50.0));
        engine.layout_node(&node_b, 800.0, |_n, _w| make_layout_box(800.0, 60.0));
        engine.set_baseline();

        // Only mark node 0 (node_a) as dirty.
        engine.dirty.mark_dirty(0);
        engine.reset_pass();

        let mut a_computed = false;
        let mut b_computed = false;

        engine.layout_node(&node_a, 800.0, |_n, _w| {
            a_computed = true;
            make_layout_box(800.0, 55.0)
        });
        engine.layout_node(&node_b, 800.0, |_n, _w| {
            b_computed = true;
            make_layout_box(800.0, 60.0)
        });

        assert!(a_computed, "dirty node_a should recompute");
        assert!(!b_computed, "clean node_b should use cache");
    }
}

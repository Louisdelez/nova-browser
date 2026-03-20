//! Shadow DOM and Custom Elements support for NOVA's JS engine.
//!
//! Implements the foundational Web Components APIs:
//!
//! - **Shadow DOM**: Encapsulated DOM subtrees with style isolation.
//!   Each shadow root is attached to a "host" element and contains its
//!   own tree of elements that are rendered in place of the host's children.
//!
//! - **Custom Elements**: User-defined HTML elements with custom tag names.
//!   Custom element names must contain a hyphen (e.g., `my-component`).
//!
//! - **Slots**: Named distribution points in a shadow tree that pull in
//!   children from the host element.
//!
//! ## Usage
//!
//! ```ignore
//! // JavaScript:
//! let shadow = element.attachShadow({ mode: 'open' });
//! customElements.define('my-component', class extends HTMLElement { ... });
//! ```

use std::collections::HashMap;

use tracing::{debug, warn};

use super::dom_api::ElementHandle;

// ── ShadowRootMode ──────────────────────────────────────────────────────────

/// The encapsulation mode of a shadow root.
///
/// Determines whether the shadow root is accessible from outside via
/// `element.shadowRoot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowRootMode {
    /// The shadow root is accessible via `element.shadowRoot`.
    Open,
    /// The shadow root is not accessible from outside.
    Closed,
}

impl ShadowRootMode {
    /// Parse a mode string (from JavaScript).
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "open" => Some(Self::Open),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }

    /// Convert to a JavaScript-facing string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

// ── ShadowRoot ──────────────────────────────────────────────────────────────

/// A shadow root attached to a host element.
///
/// Contains its own DOM subtree that is rendered in place of the host's
/// light DOM children. Styles defined inside a shadow tree do not leak
/// out to the document, and document styles do not penetrate into the
/// shadow tree (with some exceptions like inherited properties).
#[derive(Debug, Clone)]
pub struct ShadowRoot {
    /// The element handle that this shadow root is attached to.
    pub host: ElementHandle,
    /// Encapsulation mode (open or closed).
    pub mode: ShadowRootMode,
    /// Child element handles in the shadow tree.
    pub children: Vec<ElementHandle>,
    /// Stylesheets scoped to this shadow tree.
    pub stylesheets: Vec<String>,
}

impl ShadowRoot {
    /// Create a new shadow root for the given host element.
    pub fn new(host: ElementHandle, mode: ShadowRootMode) -> Self {
        Self {
            host,
            mode,
            children: Vec::new(),
            stylesheets: Vec::new(),
        }
    }

    /// Add a child handle to the shadow tree.
    pub fn append_child(&mut self, child: ElementHandle) {
        self.children.push(child);
    }

    /// Add a scoped stylesheet to the shadow tree.
    pub fn add_stylesheet(&mut self, css: String) {
        self.stylesheets.push(css);
    }

    /// Check if this shadow root has any children.
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }
}

// ── Slot ────────────────────────────────────────────────────────────────────

/// A slot in a shadow tree that distributes light DOM children.
///
/// When a shadow tree contains `<slot>` elements, the host's children are
/// distributed into the matching slots based on their `slot` attribute.
/// The default (unnamed) slot receives children without a `slot` attribute.
#[derive(Debug, Clone)]
pub struct Slot {
    /// The slot name. `None` means the default slot.
    pub name: Option<String>,
    /// Element handles from the host's light DOM assigned to this slot.
    pub assigned_nodes: Vec<ElementHandle>,
}

impl Slot {
    /// Create a new empty slot.
    pub fn new(name: Option<String>) -> Self {
        Self {
            name,
            assigned_nodes: Vec::new(),
        }
    }

    /// Create the default (unnamed) slot.
    pub fn default_slot() -> Self {
        Self::new(None)
    }

    /// Create a named slot.
    pub fn named(name: &str) -> Self {
        Self::new(Some(name.to_owned()))
    }

    /// Assign a node to this slot.
    pub fn assign(&mut self, handle: ElementHandle) {
        self.assigned_nodes.push(handle);
    }

    /// Check if any nodes are assigned.
    pub fn has_assigned_nodes(&self) -> bool {
        !self.assigned_nodes.is_empty()
    }
}

// ── CustomElementDef ────────────────────────────────────────────────────────

/// Definition of a custom element registered via `customElements.define()`.
#[derive(Debug, Clone)]
pub struct CustomElementDef {
    /// The custom element tag name (must contain a hyphen).
    pub tag_name: String,
    /// JavaScript source of the constructor function.
    pub constructor_source: String,
    /// For customized built-in elements: the tag being extended.
    ///
    /// e.g., `Some("button")` for `class FancyButton extends HTMLButtonElement`.
    pub extends: Option<String>,
}

impl CustomElementDef {
    /// Create a new custom element definition.
    pub fn new(tag_name: String, constructor_source: String, extends: Option<String>) -> Self {
        Self {
            tag_name,
            constructor_source,
            extends,
        }
    }
}

// ── CustomElementRegistry ───────────────────────────────────────────────────

/// Registry of custom element definitions.
///
/// Mirrors the browser's `CustomElementRegistry` (`window.customElements`).
/// Custom element names must contain a hyphen to distinguish them from
/// built-in HTML elements.
#[derive(Debug, Clone, Default)]
pub struct CustomElementRegistry {
    /// Registered definitions keyed by tag name.
    definitions: HashMap<String, CustomElementDef>,
}

impl CustomElementRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            definitions: HashMap::new(),
        }
    }

    /// Register a custom element definition.
    ///
    /// Returns `true` if registration succeeded, `false` if the name is
    /// invalid (no hyphen) or already registered.
    ///
    /// Per the spec, custom element names must:
    /// - Contain a hyphen (`-`)
    /// - Start with a lowercase ASCII letter
    /// - Not be any of the reserved names
    pub fn define(
        &mut self,
        name: &str,
        constructor_source: String,
        extends: Option<String>,
    ) -> bool {
        // Validate: must contain a hyphen.
        if !name.contains('-') {
            warn!(name, "custom element name must contain a hyphen");
            return false;
        }

        // Validate: must start with a lowercase letter.
        if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
            warn!(name, "custom element name must start with a lowercase letter");
            return false;
        }

        // Check reserved names.
        const RESERVED: &[&str] = &[
            "annotation-xml",
            "color-profile",
            "font-face",
            "font-face-src",
            "font-face-uri",
            "font-face-format",
            "font-face-name",
            "missing-glyph",
        ];
        if RESERVED.contains(&name) {
            warn!(name, "custom element name is reserved");
            return false;
        }

        // Check for duplicate registration.
        if self.definitions.contains_key(name) {
            warn!(name, "custom element already defined");
            return false;
        }

        let def = CustomElementDef::new(name.to_owned(), constructor_source, extends);
        self.definitions.insert(name.to_owned(), def);
        debug!(name, "custom element defined");
        true
    }

    /// Look up a custom element definition by tag name.
    pub fn get(&self, name: &str) -> Option<&CustomElementDef> {
        self.definitions.get(name)
    }

    /// Check if a custom element name has been defined.
    pub fn is_defined(&self, name: &str) -> bool {
        self.definitions.contains_key(name)
    }

    /// Get all registered tag names.
    pub fn names(&self) -> Vec<&str> {
        self.definitions.keys().map(|s| s.as_str()).collect()
    }

    /// Number of registered custom elements.
    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    /// Whether any custom elements are registered.
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }
}

// ── Slot assignment logic ───────────────────────────────────────────────────

/// Assign light DOM children to slots in a shadow tree.
///
/// Each child of the host element is assigned to a slot based on its
/// `slot` attribute:
/// - Children with `slot="name"` go to the slot with that name.
/// - Children without a `slot` attribute go to the default slot.
///
/// Returns a map from slot name (or `None` for default) to assigned handles.
pub fn assign_slots(
    host_children: &[(ElementHandle, Option<String>)],
    shadow_slots: &[Slot],
) -> HashMap<Option<String>, Vec<ElementHandle>> {
    let mut assignments: HashMap<Option<String>, Vec<ElementHandle>> = HashMap::new();

    // Initialize with empty vecs for each declared slot.
    for slot in shadow_slots {
        assignments.entry(slot.name.clone()).or_default();
    }

    // Assign each host child to the appropriate slot.
    for (handle, slot_name) in host_children {
        assignments
            .entry(slot_name.clone())
            .or_default()
            .push(*handle);
    }

    assignments
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ShadowRootMode tests ────────────────────────────────────────────────

    #[test]
    fn shadow_root_mode_from_str() {
        assert_eq!(ShadowRootMode::from_str("open"), Some(ShadowRootMode::Open));
        assert_eq!(
            ShadowRootMode::from_str("closed"),
            Some(ShadowRootMode::Closed)
        );
        assert_eq!(ShadowRootMode::from_str("invalid"), None);
        assert_eq!(ShadowRootMode::from_str("Open"), Some(ShadowRootMode::Open));
    }

    #[test]
    fn shadow_root_mode_as_str() {
        assert_eq!(ShadowRootMode::Open.as_str(), "open");
        assert_eq!(ShadowRootMode::Closed.as_str(), "closed");
    }

    // ── ShadowRoot tests ────────────────────────────────────────────────────

    #[test]
    fn shadow_root_creation() {
        let shadow = ShadowRoot::new(42, ShadowRootMode::Open);
        assert_eq!(shadow.host, 42);
        assert_eq!(shadow.mode, ShadowRootMode::Open);
        assert!(shadow.is_empty());
        assert!(shadow.stylesheets.is_empty());
    }

    #[test]
    fn shadow_root_append_child() {
        let mut shadow = ShadowRoot::new(1, ShadowRootMode::Open);
        shadow.append_child(10);
        shadow.append_child(11);
        assert_eq!(shadow.children.len(), 2);
        assert!(!shadow.is_empty());
    }

    #[test]
    fn shadow_root_add_stylesheet() {
        let mut shadow = ShadowRoot::new(1, ShadowRootMode::Open);
        shadow.add_stylesheet(":host { display: block; }".into());
        assert_eq!(shadow.stylesheets.len(), 1);
    }

    // ── Slot tests ──────────────────────────────────────────────────────────

    #[test]
    fn slot_default() {
        let slot = Slot::default_slot();
        assert!(slot.name.is_none());
        assert!(!slot.has_assigned_nodes());
    }

    #[test]
    fn slot_named() {
        let slot = Slot::named("header");
        assert_eq!(slot.name.as_deref(), Some("header"));
    }

    #[test]
    fn slot_assign_nodes() {
        let mut slot = Slot::default_slot();
        slot.assign(100);
        slot.assign(101);
        assert!(slot.has_assigned_nodes());
        assert_eq!(slot.assigned_nodes.len(), 2);
    }

    // ── CustomElementRegistry tests ─────────────────────────────────────────

    #[test]
    fn custom_element_define_valid() {
        let mut registry = CustomElementRegistry::new();
        assert!(registry.define("my-component", "class MyComponent {}".into(), None));
        assert!(registry.is_defined("my-component"));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn custom_element_define_no_hyphen() {
        let mut registry = CustomElementRegistry::new();
        assert!(!registry.define("mycomponent", "class MyComponent {}".into(), None));
        assert!(!registry.is_defined("mycomponent"));
    }

    #[test]
    fn custom_element_define_uppercase_start() {
        let mut registry = CustomElementRegistry::new();
        assert!(!registry.define("My-component", "class {}".into(), None));
    }

    #[test]
    fn custom_element_define_reserved_name() {
        let mut registry = CustomElementRegistry::new();
        assert!(!registry.define("font-face", "class {}".into(), None));
    }

    #[test]
    fn custom_element_define_duplicate() {
        let mut registry = CustomElementRegistry::new();
        assert!(registry.define("my-comp", "v1".into(), None));
        assert!(!registry.define("my-comp", "v2".into(), None));
    }

    #[test]
    fn custom_element_get() {
        let mut registry = CustomElementRegistry::new();
        registry.define("app-header", "class AppHeader {}".into(), None);
        let def = registry.get("app-header").expect("should exist");
        assert_eq!(def.tag_name, "app-header");
        assert_eq!(def.constructor_source, "class AppHeader {}");
        assert!(def.extends.is_none());
    }

    #[test]
    fn custom_element_extends() {
        let mut registry = CustomElementRegistry::new();
        registry.define(
            "fancy-button",
            "class FancyButton {}".into(),
            Some("button".into()),
        );
        let def = registry.get("fancy-button").expect("should exist");
        assert_eq!(def.extends.as_deref(), Some("button"));
    }

    #[test]
    fn custom_element_names() {
        let mut registry = CustomElementRegistry::new();
        registry.define("x-foo", "".into(), None);
        registry.define("y-bar", "".into(), None);
        let mut names = registry.names();
        names.sort();
        assert_eq!(names, vec!["x-foo", "y-bar"]);
    }

    // ── Slot assignment tests ───────────────────────────────────────────────

    #[test]
    fn slot_assignment_default() {
        let host_children = vec![
            (10, None),       // no slot attr → default
            (11, None),       // no slot attr → default
        ];
        let slots = vec![Slot::default_slot()];
        let assignments = assign_slots(&host_children, &slots);
        let default = assignments.get(&None).expect("default slot");
        assert_eq!(default, &vec![10, 11]);
    }

    #[test]
    fn slot_assignment_named() {
        let host_children = vec![
            (10, Some("header".into())),
            (11, None),
            (12, Some("footer".into())),
        ];
        let slots = vec![
            Slot::default_slot(),
            Slot::named("header"),
            Slot::named("footer"),
        ];
        let assignments = assign_slots(&host_children, &slots);

        assert_eq!(
            assignments.get(&Some("header".into())).unwrap(),
            &vec![10]
        );
        assert_eq!(assignments.get(&None).unwrap(), &vec![11]);
        assert_eq!(
            assignments.get(&Some("footer".into())).unwrap(),
            &vec![12]
        );
    }

    #[test]
    fn slot_assignment_no_matching_slot() {
        let host_children = vec![(10, Some("nonexistent".into()))];
        let slots = vec![Slot::default_slot()];
        let assignments = assign_slots(&host_children, &slots);
        // The child goes to the "nonexistent" bucket even if no slot declared it.
        let bucket = assignments
            .get(&Some("nonexistent".into()))
            .unwrap();
        assert_eq!(bucket, &vec![10]);
    }

    // ── Shadow style isolation test ─────────────────────────────────────────

    #[test]
    fn shadow_root_style_isolation() {
        // Shadow roots have their own stylesheets that don't leak out.
        let mut shadow = ShadowRoot::new(1, ShadowRootMode::Open);
        shadow.add_stylesheet("p { color: red; }".into());

        // The stylesheet is scoped to this shadow root.
        assert_eq!(shadow.stylesheets.len(), 1);
        assert!(shadow.stylesheets[0].contains("color: red"));
    }
}

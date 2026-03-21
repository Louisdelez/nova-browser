//! # dom_api
//!
//! Pure-Rust DOM bridge for the NOVA JavaScript engine.
//!
//! ## Design
//!
//! Because QuickJS is not yet wired through the workspace, we implement a
//! lightweight *script interpreter* that understands a well-defined subset of
//! the Web DOM API surface.  Each element in the DOM tree is assigned a
//! numeric **handle** (u64).  JavaScript code manipulates elements by calling
//! the helper functions below; those functions look up the handle in a shared
//! [`JsDomTree`] and apply the requested mutation.
//!
//! ### Supported API surface
//!
//! | JS expression                              | Rust entry point                    |
//! |--------------------------------------------|-------------------------------------|
//! | `document.getElementById(id)`              | [`JsDomTree::get_element_by_id`]    |
//! | `document.querySelector(sel)`              | [`JsDomTree::query_selector`]       |
//! | `document.createElement(tag)`             | [`JsDomTree::create_element`]       |
//! | `el.textContent = "…"`                    | [`JsDomTree::set_text_content`]     |
//! | `el.textContent`                          | [`JsDomTree::get_text_content`]     |
//! | `el.innerHTML = "…"`                      | [`JsDomTree::set_inner_html`]       |
//! | `el.innerHTML`                            | [`JsDomTree::get_inner_html`]       |
//! | `el.appendChild(child)`                   | [`JsDomTree::append_child`]         |
//! | `el.setAttribute(name, value)`            | [`JsDomTree::set_attribute`]        |
//! | `el.getAttribute(name)`                   | [`JsDomTree::get_attribute`]        |
//! | `el.classList.add(cls)`                   | [`JsDomTree::class_list_add`]       |
//! | `el.classList.remove(cls)`                | [`JsDomTree::class_list_remove`]    |
//! | `el.style.setProperty(name, value)`       | [`JsDomTree::style_set_property`]   |
//! | `el.addEventListener(type, cb)`           | [`JsDomTree::add_event_listener`]   |
//! | `dispatchEvent(handle, type)`             | [`JsDomTree::dispatch_event`]       |

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::{debug, warn};

use nova_mod_api::content::{DomNode, JsValue};
use nova_mod_api::CoreApi;

use crate::canvas::CanvasContext2D;
use crate::shadow_dom::{self, CustomElementRegistry, Slot, ShadowRoot, ShadowRootMode};

// ── CSS Selector Engine ───────────────────────────────────────────────────────

/// A parsed CSS selector, supporting complex combinators.
#[derive(Debug, Clone)]
enum SelectorPart {
    /// A simple selector (tag, #id, .class, [attr], :pseudo).
    Simple(SimpleSelector),
    /// Descendant combinator: `A B` (B is descendant of A).
    Descendant,
    /// Child combinator: `A > B` (B is direct child of A).
    Child,
    /// Adjacent sibling: `A + B` (B immediately follows A).
    Adjacent,
    /// General sibling: `A ~ B` (B follows A, same parent).
    General,
}

/// A single simple selector with multiple conditions that must all match.
#[derive(Debug, Clone, Default)]
struct SimpleSelector {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSelector>,
    pseudos: Vec<PseudoSelector>,
    not: Option<Box<SimpleSelector>>,
}

/// Attribute selector with operator.
#[derive(Debug, Clone)]
struct AttrSelector {
    name: String,
    op: AttrOp,
    value: Option<String>,
}

/// Attribute selector operator.
#[derive(Debug, Clone)]
enum AttrOp {
    /// `[name]` - attribute exists.
    Exists,
    /// `[name="value"]` - exact match.
    Equals,
    /// `[name^="value"]` - starts with.
    StartsWith,
    /// `[name$="value"]` - ends with.
    EndsWith,
    /// `[name*="value"]` - contains.
    Contains,
}

/// Pseudo-class selector.
#[derive(Debug, Clone)]
enum PseudoSelector {
    FirstChild,
    LastChild,
    NthChild(i32),
}

/// A complete parsed selector (a sequence of simple selectors and combinators).
#[derive(Debug, Clone)]
struct Selector {
    parts: Vec<SelectorPart>,
}

/// Parse a comma-separated selector list.
fn parse_selector_list(input: &str) -> Vec<Selector> {
    input.split(',')
        .map(|s| parse_single_selector(s.trim()))
        .collect()
}

/// Parse a single selector (no commas).
fn parse_single_selector(input: &str) -> Selector {
    let input = input.trim();
    let mut parts: Vec<SelectorPart> = Vec::new();
    let mut chars = input.chars().peekable();
    let mut current = String::new();

    while chars.peek().is_some() {
        // Skip whitespace and detect combinators.
        let mut had_space = false;
        while chars.peek() == Some(&' ') {
            chars.next();
            had_space = true;
        }

        match chars.peek() {
            Some(&'>') => {
                if !current.is_empty() {
                    parts.push(SelectorPart::Simple(parse_simple_selector(&current)));
                    current.clear();
                }
                parts.push(SelectorPart::Child);
                chars.next();
                // skip trailing space
                while chars.peek() == Some(&' ') { chars.next(); }
                continue;
            }
            Some(&'+') => {
                if !current.is_empty() {
                    parts.push(SelectorPart::Simple(parse_simple_selector(&current)));
                    current.clear();
                }
                parts.push(SelectorPart::Adjacent);
                chars.next();
                while chars.peek() == Some(&' ') { chars.next(); }
                continue;
            }
            Some(&'~') => {
                if !current.is_empty() {
                    parts.push(SelectorPart::Simple(parse_simple_selector(&current)));
                    current.clear();
                }
                parts.push(SelectorPart::General);
                chars.next();
                while chars.peek() == Some(&' ') { chars.next(); }
                continue;
            }
            _ => {
                if had_space && !current.is_empty() {
                    parts.push(SelectorPart::Simple(parse_simple_selector(&current)));
                    current.clear();
                    parts.push(SelectorPart::Descendant);
                }
            }
        }

        if let Some(&c) = chars.peek() {
            if c == '[' {
                // Consume until matching ']'.
                current.push(c);
                chars.next();
                while let Some(&inner) = chars.peek() {
                    current.push(inner);
                    chars.next();
                    if inner == ']' { break; }
                }
            } else if c == ':' {
                // Consume pseudo-class including parenthesized argument.
                current.push(c);
                chars.next();
                // Consume the name.
                while let Some(&pc) = chars.peek() {
                    if pc == '(' {
                        current.push(pc);
                        chars.next();
                        let mut depth = 1;
                        while let Some(&inner) = chars.peek() {
                            current.push(inner);
                            chars.next();
                            if inner == '(' { depth += 1; }
                            if inner == ')' { depth -= 1; if depth == 0 { break; } }
                        }
                        break;
                    } else if pc.is_alphanumeric() || pc == '-' || pc == '_' {
                        current.push(pc);
                        chars.next();
                    } else {
                        break;
                    }
                }
            } else {
                current.push(c);
                chars.next();
            }
        }
    }

    if !current.is_empty() {
        parts.push(SelectorPart::Simple(parse_simple_selector(&current)));
    }

    Selector { parts }
}

/// Parse a simple selector string like `div.foo#bar[attr="val"]:first-child`.
fn parse_simple_selector(input: &str) -> SimpleSelector {
    let mut sel = SimpleSelector::default();
    let mut chars = input.chars().peekable();
    let mut buf = String::new();
    let mut mode = 't'; // t=tag, i=id, c=class

    let flush = |mode: char, buf: &mut String, sel: &mut SimpleSelector| {
        if buf.is_empty() { return; }
        match mode {
            't' => sel.tag = Some(buf.drain(..).collect::<String>().to_lowercase()),
            'i' => sel.id = Some(buf.drain(..).collect()),
            'c' => sel.classes.push(buf.drain(..).collect()),
            _ => { buf.clear(); }
        }
    };

    while let Some(&c) = chars.peek() {
        match c {
            '#' => {
                flush(mode, &mut buf, &mut sel);
                mode = 'i';
                chars.next();
            }
            '.' => {
                flush(mode, &mut buf, &mut sel);
                mode = 'c';
                chars.next();
            }
            '[' => {
                flush(mode, &mut buf, &mut sel);
                chars.next();
                // Parse attribute selector.
                let mut attr_str = String::new();
                while let Some(&ac) = chars.peek() {
                    if ac == ']' { chars.next(); break; }
                    attr_str.push(ac);
                    chars.next();
                }
                sel.attrs.push(parse_attr_selector(&attr_str));
                mode = 't';
            }
            ':' => {
                flush(mode, &mut buf, &mut sel);
                chars.next();
                // Read pseudo name.
                let mut pseudo_name = String::new();
                while let Some(&pc) = chars.peek() {
                    if pc == '(' || !pc.is_alphanumeric() && pc != '-' && pc != '_' {
                        break;
                    }
                    pseudo_name.push(pc);
                    chars.next();
                }
                // Check for parenthesized argument.
                let mut arg = String::new();
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1;
                    while let Some(&ac) = chars.peek() {
                        chars.next();
                        if ac == '(' { depth += 1; }
                        if ac == ')' { depth -= 1; if depth == 0 { break; } }
                        arg.push(ac);
                    }
                }
                match pseudo_name.as_str() {
                    "first-child" => sel.pseudos.push(PseudoSelector::FirstChild),
                    "last-child" => sel.pseudos.push(PseudoSelector::LastChild),
                    "nth-child" => {
                        if let Ok(n) = arg.trim().parse::<i32>() {
                            sel.pseudos.push(PseudoSelector::NthChild(n));
                        }
                    }
                    "not" => {
                        let inner = parse_simple_selector(arg.trim());
                        sel.not = Some(Box::new(inner));
                    }
                    _ => {} // Unknown pseudo -- ignore.
                }
                mode = 't';
            }
            _ => {
                buf.push(c);
                chars.next();
            }
        }
    }
    flush(mode, &mut buf, &mut sel);
    sel
}

/// Parse an attribute selector like `name="value"` or `name^="val"`.
fn parse_attr_selector(s: &str) -> AttrSelector {
    let s = s.trim();
    // Try operators: ^=, $=, *=, =
    for (op_str, op) in &[("^=", AttrOp::StartsWith), ("$=", AttrOp::EndsWith), ("*=", AttrOp::Contains), ("=", AttrOp::Equals)] {
        if let Some(pos) = s.find(op_str) {
            let name = s[..pos].trim().to_string();
            let val_raw = s[pos + op_str.len()..].trim();
            let value = val_raw.trim_matches('"').trim_matches('\'').to_string();
            return AttrSelector { name, op: op.clone(), value: Some(value) };
        }
    }
    AttrSelector { name: s.to_string(), op: AttrOp::Exists, value: None }
}

// ── Handle allocation ────────────────────────────────────────────────────────

/// A numeric handle that uniquely identifies a DOM node inside a [`JsDomTree`].
pub type ElementHandle = u64;

// ── Internal node representation ─────────────────────────────────────────────

/// An element node stored inside the [`JsDomTree`].
#[derive(Debug, Clone)]
pub struct JsElement {
    pub handle: ElementHandle,
    pub tag: String,
    pub attributes: Vec<(String, String)>,
    /// Ordered list of child handles (both element and text).
    pub children: Vec<ElementHandle>,
    /// Text content (for text-only nodes; tag == "#text").
    pub text: Option<String>,
    /// Inline style properties set via `el.style.setProperty`.
    pub inline_styles: Vec<(String, String)>,
}

impl JsElement {
    fn new_element(handle: ElementHandle, tag: &str) -> Self {
        Self {
            handle,
            tag: tag.to_lowercase(),
            attributes: Vec::new(),
            children: Vec::new(),
            text: None,
            inline_styles: Vec::new(),
        }
    }

    fn new_text(handle: ElementHandle, content: &str) -> Self {
        Self {
            handle,
            tag: "#text".into(),
            attributes: Vec::new(),
            children: Vec::new(),
            text: Some(content.to_owned()),
            inline_styles: Vec::new(),
        }
    }

    /// Get the `class` attribute value.
    fn class_attr(&self) -> String {
        self.attributes
            .iter()
            .find(|(k, _)| k == "class")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// Set the `class` attribute value.
    fn set_class_attr(&mut self, value: String) {
        if let Some(pos) = self.attributes.iter().position(|(k, _)| k == "class") {
            self.attributes[pos].1 = value;
        } else {
            self.attributes.push(("class".into(), value));
        }
    }
}

// ── Event listener store ─────────────────────────────────────────────────────

/// A stored JavaScript callback (stored as source text for the interpreter).
#[derive(Debug, Clone)]
pub struct EventCallback {
    /// JavaScript source of the callback function body.
    pub source: String,
    /// The event type this listener is registered for.
    pub event_type: String,
    /// The element handle this listener is attached to.
    pub element_handle: ElementHandle,
    /// Variable-to-handle bindings captured from the enclosing scope at the
    /// time `addEventListener` was called.  When the callback runs these are
    /// pre-seeded into the interpreter env so the callback body can reference
    /// the same variables (e.g. `btn`) without re-resolving them.
    pub captured_env: Vec<(String, ElementHandle)>,
    /// Whether this listener was registered for the capture phase.
    pub capture: bool,
}

// ── Main DOM tree ─────────────────────────────────────────────────────────────

/// The shared DOM tree that lives inside a JS execution context.
///
/// Created once per page load and passed (via `Arc<Mutex<…>>`) into every
/// script that runs in that page's context.
pub struct JsDomTree {
    /// All nodes indexed by handle.
    pub(crate) nodes: HashMap<ElementHandle, JsElement>,
    /// The root document handle (always 0).
    root: ElementHandle,
    /// Next handle to allocate.
    next_handle: ElementHandle,
    /// Event listeners: (handle, event_type) → list of callbacks.
    listeners: HashMap<(ElementHandle, String), Vec<EventCallback>>,
    /// Log of pending callbacks that were triggered (for the interpreter loop).
    pub pending_events: Vec<(ElementHandle, String)>,
    /// Shadow roots attached to host elements.
    pub shadow_roots: HashMap<ElementHandle, ShadowRoot>,
    /// Custom element registry (equivalent to `window.customElements`).
    pub custom_elements: CustomElementRegistry,
    /// Canvas 2D rendering contexts keyed by the `<canvas>` element handle.
    pub canvas_contexts: HashMap<ElementHandle, CanvasContext2D>,
    /// Form element values (input, textarea, select) keyed by handle.
    values: HashMap<ElementHandle, String>,
    /// Checkbox/radio checked state keyed by handle.
    checked: HashMap<ElementHandle, bool>,
}

impl JsDomTree {
    /// Construct a `JsDomTree` from a [`DomNode`] tree.
    pub fn from_dom(root: &DomNode) -> Arc<Mutex<Self>> {
        let mut tree = Self {
            nodes: HashMap::new(),
            root: 0,
            next_handle: 1,
            listeners: HashMap::new(),
            pending_events: Vec::new(),
            shadow_roots: HashMap::new(),
            custom_elements: CustomElementRegistry::new(),
            canvas_contexts: HashMap::new(),
            values: HashMap::new(),
            checked: HashMap::new(),
        };

        // Reserve handle 0 for the document root.
        let root_handle = 0;
        let root_elem = JsElement {
            handle: root_handle,
            tag: "#document".into(),
            attributes: Vec::new(),
            children: Vec::new(),
            text: None,
            inline_styles: Vec::new(),
        };
        tree.nodes.insert(root_handle, root_elem);
        tree.import_children(root, root_handle);

        Arc::new(Mutex::new(tree))
    }

    /// Allocate a new handle.
    fn alloc_handle(&mut self) -> ElementHandle {
        let h = self.next_handle;
        self.next_handle += 1;
        h
    }

    /// Recursively import a [`DomNode`] subtree and return its handle.
    fn import_node(&mut self, node: &DomNode) -> ElementHandle {
        match node {
            DomNode::Document { children } => {
                // The document root is always handle 0; just import its children.
                self.import_children_list(children, 0);
                0
            }
            DomNode::Element {
                tag,
                attributes,
                children,
            } => {
                let handle = self.alloc_handle();
                let mut elem = JsElement::new_element(handle, tag);
                elem.attributes = attributes.clone();
                self.nodes.insert(handle, elem);
                self.import_children_list(children, handle);
                handle
            }
            DomNode::Text(text) => {
                let handle = self.alloc_handle();
                let elem = JsElement::new_text(handle, text);
                self.nodes.insert(handle, elem);
                handle
            }
            DomNode::Comment(_) => {
                // Comments don't need a handle; skip them.
                let handle = self.alloc_handle();
                let elem = JsElement {
                    handle,
                    tag: "#comment".into(),
                    attributes: Vec::new(),
                    children: Vec::new(),
                    text: None,
                    inline_styles: Vec::new(),
                };
                self.nodes.insert(handle, elem);
                handle
            }
        }
    }

    /// Import children of a DomNode into an already-created parent element.
    fn import_children(&mut self, node: &DomNode, parent_handle: ElementHandle) {
        match node {
            DomNode::Document { children } | DomNode::Element { children, .. } => {
                self.import_children_list(children, parent_handle);
            }
            _ => {}
        }
    }

    fn import_children_list(&mut self, children: &[DomNode], parent_handle: ElementHandle) {
        let child_handles: Vec<ElementHandle> = children
            .iter()
            .map(|c| self.import_node(c))
            .collect();
        if let Some(parent) = self.nodes.get_mut(&parent_handle) {
            parent.children.extend(child_handles);
        }
    }

    /// Export the full tree back to a [`DomNode`].
    pub fn to_dom(&self) -> DomNode {
        self.export_node(self.root)
    }

    fn export_node(&self, handle: ElementHandle) -> DomNode {
        let Some(elem) = self.nodes.get(&handle) else {
            return DomNode::Text(String::new());
        };

        match elem.tag.as_str() {
            "#document" => {
                let children = elem
                    .children
                    .iter()
                    .map(|&h| self.export_node(h))
                    .collect();
                DomNode::Document { children }
            }
            "#text" => DomNode::Text(elem.text.clone().unwrap_or_default()),
            "#comment" => DomNode::Comment(elem.text.clone().unwrap_or_default()),
            tag => {
                // Merge inline styles into a `style` attribute.
                let mut attributes = elem.attributes.clone();
                if !elem.inline_styles.is_empty() {
                    let style_str: String = elem
                        .inline_styles
                        .iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect::<Vec<_>>()
                        .join(";");

                    // Replace existing style attribute or append.
                    if let Some(pos) = attributes.iter().position(|(k, _)| k == "style") {
                        attributes[pos].1 = style_str;
                    } else {
                        attributes.push(("style".into(), style_str));
                    }
                }

                // ── Shadow DOM export ──────────────────────────────────
                // If this element has a shadow root, export the shadow tree
                // children (with slot distribution) instead of light DOM
                // children. The shadow root's stylesheets are serialized as
                // a `data-nova-shadow-styles` attribute so the cascade can
                // scope them correctly.
                if let Some(shadow) = self.shadow_roots.get(&handle) {
                    // Mark element as shadow host.
                    attributes.push(("data-nova-shadow-host".into(), "true".into()));

                    // Serialize shadow stylesheets.
                    if !shadow.stylesheets.is_empty() {
                        let combined = shadow.stylesheets.join("\n");
                        attributes.push(("data-nova-shadow-styles".into(), combined));
                    }

                    // Build the shadow tree children with slot distribution.
                    let children = self.export_shadow_children(handle, &shadow.children.clone());
                    DomNode::Element {
                        tag: tag.to_owned(),
                        attributes,
                        children,
                    }
                } else {
                    let children = elem
                        .children
                        .iter()
                        .map(|&h| self.export_node(h))
                        .collect();
                    DomNode::Element {
                        tag: tag.to_owned(),
                        attributes,
                        children,
                    }
                }
            }
        }
    }

    // ── Shadow DOM export helpers ──────────────────────────────────────────────

    /// Export shadow tree children, distributing light DOM children into slots.
    ///
    /// Walks the shadow tree children; when a `<slot>` element is encountered,
    /// it is replaced by the matching light DOM children from the host element.
    fn export_shadow_children(
        &self,
        host_handle: ElementHandle,
        shadow_child_handles: &[ElementHandle],
    ) -> Vec<DomNode> {
        // Gather host's light DOM children with their `slot` attribute values.
        let host_children: Vec<(ElementHandle, Option<String>)> = self
            .nodes
            .get(&host_handle)
            .map(|e| {
                e.children
                    .iter()
                    .map(|&h| {
                        let slot_attr = self
                            .nodes
                            .get(&h)
                            .and_then(|child_elem| {
                                child_elem
                                    .attributes
                                    .iter()
                                    .find(|(k, _)| k == "slot")
                                    .map(|(_, v)| v.clone())
                            });
                        (h, slot_attr)
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Collect slot definitions from the shadow tree.
        let slots = self.collect_slots(shadow_child_handles);

        // Assign light DOM children to slots.
        let assignments = shadow_dom::assign_slots(&host_children, &slots);

        // Now export shadow children, replacing <slot> elements with assigned nodes.
        shadow_child_handles
            .iter()
            .flat_map(|&h| self.export_shadow_node(h, &assignments))
            .collect()
    }

    /// Collect all `<slot>` elements from the shadow tree children.
    fn collect_slots(&self, handles: &[ElementHandle]) -> Vec<Slot> {
        let mut slots = Vec::new();
        for &h in handles {
            self.collect_slots_recursive(h, &mut slots);
        }
        slots
    }

    /// Recursively find `<slot>` elements in the shadow tree.
    fn collect_slots_recursive(&self, handle: ElementHandle, slots: &mut Vec<Slot>) {
        let Some(elem) = self.nodes.get(&handle) else { return };
        if elem.tag == "slot" {
            let name = elem
                .attributes
                .iter()
                .find(|(k, _)| k == "name")
                .map(|(_, v)| v.clone());
            slots.push(Slot::new(name));
        }
        for &child in &elem.children {
            self.collect_slots_recursive(child, slots);
        }
    }

    /// Export a single shadow node, replacing `<slot>` elements with their
    /// assigned light DOM children.
    fn export_shadow_node(
        &self,
        handle: ElementHandle,
        assignments: &std::collections::HashMap<Option<String>, Vec<ElementHandle>>,
    ) -> Vec<DomNode> {
        let Some(elem) = self.nodes.get(&handle) else {
            return vec![DomNode::Text(String::new())];
        };

        if elem.tag == "slot" {
            // Replace the slot with its assigned light DOM children.
            let slot_name = elem
                .attributes
                .iter()
                .find(|(k, _)| k == "name")
                .map(|(_, v)| v.clone());
            if let Some(assigned) = assignments.get(&slot_name) {
                if !assigned.is_empty() {
                    return assigned.iter().map(|&h| self.export_node(h)).collect();
                }
            }
            // No assigned nodes — render the slot's fallback content (its children).
            return elem.children.iter().map(|&h| self.export_node(h)).collect();
        }

        // For non-slot elements, recursively export but continue checking for
        // nested slots.
        match elem.tag.as_str() {
            "#text" => vec![DomNode::Text(elem.text.clone().unwrap_or_default())],
            "#comment" => vec![DomNode::Comment(elem.text.clone().unwrap_or_default())],
            tag => {
                let mut attributes = elem.attributes.clone();
                if !elem.inline_styles.is_empty() {
                    let style_str: String = elem
                        .inline_styles
                        .iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect::<Vec<_>>()
                        .join(";");
                    if let Some(pos) = attributes.iter().position(|(k, _)| k == "style") {
                        attributes[pos].1 = style_str;
                    } else {
                        attributes.push(("style".into(), style_str));
                    }
                }
                let children: Vec<DomNode> = elem
                    .children
                    .iter()
                    .flat_map(|&h| self.export_shadow_node(h, assignments))
                    .collect();
                vec![DomNode::Element {
                    tag: tag.to_owned(),
                    attributes,
                    children,
                }]
            }
        }
    }

    // ── DOM query API ──────────────────────────────────────────────────────────

    /// `document.getElementById(id)` — returns `None` if not found.
    pub fn get_element_by_id(&self, id: &str) -> Option<ElementHandle> {
        self.find_by_attr("id", id, self.root)
    }

    /// `document.querySelector(selector)` — supports complex CSS selectors
    /// including combinators, attribute selectors, and pseudo-classes.
    pub fn query_selector(&self, selector: &str) -> Option<ElementHandle> {
        let selectors = parse_selector_list(selector.trim());
        for sel in &selectors {
            if let Some(h) = self.find_first_matching(sel, self.root) {
                return Some(h);
            }
        }
        None
    }

    /// Recursive depth-first search by attribute.
    fn find_by_attr(&self, attr: &str, value: &str, handle: ElementHandle) -> Option<ElementHandle> {
        let elem = self.nodes.get(&handle)?;
        if elem.attributes.iter().any(|(k, v)| k == attr && v == value) {
            return Some(handle);
        }
        for &child in &elem.children {
            if let Some(found) = self.find_by_attr(attr, value, child) {
                return Some(found);
            }
        }
        None
    }

    /// Recursive depth-first search by class membership.
    #[allow(dead_code)]
    fn find_by_class(&self, class: &str, handle: ElementHandle) -> Option<ElementHandle> {
        let elem = self.nodes.get(&handle)?;
        let classes = elem.class_attr();
        if classes.split_whitespace().any(|c| c == class) {
            return Some(handle);
        }
        for &child in &elem.children {
            if let Some(found) = self.find_by_class(class, child) {
                return Some(found);
            }
        }
        None
    }

    /// Recursive depth-first search by tag name.
    fn find_by_tag(&self, tag: &str, handle: ElementHandle) -> Option<ElementHandle> {
        let elem = self.nodes.get(&handle)?;
        if elem.tag == tag.to_lowercase() && handle != self.root {
            return Some(handle);
        }
        for &child in &elem.children {
            if let Some(found) = self.find_by_tag(tag, child) {
                return Some(found);
            }
        }
        None
    }

    // ── DOM mutation API ───────────────────────────────────────────────────────

    /// `document.createElement(tag)` — creates an unattached element and returns its handle.
    pub fn create_element(&mut self, tag: &str) -> ElementHandle {
        let handle = self.alloc_handle();
        let elem = JsElement::new_element(handle, tag);
        self.nodes.insert(handle, elem);
        debug!(handle, tag, "createElement");
        handle
    }

    /// Get the text content of an element (concatenates all descendant text nodes).
    pub fn get_text_content(&self, handle: ElementHandle) -> String {
        let Some(elem) = self.nodes.get(&handle) else {
            return String::new();
        };
        if elem.tag == "#text" {
            return elem.text.clone().unwrap_or_default();
        }
        elem.children
            .iter()
            .map(|&h| self.get_text_content(h))
            .collect::<Vec<_>>()
            .join("")
    }

    /// `el.textContent = value` — replaces all children with a single text node.
    pub fn set_text_content(&mut self, handle: ElementHandle, value: &str) {
        // Remove existing children from node map.
        let children: Vec<ElementHandle> = self
            .nodes
            .get(&handle)
            .map(|e| e.children.clone())
            .unwrap_or_default();
        for child in children {
            self.remove_subtree(child);
        }
        // Create a new text node.
        let text_handle = self.alloc_handle();
        let text_elem = JsElement::new_text(text_handle, value);
        self.nodes.insert(text_handle, text_elem);
        if let Some(elem) = self.nodes.get_mut(&handle) {
            elem.children = vec![text_handle];
        }
        debug!(handle, value, "set textContent");
    }

    /// Recursively remove a subtree from the node map.
    fn remove_subtree(&mut self, handle: ElementHandle) {
        let children: Vec<ElementHandle> = self
            .nodes
            .get(&handle)
            .map(|e| e.children.clone())
            .unwrap_or_default();
        for child in children {
            self.remove_subtree(child);
        }
        self.nodes.remove(&handle);
    }

    /// Get `el.innerHTML` as a serialised HTML string.
    pub fn get_inner_html(&self, handle: ElementHandle) -> String {
        let Some(elem) = self.nodes.get(&handle) else {
            return String::new();
        };
        elem.children
            .iter()
            .map(|&h| self.serialise_node(h))
            .collect::<Vec<_>>()
            .join("")
    }

    fn serialise_node(&self, handle: ElementHandle) -> String {
        let Some(elem) = self.nodes.get(&handle) else {
            return String::new();
        };
        match elem.tag.as_str() {
            "#text" => elem.text.clone().unwrap_or_default(),
            "#comment" => format!("<!--{}-->", elem.text.clone().unwrap_or_default()),
            "#document" => elem
                .children
                .iter()
                .map(|&h| self.serialise_node(h))
                .collect::<Vec<_>>()
                .join(""),
            tag => {
                let attrs: String = elem
                    .attributes
                    .iter()
                    .map(|(k, v)| format!(" {k}=\"{v}\""))
                    .collect();
                let children: String = elem
                    .children
                    .iter()
                    .map(|&h| self.serialise_node(h))
                    .collect::<Vec<_>>()
                    .join("");
                format!("<{tag}{attrs}>{children}</{tag}>")
            }
        }
    }

    /// `el.innerHTML = html` — parses the HTML string and replaces children.
    ///
    /// This is a best-effort implementation: it uses a simple tokeniser that
    /// handles the most common patterns without a full HTML5 parser.
    pub fn set_inner_html(&mut self, handle: ElementHandle, html: &str) {
        // Remove existing children.
        let children: Vec<ElementHandle> = self
            .nodes
            .get(&handle)
            .map(|e| e.children.clone())
            .unwrap_or_default();
        for child in children {
            self.remove_subtree(child);
        }
        // Parse and attach new children.
        let new_children = self.parse_html_fragment(html);
        if let Some(elem) = self.nodes.get_mut(&handle) {
            elem.children = new_children;
        }
        debug!(handle, "set innerHTML");
    }

    /// Minimal HTML fragment parser (handles tags and text runs only).
    pub(crate) fn parse_html_fragment(&mut self, html: &str) -> Vec<ElementHandle> {
        let mut handles = Vec::new();
        let mut remaining = html;

        while !remaining.is_empty() {
            if remaining.starts_with('<') {
                // Find end of tag.
                if let Some(end) = remaining.find('>') {
                    let tag_content = &remaining[1..end];
                    remaining = &remaining[end + 1..];

                    if tag_content.starts_with('/') {
                        // Closing tag — skip.
                        continue;
                    }
                    if tag_content.starts_with('!') {
                        // Comment or doctype — skip.
                        continue;
                    }

                    // Self-closing or opening tag.
                    let tag_content = tag_content.trim_end_matches('/').trim();
                    let mut parts = tag_content.splitn(2, char::is_whitespace);
                    let tag = parts.next().unwrap_or("span").to_lowercase();
                    let attr_str = parts.next().unwrap_or("");

                    let elem_handle = self.alloc_handle();
                    let mut elem = JsElement::new_element(elem_handle, &tag);

                    // Parse attributes (name="value" pairs).
                    parse_attributes(attr_str, &mut elem.attributes);

                    self.nodes.insert(elem_handle, elem);
                    handles.push(elem_handle);
                } else {
                    // Malformed — consume rest.
                    break;
                }
            } else {
                // Text run until next '<'.
                let (text_part, rest) = match remaining.find('<') {
                    Some(pos) => (&remaining[..pos], &remaining[pos..]),
                    None => (remaining, ""),
                };
                remaining = rest;
                if !text_part.is_empty() {
                    let text_handle = self.alloc_handle();
                    let text_elem = JsElement::new_text(text_handle, text_part);
                    self.nodes.insert(text_handle, text_elem);
                    handles.push(text_handle);
                }
            }
        }

        handles
    }

    /// `el.appendChild(child)` — attaches child to parent.
    pub fn append_child(&mut self, parent: ElementHandle, child: ElementHandle) -> bool {
        if !self.nodes.contains_key(&parent) || !self.nodes.contains_key(&child) {
            warn!(parent, child, "appendChild: handle not found");
            return false;
        }
        if let Some(parent_elem) = self.nodes.get_mut(&parent) {
            parent_elem.children.push(child);
        }
        debug!(parent, child, "appendChild");
        true
    }

    /// `el.setAttribute(name, value)`.
    pub fn set_attribute(&mut self, handle: ElementHandle, name: &str, value: &str) {
        let Some(elem) = self.nodes.get_mut(&handle) else {
            warn!(handle, name, "setAttribute: handle not found");
            return;
        };
        if let Some(pos) = elem.attributes.iter().position(|(k, _)| k == name) {
            elem.attributes[pos].1 = value.to_owned();
        } else {
            elem.attributes.push((name.to_owned(), value.to_owned()));
        }
        debug!(handle, name, value, "setAttribute");
    }

    /// `el.getAttribute(name)` — returns `None` if not found.
    pub fn get_attribute(&self, handle: ElementHandle, name: &str) -> Option<String> {
        self.nodes
            .get(&handle)?
            .attributes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }

    /// `el.classList.add(cls)` — adds a CSS class without duplicates.
    pub fn class_list_add(&mut self, handle: ElementHandle, cls: &str) {
        let Some(elem) = self.nodes.get_mut(&handle) else {
            warn!(handle, cls, "classList.add: handle not found");
            return;
        };
        let mut classes = elem.class_attr();
        if !classes.split_whitespace().any(|c| c == cls) {
            if !classes.is_empty() {
                classes.push(' ');
            }
            classes.push_str(cls);
            elem.set_class_attr(classes);
        }
        debug!(handle, cls, "classList.add");
    }

    /// `el.classList.remove(cls)` — removes a CSS class.
    pub fn class_list_remove(&mut self, handle: ElementHandle, cls: &str) {
        let Some(elem) = self.nodes.get_mut(&handle) else {
            warn!(handle, cls, "classList.remove: handle not found");
            return;
        };
        let classes = elem.class_attr();
        let new_classes: String = classes
            .split_whitespace()
            .filter(|&c| c != cls)
            .collect::<Vec<_>>()
            .join(" ");
        elem.set_class_attr(new_classes);
        debug!(handle, cls, "classList.remove");
    }

    /// `el.style.setProperty(name, value)`.
    pub fn style_set_property(&mut self, handle: ElementHandle, name: &str, value: &str) {
        let Some(elem) = self.nodes.get_mut(&handle) else {
            warn!(handle, name, "style.setProperty: handle not found");
            return;
        };
        if let Some(pos) = elem.inline_styles.iter().position(|(k, _)| k == name) {
            elem.inline_styles[pos].1 = value.to_owned();
        } else {
            elem.inline_styles.push((name.to_owned(), value.to_owned()));
        }
        debug!(handle, name, value, "style.setProperty");
    }

    // ── Event listener API ─────────────────────────────────────────────────────

    /// `el.addEventListener(type, callback)` — registers a JS callback.
    ///
    /// The `callback_source` is the JavaScript source of the function body
    /// (not the full `function(e){…}` wrapper — just the body text that the
    /// interpreter will evaluate when the event fires).
    ///
    /// `captured_env` should be the current variable-to-handle bindings from
    /// the enclosing interpreter scope so the callback can reference those
    /// variables by name.
    pub fn add_event_listener(
        &mut self,
        handle: ElementHandle,
        event_type: &str,
        callback_source: &str,
        captured_env: Vec<(String, ElementHandle)>,
        capture: bool,
    ) {
        let cb = EventCallback {
            source: callback_source.to_owned(),
            event_type: event_type.to_owned(),
            element_handle: handle,
            captured_env,
            capture,
        };
        self.listeners
            .entry((handle, event_type.to_owned()))
            .or_default()
            .push(cb);
        debug!(handle, event_type, "addEventListener registered");
    }

    /// Dispatch a browser event to all registered listeners.
    ///
    /// Returns a list of `(callback_source, captured_env)` pairs.  The
    /// interpreter should seed its env with `captured_env` before executing
    /// `callback_source` so that variable references inside the callback body
    /// resolve correctly.
    pub fn dispatch_event(
        &mut self,
        handle: ElementHandle,
        event_type: &str,
    ) -> Vec<(String, Vec<(String, ElementHandle)>)> {
        self.pending_events.push((handle, event_type.to_owned()));
        let key = (handle, event_type.to_owned());
        self.listeners
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|cb| (cb.source, cb.captured_env))
            .collect()
    }

    /// Look up a node's tag.
    pub fn tag_of(&self, handle: ElementHandle) -> Option<&str> {
        self.nodes.get(&handle).map(|e| e.tag.as_str())
    }

    /// Get the root handle.
    pub fn root_handle(&self) -> ElementHandle {
        self.root
    }

    /// `document.querySelectorAll(selector)` — returns all matching elements.
    pub fn query_selector_all(&self, selector: &str) -> Vec<ElementHandle> {
        let selectors = parse_selector_list(selector.trim());
        let mut results = Vec::new();
        self.collect_all_matching(&selectors, self.root, &mut results);
        results
    }

    /// Find the first element matching a complex selector (DFS).
    fn find_first_matching(&self, selector: &Selector, scope: ElementHandle) -> Option<ElementHandle> {
        // Collect all candidates in DFS order, then test each.
        let mut candidates = Vec::new();
        self.collect_all_descendants(scope, &mut candidates);
        for &handle in &candidates {
            if self.matches_complex_selector(handle, &selector.parts) {
                return Some(handle);
            }
        }
        None
    }

    /// Collect all matching elements for a selector list.
    fn collect_all_matching(&self, selectors: &[Selector], scope: ElementHandle, out: &mut Vec<ElementHandle>) {
        let mut candidates = Vec::new();
        self.collect_all_descendants(scope, &mut candidates);
        for &handle in &candidates {
            for sel in selectors {
                if self.matches_complex_selector(handle, &sel.parts) {
                    out.push(handle);
                    break;
                }
            }
        }
    }

    /// Collect all descendants in DFS order (excluding the root itself).
    fn collect_all_descendants(&self, handle: ElementHandle, out: &mut Vec<ElementHandle>) {
        let Some(elem) = self.nodes.get(&handle) else { return };
        for &child in &elem.children {
            if self.nodes.get(&child).map(|e| e.tag != "#text" && e.tag != "#comment").unwrap_or(false) {
                out.push(child);
            }
            self.collect_all_descendants(child, out);
        }
    }

    /// Test if a handle matches a complex selector (sequence of parts with combinators).
    fn matches_complex_selector(&self, handle: ElementHandle, parts: &[SelectorPart]) -> bool {
        if parts.is_empty() {
            return true;
        }

        // Walk parts right-to-left. The rightmost simple selector must match the target.
        let mut idx = parts.len();
        let mut current = handle;

        // Find the rightmost simple selector.
        loop {
            if idx == 0 { return false; }
            idx -= 1;
            if let SelectorPart::Simple(ref s) = parts[idx] {
                if !self.element_matches_simple(current, s) {
                    return false;
                }
                break;
            }
        }

        // Now walk leftward through combinator + selector pairs.
        while idx > 0 {
            idx -= 1;
            let combinator = &parts[idx];
            if idx == 0 { return false; } // combinator without preceding selector
            idx -= 1;
            let SelectorPart::Simple(ref prev_sel) = parts[idx] else {
                return false;
            };

            match combinator {
                SelectorPart::Descendant => {
                    // Find any ancestor matching.
                    let mut ancestor = self.parent_node(current);
                    let mut found = false;
                    while let Some(anc) = ancestor {
                        if self.element_matches_simple(anc, prev_sel) {
                            current = anc;
                            found = true;
                            break;
                        }
                        ancestor = self.parent_node(anc);
                    }
                    if !found { return false; }
                }
                SelectorPart::Child => {
                    let Some(parent) = self.parent_node(current) else { return false };
                    if !self.element_matches_simple(parent, prev_sel) { return false; }
                    current = parent;
                }
                SelectorPart::Adjacent => {
                    let Some(prev_sib) = self.previous_element_sibling(current) else { return false };
                    if !self.element_matches_simple(prev_sib, prev_sel) { return false; }
                    current = prev_sib;
                }
                SelectorPart::General => {
                    let Some(parent) = self.parent_node(current) else { return false };
                    let siblings = self.children(parent);
                    let my_pos = siblings.iter().position(|&h| h == current).unwrap_or(0);
                    let mut found = false;
                    for &sib in &siblings[..my_pos] {
                        if self.element_matches_simple(sib, prev_sel) {
                            current = sib;
                            found = true;
                            break;
                        }
                    }
                    if !found { return false; }
                }
                _ => return false,
            }
        }

        true
    }

    /// Test if a single element matches a simple selector (no combinators).
    fn element_matches_simple(&self, handle: ElementHandle, sel: &SimpleSelector) -> bool {
        let Some(elem) = self.nodes.get(&handle) else { return false };
        if elem.tag == "#text" || elem.tag == "#comment" || elem.tag == "#document" {
            return false;
        }

        // Tag check.
        if let Some(ref tag) = sel.tag {
            if tag != "*" && elem.tag != *tag {
                return false;
            }
        }

        // ID check.
        if let Some(ref id) = sel.id {
            let has_id = elem.attributes.iter().any(|(k, v)| k == "id" && v == id);
            if !has_id { return false; }
        }

        // Class checks.
        for cls in &sel.classes {
            let class_attr = elem.class_attr();
            if !class_attr.split_whitespace().any(|c| c == cls.as_str()) {
                return false;
            }
        }

        // Attribute checks.
        for attr in &sel.attrs {
            let attr_val = elem.attributes.iter().find(|(k, _)| *k == attr.name).map(|(_, v)| v.as_str());
            match attr.op {
                AttrOp::Exists => {
                    if attr_val.is_none() { return false; }
                }
                AttrOp::Equals => {
                    if attr_val != attr.value.as_deref() { return false; }
                }
                AttrOp::StartsWith => {
                    match (attr_val, attr.value.as_deref()) {
                        (Some(val), Some(expected)) => { if !val.starts_with(expected) { return false; } }
                        _ => { return false; }
                    }
                }
                AttrOp::EndsWith => {
                    match (attr_val, attr.value.as_deref()) {
                        (Some(val), Some(expected)) => { if !val.ends_with(expected) { return false; } }
                        _ => { return false; }
                    }
                }
                AttrOp::Contains => {
                    match (attr_val, attr.value.as_deref()) {
                        (Some(val), Some(expected)) => { if !val.contains(expected) { return false; } }
                        _ => { return false; }
                    }
                }
            }
        }

        // Pseudo-class checks.
        for pseudo in &sel.pseudos {
            match pseudo {
                PseudoSelector::FirstChild => {
                    if let Some(parent) = self.parent_node(handle) {
                        let siblings = self.children(parent);
                        if siblings.first() != Some(&handle) { return false; }
                    }
                }
                PseudoSelector::LastChild => {
                    if let Some(parent) = self.parent_node(handle) {
                        let siblings = self.children(parent);
                        if siblings.last() != Some(&handle) { return false; }
                    }
                }
                PseudoSelector::NthChild(n) => {
                    if let Some(parent) = self.parent_node(handle) {
                        let siblings = self.children(parent);
                        let pos = siblings.iter().position(|&h| h == handle);
                        if pos != Some((*n as usize).wrapping_sub(1)) { return false; }
                    }
                }
            }
        }

        // :not() check.
        if let Some(ref not_sel) = sel.not {
            if self.element_matches_simple(handle, not_sel) {
                return false;
            }
        }

        true
    }

    /// Get the previous element sibling (skipping text/comment nodes).
    fn previous_element_sibling(&self, handle: ElementHandle) -> Option<ElementHandle> {
        let parent = self.parent_node(handle)?;
        let siblings = self.children(parent);
        let pos = siblings.iter().position(|&h| h == handle)?;
        if pos > 0 { Some(siblings[pos - 1]) } else { None }
    }

    /// Recursive collect by attribute.
    #[allow(dead_code)]
    fn collect_by_attr(
        &self,
        attr: &str,
        value: &str,
        handle: ElementHandle,
        out: &mut Vec<ElementHandle>,
    ) {
        let Some(elem) = self.nodes.get(&handle) else { return };
        if elem.attributes.iter().any(|(k, v)| k == attr && v == value) {
            out.push(handle);
        }
        for &child in &elem.children {
            self.collect_by_attr(attr, value, child, out);
        }
    }

    /// Recursive collect by class.
    fn collect_by_class(
        &self,
        class: &str,
        handle: ElementHandle,
        out: &mut Vec<ElementHandle>,
    ) {
        let Some(elem) = self.nodes.get(&handle) else { return };
        if elem.class_attr().split_whitespace().any(|c| c == class) {
            out.push(handle);
        }
        for &child in &elem.children {
            self.collect_by_class(class, child, out);
        }
    }

    /// Recursive collect by tag name.
    fn collect_by_tag(
        &self,
        tag: &str,
        handle: ElementHandle,
        out: &mut Vec<ElementHandle>,
    ) {
        let Some(elem) = self.nodes.get(&handle) else { return };
        if elem.tag == tag.to_lowercase() && handle != self.root {
            out.push(handle);
        }
        for &child in &elem.children {
            self.collect_by_tag(tag, child, out);
        }
    }

    /// Get the parent node of an element.
    pub fn parent_node(&self, handle: ElementHandle) -> Option<ElementHandle> {
        for (h, elem) in &self.nodes {
            if elem.children.contains(&handle) {
                return Some(*h);
            }
        }
        None
    }

    /// Get the next sibling of an element.
    pub fn next_sibling(&self, handle: ElementHandle) -> Option<ElementHandle> {
        let parent = self.parent_node(handle)?;
        let parent_elem = self.nodes.get(&parent)?;
        let pos = parent_elem.children.iter().position(|&h| h == handle)?;
        parent_elem.children.get(pos + 1).copied()
    }

    /// Get the previous sibling of an element.
    pub fn previous_sibling(&self, handle: ElementHandle) -> Option<ElementHandle> {
        let parent = self.parent_node(handle)?;
        let parent_elem = self.nodes.get(&parent)?;
        let pos = parent_elem.children.iter().position(|&h| h == handle)?;
        if pos > 0 {
            Some(parent_elem.children[pos - 1])
        } else {
            None
        }
    }

    /// Get the first child of an element.
    pub fn first_child(&self, handle: ElementHandle) -> Option<ElementHandle> {
        self.nodes.get(&handle)?.children.first().copied()
    }

    /// Get the last child of an element.
    pub fn last_child(&self, handle: ElementHandle) -> Option<ElementHandle> {
        self.nodes.get(&handle)?.children.last().copied()
    }

    /// Clone a node (optionally deep).
    pub fn clone_node(&mut self, handle: ElementHandle, deep: bool) -> ElementHandle {
        let Some(elem) = self.nodes.get(&handle).cloned() else {
            return self.alloc_handle();
        };
        let new_handle = self.alloc_handle();
        let mut new_elem = JsElement {
            handle: new_handle,
            tag: elem.tag.clone(),
            attributes: elem.attributes.clone(),
            children: Vec::new(),
            text: elem.text.clone(),
            inline_styles: elem.inline_styles.clone(),
        };
        if deep {
            for &child in &elem.children {
                let cloned_child = self.clone_node(child, true);
                new_elem.children.push(cloned_child);
            }
        }
        self.nodes.insert(new_handle, new_elem);
        new_handle
    }

    /// Insert a new child before a reference child.
    pub fn insert_before(
        &mut self,
        parent: ElementHandle,
        new_child: ElementHandle,
        ref_child: Option<ElementHandle>,
    ) -> bool {
        if !self.nodes.contains_key(&parent) || !self.nodes.contains_key(&new_child) {
            warn!(parent, new_child, "insertBefore: handle not found");
            return false;
        }
        let Some(parent_elem) = self.nodes.get_mut(&parent) else {
            return false;
        };
        match ref_child {
            Some(ref_h) => {
                if let Some(pos) = parent_elem.children.iter().position(|&h| h == ref_h) {
                    parent_elem.children.insert(pos, new_child);
                    true
                } else {
                    parent_elem.children.push(new_child);
                    true
                }
            }
            None => {
                parent_elem.children.push(new_child);
                true
            }
        }
    }

    /// Check if a parent element contains a child (descendant).
    pub fn contains(&self, parent: ElementHandle, child: ElementHandle) -> bool {
        if parent == child {
            return true;
        }
        let Some(elem) = self.nodes.get(&parent) else {
            return false;
        };
        for &c in &elem.children {
            if self.contains(c, child) {
                return true;
            }
        }
        false
    }

    /// Remove an attribute from an element.
    pub fn remove_attribute(&mut self, handle: ElementHandle, name: &str) {
        let Some(elem) = self.nodes.get_mut(&handle) else {
            warn!(handle, name, "removeAttribute: handle not found");
            return;
        };
        elem.attributes.retain(|(k, _)| k != name);
        debug!(handle, name, "removeAttribute");
    }

    /// Check if an element has a given attribute.
    pub fn has_attribute(&self, handle: ElementHandle, name: &str) -> bool {
        self.nodes
            .get(&handle)
            .map(|e| e.attributes.iter().any(|(k, _)| k == name))
            .unwrap_or(false)
    }

    /// Get all element children (not text nodes) of a node.
    pub fn children(&self, handle: ElementHandle) -> Vec<ElementHandle> {
        let Some(elem) = self.nodes.get(&handle) else {
            return Vec::new();
        };
        elem.children
            .iter()
            .copied()
            .filter(|&h| {
                self.nodes
                    .get(&h)
                    .map(|e| e.tag != "#text" && e.tag != "#comment")
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Check if an element matches a CSS selector.
    pub fn matches(&self, handle: ElementHandle, selector: &str) -> bool {
        let selectors = parse_selector_list(selector.trim());
        for sel in &selectors {
            if self.matches_complex_selector(handle, &sel.parts) {
                return true;
            }
        }
        false
    }

    /// `document.createTextNode(text)` — creates an unattached text node.
    pub fn create_text_node(&mut self, text: &str) -> ElementHandle {
        let handle = self.alloc_handle();
        let elem = JsElement::new_text(handle, text);
        self.nodes.insert(handle, elem);
        debug!(handle, text, "createTextNode");
        handle
    }

    /// Remove a child from its parent.
    pub fn remove_child(&mut self, parent: ElementHandle, child: ElementHandle) -> bool {
        let Some(parent_elem) = self.nodes.get_mut(&parent) else {
            warn!(parent, child, "removeChild: parent not found");
            return false;
        };
        let len_before = parent_elem.children.len();
        parent_elem.children.retain(|&h| h != child);
        let removed = parent_elem.children.len() < len_before;
        if removed {
            debug!(parent, child, "removeChild");
        }
        removed
    }

    /// Get inline style property value.
    pub fn style_get_property(&self, handle: ElementHandle, name: &str) -> Option<String> {
        self.nodes
            .get(&handle)?
            .inline_styles
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }

    /// Check if a classList contains a class.
    pub fn class_list_contains(&self, handle: ElementHandle, cls: &str) -> bool {
        self.nodes
            .get(&handle)
            .map(|e| e.class_attr().split_whitespace().any(|c| c == cls))
            .unwrap_or(false)
    }

    /// Toggle a class in classList. Returns true if class is now present.
    pub fn class_list_toggle(&mut self, handle: ElementHandle, cls: &str) -> bool {
        if self.class_list_contains(handle, cls) {
            self.class_list_remove(handle, cls);
            false
        } else {
            self.class_list_add(handle, cls);
            true
        }
    }

    /// Replace a class in classList. Returns true if `old_cls` was found and replaced.
    pub fn class_list_replace(&mut self, handle: ElementHandle, old_cls: &str, new_cls: &str) -> bool {
        let Some(elem) = self.nodes.get_mut(&handle) else {
            warn!(handle, old_cls, new_cls, "classList.replace: handle not found");
            return false;
        };
        let classes = elem.class_attr();
        if !classes.split_whitespace().any(|c| c == old_cls) {
            return false;
        }
        let new_classes: String = classes
            .split_whitespace()
            .map(|c| if c == old_cls { new_cls } else { c })
            .collect::<Vec<_>>()
            .join(" ");
        elem.set_class_attr(new_classes);
        debug!(handle, old_cls, new_cls, "classList.replace");
        true
    }

    /// Get the `className` (class attribute value) for an element.
    pub fn class_name(&self, handle: ElementHandle) -> String {
        self.nodes
            .get(&handle)
            .map(|e| e.class_attr())
            .unwrap_or_default()
    }

    /// Set the `className` attribute for an element.
    pub fn set_class_name(&mut self, handle: ElementHandle, value: &str) {
        if let Some(elem) = self.nodes.get_mut(&handle) {
            elem.set_class_attr(value.to_owned());
        }
    }

    /// Get the `id` attribute for an element.
    pub fn get_id(&self, handle: ElementHandle) -> String {
        self.get_attribute(handle, "id").unwrap_or_default()
    }

    /// Set the `id` attribute for an element.
    pub fn set_id(&mut self, handle: ElementHandle, value: &str) {
        self.set_attribute(handle, "id", value);
    }

    /// Get all child handles (including text nodes).
    pub fn child_nodes(&self, handle: ElementHandle) -> Vec<ElementHandle> {
        self.nodes
            .get(&handle)
            .map(|e| e.children.clone())
            .unwrap_or_default()
    }

    /// Find the body element handle.
    pub fn body(&self) -> Option<ElementHandle> {
        self.find_by_tag("body", self.root)
    }

    /// Get or set the document title.
    pub fn get_title(&self) -> String {
        if let Some(title_handle) = self.find_by_tag("title", self.root) {
            self.get_text_content(title_handle)
        } else {
            String::new()
        }
    }

    /// Set the document title.
    pub fn set_title(&mut self, title: &str) {
        if let Some(title_handle) = self.find_by_tag("title", self.root) {
            self.set_text_content(title_handle, title);
        }
    }

    /// Find the `<html>` document element handle.
    pub fn document_element(&self) -> Option<ElementHandle> {
        self.find_by_tag("html", self.root)
    }

    /// Get the CSS text for inline styles (e.g. "color: red; display: none;").
    pub fn style_css_text(&self, handle: ElementHandle) -> String {
        let Some(elem) = self.nodes.get(&handle) else {
            return String::new();
        };
        elem.inline_styles
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Get the outer HTML of an element (includes the element itself).
    pub fn get_outer_html(&self, handle: ElementHandle) -> String {
        self.serialise_node(handle)
    }

    /// `element.insertAdjacentHTML(position, html)` — insert parsed HTML at a position
    /// relative to the element.
    ///
    /// `position` is one of: `"beforebegin"`, `"afterbegin"`, `"beforeend"`, `"afterend"`.
    pub fn insert_adjacent_html(&mut self, handle: ElementHandle, position: &str, html: &str) {
        let new_children = self.parse_html_fragment(html);
        match position {
            "beforebegin" => {
                // Insert before this element in its parent's children list.
                if let Some(parent_handle) = self.parent_node(handle) {
                    if let Some(parent) = self.nodes.get_mut(&parent_handle) {
                        if let Some(pos) = parent.children.iter().position(|&h| h == handle) {
                            for (i, child) in new_children.into_iter().enumerate() {
                                parent.children.insert(pos + i, child);
                            }
                        }
                    }
                }
            }
            "afterbegin" => {
                // Insert as first children of this element.
                if let Some(elem) = self.nodes.get_mut(&handle) {
                    for (i, child) in new_children.into_iter().enumerate() {
                        elem.children.insert(i, child);
                    }
                }
            }
            "beforeend" => {
                // Append to this element's children.
                if let Some(elem) = self.nodes.get_mut(&handle) {
                    elem.children.extend(new_children);
                }
            }
            "afterend" => {
                // Insert after this element in its parent's children list.
                if let Some(parent_handle) = self.parent_node(handle) {
                    if let Some(parent) = self.nodes.get_mut(&parent_handle) {
                        if let Some(pos) = parent.children.iter().position(|&h| h == handle) {
                            for (i, child) in new_children.into_iter().enumerate() {
                                parent.children.insert(pos + 1 + i, child);
                            }
                        }
                    }
                }
            }
            _ => {
                warn!(handle, position, "insertAdjacentHTML: invalid position");
            }
        }
        debug!(handle, position, "insertAdjacentHTML");
    }

    /// Query selector scoped to a specific element.
    pub fn query_selector_within(
        &self,
        handle: ElementHandle,
        selector: &str,
    ) -> Option<ElementHandle> {
        let selectors = parse_selector_list(selector.trim());
        let mut candidates = Vec::new();
        self.collect_all_descendants(handle, &mut candidates);
        for &cand in &candidates {
            for sel in &selectors {
                if self.matches_complex_selector(cand, &sel.parts) {
                    return Some(cand);
                }
            }
        }
        None
    }

    // ── Shadow DOM API ─────────────────────────────────────────────────────────

    /// `element.attachShadow({ mode })` — creates a shadow root.
    pub fn attach_shadow(&mut self, host: ElementHandle, mode: ShadowRootMode) -> Option<ElementHandle> {
        if self.shadow_roots.contains_key(&host) {
            warn!(host, "shadow root already attached");
            return None;
        }
        if !self.nodes.contains_key(&host) {
            warn!(host, "attachShadow: host handle not found");
            return None;
        }
        let shadow = ShadowRoot::new(host, mode);
        self.shadow_roots.insert(host, shadow);
        debug!(host, mode = mode.as_str(), "shadow root attached");
        Some(host)
    }

    /// `element.shadowRoot` — returns the shadow root if mode is Open.
    pub fn get_shadow_root(&self, host: ElementHandle) -> Option<&ShadowRoot> {
        self.shadow_roots.get(&host).and_then(|sr| {
            if sr.mode == ShadowRootMode::Open { Some(sr) } else { None }
        })
    }

    /// Get a mutable reference to the shadow root.
    pub fn get_shadow_root_mut(&mut self, host: ElementHandle) -> Option<&mut ShadowRoot> {
        self.shadow_roots.get_mut(&host)
    }

    /// Append a child to a shadow root's tree.
    pub fn shadow_append_child(&mut self, host: ElementHandle, child: ElementHandle) -> bool {
        if let Some(shadow) = self.shadow_roots.get_mut(&host) {
            shadow.append_child(child);
            debug!(host, child, "shadow root appendChild");
            true
        } else {
            warn!(host, "no shadow root attached");
            false
        }
    }

    // ── Custom Elements helpers ────────────────────────────────────────────────

    /// Find all elements in the tree whose tag name matches a registered
    /// custom element. Returns `(handle, tag_name)` pairs.
    ///
    /// This is used to trigger `connectedCallback` after the DOM is imported.
    pub fn find_custom_elements(&self) -> Vec<(ElementHandle, String)> {
        let mut result = Vec::new();
        for (&handle, elem) in &self.nodes {
            if self.custom_elements.is_defined(&elem.tag) {
                result.push((handle, elem.tag.clone()));
            }
        }
        result
    }

    // ── Canvas API ─────────────────────────────────────────────────────────────

    /// Get or create a `CanvasRenderingContext2D` for a `<canvas>` element.
    ///
    /// Returns `None` if the handle does not refer to a `<canvas>` element.
    /// On first call, creates a pixel buffer using the element's width/height
    /// attributes (defaulting to 300x150 per the HTML5 spec).
    pub fn get_canvas_context(&mut self, handle: ElementHandle) -> Option<&mut CanvasContext2D> {
        let elem = self.nodes.get(&handle)?;
        if elem.tag != "canvas" {
            return None;
        }
        if !self.canvas_contexts.contains_key(&handle) {
            let width: u32 = self.get_attribute(handle, "width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(300);
            let height: u32 = self.get_attribute(handle, "height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(150);
            let ctx = CanvasContext2D::new(width, height);
            self.canvas_contexts.insert(handle, ctx);
            debug!(handle, width, height, "created canvas 2D context");
        }
        self.canvas_contexts.get_mut(&handle)
    }

    /// Get an immutable reference to a canvas context if it exists.
    pub fn get_canvas_context_ref(&self, handle: ElementHandle) -> Option<&CanvasContext2D> {
        self.canvas_contexts.get(&handle)
    }

    /// Collect all canvas pixel buffers for the painter.
    ///
    /// Returns a vec of `(element_id, width, height, rgba_pixels)` for each
    /// canvas that has been drawn to.
    pub fn collect_canvas_pixels(&self) -> Vec<(String, u32, u32, Vec<u8>)> {
        let mut result = Vec::new();
        for (&handle, ctx) in &self.canvas_contexts {
            // Use the element's id attribute or handle as key.
            let id = self.get_attribute(handle, "id")
                .unwrap_or_else(|| format!("__canvas_{handle}"));
            result.push((id, ctx.width, ctx.height, ctx.pixels.clone()));
        }
        result
    }

    // ── Event system helpers ──────────────────────────────────────────────────

    /// Build the path from the target element up to the document root.
    pub fn build_ancestor_path(&self, target: ElementHandle) -> Vec<ElementHandle> {
        let mut path = vec![target];
        let mut current = target;
        while let Some(parent) = self.find_parent(current) {
            path.push(parent);
            current = parent;
        }
        path
    }

    /// Find the parent of a node.
    pub fn find_parent(&self, handle: ElementHandle) -> Option<ElementHandle> {
        for (h, el) in &self.nodes {
            if el.children.contains(&handle) {
                return Some(*h);
            }
        }
        None
    }

    /// Get all listeners for a given element handle and event type.
    pub fn get_listeners(&self, handle: ElementHandle, event_type: &str) -> Vec<&EventCallback> {
        let key = (handle, event_type.to_owned());
        self.listeners
            .get(&key)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    /// Query selector all scoped to a specific element.
    pub fn query_selector_all_within(
        &self,
        handle: ElementHandle,
        selector: &str,
    ) -> Vec<ElementHandle> {
        let selectors = parse_selector_list(selector.trim());
        let mut results = Vec::new();
        self.collect_all_matching(&selectors, handle, &mut results);
        results
    }

    // ── Form element properties ──────────────────────────────────────────────

    /// Get `el.value` for form elements (input, textarea, select).
    pub fn get_value(&self, handle: ElementHandle) -> String {
        if let Some(v) = self.values.get(&handle) {
            return v.clone();
        }
        // Fall back to "value" attribute if present.
        self.get_attribute(handle, "value").unwrap_or_default()
    }

    /// Set `el.value` for form elements.
    pub fn set_value(&mut self, handle: ElementHandle, value: &str) {
        self.values.insert(handle, value.to_owned());
        debug!(handle, value, "set value");
    }

    /// Get `el.checked` for checkbox/radio inputs.
    pub fn get_checked(&self, handle: ElementHandle) -> bool {
        if let Some(&v) = self.checked.get(&handle) {
            return v;
        }
        // Fall back to presence of "checked" attribute.
        self.has_attribute(handle, "checked")
    }

    /// Set `el.checked` for checkbox/radio inputs.
    pub fn set_checked(&mut self, handle: ElementHandle, value: bool) {
        self.checked.insert(handle, value);
        debug!(handle, value, "set checked");
    }

    /// Get `el.disabled`.
    pub fn get_disabled(&self, handle: ElementHandle) -> bool {
        self.has_attribute(handle, "disabled")
    }

    /// Set `el.disabled`.
    pub fn set_disabled(&mut self, handle: ElementHandle, value: bool) {
        if value {
            self.set_attribute(handle, "disabled", "");
        } else {
            self.remove_attribute(handle, "disabled");
        }
    }

    // ── Element dimension stubs ──────────────────────────────────────────────

    /// Get `el.offsetWidth` (reads from `data-nova-width` attribute or 0).
    pub fn get_offset_width(&self, handle: ElementHandle) -> f64 {
        self.get_attribute(handle, "data-nova-width")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Get `el.offsetHeight` (reads from `data-nova-height` attribute or 0).
    pub fn get_offset_height(&self, handle: ElementHandle) -> f64 {
        self.get_attribute(handle, "data-nova-height")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    // ── getComputedStyle helper ──────────────────────────────────────────────

    /// Parse computed style from `data-nova-style` attribute and inline styles.
    ///
    /// Returns a map of CSS property name → value. Both kebab-case and
    /// camelCase keys are inserted so that `style.fontSize` and
    /// `style["font-size"]` both resolve.
    pub fn get_computed_style(&self, handle: ElementHandle) -> HashMap<String, String> {
        let mut styles = HashMap::new();

        // Parse data-nova-style attribute.
        if let Some(style_str) = self.get_attribute(handle, "data-nova-style") {
            parse_style_string(&style_str, &mut styles);
        }

        // Overlay inline style attribute.
        if let Some(style_str) = self.get_attribute(handle, "style") {
            parse_style_string(&style_str, &mut styles);
        }

        // Overlay programmatic inline styles.
        if let Some(elem) = self.nodes.get(&handle) {
            for (k, v) in &elem.inline_styles {
                insert_style_both_cases(&mut styles, k, v);
            }
        }

        styles
    }

    // ── Additional mutation convenience methods ──────────────────────────────

    /// `el.remove()` — remove this element from its parent.
    pub fn remove_element(&mut self, handle: ElementHandle) -> bool {
        if let Some(parent) = self.parent_node(handle) {
            self.remove_child(parent, handle)
        } else {
            false
        }
    }

    /// `el.replaceWith(new_el)` — replace this element with another in its parent.
    pub fn replace_with(&mut self, handle: ElementHandle, new_handle: ElementHandle) -> bool {
        let Some(parent) = self.parent_node(handle) else {
            return false;
        };
        let Some(parent_elem) = self.nodes.get_mut(&parent) else {
            return false;
        };
        if let Some(pos) = parent_elem.children.iter().position(|&h| h == handle) {
            parent_elem.children[pos] = new_handle;
            debug!(handle, new_handle, "replaceWith");
            true
        } else {
            false
        }
    }

    /// `el.after(new_el)` — insert new_el after this element in parent.
    pub fn insert_after(&mut self, handle: ElementHandle, new_handle: ElementHandle) -> bool {
        let Some(parent) = self.parent_node(handle) else {
            return false;
        };
        let Some(parent_elem) = self.nodes.get_mut(&parent) else {
            return false;
        };
        if let Some(pos) = parent_elem.children.iter().position(|&h| h == handle) {
            parent_elem.children.insert(pos + 1, new_handle);
            debug!(handle, new_handle, "after");
            true
        } else {
            false
        }
    }

    /// `el.before(new_el)` — insert new_el before this element in parent.
    pub fn insert_before_self(&mut self, handle: ElementHandle, new_handle: ElementHandle) -> bool {
        let Some(parent) = self.parent_node(handle) else {
            return false;
        };
        let Some(parent_elem) = self.nodes.get_mut(&parent) else {
            return false;
        };
        if let Some(pos) = parent_elem.children.iter().position(|&h| h == handle) {
            parent_elem.children.insert(pos, new_handle);
            debug!(handle, new_handle, "before");
            true
        } else {
            false
        }
    }

    /// `el.append(...nodes)` — append multiple children.
    pub fn append_multiple(&mut self, parent: ElementHandle, children: &[ElementHandle]) {
        for &child in children {
            self.append_child(parent, child);
        }
    }

    /// `el.prepend(...nodes)` — prepend multiple children.
    pub fn prepend_multiple(&mut self, parent: ElementHandle, children: &[ElementHandle]) {
        if let Some(parent_elem) = self.nodes.get_mut(&parent) {
            let mut new_children: Vec<ElementHandle> = children.to_vec();
            new_children.extend(parent_elem.children.iter().copied());
            parent_elem.children = new_children;
            debug!(parent, "prepend");
        }
    }

    // ── Missing DOM APIs (Sprint 2c) ─────────────────────────────────────────

    /// `document.createDocumentFragment()` — create a virtual container node.
    pub fn create_document_fragment(&mut self) -> ElementHandle {
        let handle = self.alloc_handle();
        let elem = JsElement {
            handle,
            tag: "#document-fragment".into(),
            attributes: Vec::new(),
            children: Vec::new(),
            text: None,
            inline_styles: Vec::new(),
        };
        self.nodes.insert(handle, elem);
        debug!(handle, "createDocumentFragment");
        handle
    }

    /// `el.closest(selector)` — walk up parents finding first match.
    pub fn closest(&self, handle: ElementHandle, selector: &str) -> Option<ElementHandle> {
        let mut current = Some(handle);
        while let Some(h) = current {
            if self.matches(h, selector) {
                return Some(h);
            }
            current = self.parent_node(h);
        }
        None
    }

    /// `document.getElementsByClassName(name)` — collect all elements with class.
    pub fn get_elements_by_class_name(&self, class: &str) -> Vec<ElementHandle> {
        let mut results = Vec::new();
        self.collect_by_class(class, self.root, &mut results);
        results
    }

    /// `document.getElementsByTagName(tag)` — collect all elements with tag.
    pub fn get_elements_by_tag_name(&self, tag: &str) -> Vec<ElementHandle> {
        let mut results = Vec::new();
        self.collect_by_tag(tag, self.root, &mut results);
        results
    }

    /// Get a dataset value (data-* attribute).
    pub fn get_dataset(&self, handle: ElementHandle, key: &str) -> Option<String> {
        let attr_name = format!("data-{}", camel_to_kebab(key));
        self.get_attribute(handle, &attr_name)
    }

    /// Set a dataset value (data-* attribute).
    pub fn set_dataset(&mut self, handle: ElementHandle, key: &str, value: &str) {
        let attr_name = format!("data-{}", camel_to_kebab(key));
        self.set_attribute(handle, &attr_name, value);
    }

    /// Get all dataset entries (data-* attributes as camelCase keys).
    pub fn get_dataset_all(&self, handle: ElementHandle) -> HashMap<String, String> {
        let Some(elem) = self.nodes.get(&handle) else { return HashMap::new() };
        let mut map = HashMap::new();
        for (k, v) in &elem.attributes {
            if let Some(suffix) = k.strip_prefix("data-") {
                let camel = kebab_to_camel(suffix);
                map.insert(camel, v.clone());
            }
        }
        map
    }

    /// `el.outerHTML` setter — replace this element with parsed HTML.
    pub fn set_outer_html(&mut self, handle: ElementHandle, html: &str) {
        let new_children = self.parse_html_fragment(html);
        if let Some(parent) = self.parent_node(handle) {
            if let Some(parent_elem) = self.nodes.get_mut(&parent) {
                if let Some(pos) = parent_elem.children.iter().position(|&h| h == handle) {
                    parent_elem.children.splice(pos..=pos, new_children);
                    debug!(handle, "set outerHTML");
                }
            }
        }
    }

    /// `el.replaceChild(newChild, oldChild)` — replace a child in parent.
    pub fn replace_child(
        &mut self,
        parent: ElementHandle,
        new_child: ElementHandle,
        old_child: ElementHandle,
    ) -> bool {
        let Some(parent_elem) = self.nodes.get_mut(&parent) else {
            warn!(parent, "replaceChild: parent not found");
            return false;
        };
        if let Some(pos) = parent_elem.children.iter().position(|&h| h == old_child) {
            parent_elem.children[pos] = new_child;
            debug!(parent, new_child, old_child, "replaceChild");
            true
        } else {
            false
        }
    }

    /// Get the head element handle.
    pub fn head(&self) -> Option<ElementHandle> {
        self.find_by_tag("head", self.root)
    }

    // ── Dimension stubs (extended) ───────────────────────────────────────────

    /// Get `el.offsetTop` (reads from `data-nova-top` or 0).
    pub fn get_offset_top(&self, handle: ElementHandle) -> f64 {
        self.get_attribute(handle, "data-nova-top")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Get `el.offsetLeft` (reads from `data-nova-left` or 0).
    pub fn get_offset_left(&self, handle: ElementHandle) -> f64 {
        self.get_attribute(handle, "data-nova-left")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Get `el.clientWidth` (reads from `data-nova-client-width` or `data-nova-width` or 0).
    pub fn get_client_width(&self, handle: ElementHandle) -> f64 {
        self.get_attribute(handle, "data-nova-client-width")
            .or_else(|| self.get_attribute(handle, "data-nova-width"))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Get `el.clientHeight`.
    pub fn get_client_height(&self, handle: ElementHandle) -> f64 {
        self.get_attribute(handle, "data-nova-client-height")
            .or_else(|| self.get_attribute(handle, "data-nova-height"))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Get `el.scrollWidth`.
    pub fn get_scroll_width(&self, handle: ElementHandle) -> f64 {
        self.get_attribute(handle, "data-nova-scroll-width")
            .or_else(|| self.get_attribute(handle, "data-nova-width"))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Get `el.scrollHeight`.
    pub fn get_scroll_height(&self, handle: ElementHandle) -> f64 {
        self.get_attribute(handle, "data-nova-scroll-height")
            .or_else(|| self.get_attribute(handle, "data-nova-height"))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Get `el.nodeType`.
    pub fn node_type(&self, handle: ElementHandle) -> u32 {
        let Some(elem) = self.nodes.get(&handle) else { return 0 };
        match elem.tag.as_str() {
            "#text" => 3,
            "#comment" => 8,
            "#document" => 9,
            "#document-fragment" => 11,
            _ => 1, // ELEMENT_NODE
        }
    }

    /// Get `el.nodeName`.
    pub fn node_name(&self, handle: ElementHandle) -> String {
        let Some(elem) = self.nodes.get(&handle) else { return String::new() };
        match elem.tag.as_str() {
            "#text" => "#text".to_string(),
            "#comment" => "#comment".to_string(),
            "#document" => "#document".to_string(),
            "#document-fragment" => "#document-fragment".to_string(),
            tag => tag.to_uppercase(),
        }
    }
}

// ── Style parsing helpers ─────────────────────────────────────────────────────

/// Convert a kebab-case CSS property name to camelCase.
///
/// E.g. `"font-size"` → `"fontSize"`, `"background-color"` → `"backgroundColor"`.
fn kebab_to_camel(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = false;
    for c in s.chars() {
        if c == '-' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert a camelCase CSS property name to kebab-case.
///
/// E.g. `"fontSize"` → `"font-size"`.
fn camel_to_kebab(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            result.push('-');
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

/// Parse a semicolon-separated style string into a map, inserting both
/// kebab-case and camelCase variants for each property.
fn parse_style_string(style_str: &str, out: &mut HashMap<String, String>) {
    for pair in style_str.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((key, value)) = pair.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            insert_style_both_cases(out, key, value);
        }
    }
}

/// Insert a CSS property into the map under both kebab-case and camelCase keys.
fn insert_style_both_cases(map: &mut HashMap<String, String>, key: &str, value: &str) {
    let kebab = if key.contains('-') {
        key.to_owned()
    } else {
        camel_to_kebab(key)
    };
    let camel = if key.contains('-') {
        kebab_to_camel(key)
    } else {
        key.to_owned()
    };
    map.insert(kebab, value.to_owned());
    map.insert(camel, value.to_owned());
}

// ── Attribute parser helper ───────────────────────────────────────────────────

/// Parse `key="value" key='value' key=value` attribute strings into a vec.
fn parse_attributes(input: &str, out: &mut Vec<(String, String)>) {
    let mut s = input.trim();
    while !s.is_empty() {
        // Skip whitespace.
        s = s.trim_start();
        if s.is_empty() {
            break;
        }

        // Read key.
        let key_end = s
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(s.len());
        let key = s[..key_end].to_lowercase();
        s = &s[key_end..];

        if s.starts_with('=') {
            s = &s[1..]; // consume '='

            // Read value (quoted or unquoted).
            let (value, rest) = if s.starts_with('"') {
                let end = s[1..].find('"').map(|i| i + 1).unwrap_or(s.len() - 1);
                (&s[1..end], &s[end + 1..])
            } else if s.starts_with('\'') {
                let end = s[1..].find('\'').map(|i| i + 1).unwrap_or(s.len() - 1);
                (&s[1..end], &s[end + 1..])
            } else {
                let end = s
                    .find(char::is_whitespace)
                    .unwrap_or(s.len());
                (&s[..end], &s[end..])
            };
            if !key.is_empty() {
                out.push((key, value.to_owned()));
            }
            s = rest;
        } else {
            // Boolean attribute.
            if !key.is_empty() {
                out.push((key, String::new()));
            }
        }
    }
}

// ── Script interpreter ────────────────────────────────────────────────────────

/// Evaluate a JavaScript source string against a [`JsDomTree`].
///
/// This is a line-by-line interpreter that understands a small subset of the
/// Web DOM API.  It is intentionally simple — its purpose is to enable
/// real-world pages with straightforward DOM manipulation to work, not to be a
/// full JS engine.
///
/// ## Supported statement forms
///
/// ```text
/// var x = document.getElementById("id");
/// var x = document.querySelector(".cls");
/// var x = document.createElement("div");
/// el.textContent = "value";
/// el.innerHTML = "<b>hi</b>";
/// el.setAttribute("name", "value");
/// el.classList.add("cls");
/// el.classList.remove("cls");
/// el.style.setProperty("color", "red");
/// el.appendChild(child);
/// el.addEventListener("click", function() { … });
/// console.log("…");
/// ```
///
/// Returns a [`JsValue`] (the result of the last expression, or `Undefined`).
pub fn eval_script(source: &str, tree: Arc<Mutex<JsDomTree>>) -> JsValue {
    eval_script_with_env(source, tree, &[])
}

/// Like [`eval_script`] but with an optional [`CoreApi`] for network access.
///
/// When `core` is provided, `fetch()` calls are routed through the core's
/// capability system to the network mod.
pub fn eval_script_with_core(
    source: &str,
    tree: Arc<Mutex<JsDomTree>>,
    core: Option<&Arc<dyn CoreApi>>,
) -> JsValue {
    eval_script_with_env_and_core(source, tree, &[], core)
}

/// Like [`eval_script`] but pre-seeds the interpreter's variable environment
/// with the provided `(name, handle)` pairs.
///
/// This is used when executing event-listener callbacks so that variables
/// captured from the enclosing scope (e.g. `btn`) remain accessible.
pub fn eval_script_with_env(
    source: &str,
    tree: Arc<Mutex<JsDomTree>>,
    initial_env: &[(String, ElementHandle)],
) -> JsValue {
    eval_script_with_env_and_core(source, tree, initial_env, None)
}

/// Full-featured script evaluation with environment seeding and optional CoreApi.
pub fn eval_script_with_env_and_core(
    source: &str,
    tree: Arc<Mutex<JsDomTree>>,
    initial_env: &[(String, ElementHandle)],
    core: Option<&Arc<dyn CoreApi>>,
) -> JsValue {
    let mut env: HashMap<String, ElementHandle> = initial_env
        .iter()
        .cloned()
        .collect();
    let mut last_value = JsValue::Undefined;

    // Strip /* … */ block comments.
    let source = strip_block_comments(source);

    for raw_line in source.lines() {
        let line = raw_line.trim();

        // Skip empty lines and // comments.
        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        // Evaluate and update last value.
        last_value = eval_statement(line, &mut env, Arc::clone(&tree), core);
    }

    last_value
}

/// Strip `/* … */` block comments from source (does not handle nested).
fn strip_block_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' {
            if chars.peek() == Some(&'*') {
                chars.next(); // consume '*'
                // Consume until '*/'
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
                out.push(' '); // replace block comment with space
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Evaluate a single statement line.
fn eval_statement(
    line: &str,
    env: &mut HashMap<String, ElementHandle>,
    tree: Arc<Mutex<JsDomTree>>,
    _core: Option<&Arc<dyn CoreApi>>,
) -> JsValue {
    let line = line.trim_end_matches(';').trim();

    // ── Variable declarations ─────────────────────────────────────────────────

    // `var x = …` / `let x = …` / `const x = …`
    if let Some(rest) = strip_var_prefix(line) {
        if let Some((var_name, expr)) = rest.split_once('=') {
            let var_name = var_name.trim().to_owned();
            let expr = expr.trim();
            // Try DOM expression first (returns element handle).
            if let Some(handle) = eval_dom_expr(expr, env, Arc::clone(&tree)) {
                env.insert(var_name, handle);
                return JsValue::Number(handle as f64);
            }
            // Try property read (returns JsValue directly).
            let val = eval_property_read(expr, env, Arc::clone(&tree));
            if !matches!(val, JsValue::Undefined) {
                return val;
            }
        }
    }

    // ── console.log ───────────────────────────────────────────────────────────

    if line.starts_with("console.log(") {
        let inner = extract_call_args(line, "console.log");
        debug!(msg = inner, "console.log");
        return JsValue::Undefined;
    }

    // ── el.textContent = "…" ──────────────────────────────────────────────────

    if let Some((lhs, rhs)) = split_assignment(line) {
        if let Some((var, prop)) = lhs.split_once('.') {
            let var = var.trim();
            let prop = prop.trim();

            match prop {
                "textContent" => {
                    if let Some(&handle) = env.get(var) {
                        let value = unquote(rhs);
                        tree.lock().unwrap().set_text_content(handle, &value);
                        return JsValue::String(value);
                    }
                }
                "innerHTML" => {
                    if let Some(&handle) = env.get(var) {
                        let value = unquote(rhs);
                        tree.lock().unwrap().set_inner_html(handle, &value);
                        return JsValue::String(value);
                    }
                }
                "value" => {
                    if let Some(&handle) = env.get(var) {
                        let value = unquote(rhs);
                        tree.lock().unwrap().set_value(handle, &value);
                        return JsValue::String(value);
                    }
                }
                "checked" => {
                    if let Some(&handle) = env.get(var) {
                        let rhs_trimmed = rhs.trim();
                        let value = rhs_trimmed == "true" || rhs_trimmed == "1";
                        tree.lock().unwrap().set_checked(handle, value);
                        return JsValue::Boolean(value);
                    }
                }
                "disabled" => {
                    if let Some(&handle) = env.get(var) {
                        let rhs_trimmed = rhs.trim();
                        let value = rhs_trimmed == "true" || rhs_trimmed == "1";
                        tree.lock().unwrap().set_disabled(handle, value);
                        return JsValue::Boolean(value);
                    }
                }
                "placeholder" => {
                    if let Some(&handle) = env.get(var) {
                        let value = unquote(rhs);
                        tree.lock().unwrap().set_attribute(handle, "placeholder", &value);
                        return JsValue::String(value);
                    }
                }
                _ => {}
            }
        }
    }

    // ── Method calls ──────────────────────────────────────────────────────────

    // `el.setAttribute("name", "value")`
    if let Some((obj, rest)) = split_method_call(line, "setAttribute") {
        if let Some(&handle) = env.get(obj.trim()) {
            let args = parse_two_string_args(extract_parens_with_delims(&rest));
            if let Some((name, value)) = args {
                tree.lock().unwrap().set_attribute(handle, &name, &value);
            }
        }
        return JsValue::Undefined;
    }

    // `el.getAttribute("name")`
    if let Some((obj, rest)) = split_method_call(line, "getAttribute") {
        if let Some(&handle) = env.get(obj.trim()) {
            let name = unquote(extract_parens_inner(&rest));
            let result = tree.lock().unwrap().get_attribute(handle, &name);
            return match result {
                Some(v) => JsValue::String(v),
                None => JsValue::Null,
            };
        }
        return JsValue::Null;
    }

    // `el.classList.add("cls")`
    if let Some((obj, rest)) = split_method_call(line, "classList.add") {
        if let Some(&handle) = env.get(obj.trim()) {
            let cls = unquote(extract_parens_inner(&rest));
            tree.lock().unwrap().class_list_add(handle, &cls);
        }
        return JsValue::Undefined;
    }

    // `el.classList.remove("cls")`
    if let Some((obj, rest)) = split_method_call(line, "classList.remove") {
        if let Some(&handle) = env.get(obj.trim()) {
            let cls = unquote(extract_parens_inner(&rest));
            tree.lock().unwrap().class_list_remove(handle, &cls);
        }
        return JsValue::Undefined;
    }

    // `el.style.setProperty("name", "value")`
    if let Some((obj, rest)) = split_method_call(line, "style.setProperty") {
        if let Some(&handle) = env.get(obj.trim()) {
            let args = parse_two_string_args(extract_parens_with_delims(&rest));
            if let Some((name, value)) = args {
                tree.lock().unwrap().style_set_property(handle, &name, &value);
            }
        }
        return JsValue::Undefined;
    }

    // `el.appendChild(child)`
    if let Some((obj, rest)) = split_method_call(line, "appendChild") {
        if let Some(&parent_handle) = env.get(obj.trim()) {
            let arg = extract_parens_inner(&rest).trim();
            if let Some(&child_handle) = env.get(arg) {
                tree.lock().unwrap().append_child(parent_handle, child_handle);
            }
        }
        return JsValue::Undefined;
    }

    // `el.addEventListener("type", function() { … })`
    if let Some((obj, rest)) = split_method_call(line, "addEventListener") {
        if let Some(&handle) = env.get(obj.trim()) {
            if let Some((event_type, cb_body)) = parse_event_listener_args(extract_parens_with_delims(&rest)) {
                // Capture the current scope so the callback can reference the
                // same variables by name when it executes later.
                let captured: Vec<(String, ElementHandle)> = env
                    .iter()
                    .map(|(k, &v)| (k.clone(), v))
                    .collect();
                tree.lock()
                    .unwrap()
                    .add_event_listener(handle, &event_type, &cb_body, captured, false);
            }
        }
        return JsValue::Undefined;
    }

    // ── el.remove() ────────────────────────────────────────────────────────────

    if let Some((obj, _rest)) = split_method_call(line, "remove") {
        // Make sure it's `el.remove()` not `el.removeChild(…)` etc.
        let rest_trimmed = _rest.trim();
        if rest_trimmed == "remove()" || rest_trimmed == "remove( )" {
            if let Some(&handle) = env.get(obj.trim()) {
                tree.lock().unwrap().remove_element(handle);
            }
            return JsValue::Undefined;
        }
    }

    // `el.replaceWith(newEl)`
    if let Some((obj, rest)) = split_method_call(line, "replaceWith") {
        if let Some(&handle) = env.get(obj.trim()) {
            let arg = extract_parens_inner(&rest).trim();
            if let Some(&new_handle) = env.get(arg) {
                tree.lock().unwrap().replace_with(handle, new_handle);
            }
        }
        return JsValue::Undefined;
    }

    // `el.after(newEl)`
    if let Some((obj, rest)) = split_method_call(line, "after") {
        if let Some(&handle) = env.get(obj.trim()) {
            let arg = extract_parens_inner(&rest).trim();
            if let Some(&new_handle) = env.get(arg) {
                tree.lock().unwrap().insert_after(handle, new_handle);
            }
        }
        return JsValue::Undefined;
    }

    // `el.before(newEl)`
    if let Some((obj, rest)) = split_method_call(line, "before") {
        if let Some(&handle) = env.get(obj.trim()) {
            let arg = extract_parens_inner(&rest).trim();
            if let Some(&new_handle) = env.get(arg) {
                tree.lock().unwrap().insert_before_self(handle, new_handle);
            }
        }
        return JsValue::Undefined;
    }

    // `el.append(child1, child2, …)`
    if let Some((obj, rest)) = split_method_call(line, "append") {
        // Avoid matching `appendChild`
        let rest_trimmed = rest.trim();
        if rest_trimmed.starts_with("append(") && !rest_trimmed.starts_with("appendChild(") {
            if let Some(&parent_handle) = env.get(obj.trim()) {
                let inner = extract_parens_inner(&rest);
                let child_handles: Vec<ElementHandle> = inner
                    .split(',')
                    .filter_map(|arg| env.get(arg.trim()).copied())
                    .collect();
                tree.lock().unwrap().append_multiple(parent_handle, &child_handles);
            }
            return JsValue::Undefined;
        }
    }

    // `el.prepend(child1, child2, …)`
    if let Some((obj, rest)) = split_method_call(line, "prepend") {
        if let Some(&parent_handle) = env.get(obj.trim()) {
            let inner = extract_parens_inner(&rest);
            let child_handles: Vec<ElementHandle> = inner
                .split(',')
                .filter_map(|arg| env.get(arg.trim()).copied())
                .collect();
            tree.lock().unwrap().prepend_multiple(parent_handle, &child_handles);
        }
        return JsValue::Undefined;
    }

    // ── No-op stubs: focus(), blur(), submit() ────────────────────────────────

    if let Some((obj, _rest)) = split_method_call(line, "focus") {
        if env.contains_key(obj.trim()) {
            debug!(obj = obj.trim(), "focus() stub — no-op");
        }
        return JsValue::Undefined;
    }

    if let Some((obj, _rest)) = split_method_call(line, "blur") {
        if env.contains_key(obj.trim()) {
            debug!(obj = obj.trim(), "blur() stub — no-op");
        }
        return JsValue::Undefined;
    }

    if let Some((obj, _rest)) = split_method_call(line, "submit") {
        if env.contains_key(obj.trim()) {
            warn!(obj = obj.trim(), "form.submit() called — no-op stub");
        }
        return JsValue::Undefined;
    }

    // ── window.getComputedStyle(el) in var assignment ──────────────────────────
    // Handled in eval_dom_expr for var declarations; property reads handled below.

    // Unrecognised — return undefined.
    JsValue::Undefined
}

/// Evaluate a DOM-valued expression (returns an element handle).
fn eval_dom_expr(
    expr: &str,
    env: &mut HashMap<String, ElementHandle>,
    tree: Arc<Mutex<JsDomTree>>,
) -> Option<ElementHandle> {
    let expr = expr.trim();

    // `document.getElementById("id")`
    if expr.starts_with("document.getElementById(") {
        let arg = extract_call_args(expr, "document.getElementById");
        let id = unquote(arg);
        return tree.lock().unwrap().get_element_by_id(&id);
    }

    // `document.querySelector("sel")`
    if expr.starts_with("document.querySelector(") {
        let arg = extract_call_args(expr, "document.querySelector");
        let sel = unquote(arg);
        return tree.lock().unwrap().query_selector(&sel);
    }

    // `document.createElement("tag")`
    if expr.starts_with("document.createElement(") {
        let arg = extract_call_args(expr, "document.createElement");
        let tag = unquote(arg);
        let handle = tree.lock().unwrap().create_element(&tag);
        return Some(handle);
    }

    // `window.getComputedStyle(el)` — we store the result as a special handle
    // that refers to the same element (computed style reads delegate to the element).
    if expr.starts_with("window.getComputedStyle(") || expr.starts_with("getComputedStyle(") {
        let fn_name = if expr.starts_with("window.") {
            "window.getComputedStyle"
        } else {
            "getComputedStyle"
        };
        let arg = extract_call_args(expr, fn_name).trim();
        if let Some(&handle) = env.get(arg) {
            // Return the element handle — computed style property reads will
            // resolve through `get_computed_style` on the tree.
            return Some(handle);
        }
    }

    // Variable reference.
    if let Some(&handle) = env.get(expr) {
        return Some(handle);
    }

    None
}

/// Evaluate a property-read expression that returns a [`JsValue`] rather than
/// an element handle.
///
/// Handles patterns like `el.value`, `el.checked`, `el.type`, `el.name`,
/// `el.placeholder`, `el.disabled`, `el.offsetWidth`, `el.offsetHeight`,
/// `el.offsetLeft`, `el.offsetTop`, `el.scrollWidth`, `el.scrollHeight`,
/// `el.scrollLeft`, `el.scrollTop`, `el.clientWidth`, `el.clientHeight`.
fn eval_property_read(
    expr: &str,
    env: &HashMap<String, ElementHandle>,
    tree: Arc<Mutex<JsDomTree>>,
) -> JsValue {
    let expr = expr.trim();

    // `obj.prop` pattern.
    if let Some((obj, prop)) = expr.rsplit_once('.') {
        let obj = obj.trim();
        let prop = prop.trim();

        if let Some(&handle) = env.get(obj) {
            let t = tree.lock().unwrap();
            match prop {
                "value" => return JsValue::String(t.get_value(handle)),
                "checked" => return JsValue::Boolean(t.get_checked(handle)),
                "disabled" => return JsValue::Boolean(t.get_disabled(handle)),
                "type" => {
                    return JsValue::String(
                        t.get_attribute(handle, "type").unwrap_or_default(),
                    );
                }
                "name" => {
                    return JsValue::String(
                        t.get_attribute(handle, "name").unwrap_or_default(),
                    );
                }
                "placeholder" => {
                    return JsValue::String(
                        t.get_attribute(handle, "placeholder").unwrap_or_default(),
                    );
                }
                "offsetWidth" | "clientWidth" | "scrollWidth" => {
                    return JsValue::Number(t.get_offset_width(handle));
                }
                "offsetHeight" | "clientHeight" | "scrollHeight" => {
                    return JsValue::Number(t.get_offset_height(handle));
                }
                "offsetLeft" | "offsetTop" | "scrollLeft" | "scrollTop" => {
                    return JsValue::Number(0.0);
                }
                "textContent" => {
                    return JsValue::String(t.get_text_content(handle));
                }
                "innerHTML" => {
                    return JsValue::String(t.get_inner_html(handle));
                }
                _ => {}
            }
        }
    }

    JsValue::Undefined
}

// ── Parsing helpers ───────────────────────────────────────────────────────────

/// Strip a `var`/`let`/`const` prefix and return the rest.
fn strip_var_prefix(line: &str) -> Option<&str> {
    for kw in &["var ", "let ", "const "] {
        if let Some(rest) = line.strip_prefix(kw) {
            return Some(rest);
        }
    }
    None
}

/// Split `lhs = rhs` on the first `=` that is not `==` or `!=` or `<=` or `>=`.
fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            // Ensure it's not ==, !=, <=, >=.
            let prev = if i > 0 { bytes[i - 1] } else { 0 };
            let next = bytes.get(i + 1).copied().unwrap_or(0);
            if prev != b'!' && prev != b'<' && prev != b'>' && prev != b'=' && next != b'=' {
                return Some((&line[..i], &line[i + 1..]));
            }
        }
        i += 1;
    }
    None
}

/// Split `obj.method(…)` into `(obj, rest_from_method)`.
///
/// `rest_from_method` starts at the method name (e.g. `"setAttribute(\"x\",\"y\")"`)
/// so callers can use [`extract_parens_inner`] / [`extract_parens_with_delims`] to
/// get the argument list.
fn split_method_call<'a>(line: &'a str, method: &str) -> Option<(&'a str, &'a str)> {
    let pattern = format!(".{method}(");
    if let Some(pos) = line.find(&pattern) {
        let obj = &line[..pos];
        // rest starts at the method name (skip the leading `.`)
        let rest = &line[pos + 1..]; // "method(…)"
        Some((obj, rest))
    } else {
        None
    }
}

/// Given `"method(inner)"`, return `"inner"` (the content between the first `(` and
/// the matching `)` from the end).
fn extract_parens_inner(s: &str) -> &str {
    let start = s.find('(').map(|i| i + 1).unwrap_or(0);
    let end = s.rfind(')').unwrap_or(s.len());
    if start <= end { &s[start..end] } else { "" }
}

/// Given `"method(inner)"`, return `"(inner)"` — the arg list including delimiters.
/// Used when callers need to pass to [`parse_two_string_args`] or
/// [`parse_event_listener_args`] which expect the outer parens.
fn extract_parens_with_delims(s: &str) -> &str {
    let start = s.find('(').unwrap_or(s.len());
    if start < s.len() { &s[start..] } else { "()" }
}

/// Extract argument string from `fn_name(…)`.
fn extract_call_args<'a>(call: &'a str, fn_name: &str) -> &'a str {
    let prefix = format!("{fn_name}(");
    let start = match call.find(&prefix) {
        Some(p) => p + prefix.len(),
        None => return "",
    };
    let end = call.rfind(')').unwrap_or(call.len());
    &call[start..end]
}

/// Remove surrounding quotes from a string literal.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"'))
        || (s.starts_with('\'') && s.ends_with('\''))
        || (s.starts_with('`') && s.ends_with('`'))
    {
        s[1..s.len() - 1].to_owned()
    } else {
        s.to_owned()
    }
}

/// Parse two comma-separated quoted string arguments from `("a", "b")`.
fn parse_two_string_args(args_with_parens: &str) -> Option<(String, String)> {
    let inner = args_with_parens
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');

    // Find the comma that separates the two arguments (not inside quotes).
    let mut in_quote: Option<char> = None;
    let mut split_pos = None;
    for (i, c) in inner.char_indices() {
        match c {
            '"' | '\'' | '`' => {
                if in_quote == Some(c) {
                    in_quote = None;
                } else if in_quote.is_none() {
                    in_quote = Some(c);
                }
            }
            ',' if in_quote.is_none() => {
                split_pos = Some(i);
                break;
            }
            _ => {}
        }
    }

    let pos = split_pos?;
    let first = unquote(inner[..pos].trim());
    let second = unquote(inner[pos + 1..].trim());
    Some((first, second))
}

/// Parse `addEventListener` arguments: `("type", function() { body })`.
///
/// Returns `(event_type, callback_body)`.
fn parse_event_listener_args(args_with_parens: &str) -> Option<(String, String)> {
    let inner = args_with_parens
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');

    // First argument is the event type (a quoted string).
    let comma_pos = inner.find(',')?;
    let event_type = unquote(inner[..comma_pos].trim());
    let cb_part = inner[comma_pos + 1..].trim();

    // The callback can be `function() { … }` or `() => { … }` or `function(e) { … }`.
    // Extract the body between the outermost `{` and `}`.
    let body_start = cb_part.find('{').map(|i| i + 1).unwrap_or(0);
    let body_end = cb_part.rfind('}').unwrap_or(cb_part.len());
    let body = &cb_part[body_start..body_end];

    Some((event_type, body.to_owned()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::content::DomNode;

    fn make_simple_dom() -> DomNode {
        DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "body".into(),
                    attributes: vec![],
                    children: vec![
                        DomNode::Element {
                            tag: "div".into(),
                            attributes: vec![("id".into(), "main".into())],
                            children: vec![DomNode::Text("Hello".into())],
                        },
                        DomNode::Element {
                            tag: "p".into(),
                            attributes: vec![("class".into(), "intro".into())],
                            children: vec![DomNode::Text("World".into())],
                        },
                    ],
                }],
            }],
        }
    }

    #[test]
    fn get_element_by_id() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let tree = tree_arc.lock().unwrap();
        assert!(tree.get_element_by_id("main").is_some());
        assert!(tree.get_element_by_id("nope").is_none());
    }

    #[test]
    fn query_selector_class() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let tree = tree_arc.lock().unwrap();
        assert!(tree.query_selector(".intro").is_some());
    }

    #[test]
    fn query_selector_tag() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let tree = tree_arc.lock().unwrap();
        assert!(tree.query_selector("div").is_some());
    }

    #[test]
    fn set_text_content() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let handle = {
            let tree = tree_arc.lock().unwrap();
            tree.get_element_by_id("main").unwrap()
        };
        {
            let mut tree = tree_arc.lock().unwrap();
            tree.set_text_content(handle, "Updated!");
        }
        let tree = tree_arc.lock().unwrap();
        assert_eq!(tree.get_text_content(handle), "Updated!");
    }

    #[test]
    fn set_attribute() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let handle = {
            let tree = tree_arc.lock().unwrap();
            tree.get_element_by_id("main").unwrap()
        };
        {
            let mut tree = tree_arc.lock().unwrap();
            tree.set_attribute(handle, "data-x", "42");
        }
        let tree = tree_arc.lock().unwrap();
        assert_eq!(tree.get_attribute(handle, "data-x"), Some("42".into()));
    }

    #[test]
    fn class_list_add_remove() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let handle = {
            let tree = tree_arc.lock().unwrap();
            tree.query_selector(".intro").unwrap()
        };
        {
            let mut tree = tree_arc.lock().unwrap();
            tree.class_list_add(handle, "active");
        }
        {
            let tree = tree_arc.lock().unwrap();
            assert_eq!(tree.get_attribute(handle, "class"), Some("intro active".into()));
        }
        {
            let mut tree = tree_arc.lock().unwrap();
            tree.class_list_remove(handle, "intro");
        }
        let tree = tree_arc.lock().unwrap();
        assert_eq!(tree.get_attribute(handle, "class"), Some("active".into()));
    }

    #[test]
    fn style_set_property() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let handle = {
            let tree = tree_arc.lock().unwrap();
            tree.get_element_by_id("main").unwrap()
        };
        {
            let mut tree = tree_arc.lock().unwrap();
            tree.style_set_property(handle, "color", "red");
        }
        // After export the style attribute should contain the property.
        let dom = tree_arc.lock().unwrap().to_dom();
        let doc_str = format!("{dom:?}");
        assert!(doc_str.contains("color:red"), "expected color:red in {doc_str}");
    }

    #[test]
    fn append_child() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let (parent, child) = {
            let mut tree = tree_arc.lock().unwrap();
            let parent = tree.get_element_by_id("main").unwrap();
            let child = tree.create_element("span");
            tree.set_text_content(child, "New span");
            (parent, child)
        };
        tree_arc.lock().unwrap().append_child(parent, child);
        let tree = tree_arc.lock().unwrap();
        let text = tree.get_text_content(parent);
        assert!(text.contains("New span"), "expected 'New span' in '{text}'");
    }

    #[test]
    fn event_listener_dispatch() {
        let tree_arc = JsDomTree::from_dom(&make_simple_dom());
        let handle = {
            let tree = tree_arc.lock().unwrap();
            tree.get_element_by_id("main").unwrap()
        };
        {
            let mut tree = tree_arc.lock().unwrap();
            tree.add_event_listener(handle, "click", "console.log('clicked');", vec![], false);
        }
        let callbacks = tree_arc.lock().unwrap().dispatch_event(handle, "click");
        assert_eq!(callbacks.len(), 1);
        assert!(callbacks[0].0.contains("clicked"));
    }

    #[test]
    fn eval_script_set_text_content() {
        let dom = make_simple_dom();
        let tree = JsDomTree::from_dom(&dom);

        let script = r#"
            var el = document.getElementById("main");
            el.textContent = "From JS";
        "#;
        eval_script(script, Arc::clone(&tree));

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("main").unwrap();
        assert_eq!(t.get_text_content(handle), "From JS");
    }

    #[test]
    fn eval_script_create_and_append() {
        let dom = make_simple_dom();
        let tree = JsDomTree::from_dom(&dom);

        let script = r#"
            var parent = document.getElementById("main");
            var span = document.createElement("span");
            span.textContent = "Appended";
            parent.appendChild(span);
        "#;
        eval_script(script, Arc::clone(&tree));

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("main").unwrap();
        let text = t.get_text_content(handle);
        assert!(text.contains("Appended"), "expected Appended in '{text}'");
    }

    #[test]
    fn eval_script_class_manipulation() {
        let dom = make_simple_dom();
        let tree = JsDomTree::from_dom(&dom);

        let script = r#"
            var el = document.querySelector(".intro");
            el.classList.add("active");
            el.classList.remove("intro");
        "#;
        eval_script(script, Arc::clone(&tree));

        let t = tree.lock().unwrap();
        let handle = t.query_selector(".active").expect("should have .active class");
        assert_eq!(t.get_attribute(handle, "class"), Some("active".into()));
    }

    #[test]
    fn to_dom_roundtrip() {
        let dom = make_simple_dom();
        let tree = JsDomTree::from_dom(&dom);
        let exported = tree.lock().unwrap().to_dom();
        // Should still have the main div.
        let tree2 = JsDomTree::from_dom(&exported);
        assert!(tree2.lock().unwrap().get_element_by_id("main").is_some());
    }
}

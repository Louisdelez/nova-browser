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

use crate::fetch_api;

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
    nodes: HashMap<ElementHandle, JsElement>,
    /// The root document handle (always 0).
    root: ElementHandle,
    /// Next handle to allocate.
    next_handle: ElementHandle,
    /// Event listeners: (handle, event_type) → list of callbacks.
    listeners: HashMap<(ElementHandle, String), Vec<EventCallback>>,
    /// Log of pending callbacks that were triggered (for the interpreter loop).
    pub pending_events: Vec<(ElementHandle, String)>,
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

    // ── DOM query API ──────────────────────────────────────────────────────────

    /// `document.getElementById(id)` — returns `None` if not found.
    pub fn get_element_by_id(&self, id: &str) -> Option<ElementHandle> {
        self.find_by_attr("id", id, self.root)
    }

    /// `document.querySelector(selector)` — very simple CSS-selector subset:
    /// supports `#id`, `.class`, and `tag` selectors.
    pub fn query_selector(&self, selector: &str) -> Option<ElementHandle> {
        let selector = selector.trim();
        if selector.starts_with('#') {
            self.find_by_attr("id", &selector[1..], self.root)
        } else if selector.starts_with('.') {
            self.find_by_class(&selector[1..], self.root)
        } else {
            self.find_by_tag(selector, self.root)
        }
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
    fn parse_html_fragment(&mut self, html: &str) -> Vec<ElementHandle> {
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
    ///
    /// If `capture` is `true`, the listener fires during the capture phase
    /// rather than the bubble phase.
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

    // ── Event system helpers ──────────────────────────────────────────────────

    /// Build the path from the target element up to the document root.
    ///
    /// Returns `[target, parent, grandparent, …, root]`. If the target has no
    /// parent (i.e., it is the root or disconnected), returns `[target]`.
    pub fn build_ancestor_path(&self, target: ElementHandle) -> Vec<ElementHandle> {
        let mut path = vec![target];
        let mut current = target;
        while let Some(parent) = self.find_parent(current) {
            path.push(parent);
            current = parent;
        }
        path
    }

    /// Find the parent of a node by scanning all nodes for one that has
    /// `handle` in its children list.
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
/// fetch("url", { method: "POST", headers: {...}, body: "..." });
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
///
/// Combines the features of [`eval_script_with_env`] and [`eval_script_with_core`].
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
    core: Option<&Arc<dyn CoreApi>>,
) -> JsValue {
    let line = line.trim_end_matches(';').trim();

    // ── Variable declarations ─────────────────────────────────────────────────

    // `var x = …` / `let x = …` / `const x = …`
    if let Some(rest) = strip_var_prefix(line) {
        if let Some((var_name, expr)) = rest.split_once('=') {
            let var_name = var_name.trim().to_owned();
            let expr = expr.trim();
            if let Some(handle) = eval_dom_expr(expr, env, Arc::clone(&tree)) {
                env.insert(var_name, handle);
                return JsValue::Number(handle as f64);
            }
        }
    }

    // ── fetch() ───────────────────────────────────────────────────────────────

    if line.starts_with("fetch(") {
        let args = extract_call_args(line, "fetch");
        return eval_fetch(args, core);
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

    // Variable reference.
    if let Some(&handle) = env.get(expr) {
        return Some(handle);
    }

    None
}

/// Evaluate a `fetch()` call and return the response as a JsValue.
///
/// If no CoreApi is available, returns an error object.  Otherwise, performs
/// a synchronous HTTP request through the core's capability routing.
fn eval_fetch(args: &str, core: Option<&Arc<dyn CoreApi>>) -> JsValue {
    let Some(core) = core else {
        warn!("fetch() called but no CoreApi available");
        return JsValue::Object(vec![
            ("error".to_string(), JsValue::String("fetch: no network available".to_string())),
        ]);
    };

    let (url, method, headers, body) = fetch_api::parse_fetch_call(args);
    debug!(url = %url, method = %method, "fetch() called from JS");

    match fetch_api::execute_fetch(core, &url, &method, headers, body.as_deref()) {
        Ok(resp) => {
            // Return a Response-like object.
            let headers_obj: Vec<(String, JsValue)> = resp
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), JsValue::String(v.clone())))
                .collect();

            JsValue::Object(vec![
                ("status".to_string(), JsValue::Number(resp.status as f64)),
                ("ok".to_string(), JsValue::Boolean(resp.status >= 200 && resp.status < 300)),
                ("url".to_string(), JsValue::String(resp.url)),
                ("headers".to_string(), JsValue::Object(headers_obj)),
                ("body".to_string(), JsValue::String(resp.body)),
            ])
        }
        Err(e) => {
            warn!(error = %e, "fetch() failed");
            JsValue::Object(vec![
                ("error".to_string(), JsValue::String(e)),
            ])
        }
    }
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

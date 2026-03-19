//! # mod-html-parser
//!
//! NOVA Mod for HTML5 parsing using `html5ever`. Handles the
//! `ParseDocument("text/html")` capability.
//!
//! Takes raw HTML bytes via `ContentRequest::Parse`, parses them through
//! html5ever with a custom `TreeSink`, and returns a `TypedData::Dom` tree.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{parse_document, Attribute, ExpandedName, QualName};
use semver::Version;
use tracing::{debug, info};

use nova_mod_api::{
    capability::CapabilityType,
    content::{ContentRequest, DomNode, TypedData},
    error::NovaError,
    manifest::ModManifest,
    permission::TrustLevel,
    trigger::{ContentTrigger, TriggerCondition},
    types::ModId,
    CoreApi, NovaMod,
};

/// The HTML parser mod.
pub struct HtmlParserMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
}

impl HtmlParserMod {
    /// Create a new `HtmlParserMod` instance.
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.html-parser"),
            name: "NOVA HTML Parser".into(),
            version: Version::new(0, 1, 0),
            description: "HTML5 parser powered by html5ever".into(),
            capabilities: vec![CapabilityType::ParseDocument("text/html".into())],
            permissions: vec![],
            dependencies: vec![],
            triggers: vec![ContentTrigger {
                condition: TriggerCondition::MimeType("text/html".into()),
                mod_id: ModId::new("org.nova.html-parser"),
                priority: 100,
            }],
            min_core_version: Version::new(0, 1, 0),
            trust_level: TrustLevel::Core,
        };

        Self {
            manifest,
            core: None,
        }
    }
}

impl Default for HtmlParserMod {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NovaMod for HtmlParserMod {
    fn manifest(&self) -> &ModManifest {
        &self.manifest
    }

    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "html-parser mod initializing");
        self.core = Some(core);
        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            ContentRequest::Parse { data, mime_type } => {
                if mime_type != "text/html" {
                    return Err(NovaError::UnsupportedContent(format!(
                        "html-parser cannot handle MIME type: {mime_type}"
                    )));
                }

                let html = String::from_utf8_lossy(&data).into_owned();

                debug!(len = html.len(), "parsing HTML document");

                let dom = parse_html(&html)?;
                Ok(TypedData::Dom(dom))
            }
            other => Err(NovaError::UnsupportedContent(format!(
                "html-parser cannot handle request: {other:?}"
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "html-parser mod shutting down");
        Ok(())
    }
}

/// Parse an HTML string into a `DomNode` tree using html5ever.
fn parse_html(html: &str) -> Result<DomNode, NovaError> {
    let sink = NovaSink::new();
    let parser = parse_document(sink, Default::default());
    let sink = parser.one(html);
    Ok(sink.into_dom())
}

// ── Custom TreeSink that builds DomNode ──────────────────────────────────

/// Internal node representation during parsing.
#[derive(Debug)]
enum SinkNode {
    Document {
        children: Vec<usize>,
    },
    Element {
        tag: String,
        attributes: Vec<(String, String)>,
        children: Vec<usize>,
    },
    Text(String),
    Comment(String),
}

/// A `TreeSink` that builds a flat arena of `SinkNode`s, then converts to `DomNode`.
///
/// html5ever 0.29's `TreeSink` uses `&self` for all methods, so we use
/// `RefCell`/`Cell` for interior mutability. We store leaked `QualName`s to
/// satisfy the lifetime requirements of `elem_name`.
struct NovaSink {
    /// Node arena. Index 0 is always the document node.
    nodes: RefCell<Vec<SinkNode>>,
    /// Maps element node IDs to their leaked QualName for `elem_name()`.
    names: RefCell<HashMap<usize, &'static QualName>>,
    /// Next node ID counter.
    next_id: Cell<usize>,
}

impl NovaSink {
    fn new() -> Self {
        let nodes = vec![SinkNode::Document {
            children: Vec::new(),
        }];
        Self {
            nodes: RefCell::new(nodes),
            names: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
        }
    }

    /// Allocate a new node and return its ID.
    fn alloc(&self, node: SinkNode) -> usize {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let mut nodes = self.nodes.borrow_mut();
        // Ensure the vec is large enough (IDs are sequential from 1).
        while nodes.len() <= id {
            nodes.push(SinkNode::Comment(String::new()));
        }
        nodes[id] = node;
        id
    }

    /// Append a child ID to a parent node.
    fn append_child_id(&self, parent: usize, child: usize) {
        let mut nodes = self.nodes.borrow_mut();
        match &mut nodes[parent] {
            SinkNode::Document { children } | SinkNode::Element { children, .. } => {
                children.push(child);
            }
            _ => {}
        }
    }

    /// Recursively convert a `SinkNode` to a `DomNode`.
    fn to_dom_node(nodes: &[SinkNode], id: usize) -> DomNode {
        match &nodes[id] {
            SinkNode::Document { children } => DomNode::Document {
                children: children
                    .iter()
                    .map(|&c| Self::to_dom_node(nodes, c))
                    .collect(),
            },
            SinkNode::Element {
                tag,
                attributes,
                children,
            } => DomNode::Element {
                tag: tag.clone(),
                attributes: attributes.clone(),
                children: children
                    .iter()
                    .map(|&c| Self::to_dom_node(nodes, c))
                    .collect(),
            },
            SinkNode::Text(text) => DomNode::Text(text.clone()),
            SinkNode::Comment(text) => DomNode::Comment(text.clone()),
        }
    }

    /// Convert the entire sink into a `DomNode` tree.
    fn into_dom(self) -> DomNode {
        let nodes = self.nodes.into_inner();
        Self::to_dom_node(&nodes, 0)
    }

    /// Find the parent of a given node by searching all nodes.
    fn find_parent(nodes: &[SinkNode], target: usize) -> Option<usize> {
        for (i, node) in nodes.iter().enumerate() {
            let children = match node {
                SinkNode::Document { children } | SinkNode::Element { children, .. } => children,
                _ => continue,
            };
            if children.contains(&target) {
                return Some(i);
            }
        }
        None
    }
}

impl TreeSink for NovaSink {
    type Handle = usize;
    type Output = Self;
    type ElemName<'a> = ExpandedName<'a>;

    fn finish(self) -> Self::Output {
        self
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        debug!(error = %msg, "html5ever parse error (non-fatal)");
    }

    fn get_document(&self) -> usize {
        0
    }

    fn elem_name(&self, target: &usize) -> ExpandedName<'_> {
        self.names
            .borrow()
            .get(target)
            .expect("elem_name called on non-element")
            .expanded()
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> usize {
        let tag = name.local.to_string();
        let attributes = attrs
            .into_iter()
            .map(|a| (a.name.local.to_string(), a.value.to_string()))
            .collect();
        let id = self.alloc(SinkNode::Element {
            tag,
            attributes,
            children: Vec::new(),
        });
        // Store the leaked QualName so `elem_name` can return a reference.
        self.names
            .borrow_mut()
            .insert(id, Box::leak(Box::new(name)));
        id
    }

    fn create_comment(&self, text: StrTendril) -> usize {
        self.alloc(SinkNode::Comment(text.to_string()))
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> usize {
        // Processing instructions are rare in HTML; store as empty comment.
        self.alloc(SinkNode::Comment(String::new()))
    }

    fn append(&self, parent: &usize, child: NodeOrText<usize>) {
        match child {
            NodeOrText::AppendNode(id) => {
                self.append_child_id(*parent, id);
            }
            NodeOrText::AppendText(text) => {
                // Try to merge with the last child if it's also text.
                let merge_target = {
                    let nodes = self.nodes.borrow();
                    match &nodes[*parent] {
                        SinkNode::Document { children }
                        | SinkNode::Element { children, .. } => children
                            .last()
                            .and_then(|&last| {
                                if matches!(&nodes[last], SinkNode::Text(_)) {
                                    Some(last)
                                } else {
                                    None
                                }
                            }),
                        _ => None,
                    }
                };

                if let Some(last) = merge_target {
                    let mut nodes = self.nodes.borrow_mut();
                    if let SinkNode::Text(existing) = &mut nodes[last] {
                        existing.push_str(&text);
                        return;
                    }
                }

                let id = self.alloc(SinkNode::Text(text.to_string()));
                self.append_child_id(*parent, id);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &usize,
        _prev_element: &usize,
        child: NodeOrText<usize>,
    ) {
        self.append(element, child);
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
        // We don't store doctype nodes for now.
    }

    fn get_template_contents(&self, target: &usize) -> usize {
        // Template elements are treated as their own container.
        *target
    }

    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {
        // Quirks mode tracking not needed yet.
    }

    fn append_before_sibling(&self, sibling: &usize, new_node: NodeOrText<usize>) {
        // Find the parent of the sibling, then append there.
        // True insertion-before-sibling ordering is approximate for now.
        let parent = {
            let nodes = self.nodes.borrow();
            Self::find_parent(&nodes, *sibling)
        };
        if let Some(parent) = parent {
            self.append(&parent, new_node);
        }
    }

    fn add_attrs_if_missing(&self, target: &usize, attrs: Vec<Attribute>) {
        let mut nodes = self.nodes.borrow_mut();
        if let SinkNode::Element {
            attributes, ..
        } = &mut nodes[*target]
        {
            for attr in attrs {
                let name = attr.name.local.to_string();
                if !attributes.iter().any(|(k, _)| k == &name) {
                    attributes.push((name, attr.value.to_string()));
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &usize) {
        let mut nodes = self.nodes.borrow_mut();
        let parent = Self::find_parent(&nodes, *target);
        if let Some(parent) = parent {
            match &mut nodes[parent] {
                SinkNode::Document { children } | SinkNode::Element { children, .. } => {
                    children.retain(|&c| c != *target);
                }
                _ => {}
            }
        }
    }

    fn reparent_children(&self, node: &usize, new_parent: &usize) {
        let mut nodes = self.nodes.borrow_mut();
        let children: Vec<usize> = match &mut nodes[*node] {
            SinkNode::Document { children } | SinkNode::Element { children, .. } => {
                std::mem::take(children)
            }
            _ => return,
        };
        match &mut nodes[*new_parent] {
            SinkNode::Document {
                children: parent_children,
            }
            | SinkNode::Element {
                children: parent_children,
                ..
            } => {
                parent_children.extend(children);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_html() {
        let dom = parse_html("<html><body><h1>Hello</h1></body></html>").unwrap();
        match &dom {
            DomNode::Document { children } => {
                assert!(!children.is_empty(), "document should have children");
            }
            _ => panic!("expected Document node"),
        }
    }

    #[test]
    fn manifest_provides_html() {
        let m = HtmlParserMod::new();
        assert!(m
            .manifest()
            .provides(&CapabilityType::ParseDocument("text/html".into())));
    }

    #[tokio::test]
    async fn rejects_non_html_mime() {
        let m = HtmlParserMod::new();
        let req = ContentRequest::Parse {
            data: bytes::Bytes::from("{}"),
            mime_type: "application/json".into(),
        };
        assert!(m.handle(req).await.is_err());
    }
}

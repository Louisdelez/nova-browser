//! Content requests and typed data — the universal language between core and mods.
//!
//! Mods never exchange Rust types directly with each other.
//! All data flows through `TypedData`, and all requests go through `ContentRequest`.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::capability::CapabilityType;
use crate::types::{GpuTextureHandle, RenderCommands, Viewport};

/// A request from a mod (or the pipeline) to the core.
/// The core routes it to the appropriate mod based on the capability.
#[derive(Debug, Clone)]
pub enum ContentRequest {
    /// Fetch a URL (routed to a protocol handler mod).
    Fetch {
        url: String,
        headers: Vec<(String, String)>,
    },

    /// Parse a document (routed based on MIME type).
    Parse {
        data: Bytes,
        mime_type: String,
    },

    /// Parse a CSS stylesheet.
    ParseCss {
        source: String,
        base_url: Option<String>,
    },

    /// Execute JavaScript code.
    ExecScript {
        source: String,
        context_id: Option<u64>,
    },

    /// Execute JavaScript code with a live DOM tree for mutation.
    ExecScriptWithDom {
        source: String,
        dom: Box<DomNode>,
        context_id: Option<u64>,
    },

    /// Dispatch a browser event to a JS context.
    DispatchEvent {
        element_handle: u64,
        event_type: String,
        context_id: u64,
    },

    /// Decode an image.
    DecodeImage {
        data: Bytes,
        format_hint: Option<String>,
    },

    /// Decode a video frame.
    DecodeVideo {
        data: Bytes,
        codec: String,
    },

    /// Compute styles for a DOM tree.
    ComputeStyles {
        dom: Box<TypedData>,
        stylesheets: Vec<TypedData>,
        /// Viewport width in CSS pixels, used for `@media` query evaluation.
        viewport_width: f32,
    },

    /// Perform layout.
    Layout {
        styled_dom: Box<TypedData>,
        viewport: Viewport,
    },

    /// Paint a layout tree into render commands.
    Paint {
        layout_tree: Box<TypedData>,
        /// Decoded image data keyed by source URL.
        /// Each entry is `(src_url, decoded_rgba_bytes)` where the bytes use
        /// the mod-image wire format: `[width_u32_le][height_u32_le][rgba…]`.
        images: Vec<(String, Vec<u8>)>,
    },

    /// Generic capability request (for extension capabilities).
    Custom {
        capability: CapabilityType,
        payload: TypedData,
    },
}

/// The universal data exchange format between mods.
/// Defined by the core — mods speak this language, they don't invent their own.
#[derive(Debug, Clone)]
pub enum TypedData {
    /// Raw bytes (zero-copy when possible).
    Bytes(Bytes),
    /// UTF-8 text.
    Text(String),
    /// JSON-structured data.
    Json(serde_json::Value),

    /// DOM tree representation.
    Dom(DomNode),
    /// Computed CSS styles.
    Styles(StyleMap),
    /// Layout tree (positioned boxes).
    LayoutTree(LayoutBox),
    /// GPU render commands.
    RenderCommands(RenderCommands),

    /// A decoded image as a GPU texture handle.
    GpuTexture(GpuTextureHandle),

    /// HTTP response.
    HttpResponse(HttpResponse),

    /// JavaScript execution result.
    JsResult(JsValue),

    /// JavaScript execution result bundled with the (possibly mutated) DOM.
    JsResultWithDom {
        value: JsValue,
        dom: Box<DomNode>,
    },

    /// Nothing (void response).
    None,
}

// ── DOM types (defined by the core, used by mods) ──

/// A node in the DOM tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DomNode {
    Document {
        children: Vec<DomNode>,
    },
    Element {
        tag: String,
        attributes: Vec<(String, String)>,
        children: Vec<DomNode>,
    },
    Text(String),
    Comment(String),
}

impl DomNode {
    /// Get the tag name if this is an element.
    pub fn tag(&self) -> Option<&str> {
        match self {
            DomNode::Element { tag, .. } => Some(tag),
            _ => None,
        }
    }

    /// Get children of this node.
    pub fn children(&self) -> &[DomNode] {
        match self {
            DomNode::Document { children } | DomNode::Element { children, .. } => children,
            _ => &[],
        }
    }

    /// Get an attribute value.
    pub fn attr(&self, name: &str) -> Option<&str> {
        match self {
            DomNode::Element { attributes, .. } => attributes
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str()),
            _ => None,
        }
    }

    /// Pretty-print the DOM tree with indentation, similar to browser DevTools.
    ///
    /// Each level of nesting adds `indent` spaces. Text nodes are shown inline,
    /// and whitespace-only text nodes are omitted for readability.
    pub fn pretty_print(&self, indent: usize) -> String {
        let mut buf = String::new();
        self.fmt_recursive(&mut buf, indent);
        buf
    }

    /// Recursive helper for `pretty_print`.
    fn fmt_recursive(&self, buf: &mut String, indent: usize) {
        let pad = " ".repeat(indent);
        match self {
            DomNode::Document { children } => {
                buf.push_str(&format!("{pad}#document\n"));
                for child in children {
                    child.fmt_recursive(buf, indent + 2);
                }
            }
            DomNode::Element {
                tag,
                attributes,
                children,
            } => {
                buf.push_str(&format!("{pad}<{tag}"));
                for (k, v) in attributes {
                    buf.push_str(&format!(" {k}=\"{v}\""));
                }
                buf.push_str(">\n");
                for child in children {
                    child.fmt_recursive(buf, indent + 2);
                }
                buf.push_str(&format!("{pad}</{tag}>\n"));
            }
            DomNode::Text(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    // Collapse long text for display.
                    let display = if trimmed.len() > 80 {
                        format!("{}...", &trimmed[..77])
                    } else {
                        trimmed.to_string()
                    };
                    buf.push_str(&format!("{pad}\"{display}\"\n"));
                }
            }
            DomNode::Comment(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    buf.push_str(&format!("{pad}<!-- {trimmed} -->\n"));
                }
            }
        }
    }
}

// ── CSS types ──

/// Map of computed style properties for a node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StyleMap {
    pub properties: Vec<(String, StyleValue)>,
}

/// A resolved CSS value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StyleValue {
    /// A keyword (e.g., "block", "none", "auto").
    Keyword(String),
    /// A length in pixels (already resolved).
    Px(f32),
    /// A percentage.
    Percent(f32),
    /// A color.
    Color(CssColor),
    /// A string (e.g., font-family).
    Str(String),
    /// A numeric value.
    Number(f32),
}

/// A CSS color value (resolved to RGBA).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CssColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

// ── Layout types ──

/// A box in the layout tree with computed position and size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub content: LayoutContent,
    pub style: StyleMap,
    pub children: Vec<LayoutBox>,
    /// The CSS `z-index` value for this box. Defaults to 0 (auto).
    pub z_index: i32,
}

/// What a layout box contains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LayoutContent {
    /// A block-level element.
    Block,
    /// An inline-level element.
    Inline,
    /// A text run.
    Text(String),
    /// An image.
    Image { src: String },
    /// A replaced element (video, canvas, etc.).
    Replaced,
}

// ── Network types ──

/// HTTP response from a fetch operation.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub url: String,
}

impl HttpResponse {
    /// Get a header value by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == lower)
            .map(|(_, v)| v.as_str())
    }

    /// Get the Content-Type header.
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }
}

// ── JavaScript types ──

/// A JavaScript value returned from execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JsValue {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    String(String),
    Array(Vec<JsValue>),
    Object(Vec<(String, JsValue)>),
}

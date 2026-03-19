//! Capability types — what a mod can do.
//!
//! The core uses these to route requests to the right mod.
//! A mod declares its capabilities in its manifest.

use serde::{Deserialize, Serialize};

/// Describes a specific capability that a mod provides.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum CapabilityType {
    // ── Document parsing ──
    /// Parse a document from a MIME type (e.g., "text/html", "image/svg+xml").
    ParseDocument(String),
    /// Parse a CSS stylesheet.
    ParseStylesheet,

    // ── Script execution ──
    /// Execute JavaScript.
    ExecJavaScript,
    /// Execute WebAssembly.
    ExecWasm,

    // ── Image decoding ──
    /// Decode an image format (e.g., "png", "jpeg", "webp", "avif").
    DecodeImage(String),

    // ── Video/Audio decoding ──
    /// Decode a video codec (e.g., "vp9", "h264", "av1").
    DecodeVideo(String),
    /// Decode an audio codec (e.g., "opus", "aac", "mp3").
    DecodeAudio(String),

    // ── Layout and rendering ──
    /// Compute CSS styles (cascade, inheritance, computed values).
    ComputeStyles,
    /// Perform layout (block, inline, flex, grid).
    Layout,
    /// Generate render commands from a layout tree.
    Paint,

    // ── Network protocols ──
    /// Fetch a URL using a protocol (e.g., "https", "http", "ipfs", "gemini").
    FetchUrl(String),

    // ── Special documents ──
    /// Render a special document type (e.g., "application/pdf").
    RenderDocument(String),

    // ── Web APIs ──
    /// Provide a Web API (e.g., "webrtc", "bluetooth", "gamepad").
    WebApi(String),

    // ── Fonts ──
    /// Decode/load a font format (e.g., "woff2", "opentype").
    DecodeFont(String),
}

impl std::fmt::Display for CapabilityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseDocument(mime) => write!(f, "ParseDocument({mime})"),
            Self::ParseStylesheet => write!(f, "ParseStylesheet"),
            Self::ExecJavaScript => write!(f, "ExecJavaScript"),
            Self::ExecWasm => write!(f, "ExecWasm"),
            Self::DecodeImage(fmt) => write!(f, "DecodeImage({fmt})"),
            Self::DecodeVideo(codec) => write!(f, "DecodeVideo({codec})"),
            Self::DecodeAudio(codec) => write!(f, "DecodeAudio({codec})"),
            Self::ComputeStyles => write!(f, "ComputeStyles"),
            Self::Layout => write!(f, "Layout"),
            Self::Paint => write!(f, "Paint"),
            Self::FetchUrl(proto) => write!(f, "FetchUrl({proto})"),
            Self::RenderDocument(mime) => write!(f, "RenderDocument({mime})"),
            Self::WebApi(api) => write!(f, "WebApi({api})"),
            Self::DecodeFont(fmt) => write!(f, "DecodeFont({fmt})"),
        }
    }
}

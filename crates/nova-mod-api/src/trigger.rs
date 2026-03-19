//! Content triggers — rules for auto-loading mods.
//!
//! The core evaluates triggers when it encounters new content.
//! If a trigger matches, the associated mod is loaded automatically.

use serde::{Deserialize, Serialize};

use crate::types::ModId;

/// A rule: "when condition X is met, load mod Y".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentTrigger {
    /// The condition to match.
    pub condition: TriggerCondition,
    /// The mod to load when the condition fires.
    pub mod_id: ModId,
    /// Priority (higher = checked first).
    pub priority: u8,
}

/// Conditions that trigger auto-loading of a mod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerCondition {
    /// Matches an HTTP Content-Type header (e.g., "text/html", "application/pdf").
    MimeType(String),

    /// Matches the first bytes of a file (magic bytes).
    /// E.g., PNG starts with [0x89, 0x50, 0x4E, 0x47].
    MagicBytes(Vec<u8>),

    /// Matches an HTML element encountered during parsing (e.g., "video", "canvas").
    HtmlElement(String),

    /// Matches a JavaScript API being called (e.g., "navigator.mediaDevices").
    JsApi(String),

    /// Matches a URL protocol scheme (e.g., "ipfs", "gemini", "dat").
    Protocol(String),

    /// Matches an HTTP response header (name, value pattern).
    HttpHeader(String, String),

    /// Matches a CSS feature being used (e.g., "@container", "display: grid").
    CssFeature(String),

    /// Matches a file extension (e.g., ".pdf", ".epub").
    FileExtension(String),
}

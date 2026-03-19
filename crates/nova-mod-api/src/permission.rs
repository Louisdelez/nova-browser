//! Permission model — what a mod is allowed to access.
//!
//! Each mod declares required permissions in its manifest.
//! The core enforces these at runtime.

use serde::{Deserialize, Serialize};

/// A permission that a mod can request.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum Permission {
    // ── Network ──
    /// Make outgoing HTTP(S) requests.
    NetworkFetch,
    /// Listen on a network port.
    NetworkListen,

    // ── GPU ──
    /// Use GPU for decoding.
    GpuDecode,
    /// Submit render commands.
    GpuRender,
    /// Use GPU compute shaders.
    GpuCompute,

    // ── Storage ──
    /// Read from sandboxed storage.
    StorageRead,
    /// Write to sandboxed storage.
    StorageWrite,

    // ── System ──
    /// Access the system clipboard.
    Clipboard,
    /// Show desktop notifications.
    Notifications,
    /// Access the filesystem (dangerous — requires user approval).
    FileSystem,
    /// Access the camera.
    Camera,
    /// Access the microphone.
    Microphone,
    /// Access geolocation.
    Geolocation,

    // ── Inter-mod ──
    /// Request a specific capability from the core.
    RequestCapability(String),
}

/// Trust level assigned to a mod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TrustLevel {
    /// Official mod, signed and audited by the NOVA team.
    Core = 4,
    /// Community mod with verified audit.
    Verified = 3,
    /// Community mod, sandboxed, user-approved permissions.
    Community = 2,
    /// WASM mod — double sandbox (WASM + process).
    Wasm = 1,
}

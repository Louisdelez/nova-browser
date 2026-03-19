//! Error types for the NOVA ecosystem.

use thiserror::Error;

use crate::types::ModId;
use crate::capability::CapabilityType;

/// The unified error type used across core ↔ mod boundaries.
#[derive(Error, Debug)]
pub enum NovaError {
    // ── Mod lifecycle ──
    #[error("mod '{0}' not found")]
    ModNotFound(ModId),

    #[error("mod '{0}' failed to initialize: {1}")]
    ModInitFailed(ModId, String),

    #[error("mod '{0}' is not loaded")]
    ModNotLoaded(ModId),

    #[error("mod '{0}' crashed: {1}")]
    ModCrashed(ModId, String),

    // ── Capability routing ──
    #[error("no mod registered for capability: {0:?}")]
    NoHandler(CapabilityType),

    #[error("capability request timed out: {0:?}")]
    RequestTimeout(CapabilityType),

    // ── Content ──
    #[error("unsupported content type: {0}")]
    UnsupportedContent(String),

    #[error("parse error: {0}")]
    ParseError(String),

    #[error("decode error: {0}")]
    DecodeError(String),

    // ── Layout ──
    #[error("layout error: {0}")]
    LayoutError(String),

    // ── Network ──
    #[error("network error: {0}")]
    NetworkError(String),

    #[error("DNS resolution failed for: {0}")]
    DnsError(String),

    #[error("TLS error: {0}")]
    TlsError(String),

    // ── GPU ──
    #[error("GPU error: {0}")]
    GpuError(String),

    #[error("GPU buffer allocation failed: requested {0} bytes")]
    GpuAllocFailed(usize),

    // ── Security ──
    #[error("permission denied: mod '{mod_id}' lacks permission '{permission}'")]
    PermissionDenied {
        mod_id: ModId,
        permission: String,
    },

    // ── Storage ──
    #[error("storage error: {0}")]
    StorageError(String),

    // ── IPC ──
    #[error("IPC send failed: {0}")]
    IpcSendError(String),

    #[error("IPC channel closed")]
    IpcChannelClosed,

    // ── Generic ──
    #[error("internal error: {0}")]
    Internal(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenient Result alias.
pub type NovaResult<T> = Result<T, NovaError>;

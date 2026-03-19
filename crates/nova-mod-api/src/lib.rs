//! # nova-mod-api
//!
//! Defines the contracts between the NOVA core and its mods.
//! This crate is the **single source of truth** for all interfaces.
//! Mods depend ONLY on this crate — never on the core or on each other.

pub mod capability;
pub mod content;
pub mod error;
pub mod manifest;
pub mod message;
pub mod permission;
pub mod trigger;
pub mod types;

pub use capability::CapabilityType;
pub use content::{ContentRequest, TypedData};
pub use error::NovaError;
pub use manifest::ModManifest;
pub use message::Message;
pub use permission::{Permission, TrustLevel};
pub use trigger::{ContentTrigger, TriggerCondition};
pub use types::*;

use std::sync::Arc;

/// The single entry point that a mod uses to talk to the core.
/// Mods never import the core — they only use this trait.
#[async_trait::async_trait]
pub trait CoreApi: Send + Sync {
    /// Returns the ID assigned to this mod by the core.
    fn mod_id(&self) -> &ModId;

    /// Emit data produced by this mod (the core decides where it goes).
    async fn emit(&self, output: TypedData) -> Result<(), NovaError>;

    /// Request data or processing from another capability.
    /// The mod does NOT choose which mod handles this — the core routes it.
    async fn request(&self, req: ContentRequest) -> Result<TypedData, NovaError>;

    /// Access the GPU bridge for submitting render commands.
    fn gpu(&self) -> &dyn GpuBridge;

    /// Access sandboxed storage (each mod gets its own isolated space).
    fn storage(&self) -> &dyn ModStorage;

    /// Structured logging routed through the core's tracing system.
    fn log(&self, level: LogLevel, msg: &str);
}

/// The base trait that every mod must implement.
#[async_trait::async_trait]
pub trait NovaMod: Send + Sync {
    /// Returns the mod's manifest (identity, capabilities, permissions, triggers).
    fn manifest(&self) -> &ModManifest;

    /// Called once when the mod is loaded. Receives a handle to the core.
    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError>;

    /// Handle a request routed by the core.
    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError>;

    /// Called before the mod is unloaded. Clean up resources.
    async fn shutdown(&self) -> Result<(), NovaError>;
}

/// GPU bridge — lets mods submit render commands without direct GPU access.
pub trait GpuBridge: Send + Sync {
    /// Allocate a shared GPU buffer.
    fn alloc_buffer(&self, size: usize) -> Result<GpuBufferHandle, NovaError>;

    /// Submit render commands to the compositor.
    fn submit(&self, commands: RenderCommands) -> Result<(), NovaError>;

    /// Allocate a GPU texture.
    fn alloc_texture(&self, width: u32, height: u32) -> Result<GpuTextureHandle, NovaError>;
}

/// Sandboxed key-value storage for mods.
pub trait ModStorage: Send + Sync {
    /// Read a value by key.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, NovaError>;

    /// Write a value by key.
    fn set(&self, key: &str, value: &[u8]) -> Result<(), NovaError>;

    /// Delete a value by key.
    fn delete(&self, key: &str) -> Result<(), NovaError>;

    /// List all keys.
    fn keys(&self) -> Result<Vec<String>, NovaError>;
}

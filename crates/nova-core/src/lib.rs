//! # nova-core
//!
//! The NOVA micro-kernel. This is the heart of the browser.
//!
//! The core knows NOTHING about web standards. It only:
//! - Loads and unloads mods
//! - Routes messages between mods via the IPC bus
//! - Manages the capability registry
//! - Enforces security/permissions
//! - Owns the GPU compositor
//! - Orchestrates the rendering pipeline

pub mod bridge;

use std::sync::{Arc, RwLock as StdRwLock};

use tokio::sync::RwLock;
use tracing::info;

use nova_gpu::{GpuBridgeImpl, GpuCompositor};
use nova_ipc::IpcBus;
use nova_mod_api::{
    CapabilityType, CoreApi, ModManifest, NovaError, NovaMod, TypedData, Viewport,
};
use nova_pipeline::PipelineEngine;
use nova_registry::CapabilityRegistry;
use nova_security::SecurityManager;

use crate::bridge::CoreApiBridge;

/// The NOVA core — the micro-kernel.
pub struct NovaCore {
    /// The capability registry (routes capabilities to mods).
    pub registry: Arc<CapabilityRegistry>,
    /// The IPC message bus.
    pub ipc: Arc<IpcBus>,
    /// The security manager.
    pub security: Arc<RwLock<SecurityManager>>,
    /// The GPU compositor (std RwLock — GPU ops are fast, no async needed).
    pub gpu: Arc<StdRwLock<GpuCompositor>>,
    /// The rendering pipeline.
    pub pipeline: Arc<PipelineEngine>,
}

impl NovaCore {
    /// Create a new NOVA core instance.
    pub fn new() -> Self {
        let registry = Arc::new(CapabilityRegistry::new());
        let ipc = Arc::new(IpcBus::new());
        let security = Arc::new(RwLock::new(SecurityManager::new()));
        let gpu = Arc::new(StdRwLock::new(GpuCompositor::new()));
        let pipeline = Arc::new(PipelineEngine::new(registry.clone()));

        Self {
            registry,
            ipc,
            security,
            gpu,
            pipeline,
        }
    }

    /// Initialize the core (GPU, etc.).
    pub async fn init(&self) -> Result<(), NovaError> {
        info!("NOVA Core: initializing");

        // Initialize GPU compositor.
        self.gpu
            .write()
            .map_err(|e| NovaError::GpuError(format!("lock poisoned: {e}")))?
            .init()?;

        info!("NOVA Core: ready");
        Ok(())
    }

    /// Load and register a mod.
    pub async fn load_mod(&self, mut nova_mod: Box<dyn NovaMod>) -> Result<(), NovaError> {
        let manifest = nova_mod.manifest().clone();
        let mod_id = manifest.id.clone();

        info!("Core: loading mod '{}' v{}", mod_id, manifest.version);

        // 1. Register permissions with the security manager.
        self.security.write().await.register_mod(
            mod_id.clone(),
            manifest.permissions.clone(),
            manifest.trust_level,
        );

        // 2. Create a CoreApi bridge for this mod.
        let bridge = Arc::new(CoreApiBridge::new(
            mod_id.clone(),
            self.registry.clone(),
            GpuBridgeImpl::new(self.gpu.clone()),
        ));

        // 3. Initialize the mod.
        nova_mod
            .init(bridge)
            .await
            .map_err(|e| NovaError::ModInitFailed(mod_id.clone(), e.to_string()))?;

        // 4. Register the mod in the capability registry.
        let arc_mod: Arc<dyn NovaMod> = Arc::from(nova_mod);
        self.registry.register_mod(arc_mod.clone()).await;

        // 5. Register on the IPC bus.
        self.ipc.register(mod_id.clone(), arc_mod).await;

        info!("Core: mod '{}' loaded successfully", mod_id);
        Ok(())
    }

    /// Navigate to a URL (delegates to the pipeline engine).
    pub async fn navigate(
        &self,
        url: &str,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        self.pipeline.navigate(url, viewport).await
    }

    /// Navigate using POST method with a request body.
    ///
    /// Used for `<form method="POST">` submissions.
    pub async fn navigate_post(
        &self,
        url: &str,
        body: Vec<u8>,
        content_type: &str,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        self.pipeline.navigate_post(url, body, content_type, viewport).await
    }

    /// Fetch a URL and parse it into a DOM tree (without running the full pipeline).
    ///
    /// This is useful for inspecting the intermediate DOM representation
    /// produced by the parser mod.
    pub async fn fetch_and_parse(&self, url: &str) -> Result<TypedData, NovaError> {
        self.pipeline.fetch_and_parse(url).await
    }

    /// List all loaded capabilities.
    pub async fn capabilities(&self) -> Vec<CapabilityType> {
        self.registry.capabilities().await
    }

    /// List all loaded mod manifests.
    pub async fn mods(&self) -> Vec<ModManifest> {
        self.registry.manifests().await
    }
}

impl Default for NovaCore {
    fn default() -> Self {
        Self::new()
    }
}

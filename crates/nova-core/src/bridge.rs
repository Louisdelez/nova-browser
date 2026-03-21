//! CoreApi bridge — the implementation of CoreApi given to each mod.
//!
//! Each mod gets its own bridge instance. The bridge knows the mod's ID
//! and routes all calls through the core's systems.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::{debug, error, info, trace, warn};

use nova_gpu::GpuBridgeImpl;
use nova_mod_api::{
    ContentRequest, CoreApi, GpuBridge, LogLevel, ModId, ModStorage, NovaError, TypedData,
};
use nova_registry::CapabilityRegistry;

/// The bridge given to each mod. Implements CoreApi.
pub struct CoreApiBridge {
    mod_id: ModId,
    registry: Arc<CapabilityRegistry>,
    gpu_bridge: GpuBridgeImpl,
    storage: MemoryModStorage,
}

impl CoreApiBridge {
    pub fn new(
        mod_id: ModId,
        registry: Arc<CapabilityRegistry>,
        gpu_bridge: GpuBridgeImpl,
    ) -> Self {
        Self {
            storage: MemoryModStorage::new(mod_id.clone()),
            mod_id,
            registry,
            gpu_bridge,
        }
    }
}

#[async_trait::async_trait]
impl CoreApi for CoreApiBridge {
    fn mod_id(&self) -> &ModId {
        &self.mod_id
    }

    async fn emit(&self, _output: TypedData) -> Result<(), NovaError> {
        // For now, emits are logged. In the future, they feed into the pipeline.
        debug!("Mod '{}' emitted data", self.mod_id);
        Ok(())
    }

    async fn request(&self, req: ContentRequest) -> Result<TypedData, NovaError> {
        // Route the request through the capability registry.
        // The mod doesn't know or care which mod handles it.
        let capability = capability_for_request(&req);
        debug!(
            "Mod '{}' requesting capability '{}'",
            self.mod_id, capability
        );
        self.registry.route(&capability, req).await
    }

    fn gpu(&self) -> &dyn GpuBridge {
        &self.gpu_bridge
    }

    fn storage(&self) -> &dyn ModStorage {
        &self.storage
    }

    fn log(&self, level: LogLevel, msg: &str) {
        match level {
            LogLevel::Trace => trace!("[{}] {}", self.mod_id, msg),
            LogLevel::Debug => debug!("[{}] {}", self.mod_id, msg),
            LogLevel::Info => info!("[{}] {}", self.mod_id, msg),
            LogLevel::Warn => warn!("[{}] {}", self.mod_id, msg),
            LogLevel::Error => error!("[{}] {}", self.mod_id, msg),
        }
    }
}

/// Determine the capability type for a content request.
fn capability_for_request(req: &ContentRequest) -> nova_mod_api::CapabilityType {
    use nova_mod_api::CapabilityType;

    match req {
        ContentRequest::Fetch { url, .. } | ContentRequest::FetchWithBody { url, .. } => {
            let protocol = url
                .split("://")
                .next()
                .unwrap_or("https")
                .to_string();
            CapabilityType::FetchUrl(protocol)
        }
        ContentRequest::Parse { mime_type, .. } => {
            CapabilityType::ParseDocument(mime_type.clone())
        }
        ContentRequest::ParseCss { .. } => CapabilityType::ParseStylesheet,
        ContentRequest::ExecScript { .. } => CapabilityType::ExecJavaScript,
        ContentRequest::ExecScriptWithDom { .. } => CapabilityType::ExecJavaScript,
        ContentRequest::DispatchEvent { .. } => CapabilityType::ExecJavaScript,
        ContentRequest::DecodeImage { format_hint, .. } => {
            CapabilityType::DecodeImage(format_hint.clone().unwrap_or_default())
        }
        ContentRequest::DecodeVideo { codec, .. } => {
            CapabilityType::DecodeVideo(codec.clone())
        }
        ContentRequest::ComputeStyles { .. } => CapabilityType::ComputeStyles,
        ContentRequest::Layout { .. } => CapabilityType::Layout,
        ContentRequest::Paint { .. } => CapabilityType::Paint,
        ContentRequest::GetConsoleOutput { .. } => CapabilityType::ExecJavaScript,
        ContentRequest::Custom { capability, .. } => capability.clone(),
    }
}

/// Simple in-memory mod storage (sandboxed per mod).
pub struct MemoryModStorage {
    mod_id: ModId,
    data: RwLock<HashMap<String, Vec<u8>>>,
}

impl MemoryModStorage {
    pub fn new(mod_id: ModId) -> Self {
        Self {
            mod_id,
            data: RwLock::new(HashMap::new()),
        }
    }
}

impl ModStorage for MemoryModStorage {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, NovaError> {
        Ok(self
            .data
            .read()
            .map_err(|e| NovaError::StorageError(e.to_string()))?
            .get(key)
            .cloned())
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), NovaError> {
        self.data
            .write()
            .map_err(|e| NovaError::StorageError(e.to_string()))?
            .insert(key.to_string(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), NovaError> {
        self.data
            .write()
            .map_err(|e| NovaError::StorageError(e.to_string()))?
            .remove(key);
        Ok(())
    }

    fn keys(&self) -> Result<Vec<String>, NovaError> {
        Ok(self
            .data
            .read()
            .map_err(|e| NovaError::StorageError(e.to_string()))?
            .keys()
            .cloned()
            .collect())
    }
}

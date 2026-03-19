//! # nova-registry
//!
//! The Capability Registry — the core's routing table.
//! Maps capability types to the mods that provide them.
//! Handles mod loading states (Loaded, Available, NotInstalled).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use nova_mod_api::{
    CapabilityType, ContentRequest, ModId, ModManifest, NovaError, NovaMod, TypedData,
};

/// State of a mod in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModState {
    /// The mod is loaded in memory and ready to handle requests.
    Loaded,
    /// The mod is installed on disk but not loaded in memory.
    Available,
    /// The mod exists in the remote registry but is not installed.
    NotInstalled,
    /// The mod is installed but disabled by the user.
    Disabled,
}

/// A registered mod handler with its state and priority.
struct ModEntry {
    mod_id: ModId,
    manifest: ModManifest,
    instance: Option<Arc<dyn NovaMod>>,
    state: ModState,
    priority: u8,
}

/// The Capability Registry — routes capability requests to the right mod.
pub struct CapabilityRegistry {
    /// Capability -> list of mod entries that provide it.
    handlers: RwLock<HashMap<CapabilityType, Vec<ModEntry>>>,
    /// Quick lookup: mod_id -> manifest.
    manifests: RwLock<HashMap<ModId, ModManifest>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(HashMap::new()),
            manifests: RwLock::new(HashMap::new()),
        }
    }

    /// Register a loaded mod with all its capabilities.
    pub async fn register_mod(&self, instance: Arc<dyn NovaMod>) {
        let manifest = instance.manifest().clone();
        let mod_id = manifest.id.clone();

        info!(
            "Registry: registering mod '{}' v{} with {} capabilities",
            mod_id,
            manifest.version,
            manifest.capabilities.len()
        );

        let mut handlers = self.handlers.write().await;
        for cap in &manifest.capabilities {
            let entry = ModEntry {
                mod_id: mod_id.clone(),
                manifest: manifest.clone(),
                instance: Some(instance.clone()),
                state: ModState::Loaded,
                priority: 100, // default priority for loaded mods
            };

            handlers.entry(cap.clone()).or_default().push(entry);

            debug!("  -> capability '{cap}' registered");
        }

        self.manifests
            .write()
            .await
            .insert(mod_id, manifest);
    }

    /// Register a mod that is available (installed) but not yet loaded.
    pub async fn register_available(&self, manifest: ModManifest) {
        let mod_id = manifest.id.clone();

        info!("Registry: registering available mod '{mod_id}' v{}", manifest.version);

        let mut handlers = self.handlers.write().await;
        for cap in &manifest.capabilities {
            let entry = ModEntry {
                mod_id: mod_id.clone(),
                manifest: manifest.clone(),
                instance: None,
                state: ModState::Available,
                priority: 50,
            };

            handlers.entry(cap.clone()).or_default().push(entry);
        }

        self.manifests
            .write()
            .await
            .insert(mod_id, manifest);
    }

    /// Find the best mod for a given capability.
    /// Returns the mod_id and its state.
    pub async fn resolve(&self, capability: &CapabilityType) -> Option<(ModId, ModState)> {
        let handlers = self.handlers.read().await;
        handlers.get(capability).and_then(|entries| {
            // Find the highest-priority entry that isn't disabled.
            entries
                .iter()
                .filter(|e| e.state != ModState::Disabled)
                .max_by_key(|e| e.priority)
                .map(|e| (e.mod_id.clone(), e.state.clone()))
        })
    }

    /// Route a request to the best mod for a given capability.
    /// Returns an error if no mod handles the capability.
    pub async fn route(
        &self,
        capability: &CapabilityType,
        request: ContentRequest,
    ) -> Result<TypedData, NovaError> {
        let handlers = self.handlers.read().await;
        let entries = handlers
            .get(capability)
            .ok_or_else(|| NovaError::NoHandler(capability.clone()))?;

        // Find the best loaded mod.
        let best = entries
            .iter()
            .filter(|e| e.state == ModState::Loaded)
            .max_by_key(|e| e.priority)
            .ok_or_else(|| {
                // There are entries but none are loaded.
                // In the future, this would trigger auto-loading.
                warn!("Registry: capability '{capability}' has handlers but none are loaded");
                NovaError::ModNotLoaded(
                    entries
                        .first()
                        .map(|e| e.mod_id.clone())
                        .unwrap_or_else(|| ModId::new("unknown")),
                )
            })?;

        debug!("Registry: routing '{capability}' to mod '{}'", best.mod_id);

        let instance = best
            .instance
            .as_ref()
            .ok_or_else(|| NovaError::ModNotLoaded(best.mod_id.clone()))?;

        instance.handle(request).await
    }

    /// List all registered capabilities.
    pub async fn capabilities(&self) -> Vec<CapabilityType> {
        self.handlers.read().await.keys().cloned().collect()
    }

    /// List all registered mod manifests.
    pub async fn manifests(&self) -> Vec<ModManifest> {
        self.manifests.read().await.values().cloned().collect()
    }

    /// Check if a capability has at least one handler.
    pub async fn has_handler(&self, capability: &CapabilityType) -> bool {
        self.handlers
            .read()
            .await
            .get(capability)
            .map(|entries| entries.iter().any(|e| e.state != ModState::Disabled))
            .unwrap_or(false)
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_empty_registry() {
        let reg = CapabilityRegistry::new();
        let cap = CapabilityType::ParseDocument("text/html".into());
        assert!(!reg.has_handler(&cap).await);
        assert!(reg.resolve(&cap).await.is_none());
    }
}

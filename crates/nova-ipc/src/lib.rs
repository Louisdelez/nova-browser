//! # nova-ipc
//!
//! The IPC message bus — the central nervous system of NOVA.
//! All communication between mods goes through this bus.
//! Mods never talk to each other directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, error, warn};

use nova_mod_api::{
    ContentRequest, ModId, NovaError, NovaMod, TypedData,
};

/// A pending request waiting for a response.
struct PendingRequest {
    response_tx: oneshot::Sender<Result<TypedData, NovaError>>,
}

/// The IPC bus routes requests from any mod to the appropriate handler.
pub struct IpcBus {
    /// Registered mod handlers (mod_id -> mod instance).
    handlers: RwLock<HashMap<ModId, Arc<dyn NovaMod>>>,
    /// Pending requests awaiting responses.
    pending: RwLock<HashMap<u64, PendingRequest>>,
    /// Counter for generating unique request IDs.
    next_id: AtomicU64,
}

impl IpcBus {
    /// Create a new empty IPC bus.
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a mod on the bus.
    pub async fn register(&self, mod_id: ModId, handler: Arc<dyn NovaMod>) {
        debug!("IPC: registering mod '{mod_id}'");
        self.handlers.write().await.insert(mod_id, handler);
    }

    /// Unregister a mod from the bus.
    pub async fn unregister(&self, mod_id: &ModId) {
        debug!("IPC: unregistering mod '{mod_id}'");
        self.handlers.write().await.remove(mod_id);
    }

    /// Send a request to a specific mod and wait for the response.
    pub async fn send_to(
        &self,
        target: &ModId,
        request: ContentRequest,
    ) -> Result<TypedData, NovaError> {
        let handlers = self.handlers.read().await;
        let handler = handlers
            .get(target)
            .ok_or_else(|| NovaError::ModNotLoaded(target.clone()))?
            .clone();
        drop(handlers);

        debug!("IPC: routing request to mod '{target}'");
        handler.handle(request).await
    }

    /// Generate a unique request ID.
    pub fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Check if a mod is registered.
    pub async fn is_registered(&self, mod_id: &ModId) -> bool {
        self.handlers.read().await.contains_key(mod_id)
    }

    /// List all registered mod IDs.
    pub async fn registered_mods(&self) -> Vec<ModId> {
        self.handlers.read().await.keys().cloned().collect()
    }
}

impl Default for IpcBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_unregister() {
        let bus = IpcBus::new();
        let mod_id = ModId::new("test.mod");

        assert!(!bus.is_registered(&mod_id).await);
        assert!(bus.registered_mods().await.is_empty());
    }
}

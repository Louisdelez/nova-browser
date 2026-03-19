//! # nova-security
//!
//! Enforces permissions and sandboxing for mods.
//! Every request from a mod passes through the security layer
//! before being routed by the core.

use std::collections::{HashMap, HashSet};

use tracing::{info, warn};

use nova_mod_api::{ContentRequest, ModId, NovaError, Permission, TrustLevel};

/// The security manager — gate-keeps all mod actions.
pub struct SecurityManager {
    /// Granted permissions per mod.
    grants: HashMap<ModId, HashSet<Permission>>,
    /// Trust levels per mod.
    trust_levels: HashMap<ModId, TrustLevel>,
}

impl SecurityManager {
    pub fn new() -> Self {
        Self {
            grants: HashMap::new(),
            trust_levels: HashMap::new(),
        }
    }

    /// Register a mod's permissions and trust level.
    pub fn register_mod(
        &mut self,
        mod_id: ModId,
        permissions: Vec<Permission>,
        trust_level: TrustLevel,
    ) {
        info!(
            "Security: registering mod '{}' with trust level {:?} and {} permissions",
            mod_id,
            trust_level,
            permissions.len()
        );
        self.trust_levels.insert(mod_id.clone(), trust_level);
        self.grants
            .insert(mod_id, permissions.into_iter().collect());
    }

    /// Unregister a mod.
    pub fn unregister_mod(&mut self, mod_id: &ModId) {
        self.grants.remove(mod_id);
        self.trust_levels.remove(mod_id);
    }

    /// Check if a mod has a specific permission.
    pub fn has_permission(&self, mod_id: &ModId, permission: &Permission) -> bool {
        self.grants
            .get(mod_id)
            .map(|perms| perms.contains(permission))
            .unwrap_or(false)
    }

    /// Validate that a mod is allowed to make a given request.
    /// Returns Ok(()) if allowed, Err if denied.
    pub fn check_request(
        &self,
        mod_id: &ModId,
        request: &ContentRequest,
    ) -> Result<(), NovaError> {
        let required = Self::permissions_for_request(request);

        for perm in &required {
            if !self.has_permission(mod_id, perm) {
                warn!(
                    "Security: mod '{}' denied permission '{:?}' for request",
                    mod_id, perm
                );
                return Err(NovaError::PermissionDenied {
                    mod_id: mod_id.clone(),
                    permission: format!("{perm:?}"),
                });
            }
        }

        Ok(())
    }

    /// Determine which permissions are needed for a given request type.
    fn permissions_for_request(request: &ContentRequest) -> Vec<Permission> {
        match request {
            ContentRequest::Fetch { .. } => vec![Permission::NetworkFetch],
            ContentRequest::Paint { .. } => vec![Permission::GpuRender],
            ContentRequest::DecodeImage { .. } => vec![Permission::GpuDecode],
            ContentRequest::DecodeVideo { .. } => vec![Permission::GpuDecode],
            // Most requests (parsing, layout, style) don't require special permissions.
            _ => vec![],
        }
    }

    /// Get the trust level of a mod.
    pub fn trust_level(&self, mod_id: &ModId) -> Option<TrustLevel> {
        self.trust_levels.get(mod_id).copied()
    }
}

impl Default for SecurityManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_check() {
        let mut mgr = SecurityManager::new();
        let mod_id = ModId::new("org.nova.network");

        mgr.register_mod(
            mod_id.clone(),
            vec![Permission::NetworkFetch, Permission::StorageRead],
            TrustLevel::Core,
        );

        assert!(mgr.has_permission(&mod_id, &Permission::NetworkFetch));
        assert!(mgr.has_permission(&mod_id, &Permission::StorageRead));
        assert!(!mgr.has_permission(&mod_id, &Permission::FileSystem));
    }

    #[test]
    fn test_request_denied_without_permission() {
        let mgr = SecurityManager::new();
        let mod_id = ModId::new("org.nova.test");

        let request = ContentRequest::Fetch {
            url: "https://example.com".into(),
            headers: vec![],
        };

        assert!(mgr.check_request(&mod_id, &request).is_err());
    }
}

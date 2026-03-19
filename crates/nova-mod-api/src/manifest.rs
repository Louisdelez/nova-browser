//! Mod manifest — the identity card of a mod.

use serde::{Deserialize, Serialize};
use semver::Version;

use crate::capability::CapabilityType;
use crate::permission::{Permission, TrustLevel};
use crate::trigger::ContentTrigger;
use crate::types::ModId;

/// Declares everything about a mod: what it is, what it does, what it needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModManifest {
    /// Unique identifier (e.g., "org.nova.html-parser").
    pub id: ModId,
    /// Human-readable name.
    pub name: String,
    /// Semantic version.
    pub version: Version,
    /// Short description.
    pub description: String,
    /// Capabilities this mod provides to the core.
    pub capabilities: Vec<CapabilityType>,
    /// Permissions this mod requires.
    pub permissions: Vec<Permission>,
    /// Other mods this mod depends on (by capability, NOT by mod ID).
    pub dependencies: Vec<CapabilityType>,
    /// Content triggers — when should the core auto-load this mod.
    pub triggers: Vec<ContentTrigger>,
    /// Minimum core version required.
    pub min_core_version: Version,
    /// Trust level.
    pub trust_level: TrustLevel,
}

impl ModManifest {
    /// Check if this mod provides a given capability.
    pub fn provides(&self, cap: &CapabilityType) -> bool {
        self.capabilities.contains(cap)
    }

    /// Check if this mod requires a given permission.
    pub fn requires_permission(&self, perm: &Permission) -> bool {
        self.permissions.contains(perm)
    }
}

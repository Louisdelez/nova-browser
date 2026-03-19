//! # mod-js
//!
//! NOVA Mod for JavaScript execution. Handles the `ExecJavaScript` capability.
//!
//! This is a **placeholder** — the actual QuickJS integration will come in a
//! later phase. For now, it logs the script source and returns `JsValue::Undefined`.

use std::sync::Arc;

use async_trait::async_trait;
use semver::Version;
use tracing::{debug, info, warn};

use nova_mod_api::{
    capability::CapabilityType,
    content::{ContentRequest, JsValue, TypedData},
    error::NovaError,
    manifest::ModManifest,
    permission::TrustLevel,
    trigger::{ContentTrigger, TriggerCondition},
    types::ModId,
    CoreApi, NovaMod,
};

/// The JavaScript engine mod (placeholder).
pub struct JsMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
}

impl JsMod {
    /// Create a new `JsMod` instance.
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.js"),
            name: "NOVA JavaScript Engine".into(),
            version: Version::new(0, 1, 0),
            description: "JavaScript execution engine (placeholder for QuickJS)".into(),
            capabilities: vec![CapabilityType::ExecJavaScript],
            permissions: vec![],
            dependencies: vec![],
            triggers: vec![ContentTrigger {
                condition: TriggerCondition::MimeType("application/javascript".into()),
                mod_id: ModId::new("org.nova.js"),
                priority: 100,
            }],
            min_core_version: Version::new(0, 1, 0),
            trust_level: TrustLevel::Core,
        };

        Self {
            manifest,
            core: None,
        }
    }
}

impl Default for JsMod {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NovaMod for JsMod {
    fn manifest(&self) -> &ModManifest {
        &self.manifest
    }

    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "js mod initializing (placeholder)");
        self.core = Some(core);
        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            ContentRequest::ExecScript { source, context_id } => {
                debug!(
                    len = source.len(),
                    context_id = ?context_id,
                    "received script for execution"
                );
                warn!("JS execution is a placeholder — returning undefined");

                // Log a snippet of the script for debugging.
                let preview: String = source.chars().take(100).collect();
                debug!(preview = %preview, "script preview");

                // TODO: Integrate QuickJS here.
                Ok(TypedData::JsResult(JsValue::Undefined))
            }
            other => Err(NovaError::UnsupportedContent(format!(
                "js mod cannot handle request: {other:?}"
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "js mod shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_provides_js() {
        let m = JsMod::new();
        assert!(m.manifest().provides(&CapabilityType::ExecJavaScript));
    }

    #[tokio::test]
    async fn exec_returns_undefined() {
        let m = JsMod::new();
        let req = ContentRequest::ExecScript {
            source: "console.log('hello')".into(),
            context_id: None,
        };
        let result = m.handle(req).await.unwrap();
        match result {
            TypedData::JsResult(JsValue::Undefined) => {}
            _ => panic!("expected JsResult(Undefined)"),
        }
    }
}

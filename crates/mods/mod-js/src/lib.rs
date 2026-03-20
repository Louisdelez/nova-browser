//! # mod-js
//!
//! NOVA Mod for JavaScript execution. Handles the `ExecJavaScript` capability.
//!
//! ## Architecture
//!
//! The mod uses **rquickjs** (QuickJS bindings) as its JavaScript engine,
//! providing full language support including loops, closures, Promises,
//! arrow functions, and ES6+ features.
//!
//! The DOM bridge (see [`dom_bridge`]) registers low-level Rust functions on
//! a hidden `__nova` global, and a JavaScript shim creates the standard
//! `document`, `Element`, `console`, and `window` APIs on top.
//!
//! When a script is executed alongside a DOM tree (`ExecScriptWithDom`),
//! the DOM is imported into a [`dom_api::JsDomTree`], the QuickJS runtime
//! evaluates the script, and the (possibly mutated) DOM is returned
//! alongside the script's return value.
//!
//! Event listeners registered via `addEventListener` are stored in a
//! JavaScript-side global registry.  The `DispatchEvent` request fires
//! callbacks through the QuickJS context.
//!
//! The old built-in interpreter (in [`dom_api`]) is kept as a fallback
//! but is no longer used by default.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use semver::Version;
use tracing::{debug, info, warn};

use nova_mod_api::{
    capability::CapabilityType,
    content::{ContentRequest, DomNode, JsValue, TypedData},
    error::NovaError,
    manifest::ModManifest,
    permission::TrustLevel,
    trigger::{ContentTrigger, TriggerCondition},
    types::ModId,
    CoreApi, NovaMod,
};

pub mod dom_api;
pub mod dom_bridge;
pub mod quickjs_runtime;

use dom_api::{JsDomTree, eval_script, eval_script_with_env};
use quickjs_runtime::QuickJsRuntime;

// ── Context store ─────────────────────────────────────────────────────────────

/// An active JavaScript execution context.
///
/// Each context owns a DOM tree, a set of live event listeners, and
/// optionally a QuickJS runtime for real JS execution.
struct JsContext {
    /// The live DOM tree for this context.
    tree: Arc<Mutex<JsDomTree>>,
    /// The QuickJS runtime (created lazily on first eval).
    quickjs: Option<QuickJsRuntime>,
}

// ── Mod implementation ────────────────────────────────────────────────────────

/// The JavaScript engine mod.
pub struct JsMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
    /// Active contexts keyed by context_id.
    /// Wrapped in a `Mutex` so `handle` (which takes `&self`) can mutate it.
    contexts: Mutex<HashMap<u64, JsContext>>,
    /// Counter for generating context IDs when none is provided.
    next_context_id: Mutex<u64>,
}

impl JsMod {
    /// Create a new `JsMod` instance.
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.js"),
            name: "NOVA JavaScript Engine".into(),
            version: Version::new(0, 1, 0),
            description: "JavaScript execution engine with DOM API bridge".into(),
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
            contexts: Mutex::new(HashMap::new()),
            next_context_id: Mutex::new(1),
        }
    }

    /// Allocate a fresh context ID.
    fn alloc_context_id(&self) -> u64 {
        let mut guard = self.next_context_id.lock().unwrap();
        let id = *guard;
        *guard += 1;
        id
    }

    /// Retrieve an existing context or create a new empty one (no DOM).
    fn get_or_create_context(&self, context_id: u64) -> Arc<Mutex<JsDomTree>> {
        let mut ctxs = self.contexts.lock().unwrap();
        ctxs.entry(context_id)
            .or_insert_with(|| {
                // Create a minimal document DOM for contexts with no DOM provided.
                let empty_dom = DomNode::Document { children: vec![] };
                let tree = JsDomTree::from_dom(&empty_dom);
                JsContext { tree, quickjs: None }
            })
            .tree
            .clone()
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
        info!(mod_id = %self.manifest.id, "js mod initializing");
        self.core = Some(core);
        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            // ── Plain script execution (no DOM context) ───────────────────────
            ContentRequest::ExecScript { source, context_id } => {
                debug!(
                    len = source.len(),
                    context_id = ?context_id,
                    "ExecScript: executing script (QuickJS)"
                );

                let cid = context_id.unwrap_or_else(|| self.alloc_context_id());
                let tree = self.get_or_create_context(cid);

                // Use QuickJS runtime.
                let mut ctxs = self.contexts.lock().unwrap();
                let js_ctx = ctxs.get_mut(&cid).unwrap();
                if js_ctx.quickjs.is_none() {
                    match QuickJsRuntime::new(Arc::clone(&js_ctx.tree)) {
                        Ok(rt) => js_ctx.quickjs = Some(rt),
                        Err(e) => {
                            warn!(error = %e, "failed to create QuickJS runtime, falling back");
                            let result = eval_script(&source, tree);
                            return Ok(TypedData::JsResult(result));
                        }
                    }
                }
                let result = js_ctx.quickjs.as_ref().unwrap().eval(&source);
                debug!(?result, "ExecScript: script returned");
                Ok(TypedData::JsResult(result))
            }

            // ── Script execution with a live DOM tree ─────────────────────────
            ContentRequest::ExecScriptWithDom {
                source,
                dom,
                context_id,
            } => {
                debug!(
                    len = source.len(),
                    context_id = ?context_id,
                    "ExecScriptWithDom: executing script with DOM (QuickJS)"
                );

                let cid = context_id.unwrap_or_else(|| self.alloc_context_id());

                // Import the provided DOM into a fresh JsDomTree and register
                // it as the context's tree so that subsequent DispatchEvent
                // requests can access it.
                let tree = JsDomTree::from_dom(&dom);

                // Create a new QuickJS runtime with this DOM tree.
                let quickjs = match QuickJsRuntime::new(Arc::clone(&tree)) {
                    Ok(rt) => Some(rt),
                    Err(e) => {
                        warn!(error = %e, "failed to create QuickJS runtime, falling back");
                        None
                    }
                };

                {
                    let mut ctxs = self.contexts.lock().unwrap();
                    ctxs.insert(cid, JsContext {
                        tree: tree.clone(),
                        quickjs,
                    });
                }

                let value = {
                    let ctxs = self.contexts.lock().unwrap();
                    let js_ctx = ctxs.get(&cid).unwrap();
                    if let Some(ref qjs) = js_ctx.quickjs {
                        qjs.eval_with_dom(&source)
                    } else {
                        eval_script(&source, Arc::clone(&tree))
                    }
                };

                // Export the (potentially mutated) DOM back.
                let mutated_dom = tree.lock().unwrap().to_dom();
                debug!(?value, "ExecScriptWithDom: script returned");

                Ok(TypedData::JsResultWithDom {
                    value,
                    dom: Box::new(mutated_dom),
                })
            }

            // ── Dispatch a browser event to stored listeners ──────────────────
            ContentRequest::DispatchEvent {
                element_handle,
                event_type,
                context_id,
            } => {
                debug!(
                    element_handle,
                    %event_type,
                    context_id,
                    "DispatchEvent"
                );

                // Check if we have a QuickJS runtime for this context.
                let (tree, has_quickjs) = {
                    let ctxs = self.contexts.lock().unwrap();
                    match ctxs.get(&context_id) {
                        Some(ctx) => (ctx.tree.clone(), ctx.quickjs.is_some()),
                        None => {
                            warn!(context_id, "DispatchEvent: unknown context");
                            return Ok(TypedData::JsResult(JsValue::Undefined));
                        }
                    }
                };

                if has_quickjs {
                    // Dispatch through QuickJS — fire any JS-side listeners
                    // stored on __NovaElement instances via the global dispatch
                    // helper we install.
                    let dispatch_script = format!(
                        r#"(function() {{
                            if (typeof __novaDispatchEvent === 'function') {{
                                __novaDispatchEvent({element_handle}, "{event_type}");
                            }}
                        }})()"#,
                    );

                    let value = {
                        let ctxs = self.contexts.lock().unwrap();
                        let js_ctx = ctxs.get(&context_id).unwrap();
                        js_ctx.quickjs.as_ref().unwrap().eval(&dispatch_script)
                    };

                    let mutated_dom = tree.lock().unwrap().to_dom();
                    return Ok(TypedData::JsResultWithDom {
                        value,
                        dom: Box::new(mutated_dom),
                    });
                }

                // Fallback: old interpreter dispatch.
                let callbacks = tree
                    .lock()
                    .unwrap()
                    .dispatch_event(element_handle, &event_type);

                if callbacks.is_empty() {
                    debug!(element_handle, %event_type, "no listeners registered");
                    return Ok(TypedData::JsResult(JsValue::Undefined));
                }

                let mut last = JsValue::Undefined;
                for (cb_source, captured_env) in callbacks {
                    debug!(len = cb_source.len(), "executing event callback");
                    last = eval_script_with_env(&cb_source, Arc::clone(&tree), &captured_env);
                }

                let mutated_dom = tree.lock().unwrap().to_dom();
                Ok(TypedData::JsResultWithDom {
                    value: last,
                    dom: Box::new(mutated_dom),
                })
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom_api::ElementHandle;

    #[test]
    fn manifest_provides_js() {
        let m = JsMod::new();
        assert!(m.manifest().provides(&CapabilityType::ExecJavaScript));
    }

    #[tokio::test]
    async fn exec_returns_undefined_for_unknown_script() {
        let m = JsMod::new();
        let req = ContentRequest::ExecScript {
            source: "// just a comment".into(),
            context_id: None,
        };
        let result = m.handle(req).await.unwrap();
        match result {
            TypedData::JsResult(JsValue::Undefined) => {}
            _ => panic!("expected JsResult(Undefined)"),
        }
    }

    #[tokio::test]
    async fn exec_with_dom_returns_dom() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "div".into(),
                attributes: vec![("id".into(), "test".into())],
                children: vec![DomNode::Text("original".into())],
            }],
        };

        let m = JsMod::new();
        let req = ContentRequest::ExecScriptWithDom {
            source: r#"
                var el = document.getElementById("test");
                el.textContent = "mutated";
            "#
            .into(),
            dom: Box::new(dom),
            context_id: Some(1),
        };

        let result = m.handle(req).await.unwrap();
        match result {
            TypedData::JsResultWithDom { dom, .. } => {
                let dom_str = format!("{dom:?}");
                assert!(
                    dom_str.contains("mutated"),
                    "expected 'mutated' in DOM output: {dom_str}"
                );
            }
            _ => panic!("expected JsResultWithDom"),
        }
    }

    #[tokio::test]
    async fn dispatch_event_fires_listener() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "button".into(),
                attributes: vec![("id".into(), "btn".into())],
                children: vec![],
            }],
        };

        let m = JsMod::new();

        // First, exec a script that registers a listener.
        let setup_req = ContentRequest::ExecScriptWithDom {
            source: r#"
                var btn = document.getElementById("btn");
                btn.addEventListener("click", function() {
                    btn.setAttribute("data-clicked", "yes");
                });
            "#
            .into(),
            dom: Box::new(dom),
            context_id: Some(42),
        };
        let _setup_result = m.handle(setup_req).await.unwrap();

        // Get the button handle.
        let btn_handle: ElementHandle = {
            let ctxs = m.contexts.lock().unwrap();
            let ctx = ctxs.get(&42).unwrap();
            let tree = ctx.tree.lock().unwrap();
            tree.get_element_by_id("btn").unwrap()
        };

        // Now dispatch the click event.
        let dispatch_req = ContentRequest::DispatchEvent {
            element_handle: btn_handle,
            event_type: "click".into(),
            context_id: 42,
        };
        let result = m.handle(dispatch_req).await.unwrap();
        match result {
            TypedData::JsResultWithDom { dom, .. } => {
                let dom_str = format!("{dom:?}");
                assert!(
                    dom_str.contains("data-clicked"),
                    "expected data-clicked attr after click: {dom_str}"
                );
            }
            _ => panic!("expected JsResultWithDom, got {result:?}"),
        }
    }

    #[tokio::test]
    async fn exec_script_context_persists() {
        let m = JsMod::new();

        // First execution creates a context.
        let req1 = ContentRequest::ExecScript {
            source: "// initialize".into(),
            context_id: Some(99),
        };
        m.handle(req1).await.unwrap();

        // Second execution with same context_id.
        let req2 = ContentRequest::ExecScript {
            source: "// second run".into(),
            context_id: Some(99),
        };
        let result = m.handle(req2).await.unwrap();
        assert!(matches!(result, TypedData::JsResult(_)));
    }
}

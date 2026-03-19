//! # mod-js
//!
//! NOVA Mod for JavaScript execution via QuickJS. Handles the `ExecJavaScript`
//! capability.
//!
//! Uses `rquickjs` to create a QuickJS runtime and evaluate scripts. Each
//! `handle()` call gets a fresh context (no persistent state across scripts
//! in this initial implementation).
//!
//! A minimal `console.log` is installed that routes output to `tracing::info!`.

use std::sync::Arc;

use async_trait::async_trait;
use rquickjs::{Context, Function, Runtime};
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

/// The JavaScript engine mod (QuickJS-backed).
pub struct JsMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
    runtime: Option<Runtime>,
}

impl JsMod {
    /// Create a new `JsMod` instance.
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.js"),
            name: "NOVA JavaScript Engine".into(),
            version: Version::new(0, 1, 0),
            description: "JavaScript execution engine (QuickJS)".into(),
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
            runtime: None,
        }
    }
}

impl Default for JsMod {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a QuickJS value to our `JsValue` type.
fn quickjs_to_jsvalue(ctx: &rquickjs::Ctx<'_>, val: rquickjs::Value<'_>) -> JsValue {
    if val.is_undefined() {
        JsValue::Undefined
    } else if val.is_null() {
        JsValue::Null
    } else if let Some(b) = val.as_bool() {
        JsValue::Boolean(b)
    } else if let Some(n) = val.as_int() {
        JsValue::Number(n as f64)
    } else if let Some(n) = val.as_float() {
        JsValue::Number(n)
    } else if let Some(s) = val.clone().into_string() {
        JsValue::String(s.to_string().unwrap_or_default())
    } else if val.is_array() {
        if let Some(arr) = val.into_array() {
            let items: Vec<JsValue> = arr
                .iter::<rquickjs::Value<'_>>()
                .filter_map(|r| r.ok())
                .map(|v| quickjs_to_jsvalue(ctx, v))
                .collect();
            JsValue::Array(items)
        } else {
            JsValue::Undefined
        }
    } else if val.is_object() {
        if let Some(obj) = val.into_object() {
            let mut entries = Vec::new();
            let props = obj.props::<String, rquickjs::Value<'_>>();
            for result in props {
                if let Ok((key, value)) = result {
                    entries.push((key, quickjs_to_jsvalue(ctx, value)));
                }
            }
            JsValue::Object(entries)
        } else {
            JsValue::Undefined
        }
    } else {
        JsValue::Undefined
    }
}

#[async_trait]
impl NovaMod for JsMod {
    fn manifest(&self) -> &ModManifest {
        &self.manifest
    }

    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "js mod initializing (QuickJS)");
        self.core = Some(core);

        // Create the QuickJS runtime once during init.
        let runtime = Runtime::new().map_err(|e| {
            NovaError::Internal(format!("failed to create QuickJS runtime: {e}"))
        })?;
        self.runtime = Some(runtime);

        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            ContentRequest::ExecScript { source, context_id } => {
                debug!(
                    len = source.len(),
                    context_id = ?context_id,
                    "executing script via QuickJS"
                );

                let runtime = self.runtime.as_ref().ok_or_else(|| {
                    NovaError::Internal("QuickJS runtime not initialized".into())
                })?;

                // Create a fresh context per execution.
                let context = Context::full(runtime).map_err(|e| {
                    NovaError::Internal(format!("failed to create QuickJS context: {e}"))
                })?;

                let result = context.with(|ctx| {
                    // Install console.log → tracing::info!
                    let globals = ctx.globals();

                    let console = rquickjs::Object::new(ctx.clone())
                        .map_err(|e| NovaError::Internal(format!("failed to create console object: {e}")))?;

                    let log_fn = Function::new(ctx.clone(), |args: rquickjs::function::Rest<rquickjs::Value>| {
                        let parts: Vec<String> = args
                            .0
                            .iter()
                            .map(|v| {
                                if let Some(s) = v.as_string() {
                                    s.to_string().unwrap_or_else(|_| format!("{v:?}"))
                                } else if let Some(n) = v.as_int() {
                                    n.to_string()
                                } else if let Some(n) = v.as_float() {
                                    n.to_string()
                                } else if v.is_bool() {
                                    format!("{}", v.as_bool().unwrap_or(false))
                                } else if v.is_null() {
                                    "null".to_string()
                                } else if v.is_undefined() {
                                    "undefined".to_string()
                                } else {
                                    format!("{v:?}")
                                }
                            })
                            .collect();
                        info!(target: "nova::js::console", "{}", parts.join(" "));
                    })
                    .map_err(|e| NovaError::Internal(format!("failed to create console.log: {e}")))?;

                    console.set("log", log_fn.clone())
                        .map_err(|e| NovaError::Internal(format!("failed to set console.log: {e}")))?;
                    console.set("warn", log_fn.clone())
                        .map_err(|e| NovaError::Internal(format!("failed to set console.warn: {e}")))?;
                    console.set("error", log_fn)
                        .map_err(|e| NovaError::Internal(format!("failed to set console.error: {e}")))?;

                    globals.set("console", console)
                        .map_err(|e| NovaError::Internal(format!("failed to set console global: {e}")))?;

                    // Evaluate the script.
                    let eval_result: Result<rquickjs::Value, _> = ctx.eval(source.as_bytes());
                    match eval_result {
                        Ok(val) => {
                            let js_value = quickjs_to_jsvalue(&ctx, val);
                            debug!(result = ?js_value, "script execution succeeded");
                            Ok(TypedData::JsResult(js_value))
                        }
                        Err(e) => {
                            warn!(error = %e, "script execution error");
                            Ok(TypedData::JsResult(JsValue::String(format!(
                                "Error: {e}"
                            ))))
                        }
                    }
                });

                result
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
    async fn exec_basic_expression() {
        let mut m = JsMod::new();
        // Manually init the runtime for testing (no core needed).
        m.runtime = Some(Runtime::new().unwrap());

        let req = ContentRequest::ExecScript {
            source: "1 + 2".into(),
            context_id: None,
        };
        let result = m.handle(req).await.unwrap();
        match result {
            TypedData::JsResult(JsValue::Number(n)) => {
                assert!((n - 3.0).abs() < f64::EPSILON, "expected 3, got {n}");
            }
            other => panic!("expected JsResult(Number(3)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_string_result() {
        let mut m = JsMod::new();
        m.runtime = Some(Runtime::new().unwrap());

        let req = ContentRequest::ExecScript {
            source: "'hello' + ' ' + 'world'".into(),
            context_id: None,
        };
        let result = m.handle(req).await.unwrap();
        match result {
            TypedData::JsResult(JsValue::String(s)) => {
                assert_eq!(s, "hello world");
            }
            other => panic!("expected JsResult(String), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_console_log_no_crash() {
        let mut m = JsMod::new();
        m.runtime = Some(Runtime::new().unwrap());

        let req = ContentRequest::ExecScript {
            source: "console.log('hello from QuickJS')".into(),
            context_id: None,
        };
        // Should succeed without panic.
        let result = m.handle(req).await.unwrap();
        assert!(matches!(result, TypedData::JsResult(_)));
    }

    #[tokio::test]
    async fn exec_syntax_error_returns_error_string() {
        let mut m = JsMod::new();
        m.runtime = Some(Runtime::new().unwrap());

        let req = ContentRequest::ExecScript {
            source: "function {{{".into(),
            context_id: None,
        };
        let result = m.handle(req).await.unwrap();
        match result {
            TypedData::JsResult(JsValue::String(s)) => {
                assert!(s.contains("Error"), "expected error message, got: {s}");
            }
            other => panic!("expected JsResult(String) with error, got {other:?}"),
        }
    }
}

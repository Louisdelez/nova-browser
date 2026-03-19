//! # mod-js
//!
//! NOVA Mod for JavaScript execution via QuickJS. Handles the `ExecJavaScript`
//! capability.
//!
//! Uses `rquickjs` to create a QuickJS runtime and evaluate scripts. A single
//! persistent context is shared across all script executions within a page,
//! allowing scripts to share state (variables, functions, etc.).
//!
//! Provides minimal DOM API stubs (`document`, `window`, `console`) so scripts
//! don't crash when accessing common globals.

use std::sync::{Arc, Mutex};

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
    /// Persistent context shared across script executions.
    /// Must be declared before `runtime` so it is dropped first.
    context: Mutex<Option<Context>>,
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
            context: Mutex::new(None),
            runtime: None,
        }
    }

    /// Set up a fresh persistent context with console and DOM stubs.
    fn create_context(&self) -> Result<Context, NovaError> {
        let runtime = self.runtime.as_ref().ok_or_else(|| {
            NovaError::Internal("QuickJS runtime not initialized".into())
        })?;

        let context = Context::full(runtime).map_err(|e| {
            NovaError::Internal(format!("failed to create QuickJS context: {e}"))
        })?;

        context.with(|ctx| -> Result<(), NovaError> {
            let globals = ctx.globals();

            // ── console.log / warn / error ──
            install_console(&ctx, &globals)?;

            // ── DOM API stubs ──
            install_document_stub(&ctx, &globals)?;
            install_window_stub(&ctx, &globals)?;

            Ok(())
        })?;

        Ok(context)
    }
}

impl Default for JsMod {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JsMod {
    fn drop(&mut self) {
        // Must drop context before runtime to avoid GC assertion failures.
        // Clear all global stubs first to break reference cycles from closures.
        if let Some(ctx) = self.context.lock().unwrap().take() {
            ctx.with(|c| {
                let globals = c.globals();
                // Remove globals that hold closure references.
                for name in &[
                    "console", "document", "navigator", "location",
                    "addEventListener", "setTimeout", "setInterval",
                    "clearTimeout", "clearInterval", "requestAnimationFrame",
                    "getComputedStyle",
                ] {
                    globals.remove(*name).ok();
                }
            });
            drop(ctx);
        }
    }
}

/// Install `console.log`, `console.warn`, `console.error` that route to tracing.
fn install_console<'js>(
    ctx: &rquickjs::Ctx<'js>,
    globals: &rquickjs::Object<'js>,
) -> Result<(), NovaError> {
    let console = rquickjs::Object::new(ctx.clone())
        .map_err(|e| NovaError::Internal(format!("failed to create console: {e}")))?;

    let log_fn = Function::new(
        ctx.clone(),
        |args: rquickjs::function::Rest<rquickjs::Value>| {
            let parts: Vec<String> = args.0.iter().map(format_js_value).collect();
            info!(target: "nova::js::console", "{}", parts.join(" "));
        },
    )
    .map_err(|e| NovaError::Internal(format!("failed to create console.log: {e}")))?;

    let warn_fn = Function::new(
        ctx.clone(),
        |args: rquickjs::function::Rest<rquickjs::Value>| {
            let parts: Vec<String> = args.0.iter().map(format_js_value).collect();
            warn!(target: "nova::js::console", "{}", parts.join(" "));
        },
    )
    .map_err(|e| NovaError::Internal(format!("failed to create console.warn: {e}")))?;

    console
        .set("log", log_fn)
        .map_err(|e| NovaError::Internal(format!("failed to set console.log: {e}")))?;
    console
        .set("warn", warn_fn)
        .map_err(|e| NovaError::Internal(format!("failed to set console.warn: {e}")))?;
    // info alias
    let info_fn = Function::new(
        ctx.clone(),
        |args: rquickjs::function::Rest<rquickjs::Value>| {
            let parts: Vec<String> = args.0.iter().map(format_js_value).collect();
            info!(target: "nova::js::console", "{}", parts.join(" "));
        },
    )
    .map_err(|e| NovaError::Internal(format!("failed to create console.info: {e}")))?;
    console
        .set("info", info_fn)
        .map_err(|e| NovaError::Internal(format!("failed to set console.info: {e}")))?;
    // error fn
    let error_fn = Function::new(
        ctx.clone(),
        |args: rquickjs::function::Rest<rquickjs::Value>| {
            let parts: Vec<String> = args.0.iter().map(format_js_value).collect();
            warn!(target: "nova::js::console", "ERROR: {}", parts.join(" "));
        },
    )
    .map_err(|e| NovaError::Internal(format!("failed to create console.error: {e}")))?;
    console
        .set("error", error_fn)
        .map_err(|e| NovaError::Internal(format!("failed to set console.error: {e}")))?;

    globals
        .set("console", console)
        .map_err(|e| NovaError::Internal(format!("failed to set console global: {e}")))?;

    Ok(())
}

/// Install a minimal `document` stub with common methods that return null/empty.
fn install_document_stub<'js>(
    ctx: &rquickjs::Ctx<'js>,
    globals: &rquickjs::Object<'js>,
) -> Result<(), NovaError> {
    let document = rquickjs::Object::new(ctx.clone())
        .map_err(|e| NovaError::Internal(format!("failed to create document: {e}")))?;

    // document.getElementById() → null
    let get_by_id = Function::new(ctx.clone(), || -> () {})
        .map_err(|e| NovaError::Internal(format!("failed to create getElementById: {e}")))?;

    // document.querySelector() → null
    let query_selector = Function::new(ctx.clone(), || -> () {})
        .map_err(|e| NovaError::Internal(format!("failed to create querySelector: {e}")))?;

    // document.querySelectorAll() → []
    let query_all = Function::new(ctx.clone(), {
        let ctx2 = ctx.clone();
        move |_sel: rquickjs::Value| -> rquickjs::Array {
            rquickjs::Array::new(ctx2.clone()).unwrap()
        }
    })
    .map_err(|e| NovaError::Internal(format!("failed to create querySelectorAll: {e}")))?;

    // document.createElement() → a stub element object
    let create_element = Function::new(ctx.clone(), {
        let ctx2 = ctx.clone();
        move |tag: String| -> rquickjs::Object {
            let obj = rquickjs::Object::new(ctx2.clone()).unwrap();
            obj.set("tagName", tag.to_uppercase()).ok();
            obj.set("innerHTML", "").ok();
            obj.set("textContent", "").ok();
            let style = rquickjs::Object::new(ctx2.clone()).unwrap();
            obj.set("style", style).ok();
            obj
        }
    })
    .map_err(|e| NovaError::Internal(format!("failed to create createElement: {e}")))?;

    // document.addEventListener() → no-op
    let add_event = Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| {})
        .map_err(|e| NovaError::Internal(format!("failed to create addEventListener: {e}")))?;

    // document.createTextNode() → stub
    let create_text = Function::new(ctx.clone(), {
        let ctx2 = ctx.clone();
        move |text: String| -> rquickjs::Object {
            let obj = rquickjs::Object::new(ctx2.clone()).unwrap();
            obj.set("textContent", text).ok();
            obj.set("nodeType", 3).ok();
            obj
        }
    })
    .map_err(|e| NovaError::Internal(format!("failed to create createTextNode: {e}")))?;

    document.set("getElementById", get_by_id).ok();
    document.set("querySelector", query_selector).ok();
    document.set("querySelectorAll", query_all).ok();
    document.set("createElement", create_element).ok();
    document.set("addEventListener", add_event.clone()).ok();
    document.set("createTextNode", create_text).ok();
    document.set("readyState", "complete").ok();
    document.set("title", "").ok();

    // document.body → stub
    let body = rquickjs::Object::new(ctx.clone())
        .map_err(|e| NovaError::Internal(format!("failed to create body: {e}")))?;
    let body_add_event =
        Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| {})
            .map_err(|e| NovaError::Internal(format!("failed to create body event: {e}")))?;
    let body_append =
        Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| {})
            .map_err(|e| NovaError::Internal(format!("failed to create appendChild: {e}")))?;
    body.set("addEventListener", body_add_event).ok();
    body.set("appendChild", body_append).ok();
    body.set("innerHTML", "").ok();
    document.set("body", body).ok();

    // document.head → stub
    let head = rquickjs::Object::new(ctx.clone())
        .map_err(|e| NovaError::Internal(format!("failed to create head: {e}")))?;
    let head_append =
        Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| {})
            .map_err(|e| NovaError::Internal(format!("failed to create head.appendChild: {e}")))?;
    head.set("appendChild", head_append).ok();
    document.set("head", head).ok();

    // document.documentElement → stub
    let doc_el = rquickjs::Object::new(ctx.clone())
        .map_err(|e| NovaError::Internal(format!("failed to create documentElement: {e}")))?;
    doc_el.set("lang", "en").ok();
    document.set("documentElement", doc_el).ok();

    globals
        .set("document", document)
        .map_err(|e| NovaError::Internal(format!("failed to set document global: {e}")))?;

    Ok(())
}

/// Install a minimal `window` stub with common properties.
fn install_window_stub<'js>(
    ctx: &rquickjs::Ctx<'js>,
    globals: &rquickjs::Object<'js>,
) -> Result<(), NovaError> {
    // window.addEventListener() → no-op
    let add_event = Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| {})
        .map_err(|e| NovaError::Internal(format!("failed to create window.addEventListener: {e}")))?;

    // window.setTimeout() → returns 0 (doesn't actually schedule)
    let set_timeout =
        Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| -> i32 { 0 })
            .map_err(|e| NovaError::Internal(format!("failed to create setTimeout: {e}")))?;

    // window.setInterval() → returns 0
    let set_interval =
        Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| -> i32 { 0 })
            .map_err(|e| NovaError::Internal(format!("failed to create setInterval: {e}")))?;

    // window.clearTimeout / clearInterval → no-op
    let clear =
        Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| {})
            .map_err(|e| NovaError::Internal(format!("failed to create clearTimeout: {e}")))?;

    // window.requestAnimationFrame → returns 0
    let raf =
        Function::new(ctx.clone(), |_: rquickjs::function::Rest<rquickjs::Value>| -> i32 { 0 })
            .map_err(|e| NovaError::Internal(format!("failed to create requestAnimationFrame: {e}")))?;

    // window.getComputedStyle → returns empty object
    let get_style = Function::new(ctx.clone(), {
        let ctx2 = ctx.clone();
        move |_: rquickjs::function::Rest<rquickjs::Value>| -> rquickjs::Object {
            rquickjs::Object::new(ctx2.clone()).unwrap()
        }
    })
    .map_err(|e| NovaError::Internal(format!("failed to create getComputedStyle: {e}")))?;

    // Set on globals (which IS window in a browser context).
    globals.set("addEventListener", add_event).ok();
    globals.set("setTimeout", set_timeout).ok();
    globals.set("setInterval", set_interval).ok();
    globals.set("clearTimeout", clear.clone()).ok();
    globals.set("clearInterval", clear).ok();
    globals.set("requestAnimationFrame", raf).ok();
    globals.set("getComputedStyle", get_style).ok();
    globals.set("innerWidth", 1280).ok();
    globals.set("innerHeight", 720).ok();
    globals.set("devicePixelRatio", 1.0f64).ok();

    // navigator stub
    let navigator = rquickjs::Object::new(ctx.clone())
        .map_err(|e| NovaError::Internal(format!("failed to create navigator: {e}")))?;
    navigator.set("userAgent", "NOVA/0.1.0").ok();
    navigator.set("language", "en").ok();
    navigator.set("platform", "Linux").ok();
    globals.set("navigator", navigator).ok();

    // location stub
    let location = rquickjs::Object::new(ctx.clone())
        .map_err(|e| NovaError::Internal(format!("failed to create location: {e}")))?;
    location.set("href", "").ok();
    location.set("hostname", "").ok();
    location.set("pathname", "/").ok();
    location.set("protocol", "https:").ok();
    globals.set("location", location).ok();

    // Note: We don't set window = globalThis to avoid circular reference
    // that prevents GC cleanup. Scripts can use `globalThis` instead.

    Ok(())
}

/// Format a QuickJS value to a human-readable string for console output.
fn format_js_value(v: &rquickjs::Value) -> String {
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

        // Create the persistent context with all stubs installed.
        let context = self.create_context()?;
        *self.context.lock().unwrap() = Some(context);

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

                let ctx_guard = self.context.lock().unwrap();
                let context = ctx_guard.as_ref().ok_or_else(|| {
                    NovaError::Internal("QuickJS context not initialized".into())
                })?;

                let result = context.with(|ctx| {
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
        // Drop the context.
        *self.context.lock().unwrap() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_mod() -> JsMod {
        let mut m = JsMod::new();
        let rt = Runtime::new().unwrap();
        m.runtime = Some(rt);
        let ctx = m.create_context().unwrap();
        *m.context.lock().unwrap() = Some(ctx);
        m
    }

    #[test]
    fn manifest_provides_js() {
        let m = JsMod::new();
        assert!(m.manifest().provides(&CapabilityType::ExecJavaScript));
    }

    #[tokio::test]
    async fn exec_basic_expression() {
        let m = make_mod();
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
        let m = make_mod();
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
        let m = make_mod();
        let req = ContentRequest::ExecScript {
            source: "console.log('hello from QuickJS')".into(),
            context_id: None,
        };
        let result = m.handle(req).await.unwrap();
        assert!(matches!(result, TypedData::JsResult(_)));
    }

    #[tokio::test]
    async fn exec_syntax_error_returns_error_string() {
        let m = make_mod();
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

    #[tokio::test]
    async fn persistent_context_shares_state() {
        let m = make_mod();

        // Set a variable in the first execution.
        let req1 = ContentRequest::ExecScript {
            source: "var myVar = 42;".into(),
            context_id: None,
        };
        m.handle(req1).await.unwrap();

        // Read it back in a second execution.
        let req2 = ContentRequest::ExecScript {
            source: "myVar".into(),
            context_id: None,
        };
        let result = m.handle(req2).await.unwrap();
        match result {
            TypedData::JsResult(JsValue::Number(n)) => {
                assert!((n - 42.0).abs() < f64::EPSILON, "expected 42, got {n}");
            }
            other => panic!("expected JsResult(Number(42)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn document_stubs_dont_crash() {
        let m = make_mod();
        // These should all run without crashing.
        for script in &[
            "document.getElementById('foo')",
            "document.querySelector('.bar')",
            "document.querySelectorAll('div')",
            "document.createElement('div')",
            "document.addEventListener('click', function(){})",
            "document.readyState",
            "document.body.innerHTML",
        ] {
            let req = ContentRequest::ExecScript {
                source: script.to_string(),
                context_id: None,
            };
            let result = m.handle(req).await;
            assert!(result.is_ok(), "script '{script}' should not crash");
        }
    }

    #[tokio::test]
    async fn window_stubs_dont_crash() {
        let m = make_mod();
        for script in &[
            "window.addEventListener('load', function(){})",
            "setTimeout(function(){}, 100)",
            "setInterval(function(){}, 1000)",
            "clearTimeout(0)",
            "navigator.userAgent",
            "innerWidth",
            "location.href",
        ] {
            let req = ContentRequest::ExecScript {
                source: script.to_string(),
                context_id: None,
            };
            let result = m.handle(req).await;
            assert!(result.is_ok(), "script '{script}' should not crash");
        }
    }
}

//! # quickjs_runtime
//!
//! Wraps rquickjs [`Runtime`] and [`Context`] to provide a real JavaScript
//! engine with full language support (loops, closures, Promises, etc.) and a
//! DOM bridge via [`JsDomTree`].
//!
//! ## Usage
//!
//! ```ignore
//! let tree = JsDomTree::from_dom(&dom);
//! let mut rt = QuickJsRuntime::new(tree)?;
//! let result = rt.eval("1 + 2")?;
//! assert_eq!(result, JsValue::Number(3.0));
//! ```

use std::sync::{Arc, Mutex};

use rquickjs::{Context, Runtime, Value};
use tracing::{debug, warn};

use nova_mod_api::content::JsValue;

use crate::dom_api::JsDomTree;
use crate::dom_bridge::{self, ConsoleLog};
use crate::xhr;

/// Errors specific to the QuickJS runtime.
#[derive(Debug, thiserror::Error)]
pub enum QuickJsError {
    /// Failed to create the QuickJS runtime.
    #[error("failed to create QuickJS runtime: {0}")]
    RuntimeCreation(String),
    /// Failed to evaluate a script.
    #[error("script evaluation error: {0}")]
    EvalError(String),
}

/// A wrapper around a rquickjs `Runtime` + `Context` with DOM bridge.
pub struct QuickJsRuntime {
    /// The rquickjs runtime (owns the JS heap).
    _runtime: Runtime,
    /// The rquickjs context (global environment).
    context: Context,
    /// The shared DOM tree.
    tree: Arc<Mutex<JsDomTree>>,
    /// Collected console output.
    console_log: ConsoleLog,
}

impl QuickJsRuntime {
    /// Create a new QuickJS runtime with a DOM tree bridge.
    ///
    /// Registers `window`, `document`, and `console` globals.
    pub fn new(tree: Arc<Mutex<JsDomTree>>) -> Result<Self, QuickJsError> {
        Self::with_url(tree, "about:blank")
    }

    /// Create a new runtime with a specific page URL.
    pub fn with_url(
        tree: Arc<Mutex<JsDomTree>>,
        url: &str,
    ) -> Result<Self, QuickJsError> {
        let runtime = Runtime::new()
            .map_err(|e| QuickJsError::RuntimeCreation(format!("{e}")))?;
        let context = Context::full(&runtime)
            .map_err(|e| QuickJsError::RuntimeCreation(format!("{e}")))?;

        let console_log: ConsoleLog = Arc::new(Mutex::new(Vec::new()));

        let tree_ref = tree.clone();
        let log_ref = console_log.clone();
        let url_owned = url.to_owned();

        context.with(|ctx| -> Result<(), QuickJsError> {
            let globals = ctx.globals();

            // console
            let console = dom_bridge::create_console(&ctx, log_ref)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("console: {e}")))?;
            globals.set("console", console)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("{e}")))?;

            // window
            let window = dom_bridge::create_window(&ctx, &url_owned)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("window: {e}")))?;
            globals.set("window", window)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("{e}")))?;

            // Register __nova bridge functions
            dom_bridge::register_bridge_functions(&ctx, &tree_ref)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("bridge: {e}")))?;

            // Install the JS DOM shim (creates document, Element, etc.)
            ctx.eval::<(), _>(dom_bridge::JS_DOM_SHIM)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("shim: {e}")))?;

            // Install XMLHttpRequest class.
            ctx.eval::<(), _>(xhr::JS_XHR_SHIM)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("xhr: {e}")))?;

            // Install the WebSocket API shim.
            ctx.eval::<(), _>(crate::websocket::JS_WEBSOCKET_SHIM)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("websocket shim: {e}")))?;

            // Install Service Worker stubs.
            ctx.eval::<(), _>(crate::service_worker::JS_SERVICE_WORKER_SHIM)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("service worker shim: {e}")))?;

            // Install Web API stubs (IntersectionObserver, MutationObserver, etc.).
            ctx.eval::<(), _>(crate::web_apis::JS_WEB_APIS_SHIM)
                .map_err(|e| QuickJsError::RuntimeCreation(format!("web apis shim: {e}")))?;

            Ok(())
        })?;

        debug!("QuickJS runtime created");

        Ok(Self {
            _runtime: runtime,
            context,
            tree,
            console_log,
        })
    }

    /// Evaluate a JavaScript source string and return a [`JsValue`].
    pub fn eval(&self, source: &str) -> JsValue {
        self.context.with(|ctx| {
            match ctx.eval::<Value<'_>, _>(source) {
                Ok(val) => convert_value(&val),
                Err(e) => {
                    warn!(error = %e, "QuickJS eval error");
                    JsValue::String(format!("Error: {e}"))
                }
            }
        })
    }

    /// Evaluate a script, applying mutations to the linked DOM tree.
    ///
    /// Re-registers the bridge functions before evaluation to ensure
    /// the document object reflects the current tree state.
    ///
    /// Scripts are wrapped in a try/catch at the JS level so that
    /// unhandled exceptions do not crash the browser or prevent the
    /// page from rendering. Errors are logged and the best DOM state
    /// is preserved.
    pub fn eval_with_dom(&self, source: &str) -> JsValue {
        let tree_ref = self.tree.clone();
        self.context.with(|ctx| {
            // Re-register bridge functions for potentially updated tree.
            if let Err(e) = dom_bridge::register_bridge_functions(&ctx, &tree_ref) {
                warn!(error = %e, "failed to re-register bridge");
            }
            // Re-install the DOM shim.
            let _ = ctx.eval::<(), _>(dom_bridge::JS_DOM_SHIM);

            // Wrap the script in a try/catch so that unhandled errors
            // do not prevent subsequent scripts from running or the
            // DOM from being returned in its current (partial) state.
            let wrapped = format!(
                "try {{ {source}\n}} catch(__nova_err) {{ console.error('[NOVA] Script error: ' + __nova_err); undefined; }}"
            );

            match ctx.eval::<Value<'_>, _>(wrapped.as_str()) {
                Ok(val) => convert_value(&val),
                Err(e) => {
                    warn!(error = %e, "QuickJS eval error");
                    JsValue::String(format!("Error: {e}"))
                }
            }
        })
    }

    /// Get the collected console output.
    pub fn console_output(&self) -> Vec<String> {
        self.console_log.lock().unwrap().clone()
    }

    /// Get the shared DOM tree.
    pub fn tree(&self) -> &Arc<Mutex<JsDomTree>> {
        &self.tree
    }
}

// ── Value conversion ─────────────────────────────────────────────────────────

/// Convert a rquickjs `Value` to a nova-mod-api `JsValue`.
fn convert_value(val: &Value<'_>) -> JsValue {
    if val.is_undefined() {
        JsValue::Undefined
    } else if val.is_null() {
        JsValue::Null
    } else if val.is_bool() {
        JsValue::Boolean(val.as_bool().unwrap_or(false))
    } else if val.is_int() {
        JsValue::Number(val.as_int().unwrap_or(0) as f64)
    } else if val.is_float() {
        JsValue::Number(val.as_float().unwrap_or(0.0))
    } else if val.is_string() {
        let s = val
            .as_string()
            .and_then(|s| s.to_string().ok())
            .unwrap_or_default();
        JsValue::String(s)
    } else if val.is_array() {
        if let Some(arr) = val.as_array() {
            let items: Vec<JsValue> = (0..arr.len())
                .filter_map(|i| arr.get::<Value<'_>>(i).ok())
                .map(|v| convert_value(&v))
                .collect();
            JsValue::Array(items)
        } else {
            JsValue::Undefined
        }
    } else if val.is_object() {
        if let Some(obj) = val.as_object() {
            if let Ok(handle) = obj.get::<_, f64>("__handle") {
                return JsValue::Number(handle);
            }
        }
        JsValue::Object(Vec::new())
    } else {
        JsValue::Undefined
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::content::DomNode;

    fn make_test_dom() -> DomNode {
        DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "head".into(),
                        attributes: vec![],
                        children: vec![DomNode::Element {
                            tag: "title".into(),
                            attributes: vec![],
                            children: vec![DomNode::Text("Test Page".into())],
                        }],
                    },
                    DomNode::Element {
                        tag: "body".into(),
                        attributes: vec![],
                        children: vec![
                            DomNode::Element {
                                tag: "div".into(),
                                attributes: vec![("id".into(), "main".into())],
                                children: vec![DomNode::Text("Hello".into())],
                            },
                            DomNode::Element {
                                tag: "p".into(),
                                attributes: vec![
                                    ("class".into(), "intro".into()),
                                    ("id".into(), "para".into()),
                                ],
                                children: vec![DomNode::Text("World".into())],
                            },
                            DomNode::Element {
                                tag: "button".into(),
                                attributes: vec![("id".into(), "btn".into())],
                                children: vec![DomNode::Text("Click me".into())],
                            },
                        ],
                    },
                ],
            }],
        }
    }

    #[test]
    fn basic_arithmetic() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval("1 + 2");
        assert_eq!(result, JsValue::Number(3.0));
    }

    #[test]
    fn for_loop() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval("var sum = 0; for (var i = 0; i < 10; i++) { sum += i; } sum");
        assert_eq!(result, JsValue::Number(45.0));
    }

    #[test]
    fn if_else() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval("var x = 10; if (x > 5) { 'big'; } else { 'small'; }");
        assert_eq!(result, JsValue::String("big".into()));
    }

    #[test]
    fn function_declaration() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval("function add(a, b) { return a + b; } add(3, 4)");
        assert_eq!(result, JsValue::Number(7.0));
    }

    #[test]
    fn promise_resolve() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval("var p = Promise.resolve(42); typeof p");
        assert_eq!(result, JsValue::String("object".into()));
    }

    #[test]
    fn dom_get_element_by_id() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval_with_dom(r#"
            var el = document.getElementById("main");
            el !== null ? el.tagName : "not found";
        "#);
        assert_eq!(result, JsValue::String("DIV".into()));
    }

    #[test]
    fn dom_create_element() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var span = document.createElement("span");
            span.setAttribute("id", "new-span");
            var main = document.getElementById("main");
            main.appendChild(span);
        "#);

        let t = tree.lock().unwrap();
        assert!(
            t.get_element_by_id("new-span").is_some(),
            "new element should exist in DOM"
        );
    }

    #[test]
    fn dom_set_attribute() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var el = document.getElementById("main");
            el.setAttribute("data-value", "42");
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("main").unwrap();
        assert_eq!(t.get_attribute(handle, "data-value"), Some("42".into()));
    }

    #[test]
    fn dom_query_selector() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval_with_dom(r#"
            var el = document.querySelector(".intro");
            el !== null ? el.tagName : "not found";
        "#);
        assert_eq!(result, JsValue::String("P".into()));
    }

    #[test]
    fn console_log_collects() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        rt.eval(r#"console.log("hello world")"#);
        let output = rt.console_output();
        assert_eq!(output.len(), 1);
        assert!(output[0].contains("hello world"));
    }

    #[test]
    fn dom_class_list() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var el = document.getElementById("para");
            el.classList.add("active");
            el.classList.remove("intro");
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("para").unwrap();
        assert!(t.class_list_contains(handle, "active"));
        assert!(!t.class_list_contains(handle, "intro"));
    }

    #[test]
    fn dom_style_set_property() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var el = document.getElementById("main");
            el.style.setProperty("color", "red");
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("main").unwrap();
        assert_eq!(t.style_get_property(handle, "color"), Some("red".into()));
    }

    #[test]
    fn while_loop_and_closures() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval(r#"
            var arr = [];
            var i = 0;
            while (i < 5) {
                arr.push(i * i);
                i++;
            }
            arr.length;
        "#);
        assert_eq!(result, JsValue::Number(5.0));
    }

    #[test]
    fn arrow_functions() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval(r#"
            var double = (x) => x * 2;
            double(21);
        "#);
        assert_eq!(result, JsValue::Number(42.0));
    }

    #[test]
    fn string_operations() {
        let tree = JsDomTree::from_dom(&DomNode::Document { children: vec![] });
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval(r#"
            var s = "hello" + " " + "world";
            s.toUpperCase();
        "#);
        assert_eq!(result, JsValue::String("HELLO WORLD".into()));
    }

    #[test]
    fn dom_has_remove_attribute() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        let result = rt.eval_with_dom(r#"
            var el = document.getElementById("main");
            var before = el.hasAttribute("id");
            el.removeAttribute("id");
            var after = el.hasAttribute("id");
            before && !after;
        "#);
        assert!(matches!(result, JsValue::Boolean(true)));
    }

    #[test]
    fn dom_query_selector_all() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval_with_dom(r#"
            var divs = document.querySelectorAll("div");
            divs.length;
        "#);
        assert_eq!(result, JsValue::Number(1.0));
    }

    #[test]
    fn dom_text_content_getter_setter() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var el = document.getElementById("main");
            el.textContent = "Changed!";
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("main").unwrap();
        assert_eq!(t.get_text_content(handle), "Changed!");
    }

    #[test]
    fn dom_create_text_node() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var text = document.createTextNode("New text");
            var main = document.getElementById("main");
            main.appendChild(text);
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("main").unwrap();
        let content = t.get_text_content(handle);
        assert!(content.contains("New text"), "expected 'New text' in '{content}'");
    }

    // ── Canvas 2D API tests ───────────────────────────────────────────────

    fn make_canvas_dom() -> DomNode {
        DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "body".into(),
                    attributes: vec![],
                    children: vec![DomNode::Element {
                        tag: "canvas".into(),
                        attributes: vec![
                            ("id".into(), "c".into()),
                            ("width".into(), "100".into()),
                            ("height".into(), "100".into()),
                        ],
                        children: vec![],
                    }],
                }],
            }],
        }
    }

    #[test]
    fn canvas_get_context_2d() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        let result = rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            var ctx = c.getContext("2d");
            ctx !== null ? "ok" : "fail";
        "#);
        assert_eq!(result, JsValue::String("ok".into()));
    }

    #[test]
    fn canvas_fill_rect_modifies_pixels() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            var ctx = c.getContext("2d");
            ctx.fillStyle = "red";
            ctx.fillRect(0, 0, 50, 50);
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("c").unwrap();
        let canvas_ctx = t.get_canvas_context_ref(handle).unwrap();
        // First pixel should be red (255, 0, 0, 255).
        assert_eq!(canvas_ctx.pixels[0], 255);
        assert_eq!(canvas_ctx.pixels[1], 0);
        assert_eq!(canvas_ctx.pixels[2], 0);
        assert_eq!(canvas_ctx.pixels[3], 255);
    }

    #[test]
    fn canvas_stroke_rect() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            var ctx = c.getContext("2d");
            ctx.strokeStyle = "blue";
            ctx.lineWidth = 2;
            ctx.strokeRect(10, 10, 30, 30);
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("c").unwrap();
        let canvas_ctx = t.get_canvas_context_ref(handle).unwrap();
        // Pixel at (15, 10) should be blue (on top edge).
        let idx = (10 * 100 + 15) * 4;
        assert_eq!(canvas_ctx.pixels[idx], 0);
        assert_eq!(canvas_ctx.pixels[idx + 1], 0);
        assert_eq!(canvas_ctx.pixels[idx + 2], 255);
    }

    #[test]
    fn canvas_clear_rect() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            var ctx = c.getContext("2d");
            ctx.fillStyle = "green";
            ctx.fillRect(0, 0, 100, 100);
            ctx.clearRect(10, 10, 20, 20);
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("c").unwrap();
        let canvas_ctx = t.get_canvas_context_ref(handle).unwrap();
        // Cleared pixel at (15, 15) should be transparent.
        let idx = (15 * 100 + 15) * 4;
        assert_eq!(canvas_ctx.pixels[idx + 3], 0);
        // Non-cleared pixel should still be green.
        assert_eq!(canvas_ctx.pixels[0], 0);
        assert_eq!(canvas_ctx.pixels[1], 128);
        assert_eq!(canvas_ctx.pixels[2], 0);
        assert_eq!(canvas_ctx.pixels[3], 255);
    }

    #[test]
    fn canvas_path_and_fill() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            var ctx = c.getContext("2d");
            ctx.fillStyle = "red";
            ctx.beginPath();
            ctx.rect(0, 0, 20, 20);
            ctx.fill();
        "#);

        let t = tree.lock().unwrap();
        let handle = t.get_element_by_id("c").unwrap();
        let canvas_ctx = t.get_canvas_context_ref(handle).unwrap();
        // Pixel at (10, 10) should be red.
        let idx = (10 * 100 + 10) * 4;
        assert_eq!(canvas_ctx.pixels[idx], 255);
        assert_eq!(canvas_ctx.pixels[idx + 3], 255);
    }

    #[test]
    fn canvas_save_restore() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        let result = rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            var ctx = c.getContext("2d");
            ctx.fillStyle = "red";
            ctx.save();
            ctx.fillStyle = "blue";
            ctx.restore();
            ctx.fillStyle;
        "#);
        // After restore, fillStyle should be back to red.
        assert_eq!(result, JsValue::String("#ff0000".into()));
    }

    #[test]
    fn canvas_measure_text() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        let result = rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            var ctx = c.getContext("2d");
            var m = ctx.measureText("hello");
            m.width > 0 ? "ok" : "fail";
        "#);
        assert_eq!(result, JsValue::String("ok".into()));
    }

    #[test]
    fn canvas_get_context_returns_null_for_non_canvas() {
        let tree = JsDomTree::from_dom(&make_test_dom());
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval_with_dom(r#"
            var el = document.getElementById("main");
            var ctx = el.getContext("2d");
            ctx === null ? "null" : "not null";
        "#);
        assert_eq!(result, JsValue::String("null".into()));
    }

    #[test]
    fn canvas_dimensions() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree).unwrap();
        let result = rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            c.width + "x" + c.height;
        "#);
        assert_eq!(result, JsValue::String("100x100".into()));
    }

    #[test]
    fn canvas_collect_pixels() {
        let tree = JsDomTree::from_dom(&make_canvas_dom());
        let rt = QuickJsRuntime::new(tree.clone()).unwrap();
        rt.eval_with_dom(r#"
            var c = document.getElementById("c");
            var ctx = c.getContext("2d");
            ctx.fillStyle = "red";
            ctx.fillRect(0, 0, 10, 10);
        "#);

        let t = tree.lock().unwrap();
        let canvases = t.collect_canvas_pixels();
        assert_eq!(canvases.len(), 1);
        let (id, w, h, pixels) = &canvases[0];
        assert_eq!(id, "c");
        assert_eq!(*w, 100);
        assert_eq!(*h, 100);
        assert_eq!(pixels.len(), 100 * 100 * 4);
        // First pixel should be red.
        assert_eq!(pixels[0], 255);
        assert_eq!(pixels[1], 0);
        assert_eq!(pixels[2], 0);
    }
}

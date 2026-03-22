//! # dom_bridge
//!
//! Creates JavaScript-visible objects (document, elements, classList, style,
//! events) that bridge to the [`JsDomTree`] via rquickjs closures and plain
//! JS objects.
//!
//! ## Design
//!
//! Since rquickjs closures passed to `Function::new` have strict lifetime
//! requirements, we use a two-layer approach:
//!
//! 1. **Rust-side**: register simple bridge functions that work with primitive
//!    types (handles as numbers, strings). These are stored on a hidden
//!    `__nova` global.
//!
//! 2. **JS-side**: define `document`, `Element`, etc. as JavaScript wrappers
//!    that call the bridge functions and wrap results.
//!
//! This avoids the `'js` lifetime issues with returning `Object<'js>` from
//! closures, while still providing a full DOM API surface to scripts.

use std::sync::{Arc, Mutex};

use rquickjs::{Ctx, Object, Result};
use rquickjs::prelude::{Func, MutFn};

use crate::dom_api::{ElementHandle, JsDomTree};

// ── Console ──────────────────────────────────────────────────────────────────

/// Collected console output lines.
pub type ConsoleLog = Arc<Mutex<Vec<String>>>;

/// Create a `console` JS object with all standard methods.
///
/// Supports: `log`, `warn`, `error`, `info`, `debug`, `trace`, `table`,
/// `group`, `groupEnd`, `time`, `timeEnd`, `count`, `clear`, `assert`, `dir`.
pub fn create_console<'js>(ctx: &Ctx<'js>, log_store: ConsoleLog) -> Result<Object<'js>> {
    let console = Object::new(ctx.clone())?;

    let store = log_store.clone();
    console.set("log", Func::from(MutFn::new(move |msg: String| {
        tracing::debug!(msg = %msg, "console.log");
        store.lock().unwrap().push(format!("[LOG] {msg}"));
    })))?;

    let store = log_store.clone();
    console.set("warn", Func::from(MutFn::new(move |msg: String| {
        tracing::warn!(msg = %msg, "console.warn");
        store.lock().unwrap().push(format!("[WARN] {msg}"));
    })))?;

    let store = log_store.clone();
    console.set("error", Func::from(MutFn::new(move |msg: String| {
        tracing::error!(msg = %msg, "console.error");
        store.lock().unwrap().push(format!("[ERROR] {msg}"));
    })))?;

    let store = log_store.clone();
    console.set("info", Func::from(MutFn::new(move |msg: String| {
        tracing::info!(msg = %msg, "console.info");
        store.lock().unwrap().push(format!("[INFO] {msg}"));
    })))?;

    let store = log_store.clone();
    console.set("debug", Func::from(MutFn::new(move |msg: String| {
        tracing::debug!(msg = %msg, "console.debug");
        store.lock().unwrap().push(format!("[DEBUG] {msg}"));
    })))?;

    let store = log_store.clone();
    console.set("trace", Func::from(MutFn::new(move |msg: String| {
        tracing::trace!(msg = %msg, "console.trace");
        store.lock().unwrap().push(format!("[TRACE] {msg}"));
    })))?;

    let store = log_store.clone();
    console.set("table", Func::from(MutFn::new(move |msg: String| {
        tracing::debug!(msg = %msg, "console.table");
        store.lock().unwrap().push(format!("[TABLE] {msg}"));
    })))?;

    let store = log_store.clone();
    console.set("group", Func::from(MutFn::new(move |msg: String| {
        tracing::debug!(msg = %msg, "console.group");
        store.lock().unwrap().push(format!("[GROUP] {msg}"));
    })))?;

    console.set("groupEnd", Func::from(|| {}))?;
    console.set("groupCollapsed", Func::from(|_msg: String| {}))?;

    let store = log_store.clone();
    console.set("time", Func::from(MutFn::new(move |label: String| {
        tracing::debug!(label = %label, "console.time");
        store.lock().unwrap().push(format!("[TIME] {label}: start"));
    })))?;

    let store = log_store.clone();
    console.set("timeEnd", Func::from(MutFn::new(move |label: String| {
        tracing::debug!(label = %label, "console.timeEnd");
        store.lock().unwrap().push(format!("[TIME] {label}: end"));
    })))?;

    let store = log_store.clone();
    console.set("timeLog", Func::from(MutFn::new(move |label: String| {
        tracing::debug!(label = %label, "console.timeLog");
        store.lock().unwrap().push(format!("[TIME] {label}: log"));
    })))?;

    let store = log_store.clone();
    console.set("count", Func::from(MutFn::new(move |label: String| {
        tracing::debug!(label = %label, "console.count");
        store.lock().unwrap().push(format!("[COUNT] {label}"));
    })))?;

    console.set("countReset", Func::from(|_label: String| {}))?;

    let store = log_store.clone();
    console.set("clear", Func::from(MutFn::new(move || {
        tracing::debug!("console.clear");
        store.lock().unwrap().clear();
    })))?;

    let store = log_store.clone();
    console.set("assert", Func::from(MutFn::new(move |condition: bool, msg: String| {
        if !condition {
            tracing::error!(msg = %msg, "console.assert failed");
            store.lock().unwrap().push(format!("[ASSERT] Assertion failed: {msg}"));
        }
    })))?;

    let store = log_store.clone();
    console.set("dir", Func::from(MutFn::new(move |msg: String| {
        tracing::debug!(msg = %msg, "console.dir");
        store.lock().unwrap().push(format!("[DIR] {msg}"));
    })))?;

    console.set("dirxml", Func::from(|_msg: String| {}))?;

    Ok(console)
}

// ── Window ───────────────────────────────────────────────────────────────────

/// Create a `window` JS object with basic properties.
///
/// The navigator properties mimic Chrome to avoid user-agent sniffing
/// that would redirect to unsupported-browser pages (e.g. Google).
pub fn create_window<'js>(ctx: &Ctx<'js>, url: &str) -> Result<Object<'js>> {
    let window = Object::new(ctx.clone())?;

    // ── location ──────────────────────────────────────────────────────
    let location = Object::new(ctx.clone())?;
    location.set("href", url)?;
    location.set("protocol", "https:")?;
    location.set("host", "")?;
    location.set("hostname", "")?;
    location.set("pathname", "/")?;
    location.set("search", "")?;
    location.set("hash", "")?;
    location.set("origin", "")?;
    // Parse URL parts if possible.
    if let Ok(parsed) = url::Url::parse(url) {
        location.set("protocol", format!("{}:", parsed.scheme()))?;
        location.set("host", parsed.host_str().unwrap_or(""))?;
        location.set("hostname", parsed.host_str().unwrap_or(""))?;
        location.set("pathname", parsed.path())?;
        location.set("search", if parsed.query().is_some() { format!("?{}", parsed.query().unwrap()) } else { String::new() })?;
        location.set("hash", if let Some(f) = parsed.fragment() { format!("#{f}") } else { String::new() })?;
        location.set("origin", parsed.origin().ascii_serialization())?;
    }
    window.set("location", location)?;

    // ── navigator — mimic Chrome UA to avoid user-agent sniffing ─────
    let navigator = Object::new(ctx.clone())?;
    navigator.set("userAgent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")?;
    navigator.set("language", "en-US")?;
    navigator.set("languages", vec!["en-US", "en"])?;
    navigator.set("platform", "Linux x86_64")?;
    navigator.set("vendor", "Google Inc.")?;
    navigator.set("appVersion", "5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")?;
    navigator.set("appName", "Netscape")?;
    navigator.set("product", "Gecko")?;
    navigator.set("productSub", "20030107")?;
    navigator.set("cookieEnabled", true)?;
    navigator.set("onLine", true)?;
    navigator.set("maxTouchPoints", 0)?;
    navigator.set("hardwareConcurrency", 4)?;
    navigator.set("doNotTrack", rquickjs::Null)?;

    // ── navigator.clipboard stub ────────────────────────────────
    let clipboard = Object::new(ctx.clone())?;
    clipboard.set("writeText", Func::from(|_text: String| {
        tracing::debug!("navigator.clipboard.writeText() — stub");
    }))?;
    clipboard.set("readText", Func::from(|| -> String {
        tracing::debug!("navigator.clipboard.readText() — stub");
        String::new()
    }))?;
    navigator.set("clipboard", clipboard)?;

    window.set("navigator", navigator)?;

    // ── dimensions ───────────────────────────────────────────────────
    window.set("innerWidth", 1920)?;
    window.set("innerHeight", 1080)?;
    window.set("outerWidth", 1920)?;
    window.set("outerHeight", 1080)?;
    window.set("devicePixelRatio", 1.0_f64)?;
    window.set("pageXOffset", 0)?;
    window.set("pageYOffset", 0)?;
    window.set("scrollX", 0)?;
    window.set("scrollY", 0)?;

    // ── screen ───────────────────────────────────────────────────────
    let screen = Object::new(ctx.clone())?;
    screen.set("width", 1920)?;
    screen.set("height", 1080)?;
    screen.set("availWidth", 1920)?;
    screen.set("availHeight", 1080)?;
    screen.set("colorDepth", 24)?;
    screen.set("pixelDepth", 24)?;
    window.set("screen", screen)?;

    // ── performance ──────────────────────────────────────────────────
    let performance = Object::new(ctx.clone())?;
    let perf_timing = Object::new(ctx.clone())?;
    perf_timing.set("navigationStart", 0)?;
    perf_timing.set("fetchStart", 0)?;
    perf_timing.set("domContentLoadedEventStart", 0)?;
    perf_timing.set("domContentLoadedEventEnd", 0)?;
    perf_timing.set("loadEventStart", 0)?;
    perf_timing.set("loadEventEnd", 0)?;
    performance.set("timing", perf_timing)?;

    let perf_navigation = Object::new(ctx.clone())?;
    perf_navigation.set("type", 0)?;
    perf_navigation.set("redirectCount", 0)?;
    performance.set("navigation", perf_navigation)?;

    performance.set("now", Func::from(|| -> f64 { 0.0 }))?;
    performance.set("getEntriesByType", Func::from(|_type: String| -> Vec<f64> { vec![] }))?;
    performance.set("getEntriesByName", Func::from(|_name: String| -> Vec<f64> { vec![] }))?;
    performance.set("mark", Func::from(|_name: String| {}))?;
    performance.set("measure", Func::from(|_name: String| {}))?;
    window.set("performance", performance)?;

    // ── history ──────────────────────────────────────────────────────
    let history = Object::new(ctx.clone())?;
    history.set("length", 1)?;
    history.set("pushState", Func::from(|_: f64, _: String, _: String| {}))?;
    history.set("replaceState", Func::from(|_: f64, _: String, _: String| {}))?;
    history.set("back", Func::from(|| {}))?;
    history.set("forward", Func::from(|| {}))?;
    history.set("go", Func::from(|_: f64| {}))?;
    window.set("history", history)?;

    // ── miscellaneous stubs ──────────────────────────────────────────
    window.set("setTimeout", Func::from(|_cb: rquickjs::Value<'_>, _ms: f64| -> f64 { 0.0 }))?;
    window.set("clearTimeout", Func::from(|_id: f64| {}))?;
    window.set("setInterval", Func::from(|_cb: rquickjs::Value<'_>, _ms: f64| -> f64 { 0.0 }))?;
    window.set("clearInterval", Func::from(|_id: f64| {}))?;
    window.set("requestAnimationFrame", Func::from(|_cb: rquickjs::Value<'_>| -> f64 { 0.0 }))?;
    window.set("cancelAnimationFrame", Func::from(|_id: f64| {}))?;
    window.set("atob", Func::from(|_s: String| -> String { String::new() }))?;
    window.set("btoa", Func::from(|_s: String| -> String { String::new() }))?;

    // ── window.open / window.close / window.print stubs ─────────
    window.set("open", Func::from(|url: String| {
        tracing::warn!(url = %url, "window.open() called — navigation not supported from JS, ignoring");
    }))?;
    window.set("close", Func::from(|| {
        tracing::warn!("window.close() called — not supported, ignoring");
    }))?;
    window.set("print", Func::from(|| {
        tracing::warn!("window.print() called — not supported, ignoring");
    }))?;
    window.set("stop", Func::from(|| {
        tracing::debug!("window.stop() called — no-op");
    }))?;
    window.set("focus", Func::from(|| {}))?;
    window.set("blur", Func::from(|| {}))?;
    window.set("alert", Func::from(|msg: String| {
        tracing::warn!(msg = %msg, "window.alert() called — not supported");
    }))?;
    window.set("confirm", Func::from(|_msg: String| -> bool { false }))?;
    window.set("prompt", Func::from(|_msg: String| -> Option<String> { None }))?;

    Ok(window)
}

// ── Bridge functions ─────────────────────────────────────────────────────────

/// Register low-level bridge functions on the `__nova` global object.
///
/// These functions accept/return primitive types only (numbers for handles,
/// strings for text). The JavaScript shim (see [`JS_DOM_SHIM`]) wraps them
/// into a user-friendly DOM API.
pub fn register_bridge_functions<'js>(
    ctx: &Ctx<'js>,
    tree: &Arc<Mutex<JsDomTree>>,
) -> Result<()> {
    let nova = Object::new(ctx.clone())?;

    // getElementById(id) -> handle (f64) or -1
    {
        let t = tree.clone();
        nova.set("getElementById", Func::from(move |id: String| -> f64 {
            t.lock().unwrap().get_element_by_id(&id)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // querySelector(sel) -> handle or -1
    {
        let t = tree.clone();
        nova.set("querySelector", Func::from(move |sel: String| -> f64 {
            t.lock().unwrap().query_selector(&sel)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // querySelectorAll(sel) -> array of handles
    {
        let t = tree.clone();
        nova.set("querySelectorAll", Func::from(move |sel: String| -> Vec<f64> {
            t.lock().unwrap().query_selector_all(&sel)
                .into_iter()
                .map(|h| h as f64)
                .collect()
        }))?;
    }

    // querySelectorWithin(handle, sel) -> handle or -1
    {
        let t = tree.clone();
        nova.set("querySelectorWithin", Func::from(move |handle: f64, sel: String| -> f64 {
            t.lock().unwrap().query_selector_within(handle as ElementHandle, &sel)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // querySelectorAllWithin(handle, sel) -> array of handles
    {
        let t = tree.clone();
        nova.set("querySelectorAllWithin", Func::from(move |handle: f64, sel: String| -> Vec<f64> {
            t.lock().unwrap().query_selector_all_within(handle as ElementHandle, &sel)
                .into_iter()
                .map(|h| h as f64)
                .collect()
        }))?;
    }

    // createElement(tag) -> handle
    {
        let t = tree.clone();
        nova.set("createElement", Func::from(MutFn::new(move |tag: String| -> f64 {
            t.lock().unwrap().create_element(&tag) as f64
        })))?;
    }

    // createTextNode(text) -> handle
    {
        let t = tree.clone();
        nova.set("createTextNode", Func::from(MutFn::new(move |text: String| -> f64 {
            t.lock().unwrap().create_text_node(&text) as f64
        })))?;
    }

    // createComment(data) -> handle
    {
        let t = tree.clone();
        nova.set("createComment", Func::from(MutFn::new(move |data: String| -> f64 {
            t.lock().unwrap().create_comment(&data) as f64
        })))?;
    }

    // firstElementChild(handle) -> handle or -1
    {
        let t = tree.clone();
        nova.set("firstElementChild", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().first_element_child(handle as ElementHandle)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // lastElementChild(handle) -> handle or -1
    {
        let t = tree.clone();
        nova.set("lastElementChild", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().last_element_child(handle as ElementHandle)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // requestNavigation(url) — store a URL for the shell to navigate to
    {
        let t = tree.clone();
        nova.set("requestNavigation", Func::from(MutFn::new(move |url: String| {
            t.lock().unwrap().pending_navigation = Some(url);
        })))?;
    }

    // pushState(url) — SPA navigation: update URL without page reload
    {
        let t = tree.clone();
        nova.set("pushState", Func::from(MutFn::new(move |url: String| {
            let mut tree = t.lock().unwrap();
            tree.push_state_url = Some(url.clone());
            tree.current_url = url;
        })))?;
    }

    // replaceState(url) — SPA navigation: replace current history entry without reload
    {
        let t = tree.clone();
        nova.set("replaceState", Func::from(MutFn::new(move |url: String| {
            let mut tree = t.lock().unwrap();
            tree.replace_state_url = Some(url.clone());
            tree.current_url = url;
        })))?;
    }

    // getTagName(handle) -> string
    {
        let t = tree.clone();
        nova.set("getTagName", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().tag_of(handle as ElementHandle)
                .unwrap_or("")
                .to_uppercase()
        }))?;
    }

    // getTextContent(handle) -> string
    {
        let t = tree.clone();
        nova.set("getTextContent", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().get_text_content(handle as ElementHandle)
        }))?;
    }

    // setTextContent(handle, value)
    {
        let t = tree.clone();
        nova.set("setTextContent", Func::from(MutFn::new(move |handle: f64, value: String| {
            t.lock().unwrap().set_text_content(handle as ElementHandle, &value);
        })))?;
    }

    // getInnerHTML(handle) -> string
    {
        let t = tree.clone();
        nova.set("getInnerHTML", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().get_inner_html(handle as ElementHandle)
        }))?;
    }

    // setInnerHTML(handle, html)
    {
        let t = tree.clone();
        nova.set("setInnerHTML", Func::from(MutFn::new(move |handle: f64, html: String| {
            t.lock().unwrap().set_inner_html(handle as ElementHandle, &html);
        })))?;
    }

    // getAttribute(handle, name) -> string or null
    {
        let t = tree.clone();
        nova.set("getAttribute", Func::from(move |handle: f64, name: String| -> Option<String> {
            t.lock().unwrap().get_attribute(handle as ElementHandle, &name)
        }))?;
    }

    // setAttribute(handle, name, value)
    {
        let t = tree.clone();
        nova.set("setAttribute", Func::from(MutFn::new(move |handle: f64, name: String, value: String| {
            t.lock().unwrap().set_attribute(handle as ElementHandle, &name, &value);
        })))?;
    }

    // removeAttribute(handle, name)
    {
        let t = tree.clone();
        nova.set("removeAttribute", Func::from(MutFn::new(move |handle: f64, name: String| {
            t.lock().unwrap().remove_attribute(handle as ElementHandle, &name);
        })))?;
    }

    // hasAttribute(handle, name) -> bool
    {
        let t = tree.clone();
        nova.set("hasAttribute", Func::from(move |handle: f64, name: String| -> bool {
            t.lock().unwrap().has_attribute(handle as ElementHandle, &name)
        }))?;
    }

    // appendChild(parent, child) -> bool
    {
        let t = tree.clone();
        nova.set("appendChild", Func::from(MutFn::new(move |parent: f64, child: f64| -> bool {
            t.lock().unwrap().append_child(parent as ElementHandle, child as ElementHandle)
        })))?;
    }

    // removeChild(parent, child) -> bool
    {
        let t = tree.clone();
        nova.set("removeChild", Func::from(MutFn::new(move |parent: f64, child: f64| -> bool {
            t.lock().unwrap().remove_child(parent as ElementHandle, child as ElementHandle)
        })))?;
    }

    // insertBefore(parent, newChild, refChild) -> bool
    {
        let t = tree.clone();
        nova.set("insertBefore", Func::from(MutFn::new(move |parent: f64, new_child: f64, ref_child: f64| -> bool {
            let ref_h = if ref_child < 0.0 { None } else { Some(ref_child as ElementHandle) };
            t.lock().unwrap().insert_before(parent as ElementHandle, new_child as ElementHandle, ref_h)
        })))?;
    }

    // parentNode(handle) -> handle or -1
    {
        let t = tree.clone();
        nova.set("parentNode", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().parent_node(handle as ElementHandle)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // children(handle) -> [handles]
    {
        let t = tree.clone();
        nova.set("children", Func::from(move |handle: f64| -> Vec<f64> {
            t.lock().unwrap().children(handle as ElementHandle)
                .into_iter()
                .map(|h| h as f64)
                .collect()
        }))?;
    }

    // childNodes(handle) -> [handles]
    {
        let t = tree.clone();
        nova.set("childNodes", Func::from(move |handle: f64| -> Vec<f64> {
            t.lock().unwrap().child_nodes(handle as ElementHandle)
                .into_iter()
                .map(|h| h as f64)
                .collect()
        }))?;
    }

    // firstChild(handle) -> handle or -1
    {
        let t = tree.clone();
        nova.set("firstChild", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().first_child(handle as ElementHandle)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // lastChild(handle) -> handle or -1
    {
        let t = tree.clone();
        nova.set("lastChild", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().last_child(handle as ElementHandle)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // nextSibling(handle) -> handle or -1
    {
        let t = tree.clone();
        nova.set("nextSibling", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().next_sibling(handle as ElementHandle)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // previousSibling(handle) -> handle or -1
    {
        let t = tree.clone();
        nova.set("previousSibling", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().previous_sibling(handle as ElementHandle)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // cloneNode(handle, deep) -> handle
    {
        let t = tree.clone();
        nova.set("cloneNode", Func::from(MutFn::new(move |handle: f64, deep: bool| -> f64 {
            t.lock().unwrap().clone_node(handle as ElementHandle, deep) as f64
        })))?;
    }

    // contains(parent, child) -> bool
    {
        let t = tree.clone();
        nova.set("contains", Func::from(move |parent: f64, child: f64| -> bool {
            t.lock().unwrap().contains(parent as ElementHandle, child as ElementHandle)
        }))?;
    }

    // matches(handle, sel) -> bool
    {
        let t = tree.clone();
        nova.set("matches", Func::from(move |handle: f64, sel: String| -> bool {
            t.lock().unwrap().matches(handle as ElementHandle, &sel)
        }))?;
    }

    // closest(handle, sel) -> handle or -1
    {
        let t = tree.clone();
        nova.set("closest", Func::from(move |handle: f64, sel: String| -> f64 {
            t.lock().unwrap().closest(handle as ElementHandle, &sel)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // getElementsByClassName(name) -> [handles]
    {
        let t = tree.clone();
        nova.set("getElementsByClassName", Func::from(move |name: String| -> Vec<f64> {
            t.lock().unwrap().get_elements_by_class_name(&name)
                .into_iter()
                .map(|h| h as f64)
                .collect()
        }))?;
    }

    // getElementsByTagName(tag) -> [handles]
    {
        let t = tree.clone();
        nova.set("getElementsByTagName", Func::from(move |tag: String| -> Vec<f64> {
            t.lock().unwrap().get_elements_by_tag_name(&tag)
                .into_iter()
                .map(|h| h as f64)
                .collect()
        }))?;
    }

    // createDocumentFragment() -> handle
    {
        let t = tree.clone();
        nova.set("createDocumentFragment", Func::from(MutFn::new(move || -> f64 {
            t.lock().unwrap().create_document_fragment() as f64
        })))?;
    }

    // getDataset(handle, key) -> string or null
    {
        let t = tree.clone();
        nova.set("getDataset", Func::from(move |handle: f64, key: String| -> Option<String> {
            t.lock().unwrap().get_dataset(handle as ElementHandle, &key)
        }))?;
    }

    // setDataset(handle, key, value)
    {
        let t = tree.clone();
        nova.set("setDataset", Func::from(MutFn::new(move |handle: f64, key: String, value: String| {
            t.lock().unwrap().set_dataset(handle as ElementHandle, &key, &value);
        })))?;
    }

    // getDatasetAll(handle) -> JSON string of dataset entries
    {
        let t = tree.clone();
        nova.set("getDatasetKeys", Func::from(move |handle: f64| -> Vec<String> {
            let tree = t.lock().unwrap();
            let map = tree.get_dataset_all(handle as ElementHandle);
            map.keys().cloned().collect()
        }))?;
    }

    // getOuterHTML(handle) -> string
    {
        let t = tree.clone();
        nova.set("getOuterHTML", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().get_outer_html(handle as ElementHandle)
        }))?;
    }

    // setOuterHTML(handle, html)
    {
        let t = tree.clone();
        nova.set("setOuterHTML", Func::from(MutFn::new(move |handle: f64, html: String| {
            t.lock().unwrap().set_outer_html(handle as ElementHandle, &html);
        })))?;
    }

    // replaceChild(parent, newChild, oldChild) -> bool
    {
        let t = tree.clone();
        nova.set("replaceChild", Func::from(MutFn::new(move |parent: f64, new_child: f64, old_child: f64| -> bool {
            t.lock().unwrap().replace_child(
                parent as ElementHandle,
                new_child as ElementHandle,
                old_child as ElementHandle,
            )
        })))?;
    }

    // getValue(handle) -> string
    {
        let t = tree.clone();
        nova.set("getValue", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().get_value(handle as ElementHandle)
        }))?;
    }

    // setValue(handle, value)
    {
        let t = tree.clone();
        nova.set("setValue", Func::from(MutFn::new(move |handle: f64, value: String| {
            t.lock().unwrap().set_value(handle as ElementHandle, &value);
        })))?;
    }

    // getChecked(handle) -> bool
    {
        let t = tree.clone();
        nova.set("getChecked", Func::from(move |handle: f64| -> bool {
            t.lock().unwrap().get_checked(handle as ElementHandle)
        }))?;
    }

    // setChecked(handle, value)
    {
        let t = tree.clone();
        nova.set("setChecked", Func::from(MutFn::new(move |handle: f64, value: bool| {
            t.lock().unwrap().set_checked(handle as ElementHandle, value);
        })))?;
    }

    // getDisabled(handle) -> bool
    {
        let t = tree.clone();
        nova.set("getDisabled", Func::from(move |handle: f64| -> bool {
            t.lock().unwrap().get_disabled(handle as ElementHandle)
        }))?;
    }

    // setDisabled(handle, value)
    {
        let t = tree.clone();
        nova.set("setDisabled", Func::from(MutFn::new(move |handle: f64, value: bool| {
            t.lock().unwrap().set_disabled(handle as ElementHandle, value);
        })))?;
    }

    // getOffsetWidth(handle) -> f64
    {
        let t = tree.clone();
        nova.set("getOffsetWidth", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().get_offset_width(handle as ElementHandle)
        }))?;
    }

    // getOffsetHeight(handle) -> f64
    {
        let t = tree.clone();
        nova.set("getOffsetHeight", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().get_offset_height(handle as ElementHandle)
        }))?;
    }

    // getOffsetTop(handle) -> f64
    {
        let t = tree.clone();
        nova.set("getOffsetTop", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().get_offset_top(handle as ElementHandle)
        }))?;
    }

    // getOffsetLeft(handle) -> f64
    {
        let t = tree.clone();
        nova.set("getOffsetLeft", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().get_offset_left(handle as ElementHandle)
        }))?;
    }

    // getClientWidth(handle) -> f64
    {
        let t = tree.clone();
        nova.set("getClientWidth", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().get_client_width(handle as ElementHandle)
        }))?;
    }

    // getClientHeight(handle) -> f64
    {
        let t = tree.clone();
        nova.set("getClientHeight", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().get_client_height(handle as ElementHandle)
        }))?;
    }

    // getScrollWidth(handle) -> f64
    {
        let t = tree.clone();
        nova.set("getScrollWidth", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().get_scroll_width(handle as ElementHandle)
        }))?;
    }

    // getScrollHeight(handle) -> f64
    {
        let t = tree.clone();
        nova.set("getScrollHeight", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().get_scroll_height(handle as ElementHandle)
        }))?;
    }

    // getComputedStyleProp(handle, prop) -> string
    {
        let t = tree.clone();
        nova.set("getComputedStyleProp", Func::from(move |handle: f64, prop: String| -> String {
            t.lock().unwrap().get_computed_style(handle as ElementHandle)
                .get(&prop)
                .cloned()
                .unwrap_or_default()
        }))?;
    }

    // getNodeType(handle) -> u32
    {
        let t = tree.clone();
        nova.set("getNodeType", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().node_type(handle as ElementHandle) as f64
        }))?;
    }

    // getNodeName(handle) -> string
    {
        let t = tree.clone();
        nova.set("getNodeName", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().node_name(handle as ElementHandle)
        }))?;
    }

    // getHead() -> handle or -1
    {
        let t = tree.clone();
        nova.set("getHead", Func::from(move || -> f64 {
            t.lock().unwrap().head()
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // getDocumentElement() -> handle or -1
    {
        let t = tree.clone();
        nova.set("getDocumentElement", Func::from(move || -> f64 {
            t.lock().unwrap().document_element()
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // classListAdd(handle, cls)
    {
        let t = tree.clone();
        nova.set("classListAdd", Func::from(MutFn::new(move |handle: f64, cls: String| {
            t.lock().unwrap().class_list_add(handle as ElementHandle, &cls);
        })))?;
    }

    // classListRemove(handle, cls)
    {
        let t = tree.clone();
        nova.set("classListRemove", Func::from(MutFn::new(move |handle: f64, cls: String| {
            t.lock().unwrap().class_list_remove(handle as ElementHandle, &cls);
        })))?;
    }

    // classListContains(handle, cls) -> bool
    {
        let t = tree.clone();
        nova.set("classListContains", Func::from(move |handle: f64, cls: String| -> bool {
            t.lock().unwrap().class_list_contains(handle as ElementHandle, &cls)
        }))?;
    }

    // classListToggle(handle, cls) -> bool
    {
        let t = tree.clone();
        nova.set("classListToggle", Func::from(MutFn::new(move |handle: f64, cls: String| -> bool {
            t.lock().unwrap().class_list_toggle(handle as ElementHandle, &cls)
        })))?;
    }

    // classListReplace(handle, oldCls, newCls) -> bool
    {
        let t = tree.clone();
        nova.set("classListReplace", Func::from(MutFn::new(move |handle: f64, old_cls: String, new_cls: String| -> bool {
            t.lock().unwrap().class_list_replace(handle as ElementHandle, &old_cls, &new_cls)
        })))?;
    }

    // getDocumentElement() -> handle or -1
    {
        let t = tree.clone();
        nova.set("getDocumentElement", Func::from(move || -> f64 {
            t.lock().unwrap().document_element()
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // getNodeType(handle) -> number
    {
        let t = tree.clone();
        nova.set("getNodeType", Func::from(move |handle: f64| -> f64 {
            t.lock().unwrap().node_type(handle as ElementHandle) as f64
        }))?;
    }

    // getStyleCssText(handle) -> string
    {
        let t = tree.clone();
        nova.set("getStyleCssText", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().style_css_text(handle as ElementHandle)
        }))?;
    }

    // getOuterHTML(handle) -> string
    {
        let t = tree.clone();
        nova.set("getOuterHTML", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().get_outer_html(handle as ElementHandle)
        }))?;
    }

    // insertAdjacentHTML(handle, position, html)
    {
        let t = tree.clone();
        nova.set("insertAdjacentHTML", Func::from(MutFn::new(move |handle: f64, position: String, html: String| {
            t.lock().unwrap().insert_adjacent_html(handle as ElementHandle, &position, &html);
        })))?;
    }

    // styleSetProperty(handle, name, value)
    {
        let t = tree.clone();
        nova.set("styleSetProperty", Func::from(MutFn::new(move |handle: f64, name: String, value: String| {
            t.lock().unwrap().style_set_property(handle as ElementHandle, &name, &value);
        })))?;
    }

    // styleGetProperty(handle, name) -> string
    {
        let t = tree.clone();
        nova.set("styleGetProperty", Func::from(move |handle: f64, name: String| -> String {
            t.lock().unwrap().style_get_property(handle as ElementHandle, &name).unwrap_or_default()
        }))?;
    }

    // getClassName(handle) -> string
    {
        let t = tree.clone();
        nova.set("getClassName", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().class_name(handle as ElementHandle)
        }))?;
    }

    // getId(handle) -> string
    {
        let t = tree.clone();
        nova.set("getId", Func::from(move |handle: f64| -> String {
            t.lock().unwrap().get_id(handle as ElementHandle)
        }))?;
    }

    // getBody() -> handle or -1
    {
        let t = tree.clone();
        nova.set("getBody", Func::from(move || -> f64 {
            t.lock().unwrap().body()
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        }))?;
    }

    // getTitle() -> string
    {
        let t = tree.clone();
        nova.set("getTitle", Func::from(move || -> String {
            t.lock().unwrap().get_title()
        }))?;
    }

    // setTitle(title)
    {
        let t = tree.clone();
        nova.set("setTitle", Func::from(MutFn::new(move |title: String| {
            t.lock().unwrap().set_title(&title);
        })))?;
    }

    // addEventListener(handle, type, callbackId) — we store a sentinel
    {
        let t = tree.clone();
        nova.set("addEventListener", Func::from(MutFn::new(move |handle: f64, event_type: String| {
            t.lock().unwrap().add_event_listener(
                handle as ElementHandle,
                &event_type,
                "__quickjs_callback__",
                vec![],
                false,
            );
        })))?;
    }

    // dispatchEvent(handle, type) -> callbacks count
    {
        let t = tree.clone();
        nova.set("dispatchEvent", Func::from(MutFn::new(move |handle: f64, event_type: String| -> f64 {
            t.lock().unwrap().dispatch_event(handle as ElementHandle, &event_type).len() as f64
        })))?;
    }

    // ── Shadow DOM bridge functions ────────────────────────────────────

    // attachShadow(handle, mode) -> handle or -1
    {
        let t = tree.clone();
        nova.set("attachShadow", Func::from(MutFn::new(move |handle: f64, mode: String| -> f64 {
            use crate::shadow_dom::ShadowRootMode;
            let Some(m) = ShadowRootMode::from_str(&mode) else {
                return -1.0;
            };
            t.lock().unwrap().attach_shadow(handle as ElementHandle, m)
                .map(|h| h as f64)
                .unwrap_or(-1.0)
        })))?;
    }

    // getShadowRoot(handle) -> handle or -1 (only for open mode)
    {
        let t = tree.clone();
        nova.set("getShadowRoot", Func::from(move |handle: f64| -> f64 {
            let tree = t.lock().unwrap();
            if tree.get_shadow_root(handle as ElementHandle).is_some() {
                handle
            } else {
                -1.0
            }
        }))?;
    }

    // shadowAppendChild(hostHandle, childHandle) -> bool
    {
        let t = tree.clone();
        nova.set("shadowAppendChild", Func::from(MutFn::new(move |host: f64, child: f64| -> bool {
            t.lock().unwrap().shadow_append_child(host as ElementHandle, child as ElementHandle)
        })))?;
    }

    // shadowSetInnerHTML(hostHandle, html) — parse HTML into shadow root
    {
        let t = tree.clone();
        nova.set("shadowSetInnerHTML", Func::from(MutFn::new(move |host: f64, html: String| {
            let mut tree = t.lock().unwrap();
            let host_h = host as ElementHandle;
            // Parse the HTML fragment and add children to the shadow root.
            let new_children = tree.parse_html_fragment(&html);
            // Extract <style> content from shadow children for scoped styles.
            let mut shadow_styles = Vec::new();
            for &child_h in &new_children {
                if let Some(elem) = tree.nodes.get(&child_h) {
                    if elem.tag == "style" {
                        // Collect text content.
                        let text: String = elem.children.iter()
                            .filter_map(|&h| tree.nodes.get(&h))
                            .filter_map(|e| e.text.clone())
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.trim().is_empty() {
                            shadow_styles.push(text);
                        }
                    }
                }
            }
            if let Some(shadow) = tree.shadow_roots.get_mut(&host_h) {
                shadow.children = new_children;
                for css in shadow_styles {
                    shadow.add_stylesheet(css);
                }
            }
        })))?;
    }

    // shadowAddStylesheet(hostHandle, css) — add a scoped stylesheet
    {
        let t = tree.clone();
        nova.set("shadowAddStylesheet", Func::from(MutFn::new(move |host: f64, css: String| {
            let mut tree = t.lock().unwrap();
            if let Some(shadow) = tree.shadow_roots.get_mut(&(host as ElementHandle)) {
                shadow.add_stylesheet(css);
            }
        })))?;
    }

    // ── Custom Elements bridge functions ────────────────────────────────

    // customElementsDefine(name, constructorSource, extendsTag) -> bool
    {
        let t = tree.clone();
        nova.set("customElementsDefine", Func::from(MutFn::new(move |name: String, constructor_source: String, extends: String| -> bool {
            let ext = if extends.is_empty() { None } else { Some(extends) };
            t.lock().unwrap().custom_elements.define(&name, constructor_source, ext)
        })))?;
    }

    // customElementsGet(name) -> constructorSource or ""
    {
        let t = tree.clone();
        nova.set("customElementsGet", Func::from(move |name: String| -> String {
            t.lock().unwrap().custom_elements.get(&name)
                .map(|def| def.constructor_source.clone())
                .unwrap_or_default()
        }))?;
    }

    // customElementsIsDefined(name) -> bool
    {
        let t = tree.clone();
        nova.set("customElementsIsDefined", Func::from(move |name: String| -> bool {
            t.lock().unwrap().custom_elements.is_defined(&name)
        }))?;
    }

    // ── Canvas 2D bridge functions ──────────────────────────────────────────

    // canvasGetContext(handle) -> bool (true if canvas, false otherwise)
    {
        let t = tree.clone();
        nova.set("canvasGetContext", Func::from(MutFn::new(move |handle: f64| -> bool {
            t.lock().unwrap().get_canvas_context(handle as ElementHandle).is_some()
        })))?;
    }

    // canvasFillRect(handle, x, y, w, h)
    {
        let t = tree.clone();
        nova.set("canvasFillRect", Func::from(MutFn::new(move |handle: f64, x: f64, y: f64, w: f64, h: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.fill_rect(x, y, w, h);
            }
        })))?;
    }

    // canvasStrokeRect(handle, x, y, w, h)
    {
        let t = tree.clone();
        nova.set("canvasStrokeRect", Func::from(MutFn::new(move |handle: f64, x: f64, y: f64, w: f64, h: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.stroke_rect(x, y, w, h);
            }
        })))?;
    }

    // canvasClearRect(handle, x, y, w, h)
    {
        let t = tree.clone();
        nova.set("canvasClearRect", Func::from(MutFn::new(move |handle: f64, x: f64, y: f64, w: f64, h: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.clear_rect(x, y, w, h);
            }
        })))?;
    }

    // canvasFillText(handle, text, x, y)
    {
        let t = tree.clone();
        nova.set("canvasFillText", Func::from(MutFn::new(move |handle: f64, text: String, x: f64, y: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.fill_text(&text, x, y);
            }
        })))?;
    }

    // canvasStrokeText(handle, text, x, y)
    {
        let t = tree.clone();
        nova.set("canvasStrokeText", Func::from(MutFn::new(move |handle: f64, text: String, x: f64, y: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.stroke_text(&text, x, y);
            }
        })))?;
    }

    // canvasMeasureText(handle, text) -> width
    {
        let t = tree.clone();
        nova.set("canvasMeasureText", Func::from(move |handle: f64, text: String| -> f64 {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| ctx.measure_text(&text))
                .unwrap_or(0.0)
        }))?;
    }

    // canvasBeginPath(handle)
    {
        let t = tree.clone();
        nova.set("canvasBeginPath", Func::from(MutFn::new(move |handle: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.begin_path();
            }
        })))?;
    }

    // canvasMoveTo(handle, x, y)
    {
        let t = tree.clone();
        nova.set("canvasMoveTo", Func::from(MutFn::new(move |handle: f64, x: f64, y: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.move_to(x, y);
            }
        })))?;
    }

    // canvasLineTo(handle, x, y)
    {
        let t = tree.clone();
        nova.set("canvasLineTo", Func::from(MutFn::new(move |handle: f64, x: f64, y: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.line_to(x, y);
            }
        })))?;
    }

    // canvasClosePath(handle)
    {
        let t = tree.clone();
        nova.set("canvasClosePath", Func::from(MutFn::new(move |handle: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.close_path();
            }
        })))?;
    }

    // canvasArc(handle, cx, cy, radius, startAngle, endAngle)
    {
        let t = tree.clone();
        nova.set("canvasArc", Func::from(MutFn::new(move |handle: f64, cx: f64, cy: f64, r: f64, start: f64, end: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.arc(cx, cy, r, start, end);
            }
        })))?;
    }

    // canvasRect(handle, x, y, w, h)
    {
        let t = tree.clone();
        nova.set("canvasRect", Func::from(MutFn::new(move |handle: f64, x: f64, y: f64, w: f64, h: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.rect(x, y, w, h);
            }
        })))?;
    }

    // canvasFill(handle)
    {
        let t = tree.clone();
        nova.set("canvasFill", Func::from(MutFn::new(move |handle: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.fill();
            }
        })))?;
    }

    // canvasStroke(handle)
    {
        let t = tree.clone();
        nova.set("canvasStroke", Func::from(MutFn::new(move |handle: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.stroke();
            }
        })))?;
    }

    // canvasSetFillStyle(handle, color)
    {
        let t = tree.clone();
        nova.set("canvasSetFillStyle", Func::from(MutFn::new(move |handle: f64, color: String| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.set_fill_style(&color);
            }
        })))?;
    }

    // canvasGetFillStyle(handle) -> string
    {
        let t = tree.clone();
        nova.set("canvasGetFillStyle", Func::from(move |handle: f64| -> String {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| ctx.fill_style())
                .unwrap_or_default()
        }))?;
    }

    // canvasSetStrokeStyle(handle, color)
    {
        let t = tree.clone();
        nova.set("canvasSetStrokeStyle", Func::from(MutFn::new(move |handle: f64, color: String| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.set_stroke_style(&color);
            }
        })))?;
    }

    // canvasGetStrokeStyle(handle) -> string
    {
        let t = tree.clone();
        nova.set("canvasGetStrokeStyle", Func::from(move |handle: f64| -> String {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| ctx.stroke_style())
                .unwrap_or_default()
        }))?;
    }

    // canvasSetLineWidth(handle, width)
    {
        let t = tree.clone();
        nova.set("canvasSetLineWidth", Func::from(MutFn::new(move |handle: f64, w: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.set_line_width(w);
            }
        })))?;
    }

    // canvasGetLineWidth(handle) -> number
    {
        let t = tree.clone();
        nova.set("canvasGetLineWidth", Func::from(move |handle: f64| -> f64 {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| ctx.line_width())
                .unwrap_or(1.0)
        }))?;
    }

    // canvasSetFont(handle, font)
    {
        let t = tree.clone();
        nova.set("canvasSetFont", Func::from(MutFn::new(move |handle: f64, font: String| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.set_font(&font);
            }
        })))?;
    }

    // canvasGetFont(handle) -> string
    {
        let t = tree.clone();
        nova.set("canvasGetFont", Func::from(move |handle: f64| -> String {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| ctx.font())
                .unwrap_or_default()
        }))?;
    }

    // canvasSetTextAlign(handle, align)
    {
        let t = tree.clone();
        nova.set("canvasSetTextAlign", Func::from(MutFn::new(move |handle: f64, align: String| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.set_text_align(&align);
            }
        })))?;
    }

    // canvasSetTextBaseline(handle, baseline)
    {
        let t = tree.clone();
        nova.set("canvasSetTextBaseline", Func::from(MutFn::new(move |handle: f64, baseline: String| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.set_text_baseline(&baseline);
            }
        })))?;
    }

    // canvasSetGlobalAlpha(handle, alpha)
    {
        let t = tree.clone();
        nova.set("canvasSetGlobalAlpha", Func::from(MutFn::new(move |handle: f64, alpha: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.set_global_alpha(alpha);
            }
        })))?;
    }

    // canvasGetGlobalAlpha(handle) -> number
    {
        let t = tree.clone();
        nova.set("canvasGetGlobalAlpha", Func::from(move |handle: f64| -> f64 {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| ctx.global_alpha())
                .unwrap_or(1.0)
        }))?;
    }

    // canvasSave(handle)
    {
        let t = tree.clone();
        nova.set("canvasSave", Func::from(MutFn::new(move |handle: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.save();
            }
        })))?;
    }

    // canvasRestore(handle)
    {
        let t = tree.clone();
        nova.set("canvasRestore", Func::from(MutFn::new(move |handle: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.restore();
            }
        })))?;
    }

    // canvasTranslate(handle, x, y)
    {
        let t = tree.clone();
        nova.set("canvasTranslate", Func::from(MutFn::new(move |handle: f64, x: f64, y: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.translate(x, y);
            }
        })))?;
    }

    // canvasRotate(handle, angle)
    {
        let t = tree.clone();
        nova.set("canvasRotate", Func::from(MutFn::new(move |handle: f64, angle: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.rotate(angle);
            }
        })))?;
    }

    // canvasScale(handle, sx, sy)
    {
        let t = tree.clone();
        nova.set("canvasScale", Func::from(MutFn::new(move |handle: f64, sx: f64, sy: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.scale(sx, sy);
            }
        })))?;
    }

    // canvasSetTransform(handle, a, b, c, d, e, f)
    {
        let t = tree.clone();
        nova.set("canvasSetTransform", Func::from(MutFn::new(move |handle: f64, a: f64, b: f64, c: f64, d: f64, e: f64, f_val: f64| {
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.set_transform(a, b, c, d, e, f_val);
            }
        })))?;
    }

    // canvasGetImageData(handle, x, y, w, h) -> [u8] as Vec<f64>
    {
        let t = tree.clone();
        nova.set("canvasGetImageData", Func::from(move |handle: f64, x: f64, y: f64, w: f64, h: f64| -> Vec<f64> {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| {
                    ctx.get_image_data(x as u32, y as u32, w as u32, h as u32)
                        .into_iter()
                        .map(|b| b as f64)
                        .collect()
                })
                .unwrap_or_default()
        }))?;
    }

    // canvasPutImageData(handle, data_as_vec_f64, dx, dy, sw, sh)
    {
        let t = tree.clone();
        nova.set("canvasPutImageData", Func::from(MutFn::new(move |handle: f64, data: Vec<f64>, dx: f64, dy: f64, sw: f64, sh: f64| {
            let bytes: Vec<u8> = data.into_iter().map(|v| v as u8).collect();
            if let Some(ctx) = t.lock().unwrap().get_canvas_context(handle as ElementHandle) {
                ctx.put_image_data(&bytes, dx as u32, dy as u32, sw as u32, sh as u32);
            }
        })))?;
    }

    // canvasGetWidth(handle) -> number
    {
        let t = tree.clone();
        nova.set("canvasGetWidth", Func::from(move |handle: f64| -> f64 {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| ctx.width as f64)
                .unwrap_or(0.0)
        }))?;
    }

    // canvasGetHeight(handle) -> number
    {
        let t = tree.clone();
        nova.set("canvasGetHeight", Func::from(move |handle: f64| -> f64 {
            let mut tree = t.lock().unwrap();
            tree.get_canvas_context(handle as ElementHandle)
                .map(|ctx| ctx.height as f64)
                .unwrap_or(0.0)
        }))?;
    }

    ctx.globals().set("__nova", nova)?;

    Ok(())
}

/// JavaScript shim that creates the DOM API on top of `__nova` bridge functions.
///
/// This defines `Element`, `document`, `window.location`, `window.history`,
/// `navigator`, `getComputedStyle`, form properties, dimension properties,
/// and all Sprint 2 additions.
pub const JS_DOM_SHIM: &str = r#"
(function() {
    "use strict";

    // Element wrapper
    function Element(handle) {
        this.__handle = handle;
    }

    Object.defineProperty(Element.prototype, 'tagName', {
        get: function() { return __nova.getTagName(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'nodeName', {
        get: function() { return __nova.getNodeName(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'nodeType', {
        get: function() { return __nova.getNodeType(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'id', {
        get: function() { return __nova.getId(this.__handle); },
        set: function(v) { __nova.setAttribute(this.__handle, "id", v); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'className', {
        get: function() { return __nova.getClassName(this.__handle); },
        set: function(v) { __nova.setAttribute(this.__handle, "class", v); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'textContent', {
        get: function() { return __nova.getTextContent(this.__handle); },
        set: function(v) { __nova.setTextContent(this.__handle, v); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'innerHTML', {
        get: function() { return __nova.getInnerHTML(this.__handle); },
        set: function(v) { __nova.setInnerHTML(this.__handle, v); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'outerHTML', {
        get: function() { return __nova.getOuterHTML(this.__handle); },
        set: function(v) { __nova.setOuterHTML(this.__handle, v); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'parentNode', {
        get: function() {
            var h = __nova.parentNode(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'parentElement', {
        get: function() {
            var h = __nova.parentNode(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'children', {
        get: function() {
            return __nova.children(this.__handle).map(function(h) { return new Element(h); });
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'childNodes', {
        get: function() {
            return __nova.childNodes(this.__handle).map(function(h) { return new Element(h); });
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'firstChild', {
        get: function() {
            var h = __nova.firstChild(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'lastChild', {
        get: function() {
            var h = __nova.lastChild(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'firstElementChild', {
        get: function() {
            var h = __nova.firstElementChild(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'lastElementChild', {
        get: function() {
            var h = __nova.lastElementChild(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'childElementCount', {
        get: function() {
            return __nova.children(this.__handle).length;
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'nextSibling', {
        get: function() {
            var h = __nova.nextSibling(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'nextElementSibling', {
        get: function() {
            var h = __nova.nextSibling(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'previousSibling', {
        get: function() {
            var h = __nova.previousSibling(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'previousElementSibling', {
        get: function() {
            var h = __nova.previousSibling(this.__handle);
            return h < 0 ? null : new Element(h);
        },
        configurable: true
    });

    // Form element properties
    Object.defineProperty(Element.prototype, 'value', {
        get: function() { return __nova.getValue(this.__handle); },
        set: function(v) { __nova.setValue(this.__handle, String(v)); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'checked', {
        get: function() { return __nova.getChecked(this.__handle); },
        set: function(v) { __nova.setChecked(this.__handle, !!v); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'disabled', {
        get: function() { return __nova.getDisabled(this.__handle); },
        set: function(v) { __nova.setDisabled(this.__handle, !!v); },
        configurable: true
    });

    // Dimension properties
    Object.defineProperty(Element.prototype, 'offsetWidth', {
        get: function() { return __nova.getOffsetWidth(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'offsetHeight', {
        get: function() { return __nova.getOffsetHeight(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'offsetTop', {
        get: function() { return __nova.getOffsetTop(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'offsetLeft', {
        get: function() { return __nova.getOffsetLeft(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'clientWidth', {
        get: function() { return __nova.getClientWidth(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'clientHeight', {
        get: function() { return __nova.getClientHeight(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'scrollWidth', {
        get: function() { return __nova.getScrollWidth(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'scrollHeight', {
        get: function() { return __nova.getScrollHeight(this.__handle); },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'scrollTop', {
        get: function() { return 0; },
        set: function(v) { /* stub */ },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'scrollLeft', {
        get: function() { return 0; },
        set: function(v) { /* stub */ },
        configurable: true
    });

    // classList
    Object.defineProperty(Element.prototype, 'classList', {
        get: function() {
            var self = this;
            return {
                add: function(cls) { __nova.classListAdd(self.__handle, cls); },
                remove: function(cls) { __nova.classListRemove(self.__handle, cls); },
                contains: function(cls) { return __nova.classListContains(self.__handle, cls); },
                toggle: function(cls) { return __nova.classListToggle(self.__handle, cls); },
                replace: function(oldCls, newCls) { return __nova.classListReplace(self.__handle, oldCls, newCls); }
            };
        },
        configurable: true
    });

    // dataset proxy for data-* attributes
    Object.defineProperty(Element.prototype, 'dataset', {
        get: function() {
            var self = this;
            return new Proxy({}, {
                get: function(target, prop) {
                    return __nova.getDataset(self.__handle, prop);
                },
                set: function(target, prop, value) {
                    __nova.setDataset(self.__handle, prop, String(value));
                    return true;
                }
            });
        },
        configurable: true
    });

    // style
    Object.defineProperty(Element.prototype, 'style', {
        get: function() {
            var self = this;
            return new Proxy({}, {
                set: function(target, prop, value) {
                    if (prop === 'cssText') {
                        // Parse "key: value; key2: value2;" and set each property
                        var parts = String(value).split(';');
                        for (var i = 0; i < parts.length; i++) {
                            var kv = parts[i].split(':');
                            if (kv.length >= 2) {
                                var k = kv[0].trim();
                                var v = kv.slice(1).join(':').trim();
                                if (k) __nova.styleSetProperty(self.__handle, k, v);
                            }
                        }
                        return true;
                    }
                    __nova.styleSetProperty(self.__handle, prop, String(value));
                    return true;
                },
                get: function(target, prop) {
                    if (prop === 'setProperty') {
                        return function(name, value) { __nova.styleSetProperty(self.__handle, name, String(value)); };
                    }
                    if (prop === 'getPropertyValue') {
                        return function(name) { return __nova.styleGetProperty(self.__handle, name); };
                    }
                    if (prop === 'removeProperty') {
                        return function(name) {
                            var old = __nova.styleGetProperty(self.__handle, name);
                            __nova.styleSetProperty(self.__handle, name, '');
                            return old;
                        };
                    }
                    if (prop === 'cssText') {
                        return __nova.getStyleCssText(self.__handle);
                    }
                    return __nova.styleGetProperty(self.__handle, prop);
                }
            });
        },
        configurable: true
    });

    Element.prototype.getAttribute = function(name) {
        return __nova.getAttribute(this.__handle, name);
    };

    Element.prototype.setAttribute = function(name, value) {
        __nova.setAttribute(this.__handle, name, String(value));
    };

    Element.prototype.removeAttribute = function(name) {
        __nova.removeAttribute(this.__handle, name);
    };

    Element.prototype.hasAttribute = function(name) {
        return __nova.hasAttribute(this.__handle, name);
    };

    Element.prototype.appendChild = function(child) {
        __nova.appendChild(this.__handle, child.__handle);
        return child;
    };

    Element.prototype.removeChild = function(child) {
        __nova.removeChild(this.__handle, child.__handle);
        return child;
    };

    Element.prototype.replaceChild = function(newChild, oldChild) {
        __nova.replaceChild(this.__handle, newChild.__handle, oldChild.__handle);
        return oldChild;
    };

    Element.prototype.insertBefore = function(newChild, refChild) {
        var refH = refChild ? refChild.__handle : -1;
        __nova.insertBefore(this.__handle, newChild.__handle, refH);
        return newChild;
    };

    Element.prototype.addEventListener = function(type, callback, optionsOrCapture) {
        if (typeof callback !== 'function') return;
        var once = false;
        if (optionsOrCapture && typeof optionsOrCapture === 'object') {
            once = !!optionsOrCapture.once;
            // passive is accepted but has no effect in our engine (no default to prevent)
        }
        var actualCallback = callback;
        var self = this;
        if (once) {
            actualCallback = function __onceWrapper(evt) {
                self.removeEventListener(type, actualCallback);
                callback.call(this, evt);
            };
            actualCallback.__origCallback = callback;
        }
        if (!this.__listeners) this.__listeners = {};
        if (!this.__listeners[type]) this.__listeners[type] = [];
        this.__listeners[type].push(actualCallback);
        var h = this.__handle;
        if (!__eventListeners[h]) __eventListeners[h] = {};
        if (!__eventListeners[h][type]) __eventListeners[h][type] = [];
        __eventListeners[h][type].push(actualCallback);
        __nova.addEventListener(h, type);
    };

    Element.prototype.removeEventListener = function(type, callback) {
        if (!this.__listeners || !this.__listeners[type]) return;
        this.__listeners[type] = this.__listeners[type].filter(function(cb) {
            return cb !== callback && cb.__origCallback !== callback;
        });
        var h = this.__handle;
        if (__eventListeners[h] && __eventListeners[h][type]) {
            __eventListeners[h][type] = __eventListeners[h][type].filter(function(cb) {
                return cb !== callback && cb.__origCallback !== callback;
            });
        }
    };

    Element.prototype.querySelector = function(sel) {
        var h = __nova.querySelectorWithin(this.__handle, sel);
        return h < 0 ? null : new Element(h);
    };

    Element.prototype.querySelectorAll = function(sel) {
        var arr = __nova.querySelectorAllWithin(this.__handle, sel).map(function(h) {
            return new Element(h);
        });
        arr.item = function(i) { return i >= 0 && i < arr.length ? arr[i] : null; };
        return arr;
    };

    Element.prototype.getElementsByClassName = function(name) {
        // Scope to descendants of this element
        return __nova.querySelectorAllWithin(this.__handle, "." + name).map(function(h) {
            return new Element(h);
        });
    };

    Element.prototype.getElementsByTagName = function(tag) {
        return __nova.querySelectorAllWithin(this.__handle, tag).map(function(h) {
            return new Element(h);
        });
    };

    Element.prototype.cloneNode = function(deep) {
        var h = __nova.cloneNode(this.__handle, !!deep);
        return new Element(h);
    };

    Element.prototype.contains = function(other) {
        if (!other || other.__handle === undefined) return false;
        return __nova.contains(this.__handle, other.__handle);
    };

    Element.prototype.closest = function(sel) {
        var h = __nova.closest(this.__handle, sel);
        return h < 0 ? null : new Element(h);
    };

    Element.prototype.getBoundingClientRect = function() {
        var w = __nova.getOffsetWidth(this.__handle);
        var h = __nova.getOffsetHeight(this.__handle);
        var t = __nova.getOffsetTop(this.__handle);
        var l = __nova.getOffsetLeft(this.__handle);
        return { x: l, y: t, width: w, height: h, top: t, left: l, bottom: t + h, right: l + w };
    };

    Element.prototype.matches = function(sel) {
        return __nova.matches(this.__handle, sel);
    };

    // ── Shadow DOM API ─────────────────────────────────────────────────

    Element.prototype.attachShadow = function(init) {
        var mode = (init && init.mode) || 'open';
        var h = __nova.attachShadow(this.__handle, mode);
        if (h < 0) return null;
        // Create a ShadowRoot wrapper.
        var sr = new ShadowRoot(this.__handle, mode);
        this.__shadowRoot = (mode === 'open') ? sr : null;
        return sr;
    };

    Object.defineProperty(Element.prototype, 'shadowRoot', {
        get: function() {
            if (this.__shadowRoot) return this.__shadowRoot;
            var h = __nova.getShadowRoot(this.__handle);
            return h < 0 ? null : new ShadowRoot(this.__handle, 'open');
        },
        configurable: true
    });

    // ShadowRoot wrapper
    function ShadowRoot(hostHandle, mode) {
        this.__hostHandle = hostHandle;
        this.mode = mode;
        this.host = new Element(hostHandle);
    }

    ShadowRoot.prototype.appendChild = function(child) {
        __nova.shadowAppendChild(this.__hostHandle, child.__handle);
        return child;
    };

    Object.defineProperty(ShadowRoot.prototype, 'innerHTML', {
        set: function(html) {
            __nova.shadowSetInnerHTML(this.__hostHandle, html);
        },
        configurable: true
    });

    ShadowRoot.prototype.querySelector = function(sel) {
        // For now, query within the host's shadow children.
        return new Element(this.__hostHandle).querySelector(sel);
    };

    ShadowRoot.prototype.querySelectorAll = function(sel) {
        return new Element(this.__hostHandle).querySelectorAll(sel);
    };

    // ── Custom Elements API ────────────────────────────────────────────

    // Registry of constructors (JS-side, keyed by tag name).
    var __ceConstructors = {};
    // Registry of observed attributes per custom element.
    var __ceObservedAttrs = {};

    var customElements = {
        define: function(name, constructor, options) {
            var extendsTag = (options && options.extends) || '';
            // Store the constructor JS-side.
            __ceConstructors[name] = constructor;
            // Extract observedAttributes from the class.
            if (constructor.observedAttributes) {
                __ceObservedAttrs[name] = constructor.observedAttributes;
            }
            // Register in the Rust registry.
            __nova.customElementsDefine(name, constructor.toString(), extendsTag);
        },
        get: function(name) {
            return __ceConstructors[name] || undefined;
        },
        isDefined: function(name) {
            return __nova.customElementsIsDefined(name);
        },
        whenDefined: function(name) {
            // Simplified: resolve immediately if already defined.
            if (__ceConstructors[name]) {
                return Promise.resolve(__ceConstructors[name]);
            }
            return new Promise(function(resolve) {
                var check = function() {
                    if (__ceConstructors[name]) {
                        resolve(__ceConstructors[name]);
                    }
                };
                check();
            });
        }
    };

    globalThis.customElements = customElements;

    // Hook: when a custom element is instantiated, call its constructor
    // and connectedCallback.
    globalThis.__novaUpgradeElement = function(handle, tagName) {
        var Ctor = __ceConstructors[tagName];
        if (!Ctor) return;
        var el = new Element(handle);
        try {
            var instance = new Ctor();
            if (instance.connectedCallback) {
                instance.connectedCallback.call(el);
            }
            el.__ceInstance = instance;
        } catch(e) {
            // Silently ignore constructor errors for now.
        }
    };

    // Hook: attribute change notification.
    globalThis.__novaAttributeChanged = function(handle, tagName, name, oldValue, newValue) {
        var el = new Element(handle);
        if (el.__ceInstance && el.__ceInstance.attributeChangedCallback) {
            var observed = __ceObservedAttrs[tagName];
            if (observed && observed.indexOf(name) >= 0) {
                el.__ceInstance.attributeChangedCallback.call(el, name, oldValue, newValue);
            }
        }
    };

    // HTMLElement base class for custom elements.
    function HTMLElement() {
        // Custom elements extend this.
    }
    HTMLElement.prototype = Object.create(Element.prototype);
    HTMLElement.prototype.constructor = HTMLElement;
    globalThis.HTMLElement = HTMLElement;

    // ── HTMLMediaElement stubs (video/audio) ────────────────────────────

    // play() / pause() — stubs that log a warning
    Element.prototype.play = function() {
        console.warn("[NOVA] media playback not supported: play()");
        return Promise.resolve();
    };
    Element.prototype.pause = function() {
        console.warn("[NOVA] media playback not supported: pause()");
    };

    // Media properties as getters/setters on Element prototype.
    // These apply to all elements but only matter for video/audio.
    Object.defineProperty(Element.prototype, 'src', {
        get: function() {
            return __nova.getAttribute(this.__handle, "src") || "";
        },
        set: function(v) {
            __nova.setAttribute(this.__handle, "src", String(v));
        },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'currentTime', {
        get: function() { return 0; },
        set: function(v) { /* stub */ },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'duration', {
        get: function() { return NaN; },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'paused', {
        get: function() { return true; },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'ended', {
        get: function() { return false; },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'volume', {
        get: function() { return 1; },
        set: function(v) { /* stub */ },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'muted', {
        get: function() { return false; },
        set: function(v) { /* stub */ },
        configurable: true
    });

    Object.defineProperty(Element.prototype, 'poster', {
        get: function() {
            return __nova.getAttribute(this.__handle, "poster") || "";
        },
        set: function(v) {
            __nova.setAttribute(this.__handle, "poster", String(v));
        },
        configurable: true
    });

    Element.prototype.focus = function() { /* stub */ };
    Element.prototype.blur = function() { /* stub */ };
    Element.prototype.click = function() {
        globalThis.__novaDispatchEvent(this.__handle, "click");
    };

    Element.prototype.remove = function() {
        var ph = __nova.parentNode(this.__handle);
        if (ph >= 0) __nova.removeChild(ph, this.__handle);
    };

    Element.prototype.replaceWith = function(newEl) {
        var ph = __nova.parentNode(this.__handle);
        if (ph >= 0) __nova.replaceChild(ph, newEl.__handle, this.__handle);
    };

    Element.prototype.after = function() {
        // stub — simplified
    };

    Element.prototype.before = function() {
        // stub — simplified
    };

    Element.prototype.insertAdjacentHTML = function(position, html) {
        __nova.insertAdjacentHTML(this.__handle, position, html);
    };

    Element.prototype.insertAdjacentElement = function(position, element) {
        // Use insertAdjacentHTML with the outer HTML of the element
        if (element && element.__handle !== undefined) {
            var outerHTML = __nova.getOuterHTML(element.__handle);
            __nova.insertAdjacentHTML(this.__handle, position, outerHTML);
        }
        return element;
    };

    Element.prototype.dispatchEvent = function(event) {
        var type = (event && event.type) ? event.type : '';
        var handle = this.__handle;
        // Dispatch via Rust bridge
        __nova.dispatchEvent(handle, type);
        // Also fire JS-side listeners
        if (typeof __novaDispatchEvent === 'function') {
            __novaDispatchEvent(handle, type);
        }
        return true;
    };

    // nodeType property
    Object.defineProperty(Element.prototype, 'nodeType', {
        get: function() { return __nova.getNodeType(this.__handle); },
        configurable: true
    });

    // nodeName property (alias for tagName for elements)
    Object.defineProperty(Element.prototype, 'nodeName', {
        get: function() {
            var nt = __nova.getNodeType(this.__handle);
            if (nt === 3) return '#text';
            if (nt === 8) return '#comment';
            if (nt === 9) return '#document';
            return __nova.getTagName(this.__handle);
        },
        configurable: true
    });

    // outerHTML property
    Object.defineProperty(Element.prototype, 'outerHTML', {
        get: function() { return __nova.getOuterHTML(this.__handle); },
        configurable: true
    });

    // offsetWidth, offsetHeight, offsetTop, offsetLeft — approximate from styles
    Object.defineProperty(Element.prototype, 'offsetWidth', {
        get: function() { return parseFloat(__nova.styleGetProperty(this.__handle, 'width')) || 0; },
        configurable: true
    });
    Object.defineProperty(Element.prototype, 'offsetHeight', {
        get: function() { return parseFloat(__nova.styleGetProperty(this.__handle, 'height')) || 0; },
        configurable: true
    });
    Object.defineProperty(Element.prototype, 'offsetTop', {
        get: function() { return parseFloat(__nova.styleGetProperty(this.__handle, 'top')) || 0; },
        configurable: true
    });
    Object.defineProperty(Element.prototype, 'offsetLeft', {
        get: function() { return parseFloat(__nova.styleGetProperty(this.__handle, 'left')) || 0; },
        configurable: true
    });
    Object.defineProperty(Element.prototype, 'clientWidth', {
        get: function() { return parseFloat(__nova.styleGetProperty(this.__handle, 'width')) || 0; },
        configurable: true
    });
    Object.defineProperty(Element.prototype, 'clientHeight', {
        get: function() { return parseFloat(__nova.styleGetProperty(this.__handle, 'height')) || 0; },
        configurable: true
    });
    Object.defineProperty(Element.prototype, 'scrollWidth', {
        get: function() { return parseFloat(__nova.styleGetProperty(this.__handle, 'width')) || 0; },
        configurable: true
    });
    Object.defineProperty(Element.prototype, 'scrollHeight', {
        get: function() { return parseFloat(__nova.styleGetProperty(this.__handle, 'height')) || 0; },
        configurable: true
    });

    // Make Element available globally for instanceof checks
    globalThis.__NovaElement = Element;

    // Global event listener registry: handle -> { type -> [callbacks] }
    var __eventListeners = {};

    // Global dispatch function callable from Rust
    globalThis.__novaDispatchEvent = function(handle, type) {
        var listeners = __eventListeners[handle];
        if (!listeners || !listeners[type]) return;
        var event = { type: type, target: new Element(handle), bubbles: true, cancelable: true,
                      defaultPrevented: false, propagationStopped: false,
                      preventDefault: function() { this.defaultPrevented = true; },
                      stopPropagation: function() { this.propagationStopped = true; },
                      stopImmediatePropagation: function() { this.propagationStopped = true; },
                      currentTarget: new Element(handle) };
        var cbs = listeners[type].slice();
        for (var i = 0; i < cbs.length; i++) {
            cbs[i](event);
        }
    };

    // Helper to wrap handle
    function wrapHandle(h) {
        return h < 0 ? null : new Element(h);
    }

    // getComputedStyle
    globalThis.getComputedStyle = function(el) {
        var handle = el.__handle;
        return new Proxy({}, {
            get: function(target, prop) {
                if (prop === 'getPropertyValue') {
                    return function(name) {
                        return __nova.getComputedStyleProp(handle, name);
                    };
                }
                if (typeof prop === 'string') {
                    return __nova.getComputedStyleProp(handle, prop);
                }
                return "";
            }
        });
    };

    // window.getComputedStyle alias
    if (typeof window !== 'undefined') {
        window.getComputedStyle = globalThis.getComputedStyle;
    }

    // document object
    var document = {
        getElementById: function(id) {
            return wrapHandle(__nova.getElementById(id));
        },
        querySelector: function(sel) {
            return wrapHandle(__nova.querySelector(sel));
        },
        querySelectorAll: function(sel) {
            var arr = __nova.querySelectorAll(sel).map(function(h) { return new Element(h); });
            arr.item = function(i) { return i >= 0 && i < arr.length ? arr[i] : null; };
            return arr;
        },
        createElement: function(tag) {
            return new Element(__nova.createElement(tag));
        },
        createTextNode: function(text) {
            return new Element(__nova.createTextNode(text));
        },
        createDocumentFragment: function() {
            return new Element(__nova.createDocumentFragment());
        },
        createComment: function(data) {
            return new Element(__nova.createComment(data || ''));
        },
        createEvent: function(type) {
            return {
                type: '',
                bubbles: false,
                cancelable: false,
                initEvent: function(t, b, c) { this.type = t; this.bubbles = !!b; this.cancelable = !!c; }
            };
        },
        getElementsByClassName: function(name) {
            return __nova.getElementsByClassName(name).map(function(h) { return new Element(h); });
        },
        getElementsByTagName: function(tag) {
            return __nova.getElementsByTagName(tag).map(function(h) { return new Element(h); });
        },
        getElementsByName: function(name) {
            // Fallback: search by attribute name="..."
            var all = __nova.querySelectorAll('*');
            var result = [];
            for (var i = 0; i < all.length; i++) {
                var attr = __nova.getAttribute(all[i], 'name');
                if (attr === name) result.push(new Element(all[i]));
            }
            return result;
        },
        addEventListener: function(type, callback, optionsOrCapture) {
            if (typeof callback !== 'function') return;
            var once = false;
            if (optionsOrCapture && typeof optionsOrCapture === 'object') {
                once = !!optionsOrCapture.once;
            }
            var actualCallback = callback;
            if (once) {
                actualCallback = function __onceWrapper(evt) {
                    document.removeEventListener(type, actualCallback);
                    callback(evt);
                };
                actualCallback.__origCallback = callback;
            }
            if (!document.__listeners) document.__listeners = {};
            if (!document.__listeners[type]) document.__listeners[type] = [];
            document.__listeners[type].push(actualCallback);
            if (!__eventListeners[0]) __eventListeners[0] = {};
            if (!__eventListeners[0][type]) __eventListeners[0][type] = [];
            __eventListeners[0][type].push(actualCallback);
            __nova.addEventListener(0, type);
        },
        removeEventListener: function(type, callback) {
            if (!document.__listeners || !document.__listeners[type]) return;
            document.__listeners[type] = document.__listeners[type].filter(function(cb) {
                return cb !== callback && cb.__origCallback !== callback;
            });
            if (__eventListeners[0] && __eventListeners[0][type]) {
                __eventListeners[0][type] = __eventListeners[0][type].filter(function(cb) {
                    return cb !== callback && cb.__origCallback !== callback;
                });
            }
        },
        dispatchEvent: function(event) {
            var type = (event && event.type) ? event.type : '';
            __nova.dispatchEvent(0, type);
            if (__eventListeners[0] && __eventListeners[0][type]) {
                var evt = { type: type, target: document, bubbles: true, cancelable: true,
                            defaultPrevented: false, preventDefault: function() { this.defaultPrevented = true; },
                            stopPropagation: function() {} };
                var cbs = __eventListeners[0][type].slice();
                for (var i = 0; i < cbs.length; i++) { cbs[i](evt); }
            }
            return true;
        },
        // Stubbed properties
        domain: '',
        referrer: '',
        URL: '',
        characterSet: 'UTF-8',
        charset: 'UTF-8',
        contentType: 'text/html',
        compatMode: 'CSS1Compat',
        designMode: 'off',
        dir: ''
    };

    Object.defineProperty(document, 'readyState', {
        get: function() { return 'complete'; },
        configurable: true
    });

    Object.defineProperty(document, 'body', {
        get: function() { return wrapHandle(__nova.getBody()); },
        configurable: true
    });

    Object.defineProperty(document, 'head', {
        get: function() { return wrapHandle(__nova.getHead()); },
        configurable: true
    });

    Object.defineProperty(document, 'documentElement', {
        get: function() { return wrapHandle(__nova.getDocumentElement()); },
        configurable: true
    });

    Object.defineProperty(document, 'title', {
        get: function() { return __nova.getTitle(); },
        set: function(v) { __nova.setTitle(v); },
        configurable: true
    });

    // document.forms — live collection of all <form> elements
    Object.defineProperty(document, 'forms', {
        get: function() {
            var forms = __nova.querySelectorAll('form').map(function(h) { return new Element(h); });
            // Allow access by index and by name attribute
            for (var i = 0; i < forms.length; i++) {
                var nameAttr = __nova.getAttribute(forms[i].__handle, 'name');
                if (nameAttr) forms[nameAttr] = forms[i];
            }
            return forms;
        },
        configurable: true
    });

    // document.images — live collection of all <img> elements
    Object.defineProperty(document, 'images', {
        get: function() {
            return __nova.querySelectorAll('img').map(function(h) { return new Element(h); });
        },
        configurable: true
    });

    // document.links — collection of all <a> elements with href
    Object.defineProperty(document, 'links', {
        get: function() {
            var anchors = __nova.querySelectorAll('a');
            var result = [];
            for (var i = 0; i < anchors.length; i++) {
                var href = __nova.getAttribute(anchors[i], 'href');
                if (href !== null && href !== undefined) result.push(new Element(anchors[i]));
            }
            return result;
        },
        configurable: true
    });

    // document.scripts — collection of all <script> elements
    Object.defineProperty(document, 'scripts', {
        get: function() {
            return __nova.querySelectorAll('script').map(function(h) { return new Element(h); });
        },
        configurable: true
    });

    // document.styleSheets — stub
    Object.defineProperty(document, 'styleSheets', {
        get: function() { return []; },
        configurable: true
    });

    // document.nodeType
    Object.defineProperty(document, 'nodeType', {
        get: function() { return 9; },
        configurable: true
    });

    // document.nodeName
    Object.defineProperty(document, 'nodeName', {
        get: function() { return '#document'; },
        configurable: true
    });

    // document.childNodes
    Object.defineProperty(document, 'childNodes', {
        get: function() {
            return __nova.childNodes(0).map(function(h) { return new Element(h); });
        },
        configurable: true
    });

    // document.cookie get/set (stored in-memory for now)
    var __cookieJar = "";
    Object.defineProperty(document, 'cookie', {
        get: function() { return __cookieJar; },
        set: function(v) {
            // Append new cookie (simplified — real browsers parse Set-Cookie).
            if (__cookieJar.length > 0) __cookieJar += "; ";
            __cookieJar += v;
        },
        configurable: true
    });

    // ── document.write / document.writeln ────────────────────────
    // During initial page load, append content to the body.
    // After load, this is a no-op to avoid clearing the page.
    document.write = function() {
        var html = Array.prototype.slice.call(arguments).join('');
        // Try to append to body immediately.
        try {
            var body = document.body;
            if (body) {
                var temp = document.createElement('div');
                temp.innerHTML = html;
                var children = temp.childNodes;
                for (var ci = 0; ci < children.length; ci++) {
                    body.appendChild(children[ci]);
                }
            }
        } catch(e) { /* silently ignore errors */ }
    };
    document.writeln = function() {
        var html = Array.prototype.slice.call(arguments).join('') + "\n";
        document.write(html);
    };

    // ── document.execCommand() ──────────────────────────────────
    // Stub all commands — returns false (not supported).
    document.execCommand = function(command, showUI, value) {
        if (typeof console !== 'undefined') console.debug("[NOVA] document.execCommand('" + command + "') — not supported");
        return false;
    };
    document.queryCommandEnabled = function(command) { return false; };
    document.queryCommandSupported = function(command) { return false; };
    document.queryCommandState = function(command) { return false; };
    document.queryCommandValue = function(command) { return ''; };

    // ── document.createNodeIterator stub ─────────────────────────
    document.createNodeIterator = function(root, whatToShow, filter) {
        return {
            root: root, referenceNode: root, pointerBeforeReferenceNode: true,
            whatToShow: whatToShow || 0xFFFFFFFF, filter: filter || null,
            nextNode: function() { return null; },
            previousNode: function() { return null; },
            detach: function() {}
        };
    };

    // ── document.createRange (if not already defined) ────────────
    if (!document.createRange) {
        document.createRange = function() {
            return {
                startContainer: null, startOffset: 0, endContainer: null, endOffset: 0,
                collapsed: true, setStart: function(n, o) { this.startContainer = n; this.startOffset = o; },
                setEnd: function(n, o) { this.endContainer = n; this.endOffset = o; },
                collapse: function() { this.collapsed = true; }, selectNode: function() {},
                selectNodeContents: function() {}, cloneRange: function() { return document.createRange(); },
                detach: function() {}, getBoundingClientRect: function() {
                    return { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, bottom: 0, right: 0 };
                },
                getClientRects: function() { return []; },
                createContextualFragment: function(html) { var d = document.createElement('div'); d.innerHTML = html; return d; }
            };
        };
    }

    // ── Canvas 2D Context ──────────────────────────────────────────────

    function CanvasRenderingContext2D(canvasElement) {
        this.__canvas = canvasElement;
        this.__handle = canvasElement.__handle;
    }

    // Drawing methods
    CanvasRenderingContext2D.prototype.fillRect = function(x, y, w, h) {
        __nova.canvasFillRect(this.__handle, x, y, w, h);
    };
    CanvasRenderingContext2D.prototype.strokeRect = function(x, y, w, h) {
        __nova.canvasStrokeRect(this.__handle, x, y, w, h);
    };
    CanvasRenderingContext2D.prototype.clearRect = function(x, y, w, h) {
        __nova.canvasClearRect(this.__handle, x, y, w, h);
    };
    CanvasRenderingContext2D.prototype.fillText = function(text, x, y) {
        __nova.canvasFillText(this.__handle, text, x, y);
    };
    CanvasRenderingContext2D.prototype.strokeText = function(text, x, y) {
        __nova.canvasStrokeText(this.__handle, text, x, y);
    };
    CanvasRenderingContext2D.prototype.measureText = function(text) {
        return { width: __nova.canvasMeasureText(this.__handle, text) };
    };

    // Path methods
    CanvasRenderingContext2D.prototype.beginPath = function() {
        __nova.canvasBeginPath(this.__handle);
    };
    CanvasRenderingContext2D.prototype.moveTo = function(x, y) {
        __nova.canvasMoveTo(this.__handle, x, y);
    };
    CanvasRenderingContext2D.prototype.lineTo = function(x, y) {
        __nova.canvasLineTo(this.__handle, x, y);
    };
    CanvasRenderingContext2D.prototype.closePath = function() {
        __nova.canvasClosePath(this.__handle);
    };
    CanvasRenderingContext2D.prototype.arc = function(cx, cy, r, start, end) {
        __nova.canvasArc(this.__handle, cx, cy, r, start, end);
    };
    CanvasRenderingContext2D.prototype.rect = function(x, y, w, h) {
        __nova.canvasRect(this.__handle, x, y, w, h);
    };
    CanvasRenderingContext2D.prototype.fill = function() {
        __nova.canvasFill(this.__handle);
    };
    CanvasRenderingContext2D.prototype.stroke = function() {
        __nova.canvasStroke(this.__handle);
    };

    // State methods
    CanvasRenderingContext2D.prototype.save = function() {
        __nova.canvasSave(this.__handle);
    };
    CanvasRenderingContext2D.prototype.restore = function() {
        __nova.canvasRestore(this.__handle);
    };

    // Transform methods
    CanvasRenderingContext2D.prototype.translate = function(x, y) {
        __nova.canvasTranslate(this.__handle, x, y);
    };
    CanvasRenderingContext2D.prototype.rotate = function(angle) {
        __nova.canvasRotate(this.__handle, angle);
    };
    CanvasRenderingContext2D.prototype.scale = function(sx, sy) {
        __nova.canvasScale(this.__handle, sx, sy);
    };
    CanvasRenderingContext2D.prototype.setTransform = function(a, b, c, d, e, f) {
        __nova.canvasSetTransform(this.__handle, a, b, c, d, e, f);
    };

    // Image data methods
    CanvasRenderingContext2D.prototype.createImageData = function(w, h) {
        return { width: w, height: h, data: new Array(w * h * 4).fill(0) };
    };
    CanvasRenderingContext2D.prototype.getImageData = function(x, y, w, h) {
        var raw = __nova.canvasGetImageData(this.__handle, x, y, w, h);
        return { width: w, height: h, data: raw };
    };
    CanvasRenderingContext2D.prototype.putImageData = function(imageData, dx, dy) {
        var w = imageData.width;
        var h = imageData.height;
        __nova.canvasPutImageData(this.__handle, imageData.data, dx, dy, w, h);
    };

    // State properties via defineProperty
    Object.defineProperty(CanvasRenderingContext2D.prototype, 'fillStyle', {
        get: function() { return __nova.canvasGetFillStyle(this.__handle); },
        set: function(v) { __nova.canvasSetFillStyle(this.__handle, String(v)); },
        configurable: true
    });
    Object.defineProperty(CanvasRenderingContext2D.prototype, 'strokeStyle', {
        get: function() { return __nova.canvasGetStrokeStyle(this.__handle); },
        set: function(v) { __nova.canvasSetStrokeStyle(this.__handle, String(v)); },
        configurable: true
    });
    Object.defineProperty(CanvasRenderingContext2D.prototype, 'lineWidth', {
        get: function() { return __nova.canvasGetLineWidth(this.__handle); },
        set: function(v) { __nova.canvasSetLineWidth(this.__handle, v); },
        configurable: true
    });
    Object.defineProperty(CanvasRenderingContext2D.prototype, 'font', {
        get: function() { return __nova.canvasGetFont(this.__handle); },
        set: function(v) { __nova.canvasSetFont(this.__handle, String(v)); },
        configurable: true
    });
    Object.defineProperty(CanvasRenderingContext2D.prototype, 'textAlign', {
        get: function() { return "start"; },
        set: function(v) { __nova.canvasSetTextAlign(this.__handle, String(v)); },
        configurable: true
    });
    Object.defineProperty(CanvasRenderingContext2D.prototype, 'textBaseline', {
        get: function() { return "alphabetic"; },
        set: function(v) { __nova.canvasSetTextBaseline(this.__handle, String(v)); },
        configurable: true
    });
    Object.defineProperty(CanvasRenderingContext2D.prototype, 'globalAlpha', {
        get: function() { return __nova.canvasGetGlobalAlpha(this.__handle); },
        set: function(v) { __nova.canvasSetGlobalAlpha(this.__handle, v); },
        configurable: true
    });
    Object.defineProperty(CanvasRenderingContext2D.prototype, 'canvas', {
        get: function() { return this.__canvas; },
        configurable: true
    });

    // Make available globally
    globalThis.__CanvasRenderingContext2D = CanvasRenderingContext2D;

    // ── Add getContext to Element ─────────────────────────────────────────

    Element.prototype.getContext = function(contextType) {
        if (contextType !== '2d') return null;
        var ok = __nova.canvasGetContext(this.__handle);
        if (!ok) return null;
        if (!this.__ctx2d) {
            this.__ctx2d = new CanvasRenderingContext2D(this);
        }
        return this.__ctx2d;
    };

    // Add width/height properties for canvas elements
    Object.defineProperty(Element.prototype, 'width', {
        get: function() {
            var w = __nova.canvasGetWidth(this.__handle);
            if (w > 0) return w;
            var attr = __nova.getAttribute(this.__handle, "width");
            return attr ? parseInt(attr) : 0;
        },
        set: function(v) { __nova.setAttribute(this.__handle, "width", String(v)); },
        configurable: true
    });
    Object.defineProperty(Element.prototype, 'height', {
        get: function() {
            var h = __nova.canvasGetHeight(this.__handle);
            if (h > 0) return h;
            var attr = __nova.getAttribute(this.__handle, "height");
            return attr ? parseInt(attr) : 0;
        },
        set: function(v) { __nova.setAttribute(this.__handle, "height", String(v)); },
        configurable: true
    });

    globalThis.document = document;

    // ── window.location ──────────────────────────────────────────────────────
    if (typeof window !== 'undefined') {
        var __currentUrl = window.location ? (window.location.href || "about:blank") : "about:blank";
        try {
            var urlObj = { href: __currentUrl, protocol: "", hostname: "", pathname: "/", search: "", hash: "", host: "", port: "", origin: "" };
            // Parse URL parts
            var match = __currentUrl.match(/^(https?:)\/\/([^/:]+)(:\d+)?(\/[^?#]*)(\?[^#]*)?(#.*)?$/);
            if (match) {
                urlObj.protocol = match[1] || "";
                urlObj.hostname = match[2] || "";
                urlObj.port = (match[3] || "").replace(":", "");
                urlObj.host = urlObj.hostname + (urlObj.port ? ":" + urlObj.port : "");
                urlObj.pathname = match[4] || "/";
                urlObj.search = match[5] || "";
                urlObj.hash = match[6] || "";
                urlObj.origin = urlObj.protocol + "//" + urlObj.host;
            }

            var location = {};
            var __locData = {
                href: urlObj.href,
                protocol: urlObj.protocol,
                hostname: urlObj.hostname,
                host: urlObj.host,
                port: urlObj.port,
                pathname: urlObj.pathname,
                search: urlObj.search,
                hash: urlObj.hash,
                origin: urlObj.origin
            };
            Object.defineProperty(location, 'href', {
                get: function() { return __locData.href; },
                set: function(url) {
                    var resolved = __resolveUrl(url);
                    __locData.href = resolved;
                    __nova.requestNavigation(resolved);
                },
                configurable: true
            });
            Object.defineProperty(location, 'protocol', {
                get: function() { return __locData.protocol; },
                configurable: true
            });
            Object.defineProperty(location, 'hostname', {
                get: function() { return __locData.hostname; },
                configurable: true
            });
            Object.defineProperty(location, 'host', {
                get: function() { return __locData.host; },
                configurable: true
            });
            Object.defineProperty(location, 'port', {
                get: function() { return __locData.port; },
                configurable: true
            });
            Object.defineProperty(location, 'pathname', {
                get: function() { return __locData.pathname; },
                set: function(v) {
                    __locData.pathname = v;
                    var newUrl = __locData.protocol + '//' + __locData.host + v + __locData.search + __locData.hash;
                    __locData.href = newUrl;
                    __nova.requestNavigation(newUrl);
                },
                configurable: true
            });
            Object.defineProperty(location, 'search', {
                get: function() { return __locData.search; },
                set: function(v) {
                    __locData.search = v;
                    var newUrl = __locData.protocol + '//' + __locData.host + __locData.pathname + v + __locData.hash;
                    __locData.href = newUrl;
                    __nova.requestNavigation(newUrl);
                },
                configurable: true
            });
            Object.defineProperty(location, 'hash', {
                get: function() { return __locData.hash; },
                set: function(v) {
                    var oldHash = __locData.hash;
                    var oldUrl = __locData.href;
                    __locData.hash = v.charAt(0) === '#' ? v : '#' + v;
                    __locData.href = __locData.origin + __locData.pathname + __locData.search + __locData.hash;
                    // Fire hashchange event if hash actually changed.
                    if (oldHash !== __locData.hash) {
                        __fireWindowEvent('hashchange', { oldURL: oldUrl, newURL: __locData.href });
                    }
                },
                configurable: true
            });
            Object.defineProperty(location, 'origin', {
                get: function() { return __locData.origin; },
                configurable: true
            });
            location.assign = function(url) {
                __nova.requestNavigation(__resolveUrl(url));
            };
            location.replace = function(url) {
                __nova.requestNavigation(__resolveUrl(url));
            };
            location.reload = function() {
                __nova.requestNavigation(__locData.href);
            };
            location.toString = function() { return __locData.href; };
            window.location = location;
        } catch(e) {}

        // ── window.history ───────────────────────────────────────────────────
        var __historyStack = [{ url: __currentUrl, state: null }];
        var __historyIndex = 0;

        // Helper: resolve a possibly-relative URL against the current page URL.
        function __resolveUrl(url) {
            if (!url) return __locData.href;
            // Already absolute
            if (/^https?:\/\//.test(url)) return url;
            // Protocol-relative
            if (url.indexOf('//') === 0) return __locData.protocol + url;
            // Absolute path
            if (url.charAt(0) === '/') return __locData.origin + url;
            // Hash-only
            if (url.charAt(0) === '#') return __locData.origin + __locData.pathname + __locData.search + url;
            // Query-only
            if (url.charAt(0) === '?') return __locData.origin + __locData.pathname + url;
            // Relative path — resolve against current directory
            var base = __locData.pathname.substring(0, __locData.pathname.lastIndexOf('/') + 1);
            return __locData.origin + base + url;
        }

        // Helper: update __locData from a full URL string.
        function __updateLocData(fullUrl) {
            var m = fullUrl.match(/^(https?:)\/\/([^/:]+)(:\d+)?(\/[^?#]*)?(\?[^#]*)?(#.*)?$/);
            if (m) {
                __locData.href = fullUrl;
                __locData.protocol = m[1] || "";
                __locData.hostname = m[2] || "";
                __locData.port = (m[3] || "").replace(":", "");
                __locData.host = __locData.hostname + (__locData.port ? ":" + __locData.port : "");
                __locData.pathname = m[4] || "/";
                __locData.search = m[5] || "";
                __locData.hash = m[6] || "";
                __locData.origin = __locData.protocol + "//" + __locData.host;
            }
        }

        var __historyState = null;

        window.history = {
            get length() { return __historyStack.length; },
            get state() { return __historyState; },
            pushState: function(state, title, url) {
                var resolvedUrl = __resolveUrl(url);
                var oldHash = __locData.hash;
                __historyStack.splice(__historyIndex + 1);
                __historyStack.push({ url: resolvedUrl, state: state });
                __historyIndex = __historyStack.length - 1;
                __historyState = state;
                __updateLocData(resolvedUrl);
                // Signal to the Rust side that the URL changed via pushState.
                if (typeof __nova !== 'undefined' && __nova.pushState) {
                    __nova.pushState(resolvedUrl);
                }
                // Fire hashchange if the hash changed.
                var newHash = __locData.hash;
                if (oldHash !== newHash) {
                    __fireWindowEvent('hashchange', { oldURL: __locData.origin + __locData.pathname + __locData.search + oldHash, newURL: resolvedUrl });
                }
            },
            replaceState: function(state, title, url) {
                var resolvedUrl = __resolveUrl(url);
                __historyStack[__historyIndex] = { url: resolvedUrl, state: state };
                __historyState = state;
                __updateLocData(resolvedUrl);
                // Signal to the Rust side that the URL changed via replaceState.
                if (typeof __nova !== 'undefined' && __nova.replaceState) {
                    __nova.replaceState(resolvedUrl);
                }
            },
            back: function() {
                if (__historyIndex > 0) {
                    var oldUrl = __locData.href;
                    var oldHash = __locData.hash;
                    __historyIndex--;
                    var entry = __historyStack[__historyIndex];
                    var newUrl = (typeof entry === 'object') ? entry.url : entry;
                    __historyState = (typeof entry === 'object') ? entry.state : null;
                    __updateLocData(newUrl);
                    __fireWindowEvent('popstate', { state: __historyState });
                    if (oldHash !== __locData.hash) {
                        __fireWindowEvent('hashchange', { oldURL: oldUrl, newURL: newUrl });
                    }
                }
            },
            forward: function() {
                if (__historyIndex < __historyStack.length - 1) {
                    var oldUrl = __locData.href;
                    var oldHash = __locData.hash;
                    __historyIndex++;
                    var entry = __historyStack[__historyIndex];
                    var newUrl = (typeof entry === 'object') ? entry.url : entry;
                    __historyState = (typeof entry === 'object') ? entry.state : null;
                    __updateLocData(newUrl);
                    __fireWindowEvent('popstate', { state: __historyState });
                    if (oldHash !== __locData.hash) {
                        __fireWindowEvent('hashchange', { oldURL: oldUrl, newURL: newUrl });
                    }
                }
            },
            go: function(delta) {
                var newIndex = __historyIndex + (delta || 0);
                if (newIndex >= 0 && newIndex < __historyStack.length) {
                    var oldUrl = __locData.href;
                    var oldHash = __locData.hash;
                    __historyIndex = newIndex;
                    var entry = __historyStack[__historyIndex];
                    var newUrl = (typeof entry === 'object') ? entry.url : entry;
                    __historyState = (typeof entry === 'object') ? entry.state : null;
                    __updateLocData(newUrl);
                    __fireWindowEvent('popstate', { state: __historyState });
                    if (oldHash !== __locData.hash) {
                        __fireWindowEvent('hashchange', { oldURL: oldUrl, newURL: newUrl });
                    }
                }
            }
        };

        // ── Event dispatch helpers for popstate / hashchange ─────────────────
        // These fire both `on*` handlers and `addEventListener` listeners.
        function __fireWindowEvent(type, eventObj) {
            eventObj.type = type;
            eventObj.target = window;
            eventObj.bubbles = false;
            eventObj.cancelable = false;
            eventObj.defaultPrevented = false;
            eventObj.preventDefault = function() {};
            eventObj.stopPropagation = function() {};
            // Fire on* handler.
            var handler = window['on' + type];
            if (typeof handler === 'function') {
                try { handler(eventObj); } catch(e) {}
            }
            // Fire addEventListener listeners.
            if (typeof __windowListeners !== 'undefined') {
                var cbs = __windowListeners[type];
                if (cbs) {
                    for (var i = 0; i < cbs.length; i++) {
                        try { cbs[i](eventObj); } catch(e) {}
                    }
                }
            }
        }

        // ── window.scrollTo / scrollBy ───────────────────────────────────────
        window.scrollTo = function(x, y) { /* stub */ };
        window.scrollBy = function(x, y) { /* stub */ };
        window.scroll = window.scrollTo;

        // ── navigator enhancements ───────────────────────────────────────────
        if (window.navigator) {
            window.navigator.cookieEnabled = true;
            window.navigator.onLine = true;
            window.navigator.languages = ["en-US", "en"];
            window.navigator.vendor = "NOVA";
            window.navigator.appName = "NOVA";
            window.navigator.appVersion = "0.1.0";
        }

        // ── window.requestAnimationFrame ─────────────────────────────────────
        var __rafId = 0;
        window.requestAnimationFrame = function(callback) {
            __rafId++;
            // Execute immediately in our synchronous engine.
            try { callback(Date.now()); } catch(e) {}
            return __rafId;
        };
        window.cancelAnimationFrame = function(id) { /* stub */ };

        // ── window event listeners ──────────────────────────────────────────
        var __windowListeners = {};

        window.addEventListener = function(type, callback, optionsOrCapture) {
            if (typeof callback !== 'function') return;
            var once = false;
            if (optionsOrCapture && typeof optionsOrCapture === 'object') {
                once = !!optionsOrCapture.once;
            }
            var actualCallback = callback;
            if (once) {
                actualCallback = function __onceWrapper(evt) {
                    window.removeEventListener(type, actualCallback);
                    callback(evt);
                };
                actualCallback.__origCallback = callback;
            }
            if (!__windowListeners[type]) __windowListeners[type] = [];
            __windowListeners[type].push(actualCallback);
            // For DOMContentLoaded and load, fire immediately since we are already loaded
            if (type === 'DOMContentLoaded' || type === 'load') {
                try {
                    var evt = { type: type, target: window, bubbles: false, cancelable: false,
                                defaultPrevented: false, preventDefault: function() {},
                                stopPropagation: function() {} };
                    actualCallback(evt);
                } catch(e) { /* ignore errors in listener */ }
            }
        };

        window.removeEventListener = function(type, callback) {
            if (!__windowListeners[type]) return;
            __windowListeners[type] = __windowListeners[type].filter(function(cb) {
                return cb !== callback && cb.__origCallback !== callback;
            });
        };

        window.dispatchEvent = function(event) {
            var type = (event && event.type) ? event.type : '';
            var cbs = __windowListeners[type];
            if (cbs) {
                var evt = { type: type, target: window, bubbles: false, cancelable: false,
                            defaultPrevented: false, preventDefault: function() {},
                            stopPropagation: function() {} };
                for (var i = 0; i < cbs.length; i++) {
                    try { cbs[i](evt); } catch(e) { /* ignore */ }
                }
            }
            return true;
        };

        // window.self / window.top / window.parent
        window.self = window;
        window.top = window;
        window.parent = window;

        // window.document alias
        window.document = globalThis.document;

        // Expose globalThis aliases
        globalThis.setTimeout = window.setTimeout;
        globalThis.setInterval = window.setInterval;
        globalThis.clearTimeout = window.clearTimeout;
        globalThis.clearInterval = window.clearInterval;
        globalThis.requestAnimationFrame = window.requestAnimationFrame;
        globalThis.cancelAnimationFrame = window.cancelAnimationFrame;
    }

    // ── Event constructor ────────────────────────────────────────────────
    if (typeof globalThis.Event === 'undefined') {
        globalThis.Event = function(type, options) {
            this.type = type;
            this.bubbles = (options && options.bubbles) || false;
            this.cancelable = (options && options.cancelable) || false;
            this.defaultPrevented = false;
            this.preventDefault = function() { this.defaultPrevented = true; };
            this.stopPropagation = function() {};
            this.stopImmediatePropagation = function() {};
        };
    }

    if (typeof globalThis.CustomEvent === 'undefined') {
        globalThis.CustomEvent = function(type, options) {
            this.type = type;
            this.detail = (options && options.detail) || null;
            this.bubbles = (options && options.bubbles) || false;
            this.cancelable = (options && options.cancelable) || false;
            this.defaultPrevented = false;
            this.preventDefault = function() { this.defaultPrevented = true; };
            this.stopPropagation = function() {};
            this.stopImmediatePropagation = function() {};
        };
    }

    // ── MutationObserver stub ────────────────────────────────────────────
    if (typeof globalThis.MutationObserver === 'undefined') {
        globalThis.MutationObserver = function(callback) {
            this.observe = function() {};
            this.disconnect = function() {};
            this.takeRecords = function() { return []; };
        };
    }

    // ── IntersectionObserver stub ────────────────────────────────────────
    if (typeof globalThis.IntersectionObserver === 'undefined') {
        globalThis.IntersectionObserver = function(callback, options) {
            this.observe = function() {};
            this.unobserve = function() {};
            this.disconnect = function() {};
        };
    }

    // ── ResizeObserver stub ──────────────────────────────────────────────
    if (typeof globalThis.ResizeObserver === 'undefined') {
        globalThis.ResizeObserver = function(callback) {
            this.observe = function() {};
            this.unobserve = function() {};
            this.disconnect = function() {};
        };
    }

    // matchMedia stub — returns a MediaQueryList-like object.
    // We default to light mode (prefers-color-scheme: dark → false,
    // prefers-color-scheme: light → true).
    globalThis.matchMedia = function(query) {
        var matches = false;
        if (typeof query === 'string') {
            var q = query.toLowerCase();
            if (q.indexOf('prefers-color-scheme') !== -1) {
                if (q.indexOf('light') !== -1) {
                    matches = true;
                } else if (q.indexOf('dark') !== -1) {
                    matches = false;
                }
            }
            // For width/height media queries, default to true (assume desktop).
            // This is a basic stub — real evaluation would need viewport info.
        }
        return {
            matches: matches,
            media: query || '',
            onchange: null,
            addListener: function() {},
            removeListener: function() {},
            addEventListener: function() {},
            removeEventListener: function() {},
            dispatchEvent: function() { return false; }
        };
    };
    globalThis.window = globalThis.window || {};
    globalThis.window.matchMedia = globalThis.matchMedia;

    // Fire DOMContentLoaded on document listeners since the DOM is ready
    if (document.__listeners && document.__listeners['DOMContentLoaded']) {
        var evt = { type: 'DOMContentLoaded', target: document, bubbles: false };
        var cbs = document.__listeners['DOMContentLoaded'].slice();
        for (var i = 0; i < cbs.length; i++) {
            try { cbs[i](evt); } catch(e) { /* ignore */ }
        }
    }
})();
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // Integration tests are in quickjs_runtime.rs.
}

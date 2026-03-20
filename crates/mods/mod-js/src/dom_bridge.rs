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

/// Create a `console` JS object with `log`, `warn`, `error` methods.
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

    Ok(console)
}

// ── Window ───────────────────────────────────────────────────────────────────

/// Create a `window` JS object with basic properties.
pub fn create_window<'js>(ctx: &Ctx<'js>, url: &str) -> Result<Object<'js>> {
    let window = Object::new(ctx.clone())?;

    let location = Object::new(ctx.clone())?;
    location.set("href", url)?;
    location.set("protocol", "https:")?;
    location.set("host", "")?;
    location.set("pathname", "/")?;
    window.set("location", location)?;

    let navigator = Object::new(ctx.clone())?;
    navigator.set("userAgent", "NOVA/0.1.0")?;
    navigator.set("language", "en-US")?;
    navigator.set("platform", "NOVA")?;
    window.set("navigator", navigator)?;

    window.set("innerWidth", 1920)?;
    window.set("innerHeight", 1080)?;

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

    ctx.globals().set("__nova", nova)?;

    Ok(())
}

/// JavaScript shim that creates the DOM API on top of `__nova` bridge functions.
///
/// This defines `Element`, `document`, and installs property accessors.
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

    Object.defineProperty(Element.prototype, 'parentNode', {
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

    Object.defineProperty(Element.prototype, 'nextSibling', {
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

    // classList
    Object.defineProperty(Element.prototype, 'classList', {
        get: function() {
            var self = this;
            return {
                add: function(cls) { __nova.classListAdd(self.__handle, cls); },
                remove: function(cls) { __nova.classListRemove(self.__handle, cls); },
                contains: function(cls) { return __nova.classListContains(self.__handle, cls); },
                toggle: function(cls) { return __nova.classListToggle(self.__handle, cls); }
            };
        },
        configurable: true
    });

    // style
    Object.defineProperty(Element.prototype, 'style', {
        get: function() {
            var self = this;
            return new Proxy({}, {
                set: function(target, prop, value) {
                    __nova.styleSetProperty(self.__handle, prop, value);
                    return true;
                },
                get: function(target, prop) {
                    if (prop === 'setProperty') {
                        return function(name, value) { __nova.styleSetProperty(self.__handle, name, value); };
                    }
                    if (prop === 'getPropertyValue') {
                        return function(name) { return __nova.styleGetProperty(self.__handle, name); };
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

    Element.prototype.insertBefore = function(newChild, refChild) {
        var refH = refChild ? refChild.__handle : -1;
        __nova.insertBefore(this.__handle, newChild.__handle, refH);
        return newChild;
    };

    Element.prototype.addEventListener = function(type, callback) {
        // Store in both the element and the global registry
        if (!this.__listeners) this.__listeners = {};
        if (!this.__listeners[type]) this.__listeners[type] = [];
        this.__listeners[type].push(callback);
        // Store in global registry keyed by handle for Rust dispatch
        var h = this.__handle;
        if (!__eventListeners[h]) __eventListeners[h] = {};
        if (!__eventListeners[h][type]) __eventListeners[h][type] = [];
        __eventListeners[h][type].push(callback);
        __nova.addEventListener(h, type);
    };

    Element.prototype.removeEventListener = function(type, callback) {
        if (!this.__listeners || !this.__listeners[type]) return;
        this.__listeners[type] = this.__listeners[type].filter(function(cb) {
            return cb !== callback;
        });
    };

    Element.prototype.querySelector = function(sel) {
        var h = __nova.querySelectorWithin(this.__handle, sel);
        return h < 0 ? null : new Element(h);
    };

    Element.prototype.querySelectorAll = function(sel) {
        return __nova.querySelectorAllWithin(this.__handle, sel).map(function(h) {
            return new Element(h);
        });
    };

    Element.prototype.cloneNode = function(deep) {
        var h = __nova.cloneNode(this.__handle, !!deep);
        return new Element(h);
    };

    Element.prototype.contains = function(other) {
        return __nova.contains(this.__handle, other.__handle);
    };

    Element.prototype.getBoundingClientRect = function() {
        return { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, bottom: 0, right: 0 };
    };

    Element.prototype.matches = function(sel) {
        return __nova.matches(this.__handle, sel);
    };

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
                      stopPropagation: function() { this.propagationStopped = true; } };
        var cbs = listeners[type].slice();
        for (var i = 0; i < cbs.length; i++) {
            cbs[i](event);
        }
    };

    // Helper to wrap handle
    function wrapHandle(h) {
        return h < 0 ? null : new Element(h);
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
            return __nova.querySelectorAll(sel).map(function(h) { return new Element(h); });
        },
        createElement: function(tag) {
            return new Element(__nova.createElement(tag));
        },
        createTextNode: function(text) {
            return new Element(__nova.createTextNode(text));
        },
        addEventListener: function(type, callback) {
            if (!document.__listeners) document.__listeners = {};
            if (!document.__listeners[type]) document.__listeners[type] = [];
            document.__listeners[type].push(callback);
            if (!__eventListeners[0]) __eventListeners[0] = {};
            if (!__eventListeners[0][type]) __eventListeners[0][type] = [];
            __eventListeners[0][type].push(callback);
            __nova.addEventListener(0, type);
        },
        dispatchEvent: function(event) {
            __nova.dispatchEvent(0, event.type || "");
        }
    };

    Object.defineProperty(document, 'body', {
        get: function() { return wrapHandle(__nova.getBody()); },
        configurable: true
    });

    Object.defineProperty(document, 'title', {
        get: function() { return __nova.getTitle(); },
        set: function(v) { __nova.setTitle(v); },
        configurable: true
    });

    globalThis.document = document;
})();
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // Integration tests are in quickjs_runtime.rs.
}

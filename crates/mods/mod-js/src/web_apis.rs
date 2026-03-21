//! # web_apis
//!
//! JavaScript stubs for common Web APIs that modern sites expect to exist.
//!
//! These are installed into the QuickJS context alongside the DOM shim so that
//! frameworks (React, Vue, etc.) and lazy-loading libraries do not crash when
//! they probe for browser features.
//!
//! ## Covered APIs
//!
//! - `IntersectionObserver` — lazy-loading images
//! - `MutationObserver` — framework DOM-change watching
//! - `ResizeObserver` — responsive layouts
//! - `window.performance` — timing / marks
//! - `window.matchMedia` — responsive CSS queries in JS
//! - `document.createEvent` / `document.createRange` / `document.createTreeWalker`
//! - `window.getSelection`
//! - `window.crypto.getRandomValues`
//! - `window.URL` / `window.URLSearchParams`
//! - `window.btoa` / `window.atob`
//! - `window.queueMicrotask`
//! - `window.structuredClone`
//! - `window.customElements`

/// JavaScript shim containing all Web API stubs.
///
/// This is evaluated once when the QuickJS context is created, right after the
/// DOM shim and Service Worker shim.
pub const JS_WEB_APIS_SHIM: &str = r#"
(function() {
    "use strict";

    // ── IntersectionObserver ────────────────────────────────────────────────
    if (typeof IntersectionObserver === 'undefined') {
        globalThis.IntersectionObserver = function(callback, options) {
            this._callback = callback;
            this.observe = function(el) {
                // Immediately report all entries as intersecting
                callback([{
                    target: el,
                    isIntersecting: true,
                    intersectionRatio: 1.0,
                    boundingClientRect: { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, bottom: 0, right: 0 },
                    intersectionRect: { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, bottom: 0, right: 0 },
                    rootBounds: null,
                    time: Date.now()
                }]);
            };
            this.unobserve = function() {};
            this.disconnect = function() {};
            this.takeRecords = function() { return []; };
        };
    }

    // ── MutationObserver ────────────────────────────────────────────────────
    if (typeof MutationObserver === 'undefined') {
        globalThis.MutationObserver = function(callback) {
            this._callback = callback;
            this.observe = function(target, config) {};
            this.disconnect = function() {};
            this.takeRecords = function() { return []; };
        };
    }

    // ── ResizeObserver ──────────────────────────────────────────────────────
    if (typeof ResizeObserver === 'undefined') {
        globalThis.ResizeObserver = function(callback) {
            this._callback = callback;
            this.observe = function(el) {};
            this.unobserve = function(el) {};
            this.disconnect = function() {};
        };
    }

    // ── Performance API ─────────────────────────────────────────────────────
    if (!globalThis.performance) {
        var perfStart = Date.now();
        globalThis.performance = {
            now: function() { return Date.now() - perfStart; },
            timing: { navigationStart: perfStart },
            mark: function() {},
            measure: function() {},
            getEntriesByName: function() { return []; },
            getEntriesByType: function() { return []; },
            getEntries: function() { return []; },
            clearMarks: function() {},
            clearMeasures: function() {}
        };
    }

    // ── matchMedia ──────────────────────────────────────────────────────────
    if (!globalThis.matchMedia) {
        globalThis.matchMedia = function(query) {
            return {
                matches: false,
                media: query,
                onchange: null,
                addEventListener: function() {},
                removeEventListener: function() {},
                addListener: function() {},
                removeListener: function() {},
                dispatchEvent: function() { return true; }
            };
        };
    }

    // ── document.createEvent ────────────────────────────────────────────────
    if (typeof document !== 'undefined' && !document.createEvent) {
        document.createEvent = function(type) {
            return {
                type: '',
                bubbles: false,
                cancelable: false,
                defaultPrevented: false,
                initEvent: function(type, bubbles, cancelable) {
                    this.type = type;
                    this.bubbles = !!bubbles;
                    this.cancelable = !!cancelable;
                },
                preventDefault: function() { this.defaultPrevented = true; },
                stopPropagation: function() {},
                stopImmediatePropagation: function() {}
            };
        };
    }

    // ── document.createRange ────────────────────────────────────────────────
    if (typeof document !== 'undefined' && !document.createRange) {
        document.createRange = function() {
            return {
                startContainer: null,
                startOffset: 0,
                endContainer: null,
                endOffset: 0,
                collapsed: true,
                setStart: function(node, offset) {
                    this.startContainer = node;
                    this.startOffset = offset;
                },
                setEnd: function(node, offset) {
                    this.endContainer = node;
                    this.endOffset = offset;
                },
                collapse: function(toStart) { this.collapsed = true; },
                selectNode: function(node) {},
                selectNodeContents: function(node) {},
                cloneRange: function() { return document.createRange(); },
                detach: function() {},
                getBoundingClientRect: function() {
                    return { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, bottom: 0, right: 0 };
                },
                getClientRects: function() { return []; },
                createContextualFragment: function(html) {
                    var div = document.createElement('div');
                    div.innerHTML = html;
                    return div;
                }
            };
        };
    }

    // ── document.createTreeWalker ───────────────────────────────────────────
    if (typeof document !== 'undefined' && !document.createTreeWalker) {
        document.createTreeWalker = function(root, whatToShow, filter) {
            return {
                root: root,
                currentNode: root,
                whatToShow: whatToShow || 0xFFFFFFFF,
                filter: filter || null,
                nextNode: function() { return null; },
                previousNode: function() { return null; },
                firstChild: function() { return null; },
                lastChild: function() { return null; },
                nextSibling: function() { return null; },
                previousSibling: function() { return null; },
                parentNode: function() { return null; }
            };
        };
    }

    // ── document.createDocumentFragment ─────────────────────────────────────
    if (typeof document !== 'undefined' && !document.createDocumentFragment) {
        document.createDocumentFragment = function() {
            var frag = document.createElement('div');
            frag.__isFragment = true;
            return frag;
        };
    }

    // ── window.getSelection ─────────────────────────────────────────────────
    if (!globalThis.getSelection) {
        globalThis.getSelection = function() {
            return {
                anchorNode: null,
                anchorOffset: 0,
                focusNode: null,
                focusOffset: 0,
                isCollapsed: true,
                rangeCount: 0,
                type: 'None',
                addRange: function() {},
                removeAllRanges: function() {},
                removeRange: function() {},
                collapse: function() {},
                collapseToStart: function() {},
                collapseToEnd: function() {},
                extend: function() {},
                getRangeAt: function() { return typeof document !== 'undefined' && document.createRange ? document.createRange() : {}; },
                toString: function() { return ''; },
                containsNode: function() { return false; }
            };
        };
    }

    // ── window.crypto.getRandomValues ───────────────────────────────────────
    if (!globalThis.crypto) {
        globalThis.crypto = {};
    }
    if (!globalThis.crypto.getRandomValues) {
        globalThis.crypto.getRandomValues = function(array) {
            for (var i = 0; i < array.length; i++) {
                array[i] = Math.floor(Math.random() * 256);
            }
            return array;
        };
    }
    if (!globalThis.crypto.randomUUID) {
        globalThis.crypto.randomUUID = function() {
            var hex = '0123456789abcdef';
            var uuid = '';
            for (var i = 0; i < 36; i++) {
                if (i === 8 || i === 13 || i === 18 || i === 23) {
                    uuid += '-';
                } else if (i === 14) {
                    uuid += '4';
                } else {
                    uuid += hex[Math.floor(Math.random() * 16)];
                }
            }
            return uuid;
        };
    }

    // ── window.btoa / window.atob ───────────────────────────────────────────
    if (!globalThis.btoa) {
        var _b64chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
        globalThis.btoa = function(str) {
            var out = '';
            for (var i = 0; i < str.length; i += 3) {
                var c1 = str.charCodeAt(i);
                var c2 = i + 1 < str.length ? str.charCodeAt(i + 1) : 0;
                var c3 = i + 2 < str.length ? str.charCodeAt(i + 2) : 0;
                out += _b64chars[(c1 >> 2) & 0x3F];
                out += _b64chars[((c1 << 4) | (c2 >> 4)) & 0x3F];
                out += (i + 1 < str.length) ? _b64chars[((c2 << 2) | (c3 >> 6)) & 0x3F] : '=';
                out += (i + 2 < str.length) ? _b64chars[c3 & 0x3F] : '=';
            }
            return out;
        };
    }
    if (!globalThis.atob) {
        var _b64lookup = {};
        var _b64chars2 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
        for (var i = 0; i < _b64chars2.length; i++) {
            _b64lookup[_b64chars2[i]] = i;
        }
        globalThis.atob = function(str) {
            str = str.replace(/=+$/, '');
            var out = '';
            for (var i = 0; i < str.length; i += 4) {
                var b1 = _b64lookup[str[i]] || 0;
                var b2 = _b64lookup[str[i + 1]] || 0;
                var b3 = _b64lookup[str[i + 2]] || 0;
                var b4 = _b64lookup[str[i + 3]] || 0;
                out += String.fromCharCode((b1 << 2) | (b2 >> 4));
                if (str[i + 2] !== undefined) out += String.fromCharCode(((b2 & 0x0F) << 4) | (b3 >> 2));
                if (str[i + 3] !== undefined) out += String.fromCharCode(((b3 & 0x03) << 6) | b4);
            }
            return out;
        };
    }

    // ── window.queueMicrotask ───────────────────────────────────────────────
    if (!globalThis.queueMicrotask) {
        globalThis.queueMicrotask = function(callback) {
            Promise.resolve().then(callback);
        };
    }

    // ── window.structuredClone ──────────────────────────────────────────────
    if (!globalThis.structuredClone) {
        globalThis.structuredClone = function(obj) {
            return JSON.parse(JSON.stringify(obj));
        };
    }

    // ── window.customElements ───────────────────────────────────────────────
    if (!globalThis.customElements) {
        var _registry = {};
        var _whenDefinedPromises = {};
        globalThis.customElements = {
            define: function(name, constructor, options) {
                _registry[name] = { constructor: constructor, options: options };
                if (_whenDefinedPromises[name]) {
                    _whenDefinedPromises[name].forEach(function(resolve) { resolve(constructor); });
                    delete _whenDefinedPromises[name];
                }
            },
            get: function(name) {
                var entry = _registry[name];
                return entry ? entry.constructor : undefined;
            },
            whenDefined: function(name) {
                if (_registry[name]) {
                    return Promise.resolve(_registry[name].constructor);
                }
                return new Promise(function(resolve) {
                    if (!_whenDefinedPromises[name]) _whenDefinedPromises[name] = [];
                    _whenDefinedPromises[name].push(resolve);
                });
            },
            upgrade: function(root) {}
        };
    }

    // ── URL / URLSearchParams ───────────────────────────────────────────────
    if (typeof URLSearchParams === 'undefined') {
        globalThis.URLSearchParams = function(init) {
            this._params = [];
            if (typeof init === 'string') {
                var str = init.replace(/^\?/, '');
                var pairs = str.split('&');
                for (var i = 0; i < pairs.length; i++) {
                    var kv = pairs[i].split('=');
                    if (kv[0]) {
                        this._params.push([
                            decodeURIComponent(kv[0]),
                            kv[1] !== undefined ? decodeURIComponent(kv[1]) : ''
                        ]);
                    }
                }
            }
        };
        URLSearchParams.prototype.get = function(name) {
            for (var i = 0; i < this._params.length; i++) {
                if (this._params[i][0] === name) return this._params[i][1];
            }
            return null;
        };
        URLSearchParams.prototype.getAll = function(name) {
            var result = [];
            for (var i = 0; i < this._params.length; i++) {
                if (this._params[i][0] === name) result.push(this._params[i][1]);
            }
            return result;
        };
        URLSearchParams.prototype.has = function(name) {
            return this.get(name) !== null;
        };
        URLSearchParams.prototype.set = function(name, value) {
            var found = false;
            for (var i = 0; i < this._params.length; i++) {
                if (this._params[i][0] === name) {
                    if (!found) {
                        this._params[i][1] = String(value);
                        found = true;
                    } else {
                        this._params.splice(i, 1);
                        i--;
                    }
                }
            }
            if (!found) this._params.push([name, String(value)]);
        };
        URLSearchParams.prototype.append = function(name, value) {
            this._params.push([name, String(value)]);
        };
        URLSearchParams.prototype['delete'] = function(name) {
            this._params = this._params.filter(function(p) { return p[0] !== name; });
        };
        URLSearchParams.prototype.toString = function() {
            return this._params.map(function(p) {
                return encodeURIComponent(p[0]) + '=' + encodeURIComponent(p[1]);
            }).join('&');
        };
        URLSearchParams.prototype.forEach = function(callback) {
            for (var i = 0; i < this._params.length; i++) {
                callback(this._params[i][1], this._params[i][0], this);
            }
        };
        URLSearchParams.prototype.entries = function() {
            var idx = 0;
            var params = this._params;
            return {
                next: function() {
                    if (idx < params.length) {
                        return { value: params[idx++], done: false };
                    }
                    return { value: undefined, done: true };
                }
            };
        };
        URLSearchParams.prototype.keys = function() {
            var idx = 0;
            var params = this._params;
            return {
                next: function() {
                    if (idx < params.length) {
                        return { value: params[idx++][0], done: false };
                    }
                    return { value: undefined, done: true };
                }
            };
        };
        URLSearchParams.prototype.values = function() {
            var idx = 0;
            var params = this._params;
            return {
                next: function() {
                    if (idx < params.length) {
                        return { value: params[idx++][1], done: false };
                    }
                    return { value: undefined, done: true };
                }
            };
        };
    }

    if (typeof URL === 'undefined') {
        globalThis.URL = function(url, base) {
            // Very basic URL parsing
            var full = url;
            if (base && !/^[a-zA-Z]+:\/\//.test(url)) {
                // Relative URL resolution (very simplified)
                if (url.startsWith('/')) {
                    var m = base.match(/^([a-zA-Z]+:\/\/[^\/]+)/);
                    full = m ? m[1] + url : url;
                } else {
                    full = base.replace(/[^\/]*$/, '') + url;
                }
            }

            this.href = full;
            this.origin = '';
            this.protocol = '';
            this.host = '';
            this.hostname = '';
            this.port = '';
            this.pathname = '/';
            this.search = '';
            this.hash = '';

            var match = full.match(/^([a-zA-Z]+:)\/\/([^\/:]+)(:\d+)?(\/[^?#]*)?(\\?[^#]*)?(#.*)?$/);
            if (match) {
                this.protocol = match[1] || '';
                this.hostname = match[2] || '';
                this.port = match[3] ? match[3].substring(1) : '';
                this.host = this.hostname + (this.port ? ':' + this.port : '');
                this.origin = this.protocol + '//' + this.host;
                this.pathname = match[4] || '/';
                this.search = match[5] || '';
                this.hash = match[6] || '';
            }

            this.searchParams = new URLSearchParams(this.search);
        };
        URL.prototype.toString = function() { return this.href; };
        URL.prototype.toJSON = function() { return this.href; };
        URL.createObjectURL = function(blob) { return 'blob:nova/' + Math.random().toString(36).substr(2); };
        URL.revokeObjectURL = function(url) {};
    }

    // ── requestAnimationFrame / cancelAnimationFrame ────────────────────────
    if (!globalThis.requestAnimationFrame) {
        var _rafId = 0;
        globalThis.requestAnimationFrame = function(callback) {
            return ++_rafId;
        };
        globalThis.cancelAnimationFrame = function(id) {};
    }

    // ── requestIdleCallback / cancelIdleCallback ────────────────────────────
    if (!globalThis.requestIdleCallback) {
        globalThis.requestIdleCallback = function(callback) {
            return globalThis.requestAnimationFrame(function() {
                callback({
                    didTimeout: false,
                    timeRemaining: function() { return 50; }
                });
            });
        };
        globalThis.cancelIdleCallback = function(id) {
            globalThis.cancelAnimationFrame(id);
        };
    }

    // ── Event constructor ───────────────────────────────────────────────────
    if (typeof Event === 'undefined') {
        globalThis.Event = function(type, options) {
            this.type = type;
            this.bubbles = (options && options.bubbles) || false;
            this.cancelable = (options && options.cancelable) || false;
            this.composed = (options && options.composed) || false;
            this.defaultPrevented = false;
            this.target = null;
            this.currentTarget = null;
            this.eventPhase = 0;
            this.timeStamp = Date.now();
        };
        Event.prototype.preventDefault = function() { this.defaultPrevented = true; };
        Event.prototype.stopPropagation = function() {};
        Event.prototype.stopImmediatePropagation = function() {};
    }

    // ── CustomEvent constructor ─────────────────────────────────────────────
    if (typeof CustomEvent === 'undefined') {
        globalThis.CustomEvent = function(type, options) {
            var evt = new Event(type, options);
            evt.detail = (options && options.detail) || null;
            return evt;
        };
    }

    // ── AbortController / AbortSignal ───────────────────────────────────────
    if (typeof AbortController === 'undefined') {
        globalThis.AbortSignal = function() {
            this.aborted = false;
            this.reason = undefined;
            this._listeners = [];
        };
        AbortSignal.prototype.addEventListener = function(type, callback) {
            if (type === 'abort') this._listeners.push(callback);
        };
        AbortSignal.prototype.removeEventListener = function(type, callback) {
            if (type === 'abort') {
                this._listeners = this._listeners.filter(function(cb) { return cb !== callback; });
            }
        };
        AbortSignal.prototype.throwIfAborted = function() {
            if (this.aborted) throw new Error('AbortError');
        };

        globalThis.AbortController = function() {
            this.signal = new AbortSignal();
        };
        AbortController.prototype.abort = function(reason) {
            this.signal.aborted = true;
            this.signal.reason = reason || new Error('AbortError');
            var listeners = this.signal._listeners.slice();
            for (var i = 0; i < listeners.length; i++) {
                listeners[i]({ type: 'abort' });
            }
        };
    }

    // ── TextEncoder / TextDecoder ───────────────────────────────────────────
    if (typeof TextEncoder === 'undefined') {
        globalThis.TextEncoder = function() {};
        TextEncoder.prototype.encode = function(str) {
            var arr = [];
            for (var i = 0; i < str.length; i++) {
                var code = str.charCodeAt(i);
                if (code < 0x80) {
                    arr.push(code);
                } else if (code < 0x800) {
                    arr.push(0xC0 | (code >> 6), 0x80 | (code & 0x3F));
                } else {
                    arr.push(0xE0 | (code >> 12), 0x80 | ((code >> 6) & 0x3F), 0x80 | (code & 0x3F));
                }
            }
            return new Uint8Array(arr);
        };
    }
    if (typeof TextDecoder === 'undefined') {
        globalThis.TextDecoder = function(encoding) {
            this.encoding = encoding || 'utf-8';
        };
        TextDecoder.prototype.decode = function(buffer) {
            var bytes = new Uint8Array(buffer);
            var out = '';
            var i = 0;
            while (i < bytes.length) {
                var b = bytes[i];
                if (b < 0x80) {
                    out += String.fromCharCode(b);
                    i++;
                } else if (b < 0xE0) {
                    out += String.fromCharCode(((b & 0x1F) << 6) | (bytes[i + 1] & 0x3F));
                    i += 2;
                } else {
                    out += String.fromCharCode(((b & 0x0F) << 12) | ((bytes[i + 1] & 0x3F) << 6) | (bytes[i + 2] & 0x3F));
                    i += 3;
                }
            }
            return out;
        };
    }

})();
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // Integration tests are in quickjs_runtime.rs.
}

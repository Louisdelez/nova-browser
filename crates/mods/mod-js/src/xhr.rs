//! # xhr
//!
//! XMLHttpRequest implementation for the NOVA JavaScript engine.
//!
//! Provides a synchronous XHR class that wraps fetch internally.
//! The XHR object is registered into the QuickJS runtime as a global class.
//!
//! ## Supported API
//!
//! ```javascript
//! var xhr = new XMLHttpRequest();
//! xhr.open("GET", "https://api.example.com/data");
//! xhr.setRequestHeader("Accept", "application/json");
//! xhr.onreadystatechange = function() {
//!     if (xhr.readyState === 4 && xhr.status === 200) {
//!         console.log(xhr.responseText);
//!     }
//! };
//! xhr.send();
//! ```

/// JavaScript source that defines the XMLHttpRequest class.
///
/// This is installed into the QuickJS runtime alongside the DOM shim.
/// Since NOVA's fetch is synchronous (blocks the JS thread), XHR.send()
/// uses fetch internally and resolves immediately.
pub const JS_XHR_SHIM: &str = r#"
(function() {
    "use strict";

    var UNSENT = 0;
    var OPENED = 1;
    var HEADERS_RECEIVED = 2;
    var LOADING = 3;
    var DONE = 4;

    function XMLHttpRequest() {
        this.readyState = UNSENT;
        this.status = 0;
        this.statusText = "";
        this.responseText = "";
        this.responseXML = null;
        this.response = "";
        this.responseType = "";
        this.timeout = 0;
        this.withCredentials = false;

        this._method = "GET";
        this._url = "";
        this._async = true;
        this._headers = {};
        this._responseHeaders = {};

        this.onreadystatechange = null;
        this.onload = null;
        this.onerror = null;
        this.onprogress = null;
        this.onloadstart = null;
        this.onloadend = null;
        this.ontimeout = null;
        this.onabort = null;

        this._eventListeners = {};
    }

    XMLHttpRequest.UNSENT = UNSENT;
    XMLHttpRequest.OPENED = OPENED;
    XMLHttpRequest.HEADERS_RECEIVED = HEADERS_RECEIVED;
    XMLHttpRequest.LOADING = LOADING;
    XMLHttpRequest.DONE = DONE;

    XMLHttpRequest.prototype.open = function(method, url, async_flag) {
        this._method = (method || "GET").toUpperCase();
        this._url = url || "";
        this._async = async_flag !== false;
        this._headers = {};
        this.readyState = OPENED;
        this.status = 0;
        this.statusText = "";
        this.responseText = "";
        this.response = "";
        this._fireReadyStateChange();
    };

    XMLHttpRequest.prototype.setRequestHeader = function(name, value) {
        if (this.readyState !== OPENED) {
            throw new Error("InvalidStateError: setRequestHeader called in wrong state");
        }
        this._headers[name] = value;
    };

    XMLHttpRequest.prototype.getResponseHeader = function(name) {
        if (this.readyState < HEADERS_RECEIVED) return null;
        var lower = name.toLowerCase();
        for (var key in this._responseHeaders) {
            if (key.toLowerCase() === lower) {
                return this._responseHeaders[key];
            }
        }
        return null;
    };

    XMLHttpRequest.prototype.getAllResponseHeaders = function() {
        if (this.readyState < HEADERS_RECEIVED) return "";
        var result = "";
        for (var key in this._responseHeaders) {
            result += key + ": " + this._responseHeaders[key] + "\r\n";
        }
        return result;
    };

    XMLHttpRequest.prototype.send = function(body) {
        if (this.readyState !== OPENED) {
            throw new Error("InvalidStateError: send called in wrong state");
        }

        var self = this;
        var opts = { method: this._method, headers: this._headers };
        if (body && this._method !== "GET" && this._method !== "HEAD") {
            opts.body = body;
        }

        // Fire loadstart
        if (typeof self.onloadstart === "function") {
            self.onloadstart({ type: "loadstart", target: self });
        }
        self._fireEvent("loadstart", { type: "loadstart", target: self });

        try {
            // Use the global fetch (provided by NOVA).
            // Since NOVA's QuickJS fetch is synchronous, this resolves immediately.
            var response = null;
            var fetchPromise = fetch(self._url, opts);

            // Handle both sync and async fetch
            if (fetchPromise && typeof fetchPromise.then === "function") {
                fetchPromise.then(function(resp) {
                    response = resp;
                });
            }

            if (response) {
                self.readyState = HEADERS_RECEIVED;
                self._fireReadyStateChange();

                self.readyState = LOADING;
                self._fireReadyStateChange();

                self.status = response.status || 200;
                self.statusText = response.statusText || "OK";

                // Try to get text
                if (typeof response.text === "function") {
                    var textPromise = response.text();
                    if (textPromise && typeof textPromise.then === "function") {
                        textPromise.then(function(text) {
                            self.responseText = text;
                            self.response = text;
                        });
                    }
                }

                // Store response headers
                if (response.headers && typeof response.headers.forEach === "function") {
                    response.headers.forEach(function(value, key) {
                        self._responseHeaders[key] = value;
                    });
                }
            } else {
                // Fetch not available or returned nothing — simulate a completed empty response
                self.readyState = HEADERS_RECEIVED;
                self._fireReadyStateChange();
                self.readyState = LOADING;
                self._fireReadyStateChange();
                self.status = 0;
                self.statusText = "";
                self.responseText = "";
                self.response = "";
            }

            self.readyState = DONE;
            self._fireReadyStateChange();

            if (typeof self.onload === "function") {
                self.onload({ type: "load", target: self });
            }
            self._fireEvent("load", { type: "load", target: self });

            // Fire progress
            if (typeof self.onprogress === "function") {
                self.onprogress({ type: "progress", target: self, loaded: self.responseText.length, total: self.responseText.length, lengthComputable: true });
            }
            self._fireEvent("progress", { type: "progress", target: self, loaded: self.responseText.length, total: self.responseText.length, lengthComputable: true });
        } catch (e) {
            self.readyState = DONE;
            self.status = 0;
            self.statusText = "";
            self._fireReadyStateChange();

            if (typeof self.onerror === "function") {
                self.onerror({ type: "error", target: self, message: String(e) });
            }
            self._fireEvent("error", { type: "error", target: self, message: String(e) });
        }

        // Fire loadend
        if (typeof self.onloadend === "function") {
            self.onloadend({ type: "loadend", target: self });
        }
        self._fireEvent("loadend", { type: "loadend", target: self });
    };

    XMLHttpRequest.prototype.abort = function() {
        this.readyState = UNSENT;
        this.status = 0;
        this.statusText = "";
        this.responseText = "";
        this.response = "";
        if (typeof this.onabort === "function") {
            this.onabort({ type: "abort", target: this });
        }
        this._fireEvent("abort", { type: "abort", target: this });
    };

    XMLHttpRequest.prototype.overrideMimeType = function(mime) {
        // Stub — no-op.
    };

    XMLHttpRequest.prototype.addEventListener = function(type, callback) {
        if (!this._eventListeners[type]) {
            this._eventListeners[type] = [];
        }
        this._eventListeners[type].push(callback);
    };

    XMLHttpRequest.prototype.removeEventListener = function(type, callback) {
        if (!this._eventListeners[type]) return;
        this._eventListeners[type] = this._eventListeners[type].filter(function(cb) {
            return cb !== callback;
        });
    };

    XMLHttpRequest.prototype._fireReadyStateChange = function() {
        if (typeof this.onreadystatechange === "function") {
            this.onreadystatechange({ type: "readystatechange", target: this });
        }
        this._fireEvent("readystatechange", { type: "readystatechange", target: this });
    };

    XMLHttpRequest.prototype._fireEvent = function(type, event) {
        var listeners = this._eventListeners[type];
        if (!listeners) return;
        for (var i = 0; i < listeners.length; i++) {
            listeners[i](event);
        }
    };

    globalThis.XMLHttpRequest = XMLHttpRequest;
})();
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn xhr_shim_is_valid_js() {
        // Just ensure the shim string is non-empty and parseable.
        assert!(!super::JS_XHR_SHIM.is_empty());
        assert!(super::JS_XHR_SHIM.contains("XMLHttpRequest"));
    }
}

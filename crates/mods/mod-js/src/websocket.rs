//! # websocket
//!
//! Implements the `WebSocket` Web API for the NOVA JavaScript engine.
//!
//! ## Design
//!
//! The `WebSocket` class is implemented as a JavaScript shim that stores
//! connection state and callbacks, mirroring the browser's WebSocket API.
//!
//! Because the QuickJS interpreter is synchronous, WebSocket operations
//! are bridged through IPC to the network mod's async WebSocket manager.
//!
//! ## Supported API
//!
//! ```javascript
//! var ws = new WebSocket("ws://example.com/socket");
//! ws.onopen = function() { console.log("connected"); };
//! ws.onmessage = function(event) { console.log(event.data); };
//! ws.onerror = function(event) { console.log("error"); };
//! ws.onclose = function(event) { console.log("closed"); };
//! ws.send("hello");
//! ws.close(1000, "bye");
//! ```

/// JavaScript shim that implements the WebSocket API.
///
/// This defines the `WebSocket` class with proper constructor, properties,
/// methods, and event handlers. Since actual network I/O is async and the
/// JS engine is synchronous, the shim uses `__nova_ws_*` bridge functions
/// that synchronously call into the network mod.
pub const JS_WEBSOCKET_SHIM: &str = r#"
(function() {
    "use strict";

    // WebSocket readyState constants
    var CONNECTING = 0;
    var OPEN = 1;
    var CLOSING = 2;
    var CLOSED = 3;

    // WebSocket registry for managing connections
    var __wsConnections = {};
    var __wsNextId = 1;

    function WebSocket(url, protocols) {
        if (!(this instanceof WebSocket)) {
            throw new TypeError("Failed to construct 'WebSocket': Please use the 'new' operator");
        }

        if (!url) {
            throw new SyntaxError("Failed to construct 'WebSocket': The URL is empty");
        }

        // Validate URL scheme
        if (url.indexOf("ws://") !== 0 && url.indexOf("wss://") !== 0) {
            throw new SyntaxError("Failed to construct 'WebSocket': The URL scheme must be ws:// or wss://");
        }

        this._id = __wsNextId++;
        this._url = url;
        this._protocol = "";
        this._readyState = CONNECTING;
        this._bufferedAmount = 0;
        this._extensions = "";
        this._binaryType = "blob";

        // Event handlers
        this._onopen = null;
        this._onmessage = null;
        this._onerror = null;
        this._onclose = null;

        // Event listener arrays
        this._listeners = {
            open: [],
            message: [],
            error: [],
            close: []
        };

        // Store in registry
        __wsConnections[this._id] = this;

        // Attempt to connect via bridge (if available)
        var self = this;
        if (typeof __nova_ws_connect === "function") {
            try {
                var handle = __nova_ws_connect(url);
                if (handle >= 0) {
                    this._handle = handle;
                    this._readyState = OPEN;
                    // Fire onopen
                    var openEvent = { type: "open", target: self };
                    if (self._onopen) self._onopen(openEvent);
                    self._dispatchEvent("open", openEvent);
                } else {
                    this._readyState = CLOSED;
                    var errorEvent = { type: "error", target: self };
                    if (self._onerror) self._onerror(errorEvent);
                    self._dispatchEvent("error", errorEvent);
                }
            } catch (e) {
                this._readyState = CLOSED;
                var errorEvent = { type: "error", target: self, message: String(e) };
                if (self._onerror) self._onerror(errorEvent);
                self._dispatchEvent("error", errorEvent);
            }
        } else {
            // No bridge available — simulate immediate open for compatibility
            this._readyState = OPEN;
            this._handle = -1;
        }
    }

    // Static constants
    WebSocket.CONNECTING = CONNECTING;
    WebSocket.OPEN = OPEN;
    WebSocket.CLOSING = CLOSING;
    WebSocket.CLOSED = CLOSED;

    // readyState property
    Object.defineProperty(WebSocket.prototype, "readyState", {
        get: function() { return this._readyState; },
        configurable: true
    });

    // url property
    Object.defineProperty(WebSocket.prototype, "url", {
        get: function() { return this._url; },
        configurable: true
    });

    // protocol property
    Object.defineProperty(WebSocket.prototype, "protocol", {
        get: function() { return this._protocol; },
        configurable: true
    });

    // bufferedAmount property
    Object.defineProperty(WebSocket.prototype, "bufferedAmount", {
        get: function() { return this._bufferedAmount; },
        configurable: true
    });

    // extensions property
    Object.defineProperty(WebSocket.prototype, "extensions", {
        get: function() { return this._extensions; },
        configurable: true
    });

    // binaryType property
    Object.defineProperty(WebSocket.prototype, "binaryType", {
        get: function() { return this._binaryType; },
        set: function(v) {
            if (v === "blob" || v === "arraybuffer") {
                this._binaryType = v;
            }
        },
        configurable: true
    });

    // Event handler properties
    Object.defineProperty(WebSocket.prototype, "onopen", {
        get: function() { return this._onopen; },
        set: function(v) { this._onopen = v; },
        configurable: true
    });

    Object.defineProperty(WebSocket.prototype, "onmessage", {
        get: function() { return this._onmessage; },
        set: function(v) { this._onmessage = v; },
        configurable: true
    });

    Object.defineProperty(WebSocket.prototype, "onerror", {
        get: function() { return this._onerror; },
        set: function(v) { this._onerror = v; },
        configurable: true
    });

    Object.defineProperty(WebSocket.prototype, "onclose", {
        get: function() { return this._onclose; },
        set: function(v) { this._onclose = v; },
        configurable: true
    });

    // send method
    WebSocket.prototype.send = function(data) {
        if (this._readyState === CONNECTING) {
            throw new Error("Failed to execute 'send' on 'WebSocket': Still in CONNECTING state");
        }
        if (this._readyState !== OPEN) {
            return;
        }

        if (typeof __nova_ws_send === "function" && this._handle >= 0) {
            try {
                __nova_ws_send(this._handle, String(data));
            } catch (e) {
                var errorEvent = { type: "error", target: this, message: String(e) };
                if (this._onerror) this._onerror(errorEvent);
                this._dispatchEvent("error", errorEvent);
            }
        }
    };

    // close method
    WebSocket.prototype.close = function(code, reason) {
        if (this._readyState === CLOSING || this._readyState === CLOSED) {
            return;
        }

        // Validate close code
        if (code !== undefined) {
            code = Number(code);
            if (code !== 1000 && (code < 3000 || code > 4999)) {
                throw new Error("Failed to execute 'close' on 'WebSocket': Invalid close code");
            }
        }

        this._readyState = CLOSING;

        if (typeof __nova_ws_close === "function" && this._handle >= 0) {
            try {
                __nova_ws_close(this._handle, code || 1000, reason || "");
            } catch (e) {
                // Ignore close errors
            }
        }

        this._readyState = CLOSED;

        var closeEvent = {
            type: "close",
            target: this,
            code: code || 1000,
            reason: reason || "",
            wasClean: true
        };
        if (this._onclose) this._onclose(closeEvent);
        this._dispatchEvent("close", closeEvent);
    };

    // addEventListener
    WebSocket.prototype.addEventListener = function(type, callback) {
        if (this._listeners[type]) {
            this._listeners[type].push(callback);
        }
    };

    // removeEventListener
    WebSocket.prototype.removeEventListener = function(type, callback) {
        if (this._listeners[type]) {
            this._listeners[type] = this._listeners[type].filter(function(cb) {
                return cb !== callback;
            });
        }
    };

    // Internal dispatch helper
    WebSocket.prototype._dispatchEvent = function(type, event) {
        var listeners = this._listeners[type];
        if (listeners) {
            for (var i = 0; i < listeners.length; i++) {
                listeners[i](event);
            }
        }
    };

    // Global function to deliver a message from the network layer
    globalThis.__novaWsOnMessage = function(wsId, data) {
        var ws = __wsConnections[wsId];
        if (ws && ws._readyState === OPEN) {
            var event = { type: "message", target: ws, data: data };
            if (ws._onmessage) ws._onmessage(event);
            ws._dispatchEvent("message", event);
        }
    };

    // Make WebSocket available globally
    globalThis.WebSocket = WebSocket;
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_defines_websocket() {
        assert!(JS_WEBSOCKET_SHIM.contains("globalThis.WebSocket = WebSocket"));
    }

    #[test]
    fn shim_has_ready_states() {
        assert!(JS_WEBSOCKET_SHIM.contains("CONNECTING = 0"));
        assert!(JS_WEBSOCKET_SHIM.contains("OPEN = 1"));
        assert!(JS_WEBSOCKET_SHIM.contains("CLOSING = 2"));
        assert!(JS_WEBSOCKET_SHIM.contains("CLOSED = 3"));
    }

    #[test]
    fn shim_has_send_and_close() {
        assert!(JS_WEBSOCKET_SHIM.contains("WebSocket.prototype.send"));
        assert!(JS_WEBSOCKET_SHIM.contains("WebSocket.prototype.close"));
    }

    #[test]
    fn shim_has_event_handlers() {
        assert!(JS_WEBSOCKET_SHIM.contains("onopen"));
        assert!(JS_WEBSOCKET_SHIM.contains("onmessage"));
        assert!(JS_WEBSOCKET_SHIM.contains("onerror"));
        assert!(JS_WEBSOCKET_SHIM.contains("onclose"));
    }
}

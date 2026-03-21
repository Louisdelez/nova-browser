//! # service_worker
//!
//! Service Worker stubs for the NOVA JavaScript engine.
//!
//! Most modern sites check for `navigator.serviceWorker` support. These stubs
//! prevent crashes by providing no-op implementations that return resolved
//! Promises.

/// JavaScript shim that installs `navigator.serviceWorker` stubs.
///
/// The stub provides `register()`, `ready`, `controller`, and event listener
/// methods so that sites relying on Service Worker feature detection do not
/// throw errors.
pub const JS_SERVICE_WORKER_SHIM: &str = r#"
(function() {
    "use strict";

    // navigator.serviceWorker stub
    if (typeof navigator !== 'undefined' && !navigator.serviceWorker) {
        navigator.serviceWorker = {
            register: function(url) {
                return Promise.resolve({
                    scope: '/',
                    active: null,
                    installing: null,
                    waiting: null,
                    addEventListener: function() {},
                    removeEventListener: function() {},
                    unregister: function() { return Promise.resolve(true); },
                    update: function() { return Promise.resolve(); }
                });
            },
            ready: Promise.resolve({ active: null }),
            controller: null,
            addEventListener: function() {},
            removeEventListener: function() {},
            getRegistrations: function() { return Promise.resolve([]); },
            getRegistration: function() { return Promise.resolve(undefined); }
        };
    }
})();
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // Integration tests are in quickjs_runtime.rs.
}

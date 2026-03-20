//! # timers
//!
//! JavaScript timer API implementation: `setTimeout`, `setInterval`,
//! `requestAnimationFrame`, and their cancellation counterparts.
//!
//! Since QuickJS executes synchronously, timers are implemented as a JS-side
//! registry. A Rust-side `tick_timers()` function evaluates a JS snippet that
//! checks all pending timers and fires those whose delay has elapsed.

use tracing::debug;

/// JavaScript shim code that registers the timer globals.
///
/// This is injected into the QuickJS context during initialization. It sets up
/// `setTimeout`, `clearTimeout`, `setInterval`, `clearInterval`,
/// `requestAnimationFrame`, and `cancelAnimationFrame`.
pub const JS_TIMER_SHIM: &str = r#"
var __timers = { next: 1, timers: {}, intervals: {} };

function setTimeout(fn, ms) {
    var id = __timers.next++;
    __timers.timers[id] = { fn: fn, ms: ms || 0, registered: Date.now() };
    return id;
}
function clearTimeout(id) { delete __timers.timers[id]; }

function setInterval(fn, ms) {
    var id = __timers.next++;
    __timers.intervals[id] = { fn: fn, ms: ms || 0, lastRun: Date.now() };
    return id;
}
function clearInterval(id) { delete __timers.intervals[id]; }

function requestAnimationFrame(fn) { return setTimeout(fn, 16); }
function cancelAnimationFrame(id) { clearTimeout(id); }
"#;

/// JavaScript code evaluated by `tick_timers()` to fire ready timers.
///
/// Iterates all pending `setTimeout` entries and fires those whose delay has
/// elapsed (then removes them). For `setInterval`, fires and updates `lastRun`.
const JS_TICK_TIMERS: &str = r#"
(function() {
    var now = Date.now();
    var keys;
    keys = Object.keys(__timers.timers);
    for (var i = 0; i < keys.length; i++) {
        var id = keys[i];
        var t = __timers.timers[id];
        if (t && now - t.registered >= t.ms) {
            try { t.fn(); } catch(e) {}
            delete __timers.timers[id];
        }
    }
    keys = Object.keys(__timers.intervals);
    for (var i = 0; i < keys.length; i++) {
        var id = keys[i];
        var t = __timers.intervals[id];
        if (t && now - t.lastRun >= t.ms) {
            try { t.fn(); } catch(e) {}
            t.lastRun = now;
        }
    }
})();
"#;

/// Returns the JS source that, when evaluated, ticks all pending timers.
///
/// Call this from the main loop or after script execution to drain ready
/// `setTimeout` / `setInterval` callbacks.
pub fn tick_timers_source() -> &'static str {
    JS_TICK_TIMERS
}

/// Returns `true` if there are any pending timers in the given JS source result.
///
/// This is a heuristic used by the shell to decide whether to keep ticking.
pub fn has_pending_timers_source() -> &'static str {
    r#"(Object.keys(__timers.timers).length + Object.keys(__timers.intervals).length)"#
}

/// Information about timer state, used for diagnostics.
#[derive(Debug, Clone)]
pub struct TimerStats {
    /// Number of pending `setTimeout` callbacks.
    pub pending_timeouts: usize,
    /// Number of active `setInterval` callbacks.
    pub active_intervals: usize,
}

impl TimerStats {
    /// Create empty stats.
    pub fn empty() -> Self {
        Self {
            pending_timeouts: 0,
            active_intervals: 0,
        }
    }
}

/// Log timer tick at trace level.
pub fn log_tick() {
    debug!("ticking JS timers");
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_shim_is_valid_js() {
        // Ensure the shim contains the expected function definitions.
        assert!(JS_TIMER_SHIM.contains("function setTimeout"));
        assert!(JS_TIMER_SHIM.contains("function clearTimeout"));
        assert!(JS_TIMER_SHIM.contains("function setInterval"));
        assert!(JS_TIMER_SHIM.contains("function clearInterval"));
        assert!(JS_TIMER_SHIM.contains("function requestAnimationFrame"));
        assert!(JS_TIMER_SHIM.contains("function cancelAnimationFrame"));
    }

    #[test]
    fn tick_source_is_nonempty() {
        let src = tick_timers_source();
        assert!(!src.is_empty());
        assert!(src.contains("__timers"));
    }

    #[test]
    fn has_pending_source_is_nonempty() {
        let src = has_pending_timers_source();
        assert!(!src.is_empty());
    }

    #[test]
    fn timer_stats_empty() {
        let stats = TimerStats::empty();
        assert_eq!(stats.pending_timeouts, 0);
        assert_eq!(stats.active_intervals, 0);
    }
}

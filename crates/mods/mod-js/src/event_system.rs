//! # event_system
//!
//! Proper DOM event dispatching with capture, target, and bubble phases.
//!
//! Implements the W3C DOM Events model: events travel from the root down to
//! the target (capture phase), fire at the target, then bubble back up.

use tracing::debug;

use crate::dom_api::{ElementHandle, JsDomTree};

// ── Event phase ──────────────────────────────────────────────────────────────

/// The current phase of event dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPhase {
    /// Not currently dispatching.
    None,
    /// Travelling from root toward the target.
    Capturing,
    /// At the event target itself.
    AtTarget,
    /// Bubbling back from target toward root.
    Bubbling,
}

// ── Listener options ─────────────────────────────────────────────────────────

/// Options that modify listener behaviour, mirroring `AddEventListenerOptions`.
#[derive(Debug, Clone)]
pub struct ListenerOptions {
    /// If `true`, the listener fires during the capture phase.
    pub capture: bool,
    /// If `true`, the listener is automatically removed after its first invocation.
    pub once: bool,
    /// If `true`, `preventDefault()` will be ignored for this listener.
    pub passive: bool,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            capture: false,
            once: false,
            passive: false,
        }
    }
}

// ── DomEvent ─────────────────────────────────────────────────────────────────

/// A DOM event with propagation control flags.
#[derive(Debug, Clone)]
pub struct DomEvent {
    /// The event type, e.g. `"click"`, `"keydown"`.
    pub event_type: String,
    /// The element handle where the event originated.
    pub target: ElementHandle,
    /// The element whose listener is currently being invoked.
    pub current_target: ElementHandle,
    /// Whether the event bubbles up through the DOM tree.
    pub bubbles: bool,
    /// Whether `preventDefault()` is allowed.
    pub cancelable: bool,
    /// Set to `true` when `preventDefault()` has been called.
    pub default_prevented: bool,
    /// Set to `true` when `stopPropagation()` has been called.
    pub propagation_stopped: bool,
    /// Set to `true` when `stopImmediatePropagation()` has been called.
    pub immediate_propagation_stopped: bool,
    /// Current dispatch phase.
    pub phase: EventPhase,
    /// High-resolution timestamp (milliseconds since epoch).
    pub timestamp: f64,
}

impl DomEvent {
    /// Create a new event with the given type and target.
    pub fn new(event_type: &str, target: ElementHandle) -> Self {
        Self {
            event_type: event_type.to_owned(),
            target,
            current_target: target,
            bubbles: true,
            cancelable: true,
            default_prevented: false,
            propagation_stopped: false,
            immediate_propagation_stopped: false,
            phase: EventPhase::None,
            timestamp: 0.0,
        }
    }

    /// Call `preventDefault()`.
    pub fn prevent_default(&mut self) {
        if self.cancelable {
            self.default_prevented = true;
        }
    }

    /// Call `stopPropagation()`.
    pub fn stop_propagation(&mut self) {
        self.propagation_stopped = true;
    }

    /// Call `stopImmediatePropagation()`.
    pub fn stop_immediate_propagation(&mut self) {
        self.propagation_stopped = true;
        self.immediate_propagation_stopped = true;
    }
}

// ── CustomEvent ──────────────────────────────────────────────────────────────

/// A custom event carrying an arbitrary `detail` payload.
#[derive(Debug, Clone)]
pub struct CustomEvent {
    /// The base event data.
    pub event: DomEvent,
    /// Application-specific detail payload (as a string for simplicity).
    pub detail: Option<String>,
}

impl CustomEvent {
    /// Create a new `CustomEvent`.
    pub fn new(event_type: &str, target: ElementHandle, detail: Option<String>) -> Self {
        Self {
            event: DomEvent::new(event_type, target),
            detail,
        }
    }
}

// ── Callback result ──────────────────────────────────────────────────────────

/// The result of invoking a single event callback.
#[derive(Debug, Clone)]
pub struct CallbackResult {
    /// The callback source that was executed.
    pub source: String,
    /// The captured environment for the callback.
    pub captured_env: Vec<(String, ElementHandle)>,
    /// The phase during which the callback was invoked.
    pub phase: EventPhase,
    /// Whether stopPropagation was called during this callback.
    pub propagation_stopped: bool,
}

// ── EventDispatcher ──────────────────────────────────────────────────────────

/// Dispatches DOM events through the capture → target → bubble pipeline.
pub struct EventDispatcher;

impl EventDispatcher {
    /// Dispatch an event through the full capture → target → bubble path.
    ///
    /// Returns a list of `CallbackResult`s for each listener that fired.
    pub fn dispatch(
        tree: &mut JsDomTree,
        event: &mut DomEvent,
    ) -> Vec<CallbackResult> {
        let mut results = Vec::new();

        // 1. Build the event path: from target up to root.
        let path = tree.build_ancestor_path(event.target);
        debug!(
            target = event.target,
            event_type = %event.event_type,
            path_len = path.len(),
            "dispatching event"
        );

        // 2. Capture phase: iterate from root toward target (skip target itself).
        event.phase = EventPhase::Capturing;
        for &ancestor in path.iter().rev() {
            if ancestor == event.target {
                continue;
            }
            if event.propagation_stopped {
                break;
            }
            event.current_target = ancestor;
            Self::fire_listeners(tree, event, ancestor, true, &mut results);
        }

        // 3. At target: fire all listeners regardless of capture flag.
        if !event.propagation_stopped {
            event.phase = EventPhase::AtTarget;
            event.current_target = event.target;
            Self::fire_listeners(tree, event, event.target, false, &mut results);
        }

        // 4. Bubble phase: iterate from target toward root (skip target itself).
        if event.bubbles && !event.propagation_stopped {
            event.phase = EventPhase::Bubbling;
            for &ancestor in path.iter().rev() {
                if ancestor == event.target {
                    continue;
                }
                // Walk from parent up to root in path order (path is target→root,
                // reversed above was root→target; for bubbling we want parent→root).
            }
            // Re-iterate in correct bubbling order: parent to root.
            for &ancestor in &path {
                if ancestor == event.target {
                    continue;
                }
                if event.propagation_stopped {
                    break;
                }
                event.current_target = ancestor;
                Self::fire_listeners(tree, event, ancestor, false, &mut results);
            }
        }

        event.phase = EventPhase::None;
        results
    }

    /// Fire all matching listeners on a given element.
    ///
    /// When `capture_only` is `true`, only listeners registered with `capture: true`
    /// are fired. When `false` (at-target or bubble), listeners registered with
    /// `capture: false` are fired. At the target, both kinds fire.
    fn fire_listeners(
        tree: &JsDomTree,
        event: &mut DomEvent,
        handle: ElementHandle,
        capture_only: bool,
        results: &mut Vec<CallbackResult>,
    ) {
        let listeners = tree.get_listeners(handle, &event.event_type);
        for cb in listeners {
            if event.immediate_propagation_stopped {
                break;
            }
            // At target, fire all listeners. During capture/bubble, match phase.
            if event.phase != EventPhase::AtTarget {
                if capture_only && !cb.capture {
                    continue;
                }
                if !capture_only && cb.capture {
                    continue;
                }
            }

            results.push(CallbackResult {
                source: cb.source.clone(),
                captured_env: cb.captured_env.clone(),
                phase: event.phase,
                propagation_stopped: event.propagation_stopped,
            });
        }
    }
}

/// JavaScript shim code for the Event and CustomEvent constructors.
pub const JS_EVENT_SHIM: &str = r#"
function Event(type, options) {
    this.type = type;
    this.bubbles = (options && options.bubbles) || false;
    this.cancelable = (options && options.cancelable) || false;
    this.defaultPrevented = false;
    this._stopped = false;
    this._immediateStopped = false;
}
Event.prototype.preventDefault = function() {
    if (this.cancelable) this.defaultPrevented = true;
};
Event.prototype.stopPropagation = function() { this._stopped = true; };
Event.prototype.stopImmediatePropagation = function() {
    this._stopped = true;
    this._immediateStopped = true;
};

function CustomEvent(type, options) {
    Event.call(this, type, options);
    this.detail = (options && options.detail) || null;
}
CustomEvent.prototype = Object.create(Event.prototype);
CustomEvent.prototype.constructor = CustomEvent;
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::content::DomNode;

    /// Helper: create a simple nested DOM:
    /// document > html > body > div#parent > span#child
    fn make_nested_dom() -> DomNode {
        DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "body".into(),
                    attributes: vec![],
                    children: vec![DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![("id".into(), "parent".into())],
                        children: vec![DomNode::Element {
                            tag: "span".into(),
                            attributes: vec![("id".into(), "child".into())],
                            children: vec![DomNode::Text("hello".into())],
                        }],
                    }],
                }],
            }],
        }
    }

    #[test]
    fn event_creation() {
        let evt = DomEvent::new("click", 5);
        assert_eq!(evt.event_type, "click");
        assert_eq!(evt.target, 5);
        assert!(evt.bubbles);
        assert!(evt.cancelable);
        assert!(!evt.default_prevented);
        assert!(!evt.propagation_stopped);
        assert_eq!(evt.phase, EventPhase::None);
    }

    #[test]
    fn prevent_default() {
        let mut evt = DomEvent::new("click", 1);
        evt.prevent_default();
        assert!(evt.default_prevented);

        // Non-cancelable event ignores preventDefault.
        let mut evt2 = DomEvent::new("scroll", 1);
        evt2.cancelable = false;
        evt2.prevent_default();
        assert!(!evt2.default_prevented);
    }

    #[test]
    fn stop_propagation_flags() {
        let mut evt = DomEvent::new("click", 1);
        assert!(!evt.propagation_stopped);
        evt.stop_propagation();
        assert!(evt.propagation_stopped);
        assert!(!evt.immediate_propagation_stopped);

        let mut evt2 = DomEvent::new("click", 1);
        evt2.stop_immediate_propagation();
        assert!(evt2.propagation_stopped);
        assert!(evt2.immediate_propagation_stopped);
    }

    #[test]
    fn custom_event_with_detail() {
        let ce = CustomEvent::new("myevent", 3, Some("payload".into()));
        assert_eq!(ce.event.event_type, "myevent");
        assert_eq!(ce.detail, Some("payload".into()));
    }

    #[test]
    fn bubbling_path() {
        let tree_arc = JsDomTree::from_dom(&make_nested_dom());
        let mut tree = tree_arc.lock().unwrap();

        let child_handle = tree.get_element_by_id("child").unwrap();
        let path = tree.build_ancestor_path(child_handle);
        // Path should be: [child, parent_div, body, html, document_root]
        // but exclude child itself from ancestors? Let's check:
        // build_ancestor_path includes the node itself.
        assert!(path.len() >= 4, "path should include child and ancestors, got {}", path.len());
        assert_eq!(path[0], child_handle, "first in path should be target");
    }

    #[test]
    fn dispatch_at_target_only() {
        let tree_arc = JsDomTree::from_dom(&make_nested_dom());
        let child_handle;
        {
            let mut tree = tree_arc.lock().unwrap();
            child_handle = tree.get_element_by_id("child").unwrap();
            tree.add_event_listener(child_handle, "click", "handler()", vec![], false);
        }

        let mut tree = tree_arc.lock().unwrap();
        let mut event = DomEvent::new("click", child_handle);
        let results = EventDispatcher::dispatch(&mut tree, &mut event);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "handler()");
        assert_eq!(results[0].phase, EventPhase::AtTarget);
    }

    #[test]
    fn dispatch_with_capture_listener() {
        let tree_arc = JsDomTree::from_dom(&make_nested_dom());
        let (child_handle, parent_handle);
        {
            let mut tree = tree_arc.lock().unwrap();
            child_handle = tree.get_element_by_id("child").unwrap();
            parent_handle = tree.get_element_by_id("parent").unwrap();
            // Register a capture listener on the parent.
            tree.add_event_listener(parent_handle, "click", "capture_handler()", vec![], true);
            // Register a bubble listener on the child.
            tree.add_event_listener(child_handle, "click", "target_handler()", vec![], false);
        }

        let mut tree = tree_arc.lock().unwrap();
        let mut event = DomEvent::new("click", child_handle);
        let results = EventDispatcher::dispatch(&mut tree, &mut event);
        // Capture fires first, then at-target.
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source, "capture_handler()");
        assert_eq!(results[0].phase, EventPhase::Capturing);
        assert_eq!(results[1].source, "target_handler()");
        assert_eq!(results[1].phase, EventPhase::AtTarget);
    }

    #[test]
    fn stop_propagation_prevents_bubbling() {
        let tree_arc = JsDomTree::from_dom(&make_nested_dom());
        let (child_handle, parent_handle);
        {
            let mut tree = tree_arc.lock().unwrap();
            child_handle = tree.get_element_by_id("child").unwrap();
            parent_handle = tree.get_element_by_id("parent").unwrap();
            tree.add_event_listener(child_handle, "click", "child_handler()", vec![], false);
            tree.add_event_listener(parent_handle, "click", "parent_handler()", vec![], false);
        }

        let mut tree = tree_arc.lock().unwrap();
        let mut event = DomEvent::new("click", child_handle);
        // Simulate stopPropagation at the target.
        // We can't do this mid-dispatch without a real JS engine,
        // so we pre-set the flag after the first result.
        let results = EventDispatcher::dispatch(&mut tree, &mut event);
        // Both should fire since we didn't stop propagation.
        assert_eq!(results.len(), 2);

        // Now test with propagation already stopped.
        let mut event2 = DomEvent::new("click", child_handle);
        event2.propagation_stopped = true;
        let results2 = EventDispatcher::dispatch(&mut tree, &mut event2);
        assert_eq!(results2.len(), 0);
    }

    #[test]
    fn non_bubbling_event() {
        let tree_arc = JsDomTree::from_dom(&make_nested_dom());
        let (child_handle, parent_handle);
        {
            let mut tree = tree_arc.lock().unwrap();
            child_handle = tree.get_element_by_id("child").unwrap();
            parent_handle = tree.get_element_by_id("parent").unwrap();
            tree.add_event_listener(child_handle, "focus", "child_focus()", vec![], false);
            tree.add_event_listener(parent_handle, "focus", "parent_focus()", vec![], false);
        }

        let mut tree = tree_arc.lock().unwrap();
        let mut event = DomEvent::new("focus", child_handle);
        event.bubbles = false;
        let results = EventDispatcher::dispatch(&mut tree, &mut event);
        // Only the target handler should fire (no bubbling).
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "child_focus()");
    }
}

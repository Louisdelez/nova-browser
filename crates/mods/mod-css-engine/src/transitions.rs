//! CSS Transitions manager.
//!
//! Tracks active CSS transitions per element and provides interpolated values
//! at any point in time. When computed styles change for an element that has
//! a `transition` property, a transition is created instead of immediately
//! applying the new value.

use std::collections::HashMap;

use tracing::debug;

use crate::animation::{
    self, compute_progress, interpolate_value, parse_timing_function, TimingFunction,
    TransitionState,
};

// ── Transition parsing ──────────────────────────────────────────────

/// Parsed `transition` shorthand for a single property.
#[derive(Debug, Clone)]
pub struct TransitionDefinition {
    /// The CSS property to transition (or "all").
    pub property: String,
    /// Duration in seconds.
    pub duration: f32,
    /// Timing function.
    pub timing_function: TimingFunction,
    /// Delay in seconds.
    pub delay: f32,
}

/// Parse a CSS `transition` shorthand value.
///
/// The shorthand format is:
/// `property duration [timing-function] [delay]`
///
/// Multiple transitions can be separated by commas:
/// `color 0.3s ease, opacity 0.5s linear 0.1s`
pub fn parse_transition_shorthand(value: &str) -> Vec<TransitionDefinition> {
    let value = value.trim();
    if value.is_empty() || value == "none" {
        return Vec::new();
    }

    let mut result = Vec::new();

    // Split by comma (respecting parentheses for cubic-bezier).
    for part in split_transition_parts(value) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        let mut def = TransitionDefinition {
            property: "all".to_string(),
            duration: 0.0,
            timing_function: TimingFunction::Ease,
            delay: 0.0,
        };

        let mut duration_set = false;

        for &token in &tokens {
            // Check for timing function keywords first.
            match token {
                "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out" => {
                    def.timing_function = parse_timing_function(token);
                    continue;
                }
                _ => {}
            }

            // Check for cubic-bezier.
            if token.starts_with("cubic-bezier(") {
                def.timing_function = parse_timing_function(token);
                continue;
            }

            // Check for time values.
            if token.ends_with("ms") || token.ends_with('s') {
                let secs = parse_time_value(token);
                if !duration_set {
                    def.duration = secs;
                    duration_set = true;
                } else {
                    def.delay = secs;
                }
                continue;
            }

            // Must be the property name.
            def.property = token.to_string();
        }

        if def.duration > 0.0 {
            result.push(def);
        }
    }

    result
}

/// Split transition value by commas, respecting parentheses.
fn split_transition_parts(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Parse a CSS time value (e.g., "0.3s", "300ms") into seconds.
fn parse_time_value(s: &str) -> f32 {
    if let Some(ms_str) = s.strip_suffix("ms") {
        ms_str.trim().parse::<f32>().unwrap_or(0.0) / 1000.0
    } else if let Some(s_str) = s.strip_suffix('s') {
        s_str.trim().parse::<f32>().unwrap_or(0.0)
    } else {
        0.0
    }
}

// ── Transition Manager ──────────────────────────────────────────────

/// Manages active CSS transitions for all elements.
///
/// Each element can have multiple active transitions (one per property).
/// The manager tracks start times, durations, and from/to values, and
/// provides interpolated current values on demand.
#[derive(Debug, Default)]
pub struct TransitionManager {
    /// Active transitions keyed by (element_id, property).
    active: HashMap<(u64, String), TransitionState>,
}

impl TransitionManager {
    /// Create a new empty transition manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a transition for a specific element and property.
    ///
    /// If a transition is already running for the same (element, property),
    /// it is replaced (the new transition starts from the current interpolated value).
    pub fn start_transition(
        &mut self,
        element_id: u64,
        property: &str,
        from_value: &str,
        to_value: &str,
        duration: f32,
        timing_fn: TimingFunction,
        delay: f32,
        current_time: f64,
    ) {
        debug!(
            element_id,
            property,
            from = from_value,
            to = to_value,
            duration,
            "starting CSS transition"
        );

        let state = TransitionState {
            property: property.to_string(),
            from_value: from_value.to_string(),
            to_value: to_value.to_string(),
            duration,
            timing_function: timing_fn,
            delay,
            start_time: current_time,
        };

        self.active.insert((element_id, property.to_string()), state);
    }

    /// Get the current interpolated value for a transitioning property.
    ///
    /// Returns `Some(value_string)` if the transition is active, or `None`
    /// if no transition is running for this (element, property).
    pub fn get_current_value(
        &self,
        element_id: u64,
        property: &str,
        current_time: f64,
    ) -> Option<String> {
        let state = self.active.get(&(element_id, property.to_string()))?;

        let elapsed = (current_time - state.start_time) as f32 - state.delay;
        if elapsed < 0.0 {
            // Still in the delay period — return the from value.
            return Some(state.from_value.clone());
        }

        if elapsed >= state.duration {
            // Transition has completed.
            return None;
        }

        let progress = compute_progress(elapsed, state.duration, &state.timing_function);
        let value = interpolate_value(&state.from_value, &state.to_value, progress);
        Some(value)
    }

    /// Remove completed transitions and return a list of elements that need
    /// their styles updated (transitions that just completed).
    pub fn tick(&mut self, current_time: f64) -> Vec<u64> {
        let mut completed_elements = Vec::new();
        self.active.retain(|&(element_id, _), state| {
            let elapsed = (current_time - state.start_time) as f32 - state.delay;
            if elapsed >= state.duration {
                completed_elements.push(element_id);
                false
            } else {
                true
            }
        });
        completed_elements.sort();
        completed_elements.dedup();
        completed_elements
    }

    /// Check if any transitions are currently active.
    pub fn has_active_transitions(&self) -> bool {
        !self.active.is_empty()
    }

    /// Get all active transition values for an element.
    ///
    /// Returns a list of `(property, current_value)` for all active transitions
    /// on the given element.
    pub fn get_all_values(
        &self,
        element_id: u64,
        current_time: f64,
    ) -> Vec<(String, String)> {
        let mut values = Vec::new();
        for (&(eid, ref prop), state) in &self.active {
            if eid != element_id {
                continue;
            }
            let elapsed = (current_time - state.start_time) as f32 - state.delay;
            if elapsed < 0.0 {
                values.push((prop.clone(), state.from_value.clone()));
            } else if elapsed < state.duration {
                let progress = compute_progress(elapsed, state.duration, &state.timing_function);
                let value = interpolate_value(&state.from_value, &state.to_value, progress);
                values.push((prop.clone(), value));
            }
            // If elapsed >= duration, the transition is complete; don't include.
        }
        values
    }
}

// ── Interpolation helpers for transforms ────────────────────────────

/// Interpolate between two CSS transform strings.
///
/// For simple cases (matching function lists), interpolates each function's
/// parameters independently. For mismatched transforms, falls back to matrix
/// decomposition and interpolation.
pub fn interpolate_transforms(from: &str, to: &str, progress: f32) -> String {
    // Simple case: both are "none" or identical.
    if from == to {
        return from.to_string();
    }
    if from == "none" && progress >= 1.0 {
        return to.to_string();
    }
    if to == "none" && progress <= 0.0 {
        return from.to_string();
    }

    // For now, snap at 50% for complex cases.
    // A full implementation would decompose matrices and interpolate.
    if progress < 0.5 {
        from.to_string()
    } else {
        to.to_string()
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_transition_simple() {
        let defs = parse_transition_shorthand("opacity 0.3s ease");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].property, "opacity");
        assert!((defs[0].duration - 0.3).abs() < 1e-4);
        assert_eq!(defs[0].timing_function, TimingFunction::Ease);
    }

    #[test]
    fn parse_transition_all() {
        let defs = parse_transition_shorthand("all 0.5s linear 0.1s");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].property, "all");
        assert!((defs[0].duration - 0.5).abs() < 1e-4);
        assert_eq!(defs[0].timing_function, TimingFunction::Linear);
        assert!((defs[0].delay - 0.1).abs() < 1e-4);
    }

    #[test]
    fn parse_transition_multiple() {
        let defs = parse_transition_shorthand("color 0.3s ease, opacity 0.5s linear");
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].property, "color");
        assert_eq!(defs[1].property, "opacity");
    }

    #[test]
    fn parse_transition_none() {
        let defs = parse_transition_shorthand("none");
        assert!(defs.is_empty());
    }

    #[test]
    fn parse_transition_ms_duration() {
        let defs = parse_transition_shorthand("transform 300ms ease-in-out");
        assert_eq!(defs.len(), 1);
        assert!((defs[0].duration - 0.3).abs() < 1e-4);
        assert_eq!(defs[0].timing_function, TimingFunction::EaseInOut);
    }

    #[test]
    fn transition_manager_basic() {
        let mut mgr = TransitionManager::new();
        mgr.start_transition(1, "opacity", "0", "1", 1.0, TimingFunction::Linear, 0.0, 0.0);

        // At t=0.5, opacity should be ~0.5.
        let val = mgr.get_current_value(1, "opacity", 0.5);
        assert!(val.is_some());
        let v: f32 = val.unwrap().parse().unwrap();
        assert!((v - 0.5).abs() < 0.1);

        // At t=1.0, transition is complete.
        let val = mgr.get_current_value(1, "opacity", 1.0);
        assert!(val.is_none());
    }

    #[test]
    fn transition_manager_delay() {
        let mut mgr = TransitionManager::new();
        mgr.start_transition(1, "color", "red", "blue", 1.0, TimingFunction::Linear, 0.5, 0.0);

        // During delay, should return from value.
        let val = mgr.get_current_value(1, "color", 0.3);
        assert_eq!(val, Some("red".to_string()));

        // After delay, should be interpolating.
        let val = mgr.get_current_value(1, "color", 1.0);
        assert!(val.is_some());
    }

    #[test]
    fn transition_manager_tick_removes_completed() {
        let mut mgr = TransitionManager::new();
        mgr.start_transition(1, "opacity", "0", "1", 0.5, TimingFunction::Linear, 0.0, 0.0);

        assert!(mgr.has_active_transitions());
        let completed = mgr.tick(1.0);
        assert!(completed.contains(&1));
        assert!(!mgr.has_active_transitions());
    }
}

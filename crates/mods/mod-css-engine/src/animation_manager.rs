//! CSS Animation manager (@keyframes).
//!
//! Tracks active CSS animations per element and provides interpolated
//! property values at any point in time. Handles iteration count, direction,
//! fill mode, and keyframe interpolation.

use std::collections::HashMap;

use tracing::debug;

use crate::animation::{
    compute_progress, interpolate_value, AnimationDirection, AnimationState, FillMode, Keyframe,
    TimingFunction,
};

// ── Animation Manager ───────────────────────────────────────────────

/// Manages active CSS animations for all elements.
///
/// Each element can have multiple active animations (one per animation-name).
/// The manager tracks animation state, performs keyframe interpolation, and
/// handles iteration count, direction, and fill mode.
#[derive(Debug, Default)]
pub struct AnimationManager {
    /// Active animations keyed by (element_id, animation_name).
    active: HashMap<(u64, String), AnimationState>,
}

impl AnimationManager {
    /// Create a new empty animation manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start an animation for a specific element.
    ///
    /// If an animation with the same name is already running on this element,
    /// it is replaced.
    pub fn start_animation(&mut self, element_id: u64, state: AnimationState) {
        debug!(
            element_id,
            name = %state.name,
            duration = state.duration,
            iterations = state.iteration_count,
            "starting CSS animation"
        );
        let name = state.name.clone();
        self.active.insert((element_id, name), state);
    }

    /// Advance all animations and return a list of (element_id, property, current_value)
    /// for all properties that currently have animated values.
    pub fn tick(&mut self, current_time: f64) -> Vec<(u64, String, String)> {
        let mut results = Vec::new();
        let mut to_remove = Vec::new();

        for (&(element_id, ref name), state) in &self.active {
            let elapsed = (current_time - state.start_time) as f32 - state.delay;

            // Before the animation starts (during delay).
            if elapsed < 0.0 {
                match state.fill_mode {
                    FillMode::Backwards | FillMode::Both => {
                        // Apply first keyframe values during delay.
                        let values = get_keyframe_values_at(state, 0.0);
                        for (prop, val) in values {
                            results.push((element_id, prop, val));
                        }
                    }
                    _ => {}
                }
                continue;
            }

            let total_duration = state.duration;
            if total_duration <= 0.0 {
                to_remove.push((element_id, name.clone()));
                continue;
            }

            // Compute current iteration and progress within that iteration.
            let raw_iterations = elapsed / total_duration;
            let max_iterations = state.iteration_count;

            if raw_iterations >= max_iterations {
                // Animation has finished all iterations.
                match state.fill_mode {
                    FillMode::Forwards | FillMode::Both => {
                        // Keep the final frame values.
                        let final_progress = compute_direction_progress(
                            max_iterations,
                            &state.direction,
                        );
                        let values = get_keyframe_values_at(state, final_progress);
                        for (prop, val) in values {
                            results.push((element_id, prop, val));
                        }
                    }
                    _ => {}
                }
                to_remove.push((element_id, name.clone()));
                continue;
            }

            // Compute progress accounting for direction.
            let progress = compute_direction_progress(raw_iterations, &state.direction);

            // Get interpolated values from keyframes.
            let values = get_keyframe_values_at(state, progress);
            for (prop, val) in values {
                results.push((element_id, prop, val));
            }
        }

        // Remove completed animations.
        for key in to_remove {
            self.active.remove(&key);
        }

        results
    }

    /// Check if any animations are currently active.
    pub fn has_active_animations(&self) -> bool {
        !self.active.is_empty()
    }

    /// Get all active animated property values for a specific element.
    pub fn get_element_values(
        &self,
        element_id: u64,
        current_time: f64,
    ) -> Vec<(String, String)> {
        let mut values = Vec::new();

        for (&(eid, _), state) in &self.active {
            if eid != element_id {
                continue;
            }

            let elapsed = (current_time - state.start_time) as f32 - state.delay;
            if elapsed < 0.0 {
                match state.fill_mode {
                    FillMode::Backwards | FillMode::Both => {
                        let kf_values = get_keyframe_values_at(state, 0.0);
                        values.extend(kf_values);
                    }
                    _ => {}
                }
                continue;
            }

            let total_duration = state.duration;
            if total_duration <= 0.0 {
                continue;
            }

            let raw_iterations = elapsed / total_duration;
            if raw_iterations >= state.iteration_count {
                match state.fill_mode {
                    FillMode::Forwards | FillMode::Both => {
                        let final_progress = compute_direction_progress(
                            state.iteration_count,
                            &state.direction,
                        );
                        let kf_values = get_keyframe_values_at(state, final_progress);
                        values.extend(kf_values);
                    }
                    _ => {}
                }
                continue;
            }

            let progress = compute_direction_progress(raw_iterations, &state.direction);
            let kf_values = get_keyframe_values_at(state, progress);
            values.extend(kf_values);
        }

        values
    }

    /// Remove all animations for a specific element.
    pub fn remove_element(&mut self, element_id: u64) {
        self.active.retain(|&(eid, _), _| eid != element_id);
    }
}

// ── Direction helpers ───────────────────────────────────────────────

/// Compute the effective progress (0.0 to 1.0) within a single iteration,
/// accounting for the animation direction.
fn compute_direction_progress(raw_iterations: f32, direction: &AnimationDirection) -> f32 {
    // When raw_iterations is exactly an integer (e.g. 1.0 at end of first iteration),
    // fract() returns 0.0. We want 1.0 for the final value in that case.
    let iteration_progress = raw_iterations.fract();
    let current_iteration = raw_iterations.floor() as u32;
    let progress = if iteration_progress == 0.0 && raw_iterations > 0.0 {
        1.0 // End of an iteration: treat as fully complete.
    } else {
        iteration_progress.clamp(0.0, 1.0)
    };

    // For the "end of iteration" case, use the previous iteration's index
    // to determine direction (since we're at the end of that iteration).
    let dir_iteration = if iteration_progress == 0.0 && raw_iterations > 0.0 {
        current_iteration.saturating_sub(1)
    } else {
        current_iteration
    };

    match direction {
        AnimationDirection::Normal => progress,
        AnimationDirection::Reverse => 1.0 - progress,
        AnimationDirection::Alternate => {
            if dir_iteration % 2 == 0 {
                progress
            } else {
                1.0 - progress
            }
        }
        AnimationDirection::AlternateReverse => {
            if dir_iteration % 2 == 0 {
                1.0 - progress
            } else {
                progress
            }
        }
    }
}

// ── Keyframe interpolation ──────────────────────────────────────────

/// Get interpolated property values at a given progress (0.0 to 1.0)
/// from the animation's keyframes.
fn get_keyframe_values_at(state: &AnimationState, progress: f32) -> Vec<(String, String)> {
    if state.keyframes.is_empty() {
        return Vec::new();
    }

    // Find the two keyframes that bracket the current progress.
    let keyframes = &state.keyframes;
    let mut before_idx = 0;
    let mut after_idx = keyframes.len() - 1;

    for (i, kf) in keyframes.iter().enumerate() {
        if kf.offset <= progress {
            before_idx = i;
        }
        if kf.offset >= progress && i < after_idx {
            after_idx = i;
            break;
        }
    }

    let before = &keyframes[before_idx];
    let after = &keyframes[after_idx];

    // If both keyframes are the same, return the values directly.
    if before_idx == after_idx {
        return before
            .declarations
            .iter()
            .map(|(prop, val)| (prop.clone(), val.clone()))
            .collect();
    }

    // Compute local progress between the two keyframes.
    let range = after.offset - before.offset;
    let local_t = if range > 0.0 {
        ((progress - before.offset) / range).clamp(0.0, 1.0)
    } else {
        1.0
    };

    // Apply the animation's timing function to the local progress.
    let eased_t = compute_progress(local_t * 1.0, 1.0, &state.timing_function);

    // Interpolate each property declared in the "after" keyframe.
    let mut values = Vec::new();
    for (prop, to_val) in &after.declarations {
        // Find the matching property in the "before" keyframe.
        let from_val = before
            .declarations
            .iter()
            .find(|(p, _)| p == prop)
            .map(|(_, v)| v.as_str())
            .unwrap_or(to_val.as_str());

        let interpolated = interpolate_value(from_val, to_val, eased_t);
        values.push((prop.clone(), interpolated));
    }

    values
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::{AnimationDirection, AnimationState, FillMode, Keyframe, TimingFunction};

    fn make_fade_animation(start_time: f64) -> AnimationState {
        AnimationState {
            name: "fadeIn".to_string(),
            duration: 1.0,
            timing_function: TimingFunction::Linear,
            delay: 0.0,
            iteration_count: 1.0,
            direction: AnimationDirection::Normal,
            fill_mode: FillMode::None,
            start_time,
            keyframes: vec![
                Keyframe {
                    offset: 0.0,
                    declarations: vec![("opacity".to_string(), "0".to_string())],
                },
                Keyframe {
                    offset: 1.0,
                    declarations: vec![("opacity".to_string(), "1".to_string())],
                },
            ],
        }
    }

    #[test]
    fn animation_basic_progress() {
        let mut mgr = AnimationManager::new();
        mgr.start_animation(1, make_fade_animation(0.0));

        let results = mgr.tick(0.5);
        assert!(!results.is_empty());
        let (eid, prop, val) = &results[0];
        assert_eq!(*eid, 1);
        assert_eq!(prop, "opacity");
        let v: f32 = val.parse().unwrap();
        assert!((v - 0.5).abs() < 0.1, "expected ~0.5, got {v}");
    }

    #[test]
    fn animation_completes() {
        let mut mgr = AnimationManager::new();
        mgr.start_animation(1, make_fade_animation(0.0));

        let _results = mgr.tick(2.0);
        assert!(!mgr.has_active_animations());
    }

    #[test]
    fn animation_fill_forwards() {
        let mut mgr = AnimationManager::new();
        let mut anim = make_fade_animation(0.0);
        anim.fill_mode = FillMode::Forwards;
        mgr.start_animation(1, anim);

        // After completion, fill-forwards should still return the final value.
        let results = mgr.tick(2.0);
        assert!(!results.is_empty());
        let (_, _, val) = &results[0];
        assert_eq!(val, "1");
    }

    #[test]
    fn animation_fill_backwards_during_delay() {
        let mut mgr = AnimationManager::new();
        let mut anim = make_fade_animation(0.0);
        anim.delay = 1.0;
        anim.fill_mode = FillMode::Backwards;
        mgr.start_animation(1, anim);

        // During the delay, fill-backwards should apply the first keyframe.
        let results = mgr.tick(0.5);
        assert!(!results.is_empty());
        let (_, _, val) = &results[0];
        assert_eq!(val, "0");
    }

    #[test]
    fn animation_infinite() {
        let mut mgr = AnimationManager::new();
        let mut anim = make_fade_animation(0.0);
        anim.iteration_count = f32::INFINITY;
        mgr.start_animation(1, anim);

        // Should still be active after many ticks.
        let _ = mgr.tick(100.0);
        assert!(mgr.has_active_animations());
    }

    #[test]
    fn animation_alternate_direction() {
        let progress_fwd = compute_direction_progress(0.5, &AnimationDirection::Alternate);
        assert!((progress_fwd - 0.5).abs() < 0.01);

        let progress_rev = compute_direction_progress(1.5, &AnimationDirection::Alternate);
        assert!((progress_rev - 0.5).abs() < 0.01);
    }

    #[test]
    fn animation_reverse_direction() {
        let progress = compute_direction_progress(0.25, &AnimationDirection::Reverse);
        assert!((progress - 0.75).abs() < 0.01);
    }

    #[test]
    fn direction_alternate_reverse() {
        // Even iterations: reversed (1.0 -> 0.0).
        let p0 = compute_direction_progress(0.5, &AnimationDirection::AlternateReverse);
        assert!((p0 - 0.5).abs() < 0.01);

        // Odd iterations: forward (0.0 -> 1.0).
        let p1 = compute_direction_progress(1.5, &AnimationDirection::AlternateReverse);
        assert!((p1 - 0.5).abs() < 0.01);
    }

    #[test]
    fn keyframe_interpolation_midpoint() {
        let state = AnimationState {
            name: "test".to_string(),
            duration: 1.0,
            timing_function: TimingFunction::Linear,
            delay: 0.0,
            iteration_count: 1.0,
            direction: AnimationDirection::Normal,
            fill_mode: FillMode::None,
            start_time: 0.0,
            keyframes: vec![
                Keyframe {
                    offset: 0.0,
                    declarations: vec![("width".to_string(), "0px".to_string())],
                },
                Keyframe {
                    offset: 0.5,
                    declarations: vec![("width".to_string(), "50px".to_string())],
                },
                Keyframe {
                    offset: 1.0,
                    declarations: vec![("width".to_string(), "100px".to_string())],
                },
            ],
        };

        // At progress 0.25, should be between keyframes 0 and 0.5.
        let values = get_keyframe_values_at(&state, 0.25);
        assert!(!values.is_empty());
        let (_, val) = &values[0];
        assert!(val.contains("25"), "expected ~25px, got {val}");
    }
}

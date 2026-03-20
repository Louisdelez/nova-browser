//! CSS animation interpolation and transition detection.
//!
//! Provides types and functions for CSS animations (`@keyframes`), transitions,
//! timing functions (cubic bezier curves), and value interpolation between
//! CSS states.

use tracing::debug;

// ── Timing functions ─────────────────────────────────────────────────

/// A CSS timing function controlling animation easing.
#[derive(Debug, Clone, PartialEq)]
pub enum TimingFunction {
    /// Linear interpolation (no easing).
    Linear,
    /// Default ease curve: `cubic-bezier(0.25, 0.1, 0.25, 1.0)`.
    Ease,
    /// Ease-in curve: `cubic-bezier(0.42, 0, 1, 1)`.
    EaseIn,
    /// Ease-out curve: `cubic-bezier(0, 0, 0.58, 1)`.
    EaseOut,
    /// Ease-in-out curve: `cubic-bezier(0.42, 0, 0.58, 1)`.
    EaseInOut,
    /// Custom cubic bezier curve with control points `(x1, y1, x2, y2)`.
    CubicBezier(f32, f32, f32, f32),
}

/// Direction in which a CSS animation plays.
#[derive(Debug, Clone, PartialEq)]
pub enum AnimationDirection {
    /// Play forwards each iteration.
    Normal,
    /// Play backwards each iteration.
    Reverse,
    /// Alternate between forwards and backwards on each iteration.
    Alternate,
    /// Alternate starting backwards, then forwards.
    AlternateReverse,
}

/// CSS `animation-fill-mode` — what styles apply outside the animation window.
#[derive(Debug, Clone, PartialEq)]
pub enum FillMode {
    /// No fill — styles revert after animation ends.
    None,
    /// Keep the final keyframe styles after the animation ends.
    Forwards,
    /// Apply the first keyframe styles during the delay period.
    Backwards,
    /// Combine `Forwards` and `Backwards`.
    Both,
}

// ── Keyframe ─────────────────────────────────────────────────────────

/// A single keyframe stop within an animation.
#[derive(Debug, Clone)]
pub struct Keyframe {
    /// The progress offset (0.0 = start, 1.0 = end).
    pub offset: f32,
    /// Property-value pairs declared at this keyframe.
    pub declarations: Vec<(String, String)>,
}

// ── AnimationState ───────────────────────────────────────────────────

/// Complete state for a running CSS animation.
#[derive(Debug, Clone)]
pub struct AnimationState {
    /// The animation name (matches `@keyframes` rule name).
    pub name: String,
    /// Duration in seconds.
    pub duration: f32,
    /// Timing function for easing.
    pub timing_function: TimingFunction,
    /// Delay in seconds before the animation starts.
    pub delay: f32,
    /// Number of iterations (`f32::INFINITY` for `infinite`).
    pub iteration_count: f32,
    /// Play direction.
    pub direction: AnimationDirection,
    /// Fill mode.
    pub fill_mode: FillMode,
    /// Timestamp when the animation started (seconds since page load).
    pub start_time: f64,
    /// Keyframe stops for this animation.
    pub keyframes: Vec<Keyframe>,
}

// ── TransitionState ──────────────────────────────────────────────────

/// State for a running CSS transition on a single property.
#[derive(Debug, Clone)]
pub struct TransitionState {
    /// The CSS property being transitioned.
    pub property: String,
    /// The starting value.
    pub from_value: String,
    /// The target value.
    pub to_value: String,
    /// Duration in seconds.
    pub duration: f32,
    /// Timing function for easing.
    pub timing_function: TimingFunction,
    /// Delay in seconds before the transition starts.
    pub delay: f32,
    /// Timestamp when the transition started (seconds since page load).
    pub start_time: f64,
}

// ── Cubic bezier ─────────────────────────────────────────────────────

/// Compute the value of a cubic bezier curve at parameter `t`.
///
/// The curve is defined by control points `(p1x, p1y)` and `(p2x, p2y)`,
/// with implicit start `(0, 0)` and end `(1, 1)`.
///
/// Uses Newton-Raphson iteration to solve the cubic equation for `t`
/// that gives the desired x-axis position, then evaluates the y component.
pub fn cubic_bezier(t: f32, p1x: f32, p1y: f32, p2x: f32, p2y: f32) -> f32 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }

    // Linear case — avoid iteration.
    if (p1x - p1y).abs() < 1e-6 && (p2x - p2y).abs() < 1e-6 {
        return t;
    }

    // Find the parametric `s` such that `bezier_x(s) = t` using Newton's method.
    let mut s = t; // initial guess
    for _ in 0..8 {
        let x = bezier_component(s, p1x, p2x) - t;
        let dx = bezier_derivative(s, p1x, p2x);
        if dx.abs() < 1e-8 {
            break;
        }
        s -= x / dx;
        s = s.clamp(0.0, 1.0);
    }

    // Evaluate y at the solved parameter.
    bezier_component(s, p1y, p2y)
}

/// Evaluate one component (x or y) of a cubic bezier at parameter `s`.
///
/// B(s) = 3*(1-s)^2*s*p1 + 3*(1-s)*s^2*p2 + s^3
fn bezier_component(s: f32, p1: f32, p2: f32) -> f32 {
    let s2 = s * s;
    let s3 = s2 * s;
    let inv = 1.0 - s;
    let inv2 = inv * inv;
    3.0 * inv2 * s * p1 + 3.0 * inv * s2 * p2 + s3
}

/// Derivative of the bezier component with respect to `s`.
fn bezier_derivative(s: f32, p1: f32, p2: f32) -> f32 {
    let s2 = s * s;
    3.0 * (1.0 - s) * (1.0 - s) * p1 + 6.0 * (1.0 - s) * s * (p2 - p1) + 3.0 * s2 * (1.0 - p2)
}

// ── Progress computation ─────────────────────────────────────────────

/// Compute the eased progress for an animation or transition.
///
/// `elapsed` is the time since the animation started (minus delay), in seconds.
/// `duration` is the total duration in seconds.
/// Returns the eased progress value in `[0.0, 1.0]`.
pub fn compute_progress(elapsed: f32, duration: f32, timing: &TimingFunction) -> f32 {
    if duration <= 0.0 {
        return 1.0;
    }
    let raw = (elapsed / duration).clamp(0.0, 1.0);
    apply_timing_function(raw, timing)
}

/// Apply a timing function to a linear progress value.
fn apply_timing_function(t: f32, timing: &TimingFunction) -> f32 {
    match timing {
        TimingFunction::Linear => t,
        TimingFunction::Ease => cubic_bezier(t, 0.25, 0.1, 0.25, 1.0),
        TimingFunction::EaseIn => cubic_bezier(t, 0.42, 0.0, 1.0, 1.0),
        TimingFunction::EaseOut => cubic_bezier(t, 0.0, 0.0, 0.58, 1.0),
        TimingFunction::EaseInOut => cubic_bezier(t, 0.42, 0.0, 0.58, 1.0),
        TimingFunction::CubicBezier(x1, y1, x2, y2) => cubic_bezier(t, *x1, *y1, *x2, *y2),
    }
}

// ── Value interpolation ──────────────────────────────────────────────

/// Interpolate between two CSS values at the given progress (0.0 to 1.0).
///
/// Handles:
/// - Numeric values with units (px, em, %, rem): linear interpolation
/// - Colors (hex, rgb/rgba, named): component-wise interpolation
/// - Opacity: linear interpolation
/// - Keywords: snap at 50% (use `from` before 0.5, `to` at 0.5+)
pub fn interpolate_value(from: &str, to: &str, progress: f32) -> String {
    let from = from.trim();
    let to = to.trim();

    // Same value — no interpolation needed.
    if from == to {
        return from.to_string();
    }

    // Try numeric interpolation (px, em, %, rem, bare numbers).
    if let (Some((from_num, from_unit)), Some((to_num, to_unit))) =
        (parse_numeric(from), parse_numeric(to))
    {
        if from_unit == to_unit {
            let val = from_num + (to_num - from_num) * progress;
            return if to_unit.is_empty() {
                format_number(val)
            } else {
                format!("{}{}", format_number(val), to_unit)
            };
        }
    }

    // Try color interpolation.
    if let (Some(fc), Some(tc)) = (parse_color_value(from), parse_color_value(to)) {
        let r = lerp_u8(fc.0, tc.0, progress);
        let g = lerp_u8(fc.1, tc.1, progress);
        let b = lerp_u8(fc.2, tc.2, progress);
        let a = fc.3 + (tc.3 - fc.3) * progress;
        if (a - 1.0).abs() < 1e-4 {
            return format!("rgb({r}, {g}, {b})");
        } else {
            return format!("rgba({r}, {g}, {b}, {:.2})", a);
        }
    }

    // Keywords: snap at 50%.
    if progress < 0.5 {
        from.to_string()
    } else {
        to.to_string()
    }
}

/// Parse a numeric value with optional unit suffix.
///
/// Returns `(number, unit)` where unit is "" for bare numbers.
fn parse_numeric(s: &str) -> Option<(f32, &str)> {
    let s = s.trim();
    for unit in &["px", "em", "rem", "%", "vh", "vw", "deg", "s", "ms"] {
        if let Some(num_str) = s.strip_suffix(unit) {
            if let Ok(v) = num_str.trim().parse::<f32>() {
                return Some((v, unit));
            }
        }
    }
    // Bare number.
    if let Ok(v) = s.parse::<f32>() {
        return Some((v, ""));
    }
    None
}

/// Format a float with reasonable precision, stripping trailing zeros.
fn format_number(v: f32) -> String {
    if (v - v.round()).abs() < 1e-4 {
        format!("{}", v.round() as i32)
    } else {
        format!("{:.2}", v)
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// Linearly interpolate between two u8 values.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let result = a as f32 + (b as f32 - a as f32) * t;
    result.round().clamp(0.0, 255.0) as u8
}

/// Parse a CSS color value into (r, g, b, a) components.
///
/// Handles hex (#rgb, #rrggbb, #rrggbbaa), rgb(), rgba(), and named colors.
fn parse_color_value(s: &str) -> Option<(u8, u8, u8, f32)> {
    let s = s.trim();

    // Hex colors.
    if s.starts_with('#') {
        return parse_hex_color(s);
    }

    // rgb() / rgba().
    if s.starts_with("rgb") {
        return parse_rgb_function(s);
    }

    // Named colors (basic set).
    match s.to_ascii_lowercase().as_str() {
        "black" => Some((0, 0, 0, 1.0)),
        "white" => Some((255, 255, 255, 1.0)),
        "red" => Some((255, 0, 0, 1.0)),
        "green" => Some((0, 128, 0, 1.0)),
        "blue" => Some((0, 0, 255, 1.0)),
        "yellow" => Some((255, 255, 0, 1.0)),
        "cyan" | "aqua" => Some((0, 255, 255, 1.0)),
        "magenta" | "fuchsia" => Some((255, 0, 255, 1.0)),
        "gray" | "grey" => Some((128, 128, 128, 1.0)),
        "orange" => Some((255, 165, 0, 1.0)),
        "purple" => Some((128, 0, 128, 1.0)),
        "transparent" => Some((0, 0, 0, 0.0)),
        _ => None,
    }
}

/// Parse a hex color string (#rgb, #rrggbb, or #rrggbbaa).
fn parse_hex_color(s: &str) -> Option<(u8, u8, u8, f32)> {
    let hex = s.strip_prefix('#')?;
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some((r, g, b, 1.0))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some((r, g, b, 1.0))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some((r, g, b, a as f32 / 255.0))
        }
        _ => None,
    }
}

/// Parse `rgb(r, g, b)` or `rgba(r, g, b, a)` function syntax.
fn parse_rgb_function(s: &str) -> Option<(u8, u8, u8, f32)> {
    let inner = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').collect();
    match parts.len() {
        3 => {
            let r = parts[0].trim().parse::<u8>().ok()?;
            let g = parts[1].trim().parse::<u8>().ok()?;
            let b = parts[2].trim().parse::<u8>().ok()?;
            Some((r, g, b, 1.0))
        }
        4 => {
            let r = parts[0].trim().parse::<u8>().ok()?;
            let g = parts[1].trim().parse::<u8>().ok()?;
            let b = parts[2].trim().parse::<u8>().ok()?;
            let a = parts[3].trim().parse::<f32>().ok()?;
            Some((r, g, b, a))
        }
        _ => None,
    }
}

// ── Parsing helpers ──────────────────────────────────────────────────

/// Parse a CSS `animation-timing-function` value into a `TimingFunction`.
///
/// Supports `linear`, `ease`, `ease-in`, `ease-out`, `ease-in-out`,
/// and `cubic-bezier(x1, y1, x2, y2)`.
pub fn parse_timing_function(value: &str) -> TimingFunction {
    let value = value.trim();
    match value {
        "linear" => TimingFunction::Linear,
        "ease" => TimingFunction::Ease,
        "ease-in" => TimingFunction::EaseIn,
        "ease-out" => TimingFunction::EaseOut,
        "ease-in-out" => TimingFunction::EaseInOut,
        _ => {
            // Try cubic-bezier(x1, y1, x2, y2).
            if let Some(inner) = value
                .strip_prefix("cubic-bezier(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let parts: Vec<f32> = inner
                    .split(',')
                    .filter_map(|p| p.trim().parse().ok())
                    .collect();
                if parts.len() == 4 {
                    return TimingFunction::CubicBezier(parts[0], parts[1], parts[2], parts[3]);
                }
            }
            debug!(value, "unknown timing function, defaulting to ease");
            TimingFunction::Ease
        }
    }
}

/// Parse a CSS `animation` shorthand value into an `AnimationState`.
///
/// The shorthand format is:
/// `name duration [timing-function] [delay] [iteration-count] [direction] [fill-mode]`
///
/// Example: `fadeIn 0.3s ease-in 0s 1 normal forwards`
pub fn parse_animation_shorthand(value: &str) -> AnimationState {
    let parts: Vec<&str> = value.split_whitespace().collect();

    let mut state = AnimationState {
        name: String::new(),
        duration: 0.0,
        timing_function: TimingFunction::Ease,
        delay: 0.0,
        iteration_count: 1.0,
        direction: AnimationDirection::Normal,
        fill_mode: FillMode::None,
        start_time: 0.0,
        keyframes: Vec::new(),
    };

    if parts.is_empty() {
        return state;
    }

    // First token is always the name.
    state.name = parts[0].to_string();

    // Parse remaining tokens by type.
    // Keywords must be checked BEFORE time values because words like
    // "forwards" end with 's' and would be mis-parsed as a time value.
    let mut duration_set = false;
    for &part in &parts[1..] {
        // Timing function keywords (check first — "ease-in" etc.).
        match part {
            "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out" => {
                state.timing_function = parse_timing_function(part);
                continue;
            }
            _ => {}
        }

        // Direction keywords.
        match part {
            "normal" => {
                state.direction = AnimationDirection::Normal;
                continue;
            }
            "reverse" => {
                state.direction = AnimationDirection::Reverse;
                continue;
            }
            "alternate" => {
                state.direction = AnimationDirection::Alternate;
                continue;
            }
            "alternate-reverse" => {
                state.direction = AnimationDirection::AlternateReverse;
                continue;
            }
            _ => {}
        }

        // Fill mode keywords.
        match part {
            "none" => {
                state.fill_mode = FillMode::None;
                continue;
            }
            "forwards" => {
                state.fill_mode = FillMode::Forwards;
                continue;
            }
            "backwards" => {
                state.fill_mode = FillMode::Backwards;
                continue;
            }
            "both" => {
                state.fill_mode = FillMode::Both;
                continue;
            }
            _ => {}
        }

        // Iteration count keyword.
        if part == "infinite" {
            state.iteration_count = f32::INFINITY;
            continue;
        }

        // Duration/delay: ends with 's' or 'ms' (checked after keywords).
        if part.ends_with("ms") || part.ends_with('s') {
            let secs = parse_time_value(part);
            if !duration_set {
                state.duration = secs;
                duration_set = true;
            } else {
                state.delay = secs;
            }
            continue;
        }

        // Bare number — iteration count.
        if let Ok(count) = part.parse::<f32>() {
            state.iteration_count = count;
        }
    }

    state
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

// ── Transition detection ─────────────────────────────────────────────

/// Detect CSS transitions by comparing old and new style values.
///
/// If the `transition-property` matches a property that changed between
/// `old_style` and `new_style`, and `transition-duration` is non-zero,
/// a `TransitionState` is created for each changed property.
///
/// `old_style` and `new_style` are semicolon-separated CSS declaration strings
/// (the format used in `data-nova-style`).
pub fn detect_transitions(
    old_style: &str,
    new_style: &str,
    transition_property: &str,
    transition_duration: &str,
) -> Vec<TransitionState> {
    let duration = parse_time_value(transition_duration);
    if duration <= 0.0 {
        return Vec::new();
    }

    let old_props = parse_style_map(old_style);
    let new_props = parse_style_map(new_style);

    let properties: Vec<&str> = if transition_property == "all" {
        // All properties that differ.
        new_props.keys().map(|k| k.as_str()).collect()
    } else {
        transition_property.split(',').map(|s| s.trim()).collect()
    };

    let mut transitions = Vec::new();
    for prop in properties {
        let old_val = old_props.get(prop).map(|s| s.as_str()).unwrap_or("");
        let new_val = new_props.get(prop).map(|s| s.as_str()).unwrap_or("");

        if old_val != new_val && !old_val.is_empty() && !new_val.is_empty() {
            transitions.push(TransitionState {
                property: prop.to_string(),
                from_value: old_val.to_string(),
                to_value: new_val.to_string(),
                duration,
                timing_function: TimingFunction::Ease,
                delay: 0.0,
                start_time: 0.0,
            });
        }
    }

    transitions
}

/// Parse a semicolon-separated style string into a property map.
fn parse_style_map(style: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for decl in style.split(';') {
        let parts: Vec<&str> = decl.splitn(2, ':').collect();
        if parts.len() == 2 {
            let prop = parts[0].trim().to_string();
            let val = parts[1].trim().to_string();
            if !prop.is_empty() && !val.is_empty() {
                map.insert(prop, val);
            }
        }
    }
    map
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- Cubic bezier tests --

    #[test]
    fn cubic_bezier_linear() {
        // Linear: control points on the diagonal.
        let val = cubic_bezier(0.5, 0.0, 0.0, 1.0, 1.0);
        assert!((val - 0.5).abs() < 0.05, "linear bezier at 0.5 should be ~0.5, got {val}");
    }

    #[test]
    fn cubic_bezier_endpoints() {
        assert!((cubic_bezier(0.0, 0.25, 0.1, 0.25, 1.0)).abs() < 1e-6);
        assert!((cubic_bezier(1.0, 0.25, 0.1, 0.25, 1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cubic_bezier_ease_midpoint() {
        // Ease curve should be > 0.5 at t=0.5 (it accelerates then decelerates).
        let val = cubic_bezier(0.5, 0.25, 0.1, 0.25, 1.0);
        assert!(val > 0.5, "ease at t=0.5 should be > 0.5, got {val}");
    }

    // -- Linear interpolation tests --

    #[test]
    fn interpolate_px_values() {
        assert_eq!(interpolate_value("0px", "100px", 0.0), "0px");
        assert_eq!(interpolate_value("0px", "100px", 0.5), "50px");
        assert_eq!(interpolate_value("0px", "100px", 1.0), "100px");
    }

    #[test]
    fn interpolate_percentage_values() {
        assert_eq!(interpolate_value("0%", "100%", 0.25), "25%");
    }

    #[test]
    fn interpolate_bare_numbers() {
        assert_eq!(interpolate_value("0", "1", 0.5), "0.5");
    }

    // -- Color interpolation tests --

    #[test]
    fn interpolate_hex_colors() {
        let result = interpolate_value("#000000", "#ffffff", 0.5);
        // Should be approximately rgb(128, 128, 128).
        assert!(
            result.contains("128") || result.contains("127"),
            "midpoint of black-white should be ~gray, got {result}"
        );
    }

    #[test]
    fn interpolate_named_colors() {
        let result = interpolate_value("black", "white", 1.0);
        assert!(
            result.contains("255"),
            "interpolating to white should reach 255, got {result}"
        );
    }

    #[test]
    fn interpolate_rgba_colors() {
        let result = interpolate_value("rgba(0, 0, 0, 0)", "rgba(255, 255, 255, 1)", 0.5);
        assert!(
            result.contains("128") || result.contains("127"),
            "midpoint rgba should have ~128 components, got {result}"
        );
    }

    // -- Keyword interpolation --

    #[test]
    fn interpolate_keywords_snap_at_half() {
        assert_eq!(interpolate_value("visible", "hidden", 0.3), "visible");
        assert_eq!(interpolate_value("visible", "hidden", 0.7), "hidden");
    }

    // -- Parse timing function tests --

    #[test]
    fn parse_timing_function_keywords() {
        assert_eq!(parse_timing_function("linear"), TimingFunction::Linear);
        assert_eq!(parse_timing_function("ease"), TimingFunction::Ease);
        assert_eq!(parse_timing_function("ease-in"), TimingFunction::EaseIn);
        assert_eq!(parse_timing_function("ease-out"), TimingFunction::EaseOut);
        assert_eq!(
            parse_timing_function("ease-in-out"),
            TimingFunction::EaseInOut
        );
    }

    #[test]
    fn parse_timing_function_cubic_bezier() {
        let tf = parse_timing_function("cubic-bezier(0.1, 0.2, 0.3, 0.4)");
        assert_eq!(tf, TimingFunction::CubicBezier(0.1, 0.2, 0.3, 0.4));
    }

    // -- Animation shorthand parsing --

    #[test]
    fn parse_animation_shorthand_full() {
        let anim = parse_animation_shorthand("fadeIn 0.3s ease-in 0.1s 2 alternate forwards");
        assert_eq!(anim.name, "fadeIn");
        assert!((anim.duration - 0.3).abs() < 1e-4);
        assert_eq!(anim.timing_function, TimingFunction::EaseIn);
        assert!((anim.delay - 0.1).abs() < 1e-4);
        assert!((anim.iteration_count - 2.0).abs() < 1e-4);
        assert_eq!(anim.direction, AnimationDirection::Alternate);
        assert_eq!(anim.fill_mode, FillMode::Forwards);
    }

    #[test]
    fn parse_animation_shorthand_minimal() {
        let anim = parse_animation_shorthand("spin 1s");
        assert_eq!(anim.name, "spin");
        assert!((anim.duration - 1.0).abs() < 1e-4);
        assert_eq!(anim.timing_function, TimingFunction::Ease); // default
    }

    #[test]
    fn parse_animation_shorthand_infinite() {
        let anim = parse_animation_shorthand("rotate 2s linear infinite");
        assert_eq!(anim.name, "rotate");
        assert!(anim.iteration_count.is_infinite());
        assert_eq!(anim.timing_function, TimingFunction::Linear);
    }

    #[test]
    fn parse_animation_shorthand_ms_duration() {
        let anim = parse_animation_shorthand("pulse 300ms");
        assert!((anim.duration - 0.3).abs() < 1e-4);
    }

    // -- Transition detection tests --

    #[test]
    fn detect_transition_color_change() {
        let transitions = detect_transitions(
            "color: red; font-size: 16px",
            "color: blue; font-size: 16px",
            "color",
            "0.3s",
        );
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].property, "color");
        assert_eq!(transitions[0].from_value, "red");
        assert_eq!(transitions[0].to_value, "blue");
    }

    #[test]
    fn detect_transition_no_change() {
        let transitions = detect_transitions(
            "color: red",
            "color: red",
            "color",
            "0.3s",
        );
        assert!(transitions.is_empty());
    }

    #[test]
    fn detect_transition_zero_duration() {
        let transitions = detect_transitions(
            "color: red",
            "color: blue",
            "color",
            "0s",
        );
        assert!(transitions.is_empty());
    }

    #[test]
    fn detect_transition_all_properties() {
        let transitions = detect_transitions(
            "color: red; opacity: 1",
            "color: blue; opacity: 0.5",
            "all",
            "0.5s",
        );
        assert_eq!(transitions.len(), 2);
    }

    // -- Progress computation --

    #[test]
    fn compute_progress_linear() {
        let p = compute_progress(0.5, 1.0, &TimingFunction::Linear);
        assert!((p - 0.5).abs() < 1e-4);
    }

    #[test]
    fn compute_progress_zero_duration() {
        let p = compute_progress(0.0, 0.0, &TimingFunction::Linear);
        assert!((p - 1.0).abs() < 1e-4);
    }

    #[test]
    fn compute_progress_clamped() {
        let p = compute_progress(2.0, 1.0, &TimingFunction::Linear);
        assert!((p - 1.0).abs() < 1e-4);
    }
}

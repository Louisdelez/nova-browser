//! CSS `@media` query evaluation.
//!
//! Provides types and functions for parsing and evaluating CSS media queries
//! against a viewport. This module complements the basic media query handling
//! already present in `parser.rs` by providing a richer, more complete API.

use tracing::debug;

// ── Types ────────────────────────────────────────────────────────────

/// Screen orientation.
#[derive(Debug, Clone, PartialEq)]
pub enum Orientation {
    /// Viewport height >= width.
    Portrait,
    /// Viewport width > height.
    Landscape,
}

/// A single media query condition.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaCondition {
    /// `(min-width: Npx)` — viewport width must be >= N.
    MinWidth(f32),
    /// `(max-width: Npx)` — viewport width must be <= N.
    MaxWidth(f32),
    /// `(min-height: Npx)` — viewport height must be >= N.
    MinHeight(f32),
    /// `(max-height: Npx)` — viewport height must be <= N.
    MaxHeight(f32),
    /// `(orientation: portrait|landscape)`.
    Orientation(Orientation),
    /// `(prefers-color-scheme: light|dark)`.
    PrefersColorScheme(String),
    /// `screen` media type — always matches for a screen browser.
    Screen,
    /// `print` media type — never matches for a screen browser.
    Print,
    /// `all` media type — always matches.
    All,
}

/// A parsed CSS `@media` query.
#[derive(Debug, Clone)]
pub struct MediaQuery {
    /// The conditions that must all be satisfied (AND logic).
    pub conditions: Vec<MediaCondition>,
    /// If `true`, the entire query result is negated (`not`).
    pub negated: bool,
}

// ── Evaluation ───────────────────────────────────────────────────────

/// Evaluate a media query against the current viewport dimensions.
///
/// Returns `true` if the query matches. All conditions are ANDed together,
/// then the result is negated if the query has `not`.
///
/// For our screen-based browser:
/// - `screen` and `all` always match
/// - `print` never matches
/// - `prefers-color-scheme` defaults to `light`
pub fn evaluate_media_query(
    query: &MediaQuery,
    viewport_width: f32,
    viewport_height: f32,
) -> bool {
    let mut result = true;

    for condition in &query.conditions {
        let matches = match condition {
            MediaCondition::MinWidth(min) => viewport_width >= *min,
            MediaCondition::MaxWidth(max) => viewport_width <= *max,
            MediaCondition::MinHeight(min) => viewport_height >= *min,
            MediaCondition::MaxHeight(max) => viewport_height <= *max,
            MediaCondition::Orientation(orient) => {
                let actual = if viewport_height >= viewport_width {
                    Orientation::Portrait
                } else {
                    Orientation::Landscape
                };
                *orient == actual
            }
            MediaCondition::PrefersColorScheme(scheme) => {
                // Default to light mode for now.
                scheme == "light"
            }
            MediaCondition::Screen => true,
            MediaCondition::Print => false,
            MediaCondition::All => true,
        };

        if !matches {
            result = false;
            break;
        }
    }

    if query.negated {
        !result
    } else {
        result
    }
}

// ── Parsing ──────────────────────────────────────────────────────────

/// Parse a CSS media query string into a `MediaQuery`.
///
/// Supports:
/// - Media types: `screen`, `print`, `all`
/// - `not` prefix for negation
/// - `only` prefix (ignored, treated as pass-through)
/// - Feature expressions: `(min-width: 768px)`, `(max-width: 1024px)`,
///   `(min-height: ...)`, `(max-height: ...)`, `(orientation: portrait|landscape)`,
///   `(prefers-color-scheme: light|dark)`
/// - `and` combinator
/// - Basic `or` / `,` support (first query only for now)
pub fn parse_media_query(query_str: &str) -> MediaQuery {
    let query_str = query_str.trim();

    // Handle empty query — matches all.
    if query_str.is_empty() {
        return MediaQuery {
            conditions: vec![MediaCondition::All],
            negated: false,
        };
    }

    // Take only the first query if there are commas (basic OR support).
    let first_query = query_str.split(',').next().unwrap_or(query_str).trim();

    let mut remaining = first_query;
    let mut negated = false;

    // Check for `not` prefix.
    if let Some(rest) = remaining
        .strip_prefix("not ")
        .or_else(|| remaining.strip_prefix("not\t"))
    {
        negated = true;
        remaining = rest.trim();
    }

    // Strip `only` prefix (no semantic effect in modern CSS).
    if let Some(rest) = remaining
        .strip_prefix("only ")
        .or_else(|| remaining.strip_prefix("only\t"))
    {
        remaining = rest.trim();
    }

    let mut conditions = Vec::new();

    // Split by ` and ` (with surrounding spaces) to avoid splitting inside
    // words like "landscape".
    for part in remaining.split(" and ") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Media type keywords.
        if part.eq_ignore_ascii_case("screen") {
            conditions.push(MediaCondition::Screen);
            continue;
        }
        if part.eq_ignore_ascii_case("print") {
            conditions.push(MediaCondition::Print);
            continue;
        }
        if part.eq_ignore_ascii_case("all") {
            conditions.push(MediaCondition::All);
            continue;
        }

        // Feature expression: strip parentheses.
        let inner = part
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim();

        if let Some(cond) = parse_feature_expression(inner) {
            conditions.push(cond);
        } else {
            debug!(part, "unrecognized media query part, ignoring");
        }
    }

    // Default to `all` if no conditions were parsed.
    if conditions.is_empty() {
        conditions.push(MediaCondition::All);
    }

    MediaQuery {
        conditions,
        negated,
    }
}

/// Parse a single media feature expression like `min-width: 768px`.
fn parse_feature_expression(expr: &str) -> Option<MediaCondition> {
    let (name, value) = expr.split_once(':')?;
    let name = name.trim().to_ascii_lowercase();
    let value = value.trim();

    match name.as_str() {
        "min-width" => {
            let px = parse_length_px(value)?;
            Some(MediaCondition::MinWidth(px))
        }
        "max-width" => {
            let px = parse_length_px(value)?;
            Some(MediaCondition::MaxWidth(px))
        }
        "min-height" => {
            let px = parse_length_px(value)?;
            Some(MediaCondition::MinHeight(px))
        }
        "max-height" => {
            let px = parse_length_px(value)?;
            Some(MediaCondition::MaxHeight(px))
        }
        "orientation" => {
            let orient = match value.to_ascii_lowercase().as_str() {
                "portrait" => Orientation::Portrait,
                "landscape" => Orientation::Landscape,
                _ => return None,
            };
            Some(MediaCondition::Orientation(orient))
        }
        "prefers-color-scheme" => {
            Some(MediaCondition::PrefersColorScheme(
                value.to_ascii_lowercase(),
            ))
        }
        _ => {
            debug!(name = name.as_str(), "unknown media feature");
            None
        }
    }
}

/// Parse a CSS length value into pixels.
///
/// Handles `px` (exact) and `em` (approximated as 16 * value).
fn parse_length_px(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(num_str) = s.strip_suffix("px") {
        return num_str.trim().parse().ok();
    }
    if let Some(num_str) = s.strip_suffix("em") {
        let val: f32 = num_str.trim().parse().ok()?;
        return Some(val * 16.0);
    }
    if let Some(num_str) = s.strip_suffix("rem") {
        let val: f32 = num_str.trim().parse().ok()?;
        return Some(val * 16.0);
    }
    // Bare number — treat as px.
    s.parse().ok()
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_query_min_width_match() {
        let q = parse_media_query("(min-width: 768px)");
        assert!(evaluate_media_query(&q, 1024.0, 768.0));
        assert!(!evaluate_media_query(&q, 500.0, 768.0));
    }

    #[test]
    fn media_query_max_width_match() {
        let q = parse_media_query("(max-width: 1024px)");
        assert!(evaluate_media_query(&q, 800.0, 600.0));
        assert!(!evaluate_media_query(&q, 1280.0, 600.0));
    }

    #[test]
    fn media_query_min_and_max_width() {
        let q = parse_media_query("(min-width: 768px) and (max-width: 1024px)");
        assert!(evaluate_media_query(&q, 800.0, 600.0));
        assert!(!evaluate_media_query(&q, 500.0, 600.0));
        assert!(!evaluate_media_query(&q, 1280.0, 600.0));
    }

    #[test]
    fn media_query_orientation_portrait() {
        let q = parse_media_query("(orientation: portrait)");
        // Portrait: height >= width.
        assert!(evaluate_media_query(&q, 600.0, 800.0));
        assert!(!evaluate_media_query(&q, 1024.0, 768.0));
    }

    #[test]
    fn media_query_orientation_landscape() {
        let q = parse_media_query("(orientation: landscape)");
        assert!(evaluate_media_query(&q, 1024.0, 768.0));
        assert!(!evaluate_media_query(&q, 600.0, 800.0));
    }

    #[test]
    fn media_query_negation() {
        let q = parse_media_query("not print");
        // `not print` should match for a screen browser.
        assert!(evaluate_media_query(&q, 1024.0, 768.0));
    }

    #[test]
    fn media_query_not_screen() {
        let q = parse_media_query("not screen");
        // `not screen` should NOT match for a screen browser.
        assert!(!evaluate_media_query(&q, 1024.0, 768.0));
    }

    #[test]
    fn media_query_screen_and_width() {
        let q = parse_media_query("screen and (max-width: 700px)");
        assert!(evaluate_media_query(&q, 500.0, 768.0));
        assert!(!evaluate_media_query(&q, 1024.0, 768.0));
    }

    #[test]
    fn media_query_print_never_matches() {
        let q = parse_media_query("print");
        assert!(!evaluate_media_query(&q, 1024.0, 768.0));
    }

    #[test]
    fn media_query_height_conditions() {
        let q = parse_media_query("(min-height: 600px)");
        assert!(evaluate_media_query(&q, 1024.0, 768.0));
        assert!(!evaluate_media_query(&q, 1024.0, 400.0));

        let q = parse_media_query("(max-height: 600px)");
        assert!(evaluate_media_query(&q, 1024.0, 400.0));
        assert!(!evaluate_media_query(&q, 1024.0, 768.0));
    }

    #[test]
    fn media_query_prefers_color_scheme() {
        let q = parse_media_query("(prefers-color-scheme: light)");
        assert!(evaluate_media_query(&q, 1024.0, 768.0));

        let q = parse_media_query("(prefers-color-scheme: dark)");
        assert!(!evaluate_media_query(&q, 1024.0, 768.0));
    }

    #[test]
    fn media_query_empty_matches_all() {
        let q = parse_media_query("");
        assert!(evaluate_media_query(&q, 1024.0, 768.0));
    }

    #[test]
    fn media_query_only_prefix_ignored() {
        let q = parse_media_query("only screen and (min-width: 768px)");
        assert!(evaluate_media_query(&q, 1024.0, 768.0));
        assert!(!evaluate_media_query(&q, 500.0, 768.0));
    }

    #[test]
    fn media_query_em_units() {
        // 48em = 48 * 16 = 768px
        let q = parse_media_query("(min-width: 48em)");
        assert!(evaluate_media_query(&q, 800.0, 600.0));
        assert!(!evaluate_media_query(&q, 700.0, 600.0));
    }
}

//! CSS value parsing: colors, lengths, keywords.
//!
//! Converts raw CSS tokens into `StyleValue` / `CssColor` types from `nova-mod-api`.

use nova_mod_api::content::{CssColor, StyleValue};

/// Parse a CSS value string into a `StyleValue`.
///
/// This handles the common value types: keywords, lengths (px, em, %),
/// colors (named, hex, rgb/rgba), numbers, and strings.
pub fn parse_value(property: &str, raw: &str) -> StyleValue {
    let raw = raw.trim();

    // Try color properties first.
    if is_color_property(property) {
        if let Some(color) = parse_color(raw) {
            return StyleValue::Color(color);
        }
        // Fall through to keyword/other parsing.
    }

    // Try numeric/length values.
    if let Some(sv) = parse_length_or_number(raw) {
        return sv;
    }

    // Default: treat as keyword.
    StyleValue::Keyword(raw.to_string())
}

/// Whether the given property expects a color value.
fn is_color_property(property: &str) -> bool {
    matches!(
        property,
        "color"
            | "background-color"
            | "border-color"
            | "border-top-color"
            | "border-right-color"
            | "border-bottom-color"
            | "border-left-color"
            | "outline-color"
            | "text-decoration-color"
    )
}

/// Evaluate a `calc()` CSS expression with a context value for resolving
/// percentages and viewport-relative units.
///
/// `expr` is the inner part of `calc(...)` (without the outer `calc(` and `)`).
/// `context_px` is the reference size in pixels used to resolve `%` and `vw`
/// units (e.g., the parent element's width).
///
/// Returns the computed value in pixels, or `None` if the expression cannot
/// be parsed.
///
/// # Supported operands
///
/// - `<n>px` — pixels
/// - `<n>%` — percentage of `context_px`
/// - `<n>em` / `<n>rem` — approximated at 16px per unit
/// - `<n>vw` — 1% of `context_px`
/// - `<n>pt` — 1pt = 1.333px
/// - bare numbers — treated as pixels
///
/// # Supported operators
///
/// `+`, `-`, `*`, `/` with standard precedence (`*`/`/` before `+`/`-`),
/// and parentheses for grouping. Nested `calc()` calls are also supported.
pub fn eval_calc(expr: &str, context_px: f32) -> Option<f32> {
    let tokens = tokenise_calc_ctx(expr, context_px)?;
    eval_tokens(&tokens)
}

/// Evaluate a `clamp()`, `min()`, or `max()` CSS math function with a context
/// value for resolving percentages and viewport-relative units.
///
/// `raw` is the full function call (e.g., `clamp(200px, 50%, 800px)`).
/// `context_px` is the reference size for `%` and `vw` units.
///
/// Returns the computed value in pixels, or `None` if the expression cannot
/// be parsed.
pub fn eval_math_function(raw: &str, context_px: f32) -> Option<f32> {
    if let Some(inner) = raw.strip_prefix("clamp(").and_then(|s| s.strip_suffix(')')) {
        let args = split_function_args(inner);
        if args.len() != 3 {
            return None;
        }
        let min_val = eval_math_arg_ctx(args[0].trim(), context_px)?;
        let preferred = eval_math_arg_ctx(args[1].trim(), context_px)?;
        let max_val = eval_math_arg_ctx(args[2].trim(), context_px)?;
        return Some(min_val.max(preferred.min(max_val)));
    }

    if let Some(inner) = raw.strip_prefix("min(").and_then(|s| s.strip_suffix(')')) {
        let args = split_function_args(inner);
        let mut result = f32::INFINITY;
        for arg in &args {
            let val = eval_math_arg_ctx(arg.trim(), context_px)?;
            if val < result {
                result = val;
            }
        }
        return Some(result);
    }

    if let Some(inner) = raw.strip_prefix("max(").and_then(|s| s.strip_suffix(')')) {
        let args = split_function_args(inner);
        let mut result = f32::NEG_INFINITY;
        for arg in &args {
            let val = eval_math_arg_ctx(arg.trim(), context_px)?;
            if val > result {
                result = val;
            }
        }
        return Some(result);
    }

    None
}

/// Evaluate a single math function argument to px, resolving `%` and `vw`
/// against `context_px`.
fn eval_math_arg_ctx(arg: &str, context_px: f32) -> Option<f32> {
    let arg = arg.trim();

    // Nested calc()
    if arg.starts_with("calc(") {
        let inner = arg.strip_prefix("calc(")?.strip_suffix(')')?;
        return eval_calc(inner, context_px);
    }

    // Nested min/max/clamp
    if arg.starts_with("min(") || arg.starts_with("max(") || arg.starts_with("clamp(") {
        return eval_math_function(arg, context_px);
    }

    // Try percentage
    if let Some(s) = arg.strip_suffix('%') {
        return s.trim().parse::<f32>().ok().map(|n| n / 100.0 * context_px);
    }
    // Try vw/vh
    if let Some(s) = arg.strip_suffix("vw").or_else(|| arg.strip_suffix("vh")) {
        return s.trim().parse::<f32>().ok().map(|n| n / 100.0 * context_px);
    }
    // Try px
    if let Some(s) = arg.strip_suffix("px") {
        return s.trim().parse::<f32>().ok();
    }
    // Try em/rem
    if let Some(s) = arg.strip_suffix("rem") {
        return s.trim().parse::<f32>().ok().map(|n| n * 16.0);
    }
    if let Some(s) = arg.strip_suffix("em") {
        return s.trim().parse::<f32>().ok().map(|n| n * 16.0);
    }
    // Try pt
    if let Some(s) = arg.strip_suffix("pt") {
        return s.trim().parse::<f32>().ok().map(|n| n * 1.333);
    }
    // Plain number (treated as px)
    arg.parse::<f32>().ok()
}

/// Resolve a `calc(...)` CSS expression.
///
/// If all operands are in `px` (or plain numbers), evaluates the expression fully
/// and returns `StyleValue::Px`. If the expression contains `%` or other
/// context-dependent units, returns `StyleValue::Str` preserving the original
/// value so downstream consumers can resolve it at layout time.
fn resolve_calc(raw: &str) -> Option<StyleValue> {
    // Strip the outer `calc(` prefix and matching `)` suffix.
    let inner = raw.strip_prefix("calc(")?.strip_suffix(')')?;
    let inner = inner.trim();

    // If expression contains %, vh, or vw we cannot fully resolve it here.
    // Preserve the original calc() string for downstream resolution.
    if inner.contains('%') || inner.contains("vh") || inner.contains("vw") {
        return Some(StyleValue::Str(raw.to_string()));
    }

    // Tokenise the expression into (value_in_px, operator) pairs.
    // We evaluate left-to-right honouring standard operator precedence by doing
    // two passes: first * and /, then + and -.
    match eval_calc_expr(inner) {
        Some(px) => Some(StyleValue::Px(px)),
        None => Some(StyleValue::Str(raw.to_string())),
    }
}

/// Resolve an `env()` CSS function.
///
/// Since NOVA does not have safe area insets or other environment variables,
/// the fallback value is returned. If no fallback is specified, `0px` is used.
///
/// # Examples
///
/// - `env(safe-area-inset-top)` → `0px`
/// - `env(safe-area-inset-top, 20px)` → `20px`
fn resolve_env(raw: &str) -> Option<StyleValue> {
    let inner = raw.strip_prefix("env(")?.strip_suffix(')')?.trim();

    // Split into name and optional fallback at the first comma.
    let fallback = if let Some(comma_pos) = inner.find(',') {
        inner[comma_pos + 1..].trim()
    } else {
        "0px"
    };

    // Parse the fallback as a regular value.
    parse_length_or_number(fallback).or_else(|| Some(StyleValue::Str(fallback.to_string())))
}

/// Resolve `clamp()`, `min()`, or `max()` CSS math functions.
///
/// If all arguments can be fully resolved (no `%` or viewport units),
/// returns the computed `StyleValue::Px`. Otherwise, preserves the
/// original string as `StyleValue::Str` for downstream resolution.
///
/// # Supported functions
///
/// - `clamp(min, preferred, max)` → `max(min, min(preferred, max))`
/// - `min(a, b, ...)` → smallest value
/// - `max(a, b, ...)` → largest value
fn resolve_math_function(raw: &str) -> Option<StyleValue> {
    // Check if it contains %, vw, vh — if so, preserve for layout-time resolution.
    if raw.contains('%') || raw.contains("vw") || raw.contains("vh") {
        return Some(StyleValue::Str(raw.to_string()));
    }

    if let Some(inner) = raw.strip_prefix("clamp(").and_then(|s| s.strip_suffix(')')) {
        let args = split_function_args(inner);
        if args.len() != 3 {
            return Some(StyleValue::Str(raw.to_string()));
        }
        let min_val = eval_math_arg(args[0].trim())?;
        let preferred = eval_math_arg(args[1].trim())?;
        let max_val = eval_math_arg(args[2].trim())?;
        // clamp(min, preferred, max) = max(min, min(preferred, max))
        let result = min_val.max(preferred.min(max_val));
        return Some(StyleValue::Px(result));
    }

    if let Some(inner) = raw.strip_prefix("min(").and_then(|s| s.strip_suffix(')')) {
        let args = split_function_args(inner);
        if args.is_empty() {
            return Some(StyleValue::Str(raw.to_string()));
        }
        let mut result = f32::INFINITY;
        for arg in &args {
            let val = eval_math_arg(arg.trim())?;
            if val < result {
                result = val;
            }
        }
        return Some(StyleValue::Px(result));
    }

    if let Some(inner) = raw.strip_prefix("max(").and_then(|s| s.strip_suffix(')')) {
        let args = split_function_args(inner);
        if args.is_empty() {
            return Some(StyleValue::Str(raw.to_string()));
        }
        let mut result = f32::NEG_INFINITY;
        for arg in &args {
            let val = eval_math_arg(arg.trim())?;
            if val > result {
                result = val;
            }
        }
        return Some(StyleValue::Px(result));
    }

    Some(StyleValue::Str(raw.to_string()))
}

/// Split a function's arguments at top-level commas (respecting nested parens).
fn split_function_args(inner: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;

    for (i, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            ',' if depth == 0 => {
                args.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    // Push the last argument.
    let last = &inner[start..];
    if !last.trim().is_empty() {
        args.push(last);
    }
    args
}

/// Evaluate a single math function argument to a px value.
///
/// Supports plain numbers, px/em/rem/pt values, calc(), and nested
/// min()/max()/clamp() calls.
fn eval_math_arg(arg: &str) -> Option<f32> {
    let arg = arg.trim();

    // Nested calc()
    if arg.starts_with("calc(") {
        let inner = arg.strip_prefix("calc(")?.strip_suffix(')')?;
        return eval_calc_expr(inner);
    }

    // Nested min/max/clamp
    if arg.starts_with("min(") || arg.starts_with("max(") || arg.starts_with("clamp(") {
        match resolve_math_function(arg)? {
            StyleValue::Px(px) => return Some(px),
            _ => return None,
        }
    }

    // Try px
    if let Some(s) = arg.strip_suffix("px") {
        return s.trim().parse::<f32>().ok();
    }
    // Try em/rem
    if let Some(s) = arg.strip_suffix("rem") {
        return s.trim().parse::<f32>().ok().map(|n| n * 16.0);
    }
    if let Some(s) = arg.strip_suffix("em") {
        return s.trim().parse::<f32>().ok().map(|n| n * 16.0);
    }
    // Try pt
    if let Some(s) = arg.strip_suffix("pt") {
        return s.trim().parse::<f32>().ok().map(|n| n * 1.333);
    }
    // Plain number (treated as px)
    arg.parse::<f32>().ok()
}

/// Evaluate a `calc` inner expression that contains only px / plain-number
/// operands.  Returns the result in px.
fn eval_calc_expr(expr: &str) -> Option<f32> {
    let tokens = tokenise_calc(expr)?;
    eval_tokens(&tokens)
}

/// A calc token: either a numeric value (already in px) or an operator.
#[derive(Debug, Clone)]
enum CalcToken {
    Value(f32),
    Op(char),
}

/// Convert a calc expression string into a flat list of tokens.
///
/// Handles nested parentheses by recursively evaluating sub-expressions.
fn tokenise_calc(expr: &str) -> Option<Vec<CalcToken>> {
    let expr = expr.trim();
    let mut tokens: Vec<CalcToken> = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Skip whitespace.
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        // Nested parentheses — recursively evaluate the sub-expression.
        if chars[i] == '(' {
            let start = i + 1;
            let mut depth = 1usize;
            i += 1;
            while i < len && depth > 0 {
                if chars[i] == '(' {
                    depth += 1;
                } else if chars[i] == ')' {
                    depth -= 1;
                }
                i += 1;
            }
            let sub: String = chars[start..i - 1].iter().collect();
            let val = eval_calc_expr(&sub)?;
            tokens.push(CalcToken::Value(val));
            continue;
        }

        // Operator characters.  A '-' can be a unary minus on the first token
        // or after another operator — handled via the number parser below.
        if matches!(chars[i], '+' | '*' | '/') {
            tokens.push(CalcToken::Op(chars[i]));
            i += 1;
            continue;
        }

        // A '-' that follows an existing value token is a subtraction operator.
        // A '-' at the start or after another operator is part of a negative number.
        if chars[i] == '-' {
            let after_op = tokens.last().map_or(true, |t| matches!(t, CalcToken::Op(_)));
            if !after_op {
                tokens.push(CalcToken::Op('-'));
                i += 1;
                continue;
            }
            // Fall through: parse it as part of a number literal.
        }

        // Parse a number literal (possibly with a unit suffix).
        let start = i;
        // Consume optional leading '-'.
        if i < len && chars[i] == '-' {
            i += 1;
        }
        // Consume digits and decimal point.
        while i < len && (chars[i].is_ascii_digit() || chars[i] == '.') {
            i += 1;
        }
        // Consume unit suffix (letters only, e.g. "px", "em", "rem", "pt").
        let unit_start = i;
        while i < len && chars[i].is_ascii_alphabetic() {
            i += 1;
        }

        let num_str: String = chars[start..unit_start].iter().collect();
        let unit: String = chars[unit_start..i].iter().collect();

        let num: f32 = num_str.parse().ok()?;
        let px = match unit.as_str() {
            "px" | "" => num,
            "em" | "rem" => num * 16.0,
            "pt" => num * 1.333,
            _ => return None, // Unknown unit — bail out.
        };
        tokens.push(CalcToken::Value(px));
    }

    Some(tokens)
}

/// Convert a calc expression string into a flat list of tokens, resolving
/// `%` and `vw` units against `context_px`.
///
/// Also handles nested `calc(...)` calls by stripping the inner `calc` prefix
/// and recursively evaluating.
fn tokenise_calc_ctx(expr: &str, context_px: f32) -> Option<Vec<CalcToken>> {
    let expr = expr.trim();
    let mut tokens: Vec<CalcToken> = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Skip whitespace.
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        // Detect nested calc(...) — strip it and recurse.
        if i + 5 <= len {
            let word: String = chars[i..i + 5].iter().collect();
            if word == "calc(" {
                // Find the matching closing paren.
                let start = i + 5;
                let mut depth = 1usize;
                let mut j = start;
                while j < len && depth > 0 {
                    if chars[j] == '(' {
                        depth += 1;
                    } else if chars[j] == ')' {
                        depth -= 1;
                    }
                    j += 1;
                }
                let sub: String = chars[start..j - 1].iter().collect();
                let val = eval_calc(&sub, context_px)?;
                tokens.push(CalcToken::Value(val));
                i = j;
                continue;
            }
        }

        // Nested parentheses — recursively evaluate the sub-expression.
        if chars[i] == '(' {
            let start = i + 1;
            let mut depth = 1usize;
            i += 1;
            while i < len && depth > 0 {
                if chars[i] == '(' {
                    depth += 1;
                } else if chars[i] == ')' {
                    depth -= 1;
                }
                i += 1;
            }
            let sub: String = chars[start..i - 1].iter().collect();
            let val = eval_calc(&sub, context_px)?;
            tokens.push(CalcToken::Value(val));
            continue;
        }

        // Operator characters.
        if matches!(chars[i], '+' | '*' | '/') {
            tokens.push(CalcToken::Op(chars[i]));
            i += 1;
            continue;
        }

        // A '-' that follows an existing value token is a subtraction operator.
        if chars[i] == '-' {
            let after_op = tokens.last().map_or(true, |t| matches!(t, CalcToken::Op(_)));
            if !after_op {
                tokens.push(CalcToken::Op('-'));
                i += 1;
                continue;
            }
        }

        // Parse a number literal (possibly with a unit suffix).
        let start = i;
        if i < len && chars[i] == '-' {
            i += 1;
        }
        while i < len && (chars[i].is_ascii_digit() || chars[i] == '.') {
            i += 1;
        }
        // Consume unit suffix (letters or %).
        let unit_start = i;
        if i < len && chars[i] == '%' {
            i += 1;
        } else {
            while i < len && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
        }

        let num_str: String = chars[start..unit_start].iter().collect();
        let unit: String = chars[unit_start..i].iter().collect();

        let num: f32 = num_str.parse().ok()?;
        let px = match unit.as_str() {
            "px" | "" => num,
            "%" => num / 100.0 * context_px,
            "em" | "rem" => num * 16.0,
            "vw" | "vh" => num / 100.0 * context_px,
            "pt" => num * 1.333,
            _ => return None,
        };
        tokens.push(CalcToken::Value(px));
    }

    Some(tokens)
}

/// Evaluate a flat token list respecting `*`/`/` before `+`/`-`.
fn eval_tokens(tokens: &[CalcToken]) -> Option<f32> {
    if tokens.is_empty() {
        return None;
    }

    // First pass: resolve * and /.
    let mut after_mul_div: Vec<CalcToken> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            CalcToken::Op('*') => {
                let left = match after_mul_div.pop()? {
                    CalcToken::Value(v) => v,
                    _ => return None,
                };
                i += 1;
                let right = match tokens.get(i)? {
                    CalcToken::Value(v) => *v,
                    _ => return None,
                };
                after_mul_div.push(CalcToken::Value(left * right));
            }
            CalcToken::Op('/') => {
                let left = match after_mul_div.pop()? {
                    CalcToken::Value(v) => v,
                    _ => return None,
                };
                i += 1;
                let right = match tokens.get(i)? {
                    CalcToken::Value(v) => *v,
                    _ => return None,
                };
                if right == 0.0 {
                    return None;
                }
                after_mul_div.push(CalcToken::Value(left / right));
            }
            other => after_mul_div.push(other.clone()),
        }
        i += 1;
    }

    // Second pass: resolve + and -.
    let mut result = match after_mul_div.first()? {
        CalcToken::Value(v) => *v,
        _ => return None,
    };
    let mut i = 1;
    while i < after_mul_div.len() {
        let op = match &after_mul_div[i] {
            CalcToken::Op(c) => *c,
            _ => return None,
        };
        i += 1;
        let rhs = match after_mul_div.get(i)? {
            CalcToken::Value(v) => *v,
            _ => return None,
        };
        match op {
            '+' => result += rhs,
            '-' => result -= rhs,
            _ => return None,
        }
        i += 1;
    }

    Some(result)
}

/// Try to parse a CSS length (`px`, `em`, `rem`, `%`) or plain number.
///
/// Also handles `calc()`, `clamp()`, `min()`, `max()`, and `env()` expressions.
fn parse_length_or_number(raw: &str) -> Option<StyleValue> {
    if raw.starts_with("calc(") {
        return resolve_calc(raw);
    }

    // Handle env() — return fallback value or 0px.
    if raw.starts_with("env(") {
        return resolve_env(raw);
    }

    // Handle clamp(), min(), max() — these may contain %, vw, etc.
    if raw.starts_with("clamp(") || raw.starts_with("min(") || raw.starts_with("max(") {
        return resolve_math_function(raw);
    }

    if raw.ends_with("px") {
        raw[..raw.len() - 2].trim().parse::<f32>().ok().map(StyleValue::Px)
    } else if raw.ends_with('%') {
        raw[..raw.len() - 1]
            .trim()
            .parse::<f32>()
            .ok()
            .map(StyleValue::Percent)
    } else if raw.ends_with("em") || raw.ends_with("rem") {
        // Convert em/rem to px using 16px base (approximate).
        // The cascade will re-resolve `em` values contextually against the
        // parent's computed font-size when available.
        let num_end = if raw.ends_with("rem") {
            raw.len() - 3
        } else {
            raw.len() - 2
        };
        raw[..num_end]
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| StyleValue::Px(n * 16.0))
    } else if raw.ends_with("pt") {
        // 1pt = 1.333px
        raw[..raw.len() - 2]
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| StyleValue::Px(n * 1.333))
    } else if raw.ends_with("vh") || raw.ends_with("vw") {
        // Treat viewport units as percentages for now.
        raw[..raw.len() - 2]
            .trim()
            .parse::<f32>()
            .ok()
            .map(StyleValue::Percent)
    } else {
        // Try plain number.
        raw.parse::<f32>().ok().map(StyleValue::Number)
    }
}

/// Resolve a CSS value string that may contain `em` units against a
/// given parent font-size context.
///
/// Returns the resolved px value if the input is an `em` value, or `None`
/// if the input is not an em value.
pub fn resolve_em_value(raw: &str, parent_font_size: f32) -> Option<f32> {
    let raw = raw.trim();
    if raw.ends_with("em") && !raw.ends_with("rem") {
        let num_end = raw.len() - 2;
        raw[..num_end]
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| n * parent_font_size)
    } else {
        None
    }
}

/// Parse a CSS color from a string.
///
/// Supports: named colors, `#hex`, `rgb()`, `rgba()`.
pub fn parse_color(raw: &str) -> Option<CssColor> {
    let raw = raw.trim();

    if raw.starts_with('#') {
        return parse_hex_color(raw);
    }
    if raw.starts_with("rgb") {
        return parse_rgb_color(raw);
    }
    if raw.starts_with("hsl") {
        return parse_hsl_color(raw);
    }
    parse_named_color(raw)
}

/// Parse a hex color: `#RGB`, `#RRGGBB`, `#RGBA`, `#RRGGBBAA`.
fn parse_hex_color(raw: &str) -> Option<CssColor> {
    let hex = &raw[1..];
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some(CssColor { r, g, b, a: 1.0 })
        }
        4 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            let a = u8::from_str_radix(&hex[3..4], 16).ok()? * 17;
            Some(CssColor {
                r,
                g,
                b,
                a: a as f32 / 255.0,
            })
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(CssColor { r, g, b, a: 1.0 })
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(CssColor {
                r,
                g,
                b,
                a: a as f32 / 255.0,
            })
        }
        _ => None,
    }
}

/// Parse `rgb(r, g, b)` or `rgba(r, g, b, a)`.
fn parse_rgb_color(raw: &str) -> Option<CssColor> {
    // Strip "rgb(" or "rgba(" and the closing ")".
    let inner = raw
        .strip_prefix("rgba(")
        .or_else(|| raw.strip_prefix("rgb("))?
        .strip_suffix(')')?
        .trim();

    // Split by comma or slash (modern syntax uses spaces + slash for alpha).
    let parts: Vec<&str> = if inner.contains(',') {
        inner.split(',').map(str::trim).collect()
    } else {
        // Modern syntax: `rgb(255 128 0 / 0.5)`
        let slash_parts: Vec<&str> = inner.splitn(2, '/').collect();
        let mut rgb: Vec<&str> = slash_parts[0].split_whitespace().collect();
        if slash_parts.len() > 1 {
            rgb.push(slash_parts[1].trim());
        }
        rgb
    };

    if parts.len() < 3 {
        return None;
    }

    let r = parse_color_component(parts[0])?;
    let g = parse_color_component(parts[1])?;
    let b = parse_color_component(parts[2])?;
    let a = if parts.len() >= 4 {
        parts[3].trim().parse::<f32>().unwrap_or(1.0)
    } else {
        1.0
    };

    Some(CssColor { r, g, b, a })
}

/// Parse a single color component (0-255 or 0%-100%).
fn parse_color_component(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let val: f32 = pct.trim().parse().ok()?;
        Some((val * 2.55).round().clamp(0.0, 255.0) as u8)
    } else {
        let val: f32 = s.parse().ok()?;
        Some(val.round().clamp(0.0, 255.0) as u8)
    }
}

/// Parse `hsl(h, s%, l%)` or `hsla(h, s%, l%, a)`.
///
/// Also supports the modern space-separated syntax: `hsl(h s% l% / a)`.
fn parse_hsl_color(raw: &str) -> Option<CssColor> {
    let inner = raw
        .strip_prefix("hsla(")
        .or_else(|| raw.strip_prefix("hsl("))?
        .strip_suffix(')')?
        .trim();

    // Split by comma or slash (modern syntax uses spaces + slash for alpha).
    let parts: Vec<&str> = if inner.contains(',') {
        inner.split(',').map(str::trim).collect()
    } else {
        let slash_parts: Vec<&str> = inner.splitn(2, '/').collect();
        let mut hsl: Vec<&str> = slash_parts[0].split_whitespace().collect();
        if slash_parts.len() > 1 {
            hsl.push(slash_parts[1].trim());
        }
        hsl
    };

    if parts.len() < 3 {
        return None;
    }

    // Parse hue (degrees, optionally with deg/rad/turn suffix).
    let h = parse_hue(parts[0])?;
    // Parse saturation (0-100, with optional %).
    let s = parse_percent_value(parts[1])?;
    // Parse lightness (0-100, with optional %).
    let l = parse_percent_value(parts[2])?;
    let a = if parts.len() >= 4 {
        let a_str = parts[3].trim();
        if let Some(pct) = a_str.strip_suffix('%') {
            pct.trim().parse::<f32>().unwrap_or(100.0) / 100.0
        } else {
            a_str.parse::<f32>().unwrap_or(1.0)
        }
    } else {
        1.0
    };

    let (r, g, b) = hsl_to_rgb(h, s / 100.0, l / 100.0);
    Some(CssColor {
        r: (r * 255.0).round().clamp(0.0, 255.0) as u8,
        g: (g * 255.0).round().clamp(0.0, 255.0) as u8,
        b: (b * 255.0).round().clamp(0.0, 255.0) as u8,
        a,
    })
}

/// Parse a hue value (degrees). Supports bare numbers, `deg`, `rad`, `turn`.
fn parse_hue(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("deg") {
        v.trim().parse::<f32>().ok()
    } else if let Some(v) = s.strip_suffix("rad") {
        v.trim().parse::<f32>().ok().map(|r| r.to_degrees())
    } else if let Some(v) = s.strip_suffix("turn") {
        v.trim().parse::<f32>().ok().map(|t| t * 360.0)
    } else {
        s.parse::<f32>().ok()
    }
    .map(|h| ((h % 360.0) + 360.0) % 360.0) // Normalize to 0-360
}

/// Parse a percentage value, stripping the optional `%` suffix.
fn parse_percent_value(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f32>().ok()
    } else {
        s.parse::<f32>().ok()
    }
}

/// Convert HSL to RGB. h is in degrees (0-360), s and l are 0.0-1.0.
/// Returns (r, g, b) each in 0.0-1.0.
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (l, l, l); // achromatic
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let h_norm = h / 360.0;
    let r = hue_to_rgb(p, q, h_norm + 1.0 / 3.0);
    let g = hue_to_rgb(p, q, h_norm);
    let b = hue_to_rgb(p, q, h_norm - 1.0 / 3.0);
    (r, g, b)
}

fn hue_to_rgb(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 { t += 1.0; }
    if t > 1.0 { t -= 1.0; }
    if t < 1.0 / 6.0 { return p + (q - p) * 6.0 * t; }
    if t < 1.0 / 2.0 { return q; }
    if t < 2.0 / 3.0 { return p + (q - p) * (2.0 / 3.0 - t) * 6.0; }
    p
}

/// Parse a named CSS color.
pub fn parse_named_color(name: &str) -> Option<CssColor> {
    let c = |r, g, b| {
        Some(CssColor {
            r,
            g,
            b,
            a: 1.0,
        })
    };

    match name.to_ascii_lowercase().as_str() {
        "transparent" => Some(CssColor { r: 0, g: 0, b: 0, a: 0.0 }),
        "black" => c(0, 0, 0),
        "white" => c(255, 255, 255),
        "red" => c(255, 0, 0),
        "green" => c(0, 128, 0),
        "blue" => c(0, 0, 255),
        "yellow" => c(255, 255, 0),
        "cyan" | "aqua" => c(0, 255, 255),
        "magenta" | "fuchsia" => c(255, 0, 255),
        "gray" | "grey" => c(128, 128, 128),
        "silver" => c(192, 192, 192),
        "maroon" => c(128, 0, 0),
        "olive" => c(128, 128, 0),
        "lime" => c(0, 255, 0),
        "teal" => c(0, 128, 128),
        "navy" => c(0, 0, 128),
        "purple" => c(128, 0, 128),
        "orange" => c(255, 165, 0),
        "pink" => c(255, 192, 203),
        "brown" => c(165, 42, 42),
        "coral" => c(255, 127, 80),
        "crimson" => c(220, 20, 60),
        "darkblue" => c(0, 0, 139),
        "darkgray" | "darkgrey" => c(169, 169, 169),
        "darkgreen" => c(0, 100, 0),
        "darkred" => c(139, 0, 0),
        "gold" => c(255, 215, 0),
        "indigo" => c(75, 0, 130),
        "ivory" => c(255, 255, 240),
        "khaki" => c(240, 230, 140),
        "lavender" => c(230, 230, 250),
        "lightblue" => c(173, 216, 230),
        "lightgray" | "lightgrey" => c(211, 211, 211),
        "lightgreen" => c(144, 238, 144),
        "lightyellow" => c(255, 255, 224),
        "linen" => c(250, 240, 230),
        "mintcream" => c(245, 255, 250),
        "mistyrose" => c(255, 228, 225),
        "moccasin" => c(255, 228, 181),
        "oldlace" => c(253, 245, 230),
        "orangered" => c(255, 69, 0),
        "orchid" => c(218, 112, 214),
        "peru" => c(205, 133, 63),
        "plum" => c(221, 160, 221),
        "salmon" => c(250, 128, 114),
        "sienna" => c(160, 82, 45),
        "skyblue" => c(135, 206, 235),
        "slategray" | "slategrey" => c(112, 128, 144),
        "steelblue" => c(70, 130, 180),
        "tan" => c(210, 180, 140),
        "tomato" => c(255, 99, 71),
        "turquoise" => c(64, 224, 208),
        "violet" => c(238, 130, 238),
        "wheat" => c(245, 222, 179),
        "whitesmoke" => c(245, 245, 245),
        "yellowgreen" => c(154, 205, 50),
        "aliceblue" => c(240, 248, 255),
        "antiquewhite" => c(250, 235, 215),
        "aquamarine" => c(127, 255, 212),
        "azure" => c(240, 255, 255),
        "beige" => c(245, 245, 220),
        "bisque" => c(255, 228, 196),
        "blanchedalmond" => c(255, 235, 205),
        "blueviolet" => c(138, 43, 226),
        "burlywood" => c(222, 184, 135),
        "cadetblue" => c(95, 158, 160),
        "chartreuse" => c(127, 255, 0),
        "chocolate" => c(210, 105, 30),
        "cornflowerblue" => c(100, 149, 237),
        "cornsilk" => c(255, 248, 220),
        "darkkhaki" => c(189, 183, 107),
        "darkorange" => c(255, 140, 0),
        "darkorchid" => c(153, 50, 204),
        "darksalmon" => c(233, 150, 122),
        "darkseagreen" => c(143, 188, 143),
        "darkslateblue" => c(72, 61, 139),
        "darkslategray" | "darkslategrey" => c(47, 79, 79),
        "darkturquoise" => c(0, 206, 209),
        "darkviolet" => c(148, 0, 211),
        "deeppink" => c(255, 20, 147),
        "deepskyblue" => c(0, 191, 255),
        "dimgray" | "dimgrey" => c(105, 105, 105),
        "dodgerblue" => c(30, 144, 255),
        "firebrick" => c(178, 34, 34),
        "floralwhite" => c(255, 250, 240),
        "forestgreen" => c(34, 139, 34),
        "gainsboro" => c(220, 220, 220),
        "ghostwhite" => c(248, 248, 255),
        "goldenrod" => c(218, 165, 32),
        "greenyellow" => c(173, 255, 47),
        "honeydew" => c(240, 255, 240),
        "hotpink" => c(255, 105, 180),
        "indianred" => c(205, 92, 92),
        "lawngreen" => c(124, 252, 0),
        "lemonchiffon" => c(255, 250, 205),
        "lightcoral" => c(240, 128, 128),
        "lightcyan" => c(224, 255, 255),
        "lightgoldenrodyellow" => c(250, 250, 210),
        "lightpink" => c(255, 182, 193),
        "lightsalmon" => c(255, 160, 122),
        "lightseagreen" => c(32, 178, 170),
        "lightskyblue" => c(135, 206, 250),
        "lightslategray" | "lightslategrey" => c(119, 136, 153),
        "lightsteelblue" => c(176, 196, 222),
        "limegreen" => c(50, 205, 50),
        "mediumaquamarine" => c(102, 205, 170),
        "mediumblue" => c(0, 0, 205),
        "mediumorchid" => c(186, 85, 211),
        "mediumpurple" => c(147, 111, 219),
        "mediumseagreen" => c(60, 179, 113),
        "mediumslateblue" => c(123, 104, 238),
        "mediumspringgreen" => c(0, 250, 154),
        "mediumturquoise" => c(72, 209, 204),
        "mediumvioletred" => c(199, 21, 133),
        "midnightblue" => c(25, 25, 112),
        "navajowhite" => c(255, 222, 173),
        "olivedrab" => c(107, 142, 35),
        "palegoldenrod" => c(238, 232, 170),
        "palegreen" => c(152, 251, 152),
        "paleturquoise" => c(175, 238, 238),
        "palevioletred" => c(219, 112, 147),
        "papayawhip" => c(255, 239, 213),
        "peachpuff" => c(255, 218, 185),
        "powderblue" => c(176, 224, 230),
        "rosybrown" => c(188, 143, 143),
        "royalblue" => c(65, 105, 225),
        "saddlebrown" => c(139, 69, 19),
        "sandybrown" => c(244, 164, 96),
        "seagreen" => c(46, 139, 87),
        "seashell" => c(255, 245, 238),
        "snow" => c(255, 250, 250),
        "springgreen" => c(0, 255, 127),
        "thistle" => c(216, 191, 216),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors() {
        let c = parse_color("#ff0000").unwrap();
        assert_eq!(c.r, 255);
        assert_eq!(c.g, 0);
        assert_eq!(c.b, 0);
        assert_eq!(c.a, 1.0);

        let c = parse_color("#0f0").unwrap();
        assert_eq!(c.r, 0);
        assert_eq!(c.g, 255);
        assert_eq!(c.b, 0);
    }

    #[test]
    fn rgb_colors() {
        let c = parse_color("rgb(10, 20, 30)").unwrap();
        assert_eq!(c.r, 10);
        assert_eq!(c.g, 20);
        assert_eq!(c.b, 30);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn rgba_colors() {
        let c = parse_color("rgba(10, 20, 30, 0.5)").unwrap();
        assert_eq!(c.r, 10);
        assert_eq!(c.g, 20);
        assert_eq!(c.b, 30);
        assert!((c.a - 0.5).abs() < 0.01);
    }

    #[test]
    fn named_colors() {
        let c = parse_color("red").unwrap();
        assert_eq!(c.r, 255);
        assert_eq!(c.g, 0);
        assert_eq!(c.b, 0);
    }

    #[test]
    fn parse_px_value() {
        match parse_value("width", "100px") {
            StyleValue::Px(v) => assert!((v - 100.0).abs() < 0.01),
            other => panic!("expected Px, got {other:?}"),
        }
    }

    #[test]
    fn parse_percent_value() {
        match parse_value("width", "50%") {
            StyleValue::Percent(v) => assert!((v - 50.0).abs() < 0.01),
            other => panic!("expected Percent, got {other:?}"),
        }
    }

    #[test]
    fn parse_keyword_value() {
        match parse_value("display", "flex") {
            StyleValue::Keyword(k) => assert_eq!(k, "flex"),
            other => panic!("expected Keyword, got {other:?}"),
        }
    }

    #[test]
    fn parse_color_value() {
        match parse_value("color", "#336699") {
            StyleValue::Color(c) => {
                assert_eq!(c.r, 0x33);
                assert_eq!(c.g, 0x66);
                assert_eq!(c.b, 0x99);
            }
            other => panic!("expected Color, got {other:?}"),
        }
    }

    // ── calc() tests ──────────────────────────────────────────────────────────

    #[test]
    fn calc_subtraction_px() {
        // calc(100px - 20px) → Px(80.0)
        match parse_value("width", "calc(100px - 20px)") {
            StyleValue::Px(v) => assert!((v - 80.0).abs() < 0.01, "expected 80, got {v}"),
            other => panic!("expected Px(80), got {other:?}"),
        }
    }

    #[test]
    fn calc_addition_px() {
        // calc(50px + 50px) → Px(100.0)
        match parse_value("width", "calc(50px + 50px)") {
            StyleValue::Px(v) => assert!((v - 100.0).abs() < 0.01, "expected 100, got {v}"),
            other => panic!("expected Px(100), got {other:?}"),
        }
    }

    #[test]
    fn calc_multiplication_px() {
        // calc(100px * 2) → Px(200.0)
        match parse_value("width", "calc(100px * 2)") {
            StyleValue::Px(v) => assert!((v - 200.0).abs() < 0.01, "expected 200, got {v}"),
            other => panic!("expected Px(200), got {other:?}"),
        }
    }

    #[test]
    fn calc_division_px() {
        // calc(200px / 2) → Px(100.0)
        match parse_value("width", "calc(200px / 2)") {
            StyleValue::Px(v) => assert!((v - 100.0).abs() < 0.01, "expected 100, got {v}"),
            other => panic!("expected Px(100), got {other:?}"),
        }
    }

    #[test]
    fn calc_mixed_percent_preserved_as_str() {
        // calc(100% - 20px) — cannot resolve without context, stored as Str.
        match parse_value("width", "calc(100% - 20px)") {
            StyleValue::Str(s) => assert_eq!(s, "calc(100% - 20px)"),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    // ── eval_calc() with context tests ───────────────────────────────────────

    #[test]
    fn eval_calc_percent_minus_px() {
        // calc(100% - 20px) with context 800 → 780.0
        let result = eval_calc("100% - 20px", 800.0);
        assert_eq!(result, Some(780.0));
    }

    #[test]
    fn eval_calc_percent_plus_px() {
        // calc(50% + 10px) with context 400 → 210.0
        let result = eval_calc("50% + 10px", 400.0);
        assert_eq!(result, Some(210.0));
    }

    #[test]
    fn eval_calc_percent_division() {
        // calc(100% / 3) with context 900 → 300.0
        let result = eval_calc("100% / 3", 900.0);
        assert!((result.unwrap() - 300.0).abs() < 0.01);
    }

    #[test]
    fn eval_calc_em_plus_px() {
        // calc(2em + 4px) → 36.0
        let result = eval_calc("2em + 4px", 0.0);
        assert_eq!(result, Some(36.0));
    }

    #[test]
    fn eval_calc_nested_calc() {
        // calc(calc(50%) + 10px) with context 400 → 210.0
        let result = eval_calc("calc(50%) + 10px", 400.0);
        assert_eq!(result, Some(210.0));
    }

    #[test]
    fn eval_calc_operator_precedence() {
        // calc(10px + 20px * 3) → 10 + 60 = 70
        let result = eval_calc("10px + 20px * 3", 0.0);
        assert_eq!(result, Some(70.0));
    }

    #[test]
    fn eval_calc_parentheses() {
        // calc((10px + 20px) * 3) → 90
        let result = eval_calc("(10px + 20px) * 3", 0.0);
        assert_eq!(result, Some(90.0));
    }

    // ── HSL color tests ────────────────────────────────────────────────────

    #[test]
    fn hsl_pure_red() {
        let c = parse_color("hsl(0, 100%, 50%)").unwrap();
        assert_eq!(c.r, 255);
        assert_eq!(c.g, 0);
        assert_eq!(c.b, 0);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn hsl_pure_green() {
        let c = parse_color("hsl(120, 100%, 50%)").unwrap();
        assert_eq!(c.r, 0);
        assert_eq!(c.g, 255);  // might be 128 depending on rounding
        assert!(c.g >= 127); // green channel should be high
        assert_eq!(c.b, 0);
    }

    #[test]
    fn hsl_pure_blue() {
        let c = parse_color("hsl(240, 100%, 50%)").unwrap();
        assert_eq!(c.r, 0);
        assert_eq!(c.g, 0);
        assert_eq!(c.b, 255);
    }

    #[test]
    fn hsla_with_alpha() {
        let c = parse_color("hsla(0, 100%, 50%, 0.5)").unwrap();
        assert_eq!(c.r, 255);
        assert!((c.a - 0.5).abs() < 0.01);
    }

    #[test]
    fn hsl_modern_syntax() {
        let c = parse_color("hsl(120 100% 50%)").unwrap();
        assert!(c.g >= 127);
    }

    #[test]
    fn hsl_achromatic() {
        let c = parse_color("hsl(0, 0%, 50%)").unwrap();
        // Should be gray: r ≈ g ≈ b ≈ 128
        assert!((c.r as i32 - 128).abs() <= 1);
        assert!((c.g as i32 - 128).abs() <= 1);
        assert!((c.b as i32 - 128).abs() <= 1);
    }

    #[test]
    fn eval_calc_vw() {
        // calc(50vw - 10px) with context 1000 → 490.0
        let result = eval_calc("50vw - 10px", 1000.0);
        assert_eq!(result, Some(490.0));
    }

    #[test]
    fn eval_calc_vh() {
        // calc(100vh - 60px) with context 900 → 840.0
        let result = eval_calc("100vh - 60px", 900.0);
        assert_eq!(result, Some(840.0));
    }

    #[test]
    fn eval_calc_mixed_percent_and_px() {
        // calc(50% + 20px) with context 800 → 420.0
        let result = eval_calc("50% + 20px", 800.0);
        assert_eq!(result, Some(420.0));
    }

    // ── clamp(), min(), max() tests ──────────────────────────────────────

    #[test]
    fn clamp_all_px() {
        // clamp(200px, 500px, 800px) → 500px (preferred is in range)
        match parse_value("width", "clamp(200px, 500px, 800px)") {
            StyleValue::Px(v) => assert!((v - 500.0).abs() < 0.01, "expected 500, got {v}"),
            other => panic!("expected Px(500), got {other:?}"),
        }
    }

    #[test]
    fn clamp_preferred_below_min() {
        // clamp(200px, 100px, 800px) → 200px (preferred < min)
        match parse_value("width", "clamp(200px, 100px, 800px)") {
            StyleValue::Px(v) => assert!((v - 200.0).abs() < 0.01, "expected 200, got {v}"),
            other => panic!("expected Px(200), got {other:?}"),
        }
    }

    #[test]
    fn clamp_preferred_above_max() {
        // clamp(200px, 1000px, 800px) → 800px (preferred > max)
        match parse_value("width", "clamp(200px, 1000px, 800px)") {
            StyleValue::Px(v) => assert!((v - 800.0).abs() < 0.01, "expected 800, got {v}"),
            other => panic!("expected Px(800), got {other:?}"),
        }
    }

    #[test]
    fn clamp_with_percent_preserved() {
        // clamp(200px, 50%, 800px) — contains %, preserved as Str
        match parse_value("width", "clamp(200px, 50%, 800px)") {
            StyleValue::Str(s) => assert_eq!(s, "clamp(200px, 50%, 800px)"),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn min_two_px_values() {
        // min(100px, 600px) → 100px
        match parse_value("width", "min(100px, 600px)") {
            StyleValue::Px(v) => assert!((v - 100.0).abs() < 0.01, "expected 100, got {v}"),
            other => panic!("expected Px(100), got {other:?}"),
        }
    }

    #[test]
    fn max_two_px_values() {
        // max(200px, 50px) → 200px
        match parse_value("width", "max(200px, 50px)") {
            StyleValue::Px(v) => assert!((v - 200.0).abs() < 0.01, "expected 200, got {v}"),
            other => panic!("expected Px(200), got {other:?}"),
        }
    }

    #[test]
    fn min_with_percent_preserved() {
        // min(100%, 600px) — contains %, preserved as Str
        match parse_value("width", "min(100%, 600px)") {
            StyleValue::Str(s) => assert_eq!(s, "min(100%, 600px)"),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn eval_math_clamp_with_context() {
        // clamp(200px, 50%, 800px) with context 1000 → 500 (50% of 1000 = 500, in range)
        let result = eval_math_function("clamp(200px, 50%, 800px)", 1000.0);
        assert_eq!(result, Some(500.0));
    }

    #[test]
    fn eval_math_min_with_context() {
        // min(100%, 600px) with context 400 → 400 (100% of 400 = 400 < 600)
        let result = eval_math_function("min(100%, 600px)", 400.0);
        assert_eq!(result, Some(400.0));
    }

    #[test]
    fn eval_math_max_with_context() {
        // max(200px, 50%) with context 300 → 200 (50% of 300 = 150 < 200)
        let result = eval_math_function("max(200px, 50%)", 300.0);
        assert_eq!(result, Some(200.0));
    }

    #[test]
    fn eval_math_nested_min_in_clamp() {
        // clamp(100px, min(400px, 300px), 800px) → 300px
        let result = eval_math_function("clamp(100px, min(400px, 300px), 800px)", 0.0);
        assert_eq!(result, Some(300.0));
    }

    // ── env() tests ──────────────────────────────────────────────────────

    #[test]
    fn env_with_fallback() {
        // env(safe-area-inset-top, 20px) → 20px
        match parse_value("padding-top", "env(safe-area-inset-top, 20px)") {
            StyleValue::Px(v) => assert!((v - 20.0).abs() < 0.01, "expected 20, got {v}"),
            other => panic!("expected Px(20), got {other:?}"),
        }
    }

    #[test]
    fn env_without_fallback() {
        // env(safe-area-inset-top) → 0px
        match parse_value("padding-top", "env(safe-area-inset-top)") {
            StyleValue::Px(v) => assert!((v - 0.0).abs() < 0.01, "expected 0, got {v}"),
            other => panic!("expected Px(0), got {other:?}"),
        }
    }

    #[test]
    fn env_with_percent_fallback() {
        // env(safe-area-inset-bottom, 5%) → Percent(5.0)
        match parse_value("margin-bottom", "env(safe-area-inset-bottom, 5%)") {
            StyleValue::Percent(v) => assert!((v - 5.0).abs() < 0.01, "expected 5%, got {v}"),
            other => panic!("expected Percent(5), got {other:?}"),
        }
    }
}

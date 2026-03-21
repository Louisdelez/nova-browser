//! CSS stylesheet parsing using the `cssparser` crate.
//!
//! Parses CSS text into a list of `CssRule` structs, each containing a selector
//! string and a list of property declarations.

use cssparser::{
    AtRuleParser, BasicParseErrorKind, CowRcStr, DeclarationParser, ParseError, Parser,
    ParserInput, ParserState, QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser,
    StyleSheetParser,
};
use tracing::debug;

use crate::selector::SelectorList;

/// A single declaration (property: value [!important]).
#[derive(Debug, Clone)]
pub struct Declaration {
    pub property: String,
    pub value: String,
    /// `true` when the declaration was marked `!important`.
    pub important: bool,
}

/// A parsed CSS rule: selector + declarations.
#[derive(Debug, Clone)]
pub struct CssRule {
    /// The parsed selector list.
    pub selector: SelectorList,
    /// Property declarations.
    pub declarations: Vec<Declaration>,
}

/// A parsed `@font-face` rule.
///
/// Only `font-family` and `src` are extracted. Font fetching is not performed
/// yet — this is a foundation for future custom-font support.
#[derive(Debug, Clone)]
pub struct FontFaceRule {
    /// The value of the `font-family` descriptor (without quotes).
    pub family: String,
    /// The value of the `src` descriptor (typically a `url(...)` expression).
    pub src: String,
}

/// A parsed `@keyframes` rule.
#[derive(Debug, Clone)]
pub struct KeyframesRule {
    /// The animation name.
    pub name: String,
    /// Keyframe stops: (percentage 0.0–1.0, declarations).
    pub keyframes: Vec<(f32, Vec<Declaration>)>,
}

/// Combined output from `parse_stylesheet_full`.
#[derive(Debug, Clone)]
pub struct ParsedStylesheet {
    /// Qualified CSS rules (selector + declarations).
    pub rules: Vec<CssRule>,
    /// Parsed `@font-face` rules encountered in the stylesheet.
    pub font_faces: Vec<FontFaceRule>,
    /// Parsed `@keyframes` rules.
    pub keyframes: Vec<KeyframesRule>,
}

/// A parsed `@media` query with optional width constraints.
#[derive(Debug, Clone)]
struct MediaQuery {
    /// Minimum viewport width (inclusive) in px.
    min_width: Option<f32>,
    /// Maximum viewport width (inclusive) in px.
    max_width: Option<f32>,
    /// If `true`, the query always evaluates to `false` (e.g. `print`).
    never: bool,
    /// Expected color scheme: Some("dark"), Some("light"), or None (no preference).
    prefers_color_scheme: Option<String>,
}

impl MediaQuery {
    /// Evaluate this query against a viewport width.
    fn evaluate(&self, viewport_width: f32) -> bool {
        if self.never {
            return false;
        }
        if let Some(min) = self.min_width {
            if viewport_width < min {
                return false;
            }
        }
        if let Some(max) = self.max_width {
            if viewport_width > max {
                return false;
            }
        }
        if let Some(ref scheme) = self.prefers_color_scheme {
            // Detect OS color scheme from environment variables (Linux GTK/KDE).
            let os_scheme = if std::env::var("GTK_THEME")
                .map(|t| t.to_lowercase().contains("dark"))
                .unwrap_or(false)
                || std::env::var("QT_STYLE_OVERRIDE")
                    .map(|t| t.to_lowercase().contains("dark"))
                    .unwrap_or(false)
                || std::env::var("COLORFGBG")
                    .map(|t| t.ends_with(";0"))
                    .unwrap_or(false)
                || std::env::var("DESKTOP_SESSION")
                    .map(|t| t.to_lowercase().contains("dark"))
                    .unwrap_or(false)
            {
                "dark"
            } else {
                "light"
            };
            tracing::debug!(
                os_scheme = os_scheme,
                requested_scheme = %scheme,
                "evaluating prefers-color-scheme media query"
            );
            if scheme != os_scheme {
                return false;
            }
        }
        true
    }
}

/// Parse a simple media query string into a `MediaQuery`.
///
/// Supports: `screen`, `all`, `print`, `not print`,
/// `(min-width: Npx)`, `(max-width: Npx)`,
/// `screen and (max-width: Npx)`, and combinations with `and`.
fn parse_media_query(query: &str) -> MediaQuery {
    let query = query.trim();

    // `print` medium — never matches in a screen browser.
    if query.eq_ignore_ascii_case("print") {
        return MediaQuery {
            min_width: None,
            max_width: None,
            never: true,
            prefers_color_scheme: None,
        };
    }

    // `not print` — always matches.
    if query.eq_ignore_ascii_case("not print") {
        return MediaQuery {
            min_width: None,
            max_width: None,
            never: false,
            prefers_color_scheme: None,
        };
    }

    let mut mq = MediaQuery {
        min_width: None,
        max_width: None,
        never: false,
        prefers_color_scheme: None,
    };

    // Split by `and` and parse each condition.
    for part in query.split("and") {
        let part = part.trim();
        // Skip media-type keywords.
        if part.eq_ignore_ascii_case("screen")
            || part.eq_ignore_ascii_case("all")
            || part.is_empty()
        {
            continue;
        }

        // Try to parse `(min-width: Npx)` or `(max-width: Npx)`.
        let inner = part.trim_start_matches('(').trim_end_matches(')').trim();
        if let Some(val_str) = inner.strip_prefix("min-width:").or(inner.strip_prefix("min-width :")) {
            if let Some(px) = parse_px_value(val_str.trim()) {
                mq.min_width = Some(px);
            }
        } else if let Some(val_str) = inner.strip_prefix("max-width:").or(inner.strip_prefix("max-width :")) {
            if let Some(px) = parse_px_value(val_str.trim()) {
                mq.max_width = Some(px);
            }
        } else if let Some(val_str) = inner.strip_prefix("prefers-color-scheme:").or(inner.strip_prefix("prefers-color-scheme :")) {
            let scheme = val_str.trim().to_lowercase();
            if scheme == "dark" || scheme == "light" {
                mq.prefers_color_scheme = Some(scheme);
            }
        }
    }

    mq
}

/// Parse a CSS pixel value like `700px`, `1024px`, or bare number.
fn parse_px_value(s: &str) -> Option<f32> {
    let s = s.trim();
    let num_str = s
        .strip_suffix("px")
        .or_else(|| s.strip_suffix("em")) // treat em as ~16px * value
        .unwrap_or(s);
    let val: f32 = num_str.trim().parse().ok()?;
    if s.ends_with("em") {
        Some(val * 16.0) // rough approximation
    } else {
        Some(val)
    }
}

/// Pre-process CSS text to evaluate `@media` queries and flatten matching blocks.
///
/// Non-matching `@media` blocks are stripped. `@font-face` blocks are extracted
/// into the returned `Vec<FontFaceRule>` list so callers can use them for future
/// font loading. `@keyframes`, `@charset`, and `@import` are stripped.
///
/// Returns `(preprocessed_css, font_face_rules)`.
fn preprocess_media_queries(
    css: &str,
    viewport_width: f32,
) -> (String, Vec<FontFaceRule>) {
    let mut result = String::with_capacity(css.len());
    let mut font_faces: Vec<FontFaceRule> = Vec::new();
    let bytes = css.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'@' {
            // Read the at-rule name.
            let start = i;
            i += 1; // skip '@'
            // Read identifier.
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                i += 1;
            }
            let name = &css[start + 1..i];

            if name.eq_ignore_ascii_case("media") {
                // Read until '{'.
                let query_start = i;
                while i < len && bytes[i] != b'{' {
                    i += 1;
                }
                let query_text = &css[query_start..i];

                // Read the brace-balanced block.
                let block_content = read_brace_block(css, &mut i);
                let query = parse_media_query(query_text);
                if query.evaluate(viewport_width) {
                    // Include the inner rules.
                    result.push_str(&block_content);
                    result.push('\n');
                }
                // else: skip the block entirely.
            } else if name.eq_ignore_ascii_case("font-face") {
                // Parse the @font-face block and extract font-family + src.
                // Skip whitespace up to '{'.
                while i < len && bytes[i] != b'{' {
                    i += 1;
                }
                let block_content = read_brace_block(css, &mut i);
                // Extract font-family and src from the declarations.
                let mut family = String::new();
                let mut src = String::new();
                for decl in block_content.split(';') {
                    let decl = decl.trim();
                    if let Some(rest) = decl.strip_prefix("font-family") {
                        let val = rest.trim_start_matches(':').trim().trim_matches('"').trim_matches('\'');
                        family = val.to_string();
                    } else if let Some(rest) = decl.strip_prefix("src") {
                        let val = rest.trim_start_matches(':').trim();
                        src = val.to_string();
                    }
                }
                if !family.is_empty() || !src.is_empty() {
                    font_faces.push(FontFaceRule { family, src });
                }
                // @font-face is not passed to cssparser — it would be rejected anyway.
            } else if name.eq_ignore_ascii_case("supports") {
                // Read the condition until '{'.
                let query_start = i;
                while i < len && bytes[i] != b'{' {
                    i += 1;
                }
                let condition = &css[query_start..i].trim();

                let block_content = read_brace_block(css, &mut i);

                // Evaluate the @supports condition.
                // We support a simplified version: check if the property is one we know.
                if evaluate_supports_condition(condition) {
                    result.push_str(&block_content);
                    result.push('\n');
                }
            } else if name.eq_ignore_ascii_case("keyframes")
                || name.eq_ignore_ascii_case("charset")
                || name.eq_ignore_ascii_case("import")
            {
                // Skip to end of block or semicolon.
                if name.eq_ignore_ascii_case("charset") || name.eq_ignore_ascii_case("import") {
                    // These end with `;`.
                    while i < len && bytes[i] != b';' {
                        i += 1;
                    }
                    if i < len {
                        i += 1; // skip ';'
                    }
                } else {
                    // Skip brace block.
                    while i < len && bytes[i] != b'{' {
                        i += 1;
                    }
                    let _ = read_brace_block(css, &mut i);
                }
            } else {
                // Unknown at-rule — pass through (will be handled/skipped by cssparser).
                result.push_str(&css[start..i]);
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    (result, font_faces)
}

/// Evaluate a simplified `@supports` condition.
///
/// Supports: `(property: value)`, `not (property: value)`,
/// and `(prop: val) and (prop: val)` combinations.
/// Returns `true` for properties we support, `false` for unknown ones.
fn evaluate_supports_condition(condition: &str) -> bool {
    let condition = condition.trim();

    // Handle `not (...)`
    if let Some(inner) = condition.strip_prefix("not").map(|s| s.trim()) {
        return !evaluate_supports_single(inner);
    }

    // Handle `(prop: value) and (prop: value)`
    // For simplicity, return true if ALL conditions pass.
    let parts: Vec<&str> = condition.split(" and ").collect();
    parts.iter().all(|p| evaluate_supports_single(p.trim()))
}

/// Evaluate a single `@supports` condition like `(display: flex)`.
fn evaluate_supports_single(condition: &str) -> bool {
    let inner = condition
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    if let Some(colon) = inner.find(':') {
        let prop = inner[..colon].trim();
        // We support most common CSS properties.
        let supported = [
            "display", "color", "background-color", "background", "margin", "padding",
            "width", "height", "font-size", "font-weight", "font-style", "font-family",
            "border", "border-radius", "box-shadow", "opacity", "transform", "transition",
            "flex", "flex-direction", "flex-wrap", "align-items", "justify-content",
            "grid", "grid-template-columns", "grid-template-rows", "gap",
            "position", "top", "right", "bottom", "left", "z-index",
            "overflow", "text-align", "text-decoration", "text-transform",
            "max-width", "min-width", "max-height", "min-height",
            "line-height", "letter-spacing", "word-break", "overflow-wrap",
            "box-sizing", "cursor", "visibility", "white-space",
        ];
        supported.contains(&prop)
    } else {
        // Unknown condition format — assume supported.
        true
    }
}

/// Read a brace-balanced block from `css` starting at position `i` (which should
/// point to the opening `{`). Returns the content between the braces (exclusive).
/// Advances `i` past the closing `}`.
fn read_brace_block(css: &str, i: &mut usize) -> String {
    let bytes = css.as_bytes();
    let len = bytes.len();

    if *i >= len || bytes[*i] != b'{' {
        return String::new();
    }

    *i += 1; // skip opening '{'
    let content_start = *i;
    let mut depth = 1u32;

    while *i < len && depth > 0 {
        match bytes[*i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'\'' | b'"' => {
                // Skip string literals to avoid counting braces inside them.
                let quote = bytes[*i];
                *i += 1;
                while *i < len && bytes[*i] != quote {
                    if bytes[*i] == b'\\' {
                        *i += 1; // skip escaped char
                    }
                    *i += 1;
                }
                // *i now points to the closing quote (or end); the loop will advance.
            }
            _ => {}
        }
        if depth > 0 {
            *i += 1;
        }
    }

    let content_end = *i;
    if *i < len {
        *i += 1; // skip closing '}'
    }

    css[content_start..content_end].to_string()
}

/// Parse a CSS stylesheet string into a list of rules.
///
/// `viewport_width` is used to evaluate `@media` queries. Rules inside matching
/// `@media` blocks are flattened into the top-level rule list; non-matching
/// blocks are discarded. `@font-face` rules are extracted but not returned here;
/// use [`parse_stylesheet_full`] if font-face data is needed.
pub fn parse_stylesheet(css: &str, viewport_width: f32) -> Vec<CssRule> {
    let (preprocessed, _font_faces) = preprocess_media_queries(css, viewport_width);
    parse_stylesheet_inner(&preprocessed)
}

/// Parse a CSS stylesheet and return both qualified rules and `@font-face` rules.
///
/// This is the full parse that preserves font-face information for future use.
/// `viewport_width` is used to evaluate `@media` queries.
pub fn parse_stylesheet_full(css: &str, viewport_width: f32) -> ParsedStylesheet {
    let (preprocessed, font_faces) = preprocess_media_queries(css, viewport_width);
    let keyframes = extract_keyframes(css);
    let rules = parse_stylesheet_inner(&preprocessed);
    ParsedStylesheet { rules, font_faces, keyframes }
}

/// Extract `@keyframes` rules from CSS text.
///
/// Parses `@keyframes name { from { ... } to { ... } }` and
/// `@keyframes name { 0% { ... } 50% { ... } 100% { ... } }` forms.
fn extract_keyframes(css: &str) -> Vec<KeyframesRule> {
    let mut result = Vec::new();
    let mut search_from = 0;

    // Case-insensitive search for @keyframes directly on the original string.
    while search_from < css.len() {
        let haystack = &css[search_from..];
        let idx = haystack
            .as_bytes()
            .windows(10)
            .position(|w| w.eq_ignore_ascii_case(b"@keyframes"));
        let Some(idx) = idx else { break };
        let abs_idx = search_from + idx;
        let after = &css[abs_idx + "@keyframes".len()..];
        let after = after.trim_start();

        // Extract animation name.
        let name_end = after.find(|c: char| c == '{' || c.is_whitespace()).unwrap_or(after.len());
        let name = after[..name_end].trim().trim_matches('"').trim_matches('\'').to_string();
        if name.is_empty() {
            search_from = abs_idx + 1;
            continue;
        }

        // Find the opening brace of the keyframes block.
        let Some(brace_start) = after.find('{') else {
            search_from = abs_idx + 1;
            continue;
        };
        let block_start = abs_idx + "@keyframes".len() + brace_start + 1;

        // Find matching closing brace (track depth).
        let mut depth = 1;
        let mut pos = block_start;
        let bytes = css.as_bytes();
        while pos < bytes.len() && depth > 0 {
            match bytes[pos] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            if depth > 0 { pos += 1; }
        }
        let block = &css[block_start..pos];

        // Parse individual keyframe stops from the block.
        let keyframes = parse_keyframe_stops(block);
        if !keyframes.is_empty() {
            result.push(KeyframesRule { name, keyframes });
        }

        search_from = pos + 1;
    }

    result
}

/// Parse keyframe stops like `from { opacity: 0; } 50% { opacity: 0.5; } to { opacity: 1; }`.
fn parse_keyframe_stops(block: &str) -> Vec<(f32, Vec<Declaration>)> {
    let mut stops = Vec::new();
    let mut remaining = block.trim();

    while !remaining.is_empty() {
        remaining = remaining.trim_start();
        if remaining.is_empty() { break; }

        // Read the stop selector (e.g., "from", "to", "50%").
        let brace = remaining.find('{');
        let Some(brace_idx) = brace else { break };
        let selector = remaining[..brace_idx].trim();
        let pct = match selector {
            "from" => 0.0,
            "to" => 1.0,
            s if s.ends_with('%') => {
                s[..s.len()-1].trim().parse::<f32>().unwrap_or(0.0) / 100.0
            }
            _ => { remaining = &remaining[brace_idx + 1..]; continue; }
        };

        // Find the closing brace.
        let after_brace = &remaining[brace_idx + 1..];
        let close = after_brace.find('}').unwrap_or(after_brace.len());
        let decl_str = &after_brace[..close];

        // Parse declarations.
        let decls = parse_inline_style(decl_str);
        stops.push((pct, decls));

        remaining = if close < after_brace.len() { &after_brace[close + 1..] } else { "" };
    }

    stops
}

/// Parse a CSS stylesheet string into a list of rules (internal, no media query handling).
fn parse_stylesheet_inner(css: &str) -> Vec<CssRule> {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    let mut rule_parser = NovaRuleParser;

    let iter = StyleSheetParser::new(&mut parser, &mut rule_parser);
    let mut rules = Vec::new();

    for result in iter {
        match result {
            Ok(rule) => rules.push(rule),
            Err((err, _slice)) => {
                debug!("CSS parse error (skipping rule): {:?}", err);
            }
        }
    }

    rules
}

/// Parse an inline style string (e.g., `color: red; font-size: 16px`) into declarations.
pub fn parse_inline_style(style: &str) -> Vec<Declaration> {
    let mut input = ParserInput::new(style);
    let mut parser = Parser::new(&mut input);
    let mut decl_parser = NovaDeclarationParser;

    let iter = RuleBodyParser::new(&mut parser, &mut decl_parser);
    let mut decls = Vec::new();

    for result in iter {
        match result {
            Ok(decl) => decls.push(decl),
            Err((err, _slice)) => {
                debug!("CSS inline style parse error: {:?}", err);
            }
        }
    }

    decls
}

// ── cssparser trait implementations ──────────────────────────────────

/// Top-level rule parser for stylesheets.
struct NovaRuleParser;

impl<'i> AtRuleParser<'i> for NovaRuleParser {
    type Prelude = ();
    type AtRule = CssRule;
    type Error = ();

    // We skip at-rules for now (e.g., @media, @import).
    // The default implementations reject them, which is what we want.
}

impl<'i> QualifiedRuleParser<'i> for NovaRuleParser {
    type Prelude = String;
    type QualifiedRule = CssRule;
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        // Consume all tokens until the block starts — this is the selector text.
        let start = input.position();
        // Skip to end of prelude.
        while input.next().is_ok() {}
        let selector_text = input.slice_from(start);
        Ok(selector_text.trim().to_string())
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Self::Prelude,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::QualifiedRule, ParseError<'i, Self::Error>> {
        let selector = match SelectorList::parse(&prelude) {
            Some(s) => s,
            None => {
                return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid));
            }
        };

        let declarations = parse_declaration_block(input);

        Ok(CssRule {
            selector,
            declarations,
        })
    }
}

/// Parse declarations inside a `{ ... }` block.
fn parse_declaration_block<'i>(input: &mut Parser<'i, '_>) -> Vec<Declaration> {
    let mut decl_parser = NovaDeclarationParser;
    let iter = RuleBodyParser::new(input, &mut decl_parser);
    let mut decls = Vec::new();

    for result in iter {
        match result {
            Ok(decl) => decls.push(decl),
            Err(_) => {} // Skip invalid declarations.
        }
    }

    decls
}

/// Declaration parser shared between stylesheet rules and inline styles.
struct NovaDeclarationParser;

impl<'i> DeclarationParser<'i> for NovaDeclarationParser {
    type Declaration = Declaration;
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Declaration, ParseError<'i, Self::Error>> {
        let raw_name = name.to_string();
        // Custom properties (--*) are case-sensitive; standard properties are lowercased.
        let property = if raw_name.starts_with("--") {
            raw_name
        } else {
            raw_name.to_ascii_lowercase()
        };
        let start = input.position();
        // Consume all remaining tokens for this declaration.
        while input.next().is_ok() {}
        let raw = input.slice_from(start).trim().to_string();
        // Detect and strip `!important`.
        let (value, important) = if let Some(v) = raw.strip_suffix("!important") {
            (v.trim().to_string(), true)
        } else {
            (raw, false)
        };

        Ok(Declaration {
            property,
            value,
            important,
        })
    }
}

impl<'i> AtRuleParser<'i> for NovaDeclarationParser {
    type Prelude = ();
    type AtRule = Declaration;
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for NovaDeclarationParser {
    type Prelude = ();
    type QualifiedRule = Declaration;
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, Declaration, ()> for NovaDeclarationParser {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default viewport width for tests.
    const TEST_VP: f32 = 1280.0;

    #[test]
    fn parse_simple_stylesheet() {
        let css = r#"
            body { color: black; background-color: white; }
            h1 { font-size: 32px; font-weight: bold; }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 2);
        assert!(!rules[0].declarations.is_empty());
        assert!(!rules[1].declarations.is_empty());
    }

    #[test]
    fn parse_class_selector() {
        let css = ".container { max-width: 960px; margin: 0 auto; }";
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 1);
        assert!(rules[0].selector.selectors[0].parts[0].classes.contains(&"container".to_string()));
    }

    #[test]
    fn parse_multiple_selectors() {
        let css = "h1, h2, h3 { font-weight: bold; }";
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].selector.selectors.len(), 3);
    }

    #[test]
    fn parse_inline_style_basic() {
        let decls = parse_inline_style("color: red; font-size: 16px");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].property, "color");
        assert_eq!(decls[0].value, "red");
        assert!(!decls[0].important);
        assert_eq!(decls[1].property, "font-size");
        assert_eq!(decls[1].value, "16px");
        assert!(!decls[1].important);
    }

    #[test]
    fn parse_inline_style_important() {
        let decls = parse_inline_style("color: red !important; font-size: 16px");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].property, "color");
        assert_eq!(decls[0].value, "red");
        assert!(decls[0].important, "color should be marked !important");
        assert_eq!(decls[1].property, "font-size");
        assert!(!decls[1].important);
    }

    #[test]
    fn parse_descendant_selector() {
        let css = "div p { color: blue; }";
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].selector.selectors[0].parts.len(), 2);
    }

    #[test]
    fn skips_invalid_declarations() {
        let css = "body { color: ; background: white; }";
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 1);
        // At least the valid declaration should be there.
        assert!(!rules[0].declarations.is_empty());
    }

    #[test]
    fn example_com_style() {
        // Approximation of example.com's stylesheet.
        let css = r#"
            body {
                background-color: #f0f0f2;
                margin: 0;
                padding: 0;
                font-family: -apple-system, system-ui, BlinkMacSystemFont, "Segoe UI", "Open Sans", "Helvetica Neue", Helvetica, Arial, sans-serif;
            }
            div {
                width: 600px;
                margin: 5em auto;
                padding: 2em;
                background-color: #fdfdff;
                border-radius: 0.5em;
                box-shadow: 2px 3px 7px 2px rgba(0,0,0,0.02);
            }
            a:link, a:visited {
                color: #38488f;
                text-decoration: none;
            }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        assert!(rules.len() >= 2); // body and div at minimum
    }

    // ── @media query tests ─────────────────────────────────────────────

    #[test]
    fn media_query_max_width_matching() {
        let css = r#"
            body { color: black; }
            @media (max-width: 700px) {
                div { width: auto; }
            }
        "#;
        // Viewport 500px — should include the media rule.
        let rules = parse_stylesheet(css, 500.0);
        assert_eq!(rules.len(), 2, "expected 2 rules at 500px viewport");

        // Viewport 1024px — should skip the media rule.
        let rules = parse_stylesheet(css, 1024.0);
        assert_eq!(rules.len(), 1, "expected 1 rule at 1024px viewport");
    }

    #[test]
    fn media_query_min_width_matching() {
        let css = r#"
            body { color: black; }
            @media (min-width: 768px) {
                .sidebar { display: block; }
            }
        "#;
        // Viewport 1024px — should include.
        let rules = parse_stylesheet(css, 1024.0);
        assert_eq!(rules.len(), 2);

        // Viewport 500px — should skip.
        let rules = parse_stylesheet(css, 500.0);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn media_query_screen_and_max_width() {
        let css = r#"
            body { color: black; }
            @media screen and (max-width: 1024px) {
                div { margin: 0 auto; width: auto; }
            }
        "#;
        let rules = parse_stylesheet(css, 800.0);
        assert_eq!(rules.len(), 2);

        let rules = parse_stylesheet(css, 1280.0);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn media_query_print_never_matches() {
        let css = r#"
            body { color: black; }
            @media print {
                body { color: white; }
            }
        "#;
        let rules = parse_stylesheet(css, 1280.0);
        assert_eq!(rules.len(), 1, "print media should be skipped");
    }

    #[test]
    fn media_query_multiple_blocks() {
        let css = r#"
            body { color: black; }
            @media (max-width: 700px) {
                .mobile { display: block; }
            }
            h1 { font-size: 32px; }
            @media (min-width: 1200px) {
                .desktop { display: flex; }
            }
        "#;
        // At 500px: body + .mobile + h1 = 3 rules
        let rules = parse_stylesheet(css, 500.0);
        assert_eq!(rules.len(), 3);

        // At 1280px: body + h1 + .desktop = 3 rules
        let rules = parse_stylesheet(css, 1280.0);
        assert_eq!(rules.len(), 3);

        // At 900px: body + h1 = 2 rules
        let rules = parse_stylesheet(css, 900.0);
        assert_eq!(rules.len(), 2);
    }

    #[test]
    fn media_query_example_com() {
        let css = r#"
            body { background-color: #f0f0f2; }
            div { width: 600px; }
            @media (max-width: 700px) {
                div { margin: 0 auto; width: auto; }
            }
        "#;
        // Wide viewport — media block skipped.
        let rules = parse_stylesheet(css, 1280.0);
        assert_eq!(rules.len(), 2);

        // Narrow viewport — media block included.
        let rules = parse_stylesheet(css, 500.0);
        assert_eq!(rules.len(), 3);
        // The last rule should be the div override from the media block.
        let last = &rules[2];
        assert!(last.declarations.iter().any(|d| d.property == "width" && d.value == "auto"));
    }

    // ── Phase 6C: @font-face tests ──────────────────────────────────────

    #[test]
    fn font_face_extracted() {
        let css = r#"
            @font-face {
                font-family: "MyFont";
                src: url("/fonts/myfont.woff2");
            }
            body { color: black; }
        "#;
        let sheet = parse_stylesheet_full(css, TEST_VP);
        // The body rule should still be present.
        assert_eq!(sheet.rules.len(), 1, "body rule should be present");
        // The font-face should be extracted.
        assert_eq!(sheet.font_faces.len(), 1, "one @font-face should be extracted");
        assert_eq!(sheet.font_faces[0].family, "MyFont");
        assert!(sheet.font_faces[0].src.contains("myfont.woff2"));
    }

    #[test]
    fn font_face_does_not_break_rules() {
        // @font-face between two regular rules — both rules should be parsed.
        let css = r#"
            h1 { font-size: 32px; }
            @font-face {
                font-family: "Custom";
                src: url("/fonts/custom.woff2");
            }
            p { color: blue; }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 2, "h1 and p rules should both be present");
    }

    // ── prefers-color-scheme tests ─────────────────────────────────────

    #[test]
    fn media_prefers_color_scheme_light() {
        let css = r#"
            body { color: black; }
            @media (prefers-color-scheme: light) {
                body { background: white; }
            }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        // Should have 1 or 2 rules depending on OS theme (light=2, dark=1).
        assert!(rules.len() >= 1 && rules.len() <= 2, "expected 1 or 2 rules, got {}", rules.len());
    }

    #[test]
    fn media_prefers_color_scheme_dark() {
        let css = r#"
            body { color: black; }
            @media (prefers-color-scheme: dark) {
                body { background: #333; }
            }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        // Should have 1 or 2 rules depending on OS theme (dark=2, light=1).
        assert!(rules.len() >= 1 && rules.len() <= 2, "expected 1 or 2 rules, got {}", rules.len());
    }

    #[test]
    fn multiple_font_faces() {
        let css = r#"
            @font-face { font-family: "FontA"; src: url("/a.woff2"); }
            @font-face { font-family: "FontB"; src: url("/b.woff2"); }
            body { margin: 0; }
        "#;
        let sheet = parse_stylesheet_full(css, TEST_VP);
        assert_eq!(sheet.font_faces.len(), 2);
        assert_eq!(sheet.rules.len(), 1);
    }

    // ── @supports tests ────────────────────────────────────────────────

    #[test]
    fn supports_display_flex_included() {
        let css = r#"
            body { color: black; }
            @supports (display: flex) {
                .flex-container { display: flex; }
            }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 2, "display: flex is supported, rule should be included");
    }

    #[test]
    fn supports_unknown_property_excluded() {
        let css = r#"
            body { color: black; }
            @supports (container-type: inline-size) {
                .container { width: 100%; }
            }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 1, "unknown property should exclude the @supports block");
    }

    #[test]
    fn supports_not_condition() {
        let css = r#"
            body { color: black; }
            @supports not (container-type: inline-size) {
                .fallback { width: auto; }
            }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 2, "not (unknown) should include the block");
    }

    #[test]
    fn supports_and_condition() {
        let css = r#"
            body { color: black; }
            @supports (display: grid) and (gap: 10px) {
                .grid { display: grid; gap: 10px; }
            }
        "#;
        let rules = parse_stylesheet(css, TEST_VP);
        assert_eq!(rules.len(), 2, "both properties supported, block should be included");
    }
}

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

/// A parsed CSS rule: selector + declarations.
#[derive(Debug, Clone)]
pub struct CssRule {
    /// The parsed selector list.
    pub selector: SelectorList,
    /// Property declarations: (property-name, value-string).
    pub declarations: Vec<(String, String)>,
}

/// A single declaration (property: value).
#[derive(Debug, Clone)]
pub struct Declaration {
    pub property: String,
    pub value: String,
}

/// Parse a CSS stylesheet string into a list of rules.
pub fn parse_stylesheet(css: &str) -> Vec<CssRule> {
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
pub fn parse_inline_style(style: &str) -> Vec<(String, String)> {
    let mut input = ParserInput::new(style);
    let mut parser = Parser::new(&mut input);
    let mut decl_parser = NovaDeclarationParser;

    let iter = RuleBodyParser::new(&mut parser, &mut decl_parser);
    let mut decls = Vec::new();

    for result in iter {
        match result {
            Ok(decl) => decls.push((decl.property, decl.value)),
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
fn parse_declaration_block<'i>(input: &mut Parser<'i, '_>) -> Vec<(String, String)> {
    let mut decl_parser = NovaDeclarationParser;
    let iter = RuleBodyParser::new(input, &mut decl_parser);
    let mut decls = Vec::new();

    for result in iter {
        match result {
            Ok(decl) => decls.push((decl.property, decl.value)),
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
        let property = name.to_string().to_ascii_lowercase();
        let start = input.position();
        // Consume all remaining tokens for this declaration.
        while input.next().is_ok() {}
        let value = input.slice_from(start).trim().to_string();
        // Strip !important if present.
        let value = value
            .strip_suffix("!important")
            .map(|v| v.trim().to_string())
            .unwrap_or(value);

        Ok(Declaration { property, value })
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

    #[test]
    fn parse_simple_stylesheet() {
        let css = r#"
            body { color: black; background-color: white; }
            h1 { font-size: 32px; font-weight: bold; }
        "#;
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 2);
        assert!(!rules[0].declarations.is_empty());
        assert!(!rules[1].declarations.is_empty());
    }

    #[test]
    fn parse_class_selector() {
        let css = ".container { max-width: 960px; margin: 0 auto; }";
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 1);
        assert!(rules[0].selector.selectors[0].parts[0].classes.contains(&"container".to_string()));
    }

    #[test]
    fn parse_multiple_selectors() {
        let css = "h1, h2, h3 { font-weight: bold; }";
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].selector.selectors.len(), 3);
    }

    #[test]
    fn parse_inline_style_basic() {
        let decls = parse_inline_style("color: red; font-size: 16px");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].0, "color");
        assert_eq!(decls[0].1, "red");
        assert_eq!(decls[1].0, "font-size");
        assert_eq!(decls[1].1, "16px");
    }

    #[test]
    fn parse_descendant_selector() {
        let css = "div p { color: blue; }";
        let rules = parse_stylesheet(css);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].selector.selectors[0].parts.len(), 2);
    }

    #[test]
    fn skips_invalid_declarations() {
        let css = "body { color: ; background: white; }";
        let rules = parse_stylesheet(css);
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
        let rules = parse_stylesheet(css);
        assert!(rules.len() >= 2); // body and div at minimum
    }
}

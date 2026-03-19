//! User-agent default styles.
//!
//! Provides sensible defaults for HTML elements, equivalent to a basic
//! browser user-agent stylesheet.

use nova_mod_api::content::{CssColor, StyleMap, StyleValue};

/// Return the default (user-agent) `StyleMap` for a given HTML tag.
pub fn default_style_for_tag(tag: &str) -> StyleMap {
    let mut props = Vec::new();

    // Display property.
    let display = display_for_tag(tag);
    props.push(("display".into(), StyleValue::Keyword(display.into())));

    // Default colors.
    props.push((
        "color".into(),
        StyleValue::Color(CssColor {
            r: 0,
            g: 0,
            b: 0,
            a: 1.0,
        }),
    ));
    props.push((
        "background-color".into(),
        StyleValue::Color(CssColor {
            r: 255,
            g: 255,
            b: 255,
            a: 0.0, // transparent by default
        }),
    ));

    // Font sizes for headings.
    let font_size = match tag {
        "h1" => 32.0,
        "h2" => 24.0,
        "h3" => 18.72,
        "h4" => 16.0,
        "h5" => 13.28,
        "h6" => 10.72,
        _ => 16.0,
    };
    props.push(("font-size".into(), StyleValue::Px(font_size)));

    // Bold for headings and strong/b.
    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "strong" | "b" => {
            props.push(("font-weight".into(), StyleValue::Keyword("bold".into())));
        }
        _ => {
            props.push(("font-weight".into(), StyleValue::Keyword("normal".into())));
        }
    }

    // Italic for em/i.
    if tag == "em" || tag == "i" {
        props.push(("font-style".into(), StyleValue::Keyword("italic".into())));
    }

    // Default margins for headings and paragraphs.
    match tag {
        "h1" => {
            props.push(("margin-top".into(), StyleValue::Px(21.44)));
            props.push(("margin-bottom".into(), StyleValue::Px(21.44)));
        }
        "h2" => {
            props.push(("margin-top".into(), StyleValue::Px(19.92)));
            props.push(("margin-bottom".into(), StyleValue::Px(19.92)));
        }
        "h3" => {
            props.push(("margin-top".into(), StyleValue::Px(18.72)));
            props.push(("margin-bottom".into(), StyleValue::Px(18.72)));
        }
        "p" => {
            props.push(("margin-top".into(), StyleValue::Px(16.0)));
            props.push(("margin-bottom".into(), StyleValue::Px(16.0)));
        }
        "body" => {
            props.push(("margin-top".into(), StyleValue::Px(8.0)));
            props.push(("margin-right".into(), StyleValue::Px(8.0)));
            props.push(("margin-bottom".into(), StyleValue::Px(8.0)));
            props.push(("margin-left".into(), StyleValue::Px(8.0)));
        }
        "ul" | "ol" => {
            props.push(("margin-top".into(), StyleValue::Px(16.0)));
            props.push(("margin-bottom".into(), StyleValue::Px(16.0)));
            props.push(("padding-left".into(), StyleValue::Px(40.0)));
        }
        _ => {}
    }

    // Links.
    if tag == "a" {
        props.push((
            "color".into(),
            StyleValue::Color(CssColor {
                r: 0,
                g: 0,
                b: 238,
                a: 1.0,
            }),
        ));
        props.push((
            "text-decoration".into(),
            StyleValue::Keyword("underline".into()),
        ));
    }

    StyleMap { properties: props }
}

/// Get the default display type for a tag.
pub fn display_for_tag(tag: &str) -> &'static str {
    match tag {
        "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "body" | "html" | "section"
        | "article" | "header" | "footer" | "nav" | "main" | "ul" | "ol" | "li"
        | "blockquote" | "pre" | "form" | "table" | "hr" | "figure" | "figcaption"
        | "details" | "summary" | "dialog" | "address" | "fieldset" | "dd" | "dt" | "dl"
        | "hgroup" | "search" => "block",
        "span" | "a" | "em" | "strong" | "b" | "i" | "u" | "code" | "small" | "sub" | "sup"
        | "br" | "img" | "input" | "label" | "select" | "textarea" | "button" | "abbr"
        | "cite" | "dfn" | "kbd" | "mark" | "q" | "s" | "samp" | "time" | "var" | "wbr" => {
            "inline"
        }
        "head" | "title" | "meta" | "link" | "style" | "script" | "template" => "none",
        _ => "block",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h1_defaults() {
        let style = default_style_for_tag("h1");
        let display = style
            .properties
            .iter()
            .find(|(k, _)| k == "display")
            .unwrap();
        assert!(matches!(&display.1, StyleValue::Keyword(k) if k == "block"));

        let fs = style
            .properties
            .iter()
            .find(|(k, _)| k == "font-size")
            .unwrap();
        assert!(matches!(&fs.1, StyleValue::Px(v) if (*v - 32.0).abs() < 0.01));
    }

    #[test]
    fn inline_elements() {
        for tag in &["span", "a", "em", "strong"] {
            let style = default_style_for_tag(tag);
            let display = style
                .properties
                .iter()
                .find(|(k, _)| k == "display")
                .unwrap();
            assert!(
                matches!(&display.1, StyleValue::Keyword(k) if k == "inline"),
                "{tag} should be inline"
            );
        }
    }

    #[test]
    fn body_has_margins() {
        let style = default_style_for_tag("body");
        let margin = style
            .properties
            .iter()
            .find(|(k, _)| k == "margin-top")
            .unwrap();
        assert!(matches!(&margin.1, StyleValue::Px(v) if (*v - 8.0).abs() < 0.01));
    }
}

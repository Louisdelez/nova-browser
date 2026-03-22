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

    // Default colors — only set color on root elements so that CSS
    // inheritance works for all other elements.  `<a>` overrides to blue
    // further below.
    match tag {
        "html" | "body" => {
            props.push((
                "color".into(),
                StyleValue::Color(CssColor {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 1.0,
                }),
            ));
        }
        _ => {}
    }

    // Background-color: `<html>` defaults to white (the canvas background),
    // all other elements default to transparent.
    match tag {
        "html" => {
            props.push((
                "background-color".into(),
                StyleValue::Color(CssColor {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 1.0, // opaque white — the page canvas
                }),
            ));
        }
        _ => {
            props.push((
                "background-color".into(),
                StyleValue::Color(CssColor {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 0.0, // transparent by default
                }),
            ));
        }
    }

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
        "ul" => {
            props.push(("margin-top".into(), StyleValue::Px(16.0)));
            props.push(("margin-bottom".into(), StyleValue::Px(16.0)));
            props.push(("padding-left".into(), StyleValue::Px(40.0)));
            props.push(("list-style-type".into(), StyleValue::Keyword("disc".into())));
        }
        "ol" => {
            props.push(("margin-top".into(), StyleValue::Px(16.0)));
            props.push(("margin-bottom".into(), StyleValue::Px(16.0)));
            props.push(("padding-left".into(), StyleValue::Px(40.0)));
            props.push(("list-style-type".into(), StyleValue::Keyword("decimal".into())));
        }
        _ => {}
    }

    // <pre> and <code> — monospace / light background.
    match tag {
        "pre" => {
            props.push(("white-space".into(), StyleValue::Keyword("pre".into())));
            props.push(("font-family".into(), StyleValue::Keyword("monospace".into())));
            props.push((
                "background-color".into(),
                StyleValue::Color(CssColor {
                    r: 245,
                    g: 245,
                    b: 245,
                    a: 1.0,
                }),
            ));
            props.push(("padding-top".into(), StyleValue::Px(8.0)));
            props.push(("padding-right".into(), StyleValue::Px(8.0)));
            props.push(("padding-bottom".into(), StyleValue::Px(8.0)));
            props.push(("padding-left".into(), StyleValue::Px(8.0)));
            props.push(("margin-top".into(), StyleValue::Px(16.0)));
            props.push(("margin-bottom".into(), StyleValue::Px(16.0)));
            props.push(("overflow-x".into(), StyleValue::Keyword("auto".into())));
        }
        "code" | "kbd" | "samp" => {
            props.push(("font-family".into(), StyleValue::Keyword("monospace".into())));
            props.push((
                "background-color".into(),
                StyleValue::Color(CssColor {
                    r: 245,
                    g: 245,
                    b: 245,
                    a: 1.0,
                }),
            ));
        }
        _ => {}
    }

    // <blockquote> — left and right margin indent.
    if tag == "blockquote" {
        props.push(("margin-left".into(), StyleValue::Px(40.0)));
        props.push(("margin-right".into(), StyleValue::Px(40.0)));
        props.push(("margin-top".into(), StyleValue::Px(16.0)));
        props.push(("margin-bottom".into(), StyleValue::Px(16.0)));
    }

    // <hr> — visible divider line.
    if tag == "hr" {
        props.push(("border-top-style".into(), StyleValue::Keyword("solid".into())));
        props.push(("border-top-width".into(), StyleValue::Px(1.0)));
        props.push(("border-top-color".into(), StyleValue::Color(CssColor {
            r: 204, g: 204, b: 204, a: 1.0,
        })));
        props.push(("height".into(), StyleValue::Px(2.0)));
        props.push(("margin-top".into(), StyleValue::Px(8.0)));
        props.push(("margin-bottom".into(), StyleValue::Px(8.0)));
        props.push(("overflow".into(), StyleValue::Keyword("hidden".into())));
    }

    // Table elements — display modes and defaults.
    match tag {
        "table" => {
            props.push(("border-collapse".into(), StyleValue::Keyword("collapse".into())));
        }
        "tr" => {
            // Override display to table-row so layout uses flex-row.
            if let Some(d) = props.iter_mut().find(|(k, _)| k == "display") {
                d.1 = StyleValue::Keyword("table-row".into());
            }
        }
        "td" | "th" => {
            // Table cells need padding and display override.
            if let Some(d) = props.iter_mut().find(|(k, _)| k == "display") {
                d.1 = StyleValue::Keyword("table-cell".into());
            }
            props.push(("padding-top".into(), StyleValue::Px(1.0)));
            props.push(("padding-right".into(), StyleValue::Px(4.0)));
            props.push(("padding-bottom".into(), StyleValue::Px(1.0)));
            props.push(("padding-left".into(), StyleValue::Px(4.0)));
        }
        _ => {}
    }

    // <sub> and <sup> — smaller font-size and vertical-align.
    if tag == "sub" {
        // Override the generic 16px with a smaller size.
        if let Some(existing) = props.iter_mut().find(|(k, _)| k == "font-size") {
            existing.1 = StyleValue::Px(13.0);
        }
        props.push(("vertical-align".into(), StyleValue::Keyword("sub".into())));
    }
    if tag == "sup" {
        if let Some(existing) = props.iter_mut().find(|(k, _)| k == "font-size") {
            existing.1 = StyleValue::Px(13.0);
        }
        props.push(("vertical-align".into(), StyleValue::Keyword("super".into())));
    }

    // Form elements — default styles for input, select, button, textarea.
    match tag {
        "input" => {
            props.push(("display".into(), StyleValue::Keyword("inline-block".into())));
            props.push(("border-width".into(), StyleValue::Px(1.0)));
            props.push(("border-style".into(), StyleValue::Keyword("solid".into())));
            props.push(("border-color".into(), StyleValue::Str("#767676".into())));
            props.push(("padding-top".into(), StyleValue::Px(2.0)));
            props.push(("padding-right".into(), StyleValue::Px(4.0)));
            props.push(("padding-bottom".into(), StyleValue::Px(2.0)));
            props.push(("padding-left".into(), StyleValue::Px(4.0)));
            props.push(("background-color".into(), StyleValue::Color(CssColor { r: 255, g: 255, b: 255, a: 1.0 })));
        }
        "button" => {
            props.push(("display".into(), StyleValue::Keyword("inline-block".into())));
            props.push(("border-width".into(), StyleValue::Px(1.0)));
            props.push(("border-style".into(), StyleValue::Keyword("solid".into())));
            props.push(("border-color".into(), StyleValue::Str("#767676".into())));
            props.push(("padding-top".into(), StyleValue::Px(2.0)));
            props.push(("padding-right".into(), StyleValue::Px(8.0)));
            props.push(("padding-bottom".into(), StyleValue::Px(2.0)));
            props.push(("padding-left".into(), StyleValue::Px(8.0)));
            props.push(("background-color".into(), StyleValue::Color(CssColor { r: 239, g: 239, b: 239, a: 1.0 })));
            props.push(("text-align".into(), StyleValue::Keyword("center".into())));
        }
        "select" => {
            props.push(("display".into(), StyleValue::Keyword("inline-block".into())));
            props.push(("border-width".into(), StyleValue::Px(1.0)));
            props.push(("border-style".into(), StyleValue::Keyword("solid".into())));
            props.push(("border-color".into(), StyleValue::Str("#767676".into())));
            props.push(("padding-top".into(), StyleValue::Px(2.0)));
            props.push(("padding-right".into(), StyleValue::Px(4.0)));
            props.push(("padding-bottom".into(), StyleValue::Px(2.0)));
            props.push(("padding-left".into(), StyleValue::Px(4.0)));
            props.push(("background-color".into(), StyleValue::Color(CssColor { r: 255, g: 255, b: 255, a: 1.0 })));
        }
        "textarea" => {
            props.push(("display".into(), StyleValue::Keyword("inline-block".into())));
            props.push(("border-width".into(), StyleValue::Px(1.0)));
            props.push(("border-style".into(), StyleValue::Keyword("solid".into())));
            props.push(("border-color".into(), StyleValue::Str("#767676".into())));
            props.push(("padding-top".into(), StyleValue::Px(2.0)));
            props.push(("padding-right".into(), StyleValue::Px(4.0)));
            props.push(("padding-bottom".into(), StyleValue::Px(2.0)));
            props.push(("padding-left".into(), StyleValue::Px(4.0)));
            props.push(("background-color".into(), StyleValue::Color(CssColor { r: 255, g: 255, b: 255, a: 1.0 })));
        }
        // <center> — legacy centering element.
        "center" => {
            props.push(("text-align".into(), StyleValue::Keyword("center".into())));
            props.push(("margin-left".into(), StyleValue::Keyword("auto".into())));
            props.push(("margin-right".into(), StyleValue::Keyword("auto".into())));
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
        | "blockquote" | "pre" | "form" | "hr" | "figure" | "figcaption" | "center"
        | "details" | "summary" | "dialog" | "address" | "fieldset" | "dd" | "dt" | "dl"
        | "hgroup" | "search" => "block",
        // Table elements
        "table" | "thead" | "tbody" | "tfoot" => "block",
        "tr" => "table-row",
        "td" | "th" => "table-cell",
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

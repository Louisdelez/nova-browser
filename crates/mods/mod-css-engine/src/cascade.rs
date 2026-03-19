//! Style cascade and resolution.
//!
//! Walks the DOM tree, collects matching CSS rules for each element, sorts
//! them by specificity, applies user-agent defaults, stylesheet rules, and
//! inline styles, then writes the computed styles into `data-nova-style`
//! attributes on each element.

use nova_mod_api::content::DomNode;
use tracing::debug;

use crate::defaults::default_style_for_tag;
use crate::parser::{self, CssRule};
use crate::selector::Specificity;
use crate::values;

/// A declaration with its origin and specificity, used for cascade sorting.
#[derive(Debug, Clone)]
struct CascadedDeclaration {
    property: String,
    value: String,
    specificity: Specificity,
    origin: CascadeOrigin,
}

/// Origin of a CSS declaration in the cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CascadeOrigin {
    /// User-agent defaults (lowest priority).
    UserAgent = 0,
    /// Author stylesheet (from `<style>` elements).
    AuthorStylesheet = 1,
    /// Inline style attribute (highest priority).
    Inline = 2,
}

/// Extract CSS from `<style>` elements in the DOM.
pub fn extract_stylesheets(node: &DomNode) -> Vec<String> {
    let mut sheets = Vec::new();
    extract_stylesheets_recursive(node, &mut sheets);
    sheets
}

fn extract_stylesheets_recursive(node: &DomNode, sheets: &mut Vec<String>) {
    match node {
        DomNode::Element {
            tag, children, ..
        } => {
            if tag == "style" {
                // Collect text content from children.
                let mut css_text = String::new();
                for child in children {
                    if let DomNode::Text(text) = child {
                        css_text.push_str(text);
                    }
                }
                if !css_text.trim().is_empty() {
                    sheets.push(css_text);
                }
            }
            for child in children {
                extract_stylesheets_recursive(child, sheets);
            }
        }
        DomNode::Document { children } => {
            for child in children {
                extract_stylesheets_recursive(child, sheets);
            }
        }
        _ => {}
    }
}

/// Compute styles for an entire DOM tree and write them as `data-nova-style` attributes.
///
/// This is the main entry point for the cascade. It:
/// 1. Extracts `<style>` elements from the DOM.
/// 2. Parses the CSS.
/// 3. Walks the DOM tree and for each element computes the final style.
/// 4. Serializes the style as an inline CSS string on `data-nova-style`.
///
/// Additionally accepts pre-parsed external stylesheets from the `stylesheets` parameter.
pub fn compute_styles(dom: DomNode, extra_css: &[String]) -> DomNode {
    // 1. Extract embedded stylesheets.
    let embedded = extract_stylesheets(&dom);

    // 2. Parse all stylesheets.
    let mut rules = Vec::new();
    for css in &embedded {
        rules.extend(parser::parse_stylesheet(css));
    }
    for css in extra_css {
        rules.extend(parser::parse_stylesheet(css));
    }

    tracing::info!(
        embedded_count = embedded.len(),
        rule_count = rules.len(),
        "parsed CSS rules for style computation"
    );

    // 3. Walk DOM and apply styles.
    let ancestors: Vec<&DomNode> = Vec::new();
    apply_styles_recursive(dom, &rules, &ancestors)
}

/// Recursively apply styles to a DOM node.
fn apply_styles_recursive(
    node: DomNode,
    rules: &[CssRule],
    ancestors: &[&DomNode],
) -> DomNode {
    match node {
        DomNode::Element {
            tag,
            attributes,
            children,
        } => {
            // Build a temporary node for matching (without children for efficiency).
            let temp_node = DomNode::Element {
                tag: tag.clone(),
                attributes: attributes.clone(),
                children: vec![],
            };

            // Compute styles for this element.
            let style_str = compute_element_style(&tag, &attributes, &temp_node, rules, ancestors);

            // Build new attributes with data-nova-style.
            let mut new_attributes = attributes;
            // Remove old data-nova-style if present.
            new_attributes.retain(|(k, _)| k != "data-nova-style");
            if !style_str.is_empty() {
                new_attributes.push(("data-nova-style".into(), style_str));
            }

            // Build the full node for ancestor tracking.
            let this_node = DomNode::Element {
                tag: tag.clone(),
                attributes: new_attributes.clone(),
                children: vec![], // placeholder
            };

            // Process children with this element as an ancestor.
            // We need to extend ancestors with a reference to this_node.
            // Since we own the node, we use a local reference.
            let mut new_ancestors: Vec<&DomNode> = ancestors.to_vec();
            // SAFETY: this_node lives for the duration of the children processing.
            let this_ref: &DomNode = &this_node;
            new_ancestors.push(this_ref);

            let new_children: Vec<DomNode> = children
                .into_iter()
                .map(|child| apply_styles_recursive(child, rules, &new_ancestors))
                .collect();

            DomNode::Element {
                tag,
                attributes: new_attributes,
                children: new_children,
            }
        }
        DomNode::Document { children } => {
            let new_children: Vec<DomNode> = children
                .into_iter()
                .map(|child| apply_styles_recursive(child, rules, ancestors))
                .collect();
            DomNode::Document {
                children: new_children,
            }
        }
        other => other,
    }
}

/// Compute the style string for a single element.
///
/// Collects declarations from: user-agent defaults, stylesheet rules, inline styles.
/// Sorts by cascade precedence, deduplicates (last wins), and serializes.
fn compute_element_style(
    tag: &str,
    attributes: &[(String, String)],
    node: &DomNode,
    rules: &[CssRule],
    ancestors: &[&DomNode],
) -> String {
    let mut declarations: Vec<CascadedDeclaration> = Vec::new();

    // 1. User-agent defaults.
    let ua_style = default_style_for_tag(tag);
    for (prop, val) in &ua_style.properties {
        let value_str = style_value_to_css(val);
        declarations.push(CascadedDeclaration {
            property: prop.clone(),
            value: value_str,
            specificity: Specificity(0, 0, 0),
            origin: CascadeOrigin::UserAgent,
        });
    }

    // 2. Stylesheet rules (sorted by specificity).
    for rule in rules {
        if rule.selector.matches(node, ancestors) {
            let spec = rule.selector.specificity();
            for (prop, val) in &rule.declarations {
                declarations.push(CascadedDeclaration {
                    property: prop.clone(),
                    value: val.clone(),
                    specificity: spec,
                    origin: CascadeOrigin::AuthorStylesheet,
                });
            }
        }
    }

    // 3. Inline styles (highest priority).
    if let Some(style_attr) = attributes.iter().find(|(k, _)| k == "style") {
        let inline_decls = parser::parse_inline_style(&style_attr.1);
        for (prop, val) in inline_decls {
            declarations.push(CascadedDeclaration {
                property: prop,
                value: val,
                specificity: Specificity::inline(),
                origin: CascadeOrigin::Inline,
            });
        }
    }

    // 4. Expand shorthand properties.
    let mut expanded = Vec::new();
    for decl in declarations {
        expand_shorthand(decl, &mut expanded);
    }

    // 5. Sort by cascade: origin first, then specificity.
    // Stable sort preserves source order for equal specificity.
    expanded.sort_by(|a, b| {
        a.origin
            .cmp(&b.origin)
            .then(a.specificity.cmp(&b.specificity))
    });

    // 6. Deduplicate: for each property, the last declaration wins.
    let mut final_props: Vec<(String, String)> = Vec::new();
    for decl in expanded {
        if let Some(existing) = final_props.iter_mut().find(|(p, _)| *p == decl.property) {
            existing.1 = decl.value;
        } else {
            final_props.push((decl.property, decl.value));
        }
    }

    // 7. Serialize as inline CSS string.
    final_props
        .iter()
        .map(|(p, v)| format!("{}: {}", p, v))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Expand CSS shorthand properties into their longhand equivalents.
fn expand_shorthand(decl: CascadedDeclaration, out: &mut Vec<CascadedDeclaration>) {
    match decl.property.as_str() {
        "margin" => {
            let parts = split_shorthand(&decl.value);
            let (top, right, bottom, left) = expand_trbl(&parts);
            for (suffix, val) in [
                ("margin-top", top),
                ("margin-right", right),
                ("margin-bottom", bottom),
                ("margin-left", left),
            ] {
                out.push(CascadedDeclaration {
                    property: suffix.into(),
                    value: val.to_string(),
                    specificity: decl.specificity,
                    origin: decl.origin,
                });
            }
        }
        "padding" => {
            let parts = split_shorthand(&decl.value);
            let (top, right, bottom, left) = expand_trbl(&parts);
            for (suffix, val) in [
                ("padding-top", top),
                ("padding-right", right),
                ("padding-bottom", bottom),
                ("padding-left", left),
            ] {
                out.push(CascadedDeclaration {
                    property: suffix.into(),
                    value: val.to_string(),
                    specificity: decl.specificity,
                    origin: decl.origin,
                });
            }
        }
        "background" => {
            // Simple: if it looks like a color, use it as background-color.
            let val = decl.value.trim();
            if values::parse_color(val).is_some()
                || val.starts_with('#')
                || val.starts_with("rgb")
            {
                out.push(CascadedDeclaration {
                    property: "background-color".into(),
                    value: val.to_string(),
                    specificity: decl.specificity,
                    origin: decl.origin,
                });
            } else {
                // Pass through as-is.
                out.push(decl);
            }
        }
        "border" => {
            // border: <width> <style> <color>
            let parts: Vec<&str> = decl.value.split_whitespace().collect();
            for part in &parts {
                if part.ends_with("px") || part.ends_with("em") || *part == "thin" || *part == "medium" || *part == "thick" {
                    let width_val = match *part {
                        "thin" => "1px".to_string(),
                        "medium" => "3px".to_string(),
                        "thick" => "5px".to_string(),
                        v => v.to_string(),
                    };
                    out.push(CascadedDeclaration {
                        property: "border-width".into(),
                        value: width_val,
                        specificity: decl.specificity,
                        origin: decl.origin,
                    });
                } else if values::parse_color(part).is_some()
                    || part.starts_with('#')
                    || part.starts_with("rgb")
                {
                    out.push(CascadedDeclaration {
                        property: "border-color".into(),
                        value: part.to_string(),
                        specificity: decl.specificity,
                        origin: decl.origin,
                    });
                } else {
                    // Likely border-style (solid, dashed, etc.).
                    out.push(CascadedDeclaration {
                        property: "border-style".into(),
                        value: part.to_string(),
                        specificity: decl.specificity,
                        origin: decl.origin,
                    });
                }
            }
        }
        _ => {
            // Not a shorthand; pass through.
            out.push(decl);
        }
    }
}

/// Split a shorthand value into whitespace-separated parts.
fn split_shorthand(value: &str) -> Vec<&str> {
    value.split_whitespace().collect()
}

/// Expand 1-4 values into top/right/bottom/left (CSS TRBL pattern).
fn expand_trbl<'a>(parts: &[&'a str]) -> (&'a str, &'a str, &'a str, &'a str) {
    match parts.len() {
        1 => (parts[0], parts[0], parts[0], parts[0]),
        2 => (parts[0], parts[1], parts[0], parts[1]),
        3 => (parts[0], parts[1], parts[2], parts[1]),
        4 => (parts[0], parts[1], parts[2], parts[3]),
        _ => ("0", "0", "0", "0"),
    }
}

/// Convert a `StyleValue` to a CSS string representation.
fn style_value_to_css(val: &nova_mod_api::content::StyleValue) -> String {
    use nova_mod_api::content::StyleValue;
    match val {
        StyleValue::Keyword(k) => k.clone(),
        StyleValue::Px(v) => format!("{v}px"),
        StyleValue::Percent(v) => format!("{v}%"),
        StyleValue::Color(c) => {
            if c.a >= 1.0 {
                format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
            } else {
                format!("rgba({}, {}, {}, {})", c.r, c.g, c.b, c.a)
            }
        }
        StyleValue::Str(s) => s.clone(),
        StyleValue::Number(n) => format!("{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dom(html_body: Vec<DomNode>) -> DomNode {
        DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "body".into(),
                    attributes: vec![],
                    children: html_body,
                }],
            }],
        }
    }

    #[test]
    fn extract_style_elements() {
        let dom = make_dom(vec![DomNode::Element {
            tag: "style".into(),
            attributes: vec![],
            children: vec![DomNode::Text("h1 { color: red; }".into())],
        }]);
        let sheets = extract_stylesheets(&dom);
        assert_eq!(sheets.len(), 1);
        assert!(sheets[0].contains("color: red"));
    }

    #[test]
    fn compute_applies_ua_defaults() {
        let dom = make_dom(vec![DomNode::Element {
            tag: "h1".into(),
            attributes: vec![],
            children: vec![DomNode::Text("Hello".into())],
        }]);

        let result = compute_styles(dom, &[]);
        // The h1 should have a data-nova-style attribute.
        fn find_h1(node: &DomNode) -> Option<&DomNode> {
            match node {
                DomNode::Element { tag, children, .. } => {
                    if tag == "h1" {
                        return Some(node);
                    }
                    for child in children {
                        if let Some(found) = find_h1(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                DomNode::Document { children } => {
                    for child in children {
                        if let Some(found) = find_h1(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let h1 = find_h1(&result).expect("should find h1");
        let style = h1.attr("data-nova-style").expect("h1 should have data-nova-style");
        assert!(style.contains("display: block"));
        assert!(style.contains("font-size: 32px"));
        assert!(style.contains("font-weight: bold"));
    }

    #[test]
    fn compute_applies_stylesheet_rules() {
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "style".into(),
                attributes: vec![],
                children: vec![DomNode::Text("h1 { color: red; }".into())],
            },
            DomNode::Element {
                tag: "h1".into(),
                attributes: vec![],
                children: vec![DomNode::Text("Hello".into())],
            },
        ]);

        let result = compute_styles(dom, &[]);
        fn find_h1(node: &DomNode) -> Option<&DomNode> {
            match node {
                DomNode::Element { tag, children, .. } => {
                    if tag == "h1" {
                        return Some(node);
                    }
                    for child in children {
                        if let Some(found) = find_h1(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                DomNode::Document { children } => {
                    for child in children {
                        if let Some(found) = find_h1(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let h1 = find_h1(&result).expect("should find h1");
        let style = h1.attr("data-nova-style").expect("h1 should have data-nova-style");
        // The stylesheet rule `color: red` should override the UA default.
        assert!(style.contains("color: red"), "style = {style}");
    }

    #[test]
    fn inline_style_wins_over_stylesheet() {
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "style".into(),
                attributes: vec![],
                children: vec![DomNode::Text("p { color: blue; }".into())],
            },
            DomNode::Element {
                tag: "p".into(),
                attributes: vec![("style".into(), "color: green".into())],
                children: vec![DomNode::Text("Hello".into())],
            },
        ]);

        let result = compute_styles(dom, &[]);
        fn find_p(node: &DomNode) -> Option<&DomNode> {
            match node {
                DomNode::Element { tag, children, .. } => {
                    if tag == "p" {
                        return Some(node);
                    }
                    for child in children {
                        if let Some(found) = find_p(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                DomNode::Document { children } => {
                    for child in children {
                        if let Some(found) = find_p(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let p = find_p(&result).expect("should find p");
        let style = p.attr("data-nova-style").expect("p should have data-nova-style");
        // Inline `color: green` should win over stylesheet `color: blue`.
        assert!(style.contains("color: green"), "style = {style}");
        assert!(!style.contains("color: blue"), "style = {style}");
    }

    #[test]
    fn class_selector_applies() {
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "style".into(),
                attributes: vec![],
                children: vec![DomNode::Text(".highlight { background-color: yellow; }".into())],
            },
            DomNode::Element {
                tag: "div".into(),
                attributes: vec![("class".into(), "highlight".into())],
                children: vec![DomNode::Text("Highlighted".into())],
            },
        ]);

        let result = compute_styles(dom, &[]);
        fn find_div(node: &DomNode) -> Option<&DomNode> {
            match node {
                DomNode::Element { tag, children, .. } => {
                    if tag == "div" {
                        return Some(node);
                    }
                    for child in children {
                        if let Some(found) = find_div(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                DomNode::Document { children } => {
                    for child in children {
                        if let Some(found) = find_div(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let div = find_div(&result).expect("should find div");
        let style = div.attr("data-nova-style").expect("div should have data-nova-style");
        assert!(
            style.contains("background-color: yellow"),
            "style = {style}"
        );
    }

    #[test]
    fn id_beats_class() {
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "style".into(),
                attributes: vec![],
                children: vec![DomNode::Text(
                    ".foo { color: blue; } #bar { color: red; }".into(),
                )],
            },
            DomNode::Element {
                tag: "div".into(),
                attributes: vec![
                    ("class".into(), "foo".into()),
                    ("id".into(), "bar".into()),
                ],
                children: vec![],
            },
        ]);

        let result = compute_styles(dom, &[]);
        fn find_div(node: &DomNode) -> Option<&DomNode> {
            match node {
                DomNode::Element { tag, children, .. } => {
                    if tag == "div" {
                        return Some(node);
                    }
                    for child in children {
                        if let Some(found) = find_div(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                DomNode::Document { children } => {
                    for child in children {
                        if let Some(found) = find_div(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let div = find_div(&result).expect("should find div");
        let style = div.attr("data-nova-style").expect("div should have data-nova-style");
        // #bar (id) should beat .foo (class).
        assert!(style.contains("color: red"), "style = {style}");
    }

    #[test]
    fn shorthand_margin_expands() {
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "style".into(),
                attributes: vec![],
                children: vec![DomNode::Text("div { margin: 10px 20px; }".into())],
            },
            DomNode::Element {
                tag: "div".into(),
                attributes: vec![],
                children: vec![],
            },
        ]);

        let result = compute_styles(dom, &[]);
        fn find_div(node: &DomNode) -> Option<&DomNode> {
            match node {
                DomNode::Element { tag, children, .. } => {
                    if tag == "div" {
                        return Some(node);
                    }
                    for child in children {
                        if let Some(found) = find_div(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                DomNode::Document { children } => {
                    for child in children {
                        if let Some(found) = find_div(child) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let div = find_div(&result).expect("should find div");
        let style = div.attr("data-nova-style").expect("div should have data-nova-style");
        assert!(style.contains("margin-top: 10px"), "style = {style}");
        assert!(style.contains("margin-right: 20px"), "style = {style}");
        assert!(style.contains("margin-bottom: 10px"), "style = {style}");
        assert!(style.contains("margin-left: 20px"), "style = {style}");
    }
}

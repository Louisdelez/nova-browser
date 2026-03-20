//! Style cascade and resolution.
//!
//! Walks the DOM tree, collects matching CSS rules for each element, sorts
//! them by specificity, applies user-agent defaults, stylesheet rules, and
//! inline styles, then writes the computed styles into `data-nova-style`
//! attributes on each element.

use std::collections::HashMap;

use nova_mod_api::content::DomNode;

use crate::defaults::default_style_for_tag;
use crate::parser::{self, CssRule};
use crate::selector::{PseudoElement, SiblingContext, Specificity};
use crate::values;

// ── Rule index for fast selector pre-filtering ────────────────────────

/// Pre-sorted index of CSS rules for fast selector matching.
///
/// Rules are bucketed by the rightmost compound selector's ID, class, or tag.
/// When matching an element, only relevant buckets are checked instead of all rules.
struct RuleIndex {
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
}

impl RuleIndex {
    fn build(rules: &[CssRule]) -> Self {
        let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_class: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_tag: HashMap<String, Vec<usize>> = HashMap::new();
        let mut universal: Vec<usize> = Vec::new();

        for (i, rule) in rules.iter().enumerate() {
            let mut indexed = false;
            for sel in &rule.selector.selectors {
                if let Some(last) = sel.parts.last() {
                    if let Some(ref id) = last.id {
                        by_id.entry(id.clone()).or_default().push(i);
                        indexed = true;
                    } else if !last.classes.is_empty() {
                        by_class.entry(last.classes[0].clone()).or_default().push(i);
                        indexed = true;
                    } else if let Some(ref tag) = last.tag {
                        by_tag.entry(tag.clone()).or_default().push(i);
                        indexed = true;
                    }
                }
            }
            if !indexed {
                universal.push(i);
            }
        }

        RuleIndex { by_id, by_class, by_tag, universal }
    }

    fn candidates_for(&self, tag: &str, attributes: &[(String, String)]) -> Vec<usize> {
        let mut seen = Vec::new();
        let mut result = Vec::new();
        let push = |idx: usize, seen: &mut Vec<usize>, result: &mut Vec<usize>| {
            if !seen.contains(&idx) {
                seen.push(idx);
                result.push(idx);
            }
        };

        if let Some(id) = attributes.iter().find(|(k, _)| k == "id").map(|(_, v)| v) {
            if let Some(indices) = self.by_id.get(id) {
                for &idx in indices { push(idx, &mut seen, &mut result); }
            }
        }
        if let Some(class_attr) = attributes.iter().find(|(k, _)| k == "class").map(|(_, v)| v) {
            for cls in class_attr.split_whitespace() {
                if let Some(indices) = self.by_class.get(cls) {
                    for &idx in indices { push(idx, &mut seen, &mut result); }
                }
            }
        }
        let tag_lower = tag.to_ascii_lowercase();
        if let Some(indices) = self.by_tag.get(&tag_lower) {
            for &idx in indices { push(idx, &mut seen, &mut result); }
        }
        for &idx in &self.universal {
            push(idx, &mut seen, &mut result);
        }
        result
    }
}

/// A declaration with its origin and specificity, used for cascade sorting.
#[derive(Debug, Clone)]
struct CascadedDeclaration {
    property: String,
    value: String,
    specificity: Specificity,
    origin: CascadeOrigin,
    /// `true` when the declaration was marked `!important`.
    important: bool,
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
pub fn compute_styles(dom: DomNode, extra_css: &[String], viewport_width: f32) -> DomNode {
    // 1. Extract embedded stylesheets.
    let embedded = extract_stylesheets(&dom);

    // 2. Parse all stylesheets.
    let mut rules = Vec::new();
    for css in &embedded {
        rules.extend(parser::parse_stylesheet(css, viewport_width));
    }
    for css in extra_css {
        rules.extend(parser::parse_stylesheet(css, viewport_width));
    }

    tracing::info!(
        embedded_count = embedded.len(),
        rule_count = rules.len(),
        "parsed CSS rules for style computation"
    );

    // 3. Build rule index for fast matching.
    let index = RuleIndex::build(&rules);

    // 4. Walk DOM and apply styles.
    let ancestors: Vec<&DomNode> = Vec::new();
    apply_styles_recursive(dom, &rules, &index, &ancestors)
}

/// Recursively apply styles to a DOM node.
fn apply_styles_recursive(
    node: DomNode,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
) -> DomNode {
    match node {
        DomNode::Element {
            tag,
            attributes,
            children,
        } => {
            // Skip elements that are never visible — no need to compute styles.
            if matches!(tag.as_str(), "script" | "style" | "template" | "noscript" | "svg" | "math") {
                return DomNode::Element { tag, attributes, children };
            }

            // Build a temporary node for matching (without children for efficiency).
            let temp_node = DomNode::Element {
                tag: tag.clone(),
                attributes: attributes.clone(),
                children: vec![],
            };

            // Compute styles for this element.
            let style_str = compute_element_style(&tag, &attributes, &temp_node, rules, index, ancestors);

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

            let mut new_children = apply_styles_to_children(children, rules, index, &new_ancestors);

            // Inject ::before and ::after pseudo-element text nodes when a CSS
            // rule targets this element with `::before` or `::after` and
            // declares a `content` property with a quoted string.
            if let Some(before_text) = pseudo_element_content(
                &tag,
                &new_attributes,
                &this_node,
                rules,
                ancestors,
                PseudoElement::Before,
            ) {
                new_children.insert(0, DomNode::Text(before_text));
            }
            if let Some(after_text) = pseudo_element_content(
                &tag,
                &new_attributes,
                &this_node,
                rules,
                ancestors,
                PseudoElement::After,
            ) {
                new_children.push(DomNode::Text(after_text));
            }

            DomNode::Element {
                tag,
                attributes: new_attributes,
                children: new_children,
            }
        }
        DomNode::Document { children } => {
            let new_children = apply_styles_to_children(children, rules, index, ancestors);
            DomNode::Document {
                children: new_children,
            }
        }
        other => other,
    }
}

/// Process a list of children, providing each child with sibling context
/// for `:nth-child` / `:nth-of-type` pseudo-class matching.
fn apply_styles_to_children(
    children: Vec<DomNode>,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
) -> Vec<DomNode> {
    let len = children.len();
    let mut result = Vec::with_capacity(len);
    let children_vec: Vec<DomNode> = children;

    for i in 0..len {
        let sib_ctx = SiblingContext {
            siblings: &children_vec,
            index: i,
        };
        let child = children_vec[i].clone();
        result.push(apply_styles_recursive_with_siblings(child, rules, index, ancestors, Some(sib_ctx)));
    }

    result
}

/// Like `apply_styles_recursive` but with sibling context for the current node.
fn apply_styles_recursive_with_siblings(
    node: DomNode,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
    siblings: Option<SiblingContext<'_>>,
) -> DomNode {
    match node {
        DomNode::Element {
            tag,
            attributes,
            children,
        } => {
            // Skip elements that are never visible — no need to compute styles.
            if matches!(tag.as_str(), "script" | "style" | "template" | "noscript" | "svg" | "math") {
                return DomNode::Element { tag, attributes, children };
            }

            let temp_node = DomNode::Element {
                tag: tag.clone(),
                attributes: attributes.clone(),
                children: vec![],
            };

            let style_str = compute_element_style_with_siblings(
                &tag, &attributes, &temp_node, rules, index, ancestors, siblings,
            );

            let mut new_attributes = attributes;
            new_attributes.retain(|(k, _)| k != "data-nova-style");
            if !style_str.is_empty() {
                new_attributes.push(("data-nova-style".into(), style_str));
            }

            let this_node = DomNode::Element {
                tag: tag.clone(),
                attributes: new_attributes.clone(),
                children: vec![],
            };

            let mut new_ancestors: Vec<&DomNode> = ancestors.to_vec();
            let this_ref: &DomNode = &this_node;
            new_ancestors.push(this_ref);

            let mut new_children = apply_styles_to_children(children, rules, index, &new_ancestors);

            if let Some(before_text) = pseudo_element_content(
                &tag, &new_attributes, &this_node, rules, ancestors, PseudoElement::Before,
            ) {
                new_children.insert(0, DomNode::Text(before_text));
            }
            if let Some(after_text) = pseudo_element_content(
                &tag, &new_attributes, &this_node, rules, ancestors, PseudoElement::After,
            ) {
                new_children.push(DomNode::Text(after_text));
            }

            DomNode::Element {
                tag,
                attributes: new_attributes,
                children: new_children,
            }
        }
        DomNode::Document { children } => {
            let new_children = apply_styles_to_children(children, rules, index, ancestors);
            DomNode::Document { children: new_children }
        }
        other => other,
    }
}

/// Compute element style with sibling context for nth-child matching.
fn compute_element_style_with_siblings(
    tag: &str,
    attributes: &[(String, String)],
    node: &DomNode,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
    siblings: Option<SiblingContext<'_>>,
) -> String {
    compute_element_style_impl(tag, attributes, node, rules, index, ancestors, siblings)
}

fn compute_element_style(
    tag: &str,
    attributes: &[(String, String)],
    node: &DomNode,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
) -> String {
    compute_element_style_impl(tag, attributes, node, rules, index, ancestors, None)
}

/// Internal implementation that supports optional sibling context.
fn compute_element_style_impl(
    tag: &str,
    attributes: &[(String, String)],
    node: &DomNode,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
    siblings: Option<SiblingContext<'_>>,
) -> String {
    let mut declarations: Vec<CascadedDeclaration> = Vec::new();

    // 0. CSS inheritance: inherit inheritable properties from the nearest
    //    ancestor that has a computed `data-nova-style`. These are added at
    //    the lowest priority so any explicit declaration will override them.
    static INHERITED_PROPERTIES: &[&str] = &[
        "color",
        "font-size",
        "font-weight",
        "font-style",
        "font-family",
        "text-align",
        "text-decoration",
        "text-transform",
        "text-indent",
        "line-height",
        "letter-spacing",
        "word-spacing",
        "white-space",
        "visibility",
        "cursor",
        "direction",
        "list-style-type",
        "list-style-position",
    ];

    // Track which inherited properties we've already found (from a closer
    // ancestor) so we only take the nearest value for each property.
    let mut inherited_props: Vec<String> = Vec::new();
    // Only check the nearest 5 ancestors for inherited properties to limit
    // O(depth) cost on deeply nested DOMs.
    for ancestor in ancestors.iter().rev().take(5) {
        if let DomNode::Element { attributes, .. } = ancestor {
            if let Some(style_str) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
                for decl in style_str.1.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let prop = parts[0].trim();
                        let val = parts[1].trim();
                        if INHERITED_PROPERTIES.contains(&prop)
                            && !inherited_props.contains(&prop.to_string())
                        {
                            inherited_props.push(prop.to_string());
                            declarations.push(CascadedDeclaration {
                                property: prop.to_string(),
                                value: val.to_string(),
                                specificity: Specificity(0, 0, 0),
                                origin: CascadeOrigin::UserAgent,
                                important: false,
                            });
                        }
                    }
                }
            }
        }
    }

    // 1. User-agent defaults.
    let ua_style = default_style_for_tag(tag);
    for (prop, val) in &ua_style.properties {
        let value_str = style_value_to_css(val);
        declarations.push(CascadedDeclaration {
            property: prop.clone(),
            value: value_str,
            specificity: Specificity(0, 0, 0),
            origin: CascadeOrigin::UserAgent,
            important: false,
        });
    }

    // 2. Stylesheet rules — use the index to only check candidate rules.
    let candidates = index.candidates_for(tag, attributes);
    for &rule_idx in &candidates {
        let rule = &rules[rule_idx];
        if rule.selector.matches(node, ancestors, siblings) {
            let spec = rule.selector.specificity();
            for decl in &rule.declarations {
                declarations.push(CascadedDeclaration {
                    property: decl.property.clone(),
                    value: decl.value.clone(),
                    specificity: spec,
                    origin: CascadeOrigin::AuthorStylesheet,
                    important: decl.important,
                });
            }
        }
    }

    // 3. Inline styles (highest priority).
    if let Some(style_attr) = attributes.iter().find(|(k, _)| k == "style") {
        let inline_decls = parser::parse_inline_style(&style_attr.1);
        for decl in inline_decls {
            declarations.push(CascadedDeclaration {
                property: decl.property,
                value: decl.value,
                specificity: Specificity::inline(),
                origin: CascadeOrigin::Inline,
                important: decl.important,
            });
        }
    }

    // 4. Expand shorthand properties.
    let mut expanded = Vec::new();
    for decl in declarations {
        expand_shorthand(decl, &mut expanded);
    }

    // 5. Sort by cascade: !important first, then origin, then specificity.
    // `!important` declarations always beat normal declarations, regardless of
    // origin or specificity.  Within the same importance level the existing
    // (origin, specificity) ordering applies.
    // Stable sort preserves source order for equal specificity.
    expanded.sort_by(|a, b| {
        a.important
            .cmp(&b.important)
            .then(a.origin.cmp(&b.origin))
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

    // 6b. Resolve `inherit`, `initial`, and `unset` keywords.
    for (prop, val) in &mut final_props {
        let trimmed = val.trim();
        match trimmed {
            "inherit" => {
                // Look up the property value from the nearest ancestor.
                let inherited = find_inherited_value(prop, ancestors);
                *val = inherited.unwrap_or_default();
            }
            "initial" => {
                // Reset to the CSS initial value (remove the property — UA default will apply).
                *val = String::new();
            }
            "unset" => {
                // For inherited properties, behave like `inherit`; for others, like `initial`.
                if INHERITED_PROPERTIES.contains(&prop.as_str()) {
                    let inherited = find_inherited_value(prop, ancestors);
                    *val = inherited.unwrap_or_default();
                } else {
                    *val = String::new();
                }
            }
            _ => {}
        }
    }
    // Remove properties that were reset to empty (from `initial`).
    final_props.retain(|(_, v)| !v.is_empty());

    // 7. Resolve CSS custom properties (var()).
    // Collect custom properties (--*) from ancestors and this element.
    let mut custom_props: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    // Inherit custom properties from ancestors (outermost to innermost).
    for ancestor in ancestors.iter() {
        if let DomNode::Element { attributes, .. } = ancestor {
            if let Some(style_str) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
                for decl in style_str.1.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let prop = parts[0].trim();
                        let val = parts[1].trim();
                        if prop.starts_with("--") {
                            custom_props.insert(prop.to_string(), val.to_string());
                        }
                    }
                }
            }
        }
    }

    // Add this element's own custom properties (override ancestors).
    for (prop, val) in &final_props {
        if prop.starts_with("--") {
            custom_props.insert(prop.clone(), val.clone());
        }
    }

    // Resolve var() in non-custom property values.
    for (prop, val) in &mut final_props {
        if !prop.starts_with("--") && val.contains("var(") {
            *val = resolve_var(val, &custom_props);
        }
    }

    // 7b. Resolve calc() expressions in property values.
    for (_prop, val) in &mut final_props {
        if val.contains("calc(") {
            if let Some(inner) = val.strip_prefix("calc(").and_then(|s| s.strip_suffix(')')) {
                if !inner.contains('%') && !inner.contains("vw") && !inner.contains("vh") {
                    if let Some(px) = values::eval_calc(inner, 0.0) {
                        *val = format!("{px}px");
                    }
                }
            }
        }
    }

    // 8. Serialize as inline CSS string.
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
                    important: decl.important,
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
                    important: decl.important,
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
                    important: decl.important,
                });
            } else {
                // Pass through as-is.
                out.push(decl);
            }
        }
        "border" => {
            expand_border_shorthand("border", &decl, out);
        }
        "border-top" => {
            expand_border_shorthand("border-top", &decl, out);
        }
        "border-bottom" => {
            expand_border_shorthand("border-bottom", &decl, out);
        }
        "border-left" => {
            expand_border_shorthand("border-left", &decl, out);
        }
        "border-right" => {
            expand_border_shorthand("border-right", &decl, out);
        }
        "flex" => {
            let val = decl.value.trim();
            let (grow, shrink, basis) = match val {
                "none" => ("0", "0", "auto"),
                "auto" => ("1", "1", "auto"),
                _ => {
                    let parts: Vec<&str> = val.split_whitespace().collect();
                    match parts.len() {
                        1 => {
                            // Single number → flex-grow: N; flex-shrink: 1; flex-basis: 0%
                            // But if it's not parseable as a number, treat as keyword basis.
                            if parts[0].parse::<f64>().is_ok() {
                                // Store the grow value; shrink=1, basis=0%
                                // We handle this specially below.
                                let grow_val = parts[0];
                                out.push(CascadedDeclaration {
                                    property: "flex-grow".into(),
                                    value: grow_val.to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                                out.push(CascadedDeclaration {
                                    property: "flex-shrink".into(),
                                    value: "1".to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                                out.push(CascadedDeclaration {
                                    property: "flex-basis".into(),
                                    value: "0%".to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                                return;
                            } else {
                                ("1", "1", parts[0])
                            }
                        }
                        2 => {
                            // flex-grow flex-shrink  or  flex-grow flex-basis
                            // If second token is a number, treat as flex-shrink; otherwise flex-basis.
                            if parts[1].parse::<f64>().is_ok() {
                                // grow + shrink, basis = 0%
                                out.push(CascadedDeclaration {
                                    property: "flex-grow".into(),
                                    value: parts[0].to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                                out.push(CascadedDeclaration {
                                    property: "flex-shrink".into(),
                                    value: parts[1].to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                                out.push(CascadedDeclaration {
                                    property: "flex-basis".into(),
                                    value: "0%".to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                            } else {
                                // grow + basis
                                out.push(CascadedDeclaration {
                                    property: "flex-grow".into(),
                                    value: parts[0].to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                                out.push(CascadedDeclaration {
                                    property: "flex-shrink".into(),
                                    value: "1".to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                                out.push(CascadedDeclaration {
                                    property: "flex-basis".into(),
                                    value: parts[1].to_string(),
                                    specificity: decl.specificity,
                                    origin: decl.origin,
                                    important: decl.important,
                                });
                            }
                            return;
                        }
                        _ => {
                            // 3+ tokens: flex-grow flex-shrink flex-basis
                            (parts[0], parts[1], parts.get(2).copied().unwrap_or("0%"))
                        }
                    }
                }
            };
            for (prop, val) in [("flex-grow", grow), ("flex-shrink", shrink), ("flex-basis", basis)] {
                out.push(CascadedDeclaration {
                    property: prop.into(),
                    value: val.to_string(),
                    specificity: decl.specificity,
                    origin: decl.origin,
                    important: decl.important,
                });
            }
        }
        "overflow" => {
            let parts: Vec<&str> = decl.value.split_whitespace().collect();
            let (ox, oy) = match parts.len() {
                1 => (parts[0], parts[0]),
                _ => (parts[0], parts.get(1).copied().unwrap_or(parts[0])),
            };
            for (prop, val) in [("overflow-x", ox), ("overflow-y", oy)] {
                out.push(CascadedDeclaration {
                    property: prop.into(),
                    value: val.to_string(),
                    specificity: decl.specificity,
                    origin: decl.origin,
                    important: decl.important,
                });
            }
        }
        "list-style" => {
            // Possible tokens: type keywords, position keywords (inside/outside), url(...)
            static LIST_STYLE_TYPES: &[&str] = &[
                "disc", "circle", "square", "decimal", "lower-roman", "upper-roman",
                "lower-alpha", "upper-alpha", "lower-latin", "upper-latin", "none",
            ];
            static LIST_STYLE_POSITIONS: &[&str] = &["inside", "outside"];

            let val = decl.value.trim();
            if val == "none" {
                for (prop, v) in [
                    ("list-style-type", "none"),
                    ("list-style-position", "outside"),
                    ("list-style-image", "none"),
                ] {
                    out.push(CascadedDeclaration {
                        property: prop.into(),
                        value: v.to_string(),
                        specificity: decl.specificity,
                        origin: decl.origin,
                        important: decl.important,
                    });
                }
            } else {
                let parts: Vec<&str> = val.split_whitespace().collect();
                let mut style_type = "disc";
                let mut style_position = "outside";
                let mut style_image = "none";

                for part in &parts {
                    if LIST_STYLE_POSITIONS.contains(part) {
                        style_position = part;
                    } else if part.starts_with("url(") {
                        style_image = part;
                    } else if LIST_STYLE_TYPES.contains(part) {
                        style_type = part;
                    }
                }

                for (prop, v) in [
                    ("list-style-type", style_type),
                    ("list-style-position", style_position),
                    ("list-style-image", style_image),
                ] {
                    out.push(CascadedDeclaration {
                        property: prop.into(),
                        value: v.to_string(),
                        specificity: decl.specificity,
                        origin: decl.origin,
                        important: decl.important,
                    });
                }
            }
        }
        "text-decoration" => {
            // Pass through as-is; values like `underline`, `none`, `line-through` are used directly.
            out.push(decl);
        }
        "animation" => {
            // Extract animation-name from the shorthand.
            // animation: name duration timing-function delay iteration-count direction fill-mode
            // The animation name is typically the first non-numeric, non-keyword token.
            let parts: Vec<&str> = decl.value.split_whitespace().collect();
            if !parts.is_empty() {
                for part in &parts {
                    if part.parse::<f64>().is_ok()
                        || part.ends_with('s') || part.ends_with("ms")
                    {
                        continue;
                    }
                    if matches!(
                        *part,
                        "ease" | "linear" | "ease-in" | "ease-out" | "ease-in-out"
                            | "normal" | "reverse" | "alternate" | "alternate-reverse"
                            | "none" | "forwards" | "backwards" | "both" | "infinite"
                            | "running" | "paused"
                    ) {
                        continue;
                    }
                    // This token is likely the animation name.
                    out.push(CascadedDeclaration {
                        property: "animation-name".into(),
                        value: part.to_string(),
                        specificity: decl.specificity,
                        origin: decl.origin,
                        important: decl.important,
                    });
                    break;
                }
            }
            // TODO: implement animation application — currently @keyframes rules
            // are parsed but animations are not applied. A full implementation
            // requires a timer/event loop to interpolate between keyframes. A
            // simple static version could apply the "to" (100%) keyframe styles
            // as the final state.
            out.push(decl);
        }
        _ => {
            // Not a shorthand; pass through.
            out.push(decl);
        }
    }
}

/// Parse and expand a directional border shorthand (`border`, `border-top`, etc.).
///
/// For `border` the longhands are `border-width`, `border-style`, `border-color`.
/// For `border-{side}` the longhands are `border-{side}-width`, `border-{side}-style`, `border-{side}-color`.
fn expand_border_shorthand(prefix: &str, decl: &CascadedDeclaration, out: &mut Vec<CascadedDeclaration>) {
    let (width_prop, style_prop, color_prop) = if prefix == "border" {
        (
            "border-width".to_string(),
            "border-style".to_string(),
            "border-color".to_string(),
        )
    } else {
        (
            format!("{prefix}-width"),
            format!("{prefix}-style"),
            format!("{prefix}-color"),
        )
    };

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
                property: width_prop.clone(),
                value: width_val,
                specificity: decl.specificity,
                origin: decl.origin,
                important: decl.important,
            });
        } else if values::parse_color(part).is_some()
            || part.starts_with('#')
            || part.starts_with("rgb")
        {
            out.push(CascadedDeclaration {
                property: color_prop.clone(),
                value: part.to_string(),
                specificity: decl.specificity,
                origin: decl.origin,
                important: decl.important,
            });
        } else {
            // Likely border-style keyword (solid, dashed, etc.).
            out.push(CascadedDeclaration {
                property: style_prop.clone(),
                value: part.to_string(),
                specificity: decl.specificity,
                origin: decl.origin,
                important: decl.important,
            });
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

/// Resolve `var(--name)` and `var(--name, fallback)` references in a CSS value.
///
/// Performs up to 8 passes to handle nested var() in fallback values.
fn resolve_var(value: &str, custom_props: &std::collections::HashMap<String, String>) -> String {
    let mut result = value.to_string();
    for _ in 0..8 {
        if !result.contains("var(") {
            break;
        }
        result = resolve_var_once(&result, custom_props);
    }
    result
}

/// Perform a single pass of var() substitution.
fn resolve_var_once(value: &str, custom_props: &std::collections::HashMap<String, String>) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == 'v' {
            // Check for "var("
            let mut buf = String::from('v');
            let prefix = "ar(";
            let mut matched = true;
            for expected in prefix.chars() {
                if let Some(&next) = chars.peek() {
                    if next == expected {
                        buf.push(next);
                        chars.next();
                    } else {
                        matched = false;
                        break;
                    }
                } else {
                    matched = false;
                    break;
                }
            }

            if matched {
                // Read content inside var(...), tracking parenthesis depth.
                let mut inner = String::new();
                let mut depth = 1;
                while let Some(c) = chars.next() {
                    if c == '(' {
                        depth += 1;
                        inner.push(c);
                    } else if c == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        inner.push(c);
                    } else {
                        inner.push(c);
                    }
                }

                // Parse: --name or --name, fallback
                let inner = inner.trim();
                let (var_name, fallback) = if let Some(comma_pos) = inner.find(',') {
                    let name = inner[..comma_pos].trim();
                    let fb = inner[comma_pos + 1..].trim();
                    (name, Some(fb))
                } else {
                    (inner, None)
                };

                if let Some(val) = custom_props.get(var_name) {
                    result.push_str(val);
                } else if let Some(fb) = fallback {
                    result.push_str(fb);
                }
                // If no value and no fallback, output nothing.
            } else {
                result.push_str(&buf);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Check if any CSS rule targets `node` with `pe` (::before or ::after) and
/// declares a quoted `content` string.  Returns the unquoted content text if
/// found, or `None` otherwise.
fn pseudo_element_content(
    _tag: &str,
    _attributes: &[(String, String)],
    node: &DomNode,
    rules: &[CssRule],
    ancestors: &[&DomNode],
    pe: PseudoElement,
) -> Option<String> {
    for rule in rules {
        if rule.selector.matches_with_pseudo_element(node, ancestors, pe) {
            // Look for `content` property with a quoted string value.
            for decl in &rule.declarations {
                if decl.property == "content" {
                    let val = decl.value.trim();
                    // Accept both double- and single-quoted strings.
                    let unquoted = if (val.starts_with('"') && val.ends_with('"'))
                        || (val.starts_with('\'') && val.ends_with('\''))
                    {
                        val[1..val.len() - 1].to_string()
                    } else if val == "none" || val == "normal" || val.is_empty() {
                        continue;
                    } else {
                        // Unquoted keyword — treat as raw text.
                        val.to_string()
                    };
                    if !unquoted.is_empty() {
                        return Some(unquoted);
                    }
                }
            }
        }
    }
    None
}

/// Convert a `StyleValue` to a CSS string representation.
/// Find the inherited value of a CSS property by looking up ancestor styles.
fn find_inherited_value(property: &str, ancestors: &[&DomNode]) -> Option<String> {
    for ancestor in ancestors.iter().rev() {
        if let DomNode::Element { attributes, .. } = ancestor {
            if let Some(style_str) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
                for decl in style_str.1.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 && parts[0].trim() == property {
                        let val = parts[1].trim();
                        if !val.is_empty() && val != "inherit" && val != "initial" && val != "unset" {
                            return Some(val.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

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

        let result = compute_styles(dom, &[], 1280.0);
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

        let result = compute_styles(dom, &[], 1280.0);
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

        let result = compute_styles(dom, &[], 1280.0);
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

        let result = compute_styles(dom, &[], 1280.0);
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

        let result = compute_styles(dom, &[], 1280.0);
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

        let result = compute_styles(dom, &[], 1280.0);
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

    // ── !important override test ──────────────────────────────────────────────

    /// `!important` on a lower-specificity rule must beat a normal declaration
    /// from a higher-specificity selector.
    #[test]
    fn important_beats_higher_specificity() {
        // `#high-specificity { color: blue; }` has specificity (1,0,0).
        // `.low { color: red !important; }` has specificity (0,1,0) but is !important.
        // The !important red should win.
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "style".into(),
                attributes: vec![],
                children: vec![DomNode::Text(
                    "#high { color: blue; } .low { color: red !important; }".into(),
                )],
            },
            DomNode::Element {
                tag: "p".into(),
                attributes: vec![
                    ("id".into(), "high".into()),
                    ("class".into(), "low".into()),
                ],
                children: vec![],
            },
        ]);

        let result = compute_styles(dom, &[], 1280.0);
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
        // !important red should win over the higher-specificity blue.
        assert!(style.contains("color: red"), "!important should win: style = {style}");
        assert!(!style.contains("color: blue"), "blue should be overridden: style = {style}");
    }

    /// `!important` inline style beats a normal inline style from a stylesheet rule.
    #[test]
    fn important_stylesheet_beats_normal_inline() {
        // Stylesheet: `p { color: green !important; }` — !important author.
        // Inline style on element: `color: purple` — normal inline (no !important).
        // According to CSS spec, !important author beats normal inline.
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "style".into(),
                attributes: vec![],
                children: vec![DomNode::Text("p { color: green !important; }".into())],
            },
            DomNode::Element {
                tag: "p".into(),
                attributes: vec![("style".into(), "color: purple".into())],
                children: vec![],
            },
        ]);

        let result = compute_styles(dom, &[], 1280.0);
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
        assert!(style.contains("color: green"), "!important stylesheet should win: style = {style}");
        assert!(!style.contains("color: purple"), "normal inline should lose: style = {style}");
    }

    // ── Helper: call expand_shorthand and return a map of property→value ──────

    fn expand(property: &str, value: &str) -> std::collections::HashMap<String, String> {
        let decl = CascadedDeclaration {
            property: property.to_string(),
            value: value.to_string(),
            specificity: crate::selector::Specificity(0, 0, 0),
            origin: CascadeOrigin::AuthorStylesheet,
            important: false,
        };
        let mut out = Vec::new();
        expand_shorthand(decl, &mut out);
        out.into_iter().map(|d| (d.property, d.value)).collect()
    }

    // ── flex shorthand ────────────────────────────────────────────────────────

    #[test]
    fn flex_single_number() {
        let m = expand("flex", "1");
        assert_eq!(m.get("flex-grow").map(String::as_str), Some("1"));
        assert_eq!(m.get("flex-shrink").map(String::as_str), Some("1"));
        assert_eq!(m.get("flex-basis").map(String::as_str), Some("0%"));
    }

    #[test]
    fn flex_three_values() {
        let m = expand("flex", "0 1 auto");
        assert_eq!(m.get("flex-grow").map(String::as_str), Some("0"));
        assert_eq!(m.get("flex-shrink").map(String::as_str), Some("1"));
        assert_eq!(m.get("flex-basis").map(String::as_str), Some("auto"));
    }

    #[test]
    fn flex_none_keyword() {
        let m = expand("flex", "none");
        assert_eq!(m.get("flex-grow").map(String::as_str), Some("0"));
        assert_eq!(m.get("flex-shrink").map(String::as_str), Some("0"));
        assert_eq!(m.get("flex-basis").map(String::as_str), Some("auto"));
    }

    #[test]
    fn flex_auto_keyword() {
        let m = expand("flex", "auto");
        assert_eq!(m.get("flex-grow").map(String::as_str), Some("1"));
        assert_eq!(m.get("flex-shrink").map(String::as_str), Some("1"));
        assert_eq!(m.get("flex-basis").map(String::as_str), Some("auto"));
    }

    // ── overflow shorthand ────────────────────────────────────────────────────

    #[test]
    fn overflow_single_value() {
        let m = expand("overflow", "hidden");
        assert_eq!(m.get("overflow-x").map(String::as_str), Some("hidden"));
        assert_eq!(m.get("overflow-y").map(String::as_str), Some("hidden"));
    }

    #[test]
    fn overflow_two_values() {
        let m = expand("overflow", "auto scroll");
        assert_eq!(m.get("overflow-x").map(String::as_str), Some("auto"));
        assert_eq!(m.get("overflow-y").map(String::as_str), Some("scroll"));
    }

    // ── list-style shorthand ──────────────────────────────────────────────────

    #[test]
    fn list_style_none() {
        let m = expand("list-style", "none");
        assert_eq!(m.get("list-style-type").map(String::as_str), Some("none"));
        assert_eq!(m.get("list-style-position").map(String::as_str), Some("outside"));
        assert_eq!(m.get("list-style-image").map(String::as_str), Some("none"));
    }

    #[test]
    fn list_style_disc_inside() {
        let m = expand("list-style", "disc inside");
        assert_eq!(m.get("list-style-type").map(String::as_str), Some("disc"));
        assert_eq!(m.get("list-style-position").map(String::as_str), Some("inside"));
        assert_eq!(m.get("list-style-image").map(String::as_str), Some("none"));
    }

    // ── directional border shorthands ─────────────────────────────────────────

    #[test]
    fn border_top_shorthand() {
        let m = expand("border-top", "2px solid red");
        assert_eq!(m.get("border-top-width").map(String::as_str), Some("2px"));
        assert_eq!(m.get("border-top-style").map(String::as_str), Some("solid"));
        assert_eq!(m.get("border-top-color").map(String::as_str), Some("red"));
    }

    #[test]
    fn border_bottom_shorthand() {
        let m = expand("border-bottom", "1px dashed blue");
        assert_eq!(m.get("border-bottom-width").map(String::as_str), Some("1px"));
        assert_eq!(m.get("border-bottom-style").map(String::as_str), Some("dashed"));
        assert_eq!(m.get("border-bottom-color").map(String::as_str), Some("blue"));
    }

    #[test]
    fn border_left_shorthand() {
        let m = expand("border-left", "3px solid green");
        assert_eq!(m.get("border-left-width").map(String::as_str), Some("3px"));
        assert_eq!(m.get("border-left-style").map(String::as_str), Some("solid"));
        assert_eq!(m.get("border-left-color").map(String::as_str), Some("green"));
    }

    #[test]
    fn border_right_shorthand() {
        let m = expand("border-right", "4px dotted black");
        assert_eq!(m.get("border-right-width").map(String::as_str), Some("4px"));
        assert_eq!(m.get("border-right-style").map(String::as_str), Some("dotted"));
        assert_eq!(m.get("border-right-color").map(String::as_str), Some("black"));
    }

    // ── text-decoration pass-through ──────────────────────────────────────────

    #[test]
    fn text_decoration_passes_through() {
        let m = expand("text-decoration", "underline");
        assert_eq!(m.get("text-decoration").map(String::as_str), Some("underline"));
        // Should not be split into sub-properties.
        assert!(m.len() == 1);
    }

    #[test]
    fn text_decoration_none_passes_through() {
        let m = expand("text-decoration", "none");
        assert_eq!(m.get("text-decoration").map(String::as_str), Some("none"));
    }

    // ── animation shorthand ─────────────────────────────────────────────────

    #[test]
    fn animation_shorthand_extracts_name() {
        let m = expand("animation", "fadeIn 1s ease-in-out");
        assert_eq!(m.get("animation-name").map(String::as_str), Some("fadeIn"));
        // The original shorthand should also be preserved.
        assert!(m.contains_key("animation"));
    }

    #[test]
    fn animation_shorthand_no_name_when_only_keywords() {
        let m = expand("animation", "none 0.5s linear");
        // "none" is a keyword, not a name — should not be extracted.
        assert!(!m.contains_key("animation-name"));
    }
}

#[cfg(test)]
mod pseudo_element_injection_tests {
    use super::*;
    use nova_mod_api::content::DomNode;

    fn find_element<'a>(node: &'a DomNode, tag_name: &str) -> Option<&'a DomNode> {
        match node {
            DomNode::Element { tag, children, .. } => {
                if tag == tag_name {
                    return Some(node);
                }
                for child in children {
                    if let Some(f) = find_element(child, tag_name) {
                        return Some(f);
                    }
                }
                None
            }
            DomNode::Document { children } => {
                for child in children {
                    if let Some(f) = find_element(child, tag_name) {
                        return Some(f);
                    }
                }
                None
            }
            _ => None,
        }
    }

    #[test]
    fn before_pseudo_injects_text_child() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "style".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text(
                            r#"p::before { content: ">>"; }"#.into(),
                        )],
                    },
                    DomNode::Element {
                        tag: "p".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Hello".into())],
                    },
                ],
            }],
        };

        let result = compute_styles(dom, &[], 800.0);
        let p = find_element(&result, "p").expect("should find p");
        if let DomNode::Element { children, .. } = p {
            // First child should be the injected ::before text.
            assert!(
                !children.is_empty(),
                "p should have children after ::before injection"
            );
            // The first child should be the injected text ">>"
            assert!(
                matches!(&children[0], DomNode::Text(t) if t == ">>"),
                "first child should be '>>'. Got: {:?}",
                &children[0]
            );
        } else {
            panic!("expected Element for p");
        }
    }

    #[test]
    fn after_pseudo_injects_text_child() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "style".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text(
                            r#"span::after { content: " [end]"; }"#.into(),
                        )],
                    },
                    DomNode::Element {
                        tag: "span".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("text".into())],
                    },
                ],
            }],
        };

        let result = compute_styles(dom, &[], 800.0);
        let span = find_element(&result, "span").expect("should find span");
        if let DomNode::Element { children, .. } = span {
            let last = children.last().expect("span should have children");
            assert!(
                matches!(last, DomNode::Text(t) if t == " [end]"),
                "last child should be ' [end]'. Got: {:?}",
                last
            );
        } else {
            panic!("expected Element for span");
        }
    }

    #[test]
    fn content_none_does_not_inject() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "style".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text(
                            r#"p::before { content: none; }"#.into(),
                        )],
                    },
                    DomNode::Element {
                        tag: "p".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Hello".into())],
                    },
                ],
            }],
        };

        let result = compute_styles(dom, &[], 800.0);
        let p = find_element(&result, "p").expect("should find p");
        if let DomNode::Element { children, .. } = p {
            // With content: none, no text should be injected.
            assert_eq!(
                children.len(),
                1,
                "p should have exactly 1 child (no ::before injection with content:none)"
            );
        } else {
            panic!("expected Element for p");
        }
    }
}

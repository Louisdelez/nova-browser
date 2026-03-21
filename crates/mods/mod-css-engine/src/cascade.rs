//! Style cascade and resolution.
//!
//! Walks the DOM tree, collects matching CSS rules for each element, sorts
//! them by specificity, applies user-agent defaults, stylesheet rules, and
//! inline styles, then writes the computed styles into `data-nova-style`
//! attributes on each element.

use std::collections::{HashMap, HashSet};

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
pub struct RuleIndex {
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
}

impl RuleIndex {
    pub fn build(rules: &[CssRule]) -> Self {
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

    pub fn candidates_for(&self, tag: &str, attributes: &[(String, String)]) -> Vec<usize> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();

        if let Some(id) = attributes.iter().find(|(k, _)| k == "id").map(|(_, v)| v) {
            if let Some(indices) = self.by_id.get(id) {
                for &idx in indices {
                    if seen.insert(idx) {
                        result.push(idx);
                    }
                }
            }
        }
        if let Some(class_attr) = attributes.iter().find(|(k, _)| k == "class").map(|(_, v)| v) {
            for cls in class_attr.split_whitespace() {
                if let Some(indices) = self.by_class.get(cls) {
                    for &idx in indices {
                        if seen.insert(idx) {
                            result.push(idx);
                        }
                    }
                }
            }
        }
        let tag_lower = tag.to_ascii_lowercase();
        if let Some(indices) = self.by_tag.get(&tag_lower) {
            for &idx in indices {
                if seen.insert(idx) {
                    result.push(idx);
                }
            }
        }
        for &idx in &self.universal {
            if seen.insert(idx) {
                result.push(idx);
            }
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

    // 2b. For very large stylesheets, skip rules with overly complex
    // selectors (4+ compound parts). Most visual impact comes from simple
    // selectors, and this reduces matching cost significantly.
    let original_count = rules.len();
    if rules.len() > 2000 {
        rules.retain(|rule| {
            rule.selector.selectors.iter().all(|sel| sel.parts.len() <= 4)
        });
        if rules.len() < original_count {
            tracing::info!(
                original = original_count,
                retained = rules.len(),
                dropped = original_count - rules.len(),
                "dropped complex CSS selectors for performance"
            );
        }
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
    let styled = apply_styles_recursive(dom, &rules, &index, &ancestors);

    // 5. CSS background propagation: per CSS2 spec, if the root element
    //    (html) has no author-specified background, the body's background
    //    is used for the canvas (the entire viewport).
    propagate_body_background(styled)
}

/// CSS background propagation from body to the root element.
///
/// Per the CSS2 specification, if the root element (`<html>`) has no
/// author-specified background, the body's `background-color` is used
/// as the canvas background (covering the entire viewport).  We detect
/// this by checking if html's background is the UA-default white.
pub fn propagate_body_background(dom: DomNode) -> DomNode {
    // Only applies to Document root.
    let DomNode::Document { children } = dom else {
        return dom;
    };

    let mut children = children;

    // Find the <html> element.
    for html_node in children.iter_mut() {
        let DomNode::Element {
            tag: html_tag,
            attributes: html_attrs,
            children: html_children,
        } = html_node
        else {
            continue;
        };
        if html_tag != "html" {
            continue;
        }

        // Check if html's background is the UA default (white / no author bg).
        let html_style = html_attrs
            .iter()
            .find(|(k, _)| k == "data-nova-style")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let html_has_author_bg = html_style.contains("background-color")
            && !html_style.contains("background-color: #ffffff")
            && !html_style.contains("background-color: rgb(255, 255, 255)")
            && !html_style.contains("background-color: rgba(255, 255, 255");

        if html_has_author_bg {
            break; // html has an author-specified background, don't propagate.
        }

        // Find the <body> element among html's children.
        for body_node in html_children.iter_mut() {
            let DomNode::Element {
                tag: body_tag,
                attributes: body_attrs,
                ..
            } = body_node
            else {
                continue;
            };
            if body_tag != "body" {
                continue;
            }

            // Extract body's background-color.
            let body_style_idx = body_attrs
                .iter()
                .position(|(k, _)| k == "data-nova-style");
            let Some(idx) = body_style_idx else { break };
            let body_style = body_attrs[idx].1.clone();

            // Find background-color in body's style.
            let mut body_bg = None;
            for decl in body_style.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "background-color" {
                    body_bg = Some(parts[1].trim().to_string());
                }
            }

            let Some(bg_value) = body_bg else { break };

            // Skip if body bg is transparent.
            if bg_value.contains("rgba") && bg_value.contains(", 0)") {
                break;
            }

            // Propagate: set html's background-color to body's value.
            let html_style_idx = html_attrs
                .iter()
                .position(|(k, _)| k == "data-nova-style");
            if let Some(hi) = html_style_idx {
                // Replace html's background-color with body's.
                let mut new_html_style = String::new();
                for decl in html_attrs[hi].1.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let prop = parts[0].trim();
                        if prop == "background-color" {
                            new_html_style
                                .push_str(&format!("background-color: {bg_value}; "));
                        } else {
                            new_html_style.push_str(decl);
                            new_html_style.push_str("; ");
                        }
                    }
                }
                html_attrs[hi].1 = new_html_style.trim_end_matches("; ").to_string();
            }

            // Remove background-color from body's style so it doesn't
            // paint a separate background on the body box.
            let mut new_body_style = String::new();
            for decl in body_style.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() != "background-color" {
                    new_body_style.push_str(decl.trim());
                    new_body_style.push_str("; ");
                }
            }
            body_attrs[idx].1 =
                new_body_style.trim_end_matches("; ").to_string();

            break; // done with body
        }
        break; // done with html
    }

    DomNode::Document { children }
}

/// Maximum DOM depth for style computation.
///
/// Some pages have very deeply nested DOMs (50+ levels). Beyond this depth
/// the cascade is skipped to prevent stack overflow and improve performance.
const MAX_CASCADE_DEPTH: usize = 100;

/// Recursively apply styles to a DOM node.
fn apply_styles_recursive(
    node: DomNode,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
) -> DomNode {
    // Depth limit: ancestors.len() approximates the current depth.
    if ancestors.len() >= MAX_CASCADE_DEPTH {
        return node;
    }

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
            let style_str = compute_element_style(&tag, &attributes, &temp_node, rules, index, ancestors);

            // Build new attributes with data-nova-style.
            let mut new_attributes = attributes;
            // Remove old data-nova-style if present.
            new_attributes.retain(|(k, _)| k != "data-nova-style");
            if !style_str.is_empty() {
                new_attributes.push(("data-nova-style".into(), style_str.clone()));
            }

            // ── Skip hidden subtrees ─────────────────────────────────
            // If the element has `display: none`, none of its children
            // are visible. Skip the entire subtree for performance.
            if style_str.contains("display: none") {
                return DomNode::Element {
                    tag,
                    attributes: new_attributes,
                    children: vec![],
                };
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

            // ── Shadow DOM style encapsulation ───────────────────────
            // If this element is a shadow host (has `data-nova-shadow-host`),
            // its children live in a shadow tree. We parse the shadow-scoped
            // stylesheets and use only those rules (plus `:host` rules) for
            // the children, rather than the document-level rules.
            let is_shadow_host = new_attributes.iter().any(|(k, _)| k == "data-nova-shadow-host");
            let shadow_css = new_attributes.iter()
                .find(|(k, _)| k == "data-nova-shadow-styles")
                .map(|(_, v)| v.clone());

            let mut new_children = if is_shadow_host {
                if let Some(css) = &shadow_css {
                    // Parse shadow-scoped stylesheets.
                    let shadow_rules = parser::parse_stylesheet(css, 1280.0);
                    let shadow_index = RuleIndex::build(&shadow_rules);
                    apply_styles_to_children(children, &shadow_rules, &shadow_index, &new_ancestors)
                } else {
                    // Shadow host with no scoped styles: children get only
                    // inherited properties, no document rules.
                    let empty_rules: Vec<CssRule> = Vec::new();
                    let empty_index = RuleIndex::build(&empty_rules);
                    apply_styles_to_children(children, &empty_rules, &empty_index, &new_ancestors)
                }
            } else {
                apply_styles_to_children(children, rules, index, &new_ancestors)
            };

            // Inject ::before and ::after pseudo-element synthetic nodes when a CSS
            // rule targets this element with `::before` or `::after` and
            // declares a `content` property.
            if let Some(before_node) = build_pseudo_element_node(
                &tag,
                &new_attributes,
                &this_node,
                rules,
                index,
                ancestors,
                PseudoElement::Before,
            ) {
                new_children.insert(0, before_node);
            }
            if let Some(after_node) = build_pseudo_element_node(
                &tag,
                &new_attributes,
                &this_node,
                rules,
                index,
                ancestors,
                PseudoElement::After,
            ) {
                new_children.push(after_node);
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
    if ancestors.len() >= MAX_CASCADE_DEPTH {
        return node;
    }

    apply_styles_recursive_with_siblings_inner(node, rules, index, ancestors, siblings)
}

/// Inner implementation with sibling context (after depth check).
fn apply_styles_recursive_with_siblings_inner(
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
                new_attributes.push(("data-nova-style".into(), style_str.clone()));
            }

            // ── Skip hidden subtrees ─────────────────────────────────
            if style_str.contains("display: none") {
                return DomNode::Element {
                    tag,
                    attributes: new_attributes,
                    children: vec![],
                };
            }

            let this_node = DomNode::Element {
                tag: tag.clone(),
                attributes: new_attributes.clone(),
                children: vec![],
            };

            let mut new_ancestors: Vec<&DomNode> = ancestors.to_vec();
            let this_ref: &DomNode = &this_node;
            new_ancestors.push(this_ref);

            // ── Shadow DOM style encapsulation ───────────────────────
            let is_shadow_host = new_attributes.iter().any(|(k, _)| k == "data-nova-shadow-host");
            let shadow_css = new_attributes.iter()
                .find(|(k, _)| k == "data-nova-shadow-styles")
                .map(|(_, v)| v.clone());

            let mut new_children = if is_shadow_host {
                if let Some(css) = &shadow_css {
                    let shadow_rules = parser::parse_stylesheet(css, 1280.0);
                    let shadow_index = RuleIndex::build(&shadow_rules);
                    apply_styles_to_children(children, &shadow_rules, &shadow_index, &new_ancestors)
                } else {
                    let empty_rules: Vec<CssRule> = Vec::new();
                    let empty_index = RuleIndex::build(&empty_rules);
                    apply_styles_to_children(children, &empty_rules, &empty_index, &new_ancestors)
                }
            } else {
                apply_styles_to_children(children, rules, index, &new_ancestors)
            };

            if let Some(before_node) = build_pseudo_element_node(
                &tag, &new_attributes, &this_node, rules, index, ancestors, PseudoElement::Before,
            ) {
                new_children.insert(0, before_node);
            }
            if let Some(after_node) = build_pseudo_element_node(
                &tag, &new_attributes, &this_node, rules, index, ancestors, PseudoElement::After,
            ) {
                new_children.push(after_node);
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

/// Public wrapper for computing a single element's style.
///
/// Used by the parallel cascade engine to compute per-element styles
/// while reusing the full cascade logic (inheritance, specificity, etc.).
pub fn compute_element_style_public(
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
        "word-break",
        "overflow-wrap",
        "word-wrap",
        "writing-mode",
    ];

    // Track which inherited properties we've already found (from a closer
    // ancestor) so we only take the nearest value for each property.
    let mut inherited_props: HashSet<String> = HashSet::new();
    for ancestor in ancestors.iter().rev() {
        if let DomNode::Element { attributes, .. } = ancestor {
            if let Some(style_str) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
                for decl in style_str.1.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let prop = parts[0].trim();
                        let val = parts[1].trim();
                        if INHERITED_PROPERTIES.contains(&prop)
                            && inherited_props.insert(prop.to_string())
                        {
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

    // 1b. HTML presentational attributes (bgcolor, color, width, etc.).
    // These have lower specificity than author stylesheets but higher than UA defaults.
    for (css_prop, css_val) in apply_presentational_attributes(tag, attributes) {
        declarations.push(CascadedDeclaration {
            property: css_prop,
            value: css_val,
            specificity: Specificity(0, 0, 0),
            origin: CascadeOrigin::AuthorStylesheet,
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

    // 2b. Shadow DOM `:host` rules — if this element is a shadow host,
    //     parse the shadow-scoped stylesheets and apply any `:host` rules
    //     to the host element itself.
    if let Some(shadow_css) = attributes.iter().find(|(k, _)| k == "data-nova-shadow-styles") {
        let shadow_rules = parser::parse_stylesheet(&shadow_css.1, 1280.0);
        // Create a temporary node with shadow-host attribute for :host matching.
        let host_node = DomNode::Element {
            tag: tag.to_string(),
            attributes: {
                let mut attrs = attributes.to_vec();
                if !attrs.iter().any(|(k, _)| k == "data-nova-shadow-host") {
                    attrs.push(("data-nova-shadow-host".into(), "true".into()));
                }
                attrs
            },
            children: vec![],
        };
        for rule in &shadow_rules {
            if rule.selector.matches(&host_node, ancestors, siblings) {
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
    // Use a HashMap for O(1) lookups and preserve insertion order via Vec.
    let mut prop_index: HashMap<String, usize> = HashMap::new();
    let mut final_props: Vec<(String, String)> = Vec::new();
    for decl in expanded {
        if let Some(&idx) = prop_index.get(&decl.property) {
            final_props[idx].1 = decl.value;
        } else {
            prop_index.insert(decl.property.clone(), final_props.len());
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

    // 7c. Resolve `em` units contextually against the parent's computed font-size.
    // `rem` units are already resolved at parse time against 16px (root em).
    // `em` should resolve against the inherited font-size from the nearest ancestor.
    let parent_font_size: f32 = {
        // First check if this element has its own font-size from UA defaults
        // or explicit declaration (already resolved as px).
        let own_fs = final_props
            .iter()
            .find(|(p, _)| p == "font-size")
            .and_then(|(_, v)| v.strip_suffix("px").and_then(|s| s.trim().parse::<f32>().ok()));
        // The parent font-size for resolving em in *other* properties is
        // this element's own computed font-size. But for font-size itself,
        // em resolves against the *parent's* font-size.
        let inherited_fs = ancestors
            .iter()
            .rev()
            .find_map(|a| {
                if let DomNode::Element { attributes, .. } = a {
                    attributes.iter().find(|(k, _)| k == "data-nova-style").and_then(|(_, v)| {
                        v.split(';').find_map(|decl| {
                            let parts: Vec<&str> = decl.splitn(2, ':').collect();
                            if parts.len() == 2 && parts[0].trim() == "font-size" {
                                parts[1].trim().strip_suffix("px").and_then(|s| s.trim().parse::<f32>().ok())
                            } else {
                                None
                            }
                        })
                    })
                } else {
                    None
                }
            })
            .unwrap_or(16.0);
        // For font-size property: resolve em against parent's font-size.
        // First resolve font-size em values.
        if let Some(fs_entry) = final_props.iter_mut().find(|(p, _)| p == "font-size") {
            if let Some(px) = values::resolve_em_value(&fs_entry.1, inherited_fs) {
                fs_entry.1 = format!("{px}px");
            }
        }
        // The computed font-size to use for resolving em in other properties.
        final_props
            .iter()
            .find(|(p, _)| p == "font-size")
            .and_then(|(_, v)| v.strip_suffix("px").and_then(|s| s.trim().parse::<f32>().ok()))
            .or(own_fs)
            .unwrap_or(inherited_fs)
    };
    // Resolve em in all non-font-size properties.
    for (prop, val) in &mut final_props {
        if prop != "font-size" {
            if let Some(px) = values::resolve_em_value(val, parent_font_size) {
                *val = format!("{px}px");
            }
        }
    }

    // 7d. Resolve `pt` units to `px` (1pt = 1.333px).
    for (prop, val) in &mut final_props {
        let trimmed = val.trim();
        if trimmed.ends_with("pt") && !trimmed.ends_with("ppt") {
            if let Some(s) = trimmed.strip_suffix("pt") {
                if let Ok(n) = s.trim().parse::<f32>() {
                    let px = n * 1.333;
                    *val = format!("{px}px");
                }
            }
        }
    }

    // 8. Serialize as inline CSS string.
    let result = final_props
        .iter()
        .map(|(p, v)| format!("{}: {}", p, v))
        .collect::<Vec<_>>()
        .join("; ");

    result
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
        "outline" => {
            // outline: <width> <style> <color>
            let parts: Vec<&str> = split_shorthand(&decl.value);
            let mut width = "medium";
            let mut style = "none";
            let mut color = "currentcolor";
            static OUTLINE_STYLES: &[&str] = &[
                "none", "hidden", "dotted", "dashed", "solid", "double",
                "groove", "ridge", "inset", "outset",
            ];
            for part in &parts {
                if OUTLINE_STYLES.contains(part) {
                    style = part;
                } else if parse_length_token(part) {
                    width = part;
                } else {
                    color = part;
                }
            }
            for (prop, val) in [
                ("outline-width", width),
                ("outline-style", style),
                ("outline-color", color),
            ] {
                out.push(CascadedDeclaration {
                    property: prop.into(),
                    value: val.to_string(),
                    specificity: decl.specificity,
                    origin: decl.origin,
                    important: decl.important,
                });
            }
        }
        "columns" => {
            // columns: <column-count> <column-width> (order doesn't matter)
            let parts: Vec<&str> = split_shorthand(&decl.value);
            for part in &parts {
                if part.parse::<u32>().is_ok() {
                    out.push(CascadedDeclaration {
                        property: "column-count".into(),
                        value: part.to_string(),
                        specificity: decl.specificity,
                        origin: decl.origin,
                        important: decl.important,
                    });
                } else {
                    out.push(CascadedDeclaration {
                        property: "column-width".into(),
                        value: part.to_string(),
                        specificity: decl.specificity,
                        origin: decl.origin,
                        important: decl.important,
                    });
                }
            }
        }
        "word-wrap" => {
            // word-wrap is the legacy name for overflow-wrap.
            out.push(CascadedDeclaration {
                property: "overflow-wrap".into(),
                value: decl.value,
                specificity: decl.specificity,
                origin: decl.origin,
                important: decl.important,
            });
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

/// Check if a token looks like a CSS length value (e.g. `2px`, `1em`, `3`, `medium`).
fn parse_length_token(s: &str) -> bool {
    let s = s.trim();
    matches!(s, "thin" | "medium" | "thick")
        || s.ends_with("px")
        || s.ends_with("em")
        || s.ends_with("rem")
        || s.ends_with("pt")
        || s.parse::<f32>().is_ok()
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

/// Information about a matched pseudo-element rule: the resolved content text
/// and the full set of CSS declarations to apply as inline styles.
struct PseudoElementMatch {
    /// The resolved text content (may be empty for layout-only pseudo-elements).
    content: String,
    /// All declarations from the matching rule (serialized as inline CSS).
    style: String,
}

/// Check if any CSS rule targets `node` with `pe` (::before or ::after).
///
/// Returns a `PseudoElementMatch` with the resolved content and the pseudo-element's
/// CSS declarations, or `None` if no matching rule declares a valid `content`.
///
/// Uses the rule index to only check candidate rules for the element,
/// dramatically reducing the number of rules checked on large stylesheets.
fn find_pseudo_element_match(
    tag: &str,
    attributes: &[(String, String)],
    node: &DomNode,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
    pe: PseudoElement,
) -> Option<PseudoElementMatch> {
    let candidates = index.candidates_for(tag, attributes);

    for &rule_idx in &candidates {
        let rule = &rules[rule_idx];
        if rule.selector.matches_with_pseudo_element(node, ancestors, pe) {
            // Look for `content` property.
            let mut content_val: Option<String> = None;
            for decl in &rule.declarations {
                if decl.property == "content" {
                    let val = decl.value.trim();
                    // `none` and `normal` mean "no pseudo-element".
                    if val == "none" || val == "normal" {
                        return None;
                    }
                    // Accept both double- and single-quoted strings.
                    let unquoted = if (val.starts_with('"') && val.ends_with('"'))
                        || (val.starts_with('\'') && val.ends_with('\''))
                    {
                        let raw = &val[1..val.len() - 1];
                        resolve_css_content_escapes(raw)
                    } else if val.is_empty() {
                        continue;
                    } else if val.starts_with("attr(") && val.ends_with(')') {
                        // `content: attr(name)` — resolve from the element's attributes.
                        let attr_name = val[5..val.len() - 1].trim();
                        attributes
                            .iter()
                            .find(|(k, _)| k == attr_name)
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default()
                    } else {
                        // Unquoted keyword (e.g. open-quote, close-quote) — treat as raw text.
                        val.to_string()
                    };
                    content_val = Some(unquoted);
                }
            }

            // If we found a content declaration (even empty string), build the match.
            if let Some(content) = content_val {
                // Serialize all declarations (except `content`) as inline CSS.
                let style = rule
                    .declarations
                    .iter()
                    .filter(|d| d.property != "content")
                    .map(|d| format!("{}: {}", d.property, d.value))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Some(PseudoElementMatch { content, style });
            }
        }
    }
    None
}

/// Resolve CSS escape sequences in a `content` string value.
///
/// Handles `\HHHH` hex escapes (1-6 hex digits) for Unicode code points,
/// commonly used for icon fonts (e.g. `\f00c` for FontAwesome checkmark).
/// Also handles `\\` for a literal backslash and `\"` for a literal quote.
fn resolve_css_content_escapes(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            // Peek at the next character to determine the escape type.
            match chars.peek() {
                Some(&'\\') => {
                    result.push('\\');
                    chars.next();
                }
                Some(&'"') => {
                    result.push('"');
                    chars.next();
                }
                Some(&'\'') => {
                    result.push('\'');
                    chars.next();
                }
                Some(&'n') => {
                    result.push('\n');
                    chars.next();
                }
                Some(c) if c.is_ascii_hexdigit() => {
                    // Read up to 6 hex digits.
                    let mut hex = String::new();
                    while hex.len() < 6 {
                        if let Some(&c) = chars.peek() {
                            if c.is_ascii_hexdigit() {
                                hex.push(c);
                                chars.next();
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    // Optional single trailing space is consumed per CSS spec.
                    if let Some(&' ') = chars.peek() {
                        chars.next();
                    }
                    if let Ok(code_point) = u32::from_str_radix(&hex, 16) {
                        if let Some(c) = char::from_u32(code_point) {
                            result.push(c);
                        }
                    }
                }
                _ => {
                    // Unknown escape — pass through the backslash.
                    result.push('\\');
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Build a synthetic `DomNode::Element` for a `::before` or `::after` pseudo-element.
///
/// Returns `None` if no matching rule or the content is `none`/`normal`.
fn build_pseudo_element_node(
    tag: &str,
    attributes: &[(String, String)],
    node: &DomNode,
    rules: &[CssRule],
    index: &RuleIndex,
    ancestors: &[&DomNode],
    pe: PseudoElement,
) -> Option<DomNode> {
    let m = find_pseudo_element_match(tag, attributes, node, rules, index, ancestors, pe)?;

    let pseudo_tag = match pe {
        PseudoElement::Before => "nova-before",
        PseudoElement::After => "nova-after",
    };

    let mut attrs = Vec::new();
    if !m.style.is_empty() {
        attrs.push(("data-nova-style".to_string(), m.style));
    }

    let children = if m.content.is_empty() {
        vec![]
    } else {
        vec![DomNode::Text(m.content)]
    };

    Some(DomNode::Element {
        tag: pseudo_tag.to_string(),
        attributes: attrs,
        children,
    })
}

/// Convert HTML presentational attributes to CSS property-value pairs.
///
/// This handles legacy HTML attributes such as `bgcolor`, `color`, `width`,
/// `height`, `align`, `valign`, `border`, `cellpadding`, and `cellspacing`
/// by mapping them to their CSS equivalents.  The resulting declarations are
/// inserted into the cascade with author-stylesheet origin but zero specificity
/// so that any explicit CSS rule will override them.
fn apply_presentational_attributes(
    tag: &str,
    attributes: &[(String, String)],
) -> Vec<(String, String)> {
    let mut result = Vec::new();

    for (attr, val) in attributes {
        match attr.as_str() {
            "bgcolor" => {
                result.push(("background-color".into(), val.clone()));
            }
            "color" if tag == "font" => {
                result.push(("color".into(), val.clone()));
            }
            "width" if matches!(tag, "table" | "td" | "th" | "col" | "colgroup" | "img" | "pre") => {
                if val.ends_with('%') {
                    result.push(("width".into(), val.clone()));
                } else {
                    // Bare number → pixels.
                    let num = val.trim_end_matches("px");
                    result.push(("width".into(), format!("{num}px")));
                }
            }
            "height" if matches!(tag, "table" | "td" | "th" | "tr" | "img") => {
                if val.ends_with('%') {
                    result.push(("height".into(), val.clone()));
                } else {
                    let num = val.trim_end_matches("px");
                    result.push(("height".into(), format!("{num}px")));
                }
            }
            "align" => {
                match val.as_str() {
                    "center" => {
                        if matches!(tag, "table" | "div" | "p" | "hr") {
                            result.push(("margin-left".into(), "auto".into()));
                            result.push(("margin-right".into(), "auto".into()));
                        } else {
                            result.push(("text-align".into(), "center".into()));
                        }
                    }
                    "right" => result.push(("text-align".into(), "right".into())),
                    "left" => result.push(("text-align".into(), "left".into())),
                    "justify" => result.push(("text-align".into(), "justify".into())),
                    _ => {}
                }
            }
            "valign" => {
                result.push(("vertical-align".into(), val.clone()));
            }
            "border" if matches!(tag, "table" | "img") => {
                let num = val.trim_end_matches("px");
                if let Ok(w) = num.parse::<f32>() {
                    if w > 0.0 {
                        result.push(("border-width".into(), format!("{w}px")));
                        result.push(("border-style".into(), "solid".into()));
                    }
                }
            }
            "cellpadding" if tag == "table" => {
                // cellpadding applies to child td/th cells, but we store it
                // as a custom property so layout can read it.
                let num = val.trim_end_matches("px");
                result.push(("--cellpadding".into(), format!("{num}px")));
            }
            "cellspacing" if tag == "table" => {
                let num = val.trim_end_matches("px");
                result.push(("border-spacing".into(), format!("{num}px")));
            }
            "nowrap" if matches!(tag, "td" | "th") => {
                result.push(("white-space".into(), "nowrap".into()));
            }
            _ => {}
        }
    }

    result
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

    // ── outline shorthand ────────────────────────────────────────────────────

    #[test]
    fn outline_shorthand_full() {
        let m = expand("outline", "2px solid blue");
        assert_eq!(m.get("outline-width").map(String::as_str), Some("2px"));
        assert_eq!(m.get("outline-style").map(String::as_str), Some("solid"));
        assert_eq!(m.get("outline-color").map(String::as_str), Some("blue"));
    }

    #[test]
    fn outline_shorthand_none() {
        let m = expand("outline", "none");
        assert_eq!(m.get("outline-style").map(String::as_str), Some("none"));
    }

    // ── columns shorthand ────────────────────────────────────────────────────

    #[test]
    fn columns_shorthand_count() {
        let m = expand("columns", "3");
        assert_eq!(m.get("column-count").map(String::as_str), Some("3"));
    }

    // ── word-wrap alias ──────────────────────────────────────────────────────

    #[test]
    fn word_wrap_maps_to_overflow_wrap() {
        let m = expand("word-wrap", "break-word");
        assert_eq!(m.get("overflow-wrap").map(String::as_str), Some("break-word"));
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
    fn before_pseudo_injects_element_child() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "style".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text(
                            r#"p::before { content: ">>"; color: red; }"#.into(),
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
            assert!(
                !children.is_empty(),
                "p should have children after ::before injection"
            );
            // The first child should be a <nova-before> element.
            match &children[0] {
                DomNode::Element { tag, attributes, children: inner } => {
                    assert_eq!(tag, "nova-before", "first child should be nova-before");
                    // Should have data-nova-style with the pseudo-element's styles.
                    let style = attributes.iter().find(|(k, _)| k == "data-nova-style");
                    assert!(style.is_some(), "nova-before should have data-nova-style");
                    assert!(style.unwrap().1.contains("color: red"), "should contain color: red");
                    // Should have a text child with ">>"
                    assert_eq!(inner.len(), 1);
                    assert!(matches!(&inner[0], DomNode::Text(t) if t == ">>"));
                }
                other => panic!("expected nova-before Element, got: {:?}", other),
            }
        } else {
            panic!("expected Element for p");
        }
    }

    #[test]
    fn after_pseudo_injects_element_child() {
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
            match last {
                DomNode::Element { tag, children: inner, .. } => {
                    assert_eq!(tag, "nova-after", "last child should be nova-after");
                    assert_eq!(inner.len(), 1);
                    assert!(matches!(&inner[0], DomNode::Text(t) if t == " [end]"));
                }
                other => panic!("expected nova-after Element, got: {:?}", other),
            }
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
            // With content: none, no pseudo-element should be injected.
            assert_eq!(
                children.len(),
                1,
                "p should have exactly 1 child (no ::before injection with content:none)"
            );
        } else {
            panic!("expected Element for p");
        }
    }

    #[test]
    fn empty_content_injects_empty_element() {
        // `content: ""` is commonly used for clearfix and layout tricks.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "style".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text(
                            r#"div::after { content: ""; display: block; clear: both; }"#.into(),
                        )],
                    },
                    DomNode::Element {
                        tag: "div".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("Content".into())],
                    },
                ],
            }],
        };

        let result = compute_styles(dom, &[], 800.0);
        let div = find_element(&result, "div").expect("should find div");
        if let DomNode::Element { children, .. } = div {
            assert!(children.len() >= 2, "div should have 2+ children (original + ::after)");
            let last = children.last().unwrap();
            match last {
                DomNode::Element { tag, children: inner, attributes } => {
                    assert_eq!(tag, "nova-after");
                    // Empty content means no text child.
                    assert!(inner.is_empty(), "empty content should have no text child");
                    // Should have display: block and clear: both in styles.
                    let style = attributes.iter().find(|(k, _)| k == "data-nova-style");
                    assert!(style.is_some(), "should have data-nova-style");
                    let style_val = &style.unwrap().1;
                    assert!(style_val.contains("display: block"), "style: {style_val}");
                    assert!(style_val.contains("clear: both"), "style: {style_val}");
                }
                other => panic!("expected nova-after Element, got: {:?}", other),
            }
        } else {
            panic!("expected Element for div");
        }
    }

    #[test]
    fn legacy_single_colon_syntax() {
        // `:before` (single colon) should work the same as `::before`.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "style".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text(
                            r#"p:before { content: "->"; }"#.into(),
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
            assert!(children.len() >= 2, "p should have 2+ children with :before");
            match &children[0] {
                DomNode::Element { tag, children: inner, .. } => {
                    assert_eq!(tag, "nova-before");
                    assert!(matches!(&inner[0], DomNode::Text(t) if t == "->"));
                }
                other => panic!("expected nova-before, got: {:?}", other),
            }
        }
    }

    #[test]
    fn unicode_escape_in_content() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "style".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text(
                            r#"span::before { content: "\f00c"; }"#.into(),
                        )],
                    },
                    DomNode::Element {
                        tag: "span".into(),
                        attributes: vec![],
                        children: vec![],
                    },
                ],
            }],
        };

        let result = compute_styles(dom, &[], 800.0);
        let span = find_element(&result, "span").expect("should find span");
        if let DomNode::Element { children, .. } = span {
            assert!(!children.is_empty(), "should have ::before child");
            match &children[0] {
                DomNode::Element { tag, children: inner, .. } => {
                    assert_eq!(tag, "nova-before");
                    // \f00c = U+F00C (FontAwesome checkmark)
                    let expected = char::from_u32(0xf00c).unwrap().to_string();
                    assert!(matches!(&inner[0], DomNode::Text(t) if *t == expected));
                }
                other => panic!("expected nova-before, got: {:?}", other),
            }
        }
    }

    #[test]
    fn attr_content_function() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "style".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text(
                            r#"span::before { content: attr(data-label); }"#.into(),
                        )],
                    },
                    DomNode::Element {
                        tag: "span".into(),
                        attributes: vec![("data-label".into(), "Hello!".into())],
                        children: vec![],
                    },
                ],
            }],
        };

        let result = compute_styles(dom, &[], 800.0);
        let span = find_element(&result, "span").expect("should find span");
        if let DomNode::Element { children, .. } = span {
            assert!(!children.is_empty(), "should have ::before child");
            match &children[0] {
                DomNode::Element { tag, children: inner, .. } => {
                    assert_eq!(tag, "nova-before");
                    assert!(matches!(&inner[0], DomNode::Text(t) if t == "Hello!"));
                }
                other => panic!("expected nova-before, got: {:?}", other),
            }
        }
    }

    #[test]
    fn resolve_css_content_escapes_hex() {
        // Font Awesome icon: \f00c -> U+F00C (check mark)
        assert_eq!(resolve_css_content_escapes("\\f00c"), "\u{f00c}");
    }

    #[test]
    fn resolve_css_content_escapes_mixed() {
        // Mixed text and escapes.
        assert_eq!(resolve_css_content_escapes("A\\f00c B"), "A\u{f00c}B");
    }

    #[test]
    fn resolve_css_content_escapes_no_escapes() {
        assert_eq!(resolve_css_content_escapes("hello world"), "hello world");
    }

    #[test]
    fn resolve_css_content_escapes_six_digit_hex() {
        // Full 6-digit hex: \01F600 (emoji smiley)
        assert_eq!(resolve_css_content_escapes("\\01F600"), "\u{1F600}");
    }
}

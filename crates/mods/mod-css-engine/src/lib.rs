//! # mod-css-engine
//!
//! NOVA Mod for CSS parsing and style computation. Handles `ParseStylesheet`
//! and `ComputeStyles` capabilities.
//!
//! ## Architecture
//!
//! The CSS engine is split into several modules:
//!
//! - **`parser`**: Parses CSS stylesheets and inline styles using the `cssparser` crate.
//! - **`selector`**: Parses and matches CSS selectors (tag, class, id, universal,
//!   descendant combinator, selector lists).
//! - **`values`**: Parses CSS values (colors, lengths, keywords, etc.).
//! - **`defaults`**: User-agent default styles for HTML elements.
//! - **`cascade`**: The cascade algorithm — collects matching rules, sorts by specificity,
//!   and resolves the final computed style for each element.
//!
//! ## Style output
//!
//! `ComputeStyles` walks the DOM, computes styles for each element, and writes
//! the result as a `data-nova-style` attribute (serialized as an inline CSS string).
//! Downstream mods (e.g., Layout) can read this attribute to get computed styles.

use std::sync::Arc;

use async_trait::async_trait;
use semver::Version;
use tracing::{debug, info};

use nova_mod_api::{
    capability::CapabilityType,
    content::{ContentRequest, StyleMap, TypedData},
    error::NovaError,
    manifest::ModManifest,
    permission::TrustLevel,
    trigger::{ContentTrigger, TriggerCondition},
    types::ModId,
    CoreApi, NovaMod,
};

pub mod cascade;
pub mod defaults;
pub mod parser;
pub mod selector;
pub mod values;

/// The CSS engine mod.
pub struct CssEngineMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
}

impl CssEngineMod {
    /// Create a new `CssEngineMod` instance.
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.css-engine"),
            name: "NOVA CSS Engine".into(),
            version: Version::new(0, 1, 0),
            description: "CSS parser and style computation engine".into(),
            capabilities: vec![
                CapabilityType::ParseStylesheet,
                CapabilityType::ComputeStyles,
            ],
            permissions: vec![],
            dependencies: vec![],
            triggers: vec![ContentTrigger {
                condition: TriggerCondition::MimeType("text/css".into()),
                mod_id: ModId::new("org.nova.css-engine"),
                priority: 100,
            }],
            min_core_version: Version::new(0, 1, 0),
            trust_level: TrustLevel::Core,
        };

        Self {
            manifest,
            core: None,
        }
    }
}

impl Default for CssEngineMod {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NovaMod for CssEngineMod {
    fn manifest(&self) -> &ModManifest {
        &self.manifest
    }

    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "css-engine mod initializing");
        self.core = Some(core);
        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            ContentRequest::ParseCss { source, base_url } => {
                debug!(
                    len = source.len(),
                    base_url = ?base_url,
                    "parsing CSS stylesheet"
                );

                // Parse the stylesheet into rules and convert to a StyleMap
                // containing all declarations (for use by ComputeStyles later).
                let rules = parser::parse_stylesheet(&source);
                let mut all_props = Vec::new();
                for rule in &rules {
                    for (prop, val) in &rule.declarations {
                        let style_val = values::parse_value(prop, val);
                        all_props.push((prop.clone(), style_val));
                    }
                }

                debug!(
                    rule_count = rules.len(),
                    declaration_count = all_props.len(),
                    "parsed CSS stylesheet"
                );

                Ok(TypedData::Styles(StyleMap {
                    properties: all_props,
                }))
            }
            ContentRequest::ComputeStyles { dom, stylesheets } => {
                info!(
                    stylesheet_count = stylesheets.len(),
                    "CSS ENGINE: computing styles for DOM"
                );

                // Extract the DOM from the TypedData wrapper.
                let dom_node = match *dom {
                    TypedData::Dom(node) => node,
                    _ => {
                        return Err(NovaError::UnsupportedContent(
                            "ComputeStyles expects TypedData::Dom".into(),
                        ));
                    }
                };

                // Extract CSS text from any TypedData::Text or TypedData::Styles
                // stylesheet entries.
                let mut extra_css = Vec::new();
                for sheet in &stylesheets {
                    match sheet {
                        TypedData::Text(css) => extra_css.push(css.clone()),
                        _ => {
                            // Other stylesheet types are ignored for now.
                            debug!("ignoring non-text stylesheet in ComputeStyles");
                        }
                    }
                }

                // Run the cascade: extract <style> elements, parse CSS, match
                // selectors, apply specificity ordering, and write computed
                // styles as data-nova-style attributes.
                let styled_dom = cascade::compute_styles(dom_node, &extra_css);

                Ok(TypedData::Dom(styled_dom))
            }
            other => Err(NovaError::UnsupportedContent(format!(
                "css-engine cannot handle request: {other:?}"
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "css-engine mod shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::content::DomNode;

    #[test]
    fn manifest_provides_capabilities() {
        let m = CssEngineMod::new();
        assert!(m.manifest().provides(&CapabilityType::ParseStylesheet));
        assert!(m.manifest().provides(&CapabilityType::ComputeStyles));
    }

    #[test]
    fn default_h1_style() {
        let style = defaults::default_style_for_tag("h1");
        let display = style
            .properties
            .iter()
            .find(|(k, _)| k == "display")
            .unwrap();
        assert!(matches!(&display.1, nova_mod_api::content::StyleValue::Keyword(k) if k == "block"));

        let fs = style
            .properties
            .iter()
            .find(|(k, _)| k == "font-size")
            .unwrap();
        assert!(matches!(&fs.1, nova_mod_api::content::StyleValue::Px(v) if (*v - 32.0).abs() < 0.01));
    }

    #[test]
    fn inline_elements() {
        for tag in &["span", "a", "em", "strong"] {
            let style = defaults::default_style_for_tag(tag);
            let display = style
                .properties
                .iter()
                .find(|(k, _)| k == "display")
                .unwrap();
            assert!(
                matches!(&display.1, nova_mod_api::content::StyleValue::Keyword(k) if k == "inline"),
                "{tag} should be inline"
            );
        }
    }

    #[test]
    fn parse_and_apply_full_pipeline() {
        // Build a DOM with embedded CSS.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "head".into(),
                        attributes: vec![],
                        children: vec![DomNode::Element {
                            tag: "style".into(),
                            attributes: vec![],
                            children: vec![DomNode::Text(
                                r#"
                                body { background-color: #f0f0f2; margin: 0; }
                                h1 { color: #333; }
                                .container { max-width: 960px; margin: 0 auto; }
                                "#
                                .into(),
                            )],
                        }],
                    },
                    DomNode::Element {
                        tag: "body".into(),
                        attributes: vec![],
                        children: vec![
                            DomNode::Element {
                                tag: "div".into(),
                                attributes: vec![("class".into(), "container".into())],
                                children: vec![DomNode::Element {
                                    tag: "h1".into(),
                                    attributes: vec![],
                                    children: vec![DomNode::Text("Hello, NOVA!".into())],
                                }],
                            },
                        ],
                    },
                ],
            }],
        };

        let result = cascade::compute_styles(dom, &[]);

        // Verify the result is a valid DOM.
        match &result {
            DomNode::Document { children } => {
                assert!(!children.is_empty());
            }
            _ => panic!("expected Document"),
        }

        // Find the h1 and check it got styled.
        fn find_element<'a>(node: &'a DomNode, tag_name: &str) -> Option<&'a DomNode> {
            match node {
                DomNode::Element { tag, children, .. } => {
                    if tag == tag_name {
                        return Some(node);
                    }
                    for child in children {
                        if let Some(found) = find_element(child, tag_name) {
                            return Some(found);
                        }
                    }
                    None
                }
                DomNode::Document { children } => {
                    for child in children {
                        if let Some(found) = find_element(child, tag_name) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let h1 = find_element(&result, "h1").expect("should find h1");
        let style = h1.attr("data-nova-style").expect("h1 must have data-nova-style");
        assert!(style.contains("color: #333"), "h1 style should contain color: #333, got: {style}");
        assert!(style.contains("font-size: 32px"), "h1 style should contain font-size");

        let body = find_element(&result, "body").expect("should find body");
        let body_style = body.attr("data-nova-style").expect("body must have data-nova-style");
        assert!(
            body_style.contains("background-color: #f0f0f2"),
            "body style should contain background-color, got: {body_style}"
        );
    }

    #[test]
    fn example_com_css() {
        // Test with CSS similar to what example.com uses.
        let css = r#"
            body {
                background-color: #f0f0f2;
                margin: 0;
                padding: 0;
                font-family: -apple-system, system-ui, BlinkMacSystemFont, "Segoe UI",
                    "Open Sans", "Helvetica Neue", Helvetica, Arial, sans-serif;
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
            @media (max-width: 700px) {
                div {
                    margin: 0 auto;
                    width: auto;
                }
            }
        "#;

        let rules = parser::parse_stylesheet(css);
        // Should parse body and div rules. a:link/a:visited may partially parse.
        // @media is skipped.
        assert!(rules.len() >= 2, "expected at least 2 rules, got {}", rules.len());

        // Check body rule.
        let body_rule = rules.iter().find(|r| {
            r.selector.selectors.iter().any(|s| {
                s.parts.iter().any(|p| p.tag.as_deref() == Some("body"))
            })
        });
        assert!(body_rule.is_some(), "should find body rule");
        let body_decls = &body_rule.unwrap().declarations;
        assert!(
            body_decls.iter().any(|(p, _)| p == "background-color"),
            "body should have background-color"
        );
    }
}

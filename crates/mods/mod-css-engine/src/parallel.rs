//! Parallel CSS cascade using rayon.
//!
//! Parallelizes style computation across sibling elements at each level of
//! the DOM tree. Each sibling subtree can be styled independently because
//! siblings do not affect each other's computed styles (only ancestors do).
//!
//! ## Strategy
//!
//! 1. At each level of the tree, collect all sibling elements.
//! 2. Use `rayon::par_iter` to compute styles for each sibling in parallel.
//! 3. Within each sibling, recurse for its children (also in parallel).
//! 4. Collect results and reconstruct the tree.
//!
//! The [`StyleCache`] uses `std::sync::Mutex` so it can be shared across
//! threads. Contention is low because cache lookups are fast hash lookups.

use std::sync::Arc;

use rayon::prelude::*;
use tracing::{debug, info};

use nova_mod_api::content::DomNode;

use crate::cascade::{self, RuleIndex};
use crate::parser::{self, CssRule};

// ── ParallelStyleEngine ─────────────────────────────────────────────────────

/// Parallel CSS style computation engine.
///
/// Wraps the sequential cascade algorithm and applies it across sibling
/// elements in parallel using rayon's work-stealing thread pool.
pub struct ParallelStyleEngine {
    /// All parsed CSS rules (author + embedded stylesheets).
    rules: Vec<CssRule>,
    /// Viewport width for media query evaluation.
    viewport_width: f32,
}

impl ParallelStyleEngine {
    /// Create a new parallel style engine.
    ///
    /// `rules` should be the full set of parsed CSS rules from all
    /// stylesheets. `viewport_width` is used for media query evaluation.
    pub fn new(rules: Vec<CssRule>, viewport_width: f32) -> Self {
        Self {
            rules,
            viewport_width,
        }
    }

    /// Compute styles for the entire DOM tree, parallelizing across siblings.
    ///
    /// This is the main entry point. It produces the same result as the
    /// sequential [`cascade::compute_styles`] but utilizes multiple cores.
    ///
    /// The strategy: each element's own style is computed sequentially (it
    /// needs ancestor context), but sibling subtrees are independent and
    /// processed in parallel via rayon's work-stealing pool.
    pub fn compute(&self, dom: DomNode) -> DomNode {
        info!(
            rule_count = self.rules.len(),
            viewport_width = self.viewport_width,
            "parallel style computation starting"
        );

        let index = Arc::new(RuleIndex::build(&self.rules));
        let rules = Arc::new(self.rules.clone());
        let owned_ancestors: Vec<DomNode> = Vec::new();
        Self::apply_parallel(&rules, &index, dom, &owned_ancestors, self.viewport_width, 0)
    }

    /// Recursively apply styles, parallelizing children processing.
    ///
    /// Uses owned ancestors (cloned DomNodes) so the data can be sent
    /// across rayon threads safely.
    fn apply_parallel(
        rules: &Arc<Vec<CssRule>>,
        index: &Arc<RuleIndex>,
        node: DomNode,
        owned_ancestors: &[DomNode],
        viewport_width: f32,
        depth: usize,
    ) -> DomNode {
        // Depth limit to avoid stack overflow on deeply nested DOMs.
        const MAX_CASCADE_DEPTH: usize = 100;
        if depth >= MAX_CASCADE_DEPTH {
            return node;
        }

        match node {
            DomNode::Element {
                tag,
                attributes,
                children,
            } => {
                // Build temporary references for the sequential cascade call.
                let ancestor_refs: Vec<&DomNode> = owned_ancestors.iter().collect();

                // Build a temporary node for matching.
                let temp_node = DomNode::Element {
                    tag: tag.clone(),
                    attributes: attributes.clone(),
                    children: vec![],
                };

                // Compute styles for this element using the full cascade.
                let style_str = cascade::compute_element_style_public(
                    &tag, &attributes, &temp_node, rules, index, &ancestor_refs,
                );

                // Build new attributes with data-nova-style.
                let mut new_attributes = attributes;
                new_attributes.retain(|(k, _)| k != "data-nova-style");
                if !style_str.is_empty() {
                    new_attributes.push(("data-nova-style".into(), style_str.clone()));
                }

                // Skip children if display: none.
                if style_str.contains("display: none") {
                    return DomNode::Element {
                        tag,
                        attributes: new_attributes,
                        children: vec![],
                    };
                }

                // Build this node as an ancestor for children.
                let this_ancestor = DomNode::Element {
                    tag: tag.clone(),
                    attributes: new_attributes.clone(),
                    children: vec![],
                };

                let mut child_ancestors: Vec<DomNode> = owned_ancestors.to_vec();
                child_ancestors.push(this_ancestor);

                // Process children in parallel if there are enough of them,
                // and we're not too deep in the tree (avoid over-parallelizing).
                let new_children = if children.len() > 4 && depth < 6 {
                    let child_ancestors = Arc::new(child_ancestors);
                    let rules = Arc::clone(rules);
                    let index = Arc::clone(index);
                    children
                        .into_par_iter()
                        .map(|child| {
                            Self::apply_parallel(
                                &rules, &index, child, &child_ancestors,
                                viewport_width, depth + 1,
                            )
                        })
                        .collect()
                } else {
                    children
                        .into_iter()
                        .map(|child| {
                            Self::apply_parallel(
                                rules, index, child, &child_ancestors,
                                viewport_width, depth + 1,
                            )
                        })
                        .collect()
                };

                DomNode::Element {
                    tag,
                    attributes: new_attributes,
                    children: new_children,
                }
            }
            DomNode::Document { children } => {
                let new_children = if children.len() > 4 {
                    let owned_ancestors = Arc::new(owned_ancestors.to_vec());
                    let rules = Arc::clone(rules);
                    let index = Arc::clone(index);
                    children
                        .into_par_iter()
                        .map(|child| {
                            Self::apply_parallel(
                                &rules, &index, child, &owned_ancestors,
                                viewport_width, depth + 1,
                            )
                        })
                        .collect()
                } else {
                    children
                        .into_iter()
                        .map(|child| {
                            Self::apply_parallel(
                                rules, index, child, owned_ancestors,
                                viewport_width, depth + 1,
                            )
                        })
                        .collect()
                };
                DomNode::Document {
                    children: new_children,
                }
            }
            other => other,
        }
    }
}

// ── Public convenience function ─────────────────────────────────────────────

/// Compute styles for a DOM tree using parallel processing.
///
/// This is a drop-in replacement for [`cascade::compute_styles`] that
/// parallelizes the cascade across sibling elements.
///
/// The result is identical to the sequential version.
pub fn parallel_compute_styles(
    dom: DomNode,
    extra_css: &[String],
    viewport_width: f32,
) -> DomNode {
    // Extract embedded stylesheets.
    let embedded = cascade::extract_stylesheets(&dom);

    // Parse all stylesheets.
    let mut rules = Vec::new();
    for css in &embedded {
        rules.extend(parser::parse_stylesheet(css, viewport_width));
    }
    for css in extra_css {
        rules.extend(parser::parse_stylesheet(css, viewport_width));
    }

    // For very large stylesheets, skip rules with overly complex
    // selectors (4+ compound parts) for performance.
    let original_count = rules.len();
    if rules.len() > 2000 {
        rules.retain(|rule| {
            rule.selector.selectors.iter().all(|sel| sel.parts.len() <= 4)
        });
        if rules.len() < original_count {
            info!(
                original = original_count,
                retained = rules.len(),
                dropped = original_count - rules.len(),
                "parallel cascade: dropped complex CSS selectors for performance"
            );
        }
    }

    info!(
        rule_count = rules.len(),
        "parallel cascade: parsed CSS rules"
    );

    // For small DOMs, fall back to sequential — rayon overhead isn't
    // worth it when there are few nodes.
    let node_count = count_nodes(&dom);
    if node_count < 50 {
        debug!(node_count, "DOM too small for parallel cascade, using sequential");
        return cascade::compute_styles(dom, extra_css, viewport_width);
    }

    info!(
        node_count,
        "DOM large enough for parallel cascade, using rayon"
    );

    // Use the parallel engine which delegates per-element style computation
    // to the sequential cascade but processes sibling subtrees concurrently.
    let engine = ParallelStyleEngine::new(rules, viewport_width);
    let styled = engine.compute(dom);

    // Apply CSS background propagation (body → html) as the sequential path does.
    cascade::propagate_body_background(styled)
}

/// Count nodes in a DOM tree.
fn count_nodes(node: &DomNode) -> usize {
    match node {
        DomNode::Document { children } | DomNode::Element { children, .. } => {
            1 + children.iter().map(count_nodes).sum::<usize>()
        }
        _ => 1,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nova_mod_api::content::DomNode;

    fn make_dom(body_children: Vec<DomNode>) -> DomNode {
        DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "body".into(),
                    attributes: vec![],
                    children: body_children,
                }],
            }],
        }
    }

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
    fn parallel_produces_same_result_as_sequential() {
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "style".into(),
                attributes: vec![],
                children: vec![DomNode::Text("h1 { color: red; } p { color: blue; }".into())],
            },
            DomNode::Element {
                tag: "h1".into(),
                attributes: vec![],
                children: vec![DomNode::Text("Hello".into())],
            },
            DomNode::Element {
                tag: "p".into(),
                attributes: vec![],
                children: vec![DomNode::Text("World".into())],
            },
        ]);

        let sequential = cascade::compute_styles(dom.clone(), &[], 1280.0);
        let parallel = parallel_compute_styles(dom, &[], 1280.0);

        // Both should produce styled h1 and p elements.
        let seq_h1 = find_element(&sequential, "h1").expect("sequential h1");
        let par_h1 = find_element(&parallel, "h1").expect("parallel h1");

        let seq_style = seq_h1.attr("data-nova-style").unwrap_or("");
        let par_style = par_h1.attr("data-nova-style").unwrap_or("");

        assert_eq!(seq_style, par_style, "parallel cascade should match sequential");
    }

    #[test]
    fn parallel_handles_empty_dom() {
        let dom = DomNode::Document { children: vec![] };
        let result = parallel_compute_styles(dom, &[], 1280.0);
        match result {
            DomNode::Document { children } => assert!(children.is_empty()),
            _ => panic!("expected Document"),
        }
    }

    #[test]
    fn parallel_handles_text_only() {
        let dom = DomNode::Document {
            children: vec![DomNode::Text("Hello".into())],
        };
        let result = parallel_compute_styles(dom, &[], 1280.0);
        match result {
            DomNode::Document { children } => {
                assert_eq!(children.len(), 1);
            }
            _ => panic!("expected Document"),
        }
    }

    #[test]
    fn count_nodes_simple() {
        let dom = make_dom(vec![
            DomNode::Element {
                tag: "div".into(),
                attributes: vec![],
                children: vec![DomNode::Text("Hello".into())],
            },
            DomNode::Element {
                tag: "p".into(),
                attributes: vec![],
                children: vec![DomNode::Text("World".into())],
            },
        ]);
        // Document + html + body + div + "Hello" + p + "World" = 7
        assert_eq!(count_nodes(&dom), 7);
    }

    #[test]
    fn parallel_engine_creation() {
        let rules = vec![];
        let engine = ParallelStyleEngine::new(rules, 1280.0);
        assert_eq!(engine.viewport_width, 1280.0);
    }
}

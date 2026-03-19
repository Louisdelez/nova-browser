//! CSS selector parsing and matching.
//!
//! Supports: tag, class, id, universal, descendant combinator, and selector
//! lists (comma-separated). Computes specificity for cascade ordering.

use nova_mod_api::content::DomNode;

/// A parsed CSS selector (possibly comma-separated into multiple alternatives).
#[derive(Debug, Clone)]
pub struct SelectorList {
    /// Individual selectors separated by commas.
    pub selectors: Vec<Selector>,
}

/// A single CSS selector (a sequence of simple selectors with combinators).
///
/// Stored as a list of compound selectors with the relationship between each
/// pair (currently only descendant combinator is supported).
#[derive(Debug, Clone)]
pub struct Selector {
    /// Compound selectors from outermost (leftmost) to innermost (rightmost).
    /// e.g., `div .foo p` => [CompoundSelector(div), CompoundSelector(.foo), CompoundSelector(p)]
    pub parts: Vec<CompoundSelector>,
}

/// A compound selector: one or more simple selectors that all apply to the same element.
/// e.g., `div.foo#bar` => tag="div", classes=["foo"], id=Some("bar")
#[derive(Debug, Clone, Default)]
pub struct CompoundSelector {
    /// Tag name (e.g. "div"), or None for no tag constraint.
    pub tag: Option<String>,
    /// Class names (e.g. ["foo", "bar"]).
    pub classes: Vec<String>,
    /// ID (e.g. "main").
    pub id: Option<String>,
    /// Whether this is the universal selector `*` (matches anything).
    pub universal: bool,
}

/// Specificity as (a, b, c) where a = ID count, b = class count, c = tag count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity(pub u32, pub u32, pub u32);

impl Specificity {
    /// Inline styles have the highest specificity.
    pub fn inline() -> Self {
        Specificity(1000, 0, 0)
    }
}

impl SelectorList {
    /// Parse a selector list from a CSS selector string.
    pub fn parse(input: &str) -> Option<Self> {
        let parts: Vec<&str> = input.split(',').collect();
        let mut selectors = Vec::new();
        for part in parts {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some(sel) = Selector::parse(part) {
                selectors.push(sel);
            }
        }
        if selectors.is_empty() {
            None
        } else {
            Some(SelectorList { selectors })
        }
    }

    /// Check if this selector list matches the given element in the given context.
    /// `ancestors` is the list of ancestor elements from root to parent (not including the element itself).
    pub fn matches(&self, node: &DomNode, ancestors: &[&DomNode]) -> bool {
        self.selectors.iter().any(|s| s.matches(node, ancestors))
    }

    /// Return the highest specificity among the matching selectors.
    pub fn specificity(&self) -> Specificity {
        self.selectors
            .iter()
            .map(|s| s.specificity())
            .max()
            .unwrap_or(Specificity(0, 0, 0))
    }
}

impl Selector {
    /// Parse a single selector (no commas).
    fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }

        // Split by whitespace for descendant combinator.
        let tokens: Vec<&str> = input.split_whitespace().collect();
        let mut parts = Vec::new();

        for token in tokens {
            if let Some(compound) = CompoundSelector::parse(token) {
                parts.push(compound);
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(Selector { parts })
        }
    }

    /// Check if this selector matches the given element.
    fn matches(&self, node: &DomNode, ancestors: &[&DomNode]) -> bool {
        if self.parts.is_empty() {
            return false;
        }

        // The rightmost compound must match the node itself.
        let last = &self.parts[self.parts.len() - 1];
        if !last.matches(node) {
            return false;
        }

        // If there's only one part, we're done.
        if self.parts.len() == 1 {
            return true;
        }

        // Walk the remaining parts right-to-left against ancestors (descendant combinator).
        // Each part must match some ancestor, and we go from inner to outer.
        let mut part_idx = self.parts.len() - 2; // Start from second-to-last
        let mut ancestor_idx = ancestors.len(); // Start from direct parent

        loop {
            if ancestor_idx == 0 {
                // No more ancestors to check; if we haven't matched all parts, fail.
                return false;
            }
            ancestor_idx -= 1;

            if self.parts[part_idx].matches(ancestors[ancestor_idx]) {
                if part_idx == 0 {
                    return true; // All parts matched.
                }
                part_idx -= 1;
            }
            // Otherwise, keep looking at further ancestors (descendant, not child).
        }
    }

    /// Compute the specificity of this selector.
    fn specificity(&self) -> Specificity {
        let mut a = 0u32;
        let mut b = 0u32;
        let mut c = 0u32;
        for part in &self.parts {
            if part.id.is_some() {
                a += 1;
            }
            b += part.classes.len() as u32;
            if part.tag.is_some() {
                c += 1;
            }
            // Universal selector contributes nothing.
        }
        Specificity(a, b, c)
    }
}

impl CompoundSelector {
    /// Parse a compound selector like `div.foo#bar` or `*` or `.class`.
    fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }

        if input == "*" {
            return Some(CompoundSelector {
                universal: true,
                ..Default::default()
            });
        }

        let mut sel = CompoundSelector::default();
        let mut chars = input.chars().peekable();
        let mut current = String::new();
        let mut current_type: Option<char> = None; // None=tag, '#'=id, '.'=class

        let flush = |sel: &mut CompoundSelector, ctype: Option<char>, val: &str| {
            if val.is_empty() {
                return;
            }
            match ctype {
                None => sel.tag = Some(val.to_ascii_lowercase()),
                Some('#') => sel.id = Some(val.to_string()),
                Some('.') => sel.classes.push(val.to_string()),
                _ => {}
            }
        };

        while let Some(&ch) = chars.peek() {
            match ch {
                '#' | '.' => {
                    flush(&mut sel, current_type, &current);
                    current.clear();
                    current_type = Some(ch);
                    chars.next();
                }
                _ => {
                    current.push(ch);
                    chars.next();
                }
            }
        }
        flush(&mut sel, current_type, &current);

        // If nothing was set, treat as universal.
        if sel.tag.is_none() && sel.id.is_none() && sel.classes.is_empty() {
            return None;
        }

        Some(sel)
    }

    /// Check if this compound selector matches a DOM node.
    fn matches(&self, node: &DomNode) -> bool {
        if self.universal {
            return matches!(node, DomNode::Element { .. });
        }

        match node {
            DomNode::Element {
                tag, attributes, ..
            } => {
                // Check tag.
                if let Some(ref expected_tag) = self.tag {
                    if tag.to_ascii_lowercase() != *expected_tag {
                        return false;
                    }
                }

                // Check ID.
                if let Some(ref expected_id) = self.id {
                    let actual_id = attributes
                        .iter()
                        .find(|(k, _)| k == "id")
                        .map(|(_, v)| v.as_str());
                    if actual_id != Some(expected_id.as_str()) {
                        return false;
                    }
                }

                // Check classes.
                if !self.classes.is_empty() {
                    let class_attr = attributes
                        .iter()
                        .find(|(k, _)| k == "class")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    let actual_classes: Vec<&str> = class_attr.split_whitespace().collect();
                    for cls in &self.classes {
                        if !actual_classes.contains(&cls.as_str()) {
                            return false;
                        }
                    }
                }

                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elem(tag: &str, attrs: &[(&str, &str)]) -> DomNode {
        DomNode::Element {
            tag: tag.into(),
            attributes: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            children: vec![],
        }
    }

    #[test]
    fn tag_selector() {
        let sel = SelectorList::parse("div").unwrap();
        let node = elem("div", &[]);
        assert!(sel.matches(&node, &[]));

        let node2 = elem("span", &[]);
        assert!(!sel.matches(&node2, &[]));
    }

    #[test]
    fn class_selector() {
        let sel = SelectorList::parse(".foo").unwrap();
        let node = elem("div", &[("class", "foo bar")]);
        assert!(sel.matches(&node, &[]));

        let node2 = elem("div", &[("class", "baz")]);
        assert!(!sel.matches(&node2, &[]));
    }

    #[test]
    fn id_selector() {
        let sel = SelectorList::parse("#main").unwrap();
        let node = elem("div", &[("id", "main")]);
        assert!(sel.matches(&node, &[]));
    }

    #[test]
    fn compound_selector() {
        let sel = SelectorList::parse("div.foo#bar").unwrap();
        let node = elem("div", &[("class", "foo"), ("id", "bar")]);
        assert!(sel.matches(&node, &[]));

        let node2 = elem("span", &[("class", "foo"), ("id", "bar")]);
        assert!(!sel.matches(&node2, &[]));
    }

    #[test]
    fn descendant_selector() {
        let sel = SelectorList::parse("div p").unwrap();
        let div = elem("div", &[]);
        let p = elem("p", &[]);
        assert!(sel.matches(&p, &[&div]));
        assert!(!sel.matches(&p, &[]));
    }

    #[test]
    fn selector_list() {
        let sel = SelectorList::parse("h1, h2, h3").unwrap();
        assert_eq!(sel.selectors.len(), 3);
        assert!(sel.matches(&elem("h2", &[]), &[]));
        assert!(!sel.matches(&elem("h4", &[]), &[]));
    }

    #[test]
    fn universal_selector() {
        let sel = SelectorList::parse("*").unwrap();
        assert!(sel.matches(&elem("anything", &[]), &[]));
    }

    #[test]
    fn specificity_ordering() {
        let id_sel = SelectorList::parse("#foo").unwrap();
        let class_sel = SelectorList::parse(".bar").unwrap();
        let tag_sel = SelectorList::parse("div").unwrap();
        assert!(id_sel.specificity() > class_sel.specificity());
        assert!(class_sel.specificity() > tag_sel.specificity());
    }
}

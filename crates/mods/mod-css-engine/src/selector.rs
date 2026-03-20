//! CSS selector parsing and matching.
//!
//! Supports: tag, class, id, universal, descendant combinator, selector
//! lists (comma-separated), and `:nth-child(An+B)` / `:nth-of-type(An+B)`
//! pseudo-classes. Computes specificity for cascade ordering.

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
    /// Optional pseudo-element (`::before`, `::after`) attached to this selector.
    pub pseudo_element: Option<PseudoElement>,
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
    /// Pseudo-classes attached to this compound selector.
    pub pseudos: Vec<PseudoClass>,
}

/// A CSS pseudo-class.
#[derive(Debug, Clone)]
pub enum PseudoClass {
    /// `:nth-child(An+B)` — matches by position among all sibling elements.
    NthChild(NthFormula),
    /// `:nth-of-type(An+B)` — matches by position among siblings of the same tag.
    NthOfType(NthFormula),
    /// `:first-child` — equivalent to `:nth-child(1)`.
    FirstChild,
    /// `:last-child` — matches the last child element.
    LastChild,
}

/// CSS pseudo-element (e.g. `::before`, `::after`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
}

/// An `An+B` formula for `:nth-child` / `:nth-of-type` selectors.
///
/// Matches element at 1-based position `pos` if there exists a non-negative
/// integer `n` such that `a * n + b == pos`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NthFormula {
    pub a: i32,
    pub b: i32,
}

impl NthFormula {
    /// Check whether a 1-based position matches this formula.
    pub fn matches(&self, pos: i32) -> bool {
        if self.a == 0 {
            return pos == self.b;
        }
        let diff = pos - self.b;
        // diff must be divisible by a, and the quotient must be non-negative.
        if diff % self.a != 0 {
            return false;
        }
        diff / self.a >= 0
    }

    /// Parse an An+B expression string.
    ///
    /// Supported forms: `odd`, `even`, `<number>`, `n`, `<a>n`, `<a>n+<b>`,
    /// `<a>n-<b>`, `n+<b>`, `n-<b>`, `-n+<b>`.
    pub fn parse(input: &str) -> Option<Self> {
        let s = input.trim().to_ascii_lowercase();
        if s == "odd" {
            return Some(NthFormula { a: 2, b: 1 });
        }
        if s == "even" {
            return Some(NthFormula { a: 2, b: 0 });
        }
        // Try plain integer first.
        if let Ok(num) = s.parse::<i32>() {
            return Some(NthFormula { a: 0, b: num });
        }
        // Must contain 'n'.
        let n_pos = s.find('n')?;
        let a_part = &s[..n_pos];
        let a = match a_part {
            "" | "+" => 1,
            "-" => -1,
            _ => a_part.parse::<i32>().ok()?,
        };
        let rest = s[n_pos + 1..].trim();
        let b = if rest.is_empty() {
            0
        } else {
            // rest should be like "+3" or "-2"
            rest.replace(' ', "").parse::<i32>().ok()?
        };
        Some(NthFormula { a, b })
    }
}

/// Sibling context for pseudo-class matching.
///
/// Provides the sibling elements and the current element's index among them,
/// needed for `:nth-child`, `:nth-of-type`, `:first-child`, `:last-child`.
#[derive(Debug, Clone, Copy)]
pub struct SiblingContext<'a> {
    /// All children of the parent element.
    pub siblings: &'a [DomNode],
    /// Index of the current element in `siblings`.
    pub index: usize,
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
    ///
    /// `ancestors` is the list of ancestor elements from root to parent (not
    /// including the element itself). `siblings` optionally provides the sibling
    /// context for pseudo-class matching (`:nth-child`, `:nth-of-type`, etc.).
    pub fn matches(
        &self,
        node: &DomNode,
        ancestors: &[&DomNode],
        siblings: Option<SiblingContext<'_>>,
    ) -> bool {
        self.selectors
            .iter()
            .any(|s| s.matches(node, ancestors, siblings))
    }

    /// Check if this selector list matches a specific pseudo-element on the node.
    ///
    /// Returns `true` if any selector targets `node` with the given `pe`
    /// (e.g. `p::before`). The pseudo-element is detected by examining the
    /// raw selector text for `::before` or `::after` suffixes.
    pub fn matches_with_pseudo_element(
        &self,
        node: &DomNode,
        ancestors: &[&DomNode],
        pe: PseudoElement,
    ) -> bool {
        // Check if any selector targets the given pseudo-element.
        for sel in &self.selectors {
            if sel.has_pseudo_element(pe) {
                // Check if the base selector (without pseudo-element) matches.
                let base = sel.without_pseudo_element();
                if base.matches_node(node, ancestors, None) {
                    return true;
                }
            }
        }
        false
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

        // Detect and strip pseudo-element suffixes (::before, ::after).
        let mut pseudo_element = None;
        let selector_str = if let Some(base) = input.strip_suffix("::before") {
            pseudo_element = Some(PseudoElement::Before);
            base
        } else if let Some(base) = input.strip_suffix("::after") {
            pseudo_element = Some(PseudoElement::After);
            base
        } else {
            input
        };

        // Split by whitespace for descendant combinator.
        let tokens: Vec<&str> = selector_str.split_whitespace().collect();
        let mut parts = Vec::new();

        for token in tokens {
            if let Some(compound) = CompoundSelector::parse(token) {
                parts.push(compound);
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(Selector { parts, pseudo_element })
        }
    }

    /// Check if this selector targets a specific pseudo-element.
    fn has_pseudo_element(&self, pe: PseudoElement) -> bool {
        self.pseudo_element == Some(pe)
    }

    /// Return a copy of this selector without the pseudo-element.
    fn without_pseudo_element(&self) -> Selector {
        Selector {
            parts: self.parts.clone(),
            pseudo_element: None,
        }
    }

    /// Check if this selector matches a node (public version for use by cascade).
    fn matches_node(
        &self,
        node: &DomNode,
        ancestors: &[&DomNode],
        siblings: Option<SiblingContext<'_>>,
    ) -> bool {
        self.matches(node, ancestors, siblings)
    }

    /// Check if this selector matches the given element.
    fn matches(
        &self,
        node: &DomNode,
        ancestors: &[&DomNode],
        siblings: Option<SiblingContext<'_>>,
    ) -> bool {
        if self.parts.is_empty() {
            return false;
        }

        // The rightmost compound must match the node itself (including pseudo-classes).
        let last = &self.parts[self.parts.len() - 1];
        if !last.matches(node, siblings) {
            return false;
        }

        // If there's only one part, we're done.
        if self.parts.len() == 1 {
            return true;
        }

        // Walk the remaining parts right-to-left against ancestors (descendant combinator).
        // Each part must match some ancestor, and we go from inner to outer.
        // Ancestor parts don't get sibling context (we don't have that info).
        let mut part_idx = self.parts.len() - 2; // Start from second-to-last
        let mut ancestor_idx = ancestors.len(); // Start from direct parent

        loop {
            if ancestor_idx == 0 {
                // No more ancestors to check; if we haven't matched all parts, fail.
                return false;
            }
            ancestor_idx -= 1;

            if self.parts[part_idx].matches(ancestors[ancestor_idx], None) {
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
            // Pseudo-classes count as class-level specificity.
            b += part.pseudos.len() as u32;
            if part.tag.is_some() {
                c += 1;
            }
            // Universal selector contributes nothing.
        }
        Specificity(a, b, c)
    }
}

impl CompoundSelector {
    /// Parse a compound selector like `div.foo#bar`, `*`, `.class`,
    /// `li:nth-child(2n+1)`, or `td:first-child`.
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
                ':' => {
                    // Flush any pending simple selector part first.
                    flush(&mut sel, current_type, &current);
                    current.clear();
                    current_type = None;
                    chars.next(); // consume ':'

                    // Read the pseudo-class name.
                    let mut pseudo_name = String::new();
                    while let Some(&pc) = chars.peek() {
                        if pc == '(' || pc == '#' || pc == '.' || pc == ':' {
                            break;
                        }
                        pseudo_name.push(pc);
                        chars.next();
                    }

                    let pseudo_lower = pseudo_name.to_ascii_lowercase();
                    match pseudo_lower.as_str() {
                        "nth-child" | "nth-of-type" => {
                            // Expect '(' ... ')'.
                            if chars.peek() == Some(&'(') {
                                chars.next(); // consume '('
                                let mut arg = String::new();
                                let mut depth = 1u32;
                                while let Some(&c) = chars.peek() {
                                    chars.next();
                                    if c == '(' {
                                        depth += 1;
                                        arg.push(c);
                                    } else if c == ')' {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                        arg.push(c);
                                    } else {
                                        arg.push(c);
                                    }
                                }
                                if let Some(formula) = NthFormula::parse(&arg) {
                                    let pseudo = if pseudo_lower == "nth-child" {
                                        PseudoClass::NthChild(formula)
                                    } else {
                                        PseudoClass::NthOfType(formula)
                                    };
                                    sel.pseudos.push(pseudo);
                                }
                            }
                        }
                        "first-child" => {
                            sel.pseudos.push(PseudoClass::FirstChild);
                        }
                        "last-child" => {
                            sel.pseudos.push(PseudoClass::LastChild);
                        }
                        _ => {
                            // Unknown pseudo-class — silently ignore (e.g. :hover, :link).
                        }
                    }
                }
                _ => {
                    current.push(ch);
                    chars.next();
                }
            }
        }
        flush(&mut sel, current_type, &current);

        // If nothing was set (no tag, id, classes, or pseudos), treat as invalid.
        if sel.tag.is_none() && sel.id.is_none() && sel.classes.is_empty() && sel.pseudos.is_empty()
        {
            return None;
        }

        Some(sel)
    }

    /// Check if this compound selector matches a DOM node.
    ///
    /// `siblings` provides sibling context for pseudo-class matching. If `None`,
    /// any positional pseudo-classes (`:nth-child`, `:first-child`, etc.) will
    /// not match.
    fn matches(&self, node: &DomNode, siblings: Option<SiblingContext<'_>>) -> bool {
        if self.universal {
            let is_element = matches!(node, DomNode::Element { .. });
            if !is_element {
                return false;
            }
            // Universal still needs to check pseudo-classes.
            return self.pseudos.is_empty()
                || self.check_pseudos(node, siblings);
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

                // Check pseudo-classes.
                if !self.pseudos.is_empty() && !self.check_pseudos(node, siblings) {
                    return false;
                }

                true
            }
            _ => false,
        }
    }

    /// Check all pseudo-classes on this selector against the node and sibling context.
    fn check_pseudos(&self, node: &DomNode, siblings: Option<SiblingContext<'_>>) -> bool {
        let ctx = match siblings {
            Some(ctx) => ctx,
            None => return false, // No sibling context — positional pseudos can't match.
        };

        for pseudo in &self.pseudos {
            match pseudo {
                PseudoClass::NthChild(formula) => {
                    let pos = element_position_among_all(ctx.siblings, ctx.index);
                    if !formula.matches(pos) {
                        return false;
                    }
                }
                PseudoClass::NthOfType(formula) => {
                    let tag = node.tag().unwrap_or("");
                    let pos = element_position_among_type(ctx.siblings, ctx.index, tag);
                    if !formula.matches(pos) {
                        return false;
                    }
                }
                PseudoClass::FirstChild => {
                    let pos = element_position_among_all(ctx.siblings, ctx.index);
                    if pos != 1 {
                        return false;
                    }
                }
                PseudoClass::LastChild => {
                    if !is_last_element(ctx.siblings, ctx.index) {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// Compute the 1-based position of the element at `index` among all sibling elements.
///
/// Non-element nodes (text, comments) are skipped.
fn element_position_among_all(siblings: &[DomNode], index: usize) -> i32 {
    let mut pos = 0i32;
    for (i, sib) in siblings.iter().enumerate() {
        if matches!(sib, DomNode::Element { .. }) {
            pos += 1;
        }
        if i == index {
            return pos;
        }
    }
    pos
}

/// Compute the 1-based position of the element at `index` among siblings with the same tag.
fn element_position_among_type(siblings: &[DomNode], index: usize, tag: &str) -> i32 {
    let tag_lower = tag.to_ascii_lowercase();
    let mut pos = 0i32;
    for (i, sib) in siblings.iter().enumerate() {
        if let DomNode::Element { tag: t, .. } = sib {
            if t.to_ascii_lowercase() == tag_lower {
                pos += 1;
            }
        }
        if i == index {
            return pos;
        }
    }
    pos
}

/// Check if the element at `index` is the last element among its siblings.
fn is_last_element(siblings: &[DomNode], index: usize) -> bool {
    for sib in siblings.iter().skip(index + 1) {
        if matches!(sib, DomNode::Element { .. }) {
            return false;
        }
    }
    true
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
        assert!(sel.matches(&node, &[], None));

        let node2 = elem("span", &[]);
        assert!(!sel.matches(&node2, &[], None));
    }

    #[test]
    fn class_selector() {
        let sel = SelectorList::parse(".foo").unwrap();
        let node = elem("div", &[("class", "foo bar")]);
        assert!(sel.matches(&node, &[], None));

        let node2 = elem("div", &[("class", "baz")]);
        assert!(!sel.matches(&node2, &[], None));
    }

    #[test]
    fn id_selector() {
        let sel = SelectorList::parse("#main").unwrap();
        let node = elem("div", &[("id", "main")]);
        assert!(sel.matches(&node, &[], None));
    }

    #[test]
    fn compound_selector() {
        let sel = SelectorList::parse("div.foo#bar").unwrap();
        let node = elem("div", &[("class", "foo"), ("id", "bar")]);
        assert!(sel.matches(&node, &[], None));

        let node2 = elem("span", &[("class", "foo"), ("id", "bar")]);
        assert!(!sel.matches(&node2, &[], None));
    }

    #[test]
    fn descendant_selector() {
        let sel = SelectorList::parse("div p").unwrap();
        let div = elem("div", &[]);
        let p = elem("p", &[]);
        assert!(sel.matches(&p, &[&div], None));
        assert!(!sel.matches(&p, &[], None));
    }

    #[test]
    fn selector_list() {
        let sel = SelectorList::parse("h1, h2, h3").unwrap();
        assert_eq!(sel.selectors.len(), 3);
        assert!(sel.matches(&elem("h2", &[]), &[], None));
        assert!(!sel.matches(&elem("h4", &[]), &[], None));
    }

    #[test]
    fn universal_selector() {
        let sel = SelectorList::parse("*").unwrap();
        assert!(sel.matches(&elem("anything", &[]), &[], None));
    }

    #[test]
    fn specificity_ordering() {
        let id_sel = SelectorList::parse("#foo").unwrap();
        let class_sel = SelectorList::parse(".bar").unwrap();
        let tag_sel = SelectorList::parse("div").unwrap();
        assert!(id_sel.specificity() > class_sel.specificity());
        assert!(class_sel.specificity() > tag_sel.specificity());
    }

    // ── NthFormula parsing tests ──

    #[test]
    fn nth_formula_parse_odd() {
        let f = NthFormula::parse("odd").unwrap();
        assert_eq!(f, NthFormula { a: 2, b: 1 });
    }

    #[test]
    fn nth_formula_parse_even() {
        let f = NthFormula::parse("even").unwrap();
        assert_eq!(f, NthFormula { a: 2, b: 0 });
    }

    #[test]
    fn nth_formula_parse_number() {
        let f = NthFormula::parse("3").unwrap();
        assert_eq!(f, NthFormula { a: 0, b: 3 });
    }

    #[test]
    fn nth_formula_parse_an_plus_b() {
        let f = NthFormula::parse("2n+1").unwrap();
        assert_eq!(f, NthFormula { a: 2, b: 1 });
    }

    #[test]
    fn nth_formula_parse_an_minus_b() {
        let f = NthFormula::parse("3n-1").unwrap();
        assert_eq!(f, NthFormula { a: 3, b: -1 });
    }

    #[test]
    fn nth_formula_parse_n_only() {
        let f = NthFormula::parse("n").unwrap();
        assert_eq!(f, NthFormula { a: 1, b: 0 });
    }

    #[test]
    fn nth_formula_parse_n_plus_b() {
        let f = NthFormula::parse("n+2").unwrap();
        assert_eq!(f, NthFormula { a: 1, b: 2 });
    }

    #[test]
    fn nth_formula_parse_negative_n() {
        let f = NthFormula::parse("-n+3").unwrap();
        assert_eq!(f, NthFormula { a: -1, b: 3 });
    }

    // ── NthFormula matching tests ──

    #[test]
    fn nth_formula_matches_odd() {
        let f = NthFormula { a: 2, b: 1 }; // odd
        assert!(f.matches(1));
        assert!(!f.matches(2));
        assert!(f.matches(3));
        assert!(!f.matches(4));
        assert!(f.matches(5));
    }

    #[test]
    fn nth_formula_matches_even() {
        let f = NthFormula { a: 2, b: 0 }; // even
        assert!(!f.matches(1));
        assert!(f.matches(2));
        assert!(!f.matches(3));
        assert!(f.matches(4));
    }

    #[test]
    fn nth_formula_matches_exact() {
        let f = NthFormula { a: 0, b: 3 }; // exactly 3rd
        assert!(!f.matches(1));
        assert!(!f.matches(2));
        assert!(f.matches(3));
        assert!(!f.matches(4));
    }

    #[test]
    fn nth_formula_matches_3n_minus_1() {
        let f = NthFormula { a: 3, b: -1 }; // 3n-1: positions 2, 5, 8, ...
        assert!(!f.matches(1));
        assert!(f.matches(2));
        assert!(!f.matches(3));
        assert!(!f.matches(4));
        assert!(f.matches(5));
        assert!(f.matches(8));
    }

    // ── Selector parsing tests for pseudo-classes ──

    #[test]
    fn parse_nth_child_selector() {
        let sel = SelectorList::parse("li:nth-child(2n+1)").unwrap();
        assert_eq!(sel.selectors.len(), 1);
        let compound = &sel.selectors[0].parts[0];
        assert_eq!(compound.tag, Some("li".into()));
        assert_eq!(compound.pseudos.len(), 1);
        match &compound.pseudos[0] {
            PseudoClass::NthChild(f) => assert_eq!(*f, NthFormula { a: 2, b: 1 }),
            _ => panic!("Expected NthChild"),
        }
    }

    #[test]
    fn parse_nth_of_type_selector() {
        let sel = SelectorList::parse("p:nth-of-type(even)").unwrap();
        let compound = &sel.selectors[0].parts[0];
        assert_eq!(compound.tag, Some("p".into()));
        match &compound.pseudos[0] {
            PseudoClass::NthOfType(f) => assert_eq!(*f, NthFormula { a: 2, b: 0 }),
            _ => panic!("Expected NthOfType"),
        }
    }

    #[test]
    fn parse_first_child_selector() {
        let sel = SelectorList::parse("li:first-child").unwrap();
        let compound = &sel.selectors[0].parts[0];
        assert_eq!(compound.pseudos.len(), 1);
        assert!(matches!(compound.pseudos[0], PseudoClass::FirstChild));
    }

    #[test]
    fn parse_last_child_selector() {
        let sel = SelectorList::parse("li:last-child").unwrap();
        let compound = &sel.selectors[0].parts[0];
        assert!(matches!(compound.pseudos[0], PseudoClass::LastChild));
    }

    // ── Selector matching tests with sibling context ──

    /// Helper to build a list of sibling elements for testing.
    fn make_siblings(tags: &[&str]) -> Vec<DomNode> {
        tags.iter()
            .map(|t| DomNode::Element {
                tag: t.to_string(),
                attributes: vec![],
                children: vec![],
            })
            .collect()
    }

    #[test]
    fn nth_child_matches_odd_positions() {
        let sel = SelectorList::parse("li:nth-child(odd)").unwrap();
        let siblings = make_siblings(&["li", "li", "li", "li", "li"]);

        // Positions: li(1), li(2), li(3), li(4), li(5)
        // odd = 1, 3, 5
        for (i, expected) in [(0, true), (1, false), (2, true), (3, false), (4, true)] {
            let ctx = SiblingContext {
                siblings: &siblings,
                index: i,
            };
            assert_eq!(
                sel.matches(&siblings[i], &[], Some(ctx)),
                expected,
                "index {} should be {}",
                i,
                expected
            );
        }
    }

    #[test]
    fn nth_child_matches_even_positions() {
        let sel = SelectorList::parse("li:nth-child(even)").unwrap();
        let siblings = make_siblings(&["li", "li", "li", "li"]);

        for (i, expected) in [(0, false), (1, true), (2, false), (3, true)] {
            let ctx = SiblingContext {
                siblings: &siblings,
                index: i,
            };
            assert_eq!(
                sel.matches(&siblings[i], &[], Some(ctx)),
                expected,
                "index {} should be {}",
                i,
                expected
            );
        }
    }

    #[test]
    fn nth_child_exact_number() {
        let sel = SelectorList::parse("li:nth-child(3)").unwrap();
        let siblings = make_siblings(&["li", "li", "li", "li"]);

        for (i, expected) in [(0, false), (1, false), (2, true), (3, false)] {
            let ctx = SiblingContext {
                siblings: &siblings,
                index: i,
            };
            assert_eq!(
                sel.matches(&siblings[i], &[], Some(ctx)),
                expected,
                "index {} should be {}",
                i,
                expected
            );
        }
    }

    #[test]
    fn nth_child_skips_text_nodes() {
        // Siblings: text, li, text, li, li
        // Element positions: li=1, li=2, li=3
        let siblings = vec![
            DomNode::Text("hello".into()),
            elem("li", &[]),
            DomNode::Text("world".into()),
            elem("li", &[]),
            elem("li", &[]),
        ];

        let sel = SelectorList::parse("li:nth-child(2)").unwrap();

        // Index 1 is the first li → element position 1, not a match.
        let ctx1 = SiblingContext {
            siblings: &siblings,
            index: 1,
        };
        assert!(!sel.matches(&siblings[1], &[], Some(ctx1)));

        // Index 3 is the second li → element position 2, match!
        let ctx3 = SiblingContext {
            siblings: &siblings,
            index: 3,
        };
        assert!(sel.matches(&siblings[3], &[], Some(ctx3)));
    }

    #[test]
    fn nth_of_type_counts_same_tag_only() {
        // Siblings: div, p, div, p, div
        // div positions: 1, 2, 3
        // p positions: 1, 2
        let siblings = make_siblings(&["div", "p", "div", "p", "div"]);

        let sel_div = SelectorList::parse("div:nth-of-type(2)").unwrap();
        let sel_p = SelectorList::parse("p:nth-of-type(1)").unwrap();

        // Index 0 = div(1) — not 2nd div
        let ctx0 = SiblingContext { siblings: &siblings, index: 0 };
        assert!(!sel_div.matches(&siblings[0], &[], Some(ctx0)));

        // Index 2 = div(2) — match!
        let ctx2 = SiblingContext { siblings: &siblings, index: 2 };
        assert!(sel_div.matches(&siblings[2], &[], Some(ctx2)));

        // Index 1 = p(1) — match!
        let ctx1 = SiblingContext { siblings: &siblings, index: 1 };
        assert!(sel_p.matches(&siblings[1], &[], Some(ctx1)));

        // Index 3 = p(2) — not 1st p
        let ctx3 = SiblingContext { siblings: &siblings, index: 3 };
        assert!(!sel_p.matches(&siblings[3], &[], Some(ctx3)));
    }

    #[test]
    fn first_child_matches() {
        let sel = SelectorList::parse("li:first-child").unwrap();
        let siblings = make_siblings(&["li", "li", "li"]);

        let ctx0 = SiblingContext { siblings: &siblings, index: 0 };
        assert!(sel.matches(&siblings[0], &[], Some(ctx0)));

        let ctx1 = SiblingContext { siblings: &siblings, index: 1 };
        assert!(!sel.matches(&siblings[1], &[], Some(ctx1)));
    }

    #[test]
    fn last_child_matches() {
        let sel = SelectorList::parse("li:last-child").unwrap();
        let siblings = make_siblings(&["li", "li", "li"]);

        let ctx0 = SiblingContext { siblings: &siblings, index: 0 };
        assert!(!sel.matches(&siblings[0], &[], Some(ctx0)));

        let ctx2 = SiblingContext { siblings: &siblings, index: 2 };
        assert!(sel.matches(&siblings[2], &[], Some(ctx2)));
    }

    #[test]
    fn last_child_ignores_trailing_text() {
        let siblings = vec![
            elem("li", &[]),
            elem("li", &[]),
            DomNode::Text("trailing".into()),
        ];
        let sel = SelectorList::parse("li:last-child").unwrap();

        // Index 1 is the last *element* even though there's a text node after it.
        let ctx1 = SiblingContext { siblings: &siblings, index: 1 };
        assert!(sel.matches(&siblings[1], &[], Some(ctx1)));
    }

    #[test]
    fn nth_child_with_descendant_combinator() {
        // ul li:nth-child(2) — match 2nd child li inside a ul
        let sel = SelectorList::parse("ul li:nth-child(2)").unwrap();
        let ul = elem("ul", &[]);
        let siblings = make_siblings(&["li", "li", "li"]);

        let ctx0 = SiblingContext { siblings: &siblings, index: 0 };
        assert!(!sel.matches(&siblings[0], &[&ul], Some(ctx0)));

        let ctx1 = SiblingContext { siblings: &siblings, index: 1 };
        assert!(sel.matches(&siblings[1], &[&ul], Some(ctx1)));
    }

    #[test]
    fn nth_child_no_sibling_context_does_not_match() {
        // Without sibling context, nth-child cannot match.
        let sel = SelectorList::parse("li:nth-child(1)").unwrap();
        let node = elem("li", &[]);
        assert!(!sel.matches(&node, &[], None));
    }

    #[test]
    fn pseudo_class_specificity() {
        // :nth-child counts as class-level specificity (0, 1, 0).
        let sel = SelectorList::parse("li:nth-child(odd)").unwrap();
        // tag(li) = (0,0,1), pseudo(:nth-child) = (0,1,0) => total (0,1,1)
        assert_eq!(sel.specificity(), Specificity(0, 1, 1));
    }

    #[test]
    fn nth_child_2n_plus_0() {
        // 2n+0 is the same as even
        let sel = SelectorList::parse("li:nth-child(2n+0)").unwrap();
        let siblings = make_siblings(&["li", "li", "li", "li"]);

        let ctx0 = SiblingContext { siblings: &siblings, index: 0 };
        assert!(!sel.matches(&siblings[0], &[], Some(ctx0)));

        let ctx1 = SiblingContext { siblings: &siblings, index: 1 };
        assert!(sel.matches(&siblings[1], &[], Some(ctx1)));
    }

    #[test]
    fn unknown_pseudo_class_ignored() {
        // :hover is unknown, should be silently ignored and still match the tag.
        let sel = SelectorList::parse("a:hover").unwrap();
        let node = elem("a", &[]);
        assert!(sel.matches(&node, &[], None));
    }
}

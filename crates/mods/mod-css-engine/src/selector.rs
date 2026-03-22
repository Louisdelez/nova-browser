//! CSS selector parsing and matching.
//!
//! Supports: tag, class, id, universal, descendant/child/adjacent-sibling/
//! general-sibling combinators, attribute selectors, selector lists
//! (comma-separated), and `:nth-child(An+B)` / `:nth-of-type(An+B)`
//! pseudo-classes. Computes specificity for cascade ordering.

use nova_mod_api::content::DomNode;

/// A parsed CSS selector (possibly comma-separated into multiple alternatives).
#[derive(Debug, Clone)]
pub struct SelectorList {
    /// Individual selectors separated by commas.
    pub selectors: Vec<Selector>,
}

/// Combinator between two compound selectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    /// Whitespace — matches any descendant.
    Descendant,
    /// `>` — matches only a direct child.
    Child,
    /// `+` — matches the immediately following sibling element.
    Adjacent,
    /// `~` — matches any following sibling element.
    General,
}

/// A single CSS selector (a sequence of simple selectors with combinators).
///
/// Stored as a list of compound selectors with the relationship between each
/// pair tracked in `combinators`.
#[derive(Debug, Clone)]
pub struct Selector {
    /// Compound selectors from outermost (leftmost) to innermost (rightmost).
    /// e.g., `div .foo p` => [CompoundSelector(div), CompoundSelector(.foo), CompoundSelector(p)]
    pub parts: Vec<CompoundSelector>,
    /// Combinators between consecutive parts. `combinators[i]` is the
    /// relationship between `parts[i]` and `parts[i+1]`.
    /// Length is always `parts.len() - 1` (or 0 when parts is empty).
    pub combinators: Vec<Combinator>,
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
    /// Attribute selectors (e.g. `[href]`, `[type="text"]`).
    pub attributes: Vec<AttributeSelector>,
}

/// An attribute selector such as `[href]` or `[type="text"]`.
#[derive(Debug, Clone)]
pub struct AttributeSelector {
    /// Attribute name.
    pub name: String,
    /// Matching operation.
    pub op: AttributeOp,
}

/// The kind of attribute match.
#[derive(Debug, Clone)]
pub enum AttributeOp {
    /// `[attr]` — attribute must exist.
    Exists,
    /// `[attr=val]` — attribute value must equal.
    Equals(String),
    /// `[attr^=val]` — attribute value must start with.
    StartsWith(String),
    /// `[attr$=val]` — attribute value must end with.
    EndsWith(String),
    /// `[attr*=val]` — attribute value must contain.
    Contains(String),
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
    /// `:not(selector-list)` — matches if none of the selectors match.
    Not(Box<SelectorList>),
    /// `:is(selector-list)` — matches if any of the selectors match.
    Is(Box<SelectorList>),
    /// `:where(selector-list)` — matches like `:is()` but with zero specificity.
    Where(Box<SelectorList>),
    /// `:hover` — matches when the element is being hovered by the pointer.
    Hover,
    /// `:focus` — matches when the element has keyboard focus.
    Focus,
    /// `:active` — matches when the element is being activated (e.g. mouse down).
    Active,
    /// `:host` — matches the shadow host element from within a shadow tree.
    Host,
}

/// CSS pseudo-element (e.g. `::before`, `::after`, `::first-line`, `::first-letter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
    /// `::first-line` — applies styles to the first line of a block element.
    /// Currently parsed but not applied during layout/paint (no-op).
    FirstLine,
    /// `::first-letter` — applies styles to the first letter (drop caps).
    /// Currently parsed but not applied during layout/paint (no-op).
    FirstLetter,
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
    ///
    /// Splits on commas that are not inside parentheses, so that
    /// `:is(h1, h2)` is not incorrectly split.
    pub fn parse(input: &str) -> Option<Self> {
        let parts = split_on_top_level_commas(input);
        let mut selectors = Vec::new();
        for part in &parts {
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

        // Detect and strip pseudo-element suffixes (::before, ::after,
        // ::first-line, ::first-letter). Also handle single-colon legacy syntax.
        let mut pseudo_element = None;
        let selector_str = if let Some(base) = input.strip_suffix("::before") {
            pseudo_element = Some(PseudoElement::Before);
            base
        } else if let Some(base) = input.strip_suffix("::after") {
            pseudo_element = Some(PseudoElement::After);
            base
        } else if let Some(base) = input.strip_suffix("::first-line") {
            pseudo_element = Some(PseudoElement::FirstLine);
            base
        } else if let Some(base) = input.strip_suffix("::first-letter") {
            pseudo_element = Some(PseudoElement::FirstLetter);
            base
        } else if let Some(base) = input.strip_suffix(":first-line") {
            pseudo_element = Some(PseudoElement::FirstLine);
            base
        } else if let Some(base) = input.strip_suffix(":first-letter") {
            pseudo_element = Some(PseudoElement::FirstLetter);
            base
        } else if let Some(base) = input.strip_suffix(":before") {
            pseudo_element = Some(PseudoElement::Before);
            base
        } else if let Some(base) = input.strip_suffix(":after") {
            pseudo_element = Some(PseudoElement::After);
            base
        } else {
            input
        };

        // Tokenize into compound-selector strings and combinator characters.
        // We split by whitespace (respecting parentheses), then detect `>`,
        // `+`, `~` as combinator tokens.  We also handle cases where
        // combinators are glued to selectors (e.g. `div>p`).
        let tokens = split_whitespace_respecting_parens(selector_str);
        let mut parts: Vec<CompoundSelector> = Vec::new();
        let mut combinators: Vec<Combinator> = Vec::new();
        // pending_combinator: the combinator to use before the next compound selector.
        let mut pending_combinator: Option<Combinator> = None;

        for token in &tokens {
            match token.as_str() {
                ">" => {
                    pending_combinator = Some(Combinator::Child);
                    continue;
                }
                "+" => {
                    pending_combinator = Some(Combinator::Adjacent);
                    continue;
                }
                "~" => {
                    pending_combinator = Some(Combinator::General);
                    continue;
                }
                _ => {}
            }

            // The token may contain glued combinators, e.g. `div>p`, `a+b`,
            // `h1~h2`, or even `div>p+span`.  We split on `>`, `+`, `~`
            // while preserving the combinator character.
            let sub_parts = split_token_on_combinators(token);
            for (fragment, comb_after) in &sub_parts {
                if fragment.is_empty() {
                    // This can happen for leading combinator, e.g. `>p` at start.
                    // Just record the combinator.
                    if let Some(c) = comb_after {
                        pending_combinator = Some(*c);
                    }
                    continue;
                }
                if let Some(compound) = CompoundSelector::parse(fragment) {
                    if !parts.is_empty() {
                        // Record combinator between previous part and this one.
                        combinators.push(pending_combinator.unwrap_or(Combinator::Descendant));
                        pending_combinator = None;
                    }
                    parts.push(compound);
                }
                // If there's a combinator after this fragment, store it for next.
                if let Some(c) = comb_after {
                    pending_combinator = Some(*c);
                }
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(Selector { parts, combinators, pseudo_element })
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
            combinators: self.combinators.clone(),
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
    ///
    /// `ancestors` is the chain from root to parent (not including the node).
    /// `siblings` provides sibling context for the *target* node (rightmost
    /// compound) so that sibling combinators and pseudo-classes can be evaluated.
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

        // Walk the remaining parts right-to-left.
        // `current_node` is the node that just matched `parts[part_idx + 1]`.
        // `current_siblings` is the sibling context for `current_node`.
        // `current_ancestors` is the ancestor slice for `current_node`.
        self.match_remaining(
            self.parts.len() - 2,
            ancestors,
            siblings,
        )
    }

    /// Recursively match `parts[0..=part_idx]` against the context.
    ///
    /// `ancestors` is the ancestor chain for the node that matched `parts[part_idx + 1]`.
    /// `siblings` is the sibling context for that same node.
    fn match_remaining(
        &self,
        part_idx: usize,
        ancestors: &[&DomNode],
        siblings: Option<SiblingContext<'_>>,
    ) -> bool {
        let compound = &self.parts[part_idx];
        let combinator = self.combinators[part_idx]; // combinator between part_idx and part_idx+1

        match combinator {
            Combinator::Descendant => {
                // Check any ancestor (from innermost outward).
                let mut anc_idx = ancestors.len();
                while anc_idx > 0 {
                    anc_idx -= 1;
                    if compound.matches(ancestors[anc_idx], None) {
                        if part_idx == 0 {
                            return true;
                        }
                        // Continue matching further parts against the remaining ancestors.
                        if self.match_remaining(
                            part_idx - 1,
                            &ancestors[..anc_idx],
                            None, // we don't have sibling context for ancestors
                        ) {
                            return true;
                        }
                        // If that didn't work, keep trying further ancestors.
                    }
                }
                false
            }
            Combinator::Child => {
                // Must match the immediate parent (last ancestor).
                if ancestors.is_empty() {
                    return false;
                }
                let parent = ancestors[ancestors.len() - 1];
                if !compound.matches(parent, None) {
                    return false;
                }
                if part_idx == 0 {
                    return true;
                }
                self.match_remaining(
                    part_idx - 1,
                    &ancestors[..ancestors.len() - 1],
                    None,
                )
            }
            Combinator::Adjacent => {
                // Must match the immediately preceding sibling element.
                let ctx = match siblings {
                    Some(ctx) => ctx,
                    None => return false,
                };
                // Find the immediately preceding element sibling.
                if let Some((prev_idx, prev_node)) = preceding_element_sibling(ctx.siblings, ctx.index) {
                    if compound.matches(prev_node, None) {
                        if part_idx == 0 {
                            return true;
                        }
                        // The preceding sibling shares the same parent/ancestors.
                        let prev_sib_ctx = SiblingContext {
                            siblings: ctx.siblings,
                            index: prev_idx,
                        };
                        return self.match_remaining(
                            part_idx - 1,
                            ancestors,
                            Some(prev_sib_ctx),
                        );
                    }
                }
                false
            }
            Combinator::General => {
                // Must match any preceding sibling element.
                let ctx = match siblings {
                    Some(ctx) => ctx,
                    None => return false,
                };
                // Iterate all preceding element siblings.
                for i in (0..ctx.index).rev() {
                    if matches!(ctx.siblings[i], DomNode::Element { .. }) {
                        if compound.matches(&ctx.siblings[i], None) {
                            if part_idx == 0 {
                                return true;
                            }
                            let prev_sib_ctx = SiblingContext {
                                siblings: ctx.siblings,
                                index: i,
                            };
                            if self.match_remaining(
                                part_idx - 1,
                                ancestors,
                                Some(prev_sib_ctx),
                            ) {
                                return true;
                            }
                        }
                    }
                }
                false
            }
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
            // Pseudo-classes count as class-level specificity, except
            // :where() (zero) and :not()/:is() (take their argument's specificity).
            for pseudo in &part.pseudos {
                match pseudo {
                    PseudoClass::Where(_) => {
                        // :where() has zero specificity.
                    }
                    PseudoClass::Not(inner) | PseudoClass::Is(inner) => {
                        // :not() and :is() take the specificity of their most specific argument.
                        let inner_spec = inner.specificity();
                        a += inner_spec.0;
                        b += inner_spec.1;
                        c += inner_spec.2;
                    }
                    _ => {
                        b += 1;
                    }
                }
            }
            // Attribute selectors count as class-level specificity.
            b += part.attributes.len() as u32;
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
                '[' => {
                    // Flush any pending simple selector part first.
                    flush(&mut sel, current_type, &current);
                    current.clear();
                    current_type = None;
                    chars.next(); // consume '['

                    // Read everything until ']'.
                    let mut attr_content = String::new();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c == ']' {
                            break;
                        }
                        attr_content.push(c);
                    }

                    if let Some(attr_sel) = AttributeSelector::parse(&attr_content) {
                        sel.attributes.push(attr_sel);
                    }
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
                        "not" | "is" | "where" => {
                            // Expect '(' selector-list ')'.
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
                                if let Some(inner_list) = SelectorList::parse(&arg) {
                                    let pseudo = match pseudo_lower.as_str() {
                                        "not" => PseudoClass::Not(Box::new(inner_list)),
                                        "is" => PseudoClass::Is(Box::new(inner_list)),
                                        "where" => PseudoClass::Where(Box::new(inner_list)),
                                        _ => unreachable!(),
                                    };
                                    sel.pseudos.push(pseudo);
                                }
                            }
                        }
                        "hover" => {
                            sel.pseudos.push(PseudoClass::Hover);
                        }
                        "focus" => {
                            sel.pseudos.push(PseudoClass::Focus);
                        }
                        "active" => {
                            sel.pseudos.push(PseudoClass::Active);
                        }
                        "host" => {
                            sel.pseudos.push(PseudoClass::Host);
                        }
                        "link" | "visited" => {
                            // Treat :link and :visited as always matching for <a> elements.
                            // This is a simplification — we don't track visited links.
                        }
                        _ => {
                            // Unknown pseudo-class — silently ignore.
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

        // If nothing was set (no tag, id, classes, pseudos, or attributes), treat as invalid.
        if sel.tag.is_none()
            && sel.id.is_none()
            && sel.classes.is_empty()
            && sel.pseudos.is_empty()
            && sel.attributes.is_empty()
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

                // Check attribute selectors.
                for attr_sel in &self.attributes {
                    if !attr_sel.matches(attributes) {
                        return false;
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
        for pseudo in &self.pseudos {
            match pseudo {
                PseudoClass::NthChild(formula) => {
                    let ctx = match siblings {
                        Some(ctx) => ctx,
                        None => return false,
                    };
                    let pos = element_position_among_all(ctx.siblings, ctx.index);
                    if !formula.matches(pos) {
                        return false;
                    }
                }
                PseudoClass::NthOfType(formula) => {
                    let ctx = match siblings {
                        Some(ctx) => ctx,
                        None => return false,
                    };
                    let tag = node.tag().unwrap_or("");
                    let pos = element_position_among_type(ctx.siblings, ctx.index, tag);
                    if !formula.matches(pos) {
                        return false;
                    }
                }
                PseudoClass::FirstChild => {
                    let ctx = match siblings {
                        Some(ctx) => ctx,
                        None => return false,
                    };
                    let pos = element_position_among_all(ctx.siblings, ctx.index);
                    if pos != 1 {
                        return false;
                    }
                }
                PseudoClass::LastChild => {
                    let ctx = match siblings {
                        Some(ctx) => ctx,
                        None => return false,
                    };
                    if !is_last_element(ctx.siblings, ctx.index) {
                        return false;
                    }
                }
                PseudoClass::Hover | PseudoClass::Focus | PseudoClass::Active => {
                    // Interactive pseudo-classes require interaction state.
                    // Without state, they don't match (safe default).
                    return false;
                }
                PseudoClass::Host => {
                    // :host matches elements that have a `data-nova-shadow-host`
                    // attribute (i.e. they are shadow host elements).
                    if let DomNode::Element { attributes, .. } = node {
                        if !attributes.iter().any(|(k, _)| k == "data-nova-shadow-host") {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                PseudoClass::Not(inner_list) => {
                    // :not() matches if NONE of the inner selectors match.
                    if inner_list.selectors.iter().any(|sel| {
                        sel.parts.len() == 1 && sel.parts[0].matches(node, siblings)
                    }) {
                        return false;
                    }
                }
                PseudoClass::Is(inner_list) | PseudoClass::Where(inner_list) => {
                    // :is()/:where() matches if ANY of the inner selectors match.
                    if !inner_list.selectors.iter().any(|sel| {
                        sel.parts.len() == 1 && sel.parts[0].matches(node, siblings)
                    }) {
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

/// Find the immediately preceding element sibling (skipping text/comment nodes).
///
/// Returns `(index, &DomNode)` of the preceding element, or `None`.
fn preceding_element_sibling(siblings: &[DomNode], index: usize) -> Option<(usize, &DomNode)> {
    for i in (0..index).rev() {
        if matches!(siblings[i], DomNode::Element { .. }) {
            return Some((i, &siblings[i]));
        }
    }
    None
}

/// Split a single token on combinator characters (`>`, `+`, `~`) that are
/// glued to selectors (e.g. `div>p` -> `[("div", Some(Child)), ("p", None)]`).
///
/// Parenthesized and bracketed content is preserved as-is (e.g. `:nth-child(2n+1)`
/// won't split on the `+`).
///
/// Returns a vec of `(fragment, combinator_after)` pairs.
fn split_token_on_combinators(token: &str) -> Vec<(String, Option<Combinator>)> {
    let mut results: Vec<(String, Option<Combinator>)> = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0u32;
    let mut bracket_depth = 0u32;

    for ch in token.chars() {
        match ch {
            '(' => { paren_depth += 1; current.push(ch); }
            ')' => { paren_depth = paren_depth.saturating_sub(1); current.push(ch); }
            '[' => { bracket_depth += 1; current.push(ch); }
            ']' => { bracket_depth = bracket_depth.saturating_sub(1); current.push(ch); }
            '>' | '+' | '~' if paren_depth == 0 && bracket_depth == 0 => {
                let comb = match ch {
                    '>' => Combinator::Child,
                    '+' => Combinator::Adjacent,
                    '~' => Combinator::General,
                    _ => unreachable!(),
                };
                results.push((std::mem::take(&mut current), Some(comb)));
            }
            _ => {
                current.push(ch);
            }
        }
    }
    // Push the trailing fragment (no combinator after it).
    results.push((current, None));
    results
}

impl AttributeSelector {
    /// Parse the content inside `[...]`.
    ///
    /// Supports: `attr`, `attr=val`, `attr^=val`, `attr$=val`, `attr*=val`.
    /// Values may be unquoted, single-quoted, or double-quoted.
    fn parse(input: &str) -> Option<Self> {
        let s = input.trim();
        if s.is_empty() {
            return None;
        }

        // Try to find an operator.
        // Order matters: check two-char operators first.
        let (name, op) = if let Some(pos) = s.find("^=") {
            let val = unquote(s[pos + 2..].trim());
            (s[..pos].trim(), AttributeOp::StartsWith(val))
        } else if let Some(pos) = s.find("$=") {
            let val = unquote(s[pos + 2..].trim());
            (s[..pos].trim(), AttributeOp::EndsWith(val))
        } else if let Some(pos) = s.find("*=") {
            let val = unquote(s[pos + 2..].trim());
            (s[..pos].trim(), AttributeOp::Contains(val))
        } else if let Some(pos) = s.find('=') {
            let val = unquote(s[pos + 1..].trim());
            (s[..pos].trim(), AttributeOp::Equals(val))
        } else {
            (s, AttributeOp::Exists)
        };

        if name.is_empty() {
            return None;
        }

        Some(AttributeSelector {
            name: name.to_ascii_lowercase(),
            op,
        })
    }

    /// Check if this attribute selector matches the given attributes list.
    fn matches(&self, attributes: &[(String, String)]) -> bool {
        let value = attributes
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == self.name)
            .map(|(_, v)| v.as_str());

        match &self.op {
            AttributeOp::Exists => value.is_some(),
            AttributeOp::Equals(expected) => value == Some(expected.as_str()),
            AttributeOp::StartsWith(prefix) => {
                value.is_some_and(|v| v.starts_with(prefix.as_str()))
            }
            AttributeOp::EndsWith(suffix) => {
                value.is_some_and(|v| v.ends_with(suffix.as_str()))
            }
            AttributeOp::Contains(needle) => {
                value.is_some_and(|v| v.contains(needle.as_str()))
            }
        }
    }
}

/// Split a string on commas that are not inside parentheses.
///
/// This ensures that `:is(h1, h2), div` splits into [`:is(h1, h2)`, `div`]
/// rather than naively splitting on every comma.
/// Split a string on whitespace, but not inside parentheses or brackets.
///
/// This ensures that `:is(h1, h2)` is treated as a single token even though
/// it contains spaces after the commas.
fn split_whitespace_respecting_parens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut depth = 0u32;
    let mut bracket_depth = 0u32;
    for ch in input.chars() {
        match ch {
            '(' => { depth += 1; current.push(ch); }
            ')' => { depth = depth.saturating_sub(1); current.push(ch); }
            '[' => { bracket_depth += 1; current.push(ch); }
            ']' => { bracket_depth = bracket_depth.saturating_sub(1); current.push(ch); }
            c if c.is_whitespace() && depth == 0 && bracket_depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    tokens.push(trimmed);
                    current.clear();
                }
            }
            _ => { current.push(ch); }
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        tokens.push(trimmed);
    }
    tokens
}

fn split_on_top_level_commas(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0u32;
    for ch in input.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => {
                current.push(ch);
            }
        }
    }
    parts.push(current);
    parts
}

/// Strip surrounding quotes (single or double) from a value string.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
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
    fn hover_pseudo_class_parsed() {
        // :hover is now recognized and parsed. Without interaction state it won't match.
        let sel = SelectorList::parse("a:hover").unwrap();
        let node = elem("a", &[]);
        // Without hover state, :hover doesn't match.
        assert!(!sel.matches(&node, &[], None));
    }

    #[test]
    fn hover_pseudo_parsed_correctly() {
        let sel = SelectorList::parse("button:hover").unwrap();
        let compound = &sel.selectors[0].parts[0];
        assert_eq!(compound.tag, Some("button".into()));
        assert_eq!(compound.pseudos.len(), 1);
        assert!(matches!(compound.pseudos[0], PseudoClass::Hover));
    }

    // ── Child combinator tests ──

    #[test]
    fn child_combinator_direct_child() {
        // `div > p` should match p whose immediate parent is div.
        let sel = SelectorList::parse("div > p").unwrap();
        let div = elem("div", &[]);
        let p = elem("p", &[]);
        assert!(sel.matches(&p, &[&div], None));
    }

    #[test]
    fn child_combinator_not_grandchild() {
        // `div > p` should NOT match p whose parent is span, grandparent is div.
        let sel = SelectorList::parse("div > p").unwrap();
        let div = elem("div", &[]);
        let span = elem("span", &[]);
        let p = elem("p", &[]);
        assert!(!sel.matches(&p, &[&div, &span], None));
    }

    #[test]
    fn child_combinator_no_spaces() {
        // `div>p` (no spaces) should also work.
        let sel = SelectorList::parse("div>p").unwrap();
        let div = elem("div", &[]);
        let p = elem("p", &[]);
        assert!(sel.matches(&p, &[&div], None));
    }

    #[test]
    fn descendant_still_matches_grandchild() {
        // `div p` (descendant) should match p as a grandchild of div.
        let sel = SelectorList::parse("div p").unwrap();
        let div = elem("div", &[]);
        let span = elem("span", &[]);
        let p = elem("p", &[]);
        assert!(sel.matches(&p, &[&div, &span], None));
    }

    // ── Adjacent sibling combinator tests ──

    #[test]
    fn adjacent_sibling_matches_immediately_following() {
        // `div + p` matches p immediately after a div sibling.
        let sel = SelectorList::parse("div + p").unwrap();
        let siblings = make_siblings(&["div", "p", "span"]);
        let ctx = SiblingContext {
            siblings: &siblings,
            index: 1, // the p
        };
        assert!(sel.matches(&siblings[1], &[], Some(ctx)));
    }

    #[test]
    fn adjacent_sibling_not_non_adjacent() {
        // `div + p` should NOT match p if there's a span between div and p.
        let sel = SelectorList::parse("div + p").unwrap();
        let siblings = make_siblings(&["div", "span", "p"]);
        let ctx = SiblingContext {
            siblings: &siblings,
            index: 2, // the p
        };
        assert!(!sel.matches(&siblings[2], &[], Some(ctx)));
    }

    #[test]
    fn adjacent_sibling_no_spaces() {
        // `div+p` (no spaces) should also work.
        let sel = SelectorList::parse("div+p").unwrap();
        let siblings = make_siblings(&["div", "p"]);
        let ctx = SiblingContext {
            siblings: &siblings,
            index: 1,
        };
        assert!(sel.matches(&siblings[1], &[], Some(ctx)));
    }

    #[test]
    fn adjacent_sibling_first_element_no_match() {
        // `div + p` should not match if p is the first element.
        let sel = SelectorList::parse("div + p").unwrap();
        let siblings = make_siblings(&["p", "div"]);
        let ctx = SiblingContext {
            siblings: &siblings,
            index: 0,
        };
        assert!(!sel.matches(&siblings[0], &[], Some(ctx)));
    }

    // ── General sibling combinator tests ──

    #[test]
    fn general_sibling_matches_any_preceding() {
        // `div ~ p` matches p that has any preceding div sibling.
        let sel = SelectorList::parse("div ~ p").unwrap();
        let siblings = make_siblings(&["div", "span", "p"]);
        let ctx = SiblingContext {
            siblings: &siblings,
            index: 2, // the p
        };
        assert!(sel.matches(&siblings[2], &[], Some(ctx)));
    }

    #[test]
    fn general_sibling_matches_immediately_adjacent_too() {
        // `div ~ p` should also match if div is immediately before p.
        let sel = SelectorList::parse("div ~ p").unwrap();
        let siblings = make_siblings(&["div", "p"]);
        let ctx = SiblingContext {
            siblings: &siblings,
            index: 1,
        };
        assert!(sel.matches(&siblings[1], &[], Some(ctx)));
    }

    #[test]
    fn general_sibling_not_following() {
        // `div ~ p` should NOT match if div comes AFTER p.
        let sel = SelectorList::parse("div ~ p").unwrap();
        let siblings = make_siblings(&["p", "div"]);
        let ctx = SiblingContext {
            siblings: &siblings,
            index: 0,
        };
        assert!(!sel.matches(&siblings[0], &[], Some(ctx)));
    }

    #[test]
    fn general_sibling_no_spaces() {
        // `div~p` (no spaces).
        let sel = SelectorList::parse("div~p").unwrap();
        let siblings = make_siblings(&["div", "span", "p"]);
        let ctx = SiblingContext {
            siblings: &siblings,
            index: 2,
        };
        assert!(sel.matches(&siblings[2], &[], Some(ctx)));
    }

    // ── Attribute selector tests ──

    #[test]
    fn attribute_exists() {
        // `a[href]` matches <a href="...">.
        let sel = SelectorList::parse("a[href]").unwrap();
        let node = elem("a", &[("href", "https://example.com")]);
        assert!(sel.matches(&node, &[], None));
    }

    #[test]
    fn attribute_exists_no_match() {
        // `a[href]` does NOT match <a> without href.
        let sel = SelectorList::parse("a[href]").unwrap();
        let node = elem("a", &[]);
        assert!(!sel.matches(&node, &[], None));
    }

    #[test]
    fn attribute_equals() {
        // `[type="text"]` matches input with type=text.
        let sel = SelectorList::parse("[type=\"text\"]").unwrap();
        let node = elem("input", &[("type", "text")]);
        assert!(sel.matches(&node, &[], None));
    }

    #[test]
    fn attribute_equals_no_match() {
        let sel = SelectorList::parse("[type=\"text\"]").unwrap();
        let node = elem("input", &[("type", "password")]);
        assert!(!sel.matches(&node, &[], None));
    }

    #[test]
    fn attribute_starts_with() {
        let sel = SelectorList::parse("[href^=\"https\"]").unwrap();
        let node = elem("a", &[("href", "https://example.com")]);
        assert!(sel.matches(&node, &[], None));

        let node2 = elem("a", &[("href", "http://example.com")]);
        assert!(!sel.matches(&node2, &[], None));
    }

    #[test]
    fn attribute_ends_with() {
        let sel = SelectorList::parse("[src$=\".png\"]").unwrap();
        let node = elem("img", &[("src", "photo.png")]);
        assert!(sel.matches(&node, &[], None));

        let node2 = elem("img", &[("src", "photo.jpg")]);
        assert!(!sel.matches(&node2, &[], None));
    }

    #[test]
    fn attribute_contains() {
        let sel = SelectorList::parse("[class*=\"btn\"]").unwrap();
        let node = elem("div", &[("class", "my-btn-primary")]);
        assert!(sel.matches(&node, &[], None));

        let node2 = elem("div", &[("class", "link")]);
        assert!(!sel.matches(&node2, &[], None));
    }

    #[test]
    fn attribute_selector_with_tag() {
        // `input[type="text"]` combines tag + attribute.
        let sel = SelectorList::parse("input[type=\"text\"]").unwrap();
        let input = elem("input", &[("type", "text")]);
        assert!(sel.matches(&input, &[], None));

        // Wrong tag should not match.
        let div = elem("div", &[("type", "text")]);
        assert!(!sel.matches(&div, &[], None));
    }

    #[test]
    fn attribute_specificity() {
        // [type="text"] counts as class-level specificity (0, 1, 0).
        let sel = SelectorList::parse("[type=\"text\"]").unwrap();
        assert_eq!(sel.specificity(), Specificity(0, 1, 0));

        // input[type="text"] = (0, 1, 1).
        let sel2 = SelectorList::parse("input[type=\"text\"]").unwrap();
        assert_eq!(sel2.specificity(), Specificity(0, 1, 1));
    }

    // ── Mixed combinator tests ──

    #[test]
    fn child_and_descendant_combined() {
        // `div > ul li` — ul must be direct child of div, li is descendant of ul.
        let sel = SelectorList::parse("div > ul li").unwrap();
        let div = elem("div", &[]);
        let ul = elem("ul", &[]);
        let li = elem("li", &[]);
        // ancestors for li: [div, ul]
        assert!(sel.matches(&li, &[&div, &ul], None));
    }

    #[test]
    fn combinator_parsing_preserves_order() {
        let sel = SelectorList::parse("a > b + c ~ d").unwrap();
        assert_eq!(sel.selectors[0].parts.len(), 4);
        assert_eq!(sel.selectors[0].combinators.len(), 3);
        assert_eq!(sel.selectors[0].combinators[0], Combinator::Child);
        assert_eq!(sel.selectors[0].combinators[1], Combinator::Adjacent);
        assert_eq!(sel.selectors[0].combinators[2], Combinator::General);
    }

    #[test]
    fn nth_child_with_plus_not_confused_with_combinator() {
        // `:nth-child(2n+1)` — the `+` inside parens must NOT be treated as a combinator.
        let sel = SelectorList::parse("li:nth-child(2n+1)").unwrap();
        assert_eq!(sel.selectors[0].parts.len(), 1);
        let compound = &sel.selectors[0].parts[0];
        assert_eq!(compound.tag, Some("li".into()));
        match &compound.pseudos[0] {
            PseudoClass::NthChild(f) => assert_eq!(*f, NthFormula { a: 2, b: 1 }),
            _ => panic!("Expected NthChild"),
        }
    }

    // ── :not(), :is(), :where() pseudo-class tests ──

    #[test]
    fn not_pseudo_class_excludes() {
        let sel = SelectorList::parse("div:not(.hidden)").unwrap();
        let visible = elem("div", &[("class", "visible")]);
        assert!(sel.matches(&visible, &[], None));

        let hidden = elem("div", &[("class", "hidden")]);
        assert!(!sel.matches(&hidden, &[], None));
    }

    #[test]
    fn not_pseudo_class_with_tag() {
        let sel = SelectorList::parse(":not(span)").unwrap();
        let div = elem("div", &[]);
        assert!(sel.matches(&div, &[], None));

        let span = elem("span", &[]);
        assert!(!sel.matches(&span, &[], None));
    }

    #[test]
    fn is_pseudo_class_matches_any() {
        let sel = SelectorList::parse(":is(h1, h2, h3)").unwrap();
        assert!(sel.matches(&elem("h1", &[]), &[], None));
        assert!(sel.matches(&elem("h2", &[]), &[], None));
        assert!(sel.matches(&elem("h3", &[]), &[], None));
        assert!(!sel.matches(&elem("h4", &[]), &[], None));
    }

    #[test]
    fn where_pseudo_class_matches_any() {
        let sel = SelectorList::parse(":where(h1, h2)").unwrap();
        assert!(sel.matches(&elem("h1", &[]), &[], None));
        assert!(!sel.matches(&elem("h4", &[]), &[], None));
    }

    #[test]
    fn where_pseudo_zero_specificity() {
        // :where() should have zero specificity.
        let sel = SelectorList::parse(":where(.foo)").unwrap();
        assert_eq!(sel.specificity(), Specificity(0, 0, 0));
    }

    #[test]
    fn not_pseudo_specificity() {
        // :not(.foo) should have the specificity of .foo = (0,1,0).
        let sel = SelectorList::parse(":not(.foo)").unwrap();
        assert_eq!(sel.specificity(), Specificity(0, 1, 0));
    }

    #[test]
    fn is_pseudo_specificity() {
        // :is(#id, .class) should take the highest = (1,0,0).
        let sel = SelectorList::parse(":is(#id, .class)").unwrap();
        assert_eq!(sel.specificity(), Specificity(1, 0, 0));
    }
}

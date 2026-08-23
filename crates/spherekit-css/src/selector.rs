//! Selectors: what a rule matches, and how strongly.
//!
//! The matcher is right-to-left, like every real CSS engine, because the
//! rightmost compound is the one that has to match the node being styled and
//! rejecting on it first is what makes an unmatched rule cost a single string
//! comparison.
//!
//! Matching needs more than the node itself. A combinator needs ancestors, and
//! `:nth-child()` needs a position; both arrive through [`MatchPath`] and
//! [`Node::with_index`] rather than by widening [`Node`] into a tree handle.
//! That keeps [`Node`] `Copy` and lets a caller that only has one detached node
//! — which is what the React host has during lowering — keep using it.

use std::cmp::Ordering;
use std::fmt;

/// The interaction flags a pseudo-class can test.
///
/// Deliberately a plain struct of booleans rather than a bitflag set: it is
/// built by the caller on every commit, and named fields make the call sites
/// read as prose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ElementState {
    /// The pointer is over the element.
    pub hover: bool,
    /// A pointer button is held on the element.
    pub active: bool,
    /// The element holds keyboard focus.
    pub focus: bool,
    /// The element does not accept input.
    pub disabled: bool,
    /// The element is a toggle or checkbox that is on.
    pub checked: bool,
}

/// A node identity used during selector matching.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Node<'a> {
    /// Native element name, such as `view`, `text` or `button`.
    pub element: &'a str,
    /// Optional author-facing id from the React/native props.
    pub id: Option<&'a str>,
    /// Whitespace-separated class names.
    pub classes: &'a str,
    /// Interaction flags tested by `:hover`, `:active` and friends.
    pub state: ElementState,
    /// Zero-based position among the parent's children.
    index: usize,
    /// Number of children the parent has, or zero when unknown.
    siblings: usize,
}

impl<'a> Node<'a> {
    /// Creates a node with an element name and no id or classes.
    pub const fn new(element: &'a str) -> Self {
        Self {
            element,
            id: None,
            classes: "",
            state: ElementState {
                hover: false,
                active: false,
                focus: false,
                disabled: false,
                checked: false,
            },
            index: 0,
            siblings: 0,
        }
    }

    /// Adds an author-facing id to the node.
    pub const fn with_id(mut self, id: &'a str) -> Self {
        self.id = Some(id);
        self
    }

    /// Adds an optional author-facing id to the node.
    pub const fn with_id_option(mut self, id: Option<&'a str>) -> Self {
        self.id = id;
        self
    }

    /// Adds whitespace-separated classes to the node.
    pub const fn with_classes(mut self, classes: &'a str) -> Self {
        self.classes = classes;
        self
    }

    /// Adds interaction flags, which drive `:hover`, `:active`, `:focus`,
    /// `:disabled` and `:checked`.
    pub const fn with_state(self, state: ElementState) -> Self {
        Self { state, ..self }
    }

    /// Records the node's position among its siblings, for `:nth-child()`.
    ///
    /// `index` is zero-based to match how a caller iterates its children;
    /// `:nth-child()` arithmetic is one-based, and the conversion happens here
    /// so no caller has to remember it.
    pub const fn with_index(self, index: usize, siblings: usize) -> Self {
        Self { index, siblings, ..self }
    }

    /// One-based position and total sibling count.
    ///
    /// A node that was never given a position is treated as an only child,
    /// which makes `:first-child` and `:last-child` both true. That is the
    /// right answer for the React host's single detached node: there is no
    /// sibling that could displace it.
    fn position(self) -> (usize, usize) {
        (self.index + 1, self.siblings.max(self.index + 1))
    }

    fn has_class(self, class: &str) -> bool {
        self.classes.split_whitespace().any(|item| item == class)
    }
}

impl fmt::Display for Node<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.element)
    }
}

/// A node together with the ancestors and preceding siblings a combinator needs.
///
/// The subject is the last node; ancestors run outermost first. A path built
/// with [`MatchPath::new`] has neither, which means descendant and child
/// combinators cannot match it and `:root` can — a lone node is its own root.
#[derive(Clone, Copy, Debug, Default)]
pub struct MatchPath<'a> {
    ancestors: &'a [Node<'a>],
    preceding: &'a [Node<'a>],
    node: Node<'a>,
}

impl<'a> MatchPath<'a> {
    /// A path containing only the node itself.
    pub fn new(node: Node<'a>) -> Self {
        Self { ancestors: &[], preceding: &[], node }
    }

    /// A path with the node's ancestor chain, outermost first.
    pub fn with_ancestors(ancestors: &'a [Node<'a>], node: Node<'a>) -> Self {
        Self { ancestors, preceding: &[], node }
    }

    /// Adds the subject's preceding siblings, in document order.
    ///
    /// Without them `+` and `~` never match, rather than matching everything:
    /// a caller that cannot supply siblings has not told the engine that there
    /// are none, and guessing in the permissive direction would paint the wrong
    /// element.
    pub fn with_preceding_siblings(mut self, siblings: &'a [Node<'a>]) -> Self {
        self.preceding = siblings;
        self
    }

    /// The node being styled.
    pub fn subject(&self) -> Node<'a> {
        self.node
    }

    /// The ancestor chain, outermost first.
    pub fn ancestors(&self) -> &'a [Node<'a>] {
        self.ancestors
    }

    /// The same path with different interaction flags on the subject.
    pub(crate) fn with_subject_state(&self, state: ElementState) -> Self {
        Self {
            ancestors: self.ancestors,
            preceding: self.preceding,
            node: self.node.with_state(state),
        }
    }
}

/// Selector weight, compared as the usual `(ids, classes, elements)` triple.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Specificity {
    pub ids: u16,
    pub classes: u16,
    pub elements: u16,
}

impl Specificity {
    /// Inline declarations outrank every selector, however many ids it names.
    pub(crate) const INLINE: Self = Self { ids: u16::MAX, classes: 0, elements: 0 };

    fn add(self, other: Self) -> Self {
        Self {
            ids: self.ids.saturating_add(other.ids),
            classes: self.classes.saturating_add(other.classes),
            elements: self.elements.saturating_add(other.elements),
        }
    }
}

impl Ord for Specificity {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.ids, self.classes, self.elements).cmp(&(other.ids, other.classes, other.elements))
    }
}

impl PartialOrd for Specificity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// An `an+b` expression from `:nth-child()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Nth {
    step: i32,
    offset: i32,
}

impl Nth {
    fn matches(self, position: usize) -> bool {
        let position = position as i32;
        if self.step == 0 {
            return position == self.offset;
        }
        let delta = position - self.offset;
        delta % self.step == 0 && delta / self.step >= 0
    }
}

/// The pseudo-classes the engine understands.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PseudoClass {
    Hover,
    Active,
    Focus,
    Disabled,
    Checked,
    FirstChild,
    LastChild,
    NthChild(Nth),
    Root,
    /// `:not()` over a list of compound selectors.
    Not(Vec<Compound>),
}

/// Which interaction states a selector's outcome can depend on.
///
/// Used to skip cascade passes that provably cannot differ from the base one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StateUse {
    pub hover: bool,
    pub active: bool,
    pub focus: bool,
}

impl StateUse {
    /// Folds another selector's usage into this one.
    pub(crate) fn merge(&mut self, other: StateUse) {
        self.hover |= other.hover;
        self.active |= other.active;
        self.focus |= other.focus;
    }
}

/// One compound selector: everything between two combinators.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Compound {
    element: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    pseudos: Vec<PseudoClass>,
}

impl Compound {
    fn matches(&self, node: Node<'_>, is_root: bool) -> bool {
        self.element.as_deref().is_none_or(|element| element.eq_ignore_ascii_case(node.element))
            && self.id.as_deref().is_none_or(|id| node.id == Some(id))
            && self.classes.iter().all(|class| node.has_class(class))
            && self.pseudos.iter().all(|pseudo| matches_pseudo(pseudo, node, is_root))
    }

    fn specificity(&self) -> Specificity {
        let mut total = Specificity {
            ids: u16::from(self.id.is_some()),
            classes: self.classes.len() as u16,
            elements: u16::from(self.element.is_some()),
        };
        for pseudo in &self.pseudos {
            total = match pseudo {
                // `:not()` contributes the weight of its heaviest argument and
                // nothing of its own, which is what makes `:not(.a)` and `.a`
                // tie rather than letting negation win by construction.
                PseudoClass::Not(arguments) => {
                    total.add(arguments.iter().map(Compound::specificity).max().unwrap_or_default())
                }
                _ => total.add(Specificity { ids: 0, classes: 1, elements: 0 }),
            };
        }
        total
    }

    /// The most selective name in this compound, used to bucket rules.
    fn key(&self) -> BucketKey {
        if let Some(id) = &self.id {
            return BucketKey::Id(id.clone());
        }
        if let Some(class) = self.classes.first() {
            return BucketKey::Class(class.clone());
        }
        match &self.element {
            Some(element) => BucketKey::Element(element.to_ascii_lowercase()),
            None => BucketKey::Universal,
        }
    }
}

impl Compound {
    /// The interaction states this compound's match depends on.
    fn state_use(&self) -> StateUse {
        let mut used = StateUse::default();
        for pseudo in &self.pseudos {
            match pseudo {
                PseudoClass::Hover => used.hover = true,
                PseudoClass::Active => used.active = true,
                PseudoClass::Focus => used.focus = true,
                PseudoClass::Not(inner) => {
                    for compound in inner {
                        used.merge(compound.state_use());
                    }
                }
                _ => {}
            }
        }
        used
    }
}

fn matches_pseudo(pseudo: &PseudoClass, node: Node<'_>, is_root: bool) -> bool {
    let (position, total) = node.position();
    match pseudo {
        PseudoClass::Hover => node.state.hover,
        PseudoClass::Active => node.state.active,
        PseudoClass::Focus => node.state.focus,
        PseudoClass::Disabled => node.state.disabled,
        PseudoClass::Checked => node.state.checked,
        PseudoClass::FirstChild => position == 1,
        PseudoClass::LastChild => position == total,
        PseudoClass::NthChild(nth) => nth.matches(position),
        PseudoClass::Root => is_root,
        PseudoClass::Not(arguments) => {
            !arguments.iter().any(|argument| argument.matches(node, is_root))
        }
    }
}

/// How two compounds are joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Combinator {
    /// Whitespace: any ancestor.
    Descendant,
    /// `>`: the immediate parent.
    Child,
    /// `+`: the immediately preceding sibling.
    NextSibling,
    /// `~`: any preceding sibling.
    LaterSibling,
}

/// The bucket a rule is filed under, so that matching visits a small subset.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BucketKey {
    Id(String),
    Class(String),
    Element(String),
    Universal,
}

/// A complete selector: compounds joined by combinators, subject last.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Selector {
    parts: Vec<Compound>,
    /// `combinators[i]` joins `parts[i]` to `parts[i + 1]`.
    combinators: Vec<Combinator>,
}

impl Selector {
    /// The interaction states any compound in this selector depends on.
    ///
    /// Every compound counts, not just the subject: `.card:hover .title` styles
    /// the title according to the card's hover state, so a hover pass is needed
    /// for the title too.
    pub(crate) fn state_use(&self) -> StateUse {
        let mut used = StateUse::default();
        for compound in &self.parts {
            used.merge(compound.state_use());
        }
        used
    }

    /// Matches the subject of `path`.
    pub(crate) fn matches(&self, path: &MatchPath<'_>) -> bool {
        let subject = self.parts.last().expect("a selector always has one compound");
        let is_root = path.ancestors.is_empty();
        if !subject.matches(path.node, is_root) {
            return false;
        }
        self.matches_left_of(self.parts.len() - 1, path.ancestors, path.preceding)
    }

    /// Walks the remaining compounds leftwards over the ancestor chain.
    ///
    /// Descendant and later-sibling combinators backtrack, because a rule such
    /// as `.a .b .c` can match several ways and only one of them needs to work.
    fn matches_left_of(
        &self,
        index: usize,
        ancestors: &[Node<'_>],
        preceding: &[Node<'_>],
    ) -> bool {
        if index == 0 {
            return true;
        }
        let combinator = self.combinators[index - 1];
        let compound = &self.parts[index - 1];
        match combinator {
            Combinator::Child => {
                let Some((parent, rest)) = ancestors.split_last() else { return false };
                compound.matches(*parent, rest.is_empty())
                    && self.matches_left_of(index - 1, rest, &[])
            }
            Combinator::Descendant => {
                for split in (0..ancestors.len()).rev() {
                    let (rest, candidate) = ancestors.split_at(split);
                    if compound.matches(candidate[0], rest.is_empty())
                        && self.matches_left_of(index - 1, rest, &[])
                    {
                        return true;
                    }
                }
                false
            }
            Combinator::NextSibling => {
                let Some(sibling) = preceding.last() else { return false };
                // The sibling shares this node's parent, so the ancestor chain
                // carries over unchanged.
                compound.matches(*sibling, ancestors.is_empty())
                    && self.matches_left_of(index - 1, ancestors, &preceding[..preceding.len() - 1])
            }
            Combinator::LaterSibling => {
                for split in (0..preceding.len()).rev() {
                    if compound.matches(preceding[split], ancestors.is_empty())
                        && self.matches_left_of(index - 1, ancestors, &preceding[..split])
                    {
                        return true;
                    }
                }
                false
            }
        }
    }

    pub(crate) fn specificity(&self) -> Specificity {
        self.parts.iter().fold(Specificity::default(), |total, part| total.add(part.specificity()))
    }

    pub(crate) fn bucket_key(&self) -> BucketKey {
        self.parts.last().expect("a selector always has one compound").key()
    }
}

/// Parses one comma-separated selector list.
///
/// Selectors the engine does not model — attribute selectors, pseudo-elements,
/// `:has()`, namespaces — return `None` for that entry and are dropped, so the
/// rest of the list still works and the unsupported form does nothing rather
/// than something surprising.
pub(crate) fn parse_selector_list(text: &str) -> Vec<Selector> {
    split_top_level_commas(text).into_iter().filter_map(parse_selector).collect()
}

fn parse_selector(text: &str) -> Option<Selector> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut combinators = Vec::new();
    let mut pending: Option<Combinator> = None;
    let mut cursor = 0;
    let chars: Vec<char> = text.chars().collect();

    while cursor < chars.len() {
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
            if pending.is_none() && !parts.is_empty() {
                pending = Some(Combinator::Descendant);
            }
        }
        if cursor >= chars.len() {
            break;
        }
        if let Some(explicit) = match chars[cursor] {
            '>' => Some(Combinator::Child),
            '+' => Some(Combinator::NextSibling),
            '~' => Some(Combinator::LaterSibling),
            _ => None,
        } {
            if parts.is_empty() {
                return None;
            }
            pending = Some(explicit);
            cursor += 1;
            continue;
        }
        let (compound, next) = parse_compound(&chars, cursor)?;
        if !parts.is_empty() {
            combinators.push(pending.take()?);
        }
        parts.push(compound);
        pending = None;
        cursor = next;
    }

    if parts.is_empty() || pending.is_some() {
        return None;
    }
    Some(Selector { parts, combinators })
}

fn parse_compound(chars: &[char], mut cursor: usize) -> Option<(Compound, usize)> {
    let mut compound = Compound::default();
    let start = cursor;

    if chars.get(cursor).is_some_and(|c| c.is_alphabetic() || *c == '*' || *c == '_') {
        if chars[cursor] == '*' {
            cursor += 1;
        } else {
            let name_start = cursor;
            cursor = scan_ident(chars, cursor);
            compound.element = Some(chars[name_start..cursor].iter().collect());
        }
    }

    while cursor < chars.len() {
        match chars[cursor] {
            '#' => {
                cursor += 1;
                let name_start = cursor;
                cursor = scan_ident(chars, cursor);
                if name_start == cursor || compound.id.is_some() {
                    return None;
                }
                compound.id = Some(chars[name_start..cursor].iter().collect());
            }
            '.' => {
                cursor += 1;
                let name_start = cursor;
                cursor = scan_ident(chars, cursor);
                if name_start == cursor {
                    return None;
                }
                compound.classes.push(chars[name_start..cursor].iter().collect());
            }
            ':' => {
                let (pseudo, next) = parse_pseudo(chars, cursor)?;
                compound.pseudos.push(pseudo);
                cursor = next;
            }
            _ => break,
        }
    }

    if cursor == start {
        return None;
    }
    Some((compound, cursor))
}

fn parse_pseudo(chars: &[char], mut cursor: usize) -> Option<(PseudoClass, usize)> {
    cursor += 1;
    if chars.get(cursor) == Some(&':') {
        // A pseudo-*element*. There is no second box to style, so the whole
        // selector is dropped rather than silently styling the element itself.
        return None;
    }
    let name_start = cursor;
    cursor = scan_ident(chars, cursor);
    if name_start == cursor {
        return None;
    }
    let name: String = chars[name_start..cursor].iter().collect::<String>().to_ascii_lowercase();

    let argument = if chars.get(cursor) == Some(&'(') {
        let mut depth = 0usize;
        let open = cursor;
        while cursor < chars.len() {
            match chars[cursor] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        if cursor >= chars.len() {
            return None;
        }
        let text: String = chars[open + 1..cursor].iter().collect();
        cursor += 1;
        Some(text)
    } else {
        None
    };

    let pseudo = match (name.as_str(), argument) {
        ("hover", None) => PseudoClass::Hover,
        ("active", None) => PseudoClass::Active,
        ("focus", None) | ("focus-visible", None) | ("focus-within", None) => PseudoClass::Focus,
        ("disabled", None) => PseudoClass::Disabled,
        ("enabled", None) => PseudoClass::Not(vec![Compound {
            pseudos: vec![PseudoClass::Disabled],
            ..Compound::default()
        }]),
        ("checked", None) => PseudoClass::Checked,
        ("first-child", None) => PseudoClass::FirstChild,
        ("last-child", None) => PseudoClass::LastChild,
        ("root", None) => PseudoClass::Root,
        ("nth-child", Some(argument)) => PseudoClass::NthChild(parse_nth(&argument)?),
        ("not", Some(argument)) => {
            let arguments = split_top_level_commas(&argument)
                .iter()
                .map(|part| {
                    let chars: Vec<char> = part.trim().chars().collect();
                    match parse_compound(&chars, 0) {
                        Some((compound, next)) if next == chars.len() => Some(compound),
                        _ => None,
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            if arguments.is_empty() {
                return None;
            }
            PseudoClass::Not(arguments)
        }
        _ => return None,
    };
    Some((pseudo, cursor))
}

/// Parses `odd`, `even`, `3`, `n`, `2n`, `2n+1`, `-n+3` and their spacings.
fn parse_nth(text: &str) -> Option<Nth> {
    let compact: String =
        text.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match compact.as_str() {
        "odd" => return Some(Nth { step: 2, offset: 1 }),
        "even" => return Some(Nth { step: 2, offset: 2 }),
        "" => return None,
        _ => {}
    }
    let Some(n_index) = compact.find('n') else {
        return compact.parse::<i32>().ok().map(|offset| Nth { step: 0, offset });
    };
    let step = match &compact[..n_index] {
        "" | "+" => 1,
        "-" => -1,
        head => head.parse::<i32>().ok()?,
    };
    let tail = &compact[n_index + 1..];
    let offset = if tail.is_empty() { 0 } else { tail.parse::<i32>().ok()? };
    Some(Nth { step, offset })
}

fn scan_ident(chars: &[char], mut cursor: usize) -> usize {
    while cursor < chars.len()
        && (chars[cursor].is_alphanumeric() || matches!(chars[cursor], '-' | '_'))
    {
        cursor += 1;
    }
    cursor
}

/// Splits on commas that are not inside a function or a `:not()` argument.
pub(crate) fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, character) in text.char_indices() {
        match character {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(selector: &str, path: &MatchPath<'_>) -> bool {
        parse_selector_list(selector).iter().any(|item| item.matches(path))
    }

    fn hovered() -> ElementState {
        ElementState { hover: true, ..ElementState::default() }
    }

    #[test]
    fn a_compound_selector_requires_every_part() {
        let node = Node::new("view").with_id("main").with_classes("panel primary");
        let path = MatchPath::new(node);
        assert!(matches("view.panel#main.primary", &path));
        assert!(!matches("view.panel.missing", &path));
        assert!(!matches("button.panel", &path));
    }

    #[test]
    fn the_universal_selector_matches_anything() {
        assert!(matches("*", &MatchPath::new(Node::new("anything"))));
    }

    #[test]
    fn element_names_are_case_insensitive_but_classes_are_not() {
        let path = MatchPath::new(Node::new("view").with_classes("Panel"));
        assert!(matches("VIEW", &path));
        assert!(matches(".Panel", &path));
        assert!(!matches(".panel", &path));
    }

    #[test]
    fn specificity_orders_ids_over_classes_over_elements() {
        let id = parse_selector_list("#a").pop().unwrap().specificity();
        let class = parse_selector_list(".a.b.c").pop().unwrap().specificity();
        let element = parse_selector_list("view span").pop().unwrap().specificity();
        assert!(id > class);
        assert!(class > element);
    }

    #[test]
    fn the_universal_selector_adds_no_weight() {
        let universal = parse_selector_list("*").pop().unwrap().specificity();
        assert_eq!(universal, Specificity::default());
    }

    #[test]
    fn a_pseudo_class_weighs_the_same_as_a_class() {
        let pseudo = parse_selector_list("a:hover").pop().unwrap().specificity();
        let class = parse_selector_list("a.hover").pop().unwrap().specificity();
        assert_eq!(pseudo, class);
    }

    #[test]
    fn a_descendant_combinator_walks_the_whole_ancestor_chain() {
        let ancestors = [Node::new("root").with_classes("app"), Node::new("view")];
        let path = MatchPath::with_ancestors(&ancestors, Node::new("button"));
        assert!(matches(".app button", &path));
        assert!(matches("root view button", &path));
        assert!(!matches(".missing button", &path));
    }

    #[test]
    fn a_child_combinator_only_accepts_the_immediate_parent() {
        let ancestors = [Node::new("root").with_classes("app"), Node::new("view")];
        let path = MatchPath::with_ancestors(&ancestors, Node::new("button"));
        assert!(matches("view > button", &path));
        assert!(!matches(".app > button", &path));
        assert!(matches(".app > view > button", &path));
    }

    #[test]
    fn a_combinator_never_matches_a_path_with_no_ancestors() {
        let path = MatchPath::new(Node::new("button"));
        assert!(!matches("view button", &path));
        assert!(!matches("view > button", &path));
    }

    #[test]
    fn the_next_sibling_combinator_looks_only_at_the_one_before() {
        let siblings = [Node::new("label"), Node::new("input")];
        let path = MatchPath::new(Node::new("button")).with_preceding_siblings(&siblings);
        assert!(matches("input + button", &path));
        assert!(!matches("label + button", &path));
    }

    #[test]
    fn the_later_sibling_combinator_looks_at_all_of_them() {
        let siblings = [Node::new("label"), Node::new("input")];
        let path = MatchPath::new(Node::new("button")).with_preceding_siblings(&siblings);
        assert!(matches("label ~ button", &path));
        assert!(matches("input ~ button", &path));
        assert!(!matches("view ~ button", &path));
    }

    #[test]
    fn a_sibling_combinator_fails_closed_when_no_siblings_are_supplied() {
        let path = MatchPath::new(Node::new("button"));
        assert!(!matches("input + button", &path));
        assert!(!matches("input ~ button", &path));
    }

    #[test]
    fn interaction_pseudo_classes_read_the_node_state() {
        let idle = MatchPath::new(Node::new("button"));
        let hot = MatchPath::new(Node::new("button").with_state(hovered()));
        assert!(!matches("button:hover", &idle));
        assert!(matches("button:hover", &hot));
    }

    #[test]
    fn active_focus_disabled_and_checked_each_read_their_own_flag() {
        let state =
            ElementState { hover: false, active: true, focus: true, disabled: true, checked: true };
        let path = MatchPath::new(Node::new("input").with_state(state));
        assert!(matches("input:active", &path));
        assert!(matches("input:focus", &path));
        assert!(matches("input:disabled", &path));
        assert!(matches("input:checked", &path));
        assert!(!matches("input:hover", &path));
    }

    #[test]
    fn first_and_last_child_use_the_recorded_position() {
        let first = MatchPath::new(Node::new("view").with_index(0, 3));
        let middle = MatchPath::new(Node::new("view").with_index(1, 3));
        let last = MatchPath::new(Node::new("view").with_index(2, 3));
        assert!(matches("view:first-child", &first));
        assert!(!matches("view:first-child", &middle));
        assert!(matches("view:last-child", &last));
        assert!(!matches("view:last-child", &middle));
    }

    #[test]
    fn an_unpositioned_node_counts_as_an_only_child() {
        let path = MatchPath::new(Node::new("view"));
        assert!(matches("view:first-child", &path));
        assert!(matches("view:last-child", &path));
    }

    #[test]
    fn nth_child_handles_odd_even_and_plain_indices() {
        let at = |index| MatchPath::new(Node::new("view").with_index(index, 6));
        assert!(matches("view:nth-child(odd)", &at(0)));
        assert!(!matches("view:nth-child(odd)", &at(1)));
        assert!(matches("view:nth-child(even)", &at(1)));
        assert!(matches("view:nth-child(3)", &at(2)));
        assert!(!matches("view:nth-child(3)", &at(3)));
    }

    #[test]
    fn nth_child_arithmetic_covers_step_and_offset() {
        let at = |index| MatchPath::new(Node::new("view").with_index(index, 8));
        assert!(matches("view:nth-child(3n+1)", &at(0)));
        assert!(matches("view:nth-child(3n+1)", &at(3)));
        assert!(!matches("view:nth-child(3n+1)", &at(1)));
        assert!(matches("view:nth-child(n)", &at(4)));
        assert!(matches("view:nth-child(-n+2)", &at(1)));
        assert!(!matches("view:nth-child(-n+2)", &at(2)));
    }

    #[test]
    fn nth_child_tolerates_whitespace_inside_the_argument() {
        let path = MatchPath::new(Node::new("view").with_index(3, 8));
        assert!(matches("view:nth-child( 2n )", &path));
    }

    #[test]
    fn not_inverts_a_compound_and_borrows_its_weight() {
        let path = MatchPath::new(Node::new("view").with_classes("a"));
        assert!(matches("view:not(.b)", &path));
        assert!(!matches("view:not(.a)", &path));
        let negated = parse_selector_list("view:not(.b)").pop().unwrap().specificity();
        let plain = parse_selector_list("view.b").pop().unwrap().specificity();
        assert_eq!(negated, plain);
    }

    #[test]
    fn not_accepts_a_selector_list() {
        let path = MatchPath::new(Node::new("view").with_classes("a"));
        assert!(!matches("view:not(.a, .b)", &path));
        assert!(matches("view:not(.c, .b)", &path));
    }

    #[test]
    fn root_matches_only_a_node_with_no_ancestors() {
        let ancestors = [Node::new("app")];
        assert!(matches(":root", &MatchPath::new(Node::new("app"))));
        assert!(!matches(":root", &MatchPath::with_ancestors(&ancestors, Node::new("view"))));
    }

    #[test]
    fn unsupported_selectors_are_dropped_from_the_list_not_reinterpreted() {
        assert!(parse_selector_list("[data-role=knob]").is_empty());
        assert!(parse_selector_list("view::before").is_empty());
        assert!(parse_selector_list("view:has(.x)").is_empty());
        assert_eq!(parse_selector_list("view::before, .ok").len(), 1);
    }

    #[test]
    fn a_dangling_combinator_is_rejected() {
        assert!(parse_selector_list("view >").is_empty());
        assert!(parse_selector_list("> view").is_empty());
    }

    #[test]
    fn rules_bucket_under_the_most_selective_name_in_the_subject() {
        let key = |text: &str| parse_selector_list(text).pop().unwrap().bucket_key();
        assert_eq!(key(".a view#main"), BucketKey::Id("main".into()));
        assert_eq!(key("#a view.card"), BucketKey::Class("card".into()));
        assert_eq!(key(".a > button"), BucketKey::Element("button".into()));
        assert_eq!(key(".a *"), BucketKey::Universal);
    }
}

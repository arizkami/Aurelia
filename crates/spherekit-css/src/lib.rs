//! # spherekit-css
//!
//! A native-first CSS runtime for SphereKit.
//!
//! The crate deliberately sits between a browser CSS engine and a collection of
//! ad-hoc style helpers. [`Stylesheet`] parses a useful CSS subset, applies
//! selector specificity and `!important`, and produces a [`ResolvedStyle`] that
//! can be used by a native element or by the Rust host behind
//! `spherekit-react`.
//!
//! `spherekit-css` uses Servo's `cssparser` for CSS value tokenisation, while
//! SphereKit keeps ownership of the selector engine, the cascade and the
//! supported property model. That split is what keeps the runtime small and
//! makes unsupported browser-only features explicit instead of silently giving
//! them different native semantics.
//!
//! ## The one rule everything else follows
//!
//! **Syntax the engine does not model is ignored, never reinterpreted.**
//! `width: 12` does not become `12px`; `border-radius: 8px / 4px` does not
//! become a circular radius; an unknown media feature makes its query false
//! rather than true. A stylesheet that does nothing is debuggable. A stylesheet
//! that does something slightly different from what it says is not.
//!
//! ```
//! use spherekit_css::{Node, Stylesheet};
//! use spherekit_core::Length;
//!
//! let sheet = Stylesheet::parse(".panel { display: flex; gap: 8px; }").unwrap();
//! let style = sheet.resolve(Node::new("view").with_classes("panel"), None);
//! assert_eq!(style.layout.gap.width.get(), 8.0);
//! assert_eq!(style.layout.display, spherekit_layout::Display::Flex);
//! assert!(matches!(style.layout.size.width, Length::Auto));
//! ```
//!
//! ## Beyond a single flat node
//!
//! Combinators need ancestors and `:nth-child()` needs a position, so anything
//! past a simple selector resolves through a [`MatchPath`] and a
//! [`StyleContext`]:
//!
//! ```
//! use spherekit_css::{ElementState, MatchPath, Node, StyleContext, Stylesheet};
//!
//! let sheet = Stylesheet::parse(
//!     ":root { --accent: #ff8800; }
//!      .rack > button:hover { background: var(--accent); }",
//! )
//! .unwrap();
//!
//! let ancestors = [Node::new("view").with_classes("rack")];
//! let hovered = ElementState { hover: true, ..ElementState::default() };
//! let path = MatchPath::with_ancestors(&ancestors, Node::new("button").with_state(hovered));
//!
//! let style = sheet.resolve_in(&path, None, &StyleContext::default());
//! assert_eq!(style.background, Some(spherekit_core::Color::hex(0xFF8800)));
//! ```

#![deny(missing_docs)]
#![warn(clippy::doc_markdown)]

mod color;
mod length;
mod parser;
mod property;
mod selector;
mod text;

pub use selector::{ElementState, MatchPath, Node};
pub use text::TextProperties;

use parser::{Declaration, Rule, substitute_vars};
use selector::{BucketKey, Specificity, StateUse};
use spherekit_core::{Color, Corners, Px, Shadow, Size, px};
use spherekit_layout::Style;
use spherekit_ui::{Cursor, FocusRing, Styled};
use std::collections::HashMap;
use thiserror::Error;

/// Which of a stylesheet's two colour worlds is active.
///
/// Modelled as an enum rather than a boolean because `prefers-color-scheme` has
/// exactly these two answers and a `no_preference` value would have to be
/// resolved to one of them before any rule could match anyway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ColorScheme {
    /// Light surfaces, dark text.
    #[default]
    Light,
    /// Dark surfaces, light text.
    Dark,
}

/// Everything outside a declaration that a value can depend on.
///
/// Font-relative and viewport-relative units are resolved during the cascade
/// rather than carried through to layout, because SphereKit's [`spherekit_core::Length`]
/// has no unit vocabulary of its own — resolving here is what keeps `2rem` and
/// `32px` indistinguishable to everything downstream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StyleContext {
    /// Font size of the root element, which `rem` resolves against.
    pub root_font_size: Px,
    /// Font size in effect for this element, which `em` resolves against.
    pub font_size: Px,
    /// Viewport size, which `vw`, `vh`, `vmin`, `vmax` and the width and
    /// height media features resolve against.
    pub viewport: Size<Px>,
    /// Which `prefers-color-scheme` query is satisfied.
    pub color_scheme: ColorScheme,
}

impl Default for StyleContext {
    fn default() -> Self {
        Self {
            root_font_size: px(16.0),
            font_size: px(16.0),
            viewport: Size::new(px(1280.0), px(720.0)),
            color_scheme: ColorScheme::Light,
        }
    }
}

impl StyleContext {
    /// Sets the size `rem` resolves against.
    pub const fn with_root_font_size(mut self, size: Px) -> Self {
        self.root_font_size = size;
        self
    }

    /// Sets the size `em` resolves against.
    pub const fn with_font_size(mut self, size: Px) -> Self {
        self.font_size = size;
        self
    }

    /// Sets the viewport used by viewport units and media queries.
    pub const fn with_viewport(mut self, viewport: Size<Px>) -> Self {
        self.viewport = viewport;
        self
    }

    /// Sets the active colour scheme.
    pub const fn with_color_scheme(mut self, scheme: ColorScheme) -> Self {
        self.color_scheme = scheme;
        self
    }
}

/// A parse or value error returned by [`Stylesheet::parse`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CssError {
    /// A stylesheet rule did not have a selector and declaration block.
    #[error("invalid CSS rule near byte {offset}: {message}")]
    InvalidRule {
        /// Byte offset where the parser detected the problem.
        offset: usize,
        /// Human-readable parser detail.
        message: String,
    },
    /// A declaration was missing its colon or property name.
    #[error("invalid CSS declaration `{declaration}`")]
    InvalidDeclaration {
        /// Original declaration text.
        declaration: String,
    },
}

/// A CSS stylesheet with parsed rules in source order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stylesheet {
    rules: Vec<Rule>,
    /// Rule indices grouped by the most selective name in each rule's subject
    /// compound.
    ///
    /// Selector matching runs once per node per commit, so a linear scan makes
    /// styling cost `rules × nodes` — a 300-rule sheet over a 500-node tree is
    /// 150 000 selector evaluations for a frame in which nothing may have
    /// changed. Bucketing turns that into "look up my id, my classes and my
    /// element name", which for a typical node visits a handful of rules and,
    /// crucially, never touches a rule for a class the node does not carry.
    /// Rules whose subject compound names no id, class or element.
    universal: Vec<usize>,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    /// Keyed by lower-cased element name.
    by_element: HashMap<String, Vec<usize>>,
    /// Which interaction states any rule in the sheet actually tests.
    ///
    /// `resolve_interactive` runs the whole cascade once per state. A sheet
    /// that never writes `:active` cannot produce an active style that differs
    /// from the base one, so running that pass is provably wasted work — and it
    /// is the common case: most sheets style hover and nothing else. On a
    /// 3500-node tree the difference is the whole frame budget.
    states: StateUse,
}

impl Stylesheet {
    /// Parses a CSS stylesheet.
    ///
    /// Supported selectors are comma-separated lists of compound selectors
    /// joined by the descendant, `>`, `+` and `~` combinators, where a compound
    /// is an optional element name plus any number of ids, classes and
    /// pseudo-classes. `@media` blocks are parsed and kept; their rules simply
    /// do not match while the query is false. Attribute selectors,
    /// pseudo-elements and every other at-rule are ignored as rules rather than
    /// applied with surprising semantics.
    pub fn parse(source: &str) -> Result<Self, CssError> {
        let rules = parser::parse_rules(source)?;
        let mut universal = Vec::new();
        let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_class: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_element: HashMap<String, Vec<usize>> = HashMap::new();
        let mut states = StateUse::default();
        for (index, rule) in rules.iter().enumerate() {
            // Separate maps rather than one keyed by an enum: a
            // `HashMap<String, _>` can be looked up with a `&str`, and an
            // enum-keyed one has to be handed an owned key. That difference is
            // a `String` allocation per class per node per frame.
            match rule.selector.bucket_key() {
                BucketKey::Universal => universal.push(index),
                BucketKey::Id(id) => by_id.entry(id).or_default().push(index),
                BucketKey::Class(class) => by_class.entry(class).or_default().push(index),
                BucketKey::Element(element) => {
                    by_element.entry(element.to_ascii_lowercase()).or_default().push(index)
                }
            }
            states.merge(rule.selector.state_use());
        }
        Ok(Self { rules, universal, by_id, by_class, by_element, states })
    }

    /// Returns the number of selectors retained by the runtime.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Resolves stylesheet rules and optional inline declarations for a node.
    ///
    /// `inline_css` is useful for native callers that already have a CSS style
    /// string. React callers can use the same path after serialising their
    /// `style` object to declarations.
    pub fn resolve(&self, node: Node<'_>, inline_css: Option<&str>) -> ResolvedStyle {
        self.resolve_in(&MatchPath::new(node), inline_css, &StyleContext::default())
    }

    /// Resolves a node with no inline declarations.
    pub fn resolve_node(&self, node: Node<'_>) -> ResolvedStyle {
        self.resolve(node, None)
    }

    /// Resolves a node together with its ancestors, under an explicit context.
    ///
    /// Custom properties are collected down the whole path, so `:root { --x }`
    /// reaches a descendant even though nothing else in this crate inherits.
    /// That asymmetry is deliberate: a custom property is a *value*, not a
    /// computed style, and an author who defines one on the root and uses it
    /// three levels down is not asking for CSS inheritance — they are asking
    /// for a constant.
    pub fn resolve_in(
        &self,
        path: &MatchPath<'_>,
        inline_css: Option<&str>,
        context: &StyleContext,
    ) -> ResolvedStyle {
        let variables = self.custom_properties(path, inline_css, context);
        let mut winners: Vec<Winner<'_>> = Vec::new();

        for rule in self.matching_rules(path, context) {
            let specificity = rule.selector.specificity();
            for declaration in &rule.declarations {
                consider(&mut winners, declaration, specificity, rule.order);
            }
        }
        // Parsed into a binding rather than a temporary so the winners can
        // borrow from it for the rest of this function.
        let inline = inline_css.and_then(|css| parser::parse_declarations(css).ok());
        if let Some(declarations) = inline.as_deref() {
            for declaration in declarations {
                consider(&mut winners, declaration, Specificity::INLINE, usize::MAX);
            }
        }

        // `var()` is substituted after the cascade, not before, so that a
        // variable defined by a losing rule cannot leak into a winning value.
        let mut resolved: Vec<(&str, std::borrow::Cow<'_, str>)> = winners
            .into_iter()
            .filter(|winner| !winner.property.starts_with("--"))
            .filter_map(|winner| {
                substitute_vars(winner.value, &variables, 0).map(|value| (winner.property, value))
            })
            .collect();
        resolved.sort_by(|a, b| {
            (property::apply_tier(a.0), a.0).cmp(&(property::apply_tier(b.0), b.0))
        });

        // `font-size` has to land before anything that measures in `em`, and it
        // is itself measured against the *parent's* size, so it is applied with
        // the incoming context and everything else with the updated one.
        let mut inner = *context;
        if let Some((_, value)) = resolved.iter().find(|(property, _)| *property == "font-size")
            && let Some(size) = text::parse_font_size(value, context)
        {
            inner.font_size = size;
        }

        let mut style = ResolvedStyle::default();
        for (name, value) in &resolved {
            let scope = if *name == "font-size" { context } else { &inner };
            property::apply_declaration(&mut style, name, value, scope);
        }
        style
    }

    /// Resolves the base style plus the hover, active and focus variants.
    ///
    /// A variant is `None` when it computes to exactly the base style, so a
    /// caller can tell "the author styled `:hover`" from "the author did not"
    /// without diffing two full structs itself.
    pub fn resolve_interactive(
        &self,
        path: &MatchPath<'_>,
        inline_css: Option<&str>,
        context: &StyleContext,
    ) -> InteractiveStyle {
        let base = self.resolve_in(path, inline_css, context);
        // Each variant is a full cascade. Running one for a state no rule in
        // the sheet mentions cannot produce anything but a copy of the base,
        // and it costs exactly as much as a pass that could — so the sheet's
        // precomputed usage decides whether to bother. A sheet with only
        // `:hover` does two passes here instead of four.
        let variant = |wanted: bool, mutate: fn(&mut ElementState)| {
            if !wanted {
                return None;
            }
            let mut state = path.subject().state;
            mutate(&mut state);
            let variant = self.resolve_in(&path.with_subject_state(state), inline_css, context);
            (variant != base).then_some(variant)
        };
        let hover = variant(self.states.hover, |state| state.hover = true);
        // A pressed pointer is also a hovering pointer, and a stylesheet that
        // only defines `:hover` should still light up on press.
        let active = variant(self.states.hover || self.states.active, |state| {
            state.hover = true;
            state.active = true;
        });
        let focus = variant(self.states.focus, |state| state.focus = true);
        InteractiveStyle { base, hover, active, focus }
    }

    /// Rules whose media conditions hold and whose selector matches the path.
    fn matching_rules<'s>(&'s self, path: &MatchPath<'_>, context: &StyleContext) -> Vec<&'s Rule> {
        self.candidate_rules(path.subject())
            .into_iter()
            .map(|index| &self.rules[index])
            .filter(|rule| rule.media.iter().all(|query| query.matches(context)))
            .filter(|rule| rule.selector.matches(path))
            .collect()
    }

    /// Rule indices that could possibly match this node, in source order.
    fn candidate_rules(&self, node: Node<'_>) -> Vec<usize> {
        let mut indices: Vec<usize> = Vec::new();
        indices.extend_from_slice(&self.universal);

        // Element names are almost always written lower-case already, so the
        // borrowed lookup succeeds without allocating; the owned fallback is
        // for the sheet that spells one `View`.
        if let Some(bucket) = self.by_element.get(node.element) {
            indices.extend_from_slice(bucket);
        } else if node.element.bytes().any(|byte| byte.is_ascii_uppercase())
            && let Some(bucket) = self.by_element.get(&node.element.to_ascii_lowercase())
        {
            indices.extend_from_slice(bucket);
        }

        if let Some(id) = node.id
            && let Some(bucket) = self.by_id.get(id)
        {
            indices.extend_from_slice(bucket);
        }
        for class in node.classes.split_whitespace() {
            if let Some(bucket) = self.by_class.get(class) {
                indices.extend_from_slice(bucket);
            }
        }
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    /// Collects custom properties from the whole ancestor path.
    fn custom_properties(
        &self,
        path: &MatchPath<'_>,
        inline_css: Option<&str>,
        context: &StyleContext,
    ) -> HashMap<String, String> {
        let mut variables = HashMap::new();
        let ancestors = path.ancestors();
        for depth in 0..ancestors.len() {
            let ancestor = MatchPath::with_ancestors(&ancestors[..depth], ancestors[depth]);
            self.collect_variables(&ancestor, None, context, &mut variables);
        }
        self.collect_variables(path, inline_css, context, &mut variables);
        variables
    }

    fn collect_variables(
        &self,
        path: &MatchPath<'_>,
        inline_css: Option<&str>,
        context: &StyleContext,
        variables: &mut HashMap<String, String>,
    ) {
        let mut winners: Vec<Winner> = Vec::new();
        for rule in self.matching_rules(path, context) {
            let specificity = rule.selector.specificity();
            for declaration in &rule.declarations {
                if declaration.property.starts_with("--") {
                    consider(&mut winners, declaration, specificity, rule.order);
                }
            }
        }
        // Bound rather than left a temporary, so the winners can borrow from it
        // until they are copied into the map below.
        let inline = inline_css.and_then(|css| parser::parse_declarations(css).ok());
        if let Some(declarations) = inline.as_deref() {
            for declaration in declarations {
                if declaration.property.starts_with("--") {
                    consider(&mut winners, declaration, Specificity::INLINE, usize::MAX);
                }
            }
        }
        // Owned here and only here: the variable map outlives the borrow of the
        // declarations it came from, and there are a handful of custom
        // properties in a sheet against thousands of ordinary ones.
        for winner in winners {
            variables.insert(winner.property.to_owned(), winner.value.to_owned());
        }
    }
}

/// The computed subset of CSS understood by SphereKit.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStyle {
    /// Layout style consumed by `spherekit-layout` and Taffy.
    pub layout: Style,
    /// Typography, which a [`spherekit_ui::Label`] consumes separately because
    /// the [`Styled`] trait has no text vocabulary.
    pub text: TextProperties,
    /// Optional CSS background fill.
    pub background: Option<Color>,
    /// Border colour. It is meaningful when [`Self::border_width`] is nonzero.
    pub border_color: Option<Color>,
    /// Uniform border width.
    pub border_width: Option<Px>,
    /// Uniform corner radius, set only when all four corners agree.
    pub corner_radius: Option<Px>,
    /// Per-corner radii.
    pub corner_radii: Option<Corners<Px>>,
    /// Optional paint opacity.
    pub paint_opacity: Option<f32>,
    /// Whether the native painter should clip descendants to this box.
    pub clip_content: bool,
    /// Drop shadows, in paint order.
    pub shadows: Vec<Shadow>,
    /// Pointer shape while over this element.
    pub cursor: Option<Cursor>,
}

impl Default for ResolvedStyle {
    fn default() -> Self {
        Self {
            layout: Style::DEFAULT,
            text: TextProperties::default(),
            background: None,
            border_color: None,
            border_width: None,
            corner_radius: None,
            corner_radii: None,
            paint_opacity: None,
            clip_content: false,
            shadows: Vec::new(),
            cursor: None,
        }
    }
}

impl ResolvedStyle {
    /// Applies this computed style to any native SphereKit element.
    ///
    /// An empty shadow list leaves the element's own shadows alone rather than
    /// clearing them: a stylesheet that never mentions `box-shadow` must not
    /// strip the elevation a widget gave itself, and this type cannot tell
    /// "unset" from `box-shadow: none` once the cascade has run.
    pub fn apply_to<T: Styled>(&self, mut element: T) -> T {
        *element.style_mut() = self.layout.clone();
        if let Some(background) = self.background {
            element.paint_style_mut().background = Some(background.into());
        }
        if let Some(color) = self.border_color {
            element.paint_style_mut().border_color = color;
        }
        if let Some(width) = self.border_width {
            element.paint_style_mut().border_width = width;
        }
        if let Some(radii) = self.corner_radii.or_else(|| self.corner_radius.map(Corners::all)) {
            element.paint_style_mut().corner_radii = radii;
        }
        if let Some(opacity) = self.paint_opacity {
            element.paint_style_mut().opacity = opacity;
        }
        if self.clip_content {
            element.paint_style_mut().clip_content = true;
        }
        if !self.shadows.is_empty() {
            element.paint_style_mut().shadows = self.shadows.iter().copied().collect();
        }
        if let Some(cursor) = self.cursor {
            element.paint_style_mut().cursor = Some(cursor);
        }
        element
    }
}

/// A base style plus the interaction variants a widget paints itself.
///
/// [`spherekit_ui::PaintStyle`] already carries `hover_background` and
/// `active_background` so that a hover costs a repaint and not a rebuild. This
/// type is the cascade's side of that bargain: resolve all the states once, at
/// commit time, and hand the element everything it needs.
#[derive(Clone, Debug, PartialEq)]
pub struct InteractiveStyle {
    /// The style with no interaction flags set.
    pub base: ResolvedStyle,
    /// The style under `:hover`, when it differs from the base.
    pub hover: Option<ResolvedStyle>,
    /// The style under `:active`, when it differs from the base.
    pub active: Option<ResolvedStyle>,
    /// The style under `:focus`, when it differs from the base.
    pub focus: Option<ResolvedStyle>,
}

impl InteractiveStyle {
    /// Applies the base style and the interaction backgrounds.
    ///
    /// The focus variant becomes a [`FocusRing`] rather than a background,
    /// because that is the only focus affordance `spherekit-ui` paints. It is
    /// built from the variant's border colour and width — `:focus {
    /// border-color: … }` is the closest CSS idiom to a ring — and only when
    /// the variant actually changes the border colour, so a `:focus` rule about
    /// something else does not conjure a ring out of nothing.
    pub fn apply_to<T: Styled>(&self, element: T) -> T {
        let mut element = self.base.apply_to(element);
        if let Some(hover) = self.hover.as_ref().and_then(|style| style.background) {
            element.paint_style_mut().hover_background = Some(hover.into());
        }
        if let Some(active) = self.active.as_ref().and_then(|style| style.background) {
            element.paint_style_mut().active_background = Some(active.into());
        }
        if let Some(focus) = self.focus.as_ref()
            && focus.border_color != self.base.border_color
            && let Some(color) = focus.border_color
        {
            element.paint_style_mut().focus_ring = Some(FocusRing {
                color,
                width: focus.border_width.unwrap_or(px(2.0)),
                offset: Px::ZERO,
            });
        }
        element
    }
}

/// One property's current best candidate during the cascade.
///
/// Borrows from the rule it came from. The cascade visits every declaration of
/// every matching rule, for every node, on every frame; cloning the property
/// and value out of each one — before even knowing whether it wins — was the
/// single largest cost in styling a large tree.
#[derive(Clone, Copy, Debug)]
struct Winner<'a> {
    property: &'a str,
    value: &'a str,
    important: bool,
    specificity: Specificity,
    order: usize,
}

fn consider<'a>(
    winners: &mut Vec<Winner<'a>>,
    declaration: &'a Declaration,
    specificity: Specificity,
    order: usize,
) {
    let candidate = Winner {
        property: &declaration.property,
        value: &declaration.value,
        important: declaration.important,
        specificity,
        order: order.saturating_mul(10_000).saturating_add(declaration.order),
    };
    if let Some(existing) = winners.iter_mut().find(|item| item.property == candidate.property) {
        let candidate_rank = (candidate.important, candidate.specificity, candidate.order);
        let existing_rank = (existing.important, existing.specificity, existing.order);
        if candidate_rank > existing_rank {
            *existing = candidate;
        }
    } else {
        winners.push(candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::Length;
    use spherekit_layout::FlexDirection;
    use spherekit_ui::{Element, div};

    #[test]
    fn parses_and_cascades_selectors_by_specificity_and_order() {
        let sheet = Stylesheet::parse(
            r#"
            .panel { padding: 4px 8px; gap: 2px; }
            view.panel { gap: 6px; flex-direction: column; }
            #main { gap: 10px !important; }
            "#,
        )
        .unwrap();
        let style = sheet.resolve(Node::new("view").with_id("main").with_classes("panel"), None);
        assert_eq!(style.layout.padding.top, Length::Px(px(4.0)));
        assert_eq!(style.layout.padding.left, Length::Px(px(8.0)));
        assert_eq!(style.layout.gap.width, px(10.0));
        assert_eq!(style.layout.flex_direction, FlexDirection::Column);
    }

    #[test]
    fn inline_declarations_win_over_stylesheet_rules() {
        let sheet = Stylesheet::parse(".panel { width: 10px; opacity: 0.5; }").unwrap();
        let style = sheet
            .resolve(Node::new("view").with_classes("panel"), Some("width: 42px; opacity: 0.8"));
        assert_eq!(style.layout.size.width, Length::Px(px(42.0)));
        assert_eq!(style.layout.opacity, 0.8);
    }

    #[test]
    fn applies_the_same_resolved_style_to_a_native_element() {
        let sheet = Stylesheet::parse(
            ".card { width: 120px; background-color: #102030; border-radius: 8px; }",
        )
        .unwrap();
        let resolved = sheet.resolve_node(Node::new("view").with_classes("card"));
        let element = resolved.apply_to(div());
        assert_eq!(element.layout_style().size.width, Length::Px(px(120.0)));
    }

    #[test]
    fn parses_named_colors_for_native_and_react_paint() {
        let sheet = Stylesheet::parse(".accent { background-color: red; }").unwrap();
        let style = sheet.resolve_node(Node::new("view").with_classes("accent"));
        assert_eq!(style.background, Some(Color::RED));
    }

    #[test]
    fn ignores_browser_only_selectors_and_at_rules() {
        let sheet = Stylesheet::parse(
            "@keyframes spin { from { opacity: 0 } }
             @font-face { font-family: x }
             .x[data-role=knob] { width: 2px; }
             .x::before { width: 3px; }",
        )
        .unwrap();
        assert_eq!(sheet.rule_count(), 0);
    }

    #[test]
    fn malformed_declarations_are_reported() {
        assert!(matches!(
            Stylesheet::parse(".x { padding }"),
            Err(CssError::InvalidDeclaration { .. })
        ));
    }

    #[test]
    fn a_media_rule_is_kept_and_evaluated_instead_of_dropped() {
        let sheet = Stylesheet::parse("@media (min-width: 600px) { .x { width: 20px } }").unwrap();
        assert_eq!(sheet.rule_count(), 1);
        let node = Node::new("view").with_classes("x");
        let wide = StyleContext::default().with_viewport(Size::new(px(800.0), px(600.0)));
        let narrow = StyleContext::default().with_viewport(Size::new(px(320.0), px(600.0)));
        assert_eq!(
            sheet.resolve_in(&MatchPath::new(node), None, &wide).layout.size.width,
            Length::Px(px(20.0))
        );
        assert_eq!(
            sheet.resolve_in(&MatchPath::new(node), None, &narrow).layout.size.width,
            Length::Auto
        );
    }

    #[test]
    fn a_node_never_visits_a_rule_for_a_class_it_does_not_carry() {
        let sheet = Stylesheet::parse(
            ".a { width: 1px } .b { width: 2px } .c { width: 3px }
             #d { width: 4px } button { width: 5px } * { width: 6px }",
        )
        .unwrap();
        assert_eq!(sheet.rule_count(), 6);
        let node = Node::new("view").with_classes("b");
        // The universal rule plus `.b` only: four rules are never looked at.
        assert_eq!(sheet.candidate_rules(node).len(), 2);
        assert_eq!(sheet.candidate_rules(Node::new("button")).len(), 2);
    }

    #[test]
    fn buckets_do_not_change_which_rule_wins() {
        let sheet =
            Stylesheet::parse("* { width: 1px } .a { width: 2px } #b { width: 3px }").unwrap();
        let node = Node::new("view").with_id("b").with_classes("a");
        assert_eq!(sheet.resolve_node(node).layout.size.width, Length::Px(px(3.0)));
    }

    #[test]
    fn important_beats_a_heavier_selector() {
        let sheet = Stylesheet::parse("#a { width: 1px } .b { width: 2px !important }").unwrap();
        let node = Node::new("view").with_id("a").with_classes("b");
        assert_eq!(sheet.resolve_node(node).layout.size.width, Length::Px(px(2.0)));
    }

    #[test]
    fn important_beats_an_inline_declaration() {
        let sheet = Stylesheet::parse(".b { width: 2px !important }").unwrap();
        let node = Node::new("view").with_classes("b");
        assert_eq!(sheet.resolve(node, Some("width: 9px")).layout.size.width, Length::Px(px(2.0)));
    }

    #[test]
    fn the_later_of_two_equally_specific_rules_wins() {
        let sheet = Stylesheet::parse(".a { width: 1px } .b { width: 2px }").unwrap();
        let node = Node::new("view").with_classes("a b");
        assert_eq!(sheet.resolve_node(node).layout.size.width, Length::Px(px(2.0)));
    }

    #[test]
    fn a_selector_list_expands_into_one_rule_each() {
        let sheet = Stylesheet::parse(".a, .b, .c { width: 1px }").unwrap();
        assert_eq!(sheet.rule_count(), 3);
    }

    #[test]
    fn a_descendant_rule_needs_the_ancestor_chain() {
        let sheet = Stylesheet::parse(".rack button { width: 7px }").unwrap();
        let ancestors = [Node::new("view").with_classes("rack")];
        let flat = MatchPath::new(Node::new("button"));
        let nested = MatchPath::with_ancestors(&ancestors, Node::new("button"));
        let context = StyleContext::default();
        assert_eq!(sheet.resolve_in(&flat, None, &context).layout.size.width, Length::Auto);
        assert_eq!(
            sheet.resolve_in(&nested, None, &context).layout.size.width,
            Length::Px(px(7.0))
        );
    }

    #[test]
    fn a_pseudo_class_rule_only_applies_in_that_state() {
        let sheet = Stylesheet::parse("button { width: 1px } button:hover { width: 2px }").unwrap();
        let hovered = ElementState { hover: true, ..ElementState::default() };
        let idle = sheet.resolve_node(Node::new("button"));
        let hot = sheet.resolve_node(Node::new("button").with_state(hovered));
        assert_eq!(idle.layout.size.width, Length::Px(px(1.0)));
        assert_eq!(hot.layout.size.width, Length::Px(px(2.0)));
    }

    #[test]
    fn resolve_interactive_reports_only_the_states_the_author_styled() {
        let sheet = Stylesheet::parse(
            "button { background: #101010 } button:hover { background: #202020 }",
        )
        .unwrap();
        let styles = sheet.resolve_interactive(
            &MatchPath::new(Node::new("button")),
            None,
            &StyleContext::default(),
        );
        assert_eq!(styles.base.background, Some(Color::hex(0x101010)));
        assert_eq!(
            styles.hover.as_ref().and_then(|style| style.background),
            Some(Color::hex(0x202020))
        );
        assert!(styles.focus.is_none());
    }

    #[test]
    fn an_active_rule_still_sees_hover_because_a_press_is_also_a_hover() {
        let sheet =
            Stylesheet::parse("button:hover { background: #111111 } button:active { width: 2px }")
                .unwrap();
        let styles = sheet.resolve_interactive(
            &MatchPath::new(Node::new("button")),
            None,
            &StyleContext::default(),
        );
        let active = styles.active.expect("an :active rule was declared");
        assert_eq!(active.background, Some(Color::hex(0x111111)));
    }

    #[test]
    fn interactive_backgrounds_reach_the_paint_style() {
        let sheet = Stylesheet::parse(
            "button { background: #101010 }
             button:hover { background: #202020 }
             button:active { background: #303030 }
             button:focus { border-color: #4c9aff; border-width: 3px }",
        )
        .unwrap();
        let styles = sheet.resolve_interactive(
            &MatchPath::new(Node::new("button")),
            None,
            &StyleContext::default(),
        );
        let mut element = styles.apply_to(div());
        let paint = element.paint_style_mut();
        assert!(paint.hover_background.is_some());
        assert!(paint.active_background.is_some());
        assert_eq!(paint.focus_ring.map(|ring| ring.width), Some(px(3.0)));
    }

    #[test]
    fn a_custom_property_on_root_reaches_a_descendant() {
        let sheet = Stylesheet::parse(":root { --pad: 6px } .x { padding: var(--pad) }").unwrap();
        let ancestors = [Node::new("app")];
        let path = MatchPath::with_ancestors(&ancestors, Node::new("view").with_classes("x"));
        let style = sheet.resolve_in(&path, None, &StyleContext::default());
        assert_eq!(style.layout.padding.top, Length::Px(px(6.0)));
    }

    #[test]
    fn a_custom_property_cascades_like_any_other_declaration() {
        let sheet =
            Stylesheet::parse(":root { --pad: 2px } .x { --pad: 4px } .x { padding: var(--pad) }")
                .unwrap();
        let node = Node::new("view").with_classes("x");
        assert_eq!(sheet.resolve_node(node).layout.padding.top, Length::Px(px(4.0)));
    }

    #[test]
    fn a_var_fallback_is_used_when_nothing_defines_it() {
        let sheet = Stylesheet::parse(".x { padding: var(--pad, 5px) }").unwrap();
        let node = Node::new("view").with_classes("x");
        assert_eq!(sheet.resolve_node(node).layout.padding.top, Length::Px(px(5.0)));
    }

    #[test]
    fn an_unresolvable_var_drops_only_its_own_declaration() {
        let sheet = Stylesheet::parse(".x { padding: var(--pad); width: 3px }").unwrap();
        let node = Node::new("view").with_classes("x");
        let style = sheet.resolve_node(node);
        assert_eq!(style.layout.padding.top, Length::Px(Px::ZERO));
        assert_eq!(style.layout.size.width, Length::Px(px(3.0)));
    }

    #[test]
    fn a_var_cycle_does_not_hang_the_cascade() {
        let sheet =
            Stylesheet::parse(":root { --a: var(--b); --b: var(--a) } .x { width: var(--a) }")
                .unwrap();
        let node = Node::new("view").with_classes("x");
        assert_eq!(sheet.resolve_node(node).layout.size.width, Length::Auto);
    }

    #[test]
    fn a_dark_scheme_query_swaps_the_background() {
        let sheet = Stylesheet::parse(
            ".x { background: #ffffff }
             @media (prefers-color-scheme: dark) { .x { background: #000000 } }",
        )
        .unwrap();
        let node = MatchPath::new(Node::new("view").with_classes("x"));
        let dark = StyleContext::default().with_color_scheme(ColorScheme::Dark);
        assert_eq!(sheet.resolve_in(&node, None, &dark).background, Some(Color::BLACK));
        assert_eq!(
            sheet.resolve_in(&node, None, &StyleContext::default()).background,
            Some(Color::WHITE)
        );
    }

    #[test]
    fn em_lengths_follow_a_font_size_set_by_the_same_rule() {
        let sheet = Stylesheet::parse(".x { font-size: 20px; padding: 2em }").unwrap();
        let node = Node::new("view").with_classes("x");
        assert_eq!(sheet.resolve_node(node).layout.padding.top, Length::Px(px(40.0)));
    }

    #[test]
    fn a_font_size_in_em_measures_against_the_context_not_itself() {
        let sheet = Stylesheet::parse(".x { font-size: 2em }").unwrap();
        let context = StyleContext::default().with_font_size(px(10.0));
        let path = MatchPath::new(Node::new("view").with_classes("x"));
        assert_eq!(sheet.resolve_in(&path, None, &context).text.font_size, Some(px(20.0)));
    }

    #[test]
    fn text_properties_survive_the_cascade() {
        let sheet = Stylesheet::parse(
            ".x { color: #ff0000; font-weight: bold; text-align: center; white-space: nowrap }",
        )
        .unwrap();
        let style = sheet.resolve_node(Node::new("text").with_classes("x"));
        assert_eq!(style.text.color, Some(Color::RED));
        assert_eq!(style.text.font_weight, Some(spherekit_text::FontWeight::BOLD));
        assert_eq!(style.text.align, Some(spherekit_text::TextAlign::Center));
        assert!(style.text.no_wrap);
    }

    #[test]
    fn text_properties_reach_a_label_without_touching_its_layout() {
        let sheet = Stylesheet::parse(".x { font-size: 24px; width: 80px }").unwrap();
        let style = sheet.resolve_node(Node::new("text").with_classes("x"));
        let label = style.text.apply_to_label(style.apply_to(spherekit_ui::label("hi")));
        assert_eq!(label.layout_style().size.width, Length::Px(px(80.0)));
        assert_eq!(label.content(), "hi");
    }

    #[test]
    fn a_shadow_and_a_cursor_reach_the_paint_style() {
        let sheet =
            Stylesheet::parse(".x { box-shadow: 0 2px 4px #000000; cursor: pointer }").unwrap();
        let style = sheet.resolve_node(Node::new("view").with_classes("x"));
        let mut element = style.apply_to(div());
        assert_eq!(element.paint_style_mut().shadows.len(), 1);
        assert_eq!(element.paint_style_mut().cursor, Some(Cursor::Pointer));
    }

    #[test]
    fn an_empty_stylesheet_resolves_to_the_default_style() {
        let sheet = Stylesheet::parse("").unwrap();
        assert_eq!(sheet.rule_count(), 0);
        assert_eq!(sheet.resolve_node(Node::new("view")), ResolvedStyle::default());
    }

    #[test]
    fn a_missing_brace_is_reported_rather_than_guessed_at() {
        assert!(matches!(Stylesheet::parse(".x width: 1px"), Err(CssError::InvalidRule { .. })));
    }

    #[test]
    fn an_unknown_property_is_ignored_without_failing_the_parse() {
        let sheet =
            Stylesheet::parse(".x { -webkit-font-smoothing: antialiased; width: 4px }").unwrap();
        let node = Node::new("view").with_classes("x");
        assert_eq!(sheet.resolve_node(node).layout.size.width, Length::Px(px(4.0)));
    }

    #[test]
    fn an_unparsable_value_leaves_the_property_at_its_default() {
        let sheet = Stylesheet::parse(".x { width: banana; height: 3px }").unwrap();
        let node = Node::new("view").with_classes("x");
        let style = sheet.resolve_node(node);
        assert_eq!(style.layout.size.width, Length::Auto);
        assert_eq!(style.layout.size.height, Length::Px(px(3.0)));
    }
}

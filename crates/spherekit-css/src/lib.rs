//! # spherekit-css
//!
//! A small, native-first CSS runtime for SphereKit.
//!
//! The crate deliberately sits between a browser CSS engine and a collection of
//! ad-hoc style helpers. [`Stylesheet`] parses a useful CSS subset, applies
//! selector specificity and `!important`, and produces a [`ResolvedStyle`] that
//! can be used by a native element or by the Rust host behind
//! `spherekit-react`.
//!
//! `spherekit-css` uses Servo's `cssparser` for CSS tokenisation and typed value
//! parsing, while SphereKit keeps ownership of the supported property model.
//! This keeps the runtime small and makes unsupported browser-only features
//! explicit instead of silently giving them different native semantics.
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

#![deny(missing_docs)]
#![warn(clippy::doc_markdown)]

use cssparser::{Parser, ParserInput, Token};
use spherekit_core::{Color, Corners, Edges, Length, Px, Size, percent, px};
use spherekit_layout::{
    Align, Display, Distribute, FlexDirection, FlexWrap, Overflow, Position, Style, edges_all,
};
use spherekit_ui::Styled;
use std::cmp::Ordering;
use std::fmt;
use thiserror::Error;

/// A node identity used during selector matching.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Node<'a> {
    /// Native element name, such as `view`, `text` or `button`.
    pub element: &'a str,
    /// Optional author-facing id from the React/native props.
    pub id: Option<&'a str>,
    /// Whitespace-separated class names.
    pub classes: &'a str,
}

impl<'a> Node<'a> {
    /// Creates a node with an element name and no id or classes.
    pub const fn new(element: &'a str) -> Self {
        Self { element, id: None, classes: "" }
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

    fn has_class(self, class: &str) -> bool {
        self.classes.split_whitespace().any(|item| item == class)
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
}

impl Stylesheet {
    /// Parses a CSS stylesheet.
    ///
    /// Supported selectors are comma-separated simple selectors made from an
    /// optional element name, one id, and any number of classes, for example
    /// `view.card`, `#transport`, `.panel.primary` and `*`. Unsupported
    /// at-rules and pseudo selectors are ignored as rules rather than applied
    /// with surprising semantics.
    pub fn parse(source: &str) -> Result<Self, CssError> {
        let source = strip_comments(source);
        let mut rules = Vec::new();
        let mut cursor = 0;
        let bytes = source.as_bytes();

        while cursor < bytes.len() {
            skip_ascii_whitespace(bytes, &mut cursor);
            if cursor >= bytes.len() {
                break;
            }
            let Some(open) = find_top_level_byte(&source, cursor, b'{') else {
                return Err(CssError::InvalidRule {
                    offset: cursor,
                    message: "expected `{`".into(),
                });
            };
            let selector_text = source[cursor..open].trim();
            let Some(close) = find_matching_brace(&source, open) else {
                return Err(CssError::InvalidRule {
                    offset: open,
                    message: "unclosed declaration block".into(),
                });
            };
            if !selector_text.starts_with('@') {
                let declarations = parse_declarations(&source[open + 1..close])?;
                for selector in selector_text.split(',') {
                    let selector = parse_selector(selector.trim());
                    if let Some(selector) = selector {
                        rules.push(Rule {
                            selector,
                            declarations: declarations.clone(),
                            order: rules.len(),
                        });
                    }
                }
            }
            cursor = close + 1;
        }

        Ok(Self { rules })
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
        let mut resolved = ResolvedStyle::default();
        let mut winners: Vec<Winner> = Vec::new();

        for rule in &self.rules {
            if !rule.selector.matches(node) {
                continue;
            }
            let specificity = rule.selector.specificity();
            for declaration in &rule.declarations {
                consider(&mut winners, declaration, specificity, rule.order);
            }
        }

        if let Some(inline_css) = inline_css {
            if let Ok(declarations) = parse_declarations(inline_css) {
                for declaration in declarations {
                    consider(&mut winners, &declaration, Specificity::INLINE, usize::MAX);
                }
            }
        }

        winners.sort_by(|a, b| a.property.cmp(&b.property));
        for winner in winners {
            apply_declaration(&mut resolved, &winner.property, &winner.value);
        }
        resolved
    }

    /// Resolves a node with no inline declarations.
    pub fn resolve_node(&self, node: Node<'_>) -> ResolvedStyle {
        self.resolve(node, None)
    }
}

/// The computed subset of CSS understood by SphereKit.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStyle {
    /// Layout style consumed by `spherekit-layout` and Taffy.
    pub layout: Style,
    /// Optional CSS background fill.
    pub background: Option<Color>,
    /// Border colour. It is meaningful when [`Self::border_width`] is nonzero.
    pub border_color: Option<Color>,
    /// Uniform border width.
    pub border_width: Option<Px>,
    /// Uniform corner radius.
    pub corner_radius: Option<Px>,
    /// Optional paint opacity.
    pub paint_opacity: Option<f32>,
    /// Whether the native painter should clip descendants to this box.
    pub clip_content: bool,
}

impl Default for ResolvedStyle {
    fn default() -> Self {
        Self {
            layout: Style::DEFAULT,
            background: None,
            border_color: None,
            border_width: None,
            corner_radius: None,
            paint_opacity: None,
            clip_content: false,
        }
    }
}

impl ResolvedStyle {
    /// Applies this computed style to any native SphereKit element.
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
        if let Some(radius) = self.corner_radius {
            element.paint_style_mut().corner_radii = Corners::all(radius);
        }
        if let Some(opacity) = self.paint_opacity {
            element.paint_style_mut().opacity = opacity;
        }
        if self.clip_content {
            element.paint_style_mut().clip_content = true;
        }
        element
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Rule {
    selector: Selector,
    declarations: Vec<Declaration>,
    order: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct Declaration {
    property: String,
    value: String,
    important: bool,
    order: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Specificity {
    ids: u16,
    classes: u16,
    elements: u16,
}

impl Specificity {
    const INLINE: Self = Self { ids: 1000, classes: 0, elements: 0 };
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

#[derive(Clone, Debug, PartialEq)]
struct Selector {
    element: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

impl Selector {
    fn matches(&self, node: Node<'_>) -> bool {
        self.element
            .as_deref()
            .is_none_or(|element| element == "*" || element.eq_ignore_ascii_case(node.element))
            && self.id.as_deref().is_none_or(|id| node.id == Some(id))
            && self.classes.iter().all(|class| node.has_class(class))
    }

    fn specificity(&self) -> Specificity {
        Specificity {
            ids: u16::from(self.id.is_some()),
            classes: self.classes.len() as u16,
            elements: u16::from(self.element.as_deref().is_some_and(|element| element != "*")),
        }
    }
}

#[derive(Clone, Debug)]
struct Winner {
    property: String,
    value: String,
    important: bool,
    specificity: Specificity,
    order: usize,
}

fn consider(
    winners: &mut Vec<Winner>,
    declaration: &Declaration,
    specificity: Specificity,
    order: usize,
) {
    let candidate = Winner {
        property: declaration.property.clone(),
        value: declaration.value.clone(),
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

fn parse_selector(text: &str) -> Option<Selector> {
    if text.is_empty()
        || text.contains(':')
        || text.contains('[')
        || text.contains(' ')
        || text.contains('>')
    {
        return None;
    }
    let mut selector = Selector { element: None, id: None, classes: Vec::new() };
    let mut cursor = 0;
    let chars: Vec<char> = text.chars().collect();
    if chars.first().is_some_and(|c| c.is_ascii_alphabetic() || *c == '*') {
        let start = cursor;
        cursor += 1;
        while cursor < chars.len()
            && (chars[cursor].is_ascii_alphanumeric() || matches!(chars[cursor], '-' | '_'))
        {
            cursor += 1;
        }
        selector.element = Some(chars[start..cursor].iter().collect());
    }
    while cursor < chars.len() {
        let marker = chars[cursor];
        if marker != '#' && marker != '.' {
            return None;
        }
        cursor += 1;
        let start = cursor;
        while cursor < chars.len()
            && (chars[cursor].is_ascii_alphanumeric() || matches!(chars[cursor], '-' | '_'))
        {
            cursor += 1;
        }
        if start == cursor {
            return None;
        }
        let name = chars[start..cursor].iter().collect::<String>();
        if marker == '#' {
            if selector.id.replace(name).is_some() {
                return None;
            }
        } else {
            selector.classes.push(name);
        }
    }
    Some(selector)
}

fn parse_declarations(block: &str) -> Result<Vec<Declaration>, CssError> {
    let mut declarations = Vec::new();
    for (order, raw) in split_top_level(block, ';').into_iter().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Some(colon) = raw.find(':') else {
            return Err(CssError::InvalidDeclaration { declaration: raw.into() });
        };
        let property = normalize_property(raw[..colon].trim());
        if property.is_empty() {
            return Err(CssError::InvalidDeclaration { declaration: raw.into() });
        }
        let mut value = raw[colon + 1..].trim().to_string();
        let important = value.ends_with("!important");
        if important {
            let trimmed = value.strip_suffix("!important").unwrap_or_default();
            value = trimmed.trim_end().to_string();
        }
        declarations.push(Declaration { property, value, important, order });
    }
    Ok(declarations)
}

fn normalize_property(property: &str) -> String {
    let mut result = String::with_capacity(property.len());
    for character in property.trim().chars() {
        if character.is_ascii_uppercase() {
            result.push('-');
            result.push(character.to_ascii_lowercase());
        } else {
            result.push(character.to_ascii_lowercase());
        }
    }
    result
}

fn apply_declaration(style: &mut ResolvedStyle, property: &str, value: &str) {
    match property {
        "display" => {
            if let Some(display) = parse_display(value) {
                style.layout.display = display
            }
        }
        "position" => {
            if let Some(position) = parse_position(value) {
                style.layout.position = position
            }
        }
        "flex-direction" => {
            if let Some(direction) = parse_flex_direction(value) {
                style.layout.flex_direction = direction
            }
        }
        "flex-wrap" => {
            if let Some(wrap) = parse_flex_wrap(value) {
                style.layout.flex_wrap = wrap
            }
        }
        "flex-grow" => {
            if let Some(number) = parse_number(value) {
                style.layout.flex_grow = number.max(0.0)
            }
        }
        "flex-shrink" => {
            if let Some(number) = parse_number(value) {
                style.layout.flex_shrink = number.max(0.0)
            }
        }
        "flex-basis" => {
            if let Some(length) = parse_length(value) {
                style.layout.flex_basis = length
            }
        }
        "width" => {
            if let Some(length) = parse_length(value) {
                style.layout.size.width = length
            }
        }
        "height" => {
            if let Some(length) = parse_length(value) {
                style.layout.size.height = length
            }
        }
        "min-width" => {
            if let Some(length) = parse_length(value) {
                style.layout.min_size.width = length
            }
        }
        "min-height" => {
            if let Some(length) = parse_length(value) {
                style.layout.min_size.height = length
            }
        }
        "max-width" => {
            if let Some(length) = parse_length(value) {
                style.layout.max_size.width = length
            }
        }
        "max-height" => {
            if let Some(length) = parse_length(value) {
                style.layout.max_size.height = length
            }
        }
        "padding" => {
            if let Some(edges) = parse_edges(value) {
                style.layout.padding = edges
            }
        }
        "padding-top" => set_edge(&mut style.layout.padding.top, value),
        "padding-right" => set_edge(&mut style.layout.padding.right, value),
        "padding-bottom" => set_edge(&mut style.layout.padding.bottom, value),
        "padding-left" => set_edge(&mut style.layout.padding.left, value),
        "margin" => {
            if let Some(edges) = parse_edges(value) {
                style.layout.margin = edges
            }
        }
        "margin-top" => set_edge(&mut style.layout.margin.top, value),
        "margin-right" => set_edge(&mut style.layout.margin.right, value),
        "margin-bottom" => set_edge(&mut style.layout.margin.bottom, value),
        "margin-left" => set_edge(&mut style.layout.margin.left, value),
        "border-width" => {
            if let Some(edges) = parse_edges(value) {
                style.layout.border = edges;
                style.border_width = uniform_edge(edges)
            }
        }
        "border-top-width" => set_edge(&mut style.layout.border.top, value),
        "border-right-width" => set_edge(&mut style.layout.border.right, value),
        "border-bottom-width" => set_edge(&mut style.layout.border.bottom, value),
        "border-left-width" => set_edge(&mut style.layout.border.left, value),
        "gap" => {
            if let Some((column, row)) = parse_gap(value) {
                style.layout.gap = Size::new(column, row)
            }
        }
        "column-gap" => {
            if let Some(column) = parse_px(value) {
                style.layout.gap.width = column
            }
        }
        "row-gap" => {
            if let Some(row) = parse_px(value) {
                style.layout.gap.height = row
            }
        }
        "inset" => {
            if let Some(edges) = parse_edges(value) {
                style.layout.inset = edges
            }
        }
        "top" => set_edge(&mut style.layout.inset.top, value),
        "right" => set_edge(&mut style.layout.inset.right, value),
        "bottom" => set_edge(&mut style.layout.inset.bottom, value),
        "left" => set_edge(&mut style.layout.inset.left, value),
        "align-items" => {
            if let Some(align) = parse_align(value) {
                style.layout.align_items = Some(align)
            }
        }
        "align-self" => {
            if let Some(align) = parse_align(value) {
                style.layout.align_self = Some(align)
            }
        }
        "align-content" => {
            if let Some(distribute) = parse_distribute(value) {
                style.layout.align_content = Some(distribute)
            }
        }
        "justify-content" => {
            if let Some(distribute) = parse_distribute(value) {
                style.layout.justify_content = Some(distribute)
            }
        }
        "overflow" => {
            if let Some(overflow) = parse_overflow(value) {
                style.layout.overflow_x = overflow;
                style.layout.overflow_y = overflow;
                style.clip_content = overflow != Overflow::Visible
            }
        }
        "overflow-x" => {
            if let Some(overflow) = parse_overflow(value) {
                style.layout.overflow_x = overflow;
                style.clip_content |= overflow != Overflow::Visible
            }
        }
        "overflow-y" => {
            if let Some(overflow) = parse_overflow(value) {
                style.layout.overflow_y = overflow;
                style.clip_content |= overflow != Overflow::Visible
            }
        }
        "aspect-ratio" => {
            if let Some(number) = parse_number(value) {
                style.layout.aspect_ratio = (number > 0.0).then_some(number)
            }
        }
        "z-index" => {
            if let Some(number) = parse_number(value) {
                style.layout.z_index = number as i32
            }
        }
        "opacity" => {
            if let Some(number) = parse_number(value) {
                style.layout.opacity = number.clamp(0.0, 1.0);
                style.paint_opacity = Some(style.layout.opacity)
            }
        }
        "background" | "background-color" => {
            if let Some(color) = parse_color(value) {
                style.background = Some(color)
            }
        }
        "border" => apply_border_shorthand(style, value),
        "border-color" => {
            if let Some(color) = parse_color(value) {
                style.border_color = Some(color)
            }
        }
        "border-radius" => {
            if let Some(radius) = parse_px(value) {
                style.corner_radius = Some(radius)
            }
        }
        _ => {}
    }
}

fn apply_border_shorthand(style: &mut ResolvedStyle, value: &str) {
    let mut width = None;
    let mut color = None;
    for token in split_whitespace(value) {
        if width.is_none() {
            width = parse_length(&token).and_then(|length| match length {
                Length::Px(value) => Some(value),
                _ => None,
            });
        }
        if color.is_none() {
            color = parse_color(&token);
        }
    }
    if let Some(width) = width {
        style.layout.border = edges_all(Length::Px(width));
        style.border_width = Some(width);
    }
    if let Some(color) = color {
        style.border_color = Some(color);
    }
}

fn set_edge(edge: &mut Length, value: &str) {
    if let Some(length) = parse_length(value) {
        *edge = length;
    }
}

fn parse_display(value: &str) -> Option<Display> {
    match value.trim().to_ascii_lowercase().as_str() {
        "flex" => Some(Display::Flex),
        "block" => Some(Display::Block),
        "grid" => Some(Display::Grid),
        "none" => Some(Display::None),
        _ => None,
    }
}

fn parse_position(value: &str) -> Option<Position> {
    match value.trim().to_ascii_lowercase().as_str() {
        "relative" | "static" => Some(Position::Relative),
        "absolute" => Some(Position::Absolute),
        _ => None,
    }
}

fn parse_flex_direction(value: &str) -> Option<FlexDirection> {
    match value.trim().to_ascii_lowercase().as_str() {
        "row" => Some(FlexDirection::Row),
        "column" => Some(FlexDirection::Column),
        "row-reverse" => Some(FlexDirection::RowReverse),
        "column-reverse" => Some(FlexDirection::ColumnReverse),
        _ => None,
    }
}

fn parse_flex_wrap(value: &str) -> Option<FlexWrap> {
    match value.trim().to_ascii_lowercase().as_str() {
        "nowrap" => Some(FlexWrap::NoWrap),
        "wrap" => Some(FlexWrap::Wrap),
        "wrap-reverse" => Some(FlexWrap::WrapReverse),
        _ => None,
    }
}

fn parse_align(value: &str) -> Option<Align> {
    match value.trim().to_ascii_lowercase().as_str() {
        "start" | "flex-start" => Some(Align::Start),
        "end" | "flex-end" => Some(Align::End),
        "center" => Some(Align::Center),
        "stretch" => Some(Align::Stretch),
        "baseline" => Some(Align::Baseline),
        _ => None,
    }
}

fn parse_distribute(value: &str) -> Option<Distribute> {
    match value.trim().to_ascii_lowercase().as_str() {
        "start" | "flex-start" => Some(Distribute::Start),
        "end" | "flex-end" => Some(Distribute::End),
        "center" => Some(Distribute::Center),
        "stretch" => Some(Distribute::Stretch),
        "space-between" => Some(Distribute::SpaceBetween),
        "space-evenly" => Some(Distribute::SpaceEvenly),
        "space-around" => Some(Distribute::SpaceAround),
        _ => None,
    }
}

fn parse_overflow(value: &str) -> Option<Overflow> {
    match value.trim().to_ascii_lowercase().as_str() {
        "visible" => Some(Overflow::Visible),
        "hidden" => Some(Overflow::Hidden),
        "scroll" | "auto" => Some(Overflow::Scroll),
        _ => None,
    }
}

fn parse_number(value: &str) -> Option<f32> {
    let mut input = ParserInput::new(value.trim());
    let mut parser = Parser::new(&mut input);
    match parser.next().ok()? {
        Token::Number { value, .. } => Some(*value),
        _ => None,
    }
}

fn parse_length(value: &str) -> Option<Length> {
    let mut input = ParserInput::new(value.trim());
    let mut parser = Parser::new(&mut input);
    match parser.next().ok()? {
        Token::Ident(ident) if ident.eq_ignore_ascii_case("auto") => Some(Length::Auto),
        Token::Number { value, .. } if *value == 0.0 => Some(Length::Px(Px::ZERO)),
        Token::Dimension { value, unit, .. } if unit.eq_ignore_ascii_case("px") => {
            Some(Length::Px(px(*value)))
        }
        Token::Percentage { unit_value, .. } => Some(percent(*unit_value)),
        _ => None,
    }
}

fn parse_px(value: &str) -> Option<Px> {
    match parse_length(value)? {
        Length::Px(value) => Some(value),
        _ => None,
    }
}

fn parse_edges(value: &str) -> Option<Edges<Length>> {
    let values = split_whitespace(value);
    let lengths = values.iter().map(|item| parse_length(item)).collect::<Option<Vec<_>>>()?;
    match lengths.as_slice() {
        [one] => Some(edges_all(*one)),
        [vertical, horizontal] => {
            Some(Edges { top: *vertical, right: *horizontal, bottom: *vertical, left: *horizontal })
        }
        [top, horizontal, bottom] => {
            Some(Edges { top: *top, right: *horizontal, bottom: *bottom, left: *horizontal })
        }
        [top, right, bottom, left] => {
            Some(Edges { top: *top, right: *right, bottom: *bottom, left: *left })
        }
        _ => None,
    }
}

fn uniform_edge(edges: Edges<Length>) -> Option<Px> {
    match (edges.top, edges.right, edges.bottom, edges.left) {
        (Length::Px(top), Length::Px(right), Length::Px(bottom), Length::Px(left))
            if top == right && right == bottom && bottom == left =>
        {
            Some(top)
        }
        _ => None,
    }
}

fn parse_gap(value: &str) -> Option<(Px, Px)> {
    let values = split_whitespace(value);
    match values.as_slice() {
        [one] => Some((parse_px(one)?, parse_px(one)?)),
        [row, column] => Some((parse_px(column)?, parse_px(row)?)),
        _ => None,
    }
}

fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let mut expanded = String::with_capacity(6);
                for character in hex.chars() {
                    expanded.push(character);
                    expanded.push(character);
                }
                u32::from_str_radix(&expanded, 16).ok().map(Color::hex)
            }
            6 => u32::from_str_radix(hex, 16).ok().map(Color::hex),
            8 => u32::from_str_radix(hex, 16).ok().map(Color::hex_rgba),
            _ => None,
        };
    }
    match value.to_ascii_lowercase().as_str() {
        "transparent" => Some(Color::TRANSPARENT),
        "black" => Some(Color::BLACK),
        "white" => Some(Color::WHITE),
        "red" => Some(Color::RED),
        "green" => Some(Color::GREEN),
        "blue" => Some(Color::BLUE),
        _ => None,
    }
}

fn split_whitespace(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            character if character.is_ascii_whitespace() && depth == 0 => {
                if start < index {
                    values.push(value[start..index].to_string());
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if start < value.len() {
        values.push(value[start..].to_string());
    }
    values
}

fn split_top_level(value: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            character if character == separator && depth == 0 => {
                parts.push(&value[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn strip_comments(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(character) = chars.next() {
                if character == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
                if character == '\n' {
                    result.push('\n');
                }
            }
        } else {
            result.push(character);
        }
    }
    result
}

fn skip_ascii_whitespace(bytes: &[u8], cursor: &mut usize) {
    while *cursor < bytes.len() && bytes[*cursor].is_ascii_whitespace() {
        *cursor += 1;
    }
}

fn find_top_level_byte(source: &str, start: usize, wanted: u8) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    for (offset, byte) in bytes.iter().enumerate().skip(start) {
        if let Some(current_quote) = quote {
            if *byte == current_quote && (offset == 0 || bytes[offset - 1] != b'\\') {
                quote = None;
            }
            continue;
        }
        match *byte {
            b'"' | b'\'' => quote = Some(*byte),
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            byte if byte == wanted && depth == 0 => return Some(offset),
            _ => {}
        }
    }
    None
}

fn find_matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        if let Some(current_quote) = quote {
            if *byte == current_quote && (offset == 0 || bytes[offset - 1] != b'\\') {
                quote = None;
            }
            continue;
        }
        match *byte {
            b'"' | b'\'' => quote = Some(*byte),
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

impl fmt::Display for Node<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.element)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            "@media (min-width: 1px) { .x { width: 1px; } } .x:hover { width: 2px; }",
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
}

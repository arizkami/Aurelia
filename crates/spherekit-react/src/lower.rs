//! Lowering a committed React node into a native SphereKit element.
//!
//! The walk does three things at once, and they are entangled on purpose:
//! it resolves the cascade, it threads the ancestor chain that descendant and
//! child combinators need, and it inherits typography down the tree. Doing them
//! in separate passes would mean building the same ancestor chain three times.
//!
//! ## Event names
//!
//! The names pushed onto the [`EventQueue`] are the renderer's, not React's:
//! the TypeScript dispatcher capitalises and prefixes them (`press` becomes
//! `onPress`) when it looks the prop up. Sending `onPress` from here would put
//! React's naming convention in the native host, where a second frontend would
//! then have to reproduce it.

use crate::events::{EventQueue, HostEvent};
use crate::host::ReactHost;
use crate::tree::NativeNode;
use serde_json::json;
use spherekit_core::px;
use spherekit_css::{
    ElementState, InteractiveStyle, MatchPath, Node as CssNode, ResolvedStyle, StyleContext,
    Stylesheet, TextProperties,
};
use spherekit_layout::Style;
use spherekit_ui::{
    AnyElement, IntoElement, Label, ParentElement, Presence, Styled, TextEdit, Toggle,
    ValueControl, avatar, button, checkbox, div, fader, knob, label, menu_item, panel, progress,
    progress_indeterminate, scroll_view, separator, slider, text_field, toggle,
};

/// A completed button press. The renderer maps it to `onPress`.
const PRESS: &str = "press";
/// A range control moved. The renderer maps it to `onValueChange`.
const VALUE_CHANGE: &str = "valueChange";
/// A toggle, checkbox or field changed. The renderer maps it to `onChange`.
const CHANGE: &str = "change";
/// A menu row was chosen. The renderer maps it to `onSelect`.
const SELECT: &str = "select";
/// A field was committed with Enter. The renderer maps it to `onSubmit`.
const SUBMIT: &str = "submit";

/// Builds one native element for a host node.
///
/// The context carries everything about *where* the node sits — its resolved
/// style, its inherited typography, the ancestor chain and the event queue —
/// so a builder only has to read the node's own props.
pub type NodeBuilder = Box<dyn Fn(&LowerContext<'_>, &NativeNode) -> AnyElement>;

/// Everything a [`NodeBuilder`] needs about the node's surroundings.
pub struct LowerContext<'a> {
    frame: Frame<'a>,
    subject: CssNode<'a>,
    style: InteractiveStyle,
    text: TextProperties,
    hidden: bool,
}

/// The part of the walk that is the same for every node at one level.
#[derive(Clone, Copy)]
struct Frame<'a> {
    host: &'a ReactHost,
    events: Option<&'a EventQueue>,
    /// The subject's ancestors, outermost first.
    ancestors: &'a [CssNode<'a>],
    /// Typography the parent resolved, which this node inherits what it does
    /// not state itself.
    inherited: &'a TextProperties,
}

impl<'a> LowerContext<'a> {
    /// The node's resolved style, including the interaction variants.
    pub fn style(&self) -> &InteractiveStyle {
        &self.style
    }

    /// The node's typography, with ancestor inheritance already folded in.
    pub fn text(&self) -> &TextProperties {
        &self.text
    }

    /// The queue native callbacks push into, absent when events are not wanted.
    pub fn events(&self) -> Option<&'a EventQueue> {
        self.frame.events
    }

    /// The stylesheet the whole commit is resolved through.
    pub fn stylesheet(&self) -> &'a Stylesheet {
        self.frame.host.stylesheet()
    }

    /// The context that font-relative and viewport-relative units resolve in.
    pub fn style_context(&self) -> &'a StyleContext {
        self.frame.host.style_context()
    }

    /// Applies the resolved style, and React's `hidden` flag, to an element.
    pub fn apply<T: Styled>(&self, mut element: T) -> T {
        let widget = element.style_mut().clone();
        let mut element = self.style.apply_to(element);
        restore_widget_defaults(element.style_mut(), &widget);
        if self.hidden {
            element = element.hidden();
        }
        element
    }

    /// Applies the resolved style and the inherited typography to a label.
    pub fn apply_label(&self, label: Label) -> Label {
        self.text.apply_to_label(self.apply(label))
    }

    /// Puts a widget that cannot carry a style of its own inside a styled box.
    ///
    /// `Toggle`, `MenuItem` and `Avatar` compute their whole layout style from
    /// their own state and do not implement [`Styled`], so there is nowhere to
    /// put a CSS box on them. Wrapping keeps the cascade working for those
    /// types at the cost of one extra element; the alternative — silently
    /// dropping the style — would make `.toggle { margin: 8px }` do nothing and
    /// say nothing.
    pub fn wrap(&self, inner: impl IntoElement) -> AnyElement {
        self.apply(div()).child(inner).into_element()
    }

    /// Lowers a node's children, with that node pushed onto the ancestor chain.
    pub fn children(&self, node: &NativeNode) -> Vec<AnyElement> {
        let mut chain = self.frame.ancestors.to_vec();
        chain.push(self.subject);
        let frame = Frame {
            host: self.frame.host,
            events: self.frame.events,
            ancestors: &chain,
            inherited: &self.text,
        };
        let siblings = node.children.len();
        node.children
            .iter()
            .enumerate()
            .map(|(index, child)| frame.lower(child, index, siblings))
            .collect()
    }
}

impl<'a> Frame<'a> {
    /// Resolves one node and hands it to whichever builder claims its type.
    fn lower(&self, node: &'a NativeNode, index: usize, siblings: usize) -> AnyElement {
        let subject = css_node(node, index, siblings);
        let inline = inline_css(node);
        let path = MatchPath::with_ancestors(self.ancestors, subject);
        let sheet = self.host.stylesheet();
        // An application with no stylesheet at all is the common case for a
        // host embedded in native code, and `resolve_interactive` runs the
        // cascade four times. Skipping it when there is provably nothing to
        // cascade is behaviour-identical and turns four walks into none.
        let style = if sheet.rule_count() == 0 && inline.is_none() {
            InteractiveStyle {
                base: ResolvedStyle::default(),
                hover: None,
                active: None,
                focus: None,
            }
        } else {
            sheet.resolve_interactive(&path, inline.as_deref(), self.host.style_context())
        };

        let mut text = style.base.text.clone();
        text.inherit_from(self.inherited);
        let context = LowerContext { frame: *self, subject, style, text, hidden: node.hidden };

        match self.host.builder(&node.node_type) {
            Some(builder) => builder(&context, node),
            None => built_in(&context, node),
        }
    }
}

/// Lowers the whole retained tree under one flex column.
///
/// The root container has no React node behind it, so it is not styled and does
/// not join the ancestor chain: a top-level `<View>` matches `:root`, which is
/// what an author writing `:root { … }` in the application stylesheet means.
pub(crate) fn lower_tree(host: &ReactHost, events: Option<&EventQueue>) -> AnyElement {
    let inherited = TextProperties::default();
    let frame = Frame { host, events, ancestors: &[], inherited: &inherited };
    let roots = &host.tree().children;
    let siblings = roots.len();
    div()
        .flex_col()
        .children_iter(
            roots.iter().enumerate().map(|(index, node)| frame.lower(node, index, siblings)),
        )
        .into_element()
}

/// The host types the crate knows without an application registering anything.
///
/// An unknown type becomes a plain container rather than an error: a React
/// bundle built against a newer host must degrade to a box with its children
/// in it, not take the window down.
fn built_in(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    match node.node_type.as_str() {
        "#text" | "text" => text_element(cx, node),
        "button" => button_element(cx, node),
        "slider" => range_element(cx, node, slider),
        "knob" => range_element(cx, node, knob),
        "fader" => range_element(cx, node, fader),
        "toggle" => toggle_element(cx, node, toggle),
        "checkbox" => toggle_element(cx, node, checkbox),
        "progress" => progress_element(cx, node),
        "separator" => separator_element(cx, node),
        "panel" => panel_element(cx, node),
        "scroll-view" => scroll_element(cx, node),
        "text-field" => field_element(cx, node),
        "avatar" => avatar_element(cx, node),
        "menu-item" => menu_element(cx, node),
        _ => container(cx, node),
    }
}

fn container(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    cx.apply(div().id(node.id)).children_iter(cx.children(node)).into_element()
}

/// A `text` node collapses its subtree into one string rather than laying its
/// `#text` children out, because a label paints its own glyphs and a child
/// element beside them would be positioned by flexbox instead of by the shaper.
fn text_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    cx.apply_label(label(node.text_content()).id(node.id)).into_element()
}

fn button_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    let title =
        node.prop_str("title").map(ToOwned::to_owned).unwrap_or_else(|| node.text_content());
    let mut widget =
        button(title).id(node.id).disabled(node.prop_bool("disabled").unwrap_or(false));

    // A button paints its label itself and is not a `Label`, so only the three
    // properties it can carry are forwarded. Colour and alignment come from the
    // theme's button tokens, which is what keeps a themed button readable when
    // an author sets `color` on a container three levels up.
    let text = cx.text();
    if let Some(size) = text.font_size {
        widget = widget.text_size(size);
    }
    if let Some(weight) = text.font_weight {
        widget = widget.weight(weight);
    }
    if let Some(families) = text.font_family.as_ref() {
        widget = widget.font(families.clone());
    }

    if let Some(queue) = cx.events() {
        let queue = queue.clone();
        let id = node.id;
        widget = widget.on_press(move || queue.push(HostEvent::new(PRESS, id)));
    }
    cx.apply(widget).into_element()
}

fn range_element(
    cx: &LowerContext<'_>,
    node: &NativeNode,
    make: fn(f32) -> ValueControl,
) -> AnyElement {
    let value = node.prop_f32("value").unwrap_or(0.0);
    let min = node.prop_f32("minimumValue").unwrap_or(0.0);
    let max = node.prop_f32("maximumValue").unwrap_or(1.0);
    let mut control = make(value).id(node.id).range(min, max);
    if let Some(step) = node.prop_f32("step") {
        control = control.step(step);
    }
    if node.prop_bool("disabled").unwrap_or(false) {
        control = control.disabled(true);
    }
    if let Some(name) = node.prop_str("name") {
        control = control.name(name);
    }
    if let Some(queue) = cx.events() {
        let queue = queue.clone();
        let id = node.id;
        control = control.on_change(move |value| {
            queue.push(HostEvent::with_payload(VALUE_CHANGE, id, json!({ "value": value })));
        });
    }
    cx.apply(control).into_element()
}

fn toggle_element(
    cx: &LowerContext<'_>,
    node: &NativeNode,
    make: fn(bool) -> Toggle,
) -> AnyElement {
    let mut widget = make(node.prop_bool("checked").unwrap_or(false))
        .id(node.id)
        .disabled(node.prop_bool("disabled").unwrap_or(false));
    let text = node.prop_str("label").map(ToOwned::to_owned).unwrap_or_else(|| node.text_content());
    if !text.is_empty() {
        widget = widget.label(text);
    }
    if let Some(queue) = cx.events() {
        let queue = queue.clone();
        let id = node.id;
        widget = widget.on_change(move |checked| {
            queue.push(HostEvent::with_payload(CHANGE, id, json!({ "checked": checked })));
        });
    }
    cx.wrap(widget)
}

fn progress_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    let mut bar = if node.prop_bool("indeterminate").unwrap_or(false) {
        progress_indeterminate()
    } else {
        progress(node.prop_f32("value").unwrap_or(0.0))
    };
    bar = bar.id(node.id);
    if let Some(thickness) = node.prop_f32("thickness") {
        bar = bar.thickness(px(thickness));
    }
    cx.apply(bar).into_element()
}

fn separator_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    let rule = separator(node.prop_bool("vertical").unwrap_or(false)).id(node.id);
    cx.apply(rule).into_element()
}

fn panel_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    let frame = panel(node.prop_str("title").unwrap_or_default()).id(node.id);
    cx.apply(frame).children_iter(cx.children(node)).into_element()
}

fn scroll_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    let view = scroll_view()
        .id(node.id)
        .horizontal(node.prop_bool("horizontal").unwrap_or(false))
        .both_axes(node.prop_bool("both").unwrap_or(false));
    cx.apply(view).children_iter(cx.children(node)).into_element()
}

fn field_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    let mut field = text_field(TextEdit::from_text(node.prop_str("value").unwrap_or_default()))
        .id(node.id)
        .disabled(node.prop_bool("disabled").unwrap_or(false))
        .mask(node.prop_bool("mask").unwrap_or(false));
    if let Some(placeholder) = node.prop_str("placeholder") {
        field = field.placeholder(placeholder);
    }
    if let Some(queue) = cx.events() {
        let id = node.id;
        let changes = queue.clone();
        field = field.on_change(move |edit| {
            changes.push(HostEvent::with_payload(CHANGE, id, json!({ "text": edit.text() })));
        });
        let submits = queue.clone();
        field = field.on_submit(move |text| {
            submits.push(HostEvent::with_payload(SUBMIT, id, json!({ "text": text })));
        });
    }
    cx.apply(field).into_element()
}

fn avatar_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    let mut portrait = avatar(node.prop_str("name").unwrap_or_default()).id(node.id);
    if let Some(initials) = node.prop_str("initials") {
        portrait = portrait.initials(initials);
    }
    if let Some(diameter) = node.prop_f32("size") {
        portrait = portrait.size(px(diameter));
    }
    if let Some(presence) = node.prop_str("presence").and_then(presence_of) {
        portrait = portrait.presence(presence);
    }
    cx.wrap(portrait)
}

fn menu_element(cx: &LowerContext<'_>, node: &NativeNode) -> AnyElement {
    let text = node.prop_str("label").map(ToOwned::to_owned).unwrap_or_else(|| node.text_content());
    let mut item = menu_item(text)
        .id(node.id)
        .danger(node.prop_bool("danger").unwrap_or(false))
        .disabled(node.prop_bool("disabled").unwrap_or(false));
    if let Some(shortcut) = node.prop_str("shortcut") {
        item = item.shortcut(shortcut);
    }
    if let Some(queue) = cx.events() {
        let queue = queue.clone();
        let id = node.id;
        item = item.on_select(move || queue.push(HostEvent::new(SELECT, id)));
    }
    cx.wrap(item)
}

/// An unrecognised presence draws no dot, rather than defaulting to offline:
/// "we do not know" and "signed out" are different things to show a user.
fn presence_of(value: &str) -> Option<Presence> {
    match value {
        "online" => Some(Presence::Online),
        "away" => Some(Presence::Away),
        "busy" => Some(Presence::Busy),
        "offline" => Some(Presence::Offline),
        _ => None,
    }
}

/// The selector-matching view of a host node.
///
/// `#text` is passed through as the element name even though no selector can
/// name it — a leading `#` parses as an id — which is the correct outcome: a
/// text node is not an element, so only inherited properties and the universal
/// selector should reach it.
fn css_node(node: &NativeNode, index: usize, siblings: usize) -> CssNode<'_> {
    let classes = node
        .props
        .get("className")
        .or_else(|| node.props.get("class"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    CssNode::new(&node.node_type)
        .with_id_option(node.props.get("id").and_then(serde_json::Value::as_str))
        .with_classes(classes)
        .with_state(element_state(node))
        .with_index(index, siblings)
}

/// The interaction flags that come from props rather than from the pointer.
///
/// `hover`, `active` and `focus` are deliberately left false here and resolved
/// as variants instead: they change many times a second and are the widget's
/// business, whereas `disabled` and `checked` are React state that only changes
/// on a commit.
fn element_state(node: &NativeNode) -> ElementState {
    ElementState {
        hover: false,
        active: false,
        focus: false,
        disabled: node.prop_bool("disabled").unwrap_or(false),
        checked: node.prop_bool("checked").unwrap_or(false),
    }
}

/// Renders a node's props as inline CSS declaration text.
///
/// Two sources, in this order: the `style` object, then bare props. Bare props
/// come second so that a component which takes a prop *and* accepts a style
/// object resolves to the prop, which is the more specific statement.
///
/// Property names are normalised exactly as the TypeScript `toCssText` does —
/// `_` and camelCase both become `-`, custom properties are left alone — so a
/// stylesheet authored with the `stylesheet()` helper and a `style` object that
/// crossed the bridge cannot disagree about what an author wrote.
fn inline_css(node: &NativeNode) -> Option<String> {
    let mut declarations = Vec::new();
    if let Some(style) = node.props.get("style").and_then(serde_json::Value::as_object) {
        for (property, value) in style {
            push_declaration(&mut declarations, property, value);
        }
    }
    for (property, value) in &node.props {
        if matches!(property.as_str(), "style" | "className" | "class" | "id") {
            continue;
        }
        push_declaration(&mut declarations, property, value);
    }
    (!declarations.is_empty()).then(|| declarations.join("; "))
}

fn push_declaration(declarations: &mut Vec<String>, property: &str, value: &serde_json::Value) {
    let name = kebab_case(property);
    if let Some(value) = css_value(&name, value) {
        declarations.push(format!("{name}: {value}"));
    }
}

/// Properties whose bare numbers are ratios, not lengths.
///
/// Duplicated from the TypeScript `UNITLESS_PROPERTIES` in `src/css.ts`, and
/// the one place the two sides can silently disagree: a property missing here
/// turns `opacity: 0.5` into `opacity: 0.5px`, which the CSS parser drops
/// without error, so the element renders opaque and nothing says why.
///
/// A disagreement is invisible at runtime, so it is caught at test time
/// instead: both copies are spelled out in full by a test — `the_unitless_
/// list_is_the_one_the_typescript_side_pins` below, and "pins the same
/// unitless list the Rust host does" in `test/css.test.ts`. Changing the list
/// on one side alone fails that side's suite rather than shipping.
const UNITLESS: [&str; 7] = [
    "flex-grow",
    "flex-shrink",
    "opacity",
    "z-index",
    "aspect-ratio",
    "font-weight",
    "line-height",
];

/// Renders one prop value as a CSS declaration value, or nothing.
///
/// The `None` cases are all the same case: a value the declaration block
/// cannot carry. A non-finite number could not have crossed JSON in the first
/// place, and a string containing `;` is worse than useless — it would split
/// one declaration into two, and the offcut has no colon, so
/// `parse_declarations` rejects the *whole* block and the node silently loses
/// every inline style it had, including the ones the author did write. A
/// `<MenuItem label="Copy; Paste">` taking its own `style` prop down with it
/// is not a failure anyone would trace back to the label.
///
/// Booleans are dropped rather than rendered as `true`: no property this crate
/// resolves accepts one, so `disabled` and `checked` only ever produced a
/// declaration the cascade discarded — and one the TypeScript `toCssText`
/// never emits, since its value type is `string | number`. Those props reach
/// the cascade as selector state instead, which is where `:disabled` reads
/// them.
fn css_value(property: &str, value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => {
            (!value.is_empty() && !value.contains(';')).then(|| value.clone())
        }
        serde_json::Value::Number(value) => {
            let value = value.as_f64().filter(|value| value.is_finite())?;
            let unitless = UNITLESS.contains(&property);
            Some(if unitless { value.to_string() } else { format!("{value}px") })
        }
        _ => None,
    }
}

/// Normalises a JavaScript property name to its CSS spelling.
///
/// Custom properties are left alone: `--brandAccent` is a name the author
/// chose, not a camelCase spelling of a standard property, and rewriting it
/// would break the `var()` that reads it back.
fn kebab_case(property: &str) -> String {
    if property.starts_with("--") {
        return property.to_owned();
    }
    property.replace('_', "-").chars().fold(String::new(), |mut out, character| {
        if character.is_ascii_uppercase() {
            out.push('-');
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
        out
    })
}

/// Puts back the widget's own layout wherever the cascade said nothing.
///
/// [`spherekit_css::ResolvedStyle::apply_to`] assigns the whole [`Style`],
/// because a cascade result cannot tell "nobody mentioned this" from "somebody
/// set it to the default". For a plain `div` that is right — there is nothing
/// to lose. For a widget that ships structure in its own style it is
/// destructive: `panel()` is a column and `separator()` is a one-pixel rule,
/// and a stylesheet that only sets `gap` would turn the panel back into a row
/// and shrink the rule to nothing.
///
/// So the merge runs the other way round. A field still holding
/// [`Style::DEFAULT`]'s value after the cascade is one no rule mentioned, and
/// the widget's own answer goes back in. Composite fields are merged per edge
/// and per axis, so `width: 100%` on a separator does not also erase its
/// height.
///
/// The known cost: an author who writes the default value explicitly —
/// `flex-direction: row` on a panel — does not get it. Fixing that needs the
/// cascade to report *which* properties it saw, which `spherekit-css` does not
/// expose, and the alternative failure (every styled widget losing its shape)
/// is far louder.
fn restore_widget_defaults(style: &mut Style, widget: &Style) {
    macro_rules! restore {
        ($($($field:ident).+),+ $(,)?) => {
            $(
                if style.$($field).+ == Style::DEFAULT.$($field).+ {
                    style.$($field).+ = widget.$($field).+;
                }
            )+
        };
    }

    restore!(
        display,
        position,
        inset.top,
        inset.right,
        inset.bottom,
        inset.left,
        size.width,
        size.height,
        min_size.width,
        min_size.height,
        max_size.width,
        max_size.height,
        margin.top,
        margin.right,
        margin.bottom,
        margin.left,
        padding.top,
        padding.right,
        padding.bottom,
        padding.left,
        border.top,
        border.right,
        border.bottom,
        border.left,
        flex_direction,
        flex_wrap,
        flex_grow,
        flex_shrink,
        flex_basis,
        gap.width,
        gap.height,
        align_items,
        align_self,
        justify_content,
        align_content,
        overflow_x,
        overflow_y,
        aspect_ratio,
        corner_radius.top_left,
        corner_radius.top_right,
        corner_radius.bottom_right,
        corner_radius.bottom_left,
        opacity,
        z_index,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn node(props: &[(&str, Value)]) -> NativeNode {
        NativeNode {
            id: 1,
            node_type: "view".to_owned(),
            props: props.iter().map(|(name, value)| ((*name).to_owned(), value.clone())).collect(),
            text: None,
            hidden: false,
            children: Vec::new(),
        }
    }

    #[test]
    fn camel_case_props_become_kebab_case_declarations() {
        let css = inline_css(&node(&[("backgroundColor", json!("#101010"))]));
        assert_eq!(css.as_deref(), Some("background-color: #101010"));
    }

    #[test]
    fn an_underscore_prop_becomes_a_hyphen_like_the_typescript_side_does() {
        let css = inline_css(&node(&[("border_width", json!(2))]));
        assert_eq!(css.as_deref(), Some("border-width: 2px"));
    }

    #[test]
    fn a_custom_property_keeps_the_name_the_author_chose() {
        let css = inline_css(&node(&[("--brandAccent", json!("#ff8800"))]));
        assert_eq!(css.as_deref(), Some("--brandAccent: #ff8800"));
    }

    #[test]
    fn unitless_properties_do_not_gain_pixels() {
        let css = inline_css(&node(&[("opacity", json!(0.5))]));
        assert_eq!(css.as_deref(), Some("opacity: 0.5"));
        let css = inline_css(&node(&[("lineHeight", json!(1.4))]));
        assert_eq!(css.as_deref(), Some("line-height: 1.4"));
    }

    #[test]
    fn the_event_names_are_the_five_the_typescript_dispatcher_normalises() {
        // The renderer builds a prop name by capitalising the first letter and
        // prefixing `on`, so renaming one of these does not fail anything — it
        // just stops the matching React callback from ever firing. The other
        // half of the mapping is pinned by "normalises every native event name
        // to the prop React declares" in test/renderer.test.tsx.
        assert_eq!(
            [PRESS, VALUE_CHANGE, CHANGE, SELECT, SUBMIT],
            ["press", "valueChange", "change", "select", "submit"]
        );
    }

    #[test]
    fn the_unitless_list_is_the_one_the_typescript_side_pins() {
        // Spelled out rather than iterated, because the failure this guards
        // against is the list itself drifting. The same seven names, in the
        // same order, are asserted by "pins the same unitless list the Rust
        // host does" in test/css.test.ts.
        assert_eq!(
            UNITLESS,
            [
                "flex-grow",
                "flex-shrink",
                "opacity",
                "z-index",
                "aspect-ratio",
                "font-weight",
                "line-height",
            ]
        );

        for property in UNITLESS {
            assert_eq!(
                css_value(property, &json!(2)).as_deref(),
                Some("2"),
                "{property} got a unit"
            );
        }
        assert_eq!(css_value("gap", &json!(2)).as_deref(), Some("2px"), "a length lost its unit");
    }

    #[test]
    fn a_semicolon_in_one_prop_does_not_cost_the_node_its_other_inline_styles() {
        // Carried through, the semicolon would split one declaration into two
        // and leave an offcut with no colon, which invalidates the whole block
        // — so `width` would go down with the label that broke it.
        let node = node(&[("label", json!("Copy; Paste")), ("width", json!(10))]);
        let inline = inline_css(&node).expect("the carryable prop still declares");
        assert_eq!(inline, "width: 10px");

        let resolved = Stylesheet::default().resolve(css_node(&node, 0, 1), Some(&inline));
        assert_eq!(resolved.layout.size.width, spherekit_core::Length::Px(px(10.0)));
    }

    #[test]
    fn a_boolean_prop_declares_nothing_and_speaks_through_the_selector_instead() {
        let node = node(&[("disabled", json!(true))]);
        assert_eq!(inline_css(&node), None);
        assert!(element_state(&node).disabled);
    }

    #[test]
    fn a_style_object_and_a_bare_prop_both_reach_the_cascade() {
        let css = inline_css(&node(&[("style", json!({ "gap": 4 })), ("width", json!(10))]));
        assert_eq!(css.as_deref(), Some("gap: 4px; width: 10px"));
    }

    #[test]
    fn structural_props_are_never_treated_as_css() {
        let node = node(&[("className", json!("panel")), ("id", json!("main"))]);
        assert_eq!(inline_css(&node), None);
    }

    #[test]
    fn props_the_cascade_cannot_use_leave_no_declaration() {
        let css = inline_css(&node(&[("onPress", json!(null)), ("items", json!([1, 2]))]));
        assert_eq!(css, None);
    }

    #[test]
    fn disabled_and_checked_props_become_selector_state() {
        let mut props = BTreeMap::new();
        props.insert("disabled".to_owned(), json!(true));
        props.insert("checked".to_owned(), json!(true));
        let button = NativeNode {
            id: 1,
            node_type: "button".to_owned(),
            props,
            text: None,
            hidden: false,
            children: Vec::new(),
        };
        let state = element_state(&button);
        assert!(state.disabled && state.checked);
        assert!(!state.hover && !state.active && !state.focus);
    }

    #[test]
    fn the_merge_keeps_a_widgets_shape_where_no_rule_spoke() {
        let mut resolved = Style::DEFAULT;
        resolved.size.width = spherekit_core::Length::Px(px(100.0));
        let widget = Style {
            size: spherekit_core::Size {
                width: spherekit_core::Length::Fraction(1.0),
                height: spherekit_core::Length::Px(px(1.0)),
            },
            flex_direction: spherekit_layout::FlexDirection::Column,
            ..Style::DEFAULT
        };

        restore_widget_defaults(&mut resolved, &widget);
        assert_eq!(resolved.size.width, spherekit_core::Length::Px(px(100.0)));
        assert_eq!(resolved.size.height, spherekit_core::Length::Px(px(1.0)));
        assert_eq!(resolved.flex_direction, spherekit_layout::FlexDirection::Column);
    }

    #[test]
    fn an_unknown_presence_draws_no_dot() {
        assert!(presence_of("online").is_some());
        assert!(presence_of("lurking").is_none());
    }
}

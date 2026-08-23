//! The retained host: what a React root looks like from the native side.

use crate::events::EventQueue;
use crate::lower::{LowerContext, NodeBuilder, lower_tree};
use crate::tree::{NativeNode, NativeTree, NodePath, ReactHostError, index_tree, node_at};
use spherekit_css::{StyleContext, Stylesheet};
use spherekit_ui::AnyElement;
use std::collections::HashMap;
use std::fmt;

/// Retained Rust state for one React root.
///
/// Not `Clone`: the host owns a registry of application-supplied builder
/// closures, and a boxed `Fn` cannot be duplicated. An embedder that wants two
/// roots builds two hosts and registers its types on each, which is also the
/// only way the two could ever have different type tables.
#[derive(Default)]
pub struct ReactHost {
    tree: NativeTree,
    /// Where every committed id lives, rebuilt on each accepted commit.
    index: HashMap<u64, NodePath>,
    stylesheet: Stylesheet,
    context: StyleContext,
    builders: HashMap<String, NodeBuilder>,
}

impl ReactHost {
    /// Creates an empty native host tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Commits a decoded React snapshot after validating its identities, its
    /// node shape and its size.
    ///
    /// The new index is built before anything is installed, so a rejected
    /// commit leaves the previous tree *and* its index exactly as they were. A
    /// host that half-accepted a bad commit would be worse than one that
    /// refused it: the window would keep painting from a tree the lookup table
    /// no longer describes.
    pub fn commit(&mut self, tree: NativeTree) -> Result<(), ReactHostError> {
        if tree.revision <= self.tree.revision {
            return Err(ReactHostError::StaleRevision {
                incoming: tree.revision,
                current: self.tree.revision,
            });
        }

        let index = index_tree(&tree)?;
        self.tree = tree;
        self.index = index;
        Ok(())
    }

    /// Decodes and commits a JSON snapshot from the TypeScript bridge.
    pub fn commit_json(&mut self, json: &str) -> Result<(), ReactHostError> {
        let tree = serde_json::from_str(json).map_err(|error: serde_json::Error| {
            ReactHostError::InvalidSnapshot(error.to_string())
        })?;
        self.commit(tree)
    }

    /// Returns the most recently accepted native tree.
    pub fn tree(&self) -> &NativeTree {
        &self.tree
    }

    /// Serializes the current tree for diagnostics or another host boundary.
    pub fn tree_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.tree)
    }

    /// Replaces the CSS stylesheet used when lowering the React tree.
    ///
    /// The stylesheet is parsed before it is installed, so a malformed update
    /// leaves the previous stylesheet active.
    pub fn set_stylesheet(&mut self, css: &str) -> Result<(), spherekit_css::CssError> {
        let stylesheet = Stylesheet::parse(css)?;
        self.stylesheet = stylesheet;
        Ok(())
    }

    /// Returns the stylesheet currently used for native lowering.
    pub fn stylesheet(&self) -> &Stylesheet {
        &self.stylesheet
    }

    /// Sets what `rem`, `em`, the viewport units and the media queries resolve
    /// against.
    ///
    /// Worth setting on a window resize and on a system theme change: the
    /// cascade bakes those units down to pixels at commit time, so a stale
    /// context produces a correct-looking tree measured against the wrong
    /// viewport.
    pub fn set_style_context(&mut self, context: StyleContext) {
        self.context = context;
    }

    /// Returns the context lengths and media queries resolve in.
    pub fn style_context(&self) -> &StyleContext {
        &self.context
    }

    /// Registers a builder for one host type, replacing any previous one.
    ///
    /// The registry is consulted before the built-in table, so an application
    /// can both add a type of its own and replace a built-in whose native
    /// mapping does not suit it. That is deliberate: built-ins winning would
    /// mean an embedder who dislikes how `progress` lowers has to fork the
    /// crate.
    pub fn register_host_type(&mut self, name: impl Into<String>, builder: NodeBuilder) {
        self.builders.insert(name.into(), builder);
    }

    /// The application-registered builder for a host type, if there is one.
    pub(crate) fn builder(&self, name: &str) -> Option<&NodeBuilder> {
        self.builders.get(name)
    }

    /// Looks up a committed node by its React host identity.
    ///
    /// Resolved through the commit-time index: one bounds-checked index per
    /// level, and no sibling is ever looked at. The walk this replaced visited
    /// every node in the tree before the one it wanted.
    pub fn node(&self, id: u64) -> Option<&NativeNode> {
        node_at(&self.tree.children, self.index.get(&id)?)
    }

    /// How many nodes the current commit contains.
    pub fn node_count(&self) -> usize {
        self.index.len()
    }

    /// Whether an identity exists in the current commit.
    pub fn contains(&self, id: u64) -> bool {
        self.index.contains_key(&id)
    }

    /// Builds the current React tree as a native SphereKit element tree.
    ///
    /// No event handlers are installed. Use this for a snapshot, a test or a
    /// non-interactive render; anything the user can press wants
    /// [`ReactHost::ui_element_with_events`] instead.
    pub fn ui_element(&self) -> AnyElement {
        lower_tree(self, None)
    }

    /// Builds the tree with every native callback wired to `queue`.
    ///
    /// The queue is passed per call rather than held by the host because the
    /// element tree is rebuilt on every commit and the queue is not: it belongs
    /// to the embedder's frame loop, which drains it. Giving the host ownership
    /// would turn "when is it safe to drain" into a question about commits
    /// instead of about frames.
    pub fn ui_element_with_events(&self, queue: &EventQueue) -> AnyElement {
        lower_tree(self, Some(queue))
    }
}

impl fmt::Debug for ReactHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReactHost")
            .field("revision", &self.tree.revision)
            .field("nodes", &self.index.len())
            .field("rules", &self.stylesheet.rule_count())
            .field("registered_types", &self.builders.len())
            .finish()
    }
}

/// Boxes a closure as a [`NodeBuilder`].
///
/// Exists so a call site reads `register_host_type("gauge", node_builder(|cx, node| …))`
/// rather than spelling out the boxed trait object, which otherwise needs a
/// type annotation at every registration.
pub fn node_builder(
    builder: impl Fn(&LowerContext<'_>, &NativeNode) -> AnyElement + 'static,
) -> NodeBuilder {
    Box::new(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_TREE_DEPTH;
    use serde_json::json;
    use spherekit_core::{Color, Length, Point, Px, Size, px};
    use spherekit_ui::{
        ElementState, FocusDirection, IntoElement, Key, KeyEvent, Modifiers, MouseButton,
        MouseButtonEvent, ParentElement, Styled, TextInputEvent, UiEvent, UiTree, div, label,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    fn snapshot(revision: u64) -> &'static str {
        if revision == 1 {
            r##"{"revision":1,"children":[{"id":1,"type":"view","props":{"title":"SphereKit"},"children":[{"id":2,"type":"#text","text":"Ready","children":[]}]}]}"##
        } else {
            r#"{"revision":2,"children":[]}"#
        }
    }

    /// Commits one JSON snapshot into a fresh host.
    fn host_with(json: &str) -> ReactHost {
        let mut host = ReactHost::new();
        host.commit_json(json).expect("valid React commit");
        host
    }

    /// Commits a snapshot into a host whose sheet gives every node a hit box.
    ///
    /// Layout without a text system sizes every label to nothing, and a widget
    /// with an empty box cannot be clicked. Fixing the sizes in CSS is what
    /// makes the event tests below about event routing rather than about text
    /// measurement.
    fn interactive_host(json: &str) -> ReactHost {
        let mut host = ReactHost::new();
        host.set_stylesheet("* { width: 160px; height: 40px }").expect("stylesheet parses");
        host.commit_json(json).expect("valid React commit");
        host
    }

    fn mount(host: &ReactHost, queue: &EventQueue) -> UiTree {
        let mut ui = UiTree::new();
        ui.build(host.ui_element_with_events(queue));
        ui.compute_layout(Size::new(px(400.0), px(300.0))).expect("the tree lays out");
        ui
    }

    fn mouse(position: Point<Px>, state: ElementState) -> UiEvent {
        let event = MouseButtonEvent {
            position,
            button: MouseButton::Primary,
            state,
            click_count: 1,
            modifiers: Modifiers::NONE,
        };
        match state {
            ElementState::Pressed => UiEvent::MouseDown(event),
            ElementState::Released => UiEvent::MouseUp(event),
        }
    }

    fn centre_of(ui: &UiTree, id: u64) -> Point<Px> {
        let bounds = ui.bounds_of(id).expect("the node was laid out");
        assert!(!bounds.is_empty(), "node {id} has an empty hit box");
        bounds.center()
    }

    /// Counts the elements a lowered tree produces.
    fn element_count(host: &ReactHost) -> usize {
        let mut tree = UiTree::new();
        tree.build(host.ui_element());
        tree.stats().elements as usize
    }

    /// Depth-first search of a built element tree for a node of this width.
    fn has_width(element: &mut AnyElement, width: Length) -> bool {
        element.layout_style().size.width == width
            || element.children().iter_mut().any(|child| has_width(child, width))
    }

    fn px_width(pixels: f32) -> Length {
        Length::Px(px(pixels))
    }

    // ---------------------------------------------------------------- commits

    #[test]
    fn commits_and_finds_nodes() {
        let host = host_with(snapshot(1));
        assert_eq!(host.tree().revision, 1);
        assert_eq!(host.node(2).and_then(|node| node.text.as_deref()), Some("Ready"));
    }

    #[test]
    fn rejects_stale_and_duplicate_commits() {
        let mut host = host_with(snapshot(1));
        assert_eq!(
            host.commit_json(snapshot(1)),
            Err(ReactHostError::StaleRevision { incoming: 1, current: 1 })
        );

        let duplicate = r#"{"revision":2,"children":[{"id":7,"type":"view","children":[{"id":7,"type":"text","children":[]}]}]}"#;
        assert_eq!(host.commit_json(duplicate), Err(ReactHostError::DuplicateNodeId(7)));
    }

    #[test]
    fn malformed_json_is_reported_rather_than_panicking() {
        let mut host = ReactHost::new();
        assert!(matches!(host.commit_json("{not json"), Err(ReactHostError::InvalidSnapshot(_))));
    }

    #[test]
    fn a_rejected_commit_leaves_the_previous_tree_and_index_intact() {
        let mut host = host_with(snapshot(1));
        let bad = r#"{"revision":2,"children":[{"id":3,"type":"","children":[]}]}"#;
        assert_eq!(host.commit_json(bad), Err(ReactHostError::EmptyNodeType(3)));
        assert_eq!(host.tree().revision, 1);
        assert_eq!(host.node_count(), 2);
        assert!(host.contains(2));
    }

    #[test]
    fn a_snapshot_past_the_depth_limit_is_refused_instead_of_walked() {
        let leaf = |id: u64| NativeNode {
            id,
            node_type: "view".to_owned(),
            props: Default::default(),
            text: None,
            hidden: false,
            children: Vec::new(),
        };
        let mut node = leaf(0);
        for level in 1..=MAX_TREE_DEPTH as u64 {
            node = NativeNode { children: vec![node], ..leaf(level) };
        }

        let mut host = ReactHost::new();
        assert!(matches!(
            host.commit(NativeTree { revision: 1, children: vec![node] }),
            Err(ReactHostError::TooDeep { .. })
        ));
        assert_eq!(host.node_count(), 0);
        assert_eq!(host.tree().revision, 0);
    }

    #[test]
    fn a_round_trip_through_json_preserves_the_tree() {
        let host = host_with(snapshot(1));
        let mut second = ReactHost::new();
        second.commit_json(&host.tree_json().expect("the tree serialises")).expect("re-commits");
        assert_eq!(second.tree(), host.tree());
    }

    // -------------------------------------------------------------- id index

    #[test]
    fn the_index_is_rebuilt_when_a_commit_removes_nodes() {
        let mut host = host_with(snapshot(1));
        assert_eq!(host.node_count(), 2);
        assert!(host.contains(2));

        let smaller = r#"{"revision":2,"children":[{"id":1,"type":"view","children":[]}]}"#;
        host.commit_json(smaller).expect("valid React commit");
        assert_eq!(host.node_count(), 1);
        assert!(host.contains(1));
        assert!(!host.contains(2));
        assert!(host.node(2).is_none());
    }

    #[test]
    fn a_deep_lookup_follows_the_index_instead_of_scanning_siblings() {
        const WIDTH: u64 = 16;
        const DEPTH: u64 = 4;

        fn build(next: &mut u64, depth: u64) -> serde_json::Value {
            let own = *next;
            *next += 1;
            let children: Vec<_> = if depth == 0 {
                Vec::new()
            } else {
                (0..WIDTH).map(|_| build(next, depth - 1)).collect()
            };
            json!({ "id": own, "type": "view", "children": children })
        }

        let mut next = 1;
        let root = build(&mut next, DEPTH);
        let tree: NativeTree = serde_json::from_value(json!({ "revision": 1, "children": [root] }))
            .expect("the generated snapshot decodes");

        let mut host = ReactHost::new();
        host.commit(tree).expect("valid React commit");

        // The last id allocated is the deepest, right-most leaf.
        let leaf = next - 1;
        assert_eq!(host.node(leaf).map(|node| node.id), Some(leaf));

        let path = host.index.get(&leaf).expect("the leaf is indexed");
        // One hop per level. Reaching a leaf of a 70 000-node tree touches five
        // nodes, none of which is a sibling of anything on the way down.
        assert_eq!(path.len(), DEPTH as usize + 1);
        assert!(path.len() * 100 < host.node_count());
    }

    // ---------------------------------------------------------- node lowering

    #[test]
    fn lowers_the_commit_to_a_native_ui_tree() {
        assert_eq!(element_count(&host_with(snapshot(1))), 3);
    }

    #[test]
    fn every_built_in_host_type_lowers_to_something() {
        let types = [
            "view",
            "text",
            "button",
            "slider",
            "knob",
            "fader",
            "toggle",
            "checkbox",
            "progress",
            "separator",
            "panel",
            "scroll-view",
            "text-field",
            "avatar",
            "menu-item",
            "definitely-not-a-host-type",
        ];
        let children: Vec<_> = types
            .iter()
            .enumerate()
            .map(|(index, node_type)| json!({ "id": index + 1, "type": node_type }))
            .collect();
        let tree: NativeTree =
            serde_json::from_value(json!({ "revision": 1, "children": children }))
                .expect("the snapshot decodes");

        let mut host = ReactHost::new();
        host.commit(tree).expect("valid React commit");

        // Root, one element per type, plus the styled wrapper that `toggle`,
        // `checkbox`, `avatar` and `menu-item` need because they cannot carry a
        // style of their own.
        assert_eq!(element_count(&host), 1 + types.len() + 4);
    }

    #[test]
    fn a_text_node_and_a_text_element_both_become_one_label() {
        let json = r##"{"revision":1,"children":[
            {"id":1,"type":"text","children":[{"id":2,"type":"#text","text":"Hello","children":[]}]}
        ]}"##;
        // Root plus one label: the `#text` child is folded into the label's
        // content rather than laid out beside it.
        assert_eq!(element_count(&host_with(json)), 2);
    }

    #[test]
    fn a_button_takes_its_label_from_a_title_prop_or_from_its_children() {
        let json = r##"{"revision":1,"children":[
            {"id":1,"type":"button","props":{"title":"Save"}},
            {"id":2,"type":"button","children":[{"id":3,"type":"#text","text":"Cancel","children":[]}]}
        ]}"##;
        // Neither button lays its children out, so both are a single element.
        assert_eq!(element_count(&host_with(json)), 3);
    }

    #[test]
    fn a_hidden_node_is_taken_out_of_layout() {
        let json =
            r#"{"revision":1,"children":[{"id":1,"type":"view","hidden":true,"children":[]}]}"#;
        let mut root = host_with(json).ui_element();
        assert_eq!(root.children()[0].layout_style().display, spherekit_layout::Display::None);
    }

    #[test]
    fn a_hidden_widget_that_needs_a_wrapper_is_hidden_too() {
        let json = r#"{"revision":1,"children":[{"id":1,"type":"toggle","hidden":true}]}"#;
        let mut root = host_with(json).ui_element();
        assert_eq!(root.children()[0].layout_style().display, spherekit_layout::Display::None);
    }

    #[test]
    fn an_unknown_host_type_becomes_a_container_that_keeps_its_children() {
        let json = r##"{"revision":1,"children":[
            {"id":1,"type":"future-widget","children":[{"id":2,"type":"#text","text":"hi","children":[]}]}
        ]}"##;
        assert_eq!(element_count(&host_with(json)), 3);
    }

    #[test]
    fn a_scroll_view_keeps_laying_its_children_out() {
        let json = r##"{"revision":1,"children":[
            {"id":1,"type":"scroll-view","props":{"horizontal":true},"children":[
                {"id":2,"type":"view"},{"id":3,"type":"view"}
            ]}
        ]}"##;
        assert_eq!(element_count(&host_with(json)), 4);
    }

    #[test]
    fn a_panel_keeps_its_title_and_its_children() {
        let json = r#"{"revision":1,"children":[
            {"id":1,"type":"panel","props":{"title":"Mixer"},"children":[{"id":2,"type":"view"}]}
        ]}"#;
        // Root, the panel, its title label and the child view.
        assert_eq!(element_count(&host_with(json)), 4);
    }

    // -------------------------------------------------------------- registry

    #[test]
    fn a_registered_host_type_wins_over_the_fallback() {
        let mut host = ReactHost::new();
        host.register_host_type(
            "gauge",
            node_builder(|cx, node| {
                cx.apply(div().id(node.id)).child(label("gauge")).into_element()
            }),
        );
        host.commit_json(r#"{"revision":1,"children":[{"id":1,"type":"gauge","children":[]}]}"#)
            .expect("valid React commit");
        // The fallback container would have produced two elements; the custom
        // builder adds a label of its own.
        assert_eq!(element_count(&host), 3);
    }

    #[test]
    fn a_registered_host_type_wins_over_a_built_in() {
        let mut host = ReactHost::new();
        host.register_host_type(
            "button",
            node_builder(|cx, _node| {
                cx.apply(div()).child(label("a")).child(label("b")).into_element()
            }),
        );
        host.commit_json(r#"{"revision":1,"children":[{"id":1,"type":"button","children":[]}]}"#)
            .expect("valid React commit");
        assert_eq!(element_count(&host), 4);
    }

    #[test]
    fn a_custom_builder_can_lower_the_children_itself() {
        let mut host = ReactHost::new();
        host.register_host_type(
            "stack",
            node_builder(|cx, node| {
                cx.apply(div().id(node.id).flex_col())
                    .children_iter(cx.children(node))
                    .into_element()
            }),
        );
        let json = r#"{"revision":1,"children":[
            {"id":1,"type":"stack","children":[{"id":2,"type":"view"},{"id":3,"type":"view"}]}
        ]}"#;
        host.commit_json(json).expect("valid React commit");
        assert_eq!(element_count(&host), 4);
    }

    // ---------------------------------------------------------------- events

    #[test]
    fn ui_element_installs_no_handlers_at_all() {
        let host = interactive_host(r#"{"revision":1,"children":[{"id":1,"type":"button"}]}"#);
        let mut ui = UiTree::new();
        ui.build(host.ui_element());
        ui.compute_layout(Size::new(px(400.0), px(300.0))).expect("the tree lays out");
        let at = centre_of(&ui, 1);
        ui.dispatch(&mouse(at, ElementState::Released));
        // Nothing to assert on but the absence of a panic and of a queue: the
        // point is that a caller who did not ask for events gets none.
        assert_eq!(ui.stats().elements, 2);
    }

    #[test]
    fn a_button_press_reaches_the_queue_as_press() {
        let host = interactive_host(r#"{"revision":1,"children":[{"id":4,"type":"button"}]}"#);
        let queue = EventQueue::new();
        let mut ui = mount(&host, &queue);
        let at = centre_of(&ui, 4);
        ui.dispatch(&mouse(at, ElementState::Pressed));
        ui.dispatch(&mouse(at, ElementState::Released));

        let events = queue.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "press");
        assert_eq!(events[0].node_id, 4);
        assert_eq!(events[0].payload, None);
    }

    #[test]
    fn a_disabled_button_stays_silent() {
        let json =
            r#"{"revision":1,"children":[{"id":1,"type":"button","props":{"disabled":true}}]}"#;
        let host = interactive_host(json);
        let queue = EventQueue::new();
        let mut ui = mount(&host, &queue);
        let at = centre_of(&ui, 1);
        ui.dispatch(&mouse(at, ElementState::Released));
        assert!(queue.is_empty());
    }

    #[test]
    fn a_slider_reports_its_new_value_as_value_change() {
        let host = interactive_host(r#"{"revision":1,"children":[{"id":2,"type":"slider"}]}"#);
        let queue = EventQueue::new();
        let mut ui = mount(&host, &queue);
        let bounds = ui.bounds_of(2u64).expect("the slider was laid out");
        let at = Point::new(bounds.min_x() + bounds.width() * 0.75, bounds.center().y);
        ui.dispatch(&mouse(at, ElementState::Pressed));

        let events = queue.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "valueChange");
        assert_eq!(events[0].node_id, 2);
        let value = events[0]
            .payload
            .as_ref()
            .and_then(|payload| payload.get("value"))
            .and_then(serde_json::Value::as_f64)
            .expect("the payload carries a value");
        assert!((value - 0.75).abs() < 0.01, "unexpected value {value}");
    }

    #[test]
    fn a_toggle_reports_the_state_the_user_asked_for() {
        let json =
            r#"{"revision":1,"children":[{"id":3,"type":"toggle","props":{"checked":false}}]}"#;
        let host = interactive_host(json);
        let queue = EventQueue::new();
        let mut ui = mount(&host, &queue);
        let at = centre_of(&ui, 3);
        ui.dispatch(&mouse(at, ElementState::Released));

        let events = queue.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "change");
        assert_eq!(events[0].payload, Some(json!({ "checked": true })));
    }

    #[test]
    fn a_checkbox_uses_the_same_event_as_a_toggle() {
        let json =
            r#"{"revision":1,"children":[{"id":3,"type":"checkbox","props":{"checked":true}}]}"#;
        let host = interactive_host(json);
        let queue = EventQueue::new();
        let mut ui = mount(&host, &queue);
        let at = centre_of(&ui, 3);
        ui.dispatch(&mouse(at, ElementState::Released));
        assert_eq!(queue.drain()[0].payload, Some(json!({ "checked": false })));
    }

    #[test]
    fn a_menu_row_reports_select() {
        let json =
            r#"{"revision":1,"children":[{"id":6,"type":"menu-item","props":{"label":"Copy"}}]}"#;
        let host = interactive_host(json);
        let queue = EventQueue::new();
        let mut ui = mount(&host, &queue);
        let at = centre_of(&ui, 6);
        ui.dispatch(&mouse(at, ElementState::Released));

        let events = queue.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "select");
        assert_eq!(events[0].node_id, 6);
    }

    #[test]
    fn a_text_field_reports_submit_with_the_text_it_holds() {
        let json =
            r#"{"revision":1,"children":[{"id":8,"type":"text-field","props":{"value":"hi"}}]}"#;
        let host = interactive_host(json);
        let queue = EventQueue::new();
        let mut ui = mount(&host, &queue);
        ui.navigate_focus(FocusDirection::Next);
        ui.dispatch(&UiEvent::Key(KeyEvent {
            key: Key::Enter,
            state: ElementState::Pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        }));

        let events = queue.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "submit");
        assert_eq!(events[0].payload, Some(json!({ "text": "hi" })));
    }

    #[test]
    fn a_text_field_reports_change_as_the_user_types() {
        let json = r#"{"revision":1,"children":[{"id":9,"type":"text-field"}]}"#;
        let host = interactive_host(json);
        let queue = EventQueue::new();
        let mut ui = mount(&host, &queue);
        ui.navigate_focus(FocusDirection::Next);
        ui.dispatch(&UiEvent::TextInput(TextInputEvent { text: "a".into() }));

        let events = queue.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "change");
        assert_eq!(events[0].payload, Some(json!({ "text": "a" })));
    }

    // --------------------------------------------------------------- styling

    #[test]
    fn uses_a_shared_stylesheet_when_lowering_react_nodes() {
        let mut host = ReactHost::new();
        host.set_stylesheet(".panel { flex-direction: column; gap: 4px; }")
            .expect("stylesheet parses");
        let snapshot = r#"{"revision":1,"children":[{"id":1,"type":"view","props":{"className":"panel"},"children":[]}] }"#;
        host.commit_json(snapshot).expect("valid React commit");

        assert_eq!(host.stylesheet().rule_count(), 1);
        assert_eq!(element_count(&host), 2);
    }

    #[test]
    fn a_descendant_combinator_reaches_the_element_it_names() {
        let mut host = ReactHost::new();
        host.set_stylesheet(".rack button { width: 7px }").expect("stylesheet parses");
        let json = r#"{"revision":1,"children":[
            {"id":1,"type":"view","props":{"className":"rack"},"children":[{"id":2,"type":"button"}]},
            {"id":3,"type":"button"}
        ]}"#;
        host.commit_json(json).expect("valid React commit");

        let mut root = host.ui_element();
        let children = root.children();
        assert!(has_width(&mut children[0], px_width(7.0)));
        assert!(!has_width(&mut children[1], px_width(7.0)));
    }

    #[test]
    fn a_child_combinator_does_not_match_a_grandchild() {
        let mut host = ReactHost::new();
        host.set_stylesheet(".rack > button { width: 7px }").expect("stylesheet parses");
        let json = r#"{"revision":1,"children":[
            {"id":1,"type":"view","props":{"className":"rack"},"children":[
                {"id":2,"type":"view","children":[{"id":3,"type":"button"}]}
            ]}
        ]}"#;
        host.commit_json(json).expect("valid React commit");
        let mut root = host.ui_element();
        assert!(!has_width(&mut root, px_width(7.0)));
    }

    #[test]
    fn a_disabled_prop_makes_the_disabled_pseudo_class_match() {
        let mut host = ReactHost::new();
        host.set_stylesheet("button:disabled { width: 5px }").expect("stylesheet parses");
        let json = r#"{"revision":1,"children":[
            {"id":1,"type":"button","props":{"disabled":true}},
            {"id":2,"type":"button"}
        ]}"#;
        host.commit_json(json).expect("valid React commit");

        let mut root = host.ui_element();
        let children = root.children();
        assert_eq!(children[0].layout_style().size.width, px_width(5.0));
        assert_ne!(children[1].layout_style().size.width, px_width(5.0));
    }

    #[test]
    fn a_checked_prop_makes_the_checked_pseudo_class_match() {
        let mut host = ReactHost::new();
        host.set_stylesheet("toggle:checked { width: 6px }").expect("stylesheet parses");
        host.commit_json(
            r#"{"revision":1,"children":[{"id":1,"type":"toggle","props":{"checked":true}}]}"#,
        )
        .expect("valid React commit");
        let mut root = host.ui_element();
        // The wrapper carries the style, so the search has to go one level in.
        assert!(has_width(&mut root, px_width(6.0)));
    }

    #[test]
    fn nth_child_sees_the_position_the_walk_recorded() {
        let mut host = ReactHost::new();
        host.set_stylesheet("view:nth-child(2) { width: 3px }").expect("stylesheet parses");
        let json = r#"{"revision":1,"children":[{"id":1,"type":"view","children":[
            {"id":2,"type":"view"},{"id":3,"type":"view"},{"id":4,"type":"view"}
        ]}]}"#;
        host.commit_json(json).expect("valid React commit");

        let mut root = host.ui_element();
        let widths: Vec<_> = root.children()[0]
            .children()
            .iter_mut()
            .map(|child| child.layout_style().size.width)
            .collect();
        assert_eq!(widths[1], px_width(3.0));
        assert_ne!(widths[0], px_width(3.0));
    }

    #[test]
    fn a_root_custom_property_reaches_a_descendant() {
        let mut host = ReactHost::new();
        host.set_stylesheet(":root { --pad: 6px } .x { width: var(--pad) }")
            .expect("stylesheet parses");
        let json = r#"{"revision":1,"children":[{"id":1,"type":"view","children":[
            {"id":2,"type":"view","props":{"className":"x"}}
        ]}]}"#;
        host.commit_json(json).expect("valid React commit");
        let mut root = host.ui_element();
        assert!(has_width(&mut root, px_width(6.0)));
    }

    #[test]
    fn an_inline_style_object_still_beats_the_stylesheet() {
        let mut host = ReactHost::new();
        host.set_stylesheet(".panel { width: 10px }").expect("stylesheet parses");
        let json = r#"{"revision":1,"children":[
            {"id":1,"type":"view","props":{"className":"panel","style":{"width":42}}}
        ]}"#;
        host.commit_json(json).expect("valid React commit");
        let mut root = host.ui_element();
        assert_eq!(root.children()[0].layout_style().size.width, px_width(42.0));
    }

    #[test]
    fn a_camel_case_bare_prop_reaches_the_cascade_as_kebab_case() {
        let json = r#"{"revision":1,"children":[{"id":1,"type":"view","props":{"minWidth":24}}]}"#;
        let host = host_with(json);
        let mut root = host.ui_element();
        assert_eq!(root.children()[0].layout_style().min_size.width, px_width(24.0));
    }

    #[test]
    fn a_style_context_change_re_resolves_relative_units() {
        let mut host = ReactHost::new();
        host.set_stylesheet(".x { width: 2rem }").expect("stylesheet parses");
        host.commit_json(
            r#"{"revision":1,"children":[{"id":1,"type":"view","props":{"className":"x"}}]}"#,
        )
        .expect("valid React commit");

        let mut root = host.ui_element();
        assert_eq!(root.children()[0].layout_style().size.width, px_width(32.0));

        host.set_style_context(StyleContext::default().with_root_font_size(px(10.0)));
        let mut root = host.ui_element();
        assert_eq!(root.children()[0].layout_style().size.width, px_width(20.0));
        assert_eq!(host.style_context().root_font_size, px(10.0));
    }

    #[test]
    fn a_media_query_follows_the_hosts_viewport() {
        let mut host = ReactHost::new();
        host.set_stylesheet("@media (min-width: 600px) { view { width: 8px } }")
            .expect("stylesheet parses");
        host.commit_json(r#"{"revision":1,"children":[{"id":1,"type":"view"}]}"#)
            .expect("valid React commit");
        let mut root = host.ui_element();
        assert_eq!(root.children()[0].layout_style().size.width, px_width(8.0));

        host.set_style_context(
            StyleContext::default().with_viewport(Size::new(px(320.0), px(640.0))),
        );
        let mut root = host.ui_element();
        assert_eq!(root.children()[0].layout_style().size.width, Length::Auto);
    }

    #[test]
    fn a_widget_keeps_the_shape_the_stylesheet_did_not_mention() {
        let mut host = ReactHost::new();
        host.set_stylesheet("separator { width: 40px }").expect("stylesheet parses");
        host.commit_json(r#"{"revision":1,"children":[{"id":1,"type":"separator"}]}"#)
            .expect("valid React commit");

        let mut root = host.ui_element();
        let style = root.children()[0].layout_style();
        assert_eq!(style.size.width, px_width(40.0));
        // The rule never mentioned height, so the hairline is still a hairline.
        assert_eq!(style.size.height, px_width(1.0));
    }

    #[test]
    fn a_malformed_stylesheet_leaves_the_previous_one_in_place() {
        let mut host = ReactHost::new();
        host.set_stylesheet(".x { width: 4px }").expect("stylesheet parses");
        assert!(host.set_stylesheet(".y width: 1px").is_err());
        assert_eq!(host.stylesheet().rule_count(), 1);
    }

    // ------------------------------------------------------ probes and text

    #[derive(Debug, PartialEq)]
    struct Recorded {
        color: Option<Color>,
        font_size: Option<Px>,
        family: Option<Vec<String>>,
        has_hover: bool,
    }

    /// Registers a `probe` host type that records what the walk handed it.
    fn probe(host: &mut ReactHost, sink: Rc<RefCell<Vec<Recorded>>>) {
        host.register_host_type(
            "probe",
            node_builder(move |cx, node| {
                sink.borrow_mut().push(Recorded {
                    color: cx.text().color,
                    font_size: cx.text().font_size,
                    family: cx.text().font_family.clone(),
                    has_hover: cx.style().hover.is_some(),
                });
                cx.apply(div().id(node.id)).into_element()
            }),
        );
    }

    fn recorded(host: &ReactHost, sink: &Rc<RefCell<Vec<Recorded>>>) -> Vec<Recorded> {
        let _ = host.ui_element();
        sink.take()
    }

    #[test]
    fn text_properties_inherit_down_the_tree() {
        let sink = Rc::new(RefCell::new(Vec::new()));
        let mut host = ReactHost::new();
        probe(&mut host, Rc::clone(&sink));
        host.set_stylesheet(".page { color: #ff0000; font-size: 20px; font-family: Inter }")
            .expect("stylesheet parses");
        host.commit_json(
            r#"{"revision":1,"children":[{"id":1,"type":"view","props":{"className":"page"},
                "children":[{"id":2,"type":"view","children":[{"id":3,"type":"probe"}]}]}]}"#,
        )
        .expect("valid React commit");

        let seen = recorded(&host, &sink);
        assert_eq!(seen[0].color, Some(Color::RED));
        assert_eq!(seen[0].font_size, Some(px(20.0)));
        assert_eq!(seen[0].family.as_deref(), Some(["Inter".to_owned()].as_slice()));
    }

    #[test]
    fn a_nodes_own_typography_wins_over_the_inherited_one() {
        let sink = Rc::new(RefCell::new(Vec::new()));
        let mut host = ReactHost::new();
        probe(&mut host, Rc::clone(&sink));
        host.set_stylesheet(".page { color: #ff0000 } .quiet { color: #00ff00 }")
            .expect("stylesheet parses");
        host.commit_json(
            r#"{"revision":1,"children":[{"id":1,"type":"view","props":{"className":"page"},
                "children":[{"id":2,"type":"probe","props":{"className":"quiet"}}]}]}"#,
        )
        .expect("valid React commit");

        assert_eq!(recorded(&host, &sink)[0].color, Some(Color::GREEN));
    }

    #[test]
    fn typography_does_not_leak_back_up_to_a_sibling() {
        let sink = Rc::new(RefCell::new(Vec::new()));
        let mut host = ReactHost::new();
        probe(&mut host, Rc::clone(&sink));
        host.set_stylesheet(".loud { color: #ff0000 }").expect("stylesheet parses");
        host.commit_json(
            r#"{"revision":1,"children":[
                {"id":1,"type":"view","props":{"className":"loud"},"children":[{"id":2,"type":"probe"}]},
                {"id":3,"type":"probe"}
            ]}"#,
        )
        .expect("valid React commit");

        let seen = recorded(&host, &sink);
        assert_eq!(seen[0].color, Some(Color::RED));
        assert_eq!(seen[1].color, None);
    }

    #[test]
    fn a_hover_rule_reaches_the_element_as_an_interaction_variant() {
        let sink = Rc::new(RefCell::new(Vec::new()));
        let mut host = ReactHost::new();
        probe(&mut host, Rc::clone(&sink));
        host.set_stylesheet("probe { background: #101010 } probe:hover { background: #202020 }")
            .expect("stylesheet parses");
        host.commit_json(r#"{"revision":1,"children":[{"id":1,"type":"probe"}]}"#)
            .expect("valid React commit");

        assert!(recorded(&host, &sink)[0].has_hover);
    }

    #[test]
    fn no_hover_rule_means_no_hover_variant_to_paint() {
        let sink = Rc::new(RefCell::new(Vec::new()));
        let mut host = ReactHost::new();
        probe(&mut host, Rc::clone(&sink));
        host.set_stylesheet("probe { background: #101010 }").expect("stylesheet parses");
        host.commit_json(r#"{"revision":1,"children":[{"id":1,"type":"probe"}]}"#)
            .expect("valid React commit");

        assert!(!recorded(&host, &sink)[0].has_hover);
    }

    #[test]
    fn a_debug_line_summarises_the_host_without_dumping_the_tree() {
        let rendered = format!("{:?}", host_with(snapshot(1)));
        assert!(rendered.contains("nodes: 2"), "unexpected debug output: {rendered}");
    }
}

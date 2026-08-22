//! Rust-side host tree for the TypeScript React renderer.
//!
//! The TypeScript side owns React reconciliation. After each commit it sends
//! a [`NativeTree`] snapshot to this crate. The host validates the snapshot,
//! retains it, and exposes it to the native layout/render pipeline. Keeping
//! this boundary as data rather than React internals makes the backend usable
//! from V8, an FFI host, or a test harness.

#![deny(missing_docs)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spherekit_css::{Node as CssNode, Stylesheet};
pub use spherekit_ui::AnyElement;
use spherekit_ui::{IntoElement, ParentElement, Styled, button, div, label, scroll_view, slider};
use std::collections::{BTreeMap, HashSet};
use thiserror::Error;

/// A complete React commit received by the native host.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct NativeTree {
    /// Monotonically increasing commit number from the React renderer.
    pub revision: u64,
    /// Root children. React supports more than one child at the root.
    pub children: Vec<NativeNode>,
}

/// One serializable host node in the committed native tree.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NativeNode {
    /// Stable identity allocated by the React host renderer.
    pub id: u64,
    /// Native host type, for example `view`, `text`, or `button`.
    #[serde(rename = "type")]
    pub node_type: String,
    /// Plain props understood by the native widget layer.
    #[serde(default)]
    pub props: BTreeMap<String, Value>,
    /// Text content for a `#text` node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether React has hidden this host node for Suspense or an equivalent
    /// visibility transition.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// Child nodes in paint order.
    #[serde(default)]
    pub children: Vec<NativeNode>,
}

/// Errors returned when a React commit cannot be accepted by the host.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReactHostError {
    /// The incoming JSON snapshot was malformed.
    #[error("invalid React host snapshot: {0}")]
    InvalidSnapshot(String),
    /// A commit arrived older than the current native tree.
    #[error("stale React commit {incoming}; current revision is {current}")]
    StaleRevision {
        /// Revision received from TypeScript.
        incoming: u64,
        /// Revision already retained by the host.
        current: u64,
    },
    /// Two nodes reused the same React host identity.
    #[error("duplicate React host node id {0}")]
    DuplicateNodeId(u64),
    /// A node did not have a host type.
    #[error("React host node {0} has an empty type")]
    EmptyNodeType(u64),
    /// Only text nodes may carry a `text` payload.
    #[error("React host node {0} has text but is not a #text node")]
    UnexpectedText(u64),
    /// A text node must carry text content.
    #[error("React text node {0} has no text payload")]
    MissingText(u64),
}

/// Retained Rust state for one React root.
#[derive(Clone, Debug, Default)]
pub struct ReactHost {
    tree: NativeTree,
    stylesheet: Stylesheet,
}

impl ReactHost {
    /// Creates an empty native host tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Commits a decoded React snapshot after validating its identities and
    /// node shape.
    pub fn commit(&mut self, tree: NativeTree) -> Result<(), ReactHostError> {
        if tree.revision <= self.tree.revision {
            return Err(ReactHostError::StaleRevision {
                incoming: tree.revision,
                current: self.tree.revision,
            });
        }

        let mut ids = HashSet::new();
        for node in &tree.children {
            validate_node(node, &mut ids)?;
        }

        self.tree = tree;
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

    /// Looks up a committed node by its React host identity.
    pub fn node(&self, id: u64) -> Option<&NativeNode> {
        self.tree.children.iter().find_map(|node| find_node(node, id))
    }

    /// Builds the current React tree as a native SphereKit element tree.
    ///
    /// This is the first native lowering pass: common host nodes map directly
    /// to SphereKit containers and widgets, while unknown host types become a
    /// plain layout container. React event callbacks are intentionally not
    /// reconstructed here; the event bridge will route them by node ID.
    pub fn ui_element(&self) -> AnyElement {
        let mut root = div().flex_col();
        root = root.children_iter(
            self.tree.children.iter().map(|node| node_to_element(node, &self.stylesheet)),
        );
        root.into_element()
    }
}

fn node_to_element(node: &NativeNode, stylesheet: &Stylesheet) -> AnyElement {
    match node.node_type.as_str() {
        "#text" => apply_style(
            label(node.text.as_deref().unwrap_or_default()).id(node.id),
            node,
            stylesheet,
        )
        .into_element(),
        "button" => {
            let title = prop_string(node, "title").unwrap_or_else(|| text_content(node));
            apply_style(
                button(title).id(node.id).disabled(prop_bool(node, "disabled").unwrap_or(false)),
                node,
                stylesheet,
            )
            .into_element()
        }
        "text" => {
            apply_style(label(text_content(node)).id(node.id), node, stylesheet).into_element()
        }
        "slider" => {
            let value = prop_number(node, "value").unwrap_or(0.0);
            let min = prop_number(node, "minimumValue").unwrap_or(0.0);
            let max = prop_number(node, "maximumValue").unwrap_or(1.0);
            let mut control = slider(value).id(node.id).range(min, max);
            if let Some(step) = prop_number(node, "step") {
                control = control.step(step);
            }
            if prop_bool(node, "disabled").unwrap_or(false) {
                control = control.disabled(true);
            }
            apply_style(control, node, stylesheet).into_element()
        }
        "scroll-view" => {
            let scroll = scroll_view()
                .id(node.id)
                .horizontal(prop_bool(node, "horizontal").unwrap_or(false));
            apply_style(scroll, node, stylesheet)
                .children_iter(node.children.iter().map(|child| node_to_element(child, stylesheet)))
                .into_element()
        }
        _ => {
            let view = apply_style(div().id(node.id), node, stylesheet);
            view.children_iter(node.children.iter().map(|child| node_to_element(child, stylesheet)))
                .into_element()
        }
    }
}

fn apply_style<T: Styled>(element: T, node: &NativeNode, stylesheet: &Stylesheet) -> T {
    let css_id = prop_string(node, "id");
    let css_classes = prop_string(node, "className").or_else(|| prop_string(node, "class"));
    let inline = inline_css(node);
    let css_node = CssNode::new(&node.node_type)
        .with_id_option(css_id.as_deref())
        .with_classes(css_classes.as_deref().unwrap_or(""));
    let resolved = stylesheet.resolve(css_node, inline.as_deref());
    let mut element = resolved.apply_to(element);
    if node.hidden {
        element = element.hidden();
    }
    element
}

fn text_content(node: &NativeNode) -> String {
    let mut text = node.text.clone().unwrap_or_default();
    for child in &node.children {
        text.push_str(&text_content(child));
    }
    text
}

fn prop_value<'a>(node: &'a NativeNode, name: &str) -> Option<&'a Value> {
    node.props.get(name).or_else(|| {
        node.props.get("style").and_then(Value::as_object).and_then(|style| style.get(name))
    })
}

fn prop_number(node: &NativeNode, name: &str) -> Option<f32> {
    prop_value(node, name).and_then(Value::as_f64).map(|value| value as f32)
}

fn prop_bool(node: &NativeNode, name: &str) -> Option<bool> {
    prop_value(node, name).and_then(Value::as_bool)
}

fn prop_string(node: &NativeNode, name: &str) -> Option<String> {
    prop_value(node, name).and_then(Value::as_str).map(ToOwned::to_owned)
}

fn inline_css(node: &NativeNode) -> Option<String> {
    let mut declarations = Vec::new();
    if let Some(style) = node.props.get("style").and_then(Value::as_object) {
        for (property, value) in style {
            if let Some(value) = css_value(property, value) {
                declarations.push(format!("{property}: {value}"));
            }
        }
    }
    for (property, value) in &node.props {
        if matches!(property.as_str(), "style" | "className" | "class" | "id") {
            continue;
        }
        if let Some(value) = css_value(property, value) {
            declarations.push(format!("{property}: {value}"));
        }
    }
    (!declarations.is_empty()).then(|| declarations.join("; "))
}

fn css_value(property: &str, value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => {
            let value = value.as_f64()?;
            let normalized =
                property.replace('_', "-").chars().fold(String::new(), |mut output, character| {
                    if character.is_ascii_uppercase() {
                        output.push('-');
                        output.push(character.to_ascii_lowercase());
                    } else {
                        output.push(character);
                    }
                    output
                });
            let unitless = matches!(
                normalized.as_str(),
                "flex-grow" | "flex-shrink" | "opacity" | "z-index" | "aspect-ratio"
            );
            Some(if unitless { value.to_string() } else { format!("{value}px") })
        }
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn validate_node(node: &NativeNode, ids: &mut HashSet<u64>) -> Result<(), ReactHostError> {
    if node.node_type.is_empty() {
        return Err(ReactHostError::EmptyNodeType(node.id));
    }
    if !ids.insert(node.id) {
        return Err(ReactHostError::DuplicateNodeId(node.id));
    }

    if node.node_type == "#text" {
        if node.text.is_none() {
            return Err(ReactHostError::MissingText(node.id));
        }
        if !node.children.is_empty() {
            return Err(ReactHostError::UnexpectedText(node.id));
        }
    } else if node.text.is_some() {
        return Err(ReactHostError::UnexpectedText(node.id));
    }

    for child in &node.children {
        validate_node(child, ids)?;
    }
    Ok(())
}

fn find_node(node: &NativeNode, id: u64) -> Option<&NativeNode> {
    if node.id == id {
        return Some(node);
    }
    node.children.iter().find_map(|child| find_node(child, id))
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(revision: u64) -> &'static str {
        if revision == 1 {
            r##"{"revision":1,"children":[{"id":1,"type":"view","props":{"title":"SphereKit"},"children":[{"id":2,"type":"#text","text":"Ready","children":[]}]}]}"##
        } else {
            r#"{"revision":2,"children":[]}"#
        }
    }

    #[test]
    fn commits_and_finds_nodes() {
        let mut host = ReactHost::new();
        host.commit_json(snapshot(1)).expect("valid React commit");
        assert_eq!(host.tree().revision, 1);
        assert_eq!(host.node(2).and_then(|node| node.text.as_deref()), Some("Ready"));
    }

    #[test]
    fn rejects_stale_and_duplicate_commits() {
        let mut host = ReactHost::new();
        host.commit_json(snapshot(1)).expect("valid React commit");
        assert_eq!(
            host.commit_json(snapshot(1)),
            Err(ReactHostError::StaleRevision { incoming: 1, current: 1 })
        );

        let duplicate = r#"{"revision":2,"children":[{"id":7,"type":"view","children":[{"id":7,"type":"text","children":[]}]}]}"#;
        assert_eq!(host.commit_json(duplicate), Err(ReactHostError::DuplicateNodeId(7)));
    }

    #[test]
    fn lowers_the_commit_to_a_native_ui_tree() {
        let mut host = ReactHost::new();
        host.commit_json(snapshot(1)).expect("valid React commit");

        let mut tree = spherekit_ui::UiTree::new();
        tree.build(host.ui_element());
        assert_eq!(tree.stats().elements, 3);
    }

    #[test]
    fn uses_a_shared_stylesheet_when_lowering_react_nodes() {
        let mut host = ReactHost::new();
        host.set_stylesheet(".panel { flex-direction: column; gap: 4px; }")
            .expect("stylesheet parses");
        let snapshot = r#"{"revision":1,"children":[{"id":1,"type":"view","props":{"className":"panel"},"children":[]}] }"#;
        host.commit_json(snapshot).expect("valid React commit");

        let mut tree = spherekit_ui::UiTree::new();
        tree.build(host.ui_element());
        assert_eq!(host.stylesheet().rule_count(), 1);
        assert_eq!(tree.stats().elements, 2);
    }
}

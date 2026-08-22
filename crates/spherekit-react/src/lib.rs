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
use spherekit_core::{Color, px};
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
        let mut root = apply_style(div().flex_col(), &NativeNode::root());
        root = root.children_iter(self.tree.children.iter().map(node_to_element));
        root.into_element()
    }
}

fn node_to_element(node: &NativeNode) -> AnyElement {
    match node.node_type.as_str() {
        "#text" => label(node.text.as_deref().unwrap_or_default()).id(node.id).into_element(),
        "text" => label(text_content(node)).id(node.id).into_element(),
        "button" => {
            let title = prop_string(node, "title").unwrap_or_else(|| text_content(node));
            button(title)
                .id(node.id)
                .disabled(prop_bool(node, "disabled").unwrap_or(false))
                .into_element()
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
            control.into_element()
        }
        "scroll-view" => {
            let scroll = scroll_view()
                .id(node.id)
                .horizontal(prop_bool(node, "horizontal").unwrap_or(false));
            apply_style(scroll, node)
                .children_iter(node.children.iter().map(node_to_element))
                .into_element()
        }
        _ => {
            let view = apply_style(div().id(node.id), node);
            view.children_iter(node.children.iter().map(node_to_element)).into_element()
        }
    }
}

fn apply_style<T: Styled>(mut element: T, node: &NativeNode) -> T {
    if node.hidden {
        element = element.hidden();
    }
    if let Some(direction) = prop_string(node, "flexDirection") {
        element = match direction.as_str() {
            "column" => element.flex_col(),
            _ => element.flex_row(),
        };
    }
    if let Some(gap) = prop_number(node, "gap") {
        element = element.gap(px(gap));
    }
    if let Some(padding) = prop_number(node, "padding") {
        element = element.p(px(padding));
    }
    if let Some(width) = prop_number(node, "width") {
        element = element.w(px(width));
    }
    if let Some(height) = prop_number(node, "height") {
        element = element.h(px(height));
    }
    if let Some(grow) = prop_number(node, "flexGrow") {
        element = element.grow(grow);
    }
    if let Some(color) = prop_color(node, "backgroundColor") {
        element = element.bg(color);
    }
    if let Some(radius) = prop_number(node, "borderRadius") {
        element = element.rounded(px(radius));
    }
    if let (Some(width), Some(color)) =
        (prop_number(node, "borderWidth"), prop_color(node, "borderColor"))
    {
        element = element.border(px(width), color);
    }
    if let Some(opacity) = prop_number(node, "opacity") {
        element = element.opacity(opacity);
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

fn prop_color(node: &NativeNode, name: &str) -> Option<Color> {
    let value = prop_value(node, name)?;
    if let Some(text) = value.as_str() {
        let hex = text.strip_prefix('#')?;
        return match hex.len() {
            6 => u32::from_str_radix(hex, 16).ok().map(Color::hex),
            8 => u32::from_str_radix(hex, 16).ok().map(Color::hex_rgba),
            _ => None,
        };
    }
    value.as_u64().and_then(|value| u32::try_from(value).ok()).map(Color::hex)
}

impl NativeNode {
    fn root() -> Self {
        Self {
            id: 0,
            node_type: "view".into(),
            props: BTreeMap::new(),
            text: None,
            hidden: false,
            children: Vec::new(),
        }
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
}

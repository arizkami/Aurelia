//! The committed snapshot, and what makes one acceptable.
//!
//! Everything here is plain data. The React reconciler owns the mutation
//! stream; this crate only ever sees the finished tree, which is what keeps the
//! native side from observing a half-built commit and makes the whole boundary
//! one value a test can assert on.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

/// How deep a committed React tree may nest.
///
/// Validation, lowering and `Drop` all walk the tree recursively, so a snapshot
/// deep enough to exhaust the stack would abort the process — a stack overflow
/// cannot be caught and turned into a rejected commit. The limit exists so that
/// a malformed or hostile snapshot fails the commit instead.
///
/// 128 is `serde_json`'s own nesting limit, so a JSON commit that would trip
/// this has already been refused by the decoder: the two entry points agree on
/// what is acceptable rather than accepting different trees.
pub const MAX_TREE_DEPTH: usize = 128;

/// How many nodes one commit may carry.
///
/// A React tree of this size is a bug or an attack, not a user interface: no
/// display has room for a hundred thousand widgets, and retaining one would
/// cost a lowering pass and a layout pass that never visibly finish. Refusing
/// the commit leaves the previous tree on screen, which is a far better failure
/// than a frozen window.
pub const MAX_NODE_COUNT: usize = 100_000;

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

impl NativeNode {
    /// Reads a prop, falling back to a same-named key inside the `style` object.
    ///
    /// The fallback exists because the React side has two idioms for the same
    /// intent — a bare prop and an entry in `style` — and a host that honoured
    /// only one of them would make the other silently do nothing.
    pub fn prop(&self, name: &str) -> Option<&Value> {
        self.props.get(name).or_else(|| {
            self.props.get("style").and_then(Value::as_object).and_then(|style| style.get(name))
        })
    }

    /// Reads a string prop.
    pub fn prop_str(&self, name: &str) -> Option<&str> {
        self.prop(name).and_then(Value::as_str)
    }

    /// Reads a numeric prop.
    pub fn prop_f32(&self, name: &str) -> Option<f32> {
        self.prop(name).and_then(Value::as_f64).map(|value| value as f32)
    }

    /// Reads a boolean prop.
    pub fn prop_bool(&self, name: &str) -> Option<bool> {
        self.prop(name).and_then(Value::as_bool)
    }

    /// The text of this node and every descendant, concatenated.
    ///
    /// A widget that paints its own label — a button, a menu row — takes its
    /// text this way rather than laying its children out, so `<Button>Save</Button>`
    /// and `<Button title="Save" />` produce the same native widget.
    pub fn text_content(&self) -> String {
        let mut text = self.text.clone().unwrap_or_default();
        for child in &self.children {
            text.push_str(&child.text_content());
        }
        text
    }
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
    /// The snapshot nested deeper than the host is willing to walk.
    #[error("React host node {id} nests deeper than the {limit}-level limit")]
    TooDeep {
        /// The first node found past the limit.
        id: u64,
        /// The limit that was exceeded, always [`MAX_TREE_DEPTH`].
        limit: usize,
    },
    /// The snapshot carried more nodes than the host is willing to retain.
    #[error("React host commit carries more than {limit} nodes")]
    TooManyNodes {
        /// The limit that was exceeded, always [`MAX_NODE_COUNT`].
        limit: usize,
    },
}

/// Where one node sits, as the child indices to follow from the tree root.
///
/// A path rather than a pointer or an arena handle: the retained tree is a
/// plain owned value that a commit replaces wholesale, and anything holding a
/// reference into the old one would have to be invalidated by hand on every
/// commit. Following a path costs one bounds-checked index per level and looks
/// at no siblings, which is the property that matters — the walk it replaces
/// looked at every node in the tree.
pub(crate) type NodePath = Vec<u32>;

/// Validates a snapshot and builds its identity index in one walk.
///
/// Both jobs need the same traversal and both must fail the whole commit, so
/// doing them together is what keeps a rejected commit from leaving a
/// half-built index behind: the caller only installs either once this returns.
pub(crate) fn index_tree(tree: &NativeTree) -> Result<HashMap<u64, NodePath>, ReactHostError> {
    let mut index = HashMap::new();
    let mut path = NodePath::new();
    for (position, node) in tree.children.iter().enumerate() {
        path.push(position as u32);
        walk(node, &mut path, &mut index, 1)?;
        path.pop();
    }
    Ok(index)
}

fn walk(
    node: &NativeNode,
    path: &mut NodePath,
    index: &mut HashMap<u64, NodePath>,
    depth: usize,
) -> Result<(), ReactHostError> {
    if depth > MAX_TREE_DEPTH {
        return Err(ReactHostError::TooDeep { id: node.id, limit: MAX_TREE_DEPTH });
    }
    if index.len() >= MAX_NODE_COUNT {
        return Err(ReactHostError::TooManyNodes { limit: MAX_NODE_COUNT });
    }
    if node.node_type.is_empty() {
        return Err(ReactHostError::EmptyNodeType(node.id));
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
    if index.insert(node.id, path.clone()).is_some() {
        return Err(ReactHostError::DuplicateNodeId(node.id));
    }

    for (position, child) in node.children.iter().enumerate() {
        path.push(position as u32);
        walk(child, path, index, depth + 1)?;
        path.pop();
    }
    Ok(())
}

/// Follows a path from the root children to the node it names.
pub(crate) fn node_at<'a>(roots: &'a [NativeNode], path: &[u32]) -> Option<&'a NativeNode> {
    let mut siblings = roots;
    let mut found = None;
    for step in path {
        let node = siblings.get(*step as usize)?;
        siblings = &node.children;
        found = Some(node);
    }
    found
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: u64, node_type: &str) -> NativeNode {
        NativeNode {
            id,
            node_type: node_type.to_owned(),
            props: BTreeMap::new(),
            text: None,
            hidden: false,
            children: Vec::new(),
        }
    }

    fn nest(depth: usize) -> NativeNode {
        let mut root = node(0, "view");
        let mut cursor = &mut root;
        for level in 1..depth {
            cursor.children.push(node(level as u64, "view"));
            cursor = cursor.children.last_mut().expect("the child was just pushed");
        }
        root
    }

    #[test]
    fn a_prop_falls_back_to_the_style_object() {
        let mut view = node(1, "view");
        view.props.insert("style".into(), json!({ "value": 4 }));
        assert_eq!(view.prop_f32("value"), Some(4.0));
        assert_eq!(view.prop_f32("missing"), None);
    }

    #[test]
    fn text_content_gathers_the_whole_subtree() {
        let mut view = node(1, "view");
        let mut first = node(2, "#text");
        first.text = Some("Sphere".into());
        let mut second = node(3, "#text");
        second.text = Some("Kit".into());
        view.children = vec![first, second];
        assert_eq!(view.text_content(), "SphereKit");
    }

    #[test]
    fn the_index_records_one_path_per_node() {
        let mut root = node(1, "view");
        root.children = vec![node(2, "view"), node(3, "view")];
        let mut second = node(4, "view");
        second.children = vec![node(5, "text")];
        let tree = NativeTree { revision: 1, children: vec![root, second] };

        let index = index_tree(&tree).expect("a well-formed tree indexes");
        assert_eq!(index.len(), 5);
        assert_eq!(index[&1], vec![0]);
        assert_eq!(index[&3], vec![0, 1]);
        assert_eq!(index[&5], vec![1, 0]);
        assert_eq!(node_at(&tree.children, &index[&5]).map(|node| node.id), Some(5));
    }

    #[test]
    fn a_tree_past_the_depth_limit_is_refused_rather_than_walked() {
        let tree = NativeTree { revision: 1, children: vec![nest(MAX_TREE_DEPTH + 2)] };
        assert_eq!(
            index_tree(&tree),
            Err(ReactHostError::TooDeep { id: MAX_TREE_DEPTH as u64, limit: MAX_TREE_DEPTH })
        );
    }

    #[test]
    fn a_tree_exactly_at_the_depth_limit_is_accepted() {
        let tree = NativeTree { revision: 1, children: vec![nest(MAX_TREE_DEPTH)] };
        assert_eq!(index_tree(&tree).map(|index| index.len()), Ok(MAX_TREE_DEPTH));
    }

    #[test]
    fn a_commit_past_the_node_limit_is_refused_rather_than_retained() {
        // One more than the limit, flat, so the depth check cannot fire first.
        let children = (0..=MAX_NODE_COUNT as u64).map(|id| node(id, "view")).collect();
        let tree = NativeTree { revision: 1, children };
        assert_eq!(index_tree(&tree), Err(ReactHostError::TooManyNodes { limit: MAX_NODE_COUNT }));
    }

    #[test]
    fn an_empty_node_type_is_rejected_before_it_is_indexed() {
        let tree = NativeTree { revision: 1, children: vec![node(9, "")] };
        assert_eq!(index_tree(&tree), Err(ReactHostError::EmptyNodeType(9)));
    }

    #[test]
    fn a_text_node_must_carry_text_and_no_children() {
        let tree = NativeTree { revision: 1, children: vec![node(1, "#text")] };
        assert_eq!(index_tree(&tree), Err(ReactHostError::MissingText(1)));

        let mut parent = node(2, "#text");
        parent.text = Some("hi".into());
        parent.children = vec![node(3, "#text")];
        let tree = NativeTree { revision: 1, children: vec![parent] };
        assert_eq!(index_tree(&tree), Err(ReactHostError::UnexpectedText(2)));
    }

    #[test]
    fn only_a_text_node_may_carry_text() {
        let mut view = node(1, "view");
        view.text = Some("stray".into());
        let tree = NativeTree { revision: 1, children: vec![view] };
        assert_eq!(index_tree(&tree), Err(ReactHostError::UnexpectedText(1)));
    }
}

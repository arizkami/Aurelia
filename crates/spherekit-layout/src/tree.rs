//! The retained layout tree.
//!
//! [`LayoutTree`] owns node identity, structure, styles, dirty state, scroll
//! offsets and computed geometry. It does not know how to lay anything out —
//! that is [`LayoutEngine`](crate::LayoutEngine)'s job — which is what lets the
//! backend be swapped without disturbing anything that holds a [`NodeId`].
//!
//! ## Identity
//!
//! Nodes are keyed by [`spherekit_core::NodeId`], a generational handle. A handle to
//! a removed node never resolves, and never silently aliases whatever was
//! inserted into the reused slot afterwards. That is what makes it safe for a
//! widget, an animation and a focus ring to hold ids across a rebuild.
//!
//! ## Two coordinate spaces
//!
//! Every node stores its border box twice: [`ComputedLayout::bounds`] is relative
//! to the parent's border box, which is what the layout algorithm produces, and
//! [`ComputedLayout::absolute_bounds`] is relative to the viewport, which is what
//! hit testing, clipping and painting all want. Deriving the second from the
//! first on every query would mean walking to the root on every mouse move, so it
//! is computed once per layout pass and refreshed in place when a scroll offset
//! changes.

use core::sync::atomic::{AtomicU64, Ordering};

use rustc_hash::FxHashSet;
use smallvec::SmallVec;
use spherekit_core::{Edges, GenerationalStore, LayoutError, NodeId, Point, Px, Rect, Size, size};

use crate::dirty::DirtyFlags;
use crate::style::Style;

/// The geometry the layout pass produced for one node.
///
/// Everything is in logical pixels ([`Px`]). Values are deliberately *not*
/// rounded to whole pixels here: rounding belongs in device space, after the
/// scale factor has been applied, because rounding a logical coordinate on a
/// 125 % display quantises to 0.8 device pixels and produces seams between
/// adjacent boxes.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ComputedLayout {
    /// Border box relative to the parent's border box origin.
    pub bounds: Rect<Px>,
    /// Border box in viewport coordinates, with every ancestor's scroll offset
    /// already applied.
    pub absolute_bounds: Rect<Px>,
    /// Size of the node's content, which exceeds `bounds` when content overflows.
    /// This is what a scroll range is derived from.
    pub content_size: Size<Px>,
    /// Resolved border thickness per side.
    pub border: Edges<Px>,
    /// Resolved padding per side.
    pub padding: Edges<Px>,
    /// Resolved margin per side, after `Auto` margins have absorbed free space.
    pub margin: Edges<Px>,
    /// Paint order assigned by the layout algorithm; higher paints later.
    ///
    /// This is the algorithm's own ordering (absolutely positioned items sort
    /// above in-flow ones, for instance); [`Style::z_index`] is applied on top of
    /// it by the hit tester and the painter.
    pub order: u32,
}

impl ComputedLayout {
    /// A degenerate layout at the origin, used before the first pass.
    pub const ZERO: Self = Self {
        bounds: Rect::ZERO,
        absolute_bounds: Rect::ZERO,
        content_size: Size::ZERO,
        border: Edges::ZERO,
        padding: Edges::ZERO,
        margin: Edges::ZERO,
        order: 0,
    };

    /// Absolute padding box: the border box inset by the border.
    ///
    /// This is the rectangle a clipping node clips its children to, matching CSS,
    /// so that a border is never painted over by overflowing content.
    #[inline]
    pub fn padding_box(&self) -> Rect<Px> {
        self.absolute_bounds.inset(self.border)
    }

    /// Absolute content box: the border box inset by border and padding.
    #[inline]
    pub fn content_box(&self) -> Rect<Px> {
        self.absolute_bounds.inset(Edges {
            top: self.border.top + self.padding.top,
            right: self.border.right + self.padding.right,
            bottom: self.border.bottom + self.padding.bottom,
            left: self.border.left + self.padding.left,
        })
    }

    /// Size of the visible area available to children: the padding box extent.
    #[inline]
    pub fn client_size(&self) -> Size<Px> {
        self.padding_box().size
    }

    /// The largest scroll offset that still shows content, never negative.
    #[inline]
    pub fn max_scroll(&self) -> Size<Px> {
        let client = self.client_size();
        size(
            (self.content_size.width - client.width).max(Px::ZERO),
            (self.content_size.height - client.height).max(Px::ZERO),
        )
    }
}

/// One node's retained state.
///
/// Crate-visible rather than public: the backend needs direct field access to
/// mirror the tree efficiently, but nothing outside the crate should depend on
/// the storage layout.
#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub(crate) style: Style,
    pub(crate) parent: Option<NodeId>,
    /// Four inline slots covers rows, columns and most containers in an audio UI
    /// without touching the allocator.
    pub(crate) children: SmallVec<[NodeId; 4]>,
    pub(crate) layout: ComputedLayout,
    pub(crate) dirty: DirtyFlags,
    /// This node or one of its descendants needs the layout algorithm to run.
    pub(crate) subtree_layout: bool,
    /// This node or one of its descendants needs to be painted again.
    pub(crate) subtree_paint: bool,
    pub(crate) scroll: Size<Px>,
}

/// Hands out the process-unique identity returned by [`LayoutTree::id`].
static NEXT_TREE_ID: AtomicU64 = AtomicU64::new(1);

/// A retained tree of layout nodes with stable identity.
///
/// See the [module documentation](self) for the identity and coordinate-space
/// invariants.
#[derive(Debug)]
pub struct LayoutTree {
    /// Process-unique, so a backend caching for one tree cannot mistake another
    /// for it.
    id: u64,
    nodes: GenerationalStore<NodeId, Node>,
    roots: Vec<NodeId>,
    /// Bumped whenever the parent/child structure changes, so a backend can tell
    /// "restyle" from "rebuild my mirrored tree" without diffing anything.
    structure_epoch: u64,
    /// Reusable traversal buffer. Layout, scrolling and dirty clearing all walk
    /// the tree iteratively; sharing one buffer keeps those walks allocation-free
    /// after the first frame.
    scratch: Vec<(NodeId, Point<Px>)>,
    /// Reusable buffer for the bottom-up dirty summary recomputation.
    order_scratch: Vec<NodeId>,
}

impl Default for LayoutTree {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutTree {
    /// An empty tree.
    pub fn new() -> Self {
        Self {
            id: NEXT_TREE_ID.fetch_add(1, Ordering::Relaxed),
            nodes: GenerationalStore::new(),
            roots: Vec::new(),
            structure_epoch: 0,
            scratch: Vec::new(),
            order_scratch: Vec::new(),
        }
    }

    /// An empty tree with room for `capacity` nodes.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            id: NEXT_TREE_ID.fetch_add(1, Ordering::Relaxed),
            nodes: GenerationalStore::with_capacity(capacity),
            roots: Vec::new(),
            structure_epoch: 0,
            scratch: Vec::new(),
            order_scratch: Vec::new(),
        }
    }

    // ---------------------------------------------------------------- queries

    /// Number of live nodes.
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when the tree holds no nodes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// True when `id` still refers to a live node.
    #[inline]
    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains(id)
    }

    /// The parentless nodes, in the order they will be laid out and painted.
    ///
    /// A tree may legitimately have several: a window's content and a floating
    /// menu are separate roots that share one hit-test and one layout pass.
    #[inline]
    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }

    /// The node's style, or `None` for a stale handle.
    #[inline]
    pub fn style(&self, id: NodeId) -> Option<&Style> {
        self.nodes.get(id).map(|n| &n.style)
    }

    /// The node's computed geometry, or `None` for a stale handle.
    ///
    /// Before the first successful [`LayoutEngine::compute`](crate::LayoutEngine::compute)
    /// this is [`ComputedLayout::ZERO`] rather than `None`: the node exists, it
    /// simply has no geometry yet.
    #[inline]
    pub fn layout(&self, id: NodeId) -> Option<&ComputedLayout> {
        self.nodes.get(id).map(|n| &n.layout)
    }

    /// The node's parent, or `None` for a root or a stale handle.
    #[inline]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(id).and_then(|n| n.parent)
    }

    /// The node's children in document order. Empty for a leaf or a stale handle.
    #[inline]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        self.nodes.get(id).map_or(&[], |n| n.children.as_slice())
    }

    /// Walks from `id`'s parent to its root.
    pub fn ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors { tree: self, cursor: self.parent(id) }
    }

    /// True when `ancestor` is `id` itself or one of its ancestors.
    pub fn is_ancestor_of(&self, ancestor: NodeId, id: NodeId) -> bool {
        if ancestor == id {
            return self.contains(id);
        }
        self.ancestors(id).any(|a| a == ancestor)
    }

    /// A process-unique identity for this tree.
    ///
    /// An engine caches per-node state keyed on structure and style. Two trees
    /// can easily share a [`LayoutTree::structure_epoch`] by coincidence, so a
    /// backend must compare this as well before it trusts anything it cached.
    #[inline]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// A counter that changes whenever the parent/child structure changes.
    ///
    /// Exposed so a backend, a debug overlay or a serialiser can cache anything
    /// keyed on structure and revalidate with a single integer comparison.
    #[inline]
    pub fn structure_epoch(&self) -> u64 {
        self.structure_epoch
    }

    /// Visits `id` and every descendant in depth-first pre-order, iteratively.
    ///
    /// Iterative on purpose: a deeply nested tree must not be able to blow the
    /// stack, and a plug-in host gives you whatever stack it feels like.
    pub fn for_each_in_subtree(&self, id: NodeId, mut f: impl FnMut(NodeId, usize)) {
        let mut stack: Vec<(NodeId, usize)> = Vec::new();
        if self.contains(id) {
            stack.push((id, 0));
        }
        while let Some((node, depth)) = stack.pop() {
            f(node, depth);
            let children = self.children(node);
            // Reversed so that popping yields document order.
            for &c in children.iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }

    // ---------------------------------------------------------------- structure

    /// Inserts a detached node and returns its handle.
    ///
    /// The node starts as a root; attaching it with [`LayoutTree::set_children`]
    /// or [`LayoutTree::reparent`] removes it from the root list.
    pub fn insert(&mut self, style: Style) -> NodeId {
        let id = self.nodes.insert(Node {
            style,
            parent: None,
            children: SmallVec::new(),
            layout: ComputedLayout::ZERO,
            // A brand new node has never been laid out, so it is born dirty.
            dirty: DirtyFlags::STYLE | DirtyFlags::PAINT,
            subtree_layout: true,
            subtree_paint: true,
            scroll: Size::ZERO,
        });
        self.roots.push(id);
        self.structure_epoch += 1;
        id
    }

    /// Inserts a node and appends it to `parent`'s children.
    pub fn insert_child(&mut self, parent: NodeId, style: Style) -> Result<NodeId, LayoutError> {
        if !self.contains(parent) {
            return Err(LayoutError::NodeNotFound(parent));
        }
        let id = self.insert(style);
        self.reparent(id, parent, usize::MAX)?;
        Ok(id)
    }

    /// Replaces `id`'s style, marking only what actually changed.
    ///
    /// Setting a style equal to the current one is free and marks nothing, which
    /// is what makes an immediate-mode-style "rebuild the whole tree every frame"
    /// caller cheap.
    pub fn set_style(&mut self, id: NodeId, style: Style) -> Result<(), LayoutError> {
        let node = self.nodes.get_mut(id).ok_or(LayoutError::NodeNotFound(id))?;
        let diff = node.style.diff(&style);
        if diff.is_empty() {
            return Ok(());
        }
        node.style = style;
        self.mark_dirty(id, diff);
        Ok(())
    }

    /// Mutably borrows a style, marking the node dirty according to what changed.
    ///
    /// The old style is cloned so the diff can be taken afterwards; prefer
    /// [`LayoutTree::set_style`] when you already have the new value.
    pub fn update_style<R>(
        &mut self,
        id: NodeId,
        f: impl FnOnce(&mut Style) -> R,
    ) -> Result<R, LayoutError> {
        let node = self.nodes.get_mut(id).ok_or(LayoutError::NodeNotFound(id))?;
        let before = node.style.clone();
        let out = f(&mut node.style);
        let diff = before.diff(&node.style);
        if !diff.is_empty() {
            self.mark_dirty(id, diff);
        }
        Ok(out)
    }

    /// Replaces `parent`'s child list.
    ///
    /// Children that were attached and are not in the new list become roots
    /// again rather than being deleted, because a retained tree routinely detaches
    /// a subtree for a frame and reattaches it. Use [`LayoutTree::remove`] to
    /// actually destroy nodes.
    ///
    /// Fails with [`LayoutError::Cycle`] if any of `children` is `parent` itself
    /// or one of its ancestors.
    pub fn set_children(&mut self, parent: NodeId, children: &[NodeId]) -> Result<(), LayoutError> {
        if !self.contains(parent) {
            return Err(LayoutError::NodeNotFound(parent));
        }
        self.validate_children(parent, children)?;

        // Rebuilding a tree usually re-sets an identical child list; detecting
        // that here keeps the common case free of structural invalidation.
        if self.children(parent) == children {
            return Ok(());
        }

        let old: SmallVec<[NodeId; 4]> = SmallVec::from_slice(self.children(parent));
        if let Some(node) = self.nodes.get_mut(parent) {
            node.children.clear();
        }

        for &child in children {
            self.unlink(child);
            if let Some(node) = self.nodes.get_mut(child) {
                node.parent = Some(parent);
            }
        }

        let incoming: FxHashSet<NodeId> = children.iter().copied().collect();
        for &child in &old {
            if incoming.contains(&child) {
                continue;
            }
            // Still ours (`unlink` above only touched incoming children), so just
            // promote it to a root.
            if let Some(node) = self.nodes.get_mut(child) {
                node.parent = None;
            }
            if !self.roots.contains(&child) {
                self.roots.push(child);
            }
            self.mark_dirty(child, DirtyFlags::LAYOUT | DirtyFlags::PAINT);
        }

        if let Some(node) = self.nodes.get_mut(parent) {
            node.children.extend_from_slice(children);
        }
        self.structure_epoch += 1;
        self.mark_dirty(parent, DirtyFlags::CHILDREN | DirtyFlags::PAINT);
        for &child in children {
            self.mark_dirty(child, DirtyFlags::LAYOUT | DirtyFlags::PAINT);
        }
        Ok(())
    }

    /// Moves `child` under `new_parent` at `index`, clamped to the child count.
    ///
    /// Pass `usize::MAX` to append. Detaching and reattaching in one call keeps
    /// the node's identity, style, scroll offset and computed geometry intact,
    /// which is the whole point of a retained tree.
    pub fn reparent(
        &mut self,
        child: NodeId,
        new_parent: NodeId,
        index: usize,
    ) -> Result<(), LayoutError> {
        if !self.contains(child) {
            return Err(LayoutError::NodeNotFound(child));
        }
        if !self.contains(new_parent) {
            return Err(LayoutError::NodeNotFound(new_parent));
        }
        if self.is_ancestor_of(child, new_parent) {
            return Err(LayoutError::Cycle(child));
        }

        let old_parent = self.parent(child);
        self.unlink(child);
        let at = {
            let node =
                self.nodes.get_mut(new_parent).ok_or(LayoutError::NodeNotFound(new_parent))?;
            let at = index.min(node.children.len());
            node.children.insert(at, child);
            at
        };
        debug_assert!(self.children(new_parent)[at] == child);
        if let Some(node) = self.nodes.get_mut(child) {
            node.parent = Some(new_parent);
        }
        self.structure_epoch += 1;
        if let Some(old) = old_parent {
            self.mark_dirty(old, DirtyFlags::CHILDREN | DirtyFlags::PAINT);
        }
        self.mark_dirty(new_parent, DirtyFlags::CHILDREN | DirtyFlags::PAINT);
        self.mark_dirty(child, DirtyFlags::LAYOUT | DirtyFlags::PAINT);
        Ok(())
    }

    /// Appends `child` to `parent`'s children.
    #[inline]
    pub fn add_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), LayoutError> {
        self.reparent(child, parent, usize::MAX)
    }

    /// Detaches `id` from its parent, making it a root. Does not delete it.
    pub fn detach(&mut self, id: NodeId) -> Result<(), LayoutError> {
        if !self.contains(id) {
            return Err(LayoutError::NodeNotFound(id));
        }
        if self.parent(id).is_none() {
            return Ok(());
        }
        self.unlink(id);
        if let Some(node) = self.nodes.get_mut(id) {
            node.parent = None;
        }
        self.roots.push(id);
        self.structure_epoch += 1;
        self.mark_dirty(id, DirtyFlags::LAYOUT | DirtyFlags::PAINT);
        Ok(())
    }

    /// Removes `id` and its entire subtree, returning how many nodes went away.
    ///
    /// The walk is iterative so that deleting a deep subtree cannot overflow the
    /// stack.
    pub fn remove(&mut self, id: NodeId) -> Result<usize, LayoutError> {
        if !self.contains(id) {
            return Err(LayoutError::NodeNotFound(id));
        }
        let parent = self.parent(id);
        self.unlink(id);

        let mut doomed: Vec<NodeId> = Vec::new();
        self.for_each_in_subtree(id, |node, _| doomed.push(node));
        let removed = doomed.len();
        for node in doomed {
            self.nodes.remove(node);
        }
        self.roots.retain(|r| *r != id);
        self.structure_epoch += 1;
        if let Some(parent) = parent {
            self.mark_dirty(parent, DirtyFlags::CHILDREN | DirtyFlags::PAINT);
        }
        Ok(removed)
    }

    // ---------------------------------------------------------------- dirtying

    /// Records `flags` on `id` and propagates the summaries to its ancestors.
    ///
    /// Siblings and descendants are never touched. A stale handle is ignored
    /// rather than reported: invalidation arrives from animations and input
    /// handlers that legitimately race with a node being removed, and turning
    /// that into an error would push a `let _ =` onto every call site.
    pub fn mark_dirty(&mut self, id: NodeId, flags: DirtyFlags) {
        if flags.is_empty() {
            return;
        }
        let Some(node) = self.nodes.get_mut(id) else { return };
        node.dirty |= flags;
        node.subtree_paint = true;
        if flags.needs_layout() {
            node.subtree_layout = true;
        }
        self.propagate_summaries(id);
    }

    /// The flags recorded on `id` itself. [`DirtyFlags::NONE`] for a stale handle.
    #[inline]
    pub fn dirty(&self, id: NodeId) -> DirtyFlags {
        self.nodes.get(id).map_or(DirtyFlags::NONE, |n| n.dirty)
    }

    /// True when `id` or one of its descendants needs the layout algorithm.
    #[inline]
    pub fn subtree_layout_dirty(&self, id: NodeId) -> bool {
        self.nodes.get(id).is_some_and(|n| n.subtree_layout)
    }

    /// True when `id` or one of its descendants needs repainting.
    #[inline]
    pub fn subtree_paint_dirty(&self, id: NodeId) -> bool {
        self.nodes.get(id).is_some_and(|n| n.subtree_paint)
    }

    /// True when any root's subtree needs the layout algorithm.
    ///
    /// [`LayoutEngine::compute`](crate::LayoutEngine::compute) consults this first
    /// and returns immediately when it is false; that early exit is what makes a
    /// paint-only frame cost nothing.
    pub fn needs_layout(&self) -> bool {
        self.roots.iter().any(|r| self.subtree_layout_dirty(*r))
    }

    /// True when any root's subtree needs repainting.
    pub fn needs_paint(&self) -> bool {
        self.roots.iter().any(|r| self.subtree_paint_dirty(*r))
    }

    /// Clears the layout-affecting flags across the tree.
    ///
    /// Called by the engine after a successful pass.
    pub fn clear_layout_dirty(&mut self) {
        self.clear_dirty_flags(DirtyFlags::LAYOUT_AFFECTING);
    }

    /// Clears the paint-affecting flags across the tree.
    ///
    /// Called by the renderer after it has consumed the frame.
    pub fn clear_paint_dirty(&mut self) {
        self.clear_dirty_flags(DirtyFlags::PAINT | DirtyFlags::TRANSFORM);
    }

    /// Clears every flag on every node.
    pub fn clear_all_dirty(&mut self) {
        self.clear_dirty_flags(DirtyFlags::all());
    }

    // ---------------------------------------------------------------- scrolling

    /// The node's current scroll offset. Zero for a stale handle.
    #[inline]
    pub fn scroll_offset(&self, id: NodeId) -> Size<Px> {
        self.nodes.get(id).map_or(Size::ZERO, |n| n.scroll)
    }

    /// The largest offset `id` can be scrolled to, given its last layout.
    #[inline]
    pub fn max_scroll_offset(&self, id: NodeId) -> Size<Px> {
        self.nodes.get(id).map_or(Size::ZERO, |n| n.layout.max_scroll())
    }

    /// Scrolls `id`'s content, clamped to `0..=max_scroll_offset`.
    ///
    /// Scrolling shifts descendants' absolute rects and therefore hit testing,
    /// but it deliberately does **not** invalidate layout: the boxes have not
    /// moved relative to their parent, only the window onto them has. A scroll
    /// therefore costs one subtree walk over absolute rects and a repaint, never
    /// a relayout.
    pub fn set_scroll_offset(&mut self, id: NodeId, offset: Size<Px>) -> Result<(), LayoutError> {
        let node = self.nodes.get(id).ok_or(LayoutError::NodeNotFound(id))?;
        let max = node.layout.max_scroll();
        let clamped = size(clamp_px(offset.width, max.width), clamp_px(offset.height, max.height));
        if clamped == node.scroll {
            return Ok(());
        }
        if let Some(node) = self.nodes.get_mut(id) {
            node.scroll = clamped;
        }
        self.mark_dirty(id, DirtyFlags::PAINT);
        self.refresh_absolute_from(id);
        Ok(())
    }

    /// Adds `delta` to the current scroll offset, with the same clamping.
    pub fn scroll_by(&mut self, id: NodeId, delta: Size<Px>) -> Result<(), LayoutError> {
        let current = self.nodes.get(id).ok_or(LayoutError::NodeNotFound(id))?.scroll;
        self.set_scroll_offset(id, current + delta)
    }

    // ---------------------------------------------------------------- internals

    /// Recomputes absolute rects for every root's subtree.
    pub(crate) fn refresh_absolute(&mut self) {
        let mut scratch = core::mem::take(&mut self.scratch);
        scratch.clear();
        for i in 0..self.roots.len() {
            scratch.push((self.roots[i], Point::ZERO));
        }
        self.walk_absolute(&mut scratch);
        self.scratch = scratch;
    }

    /// Recomputes absolute rects for `id`'s subtree, keeping `id` itself anchored
    /// where its parent already put it.
    pub(crate) fn refresh_absolute_from(&mut self, id: NodeId) {
        let origin = match self.parent(id) {
            Some(parent) => match self.nodes.get(parent) {
                Some(p) => p.layout.absolute_bounds.origin - p.scroll,
                None => Point::ZERO,
            },
            None => Point::ZERO,
        };
        let mut scratch = core::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.push((id, origin));
        self.walk_absolute(&mut scratch);
        self.scratch = scratch;
    }

    /// Drains `stack`, assigning `absolute_bounds` to each node it reaches.
    fn walk_absolute(&mut self, stack: &mut Vec<(NodeId, Point<Px>)>) {
        while let Some((id, parent_origin)) = stack.pop() {
            let child_origin = {
                let Some(node) = self.nodes.get_mut(id) else { continue };
                let absolute = Rect::new(
                    parent_origin + node.layout.bounds.origin.to_vector(),
                    node.layout.bounds.size,
                );
                node.layout.absolute_bounds = absolute;
                // Content may have shrunk since the offset was set, so re-clamp
                // rather than letting a stale offset scroll into empty space.
                let max = node.layout.max_scroll();
                node.scroll = size(
                    clamp_px(node.scroll.width, max.width),
                    clamp_px(node.scroll.height, max.height),
                );
                absolute.origin - node.scroll
            };
            if let Some(node) = self.nodes.get(id) {
                for &child in node.children.iter() {
                    stack.push((child, child_origin));
                }
            }
        }
    }

    /// Removes `id` from whatever list currently holds it, without changing its
    /// own `parent` field.
    fn unlink(&mut self, id: NodeId) {
        match self.parent(id) {
            Some(parent) => {
                if let Some(node) = self.nodes.get_mut(parent) {
                    node.children.retain(|c| *c != id);
                }
                self.mark_dirty(parent, DirtyFlags::CHILDREN | DirtyFlags::PAINT);
            }
            None => {
                self.roots.retain(|r| *r != id);
            }
        }
    }

    /// Rejects child lists that would break the single-parent tree invariant.
    fn validate_children(&self, parent: NodeId, children: &[NodeId]) -> Result<(), LayoutError> {
        // The ancestor set of `parent` (including `parent`) is exactly the set of
        // nodes that may not appear among its children.
        let mut forbidden: FxHashSet<NodeId> = FxHashSet::default();
        forbidden.insert(parent);
        forbidden.extend(self.ancestors(parent));

        let mut seen: FxHashSet<NodeId> = FxHashSet::default();
        for &child in children {
            if !self.contains(child) {
                return Err(LayoutError::NodeNotFound(child));
            }
            if forbidden.contains(&child) {
                return Err(LayoutError::Cycle(child));
            }
            if !seen.insert(child) {
                return Err(LayoutError::Engine(format!(
                    "{child:?} appears more than once in the child list of {parent:?}"
                )));
            }
        }
        Ok(())
    }

    /// Pushes `id`'s summary bits up the ancestor chain.
    ///
    /// Stops as soon as it reaches an ancestor that already carries them, which
    /// is what keeps repeated invalidation of the same branch O(1) rather than
    /// O(depth) per mark.
    fn propagate_summaries(&mut self, id: NodeId) {
        let Some(node) = self.nodes.get(id) else { return };
        let layout = node.subtree_layout;
        let paint = node.subtree_paint;
        if !layout && !paint {
            return;
        }
        let mut cursor = node.parent;
        while let Some(pid) = cursor {
            let Some(p) = self.nodes.get_mut(pid) else { break };
            let already = (!paint || p.subtree_paint) && (!layout || p.subtree_layout);
            if paint {
                p.subtree_paint = true;
            }
            if layout {
                p.subtree_layout = true;
            }
            cursor = p.parent;
            if already {
                break;
            }
        }
    }

    /// Removes `mask` everywhere and rebuilds both summary bits bottom-up.
    ///
    /// Only branches that are currently paint-dirty are visited. Every kind of
    /// invalidation sets the paint summary, so that is a safe superset of "might
    /// have something to clear".
    fn clear_dirty_flags(&mut self, mask: DirtyFlags) {
        let mut order = core::mem::take(&mut self.order_scratch);
        order.clear();

        // Pre-order over dirty branches only.
        let mut stack: Vec<NodeId> = Vec::new();
        for &root in &self.roots {
            if self.subtree_paint_dirty(root) {
                stack.push(root);
            }
        }
        while let Some(id) = stack.pop() {
            order.push(id);
            if let Some(node) = self.nodes.get(id) {
                for &child in node.children.iter() {
                    if self.subtree_paint_dirty(child) {
                        stack.push(child);
                    }
                }
            }
        }

        // Reverse pre-order visits every descendant before its ancestor, which is
        // exactly what recomputing the summaries needs.
        for i in (0..order.len()).rev() {
            let id = order[i];
            let (layout, paint) = {
                let Some(node) = self.nodes.get(id) else { continue };
                let mut dirty = node.dirty;
                dirty.remove(mask);
                let mut layout = dirty.needs_layout();
                let mut paint = dirty.needs_paint();
                for &child in node.children.iter() {
                    if let Some(c) = self.nodes.get(child) {
                        layout |= c.subtree_layout;
                        paint |= c.subtree_paint;
                    }
                }
                (layout, paint)
            };
            if let Some(node) = self.nodes.get_mut(id) {
                node.dirty.remove(mask);
                node.subtree_layout = layout;
                node.subtree_paint = paint;
            }
        }

        self.order_scratch = order;
    }

    /// Crate-internal borrow used by the backend when mirroring the tree.
    #[inline]
    pub(crate) fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Crate-internal write-back of one node's geometry from the backend.
    #[inline]
    pub(crate) fn write_layout(&mut self, id: NodeId, layout: ComputedLayout) {
        if let Some(node) = self.nodes.get_mut(id) {
            // `absolute_bounds` is filled in by the separate absolute pass, so it
            // is carried over here rather than being clobbered with a relative
            // rectangle that would be briefly, subtly wrong.
            let absolute = node.layout.absolute_bounds;
            node.layout = ComputedLayout { absolute_bounds: absolute, ..layout };
        }
    }
}

#[cfg(test)]
impl LayoutTree {
    /// Installs a hand-written layout, so hit testing and scrolling can be
    /// exercised without involving the layout algorithm at all.
    pub(crate) fn force_layout_for_test(&mut self, id: NodeId, layout: ComputedLayout) {
        if let Some(node) = self.nodes.get_mut(id) {
            node.layout = layout;
        }
    }

    /// Runs the absolute-rectangle pass on its own.
    pub(crate) fn refresh_absolute_for_test(&mut self) {
        self.refresh_absolute();
    }
}

/// Clamps a scroll component into `0..=max`.
#[inline]
fn clamp_px(v: Px, max: Px) -> Px {
    if !v.is_finite() { Px::ZERO } else { v.clamp(Px::ZERO, max.max(Px::ZERO)) }
}

/// Iterator over a node's ancestors, nearest first.
#[derive(Debug)]
pub struct Ancestors<'a> {
    tree: &'a LayoutTree,
    cursor: Option<NodeId>,
}

impl Iterator for Ancestors<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let current = self.cursor?;
        self.cursor = self.tree.parent(current);
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::px;

    fn tree_with_chain(depth: usize) -> (LayoutTree, Vec<NodeId>) {
        let mut tree = LayoutTree::new();
        let mut ids = Vec::with_capacity(depth);
        let root = tree.insert(Style::column());
        ids.push(root);
        for _ in 1..depth {
            let parent = *ids.last().unwrap();
            ids.push(tree.insert_child(parent, Style::column()).unwrap());
        }
        (tree, ids)
    }

    #[test]
    fn empty_tree_answers_every_query_without_panicking() {
        let tree = LayoutTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert!(tree.roots().is_empty());
        assert!(!tree.needs_layout());
        assert!(!tree.needs_paint());
    }

    #[test]
    fn stale_handles_never_resolve_after_removal() {
        let mut tree = LayoutTree::new();
        let a = tree.insert(Style::DEFAULT);
        assert_eq!(tree.remove(a).unwrap(), 1);
        let b = tree.insert(Style::DEFAULT);
        assert_eq!(a.index(), b.index(), "expected the slot to be reused");
        assert!(!tree.contains(a));
        assert!(tree.style(a).is_none());
        assert_eq!(tree.dirty(a), DirtyFlags::NONE);
        assert!(tree.contains(b));
    }

    #[test]
    fn insert_child_attaches_and_removes_from_roots() {
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let child = tree.insert_child(root, Style::row()).unwrap();
        assert_eq!(tree.roots(), &[root]);
        assert_eq!(tree.children(root), &[child]);
        assert_eq!(tree.parent(child), Some(root));
    }

    #[test]
    fn set_children_detaches_dropped_children_as_roots() {
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let a = tree.insert(Style::row());
        let b = tree.insert(Style::row());
        tree.set_children(root, &[a, b]).unwrap();
        assert_eq!(tree.roots(), &[root]);

        tree.set_children(root, &[a]).unwrap();
        assert_eq!(tree.children(root), &[a]);
        assert_eq!(tree.parent(b), None);
        assert!(tree.roots().contains(&b), "dropped child should survive as a root");
    }

    #[test]
    fn set_children_with_identical_list_is_a_no_op() {
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let a = tree.insert_child(root, Style::row()).unwrap();
        tree.clear_all_dirty();
        let epoch = tree.structure_epoch();

        tree.set_children(root, &[a]).unwrap();
        assert_eq!(tree.structure_epoch(), epoch, "no structural change should be recorded");
        assert_eq!(tree.dirty(root), DirtyFlags::NONE);
    }

    #[test]
    fn set_children_rejects_cycles_and_duplicates() {
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let child = tree.insert_child(root, Style::row()).unwrap();
        let other = tree.insert(Style::row());

        assert!(matches!(tree.set_children(child, &[root]), Err(LayoutError::Cycle(_))));
        assert!(matches!(tree.set_children(root, &[root]), Err(LayoutError::Cycle(_))));
        assert!(matches!(tree.set_children(root, &[other, other]), Err(LayoutError::Engine(_))));
        // The failed calls must not have mutated anything.
        assert_eq!(tree.children(root), &[child]);
        assert_eq!(tree.children(child), &[] as &[NodeId]);
    }

    #[test]
    fn set_children_rejects_stale_children() {
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let ghost = tree.insert(Style::row());
        tree.remove(ghost).unwrap();
        assert!(matches!(tree.set_children(root, &[ghost]), Err(LayoutError::NodeNotFound(_))));
    }

    #[test]
    fn reparent_preserves_identity_and_rejects_self_ancestry() {
        let mut tree = LayoutTree::new();
        let a = tree.insert(Style::row());
        let b = tree.insert_child(a, Style::row()).unwrap();
        let c = tree.insert_child(b, Style::row()).unwrap();

        assert!(matches!(tree.reparent(a, c, 0), Err(LayoutError::Cycle(_))));

        let other = tree.insert(Style::row());
        tree.reparent(b, other, 0).unwrap();
        assert_eq!(tree.parent(b), Some(other));
        assert_eq!(tree.children(a), &[] as &[NodeId]);
        // The grandchild travelled with its parent.
        assert_eq!(tree.parent(c), Some(b));
    }

    #[test]
    fn reparent_index_is_clamped() {
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let a = tree.insert_child(root, Style::row()).unwrap();
        let b = tree.insert(Style::row());
        tree.reparent(b, root, usize::MAX).unwrap();
        assert_eq!(tree.children(root), &[a, b]);

        let c = tree.insert(Style::row());
        tree.reparent(c, root, 0).unwrap();
        assert_eq!(tree.children(root), &[c, a, b]);
    }

    #[test]
    fn remove_deletes_the_whole_subtree() {
        let (mut tree, ids) = tree_with_chain(5);
        let removed = tree.remove(ids[1]).unwrap();
        assert_eq!(removed, 4);
        assert!(tree.contains(ids[0]));
        for id in &ids[1..] {
            assert!(!tree.contains(*id));
        }
        assert_eq!(tree.children(ids[0]), &[] as &[NodeId]);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn removing_a_root_drops_it_from_the_root_list() {
        let mut tree = LayoutTree::new();
        let a = tree.insert(Style::row());
        let b = tree.insert(Style::row());
        tree.remove(a).unwrap();
        assert_eq!(tree.roots(), &[b]);
    }

    #[test]
    fn layout_dirty_marks_ancestors_but_not_siblings() {
        // root -> [branch_a -> leaf_a, branch_b -> leaf_b]
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let branch_a = tree.insert_child(root, Style::column()).unwrap();
        let leaf_a = tree.insert_child(branch_a, Style::row()).unwrap();
        let branch_b = tree.insert_child(root, Style::column()).unwrap();
        let leaf_b = tree.insert_child(branch_b, Style::row()).unwrap();
        tree.clear_all_dirty();

        tree.mark_dirty(leaf_a, DirtyFlags::LAYOUT);

        assert_eq!(tree.dirty(leaf_a), DirtyFlags::LAYOUT);
        // Ancestors are summarised, never flagged themselves.
        assert_eq!(tree.dirty(branch_a), DirtyFlags::NONE);
        assert_eq!(tree.dirty(root), DirtyFlags::NONE);
        assert!(tree.subtree_layout_dirty(branch_a));
        assert!(tree.subtree_layout_dirty(root));
        // The other half of the tree is untouched.
        assert_eq!(tree.dirty(branch_b), DirtyFlags::NONE);
        assert_eq!(tree.dirty(leaf_b), DirtyFlags::NONE);
        assert!(!tree.subtree_layout_dirty(branch_b));
        assert!(!tree.subtree_layout_dirty(leaf_b));
    }

    #[test]
    fn paint_dirty_never_sets_a_layout_summary() {
        let (mut tree, ids) = tree_with_chain(6);
        tree.clear_all_dirty();
        tree.mark_dirty(*ids.last().unwrap(), DirtyFlags::PAINT);

        assert!(!tree.needs_layout(), "a repaint must not schedule layout");
        assert!(tree.needs_paint());
        for id in &ids {
            assert!(!tree.subtree_layout_dirty(*id));
            assert!(tree.subtree_paint_dirty(*id));
        }
    }

    #[test]
    fn transform_dirty_never_sets_a_layout_summary() {
        let (mut tree, ids) = tree_with_chain(4);
        tree.clear_all_dirty();
        tree.mark_dirty(ids[3], DirtyFlags::TRANSFORM);
        assert!(!tree.needs_layout());
        assert!(tree.needs_paint());
    }

    #[test]
    fn clearing_layout_dirty_leaves_paint_dirty_alone() {
        let (mut tree, ids) = tree_with_chain(4);
        tree.clear_all_dirty();
        tree.mark_dirty(ids[3], DirtyFlags::LAYOUT | DirtyFlags::PAINT);
        tree.clear_layout_dirty();

        assert!(!tree.needs_layout());
        assert!(tree.needs_paint());
        assert_eq!(tree.dirty(ids[3]), DirtyFlags::PAINT);

        tree.clear_paint_dirty();
        assert!(!tree.needs_paint());
        assert_eq!(tree.dirty(ids[3]), DirtyFlags::NONE);
    }

    #[test]
    fn summaries_are_recomputed_from_siblings_not_guessed() {
        // Two dirty leaves under one parent: clearing one must leave the parent's
        // summary set because of the other.
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let a = tree.insert_child(root, Style::row()).unwrap();
        let b = tree.insert_child(root, Style::row()).unwrap();
        tree.clear_all_dirty();

        tree.mark_dirty(a, DirtyFlags::LAYOUT);
        tree.mark_dirty(b, DirtyFlags::PAINT);
        tree.clear_layout_dirty();

        assert!(!tree.subtree_layout_dirty(root));
        assert!(tree.subtree_paint_dirty(root), "b is still paint-dirty");
        assert_eq!(tree.dirty(b), DirtyFlags::PAINT);
    }

    #[test]
    fn set_style_marks_only_what_changed() {
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let child = tree.insert_child(root, Style::row()).unwrap();
        tree.clear_all_dirty();

        // Paint-only edit.
        tree.set_style(child, Style::row().with_opacity(0.5)).unwrap();
        assert_eq!(tree.dirty(child), DirtyFlags::PAINT);
        assert!(!tree.needs_layout());

        // Identical edit.
        tree.clear_all_dirty();
        tree.set_style(child, Style::row().with_opacity(0.5)).unwrap();
        assert_eq!(tree.dirty(child), DirtyFlags::NONE);

        // Layout edit.
        tree.set_style(child, Style::row().with_opacity(0.5).with_px_size(10.0, 10.0)).unwrap();
        assert!(tree.dirty(child).contains(DirtyFlags::STYLE));
        assert!(tree.needs_layout());
    }

    #[test]
    fn update_style_diffs_the_closure_result() {
        let mut tree = LayoutTree::new();
        let node = tree.insert(Style::row());
        tree.clear_all_dirty();

        tree.update_style(node, |s| s.opacity = 0.25).unwrap();
        assert_eq!(tree.dirty(node), DirtyFlags::PAINT);

        tree.clear_all_dirty();
        tree.update_style(node, |s| s.flex_grow = 1.0).unwrap();
        assert!(tree.dirty(node).needs_layout());
    }

    #[test]
    fn set_style_on_stale_handle_reports_not_found() {
        let mut tree = LayoutTree::new();
        let node = tree.insert(Style::row());
        tree.remove(node).unwrap();
        assert!(matches!(tree.set_style(node, Style::row()), Err(LayoutError::NodeNotFound(_))));
    }

    #[test]
    fn deep_chain_marks_and_clears_without_recursion() {
        // 2000 levels would overflow a recursive implementation long before it
        // overflowed this one.
        let (mut tree, ids) = tree_with_chain(2000);
        tree.clear_all_dirty();
        tree.mark_dirty(*ids.last().unwrap(), DirtyFlags::LAYOUT);
        assert!(tree.needs_layout());
        assert!(tree.subtree_layout_dirty(ids[0]));
        tree.clear_layout_dirty();
        assert!(!tree.needs_layout());

        // ...and removing it iteratively as well.
        assert_eq!(tree.remove(ids[0]).unwrap(), 2000);
        assert!(tree.is_empty());
    }

    #[test]
    fn repeated_marks_on_the_same_branch_short_circuit() {
        let (mut tree, ids) = tree_with_chain(100);
        tree.clear_all_dirty();
        for _ in 0..1000 {
            tree.mark_dirty(*ids.last().unwrap(), DirtyFlags::LAYOUT);
        }
        assert!(tree.subtree_layout_dirty(ids[0]));
    }

    #[test]
    fn for_each_in_subtree_visits_in_document_order() {
        let mut tree = LayoutTree::new();
        let root = tree.insert(Style::row());
        let a = tree.insert_child(root, Style::row()).unwrap();
        let a1 = tree.insert_child(a, Style::row()).unwrap();
        let b = tree.insert_child(root, Style::row()).unwrap();

        let mut seen = Vec::new();
        tree.for_each_in_subtree(root, |id, depth| seen.push((id, depth)));
        assert_eq!(seen, vec![(root, 0), (a, 1), (a1, 2), (b, 1)]);

        // Starting part-way down visits only that subtree...
        let mut seen = Vec::new();
        tree.for_each_in_subtree(a, |id, depth| seen.push((id, depth)));
        assert_eq!(seen, vec![(a, 0), (a1, 1)]);

        // ...and a stale handle visits nothing at all.
        let ghost = tree.insert(Style::row());
        tree.remove(ghost).unwrap();
        let mut count = 0;
        tree.for_each_in_subtree(ghost, |_, _| count += 1);
        assert_eq!(count, 0);
    }

    #[test]
    fn ancestors_walks_to_the_root() {
        let (tree, ids) = tree_with_chain(4);
        let chain: Vec<NodeId> = tree.ancestors(ids[3]).collect();
        assert_eq!(chain, vec![ids[2], ids[1], ids[0]]);
        assert_eq!(tree.ancestors(ids[0]).count(), 0);
    }

    #[test]
    fn max_scroll_never_goes_negative() {
        let layout = ComputedLayout {
            bounds: spherekit_core::rect(px(0.0), px(0.0), px(100.0), px(100.0)),
            absolute_bounds: spherekit_core::rect(px(0.0), px(0.0), px(100.0), px(100.0)),
            content_size: size(px(20.0), px(20.0)),
            ..ComputedLayout::ZERO
        };
        assert_eq!(layout.max_scroll(), size(Px::ZERO, Px::ZERO));
    }

    #[test]
    fn content_box_subtracts_border_and_padding_once() {
        let layout = ComputedLayout {
            bounds: spherekit_core::rect(px(0.0), px(0.0), px(100.0), px(100.0)),
            absolute_bounds: spherekit_core::rect(px(10.0), px(20.0), px(100.0), px(100.0)),
            border: Edges::all(px(2.0)),
            padding: Edges::all(px(5.0)),
            ..ComputedLayout::ZERO
        };
        assert_eq!(
            layout.padding_box(),
            spherekit_core::rect(px(12.0), px(22.0), px(96.0), px(96.0))
        );
        assert_eq!(
            layout.content_box(),
            spherekit_core::rect(px(17.0), px(27.0), px(86.0), px(86.0))
        );
    }
}

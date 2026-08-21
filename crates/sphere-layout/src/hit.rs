//! Hit testing over computed absolute geometry.
//!
//! Hit testing runs on the input thread's latency budget, so it reads only what
//! the layout pass already produced: absolute rectangles, clip rectangles derived
//! from `Overflow`, and `z_index`. It never walks to the root to accumulate
//! offsets, and it never allocates for a subtree narrower than eight children.
//!
//! The descent keeps its own frame stack on the heap. A recursive implementation
//! would be shorter, but a plug-in host gives you whatever stack it feels like
//! and a deeply nested tree must not be able to crash the DAW.

use smallvec::SmallVec;
use sphere_core::{NodeId, Point, Px, Rect};

use crate::tree::LayoutTree;

/// One level of the descent, kept on the heap so depth cannot overflow the stack.
struct Frame {
    /// The node this frame is exploring.
    node: NodeId,
    /// The clip rectangle that applies to this node's *children*.
    clip: Rect<Px>,
    /// Children in bottom-to-top paint order.
    order: SmallVec<[NodeId; 8]>,
    /// How many children have been tried; children are consumed from the top.
    cursor: usize,
}

impl LayoutTree {
    /// The topmost node whose absolute bounds contain `point`.
    ///
    /// Returns `None` when the point lands on nothing, which includes the case of
    /// a point over a node that has been clipped away by a scroll container.
    ///
    /// # The rules, in order
    ///
    /// 1. A [`Display::None`](crate::Display::None) node and its subtree are
    ///    skipped entirely.
    /// 2. A node whose incoming clip does not contain the point is skipped, along
    ///    with its whole subtree. That is what stops a row that has scrolled out
    ///    of a list from swallowing clicks.
    /// 3. Children are tested before the node itself, topmost first, so the
    ///    deepest and highest thing under the cursor wins.
    /// 4. Children are tested even when the point is outside the parent, as long
    ///    as the parent does not clip: an absolutely positioned popup that hangs
    ///    outside its container is still clickable.
    /// 5. A node with [`Overflow::Hidden`](crate::Overflow::Hidden) or
    ///    [`Overflow::Scroll`](crate::Overflow::Scroll) narrows the clip for its
    ///    children to its own padding box, matching where the renderer clips.
    ///
    /// Sibling order is document order adjusted by
    /// [`Style::z_index`](crate::Style::z_index), which is scoped to siblings: it
    /// reorders a node among the things it shares a parent with and nothing else.
    /// Full CSS stacking contexts would let a `z_index` deep in one branch leap
    /// over an unrelated branch, which is precisely what makes CSS z-ordering hard
    /// to reason about.
    ///
    /// [`Style::opacity`](crate::Style::opacity) is ignored: a fully transparent
    /// node still receives input, exactly as in CSS. Use `Display::None` to opt
    /// out of both.
    pub fn hit_test(&self, point: Point<Px>) -> Option<NodeId> {
        let mut chain = Vec::new();
        if self.hit_test_all_into(point, &mut chain) { chain.last().copied() } else { None }
    }

    /// The full chain from a root down to the hit node, root first.
    ///
    /// This is the shape an event system needs: walk the slice forwards for the
    /// capture phase, backwards for the bubble phase. The chain includes
    /// ancestors that do not themselves contain the point — an event on an
    /// overflowing child still bubbles through its parent.
    pub fn hit_test_all(&self, point: Point<Px>) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.hit_test_all_into(point, &mut out);
        out
    }

    /// [`LayoutTree::hit_test_all`] into a caller-owned buffer.
    ///
    /// Returns whether anything was hit. Dispatching input every frame should use
    /// this and keep one buffer alive rather than allocating a `Vec` per event.
    pub fn hit_test_all_into(&self, point: Point<Px>, out: &mut Vec<NodeId>) -> bool {
        out.clear();
        let roots = self.paint_order(self.roots());
        for &root in roots.iter().rev() {
            if self.hit_subtree(root, point, Rect::INFINITE, out) {
                return true;
            }
        }
        false
    }

    /// [`LayoutTree::hit_test`] restricted to one subtree.
    ///
    /// Useful for routing an event to a specific overlay without letting it fall
    /// through to whatever is behind it.
    ///
    /// The search starts unclipped: clipping imposed by ancestors *above* `root`
    /// is not applied, because the caller has already decided this subtree is the
    /// one receiving the event.
    pub fn hit_test_in(&self, root: NodeId, point: Point<Px>) -> Option<NodeId> {
        let mut chain = Vec::new();
        if self.hit_subtree(root, point, Rect::INFINITE, &mut chain) {
            chain.last().copied()
        } else {
            None
        }
    }

    /// Depth-first search of one subtree, topmost child first.
    fn hit_subtree(
        &self,
        root: NodeId,
        point: Point<Px>,
        clip: Rect<Px>,
        out: &mut Vec<NodeId>,
    ) -> bool {
        let mut stack: Vec<Frame> = Vec::new();
        self.push_frame(root, point, clip, &mut stack);

        loop {
            let Some(frame) = stack.last_mut() else { return false };
            if frame.cursor < frame.order.len() {
                // Consume from the end: the last entry paints on top.
                let child = frame.order[frame.order.len() - 1 - frame.cursor];
                frame.cursor += 1;
                let child_clip = frame.clip;
                self.push_frame(child, point, child_clip, &mut stack);
                continue;
            }

            // Nothing below this node was hit, so the node itself is the target
            // if the point is inside its border box.
            let node = frame.node;
            let inside =
                self.layout(node).is_some_and(|layout| layout.absolute_bounds.contains(point));
            if inside {
                out.clear();
                out.extend(stack.iter().map(|f| f.node));
                return true;
            }
            stack.pop();
        }
    }

    /// Pushes `node` as a candidate, or does nothing when it cannot be hit.
    fn push_frame(&self, node: NodeId, point: Point<Px>, clip: Rect<Px>, stack: &mut Vec<Frame>) {
        let Some(style) = self.style(node) else { return };
        if !style.display.is_visible() {
            return;
        }
        // The incoming clip governs this node *and* everything beneath it, so
        // failing it prunes the whole subtree in one comparison.
        if !clip.contains(point) {
            return;
        }
        let child_clip = if style.clips() {
            match self.layout(node) {
                Some(layout) => clip.intersection(layout.padding_box()),
                None => clip,
            }
        } else {
            clip
        };
        stack.push(Frame {
            node,
            clip: child_clip,
            order: self.paint_order(self.children(node)),
            cursor: 0,
        });
    }

    /// `ids` sorted bottom-to-top by `z_index`, keeping document order for ties.
    ///
    /// The scan for a non-zero `z_index` is worth it: almost every container has
    /// none, and skipping the sort keeps the common case free of comparisons.
    fn paint_order(&self, ids: &[NodeId]) -> SmallVec<[NodeId; 8]> {
        let mut order: SmallVec<[NodeId; 8]> = SmallVec::from_slice(ids);
        let needs_sort = ids.iter().any(|id| self.style(*id).is_some_and(|s| s.z_index != 0));
        if needs_sort {
            order.sort_by_key(|id| self.style(*id).map_or(0, |s| s.z_index));
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{Overflow, Style};
    use crate::tree::ComputedLayout;
    use sphere_core::{px, rect, size};

    /// Builds a node with a hand-written absolute rectangle.
    ///
    /// Hit testing consumes computed geometry, so testing it against synthetic
    /// rectangles isolates it from the layout algorithm entirely: a failure here
    /// is a hit-testing bug and never a flexbox bug.
    fn placed(tree: &mut LayoutTree, parent: Option<NodeId>, style: Style, r: Rect<Px>) -> NodeId {
        let id = match parent {
            Some(p) => tree.insert_child(p, style).unwrap(),
            None => tree.insert(style),
        };
        tree.force_layout_for_test(
            id,
            ComputedLayout { bounds: r, absolute_bounds: r, ..ComputedLayout::ZERO },
        );
        id
    }

    #[test]
    fn empty_tree_hits_nothing() {
        let tree = LayoutTree::new();
        assert_eq!(tree.hit_test(Point::new(px(1.0), px(1.0))), None);
        assert!(tree.hit_test_all(Point::new(px(1.0), px(1.0))).is_empty());
    }

    #[test]
    fn point_outside_everything_returns_none() {
        let mut tree = LayoutTree::new();
        placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(10.0), px(10.0)));
        assert_eq!(tree.hit_test(Point::new(px(50.0), px(50.0))), None);
        // Half-open containment: the far edges belong to the next box along.
        assert_eq!(tree.hit_test(Point::new(px(10.0), px(5.0))), None);
        assert!(tree.hit_test(Point::new(px(0.0), px(0.0))).is_some());
    }

    #[test]
    fn deepest_node_wins_over_its_ancestors() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let mid = placed(
            &mut tree,
            Some(root),
            Style::row(),
            rect(px(10.0), px(10.0), px(50.0), px(50.0)),
        );
        let leaf = placed(
            &mut tree,
            Some(mid),
            Style::row(),
            rect(px(20.0), px(20.0), px(10.0), px(10.0)),
        );

        assert_eq!(tree.hit_test(Point::new(px(25.0), px(25.0))), Some(leaf));
        assert_eq!(tree.hit_test_all(Point::new(px(25.0), px(25.0))), vec![root, mid, leaf]);
        // Inside mid but outside leaf.
        assert_eq!(tree.hit_test(Point::new(px(15.0), px(15.0))), Some(mid));
        // Inside root only.
        assert_eq!(tree.hit_test(Point::new(px(90.0), px(90.0))), Some(root));
    }

    #[test]
    fn overlapping_siblings_return_the_later_one() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let under =
            placed(&mut tree, Some(root), Style::row(), rect(px(0.0), px(0.0), px(50.0), px(50.0)));
        let over =
            placed(&mut tree, Some(root), Style::row(), rect(px(0.0), px(0.0), px(50.0), px(50.0)));

        assert_eq!(tree.hit_test(Point::new(px(10.0), px(10.0))), Some(over));
        assert_ne!(tree.hit_test(Point::new(px(10.0), px(10.0))), Some(under));
    }

    #[test]
    fn z_index_overrides_document_order() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let lifted = placed(
            &mut tree,
            Some(root),
            Style::row().with_z_index(10),
            rect(px(0.0), px(0.0), px(50.0), px(50.0)),
        );
        let later =
            placed(&mut tree, Some(root), Style::row(), rect(px(0.0), px(0.0), px(50.0), px(50.0)));

        assert_eq!(tree.hit_test(Point::new(px(10.0), px(10.0))), Some(lifted));

        // Negative z pushes the first sibling below the second.
        tree.set_style(lifted, Style::row().with_z_index(-1)).unwrap();
        assert_eq!(tree.hit_test(Point::new(px(10.0), px(10.0))), Some(later));
    }

    #[test]
    fn z_index_is_scoped_to_siblings() {
        // A deeply nested high z must not jump over an unrelated later branch.
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let left =
            placed(&mut tree, Some(root), Style::row(), rect(px(0.0), px(0.0), px(50.0), px(50.0)));
        let lifted_child = placed(
            &mut tree,
            Some(left),
            Style::row().with_z_index(999),
            rect(px(0.0), px(0.0), px(50.0), px(50.0)),
        );
        let right =
            placed(&mut tree, Some(root), Style::row(), rect(px(0.0), px(0.0), px(50.0), px(50.0)));

        assert_eq!(tree.hit_test(Point::new(px(10.0), px(10.0))), Some(right));
        assert!(!tree.hit_test_all(Point::new(px(10.0), px(10.0))).contains(&lifted_child));
    }

    #[test]
    fn a_child_clipped_out_of_a_scroll_container_is_not_hit() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(200.0), px(200.0)));
        let clipper = placed(
            &mut tree,
            Some(root),
            Style::column().with_overflow(Overflow::Scroll),
            rect(px(0.0), px(0.0), px(100.0), px(50.0)),
        );
        // Second row sits below the clipper's visible area.
        let visible = placed(
            &mut tree,
            Some(clipper),
            Style::row(),
            rect(px(0.0), px(0.0), px(100.0), px(40.0)),
        );
        let hidden = placed(
            &mut tree,
            Some(clipper),
            Style::row(),
            rect(px(0.0), px(60.0), px(100.0), px(40.0)),
        );

        assert_eq!(tree.hit_test(Point::new(px(10.0), px(10.0))), Some(visible));
        // The overflowing row is drawn nowhere, so it is clickable nowhere.
        assert_eq!(tree.hit_test(Point::new(px(10.0), px(70.0))), Some(root));
        assert!(!tree.hit_test_all(Point::new(px(10.0), px(70.0))).contains(&hidden));
    }

    #[test]
    fn overflow_visible_lets_children_escape_and_stay_hittable() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(200.0), px(200.0)));
        let parent =
            placed(&mut tree, Some(root), Style::row(), rect(px(0.0), px(0.0), px(20.0), px(20.0)));
        let escapee = placed(
            &mut tree,
            Some(parent),
            Style::row(),
            rect(px(100.0), px(100.0), px(30.0), px(30.0)),
        );

        assert_eq!(tree.hit_test(Point::new(px(110.0), px(110.0))), Some(escapee));
        // The chain still routes through the geometric parent.
        assert_eq!(
            tree.hit_test_all(Point::new(px(110.0), px(110.0))),
            vec![root, parent, escapee]
        );
    }

    #[test]
    fn clipping_uses_the_padding_box_not_the_border_box() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let clipper =
            tree.insert_child(root, Style::row().with_overflow(Overflow::Hidden)).unwrap();
        tree.force_layout_for_test(
            clipper,
            ComputedLayout {
                bounds: rect(px(0.0), px(0.0), px(50.0), px(50.0)),
                absolute_bounds: rect(px(0.0), px(0.0), px(50.0), px(50.0)),
                border: sphere_core::Edges::all(px(5.0)),
                ..ComputedLayout::ZERO
            },
        );
        let child = placed(
            &mut tree,
            Some(clipper),
            Style::row(),
            rect(px(0.0), px(0.0), px(50.0), px(50.0)),
        );

        // Inside the border ring: the child is clipped out there.
        assert_eq!(tree.hit_test(Point::new(px(2.0), px(2.0))), Some(clipper));
        // Inside the padding box: the child wins.
        assert_eq!(tree.hit_test(Point::new(px(10.0), px(10.0))), Some(child));
    }

    #[test]
    fn hidden_nodes_and_their_children_are_never_hit() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let hidden = placed(
            &mut tree,
            Some(root),
            Style::hidden(),
            rect(px(0.0), px(0.0), px(50.0), px(50.0)),
        );
        let child = placed(
            &mut tree,
            Some(hidden),
            Style::row(),
            rect(px(0.0), px(0.0), px(20.0), px(20.0)),
        );

        let chain = tree.hit_test_all(Point::new(px(5.0), px(5.0)));
        assert_eq!(chain, vec![root]);
        assert!(!chain.contains(&hidden));
        assert!(!chain.contains(&child));
    }

    #[test]
    fn zero_sized_nodes_are_never_hit() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let empty =
            placed(&mut tree, Some(root), Style::row(), rect(px(10.0), px(10.0), px(0.0), px(0.0)));
        assert_eq!(tree.hit_test(Point::new(px(10.0), px(10.0))), Some(root));
        assert!(!tree.hit_test_all(Point::new(px(10.0), px(10.0))).contains(&empty));
    }

    #[test]
    fn later_roots_are_on_top() {
        let mut tree = LayoutTree::new();
        let back =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let front =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        assert_eq!(tree.hit_test(Point::new(px(5.0), px(5.0))), Some(front));

        // ...and `hit_test_in` can force the one behind.
        assert_eq!(tree.hit_test_in(back, Point::new(px(5.0), px(5.0))), Some(back));
    }

    #[test]
    fn scroll_offset_moves_what_gets_hit() {
        let mut tree = LayoutTree::new();
        let list = tree.insert(Style::column().with_overflow(Overflow::Scroll));
        tree.force_layout_for_test(
            list,
            ComputedLayout {
                bounds: rect(px(0.0), px(0.0), px(100.0), px(50.0)),
                absolute_bounds: rect(px(0.0), px(0.0), px(100.0), px(50.0)),
                content_size: size(px(100.0), px(200.0)),
                ..ComputedLayout::ZERO
            },
        );
        let a = placed(
            &mut tree,
            Some(list),
            Style::row(),
            rect(px(0.0), px(0.0), px(100.0), px(40.0)),
        );
        let b = placed(
            &mut tree,
            Some(list),
            Style::row(),
            rect(px(0.0), px(40.0), px(100.0), px(40.0)),
        );
        tree.refresh_absolute_for_test();

        assert_eq!(tree.hit_test(Point::new(px(5.0), px(10.0))), Some(a));
        tree.set_scroll_offset(list, size(Px::ZERO, px(40.0))).unwrap();
        assert_eq!(tree.hit_test(Point::new(px(5.0), px(10.0))), Some(b));
        assert_eq!(tree.scroll_offset(list), size(Px::ZERO, px(40.0)));
    }

    #[test]
    fn deeply_nested_hit_test_does_not_recurse() {
        let mut tree = LayoutTree::new();
        let mut parent: Option<NodeId> = None;
        let mut last = None;
        for _ in 0..3000 {
            let id = placed(
                &mut tree,
                parent,
                Style::row(),
                rect(px(0.0), px(0.0), px(100.0), px(100.0)),
            );
            parent = Some(id);
            last = Some(id);
        }
        assert_eq!(tree.hit_test(Point::new(px(1.0), px(1.0))), last);
        assert_eq!(tree.hit_test_all(Point::new(px(1.0), px(1.0))).len(), 3000);
    }

    #[test]
    fn hit_test_into_reuses_the_buffer() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let mut buf = vec![NodeId::new(9, 9); 8];
        assert!(tree.hit_test_all_into(Point::new(px(1.0), px(1.0)), &mut buf));
        assert_eq!(buf, vec![root]);
        // A miss must leave the buffer empty rather than stale.
        assert!(!tree.hit_test_all_into(Point::new(px(500.0), px(500.0)), &mut buf));
        assert!(buf.is_empty());
    }

    #[test]
    fn many_siblings_with_z_index_sort_stably() {
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        // More than the SmallVec's inline capacity, all overlapping.
        let mut ids = Vec::new();
        for i in 0..32 {
            ids.push(placed(
                &mut tree,
                Some(root),
                Style::row().with_z_index(if i == 3 { 5 } else { 0 }),
                rect(px(0.0), px(0.0), px(80.0), px(80.0)),
            ));
        }
        assert_eq!(tree.hit_test(Point::new(px(1.0), px(1.0))), Some(ids[3]));
    }

    #[test]
    fn transparent_nodes_still_receive_input() {
        // Matches CSS: `opacity: 0` hides pixels, it does not disable the control.
        let mut tree = LayoutTree::new();
        let root =
            placed(&mut tree, None, Style::row(), rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        let ghost = placed(
            &mut tree,
            Some(root),
            Style::row().with_opacity(0.0),
            rect(px(0.0), px(0.0), px(50.0), px(50.0)),
        );
        assert_eq!(tree.hit_test(Point::new(px(5.0), px(5.0))), Some(ghost));
    }
}

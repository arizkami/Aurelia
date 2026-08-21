//! Invalidation flags and the rules that govern how they spread.
//!
//! This module is small and it is the most important thing in the crate.
//!
//! A plug-in editor repaints its meters at the display refresh rate. If a repaint
//! could ever drag layout along with it, the whole engine is unusable on the
//! audio-adjacent thread budget. So invalidation is split by *kind*: some flags
//! mean "the geometry might have moved", others mean "only the pixels changed",
//! and the two travel through the tree along different paths.
//!
//! ## Propagation
//!
//! Marking a node dirty does two things:
//!
//! 1. It records the flags on the node itself. That is what
//!    [`LayoutTree::dirty`](crate::LayoutTree::dirty) reports.
//! 2. It walks *up* the ancestor chain setting summary bits, so that a later
//!    traversal can skip any branch that contains nothing to do.
//!
//! It never touches siblings, and it never touches descendants. A layout-
//! affecting change invalidates the ancestor chain because a child's size feeds
//! its parent's size; it does not invalidate the child's own subtree, because the
//! backend's per-node cache already returns the old answer when the inputs to a
//! subtree are unchanged.
//!
//! A paint-affecting change sets only the paint summary. Layout summaries are
//! left alone, and
//! [`LayoutEngine::compute`](crate::LayoutEngine::compute) therefore does
//! literally nothing — see the `paint_dirty_costs_no_layout` test in
//! [`crate::tree`].

use bitflags::bitflags;

bitflags! {
    /// What has changed about a node since the last time the engine looked.
    ///
    /// Flags are additive and monotonic between passes: they accumulate until the
    /// stage that consumes them clears them.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
    pub struct DirtyFlags: u8 {
        /// A layout-affecting style property changed, so the backend must
        /// re-translate this node's style before it can lay it out again.
        ///
        /// Distinct from [`DirtyFlags::LAYOUT`]: that one only invalidates cached
        /// measurements, this one also invalidates the translated style, which is
        /// the more expensive of the two.
        const STYLE = 1 << 0;
        /// Cached measurements for this node are stale even though its style is
        /// unchanged — for example because the viewport resized underneath it.
        const LAYOUT = 1 << 1;
        /// The node's pixels changed but its geometry did not.
        const PAINT = 1 << 2;
        /// The node's text content or font selection changed, so its intrinsic
        /// size must be measured again. Implies a relayout.
        const TEXT = 1 << 3;
        /// The node's child list changed. Implies a relayout, and additionally
        /// tells the backend that its mirrored tree structure is out of date.
        const CHILDREN = 1 << 4;
        /// A paint-time transform (translation, rotation, scale) changed.
        ///
        /// Never a layout change: transforms are applied to the rasteriser, not
        /// to the box model, so a spinning knob costs one matrix, not a relayout.
        const TRANSFORM = 1 << 5;
    }
}

impl DirtyFlags {
    /// Nothing to do.
    pub const NONE: Self = Self::empty();

    /// The flags that force the layout algorithm to run again.
    pub const LAYOUT_AFFECTING: Self =
        Self::STYLE.union(Self::LAYOUT).union(Self::TEXT).union(Self::CHILDREN);

    /// True when these flags require the layout algorithm to run.
    #[inline]
    pub const fn needs_layout(self) -> bool {
        self.intersects(Self::LAYOUT_AFFECTING)
    }

    /// True when these flags require the node to be painted again.
    ///
    /// Every kind of change ultimately shows up on screen, so this is simply
    /// "anything at all is set". It exists as a named predicate so that call
    /// sites read as intent rather than as a bit twiddle.
    #[inline]
    pub const fn needs_paint(self) -> bool {
        !self.is_empty()
    }

    /// True when the backend must re-translate the node's style or structure.
    #[inline]
    pub const fn needs_resync(self) -> bool {
        self.intersects(Self::STYLE.union(Self::CHILDREN))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_empty_and_inert() {
        assert_eq!(DirtyFlags::NONE, DirtyFlags::empty());
        assert!(!DirtyFlags::NONE.needs_layout());
        assert!(!DirtyFlags::NONE.needs_paint());
        assert!(!DirtyFlags::NONE.needs_resync());
    }

    #[test]
    fn paint_and_transform_never_imply_layout() {
        assert!(!DirtyFlags::PAINT.needs_layout());
        assert!(!DirtyFlags::TRANSFORM.needs_layout());
        assert!(!(DirtyFlags::PAINT | DirtyFlags::TRANSFORM).needs_layout());
        assert!((DirtyFlags::PAINT | DirtyFlags::TRANSFORM).needs_paint());
    }

    #[test]
    fn text_and_children_imply_layout() {
        assert!(DirtyFlags::TEXT.needs_layout());
        assert!(DirtyFlags::CHILDREN.needs_layout());
        assert!(DirtyFlags::STYLE.needs_layout());
        assert!(DirtyFlags::LAYOUT.needs_layout());
    }

    #[test]
    fn only_style_and_children_force_a_backend_resync() {
        assert!(DirtyFlags::STYLE.needs_resync());
        assert!(DirtyFlags::CHILDREN.needs_resync());
        // A pure measurement invalidation reuses the translated style.
        assert!(!DirtyFlags::LAYOUT.needs_resync());
        assert!(!DirtyFlags::TEXT.needs_resync());
    }

    #[test]
    fn flags_are_distinct_bits() {
        let all = [
            DirtyFlags::STYLE,
            DirtyFlags::LAYOUT,
            DirtyFlags::PAINT,
            DirtyFlags::TEXT,
            DirtyFlags::CHILDREN,
            DirtyFlags::TRANSFORM,
        ];
        for (i, a) in all.iter().enumerate() {
            assert_eq!(a.bits().count_ones(), 1, "flag {i} is not a single bit");
            for b in all.iter().skip(i + 1) {
                assert!(!a.intersects(*b), "flags overlap: {a:?} and {b:?}");
            }
        }
    }
}

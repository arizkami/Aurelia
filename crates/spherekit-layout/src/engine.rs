//! The layout engine interface: what runs a [`LayoutTree`] and how text nodes
//! report their intrinsic size.
//!
//! The backing algorithm is behind a trait for two reasons. The obvious one is
//! that it can be replaced. The less obvious one is that the *contract* stated
//! here — "skip everything when nothing is layout-dirty, and report how much you
//! actually did" — is testable against any implementation, and a layout engine
//! whose incremental behaviour is not testable is a layout engine that quietly
//! stops being incremental.

use spherekit_core::{LayoutError, NodeId, Px, Size};

use crate::style::Style;
use crate::tree::LayoutTree;

/// How much room a node has on one axis while it is being measured.
///
/// SphereKit's own enum rather than the backend's: a measure callback lives in
/// application code (it is where text shaping happens), and application code must
/// not have to name a third-party type to write one.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AvailableSpace {
    /// Exactly this many logical pixels are available.
    Definite(Px),
    /// The node should report the smallest size it can be without overflowing —
    /// for text, the width of its longest unbreakable word.
    MinContent,
    /// The node should report the size it would take with unlimited room — for
    /// text, the width of the whole run on one line.
    MaxContent,
}

impl AvailableSpace {
    /// The pixel amount, or `None` for the intrinsic-sizing constraints.
    #[inline]
    pub const fn definite(self) -> Option<Px> {
        match self {
            AvailableSpace::Definite(v) => Some(v),
            _ => None,
        }
    }

    /// True when a concrete pixel amount is available.
    #[inline]
    pub const fn is_definite(self) -> bool {
        matches!(self, AvailableSpace::Definite(_))
    }

    /// The pixel amount, falling back to `default` for the intrinsic constraints.
    #[inline]
    pub fn unwrap_or(self, default: Px) -> Px {
        match self {
            AvailableSpace::Definite(v) => v,
            _ => default,
        }
    }
}

/// Everything a measure callback is told about the node it is sizing.
///
/// Passed as one struct rather than four arguments so that adding a field later
/// (a scale factor, a writing direction) does not break every caller.
#[derive(Debug)]
pub struct MeasureRequest<'a> {
    /// The node being measured.
    pub node: NodeId,
    /// Its style, in case the callback needs the font size or padding.
    pub style: &'a Style,
    /// Axes whose size the algorithm has already decided.
    ///
    /// When `known.width` is `Some`, the callback is being asked "how tall are
    /// you at this width" — the classic text-wrapping question.
    pub known: Size<Option<Px>>,
    /// Room available on each axis for the undecided ones.
    pub available: Size<AvailableSpace>,
}

/// Reports the intrinsic size of a leaf node.
///
/// Implemented for any `FnMut(MeasureRequest<'_>) -> Size<Px>`, so the common
/// case is a closure.
///
/// The callback is invoked for every childless node, not only for text: the
/// layout crate has no way to know which leaves have content. Return
/// [`Size::ZERO`] for a plain box.
pub trait Measure {
    /// Measures one node.
    ///
    /// Returning a non-finite or negative size is a bug in the callback and is
    /// reported as [`LayoutError::InvalidMeasure`] rather than being allowed to
    /// poison the geometry of the whole tree.
    fn measure(&mut self, request: MeasureRequest<'_>) -> Size<Px>;
}

impl<F> Measure for F
where
    F: FnMut(MeasureRequest<'_>) -> Size<Px>,
{
    #[inline]
    fn measure(&mut self, request: MeasureRequest<'_>) -> Size<Px> {
        self(request)
    }
}

/// A measure function that gives every leaf a zero intrinsic size.
///
/// The default for trees with no text in them, and the fallback used by
/// [`LayoutEngine::compute`].
#[derive(Copy, Clone, Debug, Default)]
pub struct NoMeasure;

impl Measure for NoMeasure {
    #[inline]
    fn measure(&mut self, _request: MeasureRequest<'_>) -> Size<Px> {
        Size::ZERO
    }
}

/// What one call to the engine actually cost.
///
/// The headline number is [`LayoutStats::nodes_laid_out`]. It exists so that
/// "repainting a meter does no layout work" is an assertion in a test rather than
/// a claim in a comment.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LayoutStats {
    /// Layout-algorithm invocations that missed the cache during the last call.
    ///
    /// Counts *invocations*, not distinct nodes: a flex container may size a
    /// child twice under different constraints, and hiding that would make the
    /// number reassuring rather than useful.
    pub nodes_laid_out: usize,
    /// Nodes whose mirrored backend style was rebuilt during the last call.
    pub nodes_restyled: usize,
    /// Calls that did work.
    pub passes: u64,
    /// Calls that returned immediately because nothing was layout-dirty.
    pub skipped_passes: u64,
}

/// Runs layout over a [`LayoutTree`].
///
/// Implementations must honour two properties, both of which are tested against
/// the shipped backend:
///
/// * If [`LayoutTree::needs_layout`] is false, [`LayoutEngine::compute`] performs
///   no layout work and leaves [`LayoutStats::nodes_laid_out`] at zero.
/// * A layout-dirty node invalidates its own subtree and its ancestor chain, and
///   nothing else.
pub trait LayoutEngine {
    /// Lays out every root against `viewport`, with no intrinsic sizing.
    fn compute(&mut self, tree: &mut LayoutTree, viewport: Size<Px>) -> Result<(), LayoutError> {
        self.compute_with_measure(tree, viewport, &mut NoMeasure)
    }

    /// Lays out every root against `viewport`, asking `measure` for the intrinsic
    /// size of every childless node.
    fn compute_with_measure(
        &mut self,
        tree: &mut LayoutTree,
        viewport: Size<Px>,
        measure: &mut dyn Measure,
    ) -> Result<(), LayoutError>;

    /// Statistics for the most recent call.
    fn stats(&self) -> LayoutStats;

    /// Drops every cached intermediate result.
    ///
    /// Needed when something the engine cannot observe changes — a font is
    /// swapped, a scale factor moves — and the cached measurements are therefore
    /// stale even though no style is.
    fn invalidate(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{px, size};

    #[test]
    fn available_space_accessors() {
        assert_eq!(AvailableSpace::Definite(px(10.0)).definite(), Some(px(10.0)));
        assert_eq!(AvailableSpace::MinContent.definite(), None);
        assert_eq!(AvailableSpace::MaxContent.definite(), None);
        assert!(AvailableSpace::Definite(px(0.0)).is_definite());
        assert!(!AvailableSpace::MinContent.is_definite());
        assert_eq!(AvailableSpace::MaxContent.unwrap_or(px(5.0)), px(5.0));
    }

    #[test]
    fn closures_are_measure_functions() {
        let mut calls = 0usize;
        let mut f = |req: MeasureRequest<'_>| {
            calls += 1;
            size(req.available.width.unwrap_or(px(1.0)), px(2.0))
        };
        let style = Style::DEFAULT;
        let out = f.measure(MeasureRequest {
            node: NodeId::new(0, 1),
            style: &style,
            known: size(None, None),
            available: size(AvailableSpace::Definite(px(30.0)), AvailableSpace::MaxContent),
        });
        assert_eq!(out, size(px(30.0), px(2.0)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn no_measure_reports_zero() {
        let style = Style::DEFAULT;
        let out = NoMeasure.measure(MeasureRequest {
            node: NodeId::new(3, 1),
            style: &style,
            known: size(None, None),
            available: size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
        });
        assert_eq!(out, Size::ZERO);
    }
}

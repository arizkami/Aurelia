//! The style vocabulary of the layout tree.
//!
//! [`Style`] is SphereKit's own type. It is deliberately *not* a re-export of the
//! backing layout algorithm's style struct: the backend is swappable, and a
//! public type from a third-party crate would nail the engine to one version of
//! it forever.
//!
//! It is also deliberately not a faithful model of CSS. A realtime audio UI does
//! not need `float`, `writing-mode`, RTL grid line names or the `safe`/`unsafe`
//! alignment modifiers, and every property that exists has to be diffed on every
//! `set_style` call. What survives is the subset that a meter, a knob, a mixer
//! strip and a plug-in editor actually use.
//!
//! ## Layout properties vs. paint properties
//!
//! The split matters more than the property list. [`Style::diff`] classifies a
//! change as layout-affecting or paint-only, and
//! [`LayoutTree::set_style`](crate::LayoutTree::set_style) marks only what
//! actually changed. Changing [`Style::opacity`] on a VU meter 60 times a second
//! therefore costs zero layout work; changing its `width` costs a relayout of its
//! ancestor chain. Keeping [`Style`] `Clone + PartialEq` is what makes that
//! classification possible at all.

use spherekit_core::{Corners, Edges, Length, Px, Size, px, size};

use crate::dirty::DirtyFlags;

/// Which algorithm lays out a node's children.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Display {
    /// Children are stacked on the block (vertical) axis, one per line.
    ///
    /// Cheaper than flex when all you want is a vertical stack, and the right
    /// model for document-ish content such as a preset description panel.
    Block,
    /// Children are laid out by the flexbox algorithm.
    ///
    /// The default, because almost every control surface in an audio UI is a row
    /// or a column of things that share out leftover space.
    #[default]
    Flex,
    /// Children are placed into a two-dimensional grid.
    ///
    /// Every child is auto-placed into a single implicit column sized from its
    /// content, and `gap` applies between the resulting tracks.
    ///
    // NOTE(unimplemented): explicit track templates (`grid-template-rows` /
    // `grid-template-columns`), named lines and named areas, and per-item
    // placement (`grid-row` / `grid-column`) are not exposed on `Style`. The
    // backend supports all of them; adding them means adding a track-sizing
    // vocabulary to SphereKit, which is a design decision in its own right and is
    // deliberately deferred rather than half-modelled here.
    Grid,
    /// The node and its whole subtree generate no boxes.
    ///
    /// Distinct from zero opacity or zero size: a `None` node is not laid out, is
    /// not painted and is not hit-testable, and its descendants keep their
    /// identity (and their state) so that unhiding is free.
    None,
}

impl Display {
    /// True when the node participates in layout, painting and hit testing.
    #[inline]
    pub const fn is_visible(self) -> bool {
        !matches!(self, Display::None)
    }
}

/// How a node's box is placed relative to the layout its parent computed.
///
/// SphereKit has no CSS `static`: every node is a containing block for its
/// absolutely positioned children, so "the nearest positioned ancestor" is always
/// the direct parent. Dropping `static` removes an entire class of
/// action-at-a-distance bug where inserting an unrelated wrapper silently moves a
/// popup across the screen.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Position {
    /// The node takes part in its parent's flow, and [`Style::inset`] nudges it
    /// afterwards without moving anything else.
    #[default]
    Relative,
    /// The node is taken out of flow and placed by [`Style::inset`] against the
    /// parent's padding box. It reserves no space for itself.
    Absolute,
}

/// The main axis of a flex container, and the direction items flow along it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum FlexDirection {
    /// Left to right.
    #[default]
    Row,
    /// Top to bottom.
    Column,
    /// Right to left.
    RowReverse,
    /// Bottom to top.
    ColumnReverse,
}

impl FlexDirection {
    /// True when the main axis is horizontal.
    #[inline]
    pub const fn is_row(self) -> bool {
        matches!(self, FlexDirection::Row | FlexDirection::RowReverse)
    }
}

/// Whether flex items overflow on one line or wrap onto several.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum FlexWrap {
    /// Everything stays on one line, shrinking if it has to.
    #[default]
    NoWrap,
    /// Items wrap onto new lines in the cross-axis direction.
    Wrap,
    /// Items wrap onto new lines against the cross-axis direction.
    WrapReverse,
}

/// How an item is aligned on its container's cross axis.
///
/// CSS's `flex-start`/`start`/`self-start` triplet collapses to a single
/// [`Align::Start`] here: they only differ under writing modes and reversed
/// directions that SphereKit resolves at the container instead.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    /// Packed against the cross-axis start edge.
    Start,
    /// Packed against the cross-axis end edge.
    End,
    /// Centred on the cross axis.
    Center,
    /// Stretched to fill the cross axis, which is what makes a full-height
    /// mixer strip fall out of a plain row.
    Stretch,
    /// Aligned so that the first text baselines line up.
    Baseline,
}

/// How leftover space is distributed along an axis.
///
/// Used for both `justify_content` (main axis) and `align_content` (cross axis of
/// a wrapped container); the two properties differ only in which axis they apply
/// to, so they share one enum rather than two near-identical ones.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Distribute {
    /// All leftover space goes after the content.
    Start,
    /// All leftover space goes before the content.
    End,
    /// Leftover space is split evenly before and after the content.
    Center,
    /// Lines are stretched to consume the leftover space.
    Stretch,
    /// First and last items touch the edges; gaps between items are equal.
    SpaceBetween,
    /// Every gap, including the outer two, is the same size.
    SpaceEvenly,
    /// Outer gaps are half the size of the gaps between items.
    SpaceAround,
}

/// What happens to content that does not fit inside a node.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Overflow {
    /// Content spills out and is still painted and hit-testable.
    #[default]
    Visible,
    /// Content is clipped to the node's padding box and cannot be scrolled.
    Hidden,
    /// Content is clipped to the node's padding box and can be scrolled with
    /// [`LayoutTree::set_scroll_offset`](crate::LayoutTree::set_scroll_offset).
    Scroll,
}

impl Overflow {
    /// True when the node clips its descendants to its padding box.
    ///
    /// Hit testing consults this: a child pushed outside a clipping ancestor is
    /// invisible, and invisible things must not swallow clicks.
    #[inline]
    pub const fn clips(self) -> bool {
        !matches!(self, Overflow::Visible)
    }

    /// True when the node accepts a scroll offset.
    #[inline]
    pub const fn is_scrollable(self) -> bool {
        matches!(self, Overflow::Scroll)
    }
}

/// All four sides set to the same length.
///
/// [`Edges::all`](spherekit_core::Edges::all) requires a numeric scalar, and
/// [`Length`] is a sum type rather than a scalar, so the length-flavoured
/// constructors live here.
#[inline]
pub const fn edges_all(v: Length) -> Edges<Length> {
    Edges { top: v, right: v, bottom: v, left: v }
}

/// `vertical` on the top and bottom, `horizontal` on the left and right.
#[inline]
pub const fn edges_symmetric(vertical: Length, horizontal: Length) -> Edges<Length> {
    Edges { top: vertical, right: horizontal, bottom: vertical, left: horizontal }
}

/// All four sides set to the same pixel amount.
#[inline]
pub const fn edges_px(v: f32) -> Edges<Length> {
    edges_all(Length::Px(px(v)))
}

/// A fixed pixel size on both axes.
#[inline]
pub const fn size_px(width: f32, height: f32) -> Size<Length> {
    size(Length::Px(px(width)), Length::Px(px(height)))
}

/// Everything the layout algorithm and the painter need to know about one node.
///
/// Cheap to clone (no allocation, no `Vec`, no `Arc`) and comparable, because the
/// dirty-tracking design in [`crate::dirty`] is built on comparing the old style
/// against the new one rather than on trusting callers to invalidate correctly.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    /// Which algorithm lays out this node's children.
    pub display: Display,
    /// Whether the node stays in its parent's flow or is placed by `inset`.
    pub position: Position,
    /// Offsets from the parent's padding box edges.
    ///
    /// For [`Position::Relative`] this is a post-layout nudge that moves nothing
    /// else; for [`Position::Absolute`] it *is* the placement. `Auto` means "let
    /// the algorithm decide", which for an absolute node means "keep the position
    /// flow would have given it".
    pub inset: Edges<Length>,
    /// Preferred outer size. `Auto` defers to the content and the container.
    pub size: Size<Length>,
    /// Lower clamp applied after `size` resolves.
    pub min_size: Size<Length>,
    /// Upper clamp applied after `size` and `min_size` resolve.
    pub max_size: Size<Length>,
    /// Space reserved outside the node's border box.
    ///
    /// `Auto` margins absorb leftover space, which is how a single control is
    /// centred inside a row without wrapping it in another container.
    pub margin: Edges<Length>,
    /// Space between the border box and the content box.
    ///
    /// `Auto` is meaningless here and resolves to zero.
    pub padding: Edges<Length>,
    /// Border thickness, which occupies space exactly like padding does.
    ///
    /// Separate from padding because the painter needs to know where to stroke,
    /// and because the content box is inset by both.
    pub border: Edges<Length>,
    /// Main axis of a flex container.
    pub flex_direction: FlexDirection,
    /// Whether flex items wrap onto multiple lines.
    pub flex_wrap: FlexWrap,
    /// Share of leftover main-axis space this item claims. `0.0` claims none.
    pub flex_grow: f32,
    /// Share of the main-axis overflow this item absorbs when the line is too
    /// small. `1.0` (the default) means it shrinks proportionally to its basis.
    pub flex_shrink: f32,
    /// Main-axis size used as the starting point before growing or shrinking.
    /// `Auto` falls back to `size` on the main axis.
    pub flex_basis: Length,
    /// Gutter between children: `width` between columns, `height` between rows.
    ///
    /// Fixed pixels rather than [`Length`]: percentage gutters resolve against the
    /// container and interact badly with wrapping, and no audio UI has ever
    /// needed one.
    pub gap: Size<Px>,
    /// Default cross-axis alignment for this node's children. `None` means the
    /// algorithm's own default, which is [`Align::Stretch`] for flex.
    pub align_items: Option<Align>,
    /// Cross-axis alignment for *this* node, overriding the parent's
    /// `align_items`.
    pub align_self: Option<Align>,
    /// Distribution of leftover main-axis space between this node's children.
    pub justify_content: Option<Distribute>,
    /// Distribution of leftover cross-axis space between the lines of a wrapped
    /// container.
    pub align_content: Option<Distribute>,
    /// Horizontal overflow behaviour.
    pub overflow_x: Overflow,
    /// Vertical overflow behaviour.
    pub overflow_y: Overflow,
    /// Preferred width-to-height ratio, applied when exactly one axis is known.
    ///
    /// `Some(2.0)` is twice as wide as it is tall. Non-finite and non-positive
    /// values are ignored during layout rather than producing NaN geometry.
    pub aspect_ratio: Option<f32>,
    /// Corner radii of the border box.
    ///
    /// Layout ignores this entirely — it exists here so that a radius change is
    /// classified as paint-only and never triggers a relayout.
    pub corner_radius: Corners<Px>,
    /// Opacity of the node and its subtree, in `0.0..=1.0`.
    ///
    /// Paint-only. A fully transparent node is still laid out and still hit-
    /// testable, matching CSS: use [`Display::None`] to remove it from both.
    pub opacity: f32,
    /// Paint and hit-test order among siblings; higher is nearer the viewer.
    ///
    /// Scoped to siblings rather than establishing global stacking contexts. A
    /// full CSS stacking model buys a plug-in editor nothing and costs a sort key
    /// on every node.
    pub z_index: i32,
}

impl Default for Style {
    #[inline]
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Style {
    /// The default style, usable in `const` position.
    pub const DEFAULT: Self = Self {
        display: Display::Flex,
        position: Position::Relative,
        inset: edges_all(Length::Auto),
        size: size(Length::Auto, Length::Auto),
        min_size: size(Length::Auto, Length::Auto),
        max_size: size(Length::Auto, Length::Auto),
        margin: edges_all(Length::Px(Px::ZERO)),
        padding: edges_all(Length::Px(Px::ZERO)),
        border: edges_all(Length::Px(Px::ZERO)),
        flex_direction: FlexDirection::Row,
        flex_wrap: FlexWrap::NoWrap,
        flex_grow: 0.0,
        flex_shrink: 1.0,
        flex_basis: Length::Auto,
        gap: Size::ZERO,
        align_items: None,
        align_self: None,
        justify_content: None,
        align_content: None,
        overflow_x: Overflow::Visible,
        overflow_y: Overflow::Visible,
        aspect_ratio: None,
        corner_radius: Corners::ZERO,
        opacity: 1.0,
        z_index: 0,
    };

    /// A horizontal flex container.
    #[inline]
    pub const fn row() -> Self {
        Self { display: Display::Flex, flex_direction: FlexDirection::Row, ..Self::DEFAULT }
    }

    /// A vertical flex container.
    #[inline]
    pub const fn column() -> Self {
        Self { display: Display::Flex, flex_direction: FlexDirection::Column, ..Self::DEFAULT }
    }

    /// A block container: children stack vertically, one per line.
    #[inline]
    pub const fn block() -> Self {
        Self { display: Display::Block, ..Self::DEFAULT }
    }

    /// A grid container.
    #[inline]
    pub const fn grid() -> Self {
        Self { display: Display::Grid, ..Self::DEFAULT }
    }

    /// A node that is laid out, painted and hit-tested as if it did not exist.
    #[inline]
    pub const fn hidden() -> Self {
        Self { display: Display::None, ..Self::DEFAULT }
    }

    /// Sets [`Style::display`].
    #[inline]
    pub const fn with_display(mut self, v: Display) -> Self {
        self.display = v;
        self
    }

    /// Sets [`Style::position`].
    #[inline]
    pub const fn with_position(mut self, v: Position) -> Self {
        self.position = v;
        self
    }

    /// Sets [`Style::inset`].
    #[inline]
    pub const fn with_inset(mut self, v: Edges<Length>) -> Self {
        self.inset = v;
        self
    }

    /// Places the node absolutely at `left`/`top` from the parent's padding box.
    #[inline]
    pub const fn absolute_at(mut self, left: f32, top: f32) -> Self {
        self.position = Position::Absolute;
        self.inset = Edges {
            top: Length::Px(px(top)),
            right: Length::Auto,
            bottom: Length::Auto,
            left: Length::Px(px(left)),
        };
        self
    }

    /// Sets [`Style::size`].
    #[inline]
    pub const fn with_size(mut self, v: Size<Length>) -> Self {
        self.size = v;
        self
    }

    /// Sets the preferred width.
    #[inline]
    pub const fn with_width(mut self, v: Length) -> Self {
        self.size.width = v;
        self
    }

    /// Sets the preferred height.
    #[inline]
    pub const fn with_height(mut self, v: Length) -> Self {
        self.size.height = v;
        self
    }

    /// Sets a fixed pixel width and height.
    #[inline]
    pub const fn with_px_size(mut self, width: f32, height: f32) -> Self {
        self.size = size_px(width, height);
        self
    }

    /// Sets [`Style::min_size`].
    #[inline]
    pub const fn with_min_size(mut self, v: Size<Length>) -> Self {
        self.min_size = v;
        self
    }

    /// Sets [`Style::max_size`].
    #[inline]
    pub const fn with_max_size(mut self, v: Size<Length>) -> Self {
        self.max_size = v;
        self
    }

    /// Sets [`Style::margin`].
    #[inline]
    pub const fn with_margin(mut self, v: Edges<Length>) -> Self {
        self.margin = v;
        self
    }

    /// Sets [`Style::padding`].
    #[inline]
    pub const fn with_padding(mut self, v: Edges<Length>) -> Self {
        self.padding = v;
        self
    }

    /// Sets [`Style::border`].
    #[inline]
    pub const fn with_border(mut self, v: Edges<Length>) -> Self {
        self.border = v;
        self
    }

    /// Sets [`Style::flex_direction`].
    #[inline]
    pub const fn with_flex_direction(mut self, v: FlexDirection) -> Self {
        self.flex_direction = v;
        self
    }

    /// Sets [`Style::flex_wrap`].
    #[inline]
    pub const fn with_flex_wrap(mut self, v: FlexWrap) -> Self {
        self.flex_wrap = v;
        self
    }

    /// Sets grow, shrink and basis in one go, the way CSS shorthand does.
    #[inline]
    pub fn with_flex(mut self, grow: f32, shrink: f32, basis: Length) -> Self {
        self.flex_grow = sanitize_non_negative(grow);
        self.flex_shrink = sanitize_non_negative(shrink);
        self.flex_basis = basis;
        self
    }

    /// Sets [`Style::flex_grow`], clamping non-finite and negative input to zero.
    #[inline]
    pub fn with_flex_grow(mut self, v: f32) -> Self {
        self.flex_grow = sanitize_non_negative(v);
        self
    }

    /// Sets [`Style::flex_shrink`], clamping non-finite and negative input to zero.
    #[inline]
    pub fn with_flex_shrink(mut self, v: f32) -> Self {
        self.flex_shrink = sanitize_non_negative(v);
        self
    }

    /// Sets [`Style::flex_basis`].
    #[inline]
    pub const fn with_flex_basis(mut self, v: Length) -> Self {
        self.flex_basis = v;
        self
    }

    /// Sets [`Style::gap`] on both axes.
    #[inline]
    pub const fn with_gap(mut self, v: Px) -> Self {
        self.gap = Size { width: v, height: v };
        self
    }

    /// Sets [`Style::gap`] per axis.
    #[inline]
    pub const fn with_gap_xy(mut self, column: Px, row: Px) -> Self {
        self.gap = Size { width: column, height: row };
        self
    }

    /// Sets [`Style::align_items`].
    #[inline]
    pub const fn with_align_items(mut self, v: Align) -> Self {
        self.align_items = Some(v);
        self
    }

    /// Sets [`Style::align_self`].
    #[inline]
    pub const fn with_align_self(mut self, v: Align) -> Self {
        self.align_self = Some(v);
        self
    }

    /// Sets [`Style::justify_content`].
    #[inline]
    pub const fn with_justify_content(mut self, v: Distribute) -> Self {
        self.justify_content = Some(v);
        self
    }

    /// Sets [`Style::align_content`].
    #[inline]
    pub const fn with_align_content(mut self, v: Distribute) -> Self {
        self.align_content = Some(v);
        self
    }

    /// Sets both overflow axes.
    #[inline]
    pub const fn with_overflow(mut self, v: Overflow) -> Self {
        self.overflow_x = v;
        self.overflow_y = v;
        self
    }

    /// Sets the overflow axes independently.
    #[inline]
    pub const fn with_overflow_xy(mut self, x: Overflow, y: Overflow) -> Self {
        self.overflow_x = x;
        self.overflow_y = y;
        self
    }

    /// Sets [`Style::aspect_ratio`]. Non-finite and non-positive input clears it.
    #[inline]
    pub fn with_aspect_ratio(mut self, v: f32) -> Self {
        self.aspect_ratio = if v.is_finite() && v > 0.0 { Some(v) } else { None };
        self
    }

    /// Sets [`Style::corner_radius`] on every corner.
    #[inline]
    pub const fn with_corner_radius(mut self, v: Px) -> Self {
        self.corner_radius = Corners { top_left: v, top_right: v, bottom_right: v, bottom_left: v };
        self
    }

    /// Sets [`Style::opacity`], clamped into `0.0..=1.0`.
    #[inline]
    pub fn with_opacity(mut self, v: f32) -> Self {
        self.opacity = if v.is_finite() { v.clamp(0.0, 1.0) } else { 1.0 };
        self
    }

    /// Sets [`Style::z_index`].
    #[inline]
    pub const fn with_z_index(mut self, v: i32) -> Self {
        self.z_index = v;
        self
    }

    /// True when any property that feeds the layout algorithm differs.
    ///
    /// The complement of this set — `corner_radius`, `opacity`, `z_index` — is
    /// what a repainting animation is allowed to touch for free.
    pub fn layout_differs(&self, other: &Self) -> bool {
        self.display != other.display
            || self.position != other.position
            || self.inset != other.inset
            || self.size != other.size
            || self.min_size != other.min_size
            || self.max_size != other.max_size
            || self.margin != other.margin
            || self.padding != other.padding
            || self.border != other.border
            || self.flex_direction != other.flex_direction
            || self.flex_wrap != other.flex_wrap
            || self.flex_grow != other.flex_grow
            || self.flex_shrink != other.flex_shrink
            || self.flex_basis != other.flex_basis
            || self.gap != other.gap
            || self.align_items != other.align_items
            || self.align_self != other.align_self
            || self.justify_content != other.justify_content
            || self.align_content != other.align_content
            || self.overflow_x != other.overflow_x
            || self.overflow_y != other.overflow_y
            || self.aspect_ratio != other.aspect_ratio
    }

    /// Classifies the change from `self` to `other`.
    ///
    /// Returns [`DirtyFlags::NONE`] for identical styles, `PAINT` alone for a
    /// visual-only change, and `STYLE | PAINT` when the layout inputs moved. This
    /// is the single place where "did this edit cost a relayout?" is decided.
    pub fn diff(&self, other: &Self) -> DirtyFlags {
        if self.layout_differs(other) {
            DirtyFlags::STYLE | DirtyFlags::PAINT
        } else if self != other {
            DirtyFlags::PAINT
        } else {
            DirtyFlags::NONE
        }
    }

    /// True when either axis clips its content.
    #[inline]
    pub const fn clips(&self) -> bool {
        self.overflow_x.clips() || self.overflow_y.clips()
    }

    /// True when either axis accepts a scroll offset.
    #[inline]
    pub const fn is_scrollable(&self) -> bool {
        self.overflow_x.is_scrollable() || self.overflow_y.is_scrollable()
    }
}

/// Coerces NaN, infinities and negatives to zero.
///
/// A NaN `flex_grow` poisons the whole free-space distribution and produces a
/// silently blank window, so it is rejected at the door rather than debugged
/// later.
#[inline]
fn sanitize_non_negative(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 { v } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_the_const() {
        assert_eq!(Style::default(), Style::DEFAULT);
    }

    #[test]
    fn default_margins_are_zero_but_inset_is_auto() {
        // `Edges<Length>::default()` would give Auto everywhere, which is right
        // for inset and wrong for margin; the const spells both out.
        let s = Style::DEFAULT;
        assert_eq!(s.margin.left, Length::Px(Px::ZERO));
        assert_eq!(s.inset.left, Length::Auto);
    }

    #[test]
    fn paint_only_change_is_not_a_layout_change() {
        let a = Style::row();
        let b = a.clone().with_opacity(0.25).with_z_index(7).with_corner_radius(px(4.0));
        assert!(!a.layout_differs(&b));
        assert_eq!(a.diff(&b), DirtyFlags::PAINT);
    }

    #[test]
    fn layout_change_reports_style_and_paint() {
        let a = Style::row();
        let b = a.clone().with_width(Length::Px(px(10.0)));
        assert!(a.layout_differs(&b));
        assert_eq!(a.diff(&b), DirtyFlags::STYLE | DirtyFlags::PAINT);
    }

    #[test]
    fn identical_styles_are_not_dirty_at_all() {
        let a = Style::column().with_gap(px(4.0));
        assert_eq!(a.diff(&a.clone()), DirtyFlags::NONE);
    }

    #[test]
    fn every_layout_property_is_covered_by_layout_differs() {
        // A property missing from `layout_differs` would silently stop
        // triggering relayout, which is invisible until a user reports a stale
        // frame. Each mutation below must be detected.
        let base = Style::DEFAULT;
        let mutations: Vec<Style> = vec![
            base.clone().with_display(Display::Block),
            base.clone().with_position(Position::Absolute),
            base.clone().with_inset(edges_px(1.0)),
            base.clone().with_width(Length::Px(px(1.0))),
            base.clone().with_min_size(size_px(1.0, 1.0)),
            base.clone().with_max_size(size_px(1.0, 1.0)),
            base.clone().with_margin(edges_px(1.0)),
            base.clone().with_padding(edges_px(1.0)),
            base.clone().with_border(edges_px(1.0)),
            base.clone().with_flex_direction(FlexDirection::Column),
            base.clone().with_flex_wrap(FlexWrap::Wrap),
            base.clone().with_flex_grow(1.0),
            base.clone().with_flex_shrink(0.0),
            base.clone().with_flex_basis(Length::Px(px(1.0))),
            base.clone().with_gap(px(1.0)),
            base.clone().with_align_items(Align::Center),
            base.clone().with_align_self(Align::Center),
            base.clone().with_justify_content(Distribute::Center),
            base.clone().with_align_content(Distribute::Center),
            base.clone().with_overflow_xy(Overflow::Hidden, Overflow::Visible),
            base.clone().with_overflow_xy(Overflow::Visible, Overflow::Hidden),
            base.clone().with_aspect_ratio(2.0),
        ];
        for (i, m) in mutations.iter().enumerate() {
            assert!(base.layout_differs(m), "mutation {i} was not detected as layout-affecting");
        }
    }

    #[test]
    fn non_finite_flex_factors_are_rejected() {
        assert_eq!(Style::DEFAULT.with_flex_grow(f32::NAN).flex_grow, 0.0);
        assert_eq!(Style::DEFAULT.with_flex_grow(-3.0).flex_grow, 0.0);
        assert_eq!(Style::DEFAULT.with_flex_shrink(f32::INFINITY).flex_shrink, 0.0);
        assert_eq!(Style::DEFAULT.with_flex_grow(2.5).flex_grow, 2.5);
    }

    #[test]
    fn degenerate_aspect_ratio_is_dropped_not_stored() {
        assert_eq!(Style::DEFAULT.with_aspect_ratio(0.0).aspect_ratio, None);
        assert_eq!(Style::DEFAULT.with_aspect_ratio(-1.0).aspect_ratio, None);
        assert_eq!(Style::DEFAULT.with_aspect_ratio(f32::NAN).aspect_ratio, None);
        assert_eq!(Style::DEFAULT.with_aspect_ratio(1.5).aspect_ratio, Some(1.5));
    }

    #[test]
    fn opacity_is_clamped() {
        assert_eq!(Style::DEFAULT.with_opacity(5.0).opacity, 1.0);
        assert_eq!(Style::DEFAULT.with_opacity(-1.0).opacity, 0.0);
        assert_eq!(Style::DEFAULT.with_opacity(f32::NAN).opacity, 1.0);
    }

    #[test]
    fn overflow_predicates() {
        assert!(!Overflow::Visible.clips());
        assert!(Overflow::Hidden.clips());
        assert!(Overflow::Scroll.clips());
        assert!(!Overflow::Hidden.is_scrollable());
        assert!(Overflow::Scroll.is_scrollable());
    }
}

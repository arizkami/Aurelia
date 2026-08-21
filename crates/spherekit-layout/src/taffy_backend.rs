//! The shipped [`LayoutEngine`] implementation, backed by `taffy`.
//!
//! Everything `taffy` is private to this module. No `taffy::` type appears in any
//! public signature anywhere in the crate, and the only thing the rest of SphereKit
//! knows is the [`LayoutEngine`] trait.
//!
//! ## Why the low-level API
//!
//! `taffy` also ships a ready-made tree with its own node handles. Using it would
//! mean keeping two trees in step and translating handles in both directions on
//! every operation. Instead this module implements `taffy`'s traits over a flat
//! mirror of [`LayoutTree`], rebuilt only when the tree's identity or structure
//! epoch changes, which keeps `NodeId` the single identity in the system.
//!
//! ## Why layout is not rounded
//!
//! `taffy` can snap a computed tree to whole pixels. SphereKit does not use it:
//! layout is in *logical* pixels, and whole logical pixels are not whole device
//! pixels on a 125 % or 150 % display. Rounding here would quantise geometry to
//! 0.8 of a device pixel and open seams between boxes that should tile. Rounding
//! happens once, in device space, in the renderer.
//!
//! ## Incrementality
//!
//! Three things make a pass cheap:
//!
//! * If nothing is layout-dirty, the structure is unchanged and the viewport is
//!   unchanged, [`LayoutEngine::compute`] returns without touching a node.
//! * Otherwise, only the per-node caches of layout-dirty nodes *and their
//!   ancestors* are cleared. Everything else answers from cache, so an unrelated
//!   sibling branch is never re-run.
//! * The mirror is rebuilt only when [`LayoutTree::structure_epoch`] moves; a
//!   restyle reuses it.
//!
//! [`TaffyLayoutEngine::last_laid_out`] reports exactly which nodes were re-run,
//! which is how the incrementality claims above are asserted in tests rather than
//! asserted in prose.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use spherekit_core::{Edges, LayoutError, Length, NodeId, Point, Px, Rect, Size, px, size};

use crate::engine::{AvailableSpace, LayoutEngine, LayoutStats, Measure, MeasureRequest};
use crate::style::{
    Align, Display, Distribute, FlexDirection, FlexWrap, Overflow, Position, Style,
};
use crate::tree::{ComputedLayout, LayoutTree};

/// Used when a mirrored node has somehow lost its tree counterpart. Cannot
/// happen while the tree is frozen for the duration of a pass, but a measure
/// callback should never be handed a dangling reference to find out.
static FALLBACK_STYLE: Style = Style::DEFAULT;

/// One mirrored node.
struct MirrorNode {
    /// The SphereKit identity this mirrors.
    id: NodeId,
    /// Translated style; rebuilt only when the SphereKit style changed.
    style: taffy::Style,
    /// Per-node measurement cache, preserved across passes and across structural
    /// rebuilds. Throwing it away is what turns an incremental engine into a
    /// full-relayout engine.
    cache: taffy::Cache,
    /// Most recent computed geometry, in `taffy`'s own representation.
    layout: taffy::Layout,
    /// Indices into the mirror arena.
    children: SmallVec<[u32; 4]>,
}

/// A `taffy`-backed [`LayoutEngine`].
///
/// Holds a flat mirror of the tree plus the per-node caches that make repeated
/// passes cheap. An engine can be handed any tree, but it caches for exactly one
/// at a time: alternating between two trees rebuilds the mirror on every pass, so
/// give each long-lived tree its own engine.
pub struct TaffyLayoutEngine {
    arena: Vec<MirrorNode>,
    index_of: FxHashMap<NodeId, u32>,
    roots: Vec<u32>,
    /// Which tree the mirror belongs to. Two trees can share a structure epoch by
    /// coincidence, and reusing another tree's mirror would silently produce
    /// geometry for the wrong nodes.
    tree_id: u64,
    structure_epoch: u64,
    last_viewport: Option<Size<Px>>,
    valid: bool,
    /// Set by [`LayoutEngine::invalidate`]. The mirrored styles are translated
    /// from the tree's styles, and the tree's dirty flags are the only signal
    /// that they moved — a signal any other consumer of the tree is free to
    /// clear first. `invalidate` therefore has to be able to rebuild them
    /// unconditionally, or a mirror that has fallen out of step with the tree
    /// could never be repaired at all.
    restyle_all: bool,
    stats: LayoutStats,
    laid_out_ids: Vec<NodeId>,
}

impl core::fmt::Debug for TaffyLayoutEngine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TaffyLayoutEngine")
            .field("nodes", &self.arena.len())
            .field("roots", &self.roots.len())
            .field("tree_id", &self.tree_id)
            .field("structure_epoch", &self.structure_epoch)
            .field("last_viewport", &self.last_viewport)
            .field("valid", &self.valid)
            .field("stats", &self.stats)
            .finish()
    }
}

impl Default for TaffyLayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TaffyLayoutEngine {
    /// A fresh engine with no cached state.
    pub fn new() -> Self {
        Self {
            arena: Vec::new(),
            index_of: FxHashMap::default(),
            roots: Vec::new(),
            tree_id: 0,
            structure_epoch: u64::MAX,
            last_viewport: None,
            valid: false,
            restyle_all: false,
            stats: LayoutStats::default(),
            laid_out_ids: Vec::new(),
        }
    }

    /// The nodes whose layout was actually recomputed during the last pass, in
    /// the order the algorithm reached them.
    ///
    /// A node appears more than once when it was sized under several different
    /// constraints, which is normal for flex items. Empty after a skipped pass.
    ///
    /// This is a diagnostic, not a rendering input: it answers "why did that
    /// frame cost 3 ms" without a profiler.
    #[inline]
    pub fn last_laid_out(&self) -> &[NodeId] {
        &self.laid_out_ids
    }

    /// Rebuilds or refreshes the mirror so it matches `tree`.
    fn sync(&mut self, tree: &LayoutTree) {
        let restyle_all = core::mem::replace(&mut self.restyle_all, false);

        if !restyle_all
            && self.valid
            && self.tree_id == tree.id()
            && self.structure_epoch == tree.structure_epoch()
        {
            // Structure is intact: only re-translate styles that actually moved.
            for node in self.arena.iter_mut() {
                let Some(source) = tree.node(node.id) else { continue };
                if source.dirty.needs_resync() {
                    node.style = to_taffy_style(&source.style);
                    self.stats.nodes_restyled += 1;
                }
            }
            return;
        }

        // The shape the mirror currently holds, expressed in `NodeId`s. Arena
        // indices are about to be reassigned and so cannot be compared across a
        // rebuild; identities can, and that is what makes the cache invalidation
        // below independent of the tree's dirty flags. Those flags belong to the
        // tree, not to this engine, and anything else holding the tree is free to
        // clear them first — an engine that trusted them would then answer a
        // structural change entirely from cache and write zeroed geometry for
        // every node it never descended into.
        let mirrored_ids: Vec<NodeId> = self.arena.iter().map(|n| n.id).collect();
        let mirrored_roots: Vec<NodeId> =
            self.roots.iter().filter_map(|&i| mirrored_ids.get(i as usize).copied()).collect();

        // Structural rebuild. Old nodes hand over their caches so that a change
        // in one corner of the tree does not invalidate the rest of it — but only
        // within the same tree. Two trees mint `NodeId`s from zero, so reusing
        // another tree's entries would hand a node someone else's cached size.
        let mut previous: FxHashMap<NodeId, MirrorNode> = if self.tree_id == tree.id() {
            self.arena.drain(..).map(|n| (n.id, n)).collect()
        } else {
            self.arena.clear();
            FxHashMap::default()
        };

        self.index_of.clear();
        self.roots.clear();

        // Depth-first pre-order, iteratively: a parent always lands in the arena
        // before its children, which the write-back pass then relies on.
        let mut order: Vec<NodeId> = Vec::with_capacity(tree.len());
        let mut stack: Vec<NodeId> = tree.roots().iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            self.index_of.insert(id, order.len() as u32);
            order.push(id);
            for &child in tree.children(id).iter().rev() {
                stack.push(child);
            }
        }
        for &root in tree.roots() {
            if let Some(&idx) = self.index_of.get(&root) {
                self.roots.push(idx);
            }
        }

        // Whether each node's own cached answers are no longer trustworthy.
        // Parallel to the arena, so it is indexed exactly like it.
        let mut stale: Vec<bool> = Vec::with_capacity(order.len());

        self.arena.reserve(order.len());
        for &id in &order {
            let Some(source) = tree.node(id) else { continue };
            let children: SmallVec<[u32; 4]> =
                source.children.iter().filter_map(|c| self.index_of.get(c).copied()).collect();
            match previous.remove(&id) {
                Some(mut old) => {
                    // Arena indices shift on a rebuild, but a cached measurement
                    // is keyed on the *inputs* to layout, so it survives the move.
                    if restyle_all || source.dirty.needs_resync() {
                        old.style = to_taffy_style(&source.style);
                        old.cache.clear();
                        self.stats.nodes_restyled += 1;
                    }
                    // A different child list means different layout inputs, so
                    // whatever this node last cached answers the wrong question.
                    let moved = old.children.len() != source.children.len()
                        || old
                            .children
                            .iter()
                            .zip(source.children.iter())
                            .any(|(&i, want)| mirrored_ids.get(i as usize) != Some(want));
                    old.children = children;
                    self.arena.push(old);
                    stale.push(moved);
                }
                None => {
                    self.stats.nodes_restyled += 1;
                    self.arena.push(MirrorNode {
                        id,
                        style: to_taffy_style(&source.style),
                        cache: taffy::Cache::new(),
                        layout: taffy::Layout::new(),
                        children,
                    });
                    stale.push(true);
                }
            }
        }

        // A node that has just become a root is sized against the viewport
        // instead of against a parent, which its cache knows nothing about.
        if mirrored_roots.as_slice() != tree.roots() {
            for &r in &self.roots {
                stale[r as usize] = true;
            }
        }

        // A parent that answers from its own cache never descends, so a node
        // whose descendants moved has to be re-run even though its own inputs
        // look unchanged. The arena is in pre-order, so every child sits at a
        // higher index than its parent and a single reverse sweep propagates
        // staleness all the way up.
        for i in (0..self.arena.len()).rev() {
            if !stale[i] && self.arena[i].children.iter().any(|&c| stale[c as usize]) {
                stale[i] = true;
            }
        }
        for (i, &is_stale) in stale.iter().enumerate() {
            if is_stale {
                self.arena[i].cache.clear();
            }
        }
    }
}

impl LayoutEngine for TaffyLayoutEngine {
    fn compute_with_measure(
        &mut self,
        tree: &mut LayoutTree,
        viewport: Size<Px>,
        measure: &mut dyn Measure,
    ) -> Result<(), LayoutError> {
        self.stats.nodes_laid_out = 0;
        self.stats.nodes_restyled = 0;
        self.laid_out_ids.clear();

        let viewport_changed = self.last_viewport != Some(viewport);
        let structure_changed =
            self.tree_id != tree.id() || self.structure_epoch != tree.structure_epoch();
        if self.valid && !viewport_changed && !structure_changed && !tree.needs_layout() {
            self.stats.skipped_passes += 1;
            return Ok(());
        }

        let full_invalidate = !self.valid || viewport_changed;
        self.sync(tree);

        // Clear exactly the caches that can no longer be trusted: the dirty nodes
        // themselves plus every ancestor, which is precisely the set the tree
        // summarises in `subtree_layout`.
        for node in self.arena.iter_mut() {
            if full_invalidate || tree.node(node.id).is_some_and(|n| n.subtree_layout) {
                node.cache.clear();
            }
        }

        let available = taffy::Size {
            width: taffy::AvailableSpace::Definite(non_negative(viewport.width)),
            height: taffy::AvailableSpace::Definite(non_negative(viewport.height)),
        };

        // The root list is moved out so the pass can borrow the arena mutably.
        let roots = core::mem::take(&mut self.roots);
        let mut pass = Pass {
            arena: &mut self.arena,
            tree,
            measure,
            laid_out: &mut self.laid_out_ids,
            bad_measure: None,
        };
        for &root in &roots {
            taffy::compute_root_layout(&mut pass, taffy::NodeId::from(root as usize), available);
        }
        let bad_measure = pass.bad_measure;
        self.roots = roots;

        if let Some(node) = bad_measure {
            // The tree is left untouched: half-applied geometry is worse than
            // stale geometry, because stale geometry is at least self-consistent.
            self.valid = false;
            return Err(LayoutError::InvalidMeasure(node));
        }

        self.stats.nodes_laid_out = self.laid_out_ids.len();

        for node in self.arena.iter() {
            tree.write_layout(node.id, from_taffy_layout(&node.layout));
        }
        tree.refresh_absolute();
        tree.clear_layout_dirty();

        self.tree_id = tree.id();
        self.structure_epoch = tree.structure_epoch();
        self.last_viewport = Some(viewport);
        self.valid = true;
        self.stats.passes += 1;
        Ok(())
    }

    #[inline]
    fn stats(&self) -> LayoutStats {
        self.stats
    }

    fn invalidate(&mut self) {
        self.valid = false;
        self.last_viewport = None;
        // Also re-translate every style. The mirrored styles are only refreshed
        // when the tree reports a node as `STYLE`-dirty, and those flags are the
        // tree's, not the engine's: a second engine, or a caller that clears them
        // itself, can consume the notification before this engine ever sees it.
        // Without this, a mirror that has drifted out of step with the tree has
        // no way back short of building a new engine.
        self.restyle_all = true;
        for node in self.arena.iter_mut() {
            node.cache.clear();
        }
    }
}

/// One in-flight layout pass: the mirror, a frozen view of the tree, and the
/// caller's measure function.
///
/// The tree is borrowed immutably on purpose. Geometry is written back only after
/// the pass finishes, so nothing the algorithm reads can change underneath it.
struct Pass<'a> {
    arena: &'a mut Vec<MirrorNode>,
    tree: &'a LayoutTree,
    measure: &'a mut dyn Measure,
    laid_out: &'a mut Vec<NodeId>,
    bad_measure: Option<NodeId>,
}

/// Iterator over a mirrored node's children, in `taffy`'s handle space.
struct ChildIter<'a>(core::slice::Iter<'a, u32>);

impl Iterator for ChildIter<'_> {
    type Item = taffy::NodeId;

    #[inline]
    fn next(&mut self) -> Option<taffy::NodeId> {
        self.0.next().map(|&i| taffy::NodeId::from(i as usize))
    }
}

impl taffy::TraversePartialTree for Pass<'_> {
    type ChildIter<'a>
        = ChildIter<'a>
    where
        Self: 'a;

    #[inline]
    fn child_ids(&self, node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        ChildIter(self.arena[usize::from(node_id)].children.iter())
    }

    #[inline]
    fn child_count(&self, node_id: taffy::NodeId) -> usize {
        self.arena[usize::from(node_id)].children.len()
    }

    #[inline]
    fn get_child_id(&self, node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        taffy::NodeId::from(self.arena[usize::from(node_id)].children[index] as usize)
    }
}

impl taffy::LayoutPartialTree for Pass<'_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style
    where
        Self: 'a;

    /// Named grid lines and areas are not exposed by SphereKit's [`Style`], so this
    /// type is never actually instantiated.
    type CustomIdent = String;

    #[inline]
    fn get_core_container_style(&self, node_id: taffy::NodeId) -> Self::CoreContainerStyle<'_> {
        &self.arena[usize::from(node_id)].style
    }

    #[inline]
    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        self.arena[usize::from(node_id)].layout = *layout;
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::LayoutInput,
    ) -> taffy::LayoutOutput {
        // An ancestor is `Display::None`, so this node is hidden whatever its own
        // display says. Handled before the cache because a hidden result is not a
        // measurement and must not displace a real one.
        if inputs.run_mode == taffy::RunMode::PerformHiddenLayout {
            return taffy::compute_hidden_layout(self, node_id);
        }
        taffy::compute_cached_layout(self, node_id, inputs, |pass, node_id, inputs| {
            let idx = usize::from(node_id);
            pass.laid_out.push(pass.arena[idx].id);

            let display = pass.arena[idx].style.display;
            if display == taffy::Display::None {
                return taffy::compute_hidden_layout(pass, node_id);
            }

            if pass.arena[idx].children.is_empty() {
                // Leaf: the only place a caller-supplied measure function runs.
                let tree: &LayoutTree = pass.tree;
                let mirror = &pass.arena[idx];
                let node = mirror.id;
                let style = tree.node(node).map_or(&FALLBACK_STYLE, |n| &n.style);
                let measure = &mut *pass.measure;
                let bad_measure = &mut pass.bad_measure;
                return taffy::compute_leaf_layout(
                    inputs,
                    &mirror.style,
                    |_, _| 0.0,
                    |known, available| {
                        let out = measure.measure(MeasureRequest {
                            node,
                            style,
                            known: size(known.width.map(px), known.height.map(px)),
                            available: size(
                                from_taffy_space(available.width),
                                from_taffy_space(available.height),
                            ),
                        });
                        if !is_usable_extent(out.width) || !is_usable_extent(out.height) {
                            *bad_measure = Some(node);
                            return taffy::Size { width: 0.0, height: 0.0 };
                        }
                        taffy::Size { width: out.width.get(), height: out.height.get() }
                    },
                );
            }

            match display {
                taffy::Display::Flex => taffy::compute_flexbox_layout(pass, node_id, inputs),
                taffy::Display::Grid => taffy::compute_grid_layout(pass, node_id, inputs),
                _ => taffy::compute_block_layout(pass, node_id, inputs, None),
            }
        })
    }
}

impl taffy::CacheTree for Pass<'_> {
    #[inline]
    fn cache_get(
        &self,
        node_id: taffy::NodeId,
        inputs: &taffy::LayoutInput,
    ) -> Option<taffy::LayoutOutput> {
        self.arena[usize::from(node_id)].cache.get(inputs)
    }

    #[inline]
    fn cache_store(
        &mut self,
        node_id: taffy::NodeId,
        inputs: &taffy::LayoutInput,
        output: taffy::LayoutOutput,
    ) {
        self.arena[usize::from(node_id)].cache.store(inputs, output);
    }

    #[inline]
    fn cache_clear(&mut self, node_id: taffy::NodeId) {
        self.arena[usize::from(node_id)].cache.clear();
    }
}

impl taffy::LayoutFlexboxContainer for Pass<'_> {
    type FlexboxContainerStyle<'a>
        = &'a taffy::Style
    where
        Self: 'a;
    type FlexboxItemStyle<'a>
        = &'a taffy::Style
    where
        Self: 'a;

    #[inline]
    fn get_flexbox_container_style(
        &self,
        node_id: taffy::NodeId,
    ) -> Self::FlexboxContainerStyle<'_> {
        &self.arena[usize::from(node_id)].style
    }

    #[inline]
    fn get_flexbox_child_style(&self, child_node_id: taffy::NodeId) -> Self::FlexboxItemStyle<'_> {
        &self.arena[usize::from(child_node_id)].style
    }
}

impl taffy::LayoutGridContainer for Pass<'_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style
    where
        Self: 'a;
    type GridItemStyle<'a>
        = &'a taffy::Style
    where
        Self: 'a;

    #[inline]
    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        &self.arena[usize::from(node_id)].style
    }

    #[inline]
    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.arena[usize::from(child_node_id)].style
    }
}

impl taffy::LayoutBlockContainer for Pass<'_> {
    type BlockContainerStyle<'a>
        = &'a taffy::Style
    where
        Self: 'a;
    type BlockItemStyle<'a>
        = &'a taffy::Style
    where
        Self: 'a;

    #[inline]
    fn get_block_container_style(&self, node_id: taffy::NodeId) -> Self::BlockContainerStyle<'_> {
        &self.arena[usize::from(node_id)].style
    }

    #[inline]
    fn get_block_child_style(&self, child_node_id: taffy::NodeId) -> Self::BlockItemStyle<'_> {
        &self.arena[usize::from(child_node_id)].style
    }
}

// -------------------------------------------------------------- translation

/// True when a measured extent can be used as a box dimension.
#[inline]
fn is_usable_extent(v: Px) -> bool {
    v.is_finite() && v.get() >= 0.0
}

/// Clamps a viewport extent to something a layout algorithm can work with.
#[inline]
fn non_negative(v: Px) -> f32 {
    if v.is_finite() { v.get().max(0.0) } else { 0.0 }
}

/// Replaces NaN and infinities with zero.
#[inline]
fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// A length that may be `Auto`.
#[inline]
fn to_dimension(l: Length) -> taffy::Dimension {
    match l {
        Length::Auto => taffy::Dimension::auto(),
        Length::Px(v) => taffy::Dimension::length(finite(v.get())),
        Length::Fraction(f) => taffy::Dimension::percent(finite(f)),
    }
}

/// A length that may be `Auto`, for inset and margin.
#[inline]
fn to_length_auto(l: Length) -> taffy::LengthPercentageAuto {
    match l {
        Length::Auto => taffy::LengthPercentageAuto::auto(),
        Length::Px(v) => taffy::LengthPercentageAuto::length(finite(v.get())),
        Length::Fraction(f) => taffy::LengthPercentageAuto::percent(finite(f)),
    }
}

/// A length that may not be `Auto`.
///
/// `Auto` padding and `Auto` borders do not exist in any box model; treating them
/// as zero is both what CSS does and the only thing that can be drawn.
#[inline]
fn to_length(l: Length) -> taffy::LengthPercentage {
    match l {
        Length::Auto => taffy::LengthPercentage::length(0.0),
        Length::Px(v) => taffy::LengthPercentage::length(finite(v.get())),
        Length::Fraction(f) => taffy::LengthPercentage::percent(finite(f)),
    }
}

/// SphereKit `Edges` are `top/right/bottom/left`; `taffy` rects are
/// `left/right/top/bottom`.
#[inline]
fn to_rect_auto(e: Edges<Length>) -> taffy::Rect<taffy::LengthPercentageAuto> {
    taffy::Rect {
        left: to_length_auto(e.left),
        right: to_length_auto(e.right),
        top: to_length_auto(e.top),
        bottom: to_length_auto(e.bottom),
    }
}

/// See [`to_rect_auto`].
#[inline]
fn to_rect(e: Edges<Length>) -> taffy::Rect<taffy::LengthPercentage> {
    taffy::Rect {
        left: to_length(e.left),
        right: to_length(e.right),
        top: to_length(e.top),
        bottom: to_length(e.bottom),
    }
}

/// Translates one SphereKit style into the backend's representation.
fn to_taffy_style(s: &Style) -> taffy::Style {
    taffy::Style {
        display: match s.display {
            Display::Block => taffy::Display::Block,
            Display::Flex => taffy::Display::Flex,
            Display::Grid => taffy::Display::Grid,
            Display::None => taffy::Display::None,
        },
        // SphereKit sizes the border box, always. Content-box sizing makes "this
        // button is 24 px tall" false the moment someone adds a border, and every
        // audio UI is built out of things with borders.
        box_sizing: taffy::BoxSizing::BorderBox,
        overflow: taffy::Point {
            x: to_taffy_overflow(s.overflow_x),
            y: to_taffy_overflow(s.overflow_y),
        },
        // SphereKit draws overlay scrollbars, so no space is reserved in the layout.
        scrollbar_width: 0.0,
        position: match s.position {
            Position::Relative => taffy::Position::Relative,
            Position::Absolute => taffy::Position::Absolute,
        },
        inset: to_rect_auto(s.inset),
        size: taffy::Size {
            width: to_dimension(s.size.width),
            height: to_dimension(s.size.height),
        },
        min_size: taffy::Size {
            width: to_dimension(s.min_size.width),
            height: to_dimension(s.min_size.height),
        },
        max_size: taffy::Size {
            width: to_dimension(s.max_size.width),
            height: to_dimension(s.max_size.height),
        },
        aspect_ratio: s.aspect_ratio.filter(|r| r.is_finite() && *r > 0.0),
        margin: to_rect_auto(s.margin),
        padding: to_rect(s.padding),
        border: to_rect(s.border),
        align_items: s.align_items.map(to_taffy_align),
        align_self: s.align_self.map(to_taffy_align),
        align_content: s.align_content.map(to_taffy_distribute),
        justify_content: s.justify_content.map(to_taffy_distribute),
        gap: taffy::Size {
            width: taffy::LengthPercentage::length(non_negative(s.gap.width)),
            height: taffy::LengthPercentage::length(non_negative(s.gap.height)),
        },
        flex_direction: match s.flex_direction {
            FlexDirection::Row => taffy::FlexDirection::Row,
            FlexDirection::Column => taffy::FlexDirection::Column,
            FlexDirection::RowReverse => taffy::FlexDirection::RowReverse,
            FlexDirection::ColumnReverse => taffy::FlexDirection::ColumnReverse,
        },
        flex_wrap: match s.flex_wrap {
            FlexWrap::NoWrap => taffy::FlexWrap::NoWrap,
            FlexWrap::Wrap => taffy::FlexWrap::Wrap,
            FlexWrap::WrapReverse => taffy::FlexWrap::WrapReverse,
        },
        flex_basis: to_dimension(s.flex_basis),
        flex_grow: finite(s.flex_grow).max(0.0),
        flex_shrink: finite(s.flex_shrink).max(0.0),
        ..taffy::Style::DEFAULT
    }
}

/// `Overflow::Hidden` maps to `taffy`'s `Hidden` rather than `Clip`: both clip,
/// but only `Hidden` gives flex and grid items an automatic minimum size of zero,
/// which is what stops a long label from forcing its container to grow.
#[inline]
fn to_taffy_overflow(o: Overflow) -> taffy::Overflow {
    match o {
        Overflow::Visible => taffy::Overflow::Visible,
        Overflow::Hidden => taffy::Overflow::Hidden,
        Overflow::Scroll => taffy::Overflow::Scroll,
    }
}

#[inline]
fn to_taffy_align(a: Align) -> taffy::AlignItems {
    match a {
        Align::Start => taffy::AlignItems::START,
        Align::End => taffy::AlignItems::END,
        Align::Center => taffy::AlignItems::CENTER,
        Align::Stretch => taffy::AlignItems::STRETCH,
        Align::Baseline => taffy::AlignItems::BASELINE,
    }
}

#[inline]
fn to_taffy_distribute(d: Distribute) -> taffy::AlignContent {
    match d {
        Distribute::Start => taffy::AlignContent::START,
        Distribute::End => taffy::AlignContent::END,
        Distribute::Center => taffy::AlignContent::CENTER,
        Distribute::Stretch => taffy::AlignContent::STRETCH,
        Distribute::SpaceBetween => taffy::AlignContent::SPACE_BETWEEN,
        Distribute::SpaceEvenly => taffy::AlignContent::SPACE_EVENLY,
        Distribute::SpaceAround => taffy::AlignContent::SPACE_AROUND,
    }
}

#[inline]
fn from_taffy_space(a: taffy::AvailableSpace) -> AvailableSpace {
    match a {
        taffy::AvailableSpace::Definite(v) => AvailableSpace::Definite(px(v)),
        taffy::AvailableSpace::MinContent => AvailableSpace::MinContent,
        taffy::AvailableSpace::MaxContent => AvailableSpace::MaxContent,
    }
}

#[inline]
fn from_taffy_edges(r: taffy::Rect<f32>) -> Edges<Px> {
    Edges { top: px(r.top), right: px(r.right), bottom: px(r.bottom), left: px(r.left) }
}

/// `absolute_bounds` is left at [`ComputedLayout::ZERO`] here; the tree fills it
/// in during its own absolute pass, which is the only place that knows about
/// scroll offsets.
fn from_taffy_layout(l: &taffy::Layout) -> ComputedLayout {
    ComputedLayout {
        bounds: Rect::new(
            Point::new(px(l.location.x), px(l.location.y)),
            Size::new(px(l.size.width), px(l.size.height)),
        ),
        absolute_bounds: Rect::ZERO,
        content_size: size(px(l.content_size.width), px(l.content_size.height)),
        border: from_taffy_edges(l.border),
        padding: from_taffy_edges(l.padding),
        margin: from_taffy_edges(l.margin),
        order: l.order,
    }
}

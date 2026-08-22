//! The UI tree: the driver that turns elements into frames.
//!
//! This is where the declarative surface meets the retained one. A build pass
//! flattens the element tree into a parallel arena and reconciles it against
//! last frame's layout nodes by [`ElementId`]; layout runs only if something is
//! layout-dirty; painting walks the arena once; events dispatch along a hit
//! chain.
//!
//! ## Why the tree is flattened
//!
//! Elements own their children, so a naive walk is recursive, and recursion
//! depth in a UI tree is user-controlled. Flattening once into
//! `Vec<BuiltNode>` makes every subsequent pass — paint, hit test, semantics —
//! an iteration over a contiguous array with an explicit stack, so a deeply
//! nested panel cannot overflow the stack and the hot passes get cache
//! locality for free.
//!
//! ## Why identity is derived, not positional
//!
//! An element without an explicit id gets one derived from its parent's id and
//! its index. That is exactly right for static structure and exactly wrong for
//! a reorderable list: moving item 3 to position 0 would hand item 3's node,
//! its scroll offset and its focus to whatever is now at index 0. Keyed lists
//! must call [`Div::id`](crate::element::Div::id), and that is documented at
//! the call site rather than left to be discovered.

use crate::element::{AnyElement, EventContext, InteractionState, PaintContext};
use crate::event::{
    EventFlow, HitChain, HitTarget, Modifiers, MouseButton, MouseMoveEvent, Phase, UiEvent,
};
use crate::focus::{FocusDirection, FocusRegistry, Focusable, ScopeId};
use crate::style::Cursor;
use crate::theme::Theme;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use spherekit_core::{ElementId, NodeId, Point, Px, Rect, Size};
use spherekit_layout::{DirtyFlags, LayoutEngine, LayoutTree, Style, TaffyLayoutEngine};
use spherekit_render::{Canvas, Filter};
use std::sync::Arc;

/// One node of the flattened element tree.
struct BuiltNode {
    /// The layout node this element drives.
    node: NodeId,
    /// The element, with its children already moved into the arena.
    element: AnyElement,
    /// Indices into [`UiTree::built`], in paint order.
    children: SmallVec<[usize; 4]>,
    /// Whether this element clips its children.
    clips: bool,
    /// Whether this element opens an opacity layer.
    opacity: f32,
    /// Optional post-process for this element and its subtree.
    filter: Option<Filter>,
    /// Whether this element or an ancestor is `display: none`.
    ///
    /// Such a subtree is still built — the elements exist, they simply have no
    /// layout — so paint has to be told to leave it alone. Culling by bounds is
    /// not enough: a hidden node keeps its parent's origin, and a zero-extent
    /// rect is only reliably rejected when the element also has no group of its
    /// own to open.
    hidden: bool,
    /// Whether this element wants a second visit after its children.
    paints_over: bool,
}

/// How many lines one wheel notch scrolls.
///
/// The platform default, and the number that matters more than any easing here:
/// Windows reports a notch as `120` raw units, every toolkit turns that into
/// *three* lines, and a window that moves one line per notch feels stuck even
/// though it is scrolling perfectly well.
///
/// This is the fallback. The user's actual setting is read once through
/// [`spherekit_platform::wheel_scroll_lines`] and used in preference — a reader
/// who has turned their wheel up to ten lines has said what they want, and an
/// application that ignores it is the reason they had to say it twice.
pub const WHEEL_LINES_PER_NOTCH: f32 = 3.0;

/// The fraction of the visible height "one screen at a time" scrolls.
///
/// Not the whole height: every shell keeps a couple of lines of context across
/// a page jump so the reader can see where they were. Windows' own list views
/// use one line of overlap; a fixed fraction is the same idea and does not need
/// a line height to express.
const WHEEL_PAGE_FRACTION: f32 = 0.9;

/// How far one wheel notch travels, given a line height and a viewport.
///
/// Split out from the dispatch so the policy — including the page-scroll case,
/// which is easy to write as a four-million-pixel jump — can be tested without
/// a window.
fn wheel_notch_distance(setting: Option<u32>, line: Px, viewport_extent: Px) -> Px {
    match setting {
        // "One screen at a time" is a real Windows setting and is not a number
        // of lines. Taking it literally scrolls `u32::MAX` lines.
        Some(spherekit_platform::WHEEL_SCROLL_PAGE) => {
            Px((viewport_extent.get() * WHEEL_PAGE_FRACTION).max(line.get()))
        }
        // Zero is the documented "no wheel scrolling" value.
        Some(0) => Px::ZERO,
        Some(lines) => line * lines as f32,
        None => line * WHEEL_LINES_PER_NOTCH,
    }
}

/// How long a wheel notch takes to land.
///
/// Short enough that the content stays attached to the wheel — past roughly a
/// sixth of a second the eye reads the delay as the application thinking rather
/// than as motion — and long enough that consecutive notches blend into one
/// movement instead of a stack of jumps.
const SCROLL_GLIDE: f32 = 0.13;

/// A scroll offset on its way somewhere.
///
/// A tween rather than a spring: a wheel notch has a definite destination and
/// must not overshoot it, because overshooting a list means showing rows past
/// the end and pulling them back. Retargeting mid-flight restarts the curve
/// from wherever it had got to, so spinning the wheel reads as one accelerating
/// movement rather than a stack of interrupted ones.
#[derive(Copy, Clone, Debug)]
struct ScrollGlide {
    from: Size<Px>,
    to: Size<Px>,
    elapsed: f32,
}

impl ScrollGlide {
    /// Where the glide is at, and whether it has arrived.
    fn sample(&self) -> (Size<Px>, bool) {
        let t = (self.elapsed / SCROLL_GLIDE).clamp(0.0, 1.0);
        let eased = spherekit_core::animate::Curve::EaseOutCubic.eval(t);
        let at = spherekit_core::size(
            self.from.width + (self.to.width - self.from.width) * eased,
            self.from.height + (self.to.height - self.from.height) * eased,
        );
        (at, t >= 1.0)
    }
}

/// Per-node state that must outlive a rebuild.
#[derive(Copy, Clone, Debug, Default)]
struct NodeState {
    hovered: bool,
    active: bool,
    /// Four floats of widget scratch, retained across rebuilds.
    ///
    /// An element is rebuilt every frame, so it cannot hold state itself; but a
    /// drag needs to remember where it started, and a caret needs to remember
    /// where it is. The node persists, so the state belongs here. Four slots
    /// covers every built-in widget and keeps `NodeState` `Copy` — a `Box` here
    /// would put an allocation on the path every hover event takes.
    scratch: [f32; 4],
}

/// What changed as a result of dispatching an event.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct DispatchResult {
    /// The frame should be repainted.
    pub repaint: bool,
    /// Layout should run again before the next paint.
    pub relayout: bool,
    /// Focus moved.
    pub focus_changed: bool,
    /// The cursor the window should show.
    pub cursor: Option<Cursor>,
    /// Some element consumed the event.
    pub consumed: bool,
}

impl DispatchResult {
    fn merge(&mut self, other: Self) {
        self.repaint |= other.repaint;
        self.relayout |= other.relayout;
        self.focus_changed |= other.focus_changed;
        self.consumed |= other.consumed;
        self.cursor = other.cursor.or(self.cursor);
    }
}

/// What one element's handler asked for.
///
/// Copied out of the [`EventContext`] before its borrow of the node's scratch
/// ends, so the tree can act on both without the two borrows overlapping.
#[derive(Copy, Clone, Debug, Default)]
struct EventOutputs {
    repaint: bool,
    relayout: bool,
    request_focus: bool,
    capture_pointer: bool,
    release_pointer: bool,
    cursor: Option<Cursor>,
    scroll_to: Option<Size<Px>>,
}

/// Counters for the diagnostics overlay.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct TreeStats {
    /// Elements built this frame.
    pub elements: u32,
    /// Layout nodes reused from last frame rather than created.
    pub nodes_reused: u32,
    /// Layout nodes created this frame.
    pub nodes_created: u32,
    /// Layout nodes removed this frame.
    pub nodes_removed: u32,
    /// Nodes the layout engine actually laid out.
    pub nodes_laid_out: u32,
    /// Elements painted, after culling.
    pub elements_painted: u32,
    /// Elements skipped by culling.
    pub elements_culled: u32,
}

/// Drives one window's UI.
pub struct UiTree {
    layout: LayoutTree,
    engine: TaffyLayoutEngine,
    built: Vec<BuiltNode>,
    /// Maps a stable element identity to its layout node, for reconciliation.
    node_for_element: FxHashMap<ElementId, NodeId>,
    /// The previous frame's map, consulted during build and then swapped in.
    previous_nodes: FxHashMap<ElementId, NodeId>,
    /// Retained per-node interaction state.
    node_state: FxHashMap<NodeId, NodeState>,
    /// Which built index a layout node corresponds to, for hit resolution.
    index_for_node: FxHashMap<NodeId, usize>,
    root: Option<NodeId>,
    focus: FocusRegistry,
    /// The element that has captured the pointer, if any.
    captured: Option<usize>,
    hovered_chain: SmallVec<[usize; 12]>,
    theme: Theme,
    /// Text clipboard shared with event handlers.
    clipboard: Arc<spherekit_platform::Clipboard>,
    /// Where the focused editable element wants an input method, as of the last
    /// paint. `None` means nothing on screen accepts text, and the platform's
    /// input method should be switched off.
    ime: Option<crate::element::ImeArea>,
    /// Boxes that must keep receiving clicks under a custom window frame.
    /// Rebuilt every paint; the allocation is kept.
    caption_exclusions: Vec<Rect<Px>>,
    stats: TreeStats,
    /// Scratch for hit testing, reused so a mouse move never allocates.
    hit_scratch: Vec<NodeId>,
    /// The user's wheel setting, read once at construction.
    ///
    /// Cached rather than queried per event: it is a registry-backed system
    /// call, and the wheel is one of the highest-frequency events there is.
    wheel_lines: Option<u32>,
    /// Scroll offsets still travelling toward a wheel target.
    ///
    /// Empty almost always; one entry while a list is gliding. A thumb drag
    /// deliberately does not appear here — a dragged thumb must track the
    /// pointer exactly, and easing it would feel like lag.
    scroll_glide: FxHashMap<NodeId, ScrollGlide>,
}

impl Default for UiTree {
    fn default() -> Self {
        Self::new()
    }
}

impl UiTree {
    /// A new, empty tree with the default theme.
    pub fn new() -> Self {
        Self {
            layout: LayoutTree::new(),
            engine: TaffyLayoutEngine::new(),
            built: Vec::new(),
            node_for_element: FxHashMap::default(),
            previous_nodes: FxHashMap::default(),
            node_state: FxHashMap::default(),
            index_for_node: FxHashMap::default(),
            root: None,
            focus: FocusRegistry::new(),
            captured: None,
            hovered_chain: SmallVec::new(),
            theme: Theme::dark(),
            clipboard: Arc::new(spherekit_platform::Clipboard::system()),
            ime: None,
            caption_exclusions: Vec::new(),
            stats: TreeStats::default(),
            hit_scratch: Vec::new(),
            scroll_glide: FxHashMap::default(),
            wheel_lines: spherekit_platform::wheel_scroll_lines(),
        }
    }

    /// The active theme.
    #[inline]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Where the focused editable element wants an input method.
    ///
    /// Valid after [`UiTree::paint`] and not before: the caret's position is a
    /// paint-time fact. `None` means nothing focused accepts text, and the
    /// window should turn its input method off.
    #[inline]
    pub fn ime(&self) -> Option<crate::element::ImeArea> {
        self.ime
    }

    /// Boxes the platform must not treat as a title bar.
    ///
    /// Valid after [`UiTree::paint`]. Every interactive widget that painted this
    /// frame is here, wherever it is on screen; filter to the caption strip
    /// before publishing if the count matters. See
    /// [`crate::PaintContext::keep_interactive`].
    #[inline]
    pub fn caption_exclusions(&self) -> &[Rect<Px>] {
        &self.caption_exclusions
    }

    /// Replaces the theme. Marks everything paint-dirty, not layout-dirty:
    /// a colour change never moves anything.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        if let Some(root) = self.root {
            self.layout.mark_dirty(root, DirtyFlags::PAINT);
        }
    }

    /// Replaces the clipboard used by editable fields.
    ///
    /// Standalone trees default to the system clipboard. Plug-in hosts can
    /// provide a host-routed [`spherekit_platform::Clipboard`] here instead.
    pub fn set_clipboard(&mut self, clipboard: spherekit_platform::Clipboard) {
        self.clipboard = Arc::new(clipboard);
    }

    /// The focus registry.
    #[inline]
    pub fn focus(&self) -> &FocusRegistry {
        &self.focus
    }

    /// The focus registry, mutably.
    #[inline]
    pub fn focus_mut(&mut self) -> &mut FocusRegistry {
        &mut self.focus
    }

    /// The layout tree, for callers that need geometry directly.
    #[inline]
    pub fn layout(&self) -> &LayoutTree {
        &self.layout
    }

    /// Counters from the most recent frame.
    #[inline]
    pub fn stats(&self) -> TreeStats {
        self.stats
    }

    /// True when layout must run before the next paint.
    #[inline]
    pub fn needs_layout(&self) -> bool {
        self.layout.needs_layout()
    }

    /// True when a repaint is needed.
    #[inline]
    pub fn needs_paint(&self) -> bool {
        self.layout.needs_paint()
    }

    // ------------------------------------------------------------- build

    /// Rebuilds the tree from a root element.
    ///
    /// Nodes are reconciled by identity, so a rebuild that produces the same
    /// structure reuses every node and marks nothing layout-dirty. That is what
    /// makes rebuilding-on-every-frame affordable.
    pub fn build(&mut self, root: AnyElement) {
        self.stats = TreeStats::default();
        self.built.clear();
        self.index_for_node.clear();
        core::mem::swap(&mut self.previous_nodes, &mut self.node_for_element);
        self.node_for_element.clear();
        self.focus.begin_frame();

        // Explicit stack rather than recursion: nesting depth is user-controlled
        // and a deeply nested panel must not overflow.
        struct Pending {
            element: AnyElement,
            parent: Option<usize>,
            identity: ElementId,
            /// Whether an ancestor — or this node — is `display: none`.
            ///
            /// Carried down rather than looked up, because a hidden subtree is
            /// still *built*: the elements exist, they simply have no layout.
            /// Without this the focus ring would happily land inside a closed
            /// dropdown, and Tab would appear to skip into nothing.
            hidden: bool,
        }

        let root_identity = ElementId::from_key("spherekit.root");
        let mut stack: Vec<Pending> =
            vec![Pending { element: root, parent: None, identity: root_identity, hidden: false }];
        let mut child_lists: FxHashMap<usize, SmallVec<[usize; 4]>> = FxHashMap::default();

        while let Some(mut pending) = stack.pop() {
            let children = pending.element.take_children();
            let style = pending.element.layout_style();
            // Hidden and Scroll both establish a clip; Visible does not.
            let clips = !matches!(style.overflow_x, spherekit_layout::Overflow::Visible)
                || !matches!(style.overflow_y, spherekit_layout::Overflow::Visible);
            // Read before `style` is moved into the layout tree.
            let style_display = style.display;

            let node = self.reconcile(pending.identity, style);
            let index = self.built.len();

            if let Some(parent) = pending.parent {
                child_lists.entry(parent).or_default().push(index);
            }

            let hidden = pending.hidden || matches!(style_display, spherekit_layout::Display::None);

            if !hidden && pending.element.focusable() {
                let tab_index = pending.element.semantics().and_then(|s| s.tab_index);
                self.focus.register(Focusable {
                    element: pending.identity,
                    node,
                    tab_index,
                    scope: ScopeId::ROOT,
                    enabled: !pending.element.semantics().is_some_and(|s| s.disabled),
                    bounds: self
                        .layout
                        .layout(node)
                        .map(|l| l.absolute_bounds)
                        .unwrap_or(Rect::ZERO),
                });
            }

            let paints_over = pending.element.paints_over();
            let opacity = pending.element.paint_opacity().clamp(0.0, 1.0);
            let filter = pending.element.paint_filter();
            self.built.push(BuiltNode {
                node,
                element: pending.element,
                children: SmallVec::new(),
                clips,
                opacity,
                filter,
                hidden,
                paints_over,
            });
            self.index_for_node.insert(node, index);
            self.node_for_element.insert(pending.identity, node);
            self.stats.elements += 1;

            // Push in reverse so children pop in order and `child_lists` keeps
            // document order.
            for (i, child) in children.into_iter().enumerate().rev() {
                let identity = child.id().unwrap_or_else(|| pending.identity.child(i));
                stack.push(Pending { element: child, parent: Some(index), identity, hidden });
            }
        }

        for (parent, children) in child_lists {
            let nodes: SmallVec<[NodeId; 8]> =
                children.iter().map(|i| self.built[*i].node).collect();
            let parent_node = self.built[parent].node;
            let _ = self.layout.set_children(parent_node, &nodes);
            self.built[parent].children = children;
        }

        self.root = self.built.first().map(|b| b.node);

        // Anything present last frame and absent now is gone; removing it here
        // is what keeps the layout tree and the node-state map from growing
        // without bound across rebuilds.
        for (element, node) in self.previous_nodes.iter() {
            if !self.node_for_element.contains_key(element) {
                let _ = self.layout.remove(*node);
                self.node_state.remove(node);
                self.stats.nodes_removed += 1;
            }
        }

        self.focus.prune();
    }

    /// Finds or creates the layout node for an identity, updating its style.
    fn reconcile(&mut self, identity: ElementId, style: Style) -> NodeId {
        if let Some(node) = self.previous_nodes.get(&identity).copied()
            && self.layout.contains(node)
        {
            self.stats.nodes_reused += 1;
            // `set_style` is a no-op when the style is unchanged, so an
            // unchanged subtree marks nothing layout-dirty and layout skips it
            // entirely. This is the whole reason rebuilds are cheap.
            let _ = self.layout.set_style(node, style);
            return node;
        }
        self.stats.nodes_created += 1;
        self.layout.insert(style)
    }

    // ------------------------------------------------------------ layout

    /// Computes layout if anything is layout-dirty, with no intrinsic sizing.
    ///
    /// For trees with no text in them. Anything containing a label wants
    /// [`UiTree::compute_layout_with_text`], or its labels will size to zero.
    pub fn compute_layout(
        &mut self,
        viewport: Size<Px>,
    ) -> Result<(), spherekit_core::LayoutError> {
        self.engine.compute(&mut self.layout, viewport)?;
        self.stats.nodes_laid_out = self.engine.stats().nodes_laid_out as u32;
        Ok(())
    }

    /// Computes layout, asking each leaf element for its intrinsic size.
    ///
    /// This is where text shaping enters layout: a label reports how wide it
    /// wants to be, or how tall it is at a given width. The engine skips the
    /// whole pass when nothing is layout-dirty, so a paint-only frame does no
    /// shaping either.
    pub fn compute_layout_with_text(
        &mut self,
        viewport: Size<Px>,
        text: &mut spherekit_text::TextSystem,
    ) -> Result<(), spherekit_core::LayoutError> {
        // Disjoint field borrows: the measure closure holds the element arena
        // while the engine holds the layout tree.
        let built = &mut self.built;
        let index_for_node = &self.index_for_node;
        let theme = &self.theme;
        let mut measure = |request: spherekit_layout::MeasureRequest<'_>| -> Size<Px> {
            let Some(index) = index_for_node.get(&request.node) else { return Size::ZERO };
            let Some(node) = built.get_mut(*index) else { return Size::ZERO };
            node.element.measure(&request, text, theme).unwrap_or(Size::ZERO)
        };
        self.engine.compute_with_measure(&mut self.layout, viewport, &mut measure)?;
        self.stats.nodes_laid_out = self.engine.stats().nodes_laid_out as u32;
        Ok(())
    }

    // ------------------------------------------------------------- paint

    /// Paints the tree into a canvas.
    pub fn paint(
        &mut self,
        canvas: &mut Canvas<'_>,
        text: &mut spherekit_text::TextSystem,
        viewport: Size<Px>,
        time: f32,
    ) {
        self.stats.elements_painted = 0;
        self.stats.elements_culled = 0;
        // Rebuilt every pass rather than remembered: a field that was destroyed,
        // scrolled out of view or blurred since the last frame must stop asking
        // for an input method, and the only reliable signal for that is that it
        // did not ask again.
        self.ime = None;
        // Cleared, not dropped: this runs every frame and reallocating a vector
        // per frame in the paint path is exactly what the engine avoids
        // everywhere else.
        self.caption_exclusions.clear();
        if self.built.is_empty() {
            return;
        }

        let focused_node = self.focus.focused_node();
        let viewport_rect = Rect::new(Point::ZERO, viewport);

        // Explicit stack again, with an "exit" marker so clips and layers are
        // popped in the right order without recursion.
        enum Step {
            Enter(usize, Rect<Px>),
            Exit { restore: bool, layer: bool, over: Option<usize> },
        }
        let mut stack: Vec<Step> = vec![Step::Enter(0, viewport_rect)];

        while let Some(step) = stack.pop() {
            match step {
                Step::Exit { restore, layer, over } => {
                    // Before the clip and the group are torn down, so an
                    // overlay is still confined to the box that owns it.
                    if let Some(index) = over
                        && let Some(built) = self.built.get(index)
                    {
                        let node = built.node;
                        let bounds = self
                            .layout
                            .layout(node)
                            .map(|l| l.absolute_bounds)
                            .unwrap_or(Rect::ZERO);
                        let state = self.node_state.get(&node).copied().unwrap_or_default();
                        let interaction = InteractionState {
                            hovered: state.hovered,
                            active: state.active,
                            focused: focused_node == Some(node),
                            focus_within: focused_node
                                .is_some_and(|f| f == node || self.layout.is_ancestor_of(node, f)),
                            disabled: false,
                        };
                        let metrics = self.scroll_metrics(node);
                        let mut cx = PaintContext {
                            canvas,
                            text,
                            bounds,
                            visible: viewport_rect,
                            scratch: state.scratch,
                            state: interaction,
                            theme: &self.theme,
                            time,
                            ime: &mut self.ime,
                            caption_exclusions: &mut self.caption_exclusions,
                            scroll: metrics,
                        };
                        self.built[index].element.paint_over(&mut cx);
                    }
                    if layer {
                        canvas.end_layer();
                    }
                    if restore {
                        canvas.restore();
                    }
                }
                Step::Enter(index, visible) => {
                    let Some(built) = self.built.get(index) else { continue };
                    // `display: none` leaves the frame entirely: no box, no
                    // glyphs shaped, and — the part that actually bit — no
                    // group opened. An empty layer costs an offscreen target
                    // and splits the surface pass around nothing.
                    if built.hidden {
                        self.stats.elements_culled += 1;
                        continue;
                    }
                    let node = built.node;
                    let bounds =
                        self.layout.layout(node).map(|l| l.absolute_bounds).unwrap_or(Rect::ZERO);

                    if !bounds.intersects(visible) {
                        self.stats.elements_culled += 1;
                        continue;
                    }

                    let state = self.node_state.get(&node).copied().unwrap_or_default();
                    let interaction = InteractionState {
                        hovered: state.hovered,
                        active: state.active,
                        focused: focused_node == Some(node),
                        focus_within: focused_node
                            .is_some_and(|f| f == node || self.layout.is_ancestor_of(node, f)),
                        disabled: false,
                    };

                    let clips = built.clips;
                    let opacity = built.opacity;
                    let filter = built.filter;
                    let needs_group = opacity < 1.0 || filter.is_some();

                    if clips || needs_group {
                        canvas.save();
                        if clips {
                            canvas.clip_rect(bounds);
                        }
                    }
                    let opened_layer = needs_group
                        && canvas.push_layer(
                            bounds,
                            opacity,
                            spherekit_core::BlendMode::Normal,
                            filter,
                        );

                    // The element is borrowed mutably for `paint`, so the
                    // child list is copied out first. Paint order must match
                    // hit-test order: z-index is a visual stacking contract,
                    // not just an input-routing hint.
                    let children = self.built[index].children.clone();
                    let metrics = self.scroll_metrics(node);
                    {
                        let built = &mut self.built[index];
                        let mut cx = PaintContext {
                            canvas,
                            text,
                            bounds,
                            visible,
                            scratch: state.scratch,
                            state: interaction,
                            theme: &self.theme,
                            time,
                            ime: &mut self.ime,
                            caption_exclusions: &mut self.caption_exclusions,
                            scroll: metrics,
                        };
                        built.element.paint(&mut cx);
                    }
                    self.stats.elements_painted += 1;

                    let child_visible = if clips { visible.intersection(bounds) } else { visible };

                    let over = self.built[index].paints_over.then_some(index);
                    if clips || needs_group || over.is_some() {
                        stack.push(Step::Exit {
                            restore: clips || needs_group,
                            layer: opened_layer,
                            over,
                        });
                    }
                    let mut children = children;
                    let needs_sort = children
                        .iter()
                        .any(|child| self.built[*child].element.layout_style().z_index != 0);
                    if needs_sort {
                        children
                            .sort_by_key(|child| self.built[*child].element.layout_style().z_index);
                    }
                    // Reverse so children are entered in bottom-to-top order.
                    for child in children.iter().rev() {
                        stack.push(Step::Enter(*child, child_visible));
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ events

    /// Scrolls the innermost container in `indices` that can still move.
    ///
    /// `indices` is the hit chain, outermost first, so this walks it backwards.
    /// A container that is already at the end of its range in the requested
    /// direction is skipped rather than consuming the gesture — that is what
    /// makes a wheel inside a fully-scrolled list keep moving the page.
    ///
    /// Returns whether anything actually moved.
    fn scroll_chain(&mut self, indices: &[usize], delta: Size<Px>) -> Option<usize> {
        for (position, index) in indices.iter().enumerate().rev() {
            let Some(built) = self.built.get(*index) else { continue };
            let node = built.node;
            let Some(style) = self.layout.style(node).cloned() else { continue };

            let scrolls_x = matches!(style.overflow_x, spherekit_layout::Overflow::Scroll);
            let scrolls_y = matches!(style.overflow_y, spherekit_layout::Overflow::Scroll);
            if !scrolls_x && !scrolls_y {
                continue;
            }

            let max = self.layout.max_scroll_offset(node);
            // Accumulate onto the glide's *destination*, not onto where the
            // content currently is. Otherwise a second notch during the first
            // one's flight would be measured from a moving point and the two
            // would partly cancel.
            let at = match self.scroll_glide.get(&node) {
                Some(glide) => glide.to,
                None => self.layout.scroll_offset(node),
            };
            // A wheel that reports only vertical movement still scrolls a
            // horizontal-only container: it is the axis the container has, not
            // the axis the mouse has, that decides.
            let (dx, dy) = if scrolls_y && !scrolls_x && delta.height == Px::ZERO {
                (Px::ZERO, delta.width)
            } else if scrolls_x && !scrolls_y && delta.width == Px::ZERO {
                (delta.height, Px::ZERO)
            } else {
                (delta.width, delta.height)
            };

            let wants = spherekit_core::size(
                if scrolls_x { at.width - dx } else { at.width },
                if scrolls_y { at.height - dy } else { at.height },
            );
            let clamped = spherekit_core::size(
                wants.width.clamp(Px::ZERO, max.width),
                wants.height.clamp(Px::ZERO, max.height),
            );
            if clamped == at {
                // Already at the end in this direction; let an ancestor try.
                continue;
            }
            // Glide rather than jump. The offset itself is not touched here —
            // `advance` walks it there over the next few frames.
            let from = self.layout.scroll_offset(node);
            self.scroll_glide.insert(node, ScrollGlide { from, to: clamped, elapsed: 0.0 });
            self.layout.mark_dirty(node, DirtyFlags::PAINT);
            return Some(position);
        }
        None
    }

    /// Steps time-based animation the tree owns. Returns whether more is owed.
    ///
    /// Only smooth scrolling for now. Call it once per frame with the elapsed
    /// time and keep drawing while it returns `true`; a window that stops
    /// drawing mid-glide leaves the content parked halfway.
    ///
    /// [`SphereKitSurface::render`](https://docs.rs/spherekit) does this for
    /// you — an application driving a `UiTree` directly does not.
    pub fn advance(&mut self, dt: std::time::Duration) -> bool {
        if self.scroll_glide.is_empty() {
            return false;
        }
        let dt = dt.as_secs_f32();
        let mut finished: SmallVec<[NodeId; 4]> = SmallVec::new();
        let mut running = false;

        for (node, glide) in self.scroll_glide.iter_mut() {
            glide.elapsed += dt;
            let (at, done) = glide.sample();
            let _ = self.layout.set_scroll_offset(*node, at);
            self.layout.mark_dirty(*node, DirtyFlags::PAINT);
            if done {
                finished.push(*node);
            } else {
                running = true;
            }
        }
        for node in finished {
            self.scroll_glide.remove(&node);
        }
        running
    }

    /// What a node is scrolling, as the layout tree currently has it.
    ///
    /// Zero for anything that is not a scroll container, which is almost
    /// everything — the three lookups are cheap and the alternative is every
    /// widget knowing how to ask layout questions.
    fn scroll_metrics(&self, node: NodeId) -> crate::element::ScrollMetrics {
        let Some(layout) = self.layout.layout(node) else {
            return crate::element::ScrollMetrics::default();
        };
        crate::element::ScrollMetrics {
            offset: self.layout.scroll_offset(node),
            content: layout.content_size,
            client: layout.client_size(),
        }
    }

    /// Where the element with this id ended up, after the last layout pass.
    ///
    /// For anchoring something to a box flexbox placed — a tooltip, a popover,
    /// a test that wants to know whether a fixed-height strip actually got its
    /// height. `None` if no element with that id was built.
    pub fn bounds_of(&self, id: impl core::hash::Hash) -> Option<Rect<Px>> {
        let node = self.node_for_element.get(&ElementId::from_key(id))?;
        self.layout.layout(*node).map(|l| l.absolute_bounds)
    }

    /// How far the element with this id is scrolled, if it exists and scrolls.
    ///
    /// Keyed by the element's id rather than by node, because an application
    /// knows the id it wrote and has no reason to know about layout nodes.
    pub fn scroll_offset_of(&self, id: impl core::hash::Hash) -> Option<Size<Px>> {
        let node = self.node_for_element.get(&ElementId::from_key(id))?;
        Some(self.layout.scroll_offset(*node))
    }

    /// Scrolls the element with this id, clamped to its range.
    ///
    /// Returns whether anything moved. The obvious use is putting a pane back
    /// at the top when its content is replaced — without it a reader lands
    /// halfway down a page they have never seen.
    pub fn scroll_element_to(&mut self, id: impl core::hash::Hash, offset: Size<Px>) -> bool {
        let Some(node) = self.node_for_element.get(&ElementId::from_key(id)).copied() else {
            return false;
        };
        self.scroll_glide.remove(&node);
        if self.layout.scroll_offset(node) == offset {
            return false;
        }
        if self.layout.set_scroll_offset(node, offset).is_ok() {
            self.layout.mark_dirty(node, DirtyFlags::PAINT);
            return true;
        }
        false
    }

    /// The hit chain under a point, outermost first.
    pub fn hit_chain(&mut self, point: Point<Px>) -> HitChain {
        self.hit_scratch.clear();
        self.layout.hit_test_all_into(point, &mut self.hit_scratch);
        self.hit_scratch
            .iter()
            .map(|node| HitTarget {
                node: *node,
                element: None,
                bounds: self.layout.layout(*node).map(|l| l.absolute_bounds).unwrap_or(Rect::ZERO),
            })
            .collect()
    }

    /// Dispatches an event.
    pub fn dispatch(&mut self, event: &UiEvent) -> DispatchResult {
        self.dispatch_inner(event, None)
    }

    /// Dispatches with the window's text system available to handlers.
    ///
    /// The pair mirrors [`UiTree::compute_layout`] and
    /// [`UiTree::compute_layout_with_text`], and for the same reason: a text
    /// field cannot turn a click into a caret position without shaping the
    /// string, and a tree with no text in it should not have to own a font
    /// stack to be dispatched to.
    pub fn dispatch_with_text(
        &mut self,
        event: &UiEvent,
        text: &mut spherekit_text::TextSystem,
    ) -> DispatchResult {
        self.dispatch_inner(event, Some(text))
    }

    fn dispatch_inner(
        &mut self,
        event: &UiEvent,
        mut text: Option<&mut spherekit_text::TextSystem>,
    ) -> DispatchResult {
        let mut result = DispatchResult::default();
        let clipboard = Arc::clone(&self.clipboard);

        if event.is_focus_routed() {
            if let Some(handled) = self.dispatch_to_focused(event, text.as_deref_mut(), &clipboard)
            {
                result.merge(handled);
            }
            result.focus_changed |= self.focus.take_changed();
            return result;
        }

        let Some(position) = event.position() else { return result };

        // A captured pointer wins over hit testing entirely. That is the whole
        // point of capture: a fader must keep tracking after the cursor leaves
        // its narrow column.
        let mut indices: SmallVec<[usize; 12]> = if let Some(captured) = self.captured {
            SmallVec::from_slice(&[captured])
        } else {
            self.hit_scratch.clear();
            self.layout.hit_test_all_into(position, &mut self.hit_scratch);
            self.hit_scratch.iter().filter_map(|n| self.index_for_node.get(n).copied()).collect()
        };

        if matches!(event, UiEvent::MouseMove(_)) {
            result.merge(self.update_hover(&indices, position, &clipboard));
        }
        self.update_active(event, &indices);

        // The wheel moves the innermost scroll container under the pointer that
        // still has room to move, before any handler sees the event. Doing it
        // here rather than in `ScrollView` means a plain
        // `div().overflow_y_scroll()` scrolls too, and it gives scroll chaining
        // for nothing: a list that has hit its end passes the wheel to the pane
        // behind it, exactly as every other toolkit does.
        if let UiEvent::Scroll(wheel) = event {
            // A line of body text is the unit a "line" of scrolling means, and
            // a notch is three of them. `to_pixels` only converts, so the
            // notch multiple is folded into the line height handed to it —
            // which leaves a trackpad's pixel deltas untouched, as they must
            // be: those are already the distance the fingers moved.
            // A line of body text is the unit a "line" of scrolling means; how
            // many of them a notch is worth is the user's setting. The whole
            // notch distance is folded into the line height handed to
            // `to_pixels`, which leaves a trackpad's pixel deltas untouched —
            // those are already the distance the fingers moved.
            let line = self.theme.typography.md * self.theme.typography.line_height;
            let viewport = self
                .root
                .and_then(|r| self.layout.layout(r))
                .map(|l| l.client_size().height)
                .unwrap_or(line);
            let notch = wheel_notch_distance(self.wheel_lines, line, viewport);
            let delta = wheel.delta.to_pixels(notch);
            if let Some(position) = self.scroll_chain(&indices, delta) {
                result.repaint = true;
                result.consumed = true;
                // The container that moved has consumed the gesture, so the
                // wheel stops there: an outer pane's own scroll handler must
                // not also fire. `indices` is outermost first, so everything
                // before the mover is an ancestor and is dropped.
                indices = indices[position..].iter().copied().collect();
            }
        }

        let chain: HitChain = indices
            .iter()
            .filter_map(|i| self.built.get(*i))
            .map(|b| HitTarget {
                node: b.node,
                element: None,
                bounds: self.layout.layout(b.node).map(|l| l.absolute_bounds).unwrap_or(Rect::ZERO),
            })
            .collect();

        // Capture: outermost to innermost.
        for index in indices.iter() {
            let flow = self.deliver(
                *index,
                event,
                Phase::Capture,
                &chain,
                &mut result,
                text.as_deref_mut(),
                &clipboard,
            );
            if flow.is_stopped() {
                result.consumed = true;
                result.focus_changed |= self.focus.take_changed();
                return result;
            }
        }
        // Bubble: innermost to outermost.
        for index in indices.iter().rev() {
            let flow = self.deliver(
                *index,
                event,
                Phase::Bubble,
                &chain,
                &mut result,
                text.as_deref_mut(),
                &clipboard,
            );
            if flow.is_stopped() {
                result.consumed = true;
                break;
            }
        }

        result.focus_changed |= self.focus.take_changed();
        result
    }

    fn dispatch_to_focused(
        &mut self,
        event: &UiEvent,
        mut text: Option<&mut spherekit_text::TextSystem>,
        clipboard: &spherekit_platform::Clipboard,
    ) -> Option<DispatchResult> {
        let node = self.focus.focused_node()?;
        let index = *self.index_for_node.get(&node)?;
        let mut result = DispatchResult::default();

        // Keyboard events bubble from the focused element to the root, so a
        // container can implement a shortcut its children did not handle.
        let mut chain_indices: SmallVec<[usize; 12]> = SmallVec::new();
        chain_indices.push(index);
        let mut current = node;
        while let Some(parent) = self.layout.parent(current) {
            if let Some(i) = self.index_for_node.get(&parent) {
                chain_indices.push(*i);
            }
            current = parent;
        }

        let chain: HitChain = chain_indices
            .iter()
            .rev()
            .filter_map(|i| self.built.get(*i))
            .map(|b| HitTarget {
                node: b.node,
                element: None,
                bounds: self.layout.layout(b.node).map(|l| l.absolute_bounds).unwrap_or(Rect::ZERO),
            })
            .collect();

        for index in chain_indices {
            let flow = self.deliver(
                index,
                event,
                Phase::Bubble,
                &chain,
                &mut result,
                text.as_deref_mut(),
                clipboard,
            );
            if flow.is_stopped() {
                result.consumed = true;
                break;
            }
        }
        Some(result)
    }

    fn deliver(
        &mut self,
        index: usize,
        event: &UiEvent,
        phase: Phase,
        chain: &[HitTarget],
        result: &mut DispatchResult,
        text: Option<&mut spherekit_text::TextSystem>,
        clipboard: &spherekit_platform::Clipboard,
    ) -> EventFlow {
        let Some(built) = self.built.get(index) else { return EventFlow::Continue };
        let node = built.node;
        let bounds = self.layout.layout(node).map(|l| l.absolute_bounds).unwrap_or(Rect::ZERO);

        let metrics = self.scroll_metrics(node);

        let scratch_before = self.node_state.entry(node).or_default().scratch;
        let mut scratch = scratch_before;

        // The context borrows `scratch`, so it is scoped and its outputs are
        // copied out before the borrow is inspected.
        let (flow, cx) = {
            let mut cx = EventContext {
                event,
                phase,
                bounds,
                chain,
                scratch: &mut scratch,
                repaint: false,
                relayout: false,
                request_focus: false,
                capture_pointer: false,
                release_pointer: false,
                cursor: None,
                scroll: metrics,
                scroll_to: None,
                text,
                clipboard,
                theme: &self.theme,
            };
            let flow = self.built[index].element.handle_event(&mut cx);
            let outputs = EventOutputs {
                repaint: cx.repaint,
                relayout: cx.relayout,
                request_focus: cx.request_focus,
                capture_pointer: cx.capture_pointer,
                release_pointer: cx.release_pointer,
                cursor: cx.cursor,
                scroll_to: cx.scroll_to,
            };
            (flow, outputs)
        };
        if scratch != scratch_before {
            self.node_state.entry(node).or_default().scratch = scratch;
        }

        if let Some(target) = cx.scroll_to {
            // A dragged thumb must track the pointer exactly, so it wins over
            // any glide still in flight rather than fighting it.
            self.scroll_glide.remove(&node);
            // Scrolling changes where children are drawn, not how big they are,
            // so this is a repaint and never a relayout. That is the whole
            // reason a long list stays cheap to scroll.
            if self.layout.set_scroll_offset(node, target).is_ok() {
                self.layout.mark_dirty(node, DirtyFlags::PAINT);
                result.repaint = true;
            }
        }
        if cx.repaint {
            self.layout.mark_dirty(node, DirtyFlags::PAINT);
            result.repaint = true;
        }
        if cx.relayout {
            self.layout.mark_dirty(node, DirtyFlags::LAYOUT);
            result.relayout = true;
        }
        if cx.request_focus {
            for (element, n) in self.node_for_element.iter() {
                if *n == node {
                    self.focus.focus(Some(*element));
                    break;
                }
            }
        }
        if cx.capture_pointer {
            self.captured = Some(index);
        }
        if cx.release_pointer {
            self.captured = None;
        }
        if cx.cursor.is_some() {
            result.cursor = cx.cursor;
        }
        flow
    }

    /// Updates hover state and synthesises enter and leave events.
    ///
    /// Enter and leave must fire exactly once per boundary crossing. Deriving
    /// them from "is the pointer inside" on every move produces a storm of
    /// duplicate events and makes hover animations restart every frame.
    fn update_hover(
        &mut self,
        indices: &[usize],
        position: Point<Px>,
        clipboard: &spherekit_platform::Clipboard,
    ) -> DispatchResult {
        let mut result = DispatchResult::default();
        let new_chain: SmallVec<[usize; 12]> = SmallVec::from_slice(indices);

        let left: SmallVec<[usize; 8]> =
            self.hovered_chain.iter().copied().filter(|i| !new_chain.contains(i)).collect();
        let entered: SmallVec<[usize; 8]> =
            new_chain.iter().copied().filter(|i| !self.hovered_chain.contains(i)).collect();

        if left.is_empty() && entered.is_empty() {
            return result;
        }

        let synth = |kind: fn(MouseMoveEvent) -> UiEvent| {
            kind(MouseMoveEvent {
                position,
                delta: Size::default(),
                buttons: SmallVec::new(),
                modifiers: Modifiers::NONE,
            })
        };

        for index in left {
            if let Some(built) = self.built.get(index) {
                let node = built.node;
                self.node_state.entry(node).or_default().hovered = false;
                self.layout.mark_dirty(node, DirtyFlags::PAINT);
            }
            let ev = synth(UiEvent::MouseLeave);
            self.deliver(index, &ev, Phase::Bubble, &[], &mut result, None, clipboard);
            result.repaint = true;
        }
        for index in entered {
            if let Some(built) = self.built.get(index) {
                let node = built.node;
                self.node_state.entry(node).or_default().hovered = true;
                self.layout.mark_dirty(node, DirtyFlags::PAINT);
            }
            let ev = synth(UiEvent::MouseEnter);
            self.deliver(index, &ev, Phase::Bubble, &[], &mut result, None, clipboard);
            result.repaint = true;
        }

        self.hovered_chain = new_chain;
        result
    }

    fn update_active(&mut self, event: &UiEvent, indices: &[usize]) {
        match event {
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                if let Some(index) = indices.last()
                    && let Some(built) = self.built.get(*index)
                {
                    let node = built.node;
                    self.node_state.entry(node).or_default().active = true;
                    self.layout.mark_dirty(node, DirtyFlags::PAINT);
                }
            }
            UiEvent::MouseUp(e) if e.button == MouseButton::Primary => {
                // Clear every active node, not just the one under the cursor: a
                // press that ends outside its element must still un-press it.
                for state in self.node_state.values_mut() {
                    state.active = false;
                }
                if let Some(root) = self.root {
                    self.layout.mark_dirty(root, DirtyFlags::PAINT);
                }
                self.captured = None;
            }
            _ => {}
        }
    }

    /// Moves keyboard focus.
    pub fn navigate_focus(&mut self, direction: FocusDirection) -> bool {
        let moved = self.focus.navigate(direction).is_some();
        if moved && let Some(root) = self.root {
            self.layout.mark_dirty(root, DirtyFlags::PAINT);
        }
        moved
    }

    /// Clears paint dirtiness after a frame has been rendered.
    pub fn end_frame(&mut self) {
        self.layout.clear_paint_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{Interactive, IntoElement, ParentElement, Styled, div};
    use crate::event::{ElementState, MouseButtonEvent};
    use crate::style::StyledInteraction;
    use spherekit_core::{Color, ScaleFactor, px, relative, size};
    use spherekit_render::Scene;
    use std::cell::Cell;
    use std::rc::Rc;

    fn viewport() -> Size<Px> {
        size(px(400.0), px(300.0))
    }

    fn build_and_layout(tree: &mut UiTree, root: AnyElement) {
        tree.build(root);
        tree.compute_layout(viewport()).unwrap();
    }

    fn move_to(x: f32, y: f32) -> UiEvent {
        UiEvent::MouseMove(MouseMoveEvent {
            position: Point::new(px(x), px(y)),
            delta: Size::default(),
            buttons: SmallVec::new(),
            modifiers: Modifiers::NONE,
        })
    }

    fn click_up_at(x: f32, y: f32) -> UiEvent {
        UiEvent::MouseUp(MouseButtonEvent {
            position: Point::new(px(x), px(y)),
            button: MouseButton::Primary,
            state: ElementState::Released,
            click_count: 1,
            modifiers: Modifiers::NONE,
        })
    }

    #[test]
    fn a_simple_row_lays_out_its_children() {
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .flex_row()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(div().w(px(100.0)))
                .child(div().flex_1())
                .into_element(),
        );
        assert_eq!(tree.stats().elements, 3);
        let first = tree.built[1].node;
        let second = tree.built[2].node;
        assert_eq!(tree.layout().layout(first).unwrap().bounds.size.width, px(100.0));
        assert_eq!(tree.layout().layout(second).unwrap().bounds.size.width, px(300.0));
    }

    #[test]
    fn an_identical_rebuild_reuses_every_node_and_relayouts_nothing() {
        // The headline property: rebuilding on every frame is affordable only
        // if an unchanged tree costs no layout.
        let make = || {
            div()
                .flex_col()
                .w(relative(1.0))
                .child(div().id("a").h(px(20.0)))
                .child(div().id("b").h(px(30.0)))
                .into_element()
        };
        let mut tree = UiTree::new();
        build_and_layout(&mut tree, make());
        assert_eq!(tree.stats().nodes_created, 3);

        tree.build(make());
        assert_eq!(tree.stats().nodes_created, 0, "an unchanged tree created nodes");
        assert_eq!(tree.stats().nodes_reused, 3);
        tree.compute_layout(viewport()).unwrap();
        assert_eq!(tree.stats().nodes_laid_out, 0, "an unchanged tree was relaid out");
    }

    #[test]
    fn changing_a_size_relayouts_but_changing_nothing_does_not() {
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div().flex_col().child(div().id("a").h(px(20.0))).into_element(),
        );
        tree.build(div().flex_col().child(div().id("a").h(px(40.0))).into_element());
        tree.compute_layout(viewport()).unwrap();
        assert!(tree.stats().nodes_laid_out > 0, "a size change must relayout");
    }

    #[test]
    fn removed_elements_release_their_nodes() {
        let mut tree = UiTree::new();
        build_and_layout(&mut tree, div().child(div().id("a")).child(div().id("b")).into_element());
        tree.build(div().child(div().id("a")).into_element());
        assert_eq!(tree.stats().nodes_removed, 1, "a dropped element must not leak its node");
    }

    #[test]
    fn identity_survives_reordering_when_keys_are_supplied() {
        let mut tree = UiTree::new();
        build_and_layout(&mut tree, div().child(div().id("a")).child(div().id("b")).into_element());
        let node_a = *tree.node_for_element.get(&ElementId::from_key("a")).unwrap();

        tree.build(div().child(div().id("b")).child(div().id("a")).into_element());
        let node_a_after = *tree.node_for_element.get(&ElementId::from_key("a")).unwrap();
        assert_eq!(node_a, node_a_after, "a keyed element must keep its node when reordered");
        assert_eq!(tree.stats().nodes_created, 0);
    }

    #[test]
    fn painting_produces_commands_only_for_visible_elements() {
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .bg(Color::RED)
                .child(div().w(px(50.0)).h(px(50.0)).bg(Color::BLUE))
                .into_element(),
        );
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            let mut text = spherekit_text::TextSystem::new();
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
        assert_eq!(scene.len(), 2);
        assert_eq!(tree.stats().elements_painted, 2);
    }

    #[test]
    fn a_hidden_transparent_sibling_does_not_erase_what_was_painted_before_it() {
        // A closed dropdown is `display: none` *and* fully transparent. If the
        // walk still opens a group for it, the layer it pushes is empty and the
        // renderer resolves it by re-clearing the target — taking everything
        // already drawn with it.
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .flex_col()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(div().w(px(50.0)).h(px(50.0)).bg(Color::BLUE))
                .child(div().hidden().opacity(0.0).child(div().w(px(10.0)).h(px(10.0))))
                .into_element(),
        );
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            let mut text = spherekit_text::TextSystem::new();
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
        assert_eq!(scene.layers.len(), 0, "an out-of-layout element opened a layer");
        assert_eq!(scene.len(), 1, "expected just the blue quad, got {:?}", scene.commands);
    }

    #[test]
    fn a_layout_container_with_no_appearance_records_nothing() {
        let mut tree = UiTree::new();
        build_and_layout(&mut tree, div().flex_col().child(div()).into_element());
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            let mut text = spherekit_text::TextSystem::new();
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
        assert!(scene.is_empty(), "pure layout containers must not cost draw commands");
    }

    #[test]
    fn a_clipping_container_narrows_the_visible_region_for_its_children() {
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(px(50.0))
                .h(px(50.0))
                .overflow(spherekit_layout::Overflow::Hidden)
                .child(div().absolute().w(px(20.0)).h(px(20.0)).bg(Color::RED))
                .into_element(),
        );
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            let mut text = spherekit_text::TextSystem::new();
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
        assert!(scene.clips.len() > 1, "a clipping container must push a clip");
    }

    fn nested(depth: usize) -> AnyElement {
        let mut root = div().w(relative(1.0)).h(relative(1.0)).into_element();
        for _ in 0..depth {
            root = div().w(relative(1.0)).h(relative(1.0)).child(root).into_element();
        }
        root
    }

    #[test]
    fn the_build_walk_is_iterative_at_extreme_depth() {
        // Nesting depth is user-controlled, so this crate's own walks must not
        // recurse. Five thousand levels is far past anything real and exists
        // purely to catch a recursive rewrite.
        let mut tree = UiTree::new();
        tree.build(nested(5_000));
        assert_eq!(tree.stats().elements, 5_001);
        // Dropping the arena must also be flat: children were moved out of
        // their parents during build, so no nested Box chain survives.
        drop(tree);
    }

    #[test]
    fn the_paint_walk_is_iterative_at_extreme_depth() {
        let mut tree = UiTree::new();
        tree.build(nested(5_000));
        // Painting without a layout pass leaves every box at the origin with
        // zero extent, which still exercises the full walk and its clip stack.
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            let mut text = spherekit_text::TextSystem::new();
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
    }

    #[test]
    fn a_realistically_deep_tree_lays_out_and_paints() {
        // Layout recursion depth is bounded by the backing layout engine, not
        // by this crate. Two hundred levels is an order of magnitude past any
        // real interface — a deeply nested DAW mixer strip is nearer twenty —
        // and comfortably inside what the engine handles.
        let mut tree = UiTree::new();
        tree.build(nested(200));
        tree.compute_layout(viewport()).unwrap();
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            let mut text = spherekit_text::TextSystem::new();
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
        assert_eq!(tree.stats().elements, 201);
        assert!(tree.stats().elements_painted > 0);
    }

    #[test]
    fn a_wide_tree_of_many_nodes_lays_out() {
        // The other axis of scale: a mixer with hundreds of strips is wide, not
        // deep, and must stay comfortably within budget.
        let mut root = div().flex_row().w(relative(1.0)).h(relative(1.0));
        for i in 0..500 {
            root = root.child(div().id(i).w(px(2.0)).h(relative(1.0)));
        }
        let mut tree = UiTree::new();
        tree.build(root.into_element());
        tree.compute_layout(viewport()).unwrap();
        assert_eq!(tree.stats().elements, 501);
    }

    #[test]
    fn a_click_reaches_the_element_under_the_cursor() {
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(
                    div()
                        .w(px(100.0))
                        .h(px(40.0))
                        .bg(Color::BLUE)
                        .on_click(move |_| h.set(h.get() + 1)),
                )
                .into_element(),
        );
        let result = tree.dispatch(&click_up_at(50.0, 20.0));
        assert_eq!(hits.get(), 1);
        assert!(result.consumed);
    }

    #[test]
    fn a_click_outside_every_handler_is_not_consumed() {
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(div().w(px(10.0)).h(px(10.0)))
                .into_element(),
        );
        let result = tree.dispatch(&click_up_at(300.0, 200.0));
        assert!(!result.consumed);
    }

    #[test]
    fn hover_enter_and_leave_fire_once_per_boundary_crossing() {
        let enters = Rc::new(Cell::new(0));
        let leaves = Rc::new(Cell::new(0));
        let (e, l) = (enters.clone(), leaves.clone());
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(
                    div()
                        .w(px(100.0))
                        .h(px(40.0))
                        .on_mouse_enter(move |_| e.set(e.get() + 1))
                        .on_mouse_leave(move |_| l.set(l.get() + 1)),
                )
                .into_element(),
        );

        tree.dispatch(&move_to(50.0, 20.0));
        tree.dispatch(&move_to(60.0, 25.0));
        tree.dispatch(&move_to(70.0, 30.0));
        assert_eq!(enters.get(), 1, "enter fired more than once while staying inside");
        assert_eq!(leaves.get(), 0);

        tree.dispatch(&move_to(300.0, 200.0));
        assert_eq!(leaves.get(), 1);
        tree.dispatch(&move_to(310.0, 210.0));
        assert_eq!(leaves.get(), 1, "leave fired again while already outside");
    }

    #[test]
    fn hovering_marks_paint_dirty_and_never_layout_dirty() {
        // The single most important invariant in the engine.
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(div().w(px(100.0)).h(px(40.0)).bg(Color::BLUE).hover_bg(Color::RED))
                .into_element(),
        );
        tree.end_frame();
        tree.dispatch(&move_to(50.0, 20.0));
        assert!(tree.needs_paint(), "hover must request a repaint");

        tree.compute_layout(viewport()).unwrap();
        assert_eq!(tree.stats().nodes_laid_out, 0, "hover triggered a relayout");
    }

    #[test]
    fn a_captured_pointer_keeps_receiving_moves_outside_its_bounds() {
        // Without capture, a fader stops tracking the moment the cursor leaves
        // its narrow column.
        let moves = Rc::new(Cell::new(0));
        let m = moves.clone();
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(
                    div()
                        .w(px(20.0))
                        .h(px(100.0))
                        .on_mouse_down(|cx| cx.capture())
                        .on_mouse_move(move |_| m.set(m.get() + 1)),
                )
                .into_element(),
        );

        tree.dispatch(&UiEvent::MouseDown(MouseButtonEvent {
            position: Point::new(px(10.0), px(50.0)),
            button: MouseButton::Primary,
            state: ElementState::Pressed,
            click_count: 1,
            modifiers: Modifiers::NONE,
        }));
        tree.dispatch(&move_to(350.0, 250.0));
        assert_eq!(moves.get(), 1, "a captured element stopped receiving moves");
    }

    #[test]
    fn releasing_the_button_clears_capture_and_active_state() {
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(div().w(px(20.0)).h(px(100.0)).on_mouse_down(|cx| cx.capture()))
                .into_element(),
        );
        tree.dispatch(&UiEvent::MouseDown(MouseButtonEvent {
            position: Point::new(px(10.0), px(50.0)),
            button: MouseButton::Primary,
            state: ElementState::Pressed,
            click_count: 1,
            modifiers: Modifiers::NONE,
        }));
        assert!(tree.captured.is_some());
        tree.dispatch(&click_up_at(350.0, 250.0));
        assert!(tree.captured.is_none());
        assert!(tree.node_state.values().all(|s| !s.active));
    }

    #[test]
    fn events_bubble_from_the_innermost_element_outward() {
        let order = Rc::new(std::cell::RefCell::new(Vec::new()));
        let (o1, o2) = (order.clone(), order.clone());
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .on_mouse_up(move |_| o1.borrow_mut().push("outer"))
                .child(
                    div()
                        .w(px(100.0))
                        .h(px(40.0))
                        .on_mouse_up(move |_| o2.borrow_mut().push("inner")),
                )
                .into_element(),
        );
        tree.dispatch(&click_up_at(50.0, 20.0));
        assert_eq!(*order.borrow(), vec!["inner", "outer"]);
    }

    #[test]
    fn setting_a_theme_marks_paint_dirty_not_layout_dirty() {
        let mut tree = UiTree::new();
        build_and_layout(&mut tree, div().w(px(10.0)).h(px(10.0)).into_element());
        tree.end_frame();
        tree.set_theme(Theme::light());
        assert!(tree.needs_paint());
        tree.compute_layout(viewport()).unwrap();
        assert_eq!(tree.stats().nodes_laid_out, 0, "a colour change relaid out the tree");
    }

    #[test]
    fn an_empty_tree_paints_and_dispatches_without_panicking() {
        let mut tree = UiTree::new();
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            let mut text = spherekit_text::TextSystem::new();
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
        assert!(scene.is_empty());
        assert!(!tree.dispatch(&click_up_at(10.0, 10.0)).consumed);
    }

    #[test]
    fn focusable_elements_register_and_tab_moves_between_them() {
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .flex_row()
                .w(relative(1.0))
                .child(div().id("a").focusable().w(px(50.0)).h(px(20.0)))
                .child(div().id("b").focusable().w(px(50.0)).h(px(20.0)))
                .into_element(),
        );
        assert_eq!(tree.focus().len(), 2);
        assert!(tree.navigate_focus(FocusDirection::Next));
        assert_eq!(tree.focus().focused(), Some(ElementId::from_key("a")));
        assert!(tree.navigate_focus(FocusDirection::Next));
        assert_eq!(tree.focus().focused(), Some(ElementId::from_key("b")));
    }

    #[test]
    fn the_wheel_setting_decides_the_notch_distance() {
        let line = px(18.0);
        let viewport = px(600.0);

        // No setting readable: the platform default.
        assert_eq!(wheel_notch_distance(None, line, viewport), line * WHEEL_LINES_PER_NOTCH);
        // The usual Windows value.
        assert_eq!(wheel_notch_distance(Some(3), line, viewport), px(54.0));
        // A reader who turned it up gets what they asked for.
        assert_eq!(wheel_notch_distance(Some(10), line, viewport), px(180.0));
        // Zero is the documented "wheel does not scroll" value.
        assert_eq!(wheel_notch_distance(Some(0), line, viewport), Px::ZERO);
    }

    #[test]
    fn one_screen_at_a_time_scrolls_a_screen_not_four_million_lines() {
        // `WHEEL_PAGESCROLL` is `u32::MAX`. Multiplying it by a line height is
        // the obvious reading and would jump roughly 77 million pixels, which
        // lands every scroll at the end of the content.
        let line = px(18.0);
        let viewport = px(600.0);
        let page =
            wheel_notch_distance(Some(spherekit_platform::WHEEL_SCROLL_PAGE), line, viewport);
        assert!(page > line, "a page should move more than a line");
        assert!(page < viewport, "a page should keep some context on screen");
        assert_eq!(page, px(540.0));

        // And it must stay sane when the viewport is tiny: never less than a
        // line, or the wheel would appear dead in a short pane.
        assert_eq!(
            wheel_notch_distance(Some(spherekit_platform::WHEEL_SCROLL_PAGE), line, px(4.0)),
            line
        );
    }

    #[test]
    fn a_hidden_subtree_is_not_in_the_tab_order() {
        // A closed dropdown is still *built* — its rows exist as elements and
        // only lose their layout. Registering them would let Tab walk into a
        // panel nobody can see, and the focus ring would vanish for three
        // presses running.
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .flex_col()
                .w(relative(1.0))
                .child(div().id("visible").focusable().w(px(50.0)).h(px(20.0)))
                .child(
                    div()
                        .id("panel")
                        .hidden()
                        .child(div().id("buried").focusable().w(px(50.0)).h(px(20.0))),
                )
                .child(div().id("after").focusable().w(px(50.0)).h(px(20.0)))
                .into_element(),
        );

        assert_eq!(tree.focus().len(), 2, "a hidden row was registered as focusable");
        assert!(tree.navigate_focus(FocusDirection::Next));
        assert_eq!(tree.focus().focused(), Some(ElementId::from_key("visible")));
        // Straight past the hidden panel, not into it.
        assert!(tree.navigate_focus(FocusDirection::Next));
        assert_eq!(tree.focus().focused(), Some(ElementId::from_key("after")));
    }

    #[test]
    fn a_subtree_that_stops_being_hidden_rejoins_the_tab_order() {
        let build = |tree: &mut UiTree, open: bool| {
            let mut panel = div().id("panel");
            if !open {
                panel = panel.hidden();
            }
            build_and_layout(
                tree,
                div()
                    .flex_col()
                    .w(relative(1.0))
                    .child(div().id("visible").focusable().w(px(50.0)).h(px(20.0)))
                    .child(panel.child(div().id("buried").focusable().w(px(50.0)).h(px(20.0))))
                    .into_element(),
            );
        };
        let mut tree = UiTree::new();
        build(&mut tree, false);
        assert_eq!(tree.focus().len(), 1);
        build(&mut tree, true);
        assert_eq!(tree.focus().len(), 2, "opening the panel did not restore its row");
    }

    #[test]
    fn keyboard_events_go_to_the_focused_element_regardless_of_the_pointer() {
        let keys = Rc::new(Cell::new(0));
        let k = keys.clone();
        let mut tree = UiTree::new();
        build_and_layout(
            &mut tree,
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(div().id("field").focusable().w(px(50.0)).h(px(20.0)).on_key(move |_| {
                    k.set(k.get() + 1);
                    EventFlow::Stop
                }))
                .into_element(),
        );
        tree.navigate_focus(FocusDirection::Next);
        let result = tree.dispatch(&UiEvent::Key(crate::event::KeyEvent {
            key: crate::event::Key::Enter,
            state: ElementState::Pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        }));
        assert_eq!(keys.get(), 1);
        assert!(result.consumed);
    }
}

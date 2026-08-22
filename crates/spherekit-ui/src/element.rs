//! Elements: the declarative authoring surface over a retained tree.
//!
//! ```ignore
//! div()
//!     .flex_col()
//!     .gap(px(8.0))
//!     .p(px(12.0))
//!     .bg(theme.surface)
//!     .rounded(px(6.0))
//!     .child(label("Threshold").text_size(px(12.0)))
//!     .child(div().h(px(64.0)).bg(theme.accent))
//! ```
//!
//! The code reads immediate — build a tree, hand it over, done. What actually
//! happens is retained: the tree is reconciled against last frame's nodes by
//! [`ElementId`], layout is cached, and only nodes whose style actually changed
//! are re-laid out. Authoring ergonomics and update cost are separate concerns,
//! and this is where they are separated.
//!
//! ## What an element is responsible for
//!
//! Very little, deliberately. An element describes its own box — a layout
//! style, a paint style, its children — and paints that box. It does not walk
//! the tree, compute layout, hit test, or dispatch events; the framework does
//! all of that once, for everything. A widget author writes `paint` and
//! sometimes `handle_event`, and nothing else.

use crate::event::{EventFlow, HitTarget, Phase, UiEvent};
use crate::style::PaintStyle;
use smallvec::SmallVec;
use spherekit_core::{Color, Corners, ElementId, Length, Px, Rect, Size};
use spherekit_layout::{MeasureRequest, Style};
use spherekit_render::{Canvas, Filter};

/// Interaction state the framework resolves before painting.
///
/// Passed in rather than stored on the element, because an element is rebuilt
/// every frame and hover state is not — it belongs to the node, which persists.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct InteractionState {
    /// The pointer is over this element.
    pub hovered: bool,
    /// A button is held down on this element.
    pub active: bool,
    /// This element has keyboard focus.
    pub focused: bool,
    /// This element or a descendant has keyboard focus.
    pub focus_within: bool,
    /// This element is disabled and does not accept input.
    pub disabled: bool,
}

/// What an element needs in order to paint itself.
pub struct PaintContext<'a, 'canvas> {
    /// The canvas to record into.
    pub canvas: &'a mut Canvas<'canvas>,
    /// The text system, for shaping and drawing glyph runs.
    ///
    /// Handed in rather than stored on the element because it is shared by the
    /// whole window: one atlas, one font database, one shaping cache.
    pub text: &'a mut spherekit_text::TextSystem,
    /// This element's absolute bounds in window-logical pixels.
    pub bounds: Rect<Px>,
    /// The visible region after clipping, for content-level culling.
    pub visible: Rect<Px>,
    /// This node's retained scratch, as last written by an event handler.
    ///
    /// Read-only while painting: paint must be a pure function of state, or two
    /// frames with the same state would draw differently.
    pub scratch: [f32; 4],
    /// Resolved interaction state.
    pub state: InteractionState,
    /// The active theme.
    pub theme: &'a crate::theme::Theme,
    /// Seconds since the engine started, for animated painting.
    pub time: f32,
    /// Where a focused editable element wants the input method to appear.
    ///
    /// Written through [`PaintContext::request_ime`] rather than assigned. A
    /// widget that draws no caret leaves it alone, and the tree reads whatever
    /// the pass left behind.
    pub ime: &'a mut Option<ImeArea>,
    /// Boxes the platform must not treat as a title bar.
    ///
    /// Written through [`PaintContext::keep_interactive`]. Appended to rather
    /// than replaced, because every interactive element in the pass contributes.
    pub caption_exclusions: &'a mut Vec<Rect<Px>>,
}

/// An editable element's request for input-method composition.
///
/// Produced during paint rather than asked for through a trait method, because
/// both halves of the answer — *whether* this element edits text and *where* its
/// caret is on screen — are only known once it has been laid out and is drawing
/// itself. A trait method would have to be answered before layout, when the
/// caret has no position yet.
///
/// The application turns this into [`spherekit_platform::Window::set_ime_allowed`]
/// and [`spherekit_platform::Window::set_ime_cursor_area`]. The second one is not
/// optional: without it a CJK candidate window opens in the corner of the screen
/// instead of under the caret, which is a usability failure for exactly the
/// languages that need an input method at all.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ImeArea {
    /// The caret, in window-logical pixels, where the candidate window goes.
    pub caret: Rect<Px>,
}

impl PaintContext<'_, '_> {
    /// Asks the platform to enable input-method composition, with the caret
    /// here.
    ///
    /// Call it only while focused: enabling an input method for an unfocused
    /// field would put a candidate window over a control the user is not typing
    /// into. The last call in a paint pass wins, which is the right rule because
    /// only one element can hold focus.
    #[inline]
    pub fn request_ime(&mut self, caret: Rect<Px>) {
        *self.ime = Some(ImeArea { caret });
    }

    /// Declares that this element must keep receiving clicks.
    ///
    /// Only matters under a custom window frame. A press the platform resolves
    /// as the title bar is swallowed by its modal move loop, so a button inside
    /// the caption that has not said this receives no click **ever** — not
    /// merely a delayed one, and with no error to notice.
    ///
    /// Declared here rather than listed by the application because the
    /// application does not know where its buttons ended up: flexbox does.
    /// Hand-maintaining the list is the whole bug class this removes.
    ///
    /// Harmless off the caption. An exclusion nowhere near the title bar can
    /// never change a hit test, because the point would have to be inside a drag
    /// region to be affected and it is not.
    #[inline]
    pub fn keep_interactive(&mut self) {
        self.caption_exclusions.push(self.bounds);
    }

    /// True when this element's box is entirely outside the visible region.
    ///
    /// Widgets that generate expensive content — a waveform, a long list —
    /// should check this before doing the work, not after.
    #[inline]
    pub fn is_culled(&self) -> bool {
        !self.bounds.intersects(self.visible)
    }
}

/// What an element needs in order to respond to an event.
pub struct EventContext<'a> {
    /// The event.
    pub event: &'a UiEvent,
    /// Which dispatch pass this is.
    pub phase: Phase,
    /// This element's absolute bounds.
    pub bounds: Rect<Px>,
    /// The full hit chain, outermost first, for elements that need context.
    pub chain: &'a [HitTarget],
    /// Four floats of scratch that survive a rebuild.
    ///
    /// Where a drag origin, a caret index or an accumulated wheel remainder
    /// lives. The element is rebuilt every frame; the node it drives is not.
    pub scratch: &'a mut [f32; 4],
    /// Set to request a repaint without a relayout.
    pub repaint: bool,
    /// Set to request a relayout.
    pub relayout: bool,
    /// Set to take keyboard focus.
    pub request_focus: bool,
    /// Set to capture the pointer until the button is released.
    pub capture_pointer: bool,
    /// Set to release a previously captured pointer.
    pub release_pointer: bool,
    /// A cursor to show while over this element.
    pub cursor: Option<crate::style::Cursor>,
    /// The window's text system, when the caller supplied one.
    ///
    /// Present for events dispatched through [`crate::tree::UiTree::dispatch_with_text`]
    /// and absent otherwise. A text field needs it to turn a click into a byte
    /// offset, which cannot be done without shaping the string — and shaping it
    /// here is cheap, because paint shaped the same string with the same style
    /// moments earlier and the shaping cache still holds it.
    ///
    /// `Option` rather than required, so a tree can still be driven, tested and
    /// dispatched to without a font stack.
    pub text: Option<&'a mut spherekit_text::TextSystem>,
    /// The application's text clipboard.
    ///
    /// The field uses this for the standard copy, cut and paste shortcuts.
    /// It is supplied by the UI tree so an embedder can replace the system
    /// clipboard with one owned by its host.
    pub clipboard: &'a spherekit_platform::Clipboard,
    /// The active theme.
    ///
    /// Needed because a widget's *geometry* can depend on it. A text field turns
    /// a click into a caret index by laying its string out again, and it has to
    /// lay it out at the same size it painted at or the caret lands on the wrong
    /// character. Without the theme here that size would have to be duplicated
    /// on the widget and kept in sync by hand.
    pub theme: &'a crate::theme::Theme,
}

impl EventContext<'_> {
    /// Position of the event in this element's local space, if it has one.
    #[inline]
    pub fn local_position(&self) -> Option<spherekit_core::Point<Px>> {
        let p = self.event.position()?;
        Some(spherekit_core::Point::new(p.x - self.bounds.min_x(), p.y - self.bounds.min_y()))
    }

    /// Marks the element as needing a repaint.
    ///
    /// This is the cheap invalidation: no layout, no reshaping, no work on any
    /// other element.
    #[inline]
    pub fn notify(&mut self) {
        self.repaint = true;
    }

    /// Marks the element as needing a relayout, which implies a repaint.
    #[inline]
    pub fn notify_layout(&mut self) {
        self.relayout = true;
        self.repaint = true;
    }

    /// Requests keyboard focus for this element.
    #[inline]
    pub fn focus(&mut self) {
        self.request_focus = true;
    }

    /// Captures the pointer, so drags continue outside this element's bounds.
    ///
    /// Without this a fader stops tracking the moment the cursor leaves its
    /// narrow column, which is the single most common feel bug in audio UI.
    #[inline]
    pub fn capture(&mut self) {
        self.capture_pointer = true;
    }

    /// Releases a captured pointer.
    #[inline]
    pub fn release(&mut self) {
        self.release_pointer = true;
    }

    /// Sets the cursor while the pointer is over this element.
    #[inline]
    pub fn set_cursor(&mut self, cursor: crate::style::Cursor) {
        self.cursor = Some(cursor);
    }
}

/// Anything that can appear in the element tree.
pub trait Element: 'static {
    /// Stable identity, used to reconcile against last frame's node.
    ///
    /// An element without an id gets a positional identity derived from its
    /// parent and index. That is correct for static structure and wrong for a
    /// reorderable list, which is why keyed lists must supply one.
    fn id(&self) -> Option<ElementId> {
        None
    }

    /// The layout style for this element's own node.
    fn layout_style(&self) -> Style;

    /// This element's children, in order.
    fn children(&mut self) -> &mut [AnyElement] {
        &mut []
    }

    /// Removes and returns this element's children.
    ///
    /// The framework flattens the tree once during build, so children are moved
    /// out into a flat arena rather than walked recursively on every pass. An
    /// element that returns children from [`Element::children`] must also return
    /// them here, or they will be laid out and never painted.
    fn take_children(&mut self) -> Vec<AnyElement> {
        Vec::new()
    }

    /// Intrinsic size for leaf content such as text.
    ///
    /// Returning `None` means the element has no intrinsic size and its box is
    /// determined entirely by its style and children. The layout engine calls
    /// this for every childless node, so a plain box must return `None` rather
    /// than doing work.
    ///
    /// The theme is passed because a widget that paints its own text — a button
    /// draws its label as a leaf rather than laying it out as a child — has to
    /// measure with exactly the tokens it will paint with. Measuring at one size
    /// and painting at another puts the text outside the box that was reserved
    /// for it.
    fn measure(
        &mut self,
        _request: &MeasureRequest<'_>,
        _text: &mut spherekit_text::TextSystem,
        _theme: &crate::theme::Theme,
    ) -> Option<Size<Px>> {
        None
    }

    /// Records this element's own painting. Children are painted by the
    /// framework, after this returns.
    fn paint(&mut self, cx: &mut PaintContext<'_, '_>);

    /// Returns the post-process applied to this element and its subtree.
    ///
    /// The default is no effect so custom elements remain direct-to-parent.
    /// Built-in styled elements expose their [`PaintStyle`] filter here, which
    /// lets the tree open the required offscreen layer before painting them.
    fn paint_filter(&self) -> Option<Filter> {
        None
    }

    /// Returns the opacity applied to this element and its subtree.
    ///
    /// This is separate from [`Element::paint`] because group opacity must be
    /// resolved before children are painted.
    fn paint_opacity(&self) -> f32 {
        1.0
    }

    /// Responds to an event. The default ignores everything.
    fn handle_event(&mut self, _cx: &mut EventContext<'_>) -> EventFlow {
        EventFlow::Continue
    }

    /// True when this element can take keyboard focus.
    fn focusable(&self) -> bool {
        false
    }

    /// Semantic information for assistive technology.
    ///
    /// Returning `None` means the element is presentational. SphereKit does not
    /// ship a full accessibility bridge yet, but every element can already
    /// carry the information one would need, which is what keeps adding it
    /// later a wiring job rather than a redesign.
    fn semantics(&self) -> Option<crate::semantics::Semantics> {
        None
    }
}

/// A boxed element.
pub type AnyElement = Box<dyn Element>;

/// Conversion into an element, so `child()` accepts elements, strings and
/// options without ceremony.
pub trait IntoElement {
    /// Performs the conversion.
    fn into_element(self) -> AnyElement;
}

impl<E: Element> IntoElement for E {
    fn into_element(self) -> AnyElement {
        Box::new(self)
    }
}

impl IntoElement for AnyElement {
    fn into_element(self) -> AnyElement {
        self
    }
}

/// An element that renders nothing and occupies no space.
///
/// What `Option::None` and an empty branch turn into, so conditional UI does
/// not need a different shape from unconditional UI.
pub struct Empty;

impl Element for Empty {
    fn layout_style(&self) -> Style {
        Style { display: spherekit_layout::Display::None, ..Style::DEFAULT }
    }
    fn paint(&mut self, _cx: &mut PaintContext<'_, '_>) {}
}

impl<T: IntoElement> IntoElement for Option<T> {
    fn into_element(self) -> AnyElement {
        match self {
            Some(v) => v.into_element(),
            None => Box::new(Empty),
        }
    }
}

/// An element that can hold children.
pub trait ParentElement: Sized {
    /// Appends children.
    fn extend_children(&mut self, children: impl IntoIterator<Item = AnyElement>);

    /// Appends one child.
    fn child(mut self, child: impl IntoElement) -> Self {
        self.extend_children(core::iter::once(child.into_element()));
        self
    }

    /// Appends many children.
    fn children_iter<I, T>(mut self, children: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: IntoElement,
    {
        self.extend_children(children.into_iter().map(IntoElement::into_element));
        self
    }

    /// Appends a child only when `condition` holds.
    ///
    /// Keeps conditional structure inside the builder chain instead of forcing
    /// a `let mut` and an `if` around it.
    fn child_if(self, condition: bool, child: impl FnOnce() -> AnyElement) -> Self {
        if condition { self.child(child()) } else { self }
    }
}

/// Fluent styling shared by every built-in element.
pub trait Styled: Sized {
    /// The layout style being built.
    fn style_mut(&mut self) -> &mut Style;
    /// The paint style being built.
    fn paint_style_mut(&mut self) -> &mut PaintStyle;

    // ---------------------------------------------------------- layout

    /// Lays children out with flexbox.
    fn flex(mut self) -> Self {
        self.style_mut().display = spherekit_layout::Display::Flex;
        self
    }
    /// Flexbox, horizontal.
    fn flex_row(mut self) -> Self {
        let s = self.style_mut();
        s.display = spherekit_layout::Display::Flex;
        s.flex_direction = spherekit_layout::FlexDirection::Row;
        self
    }
    /// Flexbox, vertical.
    fn flex_col(mut self) -> Self {
        let s = self.style_mut();
        s.display = spherekit_layout::Display::Flex;
        s.flex_direction = spherekit_layout::FlexDirection::Column;
        self
    }
    /// Hides the element and removes it from layout entirely.
    fn hidden(mut self) -> Self {
        self.style_mut().display = spherekit_layout::Display::None;
        self
    }
    /// Grow factor.
    fn grow(mut self, factor: f32) -> Self {
        self.style_mut().flex_grow = factor;
        self
    }
    /// Shrink factor.
    fn shrink(mut self, factor: f32) -> Self {
        self.style_mut().flex_shrink = factor;
        self
    }
    /// Takes all remaining space along the main axis.
    fn flex_1(self) -> Self {
        self.grow(1.0).shrink(1.0)
    }
    /// Gap between children on both axes.
    fn gap(mut self, v: Px) -> Self {
        self.style_mut().gap = Size::new(v, v);
        self
    }
    /// Width.
    fn w(mut self, v: impl Into<Length>) -> Self {
        self.style_mut().size.width = v.into();
        self
    }
    /// Height.
    fn h(mut self, v: impl Into<Length>) -> Self {
        self.style_mut().size.height = v.into();
        self
    }
    /// Both dimensions.
    fn size(mut self, v: impl Into<Length> + Copy) -> Self {
        let s = self.style_mut();
        s.size.width = v.into();
        s.size.height = v.into();
        self
    }
    /// Fills the parent on both axes.
    fn full(self) -> Self {
        self.w(spherekit_core::relative(1.0)).h(spherekit_core::relative(1.0))
    }
    /// Minimum width.
    fn min_w(mut self, v: impl Into<Length>) -> Self {
        self.style_mut().min_size.width = v.into();
        self
    }
    /// Minimum height.
    fn min_h(mut self, v: impl Into<Length>) -> Self {
        self.style_mut().min_size.height = v.into();
        self
    }
    /// Maximum width.
    fn max_w(mut self, v: impl Into<Length>) -> Self {
        self.style_mut().max_size.width = v.into();
        self
    }
    /// Maximum height.
    fn max_h(mut self, v: impl Into<Length>) -> Self {
        self.style_mut().max_size.height = v.into();
        self
    }
    /// Padding on all sides.
    fn p(mut self, v: Px) -> Self {
        self.style_mut().padding = spherekit_layout::edges_all(Length::Px(v));
        self
    }
    /// Horizontal padding.
    fn px_(mut self, v: Px) -> Self {
        let p = &mut self.style_mut().padding;
        p.left = Length::Px(v);
        p.right = Length::Px(v);
        self
    }
    /// Vertical padding.
    fn py_(mut self, v: Px) -> Self {
        let p = &mut self.style_mut().padding;
        p.top = Length::Px(v);
        p.bottom = Length::Px(v);
        self
    }
    /// Padding on the left only.
    fn pl(mut self, v: Px) -> Self {
        self.style_mut().padding.left = Length::Px(v);
        self
    }
    /// Padding on the right only.
    fn pr(mut self, v: Px) -> Self {
        self.style_mut().padding.right = Length::Px(v);
        self
    }
    /// Padding on the top only.
    fn pt(mut self, v: Px) -> Self {
        self.style_mut().padding.top = Length::Px(v);
        self
    }
    /// Padding on the bottom only.
    fn pb(mut self, v: Px) -> Self {
        self.style_mut().padding.bottom = Length::Px(v);
        self
    }
    /// Margin on all sides.
    fn m(mut self, v: Px) -> Self {
        self.style_mut().margin = spherekit_layout::edges_all(Length::Px(v));
        self
    }
    /// Cross-axis alignment of children.
    fn items(mut self, a: spherekit_layout::Align) -> Self {
        self.style_mut().align_items = Some(a);
        self
    }
    /// Centres children on the cross axis.
    fn items_center(self) -> Self {
        self.items(spherekit_layout::Align::Center)
    }
    /// Main-axis distribution of children.
    fn justify(mut self, d: spherekit_layout::Distribute) -> Self {
        self.style_mut().justify_content = Some(d);
        self
    }
    /// Centres children on the main axis.
    fn justify_center(self) -> Self {
        self.justify(spherekit_layout::Distribute::Center)
    }
    /// Centres children on both axes.
    fn center(self) -> Self {
        self.items_center().justify_center()
    }
    /// Positions the element absolutely within its parent.
    fn absolute(mut self) -> Self {
        self.style_mut().position = spherekit_layout::Position::Absolute;
        self
    }
    /// Absolute inset from each edge.
    fn inset(mut self, v: Px) -> Self {
        self.style_mut().inset = spherekit_layout::edges_all(Length::Px(v));
        self
    }
    /// Order among siblings. Higher paints later.
    fn z(mut self, index: i32) -> Self {
        self.style_mut().z_index = index;
        self
    }
    /// Scroll behaviour on both axes.
    fn overflow(mut self, o: spherekit_layout::Overflow) -> Self {
        let s = self.style_mut();
        s.overflow_x = o;
        s.overflow_y = o;
        self
    }
    /// Makes the element a vertical scroll container.
    fn overflow_y_scroll(mut self) -> Self {
        self.style_mut().overflow_y = spherekit_layout::Overflow::Scroll;
        self
    }
    /// Clips overflowing children without offering a scrollbar.
    fn overflow_hidden(self) -> Self {
        self.overflow(spherekit_layout::Overflow::Hidden)
    }

    // ----------------------------------------------------------- paint

    /// Background fill.
    fn bg(mut self, brush: impl Into<spherekit_core::Brush>) -> Self {
        self.paint_style_mut().background = Some(brush.into());
        self
    }
    /// Uniform corner radius.
    fn rounded(mut self, r: Px) -> Self {
        self.paint_style_mut().corner_radii = Corners::all(r);
        self
    }
    /// Per-corner radii.
    fn rounded_corners(mut self, radii: Corners<Px>) -> Self {
        self.paint_style_mut().corner_radii = radii;
        self
    }
    /// Fully rounded ends, for pills and knobs.
    fn rounded_full(mut self) -> Self {
        // Clamped against the box at paint time, so an oversized radius
        // becomes exactly half the shorter side.
        self.paint_style_mut().corner_radii = Corners::all(Px(1.0e6));
        self
    }
    /// Border.
    fn border(mut self, width: Px, color: Color) -> Self {
        let p = self.paint_style_mut();
        p.border_width = width;
        p.border_color = color;
        self
    }
    /// Drop shadow.
    fn shadow(mut self, shadow: spherekit_core::Shadow) -> Self {
        self.paint_style_mut().shadows.push(shadow);
        self
    }
    /// Opacity multiplier for this element and its children.
    fn opacity(mut self, v: f32) -> Self {
        self.paint_style_mut().opacity = v.clamp(0.0, 1.0);
        self
    }
    /// Blurs this element's rendered content before compositing it.
    fn blur(mut self, sigma: Px) -> Self {
        self.paint_style_mut().filter =
            Some(spherekit_render::Filter::Blur { sigma: sigma.max(Px::ZERO) });
        self
    }
    /// Blurs the content behind this element before compositing its children.
    fn backdrop_blur(mut self, sigma: Px) -> Self {
        self.paint_style_mut().filter =
            Some(spherekit_render::Filter::BackdropBlur { sigma: sigma.max(Px::ZERO) });
        self
    }
    /// Applies a theme-coloured Mica surface with a blurred backdrop.
    fn mica(mut self, tint: Color, sigma: Px) -> Self {
        let style = self.paint_style_mut();
        style.background = Some(tint.into());
        style.filter = Some(spherekit_render::Filter::BackdropBlur { sigma: sigma.max(Px::ZERO) });
        self
    }
    /// Clips children to this element's box.
    fn clip(mut self) -> Self {
        self.paint_style_mut().clip_content = true;
        self
    }
    /// Cursor while the pointer is over this element.
    fn cursor(mut self, c: crate::style::Cursor) -> Self {
        self.paint_style_mut().cursor = Some(c);
        self
    }
}

/// Event handlers attached to an element.
///
/// Boxed closures in a `SmallVec`: most elements register none, and the ones
/// that do register one or two.
#[derive(Default)]
pub struct Handlers {
    handlers: SmallVec<[HandlerEntry; 2]>,
}

/// One registered handler: what it responds to, when, and what it does.
type HandlerEntry = (HandlerKind, Phase, Box<dyn FnMut(&mut EventContext<'_>) -> EventFlow>);

/// Which event a handler responds to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HandlerKind {
    /// A completed primary click.
    Click,
    /// A button went down.
    MouseDown,
    /// A button came up.
    MouseUp,
    /// The pointer moved.
    MouseMove,
    /// The pointer entered.
    MouseEnter,
    /// The pointer left.
    MouseLeave,
    /// A scroll gesture.
    Scroll,
    /// A key changed state.
    Key,
    /// Text was committed.
    TextInput,
    /// Focus was gained or lost.
    Focus,
    /// Every event.
    Any,
}

impl HandlerKind {
    /// True when this handler should run for `event`.
    pub fn matches(self, event: &UiEvent) -> bool {
        match self {
            HandlerKind::Any => true,
            HandlerKind::Click | HandlerKind::MouseUp => matches!(event, UiEvent::MouseUp(_)),
            HandlerKind::MouseDown => matches!(event, UiEvent::MouseDown(_)),
            HandlerKind::MouseMove => matches!(event, UiEvent::MouseMove(_)),
            HandlerKind::MouseEnter => matches!(event, UiEvent::MouseEnter(_)),
            HandlerKind::MouseLeave => matches!(event, UiEvent::MouseLeave(_)),
            HandlerKind::Scroll => matches!(event, UiEvent::Scroll(_)),
            HandlerKind::Key => matches!(event, UiEvent::Key(_)),
            HandlerKind::TextInput => {
                matches!(event, UiEvent::TextInput(_) | UiEvent::Ime(_))
            }
            HandlerKind::Focus => matches!(event, UiEvent::FocusIn | UiEvent::FocusOut),
        }
    }
}

impl Handlers {
    /// Registers a handler.
    pub fn push(
        &mut self,
        kind: HandlerKind,
        phase: Phase,
        f: impl FnMut(&mut EventContext<'_>) -> EventFlow + 'static,
    ) {
        self.handlers.push((kind, phase, Box::new(f)));
    }

    /// True when nothing is registered, which lets the dispatcher skip the
    /// element entirely.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Runs every matching handler for the current phase.
    pub fn dispatch(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        let mut flow = EventFlow::Continue;
        for (kind, phase, handler) in self.handlers.iter_mut() {
            if *phase != cx.phase || !kind.matches(cx.event) {
                continue;
            }
            // A click handler must not fire on a release that happened outside
            // the element, which is how a user cancels a press.
            if *kind == HandlerKind::Click
                && let Some(p) = cx.event.position()
                && !cx.bounds.contains(p)
            {
                continue;
            }
            flow = flow.merge(handler(cx));
            if flow.is_stopped() {
                break;
            }
        }
        flow
    }
}

impl core::fmt::Debug for Handlers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Handlers").field("count", &self.handlers.len()).finish()
    }
}

/// Interaction handlers, in builder form.
pub trait Interactive: Sized {
    /// The handler set being built.
    fn handlers_mut(&mut self) -> &mut Handlers;

    /// Runs when a primary click completes inside this element.
    fn on_click(mut self, mut f: impl FnMut(&mut EventContext<'_>) + 'static) -> Self {
        self.handlers_mut().push(HandlerKind::Click, Phase::Bubble, move |cx| {
            f(cx);
            EventFlow::Stop
        });
        self
    }
    /// Runs when a button goes down on this element.
    fn on_mouse_down(mut self, mut f: impl FnMut(&mut EventContext<'_>) + 'static) -> Self {
        self.handlers_mut().push(HandlerKind::MouseDown, Phase::Bubble, move |cx| {
            f(cx);
            EventFlow::Continue
        });
        self
    }
    /// Runs when a button comes up anywhere while this element is involved.
    fn on_mouse_up(mut self, mut f: impl FnMut(&mut EventContext<'_>) + 'static) -> Self {
        self.handlers_mut().push(HandlerKind::MouseUp, Phase::Bubble, move |cx| {
            f(cx);
            EventFlow::Continue
        });
        self
    }
    /// Runs on pointer movement over this element.
    fn on_mouse_move(mut self, mut f: impl FnMut(&mut EventContext<'_>) + 'static) -> Self {
        self.handlers_mut().push(HandlerKind::MouseMove, Phase::Bubble, move |cx| {
            f(cx);
            EventFlow::Continue
        });
        self
    }
    /// Runs when the pointer enters.
    fn on_mouse_enter(mut self, mut f: impl FnMut(&mut EventContext<'_>) + 'static) -> Self {
        self.handlers_mut().push(HandlerKind::MouseEnter, Phase::Bubble, move |cx| {
            f(cx);
            EventFlow::Continue
        });
        self
    }
    /// Runs when the pointer leaves.
    fn on_mouse_leave(mut self, mut f: impl FnMut(&mut EventContext<'_>) + 'static) -> Self {
        self.handlers_mut().push(HandlerKind::MouseLeave, Phase::Bubble, move |cx| {
            f(cx);
            EventFlow::Continue
        });
        self
    }
    /// Runs on a scroll gesture.
    fn on_scroll(mut self, mut f: impl FnMut(&mut EventContext<'_>) + 'static) -> Self {
        self.handlers_mut().push(HandlerKind::Scroll, Phase::Bubble, move |cx| {
            f(cx);
            EventFlow::Continue
        });
        self
    }
    /// Runs on a key event while this element has focus.
    fn on_key(mut self, mut f: impl FnMut(&mut EventContext<'_>) -> EventFlow + 'static) -> Self {
        self.handlers_mut().push(HandlerKind::Key, Phase::Bubble, move |cx| f(cx));
        self
    }
    /// Runs during the capture phase, before descendants see the event.
    ///
    /// For modal overlays and pointer capture. Everything else wants a bubble
    /// handler.
    fn on_capture(
        mut self,
        kind: HandlerKind,
        mut f: impl FnMut(&mut EventContext<'_>) -> EventFlow + 'static,
    ) -> Self {
        self.handlers_mut().push(kind, Phase::Capture, move |cx| f(cx));
        self
    }
}

/// The general-purpose container element.
///
/// Layout, background, border, corner radii, shadows, clipping and event
/// handling in one type, because that combination is what the overwhelming
/// majority of UI actually is. Specialised widgets compose from it.
#[derive(Default)]
pub struct Div {
    id: Option<ElementId>,
    style: Style,
    paint: PaintStyle,
    children: Vec<AnyElement>,
    handlers: Handlers,
    focusable: bool,
    semantics: Option<crate::semantics::Semantics>,
}

/// Creates a [`Div`].
pub fn div() -> Div {
    Div { style: Style::DEFAULT, ..Default::default() }
}

impl Div {
    /// Gives the element a stable identity, so its node, state, focus and
    /// animations survive a rebuild.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Makes the element focusable by keyboard.
    pub fn focusable(mut self) -> Self {
        self.focusable = true;
        self
    }

    /// Attaches semantic information for assistive technology.
    pub fn semantics(mut self, s: crate::semantics::Semantics) -> Self {
        self.semantics = Some(s);
        self
    }
}

impl Styled for Div {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }
    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl ParentElement for Div {
    fn extend_children(&mut self, children: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(children);
    }
}

impl Interactive for Div {
    fn handlers_mut(&mut self) -> &mut Handlers {
        &mut self.handlers
    }
}

impl Element for Div {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        self.style.clone()
    }

    fn children(&mut self) -> &mut [AnyElement] {
        &mut self.children
    }

    fn take_children(&mut self) -> Vec<AnyElement> {
        core::mem::take(&mut self.children)
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        self.paint.paint_box(cx.canvas, cx.bounds, cx.state);
    }

    fn paint_filter(&self) -> Option<Filter> {
        self.paint.filter
    }

    fn paint_opacity(&self) -> f32 {
        self.paint.opacity
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.handlers.is_empty() {
            return EventFlow::Continue;
        }
        if let Some(c) = self.paint.cursor {
            cx.set_cursor(c);
        }
        self.handlers.dispatch(cx)
    }

    fn focusable(&self) -> bool {
        self.focusable
    }

    fn semantics(&self) -> Option<crate::semantics::Semantics> {
        self.semantics.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ElementState, Modifiers, MouseButton, MouseButtonEvent};
    use spherekit_core::{Point, px, rect};
    use std::cell::Cell;
    use std::rc::Rc;

    fn ctx<'a>(
        event: &'a UiEvent,
        bounds: Rect<Px>,
        phase: Phase,
        scratch: &'a mut [f32; 4],
    ) -> EventContext<'a> {
        EventContext {
            event,
            phase,
            bounds,
            chain: &[],
            scratch,
            repaint: false,
            relayout: false,
            request_focus: false,
            capture_pointer: false,
            release_pointer: false,
            cursor: None,
            text: None,
            clipboard: Box::leak(Box::new(spherekit_platform::Clipboard::unsupported())),
            theme: TEST_THEME.get_or_init(crate::theme::Theme::dark),
        }
    }

    static TEST_THEME: std::sync::OnceLock<crate::theme::Theme> = std::sync::OnceLock::new();

    fn mouse_up_at(x: f32, y: f32) -> UiEvent {
        UiEvent::MouseUp(MouseButtonEvent {
            position: Point::new(px(x), px(y)),
            button: MouseButton::Primary,
            state: ElementState::Released,
            click_count: 1,
            modifiers: Modifiers::NONE,
        })
    }

    #[test]
    fn builder_chain_sets_layout_and_paint_style() {
        let d = div().flex_col().gap(px(8.0)).p(px(12.0)).w(px(200.0)).rounded(px(6.0));
        let s = d.layout_style();
        assert_eq!(s.display, spherekit_layout::Display::Flex);
        assert_eq!(s.flex_direction, spherekit_layout::FlexDirection::Column);
        assert_eq!(s.gap.width, px(8.0));
        assert_eq!(s.size.width, Length::Px(px(200.0)));
        assert_eq!(d.paint.corner_radii.top_left, px(6.0));
    }

    #[test]
    fn children_are_kept_in_order() {
        let mut d = div().child(div().w(px(1.0))).child(div().w(px(2.0)));
        let kids = d.children();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].layout_style().size.width, Length::Px(px(1.0)));
        assert_eq!(kids[1].layout_style().size.width, Length::Px(px(2.0)));
    }

    #[test]
    fn child_if_skips_when_false() {
        let mut yes = div().child_if(true, || div().into_element());
        let mut no = div().child_if(false, || div().into_element());
        assert_eq!(yes.children().len(), 1);
        assert_eq!(no.children().len(), 0);
    }

    #[test]
    fn none_becomes_an_empty_element_that_takes_no_space() {
        let e: Option<Div> = None;
        let el = e.into_element();
        assert_eq!(el.layout_style().display, spherekit_layout::Display::None);
    }

    #[test]
    fn an_id_is_stable_across_rebuilds() {
        assert_eq!(div().id("track-3").id, div().id("track-3").id);
        assert_ne!(div().id("track-3").id, div().id("track-4").id);
    }

    #[test]
    fn click_fires_when_the_release_is_inside() {
        let fired = Rc::new(Cell::new(0));
        let f = fired.clone();
        let mut d = div().on_click(move |_| f.set(f.get() + 1));
        let ev = mouse_up_at(10.0, 10.0);
        let mut scratch = [0.0f32; 4];
        let mut cx =
            ctx(&ev, rect(px(0.0), px(0.0), px(100.0), px(100.0)), Phase::Bubble, &mut scratch);
        d.handle_event(&mut cx);
        assert_eq!(fired.get(), 1);
    }

    #[test]
    fn click_does_not_fire_when_the_release_is_outside() {
        // Pressing a button and dragging off before releasing is how a user
        // cancels; firing there would be a real behavioural bug.
        let fired = Rc::new(Cell::new(0));
        let f = fired.clone();
        let mut d = div().on_click(move |_| f.set(f.get() + 1));
        let ev = mouse_up_at(500.0, 500.0);
        let mut scratch = [0.0f32; 4];
        let mut cx =
            ctx(&ev, rect(px(0.0), px(0.0), px(100.0), px(100.0)), Phase::Bubble, &mut scratch);
        d.handle_event(&mut cx);
        assert_eq!(fired.get(), 0);
    }

    #[test]
    fn a_click_handler_stops_propagation() {
        let mut d = div().on_click(|_| {});
        let ev = mouse_up_at(10.0, 10.0);
        let mut scratch = [0.0f32; 4];
        let mut cx =
            ctx(&ev, rect(px(0.0), px(0.0), px(100.0), px(100.0)), Phase::Bubble, &mut scratch);
        assert_eq!(d.handle_event(&mut cx), EventFlow::Stop);
    }

    #[test]
    fn bubble_handlers_do_not_run_during_capture() {
        let fired = Rc::new(Cell::new(0));
        let f = fired.clone();
        let mut d = div().on_click(move |_| f.set(f.get() + 1));
        let ev = mouse_up_at(10.0, 10.0);
        let mut scratch = [0.0f32; 4];
        let mut cx =
            ctx(&ev, rect(px(0.0), px(0.0), px(100.0), px(100.0)), Phase::Capture, &mut scratch);
        d.handle_event(&mut cx);
        assert_eq!(fired.get(), 0);
    }

    #[test]
    fn an_element_with_no_handlers_short_circuits() {
        let mut d = div();
        let ev = mouse_up_at(10.0, 10.0);
        let mut scratch = [0.0f32; 4];
        let mut cx =
            ctx(&ev, rect(px(0.0), px(0.0), px(100.0), px(100.0)), Phase::Bubble, &mut scratch);
        assert_eq!(d.handle_event(&mut cx), EventFlow::Continue);
        assert!(cx.cursor.is_none());
    }

    #[test]
    fn handler_kinds_match_only_their_own_events() {
        let up = mouse_up_at(0.0, 0.0);
        assert!(HandlerKind::MouseUp.matches(&up));
        assert!(HandlerKind::Any.matches(&up));
        assert!(!HandlerKind::Scroll.matches(&up));
        assert!(!HandlerKind::Key.matches(&up));
    }

    #[test]
    fn local_position_is_relative_to_the_element() {
        let ev = mouse_up_at(120.0, 60.0);
        let mut scratch = [0.0f32; 4];
        let cx =
            ctx(&ev, rect(px(100.0), px(50.0), px(80.0), px(20.0)), Phase::Bubble, &mut scratch);
        assert_eq!(cx.local_position(), Some(Point::new(px(20.0), px(10.0))));
    }

    #[test]
    fn notify_requests_paint_without_layout() {
        // The headline invariant: a repaint must not imply a relayout.
        let ev = mouse_up_at(0.0, 0.0);
        let mut scratch = [0.0f32; 4];
        let mut cx = ctx(&ev, Rect::ZERO, Phase::Bubble, &mut scratch);
        cx.notify();
        assert!(cx.repaint);
        assert!(!cx.relayout, "a repaint must never escalate to a relayout");
    }

    #[test]
    fn notify_layout_implies_paint() {
        let ev = mouse_up_at(0.0, 0.0);
        let mut scratch = [0.0f32; 4];
        let mut cx = ctx(&ev, Rect::ZERO, Phase::Bubble, &mut scratch);
        cx.notify_layout();
        assert!(cx.relayout && cx.repaint);
    }

    #[test]
    fn culling_uses_the_visible_region_not_the_bounds() {
        let mut scene = spherekit_render::Scene::new(
            Size::new(px(100.0), px(100.0)),
            spherekit_core::ScaleFactor::IDENTITY,
        );
        let mut canvas = Canvas::new(&mut scene);
        let mut text = spherekit_text::TextSystem::new();
        let theme = crate::theme::Theme::dark();
        let cx = PaintContext {
            canvas: &mut canvas,
            text: &mut text,
            bounds: rect(px(500.0), px(500.0), px(10.0), px(10.0)),
            visible: rect(px(0.0), px(0.0), px(100.0), px(100.0)),
            scratch: [0.0; 4],
            state: InteractionState::default(),
            theme: &theme,
            time: 0.0,
            ime: &mut None,
            caption_exclusions: &mut Vec::new(),
        };
        assert!(cx.is_culled());
    }
}

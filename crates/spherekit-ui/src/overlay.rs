//! Things that float above the page: a scrim, a popover, a toast.
//!
//! ## What they have in common
//!
//! None of them owns a timer, an open flag or a queue. Each takes a number in
//! `0..=1` saying how far it has arrived and draws that frame, exactly as
//! [`Dropdown`](crate::widgets::Dropdown) does — which is what lets one
//! [`Motion`](spherekit_core::animate::Motion) in the application drive it, and
//! what lets a test pass `0.5` and assert on a half-open panel without running
//! a clock.
//!
//! It also settles the question a toast library usually gets wrong. *When* a
//! toast appears, how long it stays and how many are on screen at once are
//! product decisions with no defensible default; a widget that answered them
//! would be one an application has to fight. So the widget draws one toast, the
//! application owns the list, and the two never disagree about which is which.
//!
//! ## Where they sit
//!
//! Anchoring is by containment, again like `Dropdown`: every node is a
//! containing block, so an [`Overlay`] fills its parent and a [`Popover`] hangs
//! off its parent's edge. Put an overlay at the root of the tree and it covers
//! the window; put one inside a card and it covers the card.
//!
//! ```ignore
//! div()
//!     .flex_col()
//!     .child(button("Delete").id("del"))
//!     .child(
//!         popover(self.confirm.get().value())
//!             .below()
//!             .arrow(true)
//!             .child(label("This cannot be undone")),
//!     )
//! ```

use crate::element::{
    AnyElement, Element, EventContext, IntoElement, PaintContext, ParentElement, Styled, div,
};
use crate::event::{EventFlow, Key, MouseButton, UiEvent};
use crate::semantics::{Role, Semantics};
use crate::style::{Cursor, PaintStyle};
use crate::text::label;
use crate::theme::{TextRole, TypeScale};
use crate::widgets::{OnAction, ease_out_cubic};
use spherekit_core::{Color, Corners, ElementId, Length, Point, Px, RoundedRect, Size, px};
use spherekit_layout::Style;

/// Below this an overlay is treated as gone and leaves layout entirely.
const CLOSED: f32 = 0.002;

/// The stacking order a scrim takes unless the caller overrides it.
///
/// Below [`CONTEXT_MENU_Z`](crate::widgets::CONTEXT_MENU_Z), because a menu
/// opened *from* a dialog has to sit over the dialog's own scrim.
pub const OVERLAY_Z: i32 = 9_000;
/// The stacking order a toast layer takes: above a scrim, below a menu.
pub const TOAST_Z: i32 = 9_500;

// ---------------------------------------------------------------------------
// Overlay
// ---------------------------------------------------------------------------

/// A scrim over everything behind it, with its content centred on top.
///
/// Two jobs, and the second is the one that is easy to forget: it dims what is
/// behind, and it **swallows every event that reaches it**. A dialog drawn over
/// a page whose buttons still respond is not modal, it is a picture of a modal,
/// and the bug only shows up when somebody tabs or clicks through it.
///
/// Fills its parent, so an overlay at the root of the tree covers the window and
/// one inside a card covers the card. That is the same containment rule the rest
/// of the overlay family follows, and it is what makes a card-local "are you
/// sure?" possible without a second positioning system.
pub struct Overlay {
    id: Option<ElementId>,
    visible: f32,
    tint: Option<Color>,
    dismissible: bool,
    children: Vec<AnyElement>,
    style: Style,
    paint: PaintStyle,
    on_dismiss: Option<OnAction>,
}

/// Creates an [`Overlay`] at the given visibility, `0..=1`.
pub fn overlay(visible: f32) -> Overlay {
    Overlay {
        id: None,
        visible,
        tint: None,
        dismissible: true,
        children: Vec::new(),
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
        on_dismiss: None,
    }
}

impl Overlay {
    /// Gives the scrim a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Overrides the dimming colour. Defaults to black at 45 %.
    pub fn tint(mut self, tint: Color) -> Self {
        self.tint = Some(tint);
        self
    }

    /// Whether a click on the scrim itself asks to close.
    ///
    /// On by default, because a modal a user cannot escape by clicking away
    /// from it is the single most complained-about dialog behaviour. Turn it
    /// off for the rare case that genuinely must be answered — an unsaved-work
    /// prompt — and give that one an explicit Cancel.
    pub fn dismissible(mut self, dismissible: bool) -> Self {
        self.dismissible = dismissible;
        self
    }

    /// Runs when the scrim is clicked or Escape is pressed on it.
    pub fn on_dismiss(mut self, f: impl FnMut() + 'static) -> Self {
        self.on_dismiss = Some(Box::new(f));
        self
    }

    fn eased(&self) -> f32 {
        ease_out_cubic(self.visible.clamp(0.0, 1.0))
    }
}

impl ParentElement for Overlay {
    fn extend_children(&mut self, children: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(children);
    }
}

impl Styled for Overlay {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for Overlay {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if self.visible.clamp(0.0, 1.0) <= CLOSED {
            // Out of layout, not merely transparent. An invisible scrim that
            // still hit-tested would swallow every click in the window, and
            // it would do it silently.
            style.display = spherekit_layout::Display::None;
            return style;
        }
        style.position = spherekit_layout::Position::Absolute;
        style.inset = spherekit_core::Edges {
            top: Length::Px(Px::ZERO),
            right: Length::Px(Px::ZERO),
            bottom: Length::Px(Px::ZERO),
            left: Length::Px(Px::ZERO),
        };
        style.display = spherekit_layout::Display::Flex;
        if style.align_items.is_none() {
            style.align_items = Some(spherekit_layout::Align::Center);
        }
        if style.justify_content.is_none() {
            style.justify_content = Some(spherekit_layout::Distribute::Center);
        }
        if style.z_index == 0 {
            style.z_index = OVERLAY_Z;
        }
        style
    }

    fn children(&mut self) -> &mut [AnyElement] {
        &mut self.children
    }

    fn take_children(&mut self) -> Vec<AnyElement> {
        core::mem::take(&mut self.children)
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
        let tint = self.tint.unwrap_or(Color::BLACK.with_alpha(0.45));
        cx.canvas.fill_rect(cx.bounds, tint);
    }

    fn paint_opacity(&self) -> f32 {
        (self.eased() * self.paint.opacity).clamp(0.0, 1.0)
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        // Everything stops here. A press that reached the page behind a scrim
        // would be a modal that is not modal; a wheel that reached it would
        // scroll the page out from under the dialog.
        match cx.event {
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                if self.dismissible
                    && let Some(f) = self.on_dismiss.as_mut()
                {
                    f();
                    cx.notify();
                }
                EventFlow::Stop
            }
            UiEvent::Key(k) if k.state.is_pressed() && k.key == Key::Escape => {
                if self.dismissible
                    && let Some(f) = self.on_dismiss.as_mut()
                {
                    f();
                    cx.notify();
                    return EventFlow::Stop;
                }
                EventFlow::Continue
            }
            UiEvent::MouseUp(_) | UiEvent::Scroll(_) => EventFlow::Stop,
            _ => EventFlow::Continue,
        }
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::new(Role::Dialog, String::new()))
    }
}

// ---------------------------------------------------------------------------
// Popover
// ---------------------------------------------------------------------------

/// Which edge of its anchor a [`Popover`] opens from.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum PopoverSide {
    /// Above the anchor's top edge.
    Top,
    /// Below the anchor's bottom edge. The default.
    #[default]
    Bottom,
    /// To the left of the anchor.
    Left,
    /// To the right of the anchor.
    Right,
}

/// How a [`Popover`] lines up along its anchor's other axis.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum PopoverAlign {
    /// Flush with the anchor's leading edge — left for a panel above or below,
    /// top for one beside.
    Start,
    /// Flush with the anchor's trailing edge.
    End,
    /// Spans the anchor, which is what a select menu wants. The default.
    #[default]
    Stretch,
}

/// An anchored panel with an optional beak.
///
/// The difference from a [`Dropdown`](crate::widgets::Dropdown) is the axis: a
/// dropdown belongs to the control it drops from and opens up or down along it,
/// while a popover can also sit beside its anchor and can point at it.
///
/// There is no `Center` alignment, and the omission is deliberate. Centring an
/// absolutely positioned box of unknown width over its anchor needs a
/// translation by half that width, which is a transform the layout engine does
/// not have — and the alternative, spanning a wide transparent area with the
/// card centred inside it, would make that transparent area win every hit test
/// around the card. An application that knows both widths can centre it with a
/// margin; the widget will not pretend to.
pub struct Popover {
    id: Option<ElementId>,
    open: f32,
    side: PopoverSide,
    align: PopoverAlign,
    gap: Px,
    rise: Px,
    arrow: bool,
    children: Vec<AnyElement>,
    style: Style,
    paint: PaintStyle,
}

/// Creates a [`Popover`] at the given open amount, `0..=1`.
pub fn popover(open: f32) -> Popover {
    let mut style = Style::DEFAULT;
    style.flex_direction = spherekit_layout::FlexDirection::Column;
    Popover {
        id: None,
        open,
        side: PopoverSide::default(),
        align: PopoverAlign::default(),
        gap: px(8.0),
        rise: px(6.0),
        arrow: false,
        children: Vec::new(),
        style,
        paint: PaintStyle::default(),
    }
}

/// Half the width of the beak, and how far it stands out from the panel.
const ARROW: f32 = 7.0;

impl Popover {
    /// Gives the panel a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Sets which edge of the anchor the panel opens from.
    pub fn side(mut self, side: PopoverSide) -> Self {
        self.side = side;
        self
    }

    /// Opens above the anchor.
    pub fn above(self) -> Self {
        self.side(PopoverSide::Top)
    }

    /// Opens below the anchor.
    pub fn below(self) -> Self {
        self.side(PopoverSide::Bottom)
    }

    /// Opens to the left of the anchor.
    pub fn before(self) -> Self {
        self.side(PopoverSide::Left)
    }

    /// Opens to the right of the anchor.
    pub fn after(self) -> Self {
        self.side(PopoverSide::Right)
    }

    /// Sets how the panel lines up along the anchor's other axis.
    pub fn align(mut self, align: PopoverAlign) -> Self {
        self.align = align;
        self
    }

    /// The resting distance between panel and anchor.
    ///
    /// Named `offset` rather than `gap` because [`Styled::gap`] already means
    /// the space between this panel's own children.
    pub fn offset(mut self, offset: Px) -> Self {
        self.gap = offset;
        self
    }

    /// How far the panel travels as it opens.
    pub fn rise(mut self, rise: Px) -> Self {
        self.rise = rise;
        self
    }

    /// Draws a beak pointing back at the anchor.
    ///
    /// Off by default. A beak says "this belongs to *that*", which is worth the
    /// pixels for a hint attached to one control and is noise on a menu that
    /// already sits flush against its trigger.
    pub fn arrow(mut self, arrow: bool) -> Self {
        self.arrow = arrow;
        self
    }

    fn eased(&self) -> f32 {
        ease_out_cubic(self.open.clamp(0.0, 1.0))
    }

    /// How far the panel still has to travel, in logical pixels.
    fn travel(&self) -> Px {
        self.gap + self.rise * (1.0 - self.eased())
    }
}

impl ParentElement for Popover {
    fn extend_children(&mut self, children: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(children);
    }
}

impl Styled for Popover {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for Popover {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if self.open.clamp(0.0, 1.0) <= CLOSED {
            style.display = spherekit_layout::Display::None;
            return style;
        }
        style.position = spherekit_layout::Position::Absolute;
        let travel = Length::Px(self.travel());
        // The slide is a margin rather than the inset itself: the inset is
        // already spending its budget on `100 %`, and a `Length` cannot hold
        // "one hundred per cent plus eight pixels".
        match self.side {
            PopoverSide::Top => {
                style.inset.bottom = Length::Fraction(1.0);
                style.margin.bottom = travel;
            }
            PopoverSide::Bottom => {
                style.inset.top = Length::Fraction(1.0);
                style.margin.top = travel;
            }
            PopoverSide::Left => {
                style.inset.right = Length::Fraction(1.0);
                style.margin.right = travel;
            }
            PopoverSide::Right => {
                style.inset.left = Length::Fraction(1.0);
                style.margin.left = travel;
            }
        }
        // The cross axis: which edges of the anchor the panel is pinned to.
        let vertical = matches!(self.side, PopoverSide::Top | PopoverSide::Bottom);
        let zero = Length::Px(Px::ZERO);
        match (self.align, vertical) {
            (PopoverAlign::Start, true) => style.inset.left = zero,
            (PopoverAlign::End, true) => style.inset.right = zero,
            (PopoverAlign::Stretch, true) => {
                // Only where the caller has not pinned or sized it themselves,
                // so an explicit width still wins.
                if matches!(style.inset.left, Length::Auto)
                    && matches!(style.inset.right, Length::Auto)
                    && matches!(style.size.width, Length::Auto)
                {
                    style.inset.left = zero;
                    style.inset.right = zero;
                }
            }
            (PopoverAlign::Start, false) => style.inset.top = zero,
            (PopoverAlign::End, false) => style.inset.bottom = zero,
            (PopoverAlign::Stretch, false) => {
                if matches!(style.inset.top, Length::Auto)
                    && matches!(style.inset.bottom, Length::Auto)
                    && matches!(style.size.height, Length::Auto)
                {
                    style.inset.top = zero;
                    style.inset.bottom = zero;
                }
            }
        }
        if style.z_index == 0 {
            style.z_index = OVERLAY_Z;
        }
        style
    }

    fn children(&mut self) -> &mut [AnyElement] {
        &mut self.children
    }

    fn take_children(&mut self) -> Vec<AnyElement> {
        core::mem::take(&mut self.children)
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
        let c = cx.theme.colors;
        let mut style = self.paint.clone();
        if style.background.is_none() {
            // A popover is one of the few things that has to be opaque over
            // whatever it covers, so it takes the theme's surface rather than
            // the translucency a Mica-backed window uses elsewhere.
            style.background = Some(c.surface.into());
            style.border_width = px(1.0);
            style.border_color = c.border;
            style.corner_radii = Corners::all(cx.theme.radii.lg);
            style.shadows.push(cx.theme.shadows.md);
        }
        let fill = match style.background.as_ref() {
            Some(spherekit_core::Brush::Solid(colour)) => *colour,
            _ => c.surface,
        };
        style.paint_box(cx.canvas, cx.bounds, cx.state);
        if self.arrow {
            self.paint_arrow(cx, fill);
        }
    }

    fn paint_opacity(&self) -> f32 {
        (self.eased() * self.paint.opacity).clamp(0.0, 1.0)
    }

    fn paint_filter(&self) -> Option<spherekit_render::Filter> {
        self.paint.filter
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::new(Role::Dialog, String::new()))
    }
}

impl Popover {
    /// Draws the beak on the edge facing the anchor.
    ///
    /// Painted outside the panel's own box, which an element is allowed to do:
    /// nothing clips a paint to its bounds unless it asks to be clipped. The
    /// beak carries no border, because a bordered triangle butted against a
    /// bordered box shows the seam where the two meet.
    fn paint_arrow(&self, cx: &mut PaintContext<'_, '_>, fill: Color) {
        let b = cx.bounds;
        if b.is_empty() {
            return;
        }
        let reach = px(ARROW);
        // Kept clear of the corner radius, so the beak never grows out of a
        // curve.
        let inset = cx.theme.radii.lg + reach;
        let (tip, base_a, base_b) = match self.side {
            PopoverSide::Bottom | PopoverSide::Top => {
                let along = match self.align {
                    PopoverAlign::End => b.max_x() - inset,
                    _ => b.min_x() + inset,
                };
                let along =
                    along.clamp(b.min_x() + inset, (b.max_x() - inset).max(b.min_x() + inset));
                if self.side == PopoverSide::Bottom {
                    (
                        Point::new(along, b.min_y() - reach),
                        Point::new(along - reach, b.min_y()),
                        Point::new(along + reach, b.min_y()),
                    )
                } else {
                    (
                        Point::new(along, b.max_y() + reach),
                        Point::new(along - reach, b.max_y()),
                        Point::new(along + reach, b.max_y()),
                    )
                }
            }
            PopoverSide::Right | PopoverSide::Left => {
                let along = match self.align {
                    PopoverAlign::End => b.max_y() - inset,
                    _ => b.min_y() + inset,
                };
                let along =
                    along.clamp(b.min_y() + inset, (b.max_y() - inset).max(b.min_y() + inset));
                if self.side == PopoverSide::Right {
                    (
                        Point::new(b.min_x() - reach, along),
                        Point::new(b.min_x(), along - reach),
                        Point::new(b.min_x(), along + reach),
                    )
                } else {
                    (
                        Point::new(b.max_x() + reach, along),
                        Point::new(b.max_x(), along - reach),
                        Point::new(b.max_x(), along + reach),
                    )
                }
            }
        };
        let mut path = spherekit_core::PathBuilder::new();
        path.move_to(base_a);
        path.line_to(tip);
        path.line_to(base_b);
        path.close();
        cx.canvas.fill_path(path.build(), fill);
    }
}

// ---------------------------------------------------------------------------
// Toast
// ---------------------------------------------------------------------------

/// What a [`Toast`] is reporting.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ToastVariant {
    /// Something happened. No judgement attached.
    #[default]
    Info,
    /// Something the user asked for finished.
    Success,
    /// Something finished, but not cleanly.
    Warning,
    /// Something failed.
    Danger,
}

impl ToastVariant {
    /// The palette role this variant tints with.
    fn role(self) -> TextRole {
        match self {
            ToastVariant::Info => TextRole::Accent,
            ToastVariant::Success => TextRole::Success,
            ToastVariant::Warning => TextRole::Warning,
            ToastVariant::Danger => TextRole::Danger,
        }
    }
}

/// A transient message.
///
/// Takes `visible` in `0..=1` like everything else here, which is what leaves
/// the three questions a toast library normally answers badly — when it
/// appears, how long it stays, how many are on screen — with the application
/// that alone can answer them. Give each entry in your own list a
/// [`Motion`](spherekit_core::animate::Motion), retarget it to zero when its
/// time is up, and drop it when the spring has settled.
///
/// ```ignore
/// div().absolute().flex_col().gap(px(8.0)).child(
///     toast("Sync complete", entry.fade.get().value())
///         .variant(ToastVariant::Success)
///         .on_dismiss(move || dismiss(entry.id)),
/// )
/// ```
pub struct Toast {
    id: Option<ElementId>,
    title: String,
    message: String,
    variant: ToastVariant,
    visible: f32,
    action: Option<(String, OnAction)>,
    on_dismiss: Option<OnAction>,
    width: Px,
    style: Style,
    paint: PaintStyle,
}

/// Creates a [`Toast`] with the given message and visibility, `0..=1`.
pub fn toast(message: impl Into<String>, visible: f32) -> Toast {
    Toast {
        id: None,
        title: String::new(),
        message: message.into(),
        variant: ToastVariant::default(),
        visible,
        action: None,
        on_dismiss: None,
        width: px(320.0),
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
    }
}

impl Toast {
    /// Gives the toast a stable identity, which a list of them needs so that
    /// dismissing the first does not renumber the rest.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Adds a heading above the message.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Sets what the toast is reporting.
    pub fn variant(mut self, variant: ToastVariant) -> Self {
        self.variant = variant;
        self
    }

    /// Sets the toast's width. Defaults to 320 px.
    pub fn width(mut self, width: Px) -> Self {
        self.width = width;
        self
    }

    /// Adds one action, such as Undo.
    ///
    /// One, not several: a toast is dismissed by time and a second button is a
    /// decision the user has to make before it disappears.
    pub fn action(mut self, text: impl Into<String>, f: impl FnMut() + 'static) -> Self {
        self.action = Some((text.into(), Box::new(f)));
        self
    }

    /// Adds a close button.
    pub fn on_dismiss(mut self, f: impl FnMut() + 'static) -> Self {
        self.on_dismiss = Some(Box::new(f));
        self
    }

    fn eased(&self) -> f32 {
        ease_out_cubic(self.visible.clamp(0.0, 1.0))
    }
}

impl Styled for Toast {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for Toast {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if self.visible.clamp(0.0, 1.0) <= CLOSED {
            style.display = spherekit_layout::Display::None;
            return style;
        }
        style.display = spherekit_layout::Display::Flex;
        style.flex_direction = spherekit_layout::FlexDirection::Row;
        style.align_items = Some(spherekit_layout::Align::Start);
        style.gap = Size::new(px(10.0), px(10.0));
        style.flex_shrink = 0.0;
        if matches!(style.size.width, Length::Auto) {
            style.size.width = Length::Px(self.width);
        }
        style.padding =
            spherekit_layout::edges_symmetric(Length::Px(px(12.0)), Length::Px(px(14.0)));
        // Slides in from the side it is stacked on. A margin rather than a
        // transform, for the same reason a popover's travel is.
        style.margin.left = Length::Px(px(16.0) * (1.0 - self.eased()));
        style
    }

    /// Builds the toast's contents.
    ///
    /// Done here rather than in the constructor because the builder methods run
    /// after it: `take_children` is called once, at build, and is the first
    /// point at which every option is known.
    fn take_children(&mut self) -> Vec<AnyElement> {
        let role = self.variant.role();
        let mut column = div().flex_col().flex_1().min_w(px(0.0)).gap(px(2.0));
        if !self.title.is_empty() {
            column = column.child(
                label(self.title.clone())
                    .scale(TypeScale::Sm)
                    .weight(spherekit_text::FontWeight::SEMI_BOLD),
            );
        }
        column = column.child(
            label(self.message.clone()).scale(TypeScale::Sm).role(if self.title.is_empty() {
                TextRole::Default
            } else {
                TextRole::Muted
            }),
        );

        let mut children: Vec<AnyElement> = vec![
            // The tinted rule, which is what carries the variant. A filled
            // panel in the variant's colour would shout over the page it is
            // reporting on.
            ToastRule { role }.into_element(),
            column.into_element(),
        ];

        if let Some((text, act)) = self.action.take() {
            children.push(
                crate::widgets::button(text)
                    .variant(crate::widgets::ButtonVariant::Ghost)
                    .text_size(px(12.0))
                    .on_press(act)
                    .into_element(),
            );
        }
        if let Some(dismiss) = self.on_dismiss.take() {
            children.push(ToastClose { press: Some(dismiss) }.into_element());
        }
        children
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
        let c = cx.theme.colors;
        let mut style = self.paint.clone();
        if style.background.is_none() {
            style.background = Some(c.elevated.into());
            style.border_width = px(1.0);
            style.border_color = c.border;
            style.corner_radii = Corners::all(cx.theme.radii.lg);
            style.shadows.push(cx.theme.shadows.md);
        }
        style.paint_box(cx.canvas, cx.bounds, cx.state);
    }

    fn paint_opacity(&self) -> f32 {
        (self.eased() * self.paint.opacity).clamp(0.0, 1.0)
    }

    fn semantics(&self) -> Option<Semantics> {
        let name = if self.title.is_empty() { self.message.clone() } else { self.title.clone() };
        Some(Semantics::new(Role::Dialog, name).description(self.message.clone()))
    }
}

/// The variant-coloured rule down the left of a toast.
struct ToastRule {
    role: TextRole,
}

impl Element for ToastRule {
    fn layout_style(&self) -> Style {
        Style {
            size: Size { width: Length::Px(px(3.0)), height: Length::Fraction(1.0) },
            flex_shrink: 0.0,
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        if cx.bounds.is_empty() {
            return;
        }
        cx.canvas.fill_rounded_rect(
            RoundedRect::uniform(cx.bounds, px(1.5)),
            cx.theme.text_color(self.role),
        );
    }
}

/// The close cross on a toast.
///
/// Drawn rather than shaped, so a toast needs no icon font — two strokes are
/// both cheaper and crisper at twelve pixels than a glyph would be.
struct ToastClose {
    press: Option<OnAction>,
}

impl Element for ToastClose {
    fn layout_style(&self) -> Style {
        Style {
            size: Size { width: Length::Px(px(18.0)), height: Length::Px(px(18.0)) },
            flex_shrink: 0.0,
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
        let b = cx.bounds;
        if b.is_empty() {
            return;
        }
        let c = cx.theme.colors;
        if cx.state.hovered {
            cx.canvas.fill_rounded_rect(RoundedRect::uniform(b, cx.theme.radii.sm), c.hover);
        }
        let tint = if cx.state.hovered { c.text } else { c.text_muted };
        let arm = Px(b.width().get() * 0.22);
        let centre = b.center();
        cx.canvas.draw_line(
            Point::new(centre.x - arm, centre.y - arm),
            Point::new(centre.x + arm, centre.y + arm),
            tint,
            px(1.5),
        );
        cx.canvas.draw_line(
            Point::new(centre.x + arm, centre.y - arm),
            Point::new(centre.x - arm, centre.y + arm),
            tint,
            px(1.5),
        );
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        cx.set_cursor(Cursor::Pointer);
        let pressed = match cx.event {
            UiEvent::MouseUp(e) => {
                e.button == MouseButton::Primary && cx.bounds.contains(e.position)
            }
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                cx.focus();
                false
            }
            UiEvent::Key(k) if k.state.is_pressed() => matches!(k.key, Key::Space | Key::Enter),
            _ => false,
        };
        if pressed {
            if let Some(f) = self.press.as_mut() {
                f();
            }
            cx.notify();
            return EventFlow::Stop;
        }
        EventFlow::Continue
    }

    fn focusable(&self) -> bool {
        true
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::new(Role::Button, "Dismiss").with_implied_actions())
    }
}

/// A container that stacks toasts in a corner of its parent.
///
/// Absolutely positioned and pinned to one corner, laying its children out
/// bottom-up or top-down so the newest is always the one nearest the edge. It
/// deliberately does **not** hit-test as a whole: it is sized to its content,
/// so the empty space around the toasts belongs to the page underneath.
pub fn toast_layer(bottom: bool, right: bool) -> crate::element::Div {
    let mut layer = div().flex_col().gap(px(8.0));
    {
        let style = layer.style_mut();
        style.position = spherekit_layout::Position::Absolute;
        let edge = Length::Px(px(16.0));
        if bottom {
            style.inset.bottom = edge;
        } else {
            style.inset.top = edge;
        }
        if right {
            style.inset.right = edge;
        } else {
            style.inset.left = edge;
        }
        style.align_items =
            Some(if right { spherekit_layout::Align::End } else { spherekit_layout::Align::Start });
        style.z_index = TOAST_Z;
    }
    layer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{Interactive, IntoElement, ParentElement};
    use crate::event::{ElementState, Modifiers, MouseButtonEvent};
    use crate::tree::UiTree;
    use spherekit_core::{px, relative, size};
    use std::cell::Cell;
    use std::rc::Rc;

    fn viewport() -> Size<Px> {
        size(px(400.0), px(300.0))
    }

    fn press_at(x: f32, y: f32) -> UiEvent {
        UiEvent::MouseDown(MouseButtonEvent {
            position: Point::new(px(x), px(y)),
            button: MouseButton::Primary,
            state: ElementState::Pressed,
            click_count: 1,
            modifiers: Modifiers::NONE,
            source: crate::event::PointerSource::Mouse,
        })
    }

    #[test]
    fn a_shut_overlay_leaves_layout_entirely() {
        // The property that keeps an invisible scrim from eating the window.
        let mut tree = UiTree::new();
        tree.build(
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(overlay(0.0).child(div().w(px(100.0)).h(px(50.0))))
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();
        let root = tree.layout().roots()[0];
        let scrim = tree.layout().children(root)[0];
        assert_eq!(tree.layout().layout(scrim).unwrap().bounds.width(), Px::ZERO);
    }

    #[test]
    fn an_open_overlay_fills_its_parent() {
        let mut tree = UiTree::new();
        tree.build(div().w(px(200.0)).h(px(120.0)).child(overlay(1.0)).into_element());
        tree.compute_layout(viewport()).unwrap();
        let root = tree.layout().roots()[0];
        let scrim = tree.layout().children(root)[0];
        let bounds = tree.layout().layout(scrim).unwrap().bounds;
        assert_eq!(bounds.width(), px(200.0));
        assert_eq!(bounds.height(), px(120.0));
    }

    #[test]
    fn a_scrim_swallows_a_press_and_reports_it() {
        let dismissed = Rc::new(Cell::new(0));
        let d = dismissed.clone();
        let behind = Rc::new(Cell::new(0));
        let b = behind.clone();
        let mut tree = UiTree::new();
        tree.build(
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(
                    div()
                        .id("behind")
                        .w(relative(1.0))
                        .h(relative(1.0))
                        .on_mouse_down(move |_| b.set(b.get() + 1)),
                )
                .child(overlay(1.0).id("scrim").on_dismiss(move || d.set(d.get() + 1)))
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();
        tree.dispatch(&press_at(200.0, 150.0));
        assert_eq!(dismissed.get(), 1, "the scrim did not report the press");
        assert_eq!(behind.get(), 0, "the press reached the page behind the scrim");
    }

    #[test]
    fn an_undismissible_scrim_still_swallows() {
        let behind = Rc::new(Cell::new(0));
        let b = behind.clone();
        let mut tree = UiTree::new();
        tree.build(
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(
                    div()
                        .id("behind")
                        .w(relative(1.0))
                        .h(relative(1.0))
                        .on_mouse_down(move |_| b.set(b.get() + 1)),
                )
                .child(overlay(1.0).id("scrim").dismissible(false))
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();
        tree.dispatch(&press_at(200.0, 150.0));
        assert_eq!(behind.get(), 0);
    }

    #[test]
    fn a_popover_opens_on_the_side_it_was_asked_for() {
        let anchor = px(60.0);
        for (side, expect) in [
            (PopoverSide::Bottom, "below"),
            (PopoverSide::Top, "above"),
            (PopoverSide::Right, "right"),
            (PopoverSide::Left, "left"),
        ] {
            let mut tree = UiTree::new();
            tree.build(
                div()
                    .flex_col()
                    .w(anchor)
                    .h(anchor)
                    .child(popover(1.0).id("pop").w(px(40.0)).h(px(20.0)).side(side))
                    .into_element(),
            );
            tree.compute_layout(viewport()).unwrap();
            let root = tree.layout().roots()[0];
            let anchor_box = tree.layout().layout(root).unwrap().bounds;
            let panel = tree.layout().layout(tree.layout().children(root)[0]).unwrap().bounds;
            match expect {
                "below" => assert!(panel.min_y() >= anchor_box.max_y(), "{side:?}: {panel:?}"),
                "above" => assert!(panel.max_y() <= anchor_box.min_y(), "{side:?}: {panel:?}"),
                "right" => assert!(panel.min_x() >= anchor_box.max_x(), "{side:?}: {panel:?}"),
                _ => assert!(panel.max_x() <= anchor_box.min_x(), "{side:?}: {panel:?}"),
            }
        }
    }

    #[test]
    fn a_stretched_popover_spans_its_anchor() {
        let mut tree = UiTree::new();
        tree.build(
            div()
                .flex_col()
                .w(px(180.0))
                .h(px(40.0))
                .child(popover(1.0).h(px(30.0)))
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();
        let root = tree.layout().roots()[0];
        let panel = tree.layout().layout(tree.layout().children(root)[0]).unwrap().bounds;
        assert_eq!(panel.width(), px(180.0));
    }

    #[test]
    fn an_opening_popover_travels_and_fades_together() {
        // Half open is half faded and half of the way there, which is the
        // property that lets one spring drive both.
        let half = popover(0.5);
        assert!(half.paint_opacity() > 0.0 && half.paint_opacity() < 1.0);
        assert!(half.travel() > popover(1.0).travel());
    }

    #[test]
    fn a_toast_carries_its_message_into_its_semantics() {
        let t = toast("Sync complete", 1.0).title("Backup");
        let s = t.semantics().expect("a toast is announced");
        assert_eq!(s.label.as_deref(), Some("Backup"));
        assert_eq!(s.description.as_deref(), Some("Sync complete"));
    }

    #[test]
    fn a_dismissed_toast_leaves_layout() {
        let mut tree = UiTree::new();
        tree.build(
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(toast("gone", 0.0).id("t"))
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();
        let root = tree.layout().roots()[0];
        let node = tree.layout().children(root)[0];
        assert_eq!(tree.layout().layout(node).unwrap().bounds.width(), Px::ZERO);
    }
}

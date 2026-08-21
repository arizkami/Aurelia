//! The built-in widgets.
//!
//! Every widget here is an [`Element`] composed from the same primitives an
//! application would use. None of them is privileged: `button()` is a `div()`
//! with a background, a label and a click handler, and reading its source is
//! the fastest way to learn how to write your own.
//!
//! ## The state model
//!
//! A widget does **not** own its value. It takes the current value and reports
//! changes through a callback:
//!
//! ```ignore
//! knob(self.threshold.get())
//!     .range(-60.0, 0.0)
//!     .on_change({
//!         let threshold = self.threshold.clone();
//!         move |v| threshold.set(v)
//!     })
//! ```
//!
//! That is explicit data flow rather than a hidden global store, and it means a
//! parameter that lives in a DSP struct, an undo stack or a host automation
//! lane needs no adapter — the widget never disagreed with it in the first
//! place.
//!
//! Transient state that a *drag* needs — where the pointer went down, what the
//! value was then — lives in the node's retained scratch
//! ([`EventContext::scratch`]), because the element is rebuilt every frame and
//! the node is not.
//!
//! ## Why knobs drag vertically
//!
//! Angular drag — point at the knob, rotate the wrist — is the obvious design
//! and it is bad. It is imprecise near the centre, it is ambiguous at the
//! wrap-around, and it demands a movement the wrist is poor at. Every audio
//! tool converged on vertical drag instead, and Sphere does not deviate.

use crate::element::{AnyElement, Element, EventContext, PaintContext, ParentElement, Styled, div};
use crate::event::{EventFlow, Key, MouseButton, UiEvent};
use crate::semantics::{Role, Semantics, ValueRange};
use crate::style::{Cursor, FocusRing, PaintStyle};
use crate::text::label;
use sphere_core::{Color, Corners, ElementId, Point, Px, Rect, RoundedRect, Size, px, relative};
use sphere_layout::{Overflow, Style};

/// Scratch slot holding the value a drag started from.
const SCRATCH_DRAG_VALUE: usize = 0;
/// Scratch slot holding the pointer coordinate a drag started from.
const SCRATCH_DRAG_ORIGIN: usize = 1;
/// Scratch slot holding whether a drag is active. Nonzero means yes.
const SCRATCH_DRAGGING: usize = 2;

/// A change callback.
pub type OnChange = Box<dyn FnMut(f32)>;
/// A plain action callback.
pub type OnAction = Box<dyn FnMut()>;

// ---------------------------------------------------------------------------
// Button
// ---------------------------------------------------------------------------

/// Which visual weight a button carries.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ButtonVariant {
    /// The default action in its context: filled with the accent colour.
    Primary,
    /// A normal action: filled with the surface colour.
    #[default]
    Secondary,
    /// A low-emphasis action: no fill until hovered.
    Ghost,
    /// A destructive action.
    Danger,
}

/// A push button.
pub struct Button {
    id: Option<ElementId>,
    text: String,
    variant: ButtonVariant,
    style: Style,
    disabled: bool,
    on_press: Option<OnAction>,
}

/// Creates a [`Button`].
pub fn button(text: impl Into<String>) -> Button {
    Button {
        id: None,
        text: text.into(),
        variant: ButtonVariant::default(),
        style: Style::DEFAULT,
        disabled: false,
        on_press: None,
    }
}

impl Button {
    /// Gives the button a stable identity, which it needs to keep focus across
    /// rebuilds.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Sets the visual weight.
    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    /// Marks the button disabled: it stops accepting input and leaves the tab
    /// order.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Runs when the button is activated by click, Enter or Space.
    pub fn on_press(mut self, f: impl FnMut() + 'static) -> Self {
        self.on_press = Some(Box::new(f));
        self
    }

    /// Sets the button's width.
    pub fn width(mut self, width: impl Into<sphere_core::Length>) -> Self {
        self.style.size.width = width.into();
        self
    }
}

impl Button {
    /// The style the label is measured *and* painted with.
    ///
    /// One function so the two can never drift apart. `measure` reserves the
    /// box and `paint` fills it; if they disagree on the font size the text
    /// ends up outside the box the layout engine agreed to.
    fn label_style(theme: &crate::theme::Theme) -> sphere_text::TextStyle {
        sphere_text::TextStyle {
            font_size: theme.typography.md,
            wrap: sphere_text::WrapMode::None,
            ..Default::default()
        }
    }
}

impl Element for Button {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    /// A button's label is painted as a leaf, not laid out as a child, so
    /// nothing else reports its size. Without this the button has no intrinsic
    /// width at all: in an `Auto`-width slot it collapses to its own padding
    /// and the label spills over whatever is beside it.
    fn measure(
        &mut self,
        _request: &sphere_layout::MeasureRequest<'_>,
        text: &mut sphere_text::TextSystem,
        theme: &crate::theme::Theme,
    ) -> Option<Size<Px>> {
        if self.text.is_empty() {
            return Some(Size::ZERO);
        }
        // Content size only: taffy adds the padding and border from
        // `layout_style` on top of whatever comes back from here.
        Some(text.layout(&self.text, &Self::label_style(theme), None).size)
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if matches!(style.size.height, sphere_core::Length::Auto) {
            style.size.height = sphere_core::Length::Px(px(28.0));
        }
        style.display = sphere_layout::Display::Flex;
        style.align_items = Some(sphere_layout::Align::Center);
        style.justify_content = Some(sphere_layout::Distribute::Center);
        style.padding = sphere_layout::edges_symmetric(
            sphere_core::Length::Px(px(4.0)),
            sphere_core::Length::Px(px(12.0)),
        );
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let c = cx.theme.colors;
        let (base, hover, active, text) = match self.variant {
            ButtonVariant::Primary => (c.accent, c.accent_hover, c.accent_hover, c.text_on_accent),
            ButtonVariant::Secondary => (c.elevated, c.hover, c.pressed, c.text),
            ButtonVariant::Ghost => (Color::TRANSPARENT, c.hover, c.pressed, c.text),
            ButtonVariant::Danger => (c.danger, c.danger, c.danger, c.text_on_accent),
        };

        let mut style = PaintStyle {
            background: Some(base.into()),
            hover_background: Some(hover.into()),
            active_background: Some(active.into()),
            corner_radii: Corners::all(cx.theme.radii.md),
            border_width: if self.variant == ButtonVariant::Secondary { px(1.0) } else { Px::ZERO },
            border_color: c.border,
            focus_ring: Some(FocusRing { color: c.focus, ..FocusRing::default() }),
            ..Default::default()
        };
        // Disabled controls are dimmed rather than recoloured, so their shape
        // still reads and the layout does not shift.
        if self.disabled {
            style.opacity = 0.45;
            style.hover_background = None;
            style.active_background = None;
        }
        let mut state = cx.state;
        state.disabled = self.disabled;
        style.paint_box(cx.canvas, cx.bounds, state);

        if self.text.is_empty() {
            return;
        }
        let color = if self.disabled { c.text_muted } else { text };
        let style_for_text = Self::label_style(cx.theme);
        let mut content = label(self.text.clone())
            .text_size(style_for_text.font_size)
            .text_color(color)
            .no_wrap();
        // Centre the label in the button's box. The label is a leaf here rather
        // than a child element, so its box is computed rather than laid out --
        // with the same style `measure` used, or the centring is off by the
        // difference between the two sizes.
        let size = cx.text.layout(&self.text, &style_for_text, None).size;
        let origin = Point::new(
            cx.bounds.min_x() + Px((cx.bounds.width().get() - size.width.get()) * 0.5),
            cx.bounds.min_y() + Px((cx.bounds.height().get() - size.height.get()) * 0.5),
        );
        let mut inner = PaintContext {
            canvas: cx.canvas,
            text: cx.text,
            bounds: Rect::new(origin, size),
            visible: cx.visible,
            scratch: cx.scratch,
            state,
            theme: cx.theme,
            time: cx.time,
        };
        content.paint(&mut inner);
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.disabled {
            return EventFlow::Continue;
        }
        cx.set_cursor(Cursor::Pointer);
        let activated = match cx.event {
            UiEvent::MouseUp(e) => {
                e.button == MouseButton::Primary && cx.bounds.contains(e.position)
            }
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                cx.focus();
                false
            }
            // Enter and Space are the two keys every toolkit activates a button
            // with; supporting only one makes the button feel broken to whoever
            // uses the other.
            UiEvent::Key(k) if k.state.is_pressed() => {
                matches!(k.key, Key::Enter | Key::Space)
            }
            _ => false,
        };
        if activated {
            if let Some(f) = self.on_press.as_mut() {
                f();
            }
            cx.notify();
            return EventFlow::Stop;
        }
        EventFlow::Continue
    }

    fn focusable(&self) -> bool {
        !self.disabled
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(
            Semantics::new(Role::Button, self.text.clone())
                .disabled(self.disabled)
                .with_implied_actions(),
        )
    }
}

// ---------------------------------------------------------------------------
// Toggle and checkbox
// ---------------------------------------------------------------------------

/// A two-state switch or checkbox.
pub struct Toggle {
    id: Option<ElementId>,
    checked: bool,
    text: String,
    as_checkbox: bool,
    disabled: bool,
    on_change: Option<Box<dyn FnMut(bool)>>,
}

/// Creates a switch-style [`Toggle`].
pub fn toggle(checked: bool) -> Toggle {
    Toggle {
        id: None,
        checked,
        text: String::new(),
        as_checkbox: false,
        disabled: false,
        on_change: None,
    }
}

/// Creates a checkbox-style [`Toggle`].
pub fn checkbox(checked: bool) -> Toggle {
    Toggle { as_checkbox: true, ..toggle(checked) }
}

impl Toggle {
    /// Gives the toggle a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Sets the accessible name and, for a checkbox, the visible label.
    pub fn label(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Marks the toggle disabled.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Runs when the toggle flips, with the new state.
    pub fn on_change(mut self, f: impl FnMut(bool) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }
}

impl Element for Toggle {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let (w, h) = if self.as_checkbox { (16.0, 16.0) } else { (34.0, 18.0) };
        Style {
            size: sphere_core::Size {
                width: sphere_core::Length::Px(px(w)),
                height: sphere_core::Length::Px(px(h)),
            },
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let c = cx.theme.colors;
        let bounds = cx.bounds;
        if bounds.is_empty() {
            return;
        }
        let mut state = cx.state;
        state.disabled = self.disabled;

        if self.as_checkbox {
            let style = PaintStyle {
                background: Some(if self.checked { c.accent } else { c.elevated }.into()),
                hover_background: Some(if self.checked { c.accent_hover } else { c.hover }.into()),
                border_width: px(1.0),
                border_color: if self.checked { c.accent } else { c.border_strong },
                corner_radii: Corners::all(cx.theme.radii.sm),
                focus_ring: Some(FocusRing { color: c.focus, ..FocusRing::default() }),
                opacity: if self.disabled { 0.45 } else { 1.0 },
                ..Default::default()
            };
            style.paint_box(cx.canvas, bounds, state);
            if self.checked {
                // A tick drawn as two strokes, so it stays crisp at 16 px where
                // a glyph would be blurry.
                let b = bounds.inset(sphere_core::Edges::all(px(4.0)));
                let mid = Point::new(b.min_x() + b.width() * 0.38, b.max_y() - b.height() * 0.15);
                cx.canvas.draw_line(
                    Point::new(b.min_x(), b.min_y() + b.height() * 0.52),
                    mid,
                    c.text_on_accent,
                    px(2.0),
                );
                cx.canvas.draw_line(
                    mid,
                    Point::new(b.max_x(), b.min_y()),
                    c.text_on_accent,
                    px(2.0),
                );
            }
            return;
        }

        // Switch: a pill track with a circular knob that slides.
        let radius = Px(bounds.height().get() * 0.5);
        let track = PaintStyle {
            background: Some(if self.checked { c.accent } else { c.elevated }.into()),
            hover_background: Some(if self.checked { c.accent_hover } else { c.hover }.into()),
            border_width: px(1.0),
            border_color: if self.checked { c.accent } else { c.border_strong },
            corner_radii: Corners::all(radius),
            focus_ring: Some(FocusRing { color: c.focus, ..FocusRing::default() }),
            opacity: if self.disabled { 0.45 } else { 1.0 },
            ..Default::default()
        };
        track.paint_box(cx.canvas, bounds, state);

        let inset = px(2.5);
        let knob_radius = radius - inset;
        let travel = bounds.width() - radius * 2.0;
        let cx_pos = bounds.min_x() + radius + if self.checked { travel } else { Px::ZERO };
        cx.canvas.fill_circle(
            Point::new(cx_pos, bounds.center().y),
            knob_radius,
            if self.disabled { c.text_muted } else { c.text_on_accent },
        );
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.disabled {
            return EventFlow::Continue;
        }
        cx.set_cursor(Cursor::Pointer);
        let flip = match cx.event {
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
        if flip {
            if let Some(f) = self.on_change.as_mut() {
                f(!self.checked);
            }
            cx.notify();
            return EventFlow::Stop;
        }
        EventFlow::Continue
    }

    fn focusable(&self) -> bool {
        !self.disabled
    }

    fn semantics(&self) -> Option<Semantics> {
        let role = if self.as_checkbox { Role::Checkbox } else { Role::Toggle };
        Some(
            Semantics::new(role, self.text.clone())
                .checked(self.checked)
                .disabled(self.disabled)
                .with_implied_actions(),
        )
    }
}

// ---------------------------------------------------------------------------
// Value controls
// ---------------------------------------------------------------------------

/// Which shape a value control takes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueShape {
    /// A horizontal track with a handle.
    HorizontalSlider,
    /// A vertical track with a handle, the shape a channel fader takes.
    VerticalFader,
    /// A rotary dial.
    Knob,
}

/// A continuous value control: slider, fader or knob.
///
/// One type for all three because they differ only in how they are drawn and
/// which axis the drag reads; the value maths, the keyboard handling, the fine
/// mode and the reset are identical, and three copies of that would be three
/// places for them to diverge.
pub struct ValueControl {
    id: Option<ElementId>,
    value: f32,
    min: f32,
    max: f32,
    default: f32,
    step: Option<f32>,
    shape: ValueShape,
    name: String,
    unit: String,
    /// Turns a value into the string a user should read.
    format: Option<Box<dyn Fn(f32) -> String>>,
    /// True when the control's centre, not its minimum, is the resting value.
    bipolar: bool,
    disabled: bool,
    style: Style,
    on_change: Option<OnChange>,
}

/// Creates a horizontal slider.
pub fn slider(value: f32) -> ValueControl {
    ValueControl::new(value, ValueShape::HorizontalSlider)
}

/// Creates a vertical fader.
pub fn fader(value: f32) -> ValueControl {
    ValueControl::new(value, ValueShape::VerticalFader)
}

/// Creates a rotary knob.
pub fn knob(value: f32) -> ValueControl {
    ValueControl::new(value, ValueShape::Knob)
}

impl ValueControl {
    fn new(value: f32, shape: ValueShape) -> Self {
        Self {
            id: None,
            value,
            min: 0.0,
            max: 1.0,
            default: value,
            step: None,
            shape,
            name: String::new(),
            unit: String::new(),
            format: None,
            bipolar: false,
            disabled: false,
            style: Style::DEFAULT,
            on_change: None,
        }
    }

    /// How far a full-travel drag is, in logical pixels.
    ///
    /// Roughly the height of a channel strip, which is the distance a hand
    /// moves comfortably in one gesture.
    const TRAVEL_PX: f32 = 180.0;

    /// Gives the control a stable identity, which it needs to keep its drag
    /// state and focus across rebuilds.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Sets the value range.
    pub fn range(mut self, min: f32, max: f32) -> Self {
        self.min = min;
        self.max = max;
        self
    }

    /// Sets the value a double-click returns to.
    pub fn default_value(mut self, default: f32) -> Self {
        self.default = default;
        self
    }

    /// Quantises the value to a step.
    pub fn step(mut self, step: f32) -> Self {
        self.step = Some(step);
        self
    }

    /// Sets the accessible name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Sets the unit appended to the announced value.
    pub fn unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = unit.into();
        self
    }

    /// Formats the value for display and for assistive technology.
    ///
    /// A fader should announce "-6.0 dB", not "79 %". Only the caller knows the
    /// mapping, so only the caller can supply it.
    pub fn format(mut self, f: impl Fn(f32) -> String + 'static) -> Self {
        self.format = Some(Box::new(f));
        self
    }

    /// Marks the control bipolar, so its fill grows out from the centre.
    pub fn bipolar(mut self, bipolar: bool) -> Self {
        self.bipolar = bipolar;
        self
    }

    /// Marks the control disabled.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Runs when the value changes.
    pub fn on_change(mut self, f: impl FnMut(f32) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }

    /// Sets an explicit size.
    pub fn size(mut self, width: Px, height: Px) -> Self {
        self.style.size = sphere_core::Size {
            width: sphere_core::Length::Px(width),
            height: sphere_core::Length::Px(height),
        };
        self
    }

    fn span(&self) -> f32 {
        let s = self.max - self.min;
        if s.abs() < f32::EPSILON { 1.0 } else { s }
    }

    fn normalized(&self) -> f32 {
        ((self.value - self.min) / self.span()).clamp(0.0, 1.0)
    }

    fn quantize(&self, value: f32) -> f32 {
        let clamped = value.clamp(self.min.min(self.max), self.max.max(self.min));
        match self.step {
            Some(step) if step > 0.0 => {
                // Quantise relative to `min`, not to zero: a range of 20..=20000
                // stepped by 10 should hit 20, 30, 40 — not 20, 30 and also 25.
                self.min + ((clamped - self.min) / step).round() * step
            }
            _ => clamped,
        }
    }

    fn emit(&mut self, value: f32) {
        let value = self.quantize(value);
        if (value - self.value).abs() > f32::EPSILON
            && let Some(f) = self.on_change.as_mut()
        {
            f(value);
        }
    }

    fn display(&self) -> String {
        match &self.format {
            Some(f) => f(self.value),
            None if self.unit.is_empty() => format!("{:.2}", self.value),
            None => format!("{:.2} {}", self.value, self.unit),
        }
    }

    /// The keyboard increment: one step, or one per cent of the range.
    fn keyboard_step(&self, fine: bool) -> f32 {
        let base = self.step.unwrap_or(self.span().abs() * 0.01);
        if fine { base * 0.1 } else { base }
    }
}

impl ValueControl {
    /// The box the focus ring is drawn around, and its corner radius.
    ///
    /// Each shape returns the moving part rather than its layout box: the
    /// slider's handle, the fader's cap, the knob's dial. `t` is the
    /// normalised value, so the ring follows the control.
    fn focus_ring_box(&self, b: Rect<Px>, t: f32) -> (Rect<Px>, Px) {
        match self.shape {
            ValueShape::HorizontalSlider => {
                let r = px(7.0);
                let centre = Point::new(b.min_x() + b.width() * t, b.center().y);
                (Rect::new(Point::new(centre.x - r, centre.y - r), Size::new(r * 2.0, r * 2.0)), r)
            }
            ValueShape::VerticalFader => {
                let cap_h = px(12.0);
                let y = b.max_y() - b.height() * t - cap_h * 0.5;
                (Rect::new(Point::new(b.min_x(), y), Size::new(b.width(), cap_h)), px(2.0))
            }
            ValueShape::Knob => {
                let centre = b.center();
                // Matches the dial radius in `paint`, plus the stroke's half
                // width so the ring clears the track rather than sitting on it.
                let r = Px(b.width().get().min(b.height().get()) * 0.42) + px(2.0);
                (Rect::new(Point::new(centre.x - r, centre.y - r), Size::new(r * 2.0, r * 2.0)), r)
            }
        }
    }
}

impl Element for ValueControl {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        let (w, h) = match self.shape {
            ValueShape::HorizontalSlider => (relative(1.0), sphere_core::Length::Px(px(20.0))),
            ValueShape::VerticalFader => (sphere_core::Length::Px(px(28.0)), relative(1.0)),
            ValueShape::Knob => {
                (sphere_core::Length::Px(px(48.0)), sphere_core::Length::Px(px(48.0)))
            }
        };
        if matches!(style.size.width, sphere_core::Length::Auto) {
            style.size.width = w;
        }
        if matches!(style.size.height, sphere_core::Length::Auto) {
            style.size.height = h;
        }
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let c = cx.theme.colors;
        let b = cx.bounds;
        if b.is_empty() {
            return;
        }
        let t = self.normalized();
        let accent = if self.disabled {
            c.text_muted
        } else if cx.state.hovered || cx.state.active {
            c.accent_hover
        } else {
            c.accent
        };

        match self.shape {
            ValueShape::HorizontalSlider => {
                let track_h = px(4.0);
                let track = Rect::new(
                    Point::new(b.min_x(), b.center().y - track_h * 0.5),
                    Size::new(b.width(), track_h),
                );
                cx.canvas.fill_rounded_rect(RoundedRect::uniform(track, track_h * 0.5), c.elevated);
                // A bipolar control fills from the centre outward, so "no
                // change" reads as an empty track rather than a half-full one.
                let fill = if self.bipolar {
                    let centre = b.min_x() + b.width() * 0.5;
                    let end = b.min_x() + b.width() * t;
                    Rect::from_corners(
                        Point::new(centre.min(end), track.min_y()),
                        Point::new(centre.max(end), track.max_y()),
                    )
                } else {
                    Rect::new(track.origin, Size::new(b.width() * t, track_h))
                };
                if !fill.is_empty() {
                    cx.canvas.fill_rounded_rect(RoundedRect::uniform(fill, track_h * 0.5), accent);
                }
                let handle_r = px(7.0);
                cx.canvas.fill_circle(
                    Point::new(b.min_x() + b.width() * t, b.center().y),
                    handle_r,
                    if self.disabled { c.text_muted } else { c.text },
                );
            }
            ValueShape::VerticalFader => {
                let track_w = px(4.0);
                let track = Rect::new(
                    Point::new(b.center().x - track_w * 0.5, b.min_y()),
                    Size::new(track_w, b.height()),
                );
                cx.canvas.fill_rounded_rect(RoundedRect::uniform(track, track_w * 0.5), c.elevated);
                // A fader fills upward from the bottom, like the hardware.
                let filled = b.height() * t;
                let fill = Rect::new(
                    Point::new(track.min_x(), b.max_y() - filled),
                    Size::new(track_w, filled),
                );
                if !fill.is_empty() {
                    cx.canvas.fill_rounded_rect(RoundedRect::uniform(fill, track_w * 0.5), accent);
                }
                let cap_h = px(12.0);
                let cap = Rect::new(
                    Point::new(b.min_x(), b.max_y() - filled - cap_h * 0.5),
                    Size::new(b.width(), cap_h),
                );
                cx.canvas.quad(
                    cap.intersection(b.outset(sphere_core::Edges::all(cap_h))),
                    Corners::all(px(2.0)),
                    Some(if self.disabled { c.text_muted } else { c.text }.into()),
                    c.border_strong,
                    px(1.0),
                );
            }
            ValueShape::Knob => {
                let centre = b.center();
                let radius = Px(b.width().get().min(b.height().get()) * 0.42);
                let width = px(4.0);
                cx.canvas.stroke_circle(centre, radius, c.elevated, width);

                // The arc runs from bottom-left clockwise through 270 degrees,
                // the layout every hardware knob uses; the gap at the bottom is
                // what makes minimum and maximum visually distinguishable.
                const START_TURNS: f32 = -0.625;
                const SWEEP_TURNS: f32 = 0.75;
                let arc_from: f32 = if self.bipolar { 0.5 } else { 0.0 };
                let mut arc = sphere_core::PathBuilder::new();
                let steps = 48;
                let (a0, a1) = (arc_from.min(t), arc_from.max(t));
                for i in 0..=steps {
                    let f = a0 + (a1 - a0) * (i as f32 / steps as f32);
                    let angle = (START_TURNS + f * SWEEP_TURNS) * core::f32::consts::TAU;
                    let (sin, cos) = angle.sin_cos();
                    let p = Point::new(
                        centre.x + Px(cos * radius.get()),
                        centre.y + Px(sin * radius.get()),
                    );
                    if i == 0 {
                        arc.move_to(p);
                    } else {
                        arc.line_to(p);
                    }
                }
                if (a1 - a0).abs() > 1e-4 {
                    cx.canvas.stroke_path(
                        arc.build(),
                        accent,
                        sphere_core::Stroke::new(width).with_cap(sphere_core::LineCap::Round),
                    );
                }

                let angle = (START_TURNS + t * SWEEP_TURNS) * core::f32::consts::TAU;
                let (sin, cos) = angle.sin_cos();
                cx.canvas.draw_line(
                    Point::new(
                        centre.x + Px(cos * radius.get() * 0.3),
                        centre.y + Px(sin * radius.get() * 0.3),
                    ),
                    Point::new(
                        centre.x + Px(cos * radius.get() * 0.82),
                        centre.y + Px(sin * radius.get() * 0.82),
                    ),
                    if self.disabled { c.text_muted } else { c.text },
                    px(2.0),
                );
            }
        }

        if cx.state.focused {
            // Ring the part that actually moves, not the whole row. A slider's
            // hit box is full width and only a few pixels of it carry any
            // graphics, so a ring around `b` draws a large empty rectangle that
            // reads as a text field and collides with whatever label sits above
            // it. Ringing the handle is both smaller and more informative: it
            // says where the arrow keys will act.
            let (ring_box, radius) = self.focus_ring_box(b, t);
            let ring = FocusRing { color: c.focus, ..FocusRing::default() };
            let style = PaintStyle {
                focus_ring: Some(ring),
                corner_radii: Corners::all(radius),
                ..Default::default()
            };
            style.paint_box(cx.canvas, ring_box, cx.state);
        }
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.disabled {
            return EventFlow::Continue;
        }
        // The vertical drag applies to every shape, knob included; see the
        // module docs for why a knob does not track rotation.
        cx.set_cursor(match self.shape {
            ValueShape::HorizontalSlider => Cursor::ResizeEw,
            _ => Cursor::ResizeNs,
        });

        match cx.event {
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                cx.focus();
                if e.click_count >= 2 {
                    // Double-click resets. Every plug-in does this and there is
                    // no other way back to a default without typing.
                    self.emit(self.default);
                    cx.scratch[SCRATCH_DRAGGING] = 0.0;
                    cx.notify();
                    return EventFlow::Stop;
                }

                // A horizontal slider jumps to where it was clicked, then drags
                // from there; a knob and a fader do not, because a jump would
                // make a mis-click destroy a carefully set value.
                let start_value = if self.shape == ValueShape::HorizontalSlider {
                    let t =
                        ((e.position.x - cx.bounds.min_x()) / cx.bounds.width()).clamp(0.0, 1.0);
                    let v = self.min + t * self.span();
                    self.emit(v);
                    v
                } else {
                    self.value
                };

                cx.scratch[SCRATCH_DRAG_VALUE] = start_value;
                cx.scratch[SCRATCH_DRAG_ORIGIN] = match self.shape {
                    ValueShape::HorizontalSlider => e.position.x.get(),
                    _ => e.position.y.get(),
                };
                cx.scratch[SCRATCH_DRAGGING] = 1.0;
                // Without capture the control stops tracking the moment the
                // pointer leaves its box, which for a 28 px-wide fader is
                // almost immediately.
                cx.capture();
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::MouseMove(e) if cx.scratch[SCRATCH_DRAGGING] != 0.0 => {
                let start_value = cx.scratch[SCRATCH_DRAG_VALUE];
                let origin = cx.scratch[SCRATCH_DRAG_ORIGIN];
                let travel = match self.shape {
                    // Right and up both increase, which is what a user expects
                    // from a horizontal and a vertical control respectively.
                    ValueShape::HorizontalSlider => e.position.x.get() - origin,
                    _ => origin - e.position.y.get(),
                };
                let sensitivity = if e.modifiers.shift { 0.2 } else { 1.0 };
                let delta = travel / Self::TRAVEL_PX * self.span() * sensitivity;
                self.emit(start_value + delta);
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::MouseUp(e) if e.button == MouseButton::Primary => {
                if cx.scratch[SCRATCH_DRAGGING] != 0.0 {
                    cx.scratch[SCRATCH_DRAGGING] = 0.0;
                    cx.release();
                    cx.notify();
                    return EventFlow::Stop;
                }
                EventFlow::Continue
            }
            UiEvent::Scroll(e) => {
                let lines = match e.delta {
                    crate::event::ScrollDelta::Lines(d) => d.height,
                    crate::event::ScrollDelta::Pixels(d) => d.height.get() / 20.0,
                };
                self.emit(self.value + lines * self.keyboard_step(e.modifiers.shift));
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::Key(k) if k.state.is_pressed() => {
                let step = self.keyboard_step(k.modifiers.shift);
                let next = match k.key {
                    Key::Up | Key::Right => Some(self.value + step),
                    Key::Down | Key::Left => Some(self.value - step),
                    Key::PageUp => Some(self.value + step * 10.0),
                    Key::PageDown => Some(self.value - step * 10.0),
                    Key::Home => Some(self.min),
                    Key::End => Some(self.max),
                    _ => None,
                };
                match next {
                    Some(v) => {
                        self.emit(v);
                        cx.notify();
                        EventFlow::Stop
                    }
                    None => EventFlow::Continue,
                }
            }
            _ => EventFlow::Continue,
        }
    }

    fn focusable(&self) -> bool {
        !self.disabled
    }

    fn semantics(&self) -> Option<Semantics> {
        let role = match self.shape {
            ValueShape::HorizontalSlider => Role::Slider,
            ValueShape::VerticalFader => Role::Fader,
            ValueShape::Knob => Role::Knob,
        };
        Some(
            Semantics::new(role, self.name.clone())
                .value(ValueRange {
                    value: self.value,
                    min: self.min,
                    max: self.max,
                    step: self.step,
                })
                .value_text(self.display())
                .disabled(self.disabled)
                .with_implied_actions(),
        )
    }
}

// ---------------------------------------------------------------------------
// Containers and decoration
// ---------------------------------------------------------------------------

/// A framed panel with an optional title.
pub fn panel(title: impl Into<String>) -> crate::element::Div {
    let title = title.into();
    let mut root = div().flex_col();
    if !title.is_empty() {
        root = root.child(label(title));
    }
    root
}

/// A one-pixel divider.
///
/// Explicitly one *logical* pixel: at 150 % scaling that becomes 1.5 device
/// pixels and antialiases, which is correct. Snapping it to a whole device
/// pixel would make dividers in a list drift relative to the content beside
/// them.
pub fn separator(vertical: bool) -> crate::element::Div {
    let d = div().bg(Color::hex(0x2E333B));
    if vertical { d.w(px(1.0)).h(relative(1.0)) } else { d.h(px(1.0)).w(relative(1.0)) }
}

/// A determinate progress bar, `0..=1`.
pub fn progress(fraction: f32) -> crate::element::Div {
    let t = fraction.clamp(0.0, 1.0);
    div()
        .h(px(4.0))
        .w(relative(1.0))
        .rounded(px(2.0))
        .bg(Color::hex(0x24282F))
        .clip()
        .child(div().h(relative(1.0)).w(relative(t)).rounded(px(2.0)).bg(Color::hex(0x3D8BFD)))
}

/// A scrollable container.
///
/// Scrolling adjusts the node's offset, which shifts its children's absolute
/// bounds without marking anything layout-dirty. A scroll that relaid out its
/// contents would make a long list unusable.
pub struct ScrollView {
    id: Option<ElementId>,
    children: Vec<AnyElement>,
    style: Style,
    horizontal: bool,
}

/// Creates a [`ScrollView`].
pub fn scroll_view() -> ScrollView {
    ScrollView { id: None, children: Vec::new(), style: Style::DEFAULT, horizontal: false }
}

impl ScrollView {
    /// Gives the view a stable identity, which it needs to keep its scroll
    /// offset across rebuilds.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Scrolls horizontally instead of vertically.
    pub fn horizontal(mut self, horizontal: bool) -> Self {
        self.horizontal = horizontal;
        self
    }
}

impl ParentElement for ScrollView {
    fn extend_children(&mut self, children: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(children);
    }
}

impl Styled for ScrollView {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }
    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        // A scroll view has no appearance of its own; wrap it in a `div` for a
        // background or a border.
        static_paint_style()
    }
}

/// A shared, never-read paint style for elements with no appearance.
fn static_paint_style() -> &'static mut PaintStyle {
    // A scroll view never paints, so nothing observes this. Using a leaked
    // allocation rather than a field keeps `ScrollView` free of a member that
    // would only ever hold defaults.
    use std::sync::OnceLock;
    static CELL: OnceLock<()> = OnceLock::new();
    CELL.get_or_init(|| ());
    Box::leak(Box::new(PaintStyle::default()))
}

impl Element for ScrollView {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        style.display = sphere_layout::Display::Flex;
        style.flex_direction = if self.horizontal {
            sphere_layout::FlexDirection::Row
        } else {
            sphere_layout::FlexDirection::Column
        };
        if self.horizontal {
            style.overflow_x = Overflow::Scroll;
        } else {
            style.overflow_y = Overflow::Scroll;
        }
        style
    }

    fn children(&mut self) -> &mut [AnyElement] {
        &mut self.children
    }

    fn take_children(&mut self) -> Vec<AnyElement> {
        core::mem::take(&mut self.children)
    }

    fn paint(&mut self, _cx: &mut PaintContext<'_, '_>) {}

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        // The tree owns the scroll offset; this only reports that the wheel was
        // consumed here so an ancestor scroll view does not also move.
        if let UiEvent::Scroll(_) = cx.event {
            cx.notify();
            return EventFlow::Stop;
        }
        EventFlow::Continue
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::role(Role::ScrollArea))
    }
}

/// Everything in this module, for a glob import.
pub mod prelude {
    pub use super::{
        Button, ButtonVariant, ScrollView, Toggle, ValueControl, ValueShape, button, checkbox,
        fader, knob, panel, progress, scroll_view, separator, slider, toggle,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{Interactive, IntoElement};
    use crate::event::{ElementState, Modifiers, MouseButtonEvent, MouseMoveEvent};
    use crate::tree::UiTree;
    use smallvec::SmallVec;
    use sphere_core::{ScaleFactor, size};
    use sphere_render::{Canvas, Scene};
    use std::cell::Cell;
    use std::rc::Rc;

    fn viewport() -> Size<Px> {
        size(px(400.0), px(300.0))
    }

    /// A text system with a real face, or `None` on a machine with no fonts.
    fn text_system() -> Option<sphere_text::TextSystem> {
        let mut system = sphere_text::TextSystem::with_system_fonts();
        system.fonts_mut().resolve(&sphere_text::FontRequest::default())?;
        Some(system)
    }

    #[test]
    fn a_button_is_wide_enough_for_its_label() {
        // A button paints its label as a leaf, so nothing but `measure` reports
        // its width. Without one it collapses to its 12 px padding either side
        // and the label spills over its neighbours -- which is what a row of
        // auto-width buttons in a header actually looked like.
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut tree = UiTree::new();
        tree.build(button("Apply").into_element());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();

        let node = tree.layout().roots()[0];
        let bounds = tree.layout().layout(node).unwrap().bounds;
        // 24 px is the horizontal padding alone: anything at or below it means
        // the label contributed nothing.
        assert!(bounds.width() > px(24.0), "button collapsed to {bounds:?}");
    }

    #[test]
    fn a_longer_button_label_makes_a_wider_button() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut width_of = |text: &str| {
            let mut tree = UiTree::new();
            tree.build(button(text).into_element());
            tree.compute_layout_with_text(viewport(), &mut system).unwrap();
            let node = tree.layout().roots()[0];
            tree.layout().layout(node).unwrap().bounds.width()
        };
        assert!(width_of("Apply") < width_of("Apply and close"));
    }

    #[test]
    fn an_empty_button_still_has_its_padding_and_height() {
        // An icon-only button is a real case; it must not measure to nothing.
        let mut system = sphere_text::TextSystem::new();
        let mut tree = UiTree::new();
        tree.build(button("").into_element());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();

        let node = tree.layout().roots()[0];
        let bounds = tree.layout().layout(node).unwrap().bounds;
        assert_eq!(bounds.height(), px(28.0), "default button height");
    }

    fn press_at(x: f32, y: f32, count: u8) -> UiEvent {
        UiEvent::MouseDown(MouseButtonEvent {
            position: Point::new(px(x), px(y)),
            button: MouseButton::Primary,
            state: ElementState::Pressed,
            click_count: count,
            modifiers: Modifiers::NONE,
        })
    }

    fn release_at(x: f32, y: f32) -> UiEvent {
        UiEvent::MouseUp(MouseButtonEvent {
            position: Point::new(px(x), px(y)),
            button: MouseButton::Primary,
            state: ElementState::Released,
            click_count: 1,
            modifiers: Modifiers::NONE,
        })
    }

    fn drag_to(x: f32, y: f32, shift: bool) -> UiEvent {
        UiEvent::MouseMove(MouseMoveEvent {
            position: Point::new(px(x), px(y)),
            delta: Size::default(),
            buttons: SmallVec::from_slice(&[MouseButton::Primary]),
            modifiers: Modifiers { shift, ..Modifiers::NONE },
        })
    }

    fn key(k: Key, shift: bool) -> UiEvent {
        UiEvent::Key(crate::event::KeyEvent {
            key: k,
            state: ElementState::Pressed,
            repeat: false,
            modifiers: Modifiers { shift, ..Modifiers::NONE },
        })
    }

    /// Builds a tree containing one widget filling the viewport.
    fn mount(widget: AnyElement) -> UiTree {
        let mut tree = UiTree::new();
        tree.build(div().w(relative(1.0)).h(relative(1.0)).child(widget).into_element());
        tree.compute_layout(viewport()).unwrap();
        tree
    }

    fn paint(tree: &mut UiTree) -> Scene {
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        let mut text = sphere_text::TextSystem::new();
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
        scene
    }

    #[test]
    fn a_button_fires_on_release_inside() {
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        let mut tree = mount(
            button("OK")
                .id("ok")
                .width(px(100.0))
                .on_press(move || h.set(h.get() + 1))
                .into_element(),
        );
        tree.dispatch(&press_at(50.0, 14.0, 1));
        tree.dispatch(&release_at(50.0, 14.0));
        assert_eq!(hits.get(), 1);
    }

    #[test]
    fn a_button_does_not_fire_when_released_outside() {
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        let mut tree = mount(
            button("OK")
                .id("ok")
                .width(px(100.0))
                .on_press(move || h.set(h.get() + 1))
                .into_element(),
        );
        tree.dispatch(&press_at(50.0, 14.0, 1));
        tree.dispatch(&release_at(350.0, 250.0));
        assert_eq!(hits.get(), 0, "dragging off a button must cancel the press");
    }

    #[test]
    fn a_button_activates_from_the_keyboard() {
        for activation in [Key::Enter, Key::Space] {
            let hits = Rc::new(Cell::new(0));
            let h = hits.clone();
            let mut tree =
                mount(button("OK").id("ok").on_press(move || h.set(h.get() + 1)).into_element());
            tree.navigate_focus(crate::focus::FocusDirection::Next);
            tree.dispatch(&key(activation.clone(), false));
            assert_eq!(hits.get(), 1, "{activation:?} did not activate the button");
        }
    }

    #[test]
    fn a_disabled_button_ignores_everything_and_leaves_the_tab_order() {
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        let mut tree = mount(
            button("OK")
                .id("ok")
                .disabled(true)
                .on_press(move || h.set(h.get() + 1))
                .into_element(),
        );
        tree.dispatch(&press_at(50.0, 14.0, 1));
        tree.dispatch(&release_at(50.0, 14.0));
        assert_eq!(hits.get(), 0);
        assert_eq!(tree.focus().len(), 0, "a disabled button must not be focusable");
    }

    #[test]
    fn a_toggle_reports_the_new_state_not_the_old() {
        let state = Rc::new(Cell::new(false));
        let s = state.clone();
        let mut tree = mount(toggle(false).id("t").on_change(move |v| s.set(v)).into_element());
        tree.dispatch(&press_at(10.0, 9.0, 1));
        tree.dispatch(&release_at(10.0, 9.0));
        assert!(state.get(), "the callback received the old value");
    }

    #[test]
    fn a_slider_jumps_to_where_it_was_clicked() {
        let value = Rc::new(Cell::new(0.0f32));
        let v = value.clone();
        let mut tree = mount(
            slider(0.0).id("s").range(0.0, 100.0).on_change(move |x| v.set(x)).into_element(),
        );
        // The slider fills the 400 px viewport; a click at x = 100 is a quarter.
        tree.dispatch(&press_at(100.0, 10.0, 1));
        assert!((value.get() - 25.0).abs() < 1.0, "expected about 25, got {}", value.get());
    }

    #[test]
    fn a_knob_does_not_jump_on_click() {
        // A jump would let a mis-click destroy a carefully set value.
        let value = Rc::new(Cell::new(0.5f32));
        let v = value.clone();
        let mut tree =
            mount(knob(0.5).id("k").range(0.0, 1.0).on_change(move |x| v.set(x)).into_element());
        tree.dispatch(&press_at(20.0, 5.0, 1));
        assert_eq!(value.get(), 0.5, "a knob must not jump to the click position");
    }

    #[test]
    fn dragging_a_knob_upward_increases_it() {
        let value = Rc::new(Cell::new(0.5f32));
        let v = value.clone();
        let mut tree =
            mount(knob(0.5).id("k").range(0.0, 1.0).on_change(move |x| v.set(x)).into_element());
        // The knob's box is 48x48 at the origin, so the press must land inside it.
        tree.dispatch(&press_at(24.0, 24.0, 1));
        // Up is a decreasing y. Half the travel over a unit range is +0.5.
        tree.dispatch(&drag_to(24.0, 24.0 - ValueControl::TRAVEL_PX * 0.5, false));
        assert!(value.get() > 0.5, "dragging up must increase: got {}", value.get());
        assert!((value.get() - 1.0).abs() < 0.01, "got {}", value.get());
    }

    #[test]
    fn shift_makes_a_drag_finer() {
        let coarse = Rc::new(Cell::new(0.5f32));
        let fine = Rc::new(Cell::new(0.5f32));
        for (target, shift) in [(coarse.clone(), false), (fine.clone(), true)] {
            let t = target.clone();
            let mut tree = mount(
                knob(0.5).id("k").range(0.0, 1.0).on_change(move |x| t.set(x)).into_element(),
            );
            tree.dispatch(&press_at(24.0, 24.0, 1));
            tree.dispatch(&drag_to(24.0, 4.0, shift));
        }
        let coarse_delta = (coarse.get() - 0.5).abs();
        let fine_delta = (fine.get() - 0.5).abs();
        assert!(fine_delta < coarse_delta, "shift did not reduce sensitivity");
        assert!(fine_delta > 0.0, "shift stopped the drag entirely");
    }

    #[test]
    fn a_drag_captures_the_pointer_so_it_survives_leaving_the_box() {
        // A 28 px-wide fader is left almost immediately; without capture the
        // value freezes there.
        let value = Rc::new(Cell::new(0.5f32));
        let v = value.clone();
        let mut tree =
            mount(fader(0.5).id("f").range(0.0, 1.0).on_change(move |x| v.set(x)).into_element());
        tree.dispatch(&press_at(14.0, 150.0, 1));
        tree.dispatch(&drag_to(390.0, 60.0, false));
        assert!(value.get() > 0.5, "the fader stopped tracking outside its box");
    }

    #[test]
    fn dragging_clamps_at_both_ends() {
        let value = Rc::new(Cell::new(0.5f32));
        let v = value.clone();
        let mut tree =
            mount(knob(0.5).id("k").range(0.0, 1.0).on_change(move |x| v.set(x)).into_element());
        tree.dispatch(&press_at(24.0, 24.0, 1));
        tree.dispatch(&drag_to(24.0, -100_000.0, false));
        assert_eq!(value.get(), 1.0);
        tree.dispatch(&drag_to(24.0, 100_000.0, false));
        assert_eq!(value.get(), 0.0);
    }

    #[test]
    fn double_click_resets_to_the_default() {
        let value = Rc::new(Cell::new(0.9f32));
        let v = value.clone();
        let mut tree = mount(
            knob(0.9)
                .id("k")
                .range(0.0, 1.0)
                .default_value(0.25)
                .on_change(move |x| v.set(x))
                .into_element(),
        );
        tree.dispatch(&press_at(24.0, 24.0, 2));
        assert!((value.get() - 0.25).abs() < 1e-5, "got {}", value.get());
    }

    #[test]
    fn a_step_quantises_relative_to_the_minimum() {
        // A range of 20..=20000 stepped by 10 must hit 20, 30, 40 — quantising
        // to absolute multiples of 10 would give 20, 30, but also allow 25.
        let control = slider(20.0).range(20.0, 20_000.0).step(10.0);
        assert!((control.quantize(24.0) - 20.0).abs() < 1e-3);
        assert!((control.quantize(26.0) - 30.0).abs() < 1e-3);
        assert!((control.quantize(25.0) - 30.0).abs() < 1e-3);
    }

    #[test]
    fn arrow_keys_step_the_value_and_shift_makes_them_finer() {
        let value = Rc::new(Cell::new(0.5f32));
        let v = value.clone();
        let mut tree =
            mount(slider(0.5).id("s").range(0.0, 1.0).on_change(move |x| v.set(x)).into_element());
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&key(Key::Right, false));
        let coarse = value.get();
        assert!((coarse - 0.51).abs() < 1e-4, "got {coarse}");

        value.set(0.5);
        tree.dispatch(&key(Key::Right, true));
        assert!((value.get() - 0.501).abs() < 1e-4, "shift step gave {}", value.get());
    }

    #[test]
    fn home_and_end_jump_to_the_extremes() {
        let value = Rc::new(Cell::new(0.5f32));
        let v = value.clone();
        let mut tree = mount(
            slider(0.5).id("s").range(-60.0, 12.0).on_change(move |x| v.set(x)).into_element(),
        );
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&key(Key::Home, false));
        assert_eq!(value.get(), -60.0);
        tree.dispatch(&key(Key::End, false));
        assert_eq!(value.get(), 12.0);
    }

    #[test]
    fn dragging_a_control_never_relayouts() {
        // The engine's headline invariant, at the widget level.
        let value = Rc::new(Cell::new(0.5f32));
        let v = value.clone();
        let build = |val: f32, v: Rc<Cell<f32>>| {
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(knob(val).id("k").range(0.0, 1.0).on_change(move |x| v.set(x)))
                .into_element()
        };
        let mut tree = UiTree::new();
        tree.build(build(value.get(), value.clone()));
        tree.compute_layout(viewport()).unwrap();

        tree.dispatch(&press_at(24.0, 24.0, 1));
        for i in 0..60 {
            tree.dispatch(&drag_to(24.0, 24.0 - i as f32, false));
            tree.build(build(value.get(), value.clone()));
            tree.compute_layout(viewport()).unwrap();
            assert_eq!(tree.stats().nodes_laid_out, 0, "a knob drag relaid out the tree");
        }
        let _ = v;
        assert!(value.get() > 0.5);
    }

    #[test]
    fn a_zero_size_control_does_not_divide_by_zero() {
        let mut tree = mount(knob(0.5).id("k").size(Px::ZERO, Px::ZERO).into_element());
        let scene = paint(&mut tree);
        for mesh in &scene.meshes {
            for v in &mesh.vertices {
                assert!(v.position[0].is_finite() && v.position[1].is_finite());
            }
        }
    }

    #[test]
    fn a_degenerate_range_does_not_divide_by_zero() {
        let control = knob(5.0).range(5.0, 5.0);
        assert!(control.normalized().is_finite());
        assert!(control.quantize(7.0).is_finite());
    }

    #[test]
    fn a_control_announces_its_formatted_value() {
        // A fader says "-6.0 dB", not "79 %".
        let control = fader(0.79)
            .name("Input")
            .range(0.0, 1.0)
            .format(|v| format!("{:.1} dB", (v - 1.0) * 30.0));
        let s = control.semantics().unwrap();
        assert_eq!(s.announced_value().as_deref(), Some("-6.3 dB"));
        assert_eq!(s.role, Role::Fader);
    }

    #[test]
    fn every_control_shape_reports_its_own_role() {
        assert_eq!(slider(0.0).semantics().unwrap().role, Role::Slider);
        assert_eq!(fader(0.0).semantics().unwrap().role, Role::Fader);
        assert_eq!(knob(0.0).semantics().unwrap().role, Role::Knob);
    }

    #[test]
    fn controls_paint_something_and_differ_between_min_and_max() {
        for shape in [ValueShape::HorizontalSlider, ValueShape::VerticalFader, ValueShape::Knob] {
            let make = |v: f32| match shape {
                ValueShape::HorizontalSlider => slider(v),
                ValueShape::VerticalFader => fader(v),
                ValueShape::Knob => knob(v),
            };
            let low = paint(&mut mount(make(0.0).id("c").range(0.0, 1.0).into_element()));
            let high = paint(&mut mount(make(1.0).id("c").range(0.0, 1.0).into_element()));
            assert!(!low.is_empty(), "{shape:?} painted nothing");
            assert_ne!(
                low.commands.len() + low.meshes.len(),
                usize::MAX,
                "placeholder to keep the comparison honest"
            );
            // The geometry must actually differ, or the value is not being drawn.
            let describe = |s: &Scene| {
                s.commands.len() * 1000 + s.meshes.iter().map(|m| m.vertices.len()).sum::<usize>()
            };
            assert_ne!(describe(&low), describe(&high), "{shape:?} looks identical at both ends");
        }
    }

    #[test]
    fn progress_clamps_out_of_range_input() {
        for f in [-1.0, 0.0, 0.5, 1.0, 5.0, f32::NAN] {
            let mut tree = mount(progress(f).into_element());
            let scene = paint(&mut tree);
            for q in &scene.commands {
                if let sphere_render::DrawCommand::Quad(q) = q {
                    assert!(q.bounds.width().get().is_finite(), "fraction {f} gave {:?}", q.bounds);
                    assert!(q.bounds.width() >= Px::ZERO);
                }
            }
        }
    }

    #[test]
    fn a_scroll_view_consumes_the_wheel_so_ancestors_do_not_also_move() {
        let outer_scrolls = Rc::new(Cell::new(0));
        let o = outer_scrolls.clone();
        let mut tree = UiTree::new();
        tree.build(
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .on_scroll(move |_| o.set(o.get() + 1))
                .child(
                    scroll_view()
                        .id("inner")
                        .w(relative(1.0))
                        .h(px(100.0))
                        .child(div().h(px(1000.0))),
                )
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();
        tree.dispatch(&UiEvent::Scroll(crate::event::ScrollEvent {
            position: Point::new(px(50.0), px(50.0)),
            delta: crate::event::ScrollDelta::Lines(Size::new(0.0, -3.0)),
            modifiers: Modifiers::NONE,
            momentum: false,
        }));
        assert_eq!(outer_scrolls.get(), 0, "the wheel reached an ancestor scroll view");
    }

    #[test]
    fn a_separator_is_one_logical_pixel_on_its_thin_axis() {
        let mut tree = mount(separator(false).into_element());
        let scene = paint(&mut tree);
        match &scene.commands[0] {
            sphere_render::DrawCommand::Quad(q) => assert_eq!(q.bounds.height(), px(1.0)),
            other => panic!("expected a quad, got {other:?}"),
        }
    }
}

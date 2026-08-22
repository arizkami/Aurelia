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
//! tool converged on vertical drag instead, and SphereKit does not deviate.

use crate::element::{AnyElement, Element, EventContext, PaintContext, ParentElement, Styled, div};
use crate::event::{EventFlow, Key, MouseButton, UiEvent};
use crate::semantics::{Role, Semantics, ValueRange};
use crate::style::{Cursor, FocusRing, PaintStyle};
use crate::text::label;
use spherekit_core::{Color, Corners, ElementId, Point, Px, Rect, RoundedRect, Size, px, relative};
use spherekit_layout::{Overflow, Style};

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
    css_paint: Option<PaintStyle>,
    disabled: bool,
    /// Overrides the theme's body size. For an icon button, whose glyph is
    /// designed at a size unrelated to the body text around it.
    text_size: Option<Px>,
    /// Overrides the default family, for an icon font.
    font: Option<spherekit_text::FontRequest>,
    /// Overrides the theme's body-text weight.
    weight: Option<spherekit_text::FontWeight>,
    on_press: Option<OnAction>,
}

/// Creates a [`Button`].
pub fn button(text: impl Into<String>) -> Button {
    Button {
        id: None,
        text: text.into(),
        variant: ButtonVariant::default(),
        style: Style::DEFAULT,
        css_paint: None,
        disabled: false,
        text_size: None,
        font: None,
        weight: None,
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
    pub fn width(mut self, width: impl Into<spherekit_core::Length>) -> Self {
        self.style.size.width = width.into();
        self
    }

    /// Sets the button's height, overriding the default.
    pub fn height(mut self, height: impl Into<spherekit_core::Length>) -> Self {
        self.style.size.height = height.into();
        self
    }

    /// Overrides the label's size.
    pub fn text_size(mut self, size: Px) -> Self {
        self.text_size = Some(size);
        self
    }

    /// Overrides the label's font weight.
    pub fn weight(mut self, weight: spherekit_text::FontWeight) -> Self {
        self.weight = Some(weight);
        self
    }

    /// Draws the label in a specific font family.
    ///
    /// For an icon button, where the label is a codepoint in an icon font
    /// rather than text. Falls back through the list in order, so a Windows 11
    /// icon font can name its Windows 10 predecessor after it.
    pub fn font(mut self, families: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.font = Some(spherekit_text::FontRequest {
            families: families.into_iter().map(Into::into).collect(),
            ..Default::default()
        });
        self
    }
}

impl Button {
    /// The style the label is measured *and* painted with.
    ///
    /// One function so the two can never drift apart. `measure` reserves the
    /// box and `paint` fills it; if they disagree on the font size the text
    /// ends up outside the box the layout engine agreed to.
    fn label_style(&self, theme: &crate::theme::Theme) -> spherekit_text::TextStyle {
        let mut font = self.font.clone().unwrap_or_default();
        font.weight = self.weight.unwrap_or(theme.typography.weight);
        spherekit_text::TextStyle {
            font_size: self.text_size.unwrap_or(theme.typography.md),
            font,
            wrap: spherekit_text::WrapMode::None,
            ..Default::default()
        }
    }

    /// Flat button chrome shared by every variant.
    ///
    /// Buttons communicate hierarchy through fill and label colour. They do
    /// not draw a resting border or a focus outline; hover and pressed fills
    /// remain the interaction affordance.
    fn paint_style(&self, theme: &crate::theme::Theme) -> PaintStyle {
        let c = theme.colors;
        let (base, hover, active) = match self.variant {
            ButtonVariant::Primary => (c.accent, c.accent_hover, c.accent_hover),
            ButtonVariant::Secondary => (c.elevated, c.hover, c.pressed),
            ButtonVariant::Ghost => (Color::TRANSPARENT, c.hover, c.pressed),
            ButtonVariant::Danger => (c.danger, c.danger, c.danger),
        };

        let mut style = PaintStyle {
            background: Some(base.into()),
            hover_background: Some(hover.into()),
            active_background: Some(active.into()),
            corner_radii: Corners::all(theme.radii.md),
            border_width: Px::ZERO,
            border_color: Color::TRANSPARENT,
            focus_ring: None,
            ..Default::default()
        };
        if self.disabled {
            style.opacity = 0.45;
            style.hover_background = None;
            style.active_background = None;
        }
        if let Some(custom) = &self.css_paint {
            if custom.background.is_some() {
                style.background = custom.background.clone();
            }
            if custom.border_width > Px::ZERO {
                style.border_width = custom.border_width;
                style.border_color = custom.border_color;
            }
            if !custom.corner_radii.is_zero() {
                style.corner_radii = custom.corner_radii;
            }
            style.opacity *= custom.opacity;
        }
        style
    }
}

impl Styled for Button {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        self.css_paint.get_or_insert_with(PaintStyle::default)
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
        _request: &spherekit_layout::MeasureRequest<'_>,
        text: &mut spherekit_text::TextSystem,
        theme: &crate::theme::Theme,
    ) -> Option<Size<Px>> {
        if self.text.is_empty() {
            return Some(Size::ZERO);
        }
        // Content size only: taffy adds the padding and border from
        // `layout_style` on top of whatever comes back from here.
        Some(text.layout(&self.text, &self.label_style(theme), None).size)
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if matches!(style.size.height, spherekit_core::Length::Auto) {
            style.size.height = spherekit_core::Length::Px(px(28.0));
        }
        style.display = spherekit_layout::Display::Flex;
        style.align_items = Some(spherekit_layout::Align::Center);
        style.justify_content = Some(spherekit_layout::Distribute::Center);
        style.padding = spherekit_layout::edges_symmetric(
            spherekit_core::Length::Px(px(4.0)),
            spherekit_core::Length::Px(px(12.0)),
        );
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let c = cx.theme.colors;
        let text = match self.variant {
            ButtonVariant::Primary | ButtonVariant::Danger => c.text_on_accent,
            ButtonVariant::Secondary | ButtonVariant::Ghost => c.text,
        };

        let style = self.paint_style(cx.theme);
        let mut state = cx.state;
        state.disabled = self.disabled;
        style.paint_box(cx.canvas, cx.bounds, state);
        // A button inside a custom title bar has to keep its clicks. Off the
        // caption this costs one push and changes nothing.
        cx.keep_interactive();

        if self.text.is_empty() {
            return;
        }
        let color = if self.disabled { c.text_muted } else { text };
        let style_for_text = self.label_style(cx.theme);
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
            scroll: cx.scroll,
            theme: cx.theme,
            time: cx.time,
            // Forwarded rather than dropped: a nested paint context must be able
            // to reach the same slot, or a widget that wraps an editable one
            // would silently swallow its request for an input method.
            ime: cx.ime,
            caption_exclusions: cx.caption_exclusions,
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
            size: spherekit_core::Size {
                width: spherekit_core::Length::Px(px(w)),
                height: spherekit_core::Length::Px(px(h)),
            },
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
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
                let b = bounds.inset(spherekit_core::Edges::all(px(4.0)));
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
    css_paint: Option<PaintStyle>,
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
            css_paint: None,
            on_change: None,
        }
    }

    /// How far a full-travel knob drag is, in logical pixels.
    ///
    /// Sliders and faders use their actual track length; a knob has no linear
    /// track, so it keeps a comfortable gesture distance instead.
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
        self.style.size = spherekit_core::Size {
            width: spherekit_core::Length::Px(width),
            height: spherekit_core::Length::Px(height),
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

impl Styled for ValueControl {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        self.css_paint.get_or_insert_with(PaintStyle::default)
    }
}

impl Element for ValueControl {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        let (w, h) = match self.shape {
            ValueShape::HorizontalSlider => (relative(1.0), spherekit_core::Length::Px(px(20.0))),
            ValueShape::VerticalFader => (spherekit_core::Length::Px(px(28.0)), relative(1.0)),
            ValueShape::Knob => {
                (spherekit_core::Length::Px(px(48.0)), spherekit_core::Length::Px(px(48.0)))
            }
        };
        if matches!(style.size.width, spherekit_core::Length::Auto) {
            style.size.width = w;
        }
        if matches!(style.size.height, spherekit_core::Length::Auto) {
            style.size.height = h;
        }
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
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
                    cap.intersection(b.outset(spherekit_core::Edges::all(cap_h))),
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
                let mut arc = spherekit_core::PathBuilder::new();
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
                        spherekit_core::Stroke::new(width).with_cap(spherekit_core::LineCap::Round),
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
                let travel_px = match self.shape {
                    // Pointer positions and bounds are both logical pixels.
                    // Using the control's actual travel keeps a 10 px drag on
                    // a wide slider small instead of scaling it against the
                    // knob's fixed 180 px gesture distance.
                    ValueShape::HorizontalSlider => cx.bounds.width().get(),
                    ValueShape::VerticalFader => cx.bounds.height().get(),
                    ValueShape::Knob => Self::TRAVEL_PX,
                }
                .max(1.0);
                let delta = travel / travel_px * self.span() * sensitivity;
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

/// How long one sweep of an indeterminate bar takes.
const INDETERMINATE_PERIOD: f32 = 1.6;
/// How much of the track the indeterminate shuttle covers.
const INDETERMINATE_SPAN: f32 = 0.35;
/// Default thickness of a progress bar.
const PROGRESS_THICKNESS: f32 = 4.0;

/// A progress bar, determinate or not.
///
/// The distinction is `Option<f32>` rather than a flag, because "how far along"
/// genuinely has no answer while a task has not reported one. A bar that
/// pretends to be at zero is a worse lie than one that says it does not know.
pub struct Progress {
    id: Option<ElementId>,
    /// `None` means indeterminate.
    value: Option<f32>,
    style: Style,
    paint: PaintStyle,
    thickness: Px,
    track: Option<Color>,
    fill: Option<Color>,
}

/// A determinate progress bar, `0..=1`.
///
/// A NaN fraction reads as zero rather than propagating: `done / total` with a
/// total of zero is a real thing to write, and it should show an empty bar
/// rather than a rectangle of NaN width.
pub fn progress(fraction: f32) -> Progress {
    Progress {
        id: None,
        value: Some(if fraction.is_nan() { 0.0 } else { fraction.clamp(0.0, 1.0) }),
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
        thickness: px(PROGRESS_THICKNESS),
        track: None,
        fill: None,
    }
}

/// A progress bar for work whose extent is not known.
///
/// A shuttle sweeps the track on a fixed loop. It is driven by paint time, so
/// the window has to be redrawing for it to move — ask the scheduler for
/// [`RedrawPolicy::Animating`](spherekit_platform::RedrawPolicy::Animating)
/// while one is on screen, or it will sit still and look broken.
pub fn progress_indeterminate() -> Progress {
    Progress { value: None, ..progress(0.0) }
}

impl Progress {
    /// Gives the bar a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// How thick the bar is. Defaults to 4 px.
    pub fn thickness(mut self, thickness: Px) -> Self {
        self.thickness = thickness;
        self
    }

    /// Overrides the track colour. Defaults to the theme's elevated surface.
    pub fn track_color(mut self, color: Color) -> Self {
        self.track = Some(color);
        self
    }

    /// Overrides the fill colour. Defaults to the theme's accent.
    pub fn fill_color(mut self, color: Color) -> Self {
        self.fill = Some(color);
        self
    }

    /// Whether this bar animates and therefore needs frames.
    #[inline]
    pub fn is_indeterminate(&self) -> bool {
        self.value.is_none()
    }

    /// The shuttle's span on the track at `time`, as `(start, end)` in `0..=1`.
    ///
    /// Public so a caller can test the motion without a canvas, and separate
    /// from painting for the same reason a scrollbar's geometry is: the shape
    /// is the part worth being sure about.
    pub fn shuttle(time: f32) -> (f32, f32) {
        let phase = (time / INDETERMINATE_PERIOD).rem_euclid(1.0);
        // Eased rather than linear so the shuttle slows at both ends instead of
        // hitting the edges at full speed and snapping back.
        let eased = spherekit_core::animate::Curve::EaseInOutCubic.eval(phase);
        // Travels from fully off the left to fully off the right, so the bar is
        // never briefly empty at the turn.
        let head = eased * (1.0 + INDETERMINATE_SPAN);
        ((head - INDETERMINATE_SPAN).max(0.0), head.min(1.0))
    }
}

impl Styled for Progress {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }
    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        // A progress bar draws itself from the theme; `track_color` and
        // `fill_color` are the knobs, not the generic paint style. Kept as a
        // real field so a stray `.bg()` writes somewhere harmless rather than
        // leaking an allocation per call.
        &mut self.paint
    }
}

impl Element for Progress {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if matches!(style.size.height, spherekit_core::Length::Auto) {
            style.size.height = spherekit_core::Length::Px(self.thickness);
        }
        if matches!(style.size.width, spherekit_core::Length::Auto) {
            style.size.width = spherekit_core::Length::Fraction(1.0);
        }
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let bounds = cx.bounds;
        if bounds.is_empty() {
            return;
        }
        let c = cx.theme.colors;
        let radius = Px(bounds.height().get() * 0.5);
        let track = self.track.unwrap_or(c.elevated);
        let fill = self.fill.unwrap_or(c.accent);

        cx.canvas.fill_rounded_rect(RoundedRect::new(bounds, Corners::all(radius)), track);

        let (start, end) = match self.value {
            Some(v) => (0.0, v),
            None => Self::shuttle(cx.time),
        };
        let span = end - start;
        if span <= 0.0 {
            return;
        }
        let x0 = bounds.min_x() + bounds.width() * start;
        let filled = spherekit_core::Rect::new(
            Point::new(x0, bounds.min_y()),
            Size::new(bounds.width() * span, bounds.height()),
        );
        cx.canvas.fill_rounded_rect(RoundedRect::new(filled, Corners::all(radius)), fill);
    }

    fn semantics(&self) -> Option<Semantics> {
        let s = Semantics::role(Role::Progress);
        match self.value {
            // An indeterminate bar deliberately reports no value: a screen
            // reader saying "0 percent" would be stating something false.
            None => Some(s),
            Some(v) => Some(s.value(ValueRange { value: v, min: 0.0, max: 1.0, step: None })),
        }
    }
}

// ---------------------------------------------------------------------------
// Scrolling
// ---------------------------------------------------------------------------

/// When a scroll container shows its scrollbars.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ScrollbarPolicy {
    /// Shown only while the axis actually overflows. The default, and the only
    /// one that is right for content whose length is not known in advance.
    #[default]
    Auto,
    /// Always shown, even with nothing to scroll. For a pane whose width must
    /// not change when its content grows past the fold.
    Always,
    /// Never shown. The wheel still works — this hides the indicator, it does
    /// not disable scrolling.
    Never,
}

/// Width of a scrollbar's track, in logical pixels.
const SCROLLBAR_TRACK: f32 = 10.0;
/// Width of the thumb inside that track while idle.
const SCROLLBAR_THUMB: f32 = 4.0;
/// Width of the thumb while the pointer is over the container or dragging.
const SCROLLBAR_THUMB_ACTIVE: f32 = 8.0;
/// How far the track is held off the container's edge.
const SCROLLBAR_EDGE: f32 = 3.0;
/// The shortest a thumb may get, however long the content is.
///
/// Without a floor, a thumb over a hundred thousand rows becomes a single
/// pixel that cannot be grabbed. Every shell clamps this.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// Scratch slot holding the offset a scrollbar drag started from.
const SCRATCH_SCROLL_ORIGIN: usize = 0;
/// Scratch slot holding the pointer coordinate a scrollbar drag started from.
const SCRATCH_SCROLL_POINTER: usize = 1;
/// Scratch slot holding which axis is being dragged. 0 none, 1 vertical, 2 horizontal.
const SCRATCH_SCROLL_AXIS: usize = 2;

/// The geometry of one scrollbar, derived from what the container is showing.
///
/// Split out from painting so the same arithmetic answers both "where do I draw
/// the thumb" and "what offset does a pointer at this position mean". Those two
/// disagreeing is the classic scrollbar bug — the thumb that jumps when you
/// grab it — and one function is how it stays impossible.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Scrollbar {
    /// The full track, along the container's inside edge.
    pub track: Rect<Px>,
    /// The thumb within that track.
    pub thumb: Rect<Px>,
    /// Whether this bar runs top-to-bottom.
    pub vertical: bool,
}

impl Scrollbar {
    /// Computes the bar for one axis, or `None` when there is nothing to show.
    ///
    /// `bounds` is the container's border box; the bar is laid inside it, which
    /// is what makes it an overlay that costs the content no width.
    pub fn for_axis(
        bounds: Rect<Px>,
        metrics: crate::element::ScrollMetrics,
        vertical: bool,
        policy: ScrollbarPolicy,
    ) -> Option<Self> {
        if policy == ScrollbarPolicy::Never {
            return None;
        }
        if policy == ScrollbarPolicy::Auto && !metrics.scrollable(vertical) {
            return None;
        }
        if bounds.is_empty() {
            return None;
        }

        // Held off the edge so the thumb clears a rounded corner and a border
        // rather than being drawn across them.
        let edge = Px(SCROLLBAR_EDGE);
        let track_w = Px(SCROLLBAR_TRACK);
        let track = if vertical {
            Rect::new(
                Point::new(bounds.max_x() - track_w - edge, bounds.min_y() + edge),
                Size::new(track_w, bounds.height() - edge * 2.0),
            )
        } else {
            Rect::new(
                Point::new(bounds.min_x() + edge, bounds.max_y() - track_w - edge),
                Size::new(bounds.width() - edge * 2.0, track_w),
            )
        };
        if track.is_empty() {
            return None;
        }

        let extent = if vertical { track.height().get() } else { track.width().get() };
        if extent <= 0.0 {
            return None;
        }
        let thumb_extent =
            (extent * metrics.visible_fraction(vertical)).max(SCROLLBAR_MIN_THUMB).min(extent);
        // The travel is what is left of the track once the thumb has taken its
        // share — not the whole track. Using the whole track is why a naive
        // scrollbar runs past its end.
        let travel = (extent - thumb_extent).max(0.0);
        let start = travel * metrics.progress(vertical);

        let thumb = if vertical {
            Rect::new(
                Point::new(track.min_x(), track.min_y() + Px(start)),
                Size::new(track_w, Px(thumb_extent)),
            )
        } else {
            Rect::new(
                Point::new(track.min_x() + Px(start), track.min_y()),
                Size::new(Px(thumb_extent), track_w),
            )
        };
        Some(Self { track, thumb, vertical })
    }

    /// The offset a thumb dragged by `delta` pixels along the track means.
    ///
    /// `from` is the offset the drag started at. Returns the new offset on this
    /// axis, unclamped — the layout tree clamps, and doing it twice would make
    /// a drag that overshoots stick instead of resuming.
    pub fn offset_for_drag(
        &self,
        metrics: crate::element::ScrollMetrics,
        from: Px,
        delta: Px,
    ) -> Px {
        let extent =
            if self.vertical { self.track.height().get() } else { self.track.width().get() };
        let thumb =
            if self.vertical { self.thumb.height().get() } else { self.thumb.width().get() };
        let travel = (extent - thumb).max(0.0);
        if travel <= 0.0 {
            return from;
        }
        let max = if self.vertical {
            metrics.max_offset().height.get()
        } else {
            metrics.max_offset().width.get()
        };
        // One pixel of thumb travel is `max / travel` pixels of content.
        from + Px(delta.get() * (max / travel))
    }

    /// Draws the bar. The track stays subtle; the thumb carries the state.
    pub fn paint(&self, cx: &mut PaintContext<'_, '_>, active: bool) {
        let c = cx.theme.colors;
        let width = if active { SCROLLBAR_THUMB_ACTIVE } else { SCROLLBAR_THUMB };
        let inset = (SCROLLBAR_TRACK - width) * 0.5;
        let thumb = if self.vertical {
            Rect::new(
                Point::new(self.thumb.min_x() + Px(inset), self.thumb.min_y()),
                Size::new(Px(width), self.thumb.height()),
            )
        } else {
            Rect::new(
                Point::new(self.thumb.min_x(), self.thumb.min_y() + Px(inset)),
                Size::new(self.thumb.width(), Px(width)),
            )
        };
        let radius = Px(width * 0.5);
        cx.canvas.fill_rounded_rect(
            RoundedRect::new(thumb, Corners::all(radius)),
            if active { c.border_strong } else { c.border },
        );
    }
}

/// A scrollable container.
///
/// Scrolling adjusts the node's offset, which shifts its children's absolute
/// bounds without marking anything layout-dirty. A scroll that relaid out its
/// contents would make a long list unusable.
///
/// The wheel is handled by the tree for *any* node with `Overflow::Scroll`, so
/// this widget's own job is narrower than it looks: it declares the overflow,
/// draws the overlay scrollbars, and turns a drag on a thumb into an offset.
///
/// Scrollbars are drawn **over** the content rather than beside it, so turning
/// them on never changes what the content is laid out into. That is why
/// [`ScrollbarPolicy::Always`] costs nothing but ink.
///
/// # The one thing that will catch you
///
/// A scroll view inside a flex parent needs `min_h(px(0.0))` — `min_w` for a
/// horizontal one — on **itself and every flex ancestor between it and the
/// fixed-size box**:
///
/// ```ignore
/// div().flex_col().h(relative(1.0))
///     .child(header)
///     .child(scroll_view().flex_1().min_h(px(0.0)).child(page))
/// ```
///
/// A flex item's automatic minimum size is its content, and an item cannot be
/// shrunk below that. So without the floor the *ancestor* silently grows to fit
/// the whole page instead of staying window-height; the scroll view then has
/// room for all of its content, reports no overflow, and neither the wheel nor
/// a scrollbar does anything. The content is simply cut off by the window edge,
/// which looks exactly like a clipping bug and is not one.
///
/// This is the same rule as CSS's `min-height: 0` on a scrolling flex child,
/// and it catches everyone once.
pub struct ScrollView {
    id: Option<ElementId>,
    children: Vec<AnyElement>,
    style: Style,
    paint: PaintStyle,
    horizontal: bool,
    both: bool,
    framed: bool,
    policy: ScrollbarPolicy,
}

/// Creates a [`ScrollView`].
pub fn scroll_view() -> ScrollView {
    ScrollView {
        id: None,
        children: Vec::new(),
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
        horizontal: false,
        both: false,
        framed: false,
        policy: ScrollbarPolicy::default(),
    }
}

impl ScrollView {
    /// Gives the view a stable identity, which it needs to keep its scroll
    /// offset across rebuilds.
    ///
    /// Worth spelling out: without an id the view gets a positional identity,
    /// and inserting a sibling above it silently hands its scroll position to
    /// something else.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Scrolls horizontally instead of vertically.
    pub fn horizontal(mut self, horizontal: bool) -> Self {
        self.horizontal = horizontal;
        self
    }

    /// Scrolls on both axes.
    pub fn both_axes(mut self, both: bool) -> Self {
        self.both = both;
        self
    }

    /// When the scrollbars are shown.
    pub fn scrollbars(mut self, policy: ScrollbarPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The bars this view would draw, given what it is currently showing.
    fn bars(
        &self,
        bounds: Rect<Px>,
        metrics: crate::element::ScrollMetrics,
    ) -> (Option<Scrollbar>, Option<Scrollbar>) {
        let vertical = (!self.horizontal || self.both)
            .then(|| Scrollbar::for_axis(bounds, metrics, true, self.policy))
            .flatten();
        let horizontal = (self.horizontal || self.both)
            .then(|| Scrollbar::for_axis(bounds, metrics, false, self.policy))
            .flatten();
        (vertical, horizontal)
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
        &mut self.paint
    }
}

impl Element for ScrollView {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        style.display = spherekit_layout::Display::Flex;
        style.flex_direction = if self.horizontal {
            spherekit_layout::FlexDirection::Row
        } else {
            spherekit_layout::FlexDirection::Column
        };
        if self.horizontal || self.both {
            style.overflow_x = Overflow::Scroll;
        }
        if !self.horizontal || self.both {
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

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let mut style = self.paint.clone();
        if self.framed && style.background.is_none() {
            style.background = Some(cx.theme.colors.surface.into());
            style.border_width = px(1.0);
            style.border_color = cx.theme.colors.border;
            style.corner_radii = Corners::all(cx.theme.radii.md);
        }
        style.paint_box(cx.canvas, cx.bounds, cx.state);
    }

    fn paints_over(&self) -> bool {
        self.policy != ScrollbarPolicy::Never
    }

    /// Painted after the children so the bars overlay the content.
    ///
    /// A separate pass because `paint` runs *before* the tree walks into the
    /// subtree — a bar drawn there would sit under every row it is meant to
    /// float above.
    fn paint_over(&mut self, cx: &mut PaintContext<'_, '_>) {
        let dragging = cx.scratch[SCRATCH_SCROLL_AXIS] != 0.0;
        let active = cx.state.hovered || dragging;
        let (vertical, horizontal) = self.bars(cx.bounds, cx.scroll);
        if let Some(bar) = vertical {
            bar.paint(cx, active);
        }
        if let Some(bar) = horizontal {
            bar.paint(cx, active);
        }
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        let (vertical, horizontal) = self.bars(cx.bounds, cx.scroll);

        match cx.event {
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                // Vertical first: where the two tracks meet in the corner, the
                // vertical bar is the one a shell gives the press to.
                for (bar, axis) in [(vertical, 1.0f32), (horizontal, 2.0f32)] {
                    let Some(bar) = bar else { continue };
                    if !bar.track.contains(e.position) {
                        continue;
                    }
                    let along = if bar.vertical { e.position.y } else { e.position.x };
                    let from =
                        if bar.vertical { cx.scroll.offset.height } else { cx.scroll.offset.width };

                    if bar.thumb.contains(e.position) {
                        // Grab: remember where, so the thumb does not jump.
                        cx.scratch[SCRATCH_SCROLL_ORIGIN] = from.get();
                        cx.scratch[SCRATCH_SCROLL_POINTER] = along.get();
                    } else {
                        // A press on the track jumps the thumb to the pointer
                        // and then tracks it, which is what makes a long list
                        // navigable without a drag at all.
                        let extent = if bar.vertical {
                            bar.track.height().get()
                        } else {
                            bar.track.width().get()
                        };
                        let thumb = if bar.vertical {
                            bar.thumb.height().get()
                        } else {
                            bar.thumb.width().get()
                        };
                        let origin =
                            if bar.vertical { bar.track.min_y() } else { bar.track.min_x() };
                        let travel = (extent - thumb).max(0.0);
                        let want = ((along.get() - origin.get() - thumb * 0.5) / travel.max(1.0))
                            .clamp(0.0, 1.0);
                        let max = if bar.vertical {
                            cx.scroll.max_offset().height
                        } else {
                            cx.scroll.max_offset().width
                        };
                        let jumped = Px(max.get() * want);
                        cx.scratch[SCRATCH_SCROLL_ORIGIN] = jumped.get();
                        cx.scratch[SCRATCH_SCROLL_POINTER] = along.get();
                        cx.scroll_to = Some(axis_offset(cx.scroll.offset, bar.vertical, jumped));
                    }
                    cx.scratch[SCRATCH_SCROLL_AXIS] = axis;
                    cx.capture();
                    cx.notify();
                    return EventFlow::Stop;
                }
                EventFlow::Continue
            }

            UiEvent::MouseMove(e) if cx.scratch[SCRATCH_SCROLL_AXIS] != 0.0 => {
                let is_vertical = cx.scratch[SCRATCH_SCROLL_AXIS] == 1.0;
                let bar = if is_vertical { vertical } else { horizontal };
                let Some(bar) = bar else { return EventFlow::Continue };
                let along = if is_vertical { e.position.y } else { e.position.x };
                let delta = along - Px(cx.scratch[SCRATCH_SCROLL_POINTER]);
                let from = Px(cx.scratch[SCRATCH_SCROLL_ORIGIN]);
                let next = bar.offset_for_drag(cx.scroll, from, delta);
                cx.scroll_to = Some(axis_offset(cx.scroll.offset, is_vertical, next));
                cx.notify();
                EventFlow::Stop
            }

            UiEvent::MouseUp(_) if cx.scratch[SCRATCH_SCROLL_AXIS] != 0.0 => {
                cx.scratch[SCRATCH_SCROLL_AXIS] = 0.0;
                cx.release();
                cx.notify();
                EventFlow::Stop
            }

            // The wheel is the tree's job — it owns scroll chaining, and it has
            // to work for a plain `div().overflow_y_scroll()` too. Reporting it
            // as handled here would only stop an ancestor from ever seeing it.
            _ => EventFlow::Continue,
        }
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::role(Role::ScrollArea))
    }
}

/// Replaces one axis of an offset, leaving the other alone.
fn axis_offset(offset: Size<Px>, vertical: bool, value: Px) -> Size<Px> {
    if vertical { Size::new(offset.width, value) } else { Size::new(value, offset.height) }
}

/// A framed scrolling pane: [`scroll_view`] with the theme's surface, border
/// and radius already on it.
///
/// The shape most callers actually want, and the one that is easy to get
/// subtly wrong by hand — the frame must not scroll with the content, and the
/// bars must sit inside it rather than over the border.
pub fn scroll_area() -> ScrollView {
    ScrollView { framed: true, ..scroll_view() }
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

/// One row in a menu: a label, an optional shortcut, and an action.
///
/// Themed at paint time rather than at build time, so a menu row does not need
/// the caller to thread colours through it — the same reason [`Button`] does
/// not.
pub struct MenuItem {
    id: Option<ElementId>,
    text: String,
    shortcut: Option<String>,
    danger: bool,
    disabled: bool,
    on_select: Option<OnAction>,
}

/// Creates a [`MenuItem`].
pub fn menu_item(text: impl Into<String>) -> MenuItem {
    MenuItem {
        id: None,
        text: text.into(),
        shortcut: None,
        danger: false,
        disabled: false,
        on_select: None,
    }
}

impl MenuItem {
    /// Gives the row a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// The accelerator shown right-aligned, such as `Ctrl+C`.
    ///
    /// Display only. The row does not bind the key — the field or the window
    /// that owns the shortcut does, and duplicating it here would let the two
    /// drift apart.
    pub fn shortcut(mut self, shortcut: impl Into<String>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// Draws the row as destructive.
    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    /// Greys the row out and takes it out of the tab order.
    ///
    /// Worth using rather than hiding the row: a Paste that is *there but
    /// unavailable* tells the reader the clipboard is empty, and a Paste that
    /// vanishes just makes the menu move.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Runs when the row is chosen, by click or by Space or Enter.
    pub fn on_select(mut self, f: impl FnMut() + 'static) -> Self {
        self.on_select = Some(Box::new(f));
        self
    }
}

impl Element for MenuItem {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        Style {
            size: spherekit_core::Size {
                width: spherekit_core::Length::Fraction(1.0),
                height: spherekit_core::Length::Px(px(28.0)),
            },
            flex_shrink: 0.0,
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
        let c = cx.theme.colors;
        let mut state = cx.state;
        state.disabled = self.disabled;

        let ink = match (self.disabled, self.danger) {
            (true, _) => c.text_muted,
            (false, true) => c.danger,
            (false, false) => c.text,
        };
        let hover = if self.danger { c.danger.with_alpha(0.16) } else { c.hover };
        let style = PaintStyle {
            hover_background: (!self.disabled).then(|| hover.into()),
            active_background: (!self.disabled).then(|| c.pressed.into()),
            corner_radii: Corners::all(cx.theme.radii.sm),
            focus_ring: Some(FocusRing { color: c.focus, ..FocusRing::default() }),
            opacity: if self.disabled { 0.55 } else { 1.0 },
            ..Default::default()
        };
        style.paint_box(cx.canvas, cx.bounds, state);

        let pad = cx.theme.spacing.sm;
        let inner = cx.bounds.inset(spherekit_core::Edges {
            top: Px::ZERO,
            right: pad,
            bottom: Px::ZERO,
            left: pad,
        });
        let text_style = spherekit_text::TextStyle {
            font_size: cx.theme.typography.sm,
            font: spherekit_text::FontRequest {
                weight: cx.theme.typography.weight,
                ..Default::default()
            },
            wrap: spherekit_text::WrapMode::None,
            ..Default::default()
        };

        let layout = cx.text.layout(&self.text, &text_style, None);
        let baseline = inner.min_y() + Px((inner.height().get() - layout.size.height.get()) * 0.5);
        crate::text::draw_layout(
            cx.canvas,
            &layout,
            Point::new(inner.min_x(), baseline),
            ink,
            spherekit_render::TextRasterMode::Auto,
            (Px::ZERO, Color::TRANSPARENT),
            spherekit_render::coverage_contrast_for(ink, c.surface),
        );

        if let Some(shortcut) = self.shortcut.as_deref() {
            let s = cx.text.layout(shortcut, &text_style, None);
            let x = inner.max_x() - s.size.width;
            let y = inner.min_y() + Px((inner.height().get() - s.size.height.get()) * 0.5);
            crate::text::draw_layout(
                cx.canvas,
                &s,
                Point::new(x, y),
                c.text_muted,
                spherekit_render::TextRasterMode::Auto,
                (Px::ZERO, Color::TRANSPARENT),
                spherekit_render::coverage_contrast_for(c.text_muted, c.surface),
            );
        }
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.disabled {
            return EventFlow::Continue;
        }
        cx.set_cursor(Cursor::Pointer);
        let chose = match cx.event {
            UiEvent::MouseUp(e) => {
                e.button == MouseButton::Primary && cx.bounds.contains(e.position)
            }
            UiEvent::Key(k) if k.state.is_pressed() => {
                matches!(k.key, Key::Space | Key::Enter)
            }
            _ => false,
        };
        if chose {
            if let Some(f) = self.on_select.as_mut() {
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
            Semantics::new(Role::MenuItem, self.text.clone())
                .disabled(self.disabled)
                .with_implied_actions(),
        )
    }
}

/// A menu that opens at a point rather than against an anchor.
///
/// The difference from [`Dropdown`] is where it goes: a dropdown belongs to the
/// control it hangs off, a context menu belongs to wherever the pointer was.
/// Give it a position in the coordinates of **its parent**, which for a menu
/// added to the root of a window is the window itself.
///
/// Like [`Dropdown`] it takes an open amount in `0..=1` and leaves layout
/// entirely at zero, so a closed menu cannot swallow a click.
///
/// ```ignore
/// // In the root element, so `at` is in window coordinates:
/// .child(context_menu(open, at)
///     .child(menu_item("Cut").shortcut("Ctrl+X").on_select(..))
///     .child(menu_item("Copy").shortcut("Ctrl+C").on_select(..)))
/// ```
pub struct ContextMenu {
    id: Option<ElementId>,
    children: Vec<AnyElement>,
    style: Style,
    paint: PaintStyle,
    open: f32,
    at: Point<Px>,
    rise: Px,
}

/// Creates a [`ContextMenu`] at a point in its parent's coordinates.
pub fn context_menu(open: f32, at: Point<Px>) -> ContextMenu {
    let mut style = Style::DEFAULT;
    style.flex_direction = spherekit_layout::FlexDirection::Column;
    // On top by default, and by a wide margin. Siblings are painted in
    // z-index order, so a menu left at the default zero paints *under* any
    // pane that raised itself — and a translucent pane over it does not hide
    // it outright, it just washes it out, which reads as a rendering fault
    // rather than as a stacking one. There is no case where a context menu
    // wants to be behind the thing it was opened over.
    style.z_index = CONTEXT_MENU_Z;
    ContextMenu {
        id: None,
        children: Vec::new(),
        style,
        paint: PaintStyle::default(),
        open,
        at,
        rise: px(6.0),
    }
}

impl ContextMenu {
    /// Gives the menu a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// How far the menu travels as it opens. Defaults to 6 px.
    pub fn rise(mut self, rise: Px) -> Self {
        self.rise = rise;
        self
    }

    fn eased(&self) -> f32 {
        ease_out_cubic(self.open.clamp(0.0, 1.0))
    }
}

impl ParentElement for ContextMenu {
    fn extend_children(&mut self, children: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(children);
    }
}

impl Styled for ContextMenu {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }
    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for ContextMenu {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if self.open.clamp(0.0, 1.0) <= DROPDOWN_CLOSED {
            style.display = spherekit_layout::Display::None;
            return style;
        }
        style.position = spherekit_layout::Position::Absolute;
        let drop = self.rise * (1.0 - self.eased());
        style.inset.left = spherekit_core::Length::Px(self.at.x);
        style.inset.top = spherekit_core::Length::Px(self.at.y + drop);
        style
    }

    fn children(&mut self) -> &mut [AnyElement] {
        &mut self.children
    }

    fn take_children(&mut self) -> Vec<AnyElement> {
        core::mem::take(&mut self.children)
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let mut style = self.paint.clone();
        if style.background.is_none() {
            style.background = Some(cx.theme.colors.surface.into());
            style.border_width = px(1.0);
            style.border_color = cx.theme.colors.border;
            style.corner_radii = Corners::all(cx.theme.radii.lg);
            style.shadows.push(cx.theme.shadows.md);
        }
        style.paint_box(cx.canvas, cx.bounds, cx.state);
    }

    fn paint_opacity(&self) -> f32 {
        (self.eased() * self.paint.opacity).clamp(0.0, 1.0)
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::new(Role::Menu, String::new()))
    }
}

// ---------------------------------------------------------------------------
// Avatar
// ---------------------------------------------------------------------------

/// Whether a person is available, drawn as a dot on their [`Avatar`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Available.
    Online,
    /// Idle, or away from the keyboard.
    Away,
    /// Busy, or in do-not-disturb.
    Busy,
    /// Signed out.
    Offline,
}

impl Presence {
    /// The dot colour for this state, from the theme's status palette.
    pub fn color(self, theme: &crate::theme::Theme) -> Color {
        match self {
            Presence::Online => theme.colors.success,
            Presence::Away => theme.colors.warning,
            Presence::Busy => theme.colors.danger,
            Presence::Offline => theme.colors.text_muted,
        }
    }
}

/// The tints an [`Avatar`] picks from when the caller does not name one.
///
/// A fixed set rather than a hue computed from the hash: eight colours a
/// designer signed off on beat a continuous ramp that will eventually land on
/// something that clashes with the accent, and two names that hash close
/// together get visibly different colours instead of two shades of one.
const AVATAR_TINTS: [Color; 8] = [
    Color::hex(0x4C6FEF),
    Color::hex(0x8B5CF6),
    Color::hex(0xD9488A),
    Color::hex(0xE06C3B),
    Color::hex(0xC9A227),
    Color::hex(0x3FA372),
    Color::hex(0x2E9BB5),
    Color::hex(0x6470A8),
];

/// A circular portrait: initials on a tint, with optional presence.
///
/// The tint is derived from the name, so the same person is the same colour in
/// every window of every session without anyone storing a preference.
pub struct Avatar {
    id: Option<ElementId>,
    name: String,
    initials: Option<String>,
    diameter: Px,
    color: Option<Color>,
    presence: Option<Presence>,
    ring: Option<Color>,
}

/// Creates an [`Avatar`] for a display name.
pub fn avatar(name: impl Into<String>) -> Avatar {
    Avatar {
        id: None,
        name: name.into(),
        initials: None,
        diameter: px(32.0),
        color: None,
        presence: None,
        ring: None,
    }
}

impl Avatar {
    /// Gives the avatar a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Overrides the derived initials.
    ///
    /// Worth setting for names the two-leading-letters rule reads wrongly —
    /// mononyms, handles, and every name whose family part comes first.
    pub fn initials(mut self, initials: impl Into<String>) -> Self {
        self.initials = Some(initials.into());
        self
    }

    /// Diameter in logical pixels. Defaults to 32.
    pub fn size(mut self, diameter: Px) -> Self {
        self.diameter = diameter;
        self
    }

    /// Overrides the tint derived from the name.
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    /// Shows a presence dot on the lower-right edge.
    pub fn presence(mut self, presence: Presence) -> Self {
        self.presence = Some(presence);
        self
    }

    /// The colour the presence dot is cut out of.
    ///
    /// Defaults to the theme's surface. Set it when the avatar sits on
    /// something else, or the dot's ring will not read as a hole.
    pub fn ring(mut self, color: Color) -> Self {
        self.ring = Some(color);
        self
    }
}

/// Up to two leading letters, one per word.
///
/// Returns empty for a name with no alphanumerics at all, and the circle is
/// then just a tint — which is the right answer for a placeholder account.
fn initials_of(name: &str) -> String {
    let mut out = String::new();
    for word in name.split_whitespace() {
        if let Some(ch) = word.chars().find(|c| c.is_alphanumeric()) {
            out.extend(ch.to_uppercase());
            if out.chars().count() == 2 {
                break;
            }
        }
    }
    out
}

/// Picks a stable tint for a name. FNV-1a, because it only has to be
/// well-distributed over short strings and identical on every platform.
fn tint_for(name: &str) -> Color {
    let mut hash: u32 = 0x811C_9DC5;
    for byte in name.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    AVATAR_TINTS[hash as usize % AVATAR_TINTS.len()]
}

impl Element for Avatar {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        Style {
            size: Size {
                width: spherekit_core::Length::Px(self.diameter),
                height: spherekit_core::Length::Px(self.diameter),
            },
            // An avatar in a row is the one thing that must not be squashed:
            // a name beside it can ellipsise, a circle cannot.
            flex_shrink: 0.0,
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let bounds = cx.bounds;
        if bounds.is_empty() {
            return;
        }
        let radius = Px(bounds.width().get().min(bounds.height().get()) * 0.5);
        let center = bounds.center();
        let fill = self.color.unwrap_or_else(|| tint_for(&self.name));
        cx.canvas.fill_circle(center, radius, fill);

        let initials = match self.initials.as_deref() {
            Some(explicit) => explicit.to_string(),
            None => initials_of(&self.name),
        };
        if !initials.is_empty() {
            // The ink is chosen against the tint, not taken from the theme: a
            // tint is the same colour in light and dark, so a theme text
            // colour would be unreadable on half of them.
            let ink =
                if fill.luminance() > 0.55 { Color::hex(0x14161A) } else { Color::hex(0xFFFFFF) };
            let style = spherekit_text::TextStyle {
                font_size: Px(radius.get() * 0.82),
                font: spherekit_text::FontRequest {
                    weight: cx.theme.typography.strong,
                    ..Default::default()
                },
                ..Default::default()
            };
            let layout = cx.text.layout(&initials, &style, None);
            let origin =
                Point::new(center.x - layout.size.width * 0.5, center.y - layout.size.height * 0.5);
            crate::text::draw_layout(
                cx.canvas,
                &layout,
                origin,
                ink,
                spherekit_render::TextRasterMode::Auto,
                (Px::ZERO, Color::TRANSPARENT),
                spherekit_render::coverage_contrast_for(ink, fill),
            );
        }

        if let Some(presence) = self.presence {
            // On the 45-degree diagonal, straddling the edge, which is where
            // every shell puts it and where it costs the least of the face.
            let offset = radius * core::f32::consts::FRAC_1_SQRT_2;
            let at = Point::new(center.x + offset, center.y + offset);
            let dot = radius * 0.34;
            let ring = self.ring.unwrap_or(cx.theme.colors.surface);
            cx.canvas.fill_circle(at, dot, ring);
            cx.canvas.fill_circle(at, dot - Px(2.0), presence.color(cx.theme));
        }
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::new(Role::Image, self.name.clone()))
    }
}

// ---------------------------------------------------------------------------
// Dropdown
// ---------------------------------------------------------------------------

/// Which way a [`Dropdown`] opens from its anchor.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum DropdownSide {
    /// Downward, from the anchor's bottom edge.
    #[default]
    Below,
    /// Upward, from the anchor's top edge. What a sidebar footer wants.
    Above,
}

/// The stacking order a context menu takes unless the caller overrides it.
///
/// Deliberately far above anything an application is likely to use for its
/// own panes, so a menu does not need the app to know about it.
pub const CONTEXT_MENU_Z: i32 = 10_000;

/// Below this the panel is treated as shut and leaves layout entirely.
const DROPDOWN_CLOSED: f32 = 0.002;

/// An animated popover panel.
///
/// The widget owns no timer and no open flag. It takes `open` in `0..=1` and
/// draws the frame that value describes, which is what lets one
/// [`Motion`](spherekit_core::animate::Motion) in the application drive it —
/// and what lets a test pass `0.5` and assert on a half-open panel without
/// running a clock. Pass a constant `1.0` for a menu that is simply there.
///
/// Anchoring is by containment. Every node is a containing block here, so a
/// `Dropdown` places itself against its **direct parent**: wrap the trigger and
/// the dropdown in one container and the panel lands on the trigger's edge
/// whatever height the trigger turns out to be.
///
/// ```ignore
/// div()
///     .flex_col()
///     .child(dropdown(self.menu.get().value()).above().child(/* items */))
///     .child(/* the row that opens it */)
/// ```
///
/// At `0.0` it sets `display: none` rather than merely going transparent: an
/// invisible panel that still hit-tested would swallow clicks meant for
/// whatever is behind it.
pub struct Dropdown {
    id: Option<ElementId>,
    children: Vec<AnyElement>,
    style: Style,
    paint: PaintStyle,
    open: f32,
    rise: Px,
    gap: Px,
    side: DropdownSide,
}

/// Creates a [`Dropdown`] at the given open amount, `0..=1`.
pub fn dropdown(open: f32) -> Dropdown {
    let mut style = Style::DEFAULT;
    style.flex_direction = spherekit_layout::FlexDirection::Column;
    Dropdown {
        id: None,
        children: Vec::new(),
        style,
        paint: PaintStyle::default(),
        open,
        rise: px(8.0),
        gap: px(6.0),
        side: DropdownSide::default(),
    }
}

impl Dropdown {
    /// Gives the panel a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Sets which edge the panel opens from.
    pub fn side(mut self, side: DropdownSide) -> Self {
        self.side = side;
        self
    }

    /// Opens upward, from the anchor's top edge.
    pub fn above(self) -> Self {
        self.side(DropdownSide::Above)
    }

    /// Opens downward, from the anchor's bottom edge.
    pub fn below(self) -> Self {
        self.side(DropdownSide::Below)
    }

    /// How far the panel travels as it opens. Defaults to 8 px.
    pub fn rise(mut self, rise: Px) -> Self {
        self.rise = rise;
        self
    }

    /// The resting distance between panel and anchor. Defaults to 6 px.
    ///
    /// Named `offset` rather than `gap` because [`Styled::gap`] already means
    /// the space *between this panel's children*, and an inherent method of the
    /// same name would silently shadow it in a builder chain — the panel would
    /// take the value as its anchor distance and its rows would sit flush.
    pub fn offset(mut self, offset: Px) -> Self {
        self.gap = offset;
        self
    }

    /// How far open the panel is once eased, `0..=1`.
    fn eased(&self) -> f32 {
        ease_out_cubic(self.open.clamp(0.0, 1.0))
    }
}

/// Cubic ease-out. The panel arrives quickly and settles, which reads as the
/// menu landing rather than drifting into place.
fn ease_out_cubic(t: f32) -> f32 {
    let inv = 1.0 - t.clamp(0.0, 1.0);
    1.0 - inv * inv * inv
}

impl ParentElement for Dropdown {
    fn extend_children(&mut self, children: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(children);
    }
}

impl Styled for Dropdown {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }
    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for Dropdown {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if self.open.clamp(0.0, 1.0) <= DROPDOWN_CLOSED {
            style.display = spherekit_layout::Display::None;
            return style;
        }
        style.position = spherekit_layout::Position::Absolute;
        // The slide is a margin rather than the inset itself, because the
        // inset is already spending its budget on `100 %` and a `Length`
        // cannot hold "100 % plus eight pixels".
        let travel = spherekit_core::Length::Px(self.gap + self.rise * (1.0 - self.eased()));
        match self.side {
            DropdownSide::Above => {
                style.inset.bottom = spherekit_core::Length::Fraction(1.0);
                style.margin.bottom = travel;
            }
            DropdownSide::Below => {
                style.inset.top = spherekit_core::Length::Fraction(1.0);
                style.margin.top = travel;
            }
        }
        // With no width and no horizontal anchor of its own, span the trigger.
        let unanchored = matches!(style.inset.left, spherekit_core::Length::Auto)
            && matches!(style.inset.right, spherekit_core::Length::Auto)
            && matches!(style.size.width, spherekit_core::Length::Auto);
        if unanchored {
            style.inset.left = spherekit_core::Length::Px(Px::ZERO);
            style.inset.right = spherekit_core::Length::Px(Px::ZERO);
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
        // A popover is one of the few things that has to be opaque over
        // whatever it covers, so it takes the theme's surface rather than
        // inheriting the translucency a Mica-backed window uses elsewhere.
        let mut style = self.paint.clone();
        if style.background.is_none() {
            style.background = Some(cx.theme.colors.surface.into());
            style.border_width = px(1.0);
            style.border_color = cx.theme.colors.border;
            style.corner_radii = Corners::all(cx.theme.radii.lg);
            style.shadows.push(cx.theme.shadows.md);
        }
        style.paint_box(cx.canvas, cx.bounds, cx.state);
    }

    fn paint_opacity(&self) -> f32 {
        (self.eased() * self.paint.opacity).clamp(0.0, 1.0)
    }

    fn paint_filter(&self) -> Option<spherekit_render::Filter> {
        self.paint.filter
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::new(Role::Menu, String::new()))
    }
}

/// Everything in this module, for a glob import.
pub mod prelude {
    pub use super::{
        Avatar, Button, ButtonVariant, ContextMenu, Dropdown, DropdownSide, MenuItem, Presence,
        Progress, ScrollView, Scrollbar, ScrollbarPolicy, Toggle, ValueControl, ValueShape, avatar,
        button, checkbox, context_menu, dropdown, fader, knob, menu_item, panel, progress,
        progress_indeterminate, scroll_area, scroll_view, separator, slider, toggle,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{Interactive, IntoElement};
    use crate::event::{ElementState, Modifiers, MouseButtonEvent, MouseMoveEvent};
    use crate::tree::UiTree;
    use smallvec::SmallVec;
    use spherekit_core::{ScaleFactor, size};
    use spherekit_render::{Canvas, Scene};
    use std::cell::Cell;
    use std::rc::Rc;

    fn viewport() -> Size<Px> {
        size(px(400.0), px(300.0))
    }

    /// A text system with a real face, or `None` on a machine with no fonts.
    fn text_system() -> Option<spherekit_text::TextSystem> {
        let mut system = spherekit_text::TextSystem::with_system_fonts();
        system.fonts_mut().resolve(&spherekit_text::FontRequest::default())?;
        Some(system)
    }

    #[test]
    fn a_button_inherits_theme_weight_and_allows_an_override() {
        let mut theme = crate::theme::Theme::dark();
        theme.typography.weight = spherekit_text::FontWeight::SEMI_BOLD;

        assert_eq!(
            button("Inherited").label_style(&theme).font.weight,
            spherekit_text::FontWeight::SEMI_BOLD
        );
        assert_eq!(
            button("Override")
                .weight(spherekit_text::FontWeight::BLACK)
                .label_style(&theme)
                .font
                .weight,
            spherekit_text::FontWeight::BLACK
        );
    }

    #[test]
    fn buttons_are_flat_without_borders_or_focus_outlines() {
        let theme = crate::theme::Theme::dark();

        for variant in [
            ButtonVariant::Primary,
            ButtonVariant::Secondary,
            ButtonVariant::Ghost,
            ButtonVariant::Danger,
        ] {
            let style = button("Action").variant(variant).paint_style(&theme);
            assert_eq!(style.border_width, Px::ZERO, "{variant:?} drew a border");
            assert!(style.border_color.is_transparent(), "{variant:?} kept a border colour");
            assert!(style.focus_ring.is_none(), "{variant:?} drew a focus outline");
        }
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
        let mut system = spherekit_text::TextSystem::new();
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
        let mut text = spherekit_text::TextSystem::new();
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
    fn a_short_slider_drag_uses_the_actual_track_length() {
        let value = Rc::new(Cell::new(0.0f32));
        let v = value.clone();
        let mut tree =
            mount(slider(0.0).id("s").range(0.0, 1.0).on_change(move |x| v.set(x)).into_element());

        // The mounted slider is 400 px wide. A 10 px drag from the midpoint
        // should move by 10/400, not 10/180 as the knob gesture did before.
        tree.dispatch(&press_at(200.0, 10.0, 1));
        tree.dispatch(&drag_to(210.0, 10.0, false));
        assert!((value.get() - 0.525).abs() < 0.001, "got {}", value.get());
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
                if let spherekit_render::DrawCommand::Quad(q) = q {
                    assert!(q.bounds.width().get().is_finite(), "fraction {f} gave {:?}", q.bounds);
                    assert!(q.bounds.width() >= Px::ZERO);
                }
            }
        }
    }

    /// Runs the scroll glide to completion.
    ///
    /// The wheel now sets a *destination*; `advance` walks the offset there.
    /// A test that asserts on the offset has to let that finish, exactly as a
    /// window does by continuing to draw.
    fn settle(tree: &mut UiTree) {
        for _ in 0..120 {
            if !tree.advance(std::time::Duration::from_millis(16)) {
                return;
            }
        }
        panic!("a scroll glide never finished");
    }

    #[test]
    fn one_wheel_notch_travels_a_platform_notch() {
        // A notch has to move about what every other application on the desktop
        // moves, or the window feels stuck even though it is scrolling. Windows
        // and the major toolkits step three lines per notch; one line is a third
        // of that and reads as the wheel barely working.
        let mut tree = UiTree::new();
        tree.build(
            div().w(relative(1.0)).h(relative(1.0)).child(overflowing_scroll_view()).into_element(),
        );
        tree.compute_layout(viewport()).unwrap();

        // One notch is one line of `ScrollDelta::Lines`; the platform reports
        // 120 raw units as 1.0.
        tree.dispatch(&wheel_at(50.0, 50.0, 1.0));
        settle(&mut tree);
        let moved = tree.scroll_offset_of("inner").unwrap().height.get();

        let theme = crate::theme::Theme::dark();
        let line = theme.typography.md.get() * theme.typography.line_height;
        let notch = line * crate::tree::WHEEL_LINES_PER_NOTCH;
        assert!(
            (moved - notch).abs() < 1.0,
            "one notch moved {moved:.1} px; a platform notch is {notch:.1} px \
             ({line:.1} px per line x {})",
            crate::tree::WHEEL_LINES_PER_NOTCH
        );
    }

    #[test]
    fn the_wheel_glides_rather_than_jumping() {
        // The offset must be *between* where it was and where it is going for
        // at least one frame. A jump would satisfy every other scroll test in
        // this file, which is why this one looks at the middle and not the end.
        let mut tree = UiTree::new();
        tree.build(
            div().w(relative(1.0)).h(relative(1.0)).child(overflowing_scroll_view()).into_element(),
        );
        tree.compute_layout(viewport()).unwrap();

        tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
        assert_eq!(
            tree.scroll_offset_of("inner").unwrap().height,
            Px::ZERO,
            "the wheel moved the content before a single frame had passed"
        );

        assert!(tree.advance(std::time::Duration::from_millis(16)));
        let mid = tree.scroll_offset_of("inner").unwrap().height;
        assert!(mid > Px::ZERO, "the glide never started");

        settle(&mut tree);
        let end = tree.scroll_offset_of("inner").unwrap().height;
        assert!(end > mid, "the glide stopped short: {mid:?} then {end:?}");
    }

    #[test]
    fn a_glide_is_abandoned_when_the_offset_is_set_outright() {
        // A dragged thumb and a `scroll_element_to` are both exact: a glide
        // still running would drag the content back off the mark a frame later.
        let mut tree = UiTree::new();
        tree.build(
            div().w(relative(1.0)).h(relative(1.0)).child(overflowing_scroll_view()).into_element(),
        );
        tree.compute_layout(viewport()).unwrap();

        tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
        tree.scroll_element_to("inner", Size::new(Px::ZERO, px(500.0)));
        assert!(!tree.advance(std::time::Duration::from_millis(16)), "a glide outlived the jump");
        assert_eq!(tree.scroll_offset_of("inner").unwrap().height, px(500.0));
    }

    #[test]
    fn an_indeterminate_shuttle_stays_on_the_track_and_repeats() {
        let mut widest: f32 = 0.0;
        for step in 0..200 {
            let t = step as f32 * 0.02;
            let (start, end) = Progress::shuttle(t);
            assert!(start >= 0.0 && end <= 1.0, "shuttle left the track at t={t}: {start}..{end}");
            assert!(end >= start, "shuttle inverted at t={t}");
            widest = widest.max(end - start);
        }
        // It has to actually be visible for most of the loop, not a hairline.
        assert!(widest > 0.3, "the shuttle never covered much of the track: {widest}");
        // And it loops: the same phase gives the same answer one period later.
        let a = Progress::shuttle(0.4);
        let b = Progress::shuttle(0.4 + INDETERMINATE_PERIOD);
        assert!((a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4);
    }

    #[test]
    fn an_indeterminate_bar_reports_no_value_to_a_screen_reader() {
        // Saying "0 percent" would be stating something false.
        let s = progress_indeterminate().semantics().unwrap();
        assert!(s.value.is_none());
        let s = progress(0.25).semantics().unwrap();
        assert_eq!(s.value.map(|v| v.value), Some(0.25));
    }

    /// A wheel gesture at a point, `down` lines' worth.
    fn wheel_at(x: f32, y: f32, down: f32) -> UiEvent {
        UiEvent::Scroll(crate::event::ScrollEvent {
            position: Point::new(px(x), px(y)),
            delta: crate::event::ScrollDelta::Lines(Size::new(0.0, -down)),
            modifiers: Modifiers::NONE,
            momentum: false,
        })
    }

    /// A scroll view over content that genuinely overflows it.
    ///
    /// `shrink(0.0)` is load-bearing: a flex item whose own content is empty
    /// has an automatic minimum size of zero, so a 1000 px spacer inside a
    /// 100 px column would otherwise be *shrunk to 100* and there would be
    /// nothing to scroll.
    fn overflowing_scroll_view() -> AnyElement {
        scroll_view()
            .id("inner")
            .w(relative(1.0))
            .h(px(100.0))
            .child(div().h(px(1000.0)).shrink(0.0))
            .into_element()
    }

    #[test]
    fn the_wheel_actually_moves_a_scroll_view() {
        let mut tree = UiTree::new();
        tree.build(
            div().w(relative(1.0)).h(relative(1.0)).child(overflowing_scroll_view()).into_element(),
        );
        tree.compute_layout(viewport()).unwrap();

        let before = tree.scroll_offset_of("inner").expect("the view has a node");
        assert_eq!(before.height, Px::ZERO);

        let result = tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
        assert!(result.repaint, "a scroll that moved something must repaint");
        assert!(result.consumed);
        settle(&mut tree);

        let after = tree.scroll_offset_of("inner").expect("the view has a node");
        assert!(after.height > Px::ZERO, "the wheel did not move the offset: {after:?}");
    }

    #[test]
    fn scrolling_stops_at_the_end_of_the_content() {
        let mut tree = UiTree::new();
        tree.build(
            div().w(relative(1.0)).h(relative(1.0)).child(overflowing_scroll_view()).into_element(),
        );
        tree.compute_layout(viewport()).unwrap();

        for _ in 0..200 {
            tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
            settle(&mut tree);
        }
        let end = tree.scroll_offset_of("inner").unwrap();
        // 1000 of content in a 100 window leaves 900 of travel, and not a pixel
        // more however long the wheel is spun.
        assert_eq!(end.height, px(900.0), "ran past the end of the content");
    }

    #[test]
    fn a_grown_scroll_view_still_reports_its_overflow() {
        // The shape a real page has: a scroll view that takes the leftover
        // height with `flex_1`, wrapping a centring row, wrapping a padded
        // column. Every one of those is a flex item that could be shrunk to
        // fit instead of overflowing, and if any of them is, the container
        // reports no overflow and neither the wheel nor a bar does anything.
        let mut tree = UiTree::new();
        tree.build(
            div()
                .flex_col()
                .w(relative(1.0))
                .h(px(300.0))
                .child(
                    scroll_view().id("pane").flex_1().w(relative(1.0)).child(
                        div().flex_row().justify_center().w(relative(1.0)).child(
                            div()
                                .flex_col()
                                .w(relative(1.0))
                                .max_w(px(760.0))
                                .p(px(32.0))
                                .child(div().h(px(1000.0)).shrink(0.0)),
                        ),
                    ),
                )
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();

        tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
        settle(&mut tree);
        let moved = tree.scroll_offset_of("pane").unwrap();
        assert!(moved.height > Px::ZERO, "a grown scroll view did not scroll: {moved:?}");
    }

    #[test]
    fn a_page_of_text_taller_than_its_pane_scrolls() {
        // Text, not spacers: a real page's height comes from shaping, and a
        // label's contribution to its parent's minimum size is what decides
        // whether the pane overflows or merely clips.
        let Some(mut text) = text_system() else { return };
        let mut tree = UiTree::new();
        let mut column = div().flex_col().w(relative(1.0)).max_w(px(760.0)).p(px(32.0));
        for i in 0..40 {
            column = column.child(crate::text::label(format!("Paragraph {i}")).text_size(px(14.0)));
        }
        tree.build(
            div()
                .flex_col()
                .w(relative(1.0))
                .h(px(300.0))
                .child(
                    scroll_view()
                        .id("pane")
                        .flex_1()
                        .w(relative(1.0))
                        .child(div().flex_row().justify_center().w(relative(1.0)).child(column)),
                )
                .into_element(),
        );
        tree.compute_layout_with_text(viewport(), &mut text).unwrap();

        tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
        settle(&mut tree);
        let moved = tree.scroll_offset_of("pane").unwrap();
        assert!(moved.height > Px::ZERO, "a page of text did not scroll: {moved:?}");
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
                .child(overflowing_scroll_view())
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();
        tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
        assert_eq!(outer_scrolls.get(), 0, "the wheel reached an ancestor scroll view");
    }

    #[test]
    fn a_scroll_view_at_its_end_hands_the_wheel_to_its_ancestor() {
        // Scroll chaining. Without it a list that has bottomed out swallows
        // every further notch and the page behind it feels stuck.
        let outer_scrolls = Rc::new(Cell::new(0));
        let o = outer_scrolls.clone();
        let mut tree = UiTree::new();
        tree.build(
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .on_scroll(move |_| o.set(o.get() + 1))
                .child(overflowing_scroll_view())
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();

        // A few notches, nowhere near the 900 px of travel available.
        for _ in 0..3 {
            tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
            settle(&mut tree);
        }
        assert_eq!(outer_scrolls.get(), 0, "an ancestor moved while the list still had room");

        // Spin it to the bottom, then one more notch: that one is the
        // ancestor's, because the list has nothing left to give.
        for _ in 0..200 {
            tree.dispatch(&wheel_at(50.0, 50.0, 3.0));
            settle(&mut tree);
        }
        assert_eq!(tree.scroll_offset_of("inner").unwrap().height, px(900.0));
        let chained = outer_scrolls.get();
        assert!(chained > 0, "the wheel never chained out of a finished list");
    }

    #[test]
    fn a_thumb_shrinks_as_the_content_grows_but_never_past_grabbing() {
        let bounds = Rect::new(Point::new(Px::ZERO, Px::ZERO), Size::new(px(200.0), px(100.0)));
        let short = crate::element::ScrollMetrics {
            offset: Size::new(Px::ZERO, Px::ZERO),
            content: Size::new(px(200.0), px(200.0)),
            client: Size::new(px(200.0), px(100.0)),
        };
        let long =
            crate::element::ScrollMetrics { content: Size::new(px(200.0), px(100_000.0)), ..short };

        let a = Scrollbar::for_axis(bounds, short, true, ScrollbarPolicy::Auto).unwrap();
        let b = Scrollbar::for_axis(bounds, long, true, ScrollbarPolicy::Auto).unwrap();
        assert!(a.thumb.height() > b.thumb.height(), "the thumb did not shrink with the content");
        assert!(
            b.thumb.height() >= px(SCROLLBAR_MIN_THUMB),
            "a hundred thousand rows produced an ungrabbable {:?} thumb",
            b.thumb.height()
        );
    }

    #[test]
    fn a_thumb_at_the_end_of_its_travel_sits_flush_with_the_track() {
        let bounds = Rect::new(Point::new(Px::ZERO, Px::ZERO), Size::new(px(200.0), px(100.0)));
        let metrics = crate::element::ScrollMetrics {
            offset: Size::new(Px::ZERO, px(400.0)),
            content: Size::new(px(200.0), px(500.0)),
            client: Size::new(px(200.0), px(100.0)),
        };
        let bar = Scrollbar::for_axis(bounds, metrics, true, ScrollbarPolicy::Auto).unwrap();
        // Fully scrolled means the thumb's bottom is the track's bottom — the
        // classic off-by-a-thumb bug is computing travel over the whole track.
        assert!(
            (bar.thumb.max_y().get() - bar.track.max_y().get()).abs() < 0.01,
            "thumb ended at {:?}, track ends at {:?}",
            bar.thumb.max_y(),
            bar.track.max_y()
        );
    }

    #[test]
    fn no_bar_is_drawn_for_content_that_fits_unless_asked() {
        let bounds = Rect::new(Point::new(Px::ZERO, Px::ZERO), Size::new(px(200.0), px(100.0)));
        let fits = crate::element::ScrollMetrics {
            offset: Size::ZERO,
            content: Size::new(px(200.0), px(80.0)),
            client: Size::new(px(200.0), px(100.0)),
        };
        assert!(Scrollbar::for_axis(bounds, fits, true, ScrollbarPolicy::Auto).is_none());
        assert!(Scrollbar::for_axis(bounds, fits, true, ScrollbarPolicy::Always).is_some());
        assert!(Scrollbar::for_axis(bounds, fits, true, ScrollbarPolicy::Never).is_none());
    }

    #[test]
    fn a_separator_is_one_logical_pixel_on_its_thin_axis() {
        let mut tree = mount(separator(false).into_element());
        let scene = paint(&mut tree);
        match &scene.commands[0] {
            spherekit_render::DrawCommand::Quad(q) => assert_eq!(q.bounds.height(), px(1.0)),
            other => panic!("expected a quad, got {other:?}"),
        }
    }

    // ----------------------------------------------------------- avatar

    #[test]
    fn initials_take_one_letter_from_each_of_the_first_two_words() {
        assert_eq!(initials_of("Ada Lovelace"), "AL");
        // A third word is not a third letter.
        assert_eq!(initials_of("Ada King Lovelace"), "AK");
        assert_eq!(initials_of("ada"), "A");
        // Leading punctuation is skipped rather than shown.
        assert_eq!(initials_of("@ada  lovelace"), "AL");
    }

    #[test]
    fn a_nameless_avatar_is_a_bare_tint_rather_than_a_stray_glyph() {
        assert_eq!(initials_of(""), "");
        assert_eq!(initials_of("   "), "");
        assert_eq!(initials_of("--"), "");
    }

    #[test]
    fn a_tint_is_stable_for_a_name_and_comes_from_the_palette() {
        // Stability is the whole contract: the same person must be the same
        // colour in every session, with nothing persisted anywhere.
        assert_eq!(tint_for("Ada Lovelace"), tint_for("Ada Lovelace"));
        assert!(AVATAR_TINTS.contains(&tint_for("Ada Lovelace")));
        assert!(AVATAR_TINTS.contains(&tint_for("")));
    }

    #[test]
    fn an_avatar_is_square_and_refuses_to_be_squashed() {
        let style = avatar("Ada Lovelace").size(px(28.0)).layout_style();
        assert_eq!(style.size.width, spherekit_core::Length::Px(px(28.0)));
        assert_eq!(style.size.height, spherekit_core::Length::Px(px(28.0)));
        // A name beside it can ellipsise; a circle cannot go oval.
        assert_eq!(style.flex_shrink, 0.0);
    }

    // --------------------------------------------------------- dropdown

    #[test]
    fn a_shut_dropdown_leaves_layout_rather_than_going_transparent() {
        // Merely invisible would still hit-test, and the panel would swallow
        // clicks meant for whatever it covers.
        let style = dropdown(0.0).layout_style();
        assert_eq!(style.display, spherekit_layout::Display::None);
    }

    #[test]
    fn an_opening_dropdown_travels_and_fades_together() {
        let gap = px(6.0);
        let rise = px(8.0);

        let shut = dropdown(0.02).offset(gap).rise(rise);
        let half = dropdown(0.5).offset(gap).rise(rise);
        let open = dropdown(1.0).offset(gap).rise(rise);

        let margin = |d: &Dropdown| match d.layout_style().margin.bottom {
            spherekit_core::Length::Px(v) => v,
            other => panic!("expected a pixel margin, got {other:?}"),
        };

        // Fully open sits exactly at the resting gap, with no layer pushed.
        assert_eq!(margin(&open.above()), gap);
        assert_eq!(dropdown(1.0).paint_opacity(), 1.0);

        // Part-open is further away and more transparent, in step.
        let half_margin = margin(&half.above());
        assert!(half_margin > gap, "{half_margin:?} should still be travelling");
        assert!(half_margin < gap + rise);
        let a = dropdown(0.5).paint_opacity();
        assert!(a > 0.0 && a < 1.0, "half-open opacity was {a}");

        // Barely open is nearly the full rise away and nearly invisible.
        assert!(margin(&shut.above()) > half_margin);
        assert!(dropdown(0.02).paint_opacity() < a);
    }

    #[test]
    fn a_dropdown_anchors_to_the_edge_it_opens_from() {
        let above = dropdown(1.0).above().layout_style();
        assert_eq!(above.inset.bottom, spherekit_core::Length::Fraction(1.0));
        assert_eq!(above.inset.top, spherekit_core::Length::Auto);

        let below = dropdown(1.0).below().layout_style();
        assert_eq!(below.inset.top, spherekit_core::Length::Fraction(1.0));
        assert_eq!(below.inset.bottom, spherekit_core::Length::Auto);
    }

    #[test]
    fn an_unanchored_dropdown_spans_its_trigger_but_an_explicit_width_wins() {
        let spanning = dropdown(1.0).layout_style();
        assert_eq!(spanning.inset.left, spherekit_core::Length::Px(Px::ZERO));
        assert_eq!(spanning.inset.right, spherekit_core::Length::Px(Px::ZERO));

        // A caller who names a width means it, and stretching would override it.
        let sized = dropdown(1.0).w(px(200.0)).layout_style();
        assert_eq!(sized.inset.left, spherekit_core::Length::Auto);
        assert_eq!(sized.inset.right, spherekit_core::Length::Auto);
    }

    #[test]
    fn a_dropdowns_own_gap_is_the_one_between_its_rows() {
        // `Dropdown::offset` is deliberately not called `gap`: an inherent
        // method of that name would shadow `Styled::gap` in a builder chain and
        // the rows would silently sit flush.
        let style = dropdown(1.0).above().offset(px(12.0)).gap(px(4.0)).layout_style();
        assert_eq!(style.gap, Size::new(px(4.0), px(4.0)));
        match style.margin.bottom {
            spherekit_core::Length::Px(v) => assert_eq!(v, px(12.0)),
            other => panic!("expected the offset as a pixel margin, got {other:?}"),
        }
    }

    #[test]
    fn a_dropdown_lays_its_children_out_as_a_column_by_default() {
        let style = dropdown(1.0).layout_style();
        assert_eq!(style.flex_direction, spherekit_layout::FlexDirection::Column);
    }
}

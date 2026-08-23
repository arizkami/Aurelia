//! Colour selection: the square, the ramps, and the panel that composes them.
//!
//! ## Why HSV and not the theme's HSL
//!
//! [`Palette`](crate::theme::Palette) is authored in [`Hsla`](spherekit_core::Hsla)
//! because a *theme* is written by shifting lightness — hover is "the same
//! colour, a little lighter". A *picker* is dragged, and the shape a user drags
//! on is the saturation/value square every colour tool has shipped since
//! Photoshop 1.0. Lightness would make that square a diamond with two unusable
//! corners, so the picker keeps its own [`Hsva`] and converts at the edges.
//!
//! ## The state model, unchanged
//!
//! Nothing here owns a colour. Every control takes the current [`Hsva`] and
//! reports the one the gesture produced:
//!
//! ```ignore
//! color_picker(self.tint.get())
//!     .alpha(true)
//!     .on_change({
//!         let tint = self.tint.clone();
//!         move |c| tint.set(c)
//!     })
//! ```
//!
//! ## Why the square is a mesh and not two gradients
//!
//! The textbook implementation of a saturation/value square is three quads: the
//! hue, a white ramp across it, a black ramp down it. It is wrong here, and the
//! reason is the one thing SphereKit does differently from a canvas.
//!
//! Blending happens in **linear light**. A 50 % white overlay on pure red gives
//! linear `(1.0, 0.5, 0.5)`, which is `#FFBBBB` once encoded — but HSV says
//! `s = 0.5` on red is `#FF8080`. The picker would show one colour and report
//! another, at every point except the four corners. No arrangement of gradient
//! stops fixes it, because the correction depends on the channel.
//!
//! So the square and the hue ramp are sampled: a small grid of vertices, each
//! carrying the exact colour [`Hsva::to_color`] gives for that point, with the
//! GPU interpolating between them. Between two samples the error is a fraction
//! of one 8-bit step; at every sample it is exact. The alpha ramp *is* still a
//! gradient, because premultiplied interpolation of one colour against its own
//! transparent self is exactly what an alpha ramp means.
//!
//! ## Why hue is carried alongside the colour
//!
//! Black is `v == 0`, and every hue produces it. A picker that stored only the
//! resulting [`Color`] would forget which hue the user was on the moment they
//! dragged into the bottom edge, and the ramp would snap to red as they dragged
//! back out. Carrying [`Hsva`] rather than [`Color`] is what keeps the gesture
//! reversible — which is also why [`Hsva::from_color`] is the *lossy* direction
//! and should only be used to seed a picker, never once per frame.

use crate::element::{
    AnyElement, Element, EventContext, IntoElement, PaintContext, ParentElement, Styled, div,
};
use crate::event::{EventFlow, Key, MouseButton, UiEvent};
use crate::semantics::{Role, Semantics, ValueRange};
use crate::style::{Cursor, FocusRing, PaintStyle};
use crate::text::label;
use spherekit_core::{
    Color, Corners, ElementId, Gradient, GradientStop, Point, Px, Rect, RoundedRect, Size, px,
    relative,
};
use spherekit_layout::Style;
use std::cell::RefCell;
use std::rc::Rc;

/// Scratch slot recording that a drag is in progress. Nonzero means yes.
const SCRATCH_DRAGGING: usize = 0;

/// A colour in hue, saturation, value and alpha.
///
/// Hue is in **turns** (`0..=1`), matching [`Hsla`](spherekit_core::Hsla), so
/// arithmetic on it wraps with `rem_euclid` rather than needing a modulo by
/// 360 that everybody forgets somewhere.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Hsva {
    /// Hue, in turns.
    pub h: f32,
    /// Saturation, `0..=1`.
    pub s: f32,
    /// Value (brightness), `0..=1`.
    pub v: f32,
    /// Alpha, `0..=1`.
    pub a: f32,
}

impl Default for Hsva {
    fn default() -> Self {
        Self { h: 0.0, s: 1.0, v: 1.0, a: 1.0 }
    }
}

/// Shorthand constructor for [`Hsva`].
#[inline]
pub fn hsva(h: f32, s: f32, v: f32, a: f32) -> Hsva {
    Hsva { h, s, v, a }
}

impl Hsva {
    /// Builds a colour, clamping saturation, value and alpha into range and
    /// wrapping hue.
    #[inline]
    pub fn new(h: f32, s: f32, v: f32, a: f32) -> Self {
        Self {
            h: h.rem_euclid(1.0),
            s: s.clamp(0.0, 1.0),
            v: v.clamp(0.0, 1.0),
            a: a.clamp(0.0, 1.0),
        }
    }

    /// Converts to sRGB.
    pub fn to_color(self) -> Color {
        let h = self.h.rem_euclid(1.0) * 6.0;
        let s = self.s.clamp(0.0, 1.0);
        let v = self.v.clamp(0.0, 1.0);
        let sector = h.floor();
        let f = h - sector;
        let p = v * (1.0 - s);
        let q = v * (1.0 - s * f);
        let t = v * (1.0 - s * (1.0 - f));
        let (r, g, b) = match sector as i32 % 6 {
            0 => (v, t, p),
            1 => (q, v, p),
            2 => (p, v, t),
            3 => (p, q, v),
            4 => (t, p, v),
            _ => (v, p, q),
        };
        Color { r, g, b, a: self.a.clamp(0.0, 1.0) }
    }

    /// Converts from sRGB.
    ///
    /// Lossy at the edges of the solid: every hue maps to the same black, and
    /// every hue maps to the same white at zero saturation. Seed a picker with
    /// this once; do not round-trip through it every frame, or a drag into the
    /// bottom of the square will lose the hue the user was working in.
    pub fn from_color(c: Color) -> Self {
        let (r, g, b) = (c.r.clamp(0.0, 1.0), c.g.clamp(0.0, 1.0), c.b.clamp(0.0, 1.0));
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let chroma = max - min;
        let h = if chroma <= f32::EPSILON {
            0.0
        } else if max == r {
            ((g - b) / chroma).rem_euclid(6.0)
        } else if max == g {
            (b - r) / chroma + 2.0
        } else {
            (r - g) / chroma + 4.0
        } / 6.0;
        let s = if max <= f32::EPSILON { 0.0 } else { chroma / max };
        Self { h: h.rem_euclid(1.0), s, v: max, a: c.a.clamp(0.0, 1.0) }
    }

    /// The same colour at full alpha, which is what a hue ramp and a
    /// saturation square are drawn from.
    #[inline]
    pub fn opaque(self) -> Color {
        Self { a: 1.0, ..self }.to_color()
    }

    /// The pure hue at full saturation and value: the top-right corner of the
    /// square, and the colour the ramp under the pointer is showing.
    #[inline]
    pub fn pure_hue(self) -> Color {
        Self { h: self.h, s: 1.0, v: 1.0, a: 1.0 }.to_color()
    }

    /// Returns the colour with a different hue.
    #[inline]
    pub fn with_hue(self, h: f32) -> Self {
        Self { h: h.rem_euclid(1.0), ..self }
    }

    /// Returns the colour with different saturation and value.
    #[inline]
    pub fn with_sv(self, s: f32, v: f32) -> Self {
        Self { s: s.clamp(0.0, 1.0), v: v.clamp(0.0, 1.0), ..self }
    }

    /// Returns the colour with a different alpha.
    #[inline]
    pub fn with_alpha(self, a: f32) -> Self {
        Self { a: a.clamp(0.0, 1.0), ..self }
    }

    /// Formats as `#RRGGBB`, or `#RRGGBBAA` when `with_alpha` is set.
    ///
    /// Uppercase, because a hex colour is read as a token rather than as prose
    /// and every design tool prints it that way.
    pub fn hex(self, with_alpha: bool) -> String {
        hex_string(self.to_color(), with_alpha)
    }
}

impl From<Hsva> for Color {
    #[inline]
    fn from(v: Hsva) -> Self {
        v.to_color()
    }
}

impl From<Color> for Hsva {
    #[inline]
    fn from(v: Color) -> Self {
        Hsva::from_color(v)
    }
}

/// Formats a colour as `#RRGGBB`, or `#RRGGBBAA` when `with_alpha` is set.
pub fn hex_string(color: Color, with_alpha: bool) -> String {
    let [r, g, b, a] = color.to_rgba8();
    if with_alpha {
        format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    } else {
        format!("#{r:02X}{g:02X}{b:02X}")
    }
}

/// Parses `#RGB`, `#RGBA`, `#RRGGBB` or `#RRGGBBAA`, with or without the hash.
///
/// Returns `None` rather than a default colour: a field that silently turns a
/// typo into black is worse than one that refuses it, because the user cannot
/// tell the two apart.
pub fn parse_hex(text: &str) -> Option<Color> {
    let s = text.trim().trim_start_matches('#');
    let nibble = |c: u8| (c as char).to_digit(16).map(|v| v as u8);
    let bytes = s.as_bytes();
    let (r, g, b, a) = match bytes.len() {
        // The short forms repeat each nibble, so `#0af` is `#00AAFF` — the
        // rule CSS uses, and the one everybody types into a field by hand.
        3 | 4 => {
            let v: Option<Vec<u8>> = bytes.iter().map(|c| nibble(*c).map(|n| n * 17)).collect();
            let v = v?;
            (v[0], v[1], v[2], if v.len() == 4 { v[3] } else { 255 })
        }
        6 | 8 => {
            let v: Option<Vec<u8>> =
                bytes.chunks(2).map(|p| Some(nibble(p[0])? * 16 + nibble(p[1])?)).collect();
            let v = v?;
            (v[0], v[1], v[2], if v.len() == 4 { v[3] } else { 255 })
        }
        _ => return None,
    };
    Some(Color::rgba8(r, g, b, a))
}

// ---------------------------------------------------------------------------
// Shared painting
// ---------------------------------------------------------------------------

/// The side of one checkerboard square, in logical pixels.
const CHECKER: f32 = 6.0;

/// Paints the alpha checkerboard inside a rounded box.
///
/// Drawn rather than tiled from a texture: at six logical pixels a square it is
/// a handful of quads on the same analytic pipeline everything else uses, and
/// it stays crisp at 150 % where a bitmap tile would resample.
fn paint_checkerboard(cx: &mut PaintContext<'_, '_>, shape: RoundedRect, dark: Color) {
    let b = shape.rect;
    cx.canvas.fill_rounded_rect(shape, Color::WHITE);
    cx.canvas.with_save(|canvas| {
        canvas.clip_rounded_rect(shape);
        let cols = (b.width().get() / CHECKER).ceil() as i32;
        let rows = (b.height().get() / CHECKER).ceil() as i32;
        for row in 0..rows {
            for col in 0..cols {
                if (row + col) % 2 == 0 {
                    continue;
                }
                let cell = Rect::new(
                    Point::new(
                        b.min_x() + px(col as f32 * CHECKER),
                        b.min_y() + px(row as f32 * CHECKER),
                    ),
                    Size::new(px(CHECKER), px(CHECKER)),
                );
                canvas.fill_rect(cell.intersection(b), dark);
            }
        }
    });
}

/// How many columns and rows the saturation/value square is sampled at.
///
/// Sixteen by twelve: the value axis is the steepest — it carries the sRGB
/// transfer curve — and even there the largest error between two samples is
/// under one 8-bit step. Doubling it would cost 400 more vertices and change
/// nothing anybody can see.
const SQUARE_COLS: usize = 16;
/// Rows in the sampled square. See [`SQUARE_COLS`].
const SQUARE_ROWS: usize = 12;
/// How many segments a hue ramp is sampled at.
///
/// Thirty-six is six per primary-to-primary run, and each of those runs is a
/// straight line in sRGB, so the samples land on the exact ramp with a little
/// interpolation error between them and none at all at the stops.
const RAMP_SEGMENTS: usize = 36;

/// Fills a rounded box with a grid sampled from `f`, in the box's own space.
///
/// `f` takes `(u, v)` in `0..=1` from the top-left and returns the colour that
/// point *is*. See the module docs for why this is a mesh rather than the two
/// gradient overlays every other toolkit uses.
fn fill_sampled(
    cx: &mut PaintContext<'_, '_>,
    shape: RoundedRect,
    cols: usize,
    rows: usize,
    f: impl Fn(f32, f32) -> Color,
) {
    let b = shape.rect;
    if b.is_empty() || cols == 0 || rows == 0 {
        return;
    }
    let stride = (cols + 1) as u32;
    let mut vertices = Vec::with_capacity((cols + 1) * (rows + 1));
    for row in 0..=rows {
        let v = row as f32 / rows as f32;
        for col in 0..=cols {
            let u = col as f32 / cols as f32;
            let point = Point::new(b.min_x() + b.width() * u, b.min_y() + b.height() * v);
            vertices.push(spherekit_render::MeshVertex::new(point, f(u, v).to_linear()));
        }
    }
    let mut indices = Vec::with_capacity(cols * rows * 6);
    for row in 0..rows {
        for col in 0..cols {
            let i = row as u32 * stride + col as u32;
            indices.extend_from_slice(&[i, i + 1, i + stride, i + 1, i + stride + 1, i + stride]);
        }
    }
    // Clipped rather than shaped: the mesh is a rectangle, and the corner
    // radius belongs to the box it is filling, not to the sampling.
    cx.canvas.with_save(|canvas| {
        canvas.clip_rounded_rect(shape);
        canvas.draw_mesh(spherekit_render::Mesh { vertices, indices }, None);
    });
}

/// Paints the round handle every colour control drags.
///
/// Two rings rather than one: a white ring reads on a dark swatch, a dark ring
/// reads on a light one, and a control whose handle vanishes over half its own
/// range is the defining bug of hand-rolled colour pickers.
fn paint_handle(cx: &mut PaintContext<'_, '_>, centre: Point<Px>, radius: Px, fill: Color) {
    cx.canvas.fill_circle(centre, radius + px(1.0), Color::BLACK.with_alpha(0.35));
    cx.canvas.fill_circle(centre, radius, fill);
    cx.canvas.stroke_circle(centre, radius - px(1.0), Color::WHITE, px(2.0));
}

// ---------------------------------------------------------------------------
// Saturation / value square
// ---------------------------------------------------------------------------

/// The saturation and value plane of one hue.
///
/// Saturation runs left to right and value runs bottom to top, which is the
/// arrangement every colour tool uses; the hue itself comes from the [`Hsva`]
/// handed in, so this control never changes it.
pub struct ColorArea {
    id: Option<ElementId>,
    value: Hsva,
    disabled: bool,
    style: Style,
    paint: PaintStyle,
    on_change: Option<Box<dyn FnMut(Hsva)>>,
}

/// Creates a [`ColorArea`].
pub fn color_area(value: Hsva) -> ColorArea {
    ColorArea {
        id: None,
        value,
        disabled: false,
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
        on_change: None,
    }
}

impl ColorArea {
    /// Gives the area a stable identity, which it needs to keep its drag state
    /// and focus across rebuilds.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Marks the area disabled.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Runs when the pointer or the keyboard moves the sample point.
    pub fn on_change(mut self, f: impl FnMut(Hsva) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }

    /// Sets an explicit size. Without one the area fills its row and stands
    /// 160 px tall.
    pub fn size(mut self, width: Px, height: Px) -> Self {
        self.style.size = Size {
            width: spherekit_core::Length::Px(width),
            height: spherekit_core::Length::Px(height),
        };
        self
    }

    fn emit(&mut self, next: Hsva) {
        if next != self.value
            && let Some(f) = self.on_change.as_mut()
        {
            f(next);
        }
    }

    /// Turns a pointer position into a colour.
    fn sample(&self, at: Point<Px>, bounds: Rect<Px>) -> Hsva {
        let s = ((at.x - bounds.min_x()) / bounds.width().max(px(1.0))).clamp(0.0, 1.0);
        let v = 1.0 - ((at.y - bounds.min_y()) / bounds.height().max(px(1.0))).clamp(0.0, 1.0);
        self.value.with_sv(s, v)
    }
}

impl Styled for ColorArea {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        // Kept so a caller can still set a margin or an opacity through the
        // same trait every other element uses. The plane itself is three
        // gradients rather than a styled box, so a background set here would
        // simply be drawn over.
        &mut self.paint
    }
}

impl Element for ColorArea {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if matches!(style.size.width, spherekit_core::Length::Auto) {
            style.size.width = relative(1.0);
        }
        if matches!(style.size.height, spherekit_core::Length::Auto) {
            style.size.height = spherekit_core::Length::Px(px(160.0));
        }
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
        let b = cx.bounds;
        if b.is_empty() {
            return;
        }
        let radius = cx.theme.radii.md;
        let shape = RoundedRect::uniform(b, radius);

        // Saturation across, value down, sampled from the colour solid itself
        // so that what the square shows is what a click on it reports.
        let hue = self.value.h;
        fill_sampled(cx, shape, SQUARE_COLS, SQUARE_ROWS, |u, v| {
            Hsva { h: hue, s: u, v: 1.0 - v, a: 1.0 }.to_color()
        });
        cx.canvas.stroke_rounded_rect(shape, cx.theme.colors.border, px(1.0));

        let centre = Point::new(
            b.min_x() + b.width() * self.value.s.clamp(0.0, 1.0),
            b.max_y() - b.height() * self.value.v.clamp(0.0, 1.0),
        );
        paint_handle(cx, centre, px(7.0), self.value.opaque());

        if cx.state.focused {
            let style = PaintStyle {
                focus_ring: Some(FocusRing {
                    color: cx.theme.colors.focus,
                    ..FocusRing::default()
                }),
                corner_radii: Corners::all(radius),
                ..Default::default()
            };
            style.paint_box(cx.canvas, b, cx.state);
        }
        if self.disabled {
            cx.canvas.fill_rounded_rect(shape, cx.theme.colors.background.with_alpha(0.55));
        }
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.disabled {
            return EventFlow::Continue;
        }
        cx.set_cursor(Cursor::Crosshair);
        match cx.event {
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                cx.focus();
                let next = self.sample(e.position, cx.bounds);
                self.emit(next);
                cx.scratch[SCRATCH_DRAGGING] = 1.0;
                // Captured for the same reason a fader is: a colour drag
                // routinely leaves the square and has to keep tracking, or the
                // corners become unreachable.
                cx.capture();
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::MouseMove(e) if cx.scratch[SCRATCH_DRAGGING] != 0.0 => {
                let next = self.sample(e.position, cx.bounds);
                self.emit(next);
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::MouseUp(e) if e.button == MouseButton::Primary => {
                if cx.scratch[SCRATCH_DRAGGING] == 0.0 {
                    return EventFlow::Continue;
                }
                cx.scratch[SCRATCH_DRAGGING] = 0.0;
                cx.release();
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::Key(k) if k.state.is_pressed() => {
                let step = if k.modifiers.shift { 0.01 } else { 0.04 };
                let v = self.value;
                let next = match k.key {
                    Key::Left => v.with_sv(v.s - step, v.v),
                    Key::Right => v.with_sv(v.s + step, v.v),
                    Key::Up => v.with_sv(v.s, v.v + step),
                    Key::Down => v.with_sv(v.s, v.v - step),
                    _ => return EventFlow::Continue,
                };
                self.emit(next);
                cx.notify();
                EventFlow::Stop
            }
            _ => EventFlow::Continue,
        }
    }

    fn focusable(&self) -> bool {
        !self.disabled
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(
            Semantics::new(Role::Canvas, "Saturation and brightness")
                .value_text(self.value.hex(false))
                .disabled(self.disabled),
        )
    }
}

// ---------------------------------------------------------------------------
// Hue and alpha ramps
// ---------------------------------------------------------------------------

/// Which channel a [`ColorSlider`] drags.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorChannel {
    /// The hue ramp: the full sweep, independent of the current colour.
    Hue,
    /// The alpha ramp: transparent to the current colour, over a checkerboard.
    Alpha,
}

/// A single-channel ramp beside a [`ColorArea`].
///
/// One type for hue and alpha because they differ only in the ramp they draw:
/// the geometry, the drag, the keyboard handling and the handle are identical,
/// and two copies of that is two places for them to drift.
pub struct ColorSlider {
    id: Option<ElementId>,
    value: Hsva,
    channel: ColorChannel,
    vertical: bool,
    disabled: bool,
    style: Style,
    paint: PaintStyle,
    on_change: Option<Box<dyn FnMut(Hsva)>>,
}

/// Creates a hue ramp.
pub fn hue_slider(value: Hsva) -> ColorSlider {
    ColorSlider::new(value, ColorChannel::Hue)
}

/// Creates an alpha ramp for the given colour.
pub fn alpha_slider(value: Hsva) -> ColorSlider {
    ColorSlider::new(value, ColorChannel::Alpha)
}

impl ColorSlider {
    fn new(value: Hsva, channel: ColorChannel) -> Self {
        Self {
            id: None,
            value,
            channel,
            vertical: false,
            disabled: false,
            style: Style::DEFAULT,
            paint: PaintStyle::default(),
            on_change: None,
        }
    }

    /// Gives the ramp a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Runs the ramp top-to-bottom instead of left-to-right.
    pub fn vertical(mut self, vertical: bool) -> Self {
        self.vertical = vertical;
        self
    }

    /// Marks the ramp disabled.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Runs when the channel changes, with the whole colour.
    ///
    /// The whole colour rather than one float, so a caller stores one value and
    /// never has to reassemble it from three controls that each knew a third of
    /// the answer.
    pub fn on_change(mut self, f: impl FnMut(Hsva) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }

    /// Sets an explicit size.
    pub fn size(mut self, width: Px, height: Px) -> Self {
        self.style.size = Size {
            width: spherekit_core::Length::Px(width),
            height: spherekit_core::Length::Px(height),
        };
        self
    }

    /// The channel's current position along the ramp, `0..=1`.
    fn normalized(&self) -> f32 {
        match self.channel {
            ColorChannel::Hue => self.value.h.rem_euclid(1.0),
            ColorChannel::Alpha => self.value.a.clamp(0.0, 1.0),
        }
    }

    fn with_normalized(&self, t: f32) -> Hsva {
        match self.channel {
            ColorChannel::Hue => self.value.with_hue(t.clamp(0.0, 1.0)),
            ColorChannel::Alpha => self.value.with_alpha(t),
        }
    }

    fn emit(&mut self, next: Hsva) {
        if next != self.value
            && let Some(f) = self.on_change.as_mut()
        {
            f(next);
        }
    }

    /// Turns a pointer position into a position along the ramp.
    fn sample(&self, at: Point<Px>, bounds: Rect<Px>) -> f32 {
        if self.vertical {
            ((at.y - bounds.min_y()) / bounds.height().max(px(1.0))).clamp(0.0, 1.0)
        } else {
            ((at.x - bounds.min_x()) / bounds.width().max(px(1.0))).clamp(0.0, 1.0)
        }
    }
}

impl Styled for ColorSlider {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for ColorSlider {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        let (w, h) = if self.vertical {
            (spherekit_core::Length::Px(px(14.0)), relative(1.0))
        } else {
            (relative(1.0), spherekit_core::Length::Px(px(14.0)))
        };
        if matches!(style.size.width, spherekit_core::Length::Auto) {
            style.size.width = w;
        }
        if matches!(style.size.height, spherekit_core::Length::Auto) {
            style.size.height = h;
        }
        style.flex_shrink = 0.0;
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
        let b = cx.bounds;
        if b.is_empty() {
            return;
        }
        // A pill, so the ramp reads as a track rather than as a swatch that
        // happens to be long.
        let radius = Px(b.width().get().min(b.height().get()) * 0.5);
        let shape = RoundedRect::uniform(b, radius);

        match self.channel {
            ColorChannel::Hue => {
                // Sampled, for the same reason the square is: interpolating two
                // primaries in linear light lands between the hues, not on
                // them, and a hue ramp that lies is a picker that lies.
                let (cols, rows) =
                    if self.vertical { (1, RAMP_SEGMENTS) } else { (RAMP_SEGMENTS, 1) };
                let vertical = self.vertical;
                fill_sampled(cx, shape, cols, rows, |u, v| {
                    Hsva::new(if vertical { v } else { u }, 1.0, 1.0, 1.0).to_color()
                });
            }
            ColorChannel::Alpha => {
                // The checkerboard first: an alpha ramp drawn on the theme's
                // surface tells the user nothing about what transparent looks
                // like over their own content.
                paint_checkerboard(cx, shape, cx.theme.colors.border);
                let solid = self.value.opaque();
                // A gradient here, and correctly so: premultiplied
                // interpolation between a colour and its own transparent self
                // *is* the definition of an opacity ramp.
                let (start, end) = if self.vertical {
                    (Point::new(b.min_x(), b.min_y()), Point::new(b.min_x(), b.max_y()))
                } else {
                    (Point::new(b.min_x(), b.min_y()), Point::new(b.max_x(), b.min_y()))
                };
                cx.canvas.fill_rounded_rect_with(
                    shape,
                    Gradient::Linear {
                        start,
                        end,
                        stops: smallvec::SmallVec::from_slice(&[
                            GradientStop::new(0.0, solid.with_alpha(0.0)),
                            GradientStop::new(1.0, solid),
                        ]),
                    }
                    .into(),
                );
            }
        }
        cx.canvas.stroke_rounded_rect(shape, cx.theme.colors.border, px(1.0));

        let t = self.normalized();
        let centre = if self.vertical {
            Point::new(b.center().x, b.min_y() + b.height() * t)
        } else {
            Point::new(b.min_x() + b.width() * t, b.center().y)
        };
        let fill = match self.channel {
            ColorChannel::Hue => self.value.pure_hue(),
            ColorChannel::Alpha => self.value.opaque(),
        };
        paint_handle(cx, centre, radius + px(1.0), fill);

        if cx.state.focused {
            let ring = FocusRing { color: cx.theme.colors.focus, ..FocusRing::default() };
            let style = PaintStyle {
                focus_ring: Some(ring),
                corner_radii: Corners::all(radius + px(3.0)),
                ..Default::default()
            };
            let r = radius + px(3.0);
            style.paint_box(
                cx.canvas,
                Rect::new(Point::new(centre.x - r, centre.y - r), Size::new(r * 2.0, r * 2.0)),
                cx.state,
            );
        }
        if self.disabled {
            cx.canvas.fill_rounded_rect(shape, cx.theme.colors.background.with_alpha(0.55));
        }
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.disabled {
            return EventFlow::Continue;
        }
        cx.set_cursor(if self.vertical { Cursor::ResizeNs } else { Cursor::ResizeEw });
        match cx.event {
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                cx.focus();
                let t = self.sample(e.position, cx.bounds);
                let next = self.with_normalized(t);
                self.emit(next);
                cx.scratch[SCRATCH_DRAGGING] = 1.0;
                cx.capture();
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::MouseMove(e) if cx.scratch[SCRATCH_DRAGGING] != 0.0 => {
                let t = self.sample(e.position, cx.bounds);
                let next = self.with_normalized(t);
                self.emit(next);
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::MouseUp(e) if e.button == MouseButton::Primary => {
                if cx.scratch[SCRATCH_DRAGGING] == 0.0 {
                    return EventFlow::Continue;
                }
                cx.scratch[SCRATCH_DRAGGING] = 0.0;
                cx.release();
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::Key(k) if k.state.is_pressed() => {
                let step = if k.modifiers.shift { 0.002 } else { 0.02 };
                let t = self.normalized();
                let next = match k.key {
                    Key::Left | Key::Down => t - step,
                    Key::Right | Key::Up => t + step,
                    Key::Home => 0.0,
                    Key::End => 1.0,
                    _ => return EventFlow::Continue,
                };
                // Hue wraps and alpha clamps, which is the difference between a
                // circle and a range; `with_normalized` already knows which.
                let next = match self.channel {
                    ColorChannel::Hue => self.value.with_hue(next),
                    ColorChannel::Alpha => self.value.with_alpha(next),
                };
                self.emit(next);
                cx.notify();
                EventFlow::Stop
            }
            _ => EventFlow::Continue,
        }
    }

    fn focusable(&self) -> bool {
        !self.disabled
    }

    fn semantics(&self) -> Option<Semantics> {
        let (name, value, text) = match self.channel {
            ColorChannel::Hue => (
                "Hue",
                self.value.h * 360.0,
                format!("{:.0} degrees", self.value.h.rem_euclid(1.0) * 360.0),
            ),
            ColorChannel::Alpha => {
                ("Opacity", self.value.a * 100.0, format!("{:.0}%", self.value.a * 100.0))
            }
        };
        let max = if self.channel == ColorChannel::Hue { 360.0 } else { 100.0 };
        Some(
            Semantics::new(Role::Slider, name)
                .value(ValueRange { value, min: 0.0, max, step: None })
                .value_text(text)
                .disabled(self.disabled)
                .with_implied_actions(),
        )
    }
}

// ---------------------------------------------------------------------------
// Swatch
// ---------------------------------------------------------------------------

/// A colour chip: a preset to click, or a preview of the current value.
pub struct ColorSwatch {
    id: Option<ElementId>,
    color: Color,
    selected: bool,
    interactive: bool,
    extent: Px,
    label_text: String,
    on_select: Option<Box<dyn FnMut(Color)>>,
}

/// Creates a [`ColorSwatch`].
pub fn color_swatch(color: Color) -> ColorSwatch {
    ColorSwatch {
        id: None,
        color,
        selected: false,
        interactive: false,
        extent: px(20.0),
        label_text: String::new(),
        on_select: None,
    }
}

impl ColorSwatch {
    /// Gives the chip a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Sets the chip's side length. Defaults to 20 px.
    pub fn size(mut self, extent: Px) -> Self {
        self.extent = extent;
        self
    }

    /// Draws the chip as the current choice, with a ring around it.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Sets the accessible name. Defaults to the chip's hex.
    pub fn label(mut self, text: impl Into<String>) -> Self {
        self.label_text = text.into();
        self
    }

    /// Makes the chip a button. Without this it is a read-only preview and
    /// stays out of the tab order.
    pub fn on_select(mut self, f: impl FnMut(Color) + 'static) -> Self {
        self.interactive = true;
        self.on_select = Some(Box::new(f));
        self
    }
}

impl Element for ColorSwatch {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        Style {
            size: Size {
                width: spherekit_core::Length::Px(self.extent),
                height: spherekit_core::Length::Px(self.extent),
            },
            flex_shrink: 0.0,
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        if self.interactive {
            cx.keep_interactive();
        }
        let b = cx.bounds;
        if b.is_empty() {
            return;
        }
        let c = cx.theme.colors;
        let shape = RoundedRect::uniform(b, cx.theme.radii.sm);
        if self.color.a < 1.0 {
            paint_checkerboard(cx, shape, c.border);
        }
        cx.canvas.fill_rounded_rect(shape, self.color);
        // A hairline in the theme's border colour, so a swatch the same colour
        // as the surface behind it still has an edge.
        cx.canvas.stroke_rounded_rect(shape, c.border_strong.with_alpha(0.6), px(1.0));

        if self.selected {
            let ring = RoundedRect::uniform(
                b.outset(spherekit_core::Edges::all(px(3.0))),
                cx.theme.radii.sm + px(3.0),
            );
            cx.canvas.stroke_rounded_rect(ring, c.accent, px(2.0));
        }
        if cx.state.hovered && self.interactive && !self.selected {
            let ring = RoundedRect::uniform(
                b.outset(spherekit_core::Edges::all(px(2.0))),
                cx.theme.radii.sm + px(2.0),
            );
            cx.canvas.stroke_rounded_rect(ring, c.border_strong, px(1.0));
        }
        if cx.state.focused {
            let style = PaintStyle {
                focus_ring: Some(FocusRing { color: c.focus, ..FocusRing::default() }),
                corner_radii: Corners::all(cx.theme.radii.sm),
                ..Default::default()
            };
            style.paint_box(cx.canvas, b, cx.state);
        }
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if !self.interactive {
            return EventFlow::Continue;
        }
        cx.set_cursor(Cursor::Pointer);
        let chosen = match cx.event {
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
        if chosen {
            if let Some(f) = self.on_select.as_mut() {
                f(self.color);
            }
            cx.notify();
            return EventFlow::Stop;
        }
        EventFlow::Continue
    }

    fn focusable(&self) -> bool {
        self.interactive
    }

    fn semantics(&self) -> Option<Semantics> {
        let name = if self.label_text.is_empty() {
            hex_string(self.color, self.color.a < 1.0)
        } else {
            self.label_text.clone()
        };
        let role = if self.interactive { Role::Button } else { Role::Image };
        Some(Semantics::new(role, name).checked(self.selected).with_implied_actions())
    }
}

// ---------------------------------------------------------------------------
// The composed panel
// ---------------------------------------------------------------------------

/// The presets a picker offers when the caller names none.
///
/// A spread around the wheel at one saturation and one value, plus the two
/// neutrals. Generated rather than hand-listed so the row stays evenly spaced
/// whatever length it is asked for.
fn default_swatches() -> Vec<Color> {
    let mut out: Vec<Color> =
        (0..8).map(|i| Hsva::new(i as f32 / 8.0, 0.72, 0.92, 1.0).to_color()).collect();
    out.push(Color::WHITE);
    out.push(Color::hex(0x20_2225));
    out
}

/// A complete colour picker: square, ramps, readout and presets.
///
/// Composed from the controls above rather than drawn as one thing, which is
/// what makes the parts individually usable — a levels panel wants only the hue
/// ramp, and a theme editor wants the square without the presets.
///
/// ```ignore
/// color_picker(state.tint.get())
///     .id("tint")
///     .alpha(true)
///     .on_change(move |c| state.tint.set(c))
/// ```
pub struct ColorPicker {
    id: Option<ElementId>,
    value: Hsva,
    alpha: bool,
    swatches: Option<Vec<Color>>,
    readout: bool,
    disabled: bool,
    height: Px,
    style: Style,
    paint: PaintStyle,
    /// Shared because every sub-control reports through the same callback, and
    /// a `Box<dyn FnMut>` cannot be cloned into four closures.
    on_change: Rc<RefCell<Option<Box<dyn FnMut(Hsva)>>>>,
}

/// Creates a [`ColorPicker`].
pub fn color_picker(value: Hsva) -> ColorPicker {
    ColorPicker {
        id: None,
        value,
        alpha: false,
        swatches: None,
        readout: true,
        disabled: false,
        height: px(160.0),
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
        on_change: Rc::new(RefCell::new(None)),
    }
}

impl ColorPicker {
    /// Gives the panel a stable identity. Its sub-controls derive theirs from
    /// it, so a picker that keeps focus across rebuilds needs one.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Shows the alpha ramp. Off by default: most pickers choose an opaque
    /// colour, and an alpha ramp on one of those is a control that can only
    /// produce a wrong answer.
    pub fn alpha(mut self, alpha: bool) -> Self {
        self.alpha = alpha;
        self
    }

    /// Replaces the preset row. An empty slice removes it.
    pub fn swatches(mut self, colors: impl IntoIterator<Item = Color>) -> Self {
        self.swatches = Some(colors.into_iter().collect());
        self
    }

    /// Shows or hides the hex readout. Shown by default.
    pub fn readout(mut self, readout: bool) -> Self {
        self.readout = readout;
        self
    }

    /// Sets the height of the square and the ramps beside it.
    pub fn plane_height(mut self, height: Px) -> Self {
        self.height = height;
        self
    }

    /// Marks every control in the panel disabled.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Runs when any control in the panel changes the colour.
    pub fn on_change(self, f: impl FnMut(Hsva) + 'static) -> Self {
        *self.on_change.borrow_mut() = Some(Box::new(f));
        self
    }

    /// A callback that forwards into the shared one.
    ///
    /// Each sub-control gets its own closure over the same `Rc`, so the panel
    /// takes one `on_change` from the caller instead of four.
    fn forward(&self) -> impl FnMut(Hsva) + 'static {
        let shared = Rc::clone(&self.on_change);
        move |next| {
            if let Ok(mut slot) = shared.try_borrow_mut()
                && let Some(f) = slot.as_mut()
            {
                f(next);
            }
        }
    }

    /// Derives a sub-control's identity from the panel's.
    fn key(&self, part: &'static str) -> ElementId {
        match self.id {
            Some(id) => ElementId::from_key((id, part)),
            None => ElementId::from_key(("color-picker", part)),
        }
    }
}

impl Styled for ColorPicker {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for ColorPicker {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        style.display = spherekit_layout::Display::Flex;
        style.flex_direction = spherekit_layout::FlexDirection::Column;
        style.gap = Size::new(px(10.0), px(10.0));
        if matches!(style.size.width, spherekit_core::Length::Auto) {
            style.size.width = relative(1.0);
        }
        style
    }

    /// Builds the panel's contents.
    ///
    /// Done here rather than in the constructor because the builder methods run
    /// after it — `take_children` is called once, at build, and is the first
    /// point at which every option is known. See the tree's build loop, which
    /// takes children before it asks for a layout style.
    fn take_children(&mut self) -> Vec<AnyElement> {
        let value = self.value;
        let disabled = self.disabled;
        let hex = value.hex(self.alpha);

        let mut plane = div()
            .flex_row()
            .gap(px(10.0))
            .h(self.height)
            .child(
                color_area(value).id(self.key("area")).disabled(disabled).on_change(self.forward()),
            )
            .child(
                hue_slider(value)
                    .id(self.key("hue"))
                    .vertical(true)
                    .disabled(disabled)
                    .on_change(self.forward()),
            );
        if self.alpha {
            plane = plane.child(
                alpha_slider(value)
                    .id(self.key("alpha"))
                    .vertical(true)
                    .disabled(disabled)
                    .on_change(self.forward()),
            );
        }

        let mut children: Vec<AnyElement> = vec![plane.into_element()];

        if self.readout {
            children.push(
                div()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(color_swatch(value.to_color()).size(px(28.0)))
                    .child(label(hex))
                    .into_element(),
            );
        }

        let swatches = self.swatches.clone().unwrap_or_else(default_swatches);
        if !swatches.is_empty() {
            let mut row = div().flex_row().gap(px(6.0));
            for (i, color) in swatches.into_iter().enumerate() {
                let selected = color.to_rgba8() == value.to_color().to_rgba8();
                let mut forward = self.forward();
                let alpha_kept = self.alpha;
                row = row.child(
                    color_swatch(color)
                        .id(self.key("swatch").child(i))
                        .selected(selected)
                        .on_select(move |chosen| {
                            let mut next = Hsva::from_color(chosen);
                            // A preset says nothing about opacity, so an opaque
                            // picker keeps its own alpha rather than being
                            // silently reset to 1.0 by a click on a chip.
                            if !alpha_kept {
                                next = next.with_alpha(1.0);
                            }
                            forward(next);
                        }),
                );
            }
            children.push(row.into_element());
        }

        children
    }

    fn paint(&mut self, _cx: &mut PaintContext<'_, '_>) {}

    fn semantics(&self) -> Option<Semantics> {
        Some(
            Semantics::new(Role::Group, "Colour picker")
                .value_text(self.value.hex(self.alpha))
                .disabled(self.disabled),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsva_round_trips_through_srgb() {
        for i in 0..12 {
            let start = Hsva::new(i as f32 / 12.0, 0.8, 0.7, 1.0);
            let back = Hsva::from_color(start.to_color());
            assert!((back.h - start.h).abs() < 1e-3, "hue drifted: {back:?} vs {start:?}");
            assert!((back.s - start.s).abs() < 1e-3, "saturation drifted: {back:?}");
            assert!((back.v - start.v).abs() < 1e-3, "value drifted: {back:?}");
        }
    }

    #[test]
    fn grey_has_no_hue_and_no_saturation() {
        let grey = Hsva::from_color(Color::rgb(0.5, 0.5, 0.5));
        assert_eq!(grey.s, 0.0);
        assert!((grey.v - 0.5).abs() < 1e-6);
    }

    #[test]
    fn primaries_land_on_the_expected_hues() {
        let red = Hsva::new(0.0, 1.0, 1.0, 1.0).to_color();
        let green = Hsva::new(1.0 / 3.0, 1.0, 1.0, 1.0).to_color();
        let blue = Hsva::new(2.0 / 3.0, 1.0, 1.0, 1.0).to_color();
        assert_eq!(red.to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(green.to_rgba8(), [0, 255, 0, 255]);
        assert_eq!(blue.to_rgba8(), [0, 0, 255, 255]);
    }

    #[test]
    fn hex_is_uppercase_and_optionally_carries_alpha() {
        let c = Hsva::new(0.0, 1.0, 1.0, 0.5);
        assert_eq!(c.hex(false), "#FF0000");
        assert_eq!(c.hex(true), "#FF000080");
    }

    #[test]
    fn parse_hex_accepts_every_css_length() {
        assert_eq!(parse_hex("#0af").map(|c| c.to_rgba8()), Some([0, 170, 255, 255]));
        assert_eq!(parse_hex("0AF").map(|c| c.to_rgba8()), Some([0, 170, 255, 255]));
        assert_eq!(parse_hex("#00AAFF").map(|c| c.to_rgba8()), Some([0, 170, 255, 255]));
        assert_eq!(parse_hex("#00AAFF80").map(|c| c.to_rgba8()), Some([0, 170, 255, 128]));
        assert_eq!(parse_hex("#00AAF").map(|c| c.to_rgba8()), None);
        assert_eq!(parse_hex("nonsense").map(|c| c.to_rgba8()), None);
    }

    #[test]
    fn a_black_value_keeps_the_hue_it_was_dragged_from() {
        // The property the whole `Hsva`-not-`Color` decision exists for: a drag
        // to the bottom of the square must not throw the hue away, or dragging
        // back out returns a different colour than the one that went in.
        let start = Hsva::new(0.55, 0.9, 0.0, 1.0);
        assert_eq!(start.to_color().to_rgba8(), [0, 0, 0, 255]);
        assert_eq!(start.with_sv(0.9, 1.0).h, 0.55);
    }

    #[test]
    fn the_sampled_square_agrees_with_the_colour_it_reports() {
        // The property the mesh exists for. A blend of white over red in linear
        // light lands on #FFBBBB where HSV says #FF8080, so the square is
        // sampled from `to_color` instead — which means every sample point on
        // it is, by construction, the colour a click there would report.
        let hue = 0.0;
        for (col, row) in [(0, 0), (8, 0), (16, 6), (8, 6), (4, 11)] {
            let u = col as f32 / SQUARE_COLS as f32;
            let v = row as f32 / SQUARE_ROWS as f32;
            let painted = Hsva { h: hue, s: u, v: 1.0 - v, a: 1.0 }.to_color();
            let reported = Hsva::new(hue, 1.0, 1.0, 1.0).with_sv(u, 1.0 - v).to_color();
            assert_eq!(painted.to_rgba8(), reported.to_rgba8(), "at ({col}, {row})");
        }
    }

    #[test]
    fn a_linear_blend_would_have_got_the_middle_of_the_square_wrong() {
        // Documents the bug the mesh removes, with the number, so that anyone
        // tempted to go back to two gradient overlays can see the size of it.
        // Half-saturated red is #FF8080; a 50 % white quad over red composited
        // in linear light is #FFBCBC, and the square would have shown that.
        let exact = Hsva::new(0.0, 0.5, 1.0, 1.0).to_color();
        assert_eq!(exact.to_rgba8()[1], 128);

        let blended = spherekit_core::linear_to_srgb(
            0.5 * spherekit_core::srgb_to_linear(0.0) + 0.5 * spherekit_core::srgb_to_linear(1.0),
        );
        assert_eq!((blended * 255.0).round() as u8, 188);
    }
}

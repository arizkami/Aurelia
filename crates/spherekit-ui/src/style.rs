//! Paint style: everything about an element's appearance that layout does not
//! decide.
//!
//! Layout answers *where*; this answers *what it looks like*. Keeping them in
//! separate structs is what makes the dirty-flag split meaningful — a colour
//! change touches this and marks `PAINT`, a size change touches the layout
//! style and marks `LAYOUT`.

use crate::element::InteractionState;
use smallvec::SmallVec;
use spherekit_core::{Brush, Color, Corners, Px, Rect, RoundedRect, Shadow};
use spherekit_render::{Canvas, Filter};

/// The pointer shape shown over an element.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Cursor {
    /// The platform default arrow.
    #[default]
    Default,
    /// A pointing hand, for clickable things.
    Pointer,
    /// A text I-beam.
    Text,
    /// A crosshair, for precise placement.
    Crosshair,
    /// A move cursor.
    Move,
    /// Grab, for a draggable thing not yet grabbed.
    Grab,
    /// Grabbing, while a drag is active.
    Grabbing,
    /// Horizontal resize.
    ResizeEw,
    /// Vertical resize.
    ResizeNs,
    /// Diagonal resize, north-east to south-west.
    ResizeNesw,
    /// Diagonal resize, north-west to south-east.
    ResizeNwse,
    /// Column resize, for a track divider.
    ColResize,
    /// Row resize.
    RowResize,
    /// Not allowed.
    NotAllowed,
    /// Busy.
    Wait,
    /// No cursor at all, for pointer-locked interactions such as a knob drag.
    None,
}

/// How an element's box is painted.
#[derive(Clone, Debug)]
pub struct PaintStyle {
    /// Background fill, or `None` for no background.
    pub background: Option<Brush>,
    /// Border colour. Ignored when `border_width` is zero.
    pub border_color: Color,
    /// Border width, drawn inset from the element's box.
    pub border_width: Px,
    /// Corner radii, clamped against the box at paint time.
    pub corner_radii: Corners<Px>,
    /// Drop shadows, painted behind the background in order.
    pub shadows: SmallVec<[Shadow; 2]>,
    /// Opacity multiplier for this element and its children.
    pub opacity: f32,
    /// Optional post-process for this element and its subtree.
    ///
    /// The tree turns this into an offscreen layer only when it is present;
    /// ordinary boxes keep the direct-to-parent fast path.
    pub filter: Option<Filter>,
    /// Clips children to this element's box.
    pub clip_content: bool,
    /// Cursor while the pointer is over this element.
    pub cursor: Option<Cursor>,
    /// Background used while the pointer is over this element.
    ///
    /// Held here rather than resolved by the caller so that hover feedback
    /// costs a paint and never a rebuild: the element already knows both
    /// states, and the framework just picks one.
    pub hover_background: Option<Brush>,
    /// Background used while a button is held on this element.
    pub active_background: Option<Brush>,
    /// Ring drawn when the element has keyboard focus.
    pub focus_ring: Option<FocusRing>,
}

impl Default for PaintStyle {
    fn default() -> Self {
        Self {
            background: None,
            border_color: Color::TRANSPARENT,
            border_width: Px::ZERO,
            corner_radii: Corners::ZERO,
            shadows: SmallVec::new(),
            opacity: 1.0,
            filter: None,
            clip_content: false,
            cursor: None,
            hover_background: None,
            active_background: None,
            focus_ring: None,
        }
    }
}

/// A keyboard focus indicator.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FocusRing {
    /// Ring colour.
    pub color: Color,
    /// Ring thickness.
    pub width: Px,
    /// Gap between the element's box and the ring.
    pub offset: Px,
}

impl Default for FocusRing {
    fn default() -> Self {
        Self { color: Color::hex(0x4C9AFF), width: Px(2.0), offset: Px(2.0) }
    }
}

impl PaintStyle {
    /// True when painting this style would produce nothing.
    ///
    /// The framework skips the whole box when so, which matters because the
    /// majority of elements in a real tree are pure layout containers with no
    /// appearance of their own.
    pub fn is_invisible(&self) -> bool {
        self.opacity <= 0.0
            || (self.background.is_none()
                && self.shadows.is_empty()
                && self.filter.is_none()
                && (self.border_width <= Px::ZERO || self.border_color.is_transparent())
                && self.focus_ring.is_none())
    }

    /// The background to use for the given interaction state.
    ///
    /// Active wins over hover, because a pointer held down is always also
    /// hovering and the pressed appearance is the more specific one.
    pub fn resolved_background(&self, state: InteractionState) -> Option<&Brush> {
        if state.disabled {
            return self.background.as_ref();
        }
        if state.active
            && let Some(b) = self.active_background.as_ref()
        {
            return Some(b);
        }
        if state.hovered
            && let Some(b) = self.hover_background.as_ref()
        {
            return Some(b);
        }
        self.background.as_ref()
    }

    /// Paints the element's box: shadows, background, border and focus ring.
    pub fn paint_box(&self, canvas: &mut Canvas<'_>, bounds: Rect<Px>, state: InteractionState) {
        if bounds.is_empty() || self.opacity <= 0.0 {
            return;
        }
        let radii = self.corner_radii.clamp_for(bounds.size);
        let shape = RoundedRect::new(bounds, radii);

        // Shadows first: they sit behind everything the element draws.
        for shadow in &self.shadows {
            canvas.draw_shadow(shape, shadow);
        }

        let background = self.resolved_background(state).cloned();
        let has_border = self.border_width > Px::ZERO && !self.border_color.is_transparent();

        if background.is_some() || has_border {
            canvas.quad(
                bounds,
                radii,
                background,
                if has_border { self.border_color } else { Color::TRANSPARENT },
                if has_border { self.border_width } else { Px::ZERO },
            );
        }

        // The focus ring is drawn outside the box, so it is not clipped by the
        // element's own corner radii and stays visible against a busy backdrop.
        if state.focused
            && let Some(ring) = self.focus_ring
        {
            let grown = bounds.outset(spherekit_core::Edges::all(ring.offset + ring.width * 0.5));
            let ring_radii = radii.map(|r| r + ring.offset + ring.width * 0.5);
            canvas.stroke_rounded_rect(RoundedRect::new(grown, ring_radii), ring.color, ring.width);
        }
    }
}

/// Fluent helpers for the interaction-dependent parts of a paint style.
pub trait StyledInteraction: crate::element::Styled {
    /// Background while hovered.
    fn hover_bg(mut self, brush: impl Into<Brush>) -> Self {
        self.paint_style_mut().hover_background = Some(brush.into());
        self
    }
    /// Background while pressed.
    fn active_bg(mut self, brush: impl Into<Brush>) -> Self {
        self.paint_style_mut().active_background = Some(brush.into());
        self
    }
    /// Focus ring.
    fn focus_ring(mut self, ring: FocusRing) -> Self {
        self.paint_style_mut().focus_ring = Some(ring);
        self
    }
}

impl<T: crate::element::Styled> StyledInteraction for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{ScaleFactor, Size, px, rect, size};
    use spherekit_render::{DrawCommand, Scene};

    fn state(hovered: bool, active: bool, focused: bool) -> InteractionState {
        InteractionState { hovered, active, focused, ..Default::default() }
    }

    fn scene() -> Scene {
        Scene::new(size(px(400.0), px(300.0)), ScaleFactor::IDENTITY)
    }

    #[test]
    fn a_bare_layout_container_paints_nothing() {
        assert!(PaintStyle::default().is_invisible());
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            PaintStyle::default().paint_box(
                &mut c,
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                InteractionState::default(),
            );
        }
        assert!(s.is_empty(), "an invisible style must not reach a vertex buffer");
    }

    #[test]
    fn a_background_produces_one_quad() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            let style = PaintStyle { background: Some(Color::RED.into()), ..Default::default() };
            style.paint_box(
                &mut c,
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                InteractionState::default(),
            );
        }
        assert_eq!(s.len(), 1);
        assert!(matches!(s.commands[0], DrawCommand::Quad(_)));
    }

    #[test]
    fn fill_and_border_are_one_quad_not_two() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            let style = PaintStyle {
                background: Some(Color::RED.into()),
                border_color: Color::WHITE,
                border_width: px(2.0),
                ..Default::default()
            };
            style.paint_box(
                &mut c,
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                InteractionState::default(),
            );
        }
        assert_eq!(s.len(), 1, "a bordered box must stay a single instance");
    }

    #[test]
    fn active_wins_over_hover() {
        // A pointer held down is always also hovering; the pressed appearance
        // is the more specific one and must win.
        let style = PaintStyle {
            background: Some(Color::hex(0x111111).into()),
            hover_background: Some(Color::hex(0x222222).into()),
            active_background: Some(Color::hex(0x333333).into()),
            ..Default::default()
        };
        let active = style.resolved_background(state(true, true, false)).unwrap();
        assert_eq!(active, &Brush::Solid(Color::hex(0x333333)));
        let hovered = style.resolved_background(state(true, false, false)).unwrap();
        assert_eq!(hovered, &Brush::Solid(Color::hex(0x222222)));
        let rest = style.resolved_background(state(false, false, false)).unwrap();
        assert_eq!(rest, &Brush::Solid(Color::hex(0x111111)));
    }

    #[test]
    fn a_disabled_element_ignores_hover_and_active() {
        let style = PaintStyle {
            background: Some(Color::hex(0x111111).into()),
            hover_background: Some(Color::hex(0x222222).into()),
            active_background: Some(Color::hex(0x333333).into()),
            ..Default::default()
        };
        let s =
            InteractionState { hovered: true, active: true, disabled: true, ..Default::default() };
        assert_eq!(style.resolved_background(s).unwrap(), &Brush::Solid(Color::hex(0x111111)));
    }

    #[test]
    fn a_missing_hover_background_falls_back_to_the_base() {
        let style = PaintStyle { background: Some(Color::RED.into()), ..Default::default() };
        assert_eq!(
            style.resolved_background(state(true, false, false)).unwrap(),
            &Brush::Solid(Color::RED)
        );
    }

    #[test]
    fn the_focus_ring_only_paints_when_focused() {
        let style = PaintStyle {
            background: Some(Color::RED.into()),
            focus_ring: Some(FocusRing::default()),
            ..Default::default()
        };
        let count = |st| {
            let mut s = scene();
            {
                let mut c = Canvas::new(&mut s);
                style.paint_box(&mut c, rect(px(10.0), px(10.0), px(50.0), px(20.0)), st);
            }
            s.len()
        };
        assert_eq!(count(state(false, false, false)), 1);
        assert_eq!(count(state(false, false, true)), 2, "focus adds the ring");
    }

    #[test]
    fn the_focus_ring_is_drawn_outside_the_element() {
        // Drawing it inside would hide it under adjacent content and make it
        // useless against a busy backdrop.
        let style = PaintStyle {
            focus_ring: Some(FocusRing { color: Color::BLUE, width: px(2.0), offset: px(2.0) }),
            ..Default::default()
        };
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            style.paint_box(
                &mut c,
                rect(px(50.0), px(50.0), px(40.0), px(20.0)),
                state(false, false, true),
            );
        }
        match &s.commands[0] {
            DrawCommand::Quad(q) => assert!(q.bounds.min_x() < px(50.0), "{:?}", q.bounds),
            other => panic!("expected a quad, got {other:?}"),
        }
    }

    #[test]
    fn shadows_are_painted_behind_the_background() {
        let style = PaintStyle {
            background: Some(Color::RED.into()),
            shadows: SmallVec::from_slice(&[Shadow::default()]),
            ..Default::default()
        };
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            style.paint_box(
                &mut c,
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                InteractionState::default(),
            );
        }
        assert!(matches!(s.commands[0], DrawCommand::Shadow { .. }), "shadow must be first");
        assert!(matches!(s.commands[1], DrawCommand::Quad(_)));
    }

    #[test]
    fn an_empty_box_paints_nothing() {
        let style = PaintStyle { background: Some(Color::RED.into()), ..Default::default() };
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            style.paint_box(
                &mut c,
                Rect::new(spherekit_core::Point::ZERO, Size::new(Px::ZERO, px(10.0))),
                InteractionState::default(),
            );
        }
        assert!(s.is_empty());
    }

    #[test]
    fn oversized_radii_are_clamped_to_the_box() {
        let style = PaintStyle {
            background: Some(Color::RED.into()),
            corner_radii: Corners::all(px(1.0e6)),
            ..Default::default()
        };
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            style.paint_box(
                &mut c,
                rect(px(0.0), px(0.0), px(40.0), px(20.0)),
                InteractionState::default(),
            );
        }
        match &s.commands[0] {
            DrawCommand::Quad(q) => {
                let r = q.radii.clamp_for(q.bounds.size);
                assert!(r.top_left <= px(10.0) + px(0.01), "{r:?}");
            }
            other => panic!("expected a quad, got {other:?}"),
        }
    }
}

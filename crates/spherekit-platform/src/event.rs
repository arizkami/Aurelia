//! SphereKit's own window event vocabulary.
//!
//! No backend type appears here. The winit backend translates into these types
//! at the boundary, which is what lets the backend be swapped for a native one
//! (or for a plug-in host's event pump) without touching a single widget.
//!
//! ## Units
//!
//! Pointer positions are delivered in **logical pixels** ([`Point<Px>`]), not
//! device pixels. The platform reports physical coordinates; the backend
//! divides by the window's current scale factor before the event is handed on.
//! Everything above this crate lays out in logical pixels, so converting once,
//! at the one place that knows the authoritative scale factor, is what keeps
//! hit testing correct on a 150 % display and across a monitor change.
//!
//! Window *sizes*, by contrast, are delivered in device pixels
//! ([`Size<DevicePx>`]): the surface has to be configured in physical pixels,
//! and rounding a logical size back to physical is exactly how a one-pixel
//! blurry seam appears at fractional scale factors. Use
//! [`crate::WindowState::logical_size`] when a logical size is wanted.

use std::path::PathBuf;

use spherekit_core::{DevicePx, Point, Px, ScaleFactor, Size};

use crate::keyboard::{Key, Modifiers, PhysicalKey};

/// Whether an input was pressed or released.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ElementState {
    /// The input went down.
    Pressed,
    /// The input came back up.
    Released,
}

impl ElementState {
    /// True when the input is down.
    #[inline]
    pub const fn is_pressed(self) -> bool {
        matches!(self, ElementState::Pressed)
    }
}

/// A mouse button.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum MouseButton {
    /// Primary button.
    Left,
    /// Secondary button, conventionally the context menu.
    Right,
    /// Middle button or wheel click.
    Middle,
    /// The "back" thumb button.
    Back,
    /// The "forward" thumb button.
    Forward,
    /// Any further button, by platform-assigned index.
    Other(u16),
}

/// How far a scroll gesture moved.
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum ScrollDelta {
    /// A wheel notch count, in lines. Discrete mice report this; one notch is
    /// conventionally three lines of text, but the consumer decides.
    Lines {
        /// Horizontal lines, positive to the right.
        x: f32,
        /// Vertical lines, positive when scrolling content up (finger down).
        y: f32,
    },
    /// A precise offset in logical pixels, from a trackpad or a high-resolution
    /// wheel. Converted from the platform's physical delta using the window's
    /// scale factor.
    Pixels {
        /// Horizontal offset.
        x: Px,
        /// Vertical offset.
        y: Px,
    },
}

impl ScrollDelta {
    /// Resolves to logical pixels, expanding line deltas at `line_height`.
    ///
    /// Kept explicit rather than baked in because the right line height is a
    /// property of the scrolled content, not of the platform.
    pub fn to_pixels(self, line_height: Px) -> (Px, Px) {
        match self {
            ScrollDelta::Lines { x, y } => (line_height * x, line_height * y),
            ScrollDelta::Pixels { x, y } => (x, y),
        }
    }
}

/// The phase of a touch or trackpad gesture.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum TouchPhase {
    /// Contact began.
    Started,
    /// Contact moved.
    Moved,
    /// Contact ended normally.
    Ended,
    /// The gesture was cancelled by the system; any in-progress interaction
    /// should be rolled back rather than committed.
    Cancelled,
}

/// Input-method composition events.
///
/// A text field must render [`ImeEvent::Preedit`] text as provisional (usually
/// underlined) and only insert on [`ImeEvent::Commit`]. Treating pre-edit text
/// as committed is what makes CJK input unusable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImeEvent {
    /// The IME is now active for this window; composition may begin.
    Enabled,
    /// Provisional composition text, with an optional selection range as byte
    /// offsets into that text.
    Preedit {
        /// The provisional text.
        text: String,
        /// Byte range of the cursor or selection inside `text`, when reported.
        cursor: Option<(usize, usize)>,
    },
    /// Composition finished; this text should be inserted.
    Commit(String),
    /// The IME is no longer active. Any pre-edit text must be discarded.
    Disabled,
}

/// The system's light or dark appearance.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Theme {
    /// Light appearance.
    Light,
    /// Dark appearance.
    Dark,
}

/// Something that happened to one window.
///
/// Delivered per window; the owning [`spherekit_core::WindowId`] is passed
/// alongside rather than embedded, so the same event value can be replayed
/// against another window in tests.
#[derive(Clone, PartialEq, Debug)]
#[non_exhaustive]
pub enum WindowEvent {
    /// The user asked to close the window. Nothing has closed yet: the
    /// application decides whether to honour it, which is what makes "you have
    /// unsaved changes" possible.
    CloseRequested,
    /// The window's drawable area changed, in device pixels. The surface must
    /// be reconfigured before the next frame.
    Resized(Size<DevicePx>),
    /// The window moved to a display with a different scale, or the display's
    /// scale changed. The physical size usually changes in the same breath, so
    /// a [`WindowEvent::Resized`] normally follows.
    ScaleFactorChanged(ScaleFactor),
    /// The window's contents must be redrawn.
    RedrawRequested,
    /// Keyboard focus was gained (`true`) or lost (`false`).
    ///
    /// Losing focus must clear any held modifier state: the key-up for a
    /// modifier held during an alt-tab is delivered to somebody else.
    Focused(bool),
    /// The pointer moved, in logical pixels relative to the window's top-left.
    CursorMoved(Point<Px>),
    /// The pointer entered the window.
    CursorEntered,
    /// The pointer left the window. Any hover state must be cleared: there is
    /// no further move event to do it.
    CursorLeft,
    /// A mouse button changed state.
    MouseInput {
        /// Which button.
        button: MouseButton,
        /// Pressed or released.
        state: ElementState,
        /// Modifiers held at the time.
        modifiers: Modifiers,
    },
    /// The wheel or trackpad scrolled.
    MouseWheel {
        /// How far.
        delta: ScrollDelta,
        /// Gesture phase; discrete wheels report [`TouchPhase::Moved`].
        phase: TouchPhase,
    },
    /// A key changed state.
    ///
    /// Text insertion does *not* come from here: a key that produces text also
    /// produces a [`WindowEvent::TextInput`], because the mapping from key to
    /// text depends on layout, dead keys and the IME.
    KeyboardInput {
        /// The layout-dependent key.
        key: Key,
        /// The layout-independent position, for rebindable controls.
        physical_key: PhysicalKey,
        /// Pressed or released.
        state: ElementState,
        /// True when this is an auto-repeat rather than a fresh press.
        repeat: bool,
        /// Modifiers held at the time.
        modifiers: Modifiers,
    },
    /// Text was produced by a key press. Insert this rather than deriving text
    /// from [`WindowEvent::KeyboardInput`].
    TextInput(String),
    /// An input-method composition event.
    Ime(ImeEvent),
    /// A file was dropped on the window.
    FileDropped(PathBuf),
    /// A file is being dragged over the window. One event per file.
    FileHovered(PathBuf),
    /// The drag left the window without dropping; clear any drop highlight.
    FileHoverCancelled,
    /// The system appearance changed.
    ThemeChanged(Theme),
    /// The window moved, in device pixels relative to the desktop origin.
    Moved(Point<DevicePx>),
    /// The window became fully hidden (`true`) or visible again (`false`).
    ///
    /// An occluded window must stop drawing: its surface cannot be acquired,
    /// and on a laptop a plug-in editor that keeps rendering behind a maximised
    /// arrangement window is a measurable battery cost.
    Occluded(bool),
    /// Modifier state changed without any other key event.
    ModifiersChanged(Modifiers),
    /// The window has been destroyed. No further events will arrive for it.
    Destroyed,
}

impl WindowEvent {
    /// True when this event, on its own, means the window's contents are stale.
    ///
    /// Used by the default redraw policy. Deliberately conservative: input
    /// events are *not* included, because whether a click changes anything is
    /// the application's business, not the platform's.
    pub const fn invalidates_contents(&self) -> bool {
        matches!(
            self,
            WindowEvent::RedrawRequested
                | WindowEvent::Resized(_)
                | WindowEvent::ScaleFactorChanged(_)
                | WindowEvent::ThemeChanged(_)
                | WindowEvent::Focused(_)
        )
    }

    /// True when this event ends the window's life.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, WindowEvent::Destroyed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{point, px, size};

    #[test]
    fn scroll_lines_expand_at_the_given_line_height() {
        let d = ScrollDelta::Lines { x: 0.0, y: -3.0 };
        let (x, y) = d.to_pixels(px(18.0));
        assert_eq!(x, px(0.0));
        assert_eq!(y, px(-54.0));
    }

    #[test]
    fn pixel_scroll_ignores_line_height() {
        let d = ScrollDelta::Pixels { x: px(2.5), y: px(-7.25) };
        assert_eq!(d.to_pixels(px(1000.0)), (px(2.5), px(-7.25)));
    }

    #[test]
    fn element_state_predicate() {
        assert!(ElementState::Pressed.is_pressed());
        assert!(!ElementState::Released.is_pressed());
    }

    #[test]
    fn only_content_affecting_events_invalidate() {
        assert!(WindowEvent::RedrawRequested.invalidates_contents());
        assert!(WindowEvent::Resized(size(DevicePx(8), DevicePx(8))).invalidates_contents());
        assert!(WindowEvent::ScaleFactorChanged(ScaleFactor::new(2.0)).invalidates_contents());
        assert!(WindowEvent::ThemeChanged(Theme::Dark).invalidates_contents());
        // A pointer move is not, by itself, a reason to repaint.
        assert!(!WindowEvent::CursorMoved(point(px(1.0), px(1.0))).invalidates_contents());
        assert!(!WindowEvent::CloseRequested.invalidates_contents());
        assert!(!WindowEvent::TextInput("a".into()).invalidates_contents());
    }

    #[test]
    fn destroyed_is_the_only_terminal_event() {
        assert!(WindowEvent::Destroyed.is_terminal());
        assert!(!WindowEvent::CloseRequested.is_terminal());
        assert!(!WindowEvent::Occluded(true).is_terminal());
    }

    #[test]
    fn events_compare_structurally_so_they_can_be_asserted_in_tests() {
        let a = WindowEvent::MouseInput {
            button: MouseButton::Other(7),
            state: ElementState::Pressed,
            modifiers: Modifiers::SHIFT,
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, WindowEvent::CloseRequested);
    }

    #[test]
    fn ime_preedit_carries_its_cursor_range() {
        let e = ImeEvent::Preedit { text: "\u{304B}\u{306A}".into(), cursor: Some((0, 3)) };
        match e {
            ImeEvent::Preedit { text, cursor } => {
                assert_eq!(text.len(), 6);
                assert_eq!(cursor, Some((0, 3)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

//! The winit backend: the platform window and the translation tables.
//!
//! Nothing here escapes into a public signature elsewhere. The translation
//! functions are `pub(crate)`, and [`Window`] exposes only SphereKit types plus
//! the `raw-window-handle` traits, which are the intended seam for the
//! renderer.
//!
//! The key and key-code tables are generated from the same variant lists that
//! declare [`crate::keyboard::Key`] and [`crate::keyboard::KeyCode`], so a name
//! cannot exist in one and be missing from the other.

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};
use smallvec::SmallVec;
use spherekit_core::{
    DevicePx, PlatformError, Point, Px, Rect, ScaleFactor, Size, WindowId, point, px, size,
};

use crate::cursor::Cursor;
use crate::event::{
    ElementState, ImeEvent, MouseButton, ScrollDelta, Theme, TouchPhase, WindowEvent,
};
use crate::keyboard::{
    Key, KeyCode, KeyText, Modifiers, NamedKey, PhysicalKey, Scancode, key_code_names,
    named_key_names,
};
use crate::monitor::{MonitorInfo, MonitorList, RefreshRate, VideoMode};
use crate::scheduler::ControlFlow;
use crate::window::{WindowAttributes, WindowBackdrop, WindowChrome, WindowLevel, WindowPosition};

/// How many SphereKit events one platform event can expand into.
///
/// A key press produces at most two (the key event and its text), so the
/// scratch buffer never allocates on the input path.
pub(crate) type EventBuf = SmallVec<[WindowEvent; 2]>;

// ---------------------------------------------------------------------------
// Keyboard translation
// ---------------------------------------------------------------------------

/// Generates the physical-key table from the same list that declares
/// [`KeyCode`].
macro_rules! impl_key_code_table {
    ($($name:ident),* $(,)?) => {
        fn key_code_from_winit(code: ::winit::keyboard::KeyCode) -> Option<KeyCode> {
            match code {
                $(::winit::keyboard::KeyCode::$name => Some(KeyCode::$name),)*
                other => winit_key_code_f_number(other).map(KeyCode::F),
            }
        }
    };
}
key_code_names!(impl_key_code_table);

/// Generates the named-key table from the same list that declares
/// [`NamedKey`].
macro_rules! impl_named_key_table {
    ($($name:ident),* $(,)?) => {
        fn named_key_from_winit(key: ::winit::keyboard::NamedKey) -> Option<NamedKey> {
            match key {
                $(::winit::keyboard::NamedKey::$name => Some(NamedKey::$name),)*
                other => winit_named_key_f_number(other).map(NamedKey::F),
            }
        }
    };
}
named_key_names!(impl_named_key_table);

/// The function-key number of a physical key position, if it is one.
fn winit_key_code_f_number(code: ::winit::keyboard::KeyCode) -> Option<u8> {
    use ::winit::keyboard::KeyCode as K;
    Some(match code {
        K::F1 => 1,
        K::F2 => 2,
        K::F3 => 3,
        K::F4 => 4,
        K::F5 => 5,
        K::F6 => 6,
        K::F7 => 7,
        K::F8 => 8,
        K::F9 => 9,
        K::F10 => 10,
        K::F11 => 11,
        K::F12 => 12,
        K::F13 => 13,
        K::F14 => 14,
        K::F15 => 15,
        K::F16 => 16,
        K::F17 => 17,
        K::F18 => 18,
        K::F19 => 19,
        K::F20 => 20,
        K::F21 => 21,
        K::F22 => 22,
        K::F23 => 23,
        K::F24 => 24,
        K::F25 => 25,
        K::F26 => 26,
        K::F27 => 27,
        K::F28 => 28,
        K::F29 => 29,
        K::F30 => 30,
        K::F31 => 31,
        K::F32 => 32,
        K::F33 => 33,
        K::F34 => 34,
        K::F35 => 35,
        _ => return None,
    })
}

/// The function-key number of a named key, if it is one.
fn winit_named_key_f_number(key: ::winit::keyboard::NamedKey) -> Option<u8> {
    use ::winit::keyboard::NamedKey as N;
    Some(match key {
        N::F1 => 1,
        N::F2 => 2,
        N::F3 => 3,
        N::F4 => 4,
        N::F5 => 5,
        N::F6 => 6,
        N::F7 => 7,
        N::F8 => 8,
        N::F9 => 9,
        N::F10 => 10,
        N::F11 => 11,
        N::F12 => 12,
        N::F13 => 13,
        N::F14 => 14,
        N::F15 => 15,
        N::F16 => 16,
        N::F17 => 17,
        N::F18 => 18,
        N::F19 => 19,
        N::F20 => 20,
        N::F21 => 21,
        N::F22 => 22,
        N::F23 => 23,
        N::F24 => 24,
        N::F25 => 25,
        N::F26 => 26,
        N::F27 => 27,
        N::F28 => 28,
        N::F29 => 29,
        N::F30 => 30,
        N::F31 => 31,
        N::F32 => 32,
        N::F33 => 33,
        N::F34 => 34,
        N::F35 => 35,
        _ => return None,
    })
}

/// Translates a platform raw key identifier.
///
/// The value is kept even when SphereKit has no name for the key: a control
/// surface must be able to bind it, and press/release must still pair up.
pub(crate) fn scancode_from_winit(code: ::winit::keyboard::NativeKeyCode) -> Scancode {
    use ::winit::keyboard::NativeKeyCode as N;
    match code {
        N::Unidentified => Scancode::UNIDENTIFIED,
        N::Android(v) | N::Xkb(v) => Scancode::new(v),
        N::MacOS(v) | N::Windows(v) => Scancode::new(v as u32),
    }
}

/// Translates a physical key position.
pub(crate) fn physical_key_from_winit(key: ::winit::keyboard::PhysicalKey) -> PhysicalKey {
    match key {
        ::winit::keyboard::PhysicalKey::Code(c) => match key_code_from_winit(c) {
            Some(code) => PhysicalKey::Code(code),
            // A position winit names but SphereKit does not. There is no raw value
            // to fall back on here, so the key is unbindable rather than
            // wrongly reported as some other key.
            None => PhysicalKey::Unidentified(Scancode::UNIDENTIFIED),
        },
        ::winit::keyboard::PhysicalKey::Unidentified(n) => {
            PhysicalKey::Unidentified(scancode_from_winit(n))
        }
    }
}

/// Translates a logical key.
pub(crate) fn key_from_winit(key: &::winit::keyboard::Key) -> Key {
    match key {
        ::winit::keyboard::Key::Named(n) => match named_key_from_winit(*n) {
            Some(named) => Key::Named(named),
            None => Key::Unidentified,
        },
        ::winit::keyboard::Key::Character(s) => Key::Character(KeyText::new(s.as_str())),
        ::winit::keyboard::Key::Dead(c) => Key::Dead(*c),
        ::winit::keyboard::Key::Unidentified(_) => Key::Unidentified,
    }
}

/// Translates modifier state.
pub(crate) fn modifiers_from_winit(state: ::winit::keyboard::ModifiersState) -> Modifiers {
    let mut out = Modifiers::empty();
    out.set(Modifiers::SHIFT, state.shift_key());
    out.set(Modifiers::CTRL, state.control_key());
    out.set(Modifiers::ALT, state.alt_key());
    out.set(Modifiers::SUPER, state.super_key());
    out
}

/// The insertable text of a key press, or `None` when there is none.
///
/// Platforms report control characters here — `"\r"` for <kbd>Enter</kbd>,
/// `"\u{8}"` for <kbd>Backspace</kbd> — and inserting those into a text field
/// puts literal control codes in the user's project. Those keys are handled
/// through the key event instead, so anything that is entirely control
/// characters is dropped.
pub(crate) fn insertable_text(text: &str) -> Option<&str> {
    if text.is_empty() || text.chars().all(char::is_control) { None } else { Some(text) }
}

// ---------------------------------------------------------------------------
// Event translation
// ---------------------------------------------------------------------------

fn element_state_from_winit(s: ::winit::event::ElementState) -> ElementState {
    match s {
        ::winit::event::ElementState::Pressed => ElementState::Pressed,
        ::winit::event::ElementState::Released => ElementState::Released,
    }
}

fn mouse_button_from_winit(b: ::winit::event::MouseButton) -> MouseButton {
    match b {
        ::winit::event::MouseButton::Left => MouseButton::Left,
        ::winit::event::MouseButton::Right => MouseButton::Right,
        ::winit::event::MouseButton::Middle => MouseButton::Middle,
        ::winit::event::MouseButton::Back => MouseButton::Back,
        ::winit::event::MouseButton::Forward => MouseButton::Forward,
        ::winit::event::MouseButton::Other(n) => MouseButton::Other(n),
    }
}

fn touch_phase_from_winit(p: ::winit::event::TouchPhase) -> TouchPhase {
    match p {
        ::winit::event::TouchPhase::Started => TouchPhase::Started,
        ::winit::event::TouchPhase::Moved => TouchPhase::Moved,
        ::winit::event::TouchPhase::Ended => TouchPhase::Ended,
        ::winit::event::TouchPhase::Cancelled => TouchPhase::Cancelled,
    }
}

/// Translates a scroll delta, converting a pixel delta to logical pixels.
fn scroll_delta_from_winit(
    delta: ::winit::event::MouseScrollDelta,
    scale: ScaleFactor,
) -> ScrollDelta {
    match delta {
        ::winit::event::MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines { x, y },
        ::winit::event::MouseScrollDelta::PixelDelta(p) => {
            ScrollDelta::Pixels { x: px(p.x as f32 / scale.get()), y: px(p.y as f32 / scale.get()) }
        }
    }
}

fn ime_from_winit(ime: ::winit::event::Ime) -> ImeEvent {
    match ime {
        ::winit::event::Ime::Enabled => ImeEvent::Enabled,
        ::winit::event::Ime::Preedit(text, cursor) => ImeEvent::Preedit { text, cursor },
        ::winit::event::Ime::Commit(text) => ImeEvent::Commit(text),
        ::winit::event::Ime::Disabled => ImeEvent::Disabled,
    }
}

pub(crate) fn theme_from_winit(theme: ::winit::window::Theme) -> Theme {
    match theme {
        ::winit::window::Theme::Light => Theme::Light,
        ::winit::window::Theme::Dark => Theme::Dark,
    }
}

fn theme_to_winit(theme: Theme) -> ::winit::window::Theme {
    match theme {
        Theme::Light => ::winit::window::Theme::Light,
        Theme::Dark => ::winit::window::Theme::Dark,
    }
}

/// Converts a platform physical size, saturating rather than wrapping.
///
/// A `u32` extent larger than `i32::MAX` is not a real window; wrapping it into
/// a negative would turn "absurdly large" into "empty", which fails much later
/// and much more confusingly.
fn physical_size(s: ::winit::dpi::PhysicalSize<u32>) -> Size<DevicePx> {
    size(
        DevicePx(s.width.min(i32::MAX as u32) as i32),
        DevicePx(s.height.min(i32::MAX as u32) as i32),
    )
}

/// Translates one key event into a key event plus, when it produced text, a
/// text event.
///
/// Takes the pieces rather than a `winit::event::KeyEvent` because that type
/// has a private platform-specific field and so cannot be constructed outside
/// winit — which would make this, the most bug-prone translation in the crate,
/// the one thing that could not be unit-tested.
pub(crate) fn translate_key_input(
    logical_key: &::winit::keyboard::Key,
    physical_key: ::winit::keyboard::PhysicalKey,
    text: Option<&str>,
    state: ::winit::event::ElementState,
    repeat: bool,
    is_synthetic: bool,
    modifiers: Modifiers,
    out: &mut EventBuf,
) {
    let state = element_state_from_winit(state);
    out.push(WindowEvent::KeyboardInput {
        key: key_from_winit(logical_key),
        physical_key: physical_key_from_winit(physical_key),
        state,
        repeat,
        modifiers,
    });
    // Synthetic events are winit's reconstruction of keys already held when the
    // window gained focus. They keep a key-state map honest, but turning them
    // into text would insert characters the user typed into another window.
    // A shortcut such as Ctrl+A produces a logical character in winit too,
    // but it must not become a second text event after the key event has been
    // handled. Keep Ctrl+Alt available for AltGr layouts, where the same
    // modifier chord is how users type characters such as `@`.
    let shortcut_modifier = modifiers.contains(Modifiers::SUPER)
        || (modifiers.contains(Modifiers::CTRL) && !modifiers.contains(Modifiers::ALT));
    if state.is_pressed() && !is_synthetic && !shortcut_modifier {
        if let Some(text) = text.and_then(insertable_text) {
            out.push(WindowEvent::TextInput(text.to_owned()));
        }
    }
}

/// Translates one platform event into zero, one or two SphereKit events.
///
/// `scale` must be the window's *current* scale factor: it is what converts
/// pointer positions to logical pixels, and using a stale one puts hit testing
/// off by the ratio of the two.
///
/// `modifiers` is the chord the caller has been tracking, since the platform
/// reports it separately from the events it applies to.
pub(crate) fn translate_window_event(
    event: ::winit::event::WindowEvent,
    scale: ScaleFactor,
    modifiers: Modifiers,
    out: &mut EventBuf,
) {
    use ::winit::event::WindowEvent as W;
    match event {
        W::Resized(s) => out.push(WindowEvent::Resized(physical_size(s))),
        W::Moved(p) => out.push(WindowEvent::Moved(point(DevicePx(p.x), DevicePx(p.y)))),
        W::CloseRequested => out.push(WindowEvent::CloseRequested),
        W::Destroyed => out.push(WindowEvent::Destroyed),
        W::DroppedFile(path) => out.push(WindowEvent::FileDropped(path)),
        W::HoveredFile(path) => out.push(WindowEvent::FileHovered(path)),
        W::HoveredFileCancelled => out.push(WindowEvent::FileHoverCancelled),
        W::Focused(f) => out.push(WindowEvent::Focused(f)),
        W::KeyboardInput { event, is_synthetic, .. } => translate_key_input(
            &event.logical_key,
            event.physical_key,
            event.text.as_deref(),
            event.state,
            event.repeat,
            is_synthetic,
            modifiers,
            out,
        ),
        W::ModifiersChanged(m) => {
            out.push(WindowEvent::ModifiersChanged(modifiers_from_winit(m.state())))
        }
        W::Ime(ime) => out.push(WindowEvent::Ime(ime_from_winit(ime))),
        W::CursorMoved { position, .. } => out.push(WindowEvent::CursorMoved(point(
            px(position.x as f32 / scale.get()),
            px(position.y as f32 / scale.get()),
        ))),
        W::CursorEntered { .. } => out.push(WindowEvent::CursorEntered),
        W::CursorLeft { .. } => out.push(WindowEvent::CursorLeft),
        W::MouseWheel { delta, phase, .. } => out.push(WindowEvent::MouseWheel {
            delta: scroll_delta_from_winit(delta, scale),
            phase: touch_phase_from_winit(phase),
        }),
        W::MouseInput { state, button, .. } => out.push(WindowEvent::MouseInput {
            button: mouse_button_from_winit(button),
            state: element_state_from_winit(state),
            modifiers,
        }),
        W::ScaleFactorChanged { scale_factor, .. } => {
            out.push(WindowEvent::ScaleFactorChanged(ScaleFactor::new(scale_factor as f32)))
        }
        W::ThemeChanged(t) => out.push(WindowEvent::ThemeChanged(theme_from_winit(t))),
        W::Occluded(o) => out.push(WindowEvent::Occluded(o)),
        W::RedrawRequested => out.push(WindowEvent::RedrawRequested),
        // Touch, pen pressure, pinch/pan/rotate gestures and raw axis motion
        // have no SphereKit equivalent yet. Dropping them is deliberate: a
        // half-translated gesture is worse than none.
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Cursors, monitors, attributes
// ---------------------------------------------------------------------------

/// Maps a SphereKit cursor to the platform icon, or `None` for
/// [`Cursor::Hidden`], which is a visibility change rather than a shape.
pub(crate) fn cursor_to_winit(cursor: Cursor) -> Option<::winit::window::CursorIcon> {
    use ::winit::window::CursorIcon as C;
    Some(match cursor {
        Cursor::Default => C::Default,
        Cursor::ContextMenu => C::ContextMenu,
        Cursor::Help => C::Help,
        Cursor::Pointer => C::Pointer,
        Cursor::Progress => C::Progress,
        Cursor::Wait => C::Wait,
        Cursor::Cell => C::Cell,
        Cursor::Crosshair => C::Crosshair,
        Cursor::Text => C::Text,
        Cursor::VerticalText => C::VerticalText,
        Cursor::Alias => C::Alias,
        Cursor::Copy => C::Copy,
        Cursor::Move => C::Move,
        Cursor::NoDrop => C::NoDrop,
        Cursor::NotAllowed => C::NotAllowed,
        Cursor::Grab => C::Grab,
        Cursor::Grabbing => C::Grabbing,
        Cursor::EResize => C::EResize,
        Cursor::NResize => C::NResize,
        Cursor::NeResize => C::NeResize,
        Cursor::NwResize => C::NwResize,
        Cursor::SResize => C::SResize,
        Cursor::SeResize => C::SeResize,
        Cursor::SwResize => C::SwResize,
        Cursor::WResize => C::WResize,
        Cursor::EwResize => C::EwResize,
        Cursor::NsResize => C::NsResize,
        Cursor::NeswResize => C::NeswResize,
        Cursor::NwseResize => C::NwseResize,
        Cursor::ColResize => C::ColResize,
        Cursor::RowResize => C::RowResize,
        Cursor::AllScroll => C::AllScroll,
        Cursor::ZoomIn => C::ZoomIn,
        Cursor::ZoomOut => C::ZoomOut,
        Cursor::Hidden => return None,
    })
}

/// Maps SphereKit's control flow onto the platform loop's.
///
/// This is the last step of the frame scheduler: `Wait` is the zero-CPU state,
/// `WaitUntil` is the paced state, and `Poll` is the only one that spins.
pub(crate) fn control_flow_to_winit(cf: ControlFlow) -> ::winit::event_loop::ControlFlow {
    match cf {
        ControlFlow::Wait => ::winit::event_loop::ControlFlow::Wait,
        ControlFlow::Poll => ::winit::event_loop::ControlFlow::Poll,
        ControlFlow::WaitUntil(t) => ::winit::event_loop::ControlFlow::WaitUntil(t),
    }
}

fn window_level_to_winit(level: WindowLevel) -> ::winit::window::WindowLevel {
    match level {
        WindowLevel::AlwaysOnBottom => ::winit::window::WindowLevel::AlwaysOnBottom,
        WindowLevel::Normal => ::winit::window::WindowLevel::Normal,
        WindowLevel::AlwaysOnTop => ::winit::window::WindowLevel::AlwaysOnTop,
    }
}

fn logical(s: Size<Px>) -> ::winit::dpi::LogicalSize<f64> {
    ::winit::dpi::LogicalSize::new(s.width.get() as f64, s.height.get() as f64)
}

/// Describes a display.
pub(crate) fn monitor_info(
    handle: &::winit::monitor::MonitorHandle,
    primary: Option<&::winit::monitor::MonitorHandle>,
) -> MonitorInfo {
    let position = handle.position();
    MonitorInfo {
        name: handle.name(),
        position: point(DevicePx(position.x), DevicePx(position.y)),
        size: physical_size(handle.size()),
        scale_factor: ScaleFactor::new(handle.scale_factor() as f32),
        refresh_rate: handle.refresh_rate_millihertz().map(RefreshRate::from_millihertz),
        is_primary: primary == Some(handle),
        video_modes: handle
            .video_modes()
            .map(|m| VideoMode {
                size: physical_size(m.size()),
                bit_depth: m.bit_depth(),
                refresh_rate: RefreshRate::from_millihertz(m.refresh_rate_millihertz()),
            })
            .collect(),
    }
}

/// Snapshots every attached display.
pub(crate) fn monitor_list(
    monitors: impl Iterator<Item = ::winit::monitor::MonitorHandle>,
    primary: Option<&::winit::monitor::MonitorHandle>,
) -> MonitorList {
    MonitorList::new(monitors.map(|m| monitor_info(&m, primary)).collect())
}

/// Builds platform window attributes.
///
/// # Safety
///
/// When `attrs.parent` is set, its handle must satisfy the contract of
/// [`crate::ForeignWindowHandle`]: a live window that outlives the window being
/// created.
pub(crate) unsafe fn to_winit_attributes(
    attrs: &WindowAttributes,
) -> ::winit::window::WindowAttributes {
    let (min, max) = attrs.resolved_size_limits();
    let mut w = ::winit::window::Window::default_attributes()
        .with_title(attrs.title.clone())
        .with_inner_size(logical(attrs.resolved_inner_size()))
        .with_resizable(attrs.resizable)
        // `Custom` keeps the real WS_CAPTION | WS_SIZEBOX styles so that snap,
        // the drop shadow and winit's own WM_GETMINMAXINFO arithmetic all keep
        // working; the caption is removed later by the subclass answering
        // WM_NCCALCSIZE. Only `None` actually asks winit to strip the frame.
        .with_decorations(attrs.chrome != WindowChrome::None)
        .with_transparent(attrs.transparent)
        .with_window_level(window_level_to_winit(attrs.level))
        .with_visible(attrs.visible)
        .with_maximized(attrs.maximized)
        .with_active(attrs.active)
        .with_theme(attrs.theme.map(theme_to_winit));
    if let Some(min) = min {
        w = w.with_min_inner_size(logical(min));
    }
    if let Some(max) = max {
        w = w.with_max_inner_size(logical(max));
    }
    match attrs.position {
        // "Centred" is resolved by the platform: it knows which display the
        // window will land on, and we do not until it exists.
        Some(WindowPosition::Centered) | None => {}
        Some(WindowPosition::Logical(p)) => {
            w = w.with_position(::winit::dpi::LogicalPosition::new(
                p.x.get() as f64,
                p.y.get() as f64,
            ));
        }
        Some(WindowPosition::Physical(p)) => {
            w = w.with_position(::winit::dpi::PhysicalPosition::new(p.x.get(), p.y.get()));
        }
    }
    if let Some(parent) = &attrs.parent {
        // SAFETY: forwarded from this function's own safety contract, which the
        // caller accepted when it constructed the `ForeignWindowHandle`.
        w = unsafe { w.with_parent_window(Some(parent.raw_window_handle())) };
    }
    w
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

/// A window SphereKit created and owns.
///
/// Hold it in an `Arc`: the renderer needs a `'static` handle to build a
/// surface from, and the surface must not outlive the window. `Arc<Window>`
/// implements [`HasWindowHandle`] and [`HasDisplayHandle`] through the blanket
/// impls, so it can be handed straight to a GPU backend.
///
/// Dropping the last `Arc` destroys the platform window, so drop every surface
/// built from it first.
pub struct Window {
    /// The platform window.
    inner: ::winit::window::Window,
    /// The registry-assigned identity, stable for the window's whole life.
    id: WindowId,
    /// State shared with the custom-frame window procedure.
    ///
    /// Present only where there is a custom frame to run: `None` on a platform
    /// with no such mechanism, and on a window whose chrome was never anything
    /// but [`WindowChrome::System`].
    #[cfg(windows)]
    chrome: Option<std::sync::Arc<super::ffi::ChromeState>>,
}

impl core::fmt::Debug for Window {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Window")
            .field("id", &self.id)
            .field("physical_size", &self.physical_size())
            .field("scale_factor", &self.scale_factor())
            .finish()
    }
}

impl Window {
    /// Creates a platform window.
    ///
    /// Only reachable from inside the event loop, because that is the only
    /// place the platform lets a window be created.
    pub(crate) fn create(
        event_loop: &::winit::event_loop::ActiveEventLoop,
        attrs: &WindowAttributes,
        id: WindowId,
    ) -> Result<Self, PlatformError> {
        // SAFETY: `attrs.parent`, if set, was built through the unsafe
        // constructor of `ForeignWindowHandle`, whose contract is exactly what
        // `to_winit_attributes` requires.
        let winit_attrs = unsafe { to_winit_attributes(attrs) };
        let inner = event_loop
            .create_window(winit_attrs)
            .map_err(|e| PlatformError::WindowCreation(e.to_string()))?;

        #[cfg(windows)]
        let chrome = install_chrome(&inner, attrs);
        #[cfg(windows)]
        let window = Self { inner, id, chrome };
        #[cfg(not(windows))]
        let window = Self { inner, id };
        Ok(window)
    }

    /// The platform's own window id, for routing incoming events.
    pub(crate) fn winit_id(&self) -> ::winit::window::WindowId {
        self.inner.id()
    }

    /// This window's stable identity.
    #[inline]
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// The window's current scale factor.
    #[inline]
    pub fn scale_factor(&self) -> ScaleFactor {
        ScaleFactor::new(self.inner.scale_factor() as f32)
    }

    /// The drawable extent in device pixels. Configure the GPU surface with
    /// this, never with the logical size.
    #[inline]
    pub fn physical_size(&self) -> Size<DevicePx> {
        physical_size(self.inner.inner_size())
    }

    /// The drawable extent in logical pixels, which is what layout uses.
    pub fn inner_size(&self) -> Size<Px> {
        let scale = self.scale_factor();
        let physical = self.physical_size();
        size(scale.to_logical(physical.width), scale.to_logical(physical.height))
    }

    /// The outer position in desktop device pixels, when the platform reports
    /// one. Wayland, for instance, never does.
    pub fn outer_position(&self) -> Option<Point<DevicePx>> {
        self.inner.outer_position().ok().map(|p| point(DevicePx(p.x), DevicePx(p.y)))
    }

    /// Moves the window.
    pub fn set_outer_position(&self, position: Point<DevicePx>) {
        self.inner.set_outer_position(::winit::dpi::PhysicalPosition::new(
            position.x.get(),
            position.y.get(),
        ));
    }

    /// Asks for a new inner size, returning the size actually applied when the
    /// platform can answer synchronously. On the platforms that cannot, a
    /// `Resized` event follows instead.
    ///
    /// Under [`WindowChrome::Custom`] the request is reduced by the caption the
    /// custom frame reclaimed. The platform still believes there is a title bar
    /// and sizes the outer window for one, while the subclass has already given
    /// that strip back to the client — so an uncompensated request comes out
    /// about thirty logical pixels too tall at 96 dpi, and more at higher DPI.
    pub fn request_inner_size(&self, size: Size<Px>) -> Option<Size<DevicePx>> {
        self.inner.request_inner_size(logical(self.compensate_for_caption(size))).map(physical_size)
    }

    /// Removes the reclaimed caption from a requested inner size.
    fn compensate_for_caption(&self, size: Size<Px>) -> Size<Px> {
        #[cfg(windows)]
        {
            if self.chrome() == WindowChrome::Custom
                && let Some(hwnd) = self.hwnd()
            {
                let metrics = super::ffi::frame_metrics(super::ffi::window_dpi(hwnd));
                let scale = self.inner.scale_factor() as f32;
                let reclaimed = metrics.reclaimed_top() as f32 / scale.max(0.01);
                // Never below one pixel: a window whose height collapses to zero
                // cannot be recovered by resizing it.
                let height = (size.height.get() - reclaimed).max(1.0);
                return Size::new(size.width, Px(height));
            }
        }
        size
    }

    /// Asks the platform to deliver a redraw event for this window.
    ///
    /// This is the zero-CPU path: the loop stays blocked until the platform
    /// delivers the event, rather than polling.
    #[inline]
    pub fn request_redraw(&self) {
        self.inner.request_redraw();
    }

    /// Tells the compositor a frame is about to be presented.
    ///
    /// Must be called immediately before the GPU present, not earlier: on
    /// Wayland it is what lets the compositor schedule the frame callback
    /// correctly, and calling it too early costs a frame of latency.
    #[inline]
    pub fn pre_present_notify(&self) {
        self.inner.pre_present_notify();
    }

    /// Sets the cursor shape, hiding it for [`Cursor::Hidden`].
    pub fn set_cursor(&self, cursor: Cursor) {
        match cursor_to_winit(cursor) {
            Some(icon) => {
                self.inner.set_cursor(icon);
                self.inner.set_cursor_visible(true);
            }
            None => self.inner.set_cursor_visible(false),
        }
    }

    /// Sets the title bar text.
    #[inline]
    pub fn set_title(&self, title: &str) {
        self.inner.set_title(title);
    }

    /// The title bar text.
    #[inline]
    pub fn title(&self) -> String {
        self.inner.title()
    }

    /// Enables or disables input-method composition.
    ///
    /// Off by default on every platform. A text field must turn it on when it
    /// takes focus and off when it loses it, or CJK input silently does not
    /// work — one of the most common bugs in hand-rolled plug-in UIs.
    #[inline]
    pub fn set_ime_allowed(&self, allowed: bool) {
        self.inner.set_ime_allowed(allowed);
    }

    /// Tells the IME where the caret is, in logical pixels relative to the
    /// window, so the candidate window does not cover the text being composed.
    pub fn set_ime_cursor_area(&self, area: Rect<Px>) {
        self.inner.set_ime_cursor_area(
            ::winit::dpi::LogicalPosition::new(
                area.origin.x.get() as f64,
                area.origin.y.get() as f64,
            ),
            logical(area.size),
        );
    }

    /// Shows or hides the window.
    #[inline]
    pub fn set_visible(&self, visible: bool) {
        self.inner.set_visible(visible);
    }

    /// Whether the window is mapped, when the platform can say.
    #[inline]
    pub fn is_visible(&self) -> Option<bool> {
        self.inner.is_visible()
    }

    /// Sets whether the user may resize the window.
    #[inline]
    pub fn set_resizable(&self, resizable: bool) {
        self.inner.set_resizable(resizable);
    }

    /// Sets the minimum inner size.
    pub fn set_min_inner_size(&self, min: Option<Size<Px>>) {
        self.inner.set_min_inner_size(min.map(logical));
    }

    /// Sets the maximum inner size.
    pub fn set_max_inner_size(&self, max: Option<Size<Px>>) {
        self.inner.set_max_inner_size(max.map(logical));
    }

    /// Shows or hides the platform decorations.
    #[inline]
    pub fn set_decorations(&self, decorations: bool) {
        self.inner.set_decorations(decorations);
    }

    /// Sets the stacking order.
    #[inline]
    pub fn set_level(&self, level: WindowLevel) {
        self.inner.set_window_level(window_level_to_winit(level));
    }

    /// Minimises or restores the window.
    #[inline]
    pub fn set_minimized(&self, minimized: bool) {
        self.inner.set_minimized(minimized);
    }

    /// Maximises or restores the window.
    #[inline]
    pub fn set_maximized(&self, maximized: bool) {
        self.inner.set_maximized(maximized);
    }

    /// Whether the window is maximised.
    #[inline]
    pub fn is_maximized(&self) -> bool {
        self.inner.is_maximized()
    }

    /// Raises the window and gives it keyboard focus.
    #[inline]
    pub fn focus(&self) {
        self.inner.focus_window();
    }

    /// Whether the window has keyboard focus.
    #[inline]
    pub fn has_focus(&self) -> bool {
        self.inner.has_focus()
    }

    /// The appearance the platform is applying, when it reports one.
    #[inline]
    pub fn theme(&self) -> Option<Theme> {
        self.inner.theme().map(theme_from_winit)
    }

    /// The display this window is mostly on.
    pub fn current_monitor(&self) -> Option<MonitorInfo> {
        let primary = self.inner.primary_monitor();
        self.inner.current_monitor().map(|m| monitor_info(&m, primary.as_ref()))
    }

    /// Every attached display.
    pub fn available_monitors(&self) -> MonitorList {
        let primary = self.inner.primary_monitor();
        monitor_list(self.inner.available_monitors(), primary.as_ref())
    }

    /// The refresh rate to pace animation against for this window.
    ///
    /// Falls back to 60 Hz when the platform will not say, which is better than
    /// refusing to animate.
    pub fn refresh_rate(&self) -> RefreshRate {
        self.current_monitor().and_then(|m| m.refresh_rate).unwrap_or(RefreshRate::HZ_60)
    }

    /// The raw window handle, for a renderer building a surface.
    pub fn raw_window_handle(&self) -> Result<RawWindowHandle, PlatformError> {
        self.window_handle()
            .map(|h| h.as_raw())
            .map_err(|e| PlatformError::InvalidHandle(e.to_string()))
    }

    /// The raw display handle, for a renderer building a surface.
    pub fn raw_display_handle(&self) -> Result<RawDisplayHandle, PlatformError> {
        self.display_handle()
            .map(|h| h.as_raw())
            .map_err(|e| PlatformError::InvalidHandle(e.to_string()))
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.inner.window_handle()
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.inner.display_handle()
    }
}
/// Installs the custom-frame procedure, if this window wants one.
///
/// Returns `None` for [`WindowChrome::System`], where the platform's own frame
/// is already what the application asked for and a subclass would be pure cost.
#[cfg(windows)]
fn install_chrome(
    inner: &::winit::window::Window,
    attrs: &WindowAttributes,
) -> Option<std::sync::Arc<super::ffi::ChromeState>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if attrs.chrome == WindowChrome::System {
        return None;
    }
    // A window with no HWND is a foreign or headless target. Subclassing
    // something the host owns is explicitly out of scope: it owns that window's
    // lifetime and its message loop.
    let handle = inner.window_handle().ok()?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };

    let state = std::sync::Arc::new(super::ffi::ChromeState::new(
        attrs.chrome,
        attrs.resizable,
        inner.scale_factor() as f32,
    ));
    state.publish_regions(&attrs.caption);
    let hwnd = win32.hwnd.get() as *mut core::ffi::c_void;
    Some(super::ffi::install(hwnd, state))
}

/// Turns a winit move-or-resize error into a `'static` capability name.
///
/// `PlatformError::Unsupported` names a capability rather than an incident, and
/// every one of these failures is the same capability whatever the platform
/// said about it.
#[allow(dead_code)]
fn drag_unsupported(_e: ::winit::error::ExternalError) -> PlatformError {
    PlatformError::Unsupported("user-driven window move or resize")
}

impl Window {
    /// Who draws the title bar and border.
    pub fn chrome(&self) -> WindowChrome {
        #[cfg(windows)]
        {
            self.chrome.as_ref().map_or(WindowChrome::System, |c| c.chrome())
        }
        #[cfg(not(windows))]
        {
            if self.inner.is_decorated() { WindowChrome::System } else { WindowChrome::None }
        }
    }

    /// Applies a platform compositor material behind a transparent window.
    ///
    /// Windows maps this to DWM Mica or Desktop Acrylic. Other platforms do
    /// not silently pretend to provide the effect: they return an explicit
    /// unsupported error and leave the renderer-side transparency intact.
    pub fn set_backdrop(&self, backdrop: WindowBackdrop) -> Result<(), PlatformError> {
        #[cfg(windows)]
        {
            let Some(hwnd) = self.hwnd() else {
                return Err(PlatformError::Unsupported("system window backdrop"));
            };
            if super::ffi::set_backdrop(hwnd, backdrop) {
                return Ok(());
            }
            return Err(PlatformError::Unsupported("system window backdrop"));
        }
        #[cfg(not(windows))]
        {
            let _ = backdrop;
            Err(PlatformError::Unsupported("system window backdrop"))
        }
    }

    /// Switches the frame at runtime.
    ///
    /// A window created with [`WindowChrome::System`] has no custom-frame
    /// procedure installed and cannot gain one: the subclass has to be in place
    /// before the first `WM_NCCALCSIZE`, and retrofitting it would leave the
    /// platform and the application disagreeing about where the client area is.
    /// Ask for the chrome you want in [`WindowAttributes`].
    pub fn set_chrome(&self, chrome: WindowChrome) {
        #[cfg(windows)]
        if let Some(state) = self.chrome.as_ref() {
            state.set_chrome(chrome);
            if let Some(hwnd) = self.hwnd() {
                // Without this the old frame survives until the next resize.
                super::ffi::recalculate_frame(hwnd);
            }
            return;
        }
        self.inner.set_decorations(chrome == WindowChrome::System);
    }

    /// Publishes where the custom caption is.
    ///
    /// Republish whenever the caption's layout changes. Returns `()` rather than
    /// a result because storing geometry cannot fail: on a platform with no
    /// notion of a custom caption the store is simply never read.
    pub fn set_caption_regions(&self, regions: &crate::window::CaptionRegions) {
        #[cfg(windows)]
        if let Some(state) = self.chrome.as_ref() {
            state.publish_regions(regions);
        }
        #[cfg(not(windows))]
        let _ = regions;
    }

    /// Starts a user-driven window move.
    ///
    /// Enters the platform's modal move loop, which pumps its own messages, so
    /// frames during the drag are the platform's business rather than the frame
    /// scheduler's. A caption published through [`Window::set_caption_regions`]
    /// does not need this — the platform starts the drag itself — so this is for
    /// a caption that would rather drive the gesture explicitly.
    pub fn begin_drag(&self) -> Result<(), PlatformError> {
        self.inner.drag_window().map_err(drag_unsupported)
    }

    /// Starts a user-driven resize from an edge or corner.
    ///
    /// Only needed where the platform cannot hit-test the edge itself, which on
    /// Windows means [`WindowChrome::None`].
    pub fn begin_resize(&self, edge: crate::window::ResizeEdge) -> Result<(), PlatformError> {
        use crate::window::ResizeEdge as E;
        use ::winit::window::ResizeDirection as D;
        let direction = match edge {
            E::North => D::North,
            E::South => D::South,
            E::East => D::East,
            E::West => D::West,
            E::NorthEast => D::NorthEast,
            E::NorthWest => D::NorthWest,
            E::SouthEast => D::SouthEast,
            E::SouthWest => D::SouthWest,
        };
        self.inner.drag_resize_window(direction).map_err(drag_unsupported)
    }
}

impl Window {
    /// Opens the platform's window menu at a client-logical position.
    ///
    /// Item states are corrected against the window's own state first: measured,
    /// a maximised window still reports Move and Size as enabled until someone
    /// fixes them up, so an uncorrected menu offers actions that silently do
    /// nothing.
    ///
    /// A right-click on a region published through
    /// [`Window::set_caption_regions`] already does this without the application
    /// asking. This is for a caption that wants to offer the menu from somewhere
    /// else, such as an application button.
    pub fn show_system_menu(&self, at: Point<Px>) -> Result<(), PlatformError> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::POINT;
            let Some(hwnd) = self.hwnd() else {
                return Err(PlatformError::Unsupported("system window menu"));
            };
            let origin = self
                .inner
                .inner_position()
                .map_err(|_| PlatformError::Unsupported("system window menu"))?;
            let scale = self.inner.scale_factor() as f32;
            let screen = POINT {
                x: origin.x + (at.x.get() * scale) as i32,
                y: origin.y + (at.y.get() * scale) as i32,
            };
            let states = match self.chrome.as_ref() {
                Some(state) => {
                    state.set_maximized(self.inner.is_maximized());
                    state.menu_states()
                }
                None => super::nc::menu_states(
                    self.inner.is_maximized(),
                    self.inner.is_resizable(),
                    true,
                    true,
                ),
            };
            if super::ffi::show_system_menu(hwnd, screen, states) {
                return Ok(());
            }
            Err(PlatformError::Unsupported("system window menu"))
        }
        #[cfg(not(windows))]
        {
            let _ = at;
            Err(PlatformError::Unsupported("system window menu"))
        }
    }

    /// Tells the custom frame what the window's state is now.
    ///
    /// The window procedure reads these on `WM_NCHITTEST`, which fires on every
    /// mouse move, so they are pushed on state changes rather than queried in
    /// the hot path.
    pub(crate) fn sync_chrome_state(&self) {
        #[cfg(windows)]
        if let Some(state) = self.chrome.as_ref() {
            state.set_maximized(self.inner.is_maximized());
            state.set_resizable(self.inner.is_resizable());
            state.set_scale(self.inner.scale_factor() as f32);
        }
    }

    /// The raw `HWND`, when there is one.
    #[cfg(windows)]
    fn hwnd(&self) -> Option<*mut core::ffi::c_void> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match self.inner.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get() as *mut core::ffi::c_void),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::winit::event::WindowEvent as W;
    use ::winit::keyboard::{
        Key as WKey, KeyCode as WKeyCode, ModifiersState, NamedKey as WNamedKey, NativeKey,
        NativeKeyCode, PhysicalKey as WPhysicalKey,
    };

    fn buf() -> EventBuf {
        EventBuf::new()
    }

    #[test]
    fn named_keys_translate_by_name() {
        assert_eq!(named_key_from_winit(WNamedKey::Enter), Some(NamedKey::Enter));
        assert_eq!(named_key_from_winit(WNamedKey::ArrowUp), Some(NamedKey::ArrowUp));
        assert_eq!(named_key_from_winit(WNamedKey::Space), Some(NamedKey::Space));
        assert_eq!(named_key_from_winit(WNamedKey::MediaPlayPause), Some(NamedKey::MediaPlayPause));
        // Function keys collapse into the numbered variant.
        assert_eq!(named_key_from_winit(WNamedKey::F1), Some(NamedKey::F(1)));
        assert_eq!(named_key_from_winit(WNamedKey::F12), Some(NamedKey::F(12)));
        assert_eq!(named_key_from_winit(WNamedKey::F35), Some(NamedKey::F(35)));
        // A key SphereKit does not name is dropped, not mistranslated.
        assert_eq!(named_key_from_winit(WNamedKey::LaunchWebCam), None);
    }

    #[test]
    fn physical_keys_translate_by_position() {
        assert_eq!(key_code_from_winit(WKeyCode::KeyA), Some(KeyCode::KeyA));
        assert_eq!(key_code_from_winit(WKeyCode::NumpadEnter), Some(KeyCode::NumpadEnter));
        assert_eq!(key_code_from_winit(WKeyCode::Digit0), Some(KeyCode::Digit0));
        assert_eq!(key_code_from_winit(WKeyCode::F7), Some(KeyCode::F(7)));
        assert_eq!(key_code_from_winit(WKeyCode::BrowserHome), None);
        assert_eq!(
            physical_key_from_winit(WPhysicalKey::Code(WKeyCode::ShiftLeft)),
            PhysicalKey::Code(KeyCode::ShiftLeft)
        );
    }

    #[test]
    fn every_letter_and_digit_position_round_trips() {
        // A gap in this table would silently break every keybinding on that key.
        let letters = [
            (WKeyCode::KeyA, KeyCode::KeyA),
            (WKeyCode::KeyM, KeyCode::KeyM),
            (WKeyCode::KeyQ, KeyCode::KeyQ),
            (WKeyCode::KeyW, KeyCode::KeyW),
            (WKeyCode::KeyZ, KeyCode::KeyZ),
        ];
        for (w, s) in letters {
            assert_eq!(key_code_from_winit(w), Some(s), "{w:?}");
        }
        let digits = [
            (WKeyCode::Digit1, KeyCode::Digit1),
            (WKeyCode::Digit5, KeyCode::Digit5),
            (WKeyCode::Digit9, KeyCode::Digit9),
        ];
        for (w, s) in digits {
            assert_eq!(key_code_from_winit(w), Some(s), "{w:?}");
        }
    }

    #[test]
    fn unnamed_physical_keys_keep_their_raw_value() {
        let pk = WPhysicalKey::Unidentified(NativeKeyCode::Windows(0x1234));
        assert_eq!(physical_key_from_winit(pk), PhysicalKey::Unidentified(Scancode::new(0x1234)));
        let pk = WPhysicalKey::Unidentified(NativeKeyCode::Xkb(99));
        assert_eq!(physical_key_from_winit(pk), PhysicalKey::Unidentified(Scancode::new(99)));
        let pk = WPhysicalKey::Unidentified(NativeKeyCode::Unidentified);
        assert_eq!(physical_key_from_winit(pk), PhysicalKey::Unidentified(Scancode::UNIDENTIFIED));
    }

    #[test]
    fn logical_keys_carry_layout_dependent_text() {
        assert_eq!(key_from_winit(&WKey::Character("q".into())), Key::character("q"));
        assert_eq!(key_from_winit(&WKey::Named(WNamedKey::Tab)), Key::Named(NamedKey::Tab));
        assert_eq!(key_from_winit(&WKey::Dead(Some('\u{0301}'))), Key::Dead(Some('\u{0301}')));
        assert_eq!(key_from_winit(&WKey::Unidentified(NativeKey::Unidentified)), Key::Unidentified);
    }

    #[test]
    fn modifiers_translate_bit_for_bit() {
        assert_eq!(modifiers_from_winit(ModifiersState::empty()), Modifiers::empty());
        assert_eq!(modifiers_from_winit(ModifiersState::SHIFT), Modifiers::SHIFT);
        assert_eq!(modifiers_from_winit(ModifiersState::CONTROL), Modifiers::CTRL);
        assert_eq!(modifiers_from_winit(ModifiersState::ALT), Modifiers::ALT);
        assert_eq!(modifiers_from_winit(ModifiersState::SUPER), Modifiers::SUPER);
        let all = ModifiersState::SHIFT
            | ModifiersState::CONTROL
            | ModifiersState::ALT
            | ModifiersState::SUPER;
        assert_eq!(modifiers_from_winit(all), Modifiers::all());
    }

    #[test]
    fn control_characters_are_not_inserted_as_text() {
        assert_eq!(insertable_text("a"), Some("a"));
        assert_eq!(insertable_text("\u{00E9}"), Some("\u{00E9}"));
        assert_eq!(insertable_text(" "), Some(" "), "space is real text");
        assert_eq!(insertable_text("\r"), None);
        assert_eq!(insertable_text("\n"), None);
        assert_eq!(insertable_text("\u{8}"), None, "backspace must not insert a character");
        assert_eq!(insertable_text("\u{1b}"), None, "escape must not insert a character");
        assert_eq!(insertable_text(""), None);
    }

    #[test]
    fn pointer_positions_arrive_in_logical_pixels() {
        // The whole point of converting at the boundary: a click at physical
        // (200, 100) on a 200 % display is at logical (100, 50).
        for (scale, expected) in [(1.0, 200.0), (1.25, 160.0), (1.5, 133.33334), (2.0, 100.0)] {
            let mut out = buf();
            translate_window_event(
                W::CursorMoved {
                    device_id: ::winit::event::DeviceId::dummy(),
                    position: ::winit::dpi::PhysicalPosition::new(200.0, 100.0),
                },
                ScaleFactor::new(scale),
                Modifiers::empty(),
                &mut out,
            );
            match out.as_slice() {
                [WindowEvent::CursorMoved(p)] => {
                    assert!((p.x.get() - expected).abs() < 0.001, "scale {scale}: {p:?}");
                    assert!((p.y.get() - expected / 2.0).abs() < 0.001, "scale {scale}: {p:?}");
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn pixel_scroll_is_converted_but_line_scroll_is_not() {
        let mut out = buf();
        translate_window_event(
            W::MouseWheel {
                device_id: ::winit::event::DeviceId::dummy(),
                delta: ::winit::event::MouseScrollDelta::PixelDelta(
                    ::winit::dpi::PhysicalPosition::new(0.0, -30.0),
                ),
                phase: ::winit::event::TouchPhase::Moved,
            },
            ScaleFactor::new(1.5),
            Modifiers::empty(),
            &mut out,
        );
        assert_eq!(
            out.as_slice(),
            [WindowEvent::MouseWheel {
                delta: ScrollDelta::Pixels { x: px(0.0), y: px(-20.0) },
                phase: TouchPhase::Moved,
            }]
        );

        let mut out = buf();
        translate_window_event(
            W::MouseWheel {
                device_id: ::winit::event::DeviceId::dummy(),
                delta: ::winit::event::MouseScrollDelta::LineDelta(0.0, -1.0),
                phase: ::winit::event::TouchPhase::Moved,
            },
            ScaleFactor::new(2.0),
            Modifiers::empty(),
            &mut out,
        );
        // Line deltas are counts, not distances: scaling them would be wrong.
        assert_eq!(
            out.as_slice(),
            [WindowEvent::MouseWheel {
                delta: ScrollDelta::Lines { x: 0.0, y: -1.0 },
                phase: TouchPhase::Moved,
            }]
        );
    }

    #[test]
    fn window_sizes_stay_in_device_pixels() {
        let mut out = buf();
        translate_window_event(
            W::Resized(::winit::dpi::PhysicalSize::new(1600, 1200)),
            ScaleFactor::new(2.0),
            Modifiers::empty(),
            &mut out,
        );
        assert_eq!(
            out.as_slice(),
            [WindowEvent::Resized(size(DevicePx(1600), DevicePx(1200)))],
            "a resize must not be pre-divided by the scale factor"
        );
    }

    #[test]
    fn an_absurd_size_saturates_instead_of_going_negative() {
        assert_eq!(
            physical_size(::winit::dpi::PhysicalSize::new(u32::MAX, 10)),
            size(DevicePx(i32::MAX), DevicePx(10))
        );
    }

    #[test]
    fn a_printable_key_press_produces_both_a_key_event_and_text() {
        let mut out = buf();
        press("a", WKeyCode::KeyA, Modifiers::SHIFT, &mut out);
        assert_eq!(out.len(), 2);
        match &out[0] {
            WindowEvent::KeyboardInput { key, physical_key, state, repeat, modifiers } => {
                assert_eq!(*key, Key::character("a"));
                assert_eq!(*physical_key, PhysicalKey::Code(KeyCode::KeyA));
                assert_eq!(*state, ElementState::Pressed);
                assert!(!repeat);
                assert_eq!(*modifiers, Modifiers::SHIFT);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(out[1], WindowEvent::TextInput("a".into()));
    }

    #[test]
    fn shortcut_key_presses_do_not_produce_a_second_text_event() {
        for modifiers in [Modifiers::CTRL, Modifiers::CTRL | Modifiers::SHIFT, Modifiers::SUPER] {
            let mut out = buf();
            press("a", WKeyCode::KeyA, modifiers, &mut out);
            assert_eq!(out.len(), 1, "shortcut modifiers must suppress text: {modifiers:?}");
            assert!(matches!(out[0], WindowEvent::KeyboardInput { .. }));
        }
    }

    #[test]
    fn ctrl_alt_keeps_text_for_altgr_layouts() {
        let mut out = buf();
        press("@", WKeyCode::KeyQ, Modifiers::CTRL | Modifiers::ALT, &mut out);
        assert_eq!(out.len(), 2, "AltGr must remain a text-producing chord");
        assert_eq!(out[1], WindowEvent::TextInput("@".into()));
    }

    #[test]
    fn a_key_release_never_inserts_text() {
        let mut out = buf();
        translate_key_input(
            &WKey::Character("a".into()),
            WPhysicalKey::Code(WKeyCode::KeyA),
            Some("a"),
            ::winit::event::ElementState::Released,
            false,
            false,
            Modifiers::empty(),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], WindowEvent::KeyboardInput { .. }));
    }

    #[test]
    fn synthetic_key_events_do_not_insert_text() {
        // winit replays held keys when a window gains focus; inserting those
        // would type characters the user pressed while another window had focus.
        let mut out = buf();
        translate_key_input(
            &WKey::Character("a".into()),
            WPhysicalKey::Code(WKeyCode::KeyA),
            Some("a"),
            ::winit::event::ElementState::Pressed,
            false,
            true,
            Modifiers::empty(),
            &mut out,
        );
        assert_eq!(out.len(), 1, "synthetic press must not produce TextInput");
    }

    #[test]
    fn auto_repeat_is_flagged_and_still_inserts_text() {
        let mut out = buf();
        translate_key_input(
            &WKey::Character("x".into()),
            WPhysicalKey::Code(WKeyCode::KeyX),
            Some("x"),
            ::winit::event::ElementState::Pressed,
            true,
            false,
            Modifiers::empty(),
            &mut out,
        );
        assert!(matches!(out[0], WindowEvent::KeyboardInput { repeat: true, .. }));
        assert_eq!(out[1], WindowEvent::TextInput("x".into()));
    }

    #[test]
    fn enter_produces_a_key_event_but_no_text() {
        // Platforms report "\r" as the text of Enter. A text field must insert
        // a newline because it decided to, not because the platform smuggled a
        // carriage return through.
        let mut out = buf();
        translate_key_input(
            &WKey::Named(WNamedKey::Enter),
            WPhysicalKey::Code(WKeyCode::Enter),
            Some("\r"),
            ::winit::event::ElementState::Pressed,
            false,
            false,
            Modifiers::empty(),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            WindowEvent::KeyboardInput { key: Key::Named(NamedKey::Enter), .. }
        ));
    }

    #[test]
    fn ime_events_survive_translation_intact() {
        let mut out = buf();
        translate_window_event(
            W::Ime(::winit::event::Ime::Preedit("\u{304B}".into(), Some((0, 3)))),
            ScaleFactor::IDENTITY,
            Modifiers::empty(),
            &mut out,
        );
        assert_eq!(
            out.as_slice(),
            [WindowEvent::Ime(ImeEvent::Preedit { text: "\u{304B}".into(), cursor: Some((0, 3)) })]
        );
    }

    #[test]
    fn untranslated_platform_events_are_dropped_not_faked() {
        let mut out = buf();
        translate_window_event(
            W::DoubleTapGesture { device_id: ::winit::event::DeviceId::dummy() },
            ScaleFactor::IDENTITY,
            Modifiers::empty(),
            &mut out,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn simple_events_map_one_to_one() {
        let cases: Vec<(W, WindowEvent)> = vec![
            (W::CloseRequested, WindowEvent::CloseRequested),
            (W::Destroyed, WindowEvent::Destroyed),
            (W::Focused(true), WindowEvent::Focused(true)),
            (W::Occluded(true), WindowEvent::Occluded(true)),
            (W::RedrawRequested, WindowEvent::RedrawRequested),
            (W::HoveredFileCancelled, WindowEvent::FileHoverCancelled),
            (W::ThemeChanged(::winit::window::Theme::Dark), WindowEvent::ThemeChanged(Theme::Dark)),
            (
                W::Moved(::winit::dpi::PhysicalPosition::new(-40, 12)),
                WindowEvent::Moved(point(DevicePx(-40), DevicePx(12))),
            ),
        ];
        for (input, expected) in cases {
            let mut out = buf();
            translate_window_event(input, ScaleFactor::IDENTITY, Modifiers::empty(), &mut out);
            assert_eq!(out.as_slice(), [expected]);
        }
    }

    #[test]
    fn cursors_map_onto_platform_icons_and_hidden_has_none() {
        assert!(cursor_to_winit(Cursor::Hidden).is_none());
        assert_eq!(cursor_to_winit(Cursor::Default), Some(::winit::window::CursorIcon::Default));
        assert_eq!(
            cursor_to_winit(Cursor::ColResize),
            Some(::winit::window::CursorIcon::ColResize)
        );
        assert_eq!(cursor_to_winit(Cursor::Grabbing), Some(::winit::window::CursorIcon::Grabbing));
    }

    #[test]
    fn window_attributes_translate_into_platform_attributes() {
        let attrs = WindowAttributes::new("EQ")
            .with_inner_size(size(px(600.0), px(400.0)))
            .with_min_inner_size(size(px(300.0), px(200.0)))
            .with_resizable(false)
            .with_decorations(false)
            .with_always_on_top(true)
            .with_visible(false);
        // SAFETY: no parent handle is set, so there is nothing to uphold.
        let w = unsafe { to_winit_attributes(&attrs) };
        assert_eq!(w.title, "EQ");
        assert!(!w.resizable);
        assert!(!w.decorations, "None chrome strips the platform frame");
        assert!(!w.visible);
        assert_eq!(w.window_level, ::winit::window::WindowLevel::AlwaysOnTop);
        assert_eq!(
            w.inner_size,
            Some(::winit::dpi::Size::Logical(::winit::dpi::LogicalSize::new(600.0, 400.0)))
        );
        assert!(w.parent_window().is_none());
    }

    #[test]
    fn a_parent_handle_reaches_the_platform_attributes() {
        // The plug-in embedding path: a host's view handle must actually arrive
        // at the platform as a parent window, not be quietly dropped.
        use raw_window_handle::{RawWindowHandle, Win32WindowHandle};
        use std::num::NonZeroIsize;

        let hwnd = NonZeroIsize::new(0x1234).unwrap();
        // SAFETY: the handle is never dereferenced; only its plumbing is under
        // test, and no window is created from these attributes.
        let parent = unsafe {
            crate::window::ForeignWindowHandle::new(RawWindowHandle::Win32(Win32WindowHandle::new(
                hwnd,
            )))
        };
        let attrs = WindowAttributes::new("Embedded editor").with_parent(parent);
        // SAFETY: as above.
        let w = unsafe { to_winit_attributes(&attrs) };
        assert!(matches!(w.parent_window(), Some(RawWindowHandle::Win32(_))));
    }

    #[test]
    fn scheduler_modes_reach_the_platform_loop_intact() {
        use crate::scheduler::{FrameScheduler, RedrawPolicy};
        use std::time::Instant;

        let now = Instant::now();
        let mut s = FrameScheduler::with_refresh_rate(RefreshRate::HZ_144);

        // Idle: the platform loop blocks. This is the zero-CPU state.
        assert_eq!(
            control_flow_to_winit(s.control_flow(now, RedrawPolicy::Idle)),
            ::winit::event_loop::ControlFlow::Wait
        );
        // Dirty: one immediate iteration.
        s.request_redraw();
        assert_eq!(
            control_flow_to_winit(s.control_flow(now, RedrawPolicy::Idle)),
            ::winit::event_loop::ControlFlow::Poll
        );
        s.frame_presented(now);
        // Animating: paced against the display.
        s.begin_animation();
        assert_eq!(
            control_flow_to_winit(s.control_flow(now, RedrawPolicy::Idle)),
            ::winit::event_loop::ControlFlow::WaitUntil(now + RefreshRate::HZ_144.frame_duration())
        );
        s.end_animation();
        // Realtime: never sleeps.
        s.begin_realtime();
        assert_eq!(
            control_flow_to_winit(s.control_flow(now, RedrawPolicy::Idle)),
            ::winit::event_loop::ControlFlow::Poll
        );
    }

    /// The window type must be shareable, or the renderer cannot hold an
    /// `Arc<Window>` for a `'static` surface.
    #[test]
    fn window_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Window>();
        assert_send_sync::<std::sync::Arc<Window>>();
    }

    /// A plain, non-repeating press of a character key.
    fn press(text: &str, code: WKeyCode, modifiers: Modifiers, out: &mut EventBuf) {
        translate_key_input(
            &WKey::Character(text.into()),
            WPhysicalKey::Code(code),
            Some(text),
            ::winit::event::ElementState::Pressed,
            false,
            false,
            modifiers,
            out,
        );
    }
}

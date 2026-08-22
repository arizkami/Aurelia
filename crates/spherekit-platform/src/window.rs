//! Window configuration and the surface targets a renderer can draw into.
//!
//! ## Owning the process, or not
//!
//! A standalone application owns its process: it creates an event loop, opens
//! windows and runs until the user quits. A plug-in owns none of that. The host
//! created the process, runs the event loop (or a native one SphereKit never
//! sees), and hands the plug-in a *parent window* it does not own and must not
//! destroy.
//!
//! Both cases end at the same place — a raw window handle a GPU surface can be
//! built from — so [`WindowTarget`] is the single type the renderer accepts:
//!
//! * [`WindowTarget::Owned`] wraps a window SphereKit created and will destroy.
//! * [`WindowTarget::Foreign`] wraps a [`ForeignWindow`]: a handle whose
//!   lifetime, size and scale factor are the host's business, reported to
//!   SphereKit by the embedding shim.
//!
//! ## Host lifecycle concerns
//!
//! These are the rules an embedder has to honour; they are documented here
//! because getting them wrong produces crashes that only reproduce inside one
//! particular DAW.
//!
//! 1. **The handle can die at any time.** VST3 `IPlugView::removed()` and CLAP
//!    `gui.destroy()` can arrive while a frame is in flight. Drop every GPU
//!    surface built from a [`ForeignWindow`] *before* returning from the host's
//!    teardown callback, and drop the [`ForeignWindow`] with it.
//! 2. **SphereKit never resizes a foreign window.** The host owns its geometry.
//!    The shim calls [`ForeignWindow::set_physical_size`] when the host reports
//!    a resize; nothing here touches the platform window.
//! 3. **Scale factor comes from the host, not from the display.** VST3 hosts
//!    apply their own content scaling and expect the plug-in to honour
//!    `IPlugViewContentScaleSupport`. Reading the monitor's scale factor
//!    instead produces a UI at the wrong size in exactly the hosts that got it
//!    right. Call [`ForeignWindow::set_scale_factor`].
//! 4. **The UI thread is the host's.** All of this must be touched from the
//!    thread the host calls the plug-in's view methods on, which is not
//!    necessarily the process's main thread. That is also why
//!    [`ForeignWindow`] is deliberately not `Send`: a raw handle that migrated
//!    to another thread is a crash on macOS.
//! 5. **Two editors can be open at once.** Nothing here is a singleton, and
//!    ids minted by [`crate::WindowRegistry`] are never reused, so a stale id
//!    from a destroyed editor cannot address a live one.

use raw_window_handle::{
    AppKitDisplayHandle, DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle,
    RawDisplayHandle, RawWindowHandle, UiKitDisplayHandle, WindowHandle, WindowsDisplayHandle,
};
use spherekit_core::{DevicePx, PlatformError, Point, Px, Rect, ScaleFactor, Size, px, size};

use crate::event::Theme;

/// Where a new window should be placed.
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum WindowPosition {
    /// Centred on the display the window would open on.
    Centered,
    /// A logical-pixel offset from the desktop origin. Logical because the
    /// scale factor of the target display is not known until the window
    /// exists, and the platform resolves this for us.
    Logical(Point<Px>),
    /// An exact device-pixel offset from the desktop origin. Use this when
    /// restoring a saved position: it round-trips exactly, whereas a logical
    /// position re-resolved at a different scale factor will not.
    Physical(Point<DevicePx>),
}

/// Where a window sits in the stacking order.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub enum WindowLevel {
    /// Behind normal windows.
    AlwaysOnBottom,
    /// The usual behaviour.
    #[default]
    Normal,
    /// In front of normal windows. What a floating plug-in editor or a
    /// detached meter bridge wants.
    AlwaysOnTop,
}

/// A handle to a window SphereKit does not own.
///
/// Constructing one is `unsafe` because the handle's validity cannot be
/// checked: it is a bare pointer or integer from a host that promises to keep
/// the window alive. Everything downstream — surface creation, resize, present
/// — trusts that promise, so it is made once, explicitly, here.
#[derive(Clone, Debug)]
pub struct ForeignWindowHandle {
    /// The host's window handle.
    window: RawWindowHandle,
    /// The host's display/connection handle, when it has a meaningful one.
    display: Option<RawDisplayHandle>,
}

impl ForeignWindowHandle {
    /// Wraps a host-owned window handle.
    ///
    /// The display handle is inferred where the platform has no meaningful one
    /// to carry (Windows, `AppKit`, `UIKit`). On X11 and Wayland the connection
    /// *is* meaningful and the caller must use
    /// [`ForeignWindowHandle::with_display`] instead, or surface creation will
    /// fail with [`PlatformError::InvalidHandle`].
    ///
    /// # Safety
    ///
    /// `window` must refer to a live window that outlives this value and every
    /// GPU surface created from it, and must only be used from the thread the
    /// host associates with that window.
    pub unsafe fn new(window: RawWindowHandle) -> Self {
        let display = implied_display(&window);
        Self { window, display }
    }

    /// Wraps a host-owned window handle together with its display connection.
    ///
    /// # Safety
    ///
    /// Both handles must refer to live objects that outlive this value and
    /// every GPU surface created from it, and must only be used from the thread
    /// the host associates with them.
    pub unsafe fn with_display(window: RawWindowHandle, display: RawDisplayHandle) -> Self {
        Self { window, display: Some(display) }
    }

    /// The raw window handle.
    #[inline]
    pub fn raw_window_handle(&self) -> RawWindowHandle {
        self.window
    }

    /// The raw display handle, if one is known.
    #[inline]
    pub fn raw_display_handle(&self) -> Option<RawDisplayHandle> {
        self.display
    }
}

/// The display handle implied by a window handle, on platforms where the
/// display carries no state.
///
/// X11 and Wayland deliberately return `None`: their display handles carry a
/// live connection pointer that cannot be fabricated, and inventing a null one
/// would fail later, inside the driver, with a far worse diagnostic.
fn implied_display(window: &RawWindowHandle) -> Option<RawDisplayHandle> {
    match window {
        RawWindowHandle::Win32(_) | RawWindowHandle::WinRt(_) => {
            Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new()))
        }
        RawWindowHandle::AppKit(_) => Some(RawDisplayHandle::AppKit(AppKitDisplayHandle::new())),
        RawWindowHandle::UiKit(_) => Some(RawDisplayHandle::UiKit(UiKitDisplayHandle::new())),
        _ => None,
    }
}

/// A window owned by a plug-in host, described well enough to render into.
///
/// Not `Send`: see rule 4 in the [module documentation](self).
#[derive(Clone, Debug)]
pub struct ForeignWindow {
    /// The host's handles.
    handle: ForeignWindowHandle,
    /// The drawable extent, as last reported by the host.
    physical_size: Size<DevicePx>,
    /// The content scale the host asked for.
    scale_factor: ScaleFactor,
    /// Keeps the type `!Send` and `!Sync` without an `unsafe impl`.
    _not_send: core::marker::PhantomData<*const ()>,
}

impl ForeignWindow {
    /// Describes a host-owned window.
    pub fn new(
        handle: ForeignWindowHandle,
        physical_size: Size<DevicePx>,
        scale_factor: ScaleFactor,
    ) -> Self {
        Self { handle, physical_size, scale_factor, _not_send: core::marker::PhantomData }
    }

    /// The handles this window was built from.
    #[inline]
    pub fn handle(&self) -> &ForeignWindowHandle {
        &self.handle
    }

    /// The drawable extent in device pixels.
    #[inline]
    pub fn physical_size(&self) -> Size<DevicePx> {
        self.physical_size
    }

    /// Records a resize reported by the host. SphereKit never initiates one.
    #[inline]
    pub fn set_physical_size(&mut self, size: Size<DevicePx>) {
        self.physical_size = size;
    }

    /// The content scale the host asked for.
    #[inline]
    pub fn scale_factor(&self) -> ScaleFactor {
        self.scale_factor
    }

    /// Records a content-scale change reported by the host.
    #[inline]
    pub fn set_scale_factor(&mut self, scale_factor: ScaleFactor) {
        self.scale_factor = scale_factor;
    }

    /// The drawable extent in logical pixels.
    #[inline]
    pub fn inner_size(&self) -> Size<Px> {
        logical_size(self.physical_size, self.scale_factor)
    }
}

impl HasWindowHandle for ForeignWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: `ForeignWindowHandle` can only be built through an `unsafe`
        // constructor whose contract requires the handle to outlive this value.
        Ok(unsafe { WindowHandle::borrow_raw(self.handle.window) })
    }
}

impl HasDisplayHandle for ForeignWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        match self.handle.display {
            // SAFETY: as above; the display handle carries the same contract.
            Some(d) => Ok(unsafe { DisplayHandle::borrow_raw(d) }),
            None => Err(HandleError::Unavailable),
        }
    }
}

/// Converts a physical extent to a logical one.
///
/// Shared by every target so the rounding behaviour cannot diverge between an
/// owned and a foreign window.
fn logical_size(physical: Size<DevicePx>, scale: ScaleFactor) -> Size<Px> {
    size(scale.to_logical(physical.width), scale.to_logical(physical.height))
}

/// Something a renderer can create a surface for.
///
/// This is the seam between `spherekit-platform` and `spherekit-wgpu`. Both variants
/// implement [`HasWindowHandle`] and [`HasDisplayHandle`], and
/// [`WindowTarget::raw_handles`] exposes the pair directly for backends that
/// need the unsafe surface-creation path (which is the one a foreign handle
/// requires, since it is neither `Send` nor `'static`).
///
/// A `WindowTarget` as a whole is therefore not `Send`. For the owned case the
/// renderer should take the `Arc<Window>` directly instead: that *is*
/// `Send + Sync + 'static`, which is what wgpu's safe surface constructor
/// wants.
#[derive(Clone, Debug)]
pub enum WindowTarget {
    /// A window SphereKit created and owns.
    #[cfg(feature = "winit-backend")]
    Owned(std::sync::Arc<crate::backend::Window>),
    /// A window owned by a plug-in host.
    Foreign(ForeignWindow),
}

impl WindowTarget {
    /// The drawable extent in device pixels. This is what the surface must be
    /// configured with.
    pub fn physical_size(&self) -> Size<DevicePx> {
        match self {
            #[cfg(feature = "winit-backend")]
            WindowTarget::Owned(w) => w.physical_size(),
            WindowTarget::Foreign(w) => w.physical_size(),
        }
    }

    /// The current scale factor.
    pub fn scale_factor(&self) -> ScaleFactor {
        match self {
            #[cfg(feature = "winit-backend")]
            WindowTarget::Owned(w) => w.scale_factor(),
            WindowTarget::Foreign(w) => w.scale_factor(),
        }
    }

    /// The drawable extent in logical pixels.
    pub fn inner_size(&self) -> Size<Px> {
        logical_size(self.physical_size(), self.scale_factor())
    }

    /// The raw handle pair, for backends that build surfaces unsafely.
    ///
    /// Returns [`PlatformError::InvalidHandle`] when the platform cannot
    /// produce one, which for a foreign window means the host supplied an X11
    /// or Wayland window without its connection.
    pub fn raw_handles(&self) -> Result<(RawDisplayHandle, RawWindowHandle), PlatformError> {
        let display = self
            .display_handle()
            .map_err(|e| PlatformError::InvalidHandle(format!("display handle: {e}")))?
            .as_raw();
        let window = self
            .window_handle()
            .map_err(|e| PlatformError::InvalidHandle(format!("window handle: {e}")))?
            .as_raw();
        Ok((display, window))
    }

    /// Tells the compositor a frame is about to be presented.
    ///
    /// A no-op for a foreign window: the host owns presentation timing there.
    pub fn pre_present_notify(&self) {
        match self {
            #[cfg(feature = "winit-backend")]
            WindowTarget::Owned(w) => w.pre_present_notify(),
            WindowTarget::Foreign(_) => {}
        }
    }
}

impl HasWindowHandle for WindowTarget {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        match self {
            #[cfg(feature = "winit-backend")]
            WindowTarget::Owned(w) => w.window_handle(),
            WindowTarget::Foreign(w) => w.window_handle(),
        }
    }
}

impl HasDisplayHandle for WindowTarget {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        match self {
            #[cfg(feature = "winit-backend")]
            WindowTarget::Owned(w) => w.display_handle(),
            WindowTarget::Foreign(w) => w.display_handle(),
        }
    }
}

impl From<ForeignWindow> for WindowTarget {
    fn from(w: ForeignWindow) -> Self {
        WindowTarget::Foreign(w)
    }
}

/// How a window should be created.
///
/// Sizes are logical: a plug-in editor that is "600 x 400" is 600 x 400 at
/// 100 % and 1200 x 800 at 200 %, and the author should not have to think
/// about which. The platform resolves them against the target display's scale
/// Who draws the title bar and the border.
///
/// Replaces a plain `decorations: bool`, which could not express the middle
/// case — and the middle case is the one worth having.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
#[non_exhaustive]
pub enum WindowChrome {
    /// The platform draws everything. The default.
    #[default]
    System,
    /// The application paints the caption; the platform keeps the resize
    /// borders, snap, the window menu and the drop shadow.
    ///
    /// This is what a modern application with a custom title bar wants. It is
    /// strictly better than [`WindowChrome::None`] for anything with a window
    /// frame, because none of what it keeps costs anything to keep.
    Custom,
    /// No frame at all.
    ///
    /// Gives up resize borders, snap, the window menu and the drop shadow along
    /// with the title bar. On Windows a fully stripped frame reports the whole
    /// window as client area, so nothing resizes it but an explicit
    /// `Window::begin_resize`. For splash screens and HUDs, not for
    /// applications.
    None,
}

/// System material drawn behind a transparent window by the platform
/// compositor.
///
/// On Windows this maps to the DWM system-backdrop attribute. Other platforms
/// keep the transparent window and report the capability as unsupported, so an
/// application can retain its renderer-side fallback without conditional UI
/// code.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
#[non_exhaustive]
pub enum WindowBackdrop {
    /// Remove a previously applied system material.
    #[default]
    None,
    /// Windows Mica material for a main application window.
    Mica,
    /// Windows Desktop Acrylic material for transient surfaces.
    Acrylic,
}

/// Which edge or corner a user-initiated resize grabs.
///
/// Named to match the eight resize shapes in [`crate::Cursor`], so the two
/// never need cross-referencing.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum ResizeEdge {
    /// Top edge.
    North,
    /// Bottom edge.
    South,
    /// Right edge.
    East,
    /// Left edge.
    West,
    /// Top-right corner.
    NorthEast,
    /// Top-left corner.
    NorthWest,
    /// Bottom-right corner.
    SouthEast,
    /// Bottom-left corner.
    SouthWest,
}

/// Where a custom caption is, in logical pixels relative to the client area.
///
/// Pushed down as geometry rather than answered as an event, because the
/// platform asks for a hit test *synchronously, from inside its own dispatch*.
/// There is no point at which the interface thread could be called back to
/// answer it. Republish whenever the caption's layout changes: it is a cheap
/// store, and the platform reads the newest value on its next query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptionRegions {
    /// Rectangles that behave as a title bar: drag to move, double-click to
    /// maximise, right-click for the system menu.
    pub drag: Vec<Rect<Px>>,
    /// Rectangles *inside* `drag` that stay interactive.
    ///
    /// Caption buttons, tabs and menus must be listed here. A press the
    /// platform routes as caption never reaches the client at all, so an
    /// unlisted button receives no click **ever** — not merely a delayed one.
    pub exclude: Vec<Rect<Px>>,
}

impl CaptionRegions {
    /// Nothing is a caption.
    pub const fn new() -> Self {
        Self { drag: Vec::new(), exclude: Vec::new() }
    }

    /// A full-width strip across the top, the common case.
    pub fn strip(width: Px, height: Px) -> Self {
        Self {
            drag: vec![Rect::new(Point::new(Px::ZERO, Px::ZERO), Size::new(width, height))],
            exclude: Vec::new(),
        }
    }

    /// Carves an interactive rectangle out of the caption.
    #[must_use]
    pub fn with_button(mut self, rect: Rect<Px>) -> Self {
        self.exclude.push(rect);
        self
    }

    /// True when a point should be treated as the title bar.
    ///
    /// Exclusions win over drag regions, and this is the exact function the
    /// platform-side hit test calls, so what the application reasons about and
    /// what the window manager does cannot diverge.
    pub fn hits_caption(&self, point: Point<Px>) -> bool {
        if self.exclude.iter().any(|r| r.contains(point)) {
            return false;
        }
        self.drag.iter().any(|r| r.contains(point))
    }

    /// True when no caption has been published.
    pub fn is_empty(&self) -> bool {
        self.drag.is_empty()
    }
}

/// factor at creation time.
#[derive(Clone, Debug)]
pub struct WindowAttributes {
    /// Title bar text.
    pub title: String,
    /// Initial inner (client area) size, in logical pixels.
    pub inner_size: Size<Px>,
    /// Smallest inner size the user may drag to.
    pub min_inner_size: Option<Size<Px>>,
    /// Largest inner size the user may drag to.
    pub max_inner_size: Option<Size<Px>>,
    /// Where to place the window; `None` lets the platform decide.
    pub position: Option<WindowPosition>,
    /// Whether the user may resize the window.
    pub resizable: bool,
    /// Who draws the title bar and border.
    ///
    /// Replaces the old `decorations: bool`, which could not express the middle
    /// case. [`WindowChrome::Custom`] is the one worth having.
    pub chrome: WindowChrome,
    /// Caption geometry for [`WindowChrome::Custom`], when it is known at
    /// creation time.
    ///
    /// Usually left empty and published later through
    /// `Window::set_caption_regions`, because a caption only has a size once
    /// the interface has been laid out.
    pub caption: CaptionRegions,
    /// Whether the window's background may be see-through. Requires the
    /// renderer to clear with a non-opaque alpha as well; setting it here only
    /// asks the compositor to respect the alpha channel.
    pub transparent: bool,
    /// Stacking order.
    pub level: WindowLevel,
    /// Whether the window is mapped immediately.
    ///
    /// Opening hidden and showing after the first frame is presented is what
    /// avoids the one-frame flash of an unpainted window.
    pub visible: bool,
    /// Whether to open maximised.
    pub maximized: bool,
    /// Whether the window takes focus when it opens. A plug-in editor usually
    /// should not steal focus from the host's transport.
    pub active: bool,
    /// Force a light or dark appearance; `None` follows the system.
    pub theme: Option<Theme>,
    /// A host-owned window to become a child of.
    ///
    /// This is the plug-in embedding path: the host passes its view handle and
    /// SphereKit creates a real child window inside it, which keeps input routing
    /// and IME working the way the platform expects. Set it with an
    /// unsafely-constructed [`ForeignWindowHandle`].
    pub parent: Option<ForeignWindowHandle>,
}

impl Default for WindowAttributes {
    fn default() -> Self {
        Self {
            title: String::from("SphereKit"),
            // 1024x640 is a 16:10 shape that fits a 1366x768 laptop panel with
            // room for a taskbar, which is still the floor for plug-in UIs.
            inner_size: size(px(1024.0), px(640.0)),
            min_inner_size: None,
            max_inner_size: None,
            position: None,
            resizable: true,
            chrome: WindowChrome::System,
            caption: CaptionRegions::default(),
            transparent: false,
            level: WindowLevel::Normal,
            visible: true,
            maximized: false,
            active: true,
            theme: None,
            parent: None,
        }
    }
}

impl WindowAttributes {
    /// Default attributes with a title.
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), ..Self::default() }
    }

    /// Sets the title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Sets the initial inner size, in logical pixels.
    #[must_use]
    pub fn with_inner_size(mut self, size: Size<Px>) -> Self {
        self.inner_size = size;
        self
    }

    /// Sets the minimum inner size.
    #[must_use]
    pub fn with_min_inner_size(mut self, size: Size<Px>) -> Self {
        self.min_inner_size = Some(size);
        self
    }

    /// Sets the maximum inner size.
    #[must_use]
    pub fn with_max_inner_size(mut self, size: Size<Px>) -> Self {
        self.max_inner_size = Some(size);
        self
    }

    /// Sets the initial position.
    #[must_use]
    pub fn with_position(mut self, position: WindowPosition) -> Self {
        self.position = Some(position);
        self
    }

    /// Sets whether the user may resize the window.
    #[must_use]
    pub fn with_resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    /// Sets whether the platform draws decorations.
    ///
    /// Kept under its old name with its old meaning — `true` is
    /// [`WindowChrome::System`], `false` is [`WindowChrome::None`] — so
    /// existing call sites are unaffected. Prefer
    /// [`WindowAttributes::with_chrome`]: `false` is almost never what you
    /// want, because it gives up resize borders, snap and the drop shadow to
    /// get rid of a title bar.
    #[must_use]
    pub fn with_decorations(mut self, decorations: bool) -> Self {
        self.chrome = if decorations { WindowChrome::System } else { WindowChrome::None };
        self
    }

    /// Sets who draws the title bar and border.
    #[must_use]
    pub fn with_chrome(mut self, chrome: WindowChrome) -> Self {
        self.chrome = chrome;
        self
    }

    /// Sets the caption geometry used by [`WindowChrome::Custom`].
    #[must_use]
    pub fn with_caption(mut self, caption: CaptionRegions) -> Self {
        self.caption = caption;
        self
    }

    /// Sets whether the window may be see-through.
    #[must_use]
    pub fn with_transparent(mut self, transparent: bool) -> Self {
        self.transparent = transparent;
        self
    }

    /// Sets the stacking order.
    #[must_use]
    pub fn with_level(mut self, level: WindowLevel) -> Self {
        self.level = level;
        self
    }

    /// Convenience for [`WindowLevel::AlwaysOnTop`].
    #[must_use]
    pub fn with_always_on_top(mut self, on_top: bool) -> Self {
        self.level = if on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal };
        self
    }

    /// Sets whether the window is mapped immediately.
    #[must_use]
    pub fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// Sets whether the window opens maximised.
    #[must_use]
    pub fn with_maximized(mut self, maximized: bool) -> Self {
        self.maximized = maximized;
        self
    }

    /// Sets whether the window takes focus when it opens.
    #[must_use]
    pub fn with_active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Forces a light or dark appearance.
    #[must_use]
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = Some(theme);
        self
    }

    /// Embeds the new window inside a host-owned parent.
    #[must_use]
    pub fn with_parent(mut self, parent: ForeignWindowHandle) -> Self {
        self.parent = Some(parent);
        self
    }

    /// The size limits, clamped so that `min <= max` on both axes.
    ///
    /// Platforms disagree about what a min larger than the max means — some
    /// clamp, some ignore one, X11 window managers do something else again —
    /// so the contradiction is resolved here, once, with the minimum winning.
    pub fn resolved_size_limits(&self) -> (Option<Size<Px>>, Option<Size<Px>>) {
        match (self.min_inner_size, self.max_inner_size) {
            (Some(min), Some(max)) => (Some(min), Some(max.max(min))),
            other => other,
        }
    }

    /// The initial inner size, clamped into the configured limits.
    ///
    /// A caller that sets `inner_size` outside its own min/max would otherwise
    /// get a window the user cannot drag back to the size they asked for.
    pub fn resolved_inner_size(&self) -> Size<Px> {
        let (min, max) = self.resolved_size_limits();
        let mut s = self.inner_size;
        if let Some(max) = max {
            s = s.min(max);
        }
        if let Some(min) = min {
            s = s.max(min);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_window_handle::Win32WindowHandle;
    use spherekit_core::point;
    use std::num::NonZeroIsize;

    fn fake_win32() -> RawWindowHandle {
        // A plausible-looking HWND. Never dereferenced by any test: the tests
        // here only exercise handle plumbing, never a real surface.
        let hwnd = NonZeroIsize::new(0x1234).unwrap();
        RawWindowHandle::Win32(Win32WindowHandle::new(hwnd))
    }

    fn foreign(w: i32, h: i32, scale: f32) -> ForeignWindow {
        // SAFETY: the handle is never dereferenced; see `fake_win32`.
        let handle = unsafe { ForeignWindowHandle::new(fake_win32()) };
        ForeignWindow::new(handle, size(DevicePx(w), DevicePx(h)), ScaleFactor::new(scale))
    }

    #[test]
    fn default_attributes_are_a_usable_resizable_window() {
        let a = WindowAttributes::default();
        assert_eq!(a.title, "SphereKit");
        assert_eq!(a.inner_size, size(px(1024.0), px(640.0)));
        assert!(a.resizable);
        assert_eq!(a.chrome, WindowChrome::System);
        assert!(a.visible);
        assert!(a.active);
        assert!(!a.transparent);
        assert!(!a.maximized);
        assert_eq!(a.level, WindowLevel::Normal);
        assert_eq!(a.position, None);
        assert_eq!(a.min_inner_size, None);
        assert_eq!(a.max_inner_size, None);
        assert!(a.theme.is_none());
        assert!(a.parent.is_none());
    }

    #[test]
    fn builders_compose_without_disturbing_other_fields() {
        let a = WindowAttributes::new("Compressor")
            .with_inner_size(size(px(600.0), px(400.0)))
            .with_resizable(false)
            .with_always_on_top(true)
            .with_visible(false)
            .with_theme(Theme::Dark)
            .with_position(WindowPosition::Centered);
        assert_eq!(a.title, "Compressor");
        assert_eq!(a.inner_size, size(px(600.0), px(400.0)));
        assert!(!a.resizable);
        assert_eq!(a.level, WindowLevel::AlwaysOnTop);
        assert!(!a.visible);
        assert_eq!(a.theme, Some(Theme::Dark));
        assert_eq!(a.position, Some(WindowPosition::Centered));
        // Untouched fields keep their defaults.
        assert_eq!(a.chrome, WindowChrome::System);
        assert!(a.active);
        assert_eq!(a.with_always_on_top(false).level, WindowLevel::Normal);
    }

    #[test]
    fn contradictory_size_limits_resolve_with_the_minimum_winning() {
        let a = WindowAttributes::default()
            .with_min_inner_size(size(px(400.0), px(300.0)))
            .with_max_inner_size(size(px(200.0), px(100.0)));
        let (min, max) = a.resolved_size_limits();
        assert_eq!(min, Some(size(px(400.0), px(300.0))));
        assert_eq!(max, Some(size(px(400.0), px(300.0))));
    }

    #[test]
    fn the_initial_size_is_clamped_into_its_own_limits() {
        let a = WindowAttributes::default()
            .with_inner_size(size(px(100.0), px(100.0)))
            .with_min_inner_size(size(px(400.0), px(300.0)));
        assert_eq!(a.resolved_inner_size(), size(px(400.0), px(300.0)));

        let a = WindowAttributes::default()
            .with_inner_size(size(px(4000.0), px(4000.0)))
            .with_max_inner_size(size(px(800.0), px(600.0)));
        assert_eq!(a.resolved_inner_size(), size(px(800.0), px(600.0)));

        // No limits: the requested size survives untouched.
        let a = WindowAttributes::default().with_inner_size(size(px(123.0), px(45.0)));
        assert_eq!(a.resolved_inner_size(), size(px(123.0), px(45.0)));
    }

    #[test]
    fn a_foreign_window_reports_logical_size_at_the_hosts_scale() {
        for (scale, expected) in [(1.0, 800.0), (1.25, 640.0), (1.5, 533.3333), (2.0, 400.0)] {
            let w = foreign(800, 600, scale);
            assert!(
                (w.inner_size().width.get() - expected).abs() < 0.01,
                "scale {scale} gave {:?}",
                w.inner_size()
            );
            assert_eq!(w.physical_size(), size(DevicePx(800), DevicePx(600)));
            assert_eq!(w.scale_factor(), ScaleFactor::new(scale));
        }
    }

    #[test]
    fn the_host_drives_resize_and_scale_of_a_foreign_window() {
        let mut w = foreign(800, 600, 1.0);
        w.set_physical_size(size(DevicePx(1600), DevicePx(1200)));
        w.set_scale_factor(ScaleFactor::new(2.0));
        // The host doubled both, so the logical size is unchanged: exactly the
        // behaviour a VST3 host expects when the user moves the window to a
        // HiDPI display.
        assert_eq!(w.inner_size(), size(px(800.0), px(600.0)));
    }

    #[test]
    fn a_win32_foreign_handle_infers_its_display_handle() {
        let w = foreign(10, 10, 1.0);
        assert!(w.handle().raw_display_handle().is_some());
        let target = WindowTarget::from(w);
        let (display, window) = target.raw_handles().expect("win32 handles resolve");
        assert!(matches!(display, RawDisplayHandle::Windows(_)));
        assert!(matches!(window, RawWindowHandle::Win32(_)));
    }

    #[test]
    fn an_x11_foreign_handle_without_a_connection_is_rejected_clearly() {
        // X11 display handles carry a live connection pointer, so a window
        // handle alone is not enough. The failure has to be a clear error at
        // the seam, not a segfault inside the driver.
        let xlib = raw_window_handle::XlibWindowHandle::new(0x400001);
        // SAFETY: never dereferenced; the call under test fails before use.
        let handle = unsafe { ForeignWindowHandle::new(RawWindowHandle::Xlib(xlib)) };
        assert!(handle.raw_display_handle().is_none());
        let w = ForeignWindow::new(handle, size(DevicePx(1), DevicePx(1)), ScaleFactor::IDENTITY);
        let err = WindowTarget::from(w).raw_handles().unwrap_err();
        assert!(matches!(err, PlatformError::InvalidHandle(_)), "{err}");
        assert!(err.to_string().contains("display handle"));
    }

    #[test]
    fn window_target_forwards_geometry() {
        let target = WindowTarget::from(foreign(1920, 1080, 1.5));
        assert_eq!(target.physical_size(), size(DevicePx(1920), DevicePx(1080)));
        assert_eq!(target.scale_factor(), ScaleFactor::new(1.5));
        assert_eq!(target.inner_size(), size(px(1280.0), px(720.0)));
        // Presentation notification is a no-op for a foreign target, not a panic.
        target.pre_present_notify();
    }

    #[test]
    fn positions_keep_their_unit() {
        let logical = WindowPosition::Logical(point(px(10.0), px(20.0)));
        let physical = WindowPosition::Physical(point(DevicePx(10), DevicePx(20)));
        assert_ne!(format!("{logical:?}"), format!("{physical:?}"));
        assert_eq!(WindowLevel::default(), WindowLevel::Normal);
    }
}

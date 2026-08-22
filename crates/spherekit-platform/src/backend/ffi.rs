//! Every Win32 call the custom frame makes, behind one safe wrapper each.
//!
//! The decisions live in [`super::nc`] and are pure. This module is the only
//! place that talks to `user32`, and it exists so that the unsafe surface can be
//! counted: nine distinct invariants, listed here rather than rediscovered at
//! each call site.
//!
//! 1. **Subclass installation.** `SetWindowSubclass` takes an `Arc::into_raw`
//!    pointer. Ownership passes to the window and comes back exactly once, in
//!    `WM_NCDESTROY`.
//! 2. **The window procedure must not unwind.** Unwinding through a Win32
//!    dispatch frame is undefined behaviour, so the body has no indexing, no
//!    `unwrap`, and no allocation that could abort.
//! 3. **`refdata` round-trip.** The pointer handed back on every message is the
//!    one that went in, because nothing else ever calls `SetWindowSubclass`
//!    with this ID.
//! 4. **`rgrc[0]` mutation.** `WM_NCCALCSIZE` with `wparam != 0` points at an
//!    `NCCALCSIZE_PARAMS` whose first rectangle is writable for the duration of
//!    the message.
//! 5. **Menu handles are borrowed.** `GetSystemMenu(hwnd, FALSE)` returns a
//!    handle owned by the window; it must not be destroyed.
//! 6. **`TrackPopupMenu` re-enters.** It runs its own message loop, so the
//!    caller must hold no lock across it. `TPM_RETURNCMD` is used so the result
//!    comes back as a value rather than as a posted message.
//! 7. **Metrics are per-DPI.** `GetSystemMetricsForDpi` needs the window's
//!    current DPI, which changes under the application on a monitor move.
//! 8. **Thread affinity.** Every one of these is called either from the window
//!    procedure — which runs on the owning thread by construction — or from a
//!    `&self` method on a window that is not `Send`.
//! 9. **Null handles.** Every returned handle is checked before use; a failed
//!    query degrades to a documented fallback rather than to a crash.

#![cfg(windows)]

use super::nc::{self, FrameMetrics, HitTarget, MenuItemStates, RectI, ScreenEdge};
use crate::window::{CaptionRegions, WindowBackdrop, WindowChrome};
use spherekit_core::{Point, Px, Size};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::{
    ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETSTATE, ABM_GETTASKBARPOS, ABS_AUTOHIDE,
    APPBARDATA, DefSubclassProc, RemoveWindowSubclass, SHAppBarMessage, SetWindowSubclass,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnableMenuItem, GetSystemMenu, HMENU, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, NCCALCSIZE_PARAMS,
    PostMessageW, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_MOVE, SC_RESTORE, SC_SIZE,
    SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CYCAPTION, SM_CYSIZEFRAME, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_INITMENUPOPUP, WM_NCCALCSIZE,
    WM_NCDESTROY, WM_NCHITTEST, WM_NCRBUTTONUP, WM_SYSCOMMAND,
};

/// The subclass ID. Arbitrary but must be stable, because it is half the key
/// `RemoveWindowSubclass` matches on.
const SUBCLASS_ID: usize = 0x5048_4552; // "SPHER"

/// State the window procedure reads on every message.
///
/// The scalars are atomics because the procedure runs re-entrantly from inside
/// the platform's dispatch and cannot take a lock the interface thread might
/// hold. `regions` needs more than a word, so it sits behind a mutex the
/// procedure only ever *tries*: losing that race costs one frame of drag, which
/// is a far better outcome than a hung compositor.
#[derive(Debug)]
pub(crate) struct ChromeState {
    /// A [`WindowChrome`] discriminant.
    mode: AtomicU8,
    resizable: AtomicBool,
    maximized: AtomicBool,
    minimizable: AtomicBool,
    closable: AtomicBool,
    frame_x: AtomicI32,
    frame_y: AtomicI32,
    frame_caption: AtomicI32,
    /// `f32` scale factor, as bits.
    scale: AtomicU32,
    regions: Mutex<CaptionRegions>,
}

impl ChromeState {
    /// A state for a window that has not been measured yet.
    pub(crate) fn new(chrome: WindowChrome, resizable: bool, scale: f32) -> Self {
        Self {
            mode: AtomicU8::new(mode_bits(chrome)),
            resizable: AtomicBool::new(resizable),
            maximized: AtomicBool::new(false),
            minimizable: AtomicBool::new(true),
            closable: AtomicBool::new(true),
            frame_x: AtomicI32::new(FrameMetrics::AT_96_DPI.x),
            frame_y: AtomicI32::new(FrameMetrics::AT_96_DPI.y),
            frame_caption: AtomicI32::new(FrameMetrics::AT_96_DPI.caption),
            scale: AtomicU32::new(scale.to_bits()),
            regions: Mutex::new(CaptionRegions::default()),
        }
    }

    /// The active chrome mode.
    pub(crate) fn chrome(&self) -> WindowChrome {
        match self.mode.load(Ordering::Relaxed) {
            1 => WindowChrome::Custom,
            2 => WindowChrome::None,
            _ => WindowChrome::System,
        }
    }

    /// Switches the mode. The caller must force a frame recalculation after.
    pub(crate) fn set_chrome(&self, chrome: WindowChrome) {
        self.mode.store(mode_bits(chrome), Ordering::Relaxed);
    }

    /// Records whether the window is currently maximised.
    pub(crate) fn set_maximized(&self, maximized: bool) {
        self.maximized.store(maximized, Ordering::Relaxed);
    }

    /// Records whether the user may resize the window.
    pub(crate) fn set_resizable(&self, resizable: bool) {
        self.resizable.store(resizable, Ordering::Relaxed);
    }

    /// The scale factor used to convert hit-test coordinates.
    pub(crate) fn set_scale(&self, scale: f32) {
        self.scale.store(scale.to_bits(), Ordering::Relaxed);
    }

    fn scale(&self) -> f32 {
        let s = f32::from_bits(self.scale.load(Ordering::Relaxed));
        if s.is_finite() && s > 0.0 { s } else { 1.0 }
    }

    fn frame(&self) -> FrameMetrics {
        FrameMetrics {
            x: self.frame_x.load(Ordering::Relaxed),
            y: self.frame_y.load(Ordering::Relaxed),
            caption: self.frame_caption.load(Ordering::Relaxed),
        }
    }

    fn set_frame(&self, f: FrameMetrics) {
        self.frame_x.store(f.x, Ordering::Relaxed);
        self.frame_y.store(f.y, Ordering::Relaxed);
        self.frame_caption.store(f.caption, Ordering::Relaxed);
    }

    /// Replaces the published caption geometry.
    ///
    /// Blocking here is safe: this is called from the interface thread, and the
    /// only other user of the lock never blocks on it.
    pub(crate) fn publish_regions(&self, regions: &CaptionRegions) {
        if let Ok(mut slot) = self.regions.lock() {
            slot.clone_from(regions);
        }
    }

    pub(crate) fn menu_states(&self) -> MenuItemStates {
        nc::menu_states(
            self.maximized.load(Ordering::Relaxed),
            self.resizable.load(Ordering::Relaxed),
            self.minimizable.load(Ordering::Relaxed),
            self.closable.load(Ordering::Relaxed),
        )
    }
}

const fn mode_bits(chrome: WindowChrome) -> u8 {
    match chrome {
        WindowChrome::System => 0,
        WindowChrome::Custom => 1,
        WindowChrome::None => 2,
    }
}

/// DWM attribute added in Windows 11 22000.
const DWMWA_SYSTEM_BACKDROP_TYPE: u32 = 38;
/// Legacy Mica attribute used by early Windows 11 builds.
const DWMWA_MICA_EFFECT: u32 = 1029;
const DWMSBT_NONE: u32 = 1;
const DWMSBT_MAINWINDOW: u32 = 2;
const DWMSBT_TRANSIENTWINDOW: u32 = 3;

/// Applies the native compositor material behind a transparent client area.
///
/// The system-backdrop attribute is the supported Windows 11 path. The legacy
/// Mica attribute is attempted only for Mica when the newer attribute is not
/// available, which keeps older Windows 11 builds usable without affecting
/// Acrylic's semantics.
pub(crate) fn set_backdrop(hwnd: *mut core::ffi::c_void, backdrop: WindowBackdrop) -> bool {
    let system_type = match backdrop {
        WindowBackdrop::None => DWMSBT_NONE,
        WindowBackdrop::Mica => DWMSBT_MAINWINDOW,
        WindowBackdrop::Acrylic => DWMSBT_TRANSIENTWINDOW,
    };
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEM_BACKDROP_TYPE,
            (&system_type as *const u32).cast(),
            core::mem::size_of::<u32>() as u32,
        )
    };
    if result >= 0 {
        if backdrop == WindowBackdrop::None {
            let disabled: i32 = 0;
            // Clear the legacy flag as well. Some early Windows 11 builds keep
            // it alive after a newer system-backdrop value is reset.
            let _ = unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_MICA_EFFECT,
                    (&disabled as *const i32).cast(),
                    core::mem::size_of::<i32>() as u32,
                )
            };
        }
        return true;
    }

    if backdrop == WindowBackdrop::Mica {
        let enabled: i32 = 1;
        let legacy = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_MICA_EFFECT,
                (&enabled as *const i32).cast(),
                core::mem::size_of::<i32>() as u32,
            )
        };
        return legacy >= 0;
    }
    false
}

/// Frame thickness for a DPI.
///
/// `SM_CXSIZEFRAME` and `SM_CXPADDEDBORDER` do not scale linearly, so this is
/// queried rather than derived from the scale factor.
pub(crate) fn frame_metrics(dpi: u32) -> FrameMetrics {
    let dpi = if dpi == 0 { 96 } else { dpi };
    // SAFETY: `GetSystemMetricsForDpi` reads global metrics and writes nothing.
    // Every index is a documented `SM_*` constant, and the function returns 0
    // for an unknown one rather than failing.
    let (frame_x, frame_y, padded, caption) = unsafe {
        (
            GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi),
            GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi),
            GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi),
            GetSystemMetricsForDpi(SM_CYCAPTION, dpi),
        )
    };
    let metrics = FrameMetrics { x: frame_x + padded, y: frame_y + padded, caption };
    // A query that returned nothing usable would produce a zero-thickness
    // resize border, which cannot be grabbed at all.
    if metrics.x <= 0 || metrics.y <= 0 || metrics.caption <= 0 {
        FrameMetrics::AT_96_DPI
    } else {
        metrics
    }
}

/// The window's current DPI, or 96 if it cannot be determined.
pub(crate) fn window_dpi(hwnd: HWND) -> u32 {
    // SAFETY: reads a property of a window handle the caller owns. Returns 0 for
    // an invalid handle, which the caller turns into the default.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 96 } else { dpi }
}

/// Which edge an auto-hiding taskbar occupies, if one does.
fn autohide_edge() -> Option<ScreenEdge> {
    let mut data = APPBARDATA {
        cbSize: core::mem::size_of::<APPBARDATA>() as u32,
        hWnd: core::ptr::null_mut(),
        uCallbackMessage: 0,
        uEdge: 0,
        rc: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        lParam: 0,
    };
    // SAFETY: `ABM_GETSTATE` reads shell state and ignores every field but
    // `cbSize`, which is set correctly above.
    let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) };
    if state as u32 & ABS_AUTOHIDE == 0 {
        return None;
    }
    // SAFETY: `ABM_GETTASKBARPOS` fills `uEdge` and `rc` in the struct provided.
    // It returns zero on failure, in which case `uEdge` is left as initialised.
    let ok = unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut data) };
    if ok == 0 {
        return None;
    }
    Some(match data.uEdge {
        ABE_LEFT => ScreenEdge::Left,
        ABE_TOP => ScreenEdge::Top,
        ABE_RIGHT => ScreenEdge::Right,
        ABE_BOTTOM => ScreenEdge::Bottom,
        _ => return None,
    })
}

/// Forces the platform to recompute the non-client area now.
///
/// Without this a chrome change is invisible until the next resize.
pub(crate) fn recalculate_frame(hwnd: HWND) {
    // SAFETY: moves and resizes nothing — every geometry flag is suppressed —
    // and only asks for a frame recalculation on a window the caller owns.
    unsafe {
        SetWindowPos(
            hwnd,
            core::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOOWNERZORDER
                | SWP_NOACTIVATE
                | SWP_FRAMECHANGED,
        );
    }
}

/// Corrects a system menu for the window's actual state.
///
/// Windows only fixes its own menu up when it opens the menu itself, so this
/// has to run before any menu this code shows and on `WM_INITMENUPOPUP` for the
/// keyboard path.
fn apply_menu_states(menu: HMENU, states: MenuItemStates) {
    let flag = |on: bool| if on { MF_ENABLED } else { MF_GRAYED };
    for (cmd, on) in [
        (SC_RESTORE, states.restore),
        (SC_MOVE, states.move_),
        (SC_SIZE, states.size),
        (SC_MINIMIZE, states.minimize),
        (SC_MAXIMIZE, states.maximize),
        (SC_CLOSE, states.close),
    ] {
        // SAFETY: `menu` is the window's own system menu, borrowed and not
        // owned. Every command is a documented `SC_*` value, and the call is a
        // no-op returning -1 for an item the menu does not contain.
        unsafe {
            EnableMenuItem(menu, cmd, MF_BYCOMMAND | flag(on));
        }
    }
}

/// Opens the window menu at a screen position.
///
/// Returns `false` when the window has no system menu, which is the case for a
/// window created without `WS_SYSMENU`.
pub(crate) fn show_system_menu(hwnd: HWND, screen: POINT, states: MenuItemStates) -> bool {
    // SAFETY: `FALSE` asks for the existing menu rather than a reset one. The
    // handle is owned by the window and must not be destroyed; it is only read
    // and passed back to Win32 below.
    let menu = unsafe { GetSystemMenu(hwnd, 0) };
    if menu.is_null() {
        return false;
    }
    apply_menu_states(menu, states);

    // SAFETY: runs a nested message loop. No lock is held across this call —
    // the caller reads `ChromeState` into a `MenuItemStates` value first — so
    // the re-entrancy cannot deadlock against the window procedure.
    // `TPM_RETURNCMD` makes the choice a return value instead of a message,
    // which keeps the dispatch out of winit's queue entirely.
    let chosen = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN,
            screen.x,
            screen.y,
            0,
            hwnd,
            core::ptr::null(),
        )
    };
    if chosen != 0 {
        // Posted rather than sent: the command may destroy the window, and
        // returning through a nested loop into a dead window is how that turns
        // into a crash.
        // SAFETY: `WM_SYSCOMMAND` with an `SC_*` value is exactly what the menu
        // is documented to produce, and posting cannot re-enter.
        unsafe {
            PostMessageW(hwnd, WM_SYSCOMMAND, chosen as WPARAM, 0);
        }
    }
    true
}

/// Installs the custom-frame procedure on a window.
///
/// The returned handle keeps the state alive for the caller; the window holds
/// its own reference until `WM_NCDESTROY`.
pub(crate) fn install(hwnd: HWND, state: Arc<ChromeState>) -> Arc<ChromeState> {
    let metrics = frame_metrics(window_dpi(hwnd));
    state.set_frame(metrics);

    // The window takes one strong reference. It is reclaimed in WM_NCDESTROY,
    // which is guaranteed to be the last message a window receives and fires
    // whether SphereKit or a host destroyed it — which is why teardown hangs off
    // the message rather than off `Window::drop`, since winit owns the
    // `DestroyWindow` call.
    let raw = Arc::into_raw(Arc::clone(&state)) as usize;
    // SAFETY: `subclass_proc` matches the `SUBCLASSPROC` signature and does not
    // unwind. `raw` is a leaked `Arc` pointer whose ownership passes to the
    // window and returns exactly once.
    let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, raw) };
    if ok == 0 {
        // Installation failed, so nothing will ever reclaim the reference.
        // SAFETY: `raw` came from `Arc::into_raw` on this type and has not been
        // consumed, because `SetWindowSubclass` did not take it.
        unsafe {
            drop(Arc::from_raw(raw as *const ChromeState));
        }
    } else {
        recalculate_frame(hwnd);
    }
    state
}

/// The custom-frame window procedure.
///
/// # Safety
///
/// Installed by `SetWindowSubclass` and called only by the platform, on the
/// window's own thread. `refdata` is the `Arc::into_raw` pointer handed to
/// `SetWindowSubclass` and is valid until this function reclaims it in
/// `WM_NCDESTROY`.
///
/// The body is panic-free by construction: no indexing, no `unwrap`, and no
/// allocation. Unwinding through a Win32 dispatch frame is undefined behaviour.
unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    refdata: usize,
) -> LRESULT {
    // SAFETY: `refdata` is the pointer installed above and outlives every
    // message but the last. Borrowed, never dropped, except in WM_NCDESTROY.
    let state: &ChromeState = unsafe { &*(refdata as *const ChromeState) };

    match msg {
        WM_NCDESTROY => {
            // The last message a window ever receives. Reclaim the reference
            // first, then let the chain run so the subclass is removed.
            // SAFETY: balances the `Arc::into_raw` in `install`. This branch
            // runs exactly once per window, because WM_NCDESTROY is delivered
            // exactly once.
            unsafe {
                RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
                let owned = Arc::from_raw(refdata as *const ChromeState);
                drop(owned);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
        }

        WM_NCCALCSIZE if wparam != 0 && state.chrome() == WindowChrome::Custom => {
            // Let the platform compute the normal frame first, then give back
            // only the top edge. Everything else — the resize borders, snap,
            // the shadow, the maximised clamp — survives untouched.
            // SAFETY: `wparam != 0` means `lparam` is an `NCCALCSIZE_PARAMS`
            // whose `rgrc` is writable for the duration of this message.
            let params = unsafe { &mut *(lparam as *mut NCCALCSIZE_PARAMS) };
            let original_top = params.rgrc[0].top;
            // SAFETY: forwards the unmodified message down the chain; winit's
            // own handler passes it to `DefWindowProcW` because the window
            // still has its decorations flag set.
            let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };

            let proposed = RectI {
                left: params.rgrc[0].left,
                top: params.rgrc[0].top,
                right: params.rgrc[0].right,
                bottom: params.rgrc[0].bottom,
            };
            let maximized = is_zoomed(hwnd);
            state.set_maximized(maximized);
            let adjusted = nc::client_rect_for(
                WindowChrome::Custom,
                proposed,
                original_top,
                maximized,
                state.frame(),
                if maximized { autohide_edge() } else { None },
            );
            params.rgrc[0].left = adjusted.left;
            params.rgrc[0].top = adjusted.top;
            params.rgrc[0].right = adjusted.right;
            params.rgrc[0].bottom = adjusted.bottom;
            result
        }

        WM_NCHITTEST if state.chrome() != WindowChrome::System => {
            // SAFETY: asks the chain what it thinks first; under `Custom` that
            // answer is already right for the sides and the bottom.
            let fallback_raw = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            let fallback = HitTarget::from_ht(fallback_raw as i32).unwrap_or(HitTarget::Client);

            let Some(point) = client_point(hwnd, lparam, state.scale()) else {
                return fallback_raw;
            };
            let Some(client) = client_size(hwnd, state.scale()) else {
                return fallback_raw;
            };
            // Never blocks. Losing the race costs one frame of drag, which
            // beats blocking the compositor inside its own dispatch.
            let Ok(regions) = state.regions.try_lock() else {
                return fallback_raw;
            };
            let frame = state.frame();
            let border = Px(frame.y as f32 / state.scale().max(0.01));
            let target = nc::hit_test(
                fallback,
                point,
                client,
                &regions,
                state.chrome(),
                state.resizable.load(Ordering::Relaxed),
                state.maximized.load(Ordering::Relaxed),
                border,
            );
            target.to_ht() as LRESULT
        }

        WM_NCRBUTTONUP if state.chrome() != WindowChrome::System => {
            // A right-click the hit test resolved as caption. This is the whole
            // point of answering HTCAPTION for a client-area title bar: the
            // window menu keeps working exactly as it does on a real one.
            let hit = HitTarget::from_ht(wparam as i32);
            if matches!(hit, Some(HitTarget::Caption) | Some(HitTarget::SysMenu)) {
                let screen = POINT { x: loword(lparam), y: hiword(lparam) };
                if show_system_menu(hwnd, screen, state.menu_states()) {
                    return 0;
                }
            }
            // SAFETY: unhandled, so the chain gets it unmodified.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }

        WM_INITMENUPOPUP if state.chrome() != WindowChrome::System => {
            // The Alt+Space path. Windows opens its own menu here without
            // consulting anything, and leaves Move and Size enabled on a
            // maximised window, so the correction has to happen on the way in.
            let menu = wparam as HMENU;
            if !menu.is_null() && hiword(lparam) != 0 {
                state.set_maximized(is_zoomed(hwnd));
                apply_menu_states(menu, state.menu_states());
            }
            // SAFETY: the chain still needs to see it.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }

        // SAFETY: every other message passes through untouched.
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}

/// True when the window is maximised.
fn is_zoomed(hwnd: HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed;
    // SAFETY: reads a flag from a window handle the caller owns.
    unsafe { IsZoomed(hwnd) != 0 }
}

/// The hit-test point in client-logical coordinates.
fn client_point(hwnd: HWND, lparam: LPARAM, scale: f32) -> Option<Point<Px>> {
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    let mut p = POINT { x: loword(lparam), y: hiword(lparam) };
    // SAFETY: converts in place through a window handle the caller owns.
    // Returns zero on failure, leaving `p` unspecified, which the caller
    // treats as "no answer".
    if unsafe { ScreenToClient(hwnd, &mut p) } == 0 {
        return None;
    }
    Some(Point::new(Px(p.x as f32 / scale), Px(p.y as f32 / scale)))
}

/// The client area in logical pixels.
fn client_size(hwnd: HWND, scale: f32) -> Option<Size<Px>> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: fills the rect provided, from a window handle the caller owns.
    if unsafe { GetClientRect(hwnd, &mut r) } == 0 {
        return None;
    }
    Some(Size::new(Px((r.right - r.left) as f32 / scale), Px((r.bottom - r.top) as f32 / scale)))
}

/// The low word of an `LPARAM`, sign-extended as Win32 coordinates are.
#[inline]
const fn loword(lparam: LPARAM) -> i32 {
    (lparam & 0xFFFF) as i16 as i32
}

/// The high word of an `LPARAM`, sign-extended.
#[inline]
const fn hiword(lparam: LPARAM) -> i32 {
    ((lparam >> 16) & 0xFFFF) as i16 as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lparam_coordinates_are_sign_extended() {
        // A window dragged above the top of the primary monitor produces
        // negative coordinates, and a zero-extended read turns -1 into 65535.
        let packed = ((-3i32 as u32 & 0xFFFF) | ((-7i32 as u32 & 0xFFFF) << 16)) as LPARAM;
        assert_eq!(loword(packed), -3);
        assert_eq!(hiword(packed), -7);
    }

    #[test]
    fn chrome_modes_round_trip_through_their_atomic_representation() {
        for chrome in [WindowChrome::System, WindowChrome::Custom, WindowChrome::None] {
            let state = ChromeState::new(chrome, true, 1.0);
            assert_eq!(state.chrome(), chrome);
        }
    }

    #[test]
    fn a_state_starts_at_the_96_dpi_metrics_rather_than_at_zero() {
        // A zero-thickness resize border cannot be grabbed, so the fallback has
        // to be a usable frame rather than an empty one.
        let state = ChromeState::new(WindowChrome::Custom, true, 1.0);
        assert_eq!(state.frame(), FrameMetrics::AT_96_DPI);
    }

    #[test]
    fn an_invalid_scale_factor_falls_back_to_one() {
        // The scale is read on every hit test; dividing by zero there would put
        // an infinity into a coordinate comparison.
        let state = ChromeState::new(WindowChrome::Custom, true, 0.0);
        assert_eq!(state.scale(), 1.0);
        state.set_scale(f32::NAN);
        assert_eq!(state.scale(), 1.0);
        state.set_scale(2.0);
        assert_eq!(state.scale(), 2.0);
    }

    #[test]
    fn published_regions_are_readable_by_the_procedure() {
        let state = ChromeState::new(WindowChrome::Custom, true, 1.0);
        state.publish_regions(&CaptionRegions::strip(Px(800.0), Px(32.0)));
        let regions = state.regions.try_lock().expect("uncontended");
        assert!(regions.hits_caption(Point::new(Px(400.0), Px(16.0))));
    }

    #[test]
    fn menu_states_follow_the_recorded_window_state() {
        let state = ChromeState::new(WindowChrome::Custom, true, 1.0);
        assert!(state.menu_states().move_);
        state.set_maximized(true);
        let s = state.menu_states();
        assert!(!s.move_, "a maximised window cannot be moved");
        assert!(s.restore);
    }

    #[test]
    fn frame_metrics_are_never_zero_thickness() {
        // Whatever the platform reports, the result has to be grabbable.
        for dpi in [0u32, 96, 120, 144, 192] {
            let m = frame_metrics(dpi);
            assert!(m.x > 0 && m.y > 0 && m.caption > 0, "{dpi} dpi gave {m:?}");
        }
    }

    #[test]
    fn frame_metrics_grow_with_dpi() {
        // Not a linear relationship, which is exactly why it is queried rather
        // than derived from the scale factor.
        let low = frame_metrics(96);
        let high = frame_metrics(192);
        assert!(high.caption > low.caption, "{low:?} vs {high:?}");
    }
}

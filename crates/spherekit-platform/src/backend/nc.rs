//! Non-client decisions for a custom window frame, as pure functions.
//!
//! Windows asks a window where its edges are by sending `WM_NCHITTEST`, and how
//! big its client area is by sending `WM_NCCALCSIZE`. Both are answered
//! synchronously from inside the platform's own dispatch, so neither can call
//! back into the interface. Everything they need is therefore reduced here to
//! arithmetic over values that were published earlier.
//!
//! Nothing in this module links `user32`. That is deliberate and follows the
//! same precedent as `translate_key_input`, which takes pieces rather than a
//! `winit::event::KeyEvent` so it can be tested without an event loop, and
//! `frame_is_due`, which is a free function so the idle guarantee can be tested
//! without a window. Every unsafe call lives in [`super::ffi`].
//!
//! ## Why the frame is restored rather than stripped
//!
//! The obvious way to build a borderless window is to answer `WM_NCCALCSIZE`
//! with the whole window rect. It works, and it silently throws away four
//! things that are expensive to reimplement and free to keep: the resize
//! borders on three sides, Aero Snap, the drop shadow, and the clamp that stops
//! a maximised window covering the taskbar.
//!
//! So [`client_rect_for`] instead lets `DefWindowProcW` compute the normal
//! frame and then restores **only the top edge**. That removes the caption and
//! nothing else. `DefWindowProcW` goes on answering `HTLEFT`, `HTRIGHT`,
//! `HTBOTTOM` and both bottom corners by itself, and [`hit_test`] only has to
//! deal with the top edge, the two top corners and the caption band.

use crate::window::{CaptionRegions, WindowChrome};
use spherekit_core::{Point, Px, Size};

/// A rectangle in physical pixels, matching Win32's `RECT` layout.
///
/// Its own type rather than the platform's, so the functions below can be
/// tested on any host.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct RectI {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

/// Frame thickness in physical pixels, for one DPI.
///
/// Queried from the platform rather than derived from the scale factor:
/// `SM_CXSIZEFRAME` and `SM_CXPADDEDBORDER` do not scale linearly, so
/// `8 * scale_factor` is wrong at every DPI but 96. Recompute on
/// `WM_DPICHANGED` rather than when `ScaleFactorChanged` reaches the
/// application, because that arrives later and the frame is already wrong.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct FrameMetrics {
    /// Horizontal resize border.
    pub x: i32,
    /// Vertical resize border.
    pub y: i32,
    /// Caption height, without the frame.
    pub caption: i32,
}

impl FrameMetrics {
    /// The values Windows reports at 96 dpi, for tests and for the fallback
    /// when a metric query fails.
    pub(crate) const AT_96_DPI: Self = Self { x: 8, y: 8, caption: 31 };

    /// How much taller the client area is than the frame Windows proposed.
    ///
    /// The caption plus the top resize border: exactly what restoring the top
    /// edge of `WM_NCCALCSIZE`'s proposed rectangle gives back. Anything that
    /// converts between an inner size and an outer size has to account for it,
    /// because the platform still believes the caption is there.
    #[inline]
    pub(crate) const fn reclaimed_top(self) -> i32 {
        self.caption + self.y
    }
}

/// Which part of the window a point belongs to.
///
/// An enum rather than a raw `HT*` code, so the decision is testable without
/// linking `user32` and so an invalid code cannot be invented by arithmetic.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum HitTarget {
    /// Ordinary content.
    Client,
    /// Behaves as a title bar.
    Caption,
    /// The system-menu affordance.
    SysMenu,
    /// Left resize border.
    Left,
    /// Right resize border.
    Right,
    /// Top resize border.
    Top,
    /// Bottom resize border.
    Bottom,
    /// Top-left resize corner.
    TopLeft,
    /// Top-right resize corner.
    TopRight,
    /// Bottom-left resize corner.
    BottomLeft,
    /// Bottom-right resize corner.
    BottomRight,
}

impl HitTarget {
    /// The `HT*` constant Windows expects back from `WM_NCHITTEST`.
    pub(crate) const fn to_ht(self) -> i32 {
        match self {
            HitTarget::Client => 1,
            HitTarget::Caption => 2,
            HitTarget::SysMenu => 3,
            HitTarget::Left => 10,
            HitTarget::Right => 11,
            HitTarget::Top => 12,
            HitTarget::TopLeft => 13,
            HitTarget::TopRight => 14,
            HitTarget::Bottom => 15,
            HitTarget::BottomLeft => 16,
            HitTarget::BottomRight => 17,
        }
    }

    /// The inverse, for reading what `DefSubclassProc` decided.
    pub(crate) const fn from_ht(ht: i32) -> Option<Self> {
        Some(match ht {
            1 => HitTarget::Client,
            2 => HitTarget::Caption,
            3 => HitTarget::SysMenu,
            10 => HitTarget::Left,
            11 => HitTarget::Right,
            12 => HitTarget::Top,
            13 => HitTarget::TopLeft,
            14 => HitTarget::TopRight,
            15 => HitTarget::Bottom,
            16 => HitTarget::BottomLeft,
            17 => HitTarget::BottomRight,
            _ => return None,
        })
    }

    /// True when this target starts a resize.
    pub(crate) const fn is_resize(self) -> bool {
        matches!(
            self,
            HitTarget::Left
                | HitTarget::Right
                | HitTarget::Top
                | HitTarget::Bottom
                | HitTarget::TopLeft
                | HitTarget::TopRight
                | HitTarget::BottomLeft
                | HitTarget::BottomRight
        )
    }
}

/// Which monitor edge an auto-hiding taskbar is on.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum ScreenEdge {
    /// Left.
    Left,
    /// Top.
    Top,
    /// Right.
    Right,
    /// Bottom.
    Bottom,
}

/// The client rectangle `WM_NCCALCSIZE` should install.
///
/// `proposed` is what `DefWindowProcW` computed; `original_top` is the top edge
/// of the window rectangle before it ran.
///
/// For [`WindowChrome::Custom`] only the top edge is restored, which removes the
/// caption and keeps everything else. When maximised the top is instead pushed
/// in by the frame thickness: measured, that lands the client at exactly the
/// monitor's work area with no `MonitorFromWindow` call, and is correct for a
/// taskbar on any edge.
///
/// `autohide` shrinks that edge by one pixel. An auto-hiding taskbar makes the
/// work area equal the whole monitor, so a flush maximised window covers the
/// taskbar with no way to summon it back — one pixel of overlap is what lets
/// the mouse reach it.
pub(crate) const fn client_rect_for(
    chrome: WindowChrome,
    proposed: RectI,
    original_top: i32,
    maximized: bool,
    frame: FrameMetrics,
    autohide: Option<ScreenEdge>,
) -> RectI {
    let mut r = proposed;
    match chrome {
        // The platform's own answer, untouched.
        WindowChrome::System => return r,
        // winit strips the whole frame itself for a decorationless window;
        // second-guessing it here would fight it.
        WindowChrome::None => return r,
        WindowChrome::Custom => {}
    }

    if maximized {
        r.top = original_top + frame.y;
    } else {
        r.top = original_top;
    }

    if maximized {
        match autohide {
            Some(ScreenEdge::Top) => r.top += 1,
            Some(ScreenEdge::Bottom) => r.bottom -= 1,
            Some(ScreenEdge::Left) => r.left += 1,
            Some(ScreenEdge::Right) => r.right -= 1,
            None => {}
        }
    }
    r
}

/// The answer to `WM_NCHITTEST`, in client-logical coordinates.
///
/// `fallback` is whatever `DefSubclassProc` said. Under
/// [`WindowChrome::Custom`] that is already correct for the sides, the bottom
/// and both bottom corners, so this only overrides where it has to.
///
/// Resize bands are gated on `!maximized`: a maximised window has no resize
/// edges, and an ungated band would steal the first few pixels of the caption
/// from whatever buttons sit there.
///
/// Exclusions beat drag regions. A caption button that resolved to
/// [`HitTarget::Caption`] would have its press swallowed by the modal move loop
/// and would never see a click at all.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hit_test(
    fallback: HitTarget,
    point: Point<Px>,
    client: Size<Px>,
    regions: &CaptionRegions,
    chrome: WindowChrome,
    resizable: bool,
    maximized: bool,
    border: Px,
) -> HitTarget {
    if chrome == WindowChrome::System {
        return fallback;
    }

    let (x, y) = (point.x.get(), point.y.get());
    let (w, h) = (client.width.get(), client.height.get());
    let b = border.get().max(1.0);

    if resizable && !maximized {
        // The top edge and both top corners are the part the restored frame
        // gave back, so they are ours to answer. Corners take priority over
        // edges or a diagonal grab would be impossible.
        let top = y < b;
        let left = x < b;
        let right = x >= w - b;
        if top && left {
            return HitTarget::TopLeft;
        }
        if top && right {
            return HitTarget::TopRight;
        }
        if top {
            return HitTarget::Top;
        }
        // `None` strips the whole frame, so nothing else answers the remaining
        // edges either and this has to do all of them.
        if chrome == WindowChrome::None {
            let bottom = y >= h - b;
            if bottom && left {
                return HitTarget::BottomLeft;
            }
            if bottom && right {
                return HitTarget::BottomRight;
            }
            if bottom {
                return HitTarget::Bottom;
            }
            if left {
                return HitTarget::Left;
            }
            if right {
                return HitTarget::Right;
            }
        }
    }

    // Anything the platform already resolved to a frame part wins over the
    // caption: the resize border overlaps the caption band at the top corners,
    // and resizing is the less recoverable gesture to lose.
    if fallback.is_resize() {
        return fallback;
    }

    if regions.hits_caption(point) {
        return HitTarget::Caption;
    }
    HitTarget::Client
}

/// Which system-menu items a window in this state may offer.
///
/// Windows fixes its own menu up only when *it* opens the menu. Measured: while
/// a window was maximised, `GetMenuState` still reported Move and Size as
/// enabled. So every menu this code opens has to be corrected first, or it
/// offers actions that do nothing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct MenuItemStates {
    /// Restore down.
    pub restore: bool,
    /// Move by keyboard.
    pub move_: bool,
    /// Resize by keyboard.
    pub size: bool,
    /// Minimise.
    pub minimize: bool,
    /// Maximise.
    pub maximize: bool,
    /// Close.
    pub close: bool,
}

/// Corrects the menu for the window's actual state.
pub(crate) const fn menu_states(
    maximized: bool,
    resizable: bool,
    minimizable: bool,
    closable: bool,
) -> MenuItemStates {
    MenuItemStates {
        restore: maximized,
        // A maximised window cannot be moved or resized in place; offering
        // either does nothing and looks broken.
        move_: !maximized,
        size: resizable && !maximized,
        minimize: minimizable,
        maximize: resizable && !maximized,
        close: closable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{Rect, px, size};

    fn client() -> Size<Px> {
        size(px(800.0), px(600.0))
    }

    fn caption() -> CaptionRegions {
        CaptionRegions::strip(px(800.0), px(32.0))
    }

    fn hit(x: f32, y: f32, fallback: HitTarget, maximized: bool) -> HitTarget {
        hit_test(
            fallback,
            Point::new(px(x), px(y)),
            client(),
            &caption(),
            WindowChrome::Custom,
            true,
            maximized,
            px(8.0),
        )
    }

    // -------------------------------------------------------- hit testing

    #[test]
    fn the_caption_band_drags_the_window() {
        assert_eq!(hit(400.0, 16.0, HitTarget::Client, false), HitTarget::Caption);
    }

    #[test]
    fn content_below_the_caption_is_client() {
        assert_eq!(hit(400.0, 200.0, HitTarget::Client, false), HitTarget::Client);
    }

    #[test]
    fn the_top_edge_resizes_even_though_it_is_inside_the_caption() {
        // The restored top edge is the part the platform no longer answers, so
        // this is the band that only exists because we put it back.
        assert_eq!(hit(400.0, 2.0, HitTarget::Client, false), HitTarget::Top);
        assert_eq!(hit(2.0, 2.0, HitTarget::Client, false), HitTarget::TopLeft);
        assert_eq!(hit(798.0, 2.0, HitTarget::Client, false), HitTarget::TopRight);
    }

    #[test]
    fn a_maximised_window_has_no_resize_bands_left_to_steal_the_caption() {
        // Ungated, the band would take the first eight pixels of a maximised
        // window's caption away from its buttons.
        assert_eq!(hit(400.0, 2.0, HitTarget::Client, true), HitTarget::Caption);
        assert_eq!(hit(2.0, 2.0, HitTarget::Client, true), HitTarget::Caption);
    }

    #[test]
    fn a_non_resizable_window_gets_no_resize_bands() {
        let target = hit_test(
            HitTarget::Client,
            Point::new(px(400.0), px(2.0)),
            client(),
            &caption(),
            WindowChrome::Custom,
            false,
            false,
            px(8.0),
        );
        assert_eq!(target, HitTarget::Caption);
    }

    #[test]
    fn a_caption_button_receives_its_click_rather_than_starting_a_drag() {
        // The failure this prevents is total, not cosmetic: a press routed as
        // caption is swallowed by the modal move loop and the button never sees
        // a click at all.
        let regions = caption()
            .with_button(Rect::new(Point::new(px(740.0), px(0.0)), size(px(60.0), px(32.0))));
        let target = hit_test(
            HitTarget::Client,
            Point::new(px(760.0), px(16.0)),
            client(),
            &regions,
            WindowChrome::Custom,
            true,
            false,
            px(8.0),
        );
        assert_eq!(target, HitTarget::Client);
    }

    #[test]
    fn the_platforms_own_frame_answer_wins_over_the_caption() {
        // The resize border and the caption band overlap at the top corners.
        // Losing a resize is worse than losing a drag, because a drag has the
        // whole rest of the caption to start from.
        assert_eq!(hit(4.0, 20.0, HitTarget::Left, false), HitTarget::Left);
        assert_eq!(hit(400.0, 596.0, HitTarget::Bottom, false), HitTarget::Bottom);
    }

    #[test]
    fn system_chrome_never_overrides_the_platform() {
        let target = hit_test(
            HitTarget::Client,
            Point::new(px(400.0), px(2.0)),
            client(),
            &caption(),
            WindowChrome::System,
            true,
            false,
            px(8.0),
        );
        assert_eq!(target, HitTarget::Client, "System must defer entirely");
    }

    #[test]
    fn stripped_chrome_answers_every_edge_itself() {
        // Nothing else does: a fully stripped frame reports the whole window as
        // client area.
        let check = |x: f32, y: f32| {
            hit_test(
                HitTarget::Client,
                Point::new(px(x), px(y)),
                client(),
                &CaptionRegions::default(),
                WindowChrome::None,
                true,
                false,
                px(8.0),
            )
        };
        assert_eq!(check(2.0, 300.0), HitTarget::Left);
        assert_eq!(check(798.0, 300.0), HitTarget::Right);
        assert_eq!(check(400.0, 598.0), HitTarget::Bottom);
        assert_eq!(check(2.0, 598.0), HitTarget::BottomLeft);
        assert_eq!(check(798.0, 598.0), HitTarget::BottomRight);
        assert_eq!(check(400.0, 300.0), HitTarget::Client);
    }

    #[test]
    fn a_zero_border_still_leaves_a_grabbable_band() {
        // Clamped to one pixel: a zero-width resize band cannot be hit, and a
        // window that cannot be resized at all is worse than one whose edge is
        // fiddly.
        let target = hit_test(
            HitTarget::Client,
            Point::new(px(400.0), px(0.0)),
            client(),
            &caption(),
            WindowChrome::Custom,
            true,
            false,
            Px::ZERO,
        );
        assert_eq!(target, HitTarget::Top);
    }

    // ------------------------------------------------------------ ht codes

    #[test]
    fn every_hit_target_round_trips_through_its_win32_code() {
        for t in [
            HitTarget::Client,
            HitTarget::Caption,
            HitTarget::SysMenu,
            HitTarget::Left,
            HitTarget::Right,
            HitTarget::Top,
            HitTarget::Bottom,
            HitTarget::TopLeft,
            HitTarget::TopRight,
            HitTarget::BottomLeft,
            HitTarget::BottomRight,
        ] {
            assert_eq!(HitTarget::from_ht(t.to_ht()), Some(t), "{t:?}");
        }
        assert_eq!(HitTarget::from_ht(-1), None, "HTNOWHERE is not one of ours");
        assert_eq!(HitTarget::from_ht(20), None);
    }

    #[test]
    fn the_hit_codes_match_the_win32_constants() {
        // Hard-coded on purpose: these are ABI, and a transcription error here
        // would be a window whose edges are subtly wrong rather than a crash.
        assert_eq!(HitTarget::Client.to_ht(), 1, "HTCLIENT");
        assert_eq!(HitTarget::Caption.to_ht(), 2, "HTCAPTION");
        assert_eq!(HitTarget::SysMenu.to_ht(), 3, "HTSYSMENU");
        assert_eq!(HitTarget::TopLeft.to_ht(), 13, "HTTOPLEFT");
        assert_eq!(HitTarget::BottomRight.to_ht(), 17, "HTBOTTOMRIGHT");
    }

    // ------------------------------------------------------- client rect

    #[test]
    fn a_custom_frame_gives_back_the_caption_and_nothing_else() {
        let proposed = RectI { left: 8, top: 39, right: 792, bottom: 592 };
        let r = client_rect_for(
            WindowChrome::Custom,
            proposed,
            0,
            false,
            FrameMetrics::AT_96_DPI,
            None,
        );
        assert_eq!(r.top, 0, "the caption is gone");
        assert_eq!((r.left, r.right, r.bottom), (8, 792, 592), "the sides are untouched");
    }

    #[test]
    fn system_and_stripped_frames_are_left_exactly_as_proposed() {
        let proposed = RectI { left: 8, top: 39, right: 792, bottom: 592 };
        for chrome in [WindowChrome::System, WindowChrome::None] {
            let r = client_rect_for(chrome, proposed, 0, false, FrameMetrics::AT_96_DPI, None);
            assert_eq!(r, proposed, "{chrome:?} must not be second-guessed");
        }
    }

    #[test]
    fn a_maximised_custom_frame_lands_on_the_work_area() {
        // Restoring the top outright would put the client above the monitor,
        // because a maximised window's rect overhangs on every side.
        let proposed = RectI { left: 0, top: 31, right: 1920, bottom: 1040 };
        let r = client_rect_for(
            WindowChrome::Custom,
            proposed,
            -8,
            true,
            FrameMetrics::AT_96_DPI,
            None,
        );
        assert_eq!(r.top, 0, "the client starts at the work area, not above it");
    }

    #[test]
    fn an_autohiding_taskbar_keeps_one_pixel_to_be_summoned_from() {
        // With an auto-hiding taskbar the work area is the whole monitor, so a
        // flush window covers it and there is no way to get it back.
        let proposed = RectI { left: 0, top: 31, right: 1920, bottom: 1080 };
        let r = client_rect_for(
            WindowChrome::Custom,
            proposed,
            -8,
            true,
            FrameMetrics::AT_96_DPI,
            Some(ScreenEdge::Bottom),
        );
        assert_eq!(r.bottom, 1079);

        let r = client_rect_for(
            WindowChrome::Custom,
            proposed,
            -8,
            true,
            FrameMetrics::AT_96_DPI,
            Some(ScreenEdge::Left),
        );
        assert_eq!(r.left, 1);
    }

    #[test]
    fn autohide_only_applies_when_maximised() {
        // A restored window does not cover the taskbar, so stealing a pixel
        // from it would be a visible bug for nothing.
        let proposed = RectI { left: 8, top: 39, right: 792, bottom: 592 };
        let r = client_rect_for(
            WindowChrome::Custom,
            proposed,
            0,
            false,
            FrameMetrics::AT_96_DPI,
            Some(ScreenEdge::Bottom),
        );
        assert_eq!(r.bottom, 592);
    }

    #[test]
    fn the_reclaimed_height_is_the_caption_plus_the_top_border() {
        // Anything converting between an inner and an outer size has to add
        // this back, because the platform still believes the caption is there.
        assert_eq!(FrameMetrics::AT_96_DPI.reclaimed_top(), 39);
    }

    // ------------------------------------------------------------- menu

    #[test]
    fn a_maximised_window_may_not_be_moved_or_resized() {
        // Windows leaves these enabled until it opens the menu itself, so an
        // uncorrected menu offers actions that silently do nothing.
        let s = menu_states(true, true, true, true);
        assert!(!s.move_);
        assert!(!s.size);
        assert!(!s.maximize);
        assert!(s.restore);
        assert!(s.minimize);
        assert!(s.close);
    }

    #[test]
    fn a_restored_window_may_not_be_restored() {
        let s = menu_states(false, true, true, true);
        assert!(!s.restore);
        assert!(s.move_);
        assert!(s.size);
        assert!(s.maximize);
    }

    #[test]
    fn a_fixed_size_window_offers_neither_size_nor_maximise() {
        let s = menu_states(false, false, true, true);
        assert!(!s.size);
        assert!(!s.maximize);
        assert!(s.move_, "a fixed-size window can still be moved");
    }

    #[test]
    fn a_caption_with_no_regions_hits_nothing() {
        let empty = CaptionRegions::default();
        assert!(empty.is_empty());
        assert!(!empty.hits_caption(Point::new(px(10.0), px(10.0))));
    }

    #[test]
    fn an_exclusion_outside_every_drag_region_changes_nothing() {
        let regions = CaptionRegions::strip(px(800.0), px(32.0))
            .with_button(Rect::new(Point::new(px(0.0), px(500.0)), size(px(50.0), px(50.0))));
        assert!(regions.hits_caption(Point::new(px(400.0), px(16.0))));
    }
}

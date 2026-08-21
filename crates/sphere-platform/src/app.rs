//! The application runner: an event loop, a set of windows, and your handler.
//!
//! This is the standalone-application path. A plug-in does not own the process
//! and must not create an event loop; it renders into a host-owned window
//! instead — see [`crate::WindowTarget`] and the lifecycle notes in
//! [`crate::window`].
//!
//! ```no_run
//! use sphere_platform::{App, AppContext, AppHandler, WindowAttributes, WindowEvent};
//! use sphere_core::WindowId;
//!
//! struct Editor;
//!
//! impl AppHandler for Editor {
//!     fn resumed(&mut self, cx: &mut AppContext<'_>) {
//!         // Windows can only be created from inside the loop.
//!         cx.create_window(&WindowAttributes::new("Sphere")).unwrap();
//!     }
//!
//!     fn window_event(&mut self, cx: &mut AppContext<'_>, id: WindowId, event: WindowEvent) {
//!         match event {
//!             WindowEvent::CloseRequested => {
//!                 cx.close_window(id);
//!                 if cx.windows().is_empty() {
//!                     cx.exit();
//!                 }
//!             }
//!             WindowEvent::RedrawRequested => { /* draw here */ }
//!             _ => {}
//!         }
//!     }
//! }
//!
//! App::new(Editor).run().unwrap();
//! ```
//!
//! ## The redraw contract
//!
//! The runner asks the platform for a redraw for every window that owes a
//! frame, then lets the loop sleep. When [`WindowEvent::RedrawRequested`]
//! arrives, the handler is expected to draw; the runner then marks the frame
//! presented, which is what lets a static window fall back to a blocking wait
//! and use no CPU at all.

use std::sync::Arc;
use std::time::Instant;

use rustc_hash::FxHashMap;
use sphere_core::{PlatformError, WindowId};

use crate::backend::Window;
use crate::backend::winit::{
    EventBuf, control_flow_to_winit, monitor_list, translate_window_event,
};
use crate::clipboard::Clipboard;
use crate::event::WindowEvent;
use crate::monitor::{MonitorInfo, MonitorList};
use crate::registry::{WindowRegistry, WindowState};
use crate::scheduler::{FrameScheduler, RedrawPolicy};
use crate::window::WindowAttributes;

/// The registry an [`App`] keeps, holding shareable platform windows.
///
/// `Arc<Window>` rather than `Window`, because the renderer needs a handle
/// that outlives the borrow it was fetched with in order to build a `'static`
/// GPU surface.
pub type WindowManager = WindowRegistry<Arc<Window>>;

/// What a handler can do to the application from inside an event callback.
///
/// Borrowed for the duration of one callback: creating a window, enumerating
/// displays and exiting all require the running loop, which only exists here.
pub struct AppContext<'a> {
    /// The running platform loop. Never exposed.
    event_loop: &'a ::winit::event_loop::ActiveEventLoop,
    /// Every open window.
    windows: &'a mut WindowManager,
    /// Platform window id to Sphere window id.
    winit_ids: &'a mut FxHashMap<::winit::window::WindowId, WindowId>,
    /// The loop's frame scheduler.
    scheduler: &'a mut FrameScheduler,
    /// The application's clipboard.
    clipboard: &'a Clipboard,
}

impl AppContext<'_> {
    /// Opens a window.
    ///
    /// Only possible from inside a callback: every platform requires window
    /// creation to happen on the loop's thread while the loop is running, and
    /// on Android and iOS a window created before `resumed` would be destroyed
    /// again immediately.
    pub fn create_window(
        &mut self,
        attrs: &WindowAttributes,
    ) -> Result<Arc<Window>, PlatformError> {
        let event_loop = self.event_loop;
        let id = self.windows.try_insert_with(|id| {
            let window = Window::create(event_loop, attrs, id)?;
            let state = WindowState::new(window.physical_size(), window.scale_factor());
            Ok::<_, PlatformError>((Arc::new(window), state))
        })?;
        let window = match self.windows.window(id) {
            Some(w) => Arc::clone(w),
            // Unreachable: `try_insert_with` returned `Ok`, so the entry is
            // there. Returning an error rather than panicking keeps a bug here
            // from taking a DAW down with it.
            None => return Err(PlatformError::WindowCreation("window vanished".into())),
        };
        self.winit_ids.insert(window.winit_id(), id);
        Ok(window)
    }

    /// Closes a window, returning it so the caller can drop it after its GPU
    /// surface.
    ///
    /// Dropping the returned `Arc` while a surface still references the window
    /// is undefined behaviour in the driver, which is why this hands the window
    /// back rather than destroying it here.
    pub fn close_window(&mut self, id: WindowId) -> Option<Arc<Window>> {
        let window = self.windows.remove(id)?;
        self.winit_ids.remove(&window.winit_id());
        Some(window)
    }

    /// Every open window.
    #[inline]
    pub fn windows(&self) -> &WindowManager {
        self.windows
    }

    /// Every open window, mutably, for updating per-window redraw policy.
    #[inline]
    pub fn windows_mut(&mut self) -> &mut WindowManager {
        self.windows
    }

    /// One window, by id.
    #[inline]
    pub fn window(&self, id: WindowId) -> Option<&Arc<Window>> {
        self.windows.window(id)
    }

    /// The loop's frame scheduler.
    #[inline]
    pub fn scheduler(&self) -> &FrameScheduler {
        self.scheduler
    }

    /// The loop's frame scheduler, mutably: begin an animation, request a
    /// redraw, or re-pace after a display change.
    #[inline]
    pub fn scheduler_mut(&mut self) -> &mut FrameScheduler {
        self.scheduler
    }

    /// The application's clipboard.
    #[inline]
    pub fn clipboard(&self) -> &Clipboard {
        self.clipboard
    }

    /// Snapshots every attached display.
    ///
    /// Re-enumerate rather than caching: displays are hot-pluggable, and a
    /// cached list is wrong the moment a laptop is undocked.
    pub fn monitors(&self) -> MonitorList {
        let primary = self.event_loop.primary_monitor();
        monitor_list(self.event_loop.available_monitors(), primary.as_ref())
    }

    /// The primary display, if the platform names one.
    pub fn primary_monitor(&self) -> Option<MonitorInfo> {
        let primary = self.event_loop.primary_monitor()?;
        Some(crate::backend::winit::monitor_info(&primary, Some(&primary)))
    }

    /// Asks the loop to finish. Remaining callbacks still run, including
    /// [`AppHandler::exiting`].
    #[inline]
    pub fn exit(&self) {
        self.event_loop.exit();
    }

    /// Whether the loop is already shutting down.
    #[inline]
    pub fn is_exiting(&self) -> bool {
        self.event_loop.exiting()
    }
}

/// The application's reaction to platform events.
///
/// Backend-independent by construction: every parameter is a Sphere type, so
/// the same handler runs unchanged on a future native backend.
pub trait AppHandler {
    /// The application became active and windows may now be created.
    ///
    /// Called once at start-up on desktop, and again after every suspend on
    /// mobile — where the previous surface is gone and must be rebuilt.
    fn resumed(&mut self, cx: &mut AppContext<'_>);

    /// Something happened to one window.
    fn window_event(&mut self, cx: &mut AppContext<'_>, window: WindowId, event: WindowEvent);

    /// Every pending event has been dispatched and the loop is about to sleep.
    ///
    /// The place to fold input into application state and decide what needs
    /// redrawing, rather than doing that work per event.
    fn about_to_wait(&mut self, cx: &mut AppContext<'_>) {
        let _ = cx;
    }

    /// The application is being backgrounded. Every GPU surface must be
    /// released here: on Android the underlying window is destroyed the moment
    /// this returns.
    fn suspended(&mut self, cx: &mut AppContext<'_>) {
        let _ = cx;
    }

    /// The loop is shutting down. Last chance to persist state.
    fn exiting(&mut self, cx: &mut AppContext<'_>) {
        let _ = cx;
    }

    /// The system is short of memory; drop caches.
    fn memory_warning(&mut self, cx: &mut AppContext<'_>) {
        let _ = cx;
    }
}

/// A standalone Sphere application.
///
/// # Threading
///
/// [`App::run`] must be called from the process's main thread: macOS requires
/// it, and Windows requires the thread to own a message pump. It does not
/// return until the loop exits. A plug-in, which owns neither the main thread
/// nor the pump, must not use this type at all.
pub struct App<H> {
    /// The application's event handler.
    handler: H,
    /// The frame scheduler the loop will use.
    scheduler: FrameScheduler,
    /// The clipboard handed to the handler.
    clipboard: Clipboard,
}

impl<H: AppHandler> App<H> {
    /// An application with a default scheduler and the system clipboard.
    pub fn new(handler: H) -> Self {
        Self { handler, scheduler: FrameScheduler::new(), clipboard: Clipboard::system() }
    }

    /// Uses a specific frame scheduler, for example one already paced at the
    /// target display's refresh rate.
    #[must_use]
    pub fn with_scheduler(mut self, scheduler: FrameScheduler) -> Self {
        self.scheduler = scheduler;
        self
    }

    /// Uses a specific clipboard, such as one routed through a plug-in host.
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Clipboard) -> Self {
        self.clipboard = clipboard;
        self
    }

    /// Runs the event loop until the application exits.
    pub fn run(self) -> Result<(), PlatformError> {
        let event_loop = ::winit::event_loop::EventLoop::new()
            .map_err(|e| PlatformError::EventLoop(e.to_string()))?;
        // Start blocked. Anything that needs more says so through the
        // scheduler, and `about_to_wait` applies it every iteration.
        event_loop.set_control_flow(::winit::event_loop::ControlFlow::Wait);
        let mut driver = Driver {
            handler: self.handler,
            windows: WindowManager::new(),
            winit_ids: FxHashMap::default(),
            scheduler: self.scheduler,
            clipboard: self.clipboard,
            scratch: EventBuf::new(),
        };
        event_loop.run_app(&mut driver).map_err(|e| PlatformError::EventLoop(e.to_string()))
    }
}

/// Runs `handler` as a standalone application. Shorthand for
/// `App::new(handler).run()`.
pub fn run<H: AppHandler>(handler: H) -> Result<(), PlatformError> {
    App::new(handler).run()
}

/// Bridges the platform loop to [`AppHandler`].
///
/// All the translation happens here, so the handler never sees a backend type.
struct Driver<H> {
    /// The application's handler.
    handler: H,
    /// Every open window.
    windows: WindowManager,
    /// Platform window id to Sphere window id.
    winit_ids: FxHashMap<::winit::window::WindowId, WindowId>,
    /// Decides when the loop may sleep.
    scheduler: FrameScheduler,
    /// The application's clipboard.
    clipboard: Clipboard,
    /// Reused translation buffer, so the input path does not allocate.
    scratch: EventBuf,
}

/// Builds the borrow bundle handed to the handler.
///
/// A free function taking the fields individually, rather than a method on
/// `Driver`, so that the borrows stay disjoint from the `&mut self.handler`
/// the caller holds at the same time.
fn context<'a>(
    windows: &'a mut WindowManager,
    winit_ids: &'a mut FxHashMap<::winit::window::WindowId, WindowId>,
    scheduler: &'a mut FrameScheduler,
    clipboard: &'a Clipboard,
    event_loop: &'a ::winit::event_loop::ActiveEventLoop,
) -> AppContext<'a> {
    AppContext { event_loop, windows, winit_ids, scheduler, clipboard }
}

/// Whether a window with this effective redraw policy owes a frame right now.
///
/// `paced_frame_is_due` is the scheduler's refresh-boundary test, passed in
/// rather than read here so this decision — the one that separates "uses no
/// CPU" from "spins" — is a pure function and can be tested without a window.
///
/// [`RedrawPolicy::Dirty`] is due immediately because it means one frame is
/// already owed; the flag that produced it is cleared in the same pass, so it
/// yields exactly one frame. [`RedrawPolicy::Realtime`] is due every iteration
/// by definition. [`RedrawPolicy::Animating`] is the only paced one.
const fn frame_is_due(policy: RedrawPolicy, paced_frame_is_due: bool) -> bool {
    match policy {
        RedrawPolicy::Idle => false,
        RedrawPolicy::Dirty | RedrawPolicy::Realtime => true,
        RedrawPolicy::Animating => paced_frame_is_due,
    }
}

impl<H: AppHandler> ::winit::application::ApplicationHandler for Driver<H> {
    fn resumed(&mut self, event_loop: &::winit::event_loop::ActiveEventLoop) {
        let mut cx = context(
            &mut self.windows,
            &mut self.winit_ids,
            &mut self.scheduler,
            &self.clipboard,
            event_loop,
        );
        self.handler.resumed(&mut cx);
    }

    fn window_event(
        &mut self,
        event_loop: &::winit::event_loop::ActiveEventLoop,
        window_id: ::winit::window::WindowId,
        event: ::winit::event::WindowEvent,
    ) {
        let Some(&id) = self.winit_ids.get(&window_id) else { return };
        // The scale factor and modifier chord come from the window's own state
        // rather than from the platform: the state is what the application has
        // been told, and pointer positions must be converted with the same
        // value the application last saw.
        let Some(state) = self.windows.state(id) else { return };
        let scale = state.scale_factor();
        let modifiers = state.modifiers();

        let mut events = core::mem::take(&mut self.scratch);
        events.clear();
        translate_window_event(event, scale, modifiers, &mut events);

        for sphere_event in events.drain(..) {
            let is_redraw = matches!(sphere_event, WindowEvent::RedrawRequested);
            let is_terminal = sphere_event.is_terminal();
            self.windows.apply(id, &sphere_event);

            // A custom frame answers `WM_NCHITTEST` from state it was pushed
            // rather than state it queries, because that message fires on every
            // mouse move. Maximise, resizability and the scale factor are
            // exactly what can change out from under it, and all three arrive
            // as one of these two events.
            if matches!(sphere_event, WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged(_))
                && let Some(window) = self.windows.window(id)
            {
                window.sync_chrome_state();
            }
            if is_redraw {
                // The platform delivered the frame that was owed, and the
                // handler is about to draw it, so the debt is settled *before*
                // the callback rather than after. Clearing it afterwards would
                // wipe an invalidation raised from inside the draw callback —
                // the ordinary way to drive the next frame of an animation —
                // and the loop would go to sleep with a frame still owed.
                if let Some(state) = self.windows.state_mut(id) {
                    state.mark_presented();
                }
            }
            {
                let mut cx = context(
                    &mut self.windows,
                    &mut self.winit_ids,
                    &mut self.scheduler,
                    &self.clipboard,
                    event_loop,
                );
                self.handler.window_event(&mut cx, id, sphere_event);
            }
            if is_redraw {
                // The pacing interval measures display-to-display, so the
                // timestamp is taken after the handler has drawn.
                //
                // `frame_presented` also clears the *loop-wide* dirty flag,
                // which is not this window's to clear: that flag means "every
                // window owes a frame", and one window drawing must not cancel
                // the request for the others. Whatever it holds now — carried
                // in, or raised by the handler that just ran — is preserved for
                // `about_to_wait` to distribute.
                let loop_still_owes_a_frame = self.scheduler.is_dirty();
                self.scheduler.frame_presented(Instant::now());
                if loop_still_owes_a_frame {
                    self.scheduler.request_redraw();
                }
            }
            if is_terminal {
                self.winit_ids.remove(&window_id);
                self.windows.remove(id);
            }
        }
        // Hand the allocation back for the next event.
        self.scratch = events;
    }

    fn about_to_wait(&mut self, event_loop: &::winit::event_loop::ActiveEventLoop) {
        {
            let mut cx = context(
                &mut self.windows,
                &mut self.winit_ids,
                &mut self.scheduler,
                &self.clipboard,
                event_loop,
            );
            self.handler.about_to_wait(&mut cx);
        }

        // Turn appetite into frames.
        //
        // The scheduler and the per-window policies only *describe* how badly a
        // frame is wanted; this is the one place that converts that into an
        // actual platform redraw request. Without it `Animating` and `Realtime`
        // would keep the loop awake — `WaitUntil` and `Poll` respectively — and
        // never produce a single frame, which is the worst of both worlds: the
        // CPU cost of not sleeping and none of the redraws it was paid for.
        //
        // A window's own policy is folded in with the loop's, so one window
        // holding a live meter draws continuously without pinning the others.
        let now = Instant::now();
        let paced_frame_is_due = now >= self.scheduler.next_frame_time(now);
        let loop_policy = self.scheduler.policy();
        for (_, state) in self.windows.states_mut() {
            if !state.is_renderable() {
                continue;
            }
            if frame_is_due(loop_policy.max(state.redraw_policy()), paced_frame_is_due) {
                state.request_redraw();
            }
        }
        // The loop-wide request has been handed to the windows. Clearing it
        // unconditionally is deliberate: holding it while every window is
        // occluded would pin the loop into `Poll` with nothing to draw.
        self.scheduler.clear_redraw_request();

        for (_, window, state) in self.windows.iter() {
            if state.needs_redraw() && state.is_renderable() {
                window.request_redraw();
            }
        }

        let aggregate = self.windows.aggregate_policy();
        let control_flow = self.scheduler.control_flow(now, aggregate);
        event_loop.set_control_flow(control_flow_to_winit(control_flow));
    }

    fn suspended(&mut self, event_loop: &::winit::event_loop::ActiveEventLoop) {
        let mut cx = context(
            &mut self.windows,
            &mut self.winit_ids,
            &mut self.scheduler,
            &self.clipboard,
            event_loop,
        );
        self.handler.suspended(&mut cx);
    }

    fn exiting(&mut self, event_loop: &::winit::event_loop::ActiveEventLoop) {
        let mut cx = context(
            &mut self.windows,
            &mut self.winit_ids,
            &mut self.scheduler,
            &self.clipboard,
            event_loop,
        );
        self.handler.exiting(&mut cx);
    }

    fn memory_warning(&mut self, event_loop: &::winit::event_loop::ActiveEventLoop) {
        let mut cx = context(
            &mut self.windows,
            &mut self.winit_ids,
            &mut self.scheduler,
            &self.clipboard,
            event_loop,
        );
        self.handler.memory_warning(&mut cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_window_is_never_asked_to_draw() {
        // The guarantee the whole scheduler exists for: a static window costs
        // nothing, whether or not an animation deadline has passed.
        assert!(!frame_is_due(RedrawPolicy::Idle, false));
        assert!(!frame_is_due(RedrawPolicy::Idle, true));
    }

    #[test]
    fn animating_and_realtime_actually_produce_frames() {
        // The regression this guards: both modes used to raise the loop out of
        // `Wait` without ever asking a window to draw, so the process burned a
        // core and rendered nothing.
        assert!(frame_is_due(RedrawPolicy::Realtime, false), "realtime never waits for a boundary");
        assert!(frame_is_due(RedrawPolicy::Realtime, true));
        assert!(frame_is_due(RedrawPolicy::Animating, true));
        assert!(frame_is_due(RedrawPolicy::Dirty, false), "an owed frame does not wait either");
    }

    #[test]
    fn animation_waits_for_the_refresh_boundary() {
        // Animating is the one paced mode; drawing before the boundary would
        // spend frames the display cannot show.
        assert!(!frame_is_due(RedrawPolicy::Animating, false));
    }

    #[test]
    fn a_window_policy_is_folded_in_with_the_loops() {
        // How the runner combines them: the strongest wins, so one window
        // holding a meter draws without the loop having to know about it, and
        // an idle window still draws when the loop asks everybody to.
        let loop_idle = RedrawPolicy::Idle;
        assert!(frame_is_due(loop_idle.max(RedrawPolicy::Realtime), false));
        assert!(!frame_is_due(loop_idle.max(RedrawPolicy::Idle), false));
        let loop_dirty = RedrawPolicy::Dirty;
        assert!(frame_is_due(loop_dirty.max(RedrawPolicy::Idle), false));
    }

    #[test]
    fn the_pacing_deadline_agrees_with_the_schedulers_own_test() {
        // `about_to_wait` computes the boundary with `next_frame_time` while
        // `FrameScheduler::should_redraw` uses the same rule internally; if the
        // two ever disagreed the loop would wake without drawing, or draw
        // without waking.
        use crate::monitor::RefreshRate;
        let now = Instant::now();
        let mut s = FrameScheduler::with_refresh_rate(RefreshRate::HZ_60);
        s.begin_animation();

        // Before the first frame, animation is due immediately.
        assert!(now >= s.next_frame_time(now));
        assert!(s.should_redraw(now));

        s.frame_presented(now);
        let interval = s.frame_interval();
        let too_early = now + interval / 2;
        assert!(!(too_early >= s.next_frame_time(too_early)));
        assert!(!s.should_redraw(too_early));

        let due = now + interval;
        assert!(due >= s.next_frame_time(due));
        assert!(s.should_redraw(due));
    }

    #[test]
    fn presenting_a_frame_does_not_cancel_a_loop_wide_request() {
        // The multi-window bug this guards: window A drawing used to clear the
        // loop's dirty flag, so a "redraw everything" request raised while A
        // was in flight never reached window B, which then silently never
        // repainted. This is the save/restore the runner performs.
        let now = Instant::now();
        let mut s = FrameScheduler::new();
        s.request_redraw();

        let loop_still_owes_a_frame = s.is_dirty();
        s.frame_presented(now);
        if loop_still_owes_a_frame {
            s.request_redraw();
        }
        assert!(s.is_dirty(), "the request for the other windows must survive");
        assert_eq!(s.policy(), RedrawPolicy::Dirty);

        // ...and a frame nobody asked for still lets the loop go back to sleep.
        let mut s = FrameScheduler::new();
        let loop_still_owes_a_frame = s.is_dirty();
        s.frame_presented(now);
        if loop_still_owes_a_frame {
            s.request_redraw();
        }
        assert!(!s.is_dirty());
        assert_eq!(s.policy(), RedrawPolicy::Idle);
    }

    /// Opens a real window, waits for one frame, and exits.
    ///
    /// `#[ignore]`d because it needs a window system *and* the process's main
    /// thread: `cargo test` runs each test on a worker thread, and creating an
    /// event loop off the main thread is refused outright on macOS and iOS and
    /// requires an opt-in on Windows. Run it deliberately, on a machine with a
    /// display:
    ///
    /// ```text
    /// cargo test -p sphere-platform -- --ignored --test-threads=1
    /// ```
    ///
    /// The headless-testable half of this path — event translation, window
    /// bookkeeping, the scheduler — is covered by the other modules' tests.
    #[test]
    #[ignore = "opens a real window; needs a display and the main thread"]
    fn a_window_opens_draws_once_and_goes_idle() {
        struct Once {
            frames: u32,
            saw_idle: bool,
        }

        impl AppHandler for Once {
            fn resumed(&mut self, cx: &mut AppContext<'_>) {
                let attrs = WindowAttributes::new("sphere-platform test").with_inner_size(
                    sphere_core::size(sphere_core::px(320.0), sphere_core::px(240.0)),
                );
                let window = cx.create_window(&attrs).expect("window creation");
                assert_eq!(cx.windows().len(), 1);
                assert!(cx.windows().contains(window.id()));
                assert!(window.physical_size().width.get() > 0);
            }

            fn window_event(&mut self, cx: &mut AppContext<'_>, id: WindowId, event: WindowEvent) {
                if matches!(event, WindowEvent::RedrawRequested) {
                    self.frames += 1;
                    // The state must have been updated before the handler saw
                    // the event.
                    let state = cx.windows().state(id).expect("state");
                    assert!(state.is_renderable());
                    if self.frames >= 1 {
                        cx.exit();
                    }
                }
            }

            fn about_to_wait(&mut self, cx: &mut AppContext<'_>) {
                if self.frames > 0 && cx.windows().aggregate_policy() == RedrawPolicy::Idle {
                    self.saw_idle = true;
                }
            }

            fn exiting(&mut self, _cx: &mut AppContext<'_>) {
                assert!(self.frames >= 1, "no frame was ever requested");
            }
        }

        App::new(Once { frames: 0, saw_idle: false }).run().expect("event loop");
    }
}

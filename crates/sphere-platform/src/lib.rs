//! # sphere-platform
//!
//! Windows, input and display integration for the Sphere graphics engine.
//!
//! This crate is the boundary between the operating system and everything
//! above it. Two rules shape all of it:
//!
//! 1. **No backend type appears in a public signature.** `winit` lives behind
//!    [`backend`] and is translated at the edge into Sphere's own
//!    [`WindowEvent`], [`Key`], [`Cursor`] and [`MonitorInfo`]. Replacing the
//!    backend is a translation-table job, not a rewrite of the widget layer.
//!    The one deliberate exception is `raw-window-handle`, re-exported below:
//!    that *is* the seam a GPU renderer needs, and it is not a windowing
//!    library.
//! 2. **The engine may not own the process.** A plug-in editor runs inside a
//!    host that owns the main thread, the event loop and the parent window.
//!    Everything here works in that case too — see [`WindowTarget`] and the
//!    lifecycle rules in [`window`].
//!
//! ## Units
//!
//! Pointer positions arrive in logical pixels ([`sphere_core::Px`]); window and
//! surface sizes stay in device pixels ([`sphere_core::DevicePx`]). The
//! conversion happens once, at the backend boundary, using the window's own
//! scale factor. See [`event`] for why.
//!
//! ## Idling
//!
//! A Sphere event loop *blocks* by default. [`FrameScheduler`] and
//! [`RedrawPolicy`] decide when it may not: a static window uses no CPU, an
//! animating one is paced against its display's refresh rate, and a realtime
//! meter can ask for continuous frames. An occluded or zero-sized window
//! contributes nothing, so minimising a plug-in editor really does stop the
//! work.
//!
//! ## Layout
//!
//! | Module | Contents |
//! |---|---|
//! | [`event`] | [`WindowEvent`] and its payload types |
//! | [`keyboard`] | [`Key`], [`PhysicalKey`], [`Modifiers`] |
//! | [`cursor`] | [`Cursor`] shapes |
//! | [`monitor`] | display geometry and [`RefreshRate`] |
//! | [`window`] | [`WindowAttributes`], [`WindowTarget`], foreign windows |
//! | [`registry`] | [`WindowState`] and the multi-window [`WindowRegistry`] |
//! | [`scheduler`] | [`FrameScheduler`], [`RedrawPolicy`], [`ControlFlow`] |
//! | [`clipboard`] | [`Clipboard`] and its provider seam |
//! | [`app`] | the standalone runner: [`App`], [`AppHandler`] |
//! | [`backend`] | the platform backend and its translation tables |

#![deny(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod backend;
pub mod clipboard;
pub mod cursor;
pub mod event;
pub mod keyboard;
pub mod monitor;
pub mod registry;
pub mod scheduler;
pub mod window;

#[cfg(feature = "winit-backend")]
pub mod app;

pub use clipboard::{Clipboard, ClipboardProvider};
pub use cursor::Cursor;
pub use event::{ElementState, ImeEvent, MouseButton, ScrollDelta, Theme, TouchPhase, WindowEvent};
pub use keyboard::{
    Key, KeyCode, KeyLocation, KeyText, Modifiers, NamedKey, PhysicalKey, Scancode,
};
pub use monitor::{MonitorInfo, MonitorList, RefreshRate, VideoMode};
pub use registry::{WindowRegistry, WindowState};
pub use scheduler::{ControlFlow, FrameScheduler, RedrawPolicy};
pub use window::{
    CaptionRegions, ForeignWindow, ForeignWindowHandle, ResizeEdge, WindowAttributes, WindowChrome,
    WindowLevel, WindowPosition, WindowTarget,
};

#[cfg(feature = "winit-backend")]
pub use app::{App, AppContext, AppHandler, WindowManager, run};
#[cfg(feature = "winit-backend")]
pub use backend::Window;

/// Re-exported so that a renderer builds surfaces against exactly the version
/// of `raw-window-handle` this crate's window types implement. A version
/// mismatch here produces a trait-not-implemented error whose cause is
/// invisible; re-exporting makes it impossible.
pub use raw_window_handle;

/// The identity of a window, re-exported from `sphere-core` so that consumers
/// do not have to depend on it directly just to name one.
pub use sphere_core::WindowId;

/// Everything a typical consumer needs, in one import.
pub mod prelude {
    pub use crate::cursor::Cursor;
    pub use crate::event::{ElementState, MouseButton, ScrollDelta, Theme, WindowEvent};
    pub use crate::keyboard::{Key, Modifiers, NamedKey, PhysicalKey};
    pub use crate::monitor::{MonitorInfo, RefreshRate};
    pub use crate::registry::WindowState;
    pub use crate::scheduler::{FrameScheduler, RedrawPolicy};
    pub use crate::window::{CaptionRegions, WindowAttributes, WindowChrome, WindowTarget};
    pub use sphere_core::WindowId;

    #[cfg(feature = "winit-backend")]
    pub use crate::app::{App, AppContext, AppHandler};
    #[cfg(feature = "winit-backend")]
    pub use crate::backend::Window;
}

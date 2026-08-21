//! Platform backends.
//!
//! Everything above this module speaks only Sphere's own types. A backend's
//! job is to translate — events, keys, cursors, monitors — and to own the
//! platform window object. That boundary is enforced rather than aspirational:
//! no backend type appears in any public signature outside this module, so
//! replacing the backend is a matter of writing a new translation table, not
//! of touching the widget layer.
//!
//! The only backend today is [`winit`], behind the `winit-backend` feature
//! (on by default). A native per-platform backend would live beside it and
//! provide the same `Window` surface.

// Custom-frame decisions. Compiled on every Windows build, backend or not: the
// functions are pure and their tests are the only thing standing between a
// transcription error in an `HT*` code and a window whose edges are subtly
// wrong.
#[cfg(windows)]
pub(crate) mod nc;

// Every unsafe call the custom frame makes. Needs a backend to have produced a
// window to subclass, so unlike `nc` it is gated on one.
#[cfg(all(windows, feature = "winit-backend"))]
pub(crate) mod ffi;

#[cfg(feature = "winit-backend")]
pub mod winit;

// `self::` is load-bearing: a bare `winit::` in a `use` path resolves to the
// external crate, not to the module declared just above.
#[cfg(feature = "winit-backend")]
pub use self::winit::Window;

//! System clipboard access.
//!
//! winit has no clipboard API — it deliberately stops at windowing — so Sphere
//! provides one here, behind a trait.
//!
//! The trait is not decoration. A plug-in must often route the clipboard
//! through its *host*: some DAWs run their editor views in a context where
//! touching the OS clipboard directly steals ownership of the selection from
//! the host's own text fields, and on Linux the X11 selection protocol makes
//! the owning process responsible for serving the data for as long as it is
//! held. Letting the embedder supply the implementation is the only way to make
//! both the standalone and the embedded case correct.
//!
//! ## What is implemented
//!
//! * With the `clipboard` feature (on by default), [`Clipboard::system`]
//!   returns a working OS-backed clipboard built on `arboard`. `arboard` is
//!   pulled in with `default-features = false`, which drops its image-data
//!   support and with it the whole `image` crate: Sphere only needs text here.
//!   Its `wayland-data-control` feature is also off, matching `arboard`'s own
//!   default, so on Wayland the X11 path via `XWayland` is used. Turn it on
//!   downstream if a native Wayland clipboard is required.
//! * Without it, or when the platform has no clipboard,
//!   [`Clipboard::unsupported`] returns one whose operations fail with
//!   [`PlatformError::Unsupported`] rather than silently doing nothing. Failing
//!   loudly is the point: a paste that silently does nothing is a bug report,
//!   a paste that returns an error is a disabled menu item.
//! * An embedder supplies its own with [`Clipboard::with_provider`].

use core::fmt;

use sphere_core::PlatformError;

/// A source of clipboard text.
///
/// `Send + Sync` because the clipboard is reached from the UI thread of
/// whichever window asks, and in a plug-in there is no guarantee that is the
/// same thread every time.
pub trait ClipboardProvider: Send + Sync {
    /// Reads the clipboard's text content.
    fn get_text(&self) -> Result<String, PlatformError>;

    /// Replaces the clipboard's text content.
    fn set_text(&self, text: &str) -> Result<(), PlatformError>;
}

/// The clipboard an application uses.
///
/// Cheap to hold: creating the underlying platform connection happens once, in
/// [`Clipboard::system`].
pub struct Clipboard {
    /// `None` means "this build or platform has no clipboard".
    provider: Option<Box<dyn ClipboardProvider>>,
}

impl Clipboard {
    /// A clipboard whose operations all fail with
    /// [`PlatformError::Unsupported`].
    pub const fn unsupported() -> Self {
        Self { provider: None }
    }

    /// A clipboard backed by a caller-supplied implementation, such as one that
    /// forwards to a plug-in host.
    pub fn with_provider(provider: Box<dyn ClipboardProvider>) -> Self {
        Self { provider: Some(provider) }
    }

    /// The operating system clipboard.
    ///
    /// Returns an [`Clipboard::unsupported`] clipboard rather than an error
    /// when the platform connection cannot be opened: a machine with no
    /// display server is a normal thing to run a headless render on, and it
    /// should not stop the engine from starting.
    #[cfg(feature = "clipboard")]
    pub fn system() -> Self {
        match arboard_impl::ArboardProvider::new() {
            Ok(p) => Self::with_provider(Box::new(p)),
            Err(e) => {
                tracing::warn!("system clipboard unavailable: {e}");
                Self::unsupported()
            }
        }
    }

    /// The operating system clipboard, which this build does not have.
    ///
    /// Compiled when the `clipboard` feature is off; the signature is identical
    /// so that turning the feature on and off is not a source change.
    #[cfg(not(feature = "clipboard"))]
    pub fn system() -> Self {
        Self::unsupported()
    }

    /// True when reads and writes can succeed.
    ///
    /// Use it to grey out Cut/Copy/Paste rather than letting them fail.
    #[inline]
    pub fn is_supported(&self) -> bool {
        self.provider.is_some()
    }

    /// Reads the clipboard's text content.
    pub fn get_text(&self) -> Result<String, PlatformError> {
        match &self.provider {
            Some(p) => p.get_text(),
            None => Err(PlatformError::Unsupported("clipboard")),
        }
    }

    /// Replaces the clipboard's text content.
    pub fn set_text(&self, text: &str) -> Result<(), PlatformError> {
        match &self.provider {
            Some(p) => p.set_text(text),
            None => Err(PlatformError::Unsupported("clipboard")),
        }
    }
}

impl Default for Clipboard {
    /// The system clipboard, or an unsupported one if it cannot be opened.
    fn default() -> Self {
        Self::system()
    }
}

impl fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clipboard").field("supported", &self.is_supported()).finish()
    }
}

#[cfg(feature = "clipboard")]
mod arboard_impl {
    use super::ClipboardProvider;
    use sphere_core::PlatformError;
    use std::sync::Mutex;

    /// The OS clipboard, reached through `arboard`.
    ///
    /// `arboard` needs `&mut self`, and the platform connection is expensive
    /// enough that reopening it per call is wrong, so it lives behind a mutex.
    /// Contention is not a concern: clipboard operations are user-initiated.
    pub(super) struct ArboardProvider {
        inner: Mutex<arboard::Clipboard>,
    }

    impl ArboardProvider {
        pub(super) fn new() -> Result<Self, arboard::Error> {
            Ok(Self { inner: Mutex::new(arboard::Clipboard::new()?) })
        }
    }

    impl ClipboardProvider for ArboardProvider {
        fn get_text(&self) -> Result<String, PlatformError> {
            let mut guard =
                self.inner.lock().map_err(|_| PlatformError::Clipboard("poisoned".into()))?;
            guard.get_text().map_err(|e| PlatformError::Clipboard(e.to_string()))
        }

        fn set_text(&self, text: &str) -> Result<(), PlatformError> {
            let mut guard =
                self.inner.lock().map_err(|_| PlatformError::Clipboard("poisoned".into()))?;
            guard.set_text(text.to_owned()).map_err(|e| PlatformError::Clipboard(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An in-process clipboard, used only to prove the provider seam works.
    /// Never shipped: a fake clipboard that silently does not interoperate
    /// with the rest of the desktop would be worse than no clipboard.
    struct MemoryClipboard(Mutex<String>);

    impl ClipboardProvider for MemoryClipboard {
        fn get_text(&self) -> Result<String, PlatformError> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn set_text(&self, text: &str) -> Result<(), PlatformError> {
            *self.0.lock().unwrap() = text.to_owned();
            Ok(())
        }
    }

    #[test]
    fn an_unsupported_clipboard_fails_loudly_on_both_operations() {
        let c = Clipboard::unsupported();
        assert!(!c.is_supported());
        let err = c.get_text().unwrap_err();
        assert!(matches!(err, PlatformError::Unsupported("clipboard")), "{err}");
        assert!(err.to_string().contains("clipboard"));
        assert!(c.set_text("x").is_err());
    }

    #[test]
    fn a_supplied_provider_round_trips() {
        let c = Clipboard::with_provider(Box::new(MemoryClipboard(Mutex::new(String::new()))));
        assert!(c.is_supported());
        assert_eq!(c.get_text().unwrap(), "");
        c.set_text("gain: -6.0 dB").unwrap();
        assert_eq!(c.get_text().unwrap(), "gain: -6.0 dB");
        // Unicode survives: plug-in parameter names are full of it.
        c.set_text("\u{2192} \u{221E}").unwrap();
        assert_eq!(c.get_text().unwrap(), "\u{2192} \u{221E}");
    }

    #[test]
    fn debug_output_says_whether_the_clipboard_works() {
        assert!(format!("{:?}", Clipboard::unsupported()).contains("supported: false"));
        let c = Clipboard::with_provider(Box::new(MemoryClipboard(Mutex::new(String::new()))));
        assert!(format!("{c:?}").contains("supported: true"));
    }
}

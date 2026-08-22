//! Rust backend for the `spherekit-app-react` template.

use spherekit_react::{AnyElement, NativeTree, ReactHost, ReactHostError};

/// State owned by the native application side.
#[derive(Default)]
pub struct AppBackend {
    react: ReactHost,
}

impl AppBackend {
    /// Creates a backend with an empty React host tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts the JSON commit sent by `src/renderer`.
    pub fn commit_react_snapshot(&mut self, json: &str) -> Result<(), ReactHostError> {
        self.react.commit_json(json)
    }

    /// Returns the current committed tree for diagnostics or native layout.
    pub fn react_tree(&self) -> &NativeTree {
        self.react.tree()
    }

    /// Builds the committed React tree as SphereKit native elements.
    pub fn native_root(&self) -> AnyElement {
        self.react.ui_element()
    }
}

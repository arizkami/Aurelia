//! Rust backend for the `spherekit-app-react` template.

use spherekit_bridge::{decode, encode, ApiBridge, BridgeProtocolError};
use spherekit_react::{AnyElement, NativeTree, ReactHostError};

/// State owned by the native application side.
#[derive(Default)]
pub struct AppBackend {
    bridge: ApiBridge,
}

impl AppBackend {
    /// Creates a backend with an empty React host tree.
    pub fn new() -> Self {
        Self { bridge: ApiBridge::new() }
    }

    /// Accepts the JSON commit sent by `src/renderer`.
    pub fn commit_react_snapshot(&mut self, json: &str) -> Result<(), ReactHostError> {
        self.bridge.commit_json(json)
    }

    /// Returns the current committed tree for diagnostics or native layout.
    pub fn react_tree(&self) -> &NativeTree {
        self.bridge.host().tree()
    }

    /// Builds the committed React tree as SphereKit native elements.
    pub fn native_root(&self) -> AnyElement {
        self.bridge.host().ui_element()
    }

    /// Handles one newline-delimited API Bridge frame from the renderer.
    pub fn handle_bridge_frame(&mut self, frame: &str) -> Result<Vec<String>, BridgeProtocolError> {
        let messages = self.bridge.dispatch(decode(frame)?);
        messages
            .iter()
            .map(|message| encode(message).map_err(|error| BridgeProtocolError::InvalidMessage(error.to_string())))
            .collect()
    }
}

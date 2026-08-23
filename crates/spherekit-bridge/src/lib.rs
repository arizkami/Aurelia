//! A small, platform-neutral API bridge for SphereKit applications.
//!
//! The wire format is newline-delimited JSON so it can travel over a WebView
//! callback, a native FFI shim, a child process, or a socket without making
//! the renderer depend on any of those transports. The bridge owns the React
//! host and turns incoming commits into validated native state.
//!
//! ```
//! use spherekit_bridge::{ApiBridge, BridgeMessage, PROTOCOL_VERSION};
//!
//! let mut bridge = ApiBridge::new();
//! let ready = bridge.dispatch(BridgeMessage::Hello {
//!     version: PROTOCOL_VERSION,
//!     runtime: "react".into(),
//!     platform: "windows".into(),
//! });
//! assert!(matches!(ready.as_slice(), [BridgeMessage::Ready { .. }]));
//! ```
//!
//! ## Two directions, two shapes
//!
//! JavaScript -> native is request/response: a `commit` or an `invoke` gets its
//! answer from [`ApiBridge::dispatch`] on the same call. Native -> JavaScript
//! cannot work that way, because the native side does not know when the
//! renderer is willing to be interrupted, so events are queued with
//! [`ApiBridge::emit`] and collected by [`ApiBridge::take_outbound`] whenever
//! the host is ready to deliver them. Widget callbacks deposit theirs in a
//! shared [`spherekit_react::EventQueue`], which [`ApiBridge::pump_events`]
//! turns into the same outbound messages in one call per frame.
//!
//! ## Running JavaScript in process
//!
//! With the `v8` feature the crate also ships [`JsBridge`], which owns a V8
//! isolate and connects it to an [`ApiBridge`] through two host globals. That
//! is a strictly optional extra: the protocol above is the product, and an
//! embedder using a WebView or a child process never compiles V8 at all.

#![deny(missing_docs)]

mod endpoint;
mod protocol;

#[cfg(feature = "v8")]
mod js;

pub use endpoint::ApiBridge;
pub use protocol::{
    BridgeError, BridgeMessage, BridgeProtocolError, CAPABILITIES, JsonLines, PROTOCOL_VERSION,
    capabilities, decode, encode,
};

#[cfg(feature = "v8")]
pub use js::{JsBridge, JsBridgeError, v8_version};

use serde_json::Value;
use std::collections::BTreeMap;

/// Serializes a map of API parameters deterministically for diagnostics.
pub fn sorted_object(entries: impl IntoIterator<Item = (String, Value)>) -> Value {
    Value::Object(entries.into_iter().collect::<BTreeMap<_, _>>().into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_object_is_ordered_regardless_of_insertion() {
        let sorted = sorted_object([
            ("zeta".to_owned(), Value::from(1)),
            ("alpha".to_owned(), Value::from(2)),
        ]);
        assert_eq!(sorted.to_string(), r#"{"alpha":2,"zeta":1}"#);
    }
}

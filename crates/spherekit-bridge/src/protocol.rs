//! The wire format: one JSON message per line, in both directions.
//!
//! JSON Lines is the only framing every transport this bridge has to survive
//! already agrees on. A WebView callback, an FFI shim, a child process pipe, a
//! socket and a V8 host function can all carry a line of text; none of them
//! agree on anything richer, and picking a length-prefixed binary frame would
//! have meant writing the framing three times over. The cost is that a message
//! may not contain a raw newline, which `serde_json`'s compact output never
//! emits, so nothing has to escape anything.
//!
//! Frames are versioned by [`PROTOCOL_VERSION`] rather than negotiated
//! per-field. A renderer that speaks a different version is refused at the
//! handshake, because a half-understood commit is worse than no commit: it
//! paints something the application never described.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spherekit_react::NativeTree;
use thiserror::Error;

/// Version of the JSON Lines protocol implemented by this crate.
pub const PROTOCOL_VERSION: u16 = 1;

/// Every method and event family this endpoint answers.
///
/// The `ready` handshake and the `spherekit.capabilities` method are the same
/// list, and used to be written out twice — which is exactly how they came to
/// disagree, leaving a client that asked the second way convinced a method the
/// handshake advertised did not exist. One const, two readers, no drift.
pub const CAPABILITIES: &[&str] = &[
    "react.commit",
    "react.events",
    "api.invoke",
    "spherekit.capabilities",
    "spherekit.getNode",
    "spherekit.getNodeCount",
    "spherekit.getTree",
    "spherekit.ping",
    "spherekit.setStyleContext",
    "spherekit.setStylesheet",
    "spherekit.version",
];

/// Returns [`CAPABILITIES`] in the owned form the `ready` message carries.
pub fn capabilities() -> Vec<String> {
    CAPABILITIES.iter().map(|name| (*name).to_owned()).collect()
}

/// A message exchanged by the JavaScript and native sides.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BridgeMessage {
    /// Starts a connection and advertises the JavaScript runtime.
    Hello {
        /// Protocol version requested by the client.
        version: u16,
        /// Runtime name, normally `react`.
        runtime: String,
        /// Client platform identifier.
        platform: String,
    },
    /// Confirms the connection and lists native capabilities.
    Ready {
        /// Protocol version accepted by the native side.
        version: u16,
        /// Methods and event families supported by the endpoint.
        capabilities: Vec<String>,
    },
    /// Replaces the retained React tree after a committed render.
    Commit {
        /// Complete tree snapshot, never a partial mutation sequence.
        snapshot: NativeTree,
    },
    /// Calls a native API method.
    Invoke {
        /// Client-generated request identity.
        id: String,
        /// Fully-qualified method name.
        method: String,
        /// JSON-compatible method parameters.
        params: Value,
    },
    /// Returns the result of an [`BridgeMessage::Invoke`].
    Response {
        /// Request identity being completed.
        id: String,
        /// Whether the request succeeded.
        ok: bool,
        /// Method result when `ok` is true.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        /// Structured error when `ok` is false.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<BridgeError>,
    },
    /// Sends a native input or lifecycle event to JavaScript.
    Event {
        /// Event name, such as `press` or `valueChange`.
        event: String,
        /// React host node receiving the event, when it is node-scoped.
        #[serde(rename = "nodeId", skip_serializing_if = "Option::is_none")]
        node_id: Option<u64>,
        /// Event payload, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
    /// Requests a clean connection shutdown.
    Shutdown,
}

/// A structured API failure that can safely cross the bridge.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BridgeError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Optional structured details for the caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl BridgeError {
    /// Creates an error with a stable code and message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), data: None }
    }
}

/// Errors raised while encoding or decoding bridge frames.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BridgeProtocolError {
    /// The frame was not valid JSON or did not match the protocol schema.
    #[error("invalid SphereKit bridge message: {0}")]
    InvalidMessage(String),
    /// A line exceeded the configured safety limit.
    #[error("SphereKit bridge message exceeds {0} bytes")]
    FrameTooLarge(usize),
}

/// Encodes one message as one newline-delimited JSON frame.
pub fn encode(message: &BridgeMessage) -> Result<String, serde_json::Error> {
    serde_json::to_string(message).map(|json| format!("{json}\n"))
}

/// Decodes one JSON frame without its trailing newline.
pub fn decode(frame: &str) -> Result<BridgeMessage, BridgeProtocolError> {
    serde_json::from_str(frame)
        .map_err(|error| BridgeProtocolError::InvalidMessage(error.to_string()))
}

/// Incremental decoder for transports that deliver partial chunks.
///
/// The size limit is not politeness: a transport that never sends a newline
/// would otherwise grow this buffer until the process dies, and a renderer
/// crash mid-frame is a normal way for that to happen.
#[derive(Debug)]
pub struct JsonLines {
    buffer: String,
    max_frame_bytes: usize,
}

impl Default for JsonLines {
    fn default() -> Self {
        Self::new(4 * 1024 * 1024)
    }
}

impl JsonLines {
    /// Creates a decoder with a maximum encoded frame size.
    pub fn new(max_frame_bytes: usize) -> Self {
        Self { buffer: String::new(), max_frame_bytes: max_frame_bytes.max(1) }
    }

    /// Feeds a UTF-8 chunk and returns every complete frame found in it.
    pub fn push(&mut self, chunk: &str) -> Vec<Result<BridgeMessage, BridgeProtocolError>> {
        self.buffer.push_str(chunk);
        let mut messages = Vec::new();

        while let Some(newline) = self.buffer.find('\n') {
            let mut line = self.buffer.drain(..=newline).collect::<String>();
            if line.ends_with('\n') {
                line.pop();
            }
            if line.ends_with('\r') {
                line.pop();
            }
            if line.len() > self.max_frame_bytes {
                messages.push(Err(BridgeProtocolError::FrameTooLarge(self.max_frame_bytes)));
            } else if !line.trim().is_empty() {
                messages.push(decode(&line));
            }
        }

        if self.buffer.len() > self.max_frame_bytes {
            self.buffer.clear();
            messages.push(Err(BridgeProtocolError::FrameTooLarge(self.max_frame_bytes)));
        }

        messages
    }

    /// Discards a partial frame, for example when a transport disconnects.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_messages() -> Vec<BridgeMessage> {
        vec![
            BridgeMessage::Hello {
                version: PROTOCOL_VERSION,
                runtime: "react".into(),
                platform: "windows".into(),
            },
            BridgeMessage::Invoke {
                id: "request-1".into(),
                method: "spherekit.ping".into(),
                params: json!({ "value": 7 }),
            },
            BridgeMessage::Event {
                event: "press".into(),
                node_id: Some(3),
                payload: Some(json!({ "x": 1 })),
            },
            BridgeMessage::Shutdown,
        ]
    }

    #[test]
    fn encodes_and_decodes_a_message() {
        let message = BridgeMessage::Invoke {
            id: "request-1".into(),
            method: "spherekit.capabilities".into(),
            params: json!({}),
        };
        let encoded = encode(&message).expect("message encodes");
        assert!(encoded.ends_with('\n'));
        assert_eq!(decode(encoded.trim_end()), Ok(message));
    }

    #[test]
    fn incremental_decoder_handles_split_frames() {
        let encoded = encode(&BridgeMessage::Shutdown).expect("message encodes");
        let split = encoded.len() / 2;
        let mut decoder = JsonLines::default();
        assert!(decoder.push(&encoded[..split]).is_empty());
        assert_eq!(decoder.push(&encoded[split..]), vec![Ok(BridgeMessage::Shutdown)]);
    }

    #[test]
    fn every_split_point_of_a_stream_decodes_to_the_same_messages() {
        let expected = sample_messages();
        let stream = expected.iter().map(|m| encode(m).expect("encodes")).collect::<String>();
        assert!(stream.is_ascii(), "the split loop assumes byte offsets are char boundaries");

        for split in 0..=stream.len() {
            let mut decoder = JsonLines::default();
            let mut decoded = decoder.push(&stream[..split]);
            decoded.extend(decoder.push(&stream[split..]));
            let decoded = decoded.into_iter().collect::<Result<Vec<_>, _>>();
            assert_eq!(decoded.as_deref(), Ok(expected.as_slice()), "split at byte {split}");
        }
    }

    #[test]
    fn one_byte_at_a_time_is_still_one_message_per_line() {
        let expected = sample_messages();
        let stream = expected.iter().map(|m| encode(m).expect("encodes")).collect::<String>();
        let mut decoder = JsonLines::default();
        let mut decoded = Vec::new();
        for index in 0..stream.len() {
            decoded.extend(decoder.push(&stream[index..index + 1]));
        }
        assert_eq!(decoded.into_iter().collect::<Result<Vec<_>, _>>(), Ok(expected));
    }

    #[test]
    fn blank_lines_and_carriage_returns_are_not_messages() {
        let mut decoder = JsonLines::default();
        assert!(decoder.push("\n   \n").is_empty());
        let encoded = encode(&BridgeMessage::Shutdown).expect("message encodes");
        let windows_line = format!("{}\r\n", encoded.trim_end());
        assert_eq!(decoder.push(&windows_line), vec![Ok(BridgeMessage::Shutdown)]);
    }

    #[test]
    fn rejects_oversized_partial_frames() {
        let mut decoder = JsonLines::new(4);
        let errors = decoder.push("12345");
        assert_eq!(errors, vec![Err(BridgeProtocolError::FrameTooLarge(4))]);
    }
}

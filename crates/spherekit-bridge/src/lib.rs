//! A small, platform-neutral API bridge for SphereKit applications.
//!
//! The wire format is newline-delimited JSON so it can travel over a WebView
//! callback, a native FFI shim, a child process, or a socket without making
//! the renderer depend on any of those transports. The bridge owns the React
//! host and turns incoming commits into validated native state.

#![deny(missing_docs)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spherekit_react::{NativeTree, ReactHost, ReactHostError};
use std::collections::BTreeMap;
use thiserror::Error;

/// Version of the JSON Lines protocol implemented by this crate.
pub const PROTOCOL_VERSION: u16 = 1;

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

/// Native endpoint that validates React commits and handles the built-in API.
pub struct ApiBridge {
    host: ReactHost,
    methods: BTreeMap<String, ApiMethod>,
}

type ApiMethod = Box<dyn FnMut(&mut ReactHost, Value) -> Result<Value, BridgeError> + Send>;

impl Default for ApiBridge {
    fn default() -> Self {
        Self { host: ReactHost::new(), methods: BTreeMap::new() }
    }
}

impl ApiBridge {
    /// Creates an endpoint with an empty React host tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the retained React host.
    pub fn host(&self) -> &ReactHost {
        &self.host
    }

    /// Returns mutable access for an application-specific native renderer.
    pub fn host_mut(&mut self) -> &mut ReactHost {
        &mut self.host
    }

    /// Registers an application API method callable from React's `invoke`.
    ///
    /// Method names should be namespaced by the application, for example
    /// `audio.getMeter` or `project.save`. Built-in `spherekit.*` methods stay
    /// available unless an application intentionally replaces one.
    pub fn register_method(
        &mut self,
        method: impl Into<String>,
        handler: impl FnMut(&mut ReactHost, Value) -> Result<Value, BridgeError> + Send + 'static,
    ) {
        self.methods.insert(method.into(), Box::new(handler));
    }

    /// Dispatches one decoded message and returns zero or more responses/events.
    pub fn dispatch(&mut self, message: BridgeMessage) -> Vec<BridgeMessage> {
        match message {
            BridgeMessage::Hello { version, .. } => {
                if version != PROTOCOL_VERSION {
                    return vec![BridgeMessage::Response {
                        id: "handshake".into(),
                        ok: false,
                        result: None,
                        error: Some(BridgeError::new(
                            "unsupported_version",
                            format!(
                                "expected protocol version {PROTOCOL_VERSION}, received {version}"
                            ),
                        )),
                    }];
                }
                vec![BridgeMessage::Ready {
                    version: PROTOCOL_VERSION,
                    capabilities: vec![
                        "react.commit".into(),
                        "react.events".into(),
                        "api.invoke".into(),
                        "spherekit.getTree".into(),
                        "spherekit.getNode".into(),
                    ],
                }]
            }
            BridgeMessage::Commit { snapshot } => {
                if let Err(error) = self.host.commit(snapshot) {
                    vec![BridgeMessage::Event {
                        event: "bridgeError".into(),
                        node_id: None,
                        payload: Some(serde_json::json!({
                            "code": "invalid_commit",
                            "message": error.to_string(),
                        })),
                    }]
                } else {
                    Vec::new()
                }
            }
            BridgeMessage::Invoke { id, method, params } => {
                vec![match self.invoke(&method, params) {
                    Ok(result) => {
                        BridgeMessage::Response { id, ok: true, result: Some(result), error: None }
                    }
                    Err(error) => {
                        BridgeMessage::Response { id, ok: false, result: None, error: Some(error) }
                    }
                }]
            }
            BridgeMessage::Shutdown => vec![BridgeMessage::Shutdown],
            BridgeMessage::Ready { .. }
            | BridgeMessage::Response { .. }
            | BridgeMessage::Event { .. } => Vec::new(),
        }
    }

    /// Calls one of the built-in native methods.
    pub fn invoke(&mut self, method: &str, params: Value) -> Result<Value, BridgeError> {
        if let Some(handler) = self.methods.get_mut(method) {
            return handler(&mut self.host, params);
        }
        match method {
            "spherekit.getTree" => serde_json::to_value(self.host.tree())
                .map_err(|error| BridgeError::new("serialization_error", error.to_string())),
            "spherekit.getNode" => {
                let id = params
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| BridgeError::new("invalid_params", "expected { id: number }"))?;
                let node = self.host.node(id).ok_or_else(|| {
                    BridgeError::new("not_found", format!("React node {id} does not exist"))
                })?;
                serde_json::to_value(node)
                    .map_err(|error| BridgeError::new("serialization_error", error.to_string()))
            }
            "spherekit.capabilities" => Ok(serde_json::json!([
                "react.commit",
                "react.events",
                "api.invoke",
                "spherekit.getTree",
                "spherekit.getNode"
            ])),
            _ => Err(BridgeError::new("method_not_found", format!("unknown API method {method}"))),
        }
    }

    /// Commits JSON directly for FFI hosts that do not use the message codec.
    pub fn commit_json(&mut self, json: &str) -> Result<(), ReactHostError> {
        self.host.commit_json(json)
    }

    /// Returns a JSON frame for one response message.
    pub fn encode(message: &BridgeMessage) -> Result<String, serde_json::Error> {
        encode(message)
    }
}

/// Serializes a map of API parameters deterministically for diagnostics.
pub fn sorted_object(entries: impl IntoIterator<Item = (String, Value)>) -> Value {
    Value::Object(entries.into_iter().collect::<BTreeMap<_, _>>().into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn endpoint_commits_and_answers_tree_queries() {
        let mut bridge = ApiBridge::new();
        let hello = bridge.dispatch(BridgeMessage::Hello {
            version: PROTOCOL_VERSION,
            runtime: "react".into(),
            platform: "linux".into(),
        });
        assert!(matches!(hello.as_slice(), [BridgeMessage::Ready { .. }]));

        bridge.dispatch(BridgeMessage::Commit {
            snapshot: serde_json::from_value(json!({
                "revision": 1,
                "children": [{"id": 1, "type": "text", "children": []}]
            }))
            .expect("snapshot decodes"),
        });
        let response = bridge.dispatch(BridgeMessage::Invoke {
            id: "tree".into(),
            method: "spherekit.getTree".into(),
            params: json!({}),
        });
        assert!(matches!(response.as_slice(), [BridgeMessage::Response { ok: true, .. }]));
    }

    #[test]
    fn application_methods_can_extend_the_builtin_api() {
        let mut bridge = ApiBridge::new();
        bridge.register_method("app.echo", |_host, params| Ok(params));
        let response = bridge.dispatch(BridgeMessage::Invoke {
            id: "echo".into(),
            method: "app.echo".into(),
            params: json!({ "value": 42 }),
        });
        assert_eq!(
            response,
            vec![BridgeMessage::Response {
                id: "echo".into(),
                ok: true,
                result: Some(json!({ "value": 42 })),
                error: None,
            }]
        );
    }

    #[test]
    fn rejects_oversized_partial_frames() {
        let mut decoder = JsonLines::new(4);
        let errors = decoder.push("12345");
        assert_eq!(errors, vec![Err(BridgeProtocolError::FrameTooLarge(4))]);
    }
}

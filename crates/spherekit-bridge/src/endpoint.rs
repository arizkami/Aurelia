//! The native endpoint: validates React commits and answers the native API.
//!
//! Traffic is deliberately asymmetric. JavaScript drives the conversation —
//! it commits trees and invokes methods, and gets an answer on the same call —
//! while the native side has no way to interrupt it, because the isolate has no
//! event loop of its own to interrupt. Native events are therefore *queued*
//! here and handed over the next time the host pumps, which is what
//! [`ApiBridge::take_outbound`] exists for. A transport that can push (a socket,
//! a WebView callback) writes them out immediately; the in-process V8 host in
//! `crate::js` calls back into JavaScript at a point in the frame it chooses.

use crate::protocol::{
    BridgeError, BridgeMessage, CAPABILITIES, PROTOCOL_VERSION, capabilities, encode,
};
use serde_json::Value;
use spherekit_core::{Px, Size, px};
use spherekit_css::{ColorScheme, StyleContext};
use spherekit_react::{EventQueue, HostEvent, ReactHost, ReactHostError};
use std::collections::BTreeMap;

/// An application-supplied native method.
///
/// Not `Send`: the isolate that calls these is thread-affine, so the bound was
/// buying nothing while excluding the `Rc` handles an embedder actually has —
/// a quit flag, a shared audio engine, the window it wants to close.
type ApiMethod = Box<dyn FnMut(&mut ReactHost, Value) -> Result<Value, BridgeError>>;

/// Native endpoint that validates React commits and handles the built-in API.
pub struct ApiBridge {
    host: ReactHost,
    methods: BTreeMap<String, ApiMethod>,
    outbound: Vec<BridgeMessage>,
    events: EventQueue,
}

impl Default for ApiBridge {
    fn default() -> Self {
        Self {
            host: ReactHost::new(),
            methods: BTreeMap::new(),
            outbound: Vec::new(),
            events: EventQueue::new(),
        }
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

    /// The React host's shared event queue, handed to `ui_element_with_events`.
    ///
    /// Widgets built from that call push into this queue from their callbacks,
    /// which is why it is shared rather than owned by the element tree: the tree
    /// is rebuilt every frame and anything it owned would be thrown away with
    /// the press that produced it.
    pub fn event_queue(&self) -> &EventQueue {
        &self.events
    }

    /// Registers an application API method callable from React's `invoke`.
    ///
    /// Method names should be namespaced by the application, for example
    /// `audio.getMeter` or `project.save`. Built-in `spherekit.*` methods stay
    /// available unless an application intentionally replaces one.
    pub fn register_method(
        &mut self,
        method: impl Into<String>,
        handler: impl FnMut(&mut ReactHost, Value) -> Result<Value, BridgeError> + 'static,
    ) {
        self.methods.insert(method.into(), Box::new(handler));
    }

    /// Queues a native event for delivery to JavaScript.
    pub fn emit(&mut self, event: impl Into<String>, node_id: Option<u64>, payload: Option<Value>) {
        self.outbound.push(BridgeMessage::Event { event: event.into(), node_id, payload });
    }

    /// Takes every queued outbound message.
    pub fn take_outbound(&mut self) -> Vec<BridgeMessage> {
        std::mem::take(&mut self.outbound)
    }

    /// Drains the React host's `EventQueue` into the outbound queue in one call.
    ///
    /// Takes the queue as an argument rather than only using
    /// [`ApiBridge::event_queue`] because an application that also raises events
    /// from native chrome — a menu, a status bar, a MIDI thread's handoff —
    /// keeps its own queue and wants both to arrive in one ordered batch.
    pub fn pump_events(&mut self, queue: &EventQueue) {
        self.outbound.extend(queue.drain().into_iter().map(outbound_event));
    }

    /// Drains the endpoint's own queue.
    ///
    /// Separate from [`ApiBridge::pump_events`] rather than a call to it,
    /// because `&self.events` and `&mut self` cannot be held at once and
    /// cloning the queue handle at every call would hide that rather than
    /// state it.
    #[cfg(any(feature = "v8", test))]
    pub(crate) fn pump_own_events(&mut self) {
        let drained = self.events.drain();
        self.outbound.extend(drained.into_iter().map(outbound_event));
    }

    /// Dispatches one decoded message and returns zero or more responses/events.
    ///
    /// The return value is the *immediate* reply only. Anything a handler queued
    /// on the way through stays in the outbound queue, so a transport decides
    /// whether an event rides back on this call or waits for the next pump.
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
                    capabilities: capabilities(),
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
            "spherekit.getNodeCount" => Ok(serde_json::json!({
                "nodes": self.host.node_count(),
                "revision": self.host.tree().revision,
            })),
            "spherekit.setStylesheet" => {
                let css = params.get("css").and_then(Value::as_str).ok_or_else(|| {
                    BridgeError::new("invalid_params", "expected { css: string }")
                })?;
                self.host
                    .set_stylesheet(css)
                    .map_err(|error| BridgeError::new("invalid_css", error.to_string()))?;
                Ok(serde_json::json!({ "rules": self.host.stylesheet().rule_count() }))
            }
            "spherekit.setStyleContext" => {
                let context = style_context(self.host.style_context(), &params)?;
                self.host.set_style_context(context);
                Ok(describe_style_context(&context))
            }
            "spherekit.version" => Ok(serde_json::json!({
                "bridge": env!("CARGO_PKG_VERSION"),
                "protocol": PROTOCOL_VERSION,
            })),
            // Returns its own parameters so one call proves the whole path: the
            // frame encoded, crossed, decoded, and came back the same value. A
            // bare `true` would only prove the transport is alive.
            "spherekit.ping" => Ok(serde_json::json!({ "pong": params })),
            "spherekit.capabilities" => Ok(serde_json::json!(CAPABILITIES)),
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

/// Turns one queued widget event into the frame JavaScript receives.
///
/// A [`HostEvent`] always names a node — a widget callback knows which widget
/// it is — while the wire message makes it optional, because the native side
/// also raises events that belong to no React node at all: a frame tick, a
/// window theme change, an audio device appearing.
fn outbound_event(event: HostEvent) -> BridgeMessage {
    BridgeMessage::Event {
        event: event.event,
        node_id: Some(event.node_id),
        payload: event.payload,
    }
}

/// Builds the cascade context from `spherekit.setStyleContext` parameters.
///
/// Absent fields keep their current value rather than resetting to the default,
/// because the two callers of this are a resize handler that knows only the new
/// viewport and a theme listener that knows only the new colour scheme. Making
/// either of them restate the whole context would mean a window resize could
/// silently undo a theme change.
fn style_context(current: &StyleContext, params: &Value) -> Result<StyleContext, BridgeError> {
    let mut context = *current;

    if let Some(viewport) = params.get("viewport") {
        let width = dimension(viewport, "width")?;
        let height = dimension(viewport, "height")?;
        context.viewport = Size::new(width, height);
    }

    if let Some(root) = params.get("rootFontSize") {
        let size = root.as_f64().ok_or_else(|| {
            BridgeError::new("invalid_params", "rootFontSize must be a number of pixels")
        })?;
        // `em` and `rem` mean the same thing at the root, so a context that set
        // only one of them would make `1em` and `1rem` disagree on the root
        // element itself — a difference no stylesheet author would expect.
        context.root_font_size = px(size as f32);
        context.font_size = px(size as f32);
    }

    if let Some(scheme) = params.get("colorScheme") {
        context.color_scheme = match scheme.as_str() {
            Some("light") => ColorScheme::Light,
            Some("dark") => ColorScheme::Dark,
            _ => {
                return Err(BridgeError::new(
                    "invalid_params",
                    "colorScheme must be \"light\" or \"dark\"",
                ));
            }
        };
    }

    Ok(context)
}

fn dimension(viewport: &Value, axis: &str) -> Result<Px, BridgeError> {
    let value = viewport.get(axis).and_then(Value::as_f64).ok_or_else(|| {
        BridgeError::new("invalid_params", format!("viewport.{axis} must be a number of pixels"))
    })?;
    Ok(px(value as f32))
}

fn describe_style_context(context: &StyleContext) -> Value {
    serde_json::json!({
        "viewport": {
            "width": context.viewport.width.get(),
            "height": context.viewport.height.get(),
        },
        "rootFontSize": context.root_font_size.get(),
        "colorScheme": match context.color_scheme {
            ColorScheme::Light => "light",
            ColorScheme::Dark => "dark",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn commit(bridge: &mut ApiBridge, snapshot: Value) {
        let replies = bridge.dispatch(BridgeMessage::Commit {
            snapshot: serde_json::from_value(snapshot).unwrap(),
        });
        assert!(replies.is_empty(), "a valid commit answers nothing: {replies:?}");
    }

    fn result_of(replies: Vec<BridgeMessage>) -> Value {
        match replies.into_iter().next() {
            Some(BridgeMessage::Response { ok: true, result: Some(result), .. }) => result,
            other => panic!("expected a successful response, got {other:?}"),
        }
    }

    fn error_of(replies: Vec<BridgeMessage>) -> BridgeError {
        match replies.into_iter().next() {
            Some(BridgeMessage::Response { ok: false, error: Some(error), .. }) => error,
            other => panic!("expected a failed response, got {other:?}"),
        }
    }

    fn invoke(bridge: &mut ApiBridge, method: &str, params: Value) -> Vec<BridgeMessage> {
        bridge.dispatch(BridgeMessage::Invoke {
            id: "request".into(),
            method: method.into(),
            params,
        })
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

        commit(
            &mut bridge,
            json!({ "revision": 1, "children": [{"id": 1, "type": "text", "children": []}] }),
        );
        let response = invoke(&mut bridge, "spherekit.getTree", json!({}));
        assert!(matches!(response.as_slice(), [BridgeMessage::Response { ok: true, .. }]));
    }

    #[test]
    fn stylesheet_can_be_updated_through_the_builtin_api() {
        let mut bridge = ApiBridge::new();
        let response = invoke(
            &mut bridge,
            "spherekit.setStylesheet",
            json!({ "css": ".panel { gap: 8px; }" }),
        );
        assert_eq!(
            response,
            vec![BridgeMessage::Response {
                id: "request".into(),
                ok: true,
                result: Some(json!({ "rules": 1 })),
                error: None,
            }]
        );
    }

    #[test]
    fn application_methods_can_extend_the_builtin_api() {
        let mut bridge = ApiBridge::new();
        bridge.register_method("app.echo", |_host, params| Ok(params));
        let response = invoke(&mut bridge, "app.echo", json!({ "value": 42 }));
        assert_eq!(
            response,
            vec![BridgeMessage::Response {
                id: "request".into(),
                ok: true,
                result: Some(json!({ "value": 42 })),
                error: None,
            }]
        );
    }

    #[test]
    fn an_application_method_may_capture_a_non_send_handle() {
        // The isolate calling these is thread-affine, so a captured `Rc` — a
        // quit flag, a window, an audio engine — is the normal shape of an
        // application method. A `Send` bound would forbid every one of them.
        let quit = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = std::rc::Rc::clone(&quit);
        let mut bridge = ApiBridge::new();
        bridge.register_method("app.quit", move |_host, _params| {
            flag.set(true);
            Ok(Value::Null)
        });

        invoke(&mut bridge, "app.quit", json!({}));
        assert!(quit.get());
    }

    #[test]
    fn the_handshake_and_the_capabilities_method_advertise_the_same_list() {
        let mut bridge = ApiBridge::new();
        let hello = bridge.dispatch(BridgeMessage::Hello {
            version: PROTOCOL_VERSION,
            runtime: "react".into(),
            platform: "test".into(),
        });
        let advertised = match hello.as_slice() {
            [BridgeMessage::Ready { capabilities, .. }] => capabilities.clone(),
            other => panic!("expected a ready message, got {other:?}"),
        };
        let queried = result_of(invoke(&mut bridge, "spherekit.capabilities", json!({})));
        assert_eq!(queried, json!(advertised));

        // Every advertised `spherekit.*` capability has to be a method that
        // actually answers, which is the half of the drift the shared const
        // cannot catch on its own.
        for name in advertised.iter().filter(|name| name.starts_with("spherekit.")) {
            let error = bridge.invoke(name, json!({ "id": 0, "css": "" })).err();
            assert!(
                !matches!(error.as_ref().map(|e| e.code.as_str()), Some("method_not_found")),
                "{name} is advertised but not implemented"
            );
        }
    }

    #[test]
    fn emitted_events_wait_in_the_outbound_queue_until_they_are_taken() {
        let mut bridge = ApiBridge::new();
        bridge.emit("frame", None, Some(json!({ "count": 1 })));
        bridge.emit("press", Some(7), None);

        let taken = bridge.take_outbound();
        assert_eq!(
            taken,
            vec![
                BridgeMessage::Event {
                    event: "frame".into(),
                    node_id: None,
                    payload: Some(json!({ "count": 1 })),
                },
                BridgeMessage::Event { event: "press".into(), node_id: Some(7), payload: None },
            ]
        );
        assert!(bridge.take_outbound().is_empty(), "taking the queue must empty it");
    }

    #[test]
    fn dispatching_does_not_steal_queued_events() {
        let mut bridge = ApiBridge::new();
        bridge.emit("frame", None, None);
        let replies = invoke(&mut bridge, "spherekit.ping", json!(null));
        assert_eq!(replies.len(), 1, "the response must not carry the queued event");
        assert_eq!(bridge.take_outbound().len(), 1);
    }

    #[test]
    fn pump_events_moves_widget_events_into_the_outbound_queue() {
        let mut bridge = ApiBridge::new();
        let queue = EventQueue::new();
        queue.push(HostEvent::with_payload("valueChange", 4, json!({ "value": 0.5 })));

        bridge.pump_events(&queue);
        assert!(queue.is_empty(), "a pumped queue is an empty queue");
        assert_eq!(
            bridge.take_outbound(),
            vec![BridgeMessage::Event {
                event: "valueChange".into(),
                node_id: Some(4),
                payload: Some(json!({ "value": 0.5 })),
            }]
        );
    }

    #[test]
    fn the_endpoints_own_queue_is_the_one_widgets_are_built_against() {
        let mut bridge = ApiBridge::new();
        bridge.event_queue().push(HostEvent::new("press", 1));
        bridge.pump_own_events();
        assert_eq!(
            bridge.take_outbound(),
            vec![BridgeMessage::Event { event: "press".into(), node_id: Some(1), payload: None }]
        );
    }

    #[test]
    fn node_count_counts_the_whole_tree_not_just_the_roots() {
        let mut bridge = ApiBridge::new();
        commit(
            &mut bridge,
            json!({
                "revision": 1,
                "children": [{
                    "id": 1,
                    "type": "view",
                    "children": [
                        { "id": 2, "type": "#text", "text": "a", "children": [] },
                        { "id": 3, "type": "view", "children": [
                            { "id": 4, "type": "#text", "text": "b", "children": [] }
                        ]}
                    ]
                }]
            }),
        );
        let count = result_of(invoke(&mut bridge, "spherekit.getNodeCount", json!({})));
        assert_eq!(count, json!({ "nodes": 4, "revision": 1 }));
    }

    #[test]
    fn style_context_is_forwarded_to_the_react_host() {
        let mut bridge = ApiBridge::new();
        let applied = result_of(invoke(
            &mut bridge,
            "spherekit.setStyleContext",
            json!({
                "viewport": { "width": 800.0, "height": 600.0 },
                "rootFontSize": 18.0,
                "colorScheme": "dark",
            }),
        ));
        assert_eq!(
            applied,
            json!({
                "viewport": { "width": 800.0, "height": 600.0 },
                "rootFontSize": 18.0,
                "colorScheme": "dark",
            })
        );

        let context = bridge.host().style_context();
        assert_eq!(context.viewport.width.get(), 800.0);
        assert_eq!(context.color_scheme, ColorScheme::Dark);
        // `em` at the root is `rem` at the root, so both moved together.
        assert_eq!(context.font_size, context.root_font_size);
    }

    #[test]
    fn a_partial_style_context_update_keeps_what_it_did_not_mention() {
        // A resize handler knows the viewport and nothing else; it must not
        // undo the theme the system listener set a moment earlier.
        let mut bridge = ApiBridge::new();
        invoke(&mut bridge, "spherekit.setStyleContext", json!({ "colorScheme": "dark" }));
        let applied = result_of(invoke(
            &mut bridge,
            "spherekit.setStyleContext",
            json!({ "viewport": { "width": 400.0, "height": 300.0 } }),
        ));
        assert_eq!(applied["colorScheme"], json!("dark"));
        assert_eq!(applied["viewport"]["width"], json!(400.0));
        assert_eq!(bridge.host().style_context().color_scheme, ColorScheme::Dark);
    }

    #[test]
    fn an_unknown_colour_scheme_is_refused_rather_than_guessed() {
        let mut bridge = ApiBridge::new();
        let error = error_of(invoke(
            &mut bridge,
            "spherekit.setStyleContext",
            json!({ "colorScheme": "sepia" }),
        ));
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn ping_returns_what_it_was_given() {
        let mut bridge = ApiBridge::new();
        let result = result_of(invoke(&mut bridge, "spherekit.ping", json!({ "n": 1 })));
        assert_eq!(result, json!({ "pong": { "n": 1 } }));
    }

    #[test]
    fn version_reports_both_the_crate_and_the_protocol() {
        let mut bridge = ApiBridge::new();
        let result = result_of(invoke(&mut bridge, "spherekit.version", json!({})));
        assert_eq!(result["protocol"], json!(PROTOCOL_VERSION));
        assert_eq!(result["bridge"], json!(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn a_rejected_commit_reports_itself_as_an_event_not_a_silent_drop() {
        let mut bridge = ApiBridge::new();
        commit(&mut bridge, json!({ "revision": 2, "children": [] }));
        let replies = bridge.dispatch(BridgeMessage::Commit {
            snapshot: serde_json::from_value(json!({ "revision": 1, "children": [] })).unwrap(),
        });
        let [BridgeMessage::Event { event, payload, .. }] = replies.as_slice() else {
            panic!("a stale commit must answer with one event, got {replies:?}");
        };
        assert_eq!(event, "bridgeError");
        assert_eq!(payload.as_ref().and_then(|p| p.get("code")), Some(&json!("invalid_commit")));
    }

    #[test]
    fn a_mismatched_protocol_version_is_refused_at_the_handshake() {
        let mut bridge = ApiBridge::new();
        let replies = bridge.dispatch(BridgeMessage::Hello {
            version: PROTOCOL_VERSION + 1,
            runtime: "react".into(),
            platform: "test".into(),
        });
        assert_eq!(error_of(replies).code, "unsupported_version");
    }
}

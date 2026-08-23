//! The native-to-JavaScript event bridge.
//!
//! React's callbacks live in the JavaScript heap and cannot be handed to a
//! native widget, so the two sides meet at a queue of plain values instead. A
//! widget callback pushes a [`HostEvent`] naming the node it happened on; the
//! embedder drains the queue once per turn and forwards the events over the
//! bridge, where `createRoot`'s dispatcher looks the node up and calls the
//! React prop.
//!
//! The queue is deliberately not a channel. Widget callbacks fire during event
//! dispatch on the UI thread, the drain happens on the same thread a moment
//! later, and a channel would buy nothing but an allocation per send and a
//! `Send` bound that `AnyElement` cannot satisfy anyway.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

/// A native event queued for delivery to the JavaScript side.
///
/// The field names serialise to exactly the `NativeEvent` shape the TypeScript
/// renderer decodes, so an embedder can forward one of these without a
/// translation table in between.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostEvent {
    /// The renderer's event name, such as `press` or `valueChange`.
    ///
    /// Bare rather than `onPress`, because the TypeScript side capitalises and
    /// prefixes it when it looks the prop up. Sending the prop name instead
    /// would make the host decide how React spells its callbacks.
    pub event: String,
    /// The React host identity the event happened on.
    pub node_id: u64,
    /// Event-specific values, always an object when present.
    ///
    /// An object rather than a bare value because the wire schema has one slot
    /// and some events carry more than one number; the renderer unwraps the
    /// single key its callback wants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

impl HostEvent {
    /// An event that carries nothing but its identity.
    pub fn new(event: impl Into<String>, node_id: u64) -> Self {
        Self { event: event.into(), node_id, payload: None }
    }

    /// An event with a payload object.
    pub fn with_payload(event: impl Into<String>, node_id: u64, payload: Value) -> Self {
        Self { event: event.into(), node_id, payload: Some(payload) }
    }
}

/// A shared queue that native widget callbacks push into.
///
/// Cloning shares the same queue, which is the whole point: every callback the
/// lowering pass installs holds its own clone, and the embedder holds one more
/// to drain from.
#[derive(Clone, Default)]
pub struct EventQueue {
    queued: Rc<RefCell<Vec<HostEvent>>>,
}

impl EventQueue {
    /// Creates an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one event.
    pub fn push(&self, event: HostEvent) {
        self.queued.borrow_mut().push(event);
    }

    /// Takes everything queued since the last drain.
    pub fn drain(&self) -> Vec<HostEvent> {
        std::mem::take(&mut *self.queued.borrow_mut())
    }

    /// Whether anything is waiting to be drained.
    ///
    /// Worth checking before a drain: an embedder that pumps every frame is
    /// asking this question far more often than it gets an answer, and the
    /// borrow is cheaper than the `Vec` a drain would hand back.
    pub fn is_empty(&self) -> bool {
        self.queued.borrow().is_empty()
    }

    /// How many events are waiting.
    pub fn len(&self) -> usize {
        self.queued.borrow().len()
    }
}

impl fmt::Debug for EventQueue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("EventQueue").field("queued", &self.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_clone_pushes_into_the_same_queue() {
        let queue = EventQueue::new();
        let widget = queue.clone();
        widget.push(HostEvent::new("press", 7));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn draining_empties_the_queue() {
        let queue = EventQueue::new();
        queue.push(HostEvent::new("press", 1));
        queue.push(HostEvent::new("select", 2));
        assert_eq!(queue.drain().len(), 2);
        assert!(queue.is_empty());
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn an_event_serialises_to_the_renderers_frame_shape() {
        let event = HostEvent::with_payload("valueChange", 3, json!({ "value": 0.5 }));
        let json = serde_json::to_string(&event).expect("a host event serialises");
        assert_eq!(json, r#"{"event":"valueChange","nodeId":3,"payload":{"value":0.5}}"#);
    }

    #[test]
    fn an_event_without_a_payload_omits_the_field() {
        let json = serde_json::to_string(&HostEvent::new("press", 9)).expect("serialises");
        assert_eq!(json, r#"{"event":"press","nodeId":9}"#);
    }
}

//! The in-process JavaScript host: a V8 isolate wired to an [`ApiBridge`].
//!
//! This is the transport that has no transport. The isolate lives in the same
//! address space and on the same thread as the renderer, so `__spherekitSend`
//! is an ordinary synchronous call: JavaScript hands over a frame and gets the
//! replies back before the calling expression finishes. An `invoke()` issued
//! from a React effect can therefore settle inside the same commit that made
//! it, which no socket-shaped bridge can offer.
//!
//! ## Why the endpoint lives behind `Rc<RefCell<_>>`
//!
//! V8 owns the `__spherekitSend` closure for the life of the isolate, and that
//! closure has to reach the same [`ApiBridge`] the embedder reads the committed
//! tree from. Two owners, one value, one thread — an `Rc<RefCell<_>>` is the
//! honest spelling. The discipline it demands is the part worth remembering:
//! **no borrow may be held across a call into the engine.** Every method here
//! takes what it needs out of the cell, drops the borrow, and only then calls
//! into JavaScript, because JavaScript is entitled to call `__spherekitSend`
//! from anywhere — a timer, a promise continuation, an event handler — and a
//! borrow still standing at that moment is a panic in a stack frame that will
//! look completely unrelated six months from now.

use crate::endpoint::ApiBridge;
use crate::protocol::{BridgeError, BridgeMessage, JsonLines, PROTOCOL_VERSION, encode};
use serde_json::Value;
use spherekit_jsengine::Engine;
use std::cell::{Ref, RefCell, RefMut};
use std::rc::Rc;
use thiserror::Error;

/// Script name used for the globals the host installs itself, so a stack trace
/// from inside them is not attributed to the application's bundle.
const HOST_GLOBALS: &str = "spherekit:host-globals.js";

/// Errors from starting or driving the JavaScript host.
#[derive(Debug, Error)]
pub enum JsBridgeError {
    /// V8 refused to start, or a call into it failed or threw.
    #[error(transparent)]
    Engine(#[from] spherekit_jsengine::Error),
    /// A native event could not be encoded as a frame.
    ///
    /// Separate from an engine failure because the fault is on the native side:
    /// some `serde_json::Value` in an event payload is not serialisable, and no
    /// amount of retrying the JavaScript call will help.
    #[error("could not encode a bridge frame: {0}")]
    Encode(String),
}

/// A V8 isolate wired to an [`ApiBridge`], running a JavaScript UI in process.
pub struct JsBridge {
    engine: Engine,
    bridge: Rc<RefCell<ApiBridge>>,
}

impl JsBridge {
    /// Boots V8 and installs the host globals.
    pub fn new() -> Result<Self, JsBridgeError> {
        let bridge = Rc::new(RefCell::new(ApiBridge::new()));
        let mut engine = Engine::new()?;

        let endpoint = Rc::clone(&bridge);
        engine.bind("__spherekitSend", move |frame| Ok(dispatch_frame(&endpoint, frame)))?;

        // Values, not functions, so they cannot go through `bind`. Written
        // through `serde_json` rather than string concatenation because the
        // crate version is data and this is a script being generated.
        let version = serde_json::to_string(env!("CARGO_PKG_VERSION"))
            .map_err(|error| JsBridgeError::Encode(error.to_string()))?;
        engine.eval_named(
            &format!(
                "globalThis.__spherekitVersion = {version};\n\
                 globalThis.__spherekitProtocol = {PROTOCOL_VERSION};\n"
            ),
            HOST_GLOBALS,
        )?;

        Ok(Self { engine, bridge })
    }

    /// Evaluates the runtime prelude that gives the bare V8 context its web
    /// globals.
    ///
    /// Kept separate from [`JsBridge::load`] because the prelude is the
    /// environment and the bundle is the application: a prelude failure means
    /// the isolate is unusable, while a bundle failure is an application bug
    /// that an embedder may want to report on screen and carry on from.
    pub fn install_prelude(&mut self, prelude: &str) -> Result<(), JsBridgeError> {
        self.engine.eval_named(prelude, "spherekit:prelude.js")?;
        Ok(())
    }

    /// Evaluates an application bundle.
    pub fn load(&mut self, source: &str, name: &str) -> Result<(), JsBridgeError> {
        self.engine.eval_named(source, name)?;
        Ok(())
    }

    /// Runs microtasks, platform tasks and the prelude's timer queue, then
    /// flushes queued native events into JavaScript. Call once per frame.
    ///
    /// The flush is last and is a backstop: it delivers what a method invoked
    /// during this tick queued. An event that has to be *visible* to the render
    /// this frame produces belongs before the timers, so call
    /// [`JsBridge::pump_events`] first — React commits state changes on its
    /// scheduler, which is the prelude's timer queue, so an event delivered
    /// after the timers have run waits a whole frame for its commit.
    pub fn tick(&mut self) -> Result<(), JsBridgeError> {
        self.engine.run_microtasks()?;
        self.engine.pump()?;
        self.run_timers()?;
        self.engine.run_microtasks()?;
        self.flush()
    }

    /// Sends one native event to JavaScript immediately.
    pub fn emit(
        &mut self,
        event: &str,
        node_id: Option<u64>,
        payload: Option<Value>,
    ) -> Result<(), JsBridgeError> {
        self.bridge.borrow_mut().emit(event, node_id, payload);
        self.flush()
    }

    /// Drains the React host's event queue into JavaScript.
    pub fn pump_events(&mut self) -> Result<(), JsBridgeError> {
        self.bridge.borrow_mut().pump_own_events();
        self.flush()
    }

    /// The shared endpoint, for reading the committed tree and building native
    /// elements.
    pub fn bridge(&self) -> Ref<'_, ApiBridge> {
        self.bridge.borrow()
    }

    /// The shared endpoint, for registering methods and queueing events.
    pub fn bridge_mut(&self) -> RefMut<'_, ApiBridge> {
        self.bridge.borrow_mut()
    }

    /// Console output the prelude buffered since the last call.
    ///
    /// Returns lines rather than a `Result` because an embedder calls this from
    /// a frame loop to print diagnostics: a failure here has to be reportable
    /// the same way the diagnostics are, not a second error path around them.
    pub fn drain_console(&mut self) -> Vec<String> {
        match self.engine.call_global("__drainConsole", "") {
            Ok(Some(text)) => {
                text.lines().filter(|line| !line.is_empty()).map(ToOwned::to_owned).collect()
            }
            Ok(None) => Vec::new(),
            Err(error) => vec![format!("error: console drain failed: {error}")],
        }
    }

    /// Delivers every queued outbound message to `__spherekitReceive`.
    fn flush(&mut self) -> Result<(), JsBridgeError> {
        // Borrow, take, drop — before a single frame reaches JavaScript, which
        // is entitled to call `__spherekitSend` back into the same cell.
        let outbound = self.bridge.borrow_mut().take_outbound();
        for message in outbound {
            let frame =
                encode(&message).map_err(|error| JsBridgeError::Encode(error.to_string()))?;
            self.engine.call_global("__spherekitReceive", &frame)?;
        }
        Ok(())
    }

    /// Runs the prelude's due timers, tolerating a context with no prelude.
    fn run_timers(&mut self) -> Result<(), JsBridgeError> {
        self.engine.call_global("__runTimers", "")?;
        Ok(())
    }
}

/// Returns the V8 version this build embeds.
///
/// Re-exposed here so an embedder that only wants to print it in a status bar
/// does not have to depend on `spherekit-jsengine` directly.
pub fn v8_version() -> String {
    spherekit_jsengine::v8_version()
}

/// Handles one `__spherekitSend` call: decode, dispatch, reply.
///
/// A renderer bug must not take the isolate down with it, so nothing here
/// returns `Err`: a frame that does not parse comes back as a `response`
/// carrying the protocol error, which the TypeScript transport can log and the
/// tests can assert on. Throwing instead would unwind through V8 into a React
/// commit, where the only honest thing left to do is tear down the isolate.
fn dispatch_frame(endpoint: &Rc<RefCell<ApiBridge>>, frame: &str) -> String {
    let mut chunk = frame.to_owned();
    if !chunk.ends_with('\n') {
        // The transport appends the newline, but a hand-written client speaking
        // the protocol directly is a supported thing to do, and a frame that is
        // never terminated is a request that never gets an answer.
        chunk.push('\n');
    }

    let mut decoder = JsonLines::default();
    let mut replies: Vec<String> = Vec::new();
    {
        let mut endpoint = endpoint.borrow_mut();
        for decoded in decoder.push(&chunk) {
            match decoded {
                Ok(message) => {
                    for reply in endpoint.dispatch(message) {
                        push_frame(&mut replies, &reply);
                    }
                }
                Err(error) => push_frame(
                    &mut replies,
                    &BridgeMessage::Response {
                        id: "protocol".into(),
                        ok: false,
                        result: None,
                        error: Some(BridgeError::new("invalid_frame", error.to_string())),
                    },
                ),
            }
        }
        // Anything a handler queued on the way through rides back on this same
        // synchronous call rather than waiting for the next frame's flush.
        for message in endpoint.take_outbound() {
            push_frame(&mut replies, &message);
        }
    }

    serde_json::to_string(&replies).unwrap_or_else(|_| String::from("[]"))
}

/// Appends one message as a frame, or a diagnostic in its place.
///
/// A reply that will not serialise cannot be repaired here, but dropping it
/// would strand the request it was answering — a promise on the JavaScript side
/// that never settles and never says why. An error frame at least arrives.
fn push_frame(replies: &mut Vec<String>, message: &BridgeMessage) {
    /// Written out rather than built, for the one case where building a message
    /// is the thing that failed.
    const UNSERIALIZABLE: &str = concat!(
        r#"{"kind":"response","id":"protocol","ok":false,"#,
        r#""error":{"code":"unserializable_reply","message":"reply not serialisable"}}"#
    );

    let frame = serde_json::to_string(message).or_else(|error| {
        serde_json::to_string(&BridgeMessage::Response {
            id: "protocol".into(),
            ok: false,
            result: None,
            error: Some(BridgeError::new("unserializable_reply", error.to_string())),
        })
    });
    replies.push(frame.unwrap_or_else(|_| UNSERIALIZABLE.to_owned()));
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use serde_json::json;

    /// Evaluates a script and returns its completion value.
    fn eval(js: &mut JsBridge, source: &str) -> String {
        js.engine.eval_named(source, "test.js").expect("script evaluates")
    }

    fn bridge() -> JsBridge {
        JsBridge::new().expect("V8 starts")
    }

    #[test]
    fn a_javascript_client_completes_a_hello_commit_invoke_round_trip() {
        let mut js = bridge();
        let summary = eval(
            &mut js,
            r##"
            const send = (message) => JSON.parse(__spherekitSend(JSON.stringify(message) + "\n"))
                .map((frame) => JSON.parse(frame));

            const ready = send({
                kind: "hello", version: __spherekitProtocol, runtime: "react", platform: "test",
            });
            const committed = send({ kind: "commit", snapshot: { revision: 1, children: [
                { id: 1, type: "view", children: [
                    { id: 2, type: "#text", text: "hi", children: [] },
                ] },
            ] } });
            const tree = send({
                kind: "invoke", id: "r1", method: "spherekit.getTree", params: {},
            });

            JSON.stringify({
                ready: ready[0].kind,
                capabilities: ready[0].capabilities.length,
                committed: committed.length,
                revision: tree[0].result.revision,
                id: tree[0].id,
            });
            "##,
        );
        let summary: Value = serde_json::from_str(&summary).expect("summary is JSON");
        assert_eq!(summary["ready"], json!("ready"));
        assert_eq!(summary["committed"], json!(0), "a valid commit answers nothing");
        assert_eq!(summary["revision"], json!(1));
        assert_eq!(summary["id"], json!("r1"));
        assert!(summary["capabilities"].as_u64().unwrap_or(0) >= 3);

        // The commit really landed on the native side, not just in the reply.
        assert_eq!(js.bridge().host().tree().revision, 1);
    }

    #[test]
    fn an_event_pushed_from_native_arrives_at_the_javascript_receiver() {
        let mut js = bridge();
        eval(
            &mut js,
            "globalThis.seen = [];\n\
             globalThis.__spherekitReceive = (frame) => { seen.push(JSON.parse(frame)); };",
        );

        js.emit("frame", None, Some(json!({ "count": 3 }))).expect("event delivers");
        js.bridge().event_queue().push(spherekit_react::HostEvent::new("press", 9));
        js.pump_events().expect("queued events deliver");

        let seen = eval(&mut js, "JSON.stringify(globalThis.seen)");
        let seen: Value = serde_json::from_str(&seen).expect("seen is JSON");
        assert_eq!(
            seen[0],
            json!({ "kind": "event", "event": "frame", "payload": { "count": 3 } })
        );
        assert_eq!(seen[1], json!({ "kind": "event", "event": "press", "nodeId": 9 }));
    }

    #[test]
    fn malformed_json_from_javascript_becomes_an_error_response() {
        let mut js = bridge();
        let replies = eval(&mut js, r#"__spherekitSend("{ this is not json }\n")"#);
        let replies: Vec<String> =
            serde_json::from_str(&replies).expect("replies are a JSON array");
        assert_eq!(replies.len(), 1);
        let reply: Value = serde_json::from_str(&replies[0]).expect("reply is JSON");
        assert_eq!(reply["kind"], json!("response"));
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["error"]["code"], json!("invalid_frame"));

        // The isolate survived, which is the whole point of not throwing.
        assert_eq!(eval(&mut js, "1 + 1"), "2");
    }

    #[test]
    fn ticking_a_context_without_a_prelude_is_not_an_error() {
        let mut js = bridge();
        js.tick().expect("a bare context still ticks");
        assert!(js.drain_console().is_empty());
    }

    #[test]
    fn the_prelude_timer_queue_is_driven_by_tick() {
        let mut js = bridge();
        js.install_prelude(spherekit_react::PRELUDE)
            .expect("prelude installs");
        eval(
            &mut js,
            "globalThis.ran = 0; setTimeout(() => { ran += 1; console.log('timer'); }, 0);",
        );
        assert_eq!(eval(&mut js, "String(globalThis.ran)"), "0", "nothing runs on its own");

        js.tick().expect("tick runs timers");
        assert_eq!(eval(&mut js, "String(globalThis.ran)"), "1");
        assert_eq!(js.drain_console(), vec!["log: timer".to_owned()]);
    }

    #[test]
    fn the_host_globals_describe_the_protocol_this_build_speaks() {
        let mut js = bridge();
        assert_eq!(eval(&mut js, "String(__spherekitProtocol)"), PROTOCOL_VERSION.to_string());
        assert_eq!(eval(&mut js, "__spherekitVersion"), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_method_registered_from_rust_is_callable_from_javascript() {
        let mut js = bridge();
        js.bridge_mut().register_method("app.double", |_host, params| {
            let value = params.get("value").and_then(Value::as_i64).unwrap_or_default();
            Ok(json!({ "value": value * 2 }))
        });
        let reply = eval(
            &mut js,
            r#"
            const request = { kind: "invoke", id: "d", method: "app.double", params: { value: 21 } };
            JSON.parse(__spherekitSend(JSON.stringify(request) + "\n"))[0];
            "#,
        );
        let reply: Value = serde_json::from_str(&reply).expect("reply is JSON");
        assert_eq!(reply["result"], json!({ "value": 42 }));
    }
}

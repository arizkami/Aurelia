# API Bridge

SphereKit's React integration uses a newline-delimited JSON protocol. The protocol does not choose a
transport: the application can connect it to a WebView callback, an FFI function, a child process, a
socket, or the in-process V8 host.

JSON Lines is the only framing every one of those already agrees on. A length-prefixed binary frame
would have meant writing the framing three times over; the cost of the text form is that a message
may not contain a raw newline, which `serde_json`'s compact output never emits, so nothing has to
escape anything.

Frames are versioned by `PROTOCOL_VERSION` and refused at the handshake rather than negotiated
per-field. A renderer speaking a different version is turned away, because a half-understood commit
is worse than no commit: it paints something the application never described.

## Message flow

```text
React                          Native
  │── hello ────────────────────>│
  │<─ ready ─────────────────────│
  │── commit(snapshot) ─────────>│   (silence means accepted)
  │── invoke(method, params) ───>│
  │<─ response(id, result/error) │
  │<─ event(event, nodeId, data) │
  │── shutdown ─────────────────>│
```

The commit is a complete React tree, accepted only when its revision is newer than the retained
native tree. A valid commit answers **nothing** — the reply channel is for failures, and an invalid
one comes back as a `bridgeError` event carrying `invalid_commit` rather than as a response, because
a commit has no request id to answer.

## Two directions, two shapes

Traffic is deliberately asymmetric.

**JavaScript → native is request/response.** A `commit` or an `invoke` gets its answer from
`ApiBridge::dispatch` on the same call. JavaScript drives the conversation.

**Native → JavaScript is queued.** The native side has no way to interrupt the runtime — an isolate
has no event loop of its own to interrupt, and a WebView will not accept a call at an arbitrary
moment either. Events are therefore queued with `ApiBridge::emit` and collected by
`ApiBridge::take_outbound` when the host is ready to deliver them. `dispatch` returns only the
*immediate* reply; anything a handler queued on the way through stays in the outbound queue, so a
transport decides whether an event rides back on this call or waits for the next pump.

Widget callbacks deposit their events in a shared `spherekit_react::EventQueue`, and
`ApiBridge::pump_events` turns that queue into outbound messages in one call per frame. It takes the
queue as an argument rather than only draining its own, because an application that also raises
events from native chrome — a menu, a status bar, a MIDI thread's handoff — keeps its own queue and
wants both to arrive in one ordered batch.

A `HostEvent` always names a node, since a widget callback knows which widget it is. The wire message
makes `nodeId` optional because the native side also raises events belonging to no React node at all:
a frame tick, a window theme change, an audio device appearing.

## Built-in methods

`spherekit.capabilities` and the `ready` handshake return the same `CAPABILITIES` list. They used to
be written out twice, which is exactly how they came to disagree — leaving a client that asked the
second way convinced a method the handshake advertised did not exist. One const, two readers.

| Method | Params | Result |
|---|---|---|
| `spherekit.ping` | anything | `{ pong: <params> }` |
| `spherekit.version` | — | `{ bridge, protocol }` |
| `spherekit.capabilities` | — | the capability list |
| `spherekit.getTree` | — | the retained snapshot |
| `spherekit.getNode` | `{ id }` | that node, or `not_found` |
| `spherekit.getNodeCount` | — | `{ nodes, revision }` |
| `spherekit.setStylesheet` | `{ css }` | `{ rules }`, or `invalid_css` |
| `spherekit.setStyleContext` | `{ viewport?, rootFontSize?, colorScheme? }` | the whole resulting context |

`ping` returns its own parameters rather than a bare `true`, so one call proves the entire path: the
frame encoded, crossed, decoded and came back the same value.

An application registers its own methods alongside these. Names should be namespaced by the
application — `audio.getMeter`, `project.save` — and a registered name takes precedence, so a
`spherekit.*` method can be replaced deliberately.

```rust
let mut bridge = ApiBridge::new();
bridge.register_method("app.status", |_host, _params| {
    Ok(serde_json::json!({ "ready": true }))
});
```

A handler is `FnMut(&mut ReactHost, Value) -> Result<Value, BridgeError>` and is **not** `Send`. The
isolate that calls these is thread-affine, so the bound would have bought nothing while excluding the
`Rc` handles an embedder actually has: a quit flag, a shared audio engine, the window it wants to
close.

## The style context

`spherekit.setStyleContext` sets what font-relative and viewport-relative units resolve against, and
which `prefers-color-scheme` query is satisfied.

```ts
await bridge.invoke("spherekit.setStyleContext", {
  viewport: { width: 1280, height: 720 },
  rootFontSize: 16,
  colorScheme: "dark",
});
```

**Absent fields keep their current value** rather than resetting to a default. The two callers are a
resize handler that knows only the new viewport and a theme listener that knows only the new colour
scheme; making either restate the whole context would mean a window resize could silently undo a
theme change. The result echoes the whole resulting context, so a caller can see what it now is
without a second round trip.

`rootFontSize` sets both `root_font_size` and `font_size`. `em` and `rem` mean the same thing at the
root, so a context that set only one would make `1em` and `1rem` disagree on the root element itself
— a difference no stylesheet author would expect.

An unrecognised `colorScheme` is an `invalid_params` error rather than a silent fallback to light.

## TypeScript

```ts
const bridge = createApiBridge(transport);
const root = createRoot(bridge);
await bridge.setStylesheet(`.panel { display: flex; gap: 8px; }`);
const status = await bridge.invoke<{ ready: boolean }>("app.status");
const stop = bridge.on("appReady", () => console.log("ready"));
```

`ApiBridgeTransport` only needs `send`, `subscribe`, and optionally `close`. `createApiBridge`
subscribes before it sends `hello`, correlates `response` frames to pending promises by request id,
and rejects every outstanding promise on `close()` — a request that can never be answered should fail
loudly rather than hang.

A failed request rejects with an `ApiBridgeError` carrying the native `code`, `message` and optional
`data`, so a caller can branch on `error.code === "not_found"` instead of matching message text.

`setStylesheet` sends CSS to the native host. Both React's `className` and inline `style` props are
resolved by the same `spherekit-css` runtime used by native elements — see
[`docs/spherekit-css.md`](spherekit-css.md).

## Rust

```rust
use spherekit_bridge::{ApiBridge, BridgeMessage, JsonLines, PROTOCOL_VERSION};

let mut bridge = ApiBridge::new();
let mut decoder = JsonLines::default();

for chunk in transport_chunks {
    for decoded in decoder.push(&chunk) {
        match decoded {
            Ok(message) => for reply in bridge.dispatch(message) {
                transport.write(&ApiBridge::encode(&reply)?);
            },
            Err(error) => log::warn!("dropped a frame: {error}"),
        }
    }
}

// Once per frame, after the UI has run:
bridge.pump_events(&event_queue);
for message in bridge.take_outbound() {
    transport.write(&ApiBridge::encode(&message)?);
}
```

`JsonLines` handles partial chunks — every split point of a stream decodes to the same messages,
including one byte at a time — and enforces a maximum frame size. That limit is not politeness: a
transport that never sends a newline would otherwise grow the buffer until the process dies, and a
renderer crashing mid-frame is a normal way for that to happen. The default cap is 4 MiB.

The native endpoint validates node identities and rejects malformed or stale commits before they
reach layout and painting.

## `JsBridge`: the in-process V8 host

With the `v8` feature the crate also ships `JsBridge`, which owns a V8 isolate and connects it to an
`ApiBridge` through two host globals. That is a strictly optional extra — the protocol above is the
product, and an embedder using a WebView or a child process never compiles V8 at all.

```rust
let mut js = JsBridge::new()?;                 // boots V8, installs the host globals
js.bridge_mut().register_method("app.quit", …); // ordinary ApiBridge methods
js.install_prelude(PRELUDE)?;                   // web globals a bare context lacks
js.load(BUNDLE, "app.js")?;                     // the application

// per frame:
js.bridge_mut().pump_events(&events);
js.pump_events()?;                              // outbound queue → JavaScript
js.tick()?;                                     // microtasks, platform tasks, timers, flush
for line in js.drain_console() { … }
```

`__spherekitSend` is synchronous, so an `invoke()` issued from a React effect can settle inside the
same commit that made it. A frame that does not parse comes back as an `invalid_frame` response
rather than a thrown exception: throwing would unwind through V8 into a React commit, where the only
honest thing left is to tear the isolate down.

The full embedding — the tick contract, the prelude, bundling, and the `Rc<RefCell<_>>` borrow
discipline — is in [`docs/javascript.md`](javascript.md).

# React, the bridge, and V8

## The whole path

```text
  App.tsx  --bun-->  one IIFE script
                         |
                         v
  prelude.js  ->  V8 isolate  ->  __spherekitSend(frame)
                                       |
                                       v
                                  ApiBridge  ->  ReactHost.commit
                                       |              |
                                       |              v
                                       |         spherekit-css cascade
                                       |              |
                                       |              v
                                       |         spherekit-ui elements
                                       |              |
                                       |              v
                                       |         layout -> paint -> wgpu
                                       |
  __spherekitReceive(frame)  <---------+  native events, by node id
```

Every arrow is synchronous and in-process.

## Why a whole-tree snapshot, not a mutation stream

React reconciles into a private host tree and hands the native side a
**complete** `NativeTree` only after the commit finishes. The native side
therefore never observes a half-built tree, and the entire boundary is one
serialisable value a test can assert on. It also means a second frontend — an
FFI host, a socket, a test harness — speaks the same protocol without
reproducing React's internals.

## Mounting, three ways

```ts
createRoot(jsonBridge(json => rustHost.commitJson(json)))   // FFI
createRoot(createApiBridge(transport))                       // any transport
createRoot(createApiBridge(createV8Transport()))             // in-process V8
```

## Host types and events

Built-ins: `view` `text` `#text` `button` `slider` `knob` `fader` `toggle`
`checkbox` `progress` `separator` `panel` `scroll-view` `text-field` `avatar`
`menu-item`. An unknown type becomes a plain container — a bundle built against
a newer host degrades rather than taking the window down.

Event names crossing the bridge are the renderer's, not React's:

| Rust pushes | TypeScript looks up | Payload |
|---|---|---|
| `press` | `onPress` | none |
| `valueChange` | `onValueChange` | `{value}` |
| `change` | `onChange` | `{checked}` / `{value}` / `{text}` |
| `select` | `onSelect` | none |
| `submit` | `onSubmit` | `{text}` |

Sending `onPress` from Rust would put React's naming convention in the native
host, where a second frontend would have to reproduce it.

## Clickable containers

`serializeProps` strips every `on*` prop — a function is not JSON — so the host
cannot tell an interactive container from a layout one. The TypeScript `View`
and `Native` set a `pressable: true` marker when they see an `onPress`, and
`container()` wires a click handler when it is set.

Gated rather than always on: wiring every container would put an event on the
bridge for every click anywhere in the tree, including the dozens of nested
layout views a real screen is built from.

## Custom host types — the best tool in the box

A per-frame draw callback cannot cross the bridge, and a visualiser that asked
JavaScript for its pixels sixty times a second would put the isolate on the
paint path. So split by responsibility:

```rust
host.register_host_type("spectrum", node_builder(move |cx, node| {
    let state = Rc::clone(&visuals);
    let element = realtime_canvas(move |frame| {
        frame.draw_spectrum(&state.borrow().spectrum, nyquist, &style);
    })
    .id(node.id);
    cx.apply(element).into_element()
}));
```

React writes `<Native type="spectrum" className="spectrum" />` and decides
*where* it goes and how big it is, via CSS. Rust owns *what it draws*, straight
from the audio ring. The audio path never crosses into JavaScript.
`app/musicplayer/src/visualiser.rs` is the worked example.

## Two performance rules that are not optional

**1. The style cache.** `ReactHost` caches the resolved style per node id and
clears it wholesale on a commit, a new stylesheet, or a new style context.
Without it, lowering a 3500-node tree costs ~14 ms/frame; with it, ~2.8. The
invalidation is deliberately not selective — a partial invalidation that is
wrong shows up as a node that will not restyle, which is far harder to see than
a slow frame.

**2. Rate-limit state pushed to React.** Every event you emit makes React
re-render, and this renderer commits the *whole* tree: a full serialise, parse,
validate, lower and layout of every node. Sixty of those a second costs more
than everything else put together. Push only on a real change, and quantise
anything continuous — `app/musicplayer` sends its transport state on change or
a quarter-second of clock movement, roughly four events a second instead of
sixty.

Native visualisers are unaffected by either: they read shared state directly and
stay at frame rate.

## The V8 embedding

`spherekit-jsengine::Engine` owns an isolate. The public API takes and returns
owned UTF-8 and never leaks a `v8::Local`, because handles are scope- and
thread-bound and a leaked one is a use-after-free waiting to happen.

```rust
engine.eval(src)?;                     // completion value as a String
engine.eval_named(src, "app.js")?;     // named, so stack traces mean something
engine.bind("hostFn", |arg| Ok(...))?; // a Rust closure as a JS global
engine.call_global("__cb", arg)?;      // Ok(None) if the global is absent
engine.run_microtasks()?;              // explicit policy: promises land here
engine.pump()?;                        // platform tasks, once per frame
```

The isolate runs on `MicrotasksPolicy::kExplicit`, so `eval` does **not**
implicitly flush promises — call `run_microtasks`.

**The prelude is mandatory.** A bare V8 context has no web and no Node globals
at all. React's modules reach for `setTimeout`, `queueMicrotask`, `performance`
and `console` during evaluation, before a single component renders, so an
isolate without them throws `ReferenceError` at load. Install
`spherekit_react::PRELUDE` before the bundle.

## Bundling

```bash
bun build src/main.tsx --target=browser --format=iife --production --outfile dist/app.js
```

`--production` is not optional: without it Bun emits `jsxDEV`, which the
production React build does not export.

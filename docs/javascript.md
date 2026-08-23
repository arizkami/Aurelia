# JavaScript

`spherekit-jsengine` embeds V8 directly. There is no browser, no WebView and no Node — an isolate
runs on the UI thread, in the renderer's own address space, and the host decides exactly when
JavaScript is allowed to run.

> **Windows x86_64 only today.** The prebuilt monolith `build.rs` fetches is a Windows build; on
> every other target the crate compiles to a stub whose every method returns
> `Error::UnsupportedPlatform`. See [Porting](#porting) at the end.

## Why the Rust surface owns an `Engine`

The obvious binding would hand out `v8::Local<T>` handles and let Rust code build values, read
properties and call functions the way C++ does. Two properties of V8 make that the wrong shape here.

**Isolates are thread-affine.** An isolate is entered by one thread at a time. A handle that escaped
into a `Send` type, a thread pool or a background task would be a use-after-enter that no Rust type
system in the binding could see. `Engine` is deliberately used through `&mut self` and is neither
`Send` nor `Sync`: safe Rust cannot make concurrent calls into it without the compiler objecting,
and nothing pretends the isolate is shareable.

**Local handles are stack-scoped.** A `v8::Local<T>` is only valid inside the `HandleScope` that
created it, which is a C++ stack frame. Rust's lifetimes can model that — several crates do — but
the modelling has to be exactly right or the result is a dangling handle with no diagnostic. The
alternative taken here is to never let a handle out: the native shim opens its scopes, does the
work, and returns *owned* UTF-8 to Rust before the scope closes. The unsafe surface is then a fixed
handful of pointer copies rather than a lifetime puzzle.

**So the API is string-in, string-out.** `Engine::bind` and `Engine::call_global` both speak `&str`,
not a `serde` value and not a `v8::Value`. That is not a placeholder for something richer. Anything
richer forces a shared value model across the FFI boundary, and every host that embeds this crate
already has one — `spherekit-bridge` sends JSON Lines. Keeping the boundary at "one owned UTF-8
buffer each way" means the marshaller does not grow a case for every new type.

## The shim's ownership rules

`src/v8_shim.cc` is compiled by `build.rs` and linked against `v8_monolith.lib`. Three rules govern
every buffer that crosses:

1. **A result buffer comes from `spherekit_v8_alloc` and belongs to Rust.** Rust copies it out and
   releases it with `spherekit_v8_free_buffer`. That is what `take_result` and `take_string` do, and
   it is why an error path frees three buffers (message, stack, resource) rather than one.
2. **An error struct is passed in zeroed.** On success its pointers stay null and nothing needs
   freeing; on a non-zero status they are live and Rust owns them.
3. **A bound closure's buffers belong to the shim.** The `HostCallback` thunk fills `result` *or*
   `error` from `spherekit_v8_alloc`, and the shim frees whichever was produced after reading it.

Bound closures themselves are owned by `Engine`, not by the shim. Each is double-boxed: the outer
box gives the record an address that stays put once V8 has captured it in a `v8::External`, and the
inner box erases the closure's type so one `extern "C"` thunk serves every binding. `Drop` disposes
the isolate **first** and reclaims the boxes afterwards — once the isolate is gone no script can be
running, which is what makes freeing the handlers safe.

A panic must not unwind into C++, so the thunk runs the closure inside `catch_unwind` and reports a
panic to JavaScript as a thrown `Error`. Under `panic = "abort"` that is dead weight; under the test
profile it is the difference between a failing assertion and undefined behaviour.

## `bind` and `call_global`

```rust
let mut engine = Engine::new()?;

engine.bind("__hostLog", |argument| {
    println!("{argument}");
    Ok(String::new())
})?;

engine.eval_named("__hostLog('hello from script')", "app.js")?;
```

`bind(name, handler)` installs `globalThis[name](argument: string) -> string`. The handler's `Err`
becomes a thrown JavaScript `Error` carrying the message, which is what makes a host failure
catchable by script instead of a silent `undefined`. Names are checked against a deliberately strict
rule — ASCII identifier characters only, `$` and `_` allowed — because the point is to catch `""` or
`"app.log"`, both of which V8 would happily accept as property keys and neither of which is callable
as `app.log(x)` from script. Reserved words are *not* rejected: `bind("delete", …)` produces a global
reachable only as `globalThis["delete"](x)`, which is strange but not broken, and carrying a copy of
the reserved-word table for a mistake nobody has made is not worth it. Binding a name that already
exists on `globalThis` fails rather than shadowing it.

`call_global(name, argument)` is the other direction. It returns `Ok(None)` when no such global
exists, so an optional JavaScript callback is not an error — that is how `JsBridge` can tick a
context that never installed the prelude. A global that exists but is *not callable* is an error:
that is a mistake in the script, not an absent hook.

`eval_named(source, name)` is `eval` with a script name, and it matters more than it looks. V8's
default for an unnamed script is the literal string `undefined`, which is worse than useless in a
stack trace, so even the anonymous case gets a name (`<eval>`). `Error::Exception` carries message,
stack, line, column and resource as *separate* fields rather than pre-formatted, because a REPL wants
the stack on its own lines, an error overlay wants the position to jump to, and a log line wants
neither.

## Why a bare context needs `runtime/prelude.js`

A fresh V8 context is ECMAScript and nothing else. No DOM, no Node, no timers, no console, no
`performance`. A bundle built for a browser assumes several of those exist at *module evaluation
time*, not at call time — so it throws on import, before any of the application's own code runs.

React's scheduler is the load-bearing case: it reads `setTimeout` while its module body is
evaluating. A bundle therefore fails to load even when the application only ever renders through the
synchronous path and never schedules a callback at all.

`crates/spherekit-react/runtime/prelude.js` supplies the minimum:

| Global | Why the bundle reaches for it |
|---|---|
| `setTimeout`, `clearTimeout`, `setInterval`, `clearInterval` | React's scheduler reads them during module evaluation |
| `setImmediate`, `clearImmediate` | Probed by scheduler and polyfill code paths |
| `requestAnimationFrame`, `cancelAnimationFrame` | Probed by React and by animation libraries |
| `queueMicrotask` | The reconciler's `scheduleMicrotask`, when V8 does not already provide it |
| `performance.now`, `performance.timeOrigin` | React's profiling and the reconciler's event timestamps |
| `console.*` | Anything that logs, including React's own warnings |
| `window`, `self`, `navigator` | Environment sniffing in bundled browser code |

Two design points inside it. **Nothing drives itself.** The isolate has no event loop, so timers do
not fire on their own — they accumulate and the host runs the due ones with `__runTimers()` once a
frame. `requestAnimationFrame` lands in the same queue, because a library that probes for it and
finds it missing usually falls back to a 16 ms `setTimeout` anyway, and pointing both at one clock is
simpler than having two. **Console output accumulates** into a buffer that `__drainConsole()` empties
as prefixed lines (`log: …`, `warn: …`), because there is no stdout to write to and an embedder wants
to route the lines wherever its own diagnostics go.

The prelude evaluates to the string `"prelude ready"`, so a host can assert it actually ran.

## The per-frame tick contract

The default V8 microtask policy runs promise continuations whenever script call depth drops to zero,
which puts arbitrary JavaScript in the middle of whatever the host was doing — including a layout
pass. The engine switches the isolate to an **explicit** policy. Queued JavaScript then runs at
exactly two points, `run_microtasks()` and `pump()`, and the host chooses where in its frame they
are.

`JsBridge::tick()` is the composed version:

```rust
engine.run_microtasks()?;  // promise continuations queued since the last tick
engine.pump()?;            // V8's foreground platform tasks
__runTimers();             // the prelude's due timers — React's scheduler lands here
engine.run_microtasks()?;  // continuations the timers just queued
flush();                   // queued native events → __spherekitReceive
```

`pump()` is bounded rather than drained to exhaustion — 1,024 tasks — because it runs on the thread
that draws. A task that reposts itself then costs one frame of latency instead of starving rendering
forever.

**Order matters at the call site.** The flush at the end of `tick` is a backstop for what a method
invoked during this tick queued. An event that must be *visible* to the render this frame produces
belongs before the timers, so call `pump_events()` first — React commits state changes on its
scheduler, which is the prelude's timer queue, and an event delivered after the timers have run waits
a whole frame for its commit. The frame loop in `app/reactdemo` does exactly that:

```rust
js.bridge_mut().pump_events(&self.events);  // widget callbacks → outbound queue
js.pump_events()?;                          // outbound queue → JavaScript
js.tick()?;                                 // microtasks, platform tasks, timers, backstop flush
for line in js.drain_console() { … }
```

## `JsBridge`: the transport with no transport

`spherekit-bridge`'s `v8` feature adds `JsBridge`, which owns an isolate and connects it to an
`ApiBridge` through two globals plus two values:

| Global | Direction | Shape |
|---|---|---|
| `__spherekitSend(frame)` | JS → native | Synchronous. Returns a JSON array of reply frame strings |
| `__spherekitReceive(frame)` | native → JS | Installed by the TypeScript transport; the host calls it |
| `__spherekitProtocol` | — | The protocol version this build speaks |
| `__spherekitVersion` | — | The `spherekit-bridge` crate version |

Because the isolate is in-process and on the same thread, `__spherekitSend` is an ordinary
synchronous call: JavaScript hands over a frame and gets the replies back before the calling
expression finishes. An `invoke()` issued from a React effect can settle inside the same commit that
made it, which no socket-shaped bridge can offer.

The endpoint lives behind `Rc<RefCell<ApiBridge>>` — V8 owns the `__spherekitSend` closure for the
life of the isolate, and that closure must reach the same endpoint the embedder reads the committed
tree from. Two owners, one value, one thread. The discipline that comes with it is the part worth
remembering: **no borrow may be held across a call into the engine.** Every method takes what it
needs out of the cell, drops the borrow, and only then calls into JavaScript, because JavaScript is
entitled to call `__spherekitSend` back from anywhere — a timer, a promise continuation, an event
handler — and a borrow still standing at that moment is a panic in a stack frame that will look
completely unrelated six months later.

`dispatch_frame` never returns `Err`. A frame that does not parse comes back as a `response` carrying
the protocol error, which the TypeScript transport can log and a test can assert on. Throwing instead
would unwind through V8 into a React commit, where the only honest thing left is to tear the isolate
down.

```rust
let mut js = JsBridge::new()?;
js.bridge_mut().register_method("app.quit", move |_host, _params| {
    quit.set(true);
    Ok(serde_json::json!({ "ok": true }))
});
js.install_prelude(include_str!(".../runtime/prelude.js"))?;
js.load(BUNDLE, "app.js")?;
```

`install_prelude` is separate from `load` because the prelude is the environment and the bundle is
the application: a prelude failure means the isolate is unusable, while a bundle failure is an
application bug an embedder may want to show on screen and carry on from.

## Bundling a TypeScript app

The isolate has no module loader, so the application must arrive as one classic script — no `import`,
no `export`, no top-level `await`.

```bash
bun build src/main.tsx --target=browser --format=iife --production --outfile dist/app.js
```

Each flag is load-bearing:

- `--target=browser` — the isolate is closer to a browser than to Node. A Node target emits
  `require`, `process` and `Buffer` references the prelude does not provide.
- `--format=iife` — one self-contained script with no module system. ESM output would need a loader
  that does not exist; CommonJS output would need `require`.
- `--production` — sets `NODE_ENV=production`, which is what selects React's production build.
  Without it the bundle pulls in the development build and its DevTools hooks.

Then evaluate it with the prelude first. `app/reactdemo/build.rs` runs the bundler at build time and
writes the result into `OUT_DIR`, where `include_str!` picks it up — so the compiled binary always
carries its renderer and never depends on a `dist/` directory surviving on disk. Bun is not a build
requirement: without it, that build falls back to a renderer written directly against the wire
protocol, because `cargo build --workspace` should not need a JavaScript toolchain.

`crates/spherekit-bridge/examples/js_roundtrip.rs` is the same idea at its smallest — the whole
pipeline with no bundler and no React, which is the example to run when the React demo misbehaves and
the question is whether the fault is in the bridge or in the bundle.

```bash
cargo run -p spherekit-jsengine --example repl -- crates/spherekit-jsengine/test/welcome.js
cargo run -p spherekit-bridge --features v8 --example js_roundtrip
cargo run -p reactdemo --release
```

## The V8 prebuilt

`build.rs` downloads a prebuilt monolith on first build (≈100 MB) into `crates/spherekit-jsengine/v8backend`,
under a lock file so two concurrent Cargo processes cannot fight over the extraction, and verifies
four files are present before compiling anything. `SPHEREKIT_V8_URL` overrides the source.

The compile defines must match what the prebuilt was built with — `V8_COMPRESS_POINTERS` on, sandbox
off — because `V8::Initialize` checks the embedder configuration at runtime and a mismatch fails
there rather than at link time. `icudtl.dat` is located through a `SPHEREKIT_V8_ICU_DATA_PATH`
environment variable baked in by the build script, so `Engine::new()` needs no path from the caller;
`Engine::with_icu_data_path` exists for an embedder shipping its own.

The static monolith carries dependencies on eleven Windows system libraries, emitted by `build.rs` so
consumers do not have to duplicate V8's platform link contract.

## Porting

Nothing in the Rust or C++ source is Windows-specific: the shim is portable C++20 against the public
V8 API, and the `#[cfg(windows)]` gates exist because the *prebuilt* does not exist elsewhere. What a
second target needs:

1. A V8 monolith for it, built with the same pointer-compression and sandbox configuration, published
   where `build.rs` can fetch it.
2. `build.rs` extended past its `target_os != "windows"` early return, with the right archive name and
   the platform's link libraries in place of the Windows list.
3. The `#[cfg(windows)]` gates in `src/lib.rs` widened to the supported set. The stub `Engine` is a
   complete mirror of the real API for exactly this reason — a host crate is written once and fails
   at runtime on an unsupported target rather than failing to build there.

Until then, `spherekit`'s `v8` feature stays off by default, `spherekit-bridge` compiles without a
JavaScript engine at all, and an application that wants React on another platform drives the same
[API bridge](api-bridge.md) from a WebView or a child process.

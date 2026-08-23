# reactdemo

React 19 running inside SphereKit's own V8 isolate, styled by the shared CSS runtime, rendered on
the GPU. No browser, no WebView, no Node.

```text
cargo run -p reactdemo --release
```

## What it demonstrates

| Claim | Where to look |
|---|---|
| React reconciles against a native host, not a DOM | `renderer/src/App.tsx` — ordinary hooks and JSX |
| The commit crosses as one serialisable tree, not a mutation stream | `src/main.rs`, `tick_javascript` |
| One stylesheet styles both React nodes and native elements | `styles/app.css`, and `ReactDemo::status_bar` |
| Native events route back to React callbacks by node id | `EventQueue` → `JsBridge::pump_events` |
| Native code can raise events React never asked for | the `frame` event in `ReactDemo::draw` |
| JavaScript can call into the application | `app.quit`, registered in `start_javascript` |

## The build

`build.rs` bundles `renderer/src/main.tsx` with Bun into a single IIFE script and writes it to
`OUT_DIR`, where `include_str!` picks it up. The compiled binary therefore carries its renderer and
never depends on a `dist/` directory surviving on disk.

**Bun is not required.** Without it — or before `bun install` has run — the build falls back to a
renderer written directly against the API Bridge wire protocol, with no React and no bundler. It
still runs, still styles itself from the same stylesheet, and is worth reading precisely because it
shows what React's reconciler eventually produces: one `commit` frame.

To get the React renderer:

```text
cd renderer
bun install
cd ..
cargo build -p reactdemo
```

## The stylesheet

`styles/app.css` is installed once into the shared `spherekit-css` runtime. `ReactHost` resolves
every committed React node through it. The status bar at the bottom of the window is a native
`div()` that no React component knows exists, and it is resolved through the very same rules — which
is the point. One cascade, two producers, no browser.

## Platform

The V8 prebuilt is Windows x86_64 only today. On any other target the app still builds and runs, and
renders a native panel explaining that the JavaScript runtime is unavailable. See
`docs/javascript.md` for what porting would involve.

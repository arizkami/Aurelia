# API Bridge

SphereKit's React integration uses a newline-delimited JSON protocol. The
protocol does not choose a transport: the application can connect it to a
WebView callback, an FFI function, a child process, or a socket.

## Message flow

```text
React                         Native
  │── hello ────────────────────>│
  │<─ ready ─────────────────────│
  │── commit(snapshot) ─────────>│
  │── invoke(method, params) ───>│
  │<─ response(id, result/error) ─│
  │<─ event(event, nodeId, data) ─│
```

The commit is a complete React tree and is accepted only when its revision is
newer than the retained native tree. Events are routed by `nodeId`, so native
button and slider interactions can call the original React `onPress` and
`onValueChange` callbacks without serializing functions.

## TypeScript

```ts
const bridge = createApiBridge(transport);
const root = createRoot(bridge);
const status = await bridge.invoke<{ ready: boolean }>("app.status");
const stop = bridge.on("appReady", () => console.log("ready"));
```

`ApiBridgeTransport` only needs `send`, `subscribe`, and optionally `close`.

## Rust

```rust
let mut bridge = ApiBridge::new();
bridge.register_method("app.status", |_host, _params| {
    Ok(serde_json::json!({ "ready": true }))
});
```

`JsonLines` handles partial chunks and enforces a maximum frame size. The
native endpoint validates node identities and rejects malformed or stale
commits before they reach layout and painting.

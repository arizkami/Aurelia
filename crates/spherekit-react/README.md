# spherekit-react

React is the authoring layer; `react-reconciler` produces a native host tree,
and Rust owns the committed tree that will be laid out and painted by
SphereKit. The bridge sends a complete snapshot only after a React commit, so
Rust never observes an incomplete mutation sequence.

```bash
bun install
bun run typecheck
bun test
```

The transport boundary is intentionally small:

```ts
import { Text, View, createRoot, jsonBridge } from "@spherekit/react";

const root = createRoot(jsonBridge((snapshot) => rustHost.commitJson(snapshot)));
root.render(
  <View>
    <Text>Hello</Text>
  </View>,
);
```

For a native runtime that supports request/response APIs and events, use the
transport-neutral bridge:

```ts
import { createApiBridge, createRoot } from "@spherekit/react";

const bridge = createApiBridge(transport);
const root = createRoot(bridge);
bridge.on("appReady", () => console.log("native app ready"));
const tree = await bridge.invoke("spherekit.getTree");
```

The wire protocol is JSON Lines (`hello`, `ready`, `commit`, `invoke`,
`response`, `event`, and `shutdown`). The transport can be a WebView callback,
FFI adapter, child process, or socket; the React package does not assume one.

The authoring syntax intentionally resembles React Native, but the host set is
small: `View`, `Text`, `Button`, `Slider`, `ScrollView`, and `Native`. `on*`
React props remain on the TypeScript side and are routed by native node ID when
the bridge emits `event` messages such as `press` or `valueChange`.

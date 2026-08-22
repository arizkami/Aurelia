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

The authoring syntax intentionally resembles React Native, but the host set is
small: `View`, `Text`, `Button`, `Slider`, `ScrollView`, and `Native`. `on*`
React props remain on the TypeScript side for now; the next native event bridge
can route those callbacks back through the same node IDs.

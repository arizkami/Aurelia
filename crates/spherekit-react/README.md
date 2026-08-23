# @spherekit/react

React is the authoring layer; `react-reconciler` produces a native host tree,
and Rust owns the committed tree that will be laid out and painted by
SphereKit. The bridge sends a complete snapshot only after a React commit, so
Rust never observes an incomplete mutation sequence.

```bash
bun install
bun run typecheck
bun test
```

## How a commit becomes native pixels

1. You render JSX. The components in `src/components.tsx` are thin wrappers
   over `createElement("slider", …)` — there is no virtual DOM element behind
   them, only a host type string the Rust registry knows.
2. `react-reconciler` mutates a private host tree: `createInstance`,
   `appendChild`, `commitUpdate`. Nothing crosses the boundary yet, because a
   half-reconciled tree is not a tree anyone should paint.
3. `resetAfterCommit` runs once, at the end of the commit. It bumps a revision
   counter and serialises the whole tree — ids, host types, props, text — into
   one `NativeTreeSnapshot`.
4. The bridge hands that snapshot to Rust. `on*` props never make the trip:
   they are functions, and they stay in the isolate keyed by node id.
5. Rust turns each node into a `spherekit-ui` widget, resolves `className`,
   `id` and inline `style` through `spherekit-css`, lays the tree out and
   paints it.
6. Input travels the other way as an `event` frame carrying a node id. The
   renderer looks the node up, finds the matching `on*` prop, and calls it —
   which is an ordinary React state update, which produces the next commit.

## Three ways to mount

### `jsonBridge` — one FFI function

The smallest possible host: anything that can be handed a JSON string.

```tsx
import { Text, View, createRoot, jsonBridge } from "@spherekit/react";

const root = createRoot(jsonBridge((snapshot) => rustHost.commitJson(snapshot)));
root.render(
  <View>
    <Text>Hello</Text>
  </View>,
);
```

There is no return channel, so this mount renders but does not receive events.
Reach for it when the UI is a display, or while bringing a new embedder up.

### `createApiBridge` — any transport, with events and API calls

```tsx
import { createApiBridge, createRoot, stylesheet } from "@spherekit/react";

const bridge = createApiBridge(transport);
const root = createRoot(bridge);

await bridge.setStylesheet(stylesheet({ ".panel": { display: "flex", gap: 8 } }));
bridge.on("appReady", () => console.log("native app ready"));
const tree = await bridge.invoke("spherekit.getTree");
```

The wire protocol is JSON Lines (`hello`, `ready`, `commit`, `invoke`,
`response`, `event`, `shutdown`). The transport can be a WebView callback, an
FFI adapter, a child process, or a socket; the package does not assume one. A
failed `invoke` rejects with an `ApiBridgeError` carrying the native error
code, and `close()` rejects everything still in flight rather than leaving
those promises pending forever.

### `createV8Transport` — in-process, inside SphereKit's isolate

```tsx
import { createApiBridge, createRoot, createV8Transport, isV8Host } from "@spherekit/react";

const bridge = isV8Host() ? createApiBridge(createV8Transport()) : createApiBridge(devTransport);
createRoot(bridge).render(<App />);
```

The isolate has no sockets and no event loop, so this transport is
synchronous: `__spherekitSend(frame)` is a native call that returns the frames
the host produced while handling it, and the host pushes events in by calling
`__spherekitReceive(frame)`. An `invoke()` issued in an isolate can settle
before the calling function returns.

Both globals are installed by the Rust embedder. `createV8Transport` throws and
names the missing one if they are not there, because a silently dead transport
inside an isolate with no debugger is not a fun afternoon.

Build the bundle for the isolate with:

```bash
bun build index.tsx --target=browser --format=iife --production --outfile app.js
```

and evaluate `runtime/prelude.js` before it. A bare V8 context has no web or
Node globals at all, and React's scheduler reaches for `setTimeout` while the
module is still evaluating, so the bundle throws on import without it. The
prelude's timers do not run themselves: the host calls `__runTimers()` once a
frame and `__drainConsole()` to collect `console` output.

## Components

`View`, `Text`, `Button`, `Slider`, `Knob`, `Fader`, `ScrollView`, `Toggle`,
`Checkbox`, `Progress`, `Separator`, `Panel`, `TextField`, `Avatar`,
`MenuItem`, and `Native` as the escape hatch for a host type this package has
no wrapper for yet. `NativeComponentType` is the union of the host type strings
the Rust registry knows, exported so the two sides cannot drift apart quietly.

## Styling

`className`, `id`, and inline `style` are serialisable props resolved by the
shared `spherekit-css` runtime, which stays the source of truth for the cascade
and for layout. `css()` and `cx()` help author those props; `toCssText()` and
`stylesheet()` render style objects into the CSS text `setStylesheet()` wants,
using the same unitless-property set the Rust host applies to an inline
`style` — `flex-grow`, `flex-shrink`, `opacity`, `z-index`, `aspect-ratio`,
`font-weight`, and `line-height` keep bare numbers; everything else gets `px`.

## Hooks

`useNativeEvent(bridge, event, handler)` subscribes for the life of a
component. `useInvoke(bridge, method, params)` returns
`{ data, error, loading, refetch }` and compares `params` by its JSON text, so
an object literal at the call site does not re-issue the request on every
render. `useStylesheet(bridge, css)` installs a stylesheet on mount and returns
the number of rules the host accepted — 0 means the native parser rejected it.

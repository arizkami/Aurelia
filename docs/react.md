# React

React 19 renders SphereKit's native widgets. There is no DOM, no WebView and no HTML anywhere in
the path — the reconciler's host is a small Rust tree, and what it produces is the same
`spherekit-ui` element graph a native application builds by hand.

## The shape of a frame

```text
  JSX
   │  react-reconciler
   ▼
  host tree (TypeScript)          createInstance / appendChild / commitUpdate …
   │  resetAfterCommit
   ▼
  NativeTreeSnapshot  ──JSON──>  ReactHost::commit        validate, retain, index
                                      │
                                      ▼
                                 ui_element_with_events   cascade + lower
                                      │
                                      ▼
                                 UiTree → layout → paint
                                      │
  React callback  <──JSON──  EventQueue::drain            native events, by node id
```

Every arrow on the native side is synchronous and single-threaded. Nothing in `spherekit-react` is
`Send`, and nothing needs to be: a React root, its elements and its event queue all belong to the
UI thread.

## Why a whole-tree snapshot and not a mutation stream

React's reconciler is a mutation host by nature. It calls `createInstance`, `appendChild`,
`insertBefore`, `commitUpdate` and `removeChild` one at a time, and a host that forwarded each of
those over the boundary would be sending the cheapest possible messages. It would also be wrong for
this engine, for three reasons.

**The native side must never observe a half-built tree.** The mutations that make up one commit are
not individually meaningful: between `removeChild` and `insertBefore` the tree is in a state React
never intends to display. A native host that laid out after each one would lay out states that do
not exist. Deferring the handover to `resetAfterCommit` — the one point React guarantees the tree is
whole — removes the whole class of problem instead of documenting a rule about it.

**A snapshot makes the boundary testable.** A commit is one serialisable value, so the entire
TypeScript half can be tested with no Rust in the process (`bun test` asserts on the JSON), and the
entire Rust half can be tested with no JavaScript in the process (`ReactHost::commit_json` takes a
string literal). A mutation stream would only be testable as a sequence, against a mock of the other
side, and the two mocks would be free to drift from the real implementations.

**A snapshot is transport-agnostic.** The same bytes go over an FFI call, a WebView `postMessage`, a
child-process pipe or an in-process V8 host function. Ordering, replay and partial delivery are
somebody else's problem in a mutation stream; here a frame either arrives whole or does not arrive.

The cost is real and was accepted: a commit re-serialises every node, not just the changed ones. It
is bounded by `MAX_NODE_COUNT` (100,000) and it buys a native side that never has to reason about
partial state. If a profile ever shows the serialisation mattering, the fix is a cheaper encoding of
the same whole-tree value, not a mutation stream.

## The reconciler host config

`createRoot(bridge)` in `src/renderer.ts` builds a `react-reconciler` host config over three private
types: `HostInstance` (id, type, props, children, hidden), `HostTextInstance` (id, text, hidden) and
`HostContainer` (the bridge, the root children, a revision counter). Ids are allocated by the
renderer and are the identity the event path uses later.

Three parts of it are worth knowing about:

- **Update priority.** The reconciler asks the host to resolve a lane for every update and takes
  lane `0` literally: an update tagged with it is queued against no lane and never scheduled. A host
  answering `0` unconditionally silently drops every state update that does not originate inside a
  synchronous `render()` — effects, promise callbacks and native events all do nothing. The host
  therefore falls back to `DefaultEventPriority` (lane 32), which is what React's own DOM host uses
  for an update with no ambient event priority.
- **`shouldSetTextContent` is always `false`.** Text always becomes a real `#text` node, so a node's
  text is in one place regardless of whether it had element siblings.
- **`isPrimaryRenderer` is `false`.** SphereKit is a secondary renderer; it does not claim React's
  shared internal state.

`resetAfterCommit` increments the revision and calls `bridge.commit(snapshot)`. That is the only
place a snapshot is produced.

## What crosses

```ts
interface NativeNodeSnapshot {
  readonly id: number;
  readonly type: string;
  readonly props: Readonly<Record<string, NativeValue>>;
  readonly text?: string;
  readonly hidden?: boolean;
  readonly children: readonly NativeNodeSnapshot[];
}

interface NativeTreeSnapshot {
  readonly revision: number;
  readonly children: readonly NativeNodeSnapshot[];
}
```

`serializeProps` decides what survives the crossing:

| Prop | Fate | Why |
|---|---|---|
| `children` | Dropped | It is the `children` array, not a value |
| `on*` | Dropped | A function cannot be serialised; it stays in the renderer's instance map |
| `NaN`, `Infinity` | Dropped | JSON cannot carry them, so emitting them would produce a frame the transport refuses |
| Nested objects and arrays | Kept, recursively — unless any leaf is unserialisable, in which case the whole value is dropped | A half-serialised `style` object is worse than an absent one |

`hidden` is React's own visibility flag, set by `hideInstance` during a Suspense transition. It is
carried rather than resolved on the TypeScript side because the native lowering knows what hiding
means for each widget.

## The Rust host

`ReactHost::commit` validates, retains and indexes in **one** walk, because all three need the same
traversal and all three must fail the whole commit together. A rejected commit leaves the previous
tree on screen and no half-built index behind.

What it rejects:

| Failure | Reason |
|---|---|
| Stale revision | A commit older than the retained tree would paint a state React has moved past |
| Duplicate node id | Two nodes with one identity make the event path ambiguous |
| Missing `type` | There is nothing to build |
| `text` on a non-text node, or a text node with no text | The two carriers of text would otherwise disagree |
| Depth past `MAX_TREE_DEPTH` (128) | Validation, lowering and `Drop` all recurse; a stack overflow cannot be caught and turned into a rejected commit. 128 is `serde_json`'s own nesting limit, so the two entry points agree on what is acceptable |
| More than `MAX_NODE_COUNT` (100,000) nodes | A tree that size is a bug or an attack, and retaining one costs a lowering pass and a layout pass that never visibly finish |

Lowering (`ui_element_with_events`) then walks the retained tree once and does three things at the
same time, entangled on purpose: it resolves the `spherekit-css` cascade, it threads the ancestor
chain that descendant and child combinators need, and it inherits typography down the tree. Separate
passes would build the same ancestor chain three times.

A native `div()` built by hand and a React `<View>` resolve through the same `Stylesheet` and land
on the same `spherekit_ui::PaintStyle`. That is the whole point of sharing the CSS runtime rather
than giving React its own style path — see [`docs/spherekit-css.md`](spherekit-css.md).

## Host components

`className`, `id` and inline `style` are accepted on every one of these; `className` and `id` are
what selectors match, and `style` is an inline declaration block. The React node's numeric id is
separate, and is what becomes the element's `ElementId` and what routes events.

| Component | Host type | Props | Event |
|---|---|---|---|
| `View` | `view` | — | — |
| `Text` | `text` | — | — |
| `Button` | `button` | `title`, `disabled` | `onPress()` |
| `Slider` | `slider` | `value`, `minimumValue`, `maximumValue`, `step`, `disabled` | `onValueChange(number)` |
| `Knob` | `knob` | as `Slider` | `onValueChange(number)` |
| `Fader` | `fader` | as `Slider` | `onValueChange(number)` |
| `Toggle` | `toggle` | `checked`, `label`, `disabled` | `onChange(boolean)` |
| `Checkbox` | `checkbox` | `checked`, `label`, `disabled` | `onChange(boolean)` |
| `Progress` | `progress` | `value`, `indeterminate`, `thickness` | — |
| `Separator` | `separator` | `vertical` | — |
| `Panel` | `panel` | `title` | — |
| `ScrollView` | `scroll-view` | `horizontal`, `both` | — (see below) |
| `TextField` | `text-field` | `value`, `placeholder`, `disabled`, `mask` | `onChange(string)`, `onSubmit(string)` |
| `Avatar` | `avatar` | `name`, `initials`, `size`, `presence` | — |
| `MenuItem` | `menu-item` | `label`, `shortcut`, `danger`, `disabled` | `onSelect()` |
| `Native` | anything registered | `type` plus whatever the builder reads | whatever the builder pushes |

`view` is deliberately **not** in the Rust match arm: it falls to the same default a type the host
has never heard of falls to, a plain styled container with its children inside it. An unregistered
type is not an error, so a bundle built against a newer host degrades to a box instead of taking the
window down.

Four widgets — `Toggle`, `Checkbox`, `MenuItem` and `Avatar` — compute their whole layout style
from their own state and do not implement `Styled`, so there is nowhere to hang a CSS box on them.
Lowering wraps each in a styled `div()`. It costs one extra element and keeps `.toggle { margin: 8px }`
working; silently dropping the style would make that rule do nothing and say nothing.

`Text` collapses its whole subtree into one string rather than laying out its `#text` children: a
label paints its own glyphs, and a child element beside them would be positioned by flexbox instead
of by the shaper. `Button` and `MenuItem` use `title`/`label` as both a serialised prop and the
fallback child, so a host that paints its own label and one that lays out children show the same
text without the caller writing it twice.

> **`ScrollView` has no `onScroll`, deliberately.** Nothing in `scroll_element` wires a scroll
> callback, and `spherekit-ui`'s `ScrollView` does not implement `Interactive`, so there is no
> builder to wire one to and no offset for it to report. A prop declared ahead of that path is worse
> than none: it type checks, reads as supported, and never fires. It goes in when the native side
> can report an offset.

## Events, back the other way

Native widget callbacks push a `HostEvent { event, node_id, payload }` into the shared `EventQueue`.
`ApiBridge::pump_events` turns the queue into `event` frames, and the TypeScript root's `dispatch`
looks the node id up in its instance map and calls the matching prop.

The names on the wire are the renderer's, not React's:

| Wire event | Payload | React prop | Argument |
|---|---|---|---|
| `press` | none | `onPress` | none |
| `select` | none | `onSelect` | none |
| `valueChange` | `{ value }` | `onValueChange` | `value` |
| `change` | `{ checked }` or `{ text }` | `onChange` | `checked` / `text` |
| `submit` | `{ text }` | `onSubmit` | `text` |

Two rules make that table work. Payloads are always JSON objects on the wire, because the frame
schema has nowhere else to put a value, but a handler wants the thing that changed — a toggle's
`onChange` takes a boolean, not `{checked}` — so a small table names the key to unwrap per event. An
event absent from that table receives the whole payload, which is the right default for anything
carrying more than one field. And `press` and `select` carry nothing, so they are called with *no*
argument at all; passing the raw payload anyway would make `onPress={setOpen}` — a setter that
happens to accept one argument — receive an object nobody meant to send.

Sending `onPress` from the native side instead would put React's naming convention inside the Rust
host, where a second frontend would then have to reproduce it.

## Mounting

### 1. `jsonBridge` — an FFI or IPC host with a commit function

The minimum. Anything that can be handed a JSON string is a bridge:

```ts
import { createRoot, jsonBridge } from "@spherekit/react";

const root = createRoot(jsonBridge((snapshot) => hostCommit(snapshot)));
root.render(<App />);
```

No events come back, because a bare commit function has nowhere to deliver them. This is the shape
to use for a smoke test, or for a host whose only job is to draw.

### 2. `createApiBridge` — the full protocol over any transport

```ts
import { createApiBridge, createRoot } from "@spherekit/react";

const bridge = createApiBridge({
  send: (frame) => webview.postMessage(frame),
  subscribe: (listener) => webview.onMessage(listener),
  close: () => webview.dispose(),
});

const root = createRoot(bridge);
await bridge.setStylesheet(`.panel { display: flex; gap: 8px; }`);
const status = await bridge.invoke<{ ready: boolean }>("app.status");
const stop = bridge.on("appReady", () => console.log("ready"));
root.render(<App bridge={bridge} />);
```

`ApiBridgeTransport` needs `send`, `subscribe` and optionally `close` — nothing about WebViews,
sockets or pipes reaches component code. The bridge sends `hello` as soon as it is created and
resolves `invoke()` promises against the `response` frames that come back by request id. See
[`docs/api-bridge.md`](api-bridge.md).

### 3. `createV8Transport` — in process, no transport at all

```ts
import { createApiBridge, createRoot, createV8Transport, isV8Host } from "@spherekit/react";

const bridge = isV8Host() ? createApiBridge(createV8Transport()) : jsonBridge(print);
createRoot(bridge).render(<App />);
```

The isolate has no sockets, no message ports and no event loop of its own, so the usual asynchronous
transport shape does not apply. `__spherekitSend` is a synchronous native call that returns the
frames the host produced while handling the request, and those replies are fed straight back to the
subscriber — which is why an `invoke()` issued from an isolate can settle before the calling
function returns. See [`docs/javascript.md`](javascript.md).

`isV8Host()` tests for the global rather than for a build flag, so one bundle can run in the
isolate, in a browser and under `bun test`.

## Hooks

Three, and each exists because the naive version has a specific bug.

`useNativeEvent(bridge, event, handler)` holds the handler in a ref instead of listing it as an
effect dependency. A handler written inline is a new function every render, and depending on it
would tear down and re-register the native subscription on each one, losing anything the host
emitted in between.

`useInvoke(bridge, method, params)` compares `params` by its JSON text, not by identity. An object
literal at the call site is a fresh object every render, so an identity comparison re-issues the
request forever; the JSON text is also exactly what crosses the transport, so two params that
serialise the same really are the same request. A change of method or params clears `data` — the old
result answers a question nobody is asking any more — while an explicit `refetch()` keeps it, so a
list refreshing in place does not blank out first.

`useStylesheet(bridge, css)` returns the rule count the host accepted. A stylesheet the parser
rejected comes back as `0` and is logged, rather than thrown: a malformed rule should leave the
application unstyled, not unmounted.

## Styling from TypeScript

`stylesheet({...})` builds CSS text from selector-keyed objects, and `toCssText({...})` builds one
declaration block. Both normalise `camelCase` and `snake_case` property names to their CSS spelling
and append `px` to bare numbers — except for the ratio properties (`flex-grow`, `flex-shrink`,
`opacity`, `z-index`, `aspect-ratio`, `font-weight`, `line-height`), where a bare number means a
number.

That unitless list is duplicated from the Rust host on purpose, and it is the one place the two
sides can silently disagree: a property missing from it turns `opacity: 0.5` into `opacity: 0.5px`,
which the CSS parser drops without error, so the element renders opaque and nothing says why.
Because the mismatch is invisible at runtime, both copies are spelled out in full by a test — the
list in `src/lower.rs` by `the_unitless_list_is_the_one_the_typescript_side_pins`, the one in
`src/css.ts` by "pins the same unitless list the Rust host does" — so changing one alone fails that
side's suite rather than shipping. It is also the reason to author a stylesheet as an object rather
than a template literal: a rule written as a string bypasses the mapping entirely and has to spell
out every unit by hand.

Both sides drop a value they cannot carry rather than emitting it. A non-finite number could not
have crossed JSON in the first place; a string containing `;` is the sharper case, because it would
split one declaration into two and leave an offcut with no colon, which invalidates the *whole*
block — so a `<MenuItem label="Copy; Paste">` would take the node's `style` prop down with it.
Booleans are dropped for the dull version of the same reason: no property the cascade resolves
accepts `true`, so `disabled` and `checked` reach it as selector state instead of as declarations.

`cx(...)` joins conditional class names. `css({...})` returns its argument unchanged and exists
only to give an inline style object a type.

## Extending the host

An application registers its own node types on the Rust side:

```rust
use spherekit_react::{ReactHost, node_builder};
use spherekit_ui::{IntoElement, ParentElement, div, label};

host.register_host_type(
    "gauge",
    node_builder(|cx, node| {
        cx.apply(div().id(node.id)).child(label("gauge")).into_element()
    }),
);
```

and reaches it from TypeScript through `<Native type="gauge" … />`, whose `type` is widened past
`NativeComponentType` for exactly this reason.

The `LowerContext` handed to a builder carries everything about *where* the node sits — the resolved
interactive style, the inherited typography, the ancestor chain, the event queue — so a builder only
has to read the node's own props. `cx.apply(element)` puts the cascade on it, `cx.apply_label(label)`
adds inherited typography, `cx.wrap(widget)` boxes a widget that cannot carry a style, and
`cx.children(node)` lowers the children with this node pushed onto the ancestor chain.

`NativeNode::prop(name)` falls back to a same-named key inside the `style` object, because the React
side has two idioms for the same intent and a host honouring only one of them would make the other
silently do nothing.

## Testing

The TypeScript half is tested with `bun test` in `crates/spherekit-react/test`, against the JSON a
commit produces. The Rust half is tested with `cargo test -p spherekit-react`, against string
literals. Neither half needs the other in the process, which is the property the snapshot boundary
was chosen for.

```bash
cd crates/spherekit-react && bun install && bun test && bun run typecheck
cargo test -p spherekit-react
```

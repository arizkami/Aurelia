# Layout

The retained tree, and the invalidation rules that make rebuilding it every frame affordable.

## Declarative on the outside, retained on the inside

Application code writes what looks like immediate-mode UI:

```rust
div()
    .flex_col()
    .gap(px(8.0))
    .child(label("Threshold"))
    .child(knob(self.threshold.get()).range(-60.0, 0.0))
```

What happens is retained. `UiTree::build` flattens that into an arena and reconciles it against
last frame's layout nodes by [`ElementId`]. An unchanged tree reuses every node, sets every style
to the value it already had — which `set_style` recognises as a no-op — and therefore marks nothing
layout-dirty. `LayoutEngine::compute` then returns immediately.

This is measured, not asserted. The demo reports `nodes created: 0`, `nodes reused: 39`,
`nodes laid out: 0` after 180 frames.

## Identity

An element with an explicit id keeps its node across rebuilds. An element without one gets an
identity derived from its parent's id and its index.

That derivation is exactly right for static structure and exactly wrong for a reorderable list:
moving item 3 to position 0 would hand item 3's node — and its scroll offset, its focus, its drag
state — to whatever is now at index 0. **Keyed lists must call `.id()`.** There is a test asserting
that a keyed element keeps its node when reordered and that no nodes are created.

`ElementId::from_key` hashes a key and sets the high bit; `ElementId::unique` mints from a counter
and does not. They cannot collide, and `ElementId::child` combines a parent with a key so the same
key under two different parents stays distinct.

## Dirty flags

```rust
bitflags! {
    pub struct DirtyFlags: u32 {
        const STYLE     = 1 << 0;
        const LAYOUT    = 1 << 1;
        const PAINT     = 1 << 2;
        const TEXT      = 1 << 3;
        const CHILDREN  = 1 << 4;
        const TRANSFORM = 1 << 5;
    }
}
```

The propagation rules are the whole design:

| Flag | Propagates | Because |
|---|---|---|
| `LAYOUT` | To ancestors | A child's size can change its parent's |
| `CHILDREN` | To ancestors | Structure change is a layout change |
| `PAINT` | **Nowhere** | A colour change moves nothing |
| `TRANSFORM` | Nowhere | Handled per-instance on the GPU |

`PAINT` not propagating is the load-bearing rule. A VU meter repainting at the display refresh rate
marks itself `PAINT`; no ancestor is touched, no sibling is touched, and `compute` does nothing.

The two invalidation entry points are deliberately different methods so the choice is explicit:

```rust
cx.notify();         // PAINT.  Repaint this node. No layout, no reshaping.
cx.notify_layout();  // LAYOUT. Relayout the ancestor chain and this subtree.
```

A test asserts that `notify()` never sets the relayout flag — a repaint escalating to a relayout
would silently undo the entire design.

## Measuring it, not claiming it

`LayoutStats::nodes_laid_out` exists so that "repainting a meter does no layout work" is an
assertion in a test rather than a claim in a comment.

It counts layout-algorithm *invocations*, not distinct nodes: a flex container may size a child
twice under different constraints, and hiding that would make the number reassuring rather than
useful.

Tests that assert on it:

- `sphere-layout` — marking a node `PAINT`-dirty leaves it at zero.
- `sphere-ui` — an identical rebuild leaves it at zero; hovering leaves it at zero; a theme change
  leaves it at zero.
- `sphere-ui::widgets` — a 60-frame knob drag leaves it at zero on every frame.
- `sphere-audio-ui` — 60 frames of meter repaint leave it at zero on every frame.

## The engine seam

```rust
pub trait LayoutEngine {
    fn compute(&mut self, tree: &mut LayoutTree, viewport: Size<Px>) -> Result<(), LayoutError>;
    fn compute_with_measure(
        &mut self,
        tree: &mut LayoutTree,
        viewport: Size<Px>,
        measure: &mut dyn Measure,
    ) -> Result<(), LayoutError>;
    fn stats(&self) -> LayoutStats;
    fn invalidate(&mut self);
}
```

`TaffyLayoutEngine` is the shipped implementation. **No `taffy` type appears anywhere in this
crate's public API.** Sphere has its own `Style`, its own `AvailableSpace`, its own `Overflow`,
`Align` and `Distribute`. The mapping happens inside `taffy_backend.rs`.

That is not purity for its own sake. A measure callback lives in application code — it is where
text shaping happens — and application code must not have to name a third-party type to write one.

## Where text enters layout

`compute_with_measure` calls back for every childless node:

```rust
pub struct MeasureRequest<'a> {
    pub node: NodeId,
    pub style: &'a Style,
    pub known: Size<Option<Px>>,          // axes already decided
    pub available: Size<AvailableSpace>,  // Definite | MinContent | MaxContent
}
```

`known.width == Some(w)` means "how tall are you at this width" — the wrapping question.
`MaxContent` means "how wide on one line". `MinContent` means "your longest unbreakable run".
`Label::measure` maps those three onto a `max_width` and asks the text system, which is backed by
the shaping cache, so the second call for the same string is a lookup rather than a reshape.

The callback fires for *every* leaf, not only text, because the layout crate cannot know which
leaves have content. A plain box returns `None`.

## Geometry

```rust
pub struct ComputedLayout {
    pub bounds: Rect<Px>,           // relative to the parent's border box
    pub absolute_bounds: Rect<Px>,  // viewport coordinates, scroll already applied
    pub content_size: Size<Px>,     // exceeds bounds when content overflows
    pub border: Edges<Px>,
    pub padding: Edges<Px>,
    pub margin: Edges<Px>,
    pub order: u32,
}
```

Both relative and absolute are stored. Painting and hit testing both want the absolute form, and
recomputing it per query would mean walking to the root on every mouse move.

Layout output is **not rounded**. Logical pixels are not device pixels; the renderer rounds once,
in device space, at exactly one place. Rounding here would compound at every nesting level.

## Where this deliberately differs from CSS

- **No `position: static`.** Every node is a containing block for its absolutely positioned
  children, so "the nearest positioned ancestor" is always the direct parent. Inserting a wrapper
  cannot teleport a popup.
- **`z_index` orders a node among its siblings and nothing else.** There are no stacking contexts
  to reason about.
- **Sizes are always border-box.** `box-sizing` as a per-node choice is a source of confusion with
  no upside in an engine that owns its own styling.

## Hit testing

```rust
tree.hit_test(point)                     // topmost node
tree.hit_test_all(point)                 // the full chain, root to target
tree.hit_test_all_into(point, &mut buf)  // the same, allocation-free
```

The `_into` variant exists because hit testing runs on every mouse move, and allocating there would
be a per-frame allocation in the most frequent code path in the engine.

Ordering honours `z_index` on top of the algorithm's own paint order. Clipping is respected: a
child scrolled outside its container's clip is not hit, and there is a test for it.

The chain is what makes capture-and-bubble dispatch possible; see `sphere-ui`'s event module.

## Scrolling

A scroll offset shifts a node's children's absolute bounds and marks `PAINT`. It does **not** mark
`LAYOUT`. A scroll that relaid out its contents would make a long list unusable, and a DAW timeline
impossible.

`max_scroll` derives from `content_size` minus `client_size`, so the clamp is always correct
without a separate measurement pass.

## Scale

The tree is iterative everywhere its depth is user-controlled. `sphere-ui`'s build and paint walks
use an explicit stack and are tested at 5,000 levels of nesting. Layout recursion depth is bounded
by the backing engine rather than by Sphere; 200 levels is tested and is an order of magnitude past
any real interface — a deeply nested mixer strip is nearer twenty.

The other axis is width: 500 sibling nodes is tested, and is what a large session looks like.

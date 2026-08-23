# Building a native UI

## The shape of it

```rust
use spherekit::ui::{div, label, IntoElement, ParentElement, Styled};
use spherekit::core::px;

div()
    .flex_col()
    .gap(px(8.0))
    .p(px(12.0))
    .bg(theme.colors.surface)
    .rounded(theme.radii.lg)
    .child(label("Threshold").text_size(theme.typography.sm))
    .child(
        div()
            .id("threshold")
            .focusable()
            .size(px(64.0))
            .on_mouse_down(|cx| cx.capture())
            .on_mouse_move(|cx| { /* drag */ cx.notify(); }),
    )
```

The code reads immediate; the machinery is retained. `UiTree::build` reconciles
the produced elements against last frame's layout nodes **by identity**, so an
unchanged tree reuses every node, marks nothing layout-dirty, and costs zero
layout work. There is a test asserting exactly that.

## The invalidation split — the single most important idea

| Call | Marks | Cost |
|---|---|---|
| `EventContext::notify` | `PAINT` | Repaint this node. No layout, no text reshaping. |
| `EventContext::notify_layout` | `LAYOUT` | Relayout the ancestor chain and this subtree. |

A meter, a knob drag, a hover highlight and a theme change all take the first
path. Only a structural or size change takes the second. `Style::diff`
classifies every property, which is why `Style` is `Clone + PartialEq`.

This is also why `spherekit-layout::Style` (where) and `spherekit-ui::PaintStyle`
(what it looks like) are separate structs. Changing a colour touches one and
marks `PAINT`; changing a width touches the other and marks `LAYOUT`.

## The frame

```rust
tree.build(view.render());                 // reconcile
tree.compute_layout(viewport)?;            // skipped entirely if clean
tree.paint(&mut canvas, viewport, time);   // walk once, cull, record
tree.end_frame();
```

and on input:

```rust
let result = tree.dispatch(&event);
if result.relayout { /* schedule layout */ }
if result.repaint  { window.request_redraw(); }
```

`SphereKitSurface::render(root, clear)` does all of that for one window.

## Traits

- `Styled` — `style_mut`/`paint_style_mut` plus the fluent helpers: `flex_col`,
  `gap`, `w`/`h`/`size`, `p`/`px_`/`py_`, `m`, `items_center`, `justify_center`,
  `grow`, `flex_1`, `bg`, `rounded`, `border`, `shadow`, `opacity`, `blur`,
  `clip`, `cursor`, `absolute`, `z`, `hidden`, `full`.
- `StyledInteraction` — `hover_bg`, `active_bg`, `focus_ring`. Held on the
  element so hover feedback costs a paint and never a rebuild.
- `Interactive` — `on_click`, `on_mouse_down/up/move/enter/leave`, `on_scroll`,
  `on_key`, `on_text_input`, `on_focus`.
- `ParentElement` — `child`, `children_iter`.
- `IntoElement` — `into_element()` to `AnyElement`.

## Widgets

`button` `label` `slider` `knob` `fader` `toggle` `checkbox` `progress`
`progress_indeterminate` `separator` `panel` `scroll_view` `scroll_area`
`avatar` `dropdown` `menu_item` `context_menu` `text_field`

Not every widget implements `Styled`. `Toggle`, `MenuItem` and `Avatar` compute
their whole layout from their own state, so there is nowhere to put a CSS box on
them — wrap them in a `div()` if they need one. `spherekit-react`'s
`LowerContext::wrap` exists for exactly this.

The value controls (`slider`, `knob`, `fader`) are complete: vertical drag with
pointer capture, Shift for fine mode, double-click to reset, arrow/Page/Home/End
keys, scroll wheel, step quantisation, and a caller-supplied value string so a
fader announces "−6.0 dB" rather than "79 %".

## Theme

`Theme::dark()` / `Theme::light()`, with `colors`, `typography`, `radii`,
`spacing`, `shadows`. An application owns its own mapping rather than mutating a
global — see `spherekit_dark_theme` in `app/uigallery/src/main.rs`, which is the
palette other applications in this repo match.

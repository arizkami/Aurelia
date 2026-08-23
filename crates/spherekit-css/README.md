# spherekit-css

`spherekit-css` is the shared CSS runtime for SphereKit native and React apps.
It uses Servo's `cssparser` for value tokenisation and owns the selector engine,
the cascade and the property model, resolving them into
`spherekit-layout::Style`, a text block and paint values.

```rust
use spherekit_css::{Node, Stylesheet};

let sheet = Stylesheet::parse(".panel { display: flex; gap: 8px; }").unwrap();
let style = sheet.resolve_node(Node::new("view").with_classes("panel"));
let native_view = style.apply_to(spherekit_ui::div());
```

Anything past a simple selector — combinators, `:nth-child()`, media queries,
font-relative units — resolves through a `MatchPath` and a `StyleContext`:

```rust
use spherekit_css::{ElementState, MatchPath, Node, StyleContext, Stylesheet};

let sheet = Stylesheet::parse(
    ":root { --accent: #ff8800 }
     .rack > button:hover { background: var(--accent) }",
)
.unwrap();

let ancestors = [Node::new("view").with_classes("rack")];
let hovered = ElementState { hover: true, ..ElementState::default() };
let path = MatchPath::with_ancestors(&ancestors, Node::new("button").with_state(hovered));
let style = sheet.resolve_in(&path, None, &StyleContext::default());
```

The React host uses the same resolver. Install a stylesheet through the API
Bridge and use `className` or normal inline `style` props:

```ts
await bridge.setStylesheet(`
  .panel { display: flex; flex-direction: column; gap: 8px; }
`);
root.render(<View className="panel" style={{ padding: 12 }} />);
```

Supported: compound selectors with the descendant/`>`/`+`/`~` combinators,
specificity and `!important`, the interaction and structural pseudo-classes,
`:not()`, `@media`, custom properties and `var()`, the full colour and length
vocabularies, and the layout, paint and text properties SphereKit renders.

Ignored on purpose — never reinterpreted: animations and transitions, gradients,
grid templates, pseudo-elements, attribute selectors, and inheritance beyond
text properties and custom properties. See `docs/spherekit-css.md` for why each
one is out.

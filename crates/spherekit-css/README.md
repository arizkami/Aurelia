# spherekit-css

`spherekit-css` is the shared CSS runtime for SphereKit native and React apps.
It uses Servo's `cssparser` for tokenisation and typed values, then resolves a
focused native property model into `spherekit-layout::Style` and paint values.

```rust
use spherekit_css::{Node, Stylesheet};

let sheet = Stylesheet::parse(".panel { display: flex; gap: 8px; }").unwrap();
let style = sheet.resolve_node(Node::new("view").with_classes("panel"));
let native_view = style.apply_to(spherekit_ui::div());
```

The React host uses the same resolver. Install a stylesheet through the API
Bridge and use `className` or normal inline `style` props:

```ts
await bridge.setStylesheet(`
  .panel { display: flex; flex-direction: column; gap: 8px; }
`);
root.render(<View className="panel" style={{ padding: 12 }} />);
```

The v1 runtime supports simple/compound selectors, specificity, `!important`,
Flexbox/Grid/spacing/sizing/position/overflow properties, and the paint values
SphereKit already renders. Browser-only features such as media queries,
pseudo-classes, animation, and full DOM inheritance are intentionally ignored.

# spherekit-css

## Research decision

SphereKit already uses Taffy for DOM-free Flexbox/Grid layout. Taffy accepts a
typed style object and computes layout, but it is not a stylesheet parser. The
Servo `cssparser` crate is a good fit for the missing syntax layer: it
tokenises CSS and parses generic values while intentionally leaving selectors,
properties and cascade policy to the embedding engine.

We did not embed Stylo because it is a browser-grade style system with a much
larger DOM/selector/cascade integration surface than a native audio UI needs.
We also did not use Lightning CSS as the runtime: it is primarily a parser,
transformer, bundler and minifier for build-time CSS.

## Shared runtime

`Stylesheet::parse` and `Stylesheet::resolve` live in `spherekit-css`. The
result is a `ResolvedStyle` containing SphereKit's layout `Style` plus paint
properties. Native code can apply it directly to any `spherekit_ui::Styled`
element. `spherekit-react::ReactHost` stores the same stylesheet and resolves
`className`, `id`, and inline `style` props during native lowering.

The API Bridge exposes `spherekit.setStylesheet`, and the TypeScript bridge
provides `bridge.setStylesheet(css)`, so a React app and native elements can
share one stylesheet without shipping a browser runtime.

## Deliberate v1 boundary

Supported selectors are `*`, element, `.class`, `#id`, and compound forms such
as `view.panel.primary`. Supported values cover the native layout vocabulary:
display, position, sizing, margin/padding/border, flex direction/wrap/grow,
alignment, gap, inset, overflow, aspect ratio, opacity and z-index, plus
background colour, border colour/width and border radius.

Media queries, pseudo-classes, pseudo-elements, animations, CSS variables,
fonts and DOM inheritance are not represented by the current native model. They
are ignored rather than partially emulated.

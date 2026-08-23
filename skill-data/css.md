# The CSS runtime

`spherekit-css` is a native-first CSS engine. It is not a browser engine and is
not trying to become one.

## The one rule everything else follows

**Syntax the engine does not model is ignored, never reinterpreted.**
`width: 12` does not become `12px`. `border-radius: 8px / 4px` does not become a
circular radius. An unknown media feature makes its query false rather than
true. A stylesheet that does nothing is debuggable; a stylesheet that does
something slightly different from what it says is not.

Hold to this when adding properties.

## Resolving

```rust
use spherekit::css::{MatchPath, Node, StyleContext, Stylesheet};

let sheet = Stylesheet::parse(".panel { display: flex; gap: 8px; }")?;

// Flat, no ancestors:
let style = sheet.resolve(Node::new("view").with_classes("panel"), None);

// With ancestors, so combinators and :nth-child work:
let ancestors = [Node::new("view").with_classes("rack")];
let path = MatchPath::with_ancestors(&ancestors, Node::new("button"));
let style = sheet.resolve_in(&path, inline_css, &StyleContext::default());

// Base plus the hover/active/focus variants, for a widget that owns all of them:
let styles = sheet.resolve_interactive(&path, None, &context);
let element = styles.apply_to(div());
```

`ResolvedStyle` carries `layout` (a `spherekit_layout::Style`), `text`
(a `TextProperties`), `background`, `border_color`, `border_width`,
`corner_radius`, `corner_radii`, `paint_opacity`, `clip_content`, `shadows`,
`cursor`. `apply_to` puts all of it on any `Styled`.

`StyleContext` is what `rem`, `em`, `vw`, `vh`, `vmin`, `vmax` and the
width/height/`prefers-color-scheme` media features resolve against. Changing the
viewport means changing the context.

## Supported

- Selectors: `*`, element, `.class`, `#id`, compounds, and the descendant, `>`,
  `+` and `~` combinators.
- Pseudo-classes: `:hover` `:active` `:focus` `:disabled` `:checked`
  `:first-child` `:last-child` `:nth-child(an+b)` `:not()` `:root`.
- Custom properties and `var()` with fallbacks, including cycle detection.
- `@media` with `min-width`/`max-width`/`min-height`/`max-height`/
  `prefers-color-scheme`, `and`, and comma lists. Rules inside a false query are
  parsed and kept; they simply do not match.
- Layout: display, position, inset, sizing, margin/padding/border, flex
  direction/wrap/grow/shrink/basis and the `flex` shorthand, alignment, gap,
  overflow, aspect-ratio, opacity, z-index.
- Paint: background colour, border, per-corner `border-radius`, `box-shadow`,
  `cursor`.
- Text: `color`, `font-size`, `font-family`, `font-weight`, `font-style`,
  `line-height`, `letter-spacing`, `text-align`, `white-space: nowrap`,
  `text-overflow: ellipsis`.
- Colour: the full named table, `#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa`, `rgb()`,
  `rgba()`, `hsl()`, `hsla()` in both legacy-comma and modern-space syntax.
- Units: `px`, `%`, `rem`, `em`, `vw`, `vh`, `vmin`, `vmax`, `pt`, `calc()`.

## Deliberately absent

Gradients (a `Gradient` needs the element's final box, and `ResolvedStyle` is
size-independent by design), `currentcolor`, `inherit`/`initial`/`unset`,
elliptical radii, `:has()`/`:is()`/`:where()`, attribute selectors,
pseudo-elements, animations and transitions, grid templates, `@supports`,
`@font-face`, `@import`, and DOM-style inheritance for anything but text.

## Performance, and why it is shaped this way

Styling runs per node per frame, so three things are load-bearing:

1. **Rules are bucketed** by id, class, element and universal. Matching a node
   visits a handful of rules and never touches one for a class it does not
   carry. The buckets are separate `HashMap<String, _>` rather than one map
   keyed by an enum, so a lookup takes a `&str` and allocates nothing.
2. **The cascade borrows.** `Winner` holds `&str` into the rule it came from.
   Cloning the property and value out of every declaration — before knowing
   whether it even wins — used to be the single largest cost in styling a tree.
3. **Variant passes are skipped when the sheet cannot use them.** A stylesheet
   that never writes `:active` cannot produce an active style that differs from
   the base, so `resolve_interactive` does not run that pass. Most sheets style
   hover and nothing else.

On a 3500-node tree those three took lowering from 26.3 ms/frame to 14.0. The
style cache in `spherekit-react` took it the rest of the way to 2.8 — see
`react-v8.md`.

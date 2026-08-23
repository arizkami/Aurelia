# spherekit-css

## Research decision

SphereKit already uses Taffy for DOM-free Flexbox/Grid layout. Taffy accepts a
typed style object and computes layout, but it is not a stylesheet parser. The
Servo `cssparser` crate is a good fit for the missing *value* layer: it
tokenises CSS and parses generic values while intentionally leaving selectors,
properties and cascade policy to the embedding engine. `spherekit-css` uses it
for exactly that — numbers, dimensions, percentages and identifiers — and owns
the selector engine, the cascade and the property model itself.

We did not embed Stylo because it is a browser-grade style system with a much
larger DOM/selector/cascade integration surface than a native audio UI needs.
We also did not use Lightning CSS as the runtime: it is primarily a parser,
transformer, bundler and minifier for build-time CSS.

## The rule that governs everything else

**Syntax the engine does not model is ignored, never reinterpreted.**

`width: 12` does not become `12px`. `border-radius: 8px / 4px` does not become a
circular radius. An unknown media feature makes its query *false*, not true. A
selector with an attribute test is dropped from its comma list rather than
matched loosely. A stylesheet that does nothing is debuggable; a stylesheet that
does something slightly different from what it says is not.

## Shared runtime

`Stylesheet::parse` and `Stylesheet::resolve` live in `spherekit-css`. The
result is a `ResolvedStyle` containing SphereKit's layout `Style`, a
`TextProperties` block and paint values. Native code can apply it directly to
any `spherekit_ui::Styled` element; `spherekit-react::ReactHost` stores the same
stylesheet and resolves `className`, `id` and inline `style` props during native
lowering.

The API Bridge exposes `spherekit.setStylesheet`, and the TypeScript bridge
provides `bridge.setStylesheet(css)`, so a React app and native elements can
share one stylesheet without shipping a browser runtime.

## Module map

| Module        | Responsibility |
| ------------- | -------------- |
| `lib.rs`      | `Stylesheet`, the cascade, `ResolvedStyle`, `InteractiveStyle`, `StyleContext` |
| `color.rs`    | named colours, hex, `rgb()`/`rgba()`/`hsl()`/`hsla()` |
| `length.rs`   | units and `calc()`, resolved against a `StyleContext` |
| `selector.rs` | `Node`, `ElementState`, `MatchPath`, compounds, combinators, specificity |
| `parser.rs`   | rules, declarations, `@media`, `var()` substitution |
| `property.rs` | declaration → `ResolvedStyle` |
| `text.rs`     | `TextProperties` and its route onto a `Label` |

## Selectors

Comma-separated lists of compound selectors joined by the descendant, `>`, `+`
and `~` combinators. A compound is an optional element name plus any number of
ids, classes and pseudo-classes: `view.card#main:hover:not(.disabled)`.

Supported pseudo-classes: `:hover`, `:active`, `:focus` (and `:focus-visible` /
`:focus-within`, which map onto it), `:disabled`, `:enabled`, `:checked`,
`:first-child`, `:last-child`, `:nth-child(an+b | odd | even)`, `:root`,
`:not(<compound list>)`.

Specificity is the usual `(ids, classes, elements)` triple. A pseudo-class
counts as a class; `:not()` contributes the weight of its heaviest argument and
nothing of its own; the universal selector contributes nothing. Inline
declarations outrank every selector, and `!important` outranks inline.

### What matching needs from the caller

`Node` carries the element name, id, classes, an `ElementState` and an optional
sibling position. Combinators additionally need ancestors, so they are supplied
through `MatchPath`:

```rust
let ancestors = [Node::new("view").with_classes("rack")];
let path = MatchPath::with_ancestors(&ancestors, Node::new("button"))
    .with_preceding_siblings(&siblings);
let style = sheet.resolve_in(&path, None, &StyleContext::default());
```

Two consequences are deliberate and worth stating:

* A path built with `MatchPath::new` has no ancestors, so descendant and child
  combinators cannot match it — and `:root` can, because a lone node is its own
  root.
* A sibling combinator against a path with no preceding siblings **fails
  closed**. A caller that cannot supply siblings has not told the engine that
  there are none, and guessing permissively would paint the wrong element.

### Rule bucketing

Selector matching runs once per node per commit, so a linear scan costs
`rules × nodes`. Rules are filed under the most selective name in their
rightmost compound — id, else first class, else element name, else universal —
and a node looks up only its own id, classes and element name. A node with no
matching class never visits those rules at all; there is a test that asserts
exactly that.

## Values

### Lengths

`px`, `%`, `rem`, `em`, `vw`, `vh`, `vmin`, `vmax`, `pt`, `auto`, a bare `0`,
and `calc()` over all of them with `+`, `-`, `*`, `/` and parentheses.

Font-relative and viewport-relative units resolve against the `StyleContext`
during the cascade, so everything downstream sees plain pixels. `em` inside a
rule follows a `font-size` declared in the same rule; `font-size` itself is
measured against the *incoming* context, which is what CSS means by "the
parent's size".

`calc()` reduces to an absolute part plus a percentage part. If both survive —
`calc(100% - 12px)` — the value is rejected, because SphereKit's `Length` has no
"parent minus a constant" shape and rounding to either half would be a lie.

### Colours

The full CSS named-colour table, `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, and
`rgb()`/`rgba()`/`hsl()`/`hsla()` in both the legacy comma syntax and the modern
space-with-slash syntax. Hue accepts `deg`, `rad`, `grad` and `turn`.

Note that `green` is now the CSS `#008000` and `lime` is `#00ff00`. The previous
six-entry table conflated the two.

### Custom properties

`--name: value` declarations cascade like any other declaration, and
`var(--name, fallback)` is substituted after the cascade — so a variable defined
by a *losing* rule cannot leak into a winning value. Substitution is recursive
and bounded at 16 levels, which is how a cycle terminates. A `var()` that is
neither defined nor given a fallback invalidates its own declaration and nothing
else.

Custom properties are collected down the whole `MatchPath`, so `:root { --x }`
reaches a descendant. That is the only thing this crate inherits, and it is
inherited because a custom property is a *value*, not a computed style.

## Media queries

`@media` blocks are parsed and **retained**. Their rules simply do not match
while the query is false — dropping them, as the first version of this crate
did, made a stylesheet's meaning depend on the window size at the moment it was
installed.

Supported: `(min-width:)`, `(max-width:)`, `(min-height:)`, `(max-height:)`,
`(prefers-color-scheme: dark | light)`, `and`, comma lists, the `all` and
`screen` media types, and nesting (every enclosing condition must hold). Lengths
in a query are resolved against the same `StyleContext` as the rest of the
cascade.

An unrecognised feature or media type makes its query false.

## Properties

**Layout** — `display` (including `inline-flex`, `inline-block`, `inline-grid`),
`position` (`fixed` maps onto `absolute`), `top`/`right`/`bottom`/`left`,
`inset`, `width`/`height`, `min-*`/`max-*`, `margin`, `padding`, `border-width`
and their longhands, `flex` and its longhands, `gap`/`row-gap`/`column-gap`,
`align-items`, `align-self`, `align-content`, `justify-content`, `place-items`,
`place-content`, `overflow`/`-x`/`-y`, `aspect-ratio` (number or `w / h`),
`z-index`, `opacity` (number or percentage), `visibility`.

**Paint** — `background`/`background-color`, `border`, `border-color`,
`border-radius` with one to four values plus the four per-corner longhands,
`box-shadow`, `cursor`.

**Text** — `color`, `font-size` (lengths and the absolute keywords),
`font-family`, `font-weight` (`100`–`900`, `normal`, `bold`, `lighter`,
`bolder`), `font-style`, `line-height` (number, percentage or length),
`letter-spacing`, `text-align`, `white-space: nowrap`,
`text-overflow: ellipsis`.

### Application order

The cascade picks one winner per property, after which their relative order is
arbitrary. Declarations are therefore applied by `(tier, name)`: multi-property
shorthands (`border`, `flex`, `background`, `overflow`, `place-*`) first, then
box shorthands (`padding`, `margin`, `inset`, `border-width`, `border-radius`,
`gap`), then longhands. A longhand always has the last word, whichever way round
the author wrote it.

### Mappings that are not one-to-one

* `visibility: hidden` becomes zero opacity. SphereKit has no `visibility`: a
  node is laid out and painted, or it is `Display::None` and neither. Zero
  opacity keeps CSS's actual guarantee that the box still occupies its space.
* `place-items` applies only its block-axis half. `justify-items` has no
  counterpart — every flex item is placed by its own `align-self` — and mapping
  it onto `justify-content` would silently change a distribution instead.
* `box-shadow`'s `inset` keyword is recognised and then dropped.
  `spherekit_ui::PaintStyle` paints every shadow *behind* the background, so an
  inset shadow would be covered by the fill. Recognising the keyword at least
  keeps `inset 0 1px 2px #000` from being read as a four-length outer shadow.
* `font-weight: lighter`/`bolder` map to fixed steps (300 and 700). They are
  relative to the inherited weight in CSS, and nothing here knows it at the
  point a declaration is interpreted.
* An empty shadow list leaves an element's own shadows alone. `ResolvedStyle`
  cannot tell "unset" from `box-shadow: none` once the cascade has run, and
  stripping a widget's own elevation is the worse failure.

## Interaction states

`Stylesheet::resolve_interactive` returns the base style plus the `:hover`,
`:active` and `:focus` variants, each `None` when it computes to exactly the
base. `InteractiveStyle::apply_to` writes the hover and active backgrounds into
`PaintStyle`, which is what lets a hover cost a repaint rather than a rebuild.
The `:active` variant is resolved with hover also set, because a pressed pointer
is also a hovering pointer.

A `:focus` variant becomes a `FocusRing` built from its border colour and width,
and only when it actually changes the border colour — `:focus { border-color: … }`
is the closest CSS idiom to a ring, and `spherekit-ui` paints no other focus
affordance.

## Deliberately out

Not "not yet" — these are decisions:

* **Animations and transitions.** `@keyframes`, `animation` and `transition`
  are skipped entirely. SphereKit already has `spherekit_core::animate`; a
  second, string-driven timeline that the native animator could not see would be
  two sources of truth for the same frame.
* **Gradients.** `linear-gradient()` and friends parse into
  `spherekit_core::Gradient` shapes whose stops sit at absolute points in the
  shape's local space, so a gradient cannot be computed without the element's
  final box. A `ResolvedStyle` here is deliberately size-independent — resolved
  once per node and reused — so gradients would mean re-resolving paint after
  layout, a pipeline change rather than a parser change.
* **Grid templates.** `grid-template-rows`/`-columns`, named lines, named areas
  and per-item placement. The layout backend supports them; `spherekit_layout::Style`
  deliberately does not expose a track-sizing vocabulary yet, and this crate does
  not get to invent one.
* **Pseudo-elements.** `::before`, `::after`, `::placeholder`. There is no second
  box to style, so a selector naming one is dropped rather than styling the
  element itself.
* **Attribute selectors.** `[data-role=knob]`. Native nodes have no attribute
  bag; classes already carry everything the React host can supply.
* **`:has()`, `:is()`, `:where()`, `:nth-last-child()`, `:only-child`.**
  Matching cost or plumbing out of proportion to their use in a plug-in editor.
* **Inheritance beyond text and custom properties.** `ResolvedStyle` is
  resolved per node with no parent style. `TextProperties::inherit_from` gives a
  caller that *does* have a tree the fold it needs; custom properties are
  collected down the `MatchPath` because they are values rather than computed
  styles. Nothing else inherits, and nothing pretends to.
* **`currentcolor`, `inherit`, `initial`, `unset`, `revert`.** All of them need
  either the parent's computed style or the property's initial value as a
  first-class concept, neither of which exists here.
* **Elliptical corner radii.** `border-radius: 8px / 4px`. `Corners` carries one
  radius per corner; the second set could only be discarded.
* **`@supports`, `@font-face`, `@import`, `@layer`.** Skipped with their whole
  block, so their declarations never leak into the enclosing scope.

/** Values accepted by the shared native CSS style bridge. */
export type CssValue = string | number;

/** A serializable inline style object for React native components. */
export type CssProperties = Readonly<Record<string, CssValue>>;

/** A set of CSS rules keyed by selector, in source order. */
export type CssRules = Readonly<Record<string, CssProperties>>;

/**
 * Marks an object as a CSS properties object.
 *
 * The object is intentionally returned unchanged: React's normal `style`
 * prop remains the transport format, while the native runtime performs the
 * final CSS value mapping so native and React use one property vocabulary.
 */
export function css(properties: CssProperties): CssProperties {
  return properties;
}

/**
 * Properties whose bare numbers are ratios, not lengths.
 *
 * This list is duplicated from the Rust host's `UNITLESS` on purpose, and it
 * is the one place where the two sides can silently disagree: a property
 * missing here turns `opacity: 0.5` into `opacity: 0.5px`, which the CSS
 * parser drops without error, so the element simply renders opaque and nothing
 * in the logs says why. The Rust copy lives in `spherekit-react/src/lower.rs`.
 *
 * Because a mismatch is invisible at runtime, it is caught at test time: this
 * list is spelled out in full by "pins the same unitless list the Rust host
 * does" in `test/css.test.ts`, and by `the_unitless_list_is_the_one_the_
 * typescript_side_pins` on the Rust side. Exported for that test rather than
 * from the package index — a consumer writes CSS, not the mapping table.
 *
 * `line-height` is on the list because a bare number there means "this many
 * times the font size" in CSS; appending `px` would quietly change 1.4 from a
 * comfortable multiple into a 1.4px line box.
 */
export const UNITLESS_PROPERTIES: ReadonlySet<string> = new Set([
  "flex-grow",
  "flex-shrink",
  "opacity",
  "z-index",
  "aspect-ratio",
  "font-weight",
  "line-height",
]);

/**
 * Renders a style object as the declaration text of a CSS rule.
 *
 * The property names go through the same normalisation the Rust host applies
 * to an inline `style` prop — `_` and camelCase both become `-` — so the two
 * paths to the cascade cannot disagree about what an author wrote. Numbers
 * gain `px` unless the property is one of the unitless ratios above.
 *
 * Declarations are joined with `"; "` and carry no trailing semicolon, which
 * matches the host's own `inline_css` output; the difference is invisible to a
 * parser but keeps snapshot comparisons between the two sides byte-identical.
 *
 * Non-finite numbers are dropped rather than rendered: `NaN` and `Infinity`
 * cannot cross JSON either, so emitting them would produce a declaration that
 * the transport would then refuse to carry. A string containing `;` is dropped
 * for the sharper version of the same reason — it would split one declaration
 * into two, and the offcut has no colon, so the parser rejects the whole block
 * and the node loses every style around it as well.
 */
export function toCssText(properties: CssProperties): string {
  return toDeclarations(properties).join("; ");
}

/**
 * Builds stylesheet text for the bridge's `setStylesheet` from selector-keyed
 * rules.
 *
 * Authoring a stylesheet as an object rather than a template literal is what
 * makes the unitless mapping above reachable from application code: a rule
 * written as a string bypasses `toCssText` entirely and has to spell out every
 * unit by hand.
 *
 * Selectors whose bodies render empty are omitted, because an empty rule tells
 * the cascade nothing and only makes a diff of two stylesheets noisier.
 */
export function stylesheet(rules: CssRules): string {
  const blocks: string[] = [];

  for (const [selector, properties] of Object.entries(rules)) {
    const declarations = toDeclarations(properties);
    if (declarations.length === 0) continue;
    const body = declarations.map((declaration) => `  ${declaration};`).join("\n");
    blocks.push(`${selector} {\n${body}\n}`);
  }

  return blocks.join("\n\n");
}

function toDeclarations(properties: CssProperties): string[] {
  const declarations: string[] = [];

  for (const [property, value] of Object.entries(properties)) {
    const name = toKebabCase(property);
    const rendered = toCssValue(name, value);
    if (rendered !== undefined) declarations.push(`${name}: ${rendered}`);
  }

  return declarations;
}

/** Joins conditional class names for `className` props. */
export function cx(...names: readonly (string | false | null | undefined)[]): string {
  return names.filter((name): name is string => Boolean(name && name.trim())).join(" ");
}

/**
 * Normalises a JavaScript property name to its CSS spelling.
 *
 * Custom properties are left alone: `--brandAccent` is a name the author
 * chose, not a camelCase spelling of a standard property, and rewriting it
 * would break the `var()` that reads it back.
 */
function toKebabCase(property: string): string {
  if (property.startsWith("--")) return property;

  let normalized = "";
  for (const character of property.replace(/_/g, "-")) {
    if (character >= "A" && character <= "Z") normalized += `-${character.toLowerCase()}`;
    else normalized += character;
  }
  return normalized;
}

function toCssValue(property: string, value: CssValue): string | undefined {
  if (typeof value === "string") {
    return value.length === 0 || value.includes(";") ? undefined : value;
  }
  if (!Number.isFinite(value)) return undefined;
  return UNITLESS_PROPERTIES.has(property) ? String(value) : `${value}px`;
}

/** Values accepted by the shared native CSS style bridge. */
export type CssValue = string | number;

/** A serializable inline style object for React native components. */
export type CssProperties = Readonly<Record<string, CssValue>>;

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

/** Joins conditional class names for `className` props. */
export function cx(...names: readonly (string | false | null | undefined)[]): string {
  return names.filter((name): name is string => Boolean(name && name.trim())).join(" ");
}

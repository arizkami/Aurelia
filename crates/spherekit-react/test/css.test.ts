import { describe, expect, it } from "bun:test";
import { css, cx, stylesheet, toCssText } from "../src/index";
import { UNITLESS_PROPERTIES } from "../src/css";
import type { CssProperties } from "../src/index";

/** Reads a stylesheet back into the rules record it was built from. */
function parse(sheet: string): Record<string, string[]> {
  const rules: Record<string, string[]> = {};

  for (const block of sheet.split("}")) {
    const [selector, body] = block.split("{");
    if (selector === undefined || body === undefined) continue;
    rules[selector.trim()] = body
      .split(";")
      .map((declaration) => declaration.trim())
      .filter((declaration) => declaration.length > 0);
  }

  return rules;
}

describe("CSS helpers", () => {
  it("adds pixels to lengths and leaves the unitless properties bare", () => {
    const properties: CssProperties = {
      gap: 8,
      marginTop: 0,
      flexGrow: 1,
      flexShrink: 0,
      opacity: 0.5,
      zIndex: 10,
      aspectRatio: 1.5,
      fontWeight: 600,
      lineHeight: 1.4,
    };

    expect(toCssText(properties)).toBe(
      "gap: 8px; margin-top: 0px; flex-grow: 1; flex-shrink: 0; opacity: 0.5; " +
        "z-index: 10; aspect-ratio: 1.5; font-weight: 600; line-height: 1.4",
    );
  });

  it("pins the same unitless list the Rust host does", () => {
    // Spelled out rather than derived, because the failure this guards against
    // is the list itself drifting from `UNITLESS` in src/lower.rs, which the
    // Rust test `the_unitless_list_is_the_one_the_typescript_side_pins` spells
    // out identically. A property on one list and not the other renders
    // correctly on one side and silently wrong on the other.
    expect([...UNITLESS_PROPERTIES]).toEqual([
      "flex-grow",
      "flex-shrink",
      "opacity",
      "z-index",
      "aspect-ratio",
      "font-weight",
      "line-height",
    ]);

    for (const property of UNITLESS_PROPERTIES) {
      expect(toCssText({ [property]: 2 })).toBe(`${property}: 2`);
    }
    expect(toCssText({ gap: 2 })).toBe("gap: 2px");
  });

  it("normalises camelCase and snake_case to the same CSS property", () => {
    expect(toCssText({ flexDirection: "column" })).toBe("flex-direction: column");
    expect(toCssText({ flex_direction: "column" })).toBe("flex-direction: column");
  });

  it("leaves custom properties exactly as the author spelled them", () => {
    expect(toCssText({ "--brandAccent": "#ff0055" })).toBe("--brandAccent: #ff0055");
  });

  it("drops values the transport could not carry", () => {
    expect(toCssText({ width: Number.NaN, height: Number.POSITIVE_INFINITY, color: "" })).toBe("");
  });

  it("drops a semicolon rather than letting it invalidate the whole block", () => {
    // The offcut after the split has no colon, so a parser handed the block
    // rejects all of it — `width` would go down with the value that broke it.
    expect(toCssText({ content: "a; b", width: 10 })).toBe("width: 10px");
    expect(stylesheet({ ".panel": { content: "a; b", gap: 8 } })).toBe(".panel {\n  gap: 8px;\n}");
  });

  it("keeps string values untouched so shorthands still work", () => {
    expect(toCssText({ padding: "8px 12px", background: "linear-gradient(#000, #fff)" })).toBe(
      "padding: 8px 12px; background: linear-gradient(#000, #fff)",
    );
  });

  it("round-trips a stylesheet through a selector parser", () => {
    const sheet = stylesheet({
      ".panel": { display: "flex", gap: 8 },
      ".panel .title": { fontWeight: 600, lineHeight: 1.4 },
    });

    expect(parse(sheet)).toEqual({
      ".panel": ["display: flex", "gap: 8px"],
      ".panel .title": ["font-weight: 600", "line-height: 1.4"],
    });
  });

  it("omits a rule that has nothing left to declare", () => {
    expect(stylesheet({ ".empty": {}, ".dropped": { width: Number.NaN } })).toBe("");
  });

  it("returns the style object unchanged so React still owns the style prop", () => {
    const properties = { gap: 8 };
    expect(css(properties)).toBe(properties);
  });

  it("joins only the class names that survived their conditions", () => {
    expect(cx("panel", false, undefined, null, "  ", "primary")).toBe("panel primary");
  });
});

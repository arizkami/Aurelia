import { createElement, type ReactNode } from "react";
import type { NativeProps } from "./types";

/**
 * Every host node type the Rust registry knows how to build.
 *
 * Exported as a union rather than left implicit in each component so that a
 * type the registry drops, or one it gains, shows up here as a compile error
 * on the TypeScript side instead of as a node the host silently renders as a
 * bare layout container.
 */
export type NativeComponentType =
  | "view"
  | "text"
  | "button"
  | "slider"
  | "knob"
  | "fader"
  | "scroll-view"
  | "toggle"
  | "checkbox"
  | "progress"
  | "separator"
  | "panel"
  | "text-field"
  | "avatar"
  | "menu-item";

/** Props shared by the small native component set. */
export type ViewProps = NativeProps & {
  /** Nested elements; text children become `#text` nodes in the snapshot. */
  readonly children?: ReactNode;
};

/** A flex/layout container. */
export function View({ children, ...props }: ViewProps) {
  return createElement("view", props, children);
}

/** A text leaf. */
export function Text({ children, ...props }: ViewProps) {
  return createElement("text", props, children);
}

/** Props for {@link Button}. */
export type ButtonProps = ViewProps & {
  /** Label drawn on the button when it has no children. */
  readonly title?: string;
  /** Refuses input and draws the muted variant. */
  readonly disabled?: boolean;
  /** Fired on a completed click: press and release inside the button. */
  readonly onPress?: () => void;
};

/**
 * A pressable native button.
 *
 * `title` is both serialised as a prop and used as the fallback child, so a
 * host that renders its own label and one that lays out children both show the
 * same text without the caller writing it twice.
 */
export function Button({ children, title, ...props }: ButtonProps) {
  return createElement("button", { ...props, title }, children ?? title);
}

/** Props for the three range controls: {@link Slider}, {@link Knob}, {@link Fader}. */
export type SliderProps = ViewProps & {
  /** Current position, clamped by the host into the min/max range. */
  readonly value?: number;
  /** Low end of the range. Defaults to 0 on the host. */
  readonly minimumValue?: number;
  /** High end of the range. Defaults to 1 on the host. */
  readonly maximumValue?: number;
  /** Quantises dragging; omit for continuous motion. */
  readonly step?: number;
  /** Refuses input and draws the muted variant. */
  readonly disabled?: boolean;
  /** Fired with the new value while the control is dragged. */
  readonly onValueChange?: (value: number) => void;
};

/** A horizontal range control. */
export function Slider({ children, ...props }: SliderProps) {
  return createElement("slider", props, children);
}

/** Props for {@link Knob}; identical to a slider's. */
export type KnobProps = SliderProps;

/** A rotary range control, for mixer-style panels where a slider is too wide. */
export function Knob({ children, ...props }: KnobProps) {
  return createElement("knob", props, children);
}

/** Props for {@link Fader}; identical to a slider's. */
export type FaderProps = SliderProps;

/** A vertical range control, drawn as a channel fader. */
export function Fader({ children, ...props }: FaderProps) {
  return createElement("fader", props, children);
}

/**
 * Props for {@link ScrollView}.
 *
 * There is deliberately no `onScroll`. The native `scroll-view` handles the
 * wheel itself and reports no offset back, so a callback here could only ever
 * be declared and never called — worse than its absence, because it type
 * checks and then does nothing. Adding one means giving `spherekit-ui`'s
 * `ScrollView` a way to report its offset first.
 */
export type ScrollViewProps = ViewProps & {
  /** Scrolls along x instead of y. */
  readonly horizontal?: boolean;
  /** Scrolls along both axes; overrides {@link ScrollViewProps.horizontal}. */
  readonly both?: boolean;
};

/** A scrollable native container. */
export function ScrollView({ children, ...props }: ScrollViewProps) {
  return createElement("scroll-view", props, children);
}

/** Props for {@link Toggle} and {@link Checkbox}. */
export type ToggleProps = ViewProps & {
  /** Whether the control reads as on. Controlled: the host never flips it alone. */
  readonly checked?: boolean;
  /** Text drawn beside the control and included in its hit area. */
  readonly label?: string;
  /** Refuses input and draws the muted variant. */
  readonly disabled?: boolean;
  /** Fired with the state the user asked for, which is `!checked`. */
  readonly onChange?: (checked: boolean) => void;
};

/** A sliding on/off switch. */
export function Toggle({ children, ...props }: ToggleProps) {
  return createElement("toggle", props, children);
}

/**
 * A box-and-tick switch.
 *
 * Same state as {@link Toggle}, kept as its own host type because the choice
 * between them is about what the surrounding form looks like, not about what
 * the control does.
 */
export function Checkbox({ children, ...props }: ToggleProps) {
  return createElement("checkbox", props, children);
}

/** Props for {@link Progress}. */
export type ProgressProps = ViewProps & {
  /** Completed fraction from 0 to 1; ignored while indeterminate. */
  readonly value?: number;
  /** Draws the sweeping variant for work with no measurable end. */
  readonly indeterminate?: boolean;
  /** Bar thickness in logical pixels. */
  readonly thickness?: number;
};

/** A determinate or indeterminate progress bar. */
export function Progress({ children, ...props }: ProgressProps) {
  return createElement("progress", props, children);
}

/** Props for {@link Separator}. */
export type SeparatorProps = ViewProps & {
  /** Draws a column rule instead of a row rule. */
  readonly vertical?: boolean;
};

/** A hairline rule between groups. */
export function Separator({ children, ...props }: SeparatorProps) {
  return createElement("separator", props, children);
}

/** Props for {@link Panel}. */
export type PanelProps = ViewProps & {
  /** Heading drawn above the panel body. */
  readonly title?: string;
};

/** A titled container with the theme's panel surface and padding. */
export function Panel({ children, ...props }: PanelProps) {
  return createElement("panel", props, children);
}

/** Props for {@link TextField}. */
export type TextFieldProps = ViewProps & {
  /** Current text. Controlled: the host echoes edits back through `onChange`. */
  readonly value?: string;
  /** Hint drawn while the field is empty. */
  readonly placeholder?: string;
  /** Refuses input and draws the muted variant. */
  readonly disabled?: boolean;
  /** Draws bullets instead of glyphs, and withholds the text from the clipboard. */
  readonly mask?: boolean;
  /** Fired with the full text after each edit. */
  readonly onChange?: (value: string) => void;
  /** Fired with the full text when the user commits with Enter. */
  readonly onSubmit?: (value: string) => void;
};

/** A single-line editable text field. */
export function TextField({ children, ...props }: TextFieldProps) {
  return createElement("text-field", props, children);
}

/** How available a person is, drawn as a dot on their {@link Avatar}. */
export type AvatarPresence = "online" | "away" | "busy" | "offline";

/** Props for {@link Avatar}. */
export type AvatarProps = ViewProps & {
  /** Full name; the host derives initials and the tint from it. */
  readonly name?: string;
  /** Overrides the derived initials for names the two-letter rule reads wrongly. */
  readonly initials?: string;
  /** Diameter in logical pixels. */
  readonly size?: number;
  /** Presence dot on the lower-right edge; omit to draw none. */
  readonly presence?: AvatarPresence;
};

/** A circular portrait: initials on a tint, with optional presence. */
export function Avatar({ children, ...props }: AvatarProps) {
  return createElement("avatar", props, children);
}

/** Props for {@link MenuItem}. */
export type MenuItemProps = ViewProps & {
  /** Row text. */
  readonly label?: string;
  /** Accelerator shown right-aligned, such as `Ctrl+C`. Display only. */
  readonly shortcut?: string;
  /** Draws the destructive variant. */
  readonly danger?: boolean;
  /** Refuses input and draws the muted variant. */
  readonly disabled?: boolean;
  /** Fired when the row is chosen by click or by keyboard. */
  readonly onSelect?: () => void;
};

/**
 * A row in a menu or context menu.
 *
 * `label` doubles as the fallback child for the same reason `title` does on
 * {@link Button}.
 */
export function MenuItem({ children, label, ...props }: MenuItemProps) {
  return createElement("menu-item", { ...props, label }, children ?? label);
}

/** Props for {@link Native}. */
export type NativePropsWithType = ViewProps & {
  /**
   * The registered host type to build.
   *
   * Widened past {@link NativeComponentType} on purpose: an embedder can
   * register its own node types with the Rust host, and this component is how
   * they are reached without waiting for a typed wrapper here.
   */
  readonly type: NativeComponentType | (string & {});
};

/** Escape hatch for adding a registered SphereKit host component. */
export function Native({ type, children, ...props }: NativePropsWithType) {
  return createElement(type, props, children);
}

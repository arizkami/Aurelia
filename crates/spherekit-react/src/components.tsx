import { createElement, type ReactNode } from "react";
import type { NativeProps } from "./types";

/** Props shared by the small native component set. */
export type ViewProps = NativeProps & {
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

/** A pressable native button. */
export type ButtonProps = ViewProps & {
  readonly title?: string;
  readonly disabled?: boolean;
  readonly onPress?: () => void;
};

export function Button({ children, title, ...props }: ButtonProps) {
  return createElement("button", { ...props, title }, children ?? title);
}

/** A native range control. */
export type SliderProps = ViewProps & {
  readonly value?: number;
  readonly minimumValue?: number;
  readonly maximumValue?: number;
  readonly step?: number;
  readonly onValueChange?: (value: number) => void;
};

export function Slider({ children, ...props }: SliderProps) {
  return createElement("slider", props, children);
}

/** A scrollable native container. */
export function ScrollView({ children, ...props }: ViewProps) {
  return createElement("scroll-view", props, children);
}

/** Escape hatch for adding a registered SphereKit host component. */
export type NativePropsWithType = ViewProps & { readonly type: string };

export function Native({ type, children, ...props }: NativePropsWithType) {
  return createElement(type, props, children);
}

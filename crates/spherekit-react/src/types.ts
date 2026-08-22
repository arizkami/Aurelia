import type { ReactNode } from "react";

/** Values that can cross the TypeScript/native bridge. */
export type NativeValue =
  | null
  | boolean
  | number
  | string
  | readonly NativeValue[]
  | { readonly [key: string]: NativeValue };

/** A serializable native node produced by the React host tree. */
export interface NativeNodeSnapshot {
  readonly id: number;
  readonly type: string;
  readonly props: Readonly<Record<string, NativeValue>>;
  readonly text?: string;
  readonly hidden?: boolean;
  readonly children: readonly NativeNodeSnapshot[];
}

/** The complete tree sent to the Rust backend after a React commit. */
export interface NativeTreeSnapshot {
  readonly revision: number;
  readonly children: readonly NativeNodeSnapshot[];
}

/** An input or lifecycle event emitted by the native host. */
export interface NativeEvent {
  readonly event: string;
  readonly nodeId?: number;
  readonly payload?: NativeValue;
}

/** The narrow transport contract between the React renderer and Rust. */
export interface NativeBridge {
  commit(snapshot: NativeTreeSnapshot): void;
  /** Subscribes to native events when the transport supports them. */
  onEvent?(listener: (event: NativeEvent) => void): () => void;
}

/** Adapter for a host exposing a JSON commit method over FFI or IPC. */
export function jsonBridge(commitJson: (snapshot: string) => void): NativeBridge {
  return {
    commit(snapshot) {
      commitJson(JSON.stringify(snapshot));
    },
  };
}

/** Props accepted by a native host component. React event functions stay local. */
export type NativeProps = Readonly<Record<string, unknown>> & {
  readonly children?: ReactNode;
  /** CSS class names resolved by the shared native CSS runtime. */
  readonly className?: string;
  /** Author-facing CSS id used by stylesheet selectors. */
  readonly id?: string;
};

export function serializeProps(props: NativeProps): Readonly<Record<string, NativeValue>> {
  const serialized: Record<string, NativeValue> = {};

  for (const [name, value] of Object.entries(props)) {
    if (name === "children" || name.startsWith("on")) continue;
    const nativeValue = toNativeValue(value);
    if (nativeValue !== undefined) serialized[name] = nativeValue;
  }

  return serialized;
}

function toNativeValue(value: unknown): NativeValue | undefined {
  if (value === null) return null;
  if (typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") return Number.isFinite(value) ? value : undefined;

  if (Array.isArray(value)) {
    const values = value.map(toNativeValue);
    return values.every((item): item is NativeValue => item !== undefined) ? values : undefined;
  }

  if (typeof value === "object" && value !== null) {
    const entries = Object.entries(value).map(([key, item]) => [key, toNativeValue(item)] as const);
    if (entries.some(([, item]) => item === undefined)) return undefined;
    return Object.fromEntries(entries) as { readonly [key: string]: NativeValue };
  }

  return undefined;
}

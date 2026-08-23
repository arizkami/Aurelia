import { createContext, type ReactNode } from "react";
import Reconciler from "react-reconciler";
import type { NativeBridge, NativeEvent, NativeNodeSnapshot, NativeProps, NativeTreeSnapshot } from "./types";
import { serializeProps } from "./types";

interface HostContext {
  readonly namespace: "spherekit";
}

interface HostInstance {
  readonly id: number;
  readonly type: string;
  props: NativeProps;
  children: HostChild[];
  hidden: boolean;
}

interface HostTextInstance {
  readonly id: number;
  text: string;
  hidden: boolean;
}

type HostChild = HostInstance | HostTextInstance;

interface HostContainer {
  readonly bridge: NativeBridge;
  children: HostChild[];
  revision: number;
}

/** A mounted React root backed by a SphereKit native bridge. */
export interface ReactRoot {
  render(element: ReactNode): void;
  unmount(): void;
  /** Routes a native event into the matching React callback. */
  dispatch(event: NativeEvent): void;
}

const hostTransitionContext = createContext<null>(null);

/**
 * React's lane for "no priority was established by an event".
 *
 * The reconciler asks the host to resolve a priority for every update, and it
 * takes lane 0 literally: an update tagged with it is queued against no lane
 * and never scheduled. A host that answers 0 unconditionally therefore drops
 * every state update that does not originate inside a synchronous `render()` —
 * effects, promise callbacks and native events all silently do nothing.
 */
const NO_LANE = 0;

/**
 * `DefaultEventPriority`, the lane React's own DOM host uses for an update
 * with no ambient event priority. It is the right default here for the same
 * reason: SphereKit dispatches native events itself, so an update arriving
 * outside one is ordinary work, not urgent input.
 */
const DEFAULT_LANE = 32;

/**
 * Creates a React 19 custom renderer for native SphereKit nodes.
 *
 * React mutates a private host tree during reconciliation. The complete,
 * serializable snapshot is handed to Rust only after the commit finishes, so
 * the native side never observes a half-built tree.
 */
export function createRoot(bridge: NativeBridge): ReactRoot {
  let nextId = 1;
  let currentUpdatePriority = NO_LANE;
  const container: HostContainer = { bridge, children: [], revision: 0 };
  const instances = new Map<number, HostInstance | HostTextInstance>();

  const hostConfig = {
    supportsMutation: true,
    supportsPersistence: false,
    supportsHydration: false,
    isPrimaryRenderer: false,
    warnsIfNotActing: false,

    createInstance(type: string, props: NativeProps): HostInstance {
      const instance = { id: nextId++, type, props, children: [], hidden: false };
      instances.set(instance.id, instance);
      return instance;
    },

    createTextInstance(text: string): HostTextInstance {
      const instance = { id: nextId++, text, hidden: false };
      instances.set(instance.id, instance);
      return instance;
    },

    appendInitialChild(parent: HostInstance, child: HostChild): void {
      appendUnique(parent.children, child);
    },

    finalizeInitialChildren(): boolean {
      return false;
    },

    shouldSetTextContent(): boolean {
      return false;
    },

    getRootHostContext(): HostContext {
      return { namespace: "spherekit" };
    },

    getChildHostContext(parentHostContext: HostContext): HostContext {
      return parentHostContext;
    },

    getPublicInstance(instance: HostInstance | HostTextInstance): HostInstance | HostTextInstance {
      return instance;
    },

    prepareForCommit(): null {
      return null;
    },

    resetAfterCommit(currentContainer: HostContainer): void {
      currentContainer.revision += 1;
      currentContainer.bridge.commit(toSnapshot(currentContainer));
    },

    preparePortalMount(): void {},

    scheduleTimeout: setTimeout,
    cancelTimeout: clearTimeout,
    noTimeout: -1,
    supportsMicrotasks: true,
    scheduleMicrotask: queueMicrotask,

    getInstanceFromNode(): null {
      return null;
    },

    beforeActiveInstanceBlur(): void {},
    afterActiveInstanceBlur(): void {},
    prepareScopeUpdate(): void {},
    getInstanceFromScope(): null {
      return null;
    },
    detachDeletedInstance(instance: HostInstance): void {
      instances.delete(instance.id);
    },

    appendChild(parent: HostInstance, child: HostChild): void {
      appendUnique(parent.children, child);
    },

    appendChildToContainer(currentContainer: HostContainer, child: HostChild): void {
      appendUnique(currentContainer.children, child);
    },

    insertBefore(parent: HostInstance, child: HostChild, beforeChild: HostChild): void {
      insertBefore(parent.children, child, beforeChild);
    },

    insertInContainerBefore(
      currentContainer: HostContainer,
      child: HostChild,
      beforeChild: HostChild,
    ): void {
      insertBefore(currentContainer.children, child, beforeChild);
    },

    removeChild(parent: HostInstance, child: HostChild): void {
      removeChild(parent.children, child);
    },

    removeChildFromContainer(currentContainer: HostContainer, child: HostChild): void {
      removeChild(currentContainer.children, child);
    },

    commitTextUpdate(instance: HostTextInstance, _oldText: string, newText: string): void {
      instance.text = newText;
    },

    commitUpdate(instance: HostInstance, _type: string, _oldProps: NativeProps, newProps: NativeProps): void {
      instance.props = newProps;
    },

    hideInstance(instance: HostInstance): void {
      instance.hidden = true;
    },

    hideTextInstance(instance: HostTextInstance): void {
      instance.hidden = true;
    },

    unhideInstance(instance: HostInstance): void {
      instance.hidden = false;
    },

    unhideTextInstance(instance: HostTextInstance): void {
      instance.hidden = false;
    },

    clearContainer(currentContainer: HostContainer): void {
      currentContainer.children = [];
    },

    NotPendingTransition: null,
    HostTransitionContext: hostTransitionContext,
    setCurrentUpdatePriority(priority: number): void {
      currentUpdatePriority = priority;
    },
    getCurrentUpdatePriority(): number {
      return currentUpdatePriority;
    },
    resolveUpdatePriority(): number {
      return currentUpdatePriority === NO_LANE ? DEFAULT_LANE : currentUpdatePriority;
    },
    resetFormInstance(): void {},
    requestPostPaintCallback(callback: (time: number) => void): void {
      queueMicrotask(() => callback(performance.now()));
    },
    shouldAttemptEagerTransition(): boolean {
      return false;
    },
    trackSchedulerEvent(): void {},
    resolveEventType(): null {
      return null;
    },
    resolveEventTimeStamp(): number {
      return performance.now();
    },
    maySuspendCommit(): boolean {
      return false;
    },
    preloadInstance(): boolean {
      return true;
    },
    startSuspendingCommit(): void {},
    suspendInstance(): void {},
    waitForCommitToBeReady(): null {
      return null;
    },
  };

  // @types/react-reconciler intentionally mirrors React's unstable internal
  // host-config surface. Keep the runtime implementation above explicit, but
  // avoid coupling the public renderer API to every internal generic parameter.
  const reconciler = Reconciler(hostConfig as never);
  const root = reconciler.createContainer(
    container,
    1,
    null,
    false,
    null,
    "",
    (error) => {
      throw error;
    },
    (error) => {
      throw error;
    },
    (error) => {
      console.error("SphereKit React recoverable error", error);
    },
    () => {},
  );

  const dispatch = (event: NativeEvent): void => {
    if (event.nodeId === undefined) return;
    const instance = instances.get(event.nodeId);
    if (!instance || isTextInstance(instance)) return;
    const callback = eventCallback(instance.props, event.event);
    if (!callback) return;
    if (ARGUMENT_LESS_EVENTS.has(event.event)) callback();
    else callback(unwrapPayload(event.event, event.payload));
  };
  const unsubscribe = bridge.onEvent?.(dispatch);

  return {
    render(element) {
      reconciler.updateContainerSync(element, root, null, null);
      reconciler.flushSyncWork();
    },
    unmount() {
      reconciler.updateContainerSync(null, root, null, null);
      reconciler.flushSyncWork();
      unsubscribe?.();
      instances.clear();
    },
    dispatch,
  };
}

/**
 * Events whose React callback is declared to take nothing.
 *
 * A press or a menu selection carries no value the caller could use, and
 * passing the raw frame payload anyway would make `onPress={setOpen}` — a
 * setter that happens to accept one argument — receive an object nobody meant
 * to send.
 */
const ARGUMENT_LESS_EVENTS: ReadonlySet<string> = new Set(["press", "select"]);

/**
 * Which key of an event payload each callback actually wants.
 *
 * Frames always carry `payload` as a JSON object, because the wire schema has
 * nowhere else to put the value, but the handlers in this package take the
 * thing that changed: a toggle's `onChange` takes a boolean, not `{checked}`.
 *
 * The three entries here cover every event the built-in widgets emit. Anything
 * else — an event from a type an embedder registered with
 * `ReactHost::register_host_type` — receives the payload object whole, because
 * this table cannot know which of its keys such a handler wanted, and picking
 * one would silently discard the rest.
 *
 * A `Map` rather than an object literal because the event name arrives from
 * the native host: a frame naming `constructor` would find a match on
 * `Object.prototype` and hand the loop below something that is not an array.
 */
const PAYLOAD_KEYS: ReadonlyMap<string, readonly string[]> = new Map<string, readonly string[]>([
  ["valueChange", ["value"]],
  ["change", ["checked", "value", "text"]],
  ["submit", ["text", "value"]],
]);

function unwrapPayload(event: string, payload: unknown): unknown {
  const keys = PAYLOAD_KEYS.get(event);
  if (!keys || !isObject(payload)) return payload;
  for (const key of keys) {
    if (key in payload) return payload[key];
  }
  return payload;
}

function eventCallback(props: NativeProps, event: string): ((payload?: unknown) => void) | undefined {
  const normalized = event.startsWith("on") ? event : `on${event.slice(0, 1).toUpperCase()}${event.slice(1)}`;
  const callback = props[normalized];
  return typeof callback === "function" ? callback as (payload?: unknown) => void : undefined;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function appendUnique(children: HostChild[], child: HostChild): void {
  removeChild(children, child);
  children.push(child);
}

function insertBefore(children: HostChild[], child: HostChild, beforeChild: HostChild): void {
  removeChild(children, child);
  const index = children.indexOf(beforeChild);
  if (index < 0) children.push(child);
  else children.splice(index, 0, child);
}

function removeChild(children: HostChild[], child: HostChild): void {
  const index = children.indexOf(child);
  if (index >= 0) children.splice(index, 1);
}

function toSnapshot(container: HostContainer): NativeTreeSnapshot {
  return {
    revision: container.revision,
    children: container.children.map(toNodeSnapshot),
  };
}

function toNodeSnapshot(child: HostChild): NativeNodeSnapshot {
  if (isTextInstance(child)) {
    return {
      id: child.id,
      type: "#text",
      props: {},
      text: child.text,
      hidden: child.hidden,
      children: [],
    };
  }

  return {
    id: child.id,
    type: child.type,
    props: serializeProps(child.props),
    hidden: child.hidden,
    children: child.children.map(toNodeSnapshot),
  };
}

function isTextInstance(child: HostChild): child is HostTextInstance {
  return "text" in child;
}

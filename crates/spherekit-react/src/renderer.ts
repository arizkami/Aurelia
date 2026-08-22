import { createContext, type ReactNode } from "react";
import Reconciler from "react-reconciler";
import type { NativeBridge, NativeNodeSnapshot, NativeProps, NativeTreeSnapshot } from "./types";
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
}

const hostTransitionContext = createContext<null>(null);

/**
 * Creates a React 19 custom renderer for native SphereKit nodes.
 *
 * React mutates a private host tree during reconciliation. The complete,
 * serializable snapshot is handed to Rust only after the commit finishes, so
 * the native side never observes a half-built tree.
 */
export function createRoot(bridge: NativeBridge): ReactRoot {
  let nextId = 1;
  const container: HostContainer = { bridge, children: [], revision: 0 };

  const hostConfig = {
    supportsMutation: true,
    supportsPersistence: false,
    supportsHydration: false,
    isPrimaryRenderer: false,
    warnsIfNotActing: false,

    createInstance(type: string, props: NativeProps): HostInstance {
      return { id: nextId++, type, props, children: [], hidden: false };
    },

    createTextInstance(text: string): HostTextInstance {
      return { id: nextId++, text, hidden: false };
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
    detachDeletedInstance(): void {},

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
    setCurrentUpdatePriority(): void {},
    getCurrentUpdatePriority(): number {
      return 0;
    },
    resolveUpdatePriority(): number {
      return 0;
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

  return {
    render(element) {
      reconciler.updateContainerSync(element, root, null, null);
      reconciler.flushSyncWork();
    },
    unmount() {
      reconciler.updateContainerSync(null, root, null, null);
      reconciler.flushSyncWork();
    },
  };
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

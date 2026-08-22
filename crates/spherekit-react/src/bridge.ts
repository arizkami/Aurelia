import type { NativeBridge, NativeEvent, NativeTreeSnapshot, NativeValue } from "./types";

/** Protocol version shared by the TypeScript and Rust API Bridge. */
export const API_BRIDGE_VERSION = 1;

/** A transport adapter supplied by a WebView, FFI shim, process, or socket. */
export interface ApiBridgeTransport {
  send(message: string): void | Promise<void>;
  subscribe(listener: (message: string) => void): () => void;
  close?(): void;
}

/** Public request/event surface available to application code. */
export interface SphereKitApiBridge extends NativeBridge {
  invoke<TResult = NativeValue>(method: string, params?: NativeValue): Promise<TResult>;
  on<TPayload = NativeValue>(event: string, listener: (payload: TPayload) => void): () => void;
  close(): void;
}

interface PendingRequest {
  readonly resolve: (value: unknown) => void;
  readonly reject: (reason?: unknown) => void;
}

interface HelloMessage {
  readonly kind: "hello";
  readonly version: number;
  readonly runtime: string;
  readonly platform: string;
}

interface CommitMessage {
  readonly kind: "commit";
  readonly snapshot: NativeTreeSnapshot;
}

interface InvokeMessage {
  readonly kind: "invoke";
  readonly id: string;
  readonly method: string;
  readonly params: NativeValue;
}

interface ShutdownMessage {
  readonly kind: "shutdown";
}

interface ResponseMessage {
  readonly kind: "response";
  readonly id: string;
  readonly ok: boolean;
  readonly result?: NativeValue;
  readonly error?: { readonly code: string; readonly message: string; readonly data?: NativeValue };
}

interface EventMessage {
  readonly kind: "event";
  readonly event: string;
  readonly nodeId?: number;
  readonly payload?: NativeValue;
}

type WireMessage = ResponseMessage | EventMessage | { readonly kind: "ready"; readonly version: number };

/**
 * Creates a transport-neutral API bridge.
 *
 * `createRoot(createApiBridge(transport))` handles both committed React trees
 * and native events. The same bridge can also issue request/response API calls
 * without exposing WebView, IPC, or socket details to the component layer.
 */
export function createApiBridge(
  transport: ApiBridgeTransport,
  options: { readonly platform?: string } = {},
): SphereKitApiBridge {
  let nextRequestId = 1;
  let closed = false;
  const pending = new Map<string, PendingRequest>();
  const eventListeners = new Map<string, Set<(payload: unknown) => void>>();
  const nativeEventListeners = new Set<(event: NativeEvent) => void>();

  const unsubscribe = transport.subscribe((message) => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(message);
    } catch {
      return;
    }
    if (!isWireMessage(parsed)) return;

    if (parsed.kind === "response") {
      const request = pending.get(parsed.id);
      if (!request) return;
      pending.delete(parsed.id);
      if (parsed.ok) request.resolve(parsed.result);
      else {
        request.reject(
          new ApiBridgeError(
            parsed.error?.code ?? "native_error",
            parsed.error?.message ?? "Native API request failed",
            parsed.error?.data,
          ),
        );
      }
      return;
    }

    if (parsed.kind === "event") {
      const event: NativeEvent = {
        event: parsed.event,
        ...(parsed.nodeId === undefined ? {} : { nodeId: parsed.nodeId }),
        ...(parsed.payload === undefined ? {} : { payload: parsed.payload }),
      };
      for (const listener of nativeEventListeners) listener(event);
      for (const listener of eventListeners.get(parsed.event) ?? []) listener(parsed.payload);
    }
  });

  send({
    kind: "hello",
    version: API_BRIDGE_VERSION,
    runtime: "react",
    platform: options.platform ?? detectPlatform(),
  } satisfies HelloMessage);

  const bridge: SphereKitApiBridge = {
    commit(snapshot) {
      send({ kind: "commit", snapshot } satisfies CommitMessage);
    },

    onEvent(listener) {
      nativeEventListeners.add(listener);
      return () => nativeEventListeners.delete(listener);
    },

    invoke<TResult = NativeValue>(method: string, params: NativeValue = null): Promise<TResult> {
      if (closed) return Promise.reject(new ApiBridgeError("closed", "SphereKit API Bridge is closed"));
      const id = `request-${nextRequestId++}`;
      return new Promise<TResult>((resolve, reject) => {
        pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
        send({ kind: "invoke", id, method, params } satisfies InvokeMessage, id);
      });
    },

    on<TPayload = NativeValue>(event: string, listener: (payload: TPayload) => void): () => void {
      const listeners = eventListeners.get(event) ?? new Set();
      listeners.add(listener as (payload: unknown) => void);
      eventListeners.set(event, listeners);
      return () => {
        listeners.delete(listener as (payload: unknown) => void);
        if (listeners.size === 0) eventListeners.delete(event);
      };
    },

    close() {
      if (closed) return;
      closed = true;
      send({ kind: "shutdown" });
      unsubscribe();
      for (const request of pending.values()) {
        request.reject(new ApiBridgeError("closed", "SphereKit API Bridge is closed"));
      }
      pending.clear();
      transport.close?.();
    },
  };

  return bridge;

  function send(message: HelloMessage | CommitMessage | InvokeMessage | ShutdownMessage, requestId?: string): void {
    try {
      const result = transport.send(`${JSON.stringify(message)}\n`);
      if (result && typeof result.then === "function") {
        void result.catch((error: unknown) => rejectRequest(requestId, error));
      }
    } catch (error) {
      rejectRequest(requestId, error);
    }
  }

  function rejectRequest(requestId: string | undefined, error: unknown): void {
    if (!requestId) return;
    const request = pending.get(requestId);
    if (!request) return;
    pending.delete(requestId);
    request.reject(error);
  }
}

/** Error returned when a native API call fails. */
export class ApiBridgeError extends Error {
  readonly code: string;
  readonly data?: NativeValue;

  constructor(code: string, message: string, data?: NativeValue) {
    super(message);
    this.name = "ApiBridgeError";
    this.code = code;
    this.data = data;
  }
}

function isWireMessage(value: unknown): value is WireMessage {
  if (typeof value !== "object" || value === null || !("kind" in value)) return false;
  const kind = (value as { kind?: unknown }).kind;
  return kind === "ready" || kind === "response" || kind === "event";
}

function detectPlatform(): string {
  if (typeof navigator !== "undefined" && navigator.userAgent) {
    return navigator.userAgent.toLowerCase();
  }
  return "unknown";
}

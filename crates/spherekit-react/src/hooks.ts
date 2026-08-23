import { useCallback, useEffect, useRef, useState } from "react";
import type { SphereKitApiBridge } from "./bridge";
import type { NativeValue } from "./types";

/**
 * Subscribes to a named bridge event for the life of the component.
 *
 * The handler is held in a ref rather than listed as an effect dependency: a
 * handler written inline is a new function on every render, and depending on
 * it would tear down and re-register the native subscription on each one,
 * which loses any event the host emits in between.
 */
export function useNativeEvent<TPayload = NativeValue>(
  bridge: SphereKitApiBridge,
  event: string,
  handler: (payload: TPayload) => void,
): void {
  const handlerRef = useRef(handler);

  useEffect(() => {
    handlerRef.current = handler;
  }, [handler]);

  useEffect(
    () => bridge.on<TPayload>(event, (payload) => handlerRef.current(payload)),
    [bridge, event],
  );
}

/** The state of a bridge request driven by {@link useInvoke}. */
export interface InvokeState<TResult> {
  /** The last successful result, or `undefined` before one arrives. */
  readonly data: TResult | undefined;
  /** The failure that ended the most recent attempt, usually an `ApiBridgeError`. */
  readonly error: Error | undefined;
  /** Whether a request is in flight. */
  readonly loading: boolean;
  /** Runs the same request again, keeping the current `data` on screen meanwhile. */
  readonly refetch: () => void;
}

interface InternalInvokeState<TResult> {
  readonly key: string;
  readonly data: TResult | undefined;
  readonly error: Error | undefined;
  readonly loading: boolean;
}

/**
 * Calls a native API method and tracks the reply.
 *
 * `params` is compared by its JSON text, not by identity. An object literal
 * written in the call site is a fresh object every render, so an identity
 * comparison would re-issue the request forever; the JSON text is also exactly
 * what crosses the transport, so two params that serialise the same really are
 * the same request.
 *
 * A change of method or params clears `data`, because the old result answers a
 * question nobody asked any more. An explicit `refetch()` keeps it, so a list
 * refreshing in place does not blank out first.
 */
export function useInvoke<TResult = NativeValue>(
  bridge: SphereKitApiBridge,
  method: string,
  params: NativeValue = null,
): InvokeState<TResult> {
  const paramsKey = JSON.stringify(params ?? null);
  const key = `${method}:${paramsKey}`;
  const [nonce, setNonce] = useState(0);
  const [state, setState] = useState<InternalInvokeState<TResult>>({
    key,
    data: undefined,
    error: undefined,
    loading: true,
  });

  useEffect(() => {
    let cancelled = false;

    setState((previous) =>
      previous.key === key
        ? previous.loading
          ? previous
          : { ...previous, loading: true }
        : { key, data: undefined, error: undefined, loading: true },
    );

    bridge
      .invoke<TResult>(method, JSON.parse(paramsKey) as NativeValue)
      .then((data) => {
        if (!cancelled) setState({ key, data, error: undefined, loading: false });
      })
      .catch((error: unknown) => {
        if (!cancelled) setState({ key, data: undefined, error: toError(error), loading: false });
      });

    return () => {
      cancelled = true;
    };
  }, [bridge, method, paramsKey, key, nonce]);

  const refetch = useCallback(() => setNonce((previous) => previous + 1), []);

  return { data: state.data, error: state.error, loading: state.loading, refetch };
}

/**
 * Installs a stylesheet on mount and reports how many rules the host accepted.
 *
 * The count is the diagnostic: a stylesheet the native parser rejected comes
 * back as 0 rather than as a thrown error, because a malformed rule should
 * leave the application unstyled, not unmounted. The failure is still logged,
 * so it is not silent.
 */
export function useStylesheet(bridge: SphereKitApiBridge, css: string): number {
  const [rules, setRules] = useState(0);

  useEffect(() => {
    let cancelled = false;

    bridge
      .setStylesheet(css)
      .then((result) => {
        if (!cancelled) setRules(result.rules);
      })
      .catch((error: unknown) => {
        console.error("SphereKit stylesheet was rejected by the native host", error);
        if (!cancelled) setRules(0);
      });

    return () => {
      cancelled = true;
    };
  }, [bridge, css]);

  return rules;
}

function toError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

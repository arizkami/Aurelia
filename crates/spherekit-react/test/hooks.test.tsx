import { describe, expect, it } from "bun:test";
import {
  View,
  createApiBridge,
  createRoot,
  useInvoke,
  useNativeEvent,
  useStylesheet,
} from "../src/index";
import type { InvokeState, SphereKitApiBridge } from "../src/index";

/** Lets React's scheduled work and the transport's promises settle. */
async function settle(): Promise<void> {
  for (let turn = 0; turn < 4; turn += 1) {
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  }
}

interface TestBridge {
  readonly bridge: SphereKitApiBridge;
  readonly calls: string[];
  emit(event: string, payload?: unknown): void;
}

/** An API bridge whose host answers every invoke from a table. */
function testBridge(answers: Record<string, unknown>): TestBridge {
  const calls: string[] = [];
  let listener: ((message: string) => void) | undefined;

  const bridge = createApiBridge({
    send(message) {
      const request = JSON.parse(message) as { kind: string; id?: string; method?: string };
      if (request.kind !== "invoke" || !request.id || !request.method) return;
      calls.push(request.method);
      const answer = answers[request.method];
      listener?.(
        JSON.stringify(
          answer === undefined
            ? {
                kind: "response",
                id: request.id,
                ok: false,
                error: { code: "unknown_method", message: `no such method ${request.method}` },
              }
            : { kind: "response", id: request.id, ok: true, result: answer },
        ),
      );
    },
    subscribe(next) {
      listener = next;
      return () => {
        listener = undefined;
      };
    },
  });

  return {
    bridge,
    calls,
    emit(event, payload) {
      listener?.(JSON.stringify({ kind: "event", event, payload }));
    },
  };
}

describe("hooks", () => {
  it("reports a request as loading before it reports the result", async () => {
    const { bridge } = testBridge({ "app.status": { ready: true } });
    const states: InvokeState<{ ready: boolean }>[] = [];

    function App() {
      states.push(useInvoke<{ ready: boolean }>(bridge, "app.status"));
      return <View />;
    }

    createRoot({ commit: () => {} }).render(<App />);
    expect(states[0]?.loading).toBe(true);
    expect(states[0]?.data).toBeUndefined();

    await settle();
    expect(states.at(-1)?.loading).toBe(false);
    expect(states.at(-1)?.data).toEqual({ ready: true });
    expect(states.at(-1)?.error).toBeUndefined();
  });

  it("surfaces a failed request as an error instead of throwing during render", async () => {
    const { bridge } = testBridge({});
    const states: InvokeState<unknown>[] = [];

    function App() {
      states.push(useInvoke(bridge, "app.missing"));
      return <View />;
    }

    createRoot({ commit: () => {} }).render(<App />);
    await settle();

    expect(states.at(-1)?.loading).toBe(false);
    expect(states.at(-1)?.error?.message).toBe("no such method app.missing");
  });

  it("does not reissue a request when the params object is merely rebuilt", async () => {
    const { bridge, calls } = testBridge({ "app.tree": { nodes: 1 } });

    function App() {
      useInvoke(bridge, "app.tree", { depth: 2 });
      return <View />;
    }

    const root = createRoot({ commit: () => {} });
    root.render(<App />);
    await settle();
    root.render(<App />);
    await settle();

    expect(calls).toEqual(["app.tree"]);
  });

  it("reissues the same request on refetch and keeps the previous data meanwhile", async () => {
    const { bridge, calls } = testBridge({ "app.status": { ready: true } });
    const states: InvokeState<{ ready: boolean }>[] = [];

    function App() {
      const state = useInvoke<{ ready: boolean }>(bridge, "app.status");
      states.push(state);
      return <View />;
    }

    createRoot({ commit: () => {} }).render(<App />);
    await settle();
    states.at(-1)?.refetch();
    await settle();

    expect(calls).toEqual(["app.status", "app.status"]);
    expect(states.every((state) => state.loading || state.data !== undefined)).toBe(true);
    expect(states.at(-1)?.data).toEqual({ ready: true });
  });

  it("stops listening for a native event once the component unmounts", async () => {
    const { bridge, emit } = testBridge({});
    const seen: unknown[] = [];

    function App() {
      useNativeEvent(bridge, "transport.play", (payload) => seen.push(payload));
      return <View />;
    }

    const root = createRoot({ commit: () => {} });
    root.render(<App />);
    await settle();
    emit("transport.play", { bar: 1 });
    root.unmount();
    emit("transport.play", { bar: 2 });

    expect(seen).toEqual([{ bar: 1 }]);
  });

  it("keeps delivering events to the latest handler without resubscribing", async () => {
    const { bridge, emit } = testBridge({});
    const seen: string[] = [];
    let subscriptions = 0;
    const counted: SphereKitApiBridge = {
      ...bridge,
      on(event, listener) {
        subscriptions += 1;
        return bridge.on(event, listener);
      },
    };
    const root = createRoot({ commit: () => {} });

    function App({ tag }: { readonly tag: string }) {
      useNativeEvent(counted, "transport.play", () => seen.push(tag));
      return <View />;
    }

    root.render(<App tag="first" />);
    await settle();
    root.render(<App tag="second" />);
    await settle();
    emit("transport.play");

    expect(seen).toEqual(["second"]);
    expect(subscriptions).toBe(1);
  });

  it("reports how many rules the host accepted from a stylesheet", async () => {
    const { bridge, calls } = testBridge({ "spherekit.setStylesheet": { rules: 3 } });
    const counts: number[] = [];

    function App() {
      counts.push(useStylesheet(bridge, ".panel { gap: 8px; }"));
      return <View />;
    }

    createRoot({ commit: () => {} }).render(<App />);
    await settle();

    expect(calls).toEqual(["spherekit.setStylesheet"]);
    expect(counts[0]).toBe(0);
    expect(counts.at(-1)).toBe(3);
  });
});

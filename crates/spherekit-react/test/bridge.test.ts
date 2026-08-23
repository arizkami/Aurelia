import { describe, expect, it } from "bun:test";
import { ApiBridgeError, createApiBridge } from "../src/index";
import type { ApiBridgeTransport } from "../src/index";

/** A transport that answers nothing until the test decides to. */
function manualTransport(): ApiBridgeTransport & {
  readonly sent: string[];
  deliver(message: unknown): void;
  readonly closed: () => boolean;
} {
  const sent: string[] = [];
  let listener: ((message: string) => void) | undefined;
  let closed = false;

  return {
    sent,
    send(message) {
      sent.push(message);
    },
    subscribe(next) {
      listener = next;
      return () => {
        listener = undefined;
      };
    },
    close() {
      closed = true;
    },
    deliver(message: unknown) {
      listener?.(JSON.stringify(message));
    },
    closed: () => closed,
  };
}

function lastRequestId(sent: readonly string[]): string {
  for (let index = sent.length - 1; index >= 0; index -= 1) {
    const message = JSON.parse(sent[index] ?? "{}") as { kind?: string; id?: string };
    if (message.kind === "invoke" && message.id) return message.id;
  }
  throw new Error("no invoke was sent");
}

describe("API bridge", () => {
  it("rejects with the code and message the native side reported", async () => {
    const transport = manualTransport();
    const bridge = createApiBridge(transport);

    const pending = bridge.invoke("app.open");
    transport.deliver({
      kind: "response",
      id: lastRequestId(transport.sent),
      ok: false,
      error: { code: "not_found", message: "no such window", data: { window: 4 } },
    });

    const error = (await pending.catch((reason: unknown) => reason)) as ApiBridgeError;
    expect(error).toBeInstanceOf(ApiBridgeError);
    expect(error.code).toBe("not_found");
    expect(error.message).toBe("no such window");
    expect(error.data).toEqual({ window: 4 });
  });

  it("rejects every in-flight request when the bridge is closed", async () => {
    const transport = manualTransport();
    const bridge = createApiBridge(transport);

    const first = bridge.invoke("app.status");
    const second = bridge.invoke("app.tree");
    bridge.close();

    for (const pending of [first, second]) {
      const error = (await pending.catch((reason: unknown) => reason)) as ApiBridgeError;
      expect(error).toBeInstanceOf(ApiBridgeError);
      expect(error.code).toBe("closed");
    }
    expect(JSON.parse(transport.sent.at(-1) ?? "{}").kind).toBe("shutdown");
    expect(transport.closed()).toBe(true);
  });

  it("refuses a request issued after close instead of leaving it pending forever", async () => {
    const bridge = createApiBridge(manualTransport());
    bridge.close();

    const error = (await bridge.invoke("app.status").catch((reason: unknown) => reason)) as ApiBridgeError;
    expect(error.code).toBe("closed");
  });

  it("rejects the request whose transport send threw", async () => {
    const bridge = createApiBridge({
      send(message) {
        if (JSON.parse(message).kind === "invoke") throw new Error("pipe is gone");
      },
      subscribe() {
        return () => {};
      },
    });

    const error = (await bridge.invoke("app.status").catch((reason: unknown) => reason)) as Error;
    expect(error.message).toBe("pipe is gone");
  });

  it("delivers named events to their own listeners only", () => {
    const transport = manualTransport();
    const bridge = createApiBridge(transport);
    const ready: unknown[] = [];
    const closedEvents: unknown[] = [];

    const stop = bridge.on("appReady", (payload) => ready.push(payload));
    bridge.on("appClosed", (payload) => closedEvents.push(payload));

    transport.deliver({ kind: "event", event: "appReady", payload: { version: 1 } });
    stop();
    transport.deliver({ kind: "event", event: "appReady", payload: { version: 2 } });

    expect(ready).toEqual([{ version: 1 }]);
    expect(closedEvents).toEqual([]);
  });

  it("ignores frames that are not part of the protocol", () => {
    const transport = manualTransport();
    const bridge = createApiBridge(transport);
    let seen = 0;

    bridge.on("appReady", () => seen++);
    transport.deliver({ kind: "nonsense", event: "appReady" });
    transport.deliver("not json at all");

    expect(seen).toBe(0);
  });
});

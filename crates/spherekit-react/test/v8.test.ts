import { describe, expect, it } from "bun:test";
import { createApiBridge, createV8Transport, isV8Host } from "../src/index";

interface FakeHost {
  __spherekitSend?: (frame: string) => string;
  __spherekitReceive?: (frame: string) => void;
}

/** A stand-in for the isolate globals the Rust host installs. */
function fakeIsolate(
  reply: (frame: string) => readonly string[] = () => [],
): typeof globalThis & FakeHost & { readonly sent: string[] } {
  const sent: string[] = [];
  const host = {
    sent,
    __spherekitSend(frame: string) {
      sent.push(frame);
      return JSON.stringify(reply(frame));
    },
  };
  return host as unknown as typeof globalThis & FakeHost & { readonly sent: string[] };
}

describe("V8 transport", () => {
  it("recognises an isolate by the globals the embedder installed", () => {
    expect(isV8Host(fakeIsolate())).toBe(true);
    expect(isV8Host({} as typeof globalThis)).toBe(false);
  });

  it("names the global the embedder failed to install", () => {
    expect(() => createV8Transport({} as typeof globalThis)).toThrow(/__spherekitSend/);
  });

  it("feeds every reply frame of a send straight to the subscriber", () => {
    const host = fakeIsolate((frame) =>
      JSON.parse(frame).kind === "invoke"
        ? [JSON.stringify({ kind: "response", id: "request-1", ok: true, result: 7 })]
        : [],
    );
    const received: string[] = [];
    const transport = createV8Transport(host);

    transport.subscribe((message) => received.push(message));
    transport.send(`${JSON.stringify({ kind: "invoke", id: "request-1" })}\n`);

    expect(received).toEqual([
      JSON.stringify({ kind: "response", id: "request-1", ok: true, result: 7 }),
    ]);
  });

  it("splits a reply frame that carries several newline-delimited messages", () => {
    const host = fakeIsolate(() => [
      `${JSON.stringify({ kind: "ready", version: 1 })}\n${JSON.stringify({ kind: "event", event: "appReady" })}\n`,
    ]);
    const received: string[] = [];
    const transport = createV8Transport(host);

    transport.subscribe((message) => received.push(message));
    transport.send("hello\n");

    expect(received.map((message) => JSON.parse(message).kind)).toEqual(["ready", "event"]);
  });

  it("does not lose replies that arrive before anything subscribed", () => {
    const host = fakeIsolate(() => [JSON.stringify({ kind: "ready", version: 1 })]);
    const transport = createV8Transport(host);

    transport.send("hello\n");
    const received: string[] = [];
    transport.subscribe((message) => received.push(message));

    expect(received).toHaveLength(1);
  });

  it("installs the receive global so the host can push events in", () => {
    const host = fakeIsolate();
    const received: string[] = [];
    const transport = createV8Transport(host);

    const unsubscribe = transport.subscribe((message) => received.push(message));
    expect(typeof host.__spherekitReceive).toBe("function");
    host.__spherekitReceive?.(JSON.stringify({ kind: "event", event: "press", nodeId: 3 }));

    unsubscribe();
    expect(host.__spherekitReceive).toBeUndefined();
    expect(JSON.parse(received[0] ?? "{}").event).toBe("press");
  });

  it("reports a reply that is not an array of frames as a protocol error", () => {
    const host = fakeIsolate();
    host.__spherekitSend = () => "{}";
    const transport = createV8Transport(host);

    expect(() => transport.send("hello\n")).toThrow(/array of frame strings/);
  });

  it("carries a whole invoke round trip for an API bridge in the isolate", async () => {
    const host = fakeIsolate((frame) => {
      const message = JSON.parse(frame) as { kind: string; id?: string; method?: string };
      if (message.kind !== "invoke" || !message.id) return [];
      return [
        JSON.stringify({
          kind: "response",
          id: message.id,
          ok: true,
          result: { method: message.method },
        }),
      ];
    });

    const bridge = createApiBridge(createV8Transport(host), { platform: "spherekit-v8" });
    const result = await bridge.invoke<{ method: string }>("app.status");

    expect(result.method).toBe("app.status");
    expect(JSON.parse(host.sent[0] ?? "{}").platform).toBe("spherekit-v8");
    bridge.close();
  });

  it("stops talking to the host once the transport is closed", () => {
    const host = fakeIsolate();
    const transport = createV8Transport(host);

    transport.subscribe(() => {});
    transport.close?.();
    transport.send("hello\n");

    expect(host.sent).toHaveLength(0);
    expect(host.__spherekitReceive).toBeUndefined();
  });
});

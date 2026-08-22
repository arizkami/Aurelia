import { describe, expect, it } from "bun:test";
import { Button, Slider, Text, View, createApiBridge, createRoot, cx, jsonBridge } from "../src/index";
import type { NativeEvent, NativeTreeSnapshot } from "../src/types";

describe("SphereKit React renderer", () => {
  it("commits the React Native-like JSX tree after render", () => {
    const snapshots: NativeTreeSnapshot[] = [];
    const root = createRoot({ commit: (snapshot) => snapshots.push(snapshot) });

    root.render(
      <View style={{ flexDirection: "column", gap: 8 }}>
        <Text>Hello native</Text>
        <Button title="Play" onPress={() => {}} />
        <Slider value={0.5} minimumValue={0} maximumValue={1} step={0.01} />
      </View>,
    );

    const snapshot = snapshots.at(-1);
    expect(snapshot).toBeDefined();
    expect(snapshot?.revision).toBe(1);
    expect(snapshot?.children[0]?.type).toBe("view");
    expect(snapshot?.children[0]?.children.map((child) => child.type)).toEqual([
      "text",
      "button",
      "slider",
    ]);
    expect(snapshot?.children[0]?.children[1]?.props.title).toBe("Play");
  });

  it("can serialize a commit for the Rust bridge", () => {
    let json = "";
    const root = createRoot(jsonBridge((snapshot) => {
      json = snapshot;
    }));

    root.render(<Text>Rust boundary</Text>);

    const snapshot = JSON.parse(json) as NativeTreeSnapshot;
    expect(snapshot.children[0]?.type).toBe("text");
    expect(snapshot.children[0]?.children[0]?.text).toBe("Rust boundary");
  });

  it("keeps CSS classes and inline properties in the native snapshot", () => {
    const snapshots: NativeTreeSnapshot[] = [];
    const root = createRoot({ commit: (snapshot) => snapshots.push(snapshot) });

    root.render(
      <View id="app" className={cx("panel", false, "primary")} style={{ gap: 8 }} />,
    );

    expect(snapshots.at(-1)?.children[0]?.props).toMatchObject({
      id: "app",
      className: "panel primary",
      style: { gap: 8 },
    });
  });

  it("routes native events back to React callbacks by node id", () => {
    const listeners: Array<(event: NativeEvent) => void> = [];
    let pressed = 0;
    let buttonId = 0;
    const root = createRoot({
      commit(snapshot) {
        buttonId = snapshot.children[0]?.id ?? 0;
      },
      onEvent(listener) {
        listeners.push(listener);
        return () => {};
      },
    });

    root.render(<Button title="Play" onPress={() => pressed++} />);
    root.dispatch({ event: "press", nodeId: buttonId });
    listeners[0]?.({ event: "press", nodeId: buttonId });

    expect(pressed).toBe(2);
  });

  it("supports request/response API calls on the same bridge", async () => {
    let receive: ((message: string) => void) | undefined;
    const sent: string[] = [];
    const bridge = createApiBridge({
      send(message) {
        sent.push(message);
        const request = JSON.parse(message) as { kind: string; id?: string };
        if (request.kind === "invoke" && request.id) {
          receive?.(JSON.stringify({ kind: "response", id: request.id, ok: true, result: { ready: true } }));
        }
      },
      subscribe(listener) {
        receive = listener;
        return () => {};
      },
    });

    const result = await bridge.invoke<{ ready: boolean }>("app.status");
    expect(result.ready).toBe(true);
    expect(sent[0]?.endsWith("\n")).toBe(true);
    expect(JSON.parse(sent[0] ?? "{}").kind).toBe("hello");
    expect(JSON.parse(sent[1] ?? "{}").kind).toBe("invoke");

    await bridge.setStylesheet(".panel { gap: 8px; }");
    expect(JSON.parse(sent[2] ?? "{}").method).toBe("spherekit.setStylesheet");
  });
});

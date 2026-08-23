import { describe, expect, it } from "bun:test";
import {
  Button,
  MenuItem,
  Native,
  Slider,
  Text,
  TextField,
  Toggle,
  View,
  createApiBridge,
  createRoot,
  cx,
  jsonBridge,
} from "../src/index";
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

  it("hands a toggle change to onChange as the boolean it is", () => {
    let checked: unknown;
    let nodeId = 0;
    const root = createRoot({
      commit(snapshot) {
        nodeId = snapshot.children[0]?.id ?? 0;
      },
    });

    root.render(<Toggle checked={false} onChange={(next) => (checked = next)} />);
    root.dispatch({ event: "change", nodeId, payload: { checked: true } });

    expect(checked).toBe(true);
  });

  it("hands a field submit to onSubmit as the text it committed", () => {
    let submitted: unknown;
    let nodeId = 0;
    const root = createRoot({
      commit(snapshot) {
        nodeId = snapshot.children[0]?.id ?? 0;
      },
    });

    root.render(<TextField value="" onSubmit={(text) => (submitted = text)} />);
    root.dispatch({ event: "submit", nodeId, payload: { text: "kick.wav" } });

    expect(submitted).toBe("kick.wav");
  });

  it("calls onSelect with no argument, because a menu row carries no value", () => {
    const received: unknown[] = [];
    let nodeId = 0;
    const root = createRoot({
      commit(snapshot) {
        nodeId = snapshot.children[0]?.id ?? 0;
      },
    });

    root.render(<MenuItem label="Copy" onSelect={(...args: unknown[]) => received.push(args)} />);
    root.dispatch({ event: "select", nodeId, payload: { label: "Copy" } });

    expect(received).toEqual([[]]);
  });

  it("normalises every native event name to the prop React declares", () => {
    // The Rust host sends bare names; `lower.rs` pins the sending half in
    // `the_event_names_are_the_five_the_typescript_dispatcher_normalises`.
    const called: string[] = [];
    let nodeId = 0;
    const root = createRoot({
      commit(snapshot) {
        nodeId = snapshot.children[0]?.id ?? 0;
      },
    });

    root.render(
      <Native
        type="view"
        onPress={() => called.push("onPress")}
        onValueChange={() => called.push("onValueChange")}
        onChange={() => called.push("onChange")}
        onSelect={() => called.push("onSelect")}
        onSubmit={() => called.push("onSubmit")}
      />,
    );
    for (const event of ["press", "valueChange", "change", "select", "submit"]) {
      root.dispatch({ event, nodeId, payload: {} });
    }

    expect(called).toEqual(["onPress", "onValueChange", "onChange", "onSelect", "onSubmit"]);
  });

  it("hands an embedder's own event its payload whole, since no rule can unwrap it", () => {
    // A type registered through `ReactHost::register_host_type` emits names
    // this package has never heard of, so the dispatcher must not guess which
    // key of the payload the handler wanted.
    let reading: unknown;
    let nodeId = 0;
    const root = createRoot({
      commit(snapshot) {
        nodeId = snapshot.children[0]?.id ?? 0;
      },
    });

    root.render(<Native type="gauge" onReading={(next: unknown) => (reading = next)} />);
    root.dispatch({ event: "reading", nodeId, payload: { peak: 12, rms: 4 } });

    expect(reading).toEqual({ peak: 12, rms: 4 });
  });

  it("ignores an event aimed at a node that has already been removed", () => {
    let pressed = 0;
    let nodeId = 0;
    const root = createRoot({
      commit(snapshot) {
        nodeId = snapshot.children[0]?.id ?? nodeId;
      },
    });

    root.render(<Button title="Play" onPress={() => pressed++} />);
    root.render(<View />);
    root.dispatch({ event: "press", nodeId });

    expect(pressed).toBe(0);
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

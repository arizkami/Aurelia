import { describe, expect, it } from "bun:test";
import { Button, Slider, Text, View, createRoot, jsonBridge } from "../src/index";
import type { NativeTreeSnapshot } from "../src/types";

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
});

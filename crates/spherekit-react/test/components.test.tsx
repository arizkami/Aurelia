import { describe, expect, it } from "bun:test";
import type { ReactNode } from "react";
import {
  Avatar,
  Button,
  Checkbox,
  Fader,
  Knob,
  MenuItem,
  Native,
  Panel,
  Progress,
  ScrollView,
  Separator,
  Slider,
  Text,
  TextField,
  Toggle,
  View,
  createRoot,
} from "../src/index";
import type { NativeNodeSnapshot, NativeTreeSnapshot } from "../src/types";

function commitOf(element: ReactNode): NativeTreeSnapshot {
  const commits: NativeTreeSnapshot[] = [];
  const root = createRoot({ commit: (snapshot) => commits.push(snapshot) });
  root.render(element);
  const snapshot = commits[0];
  if (!snapshot) throw new Error("the renderer produced no commit");
  return snapshot;
}

function nodeOf(element: ReactNode): NativeNodeSnapshot {
  const node = commitOf(element).children[0];
  if (!node) throw new Error("the commit had no root node");
  return node;
}

describe("native components", () => {
  it("maps every component to the host type the Rust registry expects", () => {
    const snapshot = commitOf(
      <View>
        <Text>label</Text>
        <Button title="Play" />
        <Slider value={0.5} />
        <Knob value={0.5} />
        <Fader value={0.5} />
        <ScrollView />
        <Toggle checked />
        <Checkbox checked={false} />
        <Progress value={0.5} />
        <Separator />
        <Panel title="Mixer" />
        <TextField value="kick" />
        <Avatar name="Ada Lovelace" />
        <MenuItem label="Copy" />
      </View>,
    );

    expect(snapshot.children[0]?.children.map((child) => child.type)).toEqual([
      "text",
      "button",
      "slider",
      "knob",
      "fader",
      "scroll-view",
      "toggle",
      "checkbox",
      "progress",
      "separator",
      "panel",
      "text-field",
      "avatar",
      "menu-item",
    ]);
  });

  it("keeps event handlers on the TypeScript side of the boundary", () => {
    const node = nodeOf(<Button title="Play" disabled onPress={() => {}} />);

    expect(node.props).toEqual({ title: "Play", disabled: true });
  });

  it("serialises the whole range of a slider", () => {
    const node = nodeOf(
      <Slider value={0.25} minimumValue={-1} maximumValue={1} step={0.05} disabled />,
    );

    expect(node.props).toEqual({
      value: 0.25,
      minimumValue: -1,
      maximumValue: 1,
      step: 0.05,
      disabled: true,
    });
  });

  it("gives a knob and a fader the same props as a slider", () => {
    const knob = nodeOf(<Knob value={0.75} minimumValue={0} maximumValue={1} />);
    const fader = nodeOf(<Fader value={0.75} minimumValue={0} maximumValue={1} />);

    expect(knob.type).toBe("knob");
    expect(fader.type).toBe("fader");
    expect(knob.props).toEqual(fader.props);
  });

  it("serialises a toggle and a checkbox to the same state", () => {
    const toggle = nodeOf(<Toggle checked label="Monitor" onChange={() => {}} />);
    const checkbox = nodeOf(<Checkbox checked label="Monitor" onChange={() => {}} />);

    expect(toggle.props).toEqual({ checked: true, label: "Monitor" });
    expect(checkbox.props).toEqual(toggle.props);
  });

  it("carries either a progress fraction or the indeterminate flag", () => {
    expect(nodeOf(<Progress value={0.4} thickness={4} />).props).toEqual({
      value: 0.4,
      thickness: 4,
    });
    expect(nodeOf(<Progress indeterminate />).props).toEqual({ indeterminate: true });
  });

  it("serialises a masked text field without its text callbacks", () => {
    const node = nodeOf(
      <TextField
        value="hunter2"
        placeholder="Password"
        mask
        onChange={() => {}}
        onSubmit={() => {}}
      />,
    );

    expect(node.type).toBe("text-field");
    expect(node.props).toEqual({ value: "hunter2", placeholder: "Password", mask: true });
  });

  it("serialises an avatar's presence as the host's spelling of it", () => {
    const node = nodeOf(<Avatar name="Ada Lovelace" initials="AL" size={48} presence="busy" />);

    expect(node.props).toEqual({
      name: "Ada Lovelace",
      initials: "AL",
      size: 48,
      presence: "busy",
    });
  });

  it("uses a menu item's label as its row text when it has no children", () => {
    const node = nodeOf(<MenuItem label="Duplicate" shortcut="Ctrl+D" danger />);

    expect(node.props).toEqual({ label: "Duplicate", shortcut: "Ctrl+D", danger: true });
    expect(node.children[0]?.text).toBe("Duplicate");
  });

  it("prefers explicit children over the label fallback", () => {
    const node = nodeOf(
      <MenuItem label="Duplicate">
        <Text>Duplicate track</Text>
      </MenuItem>,
    );

    expect(node.children[0]?.type).toBe("text");
    expect(node.props.label).toBe("Duplicate");
  });

  it("serialises both scroll axes and a panel title", () => {
    expect(nodeOf(<ScrollView horizontal both />).props).toEqual({
      horizontal: true,
      both: true,
    });
    expect(nodeOf(<Panel title="Mixer" />).props).toEqual({ title: "Mixer" });
    expect(nodeOf(<Separator vertical />).props).toEqual({ vertical: true });
  });

  it("reaches an unwrapped host type through the escape hatch", () => {
    const node = nodeOf(<Native type="context-menu" open />);

    expect(node.type).toBe("context-menu");
    expect(node.props).toEqual({ open: true });
  });
});

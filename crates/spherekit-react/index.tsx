import {
  Avatar,
  Button,
  Checkbox,
  Fader,
  Knob,
  MenuItem,
  Panel,
  Progress,
  ScrollView,
  Separator,
  Slider,
  Text,
  TextField,
  Toggle,
  View,
  createApiBridge,
  createRoot,
  createV8Transport,
  isV8Host,
  jsonBridge,
  stylesheet,
} from "./src/index";
import type { NativeBridge } from "./src/index";

// The smoke entry doubles as the V8 bundle entry: in the isolate it talks to
// the host over the in-process transport, and everywhere else it prints the
// commit it would have sent. Keeping one entry for both means the bundle that
// is checked by `bun run` is the same bundle that is loaded by the engine.
const commits: string[] = [];
const bridge: NativeBridge = isV8Host()
  ? createApiBridge(createV8Transport())
  : jsonBridge((snapshot) => commits.push(snapshot));

const root = createRoot(bridge);

const sheet = stylesheet({
  ".mixer": { display: "flex", flexDirection: "column", gap: 8, padding: 12 },
  ".mixer .strip": { display: "flex", gap: 12, flexGrow: 1 },
  ".mixer .label": { fontWeight: 600, lineHeight: 1.4, opacity: 0.8 },
});

root.render(
  <View id="smoke-root" className="mixer" style={{ gap: 8, opacity: 1 }}>
    <Panel title="Channel 1">
      <Text className="label">SphereKit React renderer ready</Text>
      <Separator />
      <View className="strip">
        <Knob value={0.25} minimumValue={0} maximumValue={1} onValueChange={() => {}} />
        <Fader value={0.75} step={0.01} onValueChange={() => {}} />
        <Slider value={0.5} minimumValue={0} maximumValue={1} step={0.01} />
      </View>
      <Toggle checked label="Monitor" onChange={() => {}} />
      <Checkbox checked={false} label="Solo" onChange={() => {}} />
      <Progress value={0.4} thickness={4} />
      <Progress indeterminate />
      <TextField value="kick.wav" placeholder="Sample" onChange={() => {}} onSubmit={() => {}} />
      <Avatar name="Ada Lovelace" size={32} presence="online" />
      <MenuItem label="Duplicate" shortcut="Ctrl+D" onSelect={() => {}} />
      <Button title="Play" onPress={() => {}} />
    </Panel>
    <ScrollView horizontal>
      <Text>Scrollable strip</Text>
    </ScrollView>
  </View>,
);

console.log(sheet);
console.log(commits.at(-1));
root.unmount();

import { createRoot, jsonBridge, Text, View } from "./src/index";

const commits: string[] = [];
const root = createRoot(jsonBridge((snapshot) => commits.push(snapshot)));

root.render(
  <View testID="smoke-root">
    <Text>SphereKit React renderer ready</Text>
  </View>,
);

console.log(commits.at(-1));
root.unmount();

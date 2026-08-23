import { useState } from "react";
import { Button, Slider, Text, View } from "@spherekit/react";

export function App() {
  const [volume, setVolume] = useState(0.65);

  return (
    <View
      style={{
        flexDirection: "column",
        gap: 12,
        padding: 20,
        backgroundColor: "#202124",
        borderRadius: 12,
      }}
    >
      <Text style={{ fontSize: 22 }}>SphereKit React app</Text>
      <Text style={{ opacity: 0.7 }}>Native controls, React state.</Text>
      <Slider
        value={volume}
        minimumValue={0}
        maximumValue={1}
        step={0.01}
        onValueChange={setVolume}
      />
      <Button title={`Volume ${Math.round(volume * 100)}%`} onPress={() => setVolume(0.65)} />
    </View>
  );
}

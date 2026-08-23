import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Button,
  Knob,
  Progress,
  Separator,
  Slider,
  Text,
  Toggle,
  View,
  cx,
  type SphereKitApiBridge,
} from "@spherekit/react";

/** Everything the app needs from the native side. */
export interface AppProps {
  /** The bridge this root was mounted on, for `invoke` and native events. */
  readonly bridge: SphereKitApiBridge;
}

/**
 * The demo application.
 *
 * Deliberately ordinary React: hooks, derived values, callbacks. Nothing here
 * knows it is driving a GPU renderer rather than a DOM — that is the point.
 * The only SphereKit-specific concepts are the host components and the
 * `bridge`, and the bridge is only used for the two things the DOM has no
 * equivalent for: calling a native method, and receiving a native event.
 */
export function App({ bridge }: AppProps) {
  const [gain, setGain] = useState(0.72);
  const [tone, setTone] = useState(0.4);
  const [live, setLive] = useState(true);
  const [presses, setPresses] = useState(0);
  const [frames, setFrames] = useState(0);

  // A native event that no React component raised: the Rust side emits a
  // `frame` event every render, and the header counts them. This is the
  // native -> JavaScript direction of the bridge.
  useEffect(() => bridge.on<{ count: number }>("frame", (p) => setFrames(p.count)), [bridge]);

  const decibels = useMemo(() => (gain <= 0 ? "-inf" : (20 * Math.log10(gain)).toFixed(1)), [gain]);

  const reset = useCallback(() => {
    setGain(0.72);
    setTone(0.4);
    setPresses((n) => n + 1);
  }, []);

  return (
    <View className="root">
      <View className="header">
        <View>
          <Text className="title">SphereKit React</Text>
          <Text className="subtitle">React 19 in V8, rendered on the GPU</Text>
        </View>
        <Text className="subtitle">{`frame ${frames}`}</Text>
      </View>

      <View className={cx("card", "telemetry")}>
        <View className="row">
          <Text className="label">OUTPUT GAIN</Text>
          <View className="grow" />
          <View className="meter">
            <Text className="value">{`${decibels} dB`}</Text>
          </View>
        </View>
        <Slider
          value={gain}
          minimumValue={0}
          maximumValue={1}
          step={0.001}
          onValueChange={setGain}
        />
        <Progress value={gain} />
      </View>

      <View className="card">
        <Text className="label">TONE</Text>
        <View className="row">
          <Knob value={tone} minimumValue={0} maximumValue={1} onValueChange={setTone} />
          <Text className="value">{tone.toFixed(2)}</Text>
          <View className="grow" />
          <Toggle checked={live} label="Live" onChange={setLive} />
        </View>
      </View>

      <Separator />

      <View className="row">
        <Button className="primary" title="Reset" onPress={reset} />
        <Button title={`Pressed ${presses}`} disabled={presses === 0} onPress={() => {}} />
        <View className="grow" />
        <Button
          className="danger"
          title="Quit"
          onPress={() => {
            void bridge.invoke("app.quit");
          }}
        />
      </View>
    </View>
  );
}

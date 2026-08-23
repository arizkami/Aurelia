import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Button,
  Native,
  ScrollView,
  Slider,
  Text,
  View,
  cx,
  type SphereKitApiBridge,
} from "@spherekit/react";
import { FolderTree, type FolderEntry } from "./FolderTree";
import { IDLE_STATE, formatTime, type LibraryTrack, type PlayerState } from "./types";

/** Everything the player needs from the native side. */
export interface AppProps {
  /** The bridge this root was mounted on. */
  readonly bridge: SphereKitApiBridge;
}

/**
 * The player.
 *
 * Holds no playback state of its own. Rust owns what is playing and pushes a
 * `player.state` snapshot every frame; this renders that and calls back. The
 * alternative — mirroring position and playing-ness in React and reconciling
 * the two — is how a transport ends up disagreeing with the audio.
 */
export function App({ bridge }: AppProps) {
  const [state, setState] = useState<PlayerState>(IDLE_STATE);
  const [library, setLibrary] = useState<readonly LibraryTrack[]>([]);
  /** Set while the user is dragging, so the pushed position cannot fight them. */
  const [scrubbing, setScrubbing] = useState<number | null>(null);
  /** Which sidebar pane is showing. */
  const [pane, setPane] = useState<"tracks" | "folders">("tracks");
  /** The folder the loaded library came from, highlighted in the tree. */
  const [openPath, setOpenPath] = useState<string | null>(null);
  /** Every drive on the machine, and the one the tree is showing. */
  const [drives, setDrives] = useState<readonly FolderEntry[]>([]);
  const [drive, setDrive] = useState<FolderEntry | null>(null);

  useEffect(() => bridge.on<PlayerState>("player.state", setState), [bridge]);

  useEffect(() => {
    void bridge.invoke<readonly LibraryTrack[]>("player.library").then(setLibrary);
  }, [bridge]);

  useEffect(() => {
    void bridge.invoke<readonly FolderEntry[]>("browser.roots").then(setDrives);
  }, [bridge]);

  const current = state.index === null ? undefined : library[state.index];
  const duration = state.duration ?? 0;
  const position = scrubbing ?? state.position;

  const openFolder = useCallback(
    (path: string) => {
      void bridge.invoke<readonly LibraryTrack[]>("browser.open", { path }).then((next) => {
        setLibrary(next);
        setOpenPath(path);
        // Switch back to the list: the point of choosing a folder is to see
        // what is in it, and leaving the tree up hides the result.
        setPane("tracks");
      });
    },
    [bridge],
  );

  const playFile = useCallback(
    (folder: string, file: string) => {
      void bridge
        .invoke<readonly LibraryTrack[]>("browser.playFile", { folder, file })
        .then((next) => {
          setLibrary(next);
          setOpenPath(folder);
          setPane("tracks");
        });
    },
    [bridge],
  );

  const select = useCallback(
    (index: number) => {
      void bridge.invoke("player.select", { index });
    },
    [bridge],
  );

  const commitSeek = useCallback(
    (seconds: number) => {
      setScrubbing(null);
      void bridge.invoke("player.seek", { seconds });
    },
    [bridge],
  );

  const rows = useMemo(
    () =>
      library.map((track, index) => (
        <View
          key={`${track.album}/${track.title}/${index}`}
          className={cx("track", index === state.index && "playing")}
          onPress={() => select(index)}
        >
          <Text className="track-index">{String(index + 1)}</Text>
          <Text className="track-title">{track.title}</Text>
          <Text className="track-album">{track.album}</Text>
        </View>
      )),
    [library, state.index, select],
  );

  return (
    <View className="app">
      <View className="body">
        <View className="sidebar">
          {/*
            The drive strip is always visible, in both panes. Putting drive
            switching behind the FOLDERS tab meant the one control for "look
            somewhere else entirely" was the hardest one to find.
          */}
          <View className="drives">
            {drives.map((entry) => (
              <View
                key={entry.path}
                className={cx("drive", drive?.path === entry.path && "active")}
                onPress={() => {
                  setDrive(entry);
                  setPane("folders");
                }}
              >
                <Text className="drive-label">{entry.name.replace(/[:\/]/g, "")}</Text>
              </View>
            ))}
          </View>

          <View className="sidebar-header">
            <View
              className={cx("tab", pane === "tracks" && "active")}
              onPress={() => setPane("tracks")}
            >
              <Text className="tab-label">{`TRACKS ${library.length}`}</Text>
            </View>
            <View
              className={cx("tab", pane === "folders" && "active")}
              onPress={() => setPane("folders")}
            >
              <Text className="tab-label">FOLDERS</Text>
            </View>
          </View>
          <ScrollView className="tracks">
            {pane === "tracks" ? (
              rows
            ) : (
              <FolderTree
                bridge={bridge}
                drive={drive}
                openPath={openPath}
                onOpen={openFolder}
                onPlayFile={playFile}
              />
            )}
          </ScrollView>
        </View>

        <View className="stage">
          {library.length === 0 ? (
            <View className="empty">
              <Text className="empty-title">Nothing to play</Text>
              <Text className="empty-hint">
                {state.error ?? "Start the player with a folder: musicplayer <path>"}
              </Text>
            </View>
          ) : (
            <>
              <View className="now-playing">
                <Text className="now-title">{current?.title ?? "Nothing playing"}</Text>
                <Text className="now-album">{current?.album ?? "Choose a track"}</Text>
              </View>

              <View className="viz-row">
                {/*
                  These three are native elements. React places them and sizes
                  them through CSS; Rust draws them straight from the audio
                  ring, so nothing on the paint path crosses this bridge.
                */}
                <Native type="spectrum" className="spectrum" color="#6ee7b7" />
                <View className="meters">
                  <Native type="level-meter" className="meter" channel="left" />
                  <Native type="level-meter" className="meter" channel="right" />
                </View>
              </View>
              <Native type="waveform" className="wave" color="#93c5fd" />
            </>
          )}
        </View>
      </View>

      <View className="transport">
        <View className="scrubber-row">
          <Text className="time">{formatTime(position)}</Text>
          <Slider
            className="scrubber"
            value={duration > 0 ? position / duration : 0}
            minimumValue={0}
            maximumValue={1}
            disabled={duration <= 0}
            onValueChange={(fraction) => {
              if (duration > 0) setScrubbing(fraction * duration);
            }}
          />
          <Text className="time">{formatTime(state.duration)}</Text>
        </View>

        <View className="controls">
          <Button
            title="Prev"
            disabled={library.length === 0}
            onPress={() => void bridge.invoke("player.skip", { delta: -1 })}
          />
          <Button
            className="primary"
            title={state.playing ? "Pause" : "Play"}
            disabled={library.length === 0}
            onPress={() => void bridge.invoke("player.toggle")}
          />
          <Button
            title="Next"
            disabled={library.length === 0}
            onPress={() => void bridge.invoke("player.skip", { delta: 1 })}
          />
          {scrubbing !== null && (
            <Button className="primary" title="Go" onPress={() => commitSeek(scrubbing)} />
          )}

          <View className="spacer" />

          <Text className="volume-label">{`VOL ${Math.round(state.volume * 100)}%`}</Text>
          <Slider
            className="volume"
            value={state.volume}
            minimumValue={0}
            maximumValue={1}
            step={0.01}
            onValueChange={(volume) => void bridge.invoke("player.setVolume", { volume })}
          />
        </View>
      </View>
    </View>
  );
}

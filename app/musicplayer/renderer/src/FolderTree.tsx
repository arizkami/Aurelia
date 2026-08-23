import { useCallback, useEffect, useState } from "react";
import { Text, View, cx, type SphereKitApiBridge } from "@spherekit/react";

/** A folder as the native side reports it. */
export interface FolderEntry {
  readonly name: string;
  readonly path: string;
}

/** One expanded level, as `browser.list` returns it. */
interface FolderLevel {
  readonly path: string;
  readonly name: string;
  readonly parent: string | null;
  readonly folders: readonly FolderEntry[];
  /** Playable files directly in this folder. Nothing else is listed. */
  readonly files: readonly FolderEntry[];
  readonly truncated: boolean;
  readonly trackCount: number;
}

export interface FolderTreeProps {
  readonly bridge: SphereKitApiBridge;
  /** Show only this drive. Null shows every root. */
  readonly drive?: FolderEntry | null;
  /** The folder whose tracks are currently loaded. */
  readonly openPath: string | null;
  /** Called when a folder is chosen as the library root. */
  readonly onOpen: (path: string) => void;
  /** Called when a single file is chosen: load its folder, then play it. */
  readonly onPlayFile: (folder: string, file: string) => void;
}

/**
 * A lazily expanded folder tree.
 *
 * React owns which nodes are open; Rust answers one level at a time. Nothing
 * is read from disk until a node is actually expanded, so pointing this at a
 * drive root costs one `readdir`, not a recursive walk.
 */
export function FolderTree({ bridge, drive, openPath, onOpen, onPlayFile }: FolderTreeProps) {
  const [roots, setRoots] = useState<readonly FolderEntry[]>([]);

  useEffect(() => {
    void bridge.invoke<readonly FolderEntry[]>("browser.roots").then(setRoots);
  }, [bridge]);

  const shown = drive ? [drive] : roots;

  return (
    <View className="tree">
      {shown.map((root) => (
        <FolderNode
          // Keyed by path so switching drive remounts the node rather than
          // reusing the previous drive's loaded children under a new name.
          key={root.path}
          bridge={bridge}
          entry={root}
          depth={0}
          startExpanded={shown.length === 1}
          openPath={openPath}
          onOpen={onOpen}
          onPlayFile={onPlayFile}
        />
      ))}
    </View>
  );
}

interface FolderNodeProps extends FolderTreeProps {
  readonly entry: FolderEntry;
  readonly depth: number;
  /** Opens on mount, for the single drive the sidebar is showing. */
  readonly startExpanded?: boolean;
}

function FolderNode({
  bridge,
  entry,
  depth,
  startExpanded = false,
  openPath,
  onOpen,
  onPlayFile,
}: FolderNodeProps) {
  const [expanded, setExpanded] = useState(startExpanded);
  const [level, setLevel] = useState<FolderLevel | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!startExpanded || level !== null) return;
    setLoading(true);
    void bridge
      .invoke<FolderLevel>("browser.list", { path: entry.path })
      .then(setLevel)
      .finally(() => setLoading(false));
  }, [bridge, entry.path, startExpanded, level]);

  const toggle = useCallback(() => {
    const next = !expanded;
    setExpanded(next);
    // Fetched on first expand and then kept. A folder that changes on disk
    // while the window is open is rare enough not to justify re-reading it on
    // every collapse and re-expand.
    if (next && level === null && !loading) {
      setLoading(true);
      void bridge
        .invoke<FolderLevel>("browser.list", { path: entry.path })
        .then(setLevel)
        .finally(() => setLoading(false));
    }
  }, [bridge, entry.path, expanded, level, loading]);

  const isOpen = openPath === entry.path;
  const count = level?.trackCount ?? 0;

  return (
    <View className="tree-node">
      <View
        className={cx("tree-row", isOpen && "open")}
        // Indent by depth. Padding rather than margin so the whole row stays
        // clickable out to the left edge.
        style={{ paddingLeft: 8 + depth * 14 }}
        onPress={toggle}
      >
        <Text className="tree-caret">{expanded ? "▾" : "▸"}</Text>
        <Text className="tree-name">{entry.name}</Text>
        {count > 0 && <Text className="tree-count">{String(count)}</Text>}
      </View>

      {expanded && level !== null && (
        <View className="tree-children">
          {count > 0 && (
            <View
              className="tree-row action"
              style={{ paddingLeft: 8 + (depth + 1) * 14 }}
              onPress={() => onOpen(entry.path)}
            >
              <Text className="tree-caret">♪</Text>
              <Text className="tree-name">{`Play these ${count}`}</Text>
            </View>
          )}
          {level.folders.map((child) => (
            <FolderNode
              key={child.path}
              bridge={bridge}
              entry={child}
              depth={depth + 1}
              openPath={openPath}
              onOpen={onOpen}
              onPlayFile={onPlayFile}
            />
          ))}
          {level.files.map((file) => (
            <View
              key={file.path}
              className="tree-row file"
              style={{ paddingLeft: 8 + (depth + 1) * 14 }}
              onPress={() => onPlayFile(entry.path, file.path)}
            >
              <Text className="tree-caret">♪</Text>
              <Text className="tree-name">{file.name}</Text>
            </View>
          ))}
          {level.truncated && (
            <Text className="tree-note" style={{ paddingLeft: 8 + (depth + 1) * 14 }}>
              too many folders to list
            </Text>
          )}
        </View>
      )}
    </View>
  );
}

import { createRoot, jsonBridge, type ReactRoot } from "@spherekit/react";
import { App } from "./App";

/** The Rust object exposed to the renderer by the native bootstrap. */
export interface RustReactHost {
  commitJson(snapshot: string): void;
}

/** Mounts the renderer into a Rust-owned SphereKit host. */
export function mountSphereKitReact(host: RustReactHost): ReactRoot {
  const root = createRoot(jsonBridge((snapshot) => host.commitJson(snapshot)));
  root.render(<App />);
  return root;
}

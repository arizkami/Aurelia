import { createApiBridge, createRoot, type ApiBridgeTransport, type ReactRoot } from "@spherekit/react";
import { App } from "./App";

/** The transport exposed by the Rust/native bootstrap. */
export type RustReactHost = ApiBridgeTransport;

/** Mounts the renderer into a Rust-owned SphereKit API Bridge. */
export function mountSphereKitReact(host: RustReactHost): ReactRoot {
  const root = createRoot(createApiBridge(host));
  root.render(<App />);
  return root;
}

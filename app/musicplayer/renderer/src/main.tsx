/**
 * Entry point for the bundle that runs inside SphereKit's V8 isolate.
 *
 * There is no DOM and no `index.html`. `bun build --target=browser
 * --format=iife --production` produces one script; the Rust shell evaluates
 * `runtime/prelude.js` to give the bare context the handful of web globals
 * React's modules reach for, then evaluates this.
 */
import { createApiBridge, createRoot, createV8Transport } from "@spherekit/react";
import { App } from "./App";

const bridge = createApiBridge(createV8Transport(), { platform: "spherekit-musicplayer" });
const root = createRoot(bridge);

root.render(<App bridge={bridge} />);

(globalThis as Record<string, unknown>).__appRoot = root;
(globalThis as Record<string, unknown>).__appBridge = bridge;

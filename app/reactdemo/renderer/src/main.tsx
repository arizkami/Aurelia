/**
 * Entry point for the bundle that runs inside SphereKit's V8 isolate.
 *
 * There is no `index.html` and no DOM. `bun build --target=browser
 * --format=iife --production` produces one script; the Rust host evaluates
 * `runtime/prelude.js` to give the bare V8 context the handful of web globals
 * React's module evaluation reaches for, then evaluates this bundle. From
 * there it is ordinary React: `createRoot(...).render(<App />)`.
 */
import { createApiBridge, createRoot, createV8Transport } from "@spherekit/react";
import { App } from "./App";

const bridge = createApiBridge(createV8Transport(), { platform: "spherekit-native" });
const root = createRoot(bridge);

root.render(<App bridge={bridge} />);

// The host looks these up by name to drive teardown and to report what mounted.
(globalThis as Record<string, unknown>).__appRoot = root;
(globalThis as Record<string, unknown>).__appBridge = bridge;

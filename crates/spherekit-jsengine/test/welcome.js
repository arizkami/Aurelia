// Smoke script for the spherekit-jsengine embedding API.
//
// The Rust tests evaluate this file and then drive it through the same entry
// points a real host uses, so it deliberately contains one of each: a global
// callable for `call_global`, a queued promise continuation for
// `run_microtasks`, and a completion value for `eval_named` to hand back.
//
// Keep it dependency-free — there is no module loader here, only a bare
// context, so `require`, `import` and Node globals do not exist.

globalThis.welcome = function welcome(name) {
  const who = typeof name === "string" && name.length > 0 ? name : "world";
  return `Hello, ${who}! Running on V8 via SphereKit.`;
};

// Stays false until the host performs a microtask checkpoint. The engine runs
// the isolate on an explicit microtask policy, so nothing below this line
// executes just because the script finished.
globalThis.microtaskRan = false;
Promise.resolve().then(() => {
  globalThis.microtaskRan = true;
});

globalThis.welcome("SphereKit");

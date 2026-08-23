//! The whole JavaScript pipeline, with no bundler and no React.
//!
//! ```text
//! cargo run -p spherekit-bridge --features v8 --example js_roundtrip
//! ```
//!
//! What React's reconciler eventually produces is one `commit` frame, so a
//! script that writes that frame by hand exercises exactly the same path: V8
//! calls `__spherekitSend`, the endpoint validates the snapshot, the React host
//! retains it, and `spherekit-ui` lowers it to native elements. Doing it
//! without a build step is the point — when the React demo misbehaves, this is
//! the example that says whether the fault is in the bridge or in the bundle.

#[cfg(all(feature = "v8", windows))]
fn main() {
    use spherekit_bridge::JsBridge;

    /// Web globals the bare V8 context does not have.
    const PRELUDE: &str = include_str!("../../spherekit-react/runtime/prelude.js");

    /// A renderer written directly against the wire protocol.
    const APP: &str = r##"
// One synchronous call each way. `__spherekitSend` returns a JSON array of
// reply frames, which is the whole transport: no sockets, no event loop.
function send(message) {
  const replies = JSON.parse(__spherekitSend(JSON.stringify(message) + "\n"));
  return replies.map(function (frame) { return JSON.parse(frame); });
}

const ready = send({
  kind: "hello", version: __spherekitProtocol, runtime: "vanilla", platform: "windows",
})[0];
console.log("ready: protocol " + ready.version +
            ", " + ready.capabilities.length + " capabilities");

const stylesheet = send({
  kind: "invoke",
  id: "css",
  method: "spherekit.setStylesheet",
  params: { css: ".panel { display: flex; flex-direction: column; gap: 8px; padding: 12px; }" +
                 ".title { font-size: 20px; color: #f0f0f2; }" },
})[0];
console.log("stylesheet: " + stylesheet.result.rules + " rules");

let revision = 0;
function commit(heading, items) {
  revision += 1;
  let id = 1;
  const node = function (type, props, children, text) {
    const value = { id: id++, type: type, props: props, children: children || [] };
    if (text !== undefined) value.text = text;
    return value;
  };
  send({
    kind: "commit",
    snapshot: {
      revision: revision,
      children: [
        node("view", { className: "panel" }, [
          node("text", { className: "title" }, [node("#text", {}, [], heading)]),
        ].concat(items.map(function (item) {
          return node("text", {}, [node("#text", {}, [], item)]);
        }))),
      ],
    },
  });
}

commit("SphereKit", ["one", "two"]);

// The isolate has no event loop, so this callback runs when the host calls
// __runTimers() from JsBridge::tick() — and not a moment earlier.
setTimeout(function () {
  commit("SphereKit", ["one", "two", "three (committed from a timer)"]);
  console.log("timer committed revision " + revision);
}, 0);

const tree = send({ kind: "invoke", id: "tree", method: "spherekit.getTree", params: {} })[0];
console.log("getTree answered revision " + tree.result.revision);
"##;

    let mut js = match JsBridge::new() {
        Ok(js) => js,
        Err(error) => {
            eprintln!("V8 did not start: {error}");
            std::process::exit(1);
        }
    };

    js.install_prelude(PRELUDE).expect("the prelude evaluates");
    js.load(APP, "app.js").expect("the application evaluates");
    js.tick().expect("the frame ticks");

    for line in js.drain_console() {
        println!("js: {line}");
    }

    let bridge = js.bridge();
    let host = bridge.host();
    println!(
        "\ncommitted tree (revision {}):\n{}",
        host.tree().revision,
        serde_json::to_string_pretty(host.tree()).expect("the tree serialises")
    );

    let mut ui = spherekit_ui::UiTree::new();
    ui.build(host.ui_element());
    println!("\nresolved native elements: {}", ui.stats().elements);
    println!("v8: {}", spherekit_bridge::v8_version());
}

#[cfg(not(all(feature = "v8", windows)))]
fn main() {
    // Not a failure: the example is a demonstration, and a workspace build on a
    // machine without the V8 prebuilt should still compile every target in it.
    println!(
        "js_roundtrip needs the `v8` feature on Windows x86_64.\n\
         Run: cargo run -p spherekit-bridge --features v8 --example js_roundtrip"
    );
}

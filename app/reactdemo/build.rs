//! Bundles the TypeScript renderer into a single script the V8 isolate can run.
//!
//! The bundle is written to `OUT_DIR` and pulled in with `include_str!`, so the
//! compiled binary always carries a renderer and never depends on a `dist/`
//! directory surviving on disk.
//!
//! Bun is not a build requirement. A machine without it — or a checkout where
//! `bun install` has not run — still produces a working binary: the fallback
//! renderer written below speaks the API Bridge protocol directly, with no
//! React and no bundler, and says so on screen. Failing the build instead would
//! mean `cargo build --workspace` could not succeed without a JavaScript
//! toolchain, which is too high a price for one example.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let renderer = manifest_dir.join("renderer");
    let bundle = out_dir.join("app.js");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=styles/app.css");
    println!("cargo:rerun-if-changed=renderer/package.json");
    for entry in walk(&renderer.join("src")) {
        println!("cargo:rerun-if-changed={}", entry.display());
    }
    // The renderer imports `@spherekit/react` from the workspace, so a change
    // to the TypeScript package has to rebuild this bundle too.
    for entry in walk(&manifest_dir.join("../../crates/spherekit-react/src")) {
        println!("cargo:rerun-if-changed={}", entry.display());
    }

    let package = manifest_dir.join("../../crates/spherekit-react");
    match bundle_with_bun(&renderer, &package, &bundle) {
        Ok(()) => println!("cargo:rustc-env=SPHEREKIT_REACTDEMO_BUNDLE=react"),
        Err(reason) => {
            println!("cargo:warning=reactdemo: {reason}");
            println!(
                "cargo:warning=reactdemo: falling back to the no-bundler renderer; run \
                 `bun install` in app/reactdemo/renderer for the React one"
            );
            fs::write(&bundle, FALLBACK_RENDERER).expect("write fallback renderer");
            println!("cargo:rustc-env=SPHEREKIT_REACTDEMO_BUNDLE=fallback");
        }
    }
}

/// Runs Bun's bundler, installing dependencies and building `@spherekit/react`
/// first.
///
/// The package publishes compiled output — its `exports` point at `dist/`, not
/// at the TypeScript sources — and `dist/` is build output, so it is not in the
/// repository. Bundling the renderer before that exists fails with nothing more
/// helpful than "Could not resolve @spherekit/react", so the build is run here
/// rather than left as a step someone has to know about.
fn bundle_with_bun(renderer: &Path, package: &Path, bundle: &Path) -> Result<(), String> {
    if !renderer.join("node_modules").is_dir() {
        run(renderer, "bun", &["install".into()])
            .map_err(|error| format!("`bun install` failed: {error}"))?;
    }

    if package.is_dir() {
        run(package, "bun", &["run".into(), "build".into()])
            .map_err(|error| format!("building @spherekit/react failed: {error}"))?;
    }

    run(
        renderer,
        "bun",
        &[
            "build".into(),
            "src/main.tsx".into(),
            "--target=browser".into(),
            "--format=iife".into(),
            "--production".into(),
            format!("--outfile={}", bundle.display()),
        ],
    )
    .map_err(|error| format!("`bun build` failed: {error}"))?;

    let bundled = fs::metadata(bundle).map(|meta| meta.len()).unwrap_or(0);
    if bundled == 0 {
        return Err("`bun build` produced an empty bundle".into());
    }
    Ok(())
}

fn run(cwd: &Path, tool: &str, args: &[String]) -> Result<(), String> {
    let output = Command::new(tool)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("could not start `{tool}`: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(format!("{} — {}{}", output.status, stdout.trim(), stderr.trim()))
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else { return files };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// A renderer with no React, no TypeScript and no bundler.
///
/// It speaks the API Bridge wire protocol by hand, which makes it a useful
/// thing to read: everything React's reconciler eventually produces is one
/// `commit` frame shaped exactly like this.
const FALLBACK_RENDERER: &str = r##"
(function () {
  "use strict";

  var revision = 0;
  var nextId = 1;

  function send(message) {
    var replies = JSON.parse(__spherekitSend(JSON.stringify(message) + "\n"));
    return replies.map(function (frame) {
      return JSON.parse(frame);
    });
  }

  function node(type, props, children, text) {
    var value = { id: nextId++, type: type, props: props || {}, children: children || [] };
    if (text !== undefined) value.text = text;
    return value;
  }

  function text(content, className) {
    return node("text", { className: className || "" }, [node("#text", {}, [], content)]);
  }

  send({ kind: "hello", version: 1, runtime: "fallback", platform: "spherekit-native" });

  var presses = 0;

  globalThis.__spherekitReceive = function (frame) {
    var message = JSON.parse(frame);
    if (message.kind === "event" && message.event === "press") {
      presses += 1;
      render();
    }
  };

  function render() {
    revision += 1;
    nextId = 1;
    send({
      kind: "commit",
      snapshot: {
        revision: revision,
        children: [
          node("view", { className: "root" }, [
            node("view", { className: "header" }, [
              node("view", {}, [
                text("SphereKit React", "title"),
                text("no bundler — this is the raw API Bridge protocol", "subtitle")
              ])
            ]),
            node("view", { className: "card" }, [
              text("RENDERER BUNDLE", "label"),
              text("Run `bun install` in app/reactdemo/renderer, then rebuild.", "value")
            ]),
            node("view", { className: "card telemetry" }, [
              text("PRESSES", "label"),
              node("view", { className: "meter" }, [text(String(presses), "value")]),
              node("button", { className: "primary", title: "Press me" }, [])
            ])
          ])
        ]
      }
    });
  }

  render();
})();
"##;

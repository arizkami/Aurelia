# Working in this repository

## Commands

All cargo through the **PowerShell** tool. See `pitfalls.md` #1.

```powershell
cargo test --workspace
cargo test -p spherekit-bridge --features v8
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo clippy -p spherekit-platform --no-default-features --all-targets -- -D warnings
```

```bash
bun install                                  # at the repo root, always
cd crates/spherekit-react && bun run typecheck && bun test
cd crates/spherekit-react && bun run build   # produces dist/ that consumers resolve
```

Applications:

```powershell
cargo run -p uigallery --release
cargo run -p reactdemo --release
cargo run -p musicplayer --release -- "C:\path\to\music"
```

## House style — this repository is unusually strict

- Rust edition 2024, rust-version 1.87. `max_width = 100`,
  `use_small_heuristics = "Max"`. CI runs `cargo fmt --all --check`.
- Every crate has `#![deny(missing_docs)]`. Every public item — module, struct,
  enum, variant, **field**, function, trait method — needs a doc comment. This
  is a compile error, not a lint.
- `cargo clippy --all-targets -- -D warnings` must be clean. Do not add an
  `#[allow]` to silence something; fix it or delete the dead code.
- `clippy.toml` sets `doc-valid-idents = ["SphereKit", "DirectWrite",
  "FreeType", "ClearType", "CoreText", ".."]`.
- British spelling in prose: colour, behaviour, serialise, visualise.

### Comments explain *why*, not *what*

Read the module headers in `crates/spherekit-layout/src/style.rs` and
`crates/spherekit-ui/src/style.rs` for the register. They justify departures
from CSS, name what was deliberately left out, and say what would break
otherwise. Do not write comments that restate the signature.

### Tests are named as sentences and assert the property that matters

```rust
fn active_wins_over_hover()
fn a_bare_layout_container_paints_nothing()
fn skipping_backward_from_the_first_track_wraps_instead_of_panicking()
```

Put them in `#[cfg(test)] mod tests` at the bottom of the file. Write the
comment that says *what would break* if the assertion failed.

**Test the thing, not around it.** A test that re-derives the expected value
through the same path it is checking proves nothing. If you are testing a cache,
assert on something downstream of the cache — laid-out geometry, not a fresh
re-resolution. When a test protects an invariant that is easy to get wrong,
break the invariant on purpose once and confirm the test fails.

## Before saying you are done

1. `cargo test --workspace`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo fmt --all` then `--check`
4. `bun run typecheck` and `bun test` if TypeScript changed
5. Run the app if the change is visual — tests do not catch a frozen frame loop
   or a collapsed layout

## Measure before optimising

The one time this mattered most, the intuitive answers were all wrong. A
music-player UI was slow; the audio path and the visualisers were both
innocent, and 89% of the frame was the CSS cascade re-running for an unchanged
tree. `crates/spherekit-react/examples/lower_bench.rs` is the harness — extend
it rather than guessing.

Two hypotheses were tested and **disproved** on the way: that `var()`'s ancestor
walk was expensive (it was ~1 ms of 26), and that Thai marks were mis-shaped
(shaping was correct at every stage). Both would have been plausible fixes to a
problem that was somewhere else.

## Publishing

- crates.io: 16 crates, version in the workspace `[workspace.package]` and
  mirrored in every `[workspace.dependencies]` entry. `cargo publish
  --workspace` handles ordering.
- npm: `@spherekit/react`. `prepack` runs the build; `exports` point at `dist/`,
  which is build output and not committed — anything consuming the package
  rather than its sources must build it first.
- Version the two together; the CLI derives the scaffolded npm range from the
  Cargo version.

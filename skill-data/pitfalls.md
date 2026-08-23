# Traps

Every one of these has already cost real time in this repository. They are
ordered by how likely you are to hit them.

## 1. Run cargo through PowerShell, never Bash

The Bash tool is Git Bash, and its `C:\Program Files\Git\usr\bin\link.exe` (GNU
coreutils `link`) shadows the MSVC linker. Every binary and test link dies with:

```text
/usr/bin/link: missing operand after ' ■'
note: the Visual Studio build tools may need to be repaired
```

That note is a lie — the toolchain is fine. Reading, grepping and editing via
Bash is fine; anything invoking `cargo`, `rustc` or `link.exe` goes through the
PowerShell tool. `bun` works from either.

## 2. React must be exactly one module instance

`@spherekit/react` is a React *renderer*. `react-reconciler` installs the hook
dispatcher onto `ReactSharedInternals`, and application components read it back
through their own `react` import. Two copies means the dispatcher they see is
`null`.

The failure is delayed and misleading: **mount and first paint succeed**, then
the first update originating outside a synchronous render — a native event, a
promise, an effect — dies with:

```text
TypeError: Cannot read properties of null (reading 'useState')
```

Nothing in that message mentions the install. The repo root is a Bun workspace
for this reason alone, and `react` is a `peerDependency`, never a dependency.
Add new TypeScript packages to the root `workspaces` array; never run
`bun install` inside a subpackage.

## 3. `r#"…"#` and `"#text"`

Any raw string containing `"#text"` — which every hand-written React commit
snapshot does — terminates a `r#"` literal early, and the error points somewhere
useless. Use `r##"…"##`.

## 4. `include_str!` must not escape the crate directory

`cargo package` ships a crate's own directory and nothing else, so an
`include_str!("../../../shaders/…")` compiles locally and fails for everyone who
installs from the registry. Three crates did this and would have published
broken. Keep embedded assets inside the crate; export a shared one as a `pub
const` from the crate that owns it (`spherekit_react::PRELUDE` is the pattern).

## 5. A build script must not write into its own package

`cargo package` refuses it, and rightly. `spherekit-jsengine` caches its V8
prebuilt in a user-level directory keyed by the archive URL, not in
`crates/spherekit-jsengine/`.

## 6. The React root fills its parent

`lower_tree`'s root is `full()` **and** `flex_1`. `flex_1` alone is not enough:
when the React root *is* the tree root there is no flex parent to grow into, so
a grow factor leaves it content-sized. A content-sized root means `height: 100%`
on the top-level component resolves against an indefinite parent and is dropped,
every `flex: 1` under it collapses to zero, and the window renders its last
child at the top with blank space above. A short, top-aligned tree looks
identical either way.

Corollary for stylesheets: prefer `flex: 1` over `height: 100%`, because a
percentage against an auto-height ancestor is silently dropped.

## 7. Flex items shrink, and `nowrap` text does not

A flex item shrinks below its content width by default. A shrunken box does not
shorten `white-space: nowrap` text — it paints at full width **over its
neighbour**. Put `flex-shrink: 0` on any fixed label in a row.

## 8. The frame loop has to drive itself

A media player is not an idle document: meters, spectra and scrubbers all change
with no input at all. If `draw()` does not request the next frame, the window
only repaints when an event happens to arrive, and everything looks frozen and a
mouse-move behind. Gate the request on "is anything still moving" so a still
window goes back to sleep.

## 9. `%` on a signed index

`(index - 1) % count` is negative at index zero, and indexing with it panics.
Use `rem_euclid`.

## 10. Peak metering pins on real music

See `audio-and-text.md`. Feed RMS to the bar, peak to the clip indicator.

## 11. A combining mark's `y_offset` of zero is usually correct

See `audio-and-text.md`. Do not "fix" it by inventing an offset.

## 12. Stale incremental artifacts

If a link fails with `LNK1120: unresolved external symbol` naming something like
`drop_glue::<serde_json::Value>`, look for `did not finalize incremental
compilation session directory: Access is denied` earlier in the output —
something (usually antivirus) is locking `target/debug/incremental`. Fix with
`cargo clean -p <crate>`. It is not a code fault.

## 13. crates.io rate-limits new crates

A burst of five, then roughly one per ten minutes. Publishing sixteen new crates
takes about two hours of paced retries. It never applies again once they exist.

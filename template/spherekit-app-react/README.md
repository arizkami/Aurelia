# spherekit-app-react

Starter template for a SphereKit app using React with the small native JSX
surface from `@spherekit/react`.

The app follows a Tauri-like split between the web renderer and native app
backend:

```text
src/
├── renderer/  # TypeScript + React UI
└── app/       # Rust backend + Cargo
```

```tsx
<View>
  <Text>Hello</Text>
  <Button title="Play" />
  <Slider value={0.65} />
</View>
```

The renderer sends a committed React snapshot through one native boundary,
`commitJson(snapshot)`. The Rust backend stores it in `ReactHost` and can lower
the tree into SphereKit native UI elements.

The template is intentionally not published or wired to a registry workspace
yet. The backend currently points at this repository's local
`crates/spherekit-react` package while the native package API is being built.
After `@spherekit/react` is published to npm and the Rust package is made
available to the template, run:

```bash
bun install
bun run typecheck
cargo check --manifest-path src/app/Cargo.toml
```

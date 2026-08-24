# Python

`spherekit-python` is a CPython extension module that opens a real SphereKit
window and paints a tree a Python program describes.

```python
import spherekit as sk

class Counter(sk.App):
    title = "Counter"

    def setup(self):
        self.count = 0

    def render(self):
        return sk.view(class_name="page")(
            sk.text(str(self.count), class_name="big"),
            sk.button("Add one", on_press=self.add),
        )

    def add(self):
        self.count += 1

Counter(css=".page { padding: 24px; gap: 12px } .big { font-size: 48px }").run()
```

---

## The decision that shaped it

**Python describes; it does not draw.** A frame is one JSON string in the same
committed-tree format the React renderer produces, handed to the same
`spherekit_react::ReactHost`, lowered through the same `spherekit-css` cascade.

The alternative — exposing `Div`, `Button`, `Style` and the rest through PyO3 as
classes — would have produced a second widget table, a second set of prop names,
a second styling story and a second place for every bug to live. It would also
have been perhaps ten times the code. The binding is small because it reuses the
host that already exists, and a Python application and a React application
provably go through the same lowering: there is a test asserting the node types
the Python package builds are exactly the node types the host answers to.

The cost is honest: Python cannot reach a widget the host does not already
lower. Adding one means adding it to `lower.rs`, where React gets it too.

---

## The boundary

```text
Python                          Rust
------                          ----
render() -> JSON string   ->    ReactHost::commit_json
                                ReactHost::ui_element_with_events
                                SphereKitSurface::render
on_event(name, id, payload) <-  EventQueue, drained once a frame
```

Two callables, and nothing else crosses. `render` is called once per frame;
`on_event` receives `(name, node_id, payload)` where the payload is a `dict` or
`None`.

### Whole trees, never diffs

The host takes a complete snapshot, so it cannot observe a half-built frame and
Python never has to describe a change — which is the part a binding usually gets
wrong. A frame whose JSON matches the previous one byte for byte is not
committed at all: an idle window costs one `render()` call and one string
comparison, and does no lowering, no cascade and no layout.

That is also why the Python side serialises with sorted keys and no spaces. A
dict that iterated in a different order would look like a changed frame on every
tick and quietly cost a full commit each time.

### The revision belongs to Rust

The host refuses a commit whose revision did not advance. If Python owned that
counter, a script that forgot to increment it would show a window that silently
stopped updating with nothing to point at. So the Rust side stamps it, and a
tree that arrives with a revision of its own has it **replaced** rather than
prepended — two `revision` keys are legal JSON and the last one wins, which
would hand the number straight back to the side that must not have it.

### Threading

The window runs on the thread that called `run`, because every desktop platform
requires that of an event loop. The GIL is released for the duration and
re-acquired per `render()` and per event, so a Python thread doing background
work keeps running while the window is idle.

---

## Identity

Every node needs an identity that survives to the next frame, because that is
what the layout tree reuses. Lose it and a text field loses its caret, a scroll
view loses its position, and hover jumps to a different row.

The identity is an FNV-1a hash of the node's **path**: the type and position of
each ancestor. Deliberately not Python's own `hash`, which is randomised per
interpreter run — identities that changed between runs would be correct and
would quietly defeat every cache keyed on them.

Position is right until a list reorders, at which point every row after the
change inherits the identity of the row that used to be there. `key=` replaces
the position in the path:

```python
sk.view(key=f"task-{task.id}")(sk.text(task.title))
```

Same rule as React's, and for the same reason. Two siblings given the same key
would be an ambiguous identity, which the host rejects for the *whole commit*;
the package resolves the clash itself rather than letting a copy-pasted key take
the window down.

---

## Events

| Native event | Python handler | Argument |
|---|---|---|
| `press` | `on_press` on a button, or on a `view` | none |
| `valueChange` | `on_change` on a slider, knob, fader | the value, a float |
| `change` | `on_change` on a toggle, checkbox | the checked flag |
| `change` | `on_change` on a text field | the text |
| `submit` | `on_submit` on a text field | the text |
| `select` | `on_select` on a menu item | none |

A payload carrying exactly one value is passed as that value rather than as a
dict. Every event the host sends carries either nothing or one number, string or
flag, and making every caller write `payload["value"]` would be ceremony with no
information in it. Anything richer arrives whole.

Handlers are collected fresh on every frame, keyed by identity. They are
closures over *this* frame's state, and holding the previous frame's would call
back with values the user can no longer see. An event that arrives for a node
the newest tree no longer has is dropped — which happens routinely, because
events are queued during a frame and delivered after it.

A `view` only reports presses when it is given `on_press`; the host gates it on
a `pressable` prop. Wiring every container would put an event on the queue for
every click anywhere in the tree, including the dozens of nested layout boxes a
real screen is built from.

---

## Building and loading

```bash
cargo build -p spherekit-python --release
python app/python-app/main.py
```

No wheel, no maturin. The Python package finds the cargo artifact
(`target/release/spherekit_python.dll`, `.so`, `.dylib`) and loads it with
`importlib.machinery.ExtensionFileLoader`, which takes the module *name* and the
file *path* separately — so the `PyInit_spherekit_python` symbol is found in a
file Python would otherwise refuse to import by name. Release is preferred over
debug when both exist, because a debug build of a GPU renderer is slow enough to
be misleading. `SPHEREKIT_LIB` overrides both.

The extension is built against the **stable ABI** (`abi3-py39`), so one binary
loads into every CPython from 3.9 up. Without it the extension is bound to the
exact minor version it was compiled against, and a machine with two Pythons on
it — which is most machines — loads the wrong one and fails at import.

---

## Testing without a window

```python
sk.check_tree(json)   # commits the tree into a throwaway host, returns node count
sk.check_css(css)     # parses a stylesheet, returns rule count
```

Both matter more than they look. A tree the host refuses is not an error anybody
sees — the window keeps painting the previous frame — and a stylesheet that does
not parse leaves the previous one installed. Neither failure has a symptom other
than "it looks wrong", so both have an explicit check that a test can call with
no GPU in the room.

`app/python-app/test_app.py` uses them to assert the demo's tree commits, its
stylesheet parses, identity is stable across frames and survives a keyed
reorder, and events reach the handler that asked for them.

---

## What is not there yet

* **No widget Python can reach that the host cannot lower.** Adding one is a
  change to `spherekit-react`'s `lower.rs`, which gives it to React too.
* **No custom drawing.** A Python `render` cannot paint into the canvas; it can
  only arrange widgets.
* **Controlled text fields rebuild their buffer.** Every keystroke commits a new
  tree and the caret lands at the end, so editing the middle of a string jumps.
  The React path has the same behaviour for the same reason.
* **One window per process.** `quit()` is a process-wide flag, and a second
  `run` on another thread is a platform error before it is an ambiguity.

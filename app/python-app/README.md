# python-app

The SphereKit Python demo: a small control surface, described entirely in Python.

```bash
cargo build -p spherekit-python --release
python app/python-app/main.py
```

Unattended, for CI or a screenshot:

```bash
python app/python-app/main.py --frames 120
```

Tests — no window, no GPU:

```bash
python app/python-app/test_app.py
```

## What it is

`main.py` is one `sk.App` subclass. State is plain attributes, `render()` returns
nodes, and there is no invalidation call anywhere: `render` runs every frame, and
a frame whose tree is byte-identical to the last is never committed, so an idle
window does no layout at all.

The screen exercises every built-in widget the native host lowers — button,
slider, knob, fader, toggle, checkbox, text field, progress, avatar, menu rows,
scrolling list — and styles all of them with the same CSS cascade a React
SphereKit application uses.

## How it works

```text
main.py                    spherekit/__init__.py         spherekit-python (Rust)
-------                    ---------------------         -----------------------
render() -> Node tree  ->  serialize() -> JSON      ->    ReactHost::commit_json
                           handlers by identity           ui_element_with_events
on_press / on_change   <-  dispatch by identity     <-    EventQueue, once a frame
```

Python never draws. It produces the same committed-tree JSON the React renderer
produces, and the *same* host lowers it. That is the whole reason the binding is
small: there is no second widget table, no second set of prop names, and no
second place for a styling bug to live.

## Identity, and why the list rows carry `key=`

A node's identity is derived from its path — its type and its position among its
siblings — and that identity is what the layout tree reuses between frames. Lose
it and a text field loses its caret, a list loses its scroll position, and a
hover state jumps to the wrong row.

Position is the right answer until a list reorders. Hide a finished task in the
demo and, without keys, every row below it would inherit the identity of the row
that used to be there. `key=` replaces the position in the path, which is the
same rule — and the same fix — as React's.

## Known rough edge

`text_field` is controlled: the value comes from Python state, so every
keystroke commits a new tree and the field is rebuilt with the caret at the end.
Editing the middle of a string jumps. The React demo has the same behaviour for
the same reason; a field that kept its own caret across commits needs the host
to reconcile the buffer rather than replace it.

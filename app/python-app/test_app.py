"""Tests for the Python binding and the demo, with no window and no GPU.

Run them directly, or with pytest if it is installed:

    python app/python-app/test_app.py
    python -m pytest app/python-app/test_app.py

What is worth testing without a window is precisely the part that fails
silently with one: a tree the host refuses is not an error anybody sees, it is a
window that keeps painting the previous frame. `check_tree` is the same
validation the running window does, so a tree that passes here is one that
commits there.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "crates" / "spherekit-python" / "python"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import spherekit as sk  # noqa: E402
from main import STYLESHEET, TASKS, ControlSurface  # noqa: E402


def commit_json(app: sk.App) -> str:
    """One frame of an application, in the format the host accepts."""
    body = json.loads(app._render_json())
    return json.dumps({"revision": 1, **body})


def test_the_demo_tree_is_one_the_host_accepts() -> None:
    app = ControlSurface()
    count = sk.check_tree(commit_json(app))
    # A real screen, not a placeholder: the demo builds three cards of widgets.
    assert count > 40, f"only {count} nodes"


def test_the_demo_stylesheet_parses() -> None:
    # A stylesheet that does not parse leaves the previous one installed, so the
    # failure is an unstyled window rather than an error.
    assert sk.check_css(STYLESHEET) > 10


def test_a_label_becomes_a_text_node_with_a_text_child() -> None:
    # The shape the host validates. A `text` node carrying its own string is a
    # refused commit, which is why `sk.text` builds the child itself.
    tree, _ = sk.serialize([sk.text("hello")])
    node = tree["children"][0]
    assert node["type"] == "text"
    assert node["children"][0] == {
        "id": node["children"][0]["id"],
        "type": "#text",
        "text": "hello",
    }


def identities(tree: dict) -> list[int]:
    """Every identity in a tree, in document order."""
    out: list[int] = []

    def walk(node: dict) -> None:
        out.append(node["id"])
        for child in node.get("children", ()):
            walk(child)

    for root in tree["children"]:
        walk(root)
    return out


def test_identity_is_stable_across_frames() -> None:
    # The property the layout tree depends on: a tree of the same shape must
    # produce the same identities, or every frame throws away the layout nodes
    # the last one built. The demo's *text* changes every frame — a counter and
    # a clock — and that must not move a single identity.
    app = ControlSurface()
    first = json.loads(app._render_json())
    second = json.loads(app._render_json())
    assert first != second, "the demo has nothing changing in it, so this proves nothing"
    assert identities(first) == identities(second)


def test_identity_survives_a_reorder_when_rows_carry_keys() -> None:
    # Remove a row from the middle. Without keys every row below it would take
    # the identity of the row that used to be there, and the hover state and
    # scroll position would follow the position rather than the task.
    def rows(items):
        return sk.serialize(
            [sk.view()(*[sk.view(key=f"task-{i}")(sk.text(t)) for i, t in items])]
        )[0]

    before = rows(list(enumerate(TASKS)))
    after = rows([(i, t) for i, t in enumerate(TASKS) if i != 2])

    def identity_of(tree, index):
        return tree["children"][0]["children"][index]["id"]

    # The row that was fourth is now third and must have kept its identity.
    assert identity_of(before, 3) == identity_of(after, 2)


def test_identity_moves_with_the_position_when_there_are_no_keys() -> None:
    # The other half of the same rule, asserted so the docs cannot drift from
    # the behaviour: no key means identity is the position.
    def rows(count):
        return sk.serialize([sk.view()(*[sk.text(str(i)) for i in range(count)])])[0]

    before, after = rows(4), rows(3)
    first = lambda tree: tree["children"][0]["children"][0]["id"]  # noqa: E731
    assert first(before) == first(after)


def test_two_siblings_with_the_same_key_still_commit() -> None:
    # A duplicate identity is a refused commit — the whole frame, not just the
    # offending node — so the package resolves the clash rather than letting a
    # copy-pasted key take the window down.
    tree, _ = sk.serialize([sk.view()(sk.text("a", key="dup"), sk.text("b", key="dup"))])
    children = tree["children"][0]["children"]
    assert children[0]["id"] != children[1]["id"]
    assert sk.check_tree(json.dumps({"revision": 1, **tree})) == 5


def test_events_reach_the_handler_that_asked_for_them() -> None:
    pressed = []
    changed = []

    class Tiny(sk.App):
        def render(self):
            return sk.view()(
                sk.button("go", on_press=lambda: pressed.append(True)),
                sk.slider(0.5, on_change=changed.append),
            )

    app = Tiny()
    tree = json.loads(app._render_json())
    button_id = tree["children"][0]["children"][0]["id"]
    slider_id = tree["children"][0]["children"][1]["id"]

    app._dispatch("press", button_id, None)
    # A payload with one value arrives as that value, not as a dict: every
    # event the host sends carries either nothing or one number.
    app._dispatch("valueChange", slider_id, {"value": 0.25})
    assert pressed == [True]
    assert changed == [0.25]


def test_an_event_for_a_node_that_is_gone_is_ignored() -> None:
    # Events are queued during a frame and delivered after it, so one can
    # always arrive for a node the next tree no longer has.
    class Tiny(sk.App):
        def render(self):
            return sk.view()()

    app = Tiny()
    app._render_json()
    app._dispatch("press", 12345, None)  # must not raise


def test_children_accept_what_a_comprehension_produces() -> None:
    tree, _ = sk.serialize([sk.view()(
        (sk.text(str(i)) for i in range(3)),
        None,
        "a bare string",
    )])
    assert len(tree["children"][0]["children"]) == 4


def test_every_widget_the_package_builds_is_one_the_host_lowers() -> None:
    # The list the package exposes and the list the host answers to have to be
    # the same list. A name in one and not the other is a widget that silently
    # becomes a plain box.
    built = {
        sk.view().type,
        sk.text("a").type,
        sk.button("a").type,
        sk.slider(0.0).type,
        sk.knob(0.0).type,
        sk.fader(0.0).type,
        sk.toggle(False).type,
        sk.checkbox(False).type,
        sk.progress(0.0).type,
        sk.separator().type,
        sk.panel().type,
        sk.scroll_view().type,
        sk.text_field().type,
        sk.avatar("a").type,
    }
    assert built == set(sk.node_types())
    # `menu-item` is deliberately outside that set: it lowers to a real widget
    # but is not in the advertised list, so assert it commits on its own terms.
    tree, _ = sk.serialize([sk.menu_item("Copy", shortcut="Ctrl+C")])
    assert sk.check_tree(json.dumps({"revision": 1, **tree})) == 1


def main() -> int:
    tests = [value for name, value in sorted(globals().items()) if name.startswith("test_")]
    failed = 0
    for test in tests:
        try:
            test()
        except AssertionError as error:
            failed += 1
            print(f"FAIL {test.__name__}: {error}")
        except Exception as error:  # noqa: BLE001 - a test harness reports, it does not raise
            failed += 1
            print(f"ERROR {test.__name__}: {type(error).__name__}: {error}")
        else:
            print(f"ok   {test.__name__}")
    print(f"\n{len(tests) - failed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())

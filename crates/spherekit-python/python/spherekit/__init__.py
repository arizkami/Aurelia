"""SphereKit for Python.

A GPU-first UI engine, described from Python.

    import spherekit as sk

    class Counter(sk.App):
        title = "Counter"

        def setup(self):
            self.count = 0

        def render(self):
            return sk.view(class_name="page")(
                sk.text(f"{self.count}", class_name="big"),
                sk.button("Add one", on_press=self.add),
            )

        def add(self):
            self.count += 1

    Counter(css=".page { padding: 24px } .big { font-size: 48px }").run()

## What Python actually does

Python describes a tree; the engine paints it. `render` returns nodes, the
package turns them into the JSON commit format the native host already speaks,
and the host lowers that to real widgets through the same CSS cascade a React
application uses. Nothing here re-implements a widget.

## Identity, and why `key` matters

Every node needs a stable identity across frames, because that identity is what
the layout tree reuses — lose it and a text field loses its caret and a list
loses its scroll position. Identity is derived from the node's *path*: its type
and position among its siblings. That is right until a list reorders, at which
point every row after the change gets the identity of the row that used to be
there. Pass `key=` on list rows and the key is used instead of the position,
which is the same rule, and the same fix, as React's.
"""

from __future__ import annotations

import json
from typing import Any, Callable, Iterable, Mapping, Sequence

from . import _loader

_native = _loader.load()

version = _native.version
node_types = _native.node_types
check_tree = _native.check_tree
check_css = _native.check_css
quit = _native.quit  # noqa: A001 - the name the caller wants is the platform's

__all__ = [
    "App",
    "Node",
    "avatar",
    "button",
    "check_css",
    "check_tree",
    "checkbox",
    "fader",
    "knob",
    "menu_item",
    "node_types",
    "panel",
    "progress",
    "quit",
    "run",
    "scroll_view",
    "separator",
    "slider",
    "text",
    "text_field",
    "toggle",
    "version",
    "view",
]

# --------------------------------------------------------------------------
# Identity
# --------------------------------------------------------------------------

_FNV_OFFSET = 0xCBF29CE484222325
_FNV_PRIME = 0x100000001B3
_MASK = 0xFFFFFFFFFFFFFFFF


def _fnv1a(text: str) -> int:
    """A stable 64-bit hash of a node path.

    Stable is the whole requirement: the same path must produce the same
    identity on every frame *and* in every process, so this cannot be Python's
    own `hash`, which is randomised per interpreter run by default. A tree whose
    identities changed between runs would be correct — and would quietly defeat
    every cache keyed on them.
    """
    digest = _FNV_OFFSET
    for byte in text.encode("utf-8"):
        digest = ((digest ^ byte) * _FNV_PRIME) & _MASK
    # Zero is reserved: the host treats every id as meaningful and a node that
    # hashed to zero would be indistinguishable from one that was never given
    # an identity at all.
    return digest or 1


# --------------------------------------------------------------------------
# Nodes
# --------------------------------------------------------------------------


class Node:
    """One host node: a type, some props, some children.

    Callable, so children read as nesting rather than as a list argument:

        view(class_name="row")(text("left"), text("right"))
    """

    __slots__ = ("type", "props", "text", "children", "key", "handlers")

    def __init__(
        self,
        node_type: str,
        props: Mapping[str, Any] | None = None,
        *,
        text: str | None = None,
        key: Any = None,
        handlers: Mapping[str, Callable[..., Any]] | None = None,
        children: Sequence["Node"] = (),
    ) -> None:
        self.type = node_type
        self.props = dict(props or {})
        self.text = text
        self.key = key
        self.handlers = dict(handlers or {})
        self.children = list(children)

    def __call__(self, *children: "Node | str | None | Iterable[Any]") -> "Node":
        self.children.extend(_flatten(children))
        return self

    def __repr__(self) -> str:  # pragma: no cover - diagnostics only
        return f"<Node {self.type} props={self.props} children={len(self.children)}>"


def _flatten(items: Iterable[Any]) -> list[Node]:
    """Accepts what a comprehension naturally produces.

    Lists, generators and `None` all turn up in real render code — `None` most
    often, as the tail of a conditional — and a builder that rejected them would
    push a `[n for n in ... if n]` into every call site.
    """
    out: list[Node] = []
    for item in items:
        if item is None:
            continue
        if isinstance(item, Node):
            out.append(item)
        elif isinstance(item, str):
            out.append(text(item))
        elif isinstance(item, (list, tuple)) or hasattr(item, "__iter__"):
            out.extend(_flatten(item))
        else:
            raise TypeError(f"a child must be a Node, a string or an iterable, not {type(item)!r}")
    return out


def _props(
    class_name: str | None,
    style: Mapping[str, Any] | None,
    element_id: str | None,
    extra: Mapping[str, Any],
) -> dict[str, Any]:
    props: dict[str, Any] = {}
    if class_name:
        props["className"] = class_name
    if element_id:
        props["id"] = element_id
    if style:
        props["style"] = dict(style)
    props.update({k: v for k, v in extra.items() if v is not None})
    return props


def view(
    *,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    on_press: Callable[[], Any] | None = None,
    **props: Any,
) -> Node:
    """A container. The box everything else is arranged in.

    `on_press` makes it a pressable tile without making it a button — the host
    only wires a container's click when it is asked to, because otherwise every
    layout box in the tree would report presses.
    """
    node = Node("view", _props(class_name, style, element_id, props), key=key)
    if on_press is not None:
        node.props["pressable"] = True
        node.handlers["press"] = on_press
    return node


def text(
    value: Any = "",
    *,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A label.

    The string becomes a `#text` child rather than a prop, because that is what
    the host validates: only a `#text` node may carry text, and a `text` node
    that carried its own is a refused commit — a window that silently keeps
    painting the previous frame.
    """
    node = Node("text", _props(class_name, style, element_id, props), key=key)
    node.children.append(Node("#text", text=str(value)))
    return node


def button(
    title: str,
    *,
    on_press: Callable[[], Any] | None = None,
    disabled: bool = False,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A push button."""
    node = Node(
        "button",
        _props(class_name, style, element_id, {"title": title, "disabled": disabled, **props}),
        key=key,
    )
    if on_press is not None:
        node.handlers["press"] = on_press
    return node


def _range(
    kind: str,
    value: float,
    minimum: float,
    maximum: float,
    step: float | None,
    name: str | None,
    disabled: bool,
    on_change: Callable[[float], Any] | None,
    class_name: str | None,
    style: Mapping[str, Any] | None,
    element_id: str | None,
    key: Any,
    props: Mapping[str, Any],
) -> Node:
    node = Node(
        kind,
        _props(
            class_name,
            style,
            element_id,
            {
                "value": value,
                "minimumValue": minimum,
                "maximumValue": maximum,
                "step": step,
                "name": name,
                "disabled": disabled,
                **props,
            },
        ),
        key=key,
    )
    if on_change is not None:
        node.handlers["valueChange"] = on_change
    return node


def slider(
    value: float,
    *,
    minimum: float = 0.0,
    maximum: float = 1.0,
    step: float | None = None,
    name: str | None = None,
    disabled: bool = False,
    on_change: Callable[[float], Any] | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A horizontal value control."""
    return _range("slider", value, minimum, maximum, step, name, disabled, on_change,
                  class_name, style, element_id, key, props)


def knob(
    value: float,
    *,
    minimum: float = 0.0,
    maximum: float = 1.0,
    step: float | None = None,
    name: str | None = None,
    disabled: bool = False,
    on_change: Callable[[float], Any] | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A rotary control. Dragged vertically, like every audio tool's."""
    return _range("knob", value, minimum, maximum, step, name, disabled, on_change,
                  class_name, style, element_id, key, props)


def fader(
    value: float,
    *,
    minimum: float = 0.0,
    maximum: float = 1.0,
    step: float | None = None,
    name: str | None = None,
    disabled: bool = False,
    on_change: Callable[[float], Any] | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A vertical fader."""
    return _range("fader", value, minimum, maximum, step, name, disabled, on_change,
                  class_name, style, element_id, key, props)


def _switch(
    kind: str,
    checked: bool,
    label: str | None,
    disabled: bool,
    on_change: Callable[[bool], Any] | None,
    class_name: str | None,
    style: Mapping[str, Any] | None,
    element_id: str | None,
    key: Any,
    props: Mapping[str, Any],
) -> Node:
    node = Node(
        kind,
        _props(
            class_name,
            style,
            element_id,
            {"checked": checked, "label": label, "disabled": disabled, **props},
        ),
        key=key,
    )
    if on_change is not None:
        node.handlers["change"] = on_change
    return node


def toggle(
    checked: bool,
    *,
    label: str | None = None,
    disabled: bool = False,
    on_change: Callable[[bool], Any] | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A switch."""
    return _switch("toggle", checked, label, disabled, on_change,
                   class_name, style, element_id, key, props)


def checkbox(
    checked: bool,
    *,
    label: str | None = None,
    disabled: bool = False,
    on_change: Callable[[bool], Any] | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A checkbox."""
    return _switch("checkbox", checked, label, disabled, on_change,
                   class_name, style, element_id, key, props)


def text_field(
    value: str = "",
    *,
    placeholder: str | None = None,
    mask: bool = False,
    disabled: bool = False,
    on_change: Callable[[str], Any] | None = None,
    on_submit: Callable[[str], Any] | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A single-line editable field, with IME support from the engine."""
    node = Node(
        "text-field",
        _props(
            class_name,
            style,
            element_id,
            {
                "value": value,
                "placeholder": placeholder,
                "mask": mask,
                "disabled": disabled,
                **props,
            },
        ),
        key=key,
    )
    if on_change is not None:
        node.handlers["change"] = on_change
    if on_submit is not None:
        node.handlers["submit"] = on_submit
    return node


def progress(
    value: float = 0.0,
    *,
    indeterminate: bool = False,
    thickness: float | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A progress bar. `indeterminate` reports no value to a screen reader."""
    return Node(
        "progress",
        _props(
            class_name,
            style,
            element_id,
            {
                "value": value,
                "indeterminate": indeterminate,
                "thickness": thickness,
                **props,
            },
        ),
        key=key,
    )


def separator(
    vertical: bool = False,
    *,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A rule."""
    return Node(
        "separator",
        _props(class_name, style, element_id, {"vertical": vertical, **props}),
        key=key,
    )


def panel(
    title: str = "",
    *,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A titled group box."""
    return Node(
        "panel",
        _props(class_name, style, element_id, {"title": title, **props}),
        key=key,
    )


def scroll_view(
    *,
    horizontal: bool = False,
    both: bool = False,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A scrolling container, with the wheel and touch drag already wired.

    Needs `min-height: 0` on itself and every flex ancestor between it and a
    fixed-size box, exactly as CSS does — otherwise the ancestor grows to fit
    the content and there is nothing left to scroll.
    """
    return Node(
        "scroll-view",
        _props(class_name, style, element_id, {"horizontal": horizontal, "both": both, **props}),
        key=key,
    )


def avatar(
    name: str,
    *,
    initials: str | None = None,
    size: float | None = None,
    presence: str | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """A person. `presence` is one of online, away, busy, offline."""
    return Node(
        "avatar",
        _props(
            class_name,
            style,
            element_id,
            {"name": name, "initials": initials, "size": size, "presence": presence, **props},
        ),
        key=key,
    )


def menu_item(
    label: str,
    *,
    shortcut: str | None = None,
    danger: bool = False,
    disabled: bool = False,
    on_select: Callable[[], Any] | None = None,
    class_name: str | None = None,
    style: Mapping[str, Any] | None = None,
    element_id: str | None = None,
    key: Any = None,
    **props: Any,
) -> Node:
    """One row of a menu."""
    node = Node(
        "menu-item",
        _props(
            class_name,
            style,
            element_id,
            {"label": label, "shortcut": shortcut, "danger": danger, "disabled": disabled, **props},
        ),
        key=key,
    )
    if on_select is not None:
        node.handlers["select"] = on_select
    return node


# --------------------------------------------------------------------------
# Serialisation
# --------------------------------------------------------------------------


def serialize(roots: Sequence[Node]) -> tuple[dict[str, Any], dict[int, dict[str, Callable]]]:
    """Turns nodes into the commit format, and collects their handlers.

    Returns the tree and a map from identity to handlers. The map is rebuilt
    every frame on purpose: the handlers are closures over this frame's state,
    and holding last frame's would call a callback with values the user can no
    longer see.
    """
    handlers: dict[int, dict[str, Callable]] = {}
    seen: set[int] = set()

    def walk(node: Node, path: str) -> dict[str, Any]:
        identity = _fnv1a(path)
        # A collision, or two siblings that were given the same key. Both are
        # the same failure — an ambiguous identity — and the host would refuse
        # the whole commit for it, so it is resolved here where the path is
        # still known.
        while identity in seen:
            identity = _fnv1a(f"{path}\x00{identity}")
        seen.add(identity)

        out: dict[str, Any] = {"id": identity, "type": node.type}
        if node.props:
            out["props"] = node.props
        if node.text is not None:
            out["text"] = node.text
        if node.handlers:
            handlers[identity] = node.handlers
        if node.children:
            out["children"] = [
                walk(child, f"{path}/{_step(child, index)}")
                for index, child in enumerate(node.children)
            ]
        return out

    tree = {
        "children": [walk(node, _step(node, index)) for index, node in enumerate(roots)],
    }
    return tree, handlers


def _step(node: Node, index: int) -> str:
    """One segment of a node's identity path.

    The key when there is one, the position when there is not. That is the
    whole of the identity rule, and it is one line so it cannot drift from the
    docstring at the top of this module.
    """
    return f"{node.type}:{node.key}" if node.key is not None else f"{node.type}[{index}]"


# --------------------------------------------------------------------------
# The application
# --------------------------------------------------------------------------


class App:
    """A window, and the loop that keeps it painted.

    Subclass it, put state on `self`, and return nodes from `render`. There is
    no update call and no invalidation to remember: `render` runs every frame,
    and a frame whose tree is identical to the last is never committed, so an
    idle window costs a `render` and a string comparison.
    """

    #: The window title. Overridable per instance through the constructor.
    title: str = "SphereKit"
    #: The window size, in logical pixels.
    width: float = 900.0
    height: float = 620.0
    #: The stylesheet, installed before the first frame.
    css: str = ""
    #: Whether to use the dark theme.
    dark: bool = True

    def __init__(
        self,
        *,
        title: str | None = None,
        width: float | None = None,
        height: float | None = None,
        css: str | None = None,
        dark: bool | None = None,
    ) -> None:
        if title is not None:
            self.title = title
        if width is not None:
            self.width = width
        if height is not None:
            self.height = height
        if css is not None:
            self.css = css
        if dark is not None:
            self.dark = dark
        self._handlers: dict[int, dict[str, Callable]] = {}
        self.frames = 0
        self.setup()

    # -- to override ------------------------------------------------------

    def setup(self) -> None:
        """Runs once, before the window opens. Put initial state here."""

    def render(self) -> Node | Sequence[Node]:
        """Returns this frame's tree. Called once per frame."""
        raise NotImplementedError("an App must implement render()")

    # -- the loop ---------------------------------------------------------

    def _render_json(self) -> str:
        produced = self.render()
        roots = produced if isinstance(produced, (list, tuple)) else [produced]
        tree, handlers = serialize(_flatten(roots))
        self._handlers = handlers
        self.frames += 1
        # `separators` without spaces, because this string is compared against
        # the previous frame's byte for byte to decide whether to commit at all.
        # Sorted keys for the same reason: a dict that iterated differently
        # would look like a changed frame every time.
        return json.dumps(tree, separators=(",", ":"), sort_keys=True)

    def _dispatch(self, name: str, node_id: int, payload: Mapping[str, Any] | None) -> None:
        handler = self._handlers.get(node_id, {}).get(name)
        if handler is None:
            return
        # A payload with exactly one value is passed as that value: every event
        # the host sends carries either nothing or one number, string or flag,
        # and making a caller write `payload["value"]` for all of them would be
        # ceremony with no information in it. Anything richer arrives whole.
        if payload is None:
            handler()
        elif len(payload) == 1:
            handler(next(iter(payload.values())))
        else:
            handler(payload)

    def run(self, *, frame_limit: int | None = None) -> None:
        """Opens the window and blocks until it closes.

        `frame_limit` stops after that many frames, which is how the demo runs
        unattended in a test.
        """
        _native.run(
            self._render_json,
            self._dispatch,
            title=self.title,
            width=float(self.width),
            height=float(self.height),
            css=self.css,
            dark=self.dark,
            frame_limit=frame_limit,
        )

    @staticmethod
    def quit() -> None:
        """Asks the window to close at the next turn of the loop."""
        _native.quit()


def run(app: App, *, frame_limit: int | None = None) -> None:
    """Runs an application. The function form of `App.run`."""
    app.run(frame_limit=frame_limit)

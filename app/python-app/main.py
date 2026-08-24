"""SphereKit from Python — a small control surface.

Run it:

    cargo build -p spherekit-python --release
    python app/python-app/main.py

Draw a fixed number of frames and exit, which is what CI does:

    python app/python-app/main.py --frames 120

What it shows, in the order it is on screen:

* **State without a redraw call.** Everything is a plain attribute on `self`.
  `render` runs every frame; a frame identical to the last is never committed,
  so an idle window does no layout at all.
* **Every built-in widget** the native host lowers: buttons, a slider, a knob,
  a fader, a toggle, a checkbox, a text field, a progress bar, an avatar, menu
  rows, and a scrolling list.
* **CSS.** The same cascade a React application uses. The stylesheet below is
  parsed once, before the first frame.
* **Keys.** The list rows carry `key=`, so filtering it does not hand row 5's
  identity — and its scroll position — to what used to be row 6.
"""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

# The package lives with the crate that builds the extension, so the demo can
# run straight from a checkout with nothing installed.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "crates" / "spherekit-python" / "python"))

import spherekit as sk  # noqa: E402 - the path has to be set first

STYLESHEET = """
.page {
    display: flex;
    flex-direction: column;
    width: 100%;
    height: 100%;
    padding: 16px;
    gap: 12px;
}

.title { font-size: 22px; font-weight: 700; color: #f2f3f5; }
.subtle { font-size: 12px; color: #9aa0a6; }

.row {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 12px;
}

.columns {
    display: flex;
    flex-direction: row;
    gap: 16px;
    flex: 1 1 0;
    /* The floor a scrolling column needs: without it this row grows to fit the
       list instead of the window, and nothing ever scrolls. */
    min-height: 0;
}

.card {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 14px;
    background: #26282c;
    border-radius: 10px;
    flex: 1 1 0;
    min-height: 0;
}

.count { font-size: 40px; font-weight: 700; color: #8ab4f8; }

.list {
    display: flex;
    flex-direction: column;
    gap: 4px;
    flex: 1 1 0;
    min-height: 0;
}

.list-row {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    border-radius: 6px;
    background: #2f3236;
}

.list-row:hover { background: #3a3e44; }
.done { color: #6f7681; }

.status {
    display: flex;
    flex-direction: row;
    align-items: center;
    height: 26px;
    padding: 0 6px;
    color: #9aa0a6;
    font-size: 12px;
}
"""

TASKS = [
    "Wire the audio thread",
    "Draw the spectrum",
    "Tune the fader curve",
    "Fix the collapsed sidebar",
    "Write the Python binding",
    "Ship it",
]


class ControlSurface(sk.App):
    title = "SphereKit — Python"
    width = 940.0
    height = 660.0
    css = STYLESHEET

    def setup(self) -> None:
        self.count = 0
        self.volume = 0.65
        self.drive = 0.3
        self.level = 0.8
        self.muted = False
        self.show_done = True
        self.name = "Untitled session"
        self.status = "Ready."
        self.done: set[int] = {1}
        self.started = time.monotonic()

    # -- state changes: plain methods, no invalidation to remember ---------

    def add(self) -> None:
        self.count += 1
        self.status = f"Count is {self.count}."

    def reset(self) -> None:
        self.count = 0
        self.status = "Count reset."

    def set_volume(self, value: float) -> None:
        self.volume = value
        self.status = f"Volume {value:.0%}"

    def set_drive(self, value: float) -> None:
        self.drive = value

    def set_level(self, value: float) -> None:
        self.level = value

    def set_muted(self, on: bool) -> None:
        self.muted = on
        self.status = "Muted." if on else "Unmuted."

    def set_show_done(self, on: bool) -> None:
        self.show_done = on

    def rename(self, text: str) -> None:
        self.name = text

    def submit_name(self, text: str) -> None:
        self.status = f"Renamed to {text!r}."

    def toggle_task(self, index: int) -> None:
        self.done.symmetric_difference_update({index})
        self.status = f"{TASKS[index]}: {'done' if index in self.done else 'to do'}."

    # -- the frame --------------------------------------------------------

    def render(self) -> sk.Node:
        elapsed = time.monotonic() - self.started
        # A value that changes every frame, so the window has something moving
        # in it: this is what proves the loop is live rather than painted once.
        sweep = (elapsed % 4.0) / 4.0

        return sk.view(class_name="page")(
            sk.view(class_name="row")(
                sk.avatar("Ada Lovelace", size=32, presence="online"),
                sk.view(style={"flex": "1 1 0"})(
                    sk.text("Python control surface", class_name="title"),
                    sk.text(f"{sk.version()} · frame {self.frames}", class_name="subtle"),
                ),
                sk.button("Quit", on_press=sk.App.quit),
            ),
            sk.separator(),
            sk.view(class_name="columns")(
                self.counter_card(),
                self.mixer_card(sweep),
                self.list_card(),
            ),
            sk.view(class_name="status")(sk.text(self.status)),
        )

    def counter_card(self) -> sk.Node:
        return sk.view(class_name="card")(
            sk.text("Counter", class_name="subtle"),
            sk.text(str(self.count), class_name="count"),
            sk.view(class_name="row")(
                sk.button("Add one", on_press=self.add),
                sk.button("Reset", on_press=self.reset, disabled=self.count == 0),
            ),
            sk.separator(),
            sk.text("Session name", class_name="subtle"),
            sk.text_field(
                self.name,
                placeholder="Name this session",
                on_change=self.rename,
                on_submit=self.submit_name,
            ),
            sk.view(style={"flex": "1 1 0"})(),
            sk.menu_item("Duplicate", shortcut="Ctrl+D",
                         on_select=lambda: self.say("Duplicated.")),
            sk.menu_item("Delete", shortcut="Del", danger=True,
                         on_select=lambda: self.say("Deleted.")),
        )

    def mixer_card(self, sweep: float) -> sk.Node:
        return sk.view(class_name="card")(
            sk.text("Mixer", class_name="subtle"),
            sk.text(f"Volume {self.volume:.0%}"),
            sk.slider(self.volume, on_change=self.set_volume, name="Volume"),
            sk.text(f"Drive {self.drive:.2f}"),
            sk.view(class_name="row")(
                sk.knob(self.drive, on_change=self.set_drive, name="Drive",
                        style={"width": "56px", "height": "56px"}),
                sk.fader(self.level, on_change=self.set_level, name="Level",
                         style={"width": "36px", "height": "110px"}),
            ),
            sk.toggle(self.muted, label="Mute", on_change=self.set_muted),
            sk.separator(),
            sk.text("Render", class_name="subtle"),
            sk.progress(sweep),
        )

    def list_card(self) -> sk.Node:
        rows = [
            (index, task)
            for index, task in enumerate(TASKS)
            if self.show_done or index not in self.done
        ]
        return sk.view(class_name="card")(
            sk.view(class_name="row")(
                sk.text("Tasks", class_name="subtle"),
                sk.view(style={"flex": "1 1 0"})(),
                sk.checkbox(self.show_done, label="Show done", on_change=self.set_show_done),
            ),
            sk.scroll_view(class_name="list")(
                # `key=` on every row. Without it, hiding a finished task hands
                # the row below it the identity of the row that was removed,
                # and the hover state and scroll position follow the identity
                # rather than the task.
                sk.view(class_name="list-row", key=f"task-{index}",
                        on_press=lambda index=index: self.toggle_task(index))(
                    sk.text("done" if index in self.done else "to do", class_name="subtle"),
                    sk.text(task, class_name="done" if index in self.done else None),
                )
                for index, task in rows
            ),
        )

    def say(self, message: str) -> None:
        self.status = message


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--frames",
        type=int,
        default=None,
        help="draw this many frames and exit, for an unattended run",
    )
    args = parser.parse_args()

    app = ControlSurface()
    app.run(frame_limit=args.frames)
    print(f"drew {app.frames} frames")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Finding and loading the compiled extension.

The extension is a normal cargo artifact — ``target/release/spherekit_python.dll``
on Windows, ``.so`` on Linux, ``.dylib`` on macOS — and this module loads it
straight from there.

Why not require a wheel: the whole point of the demo is ``cargo build`` then
``python main.py``. A packaging step between the two would mean anyone trying
the binding has to install maturin, learn a second build system and remember to
re-run it after every edit. ``ExtensionFileLoader`` takes the module *name* and
the file *path* separately, so the ``PyInit__spherekit`` symbol is found in a
file whose name Python would otherwise refuse to import.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import os
import sys
from pathlib import Path
from types import ModuleType

#: The cargo library name, which is what the file on disk is called.
_LIB = "spherekit_python"

#: The module name, which is what the ``PyInit`` symbol is called.
_MODULE = "spherekit_python"

_SUFFIXES = {
    "win32": (".dll",),
    "darwin": (".dylib", ".so"),
}


def _library_names() -> tuple[str, ...]:
    suffixes = _SUFFIXES.get(sys.platform, (".so",))
    prefix = "" if sys.platform == "win32" else "lib"
    return tuple(f"{prefix}{_LIB}{suffix}" for suffix in suffixes)


def repo_root() -> Path:
    """The workspace root, found from this file rather than the caller's cwd."""
    # python/spherekit/_loader.py -> python/spherekit -> python -> crate -> crates -> root
    return Path(__file__).resolve().parents[4]


def candidate_paths() -> list[Path]:
    """Every place the compiled extension might be, best first.

    Release before debug: a debug build of a GPU renderer is slow enough to be
    misleading, so when both exist the fast one is the one that gets loaded. An
    explicit ``SPHEREKIT_LIB`` beats both, because someone pointing at a
    specific binary has said something more definite than either.
    """
    override = os.environ.get("SPHEREKIT_LIB")
    paths: list[Path] = [Path(override)] if override else []
    root = repo_root()
    for profile in ("release", "debug"):
        for name in _library_names():
            paths.append(root / "target" / profile / name)
    return paths


def load() -> ModuleType:
    """Imports the extension, or explains exactly what to run."""
    if _MODULE in sys.modules:
        return sys.modules[_MODULE]

    for path in candidate_paths():
        if not path.is_file():
            continue
        spec = importlib.util.spec_from_file_location(
            _MODULE,
            path,
            loader=importlib.machinery.ExtensionFileLoader(_MODULE, str(path)),
        )
        if spec is None or spec.loader is None:  # pragma: no cover - unreachable in practice
            continue
        module = importlib.util.module_from_spec(spec)
        # Registered before it is executed, because an extension module that
        # imports itself during init would otherwise be built twice.
        sys.modules[_MODULE] = module
        spec.loader.exec_module(module)
        return module

    looked = "\n  ".join(str(path) for path in candidate_paths())
    raise ImportError(
        "the SphereKit extension is not built.\n\n"
        "  cargo build -p spherekit-python --release\n\n"
        f"looked in:\n  {looked}"
    )

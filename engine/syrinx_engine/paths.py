"""Per-OS path resolution — the single source of truth for where the engine
keeps its data and its isolated-worker stderr logs.

On **Linux** every function returns the exact strings used before this module
existed — ``~/.local/share/syrinx`` for data, bare ``~/.cache/syrinx-<name>.log``
for worker logs — so the Linux install changes by zero bytes. The old code
hard-coded ``~/.local/share/syrinx`` and never consulted ``XDG_DATA_HOME``; to
stay byte-identical we do the same. The Linux branch is therefore hand-rolled
rather than delegated to :mod:`platformdirs` (whose ``XDG_DATA_HOME`` support
would diverge from the historical literal whenever that variable is set).

On **Windows/macOS** paths come from :mod:`platformdirs`, so the engine's data
lives beside the RPC discovery file (see ``docs/RPC-PROTOCOL.md`` §2.2):

* data under ``user_data_dir("syrinx", "syrinx")`` —
  ``%LOCALAPPDATA%\\syrinx\\syrinx`` (Windows) /
  ``~/Library/Application Support/syrinx`` (macOS);
* worker logs under ``user_cache_dir("syrinx", "syrinx")``.

``SYRINX_DATA_DIR`` overrides the data dir everywhere, with unchanged semantics.

Imports stay cheap — stdlib plus a *lazy* ``platformdirs`` used only off the
hot Linux path — so the isolated workers that run in their own virtualenvs can
import this module without pulling in the ML stack (or even platformdirs, on
Linux).
"""

import os
import sys
from pathlib import Path


def _default_data_dir() -> Path:
    """The per-OS data dir *before* the ``SYRINX_DATA_DIR`` override."""
    if sys.platform == "linux":
        # Byte-identical to the historical literal; deliberately ignores
        # XDG_DATA_HOME (the old code did too — platformdirs would honor it).
        return Path.home() / ".local" / "share" / "syrinx"
    import platformdirs

    # Windows: %LOCALAPPDATA%\syrinx\syrinx ; macOS: ~/Library/Application Support/syrinx
    return Path(platformdirs.user_data_dir("syrinx", "syrinx"))


def data_dir() -> Path:
    """The engine's data root. ``SYRINX_DATA_DIR`` wins everywhere; otherwise
    the per-OS default (byte-identical to today's on Linux)."""
    override = os.environ.get("SYRINX_DATA_DIR")
    if override:
        return Path(override)
    return _default_data_dir()


def worker_log_path(name: str) -> Path:
    """Stderr-log path for the isolated worker *name* (``syrinx-<name>.log``).

    Linux keeps today's bare ``~/.cache/syrinx-<name>.log``; Win/mac put it
    under the platformdirs user cache dir."""
    if sys.platform == "linux":
        return Path.home() / ".cache" / f"syrinx-{name}.log"
    import platformdirs

    return Path(platformdirs.user_cache_dir("syrinx", "syrinx")) / f"syrinx-{name}.log"

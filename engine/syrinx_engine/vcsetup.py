"""One-click installer for the engines that live in their own virtualenvs.

Seed-VC (GPL-3.0), Vevo's Amphion checkpoints (CC-BY-NC) and LuxTTS (Apache-2.0,
git-only — the PyPI ``zipvoice`` name is an unrelated stub) are deliberately
never bundled with Syrinx — they are installed per-user, on request, into their
own virtualenvs by ``engine/setup-{seedvc,vevo,luxtts}.{sh,ps1}``. Historically
that meant telling the user to "run engine/setup-vevo.sh first", which is a dead
end for anyone who never opens a terminal (and names a ``.sh`` path on Windows).
This module lets the app run those exact same scripts itself and stream their
progress, so the opt-in stays an explicit user decision but costs one click.

Two of the three are ⇄ conversion engines and one (LuxTTS) is a cloning voice,
but nothing here cares: a setup is a script, a venv and a landmark package.

It is deliberately **stdlib + :mod:`.paths` only**: it has to be importable on a
box where nothing ML is installed yet — that is the entire point of the feature —
and it must never drag torch into the Models tab's status query.

Three environment variables steer it (all documented in ``docs/RPC-PROTOCOL.md``):

* ``SYRINX_VC_VENV_DIR`` — where the setup script puts its ``.venv-<x>``. Unset
  means "beside the script", which is what a human running the script by hand
  has always got; the engine now sets it on **every** platform, to a per-setup
  slice of the data dir (see :func:`venv_root`).
* ``SYRINX_VC_SETUP_DIR`` — escape hatch pointing at the scripts directly, for
  layouts the ancestor walk below does not anticipate.
* ``SYRINX_VC_SETUP_TIMEOUT`` — seconds before a wedged install is killed.
"""

import asyncio
import os
import re
import shutil
import subprocess
import sys
from collections import deque
from dataclasses import dataclass
from pathlib import Path

from .paths import data_dir, worker_log_path

# Module-level switches rather than inline ``sys.platform`` tests so the per-OS
# branches (venv layout, which script, which interpreter search, whether to
# bootstrap Python/Git) are all reachable from a test suite that only ever runs
# on one host OS. ``_PLATFORM`` is the three-way for wording that names a
# package manager; ``_IS_WIN`` stays the two-way for behavior.
_PLATFORM = sys.platform
_IS_WIN = sys.platform == "win32"

# The package dir; ``script_path`` walks up from here. Module-level so tests can
# stand up fake checkout / installed trees without a real package on disk.
_PKG_DIR = Path(__file__).resolve().parent

_DEFAULT_TIMEOUT = 5400  # 90 minutes — a cold torch+deps install on a slow link


class VcSetupError(RuntimeError):
    """A setup that cannot even be *started*, phrased for the user.

    ``str(exc)`` is shown verbatim in the app's error banner, so every message
    raised here is a complete one-line sentence ending in what to do next.
    """


@dataclass(frozen=True)
class _Setup:
    id: str          # the wire vocabulary: "seedvc" | "vevo" | "luxtts"
    stem: str        # engine/setup-<stem>.sh / .ps1
    venv: str        # the venv the script creates: .venv-<venv>
    py_env: str      # env var the script reads for its base interpreter
    subdir: str      # per-setup slice of the data dir (venv root on Windows)
    engine: str      # models.ModelSpec.engine this setup unblocks
    landmark: str    # site-packages dir only a finished critical stage leaves
    needs_git: bool  # the script clones something and needs git on PATH


SETUPS: "dict[str, _Setup]" = {
    # landmark seed_vc: dropped by the `pip install seed-vc` stage, which runs
    # after venv+torch — the exact stage a 2026-07-28 field failure died in.
    "seedvc": _Setup("seedvc", "seedvc", "seedvc", "SYRINX_SEEDVC_PYTHON",
                     "seedvc", "seed_vc", "seed_vc", needs_git=False),
    # vevo covers BOTH catalog rows (vevo-timbre and vevo2-singing) — they share
    # the engine name, the venv and the Amphion clone, so one install clears both.
    # landmark torchcrepe: part of the single big `deps` install, and one of the
    # packages the worker proved Amphion imports without declaring.
    "vevo": _Setup("vevo", "vevo", "vevo", "SYRINX_VEVO_PYTHON",
                   "vevo", "vevo_timbre", "torchcrepe", needs_git=True),
    # needs_git: setup-luxtts pip-installs LinaCodec and LuxTTS from two pinned
    # git SHAs — neither project publishes a usable release.
    # landmark zipvoice: dropped by the critical `luxtts` stage, which is the
    # LAST thing that can fail (venv, torch and phonemize all had to succeed to
    # reach it) and is literally the package luxtts_worker.py imports. Not
    # piper_phonemize, even though that is also a dir this venv gets: the
    # phonemize stage runs BEFORE luxtts, so an install torn open in the luxtts
    # stage would still have it and would falsely read as finished.
    "luxtts": _Setup("luxtts", "luxtts", "luxtts", "SYRINX_LUXTTS_PYTHON",
                     "luxtts", "luxtts", "zipvoice", needs_git=True),
}

SETUP_IDS = tuple(SETUPS)

# Derived, not written out twice:
# {"seed_vc": "seedvc", "vevo_timbre": "vevo", "luxtts": "luxtts"}.
ENGINE_TO_SETUP = {s.engine: s.id for s in SETUPS.values()}

# The scripts print these on stdout before each phase; everything else they emit
# is prose for the log. Tokens (not sentences) so the wording can change here
# without touching four scripts — and so the .sh/.ps1 pair can be pinned equal.
STAGE_MARKER = "== syrinx-stage:"

STAGE_LABELS = {
    "amphion": "fetching the Amphion source…",
    "venv": "creating the isolated environment…",
    "torch": "installing PyTorch — the big one (several GB)…",
    "seedvc": "installing Seed-VC…",
    "demucs": "installing Demucs (music mode)…",
    "deps": "installing the Vevo dependencies…",
    "phonemize": "installing the espeak phonemizer…",
    "luxtts": "installing LuxTTS…",
    "pins": "pinning transformers and huggingface_hub…",
    "verify": "verifying the install…",
}

# Windows only: keep every probe/bootstrap child out of the user's face. The app
# is a GUI, so a flashing console window would look like a crash.
_NO_WINDOW = {"creationflags": subprocess.CREATE_NO_WINDOW} if _IS_WIN else {}

# A candidate interpreter must import BOTH venv and ensurepip and report 3.12.
# That triple rejects the two decoys a stock Windows box is full of: the Store
# stub (which exits non-zero and advertises itself) and embedded/python.org
# "embeddable" builds (no ensurepip, so `-m venv` produces a pip-less shell).
_PROBE_CODE = (
    "import ensurepip, sys, venv; "
    "print('syrinx-python', sys.version_info[0], sys.version_info[1], sys.executable)"
)

# What to tell the user when no interpreter answers, per OS — the same shape as
# the SoX hint in backends/qwen.py. Windows is the only platform we bootstrap a
# Python on, so its sentence is what is left after winget came up empty; naming
# python.org on a Mac or a Linux box would send the user to the one installer
# their machine does NOT want.
_NO_PYTHON_HINTS = {
    "win32": "install Python 3.12 from python.org, then click Install again.",
    "darwin": "install Python 3.12 (brew install python@3.12, or "
              "uv python install 3.12), then click Install again.",
    "linux": "install your distribution's python3.12 package, then click "
             "Install again.",
}


# The same shape, for the other prerequisite. macOS names xcode-select rather
# than git-scm.com on purpose: /usr/bin/git is already there as a stub, one
# command turns it into a real git, and sending a Mac user to a downloadable
# installer would leave two gits on the box.
_NO_GIT_HINTS = {
    "darwin": "run `xcode-select --install` in Terminal to get Apple's command "
              "line tools, then click Install again.",
    "linux": "install your distribution's git package, then click Install again.",
}


def _no_python() -> str:
    return _NO_PYTHON_HINTS.get(_PLATFORM, "install Python 3.12, then click Install again.")


# --- locations --------------------------------------------------------------


def engine_dir() -> Path:
    """The ``engine/`` directory as seen from an editable/checkout install."""
    return _PKG_DIR.parent


def amphion_dir() -> Path:
    """Where the Amphion clone lives — the single source of truth.

    Mirrors vevo_worker.py exactly (env override, else ``<data>/vevo/Amphion``);
    models.py aliases this so the Models tab and the worker can never disagree
    about whether the clone is present.
    """
    override = os.environ.get("SYRINX_VEVO_AMPHION")
    return Path(override) if override else data_dir() / "vevo" / "Amphion"


def venv_root(setup_id: str) -> Path:
    """The directory a setup's venv is created *inside*. Every platform.

    Under the data dir rather than beside the script. Windows moved first, for
    length: an installed Syrinx puts the engine at
    ``…\\engine\\.venv\\Lib\\site-packages``, and a second venv nested under
    that overflows MAX_PATH the moment pip unpacks torch. macOS forced the same
    move for a harder reason — a shipped Syrinx.app is *code-signed*, and a
    signature seals a hash of every file under ``Contents/``. Creating a
    ``.venv-luxtts`` beside the setup script inside the bundle does not merely
    look untidy: ``codesign --verify`` flips to "file added" on the spot, and a
    bundle whose seal is broken eventually loses its TCC grants (the
    microphone, the system-audio tap, Accessibility) because the designated
    requirement no longer matches.

    Applied to **all** POSIX rather than gated on ``darwin``, so there is one
    placement rule and not three. Linux is the platform that could have kept
    the old location — it installs from a checkout, which nobody signs — but a
    per-OS split would mean the beside-the-script layout survived in exactly
    one place and had to be reasoned about forever. It costs Linux nothing:
    :func:`venv_candidates` still finds venvs at the old location, and a
    developer running ``bash engine/setup-vevo.sh`` by hand (no
    ``SYRINX_VC_VENV_DIR`` in the environment) still gets one there.
    """
    return data_dir() / SETUPS[setup_id].subdir


def script_path(setup_id: str) -> "Path | None":
    """The setup script for *setup_id* on this OS, or None if it wasn't shipped.

    ``SYRINX_VC_SETUP_DIR`` wins outright. Otherwise walk up from the package
    dir. Three layouts have to land inside the walk:

    * a checkout — ``engine/syrinx_engine`` → ``engine/`` at **one**;
    * the installed Windows tree —
      ``engine\\.venv\\Lib\\site-packages\\syrinx_engine`` → **four**;
    * the macOS bundle —
      ``Contents/Resources/engine/.venv/lib/python3.12/site-packages/syrinx_engine``
      → **five**, which is the whole budget. It is deliberately not raised for
      headroom: five is exactly the deepest layout we ship, one more level
      would start matching directories no layout of ours owns, and
      ``tests/test_vcsetup.py`` pins each of the three so a future layout that
      buries the package deeper fails there rather than in the field.

    Returning None (rather than guessing) is what lets the caller say "this
    build shipped without the setup scripts" instead of failing with a
    confusing spawn error.
    """
    s = SETUPS.get(setup_id)
    if s is None:
        return None
    name = f"setup-{s.stem}." + ("ps1" if _IS_WIN else "sh")
    override = os.environ.get("SYRINX_VC_SETUP_DIR")
    if override:
        cand = Path(override) / name
        return cand if cand.is_file() else None
    for base in (_PKG_DIR, *list(_PKG_DIR.parents)[:5]):
        cand = base / name
        if cand.is_file():
            return cand
    return None


def venv_candidates(setup_id: str) -> "list[Path]":
    """Every interpreter path a setup's venv could plausibly live at, best first.

    Two on every platform, and the same two for the same reason: the data-dir
    location (:func:`venv_root`) is what the engine asks for now, and the
    legacy beside-the-script location is kept as a lower-priority candidate so
    a venv that already exists — hand-built by a developer, or built by a
    Syrinx from before the move — keeps being found instead of silently
    reading as "not installed" and asking for a multi-GB reinstall.

    One list for both OSes rather than a branch: the only per-OS fact left is
    where a venv keeps its interpreter, which is the ``rel`` tuple.
    """
    s = SETUPS[setup_id]
    rel = ("Scripts", "python.exe") if _IS_WIN else ("bin", "python")
    name = f".venv-{s.venv}"
    return [
        venv_root(setup_id).joinpath(name, *rel),
        engine_dir().joinpath(name, *rel),
    ]


def site_packages(venv_dir: Path) -> "list[Path]":
    """Every ``site-packages`` inside *venv_dir*, per-OS.

    Windows pins the one location; POSIX buries it under the minor version, so
    glob rather than ask what the interpreter *we are running* is — the setup
    venv is built by a different (possibly different-version) Python than the
    engine's own, and hard-coding ours would miss it.
    """
    if _IS_WIN:
        return [venv_dir / "Lib" / "site-packages"]
    return sorted(venv_dir.glob("lib/python*/site-packages"))


def _has_landmark(s: _Setup, python: Path) -> bool:
    """Did the setup's critical install stage actually finish in this venv?

    ``python.parent.parent`` is the venv root for both layouts (``bin/python``,
    ``Scripts\\python.exe``), so this asks the venv the interpreter was found
    in — not whichever candidate we would prefer today.
    """
    return any((sp / s.landmark).is_dir() for sp in site_packages(python.parent.parent))


def venv_python(setup_id: str) -> "Path | None":
    """The venv's interpreter, or None when the setup hasn't been run.

    Checking the *interpreter* rather than the venv directory is deliberate: a
    torn install (killed mid-``pip``, or a cancel) leaves the directory behind
    and the old directory-existence check called that "installed".

    ``exists()`` follows symlinks, and that is load-bearing on macOS rather
    than incidental. ``python -m venv`` on POSIX symlinks ``bin/python`` at the
    interpreter it was run from, which for an install driven by the packaged
    app is a path *inside* ``Syrinx.app``. Move or delete the bundle and that
    symlink dangles — ``exists()`` says False, this candidate is skipped, and
    the row reads "not installed" and offers Install again. That is the failure
    mode we want: a dead symlink must never be handed to a worker, where it
    would surface as an ``ENOENT`` from ``posix_spawn`` with no explanation.

    When more than one candidate is present the landmark decides, which is a
    guard the second candidate created. A torn install at the data-dir location
    (venv made, ``pip install torch`` died) would otherwise shadow a perfectly
    good legacy venv beside the script and take a working engine away from
    someone who had one — the exact "installed but not usable" window
    :func:`installed` exists to close, re-opened at the other end. Preferring a
    candidate that carries the landmark closes it again; with nothing to choose
    between (no landmark anywhere) the order in :func:`venv_candidates` stands.
    """
    if setup_id not in SETUPS:
        return None
    found = [c for c in venv_candidates(setup_id) if c.exists()]
    if not found:
        return None
    s = SETUPS[setup_id]
    return next((c for c in found if _has_landmark(s, c)), found[0])


def installed(setup_id: str) -> bool:
    """Is this conversion engine actually usable right now?

    The interpreter alone is not the answer. ``setup-seedvc.ps1`` creates the
    venv and installs torch in stages *before* the one that installs seed-vc
    itself, so a failure there (2026-07-28 field report) left a perfectly good
    python.exe behind — the Models row cleared its warning and hid the Install
    button for an engine the worker could not import. Requiring a landmark
    package directory closes that window: pip is all-or-nothing per command, so
    any package from the critical command proves the whole command ran.
    """
    if setup_id not in SETUPS:
        return False
    python = venv_python(setup_id)
    if python is None:
        return False
    if not _has_landmark(SETUPS[setup_id], python):
        return False
    # The Vevo worker needs BOTH the venv and the Amphion clone; a restored data
    # dir can have one without the other (2026-07-24 field report).
    if setup_id == "vevo" and not amphion_dir().exists():
        return False
    return True


# --- prerequisites: a base Python 3.12 (and, on Windows, Git) ---------------


def _probe_python(argv: "list[str]") -> str:
    """Run the interpreter probe through *argv*; the real interpreter path on
    success, ``""`` on any failure. Never raises — every candidate here is a
    guess, and a guess that isn't installed must just be the next one's turn."""
    try:
        out = subprocess.run([*argv, "-c", _PROBE_CODE], capture_output=True,
                             text=True, timeout=20, **_NO_WINDOW)
    except (OSError, subprocess.SubprocessError):
        return ""
    if out.returncode != 0:
        return ""
    for line in (out.stdout or "").splitlines():
        parts = line.strip().split(None, 3)
        if len(parts) == 4 and parts[:3] == ["syrinx-python", "3", "12"]:
            return parts[3]
    return ""


def _fixed_python_candidates() -> "list[list[str]]":
    """Full paths a python.org 3.12 lands at — the set we re-probe after a
    winget bootstrap, because our own PATH was captured before it ran."""
    cands = []
    local = os.environ.get("LOCALAPPDATA")
    if local:
        cands.append([str(Path(local) / "Programs" / "Python" / "Python312" / "python.exe")])
    for var in ("ProgramFiles", "ProgramFiles(x86)"):
        root = os.environ.get(var)
        if root:
            cands.append([str(Path(root) / "Python312" / "python.exe")])
    cands.append([r"C:\Python312\python.exe"])
    return cands


def _python_candidates() -> "list[list[str]]":
    """The full pre-winget probe order (Windows)."""
    # `py -3.12` first: the launcher knows about every registered install,
    # including ones in directories we'd never guess.
    cands = [["py", "-3.12"]]
    cands += _fixed_python_candidates()
    for name in ("python3.12", "python3", "python"):
        found = shutil.which(name)
        if found:
            cands.append([found])
    return cands


def _base_python() -> str:
    """The interpreter our OWN venv was built from, or ``""``.

    The self-answer: any box that is running this engine has a working CPython
    3.12 on it by definition — the one that built ``engine/.venv``. ``venv``
    itself uses ``sys._base_executable`` for exactly this, and it is the only
    attribute that names the real binary rather than the venv's shim; the
    ``base_prefix`` walk covers an embedder that left it unset.
    """
    base = getattr(sys, "_base_executable", "") or ""
    if base and Path(base).is_file():
        return base
    prefix = Path(sys.base_prefix)
    for name in (f"python{sys.version_info[0]}.{sys.version_info[1]}", "python3"):
        cand = prefix / "bin" / name
        if cand.is_file():
            return str(cand)
    return ""


def _bundled_python() -> str:
    """Our own interpreter, but only when it is the one shipped inside a
    macOS ``.app`` — otherwise ``""``.

    A packaged Syrinx carries a whole relocatable CPython 3.12 at
    ``Syrinx.app/Contents/Resources/engine/.venv``, and that is the interpreter
    the worker venvs should be built from. Any CPython 3.12 would *work* — a
    worker venv installs its own torch and its own everything, and shares
    nothing with ours but a C ABI — so this is not correctness, it is
    hermeticity: the bundled interpreter is the only 3.12 on the machine whose
    existence is guaranteed by the app being there at all.

    Without this, the PATH probe below decides, and what it finds depends on
    how the app was started. LaunchServices hands a Finder-launched app a
    four-entry PATH with no ``python3.12`` on it, so Finder already lands here
    via :func:`_base_python`; a shell-launched one inherits the user's PATH and
    can land on a brew or uv 3.12 instead. Same app, same click, two different
    interpreters underneath a 4 GB install is not a difference worth having —
    especially since the alternative is an interpreter the user can
    ``brew uninstall`` out from under a venv that symlinks into it.

    The substring test rather than a ``sys.platform`` check plus a path walk:
    ``.app/Contents/`` is the one thing that is true of a bundled interpreter
    and false of every checkout, uv store and Homebrew prefix. It is empty on
    Linux and on a macOS dev checkout, so the ladder below is unchanged there.
    """
    base = _base_python()
    return base if ".app/Contents/" in base else ""


def _uv_python() -> str:
    """What ``uv python find 3.12`` points at, or ``""`` — guarded end to end.

    uv keeps its interpreter downloads in its own data directory and off PATH,
    so a box whose only 3.12 is a uv one that did not happen to build our venv
    is invisible to every other probe. uv being absent, old, or answering
    "no such version" are all just "next candidate", never an error.
    """
    uv = shutil.which("uv")
    if not uv:
        return ""
    try:
        out = subprocess.run([uv, "python", "find", "3.12"], capture_output=True,
                             text=True, timeout=60, **_NO_WINDOW)
    except (OSError, subprocess.SubprocessError):
        return ""
    if out.returncode != 0:
        return ""
    for line in (out.stdout or "").splitlines():
        if line.strip():
            return line.strip()
    return ""


def _posix_python_candidates():
    """The POSIX probe order, best first — a generator because the last step
    costs a subprocess of its own, which a box that answered earlier must not pay.

    0. The interpreter inside our own ``.app``, when there is one (see
       :func:`_bundled_python`). Nothing else on a user's Mac is as certain to
       still be there tomorrow, and a packaged app that resolved differently
       depending on whether it was double-clicked or run from a terminal would
       be needlessly hard to support. Empty everywhere else, so 1–3 below are
       untouched on Linux and in a dev checkout.
    1. ``python3.12`` on PATH. This is what the setup scripts' own
       ``${SYRINX_*_PYTHON:-python3.12}`` default has always resolved to, so
       probing it first keeps Linux landing on the exact same interpreter it
       lands on today — the change is a no-op there by construction.
    2. Our own base interpreter (see :func:`_base_python`). This is the macOS
       *checkout* fix: the app's LaunchServices environment hands the engine a
       four-entry PATH with no ``~/.local/bin`` in it, so on a uv-managed Mac
       step 1 finds nothing and the script died with "python3.12: command not
       found" (field report, 2026-07-30) even though a perfectly good 3.12 was
       running it.
    3. ``uv python find 3.12`` — the last resort for a 3.12 that exists but is
       neither on PATH nor ours.
    """
    for finder in (_bundled_python, lambda: shutil.which("python3.12"),
                   _base_python, _uv_python):
        found = finder()
        if found:
            yield [found]


def _winget() -> str:
    """Path to winget, or ``""``. ``which`` first; the WindowsApps alias is the
    fallback because that directory is missing from some service PATHs."""
    found = shutil.which("winget")
    if found:
        return found
    local = os.environ.get("LOCALAPPDATA")
    if local:
        alias = Path(local) / "Microsoft" / "WindowsApps" / "winget.exe"
        if alias.exists():
            return str(alias)
    return ""


def _winget_install(winget: str, package_id: str) -> None:
    """Best-effort winget install. The return code is deliberately not fatal:
    "no applicable upgrade / already installed" is a *success* for us, and the
    caller re-probes for the real artifact anyway — the filesystem is the only
    answer we trust."""
    try:
        subprocess.run(
            [winget, "install", "--id", package_id, "--exact", "--silent",
             "--source", "winget", "--accept-package-agreements",
             "--accept-source-agreements"],
            capture_output=True, text=True, timeout=1800, **_NO_WINDOW,
        )
    except (OSError, subprocess.SubprocessError):
        pass


def resolve_python(setup_id: str, on_stage=None) -> str:
    """A venv-capable Python 3.12 for *setup_id*, installing one if need be.

    Every platform. It used to be Windows-only, on the theory that "a POSIX box
    that can run Syrinx already has python3" — true, but the scripts asked for
    it by the bare name ``python3.12``, and a macOS app launched from Finder
    inherits a PATH that a uv-managed 3.12 is not on. Probing beats naming.

    The bootstrap tail (winget) stays Windows-only: it is the one platform
    where a missing Python is something we can fix for the user unattended.
    Raises :class:`VcSetupError` with a user-facing sentence when it comes up
    empty.
    """
    s = SETUPS[setup_id]
    explicit = os.environ.get(s.py_env, "")
    if explicit:
        # An override that doesn't work is a hard error, never a silent
        # fallback: someone set it on purpose and needs to hear it's wrong.
        found = _probe_python([explicit])
        if not found:
            raise VcSetupError(
                f"{s.py_env} points at {explicit}, which is not a usable "
                "Python 3.12 (it needs the venv and ensurepip modules)."
            )
        return found
    for argv in (_python_candidates() if _IS_WIN else _posix_python_candidates()):
        found = _probe_python(argv)
        if found:
            return found
    if not _IS_WIN:
        raise VcSetupError(f"no usable Python 3.12 was found — {_no_python()}")
    winget = _winget()
    if not winget:
        raise VcSetupError(
            "no Python 3.12 was found on this PC and winget is not available "
            f"to install one — {_no_python()}"
        )
    if on_stage:
        on_stage("installing Python 3.12…")
    _winget_install(winget, "Python.Python.3.12")
    for argv in _fixed_python_candidates():
        found = _probe_python(argv)
        if found:
            return found
    raise VcSetupError(
        f"Python 3.12 could not be installed automatically — {_no_python()}")


def _git_works(exe: str) -> bool:
    try:
        out = subprocess.run([exe, "--version"], capture_output=True, text=True,
                             timeout=20, **_NO_WINDOW)
    except (OSError, subprocess.SubprocessError):
        return False
    return out.returncode == 0


def ensure_git(on_stage=None) -> str:
    """Make sure ``git`` will resolve for the setup child, installing it if not.

    Returns a directory to PREPEND to the child's PATH (``""`` when git already
    works). A fresh winget install of Git updates the machine PATH, but our own
    environment block was captured at engine start-up and will not see it —
    hence the explicit prepend rather than trusting inheritance.

    Windows is the only platform we *install* on. Everywhere else this is a
    pre-flight: it fails the setup before the first download rather than
    letting ``git clone`` (vevo) or a ``pip install git+https://`` (luxtts) die
    somewhere in the middle of a multi-GB install with a message about a
    resolver. macOS deliberately does not vendor git — ``/usr/bin/git`` is a
    stub that installs the Xcode Command Line Tools on first use, so a bare Mac
    gets a system dialog and one command, not a broken app.
    """
    if _git_works("git"):
        return ""
    if not _IS_WIN:
        raise VcSetupError(
            "Git is needed to fetch this engine's source but is not installed "
            f"— {_NO_GIT_HINTS.get(_PLATFORM, _NO_GIT_HINTS['linux'])}"
        )
    winget = _winget()
    if not winget:
        raise VcSetupError(
            "Git is needed to fetch the Amphion source but is not installed, "
            "and winget is not available to install it — install Git from "
            "git-scm.com, then click Install again."
        )
    if on_stage:
        on_stage("installing Git…")
    _winget_install(winget, "Git.Git")
    for var in ("ProgramFiles", "ProgramFiles(x86)"):
        root = os.environ.get(var)
        if not root:
            continue
        cmd_dir = Path(root) / "Git" / "cmd"
        if _git_works(str(cmd_dir / "git.exe")):
            return str(cmd_dir)
    raise VcSetupError(
        "Git could not be installed automatically — install Git from "
        "git-scm.com, then click Install again."
    )


# --- running a setup --------------------------------------------------------


def _timeout_secs() -> float:
    try:
        return float(os.environ.get("SYRINX_VC_SETUP_TIMEOUT", "") or _DEFAULT_TIMEOUT)
    except ValueError:
        return float(_DEFAULT_TIMEOUT)


def _powershell() -> str:
    """PowerShell 7 when present, else the 5.1 that ships with Windows. The
    scripts' ``Invoke-Checked`` covers 5.1's native-exit-code gap, so both run."""
    return shutil.which("pwsh") or "powershell.exe"


def _command(script: Path) -> "list[str]":
    """The argv that runs *script*. A seam, not just a branch: the streaming
    tests point this at a fake ``python -c`` child so they exercise the reader
    without ever launching a multi-GB real install."""
    if _IS_WIN:
        return [_powershell(), "-NoProfile", "-ExecutionPolicy", "Bypass",
                "-File", str(script)]
    return ["bash", str(script)]


# pip rewrites one line with CR and never emits LF, and a resolver backtrack can
# print a single line far longer than StreamReader's 64 KiB limit — so read raw
# chunks and split ourselves. ``readline()`` would stall on the first and raise
# on the second.
_LINE_SPLIT = re.compile(rb"[\r\n]")

# Long-path failures are the one setup error whose fix the user cannot guess.
_LONG_PATH_HINTS = ("206", "path too long", "filename or extension is too long")

# Belt and braces for the NO_COLOR/TERM pair below: a child that colours its
# output anyway must not get escape bytes into the log file or the app's error
# banner — a 2026-07-28 field failure surfaced as "[31;1m[0m[36;1m…" in the UI.
# Two arms, and deliberately no more: CSI (``ESC [`` … final byte) is every
# colour and cursor sequence anything in this pipeline emits, and OSC
# (``ESC ]`` … BEL or ST) is the window-title strings a shell writes.
_ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\].*?(?:\x07|\x1b\\)")

# PowerShell frames a failed native command with a caret diagram and a
# "Program … ended with non-zero exit code" line, and pip closes a failed wheel
# build with two boilerplate lines of its own. Every one of them is printed
# AFTER the sentence that says what actually broke, so "the last line" — which
# is what this used to report — reliably picks the least useful one. Skip them
# while scanning back for the real error.
_DECORATION = re.compile(
    r"""^\s*(
          \|                                  #      | Program "python.exe" ended…
        | Line\s*\|                           # Line |
        | \d+\s*\|                            #   35 |     & $Args[0] @(…)
        | ~+\s*$                              #      |     ~~~~~~~~~
        | \S*NativeCommandExitException       # …the exception class + script:line
        | \[end\ of\ output\]                 # pip's failed-build wrapper
        | note:\ This\ error\ originates\ from\ a\ subprocess
    )""",
    re.VERBOSE | re.IGNORECASE,
)

# The line worth putting in a banner announces itself. Scanning backwards for
# this beats the last line whenever a build tool summarises after it fails.
_ERROR_LINE = re.compile(r"^\s*(error|fatal)\b", re.IGNORECASE)

# The banner is one line of chrome in a scrolling list, not a log viewer: past
# this the message stops being readable and starts being a wall (and the log
# path appended after it is the actionable half).
_MAX_REASON = 240


class VcSetupManager:
    """Owns the at-most-one-per-id, one-at-a-time execution of the setups."""

    def __init__(self) -> None:
        # Claims are taken SYNCHRONOUSLY, before any await, so a double-clicked
        # Install button cannot squeeze two tasks past the check.
        self._claimed: "set[str]" = set()
        self._procs: "dict[str, asyncio.subprocess.Process]" = {}
        self._cancelled: "set[str]" = set()
        # All three setups install torch. Running them concurrently would
        # multiply the download and thrash a small disk, so they queue.
        self._lock = asyncio.Lock()

    def claim(self, setup_id: str) -> bool:
        """Reserve *setup_id*. False = unknown id, or one is already running."""
        if setup_id not in SETUPS or setup_id in self._claimed:
            return False
        self._claimed.add(setup_id)
        self._cancelled.discard(setup_id)
        return True

    def running(self, setup_id: str) -> bool:
        return setup_id in self._claimed

    def cancel(self, setup_id: str) -> bool:
        """Kill a running setup's child. False = nothing was running.

        The ``cancelled`` progress event is emitted by :meth:`install`'s own
        exit path rather than here, so there is exactly one terminal event per
        install no matter which way it ends (and no window where a cancel that
        races completion emits two).
        """
        proc = self._procs.get(setup_id)
        if proc is None:
            return False
        self._cancelled.add(setup_id)
        try:
            proc.kill()
        except (ProcessLookupError, OSError):
            pass
        return True

    async def install(self, setup_id: str, on_progress) -> bool:
        """Run *setup_id*'s script to completion, streaming stages.

        The caller must already hold a :meth:`claim`; this releases it. Every
        exit path emits exactly one terminal ``done``/``error``/``cancelled``
        event, because the app clears its "installing" state on that event and
        would otherwise wedge with a spinner forever.
        """
        s = SETUPS.get(setup_id)
        if s is None:
            return False
        log_path = worker_log_path("setup-" + setup_id)

        def stage(label: str) -> None:
            on_progress(setup_id, label, "running", "")

        def fail(reason: str) -> bool:
            on_progress(setup_id, "failed", "error", f"{reason} · log: {log_path}")
            return False

        try:
            if self._lock.locked():
                stage("waiting for the other install…")
            async with self._lock:
                try:
                    return await self._run(s, log_path, on_progress, stage, fail)
                except VcSetupError as e:
                    return fail(str(e))
                except Exception as e:  # noqa: BLE001 — the banner beats a traceback
                    return fail(f"the setup could not be started: {e}")
        finally:
            self._claimed.discard(setup_id)
            self._procs.pop(setup_id, None)

    async def _run(self, s: _Setup, log_path: Path, on_progress, stage, fail) -> bool:
        script = script_path(s.id)
        if script is None:
            return fail(
                "this build of Syrinx shipped without the setup scripts — "
                "install from a full build, or point SYRINX_VC_SETUP_DIR at them"
            )

        env = dict(os.environ)
        # Quiet, non-interactive children: a progress bar is noise in a log file
        # and a credential prompt on a headless child hangs until the timeout.
        env.update({
            "PYTHONUNBUFFERED": "1",
            "PIP_PROGRESS_BAR": "off",
            "PIP_NO_INPUT": "1",
            "PIP_DISABLE_PIP_VERSION_CHECK": "1",
            "UV_NO_PROGRESS": "1",
            "GIT_TERMINAL_PROMPT": "0",
            # Colour is for a terminal, and this child has none: its output
            # goes to a log file and — one line of it — to an error banner.
            # Proven in the field on 2026-07-28: the same failing install run
            # from a harness shell that had NO_COLOR set logged plain text,
            # while the app's chain (which did not) put raw "[31;1m" escapes
            # in the banner. PowerShell 7.2+ switches $PSStyle to plain text
            # on NO_COLOR; pip, uv and git honour one or both of these.
            "NO_COLOR": "1",
            "TERM": "dumb",
            # Pass the resolved clone location explicitly. setup-vevo.sh derives
            # its own default from $HOME and ignores SYRINX_DATA_DIR, so without
            # this the script and the worker could disagree about where Amphion
            # is; passing it makes them agree by construction (and is a no-op
            # when the variable was already set).
            "SYRINX_VEVO_AMPHION": str(amphion_dir()),
        })

        # Every platform. The scripts default to a bare `python3.12`, which is
        # a name, not a location: it is right on Linux, absent on a stock
        # Windows box, and absent from the four-entry PATH a Finder-launched
        # macOS app hands us even when a 3.12 is running this very process
        # (field report, 2026-07-30). Resolving it here tells the child exactly
        # which interpreter to build its venv from — and on Linux that is the
        # same one the bare name already resolved to, so nothing moves.
        env[s.py_env] = resolve_python(s.id, stage)

        # Also every platform, since phase 3 of the macOS packaging campaign.
        # The scripts' `${SYRINX_VC_VENV_DIR:+…/}` expansion means unset is
        # still "beside the script" for a human running one by hand; setting it
        # here is what keeps an app-driven install out of a signed .app bundle
        # (macOS) and off the far side of MAX_PATH (Windows). See venv_root.
        #
        # Created before the child starts rather than left to `python -m venv`:
        # the script asks for `$SYRINX_VC_VENV_DIR/.venv-<x>`, and venv only
        # makes the LAST component of that path.
        env["SYRINX_VC_VENV_DIR"] = str(venv_root(s.id))
        Path(env["SYRINX_VC_VENV_DIR"]).mkdir(parents=True, exist_ok=True)

        if s.needs_git:
            extra_path = ensure_git(stage)
            if extra_path:
                env["PATH"] = extra_path + os.pathsep + env.get("PATH", "")

        log_path.parent.mkdir(parents=True, exist_ok=True)
        tail: "deque[str]" = deque(maxlen=40)
        proc = await asyncio.create_subprocess_exec(
            *_command(script),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
            env=env,
            # The venv root, NOT the script's directory. That used to be the
            # same answer and is not any more: on macOS the script's directory
            # is inside a code-signed Syrinx.app, and a setup child spends its
            # life running pip and git — tools that write scratch files
            # relative to cwd when they feel like it (a legacy sdist build
            # dir, an egg-info, a git lock). One of those landing under
            # Contents/ breaks the bundle's resource seal exactly as surely as
            # the venv would have. The scripts `cd` for themselves too, to the
            # same place and for the same reason; both are needed, because
            # neither can assume the other ran.
            cwd=env["SYRINX_VC_VENV_DIR"],
            **_NO_WINDOW,
        )
        self._procs[s.id] = proc

        with open(log_path, "a", encoding="utf-8", errors="replace") as logf:

            def handle(raw: bytes) -> None:
                # Strip escapes before anything else sees the line: the log,
                # the stage-marker match and the banner tail all want text.
                # (A coloured "== syrinx-stage:" would not even match.)
                line = _ANSI.sub("", raw.decode("utf-8", "replace")).rstrip()
                if not line:
                    return
                logf.write(line + "\n")
                logf.flush()  # the dialog tells users to tail this while it runs
                if line.startswith(STAGE_MARKER):
                    token = line[len(STAGE_MARKER):].strip()
                    stage(STAGE_LABELS.get(token, token))
                    return
                tail.append(line)

            async def pump() -> int:
                buf = b""
                while True:
                    chunk = await proc.stdout.read(4096)
                    if not chunk:
                        break
                    buf += chunk
                    parts = _LINE_SPLIT.split(buf)
                    buf = parts.pop()  # the (possibly partial) trailing line
                    for part in parts:
                        handle(part)
                if buf:
                    handle(buf)
                return await proc.wait()

            try:
                rc = await asyncio.wait_for(pump(), _timeout_secs())
            except asyncio.TimeoutError:
                proc.kill()
                await proc.wait()
                return fail(
                    f"the setup ran longer than {_timeout_secs():g} s and was "
                    "stopped (raise SYRINX_VC_SETUP_TIMEOUT if this PC is just slow)"
                )

        if s.id in self._cancelled:
            self._cancelled.discard(s.id)
            on_progress(s.id, "cancelled", "cancelled", "")
            return False
        if rc == 0:
            on_progress(s.id, "done", "done", "")
            return True
        return fail(_reason(tail, rc))


def _reason(tail: "deque[str]", rc: int) -> str:
    """One line of *why*, from the child's last meaningful output.

    Three passes, best first, because the useful sentence is almost never the
    last thing a failing install prints: a self-announced ``error:``/``fatal``
    line that is not shell decoration, else the last real line, else the exit
    code. The 2026-07-28 field failure is the case that forced this — its last
    line was PowerShell's ``| Program "python.exe" ended with non-zero exit
    code: 1``, three lines below the ``error: failed-wheel-build-for-install``
    that actually names the problem.
    """
    lines = [ln for ln in tail if ln.strip() and not _DECORATION.match(ln)]
    reason = next((ln.strip() for ln in reversed(lines) if _ERROR_LINE.match(ln)), "")
    if not reason and lines:
        reason = lines[-1].strip()
    if not reason:
        reason = f"the setup script exited with code {rc}"
    # Truncate here, not at the end: the log path and the MAX_PATH advice are
    # appended after this and are the parts the user acts on, so they must
    # never be what falls off the end.
    if len(reason) > _MAX_REASON:
        reason = reason[:_MAX_REASON - 1].rstrip() + "…"
    joined = " ".join(tail).lower()
    if any(h in joined for h in _LONG_PATH_HINTS):
        # ~18 characters of MAX_PATH headroom at the default location; a long
        # profile name eats it. The user cannot guess this fix, so spell it out.
        reason += " (the install path may be too long — set SYRINX_VC_VENV_DIR "
        reason += "to a short path such as C:\\syrinx-vc and try again)"
    return reason

"""One-click installer for the isolated voice-conversion engines.

Seed-VC (GPL-3.0) and Vevo's Amphion checkpoints (CC-BY-NC) are deliberately
never bundled with Syrinx — they are installed per-user, on request, into their
own virtualenvs by ``engine/setup-{seedvc,vevo}.{sh,ps1}``. Historically that
meant telling the user to "run engine/setup-vevo.sh first", which is a dead end
for anyone who never opens a terminal (and names a ``.sh`` path on Windows).
This module lets the app run those exact same scripts itself and stream their
progress, so the opt-in stays an explicit user decision but costs one click.

It is deliberately **stdlib + :mod:`.paths` only**: it has to be importable on a
box where nothing ML is installed yet — that is the entire point of the feature —
and it must never drag torch into the Models tab's status query.

Three environment variables steer it (all documented in ``docs/RPC-PROTOCOL.md``):

* ``SYRINX_VC_VENV_DIR`` — where the setup script puts its ``.venv-<x>``. Unset
  means "beside the script", which is exactly today's Linux behavior; the engine
  only sets it on Windows, where nesting a venv under an installed tree's
  ``site-packages`` blows past MAX_PATH.
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

# Module-level switch rather than an inline ``sys.platform`` test so the per-OS
# branches (venv layout, which script, whether to bootstrap Python/Git) are all
# reachable from a test suite that only ever runs on one host OS.
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
    id: str          # the wire vocabulary: "seedvc" | "vevo"
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
}

SETUP_IDS = tuple(SETUPS)

# Derived, not written out twice: {"seed_vc": "seedvc", "vevo_timbre": "vevo"}.
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

_NO_PYTHON = ("install Python 3.12 from python.org, then click Install again.")


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
    """The directory a setup's venv is created *inside* on Windows.

    Under the data dir rather than beside the script: an installed Syrinx puts
    the engine at ``…\\engine\\.venv\\Lib\\site-packages``, and a second venv
    nested under that overflows MAX_PATH the moment pip unpacks torch.
    """
    return data_dir() / SETUPS[setup_id].subdir


def script_path(setup_id: str) -> "Path | None":
    """The setup script for *setup_id* on this OS, or None if it wasn't shipped.

    ``SYRINX_VC_SETUP_DIR`` wins outright. Otherwise walk up from the package
    dir: a checkout finds ``engine/`` one level up, while an installed Windows
    tree sits at ``engine\\.venv\\Lib\\site-packages\\syrinx_engine`` and finds
    it at four. Five levels is one more than any layout we ship, and returning
    None (rather than guessing) is what lets the caller say "this build shipped
    without the setup scripts" instead of failing with a confusing spawn error.
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

    POSIX has exactly one — beside the script, byte-identical to before this
    module existed. Windows has two because the venv root moved: the data-dir
    location is what the engine asks for now, and the legacy beside-the-script
    location keeps venvs built by hand before this landed working.
    """
    s = SETUPS[setup_id]
    if _IS_WIN:
        return [
            venv_root(setup_id) / f".venv-{s.venv}" / "Scripts" / "python.exe",
            engine_dir() / f".venv-{s.venv}" / "Scripts" / "python.exe",
        ]
    return [engine_dir() / f".venv-{s.venv}" / "bin" / "python"]


def venv_python(setup_id: str) -> "Path | None":
    """The venv's interpreter, or None when the setup hasn't been run.

    Checking the *interpreter* rather than the venv directory is deliberate: a
    torn install (killed mid-``pip``, or a cancel) leaves the directory behind
    and the old directory-existence check called that "installed".
    """
    if setup_id not in SETUPS:
        return None
    for cand in venv_candidates(setup_id):
        if cand.exists():
            return cand
    return None


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


# --- Windows prerequisites --------------------------------------------------


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
    """The full pre-winget probe order."""
    # `py -3.12` first: the launcher knows about every registered install,
    # including ones in directories we'd never guess.
    cands = [["py", "-3.12"]]
    cands += _fixed_python_candidates()
    for name in ("python3.12", "python3", "python"):
        found = shutil.which(name)
        if found:
            cands.append([found])
    return cands


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

    Windows only — POSIX boxes that can run Syrinx already have python3. Raises
    :class:`VcSetupError` with a user-facing sentence when it comes up empty.
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
    for argv in _python_candidates():
        found = _probe_python(argv)
        if found:
            return found
    winget = _winget()
    if not winget:
        raise VcSetupError(
            "no Python 3.12 was found on this PC and winget is not available "
            f"to install one — {_NO_PYTHON}"
        )
    if on_stage:
        on_stage("installing Python 3.12…")
    _winget_install(winget, "Python.Python.3.12")
    for argv in _fixed_python_candidates():
        found = _probe_python(argv)
        if found:
            return found
    raise VcSetupError(f"Python 3.12 could not be installed automatically — {_NO_PYTHON}")


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
    """
    if _git_works("git"):
        return ""
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


class VcSetupManager:
    """Owns the at-most-one-per-id, one-at-a-time execution of the setups."""

    def __init__(self) -> None:
        # Claims are taken SYNCHRONOUSLY, before any await, so a double-clicked
        # Install button cannot squeeze two tasks past the check.
        self._claimed: "set[str]" = set()
        self._procs: "dict[str, asyncio.subprocess.Process]" = {}
        self._cancelled: "set[str]" = set()
        # Both setups install torch. Running them concurrently would double the
        # download and thrash a small disk, so they queue.
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
            # Pass the resolved clone location explicitly. setup-vevo.sh derives
            # its own default from $HOME and ignores SYRINX_DATA_DIR, so without
            # this the script and the worker could disagree about where Amphion
            # is; passing it makes them agree by construction (and is a no-op
            # when the variable was already set).
            "SYRINX_VEVO_AMPHION": str(amphion_dir()),
        })

        if _IS_WIN:
            env[s.py_env] = resolve_python(s.id, stage)
            if s.needs_git:
                extra_path = ensure_git(stage)
                if extra_path:
                    env["PATH"] = extra_path + os.pathsep + env.get("PATH", "")
            # Windows only — on POSIX the venv stays beside the script, which is
            # where the backends look and is byte-identical to today.
            env["SYRINX_VC_VENV_DIR"] = str(venv_root(s.id))
            Path(env["SYRINX_VC_VENV_DIR"]).mkdir(parents=True, exist_ok=True)

        log_path.parent.mkdir(parents=True, exist_ok=True)
        tail: "deque[str]" = deque(maxlen=40)
        proc = await asyncio.create_subprocess_exec(
            *_command(script),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
            env=env,
            cwd=str(script.parent),
            **_NO_WINDOW,
        )
        self._procs[s.id] = proc

        with open(log_path, "a", encoding="utf-8", errors="replace") as logf:

            def handle(raw: bytes) -> None:
                line = raw.decode("utf-8", "replace").rstrip()
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
    """One line of *why*, from the child's last meaningful output."""
    last = next((line for line in reversed(tail) if line.strip()), "")
    reason = last or f"the setup script exited with code {rc}"
    joined = " ".join(tail).lower()
    if any(h in joined for h in _LONG_PATH_HINTS):
        # ~18 characters of MAX_PATH headroom at the default location; a long
        # profile name eats it. The user cannot guess this fix, so spell it out.
        reason += " (the install path may be too long — set SYRINX_VC_VENV_DIR "
        reason += "to a short path such as C:\\syrinx-vc and try again)"
    return reason

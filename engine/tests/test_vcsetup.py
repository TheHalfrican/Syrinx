"""vcsetup.py — script discovery, venv probing, and the streaming setup runner.

Nothing here runs a real setup script: a real one downloads several GB. The
streaming tests spawn ``sys.executable -c <fake>`` through the module's own
``_command`` seam, so the reader, the log tee, the stage parsing and the error
detail are all exercised against a real child process — just a fast one.

The per-OS branches are reached by flipping ``vcsetup._IS_WIN``, so both venv
layouts and both script flavors are covered on whichever host OS is running.
"""

import asyncio
import sys

import pytest

from syrinx_engine import vcsetup


@pytest.fixture(autouse=True)
def setup_log(monkeypatch, tmp_path):
    """Keep the setup logs inside tmp_path.

    ``worker_log_path`` resolves to the real per-user cache dir on Windows/macOS,
    which conftest's ``isolated_env`` cannot redirect (it only sets the XDG
    variables, which platformdirs ignores there). Without this the tests would
    append to — and read back — the developer's actual logs.
    """
    logs = tmp_path / "logs"
    logs.mkdir()
    monkeypatch.setattr(vcsetup, "worker_log_path",
                        lambda name: logs / f"syrinx-{name}.log")
    return logs


def log_for(setup_log, setup_id):
    return setup_log / f"syrinx-setup-{setup_id}.log"


@pytest.fixture
def posix(monkeypatch):
    """Pretend to be POSIX: no Python/Git bootstrap, one venv candidate."""
    monkeypatch.setattr(vcsetup, "_IS_WIN", False)


@pytest.fixture
def windows(monkeypatch):
    monkeypatch.setattr(vcsetup, "_IS_WIN", True)


# --- the table -----------------------------------------------------------


def test_setup_ids_and_engine_mapping_are_the_pinned_vocabulary():
    assert set(vcsetup.SETUP_IDS) == {"seedvc", "vevo"}
    # both vevo catalog rows share engine "vevo_timbre", so one install clears
    # vevo-timbre AND vevo2-singing
    assert vcsetup.ENGINE_TO_SETUP == {"seed_vc": "seedvc", "vevo_timbre": "vevo"}


def test_only_vevo_needs_git():
    assert vcsetup.SETUPS["vevo"].needs_git is True
    assert vcsetup.SETUPS["seedvc"].needs_git is False


def test_every_script_stage_token_has_a_human_label():
    """An unmapped token would show the user a bare word like "pins"."""
    for token in ("venv", "torch", "seedvc", "demucs", "pins", "verify",
                  "amphion", "deps"):
        assert vcsetup.STAGE_LABELS[token]


# --- script_path ---------------------------------------------------------


def test_script_path_finds_the_checkout_layout(monkeypatch, tmp_path, posix):
    """A dev checkout has engine/syrinx_engine/ — one level up from the pkg."""
    engine = tmp_path / "engine"
    (engine / "syrinx_engine").mkdir(parents=True)
    (engine / "setup-seedvc.sh").write_text("x")
    monkeypatch.setattr(vcsetup, "_PKG_DIR", engine / "syrinx_engine")
    assert vcsetup.script_path("seedvc") == engine / "setup-seedvc.sh"


def test_script_path_finds_the_installed_windows_layout(monkeypatch, tmp_path, windows):
    """An installed tree buries the package four levels down, under the
    engine's own venv — the ancestor walk has to reach that far."""
    engine = tmp_path / "engine"
    pkg = engine / ".venv" / "Lib" / "site-packages" / "syrinx_engine"
    pkg.mkdir(parents=True)
    (engine / "setup-vevo.ps1").write_text("x")
    monkeypatch.setattr(vcsetup, "_PKG_DIR", pkg)
    assert vcsetup.script_path("vevo") == engine / "setup-vevo.ps1"


def test_setup_dir_override_wins_and_is_not_a_fallback(monkeypatch, tmp_path, posix):
    """The escape hatch is authoritative: pointing it somewhere empty must NOT
    quietly fall back to the ancestor walk, or a typo would be undebuggable."""
    engine = tmp_path / "engine"
    (engine / "syrinx_engine").mkdir(parents=True)
    (engine / "setup-seedvc.sh").write_text("x")
    monkeypatch.setattr(vcsetup, "_PKG_DIR", engine / "syrinx_engine")

    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    monkeypatch.setenv("SYRINX_VC_SETUP_DIR", str(elsewhere))
    assert vcsetup.script_path("seedvc") is None

    (elsewhere / "setup-seedvc.sh").write_text("x")
    assert vcsetup.script_path("seedvc") == elsewhere / "setup-seedvc.sh"


def test_script_path_is_none_when_the_build_shipped_without_the_scripts(
        monkeypatch, tmp_path, posix):
    pkg = tmp_path / "somewhere" / "syrinx_engine"
    pkg.mkdir(parents=True)
    monkeypatch.setattr(vcsetup, "_PKG_DIR", pkg)
    assert vcsetup.script_path("seedvc") is None


def test_script_path_of_an_unknown_id_is_none():
    assert vcsetup.script_path("nope") is None


def test_script_path_picks_the_flavor_for_the_os(monkeypatch, tmp_path, windows):
    engine = tmp_path / "engine"
    (engine / "syrinx_engine").mkdir(parents=True)
    (engine / "setup-seedvc.sh").write_text("x")
    (engine / "setup-seedvc.ps1").write_text("x")
    monkeypatch.setattr(vcsetup, "_PKG_DIR", engine / "syrinx_engine")
    assert vcsetup.script_path("seedvc").suffix == ".ps1"
    monkeypatch.setattr(vcsetup, "_IS_WIN", False)
    assert vcsetup.script_path("seedvc").suffix == ".sh"


# --- venv probing --------------------------------------------------------


def test_posix_has_exactly_the_historical_candidate(monkeypatch, tmp_path, posix):
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    assert vcsetup.venv_candidates("seedvc") == [
        tmp_path / ".venv-seedvc" / "bin" / "python"]


def test_windows_prefers_the_short_data_dir_path(monkeypatch, tmp_path, windows,
                                                 isolated_env):
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    cands = vcsetup.venv_candidates("vevo")
    # data dir first (short — MAX_PATH headroom), legacy beside-the-script
    # second so hand-built venvs from before this landed keep working
    assert cands == [
        isolated_env / "vevo" / ".venv-vevo" / "Scripts" / "python.exe",
        tmp_path / ".venv-vevo" / "Scripts" / "python.exe",
    ]


def test_venv_python_wants_the_interpreter_not_the_directory(monkeypatch, tmp_path, posix):
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    (tmp_path / ".venv-seedvc").mkdir()
    # a torn install leaves the directory behind — that is not "installed"
    assert vcsetup.venv_python("seedvc") is None
    exe = tmp_path / ".venv-seedvc" / "bin" / "python"
    exe.parent.mkdir(parents=True)
    exe.write_text("")
    assert vcsetup.venv_python("seedvc") == exe


def test_windows_falls_back_to_the_legacy_location(monkeypatch, tmp_path, windows):
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    exe = tmp_path / ".venv-seedvc" / "Scripts" / "python.exe"
    exe.parent.mkdir(parents=True)
    exe.write_text("")
    assert vcsetup.venv_python("seedvc") == exe


def test_venv_python_of_an_unknown_id_is_none():
    assert vcsetup.venv_python("nope") is None
    assert vcsetup.installed("nope") is False


def test_installed_vevo_needs_both_the_venv_and_amphion(monkeypatch, tmp_path, posix):
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    exe = tmp_path / ".venv-vevo" / "bin" / "python"
    exe.parent.mkdir(parents=True)
    exe.write_text("")
    amphion = tmp_path / "Amphion"
    monkeypatch.setenv("SYRINX_VEVO_AMPHION", str(amphion))
    assert vcsetup.installed("vevo") is False  # venv only
    amphion.mkdir()
    assert vcsetup.installed("vevo") is True
    assert vcsetup.amphion_dir() == amphion


def test_amphion_defaults_under_the_data_dir(monkeypatch, isolated_env):
    monkeypatch.delenv("SYRINX_VEVO_AMPHION", raising=False)
    assert vcsetup.amphion_dir() == isolated_env / "vevo" / "Amphion"


def test_venv_root_is_the_per_setup_data_subdir(isolated_env):
    assert vcsetup.venv_root("seedvc") == isolated_env / "seedvc"
    assert vcsetup.venv_root("vevo") == isolated_env / "vevo"


# --- resolve_python / ensure_git -----------------------------------------


def test_a_broken_explicit_override_is_a_hard_error(monkeypatch, windows):
    """Someone set SYRINX_SEEDVC_PYTHON on purpose; silently ignoring it and
    using some other interpreter would be the worst possible answer."""
    monkeypatch.setenv("SYRINX_SEEDVC_PYTHON", r"C:\nope\python.exe")
    with pytest.raises(vcsetup.VcSetupError) as ei:
        vcsetup.resolve_python("seedvc")
    assert "SYRINX_SEEDVC_PYTHON" in str(ei.value)
    assert r"C:\nope\python.exe" in str(ei.value)


def test_a_working_explicit_override_is_used(monkeypatch, windows):
    monkeypatch.setenv("SYRINX_SEEDVC_PYTHON", sys.executable)
    monkeypatch.setattr(vcsetup, "_probe_python",
                        lambda argv: sys.executable if argv == [sys.executable] else "")
    assert vcsetup.resolve_python("seedvc") == sys.executable


def test_the_probe_rejects_a_stub_and_an_embedded_build(monkeypatch, windows):
    """The Store stub exits non-zero; an embeddable build has no ensurepip, so
    it exits non-zero too. Both must read as "not a candidate", not as a crash."""
    assert vcsetup._probe_python([sys.executable, "-c", "raise SystemExit(9)"]) == ""
    assert vcsetup._probe_python(["definitely-not-an-executable-here"]) == ""


def test_the_probe_accepts_a_real_312_and_reports_its_path(monkeypatch, windows):
    found = vcsetup._probe_python([sys.executable])
    if sys.version_info[:2] == (3, 12):
        assert found == sys.executable
    else:
        assert found == ""  # the version gate is part of the probe


def test_no_python_and_no_winget_says_what_to_do(monkeypatch, windows):
    monkeypatch.delenv("SYRINX_SEEDVC_PYTHON", raising=False)
    monkeypatch.setattr(vcsetup, "_probe_python", lambda argv: "")
    monkeypatch.setattr(vcsetup, "_winget", lambda: "")
    with pytest.raises(vcsetup.VcSetupError) as ei:
        vcsetup.resolve_python("seedvc")
    msg = str(ei.value)
    assert "winget is not available" in msg
    assert "install Python 3.12 from python.org, then click Install again." in msg


def test_winget_bootstraps_python_then_reprobes_the_fixed_paths(monkeypatch, windows):
    """The engine's PATH was captured before winget ran, so only the known
    install locations are re-probed — not PATH, and not `py`."""
    monkeypatch.delenv("SYRINX_SEEDVC_PYTHON", raising=False)
    installed = []
    stages = []
    monkeypatch.setattr(vcsetup, "_winget", lambda: "winget.exe")
    monkeypatch.setattr(vcsetup, "_winget_install",
                        lambda w, pkg: installed.append((w, pkg)))
    calls = []

    def probe(argv):
        calls.append(argv)
        # nothing works until winget has run
        return r"C:\py312\python.exe" if installed else ""

    monkeypatch.setattr(vcsetup, "_probe_python", probe)
    assert vcsetup.resolve_python("seedvc", stages.append) == r"C:\py312\python.exe"
    assert installed == [("winget.exe", "Python.Python.3.12")]
    assert stages == ["installing Python 3.12…"]


def test_winget_that_installs_nothing_still_ends_with_advice(monkeypatch, windows):
    monkeypatch.delenv("SYRINX_VEVO_PYTHON", raising=False)
    monkeypatch.setattr(vcsetup, "_probe_python", lambda argv: "")
    monkeypatch.setattr(vcsetup, "_winget", lambda: "winget.exe")
    monkeypatch.setattr(vcsetup, "_winget_install", lambda w, pkg: None)
    with pytest.raises(vcsetup.VcSetupError) as ei:
        vcsetup.resolve_python("vevo")
    assert "install Python 3.12 from python.org, then click Install again." in str(ei.value)


def test_git_already_present_adds_nothing_to_path(monkeypatch):
    monkeypatch.setattr(vcsetup, "_git_works", lambda exe: exe == "git")
    assert vcsetup.ensure_git() == ""


def test_missing_git_without_winget_names_git_scm(monkeypatch):
    monkeypatch.setattr(vcsetup, "_git_works", lambda exe: False)
    monkeypatch.setattr(vcsetup, "_winget", lambda: "")
    with pytest.raises(vcsetup.VcSetupError) as ei:
        vcsetup.ensure_git()
    assert "git-scm.com" in str(ei.value)


def test_winget_installs_git_and_returns_the_dir_to_prepend(monkeypatch, tmp_path):
    """A fresh Git install updates the MACHINE PATH, which our already-captured
    environment block will never see — hence the explicit prepend."""
    monkeypatch.setenv("ProgramFiles", str(tmp_path))
    monkeypatch.delenv("ProgramFiles(x86)", raising=False)
    git_exe = tmp_path / "Git" / "cmd" / "git.exe"
    installed = []
    stages = []
    monkeypatch.setattr(vcsetup, "_winget", lambda: "winget.exe")
    monkeypatch.setattr(vcsetup, "_winget_install",
                        lambda w, pkg: installed.append(pkg))
    monkeypatch.setattr(vcsetup, "_git_works",
                        lambda exe: bool(installed) and exe == str(git_exe))
    assert vcsetup.ensure_git(stages.append) == str(git_exe.parent)
    assert installed == ["Git.Git"]
    assert stages == ["installing Git…"]


# --- the streaming runner ------------------------------------------------


def fake_script(tmp_path, body, name="fake-setup.py"):
    """Write a python "setup script" and point the runner's argv seam at it."""
    path = tmp_path / name
    path.write_text(body, encoding="utf-8")
    return path


def run_install(monkeypatch, tmp_path, body, setup_id="seedvc"):
    """Drive a whole install against a fake child; returns (ok, events)."""
    script = fake_script(tmp_path, body)
    monkeypatch.setattr(vcsetup, "_IS_WIN", False)  # skip the Windows bootstrap
    monkeypatch.setattr(vcsetup, "script_path", lambda sid: script)
    monkeypatch.setattr(vcsetup, "_command", lambda s: [sys.executable, str(s)])
    events = []
    mgr = vcsetup.VcSetupManager()
    assert mgr.claim(setup_id) is True
    ok = asyncio.run(mgr.install(setup_id, lambda *a: events.append(a)))
    return ok, events


STAGES_SCRIPT = r"""
import sys
for token in ("venv", "torch", "seedvc", "demucs", "pins", "verify"):
    sys.stdout.write("== syrinx-stage: %s\n" % token)
    sys.stdout.write("some pip chatter for %s\n" % token)
sys.stdout.flush()
"""


def test_stage_markers_become_human_labels(monkeypatch, tmp_path):
    ok, events = run_install(monkeypatch, tmp_path, STAGES_SCRIPT)
    assert ok is True
    stages = [stage for (_id, stage, status, _d) in events if status == "running"]
    assert stages == [vcsetup.STAGE_LABELS[t] for t in
                      ("venv", "torch", "seedvc", "demucs", "pins", "verify")]
    assert events[-1] == ("seedvc", "done", "done", "")


def test_every_line_is_teed_to_the_setup_log(monkeypatch, tmp_path, setup_log):
    run_install(monkeypatch, tmp_path, STAGES_SCRIPT)
    text = log_for(setup_log, "seedvc").read_text(encoding="utf-8")
    assert "some pip chatter for torch" in text
    # the markers themselves are logged too — the log is the whole transcript
    assert "== syrinx-stage: verify" in text


CR_SCRIPT = r"""
import sys
sys.stdout.write("downloading torch 10%\rdownloading torch 60%\r")
sys.stdout.write("== syrinx-stage: torch\r")
sys.stdout.write("last carriage-returned line")
sys.stdout.flush()
"""


def test_carriage_returned_progress_lines_are_read(monkeypatch, tmp_path, setup_log):
    """pip rewrites one line with CR and may never emit a newline; readline()
    would sit there until the process exited and the stage would never show."""
    ok, events = run_install(monkeypatch, tmp_path, CR_SCRIPT)
    assert ok is True
    stages = [stage for (_id, stage, status, _d) in events if status == "running"]
    assert stages == [vcsetup.STAGE_LABELS["torch"]]
    text = log_for(setup_log, "seedvc").read_text(encoding="utf-8")
    assert "downloading torch 60%" in text
    assert "last carriage-returned line" in text  # no trailing separator either


HUGE_SCRIPT = r"""
import sys
sys.stdout.write("x" * 70000 + "\n")
sys.stdout.write("== syrinx-stage: verify\n")
sys.stdout.flush()
"""


def test_a_line_past_the_stream_reader_limit_does_not_break_the_run(
        monkeypatch, tmp_path, setup_log):
    """A pip resolver backtrack prints single lines far past StreamReader's
    64 KiB limit, where readline() raises instead of returning."""
    ok, events = run_install(monkeypatch, tmp_path, HUGE_SCRIPT)
    assert ok is True
    stages = [stage for (_id, stage, status, _d) in events if status == "running"]
    assert stages == [vcsetup.STAGE_LABELS["verify"]]
    assert "x" * 70000 in log_for(setup_log, "seedvc").read_text(encoding="utf-8")


FAILING_SCRIPT = r"""
import sys
sys.stdout.write("== syrinx-stage: torch\n")
sys.stdout.write("ERROR: Could not find a version that satisfies torch\n")
sys.stdout.flush()
raise SystemExit(1)
"""


def test_a_failing_setup_reports_the_last_line_and_the_log_path(
        monkeypatch, tmp_path, setup_log):
    ok, events = run_install(monkeypatch, tmp_path, FAILING_SCRIPT)
    assert ok is False
    setup_id, stage, status, detail = events[-1]
    assert (setup_id, stage, status) == ("seedvc", "failed", "error")
    assert detail.startswith("ERROR: Could not find a version that satisfies torch")
    assert detail.endswith(f" · log: {log_for(setup_log, 'seedvc')}")


SILENT_FAILURE = "import sys\nraise SystemExit(3)\n"


def test_a_silent_failure_still_says_something(monkeypatch, tmp_path):
    ok, events = run_install(monkeypatch, tmp_path, SILENT_FAILURE)
    assert ok is False
    assert "exited with code 3" in events[-1][3]


LONG_PATH_FAILURE = r"""
import sys
sys.stdout.write("ERROR: [Errno 206] The filename or extension is too long\n")
sys.stdout.flush()
raise SystemExit(1)
"""


def test_a_max_path_failure_suggests_the_venv_dir_override(monkeypatch, tmp_path):
    """The MAX_PATH fix is the one setup failure a user cannot possibly guess."""
    ok, events = run_install(monkeypatch, tmp_path, LONG_PATH_FAILURE)
    assert ok is False
    assert "SYRINX_VC_VENV_DIR" in events[-1][3]


def test_a_missing_script_is_a_clean_error(monkeypatch, tmp_path):
    monkeypatch.setattr(vcsetup, "_IS_WIN", False)
    monkeypatch.setattr(vcsetup, "script_path", lambda sid: None)
    events = []
    mgr = vcsetup.VcSetupManager()
    mgr.claim("vevo")
    assert asyncio.run(mgr.install("vevo", lambda *a: events.append(a))) is False
    assert "shipped without the setup scripts" in events[-1][3]


def test_an_unknown_id_installs_nothing():
    mgr = vcsetup.VcSetupManager()
    events = []
    assert asyncio.run(mgr.install("nope", lambda *a: events.append(a))) is False
    assert events == []


def test_the_timeout_kills_a_wedged_setup(monkeypatch, tmp_path):
    monkeypatch.setenv("SYRINX_VC_SETUP_TIMEOUT", "0.4")
    ok, events = run_install(monkeypatch, tmp_path,
                             "import time\ntime.sleep(30)\n")
    assert ok is False
    assert events[-1][2] == "error"
    assert "SYRINX_VC_SETUP_TIMEOUT" in events[-1][3]


def test_a_bad_timeout_value_falls_back_to_the_default(monkeypatch):
    monkeypatch.setenv("SYRINX_VC_SETUP_TIMEOUT", "soon")
    assert vcsetup._timeout_secs() == 5400
    monkeypatch.setenv("SYRINX_VC_SETUP_TIMEOUT", "60")
    assert vcsetup._timeout_secs() == 60


def test_the_child_environment_carries_the_quiet_flags(monkeypatch, tmp_path, setup_log):
    """pip progress bars are noise in a log file and an interactive credential
    prompt on a child nobody can see hangs until the timeout."""
    body = (
        "import json, os, sys\n"
        "sys.stdout.write(json.dumps({k: os.environ.get(k) for k in ("
        "'PYTHONUNBUFFERED','PIP_PROGRESS_BAR','PIP_NO_INPUT',"
        "'PIP_DISABLE_PIP_VERSION_CHECK','UV_NO_PROGRESS','GIT_TERMINAL_PROMPT',"
        "'SYRINX_VEVO_AMPHION','SYRINX_VC_VENV_DIR')}) + '\\n')\n"
    )
    ok, _events = run_install(monkeypatch, tmp_path, body, setup_id="vevo")
    assert ok is True
    import json

    seen = json.loads(log_for(setup_log, "vevo").read_text(encoding="utf-8").strip())
    assert seen["PYTHONUNBUFFERED"] == "1"
    assert seen["PIP_PROGRESS_BAR"] == "off"
    assert seen["PIP_NO_INPUT"] == "1"
    assert seen["PIP_DISABLE_PIP_VERSION_CHECK"] == "1"
    assert seen["UV_NO_PROGRESS"] == "1"
    assert seen["GIT_TERMINAL_PROMPT"] == "0"
    # the clone location is passed explicitly so the script and vevo_worker.py
    # cannot disagree (setup-vevo.sh derives its default from $HOME)
    assert seen["SYRINX_VEVO_AMPHION"] == str(vcsetup.amphion_dir())
    # POSIX must NOT relocate the venv — that is the Linux byte-identity promise
    assert seen["SYRINX_VC_VENV_DIR"] is None


# --- claim / cancel ------------------------------------------------------


def test_claim_is_a_synchronous_check_and_set():
    """Taken before create_task, so a double-clicked Install cannot start two."""
    mgr = vcsetup.VcSetupManager()
    assert mgr.claim("seedvc") is True
    assert mgr.claim("seedvc") is False
    assert mgr.running("seedvc") is True
    # a different setup is unaffected; an unknown id is never claimable
    assert mgr.claim("vevo") is True
    assert mgr.claim("nope") is False


def test_a_finished_install_releases_its_claim(monkeypatch, tmp_path):
    script = fake_script(tmp_path, "pass\n")
    monkeypatch.setattr(vcsetup, "_IS_WIN", False)
    monkeypatch.setattr(vcsetup, "script_path", lambda sid: script)
    monkeypatch.setattr(vcsetup, "_command", lambda s: [sys.executable, str(s)])
    mgr = vcsetup.VcSetupManager()
    mgr.claim("seedvc")
    asyncio.run(mgr.install("seedvc", lambda *a: None))
    assert mgr.running("seedvc") is False
    assert mgr.claim("seedvc") is True  # re-runnable; the scripts are idempotent


def test_cancel_without_a_running_child_is_false():
    mgr = vcsetup.VcSetupManager()
    assert mgr.cancel("seedvc") is False
    mgr.claim("seedvc")
    assert mgr.cancel("seedvc") is False  # claimed but not yet spawned


def test_cancel_kills_the_child_and_reports_cancelled(monkeypatch, tmp_path):
    """A cancel is a user decision, not a failure: the app quietly returns the
    row to its Install state, so this must NOT come back as an error."""
    script = fake_script(tmp_path, "import time\ntime.sleep(30)\n")
    monkeypatch.setattr(vcsetup, "_IS_WIN", False)
    monkeypatch.setattr(vcsetup, "script_path", lambda sid: script)
    monkeypatch.setattr(vcsetup, "_command", lambda s: [sys.executable, str(s)])
    events = []
    mgr = vcsetup.VcSetupManager()
    mgr.claim("seedvc")

    async def go():
        task = asyncio.create_task(mgr.install("seedvc", lambda *a: events.append(a)))
        while "seedvc" not in mgr._procs:  # wait for the spawn
            await asyncio.sleep(0.01)
        assert mgr.cancel("seedvc") is True
        return await task

    assert asyncio.run(go()) is False
    assert events[-1] == ("seedvc", "cancelled", "cancelled", "")
    assert mgr.running("seedvc") is False


def test_a_second_setup_waits_for_the_first(monkeypatch, tmp_path):
    """Both setups install torch; running them at once would double the
    download and thrash a small disk."""
    script = fake_script(tmp_path, "import time\ntime.sleep(0.3)\n")
    monkeypatch.setattr(vcsetup, "_IS_WIN", False)
    monkeypatch.setattr(vcsetup, "script_path", lambda sid: script)
    monkeypatch.setattr(vcsetup, "_command", lambda s: [sys.executable, str(s)])
    events = []
    mgr = vcsetup.VcSetupManager()
    mgr.claim("seedvc")
    mgr.claim("vevo")

    async def go():
        first = asyncio.create_task(mgr.install("seedvc", lambda *a: events.append(a)))
        await asyncio.sleep(0.05)  # let the first take the lock
        second = asyncio.create_task(mgr.install("vevo", lambda *a: events.append(a)))
        return await asyncio.gather(first, second)

    assert asyncio.run(go()) == [True, True]
    waiting = [e for e in events if e[1] == "waiting for the other install…"]
    assert waiting == [("vevo", "waiting for the other install…", "running", "")]

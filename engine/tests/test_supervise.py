"""Supervised-mode lifecycle (spec §13.1): under ``SYRINX_SUPERVISED=1`` a
daemon thread watches stdin; when the parent's pipe closes (EOF/error) the
engine removes the discovery file and exits immediately (``os._exit(0)``).

The in-process tests exercise the real watchdog loop and cleanup via test seams
(a real ``os.pipe`` for stdin, a recording stand-in for ``os._exit``). One
subprocess test drives the genuine boot path end to end. All paths use a unique
``SYRINX_RPC_ENDPOINT`` so nothing touches the machine's default discovery file.
"""

import io
import json
import os
import subprocess
import sys
import threading
import time

from syrinx_engine import __main__ as m


# --- env gate ------------------------------------------------------------


def test_supervised_reads_env(monkeypatch):
    monkeypatch.delenv("SYRINX_SUPERVISED", raising=False)
    assert m._supervised() is False
    monkeypatch.setenv("SYRINX_SUPERVISED", "1")
    assert m._supervised() is True
    monkeypatch.setenv("SYRINX_SUPERVISED", "0")  # only "1" arms it
    assert m._supervised() is False


# --- arming guards (unusable stdin => cannot watch, keep running) ---------


def test_watchdog_not_armed_when_stdin_is_none(caplog):
    with caplog.at_level("WARNING"):
        armed = m._start_stdin_watchdog(lambda: None, stdin=None)
    assert armed is False
    assert "sys.stdin is None" in caplog.text


def test_watchdog_not_armed_when_stdin_has_no_fileno(caplog):
    with caplog.at_level("WARNING"):
        armed = m._start_stdin_watchdog(lambda: None, stdin=io.StringIO())
    assert armed is False
    assert "no file descriptor" in caplog.text


# --- real watchdog loop (os.pipe stdin, recorded exit) -------------------


def _pipe_stdin():
    """Return (read-text-stream, write-fd). Closing write-fd => EOF on read."""
    r, w = os.pipe()
    return os.fdopen(r, "r"), w


def test_watchdog_cleans_up_and_exits_on_eof():
    rf, w = _pipe_stdin()
    cleaned, exited = [], []
    done = threading.Event()

    def fake_exit(code):
        exited.append(code)
        done.set()

    armed = m._start_stdin_watchdog(
        lambda: cleaned.append(True), stdin=rf, _exit=fake_exit
    )
    assert armed is True
    os.close(w)  # parent closes the pipe -> EOF
    assert done.wait(5), "watchdog did not fire on EOF"
    assert cleaned == [True]
    assert exited == [0]
    rf.close()


def test_watchdog_exits_even_if_cleanup_raises():
    rf, w = _pipe_stdin()
    exited = []
    done = threading.Event()

    def fake_exit(code):
        exited.append(code)
        done.set()

    def boom():
        raise RuntimeError("cleanup blew up")

    m._start_stdin_watchdog(boom, stdin=rf, _exit=fake_exit)
    os.close(w)
    assert done.wait(5), "watchdog did not exit after cleanup raised"
    assert exited == [0]
    rf.close()


def test_watchdog_stays_put_while_pipe_is_open():
    """No false positives: while the parent keeps the pipe open (and, per the
    contract, never writes to it) the watchdog must not fire. It fires only once
    the pipe actually closes."""
    rf, w = _pipe_stdin()
    exited = []
    done = threading.Event()

    def fake_exit(code):
        exited.append(code)
        done.set()

    m._start_stdin_watchdog(lambda: None, stdin=rf, _exit=fake_exit)
    assert not done.wait(0.6)  # still alive while the pipe is open
    os.close(w)  # parent leaves
    assert done.wait(5)
    assert exited == [0]
    rf.close()


# --- end-to-end: real boot, real discovery cleanup -----------------------


def test_supervised_engine_exits_and_cleans_up_when_stdin_closes(tmp_path):
    endpoint = tmp_path / "rpc.json"
    env = dict(os.environ)
    env["SYRINX_SUPERVISED"] = "1"
    env["SYRINX_TRANSPORT"] = "rpc"
    env["SYRINX_RPC_ENDPOINT"] = str(endpoint)  # unique — never the default path

    proc = subprocess.Popen(
        [sys.executable, "-m", "syrinx_engine"],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    try:
        # Wait for the engine to bind and publish the discovery file.
        deadline = time.monotonic() + 30
        while not endpoint.exists():
            assert proc.poll() is None, "engine exited before writing discovery file"
            assert time.monotonic() < deadline, "discovery file never appeared"
            time.sleep(0.05)

        # Sanity-check the file the real engine wrote (pid is not asserted —
        # a venv python.exe can be a redirector, so proc.pid need not match the
        # engine's os.getpid()).
        disc = json.loads(endpoint.read_text())
        assert disc["protocol"] == 1
        assert isinstance(disc["port"], int)

        # Parent "goes away": close the child's stdin pipe.
        proc.stdin.close()

        # The watchdog should remove the file and os._exit(0) promptly.
        proc.wait(timeout=10)
        assert proc.returncode == 0
        assert not endpoint.exists(), "discovery file was not cleaned up on exit"
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=5)
        if proc.stdin and not proc.stdin.closed:
            proc.stdin.close()

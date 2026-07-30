"""Engine shutdown, all three doors (spec §13.1, §2.1).

* **stdin close** — under ``SYRINX_SUPERVISED=1`` a daemon thread watches
  stdin; when the parent's pipe closes (EOF/error) the engine removes the
  discovery file and exits immediately (``os._exit(0)``).
* **SIGTERM** — cancels the main task, so the transports' ``finally`` blocks
  run and the discovery file goes with them (``systemctl stop``, ``kill``).
* **SIGINT** — left to asyncio's own handler, which already does exactly that.

The in-process tests exercise the real watchdog loop and cleanup via test seams
(a real ``os.pipe`` for stdin, a recording stand-in for ``os._exit``; a fake
loop/task for the signal handler). Subprocess tests drive the genuine boot path
end to end, one per door. All paths use a unique ``SYRINX_RPC_ENDPOINT`` so
nothing touches the machine's default discovery file.
"""

import io
import json
import os
import signal
import subprocess
import sys
import threading
import time

import pytest

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


# --- SIGTERM handler (fake loop/task seams) ------------------------------


class _FakeLoop:
    """Records add_signal_handler calls; optionally refuses them the way a
    Proactor loop (Windows) or an off-main-thread loop does."""

    def __init__(self, raises=None):
        self.calls = []
        self._raises = raises

    def add_signal_handler(self, sig, callback, *args):
        self.calls.append((sig, callback, args))
        if self._raises is not None:
            raise self._raises


class _FakeTask:
    def __init__(self):
        self.cancels = 0

    def cancel(self):
        self.cancels += 1


def test_sigterm_cancels_the_main_task():
    loop, task = _FakeLoop(), _FakeTask()
    assert m._install_sigterm_handler(loop=loop, task=task) is True
    (sig, callback, args), = loop.calls
    assert sig == signal.SIGTERM
    assert args == ()
    assert task.cancels == 0  # armed, not fired
    callback()  # the signal arrives
    assert task.cancels == 1  # -> same graceful unwind Ctrl-C gets


def test_sigint_is_left_to_asyncios_own_handler():
    """Taking SIGINT over would swap KeyboardInterrupt (and the second-Ctrl-C
    escape hatch) for a bare cancel — asyncio.run already cancels the main task
    on SIGINT, so there is nothing to gain and behavior to lose."""
    loop = _FakeLoop()
    before = signal.getsignal(signal.SIGINT)
    m._install_sigterm_handler(loop=loop, task=_FakeTask())
    assert [sig for sig, _cb, _a in loop.calls] == [signal.SIGTERM]
    assert signal.getsignal(signal.SIGINT) is before


def test_sigterm_handler_is_inert_on_windows(monkeypatch):
    """Provably inert, not merely tolerated: win32 never even reaches
    add_signal_handler (it raises NotImplementedError on Proactor loops). The
    stdin watchdog is Windows' shutdown seam."""
    monkeypatch.setattr(m.sys, "platform", "win32")
    loop, task = _FakeLoop(), _FakeTask()
    assert m._install_sigterm_handler(loop=loop, task=task) is False
    assert loop.calls == []
    assert task.cancels == 0


def test_sigterm_handler_survives_a_loop_that_refuses_it(caplog):
    # e.g. an event loop running off the main thread: warn, keep booting.
    loop = _FakeLoop(raises=NotImplementedError("no signal handlers here"))
    with caplog.at_level("WARNING"):
        assert m._install_sigterm_handler(loop=loop, task=_FakeTask()) is False
    assert "SIGTERM" in caplog.text


def test_sigterm_handler_survives_a_loop_that_raises_valueerror(caplog):
    loop = _FakeLoop(raises=ValueError("signal only works in main thread"))
    with caplog.at_level("WARNING"):
        assert m._install_sigterm_handler(loop=loop, task=_FakeTask()) is False
    assert "SIGTERM" in caplog.text


# --- end-to-end: real boot, real discovery cleanup -----------------------


def _boot_engine(endpoint):
    """Spawn the real engine, supervised, on a private endpoint path; return it
    once the discovery file is on disk."""
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
    # Wait for the engine to bind and publish the discovery file.
    deadline = time.monotonic() + 30
    while not endpoint.exists():
        assert proc.poll() is None, "engine exited before writing discovery file"
        assert time.monotonic() < deadline, "discovery file never appeared"
        time.sleep(0.05)
    return proc


def _reap(proc):
    if proc.poll() is None:
        proc.kill()
        proc.wait(timeout=5)
    if proc.stdin and not proc.stdin.closed:
        proc.stdin.close()


def test_supervised_engine_exits_and_cleans_up_when_stdin_closes(tmp_path):
    endpoint = tmp_path / "rpc.json"
    proc = _boot_engine(endpoint)
    try:
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
        _reap(proc)


@pytest.mark.skipif(sys.platform == "win32",
                    reason="POSIX signals; win32 shuts down via the stdin watchdog")
@pytest.mark.parametrize("sig", [signal.SIGTERM, signal.SIGINT], ids=["sigterm", "sigint"])
def test_engine_exits_zero_and_cleans_up_on_posix_signal(tmp_path, sig):
    """The other two doors, on the real boot path. SIGTERM used to kill the
    process outright (exit -15) and strand the discovery file — spec §2.1 says
    it is removed on SIGTERM/SIGINT just as on a normal exit."""
    endpoint = tmp_path / "rpc.json"
    proc = _boot_engine(endpoint)
    try:
        proc.send_signal(sig)
        proc.wait(timeout=30)
        assert proc.returncode == 0, f"{sig!r} did not exit cleanly"
        assert not endpoint.exists(), "discovery file was not cleaned up on exit"
    finally:
        _reap(proc)

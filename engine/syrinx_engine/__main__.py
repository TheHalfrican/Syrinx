"""Entry point: run the Syrinx engine on the platform's transport.

* **Linux** (``sys.platform == "linux"``) → the ``sh.syrinx.Engine1`` D-Bus
  service, exactly as before. Run directly (``python -m syrinx_engine``) or via
  the ``syrinx-engine`` script; in production started by the
  ``syrinx-engine.service`` user unit.
* **Windows / macOS** → the JSON-RPC WebSocket server (``rpc.py``).

Override with ``SYRINX_TRANSPORT=dbus|rpc|both`` for dev and contract testing
(``both`` exports on D-Bus *and* serves RPC — Linux only). The warmup-in-
background pattern is preserved on every path.
"""

import asyncio
import logging
import os
import sys
import threading
import time

log = logging.getLogger("syrinx.engine")

_UNSET = object()

# How often the Windows watchdog peeks the parent pipe (see _start_stdin_watchdog).
_WATCHDOG_POLL_SECONDS = 0.2


def _transport() -> str:
    override = os.environ.get("SYRINX_TRANSPORT")
    if override:
        return override.lower()
    return "dbus" if sys.platform == "linux" else "rpc"


# --- supervised lifecycle (spec §13.1) ------------------------------------


def _supervised() -> bool:
    """True when the app spawned us as a supervised child (Win/mac, seam 1.2).
    Linux/systemd never sets this, so the D-Bus path is untouched."""
    return os.environ.get("SYRINX_SUPERVISED") == "1"


def _start_stdin_watchdog(on_parent_gone, *, stdin=_UNSET, _exit=os._exit) -> bool:
    """Under ``SYRINX_SUPERVISED=1`` the parent keeps our stdin an open pipe it
    never writes to; when the pipe closes (parent quit or crash) we know the
    parent is gone. A daemon thread watches that pipe and, when it closes, runs
    ``on_parent_gone`` (explicit cleanup — e.g. removing the discovery file) and
    then ``os._exit(0)``. The explicit cleanup is required because ``_exit``
    skips ``finally``/``atexit``.

    Detection is OS-specific by necessity:

    * **Windows** — we must *not* leave a blocking read pending on the stdin
      handle. Native-extension DLL loads (numpy at boot, torch during warmup)
      touch fd 0 during their init, and a pending ``ReadFile`` on that handle
      deadlocks the load (verified). So we *poll* the pipe with
      ``PeekNamedPipe`` and ``time.sleep`` between peeks — the handle is only
      touched instantaneously, never held. A non-pipe stdin (a dev console)
      fails the peek and is treated as "cannot watch".
    * **POSIX** (macOS; Linux never supervises) — ``dlopen`` doesn't touch
      fd 0, so a plain blocking ``os.read`` is safe and simplest.

    ``stdin``/``_exit`` are test seams. Returns ``True`` if the watchdog was
    armed. If stdin is unusable (``None``, no file descriptor, or — on Windows —
    not a pipe: a frozen/windowed/console context), we cannot watch the parent:
    log a warning and return ``False`` **without exiting** (conservative — a
    missing watch must not itself take the engine down)."""
    stream = sys.stdin if stdin is _UNSET else stdin
    if stream is None:
        log.warning("SYRINX_SUPERVISED=1 but sys.stdin is None; "
                    "cannot watch parent — continuing unsupervised")
        return False
    try:
        fd = stream.fileno()
    except (OSError, ValueError, AttributeError):
        log.warning("SYRINX_SUPERVISED=1 but sys.stdin has no file descriptor; "
                    "cannot watch parent — continuing unsupervised")
        return False

    if os.name == "nt":
        import ctypes
        import msvcrt
        from ctypes import wintypes

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

        try:
            handle = msvcrt.get_osfhandle(fd)
        except OSError:
            log.warning("SYRINX_SUPERVISED=1 but stdin has no OS handle; "
                        "cannot watch parent — continuing unsupervised")
            return False

        def _pipe_open() -> bool:
            # PeekNamedPipe succeeds (nonzero) while the write end is open — even
            # with bytes buffered; it fails once the pipe is broken/closed, or if
            # the fd is not a pipe at all.
            avail = wintypes.DWORD(0)
            return bool(kernel32.PeekNamedPipe(
                wintypes.HANDLE(handle), None, 0, None, ctypes.byref(avail), None))

        if not _pipe_open():
            log.warning("SYRINX_SUPERVISED=1 but stdin is not a readable pipe; "
                        "cannot watch parent — continuing unsupervised")
            return False

        def _wait_for_parent() -> None:
            while _pipe_open():
                time.sleep(_WATCHDOG_POLL_SECONDS)
    else:
        def _wait_for_parent() -> None:
            # b"" == EOF (parent gone); a byte on the pipe (contract says none is
            # ever written) just loops and keeps waiting.
            try:
                while os.read(fd, 1):
                    pass
            except OSError:  # read error == parent gone
                pass

    def _watch() -> None:
        _wait_for_parent()
        try:
            on_parent_gone()
        except Exception:  # noqa: BLE001 — cleanup is best-effort; still exit
            log.exception("stdin watchdog cleanup failed")
        _exit(0)

    threading.Thread(target=_watch, name="syrinx-stdin-watchdog", daemon=True).start()
    return True


async def _run_dbus() -> None:
    from dbus_next.aio import MessageBus

    from .service import EngineInterface

    bus = await MessageBus().connect()
    engine = EngineInterface()

    bus.export("/sh/syrinx/Engine", engine)
    await bus.request_name("sh.syrinx.Engine")

    log.info("syrinx-engine ready on sh.syrinx.Engine (backend=%s)", engine.backend_name)

    # Warm the models in the background so the bus name is claimed immediately
    # and clients can attach while weights load.
    asyncio.create_task(engine.warmup())

    await asyncio.get_event_loop().create_future()  # run forever


async def _run_rpc() -> None:
    from . import rpc

    if _supervised():
        # Arm before serving so a parent that dies mid-boot still triggers
        # cleanup. The path resolves identically to the one start_server writes
        # (both go through discovery_path(), honoring SYRINX_RPC_ENDPOINT).
        path = rpc.discovery_path()
        _start_stdin_watchdog(lambda: rpc.remove_discovery(path))

    await rpc.run()


async def _run_both() -> None:
    """Dev/contract-testing only: export on D-Bus and serve RPC over one core."""
    from dbus_next.aio import MessageBus

    from . import rpc
    from .core import EngineCore
    from .service import EngineInterface

    core = EngineCore()
    iface = EngineInterface()
    iface._core = core  # share one core between both wrappers

    bus = await MessageBus().connect()
    bus.export("/sh/syrinx/Engine", iface)
    await bus.request_name("sh.syrinx.Engine")

    # start_server wires core._emit to the RPC broadcast; fan out so BOTH the
    # D-Bus signal and the RPC notification fire for every emission.
    handle = await rpc.start_server(core)
    rpc_emit, rpc_props = core._emit, core._emit_props

    def dbus_emit(name, *a):
        getattr(iface, name)(*a)

    core._emit = lambda name, *a: (dbus_emit(name, *a), rpc_emit(name, *a))
    core._emit_props = lambda changed: (iface.emit_properties_changed(changed), rpc_props(changed))

    log.info("syrinx-engine ready on D-Bus + RPC (backend=%s)", core.backend_name)
    asyncio.create_task(core.warmup())
    try:
        await asyncio.get_event_loop().create_future()  # run forever
    finally:
        await handle.aclose()


async def _run() -> None:
    transport = _transport()
    if transport == "dbus":
        await _run_dbus()
    elif transport == "rpc":
        await _run_rpc()
    elif transport == "both":
        if sys.platform != "linux":
            raise SystemExit("SYRINX_TRANSPORT=both is Linux-only (needs a session bus)")
        await _run_both()
    else:
        raise SystemExit(f"unknown SYRINX_TRANSPORT={transport!r} (want dbus|rpc|both)")


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(message)s")
    try:
        asyncio.run(_run())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()

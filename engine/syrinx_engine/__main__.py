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

log = logging.getLogger("syrinx.engine")


def _transport() -> str:
    override = os.environ.get("SYRINX_TRANSPORT")
    if override:
        return override.lower()
    return "dbus" if sys.platform == "linux" else "rpc"


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

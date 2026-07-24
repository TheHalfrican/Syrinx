"""Contract-test machinery: two adapters over the same engine core so one set
of method/signal exercises runs against BOTH wrappers.

* :class:`DbusAdapter` drives the dbus_next ``EngineInterface`` in-process
  (no real bus needed — signal emission is a no-op without one, so we capture
  through the same ``_emit`` seam the wrapper wires up, while still calling the
  real ``@signal()`` methods for coverage).
* :class:`RpcAdapter` starts the JSON-RPC WebSocket server on an ephemeral port
  and talks to it with a real websockets client (handshake, requests, and
  notifications over the wire).

Both expose the same async surface — ``call(method, *args)``, ``notifications``,
``wait_for(name)``, ``core`` — so an exercise written once holds both transports
to the same behavior (MULTIPLATPLAN §1.1 "Drift protection").
"""

import asyncio
import inspect
import json
from pathlib import Path

from websockets.asyncio.client import connect

from syrinx_engine.service import EngineInterface
from syrinx_engine import rpc


class EngineCallError(Exception):
    """A method call that came back as a JSON-RPC error (or, on D-Bus, a raised
    handler exception). ``str(self)`` is the verbatim engine message so the
    app's substring checks behave identically on both transports (spec §7.2)."""

    def __init__(self, message, error=None):
        super().__init__(message)
        self.error = error  # the full {code,message,data} object on RPC, else None


class _Adapter:
    async def wait_for(self, name, timeout=2.0):
        async def poll():
            while not any(n == name for n, _ in self.notifications):
                await asyncio.sleep(0.005)
        await asyncio.wait_for(poll(), timeout)


class DbusAdapter(_Adapter):
    def __init__(self):
        self.iface = EngineInterface()
        self.core = self.iface._core
        self.notifications = []
        real_emit = self.core._emit          # calls the real @signal() methods
        real_props = self.core._emit_props

        def emit(name, *a):
            self.notifications.append((name, list(a)))
            real_emit(name, *a)

        def props(changed):
            self.notifications.append(("PropertiesChanged", changed))
            real_props(changed)

        self.core._emit = emit
        self.core._emit_props = props

    async def call(self, method, *args):
        self.core._audio_lock = asyncio.Lock()
        fn = getattr(type(self.iface), method)
        fn = getattr(fn, "__wrapped__", fn)
        out = fn(self.iface, *args)
        try:
            res = await out if inspect.iscoroutine(out) else out
        except Exception as e:  # dbus_next would marshal this into an error reply
            raise EngineCallError(str(e))
        # let any spawned task run so its signals land before we return
        pending = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        return res

    async def aclose(self):
        pass


class RpcAdapter(_Adapter):
    def __init__(self, core, endpoint_path):
        self.core = core
        self.notifications = []
        self._endpoint = str(endpoint_path)
        self._pending = {}
        self._next_id = 1
        self._ws = None
        self._handle = None
        self._reader = None
        self.discovery = None

    async def start(self):
        self._handle = await rpc.start_server(self.core, endpoint_path=self._endpoint)
        self.discovery = json.loads(Path(self._endpoint).read_text())
        self._ws = await connect(self.discovery["url"])
        self._reader = asyncio.create_task(self._read_loop())
        res = await self.call("Authenticate", self.discovery["token"])
        assert res is True

    async def _read_loop(self):
        try:
            async for raw in self._ws:
                msg = json.loads(raw)
                if isinstance(msg, dict) and "id" in msg and msg["id"] is not None \
                        and ("result" in msg or "error" in msg) and msg["id"] in self._pending:
                    self._pending.pop(msg["id"]).set_result(msg)
                elif isinstance(msg, dict) and "method" in msg:
                    self.notifications.append((msg["method"], msg.get("params")))
        except Exception:  # noqa: BLE001 — socket closed under us
            pass

    async def call(self, method, *args):
        mid = self._next_id
        self._next_id += 1
        fut = asyncio.get_event_loop().create_future()
        self._pending[mid] = fut
        await self._ws.send(json.dumps(
            {"jsonrpc": "2.0", "method": method, "params": list(args), "id": mid}
        ))
        msg = await asyncio.wait_for(fut, 10.0)
        if "error" in msg:
            raise EngineCallError(msg["error"]["message"], msg["error"])
        return msg["result"]

    async def aclose(self):
        if self._reader:
            self._reader.cancel()
        if self._ws:
            await self._ws.close()
        if self._handle:
            await self._handle.aclose()

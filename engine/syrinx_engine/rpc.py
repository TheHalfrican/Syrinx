"""JSON-RPC 2.0 over a localhost WebSocket — the Windows/macOS transport.

A second thin wrapper over :class:`~syrinx_engine.core.EngineCore`, mirroring the
D-Bus service byte-for-byte per ``docs/RPC-PROTOCOL.md``. Linux keeps using
D-Bus; this server is used on Win/mac (and by the contract tests / dev tooling
via ``SYRINX_TRANSPORT``).

Shape:

* one WebSocket server bound to ``127.0.0.1`` on an ephemeral port;
* a discovery file (``rpc.json``) with a fresh per-run token, written atomically
  after bind and removed on clean shutdown;
* first-message ``Authenticate`` handshake (constant-time token check);
* each request handled as its own task (so the blocking ``Transcribe`` never
  stalls other requests); each connection has a single writer draining a queue
  so responses and broadcast notifications never interleave a socket write;
* engine methods dispatched to the core; exceptions mapped to ``-32000`` with
  ``str(exc)`` preserved verbatim (load-bearing — see spec §7.2).
"""

import asyncio
import hmac
import inspect
import json
import logging
import os
import secrets
import sys
from pathlib import Path

import platformdirs
from websockets.asyncio.server import serve
from websockets.exceptions import ConnectionClosed

log = logging.getLogger("syrinx.engine.rpc")

PROTOCOL_VERSION = 1

# JSON-RPC error codes (spec §7.1)
PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603
ENGINE_ERROR = -32000
UNAUTHORIZED = -32001
NOT_AUTHENTICATED = -32002

# WebSocket close code for a policy violation (spec §2.3)
CLOSE_POLICY_VIOLATION = 1008

# Transport-only methods with no D-Bus / core analog (spec §0, §5, §9).
TRANSPORT_METHODS = ("Authenticate", "GetModelLoaded", "GetBackend", "GetProtocolVersion")


def engine_method_names(core) -> "list[str]":
    """The engine surface exposed over RPC: every PascalCase public method on
    the core (the same set the D-Bus ``@method()`` decorators expose). Private
    helpers are ``_lower``/``lower`` and are excluded, so this is a single
    source of truth the drift test pins against the D-Bus interface."""
    return sorted(
        n for n in dir(core)
        if n[:1].isupper() and callable(getattr(core, n))
    )


# --- discovery file -------------------------------------------------------


def discovery_path() -> Path:
    """Resolve the discovery-file path (spec §2.2). ``SYRINX_RPC_ENDPOINT``
    (absolute path) overrides the per-OS default."""
    override = os.environ.get("SYRINX_RPC_ENDPOINT")
    if override:
        return Path(override)
    if sys.platform == "linux":
        # XDG_RUNTIME_DIR (tmpfs, 0700) is the right home for a session secret;
        # fall back to the data dir when it is unset.
        if os.environ.get("XDG_RUNTIME_DIR"):
            base = platformdirs.user_runtime_dir("syrinx", "syrinx")
        else:
            base = platformdirs.user_data_dir("syrinx", "syrinx")
    else:
        # Windows: %LOCALAPPDATA%\syrinx\syrinx ; macOS: ~/Library/Application Support/syrinx
        base = platformdirs.user_data_dir("syrinx", "syrinx")
    return Path(base) / "rpc.json"


def write_discovery(path: Path, port: int, token: str) -> None:
    """Write the discovery file atomically (temp + rename), 0600 on POSIX."""
    path.parent.mkdir(parents=True, exist_ok=True)
    data = {
        "protocol": PROTOCOL_VERSION,
        "port": port,
        "token": token,
        "pid": os.getpid(),
        "url": f"ws://127.0.0.1:{port}",
    }
    tmp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    fd = os.open(str(tmp), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise
    os.replace(str(tmp), str(path))
    if os.name == "posix":
        # a prior umask can mask the O_CREAT mode; pin it explicitly
        os.chmod(str(path), 0o600)


def remove_discovery(path: "Path | None") -> None:
    if path is not None:
        try:
            path.unlink(missing_ok=True)
        except OSError:
            log.exception("failed to remove discovery file %s", path)


# --- server ---------------------------------------------------------------


def _result(msg_id, result) -> str:
    return json.dumps({"jsonrpc": "2.0", "result": result, "id": msg_id})


def _error(msg_id, code, message, data=None) -> str:
    err = {"code": code, "message": message}
    if data is not None:
        err["data"] = data
    return json.dumps({"jsonrpc": "2.0", "error": err, "id": msg_id})


class RpcServer:
    """Serves one :class:`EngineCore` over JSON-RPC. Broadcasts signals /
    PropertiesChanged to every authenticated connection."""

    def __init__(self, core, token: str, protocol_version: int = PROTOCOL_VERSION) -> None:
        self._core = core
        self._token = token
        self._protocol = protocol_version
        self._queues: "set[asyncio.Queue]" = set()  # authenticated send queues
        self._methods = {n: getattr(core, n) for n in engine_method_names(core)}
        # exact positional arity per exposed method (bound methods: self excluded)
        self._arity = {
            n: len(inspect.signature(fn).parameters) for n, fn in self._methods.items()
        }
        self._arity.update(
            {"GetModelLoaded": 0, "GetBackend": 0, "GetProtocolVersion": 0}
        )

    # --- notifications (called synchronously from the core's emit seam) ----

    def broadcast(self, name, *args) -> None:
        msg = json.dumps({"jsonrpc": "2.0", "method": name, "params": list(args)})
        for q in list(self._queues):
            q.put_nowait(msg)

    def broadcast_props(self, changed) -> None:
        msg = json.dumps({"jsonrpc": "2.0", "method": "PropertiesChanged", "params": changed})
        for q in list(self._queues):
            q.put_nowait(msg)

    def wire(self) -> None:
        """Point the core's emitter seam at this server's broadcast."""
        self._core._emit = self.broadcast
        self._core._emit_props = self.broadcast_props

    # --- connection handling ----------------------------------------------

    async def handle(self, ws) -> None:
        send_q: asyncio.Queue = asyncio.Queue()
        authenticated = False
        writer = asyncio.create_task(self._writer(ws, send_q))
        tasks: "set[asyncio.Task]" = set()
        try:
            async for raw in ws:
                try:
                    obj = json.loads(raw)
                except (json.JSONDecodeError, TypeError, ValueError):
                    send_q.put_nowait(_error(None, PARSE_ERROR, "parse error"))
                    continue

                if not (isinstance(obj, dict) and obj.get("jsonrpc") == "2.0"
                        and isinstance(obj.get("method"), str)):
                    msg_id = obj.get("id") if isinstance(obj, dict) else None
                    send_q.put_nowait(_error(msg_id, INVALID_REQUEST, "invalid request"))
                    continue

                method = obj["method"]
                msg_id = obj.get("id")
                params = obj.get("params", [])

                if not authenticated:
                    if method == "Authenticate":
                        token = params[0] if isinstance(params, list) and params else ""
                        if hmac.compare_digest(str(token), self._token):
                            authenticated = True
                            self._queues.add(send_q)
                            send_q.put_nowait(_result(msg_id, True))
                        else:
                            send_q.put_nowait(_error(msg_id, UNAUTHORIZED, "invalid token"))
                            await self._drain_and_close(ws, send_q)
                            break
                    else:
                        send_q.put_nowait(_error(msg_id, NOT_AUTHENTICATED, "not authenticated"))
                        await self._drain_and_close(ws, send_q)
                        break
                    continue

                if method == "Authenticate":
                    # already authenticated → no-op success (spec §2.3.5)
                    send_q.put_nowait(_result(msg_id, True))
                    continue

                # Each request is its own task so a blocking Transcribe cannot
                # stall the others (spec §7.4).
                t = asyncio.create_task(self._dispatch(send_q, method, msg_id, params))
                tasks.add(t)
                t.add_done_callback(tasks.discard)
        except ConnectionClosed:
            pass
        finally:
            self._queues.discard(send_q)
            for t in tasks:
                t.cancel()
            writer.cancel()

    async def _drain_and_close(self, ws, send_q) -> None:
        """Flush any queued frame (the error we just posted), then close 1008."""
        try:
            while not send_q.empty():
                await ws.send(send_q.get_nowait())
        except ConnectionClosed:
            pass
        await ws.close(CLOSE_POLICY_VIOLATION, "policy violation")

    async def _writer(self, ws, send_q: asyncio.Queue) -> None:
        try:
            while True:
                msg = await send_q.get()
                await ws.send(msg)
        except (ConnectionClosed, asyncio.CancelledError):
            pass

    async def _dispatch(self, send_q, method, msg_id, params) -> None:
        if not isinstance(params, list):
            send_q.put_nowait(_error(msg_id, INVALID_PARAMS, "params must be an array"))
            return
        if method not in self._methods and method not in ("GetModelLoaded", "GetBackend", "GetProtocolVersion"):
            send_q.put_nowait(_error(msg_id, METHOD_NOT_FOUND, f"method not found: {method}"))
            return
        if len(params) != self._arity.get(method, -1):
            send_q.put_nowait(_error(
                msg_id, INVALID_PARAMS,
                f"{method} expects {self._arity.get(method)} argument(s), got {len(params)}",
            ))
            return
        try:
            if method == "GetProtocolVersion":
                result = self._protocol
            elif method == "GetModelLoaded":
                result = self._core._model_loaded
            elif method == "GetBackend":
                result = self._core.backend_name
            else:
                out = self._methods[method](*params)
                result = await out if inspect.isawaitable(out) else out
        except Exception as e:  # noqa: BLE001 — engine error surfaced verbatim
            # str(exc) is what a D-Bus caller sees; the app string-matches it.
            send_q.put_nowait(_error(
                msg_id, ENGINE_ERROR, str(e), data={"type": type(e).__name__}
            ))
            return
        send_q.put_nowait(_result(msg_id, result))


# --- lifecycle ------------------------------------------------------------


class RpcHandle:
    """Handle to a running server: the WebSocket server, its port/token, the
    discovery-file path, and the :class:`RpcServer`."""

    def __init__(self, ws_server, port, token, path, server) -> None:
        self.ws_server = ws_server
        self.port = port
        self.token = token
        self.path = path
        self.server = server

    async def aclose(self) -> None:
        remove_discovery(self.path)
        self.ws_server.close()
        await self.ws_server.wait_closed()


async def start_server(core, *, write_file: bool = True, endpoint_path=None) -> RpcHandle:
    """Bind the WebSocket server on 127.0.0.1:0, wire the emitter seam, and (by
    default) write the discovery file. Warmup is the caller's responsibility."""
    token = secrets.token_hex(32)
    server = RpcServer(core, token)
    ws_server = await serve(server.handle, "127.0.0.1", 0)
    port = ws_server.sockets[0].getsockname()[1]
    server.wire()
    path = None
    if write_file:
        path = Path(endpoint_path) if endpoint_path else discovery_path()
        write_discovery(path, port, token)
    log.info("syrinx-engine RPC listening on ws://127.0.0.1:%d (backend=%s)", port, core.backend_name)
    return RpcHandle(ws_server, port, token, path, server)


async def run(core=None) -> None:
    """Production entry point (Win/mac): serve forever, warm models in the
    background, remove the discovery file on clean shutdown."""
    from .core import EngineCore

    if core is None:
        core = EngineCore()
    handle = await start_server(core)

    import atexit

    atexit.register(remove_discovery, handle.path)

    # Warm models in the background so the port is live immediately and clients
    # can attach while weights load (spec §7.3).
    asyncio.create_task(core.warmup())
    try:
        await asyncio.get_event_loop().create_future()  # run forever
    finally:
        await handle.aclose()

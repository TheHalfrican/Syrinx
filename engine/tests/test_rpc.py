"""RPC-transport specifics that have no D-Bus analog: the discovery file, the
first-message Authenticate handshake, auth failures + close codes, the four
transport-only methods, and error mapping (spec §2, §5, §7, §9).
"""

import asyncio
import json
import os
import stat
import sys

import pytest
from websockets.asyncio.client import connect
from websockets.exceptions import ConnectionClosed

from syrinx_engine import rpc
from syrinx_engine.core import EngineCore


def run(coro):
    return asyncio.run(coro)


async def _server(tmp_path, core=None):
    core = core or EngineCore()
    handle = await rpc.start_server(core, endpoint_path=str(tmp_path / "rpc.json"))
    disc = json.loads((tmp_path / "rpc.json").read_text())
    return core, handle, disc


async def _send(ws, method, params, mid=0):
    await ws.send(json.dumps({"jsonrpc": "2.0", "method": method, "params": params, "id": mid}))
    return json.loads(await asyncio.wait_for(ws.recv(), 5))


# --- discovery file ------------------------------------------------------


def test_discovery_file_has_the_required_fields(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            assert disc["protocol"] == 1
            assert disc["port"] == handle.port
            assert len(disc["token"]) == 64
            int(disc["token"], 16)  # 64 hex chars
            assert disc["pid"] == os.getpid()
            assert disc["url"] == f"ws://127.0.0.1:{handle.port}"
        finally:
            await handle.aclose()
    run(go())


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX file mode; Windows uses ACLs")
def test_discovery_file_is_0600_on_posix(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            mode = stat.S_IMODE(os.stat(tmp_path / "rpc.json").st_mode)
            assert mode == 0o600
        finally:
            await handle.aclose()
    run(go())


def test_discovery_file_is_removed_on_clean_shutdown(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        assert (tmp_path / "rpc.json").exists()
        await handle.aclose()
        assert not (tmp_path / "rpc.json").exists()
    run(go())


def test_fresh_token_each_run(tmp_path):
    async def go():
        _, h1, d1 = await _server(tmp_path)
        await h1.aclose()
        _, h2, d2 = await _server(tmp_path)
        await h2.aclose()
        assert d1["token"] != d2["token"]
    run(go())


# --- handshake / auth ----------------------------------------------------


def test_authenticate_then_call_getters(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            async with connect(disc["url"]) as ws:
                assert (await _send(ws, "Authenticate", [disc["token"]], 0))["result"] is True
                assert (await _send(ws, "GetProtocolVersion", [], 1))["result"] == 1
                assert (await _send(ws, "GetBackend", [], 2))["result"] in ("cpu", "cuda", "rocm")
                assert (await _send(ws, "GetModelLoaded", [], 3))["result"] is False
        finally:
            await handle.aclose()
    run(go())


def test_second_authenticate_is_a_noop_success(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            async with connect(disc["url"]) as ws:
                await _send(ws, "Authenticate", [disc["token"]], 0)
                assert (await _send(ws, "Authenticate", [disc["token"]], 1))["result"] is True
        finally:
            await handle.aclose()
    run(go())


def test_wrong_token_is_unauthorized_then_closes_1008(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            async with connect(disc["url"]) as ws:
                reply = await _send(ws, "Authenticate", ["deadbeef"], 0)
                assert reply["error"]["code"] == -32001
                assert reply["error"]["message"] == "invalid token"
                with pytest.raises(ConnectionClosed) as ei:
                    await asyncio.wait_for(ws.recv(), 5)
                assert ei.value.rcvd.code == 1008
        finally:
            await handle.aclose()
    run(go())


def test_method_before_auth_is_rejected_then_closes_1008(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            async with connect(disc["url"]) as ws:
                reply = await _send(ws, "GetBackend", [], 5)
                assert reply["error"]["code"] == -32002
                assert reply["error"]["message"] == "not authenticated"
                with pytest.raises(ConnectionClosed) as ei:
                    await asyncio.wait_for(ws.recv(), 5)
                assert ei.value.rcvd.code == 1008
        finally:
            await handle.aclose()
    run(go())


# --- error mapping -------------------------------------------------------


async def _authed(disc):
    ws = await connect(disc["url"])
    await _send(ws, "Authenticate", [disc["token"]], 0)
    return ws


def test_unknown_method_is_method_not_found(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            ws = await _authed(disc)
            reply = await _send(ws, "NoSuchMethod", [], 1)
            assert reply["error"]["code"] == -32601
            await ws.close()
        finally:
            await handle.aclose()
    run(go())


def test_wrong_arity_is_invalid_params(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            ws = await _authed(disc)
            reply = await _send(ws, "Speak", ["only one arg"], 1)  # Speak needs 2
            assert reply["error"]["code"] == -32602
            await ws.close()
        finally:
            await handle.aclose()
    run(go())


def test_bad_json_is_parse_error(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            ws = await _authed(disc)
            await ws.send("{not json")
            reply = json.loads(await asyncio.wait_for(ws.recv(), 5))
            assert reply["error"]["code"] == -32700
            assert reply["id"] is None
            await ws.close()
        finally:
            await handle.aclose()
    run(go())


def test_engine_exception_maps_to_minus_32000_verbatim(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            ws = await _authed(disc)
            spec = json.dumps({"name": "Dup", "voice_type": "cloned"})
            assert (await _send(ws, "CreateProfile", [spec], 1)).get("result")
            reply = await _send(ws, "CreateProfile", [spec], 2)
            assert reply["error"]["code"] == -32000
            assert reply["error"]["message"] == "UNIQUE constraint failed: profiles.name"
            assert reply["error"]["data"]["type"] == "IntegrityError"
            await ws.close()
        finally:
            await handle.aclose()
    run(go())


def test_missing_method_field_is_invalid_request(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            ws = await _authed(disc)
            await ws.send(json.dumps({"jsonrpc": "2.0", "id": 3}))
            reply = json.loads(await asyncio.wait_for(ws.recv(), 5))
            assert reply["error"]["code"] == -32600
            await ws.close()
        finally:
            await handle.aclose()
    run(go())


def test_non_array_params_is_invalid_params(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            ws = await _authed(disc)
            await ws.send(json.dumps({"jsonrpc": "2.0", "method": "Hardware", "params": {}, "id": 4}))
            reply = json.loads(await asyncio.wait_for(ws.recv(), 5))
            assert reply["error"]["code"] == -32602
            await ws.close()
        finally:
            await handle.aclose()
    run(go())


def test_void_method_returns_null_result(tmp_path):
    async def go():
        core, handle, disc = await _server(tmp_path)
        try:
            ws = await _authed(disc)
            reply = await _send(ws, "SetSetting", ["seedvc_steps", "3"], 1)
            assert reply == {"jsonrpc": "2.0", "result": None, "id": 1}
            await ws.close()
        finally:
            await handle.aclose()
    run(go())


# --- PropertiesChanged notification --------------------------------------


def test_warmup_broadcasts_properties_changed(tmp_path):
    async def go():
        class FakeLoadable:
            backend = "cpu"
            clone_engine = "kokoro"  # non-qwen → warmup skips the qwen pre-import

            async def load(self):
                return None

        core = EngineCore()
        core._tts = FakeLoadable()
        core._stt = FakeLoadable()
        core, handle, disc = await _server(tmp_path, core=core)
        try:
            ws = await _authed(disc)
            await core.warmup()
            # the notification is broadcast to the authenticated socket
            msg = json.loads(await asyncio.wait_for(ws.recv(), 5))
            assert msg == {"jsonrpc": "2.0", "method": "PropertiesChanged",
                           "params": {"ModelLoaded": True}}
            assert (await _send(ws, "GetModelLoaded", [], 9))["result"] is True
            await ws.close()
        finally:
            await handle.aclose()
    run(go())


# --- endpoint path resolution --------------------------------------------


def test_endpoint_override_wins(tmp_path, monkeypatch):
    target = tmp_path / "custom" / "endpoint.json"
    monkeypatch.setenv("SYRINX_RPC_ENDPOINT", str(target))
    assert rpc.discovery_path() == target


def test_default_paths_per_os(monkeypatch):
    monkeypatch.delenv("SYRINX_RPC_ENDPOINT", raising=False)
    p = rpc.discovery_path()
    assert p.name == "rpc.json"
    parts = p.parts
    assert "syrinx" in parts

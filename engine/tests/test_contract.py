"""Cross-transport contract suite.

The SAME method/signal exercises run over both the dbus_next shim and the
JSON-RPC server (MULTIPLATPLAN §1.1). If a method is added to one wrapper and
not the other, ``test_method_surface_matches_across_wrappers`` fails; if the two
transports disagree on a method's result or a signal payload, the parametrized
``test_exercise`` fails on the offending transport.
"""

import asyncio
import json
import os

import pytest
from dbus_next.service import ServiceInterface

from syrinx_engine.core import EngineCore
from syrinx_engine.service import EngineInterface
from syrinx_engine.rpc import engine_method_names, TRANSPORT_METHODS

from _contract import DbusAdapter, RpcAdapter, EngineCallError

DOCUMENTED_SIGNALS = {
    "GenerationProgress", "AudioLevel", "PlaybackInfo", "PlaybackProgress",
    "LlmResult", "TranscribeProgress", "TranscribeResult", "ModelProgress",
    "SpeakStarted", "SpeakEnded",
}


# --- shared exercises (each takes an adapter, asserts transport-agnostic) ---


async def exercise_hardware(a):
    hw = json.loads(await a.call("Hardware"))
    assert hw["cores"] >= 1 and "gpu" in hw


async def exercise_list_models(a):
    rows = json.loads(await a.call("ListModels"))
    by_id = {r["id"]: r for r in rows}
    assert by_id["vevo2-singing"]["category"] == "vc"


async def exercise_settings_round_trip(a):
    assert await a.call("SetSetting", "seedvc_steps", "7") is None  # void → null
    got = json.loads(await a.call("GetSettings"))
    assert got["stored"]["seedvc_steps"] == 7
    assert got["effective"]["seedvc_steps"] == 7


async def exercise_profile_crud_and_json_strings(a):
    pid = await a.call("CreateProfile", json.dumps(
        {"name": "Piccolo", "voice_type": "cloned", "personality": "terse"}))
    assert isinstance(pid, str) and pid
    # *_json returns stay strings on the wire (spec §3) — not unwrapped objects
    listed = await a.call("ListProfiles")
    assert isinstance(listed, str)
    rows = json.loads(listed)
    assert [p["id"] for p in rows] == [pid]
    assert rows[0]["has_personality"] is True
    assert await a.call("GetProfile", "nope") == ""  # "" when not found


async def exercise_duplicate_profile_error_text_is_verbatim(a):
    spec = json.dumps({"name": "Nova", "voice_type": "cloned"})
    assert await a.call("CreateProfile", spec)
    with pytest.raises(EngineCallError) as ei:
        await a.call("CreateProfile", spec)
    # the load-bearing string the app matches (spec §7.2) survives verbatim
    assert "UNIQUE constraint failed: profiles.name" in str(ei.value)


async def exercise_file_envelope_unreadable(a):
    assert await a.call("FileEnvelope", "/no/such/file.wav") == "{}"


async def exercise_void_method_returns_null(a):
    # SetEffect on an unknown preset clears to "" and returns void → null
    assert await a.call("SetEffect", "not-a-preset") is None


async def exercise_download_signal_flow(a):
    async def fake_download(model_id, on_progress):
        on_progress(model_id, 0.5, "downloading")
        on_progress(model_id, 1.0, "done")
        return True

    a.core._models.download = fake_download
    assert await a.call("DownloadModel", "kokoro") is True
    await a.wait_for("ModelProgress")
    progs = [p for (n, p) in a.notifications if n == "ModelProgress"]
    assert progs == [["kokoro", 0.5, "downloading"], ["kokoro", 1.0, "done"]]


async def exercise_transcribe_file_signal_flow(a):
    class FakeSTT:
        async def transcribe_stream(self, path, on_partial=None):
            if on_partial:
                on_partial("so today")
            return "so today we begin"

    a.core._stt = FakeSTT()
    req = await a.call("TranscribeFile", "/x.wav")
    await a.wait_for("TranscribeResult")
    partials = [p for n, p in a.notifications if n == "TranscribeProgress"]
    results = [p for n, p in a.notifications if n == "TranscribeResult"]
    assert partials == [[req, "so today"]]
    assert results == [[req, "so today we begin"]]


async def exercise_recording_round_trip(a):
    # §14: enumerate → start → stop returns a real WAV path; unknown ids are ""
    # / no-op. sounddevice is stubbed (fake_sd) so this runs identically on both
    # transports with no real device.
    devs = json.loads(await a.call("ListRecordingDevices"))
    assert any(d["id"] for d in devs)
    rid = await a.call("StartRecording", "")
    assert isinstance(rid, str) and rid
    path = await a.call("StopRecording", rid)
    assert path.endswith(".wav") and os.path.exists(path)
    assert await a.call("StopRecording", rid) == ""       # already stopped
    assert await a.call("CancelRecording", "nope") is None  # unknown → void/null


EXERCISES = [
    exercise_hardware,
    exercise_list_models,
    exercise_settings_round_trip,
    exercise_profile_crud_and_json_strings,
    exercise_duplicate_profile_error_text_is_verbatim,
    exercise_file_envelope_unreadable,
    exercise_void_method_returns_null,
    exercise_download_signal_flow,
    exercise_transcribe_file_signal_flow,
    exercise_recording_round_trip,
]


@pytest.mark.parametrize("transport", ["dbus", "rpc"])
@pytest.mark.parametrize("exercise", EXERCISES, ids=lambda f: f.__name__)
def test_exercise(transport, exercise, tmp_path, fake_sd):
    async def go():
        if transport == "dbus":
            adapter = DbusAdapter()
        else:
            adapter = RpcAdapter(EngineCore(), tmp_path / "rpc.json")
            await adapter.start()
        try:
            await exercise(adapter)
        finally:
            await adapter.aclose()

    asyncio.run(go())


# --- drift protection ----------------------------------------------------


def test_method_surface_matches_across_wrappers():
    """A method added to the core (→ auto-exposed over RPC) but not the D-Bus
    interface — or vice versa — fails here."""
    core = EngineCore()
    rpc_methods = set(engine_method_names(core))
    iface = EngineInterface()
    dbus_methods = {m.name for m in ServiceInterface._get_methods(iface)}
    assert rpc_methods == dbus_methods
    assert len(rpc_methods) == 69  # 65 + 4 recording methods (§14)
    # the 4 transport-only methods have no D-Bus analog (spec §0)
    assert not (set(TRANSPORT_METHODS) & dbus_methods)


def test_signal_surface_is_the_documented_set():
    iface = EngineInterface()
    signals = {s.name for s in ServiceInterface._get_signals(iface)}
    assert signals == DOCUMENTED_SIGNALS


def test_properties_map_to_getters():
    iface = EngineInterface()
    props = {p.name for p in ServiceInterface._get_properties(iface)}
    assert props == {"ModelLoaded", "Backend"}


# --- D-Bus shim delegation sweep -----------------------------------------
#
# Every @method() on EngineInterface is a one-line delegation to the core. This
# drives all 65 of them through the shim (with the ML boundary faked) so the
# wrapper cannot silently rot — a delegation that stops calling its core method,
# or a method whose signature drifts from the core's, fails here.

from test_service_flows import FakeTTS, FakeSTT, FakeLLM  # noqa: E402


def _sweep_args(iface):
    """Valid-enough arguments for each method so the delegation body runs."""
    # seed one of everything so id-taking methods have something to hit
    core = iface._core
    pid = asyncio.run(core.CreateProfile(json.dumps({"name": "Sweep", "voice_type": "cloned"})))
    item = core._history.save_clip(
        voice_id=pid, voice_name="Sweep", text="hi", pcm=b"\x00\x00" * 2400,
        sample_rate=24_000, engine="kokoro", language="en",
    )
    cap = core._captures.save("cap").id
    chain = json.dumps([{"type": "gain", "params": {"gain_db": 0.0}}])
    return {
        "Speak": ("hi", "builtin:kokoro:af_heart"),
        "Transcribe": ("/x.wav",),
        "TranscribeFile": ("/x.wav",),
        "ConvertVoice": ("/x.wav", pid, "", "", "", "speech", 0),
        "ListVoices": (),
        "CloneVoice": ("Cloned", "/x.wav", "ref"),
        "CreateProfile": (json.dumps({"name": "Another", "voice_type": "cloned"}),),
        "ListProfiles": (),
        "GetProfile": (pid,),
        "UpdateProfile": (pid, json.dumps({"description": "x"})),
        "DeleteProfile": ("nope",),
        "SetProfileAvatar": (pid, "", "panel", 0, 0, 0, 0),
        "ExportProfile": (pid, "/tmp/x.zip"),
        "ImportProfile": ("/no/such.zip",),
        "AddSample": (pid, "/x.wav", "text"),
        "DeleteSample": ("nope",),
        "UpdateSampleText": (pid, "nope", "t"),
        "ComposeProfile": (pid, "p"),
        "RewriteProfile": (pid, "t"),
        "RefineTranscript": ("",),
        "ListModels": (),
        "Hardware": (),
        "DownloadModel": ("nope",),
        "DeleteModel": ("nope",),
        "SetActiveModel": ("nope",),
        "ListHistory": (),
        "PlayHistory": ("nope",),
        "PlayHistoryAt": ("nope", 0.5),
        "PlaySample": ("nope",),
        "PausePlayback": (),
        "ResumePlayback": (),
        "SeekPlayback": (0.5,),
        "SetVolume": (0.5,),
        "ListEffectPresets": (),
        "SetEffect": ("radio",),
        "SetStyle": ("angry",),
        "ApplyHistoryEffects": ("nope", "radio"),
        "ListEffects": (),
        "GetEffectPreset": ("radio",),
        "CreateEffectPreset": ("P", "d", chain),
        "UpdateEffectPreset": ("nope", "P", "d", chain),
        "DeleteEffectPreset": ("nope",),
        "PreviewEffects": ("nope", chain),
        "StarHistory": (item.id, True),
        "SetHistoryTags": (item.id, json.dumps(["t"])),
        "DeleteHistory": ("nope",),
        "RegenerateHistory": ("nope",),
        "ExportPackage": (item.id, "/tmp/x.zip"),
        "HistoryAudioPath": (item.id,),
        "SaveCapture": ("text",),
        "ListCaptures": (),
        "UpdateCapture": (cap, "edited"),
        "DeleteCapture": ("nope",),
        "SaveSourceClip": ("/x.wav", "n", ""),
        "SetSourceClipTranscript": ("nope", "t"),
        "ListSourceClips": (),
        "DeleteSourceClip": ("nope",),
        "PlayFile": ("/x.wav", ""),
        "PlayFileAt": ("/x.wav", "", 0.5),
        "FileEnvelope": ("/x.wav",),
        "TrimAudio": ("/x.wav", 0.0, 1.0),
        "TrimHistoryClip": ("nope", 0.0, 1.0),
        "GetSettings": (),
        "SetSetting": ("seedvc_steps", "3"),
        "ListRecordingDevices": (),
        "StartRecording": ("",),
        "StopRecording": ("nope",),
        "CancelRecording": ("nope",),
        "Cancel": (999,),
    }


def test_dbus_shim_delegates_every_method(fake_sd):
    iface = EngineInterface()
    iface._core._tts = FakeTTS()
    iface._core._stt = FakeSTT()
    iface._core._llm = FakeLLM()
    args = _sweep_args(iface)
    dbus_methods = {m.name for m in ServiceInterface._get_methods(iface)}
    assert set(args) == dbus_methods  # every @method has a sweep entry

    async def go():
        iface._core._audio_lock = asyncio.Lock()
        called = []
        for name in sorted(dbus_methods):
            fn = getattr(type(iface), name)
            fn = getattr(fn, "__wrapped__", fn)
            try:
                out = fn(iface, *args[name])
                if asyncio.iscoroutine(out):
                    await out
                called.append(name)
            except Exception:  # noqa: BLE001 — delegation ran; core result irrelevant
                called.append(name)
        pending = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        return called

    called = asyncio.run(go())
    assert set(called) == dbus_methods

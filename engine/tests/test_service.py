"""The engine method surface, driven directly against the transport-agnostic
``EngineCore``.

The core's methods are plain coroutines (no transport decorator), so they can
be constructed and awaited directly; signal emission goes through the injected
``_emit`` seam, a no-op here. The D-Bus wrapper and the RPC server are exercised
over these same methods by the contract tests in ``test_contract.py``.

Only the paths that stay clear of ML inference are exercised: Speak /
ConvertVoice / Transcribe spawn model loads, so RegenerateHistory is tested
for its *refusals* only.
"""

import asyncio
import inspect
import json
import shutil
from pathlib import Path

import numpy as np
import pytest

from syrinx_engine.core import EngineCore

RATE = 24_000


def call(iface, name, *args):
    """Invoke an engine method for real. Works against the core (plain
    coroutines) and the D-Bus shim (the @method() wrapper drops returns, so
    reach the coroutine it left on ``__wrapped__``)."""
    fn = getattr(type(iface), name)
    fn = getattr(fn, "__wrapped__", fn)
    out = fn(iface, *args)
    return asyncio.run(out) if inspect.iscoroutine(out) else out


@pytest.fixture
def iface():
    return EngineCore()


def tone(secs=1.0, rate=RATE, amp=0.5):
    t = np.linspace(0, secs, int(secs * rate), endpoint=False, dtype=np.float32)
    return (np.sin(2 * np.pi * 440 * t) * amp).astype(np.float32).tobytes()


def seed_clip(iface, **over):
    kw = dict(voice_id="v1", voice_name="Piccolo", text="hello",
              pcm=tone(), sample_rate=RATE, engine="kokoro", language="en")
    kw.update(over)
    return iface._history.save_clip(**kw)


# --- models / hardware ---------------------------------------------------


def test_list_models_is_parseable_and_complete(iface):
    rows = json.loads(call(iface, "ListModels"))
    by_id = {r["id"]: r for r in rows}
    assert "vevo2-singing" in by_id
    assert by_id["vevo2-singing"]["category"] == "vc"
    assert {"id", "display", "downloaded", "active", "warning"} <= set(rows[0])


def test_hardware_reports_cores(iface):
    hw = json.loads(call(iface, "Hardware"))
    assert hw["cores"] >= 1
    assert "gpu" in hw and "ram_gb" in hw


def test_download_model_refuses_unknown_ids(iface):
    assert call(iface, "DownloadModel", "nope") is False


def test_set_active_model_returns_the_category(iface):
    # vc engines are picked per-conversion — there is nothing to activate
    assert call(iface, "SetActiveModel", "seed-vc") == "vc"
    assert call(iface, "SetActiveModel", "kokoro") == "voice"
    assert call(iface, "SetActiveModel", "whisper-small") == "stt"
    assert call(iface, "SetActiveModel", "qwen3-0.6b") == "llm"
    assert call(iface, "SetActiveModel", "not-a-model") == ""


def test_delete_model_of_an_unknown_id_is_a_no_op(iface):
    call(iface, "DeleteModel", "not-a-model")


def test_properties(iface):
    assert iface._model_loaded is False
    assert iface.backend_name in ("cpu", "cuda", "rocm")


# --- settings ------------------------------------------------------------


def test_settings_round_trip(iface):
    assert json.loads(call(iface, "GetSettings"))["stored"] == {}
    call(iface, "SetSetting", "seedvc_steps", "7")
    call(iface, "SetSetting", "vc_max_secs", "45.5")
    got = json.loads(call(iface, "GetSettings"))
    assert got["stored"] == {"seedvc_steps": 7, "vc_max_secs": 45.5}
    # stored values win over the env fallbacks in the effective section
    assert got["effective"]["seedvc_steps"] == 7
    assert got["effective"]["vc_max_secs"] == 45.5


def test_setting_null_clears_a_key(iface):
    call(iface, "SetSetting", "seedvc_steps", "7")
    call(iface, "SetSetting", "seedvc_steps", "null")
    assert json.loads(call(iface, "GetSettings"))["stored"] == {}


def test_a_non_json_setting_value_is_ignored(iface):
    call(iface, "SetSetting", "seedvc_steps", "not json")
    assert json.loads(call(iface, "GetSettings"))["stored"] == {}


# --- file envelope / trimming -------------------------------------------


def test_file_envelope_of_a_real_wav(iface, make_wav):
    env = json.loads(call(iface, "FileEnvelope", str(make_wav("t.wav", secs=2.0))))
    assert len(env["bars"]) == 300
    assert env["duration"] == pytest.approx(2.0)
    assert max(env["bars"]) == 1.0


def test_file_envelope_of_an_unreadable_path_is_an_empty_object(iface, tmp_path):
    assert call(iface, "FileEnvelope", str(tmp_path / "nope.wav")) == "{}"


def test_trim_audio_rewrites_a_wav_in_place(iface, make_wav):
    path = make_wav("rec.wav", secs=3.0)
    out = call(iface, "TrimAudio", str(path), 0.5, 2.0)
    assert out == str(path)
    env = json.loads(call(iface, "FileEnvelope", out))
    assert env["duration"] == pytest.approx(1.5, abs=1e-3)


def test_trim_audio_refuses_a_selection_under_a_tenth_of_a_second(iface, make_wav):
    path = make_wav("rec.wav", secs=1.0)
    assert call(iface, "TrimAudio", str(path), 0.0, 0.05) == ""
    env = json.loads(call(iface, "FileEnvelope", str(path)))
    assert env["duration"] == pytest.approx(1.0)  # untouched


def test_trim_audio_of_a_non_wav_writes_a_sibling(iface, make_wav, tmp_path):
    """m4a/webm recordings can't be rewritten in place — they get a wav next
    to them and the original survives."""
    src = make_wav("rec.wav", secs=2.0)
    other = tmp_path / "rec.dat"
    shutil.copy(src, other)
    out = call(iface, "TrimAudio", str(other), 0.25, 1.25)
    assert out == str(tmp_path / "rec-trimmed.wav")
    assert Path(out).exists() and other.exists()
    assert json.loads(call(iface, "FileEnvelope", out))["duration"] == pytest.approx(1.0, abs=1e-3)


def test_trim_audio_of_an_unreadable_path_is_empty(iface, tmp_path):
    assert call(iface, "TrimAudio", str(tmp_path / "nope.wav"), 0.0, 1.0) == ""


def test_play_file_of_an_unreadable_path_is_zero(iface, tmp_path):
    assert call(iface, "PlayFile", str(tmp_path / "nope.wav"), "title") == 0
    assert call(iface, "PlayFileAt", str(tmp_path / "nope.wav"), "title", 0.5) == 0


def test_play_sample_of_an_unknown_id_is_zero(iface):
    assert call(iface, "PlaySample", "nope") == 0


def test_play_history_of_an_unknown_id_is_zero(iface):
    assert call(iface, "PlayHistory", "nope") == 0
    assert call(iface, "PlayHistoryAt", "nope", 0.5) == 0


# --- history -------------------------------------------------------------


def test_list_history_reflects_saved_clips(iface):
    item = seed_clip(iface, text="spoken words")
    rows = json.loads(call(iface, "ListHistory"))
    assert [r["id"] for r in rows] == [item.id]
    assert rows[0]["text"] == "spoken words"
    assert rows[0]["duration"] == pytest.approx(1.0)


def test_trim_history_clip_updates_the_listed_duration(iface):
    item = seed_clip(iface, pcm=tone(secs=3.0))
    assert call(iface, "TrimHistoryClip", item.id, 0.5, 2.0) is True
    rows = json.loads(call(iface, "ListHistory"))
    assert rows[0]["duration"] == pytest.approx(1.5, abs=1e-3)
    # too short, and unknown ids, are refused
    assert call(iface, "TrimHistoryClip", item.id, 0.0, 0.05) is False
    assert call(iface, "TrimHistoryClip", "nope", 0.0, 1.0) is False


def test_star_and_tag_a_history_row(iface):
    item = seed_clip(iface)
    call(iface, "StarHistory", item.id, True)
    call(iface, "SetHistoryTags", item.id, json.dumps(["demo", "  keep  ", "", 7]))
    row = json.loads(call(iface, "ListHistory"))[0]
    assert row["starred"] is True
    assert row["tags"] == ["demo", "keep", "7"]  # trimmed, blanks dropped


def test_set_history_tags_ignores_junk_payloads(iface):
    item = seed_clip(iface)
    call(iface, "SetHistoryTags", item.id, json.dumps(["keep"]))
    call(iface, "SetHistoryTags", item.id, "{not json")
    call(iface, "SetHistoryTags", item.id, json.dumps({"not": "a list"}))
    assert json.loads(call(iface, "ListHistory"))[0]["tags"] == ["keep"]


def test_history_audio_path_and_delete(iface):
    item = seed_clip(iface)
    path = Path(call(iface, "HistoryAudioPath", item.id))
    assert path.exists() and path.is_absolute()
    call(iface, "DeleteHistory", item.id)
    assert json.loads(call(iface, "ListHistory")) == []
    assert not path.exists()
    assert call(iface, "HistoryAudioPath", "nope") == ""


def test_export_package_writes_a_zip(iface, tmp_path):
    import zipfile

    item = seed_clip(iface)
    dest = tmp_path / "clip.zip"
    call(iface, "ExportPackage", item.id, str(dest))
    with zipfile.ZipFile(dest) as z:
        assert "manifest.json" in z.namelist()


def test_apply_history_effects_saves_a_new_row(iface):
    item = seed_clip(iface)
    new_id = call(iface, "ApplyHistoryEffects", item.id, "radio")
    assert new_id and new_id != item.id
    rows = {r["id"]: r for r in json.loads(call(iface, "ListHistory"))}
    assert len(rows) == 2
    assert rows[new_id]["voice_name"].endswith("· Radio")
    assert rows[new_id]["text"] == item.text


def test_apply_history_effects_refuses_unknown_rows_and_presets(iface):
    item = seed_clip(iface)
    assert call(iface, "ApplyHistoryEffects", "nope", "radio") == ""
    assert call(iface, "ApplyHistoryEffects", item.id, "not-a-preset") == ""


# --- regenerate: refusals only (the happy paths spawn ML tasks) ----------


def test_regenerate_unknown_row_is_zero(iface):
    assert call(iface, "RegenerateHistory", "nope") == 0


def test_regenerate_refuses_a_pre_recipe_conversion_row(iface):
    """A conversion row with no stored recipe: refusing beats re-speaking its
    transcript through a TTS engine."""
    item = seed_clip(iface, engine="seed_vc", vc_json="")
    assert call(iface, "RegenerateHistory", item.id) == 0
    for engine in ("chatterbox_vc", "vevo_timbre"):
        assert call(iface, "RegenerateHistory", seed_clip(iface, engine=engine).id) == 0


def test_regenerate_refuses_when_the_source_take_changed(iface, make_wav):
    """mtime/size pin the exact take — scratch recordings get overwritten by
    the next ◉, and re-converting a different take would be a lie."""
    src = make_wav("source.wav", secs=1.0)
    st = src.stat()
    recipe = {
        "source": str(src), "engine": "seed_vc", "mode": "speech",
        "semitones": 0, "label": "take 1",
        "mtime": int(st.st_mtime) + 5, "size": st.st_size,  # stale mtime
    }
    item = seed_clip(iface, engine="seed_vc", vc_json=json.dumps(recipe))
    assert call(iface, "RegenerateHistory", item.id) == 0

    recipe["mtime"] = int(st.st_mtime)
    recipe["size"] = st.st_size + 1  # stale size
    assert call(iface, "RegenerateHistory",
                seed_clip(iface, engine="seed_vc", vc_json=json.dumps(recipe)).id) == 0


def test_regenerate_refuses_when_the_source_is_gone(iface, make_wav):
    src = make_wav("source.wav", secs=1.0)
    st = src.stat()
    recipe = json.dumps({"source": str(src), "engine": "seed_vc", "mode": "speech",
                         "mtime": int(st.st_mtime), "size": st.st_size})
    item = seed_clip(iface, engine="seed_vc", vc_json=recipe)
    src.unlink()
    assert call(iface, "RegenerateHistory", item.id) == 0


def test_regenerate_treats_a_corrupt_recipe_as_a_vc_row(iface):
    item = seed_clip(iface, engine="seed_vc", vc_json="{not json")
    assert call(iface, "RegenerateHistory", item.id) == 0


# --- captures ------------------------------------------------------------


def test_capture_lifecycle(iface):
    assert call(iface, "SaveCapture", "   ") == ""  # nothing to save
    cid = call(iface, "SaveCapture", "dictated words")
    rows = json.loads(call(iface, "ListCaptures"))
    assert [r["id"] for r in rows] == [cid]
    assert rows[0]["text"] == "dictated words"
    call(iface, "UpdateCapture", cid, "edited")
    assert json.loads(call(iface, "ListCaptures"))[0]["text"] == "edited"
    call(iface, "DeleteCapture", cid)
    assert json.loads(call(iface, "ListCaptures")) == []


# --- source clips --------------------------------------------------------


def test_source_clip_lifecycle(iface, make_wav, tmp_path):
    path = make_wav("rec.wav", secs=0.5)
    cid = call(iface, "SaveSourceClip", str(path), "Take 1", "hello there")
    rows = json.loads(call(iface, "ListSourceClips"))
    assert [r["id"] for r in rows] == [cid]
    assert rows[0]["name"] == "Take 1"
    assert rows[0]["transcript"] == "hello there"
    assert Path(rows[0]["path"]).exists()

    call(iface, "SetSourceClipTranscript", cid, "corrected")
    assert json.loads(call(iface, "ListSourceClips"))[0]["transcript"] == "corrected"

    call(iface, "DeleteSourceClip", cid)
    assert json.loads(call(iface, "ListSourceClips")) == []


def test_save_source_clip_of_an_unreadable_path_is_empty(iface, tmp_path):
    assert call(iface, "SaveSourceClip", str(tmp_path / "nope.wav"), "n", "") == ""


# --- voice profiles ------------------------------------------------------


def test_profile_lifecycle(iface, make_wav):
    pid = call(iface, "CreateProfile", json.dumps({
        "name": "Piccolo", "voice_type": "cloned", "language": "en",
        "description": "namekian", "personality": "terse",
    }))
    rows = json.loads(call(iface, "ListProfiles"))
    assert [p["id"] for p in rows] == [pid]
    assert rows[0]["samples"] == 0
    assert rows[0]["has_personality"] is True

    sample = json.loads(call(iface, "AddSample", pid, str(make_wav("ref.wav")), "spoken ref"))
    assert sample["reference_text"] == "spoken ref"
    assert json.loads(call(iface, "ListProfiles"))[0]["samples"] == 1

    call(iface, "UpdateSampleText", pid, sample["sample_id"], "fixed ref")
    full = json.loads(call(iface, "GetProfile", pid))
    assert full["samples"][0]["reference_text"] == "fixed ref"

    call(iface, "UpdateProfile", pid, json.dumps({"description": "changed"}))
    assert json.loads(call(iface, "GetProfile", pid))["description"] == "changed"

    call(iface, "DeleteSample", sample["sample_id"])
    assert json.loads(call(iface, "GetProfile", pid))["samples"] == []

    call(iface, "DeleteProfile", pid)
    assert json.loads(call(iface, "ListProfiles")) == []
    assert call(iface, "GetProfile", pid) == ""


def test_clone_voice_creates_a_cloned_profile_with_one_sample(iface, make_wav):
    pid = call(iface, "CloneVoice", "Nail", str(make_wav("ref.wav")), "reference text")
    full = json.loads(call(iface, "GetProfile", pid))
    assert full["voice_type"] == "cloned"
    assert len(full["samples"]) == 1


def test_profile_avatar_and_package_round_trip(iface, make_wav, tmp_path):
    pid = call(iface, "CreateProfile", json.dumps({"name": "Nail", "voice_type": "cloned"}))
    call(iface, "AddSample", pid, str(make_wav("ref.wav")), "ref")
    photo = tmp_path / "face.png"
    photo.write_bytes(b"\x89PNG\r\n\x1a\n fake")
    call(iface, "SetProfileAvatar", pid, str(photo), "panel", 1, 2, 3, 4)
    summary = json.loads(call(iface, "ListProfiles"))[0]
    assert summary["avatar_mode"] == "panel"
    assert (summary["avatar_sx"], summary["avatar_side"], summary["avatar_sh"]) == (1, 3, 4)

    dest = tmp_path / "voice.zip"
    call(iface, "ExportProfile", pid, str(dest))
    new_pid = call(iface, "ImportProfile", str(dest))
    assert new_pid != pid
    names = {p["name"] for p in json.loads(call(iface, "ListProfiles"))}
    assert names == {"Nail", "Nail (2)"}  # UNIQUE name, de-duped on import


def test_compose_and_rewrite_need_a_personality(iface):
    """No personality, no LLM run — the call short-circuits to 0."""
    pid = call(iface, "CreateProfile", json.dumps({"name": "Plain", "voice_type": "cloned"}))
    assert call(iface, "ComposeProfile", pid, "say something") == 0
    assert call(iface, "RewriteProfile", pid, "some text") == 0
    assert call(iface, "ComposeProfile", "builtin:kokoro:af_heart", "hi") == 0
    assert call(iface, "RefineTranscript", "   ") == 0


def test_rewrite_needs_text_as_well_as_a_personality(iface):
    pid = call(iface, "CreateProfile", json.dumps(
        {"name": "Chatty", "voice_type": "cloned", "personality": "loud"}))
    assert call(iface, "RewriteProfile", pid, "   ") == 0


# --- effects surface -----------------------------------------------------


def test_effect_preset_crud_over_dbus(iface):
    chain = [{"type": "gain", "params": {"gain_db": 3.0}}]
    pid = call(iface, "CreateEffectPreset", "Mine", "notes", json.dumps(chain))
    assert pid
    assert json.loads(call(iface, "GetEffectPreset", pid))["name"] == "Mine"
    names = {p["name"] for p in json.loads(call(iface, "ListEffectPresets"))}
    assert {"Mine", "Robotic", "Radio"} <= names

    assert call(iface, "UpdateEffectPreset", pid, "Renamed", "", json.dumps(chain)) is True
    assert json.loads(call(iface, "GetEffectPreset", pid))["name"] == "Renamed"
    assert call(iface, "DeleteEffectPreset", pid) is True
    assert call(iface, "GetEffectPreset", pid) == ""


def test_effect_preset_methods_reject_non_json_chains(iface):
    assert call(iface, "CreateEffectPreset", "Mine", "", "{not json") == ""
    assert call(iface, "UpdateEffectPreset", "any", "Mine", "", "{not json") is False
    assert call(iface, "PreviewEffects", "any", "{not json") == 0


def test_list_effects_is_the_chain_editor_registry(iface):
    defs = json.loads(call(iface, "ListEffects"))
    assert {"gain", "reverb", "pitch_shift"} <= {e["id"] for e in defs}


def test_set_effect_only_accepts_known_presets(iface):
    call(iface, "SetEffect", "radio")
    assert iface._active_effect == "radio"
    call(iface, "SetEffect", "not-a-preset")
    assert iface._active_effect == ""


def test_set_style_is_stored_verbatim(iface):
    call(iface, "SetStyle", "Speak in an extremely angry tone")
    assert iface._active_style == "Speak in an extremely angry tone"
    call(iface, "SetStyle", "")
    assert iface._active_style == ""


def test_preview_effects_refuses_bad_chains_and_unknown_rows(iface):
    item = seed_clip(iface)
    assert call(iface, "PreviewEffects", item.id, json.dumps([{"type": "warp_drive"}])) == 0
    assert call(iface, "PreviewEffects", "nope", json.dumps([])) == 0


# --- playback control ----------------------------------------------------


def test_transport_controls_are_safe_with_nothing_playing(iface):
    call(iface, "PausePlayback")
    call(iface, "ResumePlayback")
    call(iface, "SeekPlayback", 0.5)
    call(iface, "Cancel", 999)
    assert iface._ctl is None


def test_transport_controls_drive_the_current_clip(iface):
    from syrinx_engine.service import _PlayCtl

    iface._ctl = ctl = _PlayCtl()
    call(iface, "PausePlayback")
    assert ctl.paused is True
    call(iface, "ResumePlayback")
    assert ctl.paused is False
    call(iface, "SeekPlayback", 0.25)
    assert ctl.seek == 0.25


def test_set_volume_is_clamped(iface):
    call(iface, "SetVolume", 0.4)
    assert iface._volume == pytest.approx(0.4)
    call(iface, "SetVolume", 5.0)
    assert iface._volume == 1.0
    call(iface, "SetVolume", -2.0)
    assert iface._volume == 0.0


# --- voice metadata routing ---------------------------------------------


def test_voice_meta_reads_the_engine_off_the_id_or_the_profile(iface):
    assert iface._voice_meta("builtin:kokoro:af_heart") == ("kokoro", "en")
    assert iface._voice_meta("no-such-profile") == ("", "en")

    preset = call(iface, "CreateProfile", json.dumps({
        "name": "Preset", "voice_type": "preset", "preset_engine": "kokoro",
        "preset_voice_id": "af_heart", "language": "fr"}))
    assert iface._voice_meta(preset) == ("kokoro", "fr")

    cloned = call(iface, "CreateProfile", json.dumps({
        "name": "Cloned", "voice_type": "cloned", "default_engine": "luxtts"}))
    assert iface._voice_meta(cloned) == ("luxtts", "en")


def test_voice_display_name_falls_back_to_the_id(iface):
    assert asyncio.run(iface._voice_display_name("no-such-profile")) == "no-such-profile"
    pid = call(iface, "CreateProfile", json.dumps({"name": "Named", "voice_type": "cloned"}))
    assert asyncio.run(iface._voice_display_name(pid)) == "Named"

"""The generation flows — Speak, ConvertVoice, playback, LLM, transcription.

These call the real service code end to end, with only the ML boundary
replaced: the TTS/STT/LLM/VC objects are swapped for fakes that return canned
PCM and text. Nothing loads a model, so what is actually under test is the
engine's own plumbing — history persistence, effect application, the playback
serialization, the error surfaces, and the recipe that Regenerate replays.
"""

import asyncio
import json
from pathlib import Path

import numpy as np
import pytest

from syrinx_engine import models
from syrinx_engine.backends import VoiceInfo
from syrinx_engine.core import EngineCore
from syrinx_engine.service import EngineInterface

RATE = 24_000


def tone(secs=0.2, amp=0.5):
    t = np.linspace(0, secs, int(secs * RATE), endpoint=False, dtype=np.float32)
    return (np.sin(2 * np.pi * 440 * t) * amp).astype(np.float32).tobytes()


# --- fakes for the ML boundary ------------------------------------------


class SpeechOnlyVC:
    """A conversion engine with no music pipeline (chatterbox_vc's shape)."""

    engine_name = "chatterbox_vc"

    def __init__(self):
        self.checked = []
        self.loaded = False
        self.stages = []

    def check_source(self, path):
        self.checked.append(path)

    async def load(self):
        self.loaded = True

    async def convert(self, path, prof):
        return tone(), RATE


class FakeVC(SpeechOnlyVC):
    engine_name = "seed_vc"

    async def convert_music(self, path, prof, on_stage=None, semitone=0):
        self.stages.append(semitone)
        if on_stage:
            on_stage("separating")
            on_stage("remixing")
        return tone(), RATE


class FakeTTS:
    backend = "cpu"
    clone_engine = "qwen"

    def __init__(self):
        self.calls = []
        self.vc = FakeVC()
        self.loaded = False
        self.invalidated = []
        self.fail = None
        self.delay = 0.0  # >0 keeps a generation in flight long enough to cancel

    async def load(self):
        self.loaded = True

    async def list_voices(self):
        return [VoiceInfo("builtin:kokoro:af_heart", "Heart")]

    async def synthesize(self, text, voice_id, instruct=""):
        self.calls.append((text, voice_id, instruct))
        if self.delay:
            await asyncio.sleep(self.delay)
        if self.fail:
            raise RuntimeError(self.fail)
        return tone(), RATE

    def vc_backend(self, engine=""):
        return self.vc

    def invalidate_profile(self, pid):
        self.invalidated.append(pid)

    def set_voice_engine(self, engine, size=""):
        pass


class FakeSTT:
    def __init__(self):
        self.text = "transcribed words"
        self.fail = False

    async def load(self):
        pass

    def set_model(self, ident):
        pass

    async def transcribe(self, path):
        return self.text

    async def transcribe_stream(self, path, on_partial=None):
        if self.fail:
            raise RuntimeError("whisper exploded")
        if on_partial:
            on_partial("partial")
        return self.text


class FakeLLM:
    def __init__(self):
        self.fail = False

    def set_model(self, size):
        pass

    async def _out(self, kind):
        if self.fail:
            raise RuntimeError("llm exploded")
        return f"{kind} output"

    async def compose(self, personality, prompt):
        return await self._out("compose")

    async def rewrite(self, personality, text):
        return await self._out("rewrite")

    async def refine(self, text):
        return await self._out("refine")


@pytest.fixture
def iface(fake_sd):
    e = EngineCore()
    e._tts = FakeTTS()
    e._stt = FakeSTT()
    e._llm = FakeLLM()
    return e


@pytest.fixture
def signals(iface):
    """Record every signal the core emits through its ``_emit`` seam."""
    seen = {}

    def emit(name, *args):
        seen.setdefault(name, []).append(args)

    iface._emit = emit
    return seen


def drive(iface, name, *args):
    """Call an engine method and wait out every task it spawned."""

    async def go():
        # the audio lock binds to whichever loop first awaits it, and every
        # asyncio.run() is a fresh loop — rebind before each drive
        iface._audio_lock = asyncio.Lock()
        fn = getattr(type(iface), name)
        out = await getattr(fn, "__wrapped__", fn)(iface, *args)
        pending = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        return out

    return asyncio.run(go())


def profile(iface, name="Piccolo", **over):
    spec = {"name": name, "voice_type": "cloned"}
    spec.update(over)
    pid = drive(iface, "CreateProfile", json.dumps(spec))
    return pid


# --- warmup / voices -----------------------------------------------------


def test_warmup_flips_model_loaded(iface):
    assert iface._model_loaded is False
    asyncio.run(iface.warmup())
    assert iface._model_loaded is True
    assert iface._tts.loaded is True


def test_list_voices_marshals_id_name_pairs(iface):
    assert drive(iface, "ListVoices") == [["builtin:kokoro:af_heart", "Heart"]]


# --- Speak ---------------------------------------------------------------


def test_speak_synthesizes_persists_and_plays(iface, signals, fake_sd):
    gen_id = drive(iface, "Speak", "hello world", "builtin:kokoro:af_heart")
    assert gen_id == 1
    assert iface._tts.calls == [("hello world", "builtin:kokoro:af_heart", "")]

    rows = json.loads(drive(iface, "ListHistory"))
    assert len(rows) == 1
    assert rows[0]["text"] == "hello world"
    assert rows[0]["voice_name"] == "Heart"  # resolved through list_voices
    assert rows[0]["engine"] == "kokoro"

    assert signals["SpeakStarted"] == [(gen_id,)]
    assert signals["SpeakEnded"] == [(gen_id,)]
    states = [s for _g, s, _p in signals["GenerationProgress"]]
    assert states == ["synthesizing", "playing"]
    clip_id, title, duration, bars = signals["PlaybackInfo"][0][1:]
    assert clip_id == rows[0]["id"] and title == "Heart"
    assert duration == pytest.approx(0.2)
    assert len(json.loads(bars)) == 300
    assert fake_sd.made[0].frames == int(0.2 * RATE)


def test_speak_uses_the_active_style_and_effect(iface, signals):
    drive(iface, "SetStyle", "Speak in an extremely angry tone")
    drive(iface, "SetEffect", "radio")
    drive(iface, "Speak", "hi", "builtin:kokoro:af_heart")
    assert iface._tts.calls[0][2] == "Speak in an extremely angry tone"
    assert "effects" in [s for _g, s, _p in signals["GenerationProgress"]]


def test_a_synthesis_failure_is_surfaced_as_a_progress_error(iface, signals):
    iface._tts.fail = "no model"
    drive(iface, "Speak", "hi", "builtin:kokoro:af_heart")
    errors = [s for _g, s, _p in signals["GenerationProgress"] if s.startswith("error:")]
    assert errors == ["error: no model"]
    assert json.loads(drive(iface, "ListHistory")) == []
    assert signals["SpeakEnded"]  # the generation still closes out


def test_speak_through_a_profile_records_its_engine_and_language(iface):
    pid = profile(iface, "Nail", language="fr", default_engine="luxtts")
    drive(iface, "Speak", "bonjour", pid)
    row = json.loads(drive(iface, "ListHistory"))[0]
    assert (row["voice_name"], row["engine"], row["language"]) == ("Nail", "luxtts", "fr")


def test_a_history_save_failure_does_not_sink_the_generation(iface, signals, monkeypatch):
    def boom(**_kw):
        raise OSError("disk full")

    monkeypatch.setattr(iface._history, "save_clip", boom)
    drive(iface, "Speak", "hi", "builtin:kokoro:af_heart")
    assert signals["PlaybackInfo"][0][1] == ""  # played with no clip id
    assert signals["SpeakEnded"]


# --- ConvertVoice --------------------------------------------------------


def cloned_with_sample(iface, make_wav, name="Piccolo"):
    pid = profile(iface, name)
    drive(iface, "AddSample", pid, str(make_wav("ref.wav")), "reference")
    return pid


def test_convert_voice_stores_a_replayable_recipe(iface, make_wav, signals):
    pid = cloned_with_sample(iface, make_wav)
    src = make_wav("src.wav", secs=1.0)
    gen_id = drive(iface, "ConvertVoice", str(src), pid, "seed_vc", "take 1",
                   "the spoken words", "speech", 0)
    assert gen_id > 0
    assert iface._tts.vc.loaded and iface._tts.vc.checked == [str(src)]

    row = json.loads(drive(iface, "ListHistory"))[0]
    assert row["voice_name"] == "Piccolo · take 1"
    assert row["text"] == "the spoken words"
    assert row["engine"] == "seed_vc"

    recipe = json.loads(iface._history.get(row["id"]).vc_json)
    st = src.stat()
    assert recipe["source"] == str(src)
    assert recipe["mode"] == "speech"
    assert (recipe["mtime"], recipe["size"]) == (int(st.st_mtime), st.st_size)


def test_music_mode_marks_the_row_and_forwards_the_worker_stages(iface, make_wav, signals):
    pid = cloned_with_sample(iface, make_wav)
    src = make_wav("song.wav", secs=1.0)
    drive(iface, "ConvertVoice", str(src), pid, "seed_vc", "", "", "music", -12)
    row = json.loads(drive(iface, "ListHistory"))[0]
    assert row["voice_name"] == "Piccolo ♫"
    assert row["text"].startswith("[voice conversion] ")
    assert iface._tts.vc.stages == [-12]
    states = [s for _g, s, _p in signals["GenerationProgress"]]
    assert "separating" in states and "remixing" in states


def test_convert_refuses_unknown_or_sampleless_profiles(iface, make_wav, signals):
    src = make_wav("src.wav", secs=0.5)
    drive(iface, "ConvertVoice", str(src), "nope", "", "", "", "speech", 0)
    bare = profile(iface, "Bare")
    drive(iface, "ConvertVoice", str(src), bare, "", "", "", "speech", 0)
    preset = profile(iface, "Preset", voice_type="preset")
    drive(iface, "ConvertVoice", str(src), preset, "", "", "", "speech", 0)
    errors = [s for _g, s, _p in signals["GenerationProgress"] if s.startswith("error:")]
    assert len(errors) == 3
    assert "unknown profile" in errors[0]
    assert "no reference samples" in errors[1]


def test_music_mode_on_an_engine_without_it_is_refused(iface, make_wav, signals):
    iface._tts.vc = SpeechOnlyVC()
    pid = cloned_with_sample(iface, make_wav)
    drive(iface, "ConvertVoice", str(make_wav("song.wav")), pid, "", "", "", "music", 0)
    errors = [s for _g, s, _p in signals["GenerationProgress"] if s.startswith("error:")]
    assert "does not support music mode" in errors[0]


# --- speech pitch fine-tune (pre-shift) ----------------------------------


class _RecordingVC(FakeVC):
    """Captures the exact source path (and its audio) that reaches convert /
    convert_music — so the pre-shift can be observed before its temp is swept."""

    def __init__(self):
        super().__init__()
        self.speech_paths = []
        self.speech_audio = []
        self.music_paths = []

    async def convert(self, path, prof):
        import soundfile as sf

        self.speech_paths.append(path)
        self.speech_audio.append(sf.read(path, dtype="float32"))  # while it exists
        return tone(), RATE

    async def convert_music(self, path, prof, on_stage=None, semitone=0):
        self.music_paths.append(path)
        return await super().convert_music(path, prof, on_stage, semitone)


def _median_hz(data, sr):
    import librosa
    import numpy as np

    f0, _v, _p = librosa.pyin(data, sr=sr, fmin=65.0, fmax=1000.0)
    vals = f0[np.isfinite(f0)]
    return float(np.median(vals))


def test_speech_semitones_preshift_hands_the_backend_a_shifted_source(iface, make_wav):
    import math

    vc = _RecordingVC()
    iface._tts.vc = vc
    pid = cloned_with_sample(iface, make_wav)
    src = make_wav("src.wav", secs=1.0, freq=150.0)
    drive(iface, "ConvertVoice", str(src), pid, "seed_vc", "", "", "speech", 4)

    assert len(vc.speech_paths) == 1
    assert vc.speech_paths[0] != str(src)  # a temp, not the original
    data, sr = vc.speech_audio[0]
    up = 12 * math.log2(_median_hz(data, sr) / 150.0)
    assert up == pytest.approx(4, abs=0.6)  # pitched ~+4 st


def test_speech_semitones_zero_passes_the_original_source_untouched(iface, make_wav):
    vc = _RecordingVC()
    iface._tts.vc = vc
    pid = cloned_with_sample(iface, make_wav)
    src = make_wav("src.wav", secs=1.0)
    drive(iface, "ConvertVoice", str(src), pid, "seed_vc", "", "", "speech", 0)
    # semitones==0 is byte-identical to today: no load/rewrite, original path
    assert vc.speech_paths == [str(src)]


def test_music_mode_never_preshifts_the_source(iface, make_wav):
    vc = _RecordingVC()
    iface._tts.vc = vc
    pid = cloned_with_sample(iface, make_wav)
    src = make_wav("song.wav", secs=1.0)
    drive(iface, "ConvertVoice", str(src), pid, "seed_vc", "", "", "music", 5)
    # music keeps the original path (the octave shift happens inside the worker)
    assert vc.music_paths == [str(src)]
    assert vc.stages == [5]


# --- SuggestPitchShift ---------------------------------------------------


def test_suggest_pitch_shift_measures_the_median_f0_gap(iface, make_wav):
    pid = profile(iface, "Piccolo")
    # reference voice ~440 Hz, clip an octave below ~220 Hz → shift the clip +12
    drive(iface, "AddSample", pid, str(make_wav("ref.wav", secs=1.0, freq=440.0)), "ref")
    clip = make_wav("clip.wav", secs=1.0, freq=220.0)
    assert drive(iface, "SuggestPitchShift", str(clip), pid) == 12
    # symmetric: a high clip vs a low reference suggests shifting down
    lo = profile(iface, "Bass")
    drive(iface, "AddSample", lo, str(make_wav("bass.wav", secs=1.0, freq=110.0)), "ref")
    assert drive(iface, "SuggestPitchShift", str(make_wav("hi.wav", secs=1.0, freq=220.0)), lo) == -12


def test_suggest_pitch_shift_refuses_preset_or_sampleless_profiles(iface, make_wav):
    clip = str(make_wav("clip.wav", secs=1.0, freq=220.0))
    with pytest.raises(ValueError, match="unknown profile"):
        drive(iface, "SuggestPitchShift", clip, "nope")
    bare = profile(iface, "Bare")
    with pytest.raises(ValueError, match="no reference samples"):
        drive(iface, "SuggestPitchShift", clip, bare)
    preset = profile(iface, "Preset", voice_type="preset")
    with pytest.raises(ValueError, match="no reference samples"):
        drive(iface, "SuggestPitchShift", clip, preset)


# --- Regenerate ----------------------------------------------------------


def test_regenerate_of_a_tts_row_respeaks_it(iface):
    drive(iface, "Speak", "say it again", "builtin:kokoro:af_heart")
    first = json.loads(drive(iface, "ListHistory"))[0]
    assert drive(iface, "RegenerateHistory", first["id"]) > 0
    assert len(json.loads(drive(iface, "ListHistory"))) == 2
    assert iface._tts.calls[-1][0] == "say it again"


def test_regenerate_of_a_conversion_row_reruns_the_conversion(iface, make_wav):
    pid = cloned_with_sample(iface, make_wav)
    src = make_wav("src.wav", secs=1.0)
    drive(iface, "ConvertVoice", str(src), pid, "seed_vc", "take 1", "words", "speech", 3)
    original = json.loads(drive(iface, "ListHistory"))[0]

    assert drive(iface, "RegenerateHistory", original["id"]) > 0
    rows = json.loads(drive(iface, "ListHistory"))
    assert len(rows) == 2
    # the recipe was replayed, not the transcript re-spoken
    assert all(r["engine"] == "seed_vc" for r in rows)
    assert iface._tts.calls == []


# --- transcription -------------------------------------------------------


def test_transcribe_returns_the_text(iface, make_wav):
    assert drive(iface, "Transcribe", str(make_wav("a.wav"))) == "transcribed words"


def test_transcribe_file_streams_partials_then_the_result(iface, signals, make_wav):
    req = drive(iface, "TranscribeFile", str(make_wav("a.wav")))
    assert signals["TranscribeProgress"] == [(req, "partial")]
    # error flag is False for a real (or legitimately-empty) transcript
    assert signals["TranscribeResult"] == [(req, "transcribed words", False)]


def test_a_transcription_failure_is_flagged_distinct_from_empty(iface, signals, make_wav):
    # a stt-stack exception surfaces as error=True (with text=""), so the app
    # can show "transcription failed" instead of the "no speech" copy
    iface._stt.fail = True
    req = drive(iface, "TranscribeFile", str(make_wav("a.wav")))
    assert signals["TranscribeResult"] == [(req, "", True)]


def test_a_legitimately_empty_transcript_is_not_flagged_an_error(iface, signals, make_wav):
    iface._stt.text = ""  # whisper heard silence — no exception
    req = drive(iface, "TranscribeFile", str(make_wav("a.wav")))
    assert signals["TranscribeResult"] == [(req, "", False)]


def test_add_sample_auto_transcribes_when_no_text_is_given(iface, make_wav):
    pid = profile(iface, "Nail")
    out = json.loads(drive(iface, "AddSample", pid, str(make_wav("ref.wav")), "   "))
    assert out["reference_text"] == "transcribed words"
    assert iface._tts.invalidated == [pid]


# --- personality LLM -----------------------------------------------------


def test_compose_rewrite_and_refine_deliver_via_llm_result(iface, signals):
    pid = profile(iface, "Chatty", personality="loud and rude")
    assert drive(iface, "ComposeProfile", pid, "a topic") == 1
    assert drive(iface, "RewriteProfile", pid, "some text") == 2
    assert drive(iface, "RefineTranscript", "um so like the thing") == 3
    assert signals["LlmResult"] == [
        (1, "compose output"), (2, "rewrite output"), (3, "refine output"),
    ]


def test_an_llm_failure_delivers_an_empty_string(iface, signals):
    iface._llm.fail = True
    pid = profile(iface, "Chatty", personality="loud")
    req = drive(iface, "ComposeProfile", pid, "a topic")
    assert signals["LlmResult"] == [(req, "")]


# --- playback surfaces ---------------------------------------------------


def test_play_history_replays_a_stored_clip(iface, signals, fake_sd):
    drive(iface, "Speak", "hello", "builtin:kokoro:af_heart")
    hid = json.loads(drive(iface, "ListHistory"))[0]["id"]
    fake_sd.made.clear()
    assert drive(iface, "PlayHistory", hid) > 0
    assert fake_sd.made[0].frames == int(0.2 * RATE)
    assert signals["PlaybackInfo"][-1][1] == hid


def test_play_history_at_starts_partway_in(iface, fake_sd):
    drive(iface, "Speak", "hello", "builtin:kokoro:af_heart")
    hid = json.loads(drive(iface, "ListHistory"))[0]["id"]
    fake_sd.made.clear()
    drive(iface, "PlayHistoryAt", hid, 0.5)
    assert fake_sd.made[0].frames == pytest.approx(int(0.1 * RATE), abs=1024)


def test_play_sample_auditions_a_profile_reference(iface, make_wav, fake_sd, signals):
    pid = profile(iface, "Nail")
    sid = json.loads(drive(iface, "AddSample", pid, str(make_wav("ref.wav", secs=0.5)), "x"))
    assert drive(iface, "PlaySample", sid["sample_id"]) > 0
    assert fake_sd.made[0].frames == int(0.5 * RATE)
    assert signals["PlaybackInfo"][-1][2] == "Sample"


def test_play_file_auditions_any_local_audio(iface, make_wav, fake_sd, signals):
    path = make_wav("loose.wav", secs=0.5)
    assert drive(iface, "PlayFile", str(path), "") > 0
    assert fake_sd.made[0].frames == int(0.5 * RATE)
    assert signals["PlaybackInfo"][-1][2] == "loose"  # falls back to the stem


def test_play_file_at_starts_partway_in(iface, make_wav, fake_sd):
    drive(iface, "PlayFileAt", str(make_wav("loose.wav", secs=0.5)), "T", 0.5)
    assert fake_sd.made[0].frames == pytest.approx(int(0.25 * RATE), abs=1024)


def test_preview_effects_plays_an_ad_hoc_chain_without_saving(iface, fake_sd, signals):
    drive(iface, "Speak", "hello", "builtin:kokoro:af_heart")
    hid = json.loads(drive(iface, "ListHistory"))[0]["id"]
    fake_sd.made.clear()
    chain = json.dumps([{"type": "gain", "params": {"gain_db": 0.0}}])
    assert drive(iface, "PreviewEffects", hid, chain) > 0
    assert fake_sd.made[0].frames > 0
    assert signals["PlaybackInfo"][-1][2].endswith("· preview")
    assert len(json.loads(drive(iface, "ListHistory"))) == 1  # nothing new


def test_cancel_stops_a_running_generation(iface, signals):
    """Cancel flags the task; the run() body swallows CancelledError so the
    generation still reports SpeakEnded."""

    iface._tts.delay = 5.0  # keep it in flight so the cancel actually lands

    async def go():
        iface._audio_lock = asyncio.Lock()
        gen_id = iface._start_speak("hi", "builtin:kokoro:af_heart")
        await asyncio.sleep(0)  # let run() reach the synthesize await
        iface.Cancel(gen_id)
        await asyncio.gather(*[t for t in asyncio.all_tasks()
                               if t is not asyncio.current_task()],
                             return_exceptions=True)
        return gen_id

    gen_id = asyncio.run(go())
    assert signals["SpeakEnded"] == [(gen_id,)]
    assert json.loads(drive(iface, "ListHistory")) == []


def test_a_second_playback_supersedes_the_first(iface, fake_sd):
    """Latest request wins: the older clip is asked to stop and its _play
    returns without claiming the device again."""

    async def go():
        iface._audio_lock = asyncio.Lock()
        pcm = tone(secs=1.0)
        first = asyncio.create_task(iface._play(1, pcm, RATE))
        await asyncio.sleep(0)
        second = asyncio.create_task(iface._play(2, pcm, RATE))
        await asyncio.gather(first, second)

    asyncio.run(go())
    assert len(fake_sd.made) >= 1


# --- model downloads -----------------------------------------------------


def test_download_model_reports_progress(iface, signals, monkeypatch):
    async def fake_download(model_id, on_progress):
        on_progress(model_id, 0.5, "downloading")
        on_progress(model_id, 1.0, "done")
        return True

    monkeypatch.setattr(iface._models, "download", fake_download)
    assert drive(iface, "DownloadModel", "kokoro") is True
    assert signals["ModelProgress"] == [("kokoro", 0.5, "downloading"),
                                        ("kokoro", 1.0, "done")]


def test_delete_model_removes_the_cached_repo(iface, hf_cache):
    d = hf_cache / "models--hexgrad--Kokoro-82M"
    (d / "blobs").mkdir(parents=True)
    drive(iface, "DeleteModel", "kokoro")
    assert not d.exists()


# --- signals themselves --------------------------------------------------


def test_every_signal_marshals_its_payload():
    """Signal bodies build the D-Bus argument list; emission is a no-op with
    no bus, so this checks the shapes the app unpacks."""
    e = EngineInterface()
    assert e.GenerationProgress(1, "synthesizing", 0.5) == [1, "synthesizing", 0.5]
    assert e.AudioLevel(1, 0.25) == [1, 0.25]
    assert e.PlaybackInfo(1, "cid", "Title", 2.0, "[]") == [1, "cid", "Title", 2.0, "[]"]
    assert e.PlaybackProgress(1, 0.5) == [1, 0.5]
    assert e.LlmResult(3, "text") == [3, "text"]
    assert e.TranscribeProgress(4, "partial") == [4, "partial"]
    assert e.TranscribeResult(4, "final", False) == [4, "final", False]
    assert e.ModelProgress("kokoro", 0.5, "downloading") == ["kokoro", 0.5, "downloading"]
    assert e.SpeakStarted(9) == 9
    assert e.SpeakEnded(9) == 9


# --- stereo sources ------------------------------------------------------


def test_trim_audio_downmixes_a_stereo_source(iface, tmp_path):
    import soundfile as sf

    path = tmp_path / "stereo.wav"
    n = 2 * RATE
    stereo = np.stack([np.linspace(-0.5, 0.5, n), np.linspace(0.5, -0.5, n)], axis=1)
    sf.write(str(path), stereo.astype(np.float32), RATE, subtype="PCM_16")
    out = drive(iface, "TrimAudio", str(path), 0.5, 1.5)
    assert out == str(path)
    env = json.loads(drive(iface, "FileEnvelope", out))
    assert env["duration"] == pytest.approx(1.0, abs=1e-3)


def test_models_status_is_reachable_through_the_interface(iface, monkeypatch):
    monkeypatch.setattr(models, "detect_hardware",
                        lambda: {"cores": 4, "ram_gb": 8.0, "gpu": False, "gpu_name": ""})
    rows = json.loads(drive(iface, "ListModels"))
    assert any(r["warning"] for r in rows)  # a weak box warns somewhere
    assert Path(iface._history._dir).exists()

"""Mic-capture recorder (seam 1.3 — RPC-PROTOCOL.md §14).

sounddevice is not in the CI dependency contract, so every test drives the
recorder against the ``fake_sd`` stub (a fake InputStream that feeds one silent
block on start) — the start/stop/cancel/latest-wins logic is engine code worth
pinning, the PortAudio boundary is not.
"""

import json
import os
import struct
import sys
import wave

import pytest

from syrinx_engine import recording
from syrinx_engine.recording import RecordingManager, list_devices


def _block(amp, frames=480):
    """One PCM16 mono block at a constant |amplitude| (0..1) — RMS == amp."""
    v = int(round(amp * 32768))
    return struct.pack("<h", v) * frames


def _read_wav(path):
    with wave.open(path, "rb") as w:
        return w.getnchannels(), w.getsampwidth(), w.getnframes()


def test_list_devices_reports_inputs_and_default(fake_sd):
    devs = list_devices()
    assert devs == [{"id": "Fake Mic", "name": "Fake Mic", "default": True}]


def test_list_devices_json_shape(fake_sd):
    mgr = RecordingManager()
    devs = json.loads(mgr.list_devices())
    assert devs[0]["id"] == "Fake Mic"


def test_list_devices_without_sounddevice(monkeypatch):
    # sounddevice genuinely absent (CI) or unimportable → "[]"
    monkeypatch.setitem(sys.modules, "sounddevice", None)
    assert list_devices() == []
    assert RecordingManager().list_devices() == "[]"


def test_start_stop_produces_finalizable_wav(fake_sd):
    mgr = RecordingManager()
    rid = mgr.start("")
    assert rid
    path = mgr.stop(rid)
    assert path.endswith(".wav")
    assert os.path.exists(path)
    ch, width, frames = _read_wav(path)
    assert (ch, width) == (1, 2)
    assert frames > 0  # the stub fed a silent block


def test_start_without_sounddevice_returns_empty(monkeypatch):
    monkeypatch.setitem(sys.modules, "sounddevice", None)
    assert RecordingManager().start("") == ""


def test_start_uses_named_device(fake_sd):
    mgr = RecordingManager()
    rid = mgr.start("Fake Mic")
    assert rid
    # exact names resolve to a concrete PortAudio index (never the raw string —
    # bare names are ambiguous when several host APIs list the device)
    assert fake_sd.in_made[-1].device == 0
    assert fake_sd.in_made[-1].samplerate == 48000  # device-native rate
    mgr.cancel(rid)


def test_named_device_resolves_across_host_apis(fake_sd):
    # The Windows shape: ONE physical mic listed under four host APIs with an
    # identical name. sounddevice's own string matching raises "Multiple input
    # devices found" here — the recorder must pick one index itself, and it
    # must be the WASAPI entry (full names, native rate).
    fake_sd.hostapis[:] = [
        {"name": "MME"}, {"name": "Windows DirectSound"},
        {"name": "Windows WASAPI"}, {"name": "Windows WDM-KS"},
    ]
    fake_sd.devs[:] = [
        {"name": "Headset Mic", "hostapi": 0, "max_input_channels": 1,
         "max_output_channels": 0, "default_samplerate": 44100.0},
        {"name": "Headset Mic", "hostapi": 1, "max_input_channels": 1,
         "max_output_channels": 0, "default_samplerate": 44100.0},
        {"name": "Headset Mic", "hostapi": 2, "max_input_channels": 2,
         "max_output_channels": 0, "default_samplerate": 48000.0},
        {"name": "Headset Mic", "hostapi": 3, "max_input_channels": 2,
         "max_output_channels": 0, "default_samplerate": 48000.0},
    ]
    mgr = RecordingManager()
    rid = mgr.start("Headset Mic")
    assert rid
    st = fake_sd.in_made[-1]
    assert st.device == 2          # the WASAPI entry
    assert st.samplerate == 48000  # rate read from the RESOLVED device
    mgr.cancel(rid)


def test_unresolvable_name_falls_through_to_portaudio(fake_sd):
    # A stale/renamed persisted name has no exact match — hand the raw string
    # to sounddevice so PortAudio's substring matching still gets a shot.
    mgr = RecordingManager()
    rid = mgr.start("Fake")  # substring of "Fake Mic", not an exact name
    assert rid
    assert fake_sd.in_made[-1].device == "Fake"
    mgr.cancel(rid)


def test_cancel_deletes_the_file(fake_sd):
    mgr = RecordingManager()
    rid = mgr.start("")
    path = mgr._current.path  # noqa: SLF001 — white-box: capture the scratch path
    assert path.exists()
    mgr.cancel(rid)
    assert not path.exists()


def test_unknown_id_semantics(fake_sd):
    mgr = RecordingManager()
    assert mgr.stop("nope") == ""
    assert mgr.cancel("nope") is None
    rid = mgr.start("")
    assert mgr.stop("wrong") == ""   # a live recording, wrong id
    assert mgr.stop(rid)             # correct id finalizes
    assert mgr.stop(rid) == ""       # already-stopped id


def test_on_level_reports_normalized_rms(fake_sd, monkeypatch):
    # throttle off: every block reports, so the arithmetic is what's under test
    monkeypatch.setattr(recording, "LEVEL_INTERVAL", 0.0)
    seen = []
    mgr = RecordingManager()
    rid = mgr.start("", on_level=lambda r, v: seen.append((r, v)))
    assert seen == [(rid, 0.0)]        # the stub's silent block on start
    fake_sd.in_made[-1].feed(_block(0.5))
    assert seen[-1][0] == rid
    assert seen[-1][1] == pytest.approx(0.5, abs=1e-4)  # int16/32768, not raw counts
    assert 0.0 < seen[-1][1] <= 1.0
    mgr.cancel(rid)


def test_on_level_is_throttled(fake_sd):
    # the real interval: PortAudio hands us a block every few ms, and one signal
    # per block would be a transport firehose for a ~15 Hz meter
    seen = []
    mgr = RecordingManager()
    rid = mgr.start("", on_level=lambda r, v: seen.append(v))
    st = fake_sd.in_made[-1]
    st.feed(_block(0.5))
    st.feed(_block(0.5))
    assert len(seen) == 1  # start's block reported; the two back-to-back did not
    mgr.cancel(rid)


def test_raising_on_level_does_not_break_the_recording(fake_sd):
    def boom(_rid, _rms):
        raise RuntimeError("meter exploded")

    mgr = RecordingManager()
    rid = mgr.start("", on_level=boom)
    assert rid                       # the raise happened inside start's block
    fake_sd.in_made[-1].feed(_block(0.25))
    path = mgr.stop(rid)
    assert os.path.exists(path)
    assert _read_wav(path)[2] > 0    # audio kept flowing to disk


def test_latest_wins_cancels_previous(fake_sd):
    mgr = RecordingManager()
    r1 = mgr.start("")
    p1 = mgr._current.path  # noqa: SLF001
    r2 = mgr.start("")       # supersedes r1
    assert r1 != r2
    assert not p1.exists()          # previous take deleted
    assert mgr.stop(r1) == ""       # superseded id is unknown now
    assert mgr.stop(r2)             # latest finalizes fine

"""Mic-capture recorder (seam 1.3 — RPC-PROTOCOL.md §14).

sounddevice is not in the CI dependency contract, so every test drives the
recorder against the ``fake_sd`` stub (a fake InputStream that feeds one silent
block on start) — the start/stop/cancel/latest-wins logic is engine code worth
pinning, the PortAudio boundary is not.
"""

import json
import os
import sys
import wave

from syrinx_engine.recording import RecordingManager, list_devices


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
    assert fake_sd.in_made[-1].device == "Fake Mic"
    assert fake_sd.in_made[-1].samplerate == 48000  # device-native rate
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


def test_latest_wins_cancels_previous(fake_sd):
    mgr = RecordingManager()
    r1 = mgr.start("")
    p1 = mgr._current.path  # noqa: SLF001
    r2 = mgr.start("")       # supersedes r1
    assert r1 != r2
    assert not p1.exists()          # previous take deleted
    assert mgr.stop(r1) == ""       # superseded id is unknown now
    assert mgr.stop(r2)             # latest finalizes fine

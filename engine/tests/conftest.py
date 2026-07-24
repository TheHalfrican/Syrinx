"""Shared fixtures — every test runs against a throwaway data dir.

The stores read $SYRINX_DATA_DIR at *construction* time, so the env has to be
redirected before anything is instantiated (hence autouse) and stores must be
built inside the test body, never at import.

Nothing here may import torch/kokoro/pedalboard/faster-whisper: the CI
contract is numpy + soundfile + dbus-next + pytest only.
"""

import math
import struct
import sys
import types
import wave

import numpy as np
import pytest

from syrinx_engine import models


@pytest.fixture(autouse=True)
def isolated_env(monkeypatch, tmp_path):
    """Point every on-disk location at tmp_path so tests can't see (or eat)
    the real ~/.local/share/syrinx or the real HF cache."""
    data = tmp_path / "data"
    data.mkdir()
    monkeypatch.setenv("SYRINX_DATA_DIR", str(data))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "config"))
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "cache"))
    # _hf_cache() falls back to ~/.cache/huggingface/hub when huggingface_hub
    # isn't installed (it isn't, in CI) — pin it so is_cached() can never walk
    # the developer's real multi-GB cache.
    cache = tmp_path / "hf-cache"
    cache.mkdir()
    monkeypatch.setattr(models, "_hf_cache", lambda: cache)
    return data


@pytest.fixture
def hf_cache(tmp_path):
    """The fake HF cache root that isolated_env pinned _hf_cache() to."""
    return tmp_path / "hf-cache"


class FakeStream:
    """Stands in for a PortAudio output stream — records what was written."""

    def __init__(self, samplerate, channels, dtype):
        self.samplerate = samplerate
        self.channels = channels
        self.written = []
        self.started = self.stopped = self.closed = False

    def start(self):
        self.started = True

    def write(self, block):
        self.written.append(np.asarray(block).reshape(-1).copy())

    def stop(self):
        self.stopped = True

    def close(self):
        self.closed = True

    @property
    def frames(self):
        return int(sum(len(b) for b in self.written))


class FakeInputStream:
    """Stands in for a PortAudio input stream — feeds one silent block on start
    so the recorder produces a finalizable WAV without any real device."""

    def __init__(self, samplerate, channels, dtype, device=None, callback=None):
        self.samplerate = samplerate
        self.channels = channels
        self.dtype = dtype
        self.device = device
        self.callback = callback
        self.started = self.stopped = self.closed = False

    def start(self):
        self.started = True
        if self.callback:
            # 480 frames of PCM16 mono silence (bytes; the real API hands a numpy
            # array, but the recorder just does bytes(indata))
            self.callback(b"\x00\x00" * 480, 480, None, None)

    def stop(self):
        self.stopped = True

    def close(self):
        self.closed = True


@pytest.fixture
def fake_sd(monkeypatch):
    """Install a fake ``sounddevice`` module — sounddevice is not in the CI
    dependency contract and a real stream needs a PipeWire sink, but the block
    loop around it is engine logic worth testing. ``.made`` lists output
    streams, ``.in_made`` the input (recording) streams."""
    made = []
    in_made = []

    def OutputStream(samplerate, channels, dtype):  # noqa: N802 — mirrors the real name
        made.append(FakeStream(samplerate, channels, dtype))
        return made[-1]

    def InputStream(samplerate, channels, dtype, device=None, callback=None):  # noqa: N802
        in_made.append(FakeInputStream(samplerate, channels, dtype, device, callback))
        return in_made[-1]

    _devs = [
        {"name": "Fake Mic", "max_input_channels": 2, "max_output_channels": 0,
         "default_samplerate": 48000.0},
        {"name": "Fake Speakers", "max_input_channels": 0, "max_output_channels": 2,
         "default_samplerate": 48000.0},
    ]

    def query_devices(device=None, kind=None):
        if kind == "input":
            return _devs[0]
        if device is None:
            return list(_devs)
        if isinstance(device, int):
            return _devs[device]
        for d in _devs:
            if d["name"] == device or device in d["name"]:
                return d
        raise ValueError(f"no device matching {device!r}")

    module = types.SimpleNamespace(
        OutputStream=OutputStream, made=made,
        InputStream=InputStream, in_made=in_made,
        query_devices=query_devices,
        default=types.SimpleNamespace(device=[0, 1]),
    )
    monkeypatch.setitem(sys.modules, "sounddevice", module)
    return module


@pytest.fixture
def make_wav(tmp_path):
    """Write a PCM16 mono sine wav and return its path."""

    def _make(name="tone.wav", secs=1.0, rate=24_000, freq=440.0, amp=0.5):
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        n = int(secs * rate)
        frames = b"".join(
            struct.pack("<h", int(amp * 32767 * math.sin(2 * math.pi * freq * i / rate)))
            for i in range(n)
        )
        with wave.open(str(path), "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(rate)
            w.writeframes(frames)
        return path

    return _make

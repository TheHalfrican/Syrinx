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

from syrinx_engine import audio, models


@pytest.fixture(autouse=True)
def no_live_streams(monkeypatch):
    """audio's live-stream count is module state — a test that leaves it dirty
    would silently disarm the PortAudio bounce for every test after it."""
    monkeypatch.setattr(audio, "_active_streams", 0)


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


# What FakeStream reports as its PortAudio latency — 20 ms, a plausible
# CoreAudio output figure. audio._drain sizes its trailing silence from it.
FAKE_LATENCY = 0.02


class FakeStream:
    """Stands in for a PortAudio output stream — records what was written."""

    def __init__(self, samplerate, channels, dtype):
        self.samplerate = samplerate
        self.channels = channels
        self.latency = FAKE_LATENCY
        self.written = []
        self.on_write = None  # test hook: called with the block, before recording it
        self.started = self.stopped = self.closed = False

    def start(self):
        self.started = True

    def write(self, block):
        if self.on_write is not None:
            self.on_write(block)
        self.written.append(np.asarray(block).reshape(-1).copy())

    def stop(self):
        self.stopped = True

    def close(self):
        self.closed = True

    @property
    def frames(self):
        return int(sum(len(b) for b in self.written))

    @property
    def tail_zeros(self):
        """Trailing all-zero frames — audio._drain's macOS blio padding."""
        n = 0
        for block in reversed(self.written):
            if np.any(block):
                break
            n += len(block)
        return n

    @property
    def voiced_frames(self):
        """Frames of the clip itself, i.e. everything before the drain."""
        return self.frames - self.tail_zeros


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

    def feed(self, data):
        """Push one more block into the callback (level-meter tests)."""
        if self.callback:
            self.callback(data, len(data) // 2, None, None)

    def stop(self):
        self.stopped = True

    def close(self):
        self.closed = True


class FakePortAudioError(Exception):
    """Mirrors sounddevice.PortAudioError — ``args`` is ``(message, PaErrorCode)``
    (plus host-error info on a paUnanticipatedHostError, which the engine's
    stale-topology check has to look past)."""


def pa_error(code=-9986, message="Error opening OutputStream: Internal PortAudio error"):
    """A PortAudio failure carrying ``code``, shaped exactly as sounddevice raises it."""
    return FakePortAudioError(f"{message} [PaErrorCode {code}]", code)


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
        {"name": "Fake Mic", "hostapi": 0, "max_input_channels": 2,
         "max_output_channels": 0, "default_samplerate": 48000.0},
        {"name": "Fake Speakers", "hostapi": 0, "max_input_channels": 0,
         "max_output_channels": 2, "default_samplerate": 48000.0},
    ]
    _hostapis = [{"name": "Fake API"}]

    def query_devices(device=None, kind=None):
        if kind == "input":
            return _devs[0]
        if device is None:
            return list(_devs)
        if isinstance(device, int):
            return _devs[device]
        found = [d for d in _devs if d["name"] == device or device in d["name"]]
        if len(found) > 1:
            # mirrors real sounddevice: a name listed under several host APIs
            # is ambiguous (the Windows shape recording.py must resolve around)
            raise ValueError(f"Multiple devices found for {device!r}")
        if found:
            return found[0]
        raise ValueError(f"no device matching {device!r}")

    def query_hostapis():
        return list(_hostapis)

    # sd._terminate()/_initialize() — the documented way to make PortAudio
    # re-read the device topology. ``on_bounce`` lets a test reshuffle .devs
    # the way a real driver install/uninstall would.
    bounces = []

    def _terminate():
        bounces.append("terminate")

    def _initialize():
        bounces.append("initialize")
        if module.on_bounce is not None:
            module.on_bounce()

    module = types.SimpleNamespace(
        OutputStream=OutputStream, made=made,
        InputStream=InputStream, in_made=in_made,
        query_devices=query_devices, query_hostapis=query_hostapis,
        devs=_devs, hostapis=_hostapis,
        default=types.SimpleNamespace(device=[0, 1]),
        PortAudioError=FakePortAudioError,
        _terminate=_terminate, _initialize=_initialize,
        bounces=bounces, on_bounce=None,
    )
    monkeypatch.setitem(sys.modules, "sounddevice", module)
    return module


@pytest.fixture
def make_video(tmp_path):
    """Encode a sine into an audio stream inside a video container — PyAV writes
    every fixture, so no binary media lives in the repo. ``audio=False`` gives a
    video-only file (a real container with no audio stream at all).

    ``codec``/``container`` follow the file's extension by default; both encoders
    used here (native aac, native mpeg4) are in every FFmpeg build, so the CI
    wheel and a local ffmpeg agree."""

    def _make(
        name="clip.mp4", secs=1.0, rate=44_100, freq=440.0, amp=0.5,
        channels=1, audio=True, codec="aac",
    ):
        import av

        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        out = av.open(str(path), "w")
        try:
            if not audio:
                vs = out.add_stream("mpeg4", rate=10)
                vs.width = vs.height = 32
                vs.pix_fmt = "yuv420p"
                blank = np.zeros((32, 32, 3), dtype=np.uint8)
                for _ in range(int(secs * 10)):
                    for pkt in vs.encode(av.VideoFrame.from_ndarray(blank, format="rgb24")):
                        out.mux(pkt)
                for pkt in vs.encode(None):
                    out.mux(pkt)
                return path
            layout = "mono" if channels == 1 else "stereo"
            st = out.add_stream(codec, rate=rate)
            st.layout = layout
            n = int(secs * rate)
            t = np.arange(n) / rate
            sig = (np.sin(2 * np.pi * freq * t) * amp * 32767).astype(np.int16)
            # packed s16: interleaved samples in one (1, n*channels) plane
            packed = np.repeat(sig, channels).reshape(1, -1)
            size = st.codec_context.frame_size or 1024
            for i in range(0, n, size):
                block = packed[:, i * channels : (i + size) * channels]
                frame = av.AudioFrame.from_ndarray(
                    np.ascontiguousarray(block), format="s16", layout=layout
                )
                frame.sample_rate = rate
                frame.pts = i
                for pkt in st.encode(frame):
                    out.mux(pkt)
            for pkt in st.encode(None):
                out.mux(pkt)
            return path
        finally:
            out.close()

    return _make


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

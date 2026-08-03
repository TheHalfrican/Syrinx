"""Mic capture for Windows/macOS (seam 1.3 — RPC-PROTOCOL.md §14).

A sounddevice (PortAudio) input stream writes PCM16 mono WAV into engine-owned
scratch space; the app consumes the returned path (AddSample / ConvertVoice /
TranscribeFile all take file paths). Linux never reaches here — the app keeps
its native ``parecord``/``pactl`` capture, so nothing in this module changes the
Linux build.

sounddevice is imported **lazily** inside the functions (engine-wide rule: no
heavy module-level imports; sounddevice is also absent from the CI dependency
contract, so the whole engine must import without it — the tests stub it).
"""

import json
import logging
import threading
import time
import uuid
import wave
from pathlib import Path

import numpy as np

from .audio import stream_closed, stream_opened, with_fresh_portaudio_retry
from .profiles import _data_dir

log = logging.getLogger("syrinx.engine.recording")

# Fallback capture rate when the device does not report a native one. 48 kHz is
# the WASAPI/CoreAudio default and downstream (whisper / VC workers) resamples.
DEFAULT_RATE = 48_000

# Minimum seconds between two RecordingLevel emissions (~15 Hz). PortAudio hands
# us a block every few ms at 48 kHz; a signal per block would be a D-Bus/RPC
# firehose for a meter the eye reads at video rate.
LEVEL_INTERVAL = 0.066

# Same-named PortAudio entries are tie-broken by host API in this order.
# Windows lists every physical device once per host API under an identical
# name, so a bare name is ambiguous to sounddevice's own string matching
# (ValueError: "Multiple input devices found ..."). WASAPI is the modern
# shared-mode API (full device names, native rates); MME truncates names to
# 31 chars; WDM-KS wants near-exclusive access. Single-host-API platforms
# (macOS Core Audio) never have a tie to break.
_HOSTAPI_PREFERENCE = ("Windows WASAPI", "Windows DirectSound", "MME", "Windows WDM-KS")


def _resolve_input(sd, name: str) -> "int | None":
    """Persisted device name → PortAudio index, or ``None`` when nothing
    matches exactly (the caller then hands the raw string to sounddevice, so
    near-miss names keep PortAudio's own substring matching)."""
    try:
        try:
            apis = list(sd.query_hostapis())
        except Exception:  # noqa: BLE001 — stubbed/exotic PortAudio builds
            apis = []
        matches = [
            (idx, d)
            for idx, d in enumerate(sd.query_devices())
            if int(d.get("max_input_channels", 0)) >= 1
            and str(d.get("name", "")).strip() == name
        ]
    except Exception:  # noqa: BLE001
        return None
    if not matches:
        return None

    def rank(match: "tuple[int, dict]") -> int:
        try:
            api = str(apis[int(match[1]["hostapi"])]["name"])
            return _HOSTAPI_PREFERENCE.index(api)
        except Exception:  # noqa: BLE001 — no hostapi info / unlisted API
            return len(_HOSTAPI_PREFERENCE)

    return min(matches, key=rank)[0]


def _scratch_dir() -> Path:
    """Engine-owned scratch for recordings — mirrors how history.py lays out its
    subdir under ``$SYRINX_DATA_DIR`` (the 1.4 seam owns the path helpers; this
    just uses the same local pattern)."""
    d = _data_dir() / "recordings"
    d.mkdir(parents=True, exist_ok=True)
    return d


def list_devices() -> "list[dict]":
    """Input devices as ``[{"id", "name", "default"}]`` (RPC-PROTOCOL.md §14).

    ``id`` is the device **name**, not the bare PortAudio index: indices
    reshuffle on hotplug, whereas the name is stable enough to persist in
    settings. Returns ``[]`` when sounddevice is unavailable or enumeration
    fails."""
    try:
        import sounddevice as sd
    except Exception:  # noqa: BLE001 — not installed / no PortAudio
        log.warning("ListRecordingDevices: sounddevice unavailable")
        return []
    try:
        devices = with_fresh_portaudio_retry(sd, sd.query_devices)
        try:
            default_in = sd.default.device[0]
        except Exception:  # noqa: BLE001
            default_in = -1
        out: "list[dict]" = []
        seen: "set[str]" = set()
        for idx, d in enumerate(devices):
            if int(d.get("max_input_channels", 0)) < 1:
                continue
            name = str(d.get("name", "")).strip()
            if not name or name in seen:
                # name-based ids must stay unique; a host API can list the same
                # device twice — first wins (both carry the same name anyway).
                continue
            seen.add(name)
            out.append({"id": name, "name": name, "default": idx == default_in})
        return out
    except Exception:  # noqa: BLE001
        log.exception("ListRecordingDevices: enumeration failed")
        return []


def _block_rms(indata) -> float:
    """Linear RMS of one captured PCM16 block, normalized to 0..1.

    ``indata`` is whatever PortAudio handed the callback (a numpy array for a
    real stream, raw bytes for the test stub) — ``bytes()`` flattens both. The
    int16 samples are widened to float32 first: squaring them in place would
    wrap at 32767."""
    samples = np.frombuffer(bytes(indata), dtype=np.int16)
    if samples.size == 0:
        return 0.0
    norm = samples.astype(np.float32) / 32768.0
    return min(1.0, float(np.sqrt(np.mean(np.square(norm)))))


class _Recording:
    """One live capture — the open WAV writer + its PortAudio stream."""

    def __init__(self, rec_id: str, path: Path, stream, wav) -> None:
        self.rec_id = rec_id
        self.path = path
        self._stream = stream
        self._wav = wav
        self._lock = threading.Lock()
        self._closed = False

    def write(self, data) -> None:
        with self._lock:
            if not self._closed:
                self._wav.writeframes(bytes(data))

    def finalize(self) -> None:
        """Stop the stream and close the WAV header. Idempotent."""
        with self._lock:
            if self._closed:
                return
            self._closed = True
        try:
            self._stream.stop()
            self._stream.close()
        except Exception:  # noqa: BLE001
            log.exception("recording %s: stream close failed", self.rec_id)
        stream_closed()
        try:
            self._wav.close()
        except Exception:  # noqa: BLE001
            log.exception("recording %s: wav close failed", self.rec_id)


class RecordingManager:
    """Owns at most one live recording — a second ``start`` cancels the previous
    (latest-wins, mirroring the playback epoch semantics)."""

    def __init__(self) -> None:
        self._current: "_Recording | None" = None
        self._lock = threading.Lock()

    def list_devices(self) -> str:
        return json.dumps(list_devices())

    def start(self, device_id: str, on_level=None) -> str:
        """Open an input stream to a fresh WAV; returns a recording id ("" on
        failure). ``device_id`` is a name (from :func:`list_devices`); "" =
        system default input.

        ``on_level(rec_id, rms)`` — optional; called with the linear RMS of each
        captured block normalized to 0..1 (int16 / 32768), throttled to one call
        per :data:`LEVEL_INTERVAL` seconds. It runs on the **PortAudio callback
        thread**, so the caller is responsible for hopping to wherever emission
        must happen (``core.StartRecording`` does the loop hop, exactly as
        ``audio.play`` does for ``AudioLevel``). A raising callback is swallowed:
        a broken meter must never kill the capture."""
        try:
            import sounddevice as sd
        except Exception:  # noqa: BLE001
            log.warning("StartRecording: sounddevice unavailable")
            return ""

        # latest-wins: drop any in-flight capture before starting a new one
        self._discard_current()

        rec_id = uuid.uuid4().hex
        path = _scratch_dir() / f"{rec_id}.wav"
        rec: "_Recording | None" = None
        # Last level emission, monotonic seconds. 0.0 = never, so the first
        # block always reports (the meter must move the instant capture opens).
        last_level = [0.0]

        def callback(indata, _frames, _time, status) -> None:
            if status:
                log.debug("recording %s: %s", rec_id, status)
            rec.write(indata)
            if on_level is None:
                return
            now = time.monotonic()
            if now - last_level[0] < LEVEL_INTERVAL:
                return
            last_level[0] = now
            try:
                on_level(rec_id, _block_rms(indata))
            except Exception:  # noqa: BLE001 — a broken meter must not stop capture
                log.exception("recording %s: level callback failed", rec_id)

        def open_stream() -> "tuple[object, int | str | None, int]":
            # Device and rate are re-resolved on EVERY attempt: a PortAudio
            # bounce renumbers indices, so one carried over from the previous
            # attempt would address a different device (or none).
            device: "int | str | None" = device_id or None
            if device is not None:
                resolved = _resolve_input(sd, device_id)
                if resolved is not None:
                    device = resolved
            rate = self._device_rate(sd, device)
            return (
                sd.InputStream(
                    samplerate=rate, channels=1, dtype="int16",
                    device=device, callback=callback,
                ),
                device,
                rate,
            )

        try:
            stream, device, rate = with_fresh_portaudio_retry(sd, open_stream)
        except Exception:  # noqa: BLE001 — device missing / busy / no PortAudio
            log.exception("StartRecording failed (device=%r)", device_id)
            return ""
        stream_opened()

        # The WAV takes the rate the stream actually opened at, and `rec` must
        # exist before start() — PortAudio calls back the moment it does.
        wav = wave.open(str(path), "wb")
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(rate)
        rec = _Recording(rec_id, path, stream, wav)

        try:
            stream.start()
        except Exception:  # noqa: BLE001 — device grabbed between open and start
            log.exception("StartRecording failed to start (device=%r)", device_id)
            rec.finalize()
            path.unlink(missing_ok=True)
            return ""

        with self._lock:
            self._current = rec
        log.info(
            "recording %s started (device=%r -> %r, %d Hz)", rec_id, device_id, device, rate
        )
        return rec_id

    def stop(self, rec_id: str) -> str:
        """Finalize and return the WAV path ("" for an unknown/already-stopped
        id)."""
        rec = self._take(rec_id)
        if rec is None:
            return ""
        rec.finalize()
        log.info("recording %s stopped -> %s", rec_id, rec.path)
        return str(rec.path)

    def cancel(self, rec_id: str) -> None:
        """Finalize and delete the WAV. Unknown id is a no-op."""
        rec = self._take(rec_id)
        if rec is None:
            return
        rec.finalize()
        rec.path.unlink(missing_ok=True)
        log.info("recording %s cancelled", rec_id)

    # --- internals --------------------------------------------------------

    @staticmethod
    def _device_rate(sd, device) -> int:
        try:
            info = sd.query_devices(kind="input") if device is None else sd.query_devices(device)
            return int(info.get("default_samplerate") or DEFAULT_RATE)
        except Exception:  # noqa: BLE001
            return DEFAULT_RATE

    def _take(self, rec_id: str) -> "_Recording | None":
        with self._lock:
            rec = self._current
            if rec is not None and rec.rec_id == rec_id:
                self._current = None
                return rec
        return None

    def _discard_current(self) -> None:
        with self._lock:
            rec = self._current
            self._current = None
        if rec is not None:
            rec.finalize()
            rec.path.unlink(missing_ok=True)
            log.info("recording %s superseded (latest-wins)", rec.rec_id)

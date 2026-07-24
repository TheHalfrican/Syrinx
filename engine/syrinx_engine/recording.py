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
import uuid
import wave
from pathlib import Path

from .profiles import _data_dir

log = logging.getLogger("syrinx.engine.recording")

# Fallback capture rate when the device does not report a native one. 48 kHz is
# the WASAPI/CoreAudio default and downstream (whisper / VC workers) resamples.
DEFAULT_RATE = 48_000


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
        devices = sd.query_devices()
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

    def start(self, device_id: str) -> str:
        """Open an input stream to a fresh WAV; returns a recording id ("" on
        failure). ``device_id`` is a name (from :func:`list_devices`); "" =
        system default input."""
        try:
            import sounddevice as sd
        except Exception:  # noqa: BLE001
            log.warning("StartRecording: sounddevice unavailable")
            return ""

        # latest-wins: drop any in-flight capture before starting a new one
        self._discard_current()

        device = device_id or None
        rate = self._device_rate(sd, device)
        rec_id = uuid.uuid4().hex
        path = _scratch_dir() / f"{rec_id}.wav"
        wav = wave.open(str(path), "wb")
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(rate)

        rec = _Recording(rec_id, path, None, wav)

        def callback(indata, _frames, _time, status) -> None:
            if status:
                log.debug("recording %s: %s", rec_id, status)
            rec.write(indata)

        try:
            stream = sd.InputStream(
                samplerate=rate, channels=1, dtype="int16",
                device=device, callback=callback,
            )
            stream.start()
        except Exception:  # noqa: BLE001 — device missing / busy / no PortAudio
            log.exception("StartRecording failed (device=%r)", device_id)
            try:
                wav.close()
            except Exception:  # noqa: BLE001
                pass
            path.unlink(missing_ok=True)
            return ""

        rec._stream = stream
        with self._lock:
            self._current = rec
        log.info("recording %s started (device=%r, %d Hz)", rec_id, device_id, rate)
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

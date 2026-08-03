"""Audio playback via PipeWire.

CachyOS is PipeWire-native, so `sounddevice` (PortAudio) routes straight to it —
no GStreamer sinks. We stream the PCM block-by-block so we can emit a live RMS
level for the UI/pill waveform and stay cancellable between blocks.
"""

import asyncio
import logging
import math
import threading
from typing import Callable, Optional, TypeVar

import numpy as np

log = logging.getLogger("syrinx.engine.audio")

_BLOCK = 1024  # frames per write ≈ 43 ms at 24 kHz

_T = TypeVar("_T")

# Ceiling on the tail drain (see :func:`_drain`). Sized from measurement, not
# taste: sounddevice asks PortAudio for its "high" latency, and the M3's
# default output — Bluetooth earbuds — reports 1.082 s, so a 1 s cap would clip
# the very tail this drains. It only bites a host reporting nonsense; a cancel
# mid-drain is honoured between blocks, not by this bound.
_MAX_DRAIN_SECS = 2.0

# PortAudio snapshots the device topology at Pa_Initialize. When a device
# appears or disappears afterwards — a driver uninstalled under a running
# engine (BlackHole, 2026-08-03), a USB interface unplugged, Bluetooth earbuds
# dropping — its cached device list and default-device references go stale and
# EVERY open fails with one of these until PortAudio is re-initialized:
_STALE_TOPOLOGY_CODES = frozenset(
    {
        -9986,  # paInternalError — what PaMacCore/AUHAL reports for a stale device
        -9985,  # paDeviceUnavailable
        -9996,  # paInvalidDevice
    }
)
# Format/parameter codes are deliberately absent: an unsupported sample rate or
# channel count fails identically after a bounce, so retrying only hides it.

_stream_lock = threading.Lock()
_active_streams = 0


def stream_opened() -> None:
    """Register a PortAudio stream this engine now holds open.

    :func:`_bounce_portaudio` terminates PortAudio, which yanks every live
    stream out from under its owner, so it must never run while the count is
    non-zero."""
    global _active_streams
    with _stream_lock:
        _active_streams += 1


def stream_closed() -> None:
    """Retire a stream registered by :func:`stream_opened`."""
    global _active_streams
    with _stream_lock:
        _active_streams = max(0, _active_streams - 1)


def _is_stale_topology(exc: BaseException) -> bool:
    """True when ``exc`` is PortAudio complaining about a device list that no
    longer matches reality.

    ``PortAudioError.args`` is ``(message, PaErrorCode[, host-error info])``,
    so the code is read out of the args rather than parsed from the string."""
    return any(a in _STALE_TOPOLOGY_CODES for a in getattr(exc, "args", ()) if isinstance(a, int))


def _bounce_portaudio(sd) -> bool:
    """Re-initialize PortAudio so it re-reads the device topology.

    Returns False when the bounce did not happen — either the engine holds a
    live stream (terminating would kill it; a stale open alongside a working
    stream is rare and self-resolves on the next attempt) or PortAudio refused
    to cycle."""
    with _stream_lock:
        if _active_streams:
            log.warning(
                "audio open failed on a stale device list, but %d stream(s) are live; "
                "not reinitializing PortAudio",
                _active_streams,
            )
            return False
        try:
            sd._terminate()
            sd._initialize()
        except Exception:  # noqa: BLE001
            log.exception("PortAudio reinitialization failed")
            return False
    log.info("audio device topology changed; reinitialized PortAudio")
    return True


def with_fresh_portaudio_retry(sd, open_fn: Callable[[], _T]) -> _T:
    """Run ``open_fn``, retrying it once against a re-initialized PortAudio.

    ``open_fn`` must re-resolve everything it needs on each call: device
    **indices do not survive the bounce** (names do). A second failure raises
    the FIRST exception unchanged, so callers keep surfacing the error the
    device actually reported."""
    try:
        return open_fn()
    except Exception as exc:  # noqa: BLE001
        first = exc
        if not _is_stale_topology(first) or not _bounce_portaudio(sd):
            raise
    try:
        return open_fn()
    except Exception:  # noqa: BLE001
        raise first


def duration_of(pcm: bytes, sample_rate: int) -> float:
    """Seconds of float32 mono PCM (4 bytes/sample)."""
    return (len(pcm) // 4) / sample_rate if sample_rate else 0.0


def envelope(pcm: bytes, n: int = 300) -> list:
    """Downsample PCM to ``n`` peak-amplitude bars (0..1) for a static waveform."""
    samples = np.frombuffer(pcm, dtype=np.float32)
    if samples.size == 0:
        return [0.0] * n
    edges = np.linspace(0, samples.size, n + 1).astype(int)
    bars = [
        float(np.abs(samples[edges[k] : edges[k + 1]]).max()) if edges[k + 1] > edges[k] else 0.0
        for k in range(n)
    ]
    peak = max(bars)
    return [b / peak for b in bars] if peak > 0 else bars


def _drain(stream, sample_rate: int, ctl) -> None:
    """Push silence behind the clip so the queued tail actually plays.

    PaMacCore's blocking-I/O layer DISCARDS whatever is still in its ring
    buffer on ``Pa_StopStream`` instead of playing it out, so the last
    latency-worth of audio — a few hundred ms — never reaches the speaker
    ("the last half of the last word is cut off"). A blocking ``write`` only
    returns once the audio ahead of it has been consumed, so writing the
    stream's own latency in zeros IS the drain. Windows/Linux hosts drain
    themselves and silence is inaudible there, so this is not cfg-gated.

    It costs no wall-clock time beyond the audio itself: a write returns as
    soon as there is ROOM for it, so pushing one buffer of silence blocks only
    for however much real audio was still queued. ``ctl.stop`` is honoured
    between chunks — a cancel during the drain must cut immediately — and the
    total is bounded by :data:`_MAX_DRAIN_SECS`.
    """
    pad = min(
        int(math.ceil(getattr(stream, "latency", 0.0) * sample_rate)) + _BLOCK,
        int(_MAX_DRAIN_SECS * sample_rate),
    )
    silence = np.zeros((_BLOCK, 1), dtype=np.float32)
    while pad > 0 and not ctl.stop:
        n = min(_BLOCK, pad)
        stream.write(silence[:n])
        pad -= n


def _play_blocking(
    data: np.ndarray,
    sample_rate: int,
    ctl,
    emit_level,
    emit_progress,
    volume,
) -> None:
    """The whole playback loop, in ONE worker thread that owns the stream.

    The stream must only ever be touched from this thread: closing a
    PortAudio stream from another thread while ``write()`` is blocked
    inside ALSA is a use-after-free (it segfaulted the engine mid-cancel,
    and once corrupted the heap into a delayed SIGABRT). Stop/pause/seek
    arrive via ``ctl`` flags checked between blocks.
    """
    import time

    import sounddevice as sd

    total = max(1, len(data))
    try:
        stream = with_fresh_portaudio_retry(
            sd,
            lambda: sd.OutputStream(samplerate=sample_rate, channels=1, dtype="float32"),
        )
    except Exception as exc:  # noqa: BLE001
        log.warning("could not open output stream (%s); skipping playback", exc)
        return

    stream_opened()
    stream.start()
    i = 0
    try:
        while i < len(data):
            if ctl.stop:
                break
            if ctl.seek is not None:
                i = min(len(data), max(0, int(ctl.seek * len(data))))
                ctl.seek = None
            if ctl.paused:
                time.sleep(0.05)
                continue
            block = data[i : i + _BLOCK]
            # Read the gain per block so a volume change applies mid-clip.
            gain = np.float32(max(0.0, min(1.0, volume()))) if volume is not None else None
            out = block if gain is None or gain == 1.0 else block * gain
            stream.write(out.reshape(-1, 1))
            if emit_level is not None and len(block):
                emit_level(float(np.sqrt(np.mean(np.square(block)))))
            if emit_progress is not None:
                emit_progress(min(1.0, (i + len(block)) / total))
            i += _BLOCK
        if not ctl.stop:
            _drain(stream, sample_rate, ctl)
    finally:
        # Teardown in the owning thread — always runs, never raced.
        try:
            stream.stop()
        except Exception:  # noqa: BLE001
            pass
        try:
            stream.close()
        except Exception:  # noqa: BLE001
            pass
        stream_closed()


async def play(
    pcm: bytes,
    sample_rate: int,
    ctl=None,
    on_level: Optional[Callable[[float], None]] = None,
    on_progress: Optional[Callable[[float], None]] = None,
    volume: Optional[Callable[[], float]] = None,
) -> None:
    """Play float32 PCM cooperatively.

    ``ctl`` has ``stop`` / ``paused`` bool attrs and a ``seek`` (0..1 or
    None) checked between blocks. Task cancellation never touches the
    stream — it sets ``ctl.stop`` and drains the playback thread, which
    tears the stream down itself.
    """
    if not pcm:
        return

    try:
        import sounddevice  # noqa: F401
    except Exception as exc:  # noqa: BLE001
        log.warning("audio unavailable (%s); skipping playback", exc)
        return

    data = np.frombuffer(pcm, dtype=np.float32)
    loop = asyncio.get_running_loop()
    if ctl is None:
        ctl = type("Ctl", (), {"stop": False, "paused": False, "seek": None})()

    def _emit(cb):
        # signal emission must happen on the event-loop thread
        def send(v):
            try:
                loop.call_soon_threadsafe(cb, v)
            except RuntimeError:
                pass  # loop already closed (engine shutdown)

        return send

    fut = loop.run_in_executor(
        None,
        _play_blocking,
        data,
        sample_rate,
        ctl,
        _emit(on_level) if on_level is not None else None,
        _emit(on_progress) if on_progress is not None else None,
        volume,
    )
    try:
        await fut
    except asyncio.CancelledError:
        ctl.stop = True
        # Hold the caller (and its audio lock) until the thread has closed
        # the stream — at most one block write plus teardown away.
        try:
            await asyncio.wait_for(asyncio.shield(fut), timeout=2.0)
        except Exception:  # noqa: BLE001
            pass
        raise

"""audio.play — the block loop, driven against a fake PortAudio stream.

sounddevice isn't part of the CI dependency contract (and a real stream needs
a PipeWire sink), so a stand-in module goes into sys.modules: the loop, the
stop/pause/seek controls, the volume ramp and the level/progress callbacks are
all engine logic and get tested for real.
"""

import asyncio
import math
import sys
import types

import numpy as np
import pytest

from syrinx_engine import audio

from conftest import FAKE_LATENCY, pa_error


@pytest.fixture
def sd(fake_sd):
    return fake_sd


class Ctl:
    def __init__(self, stop=False, paused=False, seek=None):
        self.stop = stop
        self.paused = paused
        self.seek = seek


def pcm(n, val=0.5):
    return np.full(n, val, dtype=np.float32).tobytes()


def drain(rate=24_000, latency=FAKE_LATENCY):
    """Frames of silence audio._drain appends after a clip that ran to its end."""
    return min(int(math.ceil(latency * rate)) + audio._BLOCK,
               int(audio._MAX_DRAIN_SECS * rate))


def voiced(stream):
    """The clip's own samples, with the drain padding sliced off."""
    return np.concatenate(stream.written)[: stream.voiced_frames]


# --- the ordinary path ---------------------------------------------------


def test_play_writes_every_frame_and_reports_level_and_progress(sd):
    levels, progress = [], []
    asyncio.run(audio.play(pcm(4096), 24_000,
                           on_level=levels.append, on_progress=progress.append))
    stream = sd.made[0]
    assert stream.started and stream.stopped and stream.closed
    assert stream.voiced_frames == 4096
    assert progress[-1] == pytest.approx(1.0)
    assert all(0.0 <= p <= 1.0 for p in progress)
    assert levels and all(abs(v - 0.5) < 1e-5 for v in levels)  # RMS of a DC 0.5 block


def test_empty_pcm_never_opens_a_stream(sd):
    asyncio.run(audio.play(b"", 24_000))
    assert sd.made == []


def test_volume_is_applied_per_block(sd):
    asyncio.run(audio.play(pcm(2048), 24_000, volume=lambda: 0.25))
    assert np.allclose(voiced(sd.made[0]), 0.125)


def test_volume_out_of_range_is_clamped(sd):
    asyncio.run(audio.play(pcm(1024), 24_000, volume=lambda: 9.0))
    assert np.allclose(voiced(sd.made[0]), 0.5)
    asyncio.run(audio.play(pcm(1024), 24_000, volume=lambda: -3.0))
    assert np.allclose(np.concatenate(sd.made[1].written), 0.0)


# --- the controls --------------------------------------------------------


def test_stop_ends_playback_early_but_still_tears_the_stream_down(sd):
    ctl = Ctl(stop=True)
    asyncio.run(audio.play(pcm(48_000), 24_000, ctl))
    stream = sd.made[0]
    assert stream.frames == 0  # not even the tail drain — stop means stop
    assert stream.stopped and stream.closed


def test_a_pause_holds_the_loop_until_it_is_released(sd):
    class Paused(Ctl):
        reads = 0

        @property
        def paused(self):
            # unpause after a few polls — the loop must not have written yet
            Paused.reads += 1
            return Paused.reads <= 3

        @paused.setter
        def paused(self, _v):
            pass

    asyncio.run(audio.play(pcm(2048), 24_000, Paused()))
    assert Paused.reads > 3
    assert sd.made[0].voiced_frames == 2048


def test_seek_jumps_the_read_position(sd):
    ctl = Ctl(seek=0.5)
    asyncio.run(audio.play(pcm(4096), 24_000, ctl))
    assert sd.made[0].voiced_frames == 2048  # started halfway in
    assert ctl.seek is None  # consumed, not re-applied every block


def test_a_stream_that_will_not_open_is_a_warning_not_a_crash(monkeypatch):
    def boom(**_kw):
        raise RuntimeError("no device")

    monkeypatch.setitem(sys.modules, "sounddevice",
                        types.SimpleNamespace(OutputStream=boom))
    asyncio.run(audio.play(pcm(1024), 24_000))  # returns quietly


def test_no_sounddevice_at_all_skips_playback(monkeypatch):
    monkeypatch.setitem(sys.modules, "sounddevice", None)
    asyncio.run(audio.play(pcm(1024), 24_000))


# --- cancellation --------------------------------------------------------


def test_cancelling_stops_the_thread_instead_of_closing_the_stream_under_it(sd):
    """Cancel must flag ctl and drain the worker — closing a PortAudio stream
    from another thread while write() is blocked is a use-after-free."""

    async def go():
        ctl = Ctl()
        task = asyncio.create_task(audio.play(pcm(24_000 * 20), 24_000, ctl))
        await asyncio.sleep(0)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        return ctl

    ctl = asyncio.run(go())
    assert ctl.stop is True
    stream = sd.made[0]
    assert stream.stopped and stream.closed  # torn down by its owning thread


def test_play_without_a_ctl_makes_its_own(sd):
    asyncio.run(audio.play(pcm(1024), 24_000))
    assert sd.made[0].voiced_frames == 1024


# --- the tail drain (PaMacCore blio discards its ring buffer on stop) -----


def test_a_clip_that_ends_naturally_is_followed_by_a_latency_sized_drain(sd):
    asyncio.run(audio.play(pcm(4096), 24_000))
    stream = sd.made[0]
    assert stream.tail_zeros == drain()          # latency + one block of margin
    assert stream.frames == 4096 + drain()
    # written through the same blocking path, in the same (frames, 1) shape
    assert stream.written[-1].dtype == np.float32


def test_the_drain_is_bounded_when_a_host_reports_nonsense(sd):
    real_make = sd.OutputStream

    def wide_latency(**kw):
        stream = real_make(**kw)
        stream.latency = 30.0
        return stream

    sd.OutputStream = wide_latency
    asyncio.run(audio.play(pcm(1024), 24_000))
    assert sd.made[0].tail_zeros == 2 * 24_000  # _MAX_DRAIN_SECS, not 30


def test_a_bluetooth_sized_latency_is_drained_in_full(sd):
    # the M3's default output measures 1.082 s — the case a 1 s cap would clip
    real_make = sd.OutputStream

    def bluetooth(**kw):
        stream = real_make(**kw)
        stream.latency = 1.082375
        return stream

    sd.OutputStream = bluetooth
    asyncio.run(audio.play(pcm(1024), 24_000))
    assert sd.made[0].tail_zeros == drain(latency=1.082375)
    assert sd.made[0].tail_zeros > 24_000


def test_a_stop_during_the_drain_cuts_it_short(sd):
    ctl = Ctl()
    real_make = sd.OutputStream

    def stopping(**kw):
        stream = real_make(**kw)

        def hook(block):
            # first all-zero block = the drain started; cancel mid-drain
            if not np.any(np.asarray(block)):
                ctl.stop = True

        stream.on_write = hook
        return stream

    sd.OutputStream = stopping
    asyncio.run(audio.play(pcm(1024), 24_000, ctl))
    stream = sd.made[0]
    assert stream.voiced_frames == 1024
    assert 0 < stream.tail_zeros < drain()  # one chunk in, then the flag won


# --- stale PortAudio topology --------------------------------------------
#
# PortAudio caches the device list at Pa_Initialize. A driver uninstalled (or a
# USB/Bluetooth device dropping) under a running engine leaves that cache
# lying, and every open fails until PortAudio is bounced — the 2026-08-03
# BlackHole incident, where playback said "skipping playback" forever.


def opens_failing(sd, *errors):
    """Make ``sd.OutputStream`` raise ``errors`` in turn before succeeding.
    Returns the list of attempts (one entry per call)."""
    real_make = sd.OutputStream
    attempts = []

    def flaky(**kw):
        attempts.append(kw)
        if len(attempts) <= len(errors):
            raise errors[len(attempts) - 1]
        return real_make(**kw)

    sd.OutputStream = flaky
    return attempts


@pytest.mark.parametrize("code", sorted(audio._STALE_TOPOLOGY_CODES))
def test_a_stale_topology_error_bounces_portaudio_and_retries_once(sd, code):
    attempts = opens_failing(sd, pa_error(code))
    asyncio.run(audio.play(pcm(1024), 24_000))
    assert len(attempts) == 2
    assert sd.bounces == ["terminate", "initialize"]
    assert sd.made[0].voiced_frames == 1024  # the retry played the whole clip


def test_a_second_failure_raises_the_first_error_unchanged(sd):
    first, second = pa_error(-9986), pa_error(-9985, "and now something else")
    opens_failing(sd, first, second)
    with pytest.raises(Exception) as caught:  # noqa: PT011 — identity is the assertion
        audio.with_fresh_portaudio_retry(sd, lambda: sd.OutputStream(samplerate=1, channels=1,
                                                                     dtype="float32"))
    assert caught.value is first  # callers keep surfacing what the device said


def test_a_retry_that_fails_again_still_only_warns_and_skips(sd, caplog):
    attempts = opens_failing(sd, pa_error(), pa_error())
    asyncio.run(audio.play(pcm(1024), 24_000))
    assert len(attempts) == 2          # exactly once more, never a loop
    assert sd.made == []
    assert "skipping playback" in caplog.text


def test_a_format_error_is_never_retried(sd):
    # -9997 = paInvalidSampleRate: a bounce cannot fix it, and retrying would
    # only hide a real parameter bug behind a device story
    attempts = opens_failing(sd, pa_error(-9997, "Invalid sample rate"))
    asyncio.run(audio.play(pcm(1024), 24_000))
    assert len(attempts) == 1
    assert sd.bounces == []


def test_a_plain_exception_is_never_retried(sd):
    attempts = opens_failing(sd, RuntimeError("no device"))
    asyncio.run(audio.play(pcm(1024), 24_000))
    assert len(attempts) == 1
    assert sd.bounces == []


def test_portaudio_is_not_bounced_while_a_stream_is_live(sd, caplog):
    # _terminate() yanks every open stream out from under its owner, so a
    # stale open next to a live recording must just fail (and self-resolve on
    # the next attempt).
    audio.stream_opened()
    try:
        attempts = opens_failing(sd, pa_error())
        asyncio.run(audio.play(pcm(1024), 24_000))
    finally:
        audio.stream_closed()
    assert len(attempts) == 1
    assert sd.bounces == []
    assert "stream(s) are live" in caplog.text


def test_a_bounce_that_itself_fails_is_not_fatal(sd):
    def no_terminate():
        raise RuntimeError("Pa_Terminate: still in use")

    sd._terminate = no_terminate
    attempts = opens_failing(sd, pa_error())
    asyncio.run(audio.play(pcm(1024), 24_000))
    assert len(attempts) == 1  # no retry against a PortAudio that did not cycle


def test_playback_registers_its_stream_while_it_runs(sd):
    seen = []

    def peek():
        seen.append(audio._active_streams)
        return 1.0

    asyncio.run(audio.play(pcm(2048), 24_000, volume=peek))
    assert seen == [1, 1]              # counted for the whole clip
    assert audio._active_streams == 0  # and released by the owning thread

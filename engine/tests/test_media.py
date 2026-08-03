"""Video-container ingest — extension detection, extraction, and the cache.

The fixtures are encoded by PyAV in ``make_video`` (conftest), so the assertions
below are against AAC, which is **lossy**: the encoder adds priming samples at
the head and pads the tail, so a round-tripped clip runs ~20-40 ms long and the
waveform is not sample-identical. Tests therefore assert on duration slack,
FFT peak and rms rather than on samples.
"""

import asyncio
from pathlib import Path

import numpy as np
import pytest
import soundfile as sf

from syrinx_engine import media

pytest.importorskip("av")


def read(path):
    data, rate = sf.read(str(path), dtype="float32", always_2d=True)
    return data, rate


def peak_hz(data, rate):
    """Dominant frequency of channel 0."""
    mono = data[:, 0]
    spec = np.abs(np.fft.rfft(mono * np.hanning(len(mono))))
    return float(np.fft.rfftfreq(len(mono), 1.0 / rate)[int(np.argmax(spec))])


# --- detection ------------------------------------------------------------


@pytest.mark.parametrize(
    "name", ["a.mp4", "a.mov", "a.mkv", "a.webm", "a.avi", "a.m4v", "A.MP4", "a.MoV"]
)
def test_video_extensions_are_detected_case_insensitively(name):
    assert media.is_video(name) is True


@pytest.mark.parametrize(
    "name", ["a.wav", "a.flac", "a.ogg", "a.mp3", "a.m4a", "a.opus", "a", "a.mp4.wav"]
)
def test_audio_and_extensionless_paths_are_not_video(name):
    assert media.is_video(name) is False


def test_resolve_passes_an_audio_file_through_untouched(make_wav, isolated_env):
    path = str(make_wav("t.wav"))
    assert asyncio.run(media.resolve(path)) == path
    # a pass-through must not even create the extraction dir
    assert not (isolated_env / "video_audio").exists()


def test_resolve_passes_an_empty_path_through(isolated_env):
    assert asyncio.run(media.resolve("")) == ""


def test_resolve_does_not_second_guess_a_mislabeled_audio_file(make_wav, tmp_path):
    """A WAV named .mp4 is NOT sniffed — detection is the extension allowlist,
    so the failure surfaces where the caller's decoder reads it."""
    src = make_wav("t.wav")
    lying = tmp_path / "lying.wav"
    Path(src).replace(lying)
    assert asyncio.run(media.resolve(str(lying))) == str(lying)


# --- extraction -----------------------------------------------------------


def test_extraction_round_trips_duration_and_tone(make_video, isolated_env):
    src = make_video("lecture.mp4", secs=2.0, rate=44_100, freq=440.0, amp=0.5)

    out = Path(asyncio.run(media.resolve(str(src))))

    assert out.exists() and out.suffix == ".wav"
    assert out.parent == isolated_env / "video_audio"
    data, rate = read(out)
    assert rate == 44_100  # source rate preserved
    assert data.shape[1] == 1  # source channel count preserved
    # AAC pads head and tail — a few tens of ms long, never short
    assert 2.0 <= len(data) / rate <= 2.1
    assert peak_hz(data, rate) == pytest.approx(440.0, abs=2.0)
    assert float(np.sqrt((data**2).mean())) == pytest.approx(0.5 / np.sqrt(2), rel=0.05)


def test_extraction_preserves_a_stereo_source(make_video):
    src = make_video("stereo.mov", secs=1.0, rate=48_000, channels=2)
    data, rate = read(asyncio.run(media.resolve(str(src))))
    assert (rate, data.shape[1]) == (48_000, 2)


def test_extraction_writes_pcm16(make_video):
    """The subtype every other import path in the engine produces."""
    out = asyncio.run(media.resolve(str(make_video("t.mkv"))))
    assert sf.info(out).subtype == "PCM_16"


def test_an_audio_only_webm_extracts_too(make_video):
    """webm is filed as a video container, but plenty are audio-only — and
    soundfile cannot read one at all, so this path is the only way in."""
    src = make_video("voice.webm", secs=1.0, rate=48_000, codec="libopus")
    data, rate = read(asyncio.run(media.resolve(str(src))))
    assert peak_hz(data, rate) == pytest.approx(440.0, abs=5.0)


# --- failures -------------------------------------------------------------


def test_a_video_with_no_audio_track_is_a_plain_error(make_video):
    src = make_video("silent.mp4", secs=0.5, audio=False)
    with pytest.raises(ValueError, match="no audio track in silent.mp4"):
        asyncio.run(media.resolve(str(src)))


def test_a_video_with_no_audio_track_leaves_nothing_behind(make_video, isolated_env):
    src = make_video("silent.mp4", secs=0.5, audio=False)
    with pytest.raises(ValueError):
        asyncio.run(media.resolve(str(src)))
    assert list((isolated_env / "video_audio").iterdir()) == []


def test_an_undecodable_container_is_a_plain_error(tmp_path):
    junk = tmp_path / "broken.mp4"
    junk.write_bytes(b"not a container, just bytes")
    with pytest.raises(ValueError, match="could not read broken.mp4"):
        asyncio.run(media.resolve(str(junk)))


def test_a_container_that_opens_but_will_not_decode_is_a_plain_error(
    make_video, isolated_env
):
    """A readable header over a smashed payload — the failure mode a partial
    download or a bad copy actually produces."""
    src = make_video("torn.mp4", secs=1.0)
    raw = bytearray(src.read_bytes())
    # the middle third is mdat; the tail is the moov PyAV writes last, and
    # breaking THAT would fail at open() instead
    raw[len(raw) // 3 : len(raw) // 3 + 2000] = b"\xff" * 2000
    src.write_bytes(bytes(raw))

    with pytest.raises(ValueError, match="could not decode the audio in torn.mp4"):
        asyncio.run(media.resolve(str(src)))
    assert list((isolated_env / "video_audio").iterdir()) == []  # no half-written wav


def test_a_missing_video_is_a_plain_error(tmp_path):
    with pytest.raises(ValueError, match="could not read gone.mp4"):
        asyncio.run(media.resolve(str(tmp_path / "gone.mp4")))


# --- the extraction cache -------------------------------------------------


def test_the_same_video_is_decoded_once(make_video):
    src = str(make_video("talk.mp4", secs=1.0))
    first = Path(asyncio.run(media.resolve(src)))
    stamp = first.stat().st_mtime_ns

    second = Path(asyncio.run(media.resolve(src)))

    assert second == first
    assert second.stat().st_mtime_ns == stamp  # not rewritten


def test_a_rewritten_source_gets_a_fresh_extraction(make_video):
    first = asyncio.run(media.resolve(str(make_video("take.mp4", secs=1.0, freq=440.0))))
    # same name, different content (and length, so mtime granularity can't hide it)
    second = asyncio.run(media.resolve(str(make_video("take.mp4", secs=1.5, freq=880.0))))
    assert second != first
    data, rate = read(second)
    assert peak_hz(data, rate) == pytest.approx(880.0, abs=2.0)


def test_the_cache_is_pruned_to_its_budget(make_video, isolated_env, monkeypatch):
    monkeypatch.setattr(media, "_CACHE_BUDGET", 1)  # anything but the newest goes
    old = Path(asyncio.run(media.resolve(str(make_video("old.mp4", secs=1.0)))))
    fresh = Path(asyncio.run(media.resolve(str(make_video("new.mp4", secs=1.0)))))
    assert fresh.exists()
    assert not old.exists()


def test_pruning_sweeps_a_part_file_left_by_a_killed_extraction(
    make_video, isolated_env, monkeypatch
):
    d = isolated_env / "video_audio"
    d.mkdir(parents=True, exist_ok=True)
    stale = d / "half.wav.deadbeef.part"
    stale.write_bytes(b"partial")
    monkeypatch.setattr(media, "_STALE_PART_SECS", -1.0)  # everything counts as stale

    asyncio.run(media.resolve(str(make_video("t.mp4", secs=0.5))))

    assert not stale.exists()


def test_an_in_flight_part_file_survives_a_prune(make_video, isolated_env):
    d = isolated_env / "video_audio"
    d.mkdir(parents=True, exist_ok=True)
    live = d / "other.wav.cafe1234.part"
    live.write_bytes(b"still decoding")

    asyncio.run(media.resolve(str(make_video("t.mp4", secs=0.5))))

    assert live.exists()  # an hour old is stale; seconds old is someone's work

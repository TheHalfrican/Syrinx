"""Video-container ingest — the single chokepoint every imported path goes through.

Source audio arrives inside video files often enough (a lecture recording, a
song's music video, a take pulled off a phone) that stripping the audio in
another tool first was a standing tax. Every engine method that takes a
user-chosen path calls :func:`resolve`: real audio files pass through
untouched, a video container gets its default audio stream demuxed and decoded
to a WAV, and that WAV's path stands in for the original everywhere downstream.

PyAV — FFmpeg's demuxers/decoders — already ships in the engine venv as a
faster-whisper dependency, so this adds no install. It is imported lazily (the
CI dependency contract and the isolated workers must keep importing this
module without it).

Detection is an **extension allowlist**, deliberately: a file that claims to be
a WAV and isn't surfaces its normal soundfile/librosa error instead of silently
taking the video path. Mislabeled files are rare; predictable errors are not.

Extractions land in ``$SYRINX_DATA_DIR/video_audio`` under a name derived from
the source's (path, mtime, size), so re-importing the same video — the app
hands the same path to TranscribeFile, FileEnvelope, PlayFile, ConvertVoice and
SaveSourceClip in turn — decodes it exactly once. The dir is size-bounded (a
feature-length film's PCM16 audio is over a gigabyte), newest kept.
"""

import asyncio
import hashlib
import itertools
import logging
import time
import uuid
from pathlib import Path

log = logging.getLogger("syrinx.engine.media")

# The containers the app's import pickers offer. webm is here rather than with
# the audio formats because it IS a video container — and an audio-only webm
# decodes through this path too, which is a straight upgrade: soundfile cannot
# read one at all.
VIDEO_SUFFIXES = frozenset({".mp4", ".mov", ".mkv", ".webm", ".avi", ".m4v"})

# Extractions are a cache, not a store: nothing else references them by id, so
# they are evicted newest-first past this budget rather than kept forever.
_CACHE_BUDGET = 2 * 1024**3
# A ``.part`` this old belongs to an extraction killed mid-decode, not to one
# in flight (decode runs faster than realtime).
_STALE_PART_SECS = 3600.0


def is_video(path) -> bool:
    """True when *path*'s extension names a video container we extract from."""
    return Path(path).suffix.lower() in VIDEO_SUFFIXES


def _cache_dir() -> Path:
    """Engine-owned home for extracted audio — mirrors recording.py's
    ``$SYRINX_DATA_DIR/<subdir>`` layout."""
    from .paths import data_dir

    d = data_dir() / "video_audio"
    d.mkdir(parents=True, exist_ok=True)
    return d


def _cache_name(src: Path) -> str:
    """A stable per-take name: same file, same bytes → same extraction. The
    stem rides along so the dir stays readable when someone goes looking."""
    try:
        st = src.stat()
        key = f"{src.resolve()}|{int(st.st_mtime)}|{st.st_size}"
    except OSError:
        key = str(src)
    digest = hashlib.sha1(key.encode("utf-8", "replace")).hexdigest()[:16]
    stem = "".join(c for c in src.stem if c.isalnum() or c in "-_")[:40]
    return f"{stem}-{digest}.wav" if stem else f"{digest}.wav"


def _prune(keep: Path) -> None:
    """Hold the cache under :data:`_CACHE_BUDGET`, newest first, never
    evicting *keep* (the extraction the caller is about to use)."""
    d = _cache_dir()
    now = time.time()
    # One guard for the whole sweep: every failure here is another process
    # having removed the same entry first, and the next extraction re-runs it.
    try:
        for part in d.glob("*.part"):
            if now - part.stat().st_mtime > _STALE_PART_SECS:
                part.unlink(missing_ok=True)
        entries = sorted(
            ((p.stat().st_mtime, p.stat().st_size, p) for p in d.glob("*.wav")),
            reverse=True,
        )
        total = 0
        for _mtime, size, p in entries:
            total += size
            if total > _CACHE_BUDGET and p != keep:
                p.unlink(missing_ok=True)
    except OSError:
        log.debug("video_audio prune interrupted", exc_info=True)


def extract_audio(path: str) -> str:
    """Decode the default audio stream of the video at *path* into a WAV and
    return its path. Blocking — callers go through :func:`resolve`.

    Sample rate and channel count are the source's (downstream code resamples
    and downmixes where it needs to); the WAV is PCM16, the format every other
    import path in the engine produces. Raises ``ValueError`` when the
    container has no audio track or its audio will not decode.
    """
    import av
    import soundfile as sf

    src = Path(path)
    dest = _cache_dir() / _cache_name(src)
    if dest.exists():
        return str(dest)

    name = src.name
    try:
        container = av.open(str(src))
    except Exception as e:  # noqa: BLE001 — av raises its own error tree
        raise ValueError(f"could not read {name}: {e}") from e
    tmp = dest.with_name(f"{dest.name}.{uuid.uuid4().hex[:8]}.part")
    frames = rate = channels = 0
    try:
        stream = container.streams.best("audio")
        if stream is not None:
            rate, channels = int(stream.rate), stream.layout.nb_channels
            # Only the SAMPLE FORMAT is resampled (planar float → packed s16);
            # rate and layout stay the source's.
            resampler = av.AudioResampler(format="s16", layout=stream.layout, rate=rate)
            with sf.SoundFile(
                str(tmp), "w", samplerate=rate, channels=channels,
                format="WAV", subtype="PCM_16",
            ) as out:
                # the trailing None flushes whatever the resampler still holds
                for frame in itertools.chain(container.decode(stream), [None]):
                    for packed in resampler.resample(frame):
                        block = packed.to_ndarray().reshape(-1, channels)
                        out.write(block)
                        frames += len(block)
            if frames:
                tmp.replace(dest)
    except Exception as e:  # noqa: BLE001
        raise ValueError(f"could not decode the audio in {name}: {e}") from e
    finally:
        container.close()
        tmp.unlink(missing_ok=True)  # a no-op once replace() has moved it
    if not frames:
        # No audio stream, or one that decoded to nothing — same thing to the
        # user, and the same sentence.
        raise ValueError(f"no audio track in {name}")
    log.info(
        "extracted %.1f s of %d-ch %d Hz audio from %s", frames / rate, channels, rate, name
    )
    _prune(dest)
    return str(dest)


async def resolve(path: str) -> str:
    """The path the engine should actually read for *path*.

    Audio files come back unchanged. Video containers are extracted (off the
    event loop — decode is faster than realtime but a feature-length file still
    costs seconds) and the extraction's path is returned in their place.
    """
    if not path or not is_video(path):
        return path
    return await asyncio.to_thread(extract_audio, path)

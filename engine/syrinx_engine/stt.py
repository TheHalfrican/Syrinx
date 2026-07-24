"""Speech-to-text for dictation.

faster-whisper (CTranslate2): CPU-fast with int8, auto-CUDA (float16) on the
4090. Kept deliberately light and torch-independent. Swappable for a whisper.cpp
binding later behind this same interface.

`transcribe()` takes a file PATH (the dictate pill records a WAV) rather than raw
PCM, so we don't marshal seconds of audio over D-Bus.
"""

import asyncio
import logging
import os
import shutil
import sys
from pathlib import Path

log = logging.getLogger("syrinx.engine.stt")


# The cu12 CUDA DLLs CTranslate2 4.8.x needs at GPU-encode time, each mapped to
# the ``nvidia-*-cu12`` wheel bin dir (relative to site-packages) it ships in.
# Deliberately NOT cuDNN: torch 2.13+cu130 bundles its own cuDNN 9 (a cu13
# build) and MUST keep winning the loader — staging the cu12 ``cudnn64_9.dll``
# anywhere gives CUDNN_STATUS_SUBLIBRARY_VERSION_MISMATCH in every conv.
_CT2_CUDA_DLLS = {
    "cublas64_12.dll": ("nvidia", "cublas", "bin"),
    "cublasLt64_12.dll": ("nvidia", "cublas", "bin"),
    # cublas64_12 delay-loads cudart64_12 on the first GPU matmul; torch's
    # cu130 build ships only cudart64_13, so cudart64_12 must be staged too or
    # the very first cuBLAS call raises "cudart64_12.dll not found".
    "cudart64_12.dll": ("nvidia", "cuda_runtime", "bin"),
}


def _site_packages_dirs() -> list:
    """purelib/platlib bases under the running interpreter (where pip-installed
    wheels — ctranslate2, nvidia-*-cu12 — land)."""
    import sysconfig

    dirs = []
    paths = sysconfig.get_paths()
    for key in ("purelib", "platlib"):
        base = paths.get(key)
        if not base:
            continue
        p = Path(base)
        if p not in dirs:
            dirs.append(p)
    return dirs


def _ctranslate2_dir() -> Path | None:
    """The installed ``ctranslate2`` package dir (where ``ctranslate2.dll`` and
    its bundled ``cudnn64_9.dll`` live) — ``None`` if faster-whisper's CT2 isn't
    installed."""
    for base in _site_packages_dirs():
        d = base / "ctranslate2"
        if d.is_dir():
            return d
    return None


def _stage_ct2_cuda_dlls() -> None:
    """On Windows, copy the cu12 cuBLAS + cudart DLLs *into* the ctranslate2
    package dir so faster-whisper's CUDA path can load them.

    CTranslate2 4.8.x loads cuBLAS ONLY from its own package dir (beside
    ``ctranslate2.dll``) — it honours neither ``os.add_dll_directory`` nor
    ``PATH``. So an add-dll-directory hint (what this used to do) never worked;
    the DLLs have to physically sit next to ``ctranslate2.dll``.

    Idempotent: a DLL already present with a matching byte size is left alone,
    so re-imports and re-runs never re-copy. Size (not a hash) is enough — the
    ``nvidia-*-cu12`` wheels are versioned, so a size match is the same binary.

    Non-fatal: a missing source or a failed copy logs a single clear warning
    naming the piece and returns. CPU inference still works, and a partial or
    absent stage must never crash import.
    """
    if sys.platform != "win32":
        return
    dest = _ctranslate2_dir()
    if dest is None:
        log.debug("ctranslate2 dir not found; skipping CUDA DLL staging")
        return
    bases = _site_packages_dirs()
    for name, sub in _CT2_CUDA_DLLS.items():
        src = next(
            (b.joinpath(*sub, name) for b in bases if b.joinpath(*sub, name).exists()),
            None,
        )
        if src is None:
            # No nvidia-*-cu12 wheel for this piece — a CPU box, or the CUDA
            # runtime wheels aren't installed. Warn once and continue; CUDA STT
            # is unavailable but import (and CPU fallback) must survive.
            log.warning(
                "CUDA STT: %s not found under nvidia/*/bin — CTranslate2 GPU "
                "inference will be unavailable (CPU fallback still works)",
                name,
            )
            continue
        target = dest / name
        try:
            if target.exists() and target.stat().st_size == src.stat().st_size:
                continue  # already staged, same binary
            shutil.copy2(src, target)
            log.debug("staged CUDA DLL %s -> %s", src, target)
        except OSError as e:  # noqa: PERF203
            log.warning(
                "CUDA STT: could not stage %s into the ctranslate2 dir: %s", name, e
            )


class Transcriber:
    def __init__(self) -> None:
        self._model = None
        self.model_size = os.environ.get("SYRINX_WHISPER_MODEL", "base.en")

    def set_model(self, identifier: str) -> None:
        """Switch the whisper model (a size name or a HF repo); reloads lazily."""
        if identifier and identifier != self.model_size:
            self.model_size = identifier
            self._model = None

    async def load(self) -> None:
        if self._model is not None:
            return
        await asyncio.to_thread(self._load_sync)

    def _load_sync(self) -> None:
        _stage_ct2_cuda_dlls()  # win32: cu12 cuBLAS/cudart into ctranslate2 dir (see helper)
        from faster_whisper import WhisperModel

        try:
            import torch

            cuda = torch.cuda.is_available()
        except Exception:  # noqa: BLE001
            cuda = False

        device = "cuda" if cuda else "cpu"
        compute = "float16" if cuda else "int8"
        log.info("loading faster-whisper %s on %s (%s)...", self.model_size, device, compute)
        self._model = WhisperModel(self.model_size, device=device, compute_type=compute)
        log.info("faster-whisper loaded")

    async def transcribe(self, audio_path: str) -> str:
        await self.load()

        def _run() -> str:
            segments, _info = self._model.transcribe(
                audio_path, language="en", vad_filter=True
            )
            return " ".join(seg.text.strip() for seg in segments).strip()

        text = await asyncio.to_thread(_run)
        log.info("transcribed: %r", text[:80])
        return text

    async def transcribe_stream(self, audio_path: str, on_partial=None) -> str:
        """Transcribe a (possibly long) file, invoking ``on_partial(text_so_far)``
        on the event loop as each segment decodes — the Transcription view shows
        text arriving live instead of a spinner."""
        await self.load()
        loop = asyncio.get_running_loop()

        def _run() -> str:
            segments, _info = self._model.transcribe(
                audio_path, language="en", vad_filter=True
            )
            parts = []
            for seg in segments:
                parts.append(seg.text.strip())
                if on_partial is not None:
                    loop.call_soon_threadsafe(on_partial, " ".join(parts))
            return " ".join(parts).strip()

        text = await asyncio.to_thread(_run)
        log.info("transcribed (stream): %r", text[:80])
        return text

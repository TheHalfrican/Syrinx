"""TTS backends for Syrinx.

Two engines behind one interface, selected by ``SYRINX_TTS_ENGINE``:

- **kokoro** (default) — Kokoro-82M preset voices. Tiny, CPU-realtime. Great
  everywhere, including this iGPU box.
- **qwen** — Qwen3-TTS zero-shot voice cloning. Beefy (~3.5 GB), wants a GPU.
  This is the one that lights up on the RTX 4090.

Each backend implements the same small async interface:
    .device -> str                       # "cuda" | "rocm" | "mps" | "cpu"
    .supports_cloning -> bool
    async load()
    async list_voices() -> list[VoiceInfo]
    async synthesize(text, voice_id) -> (pcm_float32_bytes, sample_rate)
    async clone(name, sample_path, ref_text) -> voice_id
"""

import os
from dataclasses import dataclass


@dataclass
class VoiceInfo:
    id: str
    name: str


def detect_device() -> str:
    """Best available torch device: cuda / rocm / mps / cpu.

    A discrete GPU outranks Metal: a machine can report both (an eGPU, or a
    CUDA build under Rosetta), and the cuda path is the tuned one."""
    try:
        import torch

        if torch.cuda.is_available():
            return "rocm" if getattr(torch.version, "hip", None) else "cuda"
        if torch.backends.mps.is_available():
            return "mps"
    except Exception:  # noqa: BLE001
        pass
    return "cpu"


# detected device -> the string torch's ``.to()`` / ``device_map`` accepts.
# A ROCm build addresses the card as "cuda"; "mps" is torch's own name for
# Metal and passes through. Anything else (incl. an unknown) lands on cpu.
_TORCH_DEVICES = {"cuda": "cuda", "rocm": "cuda", "mps": "mps"}


def torch_device(device: str) -> str:
    """Torch's name for *device* (a ``detect_device()`` string)."""
    return _TORCH_DEVICES.get(device, "cpu")


def empty_device_cache() -> None:
    """Return freed model VRAM to the driver after an unload."""
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.empty_cache()
        elif torch.backends.mps.is_available():
            # unified memory: this returns the Metal allocator's cached blocks
            torch.mps.empty_cache()
    except Exception:  # noqa: BLE001
        pass


def make_backend(name: str | None = None, size: str = ""):
    """Instantiate the configured TTS backend (lazy imports keep deps optional).

    *size* picks the model variant for engines that have them (qwen 1.7B/0.6B,
    qwen_custom_voice 1.7B/0.6B, tada 1B/3B); "" = the backend's default.
    """
    name = (name or os.environ.get("SYRINX_TTS_ENGINE", "kokoro")).lower()
    if name == "kokoro":
        from .kokoro import KokoroBackend

        return KokoroBackend()
    if name == "qwen":
        from .qwen import QwenBackend

        return QwenBackend(size)
    if name == "luxtts":
        from .luxtts import LuxTTSBackend

        return LuxTTSBackend()
    if name == "chatterbox":
        from .chatterbox import ChatterboxBackend

        return ChatterboxBackend()
    if name == "chatterbox_turbo":
        from .chatterbox import ChatterboxTurboBackend

        return ChatterboxTurboBackend()
    if name == "qwen_custom_voice":
        from .qwen import QwenCustomVoiceBackend

        return QwenCustomVoiceBackend(size)
    if name == "tada":
        from .tada import TadaBackend

        return TadaBackend(size)
    raise ValueError(
        f"Unknown SYRINX_TTS_ENGINE={name!r} "
        "(expected: kokoro | qwen | qwen_custom_voice | luxtts | "
        "chatterbox | chatterbox_turbo | tada)"
    )

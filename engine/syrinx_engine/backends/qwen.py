"""Qwen3-TTS backend — zero-shot voice cloning.

The beefy engine: clones a voice from a short reference sample (+ its transcript)
and speaks arbitrary text in it. ~3.5 GB (1.7B) / ~1.2 GB (0.6B). Wants a GPU —
this is the one that lights up on the RTX 4090 (bf16 + TF32 + flash attention).
On CPU it still runs (float32), just slowly.

Cloned voices persist to ``$SYRINX_DATA_DIR/voices`` (default the per-OS data
dir — see ``paths.py``): one ``<id>.pt`` per prompt + an ``index.json`` mapping
id -> display name.

Grounded in the Voicebox pytorch_backend.py reference:
    Qwen3TTSModel.from_pretrained(...)
    model.create_voice_clone_prompt(ref_audio, ref_text, x_vector_only_mode=False)
    model.generate_voice_clone(text, voice_clone_prompt, language, instruct) -> (wavs, sr)

NOTE: not exercised on this iGPU box — validated on the 4090. Marked TODO(syrinx)
where a live-on-GPU check is still needed.
"""

import asyncio
import json
import logging
import os
import re
from pathlib import Path

import numpy as np

from . import VoiceInfo, detect_device, empty_device_cache
from .. import chunking
from ..paths import data_dir

log = logging.getLogger("syrinx.engine.tts.qwen")

MODELS = {
    "1.7B": "Qwen/Qwen3-TTS-12Hz-1.7B-Base",
    "0.6B": "Qwen/Qwen3-TTS-12Hz-0.6B-Base",
}


def _import_qwen_tts():
    """Import ``qwen_tts.Qwen3TTSModel``, translating the opaque pysox failure
    on a SoX-less box into an actionable message.

    qwen_tts pulls in pysox, which shells out to the ``sox`` binary at import
    time (``_get_valid_formats``); without it the import dies deep in pysox and
    the app's GenerationProgress error surface would otherwise show a cryptic
    ImportError instead of the one thing the user needs to do."""
    import importlib

    try:
        mod = importlib.import_module("qwen_tts")
    except Exception as e:  # noqa: BLE001
        if "sox" in str(e).lower():
            raise RuntimeError(
                "qwen engines need the SoX binary on PATH — install it "
                "(winget install ChrisBagwell.SoX) and restart the engine"
            ) from e
        raise
    return mod.Qwen3TTSModel


def _slug(name: str) -> str:
    s = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")
    return s or "voice"


def _as_float32(audio) -> np.ndarray:
    try:
        import torch

        if isinstance(audio, torch.Tensor):
            audio = audio.detach().cpu().numpy()
    except Exception:  # noqa: BLE001
        pass
    return np.asarray(audio, dtype=np.float32).reshape(-1)


class QwenBackend:
    supports_cloning = True

    def __init__(self, size: str = "") -> None:
        self.device = detect_device()
        self.model_size = size or os.environ.get("SYRINX_MODEL", "1.7B")
        if self.model_size not in MODELS:
            self.model_size = "1.7B"
        self._model = None
        self._prompts: dict[str, object] = {}  # voice_id -> loaded prompt (cache)
        self._voices_dir = data_dir() / "voices"
        self._voices_dir.mkdir(parents=True, exist_ok=True)

    # --- model ----------------------------------------------------------

    async def load(self) -> None:
        if self._model is not None:
            return
        await asyncio.to_thread(self._load_sync)

    def _load_sync(self) -> None:
        import torch

        Qwen3TTSModel = _import_qwen_tts()

        model_path = MODELS[self.model_size]
        if self.device in ("cuda", "rocm"):
            # Ada/RTX 4090 fast path — see docs/HARDWARE.md.
            torch.backends.cuda.matmul.allow_tf32 = True
            torch.backends.cudnn.allow_tf32 = True
            try:
                torch.backends.cuda.enable_flash_sdp(True)
            except Exception:  # noqa: BLE001
                pass
            log.info("loading Qwen3-TTS %s on %s (bf16)...", self.model_size, self.device)
            self._model = Qwen3TTSModel.from_pretrained(
                model_path, device_map=self.device, torch_dtype=torch.bfloat16
            )
        else:
            log.info("loading Qwen3-TTS %s on cpu (float32 — slow)...", self.model_size)
            self._model = Qwen3TTSModel.from_pretrained(
                model_path, torch_dtype=torch.float32, low_cpu_mem_usage=False
            )
        log.info("Qwen3-TTS %s loaded", self.model_size)

    def unload(self) -> None:
        """Free the model + on-device prompt cache (prompts reload from disk)."""
        self._model = None
        self._prompts.clear()
        empty_device_cache()

    def _prompt_path(self, key: str) -> Path:
        # Clone prompts embed model activations, so they're SIZE-specific:
        # a 1.7B prompt (hidden 2048) crashes 0.6B generation (hidden 1024)
        # with "Sizes of tensors must match". One cache file per size.
        return self._voices_dir / f"{key}_{self.model_size}.pt"

    # --- voices / cloning ----------------------------------------------

    def _index_path(self) -> Path:
        return self._voices_dir / "index.json"

    def _read_index(self) -> dict[str, str]:
        try:
            return json.loads(self._index_path().read_text())
        except Exception:  # noqa: BLE001
            return {}

    def _write_index(self, index: dict[str, str]) -> None:
        self._index_path().write_text(json.dumps(index, indent=2))

    async def list_voices(self) -> list[VoiceInfo]:
        # Qwen has no presets — only voices the user has cloned.
        return [VoiceInfo(vid, name) for vid, name in self._read_index().items()]

    async def clone(self, name: str, sample_path: str, ref_text: str = "") -> str:
        await self.load()
        if not ref_text:
            # Qwen needs the transcript of the reference clip. Once the dictate
            # pill's whisper.cpp lands we can auto-transcribe here. TODO(syrinx).
            raise ValueError("Qwen cloning needs ref_text (transcript of the sample)")

        voice_id = _slug(name)

        def _make() -> object:
            return self._model.create_voice_clone_prompt(
                ref_audio=str(sample_path), ref_text=ref_text, x_vector_only_mode=False
            )

        prompt = await asyncio.to_thread(_make)

        import torch

        torch.save(prompt, self._prompt_path(voice_id))
        index = self._read_index()
        index[voice_id] = name
        self._write_index(index)
        self._prompts[voice_id] = prompt
        log.info("cloned voice %r -> %s", name, voice_id)
        return voice_id

    def invalidate_profile(self, profile_id: str) -> None:
        """Forget a profile's cached clone prompt so it rebuilds from samples."""
        self._prompts.pop(profile_id, None)
        (self._voices_dir / f"{profile_id}.pt").unlink(missing_ok=True)  # pre-size legacy
        for size in MODELS:
            (self._voices_dir / f"{profile_id}_{size}.pt").unlink(missing_ok=True)
        (self._voices_dir / f"{profile_id}_combined.wav").unlink(missing_ok=True)

    def _get_prompt(self, voice_id: str):
        if voice_id not in self._prompts:
            import torch

            path = self._prompt_path(voice_id)
            if not path.exists():
                # pre-size legacy file — created by whatever size was active then
                path = self._voices_dir / f"{voice_id}.pt"
            if not path.exists():
                raise ValueError(f"unknown cloned voice: {voice_id}")
            self._prompts[voice_id] = torch.load(
                path, map_location=self.device, weights_only=False
            )
        return self._prompts[voice_id]

    # --- synthesis ------------------------------------------------------

    async def _generate_chunked(self, text: str, prompt, instruct) -> tuple[bytes, int]:
        # Long text generates per sentence-boundary chunk: the autoregressive
        # decode is bounded per chunk (VRAM and drift both grow with sequence
        # length), then chunks are crossfaded — same pattern as LuxTTS.
        def _run(chunk_text: str) -> tuple[np.ndarray, int]:
            wavs, sample_rate = self._model.generate_voice_clone(
                text=chunk_text,
                voice_clone_prompt=prompt,
                language="english",
                instruct=instruct,
            )
            return _as_float32(wavs[0]), int(sample_rate)

        return await chunking.synthesize_chunked(_run, text, log=log, label="qwen")

    async def synthesize(self, text: str, voice_id: str, instruct: str = "") -> tuple[bytes, int]:
        await self.load()
        prompt = self._get_prompt(voice_id)
        log.info("synthesize [qwen %s] (%s): %r", self.model_size, voice_id, text[:60])
        return await self._generate_chunked(text, prompt, instruct or None)

    # --- profile-based cloning -----------------------------------------

    def _combine_samples(self, profile) -> tuple[str, str]:
        """Concatenate a profile's reference samples into one WAV + joined text."""
        import soundfile as sf

        audios, texts, rate = [], [], 24_000
        for s in profile.samples:
            audio, rate = sf.read(s.audio_path, dtype="float32")
            if getattr(audio, "ndim", 1) > 1:
                audio = audio.mean(axis=1)  # to mono
            audios.append(audio)
            if s.reference_text:
                texts.append(s.reference_text)
        if not audios:
            raise ValueError(f"profile {profile.id} has no samples to clone from")
        combined = np.concatenate(audios).astype(np.float32)
        out = self._voices_dir / f"{profile.id}_combined.wav"
        sf.write(out, combined, rate)
        return str(out), " ".join(texts)

    def _profile_prompt(self, profile):
        key = profile.id
        if key in self._prompts:
            return self._prompts[key]
        import torch

        # sized path only — a pre-size legacy cache may have the wrong hidden
        # dims, and rebuilding from samples costs seconds
        cache = self._prompt_path(key)
        if cache.exists():
            self._prompts[key] = torch.load(cache, map_location=self.device, weights_only=False)
            return self._prompts[key]
        audio, text = self._combine_samples(profile)
        prompt = self._model.create_voice_clone_prompt(
            ref_audio=audio, ref_text=text, x_vector_only_mode=False
        )
        torch.save(prompt, cache)
        self._prompts[key] = prompt
        return prompt

    async def synthesize_profile(self, profile, text: str, instruct: str = "") -> tuple[bytes, int]:
        await self.load()
        # prompt build can hit disk / run the encoder — keep it off the loop
        prompt = await asyncio.to_thread(self._profile_prompt, profile)
        log.info("synthesize_profile [qwen %s] (%s): %r", self.model_size, profile.id, text[:60])
        return await self._generate_chunked(text, prompt, instruct or None)


# --- Qwen CustomVoice — preset speakers + instruct ------------------------

# (speaker_id, display name) — the 9 built-in CustomVoice speakers.
CV_VOICES = [
    ("Vivian", "Vivian (zh ♀)"),
    ("Serena", "Serena (zh ♀)"),
    ("Uncle_Fu", "Uncle Fu (zh ♂)"),
    ("Dylan", "Dylan (zh ♂)"),
    ("Eric", "Eric (zh ♂)"),
    ("Ryan", "Ryan (en ♂)"),
    ("Aiden", "Aiden (en ♂)"),
    ("Ono_Anna", "Ono Anna (ja ♀)"),
    ("Sohee", "Sohee (ko ♀)"),
]
CV_DEFAULT_SPEAKER = "Ryan"
CV_MODELS = {
    "1.7B": "Qwen/Qwen3-TTS-12Hz-1.7B-CustomVoice",
    "0.6B": "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice",
}
# generate_custom_voice wants a language NAME ("English"), not a code.
CV_LANGUAGES = {
    "zh": "Chinese", "en": "English", "ja": "Japanese", "ko": "Korean",
    "de": "German", "fr": "French", "ru": "Russian", "pt": "Portuguese",
    "es": "Spanish", "it": "Italian",
}


class QwenCustomVoiceBackend:
    """Preset-speaker TTS with natural-language style control (instruct).

    Same qwen_tts library as QwenBackend, different checkpoint and entry
    point (generate_custom_voice). No cloning — 9 fixed speakers.
    """

    supports_cloning = False

    def __init__(self, size: str = "") -> None:
        self.device = detect_device()
        self.model_size = size or os.environ.get("SYRINX_QWEN_CV_SIZE", "1.7B")
        if self.model_size not in CV_MODELS:
            self.model_size = "1.7B"
        self._model = None
        self._load_lock = asyncio.Lock()

    async def load(self) -> None:
        if self._model is None:
            async with self._load_lock:
                if self._model is None:
                    await asyncio.to_thread(self._load_sync)

    def _load_sync(self) -> None:
        import torch

        Qwen3TTSModel = _import_qwen_tts()

        path = CV_MODELS[self.model_size]
        if self.device in ("cuda", "rocm"):
            torch.backends.cuda.matmul.allow_tf32 = True
            torch.backends.cudnn.allow_tf32 = True
            try:
                torch.backends.cuda.enable_flash_sdp(True)
            except Exception:  # noqa: BLE001
                pass
            log.info("loading Qwen CustomVoice %s on %s (bf16)...", self.model_size, self.device)
            self._model = Qwen3TTSModel.from_pretrained(
                path, device_map=self.device, torch_dtype=torch.bfloat16
            )
        else:
            log.info("loading Qwen CustomVoice %s on cpu (float32 — slow)...", self.model_size)
            self._model = Qwen3TTSModel.from_pretrained(
                path, torch_dtype=torch.float32, low_cpu_mem_usage=False
            )
        log.info("Qwen CustomVoice %s loaded", self.model_size)

    def unload(self) -> None:
        self._model = None
        empty_device_cache()

    async def list_voices(self) -> list[VoiceInfo]:
        return [VoiceInfo(sid, name) for sid, name in CV_VOICES]

    def _gen(self, speaker: str, language: str, instruct):
        lang = CV_LANGUAGES.get(language, "Auto")

        def _run(chunk_text: str) -> tuple[np.ndarray, int]:
            kwargs = {"text": chunk_text, "language": lang, "speaker": speaker}
            if instruct:
                kwargs["instruct"] = instruct
            wavs, rate = self._model.generate_custom_voice(**kwargs)
            return _as_float32(wavs[0]), int(rate)

        return _run

    async def synthesize(self, text: str, voice_id: str, instruct: str = "") -> tuple[bytes, int]:
        await self.load()
        known = {sid for sid, _ in CV_VOICES}
        speaker = voice_id if voice_id in known else CV_DEFAULT_SPEAKER
        log.info("synthesize [qwen_cv %s] (%s): %r", self.model_size, speaker, text[:60])
        # no language context on the raw preset path — the model auto-detects
        return await chunking.synthesize_chunked(
            self._gen(speaker, "", instruct or None), text, log=log, label="qwen_cv"
        )

    async def synthesize_profile(self, profile, text: str, instruct: str = "") -> tuple[bytes, int]:
        # Preset profiles carry the speaker in preset_voice_id; a cloned
        # profile mistakenly pinned here falls back to the default speaker.
        await self.load()
        known = {sid for sid, _ in CV_VOICES}
        speaker = getattr(profile, "preset_voice_id", "") or CV_DEFAULT_SPEAKER
        if speaker not in known:
            speaker = CV_DEFAULT_SPEAKER
        language = getattr(profile, "language", "en") or "en"
        log.info(
            "synthesize_profile [qwen_cv %s] (%s→%s): %r",
            self.model_size, profile.id, speaker, text[:60],
        )
        return await chunking.synthesize_chunked(
            self._gen(speaker, language, instruct or None), text, log=log, label="qwen_cv"
        )

    def invalidate_profile(self, profile_id: str) -> None:
        pass  # nothing cached per profile

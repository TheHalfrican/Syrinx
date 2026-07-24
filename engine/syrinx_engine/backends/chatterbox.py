"""Chatterbox backends — Resemble AI zero-shot cloning, in-process.

``chatterbox-tts`` is installed ``--no-deps`` (it pins numpy<1.26 /
torch==2.6 / transformers==4.46.3, all stale — the code runs fine on the
modern stack, proven by Voicebox on transformers 4.57). Its real sub-deps
live in the engine's ``[chatterbox]`` extra. Two models, two engine names:

- ``chatterbox``        ChatterboxMultilingualTTS — 23 languages, exaggeration.
- ``chatterbox_turbo``  ChatterboxTurboTTS — 350M English, [laugh]/[cough] tags.

Both take the reference audio as a file path at generation time (no encoded
prompt to cache), so a profile's samples are concatenated into one combined
reference wav. Ported from Voicebox's chatterbox(_turbo)_backend.py: the
float64→float32 patches, the eager-attention fix, per-language generation
defaults, and the per-chunk trailing-hallucination trim.
"""

import asyncio
import logging
import threading
from pathlib import Path

import numpy as np

from . import detect_device, empty_device_cache
from .. import chunking
from ..paths import data_dir

log = logging.getLogger("syrinx.engine.tts.chatterbox")

MTL_HF_REPO = "ResembleAI/chatterbox"
TURBO_HF_REPO = "ResembleAI/chatterbox-turbo"

# guards the torch.load monkey-patch used for CPU loading
_TORCH_LOAD_LOCK = threading.Lock()


def combined_ref_wav(profile, voices_dir: Path) -> str:
    """Combined reference wav for a profile (cached on disk, shared by the
    chatterbox backends AND the VC engines that take a reference path).

    The multi-sample combining is per-sample peak normalization +
    concatenation (Voicebox's combine_voice_prompts).
    """
    out = voices_dir / f"{profile.id}_cbxref.wav"
    if out.exists():
        return str(out)
    import soundfile as sf

    if not profile.samples:
        raise ValueError(f"profile {profile.id} has no samples to clone from")
    parts, rate = [], 0
    for s in profile.samples:
        audio, sr = sf.read(s.audio_path, dtype="float32")
        if getattr(audio, "ndim", 1) > 1:
            audio = audio.mean(axis=1)  # to mono
        rate = rate or sr
        if sr != rate:
            # samples can arrive at different rates (imports vs recordings) —
            # concatenating raw would pitch-shift every mismatched sample
            import librosa

            audio = librosa.resample(audio, orig_sr=sr, target_sr=rate)
        peak = float(np.abs(audio).max()) or 1.0
        parts.append(audio / peak * 0.95)
    sf.write(out, np.concatenate(parts).astype(np.float32), rate)
    return str(out)


def _as_f32(wav) -> np.ndarray:
    try:
        import torch

        if isinstance(wav, torch.Tensor):
            wav = wav.detach().cpu().numpy()
    except Exception:  # noqa: BLE001
        pass
    return np.asarray(wav, dtype=np.float32).reshape(-1)


def _patch_f32_mel(tokenizer) -> None:
    """float64→float32 cast on the S3Tokenizer mel path (also needed by the
    VC backend, whose model has no VoiceEncoder — hence the split)."""
    import types

    _orig_log_mel = tokenizer.log_mel_spectrogram.__func__

    def _f32_log_mel(self_tokzr, audio, padding=0):
        import torch as _torch

        if _torch.is_tensor(audio):
            audio = audio.float()
        return _orig_log_mel(self_tokzr, audio, padding)

    tokenizer.log_mel_spectrogram = types.MethodType(_f32_log_mel, tokenizer)


def _patch_f32(model) -> None:
    """Patch float64→float32 dtype mismatches in upstream chatterbox.

    librosa.load returns float64; two upstream paths tensor-ify it without
    casting and matmul against float32 weights (S3Tokenizer mel + VoiceEncoder).
    """
    import types

    _patch_f32_mel(model.s3gen.tokenizer)

    _ve = model.ve
    _orig_ve_forward = _ve.forward.__func__

    def _f32_ve_forward(self_ve, mels):
        return _orig_ve_forward(self_ve, mels.float())

    _ve.forward = types.MethodType(_f32_ve_forward, _ve)


class _ChatterboxBase:
    supports_cloning = True
    engine_name = "chatterbox"

    def __init__(self) -> None:
        self.device = detect_device()
        self.model_size = "default"
        self._model = None
        self._load_lock = asyncio.Lock()
        self._voices_dir = data_dir() / "voices"
        self._voices_dir.mkdir(parents=True, exist_ok=True)

    # --- model ----------------------------------------------------------

    async def load(self) -> None:
        if self._model is not None:
            return
        async with self._load_lock:
            if self._model is None:
                await asyncio.to_thread(self._load_sync)

    def _torch_device(self) -> str:
        return "cuda" if self.device in ("cuda", "rocm") else "cpu"

    def _load_on(self, loader) -> object:
        """Run *loader(device)*, forcing torch.load to the CPU when needed."""
        device = self._torch_device()
        if device != "cpu":
            return loader(device)
        import torch

        _orig = torch.load

        def _patched(*args, **kwargs):
            kwargs.setdefault("map_location", "cpu")
            return _orig(*args, **kwargs)

        with _TORCH_LOAD_LOCK:
            torch.load = _patched
            try:
                return loader(device)
            finally:
                torch.load = _orig

    def unload(self) -> None:
        self._model = None
        empty_device_cache()

    # --- voices ----------------------------------------------------------

    async def list_voices(self) -> list:
        return []  # cloning-only, no presets

    async def synthesize(self, text: str, voice_id: str, instruct: str = "") -> tuple:
        raise ValueError(f"{self.engine_name} has no preset voices")

    def invalidate_profile(self, profile_id: str) -> None:
        (self._voices_dir / f"{profile_id}_cbxref.wav").unlink(missing_ok=True)

    def _ref_wav(self, profile) -> str:
        return combined_ref_wav(profile, self._voices_dir)

    # --- synthesis --------------------------------------------------------

    def _generate(self, text: str, ref_audio: str, language: str):
        raise NotImplementedError

    async def synthesize_profile(self, profile, text: str, instruct: str = "") -> tuple:
        await self.load()
        ref = await asyncio.to_thread(self._ref_wav, profile)
        language = getattr(profile, "language", "en") or "en"
        chunks = chunking.split_text_into_chunks(text, chunking.max_chunk_chars())

        def _run(chunk_text: str) -> tuple[np.ndarray, int]:
            wav = self._generate(chunk_text, ref, language)
            rate = int(getattr(self._model, "sr", 24_000))
            # Chatterbox hallucinates trailing noise after silence — trim
            # each chunk before the crossfade join.
            return chunking.trim_tts_output(_as_f32(wav), rate), rate

        log.info(
            "synthesize_profile [%s] (%s): %r", self.engine_name, profile.id, text[:60]
        )
        if len(chunks) <= 1:
            audio, rate = await asyncio.to_thread(_run, text)
            return audio.tobytes(), rate

        log.info("%s: %d chars -> %d chunks", self.engine_name, len(text), len(chunks))
        parts: list[np.ndarray] = []
        rate = 24_000
        for i, chunk in enumerate(chunks, 1):
            log.info("%s chunk %d/%d (%d chars)", self.engine_name, i, len(chunks), len(chunk))
            audio, rate = await asyncio.to_thread(_run, chunk)
            parts.append(audio)
        return chunking.crossfade_concat(parts, rate).tobytes(), rate


class ChatterboxBackend(_ChatterboxBase):
    """Chatterbox Multilingual — 23 languages, zero-shot cloning."""

    engine_name = "chatterbox"

    # Per-language generation defaults (Voicebox). Lower temp + higher cfg =
    # clearer speech for languages that need it.
    _LANG_DEFAULTS = {
        "he": {
            "exaggeration": 0.4,
            "cfg_weight": 0.7,
            "temperature": 0.65,
            "repetition_penalty": 2.5,
        },
    }
    _GLOBAL_DEFAULTS = {
        "exaggeration": 0.5,
        "cfg_weight": 0.5,
        "temperature": 0.8,
        "repetition_penalty": 2.0,
    }

    def _load_sync(self) -> None:
        from chatterbox.mtl_tts import ChatterboxMultilingualTTS

        log.info("loading Chatterbox Multilingual on %s...", self._torch_device())
        model = self._load_on(
            lambda dev: ChatterboxMultilingualTTS.from_pretrained(device=dev)
        )
        # sdpa attention breaks output_attentions in the t3 transformer —
        # force eager (Voicebox fix)
        t3_tfmr = model.t3.tfmr
        if hasattr(t3_tfmr, "config") and hasattr(t3_tfmr.config, "_attn_implementation"):
            t3_tfmr.config._attn_implementation = "eager"
            for layer in getattr(t3_tfmr, "layers", []):
                if hasattr(layer, "self_attn"):
                    layer.self_attn._attn_implementation = "eager"
        _patch_f32(model)
        self._model = model
        log.info("Chatterbox Multilingual loaded")

    def _generate(self, text: str, ref_audio: str, language: str):
        kw = self._LANG_DEFAULTS.get(language, self._GLOBAL_DEFAULTS)
        return self._model.generate(
            text, language_id=language, audio_prompt_path=ref_audio, **kw
        )


class ChatterboxTurboBackend(_ChatterboxBase):
    """Chatterbox Turbo — 350M English with [laugh]/[cough] paralinguistic tags."""

    engine_name = "chatterbox_turbo"

    def _load_sync(self) -> None:
        from huggingface_hub import snapshot_download
        from chatterbox.tts_turbo import ChatterboxTurboTTS

        log.info("loading Chatterbox Turbo on %s...", self._torch_device())
        local = snapshot_download(
            repo_id=TURBO_HF_REPO,
            allow_patterns=["*.safetensors", "*.json", "*.txt", "*.pt", "*.model"],
        )
        model = self._load_on(lambda dev: ChatterboxTurboTTS.from_local(local, dev))
        _patch_f32(model)
        self._model = model
        log.info("Chatterbox Turbo loaded")

    def _generate(self, text: str, ref_audio: str, language: str):
        # English-only; language is ignored. Tags like [laugh] pass through
        # in the text (the chunker keeps them atomic).
        return self._model.generate(
            text,
            audio_prompt_path=ref_audio,
            temperature=0.8,
            top_k=1000,
            top_p=0.95,
            repetition_penalty=1.2,
        )

"""HumeAI TADA backend — Llama-based speech-LM voice cloning, in-process.

``hume-tada`` is installed ``--no-deps`` (it pins torch>=2.7,<2.8 — stale;
runs fine on the modern stack, proven by Voicebox). Its only troublesome
import is ``dac`` (descript-audio-codec), which drags onnx/tensorboard via
descript-audiotools — ``dac_shim`` provides the one class TADA actually
uses (Snake1d), so the real package is never installed.

Two sizes behind one engine name (``SYRINX_TADA_SIZE=1B|3B``):
- tada-1b     English, Llama-3.2-1B base.
- tada-3b-ml  10 languages, Llama-3.2-3B base. Heavy.

Both share the HumeAI/tada-codec encoder. The encoder force-aligns the
reference audio with its transcript into an EncoderOutput prompt; prompts
are cached per profile as ``<id>_tada.pt`` (tensor dict) like Qwen's.
TADA hardcodes the gated meta-llama tokenizer repo — the ungated
``unsloth/Llama-3.2-1B`` mirror is fetched instead and injected via config
(Voicebox's approach; avoids monkey-patching AutoTokenizer).
"""

import asyncio
import logging
import os

import numpy as np

from . import detect_device, empty_device_cache
from .. import chunking
from ..dac_shim import install_dac_shim
from ..paths import data_dir

log = logging.getLogger("syrinx.engine.tts.tada")

CODEC_REPO = "HumeAI/tada-codec"
MODEL_REPOS = {"1B": "HumeAI/tada-1b", "3B": "HumeAI/tada-3b-ml"}
TOKENIZER_REPO = "unsloth/Llama-3.2-1B"  # ungated mirror, tokenizer files only


class TadaBackend:
    supports_cloning = True

    def __init__(self, size: str = "") -> None:
        self.device = detect_device()
        self.model_size = size or os.environ.get("SYRINX_TADA_SIZE", "1B")
        if self.model_size not in MODEL_REPOS:
            self.model_size = "1B"
        self._model = None
        self._encoder = None
        self._prompts: dict[str, dict] = {}  # profile_id -> tensor dict
        self._load_lock = asyncio.Lock()
        self._voices_dir = data_dir() / "voices"
        self._voices_dir.mkdir(parents=True, exist_ok=True)

    # --- model ----------------------------------------------------------

    async def load(self) -> None:
        if self._model is None:
            async with self._load_lock:
                if self._model is None:
                    await asyncio.to_thread(self._load_sync)

    def _torch_device(self) -> str:
        return "cuda" if self.device in ("cuda", "rocm") else "cpu"

    def _load_sync(self) -> None:
        install_dac_shim()  # before any tada import

        import torch
        from huggingface_hub import snapshot_download

        device = self._torch_device()
        repo = MODEL_REPOS[self.model_size]
        log.info("loading TADA %s on %s...", self.model_size, device)

        snapshot_download(
            repo_id=CODEC_REPO,
            allow_patterns=["*.safetensors", "*.json", "*.txt", "*.bin"],
        )
        snapshot_download(
            repo_id=repo,
            allow_patterns=["*.safetensors", "*.json", "*.txt", "*.bin", "*.model"],
        )
        tokenizer_path = snapshot_download(
            repo_id=TOKENIZER_REPO,
            allow_patterns=["tokenizer*", "special_tokens*"],
        )

        dtype = torch.float32
        if device == "cuda":
            try:
                if torch.cuda.is_bf16_supported():
                    dtype = torch.bfloat16
            except Exception:  # noqa: BLE001
                pass

        # point TADA's aligner + LM at the local ungated tokenizer
        from tada.modules.aligner import AlignerConfig

        AlignerConfig.tokenizer_name = tokenizer_path

        from tada.modules.encoder import Encoder

        self._encoder = Encoder.from_pretrained(CODEC_REPO, subfolder="encoder").to(device)
        self._encoder.eval()

        from tada.modules.tada import TadaConfig, TadaForCausalLM

        config = TadaConfig.from_pretrained(repo)
        config.tokenizer_name = tokenizer_path
        self._model = TadaForCausalLM.from_pretrained(
            repo, config=config, torch_dtype=dtype
        ).to(device)
        self._model.eval()
        log.info("TADA %s loaded", self.model_size)

    def unload(self) -> None:
        """Free the LM + encoder; profile prompts stay cached on CPU/disk."""
        self._model = None
        self._encoder = None
        empty_device_cache()

    # --- voices ----------------------------------------------------------

    async def list_voices(self) -> list:
        return []  # cloning-only, no presets

    async def synthesize(self, text: str, voice_id: str, instruct: str = "") -> tuple:
        raise ValueError("TADA has no preset voices")

    def invalidate_profile(self, profile_id: str) -> None:
        self._prompts.pop(profile_id, None)
        (self._voices_dir / f"{profile_id}_tada.pt").unlink(missing_ok=True)
        (self._voices_dir / f"{profile_id}_tadaref.wav").unlink(missing_ok=True)

    # --- prompt encoding --------------------------------------------------

    def _combine_samples(self, profile) -> tuple[str, str, int]:
        """Concatenate a profile's samples into one wav + joined transcript."""
        import soundfile as sf

        if not profile.samples:
            raise ValueError(f"profile {profile.id} has no samples to clone from")
        out = self._voices_dir / f"{profile.id}_tadaref.wav"
        audios, texts, rate = [], [], 24_000
        for s in profile.samples:
            audio, rate = sf.read(s.audio_path, dtype="float32")
            if getattr(audio, "ndim", 1) > 1:
                audio = audio.mean(axis=1)  # to mono
            audios.append(audio)
            if s.reference_text:
                texts.append(s.reference_text)
        sf.write(out, np.concatenate(audios).astype(np.float32), rate)
        return str(out), " ".join(texts), rate

    def _profile_prompt(self, profile) -> dict:
        """EncoderOutput (as a CPU tensor dict) for a profile, cached."""
        import torch

        key = profile.id
        if key in self._prompts:
            return self._prompts[key]
        cache = self._voices_dir / f"{key}_tada.pt"
        if cache.exists():
            self._prompts[key] = torch.load(cache, map_location="cpu", weights_only=False)
            return self._prompts[key]

        import soundfile as sf

        wav_path, ref_text, _rate = self._combine_samples(profile)
        audio_np, sr = sf.read(wav_path, dtype="float32")
        audio = torch.from_numpy(audio_np).float()
        audio = audio.unsqueeze(0) if audio.ndim == 1 else audio.T
        audio = audio.to(self._torch_device())

        # force-aligned encode; without a transcript the encoder falls back
        # to its built-in ASR (English only). inference_mode is load-bearing:
        # tada's modules don't guard autograd themselves, and tracked
        # activations for a 30 s reference are gigabytes.
        with torch.inference_mode():
            prompt = self._encoder(
                audio, text=[ref_text] if ref_text else None, sample_rate=sr
            )

        prompt_dict = {}
        for field_name in prompt.__dataclass_fields__:
            val = getattr(prompt, field_name)
            prompt_dict[field_name] = (
                val.detach().cpu() if isinstance(val, torch.Tensor) else val
            )
        torch.save(prompt_dict, cache)
        self._prompts[key] = prompt_dict
        return prompt_dict

    # --- synthesis --------------------------------------------------------

    async def synthesize_profile(self, profile, text: str, instruct: str = "") -> tuple:
        await self.load()
        prompt_dict = await asyncio.to_thread(self._profile_prompt, profile)

        def _run(chunk_text: str) -> tuple[np.ndarray, int]:
            import torch
            from tada.modules.encoder import EncoderOutput

            device = self._torch_device()
            model_dtype = next(self._model.parameters()).dtype
            restored = {}
            for k, v in prompt_dict.items():
                if isinstance(v, torch.Tensor):
                    restored[k] = v.to(
                        device=device,
                        dtype=model_dtype if v.is_floating_point() else v.dtype,
                    )
                else:
                    restored[k] = v
            # inference_mode: see _profile_prompt — without it every chunk
            # leaks its activation graph into VRAM
            with torch.inference_mode():
                output = self._model.generate(
                    prompt=EncoderOutput(**restored), text=chunk_text
                )
            if output.audio and output.audio[0] is not None:
                audio = output.audio[0].detach().cpu().float().numpy()
                return np.asarray(audio, dtype=np.float32).reshape(-1), 24_000
            log.warning("TADA produced no audio for chunk: %r", chunk_text[:60])
            return np.array([], dtype=np.float32), 24_000

        log.info(
            "synthesize_profile [tada %s] (%s): %r", self.model_size, profile.id, text[:60]
        )
        try:
            return await chunking.synthesize_chunked(_run, text, log=log, label="tada")
        finally:
            # return the generation burst to the driver; weights stay resident
            empty_device_cache()

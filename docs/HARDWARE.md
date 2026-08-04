# Syrinx — Hardware acceleration

Syrinx runs on one codebase across very different machines and auto-adapts.
Three reference targets:

| Machine | Display | Compute for TTS/STT |
|---------|---------|---------------------|
| Laptop  | Intel iGPU (UHD/Xe) | CPU (torch cpu build) |
| Desktop | 14900K iGPU **or** RTX 4090 | **RTX 4090, CUDA** (Ada / sm_89) |
| Mac     | Apple M3 (Retina native) | **MPS** (unified memory, 24 GB) |

Everything below the model layer already works on the CPU box: Kokoro presets,
LuxTTS cloning (faster than realtime), faster-whisper STT, the Qwen3
personality LLM, and pedalboard effects. The GPU's job is the heavier cloning
engines (Qwen-TTS, Chatterbox, TADA) and faster everything else.

## Engine backend selection

`backends/__init__.py::detect_device()` picks `cuda` / `rocm` / `mps` / `cpu`
from what torch can see; the active backend is surfaced via the `Backend`
D-Bus property. A shared `torch_device()` mapping turns the backend name into
what torch actually addresses (`rocm` → `"cuda"`, `mps` → `"mps"`).
Isolated-venv workers (LuxTTS, Seed-VC, Vevo) detect their own device the same
way. Seed-VC is the odd one out: its package picks a device at import time, so
the worker re-pins `seed_vc.api._device` / `seed_vc.inference.device` rather
than passing one in (the package is GPL and stays unpatched).

Environment overrides (all optional):

| Variable | Effect |
|----------|--------|
| `SYRINX_DATA_DIR` | Data root (profiles, history db, models). Default `~/.local/share/syrinx`. |
| `SYRINX_TTS_ENGINE` | Clone-engine override (`luxtts` / `qwen`) — normally set live via the Models tab. |
| `SYRINX_MODEL` | Qwen-TTS model tier. |
| `SYRINX_WHISPER_MODEL` | faster-whisper size (default `base.en`). |
| `SYRINX_LLM_MODEL` | Personality/refinement LLM (default Qwen3 `1.7B`). |
| `SYRINX_LUXTTS_DEVICE` | Force the LuxTTS worker onto `cpu` / `cuda` / `mps`. |
| `SYRINX_SEEDVC_DEVICE` | Force the Seed-VC worker (models + demucs) onto `cpu` / `cuda` / `mps`. |
| `SYRINX_VEVO_DEVICE` | Force the Vevo worker (both pipelines + demucs) onto `cpu` / `cuda` / `mps`. |
| `SYRINX_TTS_CHUNK_CHARS` | Long-text chunk size for cloning engines (default 800). |
| `SYRINX_DICTATE_REFINE` | `1` = dictation pill always runs the LLM cleanup pass. |

## Long text is chunked, not scaled by hardware

Every engine (LuxTTS, Qwen, Kokoro) synthesizes long text in sentence-boundary
chunks (crossfaded at the joins) because synthesis memory grows steeply with
target duration — flow-matching in LuxTTS, autoregressive decode in Qwen; an
unchunked 2-minute text once ballooned the LuxTTS worker to ~14 GB on a
15 GB box. Chunking caps peak memory regardless of RAM/VRAM, so the default
stays the same on every machine.

## CUDA (RTX 4090) fast path

Apply in the GPU backends as they land (see `backends/qwen.py`):

```python
import torch
torch.backends.cuda.matmul.allow_tf32 = True   # free matmul speedup
torch.backends.cudnn.allow_tf32 = True
torch.backends.cuda.enable_flash_sdp(True)      # flash attention
device = "cuda"
# inference under: torch.autocast("cuda", dtype=torch.bfloat16)   # Ada tensor cores
# optional: model = torch.compile(model)                          # Inductor JIT
```

- bf16 autocast: ~2× faster, half the VRAM (24 GB to spare).
- The hot engine keeps weights resident across requests — load cost is paid
  once, every generation after that is instant.
- LuxTTS on GPU: CUDA torch + a matching k2 wheel in `.venv-luxtts`; the worker
  then picks CUDA automatically.

## Apple silicon (MPS)

Plain-pip torch on arm64 macOS ships MPS — no special index, no extra
packages, and no `PYTORCH_ENABLE_MPS_FALLBACK` needed for anything Syrinx
runs. What lands where on the M3:

- **Kokoro, Qwen TTS, LuxTTS worker, personality LLM: MPS.** The LLM loads
  fp16; LuxTTS's worker has its own `cuda > mps > cpu` rung (warm synthesis
  2.1 s vs 3.4 s on CPU).
- **Qwen-TTS must load bf16 on MPS, not fp16** — fp16 overflows in the code
  predictor's sampling ("probability tensor contains inf/nan"). One shared
  `_load_checkpoint()` owns the per-device dtype, so every Qwen-family
  backend inherits the right one.
- **STT stays on CPU** (int8): CTranslate2 has no MPS backend. Still snappy.
- **Seed-VC / Vevo workers are not yet MPS-tuned** — they install and run,
  but fall back to CPU/fp32 paths until their workers grow the same rung.
- `detect_hardware` reports the chip name ("Apple M3") and treats
  `torch.mps.recommended_max_memory()` as the VRAM figure (~17.8 GB of the
  24 GB unified pool) — the Models tab's VRAM advisories work unchanged.

## STT

**faster-whisper** (CTranslate2) runs in the engine venv on every box.
Desktop: switch to `large-v3` via the Models tab for accuracy in <1 s.
Laptop and Mac: `base.en` on CPU stays snappy (CT2 has no MPS backend).

## Arch / CachyOS packages

- **CUDA set:** `cuda`, `cudnn`, `nvidia` (555+ for Wayland), `python-pytorch-cuda`.
- **CPU set:** `python-pytorch` (or CachyOS `-opt`).

The PKGBUILD offers these as alternative dependency sets.

## NVIDIA + Wayland (display only)

The 4090 accelerating TTS is pure CUDA compute — unaffected by Wayland. The only
Wayland consideration is if the 4090 also drives the display for the Slint UI
(want driver 555+; explicit sync is automatic). **Cleanest desktop setup:** let
the 14900K iGPU drive the display and keep the 4090 as headless compute — avoids
NVIDIA/Wayland display quirks entirely while the 4090 does all the ML.

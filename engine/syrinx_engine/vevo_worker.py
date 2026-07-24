"""Standalone Vevo worker — runs inside the isolated .venv-vevo.

Style-preserved voice conversion via Amphion (MIT code; the checkpoints are
CC-BY-NC — personal use, auto-downloaded, never redistributed). Amphion has
no pip package, so its clone (setup-vevo.sh) is put on sys.path and we call
its public pipelines; cwd moves INTO the clone because Amphion resolves
config paths and its ./ckpts download directory relative to cwd — never
this repo.

Two pipelines share the worker, one resident at a time (they are several GB
each on the GPU):
  - speech: Vevo-Timbre (models/vc/vevo) — timbre-only delivery keeper
  - music : Vevo2 FM    (models/svc/vevo2) — singing conversion of a demucs
            vocal stem, remixed over the instrumental (♫ mode)

Reads one JSON request per line on stdin, writes raw float32 PCM to a temp
file, and replies on stdout.

Request : {"id": N, "source": "<src.wav>", "target": "<ref.wav>", "steps": 32}
          {"id": N, "cmd": "music", "source": …, "target": …, "steps": 32,
           "semitone": 0}   — demucs split → Vevo2 convert vocals → remix
Reply   : {"id": N, "stage": "separating"|"converting"|"remixing"}  (interim)
          {"id": N, "ok": true, "raw": "<path>", "rate": 24000}
          {"id": N, "ok": false, "error": "..."}
"""

import json
import os
import sys
import tempfile
from pathlib import Path

# Runs under the isolated .venv-vevo, so reach the shared per-OS path resolver
# by putting the engine root on sys.path (paths.py is stdlib-only on Linux).
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))  # .../engine
from syrinx_engine.paths import data_dir  # noqa: E402

# Reserve the real stdout for the JSON protocol (model downloads and torch
# both chat on fd 1 otherwise).
_PROTO = os.fdopen(os.dup(1), "w")
os.dup2(2, 1)
sys.stdout = sys.stderr

_AMPHION = os.environ.get(
    "SYRINX_VEVO_AMPHION",
    str(data_dir() / "vevo" / "Amphion"),
)
os.chdir(_AMPHION)
sys.path.insert(0, _AMPHION)

import numpy as np  # noqa: E402 — after the stdout/path setup above

_PIPELINE = None
_PIPELINE_KIND = None  # "timbre" (speech) | "vevo2" (♫ music)

VEVO_SR = 24_000  # both pipelines synthesize at 24 kHz

# The FM-only subset of RMSnow/Vevo2 (~3 GB) — the AR stacks (6+ GB) are for
# lyrics-driven synthesis and never load here. Keep in sync with the
# "vevo2-singing" catalog entry in models.py.
VEVO2_PATTERNS = [
    "tokenizer/contentstyle_fvq16384_12.5hz/*",
    "acoustic_modeling/fm_emilia101k_singnet7k_repa/*",
    "vocoder/*",
]

# ♫ chunking: Vevo2's FM transformer has no length guard and degrades far
# past its training length (it "gives up" mid-song on whole stems) — and the
# timbre reference codes PREFIX every sequence, so the ref is capped too.
# Slices crossfade at the joins, seed-vc style.
VEVO2_CHUNK_SECS = float(os.environ.get("SYRINX_VEVO2_CHUNK_SECS", "20"))
VEVO2_OVERLAP_SECS = 0.6
VEVO2_REF_SECS = 10.0


def _xfade_concat(parts: list, ov: int) -> "np.ndarray":
    """Concatenate with an equal-power crossfade over *ov* samples."""
    out = parts[0]
    for nxt in parts[1:]:
        n = min(ov, len(out), len(nxt))
        if n == 0:
            out = np.concatenate([out, nxt])
            continue
        t = np.linspace(0.0, np.pi / 2, n, dtype=np.float32)
        blend = out[-n:] * np.cos(t) + nxt[:n] * np.sin(t)
        out = np.concatenate([out[:-n], blend, nxt[n:]])
    return out


def _free_gpu() -> None:
    import gc

    import torch

    gc.collect()
    if torch.cuda.is_available():
        torch.cuda.empty_cache()


def _drop_pipeline() -> None:
    global _PIPELINE, _PIPELINE_KIND
    if _PIPELINE is not None:
        print(f"vevo-worker: dropping {_PIPELINE_KIND} pipeline", file=sys.stderr, flush=True)
    _PIPELINE = None
    _PIPELINE_KIND = None
    _free_gpu()


def _load(kind: str):
    """Load the requested pipeline, dropping the other one first (VRAM)."""
    global _PIPELINE, _PIPELINE_KIND
    if _PIPELINE is not None and _PIPELINE_KIND == kind:
        return
    _drop_pipeline()

    import torch
    from huggingface_hub import snapshot_download

    device = torch.device("cuda") if torch.cuda.is_available() else torch.device("cpu")
    print(f"vevo-worker: downloading/loading {kind} models on {device}...", file=sys.stderr, flush=True)
    if kind == "timbre":
        from models.vc.vevo.vevo_utils import VevoInferencePipeline

        # Same three components as Amphion's infer_vevotimbre.py, but in the
        # STANDARD HF cache (not the clone's ./ckpts) so the Models tab's
        # download/status/delete machinery covers the same files.
        tokenizer_dir = snapshot_download(
            repo_id="amphion/Vevo", allow_patterns=["tokenizer/vq8192/*"],
        )
        fmt_dir = snapshot_download(
            repo_id="amphion/Vevo", allow_patterns=["acoustic_modeling/Vq8192ToMels/*"],
        )
        vocoder_dir = snapshot_download(
            repo_id="amphion/Vevo", allow_patterns=["acoustic_modeling/Vocoder/*"],
        )
        _PIPELINE = VevoInferencePipeline(
            content_style_tokenizer_ckpt_path=os.path.join(tokenizer_dir, "tokenizer/vq8192"),
            fmt_cfg_path="./models/vc/vevo/config/Vq8192ToMels.json",
            fmt_ckpt_path=os.path.join(fmt_dir, "acoustic_modeling/Vq8192ToMels"),
            vocoder_cfg_path="./models/vc/vevo/config/Vocoder.json",
            vocoder_ckpt_path=os.path.join(vocoder_dir, "acoustic_modeling/Vocoder"),
            device=device,
        )
    else:
        from models.svc.vevo2.vevo2_utils import Vevo2InferencePipeline

        # FM-only Vevo2 (no AR model); configs ship inside the snapshot.
        # Its Coco tokenizer also pulls whisper-medium (~1.5 GB) into
        # ~/.cache/whisper on first load.
        local = snapshot_download(repo_id="RMSnow/Vevo2", allow_patterns=VEVO2_PATTERNS)
        _PIPELINE = Vevo2InferencePipeline(
            content_style_tokenizer_ckpt_path=os.path.join(
                local, "tokenizer/contentstyle_fvq16384_12.5hz"
            ),
            fmt_cfg_path=os.path.join(
                local, "acoustic_modeling/fm_emilia101k_singnet7k_repa/config.json"
            ),
            fmt_ckpt_path=os.path.join(local, "acoustic_modeling/fm_emilia101k_singnet7k_repa"),
            vocoder_cfg_path=os.path.join(local, "vocoder/config.json"),
            vocoder_ckpt_path=os.path.join(local, "vocoder"),
            device=device,
        )
    _PIPELINE_KIND = kind
    print("vevo-worker: models loaded", file=sys.stderr, flush=True)


def _stage(rid, name: str) -> None:
    """Interim progress line — the backend forwards it to GenerationProgress."""
    _PROTO.write(json.dumps({"id": rid, "stage": name}) + "\n")
    _PROTO.flush()


def _handle(req: dict) -> dict:
    """Speech: source → reference timbre via Vevo-Timbre."""
    import torch

    rid = req.get("id")
    steps = int(req.get("steps", 32))
    _load("timbre")
    print(f"vevo-worker: converting (steps={steps})", file=sys.stderr, flush=True)
    gen_audio = _PIPELINE.inference_fm(          # [1, T] float tensor
        src_wav_path=req["source"],
        timbre_ref_wav_path=req["target"],
        flow_matching_steps=steps,
    )
    # Amphion's save_audio needs torchcodec on modern torchaudio just to
    # write a wav — we only need the samples, so apply its −25 dB RMS
    # normalization ourselves and skip the file format entirely.
    wav = gen_audio.detach().float().cpu()
    rms = torch.sqrt(torch.mean(wav**2))
    gain_db = -25.0 - float(20 * torch.log10(rms + 1e-9))
    wav = (wav * (10 ** (gain_db / 20))).squeeze().numpy()
    out = os.path.join(tempfile.gettempdir(), f"vevo-{rid}.raw")
    np.ascontiguousarray(wav, dtype=np.float32).tofile(out)
    return {"id": rid, "ok": True, "raw": out, "rate": VEVO_SR}


def _handle_music(req: dict) -> dict:
    """Song cover: demucs vocal split → Vevo2 singing conversion → remix."""
    import librosa
    import soundfile as sf
    import torch

    rid = req.get("id")
    steps = int(req.get("steps", 32))
    semitone = int(req.get("semitone", 0))

    # the speech pipeline (if resident) goes first — demucs needs the room
    if _PIPELINE_KIND != "vevo2":
        _drop_pipeline()

    _stage(rid, "separating")
    from demucs.api import Separator

    sep = Separator(model="htdemucs", device="cuda" if torch.cuda.is_available() else "cpu")
    print("vevo-worker: demucs separating...", file=sys.stderr, flush=True)
    _origin, stems = sep.separate_audio_file(req["source"])
    demucs_sr = int(sep.samplerate)
    vocals = stems["vocals"].mean(dim=0).numpy()
    inst = sum(
        stems[name] for name in stems if name != "vocals"
    ).mean(dim=0).numpy()
    del sep, stems, _origin
    _free_gpu()

    _stage(rid, "converting")
    _load("vevo2")
    if semitone:
        # register wrangling happens on the stem the model hears — Vevo2's
        # own use_pitch_shift stays OFF (it re-registers to the reference's
        # median pitch, which lands the vocal in a different key)
        vocals = librosa.effects.pitch_shift(vocals, sr=demucs_sr, n_steps=float(semitone))
    # slice the stem (overlapping), convert each, crossfade at 24 kHz
    chunk = max(1, int(VEVO2_CHUNK_SECS * demucs_sr))
    ov_src = int(VEVO2_OVERLAP_SECS * demucs_sr)
    slices, start = [], 0
    while start < len(vocals):
        end = min(len(vocals), start + chunk)
        slices.append(vocals[start:end])
        if end >= len(vocals):
            break
        start = end - ov_src
    stem_path = os.path.join(tempfile.gettempdir(), f"vevo-stem-{rid}.wav")
    parts = []
    try:
        for i, piece in enumerate(slices):
            print(
                f"vevo-worker: chunk {i + 1}/{len(slices)} (steps={steps})",
                file=sys.stderr, flush=True,
            )
            sf.write(stem_path, piece.astype(np.float32), demucs_sr, subtype="PCM_16")
            gen_audio = _PIPELINE.inference_fm(
                src_wav_path=stem_path,
                timbre_ref_wav_path=req["target"],
                use_pitch_shift=False,  # keep the song's key
                used_duration_of_timbre_ref_wav_path=VEVO2_REF_SECS,
                flow_matching_steps=steps,
            )
            parts.append(gen_audio.detach().float().cpu().squeeze().numpy())
    finally:
        Path(stem_path).unlink(missing_ok=True)
    conv = _xfade_concat(parts, int(VEVO2_OVERLAP_SECS * VEVO_SR))

    _stage(rid, "remixing")
    if VEVO_SR != demucs_sr:
        conv = librosa.resample(conv, orig_sr=VEVO_SR, target_sr=demucs_sr)
    # sit the converted vocal at the original stem's level so the vocal /
    # instrumental balance survives conversion
    rms_orig = float(np.sqrt(np.mean(vocals**2)))
    rms_conv = float(np.sqrt(np.mean(conv**2)))
    if rms_orig > 1e-6 and rms_conv > 1e-6:
        conv = conv * (rms_orig / rms_conv)
    n = min(len(conv), len(inst))
    mix = (conv[:n] + inst[:n]).astype(np.float32)
    peak = float(np.abs(mix).max()) or 1.0
    if peak > 0.99:
        mix = mix / peak * 0.99
    out = os.path.join(tempfile.gettempdir(), f"vevo-{rid}.raw")
    np.ascontiguousarray(mix, dtype=np.float32).tofile(out)
    return {"id": rid, "ok": True, "raw": out, "rate": demucs_sr}


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        rid = None
        try:
            req = json.loads(line)
            rid = req.get("id")
            resp = _handle_music(req) if req.get("cmd") == "music" else _handle(req)
        except Exception as e:  # noqa: BLE001
            print(f"vevo-worker: error {e}", file=sys.stderr, flush=True)
            resp = {"id": rid, "ok": False, "error": str(e)}
        _PROTO.write(json.dumps(resp) + "\n")
        _PROTO.flush()


if __name__ == "__main__":
    main()

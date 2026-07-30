"""Standalone LuxTTS worker — runs inside the isolated .venv-luxtts.

Reads one JSON request per line on stdin, synthesizes with zipvoice LuxTTS,
writes raw float32 PCM to a temp file, and replies with a JSON line on stdout.
Deliberately independent of the syrinx_engine package: it only imports zipvoice
+ numpy, which is all the lux venv has.

Request : {"id": N, "sample": "<ref.wav>", "text": "<text>"}
Reply   : {"id": N, "ok": true, "raw": "<path>", "rate": 48000}
          {"id": N, "ok": false, "error": "..."}
"""

import json
import os
import sys
import tempfile

# Reserve the real stdout for the JSON protocol: zipvoice/transformers print
# progress lines ("Loading model on CPU", Whisper transcripts) straight to fd 1,
# which would corrupt the line protocol. Keep a private dup for replies and
# point fd 1 (and sys.stdout) at stderr so all stray output lands in the log.
_PROTO = os.fdopen(os.dup(1), "w")
os.dup2(2, 1)
sys.stdout = sys.stderr

import numpy as np  # noqa: E402 — after the stdout/path setup above

_MODEL = None
_PROMPTS = {}  # sample_path -> encoded prompt


def _device() -> str:
    """cuda > mps > cpu — the same order as the main venv's backends.

    CUDA when the venv's torch can see a GPU (the 4090); Metal on Apple
    silicon, where the wheel this venv installs (cpu index or plain PyPI — on
    macOS they are the same arm64 build) has a working MPS backend. Measured on
    an M3, 2026-07-30: a one-sentence clip takes 2.1 s on mps against 3.4 s on
    cpu, with no unimplemented-operator fallbacks in the LuxTTS graph.
    Overridable via SYRINX_LUXTTS_DEVICE, which is also how a future bad Metal
    kernel gets worked around without reinstalling the venv. The AttributeError
    guard mirrors llm.py: a torch too old to know about Metal has no
    ``backends.mps`` to ask.
    """
    env = os.environ.get("SYRINX_LUXTTS_DEVICE", "")
    if env:
        return env
    try:
        import torch

        if torch.cuda.is_available():
            return "cuda"
        try:
            if torch.backends.mps.is_available():
                return "mps"
        except AttributeError:  # torch too old to know about Metal
            pass
    except Exception:  # noqa: BLE001
        pass
    return "cpu"


def _load():
    global _MODEL
    if _MODEL is not None:
        return
    from zipvoice.luxvoice import LuxTTS

    device = _device()
    threads = min(os.cpu_count() or 4, 8)
    print(f"luxtts-worker: loading model on {device}...", file=sys.stderr, flush=True)
    _MODEL = LuxTTS(model_path="YatharthS/LuxTTS", device=device, threads=threads)
    print("luxtts-worker: model loaded", file=sys.stderr, flush=True)


def _synthesize(text: str, encode_dict: dict, speed: float):
    wav = _MODEL.generate_speech(
        text=text,
        encode_dict=encode_dict,
        num_steps=4,
        guidance_scale=3.0,
        t_shift=0.5,
        speed=speed,
        return_smooth=False,  # 48 kHz
    )
    try:
        import torch

        if isinstance(wav, torch.Tensor):
            wav = wav.detach().cpu().numpy()
    except Exception:
        pass
    return np.asarray(wav).squeeze().astype(np.float32)


def _handle(req: dict) -> dict:
    rid = req.get("id")
    sample = req["sample"]
    text = req["text"]
    _load()
    if sample not in _PROMPTS:
        _PROMPTS[sample] = _MODEL.encode_prompt(prompt_audio=str(sample), duration=5, rms=0.01)

    # The duration predictor collapses on short texts: below ~7 latent frames the
    # vocoder's conv kernel raises, and slightly above it the clip comes out
    # implausibly truncated. Lower speed stretches the predicted duration, so
    # retry down a speed ladder until the output is at least plausible speech.
    min_samples = int(0.04 * len(text) * 48000)
    audio = None
    last_err = None
    for speed in (1.0, 0.6, 0.4):
        try:
            audio = _synthesize(text, _PROMPTS[sample], speed)
        except RuntimeError as e:
            if "padded input size" not in str(e):
                raise
            last_err = e
            continue
        if audio.size >= min_samples:
            break
    if audio is None:
        raise last_err
    out = os.path.join(tempfile.gettempdir(), f"luxtts-{rid}.raw")
    audio.tofile(out)
    return {"id": rid, "ok": True, "raw": out, "rate": 48000}


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        rid = None
        try:
            req = json.loads(line)
            rid = req.get("id")
            resp = _handle(req)
        except Exception as e:  # noqa: BLE001
            print(f"luxtts-worker: error {e}", file=sys.stderr, flush=True)
            resp = {"id": rid, "ok": False, "error": str(e)}
        _PROTO.write(json.dumps(resp) + "\n")
        _PROTO.flush()


if __name__ == "__main__":
    main()

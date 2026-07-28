#!/usr/bin/env bash
# Set up the isolated Vevo venv (engine/.venv-vevo) + Amphion clone for the
# ⇄ tab's Vevo-Timbre conversion engine. Safe to re-run; picks CUDA torch
# when an NVIDIA GPU is present, CPU wheels otherwise.
#
# Amphion is MIT code but has NO pip package — it is cloned OUTSIDE the repo
# (default ~/.local/share/syrinx/vevo/Amphion, override SYRINX_VEVO_AMPHION)
# and vevo_worker.py imports its modules from there. Checkpoints (CC-BY-NC —
# personal use, never redistribute) auto-download on first conversion into
# ./ckpts inside that clone.
#
# Amphion pins torch==2.0.1 (no py3.12 wheels); we run the modern stack and
# pin only what proved load-bearing. Verification imports the real pipeline,
# so a bad combination fails HERE, not at first conversion.
#
# The app can run this script itself (Models tab → Install). Two seams make that
# possible without changing anything for a human running it by hand:
#   * SYRINX_VC_VENV_DIR relocates the venv. UNSET is the historical behavior —
#     the :+ expansion below collapses to the bare literal, so every path and
#     command below is byte-identical to what Linux has always run.
#   * the syrinx-stage lines are machine-readable phase markers the engine maps
#     to human labels; the prose "==" banners stay exactly as they were.
set -euo pipefail
cd "$(dirname "$0")"

VENV="${SYRINX_VC_VENV_DIR:+$SYRINX_VC_VENV_DIR/}.venv-vevo"

echo "== syrinx-stage: amphion"
AMPHION_DIR="${SYRINX_VEVO_AMPHION:-$HOME/.local/share/syrinx/vevo/Amphion}"
if [ ! -d "$AMPHION_DIR/.git" ]; then
    mkdir -p "$(dirname "$AMPHION_DIR")"
    git clone --depth 1 https://github.com/open-mmlab/Amphion.git "$AMPHION_DIR"
else
    git -C "$AMPHION_DIR" pull --ff-only || echo "== Amphion pull failed (offline?) — using existing clone"
fi

echo "== syrinx-stage: venv"
PY="${SYRINX_VEVO_PYTHON:-python3.12}"
"$PY" -m venv "$VENV"
"$VENV"/bin/pip install -U pip

echo "== syrinx-stage: torch"
if command -v nvidia-smi > /dev/null 2>&1 && nvidia-smi > /dev/null 2>&1; then
    echo "== NVIDIA GPU detected — installing CUDA torch (default PyPI build)"
    "$VENV"/bin/pip install torch torchaudio
else
    echo "== no GPU — installing CPU torch"
    "$VENV"/bin/pip install torch torchaudio \
        --index-url https://download.pytorch.org/whl/cpu
fi

# models/vc/vevo/requirements.txt minus dev/app extras (gradio, spaces,
# black) and CJK text-frontend packages Vevo-Timbre never touches; hub<1.0
# matches the transformers 4.x era (same lesson as the seed-vc venv).
# transformers is 4.57 NOT Amphion's pinned 4.41: their main-branch code
# builds LlamaRotaryEmbedding(config=…), an API that postdates their own pin.
echo "== syrinx-stage: deps"
"$VENV"/bin/pip install \
    'numpy==1.26.*' 'scipy==1.12.*' 'transformers==4.57.3' \
    'accelerate==0.24.1' 'huggingface_hub<1.0' \
    librosa soundfile encodec unidecode json5 ruamel.yaml tqdm \
    onnxruntime 'setuptools<81' safetensors openai-whisper phonemizer g2p_en \
    ipython \
    pyworld einops \
    demucs torchvision praat-parselmouth torchcrepe
# ipython: vevo_utils has a notebook-era top-level IPython import.
# pyworld/einops: pulled by Amphion codec modules vevo_utils imports.
# setuptools<81: pyworld's __init__ imports pkg_resources, removed in newer
# setuptools (same pin as the main venv's resemble-perth).
# demucs/torchvision: ♫ music mode — demucs splits the vocal stem and
# Vevo2's vevo2_utils has a top-level torchvision import.
# parselmouth/torchcrepe: vevo2_utils → evaluation.metrics.f0 → utils.f0,
# both undeclared by Amphion (the setup-time import below proves the set).

echo "== syrinx-stage: verify"
SYRINX_VEVO_AMPHION="$AMPHION_DIR" "$VENV"/bin/python - <<'EOF'
import os
import sys

amphion = os.environ["SYRINX_VEVO_AMPHION"]
os.chdir(amphion)
sys.path.insert(0, amphion)
import torch
from demucs.api import Separator  # noqa: F401 — ♫ music mode's stem splitter
from models.vc.vevo.vevo_utils import VevoInferencePipeline  # noqa: F401
from models.svc.vevo2.vevo2_utils import Vevo2InferencePipeline  # noqa: F401 — ♫

print(
    "vevo venv OK · torch", torch.__version__,
    "· cuda", torch.cuda.is_available(),
    "· amphion", amphion,
)
EOF

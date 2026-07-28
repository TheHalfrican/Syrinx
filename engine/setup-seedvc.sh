#!/usr/bin/env bash
# Set up the isolated Seed-VC venv (engine/.venv-seedvc) for the ⇄ tab's
# Seed-VC conversion engine. Safe to re-run; picks CUDA torch when an NVIDIA
# GPU is present, CPU wheels otherwise.
#
# Seed-VC is GPL-3.0 — it is installed HERE, never vendored into the repo;
# the engine talks to it through seedvc_worker.py in a subprocess. Weights
# auto-download on first conversion to ~/.local/share/syrinx/seedvc (the
# worker's pinned cwd — seed-vc writes ./checkpoints relative to cwd).
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

VENV="${SYRINX_VC_VENV_DIR:+$SYRINX_VC_VENV_DIR/}.venv-seedvc"

echo "== syrinx-stage: venv"
PY="${SYRINX_SEEDVC_PYTHON:-python3.12}"
"$PY" -m venv "$VENV"
"$VENV"/bin/pip install -U pip

echo "== syrinx-stage: torch"
if command -v nvidia-smi > /dev/null 2>&1 && nvidia-smi > /dev/null 2>&1; then
    echo "== NVIDIA GPU detected — installing CUDA torch (default PyPI build)"
    "$VENV"/bin/pip install torch torchaudio torchvision
else
    echo "== no GPU — installing CPU torch"
    "$VENV"/bin/pip install torch torchaudio torchvision \
        --index-url https://download.pytorch.org/whl/cpu
fi

echo "== syrinx-stage: seedvc"
"$VENV"/bin/pip install seed-vc

# music mode: demucs separates the vocal stem inside this same worker venv
echo "== syrinx-stage: demucs"
"$VENV"/bin/pip install demucs

# huggingface_hub 1.x removed the proxies/resume_download kwargs that
# BigVGAN's _from_pretrained (inside seed-vc) still requires, and
# transformers 5.x demands hub>=1.5 — pin the pair to the 4.x era.
echo "== syrinx-stage: pins"
"$VENV"/bin/pip install 'transformers==4.57.3' 'huggingface_hub<1.0'

echo "== syrinx-stage: verify"
"$VENV"/bin/python - <<'EOF'
import huggingface_hub, numpy, seed_vc, torch, transformers
from demucs.api import Separator  # noqa: F401 — music mode's stem splitter
print(
    "seed-vc venv OK",
    "· torch", torch.__version__,
    "· transformers", transformers.__version__,
    "· hub", huggingface_hub.__version__,
    "· numpy", numpy.__version__,
    "· cuda", torch.cuda.is_available(),
)
EOF

# Set up the isolated Vevo venv (engine/.venv-vevo) + Amphion clone for the
# ⇄ tab's Vevo-Timbre conversion engine. PowerShell 7 port of setup-vevo.sh.
# Safe to re-run; picks CUDA torch when an NVIDIA GPU is present, CPU otherwise.
#
# ─────────────────────────────────────────────────────────────────────────────
# setup-vevo.sh IS THE REFERENCE. Linux is the reference platform; if a pin
# needs to change, change it in setup-vevo.sh FIRST, then mirror it here. A
# pin-drift guard (engine/tests/test_setup_pins.py) fails CI if the two scripts'
# version pins diverge, so this port cannot silently rot.
#
# Three DELIBERATE per-OS divergences (not pin changes, so the guard ignores them):
#   1. Torch index: on Linux plain `pip install torch` is the CUDA build; on
#      Windows plain pip torch is CPU-only, so the CUDA branch here uses the
#      cu130 wheel index (matches engine/.venv's torch 2.13.0+cu130).
#   2. The venv interpreter lives in Scripts\python.exe, not bin/python.
#   3. The Amphion clone default location follows paths.py's per-OS data dir:
#      %LOCALAPPDATA%\syrinx\syrinx\vevo\Amphion (where vevo_worker.py looks),
#      not ~/.local/share/syrinx/vevo/Amphion. SYRINX_VEVO_AMPHION overrides.
#
# Installer: prefers `uv` when on PATH (matches the Linux 4090 box per
# HANDOFF-4090 — much faster resolves/builds), falls back to plain pip when uv
# is absent. Both honor the identical pin strings, so the .sh↔.ps1 pin-drift
# guard (test_setup_pins.py) is unaffected.
# ─────────────────────────────────────────────────────────────────────────────
#
# Amphion is MIT code but has NO pip package — it is cloned OUTSIDE the repo
# and vevo_worker.py imports its modules from there. Checkpoints (CC-BY-NC —
# personal use, never redistribute) auto-download on first conversion into
# ./ckpts inside that clone.
#
# Amphion pins torch==2.0.1 (no py3.12 wheels); we run the modern stack and
# pin only what proved load-bearing. Verification imports the real pipeline,
# so a bad combination fails HERE, not at first conversion.

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-Location -LiteralPath $PSScriptRoot

function Invoke-Checked {
    param([Parameter(Mandatory, ValueFromRemainingArguments)] [string[]] $Args)
    & $Args[0] @($Args[1..($Args.Count - 1)])
    if ($LASTEXITCODE -ne 0) {
        throw "command failed ($LASTEXITCODE): $($Args -join ' ')"
    }
}

# Prefer uv (fast) when on PATH; fall back to the venv's pip. Same pins either
# way, so the .sh↔.ps1 pin-drift guard is unaffected.
$UV = (Get-Command uv -ErrorAction SilentlyContinue).Source

function Install-Pkgs {
    param([Parameter(ValueFromRemainingArguments)] [string[]] $PipArgs)
    if ($UV) {
        Invoke-Checked $UV pip install --python $PY @PipArgs
    } else {
        Invoke-Checked $PY -m pip install @PipArgs
    }
}

# Amphion clone location: override wins; else follow paths.py's data dir
# (SYRINX_DATA_DIR override, else %LOCALAPPDATA%\syrinx\syrinx) + \vevo\Amphion.
if ($env:SYRINX_VEVO_AMPHION) {
    $AMPHION_DIR = $env:SYRINX_VEVO_AMPHION
} elseif ($env:SYRINX_DATA_DIR) {
    $AMPHION_DIR = Join-Path $env:SYRINX_DATA_DIR 'vevo\Amphion'
} else {
    $AMPHION_DIR = Join-Path $env:LOCALAPPDATA 'syrinx\syrinx\vevo\Amphion'
}

if (-not (Test-Path -LiteralPath (Join-Path $AMPHION_DIR '.git'))) {
    $parent = Split-Path -Parent $AMPHION_DIR
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    Invoke-Checked git clone --depth 1 https://github.com/open-mmlab/Amphion.git $AMPHION_DIR
} else {
    & git -C $AMPHION_DIR pull --ff-only
    if ($LASTEXITCODE -ne 0) { Write-Host '== Amphion pull failed (offline?) — using existing clone' }
}

# Create the venv. SYRINX_VEVO_PYTHON overrides the interpreter (a full path
# to python.exe); otherwise the Windows `py` launcher selects 3.12.
if ($env:SYRINX_VEVO_PYTHON) {
    Invoke-Checked $env:SYRINX_VEVO_PYTHON -m venv .venv-vevo
} else {
    Invoke-Checked py -3.12 -m venv .venv-vevo
}

$PY = Join-Path $PSScriptRoot '.venv-vevo\Scripts\python.exe'
if ($UV) { Write-Host "== using uv ($UV)" } else { Write-Host '== uv not found — using pip' }
if (-not $UV) { Invoke-Checked $PY -m pip install -U pip }

$hasGpu = $false
if (Get-Command nvidia-smi -ErrorAction SilentlyContinue) {
    & nvidia-smi *> $null
    if ($LASTEXITCODE -eq 0) { $hasGpu = $true }
}

if ($hasGpu) {
    Write-Host '== NVIDIA GPU detected — installing CUDA torch (cu130 wheel index)'
    # Windows divergence: plain pip torch is CPU here, so pin the CUDA index.
    Install-Pkgs torch torchaudio `
        --index-url https://download.pytorch.org/whl/cu130
} else {
    Write-Host '== no GPU — installing CPU torch'
    Install-Pkgs torch torchaudio `
        --index-url https://download.pytorch.org/whl/cpu
}

# models/vc/vevo/requirements.txt minus dev/app extras (gradio, spaces,
# black) and CJK text-frontend packages Vevo-Timbre never touches; hub<1.0
# matches the transformers 4.x era (same lesson as the seed-vc venv).
# transformers is 4.57 NOT Amphion's pinned 4.41: their main-branch code
# builds LlamaRotaryEmbedding(config=…), an API that postdates their own pin.
Install-Pkgs `
    'numpy==1.26.*' 'scipy==1.12.*' 'transformers==4.57.3' `
    'accelerate==0.24.1' 'huggingface_hub<1.0' `
    librosa soundfile encodec unidecode json5 ruamel.yaml tqdm `
    onnxruntime 'setuptools<81' safetensors openai-whisper phonemizer g2p_en `
    ipython `
    pyworld einops `
    demucs torchvision praat-parselmouth torchcrepe
# ipython: vevo_utils has a notebook-era top-level IPython import.
# pyworld/einops: pulled by Amphion codec modules vevo_utils imports.
# setuptools<81: pyworld's __init__ imports pkg_resources, removed in newer
# setuptools (same pin as the main venv's resemble-perth).
# demucs/torchvision: ♫ music mode — demucs splits the vocal stem and
# Vevo2's vevo2_utils has a top-level torchvision import.
# parselmouth/torchcrepe: vevo2_utils → evaluation.metrics.f0 → utils.f0,
# both undeclared by Amphion (the setup-time import below proves the set).

# Setup-time import proof: imports the real pipeline so a bad combination fails
# HERE, not at first conversion. Written to a temp file and executed.
$proof = @'
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
'@
$proofPath = Join-Path $env:TEMP "vevo_import_proof_$PID.py"
Set-Content -LiteralPath $proofPath -Value $proof -Encoding UTF8
$env:SYRINX_VEVO_AMPHION = $AMPHION_DIR
try {
    Invoke-Checked $PY $proofPath
} finally {
    Remove-Item -LiteralPath $proofPath -Force -ErrorAction SilentlyContinue
}

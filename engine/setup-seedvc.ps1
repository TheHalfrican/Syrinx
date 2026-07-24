# Set up the isolated Seed-VC venv (engine/.venv-seedvc) for the ⇄ tab's
# Seed-VC conversion engine. PowerShell 7 port of setup-seedvc.sh. Safe to
# re-run; picks CUDA torch when an NVIDIA GPU is present, CPU wheels otherwise.
#
# ─────────────────────────────────────────────────────────────────────────────
# setup-seedvc.sh IS THE REFERENCE. Linux is the reference platform; if a pin
# needs to change, change it in setup-seedvc.sh FIRST, then mirror it here. A
# pin-drift guard (engine/tests/test_setup_pins.py) fails CI if the two scripts'
# version pins diverge, so this port cannot silently rot.
#
# Two DELIBERATE per-OS divergences (not pin changes, so the guard ignores them):
#   1. Torch index: on Linux plain `pip install torch` is the CUDA build; on
#      Windows plain pip torch is CPU-only, so the CUDA branch here uses the
#      cu130 wheel index (matches engine/.venv's torch 2.13.0+cu130).
#   2. The venv interpreter lives in Scripts\python.exe, not bin/python.
#
# Installer: prefers `uv` when on PATH (matches how the Linux 4090 box is built
# per HANDOFF-4090 — dramatically faster resolves/builds), falls back to plain
# pip when uv is absent. Both honor the identical pin strings, so the .sh↔.ps1
# pin-drift guard (test_setup_pins.py) is unaffected either way.
# ─────────────────────────────────────────────────────────────────────────────
#
# Seed-VC is GPL-3.0 — it is installed HERE, never vendored into the repo;
# the engine talks to it through seedvc_worker.py in a subprocess. Weights
# auto-download on first conversion to %LOCALAPPDATA%\syrinx\syrinx\seedvc (the
# worker's pinned cwd — seed-vc writes ./checkpoints relative to cwd).

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-Location -LiteralPath $PSScriptRoot

function Invoke-Checked {
    # Run a native command and abort if it exits non-zero (set -e equivalent).
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

# Create the venv. SYRINX_SEEDVC_PYTHON overrides the interpreter (a full path
# to python.exe); otherwise the Windows `py` launcher selects 3.12.
if ($env:SYRINX_SEEDVC_PYTHON) {
    Invoke-Checked $env:SYRINX_SEEDVC_PYTHON -m venv .venv-seedvc
} else {
    Invoke-Checked py -3.12 -m venv .venv-seedvc
}

$PY = Join-Path $PSScriptRoot '.venv-seedvc\Scripts\python.exe'
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
    Install-Pkgs torch torchaudio torchvision `
        --index-url https://download.pytorch.org/whl/cu130
} else {
    Write-Host '== no GPU — installing CPU torch'
    Install-Pkgs torch torchaudio torchvision `
        --index-url https://download.pytorch.org/whl/cpu
}

Install-Pkgs seed-vc

# music mode: demucs separates the vocal stem inside this same worker venv
Install-Pkgs demucs

# huggingface_hub 1.x removed the proxies/resume_download kwargs that
# BigVGAN's _from_pretrained (inside seed-vc) still requires, and
# transformers 5.x demands hub>=1.5 — pin the pair to the 4.x era.
Install-Pkgs 'transformers==4.57.3' 'huggingface_hub<1.0'

# Setup-time import proof: a bad combination must fail HERE, not at first
# conversion. Written to a temp file and executed (avoids piping a script to
# python's stdin, which on Windows can race numpy's C-extension DLL load).
$proof = @'
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
'@
$proofPath = Join-Path $env:TEMP "seedvc_import_proof_$PID.py"
Set-Content -LiteralPath $proofPath -Value $proof -Encoding UTF8
try {
    Invoke-Checked $PY $proofPath
} finally {
    Remove-Item -LiteralPath $proofPath -Force -ErrorAction SilentlyContinue
}

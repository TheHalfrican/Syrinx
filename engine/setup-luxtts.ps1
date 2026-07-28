# Set up the isolated LuxTTS venv (engine\.venv-luxtts) for the LuxTTS cloning
# voice. PowerShell 7 port of setup-luxtts.sh. Safe to re-run; picks CUDA torch
# when an NVIDIA GPU is present, CPU wheels otherwise.
#
# ─────────────────────────────────────────────────────────────────────────────
# setup-luxtts.sh IS THE REFERENCE. Linux is the reference platform; if a pin
# needs to change, change it in setup-luxtts.sh FIRST, then mirror it here. A
# pin-drift guard (engine/tests/test_setup_pins.py) fails CI if the two scripts'
# version pins — or their two git SHAs — diverge, so this port cannot silently
# rot.
#
# Two DELIBERATE per-OS divergences (not pin changes, so the guard ignores them):
#   1. Torch index: on Linux plain `pip install torch` is the CUDA build; on
#      Windows plain pip torch is CPU-only, so the CUDA branch here uses the
#      cu130 wheel index (matches engine\.venv's torch 2.13.0+cu130).
#   2. The venv interpreter lives in Scripts\python.exe, not bin/python.
#
# Note what is NOT a divergence: the piper_phonemize find-links index is on both
# scripts. Windows is the OS that cannot survive without it, but the same fork
# index serves manylinux wheels, and keeping one pin for both OSes is the whole
# point of the drift guard.
#
# Installer: prefers `uv` when on PATH (matches how the Linux 4090 box is built
# per HANDOFF-4090 — dramatically faster resolves/builds), falls back to plain
# pip when uv is absent. Both honor the identical pin strings, so the .sh↔.ps1
# pin-drift guard is unaffected either way.
# ─────────────────────────────────────────────────────────────────────────────
#
# LuxTTS is Apache-2.0, but it has no usable release: the `zipvoice` name on
# PyPI is an unrelated stub with no `luxvoice` module in it, so the real thing
# can only come from git — which is why this setup declares needs_git and the
# app bootstraps Git before running it. It is installed HERE, never vendored
# into the repo; the engine talks to it through luxtts_worker.py in a
# subprocess. Weights auto-download on first synthesis into the shared Hugging
# Face cache.

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

# Where the venv goes. SYRINX_VC_VENV_DIR is how the app relocates it out of an
# installed tree: the engine ships at engine\.venv\Lib\site-packages, and a venv
# nested under THAT overflows MAX_PATH the moment pip unpacks torch. Unset means
# beside this script, which is what a developer running it by hand has always got.
$VenvRoot = if ($env:SYRINX_VC_VENV_DIR) { $env:SYRINX_VC_VENV_DIR } else { $PSScriptRoot }
$Venv = Join-Path $VenvRoot '.venv-luxtts'

# Create the venv. SYRINX_LUXTTS_PYTHON overrides the interpreter (a full path
# to python.exe); otherwise the Windows `py` launcher selects 3.12.
Write-Host '== syrinx-stage: venv'
if ($env:SYRINX_LUXTTS_PYTHON) {
    Invoke-Checked $env:SYRINX_LUXTTS_PYTHON -m venv $Venv
} else {
    Invoke-Checked py -3.12 -m venv $Venv
}

$PY = Join-Path $Venv 'Scripts\python.exe'
if ($UV) { Write-Host "== using uv ($UV)" } else { Write-Host '== uv not found — using pip' }
if (-not $UV) { Invoke-Checked $PY -m pip install -U pip }

Write-Host '== syrinx-stage: torch'
# No torchvision, unlike the seed-vc and vevo venvs: nothing in the LuxTTS
# import graph reaches for it, and it is a gigabyte of wheels either way.
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

# The espeak phonemizer, installed BEFORE LuxTTS on purpose — the same resolver
# trick this script's seedvc sibling plays with webrtcvad. LuxTTS depends on
# piper-phonemize, whose PyPI upstream has been unmaintained since 2023 and
# ships no wheel for Windows at all; left alone, pip fetches that sdist and
# tries to compile C++ and espeak-ng, which is the one thing a stock Windows box
# with no MSVC cannot do — and it would take the whole one-click install down
# with it. The find-links index below is csukuangfj's (k2-fsa) maintained fork,
# carrying cp37–cp314 win_amd64 wheels, and each wheel bundles the espeak-ng
# DLLs AND its data directory, so the venv is self-contained and nothing has to
# be installed system-wide. Doing it first means that by the time LuxTTS's
# dependency graph is resolved pip already sees piper-phonemize satisfied and
# never reaches for the dead upstream.
# If that index ever goes away, the fallback is PyPI's `piper-phonemize-fix`.
Write-Host '== syrinx-stage: phonemize'
Install-Pkgs `
    --find-links https://k2-fsa.github.io/icefall/piper_phonemize.html `
    'piper_phonemize==1.4.7'

# ysharma3501/LuxTTS is the REAL LuxTTS. Both repos are hard-pinned to the SHAs
# verified on 2026-07-28 because neither publishes tags or releases — a bare
# branch would be an unpinned dependency that could change the worker's import
# surface between one user's install and the next.
#
# LinaCodec goes first, and explicitly: LuxTTS declares it only through
# [tool.uv.sources], which pip does not read, so pip would go hunting for
# `linacodec` on PyPI and fail. Installing it from git first satisfies that
# requirement before the resolver ever asks the index about it.
#
# Deliberately NO --find-links on this stage: pip leaves an already-satisfied
# piper-phonemize alone, whereas re-offering the fork index here could resolve
# *up* off the 1.4.7 pin the stage above just placed.
Write-Host '== syrinx-stage: luxtts'
Install-Pkgs `
    'git+https://github.com/ysharma3501/LinaCodec.git@c0ae7c7285e121475c27592cfbb600624b714290'
Install-Pkgs `
    'git+https://github.com/ysharma3501/LuxTTS.git@28ae6a61151684fffc9d1a7aa15eafa02286fe0b'

# Restated at top level for the same reason the seed-vc venv restates its pair:
# whatever the luxtts resolve happened to land on, the constraint stays visible
# here and gets corrected back. transformers 5.x drops tokenizer APIs zipvoice
# still calls, and setuptools 81 removed pkg_resources, which several of the
# audio dependencies still import at module scope.
Write-Host '== syrinx-stage: pins'
Install-Pkgs 'transformers<=4.57.6' 'setuptools<81'

Write-Host '== syrinx-stage: verify'
# Setup-time import proof: a bad combination must fail HERE, not at the user's
# first synthesis. It imports exactly what luxtts_worker.py imports, and prints
# phoneme COUNTS rather than the phonemes themselves — the IPA is non-ASCII and
# this console's cp1252 stdout raises UnicodeEncodeError on it, which would turn
# a perfectly healthy install into a failed one. The count still proves the
# espeak data files inside the wheel were found and returned real output.
# Written to a temp file and executed (avoids piping a script to python's stdin,
# which on Windows can race a C-extension DLL load).
$proof = @'
import torch
from piper_phonemize import phonemize_espeak
from zipvoice.luxvoice import LuxTTS  # noqa: F401 — what luxtts_worker.py imports

ipa = phonemize_espeak("hello world", "en-us")
print(
    "luxtts venv OK",
    "· torch", torch.__version__,
    "· cuda", torch.cuda.is_available(),
    "· espeak phonemes", sum(len(s) for s in ipa),
)
'@
$proofPath = Join-Path $env:TEMP "luxtts_import_proof_$PID.py"
Set-Content -LiteralPath $proofPath -Value $proof -Encoding UTF8
try {
    Invoke-Checked $PY $proofPath
} finally {
    Remove-Item -LiteralPath $proofPath -Force -ErrorAction SilentlyContinue
}

#!/usr/bin/env pwsh
#Requires -Version 5
#
# Syrinx first-run bootstrap (MULTIPLATPLAN §2.2 — "CUDA torch pulled on first
# run"). The shipped bundle is torch-free; this pulls the ML stack into the
# bundled embedded-python environment on the target machine, with progress
# visible. Run once after install (Start-Menu "Syrinx first-run setup"), and
# safely re-runnable.
#
#   Right-click -> Run with PowerShell, or:
#     powershell -ExecutionPolicy Bypass -File syrinx-firstrun.ps1
#     syrinx-firstrun.ps1 -Cpu        # force CPU torch (also the no-GPU path)
#
# What it does NOT do: bundle or install Seed-VC (GPL-3.0) or the Amphion clone
# (Vevo). Those install on demand via engine/setup-seedvc.* / setup-vevo.* into
# their own isolated venvs, exactly as on Linux — license boundary preserved.

[CmdletBinding()]
param([switch]$Cpu)   # force CPU torch regardless of nvidia-smi

$ErrorActionPreference = 'Stop'

$Here    = Split-Path -Parent $MyInvocation.MyCommand.Path
$PyExe   = Join-Path $Here 'engine\.venv\python.exe'
$Tools   = Join-Path $Here 'tools'
$Wheel   = Get-ChildItem (Join-Path $Here 'engine\wheels') -Filter 'syrinx_engine-*.whl' -EA SilentlyContinue | Select-Object -First 1
$Marker  = Join-Path $Here 'engine\.venv\.syrinx-firstrun-done'

function Log ($m) { Write-Host "`n==> $m" -ForegroundColor Cyan }
function Ok  ($m) { Write-Host "  ok $m" -ForegroundColor Green }
function Die ($m) { Write-Host "error: $m" -ForegroundColor Red; Read-Host 'Press Enter to close'; exit 1 }

if (-not (Test-Path $PyExe)) { Die "embedded python missing at $PyExe — reinstall Syrinx" }

# sox on PATH so the qwen-tts import check below can succeed.
$env:PATH = "$Tools;$env:PATH"

# ---- CUDA vs CPU ----------------------------------------------------------
$useCuda = $false
if (-not $Cpu) {
    $null = & { nvidia-smi 2>$null }
    if ($LASTEXITCODE -eq 0) { $useCuda = $true }
}
Log ("Target backend: " + ($(if ($useCuda) {'CUDA (nvidia-smi found a GPU)'} else {'CPU'})))

# ---- regenerate the engine console script for THIS install location -------
# pip's Windows console-script .exe embeds an absolute interpreter path at build
# time; a --force-reinstall here rewrites Scripts\syrinx-engine.exe to point at
# the python.exe at the *installed* path (the bundle was built elsewhere).
if ($Wheel) {
    Log 'Repairing engine launcher for this install path'
    & $PyExe -m pip install --no-warn-script-location --no-deps --force-reinstall $Wheel.FullName
    if ($LASTEXITCODE -ne 0) { Die 'engine relaunch-repair failed' }
    Ok 'Scripts\syrinx-engine.exe'
}

# ---- torch ----------------------------------------------------------------
if ($useCuda) {
    # cu130 index — Linux parity; the cu128 index tops out at torch 2.11
    # (2026-07-24 finding). Windows default-PyPI torch is CPU-only, so the
    # index-url is mandatory here.
    Log 'Installing CUDA torch (cu130)'
    & $PyExe -m pip install --no-warn-script-location torch torchaudio `
        --index-url https://download.pytorch.org/whl/cu130
} else {
    Log 'Installing CPU torch'
    & $PyExe -m pip install --no-warn-script-location torch torchaudio `
        --index-url https://download.pytorch.org/whl/cpu
}
if ($LASTEXITCODE -ne 0) { Die 'torch install failed' }
Ok 'torch'

# ---- ML stack: base ML deps + qwen extra ----------------------------------
# Installing "<wheel>[qwen]" pulls the base ML deps that the bundle left out
# (kokoro / misaki / faster-whisper / pedalboard) plus the qwen extra
# (qwen-tts, transformers). torch is already satisfied above, so pip's default
# only-if-needed strategy leaves the CUDA/CPU build in place.
Log 'Installing the ML stack (kokoro, faster-whisper, pedalboard, qwen-tts, numba)'
$mlSpec = if ($Wheel) { "$($Wheel.FullName)[qwen]" } else { 'kokoro>=0.9.2', 'misaki[en]', 'faster-whisper', 'pedalboard', 'qwen-tts>=0.0.5', 'transformers>=4.36.0' }
& $PyExe -m pip install --no-warn-script-location @mlSpec 'numba>=0.60'
if ($LASTEXITCODE -ne 0) { Die 'ML stack install failed' }

if ($useCuda) {
    # ctranslate2 (faster-whisper) needs cublas64_12.dll; torch's bundled cuDNN
    # is cu13 and MUST win, so ONLY cublas goes near PATH (stt.py does the
    # os.add_dll_directory dance). We only install the wheels here.
    Log 'Installing CUDA runtime wheels for ctranslate2 (cublas + cudnn cu12)'
    & $PyExe -m pip install --no-warn-script-location nvidia-cublas-cu12 nvidia-cudnn-cu12
    if ($LASTEXITCODE -ne 0) { Die 'nvidia cublas/cudnn install failed' }
}
Ok 'ML stack'

# ---- import proof (fail at setup, not at first conversion) ----------------
Log 'Proof: import the ML stack'
& $PyExe -c "import torch, kokoro, faster_whisper, pedalboard, sox; print('torch', torch.__version__, '| cuda', torch.cuda.is_available())"
if ($LASTEXITCODE -ne 0) { Die 'ML import proof failed' }

Set-Content $Marker ("firstrun ok backend=" + ($(if ($useCuda) {'cuda'} else {'cpu'})) + " " + (Get-Date -Format o)) -Encoding ascii
Write-Host "`nSyrinx is ready. Launch it from the Start Menu (Syrinx)." -ForegroundColor Green
Write-Host "Voice-conversion engines (Seed-VC / Vevo) install on demand from the app." -ForegroundColor DarkGray

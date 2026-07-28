#!/usr/bin/env bash
# Set up the isolated LuxTTS venv (engine/.venv-luxtts) for the LuxTTS cloning
# voice. Safe to re-run; picks CUDA torch when an NVIDIA GPU is present, CPU
# wheels otherwise.
#
# LuxTTS is Apache-2.0, but it has no usable release: the `zipvoice` name on
# PyPI is an unrelated stub with no `luxvoice` module in it, so the real thing
# can only come from git. It is installed HERE, never vendored into the repo;
# the engine talks to it through luxtts_worker.py in a subprocess, which does
# exactly one import off the package (`from zipvoice.luxvoice import LuxTTS`).
# Its own deps conflict with the main engine venv's (transformers<=4.57,
# torchaudio 2.11), which is why this venv exists at all. Weights auto-download
# on first synthesis into the shared Hugging Face cache.
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

VENV="${SYRINX_VC_VENV_DIR:+$SYRINX_VC_VENV_DIR/}.venv-luxtts"

echo "== syrinx-stage: venv"
PY="${SYRINX_LUXTTS_PYTHON:-python3.12}"
"$PY" -m venv "$VENV"
"$VENV"/bin/pip install -U pip

echo "== syrinx-stage: torch"
# No torchvision, unlike the seed-vc and vevo venvs: nothing in the LuxTTS
# import graph reaches for it, and it is a gigabyte of wheels either way.
if command -v nvidia-smi > /dev/null 2>&1 && nvidia-smi > /dev/null 2>&1; then
    echo "== NVIDIA GPU detected — installing CUDA torch (default PyPI build)"
    "$VENV"/bin/pip install torch torchaudio
else
    echo "== no GPU — installing CPU torch"
    "$VENV"/bin/pip install torch torchaudio \
        --index-url https://download.pytorch.org/whl/cpu
fi

# The espeak phonemizer, installed BEFORE LuxTTS on purpose — the same resolver
# trick setup-seedvc.ps1 plays with webrtcvad. LuxTTS depends on
# piper-phonemize, whose PyPI upstream has been unmaintained since 2023 and
# ships no wheel for Windows at all; left alone, pip fetches that sdist and
# tries to compile C++ and espeak-ng, which is the one thing a stock box cannot
# do. The find-links index below is csukuangfj's (k2-fsa) maintained fork,
# carrying cp37–cp314 wheels for win_amd64 and manylinux, and each wheel bundles
# the espeak-ng shared libraries AND its data directory, so the venv is
# self-contained and nothing has to be installed system-wide. Doing it first
# means that by the time LuxTTS's dependency graph is resolved pip already sees
# piper-phonemize satisfied and never reaches for the dead upstream.
# If that index ever goes away, the fallback is PyPI's `piper-phonemize-fix`.
echo "== syrinx-stage: phonemize"
"$VENV"/bin/pip install \
    --find-links https://k2-fsa.github.io/icefall/piper_phonemize.html \
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
echo "== syrinx-stage: luxtts"
"$VENV"/bin/pip install \
    'git+https://github.com/ysharma3501/LinaCodec.git@c0ae7c7285e121475c27592cfbb600624b714290'
"$VENV"/bin/pip install \
    'git+https://github.com/ysharma3501/LuxTTS.git@28ae6a61151684fffc9d1a7aa15eafa02286fe0b'

# Restated at top level for the same reason the seed-vc venv restates its pair:
# whatever the luxtts resolve happened to land on, the constraint stays visible
# here and gets corrected back. transformers 5.x drops tokenizer APIs zipvoice
# still calls, and setuptools 81 removed pkg_resources, which several of the
# audio dependencies still import at module scope.
echo "== syrinx-stage: pins"
"$VENV"/bin/pip install 'transformers<=4.57.6' 'setuptools<81'

echo "== syrinx-stage: verify"
# Setup-time import proof: a bad combination must fail HERE, not at the user's
# first synthesis. It imports exactly what luxtts_worker.py imports, and prints
# phoneme COUNTS rather than the phonemes themselves — the IPA is non-ASCII and
# Windows' cp1252 stdout raises UnicodeEncodeError on it, which would turn a
# perfectly healthy install into a failed one. The count still proves the
# espeak data files inside the wheel were found and returned real output.
"$VENV"/bin/python - <<'EOF'
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
EOF

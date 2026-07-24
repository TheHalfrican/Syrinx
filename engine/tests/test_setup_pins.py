"""Pin-drift guard for the isolated-venv setup scripts.

Each worker venv is set up by a Bash script (the Linux reference) AND a
PowerShell port for Windows. The version pins in the two MUST stay identical —
a pin that drifts between them is exactly the kind of silent rot that only
surfaces as a runtime ImportError on one OS. This test parses both members of
each pair and asserts their version-pin sets match, so CI fails the moment they
diverge.

It intentionally compares only *version-constrained* tokens (``pkg==x``,
``pkg<y`` …). Unpinned packages, the torch wheel index (cu130 on Windows vs the
default CUDA build on Linux), the venv interpreter path, and the Amphion clone
location are all deliberate per-OS divergences and carry no version, so they are
not part of the pin set by construction.

Stdlib-only (re + pathlib) — this file runs in the torch-free CI test job.
"""

import re
from pathlib import Path

_ENGINE_DIR = Path(__file__).resolve().parents[1]

# name<op>version, e.g. transformers==4.57.3, huggingface_hub<1.0, numpy==1.26.*
# Two-char operators come first in the alternation so `<=` never matches as `<`.
_PIN_RE = re.compile(r"[A-Za-z0-9_.\-]+(?:==|<=|>=|!=|<|>)[0-9][A-Za-z0-9.*]*")


def _strip_comments(text: str) -> str:
    """Drop each line's ``#``-to-EOL comment (both sh and ps1 use ``#``), so a
    version literal mentioned in prose (e.g. the ``torch==2.0.1`` Amphion pins
    in a comment) can never be mistaken for a real pin."""
    out = []
    for line in text.splitlines():
        hash_at = line.find("#")
        out.append(line if hash_at == -1 else line[:hash_at])
    return "\n".join(out)


def _pins(script: Path) -> set[str]:
    return set(_PIN_RE.findall(_strip_comments(script.read_text(encoding="utf-8"))))


def _pair(stem: str) -> tuple[set[str], set[str]]:
    sh = _pins(_ENGINE_DIR / f"{stem}.sh")
    ps1 = _pins(_ENGINE_DIR / f"{stem}.ps1")
    return sh, ps1


def test_seedvc_setup_scripts_exist():
    assert (_ENGINE_DIR / "setup-seedvc.sh").is_file()
    assert (_ENGINE_DIR / "setup-seedvc.ps1").is_file()


def test_vevo_setup_scripts_exist():
    assert (_ENGINE_DIR / "setup-vevo.sh").is_file()
    assert (_ENGINE_DIR / "setup-vevo.ps1").is_file()


def test_seedvc_pins_match():
    sh, ps1 = _pair("setup-seedvc")
    # sanity: parsing actually found the known load-bearing pins
    assert "transformers==4.57.3" in sh
    assert "huggingface_hub<1.0" in sh
    assert sh == ps1, f"seed-vc pin drift: only in .sh={sh - ps1}, only in .ps1={ps1 - sh}"


def test_vevo_pins_match():
    sh, ps1 = _pair("setup-vevo")
    expected = {
        "numpy==1.26.*",
        "scipy==1.12.*",
        "transformers==4.57.3",
        "accelerate==0.24.1",
        "huggingface_hub<1.0",
        "setuptools<81",
    }
    assert expected <= sh, f"vevo .sh lost expected pins: {expected - sh}"
    assert sh == ps1, f"vevo pin drift: only in .sh={sh - ps1}, only in .ps1={ps1 - sh}"

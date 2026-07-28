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


# The phase markers the engine's installer reads off stdout to drive the app's
# stage text. Comments are stripped first for the same reason the pin scan does
# it: a marker quoted in prose is documentation, not a marker.
_STAGE_RE = re.compile(r"==\s*syrinx-stage:\s*([A-Za-z0-9_.-]+)")


def _stages(script: Path) -> set[str]:
    return set(_STAGE_RE.findall(_strip_comments(script.read_text(encoding="utf-8"))))


def test_seedvc_setup_scripts_exist():
    assert (_ENGINE_DIR / "setup-seedvc.sh").is_file()
    assert (_ENGINE_DIR / "setup-seedvc.ps1").is_file()


def test_vevo_setup_scripts_exist():
    assert (_ENGINE_DIR / "setup-vevo.sh").is_file()
    assert (_ENGINE_DIR / "setup-vevo.ps1").is_file()


def test_luxtts_setup_scripts_exist():
    assert (_ENGINE_DIR / "setup-luxtts.sh").is_file()
    assert (_ENGINE_DIR / "setup-luxtts.ps1").is_file()


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


def test_luxtts_pins_match():
    sh, ps1 = _pair("setup-luxtts")
    expected = {
        # the whole reason LuxTTS is installable on Windows at all: PyPI's
        # piper-phonemize upstream is dead and ships no win_amd64 wheel, so this
        # exact version comes off the k2-fsa fork's find-links index
        "piper_phonemize==1.4.7",
        "transformers<=4.57.6",
        "setuptools<81",
    }
    assert expected <= sh, f"luxtts .sh lost expected pins: {expected - sh}"
    assert sh == ps1, f"luxtts pin drift: only in .sh={sh - ps1}, only in .ps1={ps1 - sh}"


# LinaCodec/LuxTTS are pinned as `…​.git@<40 hex>` fragments, which carry no
# name<op>version token and are therefore invisible to _PIN_RE above. They are
# still pins — arguably the load-bearing ones, since neither project publishes a
# release — so they get their own equality guard.
_SHA_RE = re.compile(r"git\+https://\S+?\.git@([0-9a-f]{40})")


def test_luxtts_git_shas_match():
    sh = set(_SHA_RE.findall(_strip_comments(
        (_ENGINE_DIR / "setup-luxtts.sh").read_text(encoding="utf-8"))))
    ps1 = set(_SHA_RE.findall(_strip_comments(
        (_ENGINE_DIR / "setup-luxtts.ps1").read_text(encoding="utf-8"))))
    expected = {
        "c0ae7c7285e121475c27592cfbb600624b714290",  # ysharma3501/LinaCodec main
        "28ae6a61151684fffc9d1a7aa15eafa02286fe0b",  # ysharma3501/LuxTTS master
    }
    assert sh == expected, f"luxtts .sh git pins moved: {sh ^ expected}"
    assert sh == ps1, f"luxtts SHA drift: only in .sh={sh - ps1}, only in .ps1={ps1 - sh}"


# --- one-click install seams (engine/syrinx_engine/vcsetup.py drives these) ---


_EXPECTED_STAGES = {
    "setup-seedvc": {"venv", "torch", "seedvc", "demucs", "pins", "verify"},
    "setup-vevo": {"amphion", "venv", "torch", "deps", "verify"},
    # phonemize sits BEFORE luxtts on purpose (the resolver trick that keeps pip
    # away from piper-phonemize's dead PyPI upstream); this set is order-free, so
    # the ordering itself is documented in the scripts, not asserted here.
    "setup-luxtts": {"venv", "torch", "phonemize", "luxtts", "pins", "verify"},
}


def test_stage_markers_match_across_each_pair():
    """The app's progress text comes from these markers, so a phase added to one
    OS's script and not the other would silently stall the stage display there —
    the same class of rot the pin guard above catches."""
    for stem, expected in _EXPECTED_STAGES.items():
        sh = _stages(_ENGINE_DIR / f"{stem}.sh")
        ps1 = _stages(_ENGINE_DIR / f"{stem}.ps1")
        assert sh == expected, f"{stem}.sh stage drift: {sh ^ expected}"
        assert sh == ps1, (
            f"{stem} stage drift: only in .sh={sh - ps1}, only in .ps1={ps1 - sh}")


def test_every_script_honors_the_venv_dir_override():
    """Without this the Windows installer cannot move the venv off the deeply
    nested installed tree, and MAX_PATH kills the torch unpack."""
    for stem in _EXPECTED_STAGES:
        for ext in (".sh", ".ps1"):
            text = (_ENGINE_DIR / (stem + ext)).read_text(encoding="utf-8")
            assert "SYRINX_VC_VENV_DIR" in _strip_comments(text), f"{stem}{ext}"

"""luxtts_worker.py's device pick, exercised as a subprocess.

The worker dup2's stdout at import time (to keep zipvoice/Whisper chatter off
its JSON line protocol), so importing it in-process would hijack pytest's
stdout — the same reason test_seedvc_worker.py drives its worker as a child.
Here the child answers a single question and writes it to a file, which is
also the only channel the dup2 cannot swallow.

torch comes from a per-test stand-in on PYTHONPATH rather than tests/stubs:
this file needs a DIFFERENT torch per case (Metal present, Metal absent, a
build too old to have heard of Metal), and the real .venv-luxtts torch is not
installed on a CI box at all.
"""

import os
import subprocess
import sys
from pathlib import Path

import pytest

WORKER = Path(__file__).resolve().parents[1] / "syrinx_engine" / "luxtts_worker.py"

# Ask the worker its answer and put it somewhere the stdout redirect cannot eat.
_ASK = ("import luxtts_worker, pathlib, sys; "
        "pathlib.Path(sys.argv[1]).write_text(luxtts_worker._device())")

CUDA_AND_MPS = """
class _Cuda:
    @staticmethod
    def is_available():
        return {cuda}


class _Mps:
    @staticmethod
    def is_available():
        return {mps}


class _Backends:
    mps = _Mps()


cuda = _Cuda()
backends = _Backends()
"""

# A torch predating Metal support: no backends.mps to ask at all.
NO_MPS_ATTR = """
class _Cuda:
    @staticmethod
    def is_available():
        return False


cuda = _Cuda()
"""

UNIMPORTABLE = "raise ImportError('torch built for a different libc')\n"


def ask_device(tmp_path, torch_src, **env):
    """Run ``_device()`` against a stand-in torch; returns what it picked."""
    fake = tmp_path / "fake"
    (fake / "torch").mkdir(parents=True)
    (fake / "torch" / "__init__.py").write_text(torch_src, encoding="utf-8")
    answer = tmp_path / "device.txt"
    child = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "PYTHONPATH": os.pathsep.join([str(fake), str(WORKER.parent)]),
        **env,
    }
    out = subprocess.run([sys.executable, "-c", _ASK, str(answer)],
                         env=child, capture_output=True, text=True, timeout=120)
    assert out.returncode == 0, out.stderr
    return answer.read_text(encoding="utf-8")


def test_the_worker_picks_cuda_first(tmp_path):
    """The 4090 box: a discrete GPU outranks everything."""
    assert ask_device(tmp_path, CUDA_AND_MPS.format(cuda=True, mps=True)) == "cuda"


def test_the_worker_picks_metal_over_cpu(tmp_path):
    """Apple silicon. This venv's torch comes off plain PyPI (or the pytorch
    cpu index, which serves the same macosx_arm64 wheels), and that build has
    a working MPS backend — synthesis on cpu is minutes where mps is seconds."""
    assert ask_device(tmp_path, CUDA_AND_MPS.format(cuda=False, mps=True)) == "mps"


def test_the_worker_falls_back_to_cpu(tmp_path):
    assert ask_device(tmp_path, CUDA_AND_MPS.format(cuda=False, mps=False)) == "cpu"


def test_a_torch_that_never_heard_of_metal_is_not_a_crash(tmp_path):
    """AttributeError, not a missing GPU — the guard llm.py's _pick_device
    carries for the same reason."""
    assert ask_device(tmp_path, NO_MPS_ATTR) == "cpu"


def test_an_unimportable_torch_still_answers(tmp_path):
    """The device pick runs before the model load, so a broken venv must fail
    on the import that actually matters, with a message, not here."""
    assert ask_device(tmp_path, UNIMPORTABLE) == "cpu"


@pytest.mark.parametrize("forced", ["cpu", "cuda:1"])
def test_the_env_override_wins_outright(tmp_path, forced):
    """The escape hatch for a Metal kernel LuxTTS trips over: SYRINX_LUXTTS_DEVICE
    puts the user back on cpu without reinstalling the venv."""
    got = ask_device(tmp_path, CUDA_AND_MPS.format(cuda=True, mps=True),
                     SYRINX_LUXTTS_DEVICE=forced)
    assert got == forced

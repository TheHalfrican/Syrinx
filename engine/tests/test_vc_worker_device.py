"""The Seed-VC and Vevo workers' device picks, exercised as subprocesses.

Both workers dup2 stdout at import time (to keep HF download chatter off their
JSON line protocols), so importing them in-process would hijack pytest's
stdout — the same reason test_luxtts_worker.py drives its worker as a child.
The child answers one question and writes it to a file, which is also the only
channel the dup2 cannot swallow.

torch comes from a per-test stand-in on PYTHONPATH rather than tests/stubs:
these cases need a DIFFERENT torch each (Metal present, Metal absent, a build
too old to have heard of Metal), and neither .venv-seedvc nor .venv-vevo is
installed on a CI box at all.
"""

import os
import subprocess
import sys
from pathlib import Path

import pytest

WORKERS = Path(__file__).resolve().parents[1] / "syrinx_engine"

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


class device:
    def __init__(self, spec):
        self.spec = str(spec)
        self.type = self.spec.split(":")[0]

    def __str__(self):
        return self.spec


cuda = _Cuda()
backends = _Backends()
"""

# A torch predating Metal support: no backends.mps to ask at all.
NO_MPS_ATTR = """
class _Cuda:
    @staticmethod
    def is_available():
        return False


class device:
    def __init__(self, spec):
        self.spec = str(spec)


cuda = _Cuda()
"""

UNIMPORTABLE = "raise ImportError('torch built for a different libc')\n"

# Ask the worker its answer and put it somewhere the stdout redirect cannot eat.
_ASK = ("import {mod}, pathlib, sys; "
        "pathlib.Path(sys.argv[1]).write_text({mod}._device())")


def run_child(tmp_path, torch_src, snippet, **env):
    """Run *snippet* against a stand-in torch; returns what it wrote to argv[1]."""
    fake = tmp_path / "fake"
    if not fake.exists():
        (fake / "torch").mkdir(parents=True)
        (fake / "torch" / "__init__.py").write_text(torch_src, encoding="utf-8")
    answer = tmp_path / "answer.txt"
    child = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "PYTHONPATH": os.pathsep.join([str(fake), str(WORKERS)]),
        # vevo_worker chdirs into the Amphion clone at import; point it
        # somewhere that exists so the device pick is what is under test
        "SYRINX_VEVO_AMPHION": str(tmp_path),
        "HOME": str(tmp_path),
        **env,
    }
    out = subprocess.run([sys.executable, "-c", snippet, str(answer)],
                         env=child, capture_output=True, text=True, timeout=120)
    assert out.returncode == 0, out.stderr
    return answer.read_text(encoding="utf-8")


def ask_device(tmp_path, worker, torch_src, **env):
    return run_child(tmp_path, torch_src, _ASK.format(mod=worker), **env)


WORKER_MODULES = ["seedvc_worker", "vevo_worker"]
OVERRIDES = {"seedvc_worker": "SYRINX_SEEDVC_DEVICE", "vevo_worker": "SYRINX_VEVO_DEVICE"}


@pytest.mark.parametrize("worker", WORKER_MODULES)
def test_a_vc_worker_picks_cuda_first(tmp_path, worker):
    """The 4090 box: a discrete GPU outranks everything."""
    assert ask_device(tmp_path, worker, CUDA_AND_MPS.format(cuda=True, mps=True)) == "cuda"


@pytest.mark.parametrize("worker", WORKER_MODULES)
def test_a_vc_worker_picks_metal_over_cpu(tmp_path, worker):
    """Apple silicon. Measured on an M3: Seed-VC 9.7 s vs 23.3 s cpu on a 7 s
    clip, Vevo-Timbre 11.5 s vs 22.1 s — both worth having by default."""
    assert ask_device(tmp_path, worker, CUDA_AND_MPS.format(cuda=False, mps=True)) == "mps"


@pytest.mark.parametrize("worker", WORKER_MODULES)
def test_a_vc_worker_falls_back_to_cpu(tmp_path, worker):
    assert ask_device(tmp_path, worker, CUDA_AND_MPS.format(cuda=False, mps=False)) == "cpu"


@pytest.mark.parametrize("worker", WORKER_MODULES)
def test_a_torch_that_never_heard_of_metal_is_not_a_crash(tmp_path, worker):
    """AttributeError, not a missing GPU — the guard luxtts_worker carries."""
    assert ask_device(tmp_path, worker, NO_MPS_ATTR) == "cpu"


@pytest.mark.parametrize("worker", WORKER_MODULES)
def test_an_unimportable_torch_still_answers(tmp_path, worker):
    """The device pick runs before the model load, so a broken venv must fail
    on the import that actually matters, with a message, not here."""
    assert ask_device(tmp_path, worker, UNIMPORTABLE) == "cpu"


@pytest.mark.parametrize("worker", WORKER_MODULES)
@pytest.mark.parametrize("forced", ["cpu", "cuda:1"])
def test_the_env_override_wins_outright(tmp_path, worker, forced):
    """The escape hatch for a Metal kernel one of these graphs trips over: the
    env var puts the user back on cpu without reinstalling the venv."""
    got = ask_device(tmp_path, worker, CUDA_AND_MPS.format(cuda=True, mps=True),
                     **{OVERRIDES[worker]: forced})
    assert got == forced


# --- seed-vc's import-time device snapshots ------------------------------

_PIN = """
import sys, types, pathlib
mod_api = types.ModuleType("seed_vc.api")
mod_api._device = "ORIGINAL"
mod_inf = types.ModuleType("seed_vc.inference")
mod_inf.device = "ORIGINAL"
sys.modules["seed_vc.api"] = mod_api
sys.modules["seed_vc.inference"] = mod_inf
import seedvc_worker
seedvc_worker._pin_device("mps")
pathlib.Path(sys.argv[1]).write_text(f"{mod_api._device}|{mod_inf.device}")
"""

_PIN_MISSING = """
import sys, pathlib
sys.modules.pop("seed_vc.api", None)
sys.modules.pop("seed_vc.inference", None)
import seedvc_worker
seedvc_worker._pin_device("cpu")
pathlib.Path(sys.argv[1]).write_text("survived")
"""


def test_pinning_rebinds_both_seedvc_device_snapshots(tmp_path):
    """seed-vc reads its device at IMPORT time in two modules; the adapter has
    to re-point both or half the graph lands on the wrong device."""
    got = run_child(tmp_path, CUDA_AND_MPS.format(cuda=False, mps=True), _PIN)
    assert got == "mps|mps"


def test_pinning_a_seedvc_that_is_not_imported_yet_is_a_no_op(tmp_path):
    """_pin_device runs inside _ensure_state, which imports seed_vc.api first —
    but a future seed-vc that takes a device argument would have nothing to
    pin, and that must not be an error."""
    got = run_child(tmp_path, CUDA_AND_MPS.format(cuda=False, mps=True), _PIN_MISSING)
    assert got == "survived"


# --- the singing model's float64 f0 --------------------------------------

_F0 = """
import sys, pathlib, numpy as np


class _State:
    def f0_fn(self, wave, thred=0.03):
        return np.linspace(80.0, 400.0, 16, dtype=np.float64)


import seedvc_worker
state = _State()
seedvc_worker._f0_float32(state)
out = state.f0_fn(None, thred=0.03)
pathlib.Path(sys.argv[1]).write_text(f"{out.dtype}|{out[0]:.1f}|{out[-1]:.1f}")
"""

_F0_NONE = """
import sys, pathlib


class _State:
    f0_fn = None


import seedvc_worker
state = _State()
seedvc_worker._f0_float32(state)
pathlib.Path(sys.argv[1]).write_text(repr(state.f0_fn))
"""


def test_the_f0_track_is_cast_to_float32(tmp_path):
    """Metal has no float64 at all: seed-vc's RMVPE hands back float64 and the
    very next line moves it onto the device, which raises outright on mps."""
    got = run_child(tmp_path, CUDA_AND_MPS.format(cuda=False, mps=True), _F0)
    assert got == "float32|80.0|400.0"


def test_the_speech_model_has_no_f0_to_cast(tmp_path):
    """f0_fn is None without f0 conditioning — the wrapper must not invent one."""
    got = run_child(tmp_path, CUDA_AND_MPS.format(cuda=False, mps=True), _F0_NONE)
    assert got == "None"

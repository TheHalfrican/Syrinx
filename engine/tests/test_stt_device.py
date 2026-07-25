"""Which device faster-whisper gets loaded on.

CTranslate2 (faster-whisper's runtime) is CUDA-only — it has no ROCm backend —
but a ROCm torch reports ``torch.cuda.is_available()`` True for an AMD card.
So ``_load_sync`` has to look past that, exactly like ``detect_device()`` does,
or STT dies at load on an AMD box while every torch backend is happily on GPU.

Neither torch nor faster-whisper is in the CI dependency contract, so both are
stand-in modules in ``sys.modules`` (never imported at module scope), and the
WhisperModel stub records the constructor args the loader chose.
"""

import sys
import types

import pytest

from syrinx_engine import stt


class RecordingWhisperModel:
    """Stands in for faster_whisper.WhisperModel — remembers how it was built."""

    made = []

    def __init__(self, model_size, device=None, compute_type=None):
        self.model_size = model_size
        self.device = device
        self.compute_type = compute_type
        RecordingWhisperModel.made.append(self)


@pytest.fixture
def load(monkeypatch):
    """Run ``_load_sync`` against a fake faster-whisper and return the
    WhisperModel the loader constructed. *torch* is the module to install
    (``None`` = not importable)."""
    RecordingWhisperModel.made = []
    # The DLL staging is a real-filesystem win32 side trip and has nothing to do
    # with device choice.
    monkeypatch.setattr(stt, "_stage_ct2_cuda_dlls", lambda: None)
    monkeypatch.setitem(
        sys.modules, "faster_whisper",
        types.SimpleNamespace(WhisperModel=RecordingWhisperModel),
    )

    def _load(torch):
        monkeypatch.setitem(sys.modules, "torch", torch)
        t = stt.Transcriber()
        t._load_sync()
        assert t._model is RecordingWhisperModel.made[-1]
        return t._model

    return _load


def fake_torch(hip):
    return types.SimpleNamespace(
        cuda=types.SimpleNamespace(is_available=lambda: True),
        version=types.SimpleNamespace(hip=hip),
    )


def test_a_cuda_torch_loads_on_the_gpu_in_float16(load):
    model = load(fake_torch(hip=None))
    assert model.device == "cuda"
    assert model.compute_type == "float16"


def test_a_rocm_torch_falls_back_to_cpu(load):
    """HIP masquerades as torch.cuda, but CT2 can't use it — CPU/int8."""
    model = load(fake_torch(hip="6.2.41134"))
    assert model.device == "cpu"
    assert model.compute_type == "int8"


def test_without_torch_it_loads_on_cpu(load):
    model = load(None)  # sys.modules["torch"] = None -> ImportError
    assert model.device == "cpu"
    assert model.compute_type == "int8"

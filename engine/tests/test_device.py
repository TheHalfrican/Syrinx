"""Which torch device the engine picks, and what it calls it.

Three device families reach torch by three different names: a ROCm build is
addressed as "cuda", Metal is "mps", everything else is "cpu". torch is not in
the CI dependency contract, so every case here runs against a stand-in module
in ``sys.modules`` (the ``test_stt_device`` pattern) and therefore runs on
Linux, Windows and macOS alike.
"""

import sys
import types

import pytest

from syrinx_engine import backends, llm


def fake_torch(*, cuda=False, hip=None, mps=False, mps_raises=False, calls=None):
    """A torch stand-in with the four attributes the device paths touch."""
    calls = calls if calls is not None else []

    def mps_available():
        if mps_raises:
            raise RuntimeError("Metal driver unavailable")
        return mps

    return types.SimpleNamespace(
        cuda=types.SimpleNamespace(
            is_available=lambda: cuda,
            empty_cache=lambda: calls.append("cuda"),
        ),
        backends=types.SimpleNamespace(
            mps=types.SimpleNamespace(is_available=mps_available)
        ),
        mps=types.SimpleNamespace(empty_cache=lambda: calls.append("mps")),
        version=types.SimpleNamespace(hip=hip),
        calls=calls,
    )


@pytest.fixture
def torch(monkeypatch):
    def _install(**kwargs):
        module = fake_torch(**kwargs)
        monkeypatch.setitem(sys.modules, "torch", module)
        return module

    return _install


# --- detect_device --------------------------------------------------------


def test_metal_is_detected_as_mps(torch):
    torch(mps=True)
    assert backends.detect_device() == "mps"


def test_cuda_outranks_mps(torch):
    """A box with both (eGPU / CUDA under Rosetta) takes the tuned path."""
    torch(cuda=True, mps=True)
    assert backends.detect_device() == "cuda"


def test_rocm_outranks_mps(torch):
    torch(cuda=True, hip="6.2.41134", mps=True)
    assert backends.detect_device() == "rocm"


def test_no_accelerator_is_cpu(torch):
    torch()
    assert backends.detect_device() == "cpu"


def test_a_raising_mps_probe_falls_back_to_cpu(torch):
    # A broken Metal driver must not take the engine down at boot.
    torch(mps_raises=True)
    assert backends.detect_device() == "cpu"


def test_a_torch_too_old_for_mps_is_cpu(monkeypatch):
    # torch.backends.mps doesn't exist -> AttributeError, caught like any other
    monkeypatch.setitem(
        sys.modules, "torch",
        types.SimpleNamespace(cuda=types.SimpleNamespace(is_available=lambda: False)),
    )
    assert backends.detect_device() == "cpu"


# --- torch_device (the name torch itself accepts) -------------------------


@pytest.mark.parametrize(("detected", "expected"), [
    ("cuda", "cuda"),
    ("rocm", "cuda"),   # a HIP build addresses the card as "cuda"
    ("mps", "mps"),
    ("cpu", "cpu"),
    ("xpu", "cpu"),     # an unknown device must never be handed to .to()
])
def test_torch_device_names(detected, expected):
    assert backends.torch_device(detected) == expected


def test_backends_map_mps_through_to_torch():
    """The per-backend hooks all defer to the shared table — pin one of each
    shape (a method and the module-level helper kokoro calls)."""
    from syrinx_engine.backends.chatterbox import _ChatterboxBase

    box = _ChatterboxBase.__new__(_ChatterboxBase)
    box.device = "mps"
    assert box._torch_device() == "mps"
    box.device = "rocm"
    assert box._torch_device() == "cuda"


# --- empty_device_cache ---------------------------------------------------


def test_empty_device_cache_uses_the_metal_allocator(torch):
    t = torch(mps=True)
    backends.empty_device_cache()
    assert t.calls == ["mps"]


def test_empty_device_cache_prefers_cuda(torch):
    t = torch(cuda=True, mps=True)
    backends.empty_device_cache()
    assert t.calls == ["cuda"]


def test_empty_device_cache_is_a_noop_on_cpu(torch):
    t = torch()
    backends.empty_device_cache()
    assert t.calls == []


# --- llm device + dtype ---------------------------------------------------


class FakeDtype:
    def __init__(self, name):
        self.name = name

    def __repr__(self):
        return self.name


def test_llm_picks_cuda_first(torch):
    assert llm._pick_device(torch(cuda=True, mps=True)) == "cuda"


def test_llm_picks_mps_over_cpu(torch):
    assert llm._pick_device(torch(mps=True)) == "mps"


def test_llm_falls_back_to_cpu(torch):
    assert llm._pick_device(torch()) == "cpu"


def test_llm_pick_device_survives_an_mps_less_torch():
    old = types.SimpleNamespace(cuda=types.SimpleNamespace(is_available=lambda: False))
    assert llm._pick_device(old) == "cpu"


@pytest.mark.parametrize(("device", "dtype"), [
    ("cuda", "float16"),
    ("mps", "float16"),   # half precision on both accelerators
    ("cpu", "float32"),
])
def test_llm_loads_in_the_right_dtype(monkeypatch, device, dtype):
    """``_load_sync`` end to end against stand-in torch + transformers, so the
    device pick and the dtype pick are pinned together."""
    seen = {}

    class FakeModel:
        def to(self, dev):
            seen["moved_to"] = dev
            return self

        def eval(self):
            return self

    torch_mod = types.SimpleNamespace(
        cuda=types.SimpleNamespace(is_available=lambda: device == "cuda"),
        backends=types.SimpleNamespace(
            mps=types.SimpleNamespace(is_available=lambda: device == "mps")),
        float16=FakeDtype("float16"),
        float32=FakeDtype("float32"),
    )
    transformers = types.SimpleNamespace(
        AutoTokenizer=types.SimpleNamespace(from_pretrained=lambda repo: "tok"),
        AutoModelForCausalLM=types.SimpleNamespace(
            from_pretrained=lambda repo, dtype: seen.update(repo=repo, dtype=dtype)
            or FakeModel()),
    )
    monkeypatch.setitem(sys.modules, "torch", torch_mod)
    monkeypatch.setitem(sys.modules, "transformers", transformers)

    engine = llm.PersonalityLLM()
    engine.model_size = "0.6B"
    engine._load_sync()

    assert engine._device == device
    assert seen["moved_to"] == device
    assert repr(seen["dtype"]) == dtype


# --- TADA device + dtype --------------------------------------------------


class FakeHead:
    """Stands in for lm_head — callable, and records that it was called."""

    def __init__(self):
        self.calls = 0

    def __call__(self, hidden):
        self.calls += 1
        return hidden


class FakeTadaModel:
    def __init__(self):
        self.lm_head = FakeHead()
        self.moved_to = None
        self.evaled = False

    def to(self, device):
        self.moved_to = device
        return self

    def eval(self):
        self.evaled = True
        return self

    def _lm_head_forward(self, hidden):  # the shipped mps detour, unpatched
        raise AssertionError("class method still shadows the override")


def load_tada(monkeypatch, device, cuda_bf16=True):
    """Run TadaBackend._load_sync against stand-in torch / hub / tada modules
    and hand back (backend, seen-kwargs, model). torch and hume-tada are
    outside the CI dependency contract, so every collaborator is a stand-in."""
    from syrinx_engine.backends import tada as tada_backend

    seen = {}
    model = FakeTadaModel()

    bf16_probe = cuda_bf16 if callable(cuda_bf16) else (lambda: cuda_bf16)
    torch_mod = types.SimpleNamespace(
        cuda=types.SimpleNamespace(is_available=lambda: False,
                                   is_bf16_supported=bf16_probe),
        backends=types.SimpleNamespace(
            mps=types.SimpleNamespace(is_available=lambda: False)),
        float32=FakeDtype("float32"),
        bfloat16=FakeDtype("bfloat16"),
    )
    hub = types.SimpleNamespace(
        snapshot_download=lambda repo_id, allow_patterns=None: "/tok/" + repo_id)
    aligner = types.SimpleNamespace(
        AlignerConfig=types.SimpleNamespace(tokenizer_name=""))
    encoder = types.SimpleNamespace(
        Encoder=types.SimpleNamespace(
            from_pretrained=lambda repo, subfolder: types.SimpleNamespace(
                to=lambda d: types.SimpleNamespace(eval=lambda: None))))
    tada_mod = types.SimpleNamespace(
        TadaConfig=types.SimpleNamespace(
            from_pretrained=lambda repo: types.SimpleNamespace(tokenizer_name="")),
        TadaForCausalLM=types.SimpleNamespace(
            from_pretrained=lambda repo, config, torch_dtype: seen.update(
                repo=repo, dtype=torch_dtype) or model),
    )
    for name, mod in [("torch", torch_mod), ("huggingface_hub", hub),
                      ("tada", types.SimpleNamespace()),
                      ("tada.modules", types.SimpleNamespace()),
                      ("tada.modules.aligner", aligner),
                      ("tada.modules.encoder", encoder),
                      ("tada.modules.tada", tada_mod)]:
        monkeypatch.setitem(sys.modules, name, mod)
    monkeypatch.setattr(tada_backend, "install_dac_shim", lambda: None)

    backend = tada_backend.TadaBackend()
    backend.device = device
    backend._load_sync()
    return backend, seen, model


@pytest.mark.parametrize(("device", "dtype"), [
    ("cuda", "bfloat16"),
    ("rocm", "bfloat16"),   # a HIP build reports itself as cuda to torch
    ("mps", "bfloat16"),    # Metal has bf16; fp16 overflows this LM's sampling
    ("cpu", "float32"),     # cpu bf16 is emulated — slower than what it replaces
])
def test_tada_loads_in_the_right_dtype(monkeypatch, device, dtype):
    _backend, seen, _model = load_tada(monkeypatch, device)
    assert repr(seen["dtype"]) == dtype


def test_tada_keeps_fp32_when_cuda_has_no_bf16(monkeypatch):
    """An older card: the cuda arm probes. Metal's arm doesn't have to —
    every Metal-capable Mac has bf16."""
    _backend, seen, _model = load_tada(monkeypatch, "cuda", cuda_bf16=False)
    assert repr(seen["dtype"]) == "float32"


def test_tada_keeps_fp32_when_the_bf16_probe_raises(monkeypatch):
    def boom():
        raise RuntimeError("no cuda runtime")

    _backend, seen, _model = load_tada(monkeypatch, "cuda", cuda_bf16=boom)
    assert repr(seen["dtype"]) == "float32"


def test_tada_runs_the_lm_head_in_place_on_metal(monkeypatch):
    """tada's shipped _lm_head_forward detours through the CPU without moving
    lm_head with it. The override has to beat the class method, which a
    Module-typed attribute would not."""
    _backend, _seen, model = load_tada(monkeypatch, "mps")
    assert model._lm_head_forward("hidden") == "hidden"
    assert model.lm_head.calls == 1


@pytest.mark.parametrize("device", ["cuda", "cpu"])
def test_tada_leaves_the_lm_head_alone_off_metal(monkeypatch, device):
    _backend, _seen, model = load_tada(monkeypatch, device)
    with pytest.raises(AssertionError):
        model._lm_head_forward("hidden")


def test_tada_moves_the_model_to_the_torch_device(monkeypatch):
    _backend, _seen, model = load_tada(monkeypatch, "rocm")
    assert model.moved_to == "cuda" and model.evaled

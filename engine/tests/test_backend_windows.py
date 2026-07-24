"""Windows-portability guards for the CUDA STT + qwen backends.

These modules are omitted from coverage (they lazily import torch / faster-
whisper / qwen_tts, none of which are in the CI contract), but the small
platform-adaptation helpers are pure and worth pinning. Nothing here imports
the heavy deps: the helpers are exercised with monkeypatched imports.
"""

import importlib

import pytest

from syrinx_engine import stt
from syrinx_engine.backends import qwen


# --- stt: staging cu12 cuBLAS/cudart into the ctranslate2 dir on win32 -----


def _fake_site_layout(tmp_path):
    """Build a site-packages tree with an empty ctranslate2 dir and the three
    nvidia-*-cu12 source DLLs. Returns (site_root, ct2_dir, {name: src_path})."""
    site = tmp_path / "site-packages"
    ct2 = site / "ctranslate2"
    ct2.mkdir(parents=True)
    (ct2 / "ctranslate2.dll").write_bytes(b"ct2")  # the marker file _ctranslate2_dir looks for
    srcs = {}
    for name, sub in stt._CT2_CUDA_DLLS.items():
        d = site.joinpath(*sub)
        d.mkdir(parents=True, exist_ok=True)
        p = d / name
        p.write_bytes(name.encode())  # distinct content/size per DLL
        srcs[name] = p
    return site, ct2, srcs


def test_stage_ct2_cuda_dlls_is_a_noop_off_windows(monkeypatch, tmp_path):
    site, ct2, _ = _fake_site_layout(tmp_path)
    monkeypatch.setattr(stt.sys, "platform", "linux")
    monkeypatch.setattr(stt, "_site_packages_dirs", lambda: [site])
    stt._stage_ct2_cuda_dlls()
    # Linux path must not stage anything (torch owns the CUDA libs there).
    assert not (ct2 / "cublas64_12.dll").exists()


def test_stage_ct2_cuda_dlls_copies_all_three(monkeypatch, tmp_path):
    site, ct2, srcs = _fake_site_layout(tmp_path)
    monkeypatch.setattr(stt.sys, "platform", "win32")
    monkeypatch.setattr(stt, "_site_packages_dirs", lambda: [site])
    stt._stage_ct2_cuda_dlls()
    for name, src in srcs.items():
        staged = ct2 / name
        assert staged.exists(), f"{name} not staged"
        assert staged.read_bytes() == src.read_bytes()
    # cuDNN must NEVER be staged (torch's cu13 cudnn64_9 must win the loader).
    assert not (ct2 / "cudnn64_9.dll").exists()


def test_stage_ct2_cuda_dlls_is_idempotent(monkeypatch, tmp_path):
    site, ct2, srcs = _fake_site_layout(tmp_path)
    monkeypatch.setattr(stt.sys, "platform", "win32")
    monkeypatch.setattr(stt, "_site_packages_dirs", lambda: [site])
    stt._stage_ct2_cuda_dlls()
    # Same-size present -> left untouched: no copy on the second pass.
    calls = []
    real_copy = stt.shutil.copy2
    monkeypatch.setattr(stt.shutil, "copy2", lambda s, d: calls.append((s, d)) or real_copy(s, d))
    stt._stage_ct2_cuda_dlls()
    assert calls == [], "idempotent pass should not re-copy same-size DLLs"


def test_stage_ct2_cuda_dlls_restages_on_size_mismatch(monkeypatch, tmp_path):
    site, ct2, srcs = _fake_site_layout(tmp_path)
    monkeypatch.setattr(stt.sys, "platform", "win32")
    monkeypatch.setattr(stt, "_site_packages_dirs", lambda: [site])
    stt._stage_ct2_cuda_dlls()
    # A stale, differently-sized staged DLL is overwritten from source.
    (ct2 / "cudart64_12.dll").write_bytes(b"stale")
    stt._stage_ct2_cuda_dlls()
    assert (ct2 / "cudart64_12.dll").read_bytes() == srcs["cudart64_12.dll"].read_bytes()


def test_stage_ct2_cuda_dlls_missing_source_is_non_fatal(monkeypatch, tmp_path, caplog):
    # CPU box: nvidia wheels absent. Must warn per-DLL, not raise.
    site = tmp_path / "site-packages"
    ct2 = site / "ctranslate2"
    ct2.mkdir(parents=True)
    (ct2 / "ctranslate2.dll").write_bytes(b"ct2")
    monkeypatch.setattr(stt.sys, "platform", "win32")
    monkeypatch.setattr(stt, "_site_packages_dirs", lambda: [site])
    with caplog.at_level("WARNING"):
        stt._stage_ct2_cuda_dlls()  # no source DLLs anywhere
    assert not (ct2 / "cublas64_12.dll").exists()
    assert "cublas64_12.dll" in caplog.text


def test_stage_ct2_cuda_dlls_no_ctranslate2_dir_is_non_fatal(monkeypatch, tmp_path):
    monkeypatch.setattr(stt.sys, "platform", "win32")
    monkeypatch.setattr(stt, "_site_packages_dirs", lambda: [tmp_path / "empty"])
    stt._stage_ct2_cuda_dlls()  # must not raise when CT2 isn't installed


def test_site_packages_dirs_resolves_real_bases():
    dirs = stt._site_packages_dirs()
    assert dirs, "expected at least one site-packages base"


def test_ct2_cuda_dll_map_excludes_cudnn():
    # Guard the invariant: torch's bundled cu13 cuDNN must keep winning — we
    # only ever stage cuBLAS/cublasLt/cudart, never cudnn.
    assert not any("cudnn" in n for n in stt._CT2_CUDA_DLLS)
    assert set(stt._CT2_CUDA_DLLS) == {
        "cublas64_12.dll",
        "cublasLt64_12.dll",
        "cudart64_12.dll",
    }


# --- qwen: actionable SoX-missing error -----------------------------------


def test_import_qwen_tts_translates_a_sox_failure(monkeypatch):
    def boom(_name):
        raise ImportError("sox: command not found (pysox _get_valid_formats)")

    monkeypatch.setattr(importlib, "import_module", boom)
    with pytest.raises(RuntimeError, match="SoX binary on PATH"):
        qwen._import_qwen_tts()


def test_import_qwen_tts_passes_through_unrelated_import_errors(monkeypatch):
    def boom(_name):
        raise ImportError("No module named 'transformers'")

    monkeypatch.setattr(importlib, "import_module", boom)
    with pytest.raises(ImportError, match="transformers"):
        qwen._import_qwen_tts()


def test_import_qwen_tts_returns_the_model_class(monkeypatch):
    import types

    fake = types.SimpleNamespace(Qwen3TTSModel="THE-MODEL")
    monkeypatch.setattr(importlib, "import_module", lambda _n: fake)
    assert qwen._import_qwen_tts() == "THE-MODEL"

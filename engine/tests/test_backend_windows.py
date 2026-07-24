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


# --- stt: os.add_dll_directory(nvidia/cublas/bin) on win32 ----------------


def test_add_cuda_dll_dirs_is_a_noop_off_windows(monkeypatch):
    monkeypatch.setattr(stt.sys, "platform", "linux")
    called = []
    monkeypatch.setattr(stt.os, "add_dll_directory", called.append, raising=False)
    stt._add_cuda_dll_dirs()
    assert called == []


def test_add_cuda_dll_dirs_adds_an_existing_cublas_bin(monkeypatch, tmp_path):
    cublas = tmp_path / "nvidia" / "cublas" / "bin"
    cublas.mkdir(parents=True)
    monkeypatch.setattr(stt.sys, "platform", "win32")
    monkeypatch.setattr(stt, "_cublas_bin_dirs", lambda: [cublas])
    added = []
    monkeypatch.setattr(stt.os, "add_dll_directory", added.append, raising=False)
    stt._add_cuda_dll_dirs()
    assert added == [str(cublas)]


def test_add_cuda_dll_dirs_skips_a_missing_dir(monkeypatch, tmp_path):
    # CPU boxes have no nvidia-cublas wheel — must not raise, must not add.
    missing = tmp_path / "nvidia" / "cublas" / "bin"
    monkeypatch.setattr(stt.sys, "platform", "win32")
    monkeypatch.setattr(stt, "_cublas_bin_dirs", lambda: [missing])
    added = []
    monkeypatch.setattr(stt.os, "add_dll_directory", added.append, raising=False)
    stt._add_cuda_dll_dirs()
    assert added == []


def test_cublas_bin_dirs_resolves_under_site_packages():
    # Never cuDNN — only cuBLAS gets a DLL-dir hint (torch owns cuDNN).
    dirs = stt._cublas_bin_dirs()
    assert dirs, "expected at least one site-packages candidate"
    for d in dirs:
        assert d.parts[-3:] == ("nvidia", "cublas", "bin")


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

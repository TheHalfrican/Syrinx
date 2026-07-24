"""models.py — catalog, HF-cache inspection, active-model persistence.

Nothing here touches the network or the real HF cache: conftest pins
_hf_cache() at a tmp dir and the tests fabricate repo layouts inside it.
"""

import asyncio
import json
import sys
import threading
import types

import pytest

from syrinx_engine import models

FAKE_HW = {"cores": 8, "ram_gb": 32.0, "gpu": True, "gpu_name": "Test GPU"}


def fake_repo(base, repo, *, weights=("model.safetensors",), blobs=("a.bin",),
              snapshots=True, incomplete=False):
    """Fabricate the HF cache layout: models--Org--Name/{blobs,snapshots/rev}."""
    d = base / ("models--" + repo.replace("/", "--"))
    (d / "blobs").mkdir(parents=True)
    for i, name in enumerate(blobs):
        (d / "blobs" / name).write_bytes(b"x" * (100 * (i + 1)))
    if incomplete:
        (d / "blobs" / "deadbeef.incomplete").write_bytes(b"partial")
    if snapshots:
        rev = d / "snapshots" / "rev0"
        rev.mkdir(parents=True)
        for name in weights:
            (rev / name).write_bytes(b"w" * 10)
    return d


# --- cache inspection ----------------------------------------------------


def test_is_repo_cached_true_when_weights_are_present(hf_cache):
    fake_repo(hf_cache, "Org/Name")
    assert models.is_repo_cached("Org/Name", hf_cache) is True


def test_is_repo_cached_false_when_the_repo_dir_is_missing(hf_cache):
    assert models.is_repo_cached("Org/Missing", hf_cache) is False


def test_is_repo_cached_false_without_a_snapshots_dir(hf_cache):
    fake_repo(hf_cache, "Org/NoSnaps", snapshots=False)
    assert models.is_repo_cached("Org/NoSnaps", hf_cache) is False


def test_is_repo_cached_false_when_the_snapshot_holds_no_weights(hf_cache):
    fake_repo(hf_cache, "Org/JustConfig", weights=("config.json", "README.md"))
    assert models.is_repo_cached("Org/JustConfig", hf_cache) is False


def test_is_repo_cached_false_while_a_download_is_incomplete(hf_cache):
    fake_repo(hf_cache, "Org/Partial", incomplete=True)
    assert models.is_repo_cached("Org/Partial", hf_cache) is False


@pytest.mark.parametrize("ext", [".safetensors", ".bin", ".pt", ".pth", ".gguf", ".onnx"])
def test_every_weight_extension_counts_as_cached(hf_cache, ext):
    fake_repo(hf_cache, f"Org/W{ext[1:]}", weights=(f"model{ext}",))
    assert models.is_repo_cached(f"Org/W{ext[1:]}", hf_cache) is True


def test_repo_bytes_sums_the_blobs(hf_cache):
    fake_repo(hf_cache, "Org/Sized", blobs=("a.bin", "b.bin"))  # 100 + 200
    assert models._repo_bytes("Org/Sized", hf_cache) == 300
    assert models._repo_bytes("Org/Absent", hf_cache) == 0


def test_spec_lookup():
    assert models.spec("kokoro").engine == "kokoro"
    assert models.spec("not-a-model") is None


# --- status --------------------------------------------------------------


def test_status_parses_and_carries_the_catalog(monkeypatch):
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    rows = models.ModelManager().status()
    assert len(rows) == len(models.CATALOG)
    by_id = {r["id"]: r for r in rows}
    assert by_id["kokoro"]["category"] == "voice"
    assert by_id["vevo2-singing"]["category"] == "vc"
    assert by_id["vevo2-singing"]["engine"] == "vevo_timbre"
    assert by_id["whisper-base"]["category"] == "stt"
    assert by_id["qwen3-1.7b"]["category"] == "llm"
    # a status row is JSON-marshalable — ListModels dumps it straight out
    assert json.loads(json.dumps(rows))


def test_downloaded_flips_once_the_repo_dirs_exist(monkeypatch, hf_cache):
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    mgr = models.ModelManager()
    assert {r["id"]: r["downloaded"] for r in mgr.status()}["kokoro"] is False
    for repo in models.spec("kokoro").repos:
        fake_repo(hf_cache, repo)
    assert {r["id"]: r["downloaded"] for r in mgr.status()}["kokoro"] is True


def test_multi_repo_models_need_every_repo(monkeypatch, hf_cache):
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    m = models.spec("tada-1b")
    fake_repo(hf_cache, m.repos[0])
    assert models.is_cached(m) is False  # the codec repo is still missing
    fake_repo(hf_cache, m.repos[1])
    assert models.is_cached(m) is True


def test_hardware_warning_reports_gpu_and_ram_shortfalls():
    m = models.spec("qwen-tts-1.7B")
    assert models.hardware_warning(m, FAKE_HW) == ""
    weak = {"cores": 4, "ram_gb": 4.0, "gpu": False, "gpu_name": ""}
    warn = models.hardware_warning(m, weak)
    assert "no GPU detected" in warn and "GB RAM" in warn


def test_detect_hardware_reports_cores():
    hw = models.detect_hardware()
    assert hw["cores"] >= 1
    assert set(hw) == {"cores", "ram_gb", "gpu", "gpu_name"}


# --- isolated-venv warnings ---------------------------------------------


def test_vc_setup_warning_points_at_the_setup_script(monkeypatch, tmp_path):
    """seed_vc / vevo_timbre live in their own venvs — no venv, no conversion."""
    monkeypatch.setattr(models, "_ENGINE_DIR", tmp_path)
    assert models._vc_setup_warning(models.spec("seed-vc")) == "run engine/setup-seedvc.sh first"
    assert models._vc_setup_warning(models.spec("vevo-timbre")) == "run engine/setup-vevo.sh first"
    assert models._vc_setup_warning(models.spec("vevo2-singing")) == "run engine/setup-vevo.sh first"
    assert models._vc_setup_warning(models.spec("kokoro")) == ""


def test_vc_setup_warning_clears_once_the_venvs_exist(monkeypatch, tmp_path):
    monkeypatch.setattr(models, "_ENGINE_DIR", tmp_path)
    (tmp_path / ".venv-seedvc").mkdir()
    (tmp_path / ".venv-vevo").mkdir()
    assert models._vc_setup_warning(models.spec("seed-vc")) == ""
    assert models._vc_setup_warning(models.spec("vevo-timbre")) == ""


def test_setup_warning_wins_over_the_hardware_warning(monkeypatch, tmp_path):
    monkeypatch.setattr(models, "_ENGINE_DIR", tmp_path)
    monkeypatch.setattr(models, "detect_hardware", lambda: {"cores": 2, "ram_gb": 2.0,
                                                            "gpu": False, "gpu_name": ""})
    row = {r["id"]: r for r in models.ModelManager().status()}["seed-vc"]
    assert row["warning"] == "run engine/setup-seedvc.sh first"


# --- seed-vc's own two-tier cache ---------------------------------------


def test_seed_vc_repos_resolve_under_the_data_dir(isolated_env):
    """seed-vc downloads through its own package into $DATA/seedvc/... —
    the Models tab has to look there, not in the HF cache."""
    m = models.spec("seed-vc")
    root = models._cache_root(m, "Plachta/Seed-VC")
    assert root == isolated_env / "seedvc" / "checkpoints"
    assert models._cache_root(m, "openai/whisper-small").name == "hf_cache"
    assert models._cache_root(models.spec("kokoro"), "hexgrad/Kokoro-82M") is None


# --- active selection ----------------------------------------------------


def test_set_active_persists_across_manager_instances(isolated_env):
    mgr = models.ModelManager()
    assert mgr.active_id("voice") == "kokoro"  # the default
    assert mgr.set_active("qwen-tts-0.6B") == "voice"
    assert json.loads((isolated_env / "models.json").read_text())["voice"] == "qwen-tts-0.6B"
    assert models.ModelManager().active_id("voice") == "qwen-tts-0.6B"


def test_set_active_rejects_unknown_ids():
    mgr = models.ModelManager()
    assert mgr.set_active("not-a-model") == ""
    assert mgr.active_id("voice") == "kokoro"


def test_active_spec_and_flag_in_status(monkeypatch):
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    mgr = models.ModelManager()
    mgr.set_active("whisper-turbo")
    assert mgr.active_spec("stt").id == "whisper-turbo"
    active = {r["id"] for r in mgr.status() if r["active"]}
    assert "whisper-turbo" in active
    assert "whisper-base" not in active


def test_active_id_of_an_unknown_category_is_empty():
    assert models.ModelManager().active_id("nope") == ""


def test_a_corrupt_models_json_falls_back_to_the_defaults(isolated_env):
    (isolated_env / "models.json").write_text("{ not json")
    assert models.ModelManager().active_id("voice") == "kokoro"


# --- delete --------------------------------------------------------------


def test_delete_removes_the_repo_dirs(hf_cache):
    d = fake_repo(hf_cache, "hexgrad/Kokoro-82M")
    assert d.exists()
    models.ModelManager().delete("kokoro")
    assert not d.exists()


def test_delete_of_an_unknown_model_is_a_no_op():
    models.ModelManager().delete("not-a-model")


def test_a_models_json_that_cannot_be_written_is_logged_not_raised(tmp_path):
    mgr = models.ModelManager()
    mgr._settings = tmp_path / "no-such-dir" / "models.json"
    mgr.set_active("kokoro")  # a read-only data dir must not kill the engine


# --- hardware probing ----------------------------------------------------


def test_detect_hardware_reports_ram_on_this_platform():
    # Linux (sysconf) and Windows (ctypes GlobalMemoryStatusEx) both report
    # real RAM — the cross-platform fix. Only a sysconf-less non-Windows box
    # returns 0.0 (asserted separately, below).
    hw = models.detect_hardware()
    if sys.platform in ("linux", "win32"):
        assert hw["ram_gb"] > 0.0


def test_detect_hardware_falls_back_to_zero_without_a_ram_source(monkeypatch):
    def boom(_name):
        raise OSError("no sysconf here")

    # raising=False so this also runs on Windows, where os.sysconf doesn't
    # exist. Force a non-win32 platform too, so the ctypes fallback is skipped
    # and we're asserting the true no-source path on every host OS.
    monkeypatch.setattr(models.os, "sysconf", boom, raising=False)
    monkeypatch.setattr(models.sys, "platform", "no-ram-source")
    assert models.detect_hardware()["ram_gb"] == 0.0


def test_detect_hardware_reports_a_cuda_gpu(monkeypatch):
    torch = types.SimpleNamespace(cuda=types.SimpleNamespace(
        is_available=lambda: True, get_device_name=lambda i: "NVIDIA GeForce RTX 4090"))
    monkeypatch.setitem(sys.modules, "torch", torch)
    hw = models.detect_hardware()
    assert hw["gpu"] is True
    assert hw["gpu_name"] == "NVIDIA GeForce RTX 4090"


def test_detect_hardware_without_torch_reports_no_gpu(monkeypatch):
    monkeypatch.setitem(sys.modules, "torch", None)
    hw = models.detect_hardware()
    assert hw["gpu"] is False and hw["gpu_name"] == ""


# --- download ------------------------------------------------------------


def fake_hub(monkeypatch, snapshot_download):
    monkeypatch.setitem(
        sys.modules, "huggingface_hub",
        types.SimpleNamespace(snapshot_download=snapshot_download),
    )


def test_download_polls_progress_and_finishes(monkeypatch, hf_cache):
    """Progress is on-disk byte growth against size_mb, so the fake fetch
    materializes the repo the poller is watching."""
    fetched = []

    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        fetched.append((repo, cache_dir, allow_patterns))
        fake_repo(hf_cache, repo)

    fake_hub(monkeypatch, snapshot_download)
    events = []
    ok = asyncio.run(models.ModelManager().download("kokoro", lambda *a: events.append(a)))

    assert ok is True
    assert [r for r, _c, _p in fetched] == models.spec("kokoro").repos
    assert fetched[0][1] is None  # the plain HF cache, no override
    assert events[-1] == ("kokoro", 1.0, "done")
    assert events[0][2] == "downloading"


def test_download_passes_the_allow_patterns_and_seed_vc_cache_root(monkeypatch, isolated_env):
    seen = []

    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        seen.append((repo, cache_dir, allow_patterns))

    fake_hub(monkeypatch, snapshot_download)
    asyncio.run(models.ModelManager().download("seed-vc", lambda *a: None))
    roots = {c for _r, c, _p in seen}
    assert all(str(isolated_env / "seedvc") in r for r in roots)
    assert all("*.safetensors" in p for _r, _c, p in seen)


def test_concurrent_downloads_fetch_one_at_a_time(monkeypatch):
    """huggingface_hub's per-cache symlink-support probe races under
    concurrent snapshot_downloads (WinError 1314 on boxes without Developer
    Mode) — the fetch phase is serialized, while every requested model still
    reports `downloading` immediately (queued, not rejected)."""
    import threading
    import time

    active, peak = 0, 0
    gauge = threading.Lock()

    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        nonlocal active, peak
        with gauge:
            active += 1
            peak = max(peak, active)
        time.sleep(0.05)
        with gauge:
            active -= 1

    fake_hub(monkeypatch, snapshot_download)

    async def run():
        mgr = models.ModelManager()
        t1 = asyncio.create_task(mgr.download("kokoro", lambda *a: None))
        t2 = asyncio.create_task(mgr.download("whisper-turbo", lambda *a: None))
        await asyncio.sleep(0.01)  # both past the _downloading.add
        downloading = {r["id"] for r in mgr.status() if r["downloading"]}
        assert {"kokoro", "whisper-turbo"} <= downloading
        return await asyncio.gather(t1, t2)

    assert asyncio.run(run()) == [True, True]
    assert peak == 1  # never two snapshot_downloads in flight


def test_a_failing_download_reports_error(monkeypatch):
    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        raise RuntimeError("404 from the hub")

    fake_hub(monkeypatch, snapshot_download)
    events = []
    ok = asyncio.run(models.ModelManager().download("kokoro", lambda *a: events.append(a)))
    assert ok is False
    assert events[-1] == ("kokoro", 0.0, "error")


def test_download_without_huggingface_hub_fails_cleanly(monkeypatch):
    monkeypatch.setitem(sys.modules, "huggingface_hub", None)
    assert asyncio.run(models.ModelManager().download("kokoro", lambda *a: None)) is False


def test_download_refuses_unknown_ids_and_repeat_requests():
    mgr = models.ModelManager()
    assert asyncio.run(mgr.download("not-a-model", lambda *a: None)) is False
    mgr._downloading.add("kokoro")
    assert asyncio.run(mgr.download("kokoro", lambda *a: None)) is False


def test_downloading_shows_up_in_status(monkeypatch):
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    mgr = models.ModelManager()
    mgr._downloading.add("kokoro")
    assert {r["id"] for r in mgr.status() if r["downloading"]} == {"kokoro"}


# --- honest download totals ---------------------------------------------


class FakeSibling:
    def __init__(self, rfilename, size):
        self.rfilename = rfilename
        self.size = size


def fake_hf_api(monkeypatch, files_by_repo, *, boom=False):
    """Install a fake huggingface_hub.HfApi whose model_info returns the given
    siblings per repo. ``boom`` makes model_info raise (metadata unavailable)."""
    class FakeApi:
        def model_info(self, repo, files_metadata=False):
            if boom:
                raise RuntimeError("rate limited")
            sibs = [FakeSibling(name, size) for name, size in files_by_repo[repo]]
            return types.SimpleNamespace(siblings=sibs)

    monkeypatch.setitem(sys.modules, "huggingface_hub",
                        types.SimpleNamespace(HfApi=FakeApi))


def test_expected_bytes_sums_metadata_across_repos(monkeypatch):
    m = models.spec("tada-1b")  # two repos, patterns=None (whole repo)
    fake_hf_api(monkeypatch, {
        m.repos[0]: [("model.safetensors", 1000), ("config.json", 24)],
        m.repos[1]: [("codec.pt", 500)],
    })
    assert models._expected_bytes(m) == 1524


def test_expected_bytes_respects_allow_patterns(monkeypatch):
    """Only files that snapshot_download's allow_patterns would fetch count —
    including a directory glob (fnmatch's ``*`` spans ``/``)."""
    m = models.spec("vevo-timbre")  # patterns like "tokenizer/vq8192/*"
    fake_hf_api(monkeypatch, {m.repos[0]: [
        ("tokenizer/vq8192/model.safetensors", 100),
        ("tokenizer/vq8192/nested/extra.bin", 40),
        ("acoustic_modeling/Vq8192ToMels/w.pt", 30),
        ("acoustic_modeling/Vocoder/g.pt", 20),
        ("README.md", 999),               # excluded — no pattern admits it
        ("acoustic_modeling/AR/huge.safetensors", 9999),  # excluded dir
    ]})
    assert models._expected_bytes(m) == 190


def test_expected_bytes_none_when_patterns_is_none_takes_everything(monkeypatch):
    m = models.spec("kokoro")  # patterns=None
    fake_hf_api(monkeypatch, {m.repos[0]: [("a.bin", 7), ("b/c.json", 3)]})
    assert models._expected_bytes(m) == 10


def test_expected_bytes_falls_back_on_metadata_failure(monkeypatch):
    m = models.spec("kokoro")
    fake_hf_api(monkeypatch, {}, boom=True)
    assert models._expected_bytes(m) is None


def test_expected_bytes_falls_back_on_missing_size(monkeypatch):
    """A matched file with no size metadata means we can't trust the sum."""
    m = models.spec("kokoro")
    fake_hf_api(monkeypatch, {m.repos[0]: [("a.bin", 100), ("b.bin", None)]})
    assert models._expected_bytes(m) is None


def test_expected_bytes_none_without_huggingface_hub(monkeypatch):
    monkeypatch.setitem(sys.modules, "huggingface_hub", None)
    assert models._expected_bytes(models.spec("kokoro")) is None


def test_pattern_allows_matches_snapshot_download_semantics():
    assert models._pattern_allows("anything.bin", None) is True
    assert models._pattern_allows("a/deep/x.safetensors", ["*.safetensors"]) is True
    assert models._pattern_allows("x.bin", ["*.safetensors"]) is False
    # a directory glob spans nested dirs, and a trailing "/" gets an implicit "*"
    assert models._pattern_allows("t/vq/deep/f.pt", ["t/vq/*"]) is True
    assert models._pattern_allows("t/vq/f.pt", ["t/vq/"]) is True
    assert models._pattern_allows("other/f.pt", ["t/vq/*"]) is False


def test_download_uses_the_real_metadata_total(monkeypatch, hf_cache):
    """The poll bar normalizes against the fetched metadata total, not size_mb —
    100 on-disk bytes against a 200-byte metadata total reads ~0.5, where size_mb
    (350 MB) would read ~0. Hold the fetch open so the poller samples mid-download."""
    release = threading.Event()

    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        fake_repo(hf_cache, repo, blobs=("a.bin",))  # 100 bytes on disk
        release.wait(2.0)

    monkeypatch.setitem(sys.modules, "huggingface_hub", types.SimpleNamespace(
        snapshot_download=snapshot_download,
        HfApi=type("A", (), {"model_info": lambda self, r, files_metadata=False:
                             types.SimpleNamespace(siblings=[FakeSibling("m.bin", 200)])})))
    events = []

    def on_progress(*a):
        events.append(a)
        if a[2] == "downloading" and a[1] > 0:
            release.set()  # a nonzero fraction means the bytes are on disk

    ok = asyncio.run(models.ModelManager().download("kokoro", on_progress))
    assert ok is True
    downloading = [e for e in events if e[2] == "downloading"]
    assert any(e[1] == pytest.approx(0.5) for e in downloading)


def test_download_finalizing_when_bytes_reach_the_total(monkeypatch, hf_cache):
    """Bytes on disk >= expected total while the fetch is still running emits
    "finalizing" with the fraction capped at 0.999, then "done"."""
    release = threading.Event()

    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        fake_repo(hf_cache, repo, blobs=("a.bin",))  # 100 bytes >= 50-byte total
        release.wait(2.0)  # hold the fetch open so the poller sees finalizing

    monkeypatch.setitem(sys.modules, "huggingface_hub", types.SimpleNamespace(
        snapshot_download=snapshot_download,
        HfApi=type("A", (), {"model_info": lambda self, r, files_metadata=False:
                             types.SimpleNamespace(siblings=[FakeSibling("m.bin", 50)])})))

    events = []

    def on_progress(*a):
        events.append(a)
        if a[2] == "finalizing":
            release.set()  # let the fetch complete once we've observed finalizing

    ok = asyncio.run(models.ModelManager().download("kokoro", on_progress))
    assert ok is True
    finalizing = [e for e in events if e[2] == "finalizing"]
    assert finalizing and all(e[1] == pytest.approx(0.999) for e in finalizing)
    assert events[-1] == ("kokoro", 1.0, "done")


def test_download_falls_back_to_size_mb_when_metadata_fails(monkeypatch, hf_cache):
    """A metadata failure must not break the download — it uses the size_mb
    estimate and still completes."""
    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        fake_repo(hf_cache, repo)

    class BoomApi:
        def model_info(self, repo, files_metadata=False):
            raise RuntimeError("offline")

    monkeypatch.setitem(sys.modules, "huggingface_hub", types.SimpleNamespace(
        snapshot_download=snapshot_download, HfApi=BoomApi))
    events = []
    ok = asyncio.run(models.ModelManager().download("kokoro", lambda *a: events.append(a)))
    assert ok is True
    assert events[0][2] == "downloading"
    assert events[-1] == ("kokoro", 1.0, "done")

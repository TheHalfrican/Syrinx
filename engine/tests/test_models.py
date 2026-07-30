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

from syrinx_engine import models, vcsetup

FAKE_HW = {"cores": 8, "ram_gb": 32.0, "gpu": True, "gpu_name": "Test GPU",
           "vram_gb": 24.0}


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
    weak = {"cores": 4, "ram_gb": 4.0, "gpu": False, "gpu_name": "", "vram_gb": 0.0}
    warn = models.hardware_warning(m, weak)
    assert "no GPU detected" in warn and "GB RAM" in warn


def test_detect_hardware_reports_cores():
    hw = models.detect_hardware()
    assert hw["cores"] >= 1
    assert set(hw) == {"cores", "ram_gb", "gpu", "gpu_name", "vram_gb"}


# --- VRAM warnings (advisory only — nothing gates on them) ----------------


def gpu_hw(vram_gb, ram_gb=32.0):
    return {"cores": 8, "ram_gb": ram_gb, "gpu": True, "gpu_name": "Test GPU",
            "vram_gb": vram_gb}


def test_a_small_card_warns_about_the_models_vram_appetite():
    m = models.spec("tada-3b-ml")  # 8 GB
    warn = models.hardware_warning(m, gpu_hw(4.0))
    assert warn == "needs ~8 GB VRAM (have 4) — expect very slow or failed loads"


def test_enough_vram_stays_quiet():
    m = models.spec("tada-3b-ml")
    assert models.hardware_warning(m, gpu_hw(8.0)) == ""   # exactly at the bar
    assert models.hardware_warning(m, gpu_hw(24.0)) == ""


def test_a_zero_min_vram_model_never_warns():
    assert models.spec("kokoro").min_vram_gb == 0.0
    assert models.spec("whisper-base").min_vram_gb == 0.0
    assert models.hardware_warning(models.spec("kokoro"), gpu_hw(0.5)) == ""


def test_no_gpu_box_gets_no_vram_clause():
    """A CPU-only box hears "no GPU"; a VRAM figure there would be noise."""
    m = models.spec("tada-3b-ml")
    cpu = {"cores": 4, "ram_gb": 32.0, "gpu": False, "gpu_name": "", "vram_gb": 0.0}
    assert models.hardware_warning(m, cpu) == "no GPU detected — will be slow on CPU"


def test_unknown_vram_gets_no_clause():
    """A GPU torch couldn't measure (vram_gb 0) must not warn on a guess."""
    m = models.spec("tada-3b-ml")
    assert models.hardware_warning(m, gpu_hw(0.0)) == ""


def test_vram_and_ram_warnings_compose():
    m = models.spec("tada-3b-ml")  # 16 GB RAM, 8 GB VRAM
    warn = models.hardware_warning(m, gpu_hw(4.0, ram_gb=8.0))
    assert warn == ("needs ~16 GB RAM (have 8); "
                    "needs ~8 GB VRAM (have 4) — expect very slow or failed loads")


def test_min_vram_gb_rides_along_in_status(monkeypatch):
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    by_id = {r["id"]: r for r in models.ModelManager().status()}
    assert by_id["kokoro"]["min_vram_gb"] == 0.0
    assert by_id["qwen3-4b"]["min_vram_gb"] == 10.0
    assert all("min_vram_gb" in r for r in models.ModelManager().status())


# --- isolated-venv warnings ---------------------------------------------
#
# The probe lives in vcsetup now, so these point vcsetup's engine_dir() at a
# tmp tree. They also fabricate what a FINISHED setup leaves — interpreter AND
# landmark package — because neither one alone means "ready": a cancel mid-venv
# leaves the directory, and a failure after the torch stage leaves a working
# interpreter with none of the engine's own packages behind it.


def make_venv(root, name, landmark=True):
    """Fabricate the per-OS venv vcsetup probes for. ``landmark=False`` makes the
    torn install whose warning must NOT clear."""
    venv = root / name
    setup = next(s for s in vcsetup.SETUPS.values() if name == f".venv-{s.venv}")
    if sys.platform == "win32":
        exe = venv / "Scripts" / "python.exe"
        site = venv / "Lib" / "site-packages"
    else:
        exe = venv / "bin" / "python"
        site = venv / "lib" / "python3.12" / "site-packages"
    exe.parent.mkdir(parents=True)
    exe.write_text("")
    if landmark:
        (site / setup.landmark).mkdir(parents=True)
    return exe


def test_vc_setup_warning_asks_for_the_one_time_setup(monkeypatch, tmp_path):
    """seed_vc / vevo_timbre live in their own venvs — no venv, no conversion.
    The string is OS-agnostic and names the Install button, not a .sh path."""
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    assert models._vc_setup_warning(models.spec("seed-vc")) == models.VC_SETUP_NEEDED
    assert models._vc_setup_warning(models.spec("vevo-timbre")) == models.VC_SETUP_NEEDED
    assert models._vc_setup_warning(models.spec("vevo2-singing")) == models.VC_SETUP_NEEDED
    assert models.VC_SETUP_NEEDED == "one-time setup needed — click Install"
    assert models._vc_setup_warning(models.spec("kokoro")) == ""


def test_vc_setup_warning_clears_once_the_venvs_exist(monkeypatch, tmp_path):
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    make_venv(tmp_path, ".venv-seedvc")
    make_venv(tmp_path, ".venv-vevo")
    amphion = tmp_path / "Amphion"
    amphion.mkdir()
    monkeypatch.setenv("SYRINX_VEVO_AMPHION", str(amphion))
    assert models._vc_setup_warning(models.spec("seed-vc")) == ""
    assert models._vc_setup_warning(models.spec("vevo-timbre")) == ""


def test_a_torn_venv_keeps_its_warning_and_its_install_button(monkeypatch, tmp_path):
    """2026-07-28: setup-seedvc.ps1 died at the `pip install seed-vc` stage, so
    the venv+torch stages had already produced an interpreter. The row went
    green and the Install button vanished for an engine that cannot load — the
    one state where a stale "ready" is worse than no install at all."""
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    make_venv(tmp_path, ".venv-seedvc", landmark=False)
    assert models._vc_setup_warning(models.spec("seed-vc")) == models.VC_SETUP_NEEDED
    # needs_setup is what actually re-shows the button, so assert the row too
    row = {r["id"]: r for r in models.ModelManager().status()}["seed-vc"]
    assert row["needs_setup"] is True


def test_vevo_warning_when_the_amphion_clone_is_missing(monkeypatch, tmp_path):
    """The worker needs the clone, not just the venv — a restored data dir
    can have one without the other."""
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    make_venv(tmp_path, ".venv-vevo")
    monkeypatch.setenv("SYRINX_VEVO_AMPHION", str(tmp_path / "nope"))
    assert models._vc_setup_warning(models.spec("vevo-timbre")) == models.VC_SETUP_NEEDED


def test_setup_warning_wins_over_the_hardware_warning(monkeypatch, tmp_path):
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    monkeypatch.setattr(models, "detect_hardware", lambda: {"cores": 2, "ram_gb": 2.0,
                                                            "gpu": False, "gpu_name": ""})
    row = {r["id"]: r for r in models.ModelManager().status()}["seed-vc"]
    assert row["warning"] == models.VC_SETUP_NEEDED


def test_status_rows_carry_setup_id_and_needs_setup(monkeypatch, tmp_path):
    """The app keys its Install button off these two fields, so every row has
    them: "" / False for anything that isn't an isolated-venv engine, and both
    vevo rows share the single "vevo" setup (one install clears the pair)."""
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    by_id = {r["id"]: r for r in models.ModelManager().status()}
    assert all("setup_id" in r and "needs_setup" in r for r in by_id.values())
    assert by_id["kokoro"]["setup_id"] == "" and by_id["kokoro"]["needs_setup"] is False
    assert by_id["chatterbox-vc"]["setup_id"] == ""  # in-process, no venv to build
    assert by_id["seed-vc"]["setup_id"] == "seedvc"
    assert by_id["vevo-timbre"]["setup_id"] == "vevo"
    assert by_id["vevo2-singing"]["setup_id"] == "vevo"
    # a "voice" row with a setup, not just the ⇄ converters — the field is keyed
    # off the engine name, so the category never entered into it
    assert by_id["luxtts"]["setup_id"] == "luxtts"
    assert all(by_id[i]["needs_setup"] is True
               for i in ("seed-vc", "vevo-timbre", "vevo2-singing", "luxtts"))

    # installing seed-vc clears only its own row
    make_venv(tmp_path, ".venv-seedvc")
    by_id = {r["id"]: r for r in models.ModelManager().status()}
    assert by_id["seed-vc"]["needs_setup"] is False
    assert by_id["vevo-timbre"]["needs_setup"] is True
    assert by_id["luxtts"]["needs_setup"] is True


def test_luxtts_needs_its_venv_before_it_can_speak(monkeypatch, tmp_path):
    """LuxTTS is the first VOICE row wired to the one-click installer, so the
    whole needs_setup path has to work for a row that isn't a ⇄ converter. The
    `zipvoice` landmark comes along for free — make_venv reads it off SETUPS."""
    monkeypatch.setattr(vcsetup, "engine_dir", lambda: tmp_path)
    monkeypatch.setattr(models, "detect_hardware", lambda: FAKE_HW)
    assert models._vc_setup_warning(models.spec("luxtts")) == models.VC_SETUP_NEEDED
    row = {r["id"]: r for r in models.ModelManager().status()}["luxtts"]
    assert row["needs_setup"] is True
    assert row["warning"] == models.VC_SETUP_NEEDED

    make_venv(tmp_path, ".venv-luxtts")
    row = {r["id"]: r for r in models.ModelManager().status()}["luxtts"]
    assert models._vc_setup_warning(models.spec("luxtts")) == ""
    assert row["needs_setup"] is False


def test_descriptions_no_longer_send_people_to_a_shell_script():
    """The .sh phrasing was a dead end for non-developers and a nonexistent
    path on Windows."""
    assert not any(".sh" in m.description for m in models.CATALOG)


# --- the readiness gate: no weights nobody asked for ---------------------
#
# Disk space is only ever spent by explicit choice, so every generation path
# asks require_weights first. The gate's whole design is "name the exact row or
# say nothing" — half of these tests are about the cases where it must stay out
# of the way.


def test_spec_for_resolves_a_row_in_every_category():
    assert models.spec_for("voice", "qwen", "0.6B").id == "qwen-tts-0.6B"
    assert models.spec_for("voice", "qwen_custom_voice", "1.7B").id == "qwen-custom-voice-1.7B"
    assert models.spec_for("stt", "whisper", "large-v3").id == "whisper-large"
    assert models.spec_for("llm", "qwen_llm", "4B").id == "qwen3-4b"
    assert models.spec_for("vc", "seed_vc").id == "seed-vc"


def test_spec_for_with_no_size_names_the_variant_the_backend_would_load():
    """"" is what a component that never recorded a size hands over, and a
    backend built with size="" loads its default variant — which the catalog
    lists first for every multi-size engine. If that ordering ever changes, the
    gate starts naming a row the user isn't about to load."""
    assert models.spec_for("voice", "qwen").id == "qwen-tts-1.7B"
    assert models.spec_for("voice", "qwen_custom_voice").id == "qwen-custom-voice-1.7B"
    assert models.spec_for("voice", "tada").id == "tada-1b"
    assert models.spec_for("stt", "whisper").id == "whisper-base"
    assert models.spec_for("voice", "kokoro").id == "kokoro"  # single-row engine


def test_spec_for_also_answers_to_a_repo_id():
    """Components hold different handles on the same row: the TTS router
    remembers "1.7B", the Transcriber remembers the repo it was handed."""
    assert models.spec_for("stt", "whisper", "Systran/faster-whisper-small").id == "whisper-small"
    assert models.spec_for("llm", "qwen_llm", "Qwen/Qwen3-4B").id == "qwen3-4b"


def test_spec_for_returns_none_rather_than_guessing():
    assert models.spec_for("voice", "qwen", "9B") is None
    assert models.spec_for("voice", "not-an-engine") is None
    assert models.spec_for("stt", "whisper", "openai/whisper-tiny") is None
    assert models.spec_for("nope", "kokoro") is None
    # the engine name alone isn't enough — the category has to match too
    assert models.spec_for("stt", "kokoro") is None


def test_require_weights_stays_out_of_the_way_for_an_unknown_spec():
    """A raw HF repo passes straight through. We can't know its size, and
    refusing on a hunch would break every legitimately hand-set model."""
    models.require_weights("stt", "whisper", size="openai/whisper-tiny")
    models.require_weights("llm", "qwen_llm", size="Qwen/Qwen3-32B")
    models.require_weights("voice", "not-an-engine")
    models.require_weights("vc", "seed_vc", model_id="not-a-row")


def test_require_weights_names_the_row_and_what_it_costs():
    with pytest.raises(models.ModelNotDownloaded) as e:
        models.require_weights("voice", "qwen", "0.6B")
    assert str(e.value) == (
        "Qwen TTS 0.6B isn't downloaded yet — open Models and click Download "
        "on its row (2.3 GB)."
    )


def test_a_sub_gigabyte_row_is_quoted_in_megabytes():
    """"0.1 GB" reads as a rounding error rather than a file to fetch."""
    with pytest.raises(models.ModelNotDownloaded, match=r"\(140 MB\)"):
        models.require_weights("stt", "whisper", "base.en")


def test_require_weights_passes_once_the_weights_are_on_disk(hf_cache):
    for repo in models.spec("whisper-base").repos:
        fake_repo(hf_cache, repo)
    models.require_weights("stt", "whisper", "base.en")


def test_an_explicit_row_id_wins_over_the_engine_lookup():
    """vevo_timbre is two rows; only the caller knows which mode it's in."""
    with pytest.raises(models.ModelNotDownloaded, match="Vevo2"):
        models.require_weights("vc", "vevo_timbre", model_id="vevo2-singing")
    with pytest.raises(models.ModelNotDownloaded, match="Vevo-Timbre"):
        models.require_weights("vc", "vevo_timbre")


def test_downloaded_engines_reads_the_cache(hf_cache):
    assert models.downloaded_engines("voice") == set()
    for repo in models.spec("kokoro").repos:
        fake_repo(hf_cache, repo)
    for repo in models.spec("qwen-custom-voice-0.6B").repos:
        fake_repo(hf_cache, repo)
    assert models.downloaded_engines("voice") == {"kokoro", "qwen_custom_voice"}
    assert models.downloaded_engines("stt") == set()
    assert models.downloaded_engines("nope") == set()


def test_a_half_fetched_multi_repo_row_is_not_a_downloaded_engine(hf_cache):
    """TADA is weights + codec; one of the two is not an engine you can run."""
    fake_repo(hf_cache, models.spec("tada-1b").repos[0])
    assert "tada" not in models.downloaded_engines("voice")
    fake_repo(hf_cache, models.spec("tada-1b").repos[1])
    assert "tada" in models.downloaded_engines("voice")


def test_vc_row_for_covers_every_engine_and_mode_the_converter_produces():
    """The ⇄ view picks an engine and a mode; the catalog holds rows, and the
    two aren't one-to-one. Every ⇄ engine has to be reachable from this map, and
    so does every ⇄ row — vevo2-singing's row state was consulted by nothing at
    all before it existed. chatterbox_vc has no music entry because it has no
    music pipeline, so the view can never ask for that pair."""
    assert models.VC_ROW_FOR == {
        ("chatterbox_vc", "speech"): "chatterbox-vc",
        ("seed_vc", "speech"): "seed-vc",
        ("seed_vc", "music"): "seed-vc",
        ("vevo_timbre", "speech"): "vevo-timbre",
        ("vevo_timbre", "music"): "vevo2-singing",
    }
    vc_rows = {m.id for m in models.CATALOG if m.category == "vc"}
    vc_engines = {m.engine for m in models.CATALOG if m.category == "vc"}
    assert set(models.VC_ROW_FOR.values()) == vc_rows
    assert {engine for engine, _mode in models.VC_ROW_FOR} == vc_engines


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


# The Windows GlobalMemoryStatusEx fallback, exercised on ANY host so the
# branch isn't dead code on the OS that happens to run the suite. ctypes stays
# REAL — only kernel32 is stubbed — so the MEMORYSTATUSEX layout is genuinely
# built and byref() genuinely wraps it.


def _no_sysconf(monkeypatch):
    def boom(_name):
        raise AttributeError("os.sysconf does not exist on Windows")

    monkeypatch.setattr(models.os, "sysconf", boom, raising=False)
    monkeypatch.setattr(models.sys, "platform", "win32")


def _fake_kernel32(monkeypatch, fn):
    import ctypes

    monkeypatch.setattr(
        ctypes, "windll",
        types.SimpleNamespace(kernel32=types.SimpleNamespace(GlobalMemoryStatusEx=fn)),
        raising=False,  # there is no ctypes.windll off Windows
    )


def test_windows_ram_comes_from_globalmemorystatusex(monkeypatch):
    import ctypes

    seen = {}

    def GlobalMemoryStatusEx(ref):  # noqa: N802 — mirrors the Win32 name
        stat = ref._obj  # byref() wraps the caller's real structure
        seen["dwLength"] = stat.dwLength
        seen["sizeof"] = ctypes.sizeof(type(stat))
        stat.ullTotalPhys = 32 * 1024**3
        return 1

    _no_sysconf(monkeypatch)
    _fake_kernel32(monkeypatch, GlobalMemoryStatusEx)
    assert models._total_ram_gb() == 32.0
    # The API fails outright unless dwLength is pre-set to the struct's size —
    # pin that we set it, and that the struct we build is the real thing.
    assert seen["dwLength"] == seen["sizeof"]


def test_windows_ram_is_zero_when_the_api_reports_failure(monkeypatch):
    _no_sysconf(monkeypatch)
    _fake_kernel32(monkeypatch, lambda ref: 0)  # 0 == the Win32 call failed
    assert models._total_ram_gb() == 0.0


def test_windows_ram_is_zero_when_the_ctypes_call_raises(monkeypatch):
    def boom(ref):
        raise OSError("kernel32 unreachable")

    _no_sysconf(monkeypatch)
    _fake_kernel32(monkeypatch, boom)
    # A broken/emulated box must degrade to "unknown RAM", never take the
    # engine down at hardware-probe time.
    assert models._total_ram_gb() == 0.0


def test_detect_hardware_reports_a_cuda_gpu(monkeypatch):
    torch = types.SimpleNamespace(cuda=types.SimpleNamespace(
        is_available=lambda: True, get_device_name=lambda i: "NVIDIA GeForce RTX 4090",
        get_device_properties=lambda i: types.SimpleNamespace(
            total_memory=24 * 1024**3)))
    monkeypatch.setitem(sys.modules, "torch", torch)
    hw = models.detect_hardware()
    assert hw["gpu"] is True
    assert hw["gpu_name"] == "NVIDIA GeForce RTX 4090"
    assert hw["vram_gb"] == 24.0


def test_detect_hardware_keeps_the_gpu_when_vram_cannot_be_read(monkeypatch):
    """A driver that refuses get_device_properties must not erase the GPU —
    vram_gb 0 just means "unknown", and no VRAM warning fires."""
    def boom(_i):
        raise RuntimeError("no properties")

    torch = types.SimpleNamespace(cuda=types.SimpleNamespace(
        is_available=lambda: True, get_device_name=lambda i: "Mystery GPU",
        get_device_properties=boom))
    monkeypatch.setitem(sys.modules, "torch", torch)
    hw = models.detect_hardware()
    assert hw["gpu"] is True and hw["gpu_name"] == "Mystery GPU"
    assert hw["vram_gb"] == 0.0


def test_detect_hardware_without_torch_reports_no_gpu(monkeypatch):
    monkeypatch.setitem(sys.modules, "torch", None)
    hw = models.detect_hardware()
    assert hw["gpu"] is False and hw["gpu_name"] == ""
    assert hw["vram_gb"] == 0.0


# --- download ------------------------------------------------------------


def fake_symlink_probe(monkeypatch, probe):
    """Install a fake ``huggingface_hub.file_download`` exposing only the probe.

    Registered under its dotted name so ``from huggingface_hub.file_download
    import …`` resolves it straight out of sys.modules — huggingface_hub is not
    in the CI dependency contract, so nothing here may import the real one."""
    mod = types.ModuleType("huggingface_hub.file_download")
    mod.are_symlinks_supported = probe
    monkeypatch.setitem(sys.modules, "huggingface_hub.file_download", mod)


def fake_hub(monkeypatch, snapshot_download):
    monkeypatch.setitem(
        sys.modules, "huggingface_hub",
        types.SimpleNamespace(snapshot_download=snapshot_download),
    )
    # …and the submodule the symlink pre-warm reaches for. Without this the
    # fetch path would find whatever `huggingface_hub.file_download` a real
    # install left in sys.modules and probe the tmp cache for real.
    fake_symlink_probe(monkeypatch, lambda _d: True)


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


# --- the symlink probe (WinError 1314) ------------------------------------
#
# huggingface_hub memoizes "can I symlink in this directory?" per directory,
# writing the dict optimistically-True before the trial os.symlink answers.
# snapshot_download's file workers all reach that dict at once on a fresh repo
# dir; one reading it inside the gap symlinks for real and dies with WinError
# 1314 on a box without Developer Mode. _settle_symlink_probe runs the probe
# serially first so the memo can no longer change under them.


def test_the_probe_asks_huggingface_about_the_repo_dir(monkeypatch, hf_cache):
    """The probed directory must be the one hub itself probes: the repo dir,
    the common parent of blobs/ and snapshots/ — not the cache root."""
    seen = []
    fake_symlink_probe(monkeypatch, lambda d: seen.append(d) or True)

    models._settle_symlink_probe("funasr/campplus")
    assert seen == [hf_cache / "models--funasr--campplus"]


def test_the_probe_follows_a_custom_cache_root(monkeypatch, tmp_path):
    """seed-vc's per-engine cache root is exactly where the field failure hit,
    so the pre-warm has to land under it, not under the default HF cache."""
    seen = []
    fake_symlink_probe(monkeypatch, lambda d: seen.append(d) or False)

    base = tmp_path / "seedvc" / "checkpoints"
    models._settle_symlink_probe("funasr/campplus", base)
    assert seen == [base / "models--funasr--campplus"]


def test_every_repo_is_probed_before_it_is_fetched(monkeypatch, isolated_env):
    """Per repo, in order: probe, then fetch. A probe that ran after its own
    snapshot_download would settle the memo too late to help anyone."""
    order = []
    monkeypatch.setattr(models, "_settle_symlink_probe",
                        lambda repo, base=None: order.append(("probe", repo)))

    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        order.append(("fetch", repo))

    fake_hub(monkeypatch, snapshot_download)
    asyncio.run(models.ModelManager().download("seed-vc", lambda *a: None))

    repos = models.spec("seed-vc").repos
    assert order == [step for r in repos for step in (("probe", r), ("fetch", r))]


def test_a_probe_that_cannot_run_never_blocks_the_download(monkeypatch, hf_cache):
    """Best-effort, always: the pre-warm is an optimization, and a download
    that refuses to start because of it would be a worse bug than the race."""
    def boom(_d):
        raise OSError("probe exploded")

    fake_symlink_probe(monkeypatch, boom)
    models._settle_symlink_probe("hexgrad/Kokoro-82M")  # must not raise

    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        fake_repo(hf_cache, repo)

    fake_hub(monkeypatch, snapshot_download)
    assert asyncio.run(models.ModelManager().download("kokoro", lambda *a: None)) is True


def test_a_missing_probe_never_blocks_the_download(monkeypatch, hf_cache):
    """An older/newer huggingface_hub without that function is a no-op, not a
    crash — the fetch just races the way it did before."""
    monkeypatch.setitem(sys.modules, "huggingface_hub.file_download", None)
    models._settle_symlink_probe("hexgrad/Kokoro-82M")  # must not raise

    def snapshot_download(repo, cache_dir=None, allow_patterns=None):
        fake_repo(hf_cache, repo)

    fake_hub(monkeypatch, snapshot_download)
    assert asyncio.run(models.ModelManager().download("kokoro", lambda *a: None)) is True


@pytest.fixture
def real_probe(monkeypatch):
    """The real huggingface_hub probe, with its per-directory memo emptied.

    Skipped where huggingface_hub isn't installed (the CI contract is numpy +
    soundfile + dbus-next + pytest); on a dev box it pins the behavior the fix
    actually depends on."""
    fd = pytest.importorskip("huggingface_hub.file_download")
    monkeypatch.setattr(fd, "_are_symlinks_supported_in_dir", {})
    return fd


# hub warns (once per directory) that it is falling back; that IS the branch
# under test, so it's expected output rather than suite noise
@pytest.mark.filterwarnings("ignore::UserWarning")
def test_a_box_without_the_privilege_settles_to_no_symlinks(monkeypatch, hf_cache, real_probe):
    """WinError 1314 is a bare OSError, which is what the trial symlink raises
    on a Windows box with neither Developer Mode nor admin."""
    import os as _os

    def denied(_src, _dst, **_kw):
        raise OSError(13, "A required privilege is not held by the client", None, 1314)

    monkeypatch.setattr(_os, "symlink", denied)
    monkeypatch.setattr(models, "_hf_cache", lambda: hf_cache)

    models._settle_symlink_probe("funasr/campplus")

    memo = real_probe._are_symlinks_supported_in_dir
    assert list(memo.values()) == [False]
    # settled, not merely seeded: the value cannot flip under the file workers
    models._settle_symlink_probe("funasr/campplus")
    assert list(memo.values()) == [False]


def test_a_developer_mode_box_keeps_native_symlinks(monkeypatch, hf_cache, real_probe):
    """The probe answers itself — where symlinks work the cache keeps hub's
    native layout (dedup across revisions), the fix only removes the race."""
    import os as _os

    monkeypatch.setattr(_os, "symlink", lambda src, dst, **kw: open(dst, "w").close())

    models._settle_symlink_probe("funasr/campplus", hf_cache)
    assert list(real_probe._are_symlinks_supported_in_dir.values()) == [True]


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
    fake_symlink_probe(monkeypatch, lambda _d: True)
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
    fake_symlink_probe(monkeypatch, lambda _d: True)

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
    fake_symlink_probe(monkeypatch, lambda _d: True)
    events = []
    ok = asyncio.run(models.ModelManager().download("kokoro", lambda *a: events.append(a)))
    assert ok is True
    assert events[0][2] == "downloading"
    assert events[-1] == ("kokoro", 1.0, "done")

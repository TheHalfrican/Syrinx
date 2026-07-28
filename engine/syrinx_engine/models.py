"""Model catalog, hardware detection, download manager and active-model selection.

One `ModelSpec` per downloadable model (mirrors Voicebox's ModelConfig registry).
A "download" is `huggingface_hub.snapshot_download` into the HF cache; "cached" =
the repo dir holds weight files with no `.incomplete` blobs. Progress is tracked
by polling the repo's on-disk byte growth against `size_mb`.

Active-model selection (which TTS engine/size, LLM size, STT model the engine
uses) is persisted to $SYRINX_DATA_DIR/models.json.

This module is also the authority on *readiness*: `require_weights` is the gate
every generation path passes through so that no model is ever fetched as a side
effect of pressing Generate (see "the readiness gate", below).
"""

import asyncio
import json
import logging
import os
import shutil
import sys
from dataclasses import dataclass
from fnmatch import fnmatch
from pathlib import Path

from . import vcsetup
from .profiles import _data_dir

log = logging.getLogger("syrinx.engine.models")

# What a row whose isolated venv hasn't been built yet says — the ⇄ converters
# and, since LuxTTS became one-click, a voice row too. OS-agnostic
# on purpose: the old text named engine/setup-vevo.sh, which is both a dead end
# for anyone who never opens a terminal and a path that doesn't exist on Windows.
# The Install button in the Models tab is now the answer, so the string points there.
VC_SETUP_NEEDED = "one-time setup needed — click Install"


@dataclass
class ModelSpec:
    id: str
    display: str
    category: str  # "voice" | "stt" | "llm" | "vc"
    engine: str  # kokoro|qwen|…|whisper|qwen_llm|chatterbox_vc|seed_vc|vevo_timbre
    size: str  # "1.7B" | "0.6B" | "base.en" | ""
    repos: list  # HF repo ids to fetch
    size_mb: int
    description: str
    gpu_recommended: bool = False
    min_ram_gb: float = 2.0
    supported: bool = True  # has a working backend in Syrinx today
    patterns: list = None  # snapshot_download allow_patterns (None = whole repo)
    min_vram_gb: float = 0.0  # 0 = CPU-viable / no VRAM headroom needed


# --- the catalog ------------------------------------------------------------
# Repos are the ones Syrinx actually loads (e.g. faster-whisper CT2 builds, not
# openai/whisper). `supported=False` = catalogued but no backend wired yet.
#
# min_vram_gb is advisory only (a warning row, never a gate). Derivation: sum
# the weight files the backend actually loads — the ONE variant it picks, not
# every checkpoint in the repo — at the precision it loads them (TADA casts its
# fp32 checkpoints to bf16; CT2 whisper is fp16 and its disk size is its VRAM
# size), add ~30% for activations/KV, round up to a half GB. Cross-engine
# extras count: seed-vc's whisper-small + bigvgan, Vevo2's ~1.4 GB whisper-med.
# 0 means "never warn" — kokoro and whisper-base are genuinely fine anywhere.

CATALOG: list = [
    # ---- Voice (TTS / cloning) ----
    ModelSpec("kokoro", "Kokoro 82M", "voice", "kokoro", "", ["hexgrad/Kokoro-82M"],
              350, "82M preset voices, 8 languages. CPU-realtime — great everywhere.",
              gpu_recommended=False, min_ram_gb=2.0, supported=True),
    ModelSpec("qwen-tts-1.7B", "Qwen TTS 1.7B", "voice", "qwen", "1.7B",
              ["Qwen/Qwen3-TTS-12Hz-1.7B-Base"], 4350,
              "Multilingual zero-shot voice cloning (10 langs). GPU strongly recommended.",
              gpu_recommended=True, min_ram_gb=8.0, supported=True, min_vram_gb=5.5),
    ModelSpec("qwen-tts-0.6B", "Qwen TTS 0.6B", "voice", "qwen", "0.6B",
              ["Qwen/Qwen3-TTS-12Hz-0.6B-Base"], 2400,
              "Lightweight Qwen voice cloning for lower-end hardware.",
              gpu_recommended=True, min_ram_gb=4.0, supported=True, min_vram_gb=3.5),
    ModelSpec("qwen-custom-voice-1.7B", "Qwen CustomVoice 1.7B", "voice", "qwen_custom_voice", "1.7B",
              ["Qwen/Qwen3-TTS-12Hz-1.7B-CustomVoice"], 4300,
              "9 preset voices + natural-language style control (instruct).",
              gpu_recommended=True, min_ram_gb=8.0, supported=True, min_vram_gb=5.5),
    ModelSpec("qwen-custom-voice-0.6B", "Qwen CustomVoice 0.6B", "voice", "qwen_custom_voice", "0.6B",
              ["Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice"], 2400,
              "Same 9 presets + instruct, lighter and faster.",
              gpu_recommended=True, min_ram_gb=4.0, supported=True, min_vram_gb=3.5),
    ModelSpec("luxtts", "LuxTTS", "voice", "luxtts", "", ["YatharthS/LuxTTS"], 1150,
              "ZipVoice-based, 48kHz, >150x realtime. CPU-friendly cloning, "
              "English. Runs in its own isolated environment — one-time setup.",
              gpu_recommended=False, min_ram_gb=2.0, supported=True, min_vram_gb=1.5),
    # chatterbox-tts installs --no-deps (stale pins); sub-deps in engine[chatterbox]
    ModelSpec("chatterbox", "Chatterbox (Multilingual)", "voice", "chatterbox", "",
              ["ResembleAI/chatterbox"], 13200,
              "23 languages with emotion exaggeration. GPU recommended.",
              gpu_recommended=True, min_ram_gb=8.0, supported=True, min_vram_gb=4.0),
    ModelSpec("chatterbox-turbo", "Chatterbox Turbo", "voice", "chatterbox_turbo", "",
              ["ResembleAI/chatterbox-turbo"], 3850,
              "350M English model with [laugh]/[cough] tags.",
              gpu_recommended=True, min_ram_gb=4.0, supported=True, min_vram_gb=4.0),
    # hume-tada installs --no-deps (stale torch pin); the Llama tokenizer
    # (ungated unsloth mirror, ~2 MB) is fetched by the backend at load time —
    # listing the repo here would drag in 2.5 GB of unused Llama weights and
    # break cached-detection (tokenizer-only repos have no weight files).
    # That side-fetch is a *disclosed exception* to the no-silent-downloads rule
    # below: ~2 MB, unavoidable without vendoring a tokenizer, and accepted on
    # exactly those terms — nothing else in the catalog may fetch behind a back.
    ModelSpec("tada-1b", "TADA 1B", "voice", "tada", "1B",
              ["HumeAI/tada-1b", "HumeAI/tada-codec"], 14000,
              "Llama-3.2-1B speech-LM, 700s+ coherent audio. English.",
              gpu_recommended=True, min_ram_gb=8.0, supported=True, min_vram_gb=5.0),
    ModelSpec("tada-3b-ml", "TADA 3B Multilingual", "voice", "tada", "3B",
              ["HumeAI/tada-3b-ml", "HumeAI/tada-codec"], 18700,
              "Llama-3.2-3B speech-LM, 10 languages. Heavy.",
              gpu_recommended=True, min_ram_gb=16.0, supported=True, min_vram_gb=8.0),
    # ---- Transcription (faster-whisper / CTranslate2) ----
    ModelSpec("whisper-base", "Whisper Base", "stt", "whisper", "base.en",
              ["Systran/faster-whisper-base.en"], 140,
              "74M params. Fast, moderate accuracy. English.",
              gpu_recommended=False, min_ram_gb=2.0, supported=True),
    ModelSpec("whisper-small", "Whisper Small", "stt", "whisper", "small",
              ["Systran/faster-whisper-small"], 460,
              "244M params. Balanced speed/accuracy, multilingual.",
              gpu_recommended=False, min_ram_gb=2.0, supported=True, min_vram_gb=1.0),
    ModelSpec("whisper-medium", "Whisper Medium", "stt", "whisper", "medium",
              ["Systran/faster-whisper-medium"], 1450,
              "769M params. Higher accuracy, multilingual.",
              gpu_recommended=False, min_ram_gb=4.0, supported=True, min_vram_gb=2.0),
    ModelSpec("whisper-large", "Whisper Large v3", "stt", "whisper", "large-v3",
              ["Systran/faster-whisper-large-v3"], 2950,
              "1.5B params. Best accuracy, multilingual.",
              gpu_recommended=True, min_ram_gb=6.0, supported=True, min_vram_gb=4.0),
    ModelSpec("whisper-turbo", "Whisper Turbo", "stt", "whisper", "large-v3-turbo",
              ["deepdml/faster-whisper-large-v3-turbo-ct2"], 1550,
              "Pruned large-v3: near-large accuracy, much faster.",
              gpu_recommended=False, min_ram_gb=4.0, supported=True, min_vram_gb=2.0),
    # ---- Language models (compose / rewrite) ----
    ModelSpec("qwen3-0.6b", "Qwen3 0.6B", "llm", "qwen_llm", "0.6B", ["Qwen/Qwen3-0.6B"],
              1450, "Very fast on CPU. Good for short compose/rewrite.",
              gpu_recommended=False, min_ram_gb=3.0, supported=True, min_vram_gb=2.0),
    ModelSpec("qwen3-1.7b", "Qwen3 1.7B", "llm", "qwen_llm", "1.7B", ["Qwen/Qwen3-1.7B"],
              3900, "Balanced quality. Usable on CPU, snappy on GPU.",
              gpu_recommended=False, min_ram_gb=6.0, supported=True, min_vram_gb=5.0),
    ModelSpec("qwen3-4b", "Qwen3 4B", "llm", "qwen_llm", "4B", ["Qwen/Qwen3-4B"],
              7700, "Highest-quality local rewrites. GPU recommended.",
              gpu_recommended=True, min_ram_gb=12.0, supported=True, min_vram_gb=10.0),

    # ---- Voice conversion (the ⇄ Voice Converter tab) ----
    # No "active" concept: the converter's model dropdown picks per conversion,
    # so these rows only download / report / delete weights.
    ModelSpec("chatterbox-vc", "Chatterbox VC", "vc", "chatterbox_vc", "",
              ["ResembleAI/chatterbox"], 1000,
              "Style-preserved conversion — the S3 half of Chatterbox. Shares its "
              "weights with Chatterbox (Multilingual).",
              gpu_recommended=False, min_ram_gb=4.0, supported=True, min_vram_gb=1.5,
              patterns=["s3gen.safetensors", "conds.pt"]),
    ModelSpec("seed-vc", "Seed-VC", "vc", "seed_vc", "",
              ["Plachta/Seed-VC", "funasr/campplus",
               "nvidia/bigvgan_v2_22khz_80band_256x", "openai/whisper-small"], 9250,
              "Diffusion conversion, speech + singing (f0). Runs in its own "
              "isolated environment — one-time setup.",
              gpu_recommended=True, min_ram_gb=6.0, supported=True, min_vram_gb=3.0,
              # skip the tf/flax duplicates of whisper-small
              patterns=["*.safetensors", "*.bin", "*.pt", "*.pth", "*.json",
                        "*.txt", "*.yml", "*.yaml", "*.model"]),
    ModelSpec("vevo-timbre", "Vevo-Timbre", "vc", "vevo_timbre", "",
              ["amphion/Vevo"], 2650,
              "Amphion's timbre-only converter — keeps the source delivery most "
              "literally. Runs in its own isolated environment — one-time setup. "
              "Non-commercial weights.",
              gpu_recommended=True, min_ram_gb=8.0, supported=True, min_vram_gb=3.5,
              patterns=["tokenizer/vq8192/*", "acoustic_modeling/Vq8192ToMels/*",
                        "acoustic_modeling/Vocoder/*"]),
    # FM-only subset of RMSnow/Vevo2 — keep patterns in sync with
    # vevo_worker.py's VEVO2_PATTERNS (the 6+ GB AR stacks never load)
    ModelSpec("vevo2-singing", "Vevo2 (singing)", "vc", "vevo_timbre", "",
              ["RMSnow/Vevo2"], 2830,
              "Amphion's Vevo2 singing converter — experimental: results vary "
              "wildly with the material, from striking to comically bad "
              "(Seed-VC articulates lyrics better and stays the ♫ default); "
              "first conversion also fetches whisper-medium (~1.5 GB). "
              "Runs in its own isolated environment — one-time setup. "
              "Non-commercial weights.",
              gpu_recommended=True, min_ram_gb=8.0, supported=True, min_vram_gb=5.5,
              patterns=["tokenizer/contentstyle_fvq16384_12.5hz/*",
                        "acoustic_modeling/fm_emilia101k_singnet7k_repa/*",
                        "vocoder/*"]),
]

_BY_ID = {m.id: m for m in CATALOG}


def spec(model_id: str):
    return _BY_ID.get(model_id)


# --- hardware ---------------------------------------------------------------

def _total_ram_gb() -> float:
    """Total physical RAM in GiB, cross-platform, zero new deps.

    Linux/macOS use ``os.sysconf`` (the historical path — value byte-identical
    to before). Windows has no ``sysconf``, so fall back to the Win32
    ``GlobalMemoryStatusEx`` via ctypes. 0.0 when no source is available."""
    try:
        return round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / (1024**3), 1)
    except (AttributeError, ValueError, OSError):
        pass
    if sys.platform == "win32":
        try:
            import ctypes

            class _MemoryStatusEx(ctypes.Structure):
                _fields_ = [
                    ("dwLength", ctypes.c_ulong),
                    ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong),
                    ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong),
                    ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong),
                    ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
                ]

            stat = _MemoryStatusEx()
            stat.dwLength = ctypes.sizeof(_MemoryStatusEx)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(stat)):
                return round(stat.ullTotalPhys / (1024**3), 1)
        except Exception:  # noqa: BLE001
            pass
    return 0.0


def detect_hardware() -> dict:
    cores = os.cpu_count() or 1
    ram_gb = _total_ram_gb()
    gpu = False
    gpu_name = ""
    vram_gb = 0.0
    try:
        import torch

        if torch.cuda.is_available():
            gpu = True
            gpu_name = torch.cuda.get_device_name(0)
            vram_gb = round(torch.cuda.get_device_properties(0).total_memory / 1024**3, 1)
    except Exception:  # noqa: BLE001
        pass
    return {"cores": cores, "ram_gb": ram_gb, "gpu": gpu, "gpu_name": gpu_name,
            "vram_gb": vram_gb}


def hardware_warning(m: "ModelSpec", hw: dict) -> str:
    """A short warning if the machine is below the model's recommended specs."""
    warns = []
    if m.gpu_recommended and not hw["gpu"]:
        warns.append("no GPU detected — will be slow on CPU")
    if hw["ram_gb"] and hw["ram_gb"] < m.min_ram_gb:
        warns.append(f"needs ~{m.min_ram_gb:g} GB RAM (have {hw['ram_gb']:g})")
    # GPU boxes only: on a CPU-only box the gpu_recommended clause already says
    # everything, and a VRAM figure would be noise. vram_gb 0 = unknown.
    if hw["gpu"] and m.min_vram_gb and hw.get("vram_gb") and hw["vram_gb"] < m.min_vram_gb:
        warns.append(
            f"needs ~{m.min_vram_gb:g} GB VRAM (have {hw['vram_gb']:g}) — "
            "expect very slow or failed loads"
        )
    return "; ".join(warns)


# --- HF cache inspection ----------------------------------------------------

def _hf_cache() -> Path:
    try:
        from huggingface_hub.constants import HF_HUB_CACHE

        return Path(HF_HUB_CACHE)
    except Exception:  # noqa: BLE001
        return Path.home() / ".cache" / "huggingface" / "hub"


# seed-vc downloads through its own package into this two-tier layout under
# the worker's cwd (the seedvc data dir) — encoded here so the Models tab
# pre-fetches / reports / deletes the exact files the worker uses. Everything
# else (incl. chatterbox-vc and the migrated vevo weights) uses the standard
# HF cache.
_SEEDVC_CACHE = {
    "Plachta/Seed-VC": "checkpoints",
    "funasr/campplus": "checkpoints",
    "nvidia/bigvgan_v2_22khz_80band_256x": "checkpoints/hf_cache",
    "openai/whisper-small": "checkpoints/hf_cache",
}

def _cache_root(m, repo: str):
    """Cache base for a spec's repo (None = the default HF cache)."""
    if m is not None and m.id == "seed-vc":
        return _data_dir() / "seedvc" / _SEEDVC_CACHE.get(repo, "checkpoints")
    return None


def _repo_dir(repo: str, base: "Path | None" = None) -> Path:
    return (base or _hf_cache()) / ("models--" + repo.replace("/", "--"))


def _repo_bytes(repo: str, base: "Path | None" = None) -> int:
    blobs = _repo_dir(repo, base) / "blobs"
    if not blobs.exists():
        return 0
    return sum(f.stat().st_size for f in blobs.glob("*") if f.is_file())


def _settle_symlink_probe(repo: str, base: "Path | None" = None) -> None:
    """Answer huggingface_hub's symlink question ONCE, serially, before its
    parallel file workers can ask it at the same time.

    `huggingface_hub` links every downloaded file from `snapshots/` to the blob
    it just wrote, and decides whether it *can* symlink by probing the directory
    the two share — which is the repo dir, `<cache>/models--<org>--<name>`. The
    answer is memoized in a plain dict (file_download's
    `_are_symlinks_supported_in_dir`) that is written **twice**: optimistically
    `True` on entry, then the real answer after the trial `os.symlink`. That gap
    is not locked, and nothing holds it shut. On a fresh
    repo dir, snapshot_download's eight file workers all reach `_create_symlink`
    within milliseconds of each other, so a worker that reads the memo inside
    the gap is told "symlinks work", calls `os.symlink`, and — on Windows
    without Developer Mode or admin — dies with

        OSError: [WinError 1314] A required privilege is not held by the client

    which nothing in `_create_symlink` catches (it only handles FileExistsError
    and PermissionError, and 1314 is neither). Field failure 2026-07-28: the
    seed-vc row died at ~4%, starting its second repo (funasr/campplus) into a
    brand-new cache dir.

    Serializing whole fetches (`ModelManager._fetch_lock`, 2026-07-24) closed
    the *cross-repo* half of this; calling the probe here closes the *within-
    repo* half. One serial call settles the memo, and every file worker that
    follows reads a value that can no longer change.

    Deliberately hub's own probe rather than a hand-rolled one: it is the
    function `_create_symlink` actually consults, keyed by the same
    `Path(...).resolve()` string, so pre-warming it is exact. It is also
    self-answering — a Developer-Mode box probes `True` and keeps the native
    symlink layout (whose real win is dedup across revisions), a locked-down box
    probes `False` and takes the no-symlink path, which stores each file **once,
    directly in `snapshots/`** (blobs stay empty). That layout does not double
    the cache; see packaging/WINDOWS.md.

    Unconditional rather than `sys.platform == "win32"`-gated: the race is in
    the memo, not in Windows, and one code path is one thing to reason about.
    Everything here is best-effort — a pre-warm that cannot run must never be
    the reason a download doesn't start.
    """
    try:
        from huggingface_hub.file_download import are_symlinks_supported
    except Exception:  # noqa: BLE001
        log.debug("symlink probe unavailable; skipping pre-warm", exc_info=True)
        return
    try:
        are_symlinks_supported(_repo_dir(repo, base))
    except Exception:  # noqa: BLE001
        log.debug("symlink probe failed for %s; letting the fetch try", repo, exc_info=True)


# Kept as a name here because vevo_worker.py and this module have to agree about
# where the clone is; the resolution itself lives with the rest of the setup
# knowledge in vcsetup.py.
_amphion_dir = vcsetup.amphion_dir


def _setup_id(m: "ModelSpec") -> str:
    """The setup that unblocks this row ("" = nothing to install)."""
    return vcsetup.ENGINE_TO_SETUP.get(m.engine, "")


def _vc_setup_warning(m: "ModelSpec") -> str:
    """Engines that live in isolated venvs need a one-time setup.

    Delegating the "is it there?" question to vcsetup means the warning, the
    Install button and the backends all read the same probe — the old copy here
    checked for the venv *directory* and called a torn install "ready"."""
    setup_id = _setup_id(m)
    if setup_id and not vcsetup.installed(setup_id):
        return VC_SETUP_NEEDED
    return ""


def is_repo_cached(repo: str, base: "Path | None" = None) -> bool:
    d = _repo_dir(repo, base)
    if not d.exists():
        return False
    blobs = d / "blobs"
    if blobs.exists() and any(blobs.glob("*.incomplete")):
        return False  # a download is in progress / was interrupted
    snaps = d / "snapshots"
    if not snaps.exists():
        return False
    weight_ext = (".safetensors", ".bin", ".pt", ".pth", ".npz", ".ckpt", ".onnx", ".gguf")
    for f in snaps.rglob("*"):
        if f.name.endswith(weight_ext):
            return True
    return False


def is_cached(m: "ModelSpec") -> bool:
    return all(is_repo_cached(r, _cache_root(m, r)) for r in m.repos)


# --- the readiness gate: no weights nobody asked for ------------------------
#
# Disk space is spent by explicit choice, never as a side effect of pressing
# Generate. Every ML library under this engine will happily fetch multi-GB
# weights on first use, which is how a single ⇄ Convert click could quietly
# cost 9 GB — so the components ask this gate before they load anything.
#
# The gate is deliberately shy. It refuses only when it can name the exact
# catalog row AND that row's files are missing; an engine or handle it doesn't
# recognize (a raw HF repo, a hand-set $SYRINX_* override) passes straight
# through. Guessing which row a stranger meant, and then refusing on the guess,
# would break every legitimate off-catalog model for no safety gained.


class ModelNotDownloaded(RuntimeError):
    """Weights the user never chose to download, refused instead of fetched.

    ``str(exc)`` is a complete, actionable sentence: it is shown verbatim in the
    app's generation-error banner, so nothing downstream may truncate or reword
    it (see ``core._failure_text``).
    """


def spec_for(category: str, engine: str, size: str = ""):
    """The catalog row for a (category, engine, size) triple — None if unknown.

    *size* is matched against :attr:`ModelSpec.size` first and, failing that,
    against the row's repo ids, because the components hold different handles on
    the same row: the TTS router remembers ``"1.7B"`` while the Transcriber
    remembers the faster-whisper repo it was handed.

    ``""`` means "this component never recorded a size", and the answer is the
    catalog's FIRST row for that engine — which is the variant a backend built
    with ``size=""`` loads (qwen 1.7B, tada 1B, whisper base.en). Keep the
    catalog ordered that way or this quietly starts naming the wrong row.

    ``None`` is an answer, not a failure: see the section note above.
    """
    rows = [m for m in CATALOG if m.category == category and m.engine == engine]
    if not rows:
        return None
    if not size:
        return rows[0]
    for m in rows:
        if m.size == size:
            return m
    for m in rows:
        if size in m.repos:
            return m
    return None


def downloaded_engines(category: str) -> set:
    """Engine names in *category* with at least one fully-downloaded row.

    Engine-level on purpose — the callers ask "can this engine speak at all?",
    and a row half-fetched (one of two repos) is not an engine.
    """
    return {m.engine for m in CATALOG if m.category == category and is_cached(m)}


# The ⇄ converter picks an ENGINE and a MODE; the catalog holds ROWS, and the
# two are not one-to-one — vevo_timbre is Vevo-Timbre for speech and Vevo2 for
# singing, sharing one engine name, one venv and one Amphion clone. This map is
# the single place that correspondence is written down (the app mirrors it in
# VC_SPEECH_ROWS/VC_MUSIC_ROWS), and it is what finally makes vevo2-singing's
# row state consultable at all. Only the pairs the ⇄ view can actually produce
# are listed: chatterbox_vc has no music pipeline, so it has no music entry.
VC_ROW_FOR = {
    ("chatterbox_vc", "speech"): "chatterbox-vc",
    ("seed_vc", "speech"): "seed-vc",
    ("seed_vc", "music"): "seed-vc",
    ("vevo_timbre", "speech"): "vevo-timbre",
    ("vevo_timbre", "music"): "vevo2-singing",
}


def _size_hint(m: "ModelSpec") -> str:
    """The download's cost in the unit a human recognizes. Under a gigabyte,
    "0.1 GB" reads as a rounding error rather than as a real file to fetch."""
    if m.size_mb >= 1024:
        return f"{m.size_mb / 1024:.1f} GB"
    return f"{m.size_mb} MB"


def require_weights(category: str, engine: str, size: str = "",
                    model_id: str = "") -> None:
    """Refuse to run on weights that were never downloaded.

    *model_id* names the row outright, for callers that already know it (the ⇄
    converter, via :data:`VC_ROW_FOR`); everyone else describes the row and lets
    :func:`spec_for` find it. Raises :class:`ModelNotDownloaded`; a no-op when
    the row is unknown or already on disk.
    """
    m = spec(model_id) if model_id else spec_for(category, engine, size)
    if m is None or is_cached(m):
        return
    raise ModelNotDownloaded(
        f"{m.display} isn't downloaded yet — open Models and click Download "
        f"on its row ({_size_hint(m)})."
    )


# --- honest download totals -------------------------------------------------
# size_mb is a stale catalog estimate; the poll bar needs the real byte total.


def _pattern_allows(path: str, patterns) -> bool:
    """fnmatch a repo file against allow_patterns exactly as snapshot_download's
    filter does: None = whole repo, a trailing "/" gets an implicit "*", and a
    match on any one pattern admits the file (fnmatch's "*" spans "/")."""
    if patterns is None:
        return True
    return any(fnmatch(path, p + "*" if p.endswith("/") else p) for p in patterns)


def _expected_bytes(m: "ModelSpec") -> "int | None":
    """Real download size: sum HF file metadata across m.repos, keeping only the
    files m.patterns would fetch. None on ANY failure (offline, gated, rate-limit,
    missing sizes) so the caller falls back to size_mb — metadata must never break
    a download. Blocking (network); call off-loop."""
    try:
        from huggingface_hub import HfApi

        api = HfApi()
        total = 0
        for repo in m.repos:
            info = api.model_info(repo, files_metadata=True)
            for s in info.siblings or []:
                if not _pattern_allows(s.rfilename, m.patterns):
                    continue
                if s.size is None:
                    return None  # incomplete metadata — don't trust a partial sum
                total += s.size
        return total or None
    except Exception:  # noqa: BLE001
        log.debug("expected-bytes metadata unavailable for %s", m.id, exc_info=True)
        return None


# --- manager: download / delete / active selection --------------------------

_DEFAULT_ACTIVE = {"voice": "kokoro", "stt": "whisper-base", "llm": "qwen3-1.7b"}


class ModelManager:
    def __init__(self) -> None:
        self._settings = _data_dir() / "models.json"
        self._active = dict(_DEFAULT_ACTIVE)
        self._downloading: set = set()
        # Concurrent snapshot_downloads race huggingface_hub's per-directory
        # symlink-support probe (WinError 1314 on boxes without Developer
        # Mode) — fetches must run one at a time. Queued downloads still
        # appear in `_downloading` immediately, so status() shows them.
        # This lock only covers repo-against-repo; the file workers *inside*
        # one snapshot_download race the same probe, which is what
        # `_settle_symlink_probe` is for.
        self._fetch_lock = asyncio.Lock()
        try:
            self._active.update(json.loads(self._settings.read_text()))
        except Exception:  # noqa: BLE001
            pass

    def _save(self) -> None:
        try:
            self._settings.write_text(json.dumps(self._active, indent=2))
        except Exception:  # noqa: BLE001
            log.exception("save models.json failed")

    # active selection ---------------------------------------------------
    def active_id(self, category: str) -> str:
        return self._active.get(category, _DEFAULT_ACTIVE.get(category, ""))

    def active_spec(self, category: str):
        return spec(self.active_id(category))

    def set_active(self, model_id: str) -> str:
        """Persist the active model for its category; returns the category."""
        m = spec(model_id)
        if not m:
            return ""
        self._active[m.category] = model_id
        self._save()
        return m.category

    # status -------------------------------------------------------------
    def status(self) -> list:
        hw = detect_hardware()
        rows = []
        for m in CATALOG:
            # setup_id/needs_setup drive the row's Install button; the warning
            # stays the human sentence. Weights and engine are independent — a
            # row can need setup AND still be downloaded, or vice versa.
            setup_warning = _vc_setup_warning(m)
            rows.append({
                "id": m.id, "display": m.display, "category": m.category,
                "engine": m.engine, "size": m.size, "size_mb": m.size_mb,
                "description": m.description, "gpu_recommended": m.gpu_recommended,
                "min_ram_gb": m.min_ram_gb, "min_vram_gb": m.min_vram_gb,
                "supported": m.supported,
                "downloaded": is_cached(m),
                "downloading": m.id in self._downloading,
                "active": self._active.get(m.category) == m.id,
                "setup_id": _setup_id(m),
                "needs_setup": bool(setup_warning),
                "warning": setup_warning or hardware_warning(m, hw),
            })
        return rows

    # download / delete --------------------------------------------------
    async def download(self, model_id: str, on_progress) -> bool:
        m = spec(model_id)
        if not m or m.id in self._downloading:
            return False
        self._downloading.add(m.id)
        loop = asyncio.get_running_loop()
        # Prefer the real HF metadata total over the stale size_mb estimate, but
        # never let a metadata failure block the fetch — fall back to size_mb.
        total = await loop.run_in_executor(None, _expected_bytes, m)
        if not total:
            total = max(1, m.size_mb) * 1024 * 1024
        done = asyncio.Event()

        async def poll() -> None:
            while not done.is_set():
                got = sum(_repo_bytes(r, _cache_root(m, r)) for r in m.repos)
                # bytes on disk cover the expected total but snapshot_download is
                # still working (checksums / renames / trailing files): finalizing.
                stage = "finalizing" if got >= total else "downloading"
                on_progress(model_id, min(0.999, got / total), stage)
                try:
                    await asyncio.wait_for(done.wait(), timeout=0.5)
                except asyncio.TimeoutError:
                    pass

        def fetch() -> None:
            from huggingface_hub import snapshot_download

            for r in m.repos:
                base = _cache_root(m, r)
                # Settle the symlink question for THIS repo dir before the
                # eight file workers inside snapshot_download can race it.
                _settle_symlink_probe(r, base)
                snapshot_download(
                    r,
                    cache_dir=str(base) if base else None,
                    allow_patterns=m.patterns,
                )

        poll_task = asyncio.create_task(poll())
        ok = True
        try:
            async with self._fetch_lock:
                await loop.run_in_executor(None, fetch)
        except Exception:  # noqa: BLE001
            log.exception("download %s failed", model_id)
            ok = False
        finally:
            done.set()
            await poll_task
            self._downloading.discard(m.id)
        on_progress(model_id, 1.0 if ok else 0.0, "done" if ok else "error")
        return ok

    def delete(self, model_id: str) -> None:
        m = spec(model_id)
        if not m:
            return
        for r in m.repos:
            d = _repo_dir(r, _cache_root(m, r))
            if d.exists():
                shutil.rmtree(d, ignore_errors=True)

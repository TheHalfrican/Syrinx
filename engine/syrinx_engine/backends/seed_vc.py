"""Seed-VC backend — voice conversion via an isolated worker (.venv-seedvc).

Seed-VC is GPL-3.0, so it is NEVER vendored into this repo: the package lives
in its own venv and `seedvc_worker.py` (a thin adapter over its public API)
runs as a subprocess speaking JSON over stdin/stdout — the same pattern as
LuxTTS. It also pins numpy<2, which alone rules out the engine venv.

Zero-shot, style-preserved, and — unlike ChatterboxVC — f0-conditioned
singing conversion is available (the "f0" request flag; wired for the music
mode later). CPU-usable; the worker auto-selects CUDA on the GPU box.
"""

import asyncio
import json
import logging
import os
import sys
from pathlib import Path

from . import detect_device
from .chatterbox import combined_ref_wav
from .chatterbox_vc import check_source_cap
from ..paths import data_dir, worker_log_path

log = logging.getLogger("syrinx.engine.vc.seedvc")

_HERE = Path(__file__).resolve()
_ENGINE_DIR = _HERE.parents[2]  # .../engine
# venv layout is per-OS: POSIX puts the interpreter in bin/python, Windows in
# Scripts/python.exe. The Linux string is byte-identical to before this seam.
if sys.platform == "win32":
    _SEEDVC_PY = _ENGINE_DIR / ".venv-seedvc" / "Scripts" / "python.exe"
else:
    _SEEDVC_PY = _ENGINE_DIR / ".venv-seedvc" / "bin" / "python"
_WORKER = _HERE.parents[1] / "seedvc_worker.py"
_STDERR_LOG = worker_log_path("seedvc")


def _steps() -> int:
    """Diffusion steps — Settings-tab value wins; env stays the fallback."""
    from .. import settings

    try:
        env = int(os.environ.get("SYRINX_SEEDVC_STEPS", "25"))
    except ValueError:
        env = 25
    try:
        return int(settings.value("seedvc_steps", env))
    except (TypeError, ValueError):
        return env


class SeedVCBackend:
    """Seed-VC behind the small async VC interface:
    check_source / load / convert / unload / invalidate_profile."""

    engine_name = "seed_vc"
    supports_cloning = False  # conversion only

    def __init__(self) -> None:
        self.device = detect_device()
        self.model_size = "default"
        self._proc = None
        self._lock = asyncio.Lock()
        self._req_id = 0
        root = data_dir()
        self._voices_dir = root / "voices"
        self._voices_dir.mkdir(parents=True, exist_ok=True)
        # seed-vc downloads models to ./checkpoints relative to CWD — pin the
        # worker's cwd here so weights never land in (and get committed to)
        # whatever directory the engine happened to start from
        self._work_dir = root / "seedvc"
        self._work_dir.mkdir(parents=True, exist_ok=True)

    def check_source(self, source_wav: str) -> None:
        check_source_cap(source_wav)

    async def _ensure_worker(self) -> None:
        if self._proc is not None and self._proc.returncode is None:
            return
        if not _SEEDVC_PY.exists():
            raise RuntimeError(
                "Seed-VC is not installed on this machine (.venv-seedvc missing)"
            )
        _STDERR_LOG.parent.mkdir(parents=True, exist_ok=True)
        errfile = open(_STDERR_LOG, "ab")
        self._proc = await asyncio.create_subprocess_exec(
            str(_SEEDVC_PY), str(_WORKER),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=errfile,
            cwd=str(self._work_dir),
        )
        log.info("Seed-VC worker started (pid %s)", self._proc.pid)

    async def load(self) -> None:
        await self._ensure_worker()

    def unload(self) -> None:
        """Stop the worker (it exits when stdin closes), freeing its memory."""
        if self._proc is not None:
            try:
                self._proc.stdin.close()
            except Exception:  # noqa: BLE001
                pass
            self._proc = None

    def invalidate_profile(self, profile_id: str) -> None:
        (self._voices_dir / f"{profile_id}_cbxref.wav").unlink(missing_ok=True)

    async def _request(self, payload: dict, on_stage=None) -> tuple[bytes, int]:
        """One worker round-trip; interim {"stage": …} lines hit *on_stage*."""
        async with self._lock:
            await self._ensure_worker()
            self._req_id += 1
            rid = self._req_id
            payload = dict(payload, id=rid)
            self._proc.stdin.write((json.dumps(payload) + "\n").encode())
            await self._proc.stdin.drain()
            while True:
                line = await self._proc.stdout.readline()
                if not line:
                    self._proc = None
                    raise RuntimeError(
                        f"Seed-VC worker exited (see {_STDERR_LOG})"
                    )
                try:
                    resp = json.loads(line.decode())
                except json.JSONDecodeError:
                    log.debug("seedvc worker noise: %s", line.decode(errors="replace").rstrip())
                    continue
                if resp.get("id") != rid:
                    log.debug("seedvc stale reply %s (want %s)", resp.get("id"), rid)
                    continue
                if "stage" in resp:
                    if on_stage:
                        on_stage(resp["stage"])
                    continue
                break
        if not resp.get("ok"):
            raise RuntimeError(f"Seed-VC conversion failed: {resp.get('error')}")
        raw_path = Path(resp["raw"])
        rate = int(resp["rate"])
        pcm = raw_path.read_bytes()
        raw_path.unlink(missing_ok=True)
        return pcm, rate

    async def convert(
        self, source_wav: str, profile, *, f0: bool = False, semitone: int = 0
    ) -> tuple[bytes, int]:
        """Re-render *source_wav* in *profile*'s voice; (pcm_f32_bytes, rate)."""
        self.check_source(source_wav)
        ref = combined_ref_wav(profile, self._voices_dir)
        log.info("convert [%s] -> profile %s (f0=%s)", self.engine_name, profile.id, f0)
        return await self._request({
            "source": str(source_wav), "target": ref,
            "f0": f0, "steps": _steps(), "auto_f0": True, "semitone": semitone,
        })

    async def convert_music(
        self, source_wav: str, profile, *, on_stage=None, semitone: int = 0
    ) -> tuple[bytes, int]:
        """Song cover: demucs vocal split → f0 conversion → remix over the
        instrumental. Stage names ("separating"/"converting"/"remixing")
        stream to *on_stage* as the worker progresses."""
        self.check_source(source_wav)
        ref = combined_ref_wav(profile, self._voices_dir)
        log.info("convert-music [%s] -> profile %s", self.engine_name, profile.id)
        return await self._request({
            "cmd": "music", "source": str(source_wav), "target": ref,
            # singing wants more diffusion steps (upstream recommends 30-50).
            # auto_f0 stays OFF for covers: it transposes the melody to the
            # target's median pitch — an arbitrary interval that lands the
            # vocal in a different key than the instrumental it's mixed over.
            "steps": max(_steps(), 30), "auto_f0": False, "semitone": semitone,
        }, on_stage=on_stage)

"""Vevo backend — voice conversion via an isolated worker (.venv-vevo).

Amphion's Vevo-Timbre is the most literal "style-preserved" converter in the
stack: it imitates ONLY the reference timbre and explicitly preserves the
source's prosody, emotion and accent. Amphion is MIT code with no pip
package, so setup-vevo.sh clones it outside the repo and `vevo_worker.py`
imports its pipeline from there (JSON-over-stdio, the LuxTTS pattern).
Checkpoints are CC-BY-NC: personal use, auto-downloaded, never redistributed.

Vevo2 (unified speech+singing) will ride this same worker/venv as a request
mode when the music pipeline lands.
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

log = logging.getLogger("syrinx.engine.vc.vevo")

_HERE = Path(__file__).resolve()
_ENGINE_DIR = _HERE.parents[2]  # .../engine
# venv layout is per-OS: POSIX puts the interpreter in bin/python, Windows in
# Scripts/python.exe. The Linux string is byte-identical to before this seam.
if sys.platform == "win32":
    _VEVO_PY = _ENGINE_DIR / ".venv-vevo" / "Scripts" / "python.exe"
else:
    _VEVO_PY = _ENGINE_DIR / ".venv-vevo" / "bin" / "python"
_WORKER = _HERE.parents[1] / "vevo_worker.py"
_STDERR_LOG = worker_log_path("vevo")


def _steps() -> int:
    try:
        return int(os.environ.get("SYRINX_VEVO_STEPS", "32"))
    except ValueError:
        return 32


class VevoTimbreBackend:
    """Vevo-Timbre behind the small async VC interface:
    check_source / load / convert / unload / invalidate_profile."""

    engine_name = "vevo_timbre"
    supports_cloning = False  # conversion only

    def __init__(self) -> None:
        self.device = detect_device()
        self.model_size = "default"
        self._proc = None
        self._lock = asyncio.Lock()
        self._req_id = 0
        self._voices_dir = data_dir() / "voices"
        self._voices_dir.mkdir(parents=True, exist_ok=True)

    def check_source(self, source_wav: str) -> None:
        check_source_cap(source_wav)

    async def _ensure_worker(self) -> None:
        if self._proc is not None and self._proc.returncode is None:
            return
        if not _VEVO_PY.exists():
            raise RuntimeError(
                "Vevo is not installed on this machine — run engine/setup-vevo.sh"
            )
        _STDERR_LOG.parent.mkdir(parents=True, exist_ok=True)
        errfile = open(_STDERR_LOG, "ab")
        self._proc = await asyncio.create_subprocess_exec(
            str(_VEVO_PY), str(_WORKER),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=errfile,
        )
        log.info("Vevo worker started (pid %s)", self._proc.pid)

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
                        f"Vevo worker exited (see {_STDERR_LOG})"
                    )
                try:
                    resp = json.loads(line.decode())
                except json.JSONDecodeError:
                    log.debug("vevo worker noise: %s", line.decode(errors="replace").rstrip())
                    continue
                if resp.get("id") != rid:
                    log.debug("vevo stale reply %s (want %s)", resp.get("id"), rid)
                    continue
                if "stage" in resp:
                    if on_stage:
                        on_stage(resp["stage"])
                    continue
                break
        if not resp.get("ok"):
            raise RuntimeError(f"Vevo conversion failed: {resp.get('error')}")
        raw_path = Path(resp["raw"])
        rate = int(resp["rate"])
        pcm = raw_path.read_bytes()
        raw_path.unlink(missing_ok=True)
        return pcm, rate

    async def convert(self, source_wav: str, profile) -> tuple[bytes, int]:
        """Re-render *source_wav* in *profile*'s timbre; (pcm_f32_bytes, rate)."""
        self.check_source(source_wav)
        ref = combined_ref_wav(profile, self._voices_dir)
        log.info("convert [%s] -> profile %s", self.engine_name, profile.id)
        return await self._request({
            "source": str(source_wav), "target": ref, "steps": _steps(),
        })

    async def convert_music(
        self, source_wav: str, profile, *, on_stage=None, semitone: int = 0
    ) -> tuple[bytes, int]:
        """Song cover via Vevo2: demucs vocal split → singing conversion →
        remix over the instrumental. Rides this same worker as a request
        mode; stage names stream to *on_stage* as it progresses."""
        self.check_source(source_wav)
        ref = combined_ref_wav(profile, self._voices_dir)
        log.info("convert-music [%s/vevo2] -> profile %s", self.engine_name, profile.id)
        return await self._request({
            "cmd": "music", "source": str(source_wav), "target": ref,
            "steps": _steps(), "semitone": semitone,
        }, on_stage=on_stage)

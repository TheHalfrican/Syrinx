"""LuxTTS backend — proxies to an isolated worker process (.venv-luxtts).

Zipvoice/LuxTTS pins conflicting deps (transformers<=4.57, torchaudio 2.11), so
it lives in a separate venv rather than polluting the main engine. This backend
spawns `luxtts_worker.py` under that venv's python and talks to it over a
stdin/stdout JSON line protocol; audio returns as a raw float32 PCM temp file.
CPU-friendly zero-shot cloning — the one cloning engine usable on this box.
"""

import asyncio
import json
import logging
from pathlib import Path

import numpy as np

from . import detect_device
from .. import chunking
from ..paths import worker_log_path

log = logging.getLogger("syrinx.engine.tts.luxtts")

_HERE = Path(__file__).resolve()
_ENGINE_DIR = _HERE.parents[2]  # .../engine
_LUX_PY = _ENGINE_DIR / ".venv-luxtts" / "bin" / "python"
_WORKER = _HERE.parents[1] / "luxtts_worker.py"  # .../engine/syrinx_engine/luxtts_worker.py
_STDERR_LOG = worker_log_path("luxtts")


class LuxTTSBackend:
    supports_cloning = True

    def __init__(self) -> None:
        self.device = detect_device()
        self.model_size = "default"
        self._proc = None
        self._lock = asyncio.Lock()
        self._req_id = 0

    async def _ensure_worker(self) -> None:
        if self._proc is not None and self._proc.returncode is None:
            return
        if not _LUX_PY.exists():
            raise RuntimeError("LuxTTS not installed (.venv-luxtts missing)")
        _STDERR_LOG.parent.mkdir(parents=True, exist_ok=True)
        errfile = open(_STDERR_LOG, "ab")
        self._proc = await asyncio.create_subprocess_exec(
            str(_LUX_PY), str(_WORKER),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=errfile,
        )
        log.info("LuxTTS worker started (pid %s)", self._proc.pid)

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

    async def list_voices(self) -> list:
        return []  # cloning-only, no presets

    async def synthesize(self, text: str, voice_id: str, instruct: str = "") -> tuple:
        raise ValueError("LuxTTS has no preset voices")

    async def _request(self, sample, text: str) -> tuple:
        """One worker round-trip (caller holds the lock)."""
        await self._ensure_worker()
        self._req_id += 1
        rid = self._req_id
        payload = json.dumps({"id": rid, "sample": str(sample), "text": text}) + "\n"
        self._proc.stdin.write(payload.encode())
        await self._proc.stdin.drain()
        while True:
            line = await self._proc.stdout.readline()
            if not line:
                self._proc = None
                raise RuntimeError(f"LuxTTS worker exited (see {_STDERR_LOG})")
            try:
                resp = json.loads(line.decode())
            except json.JSONDecodeError:
                # stray library print that escaped the worker's redirect
                log.debug("luxtts worker noise: %s", line.decode(errors="replace").rstrip())
                continue
            if resp.get("id") != rid:
                # reply to a request whose awaiter was cancelled — drop it
                log.debug("luxtts stale reply %s (want %s)", resp.get("id"), rid)
                continue
            break
        if not resp.get("ok"):
            raise RuntimeError(f"LuxTTS synth failed: {resp.get('error')}")
        raw_path = Path(resp["raw"])
        rate = int(resp["rate"])
        pcm = raw_path.read_bytes()
        raw_path.unlink(missing_ok=True)
        return pcm, rate

    async def synthesize_profile(self, profile, text: str, instruct: str = "") -> tuple:
        if not profile.samples:
            raise ValueError(f"profile {profile.id} has no reference samples")
        sample = profile.samples[0].audio_path
        # Long text synthesizes per sentence-boundary chunk: flow-matching
        # memory grows steeply with target duration (a 2-minute text OOM-killed
        # the worker on the 15 GB box). The worker caches the encoded prompt by
        # sample path, so per-chunk overhead is just the synthesis itself.
        chunks = chunking.split_text_into_chunks(text, chunking.max_chunk_chars())
        async with self._lock:
            if len(chunks) <= 1:
                pcm, rate = await self._request(sample, text)
                log.info("synthesize_profile [luxtts] (%s): %r", profile.id, text[:60])
                return pcm, rate
            log.info(
                "synthesize_profile [luxtts] (%s): %d chars -> %d chunks",
                profile.id, len(text), len(chunks),
            )
            parts: list[np.ndarray] = []
            rate = 48000
            for i, chunk in enumerate(chunks, 1):
                log.info("luxtts chunk %d/%d (%d chars)", i, len(chunks), len(chunk))
                pcm, rate = await self._request(sample, chunk)
                parts.append(np.frombuffer(pcm, dtype=np.float32))
        audio = chunking.crossfade_concat(parts, rate)
        return audio.tobytes(), rate

    def invalidate_profile(self, profile_id: str) -> None:
        pass  # the worker caches prompts by sample path

"""Text-to-speech router.

Voices come from two places:
  - **built-in presets** from a preset engine (Kokoro) — always available.
  - **user profiles** (ProfileStore) — preset or cloned.

`synthesize(voice_id, ...)` routes each voice to the right backend:
  - "builtin:<engine>:<voice>"  -> that preset engine
  - a profile id (preset)        -> the profile's preset engine
  - a profile id (cloned)        -> the ACTIVE cloning engine (`clone_engine`),
                                    which builds a voice prompt from the samples

There is exactly one authority for that last line. A cloned profile used to be
able to pin its own `default_engine` and win here; that made two code paths
argue about what generates while the composer's dropdown displayed a third
answer. `default_engine` is now an app-side *seed* — it preselects the composer
dropdown when you pick the voice, and this module never reads it.

Backends live in `backends/` and are selected per-voice, lazily instantiated.
`SYRINX_TTS_ENGINE` still sets the default CLONING engine for new cloned voices.

Importing `models` here is safe: models pulls in `profiles` and `vcsetup` only,
neither of which knows this module exists, so there is no cycle to trip over.
"""

import logging
import os

from . import models
from .backends import VoiceInfo, detect_device, make_backend
from .profiles import ProfileStore

log = logging.getLogger("syrinx.engine.tts")

# Preset engine whose built-in voices are always offered in the voice list.
BUILTIN_PRESET_ENGINE = "kokoro"
# Default engine used when cloning a new voice.
DEFAULT_CLONE_ENGINE = os.environ.get("SYRINX_TTS_ENGINE", "qwen")
if DEFAULT_CLONE_ENGINE == "kokoro":  # kokoro can't clone; fall back
    DEFAULT_CLONE_ENGINE = "qwen"
# Engines capable of zero-shot cloning (preset-only engines can't be the
# active clone engine, e.g. kokoro / qwen_custom_voice).
CLONING_ENGINES = {"qwen", "luxtts", "chatterbox", "chatterbox_turbo", "tada"}
# Preset engines beyond the always-on builtin whose voices join the voice list
# whenever their weights are on disk (see list_voices — downloaded, not active).
EXTRA_PRESET_ENGINES = {"qwen_custom_voice"}
# Voice-conversion engines (the ⇄ tab) — audio→audio, never in the voice
# list. Vevo2 (unified speech+singing) rides the vevo worker as a mode when
# the music pipeline lands.
VC_ENGINES = {"chatterbox_vc", "seed_vc", "vevo_timbre"}
DEFAULT_VC_ENGINE = "chatterbox_vc"


def _engine_display(engine: str) -> str:
    """The catalog's name for an engine, for sentences a user reads."""
    m = models.spec_for("voice", engine)
    return m.display if m else engine


class SpeechSynthesizer:
    def __init__(self, profiles: ProfileStore) -> None:
        self._profiles = profiles
        self._backends: dict[str, object] = {}
        self.backend = detect_device()  # exposed as the D-Bus Backend property
        self.supports_cloning = True
        # The composer's engine pick for cloned voices; "" falls through to the
        # env default. Nothing else overrides it — see the module docstring.
        self._clone_engine = ""
        # The (engine, size) that pick last resolved to, so a re-pick of what is
        # already picked costs nothing (see set_voice_engine).
        self._voice_engine = ""
        # Model size per engine (qwen 1.7B/0.6B, tada 1B/3B, …), recorded from
        # the composer so picking a size variant actually takes effect.
        self._voice_sizes: dict[str, str] = {}

    def set_clone_engine(self, engine: str) -> None:
        self._clone_engine = engine if engine in CLONING_ENGINES else ""
        log.info("active clone engine -> %r", self._clone_engine or DEFAULT_CLONE_ENGINE)

    def set_voice_engine(self, engine: str, size: str = "") -> None:
        """The active voice model — one at a time, picked in the composer.

        Record the size, make it the clone engine, evict what is no longer
        selected. There is no extra-preset-engine branch any more: CustomVoice's
        voices are listed from what is DOWNLOADED (see :meth:`list_voices`), not
        from what happens to be active, so this doesn't have to know it exists.
        """
        if engine == self._voice_engine and size == self._voice_sizes.get(engine, ""):
            # Re-picking what is already picked used to cost every OTHER voice
            # backend its VRAM: the app re-sends the active model whenever you
            # select a voice, so the common case was an eviction sweep — and a
            # multi-second reload of the engine you were already using — for a
            # change that wasn't a change.
            return
        self._voice_engine = engine
        if size:
            self._voice_sizes[engine] = size
            be = self._backends.get(engine)
            if be is not None and getattr(be, "model_size", size) != size:
                # same engine, different size — rebuild on next use
                self._backends.pop(engine)
                unload = getattr(be, "unload", None)
                if unload:
                    unload()
                log.info("%s backend will reload at size %s", engine, size)
        self.set_clone_engine(engine)
        self._evict_voice_backends(keep={engine})

    def _evict_voice_backends(self, keep: set) -> None:
        """Unload voice backends that are no longer selected so their VRAM
        comes back — seven GPU engines don't fit on one card. Profiles pinned
        to an evicted engine reload it on their next generation."""
        evicted = []
        for name in list(self._backends):
            if name == BUILTIN_PRESET_ENGINE or name in keep:
                continue
            be = self._backends.pop(name)
            unload = getattr(be, "unload", None)
            try:
                if unload:
                    unload()
                evicted.append(name)
            except Exception:  # noqa: BLE001
                log.exception("unload %s failed", name)
        if evicted:
            log.info("evicted voice backends: %s", ", ".join(evicted))

    @property
    def clone_engine(self) -> str:
        return self._clone_engine or DEFAULT_CLONE_ENGINE

    def vc_backend(self, engine: str = ""):
        """Voice-conversion backend (ConvertVoice). Lives in the shared
        backend dict, so the Models-tab eviction sweep reclaims its VRAM
        too; it reloads lazily on the next convert."""
        engine = engine or DEFAULT_VC_ENGINE
        if engine not in VC_ENGINES:
            raise ValueError(
                f"unknown VC engine {engine!r} "
                f"(expected: {', '.join(sorted(VC_ENGINES))})"
            )
        # one conversion engine on the card at a time: the ⇄ dropdown swaps
        # engines freely (bake-offs), and a resident sibling worker (vevo
        # holds ~10 GB) plus the TTS/STT/LLM stack OOMs the next engine's
        # load. Evict before loading; the evicted one reloads on next use.
        evicted = []
        for name in [n for n in self._backends if n in VC_ENGINES and n != engine]:
            be = self._backends.pop(name)
            try:
                unload = getattr(be, "unload", None)
                if unload:
                    unload()
                evicted.append(name)
            except Exception:  # noqa: BLE001
                log.exception("unload %s failed", name)
        if evicted:
            log.info("evicted VC backends: %s", ", ".join(evicted))
        if engine not in self._backends:
            if engine == "seed_vc":
                from .backends.seed_vc import SeedVCBackend

                self._backends[engine] = SeedVCBackend()
            elif engine == "vevo_timbre":
                from .backends.vevo import VevoTimbreBackend

                self._backends[engine] = VevoTimbreBackend()
            else:
                from .backends.chatterbox_vc import ChatterboxVCBackend

                self._backends[engine] = ChatterboxVCBackend()
        return self._backends[engine]

    def _be(self, engine: str):
        if engine not in self._backends:
            self._backends[engine] = make_backend(engine, self._voice_sizes.get(engine, ""))
        return self._backends[engine]

    def _require(self, engine: str) -> None:
        """Refuse to generate on weights nobody chose to download.

        The guard sits on the GENERATION paths and deliberately NOT in
        :meth:`load`: warming the built-in preset engine at boot is the one
        disclosed exception to the no-silent-downloads rule — Kokoro's pipeline
        fetches on construction, and a fresh install that refused it would boot
        with no voices at all. That is the accepted price of entry, and it is
        also why this guard never fires for kokoro in practice: by the time
        anything generates, warmup has already put it on disk.
        """
        models.require_weights("voice", engine, size=self._voice_sizes.get(engine, ""))

    async def load(self) -> None:
        # Warm the built-in preset engine so preset voices are instant.
        # Unguarded on purpose — see _require's docstring.
        await self._be(BUILTIN_PRESET_ENGINE).load()

    async def list_voices(self) -> list[VoiceInfo]:
        voices: list[VoiceInfo] = []
        for v in await self._be(BUILTIN_PRESET_ENGINE).list_voices():
            voices.append(VoiceInfo(f"builtin:{BUILTIN_PRESET_ENGINE}:{v.id}", v.name))
        # CustomVoice's nine presets are offered whenever its weights are on
        # disk. This used to be gated on it being the ACTIVE voice model, which
        # the selection redesign turns into a dead end: with the Models tab's
        # "Use" button gone, nothing would ever make CustomVoice active, so its
        # voices could never appear in the grid you pick a voice FROM. Gating on
        # inventory instead also makes the grid's content a pure function of
        # what is installed — no hidden mode decides what you can see.
        for engine in sorted(EXTRA_PRESET_ENGINES & models.downloaded_engines("voice")):
            for v in await self._be(engine).list_voices():
                voices.append(VoiceInfo(f"builtin:{engine}:{v.id}", v.name))
        for p in self._profiles.list():
            voices.append(VoiceInfo(p.id, p.name))
        return voices

    async def synthesize(self, text: str, voice_id: str, instruct: str = "") -> tuple[bytes, int]:
        if voice_id.startswith("builtin:"):
            _, engine, vid = voice_id.split(":", 2)
            self._require(engine)
            return await self._be(engine).synthesize(text, vid, instruct)

        prof = self._profiles.get(voice_id)
        if prof is None:
            # Back-compat: treat an unknown id as a raw built-in preset voice.
            self._require(BUILTIN_PRESET_ENGINE)
            return await self._be(BUILTIN_PRESET_ENGINE).synthesize(text, voice_id, instruct)

        if prof.voice_type == "preset":
            engine = prof.preset_engine or BUILTIN_PRESET_ENGINE
            self._require(engine)
            return await self._be(engine).synthesize(text, prof.preset_voice_id, instruct)

        # Cloned. One authority, and the composer is it: this read
        # `prof.default_engine or self.clone_engine`, which is exactly how the
        # dropdown could show one engine while another spoke. default_engine is
        # an app-side seed now and is never consulted here.
        engine = self.clone_engine
        self._require(engine)
        be = self._be(engine)
        if not hasattr(be, "synthesize_profile"):
            # CLONING_ENGINES is a static list and models.json is a plain file:
            # an MCP caller or a hand edit can still make a preset-only engine
            # the active voice model, and "AttributeError: synthesize_profile"
            # is not a sentence anybody can act on.
            raise ValueError(
                f"{_engine_display(engine)} has no voice cloning — "
                "pick a cloning model in the composer"
            )
        return await be.synthesize_profile(prof, text, instruct)

    async def clone(self, name: str, sample_path: str, ref_text: str = "") -> str:
        """Legacy CloneVoice: create a cloned profile with a single sample."""
        # default_engine stays "": the voice speaks on the composer's pick
        # either way, and an empty seed means the dropdown keeps whatever was
        # already selected when you choose this voice.
        pid = self._profiles.create(name, "cloned")
        self._profiles.add_sample(pid, sample_path, ref_text)
        return pid

    def invalidate_profile(self, profile_id: str) -> None:
        """Drop any cached clone prompt for a profile (e.g. after samples change)."""
        for be in self._backends.values():
            inv = getattr(be, "invalidate_profile", None)
            if inv:
                inv(profile_id)

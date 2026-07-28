"""tts.py routing — which backend actually speaks a given voice.

The selection redesign gave every category ONE authority. For cloned voices that
authority is the composer's pick (``clone_engine``); a profile's
``default_engine`` is an app-side seed the engine never reads. These tests hold
that line and the guards around it: nothing generates on weights nobody
downloaded, nothing tries to clone on an engine that can't, and re-picking the
model you already picked doesn't burn down the backend cache.

Backends are fakes pre-seeded into the router's own cache, so ``_be`` never
reaches ``make_backend`` and nothing here imports torch.
"""

import asyncio

import pytest

from syrinx_engine import models
from syrinx_engine import tts as tts_mod
from syrinx_engine.backends import VoiceInfo
from syrinx_engine.profiles import ProfileStore
from syrinx_engine.tts import SpeechSynthesizer
from test_models import fake_repo

RATE = 24_000


class PresetBackend:
    """A preset-only backend (kokoro's shape) — no ``synthesize_profile``."""

    def __init__(self, name="kokoro", size="", voices=(("af_heart", "Heart"),)):
        self.engine_name = name
        self.model_size = size
        self.calls = []
        self.profile_calls = []
        self.unloaded = False
        self._voices = [VoiceInfo(vid, vname) for vid, vname in voices]

    async def load(self):
        return None

    async def list_voices(self):
        return list(self._voices)

    async def synthesize(self, text, voice_id, instruct=""):
        self.calls.append((text, voice_id, instruct))
        return b"pcm", RATE

    def unload(self):
        self.unloaded = True


class CloningBackend(PresetBackend):
    async def synthesize_profile(self, profile, text, instruct=""):
        self.profile_calls.append((profile.id, text, instruct))
        return b"pcm", RATE


@pytest.fixture
def router():
    return SpeechSynthesizer(ProfileStore())


def download(hf_cache, model_id):
    """Put a catalog row's weights in the (fake) HF cache."""
    for repo in models.spec(model_id).repos:
        fake_repo(hf_cache, repo)


def speak(router, text, voice_id):
    return asyncio.run(router.synthesize(text, voice_id))


# --- one authority: the composer's pick ----------------------------------


def test_a_pinned_cloned_profile_still_follows_the_composer(router, hf_cache):
    """The bug this whole redesign turns on: the profile's default_engine used
    to win at generation time, so the composer could display Qwen while LuxTTS
    spoke. The pin is a seed for the dropdown now and nothing more."""
    download(hf_cache, "qwen-tts-1.7B")
    router.set_voice_engine("qwen", "1.7B")
    qwen = CloningBackend("qwen", "1.7B")
    router._backends["qwen"] = qwen
    pid = router._profiles.create("Piccolo", "cloned", default_engine="luxtts")

    speak(router, "hi", pid)

    assert qwen.profile_calls == [(pid, "hi", "")]
    assert "luxtts" not in router._backends  # never even considered


def test_an_unpinned_cloned_profile_follows_it_too(router, hf_cache):
    download(hf_cache, "luxtts")
    router.set_voice_engine("luxtts")
    lux = CloningBackend("luxtts")
    router._backends["luxtts"] = lux
    pid = router._profiles.create("Nail", "cloned")

    speak(router, "hi", pid)

    assert lux.profile_calls == [(pid, "hi", "")]


def test_builtin_and_preset_voices_ignore_the_clone_engine(router, hf_cache):
    """Only cloned voices route through the composer's pick — a preset voice
    can only ever speak on the engine that owns it."""
    download(hf_cache, "kokoro")
    kokoro = PresetBackend("kokoro")
    qwen = CloningBackend("qwen", "1.7B")
    router._backends.update({"kokoro": kokoro, "qwen": qwen})
    router.set_clone_engine("qwen")

    speak(router, "hi", "builtin:kokoro:af_heart")
    pid = router._profiles.create("Preset", "preset", preset_engine="kokoro",
                                  preset_voice_id="af_bella")
    speak(router, "yo", pid)
    speak(router, "hm", "not-a-profile-id")  # back-compat: a raw preset voice

    assert kokoro.calls == [("hi", "af_heart", ""), ("yo", "af_bella", ""),
                            ("hm", "not-a-profile-id", "")]
    assert qwen.profile_calls == []


def test_a_cloned_voice_on_a_preset_only_engine_says_what_to_do(router, hf_cache):
    """Reachable by hand-editing models.json or through an MCP caller, since
    CLONING_ENGINES is a static list. "AttributeError: synthesize_profile" is
    not something a user can act on; this sentence is."""
    download(hf_cache, "kokoro")
    router._backends["kokoro"] = PresetBackend("kokoro")
    router._clone_engine = "kokoro"
    pid = router._profiles.create("Piccolo", "cloned")

    with pytest.raises(ValueError) as e:
        speak(router, "hi", pid)
    assert str(e.value) == ("Kokoro 82M has no voice cloning — "
                            "pick a cloning model in the composer")


def test_cloning_engines_is_the_pinned_set():
    """Mirrored app-side (B4's is_cloning_engine) — the two must not drift, and
    every member has to be a real voice row for the composer to offer it."""
    assert tts_mod.CLONING_ENGINES == {
        "qwen", "luxtts", "chatterbox", "chatterbox_turbo", "tada"}
    assert all(models.spec_for("voice", e) is not None
               for e in tts_mod.CLONING_ENGINES)


# --- no silent downloads -------------------------------------------------


def test_generation_on_undownloaded_weights_never_reaches_the_backend(router):
    router.set_voice_engine("qwen", "1.7B")
    qwen = CloningBackend("qwen", "1.7B")
    router._backends["qwen"] = qwen
    pid = router._profiles.create("Piccolo", "cloned")

    with pytest.raises(models.ModelNotDownloaded) as e:
        speak(router, "hi", pid)
    assert str(e.value) == (
        "Qwen TTS 1.7B isn't downloaded yet — open Models and click Download "
        "on its row (4.2 GB).")
    assert qwen.profile_calls == []


def test_the_guard_follows_the_picked_size(router, hf_cache):
    """Downloading 1.7B doesn't make 0.6B available — the picked size is what
    the backend would fetch, so it's what the gate asks about."""
    download(hf_cache, "qwen-tts-1.7B")
    router.set_voice_engine("qwen", "0.6B")
    router._backends["qwen"] = CloningBackend("qwen", "0.6B")
    pid = router._profiles.create("Piccolo", "cloned")

    with pytest.raises(models.ModelNotDownloaded, match="Qwen TTS 0.6B"):
        speak(router, "hi", pid)

    download(hf_cache, "qwen-tts-0.6B")
    speak(router, "hi", pid)  # now it goes


def test_a_preset_voice_is_gated_too(router):
    """Kokoro is only ever ungated during warmup (the disclosed exception) —
    the generation path asks like everyone else."""
    router._backends["kokoro"] = PresetBackend("kokoro")
    with pytest.raises(models.ModelNotDownloaded, match="Kokoro 82M"):
        speak(router, "hi", "builtin:kokoro:af_heart")


def test_warmup_itself_stays_unguarded(router):
    """The accepted price of entry: a fresh install with nothing downloaded
    still warms the built-in preset engine, or it boots with no voices."""
    router._backends["kokoro"] = PresetBackend("kokoro")
    asyncio.run(router.load())  # must not raise


# --- the voice list is a function of inventory ---------------------------


def test_customvoice_presets_appear_once_their_weights_are_on_disk(router, hf_cache):
    """Gated on DOWNLOADED, not on being the active model: with the Models
    tab's "Use" button gone nothing can make CustomVoice active, so an
    active-gate would put its nine voices permanently out of reach."""
    download(hf_cache, "kokoro")
    router._backends["kokoro"] = PresetBackend("kokoro")
    router._backends["qwen_custom_voice"] = PresetBackend(
        "qwen_custom_voice", voices=(("ethan", "Ethan"),))

    ids = [v.id for v in asyncio.run(router.list_voices())]
    assert ids == ["builtin:kokoro:af_heart"]

    # any size of the row counts — the gate is per engine, not per variant
    download(hf_cache, "qwen-custom-voice-0.6B")
    ids = [v.id for v in asyncio.run(router.list_voices())]
    assert ids == ["builtin:kokoro:af_heart", "builtin:qwen_custom_voice:ethan"]


def test_profiles_are_listed_whatever_is_downloaded(router, hf_cache):
    download(hf_cache, "kokoro")
    router._backends["kokoro"] = PresetBackend("kokoro")
    pid = router._profiles.create("Piccolo", "cloned")
    assert [v.id for v in asyncio.run(router.list_voices())][-1] == pid


# --- eviction: only when something actually changed ----------------------


def test_repicking_the_same_model_evicts_nothing(router):
    """The app re-sends the active model on every voice selection, so this was
    a routine bonfire: every other backend unloaded, and the engine you were
    already using reloaded from scratch, for a change that wasn't one."""
    router.set_voice_engine("qwen", "1.7B")
    victim = CloningBackend("tada", "1B")
    router._backends.update({"tada": victim, "qwen": CloningBackend("qwen", "1.7B")})

    router.set_voice_engine("qwen", "1.7B")

    assert "tada" in router._backends and victim.unloaded is False
    assert "qwen" in router._backends


def test_picking_a_different_engine_still_evicts(router):
    router.set_voice_engine("qwen", "1.7B")
    victim = CloningBackend("tada", "1B")
    router._backends["tada"] = victim

    router.set_voice_engine("luxtts")

    assert "tada" not in router._backends and victim.unloaded is True
    assert router.clone_engine == "luxtts"


def test_a_size_change_drops_the_cached_backend(router):
    """The size-discard bug's other half: picking 0.6B has to actually rebuild
    the backend, not leave 1.7B resident and speaking."""
    router.set_voice_engine("qwen", "1.7B")
    be = CloningBackend("qwen", "1.7B")
    router._backends["qwen"] = be

    router.set_voice_engine("qwen", "0.6B")

    assert "qwen" not in router._backends and be.unloaded is True
    assert router._voice_sizes["qwen"] == "0.6B"


def test_the_builtin_preset_engine_survives_every_eviction(router):
    """Kokoro is 350 MB and instant; keeping it resident is what makes preset
    voices feel free."""
    router._backends["kokoro"] = PresetBackend("kokoro")
    router.set_voice_engine("qwen", "1.7B")
    router.set_voice_engine("tada", "1B")
    assert "kokoro" in router._backends

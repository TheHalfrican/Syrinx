"""The ``sh.syrinx.Engine1`` D-Bus interface.

Thin wrapper: every ``@method()`` delegates to the transport-agnostic
:class:`~syrinx_engine.core.EngineCore`; the core's emitter seam is wired to the
dbus_next ``@signal()`` emissions and ``emit_properties_changed`` here. No engine
logic lives in this module — see ``core.py``. The interface name, method
signatures, signal signatures, and semantics are byte-for-byte what they were
before the core was extracted (Linux D-Bus must stay identical).
"""

import logging

from dbus_next.service import ServiceInterface, method, signal, dbus_property
from dbus_next.constants import PropertyAccess

from .core import EngineCore, _PlayCtl  # noqa: F401  (_PlayCtl re-exported for tests)

log = logging.getLogger("syrinx.engine.service")


class EngineInterface(ServiceInterface):
    def __init__(self) -> None:
        super().__init__("sh.syrinx.Engine1")
        self._core = EngineCore()
        # Wire the core's emitter seam to D-Bus signal emission + properties.
        # Dispatching through getattr(self, name) means a test that monkeypatches
        # a signal method on the instance still intercepts the emission.
        self._core._emit = lambda name, *a: getattr(self, name)(*a)
        self._core._emit_props = self.emit_properties_changed

    @property
    def backend_name(self) -> str:
        return self._core.backend_name  # "cuda" | "rocm" | "cpu"

    async def warmup(self) -> None:
        """Load models in the background, then flip ModelLoaded."""
        await self._core.warmup()

    # --- Methods --------------------------------------------------------

    @method()
    async def Speak(self, text: "s", voice_id: "s") -> "u":  # noqa: F821
        return await self._core.Speak(text, voice_id)

    @method()
    async def Transcribe(self, audio_path: "s") -> "s":  # noqa: F821
        return await self._core.Transcribe(audio_path)

    @method()
    async def TranscribeFile(self, audio_path: "s") -> "u":  # noqa: F821
        """Async transcription for long files — Transcribe blocks the D-Bus
        reply (~25 s cap). Partial text streams via TranscribeProgress; the
        final text arrives in TranscribeResult ("" on failure)."""
        return await self._core.TranscribeFile(audio_path)

    @method()
    async def ConvertVoice(self, audio_path: "s", profile_id: "s", engine: "s", label: "s", transcript: "s", mode: "s", semitones: "i") -> "u":  # noqa: F821
        """Style-preserved voice conversion (the ⇄ tab): re-render the speech
        in *audio_path* with a cloned profile's voice, keeping the source's
        delivery (words/timing/prosody — only the timbre changes). *engine*
        "" = the default (chatterbox_vc; seed_vc when *mode* is "music").
        *mode* "music" runs the song pipeline: demucs vocal split →
        f0-conditioned conversion → remix over the instrumental; *semitones*
        shifts the sung melody (octave steps keep the song's key — register
        wrangling for deep/high voices; ignored for speech mode). The history
        row stores *transcript* (the source's words) as its text and folds
        *label* into the display name ("<voice> · <label>"). Returns a
        generation id; progress and errors arrive via GenerationProgress, and
        the result auto-plays and lands in history exactly like Speak."""
        return await self._core.ConvertVoice(audio_path, profile_id, engine, label, transcript, mode, semitones)

    @method()
    async def ListVoices(self) -> "a(ss)":  # noqa: F821
        return await self._core.ListVoices()

    @method()
    async def CloneVoice(self, name: "s", sample_path: "s", ref_text: "s") -> "s":  # noqa: F821
        return await self._core.CloneVoice(name, sample_path, ref_text)

    # --- voice profiles (JSON payloads for the structured bits) ---------

    @method()
    async def CreateProfile(self, spec_json: "s") -> "s":  # noqa: F821
        return await self._core.CreateProfile(spec_json)

    @method()
    async def ListProfiles(self) -> "s":  # noqa: F821
        return await self._core.ListProfiles()

    @method()
    async def GetProfile(self, profile_id: "s") -> "s":  # noqa: F821
        return await self._core.GetProfile(profile_id)

    @method()
    async def UpdateProfile(self, profile_id: "s", patch_json: "s") -> None:  # noqa: F821
        await self._core.UpdateProfile(profile_id, patch_json)

    @method()
    async def DeleteProfile(self, profile_id: "s") -> None:  # noqa: F821
        await self._core.DeleteProfile(profile_id)

    @method()
    async def SetProfileAvatar(
        self, profile_id: "s", src: "s", mode: "s", sx: "i", sy: "i", sw: "i", sh: "i"  # noqa: F821
    ) -> None:
        """Attach an avatar photo + crop rect (circle|panel); empty src re-crops."""
        await self._core.SetProfileAvatar(profile_id, src, mode, sx, sy, sw, sh)

    @method()
    async def ExportProfile(self, profile_id: "s", dest: "s") -> None:  # noqa: F821
        await self._core.ExportProfile(profile_id, dest)

    @method()
    async def ImportProfile(self, src: "s") -> "s":  # noqa: F821
        return await self._core.ImportProfile(src)

    @method()
    async def AddSample(self, profile_id: "s", audio_path: "s", reference_text: "s") -> "s":  # noqa: F821
        return await self._core.AddSample(profile_id, audio_path, reference_text)

    @method()
    async def DeleteSample(self, sample_id: "s") -> None:  # noqa: F821
        await self._core.DeleteSample(sample_id)

    @method()
    async def UpdateSampleText(self, profile_id: "s", sample_id: "s", text: "s") -> None:  # noqa: F821
        """Correct a sample's reference transcript (clone prompts rebuild)."""
        await self._core.UpdateSampleText(profile_id, sample_id, text)

    # --- personality LLM (compose / rewrite) ---------------------------

    @method()
    async def ComposeProfile(self, voice_id: "s", prompt: "s") -> "u":  # noqa: F821
        return await self._core.ComposeProfile(voice_id, prompt)

    @method()
    async def RewriteProfile(self, voice_id: "s", text: "s") -> "u":  # noqa: F821
        return await self._core.RewriteProfile(voice_id, text)

    @method()
    async def RefineTranscript(self, text: "s") -> "u":  # noqa: F821
        """Clean a dictation transcript via the LLM; result via LlmResult."""
        return await self._core.RefineTranscript(text)

    # --- model management ----------------------------------------------

    @method()
    async def ListModels(self) -> "s":  # noqa: F821
        return await self._core.ListModels()

    @method()
    async def Hardware(self) -> "s":  # noqa: F821
        return await self._core.Hardware()

    @method()
    async def DownloadModel(self, model_id: "s") -> "b":  # noqa: F821
        return await self._core.DownloadModel(model_id)

    @method()
    async def DeleteModel(self, model_id: "s") -> None:  # noqa: F821
        await self._core.DeleteModel(model_id)

    @method()
    async def SetActiveModel(self, model_id: "s") -> "s":  # noqa: F821
        return await self._core.SetActiveModel(model_id)

    # --- generation history --------------------------------------------

    @method()
    async def ListHistory(self) -> "s":  # noqa: F821
        return await self._core.ListHistory()

    @method()
    async def PlayHistory(self, hid: "s") -> "u":  # noqa: F821
        return await self._core.PlayHistory(hid)

    @method()
    async def PlayHistoryAt(self, hid: "s", pct: "d") -> "u":  # noqa: F821
        return await self._core.PlayHistoryAt(hid, pct)

    @method()
    async def PlaySample(self, sample_id: "s") -> "u":  # noqa: F821
        """Audition a profile reference sample through the normal player."""
        return await self._core.PlaySample(sample_id)

    @method()
    async def PausePlayback(self) -> None:
        await self._core.PausePlayback()

    @method()
    async def ResumePlayback(self) -> None:
        await self._core.ResumePlayback()

    @method()
    async def SeekPlayback(self, pct: "d") -> None:  # noqa: F821
        await self._core.SeekPlayback(pct)

    @method()
    async def SetVolume(self, volume: "d") -> None:  # noqa: F821
        await self._core.SetVolume(volume)

    # --- effects --------------------------------------------------------

    @method()
    async def ListEffectPresets(self) -> "s":  # noqa: F821
        return await self._core.ListEffectPresets()

    @method()
    async def SetEffect(self, preset_id: "s") -> None:  # noqa: F821
        await self._core.SetEffect(preset_id)

    @method()
    async def SetStyle(self, instruct: "s") -> None:  # noqa: F821
        """Delivery direction baked into generations ("" = neutral).

        Free-text natural-language instruct ("Speak in an extremely angry
        tone…"). Honored by the qwen engines; engines without style control
        ignore it.
        """
        await self._core.SetStyle(instruct)

    @method()
    async def ApplyHistoryEffects(self, hid: "s", preset_id: "s") -> "s":  # noqa: F821
        """Re-process a stored clip through a preset; saves a NEW history row."""
        return await self._core.ApplyHistoryEffects(hid, preset_id)

    # --- effect chain editor -------------------------------------------

    @method()
    async def ListEffects(self) -> "s":  # noqa: F821
        """Effect definitions (label, params with default/min/max/step)."""
        return await self._core.ListEffects()

    @method()
    async def GetEffectPreset(self, preset_id: "s") -> "s":  # noqa: F821
        """Full preset incl. chain ("" if unknown)."""
        return await self._core.GetEffectPreset(preset_id)

    @method()
    async def CreateEffectPreset(self, name: "s", description: "s", chain_json: "s") -> "s":  # noqa: F821
        """New user preset; returns id ("" on invalid chain / duplicate name)."""
        return await self._core.CreateEffectPreset(name, description, chain_json)

    @method()
    async def UpdateEffectPreset(self, preset_id: "s", name: "s", description: "s", chain_json: "s") -> "b":  # noqa: F821
        """Rewrite a user preset in place (builtins are immutable)."""
        return await self._core.UpdateEffectPreset(preset_id, name, description, chain_json)

    @method()
    async def DeleteEffectPreset(self, preset_id: "s") -> "b":  # noqa: F821
        return await self._core.DeleteEffectPreset(preset_id)

    @method()
    async def PreviewEffects(self, hid: "s", chain_json: "s") -> "u":  # noqa: F821
        """Play a stored clip through an ad-hoc chain (nothing is saved)."""
        return await self._core.PreviewEffects(hid, chain_json)

    @method()
    async def StarHistory(self, hid: "s", starred: "b") -> None:  # noqa: F821
        await self._core.StarHistory(hid, starred)

    @method()
    async def SetHistoryTags(self, hid: "s", tags_json: "s") -> None:  # noqa: F821
        """Replace a history row's tags (JSON array of strings)."""
        await self._core.SetHistoryTags(hid, tags_json)

    @method()
    async def DeleteHistory(self, hid: "s") -> None:  # noqa: F821
        await self._core.DeleteHistory(hid)

    @method()
    async def RegenerateHistory(self, hid: "s") -> "u":  # noqa: F821
        """Re-run the generation behind a history row. TTS rows re-speak
        their text; conversion rows re-run the conversion — but only while
        the exact source take still exists (0 when it's gone or overwritten,
        so the app can say why instead of re-speaking the transcript)."""
        return await self._core.RegenerateHistory(hid)

    @method()
    async def ExportPackage(self, hid: "s", dest: "s") -> None:  # noqa: F821
        await self._core.ExportPackage(hid, dest)

    @method()
    async def HistoryAudioPath(self, hid: "s") -> "s":  # noqa: F821
        return await self._core.HistoryAudioPath(hid)

    # --- transcription captures (text only) -----------------------------

    @method()
    async def SaveCapture(self, text: "s") -> "s":  # noqa: F821
        return await self._core.SaveCapture(text)

    @method()
    async def ListCaptures(self) -> "s":  # noqa: F821
        return await self._core.ListCaptures()

    @method()
    async def UpdateCapture(self, capture_id: "s", text: "s") -> None:  # noqa: F821
        await self._core.UpdateCapture(capture_id, text)

    @method()
    async def DeleteCapture(self, capture_id: "s") -> None:  # noqa: F821
        await self._core.DeleteCapture(capture_id)

    # --- voice-changer source clips (named recordings/imports) ----------

    @method()
    async def SaveSourceClip(self, path: "s", name: "s", transcript: "s") -> "s":  # noqa: F821
        """Copy an audio file into the clip store; returns the new clip id
        ("" on failure). An empty name gets a time-based default; *transcript*
        is cached so re-arming the clip skips re-transcription."""
        return await self._core.SaveSourceClip(path, name, transcript)

    @method()
    async def SetSourceClipTranscript(self, clip_id: "s", transcript: "s") -> None:  # noqa: F821
        """Backfill a clip's transcript cache (saved before whisper finished)."""
        await self._core.SetSourceClipTranscript(clip_id, transcript)

    @method()
    async def ListSourceClips(self) -> "s":  # noqa: F821
        return await self._core.ListSourceClips()

    @method()
    async def DeleteSourceClip(self, clip_id: "s") -> None:  # noqa: F821
        await self._core.DeleteSourceClip(clip_id)

    @method()
    async def PlayFile(self, path: "s", title: "s") -> "u":  # noqa: F821
        """Audition any local audio file through the normal player (0 if
        unreadable). Used by the ⇄ tab to verify sources before converting."""
        return await self._core.PlayFile(path, title)

    @method()
    async def PlayFileAt(self, path: "s", title: "s", pct: "d") -> "u":  # noqa: F821
        """PlayFile from a fraction (0..1) of the way in — the trim modal's
        selection preview (the app cancels when the end handle is reached)."""
        return await self._core.PlayFileAt(path, title, pct)

    @method()
    async def FileEnvelope(self, path: "s") -> "s":  # noqa: F821
        """Waveform bars + duration of any local audio file, as JSON
        {"bars": [...], "duration": secs} — the trim modal's display."""
        return await self._core.FileEnvelope(path)

    @method()
    async def TrimAudio(self, path: "s", start_s: "d", end_s: "d") -> "s":  # noqa: F821
        """Cut a recording down to [start_s, end_s). WAVs are rewritten in
        place (PCM16 mono, rate kept); other formats get a sibling
        "<stem>-trimmed.wav". Returns the resulting path — "" on failure or
        a selection shorter than 0.1 s."""
        return await self._core.TrimAudio(path, start_s, end_s)

    @method()
    async def TrimHistoryClip(self, hid: "s", start_s: "d", end_s: "d") -> "b":  # noqa: F821
        """Cut a history clip to [start_s, end_s) in place — duration
        updates; text, tags and stars stay."""
        return await self._core.TrimHistoryClip(hid, start_s, end_s)

    # --- engine settings (the ⚙ tab's knobs) -----------------------------

    @method()
    async def GetSettings(self) -> "s":  # noqa: F821
        """Persisted engine settings plus the currently effective values."""
        return await self._core.GetSettings()

    @method()
    async def SetSetting(self, key: "s", value_json: "s") -> None:  # noqa: F821
        """Set one engine setting (JSON-encoded value; null clears it)."""
        await self._core.SetSetting(key, value_json)

    @method()
    def Cancel(self, gen_id: "u") -> None:  # noqa: F821
        self._core.Cancel(gen_id)

    # --- Signals (return annotation IS the D-Bus signature) -------------

    @signal()
    def GenerationProgress(self, gen_id, state, pct) -> "usd":  # noqa: F821
        return [gen_id, state, pct]

    @signal()
    def AudioLevel(self, gen_id, rms) -> "ud":  # noqa: F821
        return [gen_id, rms]

    @signal()
    def PlaybackInfo(self, gen_id, clip_id, title, duration, bars) -> "ussds":  # noqa: F821
        # clip_id, display title, seconds, and a JSON array of waveform bars (0..1)
        return [gen_id, clip_id, title, duration, bars]

    @signal()
    def PlaybackProgress(self, gen_id, pct) -> "ud":  # noqa: F821
        return [gen_id, pct]

    @signal()
    def LlmResult(self, req_id, text) -> "us":  # noqa: F821
        return [req_id, text]

    @signal()
    def TranscribeProgress(self, req_id, partial) -> "us":  # noqa: F821
        return [req_id, partial]

    @signal()
    def TranscribeResult(self, req_id, text) -> "us":  # noqa: F821
        return [req_id, text]

    @signal()
    def ModelProgress(self, model_id, pct, status) -> "sds":  # noqa: F821
        return [model_id, pct, status]

    @signal()
    def SpeakStarted(self, gen_id) -> "u":  # noqa: F821
        return gen_id

    @signal()
    def SpeakEnded(self, gen_id) -> "u":  # noqa: F821
        return gen_id

    # --- Properties (read-only) -----------------------------------------

    @dbus_property(access=PropertyAccess.READ)
    def ModelLoaded(self) -> "b":  # noqa: F821
        return self._core._model_loaded

    @dbus_property(access=PropertyAccess.READ)
    def Backend(self) -> "s":  # noqa: F821
        return self._core.backend_name

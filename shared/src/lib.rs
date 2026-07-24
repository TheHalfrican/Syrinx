//! Shared types and the D-Bus client proxy for the Syrinx engine.
//!
//! Both `syrinx-app` and `syrinx-dictate` talk to the Python engine through
//! this crate, so the interface lives in exactly one place.

// SetProfileAvatar and ConvertVoice take 8 args each; the arity is the engine's
// D-Bus signature, not ours to shrink, and the lint fires from inside the
// `zbus::proxy` expansion where an item-level allow can't reach it.
#![allow(clippy::too_many_arguments)]

use serde::{Deserialize, Serialize};

mod client;
mod error;
mod event;
mod rpc_client;
// D-Bus is Linux-only; macOS (a unix) rides the RPC transport, so gate on
// `target_os = "linux"`, not `unix` (MULTIPLATPLAN §1.2).
#[cfg(target_os = "linux")]
mod dbus_client;

pub use client::EngineClient;
pub use error::EngineError;
pub use event::EngineEvent;
/// The engine discovery-file path (RPC-PROTOCOL.md §2.2). Exposed so the
/// Win/mac supervisor can place the engine log alongside it and honor the same
/// `SYRINX_RPC_ENDPOINT` override.
pub use rpc_client::discovery_path;

/// A voice the engine can speak with (built-in or a cloned profile).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Voice {
    pub id: String,
    pub display_name: String,
}

/// Which compute backend the engine loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    Cuda,
    Rocm,
    Cpu,
}

/// Async proxy for `sh.syrinx.Engine1` on the session bus.
///
/// `zbus` generates the method/signal/property plumbing from this trait. Linux
/// only — the D-Bus transport is unix-native; `syrinx-dictate` uses this proxy
/// directly, and the `EngineClient` seam wraps it (see `dbus_client`).
#[cfg(target_os = "linux")]
#[zbus::proxy(
    interface = "sh.syrinx.Engine1",
    default_service = "sh.syrinx.Engine",
    default_path = "/sh/syrinx/Engine"
)]
pub trait Engine {
    /// Synthesize `text` in `voice_id`; returns a generation id.
    fn speak(&self, text: &str, voice_id: &str) -> zbus::Result<u32>;

    /// Transcribe an audio file (dictation). Returns recognized text.
    fn transcribe(&self, audio_path: &str) -> zbus::Result<String>;

    /// List available voices as (id, display_name) pairs.
    fn list_voices(&self) -> zbus::Result<Vec<(String, String)>>;

    /// Clone a voice from a sample file (+ its transcript); returns the new
    /// profile id. `ref_text` is required by the Qwen cloning backend.
    fn clone_voice(&self, name: &str, sample_path: &str, ref_text: &str) -> zbus::Result<String>;

    /// Create a voice profile from a JSON spec; returns the profile id.
    fn create_profile(&self, spec_json: &str) -> zbus::Result<String>;

    /// List voice profiles as a JSON array of summaries.
    fn list_profiles(&self) -> zbus::Result<String>;

    /// Full profile as JSON (empty string if not found).
    fn get_profile(&self, profile_id: &str) -> zbus::Result<String>;

    /// Apply a JSON patch of editable fields (name/description/language/personality/default_engine).
    fn update_profile(&self, profile_id: &str, patch_json: &str) -> zbus::Result<()>;

    /// Delete a profile and its samples.
    fn delete_profile(&self, profile_id: &str) -> zbus::Result<()>;

    /// Attach an avatar photo + crop rect (source px). mode: "circle" (square
    /// rect) or "panel" (tall rect, the card's right third). Empty src re-crops.
    fn set_profile_avatar(&self, profile_id: &str, src: &str, mode: &str, sx: i32, sy: i32, sw: i32, sh: i32) -> zbus::Result<()>;

    /// Export a profile as a portable .zip (profile.json + samples + avatar).
    fn export_profile(&self, profile_id: &str, dest: &str) -> zbus::Result<()>;

    /// Import a profile from an exported .zip; returns the new profile id.
    fn import_profile(&self, src: &str) -> zbus::Result<String>;

    /// Add a reference sample (an audio file path) to a cloned profile. An empty
    /// `reference_text` auto-transcribes via whisper. Returns JSON
    /// {sample_id, reference_text}.
    fn add_sample(&self, profile_id: &str, audio_path: &str, reference_text: &str) -> zbus::Result<String>;

    /// Compose an in-character utterance for a voice, guided by `prompt` (empty =
    /// unprompted). Returns a request id (0 if no personality); result via `LlmResult`.
    fn compose_profile(&self, voice_id: &str, prompt: &str) -> zbus::Result<u32>;

    /// Rewrite `text` in the voice's personality; returns a request id (0 if no
    /// personality). The result arrives via `LlmResult`.
    fn rewrite_profile(&self, voice_id: &str, text: &str) -> zbus::Result<u32>;

    /// Clean a dictation transcript (fillers out, punctuation in); returns a
    /// request id (0 if empty). The result arrives via `LlmResult`.
    fn refine_transcript(&self, text: &str) -> zbus::Result<u32>;

    /// Async transcription for long files (Transcribe blocks the D-Bus reply).
    /// Partials stream via `TranscribeProgress`; final text via `TranscribeResult`.
    fn transcribe_file(&self, audio_path: &str) -> zbus::Result<u32>;

    /// Style-preserved voice conversion: re-render the speech in `audio_path`
    /// with a cloned profile's voice, keeping the source delivery (only the
    /// timbre changes). `engine` "" = the default (chatterbox_vc; seed_vc in
    /// music mode). `mode` "music" = demucs vocal split → f0 conversion →
    /// remix over the instrumental. The history row stores `transcript` as
    /// its text and folds `label` into the display name. Returns a generation
    /// id; progress/errors arrive via `GenerationProgress`, and the result
    /// auto-plays and lands in history like Speak.
    fn convert_voice(&self, audio_path: &str, profile_id: &str, engine: &str, label: &str, transcript: &str, mode: &str, semitones: i32) -> zbus::Result<u32>;

    /// Auto-match for the speech pitch fine-tune: the median-f0 gap in
    /// semitones between `clip_path` and `profile_id`'s reference voice
    /// (positive = shift the clip up to reach the profile). Errors when the
    /// profile is preset/sampleless or either side has no voiced speech.
    fn suggest_pitch_shift(&self, clip_path: &str, profile_id: &str) -> zbus::Result<i32>;

    /// Copy an audio file into the voice-changer clip store; returns the new
    /// clip id ("" on failure). An empty name gets a time-based default;
    /// `transcript` is cached so re-arming the clip skips re-transcription.
    /// `kind` ("speech"|"music") is the vc-mode active at save time.
    fn save_source_clip(&self, path: &str, name: &str, transcript: &str, kind: &str) -> zbus::Result<String>;

    /// Backfill a saved clip's transcript cache (for clips saved before
    /// transcription finished).
    fn set_source_clip_transcript(&self, clip_id: &str, transcript: &str) -> zbus::Result<()>;

    /// Saved voice-changer source clips as a JSON array (newest first).
    fn list_source_clips(&self) -> zbus::Result<String>;

    /// Delete a saved source clip (row + audio file).
    fn delete_source_clip(&self, clip_id: &str) -> zbus::Result<()>;

    /// Audition any local audio file through the normal player; returns a
    /// generation id (0 if unreadable). "" title = the file stem.
    fn play_file(&self, path: &str, title: &str) -> zbus::Result<u32>;

    /// Delete a reference sample.
    fn delete_sample(&self, sample_id: &str) -> zbus::Result<()>;

    /// Correct a sample's reference transcript (clone prompts rebuild).
    fn update_sample_text(&self, profile_id: &str, sample_id: &str, text: &str) -> zbus::Result<()>;

    // --- generation history (persisted clips) ---------------------------

    /// List saved generations as a JSON array (newest first).
    fn list_history(&self) -> zbus::Result<String>;

    /// Replay a stored clip; returns a generation id (0 if not found).
    fn play_history(&self, hid: &str) -> zbus::Result<u32>;

    /// Replay a stored clip starting at `pct` (0..1); returns a generation id.
    fn play_history_at(&self, hid: &str, pct: f64) -> zbus::Result<u32>;

    /// Audition a profile reference sample; returns a generation id (0 if not found).
    fn play_sample(&self, sample_id: &str) -> zbus::Result<u32>;

    /// Pause the current playback.
    fn pause_playback(&self) -> zbus::Result<()>;

    /// Resume the current playback.
    fn resume_playback(&self) -> zbus::Result<()>;

    /// Seek the current playback to `pct` (0..1).
    fn seek_playback(&self, pct: f64) -> zbus::Result<()>;

    /// Playback volume 0..1, applied live to the current and later clips.
    fn set_volume(&self, volume: f64) -> zbus::Result<()>;

    // --- effects (pedalboard post-processing) ----------------------------

    /// JSON list of built-in effect presets: [{id, name, description}].
    fn list_effect_presets(&self) -> zbus::Result<String>;

    /// Set the preset applied to subsequent generations ("" = none).
    fn set_effect(&self, preset_id: &str) -> zbus::Result<()>;

    /// Delivery direction baked into generations ("" = neutral); free-text
    /// instruct honored by the qwen engines.
    fn set_style(&self, instruct: &str) -> zbus::Result<()>;

    /// Re-process a history clip through a preset; returns the new history id.
    fn apply_history_effects(&self, hid: &str, preset_id: &str) -> zbus::Result<String>;

    /// Effect definitions for the chain editor (JSON: label, params + ranges).
    fn list_effects(&self) -> zbus::Result<String>;

    /// Full preset incl. chain as JSON ("" if unknown).
    fn get_effect_preset(&self, preset_id: &str) -> zbus::Result<String>;

    /// Create a user preset; returns its id ("" on invalid chain / dup name).
    fn create_effect_preset(&self, name: &str, description: &str, chain_json: &str) -> zbus::Result<String>;

    /// Rewrite a user preset in place (builtins are immutable).
    fn update_effect_preset(&self, preset_id: &str, name: &str, description: &str, chain_json: &str) -> zbus::Result<bool>;

    /// Delete a user preset.
    fn delete_effect_preset(&self, preset_id: &str) -> zbus::Result<bool>;

    /// Play a history clip through an ad-hoc chain (nothing saved); gen id.
    fn preview_effects(&self, hid: &str, chain_json: &str) -> zbus::Result<u32>;

    /// Star/unstar a history entry.
    fn star_history(&self, hid: &str, starred: bool) -> zbus::Result<()>;

    /// Replace a history entry's tags (JSON array of strings) — Library labels.
    fn set_history_tags(&self, hid: &str, tags_json: &str) -> zbus::Result<()>;

    /// Delete a history entry (row + audio file).
    fn delete_history(&self, hid: &str) -> zbus::Result<()>;

    /// Re-synthesize a history entry's text/voice as a new clip; returns a gen id.
    fn regenerate_history(&self, hid: &str) -> zbus::Result<u32>;

    /// Write a `.zip` package (manifest.json + audio) for a history entry to `dest`.
    fn export_package(&self, hid: &str, dest: &str) -> zbus::Result<()>;

    /// Absolute WAV path of a history entry (for the app to copy on export-audio).
    fn history_audio_path(&self, hid: &str) -> zbus::Result<String>;

    // --- trim (✂ on recordings and history clips) ------------------------

    /// Waveform bars + duration of any local audio file as JSON
    /// (`{"bars": [...], "duration": secs}`) — the trim modal's display.
    fn file_envelope(&self, path: &str) -> zbus::Result<String>;

    /// Cut a recording to [start_s, end_s); returns the resulting path
    /// (in-place for WAVs, a sibling wav otherwise; "" on failure).
    fn trim_audio(&self, path: &str, start_s: f64, end_s: f64) -> zbus::Result<String>;

    /// Cut a history clip in place; its duration column follows.
    fn trim_history_clip(&self, hid: &str, start_s: f64, end_s: f64) -> zbus::Result<bool>;

    /// PlayFile starting at a fraction (0..1) — trim-preview playback.
    fn play_file_at(&self, path: &str, title: &str, pct: f64) -> zbus::Result<u32>;

    // --- transcription captures (text only, persisted) -------------------

    /// Save a transcript as a capture; returns the new capture id ("" if empty).
    fn save_capture(&self, text: &str) -> zbus::Result<String>;

    /// List captures as a JSON array (newest first).
    fn list_captures(&self) -> zbus::Result<String>;

    /// Replace a capture's text in place (same entry, no new row).
    fn update_capture(&self, capture_id: &str, text: &str) -> zbus::Result<()>;

    /// Delete a capture.
    fn delete_capture(&self, capture_id: &str) -> zbus::Result<()>;

    // --- model management ----------------------------------------------

    /// The model catalog as a JSON array (id, display, category, size, status…).
    fn list_models(&self) -> zbus::Result<String>;

    /// Detected hardware as JSON (cores, ram_gb, gpu, gpu_name).
    fn hardware(&self) -> zbus::Result<String>;

    /// Start downloading a model; progress arrives via `ModelProgress`.
    fn download_model(&self, model_id: &str) -> zbus::Result<bool>;

    /// Delete a downloaded model's files.
    fn delete_model(&self, model_id: &str) -> zbus::Result<()>;

    /// Make a model the active one for its category; returns the category.
    fn set_active_model(&self, model_id: &str) -> zbus::Result<String>;

    /// Persisted engine settings + effective values as JSON.
    fn get_settings(&self) -> zbus::Result<String>;

    /// Set one engine setting (JSON-encoded value; null clears it).
    fn set_setting(&self, key: &str, value_json: &str) -> zbus::Result<()>;

    /// Cancel an in-flight generation.
    fn cancel(&self, gen_id: u32) -> zbus::Result<()>;

    // --- recording (mic capture on Win/mac; RPC-PROTOCOL.md §14) ----------
    // Present on D-Bus too for drift parity; the Linux app keeps its native
    // parecord/pactl path and never calls these.

    /// Input capture devices as JSON `[{id,name,default}]` ("[]" on failure).
    fn list_recording_devices(&self) -> zbus::Result<String>;

    /// Start capturing mic input to a WAV; returns a recording id ("" on
    /// failure). "" device = system default input; a second call cancels the
    /// previous capture (latest-wins).
    fn start_recording(&self, device_id: &str) -> zbus::Result<String>;

    /// Stop + finalize; returns the WAV's absolute path ("" if
    /// unknown/already-stopped).
    fn stop_recording(&self, rec_id: &str) -> zbus::Result<String>;

    /// Stop and delete the WAV. Unknown id is a no-op.
    fn cancel_recording(&self, rec_id: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn model_loaded(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn backend(&self) -> zbus::Result<String>;

    #[zbus(signal)]
    fn generation_progress(&self, gen_id: u32, state: String, pct: f64) -> zbus::Result<()>;

    #[zbus(signal)]
    fn audio_level(&self, gen_id: u32, rms: f64) -> zbus::Result<()>;

    /// Emitted when playback of a clip starts: id, title, seconds, JSON waveform bars.
    #[zbus(signal)]
    fn playback_info(
        &self,
        gen_id: u32,
        clip_id: String,
        title: String,
        duration: f64,
        bars: String,
    ) -> zbus::Result<()>;

    /// Playback position (0..1), emitted per audio block.
    #[zbus(signal)]
    fn playback_progress(&self, gen_id: u32, pct: f64) -> zbus::Result<()>;

    /// Result of a Compose/Rewrite request (empty text = failed / no personality).
    #[zbus(signal)]
    fn llm_result(&self, req_id: u32, text: String) -> zbus::Result<()>;

    /// Live partial transcript while TranscribeFile decodes.
    #[zbus(signal)]
    fn transcribe_progress(&self, req_id: u32, partial: String) -> zbus::Result<()>;

    /// Final transcript for a TranscribeFile request. `error` = the stt stack
    /// raised (text=""), distinct from a legitimately-empty transcript.
    #[zbus(signal)]
    fn transcribe_result(&self, req_id: u32, text: String, error: bool) -> zbus::Result<()>;

    /// Model download progress: pct 0..1, status "downloading"|"done"|"error".
    #[zbus(signal)]
    fn model_progress(&self, model_id: String, pct: f64, status: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn speak_started(&self, gen_id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn speak_ended(&self, gen_id: u32) -> zbus::Result<()>;
}

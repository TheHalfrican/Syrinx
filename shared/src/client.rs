//! The `EngineClient` seam: one type the app drives, two transports underneath.
//!
//! On Linux it wraps the `#[zbus::proxy]` (byte-identical to before); on every
//! platform it can instead speak JSON-RPC over a localhost WebSocket. The
//! method surface mirrors the proxy in `lib.rs` one-to-one (same snake_case
//! names, same argument order) so the app's call sites are unchanged — only the
//! error type differs. The RPC method name is the D-Bus PascalCase name
//! (RPC-PROTOCOL.md §4); params go out as a positional JSON array (§3).
//!
//! Dispatch is a plain `enum` + `match` rather than a boxed trait object: no
//! `async_trait`, no dynamic dispatch, native `async fn`, and the app stores a
//! concrete `EngineClient` so `&proxy` still coerces at every call site.

use crate::{EngineError, EngineEvent};
use serde_json::json;
use tokio::sync::mpsc;

enum Transport {
    // D-Bus is Linux-only; macOS (a unix) uses RPC, so gate on the OS, not
    // `unix` (MULTIPLATPLAN §1.2).
    #[cfg(target_os = "linux")]
    Dbus(crate::dbus_client::DbusClient),
    Rpc(crate::rpc_client::RpcClient),
}

/// A connected engine client. Construct with [`EngineClient::connect_dbus`]
/// (Linux) or [`EngineClient::connect_rpc`] (Win/mac); consume events once via
/// [`EngineClient::events`].
pub struct EngineClient {
    transport: Transport,
    events: std::sync::Mutex<Option<mpsc::UnboundedReceiver<EngineEvent>>>,
}

impl EngineClient {
    /// Connect over the session bus and wrap the zbus proxy (Linux).
    #[cfg(target_os = "linux")]
    pub async fn connect_dbus() -> Result<Self, EngineError> {
        let (client, rx) = crate::dbus_client::DbusClient::connect().await?;
        Ok(Self {
            transport: Transport::Dbus(client),
            events: std::sync::Mutex::new(Some(rx)),
        })
    }

    /// Connect over JSON-RPC via the discovery file (all platforms). One
    /// attempt — the app retries on the splash while the engine comes up.
    pub async fn connect_rpc() -> Result<Self, EngineError> {
        let (client, rx) = crate::rpc_client::RpcClient::connect().await?;
        Ok(Self {
            transport: Transport::Rpc(client),
            events: std::sync::Mutex::new(Some(rx)),
        })
    }

    /// Take the event receiver. May be called exactly once; the app consumes it
    /// in its single `select!` loop.
    pub fn events(&self) -> mpsc::UnboundedReceiver<EngineEvent> {
        self.events
            .lock()
            .unwrap()
            .take()
            .expect("EngineClient::events() called more than once")
    }
}

/// Generate the 70 method + 2 property-getter dispatchers. Each expands to a
/// `match` that either delegates to the zbus proxy (same method name) or issues
/// an RPC `call` with the PascalCase method name and the args as positional
/// JSON. Keeping this a table makes the surface auditable against `lib.rs`.
macro_rules! engine_methods {
    ($( fn $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) -> $ret:ty => $rpc:literal ; )*) => {
        impl EngineClient {
            $(
                pub async fn $name(&self, $($arg: $ty),*) -> Result<$ret, EngineError> {
                    match &self.transport {
                        #[cfg(target_os = "linux")]
                        Transport::Dbus(c) => c.proxy.$name($($arg),*).await.map_err(EngineError::from),
                        Transport::Rpc(c) => c.call($rpc, vec![$(json!($arg)),*]).await,
                    }
                }
            )*
        }
    };
}

engine_methods! {
    // --- synthesis, transcription, voices --------------------------------
    fn speak(text: &str, voice_id: &str) -> u32 => "Speak";
    fn transcribe(audio_path: &str) -> String => "Transcribe";
    fn list_voices() -> Vec<(String, String)> => "ListVoices";
    fn clone_voice(name: &str, sample_path: &str, ref_text: &str) -> String => "CloneVoice";

    // --- voice profiles --------------------------------------------------
    fn create_profile(spec_json: &str) -> String => "CreateProfile";
    fn list_profiles() -> String => "ListProfiles";
    fn get_profile(profile_id: &str) -> String => "GetProfile";
    fn update_profile(profile_id: &str, patch_json: &str) -> () => "UpdateProfile";
    fn delete_profile(profile_id: &str) -> () => "DeleteProfile";
    fn set_profile_avatar(profile_id: &str, src: &str, mode: &str, sx: i32, sy: i32, sw: i32, sh: i32) -> () => "SetProfileAvatar";
    fn export_profile(profile_id: &str, dest: &str) -> () => "ExportProfile";
    fn import_profile(src: &str) -> String => "ImportProfile";
    fn add_sample(profile_id: &str, audio_path: &str, reference_text: &str) -> String => "AddSample";
    fn delete_sample(sample_id: &str) -> () => "DeleteSample";
    fn update_sample_text(profile_id: &str, sample_id: &str, text: &str) -> () => "UpdateSampleText";

    // --- personality LLM (async → LlmResult) -----------------------------
    fn compose_profile(voice_id: &str, prompt: &str) -> u32 => "ComposeProfile";
    fn rewrite_profile(voice_id: &str, text: &str) -> u32 => "RewriteProfile";
    fn refine_transcript(text: &str) -> u32 => "RefineTranscript";

    // --- async transcription & voice conversion --------------------------
    fn transcribe_file(audio_path: &str) -> u32 => "TranscribeFile";
    fn convert_voice(audio_path: &str, profile_id: &str, engine: &str, label: &str, transcript: &str, mode: &str, semitones: i32) -> u32 => "ConvertVoice";
    fn suggest_pitch_shift(clip_path: &str, profile_id: &str) -> i32 => "SuggestPitchShift";

    // --- voice-changer source clips --------------------------------------
    fn save_source_clip(path: &str, name: &str, transcript: &str, kind: &str) -> String => "SaveSourceClip";
    fn set_source_clip_transcript(clip_id: &str, transcript: &str) -> () => "SetSourceClipTranscript";
    fn list_source_clips() -> String => "ListSourceClips";
    fn delete_source_clip(clip_id: &str) -> () => "DeleteSourceClip";
    fn play_file(path: &str, title: &str) -> u32 => "PlayFile";

    // --- generation history ----------------------------------------------
    fn list_history() -> String => "ListHistory";
    fn play_history(hid: &str) -> u32 => "PlayHistory";
    fn play_history_at(hid: &str, pct: f64) -> u32 => "PlayHistoryAt";
    fn play_sample(sample_id: &str) -> u32 => "PlaySample";
    fn pause_playback() -> () => "PausePlayback";
    fn resume_playback() -> () => "ResumePlayback";
    fn seek_playback(pct: f64) -> () => "SeekPlayback";
    fn set_volume(volume: f64) -> () => "SetVolume";

    // --- effects (presets & chain editor) --------------------------------
    fn list_effect_presets() -> String => "ListEffectPresets";
    fn set_effect(preset_id: &str) -> () => "SetEffect";
    fn set_style(instruct: &str) -> () => "SetStyle";
    fn apply_history_effects(hid: &str, preset_id: &str) -> String => "ApplyHistoryEffects";
    fn list_effects() -> String => "ListEffects";
    fn get_effect_preset(preset_id: &str) -> String => "GetEffectPreset";
    fn create_effect_preset(name: &str, description: &str, chain_json: &str) -> String => "CreateEffectPreset";
    fn update_effect_preset(preset_id: &str, name: &str, description: &str, chain_json: &str) -> bool => "UpdateEffectPreset";
    fn delete_effect_preset(preset_id: &str) -> bool => "DeleteEffectPreset";
    fn preview_effects(hid: &str, chain_json: &str) -> u32 => "PreviewEffects";
    fn star_history(hid: &str, starred: bool) -> () => "StarHistory";
    fn set_history_tags(hid: &str, tags_json: &str) -> () => "SetHistoryTags";
    fn delete_history(hid: &str) -> () => "DeleteHistory";
    fn regenerate_history(hid: &str) -> u32 => "RegenerateHistory";
    fn export_package(hid: &str, dest: &str) -> () => "ExportPackage";
    fn history_audio_path(hid: &str) -> String => "HistoryAudioPath";

    // --- trim ------------------------------------------------------------
    fn file_envelope(path: &str) -> String => "FileEnvelope";
    fn trim_audio(path: &str, start_s: f64, end_s: f64) -> String => "TrimAudio";
    fn trim_history_clip(hid: &str, start_s: f64, end_s: f64) -> bool => "TrimHistoryClip";
    fn play_file_at(path: &str, title: &str, pct: f64) -> u32 => "PlayFileAt";

    // --- transcription captures ------------------------------------------
    fn save_capture(text: &str) -> String => "SaveCapture";
    fn list_captures() -> String => "ListCaptures";
    fn update_capture(capture_id: &str, text: &str) -> () => "UpdateCapture";
    fn delete_capture(capture_id: &str) -> () => "DeleteCapture";

    // --- model management & settings -------------------------------------
    fn list_models() -> String => "ListModels";
    fn hardware() -> String => "Hardware";
    fn download_model(model_id: &str) -> bool => "DownloadModel";
    fn delete_model(model_id: &str) -> () => "DeleteModel";
    fn set_active_model(model_id: &str) -> String => "SetActiveModel";
    fn get_settings() -> String => "GetSettings";
    fn set_setting(key: &str, value_json: &str) -> () => "SetSetting";
    fn cancel(gen_id: u32) -> () => "Cancel";

    // --- recording (mic capture on Win/mac; §14) -------------------------
    fn list_recording_devices() -> String => "ListRecordingDevices";
    fn start_recording(device_id: &str) -> String => "StartRecording";
    fn stop_recording(rec_id: &str) -> String => "StopRecording";
    fn cancel_recording(rec_id: &str) -> () => "CancelRecording";

    // --- read-only properties → explicit getters (spec §5) ---------------
    fn backend() -> String => "GetBackend";
    fn model_loaded() -> bool => "GetModelLoaded";
}

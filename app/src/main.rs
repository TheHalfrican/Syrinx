//! Syrinx main window — themed shell + TTS workspace wired to the engine.
//!
//! The Slint UI runs on the main thread; a tokio worker owns the D-Bus
//! connection to `sh.syrinx.Engine1`. Theme switching and tab nav are pure UI
//! (Slint globals); voices, generate, level, and history cross the bridge.

slint::include_modules!();

// Win/mac own the engine as a supervised child process (RPC-PROTOCOL.md §13);
// on Linux the engine belongs to systemd + D-Bus activation, so this never
// compiles there.
#[cfg(not(target_os = "linux"))]
mod engine_proc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::collections::HashMap;
use std::process::Stdio;
use std::rc::Rc;
use syrinx_shared::{EngineClient, EngineError, EngineEvent};
use tokio::sync::mpsc;

enum Cmd {
    Generate { text: String, voice: String },
    Cancel { gen_id: u32 },
    Play { id: String },
    Star { id: String, on: bool },
    Delete { id: String },
    Regenerate { id: String, is_vc: bool, is_music: bool },
    Pause,
    Resume,
    Seek { id: String, pct: f64 },
    ExportAudio { id: String },
    ExportPackage { id: String },
    CvStartRecord { system: bool },
    CvStopRecord,
    CvPickFile,
    CvTranscribe,
    CvCreate { name: String, desc: String, personality: String, language: String, transcript: String, model_index: usize },
    CvCancel,
    Compose { voice_id: String, prompt: String },
    Rewrite { voice_id: String, text: String },
    ModelsLoad,
    DownloadModel { id: String },
    DeleteModel { id: String },
    ActivateModel { id: String },
    // Voicebox-style composer / cards / player
    GenerateInCharacter { text: String, voice: String },
    SelectVoice { id: String },
    PickLanguage { voice: String, index: usize },
    PickEngine { voice: String, index: usize },
    ToggleLoop { on: bool },
    SetVol { v: f64 },
    PickEffect { index: usize },
    PickStyle { index: usize },
    ApplyFx { hid: String, index: usize },
    ExportVoice { id: String, name: String },
    EditVoice { id: String },
    DeleteVoice { id: String },
    ImportVoice,
    CvPickAvatar,
    CvStageAvatar { path: String, mode: String, sx: i32, sy: i32, sw: i32, sh: i32 },
    TrToggleRecord { system: bool },
    TrPickFile,
    TrRefine { text: String },
    TrSaveCapture { id: String, text: String },
    TrDeleteCapture { id: String },
    // trim modal (✂ on recordings and history clips)
    TrimShow { ctx: String },
    TrimShowHist { hid: String },
    TrimPreview { start: f64, end: f64 },
    TrimPreviewStop,
    TrimApply { start: f64, end: f64 },
    // voice changer (⇄ tab)
    VcLoad,
    VcToggleRecord { system: bool },
    VcPickFile,
    VcConvert { index: usize, engine_index: usize, label: String, transcript: String, mode: String, semitones: i32 },
    VcSaveClip { name: String, transcript: String },
    VcDeleteClip { id: String },
    VcArmClip { id: String },
    VcAudition { id: String },
    // settings (⚙ tab)
    SettingsLoad,
    SaveTheme { theme: String },
    StPickMic { index: usize },
    StPickMonitor { index: usize },
    StToggleRefine,
    StToggleStopEngine,
    StPickExportDir,
    StPickCap { index: usize },
    StPickSteps { index: usize },
    // library (▤ tab)
    LibLoad,
    LibRefilter { q: String, type_idx: i32, voice_idx: i32, starred: bool, model_idx: i32 },
    LibSaveTags { id: String, csv: String },
    // voices tab (profile table + inspector)
    VoicesLoad,
    VoicesSearch { q: String },
    VoicesInspect { id: String },
    PlaySample { id: String },
    // effects chain editor
    FxeShow,
    FxeLoad { index: usize },
    FxeNew,
    FxeAdd { index: usize },
    FxeRemove { index: usize },
    FxeToggle { index: usize },
    FxeMove { index: usize, dir: i32 },
    FxeExpand { index: usize },
    FxeParam { index: usize, norm: f32 },
    FxeSave { name: String, desc: String },
    FxeDelete,
    FxePreview { hid: String },
}

/// serde needs a fn for a non-false bool default; `impl Default` below keeps
/// the no-file path in step with the missing-field one.
fn default_true() -> bool {
    true
}

/// App-side settings (~/.config/syrinx/settings.json) — written by the ⚙
/// tab, read here at startup and by syrinx-dictate (refine toggle).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct AppConfig {
    theme: String,
    mic_device: String,     // "" = system default source
    monitor_device: String, // "" = default sink's monitor
    refine_dictation: bool,
    export_dir: String,
    /// opt-out, not opt-in: an idle engine sits on ~12 GB of VRAM, and D-Bus
    /// activation brings it straight back on the next launch.
    #[serde(default = "default_true")]
    stop_engine_on_quit: bool,
}

// hand-rolled (not derived) so `stop_engine_on_quit` is true with no file at
// all, exactly as `default_true` makes it true for a file that predates it.
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: String::new(),
            mic_device: String::new(),
            monitor_device: String::new(),
            refine_dictation: false,
            export_dir: String::new(),
            stop_engine_on_quit: true,
        }
    }
}

fn config_path() -> std::path::PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        })
        .join("syrinx")
        .join("settings.json")
}

fn load_config() -> AppConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &AppConfig) {
    let p = config_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        if let Err(e) = std::fs::write(&p, text) {
            tracing::error!("save settings.json failed: {e}");
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let ui = AppWindow::new()?;

    // wayland app_id — must match the desktop file's basename (syrinx.desktop)
    // so launchers/taskbars associate the window. Must come AFTER
    // AppWindow::new(): set_xdg_app_id needs the platform initialised, and
    // winit only reads it later, on first show.
    slint::set_xdg_app_id("syrinx")?;
    let (tx, rx) = mpsc::unbounded_channel::<Cmd>();

    // restore the persisted theme before first paint
    {
        let cfg = load_config();
        if !cfg.theme.is_empty() {
            ui.global::<Theme>().set_name(cfg.theme.into());
        }
        // the ⚙ toggle reflects config from the start, not just after the tab
        // is first opened (SettingsLoad)
        ui.set_st_stop_engine(cfg.stop_engine_on_quit);
        // The ENGINE / stop-on-quit card is systemd-specific; hide it on Win/mac
        // where a spawned engine always dies with the app (RPC-PROTOCOL.md §13).
        ui.set_is_linux(cfg!(target_os = "linux"));
    }

    let history = Rc::new(VecModel::<HistItem>::default());
    ui.set_history(ModelRc::from(history.clone()));

    // tiled-half-screen watcher: `narrow` must be SET, not bound to
    // root.width — a width binding puts the label texts inside the window's
    // own layout-info graph (a binding loop slint deprecates). Polling a
    // bool 4x/s is imperceptible; the timer must outlive ui.run().
    let narrow_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        narrow_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(250),
            move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let w = ui.window().size().width as f32 / ui.window().scale_factor();
                    let narrow = w < 1250.0;
                    if ui.get_narrow() != narrow {
                        ui.set_narrow(narrow);
                    }
                }
            },
        );
    }

    // Generate pressed.
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        let history = history.clone();
        ui.on_generate(move || {
            let ui = ui_weak.unwrap();
            let text: String = ui.get_text().to_string();
            let voice: String = ui.get_selected_voice().to_string();
            if text.trim().is_empty() || voice.is_empty() {
                return;
            }
            ui.set_generating(true);
            ui.set_synthesizing(true);
            ui.set_gen_error("".into());
            history.insert(
                0,
                HistItem {
                    id: "".into(),
                    voice: voice_name(&ui, &voice).into(),
                    meta: "generating…".into(),
                    text: text.clone().into(),
                    starred: false,
                },
            );
            // "Speak in character" toggle: rewrite via the personality LLM first,
            // then synthesize the rewritten line (Voicebox's persona flow).
            if ui.get_persona_on() && ui.get_selected_has_personality() {
                ui.set_llm_busy(true);
                let _ = tx.send(Cmd::GenerateInCharacter { text, voice });
            } else {
                let _ = tx.send(Cmd::Generate { text, voice });
            }
        });
    }
    // Stop pressed.
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_stop(move || {
            ui_weak.unwrap().set_generating(false);
            let _ = tx.send(Cmd::Cancel { gen_id: 0 });
        });
    }
    // Track whether the selected voice has a personality (gates Compose/persona)
    // and feed the composer (placeholder name + per-engine language list).
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_select_voice(move |id| {
            let ui = ui_weak.unwrap();
            let voices = ui.get_voices();
            let mut hp = false;
            for i in 0..voices.row_count() {
                if let Some(v) = voices.row_data(i) {
                    if v.id == id {
                        hp = v.has_personality;
                        break;
                    }
                }
            }
            ui.set_selected_has_personality(hp);
            ui.set_selected_voice_name(voice_name(&ui, &id).into());
            let _ = tx.send(Cmd::SelectVoice { id: id.to_string() });
        });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_compose(move || {
            let ui = ui_weak.unwrap();
            ui.set_llm_busy(true);
            let _ = tx.send(Cmd::Compose {
                voice_id: ui.get_selected_voice().to_string(),
                prompt: ui.get_text().to_string(),
            });
        });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_rewrite(move || {
            let ui = ui_weak.unwrap();
            let text = ui.get_text().to_string();
            if text.trim().is_empty() {
                return;
            }
            ui.set_llm_busy(true);
            let _ = tx.send(Cmd::Rewrite {
                voice_id: ui.get_selected_voice().to_string(),
                text,
            });
        });
    }

    // History actions.
    {
        let tx = tx.clone();
        ui.on_play_hist(move |id| {
            let _ = tx.send(Cmd::Play { id: id.to_string() });
        });
    }
    {
        let tx = tx.clone();
        let history = history.clone();
        ui.on_star_hist(move |id, on| {
            // optimistic UI toggle; the engine persists it
            for i in 0..history.row_count() {
                if let Some(mut it) = history.row_data(i) {
                    if it.id == id {
                        it.starred = on;
                        history.set_row_data(i, it);
                        break;
                    }
                }
            }
            let _ = tx.send(Cmd::Star { id: id.to_string(), on });
        });
    }
    {
        let tx = tx.clone();
        ui.on_delete_hist(move |id| {
            let _ = tx.send(Cmd::Delete { id: id.to_string() });
        });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_regen_hist(move |id| {
            let ui = ui_weak.unwrap();
            // conversion rows re-CONVERT (⇄ status), not re-speak (composer
            // spinner) — look the row up to route the busy state correctly
            let (mut is_vc, mut is_music) = (false, false);
            let hist = ui.get_history();
            for i in 0..hist.row_count() {
                if let Some(h) = hist.row_data(i) {
                    if h.id == id {
                        is_vc = h.meta.starts_with("⇄ VC");
                        is_music = h
                            .voice
                            .split(" · ")
                            .next()
                            .map(|s| s.trim_end().ends_with('♫'))
                            .unwrap_or(false);
                        break;
                    }
                }
            }
            if is_vc {
                ui.set_vc_busy(true);
                ui.set_vc_error("".into());
                ui.set_vc_status("starting…".into());
            } else {
                ui.set_generating(true);
                ui.set_synthesizing(true);
            }
            let _ = tx.send(Cmd::Regenerate { id: id.to_string(), is_vc, is_music });
        });
    }
    {
        let tx = tx.clone();
        ui.on_download_model(move |id| { let _ = tx.send(Cmd::DownloadModel { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_delete_model(move |id| { let _ = tx.send(Cmd::DeleteModel { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_activate_model(move |id| { let _ = tx.send(Cmd::ActivateModel { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_pause(move || { let _ = tx.send(Cmd::Pause); });
    }
    {
        let tx = tx.clone();
        ui.on_resume(move || { let _ = tx.send(Cmd::Resume); });
    }
    {
        let tx = tx.clone();
        ui.on_seek(move |id, pct| {
            let _ = tx.send(Cmd::Seek { id: id.to_string(), pct: pct as f64 });
        });
    }
    {
        let tx = tx.clone();
        ui.on_export_audio(move |id| {
            let _ = tx.send(Cmd::ExportAudio { id: id.to_string() });
        });
    }
    {
        let tx = tx.clone();
        ui.on_export_package(move |id| {
            let _ = tx.send(Cmd::ExportPackage { id: id.to_string() });
        });
    }
    // --- create-voice modal ---
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_cv_start_record(move || {
            let system = ui_weak.unwrap().get_cv_mode() == "system";
            let _ = tx.send(Cmd::CvStartRecord { system });
        });
    }
    {
        let tx = tx.clone();
        ui.on_cv_stop_record(move || { let _ = tx.send(Cmd::CvStopRecord); });
    }
    {
        let tx = tx.clone();
        ui.on_cv_pick_file(move || { let _ = tx.send(Cmd::CvPickFile); });
    }
    {
        let tx = tx.clone();
        ui.on_cv_transcribe(move || { let _ = tx.send(Cmd::CvTranscribe); });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_cv_create(move || {
            let ui = ui_weak.unwrap();
            let _ = tx.send(Cmd::CvCreate {
                name: ui.get_cv_name().to_string(),
                desc: ui.get_cv_desc().to_string(),
                personality: ui.get_cv_personality().to_string(),
                language: ui.get_cv_language().to_string(),
                transcript: ui.get_cv_transcript().to_string(),
                model_index: ui.get_cv_model_index() as usize,
            });
        });
    }
    {
        let tx = tx.clone();
        ui.on_cv_cancel(move || { let _ = tx.send(Cmd::CvCancel); });
    }
    // Settings view.
    {
        let tx = tx.clone();
        ui.on_settings_open(move || { let _ = tx.send(Cmd::SettingsLoad); });
    }
    {
        let tx = tx.clone();
        ui.on_theme_changed(move |t| { let _ = tx.send(Cmd::SaveTheme { theme: t.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_st_pick_mic(move |i| { let _ = tx.send(Cmd::StPickMic { index: i.max(0) as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_st_pick_monitor(move |i| { let _ = tx.send(Cmd::StPickMonitor { index: i.max(0) as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_st_toggle_refine(move || { let _ = tx.send(Cmd::StToggleRefine); });
    }
    {
        let tx = tx.clone();
        ui.on_st_toggle_stop_engine(move || { let _ = tx.send(Cmd::StToggleStopEngine); });
    }
    {
        let tx = tx.clone();
        ui.on_st_pick_export_dir(move || { let _ = tx.send(Cmd::StPickExportDir); });
    }
    {
        let tx = tx.clone();
        ui.on_st_pick_cap(move |i| { let _ = tx.send(Cmd::StPickCap { index: i.max(0) as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_st_pick_steps(move |i| { let _ = tx.send(Cmd::StPickSteps { index: i.max(0) as usize }); });
    }
    // Library view.
    {
        let tx = tx.clone();
        ui.on_lib_open(move || { let _ = tx.send(Cmd::LibLoad); });
    }
    // Models view — workers pull weights lazily at first conversion, so the
    // cached catalog can be stale; re-inspect on every visit.
    {
        let tx = tx.clone();
        ui.on_models_open(move || { let _ = tx.send(Cmd::ModelsLoad); });
    }
    // Trim modal (✂ on recordings and history clips).
    {
        let tx = tx.clone();
        ui.on_trim_show(move |ctx| { let _ = tx.send(Cmd::TrimShow { ctx: ctx.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_trim_show_hist(move |hid| { let _ = tx.send(Cmd::TrimShowHist { hid: hid.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_trim_preview(move |s, e| { let _ = tx.send(Cmd::TrimPreview { start: s as f64, end: e as f64 }); });
    }
    {
        let tx = tx.clone();
        ui.on_trim_preview_stop(move || { let _ = tx.send(Cmd::TrimPreviewStop); });
    }
    {
        let tx = tx.clone();
        ui.on_trim_apply(move |s, e| { let _ = tx.send(Cmd::TrimApply { start: s as f64, end: e as f64 }); });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_lib_refilter(move || {
            let ui = ui_weak.unwrap();
            let _ = tx.send(Cmd::LibRefilter {
                q: ui.get_lib_search().to_string(),
                type_idx: ui.get_lib_type_index(),
                voice_idx: ui.get_lib_voice_index(),
                starred: ui.get_lib_starred_only(),
                model_idx: ui.get_lib_model_index(),
            });
        });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_lib_save_tags(move || {
            let ui = ui_weak.unwrap();
            let _ = tx.send(Cmd::LibSaveTags {
                id: ui.get_lib_tag_id().to_string(),
                csv: ui.get_lib_tag_value().to_string(),
            });
        });
    }
    {
        let tx = tx.clone();
        ui.on_voices_open(move || { let _ = tx.send(Cmd::VoicesLoad); });
    }
    {
        let tx = tx.clone();
        ui.on_voices_search(move |q| { let _ = tx.send(Cmd::VoicesSearch { q: q.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_vp_select(move |id| { let _ = tx.send(Cmd::VoicesInspect { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_vs_play(move |id| { let _ = tx.send(Cmd::PlaySample { id: id.to_string() }); });
    }

    // Composer dropdowns, card actions, player loop/volume (Voicebox parity).
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_pick_language(move |i| {
            let ui = ui_weak.unwrap();
            let _ = tx.send(Cmd::PickLanguage {
                voice: ui.get_selected_voice().to_string(),
                index: i as usize,
            });
        });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_pick_engine(move |i| {
            let ui = ui_weak.unwrap();
            let _ = tx.send(Cmd::PickEngine {
                voice: ui.get_selected_voice().to_string(),
                index: i as usize,
            });
        });
    }
    {
        let tx = tx.clone();
        ui.on_toggle_loop(move |on| { let _ = tx.send(Cmd::ToggleLoop { on }); });
    }
    {
        let tx = tx.clone();
        ui.on_pick_effect(move |i| { let _ = tx.send(Cmd::PickEffect { index: i as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_pick_style(move |i| { let _ = tx.send(Cmd::PickStyle { index: i as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_apply_fx(move |hid, i| {
            let _ = tx.send(Cmd::ApplyFx { hid: hid.to_string(), index: i as usize });
        });
    }
    {
        let tx = tx.clone();
        ui.on_set_volume(move |v| { let _ = tx.send(Cmd::SetVol { v: v as f64 }); });
    }
    {
        let tx = tx.clone();
        ui.on_export_voice(move |id, name| {
            let _ = tx.send(Cmd::ExportVoice { id: id.to_string(), name: name.to_string() });
        });
    }
    {
        let tx = tx.clone();
        ui.on_edit_voice(move |id| { let _ = tx.send(Cmd::EditVoice { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_delete_voice(move |id| { let _ = tx.send(Cmd::DeleteVoice { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_cv_pick_avatar(move || { let _ = tx.send(Cmd::CvPickAvatar); });
    }
    // Transcription view.
    {
        let tx = tx.clone();
        ui.on_tr_toggle_record(move |mode| {
            let _ = tx.send(Cmd::TrToggleRecord { system: mode.as_str() == "system" });
        });
    }
    {
        let tx = tx.clone();
        ui.on_tr_pick_file(move || { let _ = tx.send(Cmd::TrPickFile); });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_tr_refine(move || {
            let ui = ui_weak.unwrap();
            let text = ui.get_tr_text().to_string();
            if !text.trim().is_empty() {
                ui.set_tr_busy(true);
                ui.set_tr_status("refining…".into());
                let _ = tx.send(Cmd::TrRefine { text });
            }
        });
    }
    // Voice changer (⇄ tab).
    {
        let tx = tx.clone();
        ui.on_vc_open(move || { let _ = tx.send(Cmd::VcLoad); });
    }
    {
        let tx = tx.clone();
        ui.on_vc_toggle_record(move |mode| {
            let _ = tx.send(Cmd::VcToggleRecord { system: mode.as_str() == "system" });
        });
    }
    {
        let tx = tx.clone();
        ui.on_vc_pick_file(move || { let _ = tx.send(Cmd::VcPickFile); });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_vc_convert(move |i| {
            let ui = ui_weak.unwrap();
            let label = ui.get_vc_result_name().to_string();
            let transcript = ui.get_vc_transcript().to_string();
            let mode = ui.get_vc_mode().to_string();
            let engine_index = if mode == "music" {
                ui.get_vc_music_engine_index().max(0) as usize
            } else {
                ui.get_vc_engine_index().max(0) as usize
            };
            // ♫ octave dropdown: index 0..4 → −2..+2 octaves, in semitones
            let semitones = if mode == "music" {
                (ui.get_vc_octave_index().clamp(0, 4) - 2) * 12
            } else {
                0
            };
            let _ = tx.send(Cmd::VcConvert {
                index: i.max(0) as usize, engine_index, label, transcript, mode, semitones,
            });
        });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_vc_save_clip(move || {
            let ui = ui_weak.unwrap();
            let name = ui.get_vc_clip_name().to_string();
            let transcript = ui.get_vc_transcript().to_string();
            let _ = tx.send(Cmd::VcSaveClip { name, transcript });
        });
    }
    {
        let tx = tx.clone();
        ui.on_vc_delete_clip(move |id| { let _ = tx.send(Cmd::VcDeleteClip { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_vc_arm_clip(move |id| { let _ = tx.send(Cmd::VcArmClip { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        ui.on_vc_audition(move |id| { let _ = tx.send(Cmd::VcAudition { id: id.to_string() }); });
    }
    // Captures (persisted transcripts): save-or-update decided by tr-capture-id.
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_tr_save_capture(move || {
            let ui = ui_weak.unwrap();
            let text = ui.get_tr_text().to_string();
            if !text.trim().is_empty() {
                let id = ui.get_tr_capture_id().to_string();
                let _ = tx.send(Cmd::TrSaveCapture { id, text });
            }
        });
    }
    {
        let tx = tx.clone();
        ui.on_tr_delete_capture(move |id| {
            let _ = tx.send(Cmd::TrDeleteCapture { id: id.to_string() });
        });
    }
    // Effects chain editor.
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        let history = history.clone();
        ui.on_fxe_show(move || {
            let ui = ui_weak.unwrap();
            let can = !ui.get_player_id().is_empty()
                || history.iter().any(|h| !h.id.is_empty());
            ui.set_fxe_can_preview(can);
            let _ = tx.send(Cmd::FxeShow);
        });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_load(move |i| { let _ = tx.send(Cmd::FxeLoad { index: i as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_new(move || { let _ = tx.send(Cmd::FxeNew); });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_add_effect(move |i| { let _ = tx.send(Cmd::FxeAdd { index: i as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_remove(move |i| { let _ = tx.send(Cmd::FxeRemove { index: i as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_toggle(move |i| { let _ = tx.send(Cmd::FxeToggle { index: i as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_move(move |i, d| { let _ = tx.send(Cmd::FxeMove { index: i as usize, dir: d }); });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_expand(move |i| { let _ = tx.send(Cmd::FxeExpand { index: i as usize }); });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_param(move |i, v| { let _ = tx.send(Cmd::FxeParam { index: i as usize, norm: v }); });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_fxe_save(move || {
            let ui = ui_weak.unwrap();
            let _ = tx.send(Cmd::FxeSave {
                name: ui.get_fxe_name().to_string(),
                desc: ui.get_fxe_desc().to_string(),
            });
        });
    }
    {
        let tx = tx.clone();
        ui.on_fxe_delete(move || { let _ = tx.send(Cmd::FxeDelete); });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        let history = history.clone();
        ui.on_fxe_preview(move || {
            let ui = ui_weak.unwrap();
            // prefer the clip in the player; fall back to the newest history row
            let hid = if !ui.get_player_id().is_empty() {
                ui.get_player_id().to_string()
            } else {
                history.iter().find(|h| !h.id.is_empty()).map(|h| h.id.to_string()).unwrap_or_default()
            };
            if !hid.is_empty() {
                let _ = tx.send(Cmd::FxePreview { hid });
            }
        });
    }
    // Crop accepted: turn the dialog's zoom/pan into a square source-pixel rect.
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_crop_done(move |accepted| {
            if !accepted {
                return;
            }
            let ui = ui_weak.unwrap();
            let path = ui.get_crop_path().to_string();
            let sz = ui.get_crop_src().size();
            let (w, h) = (sz.width as f32, sz.height as f32);
            if path.is_empty() || w < 1.0 || h < 1.0 {
                return;
            }
            // mirror the dialog viewport's aspect: circle 220x220, panel 132x220.
            // The math runs in preview pixels (crop-src is downscaled), then the
            // rect is scaled back to ORIGINAL photo pixels for storage.
            let mode = ui.get_crop_mode().to_string();
            let (vw, vh): (f32, f32) = if mode == "panel" { (132.0, 220.0) } else { (220.0, 220.0) };
            let cw = (w.min(h * vw / vh) / ui.get_crop_zoom().max(1.0)).round();
            let ch = (cw * vh / vw).round();
            let sx = (ui.get_crop_cx() * w - cw / 2.0).clamp(0.0, (w - cw).max(0.0));
            let sy = (ui.get_crop_cy() * h - ch / 2.0).clamp(0.0, (h - ch).max(0.0));
            let (fw, fh) = (ui.get_crop_full_w() as f32, ui.get_crop_full_h() as f32);
            let scale = if fw > 0.0 && w > 0.0 { fw / w } else { 1.0 };
            let fsw = (cw * scale).round().min(fw).max(1.0);
            let fsh = (ch * scale).round().min(fh.max(1.0)).max(1.0);
            let fsx = (sx * scale).round().clamp(0.0, (fw - fsw).max(0.0));
            let fsy = (sy * scale).round().clamp(0.0, (fh - fsh).max(0.0));
            let _ = tx.send(Cmd::CvStageAvatar {
                path,
                mode,
                sx: fsx as i32,
                sy: fsy as i32,
                sw: fsw as i32,
                sh: fsh as i32,
            });
        });
    }
    {
        let tx = tx.clone();
        ui.on_import_voice(move || { let _ = tx.send(Cmd::ImportVoice); });
    }

    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        if let Err(e) = rt.block_on(worker(ui_weak.clone(), rx)) {
            tracing::error!("engine worker exited: {e:#}");
            // a bus that never came up would otherwise strand the splash
            ui_weak.upgrade_in_event_loop(|ui| ui.set_booting(false)).ok();
        }
    });

    ui.run()?;

    // Window closed gracefully: hand the GPU back. Re-read config rather than
    // trusting a startup copy — the toggle may have been flipped this session.
    // systemctl, deliberately, and NOT a D-Bus quit(): only the systemd-managed
    // engine should die here. A dev engine started by hand (`python -m
    // syrinx_engine`) owns the same bus name but no unit, so `stop` is a
    // harmless no-op failure there and the dev process survives app restarts.
    // Only the graceful path runs this — SIGTERM/pkill skip it by design.
    //
    // Linux only: on Win/mac there is no systemd unit and the ⚙ toggle does not
    // apply. A spawned engine dies with the app because the OS closes the
    // child's held stdin pipe on our exit, which the SYRINX_SUPERVISED watchdog
    // (RPC-PROTOCOL.md §13.1) turns into an immediate engine exit; an adopted
    // engine's stdin was never ours, so it keeps running.
    #[cfg(target_os = "linux")]
    if load_config().stop_engine_on_quit {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", "syrinx-engine.service"])
            .status();
    }
    Ok(())
}

fn voice_name(ui: &AppWindow, id: &str) -> String {
    let voices = ui.get_voices();
    for i in 0..voices.row_count() {
        if let Some(v) = voices.row_data(i) {
            if v.id == id {
                return v.name.to_string();
            }
        }
    }
    // Kokoro presets live in a separate model (the dropdown), not the card grid.
    let ids = ui.get_kokoro_ids();
    let names = ui.get_kokoro_names();
    for i in 0..ids.row_count() {
        if ids.row_data(i).map(|s| s.as_str() == id).unwrap_or(false) {
            if let Some(n) = names.row_data(i) {
                return n.to_string();
            }
        }
    }
    id.to_string()
}

/// The voices grid, split for the UI: bundled presets collapse into the Kokoro
/// dropdown; user-created profiles become individual cards.
/// Send-able grid entry — slint's `image` type isn't Send, so the worker builds
/// these and the UI thread converts them (loading avatar files) in
/// `to_voice_items`.
#[derive(Clone, Default)]
struct VoiceData {
    id: String,
    name: String,
    desc: String,
    lang: String,
    kind: String,
    has_personality: bool,
    avatar_path: String,
    avatar_mode: String,
    avatar_sx: i32,
    avatar_sy: i32,
    avatar_side: i32,
    avatar_sh: i32,
}

/// Baked avatar pixels: RGBA bytes + dimensions (Send-able, unlike slint::Image).
type RgbaBuf = (Vec<u8>, u32, u32);

/// Decode a photo, apply the crop rect, and downscale with a proper filter.
/// The GPU's plain bilinear sampling turns a 4K photo minified into a small
/// circle into visible pixelation — so we hand it a ≤400px thumbnail instead.
/// Cached by path + mtime + rect since grids rebake on every refresh.
fn bake_avatar_rgba(
    cache: &mut HashMap<String, RgbaBuf>,
    path: &str,
    sx: i32,
    sy: i32,
    sw: i32,
    sh: i32,
) -> Option<RgbaBuf> {
    if path.is_empty() || sw <= 0 {
        return None;
    }
    let sh = if sh > 0 { sh } else { sw };
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = format!("{path}|{mtime}|{sx},{sy},{sw},{sh}");
    if let Some(b) = cache.get(&key) {
        return Some(b.clone());
    }
    let img = image::open(path).ok()?;
    let (w, h) = (img.width(), img.height());
    let cx = (sx.max(0) as u32).min(w.saturating_sub(1));
    let cy = (sy.max(0) as u32).min(h.saturating_sub(1));
    let cw = (sw as u32).min(w - cx).max(1);
    let ch = (sh as u32).min(h - cy).max(1);
    let thumb = img.crop_imm(cx, cy, cw, ch).thumbnail(400, 400);
    let rgba = thumb.to_rgba8();
    let buf = (rgba.as_raw().clone(), rgba.width(), rgba.height());
    cache.insert(key, buf.clone());
    Some(buf)
}

/// UI-thread half of avatar handling: turn baked RGBA into a slint Image.
fn rgba_to_image(buf: &RgbaBuf) -> slint::Image {
    let pb = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&buf.0, buf.1, buf.2);
    slint::Image::from_rgba8(pb)
}

/// UI-thread conversion of pre-baked grid data into model rows.
fn to_voice_items(data: Vec<(VoiceData, Option<RgbaBuf>)>) -> Vec<VoiceItem> {
    data.into_iter()
        .map(|(d, baked)| {
            let (avatar, has) = match &baked {
                Some(b) => (rgba_to_image(b), true),
                None => (Default::default(), false),
            };
            VoiceItem {
                id: d.id.into(),
                name: d.name.into(),
                desc: d.desc.into(),
                lang: d.lang.into(),
                kind: d.kind.into(),
                has_personality: d.has_personality,
                avatar,
                avatar_mode: if d.avatar_mode.is_empty() { "circle".into() } else { d.avatar_mode.into() },
                has_avatar: has,
            }
        })
        .collect()
}

/// Worker-side pairing of grid entries with their baked avatar thumbnails.
fn bake_grid(
    cache: &mut HashMap<String, RgbaBuf>,
    grid: Vec<VoiceData>,
) -> Vec<(VoiceData, Option<RgbaBuf>)> {
    grid.into_iter()
        .map(|d| {
            let baked = bake_avatar_rgba(
                cache, &d.avatar_path, d.avatar_sx, d.avatar_sy, d.avatar_side, d.avatar_sh,
            );
            (d, baked)
        })
        .collect()
}

struct GridData {
    grid: Vec<VoiceData>,          // [Kokoro card, user voices…, spacer padding]
    kokoro_names: Vec<SharedString>,
    kokoro_ids: Vec<SharedString>,
    default_selected: String,      // a bundled voice, so generation works out of the box
}

/// Build the grid from ListVoices (built-ins `builtin:…` + profile ids) enriched
/// with profile details from ListProfiles.
fn build_grid(raw: Vec<(String, String)>, profiles_json: &str) -> GridData {
    let profs: Vec<serde_json::Value> = serde_json::from_str(profiles_json).unwrap_or_default();
    let mut pmap: HashMap<String, serde_json::Value> = HashMap::new();
    for p in profs {
        if let Some(id) = p.get("id").and_then(|v| v.as_str()) {
            pmap.insert(id.to_string(), p);
        }
    }

    let mut kokoro_names: Vec<SharedString> = Vec::new();
    let mut kokoro_ids: Vec<SharedString> = Vec::new();
    let mut users: Vec<VoiceData> = Vec::new();

    for (id, name) in raw {
        if id.starts_with("builtin:") {
            kokoro_names.push(name.into());
            kokoro_ids.push(id.into());
        } else {
            let hp = pmap
                .get(&id)
                .and_then(|p| p.get("has_personality"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let (desc, lang, kind, avatar_path, avatar_mode, asx, asy, aside, ash) = if let Some(p) = pmap.get(&id) {
                let vt = p.get("voice_type").and_then(|v| v.as_str()).unwrap_or("voice");
                let l = p.get("language").and_then(|v| v.as_str()).unwrap_or("en");
                // the profile's own description, falling back to a kind label
                let d = match p.get("description").and_then(|v| v.as_str()) {
                    Some(d) if !d.trim().is_empty() => d.to_string(),
                    _ if hp => "Has personality".to_string(),
                    _ => "Custom voice".to_string(),
                };
                let i = |k: &str| p.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                (
                    d,
                    l.to_string(),
                    vt.to_string(),
                    p.get("avatar_path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    p.get("avatar_mode").and_then(|v| v.as_str()).unwrap_or("circle").to_string(),
                    i("avatar_sx"),
                    i("avatar_sy"),
                    i("avatar_side"),
                    i("avatar_sh"),
                )
            } else {
                (String::new(), "en".to_string(), "voice".to_string(),
                 String::new(), "circle".to_string(), 0, 0, 0, 0)
            };
            users.push(VoiceData {
                id,
                name,
                desc,
                lang,
                kind,
                has_personality: hp,
                avatar_path,
                avatar_mode,
                avatar_sx: asx,
                avatar_sy: asy,
                avatar_side: aside,
                avatar_sh: ash,
            });
        }
    }

    // grid = Kokoro Defaults card + user cards, padded to a multiple of 3 with
    // invisible spacers so the 3-column GridLayout always has full first row.
    let mut grid: Vec<VoiceData> = Vec::with_capacity(users.len() + 3);
    grid.push(VoiceData {
        id: "__kokoro__".into(),
        name: "Kokoro Defaults".into(),
        kind: "model-defaults".into(),
        ..Default::default()
    });
    grid.extend(users);
    while !grid.len().is_multiple_of(3) {
        grid.push(VoiceData {
            kind: "empty".into(),
            ..Default::default()
        });
    }

    let default_selected = kokoro_ids.first().map(|s| s.to_string()).unwrap_or_default();
    GridData { grid, kokoro_names, kokoro_ids, default_selected }
}

/// Delivery directions for the composer style dropdown. Labels must match
/// the dropdown model in main.slint; the instruct text is sent verbatim to
/// the engine (SetStyle) and honored by the qwen engines. Phrasing is
/// deliberately intense — subtle directions barely move the performance.
const STYLES: &[(&str, &str)] = &[
    ("No direction", ""),
    ("Angry", "Speak in an extremely angry, furious tone — seething, sharp, and aggressive, as if barely containing rage."),
    ("Sad", "Speak in a deeply sad, sorrowful tone — heavy, slow, and grief-stricken, as if on the verge of tears."),
    ("Happy", "Speak in an intensely happy, joyful tone — bright, warm, and beaming with delight."),
    ("Excited", "Speak with overwhelming excitement and energy — fast, breathless, and absolutely thrilled."),
    ("Fearful", "Speak in a terrified, trembling tone — shaky, urgent, and full of dread."),
    ("Whisper", "Speak in a hushed, intense whisper — quiet, breathy, close, and conspiratorial."),
    ("Serious", "Speak in a grave, deadly serious tone — measured, cold, and commanding."),
];

/// Short human message for a failed profile call (either transport). Matches on
/// the raw engine failure text, which `EngineError::Display` surfaces verbatim.
fn profile_err_msg(e: &EngineError) -> String {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed: profiles.name") {
        "A voice with that name already exists.".into()
    } else {
        s
    }
}

/// Temp WAV path for in-modal recording (runtime dir → RAM, cleaned on logout).
fn cv_wav_path() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/syrinx-cv-record.wav"))
        .unwrap_or_else(|_| "/tmp/syrinx-cv-record.wav".into())
}

/// The default sink's `.monitor` source — a passive tap for system audio (the
/// same approach as Voicebox). Works on analog/HDMI; Bluetooth A2DP monitors are
/// silent, so those need a speaker/HDMI output while capturing.
async fn default_monitor() -> Option<String> {
    let out = tokio::process::Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sink.is_empty()).then(|| format!("{sink}.monitor"))
}

/// Spawn `parecord` to `wav`, optionally from a specific `device` (a sink's
/// `.monitor` for system audio). We use `parecord` (PulseAudio) rather than
/// `pw-record`: the latter's `--target` silently no-ops for monitors here, so it
/// only ever recorded the (dead) default mic.
async fn start_pw_record(wav: &str, device: Option<&str>) -> std::io::Result<tokio::process::Child> {
    let _ = std::fs::remove_file(wav);
    let mut cmd = tokio::process::Command::new("parecord");
    cmd.args(["--file-format=wav", "--rate=24000", "--channels=1", "--format=s16le"]);
    if let Some(d) = device {
        cmd.arg(format!("--device={d}"));
    }
    cmd.arg(wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// RMS level of a PCM16 mono WAV (0..1), for detecting silent captures.
fn wav_rms(path: &str) -> Option<f32> {
    let bytes = std::fs::read(path).ok()?;
    let data = bytes.windows(4).position(|w| w == b"data")? + 8; // past "data"+size
    let pcm = bytes.get(data..)?;
    let mut sumsq = 0f64;
    let mut count = 0u64;
    let mut i = 0;
    while i + 1 < pcm.len() {
        let s = i16::from_le_bytes([pcm[i], pcm[i + 1]]) as f64 / 32768.0;
        sumsq += s * s;
        count += 1;
        i += 2;
    }
    (count > 0).then(|| (sumsq / count as f64).sqrt() as f32)
}

/// Label for a finished recording, warning if it came out silent.
fn recorded_label(wav: &str) -> String {
    match wav_rms(wav) {
        Some(rms) if rms < 0.006 => "⚠ silent — check input / output device".into(),
        _ => "clip recorded ✓".into(),
    }
}

/// SIGINT `pw-record` so it finalizes the WAV header, then reap it.
///
/// The wait is bounded: a recorder that shrugs off the SIGINT (freshly spawned
/// children inherit an ignored SIGINT until parecord installs its handler)
/// would otherwise park the whole worker loop in `wait()` — frozen timers,
/// dead buttons. After 2 s we SIGKILL, which cannot be ignored.
async fn stop_pw_record(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status()
            .await;
    }
    let grace = std::time::Duration::from_secs(2);
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// Format seconds as m:ss (Voicebox-style meta).
fn fmt_dur(d: f64) -> String {
    let s = d.round().max(0.0) as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Build the history model from the engine's ListHistory JSON (newest first).
/// Engines whose history rows are conversions, not TTS generations.
fn is_vc_engine(engine: &str) -> bool {
    matches!(engine, "chatterbox_vc" | "seed_vc" | "vevo_timbre" | "vevo2")
}

/// Conversion-model ids, index-aligned with the vc-engine-names dropdown.
const VC_ENGINE_IDS: &[&str] = &["chatterbox_vc", "seed_vc", "vevo_timbre"];
/// Music-mode ids, index-aligned with vc-music-engine-names (singing-capable).
const VC_MUSIC_ENGINE_IDS: &[&str] = &["seed_vc", "vevo_timbre"];

fn build_history(json: &str) -> Vec<HistItem> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    arr.iter()
        .map(|h| {
            let get = |k: &str| h.get(k).and_then(|v| v.as_str()).unwrap_or("");
            let voice = {
                let n = get("voice_name");
                if n.is_empty() { get("voice_id") } else { n }
            };
            let engine = get("engine");
            let lang = get("language");
            let dur = h.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
            // "⇄ VC" labels conversions in the shared list; set_history_model
            // also keys the vc-tab rail off this prefix
            let meta = if is_vc_engine(engine) {
                format!("⇄ VC · {} · {}", fmt_dur(dur), lang)
            } else if engine.is_empty() {
                format!("{} · {}", fmt_dur(dur), lang)
            } else {
                format!("{} · {} · {}", engine, fmt_dur(dur), lang)
            };
            HistItem {
                id: get("id").into(),
                voice: voice.into(),
                meta: meta.into(),
                text: get("text").into(),
                starred: h.get("starred").and_then(|v| v.as_bool()).unwrap_or(false),
            }
        })
        .collect()
}

fn size_label(mb: i64) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

/// Build the three category model lists from the engine's ListModels JSON.
fn build_models(json: &str) -> (Vec<ModelItem>, Vec<ModelItem>, Vec<ModelItem>, Vec<ModelItem>) {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    let (mut voice, mut stt, mut llm, mut vc) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for m in arr.iter() {
        let s = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let b = |k: &str| m.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let mb = m.get("size_mb").and_then(|v| v.as_i64()).unwrap_or(0);
        let item = ModelItem {
            id: s("id").into(),
            display: s("display").into(),
            size_label: size_label(mb).into(),
            description: s("description").into(),
            downloaded: b("downloaded"),
            downloading: b("downloading"),
            active: b("active"),
            supported: b("supported"),
            warning: s("warning").into(),
            progress: 0.0,
        };
        match m.get("category").and_then(|v| v.as_str()).unwrap_or("") {
            "voice" => voice.push(item),
            "stt" => stt.push(item),
            "llm" => llm.push(item),
            "vc" => vc.push(item),
            _ => {}
        }
    }
    (voice, stt, llm, vc)
}

/// One-line hardware summary for the Models header.
fn hardware_line(json: &str) -> String {
    let h: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let cores = h.get("cores").and_then(|v| v.as_i64()).unwrap_or(0);
    let ram = h.get("ram_gb").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let gpu = h.get("gpu").and_then(|v| v.as_bool()).unwrap_or(false);
    let name = h.get("gpu_name").and_then(|v| v.as_str()).unwrap_or("");
    let gpu_part = if gpu {
        if name.is_empty() { "GPU".to_string() } else { name.to_string() }
    } else {
        "no GPU".to_string()
    };
    format!("{cores} cores · {ram:.1} GB RAM · {gpu_part}")
}

/// Re-fetch the catalog + hardware and push into the UI. Also rebuilds the
/// composer's engine dropdown and the modal's default-model options (usable =
/// downloaded + supported voice models); returns (model id, engine) pairs in
/// dropdown order plus the active voice engine name.
async fn refresh_models(
    ui: &slint::Weak<AppWindow>,
    proxy: &EngineClient,
) -> (Vec<(String, String)>, String) {
    let models_json = proxy.list_models().await.unwrap_or_else(|_| "[]".into());
    let hw_json = proxy.hardware().await.unwrap_or_default();
    let (voice, stt, llm, vc_conv) = build_models(&models_json);
    let hwline = hardware_line(&hw_json);

    let arr: Vec<serde_json::Value> = serde_json::from_str(&models_json).unwrap_or_default();
    let mut models: Vec<(String, String)> = Vec::new(); // (model id, engine)
    let mut eng_names: Vec<SharedString> = Vec::new();
    let mut active_idx: i32 = 0;
    let mut active_engine = String::from("kokoro");
    for m in &arr {
        let s = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let b = |k: &str| m.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        if s("category") == "voice" && b("downloaded") && b("supported") {
            if b("active") {
                active_idx = models.len() as i32;
                active_engine = s("engine").to_string();
            }
            models.push((s("id").to_string(), s("engine").to_string()));
            eng_names.push(s("display").into());
        }
    }
    // modal picker: "Follow active model" + the same list
    let cv_options: Vec<SharedString> = std::iter::once(SharedString::from("Follow active model"))
        .chain(eng_names.iter().cloned())
        .collect();

    ui.upgrade_in_event_loop(move |ui| {
        ui.set_voice_models(ModelRc::from(Rc::new(VecModel::from(voice))));
        ui.set_stt_models(ModelRc::from(Rc::new(VecModel::from(stt))));
        ui.set_llm_models(ModelRc::from(Rc::new(VecModel::from(llm))));
        ui.set_vc_conv_models(ModelRc::from(Rc::new(VecModel::from(vc_conv))));
        ui.set_hardware_line(hwline.into());
        ui.set_composer_engines(ModelRc::from(Rc::new(VecModel::from(eng_names))));
        ui.set_composer_engine_index(active_idx);
        ui.set_cv_model_options(ModelRc::from(Rc::new(VecModel::from(cv_options))));
    })
    .ok();
    (models, active_engine)
}

/// Rebuild the voice-card grid from the engine (after create/edit/delete/import).
async fn refresh_grid(
    ui: &slint::Weak<AppWindow>,
    proxy: &EngineClient,
    cache: &mut HashMap<String, RgbaBuf>,
) {
    let raw = proxy.list_voices().await.unwrap_or_default();
    let pj = proxy.list_profiles().await.unwrap_or_else(|_| "[]".into());
    let GridData { grid, .. } = build_grid(raw, &pj);
    let grid = bake_grid(cache, grid);
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_voices(ModelRc::from(Rc::new(VecModel::from(to_voice_items(grid)))));
    })
    .ok();
}

/// One Voices-tab table row, thread-safe half (images bake on the UI thread).
#[derive(Clone)]
struct VpRowData {
    id: String,
    name: String,
    desc: String,
    lang: String,
    engine: String, // "follows" when unpinned
    samples: String,
    gens: String,
    baked: Option<RgbaBuf>,
}

/// Filter + convert to slint rows. UI-thread only (creates Images).
fn vp_to_rows(data: &[VpRowData], filter: &str) -> Vec<ProfileRow> {
    let q = filter.to_lowercase();
    data.iter()
        .filter(|d| {
            q.is_empty()
                || d.name.to_lowercase().contains(&q)
                || d.desc.to_lowercase().contains(&q)
                || d.lang.to_lowercase().contains(&q)
                || d.engine.to_lowercase().contains(&q)
        })
        .map(|d| ProfileRow {
            id: d.id.clone().into(),
            name: d.name.clone().into(),
            desc: d.desc.clone().into(),
            lang: d.lang.clone().into(),
            engine: d.engine.clone().into(),
            samples: d.samples.clone().into(),
            gens: d.gens.clone().into(),
            avatar: d.baked.as_ref().map(rgba_to_image).unwrap_or_default(),
            has_avatar: d.baked.is_some(),
        })
        .collect()
}

/// Fill the Voices-tab inspector from GetProfile (+ cached table row data).
async fn inspect_profile(
    ui: &slint::Weak<AppWindow>,
    proxy: &EngineClient,
    voices_all: &[VpRowData],
    id: &str,
) {
    let Ok(pj) = proxy.get_profile(id).await else { return };
    if pj.is_empty() {
        return; // deleted since selection
    }
    let p: serde_json::Value = serde_json::from_str(&pj).unwrap_or_default();
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let samples: Vec<(String, String)> = p
        .get("samples")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|smp| (
                    smp.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    smp.get("reference_text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                ))
                .collect()
        })
        .unwrap_or_default();
    let row = voices_all.iter().find(|d| d.id == id);
    let gens = row.map(|d| d.gens.clone()).unwrap_or_else(|| "0".into());
    let baked = row.and_then(|d| d.baked.clone());
    let engine = {
        let e = s("default_engine");
        if e.is_empty() { "follows".to_string() } else { e }
    };
    let (name, desc, pers, lang) = (s("name"), s("description"), s("personality"), s("language"));
    let id2 = id.to_string();
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_vp_selected(id2.into());
        ui.set_vi_name(name.into());
        ui.set_vi_desc(desc.into());
        ui.set_vi_personality(pers.into());
        ui.set_vi_lang(lang.into());
        ui.set_vi_engine(engine.into());
        ui.set_vi_gens(gens.into());
        match baked {
            Some(b) => { ui.set_vi_avatar(rgba_to_image(&b)); ui.set_vi_has_avatar(true); }
            None => { ui.set_vi_avatar(Default::default()); ui.set_vi_has_avatar(false); }
        }
        ui.set_vi_samples(ModelRc::from(Rc::new(VecModel::from(
            samples
                .into_iter()
                .map(|(sid, t)| SampleRow { id: sid.into(), text: t.into() })
                .collect::<Vec<_>>(),
        ))));
    })
    .ok();
}

/// Rebuild the Voices-tab table (profiles + per-voice generation counts).
async fn refresh_voices_table(
    ui: &slint::Weak<AppWindow>,
    proxy: &EngineClient,
    cache: &mut HashMap<String, RgbaBuf>,
    out: &mut Vec<VpRowData>,
) {
    let pj = proxy.list_profiles().await.unwrap_or_else(|_| "[]".into());
    let hj = proxy.list_history().await.unwrap_or_else(|_| "[]".into());
    let profs: Vec<serde_json::Value> = serde_json::from_str(&pj).unwrap_or_default();
    let hist: Vec<serde_json::Value> = serde_json::from_str(&hj).unwrap_or_default();
    let mut gens: HashMap<String, usize> = HashMap::new();
    for h in &hist {
        if let Some(v) = h.get("voice_id").and_then(|v| v.as_str()) {
            *gens.entry(v.to_string()).or_default() += 1;
        }
    }
    *out = profs
        .iter()
        .filter_map(|p| {
            let id = p.get("id")?.as_str()?.to_string();
            let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let iv = |k: &str| p.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let baked = bake_avatar_rgba(
                cache, &s("avatar_path"), iv("avatar_sx"), iv("avatar_sy"),
                iv("avatar_side"), iv("avatar_sh"),
            );
            let engine = {
                let e = s("default_engine");
                if e.is_empty() { "follows".to_string() } else { e }
            };
            Some(VpRowData {
                id: id.clone(),
                name: s("name"),
                desc: s("description"),
                lang: s("language"),
                engine,
                samples: p.get("samples").and_then(|v| v.as_i64()).unwrap_or(0).to_string(),
                gens: gens.get(&id).copied().unwrap_or(0).to_string(),
                baked,
            })
        })
        .collect();
    let rows_src = out.clone();
    ui.upgrade_in_event_loop(move |ui| {
        let filter = ui.get_vp_search().to_string();
        ui.set_vp_rows(ModelRc::from(Rc::new(VecModel::from(vp_to_rows(&rows_src, &filter)))));
        // drop a selection whose profile no longer exists
        let sel = ui.get_vp_selected().to_string();
        if !sel.is_empty() && !rows_src.iter().any(|d| d.id == sel) {
            ui.set_vp_selected("".into());
        }
    })
    .ok();
}

/// Language of a Kokoro preset from its id convention: `builtin:kokoro:af_…`
/// — first letter = language (a/b American/British English, e Spanish, …).
fn kokoro_lang_code(voice_id: &str) -> &'static str {
    match voice_id.rsplit(':').next().and_then(|v| v.chars().next()) {
        Some('a') | Some('b') => "en",
        Some('e') => "es",
        Some('f') => "fr",
        Some('h') => "hi",
        Some('i') => "it",
        Some('j') => "ja",
        Some('p') => "pt",
        Some('z') => "zh",
        _ => "en",
    }
}

/// Kokoro id prefixes for a language code (inverse of kokoro_lang_code).
fn kokoro_prefixes(code: &str) -> &'static [char] {
    match code {
        "es" => &['e'],
        "fr" => &['f'],
        "hi" => &['h'],
        "it" => &['i'],
        "ja" => &['j'],
        "pt" => &['p'],
        "zh" => &['z'],
        _ => &['a', 'b'], // en
    }
}

/// Voicebox's per-engine language subsets (label, code), in Voicebox order.
fn langs_for_engine(engine: &str) -> Vec<(&'static str, &'static str)> {
    const ALL: &[(&str, &str)] = &[
        ("Arabic", "ar"), ("Danish", "da"), ("German", "de"), ("Greek", "el"),
        ("English", "en"), ("Spanish", "es"), ("Finnish", "fi"), ("French", "fr"),
        ("Hebrew", "he"), ("Hindi", "hi"), ("Italian", "it"), ("Japanese", "ja"),
        ("Korean", "ko"), ("Malay", "ms"), ("Dutch", "nl"), ("Norwegian", "no"),
        ("Polish", "pl"), ("Portuguese", "pt"), ("Russian", "ru"), ("Swedish", "sv"),
        ("Swahili", "sw"), ("Turkish", "tr"), ("Chinese", "zh"),
    ];
    let codes: &[&str] = match engine {
        "qwen" | "qwen_custom_voice" => &["zh", "en", "ja", "ko", "de", "fr", "ru", "pt", "es", "it"],
        "luxtts" | "chatterbox_turbo" => &["en"],
        "chatterbox" => return ALL.to_vec(),
        "tada" => &["en", "ar", "zh", "de", "es", "fr", "it", "ja", "pl", "pt"],
        _ => &["en", "es", "fr", "hi", "it", "pt", "ja", "zh"], // kokoro
    };
    codes
        .iter()
        .filter_map(|c| ALL.iter().find(|(_, code)| code == c).copied())
        .collect()
}

/// Push the language dropdown for `engine`, preselecting `current_code`;
/// returns the codes in dropdown order (for index → code lookups).
fn update_composer_langs(
    ui: &slint::Weak<AppWindow>,
    engine: &str,
    current_code: &str,
) -> Vec<&'static str> {
    let pairs = langs_for_engine(engine);
    let labels: Vec<SharedString> = pairs.iter().map(|(l, _)| SharedString::from(*l)).collect();
    let codes: Vec<&'static str> = pairs.iter().map(|(_, c)| *c).collect();
    let idx = codes.iter().position(|c| *c == current_code).unwrap_or(0) as i32;
    // only the qwen engines honor delivery instructs — hide the style
    // dropdown for the rest instead of offering a knob that does nothing
    let styled = matches!(engine, "qwen" | "qwen_custom_voice");
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_composer_langs(ModelRc::from(Rc::new(VecModel::from(labels))));
        ui.set_composer_lang_index(idx);
        ui.set_style_supported(styled);
    })
    .ok();
    codes
}

/// Update a single model row's download progress in place (no refetch).
fn set_model_progress(ui: &slint::Weak<AppWindow>, id: String, pct: f32, downloading: bool) {
    ui.upgrade_in_event_loop(move |ui| {
        for model in [ui.get_voice_models(), ui.get_stt_models(), ui.get_llm_models(), ui.get_vc_conv_models()] {
            for i in 0..model.row_count() {
                if let Some(mut it) = model.row_data(i) {
                    if it.id.as_str() == id {
                        it.progress = pct;
                        it.downloading = downloading;
                        model.set_row_data(i, it);
                        return;
                    }
                }
            }
        }
    })
    .ok();
}

/// Parse a FileEnvelope reply into (bars, duration).
fn parse_envelope(json: &str) -> Option<(Vec<f32>, f64)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let dur = v.get("duration")?.as_f64()?;
    let bars: Vec<f32> = v
        .get("bars")?
        .as_array()?
        .iter()
        .filter_map(|b| b.as_f64().map(|x| x as f32))
        .collect();
    if dur <= 0.0 || bars.is_empty() {
        return None;
    }
    Some((bars, dur))
}

/// Populate and show the trim modal with handles reset to the full clip.
fn open_trim_modal(ui: &slint::Weak<AppWindow>, title: String, bars: Vec<f32>, dur: f64) {
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_trim_bars(ModelRc::from(Rc::new(VecModel::from(bars))));
        ui.set_trim_title(title.into());
        ui.set_trim_duration(dur as f32);
        ui.set_trim_start(0.0);
        ui.set_trim_end(1.0);
        ui.set_trim_playing(false);
        ui.set_trim_open(true);
    })
    .ok();
}

/// Replace the history model's contents in place (keeps the shared VecModel).
fn set_history_model(ui: &AppWindow, items: Vec<HistItem>) {
    // the ⇄ tab's CONVERSIONS rail is the same data filtered to VC rows,
    // derived here so every history refresh keeps both models in sync
    let vc: Vec<HistItem> = items
        .iter()
        .filter(|h| h.meta.starts_with("⇄ VC"))
        .map(|h| {
            let mut h = h.clone();
            h.meta = h.meta.strip_prefix("⇄ VC · ").unwrap_or(&h.meta).into();
            h
        })
        .collect();
    // music covers carry the ♫ marker on the voice segment of the title;
    // the ⇄ rail shows only the current mode's rows
    let (vc_music, vc_speech): (Vec<HistItem>, Vec<HistItem>) = vc.into_iter().partition(|h| {
        h.voice
            .split(" · ")
            .next()
            .map(|s| s.trim_end().ends_with('♫'))
            .unwrap_or(false)
    });
    if let Some(vm) = ui.get_vc_history_speech().as_any().downcast_ref::<VecModel<HistItem>>() {
        vm.set_vec(vc_speech);
    } else {
        ui.set_vc_history_speech(ModelRc::from(Rc::new(VecModel::from(vc_speech))));
    }
    if let Some(vm) = ui.get_vc_history_music().as_any().downcast_ref::<VecModel<HistItem>>() {
        vm.set_vec(vc_music);
    } else {
        ui.set_vc_history_music(ModelRc::from(Rc::new(VecModel::from(vc_music))));
    }
    if let Some(vm) = ui.get_history().as_any().downcast_ref::<VecModel<HistItem>>() {
        vm.set_vec(items);
    } else {
        ui.set_history(ModelRc::from(Rc::new(VecModel::from(items))));
    }
}

/// Build the captures model from the engine's ListCaptures JSON (newest first).
fn build_captures(json: &str) -> Vec<CaptureItem> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    arr.iter()
        .map(|c| {
            let get = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("");
            CaptureItem {
                id: get("id").into(),
                text: get("text").into(),
                date: get("date").into(),
            }
        })
        .collect()
}

/// Export dialogs start in the Settings-tab folder when one is set.
fn export_dialog(cfg_dir: &str) -> rfd::AsyncFileDialog {
    let dlg = rfd::AsyncFileDialog::new();
    if cfg_dir.is_empty() { dlg } else { dlg.set_directory(cfg_dir) }
}

/// Enumerate PipeWire capture devices via pactl: (mics, sink monitors),
/// each as (technical name, human description).
async fn list_audio_devices() -> (Vec<(String, String)>, Vec<(String, String)>) {
    let out = tokio::process::Command::new("pactl")
        .args(["-f", "json", "list", "sources"])
        .output()
        .await;
    let Ok(out) = out else { return (Vec::new(), Vec::new()) };
    let arr: Vec<serde_json::Value> =
        serde_json::from_slice(&out.stdout).unwrap_or_default();
    let (mut mics, mut monitors) = (Vec::new(), Vec::new());
    for s in &arr {
        let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let desc = s.get("description").and_then(|v| v.as_str()).unwrap_or(name);
        if name.is_empty() {
            continue;
        }
        if name.ends_with(".monitor") {
            monitors.push((name.to_string(), desc.to_string()));
        } else {
            mics.push((name.to_string(), desc.to_string()));
        }
    }
    (mics, monitors)
}

/// Push the ⚙ tab's state to the UI (devices, config, engine knobs).
const ST_CAP_SECS: &[(i64, &str)] = &[(60, "1:00"), (180, "3:00"), (300, "5:00"), (600, "10:00")];
const ST_STEP_OPTS: &[i64] = &[10, 25, 30, 40, 50];

/// One library row's backing data (the slint model is derived per filter).
struct LibRow {
    id: String,
    title: String,
    meta: String,
    text: String,
    starred: bool,
    tags: Vec<String>,
    voice: String,  // ♫-stripped first title segment, for the voice filter
    kind: u8,       // 0 = TTS, 1 = speech VC, 2 = music
    engine: String, // engine id, for the model filter
    blob: String,   // lowercased title+text+tags, for search
}

/// Engine id → display label for the Library's model filter and row meta.
const LIB_ENGINE_LABELS: &[(&str, &str)] = &[
    ("kokoro", "Kokoro"),
    ("qwen", "Qwen TTS"),
    ("qwen_custom_voice", "Qwen CustomVoice"),
    ("luxtts", "LuxTTS"),
    ("chatterbox", "Chatterbox"),
    ("chatterbox_turbo", "Chatterbox Turbo"),
    ("tada", "TADA"),
    ("chatterbox_vc", "Chatterbox VC"),
    ("seed_vc", "Seed-VC"),
    ("vevo_timbre", "Vevo-Timbre"),
];

fn lib_engine_label(engine: &str) -> &str {
    LIB_ENGINE_LABELS
        .iter()
        .find(|(id, _)| *id == engine)
        .map(|(_, l)| *l)
        .unwrap_or(engine)
}

/// Model-filter options per type-dropdown index (0=All, 1=TTS, 2=speech VC,
/// 3=music). Music lists the singing-capable engines (Seed-VC and Vevo —
/// whose ♫ requests run Vevo2 in the same worker).
fn lib_engines_for_type(type_idx: i32) -> Vec<&'static str> {
    match type_idx {
        1 => vec!["kokoro", "qwen", "qwen_custom_voice", "luxtts",
                  "chatterbox", "chatterbox_turbo", "tada"],
        2 => vec!["chatterbox_vc", "seed_vc", "vevo_timbre"],
        3 => VC_MUSIC_ENGINE_IDS.to_vec(),
        _ => LIB_ENGINE_LABELS.iter().map(|(e, _)| *e).collect(),
    }
}

/// Fetch and classify all generations for the Library.
async fn lib_load(proxy: &EngineClient) -> (Vec<LibRow>, Vec<String>) {
    let j = proxy.list_history().await.unwrap_or_else(|_| "[]".into());
    let arr: Vec<serde_json::Value> = serde_json::from_str(&j).unwrap_or_default();
    let mut rows = Vec::new();
    let mut voices: Vec<String> = Vec::new();
    for h in &arr {
        let s = |k: &str| h.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let title = {
            let n = s("voice_name");
            if n.is_empty() { s("voice_id") } else { n }
        };
        let engine = s("engine");
        let first_seg = title.split(" · ").next().unwrap_or("").trim().to_string();
        let kind = if is_vc_engine(&engine) {
            if first_seg.ends_with('♫') { 2 } else { 1 }
        } else {
            0
        };
        let voice = first_seg.trim_end_matches('♫').trim().to_string();
        let dur = h.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let tags: Vec<String> = h
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let text = s("text");
        // the same details the TTS history cards carry: type · model ·
        // length · language, plus the date
        let meta = format!(
            "{} · {} · {} · {} · {}",
            ["TTS", "⇄ VC", "♫ VC"][kind as usize],
            lib_engine_label(&engine),
            fmt_dur(dur), s("language"), s("date"),
        );
        let blob = format!("{} {} {}", title, text, tags.join(" ")).to_lowercase();
        if !voice.is_empty() && !voices.contains(&voice) {
            voices.push(voice.clone());
        }
        rows.push(LibRow {
            id: s("id"), title, meta, text,
            starred: h.get("starred").and_then(|v| v.as_bool()).unwrap_or(false),
            tags, voice, kind, engine, blob,
        });
    }
    voices.sort();
    (rows, voices)
}

/// Apply the current filters and push the derived model + count line.
fn lib_apply(
    ui: &slint::Weak<AppWindow>,
    rows: &[LibRow],
    voices: &[String],
    filters: &(String, i32, i32, bool, i32),
) {
    let (q, type_idx, voice_idx, starred_only, model_idx) = filters;
    let q = q.trim().to_lowercase();
    let want_voice = if *voice_idx > 0 {
        voices.get((*voice_idx - 1) as usize).cloned()
    } else {
        None
    };
    // model options follow the chosen type; index 0 = All models
    let type_engines = lib_engines_for_type(*type_idx);
    let want_engine = if *model_idx > 0 {
        type_engines.get((*model_idx - 1) as usize).copied()
    } else {
        None
    };
    let shown: Vec<LibItem> = rows
        .iter()
        .filter(|r| q.is_empty() || r.blob.contains(&q))
        .filter(|r| *type_idx == 0 || r.kind == (*type_idx - 1) as u8)
        .filter(|r| want_voice.as_deref().is_none_or(|v| r.voice == v))
        .filter(|r| want_engine.is_none_or(|e| r.engine == e))
        .filter(|r| !*starred_only || r.starred)
        .map(|r| LibItem {
            id: r.id.clone().into(),
            title: r.title.clone().into(),
            meta: r.meta.clone().into(),
            text: r.text.clone().into(),
            starred: r.starred,
            tags: r.tags.join(", ").into(),
        })
        .collect();
    let count = format!("{} of {} generations", shown.len(), rows.len());
    let names: Vec<SharedString> = std::iter::once(SharedString::from("All voices"))
        .chain(voices.iter().map(|v| SharedString::from(v.as_str())))
        .collect();
    let voice_count = names.len() as i32;
    let model_names: Vec<SharedString> = std::iter::once(SharedString::from("All models"))
        .chain(type_engines.iter().map(|e| SharedString::from(lib_engine_label(e))))
        .collect();
    let model_count = model_names.len() as i32;
    ui.upgrade_in_event_loop(move |ui| {
        if ui.get_lib_voice_index() >= voice_count { ui.set_lib_voice_index(0); }
        if ui.get_lib_model_index() >= model_count { ui.set_lib_model_index(0); }
        ui.set_lib_voice_names(ModelRc::from(Rc::new(VecModel::from(names))));
        ui.set_lib_model_names(ModelRc::from(Rc::new(VecModel::from(model_names))));
        ui.set_lib_rows(ModelRc::from(Rc::new(VecModel::from(shown))));
        ui.set_lib_count_line(count.into());
    })
    .ok();
}

/// Refresh the ⇄ tab's saved-clip rail; returns (id, name, path) rows the
/// worker keeps for arming/audition/deletion by id.
async fn refresh_vc_clips(
    ui: &slint::Weak<AppWindow>,
    proxy: &EngineClient,
) -> Vec<(String, String, String, String)> {
    let j = proxy.list_source_clips().await.unwrap_or_else(|_| "[]".into());
    let arr: Vec<serde_json::Value> = serde_json::from_str(&j).unwrap_or_default();
    let mut data = Vec::new();
    let mut items: Vec<SourceClipItem> = Vec::new();
    for c in &arr {
        let g = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let (id, name, path, meta) = (g("id"), g("name"), g("path"), g("meta"));
        items.push(SourceClipItem {
            id: id.clone().into(),
            name: name.clone().into(),
            meta: meta.into(),
        });
        data.push((id, name, path, g("transcript")));
    }
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_vc_clips(ModelRc::from(Rc::new(VecModel::from(items))));
    }).ok();
    data
}

fn set_captures_model(ui: &AppWindow, items: Vec<CaptureItem>) {
    if let Some(vm) = ui.get_captures().as_any().downcast_ref::<VecModel<CaptureItem>>() {
        vm.set_vec(items);
    } else {
        ui.set_captures(ModelRc::from(Rc::new(VecModel::from(items))));
    }
}

/// Refresh the composer effects dropdown ("No effects" + presets) and the
/// editor's preset list. Returns (dropdown ids, editor (id, builtin) pairs).
async fn refresh_effect_presets(
    ui: &slint::Weak<AppWindow>,
    proxy: &EngineClient,
) -> (Vec<String>, Vec<(String, bool)>) {
    let fx_json = proxy.list_effect_presets().await.unwrap_or_else(|_| "[]".into());
    let fx: Vec<serde_json::Value> = serde_json::from_str(&fx_json).unwrap_or_default();
    let mut labels: Vec<SharedString> = vec!["No effects".into()];
    let mut ids = vec![String::new()];
    let mut pairs = Vec::new();
    let mut items = Vec::new();
    for p in &fx {
        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let builtin = p.get("builtin").and_then(|v| v.as_bool()).unwrap_or(true);
        labels.push(name.into());
        ids.push(id.to_string());
        pairs.push((id.to_string(), builtin));
        items.push(FxPresetItem { id: id.into(), name: name.into(), builtin });
    }
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_composer_effects(ModelRc::from(Rc::new(VecModel::from(labels))));
        ui.set_fxe_presets(ModelRc::from(Rc::new(VecModel::from(items))));
    })
    .ok();
    (ids, pairs)
}

/// Format an effect param value with decimals matched to its step size.
fn fx_fmt(v: f64, step: f64) -> String {
    if step >= 1.0 {
        format!("{v:.0}")
    } else if step >= 0.1 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// Push the editor's chain (and the expanded row's params) into the UI models.
fn fxe_sync(
    ui: &slint::Weak<AppWindow>,
    defs: &[serde_json::Value],
    chain: &[serde_json::Value],
    expanded: i32,
) {
    let def_of = |t: &str| defs.iter().find(|d| d.get("id").and_then(|v| v.as_str()) == Some(t));
    let rows: Vec<FxRowItem> = chain
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let label = def_of(t)
                .and_then(|d| d.get("label"))
                .and_then(|v| v.as_str())
                .unwrap_or(t);
            FxRowItem {
                label: label.into(),
                enabled: e.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                expanded: i as i32 == expanded,
            }
        })
        .collect();
    let params: Vec<FxParamItem> = chain
        .get(usize::try_from(expanded).unwrap_or(usize::MAX))
        .map(|e| {
            let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
            def_of(t)
                .and_then(|d| d.get("params"))
                .and_then(|p| p.as_array())
                .map(|list| {
                    list.iter()
                        .map(|pd| {
                            let name = pd.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let min = pd.get("min").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let max = pd.get("max").and_then(|v| v.as_f64()).unwrap_or(1.0);
                            let step = pd.get("step").and_then(|v| v.as_f64()).unwrap_or(0.01);
                            let dflt = pd.get("default").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let val = e
                                .get("params")
                                .and_then(|p| p.get(name))
                                .and_then(|v| v.as_f64())
                                .unwrap_or(dflt);
                            FxParamItem {
                                label: pd
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(name)
                                    .into(),
                                value_text: fx_fmt(val, step).into(),
                                norm: ((val - min) / (max - min)).clamp(0.0, 1.0) as f32,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_fxe_chain(ModelRc::from(Rc::new(VecModel::from(rows))));
        ui.set_fxe_params(ModelRc::from(Rc::new(VecModel::from(params))));
    })
    .ok();
}

/// Why a [`run_session`] returned: the UI quit for good, or the engine
/// transport dropped mid-session (Win/mac then respawn/reconnect).
enum SessionEnd {
    /// The command channel closed — the window is gone. Quit.
    UiQuit,
    /// The event stream closed — the transport died. Win/mac respawn/reconnect;
    /// the single-pass Linux worker discards this and exits (a bus drop only,
    /// never normal operation).
    TransportLost,
}

/// One engine session: connect is already done (the caller owns adopt/spawn),
/// so consume events, run the initial data loads, and drive the `select!` loop
/// until the UI quits or the transport drops. The body is byte-identical to the
/// old inline worker; only the caller decides whether a drop means reconnect.
async fn run_session(
    ui: &slint::Weak<AppWindow>,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    proxy: EngineClient,
) -> SessionEnd {
    // Own the weak handle locally so every `&ui` / `ui.…` call site below is
    // unchanged from when this was `worker(ui, rx)`.
    let ui = ui.clone();
    let mut events = proxy.events();

    let backend = proxy.backend().await.unwrap_or_else(|_| "cpu".into());
    let raw = proxy.list_voices().await.unwrap_or_default();
    let profiles_json = proxy.list_profiles().await.unwrap_or_else(|_| "[]".into());
    let GridData { grid, kokoro_names, kokoro_ids, default_selected } =
        build_grid(raw, &profiles_json);
    // full preset list kept app-side so the language dropdown can filter it
    let mut kokoro_all: Vec<(String, String)> = kokoro_ids
        .iter()
        .zip(kokoro_names.iter())
        .map(|(i, n)| (i.to_string(), n.to_string()))
        .collect();
    let hist_items = build_history(&proxy.list_history().await.unwrap_or_else(|_| "[]".into()));
    let capture_items = build_captures(&proxy.list_captures().await.unwrap_or_else(|_| "[]".into()));
    let mut avatar_cache: HashMap<String, RgbaBuf> = HashMap::new();
    let grid = bake_grid(&mut avatar_cache, grid);
    {
        ui.upgrade_in_event_loop(move |ui| {
            ui.set_backend(backend.into());
            ui.set_kokoro_names(ModelRc::from(Rc::new(VecModel::from(kokoro_names))));
            ui.set_kokoro_ids(ModelRc::from(Rc::new(VecModel::from(kokoro_ids))));
            ui.set_voices(ModelRc::from(Rc::new(VecModel::from(to_voice_items(grid)))));
            if ui.get_selected_voice().is_empty() {
                ui.set_selected_voice(default_selected.clone().into());
                ui.set_selected_voice_name(voice_name(&ui, &default_selected).into());
            }
            set_history_model(&ui, hist_items);
            set_captures_model(&ui, capture_items);
        })
        .ok();
    }

    let mut pending_llm: u32 = 0;
    let (mut voice_models, mut active_engine) = refresh_models(&ui, &proxy).await;
    let mut lang_codes = update_composer_langs(&ui, "kokoro", "en");
    // effects dropdown: "No effects" + engine presets (builtin + user)
    let (mut effect_ids, mut fxe_presets) = refresh_effect_presets(&ui, &proxy).await;
    // First engine round-trip is done (D-Bus activation + model warmup, ~10-20s
    // cold) — drop the splash. Unconditional: every call above swallows its own
    // errors, and an unreachable engine must land on the normal UI with its
    // empty states rather than a splash that never leaves.
    ui.upgrade_in_event_loop(|ui| ui.set_booting(false)).ok();
    // effects chain editor state — the worker owns the chain JSON
    let mut fxe_defs: Vec<serde_json::Value> = Vec::new();
    let mut fxe_chain: Vec<serde_json::Value> = Vec::new();
    let mut fxe_pid = String::new(); // loaded user preset id ("" = new / builtin copy)
    let mut fxe_expanded: i32 = -1;
    let mut current_gen: u32 = 0;
    let mut player_dur: f64 = 0.0;
    let mut current_play_gen: u32 = 0;
    let mut playing = false;
    // Voicebox-parity player/composer state
    let mut loop_on = false;
    let mut current_clip = String::new();
    let mut last_pct: f64 = 0.0;
    let mut speak_after_llm: Option<String> = None;
    let mut cv_edit: Option<String> = None;
    let mut voices_all: Vec<VpRowData> = Vec::new();  // voices-tab table backing data
    let mut vp_inspected = String::new();             // profile id shown in the inspector
    let mut sample_gen: u32 = 0;                      // audition playback gen (0 = none)
    let mut sample_playing = String::new();           // sample id being auditioned
    let mut cv_edit_transcript = String::new();
    let mut cv_avatar: Option<(String, String, i32, i32, i32, i32)> = None; // staged (path, mode, sx, sy, sw, sh)
    // transcription view state
    let mut tr_rec: Option<tokio::process::Child> = None;
    let mut tr_elapsed: u32 = 0;
    let tr_wav = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/syrinx-transcribe.wav"))
        .unwrap_or_else(|_| "/tmp/syrinx-transcribe.wav".into());
    const TR_REC_MAX: u32 = 600; // 10 min safety cap
    let mut pending_tr: u32 = 0;
    let mut pending_tr_refine: u32 = 0;
    // voice-changer (⇄) view state
    let mut vc_rec: Option<tokio::process::Child> = None;
    let mut vc_elapsed: u32 = 0;
    let vc_wav = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/syrinx-convert.wav"))
        .unwrap_or_else(|_| "/tmp/syrinx-convert.wav".into());
    const VC_REC_MAX: u32 = 180; // matches the engine's SYRINX_VC_MAX_SECS default
    let mut vc_source: Option<String> = None;       // armed source path
    let mut vc_voice_ids: Vec<String> = Vec::new(); // parallel to the dropdown names
    let mut pending_vc: u32 = 0;                    // in-flight conversion gen id
    let mut pending_vc_music = false;               // that conversion is a song cover
    // (id, name, path, cached transcript)
    let mut vc_clips_data: Vec<(String, String, String, String)> = Vec::new();
    let mut vc_audition_gen: u32 = 0;               // audition playback gen (0 = none)
    let mut vc_audition_id = String::new();         // clip id or "scratch" being played
    let mut pending_vc_tr: u32 = 0;                 // source auto-transcription req id
    let mut vc_tr_clip = String::new();             // saved clip awaiting transcript backfill
    // trim modal state (✂)
    let mut trim_ctx = String::new();   // "vc" | "cv" | "tr" | "hist"
    let mut trim_path = String::new();  // audio file under the handles (non-hist)
    let mut trim_hid = String::new();   // history clip id (hist context)
    let mut trim_dur = 0.0_f64;         // seconds, from FileEnvelope
    let mut trim_gen: u32 = 0;          // preview playback generation
    let mut trim_end_pct = 1.0_f64;     // preview auto-stop point (0..1)
    let mut tr_source = String::new();  // last transcription source (recording or import)
    // settings (⚙) — shared app config + enumerated capture devices
    let mut cfg = load_config();
    let mut st_mics: Vec<(String, String)> = Vec::new();
    let mut st_mons: Vec<(String, String)> = Vec::new();
    // library (▤) state — rows cached, filters applied app-side
    let mut lib_rows: Vec<LibRow> = Vec::new();
    let mut lib_voices: Vec<String> = Vec::new();
    let mut lib_loaded = false;
    let mut lib_filters: (String, i32, i32, bool, i32) = (String::new(), 0, 0, false, 0);
    // create-voice modal state
    let mut cv_rec: Option<tokio::process::Child> = None;
    let cv_wav = cv_wav_path();
    let mut cv_sample: Option<String> = None;
    let mut rec_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    // The interval is only polled while a recording is live; with the default
    // Burst behavior every idle minute becomes a backlog of instant ticks the
    // moment recording starts — the elapsed counter then blows through the cap
    // in milliseconds and insta-stops the capture. Delay = never tick faster
    // than the period, regardless of backlog.
    rec_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut rec_elapsed: u32 = 0;
    const REC_MAX: u32 = 30;

    // The one unified event stream feeds every arm the nine D-Bus signal
    // streams used to; the transport (D-Bus or RPC) is invisible here. The
    // handling body lives in a local macro so the events arm below stays
    // readable next to a `None`-means-transport-lost check (a closed channel
    // ends the session; Win/mac then reconnect, Linux exits — see the two
    // `worker` fns). It captures the surrounding worker locals by name.
    macro_rules! handle_event {
        ($ev:ident) => { match $ev {
                EngineEvent::AudioLevel { rms, .. } => {
                    let rms = rms as f32;
                    ui.upgrade_in_event_loop(move |ui| ui.set_level(rms)).ok();
                }
                EngineEvent::GenerationProgress { gen_id, state, .. } => {
                    // conversions report to the ⇄ tab, not the composer
                    let is_vc = pending_vc != 0 && gen_id == pending_vc;
                    if let Some(msg) = state.strip_prefix("error:") {
                        let msg = msg.trim().to_string();
                        ui.upgrade_in_event_loop(move |ui| {
                            if is_vc {
                                ui.set_vc_busy(false);
                                ui.set_vc_status("".into());
                                ui.set_vc_error(msg.into());
                            } else {
                                ui.set_gen_error(msg.into());
                            }
                        }).ok();
                    } else if is_vc {
                        let stage: SharedString = match state.as_str() {
                            "loading model" => "loading model…".into(),
                            "separating" => "separating stems…".into(),
                            "converting" if pending_vc_music => "converting vocals…".into(),
                            "converting" => "converting…".into(),
                            "remixing" => "remixing…".into(),
                            "playing" => "done — playing · saved to History".into(),
                            s => s.into(),
                        };
                        ui.upgrade_in_event_loop(move |ui| {
                            if stage.starts_with("done") { ui.set_vc_busy(false); }
                            ui.set_vc_status(stage);
                        }).ok();
                        // the clip is already saved when auto-play starts —
                        // surface it in the rail now, not when playback ends
                        if state == "playing" {
                            if let Ok(j) = proxy.list_history().await {
                                let items = build_history(&j);
                                ui.upgrade_in_event_loop(move |ui| set_history_model(&ui, items)).ok();
                            }
                        }
                    }
                }
                EngineEvent::TranscribeProgress { req_id, partial } => {
                    if req_id == pending_tr && pending_tr != 0 {
                        ui.upgrade_in_event_loop(move |ui| ui.set_tr_text(partial.into())).ok();
                    } else if req_id == pending_vc_tr && pending_vc_tr != 0 {
                        ui.upgrade_in_event_loop(move |ui| ui.set_vc_transcript(partial.into())).ok();
                    }
                }
                EngineEvent::TranscribeResult { req_id, text } => {
                    if req_id == pending_vc_tr && pending_vc_tr != 0 && req_id != pending_tr {
                        pending_vc_tr = 0;
                        // clip armed/saved before whisper finished — cache it
                        if !vc_tr_clip.is_empty() {
                            if !text.trim().is_empty() {
                                if let Err(e) =
                                    proxy.set_source_clip_transcript(&vc_tr_clip, &text).await
                                {
                                    tracing::error!("transcript backfill failed: {e}");
                                }
                                if let Some(row) =
                                    vc_clips_data.iter_mut().find(|(cid, _, _, _)| *cid == vc_tr_clip)
                                {
                                    row.3 = text.clone();
                                }
                            }
                            vc_tr_clip.clear();
                        }
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_vc_transcribing(false);
                            ui.set_vc_transcript(text.into());
                        }).ok();
                    } else if req_id == pending_tr && pending_tr != 0 {
                        pending_tr = 0;
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_tr_busy(false);
                            if text.trim().is_empty() {
                                ui.set_tr_status("no speech detected".into());
                            } else {
                                ui.set_tr_status("".into());
                                ui.set_tr_text(text.into());
                            }
                        }).ok();
                    }
                }
                EngineEvent::PlaybackInfo { gen_id, clip_id, title, duration, bars } => {
                    current_play_gen = gen_id;
                    playing = true;
                    player_dur = duration;
                    last_pct = 0.0;
                    current_clip = clip_id.clone();
                    let bars: Vec<f32> = serde_json::from_str(&bars).unwrap_or_default();
                    let time = format!("0:00 / {}", fmt_dur(duration));
                    ui.upgrade_in_event_loop(move |ui| {
                        ui.set_player_active_visible(true);
                        ui.set_player_bars(ModelRc::from(Rc::new(VecModel::from(bars))));
                        ui.set_player_title(title.into());
                        ui.set_player_id(clip_id.into());
                        ui.set_player_time(time.into());
                        ui.set_play_pct(0.0);
                        ui.set_player_active(true);
                        ui.set_player_playing(true);
                        ui.set_player_paused(false);
                        ui.set_synthesizing(false);
                    }).ok();
                }
                EngineEvent::ModelProgress { model_id, pct, status } => {
                    match status.as_str() {
                        "downloading" => set_model_progress(&ui, model_id, pct as f32, true),
                        _ => { // done / error
                            let r = refresh_models(&ui, &proxy).await;
                            voice_models = r.0;
                            active_engine = r.1;
                        }
                    }
                }
                EngineEvent::LlmResult { req_id, text } => {
                    // transcription-view refine result routes to tr-text
                    if req_id == pending_tr_refine && pending_tr_refine != 0 {
                        pending_tr_refine = 0;
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_tr_busy(false);
                            ui.set_tr_status("".into());
                            if !text.trim().is_empty() {
                                ui.set_tr_text(text.into());
                            }
                        }).ok();
                    } else if req_id == pending_llm && pending_llm != 0 {
                        pending_llm = 0;
                        let ui_text = text.clone();
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_llm_busy(false);
                            if !ui_text.trim().is_empty() {
                                ui.set_text(ui_text.into());
                            }
                        }).ok();
                        // persona flow: the rewrite came back — now synthesize it
                        if let Some(voice) = speak_after_llm.take() {
                            if text.trim().is_empty() {
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_generating(false);
                                    ui.set_synthesizing(false);
                                }).ok();
                            } else {
                                match proxy.speak(&text, &voice).await {
                                    Ok(id) => current_gen = id,
                                    Err(e) => tracing::error!("persona speak failed: {e}"),
                                }
                            }
                        }
                    }
                }
                EngineEvent::PlaybackProgress { gen_id, pct } => {
                    // trim preview reached the out-handle — stop there
                    if trim_gen != 0 && gen_id == trim_gen && pct >= trim_end_pct {
                        proxy.cancel(trim_gen).await.ok();
                        trim_gen = 0;
                        ui.upgrade_in_event_loop(|ui| ui.set_trim_playing(false)).ok();
                    }
                    if gen_id == current_play_gen {
                        last_pct = pct;
                        let pctf = pct as f32;
                        let time = format!("{} / {}", fmt_dur(pct * player_dur), fmt_dur(player_dur));
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_play_pct(pctf);
                            ui.set_player_time(time.into());
                        }).ok();
                    }
                }
                EngineEvent::SpeakEnded { gen_id } => {
                    let is_current = gen_id == current_play_gen;
                    if is_current { playing = false; }
                    // trim preview ran out (or was cancelled) — flip ▶ back
                    if trim_gen != 0 && gen_id == trim_gen {
                        trim_gen = 0;
                        ui.upgrade_in_event_loop(|ui| ui.set_trim_playing(false)).ok();
                    }
                    // conversion ran its course (played out or errored) — settle the ⇄ tab
                    if pending_vc != 0 && gen_id == pending_vc {
                        pending_vc = 0;
                        ui.upgrade_in_event_loop(|ui| {
                            ui.set_vc_busy(false);
                            if ui.get_vc_status().starts_with("done") {
                                ui.set_vc_status("done · saved to History".into());
                            }
                        }).ok();
                    }
                    // source-clip audition finished -> flip ■ back to ▶
                    if vc_audition_gen != 0 && gen_id == vc_audition_gen {
                        vc_audition_gen = 0;
                        vc_audition_id.clear();
                        ui.upgrade_in_event_loop(|ui| ui.set_vc_audition_id("".into())).ok();
                    }
                    // sample audition ran to its end (or was replaced) -> flip ■ back to ▶
                    let sample_done = sample_gen != 0 && gen_id == sample_gen;
                    if sample_done {
                        sample_gen = 0;
                        sample_playing.clear();
                        ui.upgrade_in_event_loop(|ui| ui.set_vs_playing("".into())).ok();
                    }
                    // Loop: re-trigger only when the clip ran to its natural end
                    // (a Stop/Cancel arrives with the progress short of 1.0).
                    let looping = is_current && loop_on && last_pct > 0.97 && !current_clip.is_empty();
                    // Refresh history only on success — never wipe the list on a failed call.
                    let refreshed = proxy.list_history().await.ok().map(|j| build_history(&j));
                    ui.upgrade_in_event_loop(move |ui| {
                        ui.set_generating(false);
                        ui.set_synthesizing(false);
                        ui.set_level(0.0);
                        if is_current && !looping {
                            ui.set_player_playing(false);
                            ui.set_player_paused(false);
                            ui.set_play_pct(1.0);
                        }
                        if let Some(items) = refreshed {
                            set_history_model(&ui, items);
                        }
                    }).ok();
                    if looping {
                        last_pct = 0.0;
                        if let Ok(gid) = proxy.play_history(&current_clip).await {
                            current_gen = gid;
                        }
                    }
                }
                // SpeakStarted / PropertiesChanged: the app consumes neither
                // (the D-Bus path never subscribed to them either).
                _ => {}
        } };
    }

    let end: SessionEnd = loop {
        tokio::select! {
            // A closed event stream means the transport dropped. On Win/mac the
            // supervisor respawns/reconnects (SessionEnd::TransportLost); on
            // Linux the session bus stays up for the app's life, so this only
            // ever trips on an abnormal bus drop — ending the session, which the
            // single-pass Linux worker treats as a plain exit. (tokio's select!
            // can't `#[cfg]` a branch, so the arm is shared; the behavior split
            // lives in the two `worker` functions, not here.)
            ev = events.recv() => match ev {
                Some(ev) => { handle_event!(ev) }
                None => break SessionEnd::TransportLost,
            },

            _ = rec_interval.tick(), if cv_rec.is_some() || tr_rec.is_some() || vc_rec.is_some() => {
                // recorder died on its own (e.g. a suspended monitor source
                // erroring at first open) — surface it instead of a phantom
                // "recording" that never advances
                if let Some(child) = vc_rec.as_mut() {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        vc_rec = None;
                        ui.upgrade_in_event_loop(|ui| {
                            ui.set_vc_recording(false);
                            ui.set_vc_status("⚠ recorder exited — try again or check the source".into());
                        }).ok();
                    }
                }
                if vc_rec.is_some() {
                    vc_elapsed += 1;
                    if vc_elapsed >= VC_REC_MAX {
                        // engine caps conversion sources — stop and keep the clip
                        if let Some(mut child) = vc_rec.take() {
                            stop_pw_record(&mut child).await;
                            vc_source = Some(vc_wav.clone());
                            let label = format!("{} · stopped at the 3:00 cap", recorded_label(&vc_wav));
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_vc_recording(false);
                                ui.set_vc_has_source(true);
                                ui.set_vc_source_label(label.into());
                                ui.set_vc_armed_id("".into());
                                ui.set_vc_armed_saved(false);
                                ui.set_vc_status("".into());
                            }).ok();
                            match proxy.transcribe_file(&vc_wav).await {
                                Ok(rid) => {
                                    pending_vc_tr = rid;
                                    vc_tr_clip.clear(); // scratch source — nothing to backfill
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_vc_transcribing(true);
                                        ui.set_vc_transcript("".into());
                                    }).ok();
                                }
                                Err(e) => tracing::error!("vc transcribe failed: {e}"),
                            }
                        }
                    } else {
                        let e = vc_elapsed;
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_vc_status(format!("● recording {}:{:02} / 3:00", e / 60, e % 60).into());
                        }).ok();
                    }
                }
                if tr_rec.is_some() {
                    tr_elapsed += 1;
                    if tr_elapsed >= TR_REC_MAX {
                        // safety cap — stop and transcribe what we have
                        if let Some(mut child) = tr_rec.take() {
                            stop_pw_record(&mut child).await;
                            match proxy.transcribe_file(&tr_wav).await {
                                Ok(id) => pending_tr = id,
                                Err(e) => tracing::error!("transcribe failed: {e}"),
                            }
                            tr_source = tr_wav.clone();
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_recording(false);
                                ui.set_tr_busy(true);
                                ui.set_tr_has_source(true);
                                ui.set_tr_status("transcribing…".into());
                            }).ok();
                        }
                    } else {
                        let e = tr_elapsed;
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_tr_status(format!("● recording {}:{:02}", e / 60, e % 60).into());
                        }).ok();
                    }
                }
                if cv_rec.is_none() { continue; }
                rec_elapsed += 1;
                if rec_elapsed >= REC_MAX {
                    // hit the cap — auto-stop and keep the clip
                    if let Some(mut child) = cv_rec.take() {
                        stop_pw_record(&mut child).await;
                        cv_sample = Some(cv_wav.clone());
                        let label = recorded_label(&cv_wav);
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_cv_recording(false);
                            ui.set_cv_sample_label(label.into());
                        }).ok();
                    }
                } else {
                    let e = rec_elapsed;
                    ui.upgrade_in_event_loop(move |ui| {
                        ui.set_cv_sample_label(format!("● recording… {e}s / 30s").into());
                    }).ok();
                }
            }
            cmd = rx.recv() => match cmd {
                Some(Cmd::Generate { text, voice }) => {
                    match proxy.speak(&text, &voice).await {
                        Ok(id) => current_gen = id,
                        Err(e) => tracing::error!("speak failed: {e}"),
                    }
                }
                Some(Cmd::Cancel { gen_id }) => {
                    let id = if gen_id == 0 { current_gen } else { gen_id };
                    if id != 0 { proxy.cancel(id).await.ok(); }
                }
                Some(Cmd::TrimShow { ctx }) => {
                    let (path, title) = match ctx.as_str() {
                        "vc" => (vc_source.clone().unwrap_or_default(), "conversion source"),
                        "cv" => (cv_sample.clone().unwrap_or_default(), "voice sample"),
                        "tr" => (tr_source.clone(), "transcription source"),
                        _ => (String::new(), ""),
                    };
                    if !path.is_empty() {
                        if let Ok(j) = proxy.file_envelope(&path).await {
                            if let Some((bars, dur)) = parse_envelope(&j) {
                                trim_ctx = ctx;
                                trim_path = path;
                                trim_hid.clear();
                                trim_dur = dur;
                                open_trim_modal(&ui, title.to_string(), bars, dur);
                            }
                        }
                    }
                }
                Some(Cmd::TrimShowHist { hid }) => {
                    let path = proxy.history_audio_path(&hid).await.unwrap_or_default();
                    if !path.is_empty() {
                        if let Ok(j) = proxy.file_envelope(&path).await {
                            if let Some((bars, dur)) = parse_envelope(&j) {
                                trim_ctx = "hist".into();
                                trim_hid = hid;
                                trim_path = path;
                                trim_dur = dur;
                                open_trim_modal(&ui, "history clip".to_string(), bars, dur);
                            }
                        }
                    }
                }
                Some(Cmd::TrimPreview { start, end }) => {
                    if trim_gen != 0 { proxy.cancel(trim_gen).await.ok(); }
                    let gid = if trim_ctx == "hist" {
                        proxy.play_history_at(&trim_hid, start).await.unwrap_or(0)
                    } else {
                        proxy.play_file_at(&trim_path, "trim preview", start).await.unwrap_or(0)
                    };
                    trim_gen = gid;
                    trim_end_pct = end;
                    if gid != 0 {
                        ui.upgrade_in_event_loop(|ui| ui.set_trim_playing(true)).ok();
                    }
                }
                Some(Cmd::TrimPreviewStop) => {
                    if trim_gen != 0 {
                        proxy.cancel(trim_gen).await.ok();
                        trim_gen = 0;
                    }
                    ui.upgrade_in_event_loop(|ui| ui.set_trim_playing(false)).ok();
                }
                Some(Cmd::TrimApply { start, end }) => {
                    if trim_gen != 0 {
                        proxy.cancel(trim_gen).await.ok();
                        trim_gen = 0;
                    }
                    let (start_s, end_s) = (start * trim_dur, end * trim_dur);
                    if trim_ctx == "hist" {
                        let ok = proxy
                            .trim_history_clip(&trim_hid, start_s, end_s)
                            .await
                            .unwrap_or(false);
                        let refreshed = if ok {
                            proxy.list_history().await.ok().map(|j| build_history(&j))
                        } else {
                            None
                        };
                        let hid = trim_hid.clone();
                        ui.upgrade_in_event_loop(move |ui| {
                            if let Some(items) = refreshed {
                                set_history_model(&ui, items);
                                // the bar's waveform/duration are stale for
                                // this clip — hide it; replay reopens fresh
                                if ui.get_player_id().as_str() == hid {
                                    ui.set_player_active(false);
                                }
                            }
                            ui.set_trim_open(false);
                        }).ok();
                    } else {
                        match proxy.trim_audio(&trim_path, start_s, end_s).await {
                            Ok(p) if !p.is_empty() => match trim_ctx.as_str() {
                                "vc" => {
                                    vc_source = Some(p.clone());
                                    // a saved armed clip was rewritten in place:
                                    // clear its cache and route the fresh
                                    // transcript back into it (backfill path)
                                    vc_tr_clip = vc_clips_data
                                        .iter()
                                        .find(|(_, _, cpath, _)| *cpath == p)
                                        .map(|(cid, _, _, _)| cid.clone())
                                        .unwrap_or_default();
                                    if !vc_tr_clip.is_empty() {
                                        proxy.set_source_clip_transcript(&vc_tr_clip, "").await.ok();
                                        if let Some(row) = vc_clips_data
                                            .iter_mut()
                                            .find(|(cid, _, _, _)| *cid == vc_tr_clip)
                                        {
                                            row.3.clear();
                                        }
                                    }
                                    match proxy.transcribe_file(&p).await {
                                        Ok(id) if id != 0 => pending_vc_tr = id,
                                        _ => vc_tr_clip.clear(),
                                    }
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_vc_transcript("".into());
                                        ui.set_vc_transcribing(true);
                                        ui.set_trim_open(false);
                                    }).ok();
                                }
                                "cv" => {
                                    cv_sample = Some(p.clone());
                                    let label = recorded_label(&p);
                                    ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_cv_sample_label(label.into());
                                        ui.set_cv_transcribing(true);
                                        ui.set_trim_open(false);
                                    }).ok();
                                    // same inline transcribe the modal's button runs
                                    let result = proxy.transcribe(&p).await;
                                    ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_cv_transcribing(false);
                                        if let Ok(text) = result {
                                            ui.set_cv_transcript(text.into());
                                        }
                                    }).ok();
                                }
                                "tr" => {
                                    tr_source = p.clone();
                                    match proxy.transcribe_file(&p).await {
                                        Ok(id) => {
                                            pending_tr = id;
                                            ui.upgrade_in_event_loop(|ui| {
                                                ui.set_tr_text("".into());
                                                ui.set_tr_busy(true);
                                                ui.set_tr_status("transcribing…".into());
                                                ui.set_trim_open(false);
                                            }).ok();
                                        }
                                        Err(e) => {
                                            tracing::error!("post-trim transcribe failed: {e}");
                                            ui.upgrade_in_event_loop(|ui| ui.set_trim_open(false)).ok();
                                        }
                                    }
                                }
                                _ => {
                                    ui.upgrade_in_event_loop(|ui| ui.set_trim_open(false)).ok();
                                }
                            },
                            _ => {
                                ui.upgrade_in_event_loop(|ui| ui.set_trim_open(false)).ok();
                            }
                        }
                    }
                }
                Some(Cmd::Play { id }) => {
                    match proxy.play_history(&id).await {
                        Ok(gid) if gid != 0 => current_gen = gid,
                        Ok(_) => {}
                        Err(e) => tracing::error!("play_history failed: {e}"),
                    }
                }
                Some(Cmd::Star { id, on }) => {
                    if let Err(e) = proxy.star_history(&id, on).await {
                        tracing::error!("star_history failed: {e}");
                    }
                    if lib_loaded {
                        let (rows, voices) = lib_load(&proxy).await;
                        lib_rows = rows;
                        lib_voices = voices;
                        lib_apply(&ui, &lib_rows, &lib_voices, &lib_filters);
                    }
                }
                Some(Cmd::Delete { id }) => {
                    if let Err(e) = proxy.delete_history(&id).await {
                        tracing::error!("delete_history failed: {e}");
                    }
                    if let Ok(json) = proxy.list_history().await {
                        let items = build_history(&json);
                        ui.upgrade_in_event_loop(move |ui| set_history_model(&ui, items)).ok();
                    }
                    if lib_loaded {
                        let (rows, voices) = lib_load(&proxy).await;
                        lib_rows = rows;
                        lib_voices = voices;
                        lib_apply(&ui, &lib_rows, &lib_voices, &lib_filters);
                    }
                }
                Some(Cmd::Regenerate { id, is_vc, is_music }) => {
                    match proxy.regenerate_history(&id).await {
                        Ok(gid) if gid != 0 => {
                            current_gen = gid;
                            if is_vc {
                                pending_vc = gid;
                                pending_vc_music = is_music;
                            }
                        }
                        Ok(_) => {
                            // engine refused — for a conversion that means the
                            // exact source take no longer exists
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_generating(false);
                                ui.set_synthesizing(false);
                                if is_vc {
                                    ui.set_vc_busy(false);
                                    ui.set_vc_status("".into());
                                    ui.set_vc_error(
                                        "can't regenerate — the source take was overwritten or deleted; re-arm a source and convert again".into(),
                                    );
                                }
                            }).ok();
                        }
                        Err(e) => {
                            tracing::error!("regenerate_history failed: {e}");
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_generating(false);
                                ui.set_synthesizing(false);
                                if is_vc { ui.set_vc_busy(false); ui.set_vc_status("".into()); }
                            }).ok();
                        }
                    }
                }
                Some(Cmd::Pause) => { proxy.pause_playback().await.ok(); }
                Some(Cmd::Resume) => { proxy.resume_playback().await.ok(); }
                Some(Cmd::Seek { id, pct }) => {
                    if playing {
                        proxy.seek_playback(pct).await.ok();
                    } else {
                        // not playing — start from the clicked position
                        match proxy.play_history_at(&id, pct).await {
                            Ok(gid) if gid != 0 => current_gen = gid,
                            _ => {}
                        }
                    }
                }
                Some(Cmd::ExportAudio { id }) => {
                    let src = proxy.history_audio_path(&id).await.unwrap_or_default();
                    if src.is_empty() {
                        tracing::error!("export audio: no source for {id}");
                    } else if let Some(handle) = export_dialog(&cfg.export_dir)
                        .set_file_name("syrinx-clip.wav")
                        .add_filter("WAV audio", &["wav"])
                        .save_file()
                        .await
                    {
                        let dest = handle.path().to_path_buf();
                        match std::fs::copy(&src, &dest) {
                            Ok(_) => tracing::info!("exported audio -> {}", dest.display()),
                            Err(e) => tracing::error!("export audio copy failed: {e}"),
                        }
                    }
                }
                Some(Cmd::ExportPackage { id }) => {
                    if let Some(handle) = export_dialog(&cfg.export_dir)
                        .set_file_name("syrinx-clip.zip")
                        .add_filter("Zip package", &["zip"])
                        .save_file()
                        .await
                    {
                        let dest = handle.path().to_string_lossy().to_string();
                        match proxy.export_package(&id, &dest).await {
                            Ok(_) => tracing::info!("exported package -> {dest}"),
                            Err(e) => tracing::error!("export package failed: {e}"),
                        }
                    }
                }
                Some(Cmd::CvStartRecord { system }) => {
                    // System audio = passive tap of the default sink's monitor.
                    // Settings-tab device choices win; "" = system default
                    let target = if system {
                        if cfg.monitor_device.is_empty() {
                            default_monitor().await
                        } else {
                            Some(cfg.monitor_device.clone())
                        }
                    } else if cfg.mic_device.is_empty() {
                        None
                    } else {
                        Some(cfg.mic_device.clone())
                    };
                    match start_pw_record(&cv_wav, target.as_deref()).await {
                        Ok(child) => {
                            cv_rec = Some(child);
                            rec_elapsed = 0;
                            rec_interval.reset();  // first tick a full second out
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_cv_recording(true);
                                ui.set_cv_sample_label("● recording… 0s / 30s".into());
                            }).ok();
                        }
                        Err(e) => tracing::error!("pw-record failed: {e}"),
                    }
                }
                Some(Cmd::CvStopRecord) => {
                    if let Some(mut child) = cv_rec.take() {
                        stop_pw_record(&mut child).await;
                        cv_sample = Some(cv_wav.clone());
                        let label = recorded_label(&cv_wav);
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_cv_recording(false);
                            ui.set_cv_sample_label(label.into());
                        }).ok();
                    }
                }
                Some(Cmd::CvPickFile) => {
                    if let Some(handle) = rfd::AsyncFileDialog::new()
                        .add_filter("Audio", &["wav", "flac", "ogg", "mp3", "m4a", "opus"])
                        .pick_file()
                        .await
                    {
                        cv_sample = Some(handle.path().to_string_lossy().to_string());
                        let label = handle.file_name();
                        ui.upgrade_in_event_loop(move |ui| ui.set_cv_sample_label(label.into())).ok();
                    }
                }
                Some(Cmd::CvTranscribe) => {
                    if let Some(path) = cv_sample.clone() {
                        ui.upgrade_in_event_loop(|ui| {
                            ui.set_cv_error("".into());
                            ui.set_cv_transcribing(true);
                        }).ok();
                        let result = proxy.transcribe(&path).await;
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_cv_transcribing(false);
                            match result {
                                Ok(text) => ui.set_cv_transcript(text.into()),
                                Err(e) => ui.set_cv_error(format!("Transcribe failed: {e}").into()),
                            }
                        }).ok();
                    } else {
                        ui.upgrade_in_event_loop(|ui| {
                            ui.set_cv_error("Record or choose a reference clip first.".into());
                        }).ok();
                    }
                }
                Some(Cmd::CvCreate { name, desc, personality, language, transcript, model_index }) => {
                    // "Follow active model" (index 0) stores "", else the engine
                    // of the picked voice model — the same field the composer
                    // dropdown pins.
                    let default_engine = if model_index == 0 {
                        String::new()
                    } else {
                        voice_models
                            .get(model_index - 1)
                            .map(|(_, e)| e.clone())
                            .unwrap_or_default()
                    };
                    ui.upgrade_in_event_loop(|ui| ui.set_cv_error("".into())).ok();
                    if let Some(pid) = cv_edit.clone() {
                        // edit mode: patch metadata + optionally replace audio
                        let patch = serde_json::json!({
                            "name": name, "description": desc,
                            "personality": personality, "language": language,
                            "default_engine": default_engine,
                        }).to_string();
                        match proxy.update_profile(&pid, &patch).await {
                            Ok(_) => {
                                let mut sample_err = String::new();
                                if let Some((path, amode, asx, asy, asw, ash)) = cv_avatar.take() {
                                    proxy.set_profile_avatar(&pid, &path, &amode, asx, asy, asw, ash).await.ok();
                                }
                                if let Some(sample) = cv_sample.take() {
                                    // a new capture replaces the existing samples
                                    if let Ok(pj) = proxy.get_profile(&pid).await {
                                        let p: serde_json::Value =
                                            serde_json::from_str(&pj).unwrap_or_default();
                                        for s in p.get("samples").and_then(|v| v.as_array()).into_iter().flatten() {
                                            if let Some(sid) = s.get("id").and_then(|v| v.as_str()) {
                                                proxy.delete_sample(sid).await.ok();
                                            }
                                        }
                                    }
                                    if let Err(e) = proxy.add_sample(&pid, &sample, &transcript).await {
                                        tracing::error!("replace sample failed: {e}");
                                        sample_err = format!(
                                            "Replacing the sample failed: {} — record a new clip and save again.",
                                            profile_err_msg(&e)
                                        );
                                    }
                                } else if transcript.trim() != cv_edit_transcript.trim()
                                    && !transcript.trim().is_empty()
                                {
                                    // transcript-only correction on the existing sample
                                    if let Ok(pj) = proxy.get_profile(&pid).await {
                                        let p: serde_json::Value =
                                            serde_json::from_str(&pj).unwrap_or_default();
                                        if let Some(sid) = p
                                            .get("samples")
                                            .and_then(|v| v.as_array())
                                            .and_then(|a| a.first())
                                            .and_then(|s| s.get("id"))
                                            .and_then(|v| v.as_str())
                                        {
                                            proxy.update_sample_text(&pid, sid, &transcript).await.ok();
                                        }
                                    }
                                }
                                refresh_grid(&ui, &proxy, &mut avatar_cache).await;
                                refresh_voices_table(&ui, &proxy, &mut avatar_cache, &mut voices_all).await;
                                if vp_inspected == pid {
                                    // saved edits land in the inspector immediately
                                    inspect_profile(&ui, &proxy, &voices_all, &pid).await;
                                }
                                if sample_err.is_empty() {
                                    cv_edit = None;
                                    let pid2 = pid.clone();
                                    let name2 = name.clone();
                                    let hp = !personality.trim().is_empty();
                                    ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_cv_open(false);
                                        ui.set_cv_edit_id("".into());
                                        ui.set_cv_name("".into());
                                        ui.set_cv_desc("".into());
                                        ui.set_cv_personality("".into());
                                        ui.set_cv_transcript("".into());
                                        ui.set_cv_sample_label("".into());
                                        ui.set_cv_model_index(0);
                                        ui.set_cv_has_avatar(false);
                                        if ui.get_selected_voice().as_str() == pid2 {
                                            ui.set_selected_voice_name(name2.into());
                                            ui.set_selected_has_personality(hp);
                                        }
                                    }).ok();
                                } else {
                                    // keep the modal open in edit mode so a re-record can retry
                                    ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_cv_sample_label("".into());
                                        ui.set_cv_error(sample_err.into());
                                    }).ok();
                                }
                            }
                            Err(e) => {
                                tracing::error!("edit voice failed: {e}");
                                let msg = profile_err_msg(&e);
                                ui.upgrade_in_event_loop(move |ui| ui.set_cv_error(msg.into())).ok();
                            }
                        }
                    } else if name.trim().is_empty() || cv_sample.is_none() {
                        ui.upgrade_in_event_loop(|ui| {
                            ui.set_cv_error("A name and a reference sample are both required.".into());
                        }).ok();
                    } else {
                        ui.upgrade_in_event_loop(|ui| ui.set_cv_creating(true)).ok();
                        let sample = cv_sample.clone().unwrap();
                        let spec = serde_json::json!({
                            "name": name, "voice_type": "cloned", "language": language,
                            "description": desc, "personality": personality,
                            "default_engine": default_engine,
                        }).to_string();
                        let outcome = async {
                            let pid = proxy.create_profile(&spec).await?;
                            if let Err(e) = proxy.add_sample(&pid, &sample, &transcript).await {
                                // roll back so a failed create leaves no sample-less ghost
                                proxy.delete_profile(&pid).await.ok();
                                return Err(e);
                            }
                            Ok::<String, EngineError>(pid)
                        }.await;
                        match outcome {
                            Ok(pid) => {
                                if let Some((path, amode, asx, asy, asw, ash)) = cv_avatar.take() {
                                    proxy.set_profile_avatar(&pid, &path, &amode, asx, asy, asw, ash).await.ok();
                                }
                                let raw = proxy.list_voices().await.unwrap_or_default();
                                let pj = proxy.list_profiles().await.unwrap_or_else(|_| "[]".into());
                                let GridData { grid, kokoro_names, kokoro_ids, .. } = build_grid(raw, &pj);
                                kokoro_all = kokoro_ids
                                    .iter()
                                    .zip(kokoro_names.iter())
                                    .map(|(i, n)| (i.to_string(), n.to_string()))
                                    .collect();
                                let grid = bake_grid(&mut avatar_cache, grid);
                                cv_sample = None;
                                ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_cv_creating(false);
                                    ui.set_cv_open(false);
                                    ui.set_cv_name("".into());
                                    ui.set_cv_desc("".into());
                                    ui.set_cv_personality("".into());
                                    ui.set_cv_transcript("".into());
                                    ui.set_cv_sample_label("".into());
                                    ui.set_cv_model_index(0);
                                    ui.set_cv_has_avatar(false);
                                    ui.set_kokoro_names(ModelRc::from(Rc::new(VecModel::from(kokoro_names))));
                                    ui.set_kokoro_ids(ModelRc::from(Rc::new(VecModel::from(kokoro_ids))));
                                    ui.set_voices(ModelRc::from(Rc::new(VecModel::from(to_voice_items(grid)))));
                                }).ok();
                                refresh_voices_table(&ui, &proxy, &mut avatar_cache, &mut voices_all).await;
                            }
                            Err(e) => {
                                tracing::error!("create voice failed: {e}");
                                let msg = profile_err_msg(&e);
                                ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_cv_creating(false);
                                    ui.set_cv_error(msg.into());
                                }).ok();
                            }
                        }
                    }
                }
                Some(Cmd::ModelsLoad) => {
                    let r = refresh_models(&ui, &proxy).await;
                    voice_models = r.0;
                    active_engine = r.1;
                }
                Some(Cmd::DownloadModel { id }) => {
                    match proxy.download_model(&id).await {
                        Ok(true) => set_model_progress(&ui, id, 0.0, true),
                        _ => tracing::error!("download_model failed: {id}"),
                    }
                }
                Some(Cmd::DeleteModel { id }) => {
                    proxy.delete_model(&id).await.ok();
                    let r = refresh_models(&ui, &proxy).await;
                    voice_models = r.0;
                    active_engine = r.1;
                }
                Some(Cmd::ActivateModel { id }) => {
                    proxy.set_active_model(&id).await.ok();
                    let r = refresh_models(&ui, &proxy).await;
                    voice_models = r.0;
                    active_engine = r.1;
                }
                Some(Cmd::Compose { voice_id, prompt }) => {
                    match proxy.compose_profile(&voice_id, &prompt).await {
                        Ok(rid) if rid != 0 => pending_llm = rid,
                        _ => { ui.upgrade_in_event_loop(|ui| ui.set_llm_busy(false)).ok(); }
                    }
                }
                Some(Cmd::Rewrite { voice_id, text }) => {
                    match proxy.rewrite_profile(&voice_id, &text).await {
                        Ok(rid) if rid != 0 => pending_llm = rid,
                        _ => { ui.upgrade_in_event_loop(|ui| ui.set_llm_busy(false)).ok(); }
                    }
                }
                Some(Cmd::CvCancel) => {
                    if let Some(mut child) = cv_rec.take() {
                        stop_pw_record(&mut child).await;
                    }
                    cv_sample = None;
                    cv_edit = None;
                    cv_edit_transcript.clear();
                    cv_avatar = None;
                    ui.upgrade_in_event_loop(|ui| {
                        ui.set_cv_recording(false);
                        ui.set_cv_sample_label("".into());
                        ui.set_cv_name("".into());
                        ui.set_cv_desc("".into());
                        ui.set_cv_personality("".into());
                        ui.set_cv_transcript("".into());
                        ui.set_cv_edit_id("".into());
                        ui.set_cv_model_index(0);
                        ui.set_cv_has_avatar(false);
                    }).ok();
                }
                Some(Cmd::GenerateInCharacter { text, voice }) => {
                    match proxy.rewrite_profile(&voice, &text).await {
                        Ok(rid) if rid != 0 => {
                            pending_llm = rid;
                            speak_after_llm = Some(voice);
                        }
                        _ => {
                            // no personality / LLM unavailable — speak the raw text
                            ui.upgrade_in_event_loop(|ui| ui.set_llm_busy(false)).ok();
                            match proxy.speak(&text, &voice).await {
                                Ok(id) => current_gen = id,
                                Err(e) => tracing::error!("speak failed: {e}"),
                            }
                        }
                    }
                }
                Some(Cmd::SelectVoice { id }) => {
                    let (engine, code) = if id.starts_with("builtin:") {
                        // builtin:<engine>:<voice> — kokoro or an extra preset
                        // engine like qwen_custom_voice
                        let engine = id.split(':').nth(1).unwrap_or("kokoro").to_string();
                        let code = if engine == "kokoro" {
                            kokoro_lang_code(&id).to_string()
                        } else {
                            "en".to_string()
                        };
                        (engine, code)
                    } else if let Ok(pj) = proxy.get_profile(&id).await {
                        let p: serde_json::Value = serde_json::from_str(&pj).unwrap_or_default();
                        let de = p.get("default_engine").and_then(|v| v.as_str()).unwrap_or("");
                        let engine = if de.is_empty() { active_engine.clone() } else { de.to_string() };
                        let code = p.get("language").and_then(|v| v.as_str()).unwrap_or("en").to_string();
                        (engine, code)
                    } else {
                        ("kokoro".to_string(), "en".to_string())
                    };
                    lang_codes = update_composer_langs(&ui, &engine, &code);
                    // the composer's model dropdown mirrors this voice's engine
                    // (profile pin, else the active model)
                    let eidx = voice_models
                        .iter()
                        .position(|(_, e)| *e == engine)
                        .unwrap_or(0) as i32;
                    ui.upgrade_in_event_loop(move |ui| ui.set_composer_engine_index(eidx)).ok();
                }
                Some(Cmd::PickLanguage { voice, index }) => {
                    if let Some(code) = lang_codes.get(index) {
                        if voice.starts_with("builtin:kokoro:") {
                            // filter the Kokoro Defaults dropdown to this language
                            let prefixes = kokoro_prefixes(code);
                            let mut filtered: Vec<(String, String)> = kokoro_all
                                .iter()
                                .filter(|(id, _)| {
                                    id.rsplit(':')
                                        .next()
                                        .and_then(|v| v.chars().next())
                                        .map(|c| prefixes.contains(&c))
                                        .unwrap_or(false)
                                })
                                .cloned()
                                .collect();
                            if filtered.is_empty() {
                                filtered = kokoro_all.clone();
                            }
                            let sel_pos = filtered.iter().position(|(id, _)| *id == voice);
                            let idx = sel_pos.unwrap_or(0) as i32;
                            let need_switch = sel_pos.is_none();
                            let (nid, nname) = filtered[idx as usize].clone();
                            let names: Vec<SharedString> =
                                filtered.iter().map(|(_, n)| n.as_str().into()).collect();
                            let ids: Vec<SharedString> =
                                filtered.iter().map(|(i, _)| i.as_str().into()).collect();
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_kokoro_names(ModelRc::from(Rc::new(VecModel::from(names))));
                                ui.set_kokoro_ids(ModelRc::from(Rc::new(VecModel::from(ids))));
                                ui.set_kokoro_index(idx);
                                if need_switch {
                                    // old selection doesn't speak this language —
                                    // jump to the first preset that does
                                    ui.set_selected_voice(nid.as_str().into());
                                    ui.set_selected_voice_name(nname.as_str().into());
                                    ui.set_selected_has_personality(false);
                                    ui.set_kokoro_active(true);
                                }
                            })
                            .ok();
                        } else if !voice.is_empty() {
                            // persisted per cloned profile
                            let patch = serde_json::json!({"language": code}).to_string();
                            proxy.update_profile(&voice, &patch).await.ok();
                            refresh_grid(&ui, &proxy, &mut avatar_cache).await;
                            refresh_voices_table(&ui, &proxy, &mut avatar_cache, &mut voices_all).await;
                        }
                    }
                }
                Some(Cmd::PickEngine { voice, index }) => {
                    if let Some((mid, meng)) = voice_models.get(index).cloned() {
                        if !voice.is_empty() && !voice.starts_with("builtin:") {
                            // cloned profile selected: the dropdown pins THIS
                            // voice's engine (same field as the edit modal)
                            let patch = serde_json::json!({"default_engine": meng}).to_string();
                            proxy.update_profile(&voice, &patch).await.ok();
                            if let Ok(pj) = proxy.get_profile(&voice).await {
                                let p: serde_json::Value = serde_json::from_str(&pj).unwrap_or_default();
                                let code = p.get("language").and_then(|v| v.as_str()).unwrap_or("en").to_string();
                                lang_codes = update_composer_langs(&ui, &meng, &code);
                            }
                        } else {
                            // preset selected: switch the global active voice model
                            proxy.set_active_model(&mid).await.ok();
                            let r = refresh_models(&ui, &proxy).await;
                            voice_models = r.0;
                            active_engine = r.1;
                        }
                    }
                }
                Some(Cmd::ToggleLoop { on }) => { loop_on = on; }
                Some(Cmd::SetVol { v }) => { proxy.set_volume(v).await.ok(); }
                Some(Cmd::PickEffect { index }) => {
                    if let Some(pid) = effect_ids.get(index) {
                        proxy.set_effect(pid).await.ok();
                    }
                }
                Some(Cmd::PickStyle { index }) => {
                    if let Some((_, instruct)) = STYLES.get(index) {
                        proxy.set_style(instruct).await.ok();
                    }
                }
                Some(Cmd::ApplyFx { hid, index }) => {
                    if let Some(pid) = effect_ids.get(index).filter(|p| !p.is_empty()) {
                        match proxy.apply_history_effects(&hid, pid).await {
                            Ok(new_id) if !new_id.is_empty() => {
                                if let Ok(j) = proxy.list_history().await {
                                    let items = build_history(&j);
                                    ui.upgrade_in_event_loop(move |ui| set_history_model(&ui, items)).ok();
                                }
                            }
                            Ok(_) => tracing::error!("apply effects: engine returned no clip"),
                            Err(e) => tracing::error!("apply effects failed: {e}"),
                        }
                    }
                }
                Some(Cmd::ExportVoice { id, name }) => {
                    let safe: String = name
                        .to_lowercase()
                        .chars()
                        .map(|c| if c.is_alphanumeric() { c } else { '-' })
                        .collect();
                    if let Some(handle) = export_dialog(&cfg.export_dir)
                        .set_file_name(format!("{}.syrinx-voice.zip", safe.trim_matches('-')))
                        .add_filter("Syrinx voice package", &["zip"])
                        .save_file()
                        .await
                    {
                        let dest = handle.path().to_string_lossy().to_string();
                        match proxy.export_profile(&id, &dest).await {
                            Ok(_) => tracing::info!("exported voice -> {dest}"),
                            Err(e) => tracing::error!("export voice failed: {e}"),
                        }
                    }
                }
                Some(Cmd::EditVoice { id }) => {
                    if let Ok(pj) = proxy.get_profile(&id).await {
                        let p: serde_json::Value = serde_json::from_str(&pj).unwrap_or_default();
                        let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let (name, desc, pers) = (s("name"), s("description"), s("personality"));
                        let lang = {
                            let l = s("language");
                            if l.is_empty() { "en".to_string() } else { l }
                        };
                        let lang_idx = ["en", "ja", "zh", "de", "es", "fr", "it", "pt"]
                            .iter()
                            .position(|c| *c == lang)
                            .unwrap_or(0) as i32;
                        // current sample transcript (edit shows/corrects it)
                        let transcript = p
                            .get("samples")
                            .and_then(|v| v.as_array())
                            .and_then(|a| a.first())
                            .and_then(|smp| smp.get("reference_text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        cv_edit_transcript = transcript.clone();
                        // pinned engine → its dropdown slot; "" → Follow active
                        let de = s("default_engine");
                        let model_idx = if de.is_empty() {
                            0
                        } else {
                            voice_models
                                .iter()
                                .position(|(_, e)| *e == de)
                                .map(|i| i as i32 + 1)
                                .unwrap_or(0)
                        };
                        // existing avatar for the modal preview (baked thumbnail)
                        let av_mode = {
                            let m = s("avatar_mode");
                            if m.is_empty() { "circle".to_string() } else { m }
                        };
                        let iv = |k: &str| p.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        let av_baked = bake_avatar_rgba(
                            &mut avatar_cache,
                            &s("avatar_path"),
                            iv("avatar_sx"),
                            iv("avatar_sy"),
                            iv("avatar_side"),
                            iv("avatar_sh"),
                        );
                        cv_edit = Some(id.clone());
                        cv_sample = None;
                        cv_avatar = None;
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_cv_error("".into());
                            ui.set_cv_name(name.into());
                            ui.set_cv_desc(desc.into());
                            ui.set_cv_personality(pers.into());
                            ui.set_cv_language(lang.into());
                            ui.set_cv_lang_index(lang_idx);
                            ui.set_cv_transcript(transcript.into());
                            ui.set_cv_model_index(model_idx);
                            ui.set_cv_sample_label("".into());
                            match &av_baked {
                                Some(b) => {
                                    ui.set_cv_avatar(rgba_to_image(b));
                                    ui.set_cv_avatar_mode(av_mode.into());
                                    ui.set_cv_has_avatar(true);
                                }
                                None => ui.set_cv_has_avatar(false),
                            }
                            ui.set_cv_edit_id(id.into());
                            ui.set_cv_open(true);
                        }).ok();
                    }
                }
                Some(Cmd::DeleteVoice { id }) => {
                    match proxy.delete_profile(&id).await {
                        Ok(_) => {
                            if vp_inspected == id {
                                vp_inspected.clear();
                            }
                            refresh_grid(&ui, &proxy, &mut avatar_cache).await;
                            refresh_voices_table(&ui, &proxy, &mut avatar_cache, &mut voices_all).await;
                            ui.upgrade_in_event_loop(move |ui| {
                                if ui.get_selected_voice().as_str() == id {
                                    // fall back to the first bundled preset
                                    if let Some(first) = ui.get_kokoro_ids().row_data(0) {
                                        ui.set_selected_voice(first.clone());
                                        ui.set_kokoro_active(true);
                                        ui.set_selected_has_personality(false);
                                        ui.set_selected_voice_name(voice_name(&ui, first.as_str()).into());
                                    }
                                }
                            }).ok();
                        }
                        Err(e) => tracing::error!("delete voice failed: {e}"),
                    }
                }
                Some(Cmd::TrToggleRecord { system }) => {
                    if let Some(mut child) = tr_rec.take() {
                        // stop → transcribe (unless the capture came out silent)
                        stop_pw_record(&mut child).await;
                        if wav_rms(&tr_wav).map(|r| r < 0.006).unwrap_or(true) {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_recording(false);
                                ui.set_tr_status("⚠ capture was silent — check the input device".into());
                            }).ok();
                        } else {
                            match proxy.transcribe_file(&tr_wav).await {
                                Ok(id) => {
                                    pending_tr = id;
                                    tr_source = tr_wav.clone();
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_tr_recording(false);
                                        ui.set_tr_busy(true);
                                        ui.set_tr_has_source(true);
                                        ui.set_tr_status("transcribing…".into());
                                    }).ok();
                                }
                                Err(e) => {
                                    tracing::error!("transcribe failed: {e}");
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_tr_recording(false);
                                        ui.set_tr_status("engine unavailable".into());
                                    }).ok();
                                }
                            }
                        }
                    } else {
                        // Settings-tab device choices win; "" = system default
                        let device = if system {
                            if cfg.monitor_device.is_empty() {
                                default_monitor().await
                            } else {
                                Some(cfg.monitor_device.clone())
                            }
                        } else if cfg.mic_device.is_empty() {
                            None
                        } else {
                            Some(cfg.mic_device.clone())
                        };
                        if system && device.is_none() {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_status("no default sink monitor found".into());
                            }).ok();
                        } else {
                            match start_pw_record(&tr_wav, device.as_deref()).await {
                                Ok(child) => {
                                    tr_rec = Some(child);
                                    tr_elapsed = 0;
                                    rec_interval.reset();  // first tick a full second out
                                    let mode = if system { "system" } else { "mic" };
                                    ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_tr_text("".into());
                                        ui.set_tr_capture_id("".into()); // fresh source = new capture
                                        ui.set_tr_rec_mode(mode.into());
                                        ui.set_tr_recording(true);
                                        ui.set_tr_status("● recording 0:00".into());
                                    }).ok();
                                }
                                Err(e) => tracing::error!("record failed: {e}"),
                            }
                        }
                    }
                }
                Some(Cmd::TrPickFile) => {
                    if let Some(handle) = rfd::AsyncFileDialog::new()
                        .add_filter("Audio", &["wav", "mp3", "flac", "ogg", "m4a", "opus", "webm"])
                        .pick_file()
                        .await
                    {
                        let path = handle.path().to_string_lossy().to_string();
                        match proxy.transcribe_file(&path).await {
                            Ok(id) => {
                                pending_tr = id;
                                tr_source = path.clone();
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_tr_text("".into());
                                    ui.set_tr_capture_id("".into()); // fresh source = new capture
                                    ui.set_tr_busy(true);
                                    ui.set_tr_has_source(true);
                                    ui.set_tr_status("transcribing…".into());
                                }).ok();
                            }
                            Err(e) => tracing::error!("transcribe failed: {e}"),
                        }
                    }
                }
                Some(Cmd::TrRefine { text }) => {
                    match proxy.refine_transcript(&text).await {
                        Ok(rid) if rid != 0 => pending_tr_refine = rid,
                        _ => {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_busy(false);
                                ui.set_tr_status("refine unavailable".into());
                            }).ok();
                        }
                    }
                }
                Some(Cmd::TrSaveCapture { id, text }) => {
                    // "" id = new row; otherwise replace the same entry in place
                    let saved = if id.is_empty() {
                        proxy.save_capture(&text).await.ok().filter(|s| !s.is_empty())
                    } else {
                        proxy.update_capture(&id, &text).await.ok().map(|()| id.clone())
                    };
                    match saved {
                        Some(cid) => {
                            let status = if id.is_empty() { "capture saved" } else { "capture updated" };
                            let items = build_captures(
                                &proxy.list_captures().await.unwrap_or_else(|_| "[]".into()),
                            );
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_tr_capture_id(cid.into());
                                ui.set_tr_status(status.into());
                                set_captures_model(&ui, items);
                            }).ok();
                        }
                        None => {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_status("save failed — engine unavailable".into());
                            }).ok();
                        }
                    }
                }
                Some(Cmd::TrDeleteCapture { id }) => {
                    match proxy.delete_capture(&id).await {
                        Ok(_) => {
                            let items = build_captures(
                                &proxy.list_captures().await.unwrap_or_else(|_| "[]".into()),
                            );
                            ui.upgrade_in_event_loop(move |ui| {
                                // the transcript stays in the box; it's just unsaved now
                                if ui.get_tr_capture_id().as_str() == id {
                                    ui.set_tr_capture_id("".into());
                                }
                                set_captures_model(&ui, items);
                            }).ok();
                        }
                        Err(e) => tracing::error!("delete capture failed: {e}"),
                    }
                }
                Some(Cmd::VcLoad) => {
                    // target dropdown = cloned profiles that have reference samples
                    let pj = proxy.list_profiles().await.unwrap_or_else(|_| "[]".into());
                    let profs: Vec<serde_json::Value> = serde_json::from_str(&pj).unwrap_or_default();
                    vc_voice_ids.clear();
                    let mut names: Vec<SharedString> = Vec::new();
                    for p in &profs {
                        let cloned = p.get("voice_type").and_then(|v| v.as_str()) == Some("cloned");
                        let samples = p.get("samples").and_then(|v| v.as_i64()).unwrap_or(0);
                        if cloned && samples > 0 {
                            if let (Some(id), Some(name)) = (
                                p.get("id").and_then(|v| v.as_str()),
                                p.get("name").and_then(|v| v.as_str()),
                            ) {
                                vc_voice_ids.push(id.to_string());
                                names.push(name.into());
                            }
                        }
                    }
                    let count = names.len() as i32;
                    ui.upgrade_in_event_loop(move |ui| {
                        if ui.get_vc_voice_index() >= count { ui.set_vc_voice_index(0); }
                        ui.set_vc_voice_names(ModelRc::from(Rc::new(VecModel::from(names))));
                    }).ok();
                    vc_clips_data = refresh_vc_clips(&ui, &proxy).await;
                }
                Some(Cmd::VcToggleRecord { system }) => {
                    if let Some(mut child) = vc_rec.take() {
                        // stop → arm the clip as the conversion source
                        stop_pw_record(&mut child).await;
                        if wav_rms(&vc_wav).map(|r| r < 0.006).unwrap_or(true) {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_vc_recording(false);
                                ui.set_vc_status("⚠ capture was silent — check the input device".into());
                            }).ok();
                        } else {
                            vc_source = Some(vc_wav.clone());
                            let e = vc_elapsed;
                            let label = format!("recorded clip · {}:{:02}", e / 60, e % 60);
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_vc_recording(false);
                                ui.set_vc_has_source(true);
                                ui.set_vc_source_label(label.into());
                                ui.set_vc_armed_id("".into());
                                ui.set_vc_armed_saved(false);
                                ui.set_vc_status("".into());
                            }).ok();
                            match proxy.transcribe_file(&vc_wav).await {
                                Ok(rid) => {
                                    pending_vc_tr = rid;
                                    vc_tr_clip.clear(); // scratch source — nothing to backfill
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_vc_transcribing(true);
                                        ui.set_vc_transcript("".into());
                                    }).ok();
                                }
                                Err(e) => tracing::error!("vc transcribe failed: {e}"),
                            }
                        }
                    } else {
                        // Settings-tab device choices win; "" = system default
                        let device = if system {
                            if cfg.monitor_device.is_empty() {
                                default_monitor().await
                            } else {
                                Some(cfg.monitor_device.clone())
                            }
                        } else if cfg.mic_device.is_empty() {
                            None
                        } else {
                            Some(cfg.mic_device.clone())
                        };
                        if system && device.is_none() {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_vc_status("no default sink monitor found".into());
                            }).ok();
                        } else {
                            match start_pw_record(&vc_wav, device.as_deref()).await {
                                Ok(child) => {
                                    vc_rec = Some(child);
                                    vc_elapsed = 0;
                                    pending_vc_tr = 0;  // a stale transcription no longer applies
                                    vc_tr_clip.clear();
                                    rec_interval.reset();  // first tick a full second out
                                    let mode = if system { "system" } else { "mic" };
                                    ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_vc_has_source(false);
                                        ui.set_vc_source_label("".into());
                                        ui.set_vc_error("".into());
                                        ui.set_vc_transcript("".into());
                                        ui.set_vc_transcribing(false);
                                        ui.set_vc_rec_mode(mode.into());
                                        ui.set_vc_recording(true);
                                        ui.set_vc_status("● recording 0:00 / 3:00".into());
                                    }).ok();
                                }
                                Err(e) => tracing::error!("record failed: {e}"),
                            }
                        }
                    }
                }
                Some(Cmd::VcPickFile) => {
                    if let Some(handle) = rfd::AsyncFileDialog::new()
                        .add_filter("Audio", &["wav", "mp3", "flac", "ogg", "m4a", "opus", "webm"])
                        .pick_file()
                        .await
                    {
                        let name = handle.file_name();
                        let path = handle.path().to_string_lossy().to_string();
                        vc_source = Some(path.clone());
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_vc_has_source(true);
                            ui.set_vc_source_label(name.into());
                            ui.set_vc_armed_id("".into());
                            ui.set_vc_armed_saved(false);
                            ui.set_vc_error("".into());
                            ui.set_vc_status("".into());
                        }).ok();
                        match proxy.transcribe_file(&path).await {
                            Ok(rid) => {
                                pending_vc_tr = rid;
                                vc_tr_clip.clear(); // scratch source — nothing to backfill
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_vc_transcribing(true);
                                    ui.set_vc_transcript("".into());
                                }).ok();
                            }
                            Err(e) => tracing::error!("vc transcribe failed: {e}"),
                        }
                    }
                }
                Some(Cmd::VcConvert { index, engine_index, label, transcript, mode, semitones }) => {
                    if let (Some(src), Some(pid)) =
                        (vc_source.clone(), vc_voice_ids.get(index).cloned())
                    {
                        let table = if mode == "music" { VC_MUSIC_ENGINE_IDS } else { VC_ENGINE_IDS };
                        let engine = table.get(engine_index).copied().unwrap_or("");
                        match proxy.convert_voice(&src, &pid, engine, &label, &transcript, &mode, semitones).await {
                            Ok(gid) if gid != 0 => {
                                pending_vc = gid;
                                // the rail's ■ and the player bar stop via
                                // Cancel{0} → current_gen; track it here too
                                current_gen = gid;
                                pending_vc_music = mode == "music";
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_vc_busy(true);
                                    ui.set_vc_error("".into());
                                    ui.set_vc_status("starting…".into());
                                }).ok();
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::error!("convert failed: {e}");
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_vc_status("engine unavailable".into());
                                }).ok();
                            }
                        }
                    }
                }
                Some(Cmd::VcSaveClip { name, transcript }) => {
                    if let Some(src) = vc_source.clone() {
                        match proxy.save_source_clip(&src, &name, &transcript).await {
                            Ok(id) if !id.is_empty() => {
                                // saved mid-transcription: route the pending
                                // result back into this clip's cache
                                if pending_vc_tr != 0 {
                                    vc_tr_clip = id.clone();
                                }
                                vc_clips_data = refresh_vc_clips(&ui, &proxy).await;
                                // arm the stored copy: the scratch wav gets
                                // overwritten by the next recording
                                if let Some((cid, cname, cpath, _)) =
                                    vc_clips_data.iter().find(|(cid, _, _, _)| *cid == id).cloned()
                                {
                                    vc_source = Some(cpath);
                                    ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_vc_armed_id(cid.into());
                                        ui.set_vc_armed_saved(true);
                                        ui.set_vc_source_label(cname.into());
                                        ui.set_vc_clip_name("".into());
                                        ui.set_vc_status("clip saved".into());
                                    }).ok();
                                }
                            }
                            _ => {
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_vc_status("⚠ save failed".into());
                                }).ok();
                            }
                        }
                    }
                }
                Some(Cmd::VcDeleteClip { id }) => {
                    if vc_audition_id == id && vc_audition_gen != 0 {
                        let _ = proxy.cancel(vc_audition_gen).await;
                        vc_audition_gen = 0;
                        vc_audition_id.clear();
                    }
                    if let Err(e) = proxy.delete_source_clip(&id).await {
                        tracing::error!("delete clip failed: {e}");
                    }
                    vc_clips_data = refresh_vc_clips(&ui, &proxy).await;
                    // deleting the armed clip disarms it — its file is gone
                    let disarm = vc_source
                        .as_deref()
                        .map(|p| !vc_clips_data.iter().any(|(_, _, cp, _)| cp == p)
                            && p.contains("/clips/"))
                        .unwrap_or(false);
                    if disarm {
                        vc_source = None;
                    }
                    ui.upgrade_in_event_loop(move |ui| {
                        ui.set_vc_audition_id("".into());
                        if disarm {
                            ui.set_vc_has_source(false);
                            ui.set_vc_source_label("".into());
                            ui.set_vc_armed_id("".into());
                            ui.set_vc_armed_saved(false);
                        }
                    }).ok();
                }
                Some(Cmd::VcArmClip { id }) => {
                    if let Some((cid, cname, cpath, ctr)) =
                        vc_clips_data.iter().find(|(cid, _, _, _)| *cid == id).cloned()
                    {
                        vc_source = Some(cpath.clone());
                        let cached = ctr.clone();
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_vc_has_source(true);
                            ui.set_vc_source_label(cname.into());
                            ui.set_vc_armed_id(cid.into());
                            ui.set_vc_armed_saved(true);
                            ui.set_vc_error("".into());
                            ui.set_vc_status("".into());
                        }).ok();
                        if !ctr.trim().is_empty() {
                            // transcript is cached with the clip — no whisper run
                            pending_vc_tr = 0;
                            vc_tr_clip.clear();
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_vc_transcribing(false);
                                ui.set_vc_transcript(cached.into());
                            }).ok();
                        } else {
                            // saved without a transcript (pre-cache row or
                            // whisper hadn't finished) — transcribe + backfill
                            match proxy.transcribe_file(&cpath).await {
                                Ok(rid) => {
                                    pending_vc_tr = rid;
                                    vc_tr_clip = id.clone();
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_vc_transcribing(true);
                                        ui.set_vc_transcript("".into());
                                    }).ok();
                                }
                                Err(e) => tracing::error!("vc transcribe failed: {e}"),
                            }
                        }
                    }
                }
                Some(Cmd::VcAudition { id }) => {
                    if vc_audition_id == id && vc_audition_gen != 0 {
                        // toggle off
                        let _ = proxy.cancel(vc_audition_gen).await;
                        vc_audition_gen = 0;
                        vc_audition_id.clear();
                        ui.upgrade_in_event_loop(|ui| ui.set_vc_audition_id("".into())).ok();
                    } else {
                        let resolved = if id == "scratch" {
                            vc_source.clone().map(|p| (p, "source clip".to_string()))
                        } else {
                            vc_clips_data.iter().find(|(cid, _, _, _)| *cid == id)
                                .map(|(_, n, p, _)| (p.clone(), n.clone()))
                        };
                        if let Some((path, title)) = resolved {
                            match proxy.play_file(&path, &title).await {
                                Ok(gid) if gid != 0 => {
                                    vc_audition_gen = gid;
                                    vc_audition_id = id.clone();
                                    ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_vc_audition_id(id.into());
                                    }).ok();
                                }
                                _ => {
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_vc_status("⚠ can't play this file".into());
                                    }).ok();
                                }
                            }
                        }
                    }
                }
                Some(Cmd::SettingsLoad) => {
                    let (mics, mons) = list_audio_devices().await;
                    st_mics = mics;
                    st_mons = mons;
                    // effective engine knobs select the dropdown rows
                    let (mut cap_secs, mut steps) = (180i64, 25i64);
                    if let Ok(j) = proxy.get_settings().await {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&j) {
                            if let Some(e) = v.get("effective") {
                                cap_secs = e.get("vc_max_secs").and_then(|x| x.as_f64()).unwrap_or(180.0) as i64;
                                steps = e.get("seedvc_steps").and_then(|x| x.as_i64()).unwrap_or(25);
                            }
                        }
                    }
                    let mic_names: Vec<SharedString> = std::iter::once(SharedString::from("System default"))
                        .chain(st_mics.iter().map(|(_, d)| SharedString::from(d.as_str())))
                        .collect();
                    let mon_names: Vec<SharedString> = std::iter::once(SharedString::from("Default sink monitor"))
                        .chain(st_mons.iter().map(|(_, d)| SharedString::from(d.as_str())))
                        .collect();
                    let mic_idx = st_mics.iter().position(|(n, _)| *n == cfg.mic_device)
                        .map(|i| i as i32 + 1).unwrap_or(0);
                    let mon_idx = st_mons.iter().position(|(n, _)| *n == cfg.monitor_device)
                        .map(|i| i as i32 + 1).unwrap_or(0);
                    let cap_names: Vec<SharedString> =
                        ST_CAP_SECS.iter().map(|(_, l)| SharedString::from(*l)).collect();
                    let cap_idx = ST_CAP_SECS.iter().position(|(s, _)| *s == cap_secs)
                        .map(|i| i as i32).unwrap_or(1);
                    let steps_names: Vec<SharedString> =
                        ST_STEP_OPTS.iter().map(|s| SharedString::from(s.to_string().as_str())).collect();
                    let steps_idx = ST_STEP_OPTS.iter().position(|s| *s == steps)
                        .map(|i| i as i32).unwrap_or(1);
                    let refine = cfg.refine_dictation;
                    let stop_engine = cfg.stop_engine_on_quit;
                    let export_dir = cfg.export_dir.clone();
                    let data_dir = std::env::var("SYRINX_DATA_DIR").unwrap_or_else(|_| {
                        format!("{}/.local/share/syrinx", std::env::var("HOME").unwrap_or_default())
                    });
                    ui.upgrade_in_event_loop(move |ui| {
                        ui.set_st_mic_names(ModelRc::from(Rc::new(VecModel::from(mic_names))));
                        ui.set_st_mic_index(mic_idx);
                        ui.set_st_mon_names(ModelRc::from(Rc::new(VecModel::from(mon_names))));
                        ui.set_st_mon_index(mon_idx);
                        ui.set_st_cap_names(ModelRc::from(Rc::new(VecModel::from(cap_names))));
                        ui.set_st_cap_index(cap_idx);
                        ui.set_st_steps_names(ModelRc::from(Rc::new(VecModel::from(steps_names))));
                        ui.set_st_steps_index(steps_idx);
                        ui.set_st_refine(refine);
                        ui.set_st_stop_engine(stop_engine);
                        ui.set_st_export_dir(export_dir.into());
                        ui.set_st_data_dir(data_dir.into());
                    }).ok();
                }
                Some(Cmd::SaveTheme { theme }) => {
                    cfg.theme = theme;
                    save_config(&cfg);
                }
                Some(Cmd::StPickMic { index }) => {
                    cfg.mic_device = if index == 0 {
                        String::new()
                    } else {
                        st_mics.get(index - 1).map(|(n, _)| n.clone()).unwrap_or_default()
                    };
                    save_config(&cfg);
                }
                Some(Cmd::StPickMonitor { index }) => {
                    cfg.monitor_device = if index == 0 {
                        String::new()
                    } else {
                        st_mons.get(index - 1).map(|(n, _)| n.clone()).unwrap_or_default()
                    };
                    save_config(&cfg);
                }
                Some(Cmd::StToggleRefine) => {
                    cfg.refine_dictation = !cfg.refine_dictation;
                    save_config(&cfg);
                    let on = cfg.refine_dictation;
                    ui.upgrade_in_event_loop(move |ui| ui.set_st_refine(on)).ok();
                }
                Some(Cmd::StToggleStopEngine) => {
                    cfg.stop_engine_on_quit = !cfg.stop_engine_on_quit;
                    save_config(&cfg);
                    let on = cfg.stop_engine_on_quit;
                    ui.upgrade_in_event_loop(move |ui| ui.set_st_stop_engine(on)).ok();
                }
                Some(Cmd::StPickExportDir) => {
                    if let Some(handle) = rfd::AsyncFileDialog::new().pick_folder().await {
                        cfg.export_dir = handle.path().to_string_lossy().to_string();
                        save_config(&cfg);
                        let dir = cfg.export_dir.clone();
                        ui.upgrade_in_event_loop(move |ui| ui.set_st_export_dir(dir.into())).ok();
                    }
                }
                Some(Cmd::StPickCap { index }) => {
                    if let Some((secs, _)) = ST_CAP_SECS.get(index) {
                        if let Err(e) = proxy.set_setting("vc_max_secs", &secs.to_string()).await {
                            tracing::error!("set vc cap failed: {e}");
                        }
                    }
                }
                Some(Cmd::StPickSteps { index }) => {
                    if let Some(steps) = ST_STEP_OPTS.get(index) {
                        if let Err(e) = proxy.set_setting("seedvc_steps", &steps.to_string()).await {
                            tracing::error!("set seedvc steps failed: {e}");
                        }
                    }
                }
                Some(Cmd::LibLoad) => {
                    let (rows, voices) = lib_load(&proxy).await;
                    lib_rows = rows;
                    lib_voices = voices;
                    lib_loaded = true;
                    lib_apply(&ui, &lib_rows, &lib_voices, &lib_filters);
                }
                Some(Cmd::LibRefilter { q, type_idx, voice_idx, starred, model_idx }) => {
                    lib_filters = (q, type_idx, voice_idx, starred, model_idx);
                    lib_apply(&ui, &lib_rows, &lib_voices, &lib_filters);
                }
                Some(Cmd::LibSaveTags { id, csv }) => {
                    let tags: Vec<String> = csv
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                    let json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into());
                    if let Err(e) = proxy.set_history_tags(&id, &json).await {
                        tracing::error!("set tags failed: {e}");
                    }
                    ui.upgrade_in_event_loop(|ui| ui.set_lib_tag_id("".into())).ok();
                    let (rows, voices) = lib_load(&proxy).await;
                    lib_rows = rows;
                    lib_voices = voices;
                    lib_apply(&ui, &lib_rows, &lib_voices, &lib_filters);
                }
                Some(Cmd::VoicesLoad) => {
                    refresh_voices_table(&ui, &proxy, &mut avatar_cache, &mut voices_all).await;
                }
                Some(Cmd::VoicesSearch { q }) => {
                    let data = voices_all.clone();
                    ui.upgrade_in_event_loop(move |ui| {
                        ui.set_vp_rows(ModelRc::from(Rc::new(VecModel::from(vp_to_rows(&data, &q)))));
                    }).ok();
                }
                Some(Cmd::VoicesInspect { id }) => {
                    vp_inspected = id.clone();
                    inspect_profile(&ui, &proxy, &voices_all, &id).await;
                }
                Some(Cmd::PlaySample { id }) => {
                    if sample_playing == id && sample_gen != 0 {
                        // toggle: same sample clicked while playing -> stop
                        proxy.cancel(sample_gen).await.ok();
                        sample_gen = 0;
                        sample_playing.clear();
                        ui.upgrade_in_event_loop(|ui| ui.set_vs_playing("".into())).ok();
                    } else {
                        // engine playback is serialized (latest wins), so starting
                        // a different sample implicitly replaces the current one
                        match proxy.play_sample(&id).await {
                            Ok(g) if g != 0 => {
                                sample_gen = g;
                                sample_playing = id.clone();
                                let id2 = id.clone();
                                ui.upgrade_in_event_loop(move |ui| ui.set_vs_playing(id2.into())).ok();
                            }
                            _ => {}
                        }
                    }
                }
                Some(Cmd::FxeShow) => {
                    if fxe_defs.is_empty() {
                        let defs_json = proxy.list_effects().await.unwrap_or_else(|_| "[]".into());
                        fxe_defs = serde_json::from_str(&defs_json).unwrap_or_default();
                        let mut add: Vec<SharedString> = vec!["＋ Add effect…".into()];
                        for d in &fxe_defs {
                            add.push(d.get("label").and_then(|v| v.as_str()).unwrap_or("").into());
                        }
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_fxe_add_model(ModelRc::from(Rc::new(VecModel::from(add))));
                        }).ok();
                    }
                    let r = refresh_effect_presets(&ui, &proxy).await;
                    effect_ids = r.0;
                    fxe_presets = r.1;
                    fxe_chain.clear();
                    fxe_pid.clear();
                    fxe_expanded = -1;
                    fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                    ui.upgrade_in_event_loop(|ui| {
                        ui.set_fxe_preset_index(-1);
                        ui.set_fxe_builtin(false);
                        ui.set_fxe_can_delete(false);
                        ui.set_fxe_name("".into());
                        ui.set_fxe_desc("".into());
                        ui.set_fxe_status("".into());
                        ui.set_fxe_open(true);
                    }).ok();
                }
                Some(Cmd::FxeLoad { index }) => {
                    if let Some((pid, builtin)) = fxe_presets.get(index).cloned() {
                        if let Ok(pjson) = proxy.get_effect_preset(&pid).await {
                            if let Ok(p) = serde_json::from_str::<serde_json::Value>(&pjson) {
                                fxe_chain = p.get("chain").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                                fxe_expanded = -1;
                                fxe_pid = if builtin { String::new() } else { pid };
                                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                // editing a builtin saves as the user's own copy
                                let display = if builtin { format!("{name} (custom)") } else { name.to_string() };
                                let desc = p.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                                let idx = index as i32;
                                ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_fxe_preset_index(idx);
                                    ui.set_fxe_builtin(builtin);
                                    ui.set_fxe_can_delete(!builtin);
                                    ui.set_fxe_name(display.into());
                                    ui.set_fxe_desc(desc.into());
                                    ui.set_fxe_status("".into());
                                }).ok();
                            }
                        }
                    }
                }
                Some(Cmd::FxeNew) => {
                    fxe_chain.clear();
                    fxe_pid.clear();
                    fxe_expanded = -1;
                    fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                    ui.upgrade_in_event_loop(|ui| {
                        ui.set_fxe_preset_index(-1);
                        ui.set_fxe_builtin(false);
                        ui.set_fxe_can_delete(false);
                        ui.set_fxe_name("".into());
                        ui.set_fxe_desc("".into());
                        ui.set_fxe_status("".into());
                    }).ok();
                }
                Some(Cmd::FxeAdd { index }) => {
                    if let Some(d) = fxe_defs.get(index) {
                        let t = d.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let mut params = serde_json::Map::new();
                        if let Some(list) = d.get("params").and_then(|p| p.as_array()) {
                            for pd in list {
                                if let (Some(n), Some(v)) =
                                    (pd.get("name").and_then(|v| v.as_str()), pd.get("default"))
                                {
                                    params.insert(n.to_string(), v.clone());
                                }
                            }
                        }
                        fxe_chain.push(serde_json::json!({"type": t, "enabled": true, "params": params}));
                        fxe_expanded = fxe_chain.len() as i32 - 1;
                        fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                    }
                }
                Some(Cmd::FxeRemove { index }) => {
                    if index < fxe_chain.len() {
                        fxe_chain.remove(index);
                        fxe_expanded = -1;
                        fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                    }
                }
                Some(Cmd::FxeToggle { index }) => {
                    if let Some(e) = fxe_chain.get_mut(index) {
                        let cur = e.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                        e["enabled"] = serde_json::Value::Bool(!cur);
                        fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                    }
                }
                Some(Cmd::FxeMove { index, dir }) => {
                    let j = index as i32 + dir;
                    if index < fxe_chain.len() && j >= 0 && (j as usize) < fxe_chain.len() {
                        fxe_chain.swap(index, j as usize);
                        if fxe_expanded == index as i32 {
                            fxe_expanded = j;
                        } else if fxe_expanded == j {
                            fxe_expanded = index as i32;
                        }
                        fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                    }
                }
                Some(Cmd::FxeExpand { index }) => {
                    fxe_expanded = if fxe_expanded == index as i32 { -1 } else { index as i32 };
                    fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                }
                Some(Cmd::FxeParam { index, norm }) => {
                    if let Some(e) = fxe_chain.get_mut(usize::try_from(fxe_expanded).unwrap_or(usize::MAX)) {
                        let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let pd = fxe_defs
                            .iter()
                            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(t.as_str()))
                            .and_then(|d| d.get("params"))
                            .and_then(|p| p.as_array())
                            .and_then(|l| l.get(index))
                            .cloned();
                        if let Some(pd) = pd {
                            let name = pd.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let min = pd.get("min").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let max = pd.get("max").and_then(|v| v.as_f64()).unwrap_or(1.0);
                            let step = pd.get("step").and_then(|v| v.as_f64()).unwrap_or(0.01);
                            let raw = min + norm as f64 * (max - min);
                            let snapped = ((raw / step).round() * step).clamp(min, max);
                            if !e.get("params").map(|p| p.is_object()).unwrap_or(false) {
                                e["params"] = serde_json::json!({});
                            }
                            e["params"][name.as_str()] = serde_json::json!(snapped);
                            // update the one param row in place — replacing the model
                            // mid-drag would tear down the slider under the pointer
                            let vt: SharedString = fx_fmt(snapped, step).into();
                            let nnorm = ((snapped - min) / (max - min)).clamp(0.0, 1.0) as f32;
                            ui.upgrade_in_event_loop(move |ui| {
                                let m = ui.get_fxe_params();
                                if let Some(vm) = m.as_any().downcast_ref::<VecModel<FxParamItem>>() {
                                    if let Some(mut row) = vm.row_data(index) {
                                        row.value_text = vt;
                                        row.norm = nnorm;
                                        vm.set_row_data(index, row);
                                    }
                                }
                            }).ok();
                        }
                    }
                }
                Some(Cmd::FxeSave { name, desc }) => {
                    if name.trim().is_empty() {
                        ui.upgrade_in_event_loop(|ui| ui.set_fxe_status("a name is required".into())).ok();
                    } else {
                        let chain_json = serde_json::to_string(&fxe_chain).unwrap_or_else(|_| "[]".into());
                        let saved = if fxe_pid.is_empty() {
                            proxy.create_effect_preset(name.trim(), &desc, &chain_json).await
                                .ok().filter(|s| !s.is_empty())
                        } else {
                            match proxy.update_effect_preset(&fxe_pid, name.trim(), &desc, &chain_json).await {
                                Ok(true) => Some(fxe_pid.clone()),
                                _ => None,
                            }
                        };
                        match saved {
                            Some(pid) => {
                                fxe_pid = pid.clone();
                                let r = refresh_effect_presets(&ui, &proxy).await;
                                effect_ids = r.0;
                                fxe_presets = r.1;
                                let idx = fxe_presets.iter().position(|(id, _)| *id == pid)
                                    .map(|i| i as i32).unwrap_or(-1);
                                let display = name.trim().to_string();
                                ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_fxe_preset_index(idx);
                                    ui.set_fxe_builtin(false);
                                    ui.set_fxe_can_delete(true);
                                    ui.set_fxe_name(display.into());
                                    ui.set_fxe_status("saved ✓".into());
                                }).ok();
                            }
                            None => {
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_fxe_status("couldn't save — duplicate name?".into());
                                }).ok();
                            }
                        }
                    }
                }
                Some(Cmd::FxeDelete) => {
                    if !fxe_pid.is_empty() {
                        let _ = proxy.delete_effect_preset(&fxe_pid).await;
                        fxe_pid.clear();
                        fxe_chain.clear();
                        fxe_expanded = -1;
                        let r = refresh_effect_presets(&ui, &proxy).await;
                        effect_ids = r.0;
                        fxe_presets = r.1;
                        fxe_sync(&ui, &fxe_defs, &fxe_chain, fxe_expanded);
                        ui.upgrade_in_event_loop(|ui| {
                            ui.set_fxe_preset_index(-1);
                            ui.set_fxe_can_delete(false);
                            ui.set_fxe_name("".into());
                            ui.set_fxe_desc("".into());
                            ui.set_fxe_status("preset deleted".into());
                        }).ok();
                    }
                }
                Some(Cmd::FxePreview { hid }) => {
                    let chain_json = serde_json::to_string(&fxe_chain).unwrap_or_else(|_| "[]".into());
                    let status = match proxy.preview_effects(&hid, &chain_json).await {
                        Ok(id) if id != 0 => "previewing…",
                        _ => "preview failed",
                    };
                    ui.upgrade_in_event_loop(move |ui| ui.set_fxe_status(status.into())).ok();
                }
                Some(Cmd::CvPickAvatar) => {
                    if let Some(handle) = rfd::AsyncFileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                        .pick_file()
                        .await
                    {
                        let path = handle.path().to_string_lossy().to_string();
                        // decode once, remember the real size, and hand the
                        // dialog a filtered ≤1200px preview (max zoom 4x on a
                        // 220px viewport needs 880px — stays sharp)
                        match image::open(&path) {
                            Ok(img) => {
                                let (fw, fh) = (img.width(), img.height());
                                let preview = if fw.max(fh) > 1200 {
                                    img.thumbnail(1200, 1200)
                                } else {
                                    img
                                };
                                let rgba = preview.to_rgba8();
                                let buf: RgbaBuf = (rgba.as_raw().clone(), rgba.width(), rgba.height());
                                ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_crop_src(rgba_to_image(&buf));
                                    ui.set_crop_full_w(fw as i32);
                                    ui.set_crop_full_h(fh as i32);
                                    ui.set_crop_path(path.into());
                                    ui.set_crop_zoom(1.0);
                                    ui.set_crop_cx(0.5);
                                    ui.set_crop_cy(0.5);
                                    ui.set_crop_stage("mode".into());
                                    ui.set_crop_open(true);
                                }).ok();
                            }
                            Err(e) => tracing::error!("could not load image: {e}"),
                        }
                    }
                }
                Some(Cmd::CvStageAvatar { path, mode, sx, sy, sw, sh }) => {
                    cv_avatar = Some((path.clone(), mode.clone(), sx, sy, sw, sh));
                    let baked = bake_avatar_rgba(&mut avatar_cache, &path, sx, sy, sw, sh);
                    ui.upgrade_in_event_loop(move |ui| {
                        if let Some(b) = baked {
                            ui.set_cv_avatar(rgba_to_image(&b));
                            ui.set_cv_avatar_mode(mode.into());
                            ui.set_cv_has_avatar(true);
                        }
                    }).ok();
                }
                Some(Cmd::ImportVoice) => {
                    if let Some(handle) = rfd::AsyncFileDialog::new()
                        .add_filter("Syrinx voice package", &["zip"])
                        .pick_file()
                        .await
                    {
                        let src = handle.path().to_string_lossy().to_string();
                        match proxy.import_profile(&src).await {
                            Ok(pid) => {
                                tracing::info!("imported voice {pid}");
                                refresh_grid(&ui, &proxy, &mut avatar_cache).await;
                                refresh_voices_table(&ui, &proxy, &mut avatar_cache, &mut voices_all).await;
                            }
                            Err(e) => tracing::error!("import voice failed: {e}"),
                        }
                    }
                }
                None => break SessionEnd::UiQuit,
            },
            else => break SessionEnd::UiQuit,
        }
    };
    end
}

/// Linux worker: one D-Bus session for the app's life, byte-identical to
/// before (the connect attempt is what wakes the engine via D-Bus activation).
/// The engine's lifecycle belongs to systemd, so there is nothing to supervise
/// and the session-end reason is discarded.
#[cfg(target_os = "linux")]
async fn worker(
    ui: slint::Weak<AppWindow>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) -> anyhow::Result<()> {
    let proxy = EngineClient::connect_dbus().await?;
    let _ = run_session(&ui, &mut rx, proxy).await;
    Ok(())
}

/// Windows/macOS worker: the app owns the engine (RPC-PROTOCOL.md §13). Adopt
/// or spawn it, run the session, and on a mid-session transport loss tear down
/// and respawn/reconnect with backoff — re-running the initial data loads each
/// time. The cold-launch splash is re-shown while a restart is in flight (the
/// same mechanism that covers the first round-trip); on the initial pass the
/// splash is already up from launch.
#[cfg(not(target_os = "linux"))]
async fn worker(
    ui: slint::Weak<AppWindow>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) -> anyhow::Result<()> {
    let mut sup = engine_proc::EngineSupervisor::new();
    // None = first pass (adopt-or-spawn); Some(uptime) = reconnect after a drop.
    let mut reconnect_after: Option<std::time::Duration> = None;
    loop {
        let proxy = match reconnect_after.take() {
            None => sup.adopt_or_spawn().await,
            Some(uptime) => sup.reconnect(uptime).await,
        };
        let started = std::time::Instant::now();
        match run_session(&ui, &mut rx, proxy).await {
            SessionEnd::UiQuit => break,
            SessionEnd::TransportLost => {
                // Re-show the cold-launch splash while the engine comes back.
                ui.upgrade_in_event_loop(|ui| ui.set_booting(true)).ok();
                reconnect_after = Some(started.elapsed());
            }
        }
    }
    // Quit: a spawned engine dies with the app (stdin close → watchdog exit);
    // an adopted one is left running (§13.2/§13.3). This active close+grace+kill
    // runs when the session ends in-loop; the guaranteed backstop is the OS
    // closing the child's held stdin on app-process exit (§13.1 watchdog).
    sup.shutdown().await;
    Ok(())
}

/// Unit tests for the pure helpers — the JSON→model translations the worker
/// leans on. No D-Bus, no filesystem, no event loop: everything here is sync
/// and deterministic.
#[cfg(test)]
mod tests {
    use super::*;

    // --- AppConfig -------------------------------------------------------

    #[test]
    fn stop_engine_on_quit_defaults_on_with_no_file_and_with_an_old_one() {
        // no settings.json at all
        assert!(AppConfig::default().stop_engine_on_quit);
        // a settings.json written before the field existed
        let old = r#"{"theme":"rice","mic_device":"","monitor_device":"",
                      "refine_dictation":true,"export_dir":"/tmp"}"#;
        let cfg: AppConfig = serde_json::from_str(old).unwrap();
        assert!(cfg.stop_engine_on_quit);
        assert_eq!(cfg.theme, "rice"); // the rest of the file still reads back
        assert!(cfg.refine_dictation);
    }

    #[test]
    fn stop_engine_on_quit_round_trips_when_explicitly_off() {
        let cfg = AppConfig { stop_engine_on_quit: false, ..Default::default() };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"stop_engine_on_quit\":false"));
        assert!(!serde_json::from_str::<AppConfig>(&json).unwrap().stop_engine_on_quit);
    }

    // --- fmt_dur ---------------------------------------------------------

    #[test]
    fn fmt_dur_rounds_to_whole_seconds() {
        assert_eq!(fmt_dur(0.0), "0:00");
        assert_eq!(fmt_dur(5.4), "0:05");
        assert_eq!(fmt_dur(5.6), "0:06");
        assert_eq!(fmt_dur(59.6), "1:00"); // rounds up across the minute
        assert_eq!(fmt_dur(65.0), "1:05");
        assert_eq!(fmt_dur(600.0), "10:00");
        assert_eq!(fmt_dur(3661.0), "61:01"); // no hours field — minutes keep counting
    }

    #[test]
    fn fmt_dur_clamps_junk_to_zero() {
        assert_eq!(fmt_dur(-3.0), "0:00");
        assert_eq!(fmt_dur(f64::NAN), "0:00"); // max() prefers the non-NaN operand
    }

    // --- is_vc_engine ----------------------------------------------------

    #[test]
    fn is_vc_engine_covers_every_conversion_engine() {
        for e in ["chatterbox_vc", "seed_vc", "vevo_timbre", "vevo2"] {
            assert!(is_vc_engine(e), "{e} should be a VC engine");
        }
        for e in ["", "kokoro", "qwen", "qwen_custom_voice", "luxtts", "chatterbox", "tada"] {
            assert!(!is_vc_engine(e), "{e} should not be a VC engine");
        }
    }

    #[test]
    fn vc_engine_id_tables_stay_in_the_vc_family() {
        assert!(VC_ENGINE_IDS.iter().all(|e| is_vc_engine(e)));
        assert!(VC_MUSIC_ENGINE_IDS.iter().all(|e| is_vc_engine(e)));
    }

    // --- build_history ---------------------------------------------------

    #[test]
    fn build_history_labels_conversions_and_generations() {
        let json = r#"[
            {"id": "h1", "voice_name": "Piccolo", "voice_id": "prof:1", "engine": "seed_vc",
             "language": "en", "duration": 12.0, "text": "hello", "starred": true},
            {"id": "h2", "voice_name": "Heart", "voice_id": "builtin:kokoro:af_heart",
             "engine": "kokoro", "language": "en", "duration": 3.0, "text": "hi"},
            {"id": "h3", "voice_name": "", "voice_id": "prof:9", "engine": "",
             "language": "es", "duration": 90.0, "text": ""}
        ]"#;
        let items = build_history(json);
        assert_eq!(items.len(), 3);

        // VC rows get the ⇄ prefix set_history_model keys the ⇄ rail off
        assert_eq!(items[0].id, "h1");
        assert_eq!(items[0].voice, "Piccolo");
        assert_eq!(items[0].meta, "⇄ VC · 0:12 · en");
        assert_eq!(items[0].text, "hello");
        assert!(items[0].starred);

        // TTS rows lead with the engine id
        assert_eq!(items[1].meta, "kokoro · 0:03 · en");
        assert!(!items[1].starred); // absent "starred" defaults to false

        // an empty engine drops the field rather than printing a blank one
        assert_eq!(items[2].voice, "prof:9"); // voice_name empty → voice_id
        assert_eq!(items[2].meta, "1:30 · es");
    }

    #[test]
    fn build_history_survives_malformed_json() {
        assert!(build_history("").is_empty());
        assert!(build_history("not json").is_empty());
        assert!(build_history("[]").is_empty());
        assert!(build_history("{\"rows\": []}").is_empty()); // object, not array
        // rows missing every field still produce a (blank) item
        let items = build_history(r#"[{}]"#);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "");
        assert_eq!(items[0].voice, "");
        assert_eq!(items[0].meta, "0:00 · ");
    }

    #[test]
    fn build_history_meta_prefix_matches_the_vc_rail_filter() {
        // set_history_model splits the ⇄ rail on this exact prefix
        let json = r#"[{"id": "h1", "voice_name": "V", "engine": "vevo2",
                        "language": "en", "duration": 4.0}]"#;
        let items = build_history(json);
        assert!(items[0].meta.starts_with("⇄ VC"));
        assert_eq!(items[0].meta.strip_prefix("⇄ VC · "), Some("0:04 · en"));
    }

    // --- size_label ------------------------------------------------------

    #[test]
    fn size_label_switches_to_gb_at_1024() {
        assert_eq!(size_label(0), "0 MB");
        assert_eq!(size_label(512), "512 MB");
        assert_eq!(size_label(1023), "1023 MB");
        assert_eq!(size_label(1024), "1.0 GB");
        assert_eq!(size_label(1536), "1.5 GB");
        assert_eq!(size_label(3072), "3.0 GB");
    }

    // --- build_models ----------------------------------------------------

    #[test]
    fn build_models_routes_by_category() {
        let json = r#"[
            {"id": "kokoro", "display": "Kokoro", "category": "voice", "size_mb": 350,
             "description": "fast", "downloaded": true, "active": true, "supported": true},
            {"id": "qwen", "display": "Qwen", "category": "voice", "size_mb": 2048,
             "downloading": true, "supported": false, "warning": "needs 12 GB VRAM"},
            {"id": "whisper", "display": "Whisper", "category": "stt", "size_mb": 1500},
            {"id": "llama", "display": "Llama", "category": "llm", "size_mb": 4096},
            {"id": "seed_vc", "display": "Seed-VC", "category": "vc", "size_mb": 900},
            {"id": "mystery", "display": "?", "category": "nonesuch", "size_mb": 1}
        ]"#;
        let (voice, stt, llm, vc) = build_models(json);
        assert_eq!(voice.len(), 2);
        assert_eq!(stt.len(), 1);
        assert_eq!(llm.len(), 1);
        assert_eq!(vc.len(), 1);
        // unknown categories are dropped, not bucketed somewhere

        assert_eq!(voice[0].id, "kokoro");
        assert_eq!(voice[0].display, "Kokoro");
        assert_eq!(voice[0].size_label, "350 MB");
        assert_eq!(voice[0].description, "fast");
        assert!(voice[0].downloaded && voice[0].active && voice[0].supported);
        assert!(!voice[0].downloading);
        assert_eq!(voice[0].progress, 0.0); // progress is pushed later by set_model_progress

        assert_eq!(voice[1].size_label, "2.0 GB");
        assert!(voice[1].downloading && !voice[1].downloaded && !voice[1].supported);
        assert_eq!(voice[1].warning, "needs 12 GB VRAM");

        assert_eq!(stt[0].size_label, "1.5 GB");
        assert_eq!(llm[0].size_label, "4.0 GB");
        assert_eq!(vc[0].id, "seed_vc");
    }

    #[test]
    fn build_models_survives_malformed_json() {
        for junk in ["", "nope", "{}", "null"] {
            let (v, s, l, c) = build_models(junk);
            assert!(v.is_empty() && s.is_empty() && l.is_empty() && c.is_empty());
        }
        // a categoryless row is dropped; a category with no fields still lands
        let (v, ..) = build_models(r#"[{}, {"category": "voice"}]"#);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, "");
        assert_eq!(v[0].size_label, "0 MB");
        assert!(!v[0].downloaded && !v[0].supported);
    }

    // --- hardware_line ---------------------------------------------------

    #[test]
    fn hardware_line_names_the_gpu_when_there_is_one() {
        let json = r#"{"cores": 16, "ram_gb": 62.5, "gpu": true,
                       "gpu_name": "NVIDIA GeForce RTX 4090"}"#;
        assert_eq!(hardware_line(json), "16 cores · 62.5 GB RAM · NVIDIA GeForce RTX 4090");
    }

    #[test]
    fn hardware_line_falls_back_when_the_gpu_is_unnamed_or_absent() {
        // present but nameless
        assert_eq!(
            hardware_line(r#"{"cores": 8, "ram_gb": 16.0, "gpu": true, "gpu_name": ""}"#),
            "8 cores · 16.0 GB RAM · GPU"
        );
        // absent — the name, if any, is ignored
        assert_eq!(
            hardware_line(r#"{"cores": 4, "ram_gb": 7.75, "gpu": false, "gpu_name": "iGPU"}"#),
            "4 cores · 7.8 GB RAM · no GPU"
        );
    }

    #[test]
    fn hardware_line_survives_malformed_json() {
        for junk in ["", "not json", "{}", "[]"] {
            assert_eq!(hardware_line(junk), "0 cores · 0.0 GB RAM · no GPU");
        }
    }

    // --- library filters -------------------------------------------------

    #[test]
    fn lib_engines_for_type_music_lists_the_singing_engines() {
        let music = lib_engines_for_type(3);
        assert!(music.contains(&"seed_vc"), "music must offer Seed-VC");
        assert!(music.contains(&"vevo_timbre"), "music must offer Vevo");
        assert_eq!(music, VC_MUSIC_ENGINE_IDS.to_vec());
    }

    #[test]
    fn lib_engines_for_type_splits_tts_from_conversion() {
        let tts = lib_engines_for_type(1);
        assert!(tts.contains(&"kokoro") && tts.contains(&"tada"));
        assert!(!tts.iter().any(|e| is_vc_engine(e)));

        let speech_vc = lib_engines_for_type(2);
        assert_eq!(speech_vc, vec!["chatterbox_vc", "seed_vc", "vevo_timbre"]);
        assert!(speech_vc.iter().all(|e| is_vc_engine(e)));
    }

    #[test]
    fn lib_engines_for_type_all_is_the_full_label_table() {
        let all = lib_engines_for_type(0);
        assert_eq!(all.len(), LIB_ENGINE_LABELS.len());
        assert_eq!(lib_engines_for_type(99), all); // out-of-range falls back to All
        assert_eq!(lib_engines_for_type(-1), all);
        // every filter option must be labelable in the dropdown
        for e in &all {
            assert_ne!(lib_engine_label(e), *e, "{e} has no display label");
        }
    }

    #[test]
    fn lib_engine_label_echoes_unknown_ids() {
        assert_eq!(lib_engine_label("kokoro"), "Kokoro");
        assert_eq!(lib_engine_label("seed_vc"), "Seed-VC");
        assert_eq!(lib_engine_label("brand_new_engine"), "brand_new_engine");
        assert_eq!(lib_engine_label(""), "");
    }

    // --- parse_envelope --------------------------------------------------

    #[test]
    fn parse_envelope_reads_bars_and_duration() {
        let (bars, dur) = parse_envelope(r#"{"duration": 2.5, "bars": [0.25, 0.5, 1.0]}"#)
            .expect("well-formed envelope");
        assert_eq!(bars, vec![0.25f32, 0.5, 1.0]);
        assert_eq!(dur, 2.5);
    }

    #[test]
    fn parse_envelope_skips_non_numeric_bars() {
        let (bars, dur) = parse_envelope(r#"{"duration": 1.0, "bars": [0.5, "x", null, 0.25]}"#)
            .expect("partially typed bars still parse");
        assert_eq!(bars, vec![0.5f32, 0.25]);
        assert_eq!(dur, 1.0);
    }

    #[test]
    fn parse_envelope_rejects_junk_and_degenerate_clips() {
        assert!(parse_envelope("").is_none());
        assert!(parse_envelope("not json").is_none());
        assert!(parse_envelope("{}").is_none()); // no duration
        assert!(parse_envelope(r#"{"duration": 3.0}"#).is_none()); // no bars
        assert!(parse_envelope(r#"{"duration": "3", "bars": [0.5]}"#).is_none()); // wrong type
        assert!(parse_envelope(r#"{"duration": 3.0, "bars": {}}"#).is_none()); // not an array
        assert!(parse_envelope(r#"{"duration": 3.0, "bars": []}"#).is_none()); // empty
        assert!(parse_envelope(r#"{"duration": 3.0, "bars": ["x"]}"#).is_none()); // all filtered
        assert!(parse_envelope(r#"{"duration": 0.0, "bars": [0.5]}"#).is_none()); // zero-length
        assert!(parse_envelope(r#"{"duration": -1.0, "bars": [0.5]}"#).is_none());
    }

    // --- build_captures --------------------------------------------------

    #[test]
    fn build_captures_maps_rows_and_tolerates_junk() {
        let items = build_captures(
            r#"[{"id": "c1", "text": "meeting notes", "date": "2026-07-23"}, {}]"#,
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "c1");
        assert_eq!(items[0].text, "meeting notes");
        assert_eq!(items[0].date, "2026-07-23");
        assert_eq!(items[1].id, ""); // missing fields blank out, no panic

        for junk in ["", "nope", "{}"] {
            assert!(build_captures(junk).is_empty());
        }
    }

    // --- fx_fmt ----------------------------------------------------------

    #[test]
    fn fx_fmt_matches_decimals_to_step_size() {
        assert_eq!(fx_fmt(12.4, 1.0), "12"); // whole steps → no decimals
        assert_eq!(fx_fmt(12.6, 5.0), "13");
        assert_eq!(fx_fmt(0.567, 0.1), "0.6"); // tenth steps → one decimal
        assert_eq!(fx_fmt(-3.44, 0.5), "-3.4");
        assert_eq!(fx_fmt(0.5678, 0.01), "0.57"); // finer → two decimals
        assert_eq!(fx_fmt(0.5678, 0.001), "0.57");
    }

    // --- kokoro language plumbing ----------------------------------------

    #[test]
    fn kokoro_lang_code_reads_the_id_prefix() {
        assert_eq!(kokoro_lang_code("builtin:kokoro:af_heart"), "en");
        assert_eq!(kokoro_lang_code("builtin:kokoro:bm_george"), "en");
        assert_eq!(kokoro_lang_code("builtin:kokoro:ef_dora"), "es");
        assert_eq!(kokoro_lang_code("builtin:kokoro:jf_alpha"), "ja");
        assert_eq!(kokoro_lang_code("builtin:kokoro:zf_xiaobei"), "zh");
        // anything unrecognized (or id-less) reads as English
        assert_eq!(kokoro_lang_code("builtin:kokoro:xx_none"), "en");
        assert_eq!(kokoro_lang_code(""), "en");
    }

    #[test]
    fn kokoro_prefixes_inverts_kokoro_lang_code() {
        for (code, prefix) in [("es", 'e'), ("fr", 'f'), ("hi", 'h'), ("it", 'i'),
                               ("ja", 'j'), ("pt", 'p'), ("zh", 'z')] {
            assert_eq!(kokoro_prefixes(code), &[prefix]);
            assert_eq!(kokoro_lang_code(&format!("builtin:kokoro:{prefix}f_x")), code);
        }
        assert_eq!(kokoro_prefixes("en"), &['a', 'b']);
        assert_eq!(kokoro_prefixes("klingon"), &['a', 'b']); // unknown → en
    }

    #[test]
    fn langs_for_engine_narrows_per_engine() {
        // chatterbox is the polyglot: the whole table
        assert_eq!(langs_for_engine("chatterbox").len(), 23);
        // english-only engines
        assert_eq!(langs_for_engine("luxtts"), vec![("English", "en")]);
        assert_eq!(langs_for_engine("chatterbox_turbo"), vec![("English", "en")]);
        // qwen keeps its own order (zh first), not the alphabetical table's
        let qwen = langs_for_engine("qwen");
        assert_eq!(qwen.len(), 10);
        assert_eq!(qwen[0], ("Chinese", "zh"));
        assert_eq!(qwen[1], ("English", "en"));
        assert_eq!(langs_for_engine("qwen_custom_voice"), qwen);
        // unknown engines fall back to kokoro's subset
        let kokoro = langs_for_engine("kokoro");
        assert_eq!(kokoro.len(), 8);
        assert_eq!(langs_for_engine("brand_new_engine"), kokoro);
        // every kokoro language must be reachable from an id prefix
        for (_, code) in &kokoro {
            let p = kokoro_prefixes(code)[0];
            assert_eq!(kokoro_lang_code(&format!("builtin:kokoro:{p}f_x")), *code);
        }
    }

    // --- profile_err_msg -------------------------------------------------

    #[test]
    fn profile_err_msg_humanizes_the_duplicate_name_error() {
        let dup = EngineError::Engine(
            "sqlite3.IntegrityError: UNIQUE constraint failed: profiles.name".into(),
        );
        assert_eq!(profile_err_msg(&dup), "A voice with that name already exists.");
        // anything else passes through as the raw engine message
        let other = EngineError::Engine("engine is busy".into());
        assert_eq!(profile_err_msg(&other), "engine is busy");
    }

    // --- build_grid ------------------------------------------------------

    fn raw_voices() -> Vec<(String, String)> {
        vec![
            ("builtin:kokoro:af_heart".into(), "Heart".into()),
            ("builtin:kokoro:bm_george".into(), "George".into()),
            ("prof:1".into(), "Piccolo".into()),
        ]
    }

    #[test]
    fn build_grid_splits_builtins_from_user_cards() {
        let profiles = r#"[{"id": "prof:1", "language": "ja", "voice_type": "clone",
                            "description": "green namekian", "has_personality": true,
                            "avatar_path": "/tmp/p.png", "avatar_mode": "panel",
                            "avatar_sx": 10, "avatar_sy": 20, "avatar_side": 300,
                            "avatar_sh": 400}]"#;
        let g = build_grid(raw_voices(), profiles);

        assert_eq!(g.kokoro_names, vec!["Heart", "George"]);
        assert_eq!(g.kokoro_ids, vec!["builtin:kokoro:af_heart", "builtin:kokoro:bm_george"]);
        assert_eq!(g.default_selected, "builtin:kokoro:af_heart");

        // grid = Kokoro card + user cards, padded to a full 3-column row
        assert_eq!(g.grid.len(), 3);
        assert_eq!(g.grid[0].id, "__kokoro__");
        assert_eq!(g.grid[0].kind, "model-defaults");

        let p = &g.grid[1];
        assert_eq!(p.id, "prof:1");
        assert_eq!(p.name, "Piccolo");
        assert_eq!(p.desc, "green namekian");
        assert_eq!(p.lang, "ja");
        assert_eq!(p.kind, "clone");
        assert!(p.has_personality);
        assert_eq!(p.avatar_path, "/tmp/p.png");
        assert_eq!(p.avatar_mode, "panel");
        assert_eq!((p.avatar_sx, p.avatar_sy, p.avatar_side, p.avatar_sh), (10, 20, 300, 400));

        assert_eq!(g.grid[2].kind, "empty"); // spacer
    }

    #[test]
    fn build_grid_pads_to_multiples_of_three() {
        // 1 kokoro card + n user cards, padded up
        for (users, want) in [(0, 3), (1, 3), (2, 3), (3, 6), (5, 6), (6, 9)] {
            let raw: Vec<(String, String)> = (0..users)
                .map(|i| (format!("prof:{i}"), format!("V{i}")))
                .collect();
            let g = build_grid(raw, "[]");
            assert_eq!(g.grid.len(), want, "{users} user voices");
            assert!(g.grid.len().is_multiple_of(3));
            assert_eq!(g.grid.iter().filter(|d| d.kind == "empty").count(), want - users - 1);
        }
    }

    #[test]
    fn build_grid_defaults_profiles_with_no_details() {
        // profile missing from ListProfiles entirely
        let g = build_grid(raw_voices(), "[]");
        let p = &g.grid[1];
        assert_eq!(p.desc, "");
        assert_eq!(p.lang, "en");
        assert_eq!(p.kind, "voice");
        assert_eq!(p.avatar_mode, "circle");
        assert!(!p.has_personality);

        // present but description-less: the kind label stands in
        let g = build_grid(raw_voices(), r#"[{"id": "prof:1", "has_personality": true}]"#);
        assert_eq!(g.grid[1].desc, "Has personality");
        let g = build_grid(raw_voices(), r#"[{"id": "prof:1", "description": "   "}]"#);
        assert_eq!(g.grid[1].desc, "Custom voice"); // whitespace counts as blank

        // malformed profiles JSON degrades to "no details", it doesn't panic
        let g = build_grid(raw_voices(), "not json");
        assert_eq!(g.grid[1].lang, "en");
    }

    #[test]
    fn build_grid_with_no_builtins_has_no_default_selection() {
        let g = build_grid(vec![("prof:1".into(), "Piccolo".into())], "[]");
        assert!(g.kokoro_ids.is_empty());
        assert_eq!(g.default_selected, "");
    }

    // --- vp_to_rows ------------------------------------------------------

    fn vp_row(name: &str, desc: &str, lang: &str, engine: &str) -> VpRowData {
        VpRowData {
            id: format!("prof:{name}"),
            name: name.into(),
            desc: desc.into(),
            lang: lang.into(),
            engine: engine.into(),
            samples: "2".into(),
            gens: "7".into(),
            baked: None,
        }
    }

    #[test]
    fn vp_to_rows_filters_case_insensitively_across_columns() {
        let data = vec![
            vp_row("Piccolo", "green namekian", "ja", "seed_vc"),
            vp_row("Heart", "warm narrator", "en", "follows"),
        ];
        assert_eq!(vp_to_rows(&data, "").len(), 2); // empty filter keeps everything
        assert_eq!(vp_to_rows(&data, "  ").len(), 0); // but a space is a real query

        let hits = |q: &str| {
            vp_to_rows(&data, q).iter().map(|r| r.name.to_string()).collect::<Vec<_>>()
        };
        assert_eq!(hits("picc"), vec!["Piccolo"]); // name
        assert_eq!(hits("PICCOLO"), vec!["Piccolo"]); // case-insensitive
        assert_eq!(hits("narrator"), vec!["Heart"]); // description
        assert_eq!(hits("ja"), vec!["Piccolo"]); // language
        assert_eq!(hits("follows"), vec!["Heart"]); // engine
        assert!(hits("nothing here").is_empty());
    }

    #[test]
    fn vp_to_rows_carries_the_row_fields_through() {
        let data = vec![vp_row("Piccolo", "green namekian", "ja", "seed_vc")];
        let rows = vp_to_rows(&data, "");
        assert_eq!(rows[0].id, "prof:Piccolo");
        assert_eq!(rows[0].desc, "green namekian");
        assert_eq!(rows[0].engine, "seed_vc");
        assert_eq!(rows[0].samples, "2");
        assert_eq!(rows[0].gens, "7");
        assert!(!rows[0].has_avatar); // no baked thumbnail → placeholder
    }
}

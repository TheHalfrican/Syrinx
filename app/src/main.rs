//! Syrinx main window — themed shell + TTS workspace wired to the engine.
//!
//! The Slint UI runs on the main thread; a tokio worker owns the D-Bus
//! connection to `sh.syrinx.Engine1`. Theme switching and tab nav are pure UI
//! (Slint globals); voices, generate, level, and history cross the bridge.

// Release builds on Windows detach from the console — the "Syrinx (dev)"
// shortcut launches the release exe, and a stray terminal window alongside the
// app is user-visible noise. Debug builds keep stdout so `cargo run` still
// streams tracing. Linux and macOS are unaffected. (Engine stdout/err already
// go to engine.log with CREATE_NO_WINDOW, seam 1.2 — no console reappears.)
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

slint::include_modules!();

// Win/mac own the engine as a supervised child process (RPC-PROTOCOL.md §13);
// on Linux the engine belongs to systemd + D-Bus activation, so this never
// compiles there.
#[cfg(not(target_os = "linux"))]
mod engine_proc;

// Windows system-audio capture is app-side WASAPI loopback (the native twin of
// Linux's `parecord <sink>.monitor`); the engine still only does mic capture.
#[cfg(target_os = "windows")]
mod capture_win;

// Windows global dictation: a hotkey-driven second RPC client (RPC-PROTOCOL.md
// §1). Linux uses the standalone gtk4/zbus `dictate/` crate instead, so this
// never compiles there.
#[cfg(target_os = "windows")]
mod dictation_win;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::collections::HashMap;
#[cfg(target_os = "linux")]
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
    InstallVc { setup_id: String },
    CancelVc { setup_id: String },
    // Voicebox-style composer / cards / player
    GenerateInCharacter { text: String, voice: String },
    SelectVoice { id: String },
    PickLanguage { voice: String, index: usize },
    PickEngine { voice: String, index: usize },
    // The other two category pickers. They live where the category is used —
    // whisper in the Transcription view, the LLM in Settings — and both funnel
    // into the same SetActiveModel the composer's picker uses.
    PickSttModel { index: usize },
    PickLlmModel { index: usize },
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
    /// The user accepted Vevo2's whisper-medium download in the consent
    /// dialog — remember it and replay the conversion that raised it.
    Vevo2Ack,
    VcSuggestPitch { index: usize },
    VcSaveClip { name: String, transcript: String, kind: String },
    VcDeleteClip { id: String },
    VcArmClip { id: String },
    VcAudition { id: String },
    // settings (⚙ tab)
    SettingsLoad,
    SaveTheme { theme: String },
    StPickMic { index: usize },
    StMicTestToggle,
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
    /// Non-Linux HiDPI escape hatch. Forces the winit scale factor (via
    /// SLINT_SCALE_FACTOR) at startup. `0.0` = unset → the app compensates the
    /// OS scale down to 1.0 so the perceived density matches the Linux
    /// reference. Set e.g. `1.25` for a slightly larger UI. File-only for now
    /// (`%APPDATA%\syrinx\settings.json`); applies on the next launch. Ignored
    /// on Linux, where native scaling is left completely untouched.
    #[serde(default)]
    ui_scale: f32,
    /// One-shot consent for Vevo2's whisper-medium content encoder (~1.5 GB).
    /// It lives inside Amphion's own cache rather than the Models catalog, so
    /// no Download button can ever cover it — this dialog is the only place the
    /// user gets to spend that disk on purpose. Sticky once accepted.
    #[serde(default)]
    vevo2_whisper_ack: bool,
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
            ui_scale: 0.0,
            vevo2_whisper_ack: false,
        }
    }
}

fn config_path() -> std::path::PathBuf {
    // `dirs::config_dir()` is XDG_CONFIG_HOME-aware with the same ~/.config
    // default on Linux (byte-identical to before), and %APPDATA%\syrinx on
    // Windows. The HOME/.config fallback preserves the prior behavior if the
    // crate can't resolve a base dir.
    dirs::config_dir()
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        })
        .join("syrinx")
        .join("settings.json")
}

/// The engine's data root, resolved to match the Python engine byte-for-byte
/// (`engine/syrinx_engine/paths.py`): `SYRINX_DATA_DIR` wins everywhere, else
/// the per-OS default. Linux keeps the historical `~/.local/share/syrinx`,
/// deliberately ignoring XDG_DATA_HOME exactly as the engine does; Win/mac use
/// the §2.2 data dir that also roots the RPC discovery file (see
/// `shared::rpc_client`'s `default_discovery_dir`).
fn engine_data_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SYRINX_DATA_DIR") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    #[cfg(target_os = "windows")]
    {
        // %LOCALAPPDATA%\syrinx\syrinx
        dirs::data_local_dir()
            .map(|d| d.join("syrinx").join("syrinx"))
            .unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    {
        // ~/Library/Application Support/syrinx
        dirs::data_dir().map(|d| d.join("syrinx")).unwrap_or_default()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // Byte-identical to the engine's hand-rolled Linux literal (ignores
        // XDG_DATA_HOME, which dirs::data_dir() would otherwise honor).
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".local")
            .join("share")
            .join("syrinx")
    }
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

    // --- HiDPI scale compensation (non-Linux only) ---
    // The UI was authored at the Hyprland density (scale ≈ 1.0). Windows
    // reports the monitor's *user* scaling (e.g. 1.5–2.0), which makes every
    // element that much larger — the "zoomed in, cramped" report. Force the
    // winit backend's scale factor via SLINT_SCALE_FACTOR *before* the window
    // exists (the only reliable override in slint 1.17; a runtime
    // ScaleFactorChanged event gets reverted by the next monitor event).
    // Windows' default target is 1.0 = match the Linux reference.
    //
    // macOS is NOT that case: a Retina factor of 2.0 is backing-store pixel
    // density, not user scaling — points are already the Linux-reference
    // density. Pinning it to 1.0 makes winit read the authored logical sizes as
    // physical pixels and the window opens at half its size, so mac stays
    // native and the env var is not set at all. `ui_scale` in settings.json
    // still overrides on both, and only then does mac force anything.
    // Linux never enters this block: no env var, native scaling untouched.
    #[cfg(not(target_os = "linux"))]
    let (ui_scale_cfg, scale_target) = {
        let cfg = load_config();
        let target = if cfg.ui_scale > 0.0 {
            Some(cfg.ui_scale)
        } else if cfg!(target_os = "macos") {
            None
        } else {
            Some(1.0)
        };
        if let Some(target) = target {
            std::env::set_var("SLINT_SCALE_FACTOR", format!("{target}"));
        }
        (cfg.ui_scale, target)
    };

    let ui = AppWindow::new()?;

    // winit has made the process per-monitor-DPI-aware by now, so os_native_scale
    // reads true (independent of SLINT_SCALE_FACTOR). The actual winit window is
    // only created once the event loop runs, so scale_factor() here would still
    // read the pre-creation default — a single-shot timer logs the applied value
    // after first paint instead. Then register the bundled fallback fonts.
    #[cfg(not(target_os = "linux"))]
    {
        match scale_target {
            Some(target) => tracing::info!(
                "ui-scale: os-native≈{:.3} → forcing effective={:.3} via SLINT_SCALE_FACTOR (ui_scale cfg={}, {})",
                os_native_scale(),
                target,
                ui_scale_cfg,
                if ui_scale_cfg > 0.0 { "override" } else { "default→1.0" },
            ),
            None => tracing::info!(
                "ui-scale: os-native≈{:.3} → left native, no SLINT_SCALE_FACTOR (ui_scale cfg={})",
                os_native_scale(),
                ui_scale_cfg,
            ),
        }
        register_fallback_fonts();
        let w = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(700), move || {
            if let Some(ui) = w.upgrade() {
                tracing::info!("ui-scale: applied window scale_factor={:.3}", ui.window().scale_factor());
            }
        });
    }

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
        // The VC engine consent modal's winget paragraph (Python 3.12 + Git)
        // only applies to Windows.
        ui.set_is_windows(cfg!(target_os = "windows"));
        // System-audio capture exists on Linux (parecord monitor) and Windows
        // (WASAPI loopback); macOS waits for phase 3. Gates the ◉ Record-system
        // buttons, the create-voice System chip, and the ⚙ tap picker.
        ui.set_system_capture_supported(cfg!(any(target_os = "linux", target_os = "windows")));
        // Dictation ships on Linux (syrinx-dictate) and Windows (the global
        // hotkey thread below); macOS waits for phase 3. Gates the whole ⚙
        // DICTATION card rather than just the Hyprland bind hint.
        ui.set_dictation_supported(cfg!(any(target_os = "linux", target_os = "windows")));
        // The ⚙ mic test has a level source on every platform: Win/mac read the
        // §14 engine recorder's RecordingLevel, Linux meters its own parecord
        // child app-side (the same capture path its real recordings use, so the
        // pactl source ids the engine's PortAudio recorder can't resolve never
        // come up).
        ui.set_st_mic_test_supported(true);
        // Decorative titlebar chrome, per OS. Linux keeps the authored
        // "hyprland · workspace 3" (the slint default); Win/mac get an
        // equivalently subtle, lowercase string.
        #[cfg(target_os = "windows")]
        ui.set_desktop_chrome("windows 11 · desktop 1".into());
        #[cfg(target_os = "macos")]
        ui.set_desktop_chrome("macos · space 1".into());
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
        let ui_weak = ui.as_weak();
        ui.on_download_model(move |id| {
            // A previous failure's banner says "click Download to resume" — so
            // clicking it has to take that banner down, exactly as Install
            // clears its own. Otherwise the retry starts under a warning about
            // the attempt it just replaced.
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_vc_install_error("".into());
            }
            let _ = tx.send(Cmd::DownloadModel { id: id.to_string() });
        });
    }
    {
        let tx = tx.clone();
        ui.on_delete_model(move |id| { let _ = tx.send(Cmd::DeleteModel { id: id.to_string() }); });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_install_vc(move |setup_id| {
            // arm the row's marquee before the round-trip — the engine's first
            // VcSetupProgress can be a minute out (venv creation), and a dead
            // Install button in the meantime reads as a no-op
            let ui = ui_weak.unwrap();
            ui.set_vc_install_error("".into());
            ui.set_vc_install_active(setup_id.clone());
            ui.set_vc_install_stage("starting…".into());
            let _ = tx.send(Cmd::InstallVc { setup_id: setup_id.to_string() });
        });
    }
    {
        let tx = tx.clone();
        ui.on_cancel_vc(move |setup_id| {
            let _ = tx.send(Cmd::CancelVc { setup_id: setup_id.to_string() });
        });
    }
    {
        let tx = tx.clone();
        ui.on_vevo2_ack(move || { let _ = tx.send(Cmd::Vevo2Ack); });
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
        ui.on_st_mic_test_toggle(move || { let _ = tx.send(Cmd::StMicTestToggle); });
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
    // The STT and LLM pickers — same dropdown-Cmd shape, one per category, each
    // living in the view that uses it (Transcription / Settings).
    {
        let tx = tx.clone();
        ui.on_pick_stt_model(move |i| {
            let _ = tx.send(Cmd::PickSttModel { index: i.max(0) as usize });
        });
    }
    {
        let tx = tx.clone();
        ui.on_pick_llm_model(move |i| {
            let _ = tx.send(Cmd::PickLlmModel { index: i.max(0) as usize });
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
            // ♫ octave dropdown: index 0..4 → −2..+2 octaves, in semitones;
            // speech semitone dropdown: index 0..12 → −6..+6 st (fine-tune)
            let semitones = if mode == "music" {
                (ui.get_vc_octave_index().clamp(0, 4) - 2) * 12
            } else {
                semitone_index_to_st(ui.get_vc_semitones_index())
            };
            let _ = tx.send(Cmd::VcConvert {
                index: i.max(0) as usize, engine_index, label, transcript, mode, semitones,
            });
        });
    }
    {
        let tx = tx.clone();
        ui.on_vc_suggest_pitch(move |i| {
            let _ = tx.send(Cmd::VcSuggestPitch { index: i.max(0) as usize });
        });
    }
    {
        let tx = tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_vc_save_clip(move || {
            let ui = ui_weak.unwrap();
            let name = ui.get_vc_clip_name().to_string();
            let transcript = ui.get_vc_transcript().to_string();
            let kind = ui.get_vc_mode().to_string();  // "speech" | "music" at save time
            let _ = tx.send(Cmd::VcSaveClip { name, transcript, kind });
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

    // Arm Windows global dictation on its own hotkey thread (its own RPC client).
    // Non-fatal and off the main loop — never blocks startup or the UI.
    #[cfg(target_os = "windows")]
    dictation_win::spawn();

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

/// The monitor's OS scale factor, for the diagnostic scale log. Read *after*
/// window creation (winit has set per-monitor-DPI awareness by then, so the
/// value is real, not clamped to 96 dpi); SLINT_SCALE_FACTOR does not affect it.
#[cfg(target_os = "windows")]
fn os_native_scale() -> f64 {
    // user32!GetDpiForSystem (Win10 1607+): 96 dpi == scale 1.0.
    extern "system" {
        fn GetDpiForSystem() -> u32;
    }
    (unsafe { GetDpiForSystem() } as f64) / 96.0
}
#[cfg(target_os = "macos")]
fn os_native_scale() -> f64 {
    // NSScreen.backingScaleFactor: 2.0 on Retina. Unlike the Windows DPI ratio
    // this is pixel density, not user scaling, so it is expected to stay 2.0
    // while the applied window scale factor below reads the same — see the
    // SLINT_SCALE_FACTOR note in main(). Main-thread-only API; every caller is
    // on the UI thread, and a headless/screen-less session yields NaN.
    objc2::MainThreadMarker::new()
        .and_then(objc2_app_kit::NSScreen::mainScreen)
        .map(|s| s.backingScaleFactor())
        .unwrap_or(f64::NAN)
}
#[cfg(all(not(target_os = "linux"), not(target_os = "windows"), not(target_os = "macos")))]
fn os_native_scale() -> f64 {
    // no cheap pre-query elsewhere; the effective window scale in the same log
    // line is the meaningful number there.
    f64::NAN
}

/// Bundle DejaVu Sans (broad symbol coverage) plus a tiny merged Noto subset
/// (⏸ ⏻ ⧉ ＋ ⌖ 🗑 — the glyphs DejaVu lacks) as *fallback* fonts. On Linux
/// fontconfig already resolves these symbols to DejaVu; the femtovg text stack
/// on Windows/mac does not fall back to system fonts, so the glyphs render as
/// tofu. Registering as fallback only (not as the default family) means the
/// themes' own font choices (Tahoma for the '95 skin, etc.) keep winning for
/// every glyph they cover — only genuinely-missing glyphs reach these fonts.
///
/// Slint's shaper groups Common-script symbols with the surrounding run (Latin
/// for the UI's text) or keeps them Common when a Text holds only a glyph, so
/// the fallback is appended for both "Latn" and "Zyyy".
///
/// Gated off Linux entirely so Linux keeps its exact fontconfig glyph selection.
#[cfg(not(target_os = "linux"))]
fn register_fallback_fonts() {
    use slint::fontique_010::fontique;
    let mut collection = slint::fontique_010::shared_collection();
    for bytes in [
        include_bytes!("../ui/fonts/DejaVuSans.ttf").as_slice(),
        include_bytes!("../ui/fonts/SyrinxFallback.ttf").as_slice(),
    ] {
        let blob = fontique::Blob::new(std::sync::Arc::new(bytes.to_vec()));
        let families: Vec<_> = collection
            .register_fonts(blob, None)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for script in ["Latn", "Zyyy"] {
            collection.append_fallbacks(
                fontique::FallbackKey::new(fontique::Script::from_str_unchecked(script), None),
                families.iter().copied(),
            );
        }
    }
    tracing::info!("registered bundled fallback fonts (DejaVu Sans + Syrinx symbols)");
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

/// Scratch WAV path for app-side capture. On Linux the runtime dir is a tmpfs
/// (RAM, cleaned on logout) with `/tmp` as the fallback; elsewhere `XDG_RUNTIME_DIR`
/// is unset and `/tmp` is not a real directory, so the platform temp dir is the
/// only path `File::create` can open. The directory is never created — both
/// branches resolve to one the OS already guarantees.
fn scratch_wav(name: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        std::env::var("XDG_RUNTIME_DIR")
            .map(|d| format!("{d}/{name}"))
            .unwrap_or_else(|_| format!("/tmp/{name}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::temp_dir().join(name).to_string_lossy().to_string()
    }
}

/// The default sink's `.monitor` source — a passive tap for system audio (the
/// same approach as Voicebox). Works on analog/HDMI; Bluetooth A2DP monitors are
/// silent, so those need a speaker/HDMI output while capturing.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

// --- capture seam (1.3) --------------------------------------------------
//
// The app-side interface the record buttons drive. On Linux it wraps the native
// `parecord` child writing to an app-chosen wav (byte-identical to before); on
// Windows a `system` capture wraps a native WASAPI loopback thread (the twin of
// parecord) while the mic still goes through the engine (sounddevice); on macOS
// the engine owns every capture and system audio is hidden until phase 3.

/// A live capture. Linux owns the `parecord` child; on Windows it is either the
/// engine's mic recording id or a WASAPI loopback for system audio; on macOS it
/// holds the engine's recording id (the engine owns the wav, path on stop).
#[cfg(target_os = "linux")]
struct Capture(tokio::process::Child);
#[cfg(target_os = "windows")]
enum Capture {
    Engine(String),                 // mic via the engine (rec_id)
    Loopback(capture_win::Loopback), // system audio via WASAPI loopback
}
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
struct Capture {
    rec_id: String,
}

/// Start a capture. `device` is a source name (`.monitor` for system on Linux,
/// a render-endpoint id for system on Windows, a mic name otherwise);
/// `None`/`""` = the platform default. `system` routes Windows to WASAPI
/// loopback instead of the engine's mic path; Linux/macOS already encode the
/// choice in `device`.
#[cfg(target_os = "linux")]
async fn capture_start(
    _proxy: &EngineClient,
    wav: &str,
    device: Option<&str>,
    _system: bool,
) -> std::io::Result<Capture> {
    start_pw_record(wav, device).await.map(Capture)
}
#[cfg(target_os = "windows")]
async fn capture_start(
    proxy: &EngineClient,
    wav: &str,
    device: Option<&str>,
    system: bool,
) -> std::io::Result<Capture> {
    if system {
        // WASAPI loopback runs entirely app-side; setup is a fast, blocking
        // handshake (the same shape as spawning parecord on Linux).
        return capture_win::Loopback::start(device, wav).map(Capture::Loopback);
    }
    match proxy.start_recording(device.unwrap_or("")).await {
        Ok(id) if !id.is_empty() => Ok(Capture::Engine(id)),
        Ok(_) => Err(std::io::Error::other("device missing or busy")),
        Err(e) => Err(std::io::Error::other(e.to_string())),
    }
}
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
async fn capture_start(
    proxy: &EngineClient,
    _wav: &str,
    device: Option<&str>,
    _system: bool,
) -> std::io::Result<Capture> {
    match proxy.start_recording(device.unwrap_or("")).await {
        Ok(id) if !id.is_empty() => Ok(Capture { rec_id: id }),
        Ok(_) => Err(std::io::Error::other("device missing or busy")),
        Err(e) => Err(std::io::Error::other(e.to_string())),
    }
}

/// Stop + finalize a capture; returns the finalized WAV path (`""` on failure).
/// On Linux the file is already at `wav`, so it is returned unchanged (the
/// failure branch is dead there, keeping behavior byte-identical).
#[cfg(target_os = "linux")]
async fn capture_stop(mut cap: Capture, _proxy: &EngineClient, wav: &str) -> String {
    stop_pw_record(&mut cap.0).await;
    wav.to_string()
}
#[cfg(target_os = "windows")]
async fn capture_stop(cap: Capture, proxy: &EngineClient, _wav: &str) -> String {
    match cap {
        Capture::Engine(id) => proxy.stop_recording(&id).await.unwrap_or_default(),
        Capture::Loopback(h) => h.stop(),
    }
}
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
async fn capture_stop(cap: Capture, proxy: &EngineClient, _wav: &str) -> String {
    proxy.stop_recording(&cap.rec_id).await.unwrap_or_default()
}

/// Discard a capture without keeping the file (modal cancel). Linux reaps the
/// child (the scratch wav is overwritten by the next take); elsewhere the
/// engine deletes its scratch recording (or the loopback drops its wav).
#[cfg(target_os = "linux")]
async fn capture_discard(mut cap: Capture, _proxy: &EngineClient) {
    stop_pw_record(&mut cap.0).await;
}
#[cfg(target_os = "windows")]
async fn capture_discard(cap: Capture, proxy: &EngineClient) {
    match cap {
        Capture::Engine(id) => {
            let _ = proxy.cancel_recording(&id).await;
        }
        Capture::Loopback(h) => h.discard(),
    }
}
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
async fn capture_discard(cap: Capture, proxy: &EngineClient) {
    let _ = proxy.cancel_recording(&cap.rec_id).await;
}

/// Did the capture terminate on its own? Linux polls the `parecord` child;
/// Windows loopback polls its drain thread's error flag; the engine mic path
/// instead surfaces death as a `StopRecording` returning `""`.
#[cfg(target_os = "linux")]
fn capture_died(cap: &mut Capture) -> bool {
    matches!(cap.0.try_wait(), Ok(Some(_)))
}
#[cfg(target_os = "windows")]
fn capture_died(cap: &mut Capture) -> bool {
    match cap {
        Capture::Engine(_) => false,
        Capture::Loopback(h) => h.died(),
    }
}
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
fn capture_died(_cap: &mut Capture) -> bool {
    false
}

/// Resolve the capture device for a record request. `system` taps the output
/// (Linux monitor / Windows render endpoint); mic uses the ⚙ mic choice
/// (`""` = default). Returns `(device, ok)` — `ok == false` means "system was
/// requested but no monitor exists" (Linux only; Windows always has a default
/// render endpoint, and the loopback surfaces its own errors).
#[cfg(target_os = "linux")]
async fn resolve_capture_device(cfg: &AppConfig, system: bool) -> (Option<String>, bool) {
    if system {
        let d = if cfg.monitor_device.is_empty() {
            default_monitor().await
        } else {
            Some(cfg.monitor_device.clone())
        };
        let ok = d.is_some();
        (d, ok)
    } else if cfg.mic_device.is_empty() {
        (None, true)
    } else {
        (Some(cfg.mic_device.clone()), true)
    }
}
#[cfg(target_os = "windows")]
async fn resolve_capture_device(cfg: &AppConfig, system: bool) -> (Option<String>, bool) {
    if system {
        // The ⚙ "System tap" choice is a render-endpoint id; "" = default.
        let d = (!cfg.monitor_device.is_empty()).then(|| cfg.monitor_device.clone());
        (d, true)
    } else if cfg.mic_device.is_empty() {
        (None, true)
    } else {
        (Some(cfg.mic_device.clone()), true)
    }
}
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
async fn resolve_capture_device(cfg: &AppConfig, _system: bool) -> (Option<String>, bool) {
    // System capture is hidden on macOS; always the mic.
    if cfg.mic_device.is_empty() {
        (None, true)
    } else {
        (Some(cfg.mic_device.clone()), true)
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

/// Speech pitch fine-tune: the 13-entry dropdown (index 0..12) maps to −6..+6
/// semitones, with index 6 = ±0. Beyond ±6 the shift artifacts outweigh the
/// register match for speech (music keeps the coarser octave tool).
const VC_SEMITONE_LIMIT: i32 = 6;

/// Dropdown index → semitones. Clamped so a stray index can't over-shift.
fn semitone_index_to_st(index: i32) -> i32 {
    index.clamp(0, 2 * VC_SEMITONE_LIMIT) - VC_SEMITONE_LIMIT
}

/// A suggested/selected semitone value → dropdown index (inverse of the above),
/// clamping the engine's suggestion into the ±6 the control can display.
fn st_to_semitone_index(st: i32) -> i32 {
    st.clamp(-VC_SEMITONE_LIMIT, VC_SEMITONE_LIMIT) + VC_SEMITONE_LIMIT
}

/// The ⇄ speech dropdown, in order: (conversion engine, the catalog row that
/// engine loads in that mode). Mirrors the engine's `models.VC_ROW_FOR` — the
/// dropdown had hardcoded labels before, which is how vevo2-singing's row state
/// ended up consulted by nothing at all. Cross-tested against the engine map.
const VC_SPEECH_ROWS: &[(&str, &str)] = &[
    ("chatterbox_vc", "chatterbox-vc"),
    ("seed_vc", "seed-vc"),
    ("vevo_timbre", "vevo-timbre"),
];
/// The ⇄ music dropdown (singing-capable engines only). `vevo_timbre` loads a
/// DIFFERENT row here — that asymmetry is the whole reason this is a table of
/// pairs rather than a list of engine ids.
const VC_MUSIC_ROWS: &[(&str, &str)] = &[("seed_vc", "seed-vc"), ("vevo_timbre", "vevo2-singing")];

/// Labels for one ⇄ engine dropdown, read off the real catalog rows so an
/// engine that isn't installed or downloaded says so in the picker instead of
/// failing at Convert time (requirement 7: unready models stay visible).
/// A pair with no catalog row falls back to its engine id — a picker slot that
/// silently vanished would shift every index below it.
fn vc_row_labels(vc: &[ModelItem], pairs: &[(&str, &str)]) -> Vec<SharedString> {
    pairs
        .iter()
        .map(|(engine, row)| {
            match vc.iter().find(|m| m.id == *row) {
                Some(m) => format!(
                    "{}{}",
                    m.display,
                    readiness_suffix(m.downloaded, m.needs_setup)
                )
                .into(),
                None => SharedString::from(*engine),
            }
        })
        .collect()
}

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

// --- model selection: one authority per category ---------------------------
//
// The Models tab is an INVENTORY (install engines, download and delete
// weights). What actually runs is chosen where it is used: the composer's model
// dropdown for speech, the Transcription view's picker for whisper, Settings
// for the LLM. Everything below serves that rule.

/// One catalog row, in the shape the pickers need. Named for its first user —
/// the composer's voice-model dropdown — but the STT and LLM pickers are built
/// from the same five fields.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VoiceRow {
    id: String,
    engine: String,
    display: String,
    downloaded: bool,
    /// the engine behind this row still needs its one-time local install
    needs_setup: bool,
}

/// The engines that can speak a CLONED voice.
///
/// Mirrors the engine's `tts.CLONING_ENGINES` exactly; the Python suite pins
/// the same five names, so a new cloning engine that lands on one side and not
/// the other fails a test rather than quietly disappearing from the composer.
const CLONING_ENGINES: &[&str] = &["qwen", "luxtts", "chatterbox", "chatterbox_turbo", "tada"];

fn is_cloning_engine(engine: &str) -> bool {
    CLONING_ENGINES.contains(&engine)
}

/// Can this row run right now? The load-bearing definition: weights on disk AND
/// the engine installed. LuxTTS made the second half real — it can be fully
/// downloaded and still not run until its venv exists.
fn row_ready(downloaded: bool, needs_setup: bool) -> bool {
    downloaded && !needs_setup
}

/// The honest tail on a picker label; "" when the row is ready to run.
///
/// `needs_setup` wins over `not downloaded`: weights are useless without the
/// engine, so the install is the one next step worth naming.
fn readiness_suffix(downloaded: bool, needs_setup: bool) -> &'static str {
    if needs_setup {
        " — needs setup"
    } else if !downloaded {
        " — not downloaded"
    } else {
        ""
    }
}

/// A picker entry: the catalog display plus what is missing, if anything.
fn option_label(r: &VoiceRow) -> String {
    format!("{}{}", r.display, readiness_suffix(r.downloaded, r.needs_setup))
}

/// The row that best represents `engine` — a downloaded one if there is one,
/// else the engine's first row.
///
/// The preference matters. The engine's own `require_weights` resolves a
/// sizeless request to the catalog's FIRST row, so a CustomVoice user who
/// downloaded only 0.6B would otherwise be shown "Qwen CustomVoice 1.7B" and
/// coachmarked onto a row they have no reason to fetch. Where something IS on
/// disk, that is the row the user means.
fn engine_row<'a>(rows: &'a [VoiceRow], engine: &str) -> Option<&'a VoiceRow> {
    rows.iter()
        .find(|r| r.engine == engine && r.downloaded)
        .or_else(|| rows.iter().find(|r| r.engine == engine))
}

/// The rows the composer's model dropdown may offer for the selected voice.
///
/// `locked` = the one engine this voice can ever speak on (Kokoro presets,
/// preset profiles): the dropdown is replaced by a read-only field, so it has
/// nothing to offer. Everything else is a cloned profile, and a cloned profile
/// may only be offered engines that can clone — picking Kokoro for one used to
/// crash generation.
fn composer_options(rows: &[VoiceRow], locked: Option<&str>) -> Vec<VoiceRow> {
    if locked.is_some() {
        return Vec::new();
    }
    rows.iter().filter(|r| is_cloning_engine(&r.engine)).cloned().collect()
}

/// Which option the composer lands on when a voice is selected.
///
/// The authorities, strongest first:
///  1. what the user picked for THIS voice in THIS session — an explicit
///     choice, honored even if the weights aren't there (the label says so and
///     Generate raises the notice);
///  2. the profile's `default_engine` SEED, preferring the engine's active
///     model when it belongs to that engine, else that engine's first ready
///     row. A pin whose engine has nothing runnable falls through rather than
///     parking the dropdown on weights nobody has;
///  3. the engine's active voice model;
///  4. the first row that can actually run.
fn seed_index(opts: &[VoiceRow], session: Option<&str>, pin: &str, active_id: &str) -> i32 {
    let pos = |id: &str| opts.iter().position(|r| r.id == id);
    let ready_pos = |id: &str| pos(id).filter(|i| row_ready(opts[*i].downloaded, opts[*i].needs_setup));
    if let Some(i) = session.and_then(pos) {
        return i as i32;
    }
    if !pin.is_empty() {
        if let Some(i) = ready_pos(active_id).filter(|i| opts[*i].engine == pin) {
            return i as i32;
        }
        if let Some(i) = opts
            .iter()
            .position(|r| r.engine == pin && row_ready(r.downloaded, r.needs_setup))
        {
            return i as i32;
        }
    }
    if let Some(i) = ready_pos(active_id) {
        return i as i32;
    }
    opts.iter()
        .position(|r| row_ready(r.downloaded, r.needs_setup))
        .unwrap_or(0) as i32
}

/// Build the three category model lists from the engine's ListModels JSON.
fn build_models(json: &str) -> (Vec<ModelItem>, Vec<ModelItem>, Vec<ModelItem>, Vec<ModelItem>) {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    let (mut voice, mut stt, mut llm, mut vc) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for m in arr.iter() {
        let s = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let b = |k: &str| m.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let mb = m.get("size_mb").and_then(|v| v.as_i64()).unwrap_or(0);
        let category = m.get("category").and_then(|v| v.as_str()).unwrap_or("");
        // "IN USE" on a voice row means "this is what a cloned voice speaks
        // on". models.json ships with `{"voice": "kokoro"}` as its factory
        // default, and Kokoro cannot clone anything — so that stale value must
        // not light a chip. Non-cloning voice engines simply have no IN USE
        // state to show; the other categories keep theirs verbatim.
        let active = b("active")
            && (category != "voice" || is_cloning_engine(m.get("engine").and_then(|v| v.as_str()).unwrap_or("")));
        let item = ModelItem {
            id: s("id").into(),
            display: s("display").into(),
            size_label: size_label(mb).into(),
            description: s("description").into(),
            downloaded: b("downloaded"),
            downloading: b("downloading"),
            active,
            supported: b("supported"),
            warning: s("warning").into(),
            progress: 0.0,
            finalizing: false,
            // VC engines only; absent (⇒ false/"") on every other row
            needs_setup: b("needs_setup"),
            setup_id: s("setup_id").into(),
        };
        match category {
            "voice" => voice.push(item),
            "stt" => stt.push(item),
            "llm" => llm.push(item),
            "vc" => vc.push(item),
            _ => {}
        }
    }
    (voice, stt, llm, vc)
}

/// The conversion engines that still need their one-time local install, as a
/// human phrase for the ⇄ Voice Converter notice ("" = nothing missing).
///
/// Keyed by *setup* id rather than by row: Vevo ships as two catalog entries
/// (timbre + singing) sharing one install, so it must be named once. Rows with
/// no setup id (Chatterbox-VC, bundled) never count — the view always works.
fn missing_vc_engines(vc: &[ModelItem]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for m in vc.iter().filter(|m| m.needs_setup) {
        let id = m.setup_id.as_str();
        if id.is_empty() || seen.contains(&id) {
            continue;
        }
        seen.push(id);
        names.push(
            match id {
                // Seed-VC is the pick of the bunch — say so, but only here,
                // where it is actually one of the things left to install
                "seedvc" => "Seed-VC (recommended)",
                "vevo" => "Vevo",
                // a future engine we have no short name for — the row's own
                // label beats printing a raw setup id at the user
                _ => m.display.as_str(),
            }
            .to_string(),
        );
    }
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, head)) => format!("{} and {last}", head.join(", ")),
    }
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

/// One catalog refresh, in the shapes the worker keeps between commands.
struct Catalog {
    /// every SUPPORTED voice row, downloaded or not — the pickers show unready
    /// models with a "— not downloaded" tail rather than hiding them
    voice: Vec<VoiceRow>,
    stt: Vec<VoiceRow>,
    llm: Vec<VoiceRow>,
    /// the ⇄ conversion rows, for Convert's readiness pre-check
    vc: Vec<ModelItem>,
    /// the engine's active model id per category ("" = none recorded)
    active_voice: String,
    active_stt: String,
    active_llm: String,
}

/// SetActiveModel — the one call that changes what a category runs on.
///
/// Every picker funnels through here: the composer's model dropdown, the
/// Transcription view's whisper picker, Settings' LLM picker. Nothing else in
/// the app calls `set_active_model` any more, which is the whole point — there
/// used to be three authorities and they disagreed.
async fn apply_active_model(proxy: &EngineClient, id: &str) {
    if let Err(e) = proxy.set_active_model(id).await {
        tracing::error!("set_active_model failed: {id}: {e}");
    }
}

/// Where a category picker lands with no per-voice seed to honor: the active
/// model if it can run, else the first row that can.
fn picker_index(rows: &[VoiceRow], active_id: &str) -> i32 {
    seed_index(rows, None, "", active_id)
}

/// Re-fetch the catalog + hardware and push into the UI: the four Models-tab
/// lists, the ⇄ engine labels, the STT and LLM pickers, and the create-voice
/// modal's seed options.
///
/// It deliberately does NOT touch `composer-engines` / `composer-engine-index`.
/// Those belong to `apply_composer_engines`, which knows which voice is
/// selected; two writers racing over the composer's dropdown is exactly the bug
/// this redesign removes.
async fn refresh_models(ui: &slint::Weak<AppWindow>, proxy: &EngineClient) -> Catalog {
    let models_json = proxy.list_models().await.unwrap_or_else(|_| "[]".into());
    let hw_json = proxy.hardware().await.unwrap_or_default();
    let (voice, stt, llm, vc_conv) = build_models(&models_json);
    let hwline = hardware_line(&hw_json);
    // ⇄ view notice — recomputed on every refresh, so a finished install clears
    // it without a restart (refresh_models runs when an install completes)
    let vc_missing = missing_vc_engines(&vc_conv);
    // ⇄ dropdown labels, read off the real rows (VC_ROW_FOR's app-side twin)
    let vc_speech = vc_row_labels(&vc_conv, VC_SPEECH_ROWS);
    let vc_music = vc_row_labels(&vc_conv, VC_MUSIC_ROWS);

    let arr: Vec<serde_json::Value> = serde_json::from_str(&models_json).unwrap_or_default();
    let (mut v_rows, mut s_rows, mut l_rows) = (Vec::new(), Vec::new(), Vec::new());
    let (mut active_voice, mut active_stt, mut active_llm) =
        (String::new(), String::new(), String::new());
    for m in &arr {
        let s = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let b = |k: &str| m.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        // "engine soon" rows have no backend at all — a picker slot for one
        // would be an offer nothing can honor, download or no download
        if !b("supported") {
            continue;
        }
        let row = VoiceRow {
            id: s("id").to_string(),
            engine: s("engine").to_string(),
            display: s("display").to_string(),
            downloaded: b("downloaded"),
            needs_setup: b("needs_setup"),
        };
        match s("category") {
            "voice" => {
                if b("active") {
                    active_voice = row.id.clone();
                }
                v_rows.push(row);
            }
            "stt" => {
                if b("active") {
                    active_stt = row.id.clone();
                }
                s_rows.push(row);
            }
            "llm" => {
                if b("active") {
                    active_llm = row.id.clone();
                }
                l_rows.push(row);
            }
            _ => {}
        }
    }
    let stt_labels: Vec<SharedString> = s_rows.iter().map(|r| option_label(r).into()).collect();
    let llm_labels: Vec<SharedString> = l_rows.iter().map(|r| option_label(r).into()).collect();
    let stt_idx = picker_index(&s_rows, &active_stt);
    let llm_idx = picker_index(&l_rows, &active_llm);
    // the ✎ Compose / Rewrite / Refine tooltips name the model they will use
    let llm_label = l_rows
        .get(llm_idx.max(0) as usize)
        .map(|r| r.display.clone())
        .unwrap_or_default();
    // create-voice modal: the optional seed. Option 0 follows the composer;
    // the rest are the engines a cloned voice can actually speak on.
    let cv_options: Vec<SharedString> = std::iter::once(SharedString::from("Follow the composer"))
        .chain(composer_options(&v_rows, None).iter().map(|r| option_label(r).into()))
        .collect();
    let out = Catalog {
        voice: v_rows,
        stt: s_rows,
        llm: l_rows,
        vc: vc_conv.clone(),
        active_voice,
        active_stt,
        active_llm,
    };

    ui.upgrade_in_event_loop(move |ui| {
        ui.set_voice_models(ModelRc::from(Rc::new(VecModel::from(voice))));
        ui.set_stt_models(ModelRc::from(Rc::new(VecModel::from(stt))));
        ui.set_llm_models(ModelRc::from(Rc::new(VecModel::from(llm))));
        ui.set_vc_conv_models(ModelRc::from(Rc::new(VecModel::from(vc_conv))));
        ui.set_vc_engines_missing(vc_missing.into());
        ui.set_hardware_line(hwline.into());
        ui.set_vc_engine_names(ModelRc::from(Rc::new(VecModel::from(vc_speech))));
        ui.set_vc_music_engine_names(ModelRc::from(Rc::new(VecModel::from(vc_music))));
        ui.set_stt_options(ModelRc::from(Rc::new(VecModel::from(stt_labels))));
        ui.set_stt_index(stt_idx);
        ui.set_llm_options(ModelRc::from(Rc::new(VecModel::from(llm_labels))));
        ui.set_llm_index(llm_idx);
        ui.set_llm_model_label(llm_label.into());
        ui.set_cv_model_options(ModelRc::from(Rc::new(VecModel::from(cv_options))));
    })
    .ok();
    out
}

/// What the composer ended up set to after `apply_composer_engines`.
struct ComposerPick {
    /// dropdown rows in order — index → model id for `Cmd::PickEngine`
    rows: Vec<VoiceRow>,
    /// the engine that will actually speak (the lock, else the seeded row's)
    engine: String,
    /// the row that will actually speak ("" = nothing to run this on)
    model: Option<VoiceRow>,
}

/// Push the composer's model dropdown for one voice: options, selected index
/// and the engine-locked read-only label, all in ONE event-loop closure.
///
/// Sole owner of those three properties. They only make sense together — an
/// index into last voice's option list is a lie — and the old arrangement had
/// `refresh_models` and `select-voice` both writing them, from two different
/// threads' closures, in whatever order the loop happened to run them.
fn apply_composer_engines(
    ui: &slint::Weak<AppWindow>,
    rows: &[VoiceRow],
    voice_id: &str,
    profile_json: &str,
    session: Option<&str>,
    active_id: &str,
) -> ComposerPick {
    let lock = locked_engine(voice_id, profile_json);
    let opts = composer_options(rows, lock.as_deref());
    let pin = serde_json::from_str::<serde_json::Value>(profile_json)
        .ok()
        .and_then(|p| p.get("default_engine").and_then(|v| v.as_str()).map(str::to_string))
        .unwrap_or_default();
    let idx = seed_index(&opts, session, &pin, active_id);
    let picked = opts.get(idx.max(0) as usize).cloned();
    // locked voices show a flat field instead of the dropdown; the row behind
    // it is still needed, for the Generate readiness pre-check
    let (engine, model) = match &lock {
        Some(e) => (e.clone(), engine_row(rows, e).cloned()),
        None => (
            picked.as_ref().map(|r| r.engine.clone()).unwrap_or_else(|| pin.clone()),
            picked.clone(),
        ),
    };
    let lock_label = lock
        .as_deref()
        .map(|e| engine_label(rows, e))
        .unwrap_or_default();
    let labels: Vec<SharedString> = opts.iter().map(|r| option_label(r).into()).collect();
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_composer_engines(ModelRc::from(Rc::new(VecModel::from(labels))));
        ui.set_composer_engine_index(idx);
        ui.set_composer_engine_locked(lock_label.into());
    })
    .ok();
    ComposerPick { rows: opts, engine, model }
}

/// One ⇄ conversion request, held whole so the Vevo2 consent dialog can replay
/// the exact request it interrupted instead of asking the user to set it up
/// again.
#[derive(Clone)]
struct VcRequest {
    src: String,
    pid: String,
    engine: &'static str,
    label: String,
    transcript: String,
    mode: String,
    semitones: i32,
}

impl VcRequest {
    /// Vevo2 (singing) pulls whisper-medium as its content encoder — the one
    /// multi-GB fetch in this app that no catalog row covers.
    fn needs_vevo2_consent(&self) -> bool {
        self.engine == "vevo_timbre" && self.mode == "music"
    }
}

/// Fire a ⇄ conversion and arm the view's busy state. Returns the generation
/// id, or None when the engine refused to start one.
async fn start_conversion(
    ui: &slint::Weak<AppWindow>,
    proxy: &EngineClient,
    r: &VcRequest,
) -> Option<u32> {
    match proxy
        .convert_voice(&r.src, &r.pid, r.engine, &r.label, &r.transcript, &r.mode, r.semitones)
        .await
    {
        Ok(gid) if gid != 0 => {
            ui.upgrade_in_event_loop(|ui| {
                ui.set_vc_busy(true);
                ui.set_vc_error("".into());
                ui.set_vc_status("starting…".into());
            })
            .ok();
            Some(gid)
        }
        Ok(_) => None,
        Err(e) => {
            tracing::error!("convert failed: {e}");
            ui.upgrade_in_event_loop(|ui| {
                ui.set_vc_busy(false);
                ui.set_vc_status("engine unavailable".into());
            })
            .ok();
            None
        }
    }
}

/// Take the composer back out of the optimistic state the ✦ click put it in
/// (busy flags plus the "generating…" placeholder card) without a generation
/// ever having started.
async fn cancel_pending_generation(ui: &slint::Weak<AppWindow>, proxy: &EngineClient) {
    let items = build_history(&proxy.list_history().await.unwrap_or_else(|_| "[]".into()));
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_generating(false);
        ui.set_synthesizing(false);
        ui.set_llm_busy(false);
        set_history_model(&ui, items);
    })
    .ok();
}

/// Raise the "this model isn't ready" notice in one of the three views that can
/// hit it (`where` is "tts" | "vc" | "tr"), naming the row its "Open Models →"
/// button will coachmark.
fn set_needs_model(ui: &slint::Weak<AppWindow>, place: &str, display: &str, id: &str) {
    let (place, display, id) = (place.to_string(), display.to_string(), id.to_string());
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_nm_where(place.into());
        ui.set_nm_display(display.into());
        ui.set_nm_id(id.into());
    })
    .ok();
}

/// Take the notice back down — every dispatch that IS ready clears it first, so
/// a stale one can never outlive the thing it was complaining about.
fn clear_needs_model(ui: &slint::Weak<AppWindow>) {
    ui.upgrade_in_event_loop(|ui| {
        ui.set_nm_where("".into());
        ui.set_nm_display("".into());
        ui.set_nm_id("".into());
    })
    .ok();
}

/// Readiness gate for a category picker: `Ok` to dispatch, `Err(row)` to
/// refuse and point at the row. An empty catalog passes — the engine's own
/// `require_weights` is the authority, and refusing on an empty app-side list
/// would break a raw-repo override the catalog never knew about.
fn ready_or_notice<'a>(rows: &'a [VoiceRow], active_id: &str) -> Option<&'a VoiceRow> {
    let idx = picker_index(rows, active_id);
    rows.get(idx.max(0) as usize)
        .filter(|r| !row_ready(r.downloaded, r.needs_setup))
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
    // DEFAULT MODEL, not ENGINE: this field is a seed for the composer's
    // picker, nothing more. Unset reads as an em dash — "follows" implied a
    // live link to an active model that no longer exists.
    let engine = {
        let e = s("default_engine");
        if e.is_empty() { "—".to_string() } else { e }
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
            // see inspect_profile: the column is DEFAULT MODEL now, and unset
            // is an em dash rather than a claim about what will speak
            let engine = {
                let e = s("default_engine");
                if e.is_empty() { "—".to_string() } else { e }
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

/// The one engine a voice can EVER speak on, if it has one.
///
/// Mirrors the engine's own router (`SpeechSynthesizer.synthesize`, and its
/// twin `Core._voice_meta`): a `builtin:<engine>:<voice>` id and a *preset*
/// profile always synthesize on their own engine — the active voice model has
/// no say in it. Only *cloned* profiles float, following their pin or whatever
/// clone engine is active, and only they get a real engine choice.
///
/// `None` = free to move. `profile_json` is GetProfile's payload for a profile
/// id (ignored for builtins; "" when the lookup failed — an unknown id falls
/// back to the built-in preset engine on the engine side, but we'd rather
/// under-lock than lock the composer on a guess).
fn locked_engine(voice_id: &str, profile_json: &str) -> Option<String> {
    if let Some(rest) = voice_id.strip_prefix("builtin:") {
        // builtin:<engine>:<voice> — kokoro, or an extra preset engine
        let engine = rest.split(':').next().unwrap_or("");
        return Some(if engine.is_empty() { "kokoro".into() } else { engine.to_string() });
    }
    let p: serde_json::Value = serde_json::from_str(profile_json).ok()?;
    if p.get("voice_type").and_then(|v| v.as_str())? != "preset" {
        return None;
    }
    // blank preset_engine falls through to the built-in preset engine, exactly
    // as `SpeechSynthesizer.synthesize` does for a preset profile
    let e = p.get("preset_engine").and_then(|v| v.as_str()).unwrap_or("");
    Some(if e.is_empty() { "kokoro".into() } else { e.to_string() })
}

/// Human label for an engine id: the catalog display of a voice model that
/// runs it ("Kokoro 82M"), else the raw id — an engine that isn't in the
/// catalog at all still has to read as something rather than leave the
/// composer's engine field blank.
///
/// Engines with several size rows (qwen 1.7B/0.6B, …) resolve through
/// [`engine_row`], so the label names the size the user actually has.
fn engine_label(models: &[VoiceRow], engine: &str) -> String {
    engine_row(models, engine)
        .map(|r| r.display.clone())
        .unwrap_or_else(|| engine.to_string())
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

/// How a `ModelProgress` status string maps to the Models-tab row treatment.
/// Split out from the event handler so the stage→display decision is
/// unit-testable without a running UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelProgressUi {
    /// Determinate fill + "downloading…". Also the graceful fallback for any
    /// unknown/future stage string (never panic, never drop the row).
    Downloading,
    /// Indeterminate shimmer + "finishing…": bytes are on disk, huggingface is
    /// verifying/renaming, so the ~0.999 fraction must not read as stuck.
    Finalizing,
    /// Terminal ("done"/"error") — refetch the model list.
    Terminal,
}

fn model_progress_ui(status: &str) -> ModelProgressUi {
    match status {
        "finalizing" => ModelProgressUi::Finalizing,
        "done" | "error" => ModelProgressUi::Terminal,
        // "downloading" and any unknown/future stage degrade to the bar
        _ => ModelProgressUi::Downloading,
    }
}

/// Where to send someone for the reason behind a failure the protocol reported
/// without one. On Linux the engine is a systemd user unit, so its output is in
/// the journal rather than a file.
#[cfg(target_os = "linux")]
fn engine_log_hint() -> String {
    "journalctl --user -u syrinx-engine".to_string()
}

/// Win/mac: the supervisor pipes the engine's output into `engine.log` beside
/// the discovery file, so the real path can be named outright.
#[cfg(not(target_os = "linux"))]
fn engine_log_hint() -> String {
    engine_proc::engine_log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "the engine log".to_string())
}

/// The Models-tab banner for a download that died — `ModelProgress` with
/// status `"error"`.
///
/// The signal is `(model_id, pct, status)` and carries no message (the counts
/// are pinned), so the reason only ever reaches the engine log. Until this
/// existed, a failed download was the app's last silent failure: the row's bar
/// flashed to 0% and reverted, which looks exactly like a click that didn't
/// register. So the banner does the three things the signal can't — name the
/// model in the words the catalog uses, say where the reason is, and say that
/// Download picks up where it stopped (huggingface_hub resumes; the partial
/// blobs on disk are not wasted).
///
/// `rows` is the catalog flattened to (id, display); an id with no row left —
/// a fetch that outlived a catalog refresh — is named by its raw id rather than
/// being reported as a blank.
fn model_download_error(model_id: &str, rows: &[(&str, &str)], log_hint: &str) -> String {
    let name = rows
        .iter()
        .find(|(id, display)| *id == model_id && !display.is_empty())
        .map(|(_, display)| *display)
        .unwrap_or(model_id);
    format!(
        "Downloading {name} failed — check {log_hint} for the reason, \
         then click Download to resume."
    )
}

/// Put a Models-tab failure in the ⚠ banner *without* touching the install
/// marquee. Unlike [`set_install_error`] this one has no install to stop — a
/// download that dies while some engine happens to be installing must not take
/// that install's progress line down with it.
fn set_models_error(ui: &slint::Weak<AppWindow>, msg: String) {
    ui.upgrade_in_event_loop(move |ui| ui.set_vc_install_error(msg.into())).ok();
}

/// How a `VcSetupProgress` status string maps to the Models-tab treatment.
/// Split out from the event handler for the same reason as
/// [`ModelProgressUi`]: the decision is unit-testable without a running UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VcSetupUi {
    /// Marquee + the stage label shown verbatim. Also the graceful fallback for
    /// any unknown/future status string (never panic, never strand the row).
    Running,
    /// The engine is installed — clear the install state and refetch, so the
    /// row's "one-time setup needed" warning goes with it.
    Done,
    /// Failed — surface the detail (reason + log path) in the banner, and
    /// refetch: a partial install can still have changed the rows.
    Error,
    /// The user pressed × — clear the state quietly, no scary banner.
    Cancelled,
}

fn vc_setup_ui(status: &str) -> VcSetupUi {
    match status {
        "done" => VcSetupUi::Done,
        "error" => VcSetupUi::Error,
        "cancelled" => VcSetupUi::Cancelled,
        // "running" and any unknown/future status keep the marquee up
        _ => VcSetupUi::Running,
    }
}

/// Human-readable name for a `VcSetupProgress` setup id, for failure banners.
/// Split out from the event handler so the vocabulary is unit-testable, and
/// written as a match rather than an if/else so a future engine degrades
/// readably (the raw id) instead of being mislabelled as the last arm.
fn setup_display_name(setup_id: &str) -> &str {
    match setup_id {
        "seedvc" => "Seed-VC",
        "vevo" => "Vevo",
        "luxtts" => "LuxTTS",
        // an id we don't have copy for yet: the raw id still names the thing
        // that failed, which beats naming the wrong engine
        other => other,
    }
}

/// Put an engine install failure in front of the user: the marquee stops and
/// the Models tab grows a ⚠ banner above its sections. Always visible — an install
/// that dies silently is indistinguishable from one that never started.
fn set_install_error(ui: &slint::Weak<AppWindow>, msg: String) {
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_vc_install_active("".into());
        ui.set_vc_install_stage("".into());
        ui.set_vc_install_error(msg.into());
    })
    .ok();
}

/// Take the install marquee — and any stale banner — back down (done/cancelled).
fn clear_install_state(ui: &slint::Weak<AppWindow>) {
    ui.upgrade_in_event_loop(|ui| {
        ui.set_vc_install_active("".into());
        ui.set_vc_install_stage("".into());
        ui.set_vc_install_error("".into());
    })
    .ok();
}

/// Update a single model row's download progress in place (no refetch).
fn set_model_progress(ui: &slint::Weak<AppWindow>, id: String, pct: f32, downloading: bool, finalizing: bool) {
    ui.upgrade_in_event_loop(move |ui| {
        for model in [ui.get_voice_models(), ui.get_stt_models(), ui.get_llm_models(), ui.get_vc_conv_models()] {
            for i in 0..model.row_count() {
                if let Some(mut it) = model.row_data(i) {
                    if it.id.as_str() == id {
                        it.progress = pct;
                        it.downloading = downloading;
                        it.finalizing = finalizing;
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

/// Enumerate capture devices for the ⚙ pickers: `(mics, sink monitors)`, each
/// as `(technical name, human description)`. On Linux this taps PipeWire via
/// pactl (monitors are a Linux feature); elsewhere the engine's sounddevice
/// enumeration lists mics only (system monitors wait for phase 3, and their
/// picker is hidden off-Linux).
#[cfg(target_os = "linux")]
async fn list_audio_devices(_proxy: &EngineClient) -> (Vec<(String, String)>, Vec<(String, String)>) {
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
#[cfg(not(target_os = "linux"))]
async fn list_audio_devices(proxy: &EngineClient) -> (Vec<(String, String)>, Vec<(String, String)>) {
    // sounddevice ids are name-based (stable across hotplug); the description
    // is the same name — good enough for the dropdown.
    let mut mics = Vec::new();
    if let Ok(json) = proxy.list_recording_devices().await {
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&json) {
            for d in &arr {
                let id = d.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = d.get("name").and_then(|v| v.as_str()).unwrap_or(id);
                if !id.is_empty() {
                    mics.push((id.to_string(), name.to_string()));
                }
            }
        }
    }
    // Windows lists WASAPI render endpoints as the "monitors" for the ⚙ tap
    // picker; macOS has none until phase 3. Enumeration is blocking COM, so it
    // runs on a blocking pool thread.
    #[cfg(target_os = "windows")]
    let monitors = tokio::task::spawn_blocking(capture_win::enumerate_render_devices)
        .await
        .unwrap_or_default();
    #[cfg(not(target_os = "windows"))]
    let monitors = Vec::new();
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
        3 => VC_MUSIC_ROWS.iter().map(|(e, _)| *e).collect(),
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
    // split by kind — the rail shows only the clips for the active vc-mode
    let mut speech: Vec<SourceClipItem> = Vec::new();
    let mut music: Vec<SourceClipItem> = Vec::new();
    for c in &arr {
        let g = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let (id, name, path, meta) = (g("id"), g("name"), g("path"), g("meta"));
        let is_music = g("kind") == "music";
        let item = SourceClipItem {
            id: id.clone().into(),
            name: name.clone().into(),
            meta: meta.into(),
            music: is_music,
        };
        if is_music { music.push(item); } else { speech.push(item); }
        data.push((id, name, path, g("transcript")));
    }
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_vc_clips_speech(ModelRc::from(Rc::new(VecModel::from(speech))));
        ui.set_vc_clips_music(ModelRc::from(Rc::new(VecModel::from(music))));
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
    // Warmup may already have failed before this session connected (a fast
    // failure — a missing weight file — beats the first round-trip); the
    // property carries the reason, the PropertiesChanged arm below catches the
    // slower ones. Empty = healthy, so this is a no-op on every normal launch.
    let load_error = proxy.model_load_error().await.unwrap_or_default();
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
            if !load_error.is_empty() {
                ui.set_gen_error(load_error.into());
            }
            ui.set_kokoro_names(ModelRc::from(Rc::new(VecModel::from(kokoro_names))));
            ui.set_kokoro_ids(ModelRc::from(Rc::new(VecModel::from(kokoro_ids))));
            ui.set_voices(ModelRc::from(Rc::new(VecModel::from(to_voice_items(grid)))));
            if ui.get_selected_voice().is_empty() {
                ui.set_selected_voice(default_selected.clone().into());
                ui.set_selected_voice_name(voice_name(&ui, &default_selected).into());
            }
            // Cold launch and reconnect both restore a selection without a
            // click, so the select-voice callback never fires for it. Run it
            // by hand or the composer keeps its defaults — a Kokoro preset
            // would come up offering the (impossible) engine picker.
            let sel = ui.get_selected_voice();
            if !sel.is_empty() {
                ui.invoke_select_voice(sel);
            }
            set_history_model(&ui, hist_items);
            set_captures_model(&ui, capture_items);
        })
        .ok();
    }

    let mut pending_llm: u32 = 0;
    // --- model-selection state (one authority per category) ---
    // `voice_models`/`stt_models`/`llm_models` are the pickers' catalogs;
    // `active_*` is what the engine currently has loaded for the category.
    // `sel_voice`/`sel_profile_json` cache the composer's selection so any
    // catalog rebuild can re-derive the dropdown without another GetProfile,
    // and `session_engine` holds this session's explicit per-voice picks —
    // never written back to the profile (requirement 3).
    let cat0 = refresh_models(&ui, &proxy).await;
    let mut voice_models = cat0.voice;
    let mut stt_models = cat0.stt;
    let mut llm_models = cat0.llm;
    let mut vc_models = cat0.vc;
    let mut active_voice = cat0.active_voice;
    let mut active_stt = cat0.active_stt;
    let mut active_llm = cat0.active_llm;
    let mut sel_voice = String::new();
    let mut sel_profile_json = String::new();
    let mut session_engine: HashMap<String, String> = HashMap::new();
    let mut composer_pick =
        apply_composer_engines(&ui, &voice_models, "", "", None, &active_voice);
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
    let mut tr_rec: Option<Capture> = None;
    let mut tr_elapsed: u32 = 0;
    let tr_wav = scratch_wav("syrinx-transcribe.wav");
    const TR_REC_MAX: u32 = 600; // 10 min safety cap
    let mut pending_tr: u32 = 0;
    let mut pending_tr_refine: u32 = 0;
    // voice-changer (⇄) view state
    let mut vc_rec: Option<Capture> = None;
    let mut vc_elapsed: u32 = 0;
    let vc_wav = scratch_wav("syrinx-convert.wav");
    const VC_REC_MAX: u32 = 180; // matches the engine's SYRINX_VC_MAX_SECS default
    let mut vc_source: Option<String> = None;       // armed source path
    let mut vc_voice_ids: Vec<String> = Vec::new(); // parallel to the dropdown names
    let mut pending_vc: u32 = 0;                    // in-flight conversion gen id
    let mut pending_vc_music = false;               // that conversion is a song cover
    // the request the Vevo2 whisper-medium consent dialog interrupted
    let mut pending_vevo2: Option<VcRequest> = None;
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
    // ⚙ test-mic: the live test (None = not testing) and its age in 1 s ticks.
    // On Win/mac a `MicTest` is the §14 recording id; on Linux it is the app's
    // own parecord child plus the task metering it. The test is normally ended
    // by the toggle or by leaving the tab (slint fires st-mic-test-toggle on any
    // tab change); MIC_TEST_MAX is the belt-and-braces stop in case that
    // interception is ever bypassed — an open input stream nobody can see is the
    // one outcome this feature may not have.
    let mut mic_test_id: Option<MicTest> = None;
    let mut mic_test_elapsed: u32 = 0;
    const MIC_TEST_MAX: u32 = 120;
    // library (▤) state — rows cached, filters applied app-side
    let mut lib_rows: Vec<LibRow> = Vec::new();
    let mut lib_voices: Vec<String> = Vec::new();
    let mut lib_loaded = false;
    let mut lib_filters: (String, i32, i32, bool, i32) = (String::new(), 0, 0, false, 0);
    // create-voice modal state
    let mut cv_rec: Option<Capture> = None;
    let cv_wav = scratch_wav("syrinx-cv-record.wav");
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

    // Re-fetch the catalog and re-derive everything that hangs off it. The
    // composer's dropdown is rebuilt from the CACHED selection rather than
    // dropped on the floor: a download finishing must not silently move which
    // model the user is pointed at. A macro because it writes half a dozen of
    // the worker's locals; captures them by name, like `handle_event!` below.
    macro_rules! reload_models {
        () => {{
            let c = refresh_models(&ui, &proxy).await;
            voice_models = c.voice;
            stt_models = c.stt;
            llm_models = c.llm;
            vc_models = c.vc;
            active_voice = c.active_voice;
            active_stt = c.active_stt;
            active_llm = c.active_llm;
            composer_pick = apply_composer_engines(
                &ui,
                &voice_models,
                &sel_voice,
                &sel_profile_json,
                session_engine.get(&sel_voice).map(|s| s.as_str()),
                &active_voice,
            );
        }};
    }

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
                EngineEvent::RecordingLevel { rec_id, rms } => {
                    // Every §14 capture emits this (dictation, ⇄, create-voice);
                    // only the ⚙ test's own id drives the meter.
                    if is_mic_test_rec(&mic_test_id, &rec_id) {
                        // sqrt = perceptual: linear RMS leaves normal speech
                        // hugging the left edge of the bar.
                        let lvl = (rms.max(0.0) as f32).sqrt();
                        ui.upgrade_in_event_loop(move |ui| ui.set_st_mic_level(lvl)).ok();
                    }
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
                EngineEvent::TranscribeResult { req_id, text, error } => {
                    if req_id == pending_vc_tr && pending_vc_tr != 0 && req_id != pending_tr {
                        pending_vc_tr = 0;
                        // clip armed/saved before whisper finished — cache it
                        // (never cache a failure: leave the row empty to retry)
                        if !vc_tr_clip.is_empty() {
                            if !error && !text.trim().is_empty() {
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
                            // error=true → whisper failed; the preview shows a
                            // failure note instead of the "no speech" copy
                            ui.set_vc_transcript_failed(error);
                            ui.set_vc_transcript(text.into());
                        }).ok();
                    } else if req_id == pending_tr && pending_tr != 0 {
                        pending_tr = 0;
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_tr_busy(false);
                            if error {
                                ui.set_tr_status("transcription failed — check engine logs".into());
                            } else if text.trim().is_empty() {
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
                    match model_progress_ui(&status) {
                        ModelProgressUi::Downloading => set_model_progress(&ui, model_id, pct as f32, true, false),
                        ModelProgressUi::Finalizing => set_model_progress(&ui, model_id, pct as f32, true, true),
                        ModelProgressUi::Terminal => { // done / error
                            // "done" and "error" agree on everything except
                            // whether the user is told: both refetch, because
                            // a torn download changes the rows too.
                            let failed = status == "error";
                            reload_models!();
                            // ListVoices gates the extra preset engines on
                            // DOWNLOADED, so a finished (or deleted) fetch
                            // changes the voice grid too — the pre-existing
                            // gap that only showed up once Use died.
                            refresh_grid(&ui, &proxy, &mut avatar_cache).await;
                            if failed {
                                tracing::error!("model download failed: {model_id}");
                                // Named off the refreshed catalog, so the
                                // banner says "Seed-VC", not "seed-vc".
                                let mut rows: Vec<(&str, &str)> = voice_models
                                    .iter()
                                    .chain(&stt_models)
                                    .chain(&llm_models)
                                    .map(|r| (r.id.as_str(), r.display.as_str()))
                                    .collect();
                                rows.extend(
                                    vc_models.iter().map(|m| (m.id.as_str(), m.display.as_str())),
                                );
                                let msg = model_download_error(
                                    &model_id, &rows, &engine_log_hint(),
                                );
                                set_models_error(&ui, msg);
                            }
                        }
                    }
                }
                EngineEvent::VcSetupProgress { setup_id, stage, status, detail } => {
                    match vc_setup_ui(&status) {
                        VcSetupUi::Running => {
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_vc_install_active(setup_id.into());
                                ui.set_vc_install_stage(stage.into());
                            }).ok();
                        }
                        VcSetupUi::Done => {
                            clear_install_state(&ui);
                            // the row's "one-time setup needed" warning comes
                            // from the engine — only a refetch clears it
                            reload_models!();
                        }
                        VcSetupUi::Error => {
                            tracing::error!("vc setup failed: {setup_id}: {detail}");
                            let label = setup_display_name(&setup_id);
                            // detail carries the reason and the log path
                            let msg = if detail.is_empty() {
                                format!("{label} install failed.")
                            } else {
                                format!("{label} install failed — {detail}")
                            };
                            set_install_error(&ui, msg);
                            // a half-done install can still have moved rows
                            reload_models!();
                        }
                        // the user asked for this one — no banner
                        VcSetupUi::Cancelled => clear_install_state(&ui),
                    }
                }
                EngineEvent::LlmResult { req_id, text, error } => {
                    // transcription-view refine result routes to tr-text
                    if req_id == pending_tr_refine && pending_tr_refine != 0 {
                        pending_tr_refine = 0;
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_tr_busy(false);
                            // error=true → the LLM raised; say so instead of
                            // clearing the status and leaving the raw text
                            if error {
                                ui.set_tr_status("refine failed — check engine logs".into());
                            } else {
                                ui.set_tr_status("".into());
                                if !text.trim().is_empty() {
                                    ui.set_tr_text(text.into());
                                }
                            }
                        }).ok();
                    } else if req_id == pending_llm && pending_llm != 0 {
                        pending_llm = 0;
                        let ui_text = text.clone();
                        ui.upgrade_in_event_loop(move |ui| {
                            ui.set_llm_busy(false);
                            // an empty result is legitimate (nothing to paste);
                            // a raised one gets the composer's ⚠ banner
                            if error {
                                ui.set_gen_error("the personality LLM failed — check engine logs".into());
                            } else if !ui_text.trim().is_empty() {
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
                EngineEvent::PropertiesChanged { changed } => {
                    // ModelLoaded is not consumed (the splash drops on the
                    // first round-trip, not on warmup); ModelLoadError is —
                    // a failed warmup otherwise leaves the models silently
                    // absent until the first generation blows up.
                    if let Some(msg) = changed.get("ModelLoadError").and_then(|v| v.as_str()) {
                        if !msg.is_empty() {
                            tracing::error!("engine warmup failed: {msg}");
                            let msg = msg.to_string();
                            ui.upgrade_in_event_loop(move |ui| ui.set_gen_error(msg.into())).ok();
                        }
                    }
                }
                // SpeakStarted: the app consumes it on neither transport (the
                // D-Bus path never subscribed to it either).
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

            _ = rec_interval.tick(), if cv_rec.is_some() || tr_rec.is_some() || vc_rec.is_some()
                                        || mic_test_id.is_some() => {
                if mic_test_id.is_some() {
                    mic_test_elapsed += 1;
                    if mic_test_elapsed >= MIC_TEST_MAX {
                        mic_test_stop(&ui, &proxy, mic_test_id.take()).await;
                    }
                }
                // recorder died on its own (e.g. a suspended monitor source
                // erroring at first open) — surface it instead of a phantom
                // "recording" that never advances
                if let Some(cap) = vc_rec.as_mut() {
                    if capture_died(cap) {
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
                        if let Some(cap) = vc_rec.take() {
                            let path = capture_stop(cap, &proxy, &vc_wav).await;
                            if path.is_empty() {
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_vc_recording(false);
                                    ui.set_vc_status("⚠ recorder exited — try again or check the source".into());
                                }).ok();
                            } else {
                            vc_source = Some(path.clone());
                            let label = format!("{} · stopped at the 3:00 cap", recorded_label(&path));
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_vc_recording(false);
                                ui.set_vc_has_source(true);
                                ui.set_vc_source_label(label.into());
                                ui.set_vc_armed_id("".into());
                                ui.set_vc_armed_saved(false);
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
                        if let Some(cap) = tr_rec.take() {
                            let path = capture_stop(cap, &proxy, &tr_wav).await;
                            if path.is_empty() {
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_tr_recording(false);
                                    ui.set_tr_status("⚠ recording failed — try again".into());
                                }).ok();
                            } else {
                            match proxy.transcribe_file(&path).await {
                                Ok(id) => pending_tr = id,
                                Err(e) => tracing::error!("transcribe failed: {e}"),
                            }
                            tr_source = path.clone();
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_recording(false);
                                ui.set_tr_busy(true);
                                ui.set_tr_has_source(true);
                                ui.set_tr_status("transcribing…".into());
                            }).ok();
                            }
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
                    if let Some(cap) = cv_rec.take() {
                        let path = capture_stop(cap, &proxy, &cv_wav).await;
                        if path.is_empty() {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_cv_recording(false);
                                ui.set_cv_sample_label("⚠ recording failed — try again".into());
                            }).ok();
                        } else {
                            cv_sample = Some(path.clone());
                            let label = recorded_label(&path);
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_cv_recording(false);
                                ui.set_cv_sample_label(label.into());
                            }).ok();
                        }
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
                    // No silent downloads: the model the composer is showing is
                    // the model that would run, so if it isn't on disk say so
                    // here rather than letting the backend fetch multiple GB.
                    // (The engine refuses too — this is the friendly half.)
                    let missing = composer_pick
                        .model
                        .clone()
                        .filter(|r| !row_ready(r.downloaded, r.needs_setup));
                    if let Some(r) = missing {
                        set_needs_model(&ui, "tts", &r.display, &r.id);
                        cancel_pending_generation(&ui, &proxy).await;
                    } else {
                        clear_needs_model(&ui);
                        match proxy.speak(&text, &voice).await {
                            Ok(id) => current_gen = id,
                            Err(e) => tracing::error!("speak failed: {e}"),
                        }
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
                                    // the in-place rewrite changed the clip's
                                    // duration — re-read the rail so its meta
                                    // (m:ss) reflects the trim
                                    vc_clips_data = refresh_vc_clips(&ui, &proxy).await;
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
                                    if let Some(r) = ready_or_notice(&stt_models, &active_stt) {
                                        // the trim itself landed; only the
                                        // re-transcription waits on the weights
                                        set_needs_model(&ui, "tr", &r.display, &r.id);
                                        ui.upgrade_in_event_loop(|ui| ui.set_trim_open(false)).ok();
                                    } else {
                                        clear_needs_model(&ui);
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
                    // System audio taps the output (Linux monitor / Windows
                    // loopback); mic uses the ⚙ mic choice ("" = default).
                    let (target, _ok) = resolve_capture_device(&cfg, system).await;
                    match capture_start(&proxy, &cv_wav, target.as_deref(), system).await {
                        Ok(cap) => {
                            cv_rec = Some(cap);
                            rec_elapsed = 0;
                            rec_interval.reset();  // first tick a full second out
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_cv_recording(true);
                                ui.set_cv_sample_label("● recording… 0s / 30s".into());
                            }).ok();
                        }
                        Err(e) => {
                            tracing::error!("record failed: {e}");
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_cv_sample_label("⚠ recording failed — try again".into());
                            }).ok();
                        }
                    }
                }
                Some(Cmd::CvStopRecord) => {
                    if let Some(cap) = cv_rec.take() {
                        let path = capture_stop(cap, &proxy, &cv_wav).await;
                        if path.is_empty() {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_cv_recording(false);
                                ui.set_cv_sample_label("⚠ recording failed — try again".into());
                            }).ok();
                        } else {
                            cv_sample = Some(path.clone());
                            let label = recorded_label(&path);
                            ui.upgrade_in_event_loop(move |ui| {
                                ui.set_cv_recording(false);
                                ui.set_cv_sample_label(label.into());
                            }).ok();
                        }
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
                    // "Follow the composer" (index 0) stores "", else the engine
                    // of the picked cloning model. This modal is the ONLY writer
                    // of `default_engine` now — it is a seed the composer reads
                    // on selection, never a value the composer writes back.
                    let default_engine = if model_index == 0 {
                        String::new()
                    } else {
                        composer_options(&voice_models, None)
                            .get(model_index - 1)
                            .map(|r| r.engine.clone())
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
                    reload_models!();
                }
                Some(Cmd::DownloadModel { id }) => {
                    match proxy.download_model(&id).await {
                        Ok(true) => set_model_progress(&ui, id, 0.0, true, false),
                        _ => tracing::error!("download_model failed: {id}"),
                    }
                }
                Some(Cmd::DeleteModel { id }) => {
                    proxy.delete_model(&id).await.ok();
                    reload_models!();
                    // deleting an extra preset engine's weights takes its
                    // voices out of ListVoices — the grid has to follow
                    refresh_grid(&ui, &proxy, &mut avatar_cache).await;
                }
                Some(Cmd::PickSttModel { index }) => {
                    if let Some(row) = stt_models.get(index).cloned() {
                        apply_active_model(&proxy, &row.id).await;
                        clear_needs_model(&ui);
                        reload_models!();
                    }
                }
                Some(Cmd::PickLlmModel { index }) => {
                    if let Some(row) = llm_models.get(index).cloned() {
                        apply_active_model(&proxy, &row.id).await;
                        clear_needs_model(&ui);
                        reload_models!();
                    }
                }
                Some(Cmd::InstallVc { setup_id }) => {
                    // the row is already showing "starting…" — every path that
                    // isn't a started install has to take it back down again
                    match proxy.install_vc_engine(&setup_id).await {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::warn!("install_vc_engine refused: {setup_id}");
                            set_install_error(&ui, "that engine is already installing.".to_string());
                        }
                        Err(e) => {
                            tracing::error!("install_vc_engine failed: {setup_id}: {e}");
                            set_install_error(&ui, format!("could not start the install: {e}"));
                        }
                    }
                }
                Some(Cmd::CancelVc { setup_id }) => {
                    // fire-and-forget: the row clears on the engine's
                    // "cancelled" progress event, not on this reply
                    if let Err(e) = proxy.cancel_vc_setup(&setup_id).await {
                        tracing::error!("cancel_vc_setup failed: {setup_id}: {e}");
                    }
                }
                Some(Cmd::Compose { voice_id, prompt }) => {
                    if let Some(r) = ready_or_notice(&llm_models, &active_llm) {
                        set_needs_model(&ui, "tts", &r.display, &r.id);
                        ui.upgrade_in_event_loop(|ui| ui.set_llm_busy(false)).ok();
                    } else {
                        clear_needs_model(&ui);
                        match proxy.compose_profile(&voice_id, &prompt).await {
                            Ok(rid) if rid != 0 => pending_llm = rid,
                            _ => { ui.upgrade_in_event_loop(|ui| ui.set_llm_busy(false)).ok(); }
                        }
                    }
                }
                Some(Cmd::Rewrite { voice_id, text }) => {
                    if let Some(r) = ready_or_notice(&llm_models, &active_llm) {
                        set_needs_model(&ui, "tts", &r.display, &r.id);
                        ui.upgrade_in_event_loop(|ui| ui.set_llm_busy(false)).ok();
                    } else {
                        clear_needs_model(&ui);
                        match proxy.rewrite_profile(&voice_id, &text).await {
                            Ok(rid) if rid != 0 => pending_llm = rid,
                            _ => { ui.upgrade_in_event_loop(|ui| ui.set_llm_busy(false)).ok(); }
                        }
                    }
                }
                Some(Cmd::CvCancel) => {
                    if let Some(cap) = cv_rec.take() {
                        capture_discard(cap, &proxy).await;
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
                    // two models to check: the LLM that rewrites the line and
                    // the voice model that then speaks it
                    let missing = ready_or_notice(&llm_models, &active_llm).cloned().or_else(|| {
                        composer_pick
                            .model
                            .clone()
                            .filter(|r| !row_ready(r.downloaded, r.needs_setup))
                    });
                    if let Some(r) = missing {
                        set_needs_model(&ui, "tts", &r.display, &r.id);
                        cancel_pending_generation(&ui, &proxy).await;
                        continue;
                    }
                    clear_needs_model(&ui);
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
                    // one GetProfile serves the language list, the engine lock
                    // and the seed; builtins have no profile row to fetch at all
                    let pj = if id.starts_with("builtin:") {
                        String::new()
                    } else {
                        proxy.get_profile(&id).await.unwrap_or_default()
                    };
                    let code = if id.starts_with("builtin:") {
                        if id.split(':').nth(1).unwrap_or("kokoro") == "kokoro" {
                            kokoro_lang_code(&id).to_string()
                        } else {
                            "en".to_string()
                        }
                    } else {
                        serde_json::from_str::<serde_json::Value>(&pj)
                            .ok()
                            .and_then(|p| p.get("language").and_then(|v| v.as_str()).map(str::to_string))
                            .unwrap_or_else(|| "en".to_string())
                    };
                    sel_voice = id.clone();
                    sel_profile_json = pj;
                    composer_pick = apply_composer_engines(
                        &ui,
                        &voice_models,
                        &sel_voice,
                        &sel_profile_json,
                        session_engine.get(&sel_voice).map(|s| s.as_str()),
                        &active_voice,
                    );
                    // The language subset follows the model that will actually
                    // speak — the composer's own pick, not a profile field.
                    lang_codes = update_composer_langs(&ui, &composer_pick.engine, &code);
                    // A cloned voice speaks on whatever `set_voice_engine` last
                    // loaded, so a seed that differs from the engine's active
                    // model has to be pushed or the dropdown would be lying
                    // about what Generate does. Locked voices route to their own
                    // engine regardless — they never touch the active model.
                    let seed = composer_pick
                        .model
                        .clone()
                        .filter(|m| !composer_pick.rows.is_empty() && m.id != active_voice);
                    if let Some(m) = seed {
                        apply_active_model(&proxy, &m.id).await;
                        reload_models!();
                    }
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
                    // The composer is the authority now. The dropdown's options
                    // ARE catalog rows, so the picked size can no longer be
                    // discarded on the way to the engine (the "0.6B picked,
                    // 1.7B speaks" bug is gone by construction), and nothing
                    // here writes the profile: the pick is this session's, for
                    // this voice, and `default_engine` stays the seed the user
                    // set in the create/edit modal.
                    if let Some(row) = composer_pick.rows.get(index).cloned() {
                        session_engine.insert(voice.clone(), row.id.clone());
                        apply_active_model(&proxy, &row.id).await;
                        clear_needs_model(&ui);
                        reload_models!();
                        let code = serde_json::from_str::<serde_json::Value>(&sel_profile_json)
                            .ok()
                            .and_then(|p| {
                                p.get("language").and_then(|v| v.as_str()).map(str::to_string)
                            })
                            .unwrap_or_else(|| "en".to_string());
                        lang_codes = update_composer_langs(&ui, &row.engine, &code);
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
                        // seeded engine → its dropdown slot; "" → Follow the composer
                        let de = s("default_engine");
                        let model_idx = if de.is_empty() {
                            0
                        } else {
                            composer_options(&voice_models, None)
                                .iter()
                                .position(|r| r.engine == de)
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
                    if let Some(cap) = tr_rec.take() {
                        // stop → transcribe (unless the capture came out silent)
                        let path = capture_stop(cap, &proxy, &tr_wav).await;
                        if path.is_empty() {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_recording(false);
                                ui.set_tr_status("⚠ recording failed — try again".into());
                            }).ok();
                        } else if wav_rms(&path).map(|r| r < 0.006).unwrap_or(true) {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_recording(false);
                                ui.set_tr_status("⚠ capture was silent — check the input device".into());
                            }).ok();
                        } else if let Some(r) = ready_or_notice(&stt_models, &active_stt) {
                            // the recording is safe on disk (✂ Trim still
                            // reaches it) — only the transcription is refused
                            tr_source = path.clone();
                            set_needs_model(&ui, "tr", &r.display, &r.id);
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_recording(false);
                                ui.set_tr_has_source(true);
                                ui.set_tr_status("".into());
                            }).ok();
                        } else {
                            clear_needs_model(&ui);
                            match proxy.transcribe_file(&path).await {
                                Ok(id) => {
                                    pending_tr = id;
                                    tr_source = path.clone();
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
                        // System taps the output (Linux only); mic uses the ⚙
                        // choice ("" = default).
                        let (device, ok) = resolve_capture_device(&cfg, system).await;
                        if !ok {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_status("no default sink monitor found".into());
                            }).ok();
                        } else {
                            match capture_start(&proxy, &tr_wav, device.as_deref(), system).await {
                                Ok(cap) => {
                                    tr_rec = Some(cap);
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
                                Err(e) => {
                                    tracing::error!("record failed: {e}");
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_tr_status("⚠ recording failed — try again".into());
                                    }).ok();
                                }
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
                        if let Some(r) = ready_or_notice(&stt_models, &active_stt) {
                            tr_source = path.clone();
                            set_needs_model(&ui, "tr", &r.display, &r.id);
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_tr_has_source(true);
                                ui.set_tr_status("".into());
                            }).ok();
                        } else {
                            clear_needs_model(&ui);
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
                }
                Some(Cmd::TrRefine { text }) => {
                    if let Some(r) = ready_or_notice(&llm_models, &active_llm) {
                        set_needs_model(&ui, "tr", &r.display, &r.id);
                        ui.upgrade_in_event_loop(|ui| {
                            ui.set_tr_busy(false);
                            ui.set_tr_status("".into());
                        }).ok();
                    } else {
                        clear_needs_model(&ui);
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
                    if let Some(cap) = vc_rec.take() {
                        // stop → arm the clip as the conversion source
                        let path = capture_stop(cap, &proxy, &vc_wav).await;
                        if path.is_empty() {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_vc_recording(false);
                                ui.set_vc_status("⚠ recorder exited — try again or check the source".into());
                            }).ok();
                        } else if wav_rms(&path).map(|r| r < 0.006).unwrap_or(true) {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_vc_recording(false);
                                ui.set_vc_status("⚠ capture was silent — check the input device".into());
                            }).ok();
                        } else {
                            vc_source = Some(path.clone());
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
                    } else {
                        // System taps the output (Linux only); mic uses the ⚙
                        // choice ("" = default).
                        let (device, ok) = resolve_capture_device(&cfg, system).await;
                        if !ok {
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_vc_status("no default sink monitor found".into());
                            }).ok();
                        } else {
                            match capture_start(&proxy, &vc_wav, device.as_deref(), system).await {
                                Ok(cap) => {
                                    vc_rec = Some(cap);
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
                                Err(e) => {
                                    tracing::error!("record failed: {e}");
                                    ui.upgrade_in_event_loop(|ui| {
                                        ui.set_vc_status("⚠ recording failed — try again".into());
                                    }).ok();
                                }
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
                        let table = if mode == "music" { VC_MUSIC_ROWS } else { VC_SPEECH_ROWS };
                        let (engine, row_id) = table.get(engine_index).copied().unwrap_or(("", ""));
                        let req = VcRequest {
                            src, pid, engine, label, transcript, mode, semitones,
                        };
                        // The row this (engine, mode) pair actually loads — the
                        // reason the table holds pairs: vevo_timbre is
                        // Vevo-Timbre for speech and Vevo2 for singing.
                        let missing = vc_models
                            .iter()
                            .find(|m| m.id == row_id)
                            .filter(|m| !row_ready(m.downloaded, m.needs_setup))
                            .cloned();
                        if let Some(m) = missing {
                            set_needs_model(&ui, "vc", &m.display, &m.id);
                            ui.upgrade_in_event_loop(|ui| ui.set_vc_busy(false)).ok();
                        } else if req.needs_vevo2_consent() && !cfg.vevo2_whisper_ack {
                            // Vevo2's content encoder lives in Amphion's own
                            // cache, outside the Models catalog — no Download
                            // button can cover it, so this dialog is the only
                            // place that ~1.5 GB gets spent on purpose.
                            pending_vevo2 = Some(req);
                            ui.upgrade_in_event_loop(|ui| {
                                ui.set_vc_busy(false);
                                ui.set_vevo2_ack_open(true);
                            }).ok();
                        } else {
                            clear_needs_model(&ui);
                            if let Some(gid) = start_conversion(&ui, &proxy, &req).await {
                                pending_vc = gid;
                                // the rail's ■ and the player bar stop via
                                // Cancel{0} → current_gen; track it here too
                                current_gen = gid;
                                pending_vc_music = req.mode == "music";
                            }
                        }
                    }
                }
                Some(Cmd::Vevo2Ack) => {
                    cfg.vevo2_whisper_ack = true;
                    save_config(&cfg);
                    if let Some(req) = pending_vevo2.take() {
                        clear_needs_model(&ui);
                        if let Some(gid) = start_conversion(&ui, &proxy, &req).await {
                            pending_vc = gid;
                            current_gen = gid;
                            pending_vc_music = req.mode == "music";
                        }
                    }
                }
                Some(Cmd::VcSuggestPitch { index }) => {
                    // ⌖ auto-match: median-f0 gap between the armed source and
                    // the target profile → clamp to ±6 st → the semitone dropdown
                    if let (Some(src), Some(pid)) =
                        (vc_source.clone(), vc_voice_ids.get(index).cloned())
                    {
                        match proxy.suggest_pitch_shift(&src, &pid).await {
                            Ok(st) => {
                                let idx = st_to_semitone_index(st);
                                ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_vc_semitones_index(idx);
                                    ui.set_vc_status(format!(
                                        "auto pitch {st:+} st"
                                    ).into());
                                }).ok();
                            }
                            Err(e) => {
                                tracing::error!("suggest pitch failed: {e}");
                                ui.upgrade_in_event_loop(|ui| {
                                    ui.set_vc_status("⚠ no pitch match".into());
                                }).ok();
                            }
                        }
                    } else {
                        ui.upgrade_in_event_loop(|ui| {
                            ui.set_vc_status("⚠ arm a source first".into());
                        }).ok();
                    }
                }
                Some(Cmd::VcSaveClip { name, transcript, kind }) => {
                    if let Some(src) = vc_source.clone() {
                        match proxy.save_source_clip(&src, &name, &transcript, &kind).await {
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
                                ui.set_vc_transcript_failed(false);
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
                    let (mics, mons) = list_audio_devices(&proxy).await;
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
                    let data_dir = engine_data_dir().to_string_lossy().into_owned();
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
                    // retarget a running test at the newly picked device instead
                    // of leaving the meter reporting the old one
                    if mic_test_id.is_some() {
                        mic_test_stop(&ui, &proxy, mic_test_id.take()).await;
                        mic_test_id = mic_test_start(&ui, &proxy, &cfg.mic_device).await;
                        mic_test_elapsed = 0;
                    }
                }
                Some(Cmd::StMicTestToggle) => {
                    if mic_test_id.is_some() {
                        mic_test_stop(&ui, &proxy, mic_test_id.take()).await;
                    } else {
                        // same device string the capture path passes ("" = default)
                        mic_test_id = mic_test_start(&ui, &proxy, &cfg.mic_device).await;
                        mic_test_elapsed = 0;
                    }
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
    // Session over (quit, or a transport loss that took the engine's capture
    // with it): zero the meter so a reconnect can't come back to a stuck bar
    // and a Stop button with nothing behind it.
    if mic_test_id.take().is_some() {
        ui.upgrade_in_event_loop(|ui| {
            ui.set_st_mic_testing(false);
            ui.set_st_mic_level(0.0);
        })
        .ok();
    }
    end
}

// --- ⚙ mic test ----------------------------------------------------------
//
// One live test, two level sources. Win/mac start a §14 engine recording and
// let its RecordingLevel signal drive the meter; Linux never starts the engine
// recorder (its mic path is parecord, whose pactl source ids the engine's
// PortAudio recorder cannot resolve) and computes levels app-side instead. The
// worker holds whichever this platform produced, so the two `mic_test_start` /
// `mic_test_stop` pairs share one signature and every call site is identical.

/// A live ⚙ mic test: the `parecord` child and the task draining its stdout
/// into the meter.
#[cfg(target_os = "linux")]
struct MicTest {
    child: tokio::process::Child,
    reader: tokio::task::JoinHandle<()>,
}
/// A live ⚙ mic test: the engine's §14 recording id.
#[cfg(not(target_os = "linux"))]
type MicTest = String;

/// Does this §14 recording belong to the ⚙ test? Only its own id may drive the
/// meter. On Linux no engine recording ever runs, so nothing can.
#[cfg(not(target_os = "linux"))]
fn is_mic_test_rec(test: &Option<MicTest>, rec_id: &str) -> bool {
    test.as_deref() == Some(rec_id)
}
#[cfg(target_os = "linux")]
fn is_mic_test_rec(_test: &Option<MicTest>, _rec_id: &str) -> bool {
    false
}

/// Start the ⚙ mic test on `device` ("" = system default) and light the UI up.
/// Returns the §14 recording id, or `None` when the engine refused the device —
/// in which case the toggle springs back to "not testing" rather than lying.
#[cfg(not(target_os = "linux"))]
async fn mic_test_start(
    ui: &slint::Weak<AppWindow>,
    proxy: &EngineClient,
    device: &str,
) -> Option<String> {
    // Flip the toggle BEFORE the round-trip: leaving ⚙ mid-start would
    // otherwise see `st-mic-testing` still false and skip the auto-off,
    // stranding an open input stream on a tab the user has left. Optimistic
    // "on" makes the tab change fire the toggle, which the worker processes
    // right after this await returns — so the stream is closed either way.
    ui.upgrade_in_event_loop(|ui| ui.set_st_mic_testing(true)).ok();
    let id = match proxy.start_recording(device).await {
        Ok(id) if !id.is_empty() => id,
        Ok(_) => {
            tracing::warn!("mic test: engine could not open {device:?}");
            String::new()
        }
        Err(e) => {
            tracing::error!("mic test: start_recording failed: {e}");
            String::new()
        }
    };
    let testing = !id.is_empty();
    ui.upgrade_in_event_loop(move |ui| {
        ui.set_st_mic_testing(testing);
        ui.set_st_mic_level(0.0);
    })
    .ok();
    testing.then_some(id)
}

/// Stop the ⚙ mic test and blank the meter. Always **cancel**, never stop: a
/// test must not leave a WAV in the engine's scratch dir.
#[cfg(not(target_os = "linux"))]
async fn mic_test_stop(ui: &slint::Weak<AppWindow>, proxy: &EngineClient, id: Option<String>) {
    if let Some(id) = id {
        if let Err(e) = proxy.cancel_recording(&id).await {
            tracing::warn!("mic test: cancel_recording failed: {e}");
        }
    }
    ui.upgrade_in_event_loop(|ui| {
        ui.set_st_mic_testing(false);
        ui.set_st_mic_level(0.0);
    })
    .ok();
}

/// Start the ⚙ mic test on `device` ("" = system default) and light the UI up.
/// The engine is not involved: `parecord` streams raw PCM the reader task turns
/// into meter levels. `None` means the spawn failed, so the toggle springs back
/// to "not testing" rather than lying.
#[cfg(target_os = "linux")]
async fn mic_test_start(
    ui: &slint::Weak<AppWindow>,
    _proxy: &EngineClient,
    device: &str,
) -> Option<MicTest> {
    // Flip the toggle BEFORE the spawn: leaving ⚙ mid-start would otherwise see
    // `st-mic-testing` still false and skip the auto-off, stranding an open
    // input stream on a tab the user has left. Optimistic "on" makes the tab
    // change fire the toggle, which the worker processes right after this await
    // returns — so the stream is closed either way.
    ui.upgrade_in_event_loop(|ui| ui.set_st_mic_testing(true)).ok();
    let mut cmd = tokio::process::Command::new("parecord");
    // --raw puts PCM on stdout instead of a file. --latency-msec is not
    // optional: parecord's default fragment here is 96000 bytes — two seconds
    // of 24 kHz mono per read, which is a slideshow, not a meter.
    cmd.args(["--raw", "--rate=24000", "--channels=1", "--format=s16le", "--latency-msec=75"]);
    if !device.is_empty() {
        cmd.arg(format!("--device={device}"));
    }
    let spawned = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // session end drops the holder without a stop call; an orphaned
        // parecord would hold the input open past the app's own exit
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            tracing::error!("mic test: parecord failed to start: {e}");
            ui.upgrade_in_event_loop(|ui| {
                ui.set_st_mic_testing(false);
                ui.set_st_mic_level(0.0);
            })
            .ok();
            return None;
        }
    };
    let Some(mut out) = child.stdout.take() else {
        tracing::error!("mic test: parecord stdout was not piped");
        let _ = child.kill().await;
        ui.upgrade_in_event_loop(|ui| {
            ui.set_st_mic_testing(false);
            ui.set_st_mic_level(0.0);
        })
        .ok();
        return None;
    };
    let ui = ui.clone();
    let reader = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        // 1600 samples ≈ 66 ms at 24 kHz — one bar update per read
        let mut buf = [0u8; 3200];
        while out.read_exact(&mut buf).await.is_ok() {
            let mut sumsq = 0f64;
            for s in buf.chunks_exact(2) {
                let v = i16::from_le_bytes([s[0], s[1]]) as f64 / 32768.0;
                sumsq += v * v;
            }
            let rms = (sumsq / (buf.len() / 2) as f64).sqrt() as f32;
            // sqrt = perceptual: linear RMS leaves normal speech hugging the
            // left edge of the bar.
            let lvl = rms.sqrt();
            if ui.upgrade_in_event_loop(move |ui| ui.set_st_mic_level(lvl)).is_err() {
                return;
            }
        }
        // EOF/read error = parecord is gone (a missing or busy source exits
        // immediately), and a dead recorder may not leave the toggle lit with
        // nothing behind it. The worker's now-stale holder needs no signal: the
        // next toggle or the auto-stop timer kills an already-dead child.
        ui.upgrade_in_event_loop(|ui| {
            ui.set_st_mic_testing(false);
            ui.set_st_mic_level(0.0);
        })
        .ok();
    });
    Some(MicTest { child, reader })
}

/// Stop the ⚙ mic test and blank the meter. Nothing needs finalizing — `--raw`
/// writes no WAV header — so parecord takes a straight SIGKILL rather than the
/// SIGINT-then-wait dance `stop_pw_record` owes a real capture.
#[cfg(target_os = "linux")]
async fn mic_test_stop(ui: &slint::Weak<AppWindow>, _proxy: &EngineClient, test: Option<MicTest>) {
    if let Some(mut test) = test {
        // reader first: one still draining the pipe would race the kill and
        // paint a last level onto an already-blanked meter
        test.reader.abort();
        if let Err(e) = test.child.kill().await {
            tracing::warn!("mic test: could not kill parecord: {e}");
        }
    }
    ui.upgrade_in_event_loop(|ui| {
        ui.set_st_mic_testing(false);
        ui.set_st_mic_level(0.0);
    })
    .ok();
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

    // --- scratch_wav -----------------------------------------------------

    #[test]
    fn scratch_wav_lands_in_a_directory_that_exists_on_this_platform() {
        let p = scratch_wav("syrinx-scratch-wav-test.wav");
        let path = std::path::Path::new(&p);
        assert_eq!(path.file_name().unwrap(), "syrinx-scratch-wav-test.wav");
        let parent = path.parent().expect("scratch path has a parent dir");
        assert!(parent.is_dir(), "{} is not a directory", parent.display());
        // the real regression: capture's WavWriter does File::create here, and a
        // path under a nonexistent dir (Windows had /tmp) fails the whole start
        std::fs::File::create(path).expect("scratch wav path is creatable");
        std::fs::remove_file(path).ok();
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
        assert!(VC_SPEECH_ROWS.iter().all(|(e, _)| is_vc_engine(e)));
        assert!(VC_MUSIC_ROWS.iter().all(|(e, _)| is_vc_engine(e)));
    }

    // --- speech pitch fine-tune index <-> semitones ----------------------

    #[test]
    fn semitone_index_maps_symmetrically_around_zero() {
        assert_eq!(semitone_index_to_st(0), -6);
        assert_eq!(semitone_index_to_st(6), 0); // default ±0
        assert_eq!(semitone_index_to_st(12), 6);
        // round-trips for every displayable value
        for st in -6..=6 {
            assert_eq!(semitone_index_to_st(st_to_semitone_index(st)), st);
        }
    }

    #[test]
    fn st_to_semitone_index_clamps_out_of_range_suggestions() {
        assert_eq!(st_to_semitone_index(99), 12); // +6 st cap
        assert_eq!(st_to_semitone_index(-99), 0); // −6 st cap
        // and a stray dropdown index never over-shifts
        assert_eq!(semitone_index_to_st(-5), -6);
        assert_eq!(semitone_index_to_st(50), 6);
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
             "engine": "kokoro",
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
        assert!(voice[0].downloaded && voice[0].supported);
        // `active` is NOT passed through for a non-cloning voice engine — see
        // build_models_suppresses_in_use_on_non_cloning_voice_rows
        assert!(!voice[0].active);
        assert!(!voice[0].downloading);
        assert_eq!(voice[0].progress, 0.0); // progress is pushed later by set_model_progress

        assert_eq!(voice[1].size_label, "2.0 GB");
        assert!(voice[1].downloading && !voice[1].downloaded && !voice[1].supported);
        assert_eq!(voice[1].warning, "needs 12 GB VRAM");

        assert_eq!(stt[0].size_label, "1.5 GB");
        assert_eq!(llm[0].size_label, "4.0 GB");
        assert_eq!(vc[0].id, "seed_vc");
    }

    // --- model_progress_ui ----------------------------------------------

    #[test]
    fn model_progress_ui_maps_known_stages() {
        assert_eq!(model_progress_ui("downloading"), ModelProgressUi::Downloading);
        assert_eq!(model_progress_ui("finalizing"), ModelProgressUi::Finalizing);
        assert_eq!(model_progress_ui("done"), ModelProgressUi::Terminal);
        assert_eq!(model_progress_ui("error"), ModelProgressUi::Terminal);
    }

    #[test]
    fn model_progress_ui_unknown_stage_degrades_to_downloading() {
        // any future/unrecognized stage string must keep the row on the bar,
        // never panic or drop it
        for s in ["", "verifying", "queued", "FINALIZING", "unknown"] {
            assert_eq!(model_progress_ui(s), ModelProgressUi::Downloading);
        }
    }

    // --- model_download_error --------------------------------------------

    const CATALOG: [(&str, &str); 3] =
        [("kokoro", "Kokoro"), ("seed-vc", "Seed-VC"), ("qwen3-1.7b", "Qwen3 1.7B")];

    #[test]
    fn model_download_error_names_the_model_the_log_and_the_way_out() {
        let msg = model_download_error("seed-vc", &CATALOG, r"C:\log\engine.log");
        assert_eq!(
            msg,
            "Downloading Seed-VC failed — check C:\\log\\engine.log for the reason, \
             then click Download to resume."
        );
    }

    #[test]
    fn model_download_error_falls_back_to_the_raw_id() {
        // a fetch that outlives its catalog row is still nameable — a blank
        // here would be worse than the id (the banner would name nothing)
        for rows in [&CATALOG[..], &[][..], &[("ghost", "")][..]] {
            assert!(model_download_error("ghost", rows, "the engine log")
                .starts_with("Downloading ghost failed —"));
        }
    }

    #[test]
    fn model_download_error_is_specific_to_the_failed_row() {
        // the id decides the name — not the first row, not the last
        assert!(model_download_error("qwen3-1.7b", &CATALOG, "L")
            .starts_with("Downloading Qwen3 1.7B failed"));
    }

    // --- vc_setup_ui -----------------------------------------------------

    #[test]
    fn vc_setup_ui_maps_the_status_vocabulary() {
        assert_eq!(vc_setup_ui("running"), VcSetupUi::Running);
        assert_eq!(vc_setup_ui("done"), VcSetupUi::Done);
        assert_eq!(vc_setup_ui("error"), VcSetupUi::Error);
        assert_eq!(vc_setup_ui("cancelled"), VcSetupUi::Cancelled);
    }

    #[test]
    fn vc_setup_ui_unknown_status_degrades_to_running() {
        // a future status must keep the marquee up rather than strand the row
        // in a state nothing ever clears
        for s in ["", "queued", "waiting", "DONE", "canceled", "unknown"] {
            assert_eq!(vc_setup_ui(s), VcSetupUi::Running);
        }
    }

    // --- setup_display_name ----------------------------------------------

    #[test]
    fn setup_display_name_covers_the_setup_vocabulary() {
        assert_eq!(setup_display_name("seedvc"), "Seed-VC");
        assert_eq!(setup_display_name("vevo"), "Vevo");
        assert_eq!(setup_display_name("luxtts"), "LuxTTS");
    }

    #[test]
    fn setup_display_name_falls_back_to_the_raw_id() {
        // a future engine must never be mislabelled as an existing one — the
        // banner says "<id> install failed", which is honest if unpolished
        for s in ["", "chatterbox", "SEEDVC", "unknown"] {
            assert_eq!(setup_display_name(s), s);
        }
    }

    // --- build_models: VC engine setup fields -----------------------------

    #[test]
    fn build_models_reads_the_vc_setup_fields() {
        let json = r#"[
            {"id": "seed_vc", "display": "Seed-VC", "category": "vc", "size_mb": 900,
             "needs_setup": true, "setup_id": "seedvc",
             "warning": "one-time setup needed — click Install"},
            {"id": "vevo_timbre", "display": "Vevo", "category": "vc", "size_mb": 2048,
             "needs_setup": false, "setup_id": "vevo"}
        ]"#;
        let (.., vc) = build_models(json);
        assert!(vc[0].needs_setup);
        assert_eq!(vc[0].setup_id, "seedvc");
        assert_eq!(vc[0].warning, "one-time setup needed — click Install");
        // installed engine: the id still rides along, so × / Install… can
        // address the row, but the affordance is gone
        assert!(!vc[1].needs_setup);
        assert_eq!(vc[1].setup_id, "vevo");
    }

    #[test]
    fn build_models_defaults_the_vc_setup_fields() {
        // every non-VC row (and an older engine) omits both keys
        let json = r#"[{"id": "kokoro", "display": "Kokoro", "category": "voice", "size_mb": 350}]"#;
        let (voice, ..) = build_models(json);
        assert!(!voice[0].needs_setup);
        assert_eq!(voice[0].setup_id, "");
    }

    // --- missing_vc_engines (⇄ view notice) -------------------------------

    /// The four real VC catalog rows, with the two installable ones toggled.
    fn vc_catalog(seedvc_missing: bool, vevo_missing: bool) -> Vec<ModelItem> {
        let json = format!(
            r#"[
            {{"id": "chatterbox-vc", "display": "Chatterbox VC", "category": "vc",
             "needs_setup": false, "setup_id": ""}},
            {{"id": "seed-vc", "display": "Seed-VC", "category": "vc",
             "needs_setup": {seedvc_missing}, "setup_id": "seedvc"}},
            {{"id": "vevo-timbre", "display": "Vevo-Timbre", "category": "vc",
             "needs_setup": {vevo_missing}, "setup_id": "vevo"}},
            {{"id": "vevo2-singing", "display": "Vevo2 (singing)", "category": "vc",
             "needs_setup": {vevo_missing}, "setup_id": "vevo"}}
        ]"#
        );
        let (.., vc) = build_models(&json);
        vc
    }

    #[test]
    fn missing_vc_engines_is_empty_when_everything_is_installed() {
        assert_eq!(missing_vc_engines(&vc_catalog(false, false)), "");
        assert_eq!(missing_vc_engines(&[]), "");
    }

    #[test]
    fn missing_vc_engines_names_one_engine() {
        assert_eq!(
            missing_vc_engines(&vc_catalog(true, false)),
            "Seed-VC (recommended)"
        );
        // nothing to recommend when Seed-VC is already installed
        assert_eq!(missing_vc_engines(&vc_catalog(false, true)), "Vevo");
    }

    #[test]
    fn missing_vc_engines_joins_both() {
        assert_eq!(
            missing_vc_engines(&vc_catalog(true, true)),
            "Seed-VC (recommended) and Vevo"
        );
    }

    #[test]
    fn missing_vc_engines_dedupes_the_two_vevo_rows() {
        // vevo-timbre and vevo2-singing share setup_id "vevo" — one install,
        // so the notice must say "Vevo" once, not twice
        let both_vevo = missing_vc_engines(&vc_catalog(false, true));
        assert_eq!(both_vevo.matches("Vevo").count(), 1);
        assert!(!both_vevo.contains("and"));
    }

    #[test]
    fn missing_vc_engines_ignores_rows_with_no_setup() {
        // Chatterbox-VC is bundled: even a stray needs_setup can't name a row
        // that has no install to run
        let json = r#"[{"id": "chatterbox-vc", "display": "Chatterbox VC", "category": "vc",
                        "needs_setup": true, "setup_id": ""}]"#;
        let (.., vc) = build_models(json);
        assert_eq!(missing_vc_engines(&vc), "");
    }

    #[test]
    fn missing_vc_engines_falls_back_to_the_row_label() {
        // an engine added after this build ships an id we have no phrase for
        let json = r#"[{"id": "future-vc", "display": "Future VC", "category": "vc",
                        "needs_setup": true, "setup_id": "futurevc"}]"#;
        let (.., vc) = build_models(json);
        assert_eq!(missing_vc_engines(&vc), "Future VC");
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
        assert_eq!(music, VC_MUSIC_ROWS.iter().map(|(e, _)| *e).collect::<Vec<_>>());
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

    // --- locked_engine / engine_label ------------------------------------

    #[test]
    fn locked_engine_binds_builtins_to_the_engine_in_their_id() {
        // the Kokoro Defaults card's voices — the case Noah hit
        assert_eq!(locked_engine("builtin:kokoro:af_heart", ""), Some("kokoro".into()));
        // an extra preset engine locks just as hard
        assert_eq!(
            locked_engine("builtin:qwen_custom_voice:ethan", ""),
            Some("qwen_custom_voice".into())
        );
        // malformed ids still name an engine rather than blanking the field
        assert_eq!(locked_engine("builtin:", ""), Some("kokoro".into()));
        assert_eq!(locked_engine("builtin:tada", ""), Some("tada".into()));
    }

    #[test]
    fn locked_engine_binds_preset_profiles_and_frees_cloned_ones() {
        let preset = r#"{"voice_type":"preset","preset_engine":"kokoro","default_engine":""}"#;
        assert_eq!(locked_engine("prof:1", preset), Some("kokoro".into()));
        // a preset row with no engine falls through to the built-in one, the
        // same way SpeechSynthesizer.synthesize does
        assert_eq!(
            locked_engine("prof:1", r#"{"voice_type":"preset"}"#),
            Some("kokoro".into())
        );
        // cloned voices are the ones with a real choice — pinned or not
        assert_eq!(locked_engine("prof:1", r#"{"voice_type":"cloned"}"#), None);
        assert_eq!(
            locked_engine("prof:1", r#"{"voice_type":"cloned","default_engine":"qwen"}"#),
            None
        );
    }

    #[test]
    fn locked_engine_under_locks_when_it_cannot_tell() {
        // no profile payload (lookup failed / empty id): leave the dropdown
        // alone rather than lock the composer on a guess
        assert_eq!(locked_engine("prof:1", ""), None);
        assert_eq!(locked_engine("prof:1", "not json"), None);
        assert_eq!(locked_engine("prof:1", r#"{"name":"Piccolo"}"#), None);
        assert_eq!(locked_engine("", ""), None);
    }

    // --- model selection: rows, labels, readiness, seeding ----------------

    /// One catalog row. `dl`/`setup` are the two halves of readiness.
    fn vrow(id: &str, engine: &str, display: &str, dl: bool, setup: bool) -> VoiceRow {
        VoiceRow {
            id: id.into(),
            engine: engine.into(),
            display: display.into(),
            downloaded: dl,
            needs_setup: setup,
        }
    }

    /// A realistic voice catalog: Kokoro (preset-only), both Qwen TTS sizes
    /// with only 0.6B fetched, LuxTTS downloaded but not installed, and a
    /// CustomVoice row that a cloned voice may never use.
    fn voice_catalog() -> Vec<VoiceRow> {
        vec![
            vrow("kokoro", "kokoro", "Kokoro 82M", true, false),
            vrow("qwen-tts-1.7B", "qwen", "Qwen TTS 1.7B", false, false),
            vrow("qwen-tts-0.6B", "qwen", "Qwen TTS 0.6B", true, false),
            vrow("luxtts", "luxtts", "LuxTTS", true, true),
            vrow("chatterbox", "chatterbox", "Chatterbox (Multilingual)", true, false),
            vrow("qwen-custom-voice-1.7B", "qwen_custom_voice", "Qwen CustomVoice 1.7B", true, false),
        ]
    }

    #[test]
    fn is_cloning_engine_matches_the_set_pinned_engine_side() {
        // mirrors tts.CLONING_ENGINES; test_tts_routing.py pins the same five
        assert_eq!(
            CLONING_ENGINES,
            &["qwen", "luxtts", "chatterbox", "chatterbox_turbo", "tada"]
        );
        for e in CLONING_ENGINES {
            assert!(is_cloning_engine(e), "{e} should clone");
        }
        // preset-only engines and every conversion engine stay out
        for e in ["kokoro", "qwen_custom_voice", "seed_vc", "chatterbox_vc", ""] {
            assert!(!is_cloning_engine(e), "{e} should not clone");
        }
    }

    #[test]
    fn row_ready_needs_both_the_weights_and_the_engine() {
        assert!(row_ready(true, false));
        assert!(!row_ready(false, false)); // no weights
        assert!(!row_ready(true, true)); // LuxTTS: fetched, not installed
        assert!(!row_ready(false, true));
    }

    #[test]
    fn readiness_suffix_lets_needs_setup_win() {
        assert_eq!(readiness_suffix(true, false), "");
        assert_eq!(readiness_suffix(false, false), " — not downloaded");
        assert_eq!(readiness_suffix(true, true), " — needs setup");
        // both missing: the install is the actionable step, so it is the one named
        assert_eq!(readiness_suffix(false, true), " — needs setup");
    }

    #[test]
    fn option_label_says_what_is_missing() {
        assert_eq!(option_label(&vrow("a", "qwen", "Qwen TTS 0.6B", true, false)), "Qwen TTS 0.6B");
        assert_eq!(
            option_label(&vrow("a", "qwen", "Qwen TTS 1.7B", false, false)),
            "Qwen TTS 1.7B — not downloaded"
        );
        assert_eq!(
            option_label(&vrow("a", "luxtts", "LuxTTS", true, true)),
            "LuxTTS — needs setup"
        );
    }

    #[test]
    fn composer_options_offers_a_locked_voice_nothing() {
        // the LockedField replaces the dropdown — an option list would be an
        // offer the router ignores
        assert!(composer_options(&voice_catalog(), Some("kokoro")).is_empty());
        assert!(composer_options(&voice_catalog(), Some("qwen_custom_voice")).is_empty());
    }

    #[test]
    fn composer_options_offers_a_cloned_voice_cloning_engines_only() {
        let opts = composer_options(&voice_catalog(), None);
        let ids: Vec<&str> = opts.iter().map(|r| r.id.as_str()).collect();
        // Kokoro and CustomVoice cannot clone — picking one used to crash Generate
        assert_eq!(ids, ["qwen-tts-1.7B", "qwen-tts-0.6B", "luxtts", "chatterbox"]);
    }

    #[test]
    fn composer_options_keeps_unready_rows_visible() {
        // requirement 7: an undownloaded/uninstalled model is offered WITH its
        // tail, not hidden — hiding it is how a user never learns it exists
        let opts = composer_options(&voice_catalog(), None);
        let labels: Vec<String> = opts.iter().map(option_label).collect();
        assert!(labels.contains(&"Qwen TTS 1.7B — not downloaded".to_string()));
        assert!(labels.contains(&"LuxTTS — needs setup".to_string()));
    }

    #[test]
    fn seed_index_honors_this_sessions_pick_even_when_it_is_unready() {
        let opts = composer_options(&voice_catalog(), None);
        // the user explicitly chose 1.7B this session; the label says it isn't
        // downloaded and Generate raises the notice — but the dropdown must not
        // silently move off the thing they clicked
        assert_eq!(seed_index(&opts, Some("qwen-tts-1.7B"), "chatterbox", "chatterbox"), 0);
        assert_eq!(seed_index(&opts, Some("luxtts"), "", ""), 2);
        // a session pick for a row that no longer exists falls through
        assert_eq!(seed_index(&opts, Some("tada-1b"), "", "chatterbox"), 3);
    }

    #[test]
    fn seed_index_prefers_the_active_model_when_the_pin_names_its_engine() {
        let opts = composer_options(&voice_catalog(), None);
        // pinned to "qwen" with 0.6B loaded: seed 0.6B, not the catalog's first
        // qwen row — this is the "picked 0.6B, 1.7B speaks" bug's twin
        assert_eq!(seed_index(&opts, None, "qwen", "qwen-tts-0.6B"), 1);
    }

    #[test]
    fn seed_index_falls_back_to_the_pinned_engines_first_ready_row() {
        let opts = composer_options(&voice_catalog(), None);
        // active model is a different engine — the pin still wins, on the qwen
        // row that can actually run (0.6B; 1.7B isn't downloaded)
        assert_eq!(seed_index(&opts, None, "qwen", "chatterbox"), 1);
    }

    #[test]
    fn seed_index_ignores_a_pin_with_nothing_runnable() {
        // options: 0 qwen 1.7B (absent), 1 qwen 0.6B, 2 LuxTTS (needs setup),
        // 3 Chatterbox — so "first ready" is 1
        let opts = composer_options(&voice_catalog(), None);
        // LuxTTS is pinned but still needs its install: seeding the dropdown
        // onto it would park the composer on something that cannot speak, so
        // the seed falls through to first-ready instead
        assert_eq!(seed_index(&opts, None, "luxtts", ""), 1);
        // an engine with no row here at all does the same
        assert_eq!(seed_index(&opts, None, "kokoro", ""), 1);
    }

    #[test]
    fn seed_index_falls_back_to_the_active_model_then_to_first_ready() {
        let opts = composer_options(&voice_catalog(), None);
        // the active model wins over first-ready when it can run
        assert_eq!(seed_index(&opts, None, "", "chatterbox"), 3);
        // an active model that isn't ready is no better than none
        assert_eq!(seed_index(&opts, None, "", "qwen-tts-1.7B"), 1);
        assert_eq!(seed_index(&opts, None, "", ""), 1);
    }

    #[test]
    fn seed_index_survives_an_empty_or_wholly_unready_list() {
        assert_eq!(seed_index(&[], Some("qwen-tts-0.6B"), "qwen", "qwen-tts-0.6B"), 0);
        let nothing_ready = vec![
            vrow("qwen-tts-1.7B", "qwen", "Qwen TTS 1.7B", false, false),
            vrow("luxtts", "luxtts", "LuxTTS", true, true),
        ];
        // no honest answer exists — land on the first row rather than a stray index
        assert_eq!(seed_index(&nothing_ready, None, "", ""), 0);
    }

    #[test]
    fn engine_row_prefers_a_downloaded_row_over_the_catalogs_first() {
        let rows = voice_catalog();
        // the field report's case: 1.7B is first, 0.6B is what's on disk
        assert_eq!(engine_row(&rows, "qwen").unwrap().id, "qwen-tts-0.6B");
        // nothing downloaded → the first row still names the engine
        assert_eq!(
            engine_row(&rows[..2], "qwen").unwrap().id,
            "qwen-tts-1.7B"
        );
        assert!(engine_row(&rows, "tada").is_none());
    }

    #[test]
    fn engine_label_names_the_size_the_user_actually_has() {
        let rows = voice_catalog();
        assert_eq!(engine_label(&rows, "kokoro"), "Kokoro 82M");
        // NOT "Qwen TTS 1.7B": the LockedField and the coachmark must point at
        // the row that exists on this machine
        assert_eq!(engine_label(&rows, "qwen"), "Qwen TTS 0.6B");
        assert_eq!(engine_label(&rows, "qwen_custom_voice"), "Qwen CustomVoice 1.7B");
        // an engine with no catalog row still reads as something
        assert_eq!(engine_label(&rows, "brand_new_engine"), "brand_new_engine");
        assert_eq!(engine_label(&[], "kokoro"), "kokoro");
    }

    #[test]
    fn picker_index_lands_on_the_active_model_else_the_first_ready_one() {
        let stt = vec![
            vrow("whisper-base", "whisper", "Whisper Base", false, false),
            vrow("whisper-small", "whisper", "Whisper Small", true, false),
            vrow("whisper-medium", "whisper", "Whisper Medium", true, false),
        ];
        assert_eq!(picker_index(&stt, "whisper-medium"), 2);
        // active but not downloaded → the first one that can run
        assert_eq!(picker_index(&stt, "whisper-base"), 1);
        assert_eq!(picker_index(&stt, ""), 1);
        assert_eq!(picker_index(&[], "whisper-base"), 0);
    }

    #[test]
    fn ready_or_notice_only_refuses_a_row_it_can_name() {
        let stt = vec![
            vrow("whisper-base", "whisper", "Whisper Base", false, false),
            vrow("whisper-small", "whisper", "Whisper Small", false, false),
        ];
        // nothing downloaded: refuse, naming the row the picker points at
        assert_eq!(ready_or_notice(&stt, "whisper-base").unwrap().id, "whisper-base");
        // a ready pick dispatches
        let ok = vec![vrow("whisper-base", "whisper", "Whisper Base", true, false)];
        assert!(ready_or_notice(&ok, "whisper-base").is_none());
        // an empty catalog passes: the engine's own require_weights is the
        // authority, and a raw-repo override was never in this list
        assert!(ready_or_notice(&[], "anything").is_none());
    }

    // --- ⇄ engine dropdowns (VC_ROW_FOR's app-side twin) ------------------

    #[test]
    fn vc_row_tables_mirror_the_engines_vc_row_for() {
        // pinned against engine/tests/test_models.py's assertion on the same map
        assert_eq!(
            VC_SPEECH_ROWS,
            &[
                ("chatterbox_vc", "chatterbox-vc"),
                ("seed_vc", "seed-vc"),
                ("vevo_timbre", "vevo-timbre"),
            ]
        );
        // the asymmetry that makes this a table of pairs: the same engine loads
        // a different row for singing
        assert_eq!(VC_MUSIC_ROWS, &[("seed_vc", "seed-vc"), ("vevo_timbre", "vevo2-singing")]);
    }

    #[test]
    fn vc_row_labels_read_the_real_catalog_rows() {
        // Vevo installed with only the timbre weights fetched — the singing row
        // is a separate download, and the ⇄ picker has to say so
        let json = r#"[
            {"id": "chatterbox-vc", "display": "Chatterbox VC", "category": "vc",
             "downloaded": true},
            {"id": "seed-vc", "display": "Seed-VC", "category": "vc",
             "needs_setup": true, "setup_id": "seedvc"},
            {"id": "vevo-timbre", "display": "Vevo-Timbre", "category": "vc",
             "downloaded": true},
            {"id": "vevo2-singing", "display": "Vevo2 (singing)", "category": "vc",
             "downloaded": false}
        ]"#;
        let (.., vc) = build_models(json);
        assert_eq!(
            vc_row_labels(&vc, VC_SPEECH_ROWS),
            vec![
                SharedString::from("Chatterbox VC"),
                SharedString::from("Seed-VC — needs setup"),
                SharedString::from("Vevo-Timbre"),
            ]
        );
        // music mode names Vevo2's OWN row — its state was consulted by nothing
        // at all while these labels were hardcoded, so the picker read
        // "Vevo2 (singing)" for weights that were never on disk
        assert_eq!(
            vc_row_labels(&vc, VC_MUSIC_ROWS),
            vec![
                SharedString::from("Seed-VC — needs setup"),
                SharedString::from("Vevo2 (singing) — not downloaded"),
            ]
        );
    }

    #[test]
    fn vc_row_labels_keep_their_slot_when_a_row_is_missing() {
        // a vanished catalog row must not shift every index below it — the
        // dropdown index is what picks the engine
        let labels = vc_row_labels(&[], VC_SPEECH_ROWS);
        assert_eq!(labels.len(), VC_SPEECH_ROWS.len());
        assert_eq!(labels[1], SharedString::from("seed_vc"));
    }

    // --- IN USE chips ------------------------------------------------------

    #[test]
    fn build_models_suppresses_in_use_on_non_cloning_voice_rows() {
        // models.json ships `{"voice": "kokoro"}` as its factory default, and
        // Kokoro cannot clone — that stale value may not light a chip
        let json = r#"[
            {"id": "kokoro", "display": "Kokoro 82M", "category": "voice",
             "engine": "kokoro", "active": true},
            {"id": "qwen-tts-0.6B", "display": "Qwen TTS 0.6B", "category": "voice",
             "engine": "qwen", "active": true},
            {"id": "whisper-base", "display": "Whisper Base", "category": "stt",
             "engine": "whisper", "active": true},
            {"id": "qwen3-1.7b", "display": "Qwen3 1.7B", "category": "llm",
             "engine": "qwen_llm", "active": true}
        ]"#;
        let (voice, stt, llm, _) = build_models(json);
        assert!(!voice[0].active, "Kokoro cannot be the cloning model in use");
        assert!(voice[1].active, "a cloning engine keeps its chip");
        // the other categories are untouched — their active row is real
        assert!(stt[0].active);
        assert!(llm[0].active);
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

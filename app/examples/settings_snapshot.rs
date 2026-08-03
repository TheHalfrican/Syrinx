//! Offscreen render of one AppWindow tab to a PNG.
//!
//! The software renderer needs no display server, no GPU and no macOS
//! Screen-Recording grant, so UI layout can be checked from a terminal — the
//! only way to see a window from an agent session or CI.
//!
//! Usage:
//!   cargo run -p syrinx-app --example settings_snapshot -- <out.png> [variant] [WxH]
//!
//! ⚙ Settings variants (macOS-shaped state):
//!   native   — macOS 14.2+: the native process tap is the default row and
//!              nothing needs saying, so the card loses its hint row
//!   hint     — a loopback driver is picked (or the OS is pre-14.2): the
//!              routing-caveat line, no ⧉
//!   install  — pre-14.2 with no driver: the brew command plus the ⧉ that
//!              copies it
//!   none     — the Linux/Windows shape: no hint row at all
//!
//! Silence-guard variants (a system capture that came back empty):
//!   tr-silent — ☰ Transcription, the notice row under the source buttons
//!   vc-silent — ⇄ Converter, the clip armed anyway with the cause on vc-error
//!   cv-silent — the create-voice modal, warning label + footer cause line
//!
//! Scrolling-list variants (enough rows to force the scrollbar out — use a
//! short window, e.g. 1200x700, or the content fits and no bar is drawn):
//!   tts-hist  — ▷ Text to Speech, the HISTORY rail full of generations
//!   lib-list  — ▤ Audio Library, the clip rows
//!   vp-list   — ◍ Voices, the profile table (checks header↔row alignment)

use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ModelRc, PhysicalSize, SharedString, VecModel};

slint::include_modules!();

/// The whole platform: one window, drawn on demand, never shown.
struct Headless {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

/// Same bundled fallbacks main.rs registers — without them every symbol glyph
/// (⧉ ◈ ☰ …) is tofu, so the snapshot would not be the app's own text metrics.
fn register_fallback_fonts() {
    use slint::fontique_010::fontique;
    let mut collection = slint::fontique_010::shared_collection();
    for bytes in [
        include_bytes!("../ui/fonts/DejaVuSans.ttf").as_slice(),
        include_bytes!("../ui/fonts/SyrinxFallback.ttf").as_slice(),
    ] {
        let blob = fontique::Blob::new(std::sync::Arc::new(bytes.to_vec()));
        let families: Vec<_> =
            collection.register_fonts(blob, None).into_iter().map(|(id, _)| id).collect();
        for script in ["Latn", "Zyyy"] {
            collection.append_fallbacks(
                fontique::FallbackKey::new(fontique::Script::from_str_unchecked(script), None),
                families.iter().copied(),
            );
        }
    }
}

fn names(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>(),
    ))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "settings.png".into());
    let variant = args.next().unwrap_or_else(|| "hint".into());
    // a tall window fits the whole scrolled page, which is how overflow out of
    // a fixed-height card is spotted
    let (w, h) = args
        .next()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1200u32, 800u32));

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless { window: window.clone() }))?;
    register_fallback_fonts();

    let ui = AppWindow::new()?;
    // the splash covers everything until Rust clears it
    ui.set_booting(false);
    ui.set_tab("settings".into());
    ui.set_system_capture_supported(true);

    // The silence guard's three surfaces. Every one is the longest string the
    // guard can produce (the loopback cause), so the snapshot shows the worst
    // case for elide and for the fixed-height create-voice card.
    const CAUSE: &str = "A loopback tap hears only what is routed to it — send the audio there with a Multi-Output Device (Audio MIDI Setup).";
    const KEPT: &str = "⚠ silent capture — kept anyway";
    if let Some(flow) = variant.strip_suffix("-silent") {
        match flow {
            "tr" => {
                ui.set_tab("stt".into());
                ui.set_tr_rec_mode("system".into());
                ui.set_tr_has_source(true);
                ui.set_tr_status(KEPT.into());
                ui.set_tr_notice(CAUSE.into());
            }
            "vc" => {
                ui.set_tab("vc".into());
                ui.set_vc_rec_mode("system".into());
                ui.set_vc_has_source(true);
                ui.set_vc_source_label("recorded clip · 0:30".into());
                ui.set_vc_status(KEPT.into());
                ui.set_vc_error(CAUSE.into());
            }
            _ => {
                ui.set_tab("voices".into());
                ui.set_cv_open(true);
                ui.set_cv_mode("system".into());
                ui.set_cv_sample_label(KEPT.into());
                ui.set_cv_error(CAUSE.into());
            }
        }
        return render(&ui, &out, w, h, &variant);
    }

    if variant == "vp-list" {
        ui.set_tab("voices".into());
        ui.set_vp_rows(ModelRc::new(VecModel::from(
            ["Nova", "Atlas", "Wren", "Cinder", "Juniper", "Marlow", "Pike", "Solace"]
                .iter()
                .enumerate()
                .map(|(i, name)| ProfileRow {
                    id: format!("v{i}").into(),
                    name: (*name).into(),
                    desc: "cloned from a 30 s reference".into(),
                    lang: "en".into(),
                    engine: if i % 2 == 0 { "Chatterbox" } else { "LuxTTS" }.into(),
                    samples: format!("{}", i + 1).into(),
                    gens: format!("{}", i * 7).into(),
                    avatar: slint::Image::default(),
                    has_avatar: false,
                })
                .collect::<Vec<_>>(),
        )));
        return render(&ui, &out, w, h, &variant);
    }

    if variant == "tts-hist" || variant == "lib-list" {
        let rows: Vec<(&str, &str, &str)> = vec![
            ("Nova", "Kokoro · 0:12 · 2026-08-03 14:02", "The quick brown fox jumps over the lazy dog, twice, for good measure."),
            ("Atlas", "Chatterbox · 0:31 · 2026-08-03 13:44", "Local inference means the audio never leaves this machine."),
            ("Wren", "Kokoro · 0:07 · 2026-08-03 12:10", "Short one."),
            ("Cinder", "LuxTTS · 1:04 · 2026-08-02 21:55", "A longer passage that wraps inside the card's message box so the inner scroll view has something to do."),
            ("Juniper", "Chatterbox · 0:19 · 2026-08-02 18:30", "Testing one, two, three."),
            ("Marlow", "Kokoro · 0:44 · 2026-08-02 09:12", "Every generation is saved and searchable from the library."),
            ("Pike", "Seed-VC · 0:26 · 2026-08-01 22:41", "Converted from a reference clip, timbre swapped."),
            ("Solace", "LuxTTS · 0:58 · 2026-08-01 16:03", "The last row, far enough down that the rail must scroll to reach it."),
        ];
        if variant == "tts-hist" {
            ui.set_tab("tts".into());
            ui.set_history(ModelRc::new(VecModel::from(
                rows.iter()
                    .enumerate()
                    .map(|(i, (voice, meta, text))| HistItem {
                        id: format!("h{i}").into(),
                        voice: (*voice).into(),
                        meta: (*meta).into(),
                        text: (*text).into(),
                        starred: i % 3 == 0,
                    })
                    .collect::<Vec<_>>(),
            )));
        } else {
            ui.set_tab("lib".into());
            ui.set_lib_count_line("8 clips · 3:41 total".into());
            ui.set_lib_rows(ModelRc::new(VecModel::from(
                rows.iter()
                    .enumerate()
                    .map(|(i, (title, meta, text))| LibItem {
                        id: format!("l{i}").into(),
                        title: (*title).into(),
                        meta: (*meta).into(),
                        text: (*text).into(),
                        starred: i % 3 == 0,
                        tags: if i % 2 == 0 { "demo, keep".into() } else { "".into() },
                    })
                    .collect::<Vec<_>>(),
            )));
        }
        return render(&ui, &out, w, h, &variant);
    }

    ui.set_st_mic_names(names(&["iMac Microphone", "BlackHole 2ch"]));
    ui.set_st_mic_index(0);
    // row 0 is the "let the app choose" default, worded per the matrix in
    // main.rs's tap_dropdown_names
    match variant.as_str() {
        "native" => {
            ui.set_st_mon_names(names(&["System audio (native tap)", "BlackHole 2ch"]));
            ui.set_st_mon_index(0);
        }
        "install" => {
            ui.set_st_mon_names(names(&["No loopback device found"]));
            ui.set_st_mon_index(0);
        }
        "none" => {
            ui.set_st_mon_names(names(&["Default sink monitor", "Monitor of Built-in Audio"]));
            ui.set_st_mon_index(0);
        }
        // the override: a driver row picked out of a native-capable list
        _ => {
            ui.set_st_mon_names(names(&["System audio (native tap)", "BlackHole 2ch"]));
            ui.set_st_mon_index(1);
        }
    }
    ui.set_st_mic_test_supported(true);
    // macOS has no dictation surface yet — keep the card off the page
    ui.set_dictation_supported(false);

    let (hint, copy) = match variant.as_str() {
        "install" => (
            "No loopback device. Install one:  brew install blackhole-2ch",
            "brew install blackhole-2ch",
        ),
        "none" | "native" => ("", ""),
        _ => (
            "A loopback tap hears only what is routed to it — a Multi-Output Device (Audio MIDI Setup) lets you hear it too.",
            "",
        ),
    };
    ui.set_st_tap_hint(hint.into());
    ui.set_st_tap_copy(copy.into());

    render(&ui, &out, w, h, &variant)
}

fn render(
    ui: &AppWindow,
    out: &str,
    w: u32,
    h: u32,
    variant: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    ui.window().set_size(PhysicalSize::new(w, h));
    ui.show()?;

    // take_snapshot() lays out and renders in one go — no event loop needed
    let buffer = ui.window().take_snapshot()?;
    image::save_buffer(
        out,
        buffer.as_bytes(),
        buffer.width(),
        buffer.height(),
        image::ColorType::Rgba8,
    )?;
    println!("wrote {out} ({}x{}, variant {variant})", buffer.width(), buffer.height());
    Ok(())
}

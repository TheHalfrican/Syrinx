//! Offscreen render of one AppWindow tab to a PNG.
//!
//! The software renderer needs no display server, no GPU and no macOS
//! Screen-Recording grant, so UI layout can be checked from a terminal — the
//! only way to see a window from an agent session or CI.
//!
//! Usage:
//!   cargo run -p syrinx-app --example settings_snapshot -- <out.png> [variant] [WxH]
//!
//! Variants (all park the window on ⚙ Settings with macOS-shaped state):
//!   hint     — a loopback driver is installed: routing-caveat line, no ⧉
//!   install  — no driver: the brew command plus the ⧉ that copies it
//!   none     — the Linux/Windows shape: no hint row at all

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

    ui.set_st_mic_names(names(&["iMac Microphone", "BlackHole 2ch"]));
    ui.set_st_mic_index(0);
    ui.set_st_mon_names(names(&["BlackHole 2ch"]));
    ui.set_st_mon_index(0);
    ui.set_st_mic_test_supported(true);
    ui.set_system_capture_supported(true);
    // macOS has no dictation surface yet — keep the card off the page
    ui.set_dictation_supported(false);

    let (hint, copy) = match variant.as_str() {
        "install" => (
            "No loopback device. Install one:  brew install blackhole-2ch",
            "brew install blackhole-2ch",
        ),
        "none" => ("", ""),
        _ => (
            "A loopback tap hears only what is routed to it — a Multi-Output Device (Audio MIDI Setup) lets you hear it too.",
            "",
        ),
    };
    ui.set_st_tap_hint(hint.into());
    ui.set_st_tap_copy(copy.into());

    ui.window().set_size(PhysicalSize::new(w, h));
    ui.show()?;

    // take_snapshot() lays out and renders in one go — no event loop needed
    let buffer = ui.window().take_snapshot()?;
    image::save_buffer(
        &out,
        buffer.as_bytes(),
        buffer.width(),
        buffer.height(),
        image::ColorType::Rgba8,
    )?;
    println!("wrote {out} ({}x{}, variant {variant})", buffer.width(), buffer.height());
    Ok(())
}

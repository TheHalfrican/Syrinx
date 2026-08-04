//! The macOS dictation feedback surface: a system sound plus a floating pill
//! that says what dictation is doing, over whatever app has focus.
//!
//! **The hard constraint is focus.** Dictation types into the *frontmost*
//! window; an indicator that activates the app, or takes key status, becomes
//! the window that eats the transcript. So this is not a Slint window: winit
//! windows activate on show and winit exposes no `NSPanel`. It is a raw
//! AppKit `NSPanel` built the way Apple's own HUDs are —
//!
//! * `NSWindowStyleMaskNonactivatingPanel | Borderless`: showing it never
//!   activates the process,
//! * shown with `orderFrontRegardless`, **never** `makeKeyAndOrderFront:` —
//!   ordering a window front is not the same as making it key, and only the
//!   latter moves focus,
//! * `ignoresMouseEvents:YES` + `becomesKeyOnlyIfNeeded:YES`, so the one
//!   remaining route to key status (a click) does not exist either: clicks
//!   pass straight through to the app underneath,
//! * `NSStatusWindowLevel` + `canJoinAllSpaces | fullScreenAuxiliary |
//!   stationary`, so it is visible over full-screen apps and on every Space
//!   without dragging the user's Space around.
//!
//! `focus_probe()` reports the three facts that prove it (see the smoke test
//! at the bottom): the panel's `canBecomeKeyWindow`, its `isKeyWindow`, and
//! who the system thinks is frontmost.
//!
//! **Threading.** `emit()` is called from the dictation worker (a background
//! thread). The sound is played right there — `NSSound` is not main-thread
//! bound and a cue that waited on the UI queue would not be a cue. Every
//! panel mutation is hopped to the main thread with
//! `slint::invoke_from_event_loop`, which is the same event loop `ui.run()`
//! drives and therefore the same thread AppKit demands; the panel and its
//! timers live in a main-thread `thread_local` because AppKit objects are
//! neither `Send` nor `Sync` and a `static` could not hold them.
#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBox, NSBoxType, NSColor, NSFont, NSFontWeightMedium, NSPanel, NSScreen,
    NSSound, NSStatusWindowLevel, NSTextAlignment, NSTextField, NSTitlePosition,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::dictation_cue::{line, DictationCue, Hud};

/// Pill geometry, in points. The width is fixed rather than fitted to the
/// text: a HUD that resizes as the state changes draws the eye to the motion
/// instead of the words, and the longest line (the Accessibility one) fits.
const PILL_W: f64 = 320.0;
const PILL_H: f64 = 34.0;
/// Above the bottom of the *visible* frame, so it clears the Dock. Bottom
/// centre is the one region no app puts its own chrome or its text cursor in,
/// which is what "unobtrusive but unmissable" means here.
const BOTTOM_GAP: f64 = 96.0;
/// Label size and its vertical inset inside the pill.
const FONT_PT: f64 = 13.0;
const LABEL_H: f64 = 18.0;
/// The counter ticks once a second; anything faster is motion for its own sake.
const TICK: Duration = Duration::from_secs(1);
/// How many just-played sounds to keep alive. `NSSound play:` is asynchronous
/// and a released sound stops mid-note, so the object has to outlive the call;
/// a short ring frees them without ever cutting one off (cues are seconds
/// apart, and two back-to-back is the worst case: stop → typed).
const SOUND_RING: usize = 4;

// ---------------------------------------------------------------- state

/// The live panel. Main thread only, hence the `thread_local` — `Retained<_>`
/// of an AppKit object is not `Send`, and `slint::Timer` is not either.
struct Overlay {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
    /// What is showing, and since when (the counter's origin).
    spec: Hud,
    since: Instant,
    /// Bumped on every state change, so a dismissal armed for an older state
    /// cannot hide a newer one that arrived while it was pending.
    generation: u64,
    /// Repeating while `spec.elapsed`, stopped otherwise.
    tick: slint::Timer,
}

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
    /// Sounds still playing. Thread-local because `emit` plays on whatever
    /// thread called it (the worker, in practice) and `Retained` is not `Send`.
    static PLAYING: RefCell<Vec<Retained<NSSound>>> = const { RefCell::new(Vec::new()) };
}

// ---------------------------------------------------------------- public

/// Announce a cue: sound now, panel as soon as the main thread is free.
/// Callable from any thread; every failure is swallowed, because feedback is
/// never allowed to be the thing that breaks dictation.
pub fn emit(cue: DictationCue) {
    play(cue);
    // Err only when the event loop is not running (early startup, or a
    // headless test) — there is no panel to update in that case anyway.
    let _ = slint::invoke_from_event_loop(move || show(cue));
}

// The pill has no explicit "hide": every branch of the state machine ends on a
// cue that dismisses itself (Typed/Blocked/Error), and the two that do not —
// Listening and Transcribing — are exactly the states the user is waiting
// inside. Idle is simply the absence of a panel.

// ---------------------------------------------------------------- sound

/// Play the cue's system sound, unless the user turned sounds off. The setting
/// is re-read per cue (a few reads per take) so a flip in ⚙ applies to the very
/// next press rather than the next launch. Volume is the system's — nothing
/// here touches `setVolume:`.
fn play(cue: DictationCue) {
    if !crate::load_config().dictation_sounds {
        return;
    }
    let name = NSString::from_str(cue.mac_sound());
    let Some(sound) = NSSound::soundNamed(&name) else {
        tracing::debug!("dictation: system sound {:?} not found", cue.mac_sound());
        return;
    };
    if !sound.play() {
        tracing::debug!("dictation: system sound {:?} refused to play", cue.mac_sound());
        return;
    }
    PLAYING.with_borrow_mut(|ring| {
        ring.push(sound);
        if ring.len() > SOUND_RING {
            ring.remove(0);
        }
    });
}

// ---------------------------------------------------------------- panel

/// Apply a cue to the panel. Main thread only.
fn show(cue: DictationCue) {
    let Some(mtm) = MainThreadMarker::new() else {
        return; // unreachable via invoke_from_event_loop; cheaper than a panic
    };
    let spec = cue.hud();
    OVERLAY.with_borrow_mut(|slot| {
        let overlay = slot.get_or_insert_with(|| build(mtm));
        overlay.spec = spec;
        overlay.since = Instant::now();
        overlay.generation = overlay.generation.wrapping_add(1);
        paint(overlay);

        // Re-place on every show: the main screen (and the Dock's size with it)
        // can have changed since the last take.
        place(&overlay.panel, mtm);
        overlay.panel.orderFrontRegardless();

        if spec.elapsed {
            overlay.tick.start(slint::TimerMode::Repeated, TICK, || {
                OVERLAY.with_borrow_mut(|slot| {
                    if let Some(o) = slot.as_mut() {
                        paint(o);
                    }
                });
            });
        } else {
            overlay.tick.stop();
        }

        if let Some(after) = spec.dismiss {
            let armed = overlay.generation;
            slint::Timer::single_shot(after, move || hide_if(armed));
        }
    });
}

/// Hide only if nothing newer has been shown since this dismissal was armed.
fn hide_if(generation: u64) {
    OVERLAY.with_borrow_mut(|slot| {
        if let Some(o) = slot.as_mut() {
            if o.generation == generation {
                o.tick.stop();
                o.panel.orderOut(None);
            }
        }
    });
}

/// Push the current state into the label.
fn paint(overlay: &Overlay) {
    let text = line(&overlay.spec, overlay.since.elapsed());
    overlay.label.setStringValue(&NSString::from_str(&text));
    let (r, g, b) = overlay.spec.tint;
    overlay.label.setTextColor(Some(&srgb(r, g, b, 1.0)));
}

/// Bottom centre of the main screen's visible frame.
fn place(panel: &NSPanel, mtm: MainThreadMarker) {
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        return;
    };
    let vf = screen.visibleFrame();
    panel.setFrameOrigin(NSPoint::new(
        vf.origin.x + (vf.size.width - PILL_W) / 2.0,
        vf.origin.y + BOTTOM_GAP,
    ));
}

fn srgb(r: u8, g: u8, b: u8, a: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
        a,
    )
}

/// Build the panel once. Every property here is either the focus contract (see
/// the module docstring) or plain appearance.
fn build(mtm: MainThreadMarker) -> Overlay {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PILL_W, PILL_H));

    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        // Borderless has no title bar to activate through; NonactivatingPanel
        // is what lets the panel appear without the app coming forward.
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setLevel(NSStatusWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    // Clicks belong to whatever is underneath — and a panel that cannot be
    // clicked is a panel that cannot be made key by the user either.
    panel.setIgnoresMouseEvents(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setFloatingPanel(true);
    // The HUD outlives our own activation: it must stay up while the user is
    // typing into someone else's app, which is the only time it matters.
    panel.setHidesOnDeactivate(false);
    panel.setExcludedFromWindowsMenu(true);
    // The pill's corners are the NSBox's; the window behind them must be clear
    // or they show up as black notches.
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    // SAFETY: the inverse of the usual hazard — we hold a `Retained` for the
    // process's life, so the panel must NOT be released out from under it when
    // it is ordered out. Nothing else owns or closes this window.
    unsafe { panel.setReleasedWhenClosed(false) };

    let pill = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    pill.setBoxType(NSBoxType::Custom);
    pill.setTitlePosition(NSTitlePosition::NoTitle);
    pill.setCornerRadius(PILL_H / 2.0);
    pill.setBorderWidth(1.0);
    pill.setFillColor(&srgb(22, 22, 24, 0.94));
    pill.setBorderColor(&srgb(255, 255, 255, 0.16));
    pill.setContentViewMargins(NSSize::new(0.0, 0.0));

    let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    label.setFrame(NSRect::new(
        NSPoint::new(0.0, (PILL_H - LABEL_H) / 2.0),
        NSSize::new(PILL_W, LABEL_H),
    ));
    label.setAlignment(NSTextAlignment::Center);
    // Monospaced digits: the seconds counter must not shuffle the whole line
    // sideways every time it ticks.
    label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(FONT_PT, unsafe {
        NSFontWeightMedium
    })));
    label.setDrawsBackground(false);
    label.setBezeled(false);
    label.setEditable(false);
    label.setSelectable(false);

    // A direct subview rather than the box's contentView: the box would resize
    // a content view to its full bounds and the single line would then draw
    // against the top edge instead of centred.
    pill.addSubview(&label);
    panel.setContentView(Some(&pill));

    Overlay {
        panel,
        label,
        spec: DictationCue::Listening.hud(),
        since: Instant::now(),
        generation: 0,
        tick: slint::Timer::default(),
    }
}

// ---------------------------------------------------------------- proof

/// The live state of the panel, as the facts no offline test can assert: the
/// focus contract (can it ever become key, is it key, who is frontmost) and
/// the geometry actually in effect (a label whose frame collapsed, or a pill
/// off the visible frame, is invisible in exactly the same way as a bug).
/// Main thread only; `None` when no panel has been built yet.
///
/// This is what `SYRINX_DICTATION_CUE_DEMO=1` logs after each cue, and the
/// answer that matters is that `frontmost` never becomes Syrinx.
pub fn focus_probe() -> Option<String> {
    let _mtm = MainThreadMarker::new()?;
    OVERLAY.with_borrow(|slot| {
        let o = slot.as_ref()?;
        let front = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .and_then(|app| app.localizedName())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());
        let pf = o.panel.frame();
        let lf = o.label.frame();
        Some(format!(
            "canBecomeKeyWindow={} isKeyWindow={} isVisible={} frontmost={front:?} \
             panel=[{:.0},{:.0} {:.0}x{:.0}] label=[{:.0},{:.0} {:.0}x{:.0}] text={:?}",
            o.panel.canBecomeKeyWindow(),
            o.panel.isKeyWindow(),
            o.panel.isVisible(),
            pf.origin.x,
            pf.origin.y,
            pf.size.width,
            pf.size.height,
            lf.origin.x,
            lf.origin.y,
            lf.size.width,
            lf.size.height,
            o.label.stringValue().to_string(),
        ))
    })
}

/// Walk every cue on a timer, logging the focus probe after each — the whole
/// overlay path, driven without a microphone, an engine or the Accessibility
/// grant. Armed by `SYRINX_DICTATION_CUE_DEMO=1` and called from `main` once
/// the event loop is up; a no-op otherwise.
pub fn demo_if_asked() {
    if std::env::var_os("SYRINX_DICTATION_CUE_DEMO").is_none() {
        return;
    }
    const SCRIPT: [DictationCue; 5] = [
        DictationCue::Listening,
        DictationCue::Transcribing,
        DictationCue::Typed,
        DictationCue::Blocked,
        DictationCue::Error("Transcription failed"),
    ];
    tracing::info!("dictation cue demo: walking {} cues, 5 s apart", SCRIPT.len());
    for (i, cue) in SCRIPT.into_iter().enumerate() {
        slint::Timer::single_shot(Duration::from_secs(3 + 5 * i as u64), move || {
            emit(cue);
            // Twice: once the panel is up, and again 3.5 s in — the second
            // probe is what shows the elapsed counter actually repainting
            // (and, for the self-dismissing cues, the panel gone).
            for after in [Duration::from_millis(250), Duration::from_millis(3500)] {
                slint::Timer::single_shot(after, move || {
                    tracing::info!(
                        "dictation cue demo: {cue:?} +{}ms -> {}",
                        after.as_millis(),
                        focus_probe().unwrap_or_else(|| "no panel".into())
                    );
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pill_holds_the_longest_line_the_hud_can_show() {
        // The width is fixed, so the widest state has to fit it. At 13 pt the
        // system font averages well under 8 pt per character; the check is on
        // the character budget rather than on a live text measurement, which
        // no headless test can take.
        let longest = [
            DictationCue::Listening,
            DictationCue::Transcribing,
            DictationCue::Typed,
            DictationCue::Blocked,
            DictationCue::Error("Transcription failed"),
        ]
        .into_iter()
        .map(|c| line(&c.hud(), Duration::from_secs(600)).chars().count())
        .max()
        .unwrap();
        assert!(
            longest as f64 * 7.5 < PILL_W,
            "the widest HUD line is {longest} chars and would clip at {PILL_W} pt"
        );
    }
}

// Live AppKit smoke — not run in CI (needs a window server session and a
// running Slint event loop, which `cargo test` has neither of). The focus
// proof is the app-level demo instead:
//
//   SYRINX_DICTATION_CUE_DEMO=1 cargo run -p syrinx-app
//
// with TextEdit frontmost: every logged line must read frontmost="TextEdit".
#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    #[ignore = "needs a window server session — run manually"]
    fn live_system_sounds_all_resolve_and_play() {
        // Proves the names in `dictation_cue` exist on THIS machine and that
        // NSSound works off the main thread, which the worker relies on.
        for cue in [
            DictationCue::Listening,
            DictationCue::Transcribing,
            DictationCue::Typed,
            DictationCue::Blocked,
            DictationCue::Error("x"),
        ] {
            let name = NSString::from_str(cue.mac_sound());
            let sound = NSSound::soundNamed(&name)
                .unwrap_or_else(|| panic!("{} is not a system sound", cue.mac_sound()));
            assert!(sound.play(), "{} would not play", cue.mac_sound());
            println!("played {}", cue.mac_sound());
            std::thread::sleep(Duration::from_millis(900));
        }
    }
}

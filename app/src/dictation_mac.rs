//! macOS global dictation: Carbon `RegisterEventHotKey` + CGEvent unicode
//! injection, with a non-activating NSPanel HUD and system sounds for feedback
//! — the architectural twin of `dictation_win.rs`.
//!
//! Same shape as Windows, for the same reasons: the Linux `dictate/` crate is
//! gtk4 + zbus and never builds here, and the engine only exists while the app
//! supervises it (RPC-PROTOCOL.md §13). So dictation lives inside the app as the
//! *second* RPC client §1 anticipates — it opens its OWN `EngineClient` rather
//! than sharing the UI worker's connection. One chord toggles: press →
//! `StartRecording` (engine-side mic capture, §14); press again →
//! `StopRecording` → `Transcribe` → optional `RefineTranscript` (gated by the
//! shared `refine_dictation` settings key) → the text is typed into whatever app
//! has focus. Every failure logs and returns to idle; nothing here may crash the
//! app. The RPC surface is untouched — this is a client, not a protocol change.
//!
//! Every transition the user could be waiting on emits a `DictationCue`
//! (`dictation_cue.rs`), which `dictation_hud_mac.rs` turns into a system sound
//! and a line on a floating pill. v1 shipped without either and the chord was
//! indistinguishable from a dead key: nothing said the mic was open, nothing
//! said a 40 s refine was still running, and a missing Accessibility grant was
//! a log line. The cue calls below are the *only* place this module talks to
//! the user, and none of them can fail into the state machine.
//!
//! Three things are mac-specific:
//!
//! * **The chord is ⌃⌥D**, the literal twin of Windows' Ctrl+Alt+D (⌃ = Ctrl,
//!   ⌥ = Alt) and free of macOS's own defaults — ⌘⌥D is show/hide Dock and
//!   ⌃⌘D is look-up-in-dictionary, both of which this avoids. VoiceOver users
//!   should rebind: ⌃⌥ *is* the VoiceOver modifier, and VO-D is a VoiceOver
//!   command. Registration is non-exclusive, so a chord another app already
//!   owns is shared rather than stolen.
//! * **The hotkey is Carbon's**, not a `CGEventTap`. `RegisterEventHotKey` needs
//!   no TCC grant of any kind (a tap would need Input Monitoring) and fires
//!   while the app is in the background. Registration and dispatch belong to the
//!   **main thread**: the SDK header says "not thread safe", and the application
//!   event target is pumped by NSApplication's run loop, which winit owns — so
//!   `spawn()` runs there, before `ui.run()`, and the handler does nothing but
//!   hand the press to the worker (a long `Transcribe` must never stall the UI).
//! * **Injection needs the Accessibility grant** (kTCCServiceAccessibility) and
//!   `CGEventPost` returns `void`: a denied injection types nothing and says
//!   nothing. So the grant is checked *before* recording starts — the one thing
//!   Windows' UIPI-then-clipboard fallback cannot do — and an untrusted process
//!   prompts once, puts the hint in the ⚙ DICTATION card, and stays idle rather
//!   than recording words it could never deliver.
#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use objc2::runtime::AnyObject;
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventTapLocation};
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use syrinx_shared::{EngineClient, EngineEvent};
use tokio::sync::mpsc;

use crate::dictation_cue::DictationCue;
use crate::dictation_hud_mac as hud;

/// The chord, spelled for the ⚙ card. Shown, not configurable (v1, as on
/// Windows) — the ledger records the choice.
pub const HOTKEY_LINE: &str = "⌃⌥D  —  anywhere in macOS";
/// What an untrusted process must be told, in the ⚙ hint voice. The pane name
/// is the one System Settings uses.
pub const TCC_HINT: &str =
    "Typing the transcript needs Accessibility: System Settings > Privacy & Security > Accessibility";
/// Refinement can load the LLM on its first call (~40 s on CPU); wait generously
/// before giving up and typing the raw transcript. (Windows' number.)
const REFINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
/// UTF-16 units per injected event. Apple documents no cap — the event object
/// itself stores hundreds — but the *delivery* path truncates silently at about
/// 20 units, which three independent implementations report and none explains.
/// 16 keeps a margin, and a chunk boundary never falls inside a surrogate pair
/// (see `chunks`).
const CHUNK_UNITS: usize = 16;
/// Breathing room between injected events. Apps that translate keystrokes on
/// their own run loop drop a burst posted with none; the field's values run from
/// 0 to 20 ms and this sits at the low end of what is reported to work.
const INJECT_GAP: std::time::Duration = std::time::Duration::from_millis(4);

/// Presses, main thread → worker. Set once by `spawn`; the Carbon handler reads
/// it (a handler that fires before the channel exists is a no-op, not a crash).
static PRESSES: OnceLock<mpsc::UnboundedSender<()>> = OnceLock::new();
/// The Accessibility prompt is a one-shot per app run: it is asynchronous and
/// does not block, so a chord pressed repeatedly while untrusted would otherwise
/// stack dialogs.
static PROMPTED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------- natives

type OSStatus = i32;
type EventRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHotKeyRef = *mut c_void;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

/// Carbon four-char codes are big-endian packed ASCII.
const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// `kEventClassKeyboard` / `kEventHotKeyPressed` (HIToolbox/CarbonEvents.h).
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
/// `kVK_ANSI_D` (HIToolbox/Events.h).
const KEY_D: u32 = 0x02;
/// `optionKey | controlKey` — bits 11 and 12 of the classic modifier word.
const MOD_CTRL_OPT: u32 = (1 << 11) | (1 << 12);

// Carbon is the only framework here without an objc2 binding (objc2-carbon is an
// empty stub and nothing covers HIToolbox's events), so its four entry points
// are declared by hand. `ItemCount` is 64-bit — declaring it u32 is the classic
// way to get this wrong. Linking the umbrella `Carbon` is mandatory: ld refuses
// a direct HIToolbox link ("not an allowed client").
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: unsafe extern "C-unwind" fn(*mut c_void, EventRef, *mut c_void) -> OSStatus,
        num_types: u64,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
}

// The trust check is three symbols, so it stays raw FFI too rather than adding
// objc2-application-services for them (`Boolean` is a byte, not a Rust bool).
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// Returns whether *this* process is a trusted Accessibility client. With
    /// `kAXTrustedCheckOptionPrompt` the prompt is raised **asynchronously** and
    /// the return value is unaffected — an untrusted first call still says no.
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    fn AXIsProcessTrusted() -> u8;
    #[allow(non_upper_case_globals)]
    static kAXTrustedCheckOptionPrompt: *const NSString;
}

// ---------------------------------------------------------------- arming

/// Arm dictation. MUST be called on the main thread, before `ui.run()`: the
/// registration is documented not-thread-safe and the application event target
/// is dispatched by the run loop winit is about to start. Any failure is logged
/// and swallowed — dictation is never load-bearing.
pub fn spawn(ui: slint::Weak<crate::AppWindow>) {
    let (tx, rx) = mpsc::unbounded_channel::<()>();
    if PRESSES.set(tx).is_err() {
        return; // already armed
    }

    // Handler first: a hotkey whose handler is not installed yet is a beep.
    // The application target sees every hotkey THIS process registers, and we
    // register exactly one, so the handler needs no id check.
    let spec = EventTypeSpec {
        event_class: fourcc(b"keyb"),
        event_kind: K_EVENT_HOT_KEY_PRESSED,
    };
    let st = unsafe {
        InstallEventHandler(
            GetApplicationEventTarget(),
            hotkey_handler,
            1,
            &spec,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if st != 0 {
        tracing::warn!("dictation: could not install the hotkey handler (OSStatus {st})");
        return;
    }

    // options = 0, i.e. NOT kEventHotKeyExclusive: another app holding ⌃⌥D
    // keeps it and both are notified, which is friendlier than stealing it.
    let mut hotkey: EventHotKeyRef = std::ptr::null_mut();
    let st = unsafe {
        RegisterEventHotKey(
            KEY_D,
            MOD_CTRL_OPT,
            EventHotKeyID { signature: fourcc(b"SYRX"), id: 1 },
            GetApplicationEventTarget(),
            0,
            &mut hotkey,
        )
    };
    if st != 0 {
        tracing::warn!("dictation: hotkey ⌃⌥D unavailable (OSStatus {st})");
        return;
    }
    // The registration is deliberately never unregistered: it lives exactly as
    // long as the process, and the system reclaims it on exit.

    if let Err(e) = std::thread::Builder::new()
        .name("syrinx-dictation-worker".into())
        .spawn(move || worker(rx, ui))
    {
        tracing::warn!("dictation: could not spawn worker thread: {e}");
        return;
    }
    tracing::info!("dictation armed — press ⌃⌥D to start/stop");
}

/// The Carbon callback, on the main thread. It does exactly one thing —
/// non-blocking hand-off — so a 30 s transcription can never stall the UI's run
/// loop (or drop a later press).
unsafe extern "C-unwind" fn hotkey_handler(
    _call_ref: *mut c_void,
    _event: EventRef,
    _user_data: *mut c_void,
) -> OSStatus {
    if let Some(tx) = PRESSES.get() {
        let _ = tx.send(());
    }
    0 // noErr — handled
}

// ---------------------------------------------------------------- TCC

/// Is this process allowed to synthesize keystrokes? Query only — never
/// prompts, so ⚙ can ask on every settings load.
///
/// `CGEventPost` is gated by `kTCCServicePostEvent` rather than
/// `kTCCServiceAccessibility`, and `CGPreflightPostEventAccess` is its literal
/// query. Both live behind the *same* System Settings pane, and only the AX call
/// raises a prompt that points at it — so trust is asked here and the user is
/// sent to one place. If a live test ever shows the two disagreeing, swap this
/// body for the preflight; nothing else in the module changes.
fn trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

/// The ⚙ DICTATION card's line for a given trust state. Split from the query so
/// the untrusted path is exercised without revoking anyone's grant.
fn hint_for(trusted: bool) -> &'static str {
    if trusted {
        ""
    } else {
        TCC_HINT
    }
}

/// The ⚙ DICTATION card's Accessibility line: `""` once the grant is in place.
pub fn untrusted_hint() -> &'static str {
    hint_for(trusted())
}

/// Trust, checked at the moment of use, prompting at most once per app run. The
/// prompt belongs here rather than at launch: an app that asks for Accessibility
/// before the user has ever dictated is asking for nothing.
fn ensure_trusted(ui: &slint::Weak<crate::AppWindow>) -> bool {
    let trusted = if PROMPTED.swap(true, Ordering::SeqCst) {
        trusted()
    } else {
        let yes = NSNumber::new_bool(true);
        let key: &NSString = unsafe { &*kAXTrustedCheckOptionPrompt };
        let options: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[key], &[&**yes as &AnyObject]);
        // Toll-free bridged: NSDictionary * is a CFDictionaryRef.
        unsafe {
            AXIsProcessTrustedWithOptions(
                objc2::rc::Retained::as_ptr(&options).cast::<c_void>(),
            ) != 0
        }
    };
    // The card follows the real state in both directions — a grant made mid
    // session clears the hint on the next press without a relaunch (the trust
    // value does update live; only an MDM-profile grant needs a restart).
    let line = hint_for(trusted);
    ui.upgrade_in_event_loop(move |ui| ui.set_st_dictation_hint(line.into())).ok();
    trusted
}

// ---------------------------------------------------------------- worker

/// A live engine connection plus its event stream (broadcast notifications —
/// we filter `LlmResult` by req_id out of it during refinement).
struct Engine {
    client: EngineClient,
    events: mpsc::UnboundedReceiver<EngineEvent>,
}

/// The engine-side recording handle returned by `StartRecording`.
struct Recording {
    rec_id: String,
}

/// What a press means. Pure, so the transitions can be tested without Carbon, an
/// engine or the Accessibility grant — the worker takes every branch from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Press {
    /// Untrusted and idle: never start a recording that cannot be delivered.
    Blocked,
    /// idle → recording.
    Start,
    /// recording → transcribing → idle.
    Finish,
}

fn plan(recording: bool, trusted: bool) -> Press {
    match (recording, trusted) {
        // Already recording: always finish. The grant can only have been revoked
        // mid-take, and the injection re-checks it anyway.
        (true, _) => Press::Finish,
        (false, true) => Press::Start,
        (false, false) => Press::Blocked,
    }
}

/// The worker: owns a tokio runtime, a lazily-established engine connection, and
/// the toggle state machine. `session = None` is idle; `Some(_)` is recording;
/// the transcribing state is the span inside `stop_and_inject`, during which
/// further presses queue on the channel exactly as they do on Windows.
fn worker(mut rx: mpsc::UnboundedReceiver<()>, ui: slint::Weak<crate::AppWindow>) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("dictation: tokio runtime unavailable: {e}");
            return;
        }
    };
    rt.block_on(async move {
        // Connect lazily on the first press (keeps app startup untouched); reused
        // across toggles and re-established if the socket drops mid-session.
        let mut engine: Option<Engine> = None;
        let mut session: Option<Recording> = None;
        while rx.recv().await.is_some() {
            match plan(session.is_some(), ensure_trusted(&ui)) {
                Press::Blocked => {
                    // The ⚙ card already carries the hint, but a user who
                    // pressed the chord is not looking at ⚙ — say it where they
                    // are, since to them the key simply did nothing.
                    hud::emit(DictationCue::Blocked);
                    tracing::warn!(
                        "dictation: no Accessibility grant — nothing would be typed. {TCC_HINT}"
                    );
                }
                Press::Start => session = start(&mut engine).await,
                Press::Finish => {
                    if let Some(rec) = session.take() {
                        stop_and_inject(&mut engine, rec, &ui).await;
                    }
                }
            }
        }
    });
}

/// Ensure a connected engine, dialing on demand. Returns `None` (and leaves
/// `engine` unset) when the engine is unreachable — dictation stays idle.
async fn ensure_engine(engine: &mut Option<Engine>) -> Option<&mut Engine> {
    if engine.is_none() {
        match EngineClient::connect_rpc().await {
            Ok(client) => {
                let events = client.events();
                *engine = Some(Engine { client, events });
            }
            Err(e) => {
                tracing::warn!("dictation: engine unreachable ({e}) — is Syrinx's engine up?");
                return None;
            }
        }
    }
    engine.as_mut()
}

/// idle → recording. Returns the new session, or `None` on any failure (staying
/// idle). A transport failure clears the connection so the next press redials.
async fn start(engine: &mut Option<Engine>) -> Option<Recording> {
    let Some(eng) = ensure_engine(engine).await else {
        // Not a silent no-op: from the user's side an unreachable engine and a
        // dead hotkey look exactly alike.
        hud::emit(DictationCue::Error("Engine unreachable"));
        return None;
    };
    // "" device = system default input (§14).
    match eng.client.start_recording("").await {
        Ok(rec_id) if !rec_id.is_empty() => {
            hud::emit(DictationCue::Listening);
            tracing::info!("dictation: ● recording — ⌃⌥D again to stop");
            Some(Recording { rec_id })
        }
        Ok(_) => {
            hud::emit(DictationCue::Error("Microphone unavailable"));
            tracing::warn!("dictation: engine could not start recording (device busy/missing)");
            None
        }
        Err(e) => {
            hud::emit(DictationCue::Error("Could not start recording"));
            tracing::warn!("dictation: start_recording failed: {e}");
            *engine = None; // socket may be dead — force a fresh connect next press
            None
        }
    }
}

/// recording → idle: stop, transcribe, optionally refine, inject. Every failure
/// path logs and returns to idle; a dead socket clears the connection.
async fn stop_and_inject(
    engine: &mut Option<Engine>,
    rec: Recording,
    ui: &slint::Weak<crate::AppWindow>,
) {
    // Before the first await: the mic just closed, and the user needs to know
    // that *now* — the span that follows can run to 40 s on a cold refine, and
    // it is the whole reason the HUD counts seconds.
    hud::emit(DictationCue::Transcribing);

    let Some(eng) = engine.as_mut() else {
        hud::emit(DictationCue::Error("Engine unreachable"));
        tracing::warn!("dictation: engine lost before stop — recording abandoned");
        return;
    };

    let path = match eng.client.stop_recording(&rec.rec_id).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            hud::emit(DictationCue::Error("Recording was lost"));
            tracing::warn!("dictation: stop_recording returned no path");
            return;
        }
        Err(e) => {
            hud::emit(DictationCue::Error("Recording was lost"));
            tracing::warn!("dictation: stop_recording failed: {e}");
            *engine = None;
            return;
        }
    };

    let text = match eng.client.transcribe(&path).await {
        Ok(t) => t,
        Err(e) => {
            hud::emit(DictationCue::Error("Transcription failed"));
            // StopRecording already finalized (and now owns) the WAV in engine
            // scratch, so there is no live recording to cancel — the scratch file
            // is engine-owned and reaped there (§14).
            tracing::warn!("dictation: transcribe failed: {e} — discarding");
            if matches!(e, syrinx_shared::EngineError::Transport(_)) {
                *engine = None;
            }
            return;
        }
    };

    let text = text.trim().to_string();
    if text.is_empty() {
        // Not a failure, but it must not read as one either — a HUD that just
        // vanished would leave the user waiting for text that is never coming.
        hud::emit(DictationCue::Error("No speech detected"));
        tracing::info!("dictation: (no speech detected)");
        return;
    }
    tracing::info!("dictation: transcribed {} chars", text.chars().count());

    let text = if refine_enabled() { refine(eng, &text).await } else { text };
    inject(&text, ui).await;
}

/// The shared opt-in from the ⚙ tab (same `refine_dictation` key the Linux
/// `dictate` reads). Re-read every toggle so a mid-session change takes effect.
fn refine_enabled() -> bool {
    crate::load_config().refine_dictation
}

/// Run the transcript through the engine's refinement LLM. The result is not a
/// method return — `RefineTranscript` returns a req_id immediately and the text
/// arrives later as an `LlmResult` notification (RPC-PROTOCOL.md §4.3/§6), which
/// this module's own event stream receives. Any failure/timeout/empty result
/// falls back to the raw transcript so dictation never loses words.
async fn refine(eng: &mut Engine, raw: &str) -> String {
    // Drain stale broadcast notifications first: req_id is engine-global and
    // monotonic (a prior result would carry a different id), but draining also
    // keeps this unbounded channel from growing across a long session.
    while eng.events.try_recv().is_ok() {}

    let req_id = match eng.client.refine_transcript(raw).await {
        Ok(0) => return raw.to_string(), // engine rejected (empty) — keep raw
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("dictation: refine request failed: {e} — using raw transcript");
            return raw.to_string();
        }
    };

    let refined = tokio::time::timeout(REFINE_TIMEOUT, async {
        while let Some(ev) = eng.events.recv().await {
            if let EngineEvent::LlmResult { req_id: rid, text, error } = ev {
                if rid == req_id {
                    // error=true carries text="" — stop waiting and say why
                    // rather than letting it read as an empty refinement
                    if error {
                        tracing::warn!("dictation: refinement failed — using raw transcript");
                        return String::new();
                    }
                    return text;
                }
            }
        }
        String::new() // stream closed (socket dropped)
    })
    .await
    .unwrap_or_default(); // timed out

    let refined = refined.trim();
    if refined.is_empty() {
        tracing::warn!("dictation: refinement empty/timed out — using raw transcript");
        raw.to_string()
    } else {
        refined.to_string()
    }
}

// ---------------------------------------------------------------- injection

/// Type `text` into whatever has focus, as unicode keyboard events. Gated on the
/// Accessibility grant a second time: the check that guards the *start* of a
/// take is minutes old by now, and a revoked grant would otherwise type nothing
/// while looking like success.
async fn inject(text: &str, ui: &slint::Weak<crate::AppWindow>) {
    if !ensure_trusted(ui) {
        hud::emit(DictationCue::Blocked);
        tracing::warn!(
            "dictation: Accessibility grant lost — {} chars not typed. {TCC_HINT}",
            text.chars().count()
        );
        return;
    }
    for chunk in chunks(text) {
        post_unicode(&chunk);
        tokio::time::sleep(INJECT_GAP).await;
    }
    // Only after the last event is posted: the confirmation has to mean the
    // words are in, not that they are on their way.
    hud::emit(DictationCue::Typed);
}

/// Split `text` into UTF-16 payloads of at most `CHUNK_UNITS`. A char contributes
/// one unit, or two for an astral char (emoji) — and because the whole char is
/// weighed before the chunk is cut, a surrogate pair can never straddle two
/// events, which would type two replacement characters instead of one emoji.
fn chunks(text: &str) -> Vec<Vec<u16>> {
    let mut out: Vec<Vec<u16>> = Vec::new();
    let mut cur: Vec<u16> = Vec::new();
    let mut buf = [0u16; 2];
    for ch in text.chars() {
        let units = ch.encode_utf16(&mut buf);
        if cur.len() + units.len() > CHUNK_UNITS {
            out.push(std::mem::take(&mut cur));
        }
        cur.extend_from_slice(units);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// One down/up pair carrying `units` as the event's unicode payload. virtualKey
/// is 0 and no keycode is mapped, so the text lands identically under any
/// keyboard layout — a Dvorak or AZERTY user gets their transcript, not a
/// transliteration of it.
fn post_unicode(units: &[u16]) {
    for down in [true, false] {
        let Some(event) = CGEvent::new_keyboard_event(None, 0, down) else {
            return;
        };
        let event: &CGEvent = &event;
        // SAFETY: the pointer/length pair describes `units`, which outlives the
        // call (the event copies the string).
        unsafe {
            CGEvent::keyboard_set_unicode_string(Some(event), units.len() as _, units.as_ptr());
        }
        // No modifier may ride along: an injected event inherits the physically
        // held modifiers, and the chord's own ⌃⌥ can still be down — which turns
        // the transcript into commands in half the apps on the machine.
        CGEvent::set_flags(Some(event), CGEventFlags::empty());
        // The HID tap is the lowest insertion point: the keystrokes reach the
        // focused app the way a real keyboard's do, session taps included.
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reassemble the chunks the way the focused app sees them.
    fn rejoin(text: &str) -> String {
        let units: Vec<u16> = chunks(text).concat();
        String::from_utf16(&units).expect("chunks are valid UTF-16")
    }

    #[test]
    fn short_text_is_one_event() {
        let c = chunks("hello");
        assert_eq!(c.len(), 1);
        assert_eq!(String::from_utf16(&c[0]).unwrap(), "hello");
    }

    #[test]
    fn empty_text_yields_no_events() {
        assert!(chunks("").is_empty());
    }

    #[test]
    fn every_chunk_respects_the_event_payload_cap() {
        let long = "the quick brown fox jumps over the lazy dog, twice, for good measure";
        for chunk in chunks(long) {
            assert!(!chunk.is_empty());
            assert!(chunk.len() <= CHUNK_UNITS, "chunk of {} units", chunk.len());
        }
        assert_eq!(rejoin(long), long);
    }

    #[test]
    fn a_surrogate_pair_never_straddles_two_events() {
        // 15 ASCII units then an emoji: a naive fill would put the high surrogate
        // in chunk 0 (16th unit) and the low one in chunk 1 — two replacement
        // chars instead of a grinning face.
        let text = format!("{}😀", "a".repeat(CHUNK_UNITS - 1));
        let c = chunks(&text);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].len(), CHUNK_UNITS - 1); // the emoji did not fit — cut early
        assert_eq!(c[1], vec![0xD83D, 0xDE00]); // and rides whole in the next event
        assert_eq!(rejoin(&text), text);

        for chunk in &c {
            let first = chunk[0];
            let last = chunk[chunk.len() - 1];
            assert!(!(0xDC00..0xE000).contains(&first), "chunk starts on a low surrogate");
            assert!(!(0xD800..0xDC00).contains(&last), "chunk ends on a high surrogate");
        }
    }

    #[test]
    fn mixed_scripts_and_emoji_round_trip() {
        // Long enough to span many events, with accents, CJK and astral chars.
        let text = "héllo wörld — 音声入力 😀 café 🎙 sûr, ça marche; 日本語のテキストもそのまま。";
        for chunk in chunks(text) {
            assert!(chunk.len() <= CHUNK_UNITS);
        }
        assert_eq!(rejoin(text), text);
    }

    #[test]
    fn a_single_astral_char_is_its_own_event() {
        assert_eq!(chunks("😀"), vec![vec![0xD83D, 0xDE00]]);
    }

    #[test]
    fn an_untrusted_process_gets_the_pane_hint_and_a_trusted_one_gets_none() {
        assert_eq!(hint_for(true), "");
        assert!(hint_for(false).contains("Accessibility"));
        assert_eq!(hint_for(false), TCC_HINT);
    }

    #[test]
    fn untrusted_never_starts_a_recording() {
        // The whole point of the pre-flight check: no take is made that could
        // not be delivered.
        assert_eq!(plan(false, false), Press::Blocked);
    }

    #[test]
    fn the_toggle_alternates_while_trusted() {
        assert_eq!(plan(false, true), Press::Start);
        assert_eq!(plan(true, true), Press::Finish);
    }

    #[test]
    fn a_grant_revoked_mid_take_still_finishes() {
        // Finishing transcribes and re-checks trust at injection; abandoning the
        // recording here would lose the words instead.
        assert_eq!(plan(true, false), Press::Finish);
    }

    #[test]
    fn the_carbon_constants_are_the_documented_ones() {
        assert_eq!(fourcc(b"keyb"), 0x6B65_7962); // kEventClassKeyboard
        assert_eq!(MOD_CTRL_OPT, 0x1800); // optionKey | controlKey
        assert_eq!(KEY_D, 0x02); // kVK_ANSI_D
    }
}

// Live natives smoke — not run in CI (needs a window server session, and the
// second one asserts a *denied* TCC state). These prove the FFI declarations and
// the framework links against the real system, which no offline test can. Run:
//   cargo test -p syrinx-app -- --ignored dictation_mac --nocapture
#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    #[ignore = "needs a window server session — run manually"]
    fn live_hotkey_registration_succeeds() {
        // Registration alone needs no run loop and no GUI window; dispatch does
        // (that part belongs to the app). A nonzero status here means the chord
        // is unavailable or the Carbon link is wrong.
        let mut hotkey: EventHotKeyRef = std::ptr::null_mut();
        let st = unsafe {
            RegisterEventHotKey(
                KEY_D,
                MOD_CTRL_OPT,
                EventHotKeyID { signature: fourcc(b"SYRT"), id: 99 },
                GetApplicationEventTarget(),
                0,
                &mut hotkey,
            )
        };
        println!("RegisterEventHotKey(⌃⌥D) -> OSStatus {st}, ref {hotkey:?}");
        assert_eq!(st, 0, "⌃⌥D could not be registered");
        assert!(!hotkey.is_null());
    }

    #[test]
    #[ignore = "reads this machine's real TCC state — run manually"]
    fn live_trust_query_answers_and_matches_the_hint() {
        // The value depends on whether the *terminal* running the tests holds
        // the grant, so only the invariant is asserted: the card says something
        // exactly when the process cannot type.
        let t = trusted();
        println!("AXIsProcessTrusted() -> {t}");
        assert_eq!(untrusted_hint().is_empty(), t);
    }
}

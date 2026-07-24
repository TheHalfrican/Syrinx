//! Windows global dictation (v1): RegisterHotKey + SendInput, no pill overlay.
//!
//! The Linux `dictate/` crate is gtk4 + zbus and never builds here; and the
//! engine only exists while the app supervises it (RPC-PROTOCOL.md §13). So on
//! Windows dictation lives inside the app, as the *second* RPC client the spec
//! anticipates (§1 "the app, or in phase 3 the dictate binary") — it opens its
//! OWN `EngineClient` rather than sharing the UI worker's connection.
//!
//! Flow mirrors `dictate/src/main.rs` semantically: a single chord (Ctrl+Alt+D —
//! Win+D is taken by show-desktop) toggles recording. Press once →
//! `StartRecording` (engine-side mic capture, §14); press again → `StopRecording`
//! → `Transcribe` → optional `RefineTranscript` (gated by the shared
//! `refine_dictation` settings key) → inject the text into the focused window via
//! synthetic keystrokes, falling back to the clipboard when injection is blocked.
//!
//! Everything here is best-effort: a failed toggle logs and returns to idle, and
//! nothing in this module may crash the app.

use syrinx_shared::{EngineClient, EngineEvent};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

/// Our single hotkey registration id (namespaced per-thread; any nonzero value).
const HOTKEY_ID: i32 = 0xD1C7;
/// Virtual-key code for 'D' (VK is ASCII-upper for letters). Chord: Ctrl+Alt+D.
const VK_D: u32 = 0x44;
/// Refinement can load the LLM on its first call (~40 s on CPU); wait generously
/// before giving up and pasting the raw transcript.
const REFINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Arm dictation. Spawns a dedicated hotkey thread and returns immediately;
/// any failure is logged and swallowed (dictation is never load-bearing).
pub fn spawn() {
    if let Err(e) = std::thread::Builder::new()
        .name("syrinx-dictation".into())
        .spawn(hotkey_thread)
    {
        tracing::warn!("dictation: could not spawn hotkey thread: {e}");
    }
}

/// The message-pump thread. `RegisterHotKey` delivers `WM_HOTKEY` to the
/// *registering thread's* message queue, and Slint/winit owns the main thread's
/// event loop — so the hotkey needs its own thread with its own `GetMessageW`
/// pump. This thread does no engine I/O: it forwards each press to the worker so
/// a long-running `Transcribe` can never stall the pump (and drop a later press).
fn hotkey_thread() {
    let (tx, rx) = mpsc::unbounded_channel::<()>();
    if let Err(e) = std::thread::Builder::new()
        .name("syrinx-dictation-worker".into())
        .spawn(move || worker(rx))
    {
        tracing::warn!("dictation: could not spawn worker thread: {e}");
        return;
    }

    // No window is needed: a thread-queue registration (hwnd = None) plus a
    // thread-message `GetMessageW` (hwnd = None) is the whole mechanism.
    // MOD_NOREPEAT stops key-repeat from firing a storm of toggles while held.
    if let Err(e) =
        unsafe { RegisterHotKey(None, HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, VK_D) }
    {
        tracing::warn!("dictation: hotkey Ctrl+Alt+D unavailable ({e}) — another app may own it");
        return;
    }
    tracing::info!("dictation armed — press Ctrl+Alt+D to start/stop");

    let mut msg = MSG::default();
    loop {
        // GetMessageW blocks; returns >0 for a message, 0 on WM_QUIT, -1 on
        // error. We never post WM_QUIT, so this parks here for the app's life.
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match ret.0 {
            -1 => {
                tracing::error!("dictation: message pump error — hotkey disabled");
                break;
            }
            0 => break, // WM_QUIT
            _ => {
                if msg.message == WM_HOTKEY {
                    // Non-blocking: hand off and keep pumping.
                    let _ = tx.send(());
                }
            }
        }
    }
}

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

/// The worker: owns a tokio runtime, a lazily-established engine connection, and
/// the toggle state machine. `session = None` is idle; `Some(_)` is recording.
fn worker(mut rx: mpsc::UnboundedReceiver<()>) {
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
            match session.take() {
                None => session = start(&mut engine).await,
                Some(rec) => stop_and_inject(&mut engine, rec).await,
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
    let eng = ensure_engine(engine).await?;
    // "" device = system default input (§14).
    match eng.client.start_recording("").await {
        Ok(rec_id) if !rec_id.is_empty() => {
            tracing::info!("dictation: ● recording — Ctrl+Alt+D again to stop");
            Some(Recording { rec_id })
        }
        Ok(_) => {
            tracing::warn!("dictation: engine could not start recording (device busy/missing)");
            None
        }
        Err(e) => {
            tracing::warn!("dictation: start_recording failed: {e}");
            *engine = None; // socket may be dead — force a fresh connect next press
            None
        }
    }
}

/// recording → idle: stop, transcribe, optionally refine, inject. Every failure
/// path logs and returns to idle; a dead socket clears the connection.
async fn stop_and_inject(engine: &mut Option<Engine>, rec: Recording) {
    let Some(eng) = engine.as_mut() else {
        tracing::warn!("dictation: engine lost before stop — recording abandoned");
        return;
    };

    let path = match eng.client.stop_recording(&rec.rec_id).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            tracing::warn!("dictation: stop_recording returned no path");
            return;
        }
        Err(e) => {
            tracing::warn!("dictation: stop_recording failed: {e}");
            *engine = None;
            return;
        }
    };

    let text = match eng.client.transcribe(&path).await {
        Ok(t) => t,
        Err(e) => {
            // Cancel/cleanup: StopRecording already finalized (and now owns) the
            // WAV in engine scratch, so there is no live recording to cancel —
            // cancel_recording on a stopped id is a documented no-op (§14). The
            // scratch file is engine-owned and reaped there; we just drop it.
            tracing::warn!("dictation: transcribe failed: {e} — discarding");
            if matches!(e, syrinx_shared::EngineError::Transport(_)) {
                *engine = None;
            }
            return;
        }
    };

    let text = text.trim().to_string();
    if text.is_empty() {
        tracing::info!("dictation: (no speech detected)");
        return;
    }
    tracing::info!("dictation: transcribed {} chars", text.chars().count());

    let text = if refine_enabled() {
        refine(eng, &text).await
    } else {
        text
    };
    inject(&text);
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
    // keeps this unbounded channel from growing across a long session. Safe to
    // drain *before* issuing the request — our result can't have arrived yet.
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
            if let EngineEvent::LlmResult { req_id: rid, text } = ev {
                if rid == req_id {
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

/// Inject `text` into the focused window as synthetic keystrokes, falling back
/// to the clipboard when injection is blocked.
fn inject(text: &str) {
    let inputs = text_to_inputs(text);
    if inputs.is_empty() {
        return;
    }
    let expected = inputs.len() as u32;
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == expected {
        return;
    }
    // SendInput rarely "fails" in the return-value sense; the real miss is UIPI —
    // a target window owned by an elevated/higher-integrity process silently
    // discards synthetic input from our (normal) process, so `sent < expected`
    // (often 0). Mirror the Linux wtype→wl-copy fallback: park the text on the
    // clipboard and tell the user to paste it.
    tracing::warn!(
        "dictation: injected {sent}/{expected} key events (elevated target?) — using clipboard"
    );
    match copy_to_clipboard(text) {
        Ok(()) => tracing::info!("dictation: transcript on clipboard — press Ctrl+V to paste"),
        Err(e) => tracing::error!("dictation: clipboard fallback failed: {e}"),
    }
}

/// Build the SendInput event stream for `text`: a keydown+keyup pair per UTF-16
/// code unit, each flagged `KEYEVENTF_UNICODE` (wVk = 0, character in wScan).
/// Astral chars (emoji) encode to a surrogate *pair* — two units — and each unit
/// is sent as its own down/up pair, exactly how Windows reassembles them.
fn text_to_inputs(text: &str) -> Vec<INPUT> {
    let mut inputs = Vec::new();
    let mut buf = [0u16; 2];
    for ch in text.chars() {
        for unit in ch.encode_utf16(&mut buf) {
            inputs.push(unicode_input(*unit, false)); // key down
            inputs.push(unicode_input(*unit, true)); // key up
        }
    }
    inputs
}

/// One KEYEVENTF_UNICODE keyboard event carrying a single UTF-16 unit.
fn unicode_input(unit: u16, key_up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0), // must be 0 for KEYEVENTF_UNICODE
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Place `text` on the clipboard as CF_UNICODETEXT. On success the global block's
/// ownership transfers to the clipboard (we must not free it); on any failure we
/// free it ourselves. The clipboard is always closed before returning.
fn copy_to_clipboard(text: &str) -> windows::core::Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0); // NUL terminator required for CF_UNICODETEXT
    let bytes = std::mem::size_of_val(utf16.as_slice());

    unsafe {
        OpenClipboard(None)?;
        // Everything from here must run before CloseClipboard, so wrap it.
        let result = (|| -> windows::core::Result<()> {
            EmptyClipboard()?;
            let hmem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
            let dst = GlobalLock(hmem);
            if dst.is_null() {
                let _ = GlobalFree(Some(hmem));
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst as *mut u16, utf16.len());
            let _ = GlobalUnlock(hmem); // returns Err with a 0 error on success — ignore
            // SetClipboardData takes ownership of hmem on success; on failure we
            // still own it and must free it.
            if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hmem.0))) {
                let _ = GlobalFree(Some(hmem));
                return Err(e);
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Read the KEYBDINPUT out of an INPUT built by our constructors (union access
    // is sound here: we only ever store the `ki` variant).
    fn ki(input: &INPUT) -> KEYBDINPUT {
        assert_eq!(input.r#type, INPUT_KEYBOARD);
        unsafe { input.Anonymous.ki }
    }

    #[test]
    fn ascii_char_makes_a_down_up_unicode_pair() {
        let inputs = text_to_inputs("A");
        assert_eq!(inputs.len(), 2);

        let down = ki(&inputs[0]);
        assert_eq!(down.wVk, VIRTUAL_KEY(0)); // 0 is mandatory for UNICODE events
        assert_eq!(down.wScan, 0x41); // 'A'
        assert_eq!(down.dwFlags, KEYEVENTF_UNICODE);

        let up = ki(&inputs[1]);
        assert_eq!(up.wScan, 0x41);
        assert_eq!(up.dwFlags, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
    }

    #[test]
    fn non_ascii_bmp_char_rides_in_wscan() {
        // 'é' (U+00E9) is a single UTF-16 unit — one down/up pair.
        let inputs = text_to_inputs("é");
        assert_eq!(inputs.len(), 2);
        assert_eq!(ki(&inputs[0]).wScan, 0x00E9);
        assert_eq!(ki(&inputs[1]).dwFlags, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
    }

    #[test]
    fn emoji_surrogate_pair_sends_two_units() {
        // U+1F600 GRINNING FACE encodes to the surrogate pair D83D DE00 — two
        // UTF-16 units → two down/up pairs (4 events), in order.
        let inputs = text_to_inputs("😀");
        assert_eq!(inputs.len(), 4);
        assert_eq!(ki(&inputs[0]).wScan, 0xD83D); // high surrogate, down
        assert_eq!(ki(&inputs[0]).dwFlags, KEYEVENTF_UNICODE);
        assert_eq!(ki(&inputs[1]).wScan, 0xD83D); // high surrogate, up
        assert_eq!(ki(&inputs[2]).wScan, 0xDE00); // low surrogate, down
        assert_eq!(ki(&inputs[3]).wScan, 0xDE00); // low surrogate, up
        assert_eq!(ki(&inputs[3]).dwFlags, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
    }

    #[test]
    fn every_event_is_a_keyboard_unicode_event() {
        // Mixed ASCII + accented + astral: every unit carries UNICODE, wVk 0.
        for input in text_to_inputs("hi héllo 😀!") {
            let k = ki(&input);
            assert_eq!(k.wVk, VIRTUAL_KEY(0));
            assert!(k.dwFlags.contains(KEYEVENTF_UNICODE));
        }
    }

    #[test]
    fn empty_text_yields_no_events() {
        assert!(text_to_inputs("").is_empty());
    }
}

// Live injection smoke — not run in CI (needs a desktop + focus). Drives the
// *shipped* SendInput path into a control we fully own and read back, so the
// assertion is deterministic. (Win11's Notepad is a packaged app whose edit
// buffer has no WM_GETTEXT-readable child window, so it can't serve as the
// target — a classic top-level EDIT control can.) Run manually:
//   cargo test -p syrinx-app -- --ignored live_inject_roundtrip --nocapture
#[cfg(test)]
mod smoke {
    use super::*;
    use std::time::Duration;
    use windows::core::w;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, SendMessageW,
        SetForegroundWindow, ShowWindow, TranslateMessage, ES_MULTILINE, PM_REMOVE, SW_SHOW,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_GETTEXT, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    /// Drain and dispatch this thread's message queue so a system EDIT control
    /// actually processes the injected WM_KEYDOWN/WM_CHAR events.
    unsafe fn pump() {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    #[test]
    #[ignore = "creates a focused window and types into it — run manually on a desktop"]
    fn live_inject_roundtrip() {
        const SAMPLE: &str = "syrinx dictation test — héllo 😀";
        unsafe {
            // A top-level system EDIT control: predefined class, no registration.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""), // empty caption → the EDIT starts blank (it seeds from this)
                WS_OVERLAPPEDWINDOW | WS_VISIBLE | WINDOW_STYLE(ES_MULTILINE as u32),
                100,
                100,
                600,
                300,
                None,
                None,
                None,
                None,
            )
            .expect("create EDIT window");
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
            for _ in 0..20 {
                pump();
                std::thread::sleep(Duration::from_millis(20));
            }

            // Inject through the shipped path.
            let inputs = text_to_inputs(SAMPLE);
            let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            for _ in 0..40 {
                pump();
                std::thread::sleep(Duration::from_millis(10));
            }

            // Read the control back.
            let mut buf = [0u16; 512];
            let n = SendMessageW(
                hwnd,
                WM_GETTEXT,
                Some(WPARAM(buf.len())),
                Some(LPARAM(buf.as_mut_ptr() as isize)),
            );
            let readback = String::from_utf16_lossy(&buf[..n.0.max(0) as usize]);
            let _ = DestroyWindow(hwnd);

            assert_eq!(
                sent,
                inputs.len() as u32,
                "SendInput inserted {sent} of {} events",
                inputs.len()
            );
            assert_eq!(readback, SAMPLE, "EDIT read-back mismatch");
        }
    }
}

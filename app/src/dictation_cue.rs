//! What global dictation *tells the user*, in one place, for both in-app
//! platforms — the seam `dictation_mac.rs` and `dictation_win.rs` share.
//!
//! v1 shipped mute and invisible: the chord worked, and the only trace of a
//! take was text appearing (or not appearing) somewhere. Everything a user
//! needs to know — am I being heard, is it still working, did the grant stop
//! it — lived in `tracing` lines nobody reads. This module names those moments
//! as a five-variant enum and maps each one to a sound and an overlay line, so
//! the two platform modules emit *the same* cues at the same transitions and
//! only the rendering differs (mac: NSSound + a non-activating NSPanel, see
//! `dictation_hud_mac.rs`; Windows: MessageBeep, overlay still TODO).
//!
//! Everything here is pure — no AppKit, no win32 — so the whole mapping is
//! tested headlessly on any host.
#![cfg(any(target_os = "macos", target_os = "windows"))]

use std::time::Duration;

/// One user-visible moment in the dictation state machine.
///
/// `Error` carries its own reason because "something failed" is exactly the
/// message v1 already had and nobody could act on: the overlay says *which*
/// step gave up. The string is `'static` (a fixed vocabulary, chosen at each
/// call site) so a cue stays `Copy` and crosses the thread hop for free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictationCue {
    /// The mic is open. Stays up until the next press.
    Listening,
    /// Recording stopped; STT — and, if enabled, the refinement LLM — is
    /// running. This is the long one (a cold refine is ~40 s), so it is the
    /// state that must look alive rather than hung.
    Transcribing,
    /// The transcript reached the focused app. Auto-dismisses.
    Typed,
    /// Refused before recording: no Accessibility grant, so nothing could ever
    /// have been typed. Points at the pane.
    Blocked,
    /// A step gave up; the payload is the short user-facing reason.
    Error(&'static str),
}

/// How the overlay renders a cue. Colours are plain 8-bit sRGB triples rather
/// than any toolkit's type — the HUD is raw AppKit and has no Slint theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hud {
    /// Leading glyph. Kept inside the fallback fonts the app already ships.
    pub glyph: &'static str,
    /// The line itself.
    pub label: &'static str,
    /// Text/glyph tint — the state is legible at a glance, from colour alone.
    pub tint: (u8, u8, u8),
    /// `None` stays until the next cue replaces it; `Some` self-dismisses.
    pub dismiss: Option<Duration>,
    /// Append a live mm:ss counter. Only the two open-ended states get one —
    /// it is what turns "is this hung?" into "it has been working 12 s".
    pub elapsed: bool,
}

/// Long enough to be read across the screen, short enough not to linger over
/// the words it just typed.
const CONFIRM: Duration = Duration::from_millis(1800);
/// The blocked line names a System Settings pane; it gets read-a-sentence time.
const EXPLAIN: Duration = Duration::from_millis(5000);
/// Failures sit between the two.
const FAILED: Duration = Duration::from_millis(3200);

impl DictationCue {
    /// The overlay's rendering of this cue.
    pub fn hud(self) -> Hud {
        match self {
            DictationCue::Listening => Hud {
                glyph: "●",
                label: "Listening",
                tint: (255, 69, 58), // the universal recording red
                dismiss: None,
                elapsed: true,
            },
            DictationCue::Transcribing => Hud {
                glyph: "◍",
                label: "Working",
                tint: (255, 179, 64),
                dismiss: None,
                elapsed: true,
            },
            DictationCue::Typed => Hud {
                glyph: "✓",
                label: "Typed",
                tint: (86, 214, 121),
                dismiss: Some(CONFIRM),
                elapsed: false,
            },
            DictationCue::Blocked => Hud {
                glyph: "⚠",
                label: "Accessibility needed — see Settings",
                tint: (255, 214, 90),
                dismiss: Some(EXPLAIN),
                elapsed: false,
            },
            DictationCue::Error(why) => Hud {
                glyph: "⚠",
                label: why,
                tint: (255, 105, 97),
                dismiss: Some(FAILED),
                elapsed: false,
            },
        }
    }

    /// The macOS system sound for this cue, by the name `NSSound soundNamed:`
    /// resolves out of /System/Library/Sounds. Nothing is vendored and nothing
    /// goes through the engine: a cue has to land the instant the chord does,
    /// and the engine may not even be connected yet.
    ///
    /// The pairs are chosen to be told apart without looking: a bright tick
    /// opens, a blunt pop closes, a chime confirms delivery, and the two
    /// failures use the sounds macOS has always used for "no" and "error".
    // Both mappings compile on both platforms so either host can test the
    // whole table; only one of them is ever called in a given build.
    #[allow(dead_code)]
    pub fn mac_sound(self) -> &'static str {
        match self {
            DictationCue::Listening => "Tink",
            DictationCue::Transcribing => "Pop",
            DictationCue::Typed => "Glass",
            DictationCue::Blocked => "Funk",
            DictationCue::Error(_) => "Basso",
        }
    }

    /// `MessageBeep`'s uType for this cue, or `None` for a cue Windows should
    /// pass over in silence. Windows has exactly five system event sounds and
    /// no way to add a sixth, so `Typed` — the least load-bearing cue — is the
    /// one that goes without rather than doubling up on another state's sound.
    ///
    /// Values are the `MB_*` constants (WinUser.h); the win module hands them
    /// straight to `MessageBeep` so this stays testable off Windows.
    #[allow(dead_code)]
    pub fn win_beep(self) -> Option<u32> {
        match self {
            DictationCue::Listening => Some(0x0000_0040),  // MB_ICONASTERISK
            DictationCue::Transcribing => Some(0x0000_0000), // MB_OK
            DictationCue::Typed => None,
            DictationCue::Blocked => Some(0x0000_0030), // MB_ICONEXCLAMATION
            DictationCue::Error(_) => Some(0x0000_0010), // MB_ICONHAND
        }
    }
}

/// The overlay's single line: glyph, label, and — for the open-ended states —
/// how long it has been in this one.
pub fn line(hud: &Hud, elapsed: Duration) -> String {
    if hud.elapsed {
        format!("{}  {}   {}", hud.glyph, hud.label, mmss(elapsed))
    } else {
        format!("{}  {}", hud.glyph, hud.label)
    }
}

/// m:ss, uncapped minutes. A dictation take running past an hour is a mistake
/// the counter should show rather than wrap.
pub fn mmss(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cue in the enum, so the exhaustiveness assertions below are real.
    const ALL: [DictationCue; 5] = [
        DictationCue::Listening,
        DictationCue::Transcribing,
        DictationCue::Typed,
        DictationCue::Blocked,
        DictationCue::Error("Transcription failed"),
    ];

    #[test]
    fn only_the_open_ended_states_stay_up() {
        // The two states the user is waiting *inside* must not vanish on a
        // timer — that is the whole complaint this module answers.
        assert!(DictationCue::Listening.hud().dismiss.is_none());
        assert!(DictationCue::Transcribing.hud().dismiss.is_none());
        for cue in [
            DictationCue::Typed,
            DictationCue::Blocked,
            DictationCue::Error("x"),
        ] {
            assert!(cue.hud().dismiss.is_some(), "{cue:?} would never clear");
        }
    }

    #[test]
    fn only_the_open_ended_states_count_up() {
        for cue in ALL {
            let h = cue.hud();
            assert_eq!(
                h.elapsed,
                h.dismiss.is_none(),
                "{cue:?}: a ticking counter and an auto-dismiss are opposites"
            );
        }
    }

    #[test]
    fn every_cue_says_something_and_says_it_differently() {
        let mut seen: Vec<&str> = Vec::new();
        for cue in ALL {
            let h = cue.hud();
            assert!(!h.label.is_empty(), "{cue:?} has no line");
            assert!(!h.glyph.is_empty(), "{cue:?} has no glyph");
            assert!(!seen.contains(&h.label), "{cue:?} reuses a line");
            seen.push(h.label);
        }
    }

    #[test]
    fn the_blocked_line_names_the_fix() {
        // A user who can't dictate must be told where to go, not just that it
        // failed — the grant is the single most likely reason nothing happens.
        assert!(DictationCue::Blocked.hud().label.contains("Accessibility"));
    }

    #[test]
    fn an_error_shows_its_own_reason() {
        assert_eq!(DictationCue::Error("No speech detected").hud().label, "No speech detected");
    }

    #[test]
    fn start_and_stop_never_share_a_sound() {
        // Distinct by ear is the requirement: the user must know which edge of
        // the toggle they just hit without looking at the screen.
        let mut sounds: Vec<&str> = ALL.iter().map(|c| c.mac_sound()).collect();
        sounds.sort_unstable();
        let n = sounds.len();
        sounds.dedup();
        assert_eq!(sounds.len(), n, "two cues share a system sound");
    }

    #[test]
    fn the_mac_sounds_are_all_stock_system_sounds() {
        // Nothing is vendored into the repo, so every name must be one macOS
        // ships in /System/Library/Sounds (the 14 that have been there since
        // 10.x). A typo here is a silent cue, not an error.
        const STOCK: [&str; 14] = [
            "Basso", "Blow", "Bottle", "Frog", "Funk", "Glass", "Hero", "Morse", "Ping", "Pop",
            "Purr", "Sosumi", "Submarine", "Tink",
        ];
        for cue in ALL {
            assert!(STOCK.contains(&cue.mac_sound()), "{cue:?} names a nonexistent sound");
        }
    }

    #[test]
    fn the_windows_beeps_are_documented_mb_constants() {
        const MB: [u32; 5] = [0x00, 0x10, 0x20, 0x30, 0x40];
        for cue in ALL {
            if let Some(t) = cue.win_beep() {
                assert!(MB.contains(&t), "{cue:?} -> MessageBeep(0x{t:x}) is not an MB_* value");
            }
        }
        // The three the requirement names — start, stop, blocked — must all be
        // audible on Windows too; only the confirmation may be silent.
        assert!(DictationCue::Listening.win_beep().is_some());
        assert!(DictationCue::Transcribing.win_beep().is_some());
        assert!(DictationCue::Blocked.win_beep().is_some());
        assert!(DictationCue::Typed.win_beep().is_none());
    }

    #[test]
    fn the_line_carries_the_counter_only_where_it_belongs() {
        let listening = DictationCue::Listening.hud();
        assert_eq!(line(&listening, Duration::from_secs(7)), "●  Listening   0:07");
        let typed = DictationCue::Typed.hud();
        assert_eq!(line(&typed, Duration::from_secs(7)), "✓  Typed");
    }

    #[test]
    fn the_counter_reads_as_minutes_and_seconds() {
        assert_eq!(mmss(Duration::ZERO), "0:00");
        assert_eq!(mmss(Duration::from_secs(9)), "0:09");
        assert_eq!(mmss(Duration::from_secs(60)), "1:00");
        assert_eq!(mmss(Duration::from_secs(125)), "2:05");
        // A 40 s cold refine — the case the counter exists for — reads plainly.
        assert_eq!(mmss(Duration::from_secs(41)), "0:41");
        // Past an hour it keeps counting minutes rather than wrapping to 0:00.
        assert_eq!(mmss(Duration::from_secs(3_601)), "60:01");
    }
}

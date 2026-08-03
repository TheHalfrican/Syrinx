//! Live smoke test for the native Core Audio process tap.
//!
//! There is no way to prove system capture from a unit test — it needs audio
//! actually playing on the machine's real output — so this is the harness that
//! stands in for one. It starts `capture_mac::SystemTap`, waits, stops, and
//! prints the WAV's rate/duration/RMS. Feed the file to a spectrum check for
//! the rest (a 440 Hz sine in should come back out at 440 Hz).
//!
//! Usage:
//!   cargo run -p syrinx-app --example catap_probe -- <out.wav> [seconds]
//!
//! Run bare from a terminal it will record *silence*: TCC attributes
//! kTCCServiceAudioCapture to the responsible process (the terminal), and tccd
//! refuses outright — "without NSAudioCaptureUsageDescription key" — with no
//! prompt and no error back to Core Audio. To prove anything, drop this binary
//! into a bundle carrying that key and launch it through LaunchServices
//! (`open -W Probe.app --args …`), which is why the numbers also go to
//! `<out.wav>.txt`: `open` throws stdout away.

// The probe drives start/stop only; discard()/died() belong to the app's modal
// and watchdog paths, so they are dead here without being dead in the binary.
#[allow(dead_code)]
#[path = "../src/capture_mac.rs"]
mod capture_mac;

fn main() {
    #[cfg(not(target_os = "macos"))]
    eprintln!("catap_probe is macOS-only");

    #[cfg(target_os = "macos")]
    {
        let mut args = std::env::args().skip(1);
        let out = args.next().unwrap_or_else(|| "catap.wav".into());
        let secs: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3.0);

        let report = format!("{out}.txt");
        let say = |s: &str| {
            println!("{s}");
            use std::io::Write;
            if let Ok(mut f) =
                std::fs::OpenOptions::new().create(true).append(true).open(&report)
            {
                let _ = writeln!(f, "{s}");
            }
        };
        let _ = std::fs::remove_file(&report);

        say(&format!("native_tap_available = {}", capture_mac::native_tap_available()));
        let cap = match capture_mac::SystemTap::start(&out) {
            Ok(c) => c,
            Err(e) => {
                say(&format!("start failed: {e}"));
                std::process::exit(1);
            }
        };
        say(&format!("capturing {secs}s -> {out}"));
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
        let path = cap.stop();
        if path.is_empty() {
            say("capture died mid-run");
            std::process::exit(1);
        }

        let bytes = std::fs::read(&path).expect("read wav");
        let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        let pcm = &bytes[44..];
        let (mut sumsq, mut n, mut nonzero, mut peak) = (0f64, 0u64, 0u64, 0i16);
        let mut i = 0;
        while i + 1 < pcm.len() {
            let s = i16::from_le_bytes([pcm[i], pcm[i + 1]]);
            if s != 0 {
                nonzero += 1;
            }
            peak = peak.max(s.saturating_abs());
            let f = s as f64 / 32768.0;
            sumsq += f * f;
            n += 1;
            i += 2;
        }
        let rms = (sumsq / n.max(1) as f64).sqrt();
        say(&format!(
            "path={path} rate={rate} dur={:.3}s samples={n} nonzero={nonzero} peak={peak} rms={rms:.6}",
            n as f64 / rate as f64
        ));
    }
}

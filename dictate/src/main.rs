//! syrinx-dictate — global dictation for Hyprland.
//!
//!   bind = SUPER, D, exec, syrinx-dictate toggle
//!
//! Press once to start recording, again to stop → transcribe (engine's
//! whisper over D-Bus) → paste into the focused window. State lives in a small
//! file under $XDG_RUNTIME_DIR, so the two key-presses are independent runs.
//!
//! Native-Hyprland stack: pw-record (PipeWire capture), the engine's
//! faster-whisper for STT, and wtype (virtual-keyboard protocol) for injection.
//! The wlr-layer-shell visual pill is the next increment — TODO(syrinx).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use syrinx_shared::EngineProxy;

fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}
fn state_path() -> PathBuf {
    runtime_dir().join("syrinx-dictate.state")
}
fn wav_path() -> PathBuf {
    runtime_dir().join("syrinx-dictate.wav")
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    match std::env::args().nth(1).as_deref().unwrap_or("toggle") {
        "toggle" => toggle(),
        "start" => start_recording(),
        "stop" => stop_and_transcribe(),
        other => {
            eprintln!("syrinx-dictate: unknown command '{other}' (toggle|start|stop)");
            std::process::exit(2);
        }
    }
}

fn toggle() -> Result<()> {
    if state_path().exists() {
        stop_and_transcribe()
    } else {
        start_recording()
    }
}

fn start_recording() -> Result<()> {
    let wav = wav_path();
    let _ = fs::remove_file(&wav);

    // pw-record captures until it receives SIGINT; 16 kHz mono is what whisper wants.
    let rec = Command::new("pw-record")
        .args(["--rate", "16000", "--channels", "1"])
        .arg(&wav)
        // Detach stdio so `start` returns cleanly (a caller piping our output
        // must not block waiting on the long-lived recorder/pill).
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn pw-record (is pipewire installed?)")?;

    // Show the layer-shell pill (best-effort — dictation works without it).
    let pill_pid = spawn_pill();

    fs::write(
        state_path(),
        format!("{}\n{}\n{}", rec.id(), pill_pid, wav.display()),
    )?;
    tracing::info!("● recording — run `syrinx-dictate toggle` again to stop");
    Ok(())
}

/// Spawn the pill overlay binary (sibling of this executable). Returns its PID,
/// or 0 if it couldn't start (dictation continues regardless).
fn spawn_pill() -> u32 {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("syrinx-dictate-pill")));
    match path.map(|p| {
        Command::new(p)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }) {
        Some(Ok(child)) => child.id(),
        Some(Err(e)) => {
            tracing::warn!("pill overlay unavailable: {e}");
            0
        }
        None => 0,
    }
}

fn stop_and_transcribe() -> Result<()> {
    let state = fs::read_to_string(state_path()).context("no recording in progress")?;
    let mut lines = state.lines();
    let rec_pid: i32 = lines
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .context("corrupt state file (rec pid)")?;
    let pill_pid: i32 = lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let wav = lines.next().unwrap_or("").trim().to_string();
    let _ = fs::remove_file(state_path());

    // Close the pill overlay, then SIGINT pw-record so it finalizes the WAV.
    if pill_pid > 0 {
        let _ = Command::new("kill").arg(pill_pid.to_string()).status();
    }
    let _ = Command::new("kill").args(["-INT", &rec_pid.to_string()]).status();
    std::thread::sleep(std::time::Duration::from_millis(400));

    let text = transcribe(&wav)?;
    let _ = fs::remove_file(&wav);
    let text = text.trim().to_string();
    if text.is_empty() {
        tracing::info!("(no speech detected)");
        return Ok(());
    }
    tracing::info!("transcribed: {text}");
    let text = if refine_enabled() {
        tracing::info!("refining transcript (LLM)…");
        refine(&text)
    } else {
        text
    };
    paste(&text)?;
    // TODO(syrinx): hide the pill.
    Ok(())
}

/// Opt-in LLM cleanup of the transcript: `--refine` anywhere in the args,
/// SYRINX_DICTATE_REFINE=1, or the Settings tab's "Refine transcript" toggle
/// (~/.config/syrinx/settings.json). Off by default — the LLM pass adds
/// ~8–15 s on CPU.
fn refine_enabled() -> bool {
    std::env::args().any(|a| a == "--refine")
        || std::env::var("SYRINX_DICTATE_REFINE").ok().as_deref() == Some("1")
        || config_refine()
}

/// The app's shared settings file — same one the ⚙ tab writes.
fn config_refine() -> bool {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        });
    let Ok(base) = base else { return false };
    let Ok(text) = std::fs::read_to_string(base.join("syrinx").join("settings.json")) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("refine_dictation").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// Run the transcript through the engine's refinement LLM (fillers out,
/// punctuation in). Any failure — no engine, timeout, empty output — falls
/// back to the raw transcript so dictation never loses words.
fn refine(raw: &str) -> String {
    let attempt = || -> Result<String> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            use futures_util::StreamExt;
            let conn = zbus::Connection::session().await?;
            let proxy = EngineProxy::new(&conn).await?;
            // subscribe BEFORE the call so the result can't race past us
            let mut results = proxy.receive_llm_result().await?;
            let req_id = proxy.refine_transcript(raw).await?;
            if req_id == 0 {
                anyhow::bail!("engine rejected the transcript");
            }
            // generous timeout: the first call may load the model (~40 s on CPU)
            let deadline = std::time::Duration::from_secs(180);
            let refined = match tokio::time::timeout(deadline, async {
                while let Some(sig) = results.next().await {
                    if let Ok(a) = sig.args() {
                        if a.req_id == req_id {
                            // error=true carries text="" — bail so the fallback
                            // names the failure instead of reading as an empty
                            // refinement (and stop waiting immediately)
                            if a.error {
                                anyhow::bail!("engine reported an LLM failure");
                            }
                            return Ok(a.text.to_string());
                        }
                    }
                }
                Ok(String::new())
            })
            .await
            {
                Ok(res) => res?,        // inner finished — Ok text, or the error bail
                Err(_) => String::new(), // timed out → fall back as empty
            };
            Ok::<String, anyhow::Error>(refined)
        })
    };
    match attempt() {
        Ok(refined) if !refined.trim().is_empty() => refined.trim().to_string(),
        Ok(_) => {
            tracing::warn!("refinement returned nothing — pasting raw transcript");
            raw.to_string()
        }
        Err(e) => {
            tracing::warn!("refinement unavailable ({e}) — pasting raw transcript");
            raw.to_string()
        }
    }
}

/// Ask the engine to transcribe the WAV over D-Bus.
fn transcribe(wav: &str) -> Result<String> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let conn = zbus::Connection::session().await?;
        let proxy = EngineProxy::new(&conn).await?;
        Ok::<String, anyhow::Error>(proxy.transcribe(wav).await?)
    })
}

/// Inject text into the focused window. Prefer wtype (virtual-keyboard protocol,
/// works natively on wlroots/Hyprland); fall back to the clipboard.
fn paste(text: &str) -> Result<()> {
    if Command::new("wtype")
        .arg(text)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let _ = Command::new("wl-copy").arg(text).status();
    tracing::warn!("wtype not found — text copied to clipboard. Install `wtype` to auto-paste.");
    Ok(())
}

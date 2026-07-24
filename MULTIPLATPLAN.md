# Syrinx Multi-Platform Plan

Syrinx today is deliberately Linux-first: Wayland-native, D-Bus-wired, tuned for
CachyOS + Hyprland. This document is the roadmap for bringing the same codebase
— not a fork — to Windows and macOS once the Linux experience is fully polished.

**Status: planning only.** Nothing here blocks or changes Linux work. Every
"phase 1" item below is written so that doing it *improves* the Linux build too.

*Updated 2026-07-23:* the audit now covers the phase-2 stack that landed after
the first draft — the ⇄ Voice Converter (ChatterboxVC / Seed-VC / Vevo
engines), ♫ music mode (demucs + singing conversion + octave shift), the ▤
Library, ⚙ Settings (device pickers, live engine knobs), ✂ trim, and
conversion-recipe Regenerate. The sequencing gate at the bottom is now
effectively met.

---

## Principles

1. **One codebase, three platforms.** Platform differences live behind small,
   explicit seams (transport, audio capture, text injection) — never as forks
   of app or engine logic.
2. **Linux remains the reference platform.** Ports follow Linux polish, not the
   other way around.
3. **Linux-native mechanisms are features, not debt.** D-Bus, parecord monitor
   taps, the wlr-layer-shell dictation stack: these stay exactly as they are.
   Each seam is a *strategy point* selected by OS detection — Linux keeps its
   native implementation, Windows/macOS get their own behind the same
   interface. Nothing Linux-native gets replaced to make porting convenient.
4. **No webviews.** The Slint UI is the cross-platform story; that's why it was
   chosen. No Tauri, no Electron.
5. **The engine contract stays thin.** All portability work happens at the
   service/transport layer; ML modules (`tts.py`, `stt.py`, `llm.py`,
   `effects.py`, backends) must not grow platform conditionals beyond device
   selection.

---

## Portability audit

| Component | Today | Portability | Action |
|---|---|---|---|
| Slint UI (app/) | winit/femtovg | ✅ Win/mac/Linux native | Font fallback, HiDPI check |
| Theme system | 5 skins | ✅ | '95 skin: Tahoma → fallback chain on mac |
| File dialogs (rfd) | ✅ | ✅ native everywhere | none |
| Avatar pipeline (image crate) | ✅ | ✅ | none |
| **IPC: D-Bus (zbus / dbus_next)** | Linux session bus | Linux-native | **Keep on Linux.** Add a second transport (JSON-RPC over localhost) selected on Win/mac (see below) |
| Engine ML core (torch/transformers/faster-whisper/kokoro/pedalboard) | CPU/CUDA | ✅ pip-installable on all three | Device matrix (below) |
| Voice conversion: ChatterboxVC | in-engine (s3gen half of Chatterbox) | ✅ same torch stack | Device matrix (below) |
| Isolated-venv workers (LuxTTS · Seed-VC · Vevo) | subprocess, JSON-over-stdio, one venv each | ✅ pattern is portable | LuxTTS: verify k2 wheels per-OS. Seed-VC: pip package, portable (pins encoded in setup-seedvc.sh). Vevo/Vevo2: **git clone of Amphion + undeclared deps** — see risks |
| ♫ music mode (demucs split → convert → remix) | demucs inside the seedvc AND vevo venvs | ✅ demucs is pip/portable | Device matrix (below); remix/octave-shift math is pure numpy/librosa |
| ✂ trim + FileEnvelope + PlayFileAt | engine-side soundfile/wave slicing | ✅ pure Python | none |
| History / source clips / conversion recipes | sqlite + wav files + JSON columns | ✅ | none |
| ⚙ engine knobs (engine-settings.json, GetSettings/SetSetting) | plain JSON file | ✅ | paths seam covers it |
| Audio playback (sounddevice/PortAudio) | ✅ | ✅ | none |
| Mic + VC-source recording | app shells out to `parecord`; ⚙ device pickers list PipeWire sources/monitors | Linux-native | **Keep on Linux** (monitor taps are a feature). Win/mac: engine-side sounddevice recording + device enumeration behind the same capture/picker interface |
| System-audio capture (create-voice, ⇄ song capture) | `parecord --device=<sink>.monitor` | Linux-native | Win: WASAPI loopback · mac: loopback driver (BlackHole) · phase 3. Until then ♫ music mode is import-file-only on Win/mac |
| Dictation (dictate/) | pw-record + wtype + wlr-layer-shell + compositor keybind | Wayland-native **by design** | **Untouched, permanently.** Win/mac get separate implementations in phase 3; v1 ports ship without dictation |
| Paths | XDG (`~/.local/share/syrinx`, XDG_RUNTIME_DIR) | XDG | `platformdirs` (py) + `dirs` (rs) — these ARE OS detection and return the exact current XDG paths on Linux; zero Linux change |
| Process lifecycle | `setsid nohup` by hand | dev workflow | Linux: keep (optionally graduate to a systemd user unit / D-Bus activation — native polish). Win/mac: app spawns/supervises the engine |
| Packaging | cargo build + venv by hand | source-first | Per-OS installers, phase 2; Linux stays source-first |

Roughly 90% of the code needs zero changes.

---

## Phase 1 — Strategy seams (Linux paths stay untouched)

The rule for every seam: extract the *interface* the app/engine already
implies, keep the existing Linux implementation behind it verbatim, add a
Win/mac implementation next to it, select by OS detection (compile-time
`#[cfg]` in Rust, `sys.platform` in Python).

### 1.1 Transport: D-Bus on Linux, JSON-RPC over localhost elsewhere

- **Linux: unchanged.** zbus + dbus_next, same bus name, `busctl` debugging,
  the dictate binary keeps talking D-Bus. This also keeps the door open for
  D-Bus activation / a systemd user unit as future Linux-native polish.
- **Win/mac:** JSON-RPC 2.0 over a WebSocket on `127.0.0.1:<ephemeral port>`
  (framing + server-push in one well-supported package). Loopback-only plus a
  session token written to the app data dir.
- **The shared abstraction (the real work, needed for any approach):**
  - Rust: an `EngineClient` trait mirroring the surface in
    `shared/src/lib.rs`, with a unified event-stream enum for the signals
    (GenerationProgress, AudioLevel, PlaybackInfo/Progress, LlmResult,
    ModelProgress, TranscriptProgress/Result, SpeakStarted/Ended). Impl A
    wraps the existing zbus proxy; impl B is the RPC client
    (`tokio-tungstenite`). The app's `tokio::select!` loop consumes the
    unified stream and stops caring which transport feeds it.
  - Note the surface keeps growing (phase-2 added ConvertVoice, the source
    clip store, trim, PlayFile/PlayFileAt, tags, GetSettings/SetSetting —
    ~50 methods now); the trait is mechanical to extend, but this is exactly
    why the contract tests below are non-negotiable.
  - Python: extract `service.py`'s handlers into a transport-agnostic core;
    the dbus_next `ServiceInterface` and the RPC server become two thin
    mechanical wrappers over it. ML modules untouched.
- **Drift protection (the cost of two transports):** a contract test suite
  that runs the same method/signal exercises over BOTH wrappers in CI, so the
  Windows transport can never silently fall behind the Linux one.

### 1.2 Engine lifecycle, per-OS

- **Linux: unchanged** (manual/dev workflow today; optional future polish is a
  systemd user service or D-Bus activation — both *more* Linux-native, not
  less).
- **Win/mac:** the app spawns `syrinx-engine` as a supervised child process
  (restart on crash, shutdown on exit); the RPC handshake doubles as the
  readiness signal.

### 1.3 Recording, per-OS

- **Linux: unchanged.** `parecord` stays — the monitor-tap system-audio
  capture is a Linux feature worth protecting.
- **Win/mac:** engine methods (`StartRecording/StopRecording → wav`) using
  sounddevice input streams (WASAPI/CoreAudio via PortAudio), selected behind
  the same app-side capture interface. The create-voice modal UX is identical;
  the "System" capture buttons (create-voice, transcription, ⇄ converter)
  hide where unsupported (until phase 3). The ⚙ device pickers enumerate via
  sounddevice instead of PipeWire — same dropdown, different lister.

### 1.4 Paths

`platformdirs` (Python) + `dirs` (Rust) — these libraries ARE the OS switch:
on Linux they resolve to the exact XDG paths used today, so this seam changes
nothing on Linux by construction. `SYRINX_DATA_DIR` override keeps working
everywhere.

**Phase 1 exit criteria:** the full studio (voices, cloning, effects, history,
avatars, compose/rewrite/refine, Models tab, ▤ Library, ⚙ Settings, ✂ trim,
the ⇄ converter with Chatterbox VC + Seed-VC, ♫ music mode from imported
files) runs on Windows and macOS from a source checkout; the Vevo engines are
allowed to lag (optional, see risks); mic capture works, system capture and
dictation wait for phase 3; the Linux build behaves byte-for-byte as before,
still on D-Bus; the transport contract tests pass on both wrappers.

---

## Phase 2 — ML device matrix & packaging

### 2.1 Device matrix

| Backend | Linux | Windows | macOS |
|---|---|---|---|
| Kokoro | CPU ✅ / CUDA ✅ | CPU / CUDA | CPU / MPS |
| Qwen-TTS | CUDA ✅ | CUDA | MPS (verify) / CPU — consider MLX port later |
| LuxTTS (venv) | CPU ✅ / CUDA (k2 cuda wheels) | verify k2 Windows wheels; fallback CPU torch + k2 CPU wheel | verify k2 mac wheels (CPU) |
| faster-whisper (CTranslate2) | CPU ✅ / CUDA ✅ | CPU / CUDA | CPU (no Metal in CT2 — still fast) |
| Qwen3 LLM | CPU ✅ / CUDA fp16 ✅ | CUDA fp16 | **MPS fp16** (add "mps" to llm.py device pick) |
| Chatterbox VC (⇄) | CPU ✅ / CUDA ✅ | CPU / CUDA | MPS (verify — same stack as Chatterbox TTS) |
| Seed-VC (⇄ + ♫, venv) | CPU ✅ / CUDA ✅ | CPU / CUDA (plain pip torch) | MPS unverified; CPU works (slow — minutes per clip) |
| Vevo-Timbre / Vevo2 (⇄ + ♫, venv) | CPU ✅ / CUDA ✅ | CUDA (heavy — 10 GB-class resident) | unverified; treat as optional engines everywhere |
| demucs (♫ stem split) | CPU ✅ / CUDA ✅ | CPU / CUDA | CPU / MPS (demucs supports it) |
| pedalboard | ✅ | ✅ | ✅ |

VRAM note: the engine keeps **one VC worker resident at a time** (eviction on
engine swap) because a 24 GB card can't hold the TTS/STT/LLM stack plus two
conversion stacks. On unified-memory macs and CPU boxes the same eviction
policy is still right — it bounds RSS, not just VRAM.

Notes:
- Device selection is already centralized (`detect_device()`, per-module
  `torch.cuda.is_available()`): extend each with an MPS branch — a few lines.
- `models.py` hardware detection: report MPS/Metal as the GPU on mac.
- The k2 wheel index (k2-fsa.github.io) is the load-bearing dependency to
  verify per-OS *before* promising LuxTTS there; Qwen-TTS is the primary
  cloning engine on GPU boxes regardless.

### 2.2 Packaging

- **Windows:** embedded CPython + pre-built venv, Rust binaries, NSIS/MSIX
  installer; CUDA torch pulled on first run (or a "full" installer variant).
- **macOS:** `.app` bundle (Slint binary), bundled Python framework,
  codesign + notarization. Universal2 or arm64-only (decide; arm64-only is
  reasonable in 2026).
- **Linux:** stays source-first; optionally AUR package and/or Flatpak later
  (Flatpak complicates D-Bus/portals less once we're on localhost RPC).
- First-run model downloads already go through the Models tab — the installers
  ship no weights.
- **License boundaries survive packaging:** Seed-VC is GPL-3.0 and is never
  bundled — installers must reproduce the setup-seedvc.sh flow (install into
  an isolated venv on demand), exactly as on Linux. Amphion is MIT code but
  has no pip package — the per-OS installer replicates the setup-vevo.sh
  clone-outside-the-app flow. Vevo/Vevo2 and Seed-VC checkpoints are
  CC-BY-NC: auto-downloaded per user, never redistributed.
- The setup scripts are the source of truth for venv pins (`setuptools<81`,
  `huggingface_hub<1.0`, `transformers==4.57.x`, numba/k2, the undeclared
  Amphion deps) — per-OS packaging must encode the same pins, and each script's
  setup-time import proof is the pattern to keep: a bad combination must fail
  at install, not at first conversion.

---

## Phase 3 — Platform-native features

- **Dictation:** per-OS global hotkey + text injection:
  - Windows: RegisterHotKey + SendInput.
  - macOS: Carbon/NSEvent hotkey + CGEventPost (needs Accessibility grant).
  - The pill overlay is cosmetic; ship without it first.
- **System-audio capture:** Windows WASAPI loopback (supported by PortAudio
  builds / cpal); macOS requires a virtual loopback device (document BlackHole,
  detect its absence gracefully).
- **Auto-update:** optional; per-OS mechanisms differ, decide when packaging
  stabilizes.

---

## Risks / open questions

- **k2 wheel coverage** on Windows/mac (LuxTTS). Mitigation: LuxTTS is
  optional; Qwen-TTS covers cloning on GPU machines.
- **Qwen-TTS on MPS** — unverified; may need CPU fallback or an MLX-based
  backend for Apple Silicon.
- **The Amphion clone (Vevo/Vevo2)** is the least portable piece: research
  code imported from a git checkout via sys.path + cwd, with undeclared deps
  discovered one ModuleNotFoundError at a time (ipython, pyworld, einops,
  torchvision, praat-parselmouth, torchcrepe so far — all encoded in
  setup-vevo.sh) and a transformers pin NEWER than their own requirements.
  Native-wheel deps (pyworld, parselmouth, torchcrepe) need per-OS wheel
  checks. Mitigation: Vevo engines are optional; Chatterbox VC + Seed-VC
  cover the ⇄ tab on every OS.
- **Slint renderer quirks** per-OS (font metrics, HiDPI, the clip+radius
  offscreen behavior) — audit visually during phase 1 bring-up. The tiled
  half-width (`narrow`) layouts added 2026-07-23 key off window width alone,
  so they port as-is — include them in the visual audit.
- **Long-path/Unicode issues on Windows** for HF cache + profile dirs + the
  Amphion clone + worker data dirs (seed-vc's two-tier cache) — test with
  non-ASCII user names.
- **Engine cold-start UX** on first run (model downloads + venv) — needs a
  first-run screen rather than a silent wait. The Models tab's VOICE
  CONVERSION section (download/status/delete, re-inspect on visit) already
  covers the weights half of this.

## Non-goals

- No Tauri/Electron, no webview UI.
- No per-platform forks of app or engine logic.
- No removal or replacement of Linux-native mechanisms (D-Bus, parecord,
  the Wayland dictation stack) in the name of portability — seams select,
  they don't substitute.
- No cloud anything — Syrinx stays fully local on every OS.

---

## Sequencing gate

Phase 1 starts only after the Linux polish backlog is done. **As of
2026-07-23 that gate is met:** the app is feature-complete against the
original mockup (composer, effects chain editor, all tabs including the ⇄
converter, ▤ Library and ⚙ Settings), and the full stack — TTS, STT, LLM,
all three conversion engines, ♫ music mode — is validated on a CUDA desktop
(RTX 4090). The one remaining Linux-polish item, the beta desktop install
(release build + systemd user service + .desktop entry), is worth doing
*before* phase 1 since 1.2's lifecycle seam builds directly on it. Phase 1
can start whenever it's prioritized; until then, append findings here.

---

## Findings

**2026-07-24 — Phase 1.1 (transport seam) landed, on Windows.**
`docs/RPC-PROTOCOL.md` is the wire contract (65 methods / 2 properties /
10 signals — the "~50" above was an undercount). Engine: `core.py` holds the
transport-agnostic `EngineCore`; `service.py` is now a thin dbus_next shim
(introspection-verified byte-identical); `rpc.py` serves JSON-RPC over a
loopback WebSocket; `__main__.py` selects by platform (`SYRINX_TRANSPORT=
dbus|rpc|both` override). Rust: `EngineClient` enum in `shared/` (zbus impl
`#[cfg(unix)]`, tungstenite RPC impl everywhere), unified `EngineEvent`
stream; `app/` rethreaded onto it, call sites unchanged. Contract tests run
the same exercises over both wrappers with drift guards (285 pytest @ 95.77%,
34+5 cargo, clippy clean, `cargo check --target x86_64-unknown-linux-gnu`
validates the unix impl from Windows). Live smoke: real engine ↔ real Rust
client ↔ real app window, on Windows, torch-free venv. First Windows
portability fixes: `HistoryStore` relative paths now stored `as_posix()`
(Linux-identical), one test's `os.sysconf` monkeypatch. Next: 1.2 lifecycle
(app spawns engine on Win), 1.3 recording, 1.4 paths.

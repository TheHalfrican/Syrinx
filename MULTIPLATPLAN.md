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
| Isolated-venv workers (LuxTTS · Seed-VC · Vevo) | subprocess, JSON-over-stdio, one venv each | ✅ pattern is portable | LuxTTS: k2 is **moot** — not a dependency of current LuxTTS master; what actually has to hold per-OS is `piper_phonemize==1.4.7` (k2-fsa *icefall* find-links index) plus the two git SHAs, all encoded in setup-luxtts.sh/.ps1. Seed-VC: pip package, portable (pins encoded in setup-seedvc.sh). Vevo/Vevo2: **git clone of Amphion + undeclared deps** — see risks |
| ♫ music mode (demucs split → convert → remix) | demucs inside the seedvc AND vevo venvs | ✅ demucs is pip/portable | Device matrix (below); remix/octave-shift math is pure numpy/librosa |
| ✂ trim + FileEnvelope + PlayFileAt | engine-side soundfile/wave slicing | ✅ pure Python | none |
| History / source clips / conversion recipes | sqlite + wav files + JSON columns | ✅ | none |
| ⚙ engine knobs (engine-settings.json, GetSettings/SetSetting) | plain JSON file | ✅ | paths seam covers it |
| Audio playback (sounddevice/PortAudio) | ✅ | ✅ | none |
| Mic + VC-source recording | app shells out to `parecord`; ⚙ device pickers list PipeWire sources/monitors | Linux-native | **Keep on Linux** (monitor taps are a feature). Win/mac: engine-side sounddevice recording + device enumeration behind the same capture/picker interface |
| System-audio capture (create-voice, ⇄ song capture) | `parecord --device=<sink>.monitor` | Linux-native | Win: WASAPI loopback ✅ (2026-07-24, app-side `capture_win.rs` — the native twin of parecord; ◉/⚙/♫ affordances unhidden via `system-capture-supported`) · mac: loopback driver (BlackHole), still phase-3-future; ♫ music mode stays import-file-only on mac |
| Dictation (dictate/) | pw-record + wtype + wlr-layer-shell + compositor keybind | Wayland-native **by design** | **Untouched, permanently.** Win ✅ (2026-07-24, in-app `dictation_win.rs`: Ctrl+Alt+D + SendInput, no pill) · mac ✅ (2026-08-03, in-app `dictation_mac.rs`: ⌃⌥D via Carbon RegisterEventHotKey + CGEvent unicode injection, no pill; needs the Accessibility grant to type) |
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
| Kokoro | CPU ✅ / CUDA ✅ | CPU / CUDA | CPU / MPS ✅ (2026-07-30, M3) |
| Qwen-TTS | CUDA ✅ | CUDA ✅ (Base + CustomVoice, 1.7B & 0.6B) | MPS ✅ bf16 (2026-07-30, 0.6B clone+speak on the M3 — fp16 overflows in code_predictor sampling, see Findings) — consider MLX port later |
| LuxTTS (venv) | CPU ✅ / CUDA (plain pip torch) | ✅ one-click install (2026-07-28) via the `setup-luxtts.ps1`/`.sh` pair: the `piper_phonemize==1.4.7` cp312 win_amd64 wheel comes off the k2-fsa **icefall** find-links index (PyPI upstream ships no Windows wheel/sdist — that was the 2026-07-24 blocker), and LuxTTS/LinaCodec install from pinned git SHAs on the ysharma3501 fork | ✅ install + MPS synthesis (2026-07-30, M3): the icefall index DOES carry `piper_phonemize-1.4.7-cp312-cp312-macosx_11_0_arm64.whl`, so the one-click install works unchanged; worker picks mps (warm 2.1 s vs 3.4 s cpu) |
| faster-whisper (CTranslate2) | CPU ✅ / CUDA ✅ | CPU / CUDA ✅ (base/large/turbo — see cu12 DLL gotcha, Findings 2026-07-24 sweep) | CPU ✅ int8 (2026-07-30 — no Metal in CT2, still fast: 0.46 s for a 3.25 s clip) |
| Qwen3 LLM | CPU ✅ / CUDA fp16 ✅ | CUDA fp16 | MPS fp16 ✅ (2026-07-30, refine on the M3) |
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
- The load-bearing per-OS dependency for LuxTTS is **piper_phonemize**, not k2 —
  k2 is absent from current LuxTTS master's requirements. Its wheel comes from
  the k2-fsa *icefall* find-links index (cp37–cp314 win_amd64, espeak-ng-data
  and DLLs bundled in the wheel), verified on Windows 2026-07-28 and still
  unverified on mac; that index is the thing to check *before* promising LuxTTS
  on a new OS. Qwen-TTS is the primary cloning engine on GPU boxes regardless.

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
  `huggingface_hub<1.0`, `transformers==4.57.x`, `piper_phonemize==1.4.7` off
  the icefall find-links index, LuxTTS's two git SHAs, the undeclared
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

- **piper_phonemize wheel coverage on mac** (LuxTTS) — Windows is settled
  (icefall find-links index, proven 2026-07-28); the same index is what a mac
  build would have to serve, and the documented fallback if it ever goes dark is
  PyPI's `piper-phonemize-fix`. k2 is no longer part of this risk at all.
  Mitigation: LuxTTS is optional; Qwen-TTS covers cloning on GPU machines.
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

**2026-07-24 — Phase 1.2 (lifecycle seam) landed.** Contract in
RPC-PROTOCOL.md §13. Engine: `SYRINX_SUPERVISED=1` arms a stdin watchdog —
pipe closes ⇒ remove discovery file, `os._exit(0)`; unset ⇒ byte-identical.
**Gotcha earned:** a blocking stdin read deadlocks numpy's (and torch's)
C-extension DLL load on Windows — any thread with a pending read on fd 0
hangs the load. Watchdog polls `PeekNamedPipe` @200ms on win32, blocking
read on POSIX. App: `app/src/engine_proc.rs` — adopt-or-spawn (manual
engines adopted, never killed — dev engines survive quits, same as Linux),
exe resolution `SYRINX_ENGINE_CMD` → cwd venv → exe-ancestor venv → PATH,
spawn with piped-held stdin + CREATE_NO_WINDOW, stdout/err → data-dir
`engine.log`, crash ⇒ respawn 1s→30s backoff + reconnect + re-loads behind
the splash; quit teardown = the held stdin closing (covers hard kills).
Transport-selection cfgs narrowed `unix` → `target_os = "linux"` so a
future mac build lands on RPC+spawn (dictate stays unix/zbus). ⚙
stop-engine-on-quit card hidden off-Linux (`is-linux` slint property).
E2E-verified on Windows: cold-spawn over a stale rpc.json / mid-session
kill → auto-respawn / app kill → engine exits + file cleaned / manual
engine adopted and survives. 292 pytest @ 95.77%, 44+5 cargo, clippy zero.
Next: 1.3 recording (sounddevice), 1.4 paths (platformdirs/dirs).

**2026-07-24 — Phases 1.3 (recording) + 1.4 (paths) landed. Phase 1 seams
COMPLETE.** 1.3: four engine methods (RPC-PROTOCOL §14 — surface now 69),
`recording.py` RecordingManager (lazy sounddevice, name-based device ids,
latest-wins, device-native-rate PCM16 WAVs under data_dir/recordings/);
app capture seam cfg-selects parecord (Linux, verbatim) vs engine methods;
system-capture buttons + monitor picker + ♫ record-from-browser hidden
off-Linux (phase 3; import-file-only there). 1.4: `paths.py` central
resolver — **Linux branches hand-rolled, not platformdirs**, because
platformdirs honors XDG_DATA_HOME/XDG_CACHE_HOME and the historical
literals don't (byte-identity proven by tests, incl. the bare
~/.cache/syrinx-*.log worker logs); Win data converges on
%LOCALAPPDATA%\syrinx\syrinx beside rpc.json, app config →
dirs::config_dir(). Live on this box: cold app launch → spawned engine →
2s real mic capture over the wire → WAV in the new root → valid envelope;
teardown clean. 315 pytest @ 95.20%, 49 cargo, ruff/clippy zero. Phase-1
exit criteria met modulo full-studio ML validation, which awaits the CUDA
venv on Windows (environment, not seams). Next: phase 2 device matrix /
packaging, or Windows CUDA venv bring-up.

**2026-07-24 — Windows CUDA venv up; first device-matrix rows validated.**
torch 2.13.0+cu130 (Linux parity; the cu128 index tops out at 2.11) +
`engine[qwen]` + `numba>=0.60` resolved clean into engine/.venv. Live on
the 4090: Hardware→RTX 4090, backend cuda, kokoro Speak (1.4s warm to
playback), whisper-base Transcribe on CUDA (0.9s, correct text). Gotchas
earned: (1) **qwen-tts needs the `sox` BINARY at import** (pysox shells
out in `_get_valid_formats`) — winget ChrisBagwell.SoX fixes dev;
packaging must bundle it; (2) the ctranslate2 `cublas64_12.dll` failure is
the Linux cu12/cu13 split replayed — fix is `nvidia-cublas-cu12` +
`nvidia-cudnn-cu12` wheels (win_amd64 exist) BUT **only cublas/bin may go
on PATH: cudnn-cu12 resolving before torch's bundled cu13 cuDNN hits
CUDNN_STATUS_SUBLIBRARY_VERSION_MISMATCH** (one cudnn64_9.dll per
process; ct2 must reuse torch's) — codify as win32
`os.add_dll_directory(nvidia/cublas/bin)` in stt.py's lazy import;
(3) flash-attn not installed (Windows build ordeal) — qwen falls back to
manual attention, works; (4) `detect_hardware` reports ram_gb 0.0 on
Windows (no os.sysconf) — fix in models.py. Suite green with the full ML
stack installed (315 @ 95.66%).

**2026-07-24 — Phase 2 (device matrix + Windows packaging) COMPLETE.**
Windows/CUDA matrix, all validated live on the 4090 over RPC (warm times):
kokoro ✅ · qwen-tts-1.7B ✅ 6.2s (real clone flow; cold first-gen ~231s =
one-time import+load, splash-note it) · chatterbox ✅ 3.3s ·
chatterbox-turbo ✅ 1.2s (needs >5s reference audio — engine assert, not
platform) · tada-1b ✅ 0.9s (VRAM flat ×3 gens) · whisper-base ✅ 0.9s
CUDA (stt.py now self-serves the cuBLAS DLL dir — no PATH setup) ·
qwen3-1.7b LLM ✅ refine 4.7s · pedalboard ✅ · chatterbox_vc ✅ 0.9s ·
seed-vc ✅ 9.6s speech (♫ music mode untested — no legitimate music file
on the box; pipeline+demucs installed) · vevo-timbre ✅ 76s incl. weights
(heavy as predicted) · LuxTTS ❌ piper-phonemize (see matrix). Fixes
landed: stt.py add_dll_directory, models.py ram_gb via
GlobalMemoryStatusEx (63.7GB, was 0.0), qwen.py actionable sox error,
worker launchers per-OS (Scripts\ vs bin/). setup-seedvc.ps1 +
setup-vevo.ps1 mirror the .sh (authoritative) with a pin-drift pytest
guard; both prefer uv, fall back to pip. Packaging: scripts/
build-windows.ps1 → 146MB torch-free bundle (embedded CPython 3.12 +
sox) → packaging/windows/syrinx.nsi → dist/SyrinxSetup-x64.exe (34.8MB,
per-user, no UAC); first-run bootstrap pulls cu130-or-CPU torch;
installed-layout verified: app spawned its bundled engine to "engine
ready", uninstall preserves user data; GPL/NC boundaries hold (Seed-VC/
Amphion never bundled). New gotcha ledger: cmd AutoRun + Git-Bash find
shadowing find.exe stalls every MSVC sdist build (webrtcvad/pyworld/
parselmouth need BuildTools + clean PATH or vcvars); Seed-VC's HF cache
overflows MAX_PATH under deep dirs (real data dir is short; enable
LongPathsEnabled in packaging); pip console-script exes hardcode the
build-time interpreter (first-run reinstalls the entry point); embeddable
python needs setuptools + --no-build-isolation; hf_xet absent → slower
HF downloads (optional install). Suite 327 @ 95.60% with all stacks
installed. Remaining before phase 3: whisper-large/turbo +
CV-0.6B/tada-3b variants (mechanical), CI release job for the installer.

**2026-07-24 (later) — Linux data restored + first-user-session polish.**
The Linux snapshot (NAS: `Z:\Backups\Syrinx Data`) restored to
`%LOCALAPPDATA%\syrinx\syrinx` + `%APPDATA%\syrinx`; Piccolo/Frieza/Goku,
16 history rows, clips, active models all live on Windows (warmup
auto-fetched whisper-large + qwen3-4b). Polish batch from real usage, all
committed (8ff54a0..4f6fb35): platform titlebar chip + dictation hint
gated `is-linux`; **DPI compensation** — this panel is 300% native; app
targets Linux density by default off-Linux, `ui_scale` in
`%APPDATA%\syrinx\settings.json` overrides (set 2.0 here; ⚙ knob deferred
— SLINT_SCALE_FACTOR is read pre-window, would need restart-to-apply);
**bundled fallback fonts** (DejaVu Sans + 2.3KB Noto merge, fontique
`unstable-fontique-010`, cfg'd off Linux — 46/46 UI glyphs, tofu gone);
**avatar AND sample paths stored data-dir-relative** with lazy re-root of
restored absolute rows — full DB path audit: category CLOSED (vc_json
.source left inert/graceful by design); `windows_subsystem=windows` on
release (consoleless shortcut, debug keeps stdout); **cold-engine qwen
import race fixed** — warmup pre-imports the qwen stack (qwen-active
only, off-loop, non-fatal, before ModelLoaded) so first generation never
races; same mechanism could theoretically hit chatterbox/tada cold-first-
gen — unconfirmed; if seen, generalize to a per-backend preimport() hook.
Suite 336 @ 95.53%.

**NEXT SESSION — the three remaining Windows items:**
1. **Model-variant sweeps** (mechanical): whisper-turbo, qwen-tts-0.6B,
   qwen-custom-voice (both sizes), tada-3b-ml — download + one generation
   each on CUDA; update the matrix.
2. **Installer CI release job**: encode packaging/WINDOWS.md's exact steps
   in Actions (windows runner: cargo release build, build-windows.ps1,
   portable-NSIS makensis, artifact upload; no signing yet).
3. **Phase 3 on Windows**: WASAPI loopback system capture (unhides the ◉
   System buttons + ⚙ tap picker, ♫ record-from-browser) and dictation
   (RegisterHotKey + SendInput; pill overlay cosmetic — ship without).

**2026-07-24 — ♫ music mode validated on Windows/CUDA: the matrix is
done.** Real 31s song → demucs separation 4.8s → seed-vc f0 singing
conversion 49s → remix instant → auto-play at 55s total; recipe stored,
Regenerate-able. Every phase-2 row is now resolved; LuxTTS remains the
sole (documented) Windows exclusion. Dev QoL: "Syrinx (dev)" Start-Menu
shortcut → target\release\syrinx-app.exe with the repo as cwd (engine
resolves via the checkout venv; shares data + HF cache with everything
else).

**2026-07-24 — Model-variant sweep COMPLETE (Windows/CUDA on the 4090).**
The five leftover variants from the prior NEXT-SESSION list, all validated
live over RPC (warm = 2nd generation, model already resident; cold = 1st
gen incl. model load; downloads via the Models-tab DownloadModel path):
- **whisper-turbo** (deepdml/faster-whisper-large-v3-turbo-ct2, 1.6 GB) ✅
  Transcribe on CUDA cold 0.5s / warm 0.21s, text verbatim.
- **qwen-tts-0.6B** (1.2 GB) ✅ real clone flow (Piccolo profile, 0.6B
  prompt cache) cold 14.2s (incl. 0.6B load) / warm 8.8s.
- **qwen-custom-voice-1.7B** (3.5 GB) ✅ preset speaker Ryan, cold 16.2s /
  warm 8.4s (`SetActiveModel` lists the 9 CV presets as
  `builtin:qwen_custom_voice:<speaker>`).
- **qwen-custom-voice-0.6B** (1.2 GB) ✅ preset speaker Ryan, cold 14.2s /
  warm 7.0s.
- **tada-3b-ml** (~8 GB; tada-codec pre-cached from tada-1b) ✅ clone flow
  (Piccolo; existing size-agnostic `_tada.pt` codec prompt reused — TADA's
  cache keys on profile id, not size, and the codec encoding is
  size-independent), cold 10.0s (incl. 3B load) / warm 1.68s. TADA routing
  needs the profile's `default_engine` = tada (temporarily pinned via
  UpdateProfile, reverted after) — `clone_engine` alone is overridden by a
  profile's pinned engine.
Every catalogued Qwen-TTS size (1.7B/0.6B × Base/CustomVoice), TADA size
(1B/3B-ml), and whisper (base/large/turbo) now run on Windows CUDA; LuxTTS
stays the sole documented exclusion.

New gotchas earned:
1. **The phase-2 "stt.py self-serves the cuBLAS DLL dir — no PATH setup"
   claim does NOT hold for CT2 4.8.1 inference on the pure cu130 venv.**
   faster-whisper's `WhisperModel` CONSTRUCTS fine on CUDA, but the first
   GPU matmul (`encode`) dies with `RuntimeError: Library cublas64_12.dll
   is not found or cannot be loaded`. Two compounding causes, both
   verified: (a) CT2 4.8.1 loads cuBLAS only from **its own package dir**
   (`site-packages/ctranslate2/`, where its bundled `cudnn64_9.dll`
   already sits) — it ignores BOTH `os.add_dll_directory` user dirs
   (what stt.py does) AND `PATH` (neither made cublas resolvable); (b)
   even once found, `cublas64_12.dll` (nvidia-cublas-cu12 12.9.2.10)
   **delay-loads `cudart64_12.dll`** on first cublas call, and that
   runtime was **entirely absent** — torch 2.13.0+cu130 bundles
   cudart64_**13** (wrong version), and no nvidia-cuda-runtime-cu12 wheel
   was installed. Fix applied to engine/.venv: `pip install
   nvidia-cuda-runtime-cu12` (12.9.79, matches cublas 12.9) **and** copy
   `cublas64_12.dll` + `cublasLt64_12.dll` + `cudart64_12.dll` into
   `site-packages/ctranslate2/` beside `ctranslate2.dll`. Transcribe then
   works cold-fresh (0.5s). **This belongs in stt.py/packaging**: the
   `add_dll_directory` approach is insufficient; the reliable pattern is to
   stage the cu12 cublas+cudart DLLs next to `ctranslate2.dll` and pin
   nvidia-cuda-runtime-cu12 alongside nvidia-cublas-cu12. Corollary:
   phase-2's whisper-base "0.9s CUDA, no PATH" was environment-luck;
   whisper on CUDA was in fact broken on this venv until this fix.
2. **HF downloads race the symlink-support probe under concurrency.** This
   box lacks SeCreateSymbolicLink privilege (no Developer Mode), so
   huggingface_hub must use copy-mode. Running 4 `DownloadModel` calls
   concurrently in one engine process over one HF cache races the
   per-cache symlink-support detection, and some downloads wrongly attempt
   `os.symlink` on `.gitattributes` → `OSError [WinError 1314] A required
   privilege is not held by the client` → download "error". Run downloads
   **sequentially** and they consistently pick copy-mode and succeed
   (whisper-turbo + cv-0.6B both errored concurrently, both succeeded
   solo). CORRECTION (later 2026-07-24, measured across the whole cache):
   copy-mode does NOT double the footprint — modern huggingface_hub's
   fallback stores each file once, directly in snapshots/ with blobs/ left
   empty; the "1.2 GB down → 2.4 GB" comparison was against the catalog's
   size_mb *estimate*, and the repo is simply ~2.5 GB. Only one repo
   (Kokoro, older layout) carried a real 0.33 GB blob+snapshot duplicate,
   deduped to a symlink once Developer Mode was enabled.

**2026-07-24 — Phase 3 on Windows COMPLETE: system capture + dictation.**
Two Opus agents on disjoint ownership, orchestrator integration on top.
- **WASAPI loopback system capture** (`app/src/capture_win.rs`, ~430 lines):
  app-side, the native twin of Linux's parecord — IMMDevice render endpoint
  (default or the ⚙ System-tap pick) → IAudioClient SHARED+LOOPBACK →
  drain thread → mono PCM16 WAV (hand-rolled streaming writer, no new dep).
  Dry loopback reads are zero-padded to wall clock so the wav duration
  matches how long ◉ was held (WASAPI delivers nothing while the system is
  silent). `Capture` is now a Windows enum { Engine(mic rec_id) |
  Loopback }; mic capture still goes through the engine unchanged; the
  RPC surface is untouched. UI gates flipped from `is-linux` to a new
  `system-capture-supported` property (Linux+Win true): ◉ Record-system
  (TR + VC/♫), create-voice System chip, ⚙ System-tap picker (now listing
  render endpoints). macOS behavior byte-identical to before.
- **Dictation v1** (`app/src/dictation_win.rs`, ~530 lines): in-app, the
  second RPC client §1 anticipated (dictate/ is gtk4+zbus and stays
  Linux-only). Dedicated RegisterHotKey thread (Ctrl+Alt+D, MOD_NOREPEAT;
  pump does zero engine I/O) → engine mic recording (§14) → Transcribe →
  optional RefineTranscript (drains its own LlmResult notification stream
  by req_id, 180s timeout, falls back to raw) → SendInput KEYEVENTF_UNICODE
  (surrogate pairs handled) with CF_UNICODETEXT clipboard fallback. No pill
  (cosmetic, per plan). Every failure logs and resets to idle.
- **Verified**: clippy -D warnings clean; 52 unit/integration tests + 2
  live smokes green under orchestrator re-run (loopback: 3.01s captured
  from a real render stream, 98.7% nonzero, rms 0.073; injection: exact
  readback incl. é + 😀 surrogate pair); live app-level e2e — real app,
  real supervised engine, real chord: armed → ● recording → whisper on
  silence → "(no speech detected)" → idle, no crash, engine died with the
  app on exit.
- Gotchas earned: windows-crate WAVEFORMATEX is repr(packed) — read fields
  via addr_of!().read_unaligned() (E0793 otherwise); COM init needs an
  RAII guard that skips CoUninitialize on RPC_E_CHANGED_MODE; windows 0.62
  relocations (GlobalFree→Foundation, Error::from_win32→from_thread,
  BOOL→windows::core, CF_UNICODETEXT is CLIPBOARD_FORMAT — pass .0 as u32);
  Win11 packaged Notepad has no WM_GETTEXT-able child (test injection
  against a classic EDIT control — which seeds its buffer from the window
  caption, pass an empty title); clippy zombie_processes wants wait()
  after kill() even in tests.
- Remaining for a mac phase 3: BlackHole detection + Carbon/NSEvent
  hotkey + CGEventPost — nothing on Windows blocks it.

**2026-07-24 evening — post-phase-3 session close.** The user's first real
day driving the Windows build produced and closed five more items, all
pushed @ 4c6625c: DownloadModel serialization (the symlink-probe race,
ModelManager._fetch_lock); honest download progress (real hub byte totals
via HfApi files_metadata w/ estimate fallback, new "finalizing" stage +
finishing marquee, all 22 catalog size_mb re-measured — the stale
estimates had ALSO manufactured the debunked "copy-mode doubles the
cache" claim); a restore-swap gap class: the overnight data-dir swap
orphaned seed-vc's data-dir weights AND the vevo Amphion clone (worker
died at os.chdir while the Models tab showed no warning) — clone restored
via setup-vevo.ps1 re-run, _vc_setup_warning now requires venv AND clone;
and one genuine new feature: ⇄ speech pitch fine-tune (−6..+6 st dropdown
+ ⌖ auto-match via new SuggestPitchShift, wire surface 70 — engine
pre-shifts the SOURCE so profile timbre stays authentic; e2e: requested
+3 measured 2.60 st at the converter output). Developer Mode is now ON
on this box (symlink layout for future downloads).

**2026-07-25 — ◉ system capture was dead on arrival on Windows: the
scratch WAV path was still Linux-shaped.** First real click of any
◉ Record-system button (TR, ⇄ VC, create-voice System chip) did nothing —
all three app-side capture WAVs were built as `XDG_RUNTIME_DIR` →
`/tmp/syrinx-*.wav`, which on Windows resolves to a nonexistent `C:\tmp`,
so `WavWriter::create` → `Loopback::start` failed instantly; only the
loopback path was hit (mic capture's WAV is owned by the engine). The
failure was invisible: all three `capture_start` Err arms logged
`tracing::error!` only, and the release exe is consoleless. Fix: one
`scratch_wav(name)` helper (Linux arm byte-identical XDG→/tmp; elsewhere
`std::env::temp_dir()`), and every Err arm now also surfaces
"⚠ recording failed — try again" on the view's status line. Regression
test pins scratch paths to a directory that exists on the current
platform. **How it slipped:** phase-3 loopback was proven via the smoke
test (which writes to `temp_dir`) and the app-level e2e that day drove
the dictation chord, not the ◉ button — a "verified" module can still be
handed an impossible path by its caller; e2e the button, not the layer.
**Gotcha earned:** `cargo test -p syrinx-app --lib` fails ("no library
targets") — the app is a bin-only crate; use `--bins`. Verified: 57
tests green (+1), live smoke 3.000s @ 48kHz rms 0.072 captured into the
real temp dir, release exe rebuilt.

**2026-07-25 — UI polish wave (tooltips / 🗑 deletes / universal confirm)
+ the fallback font grows two glyphs.** The ⌖ auto-match button was tofu
on Windows — U+2316 is in neither bundled DejaVu nor the old four-glyph
SyrinxFallback subset (Linux fontconfig had been silently covering it).
Subset regrown to six glyphs (+⌖, +🗑) by the documented Noto recipe;
fontTools.merge chokes on vhea/vmtx asymmetry vs the hand-stripped
original — match the table set before merging. Then one Opus agent swept
main.slint: TBtn gained the IconBtn delayed-tooltip idiom (44/54 sites
tipped; label-redundant ones skipped), every delete ✕ became 🗑 (7 sites;
non-delete ✕s untouched), and the delete-voice confirm generalized to a
del-kind modal (voice|hist|capture|clip|model|fx) that every delete
dispatch routes through — pure Slint, Rust untouched. Slint gotchas
earned: children with x/y bindings are excluded from implicit layout-info
merging (i-slint-compiler default_geometry.rs — why the tipbox idiom
never inflates buttons), and modal stacking is declaration order (no
z-index) — a confirm reachable from inside another modal must be
declared after it. ⚠ Next Linux session: check 🗑 U+1F5D1 rendering —
fontconfig will likely hand it to Noto Color Emoji (color, ignores
Theme.dim); if so, switch labels to the U+FE0E text-presentation form.

**2026-07-25 — VRAM-aware model warnings (the 750 Ti defense).** The
Models tab warned on "no GPU" and low system RAM but was blind to VRAM —
a 4 GB card picking tada-3b got no warning, then either a raw CUDA OOM
or (worse, Windows) the driver's silent sysmem spill: the machine crawls
instead of failing. Now: detect_hardware() reports vram_gb; every
catalog row carries an advisory min_vram_gb (measured from the weight
files each backend ACTUALLY loads — the one variant it picks, at load
precision, ×1.3 headroom — from local snapshots where cached, HfApi
sums elsewhere; cross-engine extras like seed-vc's whisper-small and
Vevo2's whisper-medium counted); hardware_warning() composes "needs ~X
GB VRAM (have Y)" into the existing chip, only when a GPU exists and
VRAM is known (0 = unknown → silent, never warn on a guess). CUDA OOM
during Speak/ConvertVoice now surfaces as "out of GPU memory loading
<engine> — try a smaller model size" instead of a truncated allocator
dump. Advisory only — nothing gates. Known holes recorded, not fixed:
warmup() load failures still vanish (fire-and-forget task, no error
channel), LLM failures still deliver "" (LlmResult has no message
field), SetActiveModel never loads (lazy — OOM fires at first
generation), and CT2-on-ROCm would crash STT (torch.cuda true under
HIP but CT2 has no ROCm backend — force STT to CPU when a mac/AMD
session ever validates rocm). 376 pytest @ 95.59%.

**2026-07-25 — the three error-visibility holes closed (two Opus agents,
disjoint ownership, zero conflicts).** (1) LlmResult us→usb with an error
flag, mirroring the TranscribeResult precedent end to end: compose/
rewrite/refine failures now show ⚠ "…failed — check engine logs" instead
of silently delivering ""; Windows dictation falls back to the raw
transcript IMMEDIATELY on a flagged failure instead of waiting out its
180s timeout. (2) New ModelLoadError property (s, "" = healthy) + Get
transport method: warmup() load failures — previously "Task exception
was never retrieved" at GC with ModelLoaded stuck false and TOTAL
silence (the splash premise was wrong: booting drops unconditionally
after the first round-trip; nothing ever consumed ModelLoaded) — now
land in the composer's existing ⚠ banner, OOM-friendly via
_failure_text, and dbus_client gained its first property-change
subscription so Linux isn't blind. Drift guards bumped deliberately;
a new contract assertion pins PROPERTY_GETTERS == Get{props} so the
sets can't drift. (3) stt.py vetoes CT2-CUDA under ROCm
(torch.version.hip, same tell as detect_device) — torch backends keep
the AMD GPU, whisper drops to cpu/int8 instead of dying at load.
Verified: 387 pytest @ 95.57%, cargo 5+1/57 + clippy clean + linux
cross-check, live rpc_smoke printed GetModelLoadError -> "" from a real
scratch-dir engine. Follow-ups parked: dictate/src/main.rs (Linux-only)
still reads only a.text — mirror the immediate-fallback + compile-check
it next Linux session; RPC-PROTOCOL §0/§11 method-count drift (doc says
66, contract pins 70) needs a deliberate re-baseline; SetActiveModel
stays lazy BY DESIGN (OOM surfaces at first generation, now readably).

**2026-07-26 — a NAMED mic never worked on Windows (PortAudio's
four-host-API name collision).** Noah picked his headset in ⚙ and every
mic ◉ failed with "⚠ recording failed" — while "System default" worked,
which is exactly why the 7/24 e2e (default device) never caught it.
Root cause: Windows lists one physical device under MME / DirectSound /
WASAPI / WDM-KS with an IDENTICAL name; recording.py's name-based ids
(chosen for hotplug stability) hit sounddevice's own string matcher,
which rejects four exact matches as "Multiple input devices found" —
`InputStream` never opens, StartRecording returns "", and the deleted
scratch WAV was the only forensic trace (the per-spawn truncating
engine.log had eaten the exception). Fix in recording.py: `_resolve_input`
maps the persisted name to a concrete PortAudio index itself, tie-breaking
WASAPI > DirectSound > MME > WDM-KS (full names, native rates vs MME's
31-char truncation); unresolvable names fall through to PortAudio's
substring matching, `""` still means default; `_device_rate` now reads
the RESOLVED device so the WAV header matches the stream. fake_sd stub
taught the Windows shape (query_hostapis + per-API duplicate rows +
ambiguity ValueError), §14 StartRecording semantics documented. Verified:
389 pytest green (was 387) + ruff; live probe resolved the A50 X to
WASAPI idx 21 and captured 2s where the old path threw; engine chain
bounced under the app's supervisor (respawn clean, new rpc.json) and a
real RPC drive against the LIVE engine recorded 3.00s @ 48 kHz through
StartRecording/StopRecording with the exact persisted name. Gotcha
earned: engine.log is truncated per spawn (File::create) — any failure
from a previous engine incarnation is gone; check dir mtimes (the
recordings/ touch at 17:38 was the tell) before declaring "no evidence".
Note: the captured frames were digital zeros both runs — nobody spoke;
the A50 X hard-mutes with the boom up and its noise gate emits true
zeros, so an rms-0 capture is NOT itself a bug signal on this headset.

**2026-07-26 — ⚙ Test Mic lands (one vertical Opus agent; the §14 meter
the protocol reserved space for).** New `RecordingLevel` signal ("sd":
rec_id, rms 0..1) mirrors AudioLevel at every layer — recording.py grows
an `on_level` callback (66 ms monotonic throttle, int16→float32 widen
before squaring, raising meter can't kill the capture), core hops it to
the loop, both transports broadcast it, and the ⚙ tab gains a "◉ Test /
■ Stop" toggle + animated level bar under the mic dropdown (sqrt
perceptual scaling; 🎙 U+1F399 would be tofu in the bundled fonts so the
known-good ◉ stands in). Auto-off is a central `changed tab => {}` on
the root tab property (Slint 1.17 supports it — none of the 8 NavIcon
sites patched), backed by a 120 s client cap, session-end reset, and
stop-always-cancels so no scratch WAV survives; picking a new mic while
testing retargets seamlessly. Gated to Win/mac (queue item 4 below).
Gotchas earned: (a) `_emit` is NOT thread-safe from arbitrary threads —
AudioLevel's safety lives in audio.play's private call_soon_threadsafe
wrapper, so every new non-loop-thread emitter needs its own hop (the
contract test asserts delivery, which catches a missing one); (b)
np.square on int16 wraps silently above ~181 counts — widen first; (c) a
Slint toggle whose state lives in Rust has an in-flight blind spot —
flip the property optimistically before the await or a `changed tab`
guard reads stale false; (d) animate-width bars inside layouts need the
VerticalLayout{alignment:center} wrapper, not a y: binding (ledger's
x/y-exclusion rule). Doc re-baseline folded in: §0/§11 now 70 methods /
11 signals / 3 props (closes queue item 2). Verified independently:
392 pytest (was 389) + ruff, shared 6+1 (was 5+1), app 57, clippy +
linux cross-check clean, release exe rebuilt via rename-aside while the
old app ran, and a scratch-dir engine over real RPC delivered 32
RecordingLevel notifications in 2.5 s (~13 Hz, correct rec_id, 0..1
range) with zero WAVs left after cancel.

**2026-07-27 — the installer CI release job is PROVEN on the hosted
runner.** The first real `workflow_dispatch` run failed in "Pack NSIS
installer": SourceForge only 302s to a mirror for **CLI** user agents —
a browser-shaped UA (Invoke-WebRequest's default) gets the HTML
download page instead, which then died in Expand-Archive with an
empty-message OperationStopped. A HEAD probe DOES get the 302, which
is exactly how the step had been pre-verified yet still failed live;
the auto-selected mirror (cfhcable) was also down that day. Fix
(`cc63288`): fetch with curl.exe (real CLI UA, ships on the runner),
fall back across explicit mirrors (master/netcologne/phoenixnap), and
verify a pinned SHA-256 (`C7D27F78…394FA1`, taken from a good download
proven by a local `makensis /VERSION`) before makensis ever sees the
archive. Retry run green end to end on a warm rust-cache: winget SoX
behaved on the hosted runner (the one stated unknown), bundle build +
NSIS pack + silent-install smoke (installed embedded python imports
`syrinx_engine` torch-free) all passed, artifacts uploaded
(SyrinxSetup-x64 ~35 MB; unpacked bundle for no-rebuild debugging).
Every item on the Windows campaign list is now closed.

**2026-07-28 — the end-user install path is PROVEN on a clean CPU box
(and the firstrun script had a splat bug the CI smoke couldn't see).**
The CI-built SyrinxSetup-x64.exe (run 30295591666) installed fine, but
`syrinx-firstrun.ps1 -Cpu` died in the ML-stack step with pip
`ERROR: Invalid requirement: ':'`. Root cause is a two-layer PowerShell
gotcha (both editions): in the bundled-wheel branch `$mlSpec` was
assigned from an `if` that yields a single string, and splatting a
SCALAR string against a native command enumerates it char-by-char —
pip received ~100 one-character args and died on the drive colon. The
obvious fix (`@()` INSIDE the branch) does nothing: statement output
rides the pipeline, which unrolls a one-element array back to a scalar
at assignment. The `@()` must wrap the whole `if` (`9d4898a`),
argv-echo-verified — pip now gets exactly `['<wheel>[qwen]',
'numba>=0.60']`. CI never caught this because its silent-install smoke
is deliberately torch-free; the wheel branch of the ML pip step first
ran live here. Rerun green end to end on the CPU-only box: full ML
stack installed (kokoro, faster-whisper, qwen-tts, numba), import
proof `torch 2.13.0+cpu | cuda False`, "Syrinx is ready". Operational
gotcha earned: `Die` ends in `Read-Host`, so a failed firstrun launched
hidden/headless hangs forever on the prompt — kill by PID; also quote
the `-File` path (this user dir has spaces).

**2026-07-28 (later) — Vevo on-demand install exercised for the first
time on an INSTALLED (non-checkout) Windows box; four gaps found, one
workaround deployed.** (1) The install tree ships neither setup-*.ps1
script (engine/ = .venv + wheels only) — the checkout's copy was used;
the installer should ship both scripts (they're installers, not GPL/NC
payloads, so the license boundary is untouched). (2) The Models-view
warning and vevo.py's not-installed error hardcode "run
engine/setup-vevo.sh" on every platform (test_models.py pins the
string) — should say .ps1 on win32. (3) MAX_PATH, again: on the
installed layout `_ENGINE_DIR` resolves to `.venv\Lib\site-packages`,
and building `.venv-vevo` there overflows MAX_PATH — torch's dist-info
ships a ~160-char licenses tree (`third_party/kineto/…/DCGM/testing/
python3/libs_3rdparty/colorama`), pip dies with WinError 206 at ~250
chars. Workaround: build the venv at the SHORT path
`%LOCALAPPDATA%\syrinx\syrinx\vevo\.venv-vevo` (setup-vevo.ps1 copied
there — $PSScriptRoot puts the venv beside it; Amphion default already
lands in that dir) and JUNCTION it into site-packages. Runtime is safe
through the junction (worker python + torch DLL paths stay well under
260; only pip-install-time license paths explode), verified: torch
imports via the junction, `_vc_setup_warning("vevo-timbre")` == "".
Real fix candidate: default the vevo/seedvc venvs to the data dir on
win32 (kills the MAX_PATH nesting AND survives reinstalls). (4) This
box had no venv-capable python: PATH python is the Store stub, the
bundled embeddable python has no venv module — winget
`Python.Python.3.12` per-user + `SYRINX_VEVO_PYTHON` override. Not yet
proven: an actual conversion (checkpoints auto-download at first ⇄
run; CPU box, expect slow — 76 s on the 4090 incl. weights).

**2026-07-28 (later still) — one-click VC engine install: the four gaps
above are closed by product, not workaround, and a fifth was found in
design.** Landing in this change: `InstallVcEngine(setup_id) -> b` +
`CancelVcSetup(setup_id) -> b` and the `VcSetupProgress(setup_id, stage,
status, detail)` signal (`ssss`), driven from a new
`engine/syrinx_engine/vcsetup.py` (stdlib + `.paths` only) that runs the
existing per-OS setup scripts as a supervised child and streams their
`== syrinx-stage:` markers to the Models view; the row's dead-end
warning becomes an "Install…" button behind a consent dialog that states
the license terms (Seed-VC GPL-3.0 isolation; Vevo MIT code but CC-BY-NC
checkpoints, personal use, never redistributed). Surface goes 70/11 →
**72 methods / 12 signals**, lib.rs 84 → 87 fn; spec'd as RPC-PROTOCOL
§15. Two setup ids, `"seedvc"` and `"vevo"` — the latter clears both
vevo-timbre and vevo2-singing (shared `vevo_timbre` engine). Gap (1) is
closed in `scripts/build-windows.ps1` step 7: both `setup-*.ps1` are
copied into `<bundle>\engine\`, which the NSIS `File /r "${BUNDLE}\*"`
picks up with no `.nsi` change — installers, not payload, so the license
boundary is exactly where it was. Gap (2): the `.sh`-hardcoding dies
entirely rather than growing a win32 branch — one OS-agnostic string,
"one-time setup needed — click Install", with the backends' errors
pointing at the Models row instead of a shell command; `status()` rows
gain `setup_id`/`needs_setup` so the app never hardcodes the mapping.
Gap (3) gets the real fix the workaround note called for: on win32 the
venv defaults to the SHORT data-dir path (`SYRINX_VC_VENV_DIR` =
`%LOCALAPPDATA%\syrinx\syrinx\<sub>`), so pip's ~160-char torch
dist-info licenses tree never nests under site-packages. That makes this
box's junction **unnecessary but harmless** — its target IS the new
default path, so the resolver finds the same interpreter with or without
it (keep it: it keeps the pre-change state bisectable). Resolution also
moves from the venv *directory* to the *interpreter*
(`Scripts\python.exe` / `bin/python`), closing the dir-vs-interpreter
divergence a half-built venv could exploit. Gap (4) is productized: the
win32 resolver probes `SYRINX_*_PYTHON` → `py -3.12` → the usual fixed
install paths → PATH, requiring a real `import venv, ensurepip` at 3.12
(which rejects both the Store stub and the bundled embeddable), and only
then winget-installs `Python.Python.3.12` per-user and silently. NEW
fifth gap found in design and unremarked on the day: a stock Windows box
has no **git** either, and `setup-vevo.ps1` clones Amphion — so vevo
gets a `Git.Git` winget pre-flight (probe `git --version`, install,
re-probe `%PROGRAMFILES%\Git\cmd\git.exe`, prepend to the child PATH).
That one **may raise a UAC prompt**; accepted by Noah, and the consent
dialog says so before anything runs. Linux stays **byte-identical**, and
the argument is explicit rather than hopeful: the scripts' only new
construct is `VENV="${SYRINX_VC_VENV_DIR:+$SYRINX_VC_VENV_DIR/}.venv-x"`,
which with the var unset expands to the literal `.venv-x` the scripts
already used, and the engine sets that var on win32 only — same venv,
same location, same commands on the reference platform. The one
deliberate Linux behavior change is that the engine now passes
`SYRINX_VEVO_AMPHION=amphion_dir()` explicitly: `setup-vevo.sh` falls
back to a hardcoded `$HOME/.local/share/syrinx/vevo/Amphion` and
therefore ignored `SYRINX_DATA_DIR`, a latent divergence from every
other engine path — identical when `SYRINX_DATA_DIR` is unset (the
normal case), correct when it is not. v1 includes **Cancel**
(`CancelVcSetup` kills the child and emits `"cancelled"`, not an error
banner; the scripts are idempotent so a re-Install resumes), a
whole-install timeout (`SYRINX_VC_SETUP_TIMEOUT`, default 5400 s), a
`SYRINX_VC_SETUP_DIR` script-location escape hatch, and a per-setup log
at `worker_log_path("setup-<id>")` whose path is appended to any error
detail. Progress is stage-based, never a percentage — pip output isn't
measurable and pretending otherwise is a lying progress bar. macOS
inherits the POSIX `.sh` path design-ready but **unvalidated** — no Mac
has run this; it stays a phase-3 item.

**2026-07-28 (evening) — first live exercise of the one-click install
found two holes, both closed same-day.** Noah's first click on Install
(Seed-VC, dev build) died building webrtcvad — a 2017 sdist with no
cp312 Windows wheels, pulled in via seed-vc → resemblyzer. Two causes
stacked: (a) the dev app had been launched from a sandboxed agent
shell whose stripped env broke setuptools' vswhere discovery ("Unable
to find a compatible Visual Studio installation" with VS Community
2022 + SDK fully present — the same wheel builds fine from a normal
shell; engine_proc passes the app env through verbatim, so launch env
is destiny); a Start-Menu launch never hits this. (b) The REAL bug the
failure exposed: the venv+torch stages had succeeded, so the torn venv
had a working interpreter and `installed()` (interpreter-exists) called
it installed — warning cleared, Install button gone, worker broken.
Fixed: `installed()` now also requires a landmark site-packages dir
only the critical pip command leaves behind (seed_vc / torchcrepe —
pip is all-or-nothing per command), checked in the venv the interpreter
was actually found in; the hand-built Vevo venv grandfathers cleanly.
Also landed from the same session: Noah's "every error message should
be copy-able" — the three existing ⧉ copy buttons consolidated into a
CopyBtn component and the fourth added to the install-error banner.
QUEUED for CI: build webrtcvad's cp312 wheel on the MSVC-equipped
windows runner and ship it in the bundle + have setup-seedvc.ps1
install it first when present — an end user without Visual Studio
cannot build it, and that wall is otherwise the first thing the
one-click Seed-VC install hits on a stock box.

**2026-07-28 (night) — Seed-VC installed END TO END via the one-click
path; the webrtcvad wall closed for stock boxes; error UX productized.**
The install completed twice on this box — once driving VcSetupManager
directly, once through the full Explorer→app→engine→pwsh chain over
RPC — including a genuine webrtcvad compile, demucs, pins, and the
import proof. The earlier failures did NOT reproduce and are now
corroborated transient: the wheel-build agent independently hit the
same "Unable to find a compatible Visual Studio installation" ONCE
mid-testing and could not reproduce it either (suspect: the VS
installer instance store briefly locked; vswhere returns empty and
setuptools gives up). Error UX fixes from Noah's live run: NO_COLOR +
TERM=dumb on the setup child plus ANSI-stripping in the reader (the
shell that never showed garbage had NO_COLOR set — the app chain
didn't); _reason scans past PowerShell/pip decoration to the first
self-announcing error line, capped at 240 chars; the banner hugs its
text (a wrapping Text advertises unbounded max height, so stretch
alignment fed it the whole viewport — whole sections fell off-page).
STOCK-BOX FIX: build-windows.ps1 builds webrtcvad-2.0.10 cp312 as a
wheel (best-effort locally, greppable WARN; the embeddable python
CANNOT build it — no Include/ or python312.lib — so a full host 3.12
is probed, Store stub rejected by execution), release.yml pins
setup-python 3.12 and HARD-FAILS if the bundle lacks the wheel, and
setup-seedvc.ps1 pre-installs it inside the seedvc stage so pip's
resolver never reaches for the sdist (file path, not a pin token —
guards untouched; .sh deliberately has no counterpart, Linux compiles
it with gcc without comment). macOS note for phase 3: the compiler
there comes from the Xcode Command Line Tools — a stock Mac PROMPTS to
install them on first cc invocation, so Seed-VC's mac story is either
that prompt or bundling a mac-built webrtcvad wheel like Windows now
does. Decide when mac packaging exists.

**2026-07-28 (late) — LuxTTS joins the one-click installs; the Windows
matrix has no ❌ left.** Noah asked whether the webrtcvad wheel trick
could rescue LuxTTS; the probe found something better — no build
needed at all. PyPI's piper-phonemize is dead upstream (no sdist, no
win wheels; the `zipvoice` name on PyPI is an unrelated stub), but
LuxTTS's own requirements point at csukuangfj's (k2-fsa) icefall
find-links index, which ships maintained cp37–cp314 win_amd64 wheels
with espeak-ng + its data bundled inside — proven on this box
(1.4.7 cp312 installs, phonemizes; full dep chain dry-runs to wheels,
zero compiles; k2 itself is NOT a dep of current master, mooting the
audit's old "verify k2 wheels" line). Landed: setup-luxtts.{sh,ps1}
(stages venv→torch→phonemize→luxtts→pins→verify — phonemize FIRST so
pip never consults the dead upstream; both git repos hard-pinned to
SHAs because upstream publishes no tags; LinaCodec installed
explicitly because pip ignores [tool.uv.sources]); the luxtts SETUPS
row (landmark zipvoice, needs_git); backends/luxtts.py resolving its
interpreter at use time; the voice section gaining the install wiring
it never had; the consent dialog's third case (honest copy: LuxTTS's
opt-in reason is isolation + third-party fetch, not a license
restriction); HANDOFF-4090's §6 recipe corrected (it was wrong on
every OS — freeze the 4090's existing .venv-luxtts before rebuilding
from the .sh there). NOT yet proven: an actual Windows synthesis ear
test (install is one click away on this box; Noah runs it organically
with the other staged engines). Suites: engine 462 passed 1 skipped
(+5 guards incl. exact-SHA pins), cargo 73+6+1, clippy clean.

**2026-07-28 (close) — the selection redesign lands: one authority per
category, and disk is only ever spent on purpose.** Noah's design,
executed whole: the Models tab is inventory only (Use is gone from
every row; the chip reads IN USE with per-section hints naming the
real authority); the composer's dropdown IS voice selection
(SetActiveModel with the row's id — engine AND size travel, killing
the 0.6B-picked-1.7B-spoke bug by construction); STT picks live in
the Transcription view, LLM in a Settings LANGUAGE MODEL card; the ⇄
view already had per-conversion pickers and now reads real row state
(vevo2-singing's own row is finally consulted). Profile
default_engine is demoted to an app-side SEED — selecting a voice
pre-selects its pin, dropdown changes are session-only, the composer
NEVER writes the profile (the old dropdown silently rewrote
default_engine and, worse, offered kokoro to cloned voices, which
crashed generation — both dead). Cloned voices are offered
cloning-capable rows only; locked presets keep the read-only label;
list_voices gates CustomVoice presets on downloaded-not-active so
they survive Use's death. Requirement 7 productized: require_weights
refuses any generation whose weights aren't on disk with an
actionable sentence, the app pre-checks every dispatch and raises a
ModelNeededNotice whose "Open Models →" coachmarks the exact row
(scroll-into-view with a sanctioned no-scroll fallback); exemptions,
all accepted by Noah and documented in code: kokoro's warmup (price
of entry), TADA's ~2 MB tokenizer, vevo2's out-of-catalog
whisper-medium behind a one-shot persisted consent dialog. Known
residuals, deliberate: history ↻ TTS rows still don't pin their
engine (coherence follow-up); engine-side require_weights(size="")
names the catalog's first row where the app-side check prefers a
downloaded one — reachable only headless. Suites: engine 492 passed
1 skipped (+30), cargo 93+6+1 (+20), clippy clean throughout.

**2026-07-28 (smoke) — Noah's first two clicks of the redesign found
and killed two more bugs; one TODO carried to tomorrow.** Click one
(Download on the freshly-installed Seed-VC row) died at ~3-5% with
WinError 1314: huggingface_hub memoizes are_symlinks_supported per
REPO dir, writes the memo optimistically True before the trial
symlink corrects it, and a parallel file worker reading inside that
unlocked gap symlinks for real on a privilege-less box (1314 maps to
neither exception _create_symlink catches). seed-vc is maximally
exposed — four repos into fresh per-engine cache dirs. Fixed
(9f76669): the engine settles hub's own probe serially per repo,
under the fetch lock, before snapshot_download spawns workers — the
exact function and memo key the workers consult, self-answering so
Developer-Mode boxes keep native symlinks, unconditional because the
race lives in the memo, not in Windows. (HF_HUB_DISABLE_SYMLINKS does
not exist in hub 0.36.2 — verified against source.) The same click
exposed the LAST log-only failure path: ModelProgress "error" just
flashed to 0% and reverted. Now it banners — which model, where
engine.log is (per-OS hint), and that Download resumes; a narrow
set_models_error so a download failure can't clobber a live install's
marquee. Click two (the retry) failed HONESTLY: ChunkedEncodingError,
0 bytes × hub's 5 internal retries on a ~785 MB checkpoint — network/
CDN, correctly bannered, resumable. WINDOWS.md names both symlink
racers, dated.

**TODO — NEXT SESSION (Noah, 2026-07-28): download auto-retry.**
"The engine could absorb a burst of transient network failures itself
— auto-retry each repo two or three times with a short backoff before
showing the banner … and when it is happening I would like the text
above the marquee to reflect that." Shape: retry loop around the
per-repo fetch inside ModelManager.download (2-3 attempts, short
backoff, only on transient network errors — ChunkedEncodingError /
IncompleteRead / timeouts, never on 1314-class or disk errors), and a
retry indication in the row's progress label. NOTE the wire shape:
ModelProgress is (model_id, pct, status) with status
downloading|finalizing|done|error and unknown statuses degrading to a
plain bar in the app — a "retrying" status string would degrade
safely on an old app, but the label text above the marquee is
app-side (main.slint ModelRow downloading block), so the honest
implementation is a new status token + app mapping. Decide whether
that counts as a §6 vocabulary change needing a RPC-PROTOCOL prose
note (no count changes either way).

**2026-07-29 — macOS day 1: phase 1 lands whole on the M3 with ZERO
source changes.** First boot of the campaign's third platform (Apple
M3, 24 GB, macOS 26.5.2, arm64). Recipe: brew uv+sox, `uv venv
--python 3.12 --seed engine/.venv`, `uv pip install -e 'engine[qwen]'
'numba>=0.60'` — 147 packages, torch 2.13.0 with MPS available
(deliberately unused until the phase-2 device matrix; Hardware
honestly reports gpu:false on the M3). Engine suite 500 passed / ruff
clean / coverage 94.27% over the 94 gate; boot smoke proved §2 live:
rpc.json at ~/Library/Application Support/syrinx/ (mac merges
config+data dirs — no filename collisions), authenticated round-trip,
stdin-close teardown clean. The Rust app built FIRST TRY — the
Windows campaign's `unix`→`target_os = "linux"` cfg-narrowing paid
off in full; 88+7 tests, clippy clean. One real bug: the AUDIO
DEVICES card height counted only one of its two optional rows, and
mac is the first platform hiding exactly one (system tap unsupported,
mic test supported). Fixed: the height now sums the rows —
62+34+34 = the authored 130px on Linux/Windows, provably identical.
Gotchas earned: SYRINX_DATA_DIR does NOT relocate rpc.json (only
SYRINX_RPC_ENDPOINT does); brew sox prints an empty version string
(probe existence, not version); backgrounded shell jobs inherit
SIG_IGN for SIGINT — spawn from a clean parent when testing signal
behavior or chase phantoms; first mic capture may return zeros until
TCC consent lands (the prompt attributes to the terminal; an
unbundled binary has no NSMicrophoneUsageDescription — packaging
item); bare `cargo build` can never work on mac (dictate/ has ungated
gtk4 deps and is a default workspace member — always `-p syrinx-app
-p syrinx-shared`); Linux cross-check from mac: shared checks clean,
app is blocked by yeslogic-fontconfig-sys's pkg-config cross-compile
panic (vendored, not ours).

**2026-07-30 — macOS wave 2: Retina stays native, the dead DICTATION
card dies, and SIGTERM finally keeps §2.1's promise.** App side:
forcing SLINT_SCALE_FACTOR=1.0 was Windows medicine (OS *user*
scaling re-applied via ui_scale) that halved the mac window — Retina
2.0 is backing-store density, so winit read the authored points as
physical pixels. mac now sets no env var and runs at true
scale_factor=2.000; an explicit ui_scale in settings.json still
overrides on both, and the Windows path is byte-identical.
os_native_scale()'s mac arm is implemented via
NSScreen.backingScaleFactor — objc2/objc2-app-kit were already in the
graph via winit, so Cargo.toml only names them (zero new packages,
nothing recompiled). New `dictation-supported` property
(linux|windows) hides the whole ⚙ DICTATION card, heading included,
the way the ENGINE card hides off-Linux. Engine side:
`_install_sigterm_handler` routes SIGTERM onto asyncio's own
cancel-the-main-task path — the byte-identical teardown Ctrl-C
already gets — so `systemctl stop` and a plain `kill` now exit 0 with
rpc.json removed (was exit -15 and a stale discovery file). win32 is
structurally inert (early return before `signal` is even imported);
SIGINT is deliberately left to asyncio's own handler and a test pins
that. Live matrix, three reps each from a clean Python parent:
SIGTERM / SIGINT / stdin-close all exit 0, rpc.json removed. The sox
hint is per-OS now (darwin→`brew install sox`; Linux names the
package, not a guessed command). Coverage de-platformed: the win32
GlobalMemoryStatusEx block and the Linux XDG endpoint branch now run
on every OS (models.py 94→98%, total 94.27→94.67 over the 94 gate —
the gate no longer cares which OS runs the suite). Suites on the
merged tree: engine 516 passed (+16), ruff clean; cargo 88+6+1,
clippy clean. GUI smoke: app spawned the venv engine, ready in 0.6 s,
expected models-missing warmup banner, engine child gone within 6 s
of quit — twice. Found, NOT fixed: (1) pre-existing flake in
test_contract's rpc download/vc-setup exercises (~30-50% of runs,
reproduced on the untouched baseline) — `_Adapter.wait_for` returns
on the FIRST notification of a name while the assertions want two,
and the second frame is still in flight on the real WebSocket (D-Bus
never flakes: in-process synchronous emit); fix shape: a `count=`
parameter on wait_for. (2) The DICTATION card keeps a hard 92px while
its second row is Linux-only — dead space on Windows, the same class
as the AUDIO DEVICES fix; the one-liner (58px + is-linux ? 34px)
waits for a Windows session to verify. (3) Agent screenshots are
impossible until the terminal gets a Screen-Recording TCC grant — the
⚙ tab is verified structurally but still wants one human glance.

**2026-07-30 (later) — phase 2: the M3's GPU turns on. MPS lands in
the core engine and Qwen voice cloning runs on Metal — in bf16,
because fp16 provably cannot.** `detect_device()` gains the mps
branch (cuda/rocm still outrank it — an eGPU or a CUDA build under
Rosetta is the tuned path); the three copy-pasted
`("cuda","rocm")→"cuda" else "cpu"` mappings collapsed into one
shared `torch_device()` that passes "mps" through;
`empty_device_cache()` learned the Metal allocator. The LLM picks
cuda→mps→cpu with fp16 on both accelerators. `detect_hardware()`
reports Metal as the GPU: name from sysctl's brand string, vram_gb
from `torch.mps.recommended_max_memory()` — on unified memory the
Metal working-set ceiling (~74% of RAM) is the honest "can I fit this
model" number. Live on the M3: `{cores 8, ram 24.0, gpu true, "Apple
M3", vram 17.8}`. The dtype finding that earns the ledger: **Qwen-TTS
on MPS must be bf16.** fp16 loads and runs the talker, then dies in
`code_predictor.generate` → transformers `_sample` with "probability
tensor contains either inf, nan or element < 0" — logit overflow;
bf16's exponent range is what that stage needs, and Metal has the
kernels (M3, torch 2.13). The per-device dtype now lives in one
`_load_checkpoint()` shared by both qwen backends, failure named in
the docstring. No PYTORCH_ENABLE_MPS_FALLBACK needed anywhere — zero
missing-op failures. Live matrix, all through the engine's own RPC:
downloads of whisper-base / kokoro / qwen3-0.6b / qwen-tts-0.6B all
clean (first mac exercise of the HF symlink-settle path — quiet on
APFS); Kokoro speaks on MPS (3.25 s @ 24 kHz, warm 3.7 s; first call
+12.5 s while spaCy auto-fetched en_core_web_sm mid-synthesis);
Qwen-TTS 0.6B clone + speak on MPS bf16 (load 2.4 s, cold synth
11.6 s, warm 4.3 s for a 3.6 s clip); STT stays CPU int8 by design
(no Metal in CT2) and round-tripped the kokoro clip exactly in
0.46 s; LLM refine on MPS fp16 in 4.89 s, no NaNs. Suites: 543
passed (+27), coverage 94.70%, ruff clean; the new device tests run
on every OS via torch stand-ins, so none of it re-platforms the
gate. New gotcha: under SYRINX_SUPERVISED=1 a backgrounded shell
spawn hands the engine /dev/null for stdin — the watchdog reads EOF
as "parent gone" and exits instantly with an empty log; spawn from a
parent holding a real pipe. Deferred, on purpose: the venv workers
(luxtts/seedvc/vevo) get their mps branches in the wave that
installs them on mac; tada still picks fp32 on MPS (its dtype block
is cuda-gated — untestable until tada is installed here). Test
residue kept as evidence: a cloned voice "MPS Probe" + 4 history
clips in ~/Library/Application Support/syrinx/.

**2026-07-30 (evening) — Syrinx.app: the dev bundle lands and TCC
finally has a name to put on its prompts.** New
`scripts/install-macos-dev.sh` (house style, `--uninstall`,
idempotent): builds the release binary, assembles a real bundle —
Info.plist with `sh.syrinx.app`, NSMicrophoneUsageDescription,
icns rendered from packaging/syrinx.svg (qlmanage won;
rsvg-convert/sips are the fallbacks) — stages in a mktemp dir so a
failed run never leaves a half-app where LaunchServices can see it,
installs to /Applications (~/Applications fallback), ad-hoc signs IN
PLACE (stable cdhash = stable TCC identity; the mic grant survives
reinstalls — proven), registers with lsregister. The launcher bakes
the checkout path in and does exactly two things Finder wouldn't:
exports SYRINX_ENGINE_CMD (Finder launches with cwd=/, so the
cwd-relative probe can't fire and the ancestor probe walks out of
/Applications — the env var is the only probe that reaches the
checkout) and prepends /opt/homebrew/bin to PATH (LaunchServices'
environment has no brew prefix; qwen needs sox at import). Then
`exec` — the binary owns the pid, quit and SIGTERM land where they
act. Verified: `open -a Syrinx` → engine child from the venv, PATH
position 1 is brew, qwen stack pre-imported (the sox seam holds),
clean quit removes rpc.json, reinstall+relaunch with no re-prompt,
Spotlight finds it. THE FIELD FINDING: first launch hung ~7 min —
the engine's Python blocked inside open() reading pyvenv.cfg, because
the checkout lives under ~/Documents and the very first file read
trips the kTCCServiceSystemPolicyDocumentsFolder gate, which BLOCKS
(does not fail) until the dialog is answered. Correctly attributed to
sh.syrinx.app — the exact day-1 gap this bundle closes. One time
only; now named in the installer summary. This is the dev seed of
§2.2 packaging, not the relocatable bundle (that phase revisits
--deep signing too, deprecated in favor of per-binary).

**2026-07-30 (night) — Noah's first user-path click found the wave-3
bug: `python3.12: command not found`, and resolve_python stops being
Windows-only. Plus: LuxTTS is PROVEN on mac — the icefall index has
the arm64 wheel.** The Models-tab Install on LuxTTS died at
setup-luxtts.sh:29: vcsetup spawned the script with the interpreter
env seam set only under `_IS_WIN`, on the theory "POSIX boxes that
can run Syrinx already have python3" — true, but the scripts ask by
the bare NAME python3.12, which a uv-managed mac never puts on PATH,
least of all the four-entry PATH a Finder-launched app hands the
engine. All three venv engines were equally broken; the mechanism got
fixed, not the symptom: resolve_python now runs on every platform.
POSIX candidates, lazily: python3.12 on PATH (Linux lands exactly
where it always did — no-op by construction) → OUR OWN base
interpreter via sys._base_executable (the self-answer: any box
running the engine has a CPython 3.12 — the one that built
engine/.venv; this is what wins on Noah's mac) → `uv python find
3.12`. The winget bootstrap stays win32-only; the no-Python sentence
went per-OS (the qwen sox-hint shape — darwin names brew/uv, not
python.org). env[py_env] is now set unconditionally at spawn; the
.sh files' Linux execution path is byte-identical. Verified live
through the real VcSetupManager.install("luxtts") under env -i with
the app's exact PATH: 76 s to a passing verify stage. The ledgered
open question is CLOSED: k2-fsa's icefall find-links index carries
piper_phonemize-1.4.7-cp312-cp312-macosx_11_0_arm64.whl (9.6 MB) —
no piper-phonemize-fix fallback needed, no script logic change,
and the torch cpu-index branch serves the standard arm64 build with
working MPS. luxtts_worker._device() gained the mps rung (cuda > mps
> cpu, llm.py's AttributeError guard): synthesis on mps, no
missing-op fallbacks, warm 2.1 s vs cpu 3.4 s. Suites: 567 passed
(+24, new test_luxtts_worker.py drives the worker as a subprocess —
importing it in-process would dup2 pytest's stdout), coverage
95.08%, ruff clean. The verified .venv-luxtts was then DELETED on
purpose: Noah gets to click Install like a user, against a warm pip
cache (~1 min) and already-cached HF weights. His running engine
predates the fix — quit + relaunch first. Known quirk for that
session: LuxTTS sampling is non-deterministic (same sentence: 3.45 /
3.11 / 2.69 s of audio), so the worker's duration-ladder retry fires
unevenly — cosmetic, ledgered, unfixed.

**2026-07-30 (late) — phase-3 opens: macOS system capture, the
loopback way. Zero new native code — a loopback driver is just
another CoreAudio input, so the engine's §14 recorder was already
the whole implementation.** system_capture_supported gains macos:
the ◉ Record-system buttons, the create-voice System chip and the ⚙
tap picker come alive, and the AUDIO DEVICES card returns to its
authored 130px via the wave-2 row-sum. The tap enumeration is the
loopback-named subset of the engine's own ListRecordingDevices reply
(one RPC, no second enumeration): case-insensitive family match over
BlackHole / Loopback / Soundflower / VB-Cable / Existential Audio,
matching either half of the (id, description) pair; loopback devices
stay in the mic list too (a legit mic choice — unlike Linux, which
moves monitors out). resolve_capture_device's mac arm re-enumerates
per request — a stored tap the user has since uninstalled must NOT
fall through to the default input, which would record the microphone
under a "system" label; engine-unreachable deliberately returns
ok=true so capture_start reports the real failure instead of blaming
BlackHole. Absence UX (the plan's "detect gracefully"): per-OS
NO_SYSTEM_TAP ("no loopback device — run: brew install
blackhole-2ch" on mac; Linux keeps its verbatim string), rendered in
tr/vc status AND the create-voice arm — which, surprise, was
ignoring the no-tap result on EVERY platform and silently recording
the mic; fixed for mac only, and the Linux fall-through is hereby
flagged as a latent bug for a Linux session to decide. New ⚙ hint
row (mac-only, 40px, hides by "" so Linux/Windows card heights are
bit-identical): with no driver, the install command + a ⧉ copying
it; with one, the routing caveat ("a loopback tap hears only what is
routed to it — a Multi-Output Device (Audio MIDI Setup) lets you
hear it too"). The silent-capture ⚠ at record time already covers
the forgot-to-route case on every platform. Suites: cargo 96+6+1
(+8; the mac helpers are cfg(macos|test) so the tests run on all
CI), clippy --all-targets clean. Verified live against the running
engine, read-only: ListRecordingDevices returns only the iMac mic on
a driverless box → enumeration [], absence path exact. The live
440 Hz loop RAN 2026-07-30 post-reboot (`brew install blackhole-2ch`
+ restart; system_profiler lists "BlackHole 2ch", Existential Audio,
2 in / 2 out): 2 s of 440 Hz at 0.5 amplitude played into the
BlackHole output while RecordingManager captured its input
in-process (SYRINX_DATA_DIR redirected, no service, no rpc.json) —
PASS, twice, byte-identical: rms 0.2776, FFT peak 440.00 Hz,
48 kHz, sample peak 0.49997 = unity gain; rms matches
0.5/√2·√(2.0 s tone / 3.24 s window) to 4 decimals, so everything
outside the tone is true digital silence — BlackHole is a
bit-transparent path, and `_resolve_input` mapped the name to the
right index on the first try. One finding worth keeping: playing
and recording the SAME CoreAudio device from ONE process fails
deterministically (PaMacCore AUHAL err -10863 "cannot do in current
context") — the play silently no-ops AND collaterally stalls the
open capture stream at zeros; a subprocess player works, and
cross-process is the real shape of system capture anyway. If the
app ever adds same-process monitoring/preview on the tap device it
hits this — remember it when CATap lands. Dev .app reinstalled at
HEAD the same evening (the installed bundle predated the capture
UI). Native CATap (AudioHardwareCreateProcessTap,
macOS 14.4+) is the zero-setup upgrade — queued for the packaging
phase, where its NSAudioCaptureUsageDescription belongs anyway.

**2026-08-03 — the BlackHole requirement dies: macOS system capture
goes NATIVE via a Core Audio process tap, app-side, the third twin of
`parecord <sink>.monitor` and WASAPI loopback.** New
`app/src/capture_mac.rs` (cfg(macos)) is capture_win's architectural
copy — `SystemTap::start(wav) -> handle` with a setup handshake off a
dedicated thread, `stop() -> path`, `discard()`, `died()`, the same
patch-on-finalize mono-PCM16 WavWriter — so the seam gained one enum
variant (`Capture::Tap`) and nothing else. Crates: **objc2-core-audio
0.3.2** carries BOTH halves (the `CATapDescription` Obj-C class *and*
the `AudioHardware*`/`AudioDevice*` C entry points), which is why
there is no coreaudio-sys bindgen and no hand-rolled msg_send here;
+objc2-core-audio-types (AudioBufferList/ASBD), +objc2-core-foundation
(the CFDictionary type), +objc2-foundation (NSDictionary/NSArray/
NSUUID) — all the objc2 0.6 / 0.3 family already in the lock file, so
the graph gained 2 crates, not a family. `default-features = false`
drops block2/dispatch2/libc/AudioServerPlugIn; feature `AudioHardware`
is the only one that matters, and `AudioHardwareBase` does NOT exist
(cargo tells you). Shape: `initStereoGlobalTapButExcludeProcesses`
with an EMPTY NSArray = the whole system mix; pin
`desc.setUUID(NSUUID::new())` BEFORE `AudioHardwareCreateProcessTap`,
because the aggregate references the tap by that UUID string.
Aggregate dictionary (NSDictionary cast toll-free to CFDictionary):
Name / UID(uuid) / IsPrivate=true / IsStacked=false / TapAutoStart=true
/ TapList=[{SubTapUID: tap uuid, SubTapDriftCompensation: true}] — and
**no sub-device list at all**. AudioCap's reference names the current
output as MainSubDevice, which for a global tap only drags the user's
real (Bluetooth) endpoint into an aggregate for nothing; the tap is
its own clock and the tap-only dictionary runs fine. IOProc downmixes
the tap's float32 (interleaved or planar, both handled) to mono i16
into a Mutex<Vec<i16>> the writer thread drains every 20 ms — `try_lock`,
never `lock`, on the realtime side; the WAV carries the tap's own rate
(48 kHz here), nothing resamples. Availability is a runtime
`AnyClass::get(c"CATapDescription")`, not a version parse; false →
capture_start refuses with NO_SYSTEM_TAP rather than falling through
to the engine's mic path (the exact bug class ledgered for Linux).

**TCC is the whole story on this one.** Key is
`NSAudioCaptureUsageDescription` (kTCCServiceAudioCapture — its own
category, NOT the mic grant), now emitted by
scripts/install-macos-dev.sh next to NSMicrophoneUsageDescription.
Two behaviours that cost the session an hour and will bite packaging:
(1) tccd attributes the request to the RESPONSIBLE process, and when
that process has no such key it logs `Refusing authorization request
for service kTCCServiceAudioCapture … without NSAudioCaptureUsageDescription
key` and returns denied **with no prompt and no error back to Core
Audio** — the tap runs and delivers pure zeros. A bare `cargo run` or
`cargo run --example` from a terminal therefore CANNOT record system
audio, ever, on any macOS: the terminal is the responsible process.
The live proof only worked once the probe binary was dropped into a
minimal ad-hoc-signed .app carrying the key and launched through
LaunchServices (`open -W …`). (2) When the key IS present,
`AudioDeviceStart` **blocks inside the call** until the user answers
the prompt — minutes, potentially — so capture_start's mac arm wraps
`SystemTap::start` in `spawn_blocking`; on the tokio worker it would
wedge every other in-flight command. Denial after that is still
silent, so nothing here can detect it: the silence-after-capture
warning is the only user-visible safety net (separate agent).
Packaging implication: the grant keys to the bundle's cdhash, so
`--force --deep -s -` re-signing on every dev install can re-prompt,
and a per-binary signature story (helper tools, an embedded engine)
has to keep the tap inside the SAME identity that owns the grant.

⚙ matrix, now four rows deep (`tap_dropdown_names(taps, native)` /
`tap_hint(taps, native, selected)`): mac+native → row 0 "System audio
(native tap)", drivers listed after it as overrides, **no hint row**
(the routing caveat does not apply — that is the entire point), card
shrinks via the existing `st-tap-hint != ""` sum; mac+native+user
picked a driver → routing caveat returns, and StPickMonitor re-pushes
the hint so the card resizes on the click; mac, no native, drivers →
routing caveat; mac, no native, none → the brew line + ⧉. Linux/
Windows unchanged in every combination (asserted over the whole
cross-product). resolve_capture_device's mac arm returns
`(None, true)` for the native default; a non-empty monitor_device
keeps the 2026-07-30 re-enumerate-per-request loopback behaviour
verbatim.

Gates: clippy `-p syrinx-app -p syrinx-shared --all-targets` clean,
`cargo test -p syrinx-app` 99 passed (+3 net), `-p syrinx-shared` 1.
Snapshots via the offscreen harness (new `native` variant, and the
variants now set the dropdown rows too, not just the hint):
settings-after-native.png shows the shrunk card, settings-before-hint.png
the override state. **LIVE PROOF, 2026-08-03, PASS** — 2 s of 440 Hz
at 0.5 amplitude looped through `afplay` to the CURRENT default output
(iMac Speakers, untouched) while the bundled probe captured 6 s:
48 kHz, 287232 samples, sample peak 0.500000 exactly, whole-file rms
0.293716, loudest-1 s-window **rms 0.353528 vs the theoretical
0.5/√2 = 0.353553**, FFT peak **440.000 Hz**, and 100.00 % of spectral
energy inside 430–450 Hz. Bit-transparent, same as the BlackHole
proof, with nothing installed and nothing routed. The AUHAL -10863
gotcha from 2026-07-30 does NOT apply: playback stayed audible and
glitch-free throughout, and both players here are separate processes
(afplay in the proof; in the app, audio.py's sd.OutputStream lives in
the engine child while the tap lives in the app) — same-process
play+record on one device remains the thing not to build.

Residual risks: (a) macOS < 14.2 is UNTESTED — the fallback is exercised
only by construction (class lookup false → BlackHole path), and Rust
emits a strong reference to AudioHardwareCreateProcessTap, so whether
dyld weak-links it below the availability floor rests on ld64's
tbd annotations rather than on anything we assert; a 13.x box would
settle it. (b) A default-output change mid-capture is not re-armed
(the tap is global, so it should not matter, but it is unproven).
(c) The dropped-samples path (writer stall / try_lock miss) only logs.

**2026-08-03 (later) — the silence guard: a system capture that came
back empty now SAYS SO, with the cause the tap in use implies.** The
incident: two 30 s create-voice "System" takes on the M3 produced
valid, perfectly silent WAVs (rms 0.000000) and the flow accepted
them; the only warning in the app, `recorded_label`'s "⚠ silent —
check input / output device", names neither the tap nor the fix, and
the ⇄/☰ arms of the same check (`wav_rms < 0.006`) DISCARDED the take
instead of warning about it. Guard is app-side, at the moment
`capture_stop` returns a path, and system-audio ONLY — mic level is
already live in the ⚙ meter and a quiet room is legitimate.

Classifier (`WavScan` + `is_silent`, app/src/main.rs): PEAK, not rms —
`SILENT_PEAK = 1e-4` full scale is ±3 LSB of PCM16, i.e. dither and
nothing else, so a genuinely quiet take can never trip it (the old
0.006 rms floor flags real audio 60× above digital silence and is why
the ⇄/☰ arms threw takes away). `SILENCE_MIN_SECS = 0.5` exempts
twitch-stops. `wav_scan` walks the chunk table (fmt/data, word-aligned
skips over LIST & friends, `data` size 0 = read to EOF for a recorder
killed mid-write) and streams the PCM through a fixed 64 kB window —
no slurp; measured 0.01 s on a 57.6 MB 10-minute 48 kHz mono capture,
and it runs on `spawn_blocking` anyway because the worker loop it is
awaited from drives every timer and button in the app. PCM16-only by
design: every writer in play (capture_mac/capture_win WavWriter,
parecord --format=s16le, the engine's `wave` + sd.InputStream
dtype=int16) produces it, and a guess at any other layout would be a
lie rather than a warning; non-PCM16 → `None` → no warning.

Cause is carried, not inferred at stop time: `TapKind::{Native,
Loopback,Sink}` is recorded from resolve_capture_device's device at
capture_start (mac None = native tap, Some = loopback driver;
everywhere else Sink) and held beside the Capture, because the safety-
cap auto-stops have no `system` argument to read and the other source
button stays clickable mid-take. `silence_hint` mirrors the ⚙ tap_hint
matrix one sentence deep — native: "Nothing was playing — the native
tap hears whatever this Mac is outputting, so start the audio (or
unmute it) and record again."; loopback: "A loopback tap hears only
what is routed to it — send the audio there with a Multi-Output Device
(Audio MIDI Setup)."; sink: "Nothing was playing on the tapped output
while the capture ran."

WARN, never discard, at all six stop sites (✦ create-voice stop + its
30 s cap, ☰ toggle-stop + its 10 min cap, ⇄ toggle-stop + its 3 min
cap): the clip is kept, armed and reachable by ✂ Trim; only the
pointless transcription of digital silence is skipped. Tight slots get
`SILENT_CAPTURE` ("⚠ silent capture — kept anyway"), the sentence goes
to each flow's own notice line — cv-error and vc-error already existed
(composer error-line pattern, elide + ⧉); ☰ had none, so `tr-notice`
is the one new property + row, styled 10.5px mono dim to match. The
mic arms of the ⇄/☰ rms check are untouched and now explicitly gated
`tap.is_none()`, so system captures take the new path exclusively.

Gates: clippy `-p syrinx-app -p syrinx-shared --all-targets` clean,
`cargo test -p syrinx-app` 103 passed (+4), `-p syrinx-shared` 1.
Snapshot harness gained `tr-silent` / `vc-silent` / `cv-silent`
variants (main() split so every variant shares one `render`): the
create-voice card holds the warning inside its fixed 92px without the
wrap+preferred-width:0 blowout, and at 900px the ☰ notice row still
reads in full while tr-status elides to "⚠ …" — which is the whole
reason it is a separate row. **Verified against the incident files**
(~/Library/Application Support/syrinx/recordings): all three scan
peak 0.0 @ 48 kHz — 1437696 / 155648 / 1437696 frames = 29.95 s /
3.24 s / 29.95 s — and classify SILENT; real audio in the same
run (a profile sample, a history clip, a vevo reference) scans
peak 0.32 / 0.37 / 1.00 and classifies NOT silent.

Residual: the guard cannot distinguish "nothing was playing" from
"the TCC AudioCapture grant was denied", because a denied tap
delivers zeros with no error (2026-08-03 entry above) — the native
hint's wording is deliberately about the audio, not the permission,
and the ⚙ card remains the place a permission story would go.

**2026-08-03 (engine) — two PortAudio truths the engine had never
accounted for: its device list is a SNAPSHOT, and PaMacCore throws the
tail away.** Both fixed in `engine/syrinx_engine/audio.py` (+
`recording.py`), no Rust, no RPC change.

(1) *Stale topology.* The incident: BlackHole 2ch was uninstalled (and
coreaudiod restarted) under a running engine, and from that moment
EVERY playback attempt logged `could not open output stream (Error
opening OutputStream: Internal PortAudio error [PaErrorCode -9986]);
skipping playback` (+ `||PaMacCore (AUHAL)|| Error on line 1332:
err='-10851'`) until the engine was restarted. PortAudio snapshots the
topology at `Pa_Initialize`, so any device appearing or disappearing
afterwards — driver install/uninstall, USB unplug, Bluetooth earbuds
dropping, i.e. an ordinary end-user event — leaves the cached list and
default-device references lying, permanently. Fix is on-demand, not a
listener: `with_fresh_portaudio_retry(sd, open_fn)` runs the open,
and on a stale-topology signature bounces PortAudio
(`sd._terminate()` + `sd._initialize()`, the documented refresh) and
retries EXACTLY once; a second failure re-raises the FIRST exception
unchanged, so every existing surface (the "skipping playback" warning,
`StartRecording` → "", `ListRecordingDevices` → "[]") reads the same.
Match set is conservative and code-based, not string-based
(`PortAudioError.args` is `(message, PaErrorCode[, host info])`):
**-9986** paInternalError (what PaMacCore reports here), **-9985**
paDeviceUnavailable, **-9996** paInvalidDevice. Format/parameter codes
are excluded on purpose — -9997 paInvalidSampleRate fails identically
after a bounce, and retrying it would dress a parameter bug up as a
device story. Hooked in at four call sites: `audio._play_blocking`'s
`sd.OutputStream`, `RecordingManager.start`'s `sd.InputStream`,
`recording.list_devices`' `sd.query_devices` (the §14 enumeration), and
— by construction — nothing else, because those are the only places the
engine touches PortAudio (the ⚙ mic meter is the same
StartRecording/RecordingLevel path, and Linux's parecord arm never
reaches Python).

**Bounce-only-when-idle** is the load-bearing rule: `_terminate()`
yanks every live stream out from under its owner, so the audio module
keeps an `_active_streams` count (`stream_opened()` right after a
successful open; `stream_closed()` in `_play_blocking`'s finally and in
`_Recording.finalize`, which is already idempotent). Non-zero ⇒ no
bounce, log at warning, let the original error propagate — a stale open
next to a working stream is rare and self-resolves on the next attempt,
whereas killing a live recording to fix a playback open is not a trade
anyone would take.

**Index-invalidation audit** (indices do NOT survive a bounce, names
do): the only saved index in the engine was `RecordingManager.start`'s
`_resolve_input()` result, computed before the open — so `start` was
restructured to resolve device AND sample rate INSIDE the retried
callable, and the WAV is now opened after the stream (it takes the rate
the stream actually got, and `rec` exists before `stream.start()`,
which is when PortAudio begins calling back). Everything else came out
clean: settings persist the device **name**, never an index
(RPC-PROTOCOL §14's `id` is the name, deliberately); `list_devices`
reads `sd.default.device[0]` only for the `default` flag and the engine
never ASSIGNS `sd.default.*`, so a bounce loses no configuration; the
Rust side stores names too. Not fixed, and deliberately out of scope: a
`query_devices` that SUCCEEDS while returning a ghost device (BlackHole
after uninstall) is still stale — the retry only fires on an error. An
unconditional refresh per enumeration is one line but would kill any
live recording, so it stays parked.

(2) *The cut-off last word.* Users reported "the last half of the last
word is always cut off", worse on macOS. `_play_blocking` wrote its
final block and went straight to `stream.stop()`; PaMacCore's blocking
I/O (blio) layer DISCARDS its ring buffer on `Pa_StopStream` instead of
playing it out (long-standing PortAudio macOS behaviour; Windows/Linux
hosts drain). Engine-dependence was a red herring: Kokoro clips carry
300-670 ms of trailing padding that absorbs the cut, while Qwen/LuxTTS
trim to 2 ms / 46 ms past the last speech energy and lose real words.
Fix is `audio._drain()`, called only when the block loop ran to its
natural end (a user stop must NOT be padded): silence sized
`ceil(stream.latency * rate) + one _BLOCK` of margin, pushed through
the same blocking `stream.write` — a blocking write returns when there
is ROOM, so filling the buffer with silence IS the proof that every
real sample was consumed, and it costs no wall time beyond the audio
that was genuinely still playing. `ctl.stop` is checked between drain
chunks so cancel/seek responsiveness is untouched (one block ≈ 43 ms).
**The cap is 2 s, not 1** (`_MAX_DRAIN_SECS`), and that is a
measurement, not a preference: sounddevice requests PortAudio's "high"
latency, and the M3's default output — Bluetooth (TOZO NC9) — reports
`stream.latency = 1.082375 s`, so a 1 s cap would clip the very tail
this exists to save. The cap never affects cancel latency (that is the
per-chunk `ctl.stop` check); it only bounds a host reporting nonsense.
Live smoke on the real default device: a 0.3 s tone took 1.548 s wall
without the drain and 2.383 s with it — i.e. the old code returned
~0.8 s before the audio it had queued could possibly have been heard,
which on a 1.08 s-latency sink is the whole clip.

Gates: `ruff check engine` clean; `pytest engine/tests --cov` 587
passed, 95.56 % (gate 94). New tests live in the existing mocked-
sounddevice idiom — `conftest.pa_error()` builds a PortAudioError with
sounddevice's real `args` shape, `fake_sd` gained
`_terminate`/`_initialize` (recording into `.bounces`, with an
`on_bounce` hook that reshuffles `.devs` the way a driver install
does), `FakeStream` gained `latency` + `voiced_frames`/`tail_zeros` so
every pre-existing frame-count assertion now reads the clip and ignores
the pad. Covered: retry-once-then-success (parametrized over all three
codes), retry-then-fail raises the FIRST error, no retry on -9997 or a
plain exception, no bounce while a stream is live, a bounce that itself
throws is not fatal, the recording retry re-resolving a MOVED device
index, drain on natural end / none on user stop / aborted mid-drain by
cancel / bounded at 2 s. Standalone PortAudio smoke (not against the
running engine): `_terminate()` + `_initialize()` cycles cleanly and
repeatably, `_initialized` back to 1, the 4-device list identical
across the bounce, and both an OutputStream and an InputStream open
fine afterwards.

**2026-08-03 (UI) — the scrollbar gutter: std-widgets' ScrollView paints
its bar OVER the viewport, so every list whose rows stretch the full
width had the bar sitting on the last column** (Noah saw it on the TTS
HISTORY cards and the Audio Library rows; it is platform-independent —
the app compiles the default `fluent` widget style, whose vertical
ScrollBar is 14px wide over a full-width Flickable). Mechanism:
`Theme.sb-gutter: 14px` (theme.slint, skin-independent — it tracks the
widget style, not the palette), applied as `padding-right` on the layout
INSIDE each affected ScrollView. Affected + fixed (11): TTS voices grid
(wide + narrow), TTS history, ☰ captures rail, ⇄ source-clip rail, ⇄
conversions rail, ▤ library rows, ◍ profile table (its column-header row
takes `12px + Theme.sb-gutter` so headers stay over the row columns), ◍
inspector samples, ✧ fx presets, ✧ fx chain. Already clean, untouched: ⚙
settings and ◈ models (their inner layouts already pad 22px > 14px).
Deliberately NOT changed: the 9 TextInput ScrollViews (composer,
history/capture message boxes, transcripts, personality, create-voice)
— their `viewport-width: self.visible-width` is the wrap width, so a
gutter there changes text wrapping and scroll extent, which this fix is
not allowed to touch; and the ThemedDropdown popup list (scrolls past 8
options, but a permanent 14px inset would visibly unbalance every
non-scrolling popup, and option text elides). Verified with
`app/examples/settings_snapshot.rs`, which grows three list variants
(`tts-hist`, `lib-list`, `vp-list`) that seed 8 dummy rows each — at
1200x700 the content overflows and the bar is drawn, which is the only
way the overlap shows. Before/after PNGs: rows now stop 14px short and
the bar has clear track; the ◍ header row still lines up with the rows'
columns; ⚙ re-rendered unchanged. Gates: clippy `-p syrinx-app -p
syrinx-shared --all-targets` clean, 103 + 7 tests green, dev bundle
reinstalled.

**2026-08-03 (feature) — ☰ captures can be named.** Rows in the
CAPTURES rail were identified by their created-at timestamp alone, so a
rail of six looked like a rail of six timestamps. Names live where every
other Syrinx label lives: the SQLite store — `captures.name TEXT DEFAULT
''` in `syrinx.db`, added by the same PRAGMA-table_info + ALTER migration
`HistoryStore` (tags/vc_json) and `SourceClipStore` (transcript/kind) use,
so pre-existing rows open unnamed instead of erroring. Nothing on disk is
keyed by the name (captures have no audio at all), so this is pure
presentation metadata and no file is ever renamed. New RPC, §4.9:
`RenameCapture [capture_id, name] → null`, trimming and blank-clearing
server-side (`CaptureStore.rename`); §0/§11 counts re-baselined 72 → **73**
methods (signals/properties unchanged), `test_contract.py`'s arity sweep
and count guard updated. The wire adds `name` to each `ListCaptures` row
next to the existing `date`; the app derives the row label with
`capture_label()` (name if non-blank, else the timestamp) — that fallback
is why clearing the field is a revert-to-default, and why a new recording
needs no default written to it. UI follows the two strongest precedents:
the affordance is a **⋮ row menu** (`CaptureRowMenu`, the history card's
`RowMenu` narrowed to Load into editor / Rename) replacing the ✎ button,
which was a duplicate of the row's own click — 🗑 stays a visible sibling;
the editor is a **centered modal** shaped exactly like the ▤ library tag
editor (`tr-rename-id != ""`, `LabeledInput`, Cancel / Save name).
`LabeledInput` gains `autofocus` + `accepted`/`escaped` so Enter commits
and Escape cancels — the app's first key handling anywhere (Slint
`TextInput.accepted` / `key-pressed` → `EventResult`), kept inside the one
component so every future dialog field inherits it. The label is used
everywhere the capture is referenced: rail header (accent-coloured when
named, dim when it is a timestamp) and the delete confirm, whose copy
moves to the quoted form the other four delete kinds already use. Gates:
clippy `-p syrinx-app -p syrinx-shared --all-targets` clean, 105 + 7 cargo
tests green, 590 pytest @ 95.58 % (the two `test_contract` download /
vc-setup flakes passed on the re-run), snapshot variants `tr-rename` /
`tr-rename-editing` added to `settings_snapshot.rs` (they also set
`narrow` from the window width, so the 165 px rail is checked too), dev
bundle reinstalled.

**2026-08-03 (feature) — every import point takes video files.** Source
audio lives inside video more often than not (a lecture mp4, a music
video, a phone take), and the workflow was: strip the audio in Ableton
first, then import. The decoder was already installed — **PyAV 18.0** is
a faster-whisper dependency, i.e. FFmpeg's demuxers/decoders sitting in
`engine/.venv` unused — so this is a new module, not a new dependency
(`av>=11` is now *named* in `engine/pyproject.toml` + the CI test-dep
line, alongside librosa, for the same reason librosa is spelled out
there: skipping the tests would drop the new code below the gate).
**One chokepoint:** `syrinx_engine/media.py` — `resolve(path)` returns
audio paths unchanged and hands back an extracted WAV for a video
container, `await asyncio.to_thread`'d because decode is fast but not
free. Detection is an **extension allowlist**
(`.mp4/.mov/.mkv/.webm/.avi/.m4v`), never sniffing: a file that lies
about being a WAV keeps failing at the decoder that reads it, which is
predictable, and mislabeled files are rare. `.webm` moves from the app's
*Audio* filter to *Video* — it IS a container, and routing audio-only
webm through extraction is a straight upgrade because soundfile cannot
read one at all. Extractions land in
`$SYRINX_DATA_DIR/video_audio/<stem>-<sha1-16>.wav` (the `vc_pitch` /
`recordings` layout), keyed on the source's **(resolved path, mtime,
size)** — so the one video the app hands to TranscribeFile, then
FileEnvelope, then PlayFile, then ConvertVoice, then SaveSourceClip
decodes exactly **once**; a re-encoded source at the same path misses
and re-extracts. Lifecycle is cache, not store: nothing references them
by id, `_prune` evicts newest-first past 2 GiB (a feature-length film's
PCM16 audio clears a gigabyte) and sweeps `.part` files older than an
hour, and each decode writes a uuid-suffixed `.part` unlinked in a
`finally`, so a kill mid-decode leaves no truncated WAV posing as a
cache hit. Output preserves the **source rate and channel count** (only
the sample format is resampled, planar float → packed s16) and is
**PCM16**, the subtype every other import path already produces
(`TrimAudio`, `_pitch_shift_wav`, the recorders). **No new RPC** — §0/§11
counts unchanged at 73/11/3; the ten path-taking methods
(`Transcribe`, `TranscribeFile`, `ConvertVoice`, `SuggestPitchShift`,
`CloneVoice`, `AddSample`, `SaveSourceClip`, `PlayFile`/`PlayFileAt`,
`FileEnvelope`, `TrimAudio`) just became container-tolerant, and each
failure rides that method's **existing** surface (raise / `error=true` /
`""` / `0` / `"{}"`) with one sentence: `no audio track in <name>`.
RPC-PROTOCOL.md gains a normative paragraph at the head of §4 plus
amended `SaveSourceClip` / `TrimAudio` rows. Three call-site notes: (1)
`_start_convert` resolves into a local `source` and leaves `audio_path`
the **original**, so the stored `vc_json` recipe pins the video the user
picked and Regenerate re-extracts (mtime/size freshness still measured
against a file the user can see); (2) `TrimAudio` siblings a
`-trimmed.wav` for a video source instead of its usual in-place WAV
rewrite — the extraction is shared cache, and rewriting it would hand
the trimmed take to every later import of that video; (3)
`PlayFile`/`PlayFileAt`/`FileEnvelope` go through `EngineCore._source`,
which logs a failed extraction and returns the original so their
already-documented unreadable-path outcome reports it once, rather than
turning a tolerant method into a raising one. App: the three pickers
(`CvPickFile`, `TrPickFile`, `VcPickFile`) collapse onto one
`import_dialog()` whose **default (first) filter is "Audio or video"**,
with "Audio" and "Video" following for narrowing — no other UI change,
no `.slint` touched, no snapshots. **Nothing reads imported audio
app-side** (verified): `wav_rms` and `silence_warning`/`wav_scan` are
reached only from the record-stop arms, and envelope/duration are
engine-side already. **AAC is lossy — the numbers:** round-tripping a
440 Hz tone through AAC-in-mp4 runs the clip **long, never short**
(encoder priming + tail padding, ~10 ms on a 12 s take, ~20 ms on 2 s),
so duration asserts are one-sided slack; FFT peak lands within 0.03 Hz
of 440 and rms within 0.15 % of ideal, which is why the tests assert on
spectrum and rms rather than samples. Test media is **encoded by PyAV in
the tests** (`make_video` in conftest — aac/libopus audio or a video-only
mpeg4 stream with no audio at all); no binary fixtures in the repo.
Live smoke on the M3, standalone against the real function: 12 s 48 kHz
stereo AAC + mpeg4 video → 48 kHz/2ch PCM16, 12.0107 s, peak 440.03 Hz,
rms 0.3531 vs 0.3536 ideal, decode **0.02 s (702× realtime)**, second
call a 0.1 ms cache hit. Gates: ruff clean, 633 pytest @ **95.65 %**
(the known `test_contract` download-flow flake passed on the re-run;
`media.py` itself 98 %), clippy `-p syrinx-app -p syrinx-shared
--all-targets` clean, 106 + 7 cargo tests green, dev bundle reinstalled.

**2026-08-03 (phase 3 closes) — macOS dictation lands: ⌃⌥D anywhere in
the OS, Carbon hotkey in, CGEvent unicode out.** `app/src/dictation_mac.rs`
(~490 lines) is the architectural twin of `dictation_win.rs` — same
second-RPC-client shape (§1: its OWN `EngineClient`, never the UI worker's),
same toggle (`StartRecording` → `StopRecording` → `Transcribe` → optional
`RefineTranscript` by req_id with the same 180 s timeout and raw-transcript
fallback → inject), same "every failure logs and returns to idle", same
no-pill decision, same `refine_dictation` key re-read per toggle. **Zero
engine changes and zero protocol changes** — nothing server-side was ever
OS-gated (`RefineTranscript`/§14 recording are plain methods); the §0/§11
method, signal and property counts are untouched. RPC-PROTOCOL §1's
"Connections" bullet now names the dictation
client per OS in place of the stale "in phase 3 the dictate binary".

**APIs, and why.** Hotkey: Carbon `RegisterEventHotKey` +
`InstallEventHandler(GetApplicationEventTarget())`. It is present and
*undeprecated* in the 26.5 SDK (`HIToolbox/CarbonEvents.h`, no deprecation
attribute), and it is the one global-hotkey API that needs **no TCC grant at
all** — a `CGEventTap` would demand Input Monitoring merely to listen, which
was the disqualifier for `livesplit-hotkey`. Injection:
`CGEventCreateKeyboardEvent(NULL, 0, down/up)` +
`CGEventKeyboardSetUnicodeString` + `CGEventPost(kCGHIDEventTap)` — virtual
key 0 and a unicode payload means **no keycode mapping**, so the transcript
lands identically under any keyboard layout. **Crate cost: zero new
packages.** `objc2-core-graphics` 0.3.2 was already in the lock file via
objc2-app-kit ← slint, so Cargo.toml only *names* it (features `CGEvent`,
`CGEventTypes`, and the non-obvious `CGRemoteOperation` — `CGKeyCode` lives
there and the keyboard-event constructor is cfg'd on it); the diff to
Cargo.lock is a single edge. Carbon and the three AX symbols stay raw FFI:
`objc2-carbon` is an explicitly empty stub, nothing in the objc2 family
covers HIToolbox events, and `objc2-application-services` would be +1 crate
for `AXIsProcessTrusted`. Traps: `ItemCount`/`ByteCount` are **u64**
(declaring them u32 is the classic way to corrupt this call); linking
HIToolbox directly is refused by ld ("not an allowed client") — the umbrella
`Carbon` framework is the only link; `Boolean` is a byte, not a Rust `bool`.
`global-hotkey` 0.8 was the fallback (Carbon-backed, correct) at ~7 effective
new crates; `enigo` would have dragged in the parallel servo core-graphics
ecosystem.

**Threading — the one place the twin diverges.** Windows spawns its own
thread because `RegisterHotKey` delivers `WM_HOTKEY` to the *registering
thread's* queue. macOS is the opposite: `RunApplicationEventLoop` does not
exist in 64-bit, the API is documented "not thread safe", and hot-key events
are dispatched by the **main** event loop (NSApplication pumps Carbon —
Cocoa and Carbon share dispatch), which winit owns. So registration happens
on the main thread in `main()` just before `ui.run()`, and the handler — on
the main thread — does exactly one thing: `tx.send(())` to a worker thread
that owns the tokio runtime and the engine connection. A 30 s `Transcribe`
therefore cannot stall the UI, which is the same property the Windows pump
buys a different way. Verified live: a bare (unbundled, no-run-loop) test
binary got `RegisterEventHotKey(⌃⌥D) → OSStatus 0`, proving both the link
and that nothing else on this machine owns the chord.

**The chord: ⌃⌥D.** The literal twin of Windows' Ctrl+Alt+D (⌃ = Ctrl,
⌥ = Alt) and free of macOS's own defaults — ⌘⌥D is show/hide Dock, ⌃⌘D is
look-up-in-dictionary, both avoided. Registration is deliberately
**non-exclusive** (options = 0, not `kEventHotKeyExclusive`): an app that
already owns the chord keeps it and both are notified. Known caveat, in the
module header: ⌃⌥ *is* the VoiceOver modifier, so a VoiceOver user should
rebind (v1 has no rebinding UI, exactly as on Windows).

**TCC, all three questions answered.** (1) The hotkey needs **nothing** —
verified empirically, and structurally true because Carbon hotkeys are
narrowly scoped. (2) `CGEventPost` needs the **Accessibility** grant
(literally `kTCCServicePostEvent`, which System Settings shows *inside* the
Accessibility pane — Apple DTS's own mapping; `CGPreflightPostEventAccess` is
the pedantically-exact query, noted in-module if the live test ever shows a
mismatch with AX trust). Input Monitoring is **not** involved anywhere in
this module. (3) **No Info.plist key exists for Accessibility** — decisive
check: the string table of `TCC.framework/Support/tccd` on 26.5 lists all 40
`NS*UsageDescription` keys TCC honors, and neither `NSAccessibilityUsage
Description` nor `NSInputMonitoringUsageDescription` is among them (the
widely-cargo-culted latter is inert). So `install-macos-dev.sh` needed **no
change at all** — unlike the CATap work, where `NSAudioCaptureUsageDescription`
genuinely gates the prompt. The one thing that *does* bite: TCC keys the grant
to the **code signature**, so every `install-macos-dev.sh` re-sign mints a new
identity and the Accessibility toggle may need re-adding after a reinstall —
the same dev-loop tax the mic grant already charges.

**Trust handling.** `AXIsProcessTrustedWithOptions(kAXTrustedCheckOptionPrompt)`
fires **once per app run**, on the **first chord press** — never at launch (an
app that asks before the user has ever dictated is asking for nothing). The
header is explicit that the prompt is *asynchronous and does not affect the
return value*, so the first press returns false, raises the dialog, and — the
important part — **does not start a recording it could never deliver**. That
pre-flight is the mac answer to Windows' UIPI clipboard fallback, and it is
strictly better: Windows cannot know until after the words exist. The ⚙
DICTATION card carries the hint (`st-dictation-hint`) whenever untrusted,
refreshed from three places — launch, every ⚙ open, and every press (the
worker pushes through `upgrade_in_event_loop`, the app's own cross-thread
plumbing) — so a grant made in System Settings clears the row without a
relaunch, which the AX trust value does support live (MDM-profile grants
excepted). Injection re-checks before typing, so a grant revoked mid-take
surfaces the hint instead of typing nothing.

**Chunking.** `CGEventKeyboardSetUnicodeString` has **no documented cap** —
the event object round-trips 500 UTF-16 units — but the *delivery* path
truncates silently at ~20 units (three independent implementations report it;
nobody explains it). We chunk at **16** units, weighing each `char` whole
before cutting, so a surrogate pair can never straddle two events (which
would type two replacement characters instead of one emoji). Each chunk is a
keydown+keyup pair with `CGEventSetFlags(0)` on both — an injected event
otherwise **inherits the physically-held modifiers**, and the chord's own ⌃⌥
can still be down, which is exactly the bug espanso hit. 4 ms between events
(the field runs 0–20 ms).

**Verified here:** clippy `-p syrinx-app -p syrinx-shared --all-targets -D
warnings` clean; **117** app tests (+11) + 7 shared green, including chunk-cap
/ surrogate-pair / round-trip / empty-input coverage and the pure `plan()`
transition table (untrusted+idle → Blocked, never a recording; trusted toggles
Start/Finish; grant revoked mid-take still finishes so the words survive).
Two `--ignored` live natives smokes both pass on this M3: hotkey registration
→ OSStatus 0, and the trust query answers and agrees with the hint. Offscreen
snapshots of all four ⚙ DICTATION states (mac trusted / mac untrusted /
Windows / Linux) read and correct. **This also closes the 2026-07-30 ledger
item (2)**, the card's hard 92px with a Linux-only second row: the height now
sums its optional rows (58px chrome + 34 per line + 40 for the wrapping
Accessibility hint), Linux is provably byte-identical at 58+34 = the authored
92px, and Windows' dead space became the chord line ("Ctrl+Alt+D — anywhere in
Windows") rather than shrinking. Both known traps respected: the wrapping hint
Text carries an explicit 32px height (preferred-width 0 in a fixed-height card
otherwise asks for one word per line).

**Left for the human, and only this:** the end-to-end chord. Grant
Accessibility to the reinstalled `Syrinx.app`, then with a third-party app
focused press ⌃⌥D, speak, press again, and watch the transcript type itself.
Nothing short of that can be verified from an agent session — dispatch needs
the app's own NSApplication run loop and a focused foreign window, and the
grant needs a human at System Settings.

**LINUX SESSION QUEUE** (consolidated 2026-07-26 — items parked from
Windows sessions; each also appears in its origin ledger entry above):
1. ~~`dictate/src/main.rs` still reads only `a.text` from LlmResult~~
   RESOLVED 2026-07-27: `refine()` now bails inside the wait loop the
   moment a matching req_id arrives with `error=true`, routing through the
   existing `Err(e)` arm for a single accurate "refinement unavailable
   (engine reported an LLM failure)" warn and immediate raw-transcript
   fallback (no 180 s wait); genuinely-empty and timeout results still take
   the "returned nothing" path. Verified on Linux (never builds on Windows):
   `cargo check`/`clippy -D warnings` clean for syrinx-dictate and
   syrinx-shared, `cargo test --bins` = 0 tests (bin-only crate).
2. ~~RPC-PROTOCOL §0/§11 method-count re-baseline~~ RESOLVED 2026-07-26
   with the RecordingLevel commit: §0/§11 now pin 70 methods / 11 signals
   / 3 props, lib.rs 84 fn — verified by hand against the decorator list.
3. ~~🗑 U+1F5D1 rendering: fontconfig will likely hand it to Noto Color
   Emoji (color glyph, ignores the text-style intent) — if so, switch the
   7 delete sites + del-kind modal to the U+FE0E text-presentation form~~
   RESOLVED 2026-07-27: no code change — the premise is false on the real
   stack. fc-match does rank Noto Color Emoji first for U+1F5D1, but the
   fontique fallback (`unstable-fontique-010`) skips the color font and
   draws the monochrome Noto Sans Symbols 2 basket, matched to ⇩/✎
   (verified in-app on Linux: screenshot of the voice-card action row).
   The planned switch actively regresses: fontique does not resolve
   variation sequences, so `🗑\u{FE0E}` renders tofu at every delete site
   (verified with a probe build carrying the 7-site edit, then reverted;
   a minimal repro also showed bare 🗑 fine / +FE0E tofu / explicit
   font-family fine). Candidate upstream Slint report. Note the del-kind
   modal contains no 🗑 — the 7 glyph sites were the full inventory.
4. ~~Test-mic meter Linux twin: the ⚙ Test button is gated off on Linux —
   the mic dropdown holds pactl source ids that the §14 engine recorder
   can't resolve. Either compute levels app-side from the existing
   `parecord` capture path, or teach the Linux arm to translate a pactl
   source to its PortAudio name. UI + RecordingLevel plumbing are already
   shared; only the level source is missing.~~
   RESOLVED 2026-07-27: levels come app-side (option 1 — the meter now
   tests the same parecord path Linux real captures use). The Linux arm
   of `mic_test_start` spawns `parecord --raw --rate=24000 --channels=1
   --format=s16le --latency-msec=75 [--device=<pactl source>]` — the
   latency flag is mandatory (default fragsize 96000 B ≈ 2 s of audio
   per delivery, a slideshow) — and a reader task RMSes 3200-byte chunks
   (1600 samples ≈ 15 Hz) into `st-mic-level` with the same perceptual
   sqrt the RecordingLevel arm applies. The worker's holder cfg-splits
   (`MicTest` = §14 rec-id String on Win/mac, parecord child + reader
   task on Linux) so every call site plus the 2-min auto-stop, tab-leave
   off, and device-change restart stay shared and byte-identical. §0/§11
   RPC surface unchanged (70/11/3). Verified live on Linux: HDMI-sink
   monitor as the mic device, 12 s 440 Hz paplay tone — meter 0 in
   silence, ~30 % fill during the tone, back to 0 after; parecord dying
   (dead/absent source) springs the toggle back instead of lying.
5. (added 2026-07-27, Noah) Engine cold-start on Linux: the app's Linux
   worker only ever `connect_dbus()`es — with no engine on the bus a
   cold app sat engine-less, and §13's "lifecycle belongs to systemd +
   D-Bus activation" had no installer on dev checkouts (the packaging/
   templates existed but only the full scripts/install.sh rendered
   them).
   RESOLVED 2026-07-27: `engine/setup-linux-activation.sh` — the
   no-build subset of scripts/install.sh, rendering the SAME packaging/
   templates (single source of truth, cannot drift) into
   ~/.config/systemd/user + ~/.local/share/dbus-1/services, then
   daemon-reload + dbus ReloadConfig and an is-activatable check.
   Starts nothing by design (no [Install] — the app's first D-Bus call
   wakes the engine). syrinx-engine.service.in gains
   TimeoutStartSec=120: Type=dbus holds the job until the name claim,
   and the claim is imports-only (~13 s measured on the CPU box —
   warmup runs in the background AFTER the claim,
   syrinx_engine/__main__.py:143-149), so 120 s covers cold caches
   where bare non-systemd dbus activation would race its fixed 25 s
   limit. Verified cold: app+engine both dead → launch app alone →
   systemd activates the engine (static unit, journald logs), app
   connects and renders full data. Per-box step: re-run the script
   after moving a checkout; 4090-Linux must run it too (HANDOFF
   checklist).

**NEXT SESSION — macOS phase 3 (the port's last frontier):**
1. ~~System capture: BlackHole loopback driver detection~~ RESOLVED
   2026-08-03 — and then superseded: the native Core Audio process tap
   (`capture_mac.rs`) needs no driver at all; the driver path survives as
   the pre-14.2 fallback and the manual ⚙ override.
2. ~~Dictation: NSEvent/Carbon global hotkey + CGEventPost injection,
   in-app like dictation_win~~ RESOLVED 2026-08-03 —
   `app/src/dictation_mac.rs`, ⌃⌥D, Carbon `RegisterEventHotKey` +
   CGEvent unicode injection behind the Accessibility grant. See the
   dated Findings entry above; the live chord test belongs to the user.
3. ~~Before either: a macOS phase-1/2 validation pass~~ RESOLVED
   2026-07-29/30 (phases 1 and 2 landed on the M3, MPS included).
   ~~Remaining mac work is Seed-VC / Vevo MPS verification~~ RESOLVED
   2026-08-04 (dated entry below); the self-contained bundle remains.

**2026-08-04 (engine) — the VC workers land on Metal; music VC exercised
end-to-end on mac.** Ledger correction first: the 8/3 note that "speech VC
ran on CPU" was true only of vevo — `seed_vc/api.py` AND
`seed_vc/inference.py` both walk cuda→mps→cpu at module import, so Seed-VC
speech was already on Metal. What Metal actually did to Seed-VC was worse
than slow: music mode (`f0=True`) hard-crashed every time — seed-vc's RMVPE
extractor returns a float64 numpy f0 and both call sites do
`torch.from_numpy(f0).to(device)`, and MPS has no float64 ("Cannot convert
a MPS Tensor to float64"). Fixed adapter-side without touching the GPL
package: `_f0_float32()` wraps the stream state's public `f0_fn` (mps-only;
the f0 is quantized into 512 mel bins two lines downstream, so float32 is
lossless). Both workers now carry the luxtts-style `_device()` ladder with
env overrides (`SYRINX_SEEDVC_DEVICE` / `SYRINX_VEVO_DEVICE`, both proven
live by forcing the cpu benchmark legs). vevo_worker had no rung at all:
`_load()` was cuda-else-cpu, `_free_gpu()` only knew `torch.cuda`
(now also `torch.mps.empty_cache()` — unified memory, keeps demucs and
Vevo2 from overlapping), and both Amphion pipelines take `device=` cleanly
(nothing outside their unused infer_*.py entrypoints hardcodes cuda).
seedvc_worker steers the package by re-pinning its two import-time
snapshots (`_pin_device()` rebinds `seed_vc.api._device` +
`seed_vc.inference.device` before the first load — both modules read the
global at call time; inference_realtime/inference_v2 are dead here,
realtime=False). demucs htdemucs rides the same rung in both workers:
torch 2.13 has MPS stft/istft kernels, stems match cpu to ~1e-4 rms
(loses only under ~10 s inputs where kernel builds dominate — music inputs
are songs, no heuristic). Two dtype non-events, documented in code:
seed-vc's whisper-small loads fp16 unconditionally and SURVIVES Metal (its
encoder output is cast straight back to fp32; nothing samples fp16
probabilities — the actual Qwen-TTS failure mode), and
PYTORCH_ENABLE_MPS_FALLBACK stays deliberately off (zero missing ops in
all four graphs; a future missing kernel should fail loudly, not crawl).
Live proof, all four paths on the M3, warm (2nd request): seedvc speech
9.7 s mps vs 23.3 s cpu (7 s src, 25 steps); seedvc music 76.6 vs 161.8
(30 s song, 30 steps); vevo timbre 11.5 vs 22.1 (32 steps); vevo2 music
56.5 vs 103.6; demucs alone 4.2 vs 7.8 (30 s mix). Every mps output is
finite, rms-sane, and faster-whisper round-trips the source text VERBATIM
— and music VC (both engines) is now exercised end-to-end on mac, which
the 8/3 testing pass never reached. Pulled ~2.9 GB of first-run weights
(whisper-medium for Vevo2's Coco tokenizer, hubert for Vevo-Timbre, RMVPE,
htdemucs); Vevo2 FM + amphion/Vevo snapshots were already cached. Tests:
new `engine/tests/test_vc_worker_device.py` (18, venv-independent
fake-torch subprocess pattern; ladders, override, `_pin_device` rebinding
+ not-yet-imported no-op, f0 cast), stub torch gains `device`; suite 652
@ 95.65%. Residuals: (1) tada.py's dtype block stays cuda-gated — the
main venv has no hume-tada installed and live proof costs ~14.6 GB of
weights (tada-1b 3.9 + tada-codec 10.7, the codec's allow_patterns pull
all ten aligners); a bf16-on-Metal op sweep (matmul, softmax, layer_norm,
SDPA, multinomial, conv1d/T, …) came back all-finite, so the extension
looks safe when someone pays the download. (2) `_f0_float32` rides
`_V1StreamState.f0_fn` staying a public attribute — a seed-vc upgrade that
inlines it returns music mode to a LOUD hard error on Metal, not silent
corruption. (3) Vevo2 remixes peak 0.99 into the normalizer on cpu and
mps alike — pre-existing, not a device artifact.

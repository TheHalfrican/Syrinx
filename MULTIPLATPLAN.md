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
| System-audio capture (create-voice, ⇄ song capture) | `parecord --device=<sink>.monitor` | Linux-native | Win: WASAPI loopback ✅ (2026-07-24, app-side `capture_win.rs` — the native twin of parecord; ◉/⚙/♫ affordances unhidden via `system-capture-supported`) · mac: loopback driver (BlackHole), still phase-3-future; ♫ music mode stays import-file-only on mac |
| Dictation (dictate/) | pw-record + wtype + wlr-layer-shell + compositor keybind | Wayland-native **by design** | **Untouched, permanently.** Win ✅ (2026-07-24, in-app `dictation_win.rs`: Ctrl+Alt+D + SendInput, no pill) · mac still phase-3-future |
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
| Qwen-TTS | CUDA ✅ | CUDA ✅ (Base + CustomVoice, 1.7B & 0.6B) | MPS (verify) / CPU — consider MLX port later |
| LuxTTS (venv) | CPU ✅ / CUDA (k2 cuda wheels) | ❌ blocked (2026-07-24): piper-phonemize ships no win wheels/sdist; k2 CPU wheels for win_amd64 EXIST and work (exact HANDOFF pin verified) — revisit if piper-phonemize gains Windows support | verify k2 mac wheels (CPU) |
| faster-whisper (CTranslate2) | CPU ✅ / CUDA ✅ | CPU / CUDA ✅ (base/large/turbo — see cu12 DLL gotcha, Findings 2026-07-24 sweep) | CPU (no Metal in CT2 — still fast) |
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
1. System capture: BlackHole loopback driver detection (document install,
   detect absence gracefully) behind the same `system-capture-supported`
   capability property; the Capture enum's macOS arm mirrors capture_win.
2. Dictation: NSEvent/Carbon global hotkey + CGEventPost injection,
   in-app like dictation_win (dictate/ stays Linux-only).
3. Before either: a macOS phase-1/2 validation pass (transport, supervised
   lifecycle, device matrix incl. MPS, packaging) — the Windows campaign's
   playbook in this file's Findings is the template. No Mac hardware has
   been touched yet; everything above is design-ready, not started.

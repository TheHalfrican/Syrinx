# Syrinx — Architecture

> **2026-07 addendum:** this document describes the Linux reference
> architecture, which is unchanged. Syrinx now also runs natively on Windows
> via small strategy seams — same core, different native mechanism per OS:
> D-Bus ↔ JSON-RPC/WebSocket transport ([`RPC-PROTOCOL.md`](RPC-PROTOCOL.md)),
> systemd/D-Bus activation ↔ app-supervised child engine (§13 there),
> parecord/PipeWire ↔ engine-side sounddevice capture, XDG ↔ platformdirs.
> The seam design and per-OS findings live in
> [`../MULTIPLATPLAN.md`](../MULTIPLATPLAN.md).

## Design principle

The pain of a portable desktop app comes from abstractions that exist *only* to
be portable. Syrinx targets exactly one environment — **CachyOS + Hyprland** —
so it deletes those layers and uses the native primitive in each case:

| Portable app's pain | Native primitive Syrinx uses instead |
|---------------------|--------------------------------------|
| WebKitGTK webview (blank/white renders) | **Slint** — GPU scene graph, native Wayland |
| PyInstaller onefile re-extracting a 2.6 GB runtime every launch | **System-installed** engine (`python-pytorch-*` from the repos) as a hot `systemd --user` service |
| Always-on-top overlay hacks | **wlr-layer-shell** surface (like waybar/wofi) |
| App-level global hotkey + synthetic keycodes | **Hyprland keybind** → IPC + **ydotool** paste |
| GStreamer `autoaudiosink` plugin roulette | **PipeWire** directly |

## Processes

```
  Hyprland keybind (SUPER+D)
        │ exec
        ▼
  ┌──────────────────┐      D-Bus       ┌────────────────────────────┐
  │ syrinx-dictate   │◄────────────────►│ syrinx-engine (Python)     │
  │ (Rust)           │                  │  Kokoro TTS · LuxTTS clone │
  │ • layer-shell UI │                  │  faster-whisper (STT)      │
  │ • PipeWire capture│                 │  Qwen3 personality LLM     │
  │ • ydotool paste  │                  │  pedalboard effects        │
  └──────────────────┘                  │  PipeWire playback         │
                                        └───────────┬────────────────┘
                                                     │ D-Bus
                        ┌────────────────┐  D-Bus    │
                        │ syrinx-mcp     │◄──────────┤
                        │ (agent speak)  │           │
                        └────────────────┘  ┌────────▼─────────┐
                                             │ syrinx (Slint UI)│
                                             └──────────────────┘
```

Everything hangs off one **D-Bus session service**: `sh.syrinx.Engine1`.
Because it's D-Bus, the whole app is scriptable from a shell (`busctl call …`),
which is great for testing.

## D-Bus interface: `sh.syrinx.Engine1`

Bus name `sh.syrinx.Engine`, object `/sh/syrinx/Engine`. The authoritative
contract is the Rust proxy trait in **`shared/src/lib.rs`** — both `syrinx-app`
and `syrinx-dictate` build against it. Summary by area:

- **TTS / STT**: `Speak`, `Transcribe` (sync, short clips),
  `TranscribeFile → req_id` (async; partials via `TranscribeProgress`, final via
  `TranscribeResult`), `ListVoices`, `Cancel`.
- **Voice conversion**: `ConvertVoice(audio_path, profile_id, engine) → gen_id` —
  style-preserved VC (the source's words/timing/prosody survive; only the timbre
  changes). Engine `""` = `chatterbox_vc` (Resemble's S3 tokenizer + S3Gen, the
  same `ResembleAI/chatterbox` weights the TTS backend uses). Progress/errors via
  `GenerationProgress`; the result auto-plays and lands in history like `Speak`.
- **Profiles**: create / list / get / update / delete, `AddSample`,
  `UpdateSampleText`, `SetProfileAvatar`, `ExportProfile` / `ImportProfile`
  (portable zips), `CloneVoice`.
- **History**: list / play (+ `PlayHistoryAt`), pause / resume / seek,
  `SetVolume`, star / delete / regenerate, `ExportPackage`, `HistoryAudioPath`.
- **LLM**: `ComposeProfile`, `RewriteProfile`, `RefineTranscript` — all return a
  request id; results arrive via the `LlmResult` signal (D-Bus replies time out
  at ~25 s, so anything slow goes id + signal).
- **Effects**: `ListEffectPresets`, `SetEffect`, `ApplyHistoryEffects`, plus the
  chain editor: `ListEffects` (definitions + param ranges), preset CRUD
  (`Get/Create/Update/DeleteEffectPreset`), `PreviewEffects`.
- **Captures**: `SaveCapture`, `ListCaptures`, `UpdateCapture`, `DeleteCapture`
  (text-only transcription history).
- **Models**: `ListModels`, `Hardware`, `DownloadModel`, `DeleteModel`,
  `SetActiveModel` (hot-switches STT / LLM / voice engine).

**Signals**: `GenerationProgress` (incl. `error: …`), `AudioLevel`,
`PlaybackInfo` / `PlaybackProgress`, `LlmResult`,
`TranscribeProgress` / `TranscribeResult`, `ModelProgress`,
`SpeakStarted` / `SpeakEnded`.

**Properties**: `ModelLoaded b` · `Backend s` (`cuda`|`rocm`|`cpu`).

Audio bytes never travel over D-Bus in bulk: the engine plays TTS output itself
via PipeWire and emits only lightweight `AudioLevel` samples for visualization.

## Dictation flow

1. Hyprland: `bind = SUPER, D, exec, syrinx-dictate toggle`
2. `syrinx-dictate` maps a **layer-shell** pill and starts audio capture.
3. On stop (keybind again), it sends the recording to `Transcribe`
   (faster-whisper). With `--refine` (or `SYRINX_DICTATE_REFINE=1`) the raw
   transcript takes an extra LLM cleanup pass — fillers out, punctuation in —
   falling back to the raw text on any failure.
4. Result → `wl-copy <text>` then `ydotool key ctrl+v` into the focused window.
5. Pill fades out.

## Packaging

- **PKGBUILD → AUR.** Native deps: `python-pytorch-cuda` / `-rocm` / `-cpu`,
  `whisper.cpp`, `ydotool`, `wl-clipboard`, `slint` toolchain at build time.
- **`systemd --user`** unit for the engine (socket-activatable), so it's loaded
  once at login and both the app and the pill attach instantly.

## Design questions — resolved

- STT: **faster-whisper** (CTranslate2) in the engine venv; no whisper.cpp.
- Engine always separate (a hot D-Bus service) — UI responsiveness won.
- Model storage: `~/.local/share/syrinx/` (`SYRINX_DATA_DIR` overrides);
  active-model choices persist in `models.json`; downloads via the Models tab.
- Backend switching: live, no restart — the Models tab hot-swaps STT / LLM /
  voice engines (`SetActiveModel`); heavyweight cloning engines run as
  isolated-venv worker subprocesses (LuxTTS is the template).
- Ops slower than the ~25 s D-Bus reply timeout return a request id and
  deliver results via signals (`LlmResult`, `TranscribeResult`, …).

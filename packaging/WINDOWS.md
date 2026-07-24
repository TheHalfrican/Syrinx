# Syrinx on Windows — packaging

This is the Windows analog of `scripts/install.sh` (Linux beta install). Where
Linux runs the engine from the checkout under systemd + D-Bus activation, the
Windows build is a **relocatable per-user app**: the Slint app owns the engine as
a supervised child process (RPC over loopback WebSocket — `docs/RPC-PROTOCOL.md`
§13), so there is **no service** to install. The app *is* the lifecycle manager.

## Artifacts

| File | Role |
|---|---|
| `scripts/build-windows.ps1` | Builds `dist/syrinx-windows-x64/` (the bundle). |
| `packaging/windows/syrinx.nsi` | NSIS installer → `dist/SyrinxSetup-x64.exe`. |
| `packaging/windows/syrinx-launch.vbs` | Windowless launch wrapper (shortcut target). |
| `packaging/windows/syrinx-firstrun.ps1` | First-run bootstrap (pulls torch + ML stack). |
| `packaging/windows/syrinx.ico` | App icon (rendered from `packaging/syrinx.svg`). |

## Bundle layout (`dist/syrinx-windows-x64/`)

```
syrinx-app.exe                         Rust app (cargo build --release -p syrinx-app)
syrinx-launch.vbs                      shortcut target: sets PATH+env, launches the app
syrinx-firstrun.ps1                    torch + ML stack bootstrap
syrinx.ico                             icon
VERSION
engine\
  .venv\                               embedded CPython 3.12 (NOT a real venv; named
    python.exe                           .venv so the app's exe-ancestor probe finds it)
    python312._pth                     patched: + Lib\site-packages, uncomment import site
    Scripts\syrinx-engine.exe          pip console script (the engine entry point)
    Lib\site-packages\                 syrinx_engine + torch-free base deps
  wheels\syrinx_engine-*.whl           bundled wheel (first-run re-installs it in place)
tools\
  sox.exe + *.dll                      qwen-tts imports sox; goes on the engine PATH
```

Installed to `%LOCALAPPDATA%\Programs\Syrinx` (per-user, no elevation).

## Why the `engine\.venv\Scripts\syrinx-engine.exe` path

`app/src/engine_proc.rs` resolves the engine executable in this order (§13.2):

1. `SYRINX_ENGINE_CMD` (absolute path, verbatim),
2. `engine/.venv/Scripts/syrinx-engine.exe` relative to the **cwd**,
3. the same relative to each **ancestor of the app-exe dir**,
4. `syrinx-engine` on `PATH`.

The bundle satisfies **both** #1 and #3:

- The embedded environment is laid out exactly like a dev venv
  (`engine/.venv/Scripts/syrinx-engine.exe`), so with the app exe at
  `<install>\syrinx-app.exe`, probe #3 joins `<install>\engine\.venv\Scripts\…`
  and matches — **no env var required**.
- The launcher (`syrinx-launch.vbs`) *also* sets `SYRINX_ENGINE_CMD` to that
  absolute path (belt-and-suspenders), and — its real job — prepends `tools\`
  to `PATH` so the spawned engine finds `sox`. The app inherits the wrapper's
  environment; the engine inherits the app's.

`engine_proc.rs` is **not** modified — the bundle is shaped to what it already
probes.

## Embedded-Python mechanics (the fiddly part)

The python.org **embeddable** zip has no `pip`, no `site`, and a `._pth` that
disables site-packages. The build script:

1. Extracts `python-3.12.x-embed-amd64.zip` into `engine\.venv\`.
2. Rewrites `python312._pth` to add `Lib\site-packages` and **uncomment
   `import site`** (without this, pip and installed packages don't import).
3. Runs `bootstrap.pypa.io/get-pip.py` with the embedded interpreter.
4. `pip install`s the **torch-free base deps** (`dbus-next`, `websockets`,
   `platformdirs`, `numpy`, `sounddevice`, `soundfile`) and the engine wheel
   `--no-deps`. torch/kokoro/faster-whisper/pedalboard/qwen are **not** bundled.
5. **Build-time proof (required):**
   `python -c "import syrinx_engine, syrinx_engine.rpc, …"` must succeed with
   base deps only. (`import syrinx_engine.core` pulls torch and is *not* proven
   here — that path comes alive after first-run.)

The engine wheel is built out-of-tree with `pip wheel --no-deps` so the build
never writes into the concurrent-agent-owned `engine/` source tree.

## First-run flow

The bundle is torch-free by design (the plan: "CUDA torch pulled on first run").
`syrinx-firstrun.ps1` runs once on the target — from the NSIS finish-page
checkbox or the "Syrinx first-run setup" Start-Menu entry — with progress
visible in a PowerShell window. It is re-runnable and:

1. Puts `tools\` on `PATH` (for the sox import proof).
2. Probes `nvidia-smi` → CUDA vs CPU (`-Cpu` forces CPU).
3. **Repairs the console script**: `pip install --force-reinstall --no-deps`
   the bundled wheel. pip's Windows console-script `.exe` embeds an absolute
   interpreter path at *build* time; the bundle is built elsewhere than it is
   installed, so this rewrites `Scripts\syrinx-engine.exe` to the interpreter at
   the **installed** location. (This is the one relocatability wrinkle of
   embedded-python + pip console scripts.)
4. Installs torch: **CUDA** from the `cu130` index (Linux parity; the `cu128`
   index tops out at torch 2.11 — 2026-07-24 finding), or **CPU** from the `cpu`
   index. Windows default-PyPI torch is CPU-only, so the index-url is mandatory.
5. Installs the ML stack via `<wheel>[qwen]` + `numba>=0.60` (pulls kokoro,
   misaki, faster-whisper, pedalboard, qwen-tts, transformers; torch already
   satisfied stays put). On CUDA it also installs `nvidia-cublas-cu12` +
   `nvidia-cudnn-cu12` (the ctranslate2 `cublas64_12.dll` fix). Per the
   2026-07-24 finding, **only cublas** may reach the loader ahead of torch's
   bundled cu13 cuDNN — that ordering lives in the engine's `stt.py`
   (`os.add_dll_directory`), not in packaging; the installer only lays the
   wheels down.
6. **Import proof:** `import torch, kokoro, faster_whisper, pedalboard, sox` — a
   bad combination fails at setup, not at first conversion (the setup-script
   philosophy).

## License boundaries preserved

- **Seed-VC (GPL-3.0)** and the **Amphion clone (Vevo/Vevo2)** are **never
  bundled** and never installed by first-run. They install on demand into their
  own isolated venvs via `engine/setup-seedvc.*` / `engine/setup-vevo.*`
  (Linux: `.sh`; the Windows `.ps1` equivalents are owned by the engine agents),
  exactly as on Linux. The pins for those live in those setup scripts — this
  packaging does not duplicate them.
- No model weights ship. First-run pulls only pip packages; checkpoints download
  per-user through the app's Models tab.

## What the uninstaller touches

- **Removes:** `%LOCALAPPDATA%\Programs\Syrinx` (the whole install tree,
  including the embedded python + everything first-run added), the Start-Menu
  shortcuts, and the per-user Add/Remove-Programs registry key.
- **Preserves:** `%LOCALAPPDATA%\syrinx\syrinx` — voices, history, settings,
  `rpc.json`. Mirrors `install.sh --uninstall` leaving the checkout untouched.

## Known gaps / out of scope

- **Code signing** — the exe, installer, and VBS are unsigned; SmartScreen will
  warn on first launch. Out of scope per the plan.
- **Auto-update** — none. Out of scope per the plan (§ Phase 3).
- **Dictation** and **system-audio capture** are Windows phase-3 features; the
  app hides those affordances off-Linux. Mic capture works (sounddevice).
- The VBS launcher relies on Windows Script Host (enabled by default). If WSH is
  disabled by policy, launch `syrinx-app.exe` after setting `SYRINX_ENGINE_CMD`
  and `PATH` yourself.
- First-run needs network (PyTorch index + PyPI) and ~2–7 GB (CUDA) of download.

## A future CI release job would run

```powershell
# 1. build the bundle (torch-free)
pwsh scripts/build-windows.ps1

# 2. pack the installer (portable NSIS or NSIS.NSIS on PATH)
makensis packaging/windows/syrinx.nsi          # -> dist/SyrinxSetup-x64.exe

# 3. (optional) smoke: install to a scratch dir, CPU first-run, boot the engine
$scratch = "$env:TEMP\syrinx-verify"
dist/SyrinxSetup-x64.exe /S /D=$scratch          # silent install to scratch
& "$scratch\engine\.venv\python.exe" -c "import syrinx_engine"
pwsh "$scratch\syrinx-firstrun.ps1" -Cpu         # CPU torch (~200 MB)
```

Code signing (`signtool`) would slot between steps 2 and 3 once a certificate
exists.

#!/usr/bin/env pwsh
#Requires -Version 7
#
# Syrinx — Windows bundle builder (MULTIPLATPLAN §2.2).
#
# Produces  dist/syrinx-windows-x64/  : the relocatable app bundle that the NSIS
# installer (packaging/windows/syrinx.nsi) packs into dist/SyrinxSetup-x64.exe.
#
# The bundle is deliberately torch-free: it ships the Rust app, an embedded
# CPython 3.12 with the engine + its *base transport/runtime* deps only, a sox
# binary, an icon, and the first-run bootstrap that pulls CUDA/CPU torch + the ML
# stack on the target machine (the plan's "CUDA torch pulled on first run").
#
#   scripts/build-windows.ps1                 build the bundle
#   scripts/build-windows.ps1 -SkipCargo      reuse an existing release exe
#   scripts/build-windows.ps1 -Clean          wipe caches first
#
# Deterministic + re-runnable: downloads are cached and checksum-agnostic only in
# that a present file is reused; the dist tree is rebuilt from scratch each run.
# Fails loudly (`$ErrorActionPreference = Stop`, explicit guards).

[CmdletBinding()]
param(
    [switch]$SkipCargo,          # reuse target/release/syrinx-app.exe as-is
    [switch]$Clean,              # delete the download cache before building
    [string]$PythonVersion = '3.12.10',
    [string]$OutName = 'syrinx-windows-x64'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# ---------------------------------------------------------------- paths / log
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$DistRoot = Join-Path $RepoRoot 'dist'
$BundleDir = Join-Path $DistRoot $OutName
$CacheDir = Join-Path $DistRoot '.build-cache'
$EngineSrc = Join-Path $RepoRoot 'engine'
$PkgWin = Join-Path $RepoRoot 'packaging\windows'

function Log  ($m) { Write-Host "`n==> $m" -ForegroundColor Cyan }
function Ok   ($m) { Write-Host "  ok $m" -ForegroundColor Green }
function Die  ($m) { Write-Host "error: $m" -ForegroundColor Red; exit 1 }

# The embedded env lives at engine/.venv so the app's own executable-resolution
# (RPC-PROTOCOL §13.2 step 3: engine/.venv/Scripts/syrinx-engine.exe relative to
# an app-exe ancestor) finds it with no env var — the same path a dev venv uses.
$EmbedDir  = Join-Path $BundleDir 'engine\.venv'
$SitePkgs  = Join-Path $EmbedDir  'Lib\site-packages'
$Scripts   = Join-Path $EmbedDir  'Scripts'
$PyExe     = Join-Path $EmbedDir  'python.exe'
$EngineExe = Join-Path $Scripts   'syrinx-engine.exe'
$ToolsDir  = Join-Path $BundleDir 'tools'
$WheelDir  = Join-Path $BundleDir 'engine\wheels'

# Torch-free base deps: everything in engine/pyproject.toml [dependencies] that
# does NOT drag torch. kokoro/misaki/faster-whisper/pedalboard (and qwen) are the
# ML half and are installed by the first-run bootstrap together with torch.
$BaseDeps = @('dbus-next>=0.2.3', 'websockets>=13', 'platformdirs>=4',
              'numpy', 'sounddevice', 'soundfile')

if ($Clean -and (Test-Path $CacheDir)) { Remove-Item -Recurse -Force $CacheDir }
New-Item -ItemType Directory -Force -Path $CacheDir | Out-Null

# ---------------------------------------------------------------- guards
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Die 'cargo not found — install Rust first' }
foreach ($f in @('syrinx-firstrun.ps1', 'syrinx-launch.vbs', 'syrinx.ico')) {
    if (-not (Test-Path (Join-Path $PkgWin $f))) { Die "packaging/windows/$f missing" }
}

# ---------------------------------------------------------------- 1. cargo
$AppExe = Join-Path $RepoRoot 'target\release\syrinx-app.exe'
if ($SkipCargo) {
    if (-not (Test-Path $AppExe)) { Die "-SkipCargo but $AppExe missing" }
    Ok 'reusing existing release exe'
} else {
    Log 'Building release app binary (cargo build --release -p syrinx-app)'
    & cargo build --release -p syrinx-app
    if ($LASTEXITCODE -ne 0) { Die 'cargo build failed' }
    if (-not (Test-Path $AppExe)) { Die 'syrinx-app.exe missing after build' }
    Ok $AppExe
}

# ---------------------------------------------------------------- 2. fresh dist
Log "Preparing bundle $BundleDir"
if (Test-Path $BundleDir) { Remove-Item -Recurse -Force $BundleDir }
New-Item -ItemType Directory -Force -Path $BundleDir, $EmbedDir, $ToolsDir, $WheelDir | Out-Null
Copy-Item $AppExe (Join-Path $BundleDir 'syrinx-app.exe')
Ok 'syrinx-app.exe'

# ---------------------------------------------------------------- 3. embedded python
$EmbedZip = Join-Path $CacheDir "python-$PythonVersion-embed-amd64.zip"
if (-not (Test-Path $EmbedZip)) {
    Log "Downloading CPython $PythonVersion embeddable"
    $url = "https://www.python.org/ftp/python/$PythonVersion/python-$PythonVersion-embed-amd64.zip"
    Invoke-WebRequest -Uri $url -OutFile $EmbedZip
}
Log 'Extracting embedded python'
Expand-Archive -Path $EmbedZip -DestinationPath $EmbedDir -Force

# Uncomment `import site` and add Lib\site-packages so pip + installed packages
# import. The ._pth filename tracks the minor version (python312._pth).
$pthName = "python$($PythonVersion.Split('.')[0])$($PythonVersion.Split('.')[1])._pth"
$pth = Join-Path $EmbedDir $pthName
if (-not (Test-Path $pth)) { Die "expected $pthName in the embeddable zip" }
Set-Content -Path $pth -Value @(
    "python$($PythonVersion.Split('.')[0])$($PythonVersion.Split('.')[1]).zip",
    '.', 'Lib\site-packages', '', 'import site'
) -Encoding ascii
Ok $pthName

# get-pip into the embedded runtime (embeddable ships no ensurepip)
$GetPip = Join-Path $CacheDir 'get-pip.py'
if (-not (Test-Path $GetPip)) {
    Invoke-WebRequest -Uri 'https://bootstrap.pypa.io/get-pip.py' -OutFile $GetPip
}
Log 'Bootstrapping pip into the embedded runtime'
& $PyExe $GetPip --no-warn-script-location
if ($LASTEXITCODE -ne 0) { Die 'get-pip failed' }
Ok 'pip'

# ---------------------------------------------------------------- 4. base deps + build backend
Log 'Installing torch-free base deps + wheel build backend'
# setuptools/wheel are the engine's PEP-517 build backend; installing them here
# lets us build with --no-build-isolation (deterministic + no slow per-build
# isolated-env download). They also stay useful in the bundle: first-run's pip
# leans on them. pip itself must stay (first-run repairs the console script and
# pulls torch through it).
& $PyExe -m pip install --no-warn-script-location setuptools wheel @BaseDeps
if ($LASTEXITCODE -ne 0) { Die 'base dep install failed' }

Log 'Building the syrinx-engine wheel (out-of-tree; does not touch engine/)'
& $PyExe -m pip wheel --no-deps --no-build-isolation $EngineSrc -w $WheelDir
if ($LASTEXITCODE -ne 0) { Die 'engine wheel build failed' }
$Wheel = Get-ChildItem $WheelDir -Filter 'syrinx_engine-*.whl' | Select-Object -First 1
if (-not $Wheel) { Die 'no syrinx_engine wheel produced' }
Ok $Wheel.Name

Log 'Installing syrinx-engine (--no-deps; console script -> Scripts\syrinx-engine.exe)'
& $PyExe -m pip install --no-warn-script-location --no-deps --force-reinstall $Wheel.FullName
if ($LASTEXITCODE -ne 0) { Die 'engine install failed' }
if (-not (Test-Path $EngineExe)) { Die "console script not generated at $EngineExe" }
Ok $EngineExe

# ---------------------------------------------------------------- 5. import proof (required)
Log 'Proof: import syrinx_engine with base deps (torch-free)'
& $PyExe -c "import syrinx_engine, syrinx_engine.rpc, syrinx_engine.paths, syrinx_engine.settings; print('syrinx_engine', syrinx_engine.__version__, 'imports OK (torch-free)')"
if ($LASTEXITCODE -ne 0) { Die 'engine import proof failed' }
Ok 'engine imports with base deps only'

# ---------------------------------------------------------------- 6. sox
Log 'Bundling sox (qwen-tts imports it at load; goes on the engine PATH)'
$soxPkg = Get-ChildItem (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages') `
    -Directory -Filter 'ChrisBagwell.SoX*' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $soxPkg) { Die 'sox not found — winget install ChrisBagwell.SoX' }
$soxRoot = Get-ChildItem $soxPkg.FullName -Directory -Filter 'sox-*' | Select-Object -First 1
if (-not $soxRoot) { Die "sox exe dir not found under $($soxPkg.FullName)" }
Copy-Item (Join-Path $soxRoot.FullName 'sox.exe') $ToolsDir
Copy-Item (Join-Path $soxRoot.FullName '*.dll') $ToolsDir
& (Join-Path $ToolsDir 'sox.exe') --version 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) { Die 'bundled sox.exe does not run (missing DLL?)' }
Ok "sox + $((Get-ChildItem $ToolsDir -Filter *.dll).Count) DLLs"

# ---------------------------------------------------------------- 7. launcher, icon, firstrun
Log 'Copying launcher / icon / first-run bootstrap'
Copy-Item (Join-Path $PkgWin 'syrinx-launch.vbs')   $BundleDir
Copy-Item (Join-Path $PkgWin 'syrinx-firstrun.ps1') $BundleDir
Copy-Item (Join-Path $PkgWin 'syrinx.ico')          $BundleDir
$ver = (Select-String -Path (Join-Path $RepoRoot 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1).Matches.Groups[1].Value
Set-Content (Join-Path $BundleDir 'VERSION') "syrinx $ver (windows-x64, python $PythonVersion)" -Encoding ascii
Ok 'bundle assets'

# ---------------------------------------------------------------- summary
$size = [math]::Round(((Get-ChildItem $BundleDir -Recurse -File | Measure-Object Length -Sum).Sum / 1MB), 1)
Log 'Bundle complete'
Write-Host "  $BundleDir  ($size MB, torch-free)" -ForegroundColor Green
Write-Host "  next: makensis packaging/windows/syrinx.nsi  ->  dist/SyrinxSetup-x64.exe" -ForegroundColor Green

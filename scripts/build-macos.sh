#!/usr/bin/env bash
#
# Syrinx macOS release build — the self-contained, relocatable bundle.
#
#   scripts/build-macos.sh                 build dist/Syrinx.app + dist/Syrinx-<v>.dmg
#   scripts/build-macos.sh --skip-build    reuse target/release/syrinx-app as-is
#   scripts/build-macos.sh --no-dmg        stop after the .app
#
# This is the sibling of packaging/WINDOWS.md's build-windows.ps1 and the
# grown-up version of scripts/install-macos-dev.sh. The dev installer wraps
# *this checkout* in a bundle and bakes the checkout path into the launcher;
# this one embeds a whole CPython 3.12 and the entire engine — torch included —
# inside Contents/Resources and computes every path from its own location, so
# the result runs on a Mac that has never seen the repo. Both scripts stay:
# the dev one is a two-minute rebuild loop, this one is a twenty-minute ship.
#
# WHERE IT DIFFERS FROM WINDOWS, and why. The Windows bundle is deliberately
# torch-free and pulls the ML stack on first run, because Windows torch comes
# in hardware flavours (cu130 vs cpu) that can only be chosen on the target.
# macOS has exactly one: a single 515 MB universal-MPS wheel that runs on every
# supported Mac. There is nothing to choose, so there is nothing to bootstrap —
# the bundle is fat, first launch is offline, and syrinx-firstrun has no mac
# analog. That decision is what makes the DMG ~2 GB and is not revisitable
# without giving the user a first-run download to sit through.
#
# WHAT GOES IN THE BUNDLE
#
#   Contents/MacOS/Syrinx                    launcher (CFBundleExecutable)
#   Contents/MacOS/syrinx-app                the Rust binary the launcher execs
#   Contents/Resources/syrinx.icns
#   Contents/Resources/engine/.venv/         embedded CPython 3.12 + all deps
#   Contents/Resources/engine/tools/         vendored sox + its dylib closure
#   Contents/Resources/engine/setup-*.sh     the on-demand VC engine installers
#
# `.venv` is a lie of convenience, exactly as on Windows: it is a whole Python
# *prefix* (python-build-standalone), not a virtualenv. The name keeps the
# layout identical to the dev checkout and to the Windows bundle, so anyone
# reading engine_proc.rs's probe list recognises it.
#
# Resources rather than Frameworks or Helpers. The concern was that
# `codesign --verify --strict` treats Mach-O files under Contents/Resources as
# misplaced code; measured on macOS 26.5 it does not — a bundle with a signed
# executable and signed dylibs nested under Resources passes `--verify --strict
# --deep` and the nested executable still runs. Resources is also the only one
# of the three that needs no extra Info.plist bookkeeping, so it wins on being
# boring. If a future macOS tightens this, Contents/Helpers is the move; only
# ENGINE_REL below and the launcher's `res=` line would change.
#
# HOW THE APP FINDS THE ENGINE (docs/RPC-PROTOCOL.md §13.2, app/src/engine_proc.rs)
#
#   1. $SYRINX_ENGINE_CMD verbatim   <- what the launcher sets, computed from $0
#   2. engine/.venv/bin/syrinx-engine relative to cwd
#   3. …relative to each ancestor of the app-exe dir
#   4. syrinx-engine on PATH
#
# Probe 3 cannot fire for us: the app exe sits at Contents/MacOS and the engine
# at Contents/Resources, which is not an ancestor of it. Probe 1 is therefore
# load-bearing, and unlike the dev bundle's baked-in checkout path the launcher
# derives it from `dirname "$0"` — move Syrinx.app anywhere, or run it out of a
# mounted DMG, and it still resolves. engine_proc.rs is NOT modified.
#
# THE SHIM. pip writes console scripts with an absolute `#!` line pointing at
# whatever interpreter path existed at install time, which in a relocatable
# bundle is a build-machine path that means nothing on the target. Windows has
# the same wound and heals it by re-running pip on first launch; with no
# first-run step we cannot. So bin/syrinx-engine is not pip's script at all: it
# is a three-line /bin/sh shim that execs its sibling python with
# `-m syrinx_engine` (engine/syrinx_engine/__main__.py is the same entry point
# the console script would have called). One executable file, no arguments
# required — which is what engine_proc.rs spawns.
#
# SIGNING. Inside-out, never --deep, with the identity scripts/install-macos-dev.sh
# already uses, so the packaged app's designated requirement is byte-identical
# to the dev bundle's:
#
#     identifier "sh.syrinx.app" and certificate leaf = H"…"
#
# Same DR means the microphone / system-audio / Accessibility grants the user
# has already answered carry straight over from the dev bundle to this one with
# no re-prompt and no tccutil reset. That is the single best reason not to
# invent a separate bundle ID for the release build.
#
# Gatekeeper will still refuse this app on any Mac that did not mint the
# certificate — self-signed is not Developer ID, and `spctl --assess` failing is
# the expected outcome, not a bug in this script. Everything here is shaped so
# that the day a Developer ID exists, SYRINX_SIGN_IDENTITY is the only input
# that changes: per-binary hardened signing, two entitlement files, no --deep,
# and a timestamp requested whenever the identity did not come from us.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="Syrinx"
BUNDLE_ID="sh.syrinx.app"
LAUNCHER="Syrinx"                  # Contents/MacOS/Syrinx — CFBundleExecutable
APP_BIN="syrinx-app"               # the copied release binary the launcher execs
DEV_IDENTITY="Syrinx Dev Signing"  # what scripts/dev-signing-identity.sh makes
ENT_APP="packaging/entitlements-app.plist"
ENT_ENGINE="packaging/entitlements-engine.plist"
PYVER="3.12"                       # must match engine/pyproject.toml's floor and the dev venv

# Engine layout inside Contents/Resources. ENGINE_REL is the only place the
# choice of Resources-vs-Helpers is written down; the launcher's `res=` follows it.
ENGINE_REL="Resources/engine"
DISK_NEEDED_GB=14                  # app (~5) + DMG staging copy (~5) + compressed DMG (~2), with slack

if [[ -t 1 ]]; then
    BOLD=$'\033[1m'; RED=$'\033[31m'; GREEN=$'\033[32m'
    YELLOW=$'\033[33m'; BLUE=$'\033[34m'; RESET=$'\033[0m'
else
    BOLD=''; RED=''; GREEN=''; YELLOW=''; BLUE=''; RESET=''
fi

log()  { printf '\n%s==>%s %s%s%s\n' "$BLUE" "$RESET" "$BOLD" "$*" "$RESET"; }
ok()   { printf '%s  ok%s %s\n' "$GREEN" "$RESET" "$*"; }
hint() { printf '%shint:%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

human() { du -sh "$1" 2>/dev/null | awk '{print $1}'; }
secs()  { date +%s; }

# --------------------------------------------------------------------------
# args

DO_BUILD=1
DO_DMG=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build) DO_BUILD=0 ;;
        --no-dmg)     DO_DMG=0 ;;
        -h|--help)
            cat <<'EOF'
Syrinx macOS release build — self-contained bundle + drag-to-Applications DMG.

usage: scripts/build-macos.sh [--skip-build] [--no-dmg]

  (no args)      cargo build, assemble dist/Syrinx.app with an embedded CPython
                 3.12 and the whole engine, sign it, and pack dist/Syrinx-<v>.dmg
  --skip-build   reuse target/release/syrinx-app instead of rebuilding
  --no-dmg       stop after the .app

env:
  SYRINX_SIGN_IDENTITY   codesign identity. Defaults to "Syrinx Dev Signing"
                         when scripts/dev-signing-identity.sh has made it, and
                         to ad-hoc when nothing else is there. Set this to a
                         Developer ID and the build becomes shippable: it is
                         the only input the notarization path needs.

The build never writes into engine/ or engine/.venv — the wheel is built from a
copy and the dependency pins are read out of the dev venv, not into it.
EOF
            exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

# --------------------------------------------------------------------------
# guards

[[ "$(uname -s)" == "Darwin" ]] || die "this build is macOS-only"
command -v cargo >/dev/null   || die "cargo not found — install rust first"
command -v hdiutil >/dev/null || die "hdiutil not found (it ships with macOS)"
command -v rsync >/dev/null   || die "rsync not found (it ships with macOS)"

DEV_PY="$ROOT/engine/.venv/bin/python"
[[ -x "$DEV_PY" ]] || die \
    "engine/.venv/bin/python not found. This build reads its dependency pins out
     of the dev venv — the proven-on-this-machine versions are the whole point —
     so the dev environment has to exist first:
       cd engine && uv venv --python $PYVER .venv && .venv/bin/pip install -e '.[qwen]'"

AVAIL_GB="$(df -g "$ROOT" | awk 'NR==2 {print $4}')"
[[ "${AVAIL_GB:-0}" -ge "$DISK_NEEDED_GB" ]] || die \
    "only ${AVAIL_GB}G free on this volume; the fat bundle plus its DMG staging copy
     needs about ${DISK_NEEDED_GB}G. Free some space or pass --no-dmg (~7G)."

# --------------------------------------------------------------------------
# the interpreter to embed
#
# python-build-standalone, the same distribution the dev venv was built from —
# `uv python find` resolves the exact 3.12.x uv installed, so the bundle and the
# venv the pins came from are the same interpreter down to the patch level. That
# matters more than it looks: torch's wheel tag, every compiled .so in
# site-packages and every .pyc pip precompiles are ABI-keyed to cp312, and a
# bundle built against a different 3.12 patch would still *import* but would
# silently diverge from the environment those pins were proven in.
#
# We copy the distribution rather than `uv python install --install-dir` into
# the bundle: the copy is offline, deterministic, and cannot pull a newer patch
# release out from under the pins. python-build-standalone is relocatable by
# construction (bin/python3.12 statically links libpython and derives sys.prefix
# from its own argv[0]), so a plain copy is all the relocation there is — no
# install_name_tool, no PYTHONHOME.

PY_SRC=""
if command -v uv >/dev/null; then
    # `uv python find` prints the interpreter; the distribution root is two up
    # from bin/. Managed installs only — a system python3.12 is not relocatable
    # and must never end up inside the bundle.
    if PY_FOUND="$(uv python find "$PYVER" 2>/dev/null)" && [[ -x "$PY_FOUND" ]]; then
        CAND="$(cd -- "$(dirname -- "$PY_FOUND")/.." && pwd)"
        [[ -d "$CAND/lib/python$PYVER" && "$CAND" == *"/uv/python/"* ]] && PY_SRC="$CAND"
    fi
fi
if [[ -z "$PY_SRC" ]]; then
    # Fall back to the interpreter the dev venv itself points at — pyvenv.cfg's
    # `home` is the standalone dist's bin/ — which is the same thing by a
    # different road, and works when uv is absent from PATH.
    HOME_LINE="$(awk -F' *= *' '/^home *=/ {print $2; exit}' "$ROOT/engine/.venv/pyvenv.cfg" 2>/dev/null || true)"
    if [[ -n "$HOME_LINE" && -d "$HOME_LINE/.." ]]; then
        CAND="$(cd -- "$HOME_LINE/.." && pwd)"
        [[ -d "$CAND/lib/python$PYVER" ]] && PY_SRC="$CAND"
    fi
fi
[[ -n "$PY_SRC" ]] || die \
    "could not locate a relocatable CPython $PYVER to embed. Install one with:
       uv python install $PYVER"

PY_FULL="$("$PY_SRC/bin/python$PYVER" -c 'import platform;print(platform.python_version())')"

# --------------------------------------------------------------------------
# signing identity — resolved before the long work, so a missing identity is a
# five-second failure and not a twenty-minute one. Same three-step ladder as
# scripts/install-macos-dev.sh; see its header for why the grep is on the whole
# listing rather than on the "valid identities" tail.

"$ROOT/scripts/dev-signing-identity.sh" --unlock 2>/dev/null || true

# --timestamp is conditional, and the condition is "did we mint this cert".
# A secure timestamp is mandatory for notarization and therefore right for any
# identity handed in from outside; for our own self-signed cert it is a network
# round-trip per signature, and this build makes thousands of them.
TIMESTAMP_ARG=()
if [[ -n "${SYRINX_SIGN_IDENTITY-}" ]]; then
    SIGN_ID="$SYRINX_SIGN_IDENTITY"
    SIGN_KIND=identity
    TIMESTAMP_ARG=(--timestamp)
elif security find-identity -p codesigning 2>/dev/null | grep -q "\"$DEV_IDENTITY\""; then
    SIGN_ID="$DEV_IDENTITY"
    SIGN_KIND=identity
    TIMESTAMP_ARG=(--timestamp=none)
else
    SIGN_ID="-"
    SIGN_KIND=adhoc
    TIMESTAMP_ARG=(--timestamp=none)
    hint "no signing identity — falling back to ad-hoc. The bundle will run, but
      every TCC grant re-keys to a build hash and the user re-answers all three
      prompts on every build. Fix it once with:
        scripts/dev-signing-identity.sh"
fi

if [[ "$SIGN_KIND" == identity ]]; then
    [[ -f "$ROOT/$ENT_APP" ]]    || die "$ENT_APP missing — can't sign the app binary"
    [[ -f "$ROOT/$ENT_ENGINE" ]] || die "$ENT_ENGINE missing — can't sign the embedded interpreter"
fi

# --------------------------------------------------------------------------
# version — [workspace.package] version in the workspace Cargo.toml, else 0.1.0
# (byte-identical to scripts/install-macos-dev.sh, deliberately)

VERSION="$(awk '
    /^\[/ { in_wp = ($0 == "[workspace.package]") }
    in_wp && /^[[:space:]]*version[[:space:]]*=/ {
        if (match($0, /"[^"]*"/)) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
    }
' "$ROOT/Cargo.toml" 2>/dev/null || true)"
[[ -n "$VERSION" ]] || VERSION="0.1.0"

T_START="$(secs)"

cat <<EOF

${BOLD}Syrinx $VERSION — macOS release bundle${RESET}
  Interpreter   CPython $PY_FULL  ($PY_SRC)
  Pins from     engine/.venv  (read-only)
  Signing       $SIGN_ID
  Output        dist/$APP_NAME.app$( ((DO_DMG)) && printf ' + dist/%s-%s.dmg' "$APP_NAME" "$VERSION" || true )
EOF

# --------------------------------------------------------------------------
# 1. build the Rust app
#
# Never a bare `cargo build`: dictate/ carries ungated gtk4 deps that don't
# resolve on macOS, so the workspace only builds member-by-member.

if (( DO_BUILD )); then
    log "Building the release app (cargo build --release -p syrinx-app -p syrinx-shared)"
    cargo build --release -p syrinx-app -p syrinx-shared
else
    log "Reusing target/release/$APP_BIN (--skip-build)"
fi
[[ -f "$ROOT/target/release/$APP_BIN" ]] || die "target/release/$APP_BIN missing"
ok "target/release/$APP_BIN"

# --------------------------------------------------------------------------
# 2. staging
#
# Everything is assembled under dist/.build so a failed run never leaves a
# half-written Syrinx.app where a DMG or an install could pick it up, and so the
# final move is a same-volume rename rather than a 5 GB copy. The stage is NOT
# trap-deleted: a 20-minute build that fails at the signing step is worth
# inspecting, and the next run clears it.

DIST="$ROOT/dist"
STAGE="$DIST/.build"
rm -rf "$STAGE"
mkdir -p "$STAGE"

APP="$STAGE/$APP_NAME.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
ENGINE="$CONTENTS/$ENGINE_REL"
PYPREFIX="$ENGINE/.venv"
PYBIN="$PYPREFIX/bin/python$PYVER"
TOOLS="$ENGINE/tools"

mkdir -p "$MACOS" "$RESOURCES"

install -m 755 "$ROOT/target/release/$APP_BIN" "$MACOS/$APP_BIN"
ok "Contents/MacOS/$APP_BIN"

# --------------------------------------------------------------------------
# 3. embed the interpreter

log "Embedding CPython $PY_FULL"
mkdir -p "$ENGINE"
# ditto rather than cp -R: it carries permissions, symlinks (bin/python3 ->
# python3.12) and resource forks across verbatim, which is what "a copy of a
# relocatable distribution" has to mean.
ditto "$PY_SRC" "$PYPREFIX"

# uv stamps its managed installs with PEP 668's EXTERNALLY-MANAGED marker, which
# makes pip refuse to install into them ("This environment is externally managed
# ... managed by uv"). That claim is true of the original and false of this copy:
# uv owns the distribution under ~/.local/share/uv, not the private duplicate
# inside the bundle, and this prefix exists precisely to be installed into. The
# alternative, --break-system-packages, would silence the check by asserting the
# opposite of what it means; deleting the marker states the actual fact.
rm -f "$PYPREFIX/lib/python$PYVER/EXTERNALLY-MANAGED"

ok "$ENGINE_REL/.venv  ($(human "$PYPREFIX"))"

# Everything the embedded python runs from here on: no user site-packages, no
# stray PYTHONPATH inherited from the build shell. Without PYTHONNOUSERSITE a
# ~/Library/Python/3.12/lib/python/site-packages on the build machine can shadow
# a bundled package during the install and the proof steps, and the bundle would
# be "self-contained" only on this Mac.
export PYTHONNOUSERSITE=1
unset PYTHONPATH PYTHONHOME || true

PIP=("$PYBIN" -m pip --disable-pip-version-check --no-input)

# --------------------------------------------------------------------------
# 4. the engine wheel, built out-of-tree
#
# `pip wheel` on a local directory builds in place these days, which would drop
# build/ and *.egg-info into engine/ — a source tree this script has no business
# writing to (and which another agent may be editing while this runs). So the
# package is copied out first, minus the 1.4 GB venv and every cache, and the
# wheel is built from the copy. Windows does the same thing for the same reason.

log "Building the syrinx-engine wheel"
SRC="$STAGE/engine-src"
rsync -a \
    --exclude '.venv' --exclude '.venv-*' \
    --exclude '__pycache__' --exclude '*.egg-info' \
    --exclude '.pytest_cache' --exclude '.ruff_cache' \
    "$ROOT/engine/" "$SRC/"
"${PIP[@]}" wheel --no-deps --wheel-dir "$STAGE/wheels" "$SRC" \
    2>&1 | sed 's/^/  /'
WHEEL="$(find "$STAGE/wheels" -maxdepth 1 -name 'syrinx_engine-*.whl' | head -1)"
[[ -n "$WHEEL" ]] || die "pip wheel produced no syrinx_engine wheel"
ok "$(basename "$WHEEL")"

# --------------------------------------------------------------------------
# 5. the pins
#
# `pip freeze` from the dev venv IS the specification: it is the exact closure
# that has been run, profiled and WER-tested on this machine (torch 2.13 on MPS,
# TADA in bf16, the whole 8/3 campaign). Resolving fresh from pyproject.toml
# would produce *a* working environment; freezing produces *the* one.
#
# Four classes of line come out, and they go to three different places:
#
#   * `-e git+ssh://…#egg=syrinx_engine` — the editable engine itself. Dropped:
#     it is installed from the wheel above, and an editable line in a
#     requirements file would point the bundle back at the checkout.
#   * test and lint tooling — pytest, pytest-cov, pluggy, iniconfig, coverage,
#     ruff. Dropped: ~27 MB of things a shipped app cannot run.
#   * `name @ https://…` — direct URLs. These CANNOT go in the constraints file
#     (pip rejects a URL there outright) but they must not be dropped either, so
#     they go to a second file installed with --no-deps. There is exactly one
#     today and it earns the whole mechanism: en_core_web_sm, spaCy's English
#     pipeline. It looks like a stray and is the opposite of one —
#     misaki/en.py:500 reads
#
#         if not spacy.util.is_package(name): spacy.cli.download(name)
#
#     so an engine that boots without it *pip installs it at warmup*, into its
#     own site-packages. Inside a signed bundle that is a new file under
#     Contents/, which breaks the resource seal (measured: `codesign --verify`
#     went from clean to "file added: …/en_core_web_sm/meta.json" after one
#     launch) and needs the network on a first run that is otherwise offline.
#     Shipping the 15 MB model is the fix; there is no engine-side switch.
#   * everything else — kept verbatim, including the ones that look like strays
#     and are not. gradio really is a qwen-tts dependency. spaCy really is
#     pulled by `misaki[en]`, which engine/pyproject.toml asks for by name.

log "Freezing the proven dependency set"
PINS="$STAGE/constraints.txt"
PIN_URLS="$STAGE/constraints-urls.txt"
FREEZE="$STAGE/freeze.txt"
"$DEV_PY" -m pip freeze --disable-pip-version-check 2>/dev/null > "$FREEZE"
grep -v '^-e ' "$FREEZE" \
  | grep -v ' @ ' \
  | grep -viE '^(pytest|pytest-cov|pluggy|iniconfig|coverage|ruff)==' \
  | grep -viE '^syrinx[-_]engine==' \
  > "$PINS"
# https only: a `file://` line would be a local path, which is exactly the kind
# of build-machine reference this bundle must not carry.
grep -E ' @ https://' "$FREEZE" > "$PIN_URLS" || true
PIN_COUNT="$(wc -l < "$PINS" | tr -d ' ')"
URL_COUNT="$(wc -l < "$PIN_URLS" | tr -d ' ')"
[[ "$PIN_COUNT" -gt 50 ]] || die "only $PIN_COUNT pins came out of engine/.venv — that venv looks empty"
ok "$PIN_COUNT pins + $URL_COUNT url-installed ($(grep -m1 '^torch==' "$PINS" || echo 'no torch?!'))"

# --------------------------------------------------------------------------
# 6. install the engine into the embedded interpreter
#
# Two passes, and the split is not redundant:
#
#   A. `pip install -c pins <wheel>[qwen]` — a real resolve, so the wheel's own
#      metadata is *proven satisfiable* at the pinned versions rather than
#      assumed. If a dependency edge is added to pyproject.toml that the dev
#      venv never had, this is the step that says so.
#   B. `pip install -c pins --no-deps -r pins` — the sweep. Everything in the
#      freeze that pass A did not already pull, installed at exactly its pinned
#      version with no resolution at all. --no-deps is what makes this possible:
#      hume-tada pins torch<2.8 and requires torchvision + descript-audio-codec
#      (dac_shim.py stands in for the latter, torchvision is never installed),
#      so a resolving install of it fails by construction — which is precisely
#      why the dev venv installed it with --no-deps too. Pass B reproduces that
#      decision for every such package at once instead of enumerating them.
#   C. the URL-installed lines, same treatment — separate only because pip will
#      not read a URL out of a -c file.
#
# Then the set is checked against the freeze, both directions, because "the
# bundle has the environment we tested" is a claim this script should be able to
# fail on rather than hope about.

log "Installing the engine (this pulls ~1.5 GB of wheels — torch alone is 515 MB)"
T_PIP="$(secs)"
"${PIP[@]}" install --constraint "$PINS" "${WHEEL}[qwen]" 2>&1 | sed 's/^/  /'
"${PIP[@]}" install --constraint "$PINS" --no-deps --requirement "$PINS" 2>&1 | sed 's/^/  /'
if [[ -s "$PIN_URLS" ]]; then
    "${PIP[@]}" install --constraint "$PINS" --no-deps --requirement "$PIN_URLS" 2>&1 | sed 's/^/  /'
fi
ok "pip finished in $(( $(secs) - T_PIP ))s"

log "Reconciling the bundle against the freeze"
"$PYBIN" - "$PINS" "$PIN_URLS" <<'PY' || die "the bundle's package set does not match engine/.venv"
import sys
from importlib.metadata import distributions
from packaging.utils import canonicalize_name

want = {}
for line in open(sys.argv[1]):
    line = line.strip()
    if not line or line.startswith("#"):
        continue
    name, _, ver = line.partition("==")
    want[canonicalize_name(name)] = ver

# The URL-installed lines carry no version to compare against — `name @ url`
# pins by artifact hash instead — so they are presence-checked only (None).
for line in open(sys.argv[2]):
    line = line.strip()
    if line:
        want[canonicalize_name(line.split(" @ ")[0])] = None

have = {}
for d in distributions():
    n = d.metadata["Name"]
    if n:
        have[canonicalize_name(n)] = d.version

# syrinx-engine is installed from the wheel, so it is expected to be present and
# absent from the pins; pip/setuptools/wheel ride in with the distribution.
ignore = {"syrinx-engine", "pip", "setuptools", "wheel"}
missing = sorted(k for k in want if k not in have)
extra = sorted(k for k in have if k not in want and k not in ignore)
drift = sorted((k, want[k], have[k]) for k in want
               if k in have and want[k] is not None and want[k] != have[k])

for k in extra:
    print(f"  extra (resolver added, not in the dev venv): {k}=={have[k]}")
for k, w, h in drift:
    print(f"  DRIFT: {k} pinned {w} but installed {h}")
if missing:
    print("  MISSING: " + ", ".join(missing))
print(f"  {len(have)} distributions installed, {len(want)} pinned")
sys.exit(1 if (missing or drift) else 0)
PY
ok "package set matches"

# `pip check` is informational here, not a gate: hume-tada's unmet torchvision /
# descript-audio-codec requirements are the *intended* state (dac_shim.py
# replaces DAC and nothing imports torchvision), and the dev venv reports them
# too. Printing it keeps the two comparable at a glance.
"${PIP[@]}" check 2>&1 | sed 's/^/  pip check: /' || true

# --------------------------------------------------------------------------
# 7. the shim, and pruning bin/
#
# Everything pip put in bin/ carries an absolute `#!` pointing at the staging
# path — dead on the target and, worse, a build-machine path baked into a
# shipped artifact. None of those scripts is ever invoked: the engine calls no
# console entry points, and vcsetup.py drives the setup scripts of step 8
# through an explicit interpreter path, never a name. So bin/ is reduced to the
# interpreter, its aliases, and our shim, which kills the whole class of stale
# shebangs rather than rewriting each one into a polyglot.
#
# `pip` itself is kept as a module (python -m pip still works) but its script
# goes with the rest — nothing in the bundle shells out to `pip`.

log "Installing the engine shim"
find "$PYPREFIX/bin" -maxdepth 1 -type f ! -name "python$PYVER" -delete
find "$PYPREFIX/bin" -maxdepth 1 -type l ! -name 'python3' ! -name 'python' -delete

cat > "$PYPREFIX/bin/syrinx-engine" <<SHIM_EOF
#!/bin/sh
#
# Syrinx engine entry point — GENERATED by scripts/build-macos.sh.
#
# This replaces the console script pip would have written, whose \`#!\` line is an
# absolute path to the interpreter that existed at *build* time. \`dirname "\$0"\`
# is the same answer computed at *run* time, which is the only version of it
# that survives being dragged to /Applications.
#
# \`-m syrinx_engine\` runs engine/syrinx_engine/__main__.py:main — byte-identical
# to what [project.scripts] syrinx-engine points at. The app spawns this with no
# arguments (docs/RPC-PROTOCOL.md §13.2); "\$@" is here for hand-running it.
#
# The three seal guards are repeated from Contents/MacOS/Syrinx deliberately.
# The launcher exports them and this shim inherits them, so under the app they
# are redundant — but that makes the bundle's protection POSITIONAL, and this
# file is a documented, executable entry point (§13.2 probe #1 names it). Set
# them here too and anything that reaches the engine without descending from
# the launcher — a Terminal debug run, a LaunchAgent, a future helper — still
# cannot write a .pyc into site-packages or a numba cache beside librosa's .py,
# both of which are inside the signature. They are idempotent; see the launcher
# for what each one is actually defending against.
PYTHONDONTWRITEBYTECODE=1
NUMBA_CACHE_DIR="\${NUMBA_CACHE_DIR:-\$HOME/Library/Caches/syrinx/numba}"
PYTHONNOUSERSITE=1
export PYTHONDONTWRITEBYTECODE NUMBA_CACHE_DIR PYTHONNOUSERSITE

exec "\$(dirname -- "\$0")/python$PYVER" -m syrinx_engine "\$@"
SHIM_EOF
chmod 755 "$PYPREFIX/bin/syrinx-engine"
ok "$ENGINE_REL/.venv/bin/syrinx-engine  ->  python$PYVER -m syrinx_engine"

# Trim what a shipped interpreter cannot use. Each of these is unreachable
# rather than merely unlikely: no Tk anywhere in the dependency set, no IDLE, no
# 2to3, no C extensions compiled on the target (so no headers), and a static
# libpython for embedders we are not.
log "Pruning the interpreter"
BEFORE_PRUNE="$(du -sk "$PYPREFIX" | awk '{print $1}')"
#
# libpython3.12.dylib is on the list because python-build-standalone's
# bin/python3.12 links the interpreter *statically* (verified: otool -L names
# only system libraries), and no extension module in the whole dependency set
# links it either — macOS extensions resolve CPython symbols out of the host
# process, not out of a shared libpython. It exists for embedders, we are not
# one, and it is 18 MB carrying a path into uv's python store.
rm -rf \
    "$PYPREFIX/lib/libpython$PYVER.dylib" \
    "$PYPREFIX/include" "$PYPREFIX/share" "$PYPREFIX/lib/pkgconfig" \
    "$PYPREFIX/lib/itcl"* "$PYPREFIX/lib/tcl"* "$PYPREFIX/lib/tk"* \
    "$PYPREFIX/lib/thread"* "$PYPREFIX/lib/libtcl"* "$PYPREFIX/lib/libtk"* \
    "$PYPREFIX/lib/python$PYVER/idlelib" \
    "$PYPREFIX/lib/python$PYVER/tkinter" \
    "$PYPREFIX/lib/python$PYVER/lib2to3" \
    "$PYPREFIX/lib/python$PYVER/pydoc_data" \
    "$PYPREFIX/lib/python$PYVER/test" \
    "$PYPREFIX/lib/python$PYVER/lib-dynload/_tkinter."*.so
find "$PYPREFIX/lib/python$PYVER/config-"* -name 'libpython*.a' -delete 2>/dev/null || true

# PEP 610 provenance metadata: pip records the file:// URL the wheel came from,
# which is a path inside this build's staging dir. Nothing reads it (it exists
# so `pip freeze` can reproduce an editable/VCS install) and it is the only
# structured place the build machine's layout is written down.
rm -f "$PYPREFIX/lib/python$PYVER/site-packages/syrinx_engine-"*.dist-info/direct_url.json

AFTER_PRUNE="$(du -sk "$PYPREFIX" | awk '{print $1}')"
ok "freed $(( (BEFORE_PRUNE - AFTER_PRUNE) / 1024 )) MB"

# Bytecode, and it is worth being fussy about all three flags.
#
# It has to happen at *build* time at all because a .pyc written at run time is
# a new file inside a signed bundle, which breaks its resource seal; the
# launcher sets PYTHONDONTWRITEBYTECODE=1 so nothing tries.
#
#   -f, -s, -p   A .pyc records the absolute path its source had at compile
#                time, in co_filename, and that is the path tracebacks print
#                and linecache reads source from. pip compiled everything it
#                installed against the staging path — which lives inside this
#                checkout — so without this the shipped artifact would quote a
#                developer's home directory in 17 thousand files. -f forces a
#                recompile of all of them, and -s/-p rewrite the recorded path
#                to /Applications/Syrinx.app/…, the canonical install location.
#                (It is a guess for a bundle run from anywhere else, but it is a
#                guess about *this app* rather than a fact about this machine.)
#   unchecked-hash
#                The default, timestamp, makes every import compare the .pyc's
#                recorded mtime against the .py's. Copying a bundle rewrites
#                mtimes, so a timestamp .pyc in a relocatable bundle is one
#                `ditto` away from being considered stale — at which point
#                Python wants to rewrite it and cannot. Hash-based .pyc have no
#                such dependency, and *unchecked* skips even hashing the source
#                on import: correct here precisely because the source cannot
#                change without breaking the signature first.
#
# Failures are ignored: a few packages ship Python 2 or deliberately-broken
# sample files that will not compile, and never needed to.
log "Precompiling bytecode"
"$PYBIN" -m compileall -q -f -j 0 --invalidation-mode unchecked-hash \
    -s "$APP" -p "/Applications/$APP_NAME.app" \
    "$PYPREFIX/lib/python$PYVER" >/dev/null 2>&1 || true
ok "compileall rewrote every .pyc against /Applications/$APP_NAME.app"

# --------------------------------------------------------------------------
# 8. the on-demand VC engine installers
#
# Seed-VC (GPL-3.0), Vevo's Amphion checkpoints (CC-BY-NC) and LuxTTS (git-only)
# are never bundled — they are installed per-user, on request, by these three
# scripts, which the Models tab runs for the user through vcsetup.py. Phase 2
# deliberately left them out: on POSIX the venv went BESIDE the script, and
# beside a script that lives at Contents/Resources/engine is *inside the
# signature*. Phase 3 moved the venv (and the child's cwd) to the data dir on
# every platform, so shipping them is now the honest thing to do — without them
# script_path() returns None and three engines are permanently unavailable in
# the packaged app.
#
# Contents/Resources/engine is exactly five levels above the package
# (.venv/lib/python3.12/site-packages/syrinx_engine), which is exactly what
# script_path()'s ancestor walk reaches; engine/tests/test_vcsetup.py pins that
# layout so a future move fails there rather than in the field.
#
# What the scripts shell out to, and what a bare Mac has:
#   bash    /bin/bash — ships with macOS. (3.2, and these scripts are 3.2-clean.)
#   python  never by name: vcsetup resolves an interpreter and passes the full
#           path in SYRINX_<X>_PYTHON. In the bundle that is the app's own
#           embedded CPython 3.12, so a Mac with no Python at all still works.
#   git     needed by setup-vevo (clones Amphion) and setup-luxtts (two
#           git+https pins). /usr/bin/git is a Command Line Tools stub on a
#           bare Mac; vcsetup.ensure_git() pre-flights it and fails the install
#           in two seconds with "run xcode-select --install" rather than
#           twenty minutes into a torch download. Deliberately NOT vendored:
#           git is ~50 MB with its own template/exec closure, Apple ships a
#           real one for free, and two gits on a box is a support problem.
#   nvidia-smi  probed with `command -v`, absent on every Mac, so the scripts
#           take their CPU/MPS branch. No macOS-specific edit was needed.
#   network  yes — several GB of wheels. These installs are online by
#           definition; the bundle's own first launch is not.
#
# Copied with install -m 755 rather than rsync'd: three named files, mode set
# explicitly (a .sh sourced by `bash <path>` does not need +x, but a user who
# finds them in the bundle and runs one directly should not have to know that).

log "Shipping the on-demand setup scripts"
for stem in seedvc vevo luxtts; do
    src="$ROOT/engine/setup-$stem.sh"
    [[ -f "$src" ]] || die "engine/setup-$stem.sh is missing — the packaged app
     would report that this build shipped without the setup scripts, and
     Seed-VC / Vevo / LuxTTS would be permanently unavailable in it."
    install -m 755 "$src" "$ENGINE/setup-$stem.sh"
done
ok "$ENGINE_REL/setup-{seedvc,vevo,luxtts}.sh"

# Prove the ancestor walk actually reaches them from where the package landed,
# using the bundle's own interpreter and its own copy of vcsetup — the same
# code path the Models tab will take. A build that shipped the scripts into a
# directory script_path() cannot see would look fine and be useless.
"$PYBIN" - "$ENGINE" <<'PY' || die "the shipped setup scripts are not where script_path() looks"
import sys
from pathlib import Path

from syrinx_engine import vcsetup

engine = Path(sys.argv[1]).resolve()
for sid in vcsetup.SETUP_IDS:
    found = vcsetup.script_path(sid)
    if found is None:
        sys.exit(f"  script_path({sid!r}) is None — the walk does not reach {engine}")
    if found.parent != engine:
        sys.exit(f"  script_path({sid!r}) found {found}, expected it under {engine}")
print(f"  script_path() resolves all {len(vcsetup.SETUP_IDS)} setups inside the bundle")
PY
ok "script_path() sees them from the embedded package"

# --------------------------------------------------------------------------
# 9. vendor sox
#
# The qwen backend imports the `sox` Python package, which shells out to the
# `sox` BINARY at import time. The dev launcher solves that by putting
# /opt/homebrew/bin on PATH; a shipped bundle cannot — a user without Homebrew,
# or with a different Homebrew prefix, or with a `brew uninstall sox` in their
# past, would get an engine that dies at import with no useful message.
#
# So sox and its entire non-system dylib closure are copied in and re-pointed at
# each other with install_name_tool: the executable's references become
# @executable_path/../lib/<name>, and each library's own install name and
# references become @loader_path/<name>. Both are resolved by dyld relative to
# the files themselves, so the closure survives the bundle being moved and needs
# no DYLD_LIBRARY_PATH (which the hardened runtime would ignore anyway without
# an entitlement we deliberately do not grant).
#
# The destination filename is the basename of each library's own LC_ID_DYLIB,
# not of the file on disk. Homebrew ships libogg as libogg.0.8.6.dylib but every
# reference to it says libogg.0.dylib; matching the install name is what makes
# the rewrite a pure basename substitution.
#
# install_name_tool invalidates any signature it touches, so this must finish
# before the signing walk. It does — signing is the last thing this script does
# to the bundle. But "invalidates" is stronger on Apple silicon than it sounds:
# arm64 refuses to execute or dlopen a Mach-O whose signature does not match its
# contents, and the failure mode is SIGKILL from the kernel with no message. So
# the fourteen rewritten files get an immediate ad-hoc repair signature below,
# purely so they are loadable for the env -i proof that follows; step 14
# replaces it with the real one.

log "Vendoring sox"
SOX_SRC="$(command -v sox || true)"
[[ -n "$SOX_SRC" ]] || die \
    "sox is not on PATH. The qwen TTS backend imports it at engine start, so a
     bundle without it boots into a broken backend. Install it and re-run:
       brew install sox"
mkdir -p "$TOOLS/bin" "$TOOLS/lib"

"$PYBIN" - "$SOX_SRC" "$TOOLS" <<'PY' || die "sox vendoring failed"
import os, shutil, subprocess, sys

sox_src, tools = os.path.realpath(sys.argv[1]), sys.argv[2]
SYSTEM = ("/usr/lib/", "/System/")


def install_name(path):
    """LC_ID_DYLIB of *path*, or the path itself for a non-library."""
    out = subprocess.run(["otool", "-D", path], capture_output=True, text=True).stdout
    lines = [l.strip() for l in out.splitlines()[1:] if l.strip()]
    return lines[0] if lines else path


def deps(path):
    """Non-system dylib references of *path*, as written in the load commands.

    otool -L prints a library's own LC_ID_DYLIB as its first entry, which is not
    a dependency; drop it. @rpath/@loader_path references are dropped too — none
    of these libraries has any, and a silent mis-rewrite would be worse than a
    loud failure at the env -i check that follows.
    """
    own = install_name(path)
    out = subprocess.run(["otool", "-L", path], capture_output=True, text=True).stdout
    refs = [ln.split()[0] for ln in out.splitlines()[1:] if ln.strip()]
    return [r for r in refs
            if r != own and not r.startswith(SYSTEM) and not r.startswith("@")]


# Walk the closure off the *originals*, before anything is copied or rewritten,
# so every load command read here is the pristine Homebrew one. `ref` is what
# the load command says; `real` is where that resolves on this machine
# (Homebrew's opt/ symlinks point into Cellar/). The destination filename is the
# basename of the library's own install name, not of the file on disk.
libs = {}          # dest basename -> source path on disk
plan = {}          # source path -> [(ref, dest basename), …]
queue, seen = [sox_src], set()
while queue:
    cur = queue.pop()
    if cur in seen:
        continue
    seen.add(cur)
    edges = []
    for ref in deps(cur):
        real = os.path.realpath(ref)
        if not os.path.isfile(real):
            sys.exit(f"cannot resolve {ref} (referenced by {cur})")
        dest = os.path.basename(install_name(real))
        libs.setdefault(dest, real)
        edges.append((ref, dest))
        queue.append(real)
    plan[cur] = edges

sox_dst = os.path.join(tools, "bin", "sox")
copied = {sox_src: sox_dst}
shutil.copy2(sox_src, sox_dst)
for dest, real in libs.items():
    copied[real] = os.path.join(tools, "lib", dest)
    shutil.copy2(real, copied[real])
for p in copied.values():
    os.chmod(p, 0o755)

# One install_name_tool call per file, built entirely from the pre-copy plan.
for src, edges in plan.items():
    dst = copied[src]
    prefix = "@executable_path/../lib/" if dst == sox_dst else "@loader_path/"
    args = ["install_name_tool"]
    if dst != sox_dst:
        args += ["-id", "@loader_path/" + os.path.basename(dst)]
    for ref, dest in edges:
        args += ["-change", ref, prefix + dest]
    subprocess.run(args + [dst], check=True, capture_output=True)

print(f"  sox + {len(libs)} dylibs: " + ", ".join(sorted(libs)))
PY
codesign --force -s - "$TOOLS/bin/sox" "$TOOLS/lib/"*.dylib 2>/dev/null \
    || die "could not repair the vendored sox signatures after install_name_tool"
ok "$ENGINE_REL/tools/bin/sox  ($(human "$TOOLS"))"

# The proof that the closure is complete, run now rather than after signing so a
# missing library is a build failure instead of a field report. `env -i` strips
# the environment to nothing: no PATH, no DYLD_*, no Homebrew. If sox answers
# here it answers on a Mac that has never had Homebrew installed.
env -i "$TOOLS/bin/sox" --version >/dev/null 2>&1 \
    || die "the vendored sox does not run under an empty environment — the dylib closure is incomplete"
ok "sox runs under env -i"

# --------------------------------------------------------------------------
# 10. icon — packaging/syrinx.svg -> Resources/syrinx.icns, best effort
#    (identical ladder to scripts/install-macos-dev.sh: rsvg-convert, qlmanage, sips)

ICON_OK=0
ICON_VIA=""

render_master() {   # $1 = destination 1024x1024 png
    local out="$1" svg="$ROOT/packaging/syrinx.svg" tmp="$STAGE/iconsrc"
    mkdir -p "$tmp"
    if command -v rsvg-convert >/dev/null; then
        if rsvg-convert -w 1024 -h 1024 -o "$out" "$svg" 2>/dev/null && [[ -s "$out" ]]; then
            ICON_VIA="rsvg-convert"; return 0
        fi
    fi
    if command -v qlmanage >/dev/null; then
        if qlmanage -t -s 1024 -o "$tmp" "$svg" >/dev/null 2>&1 \
           && [[ -s "$tmp/syrinx.svg.png" ]]; then
            mv "$tmp/syrinx.svg.png" "$out"
            ICON_VIA="qlmanage"; return 0
        fi
    fi
    if command -v sips >/dev/null; then
        if sips -s format png --resampleHeightWidth 1024 1024 "$svg" --out "$out" \
             >/dev/null 2>&1 && [[ -s "$out" ]]; then
            ICON_VIA="sips"; return 0
        fi
    fi
    return 1
}

log "Rendering the icon"
MASTER="$STAGE/syrinx-1024.png"
if [[ -f "$ROOT/packaging/syrinx.svg" ]] && render_master "$MASTER"; then
    ICONSET="$STAGE/syrinx.iconset"
    mkdir -p "$ICONSET"
    for spec in 16:icon_16x16 32:icon_16x16@2x 32:icon_32x32 64:icon_32x32@2x \
                128:icon_128x128 256:icon_128x128@2x 256:icon_256x256 \
                512:icon_256x256@2x 512:icon_512x512 1024:icon_512x512@2x; do
        px="${spec%%:*}"; name="${spec#*:}"
        sips -s format png -z "$px" "$px" "$MASTER" --out "$ICONSET/$name.png" \
            >/dev/null 2>&1 || die "sips failed resizing the icon to ${px}px"
    done
    if iconutil -c icns "$ICONSET" -o "$RESOURCES/syrinx.icns" 2>/dev/null; then
        ICON_OK=1
        ok "Contents/Resources/syrinx.icns (rendered with $ICON_VIA)"
    else
        hint "iconutil failed — building without an icon"
    fi
else
    hint "no SVG renderer worked (tried rsvg-convert, qlmanage, sips) — building without an icon"
fi

# --------------------------------------------------------------------------
# 11. the launcher
#
# Unlike the dev bundle's, nothing here is substituted at build time: every path
# is derived from $0 when it runs. That is the difference between "a bundle that
# points at this checkout" and "a bundle".

log "Writing the launcher"
cat > "$MACOS/$LAUNCHER" <<LAUNCHER_EOF
#!/bin/sh
#
# Syrinx launcher — GENERATED by scripts/build-macos.sh. Don't edit it in place:
# it is the bundle's CFBundleExecutable, so it is hashed straight into the code
# directory and an edit stops the signature verifying. Rebuild instead.
#
# Every path below is computed from \$0. Drag Syrinx.app anywhere — /Applications,
# a Downloads folder, a mounted DMG — and it still finds its own engine.

here=\$(cd -- "\$(dirname -- "\$0")" && pwd)          # Contents/MacOS
res=\$(cd -- "\$here/../Resources" && pwd)

# Probe #1 of the engine resolution order (docs/RPC-PROTOCOL.md §13.2). It is
# the only probe that can fire for this layout: Finder starts us with cwd=/, and
# the app-exe-ancestor probe walks up from Contents/MacOS, which never passes
# through Contents/Resources.
SYRINX_ENGINE_CMD="\$res/engine/.venv/bin/syrinx-engine"
export SYRINX_ENGINE_CMD

# The vendored sox, and deliberately NOT the Homebrew prefix that the dev
# launcher prepends. A shipped app that silently preferred the user's brewed sox
# over its own would work on the build machine and fail on every Mac without it,
# which is the exact bug the vendoring exists to prevent. (The prefix is not
# even named here: a shipped file mentioning it, in a comment or otherwise,
# makes every "does this bundle reach outside itself" grep a false positive.)
PATH="\$res/engine/tools/bin:\$PATH"
export PATH

# The interpreter, its site-packages and its bytecode all live inside a *signed*
# bundle. A .pyc written at run time is a new file inside that bundle, which
# breaks its resource seal — so bytecode is precompiled at build time and
# writing is switched off here. (Reading existing .pyc is unaffected.)
PYTHONDONTWRITEBYTECODE=1
export PYTHONDONTWRITEBYTECODE

# Same reasoning, different cache: librosa compiles through numba with
# cache=True, and numba's default cache location is beside the .py it compiled —
# i.e. inside the bundle. Point it at the per-user cache dir the engine already
# uses for worker logs (paths.py: platformdirs user_cache_dir).
NUMBA_CACHE_DIR="\$HOME/Library/Caches/syrinx/numba"
export NUMBA_CACHE_DIR

# A ~/Library/Python/3.12 site-packages on the user's machine must not be able
# to shadow a bundled package. The bundle ships the exact versions it was tested
# with; user site is the one path that could substitute another.
PYTHONNOUSERSITE=1
export PYTHONNOUSERSITE

# exec, not call: the real app takes over this pid, so LaunchServices' quit and
# any SIGTERM land on the process that can act on them.
exec "\$here/$APP_BIN" "\$@"
LAUNCHER_EOF
chmod 755 "$MACOS/$LAUNCHER"
ok "Contents/MacOS/$LAUNCHER"

# --------------------------------------------------------------------------
# 12. Info.plist
#
# The usage strings and the bundle ID match scripts/install-macos-dev.sh exactly.
# The ID especially: same ID + same certificate = same designated requirement =
# the TCC grants the user already answered for the dev bundle apply to this one
# untouched. Changing it here would silently cost them three prompts.

if (( ICON_OK )); then
    ICON_ENTRY=$'\t<key>CFBundleIconFile</key>\n\t<string>syrinx</string>'
else
    ICON_ENTRY=""
fi

cat > "$CONTENTS/Info.plist" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleIdentifier</key>
	<string>$BUNDLE_ID</string>
	<key>CFBundleName</key>
	<string>$APP_NAME</string>
	<key>CFBundleDisplayName</key>
	<string>$APP_NAME</string>
	<key>CFBundleExecutable</key>
	<string>$LAUNCHER</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>$ICON_ENTRY
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>Syrinx uses the microphone to test your input device and to record the reference clips it learns a cloned voice from. Audio is processed on this Mac and never leaves it.</string>
	<key>NSAudioCaptureUsageDescription</key>
	<string>Syrinx records the audio your Mac is playing so you can transcribe it or clone a voice from it. Audio is processed on this Mac and never leaves it.</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.productivity</string>
</dict>
</plist>
PLIST_EOF
plutil -lint "$CONTENTS/Info.plist" >/dev/null || die "generated Info.plist is malformed"
ok "Contents/Info.plist"

# --------------------------------------------------------------------------
# 13. move into place
#
# Same volume, so this is a rename. Every write to the bundle is now finished —
# which is the precondition for signing: codesign seals what is on disk, and
# anything added afterwards is a seal violation.

log "Moving into dist/"
rm -rf "$DIST/$APP_NAME.app"
mv "$APP" "$DIST/$APP_NAME.app"
APP="$DIST/$APP_NAME.app"
CONTENTS="$APP/Contents"
PYBIN="$CONTENTS/$ENGINE_REL/.venv/bin/python$PYVER"
SOXBIN="$CONTENTS/$ENGINE_REL/tools/bin/sox"
APP_SIZE="$(human "$APP")"
ok "dist/$APP_NAME.app  ($APP_SIZE)"

# --------------------------------------------------------------------------
# 14. sign, inside out
#
# Order is structural, not stylistic. The bundle's signature seals a record of
# every nested binary's cdhash, so each of them has to be final first; and
# syrinx-app has to be final before the bundle is signed around it. No --deep
# anywhere: --deep re-signs whatever it finds with whatever flags it was handed,
# which is exactly wrong when the interpreter needs three entitlements the Rust
# binary must not have. Apple has called it unsuitable for shipping for years.
#
# Three tiers, and the difference between them is what an entitlement *is*:
#
#   libraries (.so/.dylib, several hundred) — signed with the identity and
#     nothing else. Entitlements are a property of a main executable; a library
#     inherits the entitlements of whatever process loads it and its own would
#     be ignored. They are signed only so the bundle's resource seal holds and
#     so a Developer ID build has a uniform Team ID to validate against.
#
#   executables (python3.12, sox) — --options runtime plus
#     packaging/entitlements-engine.plist. See that file for why each of the
#     three flags is there; the short version is dlopen, MCJIT and the mic.
#
#   syrinx-app — --options runtime plus packaging/entitlements-app.plist, and
#     -i to pin the identifier to the bundle ID so its DR and the bundle's DR
#     say the same thing.
#
# Scripts (the launcher, the shim) are not signed and cannot be: the hardened
# runtime is a Mach-O load-command flag. They are not left unprotected by that —
# the launcher is CFBundleExecutable so it is hashed into the code directory
# itself, and the shim is sealed as a resource.

if [[ "$SIGN_KIND" == identity ]]; then
    log "Signing as \"$SIGN_ID\""
    T_SIGN="$(secs)"

    # Find the Mach-Os by magic number, not by extension: pip wheels ship
    # extensionless helpers and .dylib files under names like `.dylibs/`, and a
    # find -name sweep would miss them and leave holes in the seal. The filetype
    # word (offset 12) separates MH_EXECUTE from everything else, which is the
    # only distinction the tiers above need. Symlinks are skipped — codesign
    # follows them and would sign the same file twice under two names.
    MACHO_LIB="$STAGE/macho-lib.txt"
    MACHO_EXE="$STAGE/macho-exe.txt"
    "$PYBIN" - "$APP" "$MACHO_LIB" "$MACHO_EXE" <<'PY'
import os, struct, sys

root, lib_out, exe_out = sys.argv[1:4]
THIN = {b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe"}   # MH_MAGIC_64 / MH_MAGIC, LE
FAT = {b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca"}
MH_EXECUTE = 2

libs, exes = [], []
for dirpath, _, names in os.walk(root):
    for n in names:
        p = os.path.join(dirpath, n)
        if os.path.islink(p):
            continue
        try:
            with open(p, "rb") as fh:
                head = fh.read(16)
        except OSError:
            continue
        if len(head) < 16:
            continue
        magic = head[:4]
        if magic in FAT:
            # Universal binaries: we ship none today (every wheel on this
            # platform is arm64-thin), so treat them as libraries rather than
            # parsing the fat header. The two executables that matter are named
            # explicitly by the caller anyway.
            libs.append(p)
        elif magic in THIN:
            (exes if struct.unpack("<I", head[12:16])[0] == MH_EXECUTE else libs).append(p)

open(lib_out, "w").write("\n".join(libs) + ("\n" if libs else ""))
open(exe_out, "w").write("\n".join(exes) + ("\n" if exes else ""))
print(f"  {len(libs)} libraries, {len(exes)} executables")
PY

    N_LIB="$(wc -l < "$MACHO_LIB" | tr -d ' ')"
    N_EXE="$(wc -l < "$MACHO_EXE" | tr -d ' ')"

    # Batched. codesign takes many paths per invocation, and the difference at
    # this scale is minutes: one process per file otherwise spends most of its
    # life opening the keychain. -n 64 keeps the argv comfortably under ARG_MAX.
    #
    # Order *within* a batch does not matter — plain Mach-Os do not seal each
    # other, only bundles seal their nested code — but libraries before
    # executables before syrinx-app before the bundle does.
    #
    # codesign narrates "replacing existing signature" once per file on stderr,
    # so stderr goes to a log and only the tail is shown on failure. Piping it
    # through grep instead would swallow the exit status (pipefail is on, and a
    # grep that matches nothing exits 1), which is exactly the thing that must
    # not be swallowed here.
    sign_batch() {  # $1 = file of newline-separated paths, $2 = label
        local list="$1" label="$2" err="$STAGE/sign-$2.err"
        [[ -s "$list" ]] || return 0
        if ! tr '\n' '\0' < "$list" \
             | xargs -0 -n 64 codesign --force "${TIMESTAMP_ARG[@]}" -s "$SIGN_ID" 2>"$err"; then
            grep -v ': replacing existing signature' "$err" | tail -20 >&2 || true
            die "codesign failed while signing the $label batch (see $err)"
        fi
    }

    sign_batch "$MACHO_LIB" lib
    ok "$N_LIB libraries"

    # Everything Mach-O and MH_EXECUTE that is *not* one of our two — wheels
    # occasionally ship a helper binary (torch's shm manager, ctranslate2's
    # converters). They get the identity so the seal holds, but not the
    # hardened runtime: they are not processes we launch, and handing them the
    # engine's entitlements would spread three permissions across binaries that
    # have no use for them.
    grep -Fxv -e "$PYBIN" -e "$SOXBIN" "$MACHO_EXE" > "$STAGE/macho-exe-other.txt" || true
    sign_batch "$STAGE/macho-exe-other.txt" exe

    for exe in "$PYBIN" "$SOXBIN"; do
        codesign --force --options runtime "${TIMESTAMP_ARG[@]}" \
            --entitlements "$ROOT/$ENT_ENGINE" -s "$SIGN_ID" "$exe" 2>&1 \
            | sed 's/^/  /' || die "codesign failed on $exe"
    done
    ok "$N_EXE executables (python$PYVER + sox hardened with $ENT_ENGINE)"

    codesign --force --options runtime "${TIMESTAMP_ARG[@]}" \
        --entitlements "$ROOT/$ENT_APP" \
        -i "$BUNDLE_ID" -s "$SIGN_ID" \
        "$CONTENTS/MacOS/$APP_BIN" 2>&1 | sed 's/^/  /' \
        || die "codesign failed on Contents/MacOS/$APP_BIN"
    ok "Contents/MacOS/$APP_BIN (hardened runtime + $ENT_APP)"

    # The bundle last. No --options runtime: CFBundleExecutable is a shell
    # script and the hardened runtime is a Mach-O flag a script cannot carry.
    codesign --force "${TIMESTAMP_ARG[@]}" -s "$SIGN_ID" "$APP" 2>&1 | sed 's/^/  /' \
        || die "codesign failed on the bundle"
    SIGN_SECS=$(( $(secs) - T_SIGN ))
    ok "$APP_NAME.app sealed in ${SIGN_SECS}s"
else
    log "Ad-hoc signing (no identity)"
    codesign --force --deep -s - "$APP" 2>&1 | sed 's/^/  /' || die "codesign failed"
    SIGN_SECS=0; N_LIB=0; N_EXE=0
fi

log "Verifying the signature"
codesign --verify --strict --deep --verbose=2 "$APP" 2>&1 | sed 's/^/  /' \
    || die "codesign --verify --strict --deep failed on the finished bundle"
codesign -d -r- "$APP" 2>&1 | sed 's/^/  /'

# spctl is expected to refuse a self-signed app: Gatekeeper wants Developer ID
# and notarization, neither of which a certificate we minted can provide. It is
# reported rather than gated so the day it *passes* is visible.
if spctl --assess --type execute "$APP" 2>&1 | sed 's/^/  spctl: /'; then
    ok "Gatekeeper accepts the bundle"
else
    hint "Gatekeeper rejects the bundle — expected for a self-signed identity;
      the structure is notarization-shaped, only the certificate is missing."
fi

# --------------------------------------------------------------------------
# 15. self-containment proof
#
# `env -i` with nothing but HOME and the system PATH: no SYRINX_* variables, no
# PATH entry that could reach Homebrew or the checkout, no PYTHONPATH. The
# engine has 20 seconds to import the whole ML stack and write its RPC discovery
# file; if it needs anything outside the bundle it dies before that, and the
# tail of its stderr says what.
#
# SYRINX_DATA_DIR and SYRINX_RPC_ENDPOINT both point at a scratch directory so
# the check cannot disturb the real ~/Library/Application Support/syrinx. They
# are two variables because they are two mechanisms: rpc.discovery_path() reads
# SYRINX_RPC_ENDPOINT and falls through to paths._default_data_dir(), which is
# the pre-override default and does NOT honour SYRINX_DATA_DIR. Set only the
# latter and this probe would drop its rpc.json on top of a running dev engine's.

log "Proving self-containment (env -i)"
PROBE="$STAGE/probe"
rm -rf "$PROBE"; mkdir -p "$PROBE"
set +e
env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin \
    SYRINX_DATA_DIR="$PROBE" SYRINX_RPC_ENDPOINT="$PROBE/rpc.json" \
    "$CONTENTS/$ENGINE_REL/.venv/bin/syrinx-engine" \
    >"$PROBE/out.log" 2>&1 &
PROBE_PID=$!
for _ in $(seq 1 60); do
    [[ -s "$PROBE/rpc.json" ]] && break
    kill -0 "$PROBE_PID" 2>/dev/null || break
    sleep 1
done
PROBE_OK=0
[[ -s "$PROBE/rpc.json" ]] && PROBE_OK=1
kill "$PROBE_PID" 2>/dev/null
wait "$PROBE_PID" 2>/dev/null
set -e
if (( PROBE_OK )); then
    ok "the bundled engine booted and bound its RPC endpoint with no environment"
else
    sed 's/^/  /' "$PROBE/out.log" | tail -25
    die "the bundled engine did not come up under env -i — see $PROBE/out.log"
fi

# --------------------------------------------------------------------------
# 16. DMG
#
# A plain drag-to-Applications volume: the app and a symlink, compressed (UDZO).
# Not a fancy background-image layout — that needs an AppleScript pass through
# Finder, which needs an interactive session and Automation TCC, and would turn
# a headless build into one that only works when someone is logged in at the
# console. The window opens on a two-icon folder and everyone knows what to do.

if (( DO_DMG )); then
    log "Packing the DMG"
    T_DMG="$(secs)"
    DMG="$DIST/$APP_NAME-$VERSION.dmg"
    DMGSTAGE="$STAGE/dmg"
    rm -rf "$DMGSTAGE" "$DMG"
    mkdir -p "$DMGSTAGE"
    # ditto, not cp: the signed bundle's metadata has to cross verbatim, and a
    # DMG whose copy of the app no longer verifies is worse than no DMG.
    ditto "$APP" "$DMGSTAGE/$APP_NAME.app"
    ln -s /Applications "$DMGSTAGE/Applications"
    hdiutil create -quiet -volname "$APP_NAME $VERSION" -srcfolder "$DMGSTAGE" \
        -ov -format UDZO "$DMG" || die "hdiutil failed"
    rm -rf "$DMGSTAGE"
    DMG_SIZE="$(human "$DMG")"
    ok "dist/$(basename "$DMG")  ($DMG_SIZE, packed in $(( $(secs) - T_DMG ))s)"
else
    DMG=""
    DMG_SIZE="(skipped)"
fi

# --------------------------------------------------------------------------
# summary

TOTAL=$(( $(secs) - T_START ))

cat <<EOF

${BOLD}$APP_NAME $VERSION is built.${RESET}

  App             $APP  ($APP_SIZE)$( [[ -n "$DMG" ]] && printf '\n  DMG             %s  (%s)' "$DMG" "$DMG_SIZE" )
  Interpreter     CPython $PY_FULL, embedded at Contents/$ENGINE_REL/.venv
  Packages        $PIN_COUNT pinned + $URL_COUNT by url + syrinx-engine, from engine/.venv's freeze
  Signed          $SIGN_ID ($N_LIB libraries, $N_EXE executables, ${SIGN_SECS}s)
  Build time      $(( TOTAL / 60 ))m $(( TOTAL % 60 ))s

Install it by dragging from the DMG, or:

  rm -rf /Applications/$APP_NAME.app && cp -R "$APP" /Applications/
  open -a $APP_NAME

Data stays where the dev build put it — ~/Library/Application Support/syrinx —
so models, voices, history and settings carry over untouched. The bundle ID and
the certificate are the dev bundle's too, which means the microphone,
system-audio and Accessibility grants carry over as well: same designated
requirement, no re-prompt, no tccutil reset.

${YELLOW}Installed on demand, not bundled:${RESET} Seed-VC (GPL-3.0), Vevo's Amphion
checkpoints (CC-BY-NC) and LuxTTS (git-only). Their three setup scripts DO ship
now, at Contents/$ENGINE_REL/, and the Models tab runs them for the user; the
venv each one builds lands under ~/Library/Application Support/syrinx/<id>/,
never inside the bundle, so a completed install leaves the code signature as
clean as it was. Those installs want the network and, for Vevo and LuxTTS, a
working git — a bare Mac is told to run \`xcode-select --install\` before the
first byte is downloaded rather than after several GB of it.

Gatekeeper will refuse this bundle on any Mac that did not mint the signing
certificate. That is what Developer ID is for; set SYRINX_SIGN_IDENTITY and
nothing else in this script changes.
EOF

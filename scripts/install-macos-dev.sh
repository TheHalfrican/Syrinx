#!/usr/bin/env bash
#
# Syrinx macOS dev launcher — wraps THIS CHECKOUT in a real .app so Finder,
# Launchpad and Spotlight can start it.
#
#   scripts/install-macos-dev.sh              install (rebuild + replace)
#   scripts/install-macos-dev.sh --uninstall  remove the installed bundle
#
# This is the *dev* seed of the mac packaging story, not the relocatable bundle:
# the app inside Syrinx.app is a copy of target/release/syrinx-app, but the
# engine still runs from engine/.venv in this working tree, and the launcher has
# that path baked in at install time. Move or delete the checkout and the app
# stops finding an engine — re-run this script from the new location. Shipping a
# self-contained bundle (embedded CPython, no checkout) is a later phase; see
# packaging/WINDOWS.md for the shape that takes on the other platform.
#
# Why a bundle at all, for a dev build? TCC. An unbundled binary has no
# Info.plist, so it has no NSMicrophoneUsageDescription and no stable identity —
# the mic prompt gets attributed to whatever terminal launched it, and the grant
# follows the terminal instead of Syrinx. A bundle (even ad-hoc signed) fixes
# both: our own usage string, our own TCC identity.
#
# System audio (the native Core Audio tap) makes that non-negotiable rather than
# merely tidy: tccd refuses kTCCServiceAudioCapture outright — no prompt, no
# error, just a silent tap — when the responsible process has no
# NSAudioCaptureUsageDescription. A terminal never has one.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="Syrinx"
BUNDLE_ID="sh.syrinx.app"
LAUNCHER="Syrinx"          # Contents/MacOS/Syrinx — CFBundleExecutable
APP_BIN="syrinx-app"       # the copied release binary the launcher execs
BREW_BIN="/opt/homebrew/bin"

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

# Where the bundle lands: /Applications when we can write it without sudo (the
# normal case for the admin user who set the machine up), else the per-user
# ~/Applications, which LaunchServices indexes just the same.
dest_dir() {
    if [[ -w /Applications ]]; then
        printf '/Applications'
    else
        mkdir -p "$HOME/Applications"
        printf '%s' "$HOME/Applications"
    fi
}

# --------------------------------------------------------------------------
# args

case "${1-}" in
    "") ;;
    --uninstall)
        DEST="$(dest_dir)/$APP_NAME.app"
        if [[ -d "$DEST" ]]; then
            rm -rf "$DEST"
            ok "removed $DEST"
        else
            ok "nothing installed at $DEST"
        fi
        printf '\n%sSyrinx.app removed.%s The checkout at %s is untouched.\n' \
            "$BOLD" "$RESET" "$ROOT"
        exit 0 ;;
    -h|--help)
        cat <<'EOF'
Syrinx macOS dev launcher.

usage: scripts/install-macos-dev.sh [--uninstall]

  (no args)     build the release app, assemble Syrinx.app around it, ad-hoc
                sign it and install it to /Applications (or ~/Applications)
  --uninstall   delete the installed Syrinx.app

The engine keeps running from this checkout — see the header comment.
EOF
        exit 0 ;;
    *) die "unknown argument: $1 (try --uninstall)" ;;
esac

# --------------------------------------------------------------------------
# guards

[[ "$(uname -s)" == "Darwin" ]] || die "this installer is macOS-only (use scripts/install.sh on Linux)"

command -v cargo >/dev/null || die "cargo not found — install rust first"

ENGINE_CMD="$ROOT/engine/.venv/bin/syrinx-engine"
[[ -x "$ENGINE_CMD" ]] || die \
    "engine/.venv/bin/syrinx-engine not found — run the engine setup first:
       cd engine && python3 -m venv .venv && .venv/bin/pip install -e ."

# --------------------------------------------------------------------------
# version — [workspace.package] version in the workspace Cargo.toml, else 0.1.0

VERSION="$(awk '
    /^\[/ { in_wp = ($0 == "[workspace.package]") }
    in_wp && /^[[:space:]]*version[[:space:]]*=/ {
        if (match($0, /"[^"]*"/)) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
    }
' "$ROOT/Cargo.toml" 2>/dev/null || true)"
[[ -n "$VERSION" ]] || VERSION="0.1.0"

# --------------------------------------------------------------------------
# 1. build

log "Building the release app (cargo build --release -p syrinx-app -p syrinx-shared)"
# Never a bare `cargo build` here: dictate/ carries ungated gtk4 deps that don't
# resolve on macOS, so the workspace only builds member-by-member.
cargo build --release -p syrinx-app -p syrinx-shared
[[ -f "$ROOT/target/release/$APP_BIN" ]] || die "target/release/$APP_BIN missing after build"
ok "target/release/$APP_BIN"

# --------------------------------------------------------------------------
# 2. assemble the bundle in a staging dir, so a failure never leaves a
#    half-written app where LaunchServices can find it

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/syrinx-app-bundle.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

APP="$STAGE/$APP_NAME.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
mkdir -p "$MACOS" "$RESOURCES"

log "Assembling $APP_NAME.app (version $VERSION)"

install -m 755 "$ROOT/target/release/$APP_BIN" "$MACOS/$APP_BIN"
ok "Contents/MacOS/$APP_BIN"

# The launcher. @REPO@ is substituted below — that bake-in is what makes this a
# *dev* bundle. Paths go through sed's s||| with | as the delimiter, fine for
# absolute paths, which never contain a pipe.
sed "s|@REPO@|$ROOT|g" > "$MACOS/$LAUNCHER" <<'LAUNCHER_EOF'
#!/bin/sh
#
# Syrinx dev launcher — GENERATED by scripts/install-macos-dev.sh. Don't edit it
# in place; re-run the installer instead.
#
# The checkout path below is baked in at install time. That's the whole point of
# the dev bundle: the app is a copy, the engine is not.

# Probe #1 of the engine resolution order (RPC-PROTOCOL §13.2 step 2). Finder
# and Launchpad start us with cwd=/, so the cwd-relative probe (#2) can't fire,
# and the app-exe-ancestor probe (#3) walks up out of /Applications — neither
# can ever see the checkout. The env var is the only probe that reaches it.
SYRINX_ENGINE_CMD="@REPO@/engine/.venv/bin/syrinx-engine"
export SYRINX_ENGINE_CMD

# LaunchServices hands GUI apps a minimal environment: PATH is the system one,
# with no Homebrew prefix on it. The engine's qwen backend needs the `sox`
# binary at import time, and the app spawns the engine as a child, so the brew
# prefix has to be on PATH here — before the engine inherits it.
PATH="/opt/homebrew/bin:$PATH"
export PATH

# exec, not call: the real app takes over this pid, so LaunchServices' quit and
# any SIGTERM land on the process that can act on them.
exec "$(dirname "$0")/syrinx-app" "$@"
LAUNCHER_EOF
chmod 755 "$MACOS/$LAUNCHER"
ok "Contents/MacOS/$LAUNCHER (engine: $ENGINE_CMD)"

# --------------------------------------------------------------------------
# 3. icon — packaging/syrinx.svg -> Resources/syrinx.icns, best effort
#
# Tried in order: rsvg-convert (brew, sharpest), qlmanage -t (built-in Quick
# Look), sips (built-in). We never install anything to get an icon — a dev
# bundle without one still launches, it just shows the generic app tile.

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
    # The sizes iconutil expects; @2x is just the next power of two rendered at
    # the smaller slot's name.
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
        hint "iconutil failed — installing without an icon"
    fi
else
    hint "no SVG renderer worked (tried rsvg-convert, qlmanage, sips) — installing without an icon"
fi

# --------------------------------------------------------------------------
# 4. Info.plist
#
# CFBundleIconFile is only emitted when we actually produced an icns: a plist
# pointing at a missing resource makes LaunchServices cache a broken icon.

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
# 5. install — replace whatever is there (it's a build product) and sign in
#    place, so the signature is the one the final path carries

DEST_DIR="$(dest_dir)"
DEST="$DEST_DIR/$APP_NAME.app"

log "Installing to $DEST"
if [[ -d "$DEST" ]]; then
    rm -rf "$DEST"
    ok "removed the previous bundle"
fi
# ditto rather than mv: TMPDIR is a different volume, and ditto carries the
# bundle across one verbatim instead of falling back to a plain copy.
ditto "$APP" "$DEST"
ok "$DEST"

# Ad-hoc (`-s -`) — no Developer ID here. It won't satisfy Gatekeeper for
# anything downloaded, but it gives the bundle a stable cdhash, which is what
# TCC keys the microphone grant to. Without it every rebuild can re-prompt.
log "Ad-hoc signing"
codesign --force --deep -s - "$DEST" 2>&1 | sed 's/^/  /' || die "codesign failed"
codesign --verify --deep "$DEST" 2>/dev/null && ok "signature verifies" \
    || hint "codesign --verify was unhappy; the app still runs, but TCC may re-prompt"

# Nudge LaunchServices so Spotlight/Launchpad see a fresh install immediately
# instead of whenever the volume is next scanned.
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[[ -x "$LSREGISTER" ]] && "$LSREGISTER" -f "$DEST" >/dev/null 2>&1 && ok "registered with LaunchServices"

# --------------------------------------------------------------------------
# summary

cat <<EOF

${BOLD}$APP_NAME.app is installed.${RESET}

  Location        $DEST
  Launch          Spotlight/Launchpad ("Syrinx"), or: open -a Syrinx
  Engine          $ENGINE_CMD
  Engine logs     ~/Library/Application Support/syrinx/engine.log

First launch will ask for ${BOLD}microphone${RESET} access — that prompt now names Syrinx
rather than your terminal, which is the point of the bundle. The first
${BOLD}system-audio${RESET} recording asks separately, for "System Audio Recording"
(System Settings > Privacy & Security > Screen & System Audio Recording); that
grant is what the native Core Audio tap needs, and no loopback driver is
involved. Reinstalling re-signs the bundle, so both prompts can come back.

If this checkout lives under ~/Documents, ~/Desktop or ~/Downloads, the very
first launch also asks for ${BOLD}files-in-Documents${RESET} access — and the app
sits blank until that dialog is answered (the engine's first file read blocks
inside the TCC gate, it does not fail). One time only; the grant keys to the
bundle's signature and survives reinstalls.

${YELLOW}Dev bundle caveat:${RESET} the engine runs from this checkout
  $ROOT
Don't move or delete it — if you do, re-run this script from the new location.

Uninstall with: scripts/install-macos-dev.sh --uninstall
EOF

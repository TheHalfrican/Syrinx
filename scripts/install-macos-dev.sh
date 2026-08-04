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
#
# The bundle gets us an identity; a *certificate* is what makes that identity
# hold still. TCC keys every grant to the designated requirement of the code
# that asked, and an ad-hoc signature has no certificate to name, so the DR
# falls back to the binary's own cdhash — which changes on every rebuild,
# orphaning the grant behind a checkbox that no longer does anything. Signed
# with scripts/dev-signing-identity.sh's self-signed cert instead, the DR is
#
#     identifier "sh.syrinx.app" and certificate leaf = H"…"
#
# and the grants survive rebuilds. Run that script once; this one finds it.
#
# Signing is per-binary and hardened, not `--deep`: syrinx-app is signed first
# with `--options runtime` and packaging/entitlements-app.plist, then the bundle
# is signed around it. That is the shape a Developer ID build wants, so the day
# there is one, SYRINX_SIGN_IDENTITY is the only thing that changes.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="Syrinx"
BUNDLE_ID="sh.syrinx.app"
LAUNCHER="Syrinx"          # Contents/MacOS/Syrinx — CFBundleExecutable
APP_BIN="syrinx-app"       # the copied release binary the launcher execs
BREW_BIN="/opt/homebrew/bin"
DEV_IDENTITY="Syrinx Dev Signing"          # what scripts/dev-signing-identity.sh makes
ENTITLEMENTS="packaging/entitlements-app.plist"

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

  (no args)     build the release app, assemble Syrinx.app around it, sign it
                and install it to /Applications (or ~/Applications)
  --uninstall   delete the installed Syrinx.app

env:
  SYRINX_SIGN_IDENTITY   codesign identity to use. Defaults to "Syrinx Dev
                         Signing" when scripts/dev-signing-identity.sh has
                         made it, and to ad-hoc when nothing else is there.

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
# signing identity — resolved here, before the build, so the hint about the
# missing identity lands before the several minutes of cargo rather than after
#
#   1. $SYRINX_SIGN_IDENTITY, verbatim — the Developer ID hook. Nothing else in
#      this script has to change on the day there is one.
#   2. "Syrinx Dev Signing", if scripts/dev-signing-identity.sh has made it.
#   3. ad-hoc, with a hint. Still installs, still runs; just goes back to
#      re-granting everything after every rebuild.
#
# The identity's keychain locks itself at login (its password is not the login
# password), and codesign reaching for a locked key throws a GUI dialog into the
# middle of a build. --unlock is silent and never fails: if it can't, we simply
# don't find the identity below and fall through to ad-hoc.
"$ROOT/scripts/dev-signing-identity.sh" --unlock 2>/dev/null || true

# grep on the whole listing, not on the "Valid identities only" tail: a
# self-signed cert is untrusted by design, so it is *matching* but never valid,
# and codesign does not care about the difference.
if [[ -n "${SYRINX_SIGN_IDENTITY-}" ]]; then
    SIGN_ID="$SYRINX_SIGN_IDENTITY"
    SIGN_KIND=identity
elif security find-identity -p codesigning 2>/dev/null | grep -q "\"$DEV_IDENTITY\""; then
    SIGN_ID="$DEV_IDENTITY"
    SIGN_KIND=identity
else
    SIGN_ID="-"
    SIGN_KIND=adhoc
    hint "no signing identity — falling back to ad-hoc, which re-keys every TCC
      grant on every rebuild. Fix it once with:
        scripts/dev-signing-identity.sh"
fi

if [[ "$SIGN_KIND" == identity ]]; then
    [[ -f "$ROOT/$ENTITLEMENTS" ]] || die "$ENTITLEMENTS missing — can't sign with the hardened runtime"
fi

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

# Signing, inside out. Order is not a style choice: the bundle's signature seals
# a *record* of the nested binary — its cdhash and its designated requirement —
# so syrinx-app has to be final before the bundle is signed around it.
if [[ "$SIGN_KIND" == identity ]]; then
    log "Signing as \"$SIGN_ID\""

    # The app binary. This is the one that matters: the launcher execs it, so
    # this signature — not the bundle's — is the code identity every TCC lookup
    # and every entitlement check runs against.
    #
    # -i pins the identifier to the bundle ID rather than letting codesign
    # derive "syrinx-app" from the filename. Both would be stable; this one
    # makes the binary's DR and the bundle's DR say the same thing, and makes
    # `tccutil reset … sh.syrinx.app` name the thing it is actually resetting.
    #
    # No --deep anywhere. --deep re-signs whatever it finds with whatever flags
    # it was given, which is exactly wrong once different pieces need different
    # entitlements — Apple has called it unsuitable for shipping for years.
    codesign --force --options runtime \
        --entitlements "$ROOT/$ENTITLEMENTS" \
        -i "$BUNDLE_ID" -s "$SIGN_ID" \
        "$DEST/Contents/MacOS/$APP_BIN" 2>&1 | sed 's/^/  /' \
        || die "codesign failed on Contents/MacOS/$APP_BIN"
    ok "Contents/MacOS/$APP_BIN (hardened runtime + $ENTITLEMENTS)"

    # The bundle. No --options runtime here: CFBundleExecutable is the shell
    # launcher, and the hardened runtime is a Mach-O load-command flag that a
    # script cannot carry. The script is not left unsigned by that — it is the
    # main executable, so it is hashed straight into the CodeDirectory (edit it
    # in place and the bundle stops verifying), while syrinx-app is sealed
    # alongside as nested code with its own cdhash and requirement.
    codesign --force -s "$SIGN_ID" "$DEST" 2>&1 | sed 's/^/  /' \
        || die "codesign failed on the bundle"
    ok "$APP_NAME.app"

    codesign --verify --strict --deep "$DEST" 2>/dev/null && ok "signature verifies (--strict --deep)" \
        || hint "codesign --verify was unhappy; the app still runs, but TCC may re-prompt"
else
    # Ad-hoc (`-s -`) — the fallback, kept exactly as it was rather than
    # half-modernised. --deep and no hardened runtime: with no certificate to
    # anchor to there is nothing to gain from per-binary signing, and the DR is
    # a cdhash either way. Runs fine; re-prompts after every rebuild.
    log "Ad-hoc signing (no identity)"
    codesign --force --deep -s "$SIGN_ID" "$DEST" 2>&1 | sed 's/^/  /' || die "codesign failed"
    codesign --verify --deep "$DEST" 2>/dev/null && ok "signature verifies" \
        || hint "codesign --verify was unhappy; the app still runs, but TCC may re-prompt"
fi

# Nudge LaunchServices so Spotlight/Launchpad see a fresh install immediately
# instead of whenever the volume is next scanned.
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[[ -x "$LSREGISTER" ]] && "$LSREGISTER" -f "$DEST" >/dev/null 2>&1 && ok "registered with LaunchServices"

# --------------------------------------------------------------------------
# summary
#
# The TCC paragraphs differ by signing kind, and the difference is the whole
# phase: with a certificate the grants are answered once; ad-hoc, they are
# answered again after every rebuild.

# A function rather than a variable holding a here-doc: bash re-lexes a here-doc
# body that sits inside $( ), and prose full of apostrophes does not survive it.
grants() {
if [[ "$SIGN_KIND" == identity ]]; then
    cat <<EOF
Three permissions get asked for, once each:

  ${BOLD}Microphone${RESET}      on first launch. The prompt names Syrinx rather than your
                  terminal — that is what the bundle buys.
  ${BOLD}System Audio${RESET}    on the first system-audio recording. A separate grant, under
                  Privacy & Security > Screen & System Audio Recording; it is
                  what the native Core Audio tap needs, and no loopback driver
                  is involved.
  ${BOLD}Accessibility${RESET}   on the first dictation chord (${BOLD}⌃⌥D${RESET}) — it is what lets Syrinx
                  type the transcript into whatever app you are in. Until it is
                  granted the chord records nothing and the ⚙ DICTATION card
                  says why.

${GREEN}Answer them once and you are done.${RESET} This build is signed with a certificate, so
each grant keys to

  identifier "$BUNDLE_ID" and certificate leaf = H"…"

instead of to a build hash. Rebuild and reinstall as often as you like — the
designated requirement is byte-identical every time and the grants hold.

${YELLOW}One-time migration:${RESET} grants made against an earlier ${BOLD}ad-hoc${RESET} build keyed to a
cdhash this build no longer has. Those rows are dead — they don't re-prompt,
and toggling the checkbox just rewrites the same stale entry. Clear them once:

  tccutil reset Microphone $BUNDLE_ID
  tccutil reset AudioCapture $BUNDLE_ID
  tccutil reset ScreenCapture $BUNDLE_ID
  tccutil reset Accessibility $BUNDLE_ID
  tccutil reset PostEvent $BUNDLE_ID

If Syrinx still appears under Privacy & Security > Accessibility afterwards,
remove it with − and re-add it. Then let each prompt come back one last time.
EOF
else
    cat <<EOF
First launch will ask for ${BOLD}microphone${RESET} access — that prompt now names Syrinx
rather than your terminal, which is the point of the bundle. The first
${BOLD}system-audio${RESET} recording asks separately, for "System Audio Recording"
(System Settings > Privacy & Security > Screen & System Audio Recording); that
grant is what the native Core Audio tap needs, and no loopback driver is
involved. Reinstalling re-signs the bundle, so both prompts can come back.

The first time you press the dictation chord (${BOLD}⌃⌥D${RESET}) Syrinx asks for
${BOLD}Accessibility${RESET} — that grant is what lets it type the transcript into
whatever app you are in. It needs no Info.plist key (unlike the two above),
but it does key to the signature, so a reinstall can require re-adding
Syrinx under Privacy & Security > Accessibility. Until it is granted the
app records nothing on that chord and the ⚙ DICTATION card says why.

${YELLOW}All of that re-prompting is avoidable${RESET} — it is what an ad-hoc signature costs.
Run ${BOLD}scripts/dev-signing-identity.sh${RESET} once and reinstall, and the grants stop
keying to the build hash.
EOF
fi
}

if [[ "$SIGN_KIND" == identity ]]; then
    SIGNED_AS="$SIGN_ID (hardened runtime)"
else
    SIGNED_AS="ad-hoc (no identity)"
fi

cat <<EOF

${BOLD}$APP_NAME.app is installed.${RESET}

  Location        $DEST
  Signed          $SIGNED_AS
  Launch          Spotlight/Launchpad ("Syrinx"), or: open -a Syrinx
  Engine          $ENGINE_CMD
  Engine logs     ~/Library/Application Support/syrinx/engine.log

$(grants)

If this checkout lives under ~/Documents, ~/Desktop or ~/Downloads, the very
first launch also asks for ${BOLD}files-in-Documents${RESET} access — and the app
sits blank until that dialog is answered (the engine's first file read blocks
inside the TCC gate, it does not fail). One time only; the grant keys to the
bundle's signature like the rest.

${YELLOW}Dev bundle caveat:${RESET} the engine runs from this checkout
  $ROOT
Don't move or delete it — if you do, re-run this script from the new location.

Uninstall with: scripts/install-macos-dev.sh --uninstall
EOF

#!/usr/bin/env bash
#
# Syrinx dev signing identity — a self-signed code-signing certificate that
# scripts/install-macos-dev.sh signs the dev bundle with.
#
#   scripts/dev-signing-identity.sh            create it (idempotent)
#   scripts/dev-signing-identity.sh --remove   delete the cert and its keychain
#   scripts/dev-signing-identity.sh --unlock   unlock the keychain, quietly
#
# Why this exists: TCC keys a grant to the *designated requirement* of the code
# that asked for it, and ad-hoc signing (`codesign -s -`) has no certificate to
# name, so the DR it synthesises is the only stable thing left — the cdhash:
#
#     designated => cdhash H"24a7f405…"
#
# That hash is the binary. Rebuild the app and it changes, the grant no longer
# matches anything on disk, and the checkbox in System Settings goes dead:
# toggling it off and on re-writes the same stale row. The only fix is a tccutil
# reset. Every rebuild, for microphone, system audio and Accessibility alike.
#
# Sign with a real certificate and the DR becomes the pair that *doesn't* move
# when the binary does:
#
#     designated => identifier "sh.syrinx.app" and certificate leaf = H"4a323c…"
#
# — the bundle ID we choose, and the cert we made once. Rebuild all day; the
# grants hold. That stability is the entire point, and it is why the future
# Developer ID switch is a one-line parameter change rather than a redesign:
# same code path, a leaf we didn't mint ourselves.
#
# The cert is deliberately NOT added to any trust store. codesign doesn't need
# it — signing uses the private key, not a trust evaluation — so
# `security find-identity -p codesigning` lists this identity under "Matching
# identities" with a CSSMERR_TP_NOT_TRUSTED note and finds "0 valid identities",
# which looks alarming and is fine. Trusting it would mean `add-trusted-cert`,
# which throws an authorization dialog, and would buy nothing: Gatekeeper still
# rejects a self-signed app on any machine that didn't mint the cert. Gatekeeper
# is Developer ID's job, not this script's.

set -euo pipefail

IDENTITY="Syrinx Dev Signing"
KEYCHAIN="$HOME/Library/Keychains/syrinx-signing.keychain-db"
# A fixed, well-known password. There is no secret here worth protecting: the
# certificate authorises nothing off this machine, and a prompt-free installer
# is the whole requirement. It has to live somewhere; it lives in the open.
KEYCHAIN_PASS="syrinx-dev-signing"
VALID_DAYS=3650            # 10 years — outlive the dev phase, never expire mid-build

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

# --------------------------------------------------------------------------
# keychain helpers

# Is the identity usable right now? find-identity against the keychain by path
# rather than the search list, so this answers "did we make it" and not "can the
# search list see it" — the two come apart if someone rebuilds the list by hand.
identity_present() {
    [[ -f "$KEYCHAIN" ]] || return 1
    security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null \
        | grep -q "\"$IDENTITY\""
}

# The SHA-1 the DR will anchor to, for the summary. Empty if absent.
identity_hash() {
    security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null \
        | awk -v id="\"$IDENTITY\"" '$0 ~ id { print $2; exit }'
}

# `security unlock-keychain` on an already-unlocked keychain is a no-op, so this
# is safe to call unconditionally. It matters after a reboot: a keychain created
# by this script is locked at login (its password isn't the login password), and
# codesign reaching for a locked key throws a GUI dialog.
unlock_keychain() {
    [[ -f "$KEYCHAIN" ]] || return 0
    security unlock-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN" 2>/dev/null || return 1
}

# Read the user-domain search list into an array, one path per line, unquoted.
# The system domain (/Library/Keychains/System.keychain) is not in this list and
# is appended by the framework regardless — rewriting the user list can't drop
# it, which is what makes -s safe here.
read_search_list() {
    local line
    SEARCH_LIST=()
    while IFS= read -r line; do
        line="${line#"${line%%[![:space:]]*}"}"      # ltrim
        line="${line%"${line##*[![:space:]]}"}"      # rtrim
        line="${line#\"}"; line="${line%\"}"         # unquote
        [[ -n "$line" ]] || continue
        SEARCH_LIST+=("$line")
    done < <(security list-keychains -d user)
}

search_list_add() {
    local entry
    read_search_list
    for entry in ${SEARCH_LIST+"${SEARCH_LIST[@]}"}; do
        if [[ "$entry" == "$KEYCHAIN" ]]; then
            return 0
        fi
    done
    security list-keychains -d user -s ${SEARCH_LIST+"${SEARCH_LIST[@]}"} "$KEYCHAIN"
}

search_list_remove() {
    local entry found=0
    local -a keep=()
    read_search_list
    for entry in ${SEARCH_LIST+"${SEARCH_LIST[@]}"}; do
        if [[ "$entry" == "$KEYCHAIN" ]]; then
            found=1
        else
            keep+=("$entry")
        fi
    done
    (( found )) || return 1
    security list-keychains -d user -s ${keep+"${keep[@]}"}
}

# --------------------------------------------------------------------------
# args

case "${1-}" in
    "") MODE=create ;;
    --remove) MODE=remove ;;
    --unlock) MODE=unlock ;;
    -h|--help)
        cat <<'EOF'
Syrinx dev signing identity — self-signed cert for the macOS dev bundle.

usage: scripts/dev-signing-identity.sh [--remove|--unlock]

  (no args)   create "Syrinx Dev Signing" in a dedicated keychain; idempotent
  --remove    delete that keychain and drop it from the search list
  --unlock    unlock the keychain if it exists; prints nothing, always succeeds

scripts/install-macos-dev.sh picks the identity up automatically once it exists.
EOF
        exit 0 ;;
    *) die "unknown argument: $1 (try --remove, --unlock or --help)" ;;
esac

[[ "$(uname -s)" == "Darwin" ]] || die "code-signing identities are a macOS thing"

# --unlock is the installer's pre-flight, not a user-facing mode: silent, and
# never fatal — a missing or stubborn keychain just means the installer falls
# through to its ad-hoc path.
if [[ "$MODE" == unlock ]]; then
    unlock_keychain || true
    exit 0
fi

# --------------------------------------------------------------------------
# --remove

if [[ "$MODE" == remove ]]; then
    log "Removing the $IDENTITY identity"
    REMOVED=0
    if search_list_remove; then
        ok "dropped from the keychain search list"
        REMOVED=1
    fi
    if [[ -f "$KEYCHAIN" ]]; then
        # delete-keychain unlinks the file too, so the cert and its private key
        # go with it — there is nothing left in login.keychain to clean up.
        security delete-keychain "$KEYCHAIN"
        ok "deleted $KEYCHAIN"
        REMOVED=1
    fi
    (( REMOVED )) || ok "nothing to remove"

    cat <<EOF

${BOLD}$IDENTITY is gone.${RESET}

Anything already signed with it keeps its signature, but re-signing now falls
back to ad-hoc — and the TCC grants that keyed to this certificate stop
matching. Re-run this script and reinstall to get them back on a stable footing:

  scripts/dev-signing-identity.sh
  scripts/install-macos-dev.sh
EOF
    exit 0
fi

# --------------------------------------------------------------------------
# create

command -v openssl >/dev/null || die "openssl not found (it ships with macOS as /usr/bin/openssl)"

log "Creating the $IDENTITY code-signing identity"

if identity_present; then
    unlock_keychain || hint "the keychain is locked and would not open with the stored password"
    ok "already exists — $(identity_hash) in $KEYCHAIN"
    printf '\nNothing to do. %sscripts/install-macos-dev.sh%s will find it.\n' "$BOLD" "$RESET"
    exit 0
fi

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/syrinx-signing.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

# The certificate. Two extensions are load-bearing:
#   extendedKeyUsage=codeSigning — codesign refuses an identity without it, with
#     a flat "no identity found" that names no reason.
#   basicConstraints=CA:false    — a leaf, not a root that could sign others.
# Everything else (RSA-2048, SHA-256) is picked for what Security.framework
# accepts without argument rather than for cryptographic ambition.
cat > "$STAGE/openssl.cnf" <<CNF
[ req ]
distinguished_name = dn
prompt             = no

[ dn ]
CN = $IDENTITY

[ codesign ]
basicConstraints     = critical,CA:false
keyUsage             = critical,digitalSignature
extendedKeyUsage     = critical,codeSigning
subjectKeyIdentifier = hash
CNF

openssl req -x509 -newkey rsa:2048 -sha256 -days "$VALID_DAYS" -nodes \
    -keyout "$STAGE/key.pem" -out "$STAGE/cert.pem" \
    -config "$STAGE/openssl.cnf" -extensions codesign >/dev/null 2>&1 \
    || die "openssl could not generate the certificate"
ok "self-signed certificate, valid $VALID_DAYS days"

# PKCS#12 is the only container `security import` takes a key and its cert in
# together. OpenSSL 3 defaults to AES-256/PBES2, which Security.framework can't
# read — hence -legacy. LibreSSL (what /usr/bin/openssl is) has no such flag and
# already writes the old format, so try the flag and fall back without it.
p12() { openssl pkcs12 -export "$@" \
    -inkey "$STAGE/key.pem" -in "$STAGE/cert.pem" \
    -out "$STAGE/identity.p12" -passout "pass:$KEYCHAIN_PASS" \
    -name "$IDENTITY" >/dev/null 2>&1; }
p12 -legacy || p12 || die "openssl could not package the identity as PKCS#12"

# The keychain. Dedicated rather than login.keychain so --remove is one unlink
# and touches nothing the user owns. set-keychain-settings with neither -l nor
# -t clears both auto-lock triggers: an unattended build hours later still signs.
if [[ ! -f "$KEYCHAIN" ]]; then
    security create-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"
    ok "keychain $KEYCHAIN"
fi
security set-keychain-settings "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"

# -A drops the key's ACL entirely: any tool may use it, so codesign never raises
# the "wants to sign using key" dialog. -T names codesign anyway, which is what
# takes effect on the systems that ignore -A.
security import "$STAGE/identity.p12" -k "$KEYCHAIN" -P "$KEYCHAIN_PASS" \
    -A -T /usr/bin/codesign -T /usr/bin/security >/dev/null \
    || die "security import rejected the PKCS#12 bundle"

# The ACL is only half of it. Since 10.12 a key also carries a *partition list*,
# and a key imported without one prompts on first use no matter what -A said.
# apple-tool:/apple: cover the signing machinery, codesign: covers codesign.
security set-key-partition-list -S apple-tool:,apple:,codesign: \
    -s -k "$KEYCHAIN_PASS" "$KEYCHAIN" >/dev/null 2>&1 \
    || hint "set-key-partition-list failed — codesign may prompt on first use"
ok "private key imported, no ACL and no prompt"

# Onto the search list, appended to whatever is already there. codesign resolves
# an identity by name through this list, so an identity in a keychain that isn't
# on it is invisible.
if search_list_add; then
    ok "added to the keychain search list"
fi

identity_present || die "the identity did not take — 'security find-identity -p codesigning' cannot see it"

HASH="$(identity_hash)"
ok "$IDENTITY ($HASH)"

# --------------------------------------------------------------------------
# summary

cat <<EOF

${BOLD}$IDENTITY is ready.${RESET}

  Certificate     SHA-1 $HASH
  Keychain        $KEYCHAIN
  Validity        $VALID_DAYS days
  Check it        security find-identity -p codesigning

That listing reports ${BOLD}0 valid identities${RESET} and tags this one
CSSMERR_TP_NOT_TRUSTED. Expected: the cert is self-signed and deliberately
untrusted. codesign signs with it regardless — see the header comment.

Next: ${BOLD}scripts/install-macos-dev.sh${RESET}. It picks this identity up on its own,
and from then on the bundle's designated requirement is

  identifier "sh.syrinx.app" and certificate leaf = H"$HASH"

which does not change when the binary does — so the microphone, system-audio
and Accessibility grants survive every rebuild.

${YELLOW}One time only:${RESET} grants made against the old ad-hoc builds keyed to a cdhash
that no longer exists, and they will not re-prompt on their own. Clear them
after the first identity-signed install:

  tccutil reset Microphone sh.syrinx.app
  tccutil reset AudioCapture sh.syrinx.app
  tccutil reset ScreenCapture sh.syrinx.app
  tccutil reset Accessibility sh.syrinx.app
  tccutil reset PostEvent sh.syrinx.app

(AudioCapture is the system-audio tap's own service — kTCCServiceAudioCapture.
ScreenCapture shares its System Settings pane, PostEvent shares Accessibility's;
resetting the pair leaves nothing stale behind either checkbox.)

Remove all of this with: scripts/dev-signing-identity.sh --remove
EOF

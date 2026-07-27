#!/usr/bin/env bash
# Install D-Bus activation for the engine: the systemd --user unit plus the
# D-Bus .service file that points at it, so the app's connect_dbus() cold-starts
# the engine instead of needing one started by hand (RPC-PROTOCOL.md §13 — "on
# Linux the engine's lifecycle belongs to systemd + D-Bus activation").
#
# This is the no-build subset of scripts/install.sh, for dev boxes that run
# `cargo run` and don't want the release build, the binaries in ~/.local/bin,
# the launcher entry or the icon that the full desktop install also brings.
# Both scripts RENDER THE SAME TEMPLATES from packaging/ — the unit content
# lives there and only there, so the two installers cannot drift. Uninstall
# either one with `scripts/install.sh --uninstall`.
#
# Safe to re-run, and re-running is required after moving the checkout: both
# rendered files hardcode this path (the engine's venvs and worker paths are
# machine-local, hence ExecStart into the clone rather than /usr/bin).
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN_DIR="$HOME/.local/bin"
# Hardcoded rather than XDG-derived to stay byte-identical with the paths
# scripts/install.sh --uninstall removes.
UNIT_DIR="$HOME/.config/systemd/user"
DBUS_DIR="$HOME/.local/share/dbus-1/services"

ENGINE_EXEC="$ROOT/engine/.venv/bin/syrinx-engine"
if [ ! -x "$ENGINE_EXEC" ]; then
    echo "== $ENGINE_EXEC not found — set up the main engine venv first:" >&2
    echo "==   cd engine && python -m venv .venv && .venv/bin/pip install -e ." >&2
    exit 1
fi

# Same @REPO@/@BIN@ rendering as scripts/install.sh; sed's s||| takes | as the
# delimiter, which absolute paths never contain.
render() {
    local src="$1" dst="$2"
    mkdir -p "$(dirname "$dst")"
    sed -e "s|@REPO@|$ROOT|g" -e "s|@BIN@|$BIN_DIR|g" "$src" > "$dst"
    echo "== wrote $dst"
}

render "$ROOT/packaging/syrinx-engine.service.in"    "$UNIT_DIR/syrinx-engine.service"
render "$ROOT/packaging/sh.syrinx.Engine.service.in" "$DBUS_DIR/sh.syrinx.Engine.service"

systemctl --user daemon-reload
echo "== systemctl --user daemon-reload"

# dbus-daemon only rescans its service dirs when asked; without this the very
# first activation of a fresh install fails with "The name is not activatable".
if busctl --user call org.freedesktop.DBus /org/freedesktop/DBus \
        org.freedesktop.DBus ReloadConfig > /dev/null 2>&1; then
    echo "== dbus ReloadConfig"
else
    echo "== couldn't reach the session bus — log out and back in before first launch"
fi

# Capture before matching: piping busctl straight into `grep -q` lets grep exit
# on the first hit, and pipefail would then report the SIGPIPE as our failure.
ACTIVATABLE="$(busctl --user list --activatable --no-legend 2>/dev/null || true)"
if grep -q 'sh\.syrinx\.Engine' <<< "$ACTIVATABLE"; then
    echo "== sh.syrinx.Engine is activatable"
else
    echo "== sh.syrinx.Engine is NOT activatable — check $DBUS_DIR/sh.syrinx.Engine.service" >&2
    exit 1
fi

cat <<EOF

Engine activation installed. Nothing was started, by design — the unit has no
[Install] section and is not enabled at login; the first D-Bus call from the app
or the dictation pill wakes it, and systemd restarts it on failure.

  Logs   journalctl --user -u syrinx-engine -f

An engine started by hand still owns sh.syrinx.Engine while it runs, and systemd
will not activate a second one until that process exits.
EOF

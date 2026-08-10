#!/usr/bin/env bash
#
# s2u-yt — uninstaller.
# Reverses everything install.sh did:
#   1. stops and removes the systemd user service
#   2. restores the previous ~/.local/bin/yt-dlp (from .yt-dlp.s2u-yt.bak)
#   3. removes the package's data root (~/.local/share/s2u-yt)
#
# Usage: ./uninstall.sh [--keep-data]   (--keep-data leaves the downloaded
#        binaries and venv in place for a quick re-install)
set -euo pipefail

NAME="s2u-yt"
DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/$NAME"
MANIFEST="$DATA_ROOT/state/manifest"
UNIT="$HOME/.config/systemd/user/$NAME-bgutil.service"
BIN_DIR="$HOME/.local/bin"
BACKUP="$BIN_DIR/.yt-dlp.$NAME.bak"
WRAPPER="$DATA_ROOT/bin/yt-dlp"
KEEP_DATA=0

log() { printf '\033[1;34m[%s]\033[0m %s\n' "$NAME" "$*"; }
warn() { printf '\033[1;33m[%s]\033[0m warning: %s\n' "$NAME" "$*" >&2; }

[ "${1:-}" = "--keep-data" ] && KEEP_DATA=1

# 1. systemd unit
if [ -f "$UNIT" ]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now "$NAME-bgutil.service" >/dev/null 2>&1 \
            || warn "could not stop service (it may not be running)"
    fi
    rm -f "$UNIT"
    command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload >/dev/null 2>&1 || true
    log "removed service unit: $UNIT"
fi

# 2. wrapper
if [ -L "$BIN_DIR/yt-dlp" ] && [ "$(readlink -f "$BIN_DIR/yt-dlp")" = "$WRAPPER" ]; then
    rm -f "$BIN_DIR/yt-dlp"
    log "removed wrapper $BIN_DIR/yt-dlp"
    if [ -e "$BACKUP" ]; then
        mv "$BACKUP" "$BIN_DIR/yt-dlp"
        log "restored previous yt-dlp from $BACKUP"
    else
        warn "no backup at $BACKUP — yt-dlp now missing from $BIN_DIR"
    fi
else
    warn "$BIN_DIR/yt-dlp is not our wrapper; leaving it untouched"
fi

# 3. data root
if [ "$KEEP_DATA" -eq 0 ]; then
    rm -rf "$DATA_ROOT"
    log "removed data root: $DATA_ROOT"
else
    log "kept data root: $DATA_ROOT (--keep-data)"
fi

log "done."

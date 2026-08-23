#!/usr/bin/env bash
#
# s2u-yt — contained YouTube playback fix for mpv / s2udio.
#
# Fixes the "HTTP 403 Forbidden on googlevideo stream URLs" problem by
# provisioning a Proof-of-Origin (PO) token provider for yt-dlp, so every
# yt-dlp user on the box — mpv's embedded ytdl hook, s2udio's radio/search,
# plain CLI — gets minted PO tokens and YouTube stops 403ing playback.
#
# Everything this package owns lives under $DATA_ROOT
# (~/.local/share/s2u-yt by default). The only changes made outside it:
#   1. ~/.local/bin/yt-dlp is replaced by a small wrapper
#      (the previous file is preserved at ~/.local/bin/.yt-dlp.s2u-yt.bak)
#   2. a systemd --user service is installed for the token server
#      (~/.config/systemd/user/s2u-yt-bgutil.service)
#
# ./uninstall.sh reverses both and removes $DATA_ROOT.
#
# Usage:
#   ./install.sh [--provider bgutil|wpc] [--port 4416] [--test-url URL]
#                [--dry-run]
#
#   --provider wpc  use the browser-minted provider instead (needs Chromium;
#                   opens a browser window per yt-dlp call — fallback option)
#   --dry-run       print the plan without changing anything
#
set -euo pipefail

NAME="s2u-yt"
VERSION="0.1.0"
RS_VERSION="${RS_VERSION:-0.8.1}"          # jim60105/bgutil-ytdlp-pot-provider-rs release
RS_REPO="jim60105/bgutil-ytdlp-pot-provider-rs"
MIN_YTDLP="2026.08.19"                     # VISIONOS client + post-SABR client order (2026-08-22)
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-4416}"
TEST_URL="${TEST_URL:-https://www.youtube.com/watch?v=xz8tmSUddf8}"

DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/$NAME"
PLUGIN_ROOT="$DATA_ROOT/plugins"
SERVER_BIN="$DATA_ROOT/server/bgutil-pot"
WRAPPER="$DATA_ROOT/bin/yt-dlp"
CONF_FILE="$DATA_ROOT/conf/config"
STATE_DIR="$DATA_ROOT/state"
MANIFEST="$STATE_DIR/manifest"
UNIT="$HOME/.config/systemd/user/$NAME-bgutil.service"
BIN_DIR="$HOME/.local/bin"
BACKUP="$BIN_DIR/.yt-dlp.$NAME.bak"

PROVIDER="${PROVIDER:-bgutil}"
DRY=0
LOG=/dev/null

log()  { printf '\033[1;34m[%s]\033[0m %s\n' "$NAME" "$*"; }
warn() { printf '\033[1;33m[%s]\033[0m warning: %s\n' "$NAME" "$*" >&2; }
die()  { printf '\033[1;31m[%s]\033[0m error: %s\n' "$NAME" "$*" >&2; exit 1; }
run()  { if [ "$DRY" -eq 1 ]; then log "would run: $*"; else "$@"; fi; }

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

arch_name() {
    case "$(uname -m)" in
        x86_64)  echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *) die "unsupported architecture: $(uname -m) (package supports linux x86_64/aarch64)" ;;
    esac
}

find_real_ytdlp() {
    # The system yt-dlp the wrapper will exec. Never resolve to our own wrapper.
    local p
    if [ -f "$MANIFEST" ]; then
        local saved; saved="$(awk -F= '/^REAL_YTDLP=/{print $2}' "$MANIFEST" 2>/dev/null || true)"
        [ -n "$saved" ] && [ -x "$saved" ] && { echo "$saved"; return; }
    fi
    p="$(command -v yt-dlp 2>/dev/null || true)"
    if [ -n "$p" ]; then
        p="$(readlink -f "$p")"
        case "$p" in
            "$DATA_ROOT"/* | "$HOME"/.local/share/s2u-*/bin/yt-dlp) p="" ;;  # any s2u wrapper — keep looking
        esac
    fi
    if [ -z "$p" ] && [ -x "$HOME/.local/share/pipx/venvs/yt-dlp/bin/yt-dlp" ]; then
        p="$HOME/.local/share/pipx/venvs/yt-dlp/bin/yt-dlp"
    fi
    [ -n "$p" ] || die "no system yt-dlp found (install it, e.g. 'pipx install yt-dlp')"
    echo "$p"
}

check_ytdlp_version() {
    local bin="$1" ver
    ver="$("$bin" --version 2>/dev/null | head -1 || true)"
    [ -n "$ver" ] || die "cannot run yt-dlp at $bin"
    if ! python3 - "$ver" "$MIN_YTDLP" <<'EOF'
import sys
def v(s):
    return tuple(int(x) for x in s.split("."))
sys.exit(0 if v(sys.argv[1]) >= v(sys.argv[2]) else 1)
EOF
    then
        die "yt-dlp $ver is too old (need >= $MIN_YTDLP); upgrade it first"
    fi
    log "using yt-dlp $ver at $bin"
}

require_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }

# ---------------------------------------------------------------------------
# provisioning
# ---------------------------------------------------------------------------

install_conf() {
    # Copy the package's managed yt-dlp config into the runtime dir.
    local src="$(dirname "$0")/conf/config"
    if [ "$DRY" -eq 1 ]; then
        log "would write $CONF_FILE"
        return
    fi
    mkdir -p "$(dirname "$CONF_FILE")"
    if [ -f "$src" ]; then
        cp "$src" "$CONF_FILE"
    else
        # Fallback (e.g. the package dir was moved after install).
        printf '# s2u-yt managed config\n# 2026-08-22: do NOT pin player_client — android_vr URLs are SABR-range-restricted (403 on plain/open-ended GETs); the default client order reaches anonymous VISIONOS whose URLs play everywhere (verified live). See conf/config header.\n' > "$CONF_FILE"
    fi
    log "config written: $CONF_FILE"
}

install_wrapper() {
    local real="$1"
    if [ "$DRY" -eq 1 ]; then
        log "would write wrapper $WRAPPER (execs $real + plugin-dirs + config-locations)"
        log "would replace $BIN_DIR/yt-dlp (backup -> $BACKUP)"
        return
    fi
    mkdir -p "$DATA_ROOT/bin"
    cat > "$WRAPPER" <<EOF
#!/bin/sh
# $NAME managed wrapper — regenerated by install.sh; do not edit.
#
# Two-phase client strategy (see conf/config):
#   1. anonymous pass with NO pinned player_client — yt-dlp's default
#      order reaches the anonymous VISIONOS client, whose googlevideo
#      URLs answer plain GETs/open ranges (android_vr is SABR-dead since
#      2026-08: 403 on everything but bounded ranges).
#   2. on failure, retry with the user's cookies.
ROOT="\${XDG_DATA_HOME:-\$HOME/.local/share}/$NAME"
USER_CONF="\$HOME/.config/yt-dlp/config"
CONF="\$ROOT/conf/config"
PLUGINS="\$ROOT/plugins"

if [ -f "\$USER_CONF" ]; then
    # Phase 1: anonymous (cookie options stripped) — default client order.
    ANON_CONF="\$ROOT/conf/user-config.anon"
    awk '
        /^[[:space:]]*--cookies(-from-browser)?([[:space:]]|$)/ { skip = 1; next }
        skip && /^[^-]/ { skip = 0; next }
        { skip = 0; print }
    ' "\$USER_CONF" > "\$ANON_CONF" 2>/dev/null || :
    # --ignore-config: the default ~/.config/yt-dlp/config must NOT leak its
    # cookies into this anonymous pass (only the sanitized config applies).
    # Buffer phase 1's stdout: a FAILED anonymous pass still prints a leading
    # "null" line before erroring, which corrupts the stdout JSON parsers of
    # s2udio / mpv / CLI users. Forward the output only on success; on failure
    # discard it and fall through to the authenticated pass.
    TMP="$(mktemp "\${TMPDIR:-/tmp}/s2u-yt.XXXXXX")"
    "$real" --ignore-config --config-locations "\$ANON_CONF" --config-locations "\$CONF" --plugin-dirs "\$PLUGINS" "\$@" >"\$TMP"
    status=\$?
    if [ "\$status" -eq 0 ]; then
        cat "\$TMP"
        rm -f "\$TMP"
        exit 0
    fi
    rm -f "\$TMP"
    # Phase 2: authenticated retry (user cookies).
    exec "$real" --ignore-config --config-locations "\$USER_CONF" --config-locations "\$CONF" --plugin-dirs "\$PLUGINS" "\$@"
else
    exec "$real" --ignore-config --config-locations "\$CONF" --plugin-dirs "\$PLUGINS" "\$@"
fi
EOF
    chmod +x "$WRAPPER"
    log "wrapper written: $WRAPPER"

    if [ -e "$BIN_DIR/yt-dlp" ]; then
        [ -e "$BACKUP" ] && warn "overwriting previous backup $BACKUP"
        mv -f "$BIN_DIR/yt-dlp" "$BACKUP"
        log "previous yt-dlp preserved at $BACKUP"
    fi
    ln -sf "$WRAPPER" "$BIN_DIR/yt-dlp"
    log "installed wrapper at $BIN_DIR/yt-dlp"
}

fetch_bgutil() {
    local arch; arch="$(arch_name)"
    local base="https://github.com/$RS_REPO/releases/download/v$RS_VERSION"
    require_cmd curl

    if [ "$DRY" -eq 1 ]; then
        log "would download provider server + plugin (release v$RS_VERSION, linux-$arch)"
        return
    fi
    mkdir -p "$DATA_ROOT/server" "$DATA_ROOT/plugins"
    if [ ! -x "$SERVER_BIN" ] || [ "$(cat "$STATE_DIR/server-version" 2>/dev/null || true)" != "$RS_VERSION" ]; then
        run curl -fL --retry 3 -o "$SERVER_BIN.tmp" "$base/bgutil-pot-linux-$arch"
        run chmod +x "$SERVER_BIN.tmp"
        if [ "$DRY" -eq 1 ]; then
            log "would verify server binary: $SERVER_BIN.tmp --version"
        else
            mv "$SERVER_BIN.tmp" "$SERVER_BIN"
            mkdir -p "$STATE_DIR"; echo "$RS_VERSION" > "$STATE_DIR/server-version"
            "$SERVER_BIN" --version >/dev/null 2>&1 \
                || warn "bgutil-pot --version failed (binary may still work)"
        fi
        log "provider server: $SERVER_BIN ($RS_VERSION)"
    else
        log "provider server already present ($RS_VERSION)"
    fi

    # extract if missing (the release zip may lay out yt_dlp_plugins/ flat, or
    # under a plugin subdir)
    if [ ! -d "$PLUGIN_ROOT/yt_dlp_plugins" ] && [ ! -d "$PLUGIN_ROOT/bgutil-ytdlp-pot-provider/yt_dlp_plugins" ]; then
        local zip="$PLUGIN_ROOT/bgutil-ytdlp-pot-provider-rs.zip"
        run curl -fL --retry 3 -o "$zip" "$base/bgutil-ytdlp-pot-provider-rs.zip"
        if [ "$DRY" -eq 1 ]; then
            log "would extract plugin zip -> $PLUGIN_ROOT"
        else
            if command -v unzip >/dev/null 2>&1; then
                unzip -q -o "$zip" -d "$PLUGIN_ROOT"
            else
                python3 -m zipfile -e "$zip" "$PLUGIN_ROOT"
            fi
            rm -f "$zip"
        fi
    fi
    # yt-dlp's --plugin-dirs expects ONE plugin folder per child entry, so a
    # flat extraction gets normalized into plugins/bgutil-ytdlp-pot-provider/.
    if [ "$DRY" -eq 0 ] && [ -d "$PLUGIN_ROOT/yt_dlp_plugins" ] \
        && [ ! -d "$PLUGIN_ROOT/bgutil-ytdlp-pot-provider" ]; then
        mkdir -p "$PLUGIN_ROOT/bgutil-ytdlp-pot-provider"
        mv "$PLUGIN_ROOT/yt_dlp_plugins" "$PLUGIN_ROOT/bgutil-ytdlp-pot-provider/"
        log "normalized plugin layout into $PLUGIN_ROOT/bgutil-ytdlp-pot-provider"
    fi
    log "yt-dlp plugin: $PLUGIN_ROOT"
}

install_service() {
    if [ "$DRY" -eq 1 ]; then
        log "would write service unit $UNIT and enable it"
        return
    fi
    mkdir -p "$HOME/.config/systemd/user"
    cat > "$UNIT" <<EOF
[Unit]
Description=$NAME bgutil PO token provider (YouTube)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=%h/.local/share/$NAME/server/bgutil-pot server --host $HOST --port $PORT
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF
    log "service unit written: $UNIT"

    if command -v systemctl >/dev/null 2>&1 && [ -n "$(systemctl --user is-system-running 2>/dev/null || true)" ]; then
        systemctl --user daemon-reload
        systemctl --user enable --now "$NAME-bgutil.service"
        log "service enabled and started (systemd --user)"
    else
        warn "systemd user session not available; skipping auto-start"
        warn "start the server manually: nohup $SERVER_BIN server --host $HOST --port $PORT &"
    fi
}

install_wpc() {
    # Fallback provider: yt-dlp-getpot-wpc (mints tokens in a real Chromium).
    require_cmd python3
    local chrome
    chrome="$(command -v chromium || command -v chromium-browser || command -v google-chrome || true)"
    [ -n "$chrome" ] || die "--provider wpc requires chromium (not found); use bgutil instead"
    if [ "$DRY" -eq 1 ]; then
        log "would create venv at $DATA_ROOT/venv with yt-dlp + yt-dlp-getpot-wpc"
        log "would write conf with youtubepot-wpc:browser_path=$chrome"
        return
    fi
    mkdir -p "$DATA_ROOT/venv"
    if [ ! -x "$DATA_ROOT/venv/bin/yt-dlp" ]; then
        run python3 -m venv "$DATA_ROOT/venv"
        run "$DATA_ROOT/venv/bin/pip" install -q -U pip
        run "$DATA_ROOT/venv/bin/pip" install -q -U "yt-dlp" "yt-dlp-getpot-wpc"
        log "dedicated venv ready (yt-dlp + yt-dlp-getpot-wpc)"
    else
        log "venv already present"
    fi
    # conf: same mweb line + browser path for wpc
    mkdir -p "$DATA_ROOT/conf"
    {
        cat "$(dirname "$0")/conf/config"
        printf -- '--extractor-args "youtubepot-wpc:browser_path=%s"\n' "$chrome"
    } > "$CONF_FILE"
    log "wpc provider configured (browser: $chrome)"
}

# ---------------------------------------------------------------------------
# verification
# ---------------------------------------------------------------------------

verify() {
    local real="$1"
    if [ "$DRY" -eq 1 ]; then
        log "would verify: plugin registration + live stream test on $TEST_URL (expect HTTP 200, not 403)"
        return
    fi
    log "verifying provider registration..."
    local out
    out="$("$BIN_DIR/yt-dlp" -v --get-id "$TEST_URL" 2>&1 | grep -i "PO Token Providers" | head -1 || true)"
    if [ -z "$out" ]; then
        warn "could not find 'PO Token Providers' line in yt-dlp verbose output"
    elif echo "$out" | grep -qi "none"; then
        warn "no PO token provider active — check --plugin-dirs path and plugin zip"
    else
        log "OK: $out"
    fi
    log "live stream test (this mints a fresh token; can take a few seconds)..."
    local data url headers_json status
    data="$(timeout 60 "$BIN_DIR/yt-dlp" -f "b[height<=1080]/best" -j "$TEST_URL" 2>/dev/null || true)"
    if [ -z "$data" ]; then
        warn "could not resolve a stream format for $TEST_URL"
        return
    fi
    url="$(printf '%s' "$data" | python3 -c 'import json,sys; print(json.load(sys.stdin)["url"])' 2>/dev/null || true)"
    headers_json="$(printf '%s' "$data" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin).get("http_headers", {})))' 2>/dev/null || true)"
    [ -n "$url" ] || { warn "no stream URL resolved"; return; }
    status="$(python3 - "$url" "$headers_json" <<'EOF'
import json, sys, urllib.request
url, hj = sys.argv[1], sys.argv[2]
req = urllib.request.Request(url, headers=json.loads(hj or "{}"))
try:
    with urllib.request.urlopen(req, timeout=30) as r:
        print(r.status)
except urllib.error.HTTPError as e:
    print(e.code)
except Exception as e:
    print(f"ERR {type(e).__name__}")
EOF
)"
    if [ "$status" = "200" ]; then
        log "OK: stream URL returns HTTP 200 (was 403 before the fix)"
    else
        warn "stream URL returned HTTP $status — see README 'Troubleshooting'"
    fi
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
        --provider) PROVIDER="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --host) HOST="$2"; shift 2 ;;
        --test-url) TEST_URL="$2"; shift 2 ;;
        --dry-run) DRY=1; shift ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

[ "$PROVIDER" = bgutil ] || [ "$PROVIDER" = wpc ] || die "provider must be bgutil or wpc"

log "$NAME v$VERSION — provider=$PROVIDER, data root=$DATA_ROOT"

if [ "$DRY" -eq 1 ]; then log "DRY RUN — nothing will be changed"; fi

if [ "$DRY" -eq 0 ]; then mkdir -p "$DATA_ROOT" "$STATE_DIR"; fi

case "$PROVIDER" in
    bgutil)
        REAL="$(find_real_ytdlp)"
        check_ytdlp_version "$REAL"
        fetch_bgutil
        install_conf
        install_wrapper "$REAL"
        install_service
        ;;
    wpc)
        install_wpc
        REAL="$DATA_ROOT/venv/bin/yt-dlp"
        check_ytdlp_version "$REAL"
        install_wrapper "$REAL"
        ;;
esac

# manifest (used by uninstall.sh / status.sh)
if [ "$DRY" -eq 0 ]; then
    {
        echo "VERSION=$VERSION"
        echo "PROVIDER=$PROVIDER"
        echo "REAL_YTDLP=$REAL"
        echo "DATA_ROOT=$DATA_ROOT"
        echo "PORT=$PORT"
        echo "HOST=$HOST"
        echo "INSTALLED=$(date -Is)"
    } > "$MANIFEST"
    log "manifest written: $MANIFEST"
fi

verify "$REAL"
log "done. Play the video in s2udio (or: mpv '$TEST_URL')."
log "Status: ./status.sh   Uninstall: ./uninstall.sh"

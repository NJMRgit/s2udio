#!/usr/bin/env bash
#
# s2u-yt — contained YouTube playback fix for mpv / s2udio.
#
# Fixes the "HTTP 403 Forbidden on googlevideo stream URLs" problem by
# provisioning a Proof-of-Origin (PO) token provider for yt-dlp, so every
# yt-dlp user on the box — mpv's embedded ytdl hook, s2udio's radio/search,
# plain CLI — gets minted PO tokens and YouTube stops 403ing playback.
#
# Since 2026-09-10 the provider is the official bgutil **Node.js** server
# (Brainicism/bgutil-ytdlp-pot-provider) run **natively** — no container:
# install.sh downloads the release source tarball, `npm ci`s the locked
# dependencies and `npx tsc`s it into $DATA_ROOT/server/build/main.js, then
# installs a systemd --user unit that runs it on 127.0.0.1:4416. The yt-dlp
# plugin + wrapper (DASH-manifest fix, URL probe, HLS safety net) make up the
# rest of the stack — see README.md.
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
#   ./install.sh [--provider bgutil|wpc] [--port 4416] [--host 127.0.0.1]
#                [--test-url URL] [--dry-run]
#
#   --provider wpc  use the browser-minted provider instead (needs Chromium;
#                   opens a browser window per yt-dlp call — legacy fallback)
#   --dry-run       print the plan without changing anything
#
# Environment overrides:
#   BGUTIL_TAG    bgutil release tag to provision          (default 2.0.0)
#   S2U_BIN_DIR   where the yt-dlp wrapper is linked       (default ~/.local/bin)
#   S2U_UNIT      systemd user unit path                   (default ~/.config/systemd/user)
#   S2U_QUADLET_DIR  podman quadlet dir checked for a same-named container unit
#                                                          (default ~/.config/containers/systemd)
#   S2U_SYSTEMD   0 = never touch systemd (test/portable installs)
#   TEST_URL      URL for the post-install live check
#
set -euo pipefail

NAME="s2u-yt"
VERSION="0.2.0"
BGUTIL_TAG="${BGUTIL_TAG:-2.0.0}"          # Brainicism/bgutil-ytdlp-pot-provider release tag
BGUTIL_REPO="Brainicism/bgutil-ytdlp-pot-provider"
MIN_YTDLP="2026.08.19"                     # DASH-manifest + post-SABR client order (2026-08-22)
MIN_NODE_MAJOR=22                          # bgutil 2.x server engine requirement
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-4416}"
TEST_URL="${TEST_URL:-https://www.youtube.com/watch?v=xz8tmSUddf8}"

DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/$NAME"
PLUGIN_ROOT="$DATA_ROOT/plugins"
PLUGIN_DIR="$PLUGIN_ROOT/bgutil-ytdlp-pot-provider"
SERVER_ROOT="$DATA_ROOT/server"
SERVER_MAIN="$SERVER_ROOT/build/main.js"
WRAPPER="$DATA_ROOT/bin/yt-dlp"
CONF_FILE="$DATA_ROOT/conf/config"
STATE_DIR="$DATA_ROOT/state"
SERVER_VERSION_FILE="$STATE_DIR/server-version"
MANIFEST="$STATE_DIR/manifest"
UNIT="${S2U_UNIT:-$HOME/.config/systemd/user/$NAME-bgutil.service}"
QUADLET_DIR="${S2U_QUADLET_DIR:-$HOME/.config/containers/systemd}"
BIN_DIR="${S2U_BIN_DIR:-$HOME/.local/bin}"
BACKUP="$BIN_DIR/.yt-dlp.$NAME.bak"

PROVIDER="${PROVIDER:-bgutil}"
SYSTEMD="${S2U_SYSTEMD:-1}"
DRY=0
NODE_BIN=""

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
    # The system yt-dlp the wrapper will exec. Never resolve to an s2u wrapper.
    local p
    if [ -f "$MANIFEST" ]; then
        local saved; saved="$(awk -F= '/^REAL_YTDLP=/{print $2}' "$MANIFEST" 2>/dev/null || true)"
        [ -n "$saved" ] && [ -x "$saved" ] && { echo "$saved"; return; }
    fi
    p="$(command -v yt-dlp 2>/dev/null || true)"
    if [ -n "$p" ]; then
        p="$(readlink -f "$p")"
        # skip the package's own wrapper, any s2u wrapper, and any install of
        # this package (otherwise the wrapper would exec itself)
        case "$p" in
            *"/s2u-yt/bin/yt-dlp" | "$HOME"/.local/share/s2u-*/bin/yt-dlp) p="" ;;
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

check_node() {
    # The bgutil 2.x provider server is the official Node.js server; node+npm
    # are build-time AND run-time requirements (npm runs the tsc build here).
    require_cmd node
    require_cmd npm
    NODE_BIN="$(command -v node)"
    local maj
    maj="$(node -e 'process.stdout.write(process.versions.node.split(".")[0])')"
    python3 - "$maj" "$MIN_NODE_MAJOR" <<'EOF' || die "node $(node --version) is too old (bgutil server needs node >= $MIN_NODE_MAJOR)"
import sys
sys.exit(0 if int(sys.argv[1]) >= int(sys.argv[2]) else 1)
EOF
    log "using node $(node --version) at $NODE_BIN (bgutil server runtime)"
}

require_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }

wait_for_server() {
    # The node server needs a moment to start — and on a host migrating from a
    # containerized provider it may briefly retry while the old process
    # releases the port (Restart=on-failure). Poll before crying failure.
    local i=0
    while [ "$i" -lt 20 ]; do
        if curl -fsS --max-time 2 "http://$HOST:$PORT/ping" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    return 1
}

# ---------------------------------------------------------------------------
# provisioning
# ---------------------------------------------------------------------------

fetch_server() {
    # Official bgutil Node.js server, built natively from the release source.
    require_cmd curl
    require_cmd tar
    local tag="$BGUTIL_TAG"
    local tarball_url="https://github.com/$BGUTIL_REPO/archive/refs/tags/$tag.tar.gz"

    if [ "$DRY" -eq 1 ]; then
        log "would download $tarball_url"
        log "would build it: npm ci --no-audit --no-fund && npx tsc  ->  $SERVER_MAIN"
        return
    fi
    mkdir -p "$SERVER_ROOT" "$STATE_DIR"

    if [ -f "$SERVER_MAIN" ] && [ "$(cat "$SERVER_VERSION_FILE" 2>/dev/null || true)" = "$tag" ]; then
        log "bgutil server already provisioned (v$tag)"
        return
    fi

    log "provisioning bgutil server v$tag (download + npm ci + npx tsc; first run takes a few minutes)"
    local tarball="$STATE_DIR/bgutil-$tag.tar.gz"
    local src_root="$STATE_DIR/bgutil-src-$tag"
    run curl -fL --retry 3 -o "$tarball" "$tarball_url"
    rm -rf "$src_root"; mkdir -p "$src_root"
    run tar -xzf "$tarball" -C "$src_root"

    local srcdir
    srcdir="$(find "$src_root" -mindepth 1 -maxdepth 2 -type d -name server 2>/dev/null | head -1 || true)"
    [ -n "$srcdir" ] || die "no server/ directory in the $tag tarball"

    rm -rf "$SERVER_ROOT"
    mkdir -p "$SERVER_ROOT"
    run cp -a "$srcdir/." "$SERVER_ROOT/"

    ( cd "$SERVER_ROOT" && npm ci --no-audit --no-fund ) \
        || die "npm ci failed (see README 'Requirements': node >= $MIN_NODE_MAJOR, npm >= 9, and build tools for the 'canvas' native module if no prebuilt binary exists)"
    ( cd "$SERVER_ROOT" && npx tsc ) \
        || die "npx tsc failed (see README 'Troubleshooting')"
    [ -f "$SERVER_MAIN" ] || die "build produced no $SERVER_MAIN"
    echo "$tag" > "$SERVER_VERSION_FILE"
    log "server built: $SERVER_MAIN (v$tag)"
}

install_plugins() {
    # The yt-dlp plugins ship inside the package (offline-friendly). Layout:
    # plugins/bgutil-ytdlp-pot-provider/yt_dlp_plugins/extractor/*.py
    local src="$(dirname "$0")/plugins/bgutil-ytdlp-pot-provider"
    if [ "$DRY" -eq 1 ]; then
        log "would install plugins from $src -> $PLUGIN_DIR"
        return
    fi
    mkdir -p "$PLUGIN_ROOT"
    if [ -f "$src/yt_dlp_plugins/extractor/getpot_bgutil_http.py" ]; then
        rm -rf "$PLUGIN_DIR"
        run cp -a "$src" "$PLUGIN_DIR"
    else
        warn "package plugin dir missing — falling back to the GitHub release zip"
        require_cmd curl
        local zip="$STATE_DIR/bgutil-ytdlp-pot-provider.zip"
        rm -rf "$PLUGIN_DIR"
        run curl -fL --retry 3 -o "$zip" \
            "https://github.com/$BGUTIL_REPO/releases/download/$BGUTIL_TAG/bgutil-ytdlp-pot-provider.zip"
        run python3 -m zipfile -e "$zip" "$PLUGIN_ROOT"
        run mv "$PLUGIN_ROOT/yt_dlp_plugins" "$PLUGIN_DIR"
        rm -f "$zip"
    fi
    log "yt-dlp plugins: $PLUGIN_DIR"
}

install_bin() {
    # Helper scripts: the media-URL probe (wrapper phase checks).
    # bin/yt-dlp-wrap.sh is the wrapper template and is consumed by
    # install_wrapper, not copied here.
    local src="$(dirname "$0")/bin"
    if [ "$DRY" -eq 1 ]; then
        log "would install helpers from $src -> $DATA_ROOT/bin"
        return
    fi
    mkdir -p "$DATA_ROOT/bin"
    local f
    for f in s2u-yt-probe.py; do
        if [ -f "$src/$f" ]; then
            run cp "$src/$f" "$DATA_ROOT/bin/$f"
            run chmod +x "$DATA_ROOT/bin/$f"
        else
            warn "package file missing: $src/$f"
        fi
    done
    # Retire helpers earlier releases installed (the VR-OAuth token helper
    # went away with that route).
    if [ -e "$DATA_ROOT/bin/vr-oauth-token.sh" ]; then
        run rm -f "$DATA_ROOT/bin/vr-oauth-token.sh"
        log "removed obsolete helper: $DATA_ROOT/bin/vr-oauth-token.sh"
    fi
    log "helpers installed: $DATA_ROOT/bin/s2u-yt-probe.py"
}

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
        printf '# s2u-yt managed config\n# See s2u-yt/conf/config in the package: no player_client is pinned here;\n# the wrapper chooses the client per phase (anon -> cookies -> optional\n# android_vr + OAuth -> web_safari HLS) and strips the deprecated\n# --youtube-skip-dash-manifest flag.\n' > "$CONF_FILE"
    fi
    log "config written: $CONF_FILE"
}

install_wrapper() {
    local real="$1"
    local tpl="$(dirname "$0")/bin/yt-dlp-wrap.sh"
    if [ "$DRY" -eq 1 ]; then
        log "would render $tpl -> $WRAPPER (X execs $real)"
        log "would replace $BIN_DIR/yt-dlp (backup -> $BACKUP)"
        return
    fi
    [ -f "$tpl" ] || die "wrapper template missing: $tpl"
    mkdir -p "$DATA_ROOT/bin"
    python3 - "$tpl" "$WRAPPER" "$real" "$NAME" <<'PY'
import sys
tpl, out, real, name = sys.argv[1:5]
text = open(tpl).read()
if '@REAL_YTDLP@' not in text or '@NAME@' not in text:
    raise SystemExit(f'{tpl}: template placeholders missing')
text = text.replace('@REAL_YTDLP@', real).replace('@NAME@', name)
with open(out, 'w') as fh:
    fh.write(text)
PY
    chmod +x "$WRAPPER"
    log "wrapper written: $WRAPPER"

    if [ -e "$BIN_DIR/yt-dlp" ]; then
        [ -e "$BACKUP" ] && warn "overwriting previous backup $BACKUP"
        mv -f "$BIN_DIR/yt-dlp" "$BACKUP"
        log "previous yt-dlp preserved at $BACKUP"
    fi
    mkdir -p "$BIN_DIR"
    ln -sf "$WRAPPER" "$BIN_DIR/yt-dlp"
    log "installed wrapper at $BIN_DIR/yt-dlp"
}

install_service() {
    if [ "$DRY" -eq 1 ]; then
        log "would write unit $UNIT:  ExecStart=$NODE_BIN $SERVER_MAIN --host $HOST --port $PORT"
        log "would enable and start $NAME-bgutil.service (systemd --user)"
        return
    fi
    mkdir -p "$(dirname "$UNIT")"

    # A same-named podman quadlet (the earlier containerized test setup) would
    # shadow this unit on the next daemon-reload — retire it first.
    local quadlet="$QUADLET_DIR/$NAME-bgutil.container"
    if [ -f "$quadlet" ]; then
        log "podman quadlet found — disabling it so the native server owns the unit name"
        if [ "$SYSTEMD" -eq 1 ] && command -v systemctl >/dev/null 2>&1; then
            systemctl --user disable --now "$NAME-bgutil.service" >/dev/null 2>&1 || true
            systemctl --user daemon-reload >/dev/null 2>&1 || true
        fi
        mv -f "$quadlet" "$quadlet.bak-native-$(date +%Y%m%d)"
        log "quadlet retired: $quadlet.bak-native-$(date +%Y%m%d)"
    fi

    cat > "$UNIT" <<EOF
[Unit]
Description=$NAME bgutil PO token provider (YouTube) - native node 2.x server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$NODE_BIN $SERVER_MAIN --host $HOST --port $PORT
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF
    log "service unit written: $UNIT"

    if [ "$SYSTEMD" = 1 ] && command -v systemctl >/dev/null 2>&1 \
        && [ -n "$(systemctl --user is-system-running 2>/dev/null || true)" ]; then
        systemctl --user daemon-reload
        systemctl --user enable --now "$NAME-bgutil.service"
        if wait_for_server; then
            log "service enabled and started (systemd --user)"
        else
            warn "service enabled but the server is not answering on http://$HOST:$PORT yet"
            warn "check it: systemctl --user status $NAME-bgutil.service"
        fi
    else
        warn "systemd user session not available (or S2U_SYSTEMD=0); skipping auto-start"
        warn "start the server manually: nohup $NODE_BIN $SERVER_MAIN --host $HOST --port $PORT &"
    fi
}

install_wpc() {
    # Legacy fallback provider: yt-dlp-getpot-wpc (mints tokens in a real Chromium).
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
        log "would verify: server /ping + plugin registration + live stream test on $TEST_URL"
        return
    fi

    log "verifying token server..."
    wait_for_server || true
    local ping ver
    ping="$(curl -fsS --max-time 5 "http://$HOST:$PORT/ping" 2>/dev/null || true)"
    ver="$(printf '%s' "$ping" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("version",""))' 2>/dev/null || true)"
    if [ -n "$ver" ]; then
        log "OK: bgutil server v$ver answering on http://$HOST:$PORT"
    else
        warn "no answer from http://$HOST:$PORT/ping — is s2u-yt-bgutil.service running?"
    fi

    log "verifying provider registration..."
    local out
    out="$("$BIN_DIR/yt-dlp" -v --get-id "$TEST_URL" 2>&1 | grep -i "PO Token Providers" | head -1 || true)"
    if [ -z "$out" ]; then
        warn "could not find 'PO Token Providers' line in yt-dlp verbose output"
    elif echo "$out" | grep -qi "none"; then
        warn "no PO token provider active — check --plugin-dirs path and $PLUGIN_DIR"
    else
        log "OK: $out"
    fi

    log "live stream test (this mints a fresh token; can take a few seconds)..."
    local data url headers_json status
    data="$(timeout 90 "$BIN_DIR/yt-dlp" -f "b[height<=1080]/best" -j "$TEST_URL" 2>/dev/null || true)"
    if [ -z "$data" ]; then
        warn "could not resolve a stream format for $TEST_URL (a YouTube bot-check window shows up like this)"
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
        -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
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
        check_node
        fetch_server
        install_plugins
        install_bin
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
        echo "BGUTIL_TAG=$BGUTIL_TAG"
        echo "NODE_BIN=$NODE_BIN"
        echo "INSTALLED=$(date -Is)"
    } > "$MANIFEST"
    log "manifest written: $MANIFEST"
fi

verify "$REAL"
log "done. Play the video in s2udio (or: mpv '$TEST_URL')."
log "Status: ./status.sh   Uninstall: ./uninstall.sh"
log "Fallback ladder: anonymous -> cookies -> mweb + PO token -> HLS (see README)"

#!/usr/bin/env bash
#
# s2u-yt — status / health check.
#
# Checks, in order:
#   1. wrapper installed and pointing at the package
#   2. token server reachable and its version (native bgutil node server) /
#      venv present (wpc mode)
#   3. yt-dlp sees the PO token provider plugin
#   4. live stream test: resolved googlevideo URL returns HTTP 200 (not 403)
#   5. optional VR-OAuth route: configured / not configured
#
# Usage: ./status.sh [--test-url URL]
set -euo pipefail

NAME="s2u-yt"
DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/$NAME"
MANIFEST="$DATA_ROOT/state/manifest"
WRAPPER="$DATA_ROOT/bin/yt-dlp"
BIN_DIR="${S2U_BIN_DIR:-$HOME/.local/bin}"
VR_TOKEN="$HOME/.config/s2u-yt/vr-oauth.json"
PORT="${PORT:-4416}"
HOST="${HOST:-127.0.0.1}"
TEST_URL="${TEST_URL:-https://www.youtube.com/watch?v=xz8tmSUddf8}"

[ "${1:-}" = "--test-url" ] && { TEST_URL="$2"; shift 2; }

ok()   { printf '\033[1;32m  [ok]  \033[0m %s\n' "$*"; }
bad()  { printf '\033[1;31m  [FAIL]\033[0m %s\n' "$*"; }
info() { printf '\033[1;34m  [info]\033[0m %s\n' "$*"; }

PROVIDER="$(awk -F= '/^PROVIDER=/{print $2}' "$MANIFEST" 2>/dev/null || echo unknown)"
PORT="$(awk -F= '/^PORT=/{print $2}' "$MANIFEST" 2>/dev/null || echo "$PORT")"
HOST="$(awk -F= '/^HOST=/{print $2}' "$MANIFEST" 2>/dev/null || echo "$HOST")"
BGUTIL_TAG="$(awk -F= '/^BGUTIL_TAG=/{print $2}' "$MANIFEST" 2>/dev/null || true)"

echo "== $NAME status (provider: ${PROVIDER:-unknown}${BGUTIL_TAG:+, bgutil $BGUTIL_TAG}) =="

# 1. wrapper
if [ -L "$BIN_DIR/yt-dlp" ] && [ "$(readlink -f "$BIN_DIR/yt-dlp")" = "$WRAPPER" ]; then
    ok "wrapper active: $BIN_DIR/yt-dlp -> $WRAPPER"
else
    bad "wrapper not active (expected $BIN_DIR/yt-dlp -> $WRAPPER)"
fi

# 2. server / venv
if [ "$PROVIDER" = wpc ]; then
    if [ -x "$DATA_ROOT/venv/bin/yt-dlp" ]; then
        ok "wpc venv present"
    else
        bad "wpc venv missing"
    fi
else
    ping="$(curl -fsS --max-time 3 "http://$HOST:$PORT/ping" 2>/dev/null || true)"
    ver="$(printf '%s' "$ping" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("version",""))' 2>/dev/null || true)"
    if [ -n "$ver" ]; then
        ok "token server v$ver reachable at http://$HOST:$PORT/ping (native node bgutil)"
    else
        bad "token server NOT reachable at http://$HOST:$PORT/ping (is the service running?)"
        info "start it: systemctl --user start $NAME-bgutil.service"
    fi
fi

# 3. plugin registration
if [ -x "$BIN_DIR/yt-dlp" ]; then
    # --get-id: the -v probe must not download the test video (a bare call
    # would save a copy of a live stream into the current directory).
    providers="$("$BIN_DIR/yt-dlp" -v --get-id "$TEST_URL" 2>&1 | grep -i "PO Token Providers" | head -1 || true)"
    if [ -z "$providers" ]; then
        bad "could not find PO token provider line in yt-dlp verbose output"
    elif echo "$providers" | grep -qi "none"; then
        bad "yt-dlp reports NO PO token providers"
        info "check plugin dir: $DATA_ROOT/plugins"
    else
        ok "$providers"
    fi
fi

# 4. live stream test (wrapper phases: anon -> cookies -> optional android_vr
#    + OAuth -> web_safari HLS; the log tells which one answered)
echo "-- live stream test ($TEST_URL)"
# Probe the AUDIO path (-f bestaudio/best) — what s2udio/MPD consume.
data="$(timeout 90 "$BIN_DIR/yt-dlp" -f "bestaudio/best" -j "$TEST_URL" 2>/dev/null || true)"
if [ -z "$data" ]; then
    bad "could not resolve a stream format"
    info "a YouTube bot-check window looks like this; check /tmp/s2u-yt-wrapper.log"
    exit 1
fi
url="$(printf '%s' "$data" | python3 -c 'import json,sys; print(json.load(sys.stdin)["url"])' 2>/dev/null || true)"
headers_json="$(printf '%s' "$data" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin).get("http_headers", {})))' 2>/dev/null || true)"
[ -n "$url" ] || { bad "no stream URL resolved"; exit 1; }
if printf '%s' "$url" | grep -qE 'm3u8|/manifest/hls'; then
    info "resolved via an HLS manifest (the web_safari safety net — DASH paths were 403ing)"
else
    ok "resolved a progressive/DASH single-file URL (range-seekable)"
fi
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
    ok "stream URL returns HTTP 200 (playback should work)"
else
    bad "stream URL returned HTTP $status — see README 'Troubleshooting'"
fi

# 5. optional VR-OAuth route
if [ -f "$VR_TOKEN" ]; then
    if "$DATA_ROOT/bin/vr-oauth-token.sh" status >/dev/null 2>&1; then
        ok "VR OAuth configured ($VR_TOKEN) — android_vr + Bearer phase enabled"
    else
        info "VR OAuth token file present but not usable — re-run: $DATA_ROOT/bin/vr-oauth-token.sh reinit"
    fi
else
    info "VR OAuth not configured (optional full-quality route; see README 'VR OAuth')"
fi

echo "== done =="

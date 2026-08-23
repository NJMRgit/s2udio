#!/usr/bin/env bash
#
# s2u-yt — status / health check.
#
# Checks, in order:
#   1. wrapper installed and pointing at the package
#   2. provider server reachable (bgutil mode) / venv present (wpc mode)
#   3. yt-dlp sees the PO token provider plugin
#   4. live stream test: resolved googlevideo URL returns HTTP 200 (not 403)
#
# Usage: ./status.sh [--test-url URL]
set -euo pipefail

NAME="s2u-yt"
DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/$NAME"
MANIFEST="$DATA_ROOT/state/manifest"
WRAPPER="$DATA_ROOT/bin/yt-dlp"
BIN_DIR="$HOME/.local/bin"
PORT="${PORT:-4416}"
HOST="${HOST:-127.0.0.1}"
TEST_URL="${TEST_URL:-https://www.youtube.com/watch?v=xz8tmSUddf8}"

[ "${1:-}" = "--test-url" ] && { TEST_URL="$2"; shift 2; }

ok()   { printf '\033[1;32m  [ok]  \033[0m %s\n' "$*"; }
bad()  { printf '\033[1;31m  [FAIL]\033[0m %s\n' "$*"; }
info() { printf '\033[1;34m  [info]\033[0m %s\n' "$*"; }

PROVIDER="$(awk -F= '/^PROVIDER=/{print $2}' "$MANIFEST" 2>/dev/null || echo unknown)"
PORT="$(awk -F= '/^PORT=/{print $2}' "$MANIFEST" 2>/dev/null || echo "$PORT")"

echo "== $NAME status (provider: ${PROVIDER:-unknown}) =="

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
    if curl -fsS --max-time 3 "http://$HOST:$PORT/ping" >/dev/null 2>&1; then
        ok "provider server reachable at http://$HOST:$PORT/ping"
    else
        bad "provider server NOT reachable at http://$HOST:$PORT/ping (is the service running?)"
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

# 4. live stream test
#    (also reports which client won: since 2026-08-22 the conf pins no
#    client — anonymous defaults reach VISIONOS; HLS manifests mean the
#    anonymous pass fell back to an authenticated client)
echo "-- live stream test ($TEST_URL)"
# Probe the AUDIO path (-f bestaudio/best) — what s2udio/MPD consume.
# Video-only DASH formats are hit-or-miss under 2026-08 SABR enforcement;
# don't gate on them here.
data="$(timeout 60 "$BIN_DIR/yt-dlp" -f "bestaudio/best" -j "$TEST_URL" 2>/dev/null || true)"
if [ -z "$data" ]; then
    bad "could not resolve a stream format"
    exit 1
fi
url="$(printf '%s' "$data" | python3 -c 'import json,sys; print(json.load(sys.stdin)["url"])' 2>/dev/null || true)"
headers_json="$(printf '%s' "$data" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin).get("http_headers", {})))' 2>/dev/null || true)"
[ -n "$url" ] || { bad "no stream URL resolved"; exit 1; }
if printf '%s' "$url" | grep -qE 'm3u8|/manifest/hls'; then
    info "resolved via an HLS manifest (HLS clients are fallback-only now)"
    info "check the bgutil service: systemctl --user status $NAME-bgutil.service"
else
    ok "resolved a progressive single-file URL (range-seekable)"
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

echo "== done =="

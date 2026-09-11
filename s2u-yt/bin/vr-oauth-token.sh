#!/usr/bin/env bash
#
# vr-oauth-token.sh — one-time Android-VR (ANDROID_VR) OAuth login and
# on-demand access-token refresh for the s2u-yt VR OAuth route.
#
# Implements Google's OAuth 2.0 device authorization grant (RFC 8628) with the
# YouTube VR app's embedded public client credentials, i.e. the same route the
# SmartTube / ReVanced "Android VR (Auth)" clients use. The resulting
# access token is handed to yt-dlp by the s2u-yt wrapper as
#   S2U_VR_ACCESS_TOKEN=... yt-dlp --extractor-args youtube:player_client=android_vr
# and the bundled yt-dlp plugin puts it only on InnerTube API requests.
#
# Usage:
#   vr-oauth-token.sh init      start the device flow: prints https://yt.be/activate
#                               and an activation code, then polls until approved
#                               and stores the refresh token (chmod 600)
#   vr-oauth-token.sh reinit    same, after discarding an existing login
#   vr-oauth-token.sh --token   print a usable access token (refreshes when the
#                               cached one has < 5 min left); exit 1 if not logged in
#   vr-oauth-token.sh status    show login/token state
#
# Store: ~/.config/s2u-yt/vr-oauth.json (override the dir with S2U_VR_CONF_DIR).
#
# NOTE: this violates YouTube's ToS — use a throwaway Google account. See the
# README section "Optional full-quality auth (VR OAuth)".
set -euo pipefail

CONF_DIR="${S2U_VR_CONF_DIR:-$HOME/.config/s2u-yt}"
TOKEN_FILE="$CONF_DIR/vr-oauth.json"
PENDING_FILE="$CONF_DIR/vr-pending.json"

CLIENT_ID="652469312169-4lvs9bnhr9lpns9v451j5oivd81vjvu1.apps.googleusercontent.com"
CLIENT_SECRET="3fTWrBJI5Uojm1TK7_iJCW5Z"
SCOPE="https://www.googleapis.com/auth/youtube"
DEVICE_URL="https://www.youtube.com/o/oauth2/device/code?prettyPrint=false"
TOKEN_URL="https://www.youtube.com/o/oauth2/token?prettyPrint=false"
ACTIVATE_URL="https://yt.be/activate"
VR_UA="com.google.android.apps.youtube.vr.oculus/1.47.48(Linux; U; Android 10; en_US; Quest Build/QQ3A.200805.001) gzip"

POLL_EVERY=5          # seconds between token-endpoint polls
POLL_MAX=600          # give up after 10 min (user codes live ~30 min)
REFRESH_MARGIN=300    # refresh when the cached token has < 5 min left

err() { printf 'vr-oauth: %s\n' "$*" >&2; }

# json_get <file> <key>  — print a top-level key of a JSON file; exit 1 when absent
json_get() {
    python3 - "$1" "$2" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except Exception:
    raise SystemExit(1)
value = data.get(sys.argv[2])
if value is None:
    raise SystemExit(1)
print(value)
PY
}

# json_field <key>  — print a top-level key of the JSON on stdin; exit 1 when absent
# (separate from json_get because a heredoc would consume the pipe)
json_field() {
    python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)
value = data.get(sys.argv[1])
if value is None:
    raise SystemExit(1)
print(value)
' "$1"
}

# json_store <file> <token-json> <now> <expires_at>  — merge tokens into the store
json_store() {
    python3 - "$1" "$2" "$3" "$4" <<'PY'
import json, os, sys
path, raw, now, expires_at = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
try:
    fresh = json.loads(raw)
except Exception as e:
    raise SystemExit(f'cannot parse token response: {e}')
store = {}
try:
    store = json.load(open(path))
except Exception:
    pass
if fresh.get('refresh_token'):
    store['refresh_token'] = fresh['refresh_token']
if fresh.get('access_token'):
    store['access_token'] = fresh['access_token']
store['access_token_expires_at'] = expires_at
store['updated_at'] = now
store['token_type'] = fresh.get('token_type', store.get('token_type', 'Bearer'))
# never write the token with group/other permissions
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, 'w') as fh:
    json.dump(store, fh, indent=2)
    fh.write('\n')
os.chmod(path, 0o600)
PY
}

do_init() {
    mkdir -p "$CONF_DIR"
    chmod 700 "$CONF_DIR" 2>/dev/null || true
    if [ -f "$TOKEN_FILE" ] && [ "${1:-}" != "--force" ]; then
        err "already logged in ($TOKEN_FILE) — use 'reinit' to start over"
        exit 1
    fi
    if [ "${1:-}" = "--force" ]; then rm -f "$TOKEN_FILE"; fi

    local device_id device_resp device_code user_code
    device_id="$(cat /proc/sys/kernel/random/uuid 2>/dev/null || echo "vr-$$-$(date +%s)")"
    device_resp="$(curl -fsS -X POST "$DEVICE_URL" \
        -H 'Content-Type: application/json; charset=utf-8' \
        -H "User-Agent: $VR_UA" \
        -d "{\"client_id\":\"$CLIENT_ID\",\"scope\":\"$SCOPE\",\"device_id\":\"$device_id\",\"device_model\":\"QUEST1\"}")" \
        || { err "device-code request failed (network?)"; exit 1; }
    device_code="$(printf '%s' "$device_resp" | json_field device_code || true)"
    user_code="$(printf '%s' "$device_resp" | json_field user_code || true)"
    if [ -z "$device_code" ] || [ -z "$user_code" ]; then
        err "no device_code/user_code in response: $device_resp"
        exit 1
    fi
    printf '{"device_code":"%s","user_code":"%s","created_at":%s}\n' \
        "$device_code" "$user_code" "$(date +%s)" > "$PENDING_FILE"
    chmod 600 "$PENDING_FILE"

    cat <<EOF

== s2u-yt VR OAuth — one-time login ==

  1. Open in any browser:      $ACTIVATE_URL
  2. Enter activation code:    $user_code
  3. Sign in with a THROWAWAY Google account and approve.

  Polling until approved (max $POLL_MAX s; Ctrl-C is safe — re-run 'init'
  for a fresh code). The access token is short-lived (~1 h) and refreshes
  automatically afterwards.

EOF

    local elapsed=0
    while [ "$elapsed" -lt "$POLL_MAX" ]; do
        sleep "$POLL_EVERY"
        elapsed=$((elapsed + POLL_EVERY))
        local tok_resp now exp
        tok_resp="$(curl -sS -X POST "$TOKEN_URL" \
            -H 'Content-Type: application/json; charset=utf-8' \
            -d "{\"client_id\":\"$CLIENT_ID\",\"client_secret\":\"$CLIENT_SECRET\",\"code\":\"$device_code\",\"grant_type\":\"http://oauth.net/grant_type/device/1.0\"}" \
            2>/dev/null || true)"
        if printf '%s' "$tok_resp" | grep -q '"access_token"'; then
            now="$(date +%s)"
            exp=$(( now + $(printf '%s' "$tok_resp" | json_field expires_in 2>/dev/null || echo 3599) ))
            json_store "$TOKEN_FILE" "$tok_resp" "$now" "$exp"
            rm -f "$PENDING_FILE"
            printf '\nOK — VR OAuth configured (%s).\n' "$TOKEN_FILE"
            printf 'The s2u-yt wrapper now has an optional android_vr + Bearer phase.\n'
            exit 0
        fi
        case "$tok_resp" in
            *authorization_pending*) printf '.' ;;
            *slow_down*)             printf '.' ;;
            *access_denied*)         printf '\n'; err "access denied in the browser step"; exit 1 ;;
            *expired_device_code*)   printf '\n'; err "the activation code expired — re-run init"; exit 1 ;;
            *)                       printf '\n'; err "token endpoint said: $tok_resp"; exit 1 ;;
        esac
    done
    printf '\n'
    err "timed out waiting for approval — re-run init for a fresh code"
    exit 1
}

do_token() {
    [ -f "$TOKEN_FILE" ] || { err "not configured ($TOKEN_FILE missing) — run: vr-oauth-token.sh init"; exit 1; }
    local now expires_at access
    now="$(date +%s)"
    expires_at="$(json_get "$TOKEN_FILE" access_token_expires_at || echo 0)"
    access="$(json_get "$TOKEN_FILE" access_token || true)"
    if [ -n "$access" ] && [ "$expires_at" -gt $((now + REFRESH_MARGIN)) ]; then
        printf '%s\n' "$access"
        exit 0
    fi
    local refresh
    refresh="$(json_get "$TOKEN_FILE" refresh_token || true)"
    [ -n "$refresh" ] || { err "no refresh_token stored — re-run: vr-oauth-token.sh reinit"; exit 1; }
    local resp exp
    resp="$(curl -sS -X POST "$TOKEN_URL" \
        -H 'Content-Type: application/json; charset=utf-8' \
        -d "{\"client_id\":\"$CLIENT_ID\",\"client_secret\":\"$CLIENT_SECRET\",\"refresh_token\":\"$refresh\",\"grant_type\":\"refresh_token\"}" \
        2>/dev/null || true)"
    if [ -z "$resp" ] || ! printf '%s' "$resp" | grep -q '"access_token"'; then
        err "token refresh failed: ${resp:-no response}"
        exit 1
    fi
    exp=$(( now + $(printf '%s' "$resp" | json_field expires_in 2>/dev/null || echo 3599) ))
    json_store "$TOKEN_FILE" "$resp" "$now" "$exp"
    json_get "$TOKEN_FILE" access_token
}

do_status() {
    if [ ! -f "$TOKEN_FILE" ]; then
        echo "VR OAuth: not configured ($TOKEN_FILE missing)"
        echo "  optional full-quality route — see README 'VR OAuth'"
        exit 1
    fi
    local now expires_at left
    now="$(date +%s)"
    expires_at="$(json_get "$TOKEN_FILE" access_token_expires_at || echo 0)"
    left=$((expires_at - now))
    if [ "$left" -gt 0 ]; then
        echo "VR OAuth: configured (cached access token valid for another ${left}s; auto-refresh on demand)"
    else
        echo "VR OAuth: configured (cached access token expired — the next yt-dlp run refreshes it)"
    fi
    if ! json_get "$TOKEN_FILE" refresh_token >/dev/null 2>&1; then
        echo "  warning: no refresh_token stored — run 'reinit'"
    fi
    exit 0
}

case "${1:---token}" in
    init)         do_init "$@" ;;
    reinit)       do_init --force ;;
    --token|-t|token) do_token ;;
    status)       do_status ;;
    -h|--help|help) sed -n '2,28p' "$0" ;;
    *)            err "unknown command: $1 (init | reinit | --token | status)"; exit 1 ;;
esac

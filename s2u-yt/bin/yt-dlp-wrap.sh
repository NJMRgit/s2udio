#!/bin/bash
# s2u-yt managed wrapper — generated from bin/yt-dlp-wrap.sh by install.sh; do not edit.
#
# Resolution phases for one yt-dlp call (see conf/config):
#   P1  anonymous, unpinned client — cookie options stripped, buffered and
#       discarded on failure (a failed pass still prints a leading "null",
#       which corrupts the stdout JSON parsers of s2udio / mpv / CLI users).
#   P2  authenticated (your cookies) — output probed; on 403 falls through.
#   P2b VR OAuth (optional) — android_vr + OAuth Bearer, only when
#       ~/.config/s2u-yt/vr-oauth.json exists (see README "VR OAuth").
#   P3  web_safari HLS safety net (max 1080p60).
#
# v2026-09-10: drops the deprecated `--youtube-skip-dash-manifest` flag from
# consumer args — with it, the player-response URLs fail googlevideo's redirect
# validation (302 -> 403) while DASH-manifest URLs answer 206 (A/B verified
# 3/3, wrapper runs 6/6). Decisions are logged to /tmp/s2u-yt-wrapper.log.
ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/@NAME@"
USER_CONF="$HOME/.config/yt-dlp/config"
CONF="$ROOT/conf/config"
PLUGINS="$ROOT/plugins"
PROBE="$ROOT/bin/s2u-yt-probe.py"
VR_BIN="$ROOT/bin/vr-oauth-token.sh"
VR_TOKEN="$HOME/.config/s2u-yt/vr-oauth.json"
LOG="/tmp/s2u-yt-wrapper.log"
YTDLP="@REAL_YTDLP@"

log() { echo "$(date -Is) $*" >> "$LOG" 2>/dev/null || true; }

# strip the deprecated skip-dash-manifest flag; every other argument passes
# through byte-for-byte (no reordering, no insertion).
filter_args() {
  NEW_ARGS=()
  for a in "$@"; do
    case "$a" in
      --youtube-skip-dash-manifest) log "DROP deprecated --youtube-skip-dash-manifest" ;;
      *) NEW_ARGS+=("$a") ;;
    esac
  done
}

if [ -f "$USER_CONF" ]; then
    filter_args "$@"

    # ---- P1: anonymous (cookie options stripped), default client order ----
    ANON_CONF="$ROOT/conf/user-config.anon"
    awk '
        /^[[:space:]]*--cookies(-from-browser)?([[:space:]]|$)/ { skip = 1; next }
        skip && /^[^-]/ { skip = 0; next }
        { skip = 0; print }
    ' "$USER_CONF" > "$ANON_CONF" 2>/dev/null || :
    # --ignore-config: the default ~/.config/yt-dlp/config must NOT leak its
    # cookies into this pass (only the sanitized config applies).
    TMP="$(mktemp "${TMPDIR:-/tmp}/s2u-yt.XXXXXX")"
    "$YTDLP" --ignore-config --config-locations "$ANON_CONF" --config-locations "$CONF" --plugin-dirs "$PLUGINS" "${NEW_ARGS[@]}" >"$TMP"
    status=$?
    if [ "$status" -eq 0 ]; then
        cat "$TMP"; rm -f "$TMP"; exit 0
    fi
    rm -f "$TMP"

    # ---- P2: authenticated retry (your cookies); probe the media URLs ----
    TMP2="$(mktemp "${TMPDIR:-/tmp}/s2u-yt.XXXXXX")"
    "$YTDLP" --ignore-config --config-locations "$USER_CONF" --config-locations "$CONF" --plugin-dirs "$PLUGINS" "${NEW_ARGS[@]}" >"$TMP2"
    status=$?
    if [ "$status" -eq 0 ]; then
        if grep -q googlevideo "$TMP2"; then
            "$PROBE" "$TMP2" 2>/dev/null; rc=$?
            if [ "$rc" -ne 1 ]; then
                cat "$TMP2"; rm -f "$TMP2"; exit 0
            fi
            log "PHASE2_PROBE403 args=$*"
        else
            cat "$TMP2"; rm -f "$TMP2"; exit 0
        fi
    else
        log "PHASE2_FAIL exit=$status args=$*"
    fi
    rm -f "$TMP2"

    # ---- P2b: VR OAuth (android_vr + Bearer) — only when configured ----
    if [ -f "$VR_TOKEN" ] && [ -x "$VR_BIN" ]; then
        ACCESS="$("$VR_BIN" --token 2>/dev/null || true)"
        if [ -n "$ACCESS" ]; then
            # no cookies in this pass: android_vr + the bearer only
            TMP3="$(mktemp "${TMPDIR:-/tmp}/s2u-yt.XXXXXX")"
            S2U_VR_ACCESS_TOKEN="$ACCESS" "$YTDLP" --ignore-config --config-locations "$ANON_CONF" --config-locations "$CONF" --plugin-dirs "$PLUGINS" --extractor-args "youtube:player_client=android_vr" "${NEW_ARGS[@]}" >"$TMP3"
            status=$?
            if [ "$status" -eq 0 ]; then
                if grep -q googlevideo "$TMP3"; then
                    "$PROBE" "$TMP3" 2>/dev/null; rc=$?
                    if [ "$rc" -ne 1 ]; then
                        log "VR_OAUTH_OK client=android_vr args=$*"
                        cat "$TMP3"; rm -f "$TMP3"; exit 0
                    fi
                    log "VR_OAUTH_PROBE403 args=$*"
                else
                    log "VR_OAUTH_OK client=android_vr args=$*"
                    cat "$TMP3"; rm -f "$TMP3"; exit 0
                fi
            else
                log "VR_OAUTH_FAIL exit=$status args=$*"
            fi
            rm -f "$TMP3"
        else
            log "VR_OAUTH_SKIP no-access-token args=$*"
        fi
    else
        log "VR_OAUTH_SKIP not-configured args=$*"
    fi

    # ---- P3: HLS safety net ----
    log "FALLBACK_HLS args=$*"
    exec "$YTDLP" --ignore-config --config-locations "$USER_CONF" --config-locations "$CONF" --plugin-dirs "$PLUGINS" --extractor-args "youtube:player_client=web_safari" "${NEW_ARGS[@]}"
else
    # no user config: single pass with the managed config only
    filter_args "$@"
    exec "$YTDLP" --ignore-config --config-locations "$CONF" --plugin-dirs "$PLUGINS" "${NEW_ARGS[@]}"
fi

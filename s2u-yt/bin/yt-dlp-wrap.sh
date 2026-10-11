#!/bin/bash
# s2u-yt managed wrapper — generated from bin/yt-dlp-wrap.sh by install.sh; do not edit.
#
# Resolution phases for one yt-dlp call (see conf/config):
#   P1  anonymous, unpinned client — cookie options stripped, buffered and
#       discarded on failure (a failed pass still prints a leading "null",
#       which corrupts the stdout JSON parsers of s2udio / mpv / CLI users).
#   P2  authenticated (your cookies) — output probed; on 403 falls through.
#   P3  web_safari HLS safety net (max 1080p60).
#
# Every pass whose output carries googlevideo URLs is probed by
# bin/s2u-yt-probe.py before it is handed over, and probed with the request
# shape the players actually use: a plain GET (no Range header) through
# ffprobe / curl. A URL that only answers 206 to a small bounded range but
# 403s the player's open is caught here instead of dying in MPD (v2026-09-22).
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
LOG="${S2U_YT_LOG:-/tmp/s2u-yt-wrapper.log}"
YTDLP="@REAL_YTDLP@"

log() { echo "$(date -Is) $*" >> "$LOG" 2>/dev/null || true; }

# --- foreign-interpreter defence (2026-10-06) ------------------------------
# SVP4's mpv launcher (mpv-svp) exports its portable CPython 3.12 through
# LD_PRELOAD (plus PYTHONPATH/PYTHONHOME) so SVP's embedded vsscript can import
# vapoursynth from the system mpv. mpv passes its whole environment to every
# child it spawns, and mpv's built-in ytdl_hook spawns *this* wrapper: the
# preloaded foreign libpython interposes the interpreter's own initialisation,
# so the real yt-dlp dies with
#     ModuleNotFoundError: No module named 'yt_dlp'
# before it reads a single argument. mpv then opens the watch URL as HTML,
# fails with "unrecognized file format" and exits — the video window never
# opens and s2udio reports nothing (measured 2026-10-06, mpv v0.41.0 launched
# through mpv-svp + yt-dlp 2026.08.19 in a pipx venv; the identical call
# succeeds with a clean environment).
#
# Nothing here is a Python program that wants SVP's runtime — the probe is a
# Python script too — so drop the leaked variables before any pass runs. The
# wrapper's parent (mpv) keeps its own copy, which is what SVP needs.
if [ -n "$LD_PRELOAD" ] || [ -n "$PYTHONPATH" ] || [ -n "$PYTHONHOME" ]; then
    log "ENV_CLEANSED LD_PRELOAD=${LD_PRELOAD:-} PYTHONPATH=${PYTHONPATH:-} PYTHONHOME=${PYTHONHOME:-}"
fi
unset LD_PRELOAD PYTHONHOME PYTHONPATH

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
        # P1 is probed like P2 (v2026-09-22): it is the pass that usually
        # answers here, and an unprobed URL is how a 403-dead stream reached
        # MPD — the song died and the playlist skipped on to the next rows. A
        # URL the players cannot open now falls through to P2/P3 instead.
        if grep -q googlevideo "$TMP"; then
            "$PROBE" "$TMP" 2>/dev/null; rc=$?
            if [ "$rc" -ne 1 ]; then
                cat "$TMP"; rm -f "$TMP"; exit 0
            fi
            log "PHASE1_PROBE403 args=$*"
        else
            cat "$TMP"; rm -f "$TMP"; exit 0
        fi
    else
        # Mirrors PHASE2_FAIL: a broken real yt-dlp (missing module, bad
        # interpreter) used to leave no trace at all in this log.
        log "PHASE1_FAIL exit=$status args=$*"
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

    # ---- P3: HLS safety net ----
    log "FALLBACK_HLS args=$*"
    exec "$YTDLP" --ignore-config --config-locations "$USER_CONF" --config-locations "$CONF" --plugin-dirs "$PLUGINS" --extractor-args "youtube:player_client=web_safari" "${NEW_ARGS[@]}"
else
    # no user config: single pass with the managed config only
    filter_args "$@"
    exec "$YTDLP" --ignore-config --config-locations "$CONF" --plugin-dirs "$PLUGINS" "${NEW_ARGS[@]}"
fi

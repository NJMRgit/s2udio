#!/usr/bin/env bash
# setup.sh — full setup for the s2udio TUI (Arch/CachyOS, systemd user session)
#
# Goal: ONE setup run, every TUI feature works — using ONLY official
# packages from the distro repositories or the AUR. No patched third-party
# builds: no custom cava, no patched mpDris2, no yt-dlp-ejs.
#
#   * system packages            mpd, ffmpeg, cava, python-yt-dlp
#   * AUR packages               mpv-full, mpdris2-git (via yay/paru)
#   * builds + installs binary   -> ~/.local/bin/s2udio
#   * support scripts            lyrics fetcher, mpv tracker daemon (which
#                                starts the bundled s2udio-mpris bridge),
#                                the s2u-mpdris2 shim (official mpDris2 +
#                                stream art), mpvSockets.lua
#   * seeds config/theme         -> ~/.config/s2udio/ (if absent)
#   * cava + MPD fifo output     (official cava; MPD fifo output)
#   * MPD / mpDris2 user services
#
# Idempotent: safe to re-run at any time. Never overwrites existing configs
# or user files. Package installs prompt for sudo/AUR-helper confirmation;
# pass -y to accept without asking (non-interactive runs skip installs).
#
# Usage: ./setup.sh [-y]
set -euo pipefail
cd "$(dirname "$0")"

ASSUME_YES=0
[[ "${1:-}" == "-y" ]] && ASSUME_YES=1

info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m  !! %s\033[0m\n' "$*"; }

confirm() { # $1 = question; returns 0 if yes
    if [[ $ASSUME_YES -eq 1 ]]; then return 0; fi
    if [[ ! -t 0 ]]; then warn "$1 (skipped: not interactive)"; return 1; fi
    read -r -p "$1 [y/N] " ans
    [[ "${ans,,}" == "y" || "${ans,,}" == "yes" ]]
}

BIN_DIR="$HOME/.local/bin"
CFG_DIR="$HOME/.config/s2udio"
MPD_CONF="${MPD_CONF:-$HOME/.config/mpd/mpd.conf}"
MPV_SCRIPTS="$HOME/.config/mpv/scripts"

# yay (or paru) drives the AUR installs (mpv-full, mpdris2-git).
AUR_HELPER=""
for h in yay paru; do
    if command -v "$h" >/dev/null 2>&1; then AUR_HELPER="$h"; break; fi
done

# ---------------------------------------------------------------------------
info "1/9  System packages (mpd ffmpeg cava python-yt-dlp)"
PACMAN_PKGS=(mpd ffmpeg cava python-yt-dlp)
MISSING=()
for p in "${PACMAN_PKGS[@]}"; do
    pacman -Q "$p" >/dev/null 2>&1 || MISSING+=("$p")
done
if ((${#MISSING[@]})); then
    if confirm "Install system packages (${MISSING[*]})? (needs sudo)"; then
        sudo pacman -S --needed "${MISSING[@]}"
        ok "installed: ${MISSING[*]}"
    else
        warn "missing packages: ${MISSING[*]}"
    fi
else
    ok "all system packages present"
fi

# mpv-full (AUR; also in the CachyOS repos): the full-featured mpv the video
# pipeline wants (mpvSockets, Jellyfin playback, thumbnails). Replaces the
# plain mpv package.
# mpdris2-git (AUR; also in the CachyOS repos): the official MPRIS bridge
# for MPD, run through the s2u-mpdris2 shim (steps 4/8) so stream
# thumbnails still reach the media controls. No patched copy is installed.
AUR_PKGS=(mpv-full mpdris2-git)
MISSING_AUR=()
for p in "${AUR_PKGS[@]}"; do
    pacman -Q "$p" >/dev/null 2>&1 || MISSING_AUR+=("$p")
done
if ((${#MISSING_AUR[@]})); then
    if [[ -n "$AUR_HELPER" ]]; then
        if confirm "Install AUR packages (${MISSING_AUR[*]}) via $AUR_HELPER?"; then
            "$AUR_HELPER" -S --needed "${MISSING_AUR[@]}"
            ok "installed: ${MISSING_AUR[*]}"
        else
            warn "missing AUR packages: ${MISSING_AUR[*]}"
        fi
    else
        warn "no AUR helper (yay/paru) found - install manually: ${MISSING_AUR[*]}"
    fi
else
    ok "AUR packages present (${AUR_PKGS[*]})"
fi

# ---------------------------------------------------------------------------
info "2/9  Build the s2udio binary"
if ! command -v cargo >/dev/null 2>&1; then
    warn "cargo not found - install rustup/rust first (sudo pacman -S rust)"
else
    cargo build --release
    install -Dm755 target/release/s2u "$BIN_DIR/s2udio.new"
    mv -f "$BIN_DIR/s2udio.new" "$BIN_DIR/s2udio"   # atomic rename: survives a running s2udio
    ok "binary -> $BIN_DIR/s2udio ($("$BIN_DIR/s2udio" version | head -1))"
fi

# ---------------------------------------------------------------------------
info "3/9  yt-dlp (official python-yt-dlp)"
if command -v yt-dlp >/dev/null 2>&1; then
    ok "yt-dlp $(yt-dlp --version 2>/dev/null || echo '?')"
    warn "official yt-dlp only - no yt-dlp-ejs / node JS runtime / cookie profile is set up."
    warn "some YouTube format resolutions may report 'Requested format is not available';"
    warn "keep yt-dlp current (pacman -Syu) and consider a logged-in browser profile in"
    warn "~/.config/yt-dlp/config if bot checks bite."
else
    warn "yt-dlp missing - install python-yt-dlp (sudo pacman -S python-yt-dlp)"
fi

# ---------------------------------------------------------------------------
info "4/9  Support scripts"
[[ -f scripts/rmpc-fetch-lyrics ]] && { install -Dm755 scripts/rmpc-fetch-lyrics "$BIN_DIR/rmpc-fetch-lyrics"; ok "lyrics fetcher -> $BIN_DIR/rmpc-fetch-lyrics"; } || warn "scripts/rmpc-fetch-lyrics missing in this checkout"
[[ -f scripts/s2u-mpv-tracker ]] && { install -Dm755 scripts/s2u-mpv-tracker "$BIN_DIR/s2u-mpv-tracker"; ok "mpv tracker daemon -> $BIN_DIR/s2u-mpv-tracker"; } || warn "scripts/s2u-mpv-tracker missing in this checkout"
[[ -f scripts/s2udio-mpris ]] && { install -Dm755 scripts/s2udio-mpris "$BIN_DIR/s2udio-mpris"; ok "mpv MPRIS bridge -> $BIN_DIR/s2udio-mpris"; } || warn "scripts/s2udio-mpris missing in this checkout"
[[ -f scripts/s2u-mpdris2 ]] && { install -Dm755 scripts/s2u-mpdris2 "$BIN_DIR/s2u-mpdris2"; ok "mpDris2 stream-art shim -> $BIN_DIR/s2u-mpdris2"; } || warn "scripts/s2u-mpdris2 missing in this checkout"
mkdir -p "$MPV_SCRIPTS"
[[ -f scripts/mpvSockets.lua ]] && { install -Dm644 scripts/mpvSockets.lua "$MPV_SCRIPTS/mpvSockets.lua"; ok "mpvSockets.lua -> $MPV_SCRIPTS"; } || warn "scripts/mpvSockets.lua missing in this checkout"

# ---------------------------------------------------------------------------
info "5/9  Seed config + theme (only if absent; embedded defaults otherwise)"
mkdir -p "$CFG_DIR/themes"
# Round 23: every s2udio config lives in ~/.config/s2udio — nothing in
# ~/.config/rmpc anymore. A legacy ~/.config/rmpc/config.ron is migrated
# by the app on first run (base + overlay merge, sidecars/themes copied).
if [[ ! -f "$CFG_DIR/config.ron" ]]; then
    cp assets/example_config.ron "$CFG_DIR/config.ron" && ok "config -> $CFG_DIR/config.ron"
fi
if [[ ! -f "$CFG_DIR/themes/default.ron" ]]; then
    cp assets/example_theme.ron "$CFG_DIR/themes/default.ron" && ok "theme -> $CFG_DIR/themes/default.ron"
fi
mkdir -p "$CFG_DIR/lyrics"  # s2udio's own .lrc library (round 23)

# ---------------------------------------------------------------------------
info "6/9  cava"
if command -v cava >/dev/null 2>&1; then
    ok "cava $(cava -v 2>/dev/null | head -1 || pacman -Q cava)"
else
    warn "cava missing - install it (sudo pacman -S cava)"
fi

# ---------------------------------------------------------------------------
info "7/9  MPD fifo output (for cava via s2udio)"
if [[ -f "$MPD_CONF" ]]; then
    if grep -q "mpd-cava.fifo" "$MPD_CONF"; then
        ok "fifo output already configured in $MPD_CONF"
    else
        cat >> "$MPD_CONF" <<'EOF'

# cava (via s2udio) - bypasses all audio devices and PipeWire entirely.
audio_output {
    type    "fifo"
    name    "cava"
    path    "/tmp/mpd-cava.fifo"
    format  "44100:16:2"
}
EOF
        ok "fifo output appended to $MPD_CONF"
        systemctl --user restart mpd 2>/dev/null && ok "mpd restarted" || warn "restart mpd manually"
    fi
else
    warn "no MPD config at $MPD_CONF - set MPD_CONF=/path/to/mpd.conf and re-run"
fi

# ---------------------------------------------------------------------------
info "8/9  MPD + mpDris2 user services"
UNITS=$(systemctl --user list-unit-files 2>/dev/null || true)
if grep -q '^mpd.service' <<<"$UNITS"; then
    systemctl --user is-enabled mpd.service >/dev/null 2>&1 \
        && ok "mpd.service enabled" || { systemctl --user enable --now mpd.service; ok "mpd.service enabled+started"; }
else
    warn "mpd.service not found - install mpd and enable it: systemctl --user enable --now mpd"
fi
# mpDris2 runs through the s2u-mpdris2 shim: the official binary,
# extended at runtime to serve the stream thumbnail s2udio writes to
# ~/.cache/rmpc/mpris-art (the official find_cover returns None for
# http(s) URLs). A drop-in swaps ExecStart; the stale patched copy from
# older setups is removed.
DROPIN_DIR="$HOME/.config/systemd/user/mpDris2.service.d"
rm -rf "$DROPIN_DIR"   # clear stale drop-ins from older setups (mpris-art.conf, self-heal.conf, ...)
mkdir -p "$DROPIN_DIR"
cat > "$DROPIN_DIR/s2udio.conf" <<EOF
# s2udio: run the official mpDris2 through the s2u-mpdris2 shim, which
# serves the stream thumbnail (~/.cache/rmpc/mpris-art) as MPRIS art for
# non-file streams. The shim falls back to unpatched when the installed
# mpDris2 changes layout.
[Service]
ExecStart=
ExecStart=$BIN_DIR/s2u-mpdris2 --use-journal
EOF
systemctl --user daemon-reload 2>/dev/null || true
ok "mpDris2.service -> $BIN_DIR/s2u-mpdris2 (official mpDris2 + stream-art shim)"
if [[ -f "$BIN_DIR/mpDris2" ]]; then
    rm -f "$BIN_DIR/mpDris2"
    ok "removed stale patched mpDris2 copy ($BIN_DIR/mpDris2)"
fi
if grep -qi '^mpdris2.service' <<<"$UNITS"; then
    systemctl --user is-enabled mpDris2.service >/dev/null 2>&1 \
        && ok "mpDris2.service enabled (official package)" \
        || { systemctl --user enable --now mpDris2.service; ok "mpDris2.service enabled+started"; }
else
    warn "mpDris2.service not found - install mpdris2-git and enable it"
fi

# ---------------------------------------------------------------------------
info "9/9  Summary"
if [[ -x "$BIN_DIR/s2udio" ]]; then
    "$BIN_DIR/s2udio" version 2>/dev/null | head -1 | sed 's/^/  s2udio: /'
else
    warn "s2udio binary not built (cargo missing?) - build with: cargo build --release"
fi
printf '  scripts: %s\n' "$BIN_DIR"
printf '  config : %s/config.ron (embedded defaults active if absent)\n' "$CFG_DIR"
printf '  lyrics: %s/lyrics (s2udio's own .lrc library; the user's MPD\n' "$CFG_DIR"
printf '           library .lrc files are read first and never overwritten)\n' 
printf '  mpv    : %s\n' "$(command -v mpv >/dev/null && echo present || echo MISSING)"
printf '  yt-dlp : %s (%s)\n' "$(command -v yt-dlp >/dev/null && echo present || echo MISSING)" "$(yt-dlp --version 2>/dev/null || echo '?')"
printf '  cava   : %s\n' "$(command -v cava >/dev/null && echo present || echo MISSING)"
printf '  mpd    : %s\n' "$(systemctl --user is-active mpd.service 2>/dev/null || echo inactive)"
printf '  mpDris2: %s\n' "$(systemctl --user is-active mpDris2.service 2>/dev/null || echo inactive)"
printf '\n  All dependencies are official distro/AUR packages (no patched cava/mpDris2,\n'
printf '  no yt-dlp-ejs). Restart s2udio to pick up the new binary: kill s2udio\n'
printf '  (and its cava child), then just run `s2udio` in your terminal.\n'

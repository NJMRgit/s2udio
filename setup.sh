#!/usr/bin/env bash
# setup.sh — full setup for the s2udio TUI (multi-distro dispatcher)
#
# Goal: ONE setup run, every TUI feature works — using ONLY official
# packages from the distro repositories or the AUR. No patched third-party
# builds: no custom cava, no patched mpDris2, no yt-dlp-ejs.
#
# This is a DISPATCHER (plan docs/design/Validation/distro-support.md §6.2):
# it detects the distro via /etc/os-release (ID / ID_LIKE) and runs the
# matching backend:
#
#   pacman  -> Arch / CachyOS / Artix   (the original installer, unchanged)
#   dnf5    -> Fedora                   (RPM Fusion free provides mpd/ffmpeg/mpv)
#   apt     -> Debian / Ubuntu / Devuan (system mpd stopped+disabled;
#                                        user-level instance; stale yt-dlp hint)
#   apk     -> Alpine                   (cava from source; upstream python mpDris2)
#   xbps    -> Void                     (mpd setcap -r in restricted environments)
#   nix     -> NixOS                    (nix profile install, flake.nix)
#
#   * system packages            mpd, ffmpeg, cava, yt-dlp, mpv, ...
#   * AUR packages (Arch only)   mpdris2-git (via yay/paru)
#   * mpv (Arch only)            mpv-full (recommended) or standard mpv;
#                                other backends install plain mpv
#   * builds + installs binary   -> ~/.local/bin/s2udio (nix: the flake package)
#   * support scripts            lyrics fetcher, mpv tracker daemon (which
#                                starts the bundled s2udio-mpris bridge),
#                                the s2u-mpdris2 shim (official mpDris2 +
#                                stream art), mpvSockets.lua
#   * seeds config/theme         -> ~/.config/s2udio/ (if absent)
#   * cava + MPD fifo output     (official cava; MPD fifo output)
#   * MPD / mpDris2 user services (enable/start via scripts/s2u-svc; the
#                                Arch path keeps its direct systemctl --user)
#
# Idempotent: safe to re-run at any time. Never overwrites existing configs
# or user files. Package installs prompt for sudo/AUR-helper confirmation;
# pass -y to accept without asking (non-interactive runs skip installs).
#
# Usage: ./setup.sh [-y]
#        S2UDIO_OS_RELEASE=/path (testing hook; defaults to /etc/os-release)
set -euo pipefail
cd "$(dirname "$0")"

ASSUME_YES=0
[[ "${1:-}" == "-y" ]] && ASSUME_YES=1

info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m  !! %s\033[0m\n' "$*"; }
die()   { printf '\033[1;31mFATAL:\033[0m %s\n' "$*" >&2; exit 1; }

confirm() { # $1 = question; returns 0 if yes
    if [[ $ASSUME_YES -eq 1 ]]; then return 0; fi
    if [[ ! -t 0 ]]; then warn "$1 (skipped: not interactive)"; return 1; fi
    read -r -p "$1 [y/N] " ans
    [[ "${ans,,}" == "y" || "${ans,,}" == "yes" ]]
}

# Privilege-elevation helper (NON-Arch backends only, review follow-up): root
# needs no elevation; otherwise prefer sudo, then doas (Alpine's default).
# With neither, die with a clear message — no silent partial install.
# An array is used so the empty (root) case expands to ZERO words (a quoted
# empty string would become an empty command). run_arch keeps its own literal
# `sudo` calls (Arch ships sudo; byte-identity with the pre-dispatcher
# baseline is a hard requirement).
ELEVATE_CMD=()
resolve_elevation() {
    if [[ ${#ELEVATE_CMD[@]} -gt 0 ]]; then return 0; fi
    if [[ "$(id -u)" == "0" ]]; then
        ELEVATE_CMD=()
    elif command -v sudo >/dev/null 2>&1; then
        ELEVATE_CMD=(sudo)
    elif command -v doas >/dev/null 2>&1; then
        ELEVATE_CMD=(doas)
    else
        warn "no privilege-elevation tool found (sudo/doas) and not running as root"
        die "cannot install packages - re-run as root or install sudo/doas"
    fi
}

BIN_DIR="$HOME/.local/bin"
CFG_DIR="$HOME/.config/s2udio"
MPD_CONF="${MPD_CONF:-$HOME/.config/mpd/mpd.conf}"
MPV_SCRIPTS="$HOME/.config/mpv/scripts"

# summary_step() reads these (set by each backend; Arch defaults below):
SUMMARY_BIN="$BIN_DIR/s2udio"
SUMMARY_MPV_FULL=0
SUMMARY_MPD_ACTIVE=(systemctl --user is-active mpd.service)
SUMMARY_MPDRIS2_ACTIVE=(systemctl --user is-active mpDris2.service)

# ---------------------------------------------------------------------------
# Distro detection (plan §6.2): /etc/os-release ID / ID_LIKE -> backend.
# S2UDIO_OS_RELEASE overrides the file for hermetic testing.
OS_RELEASE="${S2UDIO_OS_RELEASE:-/etc/os-release}"
if [[ ! -r "$OS_RELEASE" ]]; then
    die "cannot read $OS_RELEASE - unsupported system (s2udio supports Arch/CachyOS/Artix, Fedora, Debian/Ubuntu/Devuan, Alpine, Void, NixOS)"
fi
# shellcheck disable=SC1090
. "$OS_RELEASE"
DISTRO_ID="${ID:-}"
DISTRO_ID_LIKE="${ID_LIKE:-}"
detect_backend() {
    case "$DISTRO_ID $DISTRO_ID_LIKE" in
        *arch*|*cachyos*|*artix*)    echo pacman ;;
        *fedora*)                    echo dnf5 ;;
        *debian*|*ubuntu*|*devuan*)  echo apt ;;
        *alpine*)                    echo apk ;;
        *void*)                      echo xbps ;;
        *nixos*)                     echo nix ;;
        *)                           echo unknown ;;
    esac
}

# ---------------------------------------------------------------------------
# version_ge A B — 0 when A >= B (dotted version compare; extra parts ignored)
version_ge() {
    local IFS=. a b
    a=($1); b=($2)
    local i
    for i in 0 1 2; do
        local av="${a[$i]:-0}" bv="${b[$i]:-0}"
        (( av > bv )) && return 0
        (( av < bv )) && return 1
    done
    return 0
}

# ---------------------------------------------------------------------------
# Shared step functions (used by every backend; Arch output byte-identical).

install_support_scripts() {
    info "4/9  Support scripts"
    [[ -f scripts/rmpc-fetch-lyrics ]] && { install -Dm755 scripts/rmpc-fetch-lyrics "$BIN_DIR/rmpc-fetch-lyrics"; ok "lyrics fetcher -> $BIN_DIR/rmpc-fetch-lyrics"; } || warn "scripts/rmpc-fetch-lyrics missing in this checkout"
    [[ -f scripts/s2u-mpv-tracker ]] && { install -Dm755 scripts/s2u-mpv-tracker "$BIN_DIR/s2u-mpv-tracker"; ok "mpv tracker daemon -> $BIN_DIR/s2u-mpv-tracker"; } || warn "scripts/s2u-mpv-tracker missing in this checkout"
    [[ -f scripts/s2udio-mpris ]] && { install -Dm755 scripts/s2udio-mpris "$BIN_DIR/s2udio-mpris"; ok "mpv MPRIS bridge -> $BIN_DIR/s2udio-mpris"; } || warn "scripts/s2udio-mpris missing in this checkout"
    [[ -f scripts/s2u-mpdris2 ]] && { install -Dm755 scripts/s2u-mpdris2 "$BIN_DIR/s2u-mpdris2"; ok "mpDris2 stream-art shim -> $BIN_DIR/s2u-mpdris2"; } || warn "scripts/s2u-mpdris2 missing in this checkout"
    mkdir -p "$MPV_SCRIPTS"
    [[ -f scripts/mpvSockets.lua ]] && { install -Dm644 scripts/mpvSockets.lua "$MPV_SCRIPTS/mpvSockets.lua"; ok "mpvSockets.lua -> $MPV_SCRIPTS"; } || warn "scripts/mpvSockets.lua missing in this checkout"
}

install_s2u_svc() { # every backend installs it (the tracker's mpDris2 stop/start routes through s2u-svc)
    [[ -f scripts/s2u-svc ]] && { install -Dm755 scripts/s2u-svc "$BIN_DIR/s2u-svc"; ok "s2u-svc -> $BIN_DIR/s2u-svc"; } || warn "scripts/s2u-svc missing in this checkout"
}

seed_config_theme() {
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
    migrate_radio_favourites
}

# ---------------------------------------------------------------------------
# Radio favourites migration: the favourites used to live as an MPD stored
# playlist (<playlist>.m3u in MPD's playlist dir), which rmpc's playlist UI
# showed as a playlist it doesn't understand. They now live in the s2udio
# config dir (~/.config/s2udio/radio/); move an existing file there during
# install/update so MPD stops seeing it.
migrate_radio_favourites() {
    local dest_dir="$CFG_DIR/radio"
    # The configured playlist name, if any (default "radio"); best-effort
    # read of the s2udio config so a custom name is honoured too.
    local playlist="radio" configured
    configured="$(sed -n 's/.*playlist: *Some("\([^"]*\)").*/\1/p; s/.*playlist: *"\([^"]*\)".*/\1/p' "$CFG_DIR/config.ron" 2>/dev/null | head -1 || true)"
    [[ -n "$configured" ]] && playlist="$configured"

    # Candidate old locations: MPD's configured playlist_directory first,
    # then the standard fallbacks the old app tried.
    local src="" dir pd
    pd="$(sed -n 's/^[[:space:]]*playlist_directory[[:space:]]*"\([^"]*\)".*/\1/p' "$MPD_CONF" 2>/dev/null | head -1 || true)"
    for dir in "$pd" "$HOME/.cache/mpd/playlists" "$HOME/.local/share/mpd/playlists" "$HOME/.config/mpd/playlists" "$HOME/.mpd/playlists" "/var/lib/mpd/playlists"; do
        [[ -z "$dir" ]] && continue
        dir="${dir/#\~/$HOME}"
        if [[ -f "$dir/$playlist.m3u" ]]; then
            src="$dir/$playlist.m3u"
            break
        fi
    done
    [[ -n "$src" ]] || return 0

    mkdir -p "$dest_dir"
    if [[ -f "$dest_dir/$playlist.m3u" ]]; then
        warn "radio favourites already at $dest_dir/$playlist.m3u - leaving $src in place"
        return 0
    fi
    mv -f "$src" "$dest_dir/$playlist.m3u"
    ok "radio favourites -> $dest_dir/$playlist.m3u (removed from MPD playlists)"
}

build_binary() { # $1 = cargo-missing hint
    info "2/9  Build the s2udio binary"
    if ! command -v cargo >/dev/null 2>&1; then
        warn "cargo not found - $1"
    else
        cargo build --release
        install -Dm755 target/release/s2u "$BIN_DIR/s2udio.new"
        mv -f "$BIN_DIR/s2udio.new" "$BIN_DIR/s2udio"   # atomic rename: survives a running s2udio
        ok "binary -> $BIN_DIR/s2udio ($("$BIN_DIR/s2udio" version | head -1))"
    fi
}

ytdlp_step() { # $1 = keep-current hint line; $2 = too-old hint ("" = none); $3 = missing hint
    info "3/9  yt-dlp (official python-yt-dlp)"
    if command -v yt-dlp >/dev/null 2>&1; then
        local ver; ver="$(yt-dlp --version 2>/dev/null || echo '?')"
        ok "yt-dlp $ver"
        warn "official yt-dlp only - no yt-dlp-ejs / node JS runtime / cookie profile is set up."
        warn "some YouTube format resolutions may report 'Requested format is not available';"
        warn "$1"
        warn "~/.config/yt-dlp/config if bot checks bite."
        if [[ -n "$2" && "$ver" != "?" ]] && ! version_ge "$ver" "2025.01.01"; then
            warn "$2"
        fi
    else
        warn "yt-dlp missing - $3"
    fi
}

cava_step() { # $1 = version fallback command ("" = none); $2 = missing hint
    info "6/9  cava"
    if command -v cava >/dev/null 2>&1; then
        if [[ -n "$1" ]]; then
            ok "cava $(cava -v 2>/dev/null | head -1 || $1)"
        else
            ok "cava $(cava -v 2>/dev/null | head -1)"
        fi
    else
        warn "cava missing - $2"
    fi
}

# Non-Arch only: the user-level instance needs its own mpd.conf (the system
# mpd of Debian/Ubuntu uses /etc/mpd.conf; Arch setups already have one).
# Never overwrites an existing config; confirm-gated like every install.
ensure_mpd_conf() {
    if [[ -f "$MPD_CONF" ]]; then
        return 0
    fi
    if confirm "Create a user-level MPD config at $MPD_CONF (system mpd is stopped on this distro; s2udio runs a user instance)?"; then
        mkdir -p "$(dirname "$MPD_CONF")" "$HOME/.cache/mpd"
        cat > "$MPD_CONF" <<EOF
music_directory "$HOME/Music"
bind_to_address "127.0.0.1"
port "6600"
db_file "$HOME/.cache/mpd/database"
state_file "$HOME/.cache/mpd/state"
sticker_file "$HOME/.cache/mpd/sticker.sql"
playlist_directory "$HOME/.cache/mpd/playlists"
follow_outside_symlinks "yes"
follow_inside_symlinks "yes"
auto_update "yes"
EOF
        ok "mpd.conf created at $MPD_CONF (user-level instance; fifo appended next)"
    else
        warn "no MPD config at $MPD_CONF - set MPD_CONF=/path/to/mpd.conf and re-run"
    fi
}

mpd_fifo_append() { # "$@" = restart command (Arch: systemctl --user restart mpd; others: "$BIN_DIR/s2u-svc" restart mpd)
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
            if "$@" 2>/dev/null; then ok "mpd restarted"; else warn "restart mpd manually"; fi
        fi
    else
        warn "no MPD config at $MPD_CONF - set MPD_CONF=/path/to/mpd.conf and re-run"
    fi
}

summary_step() {
    info "9/9  Summary"
    if [[ -x "$SUMMARY_BIN" ]]; then
        "$SUMMARY_BIN" version 2>/dev/null | head -1 | sed 's/^/  s2udio: /'
    else
        warn "s2udio binary not built (cargo missing?) - build with: cargo build --release"
    fi
    printf '  scripts: %s\n' "$BIN_DIR"
    printf '  config : %s/config.ron (embedded defaults active if absent)\n' "$CFG_DIR"
    printf "  lyrics: %s/lyrics (s2udio's own .lrc library; the user's MPD\n" "$CFG_DIR"
    printf '           library .lrc files are read first and never overwritten)\n'

    if command -v mpv >/dev/null 2>&1; then
        if [[ $SUMMARY_MPV_FULL -eq 1 ]]; then
            printf '  mpv    : present (mpv-full)\n'
        else
            printf '  mpv    : present (standard)\n'
        fi
    else
        printf '  mpv    : MISSING\n'
    fi
    printf '  yt-dlp : %s (%s)\n' "$(command -v yt-dlp >/dev/null && echo present || echo MISSING)" "$(yt-dlp --version 2>/dev/null || echo '?')"
    printf '  cava   : %s\n' "$(command -v cava >/dev/null && echo present || echo MISSING)"
    printf '  mpd    : %s\n' "$("${SUMMARY_MPD_ACTIVE[@]}" 2>/dev/null || echo inactive)"
    printf '  mpDris2: %s\n' "$("${SUMMARY_MPDRIS2_ACTIVE[@]}" 2>/dev/null || echo inactive)"
    printf '\n  All dependencies are official distro/AUR packages (no patched cava/mpDris2,\n'
    printf '  no yt-dlp-ejs). Restart s2udio to pick up the new binary: kill s2udio\n'
    printf '  (and its cava child), then just run `s2udio` in your terminal.\n'
}

# Non-Arch backends: distro rustc is too old for edition-2024 (Cargo.toml
# needs >= 1.88; Fedora 41 ~1.80, Debian 12 1.63, Ubuntu 24.04 1.75, Alpine
# older) — rustup minimal profile is the validated path (plan §12). Void
# ships rustc 1.97.1 and skips this. Returns 0 when a usable toolchain is
# present/installed.
ensure_rust_toolchain() {
    if command -v cargo >/dev/null 2>&1; then
        local ver; ver="$(rustc --version 2>/dev/null | awk '{print $2}')"
        if [[ -n "$ver" ]] && version_ge "$ver" "1.88"; then
            return 0
        fi
        warn "distro rustc ${ver:-?} is too old (s2udio needs >= 1.88) - installing a current toolchain"
    fi
    if confirm "Install a current Rust toolchain via rustup (minimal profile; needs network)?"; then
        if command -v rustup >/dev/null 2>&1; then
            rustup toolchain install stable --profile minimal
            rustup default stable
        else
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
            export PATH="$HOME/.cargo/bin:$PATH"
        fi
        ok "rust toolchain ready ($(rustc --version 2>/dev/null))"
        return 0
    fi
    return 1
}

# systemd targets (dnf5/apt): stop+disable a system mpd unit if the distro
# ships one (Debian/Ubuntu do — plan §12.2), write user units when the
# package has none, and enable/start through s2u-svc (systemd-user backend).
services_step_systemd() {
    info "8/9  MPD + mpDris2 user services (s2u-svc, systemd-user)"
    if systemctl list-unit-files 2>/dev/null | grep -q '^mpd.service'; then
        systemctl stop mpd.service >/dev/null 2>&1 || true
        systemctl disable mpd.service >/dev/null 2>&1 || true
        ok "system mpd.service stopped+disabled (user unit takes over)"
    fi
    mkdir -p "$HOME/.config/systemd/user/mpDris2.service.d"
    if [[ ! -f "$HOME/.config/systemd/user/mpd.service" ]]        && ! systemctl --user list-unit-files 2>/dev/null | grep -q '^mpd.service'; then
        cat > "$HOME/.config/systemd/user/mpd.service" <<EOF
[Unit]
Description=Music Player Daemon (s2udio user instance)
After=network.target
[Service]
ExecStart=/usr/bin/mpd --no-daemon $MPD_CONF
Restart=on-failure
[Install]
WantedBy=default.target
EOF
        ok "mpd.service (user) written (no packaged user unit on this distro)"
    fi
    cat > "$HOME/.config/systemd/user/mpDris2.service.d/s2udio.conf" <<EOF
[Service]
ExecStart=
ExecStart=$BIN_DIR/s2u-mpdris2 --use-journal
EOF
    # The packaged mpDris2 unit on Debian/Ubuntu carries
    # ConditionUser=!@system (no MPRIS bridge for root/system users) — right
    # for real desktops, but it blocks root (harness containers). When the
    # current user is root we fall back to our own condition-free unit so the
    # service actually runs (validated harness behavior, plan §12.2).
    if systemctl --user list-unit-files 2>/dev/null | grep -q '^mpDris2.service' \
       && systemctl --user cat mpDris2.service 2>/dev/null | grep -q 'ConditionUser=' \
       && [[ "$(id -u)" == "0" ]] \
       && [[ ! -f "$HOME/.config/systemd/user/mpDris2.service" ]]; then
        cat > "$HOME/.config/systemd/user/mpDris2.service" <<EOF
[Unit]
Description=MPRIS bridge for MPD (s2udio s2u-mpdris2 shim)
After=mpd.service
[Service]
ExecStart=$BIN_DIR/s2u-mpdris2 --use-journal
Restart=on-failure
[Install]
WantedBy=default.target
EOF
        ok "mpDris2.service (user) written (packaged unit has ConditionUser=!@system; root run)"
    fi
    if [[ ! -f "$HOME/.config/systemd/user/mpDris2.service" ]]        && ! systemctl --user list-unit-files 2>/dev/null | grep -q '^mpDris2.service'; then
        cat > "$HOME/.config/systemd/user/mpDris2.service" <<EOF
[Unit]
Description=MPRIS bridge for MPD (s2udio s2u-mpdris2 shim)
After=mpd.service
[Service]
ExecStart=$BIN_DIR/s2u-mpdris2 --use-journal
Restart=on-failure
[Install]
WantedBy=default.target
EOF
        ok "mpDris2.service (user) written (no packaged user unit on this distro)"
    fi
    systemctl --user daemon-reload 2>/dev/null || true
    "$BIN_DIR/s2u-svc" enable mpd || true
    "$BIN_DIR/s2u-svc" start mpd || true
    "$BIN_DIR/s2u-svc" enable mpDris2 || true
    "$BIN_DIR/s2u-svc" start mpDris2 || true
    sleep 2
    "$BIN_DIR/s2u-svc" is-active mpd && ok "mpd active (user service)" || warn "mpd not active yet"
    "$BIN_DIR/s2u-svc" is-active mpDris2 && ok "mpDris2 active (user service)" || warn "mpDris2 not active yet"
}

# launcher targets (apk/nix): no systemd — s2u-svc's launcher backend runs
# mpd + the s2u-mpdris2 shim as plain user processes (plan §6.1).
services_step_launcher() {
    info "8/9  MPD + mpDris2 user services (s2u-svc launcher backend)"
    "$BIN_DIR/s2u-svc" start mpd || true
    sleep 2
    "$BIN_DIR/s2u-svc" is-active mpd && ok "mpd active (launcher)" || warn "mpd not active"
    "$BIN_DIR/s2u-svc" start mpDris2 || true
    sleep 2
    "$BIN_DIR/s2u-svc" is-active mpDris2 && ok "mpDris2 active (launcher)" || warn "mpDris2 not active"
}

# runit target (xbps/Void): per-user runsvdir under ~/.config/runit + sv(1)
# through s2u-svc's runit-user backend (plan §12 / Phase 3).
services_step_runit() {
    info "8/9  MPD + mpDris2 user services (s2u-svc runit-user)"
    # step 7's fifo restart started mpd through the launcher backend (the
    # runit dirs do not exist yet) — stop that instance BEFORE the runit
    # dirs appear, or the runit-supervised mpd cannot bind port 6600. On a
    # re-run the dirs already exist and s2u-svc stops via sv instead; both
    # paths leave a clean slate for runsvdir to take over.
    "$BIN_DIR/s2u-svc" stop mpd 2>/dev/null || true
    "$BIN_DIR/s2u-svc" stop mpDris2 2>/dev/null || true
    mkdir -p "$HOME/.config/runit/mpd" "$HOME/.config/runit/mpDris2"
    cat > "$HOME/.config/runit/mpd/run" <<EOF
#!/bin/sh
exec /usr/bin/mpd --no-daemon $MPD_CONF
EOF
    cat > "$HOME/.config/runit/mpDris2/run" <<EOF
#!/bin/sh
exec $BIN_DIR/s2u-mpdris2 --use-journal
EOF
    chmod +x "$HOME/.config/runit/mpd/run" "$HOME/.config/runit/mpDris2/run"
    if ! pgrep -f "runsvdir $HOME/.config/runit" >/dev/null 2>&1; then
        setsid runsvdir "$HOME/.config/runit" >/dev/null 2>&1 &
        local i
        for i in $(seq 1 10); do
            [[ -d "$HOME/.config/runit/mpd/supervise" ]] && break
            sleep 0.5
        done
    fi
    "$BIN_DIR/s2u-svc" start mpd || true
    sleep 2
    "$BIN_DIR/s2u-svc" is-active mpd && ok "mpd active (runit-user)" || warn "mpd not active"
    "$BIN_DIR/s2u-svc" start mpDris2 || true
    sleep 2
    "$BIN_DIR/s2u-svc" is-active mpDris2 && ok "mpDris2 active (runit-user)" || warn "mpDris2 not active"
}

# apk/nix: no distro mpDris2 (Alpine) or an unshimmable compiled ELF (nixpkgs)
# — install the upstream python source at the shim's fixed /usr/bin/mpDris2
# path (plan §5 decision point, §12.6).
install_upstream_mpdris2() { # $1 = reason text
    if [[ -s /usr/bin/mpDris2 ]]; then
        ok "mpDris2 already at /usr/bin/mpDris2"
        return 0
    fi
    if confirm "Install upstream python mpDris2 at /usr/bin/mpDris2? ($1) (needs root + network)"; then
        if curl -fsSL --max-time 60             https://raw.githubusercontent.com/eonpatapon/mpDris2/master/src/mpDris2.in.py             -o /tmp/mpDris2.in.py; then
            if "${ELEVATE_CMD[@]}" mkdir -p /usr/bin                && sed -e 's/@version@/0.9.1/g' -e 's/@gitversion@/0.9.1/g' -e 's|@datadir@|/usr/share|g'                     /tmp/mpDris2.in.py | "${ELEVATE_CMD[@]}" tee /usr/bin/mpDris2 >/dev/null                && "${ELEVATE_CMD[@]}" chmod +x /usr/bin/mpDris2; then
                rm -f /tmp/mpDris2.in.py
                ok "upstream mpDris2 source -> /usr/bin/mpDris2"
            else
                warn "could not write /usr/bin/mpDris2 (permission denied?) - MPRIS bridge will not work"
            fi
        else
            warn "upstream mpDris2 fetch failed - MPRIS bridge will not work"
        fi
    else
        warn "mpDris2 not installed - MPRIS bridge will not work"
    fi
}

mpv_plain_note() {
    warn "Arch-only mpv-full recommendation not applicable here - plain mpv installed (plan §5)"
}

# ---------------------------------------------------------------------------
# Arch / CachyOS / Artix — the ORIGINAL installer (output byte-for-byte
# identical to the pre-dispatcher setup.sh; regression risk zero).
run_arch() {
    # yay (or paru) drives the AUR installs (mpdris2-git; mpv-full when chosen).
    AUR_HELPER=""
    for h in yay paru; do
        if command -v "$h" >/dev/null 2>&1; then AUR_HELPER="$h"; break; fi
    done
    # -y means "accept without asking" (header contract): pacman must not
    # prompt again. Without --noconfirm a non-interactive `setup.sh -y`
    # aborts at the first install (pacman refuses a closed stdin; found by
    # the T6b real-Arch container run). Appended AFTER the targets so the
    # mock's call assertions and the pre-dispatcher byte-identity hold.
    PACMAN_NOCONFIRM=()
    [[ $ASSUME_YES -eq 1 ]] && PACMAN_NOCONFIRM=(--noconfirm)
    # ---------------------------------------------------------------------------
    info "1/9  System packages (mpd ffmpeg cava yt-dlp)"
    PACMAN_PKGS=(mpd ffmpeg cava yt-dlp)
    MISSING=()
    for p in "${PACMAN_PKGS[@]}"; do
        pacman -Q "$p" >/dev/null 2>&1 || MISSING+=("$p")
    done
    if ((${#MISSING[@]})); then
        if confirm "Install system packages (${MISSING[*]})? (needs sudo)"; then
            sudo pacman -S --needed "${MISSING[@]}" "${PACMAN_NOCONFIRM[@]}"
            ok "installed: ${MISSING[*]}"
        else
            warn "missing packages: ${MISSING[*]}"
        fi
    else
        ok "all system packages present"
    fi

    # mpdris2-git (AUR; also in the CachyOS repos): the official MPRIS bridge
    # for MPD, run through the s2u-mpdris2 shim (steps 4/8) so stream
    # thumbnails still reach the media controls. No patched copy is installed.
    AUR_PKGS=(mpdris2-git)
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

    # mpv: mpv-full (recommended) or standard mpv. The video pipeline
    # (mpvSockets, Jellyfin playback, thumbnails) is tuned for the
    # full-featured build; plain mpv works for standard playback. mpv-full
    # replaces the standard mpv package (same binary name) - the two are
    # mutually exclusive.
    # ---------------------------------------------------------------------------
    info "1b/9  mpv (mpv-full recommended, standard mpv supported)"
    install_mpv_full() {
        if [[ -n "$AUR_HELPER" ]]; then
            "$AUR_HELPER" -S --needed mpv-full
        else
            # return the pacman failure, not the warn's success: without this
            # the -y no-AUR-helper path would print "ok mpv-full installed"
            # after pacman failed ("target not found" — mpv-full is AUR-only
            # on stock Arch; found by the T6b real-Arch container run)
            if sudo pacman -S --needed mpv-full "${PACMAN_NOCONFIRM[@]}"; then
                return 0
            fi
            warn "mpv-full not in your distro repos - install yay/paru (AUR) or choose standard mpv"
            return 1
        fi
    }
    if pacman -Q mpv-full >/dev/null 2>&1; then
        ok "mpv-full present (full-featured build)"
    elif pacman -Q mpv >/dev/null 2>&1; then
        # Standard mpv found - recommend the full-featured build
        if confirm "Standard mpv is installed. Install mpv-full instead? (recommended: full-featured build for the video pipeline; replaces standard mpv)"; then
            if install_mpv_full; then
                ok "mpv-full installed (replaced standard mpv)"
            else
                warn "mpv-full install failed - keeping standard mpv"
            fi
        else
            ok "keeping standard mpv (supported)"
        fi
    elif [[ $ASSUME_YES -eq 1 ]]; then
        # Neither installed, -y given: default to the recommended build
        if install_mpv_full; then
            ok "mpv-full installed (recommended default)"
        else
            warn "mpv install failed - install mpv-full (recommended) or standard mpv (sudo pacman -S mpv) manually"
        fi
    elif [[ -t 0 ]]; then
        # Neither installed, interactive: let the user choose
        read -r -p "mpv not installed - install mpv-full (1, recommended) or standard mpv (2)? [1/2] " mpv_choice
        if [[ "${mpv_choice:-1}" == "2" ]]; then
            if sudo pacman -S --needed mpv "${PACMAN_NOCONFIRM[@]}"; then
                ok "standard mpv installed"
            else
                warn "standard mpv install failed"
            fi
        else
            if install_mpv_full; then
                ok "mpv-full installed (recommended)"
            else
                warn "mpv install failed - install mpv-full (recommended) or standard mpv (sudo pacman -S mpv) manually"
            fi
        fi
    else
        warn "mpv not installed - re-run interactively or with -y (installs mpv-full, recommended)"
    fi
    # ---------------------------------------------------------------------------
    build_binary "install rustup/rust first (sudo pacman -S rust)"

    # ---------------------------------------------------------------------------
    ytdlp_step "keep yt-dlp current (pacman -Syu) and consider a logged-in browser profile in" "" "install yt-dlp (sudo pacman -S yt-dlp)"

    # ---------------------------------------------------------------------------
    install_support_scripts
    install_s2u_svc

    # ---------------------------------------------------------------------------
    seed_config_theme

    # ---------------------------------------------------------------------------
    cava_step "pacman -Q cava" "install it (sudo pacman -S cava)"

    # ---------------------------------------------------------------------------
    mpd_fifo_append systemctl --user restart mpd

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
    # ~/.cache/s2udio/mpris-art (the official find_cover returns None for
    # http(s) URLs). A drop-in swaps ExecStart; the stale patched copy from
    # older setups is removed.
    DROPIN_DIR="$HOME/.config/systemd/user/mpDris2.service.d"
    rm -rf "$DROPIN_DIR"   # clear stale drop-ins from older setups (mpris-art.conf, self-heal.conf, ...)
    mkdir -p "$DROPIN_DIR"
    cat > "$DROPIN_DIR/s2udio.conf" <<EOF
# s2udio: run the official mpDris2 through the s2u-mpdris2 shim, which
# serves the stream thumbnail (~/.cache/s2udio/mpris-art) as MPRIS art for
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
    pacman -Q mpv-full >/dev/null 2>&1 && SUMMARY_MPV_FULL=1
    summary_step
}

# ---------------------------------------------------------------------------
run_dnf5() {
    resolve_elevation
    info "Detected distro: ${DISTRO_ID:-?}${DISTRO_ID_LIKE:+ (ID_LIKE=$DISTRO_ID_LIKE)} -> dnf5 backend (Fedora; RPM Fusion free provides mpd/ffmpeg/mpv, plan §12.1)"
    info "1/9  System packages (mpd mpdris2 cava yt-dlp mpv ffmpeg python3-dbus python3-gobject python3-mutagen + toolchain)"
    # Fedora's official repos dropped the `mpd` server — RPM Fusion free is
    # the Fedora analogue of Arch's AUR usage for mpdris2-git (plan §12.1).
    if ! rpm -q rpmfusion-free-release >/dev/null 2>&1; then
        if confirm "Enable RPM Fusion free (provides mpd/full ffmpeg/full mpv on Fedora)? (needs root)"; then
            if "${ELEVATE_CMD[@]}" dnf5 install -y "https://download1.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm"; then
                ok "RPM Fusion free enabled"
            else
                warn "RPM Fusion enable failed - mpd/ffmpeg/mpv may be unavailable"
            fi
        else
            warn "RPM Fusion not enabled - mpd/ffmpeg/mpv may be unavailable"
        fi
    else
        ok "RPM Fusion free already enabled"
    fi
    DNF5_PKGS=(mpd mpdris2 cava yt-dlp mpv ffmpeg python3-dbus python3-gobject python3-mutagen gcc make git curl)
    MISSING=()
    for p in "${DNF5_PKGS[@]}"; do
        rpm -q "$p" >/dev/null 2>&1 || MISSING+=("$p")
    done
    if ((${#MISSING[@]})); then
        if confirm "Install system packages (${MISSING[*]})? (needs root)"; then
            "${ELEVATE_CMD[@]}" dnf5 install -y "${MISSING[@]}"
            ok "installed: ${MISSING[*]}"
        else
            warn "missing packages: ${MISSING[*]}"
        fi
    else
        ok "all system packages present"
    fi

    ensure_rust_toolchain || true
    build_binary "install rustup (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh) or the Fedora rust package"

    ytdlp_step "keep yt-dlp current with dnf (sudo dnf5 upgrade yt-dlp) and consider a logged-in browser profile in" "" "install yt-dlp (sudo dnf5 install yt-dlp)"

    install_support_scripts
    install_s2u_svc
    seed_config_theme
    cava_step "" "install it (sudo dnf5 install cava)"
    ensure_mpd_conf
    mpd_fifo_append "$BIN_DIR/s2u-svc" restart mpd
    services_step_systemd
    mpv_plain_note
    summary_step
}

# ---------------------------------------------------------------------------
run_apt() {
    resolve_elevation
    info "Detected distro: ${DISTRO_ID:-?}${DISTRO_ID_LIKE:+ (ID_LIKE=$DISTRO_ID_LIKE)} -> apt backend (Debian/Ubuntu/Devuan)"
    info "1/9  System packages (mpd mpdris2 cava yt-dlp mpv ffmpeg python3-dbus python3-gi python3-mutagen + toolchain)"
    APT_PKGS=(mpd mpdris2 cava yt-dlp mpv ffmpeg python3-dbus python3-gi python3-mutagen build-essential git curl)
    MISSING=()
    for p in "${APT_PKGS[@]}"; do
        dpkg-query -W "$p" >/dev/null 2>&1 || MISSING+=("$p")
    done
    if ((${#MISSING[@]})); then
        if confirm "Install system packages (${MISSING[*]})? (needs root)"; then
            "${ELEVATE_CMD[@]}" apt-get update -qq
            "${ELEVATE_CMD[@]}" apt-get install -y --no-install-recommends "${MISSING[@]}"
            ok "installed: ${MISSING[*]}"
        else
            warn "missing packages: ${MISSING[*]}"
        fi
    else
        ok "all system packages present"
    fi

    ensure_rust_toolchain || true
    build_binary "install rustup (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh) or the distro rust package"

    # plan §12.7: Debian 12 / Ubuntu 24.04 ship stale yt-dlp pins that fail
    # YouTube resolution — print the pip update hint when the pin is old.
    ytdlp_step "keep yt-dlp current with your package manager and consider a logged-in browser profile in" "distro yt-dlp is outdated - update it with: pip install -U --break-system-packages yt-dlp" "install yt-dlp (sudo apt-get install yt-dlp)"

    install_support_scripts
    install_s2u_svc
    seed_config_theme
    cava_step "" "install it (sudo apt-get install cava)"
    ensure_mpd_conf
    mpd_fifo_append "$BIN_DIR/s2u-svc" restart mpd
    services_step_systemd
    mpv_plain_note
    summary_step
}

# ---------------------------------------------------------------------------
run_apk() {
    resolve_elevation
    info "Detected distro: ${DISTRO_ID:-?}${DISTRO_ID_LIKE:+ (ID_LIKE=$DISTRO_ID_LIKE)} -> apk backend (Alpine)"
    info "1/9  System packages (mpd mpv yt-dlp ffmpeg python3 py3-dbus py3-gobject3 + toolchain; cava from source)"
    APK_PKGS=(mpd mpv yt-dlp ffmpeg python3 py3-dbus py3-gobject3 py3-mutagen py3-pip build-base git curl fftw-dev iniparser-dev ncurses-dev sdl2-dev autoconf automake libtool ncurses-terminfo-base)
    MISSING=()
    for p in "${APK_PKGS[@]}"; do
        apk info -e "$p" >/dev/null 2>&1 || MISSING+=("$p")
    done
    if ((${#MISSING[@]})); then
        if confirm "Install system packages (${MISSING[*]})? (needs root)"; then
            "${ELEVATE_CMD[@]}" apk add --no-cache "${MISSING[@]}"
            ok "installed: ${MISSING[*]}"
        else
            warn "missing packages: ${MISSING[*]}"
        fi
    else
        ok "all system packages present"
    fi
    # cava is NOT in the Alpine 3.20 repos (plan §5 corrected in-container) ->
    # built from source (validated in the alpine-320 harness target).
    if ! command -v cava >/dev/null 2>&1; then
        if confirm "Build cava from source (not in the Alpine repos; needs root + network)?"; then
            if git clone -q --depth 1 https://github.com/karlstav/cava /tmp/cava-src                && (cd /tmp/cava-src && ./autogen.sh >/dev/null && ./configure >/dev/null && make -j"$(nproc)" >/dev/null)                && "${ELEVATE_CMD[@]}" install -Dm755 /tmp/cava-src/cava /usr/local/bin/cava 2>/dev/null; then
                rm -rf /tmp/cava-src
                ok "cava built from source: $(cava --version 2>&1 | head -1)"
            else
                warn "cava source build failed - cava visualizer unavailable (see /tmp/cava-src)"
            fi
        else
            warn "cava not built - cava visualizer unavailable"
        fi
    fi
    # mpDris2 has no Alpine package (plan §12.6): upstream python source at
    # the shim's fixed /usr/bin/mpDris2 + python-mpd2 via pip.
    install_upstream_mpdris2 "no Alpine mpdris2 package"
    if [[ ! -s /usr/bin/mpDris2 ]] && command -v python3 >/dev/null 2>&1; then
        if confirm "Install python-mpd2 via pip (mpDris2 dependency; needs root)?"; then
            "${ELEVATE_CMD[@]}" python3 -m pip install --break-system-packages python-mpd2                 && ok "python-mpd2 installed (pip)" || warn "python-mpd2 install failed (pip)"
        fi
    fi

    ensure_rust_toolchain || true
    build_binary "install rustup (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh) or the Alpine rust package"

    ytdlp_step "keep yt-dlp current with apk (sudo apk upgrade yt-dlp) and consider a logged-in browser profile in" "" "install yt-dlp (sudo apk add yt-dlp)"

    install_support_scripts
    install_s2u_svc
    seed_config_theme
    cava_step "" "install it or rebuild from source (see step 1)"
    ensure_mpd_conf
    mpd_fifo_append "$BIN_DIR/s2u-svc" restart mpd
    services_step_launcher
    mpv_plain_note
    summary_step
}

# ---------------------------------------------------------------------------
run_xbps() {
    resolve_elevation
    info "Detected distro: ${DISTRO_ID:-?}${DISTRO_ID_LIKE:+ (ID_LIKE=$DISTRO_ID_LIKE)} -> xbps backend (Void)"
    info "1/9  System packages (mpd mpv yt-dlp cava ffmpeg mpDris2 python3-dbus python3-gobject + toolchain)"
    # runit-user backend prerequisites: sv + runsvdir. No-op on real Void
    # hosts (runit is the init system, already installed); without it the
    # service step silently degrades to the launcher backend (seen in the
    # first setup-void-glibc run) instead of supervising via runsvdir/sv.
    XBPS_PKGS=(mpd mpv yt-dlp cava ffmpeg mpDris2 python3 python3-dbus python3-gobject python3-mutagen python3-mpd2 base-devel cargo rust git curl util-linux procps-ng ncurses-term runit)
    MISSING=()
    for p in "${XBPS_PKGS[@]}"; do
        xbps-query "$p" >/dev/null 2>&1 || MISSING+=("$p")
    done
    if ((${#MISSING[@]})); then
        if confirm "Install system packages (${MISSING[*]})? (needs root)"; then
            "${ELEVATE_CMD[@]}" xbps-install -Sy "${MISSING[@]}"
            ok "installed: ${MISSING[*]}"
        else
            warn "missing packages: ${MISSING[*]}"
        fi
    else
        ok "all system packages present"
    fi
    # plan §12.8: Void's mpd ships file caps (cap_ipc_lock,cap_sys_nice=eip)
    # that restricted environments (containers) cannot grant -> execve EPERM.
    # Stripping them is harmless on real Void hosts.
    if command -v setcap >/dev/null 2>&1; then
        if confirm "Strip mpd file capabilities (setcap -r /usr/bin/mpd; needed in restricted environments)?"; then
            "${ELEVATE_CMD[@]}" setcap -r /usr/bin/mpd 2>/dev/null || true
            ok "mpd file caps stripped (setcap -r /usr/bin/mpd)"
        fi
    fi

    # Void ships rustc 1.97.1 (>= MSRV 1.88) — no rustup needed (plan §12).
    ensure_rust_toolchain || true
    build_binary "install rust (sudo xbps-install -S rust) or rustup (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh)"

    ytdlp_step "keep yt-dlp current with xbps (sudo xbps-install -Su yt-dlp) and consider a logged-in browser profile in" "" "install yt-dlp (sudo xbps-install -S yt-dlp)"

    install_support_scripts
    install_s2u_svc
    seed_config_theme
    cava_step "" "install it (sudo xbps-install -S cava)"
    ensure_mpd_conf
    mpd_fifo_append "$BIN_DIR/s2u-svc" restart mpd
    services_step_runit
    mpv_plain_note
    summary_step
}

# ---------------------------------------------------------------------------
run_nix() {
    resolve_elevation
    info "Detected distro: ${DISTRO_ID:-?}${DISTRO_ID_LIKE:+ (ID_LIKE=$DISTRO_ID_LIKE)} -> nix backend (nix profile install, flake.nix)"
    ensure_nix_flakes() {
        if ! grep -q 'experimental-features' "$HOME/.config/nix/nix.conf" 2>/dev/null; then
            mkdir -p "$HOME/.config/nix"
            printf 'experimental-features = nix-command flakes\n' >> "$HOME/.config/nix/nix.conf"
            ok "nix flakes enabled (~/.config/nix/nix.conf)"
        fi
    }
    ensure_nix_flakes

    info "1/9  nix profile install (flake.nix: s2udio + bridgePython + runtime deps)"
    NIX_RUNTIME_DEPS="nixpkgs#mpd nixpkgs#mpv nixpkgs#yt-dlp nixpkgs#cava nixpkgs#mpdris2 nixpkgs#ffmpeg nixpkgs#tmux nixpkgs#dbus nixpkgs#procps nixpkgs#systemd nixpkgs#gnused nixpkgs#gawk nixpkgs#util-linux nixpkgs#rustc nixpkgs#cargo nixpkgs#gcc nixpkgs#gnumake"
    if confirm "Install s2udio + runtime deps via 'nix profile install' (flake.nix; needs network)?"; then
        # remove stale same-name entries first (nix profile install refuses
        # to upgrade an existing name; fresh profiles have none - no-op)
        nix profile remove s2udio 2>/dev/null || true
        nix profile remove bridgePython 2>/dev/null || true
        nix profile install .#s2udio .#bridgePython
        nix profile install $NIX_RUNTIME_DEPS
        ok "nix profile install complete (~/.nix-profile/bin/s2udio)"
    else
        warn "nix profile install skipped - install manually: nix profile install .#s2udio .#bridgePython"
    fi

    # the flake package builds the binary (build.rs/vergen handled inside the
    # nix sandbox) — no local cargo build needed
    info "2/9  Build the s2udio binary"
    # grep -q would exit at the first match and SIGPIPE nix (nix exits 1 on a
    # closed stdout) -> the pipefail pipeline fails even when s2udio IS in the
    # profile. Read the full list so nix exits cleanly; match on the full
    # output (grep without -q, stdout to /dev/null).
    if command -v nix >/dev/null 2>&1 && nix profile list 2>/dev/null | grep s2udio >/dev/null; then
        ok "binary comes from the flake package (~/.nix-profile/bin/s2udio)"
    else
        warn "s2udio not in the nix profile yet - re-run with -y or install manually"
    fi

    ytdlp_step "keep yt-dlp current with nix (nix profile upgrade yt-dlp) and consider a logged-in browser profile in" "" "install nixpkgs#yt-dlp (nix profile install nixpkgs#yt-dlp)"

    install_support_scripts
    install_s2u_svc
    seed_config_theme
    cava_step "" "install it (nix profile install nixpkgs#cava)"
    ensure_mpd_conf
    mpd_fifo_append "$BIN_DIR/s2u-svc" restart mpd

    # nixpkgs ships mpDris2 as a compiled ELF the s2u-mpdris2 shim cannot
    # patch (plan §5 decision point) -> upstream python source at /usr/bin.
    install_upstream_mpdris2 "nixpkgs mpDris2 is a compiled ELF the s2u-mpdris2 shim cannot patch"
    services_step_launcher
    mpv_plain_note
    SUMMARY_BIN="$HOME/.nix-profile/bin/s2udio"
    summary_step
}


# ---------------------------------------------------------------------------
# Dispatch
BACKEND="$(detect_backend)"
case "$BACKEND" in
    pacman) run_arch ;;
    dnf5)   run_dnf5 ;;
    apt)    run_apt ;;
    apk)    run_apk ;;
    xbps)   run_xbps ;;
    nix)    run_nix ;;
    unknown)
        die "unsupported distro (ID=${DISTRO_ID:-?} ID_LIKE=${DISTRO_ID_LIKE:-}) - s2udio supports Arch/CachyOS/Artix, Fedora, Debian/Ubuntu/Devuan, Alpine, Void, NixOS (see docs/design/Validation/distro-support.md)"
        ;;
esac

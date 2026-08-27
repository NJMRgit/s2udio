#!/usr/bin/env bash
# deploy-common.sh — sourced by scripts/dev/containers/<key>/deploy.sh.
# Mirrors setup.sh's steps for non-Arch targets (plan §5/§6.2): support
# scripts, config/theme seed, cava config (PipeWire input — round 30),
# user-level MPD + mpDris2 services via the systemd-user backend of
# s2u-svc, and test media for the gates. mpv-full stays an Arch-only
# branch (setup.sh); every other target installs plain mpv (done in
# provision.sh) and gets the informational note below.
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "deploy: support scripts"
ensure_local_bin_path
BIN_DIR="$HOME/.local/bin"
CFG_DIR="$HOME/.config/s2udio"
MPD_CONF="$HOME/.config/mpd/mpd.conf"
mkdir -p "$BIN_DIR" "$CFG_DIR/themes" "$CFG_DIR/lyrics" \
         "$HOME/.config/mpv/scripts" "$HOME/.config/mpd" "$HOME/.config/cava" \
         "$HOME/.config/systemd/user/mpDris2.service.d" "$HOME/media"
# The mpv IPC socket contract (SVP4 round): seed mpv.conf when absent so a
# manually launched mpv exposes /tmp/mpvsocket too (G1 checks this).
if [[ ! -f "$HOME/.config/mpv/mpv.conf" ]] \
    || ! grep -q 'input-ipc-server=/tmp/mpvsocket' "$HOME/.config/mpv/mpv.conf" 2>/dev/null; then
    printf 'input-ipc-server=/tmp/mpvsocket\n' >> "$HOME/.config/mpv/mpv.conf"
    ok "mpv.conf: input-ipc-server=/tmp/mpvsocket seeded"
fi

for s in rmpc-fetch-lyrics s2u-mpv-tracker s2udio-mpris s2u-mpdris2 s2u-svc; do
    install -Dm755 "/s2udio/scripts/$s" "$BIN_DIR/$s"
    ok "script -> $BIN_DIR/$s"
done

info "deploy: install the built binary (target/release/s2u -> ~/.local/bin/s2udio)"
if [[ -x /s2udio/target/release/s2u ]]; then
    install -Dm755 /s2udio/target/release/s2u "$BIN_DIR/s2udio"
    ok "binary -> $BIN_DIR/s2udio ($("$BIN_DIR/s2udio" version 2>/dev/null | head -1))"
else
    warn "target/release/s2u missing — binary not installed (G2 will fail)"
fi

info "deploy: seed config + theme (only if absent)"
if [[ ! -f "$CFG_DIR/config.ron" ]]; then
    cp /s2udio/assets/example_config.ron "$CFG_DIR/config.ron"; ok "config -> ~/.config/s2udio/config.ron"
fi
if [[ ! -f "$CFG_DIR/themes/default.ron" ]]; then
    cp /s2udio/assets/example_theme.ron "$CFG_DIR/themes/default.ron"; ok "theme -> ~/.config/s2udio/themes/default.ron"
fi

info "deploy: MPD config (user-level; round 30: no cava fifo output — cava captures PipeWire)"
if [[ ! -f "$MPD_CONF" ]]; then
    mkdir -p "$HOME/.cache/mpd"
    cat > "$MPD_CONF" <<'EOF'
music_directory "/root/media"
bind_to_address "127.0.0.1"
port "6600"
db_file "/root/.cache/mpd/database"
state_file "/root/.cache/mpd/state"
sticker_file "/root/.cache/mpd/sticker.sql"
playlist_directory "/root/.cache/mpd/playlists"
follow_outside_symlinks "yes"
follow_inside_symlinks "yes"
auto_update "yes"
EOF
fi
ok "mpd.conf ready (user-level instance)"

info "deploy: cava config (PipeWire input)"
cat > "$HOME/.config/cava/config" <<'EOF'
[general]
bars = 24
[input]
method = pipewire
source = auto
[output]
method = raw
EOF
ok "cava config -> ~/.config/cava/config"

info "deploy: user-level MPD unit"
cat > "$HOME/.config/systemd/user/mpd.service" <<'EOF'
[Unit]
Description=Music Player Daemon (s2udio user instance)
After=network.target
[Service]
ExecStart=/usr/bin/mpd --no-daemon /root/.config/mpd/mpd.conf
Restart=on-failure
[Install]
WantedBy=default.target
EOF
ok "mpd.service (user) written"

info "deploy: mpDris2 drop-in (official mpDris2 through the s2u-mpdris2 shim)"
cat > "$HOME/.config/systemd/user/mpDris2.service.d/s2udio.conf" <<EOF
[Service]
ExecStart=
ExecStart=$BIN_DIR/s2u-mpdris2 --use-journal
EOF
# The packaged mpDris2 unit may not exist on every distro (Debian/Ubuntu
# ship one; Fedora's mpdris2 does not) — provide ours so the drop-in has
# a unit to patch.
if [[ ! -f "$HOME/.config/systemd/user/mpDris2.service" ]] \
   && ! systemctl --user list-unit-files 2>/dev/null | grep -q '^mpDris2.service'; then
    cat > "$HOME/.config/systemd/user/mpDris2.service" <<'EOF'
[Unit]
Description=MPRIS bridge for MPD (s2udio s2u-mpdris2 shim)
After=mpd.service
[Service]
ExecStart=/root/.local/bin/s2u-mpdris2 --use-journal
Restart=on-failure
[Install]
WantedBy=default.target
EOF
    ok "mpDris2.service (user) written (no packaged unit on this distro)"
fi

info "deploy: test media (ffmpeg-generated, for gates G5/G8)"
if [[ ! -f "$HOME/media/test.mp3" ]]; then
    ffmpeg -hide_banner -loglevel error -y -f lavfi -i "sine=frequency=440:duration=30" \
        -c:a libmp3lame -metadata title="S2U Test Tone" -metadata artist="S2U Harness" \
        "$HOME/media/test.mp3" || \
    ffmpeg -hide_banner -loglevel error -y -f lavfi -i "sine=frequency=440:duration=30" \
        -c:a mp2 -metadata title="S2U Test Tone" -metadata artist="S2U Harness" \
        "$HOME/media/test.mp3"
fi
if [[ ! -f "$HOME/media/test.mp4" ]]; then
    ffmpeg -hide_banner -loglevel error -y \
        -f lavfi -i "testsrc=duration=30:size=320x240:rate=15" \
        -f lavfi -i "sine=frequency=440:duration=30" \
        -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest \
        -metadata title="S2U Test Video" -metadata artist="S2U Harness" \
        "$HOME/media/test.mp4" || \
    ffmpeg -hide_banner -loglevel error -y \
        -f lavfi -i "testsrc=duration=30:size=320x240:rate=15" \
        -f lavfi -i "sine=frequency=440:duration=30" \
        -c:v mpeg4 -c:a mp2 -shortest \
        -metadata title="S2U Test Video" -metadata artist="S2U Harness" \
        "$HOME/media/test.mp4"
fi
ls -la "$HOME/media"

info "deploy: s2u-svc G11 test unit"
cat > "$HOME/.config/systemd/user/s2u-svc-g11.service" <<'EOF'
[Unit]
Description=s2u-svc round-trip test unit
[Service]
ExecStart=/bin/sleep 300
[Install]
WantedBy=default.target
EOF

info "deploy: mpv note (plain mpv on this target; mpv-full is Arch-only)"
warn "Arch-only mpv-full recommendation not applicable here — plain mpv installed (plan §5)"

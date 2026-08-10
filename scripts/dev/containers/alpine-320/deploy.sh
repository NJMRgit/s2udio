#!/usr/bin/env bash
# Alpine 3.20 deploy — launcher-backend services (OpenRC distro, but the
# s2udio model is user-level services; plan §6.1 sends OpenRC through the
# plain-launcher). mpDris2: no Alpine package -> upstream python source at
# the shim's fixed /usr/bin/mpDris2 path (plan §5 decision point).
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "alpine: install the built binary (target/release/s2u -> ~/.local/bin/s2udio)"
if [[ -x /s2udio/target/release/s2u ]]; then
    install -Dm755 /s2udio/target/release/s2u "$HOME/.local/bin/s2udio"
    ok "binary -> $HOME/.local/bin/s2udio ($("$HOME/.local/bin/s2udio" version 2>/dev/null | head -1))"
else
    warn "target/release/s2u missing — binary not installed (G2 will fail)"
fi

info "alpine: install support scripts + config seed"
BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR" "$HOME/.config/s2udio/themes" "$HOME/.config/s2udio/lyrics" \
         "$HOME/.config/mpv/scripts" "$HOME/.config/mpd" "$HOME/.config/cava" \
         "$HOME/.cache/mpd/playlists" "$HOME/media"
for s in rmpc-fetch-lyrics s2u-mpv-tracker s2udio-mpris s2u-mpdris2 s2u-svc; do
    install -Dm755 "/s2udio/scripts/$s" "$BIN_DIR/$s"
done
install -Dm644 /s2udio/scripts/mpvSockets.lua "$HOME/.config/mpv/scripts/mpvSockets.lua"
cp -n /s2udio/assets/example_config.ron "$HOME/.config/s2udio/config.ron" || true
cp -n /s2udio/assets/example_theme.ron "$HOME/.config/s2udio/themes/default.ron" || true

info "alpine: mpDris2 shim needs /usr/bin/mpDris2 as SOURCE (no Alpine package)"
curl -fsSL --max-time 60 \
    https://raw.githubusercontent.com/eonpatapon/mpDris2/master/src/mpDris2.in.py \
    -o /tmp/mpDris2.in.py || warn "upstream mpDris2 fetch failed"
if [[ -s /tmp/mpDris2.in.py ]]; then
    sed -e 's/@version@/0.9.1/g' -e 's/@gitversion@/0.9.1/g' -e 's|@datadir@|/usr/share|g' \
        /tmp/mpDris2.in.py > /usr/bin/mpDris2
    chmod +x /usr/bin/mpDris2
    ok "upstream mpDris2 source -> /usr/bin/mpDris2 ($(wc -l < /usr/bin/mpDris2) lines)"
fi

info "alpine: MPD config (user-level, fifo for cava)"
cat > "$HOME/.config/mpd/mpd.conf" <<'EOF'
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
audio_output {
    type    "fifo"
    name    "cava"
    path    "/tmp/mpd-cava.fifo"
    format  "44100:16:2"
}
EOF

info "alpine: cava config (fifo input, raw output)"
cat > "$HOME/.config/cava/config" <<'EOF'
[general]
bars = 24
[input]
method = fifo
source = /tmp/mpd-cava.fifo
sample_rate = 44100
sample_bits = 16
channels = 2
[output]
method = raw
EOF

info "alpine: test media"
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

info "alpine: session + services (launcher backend — no systemd in the container)"
start_user_session
s2u-svc start mpd
sleep 2
s2u-svc is-active mpd && ok "mpd active (launcher)" || warn "mpd not active"
s2u-svc start mpDris2
sleep 2
s2u-svc is-active mpDris2 && ok "mpDris2 active (launcher)" || warn "mpDris2 not active"
warn "Arch-only mpv-full recommendation not applicable here — plain mpv (plan §5)"
ok "alpine-320 deploy complete"

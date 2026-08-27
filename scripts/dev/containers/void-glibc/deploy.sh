#!/usr/bin/env bash
# Void glibc deploy — runit-user backend: per-user service dirs under
# ~/.config/runit, one runsvdir supervising them; s2u-svc's runit-user
# backend drives them via sv(1). mpDris2 from the Void repos (python
# source; the s2u-mpdris2 shim patches it — /usr/bin/mpDris2 fixed path).
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "void: install the built binary (target/release/s2u -> ~/.local/bin/s2udio)"
if [[ -x /s2udio/target/release/s2u ]]; then
    install -Dm755 /s2udio/target/release/s2u "$HOME/.local/bin/s2udio"
    ok "binary -> $HOME/.local/bin/s2udio ($("$HOME/.local/bin/s2udio" version 2>/dev/null | head -1))"
else
    warn "target/release/s2u missing — binary not installed (G2 will fail)"
fi

info "void: install support scripts + config seed"
BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR" "$HOME/.config/s2udio/themes" "$HOME/.config/s2udio/lyrics" \
         "$HOME/.config/mpv/scripts" "$HOME/.config/mpd" "$HOME/.config/cava" \
         "$HOME/.cache/mpd/playlists" "$HOME/media" "$HOME/.config/runit/mpd" \
         "$HOME/.config/runit/mpDris2"
for s in rmpc-fetch-lyrics s2u-mpv-tracker s2udio-mpris s2u-mpdris2 s2u-svc; do
    install -Dm755 "/s2udio/scripts/$s" "$BIN_DIR/$s"
done
cp -n /s2udio/assets/example_config.ron "$HOME/.config/s2udio/config.ron" || true
cp -n /s2udio/assets/example_theme.ron "$HOME/.config/s2udio/themes/default.ron" || true

info "void: mpDris2 from the Void repos (python source — shim-compatible)"
head -1 /usr/bin/mpDris2 && chmod +x /usr/bin/mpDris2

info "void: MPD config (user-level; round 30: no cava fifo output — cava captures PipeWire)"
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
EOF

info "void: cava config (PipeWire input, raw output)"
cat > "$HOME/.config/cava/config" <<'EOF'
[general]
bars = 24
[input]
method = pipewire
source = auto
[output]
method = raw
EOF

info "void: test media"
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

info "void: runit service dirs (per-user runsvdir under ~/.config/runit)"
cat > "$HOME/.config/runit/mpd/run" <<EOF
#!/bin/sh
exec /usr/bin/mpd --no-daemon /root/.config/mpd/mpd.conf
EOF
cat > "$HOME/.config/runit/mpDris2/run" <<EOF
#!/bin/sh
exec /root/.local/bin/s2u-mpdris2 --use-journal
EOF
chmod +x "$HOME/.config/runit/mpd/run" "$HOME/.config/runit/mpDris2/run"

info "void: session + runsvdir + services (runit-user backend)"
start_user_session
setsid runsvdir "$HOME/.config/runit" >/dev/null 2>&1 &
for _ in $(seq 1 10); do
    [[ -d "$HOME/.config/runit/mpd/supervise" ]] && break
    sleep 0.5
done
s2u-svc start mpd
sleep 2
s2u-svc is-active mpd && ok "mpd active (runit-user)" || warn "mpd not active"
s2u-svc start mpDris2
sleep 2
s2u-svc is-active mpDris2 && ok "mpDris2 active (runit-user)" || warn "mpDris2 not active"
warn "Arch-only mpv-full recommendation not applicable here — plain mpv (plan §5)"
ok "void-glibc deploy complete"

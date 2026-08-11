#!/usr/bin/env bash
# Nix deploy — nix profile install of the flake package (+ bridge python +
# runtime deps), config/theme/media seed, and launcher-backend services
# (this container has no systemd — the s2u-svc plain-launcher is the
# service backend, plan §6.1).
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "nix: profile install (flake package + bridge python + runtime deps)"
cd /s2udio
# remove stale same-name entries first (nix profile install refuses to
# upgrade an existing name; `remove` takes a regex — one call per name;
# fresh containers have none and this is a no-op)
nix profile remove s2udio 2>/dev/null || true
nix profile remove bridgePython 2>/dev/null || true
nix profile install .#s2udio .#bridgePython
nix profile install \
    nixpkgs#mpd nixpkgs#mpv nixpkgs#yt-dlp nixpkgs#cava nixpkgs#mpdris2 \
    nixpkgs#ffmpeg nixpkgs#tmux nixpkgs#dbus nixpkgs#procps nixpkgs#systemd \
    nixpkgs#gnused nixpkgs#gawk nixpkgs#util-linux \
    nixpkgs#rustc nixpkgs#cargo nixpkgs#gcc nixpkgs#gnumake
export PATH="/root/.nix-profile/bin:$PATH"

info "nix: mpDris2 shim needs /usr/bin/mpDris2 as SOURCE (hardcoded lookup)"
# nixpkgs ships mpDris2 as a compiled ELF (nuitka-style) — the s2u-mpdris2
# shim patches the python source, so per plan §5's decision point we install
# the upstream python mpDris2 (eonpatapon/mpDris2) at the fixed path. Its
# Notify import is already guarded upstream (degrades without libnotify).
if [[ -d /usr/bin ]]; then
    mkdir -p /usr/bin
fi
curl -fsSL --max-time 60 \
    https://raw.githubusercontent.com/eonpatapon/mpDris2/master/src/mpDris2.in.py \
    -o /tmp/mpDris2.in.py || { warn "upstream mpDris2 fetch failed"; }
if [[ -s /tmp/mpDris2.in.py ]]; then
    sed -e 's/@version@/0.9.1/g' -e 's/@gitversion@/0.9.1/g' -e 's|@datadir@|/usr/share|g' \
        /tmp/mpDris2.in.py > /usr/bin/mpDris2
    chmod +x /usr/bin/mpDris2
    ok "upstream mpDris2 source -> /usr/bin/mpDris2 ($(wc -l < /usr/bin/mpDris2) lines)"
else
    ln -sf "$(command -v mpDris2)" /usr/bin/mpDris2
    warn "fell back to the nixpkgs ELF mpDris2 (shim source-patching will fail)"
fi

info "nix: config/theme/mpvSockets seed"
mkdir -p "$HOME/.config/s2udio/themes" "$HOME/.config/s2udio/lyrics" \
         "$HOME/.config/mpv/scripts" "$HOME/.config/mpd" "$HOME/.config/cava" \
         "$HOME/.cache/mpd/playlists" "$HOME/media" "$HOME/.local/bin"
cp -n /root/.nix-profile/share/s2udio/example_config.ron "$HOME/.config/s2udio/config.ron" || true
cp -n /root/.nix-profile/share/s2udio/example_theme.ron "$HOME/.config/s2udio/themes/default.ron" || true
cp /root/.nix-profile/share/s2udio/mpvSockets.lua "$HOME/.config/mpv/scripts/mpvSockets.lua"

info "nix: MPD config (user-level, fifo for cava)"
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

info "nix: cava config (fifo input, raw output)"
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

info "nix: test media"
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

info "nix: session + services (launcher backend — no systemd here)"
start_user_session
s2u-svc start mpd
sleep 2
s2u-svc is-active mpd && ok "mpd active (launcher)" || warn "mpd not active"
s2u-svc start mpDris2
sleep 2
s2u-svc is-active mpDris2 && ok "mpDris2 active (launcher)" || warn "mpDris2 not active"
warn "Arch-only mpv-full recommendation not applicable here — nixpkgs mpv (plan §5)"
ok "nix deploy complete"

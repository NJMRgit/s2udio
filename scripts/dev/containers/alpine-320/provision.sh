#!/usr/bin/env bash
# Alpine 3.20 provision — package map (plan §5, verified in-container) +
# rustup (Alpine rust is too old for edition-2024) + git repo for vergen.
# Deltas: cava is NOT packaged in Alpine 3.20 (plan's risk table assumed it
# is) -> built from source here; mpdris2 is absent -> upstream source in
# deploy.sh (plan §5 decision point).
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "alpine: system packages"
apk update -q
apk add -q --no-cache \
    mpd mpv yt-dlp ffmpeg python3 py3-dbus py3-gobject3 py3-mutagen \
    tmux dbus procps curl git build-base autoconf automake libtool \
    fftw-dev iniparser-dev ncurses-dev sdl2-dev ncurses-terminfo-base py3-pip

info "alpine: cava from source (absent from the 3.20 repos)"
if ! command -v cava >/dev/null 2>&1; then
    git clone -q --depth 1 https://github.com/karlstav/cava /tmp/cava-src
    cd /tmp/cava-src
    ./autogen.sh >/dev/null
    ./configure >/dev/null
    make -j"$(nproc)" >/dev/null
    install -Dm755 cava /usr/local/bin/cava
    cd /
    rm -rf /tmp/cava-src
    ok "cava built from source: $(cava --version 2>&1 | head -1)"
fi

info "alpine: python-mpd2 via pip (no Alpine package; mpDris2 upstream needs it)"
pip3 install -q --break-system-packages python-mpd2 2>&1 | tail -1 || true
python3 - <<'EOF'
import dbus  # py3-dbus
import gi    # py3-gobject3
import mpd   # python-mpd2 (pip)
print("python dbus+gi+mpd2 import OK")
EOF

info "alpine: rust toolchain via rustup (distro rust too old for edition-2024)"
if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
export PATH="$HOME/.cargo/bin:$PATH"
# the cargo cache volume can persist the ~/.cargo shims from an earlier run
# while the toolchain (~/.rustup) is container-local — ensure a default
rustup default stable >/dev/null 2>&1 || rustup toolchain install stable --profile minimal >/dev/null 2>&1 || true
rustc --version && cargo --version

info "alpine: git repo for the copied tree (vergen build dependency)"
cd /s2udio
git config --global --add safe.directory /s2udio
if [[ ! -d .git ]]; then
    git init -q
    git -c user.email=harness@localhost -c user.name="s2udio harness" add -A
    git -c user.email=harness@localhost -c user.name="s2udio harness" commit -qm "harness container snapshot"
fi
ok "alpine-320 provision complete"

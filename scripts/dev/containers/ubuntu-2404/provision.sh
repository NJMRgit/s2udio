#!/usr/bin/env bash
# Ubuntu 24.04 provision — packages + rust toolchain (plan §5 package map).
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "Ubuntu 24.04: installing system packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends     mpd mpdris2 cava yt-dlp mpv ffmpeg     python3-dbus python3-gi python3-mutagen     build-essential git curl tmux ncurses-term procps     dbus systemd ca-certificates     >/dev/null
ok "system packages installed"

info "Debian 12: verifying package names resolve"
for b in mpd mpDris2 cava yt-dlp mpv ffmpeg ffprobe; do
    command -v "$b" >/dev/null || { warn "missing binary: $b"; }
done
python3 - <<'EOF'
import dbus  # python3-dbus
import gi    # python3-gi
print("python dbus+gi import OK")
EOF


# ---- rust toolchain via rustup (distro rustc is too old for edition-2024:
# Fedora 41 ~1.80, Debian 12 1.63, Ubuntu 24.04 1.75; Cargo.toml needs 1.88+) ----
if ! command -v cargo >/dev/null 2>&1; then
    info "installing rustup (stable, minimal profile)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
export PATH="$HOME/.cargo/bin:$PATH"
# the cargo cache volume can persist the ~/.cargo shims from an earlier run
# while the toolchain (~/.rustup) is container-local — ensure a default
rustup default stable >/dev/null 2>&1 || rustup toolchain install stable --profile minimal >/dev/null 2>&1 || true
rustc --version && cargo --version

# ---- make the copied tree a git repo (vergen_gitcl bakes git info at build
# time and FAILS the build without it; the host tree keeps no .git here) ----
cd /s2udio
git config --global --add safe.directory /s2udio
if [[ ! -d .git ]]; then
    git init -q
    git -c user.email=harness@localhost -c user.name="s2udio harness" add -A
    git -c user.email=harness@localhost -c user.name="s2udio harness" commit -qm "harness container snapshot"
fi

ok "ubuntu-2404 provision complete"

#!/usr/bin/env bash
# Fedora 41 provision — packages + rust toolchain (plan §5 package map).
# Runs inside the container as root. RPM Fusion free was enabled in the
# image build (Fedora dropped the `mpd` server from the official repos; the
# plan's decision point — Arch uses AUR for mpdris2-git, this is the Fedora
# analogue). mpv/ffmpeg come from RPM Fusion for the full-featured build.
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "Fedora 41: installing system packages (mpd mpdris2 cava yt-dlp mpv ffmpeg + toolchain)"
dnf -y install --nogpgcheck     mpd mpdris2 cava yt-dlp mpv ffmpeg     python3-dbus python3-gobject python3-mutagen     gcc make git curl tmux ncurses-term procps-ng dbus-daemon     >/dev/null
ok "system packages installed"

info "Fedora 41: verifying package names resolve"
for b in mpd mpDris2 cava yt-dlp mpv ffmpeg ffprobe; do
    command -v "$b" >/dev/null || { warn "missing binary: $b"; }
done
python3 - <<'EOF'
import dbus  # python3-dbus
import gi    # python3-gobject
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

ok "fedora-41 provision complete"

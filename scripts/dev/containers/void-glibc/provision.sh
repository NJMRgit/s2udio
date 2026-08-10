#!/usr/bin/env bash
# Void glibc provision — package map (plan §5, verified in-container) +
# distro rust (Void ships rustc 1.97.1 — new enough for edition-2024) +
# git repo for vergen. mpDris2 comes from the Void repos (python source —
# shim-compatible).
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "void: system packages"
xbps-install -Sy >/dev/null 2>&1
xbps-install -y \
    mpd mpv yt-dlp cava ffmpeg mpDris2 \
    python3 python3-dbus python3-gobject python3-mutagen python3-mpd2 \
    tmux dbus procps-ng curl git base-devel cargo rust runit util-linux \
    ncurses-term findutils
    >/dev/null

info "void: strip mpd file capabilities (container bounding set lacks cap_ipc_lock/cap_sys_nice -> execve EPERM)"
# Void's mpd ships cap_ipc_lock,cap_sys_nice=eip for realtime/priority; rootless
# podman containers don't grant them in the bounding set, so exec fails with
# EPERM. On a real Void host these caps are fine — this is harness-only.
command -v setcap >/dev/null 2>&1 && setcap -r /usr/bin/mpd 2>/dev/null || true
mpd --version 2>&1 | head -1

info "void: verifying python dbus/gi"
python3 - <<'EOF'
import dbus  # python3-dbus
import gi    # python3-gobject
print("python dbus+gi import OK")
EOF

info "void: rust toolchain (distro rustc)"
rustc --version && cargo --version

info "void: git repo for the copied tree (vergen build dependency)"
cd /s2udio
git config --global --add safe.directory /s2udio
if [[ ! -d .git ]]; then
    git init -q
    git -c user.email=harness@localhost -c user.name="s2udio harness" add -A
    git -c user.email=harness@localhost -c user.name="s2udio harness" commit -qm "harness container snapshot"
fi
ok "void-glibc provision complete"

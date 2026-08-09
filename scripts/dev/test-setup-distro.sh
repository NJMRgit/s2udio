#!/usr/bin/env bash
# test-setup-distro.sh <matrix-key> [--no-cache-vol] [--artifacts DIR] [--gate Gx]
#
# Real in-container validation of the NEW setup.sh distro dispatcher (plan
# docs/design/Validation/distro-support.md §6.2): run `setup.sh -y` inside the
# ephemeral harness container for the target and assert the end state
# (binary, support scripts, services via s2u-svc, mpd.conf fifo, per-distro
# deltas). Same ephemerality discipline as test-distro.sh (--rm, EXIT trap,
# start-of-run sweep, end-of-run G12 assertion).
#
# T5 (distro-support follow-ups): after the end-state checks the driver runs a
# gate-prep step (harness test tooling + ffmpeg test media in the configured
# music_directory with ~/media symlinked to it + cava config + the s2u-svc G11
# round-trip unit, deploy-common.sh media/cava glue — NOT its unit/drop-in
# parts, which setup.sh owns) and then the FULL feature gate loop G1..G11 via
# scripts/dev/gates/run-gates.sh inside the setup.sh-provisioned container
# (plan distro-support-followups.md T5).
#
# Usage: scripts/dev/test-setup-distro.sh <key> [--no-sudo] [--no-cache-vol] [--artifacts DIR] [--gate Gx ...]
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DEV_DIR="$HERE"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
source "$HERE/lib.sh"

KEY="${1:-}"
[[ -n "$KEY" ]] || die "usage: test-setup-distro.sh <matrix-key> [--no-cache-vol] [--artifacts DIR]"
shift

NO_CACHE_VOL=0
ART_DIR_OVERRIDE=""
NO_SUDO_SHIM=0
GATE_FILTER=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-cache-vol) NO_CACHE_VOL=1; shift ;;
        --no-sudo) NO_SUDO_SHIM=1; shift ;;
        --gate) GATE_FILTER+=("$2"); shift 2 ;;
        --artifacts) ART_DIR_OVERRIDE="$2"; shift 2 ;;
        *) die "unknown argument: $1" ;;
    esac
done

TARGET_DIR="$DEV_DIR/containers/$KEY"
[[ -d "$TARGET_DIR" ]] || die "no target definition at $TARGET_DIR"
[[ -f "$TARGET_DIR/image.env" ]] || die "missing $TARGET_DIR/image.env"
source "$TARGET_DIR/image.env"

ART_DIR="${ART_DIR_OVERRIDE:-$DEV_DIR/artifacts/setup-$KEY/$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$ART_DIR"
RUN_LOG="$ART_DIR/run.log"
exec > >(tee -a "$RUN_LOG") 2>&1
export ART_DIR

CID=""
cleanup() {
    local rc=$?
    if [[ -n "$CID" ]]; then
        podman cp "$CID:/root/.local/bin/." "$ART_DIR/bin/" >/dev/null 2>&1 || true
        podman rm -f "$CID" >/dev/null 2>&1 || true
        CID=""
    fi
    local remaining
    remaining="$(podman ps -a --filter name=s2u-distro- --format '{{.Names}}' 2>/dev/null | grep -v '^$' || true)"
    if [[ -n "$remaining" ]]; then
        write_gate G12 fail "EPHEMERALITY VIOLATION: s2u-distro-* containers remain: $(echo $remaining)"
        exit 1
    fi
    write_gate G12 pass "no s2u-distro-* containers remain (--rm + trap + sweep + assertion)"
    exit "$rc"
}
trap cleanup EXIT

info "s2udio setup.sh in-container validation — target: $KEY (image $IMAGE, ${INIT_MODE})"

# ---- 1. sweep ----
STALE="$(podman ps -aq --filter name=s2u-distro- 2>/dev/null || true)"
if [[ -n "${STALE// }" ]]; then
    podman rm -f $STALE >/dev/null 2>&1 || true
    warn "sweep removed stale containers: $(echo $STALE)"
else
    ok "no stale containers"
fi
write_gate G0 pass "start-of-run sweep ran; container created with --rm + EXIT trap"

# ---- 2. create ----
podman build -q -t "localhost/s2u-distro-$KEY:latest" "$TARGET_DIR" >/dev/null
VOL_ARGS=()
if [[ $NO_CACHE_VOL -eq 0 ]]; then
    podman volume create "s2u-cargo-$KEY" >/dev/null 2>&1 || true
    VOL_ARGS=(-v "s2u-cargo-$KEY:/root/.cargo")
fi
SYSTEMD_ARGS=()
[[ "$INIT_MODE" == "systemd" ]] && SYSTEMD_ARGS=(--systemd=always)
CID="$(podman run --rm -d --name "s2u-distro-$KEY" --shm-size=512m "${SYSTEMD_ARGS[@]}" "${VOL_ARGS[@]}"         "localhost/s2u-distro-$KEY:latest" $INIT)"
info "container $CID"
for _ in $(seq 1 30); do
    podman exec "$CID" true 2>/dev/null && break
    sleep 0.5
done
if [[ "$INIT_MODE" == "systemd" ]]; then
    for _ in $(seq 1 60); do
        state="$(podman exec "$CID" systemctl is-system-running 2>/dev/null || true)"
        [[ "$state" == "running" || "$state" == "degraded" ]] && break
        sleep 0.5
    done
    info "systemd state: ${state:-unknown}"
fi

# ---- 3. copy repo ----
podman exec "$CID" mkdir -p /s2udio
tar --exclude=.git --exclude=target --exclude=scripts/dev/artifacts     -C "$REPO_ROOT" -cf - . | podman exec -i "$CID" tar -C /s2udio -xf -

# ---- 4. sudo shim (containers run as root; setup.sh uses sudo) ----
# --no-sudo: omit the shim so the root+no-elevation path of resolve_elevation()
# is exercised for real (ELEVATE_CMD empty -> direct root commands)
if [[ $NO_SUDO_SHIM -eq 0 ]]; then
    # /usr/local/bin is absent on minimal images (ghcr.io/nixos/nix has no
    # /usr/local at all) — create it so the shim write succeeds everywhere
    podman exec "$CID" bash -c 'mkdir -p /usr/local/bin'
    podman exec -i "$CID" bash -c 'cat > /usr/local/bin/sudo' <<'SHIMEOF'
#!/bin/sh
exec "$@"
SHIMEOF
    podman exec "$CID" chmod +x /usr/local/bin/sudo
    ok "sudo shim installed"
else
    ok "sudo shim OMITTED (--no-sudo): exercising root+no-elevation path"
fi

# ---- 4b. alpine: drop the provision-built cava + mpDris2 so setup.sh must
# build cava from source and fetch upstream mpDris2 (the real deltas) ----
if [[ "$KEY" == "alpine-320" ]]; then
    podman exec "$CID" rm -f /usr/local/bin/cava /usr/bin/cava /usr/bin/mpDris2 || true
    ok "alpine: provision-built cava + mpDris2 removed (forcing source build + upstream fetch)"
fi

# ---- 4c. arch: seed the minimal user mpd.conf run_arch expects ----
# run_arch appends the fifo to an EXISTING ~/.config/mpd/mpd.conf and warns
# ("no MPD config at ...") when there is none — a fresh Arch host has no user
# config; same assumption the mock's arch fixture homes make (the no-config
# warn path stays covered by the mock). Music dir seeded so mpd's first start
# has a library root.
if [[ "$KEY" == "arch" ]]; then
    podman exec "$CID" bash -c 'mkdir -p /root/.config/mpd /root/.cache/mpd/playlists /root/Music && printf "%s\n" "music_directory \"/root/Music\"" "bind_to_address \"127.0.0.1\"" "port \"6600\"" "db_file \"/root/.cache/mpd/database\"" "state_file \"/root/.cache/mpd/state\"" "sticker_file \"/root/.cache/mpd/sticker.sql\"" "playlist_directory \"/root/.cache/mpd/playlists\"" "follow_outside_symlinks \"yes\"" "follow_inside_symlinks \"yes\"" "auto_update \"yes\"" > /root/.config/mpd/mpd.conf'
    ok "arch: seeded minimal user mpd.conf (run_arch fifo-append target; mock-matching assumption)"
fi

# ---- 5+6. user session + run setup.sh -y (same exec: the session env must
# reach setup.sh so s2u-svc detects the systemd-user backend; harness pattern
# from test-distro.sh deploy.sh) ----
info "running: start_user_session + bash setup.sh -y"
if podman exec "$CID" bash -lc 'source /s2udio/scripts/dev/lib.sh; start_user_session; export PATH="/root/.local/bin:/root/.cargo/bin:$PATH"; cd /s2udio && bash setup.sh -y'; then
    ok "setup.sh -y finished"
else
    die "setup.sh -y FAILED (see run.log)"
fi

# ---- 7. in-container end-state checks ----
run_check() { # $1 = gate name, $2 = command (runs with the user session up:
    # s2u-svc's systemd-user detection needs XDG_RUNTIME_DIR/DBUS from lib.sh)
    if podman exec "$CID" bash -lc "source /s2udio/scripts/dev/lib.sh; start_user_session >/dev/null 2>&1; $2" >/dev/null 2>&1; then
        write_gate "$1" pass "$2"
    else
        write_gate "$1" fail "$2"
    fi
}
export PATH_SAVE="$PATH"
case "$KEY" in
    nix)  # the flake package lands in the nix profile (plan §6.3), not ~/.local/bin
        run_check S1 'test -x /root/.nix-profile/bin/s2udio && /root/.nix-profile/bin/s2udio version >/dev/null' ;;
    *)
        run_check S1 'test -x /root/.local/bin/s2udio && /root/.local/bin/s2udio version >/dev/null' ;;
esac
# S2..S7: per-key variants — every target shares the common checks except
# arch (plan distro-support-followups.md T6b): run_arch never installed
# s2u-svc (it drives systemctl --user directly) and mpdris2-git + mpv-full
# are AUR-only, so S4 covers the pacman-installed system packages
# (mpd/yt-dlp/cava — mpv EXPECTED absent: mpv-full is AUR-only and the AUR
# branch stays mock/AUR-covered), S5 checks the user mpd.service directly,
# and S6 asserts the mpDris2 drop-in was written for the (deferred) AUR
# package while no mpdris2 unit exists yet.
case "$KEY" in
    arch)
        run_check S2 'test -x /root/.local/bin/rmpc-fetch-lyrics && test -x /root/.local/bin/s2u-mpv-tracker && test -x /root/.local/bin/s2u-mpdris2 && test -f /root/.config/mpv/scripts/mpvSockets.lua'
        run_check S3 'grep -q "mpd-cava.fifo" /root/.config/mpd/mpd.conf'
        run_check S4 'command -v yt-dlp >/dev/null && command -v cava >/dev/null && command -v mpd >/dev/null'
        run_check S5 'systemctl --user is-active mpd.service'
        run_check S6 'test -f /root/.config/systemd/user/mpDris2.service.d/s2udio.conf && grep -q "s2u-mpdris2" /root/.config/systemd/user/mpDris2.service.d/s2udio.conf && ! systemctl --user list-unit-files | grep -qi mpdris2'
        run_check S7 'test -f /root/.config/s2udio/config.ron && test -f /root/.config/s2udio/themes/default.ron'
        ;;
    *)
        run_check S2 'test -x /root/.local/bin/rmpc-fetch-lyrics && test -x /root/.local/bin/s2u-mpv-tracker && test -x /root/.local/bin/s2u-mpdris2 && test -x /root/.local/bin/s2u-svc && test -f /root/.config/mpv/scripts/mpvSockets.lua'
        run_check S3 'grep -q "mpd-cava.fifo" /root/.config/mpd/mpd.conf'
        run_check S4 'command -v mpv >/dev/null && command -v yt-dlp >/dev/null && command -v cava >/dev/null && command -v mpd >/dev/null'
        run_check S5 '/root/.local/bin/s2u-svc is-active mpd'
        run_check S6 '/root/.local/bin/s2u-svc is-active mpDris2'
        run_check S7 'test -f /root/.config/s2udio/config.ron && test -f /root/.config/s2udio/themes/default.ron'
        ;;
esac
case "$KEY" in
    debian-12|ubuntu-2404)
        run_check S8 '! systemctl is-active mpd.service >/dev/null 2>&1'   # system mpd stopped
        run_check S9 'systemctl --user is-active mpd.service >/dev/null 2>&1' ;;
    fedora-41)
        run_check S8 'rpm -q rpmfusion-free-release >/dev/null' ;;
    alpine-320|nix)
        run_check S8 'head -1 /usr/bin/mpDris2 2>/dev/null | grep -q python' ;;  # upstream python source (plan §5/§12.6)
    void-glibc)
        run_check S8 'test -x /root/.config/runit/mpd/run && test -x /root/.config/runit/mpDris2/run' ;;
    arch)
        run_check S9 'test -p /tmp/mpd-cava.fifo'   # running user mpd created the cava fifo
        # S8 (host-side): the run log must show the REAL pacman install with
        # the fixed yt-dlp name (T6a, dc96a4d) + the no-AUR-helper warn
        # paths, and mpv must be absent (mpv-full AUR-only, deferred).
        if grep -q 'installed: mpd ffmpeg cava yt-dlp' "$RUN_LOG" \
           && grep -q 'no AUR helper (yay/paru) found - install manually: mpdris2-git' "$RUN_LOG" \
           && grep -q 'mpv-full not in your distro repos' "$RUN_LOG" \
           && grep -q 'mpDris2.service not found - install mpdris2-git and enable it' "$RUN_LOG" \
           && ! podman exec "$CID" test -x /usr/bin/mpv 2>/dev/null; then
            write_gate S8 pass "run log: pacman install (mpd ffmpeg cava yt-dlp) + no-AUR-helper warns (mpdris2-git, mpv-full); mpv absent (AUR deferred)"
        else
            write_gate S8 fail "run log missing pacman install / no-AUR-helper warn paths, or mpv unexpectedly present (see run.log)"
        fi
        ;;
esac

# ---- 7b/7c. FULL feature gate loop G1..G11 — skipped for targets whose
# gate loop needs packages setup.sh cannot install. arch (T6b): G1 requires
# s2u-svc + mpDris2 + mpv, G4/G5 the mpDris2 MPRIS bridge, G8/G10 mpv —
# all AUR-only on stock Arch (mpdris2-git, mpv-full) with no AUR helper in
# the container; the AUR branch is explicitly deferred to the hermetic mock
# (scripts/dev/test-setup-mock.py). The arch end-state (detection + real
# pacman install + build + scripts + systemd-user services + fifo) is
# asserted by the S1..S9 checks above.
RUN_FEATURE_GATES=1
case "$KEY" in
    arch) RUN_FEATURE_GATES=0 ;;
esac
if [[ $RUN_FEATURE_GATES -eq 1 ]]; then
# ---- 7b. gate-prep: fixtures the G1..G11 gates need, mirroring the harness
# deploy path (deploy-common.sh media/cava glue — NOT its unit/drop-in
# parts, which setup.sh owns). Two deltas vs the deploy path, both because
# THIS container was provisioned by setup.sh:
#   * harness-only test tooling (tmux for G10, procps-ng for pgrep/pkill in
#     G4/G6/G8/G10, ncurses-term for the xterm-256color TERM) is installed
#     here — they are NOT s2udio runtime deps, so setup.sh does not install
#     them (the old harness provision.sh did);
#   * the test media goes into the music_directory setup.sh configured
#     (~/Music by default) and ~/media is a symlink to it (G8 hardcodes
#     /root/media/test.mp4). MPD is NOT restarted — restarting it drops the
#     mpDris2 bridge (official mpDris2 exits on MPD disconnect and systemd
#     sees a clean exit, so Restart=on-failure does not bring it back).
info "gate-prep: harness tooling + test media + cava config + G11 unit"
if podman exec "$CID" bash -lc '
set -euo pipefail
source /s2udio/scripts/dev/lib.sh
start_user_session >/dev/null 2>&1
# harness-only test tooling (not s2udio runtime deps)
if ! command -v tmux >/dev/null 2>&1 || ! command -v pgrep >/dev/null 2>&1; then
    # shellcheck disable=SC1091
    . /etc/os-release
    case "${ID:-} ${ID_LIKE:-}" in
        *fedora*)  dnf5 install -y --nogpgcheck tmux ncurses-term procps-ng >/dev/null ;;
        *debian*|*ubuntu*) apt-get update -qq >/dev/null 2>&1 || true; apt-get install -y --no-install-recommends tmux procps ncurses-term >/dev/null ;;
        *alpine*)  apk add --no-cache tmux procps ncurses-terminfo-base >/dev/null ;;
        *void*)    xbps-install -y tmux procps-ng ncurses-term >/dev/null ;;
        *nixos*)   nix profile install nixpkgs#tmux nixpkgs#procps nixpkgs#ncurses >/dev/null ;;
    esac
fi
# test media inside the music_directory setup.sh configured (G5 adds
# test.mp3 relative to it); ~/media -> that dir for G8s hardcoded path
MUSIC_DIR="$(sed -n "s/^music_directory \"\(.*\)\"/\1/p" "$HOME/.config/mpd/mpd.conf" 2>/dev/null | head -1)"
MUSIC_DIR="${MUSIC_DIR:-$HOME/Music}"
mkdir -p "$MUSIC_DIR" "$HOME/.config/cava" "$HOME/.config/systemd/user"
if [[ ! -f "$MUSIC_DIR/test.mp3" ]]; then
    ffmpeg -hide_banner -loglevel error -y -f lavfi -i "sine=frequency=440:duration=30" \
        -c:a libmp3lame -metadata title="S2U Test Tone" -metadata artist="S2U Harness" \
        "$MUSIC_DIR/test.mp3" || \
    ffmpeg -hide_banner -loglevel error -y -f lavfi -i "sine=frequency=440:duration=30" \
        -c:a mp2 -metadata title="S2U Test Tone" -metadata artist="S2U Harness" \
        "$MUSIC_DIR/test.mp3"
fi
if [[ ! -f "$MUSIC_DIR/test.mp4" ]]; then
    ffmpeg -hide_banner -loglevel error -y \
        -f lavfi -i "testsrc=duration=30:size=320x240:rate=15" \
        -f lavfi -i "sine=frequency=440:duration=30" \
        -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest \
        -metadata title="S2U Test Video" -metadata artist="S2U Harness" \
        "$MUSIC_DIR/test.mp4" || \
    ffmpeg -hide_banner -loglevel error -y \
        -f lavfi -i "testsrc=duration=30:size=320x240:rate=15" \
        -f lavfi -i "sine=frequency=440:duration=30" \
        -c:v mpeg4 -c:a mp2 -shortest \
        -metadata title="S2U Test Video" -metadata artist="S2U Harness" \
        "$MUSIC_DIR/test.mp4"
fi
[[ -e "$HOME/media" ]] || ln -s "$MUSIC_DIR" "$HOME/media"
cat > "$HOME/.config/cava/config" <<EOF
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
# G11 round-trips s2u-svc on a dedicated unit for systemd-user targets
# (setup.sh does not create it; deploy-common.sh does)
cat > "$HOME/.config/systemd/user/s2u-svc-g11.service" <<EOF
[Unit]
Description=s2u-svc round-trip test unit
[Service]
ExecStart=/bin/sleep 300
[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload >/dev/null 2>&1 || true
# NO mpd restart (see above): trigger a database scan so G5 add test.mp3
# resolves, then confirm the cava fifo is still there
python3 - <<PYEOF
import socket, time
for _ in range(30):
    try:
        socket.create_connection(("127.0.0.1", 6600), timeout=2).close()
        break
    except OSError:
        time.sleep(1)
else:
    raise SystemExit("mpd not reachable")
s = socket.create_connection(("127.0.0.1", 6600), timeout=5)
f = s.makefile("rwb")
while True:
    line = f.readline()
    if not line or line.startswith(b"OK"):
        break
f.write(b"update\n"); f.flush()
s.close()
time.sleep(3)
PYEOF
for _ in $(seq 1 30); do [[ -p /tmp/mpd-cava.fifo ]] && break; sleep 0.5; done
ls -la "$MUSIC_DIR" "$HOME/media"
'; then
    ok "gate-prep done (tooling + media + cava config + G11 unit)"
else
    die "gate-prep FAILED (see run.log)"
fi

# ---- 7c. FULL feature gate loop G1..G11 in the setup.sh-provisioned
# container (run-gates.sh writes per-gate JSON + gates.jsonl into
# /s2udio/artifacts, collected at step 9) ----
GATE_ARGS=()
if [[ ${#GATE_FILTER[@]} -gt 0 ]]; then
    for g in "${GATE_FILTER[@]}"; do GATE_ARGS+=(--gate "$g"); done
fi
info "feature gates: ${GATE_FILTER[*]:-G1..G11} via run-gates.sh"
podman exec "$CID" bash /s2udio/scripts/dev/gates/run-gates.sh "$KEY" "${GATE_ARGS[@]}"
else
    info "feature gates skipped for $KEY (AUR-only deps deferred to the mock — plan distro-support-followups.md T6b)"
fi

# ---- 8. debug dump (service states + journal; helps diagnose S5/S6) ----
podman exec "$CID" bash -lc 'source /s2udio/scripts/dev/lib.sh; start_user_session >/dev/null 2>&1;     echo "== systemctl --user status mpd mpDris2 =="; systemctl --user status mpd.service --no-pager -l 2>&1 | head -12;     echo; systemctl --user status mpDris2.service --no-pager -l 2>&1 | head -12;     echo "== journal (mpDris2) =="; journalctl --user -u mpDris2.service --no-pager -n 15 2>&1 | tail -15;     echo "== journal (mpd) =="; journalctl --user -u mpd.service --no-pager -n 6 2>&1 | tail -6' > "$ART_DIR/service-debug.txt" 2>&1 || true

# ---- 9. artifacts + teardown ----
cp "$ART_DIR/gates.jsonl" "$ART_DIR/gates-driver.jsonl" 2>/dev/null || true   # G0 + S1..S9 pre-collection snapshot
podman cp "$CID:/s2udio/artifacts/." "$ART_DIR/" >/dev/null 2>&1 || true     # in-container G1..G11 (run-gates.sh)
if [[ -s "$ART_DIR/gates-driver.jsonl" && -s "$ART_DIR/gates.jsonl" ]]; then
    # the in-container gates.jsonl (G1..G11) overwrote the driver file during
    # collection — merge the driver-side lines (G0 + S1..S9) back in
    awk -F'\t' 'NR==FNR{have[$1]=1; next} !($1 in have){print}' \
        "$ART_DIR/gates.jsonl" "$ART_DIR/gates-driver.jsonl" >> "$ART_DIR/gates.jsonl"
fi
rm -f "$ART_DIR/gates-driver.jsonl"
info "teardown"
podman rm -f "$CID" >/dev/null 2>&1 || true
CID=""
info "summary:"
if [[ -f "$ART_DIR/gates.jsonl" ]]; then
    sort -V -k1,1 "$ART_DIR/gates.jsonl" | awk -F'\t' '{printf "  %-4s %-5s %s\n", $1, $2, $4}'
fi
info "setup-$KEY validation complete — artifacts in $ART_DIR (G12 asserted by the EXIT trap)"

#!/usr/bin/env bash
# test-setup-distro.sh <matrix-key> [--no-cache-vol] [--artifacts DIR]
#
# Real in-container validation of the NEW setup.sh distro dispatcher (plan
# docs/design/Validation/distro-support.md §6.2): run `setup.sh -y` inside the
# ephemeral harness container for the target and assert the end state
# (binary, support scripts, services via s2u-svc, mpd.conf fifo, per-distro
# deltas). Same ephemerality discipline as test-distro.sh (--rm, EXIT trap,
# start-of-run sweep, end-of-run G12 assertion).
#
# Usage: scripts/dev/test-setup-distro.sh <key> [--no-sudo] [--no-cache-vol] [--artifacts DIR]
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
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-cache-vol) NO_CACHE_VOL=1; shift ;;
        --no-sudo) NO_SUDO_SHIM=1; shift ;;
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
run_check S1 'test -x /root/.local/bin/s2udio && /root/.local/bin/s2udio version >/dev/null'
run_check S2 'test -x /root/.local/bin/rmpc-fetch-lyrics && test -x /root/.local/bin/s2u-mpv-tracker && test -x /root/.local/bin/s2u-mpdris2 && test -x /root/.local/bin/s2u-svc && test -f /root/.config/mpv/scripts/mpvSockets.lua'
run_check S3 'grep -q "mpd-cava.fifo" /root/.config/mpd/mpd.conf'
run_check S4 'command -v mpv >/dev/null && command -v yt-dlp >/dev/null && command -v cava >/dev/null && command -v mpd >/dev/null'
run_check S5 '/root/.local/bin/s2u-svc is-active mpd'
run_check S6 '/root/.local/bin/s2u-svc is-active mpDris2'
run_check S7 'test -f /root/.config/s2udio/config.ron && test -f /root/.config/s2udio/themes/default.ron'
case "$KEY" in
    debian-12|ubuntu-2404)
        run_check S8 '! systemctl is-active mpd.service >/dev/null 2>&1'   # system mpd stopped
        run_check S9 'systemctl --user is-active mpd.service >/dev/null 2>&1' ;;
    fedora-41)
        run_check S8 'rpm -q rpmfusion-free-release >/dev/null' ;;
    alpine-320)
        run_check S8 'head -1 /usr/bin/mpDris2 2>/dev/null | grep -q python' ;;  # upstream python source
    void-glibc)
        run_check S8 'test -x /root/.config/runit/mpd/run && test -x /root/.config/runit/mpDris2/run' ;;
esac

# ---- 8. debug dump (service states + journal; helps diagnose S5/S6) ----
podman exec "$CID" bash -lc 'source /s2udio/scripts/dev/lib.sh; start_user_session >/dev/null 2>&1;     echo "== systemctl --user status mpd mpDris2 =="; systemctl --user status mpd.service --no-pager -l 2>&1 | head -12;     echo; systemctl --user status mpDris2.service --no-pager -l 2>&1 | head -12;     echo "== journal (mpDris2) =="; journalctl --user -u mpDris2.service --no-pager -n 15 2>&1 | tail -15;     echo "== journal (mpd) =="; journalctl --user -u mpd.service --no-pager -n 6 2>&1 | tail -6' > "$ART_DIR/service-debug.txt" 2>&1 || true

# ---- 9. artifacts + teardown ----
podman cp "$CID:/s2udio/artifacts/." "$ART_DIR/" >/dev/null 2>&1 || true
info "teardown"
podman rm -f "$CID" >/dev/null 2>&1 || true
CID=""
info "summary:"
if [[ -f "$ART_DIR/gates.jsonl" ]]; then
    awk -F'\t' '{printf "  %-4s %-5s %s\n", $1, $2, $4}' "$ART_DIR/gates.jsonl"
fi
info "setup-$KEY validation complete — artifacts in $ART_DIR (G12 asserted by the EXIT trap)"

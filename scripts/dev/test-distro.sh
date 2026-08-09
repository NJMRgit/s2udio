#!/usr/bin/env bash
# test-distro.sh <matrix-key> [--gate G0..G12] [--no-cache-vol] [--artifacts DIR]
#
# s2udio distro-support test harness driver (plan docs/design/Validation/
# distro-support.md §4.1). Runs one target through the full lifecycle in an
# EPHEMERAL rootless podman container:
#
#   sweep -> create (--rm, --systemd=always) -> copy repo (no .git/target)
#   -> provision (packages + rust toolchain) -> build + unit tests
#   -> deploy (scripts/config/services) -> gates G1..G11 (in-container)
#   -> collect artifacts -> teardown -> gate G12 (ephemerality assertion)
#
# Ephemerality is enforced 4 ways (§4.1): --rm, an EXIT trap that removes the
# container, a start-of-run sweep of stale s2u-distro-* containers, and the
# end-of-run G12 assertion (the run FAILS if any s2u-distro-* remains).
#
# Host-side artifacts land in scripts/dev/artifacts/<key>/<timestamp>/:
# run.log, the driver's G0/G12 JSON, and the in-container per-gate JSONs +
# gates.jsonl (podman cp'd out before teardown).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DEV_DIR="$HERE"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
source "$HERE/lib.sh"

KEY="${1:-}"
[[ -n "$KEY" ]] || die "usage: test-distro.sh <matrix-key> [--gate G0..G12] [--no-cache-vol] [--artifacts DIR]"
shift

GATE_FILTER=()
NO_CACHE_VOL=0
ART_DIR_OVERRIDE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --gate)  GATE_FILTER+=("$2"); shift 2 ;;
        --no-cache-vol) NO_CACHE_VOL=1; shift ;;
        --artifacts) ART_DIR_OVERRIDE="$2"; shift 2 ;;
        *) die "unknown argument: $1" ;;
    esac
done

TARGET_DIR="$DEV_DIR/containers/$KEY"
[[ -d "$TARGET_DIR" ]] || die "no target definition at $TARGET_DIR (matrix key $KEY)"
[[ -f "$TARGET_DIR/image.env" ]] || die "missing $TARGET_DIR/image.env (IMAGE=..., INIT=..., INIT_MODE=...)"

ART_DIR="${ART_DIR_OVERRIDE:-$DEV_DIR/artifacts/$KEY/$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$ART_DIR"
RUN_LOG="$ART_DIR/run.log"
# tee the whole run into the artifacts dir (keeps one stream per run)
exec > >(tee -a "$RUN_LOG") 2>&1

export ART_DIR   # for write_gate (host-side G0/G12)
CID=""

cleanup() {
    local rc=$?
    if [[ -n "$CID" ]]; then
        # best-effort artifact salvage on failure (container may still exist)
        podman cp "$CID:/s2udio/artifacts/." "$ART_DIR/" >/dev/null 2>&1 || true
        podman rm -f "$CID" >/dev/null 2>&1 || true
        CID=""
    fi
    # the in-container gates.jsonl (G1..G11) overwrote the host file during
    # collection — re-append the host-side G0 line so the summary is complete
    if ! grep -q $'\tG0\t' "$ART_DIR/gates.jsonl" 2>/dev/null; then
        printf 'G0\tpass\t%s\tstart-of-run sweep ran (host-side gate, recorded post-collection)\n' \
            "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$ART_DIR/gates.jsonl"
    fi
    # ---- G12 ephemerality assertion (runs on EVERY exit, success or failure)
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

# ---------------------------------------------------------------------------
info "s2udio distro harness — target: $KEY"
info "artifacts: $ART_DIR"
source "$TARGET_DIR/image.env"   # IMAGE, INIT, INIT_MODE (=systemd|plain)

# ---- 1. sweep: remove any stale leftovers from a crashed run --------------
info "sweep: removing stale s2u-distro-* containers"
STALE="$(podman ps -aq --filter name=s2u-distro- 2>/dev/null || true)"
SWEEP_COUNT=0
if [[ -n "${STALE// }" ]]; then
    SWEEP_COUNT="$(printf '%s\n' "$STALE" | grep -c . || true)"
    podman rm -f $STALE >/dev/null 2>&1 || true
    warn "sweep removed $SWEEP_COUNT stale container(s): $(echo $STALE)"
else
    ok "no stale containers"
fi
if [[ "${GATE_FILTER[*]:-}" == "" || " ${GATE_FILTER[*]} " == *" G0 "* ]]; then
    write_gate G0 pass "start-of-run sweep ran (removed $SWEEP_COUNT stale); container will be created with --rm + EXIT trap"
fi

# ---- 2. create the container ----------------------------------------------
info "image: $IMAGE"
podman build -q -t "localhost/s2u-distro-$KEY:latest" "$TARGET_DIR" >/dev/null
VOL_ARGS=()
if [[ $NO_CACHE_VOL -eq 0 ]]; then
    podman volume create "s2u-cargo-$KEY" >/dev/null 2>&1 || true
    VOL_ARGS=(-v "s2u-cargo-$KEY:/root/.cargo")
fi
SYSTEMD_ARGS=()
[[ "$INIT_MODE" == "systemd" ]] && SYSTEMD_ARGS=(--systemd=always)
info "creating container s2u-distro-$KEY (${INIT_MODE})"
# --shm-size=512m: Void's xbps needs more than the default 63M /dev/shm
# ("Failed to initialize libxbps: No buffer space available")
CID="$(podman run --rm -d --name "s2u-distro-$KEY" --shm-size=512m "${SYSTEMD_ARGS[@]}" "${VOL_ARGS[@]}" \
        "localhost/s2u-distro-$KEY:latest" $INIT)"
info "container $CID"

# wait for the container to be responsive (and systemd up for systemd targets)
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

# ---- 3. copy the repo in (no .git, no target/, no artifacts) --------------
info "copying repo into the container (excluding .git, target/, artifacts)"
podman exec "$CID" mkdir -p /s2udio
tar --exclude=.git --exclude=target --exclude=scripts/dev/artifacts \
    -C "$REPO_ROOT" -cf - . | podman exec -i "$CID" tar -C /s2udio -xf -
podman exec "$CID" mkdir -p /s2udio/artifacts

# ---- 4. provision (packages + rust toolchain) ------------------------------
info "provision: packages + toolchain"
podman exec "$CID" bash /s2udio/scripts/dev/containers/$KEY/provision.sh
ok "provision done"

# ---- 5. build + unit tests -------------------------------------------------
info "build: cargo build --release"
if ! podman exec "$CID" bash -lc 'cd /s2udio && export PATH="$HOME/.cargo/bin:$PATH" && cargo build --release 2>&1 | tail -6'; then
    die "cargo build --release failed (see run.log)"
fi
podman exec "$CID" test -x /s2udio/target/release/s2u || die "cargo build claimed success but target/release/s2u is missing"
ok "build done"

info "unit tests: cargo test --release"
if ! podman exec "$CID" bash -lc 'cd /s2udio && export PATH="$HOME/.cargo/bin:$PATH" && cargo test --release 2>&1 | tail -8'; then
    die "cargo test --release failed (see run.log)"
fi
ok "tests done"

# ---- 6. deploy (scripts, config/theme, services through s2u-svc) ----------
info "deploy"
podman exec "$CID" bash /s2udio/scripts/dev/containers/$KEY/deploy.sh
ok "deploy done"

# ---- 7. gates G1..G11 (in-container) ---------------------------------------
GATE_ARGS=()
if [[ ${#GATE_FILTER[@]} -gt 0 ]]; then
    for g in "${GATE_FILTER[@]}"; do GATE_ARGS+=(--gate "$g"); done
fi
info "gates: ${GATE_FILTER[*]:-G1..G11}"
podman exec "$CID" bash /s2udio/scripts/dev/gates/run-gates.sh "$KEY" "${GATE_ARGS[@]}"

# ---- 8. collect artifacts ---------------------------------------------------
info "collecting artifacts"
podman cp "$CID:/s2udio/artifacts/." "$ART_DIR/"
ok "artifacts -> $ART_DIR"

# ---- 9. teardown + G12 (via the EXIT trap; teardown now, G12 in trap) -------
info "teardown"
podman rm -f "$CID" >/dev/null 2>&1 || true
CID=""
info "summary:"
if [[ -f "$ART_DIR/gates.jsonl" ]]; then
    awk -F'\t' '{printf "  %-4s %-5s %s\n", $1, $2, $4}' "$ART_DIR/gates.jsonl"
fi
info "run complete — artifacts in $ART_DIR (G12 asserted by the EXIT trap)"

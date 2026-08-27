#!/usr/bin/env bash
# run-gates.sh <matrix-key> [--gate G1..G11] — in-container gate runner.
# Executes gates G1..G11 (G0/G12 are host-side driver gates) and writes one
# JSON file per gate + a gates.jsonl line into /s2udio/artifacts (collected
# by the driver before teardown). Plan §7.
set -uo pipefail
source /s2udio/scripts/dev/lib.sh

KEY="${1:-unknown}"; shift
FILTER=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --gate) FILTER+=("$2"); shift 2 ;;
        *) echo "run-gates.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done

export GATE_ART_DIR=/s2udio/artifacts
mkdir -p "$GATE_ART_DIR"
source /s2udio/scripts/dev/gates/common.sh

run_gate() {  # $1=gate name, rest = gate function
    local name="$1"; shift
    if [[ ${#FILTER[@]} -gt 0 ]]; then
        local wanted=0 f
        for f in "${FILTER[@]}"; do [[ "$f" == "$name" ]] && wanted=1; done
        [[ $wanted -eq 1 ]] || return 0
    fi
    # each gate function writes its own pass/fail/soft JSON
    "$@"
}

# session plumbing must be up for the service/MPRIS gates
start_user_session
ensure_local_bin_path
export PATH="$HOME/.local/bin:$PATH"

info "gate runner: target=$KEY"
run_gate G1  gate_g1_install
run_gate G2  gate_g2_build_version
run_gate G3  gate_g3_unit_tests
run_gate G4  gate_g4_mpd_up
run_gate G5  gate_g5_mpd_mpris
run_gate G8  gate_g8_mpv_headless
run_gate G6  gate_g6_mpv_mpris
run_gate G7  gate_g7_ytdlp_soft
run_gate G9  gate_g9_cava
run_gate G10 gate_g10_tui_smoke
run_gate G11 gate_g11_s2u_svc

# stop the mpv/tracker session left by G8/G6, leave a clean container
if [[ -f "$HOME/.cache/s2udio/mpv-tracker.pid" ]]; then
    kill "$(cat "$HOME/.cache/s2udio/mpv-tracker.pid")" 2>/dev/null || true
    pkill -f 'mpv --vo=null' 2>/dev/null || true
    pkill -x s2udio-mpris 2>/dev/null || true
fi
tmux kill-server >/dev/null 2>&1 || true

info "gates complete: $(grep -c $'\tpass\t' "$GATE_ART_DIR/gates.jsonl" 2>/dev/null || echo 0)/$(grep -c . "$GATE_ART_DIR/gates.jsonl" 2>/dev/null || echo 0) passed"

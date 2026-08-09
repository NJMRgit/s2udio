#!/usr/bin/env bash
# lib.sh — shared helpers for the s2udio distro-support test harness.
# Sourced by scripts/dev/test-distro.sh (host side) and by the in-container
# provision/deploy/gate scripts (they find this file inside the copied repo).
set -uo pipefail

info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m  !! %s\033[0m\n' "$*"; }
die()   { printf '\033[1;31mFATAL:\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Gate result recording. Each gate writes a JSON file and appends a line to
# gates.jsonl in $ART_DIR (host driver) or /s2udio/artifacts (in-container).
# status: pass | fail | soft (soft = recorded, does not fail the run)

escape_json() {  # $1 = raw string -> stdout (JSON-escaped)
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/\\t/g' \
        | tr '\n' ' ' | sed -e 's/  */ /g'
}

write_gate() {  # $1=gate $2=status $3=detail (rest joined)
    local gate="$1" status="$2"; shift 2
    local detail="$*"
    local ts; ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    local dir="${GATE_ART_DIR:-${ART_DIR:-/s2udio/artifacts}}"
    mkdir -p "$dir"
    local esc; esc="$(escape_json "$detail")"
    printf '{"gate":"%s","status":"%s","detail":"%s","ts":"%s"}\n' \
        "$gate" "$status" "$esc" "$ts" > "$dir/gate-$gate.json"
    printf '%s\t%s\t%s\t%s\n' "$gate" "$status" "$ts" "$detail" >> "$dir/gates.jsonl"
    case "$status" in
        pass) ok "gate $gate: $detail";;
        soft) warn "gate $gate (soft): $detail";;
        fail) printf '\033[1;31mFAIL\033[0m gate %s: %s\n' "$gate" "$detail";;
    esac
}

# ---------------------------------------------------------------------------
# In-container session plumbing (sourced by deploy.sh and the gate runner):
# a real D-Bus session bus + an explicit `systemd --user` manager (there is
# no logind in containers, so we start the user manager ourselves; works
# when XDG_RUNTIME_DIR + DBUS_SESSION_BUS_ADDRESS are set — plan §4.3).
# Idempotent: safe to call from any stage.

start_user_session() {
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/1000}"
    mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
    if [[ ! -S "$XDG_RUNTIME_DIR/bus" ]]; then
        dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --fork >/dev/null 2>&1 || true
        sleep 1
    fi
    if ! systemctl --user is-system-running >/dev/null 2>&1; then
        # systemd --user (the binary is not on PATH on every distro)
        local sd_user
        for sd_user in /usr/lib/systemd/systemd /lib/systemd/systemd; do
            if [[ -x "$sd_user" ]]; then break; fi
        done
        nohup "$sd_user" --user >/dev/null 2>&1 &
        for _ in $(seq 1 30); do
            systemctl --user is-system-running >/dev/null 2>&1 && break
            sleep 0.5
        done
    fi
    systemctl --user is-system-running >/dev/null 2>&1 \
        || warn "systemd --user did not reach 'running' (continuing anyway)"
}

# Keep ~/.local/bin on PATH (the tracker spawns `s2udio-mpris` from PATH).
ensure_local_bin_path() {
    case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) export PATH="$HOME/.local/bin:$PATH" ;;
    esac
}

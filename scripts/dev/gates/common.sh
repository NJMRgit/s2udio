#!/usr/bin/env bash
# gates/common.sh — gate implementations G1..G11 (run INSIDE the container).
# Each gate_* function writes its own pass/fail/soft JSON via write_gate and
# returns 0 on pass. Plan docs/design/Validation/distro-support.md §7.
set -uo pipefail

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/1000}"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
HOME_DIR="$HOME"
SVC_BIN="$(command -v s2u-svc 2>/dev/null || echo "$SVC_BIN")"

# ---------------------------------------------------------------- helpers --
wait_for() {  # $1 = seconds, rest = command; returns 0 when it succeeds
    local n="$1"; shift
    for _ in $(seq 1 "$n"); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 1
    done
    return 1
}

mpd_raw() {  # $1 = MPD command; prints response body (greeting consumed)
    python3 - "$1" <<'PYEOF'
import socket, sys
cmd = sys.argv[1]
s = socket.create_connection(("127.0.0.1", 6600), timeout=5)
f = s.makefile("rwb")
# consume the connection greeting ("OK MPD <version>")
while True:
    line = f.readline()
    if not line:
        break
    if line.startswith(b"OK"):
        break
f.write((cmd + "\n").encode()); f.flush()
lines = []
while True:
    line = f.readline()
    if not line:
        break
    text = line.decode(errors="replace").rstrip("\n")
    if text.startswith("OK") or text.startswith("ACK"):
        break
    lines.append(text)
s.close()
print("\n".join(lines))
PYEOF
}

# MPRIS probes via python-dbus (python3-dbus/py3-dbus is installed on every
# target; busctl is a systemd tool and does not exist on Alpine/Void).
bus_name_up() {  # $1 = bus name
    python3 - "$1" <<'PYEOF'
import dbus, sys
try:
    bus = dbus.SessionBus()
    sys.exit(0 if sys.argv[1] in bus.list_names() else 1)
except Exception:
    sys.exit(1)
PYEOF
}

dbus_prop() {  # $1 = bus name, $2 = interface, $3 = property -> value on stdout
    python3 - "$1" "$2" "$3" <<'PYEOF'
import dbus, sys
try:
    bus = dbus.SessionBus()
    obj = bus.get_object(sys.argv[1], "/org/mpris/MediaPlayer2")
    iface = dbus.Interface(obj, "org.freedesktop.DBus.Properties")
    print(iface.Get(sys.argv[2], sys.argv[3]))
except Exception:
    sys.exit(1)
PYEOF
}

dbus_call() {  # $1 = bus name, $2 = interface, $3 = method, $4 = signature, $5 = value
    python3 - "$1" "$2" "$3" "$4" "$5" <<'PYEOF'
import dbus, sys
try:
    bus = dbus.SessionBus()
    obj = bus.get_object(sys.argv[1], "/org/mpris/MediaPlayer2")
    iface = dbus.Interface(obj, sys.argv[2])
    args = [int(sys.argv[5])] if sys.argv[4] == "x" else [sys.argv[5]]
    getattr(iface, sys.argv[3])(*args)
except Exception:
    sys.exit(1)
PYEOF
}

mpv_state() {  # $1 = json key
    python3 - "$1" <<'PYEOF'
import json, os, sys
p = os.path.expanduser("~/.cache/s2udio/mpv-mpris.json")
try:
    st = json.load(open(p))
    print(st.get(sys.argv[1], ""))
except Exception:
    pass
PYEOF
}

# ---------------------------------------------------------------- G1 -------
gate_g1_install() {
    local missing=()
    for b in mpd mpv ffmpeg ffprobe yt-dlp cava mpDris2 python3 cargo rustc; do
        command -v "$b" >/dev/null 2>&1 || missing+=("$b")
    done
    python3 -c "import dbus" 2>/dev/null || missing+=(python-dbus)
    python3 -c "import gi" 2>/dev/null || missing+=(python-gi)
    for s in s2u-mpv-tracker s2udio-mpris s2u-mpdris2 rmpc-fetch-lyrics s2u-svc; do
        command -v "$s" >/dev/null 2>&1 || missing+=("$s")
    done
    # The mpv IPC socket contract: s2udio launches mpv with the fixed
    # /tmp/mpvsocket (the socket SVP4's manager also uses); an mpv.conf
    # line is what makes a manually launched mpv expose it too.
    grep -q 'input-ipc-server=/tmp/mpvsocket' "$HOME_DIR/.config/mpv/mpv.conf" 2>/dev/null \
        || missing+=(mpv.conf:input-ipc-server=/tmp/mpvsocket)
    [[ -f "$HOME_DIR/.config/s2udio/config.ron" ]]  || missing+=(config.ron)
    [[ -f "$HOME_DIR/.config/s2udio/themes/default.ron" ]] || missing+=(theme)
    if [[ "$("$SVC_BIN" backend 2>/dev/null)" == "systemd-user" ]]; then
        # user-level mpd.service: setup.sh writes ~/.config/systemd/user/mpd.service
        # only when the distro ships none — a packaged user unit (Fedora mpd
        # ships /usr/lib/systemd/user/mpd.service) satisfies the same contract
        if [[ ! -f "$HOME_DIR/.config/systemd/user/mpd.service" ]] \
           && ! systemctl --user list-unit-files 2>/dev/null | grep -q '^mpd.service'; then
            missing+=(user-mpd-unit)
        fi
        [[ -f "$HOME_DIR/.config/systemd/user/mpDris2.service.d/s2udio.conf" ]] || missing+=(mpdris2-dropin)
    fi
    grep -q "mpd-cava.fifo" "$HOME_DIR/.config/mpd/mpd.conf" || missing+=(fifo-in-mpd.conf)
    if ((${#missing[@]})); then
        write_gate G1 fail "install incomplete — missing: ${missing[*]}"
        return 1
    fi
    write_gate G1 pass "packages+scripts+config+services installed (mpv plain, mpDris2 via s2u-mpdris2 shim)"
}

# ---------------------------------------------------------------- G2 -------
gate_g2_build_version() {
    local bin=""
    if [[ -x /s2udio/target/release/s2u ]]; then
        bin=/s2udio/target/release/s2u
    else
        bin="$(command -v s2udio 2>/dev/null || true)"
    fi
    [[ -n "$bin" ]] || { write_gate G2 fail "no s2udio binary found (target/release/s2u or PATH)"; return 1; }
    local ver
    ver="$("$bin" version 2>&1 | head -1)"
    [[ -n "$ver" ]] || { write_gate G2 fail "s2udio version produced no output"; return 1; }
    if [[ "$bin" == /s2udio/target/release/s2u ]]; then
        write_gate G2 pass "cargo build --release -> s2udio version: $ver"
    else
        write_gate G2 pass "installed binary ($bin) -> s2udio version: $ver"
    fi
}

# ---------------------------------------------------------------- G3 -------
gate_g3_unit_tests() {
    local out
    out="$(cd /s2udio && PATH="$HOME/.cargo/bin:$PATH" cargo test --release 2>&1 | tail -6)"
    local passed failed
    passed="$(printf '%s\n' "$out" | sed -n 's/.*test result: ok\. \([0-9]*\) passed.*/\1/p' | head -1)"
    failed="$(printf '%s\n' "$out" | sed -n 's/.*\([0-9]*\) failed.*/\1/p' | head -1)"
    if [[ -n "$passed" && "${failed:-0}" == "0" ]]; then
        write_gate G3 pass "cargo test --release: $passed passed, 0 failed"
    else
        write_gate G3 fail "test summary unexpected: $(echo "$out" | tr '\n' ' ')"
        return 1
    fi
}

# ---------------------------------------------------------------- G4 -------
gate_g4_mpd_up() {
    local state
    state="$("$SVC_BIN" is-active mpd 2>/dev/null || true)"
    [[ "$state" == "active" ]] || { write_gate G4 fail "mpd state=$state via s2u-svc (want active)"; return 1; }
    local proto
    proto="$(mpd_raw status 2>/dev/null)"
    case "$proto" in
        *"state:"*) ;;
        *) write_gate G4 fail "MPD status did not answer (got: $(echo "$proto" | head -1))"; return 1 ;;
    esac
    [[ -p /tmp/mpd-cava.fifo ]] || { write_gate G4 fail "MPD fifo /tmp/mpd-cava.fifo missing"; return 1; }
    if pgrep -f s2u-mpdris2 >/dev/null 2>&1; then
        write_gate G4 pass "mpd.service active, protocol OK ($(echo "$proto" | head -1)), fifo present, shim process up"
    else
        write_gate G4 pass "mpd.service active, protocol OK, fifo present (shim process NOT found — see G5)"
    fi
}

# ---------------------------------------------------------------- G5 -------
gate_g5_mpd_mpris() {
    # play the local test tone through MPD, then assert the MPRIS surface
    mpd_raw "clear" >/dev/null
    mpd_raw "add test.mp3" >/dev/null
    mpd_raw "play" >/dev/null
    sleep 1
    wait_for 15 bus_name_up org.mpris.MediaPlayer2.mpd \
        || { write_gate G5 fail "org.mpris.MediaPlayer2.mpd never appeared on the session bus"; return 1; }
    # mpDris2 picks the song up on its own poll cadence — poll Metadata
    local meta=""
    for _ in $(seq 1 12); do
        meta="$(dbus_prop org.mpris.MediaPlayer2.mpd org.mpris.MediaPlayer2.Player Metadata 2>&1)"
        grep -q 'S2U Test Tone' <<<"$meta" && break
        sleep 1
    done
    if ! grep -q 'xesam:title' <<<"$meta" || ! grep -q 'S2U Test Tone' <<<"$meta"; then
        write_gate G5 fail "Metadata lacks title: $(echo "$meta" | tr '\n' ' ' | head -c 300)"
        return 1
    fi
    local p1 p2
    p1="$(dbus_prop org.mpris.MediaPlayer2.mpd org.mpris.MediaPlayer2.Player Position 2>/dev/null)"
    sleep 3
    p2="$(dbus_prop org.mpris.MediaPlayer2.mpd org.mpris.MediaPlayer2.Player Position 2>/dev/null)"
    if [[ -z "$p1" || -z "$p2" ]] || ! (( p2 > p1 + 500000 )); then
        write_gate G5 soft "Metadata OK but Position did not advance (p1=$p1 p2=$p2)"
        return 0
    fi
    write_gate G5 pass "MPD MPRIS: Metadata(title=S2U Test Tone) + Position advancing ($p1 -> $p2 us)"
}

# ---------------------------------------------------------------- G8 -------
gate_g8_mpv_headless() {
    pkill -f 'mpv --vo=null' 2>/dev/null || true
    # stop MPD audio first: the tracker's mpv<->MPD mutual exclusion pauses a
    # playing video while MPD plays (HANDOFF) — the video gates need mpv free
    mpd_raw "stop" >/dev/null 2>&1 || true
    rm -rf /tmp/mpvsocket /tmp/mpvSockets "$HOME/.cache/s2udio/mpv-mpris.json"
    # s2udio launches mpv with --input-ipc-server=/tmp/mpvsocket (the
    # socket SVP4's manager connects to and s2udio tracks over).
    setsid mpv --vo=null --ao=null --really-quiet --input-ipc-server=/tmp/mpvsocket \
        /root/media/test.mp4 >/tmp/mpv-g8.log 2>&1 &
    local sock=""
    for _ in $(seq 1 20); do
        [[ -S /tmp/mpvsocket ]] && { sock=/tmp/mpvsocket; break; }
        sleep 0.5
    done
    [[ -n "$sock" ]] || { write_gate G8 fail "mpv IPC socket /tmp/mpvsocket never appeared ($(cat /tmp/mpv-g8.log 2>/dev/null | head -3 | tr '\n' ' '))"; return 1; }
    # tracker caretaker writes the MPRIS state file (and spawns s2udio-mpris)
    local tracker_bin; tracker_bin="$(command -v s2u-mpv-tracker 2>/dev/null || echo "$HOME_DIR/.local/bin/s2u-mpv-tracker")"
    S2U_FORCE_CARETAKER=1 S2U_CACHE_DIR="$HOME/.cache/s2udio" \
        setsid "$tracker_bin" >/tmp/tracker-g8.log 2>&1 &
    local state=""
    for _ in $(seq 1 20); do
        state="$(mpv_state socket)"
        [[ -n "$state" ]] && break
        sleep 0.5
    done
    if [[ -z "$state" ]]; then
        write_gate G8 fail "mpv-mpris.json state file never written (tracker log: $(head -3 /tmp/tracker-g8.log 2>/dev/null | tr '\n' ' '))"
        return 1
    fi
    local title
    title="$(mpv_state title)"
    write_gate G8 pass "mpv headless playing; socket=$sock; state file written (title='$title')"
}

# ---------------------------------------------------------------- G6 -------
gate_g6_mpv_mpris() {
    wait_for 15 bus_name_up org.mpris.MediaPlayer2.s2udio \
        || { write_gate G6 fail "org.mpris.MediaPlayer2.s2udio never appeared (tracker spawned s2udio-mpris?)"; return 1; }
    local meta
    meta="$(dbus_prop org.mpris.MediaPlayer2.s2udio org.mpris.MediaPlayer2.Player Metadata 2>&1)"
    if ! grep -q 'xesam:title' <<<"$meta" || ! grep -q 'S2U Test Video' <<<"$meta"; then
        write_gate G6 fail "Metadata lacks video title: $(echo "$meta" | tr '\n' ' ' | head -c 300)"
        return 1
    fi
    local p1 p2
    p1="$(dbus_prop org.mpris.MediaPlayer2.s2udio org.mpris.MediaPlayer2.Player Position 2>/dev/null)"
    sleep 3
    p2="$(dbus_prop org.mpris.MediaPlayer2.s2udio org.mpris.MediaPlayer2.Player Position 2>/dev/null)"
    # Seek +10 s must route to the mpv socket and move the position
    dbus_call org.mpris.MediaPlayer2.s2udio org.mpris.MediaPlayer2.Player Seek x 10000000 >/dev/null 2>&1
    sleep 2
    local p3
    p3="$(dbus_prop org.mpris.MediaPlayer2.s2udio org.mpris.MediaPlayer2.Player Position 2>/dev/null)"
    local ok_advance=0 ok_seek=0
    [[ -n "$p1" && -n "$p2" && -n "$p3" ]] || { write_gate G6 fail "Position unreadable (p1=$p1 p2=$p2 p3=$p3)"; return 1; }
    (( p2 > p1 + 500000 )) && ok_advance=1
    (( p3 > p1 + 5000000 )) && ok_seek=1
    if [[ $ok_advance -eq 1 && $ok_seek -eq 1 ]]; then
        write_gate G6 pass "s2udio MPRIS: title OK, Position advancing ($p1->$p2), Seek +10s routed to mpv ($p1->$p3)"
    elif [[ $ok_advance -eq 1 ]]; then
        write_gate G6 soft "s2udio MPRIS: title OK, Position advancing, but Seek did not move position (p1=$p1 p3=$p3)"
    else
        write_gate G6 fail "Position did not advance (p1=$p1 p2=$p2 p3=$p3)"
        return 1
    fi
}

# ---------------------------------------------------------------- G7 -------
gate_g7_ytdlp_soft() {
    # android_vr is the anonymous client s2udio prefers (HANDOFF; DASH
    # single-file audio) — the default web client is blocked from the
    # container egress ("not available on this app").
    local id=""
    for attempt in 1 2; do
        id="$(timeout 60 yt-dlp --get-id --no-playlist --simulate --socket-timeout 15 \
                --extractor-args "youtube:player_client=android_vr" \
                'https://www.youtube.com/watch?v=jNQXAC9IVRw' 2>/dev/null | head -1)"
        [[ "$id" == "jNQXAC9IVRw" ]] && break
        sleep 3
    done
    local yver; yver="$(yt-dlp --version 2>/dev/null || echo '?')"
    if [[ "$id" == "jNQXAC9IVRw" ]]; then
        write_gate G7 pass "yt-dlp $yver resolved https://www.youtube.com/watch?v=jNQXAC9IVRw (android_vr) -> $id"
    else
        write_gate G7 soft "yt-dlp $yver could not resolve the test URL via android_vr (distro yt-dlp too old on Debian/Ubuntu? current pip yt-dlp resolves) — got: '${id:-<empty>}'"
    fi
}

# ---------------------------------------------------------------- G9 -------
gate_g9_cava() {
    grep -q "mpd-cava.fifo" "$HOME_DIR/.config/mpd/mpd.conf" \
        || { write_gate G9 fail "fifo not configured in mpd.conf"; return 1; }
    [[ -p /tmp/mpd-cava.fifo ]] || { write_gate G9 fail "fifo /tmp/mpd-cava.fifo does not exist (MPD not writing?)"; return 1; }
    # run cava exactly like the app does (raw stdout protocol, fifo input);
    # background + kill -9 (a blocked fifo read can swallow SIGTERM/timeout).
    TERM=xterm-256color cava -p "$HOME_DIR/.config/cava/config" \
        > /tmp/cava-g9.out 2>&1 &
    local cpid=$!
    sleep 4
    local alive=0
    kill -0 "$cpid" 2>/dev/null && alive=1
    kill -9 "$cpid" 2>/dev/null || true
    wait "$cpid" 2>/dev/null || true
    local bytes; bytes="$(wc -c < /tmp/cava-g9.out 2>/dev/null || echo 0)"
    if [[ $alive -eq 1 && "$bytes" -gt 0 ]]; then
        write_gate G9 pass "cava ran headless on the MPD fifo for 4s (raw output, $bytes bytes)"
    elif [[ $alive -eq 1 ]]; then
        write_gate G9 soft "cava ran 4s but produced no output (fifo silent?) — log: $(head -c 200 /tmp/cava-g9.out)"
    else
        write_gate G9 soft "cava exited early — log: $(head -c 300 /tmp/cava-g9.out)"
    fi
}

# ---------------------------------------------------------------- G10 ------
gate_g10_tui_smoke() {
    tmux kill-server >/dev/null 2>&1 || true
    local s2u_bin; s2u_bin="$(command -v s2udio 2>/dev/null || echo "$HOME_DIR/.local/bin/s2udio")"
    tmux new-session -d -s s2u-tui "TERM=xterm-256color $s2u_bin" >/dev/null 2>&1
    local pane=""
    for _ in $(seq 1 15); do
        sleep 1
        pane="$(tmux capture-pane -t s2u-tui -p 2>/dev/null | tr '\n' ' ')"
        grep -q "Queue" <<<"$pane" && break
    done
    local alive=0
    pgrep -x s2udio >/dev/null 2>&1 && alive=1
    tmux kill-session -t s2u-tui >/dev/null 2>&1 || true
    pkill -x s2udio 2>/dev/null || true
    if grep -q "Queue" <<<"$pane"; then
        write_gate G10 pass "TUI launched in tmux pty; capture-pane shows the Queue tab (pid alive=$alive)"
    else
        write_gate G10 fail "TUI did not render the Queue tab (pane: $(echo "$pane" | head -c 300))"
        return 1
    fi
}

# ---------------------------------------------------------------- G11 ------
gate_g11_s2u_svc() {
    local backend
    backend="$("$SVC_BIN" backend 2>/dev/null || echo unknown)"
    local svc="s2u-svc-g11.service"
    if [[ "$backend" != "systemd-user" ]]; then
        # no systemd test unit on launcher/runit/s6/openrc backends:
        # round-trip the real mpd service through the abstraction
        svc="mpd"
        "$SVC_BIN" stop "$svc" >/dev/null 2>&1 || true
    else
        "$SVC_BIN" stop "$svc" >/dev/null 2>&1 || true
    fi
    sleep 1
    if "$SVC_BIN" is-active "$svc" >/dev/null 2>&1; then
        "$SVC_BIN" stop "$svc" >/dev/null 2>&1 || true
        sleep 1
    fi
    "$SVC_BIN" start "$svc" >/dev/null 2>&1 || {
        write_gate G11 fail "s2u-svc start failed"; return 1; }
    sleep 1
    "$SVC_BIN" is-active "$svc" >/dev/null 2>&1 || {
        write_gate G11 fail "s2u-svc is-active not active after start"; return 1; }
    "$SVC_BIN" restart "$svc" >/dev/null 2>&1 || {
        write_gate G11 fail "s2u-svc restart failed"; return 1; }
    sleep 1
    "$SVC_BIN" is-active "$svc" >/dev/null 2>&1 || {
        write_gate G11 fail "s2u-svc is-active not active after restart"; return 1; }
    # leave the service running (mpd is needed by nothing after G11, but a
    # stopped launcher mpd would leave stale pidfiles on some targets)
    write_gate G11 pass "s2u-svc start/stop/restart/is-active round-tripped on backend=$backend (svc=$svc)"
}

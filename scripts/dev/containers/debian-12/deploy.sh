#!/usr/bin/env bash
# Debian 12 deploy — sources the shared deploy steps. Debian ships mpd as a
# SYSTEM service (mpd user, /etc/mpd.conf, port 6600) — plan §6.2 decision
# point: stop+disable the system unit and run the user-level instance
# (s2udio's model). Verified in-container: /lib/systemd/system/mpd.service
# exists and auto-starts on install.
set -euo pipefail
source /s2udio/scripts/dev/lib.sh
source /s2udio/scripts/dev/containers/common/deploy-common.sh

start_user_session
systemctl --user daemon-reload

# Debian's mpd package ships + enables a system unit; it would own port 6600
# as the `mpd` user. Stop+disable it (user-level instance takes over).
if systemctl list-unit-files 2>/dev/null | grep -q '^mpd.service'; then
    systemctl stop mpd.service >/dev/null 2>&1 || true
    systemctl disable mpd.service >/dev/null 2>&1 || true
    ok "system mpd.service stopped+disabled (user unit takes over)"
fi

systemctl --user enable --now mpd.service >/dev/null 2>&1 || true
systemctl --user enable --now mpDris2.service >/dev/null 2>&1 || true
sleep 2
systemctl --user is-active mpd.service && ok "mpd.service (user) active" || warn "mpd.service not active yet"
systemctl --user is-active mpDris2.service && ok "mpDris2.service (user) active" || warn "mpDris2.service not active yet"
ok "debian-12 deploy complete"

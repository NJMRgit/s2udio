#!/usr/bin/env bash
# Fedora 41 deploy — sources the shared deploy steps. Fedora has no system
# mpd unit from the RPM Fusion package (verify), so the user unit is the
# only one; nothing to stop/disable at system level.
set -euo pipefail
source /s2udio/scripts/dev/lib.sh
source /s2udio/scripts/dev/containers/common/deploy-common.sh

# session plumbing (dbus + systemd --user) must be up before enabling units
start_user_session
systemctl --user daemon-reload

# If a system-level mpd unit exists (some packages ship one), stop+disable
# it so the user-level instance owns port 6600 (plan §6.2 decision: s2udio's
# model is user-level).
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
ok "fedora-41 deploy complete"

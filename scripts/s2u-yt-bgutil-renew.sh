#!/bin/bash
# Randomized pre-expiry restart of the s2u-yt PO-token provider.
# The minter has a 12h lifetime; a stale BotGuard handshake fails renewal,
# so restart the process well before the cliff to force a fresh mint.
set -euo pipefail
systemctl --user restart s2u-yt-bgutil.service
logger -t s2u-yt-bgutil-renew "bgutil restarted by randomized timer at $(date -Is)"

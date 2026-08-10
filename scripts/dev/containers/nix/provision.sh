#!/usr/bin/env bash
# Nix provision — enable flakes + make the copied tree a git repo (the
# flake's build.rs/vergen needs one; the host tree keeps no .git).
set -euo pipefail
source /s2udio/scripts/dev/lib.sh

info "nix: enabling flake support"
mkdir -p /root/.config/nix
printf 'experimental-features = nix-command flakes\n' > /root/.config/nix/nix.conf

info "nix: git repo for the flake source (vergen)"
cd /s2udio
git config --global --add safe.directory /s2udio
if [[ ! -d .git ]]; then
    git init -q
    git -c user.email=harness@localhost -c user.name="s2udio harness" add -A
    git -c user.email=harness@localhost -c user.name="s2udio harness" commit -qm "harness container snapshot"
fi
nix --version
ok "nix provision complete"

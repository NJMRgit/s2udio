# s2udio distro-support harness — Arch Linux target (systemd; plan
# docs/design/Validation/distro-support-followups.md T6b). Fresh
# archlinux:base-devel (rolling, rebuilt daily); systemd is part of the
# `base` group, but we refresh the image + pin systemd explicitly so the
# systemd-user session pattern (lib.sh start_user_session) has a
# known-good manager. NO AUR helper (yay/paru) is installed — the
# no-AUR-helper warn paths of run_arch are what T6b exercises; the AUR
# branch (mpdris2-git, mpv-full) stays covered by the hermetic mock
# (scripts/dev/test-setup-mock.py).
# rust: the Arch path's build prerequisite (base-devel ships gcc/make but
# not rust; the real-host hint setup.sh prints is `sudo pacman -S rust`).
FROM docker.io/library/archlinux:base-devel
RUN pacman -Syu --noconfirm && pacman -S --noconfirm systemd rust
CMD ["/sbin/init"]

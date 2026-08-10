# s2udio distro-support harness — Void glibc target (runit distro; the
# container validates via the s2u-svc runit-user backend: per-user runsvdir
# under ~/.config/runit + sv).
FROM ghcr.io/void-linux/void-glibc
# bash/tar/coreutils: the harness driver + in-container scripts need them
# (the Void image ships none of them)
RUN xbps-install -Sy >/dev/null 2>&1 && xbps-install -y bash tar coreutils >/dev/null 2>&1
CMD ["sleep", "infinity"]

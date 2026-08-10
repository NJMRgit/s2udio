# s2udio distro-support harness — Fedora 41 init image (systemd as PID 1)
FROM registry.fedoraproject.org/fedora:41
# systemd + RPM Fusion (Fedora dropped the mpd server from the official repos;
# RPM Fusion is the standard extra repo, Arch-AUR-analogous per plan §5).
RUN dnf -y install systemd \
 && dnf -y install --nogpgcheck \
      https://download1.rpmfusion.org/free/fedora/rpmfusion-free-release-41.noarch.rpm \
 && dnf clean all
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]

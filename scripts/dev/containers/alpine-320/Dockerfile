# s2udio distro-support harness — Alpine 3.20 target (OpenRC distro; the
# container validates via the s2u-svc plain-launcher backend — plan §6.1:
# OpenRC/sysvinit go through the launcher unless root services are acceptable).
FROM docker.io/library/alpine:3.20
# bash: the harness driver + in-container scripts are bash (Alpine base is busybox sh)
RUN apk add --no-cache bash
CMD ["sleep", "infinity"]

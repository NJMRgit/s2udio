#!/usr/bin/env python3
"""Regenerate scripts/s2u-helper from the five source programs.

Usage:  python3 scripts/dev/build-helper.py

Reads scripts/{s2u-mpv-tracker,s2udio-mpris,s2u-mpdris2,s2u-svc,
s2u-yt-bgutil-renew.sh}, applies the small consolidation rewires (the
tracker's internal spawns/pid-guard now use `s2u-helper`), and writes
scripts/s2u-helper -- the single deployed support executable.
"""
import os

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCRIPTS = os.path.join(REPO, "scripts")
OUT = os.path.join(SCRIPTS, "s2u-helper")
# (subcommand name, source file) — the embedded dict is keyed by the
# subcommand names the dispatcher accepts.
PROGRAMS = [("tracker", "s2u-mpv-tracker"), ("mpris", "s2udio-mpris"),
            ("mpdris2", "s2u-mpdris2"), ("svc", "s2u-svc"),
            ("bgutil-renew", "s2u-yt-bgutil-renew.sh")]

TRACKER_REWIRES = [
    ('if "s2u-mpv-tracker" in cmdline:',
     'if "s2u-helper" in cmdline or "s2u-mpv-tracker" in cmdline:'),
    ('subprocess.run(["s2u-svc", action, "mpDris2.service"],',
     'subprocess.run(["s2u-helper", "svc", action, "mpDris2.service"],'),
    ('r = subprocess.run(["pgrep", "-f", "[s]2udio-mpris"],',
     'r = subprocess.run(["pgrep", "-f", "([s]2udio-mpris|[s]2u-helper mpris)"],'),
    ('subprocess.Popen(["s2udio-mpris"], stdin=devnull,',
     'subprocess.Popen(["s2u-helper", "mpris"], stdin=devnull,'),
]

HEADER = """#!/usr/bin/env python3
# s2u-helper - one executable for every s2udio helper program (round 70).
#
# Subcommands:
#   tracker       caretaker daemon: keeps the MPRIS state file + Jellyfin
#                 tracking alive when s2udio closes while a video plays
#   mpris         MPRIS bridge for the mpv session (org.mpris.MediaPlayer2
#                 .s2udio) - desktop media controls for the video
#   mpdris2       shim that runs the official mpDris2 with the stream-art /
#                 notification / seek-hardening extensions
#   svc           service-manager abstraction (systemd-user, runit-user,
#                 s6-user, openrc, sysvinit, plain-launcher backends)
#   bgutil-renew  randomized pre-expiry restart of the s2u-yt PO-token
#                 minter (12 h cliff)
#
# The five source programs are embedded below (regenerate with
# `python3 scripts/dev/build-helper.py` after editing any of them); each
# runs in its own namespace so name clashes cannot happen.
import os
import sys

_SOURCES = _SOURCES_EMBED_HERE


def _run_python(name):
    src = _SOURCES[name]
    globs = {
        "__name__": "__main__",
        "__file__": os.path.realpath(__file__),
        "__builtins__": __builtins__,
    }
    sys.argv = [sys.argv[0]] + sys.argv[2:]
    exec(compile(src, "<s2u-helper:%s>" % name, "exec"), globs)


def _run_bash(name):
    # exec replaces this process, so exit codes and systemd ExecStart
    # semantics match the original shell scripts exactly.
    os.execv("/bin/bash", ["/bin/bash", "-c", _SOURCES[name], "s2u-helper"]
             + sys.argv[2:])


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in _SOURCES:
        sys.stderr.write(
            "usage: s2u-helper <tracker|mpris|mpdris2|svc|bgutil-renew> "
            "[args...]\\n")
        return 64
    name = sys.argv[1]
    if name in ("svc", "bgutil-renew"):
        _run_bash(name)
    else:
        _run_python(name)
    return 0  # unreachable for the bash programs; Python ones exit/loop


if __name__ == "__main__":
    sys.exit(main())
"""


def _embedded_sources():
    """Round 70 consolidated the five programs INTO scripts/s2u-helper; when
    the standalone source files are absent (current layout), regenerate from
    the embedded copies — the single file of truth — instead of failing."""
    ns = {"__name__": "not_main"}
    with open(OUT, encoding="utf-8") as f:
        exec(compile(f.read(), OUT, "exec"), ns)
    return ns["_SOURCES"]


def main():
    sources = {}
    embedded = None
    for sub, file in PROGRAMS:
        path = os.path.join(SCRIPTS, file)
        try:
            with open(path, encoding="utf-8") as f:
                text = f.read()
            from_embedded = False
        except OSError:
            if embedded is None:
                embedded = _embedded_sources()
            text = embedded[sub]
            from_embedded = True
        if sub == "tracker" and not from_embedded:
            # Rewires apply only when regenerating from the standalone
            # source; the embedded copy already carries them.
            for old, new in TRACKER_REWIRES:
                assert old in text, "rewire target missing in %s: %r" % (sub, old)
                text = text.replace(old, new)
        sources[sub] = text
    lines = ["    %s: %r,\n" % (repr(sub), sources[sub]) for sub, _ in PROGRAMS]
    out = HEADER.replace("_SOURCES_EMBED_HERE", "{\n" + "".join(lines) + "}")
    with open(OUT, "w", encoding="utf-8") as f:
        f.write(out)
    os.chmod(OUT, 0o755)
    print("wrote %s (%d bytes)" % (OUT, len(out)))


if __name__ == "__main__":
    main()

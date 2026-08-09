#!/usr/bin/env python3
"""Hermetic mocked-matrix test for setup.sh (distro dispatcher) — plan §6.2.

Generates fake package managers (pacman/dnf5/apt/apk/xbps/systemctl/...) in
a temp PATH shim + fake /etc/os-release fixtures, then runs the REAL setup.sh
against them and asserts, per backend:

  - correct backend detection (ID / ID_LIKE -> pacman/dnf5/apt/apk/xbps/nix)
  - correct package names on -y (recommended defaults)
  - NO installs on non-interactive runs without -y
  - Arch path output byte-identical to the pre-dispatcher setup.sh
    (regression guarantee; the dispatcher must not change Arch behavior)

Usage: scripts/dev/test-setup-mock.py        (run from the repo root)
Exit 0 = all checks pass. No root, no containers, no network needed.
"""
import os, pathlib, shutil, signal, stat, subprocess, sys, tempfile

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parent.parent
BIN = pathlib.Path(tempfile.mkdtemp(prefix='s2u-mock-bin-', dir='/tmp'))
CU = pathlib.Path(tempfile.mkdtemp(prefix='s2u-mock-cu-', dir='/tmp'))
OSREL = pathlib.Path(tempfile.mkdtemp(prefix='s2u-mock-osrel-', dir='/tmp'))
HOMES = pathlib.Path(tempfile.mkdtemp(prefix='s2u-mock-homes-', dir='/tmp'))
LOG_ROOT = pathlib.Path(tempfile.mkdtemp(prefix='s2u-mock-logs-', dir='/tmp'))

def shim(name, body):
    p = BIN / name
    p.write_text('#!/usr/bin/env bash\n' + body)
    p.chmod(p.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

LOG = 'echo "$(basename "$0") $*" >>"${MOCK_CALLS:-/dev/null}"'

def make_shims():
    shim('pacman', f'{LOG}\ncase "$1" in -Q) exit 1 ;; *) exit 0 ;; esac')
    shim('yay', f'{LOG}\nexit 0')
    shim('paru', f'{LOG}\nexit 0')
    shim('sudo', f'{LOG}\nexec "$@"')
    shim('dnf5', f'{LOG}\nexit 0')
    shim('rpm', f'''{LOG}
if [[ "$1" == "-E" ]]; then echo "41"; exit 0; fi
exit 1''')
    shim('apt-get', f'{LOG}\nexit 0')
    shim('dpkg-query', f'{LOG}\nexit 1')
    shim('apk', f'''{LOG}
case "$1" in info) exit 1 ;; *) exit 0 ;; esac''')
    shim('xbps-query', f'{LOG}\nexit 1')
    shim('xbps-install', f'{LOG}\nexit 0')
    shim('systemctl', f'''{LOG}
case "$*" in
    *"--user is-system-running"*)
        [[ "${{MOCK_NO_SYSTEMD:-0}}" == "1" ]] && exit 1
        echo running; exit 0 ;;
    *"--user list-unit-files"*)
        [[ "${{MOCK_USER_MPD:-0}}" == "1" ]] && {{ echo "mpd.service enabled"; echo "mpDris2.service enabled"; }}
        exit 0 ;;
    *"--user is-enabled"*) exit 0 ;;
    *"--user is-active"*) echo active; exit 0 ;;
    *"--user daemon-reload"*) exit 0 ;;
    *"--user enable --now"*) exit 0 ;;
    *"--user enable"*) exit 0 ;;
    *"--user start"*) exit 0 ;;
    *"--user restart"*) exit 0 ;;
    *"--user stop"*) exit 0 ;;
    *"list-unit-files"*)
        [[ "${{MOCK_SYSTEM_MPD:-0}}" == "1" ]] && echo "mpd.service enabled"
        exit 0 ;;
    *"is-system-running"*) echo running; exit 0 ;;
    *"stop mpd.service"*) exit 0 ;;
    *"disable mpd.service"*) exit 0 ;;
    *) exit 0 ;;
esac''')
    shim('cargo', f'''{LOG}
case "$1" in
    build)
        mkdir -p target/release
        printf '#!/usr/bin/env bash\\necho "s2udio 0.11.0 (mock)"\\n' > target/release/s2u
        chmod +x target/release/s2u
        exit 0 ;;
    --version) echo "cargo 1.97.1 (mock)"; exit 0 ;;
    *) exit 0 ;;
esac''')
    shim('rustc', f'''{LOG}
echo "rustc ${{MOCK_RUSTC_VERSION:-1.80.0}} (mock, 1970-01-01)"; exit 0''')
    shim('rustup', f'{LOG}\nexit 0')
    shim('yt-dlp', f'''{LOG}
if [[ "$1" == "--version" ]]; then echo "${{MOCK_YTDLP_VERSION:-2025.06.30}}"; exit 0; fi
exit 0''')
    shim('cava', f'''{LOG}
if [[ "$1" == "-v" ]]; then echo "cava 0.10.3 (mock)"; exit 0; fi
exit 0''')
    shim('mpv', f'{LOG}\nexit 0')
    shim('mpd', f'''{LOG}
if [[ "$1" == "--no-daemon" ]]; then exec sleep 3600; fi
exit 0''')
    shim('git', f'''{LOG}
if [[ "$1" == "clone" ]]; then
    DIR=""
    for a in "$@"; do [[ "$a" == /tmp/* ]] && DIR="$a"; done
    [[ -z "$DIR" ]] && DIR="/tmp/cava-src"
    mkdir -p "$DIR"
    printf '#!/usr/bin/env bash\\nexit 0\\n' > "$DIR/autogen.sh"
    printf '#!/usr/bin/env bash\\nexit 0\\n' > "$DIR/configure"
    printf 'all:\\n\\t@true\\n' > "$DIR/Makefile"
    printf '#!/usr/bin/env bash\\necho "cava 0.10.3 (source mock)"\\n' > "$DIR/cava"
    chmod +x "$DIR/autogen.sh" "$DIR/configure" "$DIR/cava"
fi
exit 0''')
    shim('curl', f'''{LOG}
OUT=""; prev=""
for a in "$@"; do [[ "$prev" == "-o" ]] && OUT="$a"; prev="$a"; done
if [[ -n "$OUT" ]]; then
    printf '#!/usr/bin/env python3\\n# mock upstream mpDris2\\nif __name__ == "__main__":\\n    pass\\n' > "$OUT"
fi
exit 0''')
    shim('setcap', f'{LOG}\nexit 0')
    shim('nix', f'''{LOG}
case "$1" in
    profile)
        if [[ "$2" == "list" ]]; then echo "0 s2udio 0.11.0"; exit 0; fi
        exit 0 ;;
    --version) echo "nix (Nix) 2.24 (mock)"; exit 0 ;;
    *) exit 0 ;;
esac''')
    shim('sv', f'{LOG}\nexit 0')
    shim('runsvdir', f'''{LOG}
DIR="${{@: -1}}"
mkdir -p "$DIR/mpd/supervise" "$DIR/mpDris2/supervise"
exit 0''')
    shim('setsid', f'{LOG}\nexec "$@"')
    shim('pgrep', f'{LOG}\nexit 1')
    shim('nproc', 'echo 4')
    shim('install', f'''{LOG}
src=""; dest=""
for a in "$@"; do
    case "$a" in -*) ;; *) if [[ -z "$src" ]]; then src="$a"; else dest="$a"; fi ;; esac
done
[[ -z "$dest" ]] && {{ dest="$src"; src=""; }}
if [[ -n "$dest" ]]; then
    mkdir -p "$(dirname "$dest")" 2>/dev/null
    if [[ -n "$src" && -f "$src" ]]; then cp "$src" "$dest" 2>/dev/null; fi
    chmod +x "$dest" 2>/dev/null
fi
exit 0''')
    shim('tee', f'''{LOG}
cat >/dev/null
for t in "$@"; do
    [[ "$t" != -* ]] && {{ mkdir -p "$(dirname "$t")" 2>/dev/null; touch "$t" 2>/dev/null; }}
done
exit 0''')
    shim('python3', f'''{LOG}
case "$1" in -m) exit 0 ;; --version) echo "Python 3.12 (mock)"; exit 0 ;; *) exit 0 ;; esac''')

    # coreutils-only dir (no host package-manager/toolchain leakage)
    for t in ['bash', 'sh', 'env', 'sed', 'grep', 'cat', 'cp', 'mkdir', 'mv', 'rm', 'chmod',
              'head', 'tr', 'awk', 'seq', 'sleep', 'touch', 'dirname', 'ls', 'cut', 'wc',
              'readlink', 'printf', 'true', 'false', 'test', 'pwd', 'basename', 'expr',
              'tail', 'sort', 'find', 'xargs', 'stat', 'date', 'make']:
        src = shutil.which(t)
        if src:
            (CU / t).symlink_to(src)

    os_release = {
        'arch': 'NAME="Arch Linux"\nID=arch\nPRETTY_NAME="Arch Linux"\n',
        'fedora': 'NAME="Fedora Linux"\nVERSION="41"\nID=fedora\nID_LIKE="fedora"\n',
        'debian': 'NAME="Debian GNU/Linux"\nVERSION="12 (bookworm)"\nID=debian\n',
        'ubuntu': 'NAME="Ubuntu"\nVERSION="24.04 LTS"\nID=ubuntu\nID_LIKE=debian\n',
        'alpine': 'NAME="Alpine Linux"\nID=alpine\nVERSION_ID=3.20.3\n',
        'void': 'NAME="void"\nID="void"\n',
        'nixos': 'NAME="NixOS"\nID=nixos\nID_LIKE=""\n',
        'unknown': 'NAME="SomethingOS"\nID=somethingos\n',
    }
    for k, v in os_release.items():
        (OSREL / k).write_text(v)

def fresh_home(key, mode):
    home = HOMES / f'{key}-{mode}'
    shutil.rmtree(home, ignore_errors=True)
    (home / '.config' / 'mpd').mkdir(parents=True)
    (home / '.config' / 'mpd' / 'mpd.conf').write_text('music_directory "~/Music"\nport "6600"\n')
    return home

def run_case(key, args=(), extra_env=None, drop=(), old=False, mode=None):
    # -y vs non-interactive runs MUST use separate homes, or fresh_home()
    # deletes the pidfile of a still-running launcher mpd (orphan leak)
    if mode is None:
        mode = 'y' if '-y' in args else 'noy'
    home = fresh_home(key, mode)
    calls = LOG_ROOT / f'calls-{key}-{mode}.log'
    calls.unlink(missing_ok=True)
    bindir = LOG_ROOT / f'bin-{key}-{mode}'
    shutil.rmtree(bindir, ignore_errors=True)
    bindir.mkdir()
    for p in BIN.iterdir():
        if p.name not in drop:
            (bindir / p.name).symlink_to(p)
    env = dict(os.environ)
    env.update({'HOME': str(home), 'PATH': str(bindir) + ':' + str(CU),
                'MOCK_CALLS': str(calls), 'S2UDIO_OS_RELEASE': str(OSREL / key)})
    if extra_env:
        env.update(extra_env)
    script = REPO / 'setup.sh'
    if old:
        # the pre-dispatcher baseline does `cd "$(dirname "$0")"`, so it must
        # run from a dir where the repo-relative scripts/ + assets/ resolve
        olddir = LOG_ROOT / f'old-{key}-{mode}'
        shutil.rmtree(olddir, ignore_errors=True)
        olddir.mkdir(parents=True)
        shutil.copy(HERE / 'fixtures' / 'setup.sh.arch-baseline', olddir / 'setup.sh')
        (olddir / 'scripts').symlink_to(REPO / 'scripts')
        (olddir / 'assets').symlink_to(REPO / 'assets')
        script = olddir / 'setup.sh'
        env.pop('S2UDIO_OS_RELEASE', None)
    r = subprocess.run(['bash', str(script), *args], cwd=str(REPO),
                       capture_output=True, text=True, env=env, stdin=subprocess.DEVNULL)
    out = r.stdout + r.stderr
    calls_txt = calls.read_text() if calls.exists() else ''
    return {'rc': r.returncode, 'out': out, 'calls': calls_txt}

results = []
def check(name, cond, detail=''):
    results.append((name, bool(cond), detail))
    print(f'  [{"PASS" if cond else "FAIL"}] {name}' + (f' — {detail}' if detail and not cond else ''))

def main():
    make_shims()
    DNF5 = 'dnf5 install -y mpd mpdris2 cava yt-dlp mpv ffmpeg python3-dbus python3-gobject python3-mutagen gcc make git curl'
    APT = 'apt-get install -y --no-install-recommends mpd mpdris2 cava yt-dlp mpv ffmpeg python3-dbus python3-gi python3-mutagen build-essential git curl'
    APK = 'apk add --no-cache mpd mpv yt-dlp ffmpeg python3 py3-dbus py3-gobject3 py3-mutagen py3-pip build-base git curl fftw-dev iniparser-dev ncurses-dev sdl2-dev autoconf automake libtool ncurses-terminfo-base'
    XBPS = 'xbps-install -Sy mpd mpv yt-dlp cava ffmpeg mpDris2 python3 python3-dbus python3-gobject python3-mutagen python3-mpd2 base-devel cargo rust git curl util-linux procps-ng ncurses-term'

    print('== arch -y (byte-identity old vs new) ==')
    r_old = run_case('arch', ['-y'], extra_env={'MOCK_USER_MPD': '1'}, old=True, mode='y')
    r_new = run_case('arch', ['-y'], extra_env={'MOCK_USER_MPD': '1'}, mode='y')
    check('arch -y: output byte-identical', r_old['out'] == r_new['out'],
          f"old {len(r_old['out'])}B new {len(r_new['out'])}B")
    check('arch -y: pacman backend + installs (mpd ffmpeg cava python-yt-dlp)',
          'pacman -S --needed mpd ffmpeg cava python-yt-dlp' in r_new['calls'])
    check('arch -y: AUR mpdris2-git + mpv-full via yay',
          'yay -S --needed mpdris2-git' in r_new['calls'] and 'yay -S --needed mpv-full' in r_new['calls'])
    check('arch -y: exit 0', r_new['rc'] == 0, f"rc={r_new['rc']}")

    print('== arch non-interactive no -y (byte-identity) ==')
    r_old = run_case('arch', extra_env={'MOCK_USER_MPD': '1'}, old=True, mode='noy')
    r_new = run_case('arch', extra_env={'MOCK_USER_MPD': '1'}, mode='noy')
    check('arch no-y: output byte-identical', r_old['out'] == r_new['out'])
    check('arch no-y: no installs', 'pacman -S ' not in r_new['calls'] and 'yay -S ' not in r_new['calls'])

    print('== fedora (dnf5) ==')
    r = run_case('fedora', ['-y'], extra_env={'MOCK_RUSTC_VERSION': '1.80.0', 'MOCK_SYSTEM_MPD': '0'})
    check('fedora: detected dnf5 backend', '-> dnf5 backend' in r['out'])
    check('fedora -y: RPM Fusion enabled', 'rpmfusion-free-release-41.noarch.rpm' in r['calls'])
    check('fedora -y: correct package names', DNF5 in r['calls'])
    check('fedora -y: rustup (rustc too old)', 'rustup toolchain install stable' in r['calls'])
    check('fedora -y: services via s2u-svc systemd-user', 'systemctl --user enable mpd.service' in r['calls'])
    check('fedora -y: no stale-yt-dlp pip hint', 'pip install -U --break-system-packages' not in r['out'])
    r = run_case('fedora', extra_env={'MOCK_RUSTC_VERSION': '1.80.0'})
    check('fedora no-y: no installs', 'dnf5 install' not in r['calls'] and 'rustup' not in r['calls'])
    r = run_case('fedora', drop=('cargo', 'rustc'))
    check('fedora no-cargo no-y: rustup not auto-installed', 'rustup' not in r['calls'])
    check('fedora no-cargo no-y: build warns', 'cargo not found' in r['out'])

    print('== debian-12 / ubuntu-2404 (apt) ==')
    for key, label in (('debian', 'debian-12'), ('ubuntu', 'ubuntu-2404')):
        r = run_case(key, ['-y'], extra_env={'MOCK_RUSTC_VERSION': '1.63.0', 'MOCK_SYSTEM_MPD': '1', 'MOCK_YTDLP_VERSION': '2024.04.09'})
        check(f'{label}: detected apt backend', '-> apt backend' in r['out'])
        check(f'{label} -y: correct package names', APT in r['calls'])
        check(f'{label} -y: system mpd stopped+disabled', 'systemctl stop mpd.service' in r['calls'] and 'systemctl disable mpd.service' in r['calls'])
        check(f'{label} -y: services via s2u-svc', 'systemctl --user enable mpd.service' in r['calls'])
        check(f'{label} -y: stale yt-dlp pip hint (§12.7)', 'pip install -U --break-system-packages yt-dlp' in r['out'])
        r = run_case(key, extra_env={'MOCK_SYSTEM_MPD': '1'})
        check(f'{label} no-y: no installs', 'apt-get install' not in r['calls'] and 'apt-get update' not in r['calls'])

    print('== alpine (apk) ==')
    r = run_case('alpine', ['-y'], extra_env={'MOCK_NO_SYSTEMD': '1'}, drop=('cava',))
    check('alpine: detected apk backend', '-> apk backend' in r['out'])
    check('alpine -y: correct package names', APK in r['calls'])
    check('alpine -y: cava built from source', 'git clone' in r['calls'] and 'cava built from source' in r['out'])
    check('alpine -y: services via s2u-svc launcher', 'mpd active (launcher)' in r['out'])
    r = run_case('alpine', extra_env={'MOCK_NO_SYSTEMD': '1'}, drop=('cava',))
    check('alpine no-y: no installs', 'apk add' not in r['calls'] and 'git clone' not in r['calls'] and 'curl ' not in r['calls'])

    print('== void (xbps) ==')
    r = run_case('void', ['-y'], extra_env={'MOCK_RUSTC_VERSION': '1.97.1', 'MOCK_NO_SYSTEMD': '1'})
    check('void: detected xbps backend', '-> xbps backend' in r['out'])
    check('void -y: correct package names', XBPS in r['calls'])
    check('void -y: mpd setcap -r (§12.8)', 'setcap -r /usr/bin/mpd' in r['calls'])
    check('void -y: no rustup (rustc 1.97.1)', 'rustup' not in r['calls'])
    check('void -y: services via s2u-svc runit', 'sv start' in r['calls'] and 'mpd active (runit-user)' in r['out'])
    r = run_case('void', extra_env={'MOCK_RUSTC_VERSION': '1.97.1', 'MOCK_NO_SYSTEMD': '1'})
    check('void no-y: no installs/setcap', 'xbps-install' not in r['calls'] and 'setcap' not in r['calls'])

    print('== nixos (nix) ==')
    r = run_case('nixos', ['-y'], extra_env={'MOCK_NO_SYSTEMD': '1'})
    check('nixos: detected nix backend', '-> nix backend' in r['out'])
    check('nixos -y: flake profile install (app+bridge)', 'nix profile install .#s2udio .#bridgePython' in r['calls'])
    check('nixos -y: runtime deps via nixpkgs', all(x in r['calls'] for x in ('nixpkgs#mpd', 'nixpkgs#mpv', 'nixpkgs#cava', 'nixpkgs#mpdris2')))
    check('nixos -y: services via s2u-svc launcher', 'mpd active (launcher)' in r['out'])
    r = run_case('nixos', extra_env={'MOCK_NO_SYSTEMD': '1'})
    check('nixos no-y: no profile install', 'nix profile install' not in r['calls'])

    print('== unknown distro ==')
    r = run_case('unknown', ['-y'])
    check('unknown: fatal + nonzero', r['rc'] != 0 and 'unsupported distro' in r['out'], f"rc={r['rc']}")

    # cleanup mock launcher procs (kill any mpd left running by s2u-svc's
    # launcher backend; stale pidfiles for already-exited processes are fine)
    for pidfile in HOMES.glob('*/.cache/s2udio/svc/*.pid'):
        try:
            os.kill(int(pidfile.read_text().strip()), signal.SIGKILL)
        except Exception:
            pass

    print()
    fails = [n for n, ok, _ in results if not ok]
    print(f'=== {len(results) - len(fails)}/{len(results)} checks passed ===')
    return 1 if fails else 0

if __name__ == '__main__':
    try:
        sys.exit(main())
    finally:
        shutil.rmtree(BIN, ignore_errors=True)
        shutil.rmtree(CU, ignore_errors=True)
        shutil.rmtree(OSREL, ignore_errors=True)
        shutil.rmtree(HOMES, ignore_errors=True)
        shutil.rmtree(LOG_ROOT, ignore_errors=True)

# s2udio
<div align="right">
            <a href="https://ko-fi.com/s7oned" target="_blank" style="display: inline-block;">
                <img
                    src="https://img.shields.io/badge/Donate-Ko--fi-F16061.svg?style=flat-square&logo=ko-fi" 
                    align="right"
                />
            </a></div>


A heavily modified fork of [rmpc](https://github.com/mierak/rmpc) v0.11.0

a fully featured media center TUI built on rmpc that adds support for video via mpv and a bunch of other goodies!

Built with the help of Deepseek v4 Flash, pi, and prime-agent

<img width="2065" height="924" alt="image" src="https://github.com/user-attachments/assets/9da9686c-52f3-4b3c-8ef7-cfffe20a7270" />
<img width="1400" height="163" alt="image" src="https://github.com/user-attachments/assets/e4fc184f-61f7-4f23-8f32-0e15ba493362" />
Left: S2udio ootb

Right: RMPC ootb


## Added Features:
- synchronized lyrics + fetch
- Jellyfin!
- Online radio - browse and listen to stations all over the world!
    * Jellyfin/Radio tabs can be disabled in options
- Online streams via yt-dlp
    * includes description and chapters for yt
- TUI maintained playlists (persist if MPV is closed)
- mpv/mpris helper - wraps mpris data and manages/tracks mpv playback.
    * allows tui to be closed during playback without interruptions
- copy + paste and drag n' drop support for audio and video files/links/.magnet/torrent
- play videos as audio
- download stream/torrent
- SVP4 (SmoothVideo Project) support — a Settings -> mpv "svp support"
  toggle wires playback to SVP's fixed IPC socket (`/tmp/mpvsocket`), and
  `mpv.bin` can point at SVP's bundled mpv (its own VapourSynth + Python
  3.12) so SVPflow/RIFE frame interpolation runs without crashing mpv
- full mouse controls
- sensible and intuitive key binds
   

## Showcase
### Jellyfin
https://github.com/user-attachments/assets/ecb84436-4ce9-45d0-9dbf-a34cc159e31d
### Automatic theming (STTM only for now)
[Screencast_20260809_202700.webm](https://github.com/user-attachments/assets/9ed0e9b8-ecac-4f69-90aa-e39b247a3aa6)
### ctrl+click / alt+click / shift + up|down
[Screencast_20260809_202813.webm](https://github.com/user-attachments/assets/f4e21da4-28a9-4487-9e5f-19bc564d46bd)
### youtube audio streams with chapters
https://github.com/user-attachments/assets/6b049334-d996-49bb-8629-c7fdfdea82d5
### youtube video
https://github.com/user-attachments/assets/016da10c-c401-4e01-92d5-db684c548f97



## Dependencies
- yt-dlp
- mpDris2
- rqbit (torrent streaming only — static binary or `cargo install rqbit`; not installed by setup.sh)
- ffmpeg
- cava
- mpv
- mpd
- kitty* (not a hard requirement, but development and testing is focused on kitty)
- [STTM](https://github.com/NJMRgit/STTM) - TUI supports auto theming if using my KDE theme tool.
    * planned support for following KDE accent color

## Install

`setup.sh` is a multi-distro installer (a distro dispatcher): it detects the
distro via `/etc/os-release` (`ID` / `ID_LIKE`) and runs the matching backend.
`./setup.sh` prompts before every install; `./setup.sh -y` accepts without
asking (non-interactive runs without `-y` skip installs). Everything comes
from official distro repositories or the AUR — no patched cava, no patched
mpDris2, no yt-dlp-ejs. It is idempotent (safe to re-run) and never
overwrites existing configs.

### Distro support matrix

| Backend | Distros | Packages come from | Notes |
| --- | --- | --- | --- |
| `pacman` | Arch / CachyOS / Artix | official repos + AUR (mpdris2-git, mpv-full) | the original 9-step installer; `yt-dlp` is the `extra/yt-dlp` package (`python-yt-dlp` is **not** in the repos); needs `yay`/`paru` for the AUR packages |
| `dnf5` | Fedora | dnf5 + **RPM Fusion free** | Fedora dropped `mpd` from the official repos — RPM Fusion free provides mpd/full ffmpeg/full mpv |
| `apt` | Debian / Ubuntu / Devuan | apt | the distro's **system `mpd` is stopped+disabled**; s2udio runs a **user-level** MPD instance; prints a pip hint when the distro yt-dlp pin is stale |
| `apk` | Alpine | apk | **cava is built from source** (not in the Alpine 3.20 repos); **upstream python mpDris2** is installed at `/usr/bin/mpDris2` |
| `xbps` | Void | xbps | **mpd file capabilities are stripped** (`setcap -r /usr/bin/mpd`) — needed in restricted environments; services supervised via **runit-user** (`runsvdir` + `sv`) |
| `nix` | NixOS | `nix profile install` (flake.nix) | nixpkgs ships mpDris2 as a compiled ELF the shim cannot patch → **upstream python mpDris2** at `/usr/bin/mpDris2`; launcher services |

### Per-distro notes

- **RPM Fusion free (Fedora)**: the official Fedora repos dropped the `mpd`
  server, so the dnf5 backend enables RPM Fusion free (the Fedora analogue of
  Arch's AUR usage) to get `mpd`, full `ffmpeg` and full `mpv`.
- **System vs user MPD (Debian/Ubuntu/Devuan)**: the distro ships `mpd` as an
  auto-started **system** service on port 6600. setup.sh stops+disables it
  and runs s2udio's **user-level** instance (`~/.config/mpd/mpd.conf` +
  `mpd.service`, created when absent) — MPD then lives in your session, not
  the system (cava captures PipeWire directly; there is no MPD fifo tap).
- **cava from source (Alpine)**: cava is absent from the Alpine 3.20 repos;
  the apk backend clones and builds it (`autogen.sh && configure && make`,
  installed to `/usr/local/bin/cava`).
- **Upstream python mpDris2 (Alpine, NixOS)**: mpDris2 has no Alpine package,
  and nixpkgs ships a compiled ELF the s2u-mpdris2 stream-art shim cannot
  patch — setup.sh fetches the upstream python source and installs it at the
  shim's fixed `/usr/bin/mpDris2` path (python-mpd2 via pip on Alpine).
- **setcap (Void)**: Void's `mpd` ships file caps (`cap_ipc_lock,
  cap_sys_nice`) that restricted environments (containers) cannot grant →
  `execve` fails. The xbps backend strips them (`setcap -r /usr/bin/mpd`);
  harmless on real Void hosts.
- **Rust toolchain**: distro `rustc` is older than the edition-2024 MSRV
  (1.88) on Fedora/Debian/Ubuntu/Alpine — setup.sh installs a current
  toolchain via **rustup** (minimal profile). Void ships rustc ≥ 1.88 and
  skips it; the nix backend builds inside the flake sandbox.
- **yt-dlp package name (Arch/CachyOS/Artix)**: modern Arch repos carry
  `yt-dlp` (`extra/yt-dlp`); `python-yt-dlp` is NOT in the repos and would
  abort `setup.sh -y` at step 1 ("target not found") — the pacman backend
  installs `yt-dlp`.
- **mpv-full stays Arch-only**: the video pipeline is tuned for Arch's
  `mpv-full` (AUR, recommended on Arch); every other backend installs plain
  `mpv` and prints an informational note.
- **Services**: MPD + mpDris2 are enabled/started through `scripts/s2u-svc`
  (the init abstraction — systemd-user on Fedora/Debian/Ubuntu, runit-user on
  Void, plain launcher on Alpine/NixOS). The tracker's video/MPD MPRIS
  mutual exclusion also routes through s2u-svc, so it works on every backend.

**Known cosmetic limitation**: the installer's summary prints
`mpd: inactive / mpDris2: inactive` on non-systemd backends (Alpine, Void,
NixOS) even though the services are actually up — the summary's status check
uses `systemctl --user`, which only exists on systemd targets. Verify with
`~/.local/bin/s2u-svc is-active mpd` instead.

## Settings & Config
Configuration is stored at ~/.config/s2udio and separate from rmpc

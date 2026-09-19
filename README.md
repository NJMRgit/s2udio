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
- library playlist files — Settings > MPD "show .m3u/.pls/.xspf playlists from the music library": the Playlists tab also lists playlist files found inside the MPD music library (nested album .m3u files included), ♫-marked, opened with the app's own m3u/pls/xspf parsers (Add to Queue / Replace Queue); read-only — the app never edits or deletes the library files
   
## Dependencies
- yt-dlp
- mpDris2
- rqbit (torrent streaming + web UI; `./setup.sh --with-rqbit` installs the
  distro package, the interactive setup prompt offers it, or use a static
  binary / `cargo install rqbit` yourself)
- ffmpeg
- cava
- mpv
- mpd
- wl-clipboard (Ctrl+V / middle-click clipboard reads; bracketed paste and
  drag & drop work without it)
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
- **Services**: MPD + mpDris2 are enabled/started through `scripts/s2u-helper
  svc` (the init abstraction — systemd-user on Fedora/Debian/Ubuntu,
  runit-user on Void, plain launcher on Alpine/NixOS). The tracker's
  video/MPD MPRIS mutual exclusion also routes through it, so it works on
  every backend. All support programs (tracker caretaker, mpv MPRIS bridge,
  mpDris2 shim, svc, bgutil renewal) run from the one `s2u-helper`
  executable.

**Known cosmetic limitation**: the installer's summary prints
`mpd: inactive / mpDris2: inactive` on non-systemd backends (Alpine, Void,
NixOS) even though the services are actually up — the summary's status check
uses `systemctl --user`, which only exists on systemd targets. Verify with
`~/.local/bin/s2u-helper svc is-active mpd` instead.

## Settings & Config
Configuration is stored at ~/.config/s2udio and separate from rmpc

## rqbit — torrent web UI & commands

Torrent streaming runs through **rqbit** (`./setup.sh --with-rqbit`
installs the distro package, the interactive step offers it, or use a
static binary / `cargo install rqbit` yourself). Besides streaming
torrents to mpv, rqbit serves a **web UI** (torrent management +
peer/VPN-route verification) that s2udio exposes through a small
localhost proxy, so the browser needs no credentials:

```
s2udio rq start    start the standalone engine (idempotent; prints the web UI URL)
s2udio rq stop     stop the standalone engine
s2udio rq open     open the web UI (chromium --app=<url> --frameless window
                   when chromium is installed, else the default browser).
                   An already-open window is focused, not duplicated
s2udio rq tray     show a tray icon while the engine runs: menu `Open` /
                   `Shutdown`; a left DOUBLE click opens the web UI
                   (a single click does nothing). Starts an engine when none
                   is running and leaves when it is stopped; one tray per
                   machine
s2udio rq check    verify the proxy: /web/ + API answer without credentials
                   (200) while the engine port still rejects them (401)
s2udio dl status   torrent downloads in progress (name, status, %)
s2udio dl start    start the downloader daemon (idempotent; normally the
                   TUI starts it on the first committed download)
s2udio dl stop     stop the downloader daemon (partials stay)
```

Committed torrent downloads ("Stream and download", the file picker's
"Download & Play", "Download", "Download all") run in a detached
`s2udio dl` daemon, one rqbit engine per job, so a download finishes
even when the TUI exits mid-download; the Downloads modal is one list
of all downloads (torrent rows show `Source = Torrent`) with a context
menu on every row — Stop while active, Remove from list when done
(completed downloads persist until removed) — plus a one-shot
completion notice per finished download (state:
`~/.cache/s2udio/downloads.json`). Plain streams keep the ephemeral
in-TUI engines and stop their download when the stream ends.

- `rq start` spawns a detached daemon that owns the engine and the
  auth-injecting proxy; it reuses an engine the Settings panel started,
  and Settings → torrent → `web ui` reuses one the CLI started — the CLI
  and the GUI always share a single engine (registration:
  `~/.cache/s2udio/rqbit.json`).
- The web UI opens at `http://127.0.0.1:<port>/web/` (no credentials in
  the URL). The engine port itself stays protected by a random per-launch
  token — `rq check` verifies this split.
- **VPN routing**: set a SOCKS5 proxy via Settings → torrent → `socks
  proxy` (or the `torrent.socks_proxy` config key, e.g.
  `socks5://127.0.0.1:1080`) — all rqbit traffic then goes through it
  (`--socks-url`, incoming connections disabled). The web UI itself has
  no VPN settings (it is torrent management only); restart the engine
  after changing the proxy.
- `rq tray` is the engine's status icon (StatusNotifierItem; the monochrome
  light torrent glyph, drawn as a pixmap in the current colour scheme's
  foreground colour — no icon-name lookup is involved; the desktop entry
  points at the same glyph by absolute path): it tracks the engine — the icon is there exactly
  while the web UI is up — and its menu opens the web UI or shuts the
  engine down (which removes the icon). A left double click opens the web
  UI; a single click does nothing.
- The web UI is a **single window**: opening it again focuses the window
  that is already there (KWin scripting is asked to focus it — chromium's
  `--class`/`--wayland-app-id` are ignored on Wayland, so the window is
  matched by the class chromium derives from the URL); a window left over
  from an earlier engine (dead proxy port) is closed and replaced. It runs
  in its own chromium profile (`~/.cache/s2udio/chromium-webui`), so
  quitting your own browser does not close it.
- `setup.sh` also writes **S2RQ** (`~/.local/share/applications/s2rq.desktop`)
  when rqbit is on PATH: it runs `s2udio rq start` and then `s2udio rq tray`,
  so the menu entry both brings the engine up and shows the tray icon.
- Shorthand (fish): `alias s2rq 's2udio rq'` → `s2rq start|stop|open|tray|check`.

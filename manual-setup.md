# Manual setup (without `setup.sh`)

`setup.sh` is the supported installer. This guide is for people who prefer to
install by hand, or whose distro is not covered by the script. It mirrors what
the script does on Arch/CachyOS, Fedora, Debian/Ubuntu/Devuan, Alpine, Void
and NixOS; commands are shown for those families where they differ.

**Version note.** This guide describes the **current version**, s2udio
**0.11.0 git `4b2a5ff`** (2026-10-30) **or later**, the first master with the
**always-on daemon**. `s2udio version` prints the exact build
(`s2udio 0.11.0 git <sha>`); the daemon is in every build from `4b2a5ff`
onward. Installing an older, pre-daemon build? Follow the same sections except
section 5 (the daemon does not exist there), skip the daemon-only checks in
sections 11–12, and read
[Old versions (without the daemon)](#old-versions-without-the-daemon) at the
bottom for what differs — the last such master was **0.11.0 git `9e3b493`**
(2026-10-29).

Nothing here writes outside your home directory except the optional
`/opt/s2udio` prefix for programs no package manager owns (Alpine/NixOS
mpDris2, a cava built from source). Use `sudo` only where it is shown.

If you only want the quick version: install the dependencies, `cargo build
--release`, put `target/release/s2u` on your `PATH` as `s2udio`, copy
`scripts/s2u-helper` next to it, seed the config, point MPD at your music,
start the app — and on the current version run `s2udio daemon autostart on`
so the daemon owns the player across logins (section 5).

---

## 1. Packages

| Distro family | Command (your package manager) |
| --- | --- |
| Arch / CachyOS / Artix | `sudo pacman -S --needed mpd ffmpeg cava yt-dlp wl-clipboard`<br>AUR: `mpdris2-git` (MPRIS bridge), `mpv-full` (recommended video build) or `mpv` |
| Fedora | enable RPM Fusion free, then `sudo dnf install mpd cava yt-dlp mpv ffmpeg wl-clipboard python3-dbus python3-gobject python3-mutagen gcc make git curl`<br>MPRIS: `mpdris2` |
| Debian / Ubuntu / Devuan | `sudo apt install mpd mpdris2 cava yt-dlp mpv ffmpeg wl-clipboard python3-dbus python3-gi python3-mutagen build-essential git curl` |
| Alpine | `sudo apk add mpd mpv yt-dlp ffmpeg wl-clipboard python3 py3-dbus py3-gobject3 py3-mutagen py3-pip build-base git curl fftw-dev iniparser-dev ncurses-dev sdl2-dev autoconf automake libtool ncurses-terminfo-base`<br>**cava is not packaged**: build it from source into `/opt/s2udio/bin` |
| Void | `sudo xbps-install -S mpd mpv yt-dlp cava ffmpeg wl-clipboard mpDris2 python3 python3-dbus python3-gobject python3-mutagen python3-mpd2 base-devel cargo rust git curl util-linux procps-ng ncurses-term runit` |
| NixOS | use the flake instead: `nix profile install .#s2udio .#bridgePython` plus `nixpkgs#mpd nixpkgs#mpv nixpkgs#yt-dlp nixpkgs#cava nixpkgs#mpdris2 nixpkgs#ffmpeg nixpkgs#tmux` |

Notes:

- **MPRIS for audio** needs the official **mpDris2** (distro package). s2udio
  runs it through its own shim (`s2u-helper mpdris2`) so stream thumbnails and
  notification toggles work. If your distro has no package (Alpine) or ships a
  compiled ELF that cannot be shimmed (NixOS), download the upstream **python**
  source to `/opt/s2udio/bin/mpDris2` and install `python-mpd2` into
  `/opt/s2udio/lib/python` (`pip install --target … python-mpd2`); the shim
  looks there automatically (`S2UDIO_OPT_PREFIX` relocates that prefix).
- **cava** is only needed for the visualizer. `S2UDIO_CAVA_BIN` points the app
  at a custom binary.
- **wl-clipboard** (`wl-paste`) is needed for Ctrl+V on Wayland; X11 tools
  (`xclip`/`xsel`) are the fallbacks.
- **mpv**: any current mpv works. The Arch `mpv-full` build is recommended for
  the video pipeline; on other distros install plain `mpv` (the app prints a
  note).
- **SVP4** is optional; its bundled mpv is configured in the app settings.

## 2. Rust toolchain

The project is edition 2024 and needs **Rust 1.88 or newer**. Distro packages
are often older; install a current toolchain with rustup if `rustc --version`
is below 1.88:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
```

## 3. Build and install the binary

```sh
git clone https://github.com/NJMRgit/s2udio.git
cd s2udio                     # current version: 4b2a5ff or later
cargo build --release
install -Dm755 target/release/s2u ~/.local/bin/s2udio
```

The Cargo artifact is named **`s2u`**; the installed name is **`s2udio`**.
Make sure `~/.local/bin` is on your `PATH`.

```sh
s2udio version                # e.g. s2udio 0.11.0 git 4b2a5ff
```

The daemon is present in every build from `4b2a5ff` (2026-10-30) onward. An
older commit builds without it — see the old-version section before going
further.

## 4. Support programs

One helper executable contains every sidecar (tracker, mpv MPRIS bridge,
mpDris2 shim, service abstraction, bgutil renew):

```sh
install -Dm755 scripts/s2u-helper ~/.local/bin/s2u-helper
install -Dm755 scripts/rmpc-fetch-lyrics ~/.local/bin/rmpc-fetch-lyrics
```

Optional but recommended on systems with cava `node_name` support: the
PipeWire node-name shim.

```sh
mkdir -p ~/.local/share/s2udio
cc -shared -fPIC -o ~/.local/share/s2udio/libcavaname.so scripts/cava-node-name.c -ldl
```

Optional companion tools:

- `s2u-yt/` — the yt-dlp wrapper/PO-token setup (needs its own install step;
  follow `s2u-yt/README` if present). Without it the app still resolves with
  the plain `yt-dlp` binary.
- `s2u-mpdris/` — the standalone video MPRIS bridge; the `s2u-helper mpris`
  subcommand is the embedded equivalent and is enough.

## 5. The always-on daemon (current version — start it now)

The current version moves the stream buffer proxy, the web-stream caretaker
and the QAT controller into a detached per-user daemon. The TUI starts it on
demand, but starting it explicitly and enabling login autostart is what makes
the current version behave as documented:

```sh
s2udio daemon start         # idempotent; the TUI also does this on demand
s2udio daemon status        # pid, proxy port, MPD, attached TUI, hotkey
s2udio daemon autostart on  # install + enable the systemd user unit
```

What the daemon gives you:

- the stream buffer proxy survives the TUI and a destroyed QAT, so a web row
  keeps playing and the queue's next web streams are resolved/preloaded while
  no TUI is attached (the TUI's own look-ahead is authoritative while it is);
- `s2udio remote raise`, a media widget's **Raise** (MPRIS), and
  `s2udio daemon qat raise` relaunch the QAT with a fresh TUI when the
  compositor destroyed it — no live TUI socket needed;
- the QAT toggle chord is owned by the daemon (KWin script; default
  **Meta+T**). Change it in **Settings > Advanced > daemon > qat hotkey** or
  with `s2udio daemon hotkey <chord>`; it persists to
  `~/.config/s2udio/daemon.ron` and is applied to the running daemon live
  (KWin script reload + KGlobalAccel re-registration). `S2U_QAT_HOTKEY` is a
  startup override. Remove an old `net.local.kitten.desktop` shortcut on the
  same chord; two bindings fight. See section 10 for the kitty side of the
  QAT.

`daemon autostart on` writes and enables this systemd **user** unit (the
daemon detaches, so the unit is a oneshot around the idempotent CLI):

```ini
# ~/.config/systemd/user/s2udio-daemon.service
[Unit]
Description=s2udio always-on daemon (stream proxy, web-stream caretaker, QAT)
PartOf=graphical-session.target
After=graphical-session.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart="%h/.local/bin/s2udio" daemon start
ExecStop="%h/.local/bin/s2udio" daemon stop

[Install]
WantedBy=graphical-session.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now s2udio-daemon.service
```

The same switch from the TUI is **Settings > Advanced > startup > start daemon
at login**. Turning it off removes the login start but deliberately leaves a
running daemon alone (`s2udio daemon stop` stops it). The unit needs the
graphical session environment (Wayland/D-Bus for the QAT and hotkey); Plasma
imports it into the systemd user session. On a non-systemd session, run
`s2udio daemon start` from your login autostart instead (the
`daemon autostart` command itself needs systemd).

If MPD is not configured yet, the daemon retries its connection; finish
sections 6–7 and `s2udio daemon status` should show it connected.

## 6. Configuration

```sh
mkdir -p ~/.config/s2udio/themes
cp assets/example_config.ron ~/.config/s2udio/config.ron
cp assets/example_theme.ron ~/.config/s2udio/themes/default.ron
```

Everything lives in `~/.config/s2udio/` (the `rmpc` paths are read-only
legacy fallbacks). `assets/example_config.ron` documents every option; the
Settings panel edits the common ones (and persists them in `state.ron`).

mpv needs its IPC socket for the video pipeline:

```sh
mkdir -p ~/.config/mpv
printf 'input-ipc-server=/tmp/mpvsocket\n' >> ~/.config/mpv/mpv.conf
```

Smart playlists, key remaps, cava settings and Jellyfin credentials are
sidecars in the same directory, all managed by the app.

## 7. MPD

s2udio is an MPD client; MPD must be reachable (default `127.0.0.1:6600`).
A small **user-level** MPD is the least surprising setup (it puts the server
in your session and avoids fighting the distro's system service):

```sh
mkdir -p ~/.config/mpd/playlists
cat > ~/.config/mpd/mpd.conf <<'EOF'
music_directory         "~/Music"
playlist_directory      "~/.config/mpd/playlists"
db_file                 "~/.cache/mpd/db"
state_file              "~/.config/mpd/state"
bind_to_address         "127.0.0.1"
port                    "6600"
audio_output {
    type "pipewire"
    name "PipeWire"
}
EOF
```

On Debian/Ubuntu the packaged **system** MPD is auto-started on 6600; stop
and disable it before running a user instance:

```sh
sudo systemctl disable --now mpd
```

Start MPD as your user (`mpd --no-daemon ~/.config/mpd/mpd.conf`, or a user
service). Check it with `mpc status`; the app's `Settings > MPD` also has
update/rescan actions. Once MPD is up, `s2udio daemon status` (section 5)
should report it connected.

## 8. Services and desktop integration

The `s2u-helper svc` subcommand abstracts systemd-user, runit, s6, openrc and
a plain-launcher backend:

```sh
s2u-helper svc start mpd
s2u-helper svc start mpDris2
```

On systemd, a user unit for the audio bridge (the packaged unit can be
overridden too):

```ini
# ~/.config/systemd/user/mpDris2.service
[Unit]
Description=MPRIS bridge for MPD
After=mpd.service

[Service]
ExecStart=%h/.local/bin/s2u-helper mpdris2 --use-journal
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now mpDris2.service
```

A desktop entry is optional; `s2udio.desktop` in
`~/.local/share/applications/` can run `s2udio` in your terminal of choice.
The torrent engine's **S2RQ** entry (`s2udio rq start`) is written by the
script only when rqbit is installed; copy its `Exec=` line from
`setup.sh` if you want it.

## 9. Optional features

- **YouTube / SoundCloud / NicoVideo**: install `yt-dlp`; resolution is
  per-play. The `s2u-yt` wrapper adds PO tokens for YouTube when deployed.
- **Jellyfin**: configure the server in Settings > Jellyfin (or the sidecar);
  video plays through mpv.
- **Radio**: no extra service; the directory is `radio-browser.info`.
- **Torrents**: install `rqbit`, then `s2udio rq start` (or let the app start
  the engine). The persistent downloader is `s2udio dl start`.
- **Cava**: install `cava`; enable it in Settings.
- **Lyrics**: the local `.lrc` library lives in `~/.config/s2udio/lyrics/`;
  `rmpc-fetch-lyrics` (installed above) handles network fetches.
- **SVP4**: enable `Settings > mpv > svp support`; the app then launches
  SVP4's bundled mpv.

## 10. Kitty quick access terminal and raising

If you run s2udio in kitty's **quick-access-terminal**, raising the player
back into view uses kitty's own remote control. Add these two overrides to
`~/.config/kitty/quick-access-terminal.conf` (create the file if needed;
`setup.sh` offers to do this automatically):

```conf
kitty_override allow_remote_control=socket-only
kitty_override listen_on=unix:@s2udio-qat
```

Notes:

- The QAT is a layer-shell panel; while it is hidden its surface is unmapped,
  and no window manager can show it again. `s2udio` asks kitty to
  `resize-os-window --action=show` over the socket above.
- On the current version the daemon owns the QAT toggle: it registers
  **Meta+T** through KWin (changeable in Settings > Advanced > daemon or with
  `s2udio daemon hotkey`, section 5), so you do **not** need a desktop-file
  shortcut for the panel. If you keep a custom shortcut bound to `kitten
  quick-access-terminal`, give it a different chord — or remove it.
- Raising works both ways: with a live TUI the TUI maps/focuses its own
  panel; with no TUI (destroyed QAT, screen blank) the daemon launches a fresh
  one (`s2udio remote raise` / MPRIS `Raise` / `s2udio daemon qat raise`).
- The exact lines are what the app prints when raising fails with remote
  control disabled.

## 11. Verify

```sh
s2udio version                     # 0.11.0 git 4b2a5ff (or later)
s2udio debug-info                  # resolves the optional dependencies
s2udio daemon status               # daemon up, proxy port, MPD, attached TUI
s2udio daemon autostart status     # enabled/disabled + the unit path
s2udio daemon hotkey Meta+T        # persist/apply the QAT chord (Advanced row)
mpc status                         # MPD is reachable
s2udio                             # the TUI starts and connects
qdbus6 org.mpris.MediaPlayer2.mpd  # audio MPRIS (after mpDris2 starts)
s2udio remote raise                # while the TUI is live
```

Then close the QAT (or let the screen blank) and run

```sh
s2udio remote raise                # the daemon relaunches the QAT + a fresh TUI
```

Logs: `~/.local/state/s2udio/s2udio_<uid>.log` (app) and
`~/.local/state/s2udio/s2udio_daemon_<uid>*.log` (daemon).

## 12. Updating and uninstalling

Update:

```sh
git pull
cargo build --release
install -Dm755 target/release/s2u ~/.local/bin/s2udio
install -Dm755 scripts/s2u-helper ~/.local/bin/s2u-helper
s2udio daemon autostart status    # the unit's ExecStart points at the binary
```

Uninstall: stop the app and the daemon, disable autostart
(`s2udio daemon autostart off`), then remove the binary and helper from
`~/.local/bin/`, the `mpDris2` user unit, the S2RQ entry, and (optionally)
`~/.config/s2udio/` and `~/.cache/s2udio/`. MPD and your music library are
untouched.

## Old versions (without the daemon)

**These instructions are for s2udio 0.11.0 git `9e3b493` (2026-10-29) and
earlier** — the last master before the always-on daemon landed in `4b2a5ff`
(2026-10-30). The version number is still 0.11.0; the git describe/date is
what tells the builds apart.

```sh
git clone https://github.com/NJMRgit/s2udio.git
cd s2udio
git checkout 9e3b493            # or an older tag/commit
cargo build --release
install -Dm755 target/release/s2u ~/.local/bin/s2udio
```

Sections 4 (support programs), 6 (configuration), 7 (MPD), 8 (services),
9 (optional features) and 10 (kitty overrides) apply unchanged. Section 5
does **not**: there is no `s2udio daemon` command in these builds, so there is
nothing to start and no `daemon autostart`.

What is different without the daemon:

- **The TUI hosts the stream buffer proxy itself.** Closing the TUI kills the
  proxy, so a web stream still being served ends with it, and the next web
  queue row cannot be resolved while no TUI is open. (The detached mpv session
  and the tracker still cover video, as before.)
- **Raising needs a live TUI.** `s2udio remote raise` and a media widget's
  **Raise** work only while the TUI is running. If the QAT was destroyed
  (screen blank removes the compositor output), there is no daemon to relaunch
  it, and `CanRaise` is simply "a TUI is running".
- **The QAT toggle is your own shortcut.** These builds have no KWin/QAT
  controller: bind `kitten quick-access-terminal` to a shortcut yourself, for
  example KDE System Settings → Shortcuts → Add Command. The two kitty
  `kitty_override` lines are still required for raising a hidden QAT.
- There is no `Meta+T` action owned by s2udio; a leftover
  `net.local.kitten.desktop` shortcut is the only toggle in these versions.

## Troubleshooting

- **No album art**: the kitty graphics protocol is the only full-fidelity art
  path; other terminals fall back to a color-block approximation. Also check
  `Settings > General > album art`.
- **Cava is silent**: PipeWire must expose the default sink; `cava` must be
  installed; the app's cava section must be enabled.
- **MPRIS entry missing**: `mpDris2` (audio) or `s2u-helper mpris` (video)
  must run in the same session bus; check `systemctl --user status
  mpDris2.service` and the `s2u-helper svc is-active mpDris2` output.
- **YouTube fails to resolve**: update `yt-dlp`; on YouTube, deploy the
  `s2u-yt` wrapper for PO tokens; see the app's log for the yt-dlp error.
- **Raising does nothing**: the QAT needs the two kitty overrides in section
  10, and the daemon (current version) must be running — `s2udio daemon
  status`. On a pre-`4b2a5ff` build there is no daemon; raising needs a live
  TUI by design.
- **The app cannot connect to MPD**: check the address in
  `~/.config/s2udio/config.ron` and the MPD service; permissions on the music
  directory and the MPD cache/db directory are the usual culprits.
- **Daemon does not start / autostart does not stick**: check
  `s2udio daemon status`, `systemctl --user status s2udio-daemon.service`,
  and the daemon log; a session without a systemd user manager has no unit,
  so use a login autostart entry that runs `s2udio daemon start`.

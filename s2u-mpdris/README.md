# s2u-mpdris

A standalone **MPRIS2 media-player daemon** for mpv video sessions that
s2udio (or any mpv user) runs: it registers `org.mpris.MediaPlayer2.s2u-mpv`
on the D-Bus session bus, serves the playing video as MPRIS2 properties
(title, artist, thumbnail, length, position, volume), and forwards
transport controls to the running mpv through its IPC socket.

Python stdlib only — the D-Bus session bus is spoken at the wire level
(EXTERNAL auth, marshalling, signals), so there is **no python-dbus /
gi / dbus-next dependency**, and no patched mpDris2 is needed for video.

## Why it exists

s2udio plays video (Jellyfin / YouTube / local files) through mpv and
mirrors the session to `~/.cache/rmpc/mpv-mpris.json` (title, artist,
poster path, position, duration, mpv IPC socket, serialized playlist) on
its ~500 ms poll — and the `s2u-mpv-tracker` caretaker daemon keeps that
file fresh when s2udio closes while a video plays. The poster itself is
written to `~/.cache/rmpc/mpris-mpv-art`.

This daemon is the MPRIS side of that bridge. It is **separate and
complementary**: it only *reads* those state files and talks to mpv and
the desktop bus — nothing inside s2udio needs to change at runtime.

## What it does

- Owns `org.mpris.MediaPlayer2.s2u-mpv` on the session bus (falls back
  to `S2U_MPRIS_NAME`).
- Serves `PlaybackStatus`, `Metadata` (`xesam:title`, `xesam:artist`,
  `mpris:trackid`, `mpris:length`, `xesam:url`), `Position`, `Volume`,
  and the standard capability flags.
- Thumbnail: `mpris:artUrl` is a **cache-busted** `file://…?t=<mtime_ns>`
  URL, so MPRIS clients that cache art by URL (e.g. KDE's media widget)
  never show the previous video's image.
- Forwards transport controls to mpv via its IPC socket: Play, Pause,
  PlayPause, Stop, Next, Previous, Seek, SetPosition, OpenUri, Quit, and
  Volume.
- Emits `PropertiesChanged` and `Seeked` signals.
- Exits by itself when the state file goes stale or is deleted (mpv
  stopped / s2udio session ended); a pid file keeps it single-instance.

## Install

```sh
install -Dm755 s2u-mpdris ~/.local/bin/s2u-mpdris
```

s2udio's `setup.sh` does this automatically when it finds the project
sibling to the s2udio checkout (or `S2U_MPRIS_DIR`), and s2udio spawns
the daemon next to the tracker on every mpv launch and reattach.

## Run

```sh
s2u-mpdris           # needs a D-Bus session bus + a live mpv state file
```

Env overrides (also used by tests):

| Variable | Meaning | Default |
| --- | --- | --- |
| `S2U_CACHE_DIR` | cache dir for `mpv-mpris.json` / `mpris-mpv-art` / the pid file | `~/.cache/rmpc` |
| `S2U_MPRIS_BUS` | session bus address | `$DBUS_SESSION_BUS_ADDRESS` |
| `S2U_MPRIS_NAME` | D-Bus name to own | `org.mpris.MediaPlayer2.s2u-mpv` |
| `S2U_MPRIS_POLL_S` | state poll interval (s) | `0.5` |
| `S2U_MPRIS_STALE_S` | state freshness limit (s) | `15` |
| `S2U_MPRIS_PIDFILE` | pid file path | `<cache>/mpris.pid` |

## Tests

`tests/test_s2u_mpris.py` runs the real daemon against a fake D-Bus
session bus (an *independent* wire implementation, so alignment/signature
bugs in the daemon are caught) plus a fake mpv IPC server, and asserts
name acquisition, properties, signals, error paths, and the exact mpv
commands forwarded. No real bus or mpv needed:

```sh
python3 tests/test_s2u_mpris.py   # 21 tests
```

## Layout

- `s2u-mpdris` — the daemon (single file, Python stdlib only)
- `tests/test_s2u_mpris.py` — integration test suite

---
title: "MPRIS & Art — Validation Plan"
section: validation
doc_type: plan
id: "validation/mpris"
description: >
  The step-by-step validation plan for the MPRIS surface (desktop media
  controls + the TUI album art pane / info box): timeline, art and track
  info across every playback source. The coupling map explains why MPRIS
  fixes "fix one thing then break another" and which scenario rows a given
  change must re-run.
status: "current"
updated: "2026-08-07"
source_files:
  - scripts/s2u-mpdris2
  - scripts/s2udio-mpris
  - scripts/s2u-mpv-tracker
  - src/ui/modals/paste.rs (ensure_mpris_metadata / write_mpv_mpris_state)
  - src/core/work.rs (SaveMprisArt / SaveMpvMprisArt, expected-source guard)
  - src/ui/panes/album_art.rs
  - src/config/theme/mod.rs (default album art)
  - assets/ (default.jpg wired as the fallback art; the radio/music/video placeholder jpgs were removed in round 23 — the app shows `default.jpg`/no-art for those sources)
related:
  - backend/mpd-playback
  - backend/mpv-session
  - backend/image-overlays
  - backend/ytdlp-resolution
  - backend/stream-downloads
tags: [validation, mpris, album-art, regression]
---

# MPRIS & Art — Validation Plan

## 1. Purpose & definition of done

MPRIS is the most regressed surface in s2udio: the audio and video paths
share state files, caches and two bridge scripts, so a fix to one
scenario regularly breaks another. This plan makes the surface
validatable as a whole. **The goal:** for *whatever* is playing — music,
audio stream, saved/downloaded stream, local video, YouTube video,
Jellyfin video or radio — the desktop media controls **and** the TUI
album art pane / info box reliably show the **timeline**, the **art** and
the **track info / video title**.

**Definition of done (every scenario in §5 passes):**

1. **Timeline** — `mpris:length` matches the real duration (nonzero for
   finite media), `Position` advances with playback, `Seek`/`SetPosition`
   work for seekable media and **no-op without killing the daemon** for
   unseekable streams (HLS / live).
2. **Art** — `mpris:artUrl` (media controls) and the TUI album art pane
   show the *current* source's art: real cover/thumbnail when it exists,
   the right placeholder when it doesn't, and **never another source's
   stale art**.
3. **Track info / title** — `xesam:title` / `xesam:artist` (media
   controls) and the TUI info box show the real title and artist —
   never a URL basename, `index.m3u8`, or the previous track's title.
4. **Timing budget** — art/title land within ~10 s of a track/video
   start (audio art: shim retry 1.5 s × 6; video: 500 ms poll); a
   stopped player leaves no MPRIS entry beyond the bridge's stale budget
   (state file gone/stale > 10 s → bridge exits within ~30 s).

## 2. System under test — the two surfaces

| Playback | MPRIS owner | Bus name | Art file | Metadata source |
| --- | --- | --- | --- | --- |
| Audio via MPD (library, YouTube-as-audio, saved stream, radio) | official mpDris2 run through the **s2u-mpdris2** shim | `org.mpris.MediaPlayer2.mpd` | `~/.cache/rmpc/mpris-art` (shim serves it for non-file URLs) | MPD song tags (s2udio re-tags streams via `cleartagid`/`addtagid`); shim injects title/artist/duration from `yt-info.json` |
| Video via mpv (local, YouTube ytdl, Jellyfin) | bundled **s2udio-mpris** daemon (spawned by the tracker) | `org.mpris.MediaPlayer2.s2udio` | `~/.cache/rmpc/mpris-mpv-art` | `~/.cache/rmpc/mpv-mpris.json` (500 ms poll by s2udio, or the tracker caretaker when s2udio is closed) |

The **TUI album art pane** (kitty image overlay) and the **info box**
(title/channel/description, video-style for streams) follow the same
sources — `current_yt_info` / `mpv_yt_info` / MPD art lookup — and must
be checked alongside the D-Bus side.

## 3. The coupling map (why fixes break each other)

Every row is shared state or a shared script. **Changing anything in a
row requires re-running every scenario that reads it** (§5 columns mark
who reads what).

| Artifact | Written by | Read by | Scenarios affected |
| --- | --- | --- | --- |
| `~/.cache/rmpc/mpris-art` | s2udio work thread (`SaveMprisArt`, **expected-source guarded**) | `s2u-mpdris2` `find_cover()` (cache-busted `?t=<mtime_ns>`) | B, C, G (negative), A (negative) |
| `~/.cache/rmpc/mpris-mpv-art` | s2udio `SaveMpvMprisArt`; tracker `jf_write_poster` (caretaker); cleared on entry change / session end | `s2udio-mpris` (`mpris:artUrl`) | D, E, F |
| `~/.cache/rmpc/mpv-mpris.json` | s2udio 500 ms poll; tracker caretaker | `s2udio-mpris` (exits when stale > 10 s) | D, E, F |
| `~/.cache/rmpc/yt-info.json` | s2udio on resolve (keyed by stream URL + `original_url`) | shim `update_metadata`, tracker, TUI info box / art pane | B, C, E |
| MPD queue tags (`cleartagid`/`addtagid`) | `ensure_mpris_metadata` (incl. the `metadata_processed_song` queue catch-up) | official mpDris2 | B, C, G |
| `video-playlist.json` (`original_url` persistence) | `save_video_playlist` | s2udio reattach; playlist-entry title fallback | C, E, F |
| `s2u-mpdris2` (shim: `find_cover`, `update_metadata`, `seekid`, guarded Seek/SetPosition) | repo script → `~/.local/bin/s2u-mpdris2` via `mpDris2.service.d/s2udio.conf` | mpDris2.service | A, B, C, G |
| `s2udio-mpris` (bridge) | repo script → `~/.local/bin/s2udio-mpris` | spawned by `s2u-mpv-tracker` | D, E, F |
| `s2u-mpv-tracker` (caretaker: state writes, poster, mutual exclusion) | repo script → `~/.local/bin/s2u-mpv-tracker` | spawned with mpv | D, E, F (s2udio closed) |
| `assets/*.jpg` placeholders | repo assets — **only `default.jpg` is wired** (`theme.default_album_art`) | theme → art pane `show_default()` | G, H (expected behavior; currently a gap) |

**Baseline rule:** a MPRIS round must start with a byte-identical install
check (`md5sum` repo script vs `~/.local/bin`, binary vs running process)
so a stale binary/script is never validated as "the fix".

## 4. Automated gates (run first, before any live step)

```bash
cargo test --release          # expect 1185/1185 (round-3 baseline)
python3 s2u-mpdris/tests/test_s2u_mpdris.py            # expect 21/21
python3 tests/tracker/test_tracker.py                  # expect 6/6
python3 tests/mpdris_shim/test_s2u_mpdris2_shim.py     # expect 1/1
python3 -m py_compile scripts/s2u-mpdris2 scripts/s2udio-mpris scripts/s2u-mpv-tracker
```

Then confirm the deployed state matches the tree (all three must agree):

```bash
md5sum ~/.local/bin/s2u-mpdris2 ~/Projects/s2udio/scripts/s2u-mpdris2
md5sum ~/.local/bin/s2u-mpv-tracker ~/Projects/s2udio/scripts/s2u-mpv-tracker
md5sum ~/.local/bin/s2udio-mpris ~/Projects/s2udio/scripts/s2udio-mpris
md5sum /proc/$(pgrep -x s2udio | head -1)/exe ~/.local/bin/s2udio   # running == installed
systemctl --user status mpDris2.service --no-pager | head -3         # active, drop-in loaded
```

If the binary is stale, **build before installing** (`cargo build --release`
then install `target/release/s2u` → `~/.local/bin/s2udio` — atomic rename if
"Text file busy") and restart s2udio with the user's coordination.

## 5. Scenario matrix — the validation steps

**D-Bus probe recipe** (used by every scenario; pick the bus name):

```bash
BUS=org.mpris.MediaPlayer2.mpd        # audio (mpDris2) — or:
BUS=org.mpris.MediaPlayer2.s2udio     # video (s2udio-mpris)
OBJ=/org/mpris/MediaPlayer2
P=org.mpris.MediaPlayer2.Player

busctl --user get-property $BUS $OBJ $P Metadata        # title/artist/length/artUrl/trackid/url
busctl --user get-property $BUS $OBJ $P PlaybackStatus  # Playing/Paused/Stopped
busctl --user get-property $BUS $OBJ $P Position        # µs; must advance between reads
busctl --user get-property $BUS $OBJ $P Volume          # 0..1; Set(Volume) must reach the player
busctl --user call $BUS $OBJ $P Seek x 30000000         # +30 s (int64 signature 'x')
busctl --user call $BUS $OBJ $P Seek x -30000000        # −30 s
busctl --user call $BUS $OBJ $P SetPosition ox "$TRACKID" 30000000   # TRACKID from Metadata
pgrep -ax mpDris2 / s2udio-mpris     # daemon must SURVIVE every seek call

> **Every property must answer via `Properties.Get`** (what KDE's media
> controls actually use), not just as a direct method: `gdbus call --dest
> $BUS --object-path $OBJ --method org.freedesktop.DBus.Properties.Get
> $P Position` etc. A missing key returns None → dbus-python replies a
> TypeError the client can't parse (the s2udio bridge had exactly this
> bug for Position/Volume — the panel then showed no info at all).
> busctl `get-property` is equivalent (it sends Properties.Get).
```

**Queue hygiene** (before any test that plays something):
`mpc save .s2u-backup`; after: `mpc clear && mpc load .s2u-backup &&
mpc rm .s2u-backup && mpc stop`.

### A — Music from the MPD library

Setup: play a library track with embedded/adjacent cover (e.g. an album
track, not a stream).

- **Timeline**: `mpris:length` = real duration (µs, nonzero); `Position`
  advances between two reads ~5 s apart; `Seek`/`SetPosition` jump and
  MPD's `mpc status` elapsed follows; daemon survives.
- **Art**: `mpris:artUrl` present and resolves (strip `?t=`; `file
  ~/.cache/…`-style check that the target is a real image — mpDris2's own
  `find_cover` serves it); TUI album art pane shows the cover.
- **Track info**: `xesam:title`/`xesam:artist` = the album track's real
  tags (Bones — FrozenGauntlet, etc.), `xesam:url` = the file path.
- **Negative (stale-art)**: `~/.cache/rmpc/mpris-art` must **not** exist
  while a plain library file plays (it is only for stream URLs; a leftover
  file here would show wrong art).

### B — Online stream audio via MPD (YouTube)

Setup: paste a YouTube link → `[Audio]` play → resolves to a stream URL
(single-file DASH videoplayback via android_vr by default; HLS
`…/index.m3u8` only through the web_safari fallback — see
`backend/ytdlp-resolution`); MPD plays it.

- **Timeline**: `mpris:length` = the **injected** duration from
  `yt-info.json` (nonzero Int64 — check it is not an Int32 overflow for
  long videos); `Position` advances; `Seek`/`SetPosition` → MPD
  **range-seeks** the single-file DASH URL (android_vr — `mpc status`
  elapsed follows); only an HLS-fallback stream is a seek no-op (MPD
  cannot seek HLS — journal: `Seek rejected by MPD (stream not
  seekable)`), daemon survives either way.
- **Art**: `mpris:artUrl` = `file://…/mpris-art?t=<mtime_ns>`; the file is
  a JPEG thumbnail; `stat` mtime changes when the track changes (the
  cache-buster makes KDE re-fetch). **Timing budget**: art lands within
  ~10 s of the song change (async download + shim retry probe). TUI art
  pane shows the thumbnail.
- **Track info**: `xesam:title` = the video title (never `index.m3u8` or
  the googlevideo URL), `xesam:artist` = channel; TUI info box shows
  title/channel/description/chapters.

### C — Saved online audio stream from YouTube

Two sub-cases — both are "saved" content:

**C1 — saved playlist entry (stream URL in a playlist).** Setup: play the
saved entry from the Playlists tab. If the resolved URL expired,
ReplaceAndPlay re-resolves (`original_url` lookup) — **expect tags + art
- TUI art immediately, no `mpc stop && mpc play` needed** (the
`metadata_processed_song` queue-update catch-up fired).

- Same timeline/art/info checks as B.
- Identity round-trip: `original_url` survives the playlist
  (`video-playlist.json`/`mpv-mpris.json` for video; the yt-info cache
  alias for audio). **Restart s2udio and play the saved entry again** —
  title/art must still resolve (cache keyed by canonical link).

**C2 — downloaded file (`.s2udioDownloads`).** Setup: on a stream row,
`⤓ Save as audio` → the file lands in `~/.s2udioDownloads` (shown as
`Downloads` at the top of the MPD library); play the downloaded file.

- **Preservation**: the file's embedded tags carry the video's
  title/artist/album/art (yt-dlp `--embed-thumbnail --embed-metadata
  --convert-thumbnails jpg`) — verify with `ffprobe`/`metaflac` on the
  file (title, artist, album, attached picture, description where the
  format supports it).
- **Timeline**: local file → MPD **can** seek; full seek/position checks
  pass (no shim no-op path).
- **Art**: mpDris2's own `find_cover` reads the embedded art (local
  file — not the shim path); `mpris:artUrl` resolves to the file/embedded
  cover; TUI art pane shows it.
- **Track info**: `xesam:title`/`xesam:artist` come from the embedded
  tags — the real title, not the filename.

### D — Local video file

Setup: paste a local video path → `[Video] Play` (mpv).

- **Timeline**: `mpris:length` = mpv duration; `Position` advances;
  `Seek`/`SetPosition` → verify against mpv itself:
  `echo '{"command":["get_property","time-pos"]}' | socat -
  /tmp/mpvsocket` (the fixed IPC socket s2udio launches mpv with)
  reflects the jump; daemon survives.
- **Art**: no art source → `mpris:artUrl` absent, `mpris-mpv-art` must
  **not linger** from a previous video (cleared on session start / entry
  change); TUI art pane shows the **video placeholder** (expected:
  `assets/video.jpg` — currently `default.jpg`, see §6).
- **Track info**: `xesam:title` = mpv `media-title` (filename or embedded
  title — never a raw path if a title exists); no artist.

### E — Video from an online stream (ytdl)

Setup: paste a YouTube link → `[Video] Play` → yt-dlp resolves → mpv plays.

- **Timeline**: `mpris:length` = real duration; `Position` advances; seek
  works via mpv IPC (socat check as in D). **Long-video trap**: play a
  video > 35 min (e.g. the 1107 s CSGO entry) — `mpris:length` must be a
  full int64, not an Int32 overflow (~2147 s).
- **Art**: `mpris:artUrl` = `file://…/mpris-mpv-art`; the thumbnail is
  fetched via `SaveMpvMprisArt`; TUI art pane shows the thumbnail; on
  entry change the poster file is cleared first (no previous video's
  thumbnail).
- **Track info**: `xesam:title` = the real video title (not the
  provisional googlevideo URL — the poll only adopts non-provisional
  titles; cached yt-info / playlist entry title wins); `xesam:artist` =
  channel; TUI info box title/channel/description/chapters.

### F — Video from Jellyfin

Setup: Jellyfin tab → play a movie/episode → mpv.

- **Timeline**: as E (length from mpv, seek via IPC; Jellyfin resume
  position reported — validate `Playing/Progress` reaches the server or
  appears in `mpv-tracker.log`).
- **Art**: `mpris:artUrl` = `file://…/mpris-mpv-art` — the Jellyfin
  **primary image** (fetched by the app; caretaker fetches it too when
  s2udio is closed). TUI art pane = poster.
- **Track info**: `xesam:title` = item name, `xesam:artist` = series /
  album artist; TUI info box shows the show/movie title.
- **Caretaker / reattach**: while the video plays, close s2udio → the
  tracker caretaker keeps `mpv-mpris.json` fresh and the video stays on
  the media controls; reopen s2udio → reattach restores title/art/controls.

### G — Radio stream

Setup: Radio tab → play a station (temp entry; never shows in the queue).

- **Art (expected behavior)**: TUI album art pane shows the **radio
  placeholder** (`assets/radio.jpg`). **Currently a gap** — see §6: the
  pane falls back to `default.jpg` because no source-specific placeholder
  is wired.
- **Stale-art (must pass today)**: `~/.cache/rmpc/mpris-art` is removed
  (unrecognized stream branch of `ensure_mpris_metadata`) — the previous
  YouTube track's thumbnail must not linger in the media controls.
- **Timeline**: live stream → no seekable timeline; `Seek`/`SetPosition`
  no-op and the mpDris2 daemon survives (the shim's KeyError guard for
  statuses without a `time` field).
- **Track info**: `xesam:title` = station name (from the playlist
  EXTINF / MPD tags).

### H — Video / audio files with no art

Setup: play a local audio file with no embedded/adjacent cover; play a
local video with no poster.

- **Art (expected behavior)**: audio → **music placeholder**
  (`assets/music.jpg`); video → **video placeholder** (`assets/video.jpg`)
  in the TUI art pane. **Currently a gap** — both show `default.jpg`
  today (§6).
- **Timeline / track info**: normal local-file checks (timeline real and
  seekable; tags from file metadata or mpv media-title).

### I — Video session hides the paused MPD audio MPRIS

Setup: play any video (D–F) with MPD paused on a track/stream.

- **While the video plays**: `org.mpris.MediaPlayer2.mpd` is **absent**
  from the bus (mpDris2.service stopped by the tracker — probe:
  `busctl --user list | grep mpris` shows only
  `org.mpris.MediaPlayer2.s2udio`; `systemctl --user is-active
  mpDris2.service` = `inactive`). The media controls show only the video
  (title/poster/seek control it).
- **When MPD wins the mutual exclusion** (audio plays while the video is
  paused): mpDris2 **starts** (the tracker follows the winner every
  tick) — `org.mpris.MediaPlayer2.mpd` is owned again and the media
  controls show the MPD audio, not the paused video. Resuming the video
  (MPD pauses) stops mpDris2 again.
- **When the video ends**: within ~5 s the tracker exits and restarts
  mpDris2 — `org.mpris.MediaPlayer2.mpd` is owned again and shows the
  resumed MPD playback (not stuck hidden).
- **Caretaker**: close s2udio while the video plays → mpDris2 stays
  stopped (only s2udio on the bus); end the video → mpDris2 returns.
- **Audio-only regressions**: a plain audio session (A/B/C/G) must never
  stop mpDris2 — the control follows the active source, and with no mpv
  session the tracker is not running.
- **Art across entries**: `mpris:artUrl` is mtime cache-busted
  (`?t=<mtime_ns>`) — a YouTube → Jellyfin transition must show the new
  poster immediately (KDE caches art by URL).

## 6. Known gaps / expected-fail notes (report, don't patch around)

- **Placeholders are assets-only.** `assets/radio.jpg`, `music.jpg`,
  `video.jpg` (all 1024×1024, tracked) are **not referenced by any
  code** — the theme embeds only `assets/default.jpg`
  (`default_album_art`), and the art pane's fallback is always that one
  image. G and H's placeholder expectations currently **fail by design**;
  the validator records them as findings (feature gap), never as a
  regression, and the container agent picks them up from the feedback
  round.
- **Notification burst** (`ExcessNotificationGeneration` during
  stream-state churn) is filed as a non-issue — cosmetic only.
- Stray `~/NJMRgit/s2udio/~` dir (misdirected home) — trash when
  convenient; unrelated to MPRIS.

## 7. The full-sweep step (drop into a workflow when a MPRIS change lands)

1. **Gates**: §4 automated tests + md5/installed-state checks.
2. **Sweep §5 A → I in order**, recording PASS/FAIL per (scenario ×
   dimension) with evidence: busctl Metadata/Position output, `stat`
   mtime of the art files, journal lines, TUI screenshot notes, log
   greps (`/tmp/s2udio_1000.log` for `unix_wait_status(512)` = mpv
   playback errors).
3. **Cross-scenario stale-art traps** (the shared-file races):
   - A → B: library cover gives way to the YouTube thumbnail.
   - B → G: YouTube art must be *removed* when radio plays.
   - B → B' (track change): mpris-art mtime/URL must change with the
     track; the expected-source guard must drop an in-flight download
     for the previous track.
   - D → E: `mpris-mpv-art` cleared on entry change (no stale poster).
   - Video → audio: `mpris-mpv-art` cleared at session end; the audio
     bridge is the only MPRIS owner again (mpDris2 restarts with the
     tracker's exit, ~5 s after mpv).
   - Video session: `org.mpris.MediaPlayer2.mpd` absent from the bus
     while mpv plays (scenario I); it returns after mpv exits.
   - mpv exit: video drops off the media controls within the bridge's
     stale budget (~30 s max).
4. **Daemon survival**: after every seek attempt in every scenario,
   `pgrep` both daemons.
5. **User eyeball**: the user confirms the desktop media controls (KDE)
   show the right art/title/timeline per scenario — the D-Bus level can
   pass while the client renders stale-cached art (the `?t=` cache-buster
   exists for exactly this).
6. **Record**: PASS/FAIL per scenario into the session log; failures →
   dated `FEEDBACK-*.md` with repro steps, kept on `working` (never
   delete older rounds). Re-run **all** affected §5 rows before declaring
   any MPRIS fix validated — that is the point of this plan.

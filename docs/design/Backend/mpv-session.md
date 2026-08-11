---
title: "mpv Session"
section: backend
doc_type: flow
id: "backend/mpv-session"
description: >
  The mpv video session: launch, IPC socket discovery, the 100 ms poll,
  item switching, reattach, preference chains and the caretaker daemon.
status: "current"
updated: "2026-08-11"
source_files:
  - src/core/mpv.rs
  - src/core/event_loop.rs
  - src/ui/panes/queue.rs
  - src/ui/panes/queue/video.rs (video queue entries / follow_playing_video; moved out of queue.rs in Phase 4b)
  - scripts/s2u-mpv-tracker (installed to ~/.local/bin)
  - tests/tracker/test_tracker.py
related:
  - backend/jellyfin-api
  - backend/ytdlp-resolution
  - tabs/queue-tab
tags: [mpv, video, ipc, mpris, playlist]
---

# mpv Session

## Flow overview

```
play video → run_mpv_playlist (launch mpv, spawn tracker)
  → MpvSessionStarted → hide cava, refresh art
  → 100 ms poll (position/title/volume/playlist-pos)
  → entry advance → MpvItemChanged (reset title/item/art/chapters)
  → stop / 5 dead-socket polls → MpvSessionEnded
```

## Launch & IPC

- **SVP support** (Settings -> mpv -> "svp support", config.ron
  `mpv.svp`, persisted to state.ron): when **on**, mpv is launched with
  `--input-ipc-server=/tmp/mpvsocket` — a fixed socket that SVP4's
  manager also connects to for frame interpolation. s2udio tracks
  playback (pause/seek/volume/state poll) over the **same socket**, so
  one mpv has one socket and both clients talk to it. When **off**
  (default), no flag is passed and mpv's IPC socket is whatever the
  user's mpv.conf / scripts provide (mpvSockets.lua per-instance sockets
  are still discovered as a fallback).
  mpvSockets.lua is no longer shipped: its runtime override of
  `input-ipc-server` closed `/tmp/mpvsocket` out from under SVP
  (the file lingered as a dead socket and SVPManager got
  connection-refused).
- Socket discovery (`mpv_socket()`): live `/tmp/mpvsocket` first, then
  the newest per-instance socket under `/tmp/mpvSockets` (legacy
  mpvSockets.lua setups), then the stale fixed path so callers can
  detect the dead socket. `s2u-mpv-tracker` mirrors the same order.
- The player binary comes from config.ron `mpv.bin` (default `"mpv"`).
  With SVP4, point it at SVP's bundled mpv (`~/.local/bin/SVP4/mpv/mpv`):
  it carries SVP's own portable VapourSynth (core R73 / API R4.1) +
  Python 3.12 (SVP components `deps.vapoursynth` v72, `deps.python`
  3.12.11, `opt.mpv` 0.41.0, installed via `svp4-maintenance
  --installPackages deps.python,deps.vapoursynth,opt.mpv`). The distro
  VapourSynth 77 + Python 3.14 stack crashes SVPflow/RIFE ~20-30 s in
  (SIGSEGV in `libsvpflow2.so`/`libvstrt.so`), so the bundled mpv is not
  optional with SVP.
- `play_video_entries` probes the socket before switching and launches a
  fresh mpv when it is dead.
- A switch requested before the socket is up is stored in
  `MpvSession.pending_loadfile` and applied by the poll once reachable.
- mpv launches with `--volume=<MPD volume>`; `--playlist-start=<clicked
  episode>` when a season playlist is loaded.

## The poll (100 ms)

Reads from mpv's IPC: position, `media-title` (re-read while it still
looks like a URL — `is_provisional_title` — so the real title is picked up
once mpv resolves it), volume, `playlist-pos`. Writes `mpv-mpris.json`
every tick (title, artist, poster path, position, duration, socket,
serialized playlist); the bundled `s2udio-mpris` daemon (spawned by the
`scripts/s2u-mpv-tracker` caretaker) exposes it over D-Bus
(`org.mpris.MediaPlayer2.s2udio`) so desktop media controls follow the
video. No patched mpDris2 is involved. **While a video session is alive
the tracker also stops `mpDris2.service`** (and restarts it when mpv
exits): mpDris2 always reports MPD's state — including "Paused" during
video — so without this the media controls would show a stale audio
entry next to the video one. The video's MPRIS (`s2udio`) is the only
source during playback; the audio MPRIS returns when the session ends.

## Playlist handling

- Playing an episode loads the **whole season** into the video queue: the
  season's episodes are fetched on the work thread and mpv launches on the
  full playlist; the poll follows `playlist-pos` (title/item id switch to
  the new entry, so progress and resume stay correct).
- Switching items while one plays: a same-season episode jumps mpv's own
  playlist (`playlist-pos`); anything else loads the file and swaps the
  recorded playlist. Playing the same item prompts to restart it from the
  beginning.
  **mpv playlist rebuild**: mpv's `loadfile … replace` swaps the *current
  entry only* — the rest of the old playlist survives and `playlist-pos`
  keeps its old value. A playlist switch must therefore `playlist-clear`
  first, then reload (`loadfile … replace` + `append` the rest), so mpv's
  real playlist equals the recorded one; otherwise a same-length old
  playlist (e.g. two 26-episode seasons) leaves mpv's stale position
  indexing the recorded playlist one entry ahead — the TUI/MPRIS would
  show the *next* episode's title while mpv plays the selected one
  (round-14 fix). The poll additionally confirms the recorded entry it
  adopts by comparing Jellyfin item ids against mpv's live `path` and
  skips the advance on a confirmed mismatch.
- **Adding a video while one plays never spawns a second mpv** — the paste
  popup's *Play via mpv* and the resolved-YouTube path go through
  `play_video_entries` (switch the running instance: `playlist-clear`,
  then first entry `loadfile … replace`, the rest appended).

## Preference chains (Settings → mpv; controls-bar buttons)

- Audio: `{system language | chosen} > original` → `--alang=<code>` (mpv
  falls back to the default/original track when nothing matches).
- Subtitles: `signs > {hidden | system language | chosen}` — the forced
  flags (`--subs-fallback-forced=always --subs-with-matching-audio=no`)
  always apply, plus `--subs-match-os-language=yes` for system or
  `--slang=<code>` for a chosen one (hidden adds nothing).
- Persisted to `state.ron` (`mpv_audio_lang`, `mpv_subtitles`) and
  restored at startup.
- **Live re-select**: while an mpv session is the UI source, the
  controls bar's `[Audio]` / `[Sub]` buttons (help-style language popup,
  see `frontend/layout-templates` + `ui/modals/language.rs`) update the
  same preference **and apply it to the running instance**: `set alang` /
  `set slang` record the choice, then `audio auto` / `sub auto` re-runs
  the automatic track selection for the current file (`Hidden` sets
  `sub-visibility no`).

## Lifecycle & reattach

- `MpvSessionEnded` on stop or **5 consecutive stale polls** — the poll
  counts failed socket reads (`mpv_exchange` returns `None` on a failed
  connect) and synthesizes the end event at 5, so teardown runs for
  reattached sessions too. The teardown also clears the `MPV_RUNNING`
  flag (the launcher thread clears it for its own session; a reattached
  one has no launcher thread).
- **Reattach**: if mpv outlives s2udio, `detect_mpv_session` — socket
  answers + `mpv-mpris.json` fresh (< 15 s) → restore title/artist/item
  id/art + serialized playlist, start the poll, hide cava, refetch
  metadata (resume is *not* re-applied — the video is already playing).
  **Stale or playlist-less state files** (s2udio closed > 15 s without
  the tracker running) lose the playlist — and with it the YouTube
  info/chapters/thumbnail lookups, which are keyed by the playing URL —
  so the entries are recovered live from mpv (`read_mpv_playlist`: the
  `playlist` property, falling back to the playing `path`; a Jellyfin
  stream's item id is re-derived from the entry URL). The tracker daemon
  is spawned on reattach too, so the state stays fresh for the next one.
  If MPD is already playing at reattach (a race left both playing) the
  video is paused — music wins ties.
- Closing s2udio while a video plays does not stop anything: the
  `s2u-mpv-tracker` daemon (Python stdlib, spawned next to mpv) takes over
  within a second of s2udio's exit — keeps `mpv-mpris.json` fresh, fetches
  Jellyfin metadata/poster, applies resume once, reports progress, stops
  `mpDris2.service` at session start (hides the paused MPD audio from the
  media controls) and restarts it on exit, and exits when mpv does (stale
  socket ~5 s; a pid file keeps it single-instance).
  Versioned at `scripts/s2u-mpv-tracker` (installed by `setup.sh`);
  integration tests in `tests/tracker/test_tracker.py`. Test env
  overrides (also listed in the script's header): `S2U_MPV_SOCKET` /
  `S2U_FORCE_CARETAKER` / `S2U_JELLYFIN_CONFIG` / `S2U_CACHE_DIR` /
  `S2U_MPD_HOST` / `S2U_MPD_PORT` / `S2U_MPD_PASSWORD` / `S2U_POLL_S`.
- **Both mpv and the tracker are launched in their own session
  (`setsid`) and ignore SIGHUP**: the terminal driver sends SIGHUP to the
  foreground process group when the window closes, and mpv quits on SIGHUP
  by default — without the detach, closing the TUI (not just quitting it)
  killed the video and the daemon before it could take over.
  `detach_child` (mpv.rs) wraps the `pre_exec` setsid + `signal(SIGHUP,
  SIG_IGN)` (the ignore is inherited across exec and mpv keeps it — even a
  direct `kill -HUP` cannot take the video down; SIGTERM and playlist end
  still quit it). Verified by
  `core::mpv::tests::detach_child_makes_the_child_its_own_session_leader`
  (own pgrp/session + SIGHUP ignored) and a full PTY reproduction: paste a
  video → `[Video] Play` → quit with `q` → mpv reparented and playing.

## Mutual exclusion (mpv video vs MPD music)

The video and the music never play at the same time; whichever starts
last wins. Both directions are enforced while s2udio is open **and** while
it is closed:

- **mpv starts while MPD plays** → MPD pauses. s2udio does this at
  `run_mpv_playlist` launch (only when MPD was actually playing).
- **MPD starts while the video plays** → mpv pauses (IPC `set_property
  pause`). s2udio does this on the MPD status transition to `Play`;
  the tracker does it in caretaker mode when s2udio is closed.
- **The video resumes while MPD plays** (desktop media controls, mpv's
  own OSD) → MPD pauses. Only the tracker can catch this when s2udio is
  closed; s2udio's event loop only sees the MPD side.
- The tracker's latches are armed at caretaker takeover (first tick
  s2udio is not running) so pre-existing state is never mistaken for a
  fresh start; if a race left **both** playing at takeover the video is
  paused (music wins). A paused video is never touched when MPD starts.
- Pausing is one-way: a paused player stays paused until the user resumes
  it — no auto-resume when the other player stops.
- **The UI follows the active source** (`mpv_is_ui_source`): while the
  video plays, the controls bar, seekbar, album art and info/lyrics box
  route to mpv; when MPD playback takes over (the video was paused by the
  mutual exclusion), they follow MPD — the now-playing line, clock and
  volume are the music's and the transport keys drive MPD. When the music
  stops, the UI returns to the (still paused) video, whose transport keys
  resume it. The mpv session itself stays alive throughout (paused),
  MPRIS/state-file writes and the Queue tab's Video list unchanged.

## MPRIS poster

The poll writes `mpris-mpv-art` per entry (`art_path` guard); the poster
file is cleared on entry change / session start / teardown so a previous
video's art never lingers. `s2udio-mpris` serves it (`mpris:artUrl`)
while the state file is fresh; stale (> 10 s) or gone → the bridge exits
and the video drops off the media controls.

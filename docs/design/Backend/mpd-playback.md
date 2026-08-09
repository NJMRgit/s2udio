---
title: "MPD Playback"
section: backend
doc_type: flow
id: "backend/mpd-playback"
description: >
  How s2udio drives MPD playback: the audio queue, temporary play entries,
  radio-style streams and MPRIS metadata tagging.
status: "current"
updated: "2026-08-05"
source_files:
  - src/mpd/client.rs
  - src/mpd/mpd_client.rs
  - src/mpd/commands/add_id.rs
  - src/ui/panes/queue.rs
  - src/ui/modals/paste.rs
  - src/ctx.rs
related:
  - backend/radio-directory
  - backend/paste-pipeline
  - backend/config-sidecars
  - tabs/queue-tab
tags: [mpd, playback, queue, mpris]
---

# MPD Playback

## Flow overview

```
user action → MPD command (client thread) → status poll (16 ms) → UI events
                    └── SongChanged / PlaybackStateChanged / QueueChanged → panes
```

- MPD lives at `127.0.0.1:6600` (config `address`); all MPD commands run
  on the dedicated MPD thread, never the render thread.
- `ctx.status` is refreshed every `status_update_interval_ms` (16 ms in the
  live config). Events fire only on *changes*: `PlaybackStateChanged`,
  `SongChanged`, `QueueChanged`.

## The audio queue

- The Queue tab's **Audio** view is the MPD queue itself
  (`src/ui/panes/queue.rs`); rows come from `ctx.status.queue`.
- Queue mutation helpers (`MpdClientExt`): `enqueue_multiple`, `add`,
  `add_id`, `play_id`, `delete` (range/marked), `clear`.
- The queue box title shows `N songs / total time` (`QueueBoxTitle`).
- `Del` removes the highlighted/marked song; a context-menu *Remove*
  deletes every marked song (marked ranges, highest first) then clears the
  marked set (`UiAppEvent::ClearQueueMarked`).

## Temporary play entries (play without adding)

Files/streams played "without adding to the queue" use a temporary entry:

1. `addid` + `playid` (see `src/mpd/commands/add_id.rs`) inserts a queue
   entry and plays it.
2. The entry's id is recorded in `ctx.temp_play_id`; the Queue pane's
   `local_queue` filters it out, so it never shows in the list.
3. Cleanup fires on `UiEvent::PlaybackStateChanged` (delivered *after*
   `ctx.status` refreshes — `Player` events arrive with stale status), so
   the Stop transition is reliably seen: the previous temporary entry is
   deleted before a new one is played, and on song change / stop the entry
   is dropped.

Used by: radio stations, the paste popup's *Play*, Jellyfin
audio items, directories `→` PlayFile.

## Streams and the queue

- Radio/icecast URLs and Jellyfin audio streams are added as queue entries
  whose URI is `http(s)://…`; the Queue pane filters stream URIs out
  (`QueuePane::local_queue`), so they never appear in the Audio list.
- **Resolved YouTube-style streams are queue content**: the paste popup's
  Add/Append puts their resolved URL into the queue, and because the URL
  is keyed in the yt-info cache (`ctx.yt_info`) the entry **stays
  visible**, showing the cached title/channel in the row (like the MPRIS
  tags). Unresolved/radio/Jellyfin URLs (never cached) stay hidden.
- Streams never fire `SongChanged` on their own; the temp-entry cleanup is
  therefore keyed to `PlaybackStateChanged`.

## MPRIS metadata tagging

On song change, `ensure_mpris_metadata` (`src/ui/modals/paste.rs`) tags the
queue entry so the media controls show a real title/artist/album:

- `cleartagid` **first** (MPD's `addtagid` appends — without the clear,
  re-tagging accumulates duplicates), then `addtagid`
  (title/artist/album).
- Recognized YouTube streams: tags come from the resolved `YtStreamInfo`
  (`ctx.yt_info` by `song.file`), plus `SaveMprisArt` (guarded by an
  expected-source; a failed fetch removes the file).
- Jellyfin streams: `FetchJellyfinMpris` (tags + art).
- Unrecognized streams / local files: the expected source is cleared and
  `mpris-art` removed so no stale thumbnail lingers.
- Official mpDris2 (`/usr/bin/mpDris2`, mpdris2-git) serves MPD's
  current song; s2udio re-tags streams so the media controls show the
  resolved title/artist. The `mpris-art` write is expected-source-guarded
  and is served for non-file stream URLs by the bundled `s2u-mpdris2`
  shim (loads the official binary, extends `find_cover()` at runtime,
  cache-busted URL; falls back unpatched if upstream refactors). The mpv
  session is exposed by the bundled `s2udio-mpris` daemon instead (see
  backend/mpv-session).
- **ReplaceAndPlay re-resolution catch-up**: a re-resolved YouTube entry
  gets a *new* MPD song id (`delete_id`+`add_id`) whose status update
  lands before the queue refresh — the stale-queue song-change check
  would miss it. `Ctx.metadata_processed_song` remembers the last song
  id the pipeline ran for; the `GLOBAL_QUEUE_UPDATE` handler re-runs
  `ensure_chapters` + `ensure_mpris_metadata` + `auto_show_chapters`
  when a song id appears in a refreshed queue that the status handler
  never processed (guarded by the marker).
- **Seeking HLS streams**: MPD cannot seek HLS audio ("Not seekable"),
  and official mpDris2's `seekid()` goes through `__getattr__` →
  `call()` → `reconnect()` on CommandError, which released the D-Bus
  name and killed the daemon. The shim installs a real `seekid()` method
  (CommandError → logged no-op) and guards the `Seek`/`SetPosition`
  D-Bus methods (CommandError, and KeyError when a stream status has no
  'time' timeline). The seekable-looking timeline is kept by design —
  seeks on the MPD path simply no-op.

## Local files over TCP

MPD refuses absolute local paths over TCP, so pasted/dropped local files
are relativized against `music_directory` (parsed from mpd.conf,
`paste::music_directory`); a file outside the library surfaces as an MPD
error.

## Playlist mutations

MPD's `playlistadd`/`delete`/`save` **drop** `#EXTINF` names, so
favourites mutations (radio `radio.m3u`, EXTINF format) rewrite the `.m3u`
file directly; MPD hot-reloads it via inotify (no idle event fires).

## The cava FIFO tap

MPD writes raw PCM to a fifo `audio_output` (e.g. `format "44100:16:2"`
for the visualizer). The cava config's `[input]` must match that format
**exactly** — mismatched sample bits/rate garble the bars. The settings
panel no longer exposes sample rate / bit depth; `CavaPane::spawn_cava`
parses the fifo output's `format` from mpd.conf
(`paste::mpd_fifo_format`, `backend/image-overlays`) and overrides the
cava `[input]` with it (rate, bits and channels).

## Key events

| Event | When | Typical consumer |
| --- | --- | --- |
| `PlaybackStateChanged` | play/stop/pause transition | temp cleanup, cava |
| `SongChanged` | queue advanced | MPRIS tagging, chapters, lyrics |
| `QueueChanged` | queue mutated | Queue pane, box title |
| `Database` | library updated | browser refresh |

## Threads & channels

- MPD thread: owns the client socket; commands go through
  `ctx.command(...)` / `ctx.query(...)` (async results routed back by id).
- Work thread: HTTP (radio, Jellyfin, yt-dlp) — never MPD.
- Render thread: reads `ctx.status` only.

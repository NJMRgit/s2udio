---
title: "Paste / Drag&drop Pipeline"
section: backend
doc_type: flow
id: "backend/paste-pipeline"
description: >
  The bracketed-paste entry point: capture, token parsing, classification,
  the [Audio]/[Video] popup, and routing to MPD or mpv playback.
status: "current"
updated: "2026-08-08 (round 18)"
source_files:
  - src/core/input.rs
  - src/ui/modals/paste.rs
  - src/core/event_loop.rs
related:
  - backend/ytdlp-resolution
  - backend/mpv-session
  - backend/mpd-playback
  - frontend/interaction
tags: [paste, dragdrop, popup, parsing]
---

# Paste / Drag&drop Pipeline

## Flow overview

```
terminal paste (bracketed paste / drag&drop)
  → Event::Paste → AppEvent::UserPaste
  → parse_paste → recognized items?
      → yes: paste popup ([Audio] + optional [Video]) → action → play/enqueue
      → no: nothing

Ctrl+V / middle click (mouse capture swallows the terminal's own paste)
  → read the clipboard / primary selection (`wl-paste`, `xclip`, `xsel`)
  → same AppEvent::UserPaste pipeline
```

## Capture

- Bracketed paste is enabled at terminal setup (`EnableBracketedPaste`);
  `src/core/input.rs` translates `crossterm::event::Event::Paste` into
  `AppEvent::UserPaste` and hands it to the UI (drag&dropped files/links).
- **Ctrl+V** (`Char('v')` + CONTROL) and **middle click** (mouse capture
  swallows the terminal's default middle-click paste) read the clipboard
  on a background thread (`wl-paste --no-newline` / `wl-paste --primary`,
  with `xclip`/`xsel` fallbacks, `timeout 2` guard) and feed the same
  `AppEvent::UserPaste` pipeline — the popup appears for recognized
  content, anything else is ignored. Middle click reads the primary
  selection (standard terminal semantics), Ctrl+V the clipboard proper.

## Parsing (`parse_paste` / `classify`)

The pasted text is split on whitespace (kitty `\ ` escaping handled),
trims quotes, dedupes (magnets additionally by infohash — the **full**
infohash since round 20, `magnet_infohash_full`). Each token is
classified:

1. `magnet:` links → `Magnet` (before the generic http(s) branch).
2. `file://` URIs → local `.torrent` file / audio / video file
   (extension lists; torrents must exist on disk).
3. YouTube / Soundcloud / NicoVideo links (`YtDlpContent`) → `Yt`.
4. Direct `http(s)` URLs → `.torrent` URL / audio (`Url`) / video
   (`VideoUrl`) by extension.
5. Local paths → `.torrent` / audio / video files (must exist on disk).

## The popup

**Torrent/magnet scan identity (round 20 — duplicate-paste fix)**: the
`[Torrent]` section is driven by up-front scans keyed by the item's
canonical key — a magnet's **full infohash** (`torrent_item_key` /
`TorrentItem::source_key`), a `.torrent`'s path/URL. The same torrent
pasted twice (even via a different magnet URI) hits the same
`Ctx.torrent_scans` slot: while the scan is in flight the second popup
shares it (no second engine), and a landed scan is kept after the popup
closes (engine shared via `Arc`) so a repeat paste reuses the engine
instead of spawning a second rqbit against the same cache dir. Failed
scans are dropped on popup close so a re-paste retries cleanly.

- An `[Audio]` section is **always** offered (every item can play as
  audio — video files/URLs stream their audio track through MPD,
  YouTube-style links resolve first):
  - *Play* — single item only; a temporary
    `addid`+`playid` entry.
  - *Add to queue and play* — insert after the current track and start
    playing the first inserted item immediately.
  - *Append to queue* — add at the end, no autoplay.
  - *Add to playlist* — pick an existing stored playlist (radio
    favourites excluded); direct items are added immediately, YouTube-
    style links after their streams resolve.
  - *Create Playlist* — name a new stored playlist; same direct/YT
    handling (all-YT items create it after resolving).
- A `[Video]` section appears when video content is present:
  - *Play* — via mpv (YouTube-style links resolve
    first; the playlist launches with real titles).
  - *Add to queue and play* — insert into the persistent video playlist
    (after the currently playing entry) and start playing the added
    videos immediately (a running mpv switches to them instead of a
    second instance).
  - *Append to queue* — into the persistent video playlist at the end,
    no autoplay.
  - *Add to playlist* / *Create Playlist* — the stored-playlist URIs
    (local paths, direct URLs, YouTube-style **original links**, like
    the video queue's *Create video playlist*), so the playlists tab can
    show the cached titles.
- A `[Torrent]` section appears when torrents/magnets are present. It is
  **scan-driven** (round 17): each item first shows a dim *Loading
  <label>…* row while the work thread scans it (`ScanTorrent`: engine
  start + add + metainfo wait + file list). Round 18: the metainfo wait
  is **open-ended and user-controlled** — no deadline, the user decides
  how long a cold magnet may take — and runs on a **dedicated scan
  thread** (the work thread is never blocked). While it waits, the
  Loading row becomes a live **wait window**: `Loading <label>… mm:ss`
  (elapsed counter), `DL <speed> · need ≥ <min> KB/s ✓/✗` (live speed
  vs `torrent.min_download_speed_kbps`) and `esc to cancel`; each row
  refreshes with the scan thread's `WorkDone::TorrentScanProgress`
  events. Esc/close aborts every in-flight scan (its thread drops the
  engine, killing rqbit). When the scan lands the popup refreshes in
  place with the play actions its file list enables —
  *Play (stream)* (largest playable file), *Play and Download* (keeps
  the completed file to `s2udio-downloads` in `~/Downloads`, deferred
  until mpv stops using the stream — single-file torrents only), and for
  multi-video season packs *Play all (N files)* and *Select files…*
  (multi-select, name + size). The play actions reuse the scanned engine
  (falling back to a fresh spawn when it is gone). The file picker
  **captures the scan when it opens** (round-18 host fix 2026-08-09):
  opening the picker is itself a popup action, and the popup's close
  hook (run right after) clears `Ctx.torrent_scans` — so the picker
  plays the marked files from the captured scan, never re-looking it up.
  A dead magnet / data torrent shows a dim notice row instead. See
  `backend/torrent-streaming.md`. When `torrent.enabled: false` the
  section shows a dim `Torrent streaming disabled` row instead. The
  `[Audio]`/`[Video]` sections operate on their own item subsets, so a
  mixed paste (audio + magnet) keeps the audio actions and the torrent
  section separately; a pure-torrent paste shows only `[Torrent]` +
  Cancel.
- Group headers (`[Audio]` / `[Video]` / `[Torrent]`) are dim,
  non-selectable rows.
- `Cancel` closes the popup without doing anything.

## Routing

- **Audio**: `ResolveYtStreams` (yt-dlp, work thread) →
  `apply_resolved_streams` stores info (memory + disk cache) then plays
  (temp entry) or adds/appends to the queue. *Add to queue and play* uses
  `enqueue_multiple`'s `autoplay_idx` (the index the first inserted item
  lands at: past the current track, or the old queue end when nothing
  plays); the YouTube variant (`YtAction::AddAfterCurrentAndPlay`)
  inserts then `play_pos` the same index. Local files are relativized to
  `music_directory` first (MPD refuses absolute paths over TCP).
- **Playlists**: *Add to playlist* adds direct URIs via
  `add_to_playlist_multiple` and sends `YtAction::AddToPlaylist(name)`
  for YouTube-style links (resolved on the work thread, then added);
  *Create Playlist* uses `YtAction::CreatePlaylist(name)` when only
  YouTube-style links are pasted, otherwise `create_playlist` with the
  direct URIs plus `AddToPlaylist` for the links.
- **Video**: `YtAction::PlayVideo` / the video queue → mpv on the original
  links with resolved titles; session metadata (title/channel/thumb/
  chapters) is keyed by the original link. *Add to queue and play* adds
  to the persistent playlist then `play_video_entries` (launches mpv, or
  switches the running instance).

## Related utilities

- `video_entries_for` / `yt_urls` / `all_yt` — mpv playlist entry builders.
- The persistent video playlist (`<cache_dir>/video-playlist.json`) is
  restored at startup and survives mpv closing / audio playback.

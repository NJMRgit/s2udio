---
title: "Queue Tab"
section: tabs
doc_type: spec
id: "tabs/queue-tab"
description: >
  The playback queue first tab: Audio / Video / Chapters sub-tabs, the
  merged queue box, the playing marker, album sorting and the video
  playlist.
status: "current"
updated: "2026-08-11"
source_files:
  - src/ui/panes/queue.rs
  - src/ui/panes/queue/context_menus.rs (context menus; moved out of queue.rs in Phase 4b)
  - src/ui/panes/queue/video.rs (Video sub-tab; moved out of queue.rs in Phase 4b)
  - src/ui/panes/queue/chapters.rs (Chapters sub-tab; moved out of queue.rs in Phase 4b)
  - src/ui/panes/queue_header.rs
  - src/ui/tab_screen.rs
  - src/ui/widgets/sub_tab_bar.rs (the `● Audio ○ Video ○ Chapters` toggle row; extracted in Phase 4a)
related:
  - backend/mpd-playback
  - backend/mpv-session
  - backend/chapters
  - frontend/layout-templates
tags: [tab, queue, video, chapters]
---

# Queue Tab

## Position & identity

First tab, renamed from "Local": `Queue │ Playlists │ MPD • Jellyfin •
Radio`. Opens on Queue for a playing YouTube stream (only true
radio streams open Radio), keeping the YouTube thumbnail visible as album
art.

## Sub-tabs

`● Audio ○ Video ○ Chapters` on a reserved 1-row spacer above the queue
box (segments clickable; `●`/`⭘` are one cell wide so the row never
shifts). `c` **or `<S-Tab>`** cycles Audio → Video → Chapters → Audio
(Chapters skipped when unavailable).

- **Audio** = the MPD queue (see `backend/mpd-playback`). Radio/Jellyfin
  temp streams are hidden, but **resolved YouTube streams added via the
  paste popup stay visible** (cached in `yt_info`), rendering their
  cached title/channel in the row instead of "Unknown".
- **Video** = the current mpv playlist (persistent video playlist when no
  session): `Title | Duration`, the playing entry marked `❯`, a dim
  "No video playing" hint when mpv has no playlist. `Del` removes the
  highlighted entry **or every marked entry**; the context menu is
  video-scoped (Play from here / **Download** for a resolved stream /
  Remove / Clear video queue / **Create video playlist** — never the MPD
  queue), and Remove deletes every marked entry. While a Jellyfin item
  plays, the list is the live mpv session playlist and
  Remove/Clear/Create are hidden (Jellyfin videos are not eligible for
  playlists).
- **Chapters** appears when the current track has markers (see
  `backend/chapters`).

## The merged queue box

One rounded box: the column header row + a `│───…───│` divider inside;
bottom title `N songs / total time` (`N videos / total time`, `N
Chapters / total time`). Columns:

- Audio: `Title | Album | Artists | Duration`.
- Chapters: `Chapter | Time | Duration` (left / centered / right-aligned,
  padded by display width).
- Video: `Title | Duration`.

The playing marker is `❯` (U+276F, one cell) — `▶` falls back wide and
shifts the row.

## Navigation & activation

- `w`/`s`/`↑`/`↓` move; `d`/`→` play the highlighted track and
  `Enter` opens the **context menu** (same as right-click) in the Audio
  and Video lists — the Chapters list keeps `d`/`→`/`Enter` seeking to
  the highlighted chapter; `Space` toggles the video's pause while a
  video plays; `PageUp`/`PageDown` full-page scroll; `Del` deletes.
- **The wheel moves the highlight in all three lists** (Audio, Video,
  Chapters) like `w`/`s`, honoring the configured `scroll_amount` and
  clamping at the top/bottom.
- A **single click highlights only** (never reloads the playing entry the
  list opens with highlighted); a **double click loads**.
- Album header click cycles album track order: disc/track → album tracks
  a-z → album tracks z-a.
- ctrl/alt-click multi-selects; **ctrl+click is additive** (the row under
  the cursor joins the selection, clicks only grow the marked set); a
  plain click on another row clears the marks; **Ctrl+A** marks the whole
  list; context-menu Remove deletes every marked song. The audio menu's
  *Add to playlist* / *Create playlist* act on the marked rows when any
  are marked, else on the highlighted song (a single selected song gets
  them too).
- The **Video list has the same multi-selection**: ctrl+click adds a
  mark (never removes), alt+click range-marks from the anchor,
  `W`/`S`/`Shift+↑/↓` range-select (each press replaces the previous
  range), **Ctrl+A** marks the whole list, a plain click on a different
  row clears the marks, and `Del` / the context menu's Remove delete
  every marked entry (the selection shifts up past the removed rows).
  Marked rows render with the lighter `marked_item_style`; the Jellyfin
  session's live mpv playlist is never deletable.

## Playlists from the queue

**Download** (context menu, stream rows only): a resolved YouTube-style
row — in the **Audio** list or the **Video** list — offers *Download*,
which saves the media into `s2udio-downloads` in `~/Downloads` (save as
audio/video, and per-chapter files when the media has chapters). The
video row is **replaced by the file** (mpv plays absolute paths); the
MPD audio queue keeps the stream (the file is outside the library) and
a status reports the save. **Replaces the row** (video)
with the downloaded file(s) once complete (see
`backend/stream-downloads`).

Playlists are created **audio-only or video-only**, from the list that is
on screen:

- The **audio** context menu's *Create playlist from queue* saves the
  visible queue rows (local files, direct URLs, resolved streams) into a
  new MPD stored playlist; *Add queue to playlist* adds the same visible
  set to an existing playlist. The hidden temporary "play without adding
  to queue" entry and radio/stream rows are **never** saved — so a temp
  entry can't leak into an existing playlist either. *Add to playlist*
  and *Create playlist* act on the selected tracks only (the marked rows
  when any are marked, else the highlighted song).
- The **video** context menu's *Create video playlist* saves the
  persistent video queue's URLs into a new MPD stored playlist (video
  entries carry stable original links and real titles; local paths get
  the `file://` prefix so MPD accepts them). Hidden while a Jellyfin
  item plays.
- The `s`-key save flow (`CommonAction::Save`) already operates on the
  visible queue rows only.

## App-open behavior

Startup sets `queue_show_chapters` for a chaptered current song; the queue
follows the playing video (auto-switches to Chapters when the video has
markers, else Video — gated by the Settings "auto chapters" toggle). A
**chaptered audio track auto-opens the Chapters list too**: on song change
and wherever the current song's chapters arrive (resolved yt info,
Jellyfin fetch, ffprobe), `Ctx::auto_show_chapters` flips the Queue tab's
internal list — the **active tab is never switched**, and the switch is
skipped while a video plays in mpv.

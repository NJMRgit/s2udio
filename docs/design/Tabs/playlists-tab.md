---
title: "Playlists Tab"
section: tabs
doc_type: spec
id: "tabs/playlists-tab"
description: >
  The playlist browser: the playlist list, its songs and the info box,
  with the radio favourites hidden.
status: "current"
updated: "2026-08-05"
source_files:
  - src/ui/panes/playlists.rs
  - src/ui/browser.rs
related:
  - backend/mpd-playback
  - frontend/layout-templates
tags: [tab, playlists]
---

# Playlists Tab

## Layout

The rmpc playlist browser, using the tab-list palette (names explicit ANSI
white, secondary text `list_text_color`, blur accent reserved for the
selection highlight):

- **Playlist list** (left / top): the stored playlists. Like the MPD
  tab's folder tree, the left pane keeps a **minimum width of 50
  columns** and is **hidden entirely on TUIs ≤ 120 columns wide** (the
  `tree_width` split shared with the MPD and Jellyfin panes); when
  hidden, the songs pane takes the whole area and scroll over the left
  columns lands on it.
- **Right pane** — the current node's children, like the MPD pane: at the
  **root it lists every playlist** (titled ` Playlists `, mirroring the
  left list and sharing its selection); inside a playlist it lists its
  **songs** (titled ` Songs `).
- **Info box** with the selected item's details (playlist name at the
  root, the selected song's details inside — stream entries get the
  video-style layout), **scrollable** like the other tabs' info boxes:
  the themed scrollbar appears when the content overflows, the wheel
  scrolls the box and clicking/dragging the scrollbar jumps
  proportionally. Like the MPD tab, the info box takes **about two
  thirds of the pane height, capped at 15 lines** (exact-length split
  below the fixed 3-row tips strip; taller terminals give the songs
  list the space above the cap).

## Playlist types (♪ / ▶)

Playlists are created **audio-only or video-only** (see the Queue tab's
context menus), and the list reflects the type with a prefix before the
name:

- **audio** playlists get `♪`
- **video** playlists get `▶`

The kind is classified in the background (`PLAYLIST_KINDS` query): every
stored playlist's first entry is checked (`listplaylistinfo 0:1`, MPD
>= 0.24; a full fetch on older servers) and any video file/URL marks the
playlist as video. Unknown kinds (classification still pending) render
as audio.

## Streams inside a playlist

Online streams (resolved YouTube-style URLs, or their original links)
render with the **cached info** instead of a long random URL:

- The song row shows the cached title (looked up by URL in the yt-info
  cache, falling back to a matching `original_url`), in a **dark blue**
  text (`as_stream_text_style`) so streams stand apart from local files,
  which keep the normal white rows.
- The **info box of a selected stream entry uses the same video-style
  layout as the queue tab** (title, channel/subs row, `Description ↴`
  and the wrapped, emoji-scrubbed body — `lyrics::yt_stream_info_parts`)
  instead of the generic song preview, so a stream added to a playlist
  keeps its cached description/thumbnail details.

## Behavior

- Navigation is the **MPD / Jellyfin scheme**: one cursor on the list
  that is on screen — the **playlists** at the root, the **songs**
  inside a playlist. `w`/`s`/`↑`/`↓` all move it, `d`/`→` open a
  playlist or play the highlighted song (replace queue + autoplay) and
  `Enter` opens the **context menu** (same as right-click),
  `a`/`←`/Esc back out to the playlist list (no-op at the root).
  `PageUp`/`PageDown`, `Top`/`Bottom`, `W`/`S`/`Shift+↑/↓` range
  selection and `Del` (delete the highlighted/marked playlist or song)
  all operate on the same list.
- Double-click: a playlist opens, a song plays (like the MPD pane). At
  the root a click on either the left list or the right pane's playlist
  list selects the same row (they share the selection); the right pane's
  rows double-click to open too.
- The songs pane (right pane inside a playlist) has the same
  multi-selection as the queue / MPD panes: ctrl+click is additive (the
  row under the cursor joins the selection, clicks only grow the marked
  set), alt+click range-marks from the anchor, `W`/`S`/`Shift+↑/↓`
  range-select (each press replaces the previous range), **Ctrl+A** marks
  every song in the playlist, a plain click on a different row clears the
  marks, and the marked rows render with the lighter `marked_item_style`.
  The song menu acts on **every marked song** when any are marked: *Add
  to queue*, *Replace queue*, *Create playlist*, *Add to playlist* and
  *Remove from playlist* all use the marked set (a single highlighted
  song otherwise).
- Context menus are scoped to the highlighted item (play, enqueue,
  delete, rename); right-click on a playlist or song opens them. The
  song menu's **Remove from playlist** deletes the highlighted song (or
  every marked song) from the current playlist (with the narrow
  settings-style confirmation dialog), keeping the playlist list and
  songs list in sync via the usual refresh. A **stream** song (resolved YouTube-style URI) also
  offers **Download** — save into `s2udio-downloads` in `~/Downloads`
  (audio/video, per-chapter files when the media has chapters). The file
  lives outside the MPD library, so a stored playlist **keeps the stream
  entry** (a status reports the save) instead of replacing it (see
  `backend/stream-downloads`).
- Playlist rows keep the same white/grey palette as the queue.

## Radio favourites

The MPD stored playlist `radio.m3u` (`~/.config/mpd/playlists/radio.m3u`)
is **hidden** from the Playlists tab — it belongs to the Radio tab
(`backend/radio-directory`).

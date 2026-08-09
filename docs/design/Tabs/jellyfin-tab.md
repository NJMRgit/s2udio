---
title: "Jellyfin Tab"
section: tabs
doc_type: spec
id: "tabs/jellyfin-tab"
description: >
  The Jellyfin browser: library tree, shared-selection navigation, the
  poster-backed info box, playback modes and the mpv video session.
status: "current"
updated: "2026-08-07"
source_files:
  - src/ui/panes/jellyfin.rs
  - src/jellyfin/mod.rs
  - src/core/mpv.rs
related:
  - backend/jellyfin-api
  - backend/mpv-session
  - frontend/interaction
  - frontend/layout-templates
tags: [tab, jellyfin, video, poster]
---

# Jellyfin Tab

## Identity

Styled like the Radio tab: **tree left**, **items right**, **info box
below**. Cava is hidden on this tab (video browsing doesn't feed it).
Like the MPD and Playlists panes, the left library-tree pane keeps a
**minimum width of 50 columns** and is **hidden entirely on TUIs ≤ 120
columns wide** (the shared `tree_width` split): on a narrow TUI the
items pane takes the whole area and scroll over the left columns lands
on it. The info box keeps its normal height (only the MPD and Playlists
tabs' info boxes are taller).

## Library tree

- Video libraries sort **above** music libraries (when the server has
  both); music views expand to artists → albums, other views to folders →
  seasons → episodes.
- Every node loads lazily (`ensure_loaded` per kind); leaf containers
  (seasons, folders without subdirectories) show no expand arrow once
  loaded.

## Shared-selection navigation

- The right pane always lists the **current node's children**; at the root
  it lists every library.
- The tree highlight follows the right-pane cursor (falling back to the
  enclosing node for episodes); moving up **collapses the branch you
  left**; there is **no `/` root row** — a library or subdirectory is
  always highlighted.
- `w`/`s`/`↑`/`↓` move the right pane (wasd mirrors the arrows: `d` =
  `→`); `→`/`Enter`/`d` open a container or play a file; `a`/`←` back out
  one level, collapsing the branch left.
- Clicking a tree row jumps to it; double-clicking expands it.
- The playing item is highlighted with a `▶` marker in the items list.

## Info box

- **Library categories and seasons**: image-only — the poster centered in
  the whole box, no text rows.
- **Movies/episodes**: video layout — poster left 40 %, fixed header
  (year — marquee title, time, `Episode: NAME  S03E03`), scrollable
  overview, credits pinned.
- Everything else: key-value rows with the poster on the left.
- The poster is a **terminal-side overlay** (same backend as album art),
  cleared/redrawn on selection change, hidden while a modal is open and
  during resizes (`backend/image-overlays`).
- Mouse-scrollable; the offset resets on selection change.

## Playback

- **Audio items** play through MPD as temporary stream entries.
- **Video items** (movies/episodes) follow `video.playback`
  (`ask` / `mpv` / `mpd`): mpv with the `Videos/{id}/stream` URL, mpd
  audio track, or an ask popup.
- Playing an episode **loads the whole season into the video queue** (mpv
  launches on the full playlist, `--playlist-start=<clicked>`); Jellyfin
  reports progress (10 s throttle / on pause) and applies saved resume
  positions (`backend/jellyfin-api`, `backend/mpv-session`).
- While a Jellyfin item plays, the Queue tab's Video view swaps to the
  live session playlist; the persistent queue is cached untouched and
  returns when the session ends.

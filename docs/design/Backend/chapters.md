---
title: "Chapters"
section: backend
doc_type: flow
id: "backend/chapters"
description: >
  Chapter marker sources, keying, the queue's Chapters view and seek
  routing to MPD or mpv.
status: "current"
updated: "2026-08-05"
source_files:
  - src/shared/ytdlp/stream.rs
  - src/jellyfin/mod.rs
  - src/core/mpv.rs
  - src/ui/panes/queue.rs
related:
  - backend/ytdlp-resolution
  - tabs/queue-tab
tags: [chapters, youtube, jellyfin, ffprobe, seek]
---

# Chapters

## Flow overview

```
song change / startup
  → ensure_chapters: yt resolved info → Jellyfin Fields=Chapters → local ffprobe
  → keyed by the playing URL in ctx.chapters
  → the queue's Chapters view / Jellyfin video chapters use them
```

## Sources

1. **YouTube-style streams**: embedded chapters from the resolved
   `YtStreamInfo` (`yt-dlp -J` `chapters`), or derived from the
   description — lines matching `MM:SS Title`
   (`chapters_from_description`), sorted, deduped, clamped to duration.
2. **Jellyfin items**: `Fields=Chapters` on the full-item fetch
   (`Chapters[]` `StartPositionTicks`/`EndPositionTicks`, 10 ms units).
3. **Local files**: via ffprobe.

## Keying

- Stored in `ctx.chapters`, keyed by the playing URL (resolved stream URL
  **and** the original link).
- The queue's Chapters view prefers the **mpv item** when a video plays
  (`current_playback_chapters`), else the queue song.
- Startup / song change refresh (`ensure_chapters`).
- **Auto-open**: when the current song's chapters arrive (song change,
  resolved yt info, Jellyfin fetch, ffprobe) and the auto-chapters setting
  is on, `Ctx::auto_show_chapters` flips the Queue tab's internal list to
  Chapters (the active tab is never switched; skipped while mpv plays).

## Seek routing

- A seek goes to **MPD** (normal queue song) or **mpv's IPC socket** (the
  playing video).

## View rules (Queue tab → Chapters)

- Columns: `Chapter | Time | Duration` (Chapter left, Time centered,
  Duration right-aligned; padding by display width so wide glyphs can't
  push the values).
- Navigation: `w`/`s`/`↑`/`↓` move the highlight (first move selects
  chapter 0); `PageUp`/`PageDown`/Home/End page and jump; `d`/`→`/`Enter`
  seek to the highlighted chapter (dropping the highlight).
- Mouse: a **single click highlights only** (never seeks, even on an
  already-highlighted row); a **double click seeks**; the **wheel moves
  the highlight**.
- The `❯` marker uses the one-cell glyph (see `frontend/glyphs`).

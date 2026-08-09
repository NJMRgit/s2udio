---
title: "Glyph Usage"
section: frontend
doc_type: constraints
id: "frontend/glyphs"
description: >
  The glyph inventory the UI relies on, with the one-cell constraint every
  symbol must satisfy in the user's terminal font.
status: "current"
updated: "2026-08-05"
source_files:
  - src/ui/panes/queue.rs
  - src/ui/panes/queue_header.rs
  - src/ui/panes/radio.rs
  - src/ui/panes/album_art.rs
  - src/ui/modals/settings.rs
  - src/config/theme/mod.rs
related:
  - frontend/colors-typography
  - frontend/layout-templates
tags: [glyphs, symbols, unicode, layout]
---

# Glyph Usage

## The one-cell rule

Every glyph below must render at **exactly one cell** in the user's font
(kitty, ~490 px / 70 cols). A glyph that falls back to a wide (two-cell)
form shifts the row — the layout assumes one-cell glyphs and collapses
below minimums. Verify a new symbol in the target font before using it.

## Inventory

| Glyph | Meaning | Where |
| --- | --- | --- |
| `❯` | queue / chapters playing marker | queue list, chapters list |
| `▶` | **fallback hazard** — wide in some fonts, shifts the row | avoid for markers; used as Jellyfin play marker |
| `●` / `⭘` | Audio / Video sub-tab toggle (filled / hollow) | queue sub-tab row |
| `↓` / `↑` | scrollbar ends | scrollbars everywhere |
| `│` | scrollbar track / box dividers | scrollbars, settings divider |
| `↴` | info box description arrow; the always-open MPD Library root (`Library ↴`) | video info box, MPD tree |
| `⤓` | mpv-mode Download button (a resolved ytdlp stream is playing) | controls bar row 1 |
| `▸` / `▶` / `▼` | tree arrows (collapsed / expanded) | Directories, Radio, Jellyfin trees |
| `★` / `◎` | radio favourites / local markers | Radio tab |
| `✓` / `✗` | metainfo-wait speed check (live speed meets / misses the needed speed) | paste `[Torrent]` wait window |
| `▢▢` | transparent color swatch | Settings appearance rows |
| `…` | truncation ellipsis | long labels |
| `[-]` `[+]` `[<]` `[>]` `[x]` `[ ]` | stepper / cycle / toggle buttons | Settings rows |
| `█` | scrollbar thumb / progress fill | scrollbars, progress bar, seekbar |

## Border sets

Rounded (`╭─╮│╰─╯`) is the standard border for panes, modals and boxes;
plain borders for inner panes. The queue box merges a header row + a
`│───…───│` divider inside one rounded box (the divider is drawn by the
header pane; the box supplies the `│` ends).

## Scrollbars

Track = `│`, ends = `↓`/`↑`, thumb = `█`; the settings content scrollbar
and the info-box scrollbars use the same set. Scrollbars are click/drag
targets.

## Cava bars

Cava's bar symbols come from the theme (`bar_symbols` / inverted set,
default 8 levels of block glyphs); `bar_width` grows instead of adding
bars beyond `MAX_CAVA_BARS` (64) so a wider window makes thicker bars.

## Wide-glyph traps

- `▶` as a playing marker falls back wide in the user's font → use `❯`
  (U+276F).
- Wide titles in the Chapters columns are padded by **display width**, not
  character count, so values never shift.
- The `●`/`⭘` toggle glyphs are one cell wide so the sub-tab row never
  shifts.

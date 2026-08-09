---
title: "Layout Templates"
section: frontend
doc_type: constraints
id: "frontend/layout-templates"
description: >
  The reusable layout templates: the top bar, the merged queue box, the
  lyrics/info combo, the video info box, the browser template, the
  now-playing bar and the modal templates.
status: "current"
updated: "2026-08-07"
source_files:
  - src/config/theme/mod.rs
  - src/ui/tab_screen.rs
  - src/ui/panes/mod.rs
  - src/ui/panes/queue.rs
  - src/ui/panes/queue_header.rs
  - src/ui/panes/lyrics.rs
  - src/ui/panes/directories.rs
  - src/ui/panes/radio.rs
  - src/ui/panes/jellyfin.rs
  - src/ui/modals/settings.rs
related:
  - frontend/colors-typography
  - frontend/glyphs
  - tabs/queue-tab
  - tabs/settings
tags: [layout, template, panes]
---

# Layout Templates

## Top bar

Tabs left (`Queue │ Playlists │ MPD • Jellyfin • Radio • Search`), the
`Help | Settings` buttons right-aligned. Separators shrink to single space
on narrow terminals; the bullet separators follow MPD/Jellyfin/Radio.

## Queue box (merged)

One rounded box contains the column header row (`Title | Album | Artists |
Duration` in Audio, `Chapter | Time | Duration` in Chapters, `Title |
Duration` in Video) **and** a `│───…───│` divider, with the bottom title
`N songs / total time` (or videos/chapters). A reserved 1-row spacer above
hosts the `● Audio ○ Video ○ Chapters` sub-tab row.

## Lyrics / info combo

A clean solid border, **no title**. **Minimum 4 content rows** (title +
context row + description header + one content row): the pane renders
nothing below that, and the queue tab's art+lyrics split **collapses
entirely — box, borders and space** — when it can no longer fit them (the
responsive `window_sizes` sizing snaps to hidden below ~6 total rows, so
the queue takes the freed space instead of an empty bordered shell). While
**paused** it shows the
currently playing song's details; while **stopped** (nothing playing) it
shows the highlighted item's details from the visible list; while playing
it shows the current song's details when no lyrics exist, and the lyrics
when they do. Nothing playing → the now-playing line reads `No Playback`.
Yellow group
labels; the body is mouse-scrollable (scrollbar on overflow; offset resets
on song change). Lyric sync is **stateless** — computed from
`ctx.status.elapsed` each render; the next-line one-shot schedule is
invalidated on `PlaybackStateChanged` / song changes / fresh starts
(pause/resume can never leave the highlight stuck) and targets the first
line's start during the song intro.

While lyrics show, the box is framed for the buttons: a **blank top row**, the lyrics body, a **full-width `─` margin line** in the border style, and the **bottom row carries only the two buttons**, right-aligned **one cell in from the right border**, styled as one cluster — **`● hide lyrics | ● fetch lyrics`** (space-padded `|` separator; no `Artist - Title` header — the lyrics body sits between the blank top row and the margin line). Each button has a marker glyph beside its label, **`●`** by default; the **`⭘` pressed marker is pressed-while-held only** (shows while the mouse button is down, reverts to `●` on release — never persistent; a held button does not repeat its action). On terminals that report mouse release events (the kitty family) the release itself reverts the marker, so `⭘` persists for the whole hold; emulators that don't report releases get a 300 ms release-check fallback so the marker never sticks:

- **hide lyrics** — marks the current song's lyrics wrong and **hides
the body**, which then shows the **same paused-style info panel** the
pane uses while paused/stopped (the current song's metadata); the
button label switches to **`● show lyrics`**. A second click (or
`fetch lyrics`) clears the mark and the lyrics (and `● hide lyrics`)
reappear. Per-song, in-session state (no disk persistence); the mark
follows the song file, so it survives pane re-renders and other-song
detours. The hidden lyrics are the indicator — no persistent `⭘`.
- **fetch lyrics** — forces a refetch of the current song's lyrics (the
configured `on_song_change` command, `rmpc-fetch-lyrics` by default)
and reloads them when the file lands; the fetch **clears the wrong-mark
immediately**, so hidden lyrics reappear. No in-flight marker (the
`⭘` is pressed-while-held only).

Hover applies the **queue-list row highlight** (the theme's
`hovered_item_style`: a slightly-brighter-than-selection background +
bold) **plus the standard label-text brightening to the label text
only** — the `●`/`⭘` glyph keeps its completely normal style (no
hover background, no bold, no brightening); the `|` separator is
never hover-highlighted. Hover is per-button, and any keyboard input
clears the hover (global rule). The buttons are mouse-only (no
keybinds). When the row is too narrow for the full labels the cluster
**collapses to `● hide | ● fetch`** (or `● show | ● fetch` while
hidden); only when even that does not fit are the buttons omitted (no
click zones).

## Video info box

- Header: the marquee title renders in explicit ANSI white (never the
auto/blur accent) + bold right-aligned `Time: HH:MM`.
- Context row (theme color): `Channel: X   Subs: 1.71M` (YouTube) or
  `Episode: NAME   S03E03` (Jellyfin).
- `Description ↴` label: yellow text, white arrow; the wrapped body uses
  the list text color and scrubs emoji; mouse-scrollable.
- Jellyfin credits pinned below (`Director: / Writer: / Starring:`).

## Browser template (Directories / Radio / Jellyfin / Search)

Three zones: a **tree/region list left**, the **items list right**, the
**info box below** (yellow group labels). Left pane width ~20 columns;
the info box reuses the yellow-label format. Search reuses the same
template: filter inputs in a left ` Search ` pane, ` Results ` + tips +
` Info ` right.

### Browser panes override (MPD / Playlists / Jellyfin)

All three browser tabs' left panes (MPD folder tree, playlists list,
Jellyfin library tree) keep a **minimum width of 50 columns** — the
30% proportional share applies but never below 50, and the right pane
takes the remainder. On TUIs **≤ 120 columns wide the left pane is
hidden entirely**: the tab shows only the right pane, which scrolls
and navigates normally (the hidden pane's rect is inert for mouse
events). The `tree_width` split in `src/ui/panes/directories.rs` is
shared by all three panes. Below the 3-row tips strip, the **MPD and
Playlists tabs' info boxes take about two thirds of the pane height,
capped at 15 lines** (exact-length split; on taller terminals the
files/songs list takes the space above the cap); the other tabs keep
the standard info-box height.

## Now-playing / controls bar

Three rows between cava and the seekbar. Row 1: the **channel/show/album**
(album for music, the channel for a YouTube stream, the show for a
Jellyfin episode, the station name for radio) **left-aligned and truncated
(never scrolls)** (up to a third of the row), the **Artist+Song /
episode/movie / video title** **centered between it and the row's
right-aligned buttons** (marquee inside that region when it overflows:
holds 2s, scrolls left, wraps with a 5-column tail→head gap). Row 2 the
separator; row 3 the transport `|   ◀◀   ▶   ▶▶  |  ■` with a `│`
separator (stop is separated by a pipe; play/pause is the plain grey
glyph); `time | volume` line with the seekbar. Missing artist/title tags
are omitted entirely (no `Unknown -` prefix).

The right-side buttons depend on the active source: while **MPD** plays
(and while mpv is not the UI source) they are `Repeat Random Single
Consume`; **Single** is always enabled as a oneshot (Off ↔ Oneshot);
**Consume** cycles Off → On → Oneshot → Off; the oneshot state renders
yellow (`ControlsTheme::oneshot`; on = accent, off = dim). While an mpv
session is the UI source the MPD modes are replaced by the mpv buttons:
**⤓** (Download, only while the playing media is a resolved ytdlp
stream; furthest left), **[Audio]** (to the left of subtitles) and
**[Sub]** (furthest right). [Audio]/[Sub] open a **help-style language
popup** (compact centered box, `Preference`/`Languages` sections, list
navigation — `ui/modals/language.rs`) and apply the choice live to the
running mpv (`alang`/`slang` + re-running the auto track selection, see
`backend/mpv-session`); ⤓ opens the save-as menu (see
`backend/stream-downloads`).

The controls box colors follow the blur accent via
`ControlsTheme::from_ctx`.

## Settings panel (full-window)

Rounded border + centered ` Settings ` title; left sidebar
(general/keybinds/mpv/mpd/jellyfin) with `>` on the active section and a
`│` divider; right content rows are **label left / control right-aligned**;
section headers (`[ features ]` etc.) carry right-aligned descriptions;
footer at the bottom. Rows shrink labels with `…` on narrow terminals.

## Modals

- **Menu** (context menus, paste popup): centered rounded box, list
  sections with dim group headers, scrollbar.
- **Confirm**: centered box, button group (navigate with ←/→, activate
  with Enter/Space/double-click).
- **Select / Input**: centered list / text field.
- **Help** (per-tab keybinding popup): compact 46×16 window, Basic view
  by default (wasd/arrows/Enter), `a` toggles Advanced (all keybinds),
  Tab switches tabs behind it, Esc closes.
- Modals hide all terminal-side overlays while open
  (`backend/image-overlays`).

## Terminal-size behavior

The layout collapses below minimums: splits whose panes are all hidden
collapse entirely (the queue list fills the tab), the cava row drops while
a video plays, a "terminal too small" hint applies under the minimums,
and responsive sub-panes (`window_sizes`, currently the queue tab's
art+lyrics split) hide their whole box when they can no longer fit 4
content rows + their borders (the space is freed to the queue). Fixed-size
panes are never affected.
During resizes the window is kept blank until the 500 ms debounce. The
first post-resize frame is drawn **twice** (the second pass cleans up
first-pass artifacts); `Ui::resize` redraws the album art **before**
restarting cava (cava's resize is deferred until after the layout pass,
so its bars always land on top of the art, never under it).

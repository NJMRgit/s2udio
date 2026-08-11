---
title: "MPD Tab (Directories)"
section: tabs
doc_type: spec
id: "tabs/mpd-tab"
description: >
  The MPD browser: the jellyfin-style folder tree and shared-selection
  navigation — the right pane lists the current node's children, the root
  lists the top-level directories, and the tree collapses as you back out.
  Left pane: min 50 cols, hidden on TUIs ≤ 120 cols; `Enter` opens the
  context menu (parity with the Playlists pane).
status: "current"
updated: "2026-08-07"
source_files:
  - src/ui/panes/directories.rs
related:
  - tabs/jellyfin-tab
  - frontend/interaction
  - frontend/layout-templates
  - backend/stream-downloads
tags: [tab, directories, mpd, tree]
---

# MPD Tab (Directories)

## Identity

The library tab named `MPD`. It shares the Jellyfin tab's navigation
model — **folder tree left**, **items right**, **info box below** — with
the Library root kept as a fixed (never collapsible) row: `Library ↴`.

## Pane layout

The left folder-tree pane keeps a **minimum width of 50 columns** (the
right pane takes the remainder; the 30% proportional share is only a
floor, never below 50). On TUIs **120 columns wide or less the tree
pane is hidden entirely** — the MPD tab shows only the right pane, which
navigates normally: `a`/`←` still back out, and the root/folder actions
behave as usual. Nothing else in the key scheme changes; mouse events
over the (hidden) tree are inert because the tree never renders. The
same `tree_width` behavior applies to the Playlists and Jellyfin tabs'
left panes (shared helper). Below the tips strip the **info box takes
about two thirds of the pane height, capped at 15 lines** (exact-length
split; the tips strip stays a fixed 3 rows; on taller terminals the
file list gets the space above the cap).

## Shared-selection navigation

- The right pane always lists the **current node's children** — folders
  and songs one level deep (sorted with the configured directories sort;
  playlists never appear, the Playlists tab owns them). At the root it
  lists every **top-level directory**. Hidden directories (name starting
  with `.`, e.g. `.hist`) never appear — neither in the tree nor in the
  children list — and their whole subtree is skipped in `listall`.
- The tree highlight follows the right-pane cursor (a highlighted folder
  highlights its tree row; a song falls back to the enclosing node).
- Highlighting never expands anything and never lists files recursively:
  `d`/`→` open a folder (expand its tree path + show its children)
  or play a file (temporary `addid`+`playid` entry, hidden from the Queue
  list; dropped on song change / stop — see `backend/mpd-playback`).
  **`Enter` opens the context menu** (like right-click, parity with the
  Playlists songs pane and the queue lists).
- `a`/`←` back out one level: the parent's children show with the row we
  came from highlighted, and the **branch we left collapses**. At the
  root it is a no-op.
- Clicking a tree row selects it (the right pane shows its children);
  double-clicking a folder with subdirectories expands/collapses it; the
  Library root is never collapsible (`Library ↴`, no arrow).
- The wheel moves the highlight in both panes like `w`/`s`: over the
  right pane it moves the items cursor, over the tree it moves the tree
  highlight (selecting the folder under the cursor).

## Downloads folder

Stream downloads land in **`~/Downloads/s2udio-downloads`** — outside
the MPD library (see `backend/stream-downloads`). The tab still shows
the folder at the **top of the library, right under `Library ↴`**,
displayed as **`Downloads`**: the tree injects it as its first child
and the root right-pane list prepends the same entry; its contents come
from a **disk listing** (MPD cannot see the folder), re-read on every
open, and files there play through **mpv** (MPD cannot play outside
`music_directory`). Inside, the pane title reads
`Downloads`; the folder is empty until the first download.

## Rows and multi-selection (right pane)

- Directory rows render with a **▶ prefix**, songs with **no prefix** —
  the D/S symbol markers are not used on this tab.
- The right pane supports the queue tab's multi-selection: **ctrl+click**
  toggles a mark, **alt+click** range-marks from the anchor,
  `W`/`S`/`Shift+↑/↓` range-select (each press replaces the previous
  range), and a plain click on a different row clears the marks. Marked
  rows render with the lighter `marked_item_style` (the cursor row keeps
  the List's accent highlight). The marks reset whenever the children
  list changes (open, back out, database update).
- The file context menu acts on **every marked song** when any are
  marked (Add to queue / Replace queue / Create playlist / Add to
  playlist); a folder's menu always operates on its whole subtree.

## Context menus

- Right-click (or `Enter` / the context-menu key) on a **folder** — in
  the tree or the right pane — opens its whole-subtree menu: *Add folder
  to queue*, *Replace queue with folder*, *Create playlist from folder*,
  *Add folder to playlist* (the right-pane `find` is recursive:
  `Tag::File` + `StartsWith`).
- Right-click (or `Enter` / the context-menu key) on a **file** opens
  its menu: *Add to queue*, *Replace queue*, *Create playlist*, *Add to
  playlist*, *Cancel*.

## Info box

Shows the selected file's details (or the selected folder's name/path/
mtime) in the yellow-label format.

## Playback

`d`/`→`/double-click on a track plays it as a temporary entry
(never grows the queue); `Enter` opens the context menu instead.

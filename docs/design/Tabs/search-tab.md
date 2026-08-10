---
title: "Search Tab"
section: tabs
doc_type: spec
id: "tabs/search-tab"
description: >
  The re-laid-out search: filter inputs in a left pane, results list,
  tips strip and info box — the same structure as the other browsers.
status: "current"
updated: "2026-08-10"
source_files:
  - src/ui/panes/search/mod.rs
  - src/ui/panes/search/inputs.rs
related:
  - backend/mpd-playback
  - frontend/layout-templates
tags: [tab, search, filters]
---

# Search Tab

## Identity

Re-laid out to match the other browser tabs: the filter inputs live in a
left ` Search ` pane (**always visible**), with the ` Results ` list, a
3-line tips strip and a ` Info ` box (yellow group labels) stacked on the
right — the same structure as Directories / Radio / Jellyfin.

## Functionality

All search functionality is unchanged from the browser: filters
(case/diacritics options), rating/liked stickers, search modes, custom
query, results filtering, multi-select, context menus, enqueue/play.

## Interaction

- Clicking the results selects a row and moves keyboard focus there.
- Clicking a filter row focuses that input.
- Standard list navigation (`w`/`s`/`↑`/`↓`, `PageUp`/`PageDown`,
  `Enter`/`d`/`→`/double-click activate, `Del` delete, Esc/right-click
  back out).
- **Multi-select** (round 24): the results list supports the shared
  selection interactions — ctrl+click toggles a row's mark, alt+click
  range-marks from the anchor, a plain click on another row drops the
  multi-selection and re-anchors, `W`/`S`/`Shift+↑/↓` range-select;
  marked rows render with the marked highlight, the row under the mouse
  with the hover highlight (marked rows keep their marked highlight on
  hover), and Esc with an active selection clears it (a second Esc opens
  settings).
- **Dual-pane focus** (round 24): the pane holding the keyboard cursor is
  visually obvious — in the Search phase the focused filter input renders
  with the hover highlight; in the BrowseResults phase the results
  selection does (the other pane keeps the plain selection).
- The results list and info box use the tab-list palette (white names,
  `list_text_color` secondary, accent for the selection highlight).

## Info box

Shows the highlighted result's details in the yellow-label format;
mouse-scrollable.

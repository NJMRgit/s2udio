---
title: "Search Tab"
section: tabs
doc_type: spec
id: "tabs/search-tab"
description: >
  The re-laid-out search: filter inputs in a left pane, results list,
  tips strip and info box — the same structure as the other browsers.
status: "current"
updated: "2026-08-05"
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
- The results list and info box use the tab-list palette (white names,
  `list_text_color` secondary, accent for the selection highlight).

## Info box

Shows the highlighted result's details in the yellow-label format;
mouse-scrollable.

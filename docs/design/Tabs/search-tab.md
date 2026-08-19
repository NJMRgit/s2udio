---
title: "Search (MPD tab mode)"
section: tabs
doc_type: spec
id: "tabs/search-tab"
description: >
  The search UI: filter inputs in a left pane, results list, tips strip
  and info box — the same structure as the other browsers. Round 28:
  no longer a top-level tab — it folded into the MPD tab under the
  `⭘ Library  ● Search` toggle (still searches the user's MPD library).
status: "current"
updated: "2026-08-12"
source_files:
  - src/ui/panes/search/mod.rs
  - src/ui/panes/search/inputs.rs
related:
  - tabs/mpd-tab
  - backend/mpd-playback
  - frontend/layout-templates
tags: [tab, search, filters]
---

# Search (MPD tab mode)

## Identity

Round 28 (2026-08-12): the Search **tab** folded into the MPD tab — the
top-level tab bar is `Queue │ Playlists │ MPD • Jellyfin • Radio` and the
search UI lives under the MPD tab's `⭘ Library  ● Search` toggle (a
leftover "Search" tab entry in the config is hidden from the bar and the
tab cycle). The mode's UI is unchanged from the browser layout: filter
inputs in a left ` Search ` pane (**always visible**), with the
` Results ` list, a 3-line tips strip and a ` Info ` box (yellow group
labels) stacked on the right — the same structure as Directories / Radio
/ Jellyfin. Searches still query the user's MPD library; the search
state (filters, results, phase) lives for the session, surviving
Library↔Search toggles, and resets with the app (Library at startup).

## Functionality

All search functionality is unchanged from the browser: filters
(case/diacritics options), rating/liked stickers, search modes, custom
query, results filtering, multi-select, context menus, enqueue/play.

## Interaction (unchanged from the standalone tab)

- Clicking the results selects a row and moves keyboard focus there.
- Clicking a filter row focuses that input.
- Standard list navigation (`w`/`s`/`↑`/`↓`, `PageUp`/`PageDown`,
  `Enter`/`d`/`→`/double-click activate, `Del` delete, Esc/right-click
  back out).
- **Keyboard pane switching** (round 25): `d`/`→` move from the filter
  pane into the results list (BrowseResults); `a`/`←` return to the
  filters. In the results, `d`/`→` enqueue the highlighted result.
  (`w`/`s` keep moving the filter highlight / result selection; the keys
  resolve through the `directories` keybind context, so the config's
  existing `d`/`a`/`→`/`←` bindings apply.)
- **Options menu** (round 27): **Enter** in the populated results opens
  the right-click options menu (Play for the marked/selected set, Add /
  Replace queue, playlists, …); marked results get *Add to queue* and
  *Replace queue* (exactly the selected rows); **Space** with a
  multi-selection (≥2
  marked) also opens the menu instead of toggling playback — a single
  mark or none keeps Space on the transport.
- **Filter label spacing** (round 27): with the pane expanded the colons
  align one character past the longest rendered filter category with a
  space on each side (`Case sensitive : No`); narrow panes ellipsize the
  labels, keeping the value column visible.
- **Filter-row hover** (round 25): the filter/spinner/button rows are
  clickable fields and render with the hover highlight when the pointer
  is over them (the button/list-row treatment; keyboard input clears it).
- **Adaptive filter labels** (round 26): the label column pads to the
  aligned-colon width when the pane has room and ellipsizes (`…`) on
  narrow terminals, so the value column always keeps ≥ 10 cells (the
  settings-panel convention).
- **Multi-select** (round 24): the results list supports the shared
  selection interactions — ctrl+click is additive (the row under the
  cursor joins the selection, clicks only grow the marked set),
  alt+click range-marks from the anchor, a plain click on another row
  drops the multi-selection and re-anchors, `W`/`S`/`Shift+↑/↓`
  range-select, and **Ctrl+A** marks every result (the filter column is
  excluded); marked rows render with the marked highlight, the row under
  the mouse with the hover highlight (marked rows keep their marked
  highlight on hover), and Esc with an active selection clears it (a
  second Esc opens settings).
- **Dual-pane focus** (round 24): the pane holding the keyboard cursor is
  visually obvious — in the Search phase the focused filter input renders
  with the hover highlight; in the BrowseResults phase the results
  selection does (the other pane keeps the plain selection).
- The results list and info box use the tab-list palette (white names,
  `list_text_color` secondary, accent for the selection highlight).

## Info box

Shows the highlighted result's details in the yellow-label format;
mouse-scrollable.

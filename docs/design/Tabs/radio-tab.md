---
title: "Radio Tab"
section: tabs
doc_type: spec
id: "tabs/radio-tab"
description: >
  The radio browser: region tree, station list, info box, favourites and
  local stations, and selection preservation.
status: "current"
updated: "2026-08-05"
source_files:
  - src/ui/panes/radio.rs
  - src/radio/mod.rs
related:
  - backend/radio-directory
  - backend/mpd-playback
  - frontend/layout-templates
tags: [tab, radio, stations]
---

# Radio Tab

## Identity

3-pane browser: **region tree left** (`◎ Local` = 100 closest, then
countries), **station list of the selected region right**, **info box
below** (yellow group labels). Uses the tab-list white/grey palette with
the blur accent reserved for the selection highlight.

## Navigation

The radio tab uses the **MPD / Jellyfin / Playlists scheme**: one cursor
on the list in focus. The cursor starts on the **region tree** (left);
`d`/`→`/Enter expand a country (revealing its states) or **enter** a
leaf region (Favourites / Local / a state), moving the cursor to its
**station list** (right). `a`/`←` back out: on the station list the
cursor returns to the region tree; on the tree an expanded branch
collapses in place, otherwise the cursor moves up to the parent region,
collapsing the branch left.

- `w`/`s`/`↑`/`↓` all move the same (focused) list; `PageUp`/`PageDown`
  are not bound on this tab.
- `d`/`→` on a station play it; `Enter` on a station opens its context
  menu (like right-click); `d`/`→`/Enter on a region open it.
- Mouse: a click focuses the clicked pane, a double-click on a station
  plays it, right-click on a station opens the menu; the wheel moves the
  highlighted list's cursor.
- The tips strip under the station list shows the hints (w/s · ↑/↓,
  d / →, a / ← · Enter).

## Regions

- `◎ Local`: the 100 closest stations within 300 km (haversine
  client-side).
- Countries with **short display names** (`short_country_name`); the API
  name is kept for queries. The user's own country is pinned to the top.
- Provinces are the deepest level (no arrows); selecting one loads all its
  stations. All region/station caching rules in
  `backend/radio-directory`.

## Station rows

`★` favourites (the `radio.m3u` EXTINF list, cap 10) and `◎` local
markers. Rows show name + detail line (codec/bitrate/country/state/city)
in the list palette. The favourites playlist is **exclusive to radio
stations**: it is hidden from the Playlists tab, and every playlist
picker (`picker_playlists` — the queue/search/directories/playlist add
menus and the `s`-key save/delete modals) filters it out, so songs can
never be added to it.

## Playback

Playing a station = a temporary `addid`+`playid` entry (see
`backend/mpd-playback`), removed on stop/song-change; streams never fire
`SongChanged`. The Queue pane filters stream URIs out, so stations never
appear in the Queue tab.

## Selection preservation

Background data arrivals for the region on screen (directory refresh,
top-100 fills, state loads) **preserve the station selection** — the
cursor is restored by station, falling back to the same position, never
yanked to the top.

## Refresh

Settings → General → **reload radio stations** force-refreshes the whole
directory (ignoring the cache).

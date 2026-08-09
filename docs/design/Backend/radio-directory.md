---
title: "Radio Directory (radio-browser.info)"
section: backend
doc_type: flow
id: "backend/radio-directory"
description: >
  The Radio tab's data flow: directory fetch, region top-100s, lazy state
  loads, disk caching and force-reload.
status: "current"
updated: "2026-08-05"
source_files:
  - src/radio/mod.rs
  - src/config/radio.rs
  - src/ui/panes/radio.rs
  - src/core/work.rs
related:
  - backend/mpd-playback
  - tabs/radio-tab
tags: [radio, http, cache, radio-browser]
---

# Radio Directory (radio-browser.info)

## Flow overview

```
Radio tab shown
  → before_show: serve cached RadioDirectory (instant)
  → background FetchRadioDirectory (work thread, HTTP)
  → success: save cache + swap in-memory directory
  → failure: keep the cached directory, status notice
```

Base URL: `https://all.api.radio-browser.info/json`. All HTTP runs on the
work thread (`src/core/work.rs`) with `ureq` (5 s connect / 10 s read),
`rmpc/<version>` user agent. Local + directory requests run in parallel.

## Directory structure

- **◎ Local** = the 100 closest stations within 300 km
  (`geo_distance` is in **metres**, `has_geo_info=true`;
  `order=distance` does not exist). Distances are computed client-side
  (haversine over `geo_lat`/`geo_long`).
- **Countries** = top 100 per country from the global top-1000
  (`stations/search?order=votes&reverse=true&hidebroken=true&limit=1000`),
  grouped by country code. The user's own country gets its top 100 via
  `bycountrycodeexact/{cc}?limit=100` and is pinned to the top of the tree.
- **States** are the deepest level (no arrows). They are derived from a
  300-station sample (`json/states/{cc}` is broken — it leaked junk names,
  e.g. Brazil's states); Canada city names are normalized to provinces
  (`"Ottawa, ON"` → `Ontario`), comma/paren junk dropped and the
  "Unknown" bucket removed.

## Lazy loads & refresh rules

- Selecting a country fills its top-100 on first look
  (`FetchRadioCountryStations`); **expanding a country only fetches its
  states** — it never reloads the country's station list.
- Selecting a state re-fetches that state's stations
  (`countryExact` + `stateExact`, `limit=2000`).
- **Only the specific sub-region being highlighted refreshes**; the parent
  region's list is never reloaded. Plain views always serve the cache.
- Results carry their `(country, state)` so concurrent expands can't
  cross-apply stale data.
- Background arrivals for the region on screen **preserve the station
  selection** — the cursor is restored by station (falling back to the
  same position), never yanked to the top.

## Disk cache

- `~/.cache/s2udio/radio-directory.json` (round 23; legacy `~/.cache/rmpc/…` honored) — location, country_code, local
  list, countries with top-100s + lazy states, error.
- Saved **only on success**; a failed background refresh never overwrites
  it. The lazy-completed / refreshed lists are persisted too.
- Settings → General → **reload radio stations** re-fetches the whole
  directory (local + regions), ignoring the cache.

## DirectoryStation parsing

Fields consumed: `name`, `url` (`url_resolved` preferred), `country` /
`country_code`, `state`, `city`, `language[]`, `tags[]`, `codec`,
`bitrate`, `votes`, `geo_lat`/`geo_long`, `favicon`, `homepage`.

Long official country names are shortened for display
(`short_country_name`, e.g. `United States Of America` → `United States`);
the API name is kept for queries.

## Playback

Playing a station = a temporary `addid`+`playid` entry (see
`backend/mpd-playback`), removed on song change / stop. The Queue pane
filters stream URIs out.

## Favourites

`radio.m3u` (the MPD stored playlist, EXTINF format, cap
`radio.max_favourites` = 10); the Playlists tab hides it. Mutations rewrite
the `.m3u` directly (MPD playlist commands drop `#EXTINF`).

## Key events / requests

| Request | Trigger | Result |
| --- | --- | --- |
| `FetchRadioDirectory` | tab shown / settings reload | `RadioDirectory` |
| `FetchRadioCountryStations` | country selected | top-100 fill |
| `FetchRadioStateStations` | state selected | state's stations |

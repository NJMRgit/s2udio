---
title: "Jellyfin API"
section: backend
doc_type: flow
id: "backend/jellyfin-api"
description: >
  The Jellyfin integration: credential resolution, the endpoints the app
  calls, item parsing, stream URLs, playback reporting and resume.
status: "current"
updated: "2026-08-05"
source_files:
  - src/jellyfin/mod.rs
  - src/config/jellyfin.rs
  - src/ui/panes/jellyfin.rs
  - src/core/mpv.rs
related:
  - backend/mpv-session
  - tabs/jellyfin-tab
tags: [jellyfin, api, http, auth, video]
---

# Jellyfin API

## Flow overview

```
Jellyfin tab / video playback
  → resolve credentials (jellyfin.ron sidecar > jellytui config.toml)
  → authenticated requests (X-Emby-Token) on the work thread
  → parsed JfItem / image / resume data → panes + mpv session
```

## Credentials

- Default source: jellytui's `~/.config/jellytui/config.toml`
  (`server_url`, `access_token`, `user_id`).
- Override: a `jellyfin.ron` sidecar (takes precedence), written by
  Settings → jellyfin after a successful sign-in.
- Sign-in: `Jellyfin::authenticate(url, username, password)` → token +
  user id, staged until the panel closes with Save.
- Requests use `X-Emby-Token`; stream URLs pass `api_key` in the query.

## Endpoints

| Endpoint | Purpose |
| --- | --- |
| `GET /Users/{uid}/Views` | libraries (`CollectionType == "music"` → music view; video views sort first) |
| `GET /Users/{uid}/Items?ParentId={id}&Recursive=false&SortBy=SortName&SortOrder=Ascending&Fields=MediaSources` | direct children (lazy, every level on demand) |
| `…&IncludeItemTypes=MusicArtist&Recursive=true` | artists of a view |
| `…&ArtistIds={id}&IncludeItemTypes=MusicAlbum&Recursive=true` | albums of an artist |
| `…&ParentId={id}&IncludeItemTypes=Audio&Recursive=true&Fields=MediaSources` | songs |
| `GET /Users/{uid}/Items/{id}?Fields=Chapters,People` | full item (overview, credits, chapters) |
| `GET /Users/{uid}/Items/{id}/Images/Primary?maxWidth=…` | image bytes |
| `GET /UserItems/{id}/UserData` | saved resume position (seconds) |
| `POST /Sessions/Playing/Progress` | throttled to 10 s / on pause |
| `POST /Sessions/Playing/Stopped` | on exit |

## Item parsing (`JfItem`, `item_from_value`)

Reads: `Id`, `Name`, `Type`, `Overview`, `PremiereDate` → year,
`RunTimeTicks` (**10 ms units** → seconds), `ChildCount`, `SeriesId`,
`SeasonId`, `ParentIndexNumber`+`IndexNumber` (episode S/E),
`AlbumArtist`/`Artist`/`Album`, `People[]` (roles Director/Writer/
Starring), `MediaSources[]` → `AudioStream`/`SubtitleStream` languages
(ISO 639-1; `iso639_2_to_1` maps 3-letter codes).

`Type` values: `CollectionFolder` (library), `Series`, `Season`, `Episode`,
`Movie`, `Video`, `Audio`, `MusicArtist`, `MusicAlbum`, `Folder`.
`is_container` = MusicArtist | MusicAlbum | Folder | CollectionFolder |
Series | Season; playable = Audio | Movie | Episode | Video.
Chapters: `Chapters[]` `StartPositionTicks`/`EndPositionTicks` (10 ms).

## Stream URLs

- `GET /Audio/{id}/stream?static=true` — MPD plays the original file.
- `GET /Videos/{id}/stream?static=true` — the video's audio track via MPD,
  or mpv plays the video URL directly.

## Playback reporting & resume

- Jellyfin progress is reported back (`Sessions/Playing/Progress`
  throttled to 10 s or on pause, `…/Stopped` on exit).
- Replaying an item **resumes from the saved position**
  (`UserItems/{id}/UserData` → seek mpv, applied via the poll once the
  socket is up).
- The `s2u-mpv-tracker` daemon keeps reporting progress and applies resume
  once after s2udio exits (see `backend/mpv-session`).

## Threading

HTTP is confined to the work thread; the Jellyfin pane never blocks on the
MPD or render threads. Results carry item ids so stale arrivals can't
cross-apply.

---
title: "yt-dlp Stream Resolution"
section: backend
doc_type: flow
id: "backend/ytdlp-resolution"
description: >
  How YouTube / Soundcloud / NicoVideo links are classified, resolved to
  direct streams with yt-dlp, cached, and routed to playback.
status: "current"
updated: "2026-08-07"
source_files:
  - src/shared/ytdlp/stream.rs
  - src/shared/ytdlp/ytdlp_item.rs
  - src/ui/modals/paste.rs
  - src/core/work.rs
  - src/core/event_loop.rs
related:
  - backend/paste-pipeline
  - backend/mpv-session
  - backend/mpd-playback
  - backend/stream-downloads
tags: [yt-dlp, youtube, resolution, cache, streams]
---

# yt-dlp Stream Resolution

## Flow overview

```
YouTube-style link
  → classify (YtDlpContent) → PastedItem::Yt
  → ResolveYtStreams (work thread, one yt-dlp subprocess per URL)
  → YtStreamsResolved { info, action, failures }
  → apply_resolved_streams: store info (memory + disk cache), route by action
```

## The command

```
yt-dlp -J -f bestaudio/best --no-playlist --no-warnings -- <url>
```

- One subprocess per URL, on the work thread.
- The binary is `yt-dlp` from `PATH`, overridable via the
  `S2UDIO_YTDLP_BIN` environment variable (unit tests use a fake script).
- `-J` emits the full info JSON; the top-level `url` is the selected
  format's direct stream (googlevideo for YouTube).

### Client strategy (`s2u-yt` wrapper, host-side)

On this machine `yt-dlp` on `PATH` is the **s2u-yt wrapper**, which makes
**two attempts** per call (see `s2u-yt/` in the repo — installed via
`./install.sh`; `status.sh` health-checks it):

1. **Anonymous `android_vr`** — cookie options are stripped from
   `~/.config/yt-dlp/config` and the call runs with `--ignore-config`
   (the default config would leak cookies back in; yt-dlp **skips**
   `android_vr` when the session is authenticated). PO tokens come from
   the bgutil provider (systemd user unit `s2u-yt-bgutil.service`; its
   GVS policy requires them). On this bot-checked IP it is the only
   client whose googlevideo URLs are accepted (HTTP 200/206).
2. **Authenticated `web_safari`** (retry on failure) — HLS up to 1080p,
   the previously verified-good path.

**Consequences for playback:**

- **Video** (mpv): android_vr serves DASH up to **2160p** (itag 313 vp9 /
  401 av01) instead of web_safari's 1080p HLS ceiling.
- **Audio** (MPD): android_vr resolves `bestaudio` to a **single-file
  DASH URL** (opus itag 251 / m4a 140, `c=ANDROID_VR` videoplayback),
  which MPD **range-seeks** (HTTP Range) — stream seeking works again.
  The fallback web_safari URL is an **HLS manifest**, which MPD plays but
  cannot seek (`seekcur` → `ACK … Not seekable`); that path only appears
  when the anonymous pass fails (check `status.sh` / bgutil).
- The resolved-URL keying of `yt_info`/chapters is unchanged; a re-resolve
  (expired URL, restart) yields a fresh single-file URL for the same
  video info.

Risk note: if YouTube extends the SABR experiment to `android_vr` (it
currently strips URLs on the plain `android` client, yt-dlp #12482), the
web_safari fallback keeps playback working at 1080p.

## Classification (`YtDlpContent::from_str`)

| Host | Accepted forms |
| --- | --- |
| `youtube.com` | `/watch?v=…`, `?list=…` (playlist) |
| `youtu.be` | `/<id>` |
| `soundcloud.com` / `api.soundcloud.com` | `/user/track`, `/tracks/<id>` |
| `nicovideo.jp` | `/watch/<id>` |

Anything else → `PastedItem::Url` (direct http(s)) or nothing.

## `YtStreamInfo` parsing

`url` (direct stream), `original_url`, `title`, `channel`,
`subscribers` (`channel_follower_count`), `thumbnail`, `description`,
`chapters`.

Chapters are also **derived from the description**: lines matching
`MM:SS Title` (`chapters_from_description`), sorted, deduped, clamped to
the duration.

## Cache lifecycle

- Resolution inserts into the in-memory `ctx.yt_info` **and** the disk
  cache (`~/.cache/rmpc/yt-info.json`), keyed by the **resolved URL and
  the original link** (an mpv session plays the original link and looks
  the info up by it).
- Startup seeds the in-memory map from the cache and re-resolves the
  still-playing stream (`YtAction::Refresh`).
- **Neither map is ever cleared wholesale** — the info of the stream
  currently playing must survive the session (the disk cache has its own
  bound).
- Googlevideo URLs expire: info keyed by both URLs + the disk cache make
  restart/restore and title/thumbnail display survive expiry.

## Result routing (`YtAction`)

| Action | Behavior |
| --- | --- |
| `Play` | temporary `addid`+`playid` (MPD), hidden from the queue |
| `PlayVideo` | mpv on the **original links** with resolved titles; failed links still launch raw (mpv resolves them) |
| `AddAfterCurrent` / `Append` | queue the resolved stream URLs |
| `AddToVideoQueue` / `AppendVideoQueue` | persistent video playlist, keyed by original link |
| `Refresh` | re-resolve the still-playing stream at startup |

Failures are reported as status notices
(`Failed to resolve stream: <url>: <reason>`); a failed resolution never
aborts the rest.

## Consumers of the resolved info

- Now-playing line (video **title**, not the URL), the lyrics/info box
  (Title / Channel / Description — the queue tab shows the **same
  video-style layout** whether the stream plays as audio through MPD or
  as a video in mpv, via `paste::current_yt_info`), the album art (video
  **thumbnail**), the Chapters tab, the mpv session metadata, and the
  MPRIS poster (`SaveMpvMprisArt`).
- The resolved stream URL is keyed in `ctx.yt_info`, so its queue entry
  stays **visible** in the Audio list (unlike radio/Jellyfin temp
  streams) and renders the cached title/channel in the row.
- **Downloads**: a stream row (queue Audio / queue Video / playlists tab)
  or the controls bar's `⤓` button can save the media into
  `s2udio-downloads` in `~/Downloads` — the download always uses
  `info.original_url` and
  the resolved `chapters` drive the per-chapter save option (see
  `backend/stream-downloads`).
- **All these lookups are keyed by the playing URL** (the mpv playlist
  entry's original link, or the queue song's resolved stream URL): an
  mpv reattach that loses the playlist therefore blanks them all — the
  reattach recovers the entries from mpv itself when the state file is
  stale (see `backend/mpv-session`).

---
title: "Torrent Streaming (drag&drop .torrent / magnet)"
section: backend
doc_type: plan
id: "backend/torrent-streaming"
description: >
  Plan for streaming torrents from drag&dropped .torrent files / magnet
  links: engine choice, classification, bandwidth gate, mpv routing and
  session lifecycle.
status: "in-progress"
updated: "2026-08-15 (round 42: web UI + SOCKS5 proxy)"
source_files:
  - src/ui/modals/paste.rs (classify / parse_paste / popup)
  - src/core/mpv.rs (run_mpv_playlist / MpvPlaylistEntry)
  - src/core/work.rs (work thread)
  - src/config/mod.rs (config section)
related:
  - backend/paste-pipeline
  - backend/ytdlp-resolution
  - backend/mpv-session
tags: [torrent, magnet, streaming, bandwidth, rqbit]
---

# Torrent Streaming — Plan (proposed)

## 1. Goal & scope

Dropping a `.torrent` file or a `magnet:` link into the s2u TUI should
offer to **stream** the torrent: pick the media file, download pieces
on demand, and play via mpv — with an explicit **bandwidth gate** before
playback starts (no play when the peer speed is below a threshold).

In scope (v1):

- Classification: local `.torrent` files, `file://` .torrent URIs,
  `http(s)` URLs ending in `.torrent`, `magnet:` links.
- A `[Torrent]` section in the paste popup with **Play (stream)**.
- Engine process management (rqbit server, see §4), localhost-only.
- Speed verification: live stats modal → gate → dialog
  (Retry / Play anyway / Cancel).
- Multi-file selection (auto-pick largest playable file, picker when
  ambiguous).
- Playback via mpv (existing video session), now-playing info, cleanup
  when the session ends / app exits.

Out of scope (v1, listed for later):

- Torrent **audio** through MPD (seeking in a partially downloaded file
  is unreliable for MPD; mpv handles it). Torrents play via mpv
  regardless of media type.
- "Add to queue" for torrents (a queued magnet would need lazy resolve
  when reached — complex; revisit later).
- Keeping the whole torrent (subtitles, poster, extras) — the
  "Play and Download" action (§4.1) keeps only the picked media file in
  `s2udio-downloads` (`~/Downloads`); a "keep everything" variant is a
  later option.
- Mid-play bandwidth monitoring / stall warnings (enhancement, §11).

## 2. Research summary — engine choice

| Engine | Runtime | Streaming | Speed stats API | Notes |
| --- | --- | --- | --- | --- |
| **rqbit** (`ikatson/rqbit`) | single static Linux binary (~22 MB, v9.0.0-beta.2, Jan 2026) | built-in HTTP streaming server, Range support, on-demand piece selection | REST API: `GET /torrents/{id}/stats/v1` → `live.download_speed` (bytes/s) | magnet + DHT (BEP-5) + PEX (BEP-11), BEP-53 file selection, `only_files_regex`. Actively maintained. **Recommended.** |
| webtorrent-cli (`webtorrent/webtorrent-cli`) | Node.js (v26.4.0 present on this machine) | HTTP server, on-demand pieces | parse stdout / no clean stats API | Less clearly maintained; needs `npm i -g`. Documented fallback. |
| peerflix / torrent-stream | Node.js | HTTP streaming | none (outdated) | legacy, unmaintained |
| aria2c | single binary | no HTTP server; play the partial file path | RPC stats | no on-demand piece serving; mpv would block on un-downloaded ranges |

**Decision: rqbit** — same philosophy as the existing `s2u-yt` wrapper
(contained external tool, subprocess + localhost HTTP, env-overridable
binary), no new runtime on this machine, purpose-built streaming with a
proper stats API for the bandwidth gate.

Zero new Rust dependencies: `ureq` (already a dependency, json feature)
does the REST calls on the work thread; `std::process` spawns the engine.

### rqbit API facts (verified 2026-08-08 against the installed binary)

Open question 2 is **RESOLVED** — facts below were verified live against
`rqbit 9.0.0-beta.2` (static binary at `~/.local/bin/rqbit`, md5
`aec1c9f3`; GitHub release `ikatson/rqbit` v9.0.0-beta.2). Note: the
container's original M1 code assumed an older API (flag after `server
start`, bare-string add response, `size`/`id` file fields, f64
`download_speed`, `DELETE` verb) — the host aligned `src/core/torrent.rs`
+ fake fixtures to the real v9 shapes below.

- Server: `rqbit --http-api-listen-addr 127.0.0.1:<port> server start
  <download-folder>` — **`--http-api-listen-addr` is a GLOBAL option**
  (before the subcommand) since v9; `server start` rejects it after
  `start` (exit 2). Default when unset: 127.0.0.1:3030.
- Add torrent: `POST /torrents` with raw magnet string / http .torrent
  URL / binary .torrent body → response is an **object**
  `{"id": <numeric>, "details": {...}, "output_folder": …}` (older
  builds returned a bare string id). Query params: `only_files_regex`,
  `list_only`, `overwrite`.
- Details: `GET /torrents/{id}` → `{id, info_hash, name,
  output_folder, total_pieces, files[]}`; each file is `{name,
  components[], length, included, attributes}` — **no per-file `id`/
  `size`**; the stream endpoint addresses files by their **positional
  index** in `files`.
- Stats: `GET /torrents/{id}/stats/v1` → `{state, file_progress,
  progress_bytes, total_bytes, finished, uploaded_bytes, error,
  live: Option<LiveStats>}`; `live.download_speed` is an **object**
  `{mbps, human_readable}` (e.g. `"0.00 MiB/s"`), **not** a bare
  bytes/s number — the M3 gate converts `mbps` → KB/s (×1000/8).
- Delete: `DELETE /torrents/{id}` → **405**; use
  `POST /torrents/{id}/delete` (removes files) or
  `POST /torrents/{id}/forget` (keeps files).
- Stream URL: `http://127.0.0.1:<port>/torrents/<id>/stream/<file_idx>`
  — Range-capable (verified: `206`, `Accept-Ranges: bytes`,
  `Content-Range`); "streams prioritize pieces being streamed and block
  until pieces are available" (seeking within downloaded range works,
  seeking ahead triggers on-demand fetch).
- Auth: `RQBIT_HTTP_BASIC_AUTH_USERPASS=user:pass` env var → we send a
  random token at spawn and include `Authorization: Basic` in requests
  (defense in depth on 127.0.0.1). Verified: no/wrong credentials →
  401; correct → 200. `GET /stats` (the readiness probe) answers 200
  once the API is up.

## 3. Architecture overview

```
drag&drop .torrent / magnet
  → parse_paste / classify → PastedItem::Torrent / Magnet
  → paste popup [Torrent] section → Play (stream)
  → ensure rqbit server running (work thread, one-off)
  → POST /torrents → id; GET /torrents/{id} → files → pick target file
  → speed gate (poll stats, progress modal)
      ├─ speed ≥ min → play now
      └─ timeout → dialog: Retry / Play anyway / Cancel
  → mpv on http://127.0.0.1:<port>/torrents/<id>/stream/<file_id>
  → TorrentSession state in Ctx (engine pid, base url, id, stream url)
  → MpvSessionEnded / app exit → DELETE /torrents/{id}, kill engine
```

New module `src/core/torrent.rs` (or `src/shared/torrent/`): engine
spawn/kill, REST calls (ureq), file picking, speed-gate logic — unit
tested with a fake engine script via `S2UDIO_RQBIT_BIN` (mirrors the
fake yt-dlp pattern in `src/shared/ytdlp/stream.rs`).

## 4. Classification (paste pipeline)

Implemented in M2 (`src/ui/modals/paste.rs`). `PastedItem` has two
torrent variants:

- `Magnet(String)` — a `magnet:` link (checked before the generic
  http(s) branch); `magnet_infohash` extracts `xt=urn:btih:`/`btih:`
  for labels (first 8 chars) and dedupe (two magnets for the same
  infohash with different trackers are one item).
- `Torrent(String)` — a local `.torrent` path (must exist), a
  `file://` .torrent URI (stripped + unescaped), or an `http(s)` URL
  ending in `.torrent` (query/fragment stripped for the check). The
  audio/video extension lists are never consulted for torrents.

Popup: a `[Torrent]` section (dim, non-selectable header, same style as
`[Audio]`/`[Video]`). **Round 17+18 — the section is scan-driven**: when
the popup opens, each pasted torrent/magnet shows a dim non-selectable
**Loading <label>…** row (the existing item label: infohash prefix /
file name) while a background scan runs — `WorkRequest::ScanTorrent` →
engine start + `POST /torrents` + the metainfo wait + `torrent_details`
→ the running engine, the torrent's id/name and the full file list (name
+ length + positional index) as `WorkDone::TorrentScanned`. The result
lands in `Ctx.torrent_scans` (keyed by the item's source string) and the
popup is refreshed **in place** (a rebuilt popup with the `paste_modal`
replacement id replaces the open one) with the play actions the file
list enables:

**Round 18 — the metainfo wait is open-ended and user-controlled (no
deadline).** A magnet's file list only appears once peers delivered the
metainfo, which can take a long time on cold DHT/trackers (big season
packs — the SNW S03 live finding). The scan must not give up after N
seconds; it waits until the metainfo arrives or the user cancels. The
wait runs on a **dedicated scan thread per item** (spawned by the work
thread), so the work thread is never blocked by a slow magnet (album
art, yt-dlp, downloads and other scans keep working). While waiting, the
Loading row becomes a live **wait window** that refreshes once per
second with `WorkDone::TorrentScanProgress` (the scan thread sends it
from `GET /torrents/{id}/stats/v1` → `live.download_speed.mbps`, shown
as KB/s):

```
Loading <label>… 00:12
esc to cancel
```

- **Elapsed counter** — whole seconds since the scan started (`mm:ss`).
- **No speed row** (round 20): the original wait window also showed
  `DL <speed> · need ≥ <min> KB/s ✓/✗` (live speed vs
  `config.torrent.min_download_speed_kbps`, default 500 KB/s), but live
  use showed it is noise during the metainfo wait — the speed is ~0 while
  nothing is downloading, so it always read `✗`. The
  `TorrentScanProgress.download_speed_kbps` value still flows (the M3
  bandwidth gate reads it before playback); only the wait-window
  rendering drops it. (Refining per-file from the torrent's own bitrate
  once metainfo arrives is a possible follow-up, not required here.)
- **esc to cancel** — Esc closes the paste popup; the popup's close hook
  fires each in-flight scan's cancel channel (`Ctx.torrent_scan_cancels`,
  keyed by the item's source key), the scan thread aborts the wait and
  drops the engine (rqbit killed — no orphan process), and the popup's
  scan state (`torrent_scans`, `torrent_scans_pending`,
  `torrent_scan_progress`) is cleared so a late result cannot reopen it.
- Multi-item pastes: one wait block per item, each with its own key /
  cancel channel / progress — cancelling one does not cancel the others.
- The `max_wait_secs` stopgap (round-17 wiring) is obsolete: the wait is
  cancellable and open-ended. The config field stays as a relic.
- **The scan only completes once the torrent is `live`** (round-18 host
  finding 2026-08-09): a re-added torrent whose files already exist in
  the cache is adopted with `?overwrite=true` and spends some seconds in
  rqbit's `Initializing` state (checksum-validating the existing files)
  — during that window the stream endpoint errors, so the wait window
  keeps ticking until `stats/v1.state == "live"` before the play actions
  appear.
- Failure handling (missing engine binary, spawn failure, …) still shows
  the dim notice row; only the metainfo wait lost its deadline.

**Engine isolation (round-18 host fixes 2026-08-09)**: each engine is
spawned with `--disable-persistence`, a per-engine free `--listen-port`
(rqbit's default peer port 4240 collides across engines) and
`--disable-dht-persistence`. Without `--disable-persistence` every engine
restored the shared rqbit session DB (all previously added torrents) and
checksum-validated them at startup, keeping the added torrent
`Initializing` (streams 500 → mpv exit 2). s2udio manages torrents itself
(add → play/download → delete), so the clean session is correct; the add
POSTs `?overwrite=true` so a re-added torrent adopts its existing cache
files instead of a 400 "File exists".

The scan-enabled play actions:

- No playable media (a data torrent) → dim `No playable media in this
  torrent` row, no action.
- One playable file, or an audio-only torrent → **Play (stream)** and
  **Play and Download** (§4.1), unchanged.
- More than one video file (season packs) → **Play (stream)** (the
  largest video, today's behavior), **Play all (N files)** (every video,
  one sequential playlist) and **Select files…** (§7) — multi-file
  download stays out of scope (M4).
- Scan failure (dead magnet, missing engine binary, …) → dim notice row
  with the error (e.g. `No peers found — is the torrent alive?`), no
  action.

Several torrent items in one paste: each is scanned and its rows carry
its label; the popup refreshes as each scan lands (a superseded scan is
dropped via the item-keyed map). When `torrent.enabled: false` the
section shows a dim non-selectable `Torrent streaming disabled` row
instead (no scans). The `[Audio]` section operates on the audio-capable
items only and `[Video]` on the video items (mixed pastes keep their
sections); torrents never fall into `[Audio]`/`[Video]` as
`Url`/`VideoUrl`, and a pure-torrent paste shows only `[Torrent]` +
Cancel.

**Engine reuse (round 17)**: the play actions reuse the scanned engine +
torrent id — `AppEvent::TorrentScannedPlay` → `start_torrent_playback`
in the event loop (which owns the download-job scheduler guard) — instead
of spawning a fresh rqbit per play. When the scan is gone by the time an
action runs (replaced engine, popup closed), the action falls back to the
fresh path: `WorkRequest::PlayTorrent { download }` → engine start + add
+ the same **open-ended** metainfo wait (round 18: no deadline — on its
own thread so the work thread is never blocked) + `pick_playable_file` →
`WorkDone::TorrentStreamPrepared` → mpv on the stream URL, engine kept in
`Ctx.torrent_engine`.

### 4.1 Play and Download (keeps the picked file)

Offered only when the scan found a single playable file (one video, or
the largest audio of an audio-only torrent); the scanned engine is reused
(or the fresh `PlayTorrent` path when the scan is gone).

`PlayTorrent { download: true }` — the same prep, but the UI then keeps
a **download job** (`Ctx.torrent_download`) that polls the engine's
`GET /torrents/{id}/stats/v1` once per second. rqbit downloads *all*
included files (no `only_files_regex` yet — M4); completion is
`stats.finished` **and** `file_progress[file_idx] == file.length`
(rqbit preallocates files on disk, so on-disk sizes are not a progress
signal). On completion:

- **mpv still playing the stream** → mark the job complete+deferred;
  the move happens on `MpvSessionEnded` (moving the file away — and
  deleting the torrent — mid-playback would break the stream).
- **otherwise** (or at session end for a deferred job) →
  `finish_torrent_download`: move the picked file from
  `<torrent.cache_dir>/<torrent name>/<file name>` into
  `~/Downloads/s2udio-downloads` (unique-name collision handling;
  copy+remove fallback across filesystems), `POST /torrents/{id}/delete`
  (removes the leftover cache files), status "Downloaded '<file>' to
  s2udio-downloads".

Job lifecycle: a new torrent play replaces the engine, which abandons
the job (the download died with it; partials stay in the cache until
app exit — the M4 keep/cleanup policy refines this). App exit with a
download in flight: same as any engine — the child is killed, cache
files remain (`keep_after_play`/cleanup is M4).

## 5. Engine bootstrap (M1)

- `ensure_torrent_engine(ctx)`: if no rqbit child is running, spawn
  `rqbit server start <cache_dir>` with:
  - `--http-api-listen-addr 127.0.0.1:<port>` (port from config,
    fallback: scan port+1..+20 if bind fails),
  - env `RQBIT_HTTP_BASIC_AUTH_USERPASS=<random>`,
  - env `RQBIT_LOG=warn`-style quiet logging; stderr → log file
    (`/tmp/s2u-rqbit-stderr.log`, like the mpv one),
  - cache dir `~/.cache/s2udio/torrents` (round 23; legacy `~/.cache/rmpc/torrents` honored) (created first; rqbit requires
    the download folder to exist).
- Wait for `GET /stats` to succeed (poll ≤ 5 s) before use.
- Binary from `S2UDIO_RQBIT_BIN` env var, default `rqbit` from PATH
  (mirror `S2UDIO_YTDLP_BIN`). If missing at first use → status notice:
  "rqbit not found — install (cargo install rqbit / static binary) or
  set S2UDIO_RQBIT_BIN" and abort the action, nothing else breaks.
- Round-18 fixes (implemented): each engine gets its own free peer
  `--listen-port` (default 4240 would collide across concurrent engines)
  and `--disable-dht-persistence` (ephemeral DHT port); every engine runs
  `--disable-persistence` (`server start` option, AFTER the subcommand)
  for a clean per-engine session — the shared session DB made re-added
  torrents checksum-validate at startup (stuck `Initializing`, stream
  endpoint errors, mpv exit 2). `--http-api-listen-addr`,
  `--listen-port` and the proxy flags are GLOBAL options (BEFORE
  `server`).
- Round 42: the engine also serves the **rqbit web UI** at
  `GET /web/` on the HTTP API port (verified against 9.0.0-beta.2; the
  older `/ui` path is gone). The UI is auth-protected like every other
  endpoint. **Round 42 follow-up (2026-08-15)**: the UI is an SPA whose
  `fetch()` calls cannot authenticate — browsers do NOT replay
  URL-userinfo basic auth on `fetch()` (verified with headless
  Chromium: the page loads via `http://user:pass@host/web/` but every
  API call fails, UI shows "Error refreshing torrents network error" /
  401). So every engine now also spawns a tiny loopback
  **auth-injecting reverse proxy** (`src/core/torrent_proxy.rs`,
  std-only, `Connection: close` per request) that adds the
  `Authorization` header to every forwarded request; `web_url()` points
  at `http://127.0.0.1:<proxy port>/web/` (no credentials in the URL)
  and the engine port itself stays auth-protected. Verified
  end-to-end: real rqbit + proxy + headless Chromium loads the SPA;
  the engine's own `/stats` still 401s without credentials.
- Round 42: when `torrent.socks_proxy` is set, the spawn adds the GLOBAL
  `--socks-url <proxy>` and `--disable-tcp-listen` flags — ALL outgoing
  connections route through the SOCKS5 proxy (the VPN route) and the
  engine stops listening for incoming connections (the proxy only does
  outgoing, so listening would leak the real IP; rqbit's own
  recommendation).
- Check (implementation detail): whether rqbit accepts port 0 for an
  ephemeral port; if yes, prefer it and read the port from the server's
  stdout/state; otherwise the config port + fallback scan.

## 6. Bandwidth gate (the "verify speed before play" requirement)

```
play_torrent(item):
  ensure_engine()
  POST /torrents  (magnet → raw string; http .torrent → URL;
                   local file → binary body, --data-binary equivalent)
  id = response id
  files = GET /torrents/{id}
  target = pick_file(files)        # §7
  deadline = now + max_wait_secs
  while now < deadline:
    stats = GET /torrents/{id}/stats/v1   (every 500 ms)
    update progress modal: state, speed, peers*, % of target file
    if stats.live.download_speed ≥ min_speed
       and stats.progress_bytes > 0:      # first bytes actually arrived
      break → play
    sleep 500 ms
  if not played:
    if metadata never arrived (no file list usable / zero progress
        after no_peers_timeout):  → error notice "No peers found — is
        the torrent alive?" and cleanup; no dialog
    else:  dialog "⚠ Stream too slow — X MB/s (need ≥ Y)
           [Retry] [Play anyway] [Cancel]"
           Retry → restart the gate with a fresh deadline
           Play anyway → straight to play
           Cancel → cleanup (DELETE torrent)
```

- Speed source: `stats.live.download_speed` (bytes/s). Use the **median
  of the last 3 samples** (≤ 1.5 s) to smooth out peer churn, not a
  single instant sample.
- Threshold: config `min_download_speed_kbps` (default **500 KB/s** —
  comfortably above typical 720p–1080p encodings; torrent metadata has
  no duration so bitrate-relative gating is impossible).
- Defaults: `warmup_secs: 5` (minimum sampling window), `max_wait_secs:
  15` (hard gate deadline), `no_peers_timeout_secs: 30` (dead-magnet
  abort). All configurable.
- While waiting, the modal shows live numbers (state, speed, bytes, and
  peers via `GET /torrents/{id}/peer_stats` count when available) so the
  user sees *why* it's waiting, not a spinner.

## 7. File selection (multi-file torrents)

Round 17 (the up-front scan makes the file list available in the popup):

- **Play (stream)** picks the largest **video** extension file; when the
  torrent has no videos, the largest **audio** extension file (the
  `is_video_extension` / `is_audio_extension` lists from paste.rs).
- **Play all (N files)** (shown when the scan finds ≥ 2 video files) —
  every video file in scan order, one `MpvPlaylistEntry` per file.
- **Select files…** (same condition) — a multi-select modal
  (`src/ui/modals/torrent_file_picker.rs`) listing the video files sorted
  by name with human-readable sizes. Controls (the app's minimal keybind
  set binds Space to TogglePause globally, so the picker claims Space at
  the raw-key level): **Space** marks/unmarks the highlighted file,
  **Enter** plays the marked files right away (all of them when none is
  marked), **Esc** cancels; the Confirm/Cancel buttons stay reachable via
  the wrap-around navigation and the mouse (click toggles).
- Every action keeps each file's **positional index**
  (`ScannedFile.index` — the rqbit stream endpoint addresses files by
  index, so the stream URL is `engine.stream_url(&id, index)`).
- No playable file (a data torrent) → dim `No playable media in this
  torrent` row, no action.
- Multi-file "and download" is out of scope (M4): **Play and Download**
  stays single-file only.
- Optimization (future, M4): when the pick is known before download
  starts, re-add with `only_files_regex` matching the chosen file so the
  engine only downloads it (avoid filling the cache with the rest of the
  torrent).

## 8. Playback routing (mpv)

- Play = `play_video_entries` with **all** of the action's files as one
  `MpvPlaylistEntry` per file (`title = <file name>`, `url = <stream
  URL>`, duration `None`) — the same path a Jellyfin season play uses:
  the Queue tab's Video list, mpv's playlist, the poll's
  advance/position tracking and the MPRIS titles work for free. While a
  torrent plays, the Queue tab's Video view shows the mpv session's
  playlist (`mpv::session_playlist_shown` — a torrent stream URL in the
  session playlist), like it does for a Jellyfin item; the persistent
  video playlist is left untouched and returns when the session ends (its
  rows cannot host the token-bearing stream URLs).
- **The stream URL embeds the engine's auth token as URL userinfo**
  (`http://s2u:<token>@127.0.0.1:<port>/torrents/<id>/stream/<idx>` —
  `TorrentEngine::stream_url`) because rqbit enforces basic auth on the
  stream endpoint and mpv cannot send a custom `Authorization` header.
  Verified live against rqbit 9.0.0-beta.2: no credentials → 401, URL
  userinfo → 206 with `Accept-Ranges`.
- Torrent streams get extra mpv args (cache for slow/stalling networks):
  `--cache=yes --demuxer-readahead-secs=30 --demuxer-max-bytes=128MiB`.
  Implementation: either a per-entry marker or detect the localhost
  stream URL in `run_mpv_playlist`; keep the non-torrent path byte-for-
  byte identical.
- Now-playing info: `start_torrent_playback` inserts a **synthetic
  `ctx.yt_info` entry per stream URL** (title = file name, channel =
  torrent name) — the queue row, info box and MPRIS title then work for
  free (`paste::current_yt_info` / `mpv_yt_info` lookups; no
  thumbnail/chapters). The entries are **in-memory only**: the stream URL
  embeds the auth token, so nothing is ever written to yt-info.json.
- The volume/audio/subtitle preference chains from `run_mpv_playlist`
  apply unchanged.

## 9. Session lifecycle

- `Ctx` gains a `TorrentSession` (engine child handle, base URL, auth
  token, active torrent id → stream url map, poll state).
- Poll: reuse the existing work-thread/event pattern; the stats poll
  runs only while a gate modal is open; no steady-state polling once
  playing (mpv's own cache handles buffering; stall-watch is an
  enhancement).
- Cleanup triggers:
  - `MpvSessionEnded` while the last mpv entry was a torrent stream →
    `DELETE /torrents/{id}`, remove the entry from `yt_info`,
  - engine idle (no active torrents) for `keep_after_play` (default 0 s
    → immediate kill) → kill child,
  - app exit (any path) → kill child + delete cache dir contents
    (streaming cache, not user downloads; `keep_after_play: true` keeps
    the partial files for reseeding until next launch).
- Restart/restore: torrent streams are ephemeral — the persistent video
  playlist is **not** involved (magnet entries are not persisted).

### 9.1 Engine reuse & scan identity (round 20 — duplicate-paste fix)

- **Canonical scan keys**: a magnet's `Ctx.torrent_scans` /
  `torrent_scans_pending` / `torrent_scan_cancels` / `torrent_scan_progress`
  key is its **full infohash** (`magnet_infohash_full`, lowercased), not
  the raw magnet URI — so the same torrent pasted twice, even via a
  different magnet URI (extra trackers), hits the same scan slot. The UI
  key (`torrent_item_key`) and the work thread's `TorrentItem::source_key`
  agree. `.torrent` items keep their path/URL key.
- **Landed scans survive the popup**: the paste popup's close hook keeps
  `Some(Ok(scan))` entries (only failed scans are dropped, so a re-paste
  retries cleanly). The scan's engine is shared via `Arc<TorrentEngine>`:
  playback clones the Arc into `Ctx.torrent_engine` and the scan map keeps
  its own clone. A repeat paste of the same torrent therefore reuses the
  existing engine + file list (instant actions, no second rqbit against
  the same cache dir — the "pasting the same magnet twice errors" bug).
  The fresh `PlayTorrent` fallback registers a single-file scan under the
  item's canonical key for the same reason.
- **Interim lifecycle**: engines of scanned-but-unplayed torrents stay
  alive (and keep downloading) until the app exits — the full
  keep/cleanup policy is still M4. Played engines keep their scan-map
  clone too, so re-paste after play reuses the same engine.

### 9.2 Web UI & VPN routing (round 42 — Settings → torrent)

The **Settings panel's `torrent` section** manages a **standalone web-UI
engine** independent of the per-play engine (`Ctx.torrent_webui_engine`,
a plain `RefCell<Option<TorrentEngine>>`; its `Drop` kills rqbit when
the app exits):

- **`web ui`** row: starts the standalone engine when none is running
  (dead child = not running; blocking spawn ≤ 5 s readiness — the panel
  is modal, like the Jellyfin sign-in) and opens the browser via
  `xdg-open` on `TorrentEngine::web_url()` — the auth-injecting proxy
  URL, so no credentials appear in the address bar and the SPA's
  `fetch()` calls work. Row label flips `[start]` ↔ `[open]` with
  engine liveness.
- **`stop engine`** row: `take()`s the engine (Drop kills rqbit) — a
  fresh start is how a changed SOCKS proxy takes effect on a running
  engine.
- **`socks proxy`** row: edits a `socks5://[user:pass@]host:port` URL in
  an `InputModal`; staged like the other settings rows, applied to
  `config.torrent.socks_proxy` + persisted to `state.ron`
  (`torrent_socks_proxy`, "" = explicitly no proxy) on Save, restored at
  startup in `main.rs`. Takes effect on the NEXT engine spawn (any
  engine — play/download scans too, not just the web UI).
- The built-in web UI (9.0.0-beta.2) is torrent management only — it has
  **no proxy/VPN settings** (verified in its embedded JS); the SOCKS
  route is configured here and applied at spawn.

### 9.3 Shell control — `s2udio rq start|stop|open` (round 43)

The standalone engine can also be driven from the shell, sharing ONE
engine with the Settings panel through a registration file
(`~/.cache/s2udio/rqbit.json`: pid + web URL; no auth token — the proxy
URL only):

- **`s2udio rq start`** — idempotent: prints the web UI URL when an
  engine is already running (GUI- or CLI-started); otherwise spawns a
  hidden `s2udio rq serve` **daemon** (new process group, detached,
  signal-driven shutdown) that owns the rqbit child + auth proxy,
  registers itself, and self-heals (exits + unregisters when rqbit
  dies). `src/core/rqctl.rs`.
- **`s2udio rq stop`** — SIGTERM (then SIGKILL after ~2 s) the
  registered pid, removes the registration. Also stops a GUI-started
  engine.
- **`s2udio rq open`** — `xdg-open`s the registered web URL.
- The Settings panel's web-UI rows consult the same registration
  (reuse, liveness) and register engines they start, so the GUI and the
  CLI never run two standalone engines.
- Engine config for CLI starts: the `torrent` section of config.ron +
  the state.ron socks override (same rule as app startup).

## 10. Configuration (`config.ron`, new `torrent` section)

```ron
torrent: (
  enabled: true,
  port: 3030,
  min_download_speed_kbps: 500,
  warmup_secs: 5,
  max_wait_secs: 15,
  no_peers_timeout_secs: 30,
  cache_dir: "~/.cache/s2udio/torrents",
  auto_pick_file: true,
  keep_after_play: false,
  socks_proxy: None,   // socks5://[user:pass@]host:port — VPN route
)
```

Env override: `S2UDIO_RQBIT_BIN` (mirrors `S2UDIO_YTDLP_BIN`; unit tests
use a fake script). `enabled: false` → torrent items classified but the
popup section hidden / action shows "torrent streaming disabled".
`socks_proxy: Some("socks5://…")` adds `--socks-url` +
`--disable-tcp-listen` to every engine spawn; the Settings panel can set
it too (persisted to `state.ron`, overrides the config on startup).

## 11. Edge cases & failure modes

| Case | Behavior |
| --- | --- |
| Dead magnet / no peers | `no_peers_timeout_secs` → "No peers found — is the torrent alive?", cleanup, no dialog |
| Speed below threshold | dialog Retry / Play anyway / Cancel |
| rqbit missing | status notice with install hint; other features unaffected |
| Engine crash mid-session | mpv stalls; notice via session poll (enhancement: stall detection) |
| Port in use | fallback scan port+1..+20 |
| Torrent with no media | "No playable media in this torrent", cleanup |
| Duplicate magnets | dedupe by infohash (existing token dedupe + infohash key) |
| Weird torrent/file names | sanitized display (existing label handling); never executed |
| Seek beyond downloaded range | mpv stalls briefly; rqbit fetches on demand (Range) |

## 12. Security & legal

- Engine bound to 127.0.0.1 with a random basic-auth token on every
  request; cache dir per-user; the engine child is killed on exit.
- We only *stream* media to mpv — torrent content is never executed.
- The usual P2P caveat applies (distribution = uploading while
  downloading; `keep_after_play: false` + immediate kill minimizes it;
  still: user's responsibility, note in FAQ).

## 13. Validation plan (testing agent)

1. **Unit** — classification: magnet URIs (with/without `btih:`,
   uppercase), `.torrent` local/file:///http paths, mixed pastes;
   file-picker logic; speed-gate decision table (below/above threshold,
   timeout, no-peers); all with a fake `S2UDIO_RQBIT_BIN` script.
2. **Live engine smoke** — rqbit server start/stop, POST magnet vs
   binary .torrent vs http .torrent, stats JSON parse, stream URL fetch
   with Range.
3. **Manual (real torrents)** — use the canonical public test files:
   Sintel / Big Buck Bunny `.torrent` from webtorrent.io + magnets with
   and without trackers (DHT-only); a multi-file torrent (picker); a
   dead magnet (timeout path); a deliberately throttled local seeder
   (`tc netem` / seeder upload limit) to exercise the speed gate +
   Retry/Play-anyway dialog.
4. **Regression** — existing paste popup types (audio/video/YT/local)
   unchanged; non-torrent mpv sessions unchanged (byte-for-byte launch);
   MPRIS/queue/now-playing still correct for normal playback.
5. **Lifecycle** — play → audio switch → engine killed; app exit kills
   child; cache dir cleaned; no zombie rqbit after any path.

## 14. Implementation phases

- **M1 Engine bootstrap: DONE (2026-08-08, container)** —
  `src/core/torrent.rs` (spawn/kill server with a `Drop` that reaps the
  child, auth via `RQBIT_HTTP_BASIC_AUTH_USERPASS` + `Authorization:
  Basic` on every request, ureq REST client for add/details/stats/delete,
  `find_free_port` fallback scan), config section + defaults
  (`src/config/torrent.rs`, `torrent:` in `assets/example_config.ron`),
  env override `S2UDIO_RQBIT_BIN`, fake-engine unit tests (6 tests via a
  Python HTTP-server fake). **Needs host validation: `cargo test
  --release` (no toolchain in the container).**
- **M2 Classification + popup: DONE (2026-08-08, container)** —
  `PastedItem::Torrent` (local path / `file://` / `http(s)` URL ending
  in `.torrent`) and `PastedItem::Magnet` (magnet: links classified
  before the http(s) branch; infohash extracted for labels + dedupe),
  classify rules, `[Torrent]` popup section with a single **Play
  (stream)** action (a dim `Torrent streaming disabled` row when
  `torrent.enabled: false`), and an end-to-end action: work thread
  starts the engine, adds the torrent, waits ≤ `max_wait_secs`
  (default 15 s) for a magnet's metainfo (`wait_for_files` — a magnet's
  file list only appears once
  peers delivered it), picks the largest playable file
  (`pick_playable_file`: largest video extension, else largest audio),
  and the UI plays the stream URL via mpv with the engine kept in
  `Ctx.torrent_engine` (its `Drop` kills rqbit on exit). M2 plays
  without the bandwidth gate — M3 inserts it. **Host-validated
  `cargo test --release`: 1232/1232 (16 new tests: 11 paste
  classification/popup + 5 engine pick/source); real-engine smoke
  against rqbit 9.0.0-beta.2 (Big Buck Bunny .torrent): spawn + add +
  details + pick (`Big Buck Bunny.mp4`, 276 MB) + stream URL Range GET
  206 with the embedded auth userinfo.**
- **M2.5 "Play and Download" (2026-08-08, host)** — a second
  `[Torrent]` popup action that keeps the downloaded file: the same
  stream prep plus a `Ctx.torrent_download` job polling
  `stats/v1` (1 s); on completion the picked file is moved to
  `~/Downloads/s2udio-downloads` (deferred to `MpvSessionEnded` when the
  stream is still playing), the torrent is deleted from the engine. Now-playing/MPRIS: the entry title (file name) is
  used instead of the raw stream URL (`is_torrent_stream_url` in
  `MpvSessionStarted`) and the torrent name becomes the MPRIS artist;
  no art (M4's `yt_info` integration adds the info-box/thumbnail side).
  Fix: the runtime default `torrent.cache_dir` is now tilde-expanded
  (the config-file path already was; without a `torrent:` section the
  engine used a literal `~`). **Host-validated `cargo test --release`:
  1238/1238** (+5 torrent tests incl. a real-v9 stats-JSON parse, +1
  config expansion test, updated popup tests).
- **Round 17 torrent UX — scan + multi-file play + video queue
  (2026-08-08, container)**: the `[Torrent]` popup scans pasted torrents
  up front (`ScanTorrent` → `TorrentScanned`, "Loading…" rows, per-item
  labels, `Ctx.torrent_scans` engine reuse — play actions reuse the
  scanned engine via `AppEvent::TorrentScannedPlay`, falling back to the
  fresh `PlayTorrent` path when the scan is gone); multi-video torrents
  get **Play all (N files)** and **Select files…** (`torrent_file_picker.rs`
  multi-select, name + size, positional indices preserved); every torrent
  play fills the Queue tab's Video list via `play_video_entries` +
  synthetic in-memory `yt_info` entries (title = file name, channel =
  torrent name), and `session_playlist_shown` shows the session playlist.
  **`cargo test --release`: 1251/1251** (+12 net tests; 3 warnings =
  baseline). Host validation + real-engine smoke (Big Buck Bunny
  single-file + a multi-file torrent) pending. Host-validated
  **1254/1254** (round-17 implementation + follow-ups).
- **Round 18 scan wait — no deadline, live wait window (2026-08-08,
  container)**: the metainfo wait is **open-ended and user-controlled**
  (the user decides how long a cold magnet may take; no error after N
  seconds) and moves to a **dedicated thread per scan** (`ScanTorrent`
  spawns it — the work thread is never blocked; the legacy `PlayTorrent`
  path got the same open-ended wait on its own thread). The popup's
  Loading row becomes a live **wait window** — `Loading <label>… mm:ss`
  counter + `DL <speed> · need ≥ <min> KB/s ✓/✗` (live speed vs
  `torrent.min_download_speed_kbps`) + `esc to cancel` — refreshed once
  per second by `WorkDone::TorrentScanProgress` (from
  `stats/v1.live.download_speed.mbps`, ×1000/8 → KB/s). Esc/close fires
  each in-flight scan's cancel channel (`Ctx.torrent_scan_cancels`,
  keyed per item) so the scan thread aborts and drops the engine (rqbit
  killed); per-item cancels keep multi-item pastes independent. The
  `max_wait_secs` stopgap is obsolete (config relic). **`cargo test
  --release`: 1258/1258** (+4 net: replaced the deadline test with
  open-ended wait/cancel/progress tests + wait-window rendering +
  Esc-cancel wiring).
- **M3 Bandwidth gate**: stats polling + progress modal + dialogs,
  gate decision logic + tests.
- **M4 Playback + lifecycle**: file picker, `only_files_regex`
  optimization, mpv routing + cache args, `yt_info` integration,
  cleanup triggers.
- **M5 Hardening + validation**: stall notice, docs (this doc → current,
  FAQ, keybind/UX notes), full validation pass.

## 15. Open questions

- Does rqbit accept `--http-api-listen-addr 127.0.0.1:0` for an
  ephemeral port? **Resolved 2026-08-08 (M1): decided to use the config
  port + a scan of `port+1 ..= port+20` (`find_free_port` in
  `src/core/torrent.rs`) — avoids depending on unverifiable behavior;
  revisit if the scan ever bites.**
- `LiveStats.download_speed` — confirm unit (bytes/s) and that it's a
  smoothed average on first engine contact; if not, compute our own rate
  from `progress_bytes` deltas. **Open (M3): needs a real engine on the
  host; the fake engine fixtures assume bytes/s.**
- mpv stall-watch on slow mid-play networks: include in v1 or leave as
  enhancement? **Decided: enhancement (M5). mpv's cache + the pre-play
  gate cover the common case.**

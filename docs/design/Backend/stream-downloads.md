---
title: "Stream Downloads (s2udio-downloads)"
section: backend
doc_type: flow
id: "backend/stream-downloads"
description: >
  Saving ytdlp streams as files: the controls' Download button and the
  right-click Download on stream rows (queue, video queue, playlists), the
  save-as options (audio/video, chapters as one file or per chapter), the
  s2udio-downloads folder and how a downloaded file replaces the stream
  it came from.
status: "current"
updated: "2026-08-11"
source_files:
  - src/ui/panes/controls.rs
  - src/ui/modals/paste.rs
  - src/shared/ytdlp/manager.rs
  - src/shared/ytdlp/downloader.rs
  - src/core/work.rs
  - src/core/event_loop.rs
  - src/shared/mpd_client_ext.rs
  - src/ui/panes/directories.rs
  - src/ui/panes/queue.rs
  - src/ui/panes/queue/video.rs (video-queue Download; moved out of queue.rs in Phase 4b)
  - src/ui/panes/queue/context_menus.rs (right-click Download item; moved out of queue.rs in Phase 4b)
  - src/ui/panes/playlists.rs
related:
  - backend/ytdlp-resolution
  - backend/mpv-session
  - frontend/layout-templates
  - tabs/mpd-tab
tags: [yt-dlp, downloads, streams, replace, mpd]
---

# Stream Downloads

A resolved YouTube/Soundcloud/NicoVideo stream can be saved as a local
file. Downloads land in **`~/Downloads/s2udio-downloads`** (created on
demand), OUTSIDE the MPD library. The MPD tab still shows the folder at
the top of the library (right under `Library ↴`) as **`Downloads`** —
the tree injects the folder as its first child and the root right-pane
list prepends the same entry; its listing comes from **disk**
(`read_dir`, absolute paths, file stem as the title), not MPD `lsinfo`,
and is re-read on every open (downloads appear/disappear).

**MPD consequences** (the folder is not in `music_directory`): MPD
cannot play files from it. Playing a Downloads file routes through
**mpv** (audio or video) instead of the MPD queue; the MPD queue and
stored-playlist replacements keep the stream entry and only report the
save (`complete_stream_download` guards on the file being inside the
music dir).

## Entry points

| Where | Action | Replace behavior |
| --- | --- | --- |
| Controls bar **Download** button (mpv playing a ytdlp stream) | `open_download_menu` | `ReplaceAction::None` — just save |
| Queue **Audio** list right-click on a stream row | "Download" item | `ReplaceAction::Queue { song_id }` — the queue entry is deleted and the file(s) inserted at its position |
| Queue **Video** list right-click on a stream entry | "Download" item | `ReplaceAction::VideoPlaylist { index }` — the persistent video playlist entry is swapped for the file |
| Playlists tab song menu on a stream | "Download" item | `ReplaceAction::Playlist { name, uri }` — the playlist entry is replaced at its position |

A row is a "stream" when its URI (the resolved stream URL, or the
original link) is keyed in `ctx.yt_info` (or matches some info's
`original_url`).

## Save-as menu

`paste::open_stream_download_menu(ctx, &info, &replace)`:

- **Save as audio** — `yt-dlp -x` (best audio), embedded thumbnail +
  metadata.
- **Save as video** — `bv*+ba/b`, merged to mp4, `--embed-metadata
  --embed-thumbnail --embed-chapters`.
- When the media has chapters (`info.chapters.len() > 1`): **Audio/Video —
  each chapter its own file** (`--split-chapters`, output template
  `%(section_title)s.%(ext)s` — the chapter title is the file title).

The download always uses **`info.original_url`** (yt-dlp needs the
original link, not the resolved stream URL).

## Pipeline

1. `paste::queue_stream_download` resolves the URL to a `YtDlpItem`
   (`YtDlpContent::from_str`) and queues it on the `YtDlpManager` with a
   `StreamDownloadSpec` (`output_dir`, `audio_only`, `split_chapters`,
   `on_complete: ReplaceAction`).
2. The work thread runs `YtDlp::download_stream` (the spec's dir needs no
   `cache_dir`); the produced files are detected by diffing the output
   dir before/after (yt-dlp post-processing renames files, so the output
   template cannot be trusted alone). `YtDlpDownloadResult` carries
   `file_paths` (N for split chapters, 1 otherwise).
3. The event loop's `complete_stream_download` runs the replace:
   - `VideoPlaylist`: the entry's `url`/`title` are swapped in
     `ctx.video_playlist` (absolute paths are fine for mpv) and persisted.
   - `Queue`: `replace_downloaded_stream` does `delete_id` + `add` at the
     captured queue position (`QueuePosition::Absolute`) for each file; a
     `None` position (the entry is already gone — the queue changed
     mid-download) appends the files instead, and a replaced entry that
     **was playing** restarts from the downloaded file (`play_pos`).
   - `Playlist`: `delete_from_playlist` + `add_to_playlist` at the same
     index (MPD's `playlistadd` supports a target position).
   - `None`: just the save (the browser lists the folder from disk).
   The MPD-relative URI (`path_to_mpd_uri`) is the file path stripped of
   `music_directory` (from the MPD `config` command). Downloads land
   outside the library, so these replacements only fire for files inside
   it; otherwise the stream entry stays and a status explains the save.

## Notes

- The legacy search download (cache dir, `download_single`) is untouched:
  its `WorkRequest`/`WorkDone` entries carry `spec: None`.
- The `DownloadsModal` (`GlobalAction::ShowDownloads`, or the search
  result auto-open when `auto_open_downloads` is set) shows stream
  downloads too — they share the manager's queue and states.

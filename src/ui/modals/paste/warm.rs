use crate::{
    ctx::Ctx,
    mpd::mpd_client::MpdClient,
    shared::{events::WorkRequest, ytdlp::tagged_stream_entry},
};

use super::{
    PastedItem, YtAction, apply_resolved_streams, expired_stream_original_url,
    fresh_cached_stream_info, mark_stream_parse_pending, s2udio_cache_path, stream_info_for,
    stream_parse_pending, youtube_link_of,
};

/// Ask the work thread for the stream info of pasted links that have none yet
/// (round 78): the download options need their chapter list, and nothing else
/// fetches it before the link is played. The open popup refreshes when the
/// resolve lands, and every link is asked for at most once
/// (`ctx.paste_chapter_warm`), so a failing resolve cannot loop.
pub(super) fn warm_chapters(ctx: &Ctx, downloads: &[PastedItem]) {
    let urls: Vec<String> = downloads
        .iter()
        .filter_map(|item| youtube_link_of(item))
        .filter(|url| stream_info_for(ctx, url).is_none())
        .filter(|url| !ctx.paste_chapter_warm.borrow().contains(url))
        .collect();
    if urls.is_empty() {
        return;
    }
    {
        let mut warm = ctx.paste_chapter_warm.borrow_mut();
        for url in &urls {
            warm.insert(url.clone());
        }
    }
    log::debug!(urls:?; "Fetching the pasted links' stream info for the download options");
    // Round 95: a queued row for one of these links has no duration yet -
    // its cell spins until the info lands.
    mark_stream_parse_pending(ctx, &urls);
    let _ = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
        urls,
        action: YtAction::Refresh,
        // The chapters/download options only need display metadata.
        meta_only: true,
    });
}

/// Round 95b (user, restored 105e): while a track plays, resolve the queue
/// entry **after** it when that entry is still an unresolved audio link. MPD
/// cannot open a link, so without this it skips past the following entries
/// until the app's resolve lands (round 104 measured the storm: MPD failing
/// 5-15 unresolved rows in ~3 s per transition). The look ahead resolves ONE
/// row ahead; with the round-98 in-place swap, MPD's next-song prefetch finds
/// a playable URL and the current track is never cut (playing the replacement
/// was what used to cut it; `ReplaceInPlace` never sends `playid`).
///
/// Only the **next** entry is warmed (never the rest of the playlist), and
/// only when nothing is known for it yet: a fresh cached resolve, a resolve
/// already in flight, or a non-link entry all leave the queue alone.
///
/// **Which row is "next" comes from MPD** (`status.nextsongid`), not from
/// `current + 1`: in `random` mode MPD plays its own shuffled sequence, so the
/// row after the current one is usually not the row that plays next, and
/// warming it left the real next row unresolved (and the round-104 storm in
/// place) for random playback. The index is only the fallback for when MPD
/// reports no `nextsong` (a stopped player, or the end of the queue).
pub fn warm_next_queue_link(ctx: &Ctx) {
    let Some((idx, current)) = ctx.find_current_song_in_queue() else { return };
    let next = ctx
        .status
        .nextsongid
        .and_then(|id| ctx.queue.iter().find(|song| song.id == id))
        .or_else(|| ctx.queue.get(idx + 1))
        .cloned();
    let Some(next) = next else { return };
    // One look ahead per (current, next) pair: the swap below refreshes the
    // queue, and without this marker each refresh would warm the entry after
    // the one just warmed (a slow crawl through the whole playlist).
    if ctx.warmed_next_for.get() == Some((current.id, next.id)) {
        return;
    }
    let Some((link, intent)) = tagged_stream_entry(&next.file) else {
        // Round 105: the next row is a **stale resolved stream** — a signed
        // googlevideo URL whose `expire=` epoch passed. MPD would open it,
        // play a few seconds of whatever the CDN still serves, then fail and
        // skip on (nothing re-resolves it: the entry is neither a link nor
        // anything the error recovery recognizes). Warm its re-resolve from
        // the original link exactly like the tagged row, swapped in place.
        let Some(original) = expired_stream_original_url(ctx, &next.file) else {
            return;
        };
        // Let a re-resolve that is already in flight land first; the next
        // queue update retries (the marker is only set once something is
        // done).
        if stream_parse_pending(ctx, &next.file)
            && fresh_cached_stream_info(ctx, &original).is_none()
        {
            return;
        }
        ctx.warmed_next_for.set(Some((current.id, next.id)));
        if let Some(info) = fresh_cached_stream_info(ctx, &original) {
            apply_resolved_streams(ctx, vec![info], YtAction::ReplaceInPlace(next.id), Vec::new());
            return;
        }
        log::debug!(url = next.file.as_str(); "Warming the next queue entry's stream (its URL expired)");
        mark_stream_parse_pending(ctx, std::slice::from_ref(&original));
        if let Err(err) = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
            urls: vec![original],
            action: YtAction::ReplaceInPlace(next.id),
            meta_only: false,
        }) {
            log::error!(error:? = err; "Failed to request the next entry's stream resolution");
        }
        return;
    };
    if !intent.is_audio() {
        return;
    }
    // Let a resolve that is already in flight land first; the next queue
    // update retries (the marker is only set once something is done).
    if stream_parse_pending(ctx, &next.file) && fresh_cached_stream_info(ctx, &link).is_none() {
        return;
    }
    ctx.warmed_next_for.set(Some((current.id, next.id)));
    // Round 98: already resolved (the paste popup, an earlier play, a stored
    // playlist entry): swap the row right away, no yt-dlp run.
    if let Some(info) = fresh_cached_stream_info(ctx, &link) {
        apply_resolved_streams(ctx, vec![info], YtAction::ReplaceInPlace(next.id), Vec::new());
        return;
    }
    log::debug!(link = link.as_str(); "Warming the next queue entry's stream while the current one plays");
    mark_stream_parse_pending(ctx, std::slice::from_ref(&link));
    // Round 98: the resolve **swaps the row in place** instead of only
    // caching the info. MPD prefetches the next song before the current one
    // ends; while the row is still a tagged link that prefetch fails and the
    // app's error recovery used to play the entry early, cutting the current
    // track short. A swapped row is a playable URL by then.
    if let Err(err) = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
        urls: vec![link],
        action: YtAction::ReplaceInPlace(next.id),
        meta_only: false,
    }) {
        log::error!(error:? = err; "Failed to request the next entry's stream resolution");
    }
}
/// Make sure the chapters of the current song are known: sync from the
/// resolved YouTube info, or fetch them (Jellyfin items via the API, local
/// files via ffprobe). Called on song change / startup.
pub fn ensure_chapters(ctx: &Ctx) {
    let Some((_, song)) = ctx.find_current_song_in_queue() else { return };
    if ctx.chapters.borrow().contains_key(&song.file) {
        return;
    }
    if let Some(yt) = ctx.yt_info.borrow().get(&song.file)
        && !yt.chapters.is_empty()
    {
        log::debug!(
            file = song.file.as_str(), len = yt.chapters.len();
            "Chapters for the current song taken from the resolved stream info"
        );
        ctx.chapters.borrow_mut().insert(song.file.clone(), yt.chapters.clone());
        return;
    }
    if let Some(item_id) = crate::jellyfin::item_id_from_url(&song.file) {
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinChapters { item_id })
            .map_err(|err| log::error!(error:? = err; "Failed to request jellyfin chapters"));
        return;
    }
    if !crate::ui::panes::radio::is_stream_url(&song.file) {
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchFileChapters { file: song.file.clone() })
            .map_err(|err| log::error!(error:? = err; "Failed to request file chapters"));
    }
}
/// Where the MPRIS bridge (mpDris2, patched to look here) expects the album
/// art of a playing stream: `<cache_dir>/mpris-art` (default
/// `~/.cache/s2udio/mpris-art`, round 19).
pub fn mpris_art_path(cache_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    s2udio_cache_path(cache_dir, "mpris-art")
}
/// State file of the mpv MPRIS bridge: `<cache_dir>/mpv-mpris.json`
/// (written every ~500 ms while a Jellyfin video plays; the s2udio-mpris
/// daemon exposes it over D-Bus and exits when it goes stale). Default
/// `~/.cache/s2udio/mpv-mpris.json` (round 19).
pub fn mpv_mpris_state_path(cache_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    s2udio_cache_path(cache_dir, "mpv-mpris.json")
}
/// Poster file served by the mpv MPRIS bridge: `<cache_dir>/mpris-mpv-art`.
/// Defaults to `~/.cache/s2udio` (round 23; legacy `~/.cache/rmpc` is
/// honored when it still holds the file — migration read).
pub fn mpv_mpris_art_path(cache_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    if let Some(dir) = cache_dir {
        return dir.join("mpris-mpv-art");
    }
    let new = crate::shared::paths::s2udio_cache_dir()
        .unwrap_or_else(|| {
            crate::config::utils::tilde_expand("~/.cache/s2udio").into_owned().into()
        })
        .join("mpris-mpv-art");
    let legacy: std::path::PathBuf =
        crate::config::utils::tilde_expand("~/.cache/rmpc").into_owned().into();
    if new.exists() {
        new
    } else if legacy.join("mpris-mpv-art").exists() {
        legacy.join("mpris-mpv-art")
    } else {
        new
    }
}
/// Remove the mpv MPRIS poster file: a new video (or a fresh session) must
/// never keep showing the previous video's thumbnail in the media controls
/// until its own art is fetched. Call whenever `ctx.mpv.art_path` is reset.
pub fn clear_mpv_mpris_art(ctx: &Ctx) {
    let _ = std::fs::remove_file(mpv_mpris_art_path(ctx.config.cache_dir.as_deref()));
}
/// Write the mpv session state for the MPRIS daemon (called from the
/// 500 ms mpv poll).
pub fn write_mpv_mpris_state(ctx: &Ctx) {
    let path = mpv_mpris_state_path(ctx.config.cache_dir.as_deref());
    let art = ctx
        .mpv
        .art_path
        .as_ref()
        // Round 91: never advertise a poster that is no longer on disk - the
        // media controls would otherwise keep a dead file:// URL until the
        // next fetch.
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let socket =
        ctx.mpv.socket.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let playlist: Vec<serde_json::Value> = ctx
        .mpv
        .playlist
        .borrow()
        .iter()
        .map(|e| {
            serde_json::json!(
                { "title" : e.title, "url" : e.url, "duration" : e.duration,
                "original_url" : e.original_url, }
            )
        })
        .collect();
    let duration = if ctx.mpv.duration > 0.0 {
        ctx.mpv.duration
    } else {
        let entry_duration = ctx
            .mpv
            .playlist
            .borrow()
            .get(ctx.mpv.playlist_pos.get().unwrap_or(0))
            .and_then(|e| e.duration);
        entry_duration
            // Round 92: mpv reports no duration for the first seconds of a
            // resolved stream (the DASH/HLS manifest has not been parsed
            // yet), so the media controls showed a dead timeline until mpv
            // learned the length. The resolve cache already holds the real
            // duration (the link was resolved before playback), so use it
            // for the entry that is playing - keyed to that entry's URL,
            // never to the previous video.
            .or_else(|| mpv_yt_info(ctx).and_then(|info| info.duration))
            .unwrap_or(0.0)
    };
    let state = serde_json::json!(
        { "title" : ctx.mpv.title, "artist" : ctx.mpv.artist, "art" : art, "playing" : !
        ctx.mpv.paused, "position" : ctx.mpv.position, "duration" : duration, "socket" :
        socket, "item_id" : ctx.mpv.item_id.clone().unwrap_or_default(), "volume" : ctx
        .mpv.volume, "playlist" : playlist, "playlist_pos" : ctx.mpv.playlist_pos.get(),
        }
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(&state) {
        if let Err(err) = std::fs::write(&path, bytes) {
            log::error!(error:? = err; "Failed to write mpv MPRIS state");
        }
    }
}
/// Remove the mpv MPRIS state (session ended) so the daemon exits.
pub fn delete_mpv_mpris_state(ctx: &Ctx) {
    let _ = std::fs::remove_file(mpv_mpris_state_path(ctx.config.cache_dir.as_deref()));
    clear_mpv_mpris_art(ctx);
}
/// The resolved info of the video currently playing in mpv, looked up by
/// the playlist entry's URL (the original YouTube/Soundcloud/NicoVideo
/// link — `apply_resolved_streams` keys the info by it). After a restart
/// the cache only holds entries keyed by the resolved stream URL (which
/// expires), so entries are also matched by their recorded `original_url`.
pub fn mpv_yt_info(ctx: &Ctx) -> Option<crate::shared::ytdlp::YtStreamInfo> {
    let url = ctx
        .mpv
        .playlist
        .borrow()
        .get(ctx.mpv.playlist_pos.get().unwrap_or(0))?
        .lookup_url()
        .to_owned();
    let info = ctx.yt_info.borrow();
    info.get(&url).cloned().or_else(|| info.values().find(|e| e.original_url == url).cloned())
}
/// The resolved info of whatever is currently playing: the mpv video's
/// (see [`mpv_yt_info`]), or — when a YouTube-style stream plays as audio
/// through MPD — the current queue song's, looked up by its resolved
/// stream URL (or a matching `original_url`). The queue tab's info box
/// uses this so an audio stream shows the same video-style details as an
/// mpv video.
pub fn current_yt_info(ctx: &Ctx) -> Option<crate::shared::ytdlp::YtStreamInfo> {
    if crate::core::mpv::mpv_is_ui_source(ctx) {
        return mpv_yt_info(ctx);
    }
    let (_, song) = ctx.find_current_song_in_queue()?;
    // Round 107: tag-aware, so a row that is still the tagged link (before
    // MPD replaces it with the buffered URL) also resolves its info.
    super::resolve::song_stream_info(ctx, &song.file)
}
/// Make MPRIS (mpDris2, which reads MPD's song tags) show the real title and
/// thumbnail of a playing stream: a YouTube video's title/channel and
/// thumbnail, or a Jellyfin item's name and primary image. The queue entry's
/// tags are set via `addtagid` (the stream URL itself carries no metadata);
/// the thumbnail is written to `<cache_dir>/mpris-art`.
pub fn ensure_mpris_metadata(ctx: &Ctx) {
    let Some((_, song)) = ctx.find_current_song_in_queue() else {
        crate::core::work::set_expected_mpris_art(None);
        return;
    };
    let Some(song_id) = ctx.status.songid else {
        crate::core::work::set_expected_mpris_art(None);
        return;
    };
    if let Some(yt) = ctx.yt_info.borrow().get(&song.file) {
        let title = yt.title.clone();
        let channel = yt.channel.clone();
        let thumb = yt.thumbnail.clone();
        crate::core::work::set_expected_mpris_art(thumb.clone());
        ctx.command(move |client| {
            if !title.is_empty() {
                // Round 95 (user): no album tag for a YouTube-style stream -
                // it only ever repeated the title, and an empty album is
                // what the media widget should show.
                let _ = client.add_tag_id(song_id, "title", &title);
            }
            if let Some(channel) = channel
                && !channel.is_empty()
            {
                let _ = client.add_tag_id(song_id, "artist", &channel);
            }
            Ok(())
        });
        if let Some(thumb) = thumb {
            let _ = ctx
                .work_sender
                .send(WorkRequest::SaveMprisArt {
                    url: thumb,
                    crop_letterbox: ctx.config.album_art.crop_letterbox,
                })
                .map_err(|err| log::error!(error:? = err; "Failed to request MPRIS art"));
        }
        return;
    }
    if let Some(item_id) = crate::jellyfin::item_id_from_url(&song.file) {
        crate::core::work::set_expected_mpris_art(Some(format!("jellyfin:{item_id}")));
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinMpris { item_id })
            .map_err(|err| log::error!(error:? = err; "Failed to request jellyfin MPRIS info"));
        return;
    }
    crate::core::work::set_expected_mpris_art(None);
    let _ = std::fs::remove_file(mpris_art_path(ctx.config.cache_dir.as_deref()));
}

use crate::{
    config::tabs::{PaneType, TreeBrowserArgs},
    ctx::Ctx,
    mpd::{QueuePosition, commands::Song, mpd_client::MpdClient},
    shared::{
        events::WorkRequest,
        macros::{status_info, status_warn},
        mpd_client_ext::MpdClientExt as _,
        ytdlp::{YtListMeta, YtStreamInfo, tagged_stream_entry, untag_stream_link},
    },
};

use super::{
    PASTE_PLAY, YtAction, now_epoch_secs, refresh_paste_modal, s2udio_cache_path, stream_meta_key,
};

/// The cached stream info of a link or resolved stream URL (`apply_resolved_streams`
/// stores it under both the resolved stream URL and the original link). This is
/// also what makes a row a **web stream**: a queue/playlist entry whose file is
/// a cached yt-dlp stream (or a link yt-dlp resolved) can be downloaded.
pub fn stream_info_for(ctx: &Ctx, url: &str) -> Option<YtStreamInfo> {
    let info = ctx.yt_info.borrow();
    info.get(url)
        .cloned()
        .or_else(|| info.values().find(|entry| entry.original_url == url).cloned())
}
/// Round 107: the resolved stream info of a queue row, whichever form its
/// file takes — the resolved URL, the original link, or an intent-tagged
/// link (`#s2u-audio` / `#s2u-video`). The selection-time warm stores the
/// info under the original link, so a merely-selected tagged row must strip
/// its tag to find it and show its description/artwork before it plays.
pub fn song_stream_info(ctx: &Ctx, file: &str) -> Option<YtStreamInfo> {
    if let Some(info) = stream_info_for(ctx, file) {
        return Some(info);
    }
    let (link, _) = tagged_stream_entry(file)?;
    stream_info_for(ctx, &link)
}
/// Round 91b: how long a cached resolve may be reused for a play. The
/// signed stream URL is valid for hours; the bound only keeps a play from
/// reusing a URL that has been sitting around long enough to be a gamble,
/// and a miss simply resolves the link again.
const CACHED_RESOLVE_MAX_AGE_SECS: u64 = 30 * 60;

/// The `expire=` epoch of a googlevideo stream URL, when it carries one.
fn stream_url_expire(url: &str) -> Option<i64> {
    url.split("expire=").nth(1)?.split('&').next()?.parse::<i64>().ok()
}

/// Round 91b: the cached resolve of `link` while it is still usable - the
/// entry the paste popup's background resolve (round 88), an earlier play,
/// or a stored playlist entry put in `<cache_dir>/yt-info.json`. Playing a
/// queued link then starts immediately instead of waiting for yt-dlp again.
/// `None` when the link is unknown, the entry has no age (written before
/// this field existed), it is older than
/// [`CACHED_RESOLVE_MAX_AGE_SECS`], or its signed URL has expired.
pub(super) fn fresh_cached_stream_info(
    ctx: &Ctx,
    link: &str,
) -> Option<crate::shared::ytdlp::YtStreamInfo> {
    let info = {
        let cache = ctx.yt_info.borrow();
        cache
            .get(link)
            .cloned()
            .or_else(|| cache.values().find(|info| info.original_url == link).cloned())
    }?;
    if info.url.is_empty() {
        return None;
    }
    let Some(resolved_at) = info.resolved_at else {
        return None;
    };
    let now = now_epoch_secs();
    if now.saturating_sub(resolved_at) > CACHED_RESOLVE_MAX_AGE_SECS {
        log::debug!(link = link; "Cached stream resolve is too old to reuse");
        return None;
    }
    if let Some(expire) = stream_url_expire(&info.url)
        && expire <= now as i64
    {
        log::debug!(link = link; "Cached stream resolve has expired; resolving again");
        return None;
    }
    Some(info)
}

/// Round 91: resolve a queue entry that is still a pasted stream **link**
/// (`watch?v=ID#s2u-audio`) - the Queue tab's Enter on such an entry, or MPD
/// starting one on its own (media keys, `next`, a restored queue). The link
/// is resolved and the entry replaced in place with its stream, which starts
/// playing ([`YtAction::ReplaceAndPlay`]).
///
/// The replacement entry carries the resolved googlevideo URL, never a
/// tagged link, so one row cannot resolve twice. The
/// `ctx.pending_stream_resolve` marker keeps the request to **one per queue
/// entry**, which matters because MPD keeps reporting the unplayable link as
/// the current song until the replacement lands, and because the paste row's
/// "play" arm asks for the entry it just queued. `false` means nothing was
/// sent (not a tagged audio link, or its resolve is already pending).
pub fn resolve_tagged_queue_entry(ctx: &Ctx, file: &str, song_id: u32) -> bool {
    resolve_queue_entry_with(ctx, file, song_id, true)
}

/// Round 98: the same as [`resolve_tagged_queue_entry`], but the replacement
/// is **not** started: the entry is swapped in place only. The event loop's
/// MPD-error recovery uses it when MPD named an entry it has not reached yet
/// (a next-song prefetch failure), where playing the replacement would cut
/// the current track short.
pub fn resolve_tagged_queue_entry_in_place(ctx: &Ctx, file: &str, song_id: u32) -> bool {
    resolve_queue_entry_with(ctx, file, song_id, false)
}

/// Round 105: resolve a queue entry that is a **stale resolved stream** — a
/// signed googlevideo URL whose `expire=` epoch passed — from its original
/// link. `play` chooses [`YtAction::ReplaceAndPlay`] vs
/// [`YtAction::ReplaceInPlace`], the same round-91/98 split as the tagged
/// path. Returns `false` when the row is not an expired stream with a
/// remembered original link, or its resolve is already pending.
pub fn re_resolve_expired_queue_entry(ctx: &Ctx, file: &str, song_id: u32, play: bool) -> bool {
    resolve_queue_entry_with(ctx, file, song_id, play)
}

/// The shared body of the three entry points above; `play` decides whether the
/// replacement starts playing ([`YtAction::ReplaceAndPlay`]) or is only put
/// in place ([`YtAction::ReplaceInPlace`]).
fn resolve_queue_entry_with(ctx: &Ctx, file: &str, song_id: u32, play: bool) -> bool {
    // What the row must be resolved FROM: an audio-tagged link itself, or —
    // round 105 — the original link of a resolved stream whose signed URL has
    // expired. Everything else (a live resolved URL, a local file, a radio
    // URL) is not this path's business.
    let (object, expired) = match tagged_stream_entry(file) {
        Some((link, intent)) if intent.is_audio() => (link, false),
        Some(_) => return false,
        None => {
            let Some(original) = expired_stream_original_url(ctx, file) else {
                return false;
            };
            (original, true)
        }
    };
    {
        let mut pending = ctx.pending_stream_resolve.borrow_mut();
        // Keep the marker queue-sized: a replaced entry stops matching.
        pending
            .retain(|(uri, id)| ctx.queue.iter().any(|song| song.id == *id && song.file == *uri));
        if !pending.insert((file.to_owned(), song_id)) {
            return false;
        }
    }
    let action =
        if play { YtAction::ReplaceAndPlay(song_id) } else { YtAction::ReplaceInPlace(song_id) };
    // Round 91b: reuse a fresh resolve (the paste popup already resolved
    // this link, or an earlier play did) so the stream starts at once.
    if let Some(info) = fresh_cached_stream_info(ctx, &object) {
        log::debug!(link = object.as_str(); "Reusing the cached stream resolve for a queued entry");
        if play {
            status_info!(
                "{}",
                if expired {
                    "Playing the re-resolved stream…"
                } else {
                    "Playing the resolved stream…"
                }
            );
        }
        apply_resolved_streams(ctx, vec![info], action, Vec::new());
        return true;
    }
    // Round 95: the queue's Duration column spins for this row until the
    // resolve lands (its duration is unknown until then).
    mark_stream_parse_pending(ctx, std::slice::from_ref(&object));
    if let Err(err) =
        ctx.work_sender.send(WorkRequest::ResolveYtStreams { urls: vec![object], action })
    {
        log::error!(error:? = err; "Failed to request stream resolution");
        return false;
    }
    if play {
        status_info!(
            "{}",
            if expired { "Re-resolving the expired stream…" } else { "Resolving the stream…" }
        );
    }
    true
}
/// Round 105: the original link of a queue row that is a **stale resolved
/// stream** — a signed googlevideo URL whose `expire=` epoch has passed. The
/// resolved info (keyed by the URL since the resolve) remembers where the URL
/// came from; resolving that link again yields a fresh signed URL for the same
/// video. `None` for a live URL, a link row, or a stream whose original link
/// is unknown.
///
/// Round 107: a **buffered** stream URL (the local caching proxy) is handled
/// here too. The proxy URL itself never expires, but the signed upstream URL
/// registered behind it does, so an entry whose bytes are not fully cached
/// must be re-resolved from its original link; a fully cached entry needs no
/// upstream and is left to play from the buffer.
pub fn expired_stream_original_url(ctx: &Ctx, url: &str) -> Option<String> {
    if crate::core::stream_cache::is_buffered_url(url) {
        // A buffered row is stable; only an entry the proxy cannot fetch
        // (no live registration, or a failed fill) is re-resolved. Merely
        // being incomplete is NOT stale — the fill is in progress, and
        // re-resolving then made the look ahead swap the same row forever.
        if !crate::core::stream_cache::needs_upstream_refresh(url) {
            return None;
        }
        return crate::core::stream_cache::original_link_for_url(url)
            .filter(|original| !original.is_empty() && original != url);
    }
    if !crate::ui::panes::queue::resolved_stream_expired(url) {
        return None;
    }
    let info = stream_info_for(ctx, url)?;
    let original = info.original_url;
    (!original.is_empty() && original != url).then_some(original)
}
/// Round 105: a cleared queue wipes the in-flight **audio-row** markers — a
/// resolve still on its way for a row that just vanished must not be re-added
/// (the `ReplaceAndPlay` guard drops it anyway), a fresh import of the same
/// link must not trip a stale one-per-entry guard, and no Duration spinner
/// may keep spinning for a row that no longer exists. Video (mpv) resolves
/// carry separate actions and are untouched.
pub fn clear_queue_resolve_markers(ctx: &Ctx) {
    ctx.pending_stream_resolve.borrow_mut().clear();
    ctx.pending_yt_links.borrow_mut().clear();
}
/// Round 107: warm the highlighted queue row's stream metadata when nothing
/// is playing — the user selected a track without playing it, and its
/// description/artwork should be resolved into the buffer so it is ready.
/// The queue pane debounces the call; one request per original link is made
/// (a later play reuses the cached resolve).
pub fn warm_selected_queue_info(ctx: &Ctx) {
    if ctx.status.state == crate::mpd::commands::State::Play {
        return;
    }
    let Some(id) = ctx.queue_selected_id.get() else { return };
    let Some(song) = ctx.queue.iter().find(|song| song.id == id) else { return };
    // Only an unresolved audio-tagged link needs this; a resolved row, a
    // local file or a radio URL already has everything it will get.
    let Some((link, intent)) = tagged_stream_entry(&song.file) else { return };
    if !intent.is_audio() {
        return;
    }
    let link = link.to_owned();
    if stream_info_for(ctx, &link).is_some() {
        return;
    }
    if !ctx.queue_info_warm.borrow_mut().insert(link.clone()) {
        return;
    }
    log::debug!(link = link.as_str(); "Warming the selected queue row's stream metadata");
    if let Err(err) = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
        urls: vec![link],
        action: YtAction::Refresh,
    }) {
        log::error!(error:? = err; "Failed to request the selected row's stream metadata");
    }
}
/// Round 74 (74-1): remember the start offsets carried by the pasted links
/// for the streams that are about to be played through MPD.
///
/// MPD rejects a seek on a stream that is not playing yet, so the offset
/// cannot ride along with `playid`/`add`. It is armed here — keyed by the
/// stream URL, which is exactly the song file MPD reports for the entry —
/// and applied by the event loop on a later status update (see
/// `apply_pending_start_seek` in `core/event_loop/status.rs`). Arming rather than
/// seeking immediately also covers an entry that is only *queued* now: the
/// offset is still waiting when the song finally starts.
///
/// `ReplaceAndPlay` (an expired stream URL re-resolved mid-playback) is
/// deliberately not armed: that entry is already playing somewhere past the
/// original offset, and jumping it back to the pasted timestamp would be a
/// regression.
fn arm_start_offsets(
    ctx: &Ctx,
    info: &[crate::shared::ytdlp::YtStreamInfo],
    player_urls: &[Option<String>],
) {
    let mut pending = ctx.pending_start_seek.borrow_mut();
    for (idx, item) in info.iter().enumerate() {
        if let Some(secs) = item.start_secs.filter(|secs| *secs > 0.0) {
            // Round 107: the row plays from the buffered proxy URL, which is
            // what MPD reports as the song file, so the offset must be armed
            // under that URL when the buffer applies.
            let url = player_urls
                .get(idx)
                .and_then(Option::as_ref)
                .cloned()
                .unwrap_or_else(|| item.url.clone());
            log::debug!(url = url.as_str(), seconds = secs; "Arming the pasted link's start offset");
            pending.insert(url, (secs, 0, None));
        }
    }
}
/// Round 103 (user report): a queued stream entry that yt-dlp refuses for good
/// — a DRM-protected SoundCloud track, an unavailable or private video — can
/// never play. Left in the queue, MPD fails the decode when it reaches the row
/// and skips on. The row is therefore dropped and the reason reported once.
/// Returns whether the failure was handled here; a transient failure is
/// deliberately not: the MPD decode-error recovery resolves a tagged row again
/// when MPD reaches it, and that retry can succeed.
pub fn drop_unresolvable_queue_entry(ctx: &Ctx, action: &YtAction, failures: &[String]) -> bool {
    let (YtAction::ReplaceAndPlay(song_id) | YtAction::ReplaceInPlace(song_id)) = action else {
        return false;
    };
    if !is_permanent_resolve_failure(failures) {
        return false;
    }
    let song_id = *song_id;
    // The row may already be gone (a repeated resolve, a queue edit).
    if !ctx.queue.iter().any(|song| song.id == song_id) {
        return true;
    }
    let reason = unresolvable_reason(failures);
    ctx.command(move |client| {
        let _ = client.delete_id(song_id);
        Ok(())
    });
    status_warn!("Cannot play this stream ({reason}) — removed it from the queue");
    true
}
/// Whether yt-dlp's error is final for this entry. Everything else (a network
/// hiccup, a throttled resolve, an expired token) is left alone: the entry is
/// retried when MPD reaches it.
fn is_permanent_resolve_failure(failures: &[String]) -> bool {
    const FATAL: [&str; 8] = [
        "drm",
        "private video",
        "unavailable",
        "not available",
        "removed",
        "terminated",
        "does not exist",
        "no video formats found",
    ];
    let raw = failures.first().map_or("", String::as_str).to_lowercase();
    FATAL.iter().any(|needle| raw.contains(needle))
}
/// The user-facing reason, without yt-dlp's `ERROR:` prefix or the URL the row
/// was queued with (the common cases are named plainly).
fn unresolvable_reason(failures: &[String]) -> String {
    let raw = failures.first().map_or("", String::as_str);
    let error = raw.split_once(": ").map_or(raw, |(_, error)| error);
    let error = error.trim().trim_start_matches("ERROR:").trim();
    let lower = error.to_lowercase();
    if lower.contains("drm") {
        return "the track is DRM protected".to_owned();
    }
    if lower.contains("private") {
        return "it is private".to_owned();
    }
    if lower.contains("unavailable") || lower.contains("not available") {
        return "it is unavailable".to_owned();
    }
    if lower.contains("removed") || lower.contains("terminated") || lower.contains("does not exist")
    {
        return "it is no longer on the service".to_owned();
    }
    if error.is_empty() { "it could not be resolved".to_owned() } else { error.to_owned() }
}
/// Round 105: where a row replacement may go — the row's absolute position in
/// the current playlist, or `None` when MPD no longer has the row (a cleared
/// queue, a consume-mode removal). A replacement must never re-add a row the
/// user cleared.
fn replacement_position(songs: &[Song], song_id: u32) -> Option<usize> {
    songs.iter().position(|song| song.id == song_id)
}
/// Round 107: the URL to hand the player, routed through the local caching
/// buffer when there is a stable upstream to key on. Audio always buffers
/// (MPD gets the proxy URL, keyed by the original link); video buffers only
/// a direct stream URL, because a YouTube-style watch link is resolved by
/// mpv itself. `None` keeps the caller's own URL.
fn buffered_player_url(
    ctx: &Ctx,
    class: crate::core::stream_cache::StreamClass,
    item: &crate::shared::ytdlp::YtStreamInfo,
) -> Option<String> {
    use crate::core::stream_cache::{StreamClass, proxy_url};
    let cache_dir = ctx.config.cache_dir.as_deref();
    match class {
        StreamClass::Audio => {
            let source =
                if item.original_url.is_empty() { item.url.as_str() } else { item.original_url.as_str() };
            Some(proxy_url(cache_dir, class, source, &item.url))
        }
        StreamClass::Video => (item.original_url.is_empty() && item.url.starts_with("http"))
            .then(|| proxy_url(cache_dir, class, &item.url, &item.url)),
    }
}

/// The class a `YtAction`'s resolved streams belong to: the video actions
/// fill the persistent mpv playlist, everything else the MPD queue.
fn action_class(action: &YtAction) -> crate::core::stream_cache::StreamClass {
    use crate::core::stream_cache::StreamClass;
    match action {
        YtAction::PlayVideo
        | YtAction::AddToVideoQueue
        | YtAction::AppendVideoQueue
        | YtAction::AddToVideoQueueAndPlay => StreamClass::Video,
        _ => StreamClass::Audio,
    }
}

/// Apply the resolved YouTube streams: play the first one as a temporary
/// entry, or add them to the queue (order preserved).
pub fn apply_resolved_streams(
    ctx: &Ctx,
    info: Vec<crate::shared::ytdlp::YtStreamInfo>,
    action: YtAction,
    failures: Vec<String>,
) {
    if info.is_empty() {
        return;
    }
    // Round 91b: stamp the resolve so a later play of the same link can
    // reuse it ([`fresh_cached_stream_info`]) instead of running yt-dlp
    // again.
    let mut info = info;
    let resolved_at = now_epoch_secs();
    for item in &mut info {
        item.resolved_at = Some(resolved_at);
    }
    // Round 107: where each resolved stream will actually play from — the
    // local cache proxy when it applies, else the CDN URL. Registering the
    // upstream here is also what makes the buffer's stable URL resolvable.
    let class = action_class(&action);
    let player_urls: Vec<Option<String>> =
        info.iter().map(|item| buffered_player_url(ctx, class, item)).collect();
    {
        let mut yt_info = ctx.yt_info.borrow_mut();
        let mut chapters = ctx.chapters.borrow_mut();
        for (idx, item) in info.iter().enumerate() {
            if !item.title.is_empty() {
                yt_info.insert(item.url.clone(), item.clone());
                if !item.original_url.is_empty() && item.original_url != item.url {
                    yt_info.insert(item.original_url.clone(), item.clone());
                }
                // The playing URI is the buffered proxy URL for an audio
                // row, so its metadata must be keyed by that string too.
                if let Some(player) = player_urls.get(idx).and_then(Option::as_ref)
                    && player != &item.url
                {
                    yt_info.insert(player.clone(), item.clone());
                }
            }
            if !item.chapters.is_empty() {
                // Round 90.3: the keys matter — a video session looks its
                // chapters up by the mpv playlist entry's URL, so a resolve
                // stored under a different string shows no chapters.
                log::debug!(
                    url = item.url.as_str(), original = item.original_url.as_str(),
                    chapters = item.chapters.len(); "Stored the resolved chapters"
                );
                chapters.insert(item.url.clone(), item.chapters.clone());
                if !item.original_url.is_empty() && item.original_url != item.url {
                    chapters.insert(item.original_url.clone(), item.chapters.clone());
                }
                if let Some(player) = player_urls.get(idx).and_then(Option::as_ref)
                    && player != &item.url
                {
                    chapters.insert(player.clone(), item.chapters.clone());
                }
            }
        }
    }
    let cache_dir = ctx.config.cache_dir.as_deref();
    let mut cache = load_yt_cache(cache_dir);
    for (idx, item) in info.iter().enumerate() {
        if !item.title.is_empty() {
            cache.insert(item.url.clone(), item.clone());
            if !item.original_url.is_empty() && item.original_url != item.url {
                cache.insert(item.original_url.clone(), item.clone());
            }
            if let Some(player) = player_urls.get(idx).and_then(Option::as_ref)
                && player != &item.url
            {
                cache.insert(player.clone(), item.clone());
            }
        }
    }
    save_yt_cache(cache_dir, &cache);
    // The URL each item actually plays from.
    let urls: Vec<String> = info
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            player_urls
                .get(idx)
                .and_then(Option::as_ref)
                .cloned()
                .unwrap_or_else(|| item.url.clone())
        })
        .collect();
    let count = urls.len();
    match action {
        YtAction::Play => {
            arm_start_offsets(ctx, &info, &player_urls);
            let url = urls[0].clone();
            ctx.query()
                .id(PASTE_PLAY)
                .replace_id(PASTE_PLAY)
                .target(PaneType::Radio { tree: TreeBrowserArgs::default() })
                .query(move |client| {
                    let id = client.add_id(&url, None)?;
                    client.play_id(id)?;
                    Ok(crate::MpdQueryResult::Any(Box::new(id)))
                });
        }
        YtAction::PlayVideo => {
            use crate::core::mpv::MpvPlaylistEntry;
            let mut entries: Vec<MpvPlaylistEntry> = info
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    // Round 107: a direct stream URL plays from the buffer;
                    // a YouTube-style watch link keeps being resolved by mpv.
                    let url = player_urls
                        .get(idx)
                        .and_then(Option::as_ref)
                        .cloned()
                        .unwrap_or_else(|| {
                            if item.original_url.is_empty() {
                                item.url.clone()
                            } else {
                                item.original_url.clone()
                            }
                        });
                    let mut entry = MpvPlaylistEntry::new(item.title.clone(), url, None);
                    entry.original_url =
                        (!item.original_url.is_empty()).then(|| item.original_url.clone());
                    entry.start_secs = item.start_secs;
                    entry
                })
                .collect();
            for failure in failures {
                if let Some(url) = failure.split_once(": ").map(|(url, _)| url.to_owned()) {
                    entries.push(MpvPlaylistEntry::new(url.clone(), url, None));
                }
            }
            crate::core::mpv::play_video_entries(ctx, entries);
        }
        YtAction::AddAfterCurrentAndPlay => {
            arm_start_offsets(ctx, &info, &player_urls);
            let has_current = ctx.find_current_song_in_queue().is_some();
            let autoplay_idx = ctx
                .find_current_song_in_queue()
                .map(|(idx, _)| idx + 1)
                .unwrap_or_else(|| ctx.queue.len());
            let mut ordered = urls;
            ordered.reverse();
            ctx.command(move |client| {
                let position = has_current.then_some(QueuePosition::RelativeAdd(0));
                for url in &ordered {
                    client.add(url, position)?;
                }
                client.play_position_safe(autoplay_idx)?;
                Ok(())
            });
            status_info!("Added {count} item(s) to the queue and started playback");
        }
        YtAction::Append => {
            arm_start_offsets(ctx, &info, &player_urls);
            ctx.command(move |client| {
                for url in &urls {
                    client.add(url, None)?;
                }
                Ok(())
            });
            status_info!("Appended {count} item(s) to the queue");
        }
        YtAction::ReplaceAndPlay(song_id) => {
            // Round 91: the entry is still a tagged link when this is the
            // **first** play of a pasted stream (the Queue tab's Enter or an
            // MPD advance), not an expired signed URL. Such an entry never
            // played, so a pasted start offset (`?t=90`) still has to be
            // armed; the expired-URL case deliberately is not (see
            // `arm_start_offsets`).
            let first_play = ctx.queue.iter().any(|song| {
                song.id == song_id
                    && crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some()
            });
            if first_play {
                arm_start_offsets(ctx, &info, &player_urls);
            }
            let url = urls[0].clone();
            ctx.command(move |client| {
                let songs = client.playlist_info()?;
                // Round 105: the row this resolve was for no longer exists (a
                // cleared queue, a consume-mode removal). Never re-add it — a
                // resolve landing after a clear used to append the fresh URL
                // at the end and play it ("entries I attempted to play earlier
                // started resolving and adding to the queue after").
                // `ReplaceInPlace` declines the same way (round 98).
                let Some(position) =
                    songs.as_deref().and_then(|songs| replacement_position(songs, song_id))
                else {
                    log::debug!(song_id; "The resolved queue entry is gone; not re-adding it");
                    return Ok(());
                };
                let _ = client.delete_id(song_id);
                let new_id = client.add_id(&url, Some(QueuePosition::Absolute(position)))?;
                client.play_id(new_id)?;
                Ok(())
            });
            status_info!(
                "{}",
                if first_play {
                    "Resolved the stream — playing it"
                } else {
                    "Stream URL expired — re-resolved from the original link"
                }
            );
        }
        YtAction::ReplaceInPlace(song_id) => {
            // Round 98 (round-95b follow-up): swap the *next* queue entry for
            // its resolved stream and leave playback alone. MPD prefetches
            // the next song before the current one ends; an unplayable link
            // there fails that prefetch, and the app's error recovery then
            // played the entry, cutting the current track short. With the row
            // swapped ahead of time MPD finds a playable URL and simply plays
            // it when the current track reaches its end.
            let url = urls[0].clone();
            let logged = url.clone();
            ctx.command(move |client| {
                let songs = client.playlist_info()?;
                // The entry is already gone (a second resolve, or the user
                // removed it): never append a stray copy.
                let Some(position) =
                    songs.as_deref().and_then(|songs| replacement_position(songs, song_id))
                else {
                    return Ok(());
                };
                let _ = client.delete_id(song_id);
                let _ = client.add_id(&url, Some(QueuePosition::Absolute(position)))?;
                Ok(())
            });
            log::debug!(song_id, url = logged.as_str(); "Prepared the next queue entry's stream");
            // Round 107: use the buffer system for the look ahead — begin
            // filling the next row's bytes now, so the transition is
            // gap-free without relying on MPD's `audio_buffer_size` ring.
            if let Some(item) = info.first() {
                let source = if item.original_url.is_empty() {
                    item.url.as_str()
                } else {
                    item.original_url.as_str()
                };
                crate::core::stream_cache::preload(cache_dir, class, source, &item.url);
            }
        }
        YtAction::AddToVideoQueue
        | YtAction::AppendVideoQueue
        | YtAction::AddToVideoQueueAndPlay => {
            let after_current = !matches!(action, YtAction::AppendVideoQueue);
            use crate::core::mpv::MpvPlaylistEntry;
            let mut entries: Vec<MpvPlaylistEntry> = info
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    let url = player_urls
                        .get(idx)
                        .and_then(Option::as_ref)
                        .cloned()
                        .unwrap_or_else(|| {
                            if item.original_url.is_empty() {
                                item.url.clone()
                            } else {
                                item.original_url.clone()
                            }
                        });
                    let mut entry = MpvPlaylistEntry::new(item.title.clone(), url, None);
                    entry.original_url =
                        (!item.original_url.is_empty()).then(|| item.original_url.clone());
                    entry.start_secs = item.start_secs;
                    entry
                })
                .collect();
            for failure in &failures {
                if let Some(url) = failure.split_once(": ").map(|(url, _)| url.to_owned()) {
                    entries.push(MpvPlaylistEntry::new(url.clone(), url, None));
                }
            }
            if matches!(action, YtAction::AddToVideoQueueAndPlay) {
                crate::core::mpv::add_to_video_playlist(ctx, entries.clone(), after_current);
                crate::core::mpv::play_video_entries(ctx, entries);
                status_info!("Added {count} item(s) to the video queue and started playback");
            } else {
                crate::core::mpv::add_to_video_playlist(ctx, entries, after_current);
                status_info!(
                    "{} {count} item(s) to the video queue",
                    if after_current { "Added" } else { "Appended" }
                );
            }
        }
        YtAction::Refresh => {
            // Round 78: the paste popup asks for a pasted link's stream info
            // before it is played so the download options can list its
            // chapters — refresh the open popup in place once it lands.
            if ctx.paste_modal_items.borrow().is_some() {
                refresh_paste_modal(ctx);
            }
        }
    }
}
/// Rendered hint used by other panes is not needed here.
/// Cache file of resolved YouTube stream info: `<cache_dir>/yt-info.json`
/// (default `~/.cache/s2udio`, round 19 — s2udio-only cache), so a
/// restart can restore the info for a still-playing stream and re-fetch
/// it.
pub fn yt_cache_path(cache_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    s2udio_cache_path(cache_dir, "yt-info.json")
}
pub fn load_yt_cache(
    cache_dir: Option<&std::path::Path>,
) -> std::collections::HashMap<String, crate::shared::ytdlp::YtStreamInfo> {
    std::fs::read(yt_cache_path(cache_dir))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}
pub fn save_yt_cache(
    cache_dir: Option<&std::path::Path>,
    cache: &std::collections::HashMap<String, crate::shared::ytdlp::YtStreamInfo>,
) {
    let path = yt_cache_path(cache_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_vec(cache) {
        let _ = std::fs::write(path, data);
    }
}
/// Round 95: what the app knows about a queue row that is a YouTube-style
/// stream: the resolved stream info when there is one (it carries the
/// chapters and the subscribers too), else the flat playlist listing's
/// metadata. `None` for local files and for a stream nothing has been
/// resolved or listed yet.
pub fn stream_row_meta(ctx: &Ctx, file: &str) -> Option<YtListMeta> {
    let link = untag_stream_link(file);
    // Cheap guard: the lookup below falls back to a scan of the whole
    // resolve cache (`stream_info` matches an entry by its original link),
    // and the queue table asks this for **every** row of every frame. Local
    // files never have stream info, so they stop here.
    if link == file && !crate::ui::panes::radio::is_stream_url(file) {
        return None;
    }
    let resolved = crate::ui::panes::playlists::stream_info(ctx, file).or_else(|| {
        (link != file).then(|| crate::ui::panes::playlists::stream_info(ctx, link)).flatten()
    });
    if let Some(info) = resolved.filter(|info| !info.title.is_empty() || info.duration.is_some()) {
        return Some(YtListMeta {
            title: info.title,
            channel: info.channel,
            duration: info.duration,
        });
    }
    ctx.yt_list_meta.borrow().get(&stream_meta_key(link)).cloned()
}
/// Round 95: the queue renders this row as a **stream row** — a tagged
/// stream link (queued, waiting to be resolved) or a stream URL the app has
/// info for (a resolved YouTube stream, or one being parsed right now).
/// Everything else (local files, radio stations, Jellyfin items) keeps its
/// normal rendering.
pub fn is_stream_row(ctx: &Ctx, file: &str) -> bool {
    if tagged_stream_entry(file).is_some() {
        return true;
    }
    crate::ui::panes::radio::is_stream_url(file)
        && (ctx.yt_info.borrow().contains_key(file) || stream_parse_pending(ctx, file))
}
/// Round 95: mark the links whose resolve has just been requested, so the
/// queue's Duration column can spin while their duration is still unknown.
pub fn mark_stream_parse_pending(ctx: &Ctx, links: &[String]) {
    let now = std::time::Instant::now();
    let max_age = crate::ctx::PENDING_YT_LINK_MAX_AGE;
    let mut pending = ctx.pending_yt_links.borrow_mut();
    pending.retain(|_, at| now.duration_since(*at) <= max_age);
    for link in links {
        pending.insert(stream_meta_key(link), now);
    }
}
/// Round 95: the resolve of these links landed (successfully or not) — one
/// fewer row spins in the queue.
pub fn clear_stream_parse_pending(ctx: &Ctx, links: &[String]) {
    let mut pending = ctx.pending_yt_links.borrow_mut();
    for link in links {
        pending.remove(&stream_meta_key(link));
    }
    if pending.is_empty() {
        pending.shrink_to_fit();
    }
}
/// Round 107: whether this exact queue row `(file, song id)` has a resolve
/// in flight — the row the user just started (a double-click or Space) or one
/// the look ahead is warming. The queue's play-marker column renders a
/// spinner for such a row until playback begins.
pub fn stream_resolve_pending_for(ctx: &Ctx, file: &str, song_id: u32) -> bool {
    ctx.pending_stream_resolve
        .borrow()
        .iter()
        .any(|(uri, id)| *id == song_id && uri == file)
}
/// Round 95: is a resolve for this queue row's link in flight right now? A
/// marker older than [`crate::ctx::PENDING_YT_LINK_MAX_AGE`] is stale (a work
/// thread that never reported back) and is dropped.
pub fn stream_parse_pending(ctx: &Ctx, file: &str) -> bool {
    // A resolved row is keyed by its stream URL, while the marker was set
    // for the link it was resolved from - follow the cached info back to
    // that link before giving up.
    let key = {
        let pending = ctx.pending_yt_links.borrow();
        let link_key = stream_meta_key(file);
        if pending.contains_key(&link_key) {
            link_key
        } else {
            let from_info = ctx
                .yt_info
                .borrow()
                .get(file)
                .map(|info| stream_meta_key(&info.original_url))
                .filter(|key| pending.contains_key(key));
            from_info.unwrap_or(link_key)
        }
    };
    let mut pending = ctx.pending_yt_links.borrow_mut();
    let Some(at) = pending.get(&key).copied() else { return false };
    if at.elapsed() > crate::ctx::PENDING_YT_LINK_MAX_AGE {
        pending.remove(&key);
        return false;
    }
    true
}

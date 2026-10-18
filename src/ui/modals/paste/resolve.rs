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
    {
        let info = ctx.yt_info.borrow();
        if let Some(found) = info
            .get(url)
            .cloned()
            .or_else(|| info.values().find(|entry| entry.original_url == url).cloned())
        {
            return Some(found);
        }
    }
    // Round 111: a **buffered** row's file is this app's own proxy URL
    // (`/s2u/audio/<key>`), which nothing in the yt-info cache is keyed by —
    // the cache holds the YouTube link. Resolve the key back to that link
    // through the stream-buffer manifest, once, here: every consumer of this
    // primitive (artwork, the queue's info box, MPRIS metadata) then works for
    // a restored stream row exactly as it does for a freshly added one.
    let link =
        crate::core::stream_cache::original_link_for_url(ctx.config.cache_dir.as_deref(), url)?;
    let info = ctx.yt_info.borrow();
    info.get(&link)
        .cloned()
        .or_else(|| info.values().find(|entry| entry.original_url == link).cloned())
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
    if let Err(err) = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
        urls: vec![object],
        action,
        meta_only: false,
    }) {
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
        return crate::core::stream_cache::original_link_for_url(
            ctx.config.cache_dir.as_deref(),
            url,
        )
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
    // Round 111: mark the resolve in flight, which is what the info box's
    // `[YouTube] Resolving <spinner>` row renders off. Without this the marker
    // was only ever set on the play and look-ahead paths, so the spinner never
    // appeared for a merely-selected row.
    mark_stream_parse_pending(ctx, std::slice::from_ref(&link));
    if let Err(err) = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
        urls: vec![link],
        action: YtAction::Refresh,
        // Round 111: display metadata only — a merely-selected row needs its
        // title/artwork, not a playable URL. The full resolve runs when the
        // row actually plays.
        meta_only: true,
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
    let song_id = *song_id;
    // A waiting `▶▶` step whose resolve produced nothing can never be served:
    // there is no stream to play. Clear the marker here, or the `▶▶` spinner
    // spins forever and every later press is coalesced into a step that will
    // never land. Only `ReplaceAndPlay` carries a step's play intent — an
    // in-place warm-up must leave the marker alone.
    if matches!(action, YtAction::ReplaceAndPlay(_))
        && ctx.pending_next_step.get() == Some(song_id)
    {
        ctx.pending_next_step.set(None);
    }
    if !is_permanent_resolve_failure(failures) {
        return false;
    }
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
/// What a `ReplaceAndPlay` landing should act on, given the queue MPD reports
/// at that moment.
///
/// The row is named by `song_id`, but **that id does not survive the
/// one-entry look ahead's in-place swap** (`delete_id` + `add_id` gives the row
/// a new one). A step's play intent therefore cannot be keyed on the id alone:
/// when the id is gone, the row is re-found by the *stream it now holds* — the
/// URL this very resolve was about to write, or the buffered proxy URL that
/// swap wrote for the same original link.
#[derive(Debug, PartialEq, Eq)]
enum ReplacementTarget {
    /// The row this resolve was built for is still there: swap it and play it.
    Row(usize),
    /// The id is gone, but a row already holds the stream this resolve produced
    /// — the look ahead swapped the *same* row in place first. Play that row
    /// instead of re-adding a second copy or dropping the step.
    AlreadySwapped(u32),
    /// Nothing to do: the id is gone and no row holds this stream (a cleared
    /// queue, a consume-mode removal). Never re-add a row the user cleared.
    Nothing,
}
/// Resolve what a landed `ReplaceAndPlay` should act on — see
/// [`ReplacementTarget`].
///
/// `stream_keys` are the strings that identify *the stream this resolve
/// produced*: the player URL and the original link it came from. Matching on
/// them (rather than on `song_id`) is what lets a step survive the look ahead's
/// swap, because the swap replaces the row's file with that very player URL.
///
/// Extracted so the choice is testable without driving the MPD client closure
/// it feeds, and covered end to end by `resolve_e2e_tests`.
fn replacement_target(
    cache_dir: Option<&std::path::Path>,
    songs: &[Song],
    song_id: u32,
    stream_keys: &[String],
    step_waits: bool,
) -> ReplacementTarget {
    if let Some(position) = replacement_position(songs, song_id) {
        return ReplacementTarget::Row(position);
    }
    // The id is gone. A step that is still waiting must not be dropped: the
    // look ahead has already put this stream in the queue, so playing it is
    // exactly what the step asked for.
    if step_waits
        && let Some(existing) = songs
            .iter()
            .find(|song| stream_keys.iter().any(|key| key_matches_row(cache_dir, key, &song.file)))
            .map(|song| song.id)
    {
        return ReplacementTarget::AlreadySwapped(existing);
    }
    ReplacementTarget::Nothing
}
/// Whether a queue row's `file` is the stream named by `key`.
///
/// A row holds one of three forms for the same stream: the original pasted link
/// (`…watch?v=ID#s2u-audio`), the resolved player URL, or the buffered proxy URL
/// keyed by the original link. Comparing the *link* is what ties them together,
/// so a row the look ahead swapped is recognised as the step's own row.
fn key_matches_row(cache_dir: Option<&std::path::Path>, key: &str, file: &str) -> bool {
    if file == key {
        return true;
    }
    // A still-untouched tagged row: the key is its link.
    if let Some((link, _)) = crate::shared::ytdlp::tagged_stream_entry(file) {
        return link == key;
    }
    // A row the look ahead already swapped: the key is the original link, and
    // the proxy URL remembers it.
    crate::core::stream_cache::original_link_for_url(cache_dir, file)
        .is_some_and(|link| link == key)
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
            // The strings that identify *this* stream in a queue row: the URL
            // the row will hold once written, and the original link it came
            // from. The look ahead's in-place swap writes the same pair, so
            // either one re-finds the row after the swap changed its id.
            let mut stream_keys = vec![url.clone()];
            if let Some(item) = info.first() {
                if !item.original_url.is_empty() {
                    stream_keys.push(item.original_url.clone());
                }
                if !item.url.is_empty() {
                    stream_keys.push(item.url.clone());
                }
            }
            // 2026-10-15: a `▶▶` step's resolve can lose the race with the
            // one-entry look ahead, which swaps **the same row** in place
            // (`ReplaceInPlace`): the row then exists, but under a *new* id, so
            // the `song_id` this resolve was built for is gone. Bailing there
            // played nothing at all — the reported "an unwarmed ▶▶ never causes
            // the selection to update when the track actually starts playing",
            // whose log reads `stepping="true"` then
            // "The resolved queue entry is gone; not re-adding it".
            //
            // When a step is waiting, the row already holding this stream is
            // therefore taken over and played: the look ahead did the swap, this
            // resolve supplies the play.
            let step_waits = ctx.pending_next_step.get() == Some(song_id);
            // Resolved outside the closure: `ctx` is not `Send`, and the
            // manifest lookup needs the same cache dir the proxy was started
            // with.
            let cache_dir = ctx.config.cache_dir.clone();
            ctx.command(move |client| {
                let songs = client.playlist_info()?.unwrap_or_default();
                match replacement_target(
                    cache_dir.as_deref(),
                    &songs,
                    song_id,
                    &stream_keys,
                    step_waits,
                ) {
                    ReplacementTarget::Row(position) => {
                        let _ = client.delete_id(song_id);
                        let new_id =
                            client.add_id(&url, Some(QueuePosition::Absolute(position)))?;
                        client.play_id(new_id)?;
                    }
                    ReplacementTarget::AlreadySwapped(existing) => {
                        log::debug!(
                            song_id, existing;
                            "The step's row was swapped in by the look ahead; playing it"
                        );
                        client.play_id(existing)?;
                    }
                    ReplacementTarget::Nothing => {
                        log::debug!(song_id; "The resolved queue entry is gone; not re-adding it");
                    }
                }
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
            if step_waits {
                // The step has been served: it played the row holding this
                // stream (either the row it re-added, or the one the look
                // ahead had already swapped in). Clear the marker so the `▶▶`
                // spinner stops and a later press is not coalesced away.
                //
                // Note this is the arm that clears it, not `ReplaceInPlace`:
                // a warm-up that clears the step's marker leaves the step's own
                // landing with nothing to do, which was the round-9 defect.
                ctx.pending_next_step.set(None);
            }
        }
        YtAction::ReplaceInPlace(song_id) => {
            // Round 98 (round-95b follow-up): swap the *next* queue entry for
            // its resolved stream and leave playback alone. MPD prefetches
            // the next song before the current one ends; an unplayable link
            // there fails that prefetch, and the app's error recovery then
            // played the entry, cutting the current track short. With the row
            // swapped ahead of time MPD finds a playable URL and simply plays
            // it when the current track reaches its end.
            //
            // Round 112: this resolve may be the target of a `▶▶` / `>` step,
            // which resolves the row it would land on and starts it once it is
            // playable (asking MPD to step onto the raw link is what made the
            // highlight run down the list and playback land on the wrong row).
            //
            // 2026-10-15 (round 10): this arm **never starts playback**. It is
            // the one-entry look ahead's warm-up — "swap the *next* queue entry
            // for its resolved stream and leave playback alone" — and a warm-up
            // that calls `play_id` cuts the current track short, which is the
            // very defect the arm was written to prevent.
            //
            // It used to serve a waiting `▶▶` step here (`step_waits` →
            // `play_id`, added in e201c89), on the theory that the step's own
            // `ReplaceAndPlay` could not play the row afterwards because
            // `delete_id` + `add_id` gives it a new id. Driving the real
            // `Client` end to end shows what that actually did: the look ahead
            // won the race, started the track *and* cleared
            // `ctx.pending_next_step`, so the step's own landing saw
            // `step_waits == false`, took the `Nothing` branch and issued no
            // play at all — while the app's spinner and follow logic, which key
            // off `pending_next_step`, never learned playback had begun. The
            // play intent belongs to the arm that means to play.
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
                client.add_id(&url, Some(QueuePosition::Absolute(position)))?;
                Ok(())
            });
            log::debug!(
                song_id, url = logged.as_str();
                "Prepared the next queue entry's stream"
            );
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
    // Round 111: a buffered proxy URL embeds an ephemeral port and a cache
    // key, so it names nothing the yt-info cache knows. Resolve it back to
    // the original link through the stream cache's manifest first — that
    // mapping is persisted, which is what makes a restored queue row show its
    // title and duration at all.
    let buffered_link =
        crate::core::stream_cache::original_link_for_url(ctx.config.cache_dir.as_deref(), file);
    let file = buffered_link.as_deref().unwrap_or(file);
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
/// The queue's total running time.
///
/// MPD only knows a duration for media it can read; for a web-stream row it
/// returns the URL and nothing else, so a queue of 98 YouTube streams summed
/// to **zero** even though every row's Duration cell rendered a real value
/// from the yt-info cache (round 111). The total therefore falls back to the
/// same resolved metadata the rows use, so the number at the bottom of the
/// pane agrees with the column above it.
///
/// The fallback is consulted only when MPD has no duration, so local files are
/// unaffected and nothing is counted twice.
pub fn queue_total_time(ctx: &Ctx) -> std::time::Duration {
    ctx.queue
        .iter()
        .filter_map(|song| {
            song.duration.or_else(|| {
                stream_row_meta(ctx, &song.file)
                    .and_then(|meta| meta.duration)
                    .map(std::time::Duration::from_secs_f64)
            })
        })
        .sum()
}

pub fn is_stream_row(ctx: &Ctx, file: &str) -> bool {
    if tagged_stream_entry(file).is_some() {
        return true;
    }
    // Round 111: a buffered proxy URL is a web-stream row even though it is
    // not a tagged link and (after a restart) not yet in yt-info - the row is
    // still a stream, its metadata just has to be recovered through the
    // stream cache's manifest. Without this the row lost its Title/Channel
    // overlay and fell back to a raw `127.0.0.1` filename.
    if crate::core::stream_cache::is_buffered_url(file) {
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

/// The queue's "this row's stream is in flight" spinner state, and the
/// one-resolve-per-entry guard behind it.
#[cfg(test)]
mod stream_pending_tests {
    use super::*;
    use crate::mpd::commands::Song;

    fn ctx() -> Ctx {
        // The receivers are leaked: the code under test sends work requests,
        // and a dropped receiver would make every send fail.
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        Box::leak(Box::new(client.1.clone()));
        crate::tests::fixtures::ctx(app, work, client)
    }

    fn row(id: u32, file: &str) -> Song {
        Song { id, file: file.to_owned(), ..Default::default() }
    }

    #[test]
    fn marking_a_link_makes_its_row_pending_until_it_is_cleared() {
        let ctx = ctx();
        let link = "https://www.youtube.com/watch?v=EEE#s2u-audio".to_owned();
        assert!(!stream_parse_pending(&ctx, &link));

        mark_stream_parse_pending(&ctx, std::slice::from_ref(&link));
        assert!(stream_parse_pending(&ctx, &link), "the row spins while its resolve is in flight");

        clear_stream_parse_pending(&ctx, std::slice::from_ref(&link));
        assert!(!stream_parse_pending(&ctx, &link), "the resolve landed");
    }

    #[test]
    fn the_selection_warm_marks_its_row_pending() {
        // Round 111 regression: the warm never set the pending marker, so the
        // info box's `[YouTube] Resolving <spinner>` row had nothing to render
        // off and the box stayed empty until the resolve landed.
        let mut ctx = ctx();
        let link = "https://www.youtube.com/watch?v=GGG";
        let tagged = format!("{link}#s2u-audio");
        let song = row(7, &tagged);
        ctx.queue.push(song.clone());
        ctx.queue_selected_id.set(Some(song.id));
        ctx.status.state = crate::mpd::commands::State::Stop;

        assert!(!stream_parse_pending(&ctx, &tagged), "nothing is in flight before the warm runs");
        warm_selected_queue_info(&ctx);

        // The marker is keyed by the bare link, the pane asks with the row's
        // tagged file; both normalize to the same video id.
        assert!(
            stream_parse_pending(&ctx, &tagged),
            "the selected row shows its spinner while the warm resolves"
        );
        assert!(stream_parse_pending(&ctx, link), "the same marker is visible under the bare link");
    }

    #[test]
    fn a_resolved_row_still_reports_the_pending_marker_of_its_link() {
        // The marker is keyed by the link; the row is keyed by the stream URL
        // once it is replaced, and the spinner must follow it there.
        let ctx = ctx();
        let link = "https://www.youtube.com/watch?v=FFF".to_owned();
        let stream = "https://rr3.example/videoplayback?id=FFF".to_owned();
        mark_stream_parse_pending(&ctx, std::slice::from_ref(&link));
        ctx.yt_info.borrow_mut().insert(
            stream.clone(),
            crate::shared::ytdlp::YtStreamInfo {
                url: stream.clone(),
                original_url: link,
                title: "Resolved".to_owned(),
                ..Default::default()
            },
        );

        assert!(stream_parse_pending(&ctx, &stream));
    }

    #[test]
    fn a_tagged_queue_row_is_resolved_at_most_once() {
        let mut ctx = ctx();
        let link = "https://www.youtube.com/watch?v=GGG#s2u-audio".to_owned();
        ctx.queue = vec![row(7, &link)];

        assert!(resolve_tagged_queue_entry(&ctx, &link, 7), "the first call asks for the resolve");
        assert!(
            !resolve_tagged_queue_entry(&ctx, &link, 7),
            "the second call is absorbed (MPD keeps reporting the link as current)"
        );
    }

    #[test]
    fn only_audio_tagged_rows_are_resolved_by_the_queue_path() {
        let mut ctx = ctx();
        let video = "https://www.youtube.com/watch?v=HHH#s2u-video".to_owned();
        ctx.queue = vec![row(8, &video)];
        assert!(!resolve_tagged_queue_entry(&ctx, &video, 8));
    }

    #[test]
    fn a_resolved_row_is_not_resolved_again() {
        let mut ctx = ctx();
        let stream = "https://rr5.example/videoplayback?id=III".to_owned();
        ctx.queue = vec![row(9, &stream)];
        assert!(!resolve_tagged_queue_entry(&ctx, &stream, 9));
    }
}

/// Round 103: what happens to a queued stream entry whose resolve failed for
/// good (the DRM-protected SoundCloud track behind "nothing ends up playing").
#[cfg(test)]
mod unresolvable_tests {
    use super::*;
    use crate::mpd::commands::Song;

    /// yt-dlp's own line for the reported case, as `resolve_audio_urls` writes
    /// it into `failures` (`<url>: <error>`).
    const DRM: &str = "https://api.soundcloud.com/tracks/soundcloud%3Atracks%3A2092436136: \
                       ERROR: [soundcloud] 2092436136: This video is DRM protected";
    const UNAVAILABLE: &str = "https://www.youtube.com/watch?v=XXX: \
                               ERROR: [youtube] XXX: Video unavailable";

    fn rig(
        queue: Vec<Song>,
    ) -> (Ctx, crossbeam::channel::Receiver<crate::shared::events::ClientRequest>) {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        // Keep the fixture's receivers alive: the code under test sends on all
        // three channels.
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        let client_rx = client.1.clone();
        let mut ctx =
            crate::tests::fixtures::ctx((app.0, app.1), (work.0, work.1), (client.0, client.1));
        ctx.queue = queue;
        (ctx, client_rx)
    }

    fn row(id: u32, file: &str) -> Song {
        Song { id, file: file.to_owned(), ..Default::default() }
    }

    /// A track the service refuses for good leaves the queue instead of being
    /// left for MPD to fail and skip over.
    #[test]
    fn a_drm_protected_row_is_dropped() {
        let (ctx, client_rx) = rig(vec![row(4, "https://api.soundcloud.com/tracks/1#s2u-audio")]);

        assert!(drop_unresolvable_queue_entry(
            &ctx,
            &YtAction::ReplaceAndPlay(4),
            &[DRM.to_owned()]
        ));

        assert_eq!(client_rx.try_iter().count(), 1, "the row is deleted");
    }

    /// **A step whose resolve fails must release the `▶▶` spinner.**
    ///
    /// `pending_next_step` drives the spinner and coalesces later presses. If a
    /// failed resolve leaves it armed, the button spins forever and every
    /// subsequent press is swallowed as "already resolving" — the feature looks
    /// dead until a restart. Only the `ReplaceAndPlay` form carries a step's
    /// intent, so only that one may clear it.
    #[test]
    fn a_failed_step_resolve_releases_the_spinner() {
        let (ctx, _client_rx) = rig(vec![row(4, "https://www.youtube.com/watch?v=YYY#s2u-audio")]);
        ctx.pending_next_step.set(Some(4));

        // A permanent failure (the row is dropped) and a transient one (the row
        // is kept) both mean the step will never be served.
        assert!(drop_unresolvable_queue_entry(
            &ctx,
            &YtAction::ReplaceAndPlay(4),
            &[DRM.to_owned()]
        ));
        assert_eq!(ctx.pending_next_step.get(), None, "the spinner stops on a permanent failure");

        ctx.pending_next_step.set(Some(4));
        assert!(!drop_unresolvable_queue_entry(
            &ctx,
            &YtAction::ReplaceAndPlay(4),
            &["transient".to_owned()]
        ));
        assert_eq!(ctx.pending_next_step.get(), None, "and on a transient one too");
    }

    /// The other half of the rule: an in-place **warm-up** failing must leave a
    /// waiting step's marker alone. Clearing it there is what made the step's
    /// own landing take the `Nothing` branch and play nothing at all.
    #[test]
    fn a_failed_warm_up_leaves_a_waiting_step_armed() {
        let (ctx, _client_rx) = rig(vec![row(4, "https://www.youtube.com/watch?v=YYY#s2u-audio")]);
        ctx.pending_next_step.set(Some(4));

        assert!(drop_unresolvable_queue_entry(
            &ctx,
            &YtAction::ReplaceInPlace(4),
            &[DRM.to_owned()]
        ));

        assert_eq!(
            ctx.pending_next_step.get(),
            Some(4),
            "the step is still owed its play: a warm-up must not cancel it"
        );
    }

    /// A failure that can clear (a network error, a throttled resolve) keeps
    /// its row: MPD's decode-error recovery resolves it again when reached.
    #[test]
    fn a_transient_failure_keeps_the_row() {
        let (ctx, client_rx) = rig(vec![row(4, "https://www.youtube.com/watch?v=YYY#s2u-audio")]);
        let transient = "https://www.youtube.com/watch?v=YYY: \
                         ERROR: unable to download video data: HTTP Error 503"
            .to_owned();

        assert!(!drop_unresolvable_queue_entry(&ctx, &YtAction::ReplaceAndPlay(4), &[transient]));

        assert_eq!(client_rx.try_iter().count(), 0, "nothing is deleted");
    }

    /// The messages name the common causes in plain words.
    #[test]
    fn the_reason_names_the_common_causes() {
        assert_eq!(unresolvable_reason(&[DRM.to_owned()]), "the track is DRM protected");
        assert_eq!(unresolvable_reason(&[UNAVAILABLE.to_owned()]), "it is unavailable");
        assert_eq!(
            unresolvable_reason(&[]),
            "it could not be resolved",
            "an empty failure list still reads as a sentence"
        );
        assert!(!is_permanent_resolve_failure(&[]), "nothing known is not permanent");
    }
}

/// Round 105: stale resolved rows (their signed googlevideo URL expired) are
/// re-resolved from the original link, and a replacement never re-adds a row
/// that is gone.
#[cfg(test)]
mod stale_stream_tests {
    use super::*;
    use crate::mpd::commands::Song;
    use crate::shared::events::WorkRequest;

    /// A signed URL whose `expire=` epoch (2001) is long past.
    const EXPIRED: &str = "https://rr2.googlevideo.com/videoplayback?expire=1000000000&id=x";
    /// A signed URL that expires in the far future (2199).
    const FRESH: &str = "https://rr2.googlevideo.com/videoplayback?expire=7258118400&id=y";
    const WATCH: &str = "https://www.youtube.com/watch?v=NNN";

    fn rig(queue: Vec<Song>) -> (Ctx, crossbeam::channel::Receiver<WorkRequest>) {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        // Keep the fixture's receivers alive: the code under test sends on all
        // three channels.
        Box::leak(Box::new(app.1.clone()));
        let work_rx = work.1.clone();
        Box::leak(Box::new(client.1.clone()));
        let mut ctx =
            crate::tests::fixtures::ctx((app.0, app.1), (work.0, work.1), (client.0, client.1));
        ctx.queue = queue;
        (ctx, work_rx)
    }

    fn row(id: u32, file: &str) -> Song {
        Song { id, file: file.to_owned(), ..Default::default() }
    }

    fn info_for(url: &str) -> crate::shared::ytdlp::YtStreamInfo {
        crate::shared::ytdlp::YtStreamInfo {
            url: url.to_owned(),
            original_url: WATCH.to_owned(),
            title: "Stale".to_owned(),
            ..Default::default()
        }
    }

    fn asks(work_rx: &crossbeam::channel::Receiver<WorkRequest>, url: &str, id: u32) -> bool {
        work_rx.try_iter().any(|request| {
            matches!(
                request,
                WorkRequest::ResolveYtStreams { urls, action, .. }
                    if urls == vec![url.to_owned()]
                        && matches!(action, YtAction::ReplaceAndPlay(found) if found == id)
            )
        })
    }

    /// An expired resolved row is re-resolved from its original link, and the
    /// one-per-entry guard absorbs a second request for the same row.
    #[test]
    fn an_expired_resolved_row_is_resolved_from_its_original_link() {
        let (ctx, work_rx) = rig(vec![row(3, EXPIRED)]);
        ctx.yt_info.borrow_mut().insert(EXPIRED.to_owned(), info_for(EXPIRED));

        assert!(
            re_resolve_expired_queue_entry(&ctx, EXPIRED, 3, true),
            "the re-resolve is requested"
        );
        assert!(
            asks(&work_rx, WATCH, 3),
            "the original link is resolved with ReplaceAndPlay for the row"
        );
        assert!(
            !re_resolve_expired_queue_entry(&ctx, EXPIRED, 3, true),
            "a second hit in the same poll is absorbed (the marker)"
        );
    }

    /// The in-place arm (`ReplaceInPlace`) is what the look-ahead and the
    /// prefetch-error recovery use.
    #[test]
    fn the_in_place_arm_resolves_the_original_link_too() {
        let (ctx, work_rx) = rig(vec![row(4, EXPIRED)]);
        ctx.yt_info.borrow_mut().insert(EXPIRED.to_owned(), info_for(EXPIRED));

        assert!(re_resolve_expired_queue_entry(&ctx, EXPIRED, 4, false));
        assert!(
            work_rx.try_iter().any(|request| matches!(
                request,
                WorkRequest::ResolveYtStreams { urls, action, .. }
                    if urls == vec![WATCH.to_owned()]
                        && matches!(action, YtAction::ReplaceInPlace(found) if found == 4)
            )),
            "the in-place arm asks for the original link with ReplaceInPlace"
        );
    }

    /// A live resolved URL (not expired) is not this path's business — the
    /// queue play path handles it directly.
    #[test]
    fn a_live_resolved_row_is_left_alone() {
        let (ctx, work_rx) = rig(vec![row(5, FRESH)]);
        ctx.yt_info.borrow_mut().insert(FRESH.to_owned(), info_for(FRESH));

        assert!(!re_resolve_expired_queue_entry(&ctx, FRESH, 5, true));
        assert_eq!(work_rx.try_iter().count(), 0, "no resolve is requested");
    }

    /// An expired URL whose original link is not remembered cannot be
    /// re-resolved — the play path explains the failure instead.
    #[test]
    fn an_expired_row_without_a_remembered_original_is_left_alone() {
        let (ctx, work_rx) = rig(vec![row(6, EXPIRED)]);
        assert!(!re_resolve_expired_queue_entry(&ctx, EXPIRED, 6, true));
        assert_eq!(work_rx.try_iter().count(), 0, "nothing is requested");
    }

    /// **The reported bug: "an unwarmed ▶▶ never causes the selection to update
    /// when the track actually starts playing."**
    ///
    /// The step's resolve can lose the race with the one-entry look ahead, which
    /// swaps the same row in place and gives it a **new id**. The row then
    /// exists — holding exactly the stream this resolve produced — but the id
    /// the resolve was built for is gone. Bailing played nothing at all (the
    /// log read `stepping="true"` followed by "The resolved queue entry is
    /// gone"), so the step must take over that row and play it.
    #[test]
    fn a_step_takes_over_a_row_the_look_ahead_already_swapped() {
        let swapped = vec!["http://127.0.0.1:39235/s2u/audio/swapped".to_owned()];
        let songs = vec![row(1, "a"), row(2, &swapped[0]), row(3, "c")];
        assert_eq!(
            replacement_target(None, &songs, 99, &swapped, true),
            ReplacementTarget::AlreadySwapped(2),
            "the step's stream is already in the queue under a new id"
        );
    }

    /// A step's row after the look ahead swapped it holds the **buffered proxy
    /// URL**, not the raw stream URL this resolve produced: `apply_resolved_
    /// streams` rewrites an audio row to the proxy URL keyed by the original
    /// link. Matching the player URL alone would therefore miss the row and
    /// drop the step again, so the original link is a key too.
    #[test]
    fn a_step_finds_its_row_through_the_original_link_it_was_resolved_from() {
        let link = "https://www.youtube.com/watch?v=bbbbbbbbbbb";
        let proxy = crate::core::stream_cache::proxy_url(
            Some(std::env::temp_dir().join("s2udio-test-cache").as_path()),
            crate::core::stream_cache::StreamClass::Audio,
            link,
            "https://rr1.googlevideo.com/videoplayback?expire=7258118400&id=step",
        );
        // The look ahead already wrote the proxy URL for the same link.
        let songs = vec![row(1, "a"), row(2, &proxy), row(3, "c")];
        let keys = vec![
            link.to_owned(),
            "https://rr1.googlevideo.com/videoplayback?expire=7258118400&id=step".to_owned(),
        ];
        assert_eq!(
            replacement_target(None, &songs, 814, &keys, true),
            ReplacementTarget::AlreadySwapped(2),
            "the row is found by the original link the proxy URL was registered \
             from, not by the raw player URL"
        );
    }

    /// Without a waiting step the row is simply gone: never re-add it (a cleared
    /// queue, a consume-mode removal).
    #[test]
    fn a_landed_resolve_for_a_gone_row_is_dropped_when_no_step_waits() {
        let swapped = vec!["http://127.0.0.1:39235/s2u/audio/swapped".to_owned()];
        let songs = vec![row(1, "a"), row(2, &swapped[0])];
        assert_eq!(
            replacement_target(None, &songs, 99, &swapped, false),
            ReplacementTarget::Nothing,
            "no step is waiting, so nothing is played or re-added"
        );
    }

    /// The ordinary case: the row is still there, so it is swapped and played.
    #[test]
    fn a_landed_resolve_swaps_and_plays_its_own_row() {
        let songs = vec![row(1, "a"), row(2, "b")];
        assert_eq!(
            replacement_target(None, &songs, 2, &[], false),
            ReplacementTarget::Row(1)
        );
    }

    /// A waiting step whose stream is *not* in the queue either (the row was
    /// removed outright) is still dropped, not invented.
    #[test]
    fn a_step_whose_stream_is_absent_too_is_dropped() {
        let songs = vec![row(1, "a")];
        assert_eq!(
            replacement_target(None, &songs, 99, &["http://x/absent".to_owned()], true),
            ReplacementTarget::Nothing
        );
    }

    /// The replacement position is the row's own, and `None` once MPD no
    /// longer has the row — a replacement must never re-add a cleared row.
    #[test]
    fn replacement_position_finds_the_row_or_none() {
        let songs = vec![row(1, "a"), row(2, "b")];
        assert_eq!(replacement_position(&songs, 2), Some(1));
        assert_eq!(replacement_position(&songs, 9), None, "the row is gone");
    }

    /// A cleared queue wipes the in-flight audio-row markers.
    #[test]
    fn clearing_the_queue_wipes_the_audio_row_markers() {
        let (ctx, _work_rx) = rig(vec![row(7, EXPIRED)]);
        ctx.pending_stream_resolve.borrow_mut().insert((EXPIRED.to_owned(), 7));
        ctx.pending_yt_links.borrow_mut().insert(EXPIRED.to_owned(), std::time::Instant::now());

        clear_queue_resolve_markers(&ctx);

        assert!(ctx.pending_stream_resolve.borrow().is_empty());
        assert!(ctx.pending_yt_links.borrow().is_empty());
    }

    /// Round 107: the play-marker spinner keys on the exact row `(file, id)`.
    #[test]
    fn a_pending_entry_resolve_is_reported_for_its_row_only() {
        let (ctx, _work_rx) = rig(vec![row(7, WATCH)]);
        assert!(!stream_resolve_pending_for(&ctx, WATCH, 7));
        ctx.pending_stream_resolve.borrow_mut().insert((WATCH.to_owned(), 7));
        assert!(stream_resolve_pending_for(&ctx, WATCH, 7));
        assert!(!stream_resolve_pending_for(&ctx, WATCH, 8), "another row id");
        assert!(!stream_resolve_pending_for(&ctx, "other", 7), "another file");
    }
}

#[cfg(test)]
mod song_info_tests {
    use super::*;
    use crate::shared::ytdlp::{StreamIntent, tagged_stream_link};

    /// Round 107: the selection-time warm stores the info under the row's
    /// **original link**, so a selected row that still carries the tagged
    /// link must strip the tag to show its description (the same lookup the
    /// artwork already used). Without this the description only appeared once
    /// playback replaced the row with the resolved/buffered URL.
    #[test]
    fn a_tagged_row_finds_the_info_stored_under_its_original_link() {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        Box::leak(Box::new(client.1.clone()));
        let ctx =
            crate::tests::fixtures::ctx((app.0, app.1), (work.0, work.1), (client.0, client.1));

        let watch = "https://www.youtube.com/watch?v=TAG";
        ctx.yt_info.borrow_mut().insert(
            watch.to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                url: "https://rr1.googlevideo.com/videoplayback?expire=1".to_owned(),
                original_url: watch.to_owned(),
                title: "A track".to_owned(),
                description: Some("the description".to_owned()),
                ..Default::default()
            },
        );
        let tagged = tagged_stream_link(watch, StreamIntent::Audio);

        // The tagged form is not a key in the cache...
        assert!(stream_info_for(&ctx, &tagged).is_none());
        // ...but the tag-aware lookup finds it, description included.
        let found = song_stream_info(&ctx, &tagged).expect("found via the original link");
        assert_eq!(found.title, "A track");
        assert_eq!(found.description.as_deref(), Some("the description"));
    }

    /// Round 111 regression: once a stream has played, the row's file is the
    /// **buffered proxy URL**, and the cached info is still keyed by the
    /// YouTube link. `song_stream_info` matched neither the proxy URL nor the
    /// link, so artwork, the description and the info box went blank for every
    /// restored row — while the queue list's own columns (which use
    /// `stream_row_meta`) looked correct. That asymmetry is the reported
    /// "it's not parsing artwork/descriptions, but a freshly added playlist is
    /// fine".
    #[test]
    fn a_buffered_row_finds_its_info_through_the_manifest() {
        use crate::core::stream_cache;

        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        Box::leak(Box::new(client.1.clone()));
        let mut ctx =
            crate::tests::fixtures::ctx((app.0, app.1), (work.0, work.1), (client.0, client.1));

        // Real values taken from the reporter's own caches, so this test
        // exercises the shape that actually regressed: a real YouTube link, a
        // real title, a real thumbnail URL.
        let watch = "https://www.youtube.com/watch?v=BQ2P1K7Yl00";
        ctx.yt_info.borrow_mut().insert(
            watch.to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                url: "https://rr1.googlevideo.com/videoplayback?expire=1".to_owned(),
                original_url: watch.to_owned(),
                title: "So What".to_owned(),
                description: Some("a real description".to_owned()),
                thumbnail: Some("https://i.ytimg.com/vi_webp/BQ2P1K7Yl00/maxresdefault.webp".to_owned()),
                ..Default::default()
            },
        );

        // The row's file is the proxy URL, and the manifest is the only thing
        // that ties it back to the link.
        let key = "0f1f8a17fadadf20";
        let root = std::env::temp_dir()
            .join(format!("s2u-bufferedinfo-{}", std::process::id()))
            .join("stream-buffer");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
        std::fs::create_dir_all(root.join("audio")).unwrap();
        std::fs::write(
            root.join("index.json"),
            serde_json::json!({
                key: { "class": "audio", "original_url": watch, "size": 0, "total": null,
                       "complete": false, "content_type": null, "last_used": 0, "failed": false }
            })
            .to_string(),
        )
        .unwrap();
        ctx.config = std::sync::Arc::new(crate::config::Config {
            cache_dir: root.parent().map(std::path::Path::to_path_buf),
            ..(*ctx.config).clone()
        });

        let url = format!("http://127.0.0.1:39235/s2u/audio/{key}");
        assert!(
            stream_cache::is_buffered_url(&url),
            "the fixture URL must look like a buffered row"
        );

        let found = song_stream_info(&ctx, &url)
            .expect("a buffered row must resolve its info through the manifest");
        assert_eq!(found.title, "So What");
        assert_eq!(
            found.description.as_deref(),
            Some("a real description"),
            "the description (what the info box shows) must survive too"
        );
        assert!(found.thumbnail.is_some(), "artwork needs the thumbnail");

        // The primitive every consumer shares must resolve it too — album art
        // and the queue info box go through `stream_info_for`, not through
        // `song_stream_info`.
        assert!(
            stream_info_for(&ctx, &url).is_some(),
            "stream_info_for must resolve a buffered row, or art/info/MPRIS break"
        );
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// A buffered row for a video must resolve as well, so the mpv/MPRIS path
    /// is covered by the same fix.
    #[test]
    fn a_buffered_video_row_resolves_through_the_manifest() {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        Box::leak(Box::new(client.1.clone()));
        let mut ctx =
            crate::tests::fixtures::ctx((app.0, app.1), (work.0, work.1), (client.0, client.1));

        let watch = "https://www.youtube.com/watch?v=BUFFEREDVID";
        ctx.yt_info.borrow_mut().insert(
            watch.to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                url: "https://rr1.googlevideo.com/videoplayback?expire=2".to_owned(),
                original_url: watch.to_owned(),
                title: "A buffered video".to_owned(),
                ..Default::default()
            },
        );

        let key = "feedfacefeedface";
        let root = std::env::temp_dir()
            .join(format!("s2u-bufferedvid-{}", std::process::id()))
            .join("stream-buffer");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
        std::fs::create_dir_all(root.join("video")).unwrap();
        std::fs::write(
            root.join("index.json"),
            serde_json::json!({
                key: { "class": "video", "original_url": watch, "size": 0, "total": null,
                       "complete": false, "content_type": null, "last_used": 0, "failed": false }
            })
            .to_string(),
        )
        .unwrap();
        ctx.config = std::sync::Arc::new(crate::config::Config {
            cache_dir: root.parent().map(std::path::Path::to_path_buf),
            ..(*ctx.config).clone()
        });

        let url = format!("http://127.0.0.1:39235/s2u/video/{key}");
        let found = stream_info_for(&ctx, &url).expect("a buffered video row must resolve");
        assert_eq!(found.title, "A buffered video");

        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }
}

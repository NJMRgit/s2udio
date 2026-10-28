use crate::{
    ctx::Ctx,
    mpd::mpd_client::MpdClient,
    shared::{events::WorkRequest, ytdlp::tagged_stream_entry},
};

use std::collections::HashSet;

use super::{
    PastedItem, YtAction, apply_resolved_streams, expired_stream_original_url,
    fresh_cached_stream_info, mark_stream_parse_pending, s2udio_cache_path, stream_info_for,
    stream_meta_key, stream_parse_pending, youtube_link_of,
};

/// How many queue rows ahead of the current song the look ahead keeps
/// resolved.
///
/// Measured against the reported playlist storm (the user's log, 2026-10-15):
/// one row is always behind, because a `yt-dlp` resolve costs ~2.5 s while a
/// transition costs less, so every row was resolved only *after* MPD had
/// already reached and skipped it (53 warms in 61 s, median 2.46 s apart).
///
/// Four is deliberately modest. The resolve pool has only two workers
/// (`core::work::RESOLVE_WORKERS`), so a deeper window would queue behind the
/// same two workers without becoming ready any sooner — it would only add
/// latency to a user action that needs a resolve of its own (a double-clicked
/// row, a `▶▶` target). Four covers the transitions while two resolves are in
/// flight, which is the gap the log showed.
pub(crate) const LOOK_AHEAD_WINDOW: usize = 4;

/// Minimum spacing between two evaluations of an **unchanged** window (same
/// current song, same MPD next pick, same queue length).
///
/// The look ahead is re-evaluated from every status poll (round 98), and
/// `status_update_interval_ms` may be as low as 16 ms, so before this gate an
/// unchanged window was re-walked and re-stated in the log ~60×/s even when
/// its verdict was "nothing to warm". Measured live (2026-10-20): 30 418
/// look-ahead lines in ~3 minutes, the 8 MiB log rotating every 2-3 minutes.
/// The verdict for an unchanged window cannot change — its rows are still
/// skipped — so re-deciding it repeatedly only costs work and log. A window
/// whose inputs change is evaluated immediately; this interval only throttles
/// repeats of the same decision.
const LOOK_AHEAD_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(1000);

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
        // A metadata warm is not the user's play intent: it must survive any
        // newer play request.
        play_stamp: None,
    });
}

/// Round 95b (user, restored 105e): while a track plays, resolve the queue
/// entries **after** it when they are still unresolved audio links. MPD cannot
/// open a link, so without this it skips past the following entries until the
/// app's resolve lands (round 104 measured the storm: MPD failing 5-15
/// unresolved rows in ~3 s per transition). The look ahead resolves
/// [`LOOK_AHEAD_WINDOW`] rows ahead; with the round-98 in-place swap, MPD's
/// next-song prefetch finds a playable URL and the current track is never cut
/// (playing the replacement was what used to cut it; `ReplaceInPlace` never
/// sends `playid`).
///
/// **Why a window and not one row (2026-10-15).** One row ahead is always
/// behind when a resolve is slower than the transition, which is exactly the
/// reported "resolving storm" after adding a playlist: the playlist's rows
/// arrive in one batch as unresolved links, each resolve costs ~2.5 s, so MPD
/// reaches the warmed row first, fails to decode it, skips, and the next poll
/// warms the row after that — one row per resolve, walking the playlist. The
/// user's log measured it: 53 warms in 61 s, median 2.46 s apart, i.e. the
/// resolve latency itself. Warming a window keeps the pool fed so the row about
/// to play is resolved before MPD asks for it.
///
/// **Which rows come first comes from MPD** (`status.nextsongid`), not from
/// `current + 1`: in `random` mode MPD plays its own shuffled sequence, so the
/// row after the current one is usually not the row that plays next, and
/// warming it left the real next row unresolved (and the round-104 storm in
/// place) for random playback. The index is only the fallback for when MPD
/// reports no `nextsong` (a stopped player, or the end of the queue).
pub fn warm_next_queue_link(ctx: &Ctx) -> bool {
    let Some((idx, current)) = ctx.find_current_song_in_queue() else { return false };
    // 2026-10-20: the look ahead runs from **every** status poll, and
    // `status_update_interval_ms` may be 16 ms — so an unchanged window was
    // re-walked, re-computed and re-stated in the log ~60×/s. Its verdict for
    // such a window cannot change (the rows it skipped are still skipped), so
    // the answer is cached against the inputs that produced it. Measured live
    // before this gate: 30 418 look-ahead lines in ~3 minutes, the 8 MiB log
    // rotating every 2-3 minutes, every other diagnostic line buried under it.
    //
    // The key is (current song, MPD's next pick, queue length) — everything
    // the window is built from. A new current song, a new `nextsongid` or a
    // queue of another length evaluates immediately; the interval only
    // throttles repeats of the *same* window, so a row that becomes warmable
    // without any of those changing is still picked up a second later.
    let key = (current.id, ctx.status.nextsongid, ctx.queue.len());
    {
        let mut state = ctx.look_ahead.borrow_mut();
        if let Some((at, last_key)) = state.last
            && last_key == key
            && at.elapsed() < LOOK_AHEAD_MIN_INTERVAL
        {
            return false;
        }
        state.last = Some((std::time::Instant::now(), key));
    }
    // The remembered rows are **not** cleared when the song changes. A row that
    // has been resolved stays resolved, so forgetting it only re-resolves it;
    // and under `random` MPD advances every ~0.9 s while a resolve takes
    // 5.5-7.6 s, so clearing on each advance threw away work still in flight and
    // started a fresh set — measured live as 20 warms over 20 distinct rows with
    // zero repeats, the ever-growing backlog that is the reported storm.
    //
    // The set is keyed by the row's **stable stream identity**, not its song id:
    // the warm swaps the row in place, which gives it a new id, so an id-keyed
    // set would drop every row it had just resolved and warm it again.
    //
    // It is pruned against the queue by that same identity, so a row the user
    // removed (or one MPD consumed) drops out and the set cannot grow unbounded.
    ctx.warmed_ahead_for.set(Some(current.id));
    {
        let queued: HashSet<String> =
            ctx.queue.iter().map(|song| warm_row_key(ctx, &song.file)).collect();
        ctx.warmed_ahead_ids.borrow_mut().retain(|key| queued.contains(key));
    }
    let window = look_ahead_rows(ctx, idx);
    // Instrumentation kept deliberately: four rounds of this storm were
    // debugged by inferring intent from timing, and every inference was wrong.
    // This states the decision — which row, which key, and why it was or was
    // not warmed — so the log answers it directly. It states it **once per
    // window**, though: repeating the same verdict on every poll is what made
    // the log unreadable, and the window is fully described by its key.
    let log_this_window = {
        let mut state = ctx.look_ahead.borrow_mut();
        if state.logged == Some(key) {
            false
        } else {
            state.logged = Some(key);
            true
        }
    };
    let window_ids: Vec<u32> = window.iter().map(|s| s.id).collect();
    let remembered_now = ctx.warmed_ahead_ids.borrow().len();
    if log_this_window {
        log::debug!(
            current = current.id,
            idx,
            random = ctx.status.random,
            nextsongid:? = ctx.status.nextsongid,
            queue_len = ctx.queue.len(),
            window:? = window_ids,
            remembered = remembered_now;
            "Look ahead: window computed"
        );
    }
    for song in window {
        let key = warm_row_key(ctx, &song.file);
        if ctx.warmed_ahead_ids.borrow().contains(&key) {
            if log_this_window {
                log::debug!(song_id = song.id, key = key.as_str(); "Look ahead: row already warmed; skipping");
            }
            continue;
        }
        // Only a row the warm actually acted on is remembered. Marking the
        // whole window up front would poison the rows `warm_row` deliberately
        // leaves alone (a plain file, a video row), which must stay eligible
        // when a later poll sees them in the window.
        if !warm_row(ctx, &song) {
            if log_this_window {
                log::debug!(song_id = song.id, key = key.as_str(); "Look ahead: row is not warmable; not remembered");
            }
            continue;
        }
        log::debug!(song_id = song.id, key = key.as_str(); "Look ahead: warming and remembering this row");
        ctx.warmed_ahead_ids.borrow_mut().insert(key);
    }
    true
}

/// The identity of a queue row for the warm bookkeeping: the **original link**
/// when the row has been resolved (the `yt_info` cache keys a player/proxy URL
/// back to the link it came from), else the row's own stable stream key.
///
/// The warm swaps a row in place, which gives it a new song id *and* replaces
/// its file with a player URL — so bookkeeping keyed on either the id or the raw
/// file loses the row it just warmed and warms it again on the next poll. The
/// original link is the one string that is the same before and after.
pub(crate) fn warm_row_key(ctx: &Ctx, file: &str) -> String {
    let original = ctx
        .yt_info
        .borrow()
        .get(file)
        .map(|info| info.original_url.clone())
        .filter(|link| !link.is_empty());
    // Both sides go through `stream_meta_key` so the key space is one space: a
    // raw link and its resolved row must produce the *same* key, or the retained
    // set always misses and the row is re-warmed.
    match original {
        Some(link) => stream_meta_key(&link),
        None => stream_meta_key(file),
    }
}

/// The rows the look ahead warms, nearest first: MPD's own next song followed
/// by the queue rows after the current one, at most [`LOOK_AHEAD_WINDOW`] of
/// them.
///
/// MPD's pick is first because it is the row that actually plays next; the
/// `current + 1` run behind it covers the transitions after that, which is what
/// keeps a batch-imported playlist ahead of playback. Non-audio rows (the video
/// list, plain files) are skipped rather than counted, so the window is a
/// window of *resolvable stream rows*, not of arbitrary queue entries.
///
/// **In `random` mode only MPD's pick is warmed (2026-10-15).** MPD keeps its
/// own shuffled sequence, so the rows after the current one in *queue order*
/// are usually not the rows that play next — warming them spends a full
/// `yt-dlp` resolve on rows playback will never reach, and because the window
/// resets per song it re-warms a fresh set of them at every advance. Measured
/// from the live log with random on: **14 warms, only 3 of them rows that ever
/// played** — the rest were `current + 1` neighbours MPD skipped over. MPD
/// states its next pick, and states the one after that when it arrives, so one
/// row is the correct depth when the order is MPD's to decide.
fn look_ahead_rows(ctx: &Ctx, idx: usize) -> Vec<crate::mpd::commands::Song> {
    let mut rows: Vec<crate::mpd::commands::Song> = Vec::with_capacity(LOOK_AHEAD_WINDOW);
    let mpd_next = ctx
        .status
        .nextsongid
        .and_then(|id| ctx.queue.iter().find(|song| song.id == id))
        .cloned();
    let push = |song: &crate::mpd::commands::Song, rows: &mut Vec<crate::mpd::commands::Song>| {
        if rows.len() < LOOK_AHEAD_WINDOW && !rows.iter().any(|r| r.id == song.id) {
            rows.push(song.clone());
        }
    };
    if let Some(song) = &mpd_next {
        push(song, &mut rows);
    }
    // MPD's pick is warmed **first** either way: under shuffle it is the only
    // row the app can be sure about, so it must never be crowded out of the
    // window by guessed ones.
    //
    // The rows after the current one then fill the rest of the window. Under
    // shuffle they are not "the next rows" — but they are still rows MPD will
    // reach: measured live, a 65-row shuffled queue had ~19 distinct rows
    // played (or skipped) in 20 s, so they are queued work, not waste. What
    // made the earlier storm was not warming them; it was *forgetting* them at
    // every track change and starting a fresh set while the previous set's
    // resolves were still in flight.
    for song in ctx.queue.iter().skip(idx + 1) {
        push(song, &mut rows);
    }
    rows
}

/// Warm one queue row — swap it in place for its resolved stream, or re-resolve
/// an expired one. `ReplaceInPlace` never plays: starting the row is the
/// caller's job, and a warm-up that played would cut the current track short.
///
/// Returns whether the row was acted on. A row this leaves alone (a plain file,
/// a video row, one already resolving) reports `false` so the caller does not
/// remember it as warmed — it may become warmable on a later poll.
fn warm_row(ctx: &Ctx, row: &crate::mpd::commands::Song) -> bool {
    let next = row;
    let Some((link, intent)) = tagged_stream_entry(&next.file) else {
        // Round 105: the next row is a **stale resolved stream** — a signed
        // googlevideo URL whose `expire=` epoch passed. MPD would open it,
        // play a few seconds of whatever the CDN still serves, then fail and
        // skip on (nothing re-resolves it: the entry is neither a link nor
        // anything the error recovery recognizes). Warm its re-resolve from
        // the original link exactly like the tagged row, swapped in place.
        let Some(original) = expired_stream_original_url(ctx, &next.file) else {
            return false;
        };
        // Let a re-resolve that is already in flight land first; the next
        // queue update retries (the marker is only set once something is
        // done).
        if stream_parse_pending(ctx, &next.file)
            && fresh_cached_stream_info(ctx, &original).is_none()
        {
            return true;
        }
        if let Some(info) = fresh_cached_stream_info(ctx, &original) {
            apply_resolved_streams(ctx, vec![info], YtAction::ReplaceInPlace(next.id), Vec::new());
            return true;
        }
        log::debug!(url = next.file.as_str(); "Warming the next queue entry's stream (its URL expired)");
        mark_stream_parse_pending(ctx, std::slice::from_ref(&original));
        if let Err(err) = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
            urls: vec![original],
            action: YtAction::ReplaceInPlace(next.id),
            meta_only: false,
            play_stamp: None,
        }) {
            log::error!(error:? = err; "Failed to request the next entry's stream resolution");
        }
        return true;
    };
    if !intent.is_audio() {
        return false;
    }
    // Let a resolve that is already in flight land first; the next queue
    // update retries (the marker is only set once something is done).
    if stream_parse_pending(ctx, &next.file) && fresh_cached_stream_info(ctx, &link).is_none() {
        return true;
    }
    // Round 98: already resolved (the paste popup, an earlier play, a stored
    // playlist entry): swap the row right away, no yt-dlp run.
    if let Some(info) = fresh_cached_stream_info(ctx, &link) {
        apply_resolved_streams(ctx, vec![info], YtAction::ReplaceInPlace(next.id), Vec::new());
        return true;
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
        play_stamp: None,
    }) {
        log::error!(error:? = err; "Failed to request the next entry's stream resolution");
    }
    true
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
    // Round 118: the tag- and buffer-aware lookup. `ctx.yt_info` is keyed by
    // the resolved URL, the canonical link *and* the buffer's player URL, but
    // a queue row can still carry the tagged link (`#s2u-audio`) while the
    // status handler runs against a queue the `ReplaceAndPlay` has not
    // refreshed yet, and a restored buffered row's player URL is only
    // resolvable through the stream-buffer manifest. The raw
    // `get(&song.file)` missed both, so no art request was sent at all —
    // and the branch below then deleted the previous stream's art.
    if let Some(yt) = super::resolve::song_stream_info(ctx, &song.file) {
        let title = yt.title.clone();
        let channel = yt.channel.clone();
        // An empty thumbnail is no art: treating it as one would key the
        // expected source to "" and leave the previous image in place.
        let thumb = yt.thumbnail.clone().filter(|thumb| !thumb.is_empty());
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
        } else {
            // The stream has no thumbnail: drop the previous stream's image
            // instead of leaving it advertised for this track's whole
            // duration.
            let _ = std::fs::remove_file(mpris_art_path(ctx.config.cache_dir.as_deref()));
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


/// Round 95b/98/105e: the look ahead (one row, swapped in place) that keeps
/// MPD's next-song prefetch from meeting an unplayable link.
#[cfg(test)]
mod warm_next_tests {
    #[cfg(test)]
    impl crate::ctx::Ctx {
        /// Whether the row with this song id is remembered as warmed. The set is
        /// keyed by the row's **stable stream identity** (the swap renames the
        /// id), so tests ask by id and this resolves it through the queue.
        fn warmed_row(&self, song_id: u32) -> bool {
            self.queue.iter().any(|song| {
                song.id == song_id
                    && self.warmed_ahead_ids.borrow().contains(&warm_row_key(self, &song.file))
            })
        }
    }

    use super::*;
    use crate::mpd::commands::{Song, State};
    use crate::shared::events::WorkRequest;
    use crate::shared::ytdlp::YtStreamInfo;

    struct Rig {
        ctx: Ctx,
        work_rx: crossbeam::channel::Receiver<WorkRequest>,
        client_rx: crossbeam::channel::Receiver<crate::shared::events::ClientRequest>,
    }

    fn rig(queue: Vec<Song>, current_id: u32) -> Rig {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        // Keep every receiver alive: the code under test sends on all three.
        let work_rx = work.1.clone();
        let client_rx = client.1.clone();
        Box::leak(Box::new(app.1.clone()));
        let mut ctx = crate::tests::fixtures::ctx(
            (app.0, app.1),
            (work.0, work.1.clone()),
            (client.0, client.1.clone()),
        );
        ctx.queue = queue;
        ctx.status.state = State::Play;
        ctx.status.songid = Some(current_id);
        Rig { ctx, work_rx, client_rx }
    }

    fn row(id: u32, file: &str) -> Song {
        Song { id, file: file.to_owned(), ..Default::default() }
    }

    fn resolved(id: u32, url: &str) -> Song {
        row(id, url)
    }

    fn requests(rx: &crossbeam::channel::Receiver<WorkRequest>) -> usize {
        rx.try_iter().count()
    }

    #[test]
    fn the_next_tagged_link_is_warmed_once_per_pair() {
        let link = "https://www.youtube.com/watch?v=JJJ#s2u-audio";
        let rig = rig(vec![resolved(1, "https://rr1.example/vb"), row(2, link)], 1);
        assert!(rig.work_rx.is_empty());

        warm_next_queue_link(&rig.ctx);
        assert_eq!(rig.ctx.warmed_ahead_for.get(), Some(1), "the window belongs to the current song");
        assert!(
            rig.ctx.warmed_row(2),
            "the warmed row is remembered"
        );
        assert_eq!(requests(&rig.work_rx), 1, "one resolve was asked for");
        assert!(
            stream_parse_pending(&rig.ctx, link),
            "the row spins until the resolve lands"
        );

        // A second call for the same pair must not ask again (the swap's own
        // queue refresh triggers it once more).
        warm_next_queue_link(&rig.ctx);
        assert_eq!(requests(&rig.work_rx), 0, "no second resolve for the same pair");
    }

    #[test]
    fn a_fresh_cached_resolve_is_swapped_in_place_without_ytdlp() {
        let link = "https://www.youtube.com/watch?v=KKK";
        let tagged = format!("{link}#s2u-audio");
        let rig = rig(vec![resolved(1, "https://rr1.example/vb"), row(2, &tagged)], 1);
        rig.ctx.yt_info.borrow_mut().insert(
            tagged.clone(),
            YtStreamInfo {
                url: "https://rr9.example/vb?id=KKK".to_owned(),
                original_url: link.to_owned(),
                title: "Cached".to_owned(),
                resolved_at: Some(crate::ui::modals::paste::now_epoch_secs()),
                ..Default::default()
            },
        );

        warm_next_queue_link(&rig.ctx);
        assert!(rig.ctx.warmed_row(2));
        assert_eq!(requests(&rig.work_rx), 0, "a cached resolve runs no yt-dlp");
        assert_eq!(
            rig.client_rx.try_iter().count(),
            1,
            "the row is replaced in place (an MPD command was queued)"
        );
    }

    #[test]
    fn a_non_link_next_entry_is_left_alone() {
        let rig = rig(
            vec![resolved(1, "https://rr1.example/vb"), row(2, "local.mp3")],
            1,
        );
        warm_next_queue_link(&rig.ctx);
        assert!(rig.ctx.warmed_ahead_ids.borrow().is_empty());
        assert_eq!(requests(&rig.work_rx), 0);
    }

    #[test]
    fn a_video_tagged_row_is_not_warmed_by_the_audio_look_ahead() {
        let rig = rig(
            vec![
                resolved(1, "https://rr1.example/vb"),
                row(2, "https://www.youtube.com/watch?v=LLL#s2u-video"),
            ],
            1,
        );
        warm_next_queue_link(&rig.ctx);
        assert!(rig.ctx.warmed_ahead_ids.borrow().is_empty());
        assert_eq!(requests(&rig.work_rx), 0);
    }

    #[test]
    fn nothing_happens_without_a_current_song() {
        let mut rig = rig(vec![row(2, "https://www.youtube.com/watch?v=MMM#s2u-audio")], 1);
        rig.ctx.status.state = State::Stop;
        warm_next_queue_link(&rig.ctx);
        assert_eq!(rig.ctx.warmed_ahead_for.get(), None);
        assert_eq!(requests(&rig.work_rx), 0);
    }

    /// Round 105: a next row that is a *stale resolved stream* (its signed
    /// URL expired) is warmed from its original link with a ReplaceInPlace —
    /// without this, MPD would play a few seconds of the dead URL and skip.
    #[test]
    fn a_stale_resolved_next_row_is_warmed_in_place() {
        const EXPIRED: &str =
            "https://rr2.googlevideo.com/videoplayback?expire=1000000000&id=zz";
        const WATCH: &str = "https://www.youtube.com/watch?v=QQQ";
        let rig = rig(
            vec![resolved(1, "https://rr1.example/vb"), row(2, EXPIRED)],
            1,
        );
        rig.ctx.yt_info.borrow_mut().insert(
            EXPIRED.to_owned(),
            YtStreamInfo {
                url: EXPIRED.to_owned(),
                original_url: WATCH.to_owned(),
                title: "Stale".to_owned(),
                resolved_at: Some(crate::ui::modals::paste::now_epoch_secs()),
                ..Default::default()
            },
        );

        warm_next_queue_link(&rig.ctx);

        assert!(rig.ctx.warmed_row(2));
        assert!(
            rig.work_rx.try_iter().any(|request| matches!(
                request,
                WorkRequest::ResolveYtStreams { urls, action, .. }
                    if urls == vec![WATCH.to_owned()]
                        && matches!(action, YtAction::ReplaceInPlace(found) if found == 2)
            )),
            "the original link is re-resolved in place"
        );
    }

    /// A **restored buffered row** — a previous run's `/s2u/audio/<key>` URL
    /// whose key the stream-buffer manifest no longer holds — used to be
    /// "not warmable": the look ahead could not name the link, left the row
    /// alone, and MPD then 404'd on it ("a stream the app did not start") and
    /// stopped. The resolve cache is keyed by that same buffered URL
    /// (round 107), so the row can be re-resolved in place ahead of playback.
    #[test]
    fn a_restored_buffered_next_row_is_warmed_from_the_resolve_cache() {
        const LINK: &str = "https://www.youtube.com/watch?v=xzNjLuoe-nw";
        const BUFFERED: &str = "http://127.0.0.1:40393/s2u/audio/17fe2899f41933ca";
        let rig = rig(
            vec![resolved(1, "https://rr1.example/vb"), row(2, BUFFERED)],
            1,
        );
        rig.ctx.yt_info.borrow_mut().insert(
            BUFFERED.to_owned(),
            YtStreamInfo {
                url: "https://rr1.example/vb".to_owned(),
                original_url: LINK.to_owned(),
                title: "8 Graves - Burning Alive (lyrics)".to_owned(),
                ..Default::default()
            },
        );

        warm_next_queue_link(&rig.ctx);

        assert!(rig.ctx.warmed_row(2), "the restored row is remembered as warmed");
        assert!(
            rig.work_rx.try_iter().any(|request| matches!(
                request,
                WorkRequest::ResolveYtStreams { urls, action, .. }
                    if urls == vec![LINK.to_owned()]
                        && matches!(action, YtAction::ReplaceInPlace(found) if found == 2)
            )),
            "the restored buffered row is re-resolved in place from its link"
        );
    }

    /// **The pruning must not eat the rows the swap just resolved.**
    ///
    /// `warm_row` swaps a row in place (`delete_id` + `add_id`), which gives it
    /// a **new id**. The remembered id is therefore gone from the queue by the
    /// very next poll, so a naive `retain(|id| queue.contains(id))` prunes
    /// exactly the rows that were successfully warmed — the set is emptied as
    /// fast as it is filled and the row is warmed again, forever.
    ///
    /// This is why the retained set showed no repeats live: 10 warms over 10
    /// distinct rows, though only 2 tracks advanced.
    #[test]
    fn pruning_keeps_a_row_whose_swap_changed_its_id() {
        let link = "https://www.youtube.com/watch?v=JJJ";
        let tagged = format!("{link}#s2u-audio");
        let rig = rig(vec![resolved(1, "https://rr1.example/vb"), row(2, &tagged)], 1);
        let mut ctx = rig.ctx;

        warm_next_queue_link(&ctx);
        assert!(
            ctx.warmed_row(2),
            "row 2 is remembered after the warm"
        );

        // The swap landed: the row is now the player URL under a NEW id, and the
        // yt_info cache (as `apply_resolved_streams` populates it) maps that
        // URL back to the link it came from — which is what makes the row
        // recognisable again.
        let player = "http://127.0.0.1:39235/s2u/audio/abc123".to_owned();
        ctx.yt_info.borrow_mut().insert(
            player.clone(),
            YtStreamInfo {
                url: player.clone(),
                original_url: link.to_owned(),
                title: "A stream".to_owned(),
                resolved_at: Some(crate::ui::modals::paste::now_epoch_secs()),
                ..Default::default()
            },
        );
        ctx.queue = vec![resolved(1, "https://rr1.example/vb"), resolved(99, &player)];
        warm_next_queue_link(&ctx);

        // The row now has id 99 (the swap renamed it). What must survive is its
        // *identity*, so it is not warmed a second time.
        assert!(
            ctx.warmed_row(99),
            "the row must stay remembered under its new id — pruning by id alone \
             forgets every successful warm. remembered: {:?}",
            ctx.warmed_ahead_ids.borrow()
        );
        assert!(
            !ctx.warmed_row(2),
            "the pre-swap id is genuinely gone; the identity is what carries over"
        );
    }

    /// **Under shuffle MPD's own pick leads the window.**
    ///
    /// The app cannot compute MPD's shuffled sequence, so the only row it can be
    /// certain about is `nextsongid`. That row must be warmed **first**, and it
    /// must never be crowded out of the window by the guessed rows that fill the
    /// rest — a wrong guess costs a resolve, but being wrong about the row that
    /// actually plays costs the transition.
    ///
    /// (An earlier reading of the storm concluded the neighbours should not be
    /// warmed at all; the live log disproved it — a 65-row shuffled queue had
    /// ~19 distinct rows played in 20 s, so they are queued work. The churn came
    /// from discarding the warmed set on every track change.)
    #[test]
    fn random_mode_warms_mpds_pick_first() {
        let link = |n: u32| format!("https://www.youtube.com/watch?v=vid{n}#s2u-audio");
        let rig = rig(
            vec![
                resolved(1, "https://rr1.example/vb"),
                row(2, &link(2)),
                row(3, &link(3)),
                row(4, &link(4)),
                row(5, &link(5)),
            ],
            1,
        );
        let mut ctx = rig.ctx;
        ctx.status.random = true;
        ctx.status.nextsongid = Some(4);

        warm_next_queue_link(&ctx);

        let asked: Vec<YtAction> = rig
            .work_rx
            .try_iter()
            .filter_map(|r| match r {
                WorkRequest::ResolveYtStreams { action, .. } => Some(action),
                _ => None,
            })
            .collect();
        assert_eq!(
            asked.first(),
            Some(&YtAction::ReplaceInPlace(4)),
            "MPD's shuffled pick is warmed first — asked: {asked:?}"
        );
    }

    /// The window is **bounded**: a long queue of unresolved links warms at
    /// most [`LOOK_AHEAD_WINDOW`] rows, never the whole playlist. Warming
    /// everything would queue hundreds of `yt-dlp` calls behind the two
    /// resolve workers and delay any row the user actually asks for.
    #[test]
    fn the_window_is_bounded_on_a_long_queue() {
        let mut rows = vec![resolved(1, "https://rr1.example/vb")];
        for id in 2..=40 {
            rows.push(row(id, &format!("https://www.youtube.com/watch?v=vid{id}#s2u-audio")));
        }
        let rig = rig(rows, 1);

        warm_next_queue_link(&rig.ctx);

        let warmed = requests(&rig.work_rx);
        assert!(
            warmed >= 2,
            "more than the single row the old look ahead warmed — got {warmed}"
        );
        assert!(
            warmed < 39,
            "the window is bounded well below the queue length, so a long \
             playlist does not queue hundreds of resolves — got {warmed}"
        );
        assert_eq!(
            rig.ctx.warmed_ahead_ids.borrow().len(),
            warmed,
            "every warmed row is remembered, and nothing else is"
        );
    }

    /// The window **tracks** the current song without discarding what it has
    /// already resolved: moving on keeps the remembered rows and adds the new
    /// song's next pick. Clearing them was the churn — a resolved row stays
    /// resolved, and re-asking for it costs another `yt-dlp` run (measured
    /// live under shuffle: 20 warms over 20 distinct rows, zero repeats).
    #[test]
    fn the_window_follows_the_song_without_discarding_warmed_rows() {
        let link = |n: u32| format!("https://www.youtube.com/watch?v=vid{n}#s2u-audio");
        let mut rig = rig(
            vec![
                resolved(1, "https://rr1.example/vb"),
                row(2, &link(2)),
                row(3, &link(3)),
                row(4, &link(4)),
            ],
            1,
        );

        warm_next_queue_link(&rig.ctx);
        assert_eq!(rig.ctx.warmed_ahead_for.get(), Some(1));
        let first: std::collections::BTreeSet<String> =
            rig.ctx.warmed_ahead_ids.borrow().iter().cloned().collect();
        assert!(!first.is_empty(), "the first window is warmed: {first:?}");

        // Playback moves on. The window belongs to the new song now, and what
        // was already warmed stays remembered (those resolves are in flight).
        rig.ctx.status.songid = Some(2);
        warm_next_queue_link(&rig.ctx);

        assert_eq!(rig.ctx.warmed_ahead_for.get(), Some(2), "the window follows the song");
        let now: std::collections::BTreeSet<String> =
            rig.ctx.warmed_ahead_ids.borrow().iter().cloned().collect();
        assert!(
            now.is_superset(&first),
            "previously warmed rows are kept, not discarded: before={first:?} after={now:?}"
        );
        // Row 2 was warmed while it was still *ahead* of song 1, so it stays
        // remembered now that it plays — being current does not un-warm a row
        // whose resolve already happened.
        assert!(now.contains(&stream_meta_key(&link(2))), "a row stays remembered once warmed: {now:?}");
    }

    /// **The warmed set must not be discarded when the song changes.**
    ///
    /// The window was cleared on every current-song change. Under `random` MPD
    /// advances quickly, so each advance threw away the rows already being
    /// resolved and started a completely fresh set — while the previous set's
    /// resolves were still finishing seconds later. Measured live with shuffle
    /// on: MPD advancing every ~0.9 s, a resolve taking 5.5-7.6 s, and **20 warm
    /// events covering 20 distinct rows with zero repeats** — one fresh resolve
    /// per skip and an ever-growing backlog. That is the storm.
    ///
    /// A resolved row stays resolved, so remembering it is correct as well as
    /// cheaper; MPD's next pick is re-read every poll either way.
    #[test]
    fn the_warmed_set_is_not_discarded_when_the_song_changes() {
        let link = |n: u32| format!("https://www.youtube.com/watch?v=vid{n}#s2u-audio");
        let mut rig = rig(
            vec![
                resolved(1, "https://rr1.example/vb"),
                row(2, &link(2)),
                row(3, &link(3)),
                row(4, &link(4)),
            ],
            1,
        );
        rig.ctx.status.nextsongid = Some(3);

        warm_next_queue_link(&rig.ctx);
        assert!(
            rig.ctx.warmed_row(3),
            "row 3 is warmed first"
        );

        // MPD moves on while row 3's resolve is still in flight. Row 3 must stay
        // remembered: it is being resolved, and re-warming it (or losing track
        // of it) is what produced one fresh resolve per skip.
        rig.ctx.status.songid = Some(2);
        rig.ctx.status.nextsongid = Some(4);
        warm_next_queue_link(&rig.ctx);

        assert!(
            rig.ctx.warmed_row(3),
            "row 3 must stay remembered across the song change — losing it \
             re-resolves a fresh set at every skip. remembered: {:?}",
            rig.ctx.warmed_ahead_ids.borrow()
        );
        assert_eq!(
            rig.ctx.warmed_ahead_for.get(),
            Some(2),
            "the window tracks the new current song"
        );
        assert!(
            rig.ctx.warmed_row(4),
            "and MPD's fresh pick is warmed alongside the remembered rows"
        );
    }

    /// A row the warm **cannot** act on yet is not remembered as warmed, so a
    /// later poll can still warm it.
    ///
    /// This is the difference between the window and a plain "mark everything
    /// I looked at" loop: a row that is already resolving reports as handled,
    /// but a row that is *not* warmable (here a non-audio tagged row) must stay
    /// eligible — otherwise it would be poisoned for the rest of the song and
    /// never resolve when it becomes the one about to play.
    #[test]
    fn a_row_the_warm_cannot_act_on_stays_eligible() {
        let rig = rig(
            vec![
                resolved(1, "https://rr1.example/vb"),
                row(2, "https://www.youtube.com/watch?v=LLL#s2u-video"),
                row(3, "https://www.youtube.com/watch?v=MMM#s2u-audio"),
            ],
            1,
        );

        warm_next_queue_link(&rig.ctx);

        assert!(
            !rig.ctx.warmed_row(2),
            "the video row is not remembered — it is not this look ahead's to warm"
        );
        assert!(
            rig.ctx.warmed_row(3),
            "the audio row behind it is still warmed, and remembered"
        );
    }

    /// A window that contains rows the warm cannot act on still reaches the
    /// audio rows **behind** them: the window is a budget of candidate rows,
    /// and skipping a non-audio row does not consume it.
    #[test]
    fn a_window_with_skippable_rows_still_reaches_the_audio_behind_them() {
        let rig = rig(
            vec![
                resolved(1, "https://rr1.example/vb"),
                row(2, "local-a.mp3"),
                row(3, "local-b.mp3"),
                row(4, "https://www.youtube.com/watch?v=AAA#s2u-audio"),
                row(5, "https://www.youtube.com/watch?v=BBB#s2u-audio"),
            ],
            1,
        );

        warm_next_queue_link(&rig.ctx);

        assert_eq!(
            requests(&rig.work_rx),
            2,
            "both audio rows are warmed even though the window starts with two \
             plain files"
        );
    }

    /// Round 105: a *live* resolved next row (not expired) is left alone — it
    /// plays as-is.
    #[test]
    fn a_live_resolved_next_row_is_left_alone() {
        const FRESH: &str =
            "https://rr2.googlevideo.com/videoplayback?expire=7258118400&id=qq";
        let rig = rig(
            vec![resolved(1, "https://rr1.example/vb"), row(2, FRESH)],
            1,
        );
        warm_next_queue_link(&rig.ctx);
        assert!(rig.ctx.warmed_ahead_ids.borrow().is_empty());
        assert_eq!(requests(&rig.work_rx), 0);
    }
}

/// 2026-10-28: web-stream MPRIS art. The request must actually be sent for a
/// row whose `file` is not yet the key the resolve is stored under, and a
/// stream without a thumbnail must not keep the previous track's image.
#[cfg(test)]
mod mpris_metadata_tests {
    use super::*;
    use crate::mpd::commands::{Song, State};
    use crate::shared::events::WorkRequest;
    use crate::shared::ytdlp::YtStreamInfo;

    static RIG_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    struct Rig {
        ctx: Ctx,
        work_rx: crossbeam::channel::Receiver<WorkRequest>,
    }

    fn rig(queue: Vec<Song>, current_id: u32) -> Rig {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        let work_rx = work.1.clone();
        // Keep every receiver alive: the code under test sends on all three.
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(client.1.clone()));
        let mut ctx = crate::tests::fixtures::ctx(
            (app.0, app.1),
            (work.0, work.1.clone()),
            (client.0, client.1.clone()),
        );
        // A per-rig cache dir: these tests create and delete `mpris-art`, and
        // the shared fixture dir would let a parallel test remove the file
        // another one is asserting on.
        let mut config = (*ctx.config).clone();
        config.cache_dir = Some(std::env::temp_dir().join(format!(
            "s2udio-test-mpris-art-{}-{}",
            std::process::id(),
            RIG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        )));
        ctx.config = std::sync::Arc::new(config);
        ctx.queue = queue;
        ctx.status.state = State::Play;
        ctx.status.songid = Some(current_id);
        Rig { ctx, work_rx }
    }

    fn row(id: u32, file: &str) -> Song {
        Song { id, file: file.to_owned(), ..Default::default() }
    }

    fn requested_art(rig: &Rig) -> Option<String> {
        rig.work_rx.try_iter().find_map(|request| match request {
            WorkRequest::SaveMprisArt { url, .. } => Some(url),
            _ => None,
        })
    }

    const LINK: &str = "https://www.youtube.com/watch?v=xzNjLuoe-nw";
    const THUMB: &str = "https://i.ytimg.com/vi_webp/xzNjLuoe-nw/maxresdefault.webp";

    fn stream_info(thumbnail: Option<&str>) -> YtStreamInfo {
        YtStreamInfo {
            url: "https://rr1---sn.example/videoplayback".to_owned(),
            original_url: LINK.to_owned(),
            title: "8 Graves - Burning Alive (lyrics)".to_owned(),
            channel: Some("MrSuicideSheep".to_owned()),
            thumbnail: thumbnail.map(str::to_owned),
            duration: Some(203.0),
            ..Default::default()
        }
    }

    /// The status handler runs against the queue the `ReplaceAndPlay` has not
    /// refreshed yet, so the row still carries the tagged link. The resolve is
    /// keyed by the canonical link; the raw map lookup missed it and no art
    /// request was ever sent for the track (`ensure_mpris_metadata` even took
    /// the "no stream art" branch and deleted the previous art).
    #[test]
    fn a_row_still_carrying_its_tagged_link_still_gets_its_art() {
        let tagged = format!("{LINK}#s2u-audio");
        let rig = rig(vec![row(7, &tagged)], 7);
        rig.ctx.yt_info.borrow_mut().insert(LINK.to_owned(), stream_info(Some(THUMB)));

        ensure_mpris_metadata(&rig.ctx);

        assert_eq!(
            requested_art(&rig).as_deref(),
            Some(THUMB),
            "the tagged row's resolve is found through its canonical link"
        );
    }

    /// A stream whose resolve carries no thumbnail: the previous track's image
    /// must not stay in the media controls for this one's whole duration.
    #[test]
    fn a_stream_without_a_thumbnail_drops_the_previous_art() {
        let rig = rig(vec![row(7, "https://rr1---sn.example/videoplayback")], 7);
        let art = mpris_art_path(rig.ctx.config.cache_dir.as_deref());
        if let Some(parent) = art.parent() {
            std::fs::create_dir_all(parent).expect("test cache dir");
        }
        std::fs::write(&art, b"previous-stream").expect("previous art on disk");
        rig.ctx.yt_info.borrow_mut().insert(
            "https://rr1---sn.example/videoplayback".to_owned(),
            stream_info(None),
        );

        ensure_mpris_metadata(&rig.ctx);

        assert!(!art.exists(), "the previous stream's image must be removed");
        assert_eq!(requested_art(&rig), None, "there is no thumbnail to fetch");
    }
}

use anyhow::Result;
use crate::{
    config::tabs::{PaneType, TreeBrowserArgs},
    ctx::Ctx,
    mpd::{QueuePosition, mpd_client::MpdClient},
    shared::{
        events::WorkRequest,
        macros::{status_error, status_info, status_warn},
        mpd_client_ext::{Enqueue, MpdClientExt as _},
        ytdlp::{StreamIntent, tagged_stream_entry, tagged_stream_link},
    },
};

use super::{
    PASTE_PLAY, PastedItem, YtAction, music_directory, play_torrent,
    play_tagged_queue_entry, s2udio_cache_path,
};

/// Convert an absolute local path to the MPD-relative path when it lives
/// under the music directory (MPD refuses absolute local paths over TCP).
/// Returns the path unchanged when it cannot be relativized.
pub(super) fn mpd_addable_path(path: &str) -> String {
    let Some(music_dir) = music_directory() else {
        return path.to_owned();
    };
    let Ok(abs) = std::path::absolute(path) else { return path.to_owned() };
    let abs = abs.to_string_lossy();
    let Some(rel) = abs.strip_prefix(&music_dir) else {
        return path.to_owned();
    };
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() { path.to_owned() } else { rel.to_owned() }
}
/// Queue id of a temporary play entry (set by the Radio pane's result
/// handler, exposed via Ctx so the Queue pane hides the row).
/// The mpv playlist entries for the given video items: local files keep
/// their path, direct URLs and YouTube-style links their URL. Torrents are
/// routed exclusively through the `[Torrent]` popup section, so they never
/// reach this builder.
fn video_entries_for(vids: &[PastedItem]) -> Vec<crate::core::mpv::MpvPlaylistEntry> {
    use crate::core::mpv::MpvPlaylistEntry;
    vids.iter()
        .map(|item| match item {
            PastedItem::File(path) | PastedItem::VideoFile(path) => {
                MpvPlaylistEntry::new(
                    path.rsplit('/').next().unwrap_or(path).to_owned(),
                    path.clone(),
                    None,
                )
            }
            PastedItem::Url(url) | PastedItem::VideoUrl(url) | PastedItem::Yt(url) => {
                MpvPlaylistEntry::new(url.clone(), url.clone(), None)
            }
            PastedItem::Torrent(_) | PastedItem::Magnet(_) => {
                unreachable!(
                    "torrents are streamed through the [Torrent] section, never the video list"
                )
            }
        })
        .collect()
}
/// The YouTube-style links among the video items.
fn yt_urls(vids: &[PastedItem]) -> Vec<String> {
    vids.iter()
        .filter_map(|item| match item {
            PastedItem::Yt(url) => Some(url.clone()),
            _ => None,
        })
        .collect()
}
/// Every item is a YouTube-style link (resolved via yt-dlp first).
fn all_yt(vids: &[PastedItem]) -> bool {
    !vids.is_empty() && yt_urls(vids).len() == vids.len()
}
/// "Play (don't add to queue)": resolve YouTube-style links, then play via
/// mpv without touching the persistent video playlist.
pub(super) fn play_video_now(ctx: &Ctx, vids: &[PastedItem]) {
    if all_yt(vids) {
        // Nothing is on screen while the link resolves (play-now adds no
        // row), so the status line says what the wait is for.
        status_info!("Resolving the video link… mpv starts when it lands");
        let _ = ctx
            .work_sender
            .send(WorkRequest::ResolveYtStreams {
                urls: yt_urls(vids),
                action: YtAction::PlayVideo,
                meta_only: false,
                // Play-now: a newer play request supersedes this resolve.
                play_stamp: Some(crate::shared::ytdlp::play_request::next()),
            });
        return;
    }
    crate::core::mpv::play_video_entries(ctx, video_entries_for(vids));
}
/// "Add / Append to queue": insert the video items into the persistent
/// video playlist (YouTube-style links after resolving them). With `play`
/// the added videos start playing immediately. A set may mix both kinds
/// (round 89) — see the comment inside.
pub(super) fn queue_videos(ctx: &Ctx, vids: &[PastedItem], after_current: bool, play: bool) {
    use crate::core::mpv::MpvPlaylistEntry;
    // Round 89: one video set can mix local videos and YouTube-style links
    // (the paste popup's `Add to queue [> Video]` on a paste that holds
    // both). 2026-10-26 (user): the rows land **immediately**, like the audio
    // queue's: local files as they are, links as resolving placeholders the
    // resolve fills in place. Waiting for `yt-dlp` before adding anything
    // left the video queue empty for seconds after a paste.
    let links = yt_urls(vids);
    let local: Vec<PastedItem> = vids
        .iter()
        .filter(|item| !matches!(item, PastedItem::Yt(_)))
        .cloned()
        .collect();
    let mut entries = video_entries_for(&local);
    entries.extend(links.iter().map(|link| MpvPlaylistEntry::pending_link(link)));
    if entries.is_empty() {
        return;
    }
    crate::core::mpv::add_to_video_playlist(ctx, entries.clone(), after_current);
    if links.is_empty() {
        // Nothing to resolve: playback (when asked for) starts right away.
        if play {
            crate::core::mpv::play_video_entries(ctx, entries);
        }
        return;
    }
    let action = if play {
        YtAction::AddToVideoQueueAndPlay
    } else if after_current {
        YtAction::AddToVideoQueue
    } else {
        YtAction::AppendVideoQueue
    };
    let _ = ctx
        .work_sender
        .send(WorkRequest::ResolveYtStreams {
            urls: links,
            action,
            meta_only: false,
            play_stamp: play.then(crate::shared::ytdlp::play_request::next),
        });
}
/// The stored-playlist URIs of the given video items: local files keep
/// their path, direct URLs and YouTube-style links their (stable) link —
/// matching what the video queue's *Create video playlist* stores, so the
/// playlists tab can show the cached titles.
pub(super) fn video_playlist_uris(vids: &[PastedItem]) -> Vec<String> {
    vids.iter()
        .map(|item| match item {
            PastedItem::File(path) | PastedItem::VideoFile(path) => path.clone(),
            PastedItem::Url(url) | PastedItem::VideoUrl(url) | PastedItem::Yt(url) => {
                url.clone()
            }
            PastedItem::Torrent(_) | PastedItem::Magnet(_) => {
                unreachable!(
                    "torrents are streamed through the [Torrent] section, never stored in playlists"
                )
            }
        })
        .collect()
}
/// Play a single item immediately without adding it to the queue.
pub(super) fn play_item(ctx: &Ctx, item: &PastedItem) -> Result<()> {
    match item {
        PastedItem::File(path) => {
            let uri = mpd_addable_path(path);
            if uri != *path {
                status_info!("Playing {} (MPD path: {uri})", item.label());
            }
            paste_play_temp(ctx, uri, item.label());
            Ok(())
        }
        PastedItem::Url(url) => {
            paste_play_temp(ctx, url.clone(), failure_label(item));
            Ok(())
        }
        PastedItem::VideoFile(path) => {
            let uri = mpd_addable_path(path);
            paste_play_temp(ctx, uri, item.label());
            Ok(())
        }
        PastedItem::VideoUrl(url) => {
            paste_play_temp(ctx, url.clone(), failure_label(item));
            Ok(())
        }
        PastedItem::Yt(url) => yt_play_audio(ctx, url),
        PastedItem::Torrent(_) | PastedItem::Magnet(_) => {
            play_torrent(ctx, std::slice::from_ref(item))
        }
    }
}
/// What a refused play calls the item in the status line: a file keeps its
/// name, a URL is just "the stream" (a raw URL there is noise).
fn failure_label(item: &PastedItem) -> String {
    match item {
        PastedItem::File(_) | PastedItem::VideoFile(_) => item.label(),
        _ => "the stream".to_owned(),
    }
}
/// Play a pasted item's audio through MPD as a temporary entry.
///
/// One transactional `addid` + `playid` (`crate::shared::temp_play`): a file
/// or stream MPD cannot open leaves no hidden queue row behind, and the failure
/// carries the item's label.
fn paste_play_temp(ctx: &Ctx, url: String, label: String) {
    ctx.query()
        .id(PASTE_PLAY)
        .replace_id(PASTE_PLAY)
        .target(PaneType::Radio {
            tree: TreeBrowserArgs::default(),
        })
        .query(move |client| {
            let id = crate::shared::temp_play::add_and_play(client, &url, &label)?;
            Ok(crate::MpdQueryResult::Any(Box::new(id)))
        });
}
/// Resolve a YouTube-style link to its direct stream and play it through MPD
/// as a temporary entry.
fn yt_play_audio(ctx: &Ctx, url: &str) -> Result<()> {
    let _ = ctx
        .work_sender
        .send(WorkRequest::ResolveYtStreams {
            urls: vec![url.to_owned()],
            action: YtAction::Play,
            meta_only: false,
            // Play-now: a newer play request supersedes this resolve.
            play_stamp: Some(crate::shared::ytdlp::play_request::next()),
        })
        .map_err(|err| anyhow::anyhow!("Failed to request stream resolution: {err}"))?;
    status_info!("Resolving YouTube link…");
    Ok(())
}
/// Add all items to the queue in one MPD enqueue call.
///
/// Round 91: a pasted YouTube-style link is queued **verbatim** as its
/// tagged audio link (`watch?v=ID#s2u-audio`) - the shape a stored playlist
/// entry already has - so every row shows up immediately and nothing waits
/// for yt-dlp. A link that sat unresolved for seconds looked like a hang and
/// users pasted it again, which queued it twice. The link is resolved when
/// its entry is played: `play_queue_song` for the Queue tab's Enter and the
/// event loop for an entry MPD started on its own (both call
/// [`resolve_tagged_queue_entry`]).
///
/// With `play` the first queued item starts playing. When that item is such
/// a link MPD cannot open it, so its autoplay is skipped and the entry is
/// resolved + replaced as soon as the enqueue has landed
/// ([`YtAction::ReplaceAndPlay`]): the rows appear while yt-dlp runs and
/// playback starts once the stream is there.
pub(super) fn enqueue_items(
    ctx: &Ctx,
    items: &[PastedItem],
    after_current: bool,
    play: bool,
) -> Result<()> {
    let has_current = ctx.find_current_song_in_queue().is_some();
    let position = (after_current && has_current)
        .then_some(QueuePosition::RelativeAdd(0));
    // Round 91: every item keeps its pasted order, and a web stream joins
    // the same list as its tagged audio link.
    let mut uris: Vec<String> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    for item in items {
        match item {
            PastedItem::File(path) | PastedItem::VideoFile(path) => {
                uris.push(mpd_addable_path(path));
            }
            PastedItem::Url(url) | PastedItem::VideoUrl(url) => uris.push(url.clone()),
            PastedItem::Yt(url) => {
                let tagged = tagged_stream_link(url, StreamIntent::Audio);
                links.push(tagged.clone());
                uris.push(tagged);
            }
            PastedItem::Torrent(_) | PastedItem::Magnet(_) => {}
        }
    }
    if uris.is_empty() {
        return Ok(());
    }
    let count = uris.len();
    let has_links = !links.is_empty();
    // The index the first inserted item lands on: `play` starts it (after
    // the current entry with `after_current`, else at the end of the queue)
    // and the link lookup below reads it back.
    let insert_idx = play.then(|| {
        ctx.find_current_song_in_queue()
            .map(|(idx, _)| idx + 1)
            .unwrap_or_else(|| ctx.queue.len())
    });
    // MPD would fail on a tagged link, so its autoplay is left to the
    // resolve below when that link is the first queued item (Round 91).
    let first_is_link = uris.first().is_some_and(|uri| links.contains(uri));
    let autoplay_idx = if first_is_link { None } else { insert_idx };
    // Round 91: only the paste row's "play" arm starts playback. Choosing
    // "Add to queue" must leave the raw link in the queue (it resolves when
    // the entry is played) - resolving here played a row the user only
    // wanted queued (validated live: status "Resolving the stream...", the
    // entry replaced by a googlevideo URL and MPD playing it).
    let first_link = (play && first_is_link).then(|| links[0].clone());
    let enqueue: Vec<Enqueue> = uris
        .iter()
        .cloned()
        .map(|path| Enqueue::File { path })
        .collect();
    ctx.command(move |client| {
        client.enqueue_multiple(enqueue, autoplay_idx, position, false)?;
        // The rows are in the queue now (the generic "Added N item(s)" line
        // is already written): say what happens to the links, so the rows
        // are not mistaken for a stuck paste (Round 91).
        if has_links && !(play && first_is_link) {
            status_info!(
                "{count} item(s) queued - the stream link(s) resolve when played"
            );
        }
        Ok(())
    });
    if let Some(tagged) = first_link {
        // Round 91: the row exists now - find the id it was given (the query
        // is queued behind the enqueue command) and resolve it, so the paste
        // row's "play" arm starts playback without waiting for yt-dlp.
        let lookup = tagged.clone();
        let inserted = ctx.query_sync(move |client| {
            let songs = client.playlist_info()?.unwrap_or_default();
            let song = insert_idx
                .and_then(|idx| songs.get(idx))
                .filter(|song| song.file == lookup)
                .or_else(|| songs.iter().find(|song| song.file == lookup));
            Ok(song.map(|song| song.id))
        });
        if let Ok(Some(song_id)) = inserted {
            // The paste's "play" arm is a user play request (2026-10-25).
            play_tagged_queue_entry(ctx, &tagged, song_id);
        } else {
            status_warn!("Cannot find the queued stream entry to resolve");
        }
    }
    Ok(())
}
/// Round 95: put stream **links** into the MPD queue in one command list.
/// Every row is there the moment the command lands; nothing is resolved
/// here — a link is resolved when its entry is played (rounds 91 / 91b),
/// which is why the caller is told how many rows it just got.
///
/// `play` starts the first link entry: MPD cannot open a link, so the
/// autoplay is left to the resolve below, which replaces the row in place
/// and starts it ([`YtAction::ReplaceAndPlay`], the single-link shape).
/// `replace` empties the queue first (the stored playlists' *Replace
/// queue*), `play` then pins the resolved row to the top.
pub(super) fn enqueue_stream_links(
    ctx: &Ctx,
    uris: Vec<String>,
    play: bool,
    replace: bool,
) -> Result<()> {
    if uris.is_empty() {
        return Ok(());
    }
    let has_current = ctx.find_current_song_in_queue().is_some();
    let position =
        (play && !replace && has_current).then_some(QueuePosition::RelativeAdd(0));
    // Where the first added row will sit once the enqueue lands: right after the
    // current song when the rows are inserted there, else at the end. The queue
    // pane moves the highlight there when the refresh arrives (2026-10-15) — a
    // playlist import otherwise left the selection on whatever old index it
    // held, which reads as "it landed in the middle of the new tracks".
    let first_added_idx = if position.is_some() {
        ctx.find_current_song_in_queue().map_or(0, |(idx, _)| idx + 1)
    } else {
        ctx.queue.len()
    };
    ctx.pending_select_on_growth.set(Some(first_added_idx));
    // The row the `play` arm resolves and starts: the first queued link.
    let first_link = uris
        .iter()
        .find(|uri| tagged_stream_entry(uri).is_some_and(|(_, intent)| intent.is_audio()))
        .cloned();
    let enqueue: Vec<Enqueue> =
        uris.into_iter().map(|path| Enqueue::File { path }).collect();
    ctx.command(move |client| {
        client.enqueue_multiple(enqueue, None, position, replace)?;
        Ok(())
    });
    if let Some(link) = first_link.filter(|_| play) {
        // The rows are in the queue now (the query below is queued behind
        // the enqueue command): find the id the entry was given and resolve
        // it, so playback starts without waiting for a second user action.
        let lookup = link.clone();
        let inserted = ctx.query_sync(move |client| {
            let songs = client.playlist_info()?.unwrap_or_default();
            Ok(songs.iter().find(|song| song.file == lookup).map(|song| song.id))
        });
        match inserted {
            Ok(Some(song_id)) => {
                // Hold MPD while the first row resolves, exactly as a play
                // request does: a running player cannot open the link, fails it
                // and advances, which is the reported "it skips through
                // multiple entries" on a freshly imported playlist. The
                // replacement starts this row by id once the stream lands.
                crate::ui::panes::queue::hold_mpd_while_resolving(ctx, song_id);
                // The import's autoplay is a user play request (2026-10-25).
                play_tagged_queue_entry(ctx, &link, song_id);
            }
            _ => status_warn!("Cannot find the queued stream entry to resolve"),
        }
    }
    Ok(())
}
/// Round 96: queue **one** YouTube search result (the Queue tab's `Shift+S`
/// popup) as an audio link, optionally starting it. It is the same
/// tagged-link path the paste popup and stored playlists take — the entry
/// resolves when it is played — but the wording fits a single result (the
/// playlist helper above counts "playlist entries").
pub fn queue_search_result(ctx: &Ctx, link: &str, title: &str, play: bool) -> bool {
    let uri = tagged_stream_link(link, StreamIntent::Audio);
    if let Err(err) = enqueue_stream_links(ctx, vec![uri], play, false) {
        status_error!("Failed to queue the stream: {err}");
        return false;
    }
    if play {
        status_info!("Resolving \"{title}\" — playing it now");
    } else {
        status_info!("Queued \"{title}\" — it resolves when it is played");
    }
    true
}
/// Where the persistent video playlist lives: `<cache_dir>/video-playlist.json`
/// (default `~/.cache/s2udio/video-playlist.json`, round 19 — the stream/
/// video playlist is s2udio-only and kept out of rmpc's cache).
pub fn video_playlist_path(cache_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    s2udio_cache_path(cache_dir, "video-playlist.json")
}
/// Persist the video playlist (the Queue tab's Video list survives mpv
/// closing, audio playback and restarts).
pub fn save_video_playlist(ctx: &Ctx) {
    let path = video_playlist_path(ctx.config.cache_dir.as_deref());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entries: Vec<serde_json::Value> = ctx
        .video_playlist
        .borrow()
        .iter()
        .map(|e| {
            serde_json::json!(
                { "title" : e.title, "url" : e.url, "duration" : e.duration,
                "original_url" : e.original_url, }
            )
        })
        .collect();
    if let Ok(bytes) = serde_json::to_vec(&entries) {
        if let Err(err) = std::fs::write(&path, bytes) {
            log::error!(error:? = err; "Failed to save the video playlist");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rig() -> (crate::ctx::Ctx, crossbeam::channel::Receiver<WorkRequest>) {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        Box::leak(Box::new(client_rx));
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (work_tx, work_rx.clone()),
            (client_tx, crossbeam::channel::unbounded().1),
        );
        (ctx, work_rx)
    }

    /// The reported asymmetry: the audio queue showed a pasted link the
    /// moment it was added (as a tagged row that resolves later), while the
    /// video queue showed nothing until `yt-dlp` answered — a silent wait of
    /// several seconds. The row now lands as a resolving placeholder straight
    /// away, and the resolve is what it waits for.
    #[test]
    fn a_pasted_video_link_lands_in_the_queue_before_it_resolves() {
        let (ctx, work_rx) = rig();
        let link = "https://www.youtube.com/watch?v=AAA";
        queue_videos(&ctx, &[PastedItem::Yt(link.to_owned())], true, false);
        {
            let playlist = ctx.video_playlist.borrow();
            assert_eq!(playlist.len(), 1, "the row is there immediately");
            assert!(playlist[0].pending, "and says it is still resolving");
            assert_eq!(playlist[0].lookup_url(), link, "the link is its key");
        }
        assert_eq!(
            ctx.queue_tab.get(),
            crate::ctx::QueueTabMode::Video,
            "the tab switches to the list the row went into"
        );
        assert!(
            matches!(
                work_rx.try_recv(),
                Ok(WorkRequest::ResolveYtStreams { urls, .. }) if urls == vec![link.to_owned()]
            ),
            "the link is handed to the resolver"
        );
    }

    /// A resolve that produced nothing still settles the rows it inserted:
    /// the failed link's row stops spinning and keeps the link as its title
    /// (instead of spinning forever on a dead video).
    #[test]
    fn a_failed_resolve_stops_the_row_spinning() {
        let (ctx, _work_rx) = rig();
        let link = "https://www.youtube.com/watch?v=CCC";
        *ctx.video_playlist.borrow_mut() =
            vec![crate::core::mpv::MpvPlaylistEntry::pending_link(link)];
        crate::ui::modals::paste::apply_resolved_streams(
            &ctx,
            Vec::new(),
            YtAction::AddToVideoQueue,
            vec![format!("{link}: video unavailable")],
        );
        let playlist = ctx.video_playlist.borrow();
        assert_eq!(playlist.len(), 1, "the row is kept, not duplicated");
        assert!(!playlist[0].pending, "the spinner stops");
        assert_eq!(playlist[0].url, link);
    }

    /// A mixed paste keeps its order: local files are queued as they are and
    /// the links follow as placeholders.
    #[test]
    fn a_mixed_paste_queues_the_local_files_and_the_links_together() {
        let (ctx, work_rx) = rig();
        let link = "https://www.youtube.com/watch?v=BBB";
        queue_videos(
            &ctx,
            &[
                PastedItem::VideoFile("/tmp/a.mp4".to_owned()),
                PastedItem::Yt(link.to_owned()),
            ],
            true,
            false,
        );
        let playlist = ctx.video_playlist.borrow();
        assert_eq!(playlist.len(), 2);
        assert_eq!(playlist[0].url, "/tmp/a.mp4");
        assert!(!playlist[0].pending, "a local file needs no resolve");
        assert!(playlist[1].pending);
        drop(playlist);
        assert!(matches!(work_rx.try_recv(), Ok(WorkRequest::ResolveYtStreams { .. })));
    }
}

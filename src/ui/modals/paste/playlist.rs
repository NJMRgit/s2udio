use anyhow::Result;
use crate::{
    ctx::Ctx,
    shared::{
        events::{PlaylistAction, PlaylistPick},
        macros::{modal, status_error, status_info, status_warn},
        mpd_client_ext::MpdClientExt as _,
        ytdlp::{
            ReplaceAction, StreamDownloadSpec, StreamIntent, YtDlpContent, YtDlpItem, YtListMeta,
            tagged_stream_link,
        },
    },
    ui::modals::{
        input_modal::InputModal,
        menu::modal::MenuModal,
        select_modal::SelectModal,
    },
};

use super::{
    PastedItem, downloads_dir, enqueue_stream_links, mpd_addable_path, s2udio_cache_path,
    stream_meta_key,
};

/// Round 79: a link is a playlist link when it carries a playlist id
/// (`/playlist?list=…` or `watch?v=…&list=…`).
pub(super) fn is_playlist_link(url: &str) -> bool {
    matches!(url.parse::<YtDlpContent>(), Ok(YtDlpContent::Playlist(_)))
}

/// Round 83: a playlist link's queue rows. The playlist's items are added
/// as YouTube **stream entries** — nothing is downloaded: each item's
/// stream URL is resolved in the background
/// (`PlaylistAction::QueueStreams`) and the entry is added as it lands, so
/// a 10-track playlist is in the queue in seconds instead of after ten
/// full downloads. `audio` queues them for MPD (the audio queue), else for
/// the video playlist; `autoplay` starts the first entry that lands.
pub(super) fn import_playlist(ctx: &Ctx, url: &str, audio: bool, autoplay: bool) {
    // 2026-10-24: listing a 1000+ track playlist takes seconds, so say so
    // where the rows will land: switch to that queue and mark the import in
    // flight (the Queue list paints its loading row until the listings land).
    begin_playlist_import(ctx, !audio);
    let action = PlaylistAction::QueueStreams { audio, autoplay };
    if let Err(err) = ctx.ytdlp_manager.resolve_playlist(url, action) {
        // Nothing will ever land: drop the loading row again.
        finish_playlist_import(ctx);
        status_error!("Failed to add the playlist: {err}");
    }
}

/// The import is under way: show the queue it is adding to.
///
/// Switches to the Queue tab (when the user is elsewhere) and to the list the
/// rows will land in — the audio queue or the Video list — and marks
/// [`Ctx::playlist_import`] so that list paints its `Loading playlist…` row.
/// Switching at dispatch time (not when the rows arrive) is the point: the
/// listing itself is the slow part.
pub(crate) fn begin_playlist_import(ctx: &Ctx, video: bool) {
    ctx.playlist_import
        .replace(Some(crate::ctx::PlaylistImport { video }));
    ctx.queue_tab.set(if video {
        crate::ctx::QueueTabMode::Video
    } else {
        crate::ctx::QueueTabMode::Audio
    });
    let on_queue_tab = ctx
        .active_tab
        .as_str()
        .eq_ignore_ascii_case(crate::config::tabs::QUEUE_TAB_NAME);
    // A config can drop the Queue tab from its tab list; switching to a tab
    // that is not laid out would leave the user on a blank page, so the import
    // then only marks itself (the rows still land in the queue).
    let queue_tab_configured = ctx
        .config
        .tabs
        .names
        .iter()
        .any(|name| name.as_str().eq_ignore_ascii_case(crate::config::tabs::QUEUE_TAB_NAME));
    if !on_queue_tab && queue_tab_configured {
        let queue_tab: crate::config::tabs::TabName =
            crate::config::tabs::QUEUE_TAB_NAME.into();
        if let Err(err) = ctx.app_event_sender.send(crate::AppEvent::UiEvent(
            crate::ui::UiAppEvent::ChangeTab(queue_tab),
        )) {
            log::error!(
                error:? = err;
                "Could not switch to the Queue tab for the playlist import"
            );
        }
    }
    let _ = ctx.render();
}

/// The listing is over — its rows landed, or it failed: drop the loading row.
pub(crate) fn finish_playlist_import(ctx: &Ctx) {
    if ctx.playlist_import.replace(None).is_some() {
        let _ = ctx.render();
    }
}

/// Round 82: a picker row (`Add to playlist` / `Create Playlist` /
/// `Download > … > All files | Select files`). The playlist's items are
/// listed first — one fast yt-dlp call, they carry their titles — and the
/// picker opens as soon as they are known.
pub(super) fn pick_playlist_items(ctx: &Ctx, url: &str, kind: PlaylistPick) {
    if let Err(err) = ctx
        .ytdlp_manager
        .resolve_playlist(url, PlaylistAction::Pick(kind))
    {
        status_error!("Failed to read the playlist: {err}");
    } else {
        status_info!("Fetching the playlist's items…");
    }
}

/// Round 82: the playlist's items are known — open the picker the row asked
/// for. Called from the event loop when a `Pick` request lands.
pub fn open_playlist_picker(ctx: &Ctx, items: Vec<YtDlpItem>, kind: PlaylistPick) {
    match kind {
        PlaylistPick::AddToPlaylist => {
            playlist_item_picker(ctx, "Add to playlist", "Add", items, |ctx, chosen| {
                playlist_kind_choice(ctx, chosen, false);
                Ok(())
            })
        }
        PlaylistPick::CreatePlaylist => {
            playlist_item_picker(ctx, "Create playlist", "Create", items, |ctx, chosen| {
                playlist_kind_choice(ctx, chosen, true);
                Ok(())
            })
        }
        // `All files`: every track, no picker.
        PlaylistPick::SaveAll { audio_only } => {
            queue_playlist_save_files(ctx, items, audio_only);
        }
        PlaylistPick::SaveSelected { audio_only } => {
            playlist_item_picker(ctx, "Select files", "Download", items, move |ctx, chosen| {
                queue_playlist_save_files(ctx, chosen, audio_only);
                Ok(())
            })
        }
    }
}

/// Round 82: the checkbox list of a playlist's videos — the picker every
/// picker row opens, drawn like the download `Chapter(s)` picker.
/// `confirm` labels its footer button; `on_pick` receives the ticked items
/// in playlist order.
fn playlist_item_picker(
    ctx: &Ctx,
    title: &'static str,
    confirm: &'static str,
    items: Vec<YtDlpItem>,
    on_pick: impl FnOnce(&Ctx, Vec<YtDlpItem>) -> Result<()> + Send + Sync + 'static,
) {
    modal!(
        ctx, MenuModal::new(ctx).width(60).title(title).list_section(
            ctx,
            move |mut section| {
                for item in &items {
                    section.add_check_item(item.display_title().to_owned(), false);
                }
                section.add_check_buttons(confirm, "Cancel");
                section.check_list(move |ctx, selected, _choice| {
                    let chosen: Vec<YtDlpItem> = selected
                        .iter()
                        .filter_map(|idx| items.get(*idx).cloned())
                        .collect();
                    on_pick(ctx, chosen)
                });
                Some(section)
            }
        )
    );
}

/// Round 82: the Audio/Video step of the playlist rows that end in a stored
/// playlist.
fn playlist_kind_choice(ctx: &Ctx, items: Vec<YtDlpItem>, create: bool) {
    modal!(
        ctx, SelectModal::builder().ctx(ctx)
        .options(vec!["Audio".to_owned(), "Video".to_owned()])
        .confirm_label("Select").title("Audio or video")
        .on_confirm(move |ctx, kind, _idx| {
            if create {
                playlist_create_target(ctx, items, kind == "Audio");
            } else {
                playlist_add_target(ctx, items, kind == "Audio");
            }
            Ok(())
        }).build()
    );
}

/// Round 82: add the picked videos of a playlist to an existing one — the
/// stored playlist is chosen by name.
fn playlist_add_target(ctx: &Ctx, items: Vec<YtDlpItem>, audio: bool) {
    let radio_playlist = ctx.config.radio.playlist.clone();
    let playlists = ctx
        .query_sync(move |client| {
            Ok(client
                .picker_playlists(&radio_playlist)?
                .into_iter()
                .map(|p| p.name)
                .collect::<Vec<_>>())
        })
        .unwrap_or_default();
    if playlists.is_empty() {
        status_warn!("No playlists yet — use 'Create Playlist'");
        return;
    }
    modal!(
        ctx, SelectModal::builder().ctx(ctx).options(playlists)
        .confirm_label("Add").title("Select a playlist")
        .on_confirm(move |ctx, selected, _idx| {
            playlist_store_items(ctx, &items, &selected, audio);
            Ok(())
        }).build()
    );
}

/// Round 82: create a playlist from the picked videos of another one.
fn playlist_create_target(ctx: &Ctx, items: Vec<YtDlpItem>, audio: bool) {
    modal!(
        ctx, InputModal::new(ctx).title("Create playlist").confirm_label("Save")
        .input_label("Playlist name:").on_confirm(move |ctx, value| {
            let name = value.to_owned();
            // Round 86: links + intent tag, resolved when the entry is
            // played (see `playlist_store_items`).
            let intent =
                if audio { StreamIntent::Audio } else { StreamIntent::Video };
            let uris: Vec<String> = items
                .iter()
                .map(|item| tagged_stream_link(&item.to_url(), intent))
                .collect();
            ctx.command(move |client| {
                client.create_playlist(&name, uris)?;
                Ok(())
            });
            Ok(())
        })
    );
}

/// Round 82: store the picked videos in an existing playlist — `audio`
/// resolves each item's stream URL first (the single-link row's path),
/// video keeps the links.
fn playlist_store_items(ctx: &Ctx, items: &[YtDlpItem], playlist: &str, audio: bool) {
    if items.is_empty() {
        return;
    }
    // Round 86: the entry keeps the LINK with its intent tag — a resolved
    // stream URL would expire within hours; the link is resolved when the
    // entry is played (the Playlists tab) or added to the queue.
    let intent = if audio { StreamIntent::Audio } else { StreamIntent::Video };
    let uris: Vec<String> = items
        .iter()
        .map(|item| tagged_stream_link(&item.to_url(), intent))
        .collect();
    let playlist = playlist.to_owned();
    ctx.command(move |client| {
        client.add_to_playlist_multiple(&playlist, uris)?;
        Ok(())
    });
}

/// Round 79: save every track of an already-resolved playlist link as its
/// own file (one yt-dlp run per track; the download manager runs them one at
/// a time).
pub fn queue_playlist_save_files(ctx: &Ctx, items: Vec<YtDlpItem>, audio_only: bool) {
    let Some(output_dir) = downloads_dir() else {
        status_warn!("Cannot determine the downloads folder (~/Downloads)");
        return;
    };
    let count = items.len();
    for item in items {
        ctx.ytdlp_manager.queue_stream_download(
            item,
            StreamDownloadSpec {
                output_dir: output_dir.clone(),
                audio_only,
                split_chapters: false,
                sections: Vec::new(),
                on_complete: ReplaceAction::None,
            },
        );
    }
    status_info!(
        "Saving {count} track(s) of the playlist to s2udio-downloads as {}",
        if audio_only { "audio" } else { "video" }
    );
}

/// Add audio items to an existing playlist: direct URIs immediately,
/// YouTube-style links after their streams resolve (the work request
/// Split pasted items into MPD-addable audio URIs (direct files/URLs) and
/// YouTube-style links (they need resolving before their streams can be
/// added to a stored playlist). Torrents never reach this splitter (they
/// are routed through the `[Torrent]` section); the arms stay defensive.
pub(super) fn playlist_audio_uris(items: &[PastedItem]) -> (Vec<String>, Vec<String>) {
    items
        .iter()
        .fold(
            (Vec::new(), Vec::new()),
            |(mut direct, mut yt), item| match item {
                PastedItem::File(path) | PastedItem::VideoFile(path) => {
                    direct.push(mpd_addable_path(path));
                    (direct, yt)
                }
                PastedItem::Url(url) | PastedItem::VideoUrl(url) => {
                    direct.push(url.clone());
                    (direct, yt)
                }
                PastedItem::Yt(url) => {
                    yt.push(url.clone());
                    (direct, yt)
                }
                PastedItem::Torrent(_) | PastedItem::Magnet(_) => (direct, yt),
            },
        )
}
/// Round 89: the stored-playlist URIs of a pasted row's **audio** arm -
/// direct files/URLs as they are, YouTube-style links tagged `#s2u-audio`
/// (round 86: the link is stored and resolved when the entry is played).
pub(super) fn paste_audio_uris(items: &[PastedItem]) -> Vec<String> {
    let (direct, yt) = playlist_audio_uris(items);
    let mut uris = direct;
    uris.extend(yt.iter().map(|url| tagged_stream_link(url, StreamIntent::Audio)));
    uris
}
/// Round 89: the same for a pasted row's **video** arm - the video
/// playlist's own URIs, each tagged `#s2u-video` (the tag is a URL
/// fragment: MPD stores it, yt-dlp ignores it, and the Playlists tab plays
/// such an entry through mpv).
pub(super) fn paste_video_uris(items: &[PastedItem]) -> Vec<String> {
    super::video_playlist_uris(items)
        .iter()
        .map(|url| tagged_stream_link(url, StreamIntent::Video))
        .collect()
}
/// Round 89: the `Add to playlist` target picker, shared by a pasted row's
/// Audio and Video arms.
pub(super) fn paste_add_to_playlist(ctx: &Ctx, uris: Vec<String>) -> Result<()> {
    let radio_playlist = ctx.config.radio.playlist.clone();
    let playlists = ctx.query_sync(move |client| {
        Ok(client
            .picker_playlists(&radio_playlist)?
            .into_iter()
            .map(|p| p.name)
            .collect::<Vec<_>>())
    })?;
    if playlists.is_empty() {
        status_warn!("No playlists yet — use 'Create Playlist'");
        return Ok(());
    }
    modal!(
        ctx, SelectModal::builder().ctx(ctx).options(playlists)
        .confirm_label("Add").title("Select a playlist")
        .on_confirm(move |ctx, selected, _idx| {
            ctx.command(move |client| {
                client.add_to_playlist_multiple(&selected, uris)?;
                Ok(())
            });
            Ok(())
        }).build()
    );
    Ok(())
}
/// Round 89: the `Create Playlist` name prompt, shared by a pasted row's
/// Audio and Video arms.
pub(super) fn paste_create_playlist(ctx: &Ctx, uris: Vec<String>) -> Result<()> {
    modal!(
        ctx, InputModal::new(ctx).title("Create playlist")
        .confirm_label("Save").input_label("Playlist name:")
        .on_confirm(move |ctx, value| {
            let name = value.to_owned();
            let uris = uris.clone();
            ctx.command(move |client| {
                client.create_playlist(&name, uris)?;
                Ok(())
            });
            Ok(())
        })
    );
    Ok(())
}
/// Round 95: cache file of the flat playlist listings:
/// `<cache_dir>/yt-list-meta.json`, keyed by the video id (or the link when
/// it names none). A playlist added to the queue enters as its links, so the
/// queue rows take their title/channel/duration from here — and a stored
/// playlist built from such a listing keeps showing them after a restart.
pub fn yt_list_meta_path(cache_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    s2udio_cache_path(cache_dir, "yt-list-meta.json")
}
pub fn load_yt_list_meta(
    cache_dir: Option<&std::path::Path>,
) -> std::collections::HashMap<String, YtListMeta> {
    std::fs::read(yt_list_meta_path(cache_dir))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}
fn save_yt_list_meta(
    cache_dir: Option<&std::path::Path>,
    cache: &std::collections::HashMap<String, YtListMeta>,
) {
    let path = yt_list_meta_path(cache_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_vec(cache) {
        let _ = std::fs::write(path, data);
    }
}
/// Round 95: remember a flat playlist listing's metadata (title, channel,
/// duration) for every item that has a title — in the session store the
/// queue rows read and in `<cache_dir>/yt-list-meta.json`.
pub fn remember_yt_list_meta(ctx: &Ctx, items: &[YtDlpItem]) {
    let entries: Vec<(String, YtListMeta)> = items
        .iter()
        .filter_map(|item| {
            let title = item.title.clone().filter(|t| !t.trim().is_empty())?;
            let meta = YtListMeta {
                title,
                channel: item.channel.clone().filter(|c| !c.trim().is_empty()),
                duration: item.duration,
            };
            Some((stream_meta_key(&item.to_url()), meta))
        })
        .collect();
    if entries.is_empty() {
        return;
    }
    {
        let mut store = ctx.yt_list_meta.borrow_mut();
        for (key, meta) in &entries {
            store.insert(key.clone(), meta.clone());
        }
    }
    let cache_dir = ctx.config.cache_dir.as_deref();
    let mut cache = load_yt_list_meta(cache_dir);
    for (key, meta) in entries {
        cache.insert(key, meta);
    }
    save_yt_list_meta(cache_dir, &cache);
}
/// Round 95: a pasted playlist link's queue rows, all at once. The flat
/// listing that named the items carries each one's title, channel and
/// duration, so the rows are complete the moment they land — no per-item
/// resolve, which is what made a playlist trickle into the queue before.
///
/// Audio queues the items as tagged audio **links** in one `add` (each
/// resolves when played); video builds the mpv playlist entries from the
/// listing (mpv plays the links itself). `autoplay` starts the first item.
pub fn queue_playlist_streams(
    ctx: &Ctx,
    items: Vec<YtDlpItem>,
    audio: bool,
    autoplay: bool,
) {
    if items.is_empty() {
        status_warn!("The playlist has no tracks to add");
        return;
    }
    // 2026-10-24: the listing is over and its rows are about to land — the
    // import's `Loading playlist…` row goes now (whichever way the import was
    // started, the marker lives here rather than in each caller).
    finish_playlist_import(ctx);
    remember_yt_list_meta(ctx, &items);
    let count = items.len();
    if audio {
        let uris: Vec<String> = items
            .iter()
            .map(|item| tagged_stream_link(&item.to_url(), StreamIntent::Audio))
            .collect();
        if let Err(err) = enqueue_stream_links(ctx, uris, autoplay, false) {
            status_error!("Failed to add the playlist: {err}");
            return;
        }
        status_info!(
            "{count} track(s) queued — each stream resolves when it is played"
        );
    } else {
        let entries: Vec<crate::core::mpv::MpvPlaylistEntry> = items
            .iter()
            .map(|item| {
                let url = item.to_url();
                let mut entry = crate::core::mpv::MpvPlaylistEntry::new(
                    item.display_title().to_owned(),
                    url.clone(),
                    item.duration,
                );
                entry.original_url = Some(url);
                entry
            })
            .collect();
        if autoplay {
            crate::core::mpv::add_to_video_playlist(ctx, entries.clone(), true);
            crate::core::mpv::play_video_entries(ctx, entries);
            status_info!("Added {count} video(s) to the video queue and started playback");
        } else {
            crate::core::mpv::add_to_video_playlist(ctx, entries, false);
            status_info!("Added {count} video(s) to the video queue");
        }
    }
}
/// Round 95: queue the links of a stored playlist (or the marked rows of
/// one) in one command list, in the order they were read. `play` starts the
/// first link (resolved on the spot), `replace` empties the queue first.
/// Returns false when there is nothing to queue.
pub fn queue_stored_stream_links(
    ctx: &Ctx,
    uris: Vec<String>,
    play: bool,
    replace: bool,
) -> bool {
    if uris.is_empty() {
        return false;
    }
    let count = uris.len();
    if let Err(err) = enqueue_stream_links(ctx, uris, play, replace) {
        status_error!("Failed to queue the playlist: {err}");
        return true;
    }
    if !play {
        status_info!(
            "{count} playlist entry/entries queued — each stream resolves when it is played"
        );
    }
    true
}

/// 2026-10-24: the loading row a pasted playlist import shows while its
/// listing runs — the wait is spent in `yt-dlp --flat-playlist`, before any
/// row exists, so the import switches to the queue it is adding to and marks
/// itself in flight for the Queue pane to paint.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod playlist_import_loading_tests {
    use super::{begin_playlist_import, finish_playlist_import, import_playlist};
    use crate::{
        ctx::{Ctx, QueueTabMode},
        shared::events::{AppEvent, WorkRequest},
        ui::UiAppEvent,
    };

    const PLAYLIST: &str = "https://www.youtube.com/playlist?list=PL1234567890";

    fn rig() -> (
        Ctx,
        crossbeam::channel::Receiver<AppEvent>,
        crossbeam::channel::Receiver<WorkRequest>,
    ) {
        let app = crossbeam::channel::unbounded();
        let app_rx = app.1.clone();
        let work = crossbeam::channel::unbounded();
        let work_rx = work.1.clone();
        let mut ctx = crate::tests::fixtures::ctx(
            (app.0, app.1),
            (work.0, work.1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // The fixture starts on a non-Queue tab, like the user does.
        ctx.active_tab = crate::config::tabs::TabName::from("test_tab");
        (ctx, app_rx, work_rx)
    }

    fn switched_to_queue(app_rx: &crossbeam::channel::Receiver<AppEvent>) -> bool {
        app_rx.try_iter().any(|event| {
            matches!(
                event,
                AppEvent::UiEvent(UiAppEvent::ChangeTab(tab))
                    if tab.as_str().eq_ignore_ascii_case(crate::config::tabs::QUEUE_TAB_NAME)
            )
        })
    }

    /// Starting an import switches the TUI to the queue it is adding to (the
    /// tab and the target list) and marks the import in flight, so the queue
    /// list paints its loading row.
    #[test]
    fn a_playlist_import_switches_to_its_queue_and_marks_itself() {
        let (ctx, app_rx, _work_rx) = rig();
        import_playlist(&ctx, PLAYLIST, false, false);

        assert!(
            switched_to_queue(&app_rx),
            "starting an import must switch to the Queue tab"
        );
        assert_eq!(
            ctx.queue_tab.get(),
            QueueTabMode::Video,
            "a video import lands in the Video list"
        );
        assert_eq!(
            *ctx.playlist_import.borrow(),
            Some(crate::ctx::PlaylistImport { video: true }),
            "the import is marked in flight for the loading row"
        );
    }

    /// The audio target switches to the audio list instead.
    #[test]
    fn an_audio_import_selects_the_audio_list() {
        let (ctx, _app_rx, _work_rx) = rig();
        import_playlist(&ctx, PLAYLIST, true, true);
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Audio);
        assert_eq!(*ctx.playlist_import.borrow(), Some(crate::ctx::PlaylistImport { video: false }));
    }

    /// Already on the Queue tab: only the list may change, never a tab jump
    /// (which would re-enter the tab and reset the session's view).
    #[test]
    fn a_queue_tab_import_does_not_re_switch_the_tab() {
        let (mut ctx, app_rx, _work_rx) = rig();
        ctx.active_tab =
            crate::config::tabs::TabName::from(crate::config::tabs::QUEUE_TAB_NAME);
        ctx.queue_tab.set(QueueTabMode::Video);

        begin_playlist_import(&ctx, false);

        assert!(!switched_to_queue(&app_rx), "already there: no tab switch");
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Audio, "the list still follows");
    }

    /// The listing is over: the marker is cleared, whatever the outcome.
    #[test]
    fn finishing_clears_the_marker() {
        let (ctx, _app_rx, _work_rx) = rig();
        begin_playlist_import(&ctx, true);
        assert!(ctx.playlist_import.borrow().is_some());

        finish_playlist_import(&ctx);
        assert_eq!(ctx.playlist_import.borrow().clone(), None);
    }

    /// A link the manager cannot even parse fails before the listing starts;
    /// the marker must not be left behind for a loading row nothing will ever
    /// replace.
    #[test]
    fn a_rejected_link_does_not_leave_the_loading_row_behind() {
        let (ctx, _app_rx, _work_rx) = rig();
        import_playlist(&ctx, "not a link", true, false);
        assert_eq!(ctx.playlist_import.borrow().clone(), None);
    }

    /// The rows arriving is what removes the loading row: `queue_playlist_streams`
    /// clears the marker as it queues them, so the row cannot outlive the wait.
    #[test]
    fn the_arriving_rows_clear_the_loading_row() {
        let (ctx, _app_rx, _work_rx) = rig();
        begin_playlist_import(&ctx, false);
        assert!(ctx.playlist_import.borrow().is_some());

        let item = crate::shared::ytdlp::YtDlpItem {
            id: "abcdefghijk".to_owned(),
            filename: "A track".to_owned(),
            kind: crate::shared::ytdlp::YtDlpHost::Youtube,
            title: Some("A track".to_owned()),
            duration: None,
            channel: None,
        };
        super::queue_playlist_streams(&ctx, vec![item], true, false);

        assert_eq!(
            ctx.playlist_import.borrow().clone(),
            None,
            "the loading row goes as its rows land"
        );
    }

    /// A valid link actually asks for the listing it will paint the row for.
    #[test]
    fn a_valid_link_queues_the_listing_request() {
        let (ctx, _app_rx, work_rx) = rig();
        import_playlist(&ctx, PLAYLIST, true, false);

        let requests: Vec<WorkRequest> = work_rx.try_iter().collect();
        assert_eq!(requests.len(), 1, "one listing request");
        assert!(
            matches!(
                &requests[0],
                WorkRequest::YtDlpResolvePlaylist {
                    action: crate::shared::events::PlaylistAction::QueueStreams {
                        audio: true,
                        autoplay: false,
                    },
                    ..
                }
            ),
            "the listing carries the popup's queue action"
        );
    }
}

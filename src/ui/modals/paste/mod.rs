//! Paste / drag&drop handling: parse pasted text for audio files and audio
//! links (incl. YouTube-style URLs) and offer a play/enqueue popup.
//!
//! Middle-click pastes, Ctrl+V and terminal drag&drop all arrive as a single
//! bracketed-paste event (`AppEvent::UserPaste`). The text is split into
//! items — local audio file paths, direct audio URLs and
//! YouTube/Soundcloud/NicoVideo links — and when at least one item is found
//! a popup offers:
//!
//! - **Play** (single item only): play immediately without touching the
//!   queue (a temporary entry, hidden from the queue list and removed when
//!   the song changes).
//! - **Add to queue**: insert after the currently/last played track.
//! - **Append to queue**: add to the end of the queue.
//! - **Cancel**: do nothing.

use crate::{
    config::utils::tilde_expand,
    ctx::Ctx,
    shared::{
        events::{PlaylistPick, WorkRequest},
        macros::{modal, status_info, status_warn},
        ytdlp::{untag_stream_link, yt_video_id},
    },
    ui::modals::menu::{ListSection, modal::MenuModal},
};
use anyhow::Result;

mod download;
mod parse;
#[cfg(test)]
mod play_e2e_tests;
mod playlist;
#[path = "queue-stream.rs"]
mod queue_stream;
mod resolve;
#[cfg(test)]
mod resolve_e2e_tests;
mod warm;

pub(crate) use download::stream_daemon_job;
#[allow(unused_imports)]
pub use download::{
    DOWNLOADS_DIR_NAME, DlWaitState, StreamDownloadTarget, downloads_dir, on_dl_daemon_started,
    on_dl_wait_progress, on_dl_wait_ready, open_stream_download_menu, open_stream_downloads_picker,
    prune_empty_engines, queue_stream_download, queue_stream_download_sections,
    queue_stream_downloads, start_daemon_play, stream_download_target, untrack_daemon_streams,
};
pub use parse::parse_paste;
#[allow(unused_imports)]
pub(crate) use parse::{
    is_audio_extension, is_torrent_extension, is_video_extension, magnet_infohash,
    magnet_infohash_full,
};
#[allow(unused_imports)]
pub use playlist::{
    load_yt_list_meta, open_playlist_picker, queue_playlist_save_files, queue_playlist_streams,
    queue_stored_stream_links, remember_yt_list_meta, yt_list_meta_path,
};
pub use queue_stream::{queue_search_result, save_video_playlist, video_playlist_path};
#[allow(unused_imports)]
pub use resolve::{
    apply_resolved_streams, clear_queue_resolve_markers, clear_stream_parse_pending,
    drop_unresolvable_queue_entry, expired_stream_original_url, is_stream_row, load_yt_cache,
    mark_stream_parse_pending, queue_total_time, re_resolve_expired_queue_entry,
    resolve_tagged_queue_entry, resolve_tagged_queue_entry_in_place, save_yt_cache,
    song_stream_info, stream_info_for, stream_parse_pending, stream_resolve_pending_for,
    stream_row_meta, video_entry_duration, video_entry_stream_meta, video_queue_total_time,
    warm_selected_queue_info, yt_cache_path,
};
pub use warm::{
    clear_mpv_mpris_art, current_yt_info, delete_mpv_mpris_state, ensure_chapters,
    ensure_mpris_metadata, mpris_art_path, mpv_mpris_art_path, mpv_mpris_state_path, mpv_yt_info,
    warm_next_queue_link, write_mpv_mpris_state,
};
#[cfg(test)]
pub(crate) use warm::LOOK_AHEAD_WINDOW;

pub(crate) use download::commit_to_daemon;
use download::{download_submenu, playlist_download_menu, stream_via_daemon};
use playlist::{
    import_playlist, is_playlist_link, paste_add_to_playlist, paste_audio_uris,
    paste_create_playlist, paste_video_uris, pick_playlist_items,
};
use queue_stream::{
    enqueue_items, enqueue_stream_links, mpd_addable_path, play_item, play_video_now, queue_videos,
    video_playlist_uris,
};
use resolve::fresh_cached_stream_info;
use warm::warm_chapters;

/// Result id of the paste "play" query (routed through the Radio pane, which
/// owns the temporary play-entry lifecycle).
pub const PASTE_PLAY: &str = "paste_play";
/// What to do with resolved YouTube-style streams once `yt-dlp -g` has
/// produced their direct URLs (carried on the work request so the result can
/// be applied without extra state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YtAction {
    /// Play the first resolved stream as a temporary (queue-free) entry.
    Play,
    /// Append to the end of the queue.
    Append,
    /// Insert after the current track and start playing the first inserted
    /// stream immediately.
    AddAfterCurrentAndPlay,
    /// Re-resolve a stale queue entry whose signed stream URL has expired:
    /// delete the dead entry, insert the fresh stream at the same position
    /// and play it.
    ReplaceAndPlay(u32),
    /// Launch mpv on the original links once they are resolved, so the
    /// session shows the real titles (and the thumbnails/chapters are
    /// available); mpv itself plays the links.
    PlayVideo,
    /// Insert the resolved videos into the persistent video playlist right
    /// after the currently playing entry.
    AddToVideoQueue,
    /// Append the resolved videos to the persistent video playlist.
    AppendVideoQueue,
    /// Insert the resolved videos into the persistent video playlist after
    /// the current entry and start playing them immediately.
    AddToVideoQueueAndPlay,
    /// Only refresh the cached stream info (startup re-fetch when a
    /// previously resolved stream is still playing); no queue action.
    Refresh,
    /// Replace the queue entry `u32` (still a tagged link) with the resolved
    /// stream at the same position **without** starting playback (round 98).
    /// Used by the round-95b look ahead: the *next* entry is swapped as soon
    /// as its stream is known, so MPD's next-song prefetch finds a playable
    /// URL and the current track is never cut short.
    ReplaceInPlace(u32),
}
/// A single recognized item of a paste.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PastedItem {
    /// A local audio file path.
    File(String),
    /// A direct http(s) audio stream/file URL.
    Url(String),
    /// A local video file path (played via mpv / audio through MPD).
    VideoFile(String),
    /// A direct http(s) video file URL (online video).
    VideoUrl(String),
    /// A YouTube/Soundcloud/NicoVideo link (resolved to a stream via yt-dlp).
    Yt(String),
    /// A `.torrent` source: a local path, a `file://` URI (already
    /// stripped), or an `http(s)` URL ending in `.torrent`.
    Torrent(String),
    /// A `magnet:?…` link (the full URI is kept; the infohash is used for
    /// labels and dedupe).
    Magnet(String),
}
impl PastedItem {
    /// Human-readable one-line label for status messages.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::File(path) | Self::VideoFile(path) | Self::Torrent(path) => {
                std::path::Path::new(path)
                    .file_name()
                    .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned())
            }
            Self::Url(url) | Self::VideoUrl(url) | Self::Yt(url) => url.clone(),
            Self::Magnet(magnet) => magnet_infohash(magnet)
                .map_or_else(|| "magnet link".to_owned(), |hash| format!("magnet:{hash}")),
        }
    }
}
/// MPD's music directory, read from the standard mpd.conf locations (the
/// `config` MPD command is TCP-restricted, so the file is parsed directly).
pub fn music_directory() -> Option<String> {
    for candidate in [
        "~/.config/mpd/mpd.conf",
        "/etc/mpd.conf",
        "/var/lib/mpd/mpd.conf",
        "/usr/local/etc/mpd.conf",
    ] {
        let content = std::fs::read_to_string(tilde_expand(candidate).as_ref()).ok()?;
        for line in content.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if let Some(rest) = line.strip_prefix("music_directory") {
                let rest = rest.trim().trim_start_matches('=').trim();
                if let Some(value) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                    let expanded = tilde_expand(value);
                    return Some(expanded.trim_end_matches('/').to_owned());
                }
            }
        }
    }
    None
}
/// The replacement id of the paste popup: when a torrent scan completes,
/// a rebuilt popup replaces the open one in place (same spot in the modal
/// stack, selection reset) instead of stacking a second copy on top.
const PASTE_MODAL_REPLACEMENT_ID: &str = "paste_modal";
/// Open the play/enqueue popup for the parsed items.
pub fn show_paste_modal(ctx: &Ctx, items: Vec<PastedItem>) {
    *ctx.paste_modal_items.borrow_mut() = Some(items.clone());
    let menu = paste_menu(ctx, items);
    ctx.paste_modal_id.set(Some(crate::ui::modals::Modal::id(&menu)));
    modal!(ctx, menu);
}
/// (Re)build the paste popup. `refresh_paste_modal` calls this when a
/// torrent scan completes so the `[Torrent]` section swaps its Loading row
/// for the play actions the scan enables.
/// The page link of a pasted item when yt-dlp can fetch it (only those can be
/// downloaded); local files and plain stream URLs cannot.
fn youtube_link_of(item: &PastedItem) -> Option<String> {
    match item {
        PastedItem::Yt(url) => Some(url.clone()),
        _ => None,
    }
}
fn paste_menu(ctx: &Ctx, items: Vec<PastedItem>) -> MenuModal<'static> {
    let count = items.len();
    // Round 79: a pasted playlist link gets its own `[Playlist]` rows — the
    // whole playlist can be imported into the queue or saved as files.
    let playlist_url: Option<String> = (count == 1)
        .then(|| match &items[0] {
            PastedItem::Yt(url) if is_playlist_link(url) => Some(url.clone()),
            _ => None,
        })
        .flatten();
    // Round 84: a link that carries a playlist gets the `[Playlist]` rows
    // and nothing else that would repeat them — the single-link queue rows
    // (`Add to queue…` / `Add to playlist` / `Create Playlist`) and the
    // single-link `Download` are the same actions on a smaller item set, so
    // they are dropped (only `Play` stays: it plays the pasted video
    // itself). Without this the menu showed the same five rows twice.
    let playlist_carried = playlist_url.is_some();
    // A link that names only a playlist (no video id of its own) has nothing
    // else to play or download, so its single-link rows are dropped.
    let playlist_only = playlist_url.is_some()
        && matches!(&items[0], PastedItem::Yt(url) if yt_video_id(url).is_none());
    let title = if count == 1 {
        match &items[0] {
            // A pasted link is a web stream: its raw URL (query string,
            // timestamps, tracking parameters) makes a poor title.
            PastedItem::Url(_) | PastedItem::VideoUrl(_) | PastedItem::Yt(_) => {
                if playlist_only {
                    " YouTube Playlist ".to_owned()
                } else {
                    " Web Stream ".to_owned()
                }
            }
            item => format!(" Paste: {} ", item.label()),
        }
    } else {
        format!(" Paste: {} items ", count)
    };
    let audio: Vec<PastedItem> = items
        .iter()
        .filter(|item| {
            !(playlist_only && matches!(item, PastedItem::Yt(_)))
                && matches!(
                    item,
                    PastedItem::File(_)
                        | PastedItem::Url(_)
                        | PastedItem::VideoFile(_)
                        | PastedItem::VideoUrl(_)
                        | PastedItem::Yt(_)
                )
        })
        .cloned()
        .collect();
    let video: Vec<PastedItem> = items
        .iter()
        .filter(|item| {
            !(playlist_only && matches!(item, PastedItem::Yt(_)))
                && matches!(
                    item,
                    PastedItem::VideoFile(_) | PastedItem::VideoUrl(_) | PastedItem::Yt(_)
                )
        })
        .cloned()
        .collect();
    // Round 78: the consolidated queue rows act by item kind — a pasted web
    // stream counts as audio (`Play > Video` watches it in mpv instead) and a
    // local video file goes to the video playlist.
    let queue_audio: Vec<PastedItem> = items
        .iter()
        .filter(|item| {
            !(playlist_only && matches!(item, PastedItem::Yt(_)))
                && matches!(item, PastedItem::File(_) | PastedItem::Url(_) | PastedItem::Yt(_))
        })
        .cloned()
        .collect();
    let queue_video: Vec<PastedItem> = items
        .iter()
        .filter(|item| matches!(item, PastedItem::VideoFile(_) | PastedItem::VideoUrl(_)))
        .cloned()
        .collect();
    // The `Download` row's items: the pasted web streams yt-dlp can fetch.
    // Only offered for a single link — the audio/video and chapter choice is
    // per link.
    let downloads: Vec<PastedItem> = items
        .iter()
        .filter(|item| {
            !(playlist_only && matches!(item, PastedItem::Yt(_)))
                && matches!(item, PastedItem::Yt(_))
        })
        .cloned()
        .collect();
    let download_url = (downloads.len() == 1).then(|| youtube_link_of(&downloads[0])).flatten();
    let download_info = download_url.as_deref().and_then(|url| stream_info_for(ctx, url));
    // A pasted link's chapters are only known once it has been resolved (they
    // are fetched when a link is played otherwise): ask for them in the
    // background so the download options can list them. The popup refreshes
    // in place when the info lands (`YtAction::Refresh`).
    warm_chapters(ctx, &downloads);
    let torrents: Vec<PastedItem> = items
        .iter()
        .filter(|item| matches!(item, PastedItem::Torrent(_) | PastedItem::Magnet(_)))
        .cloned()
        .collect();
    let menu = MenuModal::new(ctx)
        .width(60)
        .title(title)
        .replacement_id(PASTE_MODAL_REPLACEMENT_ID)
        .list_section(ctx, |mut section| {
            // Round 79: a playlist link — import the whole playlist into
            // the queue (optionally starting it) or save every track as
            // a file. Above the single-link rows: the playlist is what
            // the link is mostly about.
            // Round 79/82/83: a playlist link — the playlist is what
            // the link is mostly about, so its rows come first. The
            // queue rows add the playlist's items as YouTube *stream*
            // entries (nothing is downloaded; each track is fetched
            // when it is played), the picker rows let the user choose
            // which of the playlist's videos to act on.
            if let Some(url) = playlist_url.clone() {
                section.header("[Playlist]");
                // Add to queue and play > Audio | Video: everything
                // lands in the queue, the first entry starts playing.
                let mut play_sub = ListSection::new(ctx.config.theme.current_item_style);
                for (label, audio) in [("Audio", true), ("Video", false)] {
                    let play_url = url.clone();
                    play_sub.add_item(label, move |ctx| {
                        import_playlist(ctx, &play_url, audio, true);
                        Ok(())
                    });
                }
                play_sub.add_item("Cancel", |_ctx| Ok(()));
                section.add_submenu_item("Add to queue and play", play_sub);

                // Add to queue > Audio | Video: the same without
                // starting playback.
                let mut queue_sub = ListSection::new(ctx.config.theme.current_item_style);
                for (label, audio) in [("Audio", true), ("Video", false)] {
                    let queue_url = url.clone();
                    queue_sub.add_item(label, move |ctx| {
                        import_playlist(ctx, &queue_url, audio, false);
                        Ok(())
                    });
                }
                queue_sub.add_item("Cancel", |_ctx| Ok(()));
                section.add_submenu_item("Add to queue", queue_sub);

                // Add to playlist / Create Playlist: which videos of the
                // playlist, then Audio or Video, then the target.
                let add_url = url.clone();
                section.add_item("Add to playlist", move |ctx| {
                    pick_playlist_items(ctx, &add_url, PlaylistPick::AddToPlaylist);
                    Ok(())
                });
                let create_url = url.clone();
                section.add_item("Create Playlist", move |ctx| {
                    pick_playlist_items(ctx, &create_url, PlaylistPick::CreatePlaylist);
                    Ok(())
                });
                section.add_submenu_item("Download", playlist_download_menu(ctx, url));
            }

            // Play: the audio stream through MPD, or the video through
            // mpv — a submenu, because a pasted web stream is both.
            // Round 85 (user): a playlist has many items, so a row that
            // plays ONE item as a temporary, queue-free entry has no
            // place in a playlist paste — playlist streams only ever
            // enter through the queue (`Add to queue…` above).
            let single_audio = (audio.len() == 1).then(|| audio[0].clone());
            let single_video = (video.len() == 1).then(|| video[0].clone());
            if !playlist_carried && (single_audio.is_some() || single_video.is_some()) {
                let mut play = ListSection::new(ctx.config.theme.current_item_style);
                if let Some(item) = single_audio {
                    play.add_item("Audio", move |ctx| play_item(ctx, &item));
                }
                if let Some(item) = single_video {
                    play.add_item("Video", move |ctx| {
                        play_video_now(ctx, std::slice::from_ref(&item));
                        Ok(())
                    });
                }
                section.add_submenu_item("Play", play);
            }
            // Round 89 (user): a pasted web stream reaches the library as
            // **audio** (MPD's queue / a stored playlist entry tagged
            // `#s2u-audio`) or as **video** (the mpv video playlist /
            // `#s2u-video`) - `Play` and `Download` already ask, these
            // rows did not. The `Audio | Video` step appears whenever both
            // destinations have items (`queue_audio` is what MPD takes,
            // `video` what the video playlist takes, and a pasted web
            // stream is in both); a paste of a single kind keeps today's
            // one-line row.
            let kind_split = !queue_audio.is_empty() && !video.is_empty();
            if !playlist_carried && (!queue_audio.is_empty() || !queue_video.is_empty()) {
                if kind_split {
                    let mut play_sub = ListSection::new(ctx.config.theme.current_item_style);
                    let play_audio = queue_audio.clone();
                    play_sub
                        .add_item("Audio", move |ctx| enqueue_items(ctx, &play_audio, true, true));
                    let play_video = video.clone();
                    play_sub.add_item("Video", move |ctx| {
                        queue_videos(ctx, &play_video, true, true);
                        Ok(())
                    });
                    play_sub.add_item("Cancel", |_ctx| Ok(()));
                    section.add_submenu_item("Add to queue and play", play_sub);

                    let mut append_sub = ListSection::new(ctx.config.theme.current_item_style);
                    let append_audio = queue_audio.clone();
                    append_sub.add_item("Audio", move |ctx| {
                        enqueue_items(ctx, &append_audio, false, false)
                    });
                    let append_video = video.clone();
                    append_sub.add_item("Video", move |ctx| {
                        queue_videos(ctx, &append_video, false, false);
                        Ok(())
                    });
                    append_sub.add_item("Cancel", |_ctx| Ok(()));
                    section.add_submenu_item("Add to queue", append_sub);

                    let mut add_sub = ListSection::new(ctx.config.theme.current_item_style);
                    let add_audio = paste_audio_uris(&queue_audio);
                    add_sub.add_item("Audio", move |ctx| paste_add_to_playlist(ctx, add_audio));
                    let add_video = paste_video_uris(&video);
                    add_sub.add_item("Video", move |ctx| paste_add_to_playlist(ctx, add_video));
                    add_sub.add_item("Cancel", |_ctx| Ok(()));
                    section.add_submenu_item("Add to playlist", add_sub);

                    let mut create_sub = ListSection::new(ctx.config.theme.current_item_style);
                    let create_audio = paste_audio_uris(&queue_audio);
                    create_sub
                        .add_item("Audio", move |ctx| paste_create_playlist(ctx, create_audio));
                    let create_video = paste_video_uris(&video);
                    create_sub
                        .add_item("Video", move |ctx| paste_create_playlist(ctx, create_video));
                    create_sub.add_item("Cancel", |_ctx| Ok(()));
                    section.add_submenu_item("Create Playlist", create_sub);
                } else {
                    let play_audio = queue_audio.clone();
                    let play_video = queue_video.clone();
                    section = section.item("Add to queue and play", move |ctx| {
                        if !play_video.is_empty() {
                            queue_videos(ctx, &play_video, true, true);
                        }
                        if !play_audio.is_empty() {
                            enqueue_items(ctx, &play_audio, true, true)?;
                        }
                        Ok(())
                    });
                    let append_audio = queue_audio.clone();
                    let append_video = queue_video.clone();
                    section = section.item("Add to queue", move |ctx| {
                        if !append_video.is_empty() {
                            queue_videos(ctx, &append_video, false, false);
                        }
                        if !append_audio.is_empty() {
                            enqueue_items(ctx, &append_audio, false, false)?;
                        }
                        Ok(())
                    });
                    // Round 86: entries keep the LINK plus its intent tag -
                    // a resolved stream URL expires within hours. The link
                    // is resolved when the entry is played.
                    let mut entry_uris = paste_audio_uris(&queue_audio);
                    entry_uris.extend(paste_video_uris(&queue_video));
                    let add_uris = entry_uris.clone();
                    section = section
                        .item("Add to playlist", move |ctx| paste_add_to_playlist(ctx, add_uris));
                    let create_uris = entry_uris;
                    section = section.item("Create Playlist", move |ctx| {
                        paste_create_playlist(ctx, create_uris)
                    });
                }
            }
            // (A playlist link's own `Download` row lives in its
            // `[Playlist]` section above.)
            if !playlist_carried && let Some(url) = download_url.clone() {
                let download = download_submenu(ctx, url, download_info.clone());
                section.add_submenu_item("Download", download);
            }
            if !torrents.is_empty() {
                section.header("[Torrent]");
                if ctx.config.torrent.enabled {
                    let multi = torrents.len() > 1;
                    for item in &torrents {
                        let key = torrent_item_key(item);
                        let label = item.label();
                        if multi {
                            section.header(format!("{label}:"));
                        }
                        // Each torrent gets one row: it opens the torrent
                        // picker, which shows the file list and (once the
                        // scan has its metainfo) the Stream / Download
                        // buttons. The scan itself starts here, so the
                        // picker opens onto either the finished list or the
                        // short "Reading torrent…" wait.
                        let pick_item = item.clone();
                        let pick_key = key.clone();
                        let pick_label = label.clone();
                        start_torrent_scan(ctx, item, &key);
                        section = section.item(format!("Open {label}"), move |ctx| {
                            open_torrent_picker(ctx, &pick_item, &pick_key, &pick_label)
                        });
                    }
                } else {
                    section.header("Torrent streaming disabled");
                }
            }
            section.add_item("Cancel", |_ctx| Ok(()));
            section.set_on_close(on_paste_popup_close);
            Some(section)
        })
        .build();
    menu
}
/// The stable scan-map key of a pasted torrent item: a magnet's **full
/// infohash** (round 20 — the same torrent pasted twice, even via a
/// different magnet URI, reuses the same scan/engine) or the `.torrent`
/// path/URL (identical pastes share one scan). Must match
/// `TorrentItem::source_key` (the work thread reports scan results under
/// that key).
pub fn torrent_item_key(item: &PastedItem) -> String {
    match item {
        PastedItem::Magnet(magnet) => {
            magnet_infohash_full(magnet).unwrap_or_else(|| magnet.clone())
        }
        PastedItem::Torrent(torrent) => torrent.clone(),
        _ => unreachable!("torrent item key is only asked for torrent items"),
    }
}
/// A pasted torrent item in its work-queue form.
fn torrent_item(item: &PastedItem) -> crate::core::torrent::TorrentItem {
    match item {
        PastedItem::Magnet(magnet) => crate::core::torrent::TorrentItem::Magnet(magnet.clone()),
        PastedItem::Torrent(torrent) => crate::core::torrent::TorrentItem::Torrent(torrent.clone()),
        _ => unreachable!("torrent items only"),
    }
}
/// A torrent scan's **file list landed** (phase 3): store it so the torrent
/// picker can render its rows and enable Download, and tell the open picker
/// to refresh in place. The torrent is usually not `live` yet — Stream stays
/// gated until `on_torrent_scanned` lands.
pub fn on_torrent_scan_files(
    ctx: &Ctx,
    key: String,
    name: String,
    files: Vec<crate::core::torrent::ScannedFile>,
) {
    log::debug!(
        key:?; "on_torrent_scan_files files={} picker_open={}", files.len(), ctx
        .torrent_picker_key.borrow().is_some()
    );
    ctx.torrent_file_lists
        .borrow_mut()
        .insert(key, (name, files));
    crate::ui::modals::torrent_picker::refresh_torrent_picker(ctx);
}

/// A torrent scan landed (round 17): store the result (or the failure) and
/// refresh the open paste popup so its `[Torrent]` section shows the play
/// actions. A result for a closed popup is dropped outright — dropping the
/// scan kills the engine's rqbit child.
pub fn on_torrent_scanned(
    ctx: &Ctx,
    key: String,
    result: Result<crate::core::torrent::TorrentScan, String>,
) {
    ctx.torrent_scans_pending.borrow_mut().remove(&key);
    ctx.torrent_scan_cancels.borrow_mut().remove(&key);
    ctx.torrent_scan_progress.borrow_mut().remove(&key);
    let picker_open = ctx
        .torrent_picker_key
        .borrow()
        .as_ref()
        .is_some_and(|open| open == &key);
    log::debug!(
        key:?; "on_torrent_scanned ok={} picker_open={}", result.is_ok(), picker_open
    );
    if ctx.paste_modal_items.borrow().is_none() && !picker_open {
        return;
    }
    ctx.torrent_scans.borrow_mut().insert(key, result);
    if picker_open {
        // Phase 4 landed: the picker's Stream button ungates in place.
        crate::ui::modals::torrent_picker::refresh_torrent_picker(ctx);
    } else {
        refresh_paste_modal(ctx);
    }
}
/// A torrent scan's live progress arrived (round 18): store it and
/// refresh the open paste popup so its wait window's counter and the
/// DL-speed / needed-speed check update. A progress event for a closed
/// popup is dropped — the popup's close hook already cancelled the scan.
pub fn on_torrent_scan_progress(
    ctx: &Ctx,
    key: String,
    progress: crate::core::torrent::TorrentScanProgress,
) {
    let picker_open = ctx.torrent_picker_key.borrow().is_some();
    if ctx.paste_modal_items.borrow().is_none() && !picker_open {
        return;
    }
    ctx.torrent_scan_progress.borrow_mut().insert(key, progress);
    // The wait may be an open torrent picker (the scan's phase 3/4) or the
    // paste popup's own `[Torrent]` row — refresh whichever is showing.
    if ctx.torrent_picker_key.borrow().is_some() {
        crate::ui::modals::torrent_picker::refresh_torrent_picker(ctx);
    } else {
        refresh_paste_modal(ctx);
    }
}
/// The paste popup's close hook: forget the popup's own state, then release
/// the torrent scans it started — **unless** the torrent picker took them
/// over. Opening the picker pops the popup on its way in, so this hook runs
/// while the picker is about to render those very scans; cancelling them here
/// would kill the engine it lists files from. The picker cancels its own
/// unfinished scans when it closes.
fn on_paste_popup_close(ctx: &Ctx) {
    ctx.paste_modal_items.borrow_mut().take();
    ctx.paste_modal_id.set(None);
    ctx.paste_chapter_warm.borrow_mut().clear();
    if ctx.torrent_picker_key.borrow().is_none() {
        cancel_in_flight_scans(ctx);
    }
    ctx.torrent_scans.borrow_mut().retain(|_, result| result.is_ok());
}

/// Cancel every in-flight torrent scan (round 18): signal the scan
/// threads' cancel channels so they stop waiting and drop their engines
/// (killing rqbit) promptly, then forget the scan bookkeeping. Called by
/// the paste popup's close hook (Esc / Cancel / an action) and by the
/// playback-start cleanup paths that drop the popup without its hook.
fn cancel_in_flight_scans(ctx: &Ctx) {
    // Clone the cancel senders first (round 55 F1 sweep): the `for`/`if
    // let` scrutinee borrows live for the whole loop, so no `borrow_mut()`
    // may fire on the same RefCell inside it — collect, then signal, then
    // clear as separate statements.
    let cancels: Vec<crossbeam::channel::Sender<()>> = ctx
        .torrent_scans_pending
        .borrow()
        .iter()
        .filter_map(|key| ctx.torrent_scan_cancels.borrow().get(key).cloned())
        .collect();
    for cancel in cancels {
        let _ = cancel.send(());
    }
    ctx.torrent_scan_cancels.borrow_mut().clear();
    ctx.torrent_scan_progress.borrow_mut().clear();
    ctx.torrent_scans_pending.borrow_mut().clear();
}
/// Rebuild the open paste popup (a torrent scan changed its `[Torrent]`
/// section). No-op when no paste popup is open.
pub fn refresh_paste_modal(ctx: &Ctx) {
    let Some(items) = ctx.paste_modal_items.borrow().clone() else { return };
    log::debug!("Refreshing the paste popup in place (a background resolve landed)");
    let menu = paste_menu(ctx, items);
    modal!(ctx, menu);
}
/// Route a torrent play action (round 54). Committed actions
/// (`download: true` — "Stream and download" / the picker's "Download &
/// Play") always go to the downloader daemon: the job survives the TUI
/// and playback runs through the daemon engine. Plain streams go through
/// the daemon engine too when a committed job for the same torrent is
/// active (§2.2 — one engine per cache dir); otherwise they use the
/// ephemeral TUI engine exactly as before round 54.
pub(crate) fn play_torrent_route(
    ctx: &Ctx,
    item: &PastedItem,
    key: &str,
    indices: Vec<usize>,
    download: bool,
) -> Result<()> {
    if download {
        return commit_to_daemon(ctx, item, key, indices, true);
    }
    let infohash = scan_infohash(ctx, key, item);
    let daemon_routed = infohash.as_deref().is_some_and(|hash| {
        crate::core::dlctl::read_state()
            .as_ref()
            .is_some_and(|state| crate::core::dlctl::job_active_for_infohash(state, hash))
    });
    if daemon_routed {
        log::debug!(
            key:?; "Plain re-stream routed through the downloader daemon (active committed job)"
        );
        return stream_via_daemon(ctx, item, key, indices);
    }
    play_scanned_or_fresh(ctx, item, key, indices)
}
/// The infohash of a scanned torrent (the scan's engine-reported value,
/// falling back to the magnet URI's for magnets scanned before the field
/// existed or with the scan gone).
fn scan_infohash(ctx: &Ctx, key: &str, item: &PastedItem) -> Option<String> {
    ctx.torrent_scans
        .borrow()
        .get(key)
        .cloned()
        .and_then(Result::ok)
        .and_then(|scan| scan.info_hash.clone())
        .or_else(|| match item {
            PastedItem::Magnet(magnet) => magnet_infohash_full(magnet),
            _ => None,
        })
}
/// Play a scanned torrent's files (round 17, the PLAIN-stream path): take
/// the scanned engine out of `Ctx.torrent_scans` and hand playback to the
/// event loop (`AppEvent::TorrentScannedPlay`). `indices` empty = the
/// single best playable file; when the scan is gone (replaced engine,
/// closed popup) the fresh-engine `play_torrent` path takes over.
fn play_scanned_or_fresh(
    ctx: &Ctx,
    item: &PastedItem,
    key: &str,
    indices: Vec<usize>,
) -> Result<()> {
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    log::debug!(
        key:?; "play_scanned_or_fresh found={} indices={}", scan.is_some(), indices.len()
    );
    let Some(scan) = scan else {
        return play_torrent(ctx, std::slice::from_ref(item));
    };
    let file_indices: Vec<usize> = if indices.is_empty() {
        match scan.pick_playable() {
            Some(pick) => vec![pick.index],
            None => return play_torrent(ctx, std::slice::from_ref(item)),
        }
    } else {
        indices.into_iter().filter(|i| *i < scan.files.len()).collect()
    };
    if file_indices.is_empty() {
        return play_torrent(ctx, std::slice::from_ref(item));
    }
    ctx.app_event_sender
        .send(crate::shared::events::AppEvent::TorrentScannedPlay { scan, file_indices })
        .map_err(|err| anyhow::anyhow!("Failed to start torrent playback: {err}"))?;
    Ok(())
}
/// Close the paste popup (used by the daemon flows: the committed action
/// takes over from the popup, and a leftover popup could re-offer the
/// stale scan's TUI-engine actions).
fn close_paste_popup(ctx: &Ctx) {
    if let Some(id) = ctx.paste_modal_id.take() {
        let _ = ctx
            .app_event_sender
            .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(id)));
    }
    ctx.paste_modal_items.borrow_mut().take();
    cancel_in_flight_scans(ctx);
}
/// Start the scan for a pasted torrent if it is not already running: the
/// popup calls this while building, so the `[Torrent]` row can open onto a
/// finished file list instead of an empty wait. Idempotent — a scan that is
/// in flight (or already landed) is never started twice.
fn start_torrent_scan(ctx: &Ctx, item: &PastedItem, key: &str) {
    if ctx.torrent_scans.borrow().contains_key(key) {
        return;
    }
    if ctx.torrent_scans_pending.borrow().contains(key) {
        return;
    }
    ctx.torrent_scans_pending.borrow_mut().insert(key.to_owned());
    let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
    ctx.torrent_scan_cancels.borrow_mut().insert(key.to_owned(), cancel_tx);
    let item = torrent_item(item);
    let _ = ctx.work_sender.send(WorkRequest::ScanTorrent { item, cancel: cancel_rx });
}

/// Open the torrent picker (the file list with Stream / Download) for a
/// pasted torrent. The picker owns the whole torrent flow from here: it
/// renders its rows the moment phase 3 lands and un-gates Stream on phase 4,
/// refreshing in place.
///
/// The paste popup closes behind it — the picker replaces it as the torrent
/// UI, and keeping the popup open would re-offer the same `[Torrent]` row.
/// The popup's own close hook runs first (closing it is what the picker row
/// does), and the scan is NOT cancelled there: the picker is the thing that
/// needs it.
fn open_torrent_picker(
    ctx: &Ctx,
    item: &PastedItem,
    key: &str,
    label: &str,
) -> Result<()> {
    if !ctx.config.torrent.enabled {
        status_warn!("Torrent streaming is disabled in the config");
        return Ok(());
    }
    // Make sure the scan is running even if the popup built before it started.
    start_torrent_scan(ctx, item, key);
    // Close the paste popup behind the picker: the picker replaces it as the
    // torrent UI, and leaving it open would strand it under the picker and
    // re-offer the same `[Torrent]` row on the way back. Unlike
    // `close_paste_popup` this must NOT cancel the in-flight scans — the
    // picker is about to render them.
    if let Some(id) = ctx.paste_modal_id.take() {
        let _ = ctx
            .app_event_sender
            .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(id)));
    }
    ctx.paste_modal_items.borrow_mut().take();
    let label = label.to_owned();
    let picker_key = key.to_owned();
    // The picker resolves its item from here on every rebuild (see
    // `TorrentPicker::new`): the popup may close underneath it, so the item
    // must outlive the popup's item list.
    *ctx.torrent_picker_item.borrow_mut() = Some(item.clone());
    *ctx.torrent_picker_key.borrow_mut() = Some(picker_key.clone());
    let picker =
        crate::ui::modals::torrent_picker::TorrentPicker::new(ctx, picker_key, label, 0);
    modal!(ctx, picker);
    Ok(())
}

/// Insert the synthetic yt-info entries for a torrent play (round 17):
/// title = file name, channel = torrent name, keyed by each stream URL —
/// the Queue tab's info box, MPRIS artist and the mpv poll look the info
/// up that way. In-memory only: the stream URL embeds the rqbit auth
/// token, so nothing may persist it.
pub fn remember_torrent_entries(
    ctx: &Ctx,
    torrent_name: &str,
    entries: &[crate::core::mpv::MpvPlaylistEntry],
) {
    let mut yt_info = ctx.yt_info.borrow_mut();
    for entry in entries {
        yt_info.insert(
            entry.url.clone(),
            crate::shared::ytdlp::YtStreamInfo {
                url: entry.url.clone(),
                title: entry.title.clone(),
                channel: Some(torrent_name.to_owned()),
                ..Default::default()
            },
        );
    }
}
/// The mpv playlist entries for the given files of a scanned torrent: one
/// entry per file (title = file name, url = that file's stream URL,
/// duration unknown). Every torrent play action (single file, play all,
/// selection) fills the video queue through this builder, so the queue
/// list, mpv playlist and MPRIS titles work like a Jellyfin season play.
pub fn torrent_entries(
    scan: &crate::core::torrent::TorrentScan,
    file_indices: &[usize],
) -> Vec<crate::core::mpv::MpvPlaylistEntry> {
    file_indices
        .iter()
        .filter_map(|i| scan.files.get(*i))
        .map(|f| {
            crate::core::mpv::MpvPlaylistEntry::new(
                f.name.clone(),
                scan.engine().stream_url(&scan.torrent_id, f.index as u64),
                None,
            )
        })
        .collect()
}
/// Stream a pasted torrent/magnet on a fresh engine (the PLAIN-stream
/// path, round 54): start rqbit, add the torrent, pick the largest
/// playable file and hand its stream URL to mpv. The engine work runs on
/// the work thread (`WorkRequest::PlayTorrent`); the prepared stream
/// arrives as `WorkDone::TorrentStreamPrepared`, which keeps the engine
/// alive and launches the mpv session.
///
/// Round 17: this is the fallback for the scanned play actions (the
/// scanned engine is gone — replaced or the popup closed). Committed
/// downloads never come here — they go to the downloader daemon.
fn play_torrent(ctx: &Ctx, items: &[PastedItem]) -> Result<()> {
    let Some(item) = items.first() else { return Ok(()) };
    let torrent_item = torrent_item(item);
    if !ctx.config.torrent.enabled {
        status_warn!("Torrent streaming is disabled in the config");
        return Ok(());
    }
    ctx.work_sender
        .send(WorkRequest::PlayTorrent { item: torrent_item })
        .map_err(|err| anyhow::anyhow!("Failed to request torrent stream: {err}"))?;
    status_info!("Starting torrent stream…");
    Ok(())
}
/// Seconds since the Unix epoch.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
/// Round 95: the key a YouTube-style link is remembered and looked up under —
/// the video id when the link names one (`watch?v=ID`, `youtu.be/ID`,
/// `…&list=…`, `shorts/ID` all name the same video), else the link itself.
/// One video therefore matches however its link is spelled.
fn stream_meta_key(link: &str) -> String {
    let link = untag_stream_link(link);
    yt_video_id(link).unwrap_or_else(|| link.to_owned())
}
/// Round 19: s2udio-only cache files (video playlist, mpv MPRIS state,
/// MPRIS art) live in `~/.cache/s2udio/` by default — separate from rmpc's
/// cache so stream/video playlists never collide with rmpc/MPD state. An
/// explicit `cache_dir` in the config still wins; when no cache dir is
/// configured and the legacy `~/.cache/rmpc/…` file exists, that path is
/// returned (migration) so pre-round-19 state keeps loading.
fn s2udio_cache_path(cache_dir: Option<&std::path::Path>, file: &str) -> std::path::PathBuf {
    if let Some(dir) = cache_dir {
        return dir.join(file);
    }
    let new = crate::shared::paths::s2udio_cache_dir()
        .unwrap_or_else(|| {
            crate::config::utils::tilde_expand("~/.cache/s2udio").into_owned().into()
        })
        .join(file);
    let legacy: std::path::PathBuf =
        crate::config::utils::tilde_expand("~/.cache/rmpc").into_owned().into();
    if new.exists() {
        new
    } else if legacy.join(file).exists() {
        legacy.join(file)
    } else {
        new
    }
}
/// The full popup flow from a paste event: parse, and when something
/// recognized was found, show the popup. Returns true when a popup was
/// opened.
pub fn handle_paste(ctx: &Ctx, text: &str) -> bool {
    let items = parse_paste(text);
    if items.is_empty() {
        return false;
    }
    // A paste that is nothing but one torrent/magnet opens the torrent
    // picker straight away: the picker IS the torrent UI (it shows the
    // label while the metainfo is fetched, then the file list in the same
    // window). Routing it through the paste popup first would add a
    // pointless `[Torrent]` + `Open …` step.
    if let [item] = items.as_slice()
        && ctx.config.torrent.enabled
        && matches!(item, PastedItem::Torrent(_) | PastedItem::Magnet(_))
    {
        return open_torrent_picker(ctx, item, &torrent_item_key(item), &item.label())
            .is_ok();
    }
    show_paste_modal(ctx, items);
    true
}

#[cfg(test)]
mod picker_handoff_tests {
    //! The paste popup hands its torrents to the torrent picker, and the
    //! picker outlives the popup. These pin the scan bookkeeping across that
    //! hand-off: the popup's close hook must not kill the scans the picker is
    //! about to render, while a picker closed before its scan completes must
    //! still kill its engine.

    use super::*;

    fn test_ctx() -> Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    fn scanned_files() -> Vec<crate::core::torrent::ScannedFile> {
        vec![crate::core::torrent::ScannedFile {
            index: 0,
            name: "movie.mkv".to_owned(),
            length: 100,
        }]
    }

    #[test]
    fn the_popups_close_hook_spares_the_scan_the_picker_is_rendering() {
        // The picker opens onto a scan that is still running (or finished):
        // the popup is popped underneath it and its close hook runs. Cancelling
        // here would kill the engine and leave the picker with no file list.
        let ctx = test_ctx();
        let key = "magnet:?xt=urn:btih:0123456789abcdef".to_owned();
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        ctx.torrent_scan_cancels.borrow_mut().insert(key.clone(), cancel_tx);
        ctx.torrent_scans_pending.borrow_mut().insert(key.clone());
        // The picker has taken ownership.
        *ctx.torrent_picker_key.borrow_mut() = Some(key.clone());

        // The real close hook: with a picker owning the torrent, the scans
        // are left alone.
        on_paste_popup_close(&ctx);

        assert!(
            ctx.torrent_scan_cancels.borrow().contains_key(&key),
            "the scan's cancel channel survives the popup teardown"
        );
        assert!(
            ctx.torrent_scans_pending.borrow().contains(&key),
            "the scan is still tracked as pending"
        );
        assert!(cancel_rx.try_recv().is_err(), "no cancel signal was sent");
    }

    #[test]
    fn the_popup_still_cancels_scans_nobody_took_over() {
        // A plain dismissal (Esc on the popup) must keep the old behaviour:
        // the engine dies promptly.
        let ctx = test_ctx();
        let key = "magnet:?xt=urn:btih:0123456789abcdef".to_owned();
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        ctx.torrent_scan_cancels.borrow_mut().insert(key.clone(), cancel_tx);
        ctx.torrent_scans_pending.borrow_mut().insert(key.clone());

        on_paste_popup_close(&ctx);

        assert!(cancel_rx.try_recv().is_ok(), "the scan is cancelled");
        assert!(ctx.torrent_scan_cancels.borrow().is_empty(), "bookkeeping cleared");
    }

    #[test]
    fn closing_the_picker_mid_scan_cancels_its_engine() {
        // No completed scan: closing the picker must not orphan rqbit.
        let ctx = test_ctx();
        let key = "magnet:?xt=urn:btih:0123456789abcdef".to_owned();
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        ctx.torrent_scan_cancels.borrow_mut().insert(key.clone(), cancel_tx);
        ctx.torrent_scans_pending.borrow_mut().insert(key.clone());
        *ctx.torrent_picker_key.borrow_mut() = Some(key.clone());

        crate::ui::modals::torrent_picker::clear_picker_for_test(&ctx, &key);

        assert!(cancel_rx.try_recv().is_ok(), "the unfinished scan is cancelled");
        assert!(ctx.torrent_picker_key.borrow().is_none(), "picker state cleared");
    }

    #[test]
    fn closing_the_picker_after_the_scan_keeps_the_engine() {
        // A completed scan backs a download job / a repeat paste: keep it.
        let ctx = test_ctx();
        let key = "magnet:?xt=urn:btih:0123456789abcdef".to_owned();
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        ctx.torrent_scan_cancels.borrow_mut().insert(key.clone(), cancel_tx);
        ctx.torrent_scans.borrow_mut().insert(
            key.clone(),
            Ok(crate::core::torrent::TorrentScan {
                engine: None,
                torrent_id: "1".to_owned(),
                torrent_name: "fake.torrent".to_owned(),
                info_hash: None,
                files: scanned_files(),
            }),
        );
        *ctx.torrent_picker_key.borrow_mut() = Some(key.clone());

        crate::ui::modals::torrent_picker::clear_picker_for_test(&ctx, &key);

        assert!(cancel_rx.try_recv().is_err(), "a completed scan is not cancelled");
        assert!(ctx.torrent_scans.borrow().contains_key(&key), "the scan is kept");
    }
}

#[cfg(test)]
mod paste_entry_tests {
    //! Which modal a paste opens: a torrent-only paste goes straight to the
    //! torrent picker (the picker is the torrent UI), everything else keeps
    //! the consolidated paste popup.

    use super::*;

    fn ctx_with_torrent(enabled: bool) -> Ctx {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut config = (*ctx.config).clone();
        config.torrent.enabled = enabled;
        odr(config, ctx)
    }

    fn odr(config: crate::config::Config, ctx: Ctx) -> Ctx {
        Ctx { config: std::sync::Arc::new(config), ..ctx }
    }

    #[test]
    fn a_magnet_paste_opens_the_torrent_picker_directly() {
        let ctx = ctx_with_torrent(true);
        let opened = handle_paste(&ctx, "magnet:?xt=urn:btih:0123456789abcdef");
        assert!(opened, "a magnet is recognized");
        assert!(
            ctx.torrent_picker_key.borrow().is_some(),
            "the picker took the paste directly (no popup in between)"
        );
        assert!(
            ctx.paste_modal_items.borrow().is_none(),
            "no paste popup was opened for a pure torrent paste"
        );
        assert!(
            ctx.torrent_picker_item.borrow().is_some(),
            "the picker knows which torrent it acts on"
        );
    }

    #[test]
    fn a_mixed_paste_still_opens_the_popup() {
        let ctx = ctx_with_torrent(true);
        let opened = handle_paste(
            &ctx,
            // A direct stream URL parses without touching the disk (a local
            // path has to exist to be recognized).
            "magnet:?xt=urn:btih:0123456789abcdef\nhttps://example.com/song.mp3",
        );
        assert!(opened);
        assert!(
            ctx.paste_modal_items.borrow().is_some(),
            "a mixed paste keeps the consolidated popup"
        );
        assert!(
            ctx.torrent_picker_key.borrow().is_none(),
            "the picker is not opened directly for a mixed paste"
        );
    }

    #[test]
    fn a_torrent_paste_with_the_feature_disabled_opens_no_picker() {
        let ctx = ctx_with_torrent(false);
        let opened = handle_paste(&ctx, "magnet:?xt=urn:btih:0123456789abcdef");
        assert!(opened, "the paste is still recognized");
        assert!(
            ctx.torrent_picker_key.borrow().is_none(),
            "a disabled torrent feature must not open the picker"
        );
        assert!(
            ctx.paste_modal_items.borrow().is_some(),
            "it falls back to the popup, which shows the disabled notice"
        );
    }
}

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
use anyhow::Result;
use crate::{
    config::{
        tabs::{PaneType, TreeBrowserArgs},
        utils::tilde_expand,
    },
    ctx::Ctx, mpd::{QueuePosition, mpd_client::MpdClient},
    shared::{
        events::WorkRequest, macros::{modal, status_info, status_warn},
        mpd_client_ext::{Enqueue, MpdClientExt as _},
        ytdlp::YtDlpContent,
    },
    ui::modals::{
        input_modal::InputModal, menu::modal::MenuModal, select_modal::SelectModal,
    },
};
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
    /// Add the resolved streams to an existing stored playlist.
    AddToPlaylist(String),
    /// Create a new stored playlist from the resolved streams.
    CreatePlaylist(String),
    /// Only refresh the cached stream info (startup re-fetch when a
    /// previously resolved stream is still playing); no queue action.
    Refresh,
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
    fn label(&self) -> String {
        match self {
            Self::File(path) | Self::VideoFile(path) | Self::Torrent(path) => {
                std::path::Path::new(path)
                    .file_name()
                    .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned())
            }
            Self::Url(url) | Self::VideoUrl(url) | Self::Yt(url) => url.clone(),
            Self::Magnet(magnet) => {
                magnet_infohash(magnet)
                    .map_or_else(
                        || "magnet link".to_owned(),
                        |hash| format!("magnet:{hash}"),
                    )
            }
        }
    }
}
/// File extensions considered audio (lowercase, no dot).
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3",
    "flac",
    "ogg",
    "opus",
    "oga",
    "m4a",
    "m4b",
    "aac",
    "wav",
    "wma",
    "ape",
    "alac",
    "aiff",
    "aif",
    "wv",
    "mka",
    "spx",
    "ac3",
];
/// File extensions considered video (lowercase, no dot).
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4",
    "m4v",
    "mkv",
    "webm",
    "mov",
    "avi",
    "mpg",
    "mpeg",
    "ts",
    "m2ts",
    "mts",
    "flv",
    "wmv",
    "vob",
    "ogv",
    "3gp",
    "divx",
];
/// Whether a path/URL carries an audio extension (query strings and
/// fragments stripped first, so `…/song.mp3?x=1` matches). Used by the
/// paste classification and the torrent file picker.
pub(crate) fn is_audio_extension(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}
/// Whether a path/URL carries a video extension (query strings and
/// fragments stripped first, so `…/movie.mp4?x=1` matches). Used by the
/// paste popup's classification and the playlist audio/video detection.
pub(crate) fn is_video_extension(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}
/// Whether a path/URL carries a torrent metainfo extension (`.torrent`,
/// query strings and fragments stripped first).
pub(crate) fn is_torrent_extension(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}
/// A local path is a real `.torrent` file (~ expanded).
fn is_local_torrent(path: &str) -> bool {
    if !is_torrent_extension(path) {
        return false;
    }
    let expanded = tilde_expand(path);
    std::path::Path::new(expanded.as_ref()).is_file()
}
/// The full infohash of a magnet URI (lowercased): from
/// `xt=urn:btih:<hash>` or a bare `btih=<hash>` query parameter. `None`
/// when the magnet carries no recognizable infohash.
///
/// Round 20: this is the magnet's **canonical scan key** — the same
/// torrent pasted twice (even via a different magnet URI with extra
/// trackers) must hit the same `Ctx.torrent_scans` slot so the second
/// paste reuses the first engine instead of spawning a second rqbit
/// against the same cache dir.
pub(crate) fn magnet_infohash_full(magnet: &str) -> Option<String> {
    let query = magnet.split_once('?').map_or(magnet, |(_, q)| q);
    query
        .split('&')
        .find_map(|param| {
            param
                .strip_prefix("xt=urn:btih:")
                .or_else(|| param.strip_prefix("btih="))
                .map(|hash| hash.to_lowercase())
        })
}
/// The infohash prefix of a magnet URI (the first 8 characters, used for
/// labels): from `xt=urn:btih:<hash>` or a bare `btih=<hash>` query
/// parameter. `None` when the magnet carries no recognizable infohash.
pub(crate) fn magnet_infohash(magnet: &str) -> Option<String> {
    magnet_infohash_full(magnet).map(|hash| hash.chars().take(8).collect())
}
/// Unescape kitty's drag&drop escaping (`\ ` for spaces, `\\` for a
/// backslash) in a local path.
fn unescape_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(' ') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}
/// Split on whitespace while keeping backslash-escaped characters (kitty's
/// drag&drop escapes spaces as `\ `) inside one token.
fn split_unescaped(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                current.push(c);
                current.push(next);
                chars.next();
            } else {
                current.push(c);
            }
        } else if c.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}
/// Split the pasted text into recognized audio items. Anything unrecognized
/// is silently ignored; the paste is dropped entirely when nothing matches.
pub fn parse_paste(input: &str) -> Vec<PastedItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut seen_magnets = std::collections::HashSet::new();
    for raw in split_unescaped(input) {
        let token = raw.trim().trim_matches('"').trim_matches('\'').trim();
        if token.is_empty() {
            continue;
        }
        let Some(item) = classify(token) else { continue };
        let is_new = match &item {
            PastedItem::Magnet(magnet) => {
                let key = magnet_infohash_full(magnet).unwrap_or_else(|| magnet.clone());
                seen_magnets.insert(key)
            }
            _ => seen.insert(item.clone()),
        };
        if is_new {
            items.push(item);
        }
    }
    items
}
/// Classify a single whitespace-separated token.
fn classify(token: &str) -> Option<PastedItem> {
    if token.starts_with("magnet:") {
        return Some(PastedItem::Magnet(token.to_owned()));
    }
    if let Some(rest) = token.strip_prefix("file://") {
        let path = unescape_path(rest);
        if is_local_torrent(&path) {
            return Some(PastedItem::Torrent(path));
        }
        if is_local_audio(&path) {
            return Some(PastedItem::File(path));
        }
        if is_local_video(&path) {
            return Some(PastedItem::VideoFile(path));
        }
        return None;
    }
    if let Ok(_content) = token.parse::<YtDlpContent>() {
        return Some(PastedItem::Yt(token.to_owned()));
    }
    if token.starts_with("http://") || token.starts_with("https://") {
        if is_torrent_extension(token) {
            return Some(PastedItem::Torrent(token.to_owned()));
        }
        if is_audio_extension(token) {
            return Some(PastedItem::Url(token.to_owned()));
        }
        if is_video_extension(token) {
            return Some(PastedItem::VideoUrl(token.to_owned()));
        }
        return None;
    }
    let path = unescape_path(token);
    if is_local_torrent(&path) {
        return Some(PastedItem::Torrent(path));
    }
    if is_local_audio(&path) {
        return Some(PastedItem::File(path));
    }
    if is_local_video(&path) {
        return Some(PastedItem::VideoFile(path));
    }
    None
}
/// A local path is a real file with an audio extension (~ expanded).
fn is_local_audio(path: &str) -> bool {
    if !is_audio_extension(path) {
        return false;
    }
    let expanded = tilde_expand(path);
    std::path::Path::new(expanded.as_ref()).is_file()
}
/// A local path is a real file with a video extension (~ expanded).
fn is_local_video(path: &str) -> bool {
    if !is_video_extension(path) {
        return false;
    }
    let expanded = tilde_expand(path);
    std::path::Path::new(expanded.as_ref()).is_file()
}
/// MPD's music directory, read from the standard mpd.conf locations (the
/// `config` MPD command is TCP-restricted, so the file is parsed directly).
/// The downloads folder name (`s2udio-downloads`). Downloads land in
/// `~/Downloads/<name>`, outside the MPD library; the MPD tab shows the
/// folder as "Downloads" at the top of the library from a disk listing
/// (see `directories.rs`).
pub const DOWNLOADS_DIR_NAME: &str = "s2udio-downloads";
/// The downloads folder (`~/Downloads/s2udio-downloads`): stream
/// downloads, torrent "Download" / "Download all" saves and future saved
/// torrents land here. It lives OUTSIDE the MPD library — MPD cannot
/// play files from it (video plays via mpv; the browser lists the folder
/// from disk). `None` when `$HOME` is unset.
pub fn downloads_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join("Downloads").join(DOWNLOADS_DIR_NAME))
}
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
                if let Some(value) = rest
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                {
                    let expanded = tilde_expand(value);
                    return Some(expanded.trim_end_matches('/').to_owned());
                }
            }
        }
    }
    None
}
/// Convert an absolute local path to the MPD-relative path when it lives
/// under the music directory (MPD refuses absolute local paths over TCP).
/// Returns the path unchanged when it cannot be relativized.
fn mpd_addable_path(path: &str) -> String {
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
fn play_video_now(ctx: &Ctx, vids: &[PastedItem]) {
    if all_yt(vids) {
        let _ = ctx
            .work_sender
            .send(WorkRequest::ResolveYtStreams {
                urls: yt_urls(vids),
                action: YtAction::PlayVideo,
            });
        return;
    }
    crate::core::mpv::play_video_entries(ctx, video_entries_for(vids));
}
/// "Add / Append to queue": insert the video items into the persistent
/// video playlist (YouTube-style links after resolving them). With `play`
/// the added videos start playing immediately.
fn queue_videos(ctx: &Ctx, vids: &[PastedItem], after_current: bool, play: bool) {
    if all_yt(vids) {
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
                urls: yt_urls(vids),
                action,
            });
        return;
    }
    let entries = video_entries_for(vids);
    crate::core::mpv::add_to_video_playlist(ctx, entries.clone(), after_current);
    if play {
        crate::core::mpv::play_video_entries(ctx, entries);
    }
}
/// The stored-playlist URIs of the given video items: local files keep
/// their path, direct URLs and YouTube-style links their (stable) link —
/// matching what the video queue's *Create video playlist* stores, so the
/// playlists tab can show the cached titles.
fn video_playlist_uris(vids: &[PastedItem]) -> Vec<String> {
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
/// Split pasted items into MPD-addable audio URIs (direct files/URLs) and
/// YouTube-style links (they need resolving before their streams can be
/// added to a stored playlist). Torrents never reach this splitter (they
/// are routed through the `[Torrent]` section); the arms stay defensive.
fn playlist_audio_uris(items: &[PastedItem]) -> (Vec<String>, Vec<String>) {
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
/// Add audio items to an existing playlist: direct URIs immediately,
/// YouTube-style links after their streams resolve (the work request
/// carries the playlist name so the result handler can add them).
fn add_audio_items_to_playlist(
    ctx: &Ctx,
    direct: &[String],
    yt: &[String],
    playlist: &str,
) {
    if !direct.is_empty() {
        let direct = direct.to_vec();
        let playlist = playlist.to_owned();
        ctx.command(move |client| {
            client.add_to_playlist_multiple(&playlist, direct)?;
            Ok(())
        });
    }
    if !yt.is_empty() {
        let playlist = playlist.to_owned();
        let _ = ctx
            .work_sender
            .send(WorkRequest::ResolveYtStreams {
                urls: yt.to_vec(),
                action: YtAction::AddToPlaylist(playlist),
            });
    }
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
fn paste_menu(ctx: &Ctx, items: Vec<PastedItem>) -> MenuModal<'static> {
    let count = items.len();
    let title = if count == 1 {
        format!(" Paste: {} ", items[0].label())
    } else {
        format!(" Paste: {} items ", count)
    };
    let audio: Vec<PastedItem> = items
        .iter()
        .filter(|item| {
            matches!(
                item, PastedItem::File(_) | PastedItem::Url(_) | PastedItem::VideoFile(_)
                | PastedItem::VideoUrl(_) | PastedItem::Yt(_)
            )
        })
        .cloned()
        .collect();
    let video: Vec<PastedItem> = items
        .iter()
        .filter(|item| {
            matches!(
                item, PastedItem::VideoFile(_) | PastedItem::VideoUrl(_) |
                PastedItem::Yt(_)
            )
        })
        .cloned()
        .collect();
    let torrents: Vec<PastedItem> = items
        .iter()
        .filter(|item| matches!(item, PastedItem::Torrent(_) | PastedItem::Magnet(_)))
        .cloned()
        .collect();
    let menu = MenuModal::new(ctx)
        .width(60)
        .title(title)
        .replacement_id(PASTE_MODAL_REPLACEMENT_ID)
        .list_section(
            ctx,
            |mut section| {
                if !audio.is_empty() {
                    section.header("[Audio]");
                    if audio.len() == 1 {
                        let item = audio[0].clone();
                        section = section.item("Play", move |ctx| play_item(ctx, &item));
                    }
                    let all = audio.clone();
                    section = section
                        .item(
                            "Add to queue and play",
                            move |ctx| enqueue_items(ctx, &all, true, true),
                        );
                    let append = audio.clone();
                    section = section
                        .item(
                            "Append to queue",
                            move |ctx| enqueue_items(ctx, &append, false, false),
                        );
                    let (audio_direct, audio_yt) = playlist_audio_uris(&audio);
                    let audio_direct_pick = audio_direct.clone();
                    let audio_yt_pick = audio_yt.clone();
                    section = section
                        .item(
                            "Add to playlist",
                            move |ctx| {
                                let radio_playlist = ctx.config.radio.playlist.clone();
                                let (direct, yt, playlists) = ctx
                                    .query_sync(move |client| {
                                        let playlists = client
                                            .picker_playlists(&radio_playlist)?
                                            .into_iter()
                                            .map(|p| p.name)
                                            .collect::<Vec<_>>();
                                        Ok((
                                            audio_direct_pick.clone(),
                                            audio_yt_pick.clone(),
                                            playlists,
                                        ))
                                    })?;
                                if playlists.is_empty() {
                                    status_warn!("No playlists yet — use 'Create Playlist'");
                                    return Ok(());
                                }
                                modal!(
                                    ctx, SelectModal::builder().ctx(ctx).options(playlists)
                                    .confirm_label("Add").title("Select a playlist")
                                    .on_confirm(move | ctx, selected, _idx | {
                                    add_audio_items_to_playlist(ctx, & direct, & yt, &
                                    selected); Ok(()) }).build()
                                );
                                Ok(())
                            },
                        );
                    let audio_direct_create = audio_direct.clone();
                    let audio_yt_create = audio_yt.clone();
                    section = section
                        .item(
                            "Create Playlist",
                            move |ctx| {
                                modal!(
                                    ctx, InputModal::new(ctx).title("Create playlist")
                                    .confirm_label("Save").input_label("Playlist name:")
                                    .on_confirm(move | ctx, value | { let value = value
                                    .to_owned(); let create_with = audio_direct_create.clone();
                                    let create_name = value.clone(); ctx.command(move | client |
                                    { client.create_playlist(& create_name, create_with) ?;
                                    Ok(()) }); if ! audio_yt_create.is_empty() { let action = if
                                    audio_direct_create.is_empty() {
                                    YtAction::CreatePlaylist(value) } else {
                                    YtAction::AddToPlaylist(value) }; let _ = ctx.work_sender
                                    .send(WorkRequest::ResolveYtStreams { urls : audio_yt_create
                                    .clone(), action, }); } Ok(()) })
                                );
                                Ok(())
                            },
                        );
                }
                if !video.is_empty() {
                    section.header("[Video]");
                    let vids = video.clone();
                    section = section
                        .item(
                            "Play",
                            move |ctx| {
                                play_video_now(ctx, &vids);
                                Ok(())
                            },
                        );
                    let vids = video.clone();
                    section = section
                        .item(
                            "Add to queue and play",
                            move |ctx| {
                                queue_videos(ctx, &vids, true, true);
                                Ok(())
                            },
                        );
                    let vids = video.clone();
                    section = section
                        .item(
                            "Append to queue",
                            move |ctx| {
                                queue_videos(ctx, &vids, false, false);
                                Ok(())
                            },
                        );
                    let vids = video.clone();
                    section = section
                        .item(
                            "Add to playlist",
                            move |ctx| {
                                let uris = video_playlist_uris(&vids);
                                let radio_playlist = ctx.config.radio.playlist.clone();
                                let playlists = ctx
                                    .query_sync(move |client| {
                                        Ok(
                                            client
                                                .picker_playlists(&radio_playlist)?
                                                .into_iter()
                                                .map(|p| p.name)
                                                .collect::<Vec<_>>(),
                                        )
                                    })?;
                                if playlists.is_empty() {
                                    status_warn!("No playlists yet — use 'Create Playlist'");
                                    return Ok(());
                                }
                                modal!(
                                    ctx, SelectModal::builder().ctx(ctx).options(playlists)
                                    .confirm_label("Add").title("Select a playlist")
                                    .on_confirm(move | ctx, selected, _idx | { let uris = uris
                                    .clone(); ctx.command(move | client | { client
                                    .add_to_playlist_multiple(& selected, uris) ?; Ok(()) });
                                    Ok(()) }).build()
                                );
                                Ok(())
                            },
                        );
                    let vids = video.clone();
                    section = section
                        .item(
                            "Create Playlist",
                            move |ctx| {
                                let uris = video_playlist_uris(&vids);
                                modal!(
                                    ctx, InputModal::new(ctx).title("Create playlist")
                                    .confirm_label("Save").input_label("Playlist name:")
                                    .on_confirm(move | ctx, value | { let value = value
                                    .to_owned(); let uris = uris.clone(); ctx.command(move |
                                    client | { client.create_playlist(& value, uris) ?; Ok(())
                                    }); Ok(()) })
                                );
                                Ok(())
                            },
                        );
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
                            match ctx.torrent_scans.borrow().get(&key) {
                                Some(Ok(scan)) => {
                                    let videos = scan.videos();
                                    if videos.is_empty() && scan.audios().is_empty() {
                                        section.header("No playable media in this torrent");
                                    } else if videos.len() > 1 {
                                        let all_item = item.clone();
                                        let all_key = key.clone();
                                        section = section
                                            .item(
                                                "Stream all",
                                                move |ctx| {
                                                    let indices: Vec<usize> = ctx
                                                        .torrent_scans
                                                        .borrow()
                                                        .get(&all_key)
                                                        .and_then(|r| r.as_ref().ok())
                                                        .map(|s| s.videos().iter().map(|f| f.index).collect())
                                                        .unwrap_or_default();
                                                    play_scanned_or_fresh(
                                                        ctx,
                                                        &all_item,
                                                        &all_key,
                                                        indices,
                                                        false,
                                                    )
                                                },
                                            );
                                        let dl_item = item.clone();
                                        let dl_key = key.clone();
                                        section = section
                                            .item(
                                                "Download all",
                                                move |ctx| {
                                                    let indices: Vec<usize> = ctx
                                                        .torrent_scans
                                                        .borrow()
                                                        .get(&dl_key)
                                                        .and_then(|r| r.as_ref().ok())
                                                        .map(|s| s.videos().iter().map(|f| f.index).collect())
                                                        .unwrap_or_default();
                                                    download_scanned_or_fresh(ctx, &dl_item, &dl_key, indices)
                                                },
                                            );
                                        let pick_item = item.clone();
                                        let pick_key = key.clone();
                                        section = section
                                            .item(
                                                "Select files…",
                                                move |ctx| {
                                                    open_torrent_file_picker(ctx, &pick_item, &pick_key)
                                                },
                                            );
                                    } else {
                                        let play_item = item.clone();
                                        let play_key = key.clone();
                                        section = section
                                            .item(
                                                "Stream",
                                                move |ctx| {
                                                    play_scanned_or_fresh(
                                                        ctx,
                                                        &play_item,
                                                        &play_key,
                                                        Vec::new(),
                                                        false,
                                                    )
                                                },
                                            );
                                        let dl_item = item.clone();
                                        let dl_key = key.clone();
                                        section = section
                                            .item(
                                                "Download",
                                                move |ctx| {
                                                    download_scanned_or_fresh(
                                                        ctx,
                                                        &dl_item,
                                                        &dl_key,
                                                        Vec::new(),
                                                    )
                                                },
                                            );
                                    }
                                }
                                Some(Err(err)) => {
                                    section.header(err.clone());
                                }
                                None => {
                                    let progress = ctx
                                        .torrent_scan_progress
                                        .borrow()
                                        .get(&key)
                                        .copied()
                                        .unwrap_or_default();
                                    section
                                        .header(
                                            format!(
                                                "Loading {label}… {}", scan_wait_elapsed(progress
                                                .elapsed_secs)
                                            ),
                                        );
                                    section.header("esc to cancel");
                                    if !ctx.torrent_scans_pending.borrow().contains(&key) {
                                        ctx.torrent_scans_pending.borrow_mut().insert(key.clone());
                                        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
                                        ctx.torrent_scan_cancels
                                            .borrow_mut()
                                            .insert(key.clone(), cancel_tx);
                                        let item = torrent_item(item);
                                        let _ = ctx
                                            .work_sender
                                            .send(WorkRequest::ScanTorrent {
                                                item,
                                                cancel: cancel_rx,
                                            });
                                    }
                                }
                            }
                        }
                    } else {
                        section.header("Torrent streaming disabled");
                    }
                }
                section.add_item("Cancel", |_ctx| Ok(()));
                section
                    .set_on_close(|ctx| {
                        ctx.paste_modal_items.borrow_mut().take();
                        ctx.paste_modal_id.set(None);
                        cancel_in_flight_scans(ctx);
                        ctx.torrent_scans
                            .borrow_mut()
                            .retain(|_, result| result.is_ok());
                    });
                Some(section)
            },
        )
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
        PastedItem::Magnet(magnet) => {
            crate::core::torrent::TorrentItem::Magnet(magnet.clone())
        }
        PastedItem::Torrent(torrent) => {
            crate::core::torrent::TorrentItem::Torrent(torrent.clone())
        }
        _ => unreachable!("torrent items only"),
    }
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
    log::debug!(
        key:?; "on_torrent_scanned ok={} popup_open={}", result.is_ok(), ctx
        .paste_modal_items.borrow().is_some()
    );
    if ctx.paste_modal_items.borrow().is_none() {
        return;
    }
    ctx.torrent_scans.borrow_mut().insert(key, result);
    refresh_paste_modal(ctx);
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
    if ctx.paste_modal_items.borrow().is_none() {
        return;
    }
    ctx.torrent_scan_progress.borrow_mut().insert(key, progress);
    refresh_paste_modal(ctx);
}
/// Cancel every in-flight torrent scan (round 18): signal the scan
/// threads' cancel channels so they stop waiting and drop their engines
/// (killing rqbit) promptly, then forget the scan bookkeeping. Called by
/// the paste popup's close hook (Esc / Cancel / an action) and by the
/// playback-start cleanup paths that drop the popup without its hook.
fn cancel_in_flight_scans(ctx: &Ctx) {
    for key in ctx.torrent_scans_pending.borrow().iter() {
        if let Some(cancel) = ctx.torrent_scan_cancels.borrow().get(key) {
            let _ = cancel.send(());
        }
    }
    ctx.torrent_scan_cancels.borrow_mut().clear();
    ctx.torrent_scan_progress.borrow_mut().clear();
    ctx.torrent_scans_pending.borrow_mut().clear();
}
/// The wait window's elapsed counter ("mm:ss", e.g. `00:12`).
fn scan_wait_elapsed(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}
/// Rebuild the open paste popup (a torrent scan changed its `[Torrent]`
/// section). No-op when no paste popup is open.
pub fn refresh_paste_modal(ctx: &Ctx) {
    let Some(items) = ctx.paste_modal_items.borrow().clone() else { return };
    let menu = paste_menu(ctx, items);
    modal!(ctx, menu);
}
/// Play a scanned torrent's files (round 17): take the scanned engine out
/// of `Ctx.torrent_scans` and hand playback to the event loop
/// (`AppEvent::TorrentScannedPlay` — it owns the download-job guard).
/// `indices` empty = the single best playable file; when the scan is gone
/// (replaced engine, closed popup) the fresh-engine `play_torrent` path
/// takes over.
fn play_scanned_or_fresh(
    ctx: &Ctx,
    item: &PastedItem,
    key: &str,
    indices: Vec<usize>,
    download: bool,
) -> Result<()> {
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    log::debug!(
        key:?; "play_scanned_or_fresh found={} indices={}", scan.is_some(), indices.len()
    );
    let Some(scan) = scan else {
        return play_torrent(ctx, std::slice::from_ref(item), download);
    };
    let file_indices: Vec<usize> = if indices.is_empty() {
        match scan.pick_playable() {
            Some(pick) => vec![pick.index],
            None => return play_torrent(ctx, std::slice::from_ref(item), download),
        }
    } else {
        indices.into_iter().filter(|i| *i < scan.files.len()).collect()
    };
    if file_indices.is_empty() {
        return play_torrent(ctx, std::slice::from_ref(item), download);
    }
    ctx.app_event_sender
        .send(crate::shared::events::AppEvent::TorrentScannedPlay {
            scan,
            file_indices,
            download,
        })
        .map_err(|err| anyhow::anyhow!("Failed to start torrent playback: {err}"))?;
    Ok(())
}
/// "Select files…" (round 17): a multi-select modal over the torrent's
/// video files (name + size); confirming plays the marked files.
///
/// Round-18 host finding (2026-08-09): the picker's Enter used to re-read
/// `Ctx.torrent_scans` via `play_scanned_or_fresh` — but opening the picker
/// is itself a paste-popup action, and the popup's close hook (run right
/// after the action, when `MenuModal::destroy` fires) clears
/// `Ctx.torrent_scans`. By the time the user marked files and pressed
/// Enter the scan was gone, so the play fell back to the fresh single-file
/// path ("select files never plays"). Fix: capture the scan when the
/// picker opens and move it into the picker's confirm closure — the play
/// builds its entries from the captured scan, independent of the popup
/// teardown.
///
/// Round 20: the picker's buttons are **Play** / **Download & Play** /
/// **Cancel** (the second starts the download job too), and the captured
/// scan is a clone — the map keeps its own copy so a repeat paste reuses
/// the engine (the engine is shared via `Arc`).
fn open_torrent_file_picker(ctx: &Ctx, item: &PastedItem, key: &str) -> Result<()> {
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    let Some(scan) = scan else {
        return play_torrent(ctx, std::slice::from_ref(item), false);
    };
    let mut videos: Vec<(usize, String, u64)> = scan
        .videos()
        .into_iter()
        .map(|f| (f.index, f.name.clone(), f.length))
        .collect();
    videos.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    let title = format!("▶ files — {} ", scan.torrent_name);
    let item = item.clone();
    modal!(
        ctx, crate ::ui::modals::torrent_file_picker::TorrentFilePicker::new(ctx, title,
        videos, move | ctx, indices, action | { let file_indices : Vec < usize > =
        indices.into_iter().filter(| i | * i < scan.files.len()).collect(); let download
        = action == crate
        ::ui::modals::torrent_file_picker::TorrentPickerAction::DownloadAndPlay; let
        result = if file_indices.is_empty() { play_torrent(ctx, std::slice::from_ref(&
        item), download) } else { ctx.app_event_sender.send(crate
        ::shared::events::AppEvent::TorrentScannedPlay { scan, file_indices, download, })
        .map_err(| err | anyhow::anyhow!("Failed to start torrent playback: {err}")) };
        if let Some(id) = ctx.paste_modal_id.take() { let _ = ctx.app_event_sender
        .send(crate ::AppEvent::UiEvent(crate ::ui::UiAppEvent::PopModal(id))); } ctx
        .paste_modal_items.borrow_mut().take(); cancel_in_flight_scans(ctx); result },)
    );
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
        yt_info
            .insert(
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
                scan.engine.stream_url(&scan.torrent_id, f.index as u64),
                None,
            )
        })
        .collect()
}
/// Play a single item immediately without adding it to the queue.
fn play_item(ctx: &Ctx, item: &PastedItem) -> Result<()> {
    match item {
        PastedItem::File(path) => {
            let uri = mpd_addable_path(path);
            if uri != *path {
                status_info!("Playing {} (MPD path: {uri})", item.label());
            }
            ctx.query()
                .id(PASTE_PLAY)
                .replace_id(PASTE_PLAY)
                .target(PaneType::Radio {
                    tree: TreeBrowserArgs::default(),
                })
                .query(move |client| {
                    let id = client.add_id(&uri, None)?;
                    client.play_id(id)?;
                    Ok(crate::MpdQueryResult::Any(Box::new(id)))
                });
            Ok(())
        }
        PastedItem::Url(url) => {
            let url = url.clone();
            ctx.query()
                .id(PASTE_PLAY)
                .replace_id(PASTE_PLAY)
                .target(PaneType::Radio {
                    tree: TreeBrowserArgs::default(),
                })
                .query(move |client| {
                    let id = client.add_id(&url, None)?;
                    client.play_id(id)?;
                    Ok(crate::MpdQueryResult::Any(Box::new(id)))
                });
            Ok(())
        }
        PastedItem::VideoFile(path) => {
            let uri = mpd_addable_path(path);
            paste_play_temp(ctx, uri);
            Ok(())
        }
        PastedItem::VideoUrl(url) => {
            paste_play_temp(ctx, url.clone());
            Ok(())
        }
        PastedItem::Yt(url) => yt_play_audio(ctx, url),
        PastedItem::Torrent(_) | PastedItem::Magnet(_) => {
            play_torrent(ctx, std::slice::from_ref(item), false)
        }
    }
}
/// Download a scanned torrent's files without playback (round 21): keep
/// the scanned engine and hand the files to the event loop
/// (`AppEvent::TorrentScannedDownload` — it owns the download-job guard).
/// `indices` empty = the single best playable file ("Download"), non-empty
/// = the exact files to keep ("Download all" passes every video). When the
/// scan is gone (replaced engine, closed popup) the fresh-engine
/// `download_torrent` path takes over.
fn download_scanned_or_fresh(
    ctx: &Ctx,
    item: &PastedItem,
    key: &str,
    indices: Vec<usize>,
) -> Result<()> {
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    log::debug!(
        key:?; "download_scanned_or_fresh found={} indices={}", scan.is_some(), indices
        .len()
    );
    let Some(scan) = scan else {
        return download_torrent(ctx, std::slice::from_ref(item), Vec::new());
    };
    let file_indices: Vec<usize> = if indices.is_empty() {
        match scan.pick_playable() {
            Some(pick) => vec![pick.index],
            None => return download_torrent(ctx, std::slice::from_ref(item), Vec::new()),
        }
    } else {
        indices.into_iter().filter(|i| *i < scan.files.len()).collect()
    };
    if file_indices.is_empty() {
        return download_torrent(ctx, std::slice::from_ref(item), Vec::new());
    }
    ctx.app_event_sender
        .send(crate::shared::events::AppEvent::TorrentScannedDownload {
            scan,
            file_indices,
        })
        .map_err(|err| anyhow::anyhow!("Failed to start torrent download: {err}"))?;
    Ok(())
}
/// Download a pasted torrent/magnet on a fresh engine without playback
/// (round 21, the fallback when the scanned engine is gone — replaced or
/// the popup closed): `WorkRequest::DownloadTorrent` prepares the engine
/// and the UI starts the download job from `TorrentDownloadPrepared`.
fn download_torrent(ctx: &Ctx, items: &[PastedItem], indices: Vec<usize>) -> Result<()> {
    let Some(item) = items.first() else { return Ok(()) };
    let torrent_item = torrent_item(item);
    if !ctx.config.torrent.enabled {
        status_warn!("Torrent streaming is disabled in the config");
        return Ok(());
    }
    ctx.work_sender
        .send(WorkRequest::DownloadTorrent {
            item: torrent_item,
            indices,
        })
        .map_err(|err| anyhow::anyhow!("Failed to request torrent download: {err}"))?;
    status_info!("Starting torrent download…");
    Ok(())
}
/// Stream a pasted torrent/magnet on a fresh engine: start rqbit,
/// add the torrent, pick the largest playable file and hand its stream
/// URL to mpv. The engine work runs on the work thread
/// (`WorkRequest::PlayTorrent`); the prepared stream arrives as
/// `WorkDone::TorrentStreamPrepared`, which keeps the engine alive and
/// launches the mpv session. `download` is the "Play and Download" / picker "Download & Play"
/// action: the engine keeps downloading after the stream starts and the
/// completed file is moved to `s2udio-downloads`.
///
/// Round 17: this is the fallback for the scanned play actions (the
/// scanned engine is gone — replaced or the popup closed); M2 wired the
/// end-to-end path, the bandwidth gate (M3) and `only_files_regex` /
/// cleanup triggers (M4) refine it.
fn play_torrent(ctx: &Ctx, items: &[PastedItem], download: bool) -> Result<()> {
    let Some(item) = items.first() else { return Ok(()) };
    let torrent_item = torrent_item(item);
    if !ctx.config.torrent.enabled {
        status_warn!("Torrent streaming is disabled in the config");
        return Ok(());
    }
    ctx.work_sender
        .send(WorkRequest::PlayTorrent {
            item: torrent_item,
            download,
        })
        .map_err(|err| anyhow::anyhow!("Failed to request torrent stream: {err}"))?;
    if download {
        status_info!("Starting torrent stream + download…");
    } else {
        status_info!("Starting torrent stream…");
    }
    Ok(())
}
/// Play a pasted item's audio through MPD as a temporary entry.
fn paste_play_temp(ctx: &Ctx, url: String) {
    ctx.query()
        .id(PASTE_PLAY)
        .replace_id(PASTE_PLAY)
        .target(PaneType::Radio {
            tree: TreeBrowserArgs::default(),
        })
        .query(move |client| {
            let id = client.add_id(&url, None)?;
            client.play_id(id)?;
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
        })
        .map_err(|err| anyhow::anyhow!("Failed to request stream resolution: {err}"))?;
    status_info!("Resolving YouTube link…");
    Ok(())
}
/// Add all items to the queue: direct files/URLs immediately, YouTube-style
/// links after their streams are resolved on the work thread. With `play`
/// the first inserted item starts playing immediately.
fn enqueue_items(
    ctx: &Ctx,
    items: &[PastedItem],
    after_current: bool,
    play: bool,
) -> Result<()> {
    let has_current = ctx.find_current_song_in_queue().is_some();
    let position = (after_current && has_current)
        .then_some(QueuePosition::RelativeAdd(0));
    let autoplay_idx = play
        .then(|| {
            ctx.find_current_song_in_queue()
                .map(|(idx, _)| idx + 1)
                .unwrap_or_else(|| ctx.queue.len())
        });
    let (direct, yt): (Vec<String>, Vec<String>) = items
        .iter()
        .fold(
            (Vec::new(), Vec::new()),
            |(mut direct, mut yt), item| match item {
                PastedItem::File(path) => {
                    direct.push(mpd_addable_path(path));
                    (direct, yt)
                }
                PastedItem::VideoFile(path) => {
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
        );
    if !direct.is_empty() {
        let enqueue: Vec<Enqueue> = direct
            .iter()
            .cloned()
            .map(|path| Enqueue::File { path })
            .collect();
        ctx.command(move |client| {
            client.enqueue_multiple(enqueue, autoplay_idx, position, false)?;
            Ok(())
        });
    }
    if !yt.is_empty() {
        let action = if play {
            YtAction::AddAfterCurrentAndPlay
        } else {
            YtAction::Append
        };
        let count = yt.len();
        let _ = ctx
            .work_sender
            .send(WorkRequest::ResolveYtStreams {
                urls: yt,
                action,
            })
            .map_err(|err| {
                anyhow::anyhow!("Failed to request stream resolution: {err}")
            })?;
        status_info!("Resolving YouTube link{}…", if count == 1 { "" } else { "s" });
    }
    Ok(())
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
    {
        let mut yt_info = ctx.yt_info.borrow_mut();
        let mut chapters = ctx.chapters.borrow_mut();
        for item in &info {
            if !item.title.is_empty() {
                yt_info.insert(item.url.clone(), item.clone());
                if !item.original_url.is_empty() && item.original_url != item.url {
                    yt_info.insert(item.original_url.clone(), item.clone());
                }
            }
            if !item.chapters.is_empty() {
                chapters.insert(item.url.clone(), item.chapters.clone());
                if !item.original_url.is_empty() && item.original_url != item.url {
                    chapters.insert(item.original_url.clone(), item.chapters.clone());
                }
            }
        }
    }
    let cache_dir = ctx.config.cache_dir.as_deref();
    let mut cache = load_yt_cache(cache_dir);
    for item in &info {
        if !item.title.is_empty() {
            cache.insert(item.url.clone(), item.clone());
            if !item.original_url.is_empty() && item.original_url != item.url {
                cache.insert(item.original_url.clone(), item.clone());
            }
        }
    }
    save_yt_cache(cache_dir, &cache);
    let urls: Vec<String> = info.iter().map(|i| i.url.clone()).collect();
    let count = urls.len();
    match action {
        YtAction::Play => {
            let url = urls[0].clone();
            ctx.query()
                .id(PASTE_PLAY)
                .replace_id(PASTE_PLAY)
                .target(PaneType::Radio {
                    tree: TreeBrowserArgs::default(),
                })
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
                .map(|item| {
                    let url = if item.original_url.is_empty() {
                        item.url.clone()
                    } else {
                        item.original_url.clone()
                    };
                    let mut entry = MpvPlaylistEntry::new(item.title.clone(), url, None);
                    entry.original_url = (!item.original_url.is_empty())
                        .then(|| item.original_url.clone());
                    entry
                })
                .collect();
            for failure in failures {
                if let Some(url) = failure
                    .split_once(": ")
                    .map(|(url, _)| url.to_owned())
                {
                    entries.push(MpvPlaylistEntry::new(url.clone(), url, None));
                }
            }
            crate::core::mpv::play_video_entries(ctx, entries);
        }
        YtAction::AddAfterCurrentAndPlay => {
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
            ctx.command(move |client| {
                for url in &urls {
                    client.add(url, None)?;
                }
                Ok(())
            });
            status_info!("Appended {count} item(s) to the queue");
        }
        YtAction::ReplaceAndPlay(song_id) => {
            let url = urls[0].clone();
            ctx.command(move |client| {
                let position = client
                    .playlist_info()?
                    .and_then(|songs| songs.iter().position(|song| song.id == song_id));
                let _ = client.delete_id(song_id);
                let new_id = client.add_id(&url, position.map(QueuePosition::Absolute))?;
                client.play_id(new_id)?;
                Ok(())
            });
            status_info!("Stream URL expired — re-resolved from the original link");
        }
        YtAction::AddToPlaylist(playlist) => {
            let playlist_add = playlist.clone();
            let urls_add = urls.clone();
            ctx.command(move |client| {
                client.add_to_playlist_multiple(&playlist_add, urls_add)?;
                Ok(())
            });
            status_info!("Added {count} item(s) to playlist '{playlist}'");
        }
        YtAction::CreatePlaylist(playlist) => {
            let playlist_create = playlist.clone();
            let urls_create = urls.clone();
            ctx.command(move |client| {
                client.create_playlist(&playlist_create, urls_create)?;
                Ok(())
            });
            status_info!("Created playlist '{playlist}' with {count} item(s)");
        }
        YtAction::AddToVideoQueue
        | YtAction::AppendVideoQueue
        | YtAction::AddToVideoQueueAndPlay => {
            let after_current = !matches!(action, YtAction::AppendVideoQueue);
            use crate::core::mpv::MpvPlaylistEntry;
            let mut entries: Vec<MpvPlaylistEntry> = info
                .iter()
                .map(|item| {
                    let url = if item.original_url.is_empty() {
                        item.url.clone()
                    } else {
                        item.original_url.clone()
                    };
                    let mut entry = MpvPlaylistEntry::new(item.title.clone(), url, None);
                    entry.original_url = (!item.original_url.is_empty())
                        .then(|| item.original_url.clone());
                    entry
                })
                .collect();
            for failure in &failures {
                if let Some(url) = failure
                    .split_once(": ")
                    .map(|(url, _)| url.to_owned())
                {
                    entries.push(MpvPlaylistEntry::new(url.clone(), url, None));
                }
            }
            if matches!(action, YtAction::AddToVideoQueueAndPlay) {
                crate::core::mpv::add_to_video_playlist(
                    ctx,
                    entries.clone(),
                    after_current,
                );
                crate::core::mpv::play_video_entries(ctx, entries);
                status_info!(
                    "Added {count} item(s) to the video queue and started playback"
                );
            } else {
                crate::core::mpv::add_to_video_playlist(ctx, entries, after_current);
                status_info!(
                    "{} {count} item(s) to the video queue", if after_current { "Added" }
                    else { "Appended" }
                );
            }
        }
        YtAction::Refresh => {}
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
/// Make sure the chapters of the current song are known: sync from the
/// resolved YouTube info, or fetch them (Jellyfin items via the API, local
/// files via ffprobe). Called on song change / startup.
pub fn ensure_chapters(ctx: &Ctx) {
    let Some((_, song)) = ctx.find_current_song_in_queue() else { return };
    if ctx.chapters.borrow().contains_key(&song.file) {
        return;
    }
    if let Some(yt) = ctx.yt_info.borrow().get(&song.file) && !yt.chapters.is_empty() {
        ctx.chapters.borrow_mut().insert(song.file.clone(), yt.chapters.clone());
        return;
    }
    if let Some(item_id) = crate::jellyfin::item_id_from_url(&song.file) {
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinChapters {
                item_id,
            })
            .map_err(|err| {
                log::error!(error:? = err; "Failed to request jellyfin chapters")
            });
        return;
    }
    if !crate::ui::panes::radio::is_stream_url(&song.file) {
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchFileChapters {
                file: song.file.clone(),
            })
            .map_err(|err| {
                log::error!(error:? = err; "Failed to request file chapters")
            });
    }
}
/// Round 19: s2udio-only cache files (video playlist, mpv MPRIS state,
/// MPRIS art) live in `~/.cache/s2udio/` by default — separate from rmpc's
/// cache so stream/video playlists never collide with rmpc/MPD state. An
/// explicit `cache_dir` in the config still wins; when no cache dir is
/// configured and the legacy `~/.cache/rmpc/…` file exists, that path is
/// returned (migration) so pre-round-19 state keeps loading.
fn s2udio_cache_path(
    cache_dir: Option<&std::path::Path>,
    file: &str,
) -> std::path::PathBuf {
    if let Some(dir) = cache_dir {
        return dir.join(file);
    }
    let new = crate::shared::paths::s2udio_cache_dir()
        .unwrap_or_else(|| {
            crate::config::utils::tilde_expand("~/.cache/s2udio").into_owned().into()
        })
        .join(file);
    let legacy: std::path::PathBuf = crate::config::utils::tilde_expand("~/.cache/rmpc")
        .into_owned()
        .into();
    if new.exists() {
        new
    } else if legacy.join(file).exists() {
        legacy.join(file)
    } else {
        new
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
    let legacy: std::path::PathBuf = crate::config::utils::tilde_expand("~/.cache/rmpc")
        .into_owned()
        .into();
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
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let socket = ctx
        .mpv
        .socket
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
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
        ctx.mpv
            .playlist
            .borrow()
            .get(ctx.mpv.playlist_pos.get().unwrap_or(0))
            .and_then(|e| e.duration)
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
    info.get(&url)
        .cloned()
        .or_else(|| info.values().find(|e| e.original_url == url).cloned())
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
    let info = ctx.yt_info.borrow();
    info.get(&song.file)
        .cloned()
        .or_else(|| info.values().find(|e| e.original_url == song.file).cloned())
}
/// Queue a `s2udio-downloads` download of a ytdlp stream (yt-dlp needs the
/// original link, not the resolved stream URL). `audio_only` extracts the
/// audio (`-x`), `split_chapters` saves each chapter as its own file named
/// after the chapter title; `replace` is what the downloaded file(s)
/// should take the place of once complete.
pub fn queue_stream_download(
    ctx: &Ctx,
    original_url: &str,
    audio_only: bool,
    split_chapters: bool,
    replace: crate::shared::ytdlp::ReplaceAction,
) {
    use crate::shared::ytdlp::StreamDownloadSpec;
    let Some(output_dir) = downloads_dir() else {
        status_warn!("Cannot determine the downloads folder (~/Downloads)");
        return;
    };
    let parsed: Result<crate::shared::ytdlp::YtDlpContent, _> = original_url.parse();
    let Ok(crate::shared::ytdlp::YtDlpContent::Single(item)) = parsed else {
        status_warn!("Cannot download: not a YouTube/Soundcloud/NicoVideo link");
        return;
    };
    let spec = StreamDownloadSpec {
        output_dir,
        audio_only,
        split_chapters,
        on_complete: replace,
    };
    ctx.ytdlp_manager.queue_stream_download(item, spec);
    status_info!("Downloading '{}' to s2udio-downloads", original_url);
}
/// The save-as menu for a ytdlp stream: audio or video, and — when the
/// media has chapters — one file with chapters or each chapter as its own
/// file. `replace` is what the downloaded file(s) replace in the
/// queue/playlist (the controls' Download button passes
/// `ReplaceAction::None`).
pub fn open_stream_download_menu(
    ctx: &Ctx,
    info: &crate::shared::ytdlp::YtStreamInfo,
    replace: &crate::shared::ytdlp::ReplaceAction,
) {
    let has_chapters = info.chapters.len() > 1;
    let original = info.original_url.clone();
    let menu = MenuModal::new(ctx)
        .width(46)
        .title(" Download ")
        .list_section(
            ctx,
            |mut section| {
                section = section
                    .item(
                        "Save as audio",
                        {
                            let original = original.clone();
                            let replace = replace.clone();
                            move |ctx| {
                                queue_stream_download(ctx, &original, true, false, replace);
                                Ok(())
                            }
                        },
                    );
                section = section
                    .item(
                        "Save as video",
                        {
                            let original = original.clone();
                            let replace = replace.clone();
                            move |ctx| {
                                queue_stream_download(
                                    ctx,
                                    &original,
                                    false,
                                    false,
                                    replace,
                                );
                                Ok(())
                            }
                        },
                    );
                if has_chapters {
                    section = section
                        .item(
                            "Audio — each chapter its own file",
                            {
                                let original = original.clone();
                                let replace = replace.clone();
                                move |ctx| {
                                    queue_stream_download(ctx, &original, true, true, replace);
                                    Ok(())
                                }
                            },
                        );
                    section = section
                        .item(
                            "Video — each chapter its own file",
                            {
                                let original = original.clone();
                                let replace = replace.clone();
                                move |ctx| {
                                    queue_stream_download(ctx, &original, false, true, replace);
                                    Ok(())
                                }
                            },
                        );
                }
                Some(section)
            },
        )
        .list_section(ctx, |section| Some(section.item("Cancel", |_ctx| Ok(()))))
        .build();
    modal!(ctx, menu);
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
                let _ = client.add_tag_id(song_id, "title", &title);
                let _ = client.add_tag_id(song_id, "album", &title);
            }
            if let Some(channel) = channel && !channel.is_empty() {
                let _ = client.add_tag_id(song_id, "artist", &channel);
            }
            Ok(())
        });
        if let Some(thumb) = thumb {
            let _ = ctx
                .work_sender
                .send(WorkRequest::SaveMprisArt {
                    url: thumb,
                })
                .map_err(|err| {
                    log::error!(error:? = err; "Failed to request MPRIS art")
                });
        }
        return;
    }
    if let Some(item_id) = crate::jellyfin::item_id_from_url(&song.file) {
        crate::core::work::set_expected_mpris_art(Some(format!("jellyfin:{item_id}")));
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinMpris {
                item_id,
            })
            .map_err(|err| {
                log::error!(error:? = err; "Failed to request jellyfin MPRIS info")
            });
        return;
    }
    crate::core::work::set_expected_mpris_art(None);
    let _ = std::fs::remove_file(mpris_art_path(ctx.config.cache_dir.as_deref()));
}
/// The full popup flow from a paste event: parse, and when something
/// recognized was found, show the popup. Returns true when a popup was
/// opened.
pub fn handle_paste(ctx: &Ctx, text: &str) -> bool {
    let items = parse_paste(text);
    if items.is_empty() {
        return false;
    }
    show_paste_modal(ctx, items);
    true
}

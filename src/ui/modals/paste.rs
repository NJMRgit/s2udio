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
    config::{tabs::{PaneType, TreeBrowserArgs}, utils::tilde_expand},
    ctx::Ctx,
    mpd::{QueuePosition, mpd_client::MpdClient},
    shared::{
        events::WorkRequest,
        macros::{modal, status_info, status_warn},
        mpd_client_ext::{Enqueue, MpdClientExt as _},
        ytdlp::YtDlpContent,
    },
    ui::modals::{input_modal::InputModal, menu::modal::MenuModal, select_modal::SelectModal},
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
    /// Insert after the currently/last played track.
    AddAfterCurrent,
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
            // Raw magnet URIs are noisy: show the infohash prefix instead.
            Self::Magnet(magnet) => magnet_infohash(magnet)
                .map_or_else(|| "magnet link".to_owned(), |hash| format!("magnet:{hash}")),
        }
    }
}

/// File extensions considered audio (lowercase, no dot).
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "oga", "m4a", "m4b", "aac", "wav", "wma", "ape", "alac",
    "aiff", "aif", "wv", "mka", "spx", "ac3",
];

/// File extensions considered video (lowercase, no dot).
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "mpg", "mpeg", "ts", "m2ts", "mts",
    "flv", "wmv", "vob", "ogv", "3gp", "divx",
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
    query.split('&').find_map(|param| {
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
            // Keep the escape pair together; unescaped later.
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
    // Magnets dedupe by infohash: two magnets for the same torrent that
    // only differ in trackers are the same content (the token-level dedupe
    // below would keep both).
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
    // Magnet links (`magnet:?xt=…`): checked before the generic http(s)
    // branch — a magnet is its own source type, never a fetchable URL.
    if token.starts_with("magnet:") {
        return Some(PastedItem::Magnet(token.to_owned()));
    }

    // file:// URIs (e.g. from some file managers / browsers).
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

    // YouTube / Soundcloud / NicoVideo links: resolved via yt-dlp.
    if let Ok(_content) = token.parse::<YtDlpContent>() {
        return Some(PastedItem::Yt(token.to_owned()));
    }

    // Direct http(s) URLs: a `.torrent` URL, then audio/video by
    // extension.
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

    // Local paths (possibly kitty-escaped).
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
                if let Some(value) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
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
    if rel.is_empty() {
        path.to_owned()
    } else {
        rel.to_owned()
    }
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
            PastedItem::File(path) | PastedItem::VideoFile(path) => MpvPlaylistEntry::new(
                path.rsplit('/').next().unwrap_or(path).to_owned(),
                path.clone(),
                None,
            ),
            PastedItem::Url(url) | PastedItem::VideoUrl(url) | PastedItem::Yt(url) => {
                MpvPlaylistEntry::new(url.clone(), url.clone(), None)
            }
            PastedItem::Torrent(_) | PastedItem::Magnet(_) => unreachable!(
                "torrents are streamed through the [Torrent] section, never the video list"
            ),
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
        let _ = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
            urls: yt_urls(vids),
            action: YtAction::PlayVideo,
        });
        return;
    }
    // A video already playing in mpv switches the running instance to
    // these entries instead of a second mpv.
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
        let _ = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
            urls: yt_urls(vids),
            action,
        });
        return;
    }
    let entries = video_entries_for(vids);
    crate::core::mpv::add_to_video_playlist(ctx, entries.clone(), after_current);
    if play {
        // A video already playing in mpv switches the running instance to
        // these entries instead of a second mpv.
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
            PastedItem::Url(url) | PastedItem::VideoUrl(url) | PastedItem::Yt(url) => url.clone(),
            // Defensive: torrents are streamed, never stored in playlists.
            PastedItem::Torrent(_) | PastedItem::Magnet(_) => unreachable!(
                "torrents are streamed through the [Torrent] section, never stored in playlists"
            ),
        })
        .collect()
}

/// Split pasted items into MPD-addable audio URIs (direct files/URLs) and
/// YouTube-style links (they need resolving before their streams can be
/// added to a stored playlist). Torrents never reach this splitter (they
/// are routed through the `[Torrent]` section); the arms stay defensive.
fn playlist_audio_uris(items: &[PastedItem]) -> (Vec<String>, Vec<String>) {
    items.iter().fold((Vec::new(), Vec::new()), |(mut direct, mut yt), item| match item {
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
    })
}

/// Add audio items to an existing playlist: direct URIs immediately,
/// YouTube-style links after their streams resolve (the work request
/// carries the playlist name so the result handler can add them).
fn add_audio_items_to_playlist(ctx: &Ctx, direct: &[String], yt: &[String], playlist: &str) {
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
        let _ = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
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
    // Remember the popup's items: a completed torrent scan refreshes the
    // popup (`refresh_paste_modal`) with the same sections + scan results.
    *ctx.paste_modal_items.borrow_mut() = Some(items.clone());
    let menu = paste_menu(ctx, items);
    // Remember the popup's modal id so a nested flow (the "Select
    // files…" picker) can close it once playback starts.
    ctx.paste_modal_id
        .set(Some(crate::ui::modals::Modal::id(&menu)));
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

    // Partition the items into the popup's sections. Everything
    // audio-capable (local files, direct URLs, video files/URLs and
    // YouTube-style links) goes to [Audio] — MPD plays video files' audio
    // tracks, YouTube-style links resolve to a stream. Video items
    // additionally get the [Video] actions (played via mpv). Torrents and
    // magnets get their own [Torrent] section and never fall into
    // [Audio]/[Video] as a fetchable URL.
    let audio: Vec<PastedItem> = items
        .iter()
        .filter(|item| {
            matches!(
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
            matches!(
                item,
                PastedItem::VideoFile(_) | PastedItem::VideoUrl(_) | PastedItem::Yt(_)
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
        .list_section(ctx, |mut section| {
            if !audio.is_empty() {
                section.header("[Audio]");
                // "Play" is offered for a single audio item only.
                if audio.len() == 1 {
                    let item = audio[0].clone();
                    section = section.item("Play", move |ctx| play_item(ctx, &item));
                }
                // Add to queue and play: insert after the current track and
                // start playing the first inserted item immediately.
                let all = audio.clone();
                section = section.item("Add to queue and play", move |ctx| {
                    enqueue_items(ctx, &all, true, true)
                });
                let append = audio.clone();
                section = section.item("Append to queue", move |ctx| {
                    enqueue_items(ctx, &append, false, false)
                });
                // Add to playlist / Create Playlist: the audio URIs of the
                // pasted items (local files relativized to the music
                // directory, YouTube-style links resolved first).
                let (audio_direct, audio_yt) = playlist_audio_uris(&audio);
                let audio_direct_pick = audio_direct.clone();
                let audio_yt_pick = audio_yt.clone();
                section = section.item("Add to playlist", move |ctx| {
                    let radio_playlist = ctx.config.radio.playlist.clone();
                    let (direct, yt, playlists) = ctx.query_sync(move |client| {
                        let playlists = client
                            .picker_playlists(&radio_playlist)?
                            .into_iter()
                            .map(|p| p.name)
                            .collect::<Vec<_>>();
                        Ok((audio_direct_pick.clone(), audio_yt_pick.clone(), playlists))
                    })?;
                    if playlists.is_empty() {
                        status_warn!("No playlists yet — use 'Create Playlist'");
                        return Ok(());
                    }
                    modal!(
                        ctx,
                        SelectModal::builder()
                            .ctx(ctx)
                            .options(playlists)
                            .confirm_label("Add")
                            .title("Select a playlist")
                            .on_confirm(move |ctx, selected, _idx| {
                                add_audio_items_to_playlist(ctx, &direct, &yt, &selected);
                                Ok(())
                            })
                            .build()
                    );
                    Ok(())
                });
                let audio_direct_create = audio_direct.clone();
                let audio_yt_create = audio_yt.clone();
                section = section.item("Create Playlist", move |ctx| {
                    modal!(
                        ctx,
                        InputModal::new(ctx)
                            .title("Create playlist")
                            .confirm_label("Save")
                            .input_label("Playlist name:")
                            .on_confirm(move |ctx, value| {
                                let value = value.to_owned();
                                // Direct items create the playlist now;
                                // YouTube links resolve first and are then
                                // added (or create it when there is nothing
                                // direct).
                                let create_with = audio_direct_create.clone();
                                let create_name = value.clone();
                                ctx.command(move |client| {
                                    client.create_playlist(&create_name, create_with)?;
                                    Ok(())
                                });
                                if !audio_yt_create.is_empty() {
                                    let action = if audio_direct_create.is_empty() {
                                        YtAction::CreatePlaylist(value)
                                    } else {
                                        YtAction::AddToPlaylist(value)
                                    };
                                    let _ = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
                                        urls: audio_yt_create.clone(),
                                        action,
                                    });
                                }
                                Ok(())
                            })
                    );
                    Ok(())
                });
            }
            if !video.is_empty() {
                section.header("[Video]");
                // Play: resolve YouTube-style links, then play via mpv
                // without touching the video playlist.
                let vids = video.clone();
                section = section.item("Play", move |ctx| {
                    play_video_now(ctx, &vids);
                    Ok(())
                });
                // Add to queue and play: insert into the persistent video
                // playlist right after the currently playing entry and
                // start playing the added videos immediately.
                let vids = video.clone();
                section = section.item("Add to queue and play", move |ctx| {
                    queue_videos(ctx, &vids, true, true);
                    Ok(())
                });
                // Append to queue: add at the end of the video playlist.
                let vids = video.clone();
                section = section.item("Append to queue", move |ctx| {
                    queue_videos(ctx, &vids, false, false);
                    Ok(())
                });
                // Add to playlist / Create Playlist: the stored-playlist
                // URIs (paths / stable links).
                let vids = video.clone();
                section = section.item("Add to playlist", move |ctx| {
                    let uris = video_playlist_uris(&vids);
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
                        ctx,
                        SelectModal::builder()
                            .ctx(ctx)
                            .options(playlists)
                            .confirm_label("Add")
                            .title("Select a playlist")
                            .on_confirm(move |ctx, selected, _idx| {
                                let uris = uris.clone();
                                ctx.command(move |client| {
                                    client.add_to_playlist_multiple(&selected, uris)?;
                                    Ok(())
                                });
                                Ok(())
                            })
                            .build()
                    );
                    Ok(())
                });
                let vids = video.clone();
                section = section.item("Create Playlist", move |ctx| {
                    let uris = video_playlist_uris(&vids);
                    modal!(
                        ctx,
                        InputModal::new(ctx)
                            .title("Create playlist")
                            .confirm_label("Save")
                            .input_label("Playlist name:")
                            .on_confirm(move |ctx, value| {
                                let value = value.to_owned();
                                let uris = uris.clone();
                                ctx.command(move |client| {
                                    client.create_playlist(&value, uris)?;
                                    Ok(())
                                });
                                Ok(())
                            })
                    );
                    Ok(())
                });
            }
            if !torrents.is_empty() {
                section.header("[Torrent]");
                if ctx.config.torrent.enabled {
                    // Round 17: the section is driven by the up-front scans.
                    // Each pasted torrent shows a dim "Loading <label>…" row
                    // until its scan lands, then the play actions its file
                    // list enables (single-file vs multi-video season pack).
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
                                    // A data torrent: nothing to play.
                                    section.header("No playable media in this torrent");
                                } else if videos.len() > 1 {
                                    // Multi-video (season packs): stream
                                    // the whole season, download it, or
                                    // hand-pick the files (round 21 labels:
                                    // Stream all / Download all / Select
                                    // files… — the plain single-file
                                    // action is gone from the multi popup).
                                    let all_item = item.clone();
                                    let all_key = key.clone();
                                    section = section.item("Stream all", move |ctx| {
                                        let indices: Vec<usize> = ctx
                                            .torrent_scans
                                            .borrow()
                                            .get(&all_key)
                                            .and_then(|r| r.as_ref().ok())
                                            .map(|s| {
                                                s.videos().iter().map(|f| f.index).collect()
                                            })
                                            .unwrap_or_default();
                                        play_scanned_or_fresh(
                                            ctx, &all_item, &all_key, indices, false,
                                        )
                                    });
                                    let dl_item = item.clone();
                                    let dl_key = key.clone();
                                    section = section.item("Download all", move |ctx| {
                                        let indices: Vec<usize> = ctx
                                            .torrent_scans
                                            .borrow()
                                            .get(&dl_key)
                                            .and_then(|r| r.as_ref().ok())
                                            .map(|s| {
                                                s.videos().iter().map(|f| f.index).collect()
                                            })
                                            .unwrap_or_default();
                                        download_scanned_or_fresh(ctx, &dl_item, &dl_key, indices)
                                    });
                                    let pick_item = item.clone();
                                    let pick_key = key.clone();
                                    section = section.item("Select files…", move |ctx| {
                                        open_torrent_file_picker(ctx, &pick_item, &pick_key)
                                    });
                                } else {
                                    // One playable file (or an audio-only
                                    // torrent): Stream (play now) /
                                    // Download (keep the file in
                                    // s2udio-downloads, no playback) —
                                    // round 21 labels; the scanned engine
                                    // is reused instead of a fresh rqbit.
                                    let play_item = item.clone();
                                    let play_key = key.clone();
                                    section = section.item("Stream", move |ctx| {
                                        play_scanned_or_fresh(
                                            ctx, &play_item, &play_key, Vec::new(), false,
                                        )
                                    });
                                    let dl_item = item.clone();
                                    let dl_key = key.clone();
                                    section = section.item("Download", move |ctx| {
                                        download_scanned_or_fresh(
                                            ctx, &dl_item, &dl_key, Vec::new(),
                                        )
                                    });
                                }
                            }
                            Some(Err(err)) => {
                                // Scan failure (dead magnet, missing engine
                                // binary, …): a dim notice, no actions.
                                section.header(err.clone());
                            }
                            None => {
                                // Round 18: an open-ended, user-cancellable
                                // metainfo wait. The wait block shows the
                                // elapsed counter and the esc-to-cancel
                                // hint — refreshed by the scan thread's
                                // `TorrentScanProgress` events. (Round 20:
                                // the DL-speed / needed-speed ✓✗ row was
                                // dropped — live use showed it is noise
                                // during the metainfo wait, where the speed
                                // is ~0 so it always reads ✗. The
                                // `download_speed_kbps` value itself stays:
                                // the M3 bandwidth gate still reads it.)
                                let progress = ctx
                                    .torrent_scan_progress
                                    .borrow()
                                    .get(&key)
                                    .copied()
                                    .unwrap_or_default();
                                section.header(format!(
                                    "Loading {label}… {}",
                                    scan_wait_elapsed(progress.elapsed_secs)
                                ));
                                section.header("esc to cancel");
                                // Scan in the background — once per item
                                // (the rebuilt popup skips items whose scan
                                // is in flight or has landed). The scan's
                                // cancel channel is registered in Ctx so the
                                // popup's close hook can abort the wait.
                                if !ctx.torrent_scans_pending.borrow().contains(&key) {
                                    ctx.torrent_scans_pending.borrow_mut().insert(key.clone());
                                    let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
                                    ctx.torrent_scan_cancels
                                        .borrow_mut()
                                        .insert(key.clone(), cancel_tx);
                                    let item = torrent_item(item);
                                    let _ = ctx.work_sender.send(WorkRequest::ScanTorrent {
                                        item,
                                        cancel: cancel_rx,
                                    });
                                }
                            }
                        }
                    }
                } else {
                    // Torrents are still classified, but streaming is
                    // switched off in the config: a dim, non-selectable
                    // status row explains why there is no action.
                    section.header("Torrent streaming disabled");
                }
            }
            section.add_item("Cancel", |_ctx| Ok(()));
            // Closing the popup (Cancel, an action, Esc) drops the scanned
            // torrent engines (their `Drop` kills the rqbit children),
            // forgets the popup's items — so a late scan result can never
            // re-open it — and lets in-flight scans land in the void.
            section.set_on_close(|ctx| {
                ctx.paste_modal_items.borrow_mut().take();
                ctx.paste_modal_id.set(None);
                // Round 18: abort every in-flight scan so its thread stops
                // waiting and drops the engine (kills rqbit) promptly.
                cancel_in_flight_scans(ctx);
                // Round 20: keep the *landed* scans (their engines stay
                // alive, so a repeat paste of the same torrent reuses the
                // engine instead of spawning a second rqbit against the
                // same cache dir — the duplicate-paste fix); drop failed
                // scans so a re-paste retries cleanly.
                ctx.torrent_scans.borrow_mut().retain(|_, result| result.is_ok());
            });
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
    log::debug!(key:?; "on_torrent_scanned ok={} popup_open={}", result.is_ok(), ctx.paste_modal_items.borrow().is_some());
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
    // Round 20: the scan stays in `Ctx.torrent_scans` (the event carries a
    // clone; the engine is shared via `Arc`), so a repeat paste of the
    // same torrent reuses the engine instead of spawning a second rqbit
    // against the same cache dir. The fresh-engine fallback only fires
    // when the scan is genuinely gone (never scanned / failed).
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    log::debug!(key:?; "play_scanned_or_fresh found={} indices={}", scan.is_some(), indices.len());
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
    // Round 18: the picker captures the scan when it opens (the paste
    // popup's close hook runs right after this action). Round 20: the
    // scan is CLONED, not removed — the map keeps it so a repeat paste
    // reuses the engine (the engine itself is shared via `Arc`), while
    // the closure still owns a copy independent of the popup teardown.
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
    // Round 22: the picker title starts with "▶" — "Select" was dropped
    // so long torrent names truncate cleanly ("▶…") instead of leaving a
    // stray "t" (the centered title used to overflow and get cut from
    // the left). "▶ files — " is 10 chars and fits the title window.
    let title = format!("▶ files — {} ", scan.torrent_name);
    let item = item.clone();
    modal!(
        ctx,
        crate::ui::modals::torrent_file_picker::TorrentFilePicker::new(
            ctx,
            title,
            videos,
            move |ctx, indices, action| {
                // The marked files from the captured scan: hand playback
                // to the event loop (`TorrentScannedPlay`) like the
                // popup's other play actions; `Download & Play` sets the
                // download flag so the engine keeps the picked files and
                // the completed one is moved to `s2udio-downloads`.
                let file_indices: Vec<usize> = indices
                    .into_iter()
                    .filter(|i| *i < scan.files.len())
                    .collect();
                let download = action
                    == crate::ui::modals::torrent_file_picker::TorrentPickerAction::DownloadAndPlay;
                let result = if file_indices.is_empty() {
                    play_torrent(ctx, std::slice::from_ref(&item), download)
                } else {
                    ctx.app_event_sender
                        .send(crate::shared::events::AppEvent::TorrentScannedPlay {
                            scan,
                            file_indices,
                            download,
                        })
                        .map_err(|err| anyhow::anyhow!("Failed to start torrent playback: {err}"))
                };
                // Playback started: close the paste popup beneath the
                // picker too. `PopModal` drops it without running the
                // section's close hook, so cancel the other items'
                // in-flight scans here (the played torrent's own scan
                // stays in the map for reuse — round 20).
                if let Some(id) = ctx.paste_modal_id.take() {
                    let _ = ctx.app_event_sender.send(crate::AppEvent::UiEvent(
                        crate::ui::UiAppEvent::PopModal(id),
                    ));
                }
                ctx.paste_modal_items.borrow_mut().take();
                cancel_in_flight_scans(ctx);
                result
            },
        )
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
            // addid+playid of a temporary entry, cleaned up on song change
            // by the Radio pane (which receives the result).
            ctx.query().id(PASTE_PLAY).replace_id(PASTE_PLAY).target(PaneType::Radio { tree: TreeBrowserArgs::default() }).query(
                move |client| {
                    let id = client.add_id(&uri, None)?;
                    client.play_id(id)?;
                    Ok(crate::MpdQueryResult::Any(Box::new(id)))
                },
            );
            Ok(())
        }
        PastedItem::Url(url) => {
            let url = url.clone();
            ctx.query().id(PASTE_PLAY).replace_id(PASTE_PLAY).target(PaneType::Radio { tree: TreeBrowserArgs::default() }).query(
                move |client| {
                    let id = client.add_id(&url, None)?;
                    client.play_id(id)?;
                    Ok(crate::MpdQueryResult::Any(Box::new(id)))
                },
            );
            Ok(())
        }
        // Video items play their audio track through MPD as a temporary
        // entry (this is the [Audio] section's "Play" choice).
        PastedItem::VideoFile(path) => {
            let uri = mpd_addable_path(path);
            paste_play_temp(ctx, uri);
            Ok(())
        }
        PastedItem::VideoUrl(url) => {
            paste_play_temp(ctx, url.clone());
            Ok(())
        }
        // YouTube-style links: resolve to a stream and play it through MPD
        // as a temporary entry.
        PastedItem::Yt(url) => yt_play_audio(ctx, url),
        // Defensive: the [Audio] "Play" is only offered for audio items,
        // but a torrent here streams through the [Torrent] action anyway.
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
    // Round 20: the scan stays in `Ctx.torrent_scans` (the event carries a
    // clone; the engine is shared via `Arc`), so a repeat paste of the
    // same torrent reuses the engine instead of spawning a second rqbit
    // against the same cache dir. The fresh-engine fallback only fires
    // when the scan is genuinely gone (never scanned / failed).
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    log::debug!(
        key:?;
        "download_scanned_or_fresh found={} indices={}",
        scan.is_some(),
        indices.len()
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
        .send(WorkRequest::DownloadTorrent { item: torrent_item, indices })
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
        .send(WorkRequest::PlayTorrent { item: torrent_item, download })
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
    ctx.query().id(PASTE_PLAY).replace_id(PASTE_PLAY).target(PaneType::Radio { tree: TreeBrowserArgs::default() }).query(move |client| {
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
fn enqueue_items(ctx: &Ctx, items: &[PastedItem], after_current: bool, play: bool) -> Result<()> {
    // "After the current track" needs a current track; with nothing playing
    // MPD rejects the relative position, so fall back to appending.
    let has_current = ctx.find_current_song_in_queue().is_some();
    let position =
        (after_current && has_current).then_some(QueuePosition::RelativeAdd(0));
    // The queue index the first inserted item lands at (played when `play`):
    // one past the current track, or the end when nothing plays.
    let autoplay_idx = play.then(|| {
        ctx.find_current_song_in_queue()
            .map(|(idx, _)| idx + 1)
            .unwrap_or_else(|| ctx.queue.len())
    });
    let (direct, yt): (Vec<String>, Vec<String>) = items.iter().fold(
        (Vec::new(), Vec::new()),
        |(mut direct, mut yt), item| match item {
            PastedItem::File(path) => {
                direct.push(mpd_addable_path(path));
                (direct, yt)
            }
            // Video items queue their audio track into MPD.
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
            // Defensive: the [Audio] queue actions only ever see audio
            // items (torrents are routed through the [Torrent] section).
            PastedItem::Torrent(_) | PastedItem::Magnet(_) => (direct, yt),
        });

    if !direct.is_empty() {
        let enqueue: Vec<Enqueue> =
            direct.iter().cloned().map(|path| Enqueue::File { path }).collect();
        ctx.command(move |client| {
            client.enqueue_multiple(enqueue, autoplay_idx, position, false)?;
            Ok(())
        });
    }

    if !yt.is_empty() {
        let action =
            if play { YtAction::AddAfterCurrentAndPlay } else { YtAction::Append };
        let count = yt.len();
        let _ = ctx
            .work_sender
            .send(WorkRequest::ResolveYtStreams { urls: yt, action })
            .map_err(|err| anyhow::anyhow!("Failed to request stream resolution: {err}"))?;
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
    // Remember each stream URL's video info (title/thumbnail/description)
    // so the now-playing bar, album art and info box can show it (the
    // stream itself carries no metadata), and persist it so a restart can
    // restore + re-fetch it while the stream is still playing.
    {
        let mut yt_info = ctx.yt_info.borrow_mut();
        let mut chapters = ctx.chapters.borrow_mut();
        for item in &info {
            if !item.title.is_empty() {
                yt_info.insert(item.url.clone(), item.clone());
                // Key the original link too: an mpv session plays the link
                // itself and looks the info up by it.
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
        // No size cap on the in-memory map: clearing it would wipe the
        // info of the stream currently playing (the map is seeded from the
        // on-disk cache, which can easily exceed any small cap). The disk
        // cache below has its own bound.
    }
    let cache_dir = ctx.config.cache_dir.as_deref();
    let mut cache = load_yt_cache(cache_dir);
    for item in &info {
        if !item.title.is_empty() {
            cache.insert(item.url.clone(), item.clone());
            // The playlist entry of an mpv session carries the original
            // link: key the cache by it too, so a restart can restore the
            // info for a video playing through mpv (the resolved stream
            // URL expires).
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
            ctx.query().id(PASTE_PLAY).replace_id(PASTE_PLAY).target(PaneType::Radio { tree: TreeBrowserArgs::default() }).query(
                move |client| {
                    let id = client.add_id(&url, None)?;
                    client.play_id(id)?;
                    Ok(crate::MpdQueryResult::Any(Box::new(id)))
                },
            );
        }
        // Launch mpv on the original links with the resolved titles (mpv
        // plays them itself; the session shows the real titles and the
        // thumbnails/chapters are keyed by the original link).
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
            // Links that failed to resolve still play — mpv resolves them
            // itself (or shows its own error). The failure format is
            // `{url}: {err}` and YouTube-style URLs never contain `: `.
            for failure in failures {
                if let Some(url) = failure.split_once(": ").map(|(url, _)| url.to_owned()) {
                    entries.push(MpvPlaylistEntry::new(url.clone(), url, None));
                }
            }
            // A video already playing in mpv switches the running instance
            // to these entries instead of a second mpv.
            crate::core::mpv::play_video_entries(ctx, entries);
        }
        YtAction::AddAfterCurrent => {
            // Insert after the current track: adding in reverse keeps the
            // selection order (each insert pushes earlier ones down). With
            // nothing playing MPD rejects the relative position, so append.
            let has_current = ctx.find_current_song_in_queue().is_some();
            if !has_current {
                ctx.command(move |client| {
                    for url in &urls {
                        client.add(url, None)?;
                    }
                    Ok(())
                });
                status_info!("Appended {count} item(s) to the queue");
                return;
            }
            let mut ordered = urls;
            ordered.reverse();
            ctx.command(move |client| {
                for url in &ordered {
                    client.add(url, Some(QueuePosition::RelativeAdd(0)))?;
                }
                Ok(())
            });
            status_info!("Added {count} item(s) after the current track");
        }
        // Add after the current track and start playing the first inserted
        // stream immediately (same reverse-order insertion; the autoplay
        // index is where the first item lands: past the current track, or
        // the end of the old queue when nothing plays).
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
                // Tolerate a stale index (another client may have changed
                // the queue while the streams were resolving).
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
        // A stale queue entry's signed stream URL expired (googlevideo
        // URLs die after a few hours): replace it in place with the fresh
        // stream and play it.
        YtAction::ReplaceAndPlay(song_id) => {
            let url = urls[0].clone();
            ctx.command(move |client| {
                // The queue may have shifted since the play attempt:
                // locate the stale entry by id so the fresh stream takes
                // its place (deleting shifts the rest, so insert at the
                // recorded position after the delete).
                let position = client
                    .playlist_info()?
                    .and_then(|songs| songs.iter().position(|song| song.id == song_id));
                let _ = client.delete_id(song_id); // may already be gone
                let new_id = client.add_id(&url, position.map(QueuePosition::Absolute))?;
                client.play_id(new_id)?;
                Ok(())
            });
            status_info!("Stream URL expired — re-resolved from the original link");
        }
        // Add the resolved streams to an existing stored playlist (the
        // paste popup's [Audio] Add to playlist / Create Playlist with
        // YouTube-style links).
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
        // Add/append to the persistent video playlist (the Video list).
        YtAction::AddToVideoQueue | YtAction::AppendVideoQueue
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
            // Links that failed to resolve still queue — mpv resolves them
            // itself (or shows its own error) when reached.
            for failure in &failures {
                if let Some(url) = failure.split_once(": ").map(|(url, _)| url.to_owned()) {
                    entries.push(MpvPlaylistEntry::new(url.clone(), url, None));
                }
            }
            // "Add to queue and play" starts the added videos immediately
            // (a running mpv switches to them instead of a second instance).
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
        // Startup re-fetch: the info was already stored above; nothing else.
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
    // A resolved YouTube stream carries its chapters (embedded or parsed
    // from the description) in the yt info.
    if let Some(yt) = ctx.yt_info.borrow().get(&song.file)
        && !yt.chapters.is_empty()
    {
        ctx.chapters.borrow_mut().insert(song.file.clone(), yt.chapters.clone());
        return;
    }
    // Jellyfin stream: fetch the item's chapters.
    if let Some(item_id) = crate::jellyfin::item_id_from_url(&song.file) {
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinChapters { item_id })
            .map_err(|err| log::error!(error:? = err; "Failed to request jellyfin chapters"));
        return;
    }
    // Local file: ffprobe it.
    if !crate::ui::panes::radio::is_stream_url(&song.file) {
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchFileChapters { file: song.file.clone() })
            .map_err(|err| log::error!(error:? = err; "Failed to request file chapters"));
    }
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
        .unwrap_or_else(|| crate::config::utils::tilde_expand("~/.cache/s2udio").into_owned().into())
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
            serde_json::json!({
                "title": e.title,
                "url": e.url,
                "duration": e.duration,
                // The canonical link the resolved URL was derived from;
                // kept so title/thumbnail lookups survive a restart (the
                // resolved stream URL expires and carries no video ID).
                "original_url": e.original_url,
            })
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
        .unwrap_or_else(|| crate::config::utils::tilde_expand("~/.cache/s2udio").into_owned().into())
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
    let art = ctx.mpv.art_path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let socket = ctx.mpv.socket.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    // The playlist is serialized too: a later s2udio instance reattaching to
    // this session (mpv outlives the app; the tracker daemon keeps the file
    // fresh meanwhile) restores the Queue tab's Video view from it.
    let playlist: Vec<serde_json::Value> = ctx
        .mpv
        .playlist
        .borrow()
        .iter()
        .map(|e| {
            serde_json::json!({
                "title": e.title,
                "url": e.url,
                "duration": e.duration,
                "original_url": e.original_url,
            })
        })
        .collect();
    // Duration: trust mpv's report when it has one. YouTube DASH streams
    // (and some HLS) can answer `duration` as unavailable for a long time
    // — the desktop widget then drops the timeline and disables seeking.
    // Fall back to the known duration of the playing entry (yt-dlp/Jellyfin
    // metadata carried on the entry) so mpris:length stays non-zero.
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
    let state = serde_json::json!({
        "title": ctx.mpv.title,
        "artist": ctx.mpv.artist,
        "art": art,
        "playing": !ctx.mpv.paused,
        "position": ctx.mpv.position,
        "duration": duration,
        "socket": socket,
        "item_id": ctx.mpv.item_id.clone().unwrap_or_default(),
        "volume": ctx.mpv.volume,
        "playlist": playlist,
        "playlist_pos": ctx.mpv.playlist_pos.get(),
    });
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
    info.get(&url).cloned().or_else(|| {
        info.values().find(|e| e.original_url == url).cloned()
    })
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
    info.get(&song.file).cloned().or_else(|| {
        info.values().find(|e| e.original_url == song.file).cloned()
    })
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
    // Downloads land in ~/Downloads/s2udio-downloads (outside the MPD
    // library; the MPD tab's browser shows the folder as "Downloads"
    // from a disk listing).
    let Some(output_dir) = downloads_dir() else {
        status_warn!("Cannot determine the downloads folder (~/Downloads)");
        return;
    };
    let parsed: Result<crate::shared::ytdlp::YtDlpContent, _> = original_url.parse();
    let Ok(crate::shared::ytdlp::YtDlpContent::Single(item)) = parsed else {
        status_warn!("Cannot download: not a YouTube/Soundcloud/NicoVideo link");
        return;
    };
    let spec = StreamDownloadSpec { output_dir, audio_only, split_chapters, on_complete: replace };
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
        .list_section(ctx, |mut section| {
            section = section.item("Save as audio", {
                let original = original.clone();
                let replace = replace.clone();
                move |ctx| {
                    queue_stream_download(ctx, &original, true, false, replace);
                    Ok(())
                }
            });
            section = section.item("Save as video", {
                let original = original.clone();
                let replace = replace.clone();
                move |ctx| {
                    queue_stream_download(ctx, &original, false, false, replace);
                    Ok(())
                }
            });
            if has_chapters {
                section = section.item("Audio — each chapter its own file", {
                    let original = original.clone();
                    let replace = replace.clone();
                    move |ctx| {
                        queue_stream_download(ctx, &original, true, true, replace);
                        Ok(())
                    }
                });
                section = section.item("Video — each chapter its own file", {
                    let original = original.clone();
                    let replace = replace.clone();
                    move |ctx| {
                        queue_stream_download(ctx, &original, false, true, replace);
                        Ok(())
                    }
                });
            }
            Some(section)
        })
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
        // No stream playing: drop the expected-art source so an in-flight
        // download for a previous stream can never land.
        crate::core::work::set_expected_mpris_art(None);
        return;
    };
    let Some(song_id) = ctx.status.songid else {
        crate::core::work::set_expected_mpris_art(None);
        return;
    };

    // A resolved YouTube stream: tag it now, download the thumbnail in the
    // background.
    if let Some(yt) = ctx.yt_info.borrow().get(&song.file) {
        let title = yt.title.clone();
        let channel = yt.channel.clone();
        // Record the art the current stream expects; the work thread skips
        // writes for stale sources (a slow download for the previous stream
        // must never overwrite the current thumbnail).
        let thumb = yt.thumbnail.clone();
        crate::core::work::set_expected_mpris_art(thumb.clone());
        ctx.command(move |client| {
            if !title.is_empty() {
                let _ = client.add_tag_id(song_id, "title", &title);
                let _ = client.add_tag_id(song_id, "album", &title);
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
                .send(WorkRequest::SaveMprisArt { url: thumb })
                .map_err(|err| log::error!(error:? = err; "Failed to request MPRIS art"));
        }
        return;
    }

    // A Jellyfin stream: fetch the item's metadata + image; the result
    // handler (event loop) tags the entry and writes the art file.
    if let Some(item_id) = crate::jellyfin::item_id_from_url(&song.file) {
        crate::core::work::set_expected_mpris_art(Some(format!("jellyfin:{item_id}")));
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinMpris { item_id })
            .map_err(|err| log::error!(error:? = err; "Failed to request jellyfin MPRIS info"));
        return;
    }

    // An unrecognized stream (a plain radio/icecast URL, a YouTube link
    // that was never resolved, ...) or a local file: there is nothing to
    // tag, and the previous stream's thumbnail must not linger — mpDris2
    // serves `mpris-art` for *any* non-local URL, so a stale file would
    // show as wrong album art in the media controls. Drop the expected
    // source (stops an in-flight download from landing) and remove the
    // file.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The paste popup labels and order per the user spec: both sections
    /// offer Play / Add to queue and play / Append to queue / Add to
    /// playlist / Create Playlist, then Cancel.
    #[test]
    fn paste_popup_labels_and_order() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // A video item so both the [Audio] and [Video] sections render.
        show_paste_modal(&ctx, vec![PastedItem::VideoFile("/tmp/fake-video.mp4".into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, &mut ctx).expect("paste menu renders"))
            .expect("draw ok");
        let text: String =
            terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();

        // Each label appears the expected number of times (both sections).
        // "Play" also matches inside "Create Playlist".
        for (label, count) in [
            ("[Audio]", 1),
            ("Play", 4),
            ("Add to queue and play", 2),
            ("Append to queue", 2),
            ("Add to playlist", 2),
            ("Create Playlist", 2),
            ("[Video]", 1),
            ("Cancel", 1),
        ] {
            assert_eq!(text.matches(label).count(), count, "label {label:?} in {text:?}");
        }
        // The old labels are gone ("Add to queue" only appears as part of
        // the new "Add to queue and play" label).
        assert!(!text.contains("Play (don't add to queue)"), "old label in {text:?}");
        assert_eq!(
            text.matches("Add to queue").count(),
            2,
            "plain add label must be gone in {text:?}"
        );
        // Order: [Audio] section first, then [Video], Cancel last. The
        // first occurrences are the [Audio] section's rows; the second
        // occurrences the [Video] section's.
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s:?} missing in {text:?}"));
        let at2 = |s: &str| {
            text.match_indices(s).nth(1).map(|(i, _)| i).unwrap_or_else(|| panic!("{s:?} missing in {text:?}"))
        };
        assert!(at("[Audio]") < at("Add to queue and play"));
        assert!(at("Add to queue and play") < at("Append to queue"));
        assert!(at("Append to queue") < at("Add to playlist"));
        assert!(at("Add to playlist") < at("Create Playlist"));
        assert!(at("Create Playlist") < at("[Video]"));
        assert!(at("[Video]") < at2("Add to playlist"));
        assert!(at2("Add to playlist") < at2("Create Playlist"));
        assert!(at2("Create Playlist") < at("Cancel"));
    }

    use crate::{
        mpd::commands::{Song, State},
        shared::{
            events::{ClientRequest, WorkRequest},
            ytdlp::YtStreamInfo,
        },
    };

    #[test]
    fn ensure_mpris_metadata_for_yt_stream_tags_and_requests_art() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx.clone(), work_rx.clone()),
            (client_tx.clone(), client_rx.clone()),
        );
        ctx.queue = vec![Song {
            id: 1,
            file: "https://rr4.example/audio.m4a".to_owned(),
            ..Default::default()
        }];
        ctx.status.songid = Some(1);
        ctx.status.state = State::Play;
        ctx.yt_info.borrow_mut().insert(
            "https://rr4.example/audio.m4a".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                title: "Some Mix".to_owned(),
                channel: Some("Some Channel".to_owned()),
                thumbnail: Some("https://i.ytimg.com/x.jpg".to_owned()),
                ..Default::default()
            },
        );

        ensure_mpris_metadata(&ctx);

        // The thumbnail download is queued to the work thread.
        assert!(matches!(
            work_rx.try_recv(),
            Ok(WorkRequest::SaveMprisArt { url }) if url == "https://i.ytimg.com/x.jpg"
        ));
        // A single MPD command carrying the title/album/artist addtagid
        // calls is queued to the client.
        assert!(matches!(client_rx.try_recv(), Ok(ClientRequest::Command(_))));
    }

    #[test]
    fn mpris_art_path_uses_the_cache_dir() {
        let path = mpris_art_path(None);
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("mpris-art"));
        // Round 19: s2udio-only cache files live under ~/.cache/s2udio
        // (kept out of rmpc's cache).
        assert!(
            path.to_string_lossy().contains(".cache/s2udio"),
            "default art path should live under ~/.cache/s2udio: {path:?}"
        );
    }

    #[test]
    fn s2udio_cache_path_falls_back_to_the_legacy_rmpc_file() {
        // Round 19 migration: with no configured cache dir, an existing
        // pre-round-19 file at ~/.cache/rmpc/video-playlist.json is used
        // until the new ~/.cache/s2udio location has one. Hermetic: run
        // under a temp HOME via the ENV facade (tests run in parallel).
        use crate::shared::env::ENV;
        let home = std::env::temp_dir().join(format!("s2u-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        ENV.set("HOME".to_string(), home.to_string_lossy().into_owned());

        let legacy = std::path::PathBuf::from(
            crate::config::utils::tilde_expand("~/.cache/rmpc/video-playlist.json").into_owned(),
        );
        let new = std::path::PathBuf::from(
            crate::config::utils::tilde_expand("~/.cache/s2udio/video-playlist.json").into_owned(),
        );
        // No file anywhere: the new path is returned (writes go there).
        assert_eq!(video_playlist_path(None), new);
        // Legacy file present: it is returned (migration read).
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "[]").unwrap();
        assert_eq!(video_playlist_path(None), legacy);
        // Once the new location exists, it wins.
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, "[]").unwrap();
        assert_eq!(video_playlist_path(None), new);

        ENV.remove("HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn parses_youtube_links() {
        let items = parse_paste("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::Yt(_)));
    }

    #[test]
    fn parses_youtu_be() {
        let items = parse_paste("https://youtu.be/abc123");
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::Yt(_)));
    }

    #[test]
    fn parses_soundcloud() {
        let items = parse_paste("https://soundcloud.com/artist/track");
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::Yt(_)));
    }

    #[test]
    fn parses_direct_audio_url() {
        let items = parse_paste("https://example.com/song.mp3");
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::Url(_)));
    }

    #[test]
    fn parses_direct_video_url() {
        let items = parse_paste("https://example.com/movie.mp4");
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::VideoUrl(_)));
    }

    #[test]
    fn parses_local_video_file() {
        let dir = std::env::temp_dir().join(format!("paste-video-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("clip.mkv");
        std::fs::write(&path, b"x").unwrap();
        let items = parse_paste(&path.to_string_lossy());
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::VideoFile(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_magnet_links() {
        let items = parse_paste(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Big+Buck+Bunny",
        );
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::Magnet(_)));
    }

    #[test]
    fn magnet_label_shows_infohash_prefix() {
        let item = PastedItem::Magnet(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Big+Buck+Bunny".into(),
        );
        assert_eq!(item.label(), "magnet:01234567");
    }

    #[test]
    fn magnet_without_infohash_falls_back_to_generic_label() {
        let item = PastedItem::Magnet("magnet:?dn=onlyname".into());
        assert_eq!(item.label(), "magnet link");
    }

    #[test]
    fn dedupes_magnets_by_infohash() {
        // The same infohash with different trackers is the same content.
        let items = parse_paste(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&tr=http://a \
             magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&tr=http://b",
        );
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::Magnet(_)));
    }

    #[test]
    fn parses_torrent_http_url() {
        let items = parse_paste("https://example.com/movie.torrent");
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0], PastedItem::Torrent(u) if u == "https://example.com/movie.torrent")
        );
        // Query strings still match.
        let items = parse_paste("https://example.com/movie.torrent?x=1");
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0], PastedItem::Torrent(u) if u == "https://example.com/movie.torrent?x=1")
        );
    }

    #[test]
    fn parses_local_torrent_file() {
        let dir = std::env::temp_dir().join(format!("paste-torrent-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("movie.torrent");
        std::fs::write(&path, b"d8:announce0e").unwrap();
        let items = parse_paste(&path.to_string_lossy());
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::Torrent(t) if t == &path.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_torrent_file_uri_with_unescaping() {
        let dir = std::env::temp_dir().join(format!("paste-torrent-uri-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("My Movie.torrent");
        std::fs::write(&path, b"d8:announce0e").unwrap();
        let escaped = path.to_string_lossy().replace(' ', "\\ ");
        let items = parse_paste(&format!("file://{escaped}"));
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::Torrent(t) if t == &path.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_missing_local_torrents() {
        assert!(parse_paste("/nonexistent/definitely-not-here.torrent").is_empty());
    }

    #[test]
    fn mixed_paste_keeps_torrent_out_of_audio_and_video() {
        let dir = std::env::temp_dir().join(format!("paste-mixed-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let audio = dir.join("song.mp3");
        std::fs::write(&audio, b"x").unwrap();
        let items = parse_paste(&format!(
            "{}
magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567
https://example.com/movie.torrent",
            audio.display()
        ));
        assert_eq!(items.len(), 3);
        // A magnet is never classified as an audio/video URL.
        assert!(items.iter().any(|i| matches!(i, PastedItem::File(_))));
        assert!(items.iter().any(|i| matches!(i, PastedItem::Magnet(_))));
        assert!(items.iter().any(|i| matches!(i, PastedItem::Torrent(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Render a modal into a test terminal and return the buffer text.
    fn render_modal_text(
        modal: &mut Box<dyn crate::ui::modals::Modal + Send + Sync>,
        ctx: &mut Ctx,
    ) -> String {
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, ctx).expect("modal renders"))
            .expect("draw ok");
        terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect()
    }

    /// A pasted magnet with a stable infohash (used by several tests).
    const MAGNET: &str = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";
    /// The magnet's canonical scan key (its full infohash — round 20):
    /// `Ctx.torrent_scans` / pending / progress are indexed by it.
    const MAGNET_KEY: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn magnet_scan_keys_are_the_full_infohash_and_match_the_work_thread() {
        // Round 20 (duplicate-paste fix): a magnet's scan key is its full
        // infohash, so pasting the same torrent twice — even via a
        // different magnet URI (extra trackers) — reuses the same scan
        // slot, and the UI key matches the work thread's `source_key`.
        let plain = PastedItem::Magnet(MAGNET.into());
        let variant = PastedItem::Magnet(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&tr=udp://extra:1337"
                .into(),
        );
        let other = PastedItem::Magnet(
            "magnet:?xt=urn:btih:ffffffffffffffffffffffffffffffffffffffff".into(),
        );
        assert_eq!(torrent_item_key(&plain), MAGNET_KEY, "key is the infohash");
        assert_eq!(
            torrent_item_key(&variant),
            MAGNET_KEY,
            "tracker variants of the same torrent share the key"
        );
        assert_ne!(torrent_item_key(&other), MAGNET_KEY, "a different torrent is a different key");
        // The UI key and the work thread's key must agree (the scan result
        // arrives under `TorrentItem::source_key`).
        assert_eq!(
            torrent_item_key(&plain),
            crate::core::torrent::TorrentItem::Magnet(MAGNET.into()).source_key()
        );
        // Uppercase hex is normalized (lowercased).
        let upper = PastedItem::Magnet(
            "magnet:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567".into(),
        );
        assert_eq!(torrent_item_key(&upper), MAGNET_KEY, "infohash lowercased");
        // A magnet without a recognizable infohash falls back to the raw URI.
        let nohash = PastedItem::Magnet("magnet:?dn=name&tr=udp://x:1337".into());
        assert_eq!(torrent_item_key(&nohash), "magnet:?dn=name&tr=udp://x:1337");
    }

    #[test]
    fn repeat_paste_of_the_same_magnet_reuses_the_landed_scan() {
        // Round 20 (duplicate-paste fix): the first paste scanned the
        // magnet and the scan is still in `Ctx.torrent_scans` (landed,
        // engine alive). Pasting the same magnet again must NOT spawn a
        // second engine — the popup shows the play actions backed by the
        // existing scan.
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "Big Buck Bunny.mp4".to_owned(),
                length: 276_000_000,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));

        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);
        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);
        assert!(text.contains("Stream"), "actions shown from the landed scan in {text:?}");
        assert!(!text.contains("Loading"), "no wait window in {text:?}");
        assert!(
            work_rx.recv_timeout(std::time::Duration::from_millis(50)).is_err(),
            "no new scan request — the landed scan is reused"
        );
    }

    #[test]
    fn repeat_paste_while_the_scan_is_in_flight_does_not_start_a_second_scan() {
        // Round 20 (duplicate-paste fix): the first paste's scan is still
        // in flight (wait window). A second paste of the same magnet shows
        // the wait window too but must not queue a second ScanTorrent.
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );

        // First paste: the wait window opens and one ScanTorrent request
        // goes out (the popup registers the key as in-flight).
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);
        let _ = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let first = work_rx.recv_timeout(std::time::Duration::from_millis(200)).expect("scan queued");
        assert!(matches!(first, WorkRequest::ScanTorrent { .. }), "first scan request");

        // Second paste while the scan is in flight: the wait window shows
        // again (no actions yet) and NO second request goes out.
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);
        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the replaced paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);
        assert!(text.contains("Loading magnet:01234567"), "wait window in {text:?}");
        assert!(
            work_rx.recv_timeout(std::time::Duration::from_millis(50)).is_err(),
            "the in-flight scan is shared — no second engine"
        );
    }

    #[test]
    fn torrent_popup_shows_loading_and_requests_a_scan() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);

        // Round 17/18: a pure-torrent paste shows the [Torrent] section
        // with the round-18 wait window (Loading row + elapsed counter,
        // esc to cancel — round 20 dropped the DL-speed / needed-speed
        // ✓✗ row) and no actions until the scan lands.
        assert!(text.contains("[Torrent]"), "torrent section in {text:?}");
        assert!(text.contains("Loading magnet:01234567"), "loading row in {text:?}");
        assert!(text.contains("00:00"), "elapsed counter starts at zero in {text:?}");
        assert!(!text.contains("DL "), "no DL-speed row in the wait window in {text:?}");
        assert!(text.contains("esc to cancel"), "cancel hint in {text:?}");
        assert!(!text.contains("Stream"), "no action before the scan in {text:?}");
        assert!(!text.contains("[Audio]"), "no audio section in {text:?}");
        assert!(!text.contains("[Video]"), "no video section in {text:?}");
        assert!(text.contains("Cancel"), "cancel in {text:?}");

        // A ScanTorrent work request went out (once), with a live cancel
        // channel registered in Ctx for the close hook.
        let req = work_rx.recv_timeout(std::time::Duration::from_millis(200)).expect("scan queued");
        match req {
            WorkRequest::ScanTorrent { item, cancel } => {
                assert!(matches!(
                    item,
                    crate::core::torrent::TorrentItem::Magnet(m) if m.starts_with("magnet:")
                ));
                assert!(cancel.try_recv().is_err(), "no cancel before the popup closes");
            }
            other => panic!("expected a ScanTorrent request, got a different request"),
        }
        assert!(
            work_rx.recv_timeout(std::time::Duration::from_millis(50)).is_err(),
            "exactly one scan request per item"
        );
    }

    #[test]
    fn torrent_popup_wait_block_renders_counter_and_cancel_hint_only() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // A scan is in flight with live progress (round 18): 12 s in.
        // Round 20: the wait window shows ONLY the elapsed counter + the
        // esc hint — the DL-speed / needed-speed ✓✗ row was dropped (the
        // speed is ~0 during the metainfo wait, so it always read ✗), even
        // though the progress value still flows for the M3 bandwidth gate.
        ctx.torrent_scans_pending.borrow_mut().insert(MAGNET_KEY.to_owned());
        ctx.torrent_scan_progress.borrow_mut().insert(
            MAGNET_KEY.to_owned(),
            crate::core::torrent::TorrentScanProgress {
                elapsed_secs: 12,
                download_speed_kbps: 840.0,
            },
        );
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);

        // The trimmed wait window: the counter row, the esc-to-cancel hint
        // — and no DL-speed row (any speed value) and no play actions yet.
        assert!(
            text.contains("Loading magnet:01234567… 00:12"),
            "counter row in {text:?}"
        );
        assert!(text.contains("esc to cancel"), "cancel hint in {text:?}");
        assert!(!text.contains("DL "), "no DL-speed row in {text:?}");
        assert!(!text.contains("need ≥"), "no needed-speed check in {text:?}");
        assert!(!text.contains("Stream"), "no action before the scan in {text:?}");
    }

    #[test]
    fn torrent_popup_wait_block_never_shows_the_speed_check_row() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // Round 20: even far into the wait (30 s) with a live speed value
        // the popup renders counter + esc hint only — the ✓/✗ gate row is
        // gone from the wait window entirely.
        ctx.torrent_scans_pending.borrow_mut().insert(MAGNET_KEY.to_owned());
        ctx.torrent_scan_progress.borrow_mut().insert(
            MAGNET_KEY.to_owned(),
            crate::core::torrent::TorrentScanProgress {
                elapsed_secs: 30,
                download_speed_kbps: 120.0,
            },
        );
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);

        assert!(
            text.contains("Loading magnet:01234567… 00:30"),
            "counter row in {text:?}"
        );
        assert!(text.contains("esc to cancel"), "cancel hint in {text:?}");
        assert!(!text.contains("DL "), "no DL-speed row in {text:?}");
        assert!(!text.contains("need ≥"), "no needed-speed check in {text:?}");
        assert!(!text.contains("✗"), "no cross marker in {text:?}");
    }

    #[test]
    fn esc_closing_the_popup_cancels_in_flight_scans() {
        use std::sync::Arc;

        use crate::{
            config::keys::CommonAction,
            shared::events::AppEvent,
            shared::keys::{ActionEvent, Actions},
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        // The scan request went out with its cancel channel registered.
        let cancel_rx = match work_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(WorkRequest::ScanTorrent { cancel, .. }) => cancel,
            _ => panic!("expected a ScanTorrent request"),
        };
        assert!(cancel_rx.try_recv().is_err(), "no cancel while the popup is open");

        // Esc closes the popup: the close hook fires the scan's cancel
        // channel (round 18) and clears the scan bookkeeping.
        let mut action =
            ActionEvent::from(Arc::new(vec![Actions::Common(CommonAction::Close)]));
        modal.handle_key(&mut action, &mut ctx).unwrap();

        assert!(
            cancel_rx.try_recv().is_ok(),
            "the close hook signalled the scan's cancel channel"
        );
        assert!(ctx.torrent_scan_cancels.borrow().is_empty(), "cancel map cleared");
        assert!(ctx.torrent_scan_progress.borrow().is_empty(), "progress map cleared");
        assert!(ctx.torrent_scans_pending.borrow().is_empty(), "pending cleared");
        assert!(ctx.paste_modal_items.borrow().is_none(), "items forgotten");
    }

    #[test]
    fn torrent_popup_single_file_scan_shows_play_actions() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "Big Buck Bunny.mp4".to_owned(),
                length: 276_000_000,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);

        // One video file: Stream / Download / Cancel, no multi-file extras.
        assert!(text.contains("Stream"), "stream action in {text:?}");
        assert!(text.contains("Download"), "download action in {text:?}");
        assert!(!text.contains("Stream all"), "no stream-all for one file in {text:?}");
        assert!(!text.contains("Select files"), "no select for one file in {text:?}");
        assert!(!text.contains("Loading"), "no loading row after the scan in {text:?}");
    }

    #[test]
    fn torrent_popup_multi_video_scan_shows_stream_download_all_and_select() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "readme.txt".to_owned(),
                length: 10,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E02.mkv".to_owned(),
                length: 900,
            },
            crate::core::torrent::ScannedFile {
                index: 3,
                name: "S01E03.mkv".to_owned(),
                length: 800,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);

        // A season pack (round 21): Stream all / Download all / Select
        // files… — the plain single-file action is gone from the multi
        // popup (every "Stream" is part of "Stream all", every "Download"
        // part of "Download all").
        assert!(text.contains("Stream all"), "stream-all in {text:?}");
        assert!(text.contains("Download all"), "download-all in {text:?}");
        assert!(text.contains("Select files…"), "select action in {text:?}");
        assert_eq!(
            text.matches("Stream").count(),
            text.matches("Stream all").count(),
            "no plain Stream row in {text:?}"
        );
        assert_eq!(
            text.matches("Download").count(),
            text.matches("Download all").count(),
            "no plain Download row in {text:?}"
        );
        assert!(!text.contains("Loading"), "no loading row after the scan in {text:?}");
    }

    #[test]
    fn torrent_popup_scan_failure_shows_the_notice_row() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.torrent_scans.borrow_mut().insert(
            MAGNET_KEY.to_owned(),
            Err("No peers found — is the torrent alive?".to_owned()),
        );
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);

        // A dead magnet: a dim notice, no actions.
        assert!(
            text.contains("No peers found — is the torrent alive?"),
            "notice row in {text:?}"
        );
        assert!(!text.contains("Stream"), "no action on failure in {text:?}");
    }

    #[test]
    fn torrent_popup_no_playable_media_shows_the_notice_row() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "readme.txt".to_owned(),
                length: 10,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "data.bin".to_owned(),
                length: 1_000,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        show_paste_modal(&ctx, vec![PastedItem::Magnet(MAGNET.into())]);

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let text = render_modal_text(&mut modal, &mut ctx);

        // A data torrent: nothing to play.
        assert!(
            text.contains("No playable media in this torrent"),
            "notice row in {text:?}"
        );
        assert!(!text.contains("Stream"), "no action without media in {text:?}");
    }

    #[test]
    fn torrent_entries_preserve_the_positional_indices() {
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "S01E02.mkv".to_owned(),
                length: 900,
            },
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E03.mkv".to_owned(),
                length: 800,
            },
        ]);
        // "Stream all": every video, in scan order, one entry per file.
        let entries = torrent_entries(&scan, &[0, 1, 2]);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].title, "S01E01.mkv");
        assert!(entries[0].url.ends_with("/torrents/1/stream/0"), "{}", entries[0].url);
        assert!(entries[1].url.ends_with("/torrents/1/stream/1"), "{}", entries[1].url);
        assert!(entries[2].url.ends_with("/torrents/1/stream/2"), "{}", entries[2].url);
        // "Select files…": the picked indices keep their stream targets.
        let selection = torrent_entries(&scan, &[2, 0]);
        assert_eq!(selection[0].title, "S01E03.mkv");
        assert!(selection[0].url.ends_with("/torrents/1/stream/2"), "{}", selection[0].url);
        assert_eq!(selection[1].title, "S01E01.mkv");
        assert!(selection[1].url.ends_with("/torrents/1/stream/0"), "{}", selection[1].url);
    }

    #[test]
    fn torrent_play_remembers_synthetic_yt_info_for_the_video_queue() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "S01E02.mkv".to_owned(),
                length: 900,
            },
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E03.mkv".to_owned(),
                length: 800,
            },
        ]);
        let entries = torrent_entries(&scan, &[0, 1, 2]);
        // The event loop's playback start calls remember_torrent_entries
        // (the Queue tab's info box / MPRIS artist look the info up by the
        // stream URL; it is never persisted — the URL embeds the token).
        remember_torrent_entries(&ctx, &scan.torrent_name, &entries);
        let info = ctx.yt_info.borrow().get(&entries[1].url).cloned().expect("yt info");
        assert_eq!(info.title, "S01E02.mkv");
        assert_eq!(info.channel.as_deref(), Some("Fake Pack"));
        // The current-entry lookup (mpv_yt_info) resolves it the same way.
        ctx.mpv.active = true;
        *ctx.mpv.playlist.borrow_mut() = entries.clone();
        ctx.mpv.playlist_pos.set(Some(1));
        let info = mpv_yt_info(&ctx).expect("current entry info");
        assert_eq!(info.title, "S01E02.mkv");
        assert_eq!(info.channel.as_deref(), Some("Fake Pack"));
    }

    #[test]
    fn play_all_uses_the_scanned_engine_and_all_video_indices() {
        // Round-18 host finding (2026-08-09): the multi-file play action
        // must send `TorrentScannedPlay` with every video index (one
        // stream URL per file; mpv advances the playlist one at a time and
        // the Queue tab's Video list shows all files) — NOT fall back to
        // the fresh single-file path. This mirrors the user flow: the
        // scan landed in `Ctx.torrent_scans`, the popup rendered the play
        // actions, and the click runs `play_scanned_or_fresh`.
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "readme.txt".to_owned(),
                length: 10,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E02.mkv".to_owned(),
                length: 900,
            },
            crate::core::torrent::ScannedFile {
                index: 3,
                name: "S01E03.mkv".to_owned(),
                length: 800,
            },
        ]);
        // The scan result is in Ctx (on_torrent_scanned stored it).
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        let item = PastedItem::Magnet(MAGNET.into());
        let key = torrent_item_key(&item);
        // "Stream all": the popup action passes every video index.
        let indices = ctx
            .torrent_scans
            .borrow()
            .get(&key)
            .and_then(|r| r.as_ref().ok())
            .map(|s| s.videos().iter().map(|f| f.index).collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(indices, vec![1, 2, 3], "the three video files (readme.txt excluded)");
        play_scanned_or_fresh(&ctx, &item, &key, indices, false).expect("play all starts");
        match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::TorrentScannedPlay { scan, file_indices, download }) => {
                assert_eq!(file_indices, vec![1, 2, 3], "every video index is queued");
                assert_eq!(scan.files.len(), 4);
                assert!(!download, "play all is not a download action");
                // One stream URL per file, addressed by positional index.
                assert!(
                    scan.engine.stream_url(&scan.torrent_id, 1).contains("/stream/1"),
                    "{}",
                    scan.engine.stream_url(&scan.torrent_id, 1)
                );
                assert!(
                    scan.engine.stream_url(&scan.torrent_id, 2).contains("/stream/2"),
                    "{}",
                    scan.engine.stream_url(&scan.torrent_id, 2)
                );
            }
            other => panic!("expected TorrentScannedPlay, got {other:?}"),
        }
        // Round 20: the scan STAYS in Ctx (the engine is shared via
        // `Arc`) so a repeat paste of the same magnet reuses the engine
        // instead of spawning a second rqbit on the same cache dir.
        assert!(
            ctx.torrent_scans.borrow().get(&key).is_some(),
            "the played scan stays in Ctx for reuse"
        );
    }

    #[test]
    fn download_action_sends_the_download_only_event_for_the_picked_file() {
        // Round 21: the popup's single-file "Download" keeps the scanned
        // engine and hands the picked file to the event loop as
        // `TorrentScannedDownload` — no playback (the event carries no
        // download/play flag at all).
        use crate::shared::events::AppEvent;
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "readme.txt".to_owned(),
                length: 10,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E02.mkv".to_owned(),
                length: 900,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        let item = PastedItem::Magnet(MAGNET.into());
        let key = torrent_item_key(&item);
        // "Download": empty indices = the single best playable file.
        download_scanned_or_fresh(&ctx, &item, &key, Vec::new()).expect("download starts");
        match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::TorrentScannedDownload { scan, file_indices }) => {
                assert_eq!(file_indices, vec![1], "the largest playable file is picked");
                assert_eq!(scan.files.len(), 3);
                assert!(
                    scan.engine.stream_url(&scan.torrent_id, 1).contains("/stream/1"),
                    "{}",
                    scan.engine.stream_url(&scan.torrent_id, 1)
                );
            }
            other => panic!("expected TorrentScannedDownload, got {other:?}"),
        }
        // The scan stays in Ctx for reuse (round 20 engine sharing).
        assert!(
            ctx.torrent_scans.borrow().get(&key).is_some(),
            "the downloaded scan stays in Ctx"
        );
    }

    #[test]
    fn download_all_sends_the_download_event_with_every_video_index() {
        // Round 21: the popup's multi-file "Download all" keeps the
        // scanned engine and hands EVERY video index to the event loop as
        // `TorrentScannedDownload` (no playback) — mirrors the "Stream
        // all" action's index set, minus the play flag.
        use crate::shared::events::AppEvent;
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "readme.txt".to_owned(),
                length: 10,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E02.mkv".to_owned(),
                length: 900,
            },
            crate::core::torrent::ScannedFile {
                index: 3,
                name: "S01E03.mkv".to_owned(),
                length: 800,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        let item = PastedItem::Magnet(MAGNET.into());
        let key = torrent_item_key(&item);
        // "Download all": the popup action passes every video index.
        let indices = ctx
            .torrent_scans
            .borrow()
            .get(&key)
            .and_then(|r| r.as_ref().ok())
            .map(|s| s.videos().iter().map(|f| f.index).collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(indices, vec![1, 2, 3], "the three video files (readme.txt excluded)");
        download_scanned_or_fresh(&ctx, &item, &key, indices).expect("download all starts");
        match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::TorrentScannedDownload { scan, file_indices }) => {
                assert_eq!(file_indices, vec![1, 2, 3], "every video index is kept");
                assert_eq!(scan.files.len(), 4);
                // One stream URL per file, addressed by positional index.
                assert!(
                    scan.engine.stream_url(&scan.torrent_id, 2).contains("/stream/2"),
                    "{}",
                    scan.engine.stream_url(&scan.torrent_id, 2)
                );
            }
            other => panic!("expected TorrentScannedDownload, got {other:?}"),
        }
        assert!(
            ctx.torrent_scans.borrow().get(&key).is_some(),
            "the downloaded scan stays in Ctx for reuse"
        );
    }

    #[test]
    fn select_files_confirm_plays_the_marked_files_from_the_captured_scan() {
        // Round-18 host finding (2026-08-09): "select files, press Enter,
        // it never plays". Opening the picker is a paste-popup action, and
        // the popup's close hook (fired right after by `MenuModal::destroy`)
        // cleared `Ctx.torrent_scans` — so the picker's Enter re-lookup via
        // `play_scanned_or_fresh` found nothing and fell back to the fresh
        // single-file path. The picker now captures the scan when it opens;
        // confirm must emit `TorrentScannedPlay` with the marked indices.
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "readme.txt".to_owned(),
                length: 10,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E02.mkv".to_owned(),
                length: 900,
            },
            crate::core::torrent::ScannedFile {
                index: 3,
                name: "S01E03.mkv".to_owned(),
                length: 800,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        let item = PastedItem::Magnet(MAGNET.into());
        let key = torrent_item_key(&item);
        open_torrent_file_picker(&ctx, &item, &key).expect("picker opens");

        // The picker queued as a modal; grab it, then simulate the paste
        // popup being torn down beneath it (its close hook clears Ctx —
        // the exact condition that broke the pre-fix lookup).
        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the picker modal, got {other:?}"),
        };
        ctx.torrent_scans.borrow_mut().clear();

        // Mark two videos and confirm (the picker's Enter path). The list
        // starts with the name-sorted options selected at row 0; Space
        // toggles the mark on the highlighted row, so mark rows 0 and 1
        // (S01E01.mkv → positional 1, S01E02.mkv → positional 2).
        use crate::{
            shared::keys::Actions,
            ui::ActionEvent,
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // Render once so the picker's list selection is initialized.
        render_modal_text(&mut modal, &mut ctx);
        for row in 0..2 {
            let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
            assert!(modal.handle_raw_key(space, &mut ctx).expect("space marks"));
            if row == 0 {
                // Move to the next row so the second Space marks row 1.
                let mut down = ActionEvent::from(std::sync::Arc::new(vec![Actions::Common(
                    crate::config::keys::CommonAction::Down,
                )]));
                modal.handle_key(&mut down, &mut ctx).expect("down to row 1");
            }
        }
        // Enter on the list moves the cursor to the action buttons (Play
        // focused); the second Enter confirms Play for the marked files.
        let mut enter = ActionEvent::from(std::sync::Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal.handle_key(&mut enter, &mut ctx).expect("enter moves to the buttons");
        let mut enter2 = ActionEvent::from(std::sync::Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal.handle_key(&mut enter2, &mut ctx).expect("enter confirms the picker");

        // The picker's Space/render calls queue RequestRender events ahead
        // of the confirm's TorrentScannedPlay — skip those.
        loop {
            match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(AppEvent::TorrentScannedPlay { scan, file_indices, download }) => {
                    assert_eq!(file_indices, vec![1, 2], "the marked videos play, in order");
                    assert_eq!(scan.files.len(), 4, "the captured scan is intact");
                    assert!(!download);
                    break;
                }
                // Render/PopModal noise from the picker's interactions.
                Ok(AppEvent::UiEvent(_)) | Ok(AppEvent::RequestRender) => {}
                Ok(other) => panic!("expected TorrentScannedPlay from the picker, got {other:?}"),
                Err(_) => panic!("TorrentScannedPlay never arrived"),
            }
        }
    }

    #[test]
    fn select_files_action_opens_the_picker_modal() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E03.mkv".to_owned(),
                length: 800,
            },
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        let item = PastedItem::Magnet(MAGNET.into());
        open_torrent_file_picker(&ctx, &item, &torrent_item_key(&item)).expect("picker opens");
        let event = app_rx.recv_timeout(std::time::Duration::from_millis(200)).expect("picker queued");
        match event {
            AppEvent::UiEvent(UiAppEvent::Modal(_)) => {}
            other => panic!("expected the picker modal, got {other:?}"),
        }
    }

    #[test]
    fn torrent_file_picker_lists_videos_with_sizes_and_toggles_marks() {
        use crate::ui::modals::torrent_file_picker::TorrentFilePicker;
        use crate::{
            shared::keys::Actions,
            ui::ActionEvent,
        };
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                |_ctx, _indices, _action| Ok(()),
            ));
        let text = render_modal_text(&mut modal, &mut ctx);
        // Name + human-readable size for every video file.
        assert!(text.contains("S01E03.mkv"), "row in {text:?}");
        assert!(text.contains("1.0 GB"), "size in {text:?}");
        assert!(text.contains("S01E01.mkv"), "row in {text:?}");
        assert!(text.contains("500.0 MB"), "size in {text:?}");
        // Space marks the highlighted file; the header shows the count.
        let mut key = ActionEvent::from(std::sync::Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Select,
        )]));
        modal.handle_key(&mut key, &mut ctx).expect("space toggles the mark");
        let text = render_modal_text(&mut modal, &mut ctx);
        assert!(text.contains("1 marked"), "mark count in {text:?}");

        // The help row lists the actual controls.
        assert!(text.contains("Space toggles"), "help row in {text:?}");
        assert!(text.contains("Enter: buttons"), "help row in {text:?}");
    }

    #[test]
    fn torrent_file_picker_round22_title_marker_and_right_margin() {
        // Round 22 (FEEDBACK-2026-08-09-16.md): the picker title starts
        // with "▶" (no "Select" prefix that left-truncates to a stray
        // "t" on long torrent names), and the file rows keep one blank
        // column before the scrollbar.
        use crate::ui::modals::torrent_file_picker::TorrentFilePicker;
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Banana.Fish.S01.1080p.NF.WEB-DL.DUAL.AAC2.0.H.264-VARYG ",
                vec![
                    // Long enough that the row would touch the scrollbar
                    // column without the round-22 right margin.
                    (
                        0,
                        "Banana.Fish.S01E01.1080p.NF.WEB-DL.DUAL.AAC2.0.H.264-VARYG.mkv".to_owned(),
                        1_073_741_824,
                    ),
                ],
                |_ctx, _indices, _action| Ok(()),
            ));
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, &mut ctx).expect("modal renders"))
            .expect("draw ok");
        let buf = terminal.backend().buffer();

        // 1. Title: "▶ files — <name>", and the old "Select files —"
        // prefix is gone (no stray "t" for long names).
        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("▶ files — Banana.Fish.S01.1080p"), "title in {text:?}");
        assert!(!text.contains("Select files —"), "old title prefix gone in {text:?}");

        // 2. The long file row ends one column before the scrollbar: the
        // last content cell is a name char, the next cell is blank (the
        // margin), and the rightmost cell is the scrollbar glyph.
        let mut row: Option<String> = None;
        for y in 0..buf.area.height {
            let line: String =
                (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>();
            if line.contains("Banana.Fish.S01E01") {
                row = Some(line);
                break;
            }
        }
        let row = row.expect("the long file row rendered");
        let cells: Vec<char> = row.chars().collect();
        assert_eq!(cells.len(), 70, "row width in {row:?}");
        assert_ne!(cells[67], ' ', "the name reaches the content edge (precondition)");
        assert_eq!(cells[68], ' ', "one blank column before the scrollbar in {row:?}");
        assert!(
            matches!(cells[69], '│' | '█' | '▲' | '▼'),
            "scrollbar glyph at the right edge, got {:?} in {row:?}",
            cells[69]
        );
    }

    #[test]
    fn torrent_file_picker_raw_space_toggles_then_enter_moves_to_buttons() {
        use crate::ui::modals::torrent_file_picker::TorrentFilePicker;
        use crate::{
            shared::keys::Actions,
            ui::ActionEvent,
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::sync::Arc;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let played = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let played_cb = played.clone();
        let mut modal: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                move |_ctx, indices, _action| {
                    *played_cb.lock().unwrap() = indices;
                    Ok(())
                },
            ));

        // The global keybind set maps Space to TogglePause, so the picker
        // must claim the raw key itself: a plain Space toggles the mark on
        // the highlighted (first) row.
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(
            modal.handle_raw_key(space, &mut ctx).expect("raw space handled"),
            "the picker consumes Space"
        );
        let text = render_modal_text(&mut modal, &mut ctx);
        assert!(text.contains("1 marked"), "space marked a file in {text:?}");

        // Enter (Confirm) on the list moves the cursor to the action
        // buttons (Play is focused first); a second Enter confirms Play.
        let mut enter = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal.handle_key(&mut enter, &mut ctx).expect("enter moves to the buttons");
        let mut enter2 = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal.handle_key(&mut enter2, &mut ctx).expect("enter confirms Play");
        assert_eq!(
            *played.lock().unwrap(),
            vec![3],
            "the marked file's positional index is played"
        );

        // Nothing marked plays everything (all positional indices).
        let played_cb2 = played.clone();
        let mut modal2: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                move |_ctx, indices, _action| {
                    *played_cb2.lock().unwrap() = indices;
                    Ok(())
                },
            ));
        let mut enter = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal2.handle_key(&mut enter, &mut ctx).expect("enter moves to the buttons");
        let mut enter2 = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal2.handle_key(&mut enter2, &mut ctx).expect("enter confirms Play");
        assert_eq!(*played.lock().unwrap(), vec![3, 1], "no marks plays every file");
    }

    #[test]
    fn torrent_file_picker_offers_play_downloads_and_play_and_cancel() {
        use crate::ui::modals::torrent_file_picker::TorrentFilePicker;
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                |_ctx, _indices, _action| Ok(()),
            ));
        let text = render_modal_text(&mut modal, &mut ctx);
        // Round 20: the picker's confirm buttons are Play / Downloads &
        // Play / Cancel (the middle one also keeps the picked files).
        assert!(text.contains("Play"), "play button in {text:?}");
        assert!(text.contains("Download & Play"), "download button in {text:?}");
        assert!(text.contains("Cancel"), "cancel button in {text:?}");
        assert!(!text.contains("Play selected"), "old label gone in {text:?}");
    }

    #[test]
    fn select_files_downloads_and_play_sends_the_download_flag() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let scan = crate::core::torrent::test_scan(vec![
            crate::core::torrent::ScannedFile {
                index: 0,
                name: "readme.txt".to_owned(),
                length: 10,
            },
            crate::core::torrent::ScannedFile {
                index: 1,
                name: "S01E01.mkv".to_owned(),
                length: 1_000,
            },
            crate::core::torrent::ScannedFile {
                index: 2,
                name: "S01E02.mkv".to_owned(),
                length: 900,
            },
        ]);
        ctx.torrent_scans.borrow_mut().insert(MAGNET_KEY.to_owned(), Ok(scan));
        let item = PastedItem::Magnet(MAGNET.into());
        open_torrent_file_picker(&ctx, &item, &torrent_item_key(&item)).expect("picker opens");

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the picker modal, got {other:?}"),
        };
        use crate::{
            config::keys::CommonAction,
            shared::keys::{ActionEvent, Actions},
        };
        use std::sync::Arc;

        // Navigate from the list to the buttons and on to "Download & Play"
        // (button index 1): Up from the list's first row enters the button
        // row at the last button (Cancel), a second Up moves to
        // Download & Play — independent of the list length. A fresh
        // ActionEvent per press (an event is single-use — `claim_common`
        // marks it handled).
        render_modal_text(&mut modal, &mut ctx);
        for _ in 0..2 {
            let mut up = ActionEvent::from(Arc::new(vec![Actions::Common(CommonAction::Up)]));
            modal.handle_key(&mut up, &mut ctx).expect("up navigates the picker");
        }
        let mut enter = ActionEvent::from(Arc::new(vec![Actions::Common(CommonAction::Confirm)]));
        modal.handle_key(&mut enter, &mut ctx).expect("enter confirms the download action");

        loop {
            match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(AppEvent::TorrentScannedPlay { scan, file_indices, download }) => {
                    assert_eq!(file_indices, vec![1, 2], "all videos, in list order");
                    assert_eq!(scan.files.len(), 3, "the captured scan is intact");
                    assert!(download, "Download & Play sets the download flag");
                    break;
                }
                Ok(AppEvent::UiEvent(_)) | Ok(AppEvent::RequestRender) => {}
                Ok(other) => panic!("expected TorrentScannedPlay from the picker, got {other:?}"),
                Err(_) => panic!("TorrentScannedPlay never arrived"),
            }
        }
        // Round 20: the scan stays in Ctx for a repeat paste (engine reuse).
        assert!(
            ctx.torrent_scans.borrow().get(MAGNET_KEY).is_some(),
            "the picked scan stays in Ctx"
        );
    }

    #[test]
    fn picker_enter_on_the_list_moves_to_buttons_then_choose_download_and_play() {
        // Round-21 user note: after marking files, Enter must NOT play
        // immediately — it moves the cursor to the action buttons so the
        // user can choose Play / Download & Play / Cancel. Enter lands on
        // Play (button 0); Down moves to Download & Play (button 1);
        // Enter then confirms the download-and-play action.
        use crate::ui::modals::torrent_file_picker::TorrentFilePicker;
        use crate::{
            shared::keys::Actions,
            ui::ActionEvent,
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::sync::Arc;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let confirmed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let confirmed_cb = confirmed.clone();
        let mut modal: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                move |_ctx, indices, action| {
                    *confirmed_cb.lock().unwrap() = Some((indices, action));
                    Ok(())
                },
            ));

        // Mark the first file (Space), then Enter on the list: the cursor
        // moves to the buttons, nothing is confirmed yet.
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(modal.handle_raw_key(space, &mut ctx).expect("space marks"));
        let mut enter = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal.handle_key(&mut enter, &mut ctx).expect("enter moves to the buttons");
        assert!(
            confirmed.lock().unwrap().is_none(),
            "Enter on the list must not confirm yet"
        );

        // Down: Play -> Download & Play; Enter confirms the download flag.
        let mut down = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Down,
        )]));
        modal.handle_key(&mut down, &mut ctx).expect("down to Download & Play");
        let mut enter2 = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal.handle_key(&mut enter2, &mut ctx).expect("enter confirms Download & Play");
        let got = confirmed.lock().unwrap().clone().expect("confirmed");
        assert_eq!(got.0, vec![3], "the marked file's positional index");
        assert_eq!(
            got.1,
            crate::ui::modals::torrent_file_picker::TorrentPickerAction::DownloadAndPlay,
            "the user chose Download & Play"
        );
    }

    /// Render the picker to a 70x20 test terminal and return the cell
    /// coordinates at the CENTER of the first buffer occurrence of `label`
    /// (used to aim mouse events at the buttons — "Play" / "Download" /
    /// "Cancel" each appear once, in the button row).
    fn picker_label_pos(
        modal: &mut Box<dyn crate::ui::modals::Modal + Send + Sync>,
        ctx: &mut Ctx,
        label: &str,
    ) -> (u16, u16) {
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, ctx).expect("modal renders"))
            .expect("draw ok");
        let buf = terminal.backend().buffer();
        let chars: Vec<String> = label.chars().map(|c| c.to_string()).collect();
        for (i, cell) in buf.content.iter().enumerate() {
            let x = (i % buf.area.width as usize) as u16;
            let y = (i / buf.area.width as usize) as u16;
            if cell.symbol() == &chars[0]
                && (0..chars.len()).all(|k| buf.content[i + k].symbol() == &chars[k])
            {
                return (x + (chars.len() as u16) / 2, y);
            }
        }
        panic!("label {label} not found in the picker buffer");
    }

    /// The picker's rendered option rows ("S01E01.mkv" …) — whether the
    /// view has moved, by checking if the first row's file is still shown.
    fn picker_buffer_contains(
        modal: &mut Box<dyn crate::ui::modals::Modal + Send + Sync>,
        ctx: &mut Ctx,
        needle: &str,
    ) -> bool {
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, ctx).expect("modal renders"))
            .expect("draw ok");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        text.contains(needle)
    }

    #[test]
    fn picker_a_and_d_keys_move_between_the_buttons() {
        // Round-21 user note: with focus on the buttons, `a` / `d` (and
        // ←/→) move the selection — the minimal keybind set binds only
        // w/s/↑/↓, so the picker claims the horizontal keys raw.
        use crate::ui::modals::torrent_file_picker::TorrentFilePicker;
        use crate::{
            shared::keys::Actions,
            ui::ActionEvent,
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::sync::Arc;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let confirmed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let confirmed_cb = confirmed.clone();
        let mut modal: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                move |_ctx, indices, action| {
                    *confirmed_cb.lock().unwrap() = Some((indices, action));
                    Ok(())
                },
            ));

        // Enter moves to the buttons (Play focused); `d` moves right to
        // Download & Play; Enter confirms it.
        let mut enter = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal.handle_key(&mut enter, &mut ctx).expect("enter moves to the buttons");
        let d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            modal.handle_raw_key(d, &mut ctx).expect("raw d handled"),
            "the picker consumes d on the buttons"
        );
        let mut enter2 = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal.handle_key(&mut enter2, &mut ctx).expect("enter confirms Download & Play");
        let got = confirmed.lock().unwrap().clone().expect("confirmed");
        assert_eq!(
            got.1,
            crate::ui::modals::torrent_file_picker::TorrentPickerAction::DownloadAndPlay,
            "d moved to Download & Play"
        );

        // A fresh picker: Enter → buttons (Play); `a` wraps left to
        // Cancel; Enter closes without confirming.
        let confirmed2 = std::sync::Arc::new(std::sync::Mutex::new(None));
        let confirmed2_cb = confirmed2.clone();
        let mut modal2: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                move |_ctx, indices, action| {
                    *confirmed2_cb.lock().unwrap() = Some((indices, action));
                    Ok(())
                },
            ));
        let mut enter = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal2.handle_key(&mut enter, &mut ctx).expect("enter moves to the buttons");
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(
            modal2.handle_raw_key(a, &mut ctx).expect("raw a handled"),
            "the picker consumes a on the buttons"
        );
        let mut enter2 = ActionEvent::from(Arc::new(vec![Actions::Common(
            crate::config::keys::CommonAction::Confirm,
        )]));
        modal2.handle_key(&mut enter2, &mut ctx).expect("enter on Cancel closes");
        assert!(
            confirmed2.lock().unwrap().is_none(),
            "a wrapped to Cancel: no confirm callback"
        );
    }

    #[test]
    fn picker_double_click_activates_the_buttons() {
        // Round-21 user note: double-clicking a button activates it —
        // Play (0) confirms a play, Download & Play (1) confirms with the
        // download flag, Cancel (2) closes.
        use crate::ui::modals::torrent_file_picker::TorrentFilePicker;
        use crate::shared::mouse_event::{MouseEvent, MouseEventKind};
        use crossterm::event::KeyModifiers;
        use std::sync::Arc;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let confirmed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let confirmed_cb = confirmed.clone();
        let mut modal: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                move |_ctx, indices, action| {
                    *confirmed_cb.lock().unwrap() = Some((indices, action));
                    Ok(())
                },
            ));

        // Double-click "Play" (the first occurrence of the label).
        let (x, y) = picker_label_pos(&mut modal, &mut ctx, "Play");
        modal
            .handle_mouse_event(
                MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::DoubleClick,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .expect("double-click play");
        let got = confirmed.lock().unwrap().clone().expect("play confirmed");
        assert_eq!(
            got.1,
            crate::ui::modals::torrent_file_picker::TorrentPickerAction::Play,
            "double-click on Play activates Play"
        );

        // A fresh picker: double-click "Download & Play".
        let confirmed2 = std::sync::Arc::new(std::sync::Mutex::new(None));
        let confirmed2_cb = confirmed2.clone();
        let mut modal2: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                move |_ctx, indices, action| {
                    *confirmed2_cb.lock().unwrap() = Some((indices, action));
                    Ok(())
                },
            ));
        let (x, y) = picker_label_pos(&mut modal2, &mut ctx, "Download");
        modal2
            .handle_mouse_event(
                MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::DoubleClick,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .expect("double-click download & play");
        let got2 = confirmed2.lock().unwrap().clone().expect("download confirmed");
        assert_eq!(
            got2.1,
            crate::ui::modals::torrent_file_picker::TorrentPickerAction::DownloadAndPlay,
            "double-click on Download & Play activates it"
        );

        // A fresh picker: double-click "Cancel" closes without confirming.
        let confirmed3 = std::sync::Arc::new(std::sync::Mutex::new(None));
        let confirmed3_cb = confirmed3.clone();
        let mut modal3: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                vec![
                    (3, "S01E03.mkv".to_owned(), 1_073_741_824),
                    (1, "S01E01.mkv".to_owned(), 524_288_000),
                ],
                move |_ctx, indices, action| {
                    *confirmed3_cb.lock().unwrap() = Some((indices, action));
                    Ok(())
                },
            ));
        let (x, y) = picker_label_pos(&mut modal3, &mut ctx, "Cancel");
        modal3
            .handle_mouse_event(
                MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::DoubleClick,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .expect("double-click cancel");
        assert!(
            confirmed3.lock().unwrap().is_none(),
            "double-click on Cancel closes without confirming"
        );
    }

    #[test]
    fn picker_wheel_scrolls_the_selection_and_scrollbar_click_jumps() {
        // Round-21 user note: the scroll wheel moves the list selection
        // and the scrollbar is mouse-interactive (click/drag scrolls).
        use crate::ui::modals::torrent_file_picker::TorrentFilePicker;
        use crate::shared::mouse_event::{MouseEvent, MouseEventKind};
        use crossterm::event::KeyModifiers;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // 20 files overflow the 70x20 test terminal's ~14-row viewport.
        let options: Vec<(usize, String, u64)> = (1..=20)
            .map(|i| (i, format!("S01E{i:02}.mkv"), 1_000 + i as u64))
            .collect();
        let mut modal: Box<dyn crate::ui::modals::Modal + Send + Sync> =
            Box::new(TorrentFilePicker::new(
                &ctx,
                "▶ files — Fake Pack ",
                options,
                |_ctx, _indices, _action| Ok(()),
            ));
        assert!(
            picker_buffer_contains(&mut modal, &mut ctx, "S01E01.mkv"),
            "the first file is visible at the top"
        );

        // Wheel down over the list moves the selection down; after enough
        // wheels the view scrolls and the first file leaves the viewport.
        let wheel_down = MouseEvent {
            x: 35,
            y: 8,
            kind: MouseEventKind::ScrollDown,
            modifiers: KeyModifiers::NONE,
        };
        for _ in 0..16 {
            modal.handle_mouse_event(wheel_down, &mut ctx).expect("wheel down");
        }
        assert!(
            !picker_buffer_contains(&mut modal, &mut ctx, "S01E01.mkv"),
            "the wheel scrolled the selection out of the viewport"
        );

        // A click at the bottom of the scrollbar jumps to the end: the
        // list's rightmost column (x = popup width - 1), bottom row.
        // popup: height = min(20+5, 18) = 18, so y = (20-18)/2 = 1; the
        // list area is 15 rows tall and the scrollbar column is its last.
        let sb_x = 69;
        let sb_bottom_y = 1 + 15 - 1; // list rows y = 2..=15 (popup.y+1 .. +14)
        modal
            .handle_mouse_event(
                MouseEvent {
                    x: sb_x,
                    y: sb_bottom_y,
                    kind: MouseEventKind::LeftClick,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .expect("scrollbar click");
        assert!(
            picker_buffer_contains(&mut modal, &mut ctx, "S01E20.mkv"),
            "the scrollbar click jumped to the end of the list"
        );
        assert!(
            !picker_buffer_contains(&mut modal, &mut ctx, "S01E01.mkv"),
            "the end of the list no longer shows the first file"
        );
    }

    #[test]
    fn torrent_popup_shows_disabled_row_when_streaming_is_off() {
        use crate::{
            shared::events::AppEvent,
            ui::UiAppEvent,
        };
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut config = crate::config::Config::default();
        config.torrent.enabled = false;
        ctx.config = std::sync::Arc::new(config);
        show_paste_modal(
            &ctx,
            vec![PastedItem::Torrent("/tmp/movie.torrent".into())],
        );

        let mut modal = match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(AppEvent::UiEvent(UiAppEvent::Modal(modal))) => modal,
            other => panic!("expected the paste modal, got {other:?}"),
        };
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, &mut ctx).expect("paste menu renders"))
            .expect("draw ok");
        let text: String =
            terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();

        // Streaming is off: the torrent is still classified and the section
        // explains why there is no action.
        assert!(text.contains("[Torrent]"), "torrent section in {text:?}");
        assert!(text.contains("Torrent streaming disabled"), "disabled row in {text:?}");
        assert!(!text.contains("Stream"), "no stream action in {text:?}");
        assert!(!text.contains("Download"), "no download action in {text:?}");
    }

    #[test]
    fn ignores_non_audio_urls() {
        assert!(parse_paste("https://example.com/page").is_empty());
        assert!(parse_paste("https://example.com/song.png").is_empty());
    }

    #[test]
    fn parses_multiple_items_from_lines() {
        let dir = std::env::temp_dir().join(format!("paste-multi-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let local = dir.join("a.mp3");
        std::fs::write(&local, b"x").unwrap();
        let items = parse_paste(&format!(
            "https://youtu.be/abc\n{}\nhttps://example.com/b.flac\njunk text",
            local.display()
        ));
        assert_eq!(items.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_local_audio_file_with_unescaping() {
        let dir = std::env::temp_dir().join(format!("paste-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("My Song.flac");
        std::fs::write(&path, b"x").unwrap();
        let escaped = path.to_string_lossy().replace(' ', "\\ ");
        let items = parse_paste(&escaped);
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PastedItem::File(p) if p == &path.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_missing_local_files() {
        assert!(parse_paste("/nonexistent/definitely-not-here.mp3").is_empty());
    }

    #[test]
    fn dedupes_items() {
        let items = parse_paste("https://youtu.be/abc https://youtu.be/abc");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn is_audio_extension_cases() {
        assert!(is_audio_extension("x.MP3"));
        assert!(is_audio_extension("x.flac"));
        assert!(!is_audio_extension("x.mp4"));
    }

    #[test]
    fn mpv_state_file_carries_the_reattach_fields() {
        // A later s2udio instance reattaches to a still-playing mpv session
        // from this file: item id, volume and the playlist must be there.
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, _work_rx) = crossbeam::channel::unbounded();
        let (client_tx, _client_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, _work_rx),
            (client_tx, _client_rx),
        );
        let dir = std::env::temp_dir().join(format!("mpv-state-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut config = crate::config::Config::default();
        config.cache_dir = Some(dir.clone());
        ctx.config = std::sync::Arc::new(config);
        ctx.mpv.active = true;
        ctx.mpv.item_id = Some("0123456789abcdef0123456789abcdef".to_owned());
        ctx.mpv.volume = Some(71);
        ctx.mpv.playlist_pos.set(Some(1));
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "ep one", "https://s/Videos/0123456789abcdef0123456789abcdef/stream",
            Some(1200.0),
        ));
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "ep two", "https://s/Videos/0123456789abcdef0123456789abcdef/stream?x=1", None,
        ));

        write_mpv_mpris_state(&ctx);
        let path = mpv_mpris_state_path(Some(&dir));
        let content = std::fs::read_to_string(&path).expect("state file written");
        let state: serde_json::Value =
            serde_json::from_str(&content).expect("state file is valid JSON");
        assert_eq!(state["item_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(state["volume"], 71);
        assert_eq!(state["playlist_pos"], 1);
        assert_eq!(state["playlist"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(state["playlist"][1]["title"], "ep two");
        assert_eq!(state["socket"], "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn video_playlist_uris_keeps_paths_and_links() {
        let uris = video_playlist_uris(&[
            PastedItem::VideoFile("/tmp/vid.mp4".into()),
            PastedItem::VideoUrl("https://example.com/movie.mkv?x=1".into()),
            PastedItem::Yt("https://youtu.be/abc".into()),
        ]);
        assert_eq!(
            uris,
            vec![
                "/tmp/vid.mp4",
                "https://example.com/movie.mkv?x=1",
                "https://youtu.be/abc"
            ]
        );
    }

    #[test]
    fn playlist_audio_uris_splits_direct_and_yt() {
        let (direct, yt) = playlist_audio_uris(&[
            PastedItem::File("/music/song.mp3".into()),
            PastedItem::Url("https://example.com/song.mp3".into()),
            PastedItem::Yt("https://youtu.be/abc".into()),
            PastedItem::Yt("https://youtu.be/def".into()),
        ]);
        assert_eq!(direct, vec!["/music/song.mp3", "https://example.com/song.mp3"]);
        assert_eq!(yt, vec!["https://youtu.be/abc", "https://youtu.be/def"]);
    }

    /// A minimal in-process fake MPD: a Unix listener answering every
    /// command with OK, recording the received command lines. Returns the
    /// received commands after `f` ran.
    fn run_against_fake_mpd(
        f: impl FnOnce(&mut crate::mpd::client::Client<'_>),
    ) -> Vec<String> {
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::net::UnixListener,
            sync::atomic::{AtomicUsize, Ordering},
        };
        // Unique per invocation: the tests share the process (same pid), so
        // a fixed path would collide when two of them run in one suite.
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "s2u-fake-mpd-{}-{}.sock",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let received: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let received2 = received.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut read = BufReader::new(stream.try_clone().unwrap());
            stream.write_all(b"OK MPD 0.24.0\n").unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                let n = read.read_line(&mut line).unwrap_or(0);
                if n == 0 {
                    break;
                }
                let cmd = line.trim().to_string();
                received2.lock().unwrap().push(cmd.clone());
                match cmd.as_str() {
                    "commands" => {
                        stream
                            .write_all(
                                b"command: add\ncommand: addid\ncommand: playlistadd\ncommand: save\ncommand: playlistclear\ncommand: listplaylists\nOK\n",
                            )
                            .unwrap();
                    }
                    "command_list_begin" => {
                        loop {
                            line.clear();
                            let n = read.read_line(&mut line).unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            let c = line.trim().to_string();
                            received2.lock().unwrap().push(c.clone());
                            if c == "command_list_end" {
                                break;
                            }
                        }
                        stream.write_all(b"OK\n").unwrap();
                    }
                    _ => {
                        stream.write_all(b"OK\n").unwrap();
                    }
                }
            }
        });
        let mut client = crate::mpd::client::Client::init(
            crate::config::MpdAddress::SocketPath(path.clone().to_string_lossy().into_owned()),
            None,
            "test",
            None,
            false,
        )
        .unwrap();
        f(&mut client);
        drop(client);
        let _ = std::fs::remove_file(&path);
        let _ = server.join();
        std::sync::Arc::try_unwrap(received)
            .ok()
            .and_then(|m| m.into_inner().ok())
            .unwrap_or_default()
    }

    #[test]
    fn resolved_streams_add_to_playlist_issues_playlistadd() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, _work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, _work_rx),
            (client_tx.clone(), client_rx.clone()),
        );
        let info = vec![crate::shared::ytdlp::YtStreamInfo {
            url: "https://rr4.example/audio.m4a".to_owned(),
            title: "Some Mix".to_owned(),
            ..Default::default()
        }];
        apply_resolved_streams(&ctx, info, YtAction::AddToPlaylist("favs".to_owned()), vec![]);

        let req = client_rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .expect("a command is queued");
        let crate::shared::events::ClientRequest::Command(command) = req else {
            panic!("expected a Command request");
        };
        let received = run_against_fake_mpd(|client| {
            (command.callback)(client).expect("playlistadd command succeeds");
        });
        assert!(
            received.iter().any(|l| l == "playlistadd \"favs\" \"https://rr4.example/audio.m4a\""),
            "expected playlistadd into favs, got: {received:?}"
        );
    }

    #[test]
    fn resolved_streams_create_playlist_issues_save_and_playlistadd() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, _work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, _work_rx),
            (client_tx.clone(), client_rx.clone()),
        );
        let info = vec![crate::shared::ytdlp::YtStreamInfo {
            url: "https://rr4.example/audio.m4a".to_owned(),
            title: "Some Mix".to_owned(),
            ..Default::default()
        }];
        apply_resolved_streams(
            &ctx,
            info,
            YtAction::CreatePlaylist("new mix".to_owned()),
            vec![],
        );

        let req = client_rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .expect("a command is queued");
        let crate::shared::events::ClientRequest::Command(command) = req else {
            panic!("expected a Command request");
        };
        let received = run_against_fake_mpd(|client| {
            (command.callback)(client).expect("create playlist command succeeds");
        });
        assert!(
            received.iter().any(|l| l == "save \"new mix\""),
            "expected save, got: {received:?}"
        );
        assert!(
            received
                .iter()
                .any(|l| l == "playlistadd \"new mix\" \"https://rr4.example/audio.m4a\""),
            "expected playlistadd into the new playlist, got: {received:?}"
        );
    }
}

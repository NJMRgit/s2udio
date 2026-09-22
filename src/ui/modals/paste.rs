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
        events::{PlaylistAction, PlaylistPick, WorkRequest},
        macros::{modal, status_error, status_info, status_warn},
        mpd_client_ext::{Enqueue, MpdClientExt as _},
        ytdlp::{
            ChapterSection, ReplaceAction, StreamDownloadSpec, StreamIntent, YtDlpContent,
            YtDlpHost, YtDlpItem, YtListMeta, YtStreamInfo, tagged_stream_entry,
            tagged_stream_link, untag_stream_link, yt_video_id,
        },
    },
    ui::modals::{
        input_modal::InputModal,
        menu::{ListSection, modal::MenuModal},
        select_modal::SelectModal,
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
/// Decode `%XX` escapes (UTF-8 aware). Sequences that are malformed — a
/// lone `%`, a non-hex digit, or bytes that do not form valid UTF-8 — are
/// left exactly as they arrived, so a path that legitimately contains a
/// percent sign is never mangled. Round 90.
fn unescape_percent(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
            if let (Some(high), Some(low)) = (hex(bytes[idx + 1]), hex(bytes[idx + 2])) {
                out.push(high * 16 + low);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    match String::from_utf8(out) {
        Ok(decoded) => decoded,
        // Not valid UTF-8 once decoded (a truncated multi-byte escape):
        // keep the text as it was pasted.
        Err(_) => s.to_owned(),
    }
}
/// Split on whitespace while keeping backslash-escaped characters (kitty's
/// drag&drop escapes spaces as `\ `) and single- or double-quoted groups
/// (round 90: `"a path with spaces.flac"` is one token) inside one token.
fn split_unescaped(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
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
        } else if let Some(open) = quote {
            // Inside a quoted group quoting is literal (`'it''s'`); only
            // the matching quote closes it.
            current.push(c);
            if c == open {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            current.push(c);
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
///
/// Round 90: the text is processed one pasted LINE at a time, so an
/// unquoted path that contains spaces can still be recognized as a whole.
pub fn parse_paste(input: &str) -> Vec<PastedItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut seen_magnets = std::collections::HashSet::new();
    for line in input.split('\n') {
        for item in parse_paste_line(line) {
            let is_new = match &item {
                PastedItem::Magnet(magnet) => {
                    let key = magnet_infohash_full(magnet)
                        .unwrap_or_else(|| magnet.clone());
                    seen_magnets.insert(key)
                }
                _ => seen.insert(item.clone()),
            };
            if is_new {
                items.push(item);
            }
        }
    }
    items
}
/// The recognized items of a single pasted line: the whitespace/quoted
/// token split first, and — only when that found nothing at all — the
/// whole line as one token (the unquoted spaced path case, round 90).
fn parse_paste_line(line: &str) -> Vec<PastedItem> {
    let tokens: Vec<String> = split_unescaped(line)
        .iter()
        .filter_map(|raw| {
            let token = strip_quotes(raw).trim();
            (!token.is_empty()).then(|| token.to_owned())
        })
        .collect();
    let mut items = Vec::new();
    for token in &tokens {
        if let Some(item) = classify(token) {
            items.push(item);
        }
    }
    if items.is_empty() {
        return whole_line_item(line).into_iter().collect();
    }
    items
}
/// The whole line as ONE candidate (an unquoted path with spaces): trimmed,
/// quotes stripped, then backslash- and percent-unescaped. Round 90.
fn whole_line_item(line: &str) -> Option<PastedItem> {
    let candidate = strip_quotes(line.trim());
    let candidate = candidate.trim();
    ((candidate.contains(' ') || candidate.contains('\t')) && looks_like_path(candidate))
        .then(|| classify(&unescape_path(&unescape_percent(candidate))))
        .flatten()
}
/// Strip one layer of surrounding quotes (`"…"` or `'…'`).
fn strip_quotes(token: &str) -> &str {
    let trimmed = token.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
}
/// Whether a whole line carries an audio/video/torrent extension (query
/// strings/fragments and a `file://` prefix stripped): the guard that stops
/// an arbitrary prose line (a chat message) from being read as one path.
fn looks_like_path(candidate: &str) -> bool {
    let candidate = candidate.strip_prefix("file://").unwrap_or(candidate);
    let candidate = candidate.split(['?', '#']).next().unwrap_or(candidate);
    is_audio_extension(candidate)
        || is_video_extension(candidate)
        || is_torrent_extension(candidate)
}
/// Classify a single whitespace-separated token.
fn classify(token: &str) -> Option<PastedItem> {
    if token.starts_with("magnet:") {
        return Some(PastedItem::Magnet(token.to_owned()));
    }
    if let Some(rest) = token.strip_prefix("file://") {
        // Round 90: `file://` URIs arrive percent-encoded (spaces as
        // `%20`) — decode before the on-disk checks.
        let path = unescape_path(&unescape_percent(rest));
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
    // The extractors are known-host URLs, so accept them with or without a
    // scheme: links copied out of a chat line, a status bar or a plain-text
    // list often arrive as `youtu.be/ID` / `youtube.com/watch?v=ID`
    // (2026-09-10 — those pasted as nothing before). A local path never
    // parses as one of these hosts, so this cannot swallow file pastes.
    let candidate = if token.contains("://") {
        token.to_owned()
    } else {
        format!("https://{token}")
    };
    if candidate.parse::<YtDlpContent>().is_ok() {
        return Some(PastedItem::Yt(candidate));
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
    // Round 90: a plain path can carry `%XX` escapes too (a path dropped
    // or pasted from a URL-aware source).
    let path = unescape_path(&unescape_percent(token));
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
/// the added videos start playing immediately. A set may mix both kinds
/// (round 89) — see the comment inside.
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
    // Round 89: one video set can mix local videos and YouTube-style links
    // (the paste popup's `Add to queue [> Video]` on a paste that holds
    // both). The links need yt-dlp before mpv can open them — they are
    // resolved on the work thread and land as they arrive — while the local
    // ones enter the video playlist right away.
    let links = yt_urls(vids);
    let local: Vec<PastedItem> = vids
        .iter()
        .filter(|item| !matches!(item, PastedItem::Yt(_)))
        .cloned()
        .collect();
    if !local.is_empty() {
        let entries = video_entries_for(&local);
        crate::core::mpv::add_to_video_playlist(ctx, entries.clone(), after_current);
        // With links in the same set, playback is started by the links'
        // resolve (the local entries precede them, so nothing is skipped).
        if play && links.is_empty() {
            crate::core::mpv::play_video_entries(ctx, entries);
        }
    }
    if !links.is_empty() {
        let action = if play {
            YtAction::AddToVideoQueueAndPlay
        } else if after_current {
            YtAction::AddToVideoQueue
        } else {
            YtAction::AppendVideoQueue
        };
        let _ = ctx
            .work_sender
            .send(WorkRequest::ResolveYtStreams { urls: links, action });
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
/// Ask the work thread for the stream info of pasted links that have none yet
/// (round 78): the download options need their chapter list, and nothing else
/// fetches it before the link is played. The open popup refreshes when the
/// resolve lands, and every link is asked for at most once
/// (`ctx.paste_chapter_warm`), so a failing resolve cannot loop.
fn warm_chapters(ctx: &Ctx, downloads: &[PastedItem]) {
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
    let _ = ctx
        .work_sender
        .send(WorkRequest::ResolveYtStreams {
            urls,
            action: YtAction::Refresh,
        });
}
/// The `Download` row's child list (round 78): audio or video, each with the
/// single-file / all-chapters / chapter-picker choices.
fn download_submenu(ctx: &Ctx, url: String, info: Option<YtStreamInfo>) -> ListSection {
    let mut sub = ListSection::new(ctx.config.theme.current_item_style);
    let audio = download_kind_menu(ctx, url.clone(), info.clone(), true);
    let video = download_kind_menu(ctx, url, info, false);
    sub.add_submenu_item("Audio", audio);
    sub.add_submenu_item("Video", video);
    sub.add_item("Cancel", |_ctx| Ok(()));
    sub
}
/// One download kind's options: `Single File`, `All Chapters` (one file per
/// chapter) and `Chapter(s)` (pick the chapters to save) — the chapter rows
/// appear once the link's stream info is known.
fn download_kind_menu(
    ctx: &Ctx,
    url: String,
    info: Option<YtStreamInfo>,
    audio_only: bool,
) -> ListSection {
    let mut sub = ListSection::new(ctx.config.theme.current_item_style);
    let chapters = info
        .as_ref()
        .map(|info| info.chapters.clone())
        .unwrap_or_default();
    let known = info.is_some();
    let has_chapters = chapters.len() > 1;
    let single_url = url.clone();
    sub.add_item("Single File", move |ctx| {
        queue_stream_download(ctx, &single_url, audio_only, false, ReplaceAction::None);
        Ok(())
    });
    if has_chapters {
        let all_url = url.clone();
        sub.add_item("All Chapters", move |ctx| {
            queue_stream_download(ctx, &all_url, audio_only, true, ReplaceAction::None);
            Ok(())
        });
        let mut picker = ListSection::new(ctx.config.theme.current_item_style);
        for chapter in &chapters {
            picker.add_check_item(chapter.title.clone(), false);
        }
        picker.add_check_buttons("Download", "Cancel");
        let picker_url = url;
        let picker_chapters = chapters;
        picker.check_list(move |ctx, selected, _choice| {
            let sections: Vec<ChapterSection> = selected
                .iter()
                .filter_map(|idx| picker_chapters.get(*idx))
                .map(|chapter| ChapterSection {
                    start_secs: chapter.start_secs,
                    end_secs: chapter.end_secs,
                    title: chapter.title.clone(),
                })
                .collect();
            queue_stream_download_sections(ctx, &picker_url, audio_only, sections);
            Ok(())
        });
        sub.add_submenu_item("Chapter(s)", picker);
    } else if !known {
        sub.header("Resolving chapters…");
    }
    sub
}
/// Round 79: a link is a playlist link when it carries a playlist id
/// (`/playlist?list=…` or `watch?v=…&list=…`).
fn is_playlist_link(url: &str) -> bool {
    matches!(url.parse::<YtDlpContent>(), Ok(YtDlpContent::Playlist(_)))
}

/// Round 82: a playlist's `Download` row — Audio or Video, each with
/// `All files` (save every track of the playlist) and `Select files` (pick
/// which videos to save). Chapters never apply to a playlist's items, so
/// there is no `Single File` / `All Chapters` / `Chapter(s)` row here.
fn playlist_download_menu(ctx: &Ctx, url: String) -> ListSection {
    let mut sub = ListSection::new(ctx.config.theme.current_item_style);
    for (label, audio_only) in [("Audio", true), ("Video", false)] {
        let mut kind = ListSection::new(ctx.config.theme.current_item_style);
        let all_url = url.clone();
        kind.add_item("All files", move |ctx| {
            pick_playlist_items(ctx, &all_url, PlaylistPick::SaveAll { audio_only });
            Ok(())
        });
        let pick_url = url.clone();
        kind.add_item("Select files", move |ctx| {
            pick_playlist_items(ctx, &pick_url, PlaylistPick::SaveSelected { audio_only });
            Ok(())
        });
        kind.add_item("Cancel", |_ctx| Ok(()));
        sub.add_submenu_item(label, kind);
    }
    sub.add_item("Cancel", |_ctx| Ok(()));
    sub
}

/// Round 83: a playlist link's queue rows. The playlist's items are added
/// as YouTube **stream entries** — nothing is downloaded: each item's
/// stream URL is resolved in the background
/// (`PlaylistAction::QueueStreams`) and the entry is added as it lands, so
/// a 10-track playlist is in the queue in seconds instead of after ten
/// full downloads. `audio` queues them for MPD (the audio queue), else for
/// the video playlist; `autoplay` starts the first entry that lands.
fn import_playlist(ctx: &Ctx, url: &str, audio: bool, autoplay: bool) {
    let action = PlaylistAction::QueueStreams { audio, autoplay };
    if let Err(err) = ctx.ytdlp_manager.resolve_playlist(url, action) {
        status_error!("Failed to add the playlist: {err}");
    }
}

/// Round 82: a picker row (`Add to playlist` / `Create Playlist` /
/// `Download > … > All files | Select files`). The playlist's items are
/// listed first — one fast yt-dlp call, they carry their titles — and the
/// picker opens as soon as they are known.
fn pick_playlist_items(ctx: &Ctx, url: &str, kind: PlaylistPick) {
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

/// Round 89: the stored-playlist URIs of a pasted row's **audio** arm -
/// direct files/URLs as they are, YouTube-style links tagged `#s2u-audio`
/// (round 86: the link is stored and resolved when the entry is played).
fn paste_audio_uris(items: &[PastedItem]) -> Vec<String> {
    let (direct, yt) = playlist_audio_uris(items);
    let mut uris = direct;
    uris.extend(yt.iter().map(|url| tagged_stream_link(url, StreamIntent::Audio)));
    uris
}
/// Round 89: the same for a pasted row's **video** arm - the video
/// playlist's own URIs, each tagged `#s2u-video` (the tag is a URL
/// fragment: MPD stores it, yt-dlp ignores it, and the Playlists tab plays
/// such an entry through mpv).
fn paste_video_uris(items: &[PastedItem]) -> Vec<String> {
    video_playlist_uris(items)
        .iter()
        .map(|url| tagged_stream_link(url, StreamIntent::Video))
        .collect()
}
/// Round 89: the `Add to playlist` target picker, shared by a pasted row's
/// Audio and Video arms.
fn paste_add_to_playlist(ctx: &Ctx, uris: Vec<String>) -> Result<()> {
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
fn paste_create_playlist(ctx: &Ctx, uris: Vec<String>) -> Result<()> {
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
                    item, PastedItem::File(_) | PastedItem::Url(_) | PastedItem::VideoFile(_)
                    | PastedItem::VideoUrl(_) | PastedItem::Yt(_)
                )
        })
        .cloned()
        .collect();
    let video: Vec<PastedItem> = items
        .iter()
        .filter(|item| {
            !(playlist_only && matches!(item, PastedItem::Yt(_)))
                && matches!(
                    item, PastedItem::VideoFile(_) | PastedItem::VideoUrl(_) |
                    PastedItem::Yt(_)
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
        .filter(|item| {
            matches!(item, PastedItem::VideoFile(_) | PastedItem::VideoUrl(_))
        })
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
    let download_url = (downloads.len() == 1)
        .then(|| youtube_link_of(&downloads[0]))
        .flatten();
    let download_info = download_url
        .as_deref()
        .and_then(|url| stream_info_for(ctx, url));
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
        .list_section(
            ctx,
            |mut section| {
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
                        let mut play_sub =
                            ListSection::new(ctx.config.theme.current_item_style);
                        let play_audio = queue_audio.clone();
                        play_sub.add_item("Audio", move |ctx| {
                            enqueue_items(ctx, &play_audio, true, true)
                        });
                        let play_video = video.clone();
                        play_sub.add_item("Video", move |ctx| {
                            queue_videos(ctx, &play_video, true, true);
                            Ok(())
                        });
                        play_sub.add_item("Cancel", |_ctx| Ok(()));
                        section.add_submenu_item("Add to queue and play", play_sub);

                        let mut append_sub =
                            ListSection::new(ctx.config.theme.current_item_style);
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

                        let mut add_sub =
                            ListSection::new(ctx.config.theme.current_item_style);
                        let add_audio = paste_audio_uris(&queue_audio);
                        add_sub.add_item("Audio", move |ctx| {
                            paste_add_to_playlist(ctx, add_audio)
                        });
                        let add_video = paste_video_uris(&video);
                        add_sub.add_item("Video", move |ctx| {
                            paste_add_to_playlist(ctx, add_video)
                        });
                        add_sub.add_item("Cancel", |_ctx| Ok(()));
                        section.add_submenu_item("Add to playlist", add_sub);

                        let mut create_sub =
                            ListSection::new(ctx.config.theme.current_item_style);
                        let create_audio = paste_audio_uris(&queue_audio);
                        create_sub.add_item("Audio", move |ctx| {
                            paste_create_playlist(ctx, create_audio)
                        });
                        let create_video = paste_video_uris(&video);
                        create_sub.add_item("Video", move |ctx| {
                            paste_create_playlist(ctx, create_video)
                        });
                        create_sub.add_item("Cancel", |_ctx| Ok(()));
                        section.add_submenu_item("Create Playlist", create_sub);
                    } else {
                        let play_audio = queue_audio.clone();
                        let play_video = queue_video.clone();
                        section = section.item(
                            "Add to queue and play",
                            move |ctx| {
                                if !play_video.is_empty() {
                                    queue_videos(ctx, &play_video, true, true);
                                }
                                if !play_audio.is_empty() {
                                    enqueue_items(ctx, &play_audio, true, true)?;
                                }
                                Ok(())
                            },
                        );
                        let append_audio = queue_audio.clone();
                        let append_video = queue_video.clone();
                        section = section.item(
                            "Add to queue",
                            move |ctx| {
                                if !append_video.is_empty() {
                                    queue_videos(ctx, &append_video, false, false);
                                }
                                if !append_audio.is_empty() {
                                    enqueue_items(ctx, &append_audio, false, false)?;
                                }
                                Ok(())
                            },
                        );
                        // Round 86: entries keep the LINK plus its intent tag -
                        // a resolved stream URL expires within hours. The link
                        // is resolved when the entry is played.
                        let mut entry_uris = paste_audio_uris(&queue_audio);
                        entry_uris.extend(paste_video_uris(&queue_video));
                        let add_uris = entry_uris.clone();
                        section = section.item("Add to playlist", move |ctx| {
                            paste_add_to_playlist(ctx, add_uris)
                        });
                        let create_uris = entry_uris;
                        section = section.item("Create Playlist", move |ctx| {
                            paste_create_playlist(ctx, create_uris)
                        });
                    }
                }
                // (A playlist link's own `Download` row lives in its
                // `[Playlist]` section above.)
                if !playlist_carried
                    && let Some(url) = download_url.clone()
                {
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
                                                    play_torrent_route(
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
                                                    commit_to_daemon(ctx, &dl_item, &dl_key, indices, false)
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
                                                    play_torrent_route(
                                                        ctx,
                                                        &play_item,
                                                        &play_key,
                                                        Vec::new(),
                                                        false,
                                                    )
                                                },
                                            );
                                        let both_item = item.clone();
                                        let both_key = key.clone();
                                        section = section
                                            .item(
                                                "Stream and download",
                                                move |ctx| {
                                                    play_torrent_route(
                                                        ctx,
                                                        &both_item,
                                                        &both_key,
                                                        Vec::new(),
                                                        true,
                                                    )
                                                },
                                            );
                                        let dl_item = item.clone();
                                        let dl_key = key.clone();
                                        section = section
                                            .item(
                                                "Download",
                                                move |ctx| {
                                                    commit_to_daemon(ctx, &dl_item, &dl_key, Vec::new(), false)
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
                        ctx.paste_chapter_warm.borrow_mut().clear();
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
/// The wait window's elapsed counter ("mm:ss", e.g. `00:12`).
fn scan_wait_elapsed(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
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
fn play_torrent_route(
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
        crate::core::dlctl::read_state().as_ref().is_some_and(|state| {
            crate::core::dlctl::job_active_for_infohash(state, hash)
        })
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
        .send(crate::shared::events::AppEvent::TorrentScannedPlay {
            scan,
            file_indices,
        })
        .map_err(|err| anyhow::anyhow!("Failed to start torrent playback: {err}"))?;
    Ok(())
}

// ============================================================================
// Round 54 — downloader daemon client (committed torrent downloads)
// ============================================================================

/// The replacement id of the "Preparing downloader…" wait window: the
/// rebuilt modal (elapsed counter tick) replaces the open one in place.
const DL_WAIT_REPLACEMENT_ID: &str = "dl_wait";
/// An open round-54 download wait ("Preparing downloader…"): a committed
/// enqueue-with-play or a daemon-routed re-stream is waiting for the
/// daemon's response file. The wait shows while the daemon adds the
/// torrent (a cold magnet can take minutes — open-ended, like the scan
/// wait); Esc cancels it (a stop request stops the daemon job).
pub struct DlWaitState {
    /// The request id whose response file the wait polls.
    pub request_id: String,
    /// The open wait modal (popped by the ready/failure path without
    /// running its Esc-cancel close hook).
    pub modal_id: Option<crate::shared::id::Id>,
    /// The wait watcher thread's cancel signal (fired on Esc).
    pub cancel: crossbeam::channel::Sender<()>,
}

/// The wait window's elapsed counter ("mm:ss", same renderer as the scan
/// wait).
fn dl_wait_elapsed(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Open the "Preparing downloader…" wait window and start its watcher
/// thread (`WorkRequest::WatchDlResponse` polls the daemon's response
/// file, sends `DlWaitProgress` every second and `DlWaitReady` when the
/// response lands).
fn open_dl_wait(ctx: &Ctx, request_id: String) {
    let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
    let menu = dl_wait_menu(ctx, request_id.clone(), 0);
    let modal_id = crate::ui::modals::Modal::id(&menu);
    *ctx.dl_wait.borrow_mut() = Some(DlWaitState {
        request_id: request_id.clone(),
        modal_id: Some(modal_id),
        cancel: cancel_tx,
    });
    modal!(ctx, menu);
    let _ = ctx.work_sender.send(WorkRequest::WatchDlResponse {
        request_id,
        cancel: cancel_rx,
    });
}

/// The wait window modal: a live "Preparing downloader… mm:ss" header
/// (rebuilt with a `dl_wait` replacement id so the tick replaces it in
/// place), an "esc to cancel" hint and a Cancel item. Its close hook
/// (Esc / Cancel / any destroy) stops the daemon job and cancels the
/// watcher.
fn dl_wait_menu(ctx: &Ctx, request_id: String, elapsed_secs: u64) -> MenuModal<'static> {
    MenuModal::new(ctx)
        .replacement_id(DL_WAIT_REPLACEMENT_ID)
        .list_section(ctx, |mut section| {
            section.header(format!(
                "Preparing downloader… {}",
                dl_wait_elapsed(elapsed_secs)
            ));
            section.header("esc to cancel");
            section.set_on_close(move |ctx| {
                // The user cancelled the wait: stop the daemon job (the
                // daemon matches it by the creating request id).
                let cancel = ctx.dl_wait.borrow().as_ref().map(|w| w.cancel.clone());
                let _ = crate::core::dlctl::write_stop_request(
                    None,
                    Some(&request_id),
                    None,
                );
                if let Some(cancel) = cancel {
                    let _ = cancel.send(());
                }
                ctx.dl_wait.borrow_mut().take();
            });
            section.add_item("Cancel", |_ctx| Ok(()));
            Some(section)
        })
        .build()
}

/// The wait window's 1 s tick: refresh the open modal's elapsed counter
/// (replacement id — the rebuilt modal replaces the open one in place).
pub fn on_dl_wait_progress(ctx: &Ctx, request_id: &str, elapsed_secs: u64) {
    let is_current = ctx
        .dl_wait
        .borrow()
        .as_ref()
        .is_some_and(|w| w.request_id == request_id);
    if !is_current {
        return;
    }
    let menu = dl_wait_menu(ctx, request_id.to_owned(), elapsed_secs);
    let modal_id = crate::ui::modals::Modal::id(&menu);
    if let Some(wait) = ctx.dl_wait.borrow_mut().as_mut() {
        wait.modal_id = Some(modal_id);
    }
    modal!(ctx, menu);
}

/// A downloader wait finished: the daemon response is ready — close the
/// wait modal (without its Esc hook — the job exists, a stop request must
/// NOT fire) and play through the daemon engine (or report the error).
/// Stale events (the wait was cancelled / a newer wait replaced it) are
/// dropped.
pub fn on_dl_wait_ready(
    ctx: &mut Ctx,
    request_id: &str,
    result: Result<crate::core::dlctl::DlJobResponse, String>,
) {
    let is_current = ctx
        .dl_wait
        .borrow()
        .as_ref()
        .is_some_and(|w| w.request_id == request_id);
    if !is_current {
        return;
    }
    let wait = ctx.dl_wait.borrow_mut().take().expect("the current wait exists");
    if let Some(modal_id) = wait.modal_id {
        let _ = ctx
            .app_event_sender
            .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(modal_id)));
    }
    match result {
        Ok(response) if response.error.is_none() => {
            start_daemon_play(ctx, &response);
        }
        Ok(response) => {
            status_error!("{}", response.error.clone().unwrap_or_else(|| "Downloader failed".to_owned()));
        }
        Err(err) => {
            status_error!("{err}");
        }
    }
}

/// The daemon's start finished on the work thread: a failure aborts an
/// open wait (closes the modal, cancels the watcher, removes the spool
/// request so it cannot fire later); a success is a no-op (the watcher
/// handles the rest).
pub fn on_dl_daemon_started(ctx: &Ctx, result: Result<(), String>) {
    if result.is_ok() {
        return;
    }
    let err = result.unwrap_err();
    let Some(wait) = ctx.dl_wait.borrow_mut().take() else {
        // Download-only commits keep their spool request: a later
        // `s2udio dl start` resumes the download (R1 — the commit must
        // not vanish).
        return;
    };
    let _ = wait.cancel.send(());
    if let Some(path) = crate::core::dlctl::request_path(&wait.request_id) {
        let _ = std::fs::remove_file(path);
    }
    if let Some(modal_id) = wait.modal_id {
        let _ = ctx
            .app_event_sender
            .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(modal_id)));
    }
    status_error!("The downloader daemon failed to start: {err}");
}

/// Round-54 hazard guard (§2.2): before a torrent gets a daemon engine on
/// the cache dir, no TUI engine may still download it. Forget it on the
/// scan engine and on every engine currently plain-streaming it (matched
/// by infohash), and drop the scan entry. A plain stream still playing on
/// a TUI engine stalls briefly — the daemon response replays it through
/// the daemon engine.
fn release_torrent_from_tui(ctx: &Ctx, key: &str, infohash: Option<&str>) {
    // Clone first (round 55 F1): a `borrow()` Ref live across the
    // `borrow_mut()` below panicked with `RefCell already borrowed` — the
    // `if let` scrutinee's temporary lives through the whole block.
    let scanned = ctx.torrent_scans.borrow().get(key).cloned();
    if let Some(Ok(scan)) = scanned {
        let _ = crate::core::torrent::forget_torrent(&scan.engine, &scan.torrent_id);
        ctx.torrent_scans.borrow_mut().remove(key);
        log::debug!(
            key:?; "Forgot the scanned torrent on its TUI engine (daemon takes over)"
        );
    }
    if let Some(hash) = infohash {
        let mut refs = ctx.plain_stream_torrents.borrow_mut();
        refs.retain(|stream| {
            if stream.infohash.as_deref() == Some(hash) {
                let _ = crate::core::torrent::forget_torrent(&stream.engine, &stream.torrent_id);
                log::debug!(
                    hash:?; "Forgot a plain-streamed torrent on its TUI engine (daemon takes over)"
                );
                false
            } else {
                true
            }
        });
    }
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

/// Ensure the downloader daemon is running (the work thread spawns
/// `s2udio dl serve` when it is not; the wait watcher covers the spawn
/// window).
fn ensure_daemon_started(ctx: &Ctx) {
    let running = crate::core::dlctl::read_state()
        .as_ref()
        .is_some_and(crate::core::dlctl::daemon_running);
    if !running {
        let _ = ctx.work_sender.send(WorkRequest::StartDlDaemon);
    }
}

/// A committed torrent action (round 54): enqueue a downloader-daemon job
/// (deduped by infohash — a second committed action extends the existing
/// job's kept-file list) and — for "Stream and download" / the picker's
/// "Download & Play" (`play: true`) — wait for the daemon's response,
/// then play through the daemon engine (survives the TUI, R1). For
/// download-only the Downloads modal shows the progress.
fn commit_to_daemon(
    ctx: &Ctx,
    item: &PastedItem,
    key: &str,
    indices: Vec<usize>,
    play: bool,
) -> Result<()> {
    if !ctx.config.torrent.enabled {
        status_warn!("Torrent streaming is disabled in the config");
        return Ok(());
    }
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    let (infohash, torrent_name, kept) = match &scan {
        Some(scan) => {
            let file_indices: Vec<usize> = if indices.is_empty() {
                scan.pick_playable().into_iter().map(|f| f.index).collect()
            } else {
                indices.iter().filter(|i| **i < scan.files.len()).copied().collect()
            };
            let kept = file_indices
                .iter()
                .filter_map(|i| scan.files.get(*i))
                .map(|f| crate::core::dlctl::DlKeptFile {
                    index: f.index,
                    name: f.name.clone(),
                    length: f.length,
                })
                .collect::<Vec<_>>();
            (scan.info_hash.clone(), Some(scan.torrent_name.clone()), kept)
        }
        None => (None, None, Vec::new()),
    };
    let infohash = infohash.or_else(|| match item {
        PastedItem::Magnet(magnet) => magnet_infohash_full(magnet),
        _ => None,
    });
    release_torrent_from_tui(ctx, key, infohash.as_deref());
    let request_id = crate::core::dlctl::new_request_id();
    let request = crate::core::dlctl::DlJobRequest::Enqueue {
        id: request_id.clone(),
        infohash,
        source_key: key.to_owned(),
        torrent_item: torrent_item(item).into(),
        torrent_name,
        files: kept,
        play,
    };
    crate::core::dlctl::write_request(&request)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    ensure_daemon_started(ctx);
    // Refresh + keep `Ctx.dl_state` fresh while the job runs (the event
    // loop's `DlStatePoll` arm restarts its 1 s guard while jobs are
    // active).
    let _ = ctx
        .app_event_sender
        .send(crate::AppEvent::DlStatePoll);
    close_paste_popup(ctx);
    if play {
        open_dl_wait(ctx, request_id);
    } else {
        let label = scan
            .as_ref()
            .map(|scan| scan.torrent_name.clone())
            .unwrap_or_else(|| item.label());
        status_info!("Downloading {label}… (progress in the Downloads modal)");
    }
    Ok(())
}

/// A plain re-stream of a torrent that has an ACTIVE committed job
/// (round 54, §2.2): playback routes through the daemon engine — no
/// second TUI engine on the cache dir. The stream request does NOT extend
/// the job's kept-file list.
fn stream_via_daemon(ctx: &Ctx, item: &PastedItem, key: &str, indices: Vec<usize>) -> Result<()> {
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    let (infohash, torrent_name) = match &scan {
        Some(scan) => (scan.info_hash.clone(), Some(scan.torrent_name.clone())),
        None => (None, None),
    };
    let infohash = infohash.or_else(|| match item {
        PastedItem::Magnet(magnet) => magnet_infohash_full(magnet),
        _ => None,
    });
    release_torrent_from_tui(ctx, key, infohash.as_deref());
    let request_id = crate::core::dlctl::new_request_id();
    let request = crate::core::dlctl::DlJobRequest::Stream {
        id: request_id.clone(),
        infohash,
        source_key: key.to_owned(),
        torrent_item: torrent_item(item).into(),
        torrent_name,
        file_indices: indices,
    };
    crate::core::dlctl::write_request(&request)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    ensure_daemon_started(ctx);
    let _ = ctx
        .app_event_sender
        .send(crate::AppEvent::DlStatePoll);
    close_paste_popup(ctx);
    open_dl_wait(ctx, request_id);
    Ok(())
}

/// Round 64: stream a currently-downloading daemon job from the Downloads
/// modal (same flow as the paste picker's "Stream" — the engine serves
/// the already-downloaded pieces while the job continues). The request
/// carries the job's own fields (`file_indices` empty = the single best
/// playable file) and does NOT extend the job's kept-file list.
pub(crate) fn stream_daemon_job(ctx: &Ctx, job: &crate::core::dlctl::DlJob) -> Result<()> {
    let request_id = crate::core::dlctl::new_request_id();
    let request = crate::core::dlctl::DlJobRequest::Stream {
        id: request_id.clone(),
        infohash: job.infohash.clone(),
        source_key: job.source_key.clone(),
        torrent_item: job.torrent_item.clone(),
        torrent_name: Some(job.torrent_name.clone()),
        file_indices: Vec::new(),
    };
    crate::core::dlctl::write_request(&request)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    ensure_daemon_started(ctx);
    let _ = ctx
        .app_event_sender
        .send(crate::AppEvent::DlStatePoll);
    open_dl_wait(ctx, request_id);
    Ok(())
}

/// Play a downloader-daemon response (round 54, §2.3): build the mpv
/// entries from the response's stream URLs (userinfo auth), insert the
/// synthetic yt-info entries (title = file name, channel = torrent name —
/// exactly like a TUI torrent play), record the job in the streaming
/// marker (R2.5 — the daemon defers moving its files) and prune TUI
/// engines the commit path left idle.
pub fn start_daemon_play(ctx: &mut Ctx, response: &crate::core::dlctl::DlJobResponse) {
    let entries: Vec<crate::core::mpv::MpvPlaylistEntry> = response
        .files
        .iter()
        .map(|file| {
            crate::core::mpv::MpvPlaylistEntry::new(
                file.name.clone(),
                file.stream_url.clone(),
                None,
            )
        })
        .collect();
    if entries.is_empty() {
        status_warn!("The downloader response had no playable files");
        return;
    }
    ctx.mpv.artist = response.torrent_name.clone();
    remember_torrent_entries(ctx, &response.torrent_name, &entries);
    crate::core::mpv::play_video_entries(ctx, entries);
    // R2.5: while the TUI streams this job, its completed files must not
    // be moved away from under mpv.
    ctx.dl_streaming_jobs.borrow_mut().insert(response.job_id.clone());
    let jobs: Vec<String> = ctx.dl_streaming_jobs.borrow().iter().cloned().collect();
    crate::core::dlctl::write_streaming_marker(&jobs);
    prune_empty_engines(ctx);
    status_info!("Streaming {}…", response.torrent_name);
}

/// The mpv session ended (R2.5): clear the streaming marker so the daemon
/// stops deferring completed moves ("streams over").
pub fn untrack_daemon_streams(ctx: &Ctx) {
    ctx.dl_streaming_jobs.borrow_mut().clear();
    crate::core::dlctl::clear_streaming_marker();
}

/// Kill TUI engines that no longer host any torrent (round 54, §2.4
/// prune): an engine whose `GET /torrents` list is empty has nothing to
/// do — drop its scan entries and, when it is the current engine, clear
/// `Ctx.torrent_engine`. The last `Arc`'s `Drop` kills rqbit (no idle
/// engine lingers).
pub fn prune_empty_engines(ctx: &Ctx) {
    let bases: std::collections::HashSet<String> = {
        let mut bases: Vec<String> = ctx
            .torrent_scans
            .borrow()
            .values()
            .filter_map(|result| result.as_ref().ok())
            .map(|scan| scan.engine.base_url().to_owned())
            .collect();
        if let Some(engine) = ctx.torrent_engine.borrow().as_ref() {
            bases.push(engine.base_url().to_owned());
        }
        bases.into_iter().collect()
    };
    for base in bases {
        let engine = ctx
            .torrent_scans
            .borrow()
            .values()
            .filter_map(|result| result.as_ref().ok())
            .map(|scan| scan.engine.clone())
            .find(|engine| engine.base_url() == base)
            .or_else(|| ctx.torrent_engine.borrow().clone())
            .filter(|engine| engine.base_url() == base);
        let Some(engine) = engine else { continue };
        let empty = crate::core::torrent::list_torrents(&engine)
            .map(|ids| ids.is_empty())
            .unwrap_or(false);
        if !empty {
            continue;
        }
        log::debug!(base:?; "Pruning an idle TUI torrent engine");
        ctx.torrent_scans.borrow_mut().retain(|_, result| {
            !(result.as_ref().is_ok_and(|scan| scan.engine.base_url() == base))
        });
        if ctx
            .torrent_engine
            .borrow()
            .as_ref()
            .is_some_and(|engine| engine.base_url() == base)
        {
            *ctx.torrent_engine.borrow_mut() = None;
        }
    }
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
        return play_torrent(ctx, std::slice::from_ref(item));
    };
    let mut videos: Vec<(usize, String, u64)> = scan
        .videos()
        .into_iter()
        .map(|f| (f.index, f.name.clone(), f.length))
        .collect();
    videos.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    let title = format!("▶ files — {} ", scan.torrent_name);
    let item = item.clone();
    let key = key.to_owned();
    modal!(
        ctx, crate ::ui::modals::torrent_file_picker::TorrentFilePicker::new(ctx, title,
        videos, move | ctx, indices, action | { let file_indices : Vec < usize > =
        indices.into_iter().filter(| i | * i < scan.files.len()).collect(); let download
        = action == crate
        ::ui::modals::torrent_file_picker::TorrentPickerAction::DownloadAndPlay; let
        result = if file_indices.is_empty() { if download {
            commit_to_daemon(ctx, &item, &key, Vec::new(), true) } else {
            play_torrent_route(ctx, &item, &key, Vec::new(), false) } } else {
            if download { commit_to_daemon(ctx, &item, &key, file_indices.clone(), true) }
            else { play_torrent_route(ctx, &item, &key, file_indices.clone(), false) } };
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
            play_torrent(ctx, std::slice::from_ref(item))
        }
    }
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
        .send(WorkRequest::PlayTorrent {
            item: torrent_item,
        })
        .map_err(|err| anyhow::anyhow!("Failed to request torrent stream: {err}"))?;
    status_info!("Starting torrent stream…");
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
fn enqueue_items(
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
            resolve_tagged_queue_entry(ctx, &tagged, song_id);
        } else {
            status_warn!("Cannot find the queued stream entry to resolve");
        }
    }
    Ok(())
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
/// Round 91b: how long a cached resolve may be reused for a play. The
/// signed stream URL is valid for hours; the bound only keeps a play from
/// reusing a URL that has been sitting around long enough to be a gamble,
/// and a miss simply resolves the link again.
const CACHED_RESOLVE_MAX_AGE_SECS: u64 = 30 * 60;

/// Seconds since the Unix epoch.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
fn fresh_cached_stream_info(
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

pub fn resolve_tagged_queue_entry(ctx: &Ctx, file: &str, song_id: u32) -> bool {
    let Some((link, intent)) = tagged_stream_entry(file) else {
        return false;
    };
    if !intent.is_audio() {
        return false;
    }
    {
        let mut pending = ctx.pending_stream_resolve.borrow_mut();
        // Keep the marker queue-sized: a replaced entry stops matching.
        pending.retain(|(uri, id)| {
            ctx.queue.iter().any(|song| song.id == *id && song.file == *uri)
        });
        if !pending.insert((file.to_owned(), song_id)) {
            return false;
        }
    }
    // Round 91b: reuse a fresh resolve (the paste popup already resolved
    // this link, or an earlier play did) so the stream starts at once.
    if let Some(info) = fresh_cached_stream_info(ctx, &link) {
        log::debug!(link = link.as_str(); "Reusing the cached stream resolve for a queued entry");
        status_info!("Playing the resolved stream…");
        apply_resolved_streams(ctx, vec![info], YtAction::ReplaceAndPlay(song_id), Vec::new());
        return true;
    }
    // Round 95: the queue's Duration column spins for this row until the
    // resolve lands (its duration is unknown until then).
    mark_stream_parse_pending(ctx, std::slice::from_ref(&link));
    if let Err(err) = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
        urls: vec![link],
        action: YtAction::ReplaceAndPlay(song_id),
    }) {
        log::error!(error:? = err; "Failed to request stream resolution");
        return false;
    }
    status_info!("Resolving the stream…");
    true
}
/// Round 74 (74-1): remember the start offsets carried by the pasted links
/// for the streams that are about to be played through MPD.
///
/// MPD rejects a seek on a stream that is not playing yet, so the offset
/// cannot ride along with `playid`/`add`. It is armed here — keyed by the
/// stream URL, which is exactly the song file MPD reports for the entry —
/// and applied by the event loop on a later status update (see
/// `apply_pending_start_seek` in `core/event_loop.rs`). Arming rather than
/// seeking immediately also covers an entry that is only *queued* now: the
/// offset is still waiting when the song finally starts.
///
/// `ReplaceAndPlay` (an expired stream URL re-resolved mid-playback) is
/// deliberately not armed: that entry is already playing somewhere past the
/// original offset, and jumping it back to the pasted timestamp would be a
/// regression.
fn arm_start_offsets(ctx: &Ctx, info: &[crate::shared::ytdlp::YtStreamInfo]) {
    let mut pending = ctx.pending_start_seek.borrow_mut();
    for item in info {
        if let Some(secs) = item.start_secs.filter(|secs| *secs > 0.0) {
            log::debug!(url = item.url.as_str(), seconds = secs; "Arming the pasted link's start offset");
            pending.insert(item.url.clone(), (secs, 0, None));
        }
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
            arm_start_offsets(ctx, &info);
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
                    entry.start_secs = item.start_secs;
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
            arm_start_offsets(ctx, &info);
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
            arm_start_offsets(ctx, &info);
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
                arm_start_offsets(ctx, &info);
            }
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
            status_info!(
                "{}",
                if first_play {
                    "Resolved the stream — playing it"
                } else {
                    "Stream URL expired — re-resolved from the original link"
                }
            );
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
                    entry.start_secs = item.start_secs;
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
/// Round 95: the key a YouTube-style link is remembered and looked up under —
/// the video id when the link names one (`watch?v=ID`, `youtu.be/ID`,
/// `…&list=…`, `shorts/ID` all name the same video), else the link itself.
/// One video therefore matches however its link is spelled.
fn stream_meta_key(link: &str) -> String {
    let link = untag_stream_link(link);
    yt_video_id(link).unwrap_or_else(|| link.to_owned())
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
        (link != file)
            .then(|| crate::ui::panes::playlists::stream_info(ctx, link))
            .flatten()
    });
    if let Some(info) = resolved.filter(|info| {
        !info.title.is_empty() || info.duration.is_some()
    }) {
        return Some(YtListMeta {
            title: info.title,
            channel: info.channel,
            duration: info.duration,
        });
    }
    ctx.yt_list_meta.borrow().get(&stream_meta_key(link)).cloned()
}
/// Round 95b (user): while a track plays, resolve the queue entry **after**
/// it when that entry is still an unresolved audio link. MPD cannot open a
/// link, so without this it skips past the following entries until the app's
/// resolve lands (~1-2 s later) — with the look-ahead the resolve is already
/// cached when MPD reaches the entry, and the app replaces it in place
/// immediately (round 91b reuses a fresh cached resolve).
///
/// Only the **next** entry is warmed (never the rest of the playlist), and
/// only when nothing is known for it yet: a fresh cached resolve, a resolve
/// already in flight, or a non-link entry all leave the queue alone.
pub fn warm_next_queue_link(ctx: &Ctx) {
    let Some((idx, _)) = ctx.find_current_song_in_queue() else { return };
    let Some(next) = ctx.queue.get(idx + 1) else { return };
    let Some((link, intent)) = tagged_stream_entry(&next.file) else { return };
    if !intent.is_audio() {
        return;
    }
    if fresh_cached_stream_info(ctx, &link).is_some() {
        return;
    }
    if stream_parse_pending(ctx, &next.file) {
        return;
    }
    log::debug!(link = link.as_str(); "Warming the next queue entry's stream while the current one plays");
    mark_stream_parse_pending(ctx, std::slice::from_ref(&link));
    if let Err(err) = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
        urls: vec![link],
        action: YtAction::Refresh,
    }) {
        log::error!(error:? = err; "Failed to request the next entry's stream resolution");
    }
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
fn enqueue_stream_links(
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
                resolve_tagged_queue_entry(ctx, &link, song_id);
            }
            _ => status_warn!("Cannot find the queued stream entry to resolve"),
        }
    }
    Ok(())
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
/// Make sure the chapters of the current song are known: sync from the
/// resolved YouTube info, or fetch them (Jellyfin items via the API, local
/// files via ffprobe). Called on song change / startup.
pub fn ensure_chapters(ctx: &Ctx) {
    let Some((_, song)) = ctx.find_current_song_in_queue() else { return };
    if ctx.chapters.borrow().contains_key(&song.file) {
        return;
    }
    if let Some(yt) = ctx.yt_info.borrow().get(&song.file) && !yt.chapters.is_empty() {
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
        // Round 91: never advertise a poster that is no longer on disk - the
        // media controls would otherwise keep a dead file:// URL until the
        // next fetch.
        .filter(|p| p.exists())
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
    replace: ReplaceAction,
) {
    queue_stream_download_with(
        ctx,
        original_url,
        audio_only,
        split_chapters,
        replace,
        Vec::new(),
    );
}
/// Queue the download of selected chapter ranges of a ytdlp stream
/// (round 78): one file per range, named after the chapter (the yt-dlp
/// plumbing lives in `YtDlp::download_stream`).
pub fn queue_stream_download_sections(
    ctx: &Ctx,
    original_url: &str,
    audio_only: bool,
    sections: Vec<ChapterSection>,
) {
    if sections.is_empty() {
        status_warn!("No chapters selected");
        return;
    }
    queue_stream_download_with(
        ctx,
        original_url,
        audio_only,
        false,
        ReplaceAction::None,
        sections,
    );
}
/// The yt-dlp item of a downloadable link, or `None` when the link is not one
/// yt-dlp can save: a playlist-only link (nothing to save on its own) or a
/// non-YouTube/Soundcloud link. Round 79: a playlist link that names a video
/// (`watch?v=ID&list=…`) downloads THAT video — the item gets the bare watch
/// id, not the raw link (which would save the whole playlist into one file).
fn ytdlp_item_of(original_url: &str) -> Option<YtDlpItem> {
    match original_url.parse::<YtDlpContent>() {
        Ok(YtDlpContent::Single(item)) => Some(item),
        Ok(YtDlpContent::Playlist(_)) => yt_video_id(original_url).map(|id| YtDlpItem {
            filename: id.clone(),
            id,
            kind: YtDlpHost::Youtube,
            title: None,
            ..Default::default()
        }),
        Err(_) => None,
    }
}
/// The common body of the stream-download requests: yt-dlp needs the original
/// link, not the resolved stream URL. `sections` is empty for a whole-media
/// download.
fn queue_stream_download_with(
    ctx: &Ctx,
    original_url: &str,
    audio_only: bool,
    split_chapters: bool,
    replace: ReplaceAction,
    sections: Vec<ChapterSection>,
) {
    use crate::shared::ytdlp::StreamDownloadSpec;
    let Some(output_dir) = downloads_dir() else {
        status_warn!("Cannot determine the downloads folder (~/Downloads)");
        return;
    };
    let Some(item) = ytdlp_item_of(original_url) else {
        status_warn!(
            "Cannot download '{original_url}': not a YouTube/Soundcloud/NicoVideo link \
             (a playlist-only link needs 'Download > All files')"
        );
        return;
    };
    let chapter_count = sections.len();
    let spec = StreamDownloadSpec {
        output_dir,
        audio_only,
        split_chapters,
        on_complete: replace,
        sections,
    };
    ctx.ytdlp_manager.queue_stream_download(item, spec);
    if chapter_count > 0 {
        status_info!(
            "Downloading {chapter_count} chapter(s) of '{original_url}' to s2udio-downloads"
        );
    } else {
        status_info!("Downloading '{}' to s2udio-downloads", original_url);
    }
}

/// One downloadable row of the queue/playlist menus (round 97): the link
/// yt-dlp saves, the label the picker shows and what the file replaces.
///
/// The stream does **not** have to be resolved yet. A playlist import queues
/// intent-tagged links (`watch?v=ID#s2u-audio`) that only resolve when they
/// are played, so their row has no `yt_info` entry; the link itself is what
/// yt-dlp needs, and the listing metadata (`yt_list_meta`) carries the title.
#[derive(Debug, Clone)]
pub struct StreamDownloadTarget {
    /// The original link, intent tag stripped: what `yt-dlp` is run on.
    pub original_url: String,
    /// The row label for the picker (the cached/resolved title, else the
    /// listing title, else the link).
    pub label: String,
    /// The cached stream info when the row was already resolved (it carries
    /// the chapters, so the single-row menu can offer the chapter options).
    pub info: Option<YtStreamInfo>,
    /// What the downloaded file replaces once it lands.
    pub replace: ReplaceAction,
}

impl StreamDownloadTarget {
    /// The stream info the single-row download menu is built from: the cached
    /// one, or a link + label pair for a row that was not resolved yet (no
    /// chapters, so only `Save as audio` / `Save as video`).
    pub fn info(&self) -> YtStreamInfo {
        self.info.clone().unwrap_or_else(|| YtStreamInfo {
            url: self.original_url.clone(),
            original_url: self.original_url.clone(),
            title: self.label.clone(),
            ..Default::default()
        })
    }
}

/// The download target of a queue row or video-playlist entry, or `None` when
/// yt-dlp cannot save it (a local file, a radio/icecast URL, an unknown host).
/// `fallback_label` is used when neither the resolve cache nor the playlist
/// listing knows a title.
pub fn stream_download_target(
    ctx: &Ctx,
    uri: &str,
    fallback_label: &str,
    replace: ReplaceAction,
) -> Option<StreamDownloadTarget> {
    let (original_url, info, title) = match stream_info_for(ctx, uri).or_else(|| {
        // A tagged row that was resolved under its untagged link (a stored
        // playlist entry, or a queue row an intent tag never left): the
        // resolved info still carries the chapters the menu wants.
        let link = crate::shared::ytdlp::untag_stream_link(uri);
        (link != uri).then(|| stream_info_for(ctx, link)).flatten()
    }) {
        Some(info) => (info.original_url.clone(), Some(info.clone()), info.title),
        None => {
            // Not resolved (yet): a playlist import's tagged link, or a plain
            // link that was never played. Strip the intent tag and check that
            // yt-dlp knows the host before offering the row.
            let link = crate::shared::ytdlp::untag_stream_link(uri);
            if link == uri && !crate::ui::panes::radio::is_stream_url(uri) {
                return None;
            }
            ytdlp_item_of(link)?;
            let title = stream_row_meta(ctx, uri).map(|meta| meta.title).unwrap_or_default();
            (link.to_owned(), None, title)
        }
    };
    let label = if !title.trim().is_empty() {
        title
    } else if !fallback_label.trim().is_empty() {
        fallback_label.to_owned()
    } else {
        original_url.clone()
    };
    Some(StreamDownloadTarget {
        original_url,
        label,
        info,
        replace,
    })
}

/// Queue the downloads of several web streams at once (the queue's
/// multi-selection picker): one file per link, in the order they were listed.
/// Each `ReplaceAction` names what that link's file replaces when it lands
/// (the marked queue rows), and `audio_only` comes from the picker's
/// `Audio` / `Video` button. Links yt-dlp cannot save are skipped and
/// reported once.
pub fn queue_stream_downloads(
    ctx: &Ctx,
    targets: Vec<(String, ReplaceAction)>,
    audio_only: bool,
) {
    use crate::shared::ytdlp::StreamDownloadSpec;
    let Some(output_dir) = downloads_dir() else {
        status_warn!("Cannot determine the downloads folder (~/Downloads)");
        return;
    };
    let asked = targets.len();
    let mut queued = 0usize;
    for (original_url, replace) in targets {
        let Some(item) = ytdlp_item_of(&original_url) else {
            log::warn!(url:% = original_url; "Skipping a link yt-dlp cannot download");
            continue;
        };
        ctx.ytdlp_manager.queue_stream_download(
            item,
            StreamDownloadSpec {
                output_dir: output_dir.clone(),
                audio_only,
                split_chapters: false,
                on_complete: replace,
                sections: Vec::new(),
            },
        );
        queued += 1;
    }
    if queued < asked {
        status_warn!("{} of {asked} link(s) cannot be downloaded", asked - queued);
    }
    if queued > 0 {
        status_info!(
            "Downloading {queued} stream(s) to s2udio-downloads as {}",
            if audio_only { "audio" } else { "video" }
        );
    }
}

/// The multi-stream download picker (the queue's Download row when every
/// selected row is a web stream): one checkbox row per stream — all ticked,
/// so the selection is confirmed as-is — and one `Enter` starts the
/// download. `Up`/`Down` (or `w`/`s`) walk the rows and `Space` ticks them;
/// the output kind is the footer button that is activated (`Audio` = one
/// audio file per stream, `Video` = one video file), `Right`/`d` moves along
/// the buttons and `Esc` closes the picker. Each entry carries the queue row
/// its file replaces.
pub fn open_stream_downloads_picker(ctx: &Ctx, streams: Vec<StreamDownloadTarget>) {
    if streams.is_empty() {
        status_warn!("No streams selected");
        return;
    }
    let count = streams.len();
    modal!(
        ctx,
        MenuModal::new(ctx)
            .width(64)
            .title(format!(" Download {count} stream(s) "))
            .list_section(ctx, move |mut section| {
                for stream in &streams {
                    section.add_check_item(stream.label.clone(), true);
                }
                section.add_choice_buttons(&["Audio", "Video"], "Cancel");
                section.set_confirm_on_enter();
                section.check_list(move |ctx, selected, choice| {
                    let targets: Vec<(String, ReplaceAction)> = selected
                        .iter()
                        .filter_map(|idx| streams.get(*idx))
                        .map(|stream| {
                            (stream.original_url.clone(), stream.replace.clone())
                        })
                        .collect();
                    queue_stream_downloads(ctx, targets, choice == 0);
                    Ok(())
                });
                Some(section)
            })
            // `build()` places the cursor on the first row (`modal!` does not
            // call it), so the picker opens ready to walk and tick.
            .build()
    );
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
                // Round 95 (user): no album tag for a YouTube-style stream -
                // it only ever repeated the title, and an empty album is
                // what the media widget should show.
                let _ = client.add_tag_id(song_id, "title", &title);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir with files to paste; `None` when it cannot be created.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "s2udio-paste-{name}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        /// Create an empty file and return its absolute path.
        fn file(&self, name: &str) -> String {
            let path = self.0.join(name);
            std::fs::write(&path, b"").expect("temp file");
            path.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn spaced_path_is_one_token() {
        // Round 90: dragging (or pasting) an unquoted path with spaces did
        // nothing because the tokenizer split it. The whole line is now
        // tried as one path when the split found nothing.
        let dir = TempDir::new("spaced");
        let path = dir.file("my song.flac");
        assert_eq!(parse_paste(&path), vec![PastedItem::File(path.clone())]);
        // Leading directories contain spaces too.
        assert_eq!(
            parse_paste(&format!("  {path}  ")),
            vec![PastedItem::File(path)]
        );
    }

    #[test]
    fn quoted_spaced_path_is_one_token() {
        let dir = TempDir::new("quoted");
        let path = dir.file("a song with spaces.flac");
        assert_eq!(
            parse_paste(&format!("\"{path}\"")),
            vec![PastedItem::File(path.clone())]
        );
        assert_eq!(
            parse_paste(&format!("'{path}'")),
            vec![PastedItem::File(path.clone())]
        );
        // Two quoted paths on one line stay two items.
        let other = dir.file("second song.mp3");
        assert_eq!(
            parse_paste(&format!("\"{path}\" \"{other}\"")),
            vec![PastedItem::File(path), PastedItem::File(other)]
        );
    }

    #[test]
    fn file_uri_decodes_percent_escapes() {
        let dir = TempDir::new("uri");
        let path = dir.file("uri song.flac");
        let encoded = path.replace(' ', "%20");
        assert_eq!(
            parse_paste(&format!("file://{encoded}")),
            vec![PastedItem::File(path)]
        );
        // UTF-8 escapes decode to the original characters.
        let utf8 = dir.file("M\u{e4}rchen.flac");
        let encoded = utf8.replace('\u{e4}', "%C3%A4");
        assert_eq!(
            parse_paste(&format!("file://{encoded}")),
            vec![PastedItem::File(utf8)]
        );
    }

    #[test]
    fn plain_path_decodes_percent_escapes() {
        let dir = TempDir::new("plain");
        let path = dir.file("plain song.flac");
        assert_eq!(
            parse_paste(&path.replace(' ', "%20")),
            vec![PastedItem::File(path.clone())]
        );
        // A malformed escape is left alone instead of being mangled.
        let weird = dir.file("100% song.flac");
        assert_eq!(
            parse_paste(&weird.replace(' ', "%20")),
            vec![PastedItem::File(weird)]
        );
    }

    #[test]
    fn backslash_escaped_path_still_works() {
        // Kitty's legacy drag&drop escaping (`\ ` for a space).
        let dir = TempDir::new("backslash");
        let path = dir.file("escaped song.flac");
        let escaped = path.replace(' ', "\\ ");
        assert_eq!(parse_paste(&escaped), vec![PastedItem::File(path)]);
    }

    #[test]
    fn multi_item_paste_keeps_every_item() {
        // One URL plus one spaced path: the whole-line fallback must not
        // swallow the URL's own line.
        let dir = TempDir::new("multi");
        let path = dir.file("another song.flac");
        let text = format!("https://example.com/song.mp3\n{path}");
        assert_eq!(
            parse_paste(&text),
            vec![
                PastedItem::Url("https://example.com/song.mp3".to_owned()),
                PastedItem::File(path),
            ]
        );
    }

    #[test]
    fn prose_with_spaces_stays_unrecognized() {
        // Guard: a chat line must never be read as one long path.
        assert!(parse_paste("hello there, how are you today?").is_empty());
        assert!(parse_paste("no such audio file here.flac").is_empty());
    }
}

//! mpv session tracking: when s2udio launches mpv for a video, the app
//! mirrors the player's state (title, position, pause) so the now-playing
//! bar, transport controls and seekbar control mpv instead of MPD, and so
//! playback progress can be reported back to Jellyfin.
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use serde::{Deserialize, Serialize};
use crate::ctx::Ctx;
/// One entry of the video playlist, shown in the Queue tab's Video view
/// (and the mpv session's playlist while it plays).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MpvPlaylistEntry {
    /// Display title (episode/movie name, file name, or the URL).
    pub title: String,
    /// The URL/path passed to mpv.
    pub url: String,
    /// Known duration in seconds (None for streams whose length mpv has to
    /// resolve).
    pub duration: Option<f64>,
    /// The canonical YouTube/Soundcloud/NicoVideo link this entry was
    /// resolved from, when known (the resolved stream URL in `url` expires
    /// and carries no video ID). Persisted so title/thumbnail/duration
    /// lookups survive a restart; `None` for local files and entries with
    /// no resolved origin.
    #[serde(default)]
    pub original_url: Option<String>,
}
impl MpvPlaylistEntry {
    pub fn new(
        title: impl Into<String>,
        url: impl Into<String>,
        duration: Option<f64>,
    ) -> Self {
        Self {
            title: title.into(),
            url: url.into(),
            duration,
            original_url: None,
        }
    }
    /// The identity key for yt-info / chapters / thumbnail lookups: the
    /// canonical link when the entry was resolved from one (the resolved
    /// stream URL expires and carries no video ID), else the URL itself.
    pub fn lookup_url(&self) -> &str {
        self.original_url.as_deref().filter(|u| !u.is_empty()).unwrap_or(&self.url)
    }
}
/// The playing state of an mpv video launched from s2udio. Updated by the
/// event loop from mpv's IPC socket; read by the controls/seekbar/info.
#[derive(Debug, Clone, Default)]
pub struct MpvSession {
    pub active: bool,
    /// IPC socket of the running mpv — the fixed `/tmp/mpvsocket` that
    /// SVP4's manager also connects to (s2udio tracks playback over the
    /// same socket mpv exposes for SVP).
    pub socket: Option<PathBuf>,
    /// Jellyfin item id when the video is a Jellyfin stream (else None).
    pub item_id: Option<String>,
    /// The Jellyfin item metadata of the currently playing video (stashed
    /// from the JF_MPRIS/JF_ITEM fetch; feeds the info box).
    pub item: Option<crate::jellyfin::JfItem>,
    pub title: String,
    pub artist: String,
    /// Poster file written for the MPRIS daemon (None until fetched).
    pub art_path: Option<std::path::PathBuf>,
    pub duration: f64,
    pub position: f64,
    pub paused: bool,
    /// mpv's volume (0-100), read by the poll. `None` until the first
    /// read; the volume bar falls back to the MPD volume meanwhile.
    pub volume: Option<u8>,
    /// Resume seek requested before the mpv IPC socket was reachable;
    /// applied by the poll once the socket is live.
    pub pending_seek: Option<f64>,
    /// A playlist switch requested before the mpv IPC socket was reachable
    /// (a video added while the session was still starting): the URLs are
    /// loaded (first replaces, the rest append) by the poll once the
    /// socket is live.
    pub pending_loadfile: std::cell::RefCell<Option<Vec<String>>>,
    /// The playlist mpv was launched with (shown in the Queue tab's Video
    /// view).
    pub playlist: std::cell::RefCell<Vec<MpvPlaylistEntry>>,
    /// The playlist entry currently playing (mpv's `playlist-pos`), read by
    /// the poll.
    pub playlist_pos: std::cell::Cell<Option<usize>>,
}
/// The fixed mpv IPC socket. s2udio launches mpv with
/// `--input-ipc-server=/tmp/mpvsocket` and tracks playback over it — the
/// same socket SVP4's manager connects to for frame interpolation, so one
/// mpv has one socket and both clients talk to it. (Legacy setups without
/// SVP: mpvSockets.lua's per-instance `/tmp/mpvSockets/<pid>` sockets are
/// still discovered as a fallback.)
pub const MPV_SOCKET: &str = "/tmp/mpvsocket";
/// Whether s2udio launched mpv and it is still running.
pub static MPV_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(
    false,
);
/// Pick up a live mpv session left behind by a previous s2udio instance
/// (mpv survives the app's exit; the standalone `s2u-mpv-tracker` daemon
/// keeps the MPRIS state file fresh while the app is closed).
///
/// Returns true when a session was found: `ctx.mpv` is populated from mpv's
/// live state (position/duration/pause/volume) plus the session hints in
/// the state file (title/artist/art/item_id/playlist). The hints are only
/// trusted while the file is fresh, so a stale one from an older session
/// cannot feed a wrong item id into the new session.
pub fn detect_mpv_session(ctx: &mut Ctx) -> bool {
    let Some(socket) = mpv_socket() else { return false };
    detect_mpv_session_at(ctx, socket)
}
/// The reattach logic for a known mpv socket, split out so tests can point
/// it at a fake socket (the public [`detect_mpv_session`] discovers the
/// real one).
fn detect_mpv_session_at(ctx: &mut Ctx, socket: std::path::PathBuf) -> bool {
    use crate::ui::modals::paste::mpv_mpris_state_path;
    let Some((position, paused, duration, volume, playlist_pos, _)) = read_mpv_state(
        &socket,
    ) else {
        return false;
    };
    let state_path = mpv_mpris_state_path(ctx.config.cache_dir.as_deref());
    let fresh = std::fs::metadata(&state_path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() < 15));
    let mut title = read_mpv_title(&socket).unwrap_or_default();
    let mut artist = String::new();
    let mut item_id = None;
    let mut art_path = None;
    let mut playlist: Vec<MpvPlaylistEntry> = Vec::new();
    let mut restored_pos = None;
    if fresh {
        if let Ok(content) = std::fs::read_to_string(&state_path)
            && let Ok(state) = serde_json::from_str::<serde_json::Value>(&content)
        {
            title = state
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&title)
                .to_owned();
            artist = state
                .get("artist")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            item_id = state
                .get("item_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let art = state.get("art").and_then(|v| v.as_str()).unwrap_or_default();
            if !art.is_empty() {
                art_path = Some(std::path::PathBuf::from(art));
            }
            if let Some(entries) = state.get("playlist").and_then(|v| v.as_array()) {
                playlist = entries
                    .iter()
                    .filter_map(|e| {
                        let mut entry = MpvPlaylistEntry::new(
                            e.get("title").and_then(|v| v.as_str()).unwrap_or_default(),
                            e.get("url").and_then(|v| v.as_str()).unwrap_or_default(),
                            e.get("duration").and_then(|v| v.as_f64()),
                        );
                        entry.original_url = e
                            .get("original_url")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned);
                        Some(entry)
                    })
                    .collect();
            }
            restored_pos = state
                .get("playlist_pos")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
        }
    }
    let resolved_pos = restored_pos.or(playlist_pos).unwrap_or(0);
    if playlist.is_empty() {
        playlist = read_mpv_playlist(&socket);
        if item_id.is_none() && let Some(entry) = playlist.first() {
            item_id = crate::jellyfin::item_id_from_url(&entry.url);
        }
    }
    *ctx.mpv.playlist.borrow_mut() = playlist;
    ctx.mpv.playlist_pos.set(Some(resolved_pos));
    ctx.mpv.active = true;
    ctx.mpv.socket = Some(socket);
    ctx.mpv.item_id = item_id;
    ctx.mpv.title = title;
    ctx.mpv.artist = artist;
    ctx.mpv.art_path = art_path;
    ctx.mpv.duration = duration;
    ctx.mpv.position = position;
    ctx.mpv.paused = paused;
    ctx.mpv.volume = volume;
    ctx.mpv.pending_seek = None;
    log::info!(
        title:? = ctx.mpv.title, item_id:? = ctx.mpv.item_id, position:? = ctx.mpv
        .position, duration:? = ctx.mpv.duration; "Reattached to a running mpv session"
    );
    spawn_tracker();
    true
}
/// Discover the running mpv's IPC socket.
///
/// Priority:
/// 1. the fixed [`MPV_SOCKET`] socket (`/tmp/mpvsocket`) when it is live —
///    the socket s2udio passes on launch and SVP4's manager connects to;
/// 2. the newest per-instance socket under `/tmp/mpvSockets` (legacy
///    mpvSockets.lua setups, or an externally launched mpv without the
///    fixed socket);
/// 3. the fixed path even when stale, so callers can detect the dead
///    socket (a stale file outlives a crashed mpv).
pub fn mpv_socket() -> Option<PathBuf> {
    mpv_socket_in(Path::new(MPV_SOCKET), Path::new("/tmp/mpvSockets"))
}
/// [`mpv_socket`] with explicit paths, split out for tests.
fn mpv_socket_in(fixed: &Path, sockets_dir: &Path) -> Option<PathBuf> {
    if is_live_socket(fixed) {
        return Some(fixed.to_path_buf());
    }
    newest_mpv_sockets_socket(sockets_dir)
        .or_else(|| fixed.exists().then(|| fixed.to_path_buf()))
}
/// A Unix socket that accepts connections (mpv's IPC is live when
/// connecting succeeds; a stale file left by a crashed mpv is refused, and
/// a stuck listener times out instead of hanging the caller).
fn is_live_socket(path: &Path) -> bool {
    connect_socket(path, Duration::from_millis(300)).is_ok()
}
/// Newest socket under `sockets_dir` (mpvSockets.lua creates one per
/// instance at `/tmp/mpvSockets/<pid>`).
fn newest_mpv_sockets_socket(sockets_dir: &Path) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(sockets_dir) else {
        return None;
    };
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}
/// Connect to a Unix socket with a `timeout`. A plain blocking connect can
/// hang forever when the socket file is held by a stuck listener (e.g. a
/// thumbfast thumbnailer that inherited a crashed mpv's IPC socket with a
/// full backlog) — that would freeze the app's event loop at startup.
/// Returns the stream in blocking mode (the caller's read timeouts work as
/// before).
fn connect_socket(
    path: &Path,
    timeout: Duration,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::{ffi::CString, os::fd::FromRawFd};
    let cpath = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path has a NUL byte",
            )
        })?;
    let bytes = cpath.as_bytes();
    if bytes.len()
        >= std::mem::size_of_val(
            &unsafe { std::mem::zeroed::<libc::sockaddr_un>() }.sun_path,
        )
    {
        return Err(
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket path too long"),
        );
    }
    let rc = unsafe {
        let fd = libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            addr.sun_path.as_mut_ptr() as *mut u8,
            bytes.len(),
        );
        let connect_rc = libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        if connect_rc == 0 {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
            return Ok(std::os::unix::net::UnixStream::from_raw_fd(fd));
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) => {
                libc::close(fd);
                return Err(err);
            }
            Some(libc::EINPROGRESS) => {}
            _ => {
                libc::close(fd);
                return Err(err);
            }
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let polled = libc::poll(
            &mut pfd,
            1,
            timeout.as_millis().min(i32::MAX as u128) as libc::c_int,
        );
        if polled <= 0 {
            libc::close(fd);
            return Err(
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connect to {} timed out", path.display()),
                ),
            );
        }
        let mut err_code: libc::c_int = 0;
        let mut err_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut err_code as *mut _ as *mut libc::c_void,
            &mut err_len,
        ) != 0
        {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        if err_code != 0 {
            libc::close(fd);
            return Err(std::io::Error::from_raw_os_error(err_code));
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        Ok(std::os::unix::net::UnixStream::from_raw_fd(fd))
    };
    rc
}
/// Send a command to mpv's IPC socket and read up to `max_lines` response
/// lines. A fresh connection is used per call so responses cannot be
/// confused with another client's. `None` when mpv cannot be reached at all
/// (no socket, connection refused, timeout) — callers must not treat that
/// as a successful read of empty data.
fn mpv_exchange(socket: &Path, command: &str, max_lines: usize) -> Option<Vec<String>> {
    let Ok(stream) = connect_socket(socket, Duration::from_millis(300)) else {
        return None;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = (&stream).write_all(command.as_bytes());
    let _ = (&stream).write_all(b"\n");
    let mut reader = std::io::BufReader::new(stream);
    let mut lines = Vec::new();
    for _ in 0..max_lines {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = line.trim();
                if !line.is_empty() {
                    lines.push(line.to_owned());
                }
            }
        }
    }
    Some(lines)
}
/// Read mpv's playback state: (position, paused, duration, volume,
/// playlist-pos, playlist-count). Missing properties come back as
/// errors/null and are skipped.
pub fn read_mpv_state(
    socket: &Path,
) -> Option<(f64, bool, f64, Option<u8>, Option<usize>, Option<usize>)> {
    let command = concat!(
        r#"{"command":["get_property","time-pos"]}"#, "\n",
        r#"{"command":["get_property","pause"]}"#, "\n",
        r#"{"command":["get_property","duration"]}"#, "\n",
        r#"{"command":["get_property","volume"]}"#, "\n",
        r#"{"command":["get_property","playlist-pos"]}"#, "\n",
        r#"{"command":["get_property","playlist-count"]}"#, "\n",
    );
    let lines = mpv_exchange(socket, command, 6)?;
    let mut position = 0.0;
    let mut paused = false;
    let mut duration = 0.0;
    let mut volume = None;
    let mut playlist_pos = None;
    let mut playlist_count = None;
    for (idx, line) in lines.iter().enumerate() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue
        };
        if value.get("error").and_then(|e| e.as_str()) != Some("success") {
            continue;
        }
        let Some(data) = value.get("data") else { continue };
        let number = data.as_f64();
        match idx {
            0 => {
                if let Some(v) = number {
                    position = v;
                }
            }
            1 => {
                if data.is_boolean() {
                    paused = data.as_bool().unwrap_or(false);
                }
            }
            2 => {
                if let Some(v) = number {
                    duration = v;
                }
            }
            3 => {
                if let Some(v) = number && let Ok(v) = u8::try_from(v as i64) {
                    volume = Some(v);
                }
            }
            4 => {
                if let Some(v) = number {
                    let v = v as i64;
                    playlist_pos = (v >= 0).then_some(v as usize);
                }
            }
            5 => {
                if let Some(v) = number {
                    let v = v as i64;
                    playlist_count = (v >= 0).then_some(v as usize);
                }
            }
            _ => {}
        }
    }
    Some((position, paused, duration, volume, playlist_pos, playlist_count))
}
/// Read the entries currently loaded in mpv, live from its IPC: the
/// `playlist` property, falling back to the single playing `path`. Used by
/// [`detect_mpv_session`] to restore the session playlist when the state
/// file is stale or missing (the yt-info / chapters / thumbnail lookups
/// are keyed by the playing URL, so a lost playlist blanks them). A stream
/// entry's `filename` is its URL — the original YouTube/Soundcloud link
/// for a resolved stream — and mpv's `title` is the media title.
pub fn read_mpv_playlist(socket: &Path) -> Vec<MpvPlaylistEntry> {
    let command = concat!(
        r#"{"command":["get_property","playlist"]}"#, "\n",
        r#"{"command":["get_property","path"]}"#, "\n",
    );
    let lines = mpv_exchange(socket, command, 2).unwrap_or_default();
    let mut entries: Vec<MpvPlaylistEntry> = Vec::new();
    let mut path: Option<String> = None;
    for line in lines {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("error").and_then(|e| e.as_str()) != Some("success") {
            continue;
        }
        let Some(data) = value.get("data") else { continue };
        if let Some(list) = data.as_array() {
            for entry in list {
                let url = entry
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !url.is_empty() {
                    let title = entry
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or(url);
                    entries
                        .push(
                            MpvPlaylistEntry::new(title.to_owned(), url.to_owned(), None),
                        );
                }
            }
        } else if let Some(p) = data.as_str().filter(|p| !p.is_empty()) {
            path = Some(p.to_owned());
        }
    }
    if entries.is_empty() && let Some(path) = path {
        entries.push(MpvPlaylistEntry::new(path.clone(), path, None));
    }
    entries
}
/// Read the URL/file mpv is currently playing (`path` property). Used to
/// verify a recorded playlist entry is the entry mpv actually plays before
/// the poll adopts it (a `loadfile … replace` splice can leave mpv's real
/// playlist diverging from the recorded one).
pub fn read_mpv_path(socket: &Path) -> Option<String> {
    let lines = mpv_exchange(socket, r#"{"command":["get_property","path"]}"#, 1)
        .unwrap_or_default();
    lines
        .first()
        .and_then(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value.get("error").and_then(|e| e.as_str()) == Some("success"))
                .then(|| value.get("data")?.as_str().map(str::to_owned))
                .flatten()
        })
}
/// The recorded playlist entry the poll should adopt for mpv's reported
/// `playlist-pos`, or `None` when it cannot be confirmed as what mpv is
/// actually playing.
///
/// The entry is confirmed by comparing its Jellyfin item id with the id
/// of mpv's current `path` when both are Jellyfin streams. mpv's playlist
/// can diverge from the recorded one: `loadfile … replace` splices the new
/// file into the *old* playlist, and when the old and new playlist lengths
/// coincide the poll's count gate alone cannot detect it — following would
/// then index the recorded playlist by a stale mpv position and surface
/// the *next* episode's metadata (+1) while mpv plays the selected one.
/// Only a *confirmed* mismatch (both sides are Jellyfin and disagree) is
/// treated as divergence; an unreadable/unresolvable mpv path keeps the
/// previous positional behavior (YouTube entries carry no item id and are
/// followed by position as before).
pub fn recorded_entry_for_mpv_pos(
    recorded: &[MpvPlaylistEntry],
    pos: usize,
    mpv_path: Option<&str>,
) -> Option<MpvPlaylistEntry> {
    let entry = recorded.get(pos)?.clone();
    let entry_id = crate::jellyfin::item_id_from_url(&entry.url);
    if let Some(entry_id) = entry_id {
        if let Some(path_id) = mpv_path
            .and_then(|p| crate::jellyfin::item_id_from_url(p)) && path_id != entry_id
        {
            return None;
        }
    }
    Some(entry)
}
/// Read mpv's media-title (may be the raw URL for streams).
pub fn read_mpv_title(socket: &Path) -> Option<String> {
    let lines = mpv_exchange(socket, r#"{"command":["get_property","media-title"]}"#, 1)
        .unwrap_or_default();
    lines
        .first()
        .and_then(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value.get("error").and_then(|e| e.as_str()) == Some("success"))
                .then(|| value.get("data")?.as_str().map(str::to_owned))
                .flatten()
        })
}
/// Whether a title still looks like the raw URL — or mpv's provisional
/// title (the URL with the scheme stripped, e.g. `watch?v=…`) — rather
/// than a real media title. The poll keeps re-reading mpv's media-title
/// until it stops looking provisional, so a YouTube video's real title is
/// picked up the moment mpv resolves it (the old one-shot `starts_with(
/// "http")` guard got stuck on the provisional title and never updated).
pub fn is_provisional_title(title: &str) -> bool {
    let t = title.trim();
    t.starts_with("http://") || t.starts_with("https://") || t.starts_with("www.")
        || t.contains("watch?v=") || t.contains("youtu.be/")
        || t.contains("youtube.com/") || t.contains("soundcloud.com/")
        || t.contains("nicovideo.jp/") || is_stream_basename(t)
}
/// Whether `title` looks like the basename of a stream URL (the resolved
/// HLS/audio URL of a YouTube-style link, e.g. `index.m3u8`) rather than
/// a real media title. mpv falls back to this when the stream carries no
/// `media-title`; s2udio must then use the saved entry / yt-info title
/// instead of pushing the basename into MPRIS.
pub fn is_stream_basename(title: &str) -> bool {
    let t = title.trim();
    !t.is_empty() && !t.contains('/') && !t.contains('\\') && !t.contains(' ')
        && ["m3u8", "m3u", "m4a", "mp3", "aac", "opus", "flac", "ogg", "webm"]
            .iter()
            .any(|ext| t.len() > ext.len() + 1 && t.ends_with(&format!(".{ext}")))
}
/// Toggle mpv pause (fire and forget).
pub fn mpv_toggle_pause(socket: &Path) {
    let _ = mpv_exchange(socket, r#"{"command":["cycle","pause"]}"#, 1);
}
/// Seek mpv to an absolute position in seconds.
pub fn mpv_seek(socket: &Path, seconds: f64) {
    let command = format!(r#"{{"command":["set_property","time-pos",{seconds}]}}"#);
    let _ = mpv_exchange(socket, &command, 1);
}
/// Seek mpv relative to the current position (positive = forward).
pub fn mpv_seek_relative(socket: &Path, delta_seconds: f64) {
    let command = format!(r#"{{"command":["seek",{delta_seconds},"relative"]}}"#);
    let _ = mpv_exchange(socket, &command, 1);
}
/// Step a video one frame forward or backward (mpv `frame-step` /
/// `frame-back-step`).
pub fn mpv_frame_step(socket: &Path, forward: bool) {
    let command = if forward {
        r#"{"command":["frame-step"]}"#
    } else {
        r#"{"command":["frame-back-step"]}"#
    };
    let _ = mpv_exchange(socket, command, 1);
}
/// Load a new stream/file in the running mpv instance, replacing the
/// current one (used when the user switches to another video mid-playback).
///
/// NOTE: `loadfile … replace` swaps the *current entry only* — the rest of
/// the old playlist survives behind it (mpv keeps the other entries and
/// the current `playlist-pos`). A caller that wants mpv's playlist to
/// become exactly `entries` must first [`mpv_playlist_clear`] and then
/// reload, or mpv's `playlist-pos`/`playlist-count` desync from the
/// recorded playlist.
pub fn mpv_loadfile(socket: &Path, url: &str) {
    let command = serde_json::json!({ "command" : ["loadfile", url, "replace"] })
        .to_string();
    let _ = mpv_exchange(socket, &command, 1);
}
/// Clear mpv's playlist except the currently played file
/// (`playlist-clear`). Used before reloading a new playlist so the old
/// entries cannot desync mpv's `playlist-pos` from the recorded one.
pub fn mpv_playlist_clear(socket: &Path) {
    let _ = mpv_exchange(socket, r#"{"command":["playlist-clear"]}"#, 1);
}
/// Append a stream/file to the running mpv instance's playlist (after a
/// [`mpv_loadfile`] replace, so a multi-entry switch keeps the sequence).
pub fn mpv_append_load(socket: &Path, url: &str) {
    let command = serde_json::json!({ "command" : ["loadfile", url, "append"] })
        .to_string();
    let _ = mpv_exchange(socket, &command, 1);
}
/// Switch the running mpv instance to another entry of its current
/// playlist (a same-season episode switch; the playlist stays intact so
/// `playlist-pos` keeps tracking correctly).
pub fn mpv_set_playlist_pos(socket: &Path, index: usize) {
    let command = serde_json::json!(
        { "command" : ["set_property", "playlist-pos", index] }
    )
        .to_string();
    let _ = mpv_exchange(socket, &command, 1);
}
/// Unpause mpv (used when restarting the current video from the beginning).
pub fn mpv_unpause(socket: &Path) {
    let command = r#"{"command":["set_property","pause",false]}"#;
    let _ = mpv_exchange(socket, command, 1);
}
/// Quit mpv gracefully (fire and forget).
pub fn mpv_quit(socket: &Path) {
    let _ = mpv_exchange(socket, r#"{"command":["quit"]}"#, 1);
}
/// Ask the running mpv to pause (via its IPC socket).
pub fn pause_mpv() {
    let Some(socket) = mpv_socket() else { return };
    let _ = mpv_exchange(&socket, r#"{"command":["set_property","pause",true]}"#, 1);
}
/// Launch `mpv` on a single URL/path (see [`run_mpv_many`]).
pub fn run_mpv(ctx: &Ctx, url: &str) {
    run_mpv_many(ctx, vec![url.to_owned()]);
}
/// Play `entries` in mpv. When a video is already playing, the running
/// instance is switched to the new entry (loadfile replace + the recorded
/// playlist + the session state) instead of launching a second mpv; only
/// when no session is active (or the session's mpv is gone) is a fresh mpv
/// spawned (see [`run_mpv_playlist`]).
pub fn play_video_entries(ctx: &Ctx, entries: Vec<MpvPlaylistEntry>) {
    let first = entries.first().cloned().unwrap_or_default();
    if !ctx.mpv.active {
        run_mpv_playlist(ctx, entries, None);
        return;
    }
    if let Some(socket) = ctx.mpv.socket.clone() {
        if read_mpv_state(&socket).is_none() {
            log::warn!(
                socket:?; "mpv socket is dead; launching a fresh mpv for the new video"
            );
            run_mpv_playlist(ctx, entries, None);
            return;
        }
        mpv_playlist_clear(&socket);
        mpv_loadfile(&socket, &first.url);
        for entry in entries.iter().skip(1) {
            mpv_append_load(&socket, &entry.url);
        }
    } else {
        *ctx.mpv.pending_loadfile.borrow_mut() = Some(
            entries.iter().map(|e| e.url.clone()).collect::<Vec<_>>(),
        );
    }
    *ctx.mpv.playlist.borrow_mut() = entries;
    ctx.mpv.playlist_pos.set(Some(0));
    let item_id = crate::jellyfin::item_id_from_url(&first.url).unwrap_or_default();
    let _ = ctx
        .app_event_sender
        .send(
            crate::AppEvent::UiEvent(crate::ui::UiAppEvent::MpvItemChanged {
                item_id,
                title: first.title,
            }),
        );
}
/// The index of the currently playing entry in the persistent video
/// playlist, matched by URL. `None` when nothing plays in mpv or the
/// playing entry is not part of the playlist (e.g. a one-off "Play without
/// adding" session).
pub fn video_playlist_current_idx(ctx: &Ctx) -> Option<usize> {
    if !ctx.mpv.active {
        return None;
    }
    let url = ctx
        .mpv
        .playlist
        .borrow()
        .get(ctx.mpv.playlist_pos.get().unwrap_or(0))
        .map(|e| e.url.clone())?;
    ctx.video_playlist.borrow().iter().position(|e| e.url == url)
}
/// Whether the Queue tab's Video view shows the mpv session's own playlist
/// instead of the persistent video playlist: a Jellyfin item *or* a
/// torrent stream is playing (round 17 — a torrent play fills the list
/// with the torrent's files, like a Jellyfin season play; the persistent
/// playlist is left untouched and returns when the session ends). The
/// persistent list cannot host torrent files: their stream URLs embed the
/// rqbit auth token and must never be persisted.
pub fn session_playlist_shown(ctx: &Ctx) -> bool {
    ctx.mpv.active
        && (ctx.mpv.item_id.is_some()
            || ctx
                .mpv
                .playlist
                .borrow()
                .iter()
                .any(|e| crate::core::torrent::is_torrent_stream_url(&e.url)))
}
/// The Jellyfin item metadata arrived for the item currently playing: the
/// session playlist entry (and a matching persistent queue entry) still
/// carries the URL-derived title ("stream"). Update them so the Video view
/// and the serialized state show the real title.
pub fn update_jellyfin_entry_title(ctx: &Ctx, item_id: &str, title: &str) {
    let mut changed = false;
    {
        let mut playlist = ctx.mpv.playlist.borrow_mut();
        if let Some(entry) = playlist
            .iter_mut()
            .find(|e| {
                crate::jellyfin::item_id_from_url(&e.url).as_deref() == Some(item_id)
            }) && entry.title != title
        {
            entry.title = title.to_owned();
            changed = true;
        }
    }
    {
        let mut queue = ctx.video_playlist.borrow_mut();
        if let Some(entry) = queue
            .iter_mut()
            .find(|e| {
                crate::jellyfin::item_id_from_url(&e.url).as_deref() == Some(item_id)
            }) && entry.title != title
        {
            entry.title = title.to_owned();
            changed = true;
        }
    }
    if changed {
        crate::ui::modals::paste::save_video_playlist(ctx);
    }
}
/// Add entries to the persistent video playlist (the Queue tab's Video
/// list, which survives mpv closing and audio playback). When
/// `after_current`, the entries are inserted right after the currently
/// playing entry (or appended when nothing plays); otherwise they are
/// appended at the end. The playlist is persisted, and a live mpv session
/// gets the entries appended so they play after the current one.
pub fn add_to_video_playlist(
    ctx: &Ctx,
    entries: Vec<MpvPlaylistEntry>,
    after_current: bool,
) {
    let entries: Vec<MpvPlaylistEntry> = entries
        .into_iter()
        .filter(|e| !e.url.is_empty())
        .collect();
    if entries.is_empty() {
        return;
    }
    {
        let insert_at = after_current
            .then(|| video_playlist_current_idx(ctx))
            .flatten()
            .map(|i| i + 1);
        let mut playlist = ctx.video_playlist.borrow_mut();
        match insert_at {
            Some(at) => {
                for (offset, entry) in entries.iter().enumerate() {
                    playlist.insert(at + offset, entry.clone());
                }
            }
            None => playlist.extend(entries.iter().cloned()),
        }
    }
    crate::ui::modals::paste::save_video_playlist(ctx);
    if ctx.mpv.active && let Some(socket) = ctx.mpv.socket.clone() {
        for entry in &entries {
            crate::core::mpv::mpv_append_load(&socket, &entry.url);
        }
        ctx.mpv.playlist.borrow_mut().extend(entries.iter().cloned());
    }
    if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Audio {
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);
    }
    log::debug!(added = entries.len(); "Added entries to the video playlist");
}
/// Detach a spawned child from the terminal's session/process group
/// (`setsid`) and ignore SIGHUP, so closing the TUI cannot kill it: the
/// terminal driver sends SIGHUP to the foreground process group on window
/// close, and mpv (plus the tracker daemon) would die with s2udio
/// otherwise. Both must survive the app's exit — that is the whole point
/// of the session. The SIGHUP-ignore is inherited across exec and mpv does
/// not override it (verified), so no SIGHUP delivery path — window close,
/// shell job control, even a direct `kill -HUP` — can take the
/// player/daemon down.
///
/// # Safety
///
/// `pre_exec` runs in the child after fork; only async-signal-safe calls
/// are allowed. `setsid` and `signal` qualify, so the closure is limited
/// to them.
pub(crate) unsafe fn detach_child(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid().map_err(std::io::Error::from)?;
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
}
/// Launch `mpv` on one or more URLs/paths, fully detached from the terminal
/// (no output into the TUI, own session so a terminal close does not kill
/// it). Used for Jellyfin/online video playback; mpv plays the items in
/// sequence. The playlist is recorded on the session so the Queue tab's
/// Video view can show it.
pub fn run_mpv_many(ctx: &Ctx, urls: Vec<String>) {
    let entries: Vec<MpvPlaylistEntry> = urls
        .into_iter()
        .map(|url| MpvPlaylistEntry::new(display_title(&url), url, None))
        .collect();
    run_mpv_playlist(ctx, entries, None);
}
/// A display title for a raw URL/path (the file name, or the URL when
/// nothing else can be derived).
fn display_title(url: &str) -> String {
    let name = url.rsplit('/').next().unwrap_or(url);
    let name = name.split('?').next().unwrap_or(name);
    if name.is_empty() { url.to_owned() } else { name.to_owned() }
}
/// Launch `mpv` on a playlist of entries (titles + URLs), starting at
/// `start_index` when given. Fully detached from the terminal; mpv plays
/// the items in sequence. The playlist is recorded on the session (shown in
/// the Queue tab's Video view).
///
/// Playing video through mpv pauses MPD playback while it runs (resuming it
/// when mpv exits), and starting MPD playback pauses mpv (via its IPC
/// socket) — the two players never run at the same time.
pub fn run_mpv_playlist(
    ctx: &Ctx,
    entries: Vec<MpvPlaylistEntry>,
    start_index: Option<usize>,
) {
    use crate::{
        MpdCommand, mpd::{commands::State, mpd_client::MpdClient},
        shared::events::{AppEvent, ClientRequest},
    };
    *ctx.mpv.playlist.borrow_mut() = entries;
    ctx.mpv.playlist_pos.set(start_index.or(Some(0)));
    crate::ui::modals::paste::clear_mpv_mpris_art(ctx);
    let urls: Vec<String> = ctx
        .mpv
        .playlist
        .borrow()
        .iter()
        .map(|e| e.url.clone())
        .collect();
    let was_playing = ctx.status.state == State::Play;
    use crate::mpd::commands::volume::Bound as _;
    let volume = *ctx.status.volume.value();
    let audio_lang = ctx.config.mpv.audio_lang.clone();
    let subtitles = ctx.config.mpv.subtitles.clone();
    let mpv_bin = ctx.config.mpv.bin.clone();
    let svp = ctx.config.mpv.svp;
    let client_sender = ctx.client_request_sender.clone();
    let event_sender = ctx.app_event_sender.clone();
    log::debug!(urls:?, start_index, mpv_bin:?, svp; "Launching mpv for video playback");
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new(&mpv_bin);
        if svp {
            cmd.arg(format!("--input-ipc-server={MPV_SOCKET}"));
        }
        cmd.arg("--no-terminal");
        cmd.arg(format!("--volume={volume}"));
        if let Some(start) = start_index {
            cmd.arg(format!("--playlist-start={start}"));
        }
        if let Some(alang) = audio_lang.alang() {
            cmd.arg(format!("--alang={alang}"));
        }
        cmd.arg("--subs-fallback-forced=always");
        cmd.arg("--subs-with-matching-audio=no");
        match subtitles {
            crate::config::mpv::MpvSubtitleMode::Hidden => {}
            crate::config::mpv::MpvSubtitleMode::SystemLanguage => {
                cmd.arg("--subs-match-os-language=yes");
            }
            crate::config::mpv::MpvSubtitleMode::Custom { lang } => {
                cmd.arg(format!("--slang={lang}"));
            }
        }
        cmd.arg("--");
        cmd.args(&urls);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(
            std::fs::File::create("/tmp/s2u-mpv-stderr.log")
                .map(std::process::Stdio::from)
                .unwrap_or(std::process::Stdio::null()),
        );
        unsafe { detach_child(&mut cmd) };
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                crate::shared::macros::status_error!("Failed to launch mpv: {}", err);
                return;
            }
        };
        MPV_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
        spawn_tracker();
        let start_url = urls.get(start_index.unwrap_or(0)).cloned().unwrap_or_default();
        let _ = event_sender
            .send(AppEvent::MpvSessionStarted {
                url: start_url,
            });
        if was_playing {
            log::debug!("Pausing MPD while mpv plays");
            let _ = client_sender
                .send(
                    ClientRequest::Command(MpdCommand {
                        callback: Box::new(|client| {
                            client.pause()?;
                            log::debug!("MPD paused by mpv launch");
                            Ok(())
                        }),
                    }),
                );
        }
        let status = child.wait();
        MPV_RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
        log::debug!(status:?; "mpv exited");
        let _ = event_sender.send(AppEvent::MpvSessionEnded);
    });
}
/// Spawn the `s2u-mpv-tracker` daemon (fire and forget): it keeps the MPRIS
/// state file and Jellyfin playback tracking alive when s2udio closes while
/// a video plays. Detached like mpv so it survives the terminal close; its
/// single-instance pid guard makes repeated spawns harmless.
fn spawn_tracker() {
    if let Err(err) = {
        let mut tracker = std::process::Command::new("s2u-mpv-tracker");
        if let Some(cache) = crate::shared::paths::s2udio_cache_dir() {
            tracker.env("S2U_CACHE_DIR", cache);
        }
        tracker
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe { detach_child(&mut tracker) };
        tracker.spawn()
    } {
        log::debug!(error:? = err; "Failed to spawn the mpv tracker daemon");
    }
}
/// Set mpv's volume (0-100), fire and forget.
pub fn mpv_exchange_volume(socket: &Path, volume: f64) {
    let command = format!(r#"{{"command":["set_property","volume",{volume}]}}"#);
    let _ = mpv_exchange(socket, &command, 1);
}
/// Whether the mpv video session is the active UI source (controls bar,
/// seekbar, album art, info/lyrics box). The video is followed while it
/// plays; when MPD playback takes over — the mutual exclusion pauses the
/// video — the UI follows MPD instead, and returns to the video when the
/// music stops (the video stays paused; the transport keys then resume
/// it).
pub fn mpv_is_ui_source(ctx: &Ctx) -> bool {
    ctx.mpv.active
        && (!ctx.mpv.paused || ctx.status.state != crate::mpd::commands::State::Play)
}
/// Turn the subtitles of the running mpv instance off (or back on).
pub fn mpv_set_sub_visibility(socket: &Path, visible: bool) {
    let command = serde_json::json!(
        { "command" : ["set_property", "sub-visibility", visible] }
    )
        .to_string();
    let _ = mpv_exchange(socket, &command, 1);
}
/// Apply the audio-language preference to the running mpv: record it for
/// future loads (`alang`) and re-run the automatic track selection so the
/// current file switches immediately (the `audio` property set to "auto"
/// re-evaluates the preference chain).
pub fn mpv_apply_audio_lang(socket: &Path, pref: &crate::config::mpv::MpvAudioLang) {
    let alang = pref.alang().unwrap_or_default();
    let cmd = serde_json::json!({ "command" : ["set_property", "alang", alang] })
        .to_string();
    let _ = mpv_exchange(socket, &cmd, 1);
    let cmd = serde_json::json!({ "command" : ["set_property", "audio", "auto"] })
        .to_string();
    let _ = mpv_exchange(socket, &cmd, 1);
}
/// Apply the subtitle preference to the running mpv: hidden turns the
/// subtitles off; system/custom record the `slang` preference and re-run
/// the automatic selection.
pub fn mpv_apply_subtitles(socket: &Path, pref: &crate::config::mpv::MpvSubtitleMode) {
    use crate::config::mpv::MpvSubtitleMode;
    match pref {
        MpvSubtitleMode::Hidden => {
            let cmd = serde_json::json!({ "command" : ["set_property", "slang", ""] })
                .to_string();
            let _ = mpv_exchange(socket, &cmd, 1);
            mpv_set_sub_visibility(socket, false);
        }
        MpvSubtitleMode::SystemLanguage => {
            let slang = crate::config::mpv::os_language_code().unwrap_or_default();
            let cmd = serde_json::json!({ "command" : ["set_property", "slang", slang] })
                .to_string();
            let _ = mpv_exchange(socket, &cmd, 1);
            mpv_set_sub_visibility(socket, true);
            let cmd = serde_json::json!({ "command" : ["set_property", "sub", "auto"] })
                .to_string();
            let _ = mpv_exchange(socket, &cmd, 1);
        }
        MpvSubtitleMode::Custom { lang } => {
            let cmd = serde_json::json!({ "command" : ["set_property", "slang", lang] })
                .to_string();
            let _ = mpv_exchange(socket, &cmd, 1);
            mpv_set_sub_visibility(socket, true);
            let cmd = serde_json::json!({ "command" : ["set_property", "sub", "auto"] })
                .to_string();
            let _ = mpv_exchange(socket, &cmd, 1);
        }
    }
}
/// Apply the configured mpv audio/subtitle preferences to the running mpv
/// instance (no-op when mpv is not reachable).
pub fn apply_mpv_prefs_live(ctx: &Ctx) {
    let Some(socket) = ctx.mpv.socket.clone() else { return };
    mpv_apply_audio_lang(&socket, &ctx.config.mpv.audio_lang);
    mpv_apply_subtitles(&socket, &ctx.config.mpv.subtitles);
}
/// Persist the mpv audio/subtitle preferences to state.ron (the Settings
/// panel writes them the same way, so a restart keeps the choice).
pub fn persist_mpv_prefs(ctx: &Ctx) {
    let mut state = crate::config::state::AppStateFile::load();
    state.mpv_audio_lang = Some(ctx.config.mpv.audio_lang.as_str());
    state.mpv_subtitles = Some(ctx.config.mpv.subtitles.as_str());
    if let Err(err) = state.save() {
        log::warn!(error:? = err; "Failed to save mpv preferences");
    }
}
/// The volume the UI should display/control: mpv's while a video plays
/// (falling back to the MPD volume until the first poll read), MPD's
/// otherwise.
pub fn ui_volume(ctx: &Ctx) -> u32 {
    use crate::mpd::commands::volume::Bound as _;
    if mpv_is_ui_source(ctx) {
        u32::from(ctx.mpv.volume.unwrap_or(*ctx.status.volume.value() as u8))
    } else {
        *ctx.status.volume.value()
    }
}
/// Set the volume: mpv when a video is active, MPD otherwise. The mpv
/// poll picks the new value up within ~500 ms and the volume bar follows.
pub fn set_volume(ctx: &Ctx, new_volume: u32) {
    let new_volume = new_volume.clamp(0, 100);
    if mpv_is_ui_source(ctx) && let Some(socket) = ctx.mpv.socket.clone() {
        mpv_exchange_volume(&socket, f64::from(new_volume));
    } else {
        use crate::mpd::mpd_client::{MpdClient, ValueChange};
        ctx.command(move |client| {
            client.volume(ValueChange::Set(new_volume))?;
            Ok(())
        });
    }
}

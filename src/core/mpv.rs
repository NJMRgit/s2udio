//! mpv session tracking: when s2udio launches mpv for a video, the app
//! mirrors the player's state (title, position, pause) so the now-playing
//! bar, transport controls and seekbar control mpv instead of MPD, and so
//! playback progress can be reported back to Jellyfin.
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
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
    /// Round 74 (74-3): start offset in seconds carried by the pasted link
    /// this entry was resolved from (`?t=90`). Applied when mpv opens the
    /// entry: `--start=` on a fresh launch, a seek for a session mpv is
    /// already running. `None` for every other source.
    #[serde(default)]
    pub start_secs: Option<f64>,
    /// A row inserted **the moment a link was pasted**, before `yt-dlp`
    /// finished resolving it: the Video list paints it as a resolving row
    /// (spinner) and the resolve fills its title/URL/duration in place (see
    /// [`complete_video_playlist`]), so a paste shows up immediately instead
    /// of after a silent wait. Not persisted — after a restart the row is
    /// just an unresolved link the warm resolve fills on selection.
    #[serde(skip)]
    pub pending: bool,
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
            start_secs: None,
            pending: false,
        }
    }
    /// The placeholder row a paste inserts before its link is resolved. The
    /// title is the link itself until [`complete_video_playlist`] fills in
    /// the real one, so the row is recognisable (and playable — mpv resolves
    /// a watch link itself) even while `yt-dlp` runs.
    pub fn pending_link(link: &str) -> Self {
        Self {
            title: link.to_owned(),
            url: link.to_owned(),
            duration: None,
            original_url: Some(link.to_owned()),
            start_secs: None,
            pending: true,
        }
    }
    /// The identity key for yt-info / chapters / thumbnail lookups: the
    /// canonical link when the entry was resolved from one (the resolved
    /// stream URL expires and carries no video ID), else the URL itself.
    pub fn lookup_url(&self) -> &str {
        self.original_url.as_deref().filter(|u| !u.is_empty()).unwrap_or(&self.url)
    }
}
/// The answer to a resume prompt that was asked **before** mpv started.
///
/// The prompt is shown by the play path, not by the session-start handler, so
/// a slow answer cannot leave mpv owning the window: mpv is spawned only once
/// the user has chosen. The answer rides this value into the session start,
/// which arms the seek and keeps the post-start fetch from asking again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PendingResume {
    /// "Continue": seek here once the entry is open.
    Continue(f64),
    /// "Start over": play from the beginning, no seek.
    StartOver,
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
    /// A resume seek the Continue button armed: `(seconds, attempts, when
    /// the last attempt was made)`. mpv drops a seek aimed at an entry it
    /// has not finished loading, so the poll retries it until playback has
    /// passed the target (see [`apply_pending_resume_seek`]). A `Cell` so
    /// the `&Ctx` paths that arm it can write it — same as `playlist_pos`.
    pub resume_seek: std::cell::Cell<Option<(f64, u8, Option<std::time::Instant>)>>,
    /// The entry key a Continue / Start over prompt was already shown for,
    /// so the same entry never prompts twice (set when the prompt opens,
    /// cleared when the session starts or mpv advances to another entry).
    /// A `RefCell` (not a `Cell`: `String` is not `Copy`, and `MpvSession`
    /// derives `Clone`).
    pub resume_prompted_for: std::cell::RefCell<Option<String>>,
    /// The answer to a pre-launch resume prompt (see [`PendingResume`]),
    /// together with the Jellyfin item it was asked about: the play path can
    /// take several steps before mpv is spawned (the Home page opens the
    /// library tree first), and a video started some other way in between
    /// must not inherit the position. Taken at session start; cleared when a
    /// session ends.
    pub pending_resume: std::cell::RefCell<Option<(String, PendingResume)>>,
    /// The Jellyfin item whose saved resume position is being fetched. The
    /// `JF_RESUME` handler only acts on a response while the item playing
    /// now is still the one it was asked for, so a request that lost a race
    /// with a playlist advance cannot prompt for the wrong entry.
    pub resume_fetch_item: std::cell::RefCell<Option<String>>,
    /// Round 74 (74-3): start offset still to apply to the entry mpv is on
    /// now — `(playlist position, attempts, when the last one was made)`.
    /// A session that is already running cannot take `--start=` (a launch
    /// argument), and arming `resume_seek` does not work either: the
    /// `MpvItemChanged` event the same action sends clears it before the
    /// poll runs. So the poll applies the offset itself, while the position
    /// is still short of it (see `apply_entry_start_seek`).
    pub start_seek: std::cell::Cell<Option<(usize, u8, Option<std::time::Instant>)>>,
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
/// mpv has one socket and both clients talk to it. It is best-effort: the
/// `mpvSockets.lua` script SVP4's own bundled mpv installs (SVP's
/// `opt.mpv` component, copied into `~/.config/mpv/scripts/` by SVP's
/// `initcfg.sh`) re-points `input-ipc-server` to
/// `/tmp/mpvSockets/<pid>` at startup and leaves the fixed file dead —
/// see [`LAUNCHED_MPV_PID`] and [`mpv_socket`].
pub const MPV_SOCKET: &str = "/tmp/mpvsocket";
/// Whether s2udio launched mpv and it is still running.
pub static MPV_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(
    false,
);
/// The pid of the mpv s2udio launched (`0` = none, e.g. a player started by
/// hand). mpvSockets.lua names its per-instance socket after mpv's own pid,
/// so this makes the socket of *our* player known exactly instead of
/// guessed by mtime — the fixed `/tmp/mpvsocket` is dead whenever that
/// script is installed (SVP4 ships it).
pub static LAUNCHED_MPV_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(
    0,
);
/// Pick up a live mpv session left behind by a previous s2udio instance
/// (mpv survives the app's exit; the standalone `s2u-helper tracker`
/// caretaker daemon keeps the MPRIS state file fresh while the app is
/// closed).
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
    ctx.mpv.resume_seek.set(None);
    *ctx.mpv.resume_prompted_for.borrow_mut() = None;
    *ctx.mpv.resume_fetch_item.borrow_mut() = None;
    *ctx.mpv.pending_resume.borrow_mut() = None;
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
/// 1. `/tmp/mpvSockets/<pid>` of the mpv **s2udio launched** (its pid comes
///    from [`LAUNCHED_MPV_PID`]) when it is live — with mpvSockets.lua
///    installed (SVP4's bundled mpv does) that per-instance socket is the
///    only live one, and knowing the pid keeps the pick exact when several
///    mpv windows run;
/// 2. the fixed [`MPV_SOCKET`] socket (`/tmp/mpvsocket`) when it is live —
///    the socket s2udio passes on launch, and the live one when no
///    mpvSockets.lua is in play;
/// 3. the newest per-instance socket under `/tmp/mpvSockets` (an mpv
///    started by hand, or a session left behind by an earlier s2udio);
/// 4. the fixed path even when stale, so callers can detect the dead
///    socket (a stale file outlives a crashed mpv).
pub fn mpv_socket() -> Option<PathBuf> {
    let pid = LAUNCHED_MPV_PID.load(std::sync::atomic::Ordering::Relaxed);
    mpv_socket_in(
        Path::new(MPV_SOCKET),
        Path::new("/tmp/mpvSockets"),
        (pid != 0).then_some(pid),
    )
}
/// [`mpv_socket`] with explicit paths/pid, split out for tests.
fn mpv_socket_in(fixed: &Path, sockets_dir: &Path, launched_pid: Option<u32>) -> Option<PathBuf> {
    if let Some(pid) = launched_pid {
        let launched = sockets_dir.join(pid.to_string());
        if is_live_socket(&launched) {
            return Some(launched);
        }
    }
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
        // Round 91: a crashed mpv leaves its socket file behind, so picking
        // the newest entry without checking it can return a DEAD socket
        // while the freshly launched mpv is still starting up; the poll then
        // reads nothing and tears the healthy session down (which deletes
        // the MPRIS state file the desktop media controls serve from). Only
        // live sockets qualify.
        if !is_live_socket(&path) {
            continue;
        }
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
fn read_mpv_string_property(socket: &Path, property: &str) -> Option<String> {
    let command = format!(r#"{{"command":["get_property","{property}"]}}"#);
    let lines = mpv_exchange(socket, &command, 1).unwrap_or_default();
    lines
        .first()
        .and_then(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value.get("error").and_then(|e| e.as_str()) == Some("success"))
                .then(|| value.get("data")?.as_str().map(str::to_owned))
                .flatten()
        })
}

/// Read the URL/file mpv is currently playing (`path` property). Used to
/// verify a recorded playlist entry is the entry mpv actually plays before
/// the poll adopts it (a `loadfile … replace` splice can leave mpv's real
/// playlist diverging from the recorded one).
pub fn read_mpv_path(socket: &Path) -> Option<String> {
    read_mpv_string_property(socket, "path")
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
    read_mpv_string_property(socket, "media-title")
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
/// Write `entries` to a temporary `.m3u` carrying their titles (`#EXTINF`).
///
/// Round 68 (host-validated 2026-09-08): mpv v0.41 mis-associates
/// interleaved `--force-media-title=<title> <url>` args — the LAST title
/// lands on an early playlist entry and every other entry gets none (only
/// single-file launches showed the right name). A `.m3u` with `#EXTINF`
/// titles is the reliable carrier for both fresh launches and runtime
/// playlist switches: every entry keeps its correct name in mpv's OSD /
/// playlist.
pub fn write_mpv_m3u(entries: &[MpvPlaylistEntry]) -> PathBuf {
    // A private subdirectory keeps the launch directory clean: mpv v0.41
    // appends ANY sibling .m3u files found next to the playlist file as
    // untitled extra entries (verified 2026-10-08: with a stray sibling the
    // launch plays the extra entry and exits 3 — "some files played, some
    // didn't"), so a fresh launch must see exactly one .m3u. Round 110: empty
    // EVERY .m3u in the directory, not just this app's own name prefix — a
    // second instance, a crashed run or a hand-placed file would otherwise
    // still pollute the playlist.
    let dir = std::env::temp_dir().join("s2u-mpv");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".m3u") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let path = dir.join(format!("s2u-mpv-playlist-{}.m3u", std::process::id()));
    let mut out = String::from("#EXTM3U\n");
    for entry in entries {
        if !entry.title.is_empty() {
            let seconds = entry.duration.map(|d| d.round() as u64).unwrap_or(0);
            // The title follows the first comma (EXTINF syntax); strip
            // line breaks so one entry cannot span multiple lines.
            let title = entry.title.replace(['\r', '\n'], " ");
            out.push_str(&format!("#EXTINF:{seconds},{title}\n"));
        }
        out.push_str(&entry.url);
        out.push('\n');
    }
    if let Err(err) = std::fs::write(&path, out) {
        log::warn!(path:?, error:? = err; "Failed to write the mpv playlist .m3u");
    }
    path
}
/// A process-unique serial for per-launch files (the mpv log).
fn child_serial() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// The first useful error line of mpv's `--log-file`, for the status notice
/// shown when mpv exits without playing. mpv's own log is the only place a
/// stream failure is described: `--no-terminal` (which s2udio always passes)
/// suppresses those messages on stderr.
fn mpv_log_reason(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    // Strip mpv's `[ 1.234][e][component] ` prefix and keep the message.
    let strip = |line: &str| {
        line.split_once("] ")
            .map(|(_, rest)| rest)
            .unwrap_or(line)
            .trim()
            .to_owned()
    };
    // mpv's *error* level wins over a line that merely mentions a failure: a
    // healthy launch logs benign `[v] … Failed sending hook command
    // auto_profiles/…` noise before the real `[e][ffmpeg] … Connection
    // refused`, and the notice must name the latter (host-verified 2026-10-08).
    let error_level = text
        .lines()
        .filter(|line| line.contains("[e]"))
        .map(strip)
        .find(|line| !line.is_empty());
    let mentions_failure = text
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error")
                || lower.contains("failed")
                || lower.contains("unrecognized")
                || lower.contains("could not")
        })
        .map(strip)
        .find(|line| !line.is_empty());
    error_level.or(mentions_failure)
}

/// Replace the running mpv's whole playlist with a titled `.m3u` written by
/// [`write_mpv_m3u`]: `loadfile <m3u> replace` expands the file into its
/// entries, each carrying its `#EXTINF` title.
///
/// URL-only replaces/append cannot do this on mpv v0.41: per-file options
/// over IPC are rejected (`invalid parameter`) and a `loadfile … replace`
/// without a title inherits the stale title of the entry it replaces —
/// that left the OSD on the wrong episode name after a mid-play switch.
pub fn mpv_load_playlist(socket: &Path, m3u: &Path) {
    // `loadlist`, not `loadfile`: loadfile treats the .m3u as a single
    // media file and, once the session's playlist originated from an .m3u,
    // keeps the old playlist-file as a stale untitled entry (verified);
    // `loadlist <file> replace` swaps the whole playlist for the file's
    // titled entries, position 0 = first entry.
    let command = serde_json::json!(
        { "command" : ["loadlist", m3u.to_string_lossy(), "replace"] }
    )
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
/// Launch `mpv` on a single URL with a display title (round 64: Jellyfin
/// single plays used to pass only the URL, so mpv's window showed the raw
/// `stream?static=true&api_key=…` — the title now comes along as
/// `--force-media-title`).
pub fn run_mpv_titled(ctx: &Ctx, url: &str, title: &str) {
    run_mpv_playlist(
        ctx,
        vec![MpvPlaylistEntry::new(title.to_owned(), url.to_owned(), None)],
        None,
    );
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
        // Round 68: switch mpv onto a titled .m3u of the whole list.
        // URL-only appends carry no titles once mpv v0.41 is loaded, and a
        // replace without a title inherits the old current entry's (stale)
        // name — the OSD then shows the wrong episode after a switch.
        let m3u = write_mpv_m3u(&entries);
        mpv_load_playlist(&socket, &m3u);
        // Round 74 (74-3): a pasted link's start offset for the entry mpv
        // just switched to is applied by the poll (see
        // `apply_entry_start_seek`): a live session cannot take `--start=`,
        // and `resume_seek` is cleared by the `MpvItemChanged` event this
        // function sends a few lines below.
        ctx.mpv.start_seek.set(None);
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

/// Round 74 (74-3): attempts the mpv poll makes to place a pasted link's
/// start offset. mpv drops a seek aimed at an entry it has not finished
/// loading, so the poll retries; ~500 ms apart this is a ~10 s window, which
/// also bounds how long the retry can fight a *user* seek backwards past the
/// offset.
const MAX_START_SEEK_ATTEMPTS: u8 = 20;

/// Round 74 (74-3): minimum gap between those attempts (a poll tick is
/// 100 ms while a session is active).
const START_SEEK_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Round 74 (74-3): apply the start offset a pasted link carried (`?t=90`)
/// to the entry mpv is playing now.
///
/// `--start=` covers a fresh launch, but not a session that is already
/// running: `play_video_entries` switches such a session with
/// `loadlist … replace`, and neither `resume_seek` (cleared by the
/// `MpvItemChanged` event the same action sends) nor an immediate seek (mpv
/// drops it while the entry is still loading) can do it. The offset stays on
/// the playlist entry, so the poll applies it here:
///
/// - only while the entry mpv is on is the one that wants an offset, and its
///   position is still short of it — the state is read from the playlist,
///   so an entry that is merely *queued* is left alone;
/// - rate-limited and attempt-bounded, so a seek mpv was not ready for is
///   retried, but a stream that never reaches the offset (or a user who
///   seeked backwards) is not fought indefinitely;
/// - an offset at or past the entry's known duration is dropped: nothing to
///   seek to.
pub fn apply_entry_start_seek(
    ctx: &Ctx,
    socket: &Path,
    position: f64,
    duration: f64,
    playlist_pos: Option<usize>,
) {
    let Some(pos) = playlist_pos else { return };
    let Some(secs) = ctx
        .mpv
        .playlist
        .borrow()
        .get(pos)
        .and_then(|entry| entry.start_secs)
        .filter(|secs| *secs > 0.0)
    else {
        return;
    };
    if position >= secs {
        // There already: the seek landed (or the user got there first). A
        // launch that was given `--start=` reads its position here and is
        // done without a single redundant seek.
        ctx.mpv.start_seek.set(None);
        return;
    }
    if position <= 0.0 {
        // mpv reports no position while it is still opening an entry (and a
        // seek sent then is dropped). Wait until playback has actually
        // started: that is what keeps a fresh launch — whose `--start=`
        // already placed it — from being seeked again while it loads.
        return;
    }
    if duration > 0.0 && secs >= duration {
        // Past the end of the video: no position to start at. Drop it, or
        // the retry would seek to the end of the file on every tick.
        log::debug!(seconds = secs, duration = duration; "Dropping a start offset past the end of the video");
        ctx.mpv.start_seek.set(None);
        return;
    }

    let (attempts, last) = match ctx.mpv.start_seek.get() {
        // A different entry started: this is a fresh offset, and the attempt
        // budget starts over.
        Some((pos_now, attempts, last)) if pos_now == pos => (attempts, last),
        _ => (0, None),
    };
    if attempts >= MAX_START_SEEK_ATTEMPTS {
        ctx.mpv.start_seek.set(None);
        return;
    }
    if last.is_some_and(|at| at.elapsed() < START_SEEK_RETRY_INTERVAL) {
        return;
    }
    ctx.mpv.start_seek.set(Some((pos, attempts + 1, Some(Instant::now()))));
    log::debug!(seconds = secs, attempts = attempts + 1; "Applying the pasted link's start offset in mpv");
    mpv_seek(socket, secs);
}

/// Apply the resume position the Continue button armed (`resume_seek`).
///
/// Same reason as [`apply_entry_start_seek`]: mpv drops a seek aimed at an
/// entry it has not finished loading, so a single seek sent when the socket
/// merely answers is lost and the video restarts from the beginning. The
/// poll therefore applies the target itself, while the entry is loading and
/// its position is still short of it:
///
/// - `position <= 0.0` means mpv is still opening the entry (a seek now is
///   dropped): wait, without spending an attempt;
/// - once playback has passed the target (or come within 5 s of it) the seek
///   landed (or the viewer got there first): done;
/// - a target at or past the entry's known end is dropped;
/// - rate-limited and attempt-bounded, so a stream that never reaches the
///   target is not fought indefinitely.
pub fn apply_pending_resume_seek(ctx: &Ctx, socket: &Path, position: f64, duration: f64) {
    let Some((secs, attempts, last)) = ctx.mpv.resume_seek.get() else {
        return;
    };
    if position > 0.0 && position >= secs - 5.0 {
        ctx.mpv.resume_seek.set(None);
        return;
    }
    if position <= 0.0 {
        // Still opening the entry: a seek sent now is dropped. Position 0 is
        // also what a stream that never starts reports, so the attempt
        // budget is not spent here.
        return;
    }
    if duration > 0.0 && secs >= duration - 5.0 {
        log::debug!(seconds = secs, duration = duration; "Dropping a resume position past the end of the video");
        ctx.mpv.resume_seek.set(None);
        return;
    }
    if attempts >= MAX_START_SEEK_ATTEMPTS {
        ctx.mpv.resume_seek.set(None);
        return;
    }
    if last.is_some_and(|at| at.elapsed() < START_SEEK_RETRY_INTERVAL) {
        return;
    }
    ctx.mpv.resume_seek.set(Some((secs, attempts + 1, Some(Instant::now()))));
    log::debug!(seconds = secs, attempts = attempts + 1; "Applying the saved resume position in mpv");
    mpv_seek(socket, secs);
}

/// Play `entries` starting at `start_index` while KEEPING the whole list.
///
/// Round 69 (user feedback): selecting an episode from the Queue page used
/// to re-queue `entries[start_index..]`, dropping everything before it —
/// the season then lost its earlier episodes and later selections indexed
/// a shrunken list (the "plays the Nth of the remaining queue" drift).
/// A selection now only POSITIONS playback inside the full list: a running
/// mpv that already holds exactly this list is switched in place
/// (`playlist-pos`), otherwise the list is loaded through a titled .m3u
/// (`loadlist replace`) and then positioned at `start_index`. Nothing is
/// cleared in either case.
pub fn play_video_entries_at(
    ctx: &Ctx,
    entries: Vec<MpvPlaylistEntry>,
    start_index: usize,
) {
    let target = entries.get(start_index).cloned().unwrap_or_default();
    if !ctx.mpv.active {
        run_mpv_playlist(ctx, entries.clone(), Some(start_index));
        return;
    }
    if let Some(socket) = ctx.mpv.socket.clone() {
        if read_mpv_state(&socket).is_none() {
            log::warn!(
                socket:?; "mpv socket is dead; launching a fresh mpv for the new video"
            );
            run_mpv_playlist(ctx, entries.clone(), Some(start_index));
            return;
        }
        let recorded: Vec<String> = ctx
            .mpv
            .playlist
            .borrow()
            .iter()
            .map(|e| e.url.clone())
            .collect();
        let target_urls: Vec<String> = entries.iter().map(|e| e.url.clone()).collect();
        if recorded == target_urls && start_index < recorded.len() {
            // Same list is already in mpv: just move playback to the
            // selected entry, the rest of the list stays intact.
            mpv_set_playlist_pos(&socket, start_index);
        } else {
            let m3u = write_mpv_m3u(&entries);
            mpv_load_playlist(&socket, &m3u);
            if start_index < entries.len() {
                mpv_set_playlist_pos(&socket, start_index);
            }
        }
    } else {
        run_mpv_playlist(ctx, entries.clone(), Some(start_index));
    }
    *ctx.mpv.playlist.borrow_mut() = entries;
    ctx.mpv.playlist_pos.set(Some(start_index.min(
        ctx.mpv.playlist.borrow().len().saturating_sub(1),
    )));
    let item_id = crate::jellyfin::item_id_from_url(&target.url).unwrap_or_default();
    let _ = ctx
        .app_event_sender
        .send(
            crate::AppEvent::UiEvent(crate::ui::UiAppEvent::MpvItemChanged {
                item_id,
                title: target.title,
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
/// Fill the placeholder rows a paste inserted ([`MpvPlaylistEntry::pending`])
/// with their resolved entries.
///
/// The rows are matched by their lookup URL (the pasted link), so a resolve
/// that lands seconds later updates the row the user already sees instead of
/// appending a second one: position, order and any marks stay where they
/// were. Both the persistent video playlist and the live mpv-session list are
/// updated; an entry with no placeholder (a resolve that ran for a set with
/// no queued rows) is appended like before.
///
/// Returns `(resolved, appended)`: the resolved entries in the order they
/// were given (what a "and play" action starts) and how many had to be
/// appended because no placeholder was waiting for them.
pub fn complete_video_playlist(
    ctx: &Ctx,
    entries: Vec<MpvPlaylistEntry>,
    after_current: bool,
) -> (Vec<MpvPlaylistEntry>, usize) {
    let mut resolved: Vec<MpvPlaylistEntry> = Vec::with_capacity(entries.len());
    let mut unmatched: Vec<MpvPlaylistEntry> = Vec::new();
    for mut entry in entries {
        entry.pending = false;
        let key = entry.lookup_url().to_owned();
        let mut filled = false;
        // The persistent list (rows queued while no mpv session ran).
        for slot in ctx.video_playlist.borrow_mut().iter_mut() {
            if slot.pending && slot.lookup_url() == key {
                *slot = entry.clone();
                filled = true;
                break;
            }
        }
        // The live session's mirror (rows queued while a video played).
        for slot in ctx.mpv.playlist.borrow_mut().iter_mut() {
            if slot.pending && slot.lookup_url() == key {
                *slot = entry.clone();
                filled = true;
                break;
            }
        }
        if filled {
            resolved.push(entry);
        } else {
            unmatched.push(entry);
        }
    }
    let appended = unmatched.len();
    if appended > 0 {
        add_to_video_playlist(ctx, unmatched, after_current);
    } else {
        crate::ui::modals::paste::save_video_playlist(ctx);
        ctx.render().ok();
    }
    log::debug!(
        filled = resolved.len(), appended; "Completed the video queue's resolving rows"
    );
    (resolved, appended)
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
    // Round 68 (host-validated): the round-64 `--force-media-title`
    // approach only worked for single-file plays — with several
    // interleaved `--force-media-title=<t> <url>` pairs mpv v0.41 puts the
    // LAST title on an early entry and gives the rest none. Hand mpv a
    // `.m3u` carrying the entry titles (`#EXTINF`) instead: every playlist
    // entry keeps its correct name in the window / OSD.
    let m3u = write_mpv_m3u(&ctx.mpv.playlist.borrow());
    // Round 74 (74-3): a pasted link that carried a start offset (`?t=90`)
    // must open at that position; `--start=` applies to the entry mpv opens
    // first, i.e. the one `--playlist-start` jumps to.
    let start_secs = ctx
        .mpv
        .playlist
        .borrow()
        .get(start_index.unwrap_or(0))
        .and_then(|entry| entry.start_secs)
        .filter(|secs| *secs > 0.0);
    let was_playing = ctx.status.state == State::Play;
    use crate::mpd::commands::volume::Bound as _;
    let volume = *ctx.status.volume.value();
    let audio_lang = ctx.config.mpv.audio_lang.clone();
    let subtitles = ctx.config.mpv.subtitles.clone();
    let svp = ctx.config.mpv.svp;
    // SVP4 runs on its own mpv — SVPflow is built against SVP's bundled
    // VapourSynth/Python and is refused by the distro/pipx builds. Every
    // other launch uses the configured `bin` (default `"mpv"`), so s2udio
    // never depends on SVP4 being installed.
    let mpv_bin = ctx.config.mpv.launch_bin().to_owned();
    let client_sender = ctx.client_request_sender.clone();
    let event_sender = ctx.app_event_sender.clone();
    log::debug!(urls:?, start_index, mpv_bin:?, svp; "Launching mpv for video playback");
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new(&mpv_bin);
        if svp {
            cmd.arg(format!("--input-ipc-server={MPV_SOCKET}"));
        }
        cmd.arg("--no-terminal");
        // mpv's own watch_later resume is off. `save-position-on-quit` makes
        // mpv inject a startup seek from its saved state, which is invisible
        // to s2udio and - on a YouTube HLS (m3u8) video stream - makes the
        // video end at once: the window never opens while the audio plays.
        // s2udio applies the resume position itself (`resume_seek`, the
        // tracker's state file), so mpv's copy is redundant as well.
        cmd.arg("--no-resume-playback");
        cmd.arg(format!("--volume={volume}"));
        if let Some(start) = start_index {
            cmd.arg(format!("--playlist-start={start}"));
        }
        if let Some(secs) = start_secs {
            cmd.arg(format!("--start={secs}"));
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
        // The .m3u carries the entry titles (and the per-entry URLs); a
        // single playlist-file argument replaces the interleaved
        // `--force-media-title` pairs. `--` is no longer needed: the URLs
        // live inside the playlist file, not on the command line.
        cmd.arg(&m3u);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(
            std::fs::File::create("/tmp/s2u-mpv-stderr.log")
                .map(std::process::Stdio::from)
                .unwrap_or(std::process::Stdio::null()),
        );
        // Round 110: mpv runs `--no-terminal`, so its OWN messages (the
        // reason a stream failed) go nowhere — the stderr file above only
        // ever carries library noise. `--log-file` writes them regardless,
        // and the exit handler below reports the first error line, so a
        // failed stream stops being silent (see the round-108 known gap).
        let mpv_log = std::env::temp_dir().join(format!("s2u-mpv-{}.log", child_serial()));
        let _ = std::fs::remove_file(&mpv_log);
        cmd.arg(format!("--log-file={}", mpv_log.display()));
        unsafe { detach_child(&mut cmd) };
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                crate::shared::macros::status_error!("Failed to launch mpv: {}", err);
                return;
            }
        };
        MPV_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
        // The player's own pid: mpvSockets.lua (SVP4's bundled mpv installs
        // it) names its socket `/tmp/mpvSockets/<pid>`, which the discovery
        // then uses instead of guessing by mtime.
        LAUNCHED_MPV_PID.store(child.id(), std::sync::atomic::Ordering::Relaxed);
        spawn_tracker();
        let start_url = urls.get(start_index.unwrap_or(0)).cloned().unwrap_or_default();
        let _ = event_sender
            .send(AppEvent::MpvSessionStarted {
                url: start_url.clone(),
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
        LAUNCHED_MPV_PID.store(0, std::sync::atomic::Ordering::Relaxed);
        log::debug!(status:?; "mpv exited");
        // Round 59: mpv exiting non-zero on a YouTube link usually means
        // yt-dlp could not resolve a playable stream (the bgutil PO-token
        // minter's 12 h cliff — see shared::bgutil). Heal the service once
        // so the NEXT playback attempt gets a fresh minter.
        let failed = status
            .as_ref()
            .map(|s| !s.success())
            .unwrap_or(true);
        if failed {
            // Round 110: say WHY. A stream URL the engine rejects (401/404/500)
            // or a player-side load error used to end in a silent one-second
            // mpv exit with no window and no message.
            let reason = mpv_log_reason(&mpv_log).unwrap_or_else(|| "no reason logged".to_owned());
            let code = status
                .as_ref()
                .ok()
                .and_then(|s| s.code())
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_owned());
            log::warn!(
                status:? = status, url:? = start_url.as_str(), reason:? = reason.as_str();
                "mpv exited without playing the stream"
            );
            crate::shared::macros::status_warn!(
                "mpv exited (code {code}) without playing: {reason}"
            );
        }
        if failed && crate::shared::bgutil::is_youtube_url(&start_url) {
            match crate::shared::bgutil::maybe_heal() {
                crate::shared::bgutil::HealOutcome::Restarted => {
                    crate::shared::macros::status_info!(
                        "YouTube stream failed — bgutil token service was stale, restarted it; try again"
                    );
                }
                crate::shared::bgutil::HealOutcome::Failed => {
                    crate::shared::macros::status_warn!(
                        "YouTube stream failed and the bgutil token service restart failed"
                    );
                }
                _ => {}
            }
        }
        let _ = event_sender.send(AppEvent::MpvSessionEnded);
    });
}
/// Spawn the tracker caretaker daemon (`s2u-helper tracker`, fire and
/// forget): it keeps the MPRIS state file and Jellyfin playback tracking
/// alive when s2udio closes while a video plays. Detached like mpv so it
/// survives the terminal close; its single-instance pid guard makes
/// repeated spawns harmless.
fn spawn_tracker() {
    if let Err(err) = {
        let mut tracker = std::process::Command::new("s2u-helper");
        tracker.arg("tracker");
        if let Some(cache) = crate::shared::paths::s2udio_cache_dir() {
            tracker.env("S2U_CACHE_DIR", cache);
        }
        // Same socket the app tracks: the tracker must not fall back to the
        // dead fixed path when mpvSockets.lua moved the mpv to
        // `/tmp/mpvSockets/<pid>` (SVP4's bundled mpv does that).
        let pid = LAUNCHED_MPV_PID.load(std::sync::atomic::Ordering::Relaxed);
        if pid != 0 {
            tracker.env("S2U_MPV_PID", pid.to_string());
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




#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    use super::{MpvPlaylistEntry, complete_video_playlist, read_mpv_state, read_mpv_title};

    /// A resolved entry for `link`: what `apply_resolved_streams` builds.
    fn resolved(link: &str, title: &str, url: &str) -> MpvPlaylistEntry {
        let mut entry = MpvPlaylistEntry::new(title, url, Some(120.0));
        entry.original_url = Some(link.to_owned());
        entry
    }

    /// The row a paste inserted is **filled in place**: same position, no
    /// second row, and the resolved URL/title/duration are the ones the list
    /// shows from now on — the visible half of "rows appear immediately, the
    /// details follow".
    #[test]
    fn a_resolved_entry_fills_its_placeholder_in_place() {
        let ctx = crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let link = "https://www.youtube.com/watch?v=AAA";
        *ctx.video_playlist.borrow_mut() = vec![
            MpvPlaylistEntry::new("Before", "before", None),
            MpvPlaylistEntry::pending_link(link),
            MpvPlaylistEntry::new("After", "after", None),
        ];
        let (filled, appended) =
            complete_video_playlist(&ctx, vec![resolved(link, "Real title", "http://cdn/1")], true);
        assert_eq!(appended, 0, "the placeholder took the entry");
        assert_eq!(filled.len(), 1);
        let list = ctx.video_playlist.borrow();
        assert_eq!(list.len(), 3, "no duplicate row was added");
        assert_eq!(list[1].title, "Real title");
        assert_eq!(list[1].url, "http://cdn/1");
        assert_eq!(list[1].duration, Some(120.0));
        assert!(!list[1].pending, "the row stops spinning");
        assert_eq!(list[0].title, "Before", "the rows around it keep their place");
        assert_eq!(list[2].title, "After");
    }

    /// A resolve whose placeholder is gone (the user deleted the row while it
    /// resolved) still lands, as an appended row — never silently dropped.
    #[test]
    fn a_resolved_entry_without_a_placeholder_is_appended() {
        let ctx = crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let link = "https://www.youtube.com/watch?v=BBB";
        ctx.video_playlist.borrow_mut().clear();
        let (filled, appended) =
            complete_video_playlist(&ctx, vec![resolved(link, "Title", "http://cdn/2")], false);
        assert!(filled.is_empty());
        assert_eq!(appended, 1);
        let list = ctx.video_playlist.borrow();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Title");
        assert!(!list[0].pending);
    }

    /// A `Ctx` for the resume-seek logic: the socket path points nowhere, so
    /// every `mpv_seek` is a fast connect failure and only the state machine
    /// under test is exercised.
    fn resume_test_ctx() -> crate::ctx::Ctx {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    /// The attempt counter of the armed resume seek, if any.
    fn armed(ctx: &crate::ctx::Ctx) -> Option<(f64, u8)> {
        ctx.mpv.resume_seek.get().map(|(secs, attempts, _)| (secs, attempts))
    }

    /// Round 110 (host-verified): mpv v0.41 appends ANY sibling `.m3u` next to
    /// the playlist file as an extra entry — with the stray file present the
    /// launch played the extra entry and exited 3 ("some files played, some
    /// didn't"), which is `unix_wait_status(768)`, the exact code the reported
    /// torrent stream died with. The writer must therefore leave the directory
    /// with exactly one `.m3u`, whatever was dropped there.
    #[test]
    fn write_mpv_m3u_leaves_no_sibling_playlist_behind() {
        let dir = std::env::temp_dir().join("s2u-mpv");
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let foreign = dir.join("someone-elses-playlist.m3u");
        std::fs::write(&foreign, "#EXTM3U\nhttp://127.0.0.1:1/other\n").expect("write");
        let own_stale = dir.join("s2u-mpv-playlist-1.m3u");
        std::fs::write(&own_stale, "#EXTM3U\nhttp://127.0.0.1:1/stale\n").expect("write");

        let entries = vec![MpvPlaylistEntry::new("t".to_owned(), "http://127.0.0.1:2/keep".to_owned(), None)];
        let path = super::write_mpv_m3u(&entries);

        assert!(path.exists(), "the launch's own playlist exists");
        assert!(!foreign.exists(), "a foreign sibling playlist is removed");
        assert!(!own_stale.exists(), "a stale sibling playlist is removed");
        let siblings: Vec<String> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".m3u"))
            .collect();
        assert_eq!(siblings, vec![path.file_name().unwrap().to_string_lossy().into_owned()]);
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("http://127.0.0.1:2/keep"), "body: {body}");
        let _ = std::fs::remove_file(&path);
    }

    /// The failure notice shown when mpv exits without playing must name the
    /// reason mpv logged — `--no-terminal` suppresses it on stderr, so the log
    /// file is the only witness (round 108's known gap, closed in round 110).
    /// A benign `[v] … Failed …` line must not win over mpv's error level.
    #[test]
    fn mpv_log_reason_finds_the_first_error_line() {
        let path = std::env::temp_dir().join(format!("s2u-mpv-reason-{}.log", std::process::id()));
        std::fs::write(
            &path,
            "[   0.010][v][cplayer] Playing: http://127.0.0.1:3030/torrents/0/stream/0\n\
             [   0.015][v][cplayer] Failed sending hook command auto_profiles/on_load. Removing hook.\n\
             [   0.020][e][ffmpeg] tcp: Connection to tcp://127.0.0.1:3030 failed: Connection refused\n\
             [   0.030][e][stream] Failed to open http://127.0.0.1:3030/torrents/0/stream/0.\n",
        )
        .expect("write");
        let reason = super::mpv_log_reason(&path).expect("a reason");
        assert!(reason.contains("Connection refused"), "reason: {reason}");
        assert!(!reason.starts_with('['), "the mpv prefix is stripped: {reason}");
        let _ = std::fs::remove_file(&path);
        // No error level at all: fall back to the first failure-mentioning line.
        std::fs::write(&path, "[   0.010][v][ytdl_hook] youtube-dl failed: unexpected error\n")
            .expect("write");
        let fallback = super::mpv_log_reason(&path).expect("a fallback reason");
        assert!(fallback.contains("youtube-dl failed"), "fallback: {fallback}");
        let _ = std::fs::remove_file(&path);
        assert_eq!(super::mpv_log_reason(Path::new("/nonexistent/s2u-mpv.log")), None);
    }

    /// The polling bug this fixes: mpv reports position 0 while it is still
    /// opening an entry, and a seek sent then is dropped. The retry must not
    /// spend an attempt until playback has actually started, must seek once
    /// it has, and must stop once the position passed the target.
    #[test]
    fn resume_seek_waits_for_playback_then_clears_when_it_lands() {
        let ctx = resume_test_ctx();
        let socket = std::path::Path::new("/tmp/s2u-tests-no-such-mpv-socket");
        ctx.mpv.resume_seek.set(Some((600.0, 0, None)));

        // mpv is still opening the entry: no attempt, still armed.
        super::apply_pending_resume_seek(&ctx, socket, 0.0, 0.0);
        assert_eq!(armed(&ctx), Some((600.0, 0)));

        // Playback started short of the target: one seek attempt.
        super::apply_pending_resume_seek(&ctx, socket, 5.0, 0.0);
        assert_eq!(armed(&ctx), Some((600.0, 1)));

        // Within the 5 s tolerance of the target: done, cleared.
        super::apply_pending_resume_seek(&ctx, socket, 600.0, 0.0);
        assert_eq!(armed(&ctx), None);
    }

    /// A target at or past the known end of the video is dropped instead of
    /// being retried forever.
    #[test]
    fn resume_seek_past_the_end_is_dropped() {
        let ctx = resume_test_ctx();
        let socket = std::path::Path::new("/tmp/s2u-tests-no-such-mpv-socket");
        ctx.mpv.resume_seek.set(Some((96.0, 0, None)));
        super::apply_pending_resume_seek(&ctx, socket, 5.0, 100.0);
        assert_eq!(armed(&ctx), None);
    }

    /// The retry is attempt-bounded and rate-limited: a stream that never
    /// reaches the target is not fought indefinitely, and a second seek is
    /// not sent within the retry interval.
    #[test]
    fn resume_seek_retries_are_bounded_and_rate_limited() {
        let ctx = resume_test_ctx();
        let socket = std::path::Path::new("/tmp/s2u-tests-no-such-mpv-socket");
        ctx.mpv
            .resume_seek
            .set(Some((600.0, super::MAX_START_SEEK_ATTEMPTS, None)));
        super::apply_pending_resume_seek(&ctx, socket, 5.0, 0.0);
        assert_eq!(armed(&ctx), None, "the attempt budget is spent");

        ctx.mpv
            .resume_seek
            .set(Some((600.0, 1, Some(std::time::Instant::now()))));
        super::apply_pending_resume_seek(&ctx, socket, 5.0, 0.0);
        assert_eq!(armed(&ctx), Some((600.0, 1)), "rate-limited between attempts");
    }

    /// A fake mpv IPC server: answers each incoming line with one JSON
    /// response line (like mpv's ipc protocol).
    fn fake_mpv(socket: &std::path::Path, responses: Vec<&'static str>) {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let mut i = 0;
                // Like real mpv: answer each command line immediately.
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    if i < responses.len() {
                        let _ = writeln!(reader.get_mut(), "{}", responses[i]);
                        let _ = reader.get_mut().flush();
                    }
                    i += 1;
                    line.clear();
                }
            }
        });
    }

    #[test]
    fn detach_child_makes_the_child_its_own_session_leader() {
        use super::detach_child;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        unsafe { detach_child(&mut cmd) };
        let mut child = cmd.spawn().expect("sleep must spawn");
        // /proc/<pid>/stat: after the comm field (the last ')') the fields
        // are state, ppid, pgrp, session, ... A setsid'd child is its own
        // process-group leader and session leader (pgrp == session == pid).
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id()))
            .expect("child stat readable");
        let after_comm = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or(&stat);
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        assert!(fields.len() > 3, "stat fields: {stat}");
        let pid = i64::from(child.id());
        assert_eq!(fields[2].parse::<i64>().unwrap(), pid, "pgrp must be its own");
        assert_eq!(fields[3].parse::<i64>().unwrap(), pid, "session must be its own");
        // SIGHUP (signal 1) must be ignored: /proc/<pid>/status SigIgn is a
        // bitmask whose bit 0 is SIGHUP.
        let status = std::fs::read_to_string(format!("/proc/{}/status", child.id()))
            .expect("child status readable");
        let sigign = status.lines().find_map(|l| l.strip_prefix("SigIgn:")).expect("SigIgn line");
        let sigign = u64::from_str_radix(sigign.trim(), 16).expect("SigIgn hex");
        assert_eq!(sigign & 1, 1, "SIGHUP must be ignored: SigIgn={sigign:#x}");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn read_mpv_state_parses_responses_and_detects_a_dead_socket() {
        let dir = std::env::temp_dir().join(format!("mpv-read-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("mpv.sock");

        // Live mpv: all six properties answered.
        fake_mpv(
            &socket,
            vec![
                r#"{"error":"success","data":42.5}"#,
                r#"{"error":"success","data":false}"#,
                r#"{"error":"success","data":600.0}"#,
                r#"{"error":"success","data":71}"#,
                r#"{"error":"success","data":0}"#,
                r#"{"error":"success","data":1}"#,
            ],
        );
        let state = read_mpv_state(&socket).expect("live mpv must read Some");
        assert_eq!(state.0, 42.5);
        assert!(!state.1);
        assert_eq!(state.2, 600.0);
        assert_eq!(state.3, Some(71));
        assert_eq!(state.4, Some(0));
        assert_eq!(state.5, Some(1));
        assert_eq!(read_mpv_title(&socket), None, "no title property was answered");

        // Dead mpv: the socket file is gone / connection refused. This used
        // to come back as Some((0.0, false, 0.0, ..)) — the zombie session
        // that kept MPRIS alive and swallowed new videos instead of
        // launching a fresh mpv.
        let _ = std::fs::remove_file(&socket);
        assert!(read_mpv_state(&socket).is_none(), "dead mpv must read None");
        assert!(read_mpv_title(&socket).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_mpv_state_keeps_fields_aligned_when_duration_is_unavailable() {
        // An HLS stream whose demuxer has not loaded yet answers `duration`
        // as unavailable. Each property must land in its own slot: the old
        // positional-number parser shifted every later field (volume would
        // show up as `duration`).
        let dir = std::env::temp_dir().join(format!("mpv-dur-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("mpv.sock");
        fake_mpv(
            &socket,
            vec![
                r#"{"error":"success","data":42.5}"#,
                r#"{"error":"success","data":false}"#,
                r#"{"error":"property unavailable","data":null}"#,
                r#"{"error":"success","data":71}"#,
                r#"{"error":"success","data":0}"#,
                r#"{"error":"success","data":1}"#,
            ],
        );
        let state = read_mpv_state(&socket).expect("live mpv must read Some");
        assert_eq!(state.0, 42.5, "position stays in slot 0");
        assert!(!state.1);
        assert_eq!(state.2, 0.0, "duration is 0, not the volume value");
        assert_eq!(state.3, Some(71), "volume stays in slot 3");
        assert_eq!(state.4, Some(0), "playlist-pos stays in slot 4");
        assert_eq!(state.5, Some(1), "playlist-count stays in slot 5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_provisional_title_detects_stream_basenames() {
        use super::{is_provisional_title, is_stream_basename};
        // A resolved HLS/audio stream's basename must be treated as
        // provisional so the saved title / yt-info is used instead.
        assert!(is_stream_basename("index.m3u8"));
        assert!(is_stream_basename("audio.m4a"));
        assert!(!is_stream_basename("a real song name.flac"));
        assert!(!is_stream_basename("dir/index.m3u8"));
        assert!(is_provisional_title("index.m3u8"));
        assert!(is_provisional_title("watch?v=abc123"));
        assert!(is_provisional_title("https://example/stream"));
        assert!(!is_provisional_title("Rick Astley - Never Gonna Give You Up"));
    }

    #[test]
    fn playlist_entry_serializes_original_url_and_defaults_when_missing() {
        use super::MpvPlaylistEntry;
        // New format round-trips the canonical link.
        let mut entry = MpvPlaylistEntry::new(
            "Rick Astley - Never Gonna Give You Up",
            "https://rr4.example/audio.m3u8",
            None,
        );
        entry.original_url = Some("https://youtu.be/x".to_owned());
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("original_url"), "{json}");
        let back: MpvPlaylistEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.original_url.as_deref(), Some("https://youtu.be/x"));
        assert_eq!(back.lookup_url(), "https://youtu.be/x");
        // Old saved files (no `original_url` field) still deserialize.
        let old = r#"{"title":"T","url":"https://u/x","duration":null}"#;
        let old_entry: MpvPlaylistEntry = serde_json::from_str(old).unwrap();
        assert_eq!(old_entry.original_url, None);
        assert_eq!(old_entry.lookup_url(), "https://u/x");
    }

    #[test]
    fn read_mpv_playlist_parses_entries_and_falls_back_to_the_path() {
        use super::read_mpv_playlist;
        let dir = std::env::temp_dir().join(format!("mpv-pl-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("mpv.sock");

        // mpv reports its playlist: filename is the URL (the original
        // YouTube link for a resolved stream), title the media title.
        fake_mpv(
            &socket,
            vec![
                r#"{"error":"success","data":[{"filename":"https://www.youtube.com/watch?v=Hc9qrvQ3QPg","title":"I tested EVERY 32bit float wireless lav mic","current":true}]}"#,
                r#"{"error":"success","data":"https://www.youtube.com/watch?v=Hc9qrvQ3QPg"}"#,
            ],
        );
        let entries = read_mpv_playlist(&socket);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "https://www.youtube.com/watch?v=Hc9qrvQ3QPg");
        assert_eq!(entries[0].title, "I tested EVERY 32bit float wireless lav mic");

        // No playlist property yet (single loaded file): the playing path
        // is the only entry.
        fake_mpv(
            &socket,
            vec![
                r#"{"error":"property unavailable","data":null}"#,
                r#"{"error":"success","data":"https://youtu.be/abc123"}"#,
            ],
        );
        let entries = read_mpv_playlist(&socket);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "https://youtu.be/abc123");
        assert_eq!(entries[0].title, "https://youtu.be/abc123");

        // Dead socket: no entries (the reattach falls back gracefully).
        let _ = std::fs::remove_file(&socket);
        assert!(read_mpv_playlist(&socket).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reattach_recovers_the_playlist_from_mpv_when_the_state_file_has_none() {
        use std::io::{BufRead, BufReader};

        use super::detect_mpv_session_at;
        let dir = std::env::temp_dir().join(format!("mpv-reattach-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("mpv.sock");

        // mpv answers in order: read_mpv_state (6 lines), read_mpv_title
        // (1 line), read_mpv_playlist (playlist + path).
        let responses = [
            r#"{"error":"success","data":123.4}"#,
            r#"{"error":"success","data":false}"#,
            r#"{"error":"success","data":1598.0}"#,
            r#"{"error":"success","data":80}"#,
            r#"{"error":"success","data":0}"#,
            r#"{"error":"success","data":1}"#,
            r#"{"error":"success","data":"I tested EVERY 32bit float wireless lav mic"}"#,
            r#"{"error":"success","data":[{"filename":"https://www.youtube.com/watch?v=Hc9qrvQ3QPg","title":"I tested EVERY 32bit float wireless lav mic","current":true}]}"#,
            r#"{"error":"success","data":"https://www.youtube.com/watch?v=Hc9qrvQ3QPg"}"#,
        ];
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            // Three sequential connections (one per mpv_exchange call);
            // answer all command lines in order across connections.
            let mut i = 0;
            for _ in 0..3 {
                if let Ok((stream, _)) = listener.accept() {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                        if i < responses.len() {
                            let _ = writeln!(reader.get_mut(), "{}", responses[i]);
                            let _ = reader.get_mut().flush();
                        }
                        i += 1;
                        line.clear();
                    }
                }
            }
        });

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut config = crate::config::Config::default();
        config.cache_dir = Some(dir.clone());
        ctx.config = std::sync::Arc::new(config);
        // A fresh state file that knows the title but carries an empty
        // playlist (e.g. written before the session had entries, or the
        // stale-file case — both leave the playlist empty).
        crate::ui::modals::paste::write_mpv_mpris_state(&ctx);
        {
            // Rewrite with an empty playlist and the known title.
            let path = crate::ui::modals::paste::mpv_mpris_state_path(Some(&dir));
            std::fs::write(
                &path,
                r#"{"title":"stale title","artist":"","art":"","playing":true,"position":0,"duration":0,"socket":"","item_id":"","playlist":[],"playlist_pos":0}"#,
            )
            .unwrap();
        }

        assert!(detect_mpv_session_at(&mut ctx, socket), "a live mpv session must be detected");
        // The playlist is recovered from mpv itself, so the yt-info /
        // chapters / thumbnail lookups (keyed by the playing URL) work.
        assert_eq!(ctx.mpv.playlist.borrow().len(), 1, "playlist recovered");
        assert_eq!(ctx.mpv.playlist.borrow()[0].url, "https://www.youtube.com/watch?v=Hc9qrvQ3QPg");
        assert_eq!(ctx.mpv.title, "stale title", "fresh state file is trusted for the title");
        assert_eq!(ctx.mpv.playlist_pos.get(), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The persistent video playlist: add/append semantics, the current
    /// entry matched by URL, and the persistence file round-trip.
    #[test]
    fn video_playlist_add_append_current_and_persist() {
        use crate::core::mpv::{
            MpvPlaylistEntry, add_to_video_playlist, video_playlist_current_idx,
        };
        use crate::tests::fixtures::ctx;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let dir = std::env::temp_dir().join(format!("video-playlist-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut config = crate::config::Config::default();
        config.cache_dir = Some(dir.clone());
        ctx.config = std::sync::Arc::new(config);

        *ctx.video_playlist.borrow_mut() = vec![
            MpvPlaylistEntry::new(
                "A",
                "http://jf/Videos/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/stream",
                None,
            ),
            MpvPlaylistEntry::new(
                "B",
                "http://jf/Videos/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/stream",
                None,
            ),
            MpvPlaylistEntry::new(
                "C",
                "http://jf/Videos/cccccccccccccccccccccccccccccccc/stream",
                None,
            ),
        ];
        // The session plays B (matched by URL, not by position).
        ctx.mpv.active = true;
        ctx.mpv.playlist.borrow_mut().push(MpvPlaylistEntry::new(
            "B",
            "http://jf/Videos/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/stream",
            None,
        ));
        ctx.mpv.playlist_pos.set(Some(0));
        assert_eq!(video_playlist_current_idx(&ctx), Some(1));

        // Add (after the current entry) and append.
        add_to_video_playlist(
            &ctx,
            vec![MpvPlaylistEntry::new(
                "D",
                "http://jf/Videos/dddddddddddddddddddddddddddddddd/stream",
                None,
            )],
            true,
        );
        add_to_video_playlist(
            &ctx,
            vec![MpvPlaylistEntry::new(
                "E",
                "http://jf/Videos/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee/stream",
                None,
            )],
            false,
        );
        let titles: Vec<String> =
            ctx.video_playlist.borrow().iter().map(|e| e.title.clone()).collect();
        assert_eq!(titles, vec!["A", "B", "D", "C", "E"]);

        // A URL-less entry is dropped.
        add_to_video_playlist(&ctx, vec![MpvPlaylistEntry::new("X", "", None)], true);
        assert_eq!(ctx.video_playlist.borrow().len(), 5);

        // The queue tab switched to the Video list.
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Video);

        // The persistence file round-trips the queue.
        let path = crate::ui::modals::paste::video_playlist_path(Some(&dir));
        let content = std::fs::read_to_string(&path).expect("video playlist saved");
        let stored: Vec<MpvPlaylistEntry> =
            serde_json::from_str(&content).expect("valid JSON playlist");
        assert_eq!(stored.len(), 5);
        assert_eq!(stored[2].title, "D");
        assert_eq!(stored[3].title, "C");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Jellyfin item metadata replaces the URL-derived "stream" titles in
    /// the session playlist and the persistent queue.
    #[test]
    fn jellyfin_metadata_updates_the_playlist_titles() {
        use crate::core::mpv::{MpvPlaylistEntry, update_jellyfin_entry_title};
        use crate::tests::fixtures::ctx;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let item_id = "0123456789abcdef0123456789abcdef";
        let url = format!("http://jf/Videos/{item_id}/stream?static=true");
        // Session plays a single Jellyfin video recorded with the
        // URL-derived title; the queue holds the same stream too.
        ctx.mpv.active = true;
        ctx.mpv.playlist.borrow_mut().push(MpvPlaylistEntry::new("stream", url.clone(), None));
        ctx.video_playlist.borrow_mut().push(MpvPlaylistEntry::new("stream", url.clone(), None));

        update_jellyfin_entry_title(&ctx, item_id, "Real Episode Name");

        assert_eq!(ctx.mpv.playlist.borrow()[0].title, "Real Episode Name");
        assert_eq!(ctx.video_playlist.borrow()[0].title, "Real Episode Name");
        // Unrelated entries are untouched.
        ctx.video_playlist.borrow_mut().push(MpvPlaylistEntry::new(
            "stream",
            "http://jf/Videos/ffffffffffffffffffffffffffffffff/stream",
            None,
        ));
        update_jellyfin_entry_title(&ctx, item_id, "Real Episode Name");
        assert_eq!(
            ctx.video_playlist.borrow()[1].title,
            "stream",
            "other items' titles are not overwritten"
        );
    }

    /// The poll confirms a recorded playlist entry against mpv's actual
    /// playing path before adopting it as the current item.
    #[test]
    fn recorded_entry_for_mpv_pos_confirms_the_playing_entry() {
        use super::{MpvPlaylistEntry, recorded_entry_for_mpv_pos};

        // Jellyfin stream URLs: {base}/Videos/<32-hex-id>/stream. The
        // recorded season (rotated: the clicked episode first) and mpv's
        // real playlist can diverge after a `loadfile … replace` splice;
        // a same-length old season leaves mpv's `playlist-pos` pointing at
        // a stale index, and following it would surface the *next*
        // episode's metadata (+1) while mpv plays the clicked one.
        let clicked = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let next = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let season: Vec<MpvPlaylistEntry> = [clicked, next, "cccccccccccccccccccccccccccccccc"]
            .iter()
            .map(|id| {
                MpvPlaylistEntry::new(
                    format!("episode-{id}"),
                    format!("http://jf/Videos/{id}/stream?static=true"),
                    None,
                )
            })
            .collect();

        // mpv is playing the clicked episode: the recorded entry at mpv's
        // position matches its path -> adopted.
        let adopted = recorded_entry_for_mpv_pos(
            &season,
            0,
            Some(&format!("http://jf/Videos/{clicked}/stream?static=true")),
        );
        let expected_title = format!("episode-{clicked}");
        assert_eq!(
            adopted.as_ref().map(|e| e.title.as_str()),
            Some(expected_title.as_str()),
            "matching entry is adopted"
        );

        // mpv is still on the clicked episode but its reported position is
        // the stale index 1 (old-season splice): the recorded entry at that
        // index is the NEXT episode -> the advance is refused so the UI
        // keeps showing the clicked episode.
        let refused = recorded_entry_for_mpv_pos(
            &season,
            1,
            Some(&format!("http://jf/Videos/{clicked}/stream?static=true")),
        );
        assert!(refused.is_none(), "diverged position is not adopted");

        // mpv genuinely advanced to the next episode: its path matches the
        // recorded entry at the new position -> adopted.
        let advanced = recorded_entry_for_mpv_pos(
            &season,
            1,
            Some(&format!("http://jf/Videos/{next}/stream?static=true")),
        );
        assert!(advanced.is_some(), "real auto-advance is adopted");

        // Out of range: nothing to adopt.
        let clicked_path = format!("http://jf/Videos/{clicked}/stream?static=true");
        assert!(recorded_entry_for_mpv_pos(&season, 5, Some(&clicked_path)).is_none());

        // A non-Jellyfin entry (YouTube etc.) carries no item id: the
        // positional behavior is preserved regardless of mpv's path.
        let yt = vec![MpvPlaylistEntry::new("y", "https://www.youtube.com/watch?v=abc", None)];
        assert!(
            recorded_entry_for_mpv_pos(&yt, 0, Some("https://rr4.example/stream.m3u8")).is_some()
        );
        assert!(recorded_entry_for_mpv_pos(&yt, 0, None).is_some());
    }

    /// `read_mpv_path` returns the playing file/URL, `None` when mpv does
    /// not answer (or the socket is gone).
    #[test]
    fn read_mpv_path_parses_the_playing_path() {
        use super::read_mpv_path;

        let dir = std::env::temp_dir().join(format!("mpv-path-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("mpv.sock");
        fake_mpv(
            &socket,
            vec![
                r#"{"error":"success","data":"http://jf/Videos/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/stream"}"#,
            ],
        );
        assert_eq!(
            read_mpv_path(&socket).as_deref(),
            Some("http://jf/Videos/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/stream")
        );

        let _ = std::fs::remove_file(&socket);
        assert!(read_mpv_path(&socket).is_none(), "dead socket reads None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mpv_playlist_clear` sends mpv's `playlist-clear` command.
    #[test]
    fn mpv_playlist_clear_sends_the_command() {
        use std::io::{BufRead, BufReader};

        use super::mpv_playlist_clear;
        let dir = std::env::temp_dir().join(format!("mpv-clear-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket = dir.join("mpv.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    let _ = writeln!(reader.get_mut(), r#"{{"error":"success"}}"#);
                    let _ = reader.get_mut().flush();
                }
            }
        });
        mpv_playlist_clear(&socket);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_playlist_shown_for_torrent_and_jellyfin_but_not_yt() {
        use crate::core::mpv::MpvPlaylistEntry;
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.mpv.active = true;

        // A torrent session: the Queue tab's Video list shows the session
        // playlist (the torrent's files) even though no Jellyfin item
        // plays — the persistent list cannot hold the token-bearing URLs.
        ctx.mpv.playlist.borrow_mut().push(MpvPlaylistEntry::new(
            "ep01.mkv",
            "http://s2u:tok@127.0.0.1:3030/torrents/1/stream/0",
            None,
        ));
        ctx.mpv.playlist.borrow_mut().push(MpvPlaylistEntry::new(
            "ep02.mkv",
            "http://s2u:tok@127.0.0.1:3030/torrents/1/stream/1",
            None,
        ));
        assert!(super::session_playlist_shown(&ctx), "torrent session playlist is shown");

        // A Jellyfin item session: the season playlist is shown too.
        ctx.mpv.item_id = Some("0123456789abcdef0123456789abcdef".to_owned());
        assert!(super::session_playlist_shown(&ctx), "jellyfin session playlist is shown");
        ctx.mpv.item_id = None;

        // A plain YouTube video: the persistent playlist is shown again.
        ctx.mpv.playlist.borrow_mut().clear();
        ctx.mpv.playlist.borrow_mut().push(MpvPlaylistEntry::new(
            "some video",
            "https://youtu.be/abc123",
            None,
        ));
        assert!(!super::session_playlist_shown(&ctx), "non-torrent session uses the queue");
    }

    #[test]
    fn mpv_socket_prefers_live_fixed_socket_then_per_instance_then_stale() {
        use std::io::{BufRead as _, Write as _};

        // One temp dir per scenario so listener lifetimes stay obvious.
        let base = std::env::temp_dir().join(format!("mpv-socket-test-{}", std::process::id()));

        // 1. No sockets at all -> None.
        let dir = base.join("none");
        std::fs::create_dir_all(dir.join("mpvSockets")).unwrap();
        assert_eq!(super::mpv_socket_in(&dir.join("mpvsocket"), &dir.join("mpvSockets"), None), None);

        // 2. A stale fixed socket file (no listener) plus a live per-instance
        //    socket -> the live per-instance socket wins.
        let dir = base.join("per-instance");
        std::fs::create_dir_all(dir.join("mpvSockets")).unwrap();
        let fixed = dir.join("mpvsocket");
        let per = dir.join("mpvSockets");
        std::fs::write(&fixed, b"stale").unwrap();
        let per_sock = per.join("100");
        let listener = UnixListener::bind(&per_sock).unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut line = String::new();
                let _ = std::io::BufReader::new(&stream).read_line(&mut line);
                let _ = writeln!(stream, r#"{{"error":"success"}}"#);
            }
        });
        assert_eq!(super::mpv_socket_in(&fixed, &per, None).as_deref(), Some(per_sock.as_path()));

        // 3. The fixed socket live -> it wins even over a newer per-instance
        //    entry (the fixed socket is the SVP4 one and must be used).
        let dir = base.join("fixed-live");
        std::fs::create_dir_all(dir.join("mpvSockets")).unwrap();
        let fixed = dir.join("mpvsocket");
        let per = dir.join("mpvSockets");
        let fixed_listener = UnixListener::bind(&fixed).unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = fixed_listener.accept() {
                let mut line = String::new();
                let _ = std::io::BufReader::new(&stream).read_line(&mut line);
                let _ = writeln!(stream, r#"{{"error":"success"}}"#);
            }
        });
        let newer = per.join("101");
        std::fs::write(&newer, b"x").unwrap();
        assert_eq!(super::mpv_socket_in(&fixed, &per, None).as_deref(), Some(fixed.as_path()));

        // 4. Fixed stale, no per-instance sockets -> the stale fixed path is
        //    returned so callers can detect the dead socket (old fallback).
        let dir = base.join("stale-only");
        std::fs::create_dir_all(dir.join("mpvSockets")).unwrap();
        let fixed = dir.join("mpvsocket");
        std::fs::write(&fixed, b"stale").unwrap();
        assert_eq!(
            super::mpv_socket_in(&fixed, &dir.join("mpvSockets"), None).as_deref(),
            Some(fixed.as_path())
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn connect_socket_times_out_on_a_stuck_listener() {
        use std::time::{Duration, Instant};

        // A listener with a zero backlog that never accepts: the one
        // allowed pending connection fills the slot, so the next connect
        // gets EAGAIN — a blocking connect would wait forever (a leftover
        // thumbfast thumbnailer once held /tmp/mpvsocket like this and
        // froze the whole app at startup). connect_socket must give up.
        let dir = std::env::temp_dir().join(format!("mpv-connect-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("stuck.sock");
        let _ = std::fs::remove_file(&socket);

        let fd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
            let path_c =
                std::ffi::CString::new(socket.as_os_str().as_encoded_bytes()).expect("no NUL");
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            std::ptr::copy_nonoverlapping(
                path_c.as_bytes().as_ptr(),
                addr.sun_path.as_mut_ptr() as *mut u8,
                path_c.as_bytes().len(),
            );
            let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            assert_eq!(
                libc::bind(fd, &addr as *const libc::sockaddr_un as *const libc::sockaddr, len),
                0,
                "bind: {}",
                std::io::Error::last_os_error()
            );
            assert_eq!(libc::listen(fd, 0), 0, "listen: {}", std::io::Error::last_os_error());
            fd
        };
        // Occupy the single backlog slot (keep it open until the end).
        let _slot =
            std::os::unix::net::UnixStream::connect(&socket).expect("first connect is allowed");

        let start = Instant::now();
        let result = super::connect_socket(&socket, Duration::from_millis(200));
        let elapsed = start.elapsed();
        assert!(result.is_err(), "a stuck listener must not connect: {result:?}");
        assert!(elapsed < Duration::from_secs(5), "connect must give up fast, took {elapsed:?}");

        drop(_slot);
        unsafe {
            libc::close(fd);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

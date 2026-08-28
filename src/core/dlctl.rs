//! `s2udio dl start|status|stop` — the durable torrent downloader daemon
//! (round 54).
//!
//! Committed torrent downloads — "Stream and download", the file picker's
//! "Download & Play", "Download", "Download all" — must COMPLETE even when
//! the TUI exits before the download finishes (R1). They therefore live
//! OUTSIDE the TUI in a detached daemon process (`s2udio dl serve`,
//! mirroring round 43's `s2udio rq serve`):
//!
//! - the TUI enqueues jobs through a spool directory
//!   (`~/.cache/s2udio/dl-jobs/*.json`, written atomically, deleted after
//!   consumption);
//! - the daemon owns **one rqbit engine per committed job** (each via
//!   `torrent::start_engine` with the same config rule as the app and
//!   `s2udio rq`: the config `torrent` section + the state.ron socks
//!   override), so multiple committed downloads run concurrently and
//!   independently (R3);
//! - progress + status live in the shared state file
//!   `~/.cache/s2udio/downloads.json`, which the TUI reads for the
//!   Downloads modal and the startup status line; the file stores engine
//!   **proxy URLs only** (click) — never auth tokens;
//! - the daemon answers "stream and download" / "Download & Play"
//!   requests with a per-request response file
//!   (`dl-responses/<request_id>.json`) carrying the torrent id + the
//!   stream URLs (token embedded as URL userinfo for mpv), which the TUI
//!   deletes after consuming;
//! - the auth token itself lives in a 0600 per-job sidecar
//!   (`dl-tokens/<job_id>`);
//! - jobs are deduplicated by infohash: a second committed action on the
//!   same torrent extends the existing job's kept-file list instead of
//!   starting a second engine (the same-cache-dir hazard guard, §2.2);
//! - on completion the kept files are moved to
//!   `~/Downloads/s2udio-downloads` (deferred while the TUI is streaming
//!   them — R2.5), the torrent is forgotten, and the job stays listed as
//!   `Completed` until the user removes it (round 56.6: terminal rows
//!   persist — no auto-prune; the daemon exits and unregisters when no
//!   active jobs remain, leaving the rows with pid 0).
//!
//! A plain stream of a torrent WITH an active committed job is routed
//! through the daemon engine too (no second TUI engine on the cache dir);
//! plain streams of torrents without a committed job stay on the
//! ephemeral TUI engines, exactly as before round 54.
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use serde::{Deserialize, Serialize};
use crate::config::cli::DlCmd;

/// The state file name inside the s2udio cache dir (`downloads.json`).
const STATE_FILE: &str = "downloads.json";
/// Job spool dir: the TUI drops enqueue/stream/stop requests here; the
/// daemon watches it and deletes consumed files.
const JOBS_DIR: &str = "dl-jobs";
/// Response dir: per-request responses (`<request_id>.json`) with the
/// torrent id + token-bearing stream URLs; the TUI deletes after reading.
const RESPONSES_DIR: &str = "dl-responses";
/// Per-job token sidecars (`<job_id>`, mode 0600): the raw `user:pass`
/// pair of the job's engine. Needed for mpv stream URLs (URL userinfo
/// auth); never appears in `downloads.json` (proxy URLs only) or the UI.
const TOKENS_DIR: &str = "dl-tokens";
/// The TUI's "streams over" marker (R2.5): `{"pid": <tui pid>,
/// "jobs": [...]}` — written while the TUI streams a daemon job's files,
/// removed on `MpvSessionEnded`. The daemon defers moving completed
/// files while the marker is fresh (TUI pid alive) or a live mpv still
/// references the stream URLs, and completes anyway after a bounded wait.
const STREAMING_MARKER: &str = "dl-streaming.json";
/// How long `start` waits for the daemon's READY line.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Spool scan interval (short — the UI waits on responses).
const SPOOL_POLL_INTERVAL: Duration = Duration::from_millis(300);
/// Stats/progress poll interval for active jobs.
const JOB_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// R2.5 bounded wait: after a completed download has waited this long for
/// a stream to end, the daemon moves the files anyway (rename-first, then
/// forget — a continued stream tail may break; mpv is gone in practice).
const MOVE_DEFER_LIMIT: Duration = Duration::from_secs(10 * 60);
/// A spool file that fails to parse is retried for this long before it is
/// dropped with a warning (guards against a torn write).
const SPOOL_PARSE_RETRY: Duration = Duration::from_secs(30);
/// Round 56.6 (56.6-4): the window in which a `Completed` row persisted
/// in `downloads.json` is surfaced at TUI startup ("Download complete:
/// …"). Rows completed longer ago stay listed but are NOT re-noticed —
/// a TUI restarted right after a download finished still sees it, old
/// rows do not spam every launch (rows now persist indefinitely).
const COMPLETED_NOTICE_STARTUP_WINDOW: Duration = Duration::from_secs(10 * 60);

/// The per-job status shown in `downloads.json` / the Downloads modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DlStatus {
    /// Enqueued, waiting for the engine/add thread to start.
    Queued,
    /// The torrent is being added (metainfo wait — a cold magnet can take
    /// a while; the "Preparing downloader…" window covers this).
    Adding,
    /// The engine has the torrent and is downloading.
    Downloading,
    /// The download is complete; the kept files are being moved to
    /// `s2udio-downloads` (the move may be deferred while streamed).
    Moving,
    /// The job failed (engine died, dead magnet, move failure, …).
    Failed,
    /// The user stopped the job (torrent forgotten, partials kept).
    Stopped,
    /// Round 56 (56-1): the download finished and the kept files were
    /// moved to `s2udio-downloads`. Round 56.6: the row stays listed
    /// (`done_at` + `moved_to`) until the user removes it — completed
    /// downloads are never pruned automatically.
    Completed,
}
impl std::fmt::Display for DlStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Queued => "queued",
            Self::Adding => "adding",
            Self::Downloading => "downloading",
            Self::Moving => "moving",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
        };
        f.write_str(s)
    }
}
impl DlStatus {
    /// Whether the daemon is still working on this job.
    pub fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Adding | Self::Downloading | Self::Moving)
    }
}

/// One kept file of a commit job (the file to keep in `s2udio-downloads`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DlKeptFile {
    /// Positional index in the torrent's file list (stream endpoint +
    /// `file_progress` addressing).
    pub index: usize,
    /// File name (display + the moved file's name).
    pub name: String,
    /// Byte length (completion check against `stats.file_progress`).
    pub length: u64,
}

/// The job's engine view in the state file: proxy URLs only, no tokens
/// (precedent: `rqbit.json` stores only the proxy web URL).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DlEngineInfo {
    /// `http://127.0.0.1:<proxy port>` — the auth-injecting proxy base;
    /// the TUI reads progress through it without the token.
    pub proxy_url: String,
    /// The engine's own HTTP API port (behind the proxy; informational).
    pub engine_port: u16,
    /// The engine's cache/download dir (informational).
    pub cache_dir: String,
}

/// One committed download job, as serialized into `downloads.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlJob {
    pub job_id: String,
    /// The enqueue request id that created the job (stop-by-request).
    pub request_id: String,
    /// Canonical lowercase infohash — the dedup key (one job per torrent).
    /// `None` until the engine reports it (a `.torrent` file's infohash
    /// is only known after the add).
    #[serde(default)]
    pub infohash: Option<String>,
    /// The TUI-side canonical scan key (magnet infohash / `.torrent`
    /// path) — secondary identity while the infohash is unknown.
    pub source_key: String,
    /// The torrent the job adds (kept so a resumed daemon can re-add).
    pub torrent_item: DlTorrentItem,
    /// The torrent's display name (rqbit details, or the item label).
    pub torrent_name: String,
    /// The files to keep in `s2udio-downloads` once the download is done.
    pub kept_files: Vec<DlKeptFile>,
    pub status: DlStatus,
    /// Overall progress of the kept files (0-100).
    pub progress_percent: f64,
    /// The engine behind this job (proxy URLs only), once running.
    pub engine: Option<DlEngineInfo>,
    /// The torrent's id on the engine.
    pub torrent_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Unix seconds of the last state change (UI ordering).
    pub updated_at: u64,
    /// Round 56 (56-1): unix seconds when the download completed (set on
    /// `Completed` only).
    #[serde(default)]
    pub done_at: Option<u64>,
    /// Round 56 (56-1): the destination folder the kept files were moved
    /// to (set on `Completed` only — `~/Downloads/s2udio-downloads`).
    #[serde(default)]
    pub moved_to: Option<String>,
}

/// The shared state file: daemon identity + every job.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DlStateFile {
    /// The daemon pid (0 = not running; jobs may still be listed).
    pub daemon_pid: u32,
    pub started_at: u64,
    #[serde(default)]
    pub jobs: Vec<DlJob>,
}

/// A pasted torrent source in its spool-request form.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum DlTorrentItem {
    Magnet(String),
    Torrent(String),
}
impl From<&DlTorrentItem> for crate::core::torrent::TorrentItem {
    fn from(item: &DlTorrentItem) -> Self {
        match item {
            DlTorrentItem::Magnet(magnet) => Self::Magnet(magnet.clone()),
            DlTorrentItem::Torrent(torrent) => Self::Torrent(torrent.clone()),
        }
    }
}
impl From<crate::core::torrent::TorrentItem> for DlTorrentItem {
    fn from(item: crate::core::torrent::TorrentItem) -> Self {
        match item {
            crate::core::torrent::TorrentItem::Magnet(magnet) => Self::Magnet(magnet),
            crate::core::torrent::TorrentItem::Torrent(torrent) => Self::Torrent(torrent),
        }
    }
}
impl From<DlTorrentItem> for crate::core::torrent::TorrentItem {
    fn from(item: DlTorrentItem) -> Self {
        match item {
            DlTorrentItem::Magnet(magnet) => Self::Magnet(magnet),
            DlTorrentItem::Torrent(torrent) => Self::Torrent(torrent),
        }
    }
}
impl DlTorrentItem {
    /// The display label (magnet infohash prefix / `.torrent` file name).
    pub fn label(&self) -> String {
        match self {
            Self::Magnet(magnet) => {
                crate::ui::modals::paste::magnet_infohash(magnet)
                    .map_or_else(|| "magnet link".to_owned(), |hash| format!("magnet:{hash}"))
            }
            Self::Torrent(path) => Path::new(path)
                .file_name()
                .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned()),
        }
    }
}

/// A spool request: the TUI writes one file per request, the daemon
/// consumes (and deletes) it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum DlJobRequest {
    /// A committed action: download (and optionally stream) the torrent.
    /// Deduplicated by infohash — an existing job's kept-file list is
    /// extended instead of starting a second engine.
    Enqueue {
        id: String,
        #[serde(default)]
        infohash: Option<String>,
        source_key: String,
        torrent_item: DlTorrentItem,
        #[serde(default)]
        torrent_name: Option<String>,
        /// The committed kept files (name/length known from the scan).
        /// Empty = the daemon picks the single best playable file once the
        /// file list is known.
        #[serde(default)]
        files: Vec<DlKeptFile>,
        /// `true` for "Stream and download" / "Download & Play": the
        /// daemon writes the stream-URL response once the torrent is
        /// added, and the TUI plays through the daemon engine.
        #[serde(default)]
        play: bool,
    },
    /// A plain re-stream of a torrent that has an ACTIVE committed job:
    /// route playback through the daemon engine (no second TUI engine on
    /// the cache dir). Does not extend the job's kept-file list.
    Stream {
        id: String,
        #[serde(default)]
        infohash: Option<String>,
        source_key: String,
        torrent_item: DlTorrentItem,
        #[serde(default)]
        torrent_name: Option<String>,
        /// The file indices to stream (the play selection). Empty = the
        /// single best playable file.
        #[serde(default)]
        file_indices: Vec<usize>,
    },
    /// Stop a job (the Downloads modal's "Stop download", or Esc on the
    /// "Preparing downloader…" wait window): the torrent is forgotten
    /// (partials kept), the job is dropped. Matched by job id, then by
    /// the creating request id, then by infohash.
    Stop {
        id: String,
        #[serde(default)]
        job_id: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        infohash: Option<String>,
    },
    /// Round 56.6 (56.6-2): remove a TERMINAL job's row from
    /// `downloads.json` (the Downloads modal's "Remove from list" on a
    /// Completed/Stopped/Failed row — the downloaded files are never
    /// touched). Matched exactly like `Stop` (job id, then creating
    /// request id, then infohash). Active rows keep using `Stop`.
    Remove {
        id: String,
        #[serde(default)]
        job_id: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        infohash: Option<String>,
    },
}

/// One file of a daemon response with its stream URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DlResponseFile {
    pub index: usize,
    pub name: String,
    pub length: u64,
    /// `http://user:pass@127.0.0.1:<port>/torrents/<id>/stream/<idx>` —
    /// token embedded as URL userinfo for mpv.
    pub stream_url: String,
}

/// The response the daemon writes for an enqueue-with-play or stream
/// request: the torrent id + the stream URLs the TUI plays through the
/// daemon engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlJobResponse {
    pub job_id: String,
    pub request_id: String,
    pub torrent_id: String,
    pub torrent_name: String,
    pub engine: DlEngineInfo,
    /// The files to play (stream URLs in play order).
    pub files: Vec<DlResponseFile>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The TUI's streaming marker content (R2.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingMarker {
    /// The TUI process that wrote it (alive = the marker is fresh).
    pub pid: u32,
    /// Job ids the TUI is currently streaming (one mpv session at a time).
    #[serde(default)]
    pub jobs: Vec<String>,
}

/// `~/.cache/s2udio/downloads.json`.
pub fn state_path() -> Option<PathBuf> {
    crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join(STATE_FILE))
}
/// `~/.cache/s2udio/dl-jobs`.
pub fn jobs_dir() -> Option<PathBuf> {
    crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join(JOBS_DIR))
}
/// `~/.cache/s2udio/dl-responses`.
pub fn responses_dir() -> Option<PathBuf> {
    crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join(RESPONSES_DIR))
}
/// `~/.cache/s2udio/dl-tokens`.
pub fn tokens_dir() -> Option<PathBuf> {
    crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join(TOKENS_DIR))
}
/// `~/.cache/s2udio/dl-streaming.json`.
pub fn streaming_marker_path() -> Option<PathBuf> {
    crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join(STREAMING_MARKER))
}

/// Read the state file (missing/unparsable -> None).
pub fn read_state() -> Option<DlStateFile> {
    let path = state_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}
/// Persist the state file (creates the cache dir as needed). Written
/// atomically (tmp + rename) so a reader never sees a torn file — the
/// TUI also edits `downloads.json` directly when the daemon is dead
/// (round 56.6-2).
pub fn write_state(state: &DlStateFile) -> Result<(), String> {
    let Some(path) = state_path() else {
        return Err("Could not determine the s2udio cache dir".to_owned());
    };
    let content = serde_json::to_string_pretty(state)
        .map_err(|err| format!("Failed to serialize downloads.json: {err}"))?;
    let Some(parent) = path.parent() else {
        return Err("Bad downloads.json path".to_owned());
    };
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    let tmp = parent.join(".downloads.json.tmp");
    std::fs::write(&tmp, content)
        .map_err(|err| format!("Cannot write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|err| format!("Cannot move {} into place: {err}", path.display()))
}

/// Whether the registered daemon pid is a live process.
pub fn daemon_running(state: &DlStateFile) -> bool {
    state.daemon_pid != 0
        && crate::core::rqctl::pid_alive(state.daemon_pid)
}
/// The number of jobs the daemon is currently working on.
pub fn active_job_count(state: &DlStateFile) -> usize {
    state.jobs.iter().filter(|job| job.status.active()).count()
}

/// Round 56 (56-1): the one-shot status-line notice for a `Completed`
/// job — "Download complete: <name> → ~/Downloads/s2udio-downloads".
pub fn completion_notice(job: &DlJob) -> String {
    let dest = job
        .moved_to
        .as_deref()
        .unwrap_or("~/Downloads/s2udio-downloads");
    format!("Download complete: {} → {dest}", job.torrent_name)
}
/// Whether any ACTIVE committed job matches the infohash (the TUI's
/// re-stream routing rule: playback goes through the daemon engine).
pub fn job_active_for_infohash(state: &DlStateFile, infohash: &str) -> bool {
    let hash = infohash.to_lowercase();
    daemon_running(state)
        && state
            .jobs
            .iter()
            .any(|job| job.status.active() && job.infohash.as_deref() == Some(hash.as_str()))
}

/// The download destination: `~/Downloads/s2udio-downloads` (same folder
/// as stream downloads; the browser lists it from disk).
pub fn downloads_dir() -> Option<PathBuf> {
    crate::ui::modals::paste::downloads_dir()
}

/// A fresh unique request id (`req-<16 hex>`).
pub fn new_request_id() -> String {
    format!("req-{:016x}", rand::random::<u64>())
}
/// The spool file path of a request.
pub fn request_path(request_id: &str) -> Option<PathBuf> {
    jobs_dir().map(|dir| dir.join(format!("{request_id}.json")))
}
/// The response file path of a request.
pub fn response_path(request_id: &str) -> Option<PathBuf> {
    responses_dir().map(|dir| dir.join(format!("{request_id}.json")))
}
/// The token sidecar path of a job.
pub fn token_path(job_id: &str) -> Option<PathBuf> {
    tokens_dir().map(|dir| dir.join(job_id))
}

/// Write a spool request atomically (tmp + rename so the daemon never
/// sees a torn file).
pub fn write_request(request: &DlJobRequest) -> Result<(), String> {
    let id = match request {
        DlJobRequest::Enqueue { id, .. }
        | DlJobRequest::Stream { id, .. }
        | DlJobRequest::Stop { id, .. }
        | DlJobRequest::Remove { id, .. } => id,
    };
    let Some(final_path) = request_path(id) else {
        return Err("Could not determine the s2udio cache dir".to_owned());
    };
    let content = serde_json::to_string_pretty(request)
        .map_err(|err| format!("Failed to serialize the downloader request: {err}"))?;
    let Some(parent) = final_path.parent() else {
        return Err("Bad downloader spool path".to_owned());
    };
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    let tmp = parent.join(format!(".{id}.tmp"));
    std::fs::write(&tmp, &content)
        .map_err(|err| format!("Cannot write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &final_path)
        .map_err(|err| format!("Cannot move {} into the spool: {err}", final_path.display()))
}
/// Write a stop request (the Downloads modal's "Stop download" / Esc on
/// the wait window). Matched by job id, request id or infohash.
pub fn write_stop_request(
    job_id: Option<&str>,
    request_id: Option<&str>,
    infohash: Option<&str>,
) -> Result<(), String> {
    write_request(&DlJobRequest::Stop {
        id: new_request_id(),
        job_id: job_id.map(str::to_owned),
        request_id: request_id.map(str::to_owned),
        infohash: infohash.map(|h| h.to_lowercase()),
    })
    .map_err(|err| {
        log::debug!(job_id:?, request_id:?, error:? = err; "Failed to write a stop request for the downloader daemon");
        format!("Cannot reach the downloader daemon: {err}")
    })
}

/// Round 56.6 (56.6-2): write a remove request (the Downloads modal's
/// "Remove from list" on a Completed/Stopped/Failed row — the daemon
/// drops the row; the downloaded files stay). Matched by job id, request
/// id or infohash, exactly like a stop.
pub fn write_remove_request(
    job_id: Option<&str>,
    request_id: Option<&str>,
    infohash: Option<&str>,
) -> Result<(), String> {
    write_request(&DlJobRequest::Remove {
        id: new_request_id(),
        job_id: job_id.map(str::to_owned),
        request_id: request_id.map(str::to_owned),
        infohash: infohash.map(|h| h.to_lowercase()),
    })
    .map_err(|err| {
        log::debug!(job_id:?, request_id:?, error:? = err; "Failed to write a remove request for the downloader daemon");
        format!("Cannot reach the downloader daemon: {err}")
    })
}

/// Round 56.6 (56.6-2): remove a TERMINAL job's row directly from
/// `downloads.json` — the TUI's path when the daemon is NOT running (no
/// daemon writer to race; the other rows are preserved). Refuses to run
/// while a daemon is alive (then the TUI must spool a `Remove` instead).
/// Removing the last row deletes the state file — a clean state (the
/// daemon's own clean-exit shape). The downloaded files are never
/// touched.
pub fn remove_job_offline(job_id: &str) -> Result<(), String> {
    let mut state =
        read_state().ok_or_else(|| "No downloader state file to edit".to_owned())?;
    if daemon_running(&state) {
        return Err("The downloader daemon is running — spool a remove instead".to_owned());
    }
    let before = state.jobs.len();
    state.jobs.retain(|job| job.job_id != job_id);
    if state.jobs.len() == before {
        return Ok(());
    }
    if state.jobs.is_empty() {
        if let Some(path) = state_path() {
            let _ = std::fs::remove_file(path);
        }
        return Ok(());
    }
    write_state(&state)
}

/// Round 56.6 (56.6-4): whether a `Completed` job finished within the
/// startup notice window — the TUI surfaces these once at launch; older
/// persisted rows stay listed quietly (their ids still seed the seen-set
/// so a later poll does not notice them either).
pub fn completed_recently(job: &DlJob) -> bool {
    job.done_at.is_some_and(|t| {
        now_unix().saturating_sub(t) < COMPLETED_NOTICE_STARTUP_WINDOW.as_secs()
    })
}

// ============================================================================
// CLI entry points (`s2udio dl start|status|stop`) and the daemon (`serve`)
// ============================================================================

/// `s2udio dl start|status|stop|serve`.
pub fn run(cmd: DlCmd) -> Result<(), String> {
    match cmd {
        DlCmd::Start => start(),
        DlCmd::Status => status(),
        DlCmd::Stop => stop(),
        DlCmd::Serve => serve(),
    }
}

/// Print one summary line per job (also used by `start`).
fn print_job_summary(state: &DlStateFile) {
    if daemon_running(state) {
        println!(
            "downloader daemon: RUNNING (pid {}, started {})",
            state.daemon_pid, state.started_at
        );
    } else {
        println!("downloader daemon: not running (pid 0)");
    }
    if state.jobs.is_empty() {
        println!("torrent downloads: none");
        return;
    }
    for job in &state.jobs {
        let progress = if job.status.active() {
            format!("{:.0}%", job.progress_percent)
        } else {
            "--".to_owned()
        };
        let kept = match job.kept_files.len() {
            0 => String::new(),
            1 => " (1 file)".to_owned(),
            n => format!(" ({n} files)"),
        };
        let error = job
            .error
            .as_ref()
            .map(|e| format!(" — {e}"))
            .unwrap_or_default();
        println!(
            "  [{}] {} {}{}{}",
            job.status, progress, job.torrent_name, kept, error
        );
    }
}

/// Start the daemon detached (idempotent).
fn start() -> Result<(), String> {
    start_daemon_for_tui()?;
    match read_state() {
        Some(state) => {
            print_job_summary(&state);
            println!("the daemon exits itself when the last download finishes");
        }
        None => {
            // The daemon started and immediately exited: there was nothing
            // in the spool (a manual `dl start` with no pending work).
            println!("downloader daemon started; no jobs to run");
        }
    }
    Ok(())
}
/// Spawn the detached daemon if none is running and wait for its READY
/// line (≤ 15 s). Prints nothing — the TUI's work thread calls this from
/// inside the TUI (stdout is the terminal).
pub fn start_daemon_for_tui() -> Result<(), String> {
    if let Some(state) = read_state()
        && daemon_running(&state)
    {
        return Ok(());
    }
    let exe = std::env::current_exe()
        .map_err(|err| format!("Cannot find the s2udio binary: {err}"))?;
    let mut child = std::process::Command::new(&exe)
        .arg("dl")
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| format!("Failed to spawn the downloader daemon: {err}"))?;
    let mut stdout = child.stdout.take().expect("daemon stdout is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        use std::io::BufRead;
        let _ = std::io::BufReader::new(&mut stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    match rx.recv_timeout(DAEMON_READY_TIMEOUT) {
        Ok(line) if line.starts_with("READY") => {
            reap_daemon_child(child);
        }
        Ok(other) => {
            let _ = child.kill();
            reap_daemon_child(child);
            return Err(format!("downloader daemon failed to start: {other}"));
        }
        Err(_) => {
            let _ = child.kill();
            reap_daemon_child(child);
            return Err("downloader daemon did not become ready within 15 s".to_owned());
        }
    }
    Ok(())
}

/// Round 56 (56-3): reap the daemon child once it exits. The daemon is
/// spawned detached and outlives the READY handshake; a child that is
/// never `wait()`ed leaves a zombie under the TUI (observed after every
/// daemon exit), accumulating across a long-lived session.
fn reap_daemon_child(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// `s2udio dl status` — the shell view of `downloads.json`.
fn status() -> Result<(), String> {
    let Some(state) = read_state() else {
        println!("downloader daemon: not running (no state yet)");
        return Ok(());
    };
    if daemon_running(&state) && active_job_count(&state) == 0 {
        println!("downloader daemon: RUNNING (pid {})", state.daemon_pid);
        println!("torrent downloads: none active");
        return Ok(());
    }
    print_job_summary(&state);
    Ok(())
}

/// `s2udio dl stop` — SIGTERM (then SIGKILL after ~2 s) the daemon; the
/// state file keeps the last job statuses with a dead pid (the TUI shows
/// them as offline). In-flight downloads die with their engines; partials
/// stay in the torrent cache.
fn stop() -> Result<(), String> {
    let Some(state) = read_state() else {
        return Err("No downloader daemon state found".to_owned());
    };
    if !daemon_running(&state) {
        println!("downloader daemon is not running");
        return Ok(());
    }
    crate::core::rqctl::kill_pid(state.daemon_pid);
    let mut state = state;
    state.daemon_pid = 0;
    state.started_at = 0;
    let _ = write_state(&state);
    println!("downloader daemon stopped (partials stay in the torrent cache)");
    Ok(())
}

/// The daemon's shutdown flag (SIGTERM/SIGINT -> graceful exit).
static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// A pending response request (answered once the job's engine is ready).
struct PendingResponse {
    request_id: String,
    kind: ResponseKind,
}
enum ResponseKind {
    /// The job's kept files (an enqueue with `play: true`).
    EnqueuePlay,
    /// The requested stream indices (a plain re-stream through the daemon).
    Stream(Vec<usize>),
}

/// The result of a job's add thread.
enum AddOutcome {
    Ready {
        engine: crate::core::torrent::TorrentEngine,
        torrent_id: String,
        torrent_name: String,
        details: crate::core::torrent::TorrentDetails,
    },
    Cancelled,
    Failed(String),
}

/// Per-job daemon state (engine + add bookkeeping + pending responses).
struct JobRuntime {
    job: DlJob,
    engine: Option<crate::core::torrent::TorrentEngine>,
    /// The torrent's file list once known (names/lengths for stream
    /// responses and the daemon-side kept-file pick).
    details: Option<crate::core::torrent::TorrentDetails>,
    add_result: Option<crossbeam::channel::Receiver<AddOutcome>>,
    add_cancel: Option<crossbeam::channel::Sender<()>>,
    pending_responses: Vec<PendingResponse>,
    /// When the completed job first tried its (possibly deferred) move.
    move_started: Option<Instant>,
}

/// The daemon: owns the engines, consumes the spool, writes the state.
struct Daemon {
    config: std::sync::Arc<crate::config::torrent::Torrent>,
    started_at: u64,
    runtimes: Vec<JobRuntime>,
    dirty: bool,
}

impl Daemon {
    fn new() -> Result<Self, String> {
        let config = std::sync::Arc::new(crate::core::rqctl::torrent_config());
        let started_at = now_unix();
        let mut daemon = Self {
            config,
            started_at,
            runtimes: Vec::new(),
            dirty: false,
        };
        daemon.adopt_existing_jobs();
        Ok(daemon)
    }

    /// Resume jobs a previous daemon instance left in `downloads.json`
    /// (a crashed/killed daemon): active jobs are re-added (engines die
    /// with the machine; the re-add adopts the surviving cache partials),
    /// terminal ones stay listed until removed. Round 56.6 (56.6-1):
    /// `Completed` rows are NEVER pruned on restart — completed
    /// downloads persist until the user removes them.
    fn adopt_existing_jobs(&mut self) {
        let Some(state) = read_state() else { return };
        for job in state.jobs {
            let mut job = job;
            if job.status.active() {
                job.status = DlStatus::Queued;
                job.error = None;
                job.progress_percent = 0.0;
                self.runtimes.push(JobRuntime {
                    job,
                    engine: None,
                    details: None,
                    add_result: None,
                    add_cancel: None,
                    pending_responses: Vec::new(),
                    move_started: None,
                });
                let len = self.runtimes.len();
                self.start_add(len - 1);
            } else {
                self.runtimes.push(JobRuntime {
                    job,
                    engine: None,
                    details: None,
                    add_result: None,
                    add_cancel: None,
                    pending_responses: Vec::new(),
                    move_started: None,
                });
            }
        }
        self.dirty = true;
    }

    fn run(mut self) -> Result<(), String> {
        unsafe {
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
        self.dirty = true;
        self.write_state()?;
        println!("READY");
        let mut last_poll = Instant::now();
        loop {
            if SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            self.process_spool();
            self.resume_adds();
            if last_poll.elapsed() >= JOB_POLL_INTERVAL {
                last_poll = Instant::now();
                self.poll_jobs();
            }
            if self.dirty {
                self.write_state()?;
                self.dirty = false;
            }
            if !self.has_work() {
                break;
            }
            std::thread::sleep(SPOOL_POLL_INTERVAL);
        }
        if SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            // Graceful stop: engines die with the daemon; the state keeps
            // the last job statuses (pid 0 = offline) for the TUI.
            let mut state = self.to_state();
            state.daemon_pid = 0;
            state.started_at = 0;
            let _ = write_state(&state);
        } else if self.runtimes.is_empty() && !self.has_work() {
            // Clean exit (last job done): remove the whole state.
            if let Some(path) = state_path() {
                let _ = std::fs::remove_file(path);
            }
        } else {
            // Only terminal jobs remain: leave them visible, pid 0.
            let mut state = self.to_state();
            state.daemon_pid = 0;
            state.started_at = 0;
            let _ = write_state(&state);
        }
        Ok(())
    }

    fn to_state(&self) -> DlStateFile {
        DlStateFile {
            daemon_pid: std::process::id(),
            started_at: self.started_at,
            jobs: self
                .runtimes
                .iter()
                .map(|runtime| runtime.job.clone())
                .collect(),
        }
    }

    fn write_state(&self) -> Result<(), String> {
        write_state(&self.to_state())
    }

    /// Whether the daemon must keep running: an active job, an in-flight
    /// add, or a pending response the user is waiting for. Terminal rows
    /// (`Completed`/`Stopped`/`Failed`) never count — round 56.6
    /// (56.6-1): completed rows persist in `downloads.json` until the
    /// user removes them; with only terminal rows the daemon exits via
    /// the pid-0 branch, leaving them listed.
    fn has_work(&self) -> bool {
        self.runtimes.iter().any(|runtime| {
            runtime.job.status.active()
                || runtime.add_result.is_some()
                || !runtime.pending_responses.is_empty()
        })
    }

    fn index_of(&self, job_id: &str) -> Option<usize> {
        self.runtimes.iter().position(|r| r.job.job_id == job_id)
    }

    /// Find a job by infohash (canonical) or, while the infohash is still
    /// unknown, by the TUI's source key (the `.torrent` pre-add case).
    fn find_existing(&self, infohash: Option<&str>, source_key: Option<&str>) -> Option<usize> {
        if let Some(hash) = infohash.filter(|h| !h.is_empty()) {
            if let Some(idx) = self
                .runtimes
                .iter()
                .position(|r| r.job.infohash.as_deref() == Some(hash))
            {
                return Some(idx);
            }
        }
        source_key.and_then(|key| {
            self.runtimes
                .iter()
                .position(|r| r.job.infohash.is_none() && r.job.source_key == key)
        })
    }

    fn find_by_request(&self, request_id: &str) -> Option<usize> {
        self.runtimes
            .iter()
            .position(|r| r.job.request_id == request_id)
    }

    /// Consume the spool directory.
    fn process_spool(&mut self) {
        let Some(dir) = jobs_dir() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        for path in files {
            let request = match std::fs::read_to_string(&path)
                .ok()
                .and_then(|content| serde_json::from_str::<DlJobRequest>(&content).ok())
            {
                Some(request) => request,
                None => {
                    // A torn write? Retry for a bounded time, then drop.
                    match std::fs::metadata(&path).and_then(|m| m.modified()) {
                        Ok(modified)
                            if modified.elapsed().unwrap_or_default() < SPOOL_PARSE_RETRY =>
                        {
                            continue;
                        }
                        _ => {
                            log::warn!(path:?; "Dropping an unparsable downloader spool file");
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                    continue;
                }
            };
            log::debug!(request:?; "Downloader daemon consumed a spool request");
            self.handle_request(request);
            let _ = std::fs::remove_file(&path);
            self.dirty = true;
        }
    }

    fn handle_request(&mut self, request: DlJobRequest) {
        match request {
            DlJobRequest::Enqueue {
                id,
                infohash,
                source_key,
                torrent_item,
                torrent_name,
                files,
                play,
            } => {
                let hash = infohash.map(|h| h.to_lowercase());
                if let Some(idx) = self.find_existing(hash.as_deref(), Some(&source_key)) {
                    self.merge_enqueue(idx, &id, files, play, hash);
                    return;
                }
                let name = torrent_name.unwrap_or_else(|| torrent_item.label());
                let job = DlJob {
                    job_id: format!("dl-{:016x}", rand::random::<u64>()),
                    request_id: id.clone(),
                    infohash: hash,
                    source_key,
                    torrent_item,
                    torrent_name: name,
                    kept_files: files,
                    status: DlStatus::Queued,
                    progress_percent: 0.0,
                    engine: None,
                    torrent_id: None,
                    error: None,
                    updated_at: now_unix(),
                    done_at: None,
                    moved_to: None,
                };
                self.runtimes.push(JobRuntime {
                    job,
                    engine: None,
                    details: None,
                    add_result: None,
                    add_cancel: None,
                    pending_responses: Vec::new(),
                    move_started: None,
                });
                let len = self.runtimes.len();
                self.start_add(len - 1);
                if play {
                    self.runtimes[len - 1].pending_responses.push(PendingResponse {
                        request_id: id,
                        kind: ResponseKind::EnqueuePlay,
                    });
                }
            }
            DlJobRequest::Stream {
                id,
                infohash,
                source_key,
                torrent_item: _,
                torrent_name: _,
                file_indices,
            } => {
                let hash = infohash.map(|h| h.to_lowercase());
                let Some(idx) = self.find_existing(hash.as_deref(), Some(&source_key)) else {
                    self.respond_error(
                        &id,
                        "No active downloader job for this torrent — plain streams of torrents without a committed download run on the TUI engine",
                    );
                    return;
                };
                let pending = PendingResponse {
                    request_id: id,
                    kind: ResponseKind::Stream(file_indices),
                };
                if self.runtimes[idx].engine.is_some() {
                    self.respond(idx, pending);
                } else if self.runtimes[idx].job.status.active()
                    || self.runtimes[idx].add_result.is_some()
                {
                    self.runtimes[idx].pending_responses.push(pending);
                } else {
                    self.respond_error(&pending.request_id, "The downloader job for this torrent is not active");
                }
            }
            // `Stop` (active rows) and `Remove` (terminal rows, round
            // 56.6-2) resolve their target identically — by job id, then
            // creating request id, then infohash — and then drop the job
            // (`stop_job` cancels the add, forgets the engine, removes
            // the token sidecar + pending responses and removes the row;
            // for a terminal row all of that is a no-op except the drop).
            DlJobRequest::Stop {
                job_id,
                request_id,
                infohash,
                id: _,
            }
            | DlJobRequest::Remove {
                job_id,
                request_id,
                infohash,
                id: _,
            } => {
                let target = job_id
                    .as_deref()
                    .and_then(|id| self.index_of(id))
                    .or_else(|| request_id.as_deref().and_then(|r| self.find_by_request(r)))
                    .or_else(|| {
                        infohash
                            .as_deref()
                            .and_then(|h| self.find_existing(Some(h), None))
                    });
                if let Some(idx) = target {
                    self.stop_job(idx);
                }
            }
        }
    }

    /// A second committed action on a torrent that already has a job:
    /// extend the kept-file list (union by index) and answer any
    /// play/stream requests against the existing engine.
    fn merge_enqueue(
        &mut self,
        idx: usize,
        request_id: &str,
        files: Vec<DlKeptFile>,
        play: bool,
        infohash: Option<String>,
    ) {
        if let Some(infohash) = infohash {
            self.runtimes[idx].job.infohash = Some(infohash);
        }
        let existing_indices: std::collections::HashSet<usize> = self.runtimes[idx]
            .job
            .kept_files
            .iter()
            .map(|f| f.index)
            .collect();
        for file in files {
            if existing_indices.contains(&file.index) {
                continue;
            }
            self.runtimes[idx].job.kept_files.push(file);
        }
        // A stopped/failed job is revived by the new committed action.
        if !self.runtimes[idx].job.status.active() {
            self.runtimes[idx].job.status = DlStatus::Queued;
            self.runtimes[idx].job.error = None;
            self.start_add(idx);
        }
        if play {
            let pending = PendingResponse {
                request_id: request_id.to_owned(),
                kind: ResponseKind::EnqueuePlay,
            };
            if self.runtimes[idx].engine.is_some() {
                self.respond(idx, pending);
            } else {
                self.runtimes[idx].pending_responses.push(pending);
            }
        }
        self.dirty = true;
    }

    /// Spawn the add thread for runtime `idx` (engine start + add +
    /// open-ended metainfo wait; cancellable).
    fn start_add(&mut self, idx: usize) {
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        let (result_tx, result_rx) = crossbeam::channel::bounded(1);
        {
            let runtime = &mut self.runtimes[idx];
            runtime.add_cancel = Some(cancel_tx);
            runtime.add_result = Some(result_rx);
            runtime.job.status = DlStatus::Adding;
        }
        let config = std::sync::Arc::clone(&self.config);
        let item: crate::core::torrent::TorrentItem = self.runtimes[idx].job.torrent_item.clone().into();
        let label = self.runtimes[idx].job.torrent_item.label();
        let thread_name = format!(
            "dl-add-{}",
            item.source_key().chars().take(24).collect::<String>()
        );
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let outcome = (|| {
                    if cancel_rx.try_recv().is_ok() {
                        return AddOutcome::Cancelled;
                    }
                    let engine = match crate::core::torrent::start_engine(&config) {
                        Ok(engine) => engine,
                        Err(err) => return AddOutcome::Failed(err),
                    };
                    if cancel_rx.try_recv().is_ok() {
                        return AddOutcome::Cancelled;
                    }
                    let id = match crate::core::torrent::add_torrent(&engine, item.source()) {
                        Ok(id) => id,
                        Err(err) => return AddOutcome::Failed(err),
                    };
                    if cancel_rx.try_recv().is_ok() {
                        let _ = crate::core::torrent::forget_torrent(&engine, &id);
                        return AddOutcome::Cancelled;
                    }
                    let details = match crate::core::torrent::wait_for_files(
                        &engine,
                        &id,
                        Some(&cancel_rx),
                    ) {
                        Ok(details) => details,
                        Err(err) if err.contains("Scan cancelled") => return AddOutcome::Cancelled,
                        Err(err) => return AddOutcome::Failed(err),
                    };
                    if cancel_rx.try_recv().is_ok() {
                        let _ = crate::core::torrent::forget_torrent(&engine, &id);
                        return AddOutcome::Cancelled;
                    }
                    let torrent_name = details.name.clone().unwrap_or_else(|| label.clone());
                    AddOutcome::Ready {
                        engine,
                        torrent_id: id,
                        torrent_name,
                        details,
                    }
                })();
                let _ = result_tx.send(outcome);
            });
        if let Err(err) = spawn_result {
            log::error!(error:? = err; "Failed to spawn the downloader add thread");
            let runtime = &mut self.runtimes[idx];
            runtime.job.status = DlStatus::Failed;
            runtime.job.error = Some(format!("Failed to spawn the add thread: {err}"));
            self.dirty = true;
        }
    }

    /// Collect finished add threads.
    fn resume_adds(&mut self) {
        let mut pending: Vec<usize> = self
            .runtimes
            .iter()
            .enumerate()
            .filter(|(_, r)| r.add_result.is_some())
            .map(|(i, _)| i)
            .collect();
        for idx in pending.drain(..) {
            let outcome = self
                .runtimes
                .get(idx)
                .and_then(|r| r.add_result.as_ref())
                .and_then(|rx| rx.try_recv().ok());
            let Some(outcome) = outcome else { continue };
            match outcome {
                AddOutcome::Ready { engine, torrent_id, torrent_name, details } => {
                    self.on_add_ready(idx, engine, torrent_id, torrent_name, details);
                }
                AddOutcome::Cancelled => {
                    self.remove_runtime(idx);
                }
                AddOutcome::Failed(err) => {
                    let ids: Vec<String> = self.runtimes[idx]
                        .pending_responses
                        .iter()
                        .map(|p| p.request_id.clone())
                        .collect();
                    self.runtimes[idx].pending_responses.clear();
                    for request_id in ids {
                        self.respond_error(&request_id, &err);
                    }
                    self.runtimes[idx].job.status = DlStatus::Failed;
                    self.runtimes[idx].job.error = Some(err);
                    self.runtimes[idx].add_result = None;
                    self.runtimes[idx].add_cancel = None;
                    self.dirty = true;
                }
            }
        }
    }

    fn on_add_ready(
        &mut self,
        idx: usize,
        engine: crate::core::torrent::TorrentEngine,
        torrent_id: String,
        torrent_name: String,
        details: crate::core::torrent::TorrentDetails,
    ) {
        let reported_hash = details.info_hash.as_deref().map(|h| h.to_lowercase());
        // Dedup by infohash: the same torrent on two engines (a `.torrent`
        // enqueued twice whose infohash was unknown at enqueue time) must
        // collapse onto one engine — forget the duplicate and merge. Match
        // on BOTH the engine-reported hash and the job's own (the TUI's
        // magnet-derived canonical hash must win when they differ).
        let own_hash = self.runtimes[idx].job.infohash.clone();
        let dup = reported_hash
            .as_deref()
            .or(own_hash.as_deref())
            .and_then(|hash| {
                self.runtimes
                    .iter()
                    .position(|r| r.job.infohash.as_deref() == Some(hash) && r.engine.is_some())
            });
        if let Some(dup) = dup
            && dup != idx
        {
            self.merge_into(dup, idx);
            return;
        }
        let runtime = &mut self.runtimes[idx];
        // Keep the TUI's canonical infohash when the enqueue carried one
        // (the magnet's xt=urn:btih: — rqbit's report should agree, but
        // the TUI's dedup/routing keys on the magnet string); adopt the
        // engine-reported hash for `.torrent` enqueues that had none.
        if runtime.job.infohash.is_none() {
            runtime.job.infohash = reported_hash;
        }
        runtime.job.torrent_id = Some(torrent_id.clone());
        runtime.job.torrent_name = torrent_name;
        runtime.job.error = None;
        // The daemon-side pick: an enqueue without file info keeps the
        // single best playable file.
        if runtime.job.kept_files.is_empty() {
            if let Some((file_idx, file)) = crate::core::torrent::pick_playable_file(&details.files) {
                runtime.job.kept_files.push(DlKeptFile {
                    index: file_idx,
                    name: file.name.clone(),
                    length: file.length,
                });
            }
        }
        let empty_kept_ids: Option<Vec<String>> = if runtime.job.kept_files.is_empty() {
            // Nothing playable: fail the job like the scan path would.
            let ids: Vec<String> = runtime
                .pending_responses
                .iter()
                .map(|p| p.request_id.clone())
                .collect();
            runtime.pending_responses.clear();
            runtime.job.status = DlStatus::Failed;
            runtime.job.error = Some("No playable media in this torrent".to_owned());
            runtime.add_result = None;
            runtime.add_cancel = None;
            self.dirty = true;
            Some(ids)
        } else {
            None
        };
        if let Some(ids) = empty_kept_ids {
            for request_id in ids {
                self.respond_error(&request_id, "No playable media in this torrent");
            }
            return;
        }
        runtime.engine = Some(engine);
        runtime.details = Some(details);
        runtime.job.status = DlStatus::Downloading;
        // Token sidecar (0600): the durable per-job token store.
        if let Some(user_pass) = runtime.engine.as_ref().map(|e| e.auth_user_pass().to_owned()) {
            let _ = write_token_sidecar(&runtime.job.job_id, &user_pass);
        }
        runtime.job.engine = Some(DlEngineInfo {
            proxy_url: runtime.engine.as_ref().map(crate::core::torrent::TorrentEngine::proxy_base_url).unwrap_or_default(),
            engine_port: runtime.engine.as_ref().map(crate::core::torrent::TorrentEngine::http_port).unwrap_or(0),
            cache_dir: runtime
                .engine
                .as_ref()
                .map(|e| e.cache_dir().display().to_string())
                .unwrap_or_default(),
        });
        let pending: Vec<PendingResponse> = runtime.pending_responses.drain(..).collect();
        runtime.add_result = None;
        runtime.add_cancel = None;
        self.dirty = true;
        for pending in pending {
            self.respond(idx, pending);
        }
    }

    /// Merge runtime `new` into `dup` (same infohash): kept files + pending
    /// responses move to the surviving engine; the duplicate's engine
    /// forgets its torrent (it shared the same cache dir) and is dropped.
    fn merge_into(&mut self, dup: usize, new: usize) {
        let dup_job_id = self.runtimes[dup].job.job_id.clone();
        let new_kept = self.runtimes[new].job.kept_files.clone();
        let new_request = self.runtimes[new].job.request_id.clone();
        let new_pending: Vec<PendingResponse> = self.runtimes[new].pending_responses.drain(..).collect();
        if let (Some(engine), Some(torrent_id)) = (
            self.runtimes[new].engine.take(),
            self.runtimes[new].job.torrent_id.clone(),
        ) {
            let _ = crate::core::torrent::forget_torrent(&engine, &torrent_id);
        }
        self.merge_enqueue(dup, &new_request, new_kept, false, None);
        self.remove_runtime(new);
        // Re-resolve the survivor by id (indices may have shifted).
        let Some(dup) = self.index_of(&dup_job_id) else { return };
        for pending in new_pending {
            if self.runtimes[dup].engine.is_some() {
                self.respond(dup, pending);
            } else {
                self.runtimes[dup].pending_responses.push(pending);
            }
        }
        self.dirty = true;
    }

    /// Remove runtime `idx` (drops its engine — rqbit killed; partials
    /// and the torrent cache stay).
    fn remove_runtime(&mut self, idx: usize) {
        if idx >= self.runtimes.len() {
            return;
        }
        let job_id = self.runtimes[idx].job.job_id.clone();
        self.runtimes.remove(idx);
        remove_token_sidecar(&job_id);
        self.dirty = true;
    }

    /// Stop a job: cancel any in-flight add, forget the torrent on the
    /// engine (partials kept), drop the job + its token sidecar + any
    /// pending response files.
    fn stop_job(&mut self, idx: usize) {
        let runtime = &mut self.runtimes[idx];
        let job_id = runtime.job.job_id.clone();
        if let Some(cancel) = runtime.add_cancel.take() {
            let _ = cancel.send(());
        }
        let pending_ids: Vec<String> = runtime
            .pending_responses
            .iter()
            .map(|p| p.request_id.clone())
            .collect();
        runtime.pending_responses.clear();
        if let Some(engine) = runtime.engine.take() {
            if let Some(torrent_id) = runtime.job.torrent_id.take() {
                let _ = crate::core::torrent::forget_torrent(&engine, &torrent_id);
            }
            drop(engine);
        }
        self.remove_runtime(idx);
        remove_token_sidecar(&job_id);
        for request_id in pending_ids {
            if let Some(path) = response_path(&request_id) {
                let _ = std::fs::remove_file(path);
            }
        }
        self.dirty = true;
    }
}

impl Daemon {
    /// Periodic stats/complete/move pass over every engine-backed job.
    fn poll_jobs(&mut self) {
        let ids: Vec<String> = self
            .runtimes
            .iter()
            .filter(|r| r.engine.is_some())
            .map(|r| r.job.job_id.clone())
            .collect();
        for job_id in ids {
            let Some(idx) = self.index_of(&job_id) else { continue };
            let engine_dead = self
                .runtimes
                .get_mut(idx)
                .and_then(|r| r.engine.as_mut())
                .is_some_and(|engine| !engine.is_running());
            if engine_dead {
                let msg = "The torrent engine exited unexpectedly".to_owned();
                let ids: Vec<String> = self.runtimes[idx]
                    .pending_responses
                    .iter()
                    .map(|p| p.request_id.clone())
                    .collect();
                self.runtimes[idx].pending_responses.clear();
                for request_id in ids {
                    self.respond_error(&request_id, &msg);
                }
                self.runtimes[idx].job.status = DlStatus::Failed;
                self.runtimes[idx].job.error = Some(msg);
                self.runtimes[idx].engine = None;
                self.dirty = true;
                continue;
            }
            let (torrent_id, kept) = {
                let runtime = &self.runtimes[idx];
                (
                    runtime.job.torrent_id.clone().unwrap_or_default(),
                    runtime.job.kept_files.clone(),
                )
            };
            let stats = {
                let runtime = &self.runtimes[idx];
                let Some(engine) = runtime.engine.as_ref() else { continue };
                crate::core::torrent::torrent_stats(engine, &torrent_id)
            };
            match stats {
                Ok(stats) => {
                    let runtime = &mut self.runtimes[idx];
                    runtime.job.progress_percent = job_progress(&stats, &runtime.job.kept_files);
                    let kept_pairs: Vec<(usize, u64)> = kept
                        .iter()
                        .map(|f| (f.index, f.length))
                        .collect();
                    if crate::core::torrent::files_downloaded(&stats, &kept_pairs) {
                        runtime.job.status = DlStatus::Moving;
                        if runtime.move_started.is_none() {
                            runtime.move_started = Some(Instant::now());
                        }
                    } else if runtime.job.status == DlStatus::Moving {
                        runtime.job.status = DlStatus::Downloading;
                        runtime.move_started = None;
                    }
                    self.dirty = true;
                }
                Err(err) => {
                    log::warn!(job_id:?, error:? = err; "Failed to poll a downloader job's stats");
                }
            }
            // The (possibly deferred) move for a completed download.
            if self
                .runtimes
                .get(idx)
                .is_some_and(|r| r.job.status == DlStatus::Moving)
            {
                self.try_move(idx);
            }
        }
    }

    /// Move the completed kept files to `s2udio-downloads`, unless the
    /// TUI is still streaming them (marker + /proc probe; bounded by
    /// `MOVE_DEFER_LIMIT`), then forget the torrent and drop the job.
    fn try_move(&mut self, idx: usize) {
        let (job_id, torrent_id, torrent_name, kept, move_started) = {
            let runtime = &self.runtimes[idx];
            (
                runtime.job.job_id.clone(),
                runtime.job.torrent_id.clone().unwrap_or_default(),
                runtime.job.torrent_name.clone(),
                runtime.job.kept_files.clone(),
                runtime.move_started.unwrap_or_else(Instant::now),
            )
        };
        if move_started.elapsed() < MOVE_DEFER_LIMIT && self.move_deferred(&job_id, &torrent_id, &kept) {
            return;
        }
        let Some(dest_dir) = downloads_dir() else {
            self.runtimes[idx].job.status = DlStatus::Failed;
            self.runtimes[idx].job.error = Some("Cannot determine the downloads folder".to_owned());
            self.dirty = true;
            return;
        };
        let cache_dir = {
            let runtime = &self.runtimes[idx];
            runtime
                .engine
                .as_ref()
                .map(|e| e.cache_dir().to_path_buf())
                .unwrap_or_default()
        };
        if cache_dir.as_os_str().is_empty() {
            self.runtimes[idx].job.status = DlStatus::Failed;
            self.runtimes[idx].job.error = Some("The job's engine is gone before the move".to_owned());
            self.dirty = true;
            return;
        }
        // Round 55 (F2): the move source comes from the engine's own
        // layout — `output_folder` + per-file `components` (single-file
        // torrents are stored FLAT: `output_folder` IS the cache dir).
        let details = self.runtimes[idx].details.clone();
        for file in &kept {
            let source = crate::core::torrent::kept_file_source(
                &cache_dir,
                &torrent_name,
                details.as_ref(),
                file,
            );
            if let Err(err) = crate::core::torrent::move_completed_file(&source, &dest_dir) {
                self.runtimes[idx].job.status = DlStatus::Failed;
                self.runtimes[idx].job.error =
                    Some(format!("Failed to keep downloaded file '{}': {err}", file.name));
                self.dirty = true;
                return;
            }
            log::info!(file:?; "Moved a completed torrent download to s2udio-downloads");
        }
        // Done: forget the torrent (leftover cache stays). Round 56
        // (56-1) + 56.6: keep the job listed as `Completed` instead of
        // dropping it — `done_at` + `moved_to` carry the completion
        // facts, the TUI shows the done row and fires the one-shot
        // notice, and the row persists until the user removes it (56.6:
        // no more auto-prune). The engine is dropped here (as
        // `remove_runtime` did: the rqbit child dies, the token sidecar
        // goes).
        let forget_result = {
            let runtime = &self.runtimes[idx];
            match runtime.engine.as_ref() {
                Some(engine) => crate::core::torrent::forget_torrent(engine, &torrent_id),
                None => Ok(()),
            }
        };
        if let Err(err) = forget_result {
            log::warn!(error:? = err; "Failed to forget a completed torrent on its engine");
        }
        let now = now_unix();
        log::info!(job_id:? = job_id, dest:? = dest_dir.display().to_string();
            "Committed torrent download completed");
        let runtime = &mut self.runtimes[idx];
        runtime.job.status = DlStatus::Completed;
        runtime.job.progress_percent = 100.0;
        runtime.job.done_at = Some(now);
        runtime.job.moved_to = Some(dest_dir.display().to_string());
        runtime.job.updated_at = now;
        runtime.engine = None;
        runtime.details = None;
        runtime.add_result = None;
        runtime.add_cancel = None;
        remove_token_sidecar(&job_id);
        self.dirty = true;
    }

    /// Whether the completed job's move must wait: the TUI's streaming
    /// marker is fresh (its pid is alive and lists the job) or a live mpv
    /// process still references one of the kept stream URLs.
    fn move_deferred(&self, job_id: &str, torrent_id: &str, kept: &[DlKeptFile]) -> bool {
        let tui_streaming = read_streaming_marker().is_some_and(|marker| {
            crate::core::rqctl::pid_alive(marker.pid) && marker.jobs.iter().any(|j| j == job_id)
        });
        let mpv_live = mpv_references_stream(torrent_id, kept);
        tui_streaming || mpv_live
    }

    /// Write one response file (or an error response) for a pending
    /// request. The response is built from the job's engine + torrent.
    fn respond(&mut self, idx: usize, pending: PendingResponse) {
        let resolved = self.response_files(idx, &pending);
        match resolved {
            Ok(files) if !files.is_empty() => {
                let (job_id, torrent_id, torrent_name, engine) = {
                    let runtime = &self.runtimes[idx];
                    (
                        runtime.job.job_id.clone(),
                        runtime.job.torrent_id.clone().unwrap_or_default(),
                        runtime.job.torrent_name.clone(),
                        DlEngineInfo {
                            proxy_url: runtime
                                .engine
                                .as_ref()
                                .map(crate::core::torrent::TorrentEngine::proxy_base_url)
                                .unwrap_or_default(),
                            engine_port: runtime.engine.as_ref().map(crate::core::torrent::TorrentEngine::http_port).unwrap_or(0),
                            cache_dir: runtime
                                .engine
                                .as_ref()
                                .map(|e| e.cache_dir().display().to_string())
                                .unwrap_or_default(),
                        },
                    )
                };
                let response = DlJobResponse {
                    job_id,
                    request_id: pending.request_id.clone(),
                    torrent_id,
                    torrent_name,
                    engine,
                    files,
                    error: None,
                };
                write_response(&pending.request_id, &response);
            }
            Ok(_) => {
                self.respond_error(&pending.request_id, "No playable media in this torrent");
            }
            Err(err) => {
                self.respond_error(&pending.request_id, &err);
            }
        }
    }

    /// The response files for a pending request: the job's kept files for
    /// an enqueue-with-play, or the requested stream indices resolved
    /// against the torrent's file list.
    fn response_files(
        &self,
        idx: usize,
        pending: &PendingResponse,
    ) -> Result<Vec<DlResponseFile>, String> {
        let engine = self
            .runtimes
            .get(idx)
            .and_then(|r| r.engine.as_ref())
            .ok_or_else(|| "The job's engine is not ready".to_owned())?;
        let torrent_id = self.runtimes[idx]
            .job
            .torrent_id
            .as_deref()
            .ok_or_else(|| "The job has no torrent id yet".to_owned())?;
        match &pending.kind {
            ResponseKind::EnqueuePlay => Ok(self.runtimes[idx]
                .job
                .kept_files
                .iter()
                .map(|f| DlResponseFile {
                    index: f.index,
                    name: f.name.clone(),
                    length: f.length,
                    stream_url: engine.stream_url(torrent_id, f.index as u64),
                })
                .collect()),
            ResponseKind::Stream(indices) => {
                let details = self
                    .runtimes
                    .get(idx)
                    .and_then(|r| r.details.as_ref())
                    .ok_or_else(|| "The torrent's file list is not available".to_owned())?;
                let picked: Vec<(usize, &crate::core::torrent::TorrentFileInfo)> =
                    if indices.is_empty() {
                        crate::core::torrent::pick_playable_file(&details.files)
                            .into_iter()
                            .collect()
                    } else {
                        indices
                            .iter()
                            .filter_map(|i| details.files.get(*i).map(|f| (*i, f)))
                            .collect()
                    };
                if picked.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(picked
                    .into_iter()
                    .map(|(i, f)| DlResponseFile {
                        index: i,
                        name: f.name.clone(),
                        length: f.length,
                        stream_url: engine.stream_url(torrent_id, i as u64),
                    })
                    .collect())
            }
        }
    }

    /// Write an error response for a request (the wait window / re-stream
    /// caller shows it and stops waiting).
    fn respond_error(&self, request_id: &str, error: &str) {
        let response = DlJobResponse {
            job_id: String::new(),
            request_id: request_id.to_owned(),
            torrent_id: String::new(),
            torrent_name: String::new(),
            engine: DlEngineInfo {
                proxy_url: String::new(),
                engine_port: 0,
                cache_dir: String::new(),
            },
            files: Vec::new(),
            error: Some(error.to_owned()),
        };
        write_response(request_id, &response);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The overall kept-file progress (0-100).
fn job_progress(stats: &crate::core::torrent::TorrentStats, kept: &[DlKeptFile]) -> f64 {
    let total: u64 = kept.iter().map(|f| f.length).sum();
    if total == 0 {
        return 0.0;
    }
    let done: u64 = kept
        .iter()
        .map(|f| stats.file_progress.get(f.index).copied().unwrap_or(0).min(f.length))
        .sum();
    (done as f64 / total as f64) * 100.0
}

/// Write the per-request response file (atomic tmp + rename).
fn write_response(request_id: &str, response: &DlJobResponse) {
    let Some(path) = response_path(request_id) else { return };
    let Some(parent) = path.parent() else { return };
    let _ = std::fs::create_dir_all(parent);
    let Ok(content) = serde_json::to_string_pretty(response) else { return };
    let tmp = parent.join(format!(".{request_id}.tmp"));
    let _ = std::fs::write(&tmp, &content);
    let _ = std::fs::rename(&tmp, &path);
    log::debug!(request_id:?; "Wrote a downloader response file");
}

/// The TUI consumes a response (read + delete). Matched by request id.
pub fn take_response(request_id: &str) -> Option<DlJobResponse> {
    let path = response_path(request_id)?;
    let response = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok());
    if response.is_some() {
        let _ = std::fs::remove_file(&path);
    }
    response
}

/// The "Preparing downloader…" wait loop (round 54): polls the response
/// file of `request_id` every 500 ms until it appears (the daemon answers
/// once the torrent is added and streamable), the wait is cancelled, or
/// the daemon proves dead (no live daemon within a spawn grace window —
/// the daemon start itself is a separate work request that can take up to
/// 15 s). Returns the parsed response; the response file is consumed.
pub fn wait_for_response(
    request_id: &str,
    cancel: &crossbeam::channel::Receiver<()>,
    progress: Option<&crossbeam::channel::Sender<crate::shared::events::AppEvent>>,
) -> Result<DlJobResponse, String> {
    let started = Instant::now();
    let mut last_progress = Instant::now();
    loop {
        if cancel.try_recv().is_ok() {
            return Err("Wait cancelled".to_owned());
        }
        if let Some(response) = take_response(request_id) {
            return Ok(response);
        }
        let daemon_ok = read_state().as_ref().is_some_and(daemon_running);
        if !daemon_ok && started.elapsed() > DAEMON_READY_TIMEOUT + Duration::from_secs(10) {
            return Err("The downloader daemon did not start".to_owned());
        }
        if last_progress.elapsed() >= Duration::from_secs(1) {
            last_progress = Instant::now();
            if let Some(tx) = progress {
                let _ = tx.send(crate::shared::events::AppEvent::DlWaitProgress {
                    request_id: request_id.to_owned(),
                    elapsed_secs: started.elapsed().as_secs(),
                });
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Write the job's token sidecar (0600): the engine's `user:pass` — the
/// durable per-job token store; mpv stream URLs embed it as userinfo.
fn write_token_sidecar(job_id: &str, user_pass: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(path) = token_path(job_id) else {
        return Err("Could not determine the s2udio cache dir".to_owned());
    };
    let Some(parent) = path.parent() else {
        return Err("Bad token sidecar path".to_owned());
    };
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options
        .open(&path)
        .map_err(|err| format!("Cannot create {}: {err}", path.display()))?;
    use std::io::Write;
    file.write_all(user_pass.as_bytes())
        .map_err(|err| format!("Cannot write {}: {err}", path.display()))
}
fn remove_token_sidecar(job_id: &str) {
    if let Some(path) = token_path(job_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Read the TUI's streaming marker (missing/unparsable -> None).
pub fn read_streaming_marker() -> Option<StreamingMarker> {
    let path = streaming_marker_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}
/// Write the TUI's streaming marker (`{"pid": <tui pid>, "jobs": [...]}`).
pub fn write_streaming_marker(jobs: &[String]) {
    let Some(path) = streaming_marker_path() else { return };
    let marker = StreamingMarker {
        pid: std::process::id(),
        jobs: jobs.to_vec(),
    };
    let Ok(content) = serde_json::to_string(&marker) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, content);
}
/// Clear the TUI's streaming marker (MpvSessionEnded / app exit).
pub fn clear_streaming_marker() {
    if let Some(path) = streaming_marker_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether a live process references one of the job's stream URLs
/// (`/proc/<pid>/cmdline` contains `/torrents/<id>/stream/<idx>`).
fn mpv_references_stream(torrent_id: &str, kept: &[DlKeptFile]) -> bool {
    let tokens: Vec<String> = kept
        .iter()
        .map(|f| format!("/torrents/{torrent_id}/stream/{}", f.index))
        .collect();
    if tokens.is_empty() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else { return false };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let cmdline = String::from_utf8_lossy(&cmdline);
        if tokens.iter().any(|token| cmdline.contains(token.as_str())) {
            return true;
        }
    }
    false
}

/// `s2udio dl serve` — the daemon main entry (hidden; spawned by the TUI
/// or `dl start`).
fn serve() -> Result<(), String> {
    let daemon = Daemon::new()?;
    daemon.run()
}

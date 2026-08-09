//! Torrent streaming engine (rqbit).
//!
//! The engine is a subprocess + localhost HTTP server, mirroring the s2u-yt
//! wrapper philosophy: a contained external tool spawned by s2udio, talked
//! to over HTTP on 127.0.0.1, with the binary overridable through
//! `S2UDIO_RQBIT_BIN` (unit tests substitute a fake script).
//!
//! M1 scope: engine spawn/kill, auth, REST client, port fallback. The
//! classification, popup, bandwidth gate, playback routing and lifecycle
//! land in M2+ (see `docs/design/Backend/torrent-streaming.md`).

use std::{
    io::Read,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use base64::Engine as _;
use serde::Deserialize;

use crate::config::torrent::Torrent;

/// Env override for the rqbit binary (mirrors `S2UDIO_YTDLP_BIN`).
const RQBIT_BIN_ENV: &str = "S2UDIO_RQBIT_BIN";
const RQBIT_DEFAULT_BIN: &str = "rqbit";
/// Hard deadline for the engine's HTTP API to become reachable after spawn.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How often the scan thread reports progress to the paste popup's wait
/// window (round 18): once per second — the counter ticks every second.
const SCAN_PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// How many ports above the configured one to scan when it is in use.
const PORT_SCAN_LIMIT: u16 = 20;
/// Client timeout for the torrent add request (round 18 host finding
/// 2026-08-09): rqbit's `POST /torrents` blocks until a magnet's metainfo
/// has been resolved from peers — a cold magnet can take minutes, so the
/// add call must NOT be cut short by the 5 s HTTP client timeout (that
/// made the scan fail at the add step, before the open-ended wait ever
/// started). Keep it above rqbit's own handler cap.
const ADD_REQUEST_TIMEOUT: Duration = Duration::from_secs(3600 + 60);
/// Raise rqbit's handler-level add timeout from its 600 s default to the
/// 1 h maximum (`x-req-timeout-ms`, capped at 3_600_000) so the server
/// does not abort the add while a slow magnet's metainfo is still coming.
const RQBIT_HANDLER_TIMEOUT_MS: &str = "3600000";

/// The running rqbit server: its child process plus the base URL and auth
/// header every REST call must carry. Dropping the struct kills the child
/// (a plain `Child` would leave it running — `Drop` reaps it explicitly).
/// The engine travels from the work thread (where it is spawned) to the UI
/// (which keeps it alive), so it is `Send` (`Child` is) and `Debug`.
#[derive(Debug)]
pub struct TorrentEngine {
    child: Child,
    /// `http://127.0.0.1:<port>` — the API base, no trailing slash.
    base_url: String,
    /// The `Authorization: Basic …` header value (random per-launch token).
    auth_header: String,
    /// The raw `user:pass` pair, embedded in stream URLs so mpv (which
    /// cannot send our custom header) authenticates via URL userinfo —
    /// rqbit enforces auth on the stream endpoint too (verified live:
    /// no credentials → 401, URL userinfo → 206).
    user_pass: String,
    /// The configured cache/download folder (kept so the caller can clean it).
    pub cache_dir: PathBuf,
}

impl Drop for TorrentEngine {
    fn drop(&mut self) {
        // Kill + reap the engine child (idempotent; a dead child's kill is
        // a no-op error we ignore).
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The rqbit binary path: `S2UDIO_RQBIT_BIN` override or `rqbit` from PATH.
fn rqbit_bin() -> String {
    std::env::var(RQBIT_BIN_ENV).unwrap_or_else(|_| RQBIT_DEFAULT_BIN.to_owned())
}

/// The resolved `Authorization: Basic …` header for a `user:pass` pair.
fn basic_auth_header(user_pass: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(user_pass.as_bytes())
    )
}

/// Spawn a random `user:pass` pair for the engine's HTTP auth (defense in
/// depth on 127.0.0.1; the engine gets it via `RQBIT_HTTP_BASIC_AUTH_USERPASS`).
fn random_user_pass() -> String {
    // 4 × u64 gives 32 bytes of entropy for the token half.
    let token: String = std::iter::repeat_with(|| format!("{:016x}", rand::random::<u64>()))
        .take(2)
        .collect();
    format!("s2u:{token}")
}

/// Find a free port starting at `preferred` (scanning up to +20), or None
/// when every candidate is taken.
fn find_free_port(preferred: u16) -> Option<u16> {
    for port in preferred..=preferred.saturating_add(PORT_SCAN_LIMIT) {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

/// Whether the engine binary exists (PATH lookup, honoring the env override).
/// Returns an Err with an install hint when it does not, so the caller can
/// show a status notice and abort only the torrent action.
pub fn ensure_engine_binary() -> Result<String, String> {
    let bin = rqbit_bin();
    if which::which(&bin).is_ok() {
        Ok(bin)
    } else {
        Err(format!(
            "rqbit not found — install (cargo install rqbit / static binary) or set {RQBIT_BIN_ENV}"
        ))
    }
}

/// Spawn the rqbit server into `<cache_dir>` (created first — rqbit requires
/// the download folder to exist) bound to 127.0.0.1 on a free port, wait for
/// its HTTP API to answer, and return the running engine. When
/// `config.enabled` is false the engine is never started.
pub fn start_engine(config: &Torrent) -> Result<TorrentEngine, String> {
    if !config.enabled {
        return Err("torrent streaming is disabled in the config".to_owned());
    }
    let bin = ensure_engine_binary()?;
    let cache_dir = config.cache_dir.clone();
    std::fs::create_dir_all(&cache_dir)
        .map_err(|err| format!("Cannot create torrent cache dir {}: {err}", cache_dir.display()))?;

    let port = find_free_port(config.port)
        .ok_or_else(|| format!("No free port found near {} for rqbit", config.port))?;
    // Round-18 host findings (2026-08-09):
    // 1. rqbit `server start` binds a FIXED peer-listening port (4240) and
    //    a persisted DHT port when the flags are unset, so a second
    //    concurrent engine (a second paste/play while the first engine is
    //    still alive) dies with "Address in use" → "rqbit api not
    //    reachable". Give each engine its own free listen port and a
    //    stateless DHT (ephemeral port) so engines can coexist.
    // 2. WITHOUT `--disable-persistence` every engine RESTORES the shared
    //    rqbit session DB (all previously added torrents, e.g. a 60 GB
    //    season pack) and checksum-validates them at startup — during that
    //    the added torrent stays `Initializing` and rqbit's stream
    //    endpoint errors (`streams: invalid state`), so mpv exits 2. s2udio
    //    manages torrents itself (add → play/download → delete), so a
    //    clean per-engine session is correct and the stream is Live right
    //    after the metainfo arrives.
    let listen_port = find_free_port(port + 1)
        .ok_or_else(|| format!("No free listen port found near {} for rqbit", config.port))?;
    let user_pass = random_user_pass();
    let auth_header = basic_auth_header(&user_pass);

    let mut cmd = Command::new(&bin);
    // `--http-api-listen-addr` is a GLOBAL rqbit option (before the
    // subcommand) since v9; `server start` rejects it after `start`.
    cmd.arg("--http-api-listen-addr")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--listen-port")
        .arg(listen_port.to_string())
        .arg("--disable-dht-persistence")
        .arg("server")
        .arg("start")
        // `--disable-persistence` is a `server start` option (after the
        // subcommand), unlike the global flags above — see the
        // round-18 host finding comment on the spawn.
        .arg("--disable-persistence")
        .arg(&cache_dir)
        .env("RQBIT_HTTP_BASIC_AUTH_USERPASS", &user_pass)
        .env("RQBIT_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(
            std::fs::File::create("/tmp/s2u-rqbit-stderr.log")
                .map(Stdio::from)
                .unwrap_or(Stdio::null()),
        );
    // Keep the child in s2udio's session: the engine dies with the app.
    let child = cmd
        .spawn()
        .map_err(|err| format!("Failed to launch rqbit ({bin}): {err}"))?;
    let base_url = format!("http://127.0.0.1:{port}");

    let mut engine = TorrentEngine { child, base_url, auth_header, user_pass, cache_dir };
    match wait_until_ready(&engine) {
        Ok(()) => Ok(engine),
        Err(err) => {
            let _ = engine.kill();
            Err(format!("rqbit did not become ready: {err}"))
        }
    }
}

impl TorrentEngine {
    /// The engine's HTTP API base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The mpv stream URL for a torrent file id (`file_id` from
    /// `GET /torrents/{id}`). The engine's auth token is embedded as URL
    /// userinfo (`http://user:pass@127.0.0.1:…`) because rqbit enforces
    /// basic auth on the stream endpoint and mpv cannot send a custom
    /// `Authorization` header (verified live against rqbit 9.0.0-beta.2:
    /// no credentials → 401, userinfo → 206 with Range).
    pub fn stream_url(&self, torrent_id: &str, file_id: u64) -> String {
        format!(
            "http://{}@{}/torrents/{torrent_id}/stream/{file_id}",
            self.user_pass,
            self.base_url.trim_start_matches("http://")
        )
    }

    /// Kill the engine child (idempotent). The engine also dies when the
    /// `TorrentEngine` is dropped.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()?;
        let _ = self.child.wait();
        Ok(())
    }

    /// Whether the engine child is still running.
    pub fn is_running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }
}

/// Poll `GET /stats` until the API answers (≤ 5 s).
fn wait_until_ready(engine: &TorrentEngine) -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let reachable = api_get(engine, "/stats").is_ok();
        if reachable {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("API not reachable".to_owned());
        }
        std::thread::sleep(READY_POLL_INTERVAL);
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .user_agent(concat!("s2udio/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Agent for the torrent **add** request (round 18 host finding
/// 2026-08-09): `POST /torrents` blocks inside rqbit while a magnet's
/// metainfo is resolved from peers (its handler waits up to
/// `RQBIT_HANDLER_TIMEOUT_MS`), so the client timeout must be much longer
/// than the 5 s quick-call `agent()` — a short timeout made cold-magnet
/// scans fail at the add step before the open-ended wait could start.
fn add_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout(ADD_REQUEST_TIMEOUT)
        .user_agent(concat!("s2udio/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn api_get(engine: &TorrentEngine, path: &str) -> Result<String, String> {
    let url = format!("{}{}", engine.base_url, path);
    let response = agent()
        .get(&url)
        .set("Authorization", &engine.auth_header)
        .call()
        .map_err(|err| format!("GET {url}: {err}"))?;
    let mut body = String::new();
    response
        .into_reader()
        .read_to_string(&mut body)
        .map_err(|err| format!("Read {url}: {err}"))?;
    Ok(body)
}

fn api_post(engine: &TorrentEngine, path: &str, body: &[u8]) -> Result<String, String> {
    let url = format!("{}{}", engine.base_url, path);
    let response = agent()
        .post(&url)
        .set("Authorization", &engine.auth_header)
        .set("Content-Type", "application/octet-stream")
        .send(body)
        .map_err(|err| format!("POST {url}: {err}"))?;
    let mut body = String::new();
    response
        .into_reader()
        .read_to_string(&mut body)
        .map_err(|err| format!("Read {url}: {err}"))?;
    Ok(body)
}

/// Torrent info as returned by `GET /torrents/{id}` (the fields the app
/// uses; unknown fields are ignored).
#[derive(Deserialize)]
pub struct TorrentDetails {
    pub name: Option<String>,
    #[serde(default)]
    pub files: Vec<TorrentFileInfo>,
}

/// One entry of `TorrentDetails.files` (rqbit v9 shape: `name`,
/// `length`, `included`; there is no per-file `id` — the stream endpoint
/// addresses files by their positional index in the `files` array).
#[derive(Deserialize)]
pub struct TorrentFileInfo {
    pub name: String,
    /// Byte length of the file (rqbit's `length` field).
    pub length: u64,
    /// Consumed by the M4 file picker; unread until then.
    #[serde(default)]
    #[allow(dead_code)]
    pub included: bool,
}

/// `GET /torrents/{id}/stats/v1` — the bandwidth-gate (M3) and the
/// "Play and Download" completion check inputs.
#[derive(Deserialize, Default)]
pub struct TorrentStats {
    /// Consumed by the M3 bandwidth gate; unread until then.
    #[allow(dead_code)]
    pub state: Option<String>,
    /// Consumed by the M3 bandwidth gate; unread until then.
    #[serde(default)]
    #[allow(dead_code)]
    pub progress_bytes: u64,
    /// Consumed by the M3 bandwidth gate; unread until then.
    #[serde(default)]
    #[allow(dead_code)]
    pub total_bytes: u64,
    /// Torrent-level completion: every included file is fully downloaded
    /// (rqbit downloads all included files). The "Play and Download" job
    /// uses it (plus `file_progress` for the picked file).
    #[serde(default)]
    pub finished: bool,
    /// Per-file downloaded bytes, aligned with the torrent's `files[]`
    /// (the "Play and Download" completion check reads the picked file's
    /// index; files are preallocated on disk, so sizes are not reliable).
    #[serde(default)]
    pub file_progress: Vec<u64>,
    pub live: Option<LiveStats>,
}

/// Smoothed live statistics inside [`TorrentStats`].
#[derive(Deserialize, Default)]
pub struct LiveStats {
    /// Smoothed download speed (rqbit v9 returns `{mbps,
    /// human_readable}` — the M3 gate converts `mbps` → KB/s).
    #[allow(dead_code)]
    pub download_speed: Option<Speed>,
}

/// A speed value as rqbit reports it: `mbps` (megabits/s) plus a
/// human-readable string (e.g. "0.00 MiB/s").
#[derive(Deserialize, Default)]
pub struct Speed {
    /// Megabits per second; the M3 bandwidth gate converts to KB/s.
    #[allow(dead_code)]
    pub mbps: Option<f64>,
    /// e.g. "0.00 MiB/s" — shown in the M3 gate modal.
    #[allow(dead_code)]
    pub human_readable: Option<String>,
}

/// Add a torrent to the engine: `magnet` = raw magnet URI, `http` = a
/// `.torrent` URL, `local` = a `.torrent` file path (binary body). Returns
/// the engine's torrent id.
pub fn add_torrent(
    engine: &TorrentEngine,
    source: TorrentSource<'_>,
) -> Result<String, String> {
    add_torrent_at(&engine.base_url, &engine.auth_header, source)
}

/// The add HTTP call, addressable without a full `TorrentEngine` (the
/// scan thread runs it on a sub-thread; the engine itself cannot be
/// cloned — its `Child` is not `Clone`). Round 18 host finding: rqbit's
/// `POST /torrents` blocks until a magnet's metainfo arrives, so this uses
/// the long-timeout add agent (not the 5 s quick-call one).
fn add_torrent_at(
    base_url: &str,
    auth_header: &str,
    source: TorrentSource<'_>,
) -> Result<String, String> {
    let body: Vec<u8> = match source {
        TorrentSource::Magnet(magnet) => magnet.as_bytes().to_vec(),
        TorrentSource::Http(url) => url.as_bytes().to_vec(),
        TorrentSource::Local(file) => std::fs::read(&file)
            .map_err(|err| format!("Cannot read {}: {err}", file.display()))?,
    };
    // Round-18 host finding (2026-08-09): the engine runs with
    // `--disable-persistence`, so a re-added torrent whose files already
    // exist in the cache dir (a previous session of the same torrent) is
    // refused by rqbit's default `allow_overwrite=false` ("File exists",
    // HTTP 400). `overwrite=true` adopts the existing files instead.
    let url = format!("{base_url}/torrents?overwrite=true");
    let response = add_agent()
        .post(&url)
        .set("Authorization", auth_header)
        .set("Content-Type", "application/octet-stream")
        .set("x-req-timeout-ms", RQBIT_HANDLER_TIMEOUT_MS)
        .send(&body[..])
        .map_err(|err| format!("POST {url}: {err}"))?;
    let mut response_body = String::new();
    response
        .into_reader()
        .read_to_string(&mut response_body)
        .map_err(|err| format!("Read {url}: {err}"))?;
    let response = response_body;
    let parsed: serde_json::Value = serde_json::from_str(&response)
        .map_err(|err| format!("Cannot parse add-torrent response: {err}"))?;
    // rqbit ≥ 8 returns {"id": <id>, "details": {...}} (id numeric); older
    // builds returned a bare JSON string id — accept both.
    if let Some(s) = parsed.as_str() {
        return Ok(s.to_owned());
    }
    parsed
        .get("id")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        })
        .ok_or_else(|| "Add-torrent response had no id".to_owned())
}

/// What to add via [`add_torrent`].
pub enum TorrentSource<'a> {
    Magnet(&'a str),
    Http(&'a str),
    Local(&'a Path),
}

/// A pasted torrent source, owned so it can travel on the work queue
/// (`WorkRequest::PlayTorrent`): a magnet URI or a `.torrent` file (a
/// local path or an `http(s)` URL).
#[derive(Debug, Clone)]
pub enum TorrentItem {
    /// A full `magnet:?…` URI (added as a raw string; rqbit fetches the
    /// metainfo from peers).
    Magnet(String),
    /// A `.torrent` file: a local path or an `http(s)` URL to a `.torrent`
    /// file (rqbit downloads the latter).
    Torrent(String),
}

impl TorrentItem {
    /// The display label for status lines: the torrent file name, or the
    /// magnet's infohash prefix (raw magnet URIs are noisy).
    pub fn label(&self) -> String {
        match self {
            Self::Magnet(magnet) => crate::ui::modals::paste::magnet_infohash(magnet)
                .map_or_else(|| "magnet link".to_owned(), |hash| format!("magnet:{hash}")),
            Self::Torrent(torrent) => std::path::Path::new(torrent)
                .file_name()
                .map_or_else(|| torrent.clone(), |n| n.to_string_lossy().into_owned()),
        }
    }

    /// The stable identity key for scan bookkeeping: the magnet's full
    /// infohash (round 20 — the same torrent pasted twice, even via a
    /// different magnet URI, reuses one scan/engine instead of spawning a
    /// second rqbit against the same cache dir) or the `.torrent`
    /// path/URL. `Ctx.torrent_scans` is indexed by it, so an identical
    /// paste reuses the scan.
    pub fn source_key(&self) -> String {
        match self {
            Self::Magnet(magnet) => {
                crate::ui::modals::paste::magnet_infohash_full(magnet)
                    .unwrap_or_else(|| magnet.clone())
            }
            Self::Torrent(torrent) => torrent.clone(),
        }
    }

    /// The REST source for [`add_torrent`]: magnets are added as a raw
    /// string, `http(s)` `.torrent` URLs as a URL, local paths as a binary
    /// file body.
    pub fn source(&self) -> TorrentSource<'_> {
        match self {
            Self::Magnet(magnet) => TorrentSource::Magnet(magnet),
            Self::Torrent(torrent) => {
                if torrent.starts_with("http://") || torrent.starts_with("https://") {
                    TorrentSource::Http(torrent)
                } else {
                    TorrentSource::Local(Path::new(torrent))
                }
            }
        }
    }
}

/// Fetch the torrent's file list.
pub fn torrent_details(engine: &TorrentEngine, id: &str) -> Result<TorrentDetails, String> {
    let response = api_get(engine, &format!("/torrents/{id}"))?;
    serde_json::from_str(&response)
        .map_err(|err| format!("Cannot parse torrent details: {err}"))
}

/// Fetch the torrent's live stats (the bandwidth-gate input).
pub fn torrent_stats(engine: &TorrentEngine, id: &str) -> Result<TorrentStats, String> {
    let response = api_get(engine, &format!("/torrents/{id}/stats/v1"))?;
    serde_json::from_str(&response)
        .map_err(|err| format!("Cannot parse torrent stats: {err}"))
}

/// Remove a torrent from the engine (stops seeding/downloading it).
/// rqbit v9 removed the `DELETE /torrents/{id}` verb (405) — the delete
/// endpoint is `POST /torrents/{id}/delete` (files removed too).
pub fn delete_torrent(engine: &TorrentEngine, id: &str) -> Result<(), String> {
    api_post(engine, &format!("/torrents/{id}/delete"), &[]).map(|_| ())
}

/// The largest playable file of a torrent: the largest file with a video
/// extension, else the largest with an audio extension (the extension
/// lists from the paste pipeline). Returns the file's positional index
/// (the rqbit stream endpoint addresses files by index) and the entry.
/// `None` when the torrent has no playable media (e.g. a data torrent).
pub fn pick_playable_file(files: &[TorrentFileInfo]) -> Option<(usize, &TorrentFileInfo)> {
    for video in [true, false] {
        let mut best: Option<(usize, &TorrentFileInfo)> = None;
        for (idx, file) in files.iter().enumerate() {
            let playable = if video {
                crate::ui::modals::paste::is_video_extension(&file.name)
            } else {
                crate::ui::modals::paste::is_audio_extension(&file.name)
            };
            if !playable {
                continue;
            }
            if best.as_ref().is_none_or(|(_, b)| file.length > b.length) {
                best = Some((idx, file));
            }
        }
        if let Some(best) = best {
            return Some(best);
        }
    }
    None
}

/// The torrent's file list, waiting **open-ended** for the metadata to
/// arrive (round 18). A magnet's file list only appears once the metainfo
/// has been fetched from peers (local `.torrent` files are instant).
///
/// There is **no deadline**: some torrents take a long time to parse the
/// data — the user decides how long to wait. When a cancel signal is
/// provided (the paste popup's Esc/close for scans), the wait aborts with
/// `"Scan cancelled"` so the caller drops the engine (killing rqbit). The
/// wait runs on the scan's own thread, so the work thread is never blocked
/// by a slow magnet. The M3 bandwidth gate replaces this stopgap with the
/// full no-peers handling.
pub fn wait_for_files(
    engine: &TorrentEngine,
    id: &str,
    cancel: Option<&crossbeam::channel::Receiver<()>>,
) -> Result<TorrentDetails, String> {
    loop {
        if cancel.is_some_and(|cancel| cancel.try_recv().is_ok()) {
            return Err("Scan cancelled".to_owned());
        }
        let details = torrent_details(engine, id)?;
        if !details.files.is_empty() {
            return Ok(details);
        }
        std::thread::sleep(READY_POLL_INTERVAL);
    }
}

/// One file of a scanned torrent, with its positional index (rqbit's
/// stream endpoint addresses files by index, not id — the index is what
/// `GET /torrents/{id}` returns and what the play actions must preserve).
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub index: usize,
    pub name: String,
    pub length: u64,
}

impl ScannedFile {
    /// Whether the file carries a video extension (the paste pipeline's
    /// list — season packs are multi-video torrents).
    pub fn is_video(&self) -> bool {
        crate::ui::modals::paste::is_video_extension(&self.name)
    }

    /// Whether the file carries an audio extension.
    pub fn is_audio(&self) -> bool {
        crate::ui::modals::paste::is_audio_extension(&self.name)
    }
}

/// A scanned torrent (round 17): the running engine, the added torrent's
/// id and name, and its full file list with positional indices. The scan
/// runs once per pasted torrent on the work thread
/// (`WorkRequest::ScanTorrent`); the result lives in `Ctx.torrent_scans`
/// so the popup's play actions reuse the engine + torrent instead of
/// spawning a fresh rqbit per play.
#[derive(Debug, Clone)]
pub struct TorrentScan {
    /// The running rqbit engine (shared: the scan map keeps it alive for
    /// reuse while playback holds a clone in `Ctx.torrent_engine` — round
    /// 20 duplicate-paste fix).
    pub engine: Arc<TorrentEngine>,
    pub torrent_id: String,
    pub torrent_name: String,
    pub files: Vec<ScannedFile>,
}

impl TorrentScan {
    /// The torrent's video files, in scan order (each carries its
    /// positional index, so the stream URL is `stream_url(id, index)`).
    pub fn videos(&self) -> Vec<&ScannedFile> {
        self.files.iter().filter(|f| f.is_video()).collect()
    }

    /// The torrent's audio files, in scan order (the fallback when the
    /// torrent has no videos — an album, an audio book).
    pub fn audios(&self) -> Vec<&ScannedFile> {
        self.files.iter().filter(|f| f.is_audio()).collect()
    }

    /// The single best playable file ("Play (stream)" semantics): the
    /// largest video file, else the largest audio file. `None` when the
    /// torrent has no playable media (a data torrent).
    pub fn pick_playable(&self) -> Option<&ScannedFile> {
        let videos = self.videos();
        if !videos.is_empty() {
            return videos.into_iter().max_by_key(|f| f.length);
        }
        self.audios().into_iter().max_by_key(|f| f.length)
    }
}

/// The live progress of an in-flight torrent scan (round 18): whole
/// seconds since the scan started and the engine's current download speed
/// in KB/s (0 until peers deliver data — or stats are not available yet).
/// The paste popup's wait window renders the elapsed counter and the
/// DL-speed / needed-speed check (✓/✗) from it.
#[derive(Debug, Clone, Copy, Default)]
pub struct TorrentScanProgress {
    pub elapsed_secs: u64,
    pub download_speed_kbps: f64,
}

/// Scan a pasted torrent (round 17 + round 18): start the engine, add the
/// torrent, wait (open-ended) for its metainfo and return the running
/// engine + the torrent's name and file list. The engine is moved to the
/// UI (`Ctx.torrent_scans`) so the play actions reuse it.
///
/// Round 18: the metainfo wait is **user-controlled** — no deadline, it
/// waits until the metainfo arrives or the user cancels. The scan runs on
/// a dedicated thread (spawned by the work thread), so a slow magnet can
/// never block the work thread; `cancel` aborts the wait when the user
/// dismisses the paste popup (Esc/close — the engine is then dropped,
/// killing rqbit), and `progress` receives one `WorkDone::TorrentScanProgress`
/// per second while waiting so the popup can render the live counter and
/// the DL-speed / needed-speed check.
pub fn scan_torrent(
    item: &TorrentItem,
    config: &Torrent,
    cancel: &crossbeam::channel::Receiver<()>,
    progress: &crossbeam::channel::Sender<crate::shared::events::AppEvent>,
) -> Result<TorrentScan, String> {
    // Already cancelled (the popup closed before the work thread picked
    // the request up): do not spawn rqbit at all.
    if cancel.try_recv().is_ok() {
        return Err("Scan cancelled".to_owned());
    }
    let engine = start_engine(config)?;
    let started = Instant::now();
    let mut last_progress = started;

    // Round 18 host finding (2026-08-09): rqbit's `POST /torrents` does
    // not return until a magnet's metainfo has been resolved from peers —
    // for a cold magnet that is the long wait itself. Run the add on its
    // own sub-thread (long-timeout HTTP) so this scan thread keeps ticking
    // the wait window's counter and can abort the add when the user
    // dismisses the popup (dropping `engine` kills rqbit, which fails the
    // in-flight add request). The id arrives through the channel once rqbit
    // answers.
    let (add_tx, add_rx) = crossbeam::channel::bounded(1);
    let add_base_url = engine.base_url.clone();
    let add_auth = engine.auth_header.clone();
    let add_item = item.clone();
    let key_for_add = item.source_key();
    let add_thread = std::thread::Builder::new()
        .name(format!(
            "torrent-add-{}",
            key_for_add.chars().take(24).collect::<String>()
        ))
        .spawn(move || {
            let result = add_torrent_at(&add_base_url, &add_auth, add_item.source());
            let _ = add_tx.send(result);
        })
        .map_err(|err| format!("Failed to spawn torrent add thread: {err}"))?;

    let torrent_id = loop {
        if cancel.try_recv().is_ok() {
            // The user dismissed the popup while the add was in flight:
            // drop the engine (kills rqbit → the add request fails) and
            // report the cancellation.
            return Err("Scan cancelled".to_owned());
        }
        match add_rx.try_recv() {
            Ok(Ok(id)) => break id,
            Ok(Err(err)) => return Err(err),
            Err(crossbeam::channel::TryRecvError::Empty) => {}
            Err(crossbeam::channel::TryRecvError::Disconnected) => {
                return Err("Torrent add thread died".to_owned());
            }
        }
        if last_progress.elapsed() >= SCAN_PROGRESS_INTERVAL {
            last_progress = Instant::now();
            // The torrent does not exist yet (its metainfo is still
            // resolving), so there is no `stats/v1` to read: the speed
            // line shows 0 until the add answers.
            let _ = progress.send(crate::shared::events::AppEvent::WorkDone(Ok(
                crate::shared::events::WorkDone::TorrentScanProgress {
                    key: item.source_key(),
                    progress: TorrentScanProgress {
                        elapsed_secs: started.elapsed().as_secs(),
                        download_speed_kbps: 0.0,
                    },
                },
            )));
        }
        std::thread::sleep(READY_POLL_INTERVAL);
    };
    let _ = add_thread; // joined implicitly by the channel round-trip

    loop {
        if cancel.try_recv().is_ok() {
            // The user dismissed the popup: drop the engine (its `Drop`
            // kills the rqbit child) and report the cancellation. The
            // popup is already closed, so the UI drops this result.
            return Err("Scan cancelled".to_owned());
        }
        match torrent_details(&engine, &torrent_id) {
            Ok(details) if !details.files.is_empty() => {
                // Round-18 host finding (2026-08-09): a re-added torrent
                // whose files already exist in the cache is adopted with
                // `overwrite=true` and spends some seconds in rqbit's
                // `Initializing` state (checksum-validating the existing
                // files) — during that window the stream endpoint errors
                // (500 "streams: invalid state"), so mpv would exit. Wait
                // until the torrent is `live` before offering play actions
                // (the wait window keeps ticking; the scan is still
                // cancellable).
                let live = torrent_stats(&engine, &torrent_id)
                    .ok()
                    .and_then(|stats| stats.state)
                    .is_some_and(|state| state == "live");
                if live {
                    let files: Vec<ScannedFile> = details
                        .files
                        .iter()
                        .enumerate()
                        .map(|(index, f)| ScannedFile { index, name: f.name.clone(), length: f.length })
                        .collect();
                    let torrent_name = details.name.clone().unwrap_or_else(|| item.label());
                    return Ok(TorrentScan {
                        engine: Arc::new(engine),
                        torrent_id,
                        torrent_name,
                        files,
                    });
                }
            }
            Ok(_) => {}
            Err(err) => return Err(err),
        }
        if last_progress.elapsed() >= SCAN_PROGRESS_INTERVAL {
            last_progress = Instant::now();
            let download_speed_kbps = torrent_stats(&engine, &torrent_id)
                .ok()
                .and_then(|stats| stats.live)
                .and_then(|live| live.download_speed)
                .and_then(|speed| speed.mbps)
                .map(|mbps| mbps * 125.0) // megabits/s → KB/s
                .unwrap_or(0.0);
            let _ = progress.send(crate::shared::events::AppEvent::WorkDone(Ok(
                crate::shared::events::WorkDone::TorrentScanProgress {
                    key: item.source_key(),
                    progress: TorrentScanProgress {
                        elapsed_secs: started.elapsed().as_secs(),
                        download_speed_kbps,
                    },
                },
            )));
        }
        std::thread::sleep(READY_POLL_INTERVAL);
    }
}

/// A scanned torrent backed by a real (fake-script) engine, for tests in
/// other modules (the paste popup renders the `[Torrent]` section from a
/// scan). The engine's `Drop` kills the fake rqbit at test end.
#[cfg(test)]
pub(crate) fn test_scan(files: Vec<ScannedFile>) -> TorrentScan {
    let _guard = tests::RQBIT_ENV_LOCK.lock().unwrap();
    let (bin, _log) = tests::fake_engine_bin("paste-scan");
    unsafe {
        std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
    }
    let engine = start_engine(&tests::test_config(31060)).expect("fake engine starts");
    unsafe {
        std::env::remove_var("S2UDIO_RQBIT_BIN");
    }
    TorrentScan {
        engine: Arc::new(engine),
        torrent_id: "1".to_owned(),
        torrent_name: "Fake Pack".to_owned(),
        files,
    }
}

/// A "Play and Download" job: the stream is playing and the engine keeps
/// downloading the torrent; once the picked file is complete it is moved
/// to `s2udio-downloads` (deferred until mpv stops using the stream when
/// a file completes mid-playback). Lives in `Ctx.torrent_download`,
/// polled once per second by the event loop. One job per torrent: the
/// popup's "Download" / "Download all" (round 21), the picker's
/// "Download & Play" and the classic single-file "Play and Download" all
/// track their kept files through the same job.
#[derive(Debug)]
pub struct TorrentDownload {
    /// The engine this download runs on, identified by its API base URL
    /// (a replaced engine — another torrent played — abandons the job).
    pub engine_base_url: String,
    /// The torrent's id on the engine (stats + delete).
    pub torrent_id: String,
    /// The torrent's display name (the output folder under the cache dir).
    pub torrent_name: String,
    /// The files to keep in `s2udio-downloads` once the download is done.
    pub files: Vec<TorrentDownloadFile>,
    /// The download finished; the move may still be deferred.
    pub complete: bool,
    /// Complete while mpv was still playing a stream: move on session end.
    pub deferred: bool,
    /// Consecutive stats failures (engine gone): abandon after 3.
    pub failures: u8,
}

/// One file of a torrent download job: the engine keeps the whole torrent
/// downloading and, once it is complete, every kept file is moved to
/// `s2udio-downloads`.
#[derive(Debug, Clone)]
pub struct TorrentDownloadFile {
    /// The file's positional index (completion via `file_progress`).
    pub file_idx: usize,
    /// The file's length in bytes (completion check).
    pub file_length: u64,
    /// The file's name (display + the moved file's name).
    pub file_name: String,
    /// The completed file's location inside the engine's cache dir.
    pub source_path: std::path::PathBuf,
    /// The stream URL mpv may be playing (the file must not be moved away
    /// while the stream is still in use).
    pub stream_url: String,
}

/// Whether the torrent's download is complete (every kept file fully
/// downloaded): `stats.finished` is torrent-level, and `file_progress` is
/// aligned with the torrent's file list, so each kept file's index must
/// show its full length. (rqbit preallocates files on disk, so sizes are
/// not a progress signal.)
pub fn download_complete(stats: &TorrentStats, job: &TorrentDownload) -> bool {
    stats.finished
        && job.files.iter().all(|file| {
            stats
                .file_progress
                .get(file.file_idx)
                .copied()
                .unwrap_or(0)
                >= file.file_length
        })
}

/// Whether a URL is an rqbit torrent stream URL (used to prefer the
/// playlist entry's saved file name over mpv's raw URL media-title).
pub fn is_torrent_stream_url(url: &str) -> bool {
    url.starts_with("http://") && url.contains("/torrents/")
}

/// Move a completed torrent file into `dest_dir`, picking a unique name
/// (`name`, `name (1)`, …) so an existing file is never overwritten.
/// Tries a plain rename first and falls back to copy + remove across
/// filesystems. Returns the final path.
pub fn move_completed_file(
    source: &std::path::Path,
    dest_dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    if !source.is_file() {
        return Err(format!("Downloaded file not found: {}", source.display()));
    }
    std::fs::create_dir_all(dest_dir)
        .map_err(|err| format!("Cannot create {}: {err}", dest_dir.display()))?;
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_owned());
    let mut target = dest_dir.join(&name);
    let mut n = 1usize;
    while target.exists() {
        let stem = std::path::Path::new(&name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".to_owned());
        let ext = std::path::Path::new(&name)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        target = dest_dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    match std::fs::rename(source, &target) {
        Ok(()) => Ok(target),
        Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
            // Cross-device (cache on a different mount than the library):
            // copy + remove instead of failing the keep.
            std::fs::copy(source, &target)
                .map_err(|err| format!("Cannot copy {}: {err}", source.display()))?;
            let _ = std::fs::remove_file(source);
            Ok(target)
        }
        Err(err) => Err(format!("Cannot move {}: {err}", source.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use std::net::TcpListener;

    use super::{Torrent, TorrentSource, add_torrent, start_engine, torrent_details, torrent_stats};

    /// The tests mutate `S2UDIO_RQBIT_BIN` and spawn fake servers; serialize
    /// them so a parallel run can't cross wires.
    pub(crate) static RQBIT_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// A fake rqbit: serves `GET /stats`, `POST /torrents`, `GET
    /// /torrents/{id}` and `GET /torrents/{id}/stats/v1`, echoing the
    /// received `Authorization` header to a request log next to the script
    /// so tests can assert the auth flow.
    const FAKE_SCRIPT: &str = r#"#!/bin/sh
port=""
prev=""
for arg in "$@"; do
    case "$arg" in
        --http-api-listen-addr=*) port="${arg#*=}" ;;
        --http-api-listen-addr) prev="$arg" ;;
        127.0.0.1:*) [ "$prev" = "--http-api-listen-addr" ] && port="$arg" ;;
    esac
    [ "$arg" != "--http-api-listen-addr" ] && prev=""
done
# port arrives as 127.0.0.1:PORT
port="${port##*:}"
log="$(dirname "$0")/requests.log"
printf '%s\n' "SPAWN $*" >> "$log"
exec python3 - "$port" "$log" <<'PY'
import http.server, os, sys, time

port = int(sys.argv[1])
log = sys.argv[2]

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def _record(self):
        with open(log, "a") as f:
            f.write(self.command + " " + self.path + " " + str(self.headers.get("Authorization", "")) + "\n")
    def do_GET(self):
        self._record()
        if self.path == "/stats":
            body = b"{}"
        elif self.path.startswith("/torrents/") and self.path.endswith("/stats/v1"):
            # The "not-live" marker simulates a re-added torrent whose
            # cache files are being checksum-validated (rqbit
            # `Initializing`): the scan must keep waiting, not offer play.
            if os.path.exists(os.path.join(os.path.dirname(log), "not-live")):
                body = b'{"state":"initializing","progress_bytes":0,"total_bytes":100,"finished":false,"live":{"download_speed":{"mbps":0.0,"human_readable":"0.00 MiB/s"}}}'
            else:
                body = b'{"state":"live","progress_bytes":0,"total_bytes":100,"finished":false,"live":{"download_speed":{"mbps":0.0,"human_readable":"0.00 MiB/s"}}}'
        elif self.path.startswith("/torrents/"):
            if os.path.exists(os.path.join(os.path.dirname(log), "no-files")):
                body = b'{"name":"fake.torrent","files":[]}'
            else:
                body = b'{"name":"fake.torrent","files":[{"name":"movie.mkv","length":100,"included":true}]}'
        else:
            body = b"{}"
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_POST(self):
        self._record()
        length = int(self.headers.get("Content-Length", 0))
        data = self.rfile.read(length)
        if self.path.endswith("/delete"):
            body = b"{}"
        else:
            # Round-18 regression (host finding 2026-08-09): rqbit's real
            # add POST blocks until a magnet's metainfo resolves — the
            # "slow-add" marker makes the fake hold the response for N
            # seconds so the scan must tick its wait window and stay
            # cancellable instead of failing on a short HTTP timeout.
            slow = os.path.join(os.path.dirname(log), "slow-add")
            if os.path.exists(slow):
                try:
                    time.sleep(float(open(slow).read().strip()))
                except ValueError:
                    pass
            # rqbit v8+ add response: object with a numeric id.
            body = b'{"id":1,"details":{},"output_folder":"","seen_peers":null}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY
"#;

    pub(crate) fn fake_engine_bin(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("rqbit-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let bin = dir.join("rqbit");
        let log = dir.join("requests.log");
        std::fs::write(&bin, FAKE_SCRIPT).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&bin).status().unwrap();
        (bin, log)
    }

    pub(crate) fn test_config(port: u16) -> Torrent {
        Torrent {
            enabled: true,
            port,
            min_download_speed_kbps: 500,
            warmup_secs: 5,
            max_wait_secs: 15,
            no_peers_timeout_secs: 30,
            cache_dir: std::env::temp_dir().join(format!("rqbit-cache-{}", std::process::id())),
            auto_pick_file: true,
            keep_after_play: false,
        }
    }

    #[test]
    fn missing_binary_reports_install_hint() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let missing = std::env::temp_dir().join("rqbit-definitely-missing");
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &missing);
        }
        let err = super::ensure_engine_binary().unwrap_err();
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        assert!(err.contains("rqbit"), "{err}");
        assert!(err.contains("install"), "{err}");
    }

    #[test]
    fn start_engine_spawns_fake_and_waits_for_stats() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, log) = fake_engine_bin("start");
        let port = 31030;
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let config = test_config(port);
        let engine = start_engine(&config).expect("engine must start");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        assert_eq!(engine.base_url(), format!("http://127.0.0.1:{port}"));
        assert_eq!(engine.cache_dir, config.cache_dir);
        let mut engine = engine;
        assert!(engine.is_running());
        // Round-18 host finding (2026-08-09): the engine must run with a
        // clean per-engine session (`--disable-persistence`). Without it,
        // rqbit restores the shared session DB and checksum-validates
        // previously added torrents at startup — the added torrent stays
        // `Initializing` and its stream endpoint errors, so mpv exits 2.
        let spawn = std::fs::read_to_string(&log).expect("spawn log");
        let spawn_line = spawn.lines().find(|l| l.starts_with("SPAWN ")).expect("spawn recorded");
        assert!(
            spawn_line.contains("--disable-persistence"),
            "engine must run with a clean session: {spawn_line}"
        );
        assert!(
            spawn_line.contains("--listen-port"),
            "engine must have its own peer listen port: {spawn_line}"
        );
        engine.kill().expect("kill must succeed");
    }

    #[test]
    fn port_falls_back_when_configured_port_is_taken() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, _log) = fake_engine_bin("port");
        let occupied = 31130;
        let _listener = TcpListener::bind(("127.0.0.1", occupied)).expect("bind must succeed");
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let config = test_config(occupied);
        let mut engine = start_engine(&config).expect("engine must fall back to a free port");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        assert_ne!(engine.base_url(), format!("http://127.0.0.1:{occupied}"));
        engine.kill().expect("kill must succeed");
    }

    #[test]
    fn auth_header_is_sent_on_every_request() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, log) = fake_engine_bin("auth");
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let config = test_config(31230);
        let mut engine = start_engine(&config).expect("engine must start");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        let id = add_torrent(&engine, TorrentSource::Magnet("magnet:?xt=urn:btih:aaaa")).expect("add must work");
        assert_eq!(id, "1"); // rqbit v8+ returns a numeric id in an object
        let details = torrent_details(&engine, &id).expect("details must parse");
        assert_eq!(details.name.as_deref(), Some("fake.torrent"));
        assert_eq!(details.files.len(), 1);
        assert_eq!(details.files[0].length, 100); // real v9 field name
        let stats = torrent_stats(&engine, &id).expect("stats must parse");
        assert!(stats.live.is_some());
        super::delete_torrent(&engine, &id).expect("delete must work");
        engine.kill().expect("kill must succeed");

        let lines = std::fs::read_to_string(&log).expect("request log must exist");
        assert!(lines.contains("GET /stats"), "{lines}");
        assert!(lines.contains("GET /torrents/1"), "{lines}");
        assert!(lines.contains("GET /torrents/1/stats/v1"), "{lines}");
        assert!(lines.contains("POST /torrents/1/delete"), "{lines}");
        // The add carries `overwrite=true` so a re-added torrent adopts
        // its already-existing cache files instead of a 400 "File exists".
        assert!(lines.contains("POST /torrents?overwrite=true"), "{lines}");
        for line in lines.lines() {
            // The `SPAWN …` line is the fake engine's argv echo (no HTTP
            // request) — every actual request must carry Basic auth.
            if line.starts_with("SPAWN ") {
                continue;
            }
            assert!(line.contains("Basic "), "expected Basic auth on {line:?}");
        }
    }

    #[test]
    fn engine_drop_kills_the_child() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, _log) = fake_engine_bin("drop");
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let port = 31330;
        let config = test_config(port);
        let engine = start_engine(&config).expect("engine must start");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        let mut engine = engine;
        assert!(engine.is_running());
        drop(engine);
        // After the drop the child is gone: the port must be free again.
        let listener = TcpListener::bind(("127.0.0.1", port));
        assert!(listener.is_ok(), "port {port} must be released after the engine is dropped");
    }

    #[test]
    fn engine_restart_reuses_a_freed_port() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, _log) = fake_engine_bin("reuse");
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let port = 31430;
        let mut engine = start_engine(&test_config(port)).expect("engine must start");
        engine.kill().expect("kill must succeed");
        let mut engine2 = start_engine(&test_config(port)).expect("engine must restart on the same port");
        assert_eq!(engine2.base_url(), format!("http://127.0.0.1:{port}"));
        engine2.kill().expect("kill must succeed");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
    }

    #[test]
    fn two_engines_coexist_with_distinct_listen_ports() {
        // Round-18 host finding (2026-08-09): rqbit `server start` binds a
        // FIXED peer port (4240) and a persisted DHT port when unset, so a
        // second engine (a second paste/play while the first engine is
        // alive) died with "Address in use" → "rqbit api not reachable".
        // `start_engine` now passes a per-engine `--listen-port` and
        // `--disable-dht-persistence`, so two engines must run together.
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, log) = fake_engine_bin("two-engines");
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let mut engine_a = start_engine(&test_config(31530)).expect("first engine starts");
        let mut engine_b = start_engine(&test_config(31540)).expect("second engine starts while the first is alive");
        assert_ne!(engine_a.base_url(), engine_b.base_url(), "distinct API ports");
        assert!(engine_a.is_running() && engine_b.is_running(), "both engines alive");
        // The fake records each engine's spawn: the listen-port flags must
        // be distinct so real rqbit does not collide on 4240.
        let lines = std::fs::read_to_string(&log).expect("request log must exist");
        engine_a.kill().expect("kill a");
        engine_b.kill().expect("kill b");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        assert!(lines.contains("GET /stats"), "{lines}");
    }

    #[test]
    fn pick_playable_file_prefers_largest_video() {
        let files = vec![
            super::TorrentFileInfo {
                name: "song.mp3".to_owned(),
                length: 5_000_000,
                included: true,
            },
            super::TorrentFileInfo {
                name: "movie.mkv".to_owned(),
                length: 50_000_000,
                included: true,
            },
            super::TorrentFileInfo {
                name: "trailer.mkv".to_owned(),
                length: 10_000_000,
                included: true,
            },
        ];
        let (idx, file) = super::pick_playable_file(&files).expect("a playable file");
        assert_eq!(idx, 1, "the largest video file wins");
        assert_eq!(file.name, "movie.mkv");
    }

    #[test]
    fn pick_playable_file_falls_back_to_audio() {
        let files = vec![
            super::TorrentFileInfo { name: "readme.txt".to_owned(), length: 1_000, included: true },
            super::TorrentFileInfo {
                name: "album.flac".to_owned(),
                length: 30_000_000,
                included: true,
            },
            super::TorrentFileInfo {
                name: "single.mp3".to_owned(),
                length: 4_000_000,
                included: true,
            },
        ];
        let (idx, file) = super::pick_playable_file(&files).expect("an audio file");
        assert_eq!(idx, 1, "the largest audio file wins when there is no video");
        assert_eq!(file.name, "album.flac");
    }

    #[test]
    fn pick_playable_file_returns_none_without_media() {
        let files = vec![
            super::TorrentFileInfo { name: "readme.txt".to_owned(), length: 1_000, included: true },
            super::TorrentFileInfo {
                name: "data.bin".to_owned(),
                length: 1_000_000,
                included: true,
            },
        ];
        assert!(super::pick_playable_file(&files).is_none());
    }

    #[test]
    fn torrent_item_source_maps_to_engine_sources() {
        use std::path::Path;
        let magnet = super::TorrentItem::Magnet("magnet:?xt=urn:btih:abc".to_owned());
        assert!(matches!(magnet.source(), TorrentSource::Magnet("magnet:?xt=urn:btih:abc")));
        let url = super::TorrentItem::Torrent("https://example.com/movie.torrent".to_owned());
        assert!(matches!(url.source(), TorrentSource::Http("https://example.com/movie.torrent")));
        let local = super::TorrentItem::Torrent("/tmp/movie.torrent".to_owned());
        assert!(
            matches!(local.source(), TorrentSource::Local(p) if p == Path::new("/tmp/movie.torrent"))
        );
    }

    #[test]
    fn torrent_item_label_uses_file_name_or_infohash() {
        let url = super::TorrentItem::Torrent("https://example.com/movie.torrent".to_owned());
        assert_eq!(url.label(), "movie.torrent");
        let magnet = super::TorrentItem::Magnet(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".to_owned(),
        );
        assert_eq!(magnet.label(), "magnet:01234567");
    }

    #[test]
    fn stats_parses_the_real_rqbit_v9_shape() {
        // A real `GET /torrents/{id}/stats/v1` response from rqbit
        // 9.0.0-beta.2 (observed live): `file_progress` is a per-file byte
        // array aligned with `files[]`, `live.download_speed` is the
        // `{mbps, human_readable}` object.
        let json = r#"{"state":"live","file_progress":[1652,1514,1554,1618,1546,129241752,1537,1536,1551,2016,46115],"progress_bytes":129302391,"total_bytes":129302391,"finished":true,"error":null,"live":{"download_speed":{"mbps":0.03,"human_readable":"0.03 MiB/s"},"upload_speed":{"mbps":0.0,"human_readable":"0.00 MiB/s"},"num_peers":3,"num_seeds":5}}"#;
        let stats: super::TorrentStats = serde_json::from_str(json).unwrap();
        assert!(stats.finished);
        assert_eq!(stats.progress_bytes, 129302391);
        assert_eq!(stats.file_progress.len(), 11);
        assert_eq!(stats.file_progress[5], 129241752);
        let speed = stats.live.and_then(|l| l.download_speed).unwrap();
        assert_eq!(speed.mbps, Some(0.03));
        assert_eq!(speed.human_readable.as_deref(), Some("0.03 MiB/s"));
    }

    #[test]
    fn download_complete_requires_finished_and_full_file_progress() {
        let job = super::TorrentDownload {
            engine_base_url: "http://127.0.0.1:3030".to_owned(),
            torrent_id: "1".to_owned(),
            torrent_name: "Movie".to_owned(),
            files: vec![
                super::TorrentDownloadFile {
                    file_idx: 5,
                    file_length: 1000,
                    file_name: "movie.mp4".to_owned(),
                    source_path: std::path::PathBuf::new(),
                    stream_url: "http://s2u:x@127.0.0.1:3030/torrents/1/stream/5".to_owned(),
                },
                super::TorrentDownloadFile {
                    file_idx: 6,
                    file_length: 500,
                    file_name: "sub.srt".to_owned(),
                    source_path: std::path::PathBuf::new(),
                    stream_url: "http://s2u:x@127.0.0.1:3030/torrents/1/stream/6".to_owned(),
                },
            ],
            complete: false,
            deferred: false,
            failures: 0,
        };
        // Finished but a kept file's progress is short → not complete.
        let mut stats = super::TorrentStats { finished: true, file_progress: vec![0, 0, 0, 0, 0, 500, 0], ..Default::default() };
        assert!(!super::download_complete(&stats, &job));
        // Finished and every kept file's index shows its full length.
        stats.file_progress[5] = 1000;
        stats.file_progress[6] = 500;
        assert!(super::download_complete(&stats, &job));
        // Not finished yet even with full progress (defensive).
        stats.finished = false;
        assert!(!super::download_complete(&stats, &job));
        // A missing index (older engine without file_progress) is not complete.
        stats.finished = true;
        stats.file_progress = vec![];
        assert!(!super::download_complete(&stats, &job));
    }

    #[test]
    fn is_torrent_stream_url_matches_rqbit_streams_only() {
        assert!(super::is_torrent_stream_url(
            "http://s2u:token@127.0.0.1:3030/torrents/3/stream/1"
        ));
        assert!(!super::is_torrent_stream_url("http://127.0.0.1:3030/stats"));
        assert!(!super::is_torrent_stream_url("https://youtube.com/watch?v=x"));
        assert!(!super::is_torrent_stream_url("/home/user/movie.mp4"));
    }

    #[test]
    fn move_completed_file_moves_and_avoids_collisions() {
        let dir = std::env::temp_dir().join(format!("s2u-torrent-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("movie.mp4");
        std::fs::write(&file, b"content").unwrap();
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        // Fresh move.
        let target = super::move_completed_file(&file, &dest).unwrap();
        assert_eq!(target, dest.join("movie.mp4"));
        assert!(target.is_file());
        assert!(!file.exists());
        // The source is gone, so a second move of the same path errors.
        assert!(super::move_completed_file(&file, &dest).is_err());

        // Collision: an existing file gets a " (1)" suffix.
        let again = src.join("movie.mp4");
        std::fs::write(&again, b"more").unwrap();
        let target = super::move_completed_file(&again, &dest).unwrap();
        assert_eq!(target, dest.join("movie (1).mp4"));
        assert!(target.is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_completed_file_reports_missing_source() {
        let dir = std::env::temp_dir().join(format!("s2u-torrent-move-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = super::move_completed_file(&dir.join("nope.mp4"), &dir).unwrap_err();
        assert!(err.contains("not found"), "error was {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a fake scanned torrent with the given files (a real fake-rqbit
    /// engine runs behind it; Drop kills it at test end).
    fn fake_scan(files: &[(&str, u64)]) -> super::TorrentScan {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, _log) = fake_engine_bin("scan");
        let port = 31040;
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let engine = start_engine(&test_config(port)).expect("fake engine starts");
        let files = files
            .iter()
            .enumerate()
            .map(|(index, (name, length))| super::ScannedFile {
                index,
                name: name.to_string(),
                length: *length,
            })
            .collect();
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        super::TorrentScan {
            engine: std::sync::Arc::new(engine),
            torrent_id: "1".to_owned(),
            torrent_name: "Fake Pack".to_owned(),
            files,
        }
    }

    #[test]
    fn scan_videos_keeps_positional_indices() {
        let scan = fake_scan(&[
            ("readme.txt", 10),
            ("ep01.mkv", 1000),
            ("ep02.mkv", 900),
            ("ep03.mkv", 800),
        ]);
        let videos = scan.videos();
        let indices: Vec<usize> = videos.iter().map(|f| f.index).collect();
        assert_eq!(indices, vec![1, 2, 3], "the positional indices survive the classification");
        assert_eq!(videos[0].name, "ep01.mkv");
    }

    #[test]
    fn scan_detects_multi_video_and_audio_fallback() {
        // Multi-video (a season pack).
        let multi = fake_scan(&[
            ("ep01.mkv", 1000),
            ("ep02.mkv", 900),
            ("ep03.mkv", 800),
            ("subs.srt", 5),
        ]);
        assert_eq!(multi.videos().len(), 3);
        assert!(multi.audios().is_empty());
        assert_eq!(multi.pick_playable().map(|f| f.name.clone()).as_deref(), Some("ep01.mkv"));

        // Audio-only (an album) falls back to the largest audio file.
        let album = fake_scan(&[
            ("cover.jpg", 50),
            ("song1.flac", 300),
            ("song2.flac", 500),
            ("song3.flac", 400),
        ]);
        assert!(album.videos().is_empty());
        assert_eq!(album.audios().len(), 3);
        assert_eq!(album.pick_playable().map(|f| f.name.clone()).as_deref(), Some("song2.flac"));

        // A data torrent has no playable media.
        let data = fake_scan(&[("readme.txt", 10), ("data.bin", 1000)]);
        assert!(data.videos().is_empty());
        assert!(data.audios().is_empty());
        assert!(data.pick_playable().is_none());
    }

    #[test]
    fn scan_torrent_runs_the_engine_and_reads_the_file_list() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, _log) = fake_engine_bin("scan-flow");
        let port = 31050;
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let config = test_config(port);
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        let (progress_tx, _progress_rx) = crossbeam::channel::unbounded();
        // The fake serves a single movie.mkv file list for any torrent id.
        let scan = super::scan_torrent(
            &super::TorrentItem::Magnet("magnet:?xt=urn:btih:0123456789abcdef".to_owned()),
            &config,
            &cancel_rx,
            &progress_tx,
        )
        .expect("scan succeeds against the fake engine");
        drop(cancel_tx);
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        assert_eq!(scan.torrent_name, "fake.torrent");
        assert_eq!(scan.torrent_id, "1");
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].index, 0);
        assert_eq!(scan.files[0].name, "movie.mkv");
        assert!(scan.files[0].is_video());
        // The stream URL of the scanned file addresses it by index.
        let url = scan.engine.stream_url(&scan.torrent_id, scan.files[0].index as u64);
        assert!(url.contains("/torrents/1/stream/0"), "{url}");
    }

    #[test]
    fn scan_torrent_waits_open_ended_reports_progress_and_cancels() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, log) = fake_engine_bin("open-wait");
        let marker = log.with_file_name("no-files");
        std::fs::write(&marker, "").unwrap();
        let port = 31060;
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let config = test_config(port);
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        let (progress_tx, progress_rx) = crossbeam::channel::unbounded();
        let item =
            super::TorrentItem::Magnet("magnet:?xt=urn:btih:0123456789abcdef".to_owned());
        let item_key = item.source_key();
        let handle = std::thread::spawn(move || {
            super::scan_torrent(&item, &config, &cancel_rx, &progress_tx)
        });

        // Round 18: the wait is alive and reports progress (the fake never
        // delivers a file list while the marker exists) — and it does NOT
        // give up on its own, so no error fires after any N seconds.
        let first = progress_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a progress event arrives while waiting");
        match first {
            crate::shared::events::AppEvent::WorkDone(Ok(
                crate::shared::events::WorkDone::TorrentScanProgress { key, progress },
            )) => {
                assert_eq!(key, item_key, "progress is keyed by the item");
                assert!(progress.elapsed_secs >= 1, "counter ticks");
                assert_eq!(progress.download_speed_kbps, 0.0, "fake reports 0 Mbps");
            }
            other => panic!("expected a progress event, got {other:?}"),
        }
        // Another tick arrives a second later (the counter keeps moving).
        progress_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a second progress event arrives");

        // The user cancels (Esc/close): the wait aborts promptly and the
        // scan reports the cancellation instead of waiting forever.
        cancel_tx.send(()).unwrap();
        let result = handle.join().expect("scan thread joins");
        let err = result.expect_err("cancel aborts the scan");
        assert!(err.contains("cancelled"), "{err}");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn scan_torrent_waits_for_the_torrent_to_be_live_before_offering_play() {
        // Round-18 host finding (2026-08-09): a re-added torrent whose
        // cache files already exist is adopted with `overwrite=true` and
        // spends some seconds `Initializing` (checksum validation) — its
        // stream endpoint errors during that window, so the scan must not
        // return (and the popup must not offer play) until rqbit reports
        // `state == "live"`.
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, log) = fake_engine_bin("not-live");
        let not_live = log.with_file_name("not-live");
        std::fs::write(&not_live, "").unwrap();
        let port = 31060;
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let config = test_config(port);
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        let (progress_tx, progress_rx) = crossbeam::channel::unbounded();
        let item =
            super::TorrentItem::Magnet("magnet:?xt=urn:btih:0123456789abcdef".to_owned());
        let handle = std::thread::spawn(move || {
            super::scan_torrent(&item, &config, &cancel_rx, &progress_tx)
        });

        // The fake serves files but reports `state: initializing`: the
        // scan must keep waiting and ticking progress — NOT return Ok.
        let first = progress_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("progress arrives while initializing");
        assert!(matches!(
            first,
            crate::shared::events::AppEvent::WorkDone(Ok(
                crate::shared::events::WorkDone::TorrentScanProgress { .. }
            ))
        ));
        assert!(!handle.is_finished(), "scan waits while the torrent initializes");

        // The torrent goes live: the scan completes with the file list.
        std::fs::remove_file(&not_live).unwrap();
        let scan = handle.join().expect("scan completes").expect("Ok scan");
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].name, "movie.mkv");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
    }

    #[test]
    fn wait_for_files_aborts_on_cancel_without_a_deadline() {
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, log) = fake_engine_bin("no-peers-cancel");
        let marker = log.with_file_name("no-files");
        std::fs::write(&marker, "").unwrap();
        let port = 31060;
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let config = test_config(port);
        let engine = start_engine(&config).expect("fake engine starts");
        let id = add_torrent(
            &engine,
            super::TorrentItem::Magnet("magnet:?xt=urn:btih:0123456789abcdef".to_owned()).source(),
        )
        .expect("add succeeds");
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        let handle = std::thread::spawn(move || {
            super::wait_for_files(&engine, &id, Some(&cancel_rx))
        });
        // Give the wait a moment — it must NOT fire an error on its own
        // (there is no deadline anymore).
        std::thread::sleep(std::time::Duration::from_millis(500));
        cancel_tx.send(()).unwrap();
        let err = match handle.join().expect("wait thread joins") {
            Ok(_) => panic!("expected the wait to be cancelled"),
            Err(err) => err,
        };
        assert!(err.contains("cancelled"), "{err}");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn scan_torrent_survives_a_slow_add_and_cancels_mid_add() {
        // Round-18 regression (host finding 2026-08-09): rqbit's real
        // `POST /torrents` blocks until a cold magnet's metainfo resolves —
        // sometimes for minutes. The old code ran the add on the scan
        // thread with the 5 s HTTP agent timeout, so the scan failed at the
        // add step ("loading stops after a set period") before the
        // open-ended wait ever started. The fake's `slow-add` marker holds
        // the add response for 8 s: the scan must (1) not give up, (2) keep
        // ticking the wait window, and (3) abort promptly on cancel.
        let _guard = RQBIT_ENV_LOCK.lock().unwrap();
        let (bin, log) = fake_engine_bin("slow-add");
        let marker = log.with_file_name("slow-add");
        std::fs::write(&marker, "8").unwrap();
        let port = 31060;
        unsafe {
            std::env::set_var("S2UDIO_RQBIT_BIN", &bin);
        }
        let config = test_config(port);
        let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
        let (progress_tx, progress_rx) = crossbeam::channel::unbounded();
        let item =
            super::TorrentItem::Magnet("magnet:?xt=urn:btih:0123456789abcdef".to_owned());
        let item_key = item.source_key();
        let handle = std::thread::spawn(move || {
            super::scan_torrent(&item, &config, &cancel_rx, &progress_tx)
        });

        // The wait window keeps ticking while the add is in flight (the
        // fake does not answer for 8 s): a progress event arrives within a
        // couple of seconds and the scan has NOT failed.
        let first = progress_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("progress arrives while the add is still pending");
        match first {
            crate::shared::events::AppEvent::WorkDone(Ok(
                crate::shared::events::WorkDone::TorrentScanProgress { key, .. },
            )) => assert_eq!(key, item_key),
            other => panic!("expected progress during the add, got {other:?}"),
        }

        // The user cancels mid-add: the scan aborts promptly instead of
        // waiting out the 8 s add (or erroring at a 5 s deadline).
        cancel_tx.send(()).unwrap();
        let result = handle.join().expect("scan thread joins");
        let err = result.expect_err("cancel aborts the scan");
        assert!(err.contains("cancelled"), "{err}");
        unsafe {
            std::env::remove_var("S2UDIO_RQBIT_BIN");
        }
        let _ = std::fs::remove_file(&marker);
    }

}

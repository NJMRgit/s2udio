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
    io::Read, net::TcpListener, path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc, time::{Duration, Instant},
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
    /// Loopback auth-injecting proxy for the web UI (`web_url()` points
    /// at it; the engine port itself stays auth-protected — browsers do
    /// not replay URL-userinfo credentials on SPA `fetch()` calls, see
    /// `torrent_proxy`). None only when the proxy bind failed (the
    /// userinfo URL is the fallback).
    webui_proxy: Option<crate::core::torrent_proxy::WebUiProxy>,
}
impl Drop for TorrentEngine {
    fn drop(&mut self) {
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
        "Basic {}", base64::engine::general_purpose::STANDARD.encode(user_pass
        .as_bytes())
    )
}
/// Spawn a random `user:pass` pair for the engine's HTTP auth (defense in
/// depth on 127.0.0.1; the engine gets it via `RQBIT_HTTP_BASIC_AUTH_USERPASS`).
fn random_user_pass() -> String {
    let token: String = std::iter::repeat_with(|| {
            format!("{:016x}", rand::random::< u64 > ())
        })
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
        Err(
            format!(
                "rqbit not found — install (cargo install rqbit / static binary) or set {RQBIT_BIN_ENV}"
            ),
        )
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
        .map_err(|err| {
            format!("Cannot create torrent cache dir {}: {err}", cache_dir.display())
        })?;
    let port = find_free_port(config.port)
        .ok_or_else(|| format!("No free port found near {} for rqbit", config.port))?;
    let listen_port = find_free_port(port + 1)
        .ok_or_else(|| {
            format!("No free listen port found near {} for rqbit", config.port)
        })?;
    let user_pass = random_user_pass();
    let auth_header = basic_auth_header(&user_pass);
    let mut cmd = Command::new(&bin);
    cmd.arg("--http-api-listen-addr")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--listen-port")
        .arg(listen_port.to_string())
        .arg("--disable-dht-persistence");
    if let Some(proxy) = config.socks_proxy.as_deref().filter(|p| !p.trim().is_empty()) {
        cmd.arg("--socks-url").arg(proxy).arg("--disable-tcp-listen");
    }
    cmd.arg("server")
        .arg("start")
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
    let child = cmd
        .spawn()
        .map_err(|err| format!("Failed to launch rqbit ({bin}): {err}"))?;
    let base_url = format!("http://127.0.0.1:{port}");
    let mut engine = TorrentEngine {
        child,
        base_url,
        auth_header: auth_header.clone(),
        user_pass,
        cache_dir,
        webui_proxy: None,
    };
    match wait_until_ready(&engine) {
        Ok(()) => {
            engine.webui_proxy = crate::core::torrent_proxy::WebUiProxy::spawn(
                    port,
                    auth_header.clone(),
                )
                .ok();
            if engine.webui_proxy.is_none() {
                log::warn!("Failed to spawn the rqbit web-UI auth proxy");
            }
            Ok(engine)
        }
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
            "http://{}@{}/torrents/{torrent_id}/stream/{file_id}", self.user_pass, self
            .base_url.trim_start_matches("http://")
        )
    }
    /// The rqbit web UI URL (`/web/`). Normally the auth-injecting
    /// proxy URL (`http://127.0.0.1:<proxy port>/web/`, no credentials —
    /// the SPA's fetch() calls work through it); falls back to the raw
    /// userinfo URL (`http://user:pass@…/web/`) when the proxy could not
    /// be spawned. Opens in a browser for torrent management /
    /// VPN-route verification.
    pub fn web_url(&self) -> String {
        match &self.webui_proxy {
            Some(proxy) => format!("http://127.0.0.1:{}/web/", proxy.port()),
            None => {
                format!(
                    "http://{}@{}/web/", self.user_pass, self.base_url
                    .trim_start_matches("http://")
                )
            }
        }
    }
    /// The engine child's pid (used by the CLI `s2udio rq` registration
    /// file so a separate process can stop the engine).
    pub fn pid(&self) -> u32 {
        self.child.id()
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
        TorrentSource::Local(file) => {
            std::fs::read(&file)
                .map_err(|err| format!("Cannot read {}: {err}", file.display()))?
        }
    };
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
    if let Some(s) = parsed.as_str() {
        return Ok(s.to_owned());
    }
    parsed
        .get("id")
        .and_then(|v| {
            v.as_str().map(str::to_owned).or_else(|| v.as_i64().map(|n| n.to_string()))
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
            Self::Magnet(magnet) => {
                crate::ui::modals::paste::magnet_infohash(magnet)
                    .map_or_else(
                        || "magnet link".to_owned(),
                        |hash| format!("magnet:{hash}"),
                    )
            }
            Self::Torrent(torrent) => {
                std::path::Path::new(torrent)
                    .file_name()
                    .map_or_else(
                        || torrent.clone(),
                        |n| n.to_string_lossy().into_owned(),
                    )
            }
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
pub fn torrent_details(
    engine: &TorrentEngine,
    id: &str,
) -> Result<TorrentDetails, String> {
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
pub fn pick_playable_file(
    files: &[TorrentFileInfo],
) -> Option<(usize, &TorrentFileInfo)> {
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
    if cancel.try_recv().is_ok() {
        return Err("Scan cancelled".to_owned());
    }
    let engine = start_engine(config)?;
    let started = Instant::now();
    let mut last_progress = started;
    let (add_tx, add_rx) = crossbeam::channel::bounded(1);
    let add_base_url = engine.base_url.clone();
    let add_auth = engine.auth_header.clone();
    let add_item = item.clone();
    let key_for_add = item.source_key();
    let add_thread = std::thread::Builder::new()
        .name(
            format!(
                "torrent-add-{}", key_for_add.chars().take(24).collect::< String > ()
            ),
        )
        .spawn(move || {
            let result = add_torrent_at(&add_base_url, &add_auth, add_item.source());
            let _ = add_tx.send(result);
        })
        .map_err(|err| format!("Failed to spawn torrent add thread: {err}"))?;
    let torrent_id = loop {
        if cancel.try_recv().is_ok() {
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
            let _ = progress
                .send(
                    crate::shared::events::AppEvent::WorkDone(
                        Ok(crate::shared::events::WorkDone::TorrentScanProgress {
                            key: item.source_key(),
                            progress: TorrentScanProgress {
                                elapsed_secs: started.elapsed().as_secs(),
                                download_speed_kbps: 0.0,
                            },
                        }),
                    ),
                );
        }
        std::thread::sleep(READY_POLL_INTERVAL);
    };
    let _ = add_thread;
    loop {
        if cancel.try_recv().is_ok() {
            return Err("Scan cancelled".to_owned());
        }
        match torrent_details(&engine, &torrent_id) {
            Ok(details) if !details.files.is_empty() => {
                let live = torrent_stats(&engine, &torrent_id)
                    .ok()
                    .and_then(|stats| stats.state)
                    .is_some_and(|state| state == "live");
                if live {
                    let files: Vec<ScannedFile> = details
                        .files
                        .iter()
                        .enumerate()
                        .map(|(index, f)| ScannedFile {
                            index,
                            name: f.name.clone(),
                            length: f.length,
                        })
                        .collect();
                    let torrent_name = details
                        .name
                        .clone()
                        .unwrap_or_else(|| item.label());
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
                .map(|mbps| mbps * 125.0)
                .unwrap_or(0.0);
            let _ = progress
                .send(
                    crate::shared::events::AppEvent::WorkDone(
                        Ok(crate::shared::events::WorkDone::TorrentScanProgress {
                            key: item.source_key(),
                            progress: TorrentScanProgress {
                                elapsed_secs: started.elapsed().as_secs(),
                                download_speed_kbps,
                            },
                        }),
                    ),
                );
        }
        std::thread::sleep(READY_POLL_INTERVAL);
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
        && job
            .files
            .iter()
            .all(|file| {
                stats.file_progress.get(file.file_idx).copied().unwrap_or(0)
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
            std::fs::copy(source, &target)
                .map_err(|err| format!("Cannot copy {}: {err}", source.display()))?;
            let _ = std::fs::remove_file(source);
            Ok(target)
        }
        Err(err) => Err(format!("Cannot move {}: {err}", source.display())),
    }
}

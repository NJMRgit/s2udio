//! A localhost caching proxy for web audio/video streams (round 107).
//!
//! MPD and mpv fetch a resolved stream URL straight from the CDN today, so
//! a replayed item is fetched again and the only buffer is the player's own
//! (MPD's `audio_buffer_size` ring, mpv's in-memory cache). This module
//! gives both players a stable **localhost** URL instead:
//!
//! ```text
//! resolve (yt-dlp) --register--> proxy  http://127.0.0.1:<port>/s2u/<class>/<key>
//!                                    |
//!                                    +- cache hit + complete --> serve from disk (instant replay)
//!                                    +- miss/partial ----------> GET the CDN, stream through,
//!                                                                write to disk as it arrives
//! ```
//!
//! The key is a stable hash of the entry's **original link** (a watch URL),
//! not the signed CDN URL, so the same row keeps the same proxy URL across
//! re-resolves and restarts. A completed cache file needs no upstream at
//! all, which is what makes an already-played item replay instantly while it
//! is still in the buffer.
//!
//! Budgets are per class: 1 GiB for audio, 2 GiB for video. Eviction is
//! least-recently-used, never touching an entry with a live fill or reader.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// Audio stream budget (1 GiB).
pub const AUDIO_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
/// Video stream budget (2 GiB).
pub const VIDEO_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Which budget an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamClass {
    Audio,
    Video,
}

impl StreamClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            _ => None,
        }
    }

    /// The class's total on-disk budget.
    pub fn budget(self) -> u64 {
        match self {
            Self::Audio => AUDIO_BUDGET_BYTES,
            Self::Video => VIDEO_BUDGET_BYTES,
        }
    }
}

/// A stable cache key for an entry, derived from its original link. FNV-1a
/// (64 bit) is enough: the key only needs to be stable and collision-cheap
/// within one class, and the original link is not secret.
pub fn key_for(original_url: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in original_url.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Parse a single HTTP `Range: bytes=S-E` / `bytes=S-` header.
fn parse_range(header: &str) -> Option<(u64, Option<u64>)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    // Multi-range requests are not used by MPD/mpv; take the first.
    let first = spec.split(',').next()?.trim();
    let (start, end) = first.split_once('-')?;
    if start.is_empty() {
        // A suffix range (`bytes=-500`): unsupported, fall back to the whole
        // body rather than guessing the total.
        return None;
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() { None } else { Some(end.parse().ok()?) };
    Some((start, end))
}

/// The total length in a `Content-Range: bytes S-E/TOTAL` header.
fn content_range_total(header: &str) -> Option<u64> {
    let total = header.trim().strip_prefix("bytes ")?.split_once('/')?.1.trim();
    (total != "*").then(|| total.parse().ok()).flatten()
}

/// The bytes a request asks for, resolved against a known total.
fn wanted_range(request: Option<(u64, Option<u64>)>, total: u64) -> Option<(u64, u64)> {
    let last = total.saturating_sub(1);
    match request {
        None => Some((0, last)),
        Some((start, end)) => {
            if total == 0 || start > last {
                return None;
            }
            let end = end.unwrap_or(last).min(last);
            (end >= start).then_some((start, end))
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Per-entry metadata, persisted in `<root>/index.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    class: StreamClass,
    #[serde(default)]
    original_url: String,
    /// Bytes written so far (the whole file once `complete`).
    size: u64,
    /// Upstream total, when known.
    total: Option<u64>,
    complete: bool,
    #[serde(default)]
    content_type: Option<String>,
    last_used: u64,
    /// The last fill failed (a non-2xx upstream, a network error). An
    /// incomplete entry with a live registration is otherwise assumed to be
    /// filling; this flag is what makes the app re-resolve it from its
    /// original link.
    #[serde(default)]
    failed: bool,
}

/// The current upstream URL for a key. In memory only: a signed CDN URL
/// expires, so it is re-registered on every resolve.
#[derive(Debug, Clone)]
struct Registration {
    original_url: String,
    upstream_url: String,
}

/// Progress of one key's fill, shared with every client streaming it.
#[derive(Debug, Default)]
struct FillProgress {
    /// Response headers are in: `total`/`content_type` are final.
    started: bool,
    filled: u64,
    total: Option<u64>,
    content_type: Option<String>,
    complete: bool,
    failed: bool,
    /// Why the fill failed (diagnostics; also read by the tests).
    error: Option<String>,
}

struct Fill {
    progress: Mutex<FillProgress>,
    ready: Condvar,
}

struct Inner {
    registrations: HashMap<String, Registration>,
    entries: HashMap<String, Entry>,
    fills: HashMap<String, Arc<Fill>>,
    /// Live readers per key (a fill counts while it runs).
    active: HashMap<String, usize>,
}

/// The singleton proxy. `port` is `None` when the loopback bind failed; the
/// caller then falls back to the upstream URL.
pub struct StreamCache {
    port: Option<u16>,
    root: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

static PROXY: OnceLock<Arc<StreamCache>> = OnceLock::new();

/// Start the buffer (bind its loopback port and load the on-disk index).
/// Called once at app startup so a queue restored from MPD still resolves
/// its buffered URLs (their upstream URLs are gone; the original links and
/// any completed cache files are not).
pub fn init(cache_dir: Option<&Path>) {
    let _ = PROXY.get_or_init(|| StreamCache::start(cache_dir));
}

/// Register the entry's current upstream URL and return the stable
/// localhost URL to hand MPD/mpv. Falls back to `upstream_url` itself when
/// the proxy could not start, so a failure never blocks playback.
pub fn proxy_url(
    cache_dir: Option<&Path>,
    class: StreamClass,
    original_url: &str,
    upstream_url: &str,
) -> String {
    let cache = PROXY.get_or_init(|| StreamCache::start(cache_dir));
    cache.register(class, original_url, upstream_url);
    match cache.port {
        Some(port) => {
            format!("http://127.0.0.1:{port}/s2u/{}/{}", class.as_str(), key_for(original_url))
        }
        None => upstream_url.to_owned(),
    }
}

/// Start caching an entry's bytes now, without a client (the one-entry look
/// ahead's prefetch). A no-op when the proxy is unavailable.
pub fn preload(
    cache_dir: Option<&Path>,
    class: StreamClass,
    original_url: &str,
    upstream_url: &str,
) {
    let cache = PROXY.get_or_init(|| StreamCache::start(cache_dir));
    cache.register(class, original_url, upstream_url);
    if cache.port.is_some() {
        cache.ensure_fill(&key_for(original_url));
    }
}

/// The `/s2u/<class>/<key>` key of a buffered stream URL, when `url` is one.
pub fn buffered_key(url: &str) -> Option<&str> {
    let rest = url.split_once("/s2u/")?.1;
    let (class, key) = rest.split_once('/')?;
    StreamClass::from_str(class)?;
    (!key.is_empty()).then_some(key)
}

/// Whether `url` is one of this proxy's buffered stream URLs.
pub fn is_buffered_url(url: &str) -> bool {
    buffered_key(url).is_some()
}

/// The original link a buffered URL was registered from — the link a row
/// must be re-resolved from when its bytes are not (fully) cached. `None`
/// for a non-buffered URL or an unknown key.
pub fn original_link_for_url(url: &str) -> Option<String> {
    let key = buffered_key(url)?;
    let cache = PROXY.get()?;
    let inner = lock(&cache.inner);
    inner
        .entries
        .get(key)
        .map(|entry| entry.original_url.clone())
        .or_else(|| inner.registrations.get(key).map(|reg| reg.original_url.clone()))
        .filter(|original| !original.is_empty())
}

/// Whether a buffered URL needs a fresh resolve of its original link.
///
/// A **complete** entry on this run's port never does (the proxy serves it
/// from disk). It does when the row points at a **different port** than this
/// run's proxy (a queue from a run that could not reuse the port — the URL is
/// dead regardless of the cache), or when the proxy cannot fetch it: no live
/// upstream registration (a restart, or a row restored from MPD) or the last
/// fill failed. While a registration is live the fill is in progress or will
/// start on demand — re-resolving then is what made the look ahead swap the
/// same row forever (round 107 regression).
///
/// Round 109: a key some client is **reading right now** is never stale
/// either, even after a failed fill. MPD streaming the row is proof the proxy
/// is serving it, and the current-song hook re-resolved such a row with
/// `ReplaceAndPlay` — which deleted it, re-added it and sent `playid`, making
/// MPD restart the playing track from the beginning on every upstream hiccup.
pub fn needs_upstream_refresh(url: &str) -> bool {
    let Some(key) = buffered_key(url) else { return false };
    let Some(cache) = PROXY.get() else { return true };
    if let Some(port) = cache.port
        && url_port(url).is_some_and(|row_port| row_port != port)
    {
        return true;
    }
    let inner = lock(&cache.inner);
    entry_needs_refresh(
        inner.entries.get(key),
        inner.registrations.contains_key(key),
        inner.active.get(key).is_some_and(|readers| *readers > 0),
    )
}

/// This run's proxy port, when the proxy is running.
pub fn current_port() -> Option<u16> {
    PROXY.get().and_then(|cache| cache.port)
}

/// The port a buffered URL points at.
fn url_port(url: &str) -> Option<u16> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let (port, tail) = rest.split_once('/')?;
    tail.starts_with("s2u/").then(|| port.parse().ok())?
}

/// `url` rewritten to `port` when it is a buffered URL on a different port
/// (a queue restored from a previous run); `None` when it is not a buffered
/// URL or already on `port`.
pub fn url_with_port(url: &str, port: u16) -> Option<String> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let (row_port, tail) = rest.split_once('/')?;
    if !tail.starts_with("s2u/") {
        return None;
    }
    if row_port.parse::<u16>().ok()? == port {
        return None;
    }
    Some(format!("http://127.0.0.1:{port}/{tail}"))
}

/// The pure decision behind [`needs_upstream_refresh`].
fn entry_needs_refresh(entry: Option<&Entry>, registered: bool, reading: bool) -> bool {
    match entry {
        None => true,
        Some(entry) if entry.complete => false,
        // A live reader (MPD playing or prefetching this row) means the proxy
        // is serving it; a failed fill is retried rather than re-resolved.
        Some(_) if reading => false,
        Some(entry) => entry.failed || !registered,
    }
}

impl StreamCache {
    fn start(cache_dir: Option<&Path>) -> Arc<Self> {
        // Never fall back to a bare relative path: a test (or a config with
        // no cache dir) must not drop a `stream-buffer/` in the process cwd.
        let root = cache_dir
            .map(|dir| dir.join("stream-buffer"))
            .or_else(|| crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join("stream-buffer")))
            .unwrap_or_else(|| std::env::temp_dir().join("s2udio-stream-buffer"));
        let _ = std::fs::create_dir_all(root.join("audio"));
        let _ = std::fs::create_dir_all(root.join("video"));

        let mut entries = load_manifest(&root);
        // Drop manifest rows whose file vanished (a manual clean).
        entries.retain(|key, entry| {
            !entry.complete || cache_path(&root, entry.class, key).is_file()
        });
        let inner = Arc::new(Mutex::new(Inner {
            registrations: HashMap::new(),
            entries,
            fills: HashMap::new(),
            active: HashMap::new(),
        }));

        let listener = bind_proxy_listener(&root);
        let port = listener
            .as_ref()
            .and_then(|listener| listener.local_addr().ok())
            .map(|addr| addr.port());
        match port {
            Some(port) => remember_port(&root, port),
            None => {
                log::warn!("Could not start the stream buffer proxy; streams use the CDN directly");
            }
        }
        let cache = Arc::new(Self { port, root: root.clone(), inner: Arc::clone(&inner) });
        if let Some(listener) = listener {
            let accept_cache = Arc::clone(&cache);
            std::thread::spawn(move || accept_loop(listener, inner, root, accept_cache));
        }
        cache
    }

    fn register(&self, class: StreamClass, original_url: &str, upstream_url: &str) {
        let key = key_for(original_url);
        let mut inner = lock(&self.inner);
        inner.registrations.insert(
            key.clone(),
            Registration {
                original_url: original_url.to_owned(),
                upstream_url: upstream_url.to_owned(),
            },
        );
        inner
            .entries
            .entry(key)
            .and_modify(|entry| {
                entry.last_used = now_unix();
                // A freshly registered upstream URL may work: clear a
                // previous fill failure so the row is not re-resolved again.
                entry.failed = false;
            })
            .or_insert_with(|| Entry {
                class,
                original_url: original_url.to_owned(),
                size: 0,
                total: None,
                complete: false,
                content_type: None,
                last_used: now_unix(),
                failed: false,
            });
        // Persist the key -> original link mapping so a queue restored from
        // MPD after a restart can still be re-resolved through the proxy
        // (the signed upstream URL is gone; the original link is not).
        save_manifest(&self.root, &inner);
    }

    /// Start the fill for `key` if one is not already running and the entry
    /// is not already complete. `None` when there is nothing to fill.
    fn ensure_fill(self: &Arc<Self>, key: &str) -> Option<Arc<Fill>> {
        let (fill, reg) = {
            let mut inner = lock(&self.inner);
            if let Some(existing) = inner.fills.get(key) {
                return Some(Arc::clone(existing));
            }
            if inner.entries.get(key).is_some_and(|entry| entry.complete) {
                return None;
            }
            let reg = inner.registrations.get(key).cloned()?;
            let fill = Arc::new(Fill {
                progress: Mutex::new(FillProgress::default()),
                ready: Condvar::new(),
            });
            inner.fills.insert(key.to_owned(), Arc::clone(&fill));
            *inner.active.entry(key.to_owned()).or_insert(0) += 1;
            (fill, reg)
        };
        let this = Arc::clone(self);
        let key_owned = key.to_owned();
        let thread_fill = Arc::clone(&fill);
        std::thread::spawn(move || {
            fill_entry(&this, &key_owned, &reg, &thread_fill);
            let mut inner = lock(&this.inner);
            inner.fills.remove(&key_owned);
            dec_active(&mut inner, &key_owned);
            save_manifest(&this.root, &inner);
        });
        Some(fill)
    }

    /// Whether the key currently has a completed cache file.
    fn is_complete(&self, key: &str) -> bool {
        lock(&self.inner).entries.get(key).is_some_and(|entry| entry.complete)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn dec_active(inner: &mut Inner, key: &str) {
    if let Some(count) = inner.active.get_mut(key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            inner.active.remove(key);
        }
    }
}

/// How many times one fill may fetch the upstream before it gives up. A CDN can
/// reset a long transfer (googlevideo did, at ~2 min — see
/// [`UPSTREAM_RANGE_REQUEST`]); one reset must not fail the entry.
const FILL_ATTEMPTS: u32 = 3;
/// The pause before a fill retries a failed fetch.
const FILL_RETRY_DELAY: Duration = Duration::from_millis(250);

/// The `Range` header every fill sends. Measured 2026-10-08 on this machine, a
/// 4.7 MB itag-251 audio URL: a plain GET is throttled to ~34 KB/s (137.7 s)
/// and reset around the two-minute mark, while `Range: bytes=0-` returns the
/// **same, byte-identical** body at full speed (0.25 s). A fill that took
/// minutes stalled the row MID-PLAYBACK until the CDN reset it, and the failed
/// fill then made the app re-resolve and restart the playing track. MPD and
/// mpv already open a *seek* with this shape; the players' plain open is what
/// googlevideo throttles.
const UPSTREAM_RANGE_REQUEST: &str = "bytes=0-";

/// Fetch `key`'s upstream URL, writing the body to the cache file. A failed
/// fetch is retried ([`FILL_ATTEMPTS`]) before the entry is marked failed, so a
/// client streaming the row is not cut off by one reset.
fn fill_entry(cache: &Arc<StreamCache>, key: &str, reg: &Registration, fill: &Arc<Fill>) {
    let path = cache_path(&cache.root, class_of(cache, key), key);
    let class = class_of(cache, key);
    let mut last_error: Option<io::Error> = None;
    for attempt in 1..=FILL_ATTEMPTS {
        match fill_attempt(&path, reg, fill) {
            Ok(filled) => {
                // Index the entry BEFORE publishing completion: a waiter that
                // observes `progress.complete` must also observe `is_complete`.
                let (total, content_type) = {
                    let progress = lock(&fill.progress);
                    (progress.total, progress.content_type.clone())
                };
                {
                    let mut inner = lock(&cache.inner);
                    let entry = inner.entries.entry(key.to_owned()).or_insert_with(|| Entry {
                        class,
                        original_url: reg.original_url.clone(),
                        size: 0,
                        total: None,
                        complete: false,
                        content_type: None,
                        last_used: now_unix(),
                        failed: false,
                    });
                    entry.size = filled;
                    entry.total = total.or(Some(filled));
                    entry.complete = true;
                    entry.content_type = content_type;
                    entry.failed = false;
                    entry.last_used = now_unix();
                    enforce_budget(&mut inner, class, &cache.root);
                }
                let mut progress = lock(&fill.progress);
                progress.complete = true;
                progress.total = progress.total.or(Some(filled));
                fill.ready.notify_all();
                return;
            }
            Err(err) => {
                log::warn!(
                    error:? = err, key:? = key, attempt, attempts = FILL_ATTEMPTS;
                    "Stream buffer fill attempt failed"
                );
                // The retry rewrites the file from byte 0: a reader parked on
                // the old count must wait for the rewrite, not read it as an
                // early end of the body.
                {
                    let mut progress = lock(&fill.progress);
                    progress.filled = 0;
                    fill.ready.notify_all();
                }
                last_error = Some(err);
                if attempt < FILL_ATTEMPTS {
                    std::thread::sleep(FILL_RETRY_DELAY);
                }
            }
        }
    }

    let err = last_error.unwrap_or_else(|| io::Error::other("upstream fetch failed"));
    log::warn!(error:? = err, key:? = key; "Stream buffer fill failed");
    {
        let mut inner = lock(&cache.inner);
        if let Some(entry) = inner.entries.get_mut(key) {
            entry.failed = true;
        }
    }
    let mut progress = lock(&fill.progress);
    progress.started = true;
    progress.failed = true;
    progress.error = Some(err.to_string());
    fill.ready.notify_all();
}

/// One fetch of the registered upstream, written to `path` from byte 0. See
/// [`UPSTREAM_RANGE_REQUEST`] for why the request carries a range. A body short
/// of the announced total is an error (the CDN closed early), never a complete
/// cache entry.
fn fill_attempt(path: &Path, reg: &Registration, fill: &Arc<Fill>) -> io::Result<u64> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .build();
    let response = agent
        .get(&reg.upstream_url)
        .set("Range", UPSTREAM_RANGE_REQUEST)
        .call()
        .map_err(|err| io::Error::other(err.to_string()))?;
    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(io::Error::other(format!("upstream answered HTTP {status}")));
    }
    let content_type = response.header("Content-Type").map(str::to_owned);
    // A 206 answers with the *remaining* bytes in Content-Length, so the full
    // length comes from Content-Range (`bytes 0-…/total`).
    let total = response
        .header("Content-Range")
        .and_then(content_range_total)
        .or_else(|| response.header("Content-Length").and_then(|value| value.parse::<u64>().ok()));
    {
        let mut progress = lock(&fill.progress);
        progress.started = true;
        progress.total = total;
        progress.content_type = content_type.clone();
        fill.ready.notify_all();
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    let mut reader = response.into_reader();
    let mut buffer = [0u8; 64 * 1024];
    let mut filled: u64 = 0;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        filled += read as u64;
        let mut progress = lock(&fill.progress);
        progress.filled = filled;
        fill.ready.notify_all();
    }
    if let Some(total) = total
        && filled < total
    {
        return Err(io::Error::other(format!(
            "upstream closed after {filled} of {total} bytes"
        )));
    }
    Ok(filled)
}

fn class_of(cache: &StreamCache, key: &str) -> StreamClass {
    lock(&cache.inner)
        .entries
        .get(key)
        .map_or(StreamClass::Audio, |entry| entry.class)
}

/// Bind the proxy listener, preferring the port the previous run used. The
/// buffered row URLs stored in MPD's queue embed the port, so an ephemeral
/// port would make every buffered row from an earlier session dead after a
/// restart — MPD then fails forward through them ("keeps moving to the next
/// item"). The port is persisted in `<root>/port`; a run that cannot reuse
/// it falls back to an ephemeral port and the queue repair rewrites the rows.
fn bind_proxy_listener(root: &Path) -> Option<TcpListener> {
    if let Some(port) = persisted_port(root)
        && let Ok(listener) = TcpListener::bind(("127.0.0.1", port))
    {
        return Some(listener);
    }
    TcpListener::bind(("127.0.0.1", 0)).ok()
}

/// The proxy port the previous run used, when one was recorded.
fn persisted_port(root: &Path) -> Option<u16> {
    std::fs::read_to_string(root.join("port")).ok()?.trim().parse().ok()
}

/// Record the port so the next run can reuse it.
fn remember_port(root: &Path, port: u16) {
    let _ = std::fs::write(root.join("port"), port.to_string());
}

fn cache_path(root: &Path, class: StreamClass, key: &str) -> PathBuf {
    root.join(class.as_str()).join(format!("{key}.bin"))
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("index.json")
}

fn load_manifest(root: &Path) -> HashMap<String, Entry> {
    let Ok(text) = std::fs::read_to_string(manifest_path(root)) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_manifest(root: &Path, inner: &Inner) {
    let Ok(text) = serde_json::to_string(&inner.entries) else { return };
    let path = manifest_path(root);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Evict least-recently-used entries of `class` until the class is within
/// its budget. Entries with a live fill or reader are never touched.
fn enforce_budget(inner: &mut Inner, class: StreamClass, root: &Path) {
    let budget = class.budget();
    let mut total: u64 = inner
        .entries
        .values()
        .filter(|entry| entry.class == class)
        .map(|entry| entry.size)
        .sum();
    if total <= budget {
        return;
    }
    let mut candidates: Vec<(String, u64)> = inner
        .entries
        .iter()
        .filter(|(key, entry)| {
            entry.class == class
                && !inner.fills.contains_key(*key)
                && !inner.active.contains_key(*key)
        })
        .map(|(key, entry)| (key.clone(), entry.last_used))
        .collect();
    candidates.sort_by_key(|(_, last_used)| *last_used);
    for (key, _) in candidates {
        if total <= budget {
            break;
        }
        if let Some(entry) = inner.entries.remove(&key) {
            let size = entry.size;
            let _ = std::fs::remove_file(cache_path(root, entry.class, &key));
            total = total.saturating_sub(size);
            log::debug!(key:? = key; "Evicted a stream buffer entry");
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------------------

fn accept_loop(
    listener: TcpListener,
    inner: Arc<Mutex<Inner>>,
    root: PathBuf,
    cache: Arc<StreamCache>,
) {
    let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for client in listener.incoming() {
        let Ok(client) = client else { continue };
        let inner = Arc::clone(&inner);
        let root = root.clone();
        let cache = Arc::clone(&cache);
        threads.push(std::thread::spawn(move || {
            let _ = handle_client(client, &inner, &root, &cache);
        }));
        if threads.len() > 64 {
            threads.drain(0..threads.len() - 64);
        }
    }
}

fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    loop {
        let read = stream.read(&mut tmp)?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed"));
        }
        buf.extend_from_slice(&tmp[..read]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            buf.truncate(end);
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        if buf.len() > 1 << 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"));
        }
    }
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn request_path(head: &str) -> Option<&str> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" {
        return None;
    }
    parts.next()
}

fn write_head(stream: &mut TcpStream, status: &str, headers: &[(&str, String)]) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    stream.write_all(out.as_bytes())
}

fn handle_client(
    mut client: TcpStream,
    inner: &Arc<Mutex<Inner>>,
    root: &Path,
    cache: &Arc<StreamCache>,
) -> io::Result<()> {
    let head = match read_head(&mut client) {
        Ok(head) => head,
        Err(_) => return Ok(()),
    };
    let Some(path) = request_path(&head) else {
        return write_head(&mut client, "405 Method Not Allowed", &[]);
    };
    let Some(rest) = path.strip_prefix("/s2u/") else {
        return write_head(&mut client, "404 Not Found", &[]);
    };
    let Some((class_raw, key)) = rest.split_once('/') else {
        return write_head(&mut client, "404 Not Found", &[]);
    };
    let Some(class) = StreamClass::from_str(class_raw) else {
        return write_head(&mut client, "404 Not Found", &[]);
    };
    let key = key.to_owned();

    let known = {
        let mut state = lock(inner);
        let known = state.registrations.contains_key(&key)
            || state.entries.get(&key).is_some_and(|entry| entry.complete);
        if known {
            *state.active.entry(key.clone()).or_insert(0) += 1;
        }
        known
    };
    if !known {
        return write_head(&mut client, "404 Not Found", &[]);
    }

    let result = if cache.is_complete(&key) {
        serve_from_disk(&mut client, root, class, &key, header_value(&head, "Range"))
    } else {
        serve_via_fill(&mut client, inner, root, cache, class, &key, header_value(&head, "Range"))
    };

    let mut state = lock(inner);
    dec_active(&mut state, &key);
    if let Some(entry) = state.entries.get_mut(&key) {
        entry.last_used = now_unix();
    }
    result
}

/// Serve a completed cache file: any range, straight from disk.
fn serve_from_disk(
    client: &mut TcpStream,
    root: &Path,
    class: StreamClass,
    key: &str,
    range_header: Option<&str>,
) -> io::Result<()> {
    let path = cache_path(root, class, key);
    let mut file = File::open(&path)?;
    let total = file.metadata()?.len();
    if total == 0 {
        return write_head(client, "404 Not Found", &[]);
    }
    let wanted = parse_range(range_header.unwrap_or(""));
    let Some((start, end)) = wanted_range(wanted, total) else {
        return write_head(
            client,
            "416 Range Not Satisfiable",
            &[("Content-Range", format!("bytes */{total}"))],
        );
    };
    let length = end.saturating_sub(start) + 1;
    let partial = wanted.is_some();
    let status = if partial { "206 Partial Content" } else { "200 OK" };
    let mut headers = vec![
        ("Content-Type", "application/octet-stream".to_owned()),
        ("Accept-Ranges", "bytes".to_owned()),
        ("Content-Length", length.to_string()),
    ];
    if partial {
        headers.push(("Content-Range", format!("bytes {start}-{end}/{total}")));
    }
    write_head(client, status, &headers)?;
    file.seek(SeekFrom::Start(start))?;
    copy_exact(&mut file, client, length)
}

/// Stream a not-yet-complete entry: start (or join) its fill and copy the
/// growing cache file. A range past the cached prefix is forwarded upstream
/// so a seek mid-fill still works.
fn serve_via_fill(
    client: &mut TcpStream,
    inner: &Arc<Mutex<Inner>>,
    root: &Path,
    cache: &Arc<StreamCache>,
    class: StreamClass,
    key: &str,
    range_header: Option<&str>,
) -> io::Result<()> {
    let wanted = parse_range(range_header.unwrap_or(""));
    if let Some((start, _)) = wanted
        && start > 0
    {
        let reg = lock(inner).registrations.get(key).cloned();
        let Some(reg) = reg else {
            return write_head(client, "404 Not Found", &[]);
        };
        return forward_range(client, &reg.upstream_url, range_header);
    }
    let Some(fill) = cache.ensure_fill(key) else {
        // It completed between the lookup and here: serve it from disk.
        return serve_from_disk(client, root, class, key, range_header);
    };
    // Wait for the upstream response headers (or a failure).
    let (total, content_type, failed) = {
        let mut progress = lock(&fill.progress);
        let mut waited = Duration::ZERO;
        while !progress.started && waited < Duration::from_secs(30) {
            let (next, timeout) = fill
                .ready
                .wait_timeout(progress, Duration::from_millis(200))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            progress = next;
            if timeout.timed_out() {
                waited += Duration::from_millis(200);
            }
        }
        (progress.total, progress.content_type.clone(), progress.failed)
    };
    if failed {
        return write_head(client, "502 Bad Gateway", &[]);
    }

    let path = cache_path(root, class, key);
    let mut file = open_with_retry(&path)?;
    let start = wanted.map(|(start, _)| start).unwrap_or(0);
    let end = match total {
        Some(total) => match wanted_range(wanted, total) {
            Some((_, end)) => Some(end),
            None => {
                return write_head(
                    client,
                    "416 Range Not Satisfiable",
                    &[("Content-Range", format!("bytes */{total}"))],
                );
            }
        },
        None => None,
    };
    let partial = wanted.is_some();
    let mut headers = vec![
        (
            "Content-Type",
            content_type.unwrap_or_else(|| "application/octet-stream".to_owned()),
        ),
        ("Accept-Ranges", "bytes".to_owned()),
    ];
    match (total, end) {
        (Some(total), Some(end)) => {
            let length = end.saturating_sub(start) + 1;
            headers.push(("Content-Length", length.to_string()));
            if partial {
                headers.push(("Content-Range", format!("bytes {start}-{end}/{total}")));
            }
        }
        _ => headers.push(("Transfer-Encoding", "chunked".to_owned())),
    }
    let status = if partial { "206 Partial Content" } else { "200 OK" };
    write_head(client, status, &headers)?;

    file.seek(SeekFrom::Start(start))?;
    match end {
        Some(end) => stream_growing(client, &mut file, &fill, end, false),
        None => stream_growing(client, &mut file, &fill, 0, true),
    }
}

/// Copy from a file another thread is still appending to, waiting for more
/// data until `end` is reached (or, with `chunked`, until the fill is done).
fn stream_growing(
    client: &mut TcpStream,
    file: &mut File,
    fill: &Arc<Fill>,
    end: u64,
    chunked: bool,
) -> io::Result<()> {
    let mut position: u64 = file.stream_position()?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read > 0 {
            if chunked {
                write_chunk(client, &buffer[..read])?;
            } else {
                let remaining = end.saturating_sub(position) + 1;
                let usable = read.min(remaining as usize);
                client.write_all(&buffer[..usable])?;
                position += usable as u64;
                if position > end {
                    break;
                }
                continue;
            }
            position += read as u64;
            continue;
        }
        let mut progress = lock(&fill.progress);
        if progress.failed || (progress.complete && progress.filled <= position) {
            break;
        }
        let (next, _) = fill
            .ready
            .wait_timeout(progress, Duration::from_millis(100))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress = next;
        let done = progress.complete && progress.filled <= position;
        let failed = progress.failed;
        drop(progress);
        if done || failed {
            break;
        }
    }
    if chunked {
        client.write_all(b"0\r\n\r\n")?;
    }
    Ok(())
}

fn write_chunk(client: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    client.write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
    client.write_all(data)?;
    client.write_all(b"\r\n")
}

/// Open the cache file, waiting briefly for the fill thread to create it
/// (the fill signals its headers before it opens the file).
fn open_with_retry(path: &Path) -> io::Result<File> {
    for _ in 0..50 {
        if let Ok(file) = File::open(path) {
            return Ok(file);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    File::open(path)
}

fn copy_exact(file: &mut File, client: &mut TcpStream, mut length: u64) -> io::Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    while length > 0 {
        let want = buffer.len().min(length as usize);
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        client.write_all(&buffer[..read])?;
        length -= read as u64;
    }
    Ok(())
}

/// Forward a range request upstream when the cache cannot serve it yet.
fn forward_range(
    client: &mut TcpStream,
    upstream_url: &str,
    range_header: Option<&str>,
) -> io::Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .build();
    let mut request = agent.get(upstream_url);
    if let Some(range) = range_header {
        request = request.set("Range", range);
    }
    let response = request
        .call()
        .map_err(|err| io::Error::other(err.to_string()))?;
    let status = response.status();
    let mut headers: Vec<(&str, String)> = Vec::new();
    for name in ["Content-Type", "Content-Length", "Content-Range", "Accept-Ranges"] {
        if let Some(value) = response.header(name) {
            headers.push((name, value.to_owned()));
        }
    }
    let status_line = match status {
        200 => "200 OK".to_owned(),
        206 => "206 Partial Content".to_owned(),
        other => format!("{other} Upstream"),
    };
    write_head(client, &status_line, &headers)?;
    let mut reader = response.into_reader();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        client.write_all(&buffer[..read])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("s2udio-buffer-{name}-{}", std::process::id()))
    }

    fn empty_inner() -> Inner {
        Inner {
            registrations: HashMap::new(),
            entries: HashMap::new(),
            fills: HashMap::new(),
            active: HashMap::new(),
        }
    }

    #[test]
    fn key_is_stable_and_distinct_per_link() {
        assert_eq!(key_for("https://youtu.be/abc"), key_for("https://youtu.be/abc"));
        assert_ne!(key_for("https://youtu.be/abc"), key_for("https://youtu.be/abd"));
        assert_eq!(key_for("https://youtu.be/abc").len(), 16);
    }

    #[test]
    fn parse_range_handles_the_shapes_mpd_and_mpv_send() {
        assert_eq!(parse_range("bytes=0-"), Some((0, None)));
        assert_eq!(parse_range("bytes=100-"), Some((100, None)));
        assert_eq!(parse_range("bytes=100-200"), Some((100, Some(200))));
        assert_eq!(parse_range("bytes=-500"), None);
        assert_eq!(parse_range("items=0-1"), None);
    }

    #[test]
    fn wanted_range_clamps_to_the_total() {
        assert_eq!(wanted_range(None, 1000), Some((0, 999)));
        assert_eq!(wanted_range(Some((0, None)), 1000), Some((0, 999)));
        assert_eq!(wanted_range(Some((100, Some(200))), 1000), Some((100, 200)));
        assert_eq!(wanted_range(Some((100, None)), 1000), Some((100, 999)));
        // Past the end: unsatisfiable.
        assert_eq!(wanted_range(Some((1000, None)), 1000), None);
    }

    #[test]
    fn budget_eviction_drops_the_least_recently_used_entry() {
        let root = temp_root("lru");
        let _ = std::fs::remove_dir_all(&root);
        let mut inner = empty_inner();
        for (key, last_used) in [("old", 10u64), ("new", 20u64)] {
            let path = cache_path(&root, StreamClass::Audio, key);
            std::fs::create_dir_all(path.parent().unwrap_or(root.as_path())).expect("tmp dir");
            std::fs::write(&path, b"x").expect("tmp file");
            inner.entries.insert(
                key.to_owned(),
                Entry {
                    class: StreamClass::Audio,
                    original_url: key.to_owned(),
                    size: AUDIO_BUDGET_BYTES,
                    total: Some(AUDIO_BUDGET_BYTES),
                    complete: true,
                    content_type: None,
                    last_used,
                    failed: false,
                },
            );
        }
        enforce_budget(&mut inner, StreamClass::Audio, &root);
        assert!(!inner.entries.contains_key("old"), "the LRU entry is evicted");
        assert!(inner.entries.contains_key("new"), "the most recent entry stays");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_active_entry_is_never_evicted() {
        let root = temp_root("active");
        let _ = std::fs::remove_dir_all(&root);
        let mut inner = empty_inner();
        inner.entries.insert(
            "live".to_owned(),
            Entry {
                class: StreamClass::Video,
                original_url: "live".to_owned(),
                size: VIDEO_BUDGET_BYTES,
                total: Some(VIDEO_BUDGET_BYTES),
                complete: true,
                content_type: None,
                last_used: 1,
                failed: false,
            },
        );
        inner.active.insert("live".to_owned(), 1);
        enforce_budget(&mut inner, StreamClass::Video, &root);
        assert!(inner.entries.contains_key("live"), "a live reader holds its entry");
    }

    #[test]
    fn audio_and_video_budgets_are_separate() {
        let root = temp_root("classes");
        let _ = std::fs::remove_dir_all(&root);
        let mut inner = empty_inner();
        for (key, class) in [("a", StreamClass::Audio), ("v", StreamClass::Video)] {
            inner.entries.insert(
                key.to_owned(),
                Entry {
                    class,
                    original_url: key.to_owned(),
                    size: class.budget(),
                    total: Some(class.budget()),
                    complete: true,
                    content_type: None,
                    last_used: 1,
                    failed: false,
                },
            );
        }
        // Enforcing the audio budget must not touch the video entry.
        enforce_budget(&mut inner, StreamClass::Audio, &root);
        assert!(inner.entries.contains_key("v"), "the video budget is independent");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Round 107 regression: an **incomplete** entry with a live
    /// registration is filling, not stale. Treating it as re-resolvable made
    /// the look ahead re-resolve and swap the same row forever.
    ///
    /// Round 109: a **live reader** streaming the key is not stale either —
    /// otherwise a failed fill made the current-song hook `ReplaceAndPlay`
    /// the playing row and restart the track from the beginning.
    #[test]
    fn an_incomplete_entry_with_a_live_registration_is_not_stale() {
        let filling = Entry {
            class: StreamClass::Audio,
            original_url: "https://youtu.be/x".to_owned(),
            size: 1024,
            total: Some(1_000_000),
            complete: false,
            content_type: None,
            last_used: 1,
            failed: false,
        };
        assert!(
            !entry_needs_refresh(Some(&filling), true, false),
            "a live fill must not be re-resolved"
        );
        // No registration (a restart, or a restored queue): re-resolve.
        assert!(entry_needs_refresh(Some(&filling), false, false));
        // A failed fill with nobody reading it: re-resolve.
        let failed = Entry { failed: true, ..filling.clone() };
        assert!(entry_needs_refresh(Some(&failed), true, false));
        // A failed fill a client is streaming right now: the proxy is serving
        // the row, so re-resolving it would restart the playing track.
        assert!(!entry_needs_refresh(Some(&failed), true, true));
        assert!(!entry_needs_refresh(Some(&failed), false, true));
        // A complete entry never needs the upstream, registration or not.
        let complete = Entry { complete: true, ..filling.clone() };
        assert!(!entry_needs_refresh(Some(&complete), false, false));
        // An unknown key: nothing to serve from.
        assert!(entry_needs_refresh(None, false, false));
    }

    #[test]
    fn content_range_total_reads_the_full_length() {
        assert_eq!(content_range_total("bytes 0-4757660/4757661"), Some(4757661));
        assert_eq!(content_range_total("bytes 100-199/500"), Some(500));
        assert_eq!(content_range_total("bytes 0-9/*"), None);
        assert_eq!(content_range_total("bytes 0-9"), None);
        assert_eq!(content_range_total(""), None);
    }

    /// The row URLs embed the port, so it must survive a restart.
    #[test]
    fn the_proxy_port_is_persisted_for_the_next_run() {
        let root = temp_root("port");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("tmp dir");
        assert_eq!(persisted_port(&root), None);
        remember_port(&root, 41234);
        assert_eq!(persisted_port(&root), Some(41234));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_buffered_url_on_another_port_is_rewritten() {
        let url = "http://127.0.0.1:45303/s2u/audio/abc123";
        assert_eq!(url_port(url), Some(45303));
        assert_eq!(
            url_with_port(url, 33539).as_deref(),
            Some("http://127.0.0.1:33539/s2u/audio/abc123")
        );
        // Already on the port, or not a buffered URL: nothing to rewrite.
        assert_eq!(url_with_port(url, 45303), None);
        assert_eq!(url_with_port("https://rr1.googlevideo.com/vb", 45303), None);
        assert_eq!(url_with_port("http://127.0.0.1:45303/other/x", 33539), None);
        assert_eq!(url_port("http://127.0.0.1:45303/other/x"), None);
    }

    /// A connected client/server pair for exercising the response writer.
    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (server, client)
    }

    /// Serve one known body over a throwaway HTTP listener, counting the
    /// requests it sees.
    fn fake_upstream(body: Vec<u8>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                count.fetch_add(1, Ordering::SeqCst);
                let mut head = [0u8; 1024];
                let _ = stream.read(&mut head);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}/stream"), hits)
    }

    /// A throwaway upstream that answers with `206` + `Content-Range`, records
    /// every request head, and cuts the first `resets` responses short
    /// mid-body — the shape of a CDN resetting a long transfer.
    fn fake_ranged_upstream(
        body: Vec<u8>,
        resets: usize,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::clone(&hits);
        let recorded = Arc::clone(&heads);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let index = count.fetch_add(1, Ordering::SeqCst);
                let mut head = [0u8; 2048];
                let read = stream.read(&mut head).unwrap_or(0);
                lock(&recorded).push(String::from_utf8_lossy(&head[..read]).into_owned());
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/webm\r\nAccept-Ranges: bytes\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len().saturating_sub(1),
                    body.len(),
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                if index < resets {
                    let _ = stream.write_all(&body[..body.len() / 2]);
                } else {
                    let _ = stream.write_all(&body);
                }
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        (format!("http://{addr}/stream"), hits, heads)
    }

    /// Drive a fill to a terminal state: (complete, failed, error).
    fn wait_for_fill(fill: &Arc<Fill>) -> (bool, bool, Option<String>) {
        let mut progress = lock(&fill.progress);
        let mut waited = Duration::ZERO;
        while !progress.complete && !progress.failed && waited < Duration::from_secs(20) {
            let (next, timeout) = fill
                .ready
                .wait_timeout(progress, Duration::from_millis(100))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            progress = next;
            if timeout.timed_out() {
                waited += Duration::from_millis(100);
            }
        }
        (progress.complete, progress.failed, progress.error.clone())
    }

    fn streaming_cache(root: &Path) -> Arc<StreamCache> {
        Arc::new(StreamCache {
            port: None,
            root: root.to_path_buf(),
            inner: Arc::new(Mutex::new(empty_inner())),
        })
    }

    /// Round 109: the fill must open the upstream with a range. A plain GET is
    /// throttled to ~34 KB/s and reset after ~2 min, which stalled the row
    /// mid-playback and turned into a restart.
    #[test]
    fn a_fill_asks_for_an_open_range_and_caches_the_whole_body() {
        use std::sync::atomic::Ordering;
        let root = temp_root("ranged");
        let _ = std::fs::remove_dir_all(&root);
        let body: Vec<u8> = (0..=255u8).collect();
        let (upstream, hits, heads) = fake_ranged_upstream(body.clone(), 0);
        let cache = streaming_cache(&root);
        let original = "https://example.test/watch?v=ranged-range";
        cache.register(StreamClass::Audio, original, &upstream);
        let key = key_for(original);
        let fill = cache.ensure_fill(&key).expect("a fill starts");
        let (complete, failed, error) = wait_for_fill(&fill);

        assert!(complete && !failed, "the fill completed (error: {error:?})");
        let head = lock(&heads).first().cloned().unwrap_or_default();
        assert!(
            head.to_ascii_lowercase().contains("range: bytes=0-"),
            "the fill asks for the players' seek shape; got: {head}"
        );
        let inner = lock(&cache.inner);
        let entry = inner.entries.get(&key).expect("the entry is indexed");
        assert_eq!(entry.size, body.len() as u64);
        // The total comes from Content-Range: a 206's Content-Length is only
        // the remaining bytes.
        assert_eq!(entry.total, Some(body.len() as u64));
        assert_eq!(entry.content_type.as_deref(), Some("audio/webm"));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one upstream fetch");
        drop(inner);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Round 109: a reset partway through the body is retried, not turned into
    /// a failed entry that restarts the playing row.
    #[test]
    fn an_upstream_reset_mid_body_is_retried_and_the_entry_completes() {
        use std::sync::atomic::Ordering;
        let root = temp_root("retry");
        let _ = std::fs::remove_dir_all(&root);
        let body: Vec<u8> = (0..=255u8).collect();
        let (upstream, hits, _) = fake_ranged_upstream(body.clone(), 1);
        let cache = streaming_cache(&root);
        let original = "https://example.test/watch?v=ranged-retry";
        cache.register(StreamClass::Audio, original, &upstream);
        let key = key_for(original);
        let fill = cache.ensure_fill(&key).expect("a fill starts");
        let (complete, failed, error) = wait_for_fill(&fill);

        assert!(complete && !failed, "the retry completed (error: {error:?})");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "the cut transfer was retried");
        let inner = lock(&cache.inner);
        let entry = inner.entries.get(&key).expect("the entry is indexed");
        assert!(entry.complete, "the entry is complete");
        assert!(!entry.failed, "a retried reset does not fail the entry");
        assert_eq!(entry.size, body.len() as u64, "the whole body is cached");
        drop(inner);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A body the CDN cuts short every time must never be indexed as complete:
    /// a truncated cache file would play as a broken stream.
    #[test]
    fn a_body_cut_short_on_every_attempt_never_completes() {
        use std::sync::atomic::Ordering;
        let root = temp_root("short");
        let _ = std::fs::remove_dir_all(&root);
        let body: Vec<u8> = (0..=255u8).collect();
        let (upstream, hits, _) = fake_ranged_upstream(body.clone(), FILL_ATTEMPTS as usize);
        let cache = streaming_cache(&root);
        let original = "https://example.test/watch?v=ranged-short";
        cache.register(StreamClass::Audio, original, &upstream);
        let key = key_for(original);
        let fill = cache.ensure_fill(&key).expect("a fill starts");
        let (complete, failed, error) = wait_for_fill(&fill);

        assert!(!complete, "a short body is not a complete entry");
        assert!(failed, "every attempt failed");
        assert!(error.is_some(), "the failure carries a reason: {error:?}");
        assert_eq!(hits.load(Ordering::SeqCst), FILL_ATTEMPTS as usize, "every attempt was made");
        let inner = lock(&cache.inner);
        let entry = inner.entries.get(&key).expect("the entry is indexed");
        assert!(!entry.complete);
        assert!(entry.failed, "the entry is marked for a re-resolve");
        drop(inner);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole point of the buffer: a fill caches the upstream body once,
    /// and a later range request is served from disk without touching the
    /// upstream again. (An upstream that ignores the fill's `Range` — a plain
    /// `200` here — still fills correctly.)
    #[test]
    fn a_fill_caches_the_upstream_body_and_replays_come_from_disk() {
        let root = temp_root("fill");
        let _ = std::fs::remove_dir_all(&root);
        let body: Vec<u8> = (0..=255u8).collect();
        let (upstream, hits) = fake_upstream(body.clone());
        let cache = Arc::new(StreamCache {
            port: None,
            root: root.clone(),
            inner: Arc::new(Mutex::new(empty_inner())),
        });
        let original = "https://example.test/watch?v=fill-unique";
        cache.register(StreamClass::Audio, original, &upstream);
        let key = key_for(original);
        let fill = cache.ensure_fill(&key).expect("a fill starts");
        {
            let mut progress = lock(&fill.progress);
            let mut waited = Duration::ZERO;
            while !progress.complete && !progress.failed && waited < Duration::from_secs(10) {
                let (next, timeout) = fill
                    .ready
                    .wait_timeout(progress, Duration::from_millis(100))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                progress = next;
                if timeout.timed_out() {
                    waited += Duration::from_millis(100);
                }
            }
            assert!(
                progress.complete,
                "the fill completed (error: {:?})",
                progress.error
            );
        }
        assert!(cache.is_complete(&key));
        {
            let inner = lock(&cache.inner);
            let entry = inner.entries.get(&key).expect("the entry is indexed");
            assert_eq!(entry.size, 256);
            assert!(entry.complete);
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the fill fetched the upstream exactly once"
        );

        // A replay range is served from the cache file, not the upstream.
        let (mut server, mut client) = connected_pair();
        serve_from_disk(&mut server, &root, StreamClass::Audio, &key, Some("bytes=10-19"))
            .expect("served from disk");
        // The client reads raw bytes, not HTTP framing: close the server side
        // so `read_to_end` sees EOF.
        let _ = server.shutdown(std::net::Shutdown::Both);
        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read response");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 206 Partial Content"), "got: {text}");
        let marker: Vec<u8> = (10..=19u8).collect();
        assert!(
            response.windows(marker.len()).any(|window| window == marker.as_slice()),
            "the requested range bytes are served"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the replay did not touch the upstream"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

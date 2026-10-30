//! The detached web-stream caretaker.
//!
//! While no TUI is attached the daemon keeps the queue's look-ahead window
//! resolved, exactly where the TUI's own look ahead
//! (`ui/modals/paste/warm.rs`) leaves off: MPD's `nextsongid` first, then the
//! rows after the current one, at most [`LOOK_AHEAD_WINDOW`], audio intents
//! only. A row is swapped **in place** (`delete_id` + `add_id` at the same
//! position) and never played — a warm-up that played would cut the current
//! track short. Resolved info is persisted into `yt-info.json` so the TUI
//! shows titles/artwork when it next attaches.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};

use super::server::State;
use crate::{
    core::stream_cache::{self, StreamClass},
    mpd::{
        QueuePosition,
        client::Client,
        commands::{Song, Status},
        mpd_client::MpdClient,
    },
    shared::{
        paths::s2udio_cache_path,
        ytdlp::{self, YtStreamInfo, tagged_stream_entry},
    },
};

/// How many queue rows ahead of the current song are kept resolved. Same
/// window as the TUI's look ahead (and the same reasoning: one row is behind
/// a ~2.5 s resolve).
const LOOK_AHEAD_WINDOW: usize = 4;
/// A cached resolve older than this is not reused (`ui/modals/paste` uses
/// the same 30-minute rule).
const CACHED_MAX_AGE_SECS: u64 = 30 * 60;
/// After a failed resolve, do not retry the same link for this long: a dead
/// video would otherwise cost one yt-dlp run per poll.
const FAILURE_BACKOFF: Duration = Duration::from_secs(120);

/// Per-daemon-run bookkeeping. Lives on the watcher thread.
#[derive(Default)]
pub(crate) struct WarmState {
    /// Links already resolved (or swapped) in this run.
    warmed: HashSet<String>,
    /// Link -> when its last resolve failed.
    failures: HashMap<String, Instant>,
}

/// Warm the queue rows around the current song. Called from the MPD poll
/// while no TUI is attached.
pub(crate) fn warm_queue(
    client: &mut Client<'static>,
    status: &Status,
    queue: &[Song],
    cache_dir: Option<&Path>,
    state: &Arc<State>,
    warm: &mut WarmState,
) {
    warm.failures.retain(|_, at| at.elapsed() < FAILURE_BACKOFF);

    // Forget warmed links whose rows left the queue, so a re-added link is
    // resolved again instead of being skipped forever.
    let present: HashSet<String> = queue
        .iter()
        .filter_map(|song| row_link(cache_dir, &song.file))
        .collect();
    warm.warmed.retain(|link| present.contains(link));

    let current_idx = status
        .songid
        .and_then(|id| queue.iter().position(|song| song.id == id))
        .or(status.song);
    let mut candidates: Vec<&Song> = Vec::with_capacity(LOOK_AHEAD_WINDOW);
    // MPD's own pick comes first: under `random` it is the only row that is
    // actually next (the TUI's look ahead reasons the same way).
    if let Some(next) = status
        .nextsongid
        .and_then(|id| queue.iter().find(|song| song.id == id))
    {
        candidates.push(next);
    }
    if let Some(idx) = current_idx {
        for song in queue.iter().skip(idx + 1) {
            if candidates.len() >= LOOK_AHEAD_WINDOW {
                break;
            }
            if !candidates.iter().any(|candidate| candidate.id == song.id) {
                candidates.push(song);
            }
        }
    }

    for song in candidates {
        if state.stopping() || state.attached() > 0 {
            return;
        }
        let Some(link) = row_link(cache_dir, &song.file) else { continue };
        if warm.warmed.contains(&link) || warm.failures.contains_key(&link) {
            continue;
        }
        match resolve_and_swap(client, song.id, &link, cache_dir) {
            Ok(()) => {
                log::info!(song_id = song.id, link = link.as_str(); "daemon: warmed a queue row");
                state.note_warmed();
                warm.warmed.insert(link);
            }
            Err(err) => {
                log::warn!(
                    song_id = song.id,
                    link = link.as_str(),
                    error: ? = err;
                    "daemon: could not warm the row; backing off"
                );
                state.set_error(format!("could not resolve {link}: {err}"));
                warm.failures.insert(link, Instant::now());
            }
        }
    }
}

/// The original link a queue row needs resolved, when it needs one:
/// a tagged audio link, or a buffered row whose URL expired / points at a
/// previous run's proxy port. `None` for local files, radio, video rows and
/// healthy buffered rows.
fn row_link(cache_dir: Option<&Path>, file: &str) -> Option<String> {
    if let Some((link, intent)) = tagged_stream_entry(file) {
        return intent.is_audio().then_some(link);
    }
    if stream_cache::is_buffered_url(file)
        && stream_cache::needs_upstream_refresh(file)
        && let Some(original) = stream_cache::original_link_for_url(cache_dir, file)
    {
        return Some(original);
    }
    None
}

/// Resolve `link`, register the result with the proxy, and swap the row at
/// `song_id` for the buffered URL without starting playback.
fn resolve_and_swap(
    client: &mut Client<'static>,
    song_id: u32,
    link: &str,
    cache_dir: Option<&Path>,
) -> Result<()> {
    let info = match fresh_cached(cache_dir, link) {
        Some(info) => info,
        None => {
            let urls = [link.to_owned()];
            let (mut infos, failures) = ytdlp::resolve_audio_urls(&urls);
            infos.pop().ok_or_else(|| {
                anyhow!(
                    "{}",
                    failures
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| "yt-dlp returned nothing".to_owned())
                )
            })?
        }
    };
    let mut info = info;
    if info.resolved_at.is_none() {
        info.resolved_at = Some(epoch_secs());
    }
    let source = if info.original_url.is_empty() { link.to_owned() } else { info.original_url.clone() };
    let upstream = info.url.clone();
    let url = stream_cache::proxy_url(cache_dir, StreamClass::Audio, &source, &upstream);
    // Fill now so the transition is gap-free (the TUI does the same for its
    // look ahead).
    stream_cache::preload(cache_dir, StreamClass::Audio, &source, &upstream);
    save_yt_info(cache_dir, link, &info)?;

    // Re-read: the user may have edited the queue while yt-dlp ran.
    let songs = client.playlist_info()?.unwrap_or_default();
    let Some(position) = songs.iter().position(|song| song.id == song_id) else {
        log::debug!(song_id; "daemon: the warmed row disappeared before the swap");
        return Ok(());
    };
    let _ = client.delete_id(song_id);
    let new_id = client.add_id(&url, Some(QueuePosition::Absolute(position)))?;
    log::debug!(song_id, new_id, position; "daemon: swapped the row for its stream");
    Ok(())
}

/// A cached resolve for `link` while it is fresh and its signed URL has not
/// expired. Same rule as the TUI's `fresh_cached_stream_info`.
fn fresh_cached(cache_dir: Option<&Path>, link: &str) -> Option<YtStreamInfo> {
    let data = std::fs::read(yt_cache_path(cache_dir)).ok()?;
    let cache: HashMap<String, YtStreamInfo> = serde_json::from_slice(&data).ok()?;
    let info = cache
        .get(link)
        .cloned()
        .or_else(|| cache.values().find(|info| info.original_url == link).cloned())?;
    if info.url.is_empty() {
        return None;
    }
    let resolved_at = info.resolved_at?;
    let now = epoch_secs();
    if now.saturating_sub(resolved_at) > CACHED_MAX_AGE_SECS {
        log::debug!(link; "daemon: cached resolve is too old to reuse");
        return None;
    }
    if let Some(expire) = stream_url_expire(&info.url)
        && expire <= i64::try_from(now).unwrap_or(i64::MAX)
    {
        log::debug!(link; "daemon: cached resolve's URL expired; resolving again");
        return None;
    }
    Some(info)
}

/// Merge one resolved entry into `yt-info.json` (the TUI's cache, so the next
/// attach shows the title/artwork). Read-modify-write with an atomic rename;
/// the TUI's own writer uses the same file and shape.
fn save_yt_info(cache_dir: Option<&Path>, link: &str, info: &YtStreamInfo) -> Result<()> {
    let path = yt_cache_path(cache_dir);
    let mut cache: HashMap<String, YtStreamInfo> = std::fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    cache.insert(link.to_owned(), info.clone());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&cache)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn yt_cache_path(cache_dir: Option<&Path>) -> PathBuf {
    s2udio_cache_path(cache_dir, "yt-info.json")
}

/// The `expire=` epoch of a signed stream URL, when it carries one.
fn stream_url_expire(url: &str) -> Option<i64> {
    let (_, rest) = url.split_once("expire=")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn epoch_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("s2u-daemon-caretaker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn info(url: &str, resolved_at: Option<u64>) -> YtStreamInfo {
        YtStreamInfo {
            url: url.to_owned(),
            original_url: "url".to_owned(),
            resolved_at,
            ..YtStreamInfo::default()
        }
    }

    #[test]
    fn a_expired_signed_url_is_not_reused() {
        assert_eq!(stream_url_expire("https://cdn.invalid/a?expire=1700000000&x=1"), Some(1_700_000_000));
        assert_eq!(stream_url_expire("https://cdn.invalid/a?x=1"), None);
        assert_eq!(stream_url_expire("https://cdn.invalid/a?expire=abc"), None);
    }

    #[test]
    fn fresh_cache_entries_are_reused_but_stale_or_expired_ones_are_not() {
        let dir = scratch("fresh");
        let link = "https://youtube.invalid/watch?v=abc";
        let now = epoch_secs();
        let mut cache = HashMap::new();
        cache.insert(link.to_owned(), info("https://cdn.invalid/ok?sig=1", Some(now)));
        std::fs::write(dir.join("yt-info.json"), serde_json::to_vec(&cache).unwrap()).unwrap();

        let got = fresh_cached(Some(&dir), link).expect("fresh entry is reused");
        assert_eq!(got.url, "https://cdn.invalid/ok?sig=1");

        // A signed URL that already expired must be resolved again.
        cache.insert(link.to_owned(), info("https://cdn.invalid/old?expire=100&x=1", Some(now)));
        std::fs::write(dir.join("yt-info.json"), serde_json::to_vec(&cache).unwrap()).unwrap();
        assert!(fresh_cached(Some(&dir), link).is_none(), "expired URL must not be reused");

        // An old resolve must be resolved again.
        cache.insert(link.to_owned(), info("https://cdn.invalid/ok?sig=1", Some(now - CACHED_MAX_AGE_SECS - 5)));
        std::fs::write(dir.join("yt-info.json"), serde_json::to_vec(&cache).unwrap()).unwrap();
        assert!(fresh_cached(Some(&dir), link).is_none(), "stale resolve must not be reused");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_merges_into_the_tui_cache_instead_of_replacing_it() {
        let dir = scratch("merge");
        let now = epoch_secs();
        let mut cache = HashMap::new();
        let other = "https://youtube.invalid/watch?v=other";
        cache.insert(other.to_owned(), info("https://cdn.invalid/other", Some(now)));
        std::fs::write(dir.join("yt-info.json"), serde_json::to_vec(&cache).unwrap()).unwrap();

        let link = "https://youtube.invalid/watch?v=abc";
        save_yt_info(Some(&dir), link, &info("https://cdn.invalid/abc", Some(now))).expect("save");

        let saved: HashMap<String, YtStreamInfo> =
            serde_json::from_slice(&std::fs::read(dir.join("yt-info.json")).unwrap()).unwrap();
        assert_eq!(saved.len(), 2, "the other entry survives");
        assert!(saved.contains_key(link), "the new entry is written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_tagged_audio_rows_and_stale_buffered_rows_need_a_resolve() {
        assert_eq!(
            row_link(None, "https://youtube.invalid/watch?v=abc#s2u-audio"),
            Some("https://youtube.invalid/watch?v=abc".to_owned()),
            "a tagged audio row carries its link"
        );
        assert_eq!(
            row_link(None, "https://youtube.invalid/watch?v=abc#s2u-video"),
            None,
            "video intents belong to mpv, not the MPD caretaker"
        );
        assert_eq!(row_link(None, "/music/track.flac"), None, "local files never resolve");
        assert_eq!(row_link(None, "https://radio.invalid/stream"), None, "plain streams never resolve");
        // A buffered row whose proxy is not running has no original-link
        // registry to consult, so it is left for the TUI's error recovery.
        assert!(row_link(None, "http://127.0.0.1:47123/s2u/audio/abcdef0123456789").is_none());
    }
}

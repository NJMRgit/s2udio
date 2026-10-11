//! Local resume positions for every video s2udio plays in mpv.
//!
//! Jellyfin keeps a resume position server-side, but local files, torrent
//! streams and resolved YouTube links have no server to ask: their position
//! is remembered here, in `<cache_dir>/video-resume.json`, so the Continue /
//! Start over prompt can offer it for all video content. The store is keyed
//! by [`entry_key`] (item id / canonical link / URL / path), small and
//! written synchronously, like the existing `yt-info.json` cache.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use serde::{Deserialize, Serialize};
use crate::core::mpv::MpvPlaylistEntry;
/// One remembered position: `position` in seconds and when it was written
/// (`updated_at`, unix seconds — informational, so a stale entry can be
/// spotted by hand).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPosition {
    position: f64,
    #[serde(default)]
    updated_at: u64,
}
/// The store's file path: `<cache_dir>/video-resume.json` (the caller's
/// cache dir, else s2udio's default `~/.cache/s2udio`). `None` when no cache
/// dir is known at all, which makes load/store no-ops.
fn cache_path(cache_dir: Option<&Path>) -> Option<PathBuf> {
    cache_dir
        .map(Path::to_path_buf)
        .or_else(crate::shared::paths::s2udio_cache_dir)
        .map(|dir| dir.join("video-resume.json"))
}
/// The identity a video's resume position is remembered under:
///
/// - `jf:<item_id>` for a Jellyfin stream (the server's own key);
/// - `link:<original_url>` for an entry resolved from a canonical
///   YouTube/Soundcloud link (the resolved stream URL expires and would
///   otherwise look like a different video on every play);
/// - `url:<url>` for any other real http(s) URL (radio-style streams);
/// - `file:<path>` for a local path;
/// - `None` for an empty URL (nothing to key on).
pub fn entry_key(entry: &MpvPlaylistEntry) -> Option<String> {
    let url = entry.url.trim();
    if url.is_empty() {
        return None;
    }
    if let Some(id) = crate::jellyfin::item_id_from_url(url) {
        return Some(format!("jf:{id}"));
    }
    if let Some(original) = entry
        .original_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        return Some(format!("link:{original}"));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(format!("url:{url}"));
    }
    Some(format!("file:{url}"))
}
/// The seconds remembered for `key`, when any.
pub fn load(cache_dir: Option<&Path>, key: &str) -> Option<f64> {
    let path = cache_path(cache_dir)?;
    let data = std::fs::read(path).ok()?;
    let entries: BTreeMap<String, StoredPosition> = serde_json::from_slice(&data).ok()?;
    entries.get(key).map(|entry| entry.position)
}
/// Remember `seconds` for `key`. A non-positive `seconds` REMOVES the key:
/// that is what the prompt's "Start over" stores, so the video plays from
/// the beginning and is not offered again.
pub fn store(cache_dir: Option<&Path>, key: &str, seconds: f64) {
    let Some(path) = cache_path(cache_dir) else {
        return;
    };
    let mut entries: BTreeMap<String, StoredPosition> = std::fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    if seconds <= 0.0 {
        if entries.remove(key).is_none() {
            // Nothing to clear: leave the file untouched.
            return;
        }
    } else {
        let updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        entries.insert(key.to_owned(), StoredPosition { position: seconds, updated_at });
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_vec(&entries) {
        let _ = std::fs::write(path, data);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A throwaway cache dir under the system temp dir (never the user's
    /// real `~/.cache/s2udio`).
    fn temp_cache_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "s2u-video-resume-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_of_a_jellyfin_stream_is_its_item_id() {
        let id = "0123456789abcdef0123456789abcdef";
        let entry = MpvPlaylistEntry::new(
            "Episode",
            format!("http://jf.local/Videos/{id}/stream?static=true"),
            None,
        );
        assert_eq!(entry_key(&entry), Some(format!("jf:{id}")));
    }

    #[test]
    fn key_of_a_resolved_link_prefers_the_canonical_url() {
        let mut entry = MpvPlaylistEntry::new(
            "A video",
            "https://rr1.googlevideo.com/videoplayback?expire=1",
            None,
        );
        entry.original_url = Some("https://www.youtube.com/watch?v=abc".to_owned());
        assert_eq!(
            entry_key(&entry).as_deref(),
            Some("link:https://www.youtube.com/watch?v=abc")
        );
    }

    #[test]
    fn key_of_a_plain_http_url_is_the_url() {
        let entry = MpvPlaylistEntry::new(
            "Radio",
            "https://example.test/live.m3u8",
            None,
        );
        assert_eq!(
            entry_key(&entry).as_deref(),
            Some("url:https://example.test/live.m3u8")
        );
    }

    #[test]
    fn key_of_a_local_path_is_the_path() {
        let entry = MpvPlaylistEntry::new("Movie", "/media/films/a.mkv", None);
        assert_eq!(entry_key(&entry).as_deref(), Some("file:/media/films/a.mkv"));
    }

    #[test]
    fn key_of_an_empty_url_is_none() {
        let entry = MpvPlaylistEntry::new("Nothing", "   ", None);
        assert_eq!(entry_key(&entry), None);
    }

    #[test]
    fn store_and_load_round_trip_through_the_cache_file() {
        let dir = temp_cache_dir("round-trip");
        assert_eq!(load(Some(&dir), "file:/a.mkv"), None);
        store(Some(&dir), "file:/a.mkv", 123.5);
        store(Some(&dir), "url:https://x.test/y", 7.0);
        assert_eq!(load(Some(&dir), "file:/a.mkv"), Some(123.5));
        assert_eq!(load(Some(&dir), "url:https://x.test/y"), Some(7.0));
        assert!(dir.join("video-resume.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storing_zero_removes_the_entry() {
        let dir = temp_cache_dir("start-over");
        store(Some(&dir), "file:/a.mkv", 42.0);
        assert_eq!(load(Some(&dir), "file:/a.mkv"), Some(42.0));
        store(Some(&dir), "file:/a.mkv", 0.0);
        assert_eq!(load(Some(&dir), "file:/a.mkv"), None);
        // The other entries must survive the removal.
        store(Some(&dir), "file:/b.mkv", 9.0);
        store(Some(&dir), "file:/a.mkv", 0.0);
        assert_eq!(load(Some(&dir), "file:/b.mkv"), Some(9.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

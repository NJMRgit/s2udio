use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::Result;
use itertools::Itertools;
use rustix::path::Arg;
use walkdir::WalkDir;

use crate::shared::ytdlp::error::YtDlpParseError;

#[derive(Debug, Clone)]
pub struct YtDlpItem {
    /// id of the video/audio, to be used in the url
    pub id: String,
    /// filename of the video/audio, will be used to cache the file
    pub filename: String,
    pub kind: YtDlpHost,
}

pub struct YtDlpPlaylist {
    pub kind: YtDlpHost,
    pub id: String,
}

pub enum YtDlpContent {
    Single(YtDlpItem),
    Playlist(YtDlpPlaylist),
}

#[derive(Clone, Copy, Debug, strum::AsRefStr, strum::Display)]
pub enum YtDlpHost {
    Youtube,
    Soundcloud,
    NicoVideo,
}

impl YtDlpItem {
    pub fn to_url(&self) -> String {
        self.kind.watch_url(&self.id)
    }

    pub fn cache_subdir(&self, root: &Path) -> PathBuf {
        root.join(self.kind.cache_dir_name())
    }

    pub fn get_cached(&self, cache_dir: &Path) -> Option<PathBuf> {
        WalkDir::new(self.cache_subdir(cache_dir))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .find(|e| e.path().file_stem().is_some_and(|stem| stem == OsStr::new(&self.filename)))
            .map(|entry| entry.into_path())
    }

    pub fn delete_cached(&self, cache_dir: &Path) -> Result<()> {
        let files = WalkDir::new(self.cache_subdir(cache_dir))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().file_stem().is_some_and(|stem| stem == OsStr::new(&self.filename)))
            .map(|entry| entry.into_path());

        for file in files {
            log::debug!(file:? = file.as_str(); "Deleting cached file");
            std::fs::remove_file(file)?;
        }

        Ok(())
    }
}

impl YtDlpHost {
    fn cache_dir_name(self) -> &'static str {
        match self {
            Self::Youtube => "youtube",
            Self::Soundcloud => "soundcloud",
            Self::NicoVideo => "nicovideo",
        }
    }

    pub fn watch_url(self, id: &str) -> String {
        match self {
            Self::Youtube => format!("https://www.youtube.com/watch?v={id}"),
            Self::NicoVideo => format!("https://www.nicovideo.jp/watch/{id}"),
            Self::Soundcloud => {
                if id.contains('/') {
                    format!("https://soundcloud.com/{id}")
                } else if id.chars().all(|c| c.is_ascii_digit()) {
                    format!("https://api.soundcloud.com/tracks/{id}")
                } else {
                    // fallback to web
                    format!("https://soundcloud.com/{id}")
                }
            }
        }
    }

    pub fn search_key(self) -> &'static str {
        match self {
            Self::Youtube => "ytsearch",
            Self::Soundcloud => "scsearch",
            Self::NicoVideo => "nicosearch",
        }
    }
}

/// Hosts that serve YouTube content: `youtube.com` plus the mobile /
/// music / nocookie flavours (`www.` is stripped before this is called).
fn is_youtube_host(host: &str) -> bool {
    matches!(
        host,
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com"
    )
}

impl FromStr for YtDlpContent {
    type Err = YtDlpParseError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let url = url::Url::parse(s)?;

        let Some(host) = url.host_str() else {
            return Err(YtDlpParseError::NoHost { url: s.to_string() });
        };

        let bare_host = host.strip_prefix("www.").unwrap_or(host);

        match bare_host {
            // 2026-09-10: every YouTube host flavour (mobile share links,
            // YouTube Music, the nocookie embed domain), not just
            // `www.youtube.com` — pasting those used to do nothing.
            _ if is_youtube_host(bare_host) => {
                let segments = url
                    .path_segments()
                    .ok_or_else(|| YtDlpParseError::invalid_yt(s, "cannot-be-a-base"))?
                    .collect_vec();

                let is_playlist_url = segments.contains(&"playlist")
                    || url.query_pairs().any(|(key, _)| key == "list");
                // 2026-09-10: the video id does not always live in `?v=`;
                // Shorts / live / embed / legacy `/v/` links carry it in the
                // path.
                let path_video_id = segments
                    .iter()
                    .position(|seg| matches!(*seg, "shorts" | "live" | "embed" | "v"))
                    .and_then(|idx| segments.get(idx + 1))
                    .copied()
                    .filter(|id| !id.is_empty());

                if is_playlist_url {
                    url.query_pairs()
                        .find(|(k, _)| k == "list")
                        .map(|(_, v)| YtDlpPlaylist { id: v.to_string(), kind: YtDlpHost::Youtube })
                        .ok_or_else(|| YtDlpParseError::invalid_yt(s, "no playlist id found"))
                        .map(YtDlpContent::Playlist)
                } else {
                    url.query_pairs()
                        .find(|(k, _)| k == "v")
                        .map(|(_, v)| v.to_string())
                        .or_else(|| path_video_id.map(str::to_owned))
                        .map(|id| YtDlpContent::Single(YtDlpItem {
                            id: id.clone(),
                            filename: id,
                            kind: YtDlpHost::Youtube,
                        }))
                        .ok_or_else(|| YtDlpParseError::invalid_yt(s, "no video id found"))
                }
            }
            "youtu.be" => url
                .path_segments()
                .ok_or_else(|| YtDlpParseError::invalid_yt(s, "cannot-be-a-base"))?
                .next()
                .map(|x| YtDlpItem {
                    id: x.to_string(),
                    filename: x.to_string(),
                    kind: YtDlpHost::Youtube,
                })
                .ok_or_else(|| YtDlpParseError::invalid_yt(s, "no video id found"))
                .map(YtDlpContent::Single),
            "soundcloud.com" | "api.soundcloud.com" => {
                let mut path_segments = url
                    .path_segments()
                    .ok_or_else(|| YtDlpParseError::invalid_sc(s, "cannot-be-a-base"))?;
                let Some(first) = path_segments.next() else {
                    return Err(YtDlpParseError::invalid_sc(s, "no path segments found"));
                };

                if first == "tracks" {
                    // API form: https://api.soundcloud.com/tracks/<id>
                    let Some(track_id) = path_segments.next() else {
                        return Err(YtDlpParseError::invalid_sc(s, "no track id found"));
                    };

                    Ok(YtDlpContent::Single(YtDlpItem {
                        id: track_id.to_string(),
                        filename: track_id.to_string(),
                        kind: YtDlpHost::Soundcloud,
                    }))
                } else {
                    // Web form: https://soundcloud.com/<user>/<track>
                    let username = first;
                    let Some(track_name) = path_segments.next() else {
                        return Err(YtDlpParseError::invalid_sc(s, "no track name found"));
                    };

                    Ok(YtDlpContent::Single(YtDlpItem {
                        id: format!("{username}/{track_name}"),
                        filename: format!("{username}-{track_name}"),
                        kind: YtDlpHost::Soundcloud,
                    }))
                }
            }
            "nicovideo.jp" => {
                let mut path_segments = url
                    .path_segments()
                    .ok_or_else(|| YtDlpParseError::invalid_nv(s, "cannot-be-a-base"))?;

                let Some(_watch_segment) = path_segments.next() else {
                    return Err(YtDlpParseError::invalid_nv(s, "no watch segment"));
                };

                let Some(id) = path_segments.next() else {
                    return Err(YtDlpParseError::invalid_nv(s, "no video id found"));
                };

                Ok(YtDlpContent::Single(YtDlpItem {
                    id: id.to_string(),
                    filename: id.to_string(),
                    kind: YtDlpHost::NicoVideo,
                }))
            }
            _ => {
                Err(YtDlpParseError::UnsupportedHost { host: host.to_string(), url: s.to_string() })
            }
        }
    }
}

/// Round 74 (74-1): the start offset a shared link carries, in seconds.
///
/// A link copied from the YouTube player's "Copy video URL at current time"
/// (or typed by hand) carries the position in `?t=`, `?start=` or the
/// `#t=` fragment; `?time_continue=` shows up on some embeds. All four are
/// accepted, on every supported host (the other hosts simply never send
/// them, so the parse is a no-op there).
///
/// `YtDlpContent::from_str` deliberately drops the whole query — it only
/// wants the video id — and yt-dlp ignores the offset when it resolves a
/// stream URL, so nothing downstream could know where to start. The player
/// has to apply the offset itself: MPD seeks once the stream is playing,
/// mpv gets `--start=`. This function is the single place that reads it.
pub fn parse_start_offset(url: &str) -> Option<f64> {
    let url = url.trim();
    let (without_fragment, fragment) = match url.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (url, None),
    };

    // `#t=1m30s` — the fragment form. Checked first: a URL may carry both
    // (YouTube's own share links put the offset in the fragment when the
    // player rewrites the address bar).
    if let Some(offset) = fragment
        .and_then(|fragment| fragment.strip_prefix("t="))
        .and_then(parse_time_value)
    {
        return Some(offset);
    }

    let (_, query) = without_fragment.split_once('?')?;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "t" | "start" | "time_continue"
        ) && let Some(offset) = parse_time_value(value)
        {
            return Some(offset);
        }
    }

    None
}

/// Round 74 (74-1): a YouTube time value into seconds — `90`, `90.5`,
/// `45s`, `1m30s`, `2m`, `1h2m3s`, and the shorthand players accept without
/// the trailing unit (`1h30`, `2m30`). Anything else (a stray `t=foo` on a
/// non-YouTube link) yields `None` so the caller applies no offset.
fn parse_time_value(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // Collapse the `h`/`m`/`s` groups into seconds; each group's digits are
    // accumulated until its unit letter arrives.
    let mut total = 0.0f64;
    let mut digits = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            digits.push(ch);
            continue;
        }
        let value: f64 = digits.parse().ok()?;
        digits.clear();
        match ch.to_ascii_lowercase() {
            'h' => total += value * 3600.0,
            'm' => total += value * 60.0,
            's' => total += value,
            _ => return None,
        }
    }
    // A trailing group without its unit is seconds (`t=1m30`); with no unit
    // at all the whole value was already one group (`t=90`) or the string
    // was empty of digits.
    if !digits.is_empty() {
        total += digits.parse::<f64>().ok()?;
    }

    // `0`/`t=0s` is not an offset (nothing to seek to); non-positive or
    // non-finite values are rejected the same way.
    (total.is_finite() && total > 0.0).then_some(total)
}

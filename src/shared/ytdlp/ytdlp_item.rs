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

#[derive(Debug, Clone, Default)]
pub struct YtDlpItem {
    /// id of the video/audio, to be used in the url
    pub id: String,
    /// filename of the video/audio, will be used to cache the file
    pub filename: String,
    pub kind: YtDlpHost,
    /// Round 82: the video's title when the item came from a playlist
    /// listing (yt-dlp's flat playlist carries it). `None` for a pasted
    /// single link, whose title is only known once its stream is resolved.
    /// The pickers (which videos of a playlist to add/save) label their
    /// rows with it.
    pub title: Option<String>,
    /// Round 95: the video's duration in seconds and its channel/uploader,
    /// both from the flat playlist listing. A playlist added to the queue
    /// takes its rows' titles and durations from this listing (one yt-dlp
    /// call) instead of resolving every item, so the rows are correct the
    /// moment they land. `None` when the listing has no value for them.
    pub duration: Option<f64>,
    pub channel: Option<String>,
}

pub struct YtDlpPlaylist {
    pub kind: YtDlpHost,
    pub id: String,
}

pub enum YtDlpContent {
    Single(YtDlpItem),
    Playlist(YtDlpPlaylist),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, strum::AsRefStr, strum::Display)]
pub enum YtDlpHost {
    #[default]
    Youtube,
    Soundcloud,
    NicoVideo,
}

impl YtDlpItem {
    pub fn to_url(&self) -> String {
        self.kind.watch_url(&self.id)
    }

    /// Round 82: the label a picker row shows — the playlist listing's
    /// title when there is one, else the id/filename.
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&self.filename)
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

    /// Round 96: the provider's name as the search modal labels it — the
    /// title on the popup's top border and the provider row itself. The
    /// variant name is close but not right ("Youtube" / "Soundcloud").
    pub fn search_label(self) -> &'static str {
        match self {
            Self::Youtube => "YouTube",
            Self::Soundcloud => "SoundCloud",
            Self::NicoVideo => "NicoVideo",
        }
    }
}

/// Round 95: what a yt-dlp **flat playlist listing** knows about one
/// YouTube-style link: the title, the channel/uploader and the duration.
/// A playlist added to the queue shows these in its rows right away (the
/// listing is one yt-dlp call, no per-item resolve), and they are cached so
/// a stored playlist built from that listing keeps showing them later.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct YtListMeta {
    pub title: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
}

/// Round 86: how a stored playlist entry is meant to be played. A playlist
/// keeps the **link** (a resolved googlevideo URL expires after a few
/// hours), so the entry needs the intent alongside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamIntent {
    Audio,
    Video,
}

impl StreamIntent {
    fn tag(self) -> &'static str {
        match self {
            Self::Audio => "#s2u-audio",
            Self::Video => "#s2u-video",
        }
    }

    pub fn is_audio(self) -> bool {
        matches!(self, Self::Audio)
    }
}

/// Round 86: the stored-playlist form of a YouTube-style link: the link
/// with its intent tag as a URL **fragment**. MPD stores the URI verbatim,
/// yt-dlp ignores fragments, and the link stays recognisable in other MPD
/// clients (`mpc listplaylist` shows `…watch?v=ID#s2u-audio`).
pub fn tagged_stream_link(link: &str, intent: StreamIntent) -> String {
    format!("{}{}", untag_stream_link(link), intent.tag())
}

/// Round 86: `(link, intent)` when a stored playlist entry is a tagged
/// YouTube-style link (what `tagged_stream_link` wrote).
pub fn tagged_stream_entry(uri: &str) -> Option<(String, StreamIntent)> {
    let (link, intent) = split_stream_tag(uri);
    intent.map(|intent| (link.to_owned(), intent))
}

/// Round 86: the link of a stored entry with its tag stripped, for
/// `yt_info`/title lookups.
pub fn untag_stream_link(uri: &str) -> &str {
    split_stream_tag(uri).0
}

fn split_stream_tag(uri: &str) -> (&str, Option<StreamIntent>) {
    for (tag, intent) in [
        ("#s2u-audio", StreamIntent::Audio),
        ("#s2u-video", StreamIntent::Video),
    ] {
        if let Some(link) = uri.strip_suffix(tag) {
            return (link, Some(intent));
        }
    }
    (uri, None)
}

/// Round 79: the video id a YouTube-style link names (`?v=` or the id in a
/// `shorts` / `live` / `embed` / legacy `/v/` path). `None` for a link that
/// carries only a playlist (`/playlist?list=…`).
pub fn yt_video_id(s: &str) -> Option<String> {
    let url = url::Url::parse(s).ok()?;
    let host = url.host_str()?;
    let bare_host = host.strip_prefix("www.").unwrap_or(host);

    if bare_host == "youtu.be" {
        return url
            .path_segments()?
            .next()
            .map(str::to_owned)
            .filter(|id| !id.is_empty());
    }

    if !is_youtube_host(bare_host) {
        return None;
    }

    url.query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.to_string())
        .filter(|id| !id.is_empty())
        .or_else(|| {
            let segments = url.path_segments()?.collect_vec();
            segments
                .iter()
                .position(|seg| matches!(*seg, "shorts" | "live" | "embed" | "v"))
                .and_then(|idx| segments.get(idx + 1))
                .map(|id| (*id).to_owned())
                .filter(|id| !id.is_empty())
        })
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
                            title: None,
                            ..Default::default()
                        }))
                        .ok_or_else(|| YtDlpParseError::invalid_yt(s, "no video id found"))
                }
            }
            "youtu.be" => {
                // Round 79: a `youtu.be` share link copied from inside a
                // playlist carries that playlist in `?list=` — treat it
                // like the `watch?v=…&list=…` form, so the paste popup
                // offers the same `[Playlist]` rows for both. The video id
                // stays available through `yt_video_id`, which is what the
                // single-link rows use.
                if let Some(list_id) = url
                    .query_pairs()
                    .find(|(key, _)| key == "list")
                    .map(|(_, value)| value.to_string())
                    .filter(|id| !id.is_empty())
                {
                    return Ok(YtDlpContent::Playlist(YtDlpPlaylist {
                        id: list_id,
                        kind: YtDlpHost::Youtube,
                    }));
                }
                url.path_segments()
                    .ok_or_else(|| YtDlpParseError::invalid_yt(s, "cannot-be-a-base"))?
                    .next()
                    .map(|x| YtDlpItem {
                        id: x.to_string(),
                        filename: x.to_string(),
                        kind: YtDlpHost::Youtube,
                        title: None,
                        ..Default::default()
                    })
                    .ok_or_else(|| YtDlpParseError::invalid_yt(s, "no video id found"))
                    .map(YtDlpContent::Single)
            }
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
                        title: None,
                        ..Default::default()
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
                        title: None,
                        ..Default::default()
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
                    title: None,
                    ..Default::default()
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

use std::process::Command;
use anyhow::Context;
use serde::Deserialize;

use super::ytdlp_item::parse_start_offset;
/// Everything the app shows for a YouTube-style link resolved to a direct
/// audio stream: the stream URL (fed to MPD), the video title (now-playing
/// info), the thumbnail (album art) and the description (info box). Also
/// carries the original link so the info can be re-fetched later (e.g. after
/// a restart when the stream is still playing).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct YtStreamInfo {
    pub url: String,
    /// The original YouTube/Soundcloud/NicoVideo link this stream came from.
    pub original_url: String,
    pub title: String,
    /// The channel/uploader name (for MPRIS artist).
    pub channel: Option<String>,
    /// The channel's follower/subscriber count (yt-dlp
    /// `channel_follower_count`), shown in the video info box.
    pub subscribers: Option<u64>,
    pub thumbnail: Option<String>,
    pub description: Option<String>,
    /// The video's duration in seconds (yt-dlp `duration`), carried so a
    /// later MPRIS bridge can inject a timeline even when the playback
    /// backend (MPD on an HLS stream) reports none.
    pub duration: Option<f64>,
    /// Chapter markers: the video's embedded chapters, or — when the video
    /// has none — timestamp lines parsed from its description.
    pub chapters: Vec<crate::shared::chapters::Chapter>,
    /// Round 74 (74-1): the start offset the pasted link carried (`?t=90`,
    /// `?start=90`, `#t=1m30s`), in seconds. yt-dlp resolves the stream URL
    /// but drops the offset, so the player applies it: MPD seeks once the
    /// stream is playing, mpv is launched with `--start=`. `#[serde(default)]`
    /// keeps the existing `<cache_dir>/yt-info.json` loadable (the field is
    /// absent in entries written before this round).
    #[serde(default)]
    pub start_secs: Option<f64>,
    /// Round 91b: when this entry was resolved, in epoch seconds. A later
    /// play of the same link reuses the entry while it is fresh instead of
    /// running yt-dlp again, so a link the paste popup already resolved
    /// starts immediately. `#[serde(default)]` keeps the existing
    /// `<cache_dir>/yt-info.json` loadable - an entry written before this
    /// field has no age and is therefore treated as stale.
    #[serde(default)]
    pub resolved_at: Option<u64>,
}
/// Resolve a YouTube/Soundcloud/NicoVideo URL to its direct audio stream
/// URL(s) with yt-dlp, along with each video's title, thumbnail and
/// description. No download happens: the returned URLs are fed straight to
/// MPD, which decodes them like a radio stream.
///
/// A single video resolves to one entry; a playlist URL resolves to one
/// entry per item. Entries that fail to resolve are skipped and reported in
/// `failures`.
pub fn resolve_audio_urls(urls: &[String]) -> (Vec<YtStreamInfo>, Vec<String>) {
    let bin = std::env::var("S2UDIO_YTDLP_BIN").unwrap_or_else(|_| "yt-dlp".to_owned());
    let mut resolved: Vec<YtStreamInfo> = Vec::new();
    let mut failures = Vec::new();
    // Round 59: before resolving any YouTube-style link, heal a stale
    // bgutil PO-token service (its 12 h minter cliff leaves yt-dlp unable
    // to mint tokens and every YouTube video "won't play" until the
    // service is restarted). The heal is cooldown-guarded and a fresh
    // service is never touched.
    if urls.iter().any(|u| crate::shared::bgutil::is_youtube_url(u)) {
        match crate::shared::bgutil::maybe_heal() {
            crate::shared::bgutil::HealOutcome::Restarted => {
                log::info!("bgutil restarted before YouTube resolution (12h minter renewal)");
            }
            crate::shared::bgutil::HealOutcome::Failed => {
                log::warn!("bgutil was stale but the restart failed; YouTube may not resolve");
            }
            _ => {}
        }
    }
    for url in urls {
        match resolve_one(&bin, url) {
            Ok(mut entries) => resolved.append(&mut entries),
            Err(err) => {
                // Round 59 self-heal: a failed YouTube resolution with a
                // stale bgutil is exactly the 12 h minter cliff — restart
                // once and retry the URL a single time.
                if crate::shared::bgutil::is_youtube_url(url)
                    && crate::shared::bgutil::maybe_heal()
                        == crate::shared::bgutil::HealOutcome::Restarted
                {
                    log::info!("bgutil restarted after a YouTube resolution failure; retrying once");
                    match resolve_one(&bin, url) {
                        Ok(mut entries) => {
                            resolved.append(&mut entries);
                            continue;
                        }
                        Err(retry_err) => {
                            failures.push(format!("{url}: {retry_err} (after bgutil renewal)"));
                            continue;
                        }
                    }
                }
                failures.push(format!("{url}: {err}"));
            }
        }
    }
    (resolved, failures)
}
/// Round 111: the **metadata-only** of one link — the fields the info box and
/// the album-art pane need (title, channel, duration, thumbnail), without the
/// stream URL or the chapters.
///
/// A full `-J` resolve costs ~2.6 s measured on this machine (it emits every
/// audio format, the format list, the heatmap and the description, i.e.
/// megabytes of JSON). `--print` with only these fields costs ~1.6 s and a few
/// hundred bytes, which is what the selection-time warm uses: a merely-selected
/// queue row needs its description/artwork, and neither the signed stream URL
/// nor the chapter list is wanted until the row actually plays (that is when
/// the full resolve runs).
///
/// Fields yt-dlp does not know print as `NA`; each is mapped to `None` rather
/// than leaking that sentinel into the UI.
pub fn resolve_meta_urls(urls: &[String]) -> (Vec<YtStreamInfo>, Vec<String>) {
    let bin = std::env::var("S2UDIO_YTDLP_BIN").unwrap_or_else(|_| "yt-dlp".to_owned());
    let mut resolved: Vec<YtStreamInfo> = Vec::new();
    let mut failures = Vec::new();
    if urls.iter().any(|u| crate::shared::bgutil::is_youtube_url(u)) {
        let _ = crate::shared::bgutil::maybe_heal();
    }
    for url in urls {
        match resolve_meta_one(&bin, url) {
            Ok(info) => resolved.push(info),
            Err(err) => {
                if crate::shared::bgutil::is_youtube_url(url)
                    && crate::shared::bgutil::maybe_heal()
                        == crate::shared::bgutil::HealOutcome::Restarted
                {
                    match resolve_meta_one(&bin, url) {
                        Ok(info) => {
                            resolved.push(info);
                            continue;
                        }
                        Err(retry_err) => {
                            failures.push(format!("{url}: {retry_err} (after bgutil renewal)"));
                            continue;
                        }
                    }
                }
                failures.push(format!("{url}: {err}"));
            }
        }
    }
    (resolved, failures)
}
/// One link's metadata through `--print`, parsed from the `|`-separated line
/// (see [`resolve_meta_urls`]).
fn resolve_meta_one(bin: &str, input_url: &str) -> anyhow::Result<YtStreamInfo> {
    // The separator must not occur in a title; `\x1f` (unit separator) cannot
    // come out of yt-dlp's templating for these fields and keeps a title with
    // a literal `|` intact.
    const SEP: &str = "\u{1f}";
    let out = Command::new(bin)
        .args([
            "--print",
            "%(title)s\u{1f}%(channel)s\u{1f}%(duration)s\u{1f}%(thumbnail)s\u{1f}%(channel_follower_count)s\u{1f}%(description)s",
            "--no-playlist",
            "--no-warnings",
            "--",
            input_url,
        ])
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("yt-dlp failed")
            .trim()
            .to_owned();
        anyhow::bail!(line);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        anyhow::bail!("yt-dlp produced no metadata");
    }
    // The **description is last and holds newlines**, so the line is split on
    // at most `FIELD_COUNT - 1` separators: everything after the last one is
    // the description verbatim, newlines and all (`lines()` would truncate it
    // at the first blank line, which is what a YouTube description starts
    // with).
    const FIELD_COUNT: usize = 6;
    let mut fields = stdout.trim_end().splitn(FIELD_COUNT, SEP);
    // yt-dlp prints `NA` for a field the video does not have.
    let field = |value: Option<&str>| {
        value.map(str::trim).filter(|v| !v.is_empty() && *v != "NA").map(str::to_owned)
    };
    let title = field(fields.next()).unwrap_or_default();
    let channel = field(fields.next());
    let duration = field(fields.next()).and_then(|d| d.parse::<f64>().ok());
    let thumbnail = field(fields.next());
    let subscribers = field(fields.next()).and_then(|s| s.parse::<u64>().ok());
    let description = field(fields.next());
    if title.is_empty() && thumbnail.is_none() {
        anyhow::bail!("yt-dlp produced no metadata");
    }
    Ok(YtStreamInfo {
        // The stream URL arrives with the full resolve, when the row plays.
        url: String::new(),
        original_url: input_url.to_owned(),
        title,
        channel,
        subscribers,
        thumbnail,
        description,
        duration,
        chapters: Vec::new(),
        start_secs: parse_start_offset(input_url),
        resolved_at: None,
    })
}
/// A subset of yt-dlp's `-J` JSON for the fields the app uses.
#[derive(Deserialize)]
struct YtDlpJson {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    channel_follower_count: Option<u64>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    chapters: Option<Vec<YtDlpChapter>>,
}
#[derive(Deserialize)]
struct YtDlpChapter {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    start_time: Option<f64>,
    #[serde(default)]
    end_time: Option<f64>,
}
fn resolve_one(bin: &str, input_url: &str) -> anyhow::Result<Vec<YtStreamInfo>> {
    let out = Command::new(bin)
        .args([
            "-J",
            "-f",
            "bestaudio/best",
            "--no-playlist",
            "--no-warnings",
            "--",
            input_url,
        ])
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("yt-dlp failed")
            .trim()
            .to_owned();
        anyhow::bail!(line);
    }
    let parsed: YtDlpJson = serde_json::from_slice(&out.stdout)
        .context("Cannot parse yt-dlp output")?;
    let Some(url) = parsed
        .url
        .as_deref()
        .filter(|u| u.starts_with("http://") || u.starts_with("https://")) else {
        anyhow::bail!("yt-dlp produced no stream URL");
    };
    let url = url.to_owned();
    let description = parsed.description.as_deref().filter(|d| !d.trim().is_empty());
    let chapters = chapters_from_json(&parsed, description);
    let description = description.map(str::to_owned);
    Ok(
        vec![
            YtStreamInfo { url, original_url : input_url.to_owned(), title : parsed.title
            .unwrap_or_default(), channel : parsed.channel.or(parsed.uploader).filter(| c
            | ! c.is_empty()), subscribers : parsed.channel_follower_count, thumbnail :
            parsed.thumbnail.filter(| t | ! t.is_empty()), description, duration : parsed
            .duration, chapters, start_secs : parse_start_offset(input_url),
            resolved_at : None, }
        ],
    )
}
/// Chapters of the video: the embedded `chapters` array, or — when the
/// video has no embedded chapters — timestamp lines parsed from the
/// description (e.g. `0:00 Intro`, `2:34 Second part`).
fn chapters_from_json(
    parsed: &YtDlpJson,
    description: Option<&str>,
) -> Vec<crate::shared::chapters::Chapter> {
    if let Some(chapters) = &parsed.chapters {
        let embedded: Vec<crate::shared::chapters::Chapter> = chapters
            .iter()
            .filter_map(|c| {
                let title = c.title.as_deref().unwrap_or("").trim().to_owned();
                let start = c.start_time?;
                if title.is_empty() {
                    return None;
                }
                Some(crate::shared::chapters::Chapter {
                    title,
                    start_secs: start,
                    end_secs: c.end_time.unwrap_or(start),
                })
            })
            .collect();
        if embedded.len() > 1 {
            return embedded;
        }
    }
    let Some(description) = description else { return Vec::new() };
    let duration = parsed.duration.unwrap_or(0.0);
    chapters_from_description(description, duration)
}
/// Parse `mm:ss Title` / `hh:mm:ss Title` lines from a description into
/// chapter markers. Each chapter ends where the next begins (or at the
/// video's duration).
fn chapters_from_description(
    description: &str,
    duration: f64,
) -> Vec<crate::shared::chapters::Chapter> {
    let mut markers: Vec<(f64, String)> = Vec::new();
    for line in description.lines() {
        let line = line.trim();
        let Some((timestamp, title)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Some(seconds) = parse_timestamp(timestamp) else { continue };
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        markers.push((seconds, title.to_owned()));
    }
    markers.sort_by(|a, b| a.0.total_cmp(&b.0));
    markers.dedup_by(|a, b| a.0 == b.0);
    markers.retain(|(s, _)| *s >= 0.0);
    let mut chapters = Vec::new();
    for (idx, (start, title)) in markers.iter().enumerate() {
        let end = markers.get(idx + 1).map(|(s, _)| *s).unwrap_or(duration);
        if end <= *start && idx + 1 < markers.len() {
            continue;
        }
        chapters
            .push(crate::shared::chapters::Chapter {
                title: title.clone(),
                start_secs: *start,
                end_secs: end,
            });
    }
    chapters
}
/// Parse `m:ss`, `mm:ss` or `hh:mm:ss` into seconds.
fn parse_timestamp(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    let secs: f64 = match parts.as_slice() {
        [mm, ss] => mm.parse::<f64>().ok()? * 60.0 + ss.parse::<f64>().ok()?,
        [hh, mm, ss] => {
            hh.parse::<f64>().ok()? * 3600.0 + mm.parse::<f64>().ok()? * 60.0
                + ss.parse::<f64>().ok()?
        }
        _ => return None,
    };
    (secs >= 0.0).then_some(secs)
}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::YtStreamInfo;

    /// The two resolve tests mutate the shared `S2UDIO_YTDLP_BIN` env var;
    /// serialize them so a parallel run can't spawn the other test's fake
    /// script.
    static YTDLP_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// A fake yt-dlp that echoes a minimal `-J` JSON document.
    const JSON: &str = "{\"title\":\"Rick Astley - Never Gonna Give You Up\",\
                        \"url\":\"https://rr4.example/audio.m4a\",\
                        \"thumbnail\":\"https://i.ytimg.com/vi/x/hqdefault.jpg\",\
                        \"channel\":\"Rick Astley\",\
                        \"channel_follower_count\":15200000,\
                        \"description\":\"Never gonna give you up.\"}";

    fn fake_bin(script: &str, tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ytdlp-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let bin = dir.join("yt-dlp");
        std::fs::write(&bin, script).unwrap();
        let _ = std::process::Command::new("chmod").arg("+x").arg(&bin).status();
        bin
    }

    #[test]
    fn resolve_parses_json_fields() {
        let _guard = YTDLP_ENV_LOCK.lock().unwrap();
        let script = format!("#!/bin/sh\necho '{JSON}'\n");
        let bin = fake_bin(&script, "ok");
        unsafe {
            std::env::set_var("S2UDIO_YTDLP_BIN", &bin);
        }
        let (entries, failures) = super::resolve_audio_urls(&["https://youtu.be/x".to_owned()]);
        unsafe {
            std::env::remove_var("S2UDIO_YTDLP_BIN");
        }

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            entries,
            vec![YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                original_url: "https://youtu.be/x".to_owned(),
                title: "Rick Astley - Never Gonna Give You Up".to_owned(),
                channel: Some("Rick Astley".to_owned()),
                subscribers: Some(15_200_000),
                thumbnail: Some("https://i.ytimg.com/vi/x/hqdefault.jpg".to_owned()),
                description: Some("Never gonna give you up.".to_owned()),
                start_secs: None,
                resolved_at: None,
                duration: None,
                chapters: Vec::new(),
            }]
        );
    }

    #[test]
    fn meta_resolve_parses_printed_fields() {
        // Round 111: the selection-time warm's cheap call. `--print` emits one
        // unit-separator line instead of a multi-megabyte JSON document.
        let _guard = YTDLP_ENV_LOCK.lock().unwrap();
        let script = format!(
            "#!/bin/sh\necho 'Rick Astley - Never Gonna Give You Up\u{1f}Rick Astley\u{1f}213\u{1f}https://i.ytimg.com/vi/x/maxresdefault.jpg\u{1f}15200000\u{1f}Never gonna give you up.'\n"
        );
        let bin = fake_bin(&script, "meta");
        unsafe {
            std::env::set_var("S2UDIO_YTDLP_BIN", &bin);
        }
        let (entries, failures) = super::resolve_meta_urls(&["https://youtu.be/x".to_owned()]);
        unsafe {
            std::env::remove_var("S2UDIO_YTDLP_BIN");
        }

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(entries.len(), 1);
        let info = &entries[0];
        assert_eq!(info.title, "Rick Astley - Never Gonna Give You Up");
        assert_eq!(info.channel.as_deref(), Some("Rick Astley"));
        assert_eq!(info.duration, Some(213.0));
        assert_eq!(info.thumbnail.as_deref(), Some("https://i.ytimg.com/vi/x/maxresdefault.jpg"));
        assert_eq!(info.subscribers, Some(15_200_000));
        // The description lands with the selection warm: the info box shows it
        // before the row plays.
        assert_eq!(info.description.as_deref(), Some("Never gonna give you up."));
        // The stream URL and chapters belong to the full resolve, which runs
        // when the row actually plays.
        assert!(info.url.is_empty());
        assert!(info.chapters.is_empty());
        assert_eq!(info.original_url, "https://youtu.be/x");
    }

    #[test]
    fn meta_resolve_maps_missing_fields_to_none() {
        // yt-dlp prints `NA` for a field the video does not have; it must not
        // reach the UI as a literal "NA" channel or duration.
        let _guard = YTDLP_ENV_LOCK.lock().unwrap();
        let script = "#!/bin/sh\necho 'Some Video\u{1f}NA\u{1f}NA\u{1f}https://i.ytimg.com/vi/x/hqdefault.jpg\u{1f}NA\u{1f}NA'\n";
        let bin = fake_bin(&script, "meta-na");
        unsafe {
            std::env::set_var("S2UDIO_YTDLP_BIN", &bin);
        }
        let (entries, failures) = super::resolve_meta_urls(&["https://youtu.be/x".to_owned()]);
        unsafe {
            std::env::remove_var("S2UDIO_YTDLP_BIN");
        }

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].channel, None);
        assert_eq!(entries[0].duration, None);
        assert_eq!(entries[0].subscribers, None);
        assert_eq!(entries[0].description, None);
        assert_eq!(entries[0].title, "Some Video");
    }

    #[test]
    fn meta_resolve_keeps_a_multiline_description() {
        // A YouTube description is multi-paragraph and starts with a blank
        // line; the parser must take everything after the last separator
        // verbatim instead of stopping at the first newline.
        let _guard = YTDLP_ENV_LOCK.lock().unwrap();
        let script = "#!/bin/sh\nprintf 'Some Video\u{1f}Some Channel\u{1f}120\u{1f}https://i.ytimg.com/vi/x/hqdefault.jpg\u{1f}42\u{1f}\nProvided to YouTube by X\n\nLine two\nLine three\n'\n";
        let bin = fake_bin(&script, "meta-multiline");
        unsafe {
            std::env::set_var("S2UDIO_YTDLP_BIN", &bin);
        }
        let (entries, failures) = super::resolve_meta_urls(&["https://youtu.be/x".to_owned()]);
        unsafe {
            std::env::remove_var("S2UDIO_YTDLP_BIN");
        }

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].subscribers, Some(42));
        let description = entries[0].description.as_deref().unwrap_or_default();
        assert!(description.contains("Provided to YouTube by X"), "{description:?}");
        assert!(description.contains("Line two"), "{description:?}");
        assert!(description.contains("Line three"), "{description:?}");
    }

    #[test]
    fn meta_resolve_reports_a_failure() {
        let _guard = YTDLP_ENV_LOCK.lock().unwrap();
        let script = "#!/bin/sh\necho 'ERROR: Video unavailable' >&2\nexit 1\n";
        let bin = fake_bin(&script, "meta-fail");
        unsafe {
            std::env::set_var("S2UDIO_YTDLP_BIN", &bin);
        }
        let (entries, failures) = super::resolve_meta_urls(&["https://youtu.be/x".to_owned()]);
        unsafe {
            std::env::remove_var("S2UDIO_YTDLP_BIN");
        }

        assert!(entries.is_empty());
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("Video unavailable"), "{failures:?}");
    }

    #[test]
    fn description_timestamps_become_chapters() {
        let description = "0:00 Intro\n2:34 Second part\n1:02:03 Finale\nsome other text\n";
        let chapters = super::chapters_from_description(description, 3600.0);
        assert_eq!(chapters.len(), 3);
        assert_eq!(chapters[0].title, "Intro");
        assert_eq!(chapters[0].start_secs, 0.0);
        assert_eq!(chapters[1].title, "Second part");
        assert_eq!(chapters[1].start_secs, 154.0);
        assert_eq!(chapters[1].end_secs, 3723.0);
        assert_eq!(chapters[2].title, "Finale");
        assert_eq!(chapters[2].start_secs, 3723.0);
        assert_eq!(chapters[2].end_secs, 3600.0); // clamped to duration
    }

    #[test]
    fn description_without_timestamps_has_no_chapters() {
        assert!(super::chapters_from_description("no timestamps here", 100.0).is_empty());
        assert!(super::chapters_from_description("12:34", 100.0).is_empty());
    }

    #[test]
    fn parse_timestamp_formats() {
        assert_eq!(super::parse_timestamp("0:00"), Some(0.0));
        assert_eq!(super::parse_timestamp("2:34"), Some(154.0));
        assert_eq!(super::parse_timestamp("1:02:03"), Some(3723.0));
        assert_eq!(super::parse_timestamp("12"), None);
        assert_eq!(super::parse_timestamp("abc"), None);
    }

    #[test]
    fn resolve_fails_on_missing_url() {
        let _guard = YTDLP_ENV_LOCK.lock().unwrap();
        let script = "#!/bin/sh\necho '{\"title\":\"x\"}'\n";
        let bin = fake_bin(&script, "nourl");
        unsafe {
            std::env::set_var("S2UDIO_YTDLP_BIN", &bin);
        }
        let (entries, failures) = super::resolve_audio_urls(&["https://youtu.be/x".to_owned()]);
        unsafe {
            std::env::remove_var("S2UDIO_YTDLP_BIN");
        }

        assert!(!failures.is_empty());
        assert!(entries.is_empty());
    }
}

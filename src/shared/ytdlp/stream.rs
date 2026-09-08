use std::process::Command;
use anyhow::Context;
use serde::Deserialize;
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
            .duration, chapters, }
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

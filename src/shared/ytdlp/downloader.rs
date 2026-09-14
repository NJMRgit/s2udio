use std::{path::PathBuf, process::Command};

use itertools::Itertools;

use crate::shared::ytdlp::{
    error::YtDlpDownloadError,
    ytdlp_item::{YtDlpHost, YtDlpItem, YtDlpPlaylist},
};

pub struct YtDlp {
    pub cache_dir: PathBuf,
}

#[derive(Debug)]
pub struct YtDlpDownloadResult {
    pub file_path: PathBuf,
    /// Every file the download produced (1 for a plain download, N for a
    /// split-chapters download). The first entry equals `file_path`.
    pub file_paths: Vec<PathBuf>,
    pub stderr: String,
    pub stdout: String,
    pub exit_code: Option<i32>,
    pub was_already_downloaded: bool,
}

impl YtDlpDownloadResult {
    pub fn new_already_downloaded(file_path: PathBuf) -> Self {
        Self {
            file_paths: vec![file_path.clone()],
            file_path,
            stderr: String::new(),
            stdout: String::new(),
            was_already_downloaded: true,
            exit_code: None,
        }
    }
}

#[derive(serde::Deserialize)]
struct SearchEntry {
    id: Option<String>,
    url: Option<String>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(serde::Deserialize)]
struct SearchJson {
    entries: Vec<SearchEntry>,
}

#[derive(Debug, Clone)]
pub struct YtDlpSearchItem {
    pub title: Option<String>,
    pub url: String,
}

impl YtDlp {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    pub fn search(
        kind: YtDlpHost,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<YtDlpSearchItem>> {
        let expr = format!("{}{}:{query}", kind.search_key(), limit.max(1));
        let out =
            std::process::Command::new("yt-dlp").args(["-J", "--flat-playlist", &expr]).output()?;
        if !out.status.success() {
            anyhow::bail!("yt-dlp search failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let parsed: SearchJson = serde_json::from_slice(&out.stdout)?;
        let items = parsed
            .entries
            .into_iter()
            .filter_map(|e| {
                let url = match kind {
                    YtDlpHost::Soundcloud => e.webpage_url.or(e.url),
                    _ => e.url.or(e.webpage_url),
                }
                .or_else(|| e.id.clone().map(|id| kind.watch_url(&id)))?;
                Some(YtDlpSearchItem { title: e.title, url })
            })
            .collect::<Vec<_>>();

        Ok(items)
    }

    pub fn download_single(
        &self,
        id: &YtDlpItem,
    ) -> Result<YtDlpDownloadResult, YtDlpDownloadError> {
        if let Some(cached_file) = id.get_cached(&self.cache_dir) {
            return Ok(YtDlpDownloadResult::new_already_downloaded(cached_file));
        }

        let mut cache = id.cache_subdir(&self.cache_dir);
        std::fs::create_dir_all(&cache)?;
        cache.push(format!("{}.%(ext)s", id.filename));

        let mut command = Command::new("yt-dlp");
        command.arg("-x");
        command.arg("--embed-thumbnail");
        command.arg("--embed-metadata");
        command.arg("-f");
        // bestaudio/best (not bare bestaudio): with the authenticated
        // web_safari client no pure audio-only format exists, so fall back
        // to the best combined format (-x extracts audio via ffmpeg).
        command.arg("bestaudio/best");
        command.arg("--convert-thumbnails");
        command.arg("jpg");
        command.arg("--output");
        command.arg(cache);
        command.arg(id.to_url());
        let args = command
            .get_args()
            .map(|arg| format!("\"{}\"", arg.to_string_lossy()))
            .join(" ")
            .clone();
        log::debug!(args = args.as_str(); "Executing yt-dlp");

        let out = command.output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let exit_code = out.status.code();
        log::debug!(stdout = stdout.as_str().trim(), stderr = stderr.as_str().trim(), exit_code:?; "yt-dlp finished");

        if exit_code != Some(0) {
            log::error!(stderr = stderr.as_str().trim();"yt-dlp failed");
            if let Err(err) = id.delete_cached(&self.cache_dir) {
                log::error!(err = err.to_string().as_str(); "Failed to cleanup after yt-dlp failed");
            }

            return Err(YtDlpDownloadError::YtDlpError { stdout, stderr, code: exit_code });
        }

        // yt-dlp for some reason does not respect output file template when
        // doing post processing with ffmpeg. This results in the file
        // having different extensions than the one specified so we work
        // around it by trying to find the file in the cache directory as that
        // should still be reliable.
        match id.get_cached(&self.cache_dir) {
            Some(file_path) => Ok(YtDlpDownloadResult {
                file_paths: vec![file_path.clone()],
                file_path,
                stderr,
                stdout,
                was_already_downloaded: false,
                exit_code,
            }),
            None => Err(YtDlpDownloadError::FileNotFound { stdout, stderr, code: exit_code }),
        }
    }

    /// Download a ytdlp media item straight into `spec.output_dir` (the
    /// `s2udio-downloads` folder in ~/Downloads): audio
    /// only (`-x`) or the best video+audio merged into mp4, and either one
    /// file with chapters or — with `split_chapters` — one file per
    /// chapter named after the chapter title. A non-empty `sections` list
    /// instead downloads exactly those chapter ranges, one yt-dlp run and
    /// one output file per range (`split_chapters` is then ignored, and
    /// `--split-chapters` is never passed). Returns every file the
    /// download produced.
    pub fn download_stream(
        &self,
        item: &YtDlpItem,
        spec: &crate::shared::ytdlp::StreamDownloadSpec,
    ) -> Result<YtDlpDownloadResult, YtDlpDownloadError> {
        let dir = &spec.output_dir;
        std::fs::create_dir_all(dir)?;
        // Snapshot the dir so the produced files can be told apart from
        // files that were already there (yt-dlp post-processing renames
        // files, so the output template cannot be trusted alone).
        let before: std::collections::HashSet<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();

        // Chapter-section mode: one yt-dlp run per requested range, one
        // output file each. `--split-chapters` is never used here.
        if !spec.sections.is_empty() {
            let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            let mut files: Vec<PathBuf> = Vec::new();
            let mut stdout = String::new();
            let mut stderr = String::new();
            let mut exit_code: Option<i32> = None;
            let mut failed: Option<Option<i32>> = None;

            for (index, section) in spec.sections.iter().enumerate() {
                let template = dir.join(format!(
                    "{:02} - {}.%(ext)s",
                    index + 1,
                    sanitize_section_title(&section.title)
                ));
                let range =
                    format!("*{:.3}-{:.3}", section.start_secs, section.end_secs);
                log::debug!(range = range.as_str(); "Executing yt-dlp (stream download section)");

                let mut command = Command::new("yt-dlp");
                command.arg("--no-warnings");
                if spec.audio_only {
                    command.arg("-x");
                    command.arg("-f").arg("bestaudio/best");
                    command.arg("--embed-thumbnail");
                    command.arg("--embed-metadata");
                    command.arg("--convert-thumbnails").arg("jpg");
                } else {
                    command.arg("-f").arg("bv*+ba/b");
                    command.arg("--merge-output-format").arg("mp4");
                    command.arg("--embed-metadata");
                    command.arg("--embed-thumbnail");
                    command.arg("--embed-chapters");
                    command.arg("--convert-thumbnails").arg("jpg");
                }
                command.arg("--download-sections").arg(range);
                command.arg("--force-keyframes-at-cuts");
                command.arg("--output").arg(template);
                command.arg(item.to_url());
                let args = command
                    .get_args()
                    .map(|arg| format!("\"{}\"", arg.to_string_lossy()))
                    .join(" ")
                    .clone();
                log::debug!(args = args.as_str(); "Executing yt-dlp (stream download)");

                let out = command.output()?;
                stdout.push_str(&String::from_utf8_lossy(&out.stdout));
                stderr.push_str(&String::from_utf8_lossy(&out.stderr));
                exit_code = out.status.code();
                log::debug!(stdout = stdout.as_str().trim(), stderr = stderr.as_str().trim(), exit_code:?; "yt-dlp finished");

                if exit_code != Some(0) {
                    log::error!(stderr = stderr.as_str().trim(); "yt-dlp failed");
                    failed = failed.or(Some(exit_code));
                }

                // Accumulate the files produced by this run (the union over
                // all runs, deduplicated and sorted).
                let mut produced: Vec<PathBuf> = std::fs::read_dir(dir)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.path())
                            .filter(|p| {
                                !before.contains(p)
                                    && !seen.contains(p)
                                    && !is_thumbnail_file(p)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                for path in produced.drain(..) {
                    seen.insert(path.clone());
                    files.push(path);
                }
            }

            if let Some(code) = failed {
                return Err(YtDlpDownloadError::YtDlpError { stdout, stderr, code });
            }

            files.sort();
            let Some(file_path) = files.first().cloned() else {
                return Err(YtDlpDownloadError::FileNotFound { stdout, stderr, code: exit_code });
            };
            return Ok(YtDlpDownloadResult {
                file_paths: files,
                file_path,
                stderr,
                stdout,
                was_already_downloaded: false,
                exit_code,
            });
        }

        let template = if spec.split_chapters {
            dir.join("%(section_title)s.%(ext)s")
        } else {
            dir.join("%(title)s [%(id)s].%(ext)s")
        };
        let mut command = Command::new("yt-dlp");
        command.arg("--no-warnings");
        if spec.audio_only {
            command.arg("-x");
            command.arg("-f").arg("bestaudio/best");
            command.arg("--embed-thumbnail");
            command.arg("--embed-metadata");
            command.arg("--convert-thumbnails").arg("jpg");
        } else {
            command.arg("-f").arg("bv*+ba/b");
            command.arg("--merge-output-format").arg("mp4");
            command.arg("--embed-metadata");
            command.arg("--embed-thumbnail");
            command.arg("--embed-chapters");
            command.arg("--convert-thumbnails").arg("jpg");
        }
        if spec.split_chapters {
            command.arg("--split-chapters");
        }
        command.arg("--output").arg(template);
        command.arg(item.to_url());
        let args = command
            .get_args()
            .map(|arg| format!("\"{}\"", arg.to_string_lossy()))
            .join(" ")
            .clone();
        log::debug!(args = args.as_str(); "Executing yt-dlp (stream download)");

        let out = command.output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let exit_code = out.status.code();
        log::debug!(stdout = stdout.as_str().trim(), stderr = stderr.as_str().trim(), exit_code:?; "yt-dlp finished");

        if exit_code != Some(0) {
            log::error!(stderr = stderr.as_str().trim(); "yt-dlp failed");
            return Err(YtDlpDownloadError::YtDlpError { stdout, stderr, code: exit_code });
        }

        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| !before.contains(p) && !is_thumbnail_file(p))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        files.sort();
        let Some(file_path) = files.first().cloned() else {
            return Err(YtDlpDownloadError::FileNotFound { stdout, stderr, code: exit_code });
        };
        Ok(YtDlpDownloadResult {
            file_paths: files,
            file_path,
            stderr,
            stdout,
            was_already_downloaded: false,
            exit_code,
        })
    }

    pub fn resolve_playlist_urls(
        &self,
        playlist: &YtDlpPlaylist,
    ) -> Result<Vec<YtDlpItem>, YtDlpDownloadError> {
        let mut command = Command::new("yt-dlp");
        command.arg("--print");
        command.arg("%(id)s");
        command.arg("--flat-playlist");
        command.arg("--compat-options");
        command.arg("no-youtube-unavailable-videos");
        command.arg(&playlist.id);
        let args = command
            .get_args()
            .map(|arg| format!("\"{}\"", arg.to_string_lossy()))
            .join(" ")
            .clone();
        log::debug!(args = args.as_str(); "Executing yt-dlp");

        let out = command.output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let exit_code = out.status.code();
        log::debug!(stdout = stdout.as_str().trim(), stderr = stderr.as_str().trim(), exit_code:?; "yt-dlp finished");

        if exit_code != Some(0) {
            log::error!(stderr = stderr.as_str().trim();"yt-dlp failed");
            return Err(YtDlpDownloadError::YtDlpError { stdout, stderr, code: exit_code });
        }

        Ok(stdout
            .lines()
            .map(|line| YtDlpItem {
                id: line.to_owned(),
                filename: line.to_owned(),
                kind: playlist.kind,
            })
            .collect())
    }
}

/// Make a chapter title safe to use as (part of) an output file name:
/// path separators, `:`, NUL and control characters become `_`, runs of
/// whitespace collapse to single spaces and the result is trimmed. An empty
/// result falls back to `chapter`.
/// True for the thumbnail images yt-dlp leaves next to a download
/// (`--embed-thumbnail` also writes the file; after a section download it can
/// stay on disk). A thumbnail is never the media file the caller asked for, so
/// the produced-file list must not include it.
fn is_thumbnail_file(path: &std::path::Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "webp"
        )
    })
}

fn sanitize_section_title(title: &str) -> String {
    let replaced: String = title
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '\0') || c.is_control() { '_' } else { c })
        .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() { "chapter".to_string() } else { collapsed }
}

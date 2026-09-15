use std::{cell::RefCell, collections::BTreeMap, path::PathBuf, str::FromStr};

use anyhow::{Result, bail};
use crossbeam::channel::Sender;

use crate::{
    mpd::QueuePosition,
    shared::{
        events::{PlaylistAction, WorkRequest},
        id::{self, Id},
        macros::{status_error, status_info},
        ytdlp::{
            YtDlpDownloadError, YtDlpDownloadResult,
            error::YtDlpParseError,
            ytdlp_item::{YtDlpContent, YtDlpItem},
        },
    },
};

#[derive(Debug, derive_more::Deref, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct DownloadId(Id);

impl DownloadId {
    pub fn new() -> Self {
        Self(id::new())
    }
}

#[derive(Debug)]
pub struct YtDlpManager {
    queue: RefCell<BTreeMap<DownloadId, QueuedYtDlpItem>>,
    work_sender: Sender<WorkRequest>,
}

/// Where a stream download should land and what the downloaded file(s)
/// should take the place of once complete.
#[derive(Debug, Clone)]
pub struct StreamDownloadSpec {
    /// Absolute output directory (`s2udio-downloads` in `~/Downloads`,
    /// outside the MPD library).
    pub output_dir: PathBuf,
    /// `-x` audio-only download (else the best video+audio, merged to mp4).
    pub audio_only: bool,
    /// Split the media into one file per chapter (named after the chapter
    /// title) instead of one file with chapters.
    pub split_chapters: bool,
    /// Empty = download the whole media. Non-empty = download exactly these
    /// chapter ranges, one file each (one yt-dlp run per range); `split_chapters`
    /// is then ignored.
    pub sections: Vec<ChapterSection>,
    /// What to do with the produced files when the download finishes.
    pub on_complete: ReplaceAction,
}

/// One chapter range that should become its own output file.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterSection {
    /// Range start in seconds.
    pub start_secs: f64,
    /// Range end in seconds.
    pub end_secs: f64,
    /// Chapter title; used for the output file name (the downloader sanitizes it).
    pub title: String,
}

/// The replace-the-stream behavior of a stream download.
#[derive(Debug, Clone)]
pub enum ReplaceAction {
    /// Just save the file(s) (the controls' Download button).
    None,
    /// Replace the MPD queue entry with the downloaded file(s).
    Queue { song_id: u32 },
    /// Replace the persistent video-playlist entry (by index) with the
    /// first downloaded file.
    VideoPlaylist { index: usize },
    /// Replace the entry (by URI) in a stored MPD playlist.
    Playlist { name: String, uri: String },
}

#[derive(Debug, Clone)]
pub struct QueuedYtDlpItem {
    pub state: DownloadState,
    pub add_position: Option<QueuePosition>,
    /// Round 79: start playing this item the moment it lands in the queue
    /// (the paste popup's "Import and play" sets it on the first track of
    /// a playlist).
    pub autoplay: bool,
    pub inner: YtDlpItem,
    /// The stream-download spec (None = the classic cache-dir download).
    pub spec: Option<StreamDownloadSpec>,
}

#[derive(Debug, Clone, strum::AsRefStr, strum::EnumDiscriminants, strum::Display)]
#[strum_discriminants(derive(strum::AsRefStr))]
pub enum DownloadState {
    Queued,
    Downloading,
    Completed { logs: Vec<String>, path: PathBuf },
    AlreadyDownloaded { path: PathBuf },
    Failed { logs: Vec<String> },
    Canceled,
}

impl YtDlpManager {
    pub fn new(work_sender: Sender<WorkRequest>) -> Self {
        Self { queue: RefCell::new(BTreeMap::new()), work_sender }
    }

    pub fn len(&self) -> usize {
        self.queue.borrow().len()
    }

    pub fn ids(&self) -> Vec<DownloadId> {
        self.queue.borrow().keys().copied().collect()
    }

    pub fn get(&self, id: DownloadId) -> Option<QueuedYtDlpItem> {
        self.queue.borrow().get(&id).cloned()
    }

    pub fn map_values<F, T>(&self, f: F) -> Vec<T>
    where
        F: FnMut(&QueuedYtDlpItem) -> T,
    {
        self.queue.borrow().values().map(f).collect()
    }

    pub fn download_url(
        &self,
        url: &str,
        position: Option<QueuePosition>,
    ) -> Result<(), YtDlpParseError> {
        let resolved = YtDlpContent::from_str(url)?;

        match resolved {
            YtDlpContent::Single(host) => {
                self.queue_download(host, position);
                self.download_next();
            }
            YtDlpContent::Playlist(playlist) => {
                // Round 79: the playlist link's import keeps the caller's
                // position (the CLI's `add-yt` used to drop it).
                let action = PlaylistAction::ImportToQueue { position, autoplay: false };
                if let Err(err) = self
                    .work_sender
                    .send(WorkRequest::YtDlpResolvePlaylist { playlist, action })
                {
                    status_error!(err:?; "Failed to send playlist download request");
                } else {
                    status_info!("Fetching playlist info");
                }
            }
        }

        Ok(())
    }

    /// Round 79: resolve a playlist link and act on the items it contains —
    /// import every track into the cache + queue, or save every track as a
    /// file. The popup's `[Playlist]` rows use this; `download_url` (the
    /// CLI's `add-yt` and the yt search) keeps the plain import.
    pub fn resolve_playlist(
        &self,
        url: &str,
        action: PlaylistAction,
    ) -> Result<(), YtDlpParseError> {
        match YtDlpContent::from_str(url)? {
            YtDlpContent::Playlist(playlist) => {
                if let Err(err) =
                    self.work_sender.send(WorkRequest::YtDlpResolvePlaylist { playlist, action })
                {
                    status_error!(err:?; "Failed to send playlist request");
                } else {
                    status_info!("Fetching playlist info");
                }
            }
            // A link with no playlist id (should not happen: the caller
            // checks first) is downloaded the classic way.
            YtDlpContent::Single(item) => {
                self.queue_download(item, None);
                self.download_next();
            }
        }

        Ok(())
    }

    pub fn download_next(&self) {
        for (id, item) in self.queue.borrow_mut().iter_mut() {
            match item.state {
                // First queued item found, start downloading
                DownloadState::Queued => {
                    let spec = item.spec.clone();
                    if let Err(err) = self.work_sender.send(WorkRequest::YtDlpDownload {
                        id: *id,
                        url: item.inner.clone(),
                        spec,
                    }) {
                        status_error!(err:?; "Failed to send download request");
                        break;
                    }

                    status_info!("Downloading {} from {}", item.inner.id, item.inner.kind);
                    item.state = DownloadState::Downloading;

                    // Only ever download one at a time
                    break;
                }
                // A different item is already downloading, do nothing
                DownloadState::Downloading => {
                    break;
                }
                // Noop, nothing to do with terminal states
                DownloadState::Completed { .. } => {}
                DownloadState::AlreadyDownloaded { .. } => {}
                DownloadState::Failed { .. } => {}
                DownloadState::Canceled => {}
            }
        }
    }

    pub fn redownload(&self, id: DownloadId) {
        if let Some(item) = self.queue.borrow_mut().get_mut(&id) {
            item.state = DownloadState::Queued;
        }
        self.download_next();
    }

    pub fn cancel_download(&self, id: DownloadId) {
        if let Some(item) = self.queue.borrow_mut().get_mut(&id) {
            item.state = DownloadState::Canceled;
        }
    }

    /// Round 56.6 (56.6-3): drop the entry from the Downloads list (the
    /// modal's "Remove from list"). List removal only — the downloaded /
    /// cached files are NOT deleted (`delete_cached` untouched). The
    /// caller sends the refresh (`DownloadsUpdated` + rerender) so the
    /// list updates in place.
    pub fn remove(&self, id: DownloadId) {
        self.queue.borrow_mut().remove(&id);
    }

    pub fn queue_download(&self, item: YtDlpItem, position: Option<QueuePosition>) {
        self.queue_download_with(item, position, false);
    }

    /// Round 79: queue a cache-dir download, optionally starting playback
    /// as soon as it lands in the queue.
    pub fn queue_download_with(
        &self,
        item: YtDlpItem,
        position: Option<QueuePosition>,
        autoplay: bool,
    ) {
        self.queue.borrow_mut().insert(
            DownloadId::new(),
            QueuedYtDlpItem {
                state: DownloadState::Queued,
                add_position: position,
                autoplay,
                inner: item,
                spec: None,
            },
        );
    }

    /// Queue a stream download (a `s2udio-downloads` save-as) and start it
    /// if nothing else is downloading.
    pub fn queue_stream_download(&self, item: YtDlpItem, spec: StreamDownloadSpec) {
        self.queue.borrow_mut().insert(
            DownloadId::new(),
            QueuedYtDlpItem {
                state: DownloadState::Queued,
                add_position: None,
                autoplay: false,
                inner: item,
                spec: Some(spec),
            },
        );
        self.download_next();
    }

    /// Round 79: queue a whole playlist. `autoplay` is set on the first
    /// item only — the first track that lands starts playback.
    ///
    /// `position` is also applied to the first item only: tracks land in
    /// completion order, and `add <uri> <pos>` inserts AT that index, so a
    /// position shared by the whole batch would store the playlist in
    /// reverse order (the rest append at the end, which is what a caller
    /// asking for a start position means).
    pub fn queue_download_many(
        &self,
        items: Vec<YtDlpItem>,
        position: Option<QueuePosition>,
        autoplay: bool,
    ) {
        status_info!("Queueing {} items for download", items.len());
        for (idx, item) in items.into_iter().enumerate() {
            let position = (idx == 0).then_some(position).flatten();
            self.queue_download_with(item, position, autoplay && idx == 0);
        }
    }

    pub fn resolve_download(
        &self,
        id: DownloadId,
        result: Result<YtDlpDownloadResult, YtDlpDownloadError>,
    ) -> Result<(YtDlpDownloadResult, Option<QueuePosition>, bool)> {
        if let Some(item) = self.queue.borrow_mut().get_mut(&id) {
            match result {
                Ok(result) => {
                    if result.was_already_downloaded {
                        item.state =
                            DownloadState::AlreadyDownloaded { path: result.file_path.clone() };
                        status_info!(
                            "File for {} was already downloaded, skipping download",
                            item.inner.id
                        );
                    } else {
                        item.state = DownloadState::Completed {
                            logs: Self::join_stdout_stderr(
                                &result.stdout,
                                &result.stderr,
                                result.exit_code,
                            ),
                            path: result.file_path.clone(),
                        };
                        status_info!("Downloaded {}", item.inner.id);
                    }
                    Ok((result, item.add_position, item.autoplay))
                }
                Err(YtDlpDownloadError::YtDlpError { stdout, stderr, code }) => {
                    item.state = DownloadState::Failed {
                        logs: Self::join_stdout_stderr(&stdout, &stderr, code),
                    };
                    bail!("Download failed");
                }
                Err(YtDlpDownloadError::FileNotFound { stdout, stderr, code }) => {
                    item.state = DownloadState::Failed {
                        logs: Self::join_stdout_stderr(&stdout, &stderr, code),
                    };
                    bail!("Download failed because the downloaded file was not found");
                }
                Err(YtDlpDownloadError::IoError(err)) => {
                    item.state = DownloadState::Failed { logs: vec![err.to_string()] };
                    bail!("Download failed because of IO error");
                }
                Err(YtDlpDownloadError::InvalidConfig(err)) => {
                    item.state = DownloadState::Failed { logs: vec![err.to_string()] };
                    bail!(err);
                }
            }
        } else {
            Err(anyhow::anyhow!("Download ID not found"))
        }
    }

    fn join_stdout_stderr(stdout: &str, stderr: &str, exit_code: Option<i32>) -> Vec<String> {
        let mut logs = Vec::new();
        if stdout.is_empty() && stderr.is_empty() {
            logs.push("<no output>".to_string());
            return logs;
        }
        if !stdout.is_empty() {
            logs.extend(stdout.lines().map(|line| line.to_string()));
        }

        if !stderr.is_empty() {
            logs.push(String::from("\n")); // separate stdout and stderr
            logs.extend(stderr.lines().map(|line| line.to_string()));
        }

        if let Some(code) = exit_code {
            logs.push(String::from("\n"));
            logs.push(format!("yt-dlp exited with code: {code}"));
        }

        logs
    }
}

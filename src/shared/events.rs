use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use crossterm::event::KeyEvent;
use serde::{Deserialize, Serialize};

use super::{
    ipc::ipc_stream::IpcStream,
    lrc::LrcIndex,
    mouse_event::MouseEvent,
    mpd_query::{MpdCommand, MpdQuery, MpdQueryResult, MpdQuerySync},
};
use crate::{
    config::{
        Config,
        Size,
        cli::{Command, RemoteCommandQuery},
        keys::Key,
        tabs::PaneType,
        theme::UiConfig,
    },
    mpd::{QueuePosition, commands::IdleEvent},
    shared::{
        keys::ActionEvent,
        lrc::LrcMetadata,
        ytdlp::{
            DownloadId,
            StreamDownloadSpec,
            YtDlpDownloadError,
            YtDlpDownloadResult,
            YtDlpHost,
            YtDlpItem,
            YtDlpPlaylist,
            YtDlpSearchItem,
        },
    },
    ui::{UiAppEvent, image::facade::EncodeData},
};

#[derive(Debug)]
#[allow(unused)]
pub(crate) enum ClientRequest {
    Query(MpdQuery),
    QuerySync(MpdQuerySync),
    Command(MpdCommand),
}

#[allow(unused)]
pub(crate) enum WorkRequest {
    IndexLyrics {
        lyrics_dir: String,
    },
    IndexSingleLrc {
        /// Absolute path to the lrc file
        path: PathBuf,
    },
    SearchYt {
        query: String,
        kind: YtDlpHost,
        limit: usize,
        interactive: bool,
        position: Option<QueuePosition>,
    },
    YtDlpDownload {
        id: DownloadId,
        url: YtDlpItem,
        /// The stream-download spec (None = the classic cache-dir
        /// download from a search).
        spec: Option<StreamDownloadSpec>,
    },
    YtDlpResolvePlaylist {
        playlist: YtDlpPlaylist,
    },
    /// Fetch the radio-browser.info station directory on the work thread;
    /// the result is delivered to the Radio pane like an MPD query result.
    FetchRadioDirectory {
        location: Option<crate::config::radio::GeoLocation>,
        cache_dir: Option<PathBuf>,
    },

    /// Resolve one or more YouTube/Soundcloud/NicoVideo URLs to direct
    /// stream URLs via `yt-dlp -g`, so MPD can play them without a
    /// download/cache step.
    ResolveYtStreams {
        urls: Vec<String>,
        /// What to do with the resolved streams once they are known.
        action: crate::ui::modals::paste::YtAction,
    },

    /// Fetch the Jellyfin library tree (views of the server).
    FetchJellyfinViews,
    /// Children of a Jellyfin folder/view (non-music views).
    FetchJellyfinFolder {
        parent_id: String,
    },
    /// Artists of a music library view.
    FetchJellyfinArtists {
        view_id: String,
    },
    /// Albums of an artist.
    FetchJellyfinAlbums {
        artist_id: String,
    },
    /// Songs of an album.
    FetchJellyfinSongs {
        album_id: String,
    },
    /// Metadata of a single item (for the mpv now-playing info).
    FetchJellyfinItem {
        item_id: String,
    },
    /// The saved resume position of an item.
    FetchJellyfinResume {
        item_id: String,
    },
    /// The primary image (poster / episode preview) of an item, with an
    /// optional fallback — e.g. the series poster when a season has no
    /// image of its own. The result reports `item_id`, so the caller can
    /// match it to the selection regardless of which fetch succeeded.
    FetchJellyfinImage {
        item_id: String,
        fallback_item_id: Option<String>,
    },
    /// The primary image of the video currently playing in mpv, shown as
    /// the album art while the video plays.
    FetchJellyfinVideoArt {
        item_id: String,
    },
    /// Download a YouTube video's thumbnail so it can be shown as album art
    /// while its audio stream plays.
    FetchYtThumbnail {
        url: String,
    },
    /// Download a thumbnail to `<cache_dir>/mpris-art` for the MPRIS bridge
    /// (mpDris2 serves it as the album art of the current stream).
    SaveMprisArt {
        url: String,
    },
    /// Download a YouTube video's thumbnail to the mpv-mpris poster file so
    /// the MPRIS bridge shows it while the video plays in mpv.
    SaveMpvMprisArt {
        url: String,
        cache_dir: Option<PathBuf>,
    },
    /// Fetch a Jellyfin item's metadata + primary image so the MPRIS bridge
    /// can show the episode/movie title and thumbnail for a playing stream.
    FetchJellyfinMpris {
        item_id: String,
    },
    /// Chapter markers of a Jellyfin item (`Fields=Chapters`).
    FetchJellyfinChapters {
        item_id: String,
    },
    /// An episode's whole season, fetched so mpv can play it as a playlist
    /// starting at the clicked episode.
    FetchJellyfinSeason {
        season_id: String,
        episode_id: String,
    },
    /// Chapter markers of a local file (via ffprobe).
    FetchFileChapters {
        file: String,
    },

    /// States/provinces of a country, for the Radio pane's country groups.
    FetchRadioStates {
        /// Country name (for routing the result back).
        country: String,
        /// ISO country code (for the state-list query).
        country_code: Option<String>,
    },
    /// The top-100 most-voted stations of a country, to complete or refresh
    /// a region's cached station list.
    FetchRadioCountryStations {
        country: String,
        country_code: String,
    },
    /// All stations of a state/province, for the Radio pane's state groups.
    FetchRadioStateStations {
        country: String,
        state: String,
    },
    Command(Command),
    ResizeImage(Box<dyn FnOnce() -> Result<EncodeData> + Send + Sync>),
    /// Prepare a torrent stream (M2): start the rqbit engine, add the
    /// torrent, pick the largest playable file and build the mpv stream
    /// URL. Runs on the work thread; the engine is moved back to the UI
    /// (`WorkDone::TorrentStreamPrepared`) so it stays alive.
    PlayTorrent {
        item: crate::core::torrent::TorrentItem,
        /// `true` for the "Play and Download" action: after the stream
        /// starts, the engine keeps downloading the torrent and the
        /// completed file is moved to `s2udio-downloads`.
        download: bool,
    },
    /// Prepare a torrent for download-only (round 21, the fresh-engine
    /// fallback of the popup's "Download" / "Download all" when the scan
    /// is gone): start the engine, add the torrent, wait for its file
    /// list and hand the running engine + chosen files back as
    /// `WorkDone::TorrentDownloadPrepared` — no stream playback.
    DownloadTorrent {
        item: crate::core::torrent::TorrentItem,
        /// The files to keep, as positional indices (empty = the single
        /// best playable file).
        indices: Vec<usize>,
    },
    /// Scan a pasted torrent/magnet (round 17): start the engine, add the
    /// torrent, wait (round 18: **open-ended**, no deadline) for its
    /// metainfo and hand the running engine + file list back as
    /// `WorkDone::TorrentScanned`. The popup shows a live wait window
    /// (elapsed counter + DL-speed / needed-speed check, refreshed by
    /// `WorkDone::TorrentScanProgress`) until the scan lands, then offers
    /// the play actions driven by the file list (single-file vs
    /// multi-video). The scan runs on its own thread.
    ScanTorrent {
        item: crate::core::torrent::TorrentItem,
        /// Per-scan cancel signal (round 18): the paste popup's close hook
        /// fires it when the user dismisses the popup, so the scan thread
        /// stops waiting and drops the engine promptly (no orphan rqbit).
        cancel: crossbeam::channel::Receiver<()>,
    },
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // the instances are short lived events, its fine.
pub(crate) enum WorkDone {
    LyricsIndexed {
        index: LrcIndex,
    },
    SingleLrcIndexed {
        path: PathBuf,
        metadata: Option<LrcMetadata>,
    },
    MpdCommandFinished {
        id: &'static str,
        target: Option<PaneType>,
        data: MpdQueryResult,
    },
    ImageResized {
        data: Result<EncodeData>,
    },
    /// The resolved direct stream info (URL, title, thumbnail,
    /// description) for a pasted YouTube-style link.
    YtStreamsResolved {
        info: Vec<crate::shared::ytdlp::YtStreamInfo>,
        action: crate::ui::modals::paste::YtAction,
        /// URLs that could not be resolved.
        failures: Vec<String>,
    },
    /// Result of a Jellyfin API fetch, routed to the Jellyfin pane.
    JellyfinFetched {
        id: &'static str,
        data: crate::jellyfin::JellyfinResult,
    },
    SearchYtResults {
        items: Vec<YtDlpSearchItem>,
        position: Option<QueuePosition>,
        interactive: bool,
    },
    YtDlpPlaylistResolved {
        urls: Vec<YtDlpItem>,
    },
    YtDlpDownloaded {
        id: DownloadId,
        result: Result<YtDlpDownloadResult, YtDlpDownloadError>,
        /// The stream-download spec the request was queued with (None for
        /// classic cache-dir downloads).
        spec: Option<StreamDownloadSpec>,
    },
    /// A torrent stream is prepared (M2): the rqbit engine is running and
    /// `stream_url` is ready for mpv. The engine handle is moved to the UI
    /// (kept in `Ctx.torrent_engine` — its `Drop` kills rqbit when the app
    /// exits; M4 replaces this with the full session lifecycle).
    TorrentStreamPrepared {
        /// The item's canonical scan key (round 20): the UI registers the
        /// prepared single-file scan under it so a repeat paste reuses the
        /// engine instead of spawning a second rqbit on the same cache.
        key: String,
        /// The running engine, kept alive for the whole mpv session.
        engine: crate::core::torrent::TorrentEngine,
        /// The mpv stream URL
        /// (`http://127.0.0.1:<port>/torrents/<id>/stream/<file_idx>`).
        stream_url: String,
        /// The torrent's display name (rqbit details, or the item label).
        torrent_name: String,
        /// The picked file's name (used as the mpv playlist title).
        file_name: String,
        /// The engine's torrent id (the "Play and Download" job polls its
        /// stats and deletes it once the file is kept).
        torrent_id: String,
        /// The picked file's positional index in the torrent's file list
        /// (completion check against `stats.file_progress`).
        file_idx: usize,
        /// The picked file's length in bytes (completion check).
        file_length: u64,
        /// `true` when the action was "Play and Download" (the UI keeps a
        /// download job polling until the file is moved to
        /// `s2udio-downloads`).
        download: bool,
    },
    /// A torrent is prepared for download-only (round 21): the engine is
    /// running and the file list is known — the UI registers the scan
    /// under the item's canonical key and starts a download job for the
    /// chosen files (no playback; the fresh-engine fallback of the
    /// popup's "Download" / "Download all").
    TorrentDownloadPrepared {
        /// The item's canonical scan key (round 20): the UI registers the
        /// prepared download as a scan under it so a repeat paste reuses
        /// the engine instead of spawning a second rqbit on the same cache.
        key: String,
        /// The running engine, kept alive for the whole download job.
        engine: crate::core::torrent::TorrentEngine,
        /// The engine's torrent id (stats + delete).
        torrent_id: String,
        /// The torrent's display name (rqbit details, or the item label).
        torrent_name: String,
        /// The files to keep in `s2udio-downloads`, in scan order.
        files: Vec<crate::core::torrent::ScannedFile>,
    },
    /// A torrent/magnet scan finished (round 17). The UI stores the scan
    /// (engine + torrent id + file list) in `Ctx.torrent_scans` keyed by
    /// the item and refreshes the paste popup: the `[Torrent]` section
    /// swaps its "Loading…" row for the play actions the scan enables.
    TorrentScanned {
        /// The item's raw source string (the magnet URI or the `.torrent`
        /// path/URL) — the key `Ctx.torrent_scans` is indexed by (the
        /// popup renders the item's own label).
        key: String,
        /// The scan outcome: the running engine + torrent id + file list,
        /// or the error (dead magnet, missing engine binary, …).
        result: Result<crate::core::torrent::TorrentScan, String>,
    },
    /// A torrent scan's live progress (round 18): one event per second
    /// while the scan waits for a magnet's metainfo. The paste popup's
    /// wait window renders the elapsed counter and the DL-speed /
    /// needed-speed check (✓/✗) from it, refreshing with each event.
    TorrentScanProgress {
        /// The item's raw source string — the same key as `TorrentScanned`.
        key: String,
        /// Elapsed seconds since the scan started + the engine's live
        /// download speed (KB/s).
        progress: crate::core::torrent::TorrentScanProgress,
    },
    None,
}

// The instances are short lived events, boxing would most likely only hurt
// here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum AppEvent {
    UserKeyInput(KeyEvent),
    UserMouseInput(MouseEvent),
    /// A bracketed-paste arrived (middle click, drag&drop, or Ctrl+V). The
    /// UI parses it for audio files/URLs and offers the play/enqueue popup.
    UserPaste(String),
    /// mpv was launched from s2udio; the now-playing UI switches to the
    /// video session.
    MpvSessionStarted {
        url: String,
    },
    /// mpv exited; the video session is over (MPD resumes if it was paused).
    MpvSessionEnded,
    /// Periodic tick while an mpv video plays: read its state from the IPC
    /// socket and refresh the UI.
    MpvPoll,
    /// Periodic tick while a "Play and Download" torrent job is active:
    /// poll the engine's stats and move the completed file to
    /// `s2udio-downloads` once it is done.
    TorrentDownloadPoll,
    /// The paste popup's play action on an already-scanned torrent (round
    /// 17): the engine is running and the file list is known, so playback
    /// starts directly in the event loop (which owns the download job's
    /// scheduler guard) instead of re-scanning on the work thread.
    TorrentScannedPlay {
        /// The scanned torrent (the engine moves here from the scan map;
        /// `Ctx.torrent_engine` keeps it alive once playing).
        scan: crate::core::torrent::TorrentScan,
        /// The files to play, as indices into `scan.files`, in play order
        /// ("Play (stream)" = the single picked file, "Play all" = every
        /// video, "Select files…" = the user's choice).
        file_indices: Vec<usize>,
        /// `true` for the "Play and Download" / picker "Download & Play"
        /// actions (the download job tracks every played file).
        download: bool,
    },
    /// The paste popup's download-only action on an already-scanned
    /// torrent (round 21): keep the scanned engine running and move the
    /// chosen files to `s2udio-downloads` once the torrent's download is
    /// complete — no playback (unlike `TorrentScannedPlay { download: true }`).
    TorrentScannedDownload {
        /// The scanned torrent (the engine is Arc-shared with the scan map).
        scan: crate::core::torrent::TorrentScan,
        /// The files to keep, as indices into `scan.files` ("Download" =
        /// the single picked file, "Download all" = every video).
        file_indices: Vec<usize>,
    },
    KeyTimeout,
    ActionResolved(ActionEvent),
    InsertModeFlush((Option<ActionEvent>, Vec<Key>)),
    Status(String, Level, Duration),
    InfoModal {
        message: Vec<String>,
        title: Option<String>,
        size: Option<Size>,
        replacement_id: Option<String>,
    },
    Log(Vec<u8>),
    IdleEvent(IdleEvent),
    RequestRender,
    Resized {
        columns: u16,
        rows: u16,
    },
    ResizedDebounced {
        columns: u16,
        rows: u16,
    },
    WorkDone(Result<WorkDone>),
    UiEvent(UiAppEvent),
    /// Fired every second by the blur-schedule watcher; applies the active
    /// mode's colors when the schedule changed.
    BlurCheck,
    Reconnected,
    LostConnection,
    TmuxHook {
        hook: String,
    },
    ConfigChanged {
        config: Box<Config>,
        keep_old_theme: bool,
    },
    ThemeChanged {
        theme: Box<UiConfig>,
    },
    RemoteSwitchTab {
        tab_name: String,
    },
    IpcQuery {
        stream: IpcStream,
        targets: Vec<RemoteCommandQuery>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy, Eq, Hash, PartialEq)]
#[allow(dead_code)]
pub enum Level {
    Trace,
    Debug,
    Warn,
    Error,
    Info,
}

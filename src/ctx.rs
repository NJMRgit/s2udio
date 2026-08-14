use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ops::AddAssign,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use bon::bon;
use crossbeam::channel::{SendError, Sender, bounded};

use crate::{
    AppEvent,
    MpdCommand,
    MpdQuery,
    MpdQueryResult,
    WorkRequest,
    config::{
        Config,
        album_art::ImageMethod,
        tabs::{PaneType, TabName},
    },
    core::scheduler::{Scheduler, time_provider::DefaultTimeProvider},
    mpd::{
        client::Client,
        commands::{Song, State, Status},
        mpd_client::MpdClient,
        version::Version,
    },
    shared::{
        events::ClientRequest,
        keys::KeyResolver,
        lrc::{Lrc, LrcIndex},
        macros::{status_error, status_warn},
        mpd_client_ext::MpdClientExt,
        mpd_query::MpdQuerySync,
        ring_vec::RingVec,
        ytdlp::YtDlpManager,
    },
    ui::{StatusMessage, input::InputManager},
};

/// The sub-tab shown in the Queue tab's list area: the MPD queue (Audio),
/// the mpv video playlist (Video), or the current track's chapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueTabMode {
    Audio,
    Video,
    Chapters,
}

pub const FETCH_SONG_STICKERS: &str = "fetch_song_stickers";
pub const LIKE_STICKER: &str = "like";
pub const RATING_STICKER: &str = "rating";

/// Choose the tab the app should open on: the tab matching the currently
/// playing media (a stream URI -> Radio, otherwise Local), or the last tab
/// that was used when nothing is playing.
fn initial_tab(
    config: &Config,
    status: &Status,
    queue: &[Song],
    yt_streams: &HashSet<String>,
) -> TabName {
    initial_tab_with(
        config,
        status,
        queue,
        crate::config::state::AppStateFile::load().last_tab.as_deref(),
        yt_streams,
    )
}

/// The pure core of [`initial_tab`]; `last_tab` is the persisted tab name
/// (None when the state file is absent), `yt_streams` the restored stream
/// URLs of resolved YouTube links (they are queue content, not radio).
fn initial_tab_with(
    config: &Config,
    status: &Status,
    queue: &[Song],
    last_tab: Option<&str>,
    yt_streams: &HashSet<String>,
) -> TabName {
    let fallback = || -> TabName {
        config
            .tabs
            .names
            .iter()
            .find(|name| !config.is_tab_hidden(name))
            .cloned()
            .or_else(|| config.tabs.names.first().cloned())
            .unwrap_or_else(|| TabName::from("Queue"))
    };

    if status.state != State::Stop {
        if let Some(song) = status.songid.and_then(|id| queue.iter().find(|s| s.id == id)) {
            // A playing radio stream opens the Radio tab; a resolved
            // YouTube stream is queue content and opens the Queue tab.
            let kind = if crate::ui::panes::radio::is_stream_url(&song.file)
                && !yt_streams.contains(&song.file)
            {
                "Radio"
            } else {
                "Queue"
            };
            if let Some(tab) = config
                .tabs
                .names
                .iter()
                .find(|name| {
                    name.as_str().eq_ignore_ascii_case(kind) && !config.is_tab_hidden(name)
                })
            {
                return tab.clone();
            }
        }
    }

    if let Some(last) = last_tab {
        if let Some(tab) = config
            .tabs
            .names
            .iter()
            .find(|name| name.as_str().eq_ignore_ascii_case(last) && !config.is_tab_hidden(name))
        {
            return tab.clone();
        }
    }

    fallback()
}

#[derive(derive_more::Debug)]
pub struct Ctx {
    pub(crate) mpd_version: Version,
    pub(crate) config: std::sync::Arc<Config>,
    pub(crate) status: Status,
    pub(crate) queue: Vec<Song>,
    #[cfg(test)]
    pub(crate) stickers: HashMap<String, HashMap<String, String>>,
    #[cfg(not(test))]
    stickers: HashMap<String, HashMap<String, String>>,
    pub(crate) active_tab: TabName,
    pub(crate) supported_commands: HashSet<String>,
    /// The id of the song currently selected in the queue pane (kept in sync
    /// by the queue's render); the lyrics pane shows its info when paused.
    pub(crate) queue_selected_id: Cell<Option<u32>>,
    /// Lyrics edit mode is on (round 35): the lyrics pane sets it when the
    /// pencil toggles edit mode; the cava pane swaps the visualizer for an
    /// edit-controls legend while it is set.
    pub(crate) lyrics_edit_mode: Cell<bool>,
    /// Id of a file played via the Directories pane's right arrow /
    /// double-click (a temporary queue entry). The queue pane hides it from
    /// the list; the directories pane clears it when the entry is dropped.
    pub(crate) temp_play_id: Cell<Option<u32>>,
    pub(crate) db_update_start: Option<Instant>,
    #[debug(skip)]
    pub(crate) app_event_sender: Sender<AppEvent>,
    #[debug(skip)]
    pub(crate) work_sender: Sender<WorkRequest>,
    #[debug(skip)]
    pub(crate) client_request_sender: Sender<ClientRequest>,
    pub(crate) needs_render: Cell<bool>,
    /// True while a terminal resize is in progress (the event loop's 500 ms
    /// debounce window). The cava pane skips geometry restarts during this
    /// window: with the pipewire/pulse input methods a restart makes the USB
    /// DAC renegotiate its ALSA period (an audible dropout), and the
    /// debounced resize that follows handles the final geometry anyway.
    pub(crate) resizing: Cell<bool>,
    pub(crate) stickers_to_fetch: RefCell<HashSet<String>>,
    #[debug(skip)]
    pub(crate) lrc_index: LrcIndex,
    pub(crate) rendered_frames: u64,
    #[debug(skip)]
    pub(crate) scheduler: Scheduler<(Sender<AppEvent>, Sender<ClientRequest>), DefaultTimeProvider>,
    pub(crate) messages: RingVec<10, StatusMessage>,
    pub(crate) last_status_update: Instant,
    pub(crate) song_played: Option<Duration>,
    /// The last song id whose metadata/chapters were processed by
    /// `ensure_chapters` + `ensure_mpris_metadata`. The status-update
    /// handler sets it when the song-change pipeline runs; the
    /// queue-update handler re-runs the pipeline when a song id appears
    /// in a refreshed queue that the status handler never saw (a
    /// ReplaceAndPlay re-resolution changes MPD's song id between the
    /// status and queue updates, so the song-change check misses it).
    pub(crate) metadata_processed_song: Option<u32>,
    pub(crate) stickers_supported: StickersSupport,
    pub(crate) input: InputManager,
    pub(crate) key_resolver: KeyResolver,
    pub(crate) ytdlp_manager: YtDlpManager,
    pub(crate) cached_queue_time_total: Duration,
    /// State of an mpv video launched from s2udio (empty/inactive when only
    /// MPD is playing). The now-playing bar, seekbar and transport controls
    /// route to mpv while it is active.
    pub(crate) mpv: crate::core::mpv::MpvSession,
    /// Resolved stream URL -> video info (title/thumbnail/description) for
    /// YouTube-style links played as audio through MPD (the stream URL
    /// itself has no metadata, so the controls, album art and info box look
    /// the info up when the playing song matches).
    pub(crate) yt_info: RefCell<HashMap<String, crate::shared::ytdlp::YtStreamInfo>>,
    /// Song file -> chapter markers (YouTube videos, Jellyfin items, local
    /// files with embedded chapters). Shown in the Queue tab via the
    /// Queue / Chapters toggle.
    pub(crate) chapters: RefCell<HashMap<String, Vec<crate::shared::chapters::Chapter>>>,
    /// Whether the Queue tab shows the chapter list instead of the queue.
    pub(crate) queue_tab: Cell<QueueTabMode>,
    /// The persistent video playlist (the Queue tab's Video list). Unlike
    /// the mpv session's own playlist it survives mpv closing and audio
    /// playback, and is saved to `<cache_dir>/video-playlist.json` so it
    /// survives restarts too.
    pub(crate) video_playlist: RefCell<Vec<crate::core::mpv::MpvPlaylistEntry>>,
    /// Width of the queue table area (set by the QueuePane's layout pass),
    /// so the QueueHeaderPane's chapters header (Chapter | Time | Duration)
    /// renders into the same width and its labels sit exactly above the
    /// chapter list values.
    pub(crate) queue_table_width: Cell<Option<u16>>,
    /// The custom mpv subtitle language picked in the Settings -> mpv
    /// language picker; consumed by the settings panel on the next render.
    pub(crate) mpv_custom_subtitle_lang: std::cell::RefCell<Option<String>>,
    /// The custom mpv audio language picked in the Settings -> mpv language
    /// picker; consumed by the settings panel on the next render.
    pub(crate) mpv_custom_audio_lang: std::cell::RefCell<Option<String>>,
    /// The last mouse position (cell coordinates) reported by the terminal.
    /// Mouseover effects (buttons, toggles, list rows) read it during render;
    /// `None` when the pointer left the window / was never reported. This is
    /// the *pane* position: while a modal is open it is forced to `None` so
    /// hover effects never paint on the UI behind the popup.
    pub(crate) mouse_pos: Cell<Option<ratatui::layout::Position>>,
    /// The pointer position while a modal is open. Modal/popup content (e.g.
    /// context-menu rows, settings sidebar) reads this for its own hover
    /// effects, so the popup still lights up under the cursor while the
    /// panes behind it do not.
    pub(crate) modal_mouse_pos: Cell<Option<ratatui::layout::Position>>,
    /// Keyboard-control state of the seekbar (entered with Ctrl+Tab on the
    /// Queue tab). While focused, the seekbar owns the keyboard: arrows /
    /// a/d move the seek cursor, Space or Enter seeks and returns control.
    #[debug(skip)]
    pub(crate) seekbar: RefCell<crate::ui::seekbar::SeekbarState>,
    /// The running rqbit engine for torrent streaming (None until a
    /// torrent is played). Set from the work thread's
    /// `WorkDone::TorrentStreamPrepared` and the scanned play path; a
    /// clone of the engine's `Arc` (the scan map keeps another, so a
    /// repeat paste of the same torrent reuses it — round 20). The engine
    /// stays alive for the mpv session, and the last `Arc`'s `Drop` kills
    /// the rqbit child when the app exits. M4 replaces this with the full
    /// torrent session (keep/cleanup policy).
    #[debug(skip)]
    pub(crate) torrent_engine:
        RefCell<Option<std::sync::Arc<crate::core::torrent::TorrentEngine>>>,
    /// The active "Play and Download" job (None unless the user picked
    /// that action): the event loop polls the engine's stats once per
    /// second and moves the completed file to `s2udio-downloads`.
    #[debug(skip)]
    pub(crate) torrent_download: RefCell<Option<crate::core::torrent::TorrentDownload>>,
    /// Scanned torrents (round 17): item source key -> the scan outcome
    /// (the running engine + torrent id + file list, or the failure the
    /// popup shows as a dim notice). The paste popup's `[Torrent]` section
    /// is driven by this map ("Loading…" until a scan lands, then the play
    /// actions); play actions reuse the scanned engine instead of spawning
    /// a fresh rqbit. Cleared when the paste modal closes (engines killed
    /// via `Drop`).
    #[debug(skip)]
    pub(crate)
        torrent_scans: RefCell<HashMap<String, Result<crate::core::torrent::TorrentScan, String>>>,
    /// Item source keys whose scan is still in flight (so reopening the
    /// paste modal does not re-scan; removed when `TorrentScanned` lands).
    pub(crate) torrent_scans_pending: RefCell<HashSet<String>>,
    /// Round 18: per-item cancel signals for in-flight torrent scans (item
    /// source key -> the scan thread's cancel sender). The paste popup's
    /// close hook fires every entry when the user dismisses the popup, so
    /// the scan threads stop waiting and drop their engines promptly (no
    /// background leak); entries are removed when the scan lands.
    #[debug(skip)]
    pub(crate) torrent_scan_cancels:
        RefCell<HashMap<String, crossbeam::channel::Sender<()>>>,
    /// Round 18: the latest progress of each in-flight torrent scan (item
    /// source key -> elapsed seconds + live download speed), rendered by
    /// the paste popup's wait window. Cleared when the popup closes.
    #[debug(skip)]
    pub(crate) torrent_scan_progress:
        RefCell<HashMap<String, crate::core::torrent::TorrentScanProgress>>,
    /// The items of the currently open paste popup (None when no paste
    /// popup is open). Set by `show_paste_modal`, cleared on close; the
    /// scan-completion handler refreshes the popup only while it is set.
    pub(crate) paste_modal_items: RefCell<Option<Vec<crate::ui::modals::paste::PastedItem>>>,
    /// The open paste popup's modal id (so a nested flow — e.g. the
    /// "Select files…" picker — can close it once playback starts;
    /// `PopModal` drops the modal without running its close hook, so the
    /// caller also clears the scan state itself).
    pub(crate) paste_modal_id: Cell<Option<crate::shared::id::Id>>,
}

#[bon]
impl Ctx {
    pub(crate) fn try_new(
        client: &mut Client<'_>,
        mut config: Config,
        app_event_sender: Sender<AppEvent>,
        work_sender: Sender<WorkRequest>,
        client_request_sender: Sender<ClientRequest>,
        mut scheduler: Scheduler<(Sender<AppEvent>, Sender<ClientRequest>), DefaultTimeProvider>,
    ) -> Result<Self> {
        let supported_commands: HashSet<String> = client.supported_commands.clone();
        let stickers_supported = if supported_commands.contains("sticker") {
            StickersSupport::Supported
        } else {
            StickersSupport::Unsupported
        };
        log::info!(supported_commands:? = supported_commands; "Supported commands by server");

        let status = client.get_status()?;
        let queue = client.playlist_info()?.unwrap_or_default();
        let cached_queue_time_total = queue.iter().filter_map(|s| s.duration).sum();

        if !supported_commands.contains("albumart") || !supported_commands.contains("readpicture") {
            config.album_art.method = ImageMethod::None;
            status_warn!("Album art is disabled because it is not supported by MPD");
        }

        log::info!(config:? = config; "Resolved config");

        let key_resolver = KeyResolver::new(&config);

        // Open on the tab matching the currently playing media (radio stream
        // -> Radio tab, local file -> Local tab); with nothing playing, the
        // last tab that was used is restored. Persist the choice so a later
        // empty start remembers it.
        // Restore the cached YouTube stream info first: it decides whether a
        // still-playing stream is queue content (YouTube) or radio.
        let yt_info = crate::ui::modals::paste::load_yt_cache(config.cache_dir.as_deref());
        let yt_streams: HashSet<String> = yt_info.keys().cloned().collect();
        let active_tab = initial_tab(&config, &status, &queue, &yt_streams);
        // Merge with the previous state so persisted preferences (e.g. the
        // video playback choice) survive this startup write.
        let mut state = crate::config::state::AppStateFile::load();
        state.last_tab = Some(active_tab.to_string());
        state.mpd_library_path = None;
        let _ = state.save();
        scheduler.start();
        let ctx = Self {
            ytdlp_manager: YtDlpManager::new(work_sender.clone()),
            mpd_version: client.version(),
            lrc_index: LrcIndex::default(),
            config: std::sync::Arc::new(config),
            status,
            queue,
            stickers: HashMap::new(),
            active_tab,
            supported_commands,
            queue_selected_id: Cell::new(None),
            lyrics_edit_mode: Cell::new(false),
            temp_play_id: Cell::new(None),
            db_update_start: None,
            app_event_sender,
            work_sender,
            scheduler,
            client_request_sender,
            needs_render: Cell::new(false),
            resizing: Cell::new(false),
            stickers_to_fetch: RefCell::new(HashSet::new()),
            rendered_frames: 0,
            messages: RingVec::default(),
            song_played: None,
            metadata_processed_song: None,
            last_status_update: Instant::now(),
            stickers_supported,
            input: InputManager::default(),
            key_resolver,
            cached_queue_time_total,
            mpv: crate::core::mpv::MpvSession::default(),
            yt_info: RefCell::new(yt_info),
            chapters: RefCell::new(HashMap::new()),
            queue_tab: Cell::new(QueueTabMode::Audio),
            video_playlist: RefCell::new(Vec::new()),
            queue_table_width: Cell::new(None),
            mpv_custom_subtitle_lang: std::cell::RefCell::new(None),
            mpv_custom_audio_lang: std::cell::RefCell::new(None),
            mouse_pos: Cell::new(None),
            modal_mouse_pos: Cell::new(None),
            seekbar: RefCell::new(crate::ui::seekbar::SeekbarState::default()),
            torrent_engine: RefCell::new(None),
            torrent_download: RefCell::new(None),
            torrent_scans: RefCell::new(HashMap::new()),
            torrent_scans_pending: RefCell::new(HashSet::new()),
            torrent_scan_cancels: RefCell::new(HashMap::new()),
            torrent_scan_progress: RefCell::new(HashMap::new()),
            paste_modal_items: RefCell::new(None),
            paste_modal_id: Cell::new(None),
        };
        // A previously resolved YouTube stream that is still playing: kick
        // off a background re-resolution so the info is refreshed, and
        // restore its chapters from the cache.
        if let Some((_, song)) = ctx.find_current_song_in_queue() {
            if let Some(entry) = ctx.yt_info.borrow().get(&song.file) {
                if !entry.original_url.is_empty() {
                    let _ = ctx.work_sender.send(WorkRequest::ResolveYtStreams {
                        urls: vec![entry.original_url.clone()],
                        action: crate::ui::modals::paste::YtAction::Refresh,
                    });
                }
                if !entry.chapters.is_empty() {
                    ctx.chapters
                        .borrow_mut()
                        .insert(song.file.clone(), entry.chapters.clone());
                }
            }
        }
        crate::ui::modals::paste::ensure_chapters(&ctx);
        crate::ui::modals::paste::ensure_mpris_metadata(&ctx);
        // Restore the chapters of every cached stream, keyed by the
        // resolved URL and the original link: an mpv session playing the
        // link (or the queue song playing the stream) finds its markers
        // after a restart.
        {
            let mut chapters = ctx.chapters.borrow_mut();
            for entry in ctx.yt_info.borrow().values() {
                if entry.chapters.is_empty() {
                    continue;
                }
                chapters.insert(entry.url.clone(), entry.chapters.clone());
                if !entry.original_url.is_empty() && entry.original_url != entry.url {
                    chapters.insert(entry.original_url.clone(), entry.chapters.clone());
                }
            }
        }
        // Restore the persistent video playlist (the Video list survives
        // mpv closing, audio playback and restarts).
        if let Ok(content) = std::fs::read_to_string(
            crate::ui::modals::paste::video_playlist_path(ctx.config.cache_dir.as_deref()),
        ) && let Ok(entries) =
            serde_json::from_str::<Vec<crate::core::mpv::MpvPlaylistEntry>>(&content)
        {
            *ctx.video_playlist.borrow_mut() =
                entries.into_iter().filter(|e| !e.url.is_empty()).collect();
            log::debug!(count = ctx.video_playlist.borrow().len(); "Restored the video playlist");
        }
        // A still-playing track with chapter markers opens straight into the
        // chapters view (the Queue tab is already the startup tab for
        // YouTube streams); gated by the auto-chapters setting.
        if ctx.config.ui.auto_show_chapters
            && ctx.find_current_song_in_queue().is_some_and(|(_, song)| {
                !ctx.chapters.borrow().get(&song.file).is_none_or(Vec::is_empty)
            })
        {
            ctx.queue_tab.set(QueueTabMode::Chapters);
        }
        Ok(ctx)
    }

    // TODO: Error comes from crossebeam, try to remove later if it gets solved
    // upstream
    #[allow(clippy::result_large_err)]
    pub(crate) fn render(&self) -> Result<(), SendError<AppEvent>> {
        if self.needs_render.get() {
            return Ok(());
        }

        self.needs_render.replace(true);
        self.app_event_sender.send(AppEvent::RequestRender)
    }

    /// The cell the pointer is over, `None` when it left the window.
    pub(crate) fn mouse_pos(&self) -> Option<ratatui::layout::Position> {
        self.mouse_pos.get()
    }

    pub(crate) fn set_mouse_pos(&self, pos: Option<ratatui::layout::Position>) {
        self.mouse_pos.set(pos);
    }

    /// The pointer position used by modals for their own hover effects
    /// (separate from `mouse_pos`, which is suppressed while a modal is
    /// open so the popup never highlights the UI behind it).
    pub(crate) fn modal_mouse_pos(&self) -> Option<ratatui::layout::Position> {
        self.modal_mouse_pos.get()
    }

    pub(crate) fn set_modal_mouse_pos(&self, pos: Option<ratatui::layout::Position>) {
        self.modal_mouse_pos.set(pos);
    }

    pub(crate) fn finish_frame(&mut self) {
        self.needs_render.replace(false);
        self.rendered_frames.add_assign(1);

        let stickers = self.stickers_to_fetch.take();
        if !stickers.is_empty() {
            match self.stickers_supported {
                StickersSupport::Unsupported => {
                    self.stickers_supported = StickersSupport::UnsupportedAndChecked;
                    // Shoot a dummy sticker request to MPD to see what error we get to determine
                    // what exactly is wrong.
                    self.command(|client| {
                        if let Err(err) = client.sticker("", "test") {
                            status_error!(
                                "Stickers are not supported by MPD server: '{}'",
                                err.detail_or_display()
                            );
                        } else {
                            status_error!("Stickers are not supported by MPD server");
                        }
                        Ok(())
                    });
                }
                StickersSupport::UnsupportedAndChecked => {}
                StickersSupport::Supported => {
                    let uris = stickers.into_iter().collect();
                    log::debug!(uris:?; "Fetching stickers after frame");
                    self.query().id(FETCH_SONG_STICKERS).replace_id(FETCH_SONG_STICKERS).query(
                        |client| {
                            let stickers = client.fetch_song_stickers(uris)?;
                            Ok(MpdQueryResult::SongStickers(stickers))
                        },
                    );
                }
            }
        }
    }

    pub(crate) fn query_sync<T: Send + Sync + 'static>(
        &self,
        on_done: impl FnOnce(&mut Client<'_>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = bounded(1);
        let query = MpdQuerySync {
            callback: Box::new(|client| Ok(MpdQueryResult::Any(Box::new((on_done)(client)?)))),
            tx,
        };

        if let Err(err) = self.client_request_sender.send(ClientRequest::QuerySync(query)) {
            log::error!(error:? = err; "Failed to send query request");
            bail!("Failed to send sync query request");
        }

        if let MpdQueryResult::Any(any) = rx.recv()? {
            if let Ok(val) = any.downcast::<T>() {
                return Ok(*val);
            }
            bail!("Received unknown type answer for sync query request",);
        }

        bail!("Received unknown MpdQueryResult for sync query request");
    }

    #[builder(finish_fn(name = query))]
    pub(crate) fn query(
        &self,
        #[builder(finish_fn)] on_done: impl FnOnce(&mut Client<'_>) -> Result<MpdQueryResult>
        + Send
        + 'static,
        id: &'static str,
        target: Option<PaneType>,
        replace_id: Option<&'static str>,
    ) {
        let query = MpdQuery { id, target, replace_id, callback: Box::new(on_done) };
        if let Err(err) = self.client_request_sender.send(ClientRequest::Query(query)) {
            log::error!(error:? = err; "Failed to send query request");
        }
    }

    pub(crate) fn command(
        &self,
        callback: impl FnOnce(&mut Client<'_>) -> Result<()> + Send + 'static,
    ) {
        if let Err(err) = self
            .client_request_sender
            .send(ClientRequest::Command(MpdCommand { callback: Box::new(callback) }))
        {
            log::error!(error:? = err; "Failed to send command request");
        }
    }

    pub(crate) fn find_current_song_in_queue(&self) -> Option<(usize, &Song)> {
        if self.status.state == State::Stop {
            return None;
        }
        // Use indexing by "song" instead of finding the song by id when the
        // queue is very large to avoid performance issues. The indexing is
        // not used by default because it can cause small/short desyncs when
        // queue is being updated by moving/shuffling the songs.
        if self.queue.len() > 3_000 {
            self.status.song.and_then(|idx| self.queue.get(idx).map(|song| (idx, song)))
        } else {
            self.status
                .songid
                .and_then(|id| self.queue.iter().enumerate().find(|(_, song)| song.id == id))
        }
    }

    /// The chapter markers relevant to the current playback: the mpv
    /// video's (keyed by Jellyfin item id, or the original `YouTube` link
    /// for resolved streams) while the video is the active UI source, else
    /// the current queue song's. When MPD playback takes over (the mutual
    /// exclusion pauses the video), the queue song's chapters apply even
    /// though the mpv session is still alive.
    pub(crate) fn current_playback_chapters(
        &self,
    ) -> Vec<crate::shared::chapters::Chapter> {
        if crate::core::mpv::mpv_is_ui_source(self) {
            if let Some(item_id) = self.mpv.item_id.as_deref() {
                return self.chapters.borrow().get(item_id).cloned().unwrap_or_default();
            }
            // A YouTube-style video: chapters are keyed by the original
            // link the mpv entry plays.
            let url = self
                .mpv
                .playlist
                .borrow()
                .get(self.mpv.playlist_pos.get().unwrap_or(0))
                .map(|entry| entry.url.clone());
            if let Some(url) = url {
                return self.chapters.borrow().get(&url).cloned().unwrap_or_default();
            }
            return Vec::new();
        }
        self.find_current_song_in_queue()
            .and_then(|(_, song)| self.chapters.borrow().get(&song.file).cloned())
            .unwrap_or_default()
    }

    /// Whether the current playback has chapter markers (shows the
    /// Chapters tab of the Queue list).
    pub(crate) fn has_current_chapters(&self) -> bool {
        !self.current_playback_chapters().is_empty()
    }

    /// Auto-open the Queue tab's Chapters list when the *current* song has
    /// chapter markers and the auto-chapters setting allows it. Only the
    /// Queue tab's internal list flips — the active tab is never changed,
    /// so a session on another tab stays put and the Chapters view is
    /// ready when the user returns to Queue. A no-op while the mpv video
    /// is the active UI source (the Queue list is then owned by
    /// `follow_playing_video`); once MPD playback takes over, the current
    /// song is the music, so its markers apply again.
    /// Called wherever the current song's chapters arrive (song change,
    /// resolved yt info, Jellyfin fetch, ffprobe).
    pub(crate) fn auto_show_chapters(&self) {
        if !self.config.ui.auto_show_chapters || crate::core::mpv::mpv_is_ui_source(self) {
            return;
        }
        if self.find_current_song_in_queue().is_some_and(|(_, song)| {
            !self.chapters.borrow().get(&song.file).is_none_or(Vec::is_empty)
        }) {
            self.queue_tab.set(QueueTabMode::Chapters);
        }
    }

    /// Whether a pane should be hidden: the Settings toggles, plus the cava
    /// visualizer is hidden while a video plays in mpv (MPD is paused then,
    /// so the bars go flat — on every tab, not just the Queue tab) and
    /// always on the Jellyfin tab.
    pub(crate) fn is_pane_hidden(&self, pane: &crate::config::tabs::PaneType) -> bool {
        if matches!(pane, crate::config::tabs::PaneType::Cava)
            && self.cava_hidden_on(self.active_tab.as_str())
        {
            return true;
        }
        self.config.is_pane_hidden(pane)
    }

    /// Whether the cava visualizer is hidden on `tab`: always on the
    /// Jellyfin tab (video browsing doesn't feed it), and on every tab while
    /// a video plays in mpv (MPD is paused, the bars would go flat).
    pub(crate) fn cava_hidden_on(&self, tab: &str) -> bool {
        tab.eq_ignore_ascii_case("Jellyfin") || self.mpv.active
    }

    pub(crate) fn find_current_lyrics_path(&self) -> Option<PathBuf> {
        use crate::shared::lrc::colocated_lrc_path;
        let (_, song) = self.find_current_song_in_queue()?;

        // Round 23 lookup order:
        // 1. The user's MPD library, colocated with the song (read-only —
        //    s2udio never writes there). Absolute song paths stand alone;
        //    relative MPD paths resolve against MPD's music_directory.
        //    This check runs even without a configured `lyrics_dir`.
        // 2. s2udio's own lyrics library (`lyrics_dir`), colocated mirror
        //    of the library layout.
        // 3. The s2udio lyrics index (metadata match).
        let song_path = std::path::Path::new(&song.file);
        let user_library_lrc = if song_path.is_absolute() {
            colocated_lrc_path(song_path).ok()
        } else {
            crate::ui::modals::paste::music_directory()
                .map(|dir| colocated_lrc_path(&std::path::Path::new(&dir).join(song_path)))
                .and_then(|res| res.ok())
        };
        let user_library = user_library_lrc.filter(|p| p.is_file());
        let path = if user_library.is_some() {
            user_library
        } else {
            let lyrics_dir = self.config.lyrics_dir.as_ref()?;
            crate::shared::lrc::get_lrc_path(lyrics_dir, &song.file)
                .ok()
                .filter(|p| p.is_file())
                .or_else(|| {
                    self.lrc_index.find_entry(song).map(|(path, _)| path.to_path_buf())
                })
        };

        let artist = song.metadata.get("artist").map(|v| v.last())?;
        let title = song.metadata.get("title").map(|v| v.last())?;
        let album = song.metadata.get("album").map(|v| v.last());
        match &path {
            Some(path) => log::debug!(artist, title, album; "Lyrics found at {}", path.display()),
            None => log::debug!(artist, title, album; "No lyrics found"),
        }

        path
    }

    pub(crate) fn find_lrc(&self) -> Result<Option<(PathBuf, Lrc)>> {
        let Some(path) = self.find_current_lyrics_path() else { return Ok(None) };
        let lrc = std::fs::read_to_string(&path)?.parse()?;
        Ok(Some((path, lrc)))
    }

    pub(crate) fn song_stickers(&self, uri: &str) -> Option<&HashMap<String, String>> {
        if matches!(self.stickers_supported, StickersSupport::UnsupportedAndChecked) {
            return None;
        }
        let stickers = self.stickers.get(uri);

        if stickers.is_none() {
            self.stickers_to_fetch.borrow_mut().insert(uri.to_owned());
        }

        stickers
    }

    /// Search for song stickers only if they are supported, does not trigger
    /// check for reason.
    pub(crate) fn song_stickers_if_supported(&self, uri: &str) -> Option<&HashMap<String, String>> {
        if !matches!(self.stickers_supported, StickersSupport::Supported) {
            return None;
        }

        let stickers = self.stickers.get(uri);

        if stickers.is_none() {
            self.stickers_to_fetch.borrow_mut().insert(uri.to_owned());
        }

        stickers
    }

    pub(crate) fn set_song_stickers(
        &mut self,
        uri: String,
        stickers: HashMap<String, String>,
    ) -> Option<HashMap<String, String>> {
        self.stickers.insert(uri, stickers)
    }

    pub(crate) fn set_stickers(&mut self, stickers: HashMap<String, HashMap<String, String>>) {
        self.stickers = stickers;
    }

    pub(crate) fn stickers(&self) -> &HashMap<String, HashMap<String, String>> {
        &self.stickers
    }
}

#[derive(Debug, Clone, Copy)]
pub enum StickersSupport {
    Supported,
    Unsupported,
    UnsupportedAndChecked,
}

impl From<StickersSupport> for bool {
    fn from(value: StickersSupport) -> Self {
        match value {
            StickersSupport::Supported => true,
            StickersSupport::Unsupported => false,
            StickersSupport::UnsupportedAndChecked => false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::{
        config::Config,
        mpd::commands::{Song, State, Status},
    };

    use super::initial_tab_with;

    fn song(id: u32, file: &str) -> Song {
        Song { id, file: file.to_owned(), ..Default::default() }
    }

    fn no_yt() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    /// The chapters shown follow the *active UI source*: while the mpv
    /// video plays, its chapters; when MPD playback takes over (the mutual
    /// exclusion pauses the video, but the mpv session stays alive), the
    /// current queue song's chapters — the list must not stay stuck on the
    /// video's markers.
    #[test]
    fn playback_chapters_follow_the_ui_source() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        use crate::shared::chapters::Chapter;
        // A chaptered video plays in mpv.
        ctx.mpv.active = true;
        ctx.mpv.item_id = Some("0123456789abcdef0123456789abcdef".to_owned());
        ctx.chapters.borrow_mut().insert(
            "0123456789abcdef0123456789abcdef".to_owned(),
            vec![Chapter { title: "Video intro".into(), start_secs: 0.0, end_secs: 60.0 }],
        );
        // A chaptered music file sits in the queue.
        ctx.status.state = State::Play;
        ctx.status.songid = Some(1);
        ctx.queue = vec![song(1, "audio/song.flac")];
        ctx.chapters.borrow_mut().insert(
            "audio/song.flac".to_owned(),
            vec![Chapter { title: "Audio intro".into(), start_secs: 0.0, end_secs: 30.0 }],
        );
        // While the video is the UI source, its chapters apply.
        assert_eq!(ctx.current_playback_chapters()[0].title, "Video intro");
        assert!(ctx.has_current_chapters());
        // MPD starts playing (pausing the video): the music's chapters now.
        ctx.mpv.paused = true;
        assert_eq!(ctx.current_playback_chapters()[0].title, "Audio intro");
        // The music stops again (video still paused): the video's chapters
        // are back (it is the current thing again).
        ctx.status.state = State::Stop;
        assert_eq!(ctx.current_playback_chapters()[0].title, "Video intro");
    }

    #[test]
    fn cava_hides_on_the_queue_tab_while_a_video_plays() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.active_tab = crate::config::tabs::TabName::from("Queue");
        let cava = crate::config::tabs::PaneType::Cava;
        // No video: cava shows on the Queue tab.
        assert!(!ctx.cava_hidden_on("Queue"));
        assert!(!ctx.is_pane_hidden(&cava));
        // A video starts playing in mpv: cava hides on every tab (MPD is
        // paused, the bars would go flat everywhere).
        ctx.mpv.active = true;
        assert!(ctx.cava_hidden_on("Queue"));
        assert!(ctx.is_pane_hidden(&cava));
        assert!(ctx.cava_hidden_on("Directories"));
        // The Jellyfin tab hides it regardless.
        assert!(ctx.cava_hidden_on("Jellyfin"));
        // Video ends: cava is back on the Queue tab.
        ctx.mpv.active = false;
        assert!(!ctx.cava_hidden_on("Queue"));
        assert!(!ctx.is_pane_hidden(&cava));
    }

    #[test]
    fn playing_local_file_opens_queue_tab() {
        let config = Config::default();
        let queue = vec![song(1, "/mnt/music/album/track.flac")];
        let status = Status { state: State::Play, songid: Some(1), ..Default::default() };
        assert_eq!(initial_tab_with(&config, &status, &queue, None, &no_yt()).as_str(), "Queue");
    }

    #[test]
    fn playing_stream_opens_radio_tab() {
        let config = Config::default();
        let queue = vec![song(2, "http://stream.example/live.mp3")];
        let status = Status { state: State::Play, songid: Some(2), ..Default::default() };
        assert_eq!(initial_tab_with(&config, &status, &queue, None, &no_yt()).as_str(), "Radio");
    }

    #[test]
    fn playing_youtube_stream_opens_queue_tab() {
        let config = Config::default();
        let url = "https://rr4.googlevideo.com/videoplayback?source=youtube";
        let queue = vec![song(4, url)];
        let status = Status { state: State::Play, songid: Some(4), ..Default::default() };
        let yt = std::collections::HashSet::from([url.to_owned()]);
        assert_eq!(initial_tab_with(&config, &status, &queue, None, &yt).as_str(), "Queue");
    }

    #[test]
    fn stopped_restores_last_tab() {
        let config = Config::default();
        let status = Status { state: State::Stop, ..Default::default() };
        assert_eq!(
            initial_tab_with(&config, &status, &[], Some("Radio"), &no_yt()).as_str(),
            "Radio"
        );
        assert_eq!(
            initial_tab_with(&config, &status, &[], Some("Queue"), &no_yt()).as_str(),
            "Queue"
        );
    }

    #[test]
    fn stopped_without_state_falls_back_to_first_tab() {
        let config = Config::default();
        let status = Status { state: State::Stop, ..Default::default() };
        assert_eq!(initial_tab_with(&config, &status, &[], None, &no_yt()).as_str(), "Queue");
    }

    #[test]
    fn paused_stream_still_counts_as_radio() {
        let config = Config::default();
        let queue = vec![song(3, "https://radio.example/stream")];
        let status = Status { state: State::Pause, songid: Some(3), ..Default::default() };
        assert_eq!(initial_tab_with(&config, &status, &queue, None, &no_yt()).as_str(), "Radio");
    }

    #[test]
    fn unknown_last_tab_falls_back() {
        let config = Config::default();
        let status = Status { state: State::Stop, ..Default::default() };
        assert_eq!(
            initial_tab_with(&config, &status, &[], Some("Nope"), &no_yt()).as_str(),
            "Queue"
        );
    }

    /// Round 23 lookup order: the user's MPD library (colocated, read-only)
    /// is checked FIRST, then s2udio's own lyrics library (colocated), then
    /// the s2udio lyrics index (metadata match). s2udio never writes to the
    /// user's library — the write path only ever targets `lyrics_dir`.
    #[test]
    fn lyrics_lookup_prefers_user_library_then_s2udio_library_then_index() {
        let _home_guard = crate::tests::fixtures::HOME_LOCK.lock().unwrap();
        use crate::shared::env::ENV;
        use crate::shared::lrc::{LrcIndex, LrcMetadata};
        let home = std::env::temp_dir().join(format!("s2u-lyrics-lookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        ENV.set("HOME".to_string(), home.to_string_lossy().into_owned());

        let music = home.join("Music");
        let lyrics_dir = home.join(".config/s2udio/lyrics");
        std::fs::create_dir_all(music.join("Artist/Album")).unwrap();
        std::fs::create_dir_all(lyrics_dir.join("Artist/Album")).unwrap();
        // MPD's music_directory, read by the user-library lookup.
        let mpd_conf_dir = home.join(".config/mpd");
        std::fs::create_dir_all(&mpd_conf_dir).unwrap();
        std::fs::write(
            mpd_conf_dir.join("mpd.conf"),
            format!("music_directory \"{}\"\n", music.display()),
        )
        .unwrap();

        let user_lrc = music.join("Artist/Album/01 Track.lrc");
        let s2udio_lrc = lyrics_dir.join("Artist/Album/01 Track.lrc");
        std::fs::write(&user_lrc, "[ti:01 Track]\n[ar:Artist]\n[00:01.00]user line\n").unwrap();
        std::fs::write(&s2udio_lrc, "[ti:01 Track]\n[ar:Artist]\n[00:01.00]s2udio line\n")
            .unwrap();

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut song = song(1, "Artist/Album/01 Track.flac");
        song.metadata.insert("artist".to_owned(), "Artist".into());
        song.metadata.insert("title".to_owned(), "01 Track".into());
        ctx.queue = vec![song];
        ctx.status.state = crate::mpd::commands::State::Play;
        ctx.status.songid = Some(1);
        let mut config = (*ctx.config).clone();
        config.lyrics_dir = Some(format!("{}/", lyrics_dir.display()));
        ctx.config = std::sync::Arc::new(config);

        // 1. The user's library .lrc wins over s2udio's own copy.
        assert_eq!(ctx.find_current_lyrics_path(), Some(user_lrc.clone()));

        // 2. When the user's library file is gone, s2udio's own copy is used.
        std::fs::remove_file(&user_lrc).unwrap();
        assert_eq!(ctx.find_current_lyrics_path(), Some(s2udio_lrc.clone()));

        // 3. When neither colocated file exists, the index (metadata
        //    match) still resolves the s2udio file.
        std::fs::remove_file(&s2udio_lrc).unwrap();
        ctx.lrc_index = LrcIndex::default();
        ctx.lrc_index.add(
            s2udio_lrc.clone(),
            LrcMetadata {
                title: Some("01 Track".to_owned()),
                artist: Some("Artist".to_owned()),
                album: None,
                ..Default::default()
            },
        );
        assert_eq!(ctx.find_current_lyrics_path(), Some(s2udio_lrc.clone()));

        ENV.remove("HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}

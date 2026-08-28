use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ops::AddAssign, path::PathBuf, time::{Duration, Instant},
};
use anyhow::{Result, bail};
use bon::bon;
use crossbeam::channel::{SendError, Sender, bounded};
use crate::{
    AppEvent, MpdCommand, MpdQuery, MpdQueryResult, WorkRequest,
    config::{Config, album_art::ImageMethod, tabs::{PaneType, TabName}},
    core::scheduler::{Scheduler, time_provider::DefaultTimeProvider},
    mpd::{
        client::Client, commands::{Song, State, Status},
        mpd_client::MpdClient, version::Version,
    },
    shared::{
        events::ClientRequest, keys::KeyResolver, lrc::{Lrc, LrcIndex},
        macros::{status_error, status_warn},
        mpd_client_ext::MpdClientExt, mpd_query::MpdQuerySync, ring_vec::RingVec,
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
        if let Some(song) = status
            .songid
            .and_then(|id| queue.iter().find(|s| s.id == id))
        {
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
                    name.as_str().eq_ignore_ascii_case(kind)
                        && !config.is_tab_hidden(name)
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
            .find(|name| {
                name.as_str().eq_ignore_ascii_case(last) && !config.is_tab_hidden(name)
            })
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
    /// Whether the album art pane should collapse entirely because there is
    /// no art to display (Round 48): set by the album art pane as art state
    /// changes; `is_pane_hidden` consults it so the layout hides the pane
    /// (no default placeholder box) until art becomes available.
    pub(crate) album_art_collapsed: Cell<bool>,
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
    pub(crate) scheduler: Scheduler<
        (Sender<AppEvent>, Sender<ClientRequest>),
        DefaultTimeProvider,
    >,
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
    pub(crate) torrent_engine: RefCell<
        Option<std::sync::Arc<crate::core::torrent::TorrentEngine>>,
    >,
    /// The round-54 downloader daemon's shared state
    /// (`~/.cache/s2udio/downloads.json`), refreshed by the 1 s
    /// `AppEvent::DlStatePoll` while the daemon has jobs: feeds the
    /// Downloads modal's Torrent section, the startup status line and the
    /// TUI's re-stream routing decision.
    #[debug(skip)]
    pub(crate) dl_state: RefCell<Option<crate::core::dlctl::DlStateFile>>,
    /// An open "Preparing downloader…" wait (round 54): a committed
    /// "Stream and download" / "Download & Play" enqueued a job and is
    /// waiting for the daemon's response (stream URLs to play through the
    /// daemon engine). Esc cancels the wait (the daemon job is stopped).
    #[debug(skip)]
    pub(crate) dl_wait: RefCell<Option<crate::ui::modals::paste::DlWaitState>>,
    /// Job ids the TUI is currently streaming through the daemon engine
    /// (one mpv session at a time). Written into the `dl-streaming.json`
    /// marker (R2.5) so the daemon defers moving those jobs' completed
    /// files; cleared on `MpvSessionEnded` / app exit.
    #[debug(skip)]
    pub(crate) dl_streaming_jobs: RefCell<std::collections::HashSet<String>>,
    /// Torrents the TUI is currently PLAIN-streaming on its own ephemeral
    /// engines (round 54, R2): forgotten (partials kept) when the stream
    /// ends or is replaced, so a stopped stream never keeps downloading or
    /// seeding.
    #[debug(skip)]
    pub(crate) plain_stream_torrents: RefCell<
        Vec<crate::core::torrent::PlainTorrentStream>,
    >,
    /// The standalone rqbit engine behind Settings -> torrent -> web ui
    /// (None until first opened): spawned on demand and kept alive (its
    /// `Drop` kills the child) until the user stops it or the app exits —
    /// independent of the per-play `torrent_engine` (UI-thread only, so a
    /// plain value suffices), so the web UI survives a playback session
    /// (VPN setup / verification use case).
    #[debug(skip)]
    pub(crate) torrent_webui_engine: RefCell<
        Option<crate::core::torrent::TorrentEngine>,
    >,
    /// The value typed into the Settings torrent socks-proxy input modal,
    /// drained by the settings panel's render (mpv custom-language
    /// pattern): the modal's `on_confirm` writes here because it only
    /// receives `&Ctx`.
    #[debug(skip)]
    pub(crate) torrent_socks_proxy_input: RefCell<Option<String>>,
    /// Scanned torrents (round 17): item source key -> the scan outcome
    /// (the running engine + torrent id + file list, or the failure the
    /// popup shows as a dim notice). The paste popup's `[Torrent]` section
    /// is driven by this map ("Loading…" until a scan lands, then the play
    /// actions); play actions reuse the scanned engine instead of spawning
    /// a fresh rqbit. Cleared when the paste modal closes (engines killed
    /// via `Drop`).
    #[debug(skip)]
    pub(crate) torrent_scans: RefCell<
        HashMap<String, Result<crate::core::torrent::TorrentScan, String>>,
    >,
    /// Item source keys whose scan is still in flight (so reopening the
    /// paste modal does not re-scan; removed when `TorrentScanned` lands).
    pub(crate) torrent_scans_pending: RefCell<HashSet<String>>,
    /// Round 18: per-item cancel signals for in-flight torrent scans (item
    /// source key -> the scan thread's cancel sender). The paste popup's
    /// close hook fires every entry when the user dismisses the popup, so
    /// the scan threads stop waiting and drop their engines promptly (no
    /// background leak); entries are removed when the scan lands.
    #[debug(skip)]
    pub(crate) torrent_scan_cancels: RefCell<
        HashMap<String, crossbeam::channel::Sender<()>>,
    >,
    /// Round 18: the latest progress of each in-flight torrent scan (item
    /// source key -> elapsed seconds + live download speed), rendered by
    /// the paste popup's wait window. Cleared when the popup closes.
    #[debug(skip)]
    pub(crate) torrent_scan_progress: RefCell<
        HashMap<String, crate::core::torrent::TorrentScanProgress>,
    >,
    /// The items of the currently open paste popup (None when no paste
    /// popup is open). Set by `show_paste_modal`, cleared on close; the
    /// scan-completion handler refreshes the popup only while it is set.
    pub(crate) paste_modal_items: RefCell<
        Option<Vec<crate::ui::modals::paste::PastedItem>>,
    >,
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
        mut scheduler: Scheduler<
            (Sender<AppEvent>, Sender<ClientRequest>),
            DefaultTimeProvider,
        >,
    ) -> Result<Self> {
        let supported_commands: HashSet<String> = client.supported_commands.clone();
        let stickers_supported = if supported_commands.contains("sticker") {
            StickersSupport::Supported
        } else {
            StickersSupport::Unsupported
        };
        log::info!(
            supported_commands:? = supported_commands; "Supported commands by server"
        );
        let status = client.get_status()?;
        let queue = client.playlist_info()?.unwrap_or_default();
        let cached_queue_time_total = queue.iter().filter_map(|s| s.duration).sum();
        if !supported_commands.contains("albumart")
            || !supported_commands.contains("readpicture")
        {
            config.album_art.method = ImageMethod::None;
            status_warn!("Album art is disabled because it is not supported by MPD");
        }
        log::info!(config:? = config; "Resolved config");
        let key_resolver = KeyResolver::new(&config);
        let yt_info = crate::ui::modals::paste::load_yt_cache(
            config.cache_dir.as_deref(),
        );
        let yt_streams: HashSet<String> = yt_info.keys().cloned().collect();
        let active_tab = initial_tab(&config, &status, &queue, &yt_streams);
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
            album_art_collapsed: Cell::new(false),
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
            dl_state: RefCell::new(None),
            dl_wait: RefCell::new(None),
            dl_streaming_jobs: RefCell::new(std::collections::HashSet::new()),
            plain_stream_torrents: RefCell::new(Vec::new()),
            torrent_webui_engine: RefCell::new(None),
            torrent_socks_proxy_input: RefCell::new(None),
            torrent_scans: RefCell::new(HashMap::new()),
            torrent_scans_pending: RefCell::new(HashSet::new()),
            torrent_scan_cancels: RefCell::new(HashMap::new()),
            torrent_scan_progress: RefCell::new(HashMap::new()),
            paste_modal_items: RefCell::new(None),
            paste_modal_id: Cell::new(None),
        };
        if let Some((_, song)) = ctx.find_current_song_in_queue() {
            if let Some(entry) = ctx.yt_info.borrow().get(&song.file) {
                if !entry.original_url.is_empty() {
                    let _ = ctx
                        .work_sender
                        .send(WorkRequest::ResolveYtStreams {
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
        if let Ok(content) = std::fs::read_to_string(
            crate::ui::modals::paste::video_playlist_path(
                ctx.config.cache_dir.as_deref(),
            ),
        )
            && let Ok(entries) = serde_json::from_str::<
                Vec<crate::core::mpv::MpvPlaylistEntry>,
            >(&content)
        {
            *ctx.video_playlist.borrow_mut() = entries
                .into_iter()
                .filter(|e| !e.url.is_empty())
                .collect();
            log::debug!(
                count = ctx.video_playlist.borrow().len(); "Restored the video playlist"
            );
        }
        if ctx.config.ui.auto_show_chapters
            && ctx
                .find_current_song_in_queue()
                .is_some_and(|(_, song)| {
                    !ctx.chapters.borrow().get(&song.file).is_none_or(Vec::is_empty)
                })
        {
            ctx.queue_tab.set(QueueTabMode::Chapters);
        }
        Ok(ctx)
    }
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
                    self.command(|client| {
                        if let Err(err) = client.sticker("", "test") {
                            status_error!(
                                "Stickers are not supported by MPD server: '{}'", err
                                .detail_or_display()
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
                    self.query()
                        .id(FETCH_SONG_STICKERS)
                        .replace_id(FETCH_SONG_STICKERS)
                        .query(|client| {
                            let stickers = client.fetch_song_stickers(uris)?;
                            Ok(MpdQueryResult::SongStickers(stickers))
                        });
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
            callback: Box::new(|client| Ok(
                MpdQueryResult::Any(Box::new((on_done)(client)?)),
            )),
            tx,
        };
        if let Err(err) = self
            .client_request_sender
            .send(ClientRequest::QuerySync(query))
        {
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
        #[builder(finish_fn)]
        on_done: impl FnOnce(&mut Client<'_>) -> Result<MpdQueryResult> + Send + 'static,
        id: &'static str,
        target: Option<PaneType>,
        replace_id: Option<&'static str>,
    ) {
        let query = MpdQuery {
            id,
            target,
            replace_id,
            callback: Box::new(on_done),
        };
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
            .send(
                ClientRequest::Command(MpdCommand {
                    callback: Box::new(callback),
                }),
            )
        {
            log::error!(error:? = err; "Failed to send command request");
        }
    }
    pub(crate) fn find_current_song_in_queue(&self) -> Option<(usize, &Song)> {
        if self.status.state == State::Stop {
            return None;
        }
        if self.queue.len() > 3_000 {
            self.status.song.and_then(|idx| self.queue.get(idx).map(|song| (idx, song)))
        } else {
            self.status
                .songid
                .and_then(|id| {
                    self.queue.iter().enumerate().find(|(_, song)| song.id == id)
                })
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
        if !self.config.ui.auto_show_chapters || crate::core::mpv::mpv_is_ui_source(self)
        {
            return;
        }
        if self
            .find_current_song_in_queue()
            .is_some_and(|(_, song)| {
                !self.chapters.borrow().get(&song.file).is_none_or(Vec::is_empty)
            })
        {
            self.queue_tab.set(QueueTabMode::Chapters);
        }
    }
    /// Whether a pane should be hidden: the Settings toggles, plus the cava
    /// visualizer is hidden while a video plays in mpv (MPD is paused then,
    /// so the bars go flat — on every tab, not just the Queue tab) and
    /// always on the Jellyfin tab, plus the album art pane collapses
    /// entirely when there is no art to display (Round 48).
    pub(crate) fn is_pane_hidden(&self, pane: &crate::config::tabs::PaneType) -> bool {
        if matches!(pane, crate ::config::tabs::PaneType::Cava)
            && self.cava_hidden_on(self.active_tab.as_str())
        {
            return true;
        }
        if matches!(pane, crate::config::tabs::PaneType::AlbumArt)
            && self.album_art_collapsed.get()
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
        let song_path = std::path::Path::new(&song.file);
        let user_library_lrc = if song_path.is_absolute() {
            colocated_lrc_path(song_path).ok()
        } else {
            crate::ui::modals::paste::music_directory()
                .map(|dir| colocated_lrc_path(
                    &std::path::Path::new(&dir).join(song_path),
                ))
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
            Some(path) => {
                log::debug!(artist, title, album; "Lyrics found at {}", path.display())
            }
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
    pub(crate) fn song_stickers_if_supported(
        &self,
        uri: &str,
    ) -> Option<&HashMap<String, String>> {
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
    pub(crate) fn set_stickers(
        &mut self,
        stickers: HashMap<String, HashMap<String, String>>,
    ) {
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

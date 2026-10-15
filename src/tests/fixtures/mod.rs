use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, Sender, unbounded};
use ratatui::{Terminal, backend::TestBackend};
use rstest::fixture;

/// Serializes tests that mutate the process-global `HOME` (via
/// `crate::shared::env::ENV`): several config/lookup tests point HOME at a
/// temp dir and race each other when run in parallel.
pub static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use crate::{
    config::{Config, tabs::TabName},
    core::scheduler::Scheduler,
    ctx::{Ctx, QueueTabMode, StickersSupport},
    mpd::{commands::Status, version::Version},
    shared::{
        events::{AppEvent, ClientRequest, WorkRequest},
        ipc::ipc_stream::IpcStream,
        keys::KeyResolver,
        lrc::LrcIndex,
        ring_vec::RingVec,
        ytdlp::YtDlpManager,
    },
    ui::input::InputManager,
};

pub mod fake_mpd_server;
pub mod mpd_client;

#[fixture]
pub fn ipc_stream() -> IpcStream {
    let pair = UnixStream::pair().expect("UnixStream pair should not fail");
    pair.0.into()
}

#[fixture]
pub fn status() -> Status {
    Status::default()
}

#[fixture]
pub fn work_request_channel() -> (Sender<WorkRequest>, Receiver<WorkRequest>) {
    unbounded()
}

#[fixture]
pub fn client_request_channel() -> (Sender<ClientRequest>, Receiver<ClientRequest>) {
    unbounded()
}

#[fixture]
pub fn app_event_channel() -> (Sender<AppEvent>, Receiver<AppEvent>) {
    unbounded()
}

#[fixture]
pub fn ctx(
    app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
    work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
    client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
) -> Ctx {
    let mut config = Config::default();
    // Keep test writes (the stream buffer's on-disk index, the yt caches) out
    // of the user's real cache dir.
    config.cache_dir = Some(std::env::temp_dir().join("s2udio-test-cache"));

    let scheduler = Scheduler::new((app_event_channel.0.clone(), unbounded().0));
    let key_resolver = KeyResolver::new(&config);
    Box::leak(Box::new(app_event_channel.1.clone()));
    Ctx {
        ytdlp_manager: YtDlpManager::new(work_request_channel.0.clone()),
        mpd_version: Version::new(1, 0, 0),
        lrc_index: LrcIndex::default(),
        config: std::sync::Arc::new(config),
        status: Status::default(),
        queue: Vec::default(),
        stickers: HashMap::new(),
        active_tab: TabName::from("test_tab"),
        supported_commands: HashSet::new(),
        queue_selected_id: Cell::new(None),
        lyrics_edit_mode: Cell::new(false),
        album_art_collapsed: Cell::new(false),
        temp_play_id: Cell::new(None),
        db_update_start: None,
        app_event_sender: app_event_channel.0.clone(),
        work_sender: work_request_channel.0.clone(),
        scheduler: scheduler,
        client_request_sender: client_request_channel.0.clone(),
        needs_render: Cell::new(false),
        resizing: Cell::new(false),
        stickers_to_fetch: RefCell::new(HashSet::new()),
        rendered_frames: 0,
        messages: RingVec::default(),
        song_played: None,
        metadata_processed_song: None,
        last_status_update: Instant::now(),
        stickers_supported: StickersSupport::Unsupported,
        input: InputManager::default(),
        key_resolver: key_resolver,
        cached_queue_time_total: Duration::default(),
        mpv: crate::core::mpv::MpvSession::default(),
        yt_info: RefCell::new(HashMap::new()),
        yt_list_meta: RefCell::new(HashMap::new()),
        pending_yt_links: RefCell::new(HashMap::new()),
        pending_start_seek: RefCell::new(HashMap::new()),
        pending_stream_resolve: RefCell::new(HashSet::new()),
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
        dl_completed_notified: RefCell::new(std::collections::HashSet::new()),
        plain_stream_torrents: RefCell::new(Vec::new()),
        torrent_webui_engine: RefCell::new(None),
        torrent_socks_proxy_input: RefCell::new(None),
        torrent_scans: RefCell::new(HashMap::new()),
        torrent_scans_pending: RefCell::new(HashSet::new()),
        torrent_scan_cancels: RefCell::new(HashMap::new()),
        torrent_scan_progress: RefCell::new(HashMap::new()),
        paste_modal_items: RefCell::new(None),
        paste_modal_id: Cell::new(None),
        paste_chapter_warm: RefCell::new(HashSet::new()),
        queue_info_warm: RefCell::new(HashSet::new()),
        warmed_ahead_ids: RefCell::new(HashSet::new()),
        warmed_ahead_for: Cell::new(None),
        pending_next_step: Cell::new(None),
        mpd_music_directory: None,
    }
}

#[fixture]
pub fn config() -> Config {
    Config::default()
}

#[fixture]
#[allow(clippy::unwrap_used)]
pub fn terminal() -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(100, 100)).unwrap()
}

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
    ctx::{Ctx, StickersSupport},
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
    let config = Config::default();

    let scheduler = Scheduler::new((app_event_channel.0.clone(), unbounded().0));
    let key_resolver = KeyResolver::new(&config);
    Box::leak(Box::new(app_event_channel.1.clone()));
    Ctx {
        ytdlp_manager: YtDlpManager::new(work_request_channel.0.clone()),
        mpd_version: Version::new(1, 0, 0),
        status: Status::default(),
        config: std::sync::Arc::new(config),
        queue: Vec::default(),
        stickers: HashMap::new(),
        active_tab: TabName::from("test_tab"),
        queue_selected_id: Cell::new(None),
        lyrics_edit_mode: Cell::new(false),
        temp_play_id: Cell::new(None),
        app_event_sender: app_event_channel.0.clone(),
        work_sender: work_request_channel.0.clone(),
        client_request_sender: client_request_channel.0.clone(),
        supported_commands: HashSet::new(),
        needs_render: Cell::new(false),
        resizing: Cell::new(false),
        stickers_to_fetch: RefCell::new(HashSet::new()),
        lrc_index: LrcIndex::default(),
        stickers_supported: StickersSupport::Unsupported,
        rendered_frames: 0,
        scheduler,
        db_update_start: None,
        messages: RingVec::default(),
        last_status_update: Instant::now(),
        song_played: None,
        metadata_processed_song: None,
        input: InputManager::default(),
        key_resolver,
        cached_queue_time_total: Duration::default(),
        mpv: crate::core::mpv::MpvSession::default(),
        yt_info: RefCell::new(HashMap::new()),
        chapters: RefCell::new(HashMap::new()),
        queue_tab: std::cell::Cell::new(crate::ctx::QueueTabMode::Audio),
        video_playlist: RefCell::new(Vec::new()),
        queue_table_width: Cell::new(None),
        mpv_custom_subtitle_lang: std::cell::RefCell::new(None),
        mpv_custom_audio_lang: std::cell::RefCell::new(None),
        mouse_pos: std::cell::Cell::new(None),
        modal_mouse_pos: std::cell::Cell::new(None),
        seekbar: std::cell::RefCell::new(crate::ui::seekbar::SeekbarState::default()),
        torrent_engine: std::cell::RefCell::new(None),
        torrent_download: std::cell::RefCell::new(None),
        torrent_webui_engine: std::cell::RefCell::new(None),
        torrent_socks_proxy_input: std::cell::RefCell::new(None),
        torrent_scans: std::cell::RefCell::new(std::collections::HashMap::new()),
        torrent_scans_pending: std::cell::RefCell::new(std::collections::HashSet::new()),
        torrent_scan_cancels: std::cell::RefCell::new(std::collections::HashMap::new()),
        torrent_scan_progress: std::cell::RefCell::new(std::collections::HashMap::new()),
        paste_modal_items: std::cell::RefCell::new(None),
        paste_modal_id: std::cell::Cell::new(None),
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

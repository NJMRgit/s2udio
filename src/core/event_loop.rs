use std::{
    collections::HashSet,
    ops::Sub,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, RecvTimeoutError, Sender};
use ratatui::{Terminal, layout::Rect, prelude::Backend};

use super::command::{create_env, run_external};
use crate::{
    config::{Config, LyricsSource, cli::RemoteCommandQuery},
    ctx::Ctx,
    mpd::{
        commands::{volume::Bound as _, IdleEvent, State},
        mpd_client::{MpdClient, SaveMode},
    },
    shared::{
        events::{AppEvent, ClientRequest, WorkDone, WorkRequest},
        ext::error::ErrorExt,
        id::{self, Id},
        keys::KeyResolver,
        macros::{modal, status_error, status_info, status_warn},
        mpd_client_ext::MpdClientExt,
        mpd_query::{
            EXTERNAL_COMMAND,
            GLOBAL_QUEUE_UPDATE,
            GLOBAL_STATUS_UPDATE,
            GLOBAL_STICKERS_UPDATE,
            GLOBAL_VOLUME_UPDATE,
            MpdQueryResult,
            run_status_update,
        },
    },
    ui::{
        KeyHandleResult,
        StatusMessage,
        Ui,
        UiAppEvent,
        UiEvent,
        modals::{downloads::DownloadsModal, info_modal::InfoModal, select_modal::SelectModal},
    },
};

static ON_RESIZE_SCHEDULE_ID: LazyLock<Id> = LazyLock::new(id::new);

pub fn init<B: Backend + std::io::Write + Send + 'static>(
    ctx: Ctx,
    event_rx: Receiver<AppEvent>,
    terminal: Terminal<B>,
) -> std::io::Result<std::thread::JoinHandle<Terminal<B>>> {
    std::thread::Builder::new()
        .name("main".to_owned())
        .spawn(move || main_task(ctx, event_rx, terminal))
}

fn main_task<B: Backend + std::io::Write>(
    mut ctx: Ctx,
    event_rx: Receiver<AppEvent>,
    mut terminal: Terminal<B>,
) -> Terminal<B> {
    let size = terminal.size().expect("To be able to get terminal size");
    let area = Rect::new(0, 0, size.width, size.height);
    let mut ui = Ui::new(&ctx).expect("UI to be created correctly");
    let event_receiver = event_rx;
    let mut render_wanted = false;
    // After a resize settles the first frame is drawn twice: the second
    // pass cleans up any artifacts from the first (terminal-side overlays
    // redraw at the new size, stale cells from the blank resize state).
    let mut resize_render_passes = 0u8;
    let max_fps = f64::from(ctx.config.max_fps);
    let mut min_frame_duration = Duration::from_secs_f64(1f64 / max_fps);
    let mut last_render = std::time::Instant::now().sub(Duration::from_secs(10));
    let mut additional_evs = HashSet::new();
    let mut connected = true;
    ui.before_show(area, &mut ctx).expect("Initial render init to succeed");
    let mut _update_loop_guard = None;
    let mut _update_db_loop_guard = None;

    // mpv video session: poll its IPC socket while active and report
    // playback progress back to Jellyfin (throttled).
    let mut mpv_poll_guard: Option<
        crate::core::scheduler::TaskGuard<(Sender<AppEvent>, Sender<ClientRequest>)>,
    > = None;
    // "Play and Download" torrent job: polls the engine's stats once per
    // second while a download is active (stopped when the job finishes or
    // is abandoned).
    let mut torrent_download_guard: Option<
        crate::core::scheduler::TaskGuard<(Sender<AppEvent>, Sender<ClientRequest>)>,
    > = None;
    let mut last_mpv_report = Instant::now() - Duration::from_secs(30);
    let mut mpv_last_paused = false;
    // Previous tick's mpv pause state (None until the first successful
    // poll read): the mpv-resume -> pause-MPD side of the mutual exclusion.
    let mut mpv_prev_paused: Option<bool> = None;
    // Consecutive MpvPoll reads that failed to reach mpv: after a few, the
    // session is treated as ended (needed for reattached sessions, where no
    // launcher thread exists to send MpvSessionEnded when mpv exits).
    let mut mpv_stale_ticks = 0u8;

    // A previous s2udio instance may have left mpv playing (mpv survives the
    // app's exit; the standalone tracker daemon keeps the MPRIS state + the
    // Jellyfin tracking alive while the app is closed). Reattach so the
    // controls, seekbar, album art and MPRIS resume working in the TUI.
    if crate::core::mpv::detect_mpv_session(&mut ctx) {
        crate::core::mpv::MPV_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
        // Restore the mutual exclusion: while the app was closed the tracker
        // paused mpv when MPD started, but a race (or a session without a
        // tracker) can leave both playing — pause the video, music wins.
        if ctx.status.state == State::Play {
            log::debug!("MPD is playing at reattach; pausing mpv");
            crate::core::mpv::pause_mpv();
        }
        // Don't report progress for the first 10 seconds: this instance
        // should settle first (and the tracker may have just reported).
        last_mpv_report = Instant::now();
        // Poll mpv at 100ms: during playback frames render at this rate,
        // which keeps the controls-bar title carousel, the progress bar and
        // the info-box marquee smooth (a 500ms poll stepped the carousel
        // ~3.75 columns per frame).
        mpv_poll_guard = Some(ctx.scheduler.repeated(
            Duration::from_millis(100),
            move |(tx, _)| {
                let _ = tx.send(AppEvent::MpvPoll);
                Ok(())
            },
        ));
        // A video on the Queue tab pauses MPD, so the cava visualizer goes
        // flat: hide it while the video plays (same as MpvSessionStarted).
        if ctx.cava_hidden_on(ctx.active_tab.as_str())
            && let Err(err) = ui.hide_cava(&ctx)
        {
            log::error!(error:? = err; "Failed to hide cava for the reattached video");
        }
        // Make sure the tracker daemon covers this session again: when this
        // s2udio closes, the MPRIS state + Jellyfin tracking must survive.
        // (A session started by an older build may have no tracker; the
        // pid lock makes a duplicate spawn exit immediately.)
        if let Err(err) = {
            let mut tracker = std::process::Command::new("s2u-mpv-tracker");
            tracker
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            // Own session, so closing the TUI (terminal SIGHUP to the
            // foreground process group) does not kill the daemon before it
            // can take over the tracking.
            unsafe { crate::core::mpv::detach_child(&mut tracker) };
            tracker.spawn()
        } {
            log::debug!(error:? = err; "Failed to spawn the mpv tracker daemon on reattach");
        }
        // The album art box belongs to the video now: refresh it (Jellyfin
        // primary image / YouTube thumbnail / default).
        if let Err(err) = ui.refresh_album_art(&ctx) {
            log::error!(error:? = err; "Failed to refresh album art for the reattached video");
        }
        // Jellyfin item: fetch the metadata + chapters + art the TUI shows.
        // The saved resume position is *not* re-applied: the video is
        // already playing (the tracker applied it at launch).
        if let Some(item_id) = ctx.mpv.item_id.clone() {
            let _ = ctx.work_sender.send(
                WorkRequest::FetchJellyfinMpris { item_id: item_id.clone() },
            );
            let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinChapters {
                item_id: item_id.clone(),
            });
            let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinVideoArt { item_id });
        }
        // The Queue tab follows the playing video (Chapters / Video list).
        if let Err(err) = ui.follow_video_session(&ctx) {
            log::error!(error:? = err; "Failed to follow the reattached video in the queue tab");
        }
    }

    // Watch ~/.blur-schedule once a second; when it changes, the active
    // mode's colors (read from ~/.local/bin/blsw) are applied to the theme.
    let mut last_blur_mode: Option<String> = None;
    let _blur_guard = ctx.scheduler.repeated(Duration::from_secs(1), move |(tx, _)| {
        let _ = tx.send(AppEvent::BlurCheck);
        Ok(())
    });
    let _ = ctx.app_event_sender.send(AppEvent::BlurCheck); // apply the current mode at startup

    // Tmux hooks have to be initialized after ui, because ueberzugpp replaces all
    // hooks on its init instead of simply appending and might break rmpc's hooks
    let mut tmux = match crate::shared::tmux::TmuxHooks::new() {
        Ok(Some(val)) => Some(val),
        Ok(None) => None,
        Err(err) => {
            log::error!(error:? = err; "Failed to install tmux hooks");
            None
        }
    };

    // Execute on_song_change at startup if
    // configured and current song is available. Round 38: with
    // `lyrics_source: LocalOnly` the hook (the network-fetch vehicle) is
    // never spawned — lyrics only ever come from local files.
    if ctx.config.exec_on_song_change_at_start
        && ctx.config.lyrics_source != LyricsSource::LocalOnly
        && let Some((_, _song)) = ctx.find_current_song_in_queue()
        && let Some(command) = &ctx.config.on_song_change
    {
        let env = create_env(&ctx, std::iter::empty());
        run_external(command.clone(), env);
    }

    // Listen to changes to lyrics when enabled
    let mut lyrics_watcher = if ctx.config.enable_lyrics_hot_reload
        && ctx.config.enable_lyrics_index
        && let Some(lyrics_dir) = &ctx.config.lyrics_dir
    {
        let lyrics_dir = PathBuf::from(lyrics_dir);
        let request_tx = ctx.work_sender.clone();
        Some(crate::core::lyrics_watcher::init(&lyrics_dir, request_tx))
    } else {
        None
    };

    match ctx.status.state {
        State::Play => {
            // Start update loop since a song is playing on startup
            _update_loop_guard = ctx
                .config
                .status_update_interval_ms
                .map(Duration::from_millis)
                .map(|interval| ctx.scheduler.repeated(interval, run_status_update));

            ctx.song_played = Some(ctx.status.elapsed);
        }
        State::Pause => {
            ctx.song_played = Some(ctx.status.elapsed);
        }
        State::Stop => {}
    }

    loop {
        let now = std::time::Instant::now();

        let event = if render_wanted {
            match event_receiver.recv_timeout(
                min_frame_duration.checked_sub(now - last_render).unwrap_or(Duration::ZERO),
            ) {
                Ok(v) => Some(v),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => None,
            }
        } else {
            event_receiver.recv().ok()
        };

        if let Some(event) = event {
            match event {
                AppEvent::ConfigChanged { config: mut new_config, keep_old_theme } => {
                    // Technical limitation. Keep the old image backend because it was not rechecked
                    // anyway. Sending the escape sequences to determine image support would mess up
                    // the terminal output at this point.
                    new_config.album_art.method = ctx.config.album_art.method;
                    if keep_old_theme {
                        new_config.theme = ctx.config.theme.clone();
                    }

                    if let Err(err) = new_config.validate() {
                        status_error!(error:? = err; "Cannot change config, invalid value: '{err}'");
                        continue;
                    }

                    new_config.active_panes =
                        Config::calc_active_panes(&new_config.tabs.tabs, &new_config.theme.layout);
                    ctx.config = Arc::new(*new_config);
                    let max_fps = f64::from(ctx.config.max_fps);
                    min_frame_duration = Duration::from_secs_f64(1f64 / max_fps);

                    // Update lyrics watcher as needed
                    if ctx.config.enable_lyrics_hot_reload != lyrics_watcher.is_some()
                        && ctx.config.enable_lyrics_index
                    {
                        // IIFE may be better expressed with try blocks when it becomes stable
                        lyrics_watcher = (|| {
                            if !ctx.config.enable_lyrics_hot_reload {
                                return None;
                            }

                            let lyrics_dir = PathBuf::from(ctx.config.lyrics_dir.as_ref()?);
                            let request_tx = ctx.work_sender.clone();
                            Some(crate::core::lyrics_watcher::init(&lyrics_dir, request_tx))
                        })();
                    }

                    // Update keybinds
                    ctx.key_resolver = KeyResolver::new(&ctx.config);

                    if let Err(err) = ui.on_event(UiEvent::ConfigChanged, &mut ctx) {
                        log::error!(error:? = err; "UI failed to handle config changed event");
                        continue;
                    }

                    // Need to clear the terminal to avoid artifacts from album art and other
                    // elements
                    if let Err(err) = terminal.clear() {
                        log::error!(error:? = err; "Failed to clear terminal after config change");
                        continue;
                    }

                    render_wanted = true;
                }
                AppEvent::ThemeChanged { theme } => {
                    let mut config = ctx.config.as_ref().clone();
                    config.theme = *theme;
                    if let Err(err) = config.validate() {
                        status_error!(error:? = err; "Cannot change theme, invalid config: '{err}'");
                        continue;
                    }

                    config.tabs = match config
                        .original_tabs_definition
                        .clone()
                        .convert(&config.theme.components, &config.theme.border_symbol_sets)
                    {
                        Ok(v) => v,
                        Err(err) => {
                            status_error!(error:? = err; "Cannot change theme, failed to convert tabs: '{err}'");
                            continue;
                        }
                    };

                    config.active_panes =
                        Config::calc_active_panes(&config.tabs.tabs, &config.theme.layout);
                    ctx.config = Arc::new(config);

                    if let Err(err) = ui.on_event(UiEvent::ConfigChanged, &mut ctx) {
                        log::error!(error:? = err; "UI failed to handle config changed event");
                    }

                    // Need to clear the terminal to avoid artifacts from album art and other
                    // elements
                    if let Err(err) = terminal.clear() {
                        log::error!(error:? = err; "Failed to clear terminal after config change");
                        continue;
                    }
                    render_wanted = true;
                }
                AppEvent::UserKeyInput(key) => {
                    // Keyboard interaction takes over from the mouse: drop
                    // the hover highlight so it never sits on a stale row
                    // while navigating with keys (it returns on the next
                    // pointer move).
                    ctx.set_mouse_pos(None);
                    ctx.set_modal_mouse_pos(None);
                    // Key-capture (e.g. the key remapping view) consumes the
                    // raw event before the resolver sees it.
                    let captured = match ui.handle_raw_key(key, &mut ctx) {
                        Ok(v) => v,
                        Err(err) => {
                            status_error!(err:?; "Error: {}", err.to_status());
                            false
                        }
                    };
                    if !captured {
                        ctx.key_resolver.handle_key_event(key.into(), key.kind, &ctx);
                    }
                    render_wanted = true;
                }
                AppEvent::UserMouseInput(ev) => match ui.handle_mouse_event(ev, &mut ctx) {
                    Ok(()) => {}
                    Err(err) => {
                        status_error!(err:?; "Error: {}", err.to_status());
                        render_wanted = true;
                    }
                },
                AppEvent::UserPaste(text) => {
                    // Middle-click pastes / drag&dropped files and links:
                    // offer the play/enqueue popup when audio was recognized.
                    if crate::ui::modals::paste::handle_paste(&ctx, &text) {
                        render_wanted = true;
                    }
                }
                AppEvent::MpvSessionStarted { url } => {
                    // Switch the now-playing UI to the mpv video session.
                    ctx.mpv.active = true;
                    // A video on the Queue tab pauses MPD, so the cava
                    // visualizer goes flat: hide it while the video plays.
                    if ctx.cava_hidden_on(ctx.active_tab.as_str())
                        && let Err(err) = ui.hide_cava(&ctx)
                    {
                        log::error!(error:? = err; "Failed to hide cava for video playback");
                    }
                    ctx.mpv.socket = None;
                    // A torrent stream has no yt-info and mpv's media-title
                    // is the raw URL: use the saved entry title (the picked
                    // file's name) right away instead of the URL.
                    ctx.mpv.title = if crate::core::torrent::is_torrent_stream_url(&url) {
                        ctx.mpv
                            .playlist
                            .borrow()
                            .iter()
                            .find(|e| e.url == url)
                            .map(|e| e.title.clone())
                            .unwrap_or_else(|| url.clone())
                    } else {
                        url.clone()
                    };
                    // A YouTube-style link was resolved before launch: use
                    // its real title/channel immediately (mpv's media-title
                    // catches up via the poll if not). Look the info up by
                    // the playlist entry's canonical link when the started
                    // URL is a resolved stream that carries one.
                    let lookup = {
                        let playlist = ctx.mpv.playlist.borrow();
                        playlist
                            .iter()
                            .find(|e| e.url == url)
                            .map(|e| e.lookup_url().to_owned())
                    };
                    if let Some(info) = lookup
                        .as_deref()
                        .and_then(|u| ctx.yt_info.borrow().get(u).cloned())
                        .or_else(|| ctx.yt_info.borrow().get(&url).cloned())
                    {
                        ctx.mpv.title = info.title.clone();
                        ctx.mpv.artist = info.channel.clone().unwrap_or_default();
                    }
                    ctx.mpv.item_id = crate::jellyfin::item_id_from_url(&url);
                    ctx.mpv.position = 0.0;
                    ctx.mpv.duration = 0.0;
                    ctx.mpv.paused = false;
                    ctx.mpv.pending_seek = None;
                    *ctx.mpv.pending_loadfile.borrow_mut() = None;
                    mpv_stale_ticks = 0;
                    // Don't report progress for the first 10 seconds so a
                    // saved resume position is applied before the first
                    // progress update (which would otherwise overwrite it).
                    last_mpv_report = Instant::now();
                    mpv_last_paused = false;
                    mpv_prev_paused = None;
                    if mpv_poll_guard.is_none() {
                        // 100ms so playback frames (and with them the title
                        // carousel / progress bar) stay smooth.
                        mpv_poll_guard = Some(ctx.scheduler.repeated(
                            Duration::from_millis(100),
                            move |(tx, _)| {
                                let _ = tx.send(AppEvent::MpvPoll);
                                Ok(())
                            },
                        ));
                    }
                    // For Jellyfin items: fetch the real title + poster (for
                    // the MPRIS bridge), the saved resume position, the
                    // chapter markers (Queue tab's Chapters view) and the
                    // primary image (shown as album art while it plays).
                    if let Some(item_id) = ctx.mpv.item_id.clone() {
                        let _ = ctx.work_sender.send(
                            WorkRequest::FetchJellyfinMpris { item_id: item_id.clone() },
                        );
                        let _ = ctx
                            .work_sender
                            .send(WorkRequest::FetchJellyfinResume { item_id: item_id.clone() });
                        let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinChapters {
                            item_id: item_id.clone(),
                        });
                        let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinVideoArt {
                            item_id,
                        });
                    }
                    // The album art box belongs to the video now: refresh
                    // it so the Queue tab shows the video's thumbnail
                    // (Jellyfin primary image / resolved YouTube thumbnail)
                    // instead of the stale audio art. The fetch requests go
                    // out from the pane's before_show (a no-op on tabs
                    // without the album art pane).
                    if let Err(err) = ui.refresh_album_art(&ctx) {
                        log::error!(error:? = err; "Failed to refresh album art for the video session");
                    }
                    // The Queue tab follows the playing video: its Chapters
                    // list when the video has markers (known synchronously
                    // for resolved YouTube streams; Jellyfin's arrive with
                    // the chapters fetch below), else the mpv playlist.
                    if let Err(err) = ui.follow_video_session(&ctx) {
                        log::error!(error:? = err; "Failed to follow the video session in the queue tab");
                    }
                    // The mpv session state file feeds the s2udio-mpris
                    // bridge (spawned by the tracker), which serves the
                    // video through its own MPRIS player.
                    render_wanted = true;
                }
                AppEvent::MpvSessionEnded => {
                    // Clear the flag the MPD-start pause guard checks (the
                    // launcher thread clears it for its own session, but a
                    // reattached one has no launcher thread — without this a
                    // dead session kept flagging "mpv running" forever).
                    crate::core::mpv::MPV_RUNNING
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    // Save the final position for resume (background thread).
                    if let Some(item_id) = ctx.mpv.item_id.clone() {
                        let config_file = ctx.config.jellyfin.config_file.clone();
                        let position = ctx.mpv.position;
                        std::thread::spawn(move || {
                            let sidecar = crate::config::jellyfin::jellyfin_sidecar_path();
                            if let Some(jf) =
                                crate::jellyfin::Jellyfin::load(&config_file, Some(&sidecar))
                            {
                                let _ = jf.report_playing_stopped(&item_id, position);
                            }
                        });
                    }
                    // Drop the mpv state file so mpDris2 falls back to MPD.
                    crate::ui::modals::paste::delete_mpv_mpris_state(&ctx);
                    ctx.mpv = crate::core::mpv::MpvSession::default();
                    // A "Play and Download" torrent whose download finished
                    // while mpv was still playing its stream: the stream is
                    // gone now, so move the completed file to
                    // the completed download and drop the job.
                    if ctx
                        .torrent_download
                        .borrow()
                        .as_ref()
                        .is_some_and(|job| job.complete && job.deferred)
                    {
                        let base_url = ctx
                            .torrent_download
                            .borrow()
                            .as_ref()
                            .map(|job| job.engine_base_url.clone())
                            .unwrap_or_default();
                        let engine = ctx.torrent_engine.borrow();
                        if let Some(engine) =
                            engine.as_ref().filter(|e| e.base_url() == base_url)
                        {
                            finish_torrent_download(&ctx, engine);
                        }
                        *ctx.torrent_download.borrow_mut() = None;
                        torrent_download_guard = None;
                    }
                    // The album art overlay belongs to the audio source
                    // again: restore the current song's art (or the default).
                    if let Err(err) = ui.refresh_album_art(&ctx) {
                        log::error!(error:? = err; "Failed to restore album art");
                    }
                    // The video ended: bring the cava visualizer back on the
                    // tabs that hide it during playback.
                    if !ctx.cava_hidden_on(ctx.active_tab.as_str())
                        && let Err(err) = ui.show_cava(&ctx)
                    {
                        log::error!(error:? = err; "Failed to restore cava");
                    }
                    mpv_poll_guard = None;
                    render_wanted = true;
                }
                AppEvent::TorrentDownloadPoll => {
                    // A stale event (the job was finished or abandoned
                    // since it was queued): nothing to poll.
                    if torrent_download_guard.is_none() {
                        continue;
                    }
                    // One "Play and Download" tick: poll the engine's
                    // stats; when the torrent's download is complete, move
                    // the picked file to s2udio-downloads — unless mpv is
                    // still playing the stream, in which case the move is
                    // deferred to MpvSessionEnded.
                    enum PollOutcome {
                        InProgress,
                        /// Download finished but mpv is still playing the
                        /// stream: mark the job complete+deferred.
                        Defer,
                        Complete,
                        Abandon(String),
                    }
                    let outcome = {
                        let job = ctx.torrent_download.borrow();
                        let Some(job) = job.as_ref() else { continue };
                        let engine = ctx.torrent_engine.borrow();
                        let Some(engine) = engine.as_ref() else { continue };
                        if engine.base_url() != job.engine_base_url {
                            // The engine was replaced by another torrent
                            // play: the download died with it.
                            PollOutcome::Abandon("Torrent download interrupted".to_owned())
                        } else {
                            match crate::core::torrent::torrent_stats(engine, &job.torrent_id)
                            {
                                Ok(stats)
                                    if crate::core::torrent::download_complete(&stats, job) =>
                                {
                                    // Is mpv still playing one of the kept
                                    // streams? Moving a file away (and
                                    // deleting the torrent) would break
                                    // playback — defer the whole move to
                                    // MpvSessionEnded (round 21: any kept
                                    // file, not just the first).
                                    let playing = ctx.mpv.active
                                        && ctx.mpv.playlist.borrow().iter().any(|entry| {
                                            job.files
                                                .iter()
                                                .any(|f| f.stream_url == entry.url)
                                        });
                                    if playing {
                                        PollOutcome::Defer
                                    } else {
                                        PollOutcome::Complete
                                    }
                                }
                                Ok(_) => PollOutcome::InProgress,
                                Err(err) => {
                                    let msg = format!("Torrent download interrupted: {err}");
                                    let mut job = ctx.torrent_download.borrow_mut();
                                    let Some(job) = job.as_mut() else { continue };
                                    job.failures += 1;
                                    if job.failures >= 3 {
                                        PollOutcome::Abandon(msg)
                                    } else {
                                        PollOutcome::InProgress
                                    }
                                }
                            }
                        }
                    };
                    match outcome {
                        PollOutcome::InProgress => {}
                        PollOutcome::Defer => {
                            // The file is complete but mpv still plays the
                            // stream: keep the job; MpvSessionEnded moves
                            // the file.
                            let mut job = ctx.torrent_download.borrow_mut();
                            if let Some(job) = job.as_mut() {
                                job.complete = true;
                                job.deferred = true;
                            }
                        }
                        PollOutcome::Complete => {
                            let base_url = ctx
                                .torrent_download
                                .borrow()
                                .as_ref()
                                .map(|job| job.engine_base_url.clone())
                                .unwrap_or_default();
                            let engine = ctx.torrent_engine.borrow();
                            if let Some(engine) =
                                engine.as_ref().filter(|e| e.base_url() == base_url)
                            {
                                finish_torrent_download(&ctx, engine);
                            }
                            *ctx.torrent_download.borrow_mut() = None;
                            torrent_download_guard = None;
                        }
                        PollOutcome::Abandon(msg) => {
                            status_warn!("{msg}");
                            *ctx.torrent_download.borrow_mut() = None;
                            torrent_download_guard = None;
                        }
                    }
                    render_wanted = true;
                }
                AppEvent::TorrentScannedPlay { scan, file_indices, download } => {
                    // Round 17: the paste popup's play action on an
                    // already-scanned torrent — the engine is running and
                    // the file list is known, so playback starts here (the
                    // event loop owns the download job's scheduler guard)
                    // instead of re-scanning on the work thread. Round 20:
                    // the scan map keeps its own `Arc` clone of the engine
                    // (the played scan is NOT consumed), so a repeat paste
                    // of the same torrent reuses the engine instead of
                    // spawning a second rqbit on the same cache dir.
                    let entries = crate::ui::modals::paste::torrent_entries(&scan, &file_indices);
                    // "Play and Download" / the picker's "Download & Play"
                    // (round 21): the job tracks every played file's
                    // completion — one job per torrent.
                    let cache_dir = ctx.config.torrent.cache_dir.clone();
                    let torrent_name = scan.torrent_name.clone();
                    let download: Vec<crate::core::torrent::TorrentDownloadFile> = if download {
                        file_indices
                            .iter()
                            .filter_map(|i| scan.files.get(*i))
                            .map(|f| crate::core::torrent::TorrentDownloadFile {
                                file_idx: f.index,
                                file_length: f.length,
                                file_name: f.name.clone(),
                                source_path: cache_dir.join(&torrent_name).join(&f.name),
                                stream_url: scan
                                    .engine
                                    .stream_url(&scan.torrent_id, f.index as u64),
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    start_torrent_playback(
                        &mut ctx,
                        scan.engine,
                        scan.torrent_id.clone(),
                        torrent_name,
                        entries,
                        download,
                        &mut torrent_download_guard,
                    );
                    render_wanted = true;
                }
                AppEvent::TorrentScannedDownload { scan, file_indices } => {
                    // Round 21: the popup's download-only actions
                    // ("Download" / "Download all") — keep the scanned
                    // engine alive and move the chosen files to
                    // `s2udio-downloads` once the torrent's download
                    // completes. No playback (unlike TorrentScannedPlay
                    // with `download: true`); the job owns the scheduler
                    // guard here.
                    let cache_dir = ctx.config.torrent.cache_dir.clone();
                    let torrent_name = scan.torrent_name.clone();
                    let files: Vec<crate::core::torrent::TorrentDownloadFile> = file_indices
                        .iter()
                        .filter_map(|i| scan.files.get(*i))
                        .map(|f| crate::core::torrent::TorrentDownloadFile {
                            file_idx: f.index,
                            file_length: f.length,
                            file_name: f.name.clone(),
                            source_path: cache_dir.join(&torrent_name).join(&f.name),
                            stream_url: scan.engine.stream_url(&scan.torrent_id, f.index as u64),
                        })
                        .collect();
                    start_torrent_download(
                        &mut ctx,
                        scan.engine,
                        scan.torrent_id,
                        torrent_name,
                        files,
                        &mut torrent_download_guard,
                    );
                    render_wanted = true;
                }
                AppEvent::MpvPoll => {
                    if !ctx.mpv.active {
                        continue;
                    }
                    // Keep the MPRIS bridge in sync (title/art/position);
                    // written before the socket is even up so the daemon
                    // never sees a missing state file.
                    crate::ui::modals::paste::write_mpv_mpris_state(&ctx);
                    let Some(socket) = crate::core::mpv::mpv_socket() else {
                        // No socket at all: mpv exited. For a session this
                        // instance reattached to there is no launcher thread
                        // to send MpvSessionEnded, so count failures and
                        // tear the session down ourselves after a few.
                        mpv_stale_ticks += 1;
                        if mpv_stale_ticks >= 5 {
                            let _ = ctx.app_event_sender.send(AppEvent::MpvSessionEnded);
                        }
                        render_wanted = true;
                        continue;
                    };
                    ctx.mpv.socket = Some(socket.clone());
                    let Some((position, paused, duration, volume, playlist_pos, playlist_count)) =
                        crate::core::mpv::read_mpv_state(&socket)
                    else {
                        // Socket file exists but mpv is unreachable: same as
                        // above (the stale file can outlive mpv).
                        mpv_stale_ticks += 1;
                        if mpv_stale_ticks >= 5 {
                            let _ = ctx.app_event_sender.send(AppEvent::MpvSessionEnded);
                        }
                        render_wanted = true;
                        continue;
                    };
                    mpv_stale_ticks = 0;
                        // The mpv video / MPD audio UI-source switch (MPD
                        // playback started and the mutual exclusion paused
                        // the video, or the video resumed): the album art
                        // box follows whichever source is active, so refresh
                        // it when the source flips (nothing else repaints it
                        // — a SongChanged may never fire for a resumed
                        // track).
                        let source_before = ctx.mpv.active
                            && (!ctx.mpv.paused
                                || ctx.status.state != crate::mpd::commands::State::Play);
                        ctx.mpv.position = position;
                        ctx.mpv.paused = paused;
                        ctx.mpv.duration = duration;
                        ctx.mpv.volume = volume;
                        let source_after = ctx.mpv.active
                            && (!paused
                                || ctx.status.state != crate::mpd::commands::State::Play);
                        if source_before != source_after
                            && let Err(err) = ui.refresh_album_art(&ctx)
                        {
                            log::error!(error:? = err; "Failed to refresh album art after the playback source switched");
                        }
                        // The video resumed while MPD plays: pause the
                        // music (the other side of the mutual exclusion;
                        // MPD-start -> pause-mpv is handled on the status
                        // update). Only on the paused->playing transition:
                        // the user unpaused the video, so the music gives
                        // way.
                        if let Some(prev) = mpv_prev_paused
                            && prev
                            && !paused
                            && ctx.status.state == State::Play
                        {
                            log::debug!("mpv resumed while MPD plays; pausing MPD");
                            let _ = ctx.client_request_sender.send(
                                ClientRequest::Command(crate::MpdCommand {
                                    callback: Box::new(|client| {
                                        client.pause()?;
                                        Ok(())
                                    }),
                                }),
                            );
                        }
                        mpv_prev_paused = Some(paused);
                        // mpv advanced to another playlist entry: follow it
                        // in the session (title + Jellyfin item id switch to
                        // the new entry so progress/resume stay correct).
                        // Only when mpv's own playlist still matches the
                        // recorded one: after a `loadfile ... replace` (a
                        // video picked from the Queue Video view, a
                        // cross-season switch) mpv's playlist is a single
                        // entry at position 0, and the session state was
                        // already set by the load action.
                        let playlist_matches = playlist_count
                            .is_some_and(|count| count == ctx.mpv.playlist.borrow().len());
                        if playlist_matches
                            && playlist_pos.is_some()
                            && ctx.mpv.playlist_pos.get() != playlist_pos
                        {
                            // Confirm the recorded entry at mpv's reported
                            // position is the entry mpv is actually playing:
                            // `loadfile … replace` splices into the old
                            // playlist, so when the old and new lengths
                            // coincide the count gate alone cannot detect a
                            // diverged mpv playlist (following would surface
                            // the next episode's metadata while mpv plays
                            // the selected one). A confirmed mismatch skips
                            // the advance entirely and keeps the recorded
                            // position; the JF_SEASON_PLAY rebuild (or the
                            // pending-loadfile reload) already corrected the
                            // actual playlist, so the next matching poll
                            // adopts it.
                            let advanced = crate::core::mpv::recorded_entry_for_mpv_pos(
                                &ctx.mpv.playlist.borrow(),
                                playlist_pos.unwrap_or(0),
                                crate::core::mpv::read_mpv_path(&socket).as_deref(),
                            );
                            // A confirmed mismatch leaves the recorded
                            // position untouched and skips only the advance
                            // (the rest of the poll — pending seek, title
                            // refresh, MPRIS — still runs this tick); the
                            // next matching poll adopts the entry once the
                            // playlist rebuild landed.
                            if let Some(entry) = advanced {
                                ctx.mpv.playlist_pos.set(playlist_pos);
                                ctx.mpv.title = entry.title.clone();
                                ctx.mpv.item_id =
                                    crate::jellyfin::item_id_from_url(&entry.url);
                                ctx.mpv.item = None;
                                // A YouTube-style entry: the resolved info
                                // supplies the real title/channel, and the
                                // MPRIS poster is re-fetched for the new
                                // entry.
                                if let Some(info) =
                                    ctx.yt_info.borrow().get(&entry.lookup_url().to_owned())
                                {
                                    ctx.mpv.title = info.title.clone();
                                    ctx.mpv.artist =
                                        info.channel.clone().unwrap_or_default();
                                }
                                ctx.mpv.art_path = None;
                                // Don't serve the previous entry's poster
                                // until the new art is fetched.
                                crate::ui::modals::paste::clear_mpv_mpris_art(&ctx);
                                // The new entry is another Jellyfin item: refresh
                                // its metadata and chapters.
                                if let Some(item_id) = ctx.mpv.item_id.clone() {
                                    let _ = ctx.work_sender.send(
                                        WorkRequest::FetchJellyfinMpris {
                                            item_id: item_id.clone(),
                                        },
                                    );
                                    let _ = ctx.work_sender.send(
                                        WorkRequest::FetchJellyfinChapters {
                                            item_id: item_id.clone(),
                                        },
                                    );
                                    let _ = ctx.work_sender.send(
                                        WorkRequest::FetchJellyfinVideoArt { item_id },
                                    );
                                }
                                // A YouTube-style entry: refresh the album art
                                // thumbnail for the new entry.
                                if let Some(info) = crate::ui::modals::paste::mpv_yt_info(&ctx)
                                    && let Some(thumb) = info.thumbnail
                                {
                                    let _ =
                                        ctx.work_sender.send(WorkRequest::FetchYtThumbnail {
                                            url: thumb,
                                        });
                                }
                            }
                        }
                    // Apply a pending resume seek now that the socket is
                    // reachable.
                    if let Some(seconds) = ctx.mpv.pending_seek.take() {
                        crate::core::mpv::mpv_seek(&socket, seconds);
                        ctx.mpv.position = seconds;
                    }
                    // A playlist switch requested before the socket was up
                    // (a video added while the session was still starting):
                    // load it now, first entry replacing, the rest appended.
                    // Clear first, for the same reason as `play_video_entries`
                    // (mpv's `loadfile … replace` keeps the old entries).
                    if let Some(urls) = ctx.mpv.pending_loadfile.borrow_mut().take()
                        && let Some(first) = urls.first()
                    {
                        crate::core::mpv::mpv_playlist_clear(&socket);
                        crate::core::mpv::mpv_loadfile(&socket, first);
                        for url in urls.iter().skip(1) {
                            crate::core::mpv::mpv_append_load(&socket, url);
                        }
                        ctx.mpv.position = 0.0;
                    }
                    // A raw stream URL (or mpv's provisional URL-minus-scheme
                    // title) is not useful; prefer mpv's media-title once it
                    // has loaded metadata. Re-read every poll until the title
                    // stops looking provisional (a one-shot guard got stuck
                    // on the provisional title and never updated). When mpv
                    // has nothing better than the stream basename (e.g.
                    // `index.m3u8` for a resolved HLS URL), keep the saved
                    // entry title instead of replacing it with the basename:
                    // only adopt mpv's title when it is a real one.
                    if crate::core::mpv::is_provisional_title(&ctx.mpv.title)
                        && let Some(title) = crate::core::mpv::read_mpv_title(&socket)
                    {
                        if !crate::core::mpv::is_provisional_title(&title) {
                                ctx.mpv.title = title;
                            } else {
                                // mpv's media-title is just the stream URL / a
                                // basename: fall back to the cached resolved
                                // info (title + channel), then to the playlist
                                // entry title, rather than pushing the
                                // basename into MPRIS.
                                if let Some(info) =
                                    crate::ui::modals::paste::mpv_yt_info(&ctx)
                                {
                                    if !info.title.is_empty() {
                                        ctx.mpv.title = info.title.clone();
                                    }
                                    if !info.channel.as_deref().unwrap_or("").is_empty() {
                                        ctx.mpv.artist =
                                            info.channel.clone().unwrap_or_default();
                                    }
                                } else if let Some(entry) = ctx
                                    .mpv
                                    .playlist
                                    .borrow()
                                    .get(ctx.mpv.playlist_pos.get().unwrap_or(0))
                                    && !entry.title.is_empty()
                                {
                                    ctx.mpv.title = entry.title.clone();
                                }
                            }
                    }
                    // MPRIS art for a YouTube (etc.) video playing in mpv:
                    // fetch its resolved thumbnail into the mpv-mpris poster
                    // file once (the album art pane shows it separately via
                    // FetchYtThumbnail).
                    if ctx.mpv.art_path.is_none()
                        && let Some(info) = crate::ui::modals::paste::mpv_yt_info(&ctx)
                        && let Some(thumb) = info.thumbnail
                    {
                        let cache_dir = ctx.config.cache_dir.clone();
                        let _ = ctx.work_sender.send(WorkRequest::SaveMpvMprisArt {
                            url: thumb,
                            cache_dir,
                        });
                        ctx.mpv.art_path =
                            Some(crate::ui::modals::paste::mpv_mpris_art_path(
                                ctx.config.cache_dir.as_deref(),
                            ));
                    }
                    // Report progress to Jellyfin: on pause changes and
                    // otherwise at most every 10 seconds.
                    if let Some(item_id) = ctx.mpv.item_id.clone() {
                        let changed = ctx.mpv.paused != mpv_last_paused;
                        if changed || last_mpv_report.elapsed() >= Duration::from_secs(10) {
                            last_mpv_report = Instant::now();
                            mpv_last_paused = ctx.mpv.paused;
                            let position = ctx.mpv.position;
                            let paused = ctx.mpv.paused;
                            let config_file = ctx.config.jellyfin.config_file.clone();
                            std::thread::spawn(move || {
                                let sidecar = crate::config::jellyfin::jellyfin_sidecar_path();
                                if let Some(jf) =
                                    crate::jellyfin::Jellyfin::load(&config_file, Some(&sidecar))
                                {
                                    let _ =
                                        jf.report_playing_progress(&item_id, position, paused);
                                }
                            });
                        }
                    }
                    render_wanted = true;
                }
                AppEvent::ActionResolved(mut action) => {
                    match ui.handle_action(&mut action, &mut ctx) {
                        Ok(KeyHandleResult::None) => continue,
                        Ok(KeyHandleResult::Quit) => {
                            if let Err(err) = ui.on_event(UiEvent::Exit, &mut ctx) {
                                log::error!(error:? = err; "UI failed to handle quit event");
                            }
                            break;
                        }
                        Err(err) => {
                            status_error!(err:?; "Error: {}", err.to_status());
                            render_wanted = true;
                        }
                    }
                }
                AppEvent::InsertModeFlush((mut action, buf)) => {
                    if let Err(err) = ui.handle_insert_mode(action.as_mut(), &buf, &mut ctx) {
                        log::error!(error:? = err, action:?, buf:?; "UI failed to handle insert mode flush");
                    }
                    render_wanted = true;
                }
                AppEvent::KeyTimeout => {
                    log::debug!("Key timeout reached, handling queued keys");
                    ctx.key_resolver.handle_timeout(&ctx);
                    render_wanted = true;
                }
                AppEvent::Status(mut message, level, timeout) => {
                    ctx.messages.push(StatusMessage {
                        level,
                        timeout,
                        message: std::mem::take(&mut message),
                        created: std::time::Instant::now(),
                    });

                    render_wanted = true;
                    // Send delayed render event to make the status message
                    // disappear
                    ctx.scheduler
                        .schedule(timeout, |(tx, _)| Ok(tx.send(AppEvent::RequestRender)?));
                }
                AppEvent::InfoModal { message, title, size, replacement_id: id } => {
                    if let Err(err) = ui.on_ui_app_event(
                        UiAppEvent::Modal(Box::new(
                            InfoModal::builder()
                                .ctx(&ctx)
                                .maybe_title(title)
                                .maybe_size(size)
                                .maybe_replacement_id(id)
                                .message(message)
                                .build(),
                        )),
                        &mut ctx,
                    ) {
                        log::error!(error:? = err; "UI failed to handle modal event");
                    }
                }
                AppEvent::Log(msg) => {
                    if let Err(err) = ui.on_event(UiEvent::LogAdded(msg), &mut ctx) {
                        log::error!(error:? = err; "UI failed to handle log event");
                    }
                }
                AppEvent::IdleEvent(event) => {
                    handle_idle_event(event, &ctx, &mut additional_evs);
                    for ev in additional_evs.drain().filter_map(|ev| UiEvent::try_from(ev).ok()) {
                        if let Err(err) = ui.on_event(ev, &mut ctx) {
                            status_error!(error:? = err, event:?; "UI failed to handle idle event, event: '{:?}', error: '{}'", event, err.to_status());
                        }
                    }
                    render_wanted = true;
                }
                AppEvent::RequestRender => {
                    render_wanted = true;
                }
                AppEvent::WorkDone(Ok(result)) => match result {
                    WorkDone::YtStreamsResolved { info, action, failures } => {
                        for failure in &failures {
                            status_warn!("Failed to resolve stream: {failure}");
                        }
                        if info.is_empty() {
                            status_warn!("No stream could be resolved");
                        } else {
                            crate::ui::modals::paste::apply_resolved_streams(
                                &ctx, info, action, failures,
                            );
                        }
                        render_wanted = true;
                    }
                    WorkDone::TorrentScanned { key, result, .. } => {
                        // Round 17: a popup scan landed — store the engine
                        // + file list (or the failure) and refresh the
                        // popup's [Torrent] section ("Loading…" → the play
                        // actions the scan enables).
                        crate::ui::modals::paste::on_torrent_scanned(&ctx, key, result);
                        render_wanted = true;
                    }
                    WorkDone::TorrentScanProgress { key, progress } => {
                        // Round 18: refresh the paste popup's wait window
                        // (elapsed counter + DL-speed / needed-speed check)
                        // with the scan's live progress.
                        crate::ui::modals::paste::on_torrent_scan_progress(&ctx, key, progress);
                        render_wanted = true;
                    }
                    WorkDone::TorrentStreamPrepared {
                        key,
                        engine,
                        stream_url,
                        torrent_name,
                        file_name,
                        torrent_id,
                        file_idx,
                        file_length,
                        download,
                    } => {
                        // M2 single-file play (the fresh-engine fallback —
                        // the scanned path arrives as
                        // AppEvent::TorrentScannedPlay). M3 inserts the
                        // bandwidth gate before this point.
                        // Round 20: register the prepared play as a
                        // single-file scan under the item's canonical key —
                        // the engine is shared via `Arc`, so a repeat
                        // paste of the same torrent reuses it instead of
                        // spawning a second rqbit against the same cache.
                        let engine = std::sync::Arc::new(engine);
                        ctx.torrent_scans.borrow_mut().insert(
                            key,
                            Ok(crate::core::torrent::TorrentScan {
                                engine: engine.clone(),
                                torrent_id: torrent_id.clone(),
                                torrent_name: torrent_name.clone(),
                                files: vec![crate::core::torrent::ScannedFile {
                                    index: file_idx,
                                    name: file_name.clone(),
                                    length: file_length,
                                }],
                            }),
                        );
                        let entry = crate::core::mpv::MpvPlaylistEntry::new(
                            file_name.clone(),
                            stream_url.clone(),
                            None,
                        );
                        let cache_dir = ctx.config.torrent.cache_dir.clone();
                        let download: Vec<crate::core::torrent::TorrentDownloadFile> = download
                            .then(|| {
                                vec![crate::core::torrent::TorrentDownloadFile {
                                    file_idx,
                                    file_length,
                                    file_name: file_name.clone(),
                                    source_path: cache_dir.join(&torrent_name).join(&file_name),
                                    stream_url: stream_url.clone(),
                                }]
                            })
                            .unwrap_or_default();
                        start_torrent_playback(
                            &mut ctx,
                            engine,
                            torrent_id,
                            torrent_name,
                            vec![entry],
                            download,
                            &mut torrent_download_guard,
                        );
                        render_wanted = true;
                    }
                    WorkDone::TorrentDownloadPrepared {
                        key,
                        engine,
                        torrent_id,
                        torrent_name,
                        files,
                    } => {
                        // Round 21: the fresh-engine fallback of the
                        // popup's download-only actions — register the
                        // prepared download as a scan under the item's
                        // canonical key (engine Arc-shared for reuse, like
                        // the round-20 prepared-play path) and start the
                        // download job (no playback).
                        let engine = std::sync::Arc::new(engine);
                        ctx.torrent_scans.borrow_mut().insert(
                            key,
                            Ok(crate::core::torrent::TorrentScan {
                                engine: engine.clone(),
                                torrent_id: torrent_id.clone(),
                                torrent_name: torrent_name.clone(),
                                files: files.clone(),
                            }),
                        );
                        let cache_dir = ctx.config.torrent.cache_dir.clone();
                        let download: Vec<crate::core::torrent::TorrentDownloadFile> = files
                            .iter()
                            .map(|f| crate::core::torrent::TorrentDownloadFile {
                                file_idx: f.index,
                                file_length: f.length,
                                file_name: f.name.clone(),
                                source_path: cache_dir.join(&torrent_name).join(&f.name),
                                stream_url: engine.stream_url(&torrent_id, f.index as u64),
                            })
                            .collect();
                        start_torrent_download(
                            &mut ctx,
                            engine,
                            torrent_id,
                            torrent_name,
                            download,
                            &mut torrent_download_guard,
                        );
                        render_wanted = true;
                    }
                    WorkDone::JellyfinFetched { id, data } => {
                        use crate::jellyfin::JellyfinResult;
                        // Item metadata + resume position feed the mpv
                        // session; everything else goes to the Jellyfin pane.
                        match (id, data) {
                            (crate::ui::panes::jellyfin::JF_ITEM, JellyfinResult::Item(item)) => {
                                if ctx.mpv.active
                                    && ctx.mpv.item_id.as_deref() == Some(item.id.as_str())
                                {
                                    ctx.mpv.title = item.name.clone();
                                    ctx.mpv.artist = item
                                        .album_artist
                                        .clone()
                                        .or_else(|| item.artist.clone())
                                        .or_else(|| item.series_name.clone())
                                        .unwrap_or_default();
                                    ctx.mpv.item = Some(item.clone());
                                    // The session playlist / queue entry
                                    // still shows the URL-derived title
                                    // ("stream"): use the real name.
                                    crate::core::mpv::update_jellyfin_entry_title(
                                        &ctx,
                                        &item.id,
                                        &item.name,
                                    );
                                }
                                // The Jellyfin tab's info box shows the full
                                // item metadata (identical to the queue
                                // tab's): route it to the pane as well.
                                let data = crate::MpdQueryResult::Any(Box::new(
                                    crate::jellyfin::JellyfinResult::Item(item),
                                ));
                                if let Err(err) = ui.on_command_finished(
                                    crate::ui::panes::jellyfin::JF_ITEM,
                                    Some(crate::config::tabs::PaneType::Jellyfin { tree: crate::config::tabs::TreeBrowserArgs::default() }),
                                    data,
                                    &mut ctx,
                                ) {
                                    log::error!(error:? = err; "UI failed to handle jellyfin item result");
                                }
                                render_wanted = true;
                            }
                            (crate::ui::panes::jellyfin::JF_MPRIS, JellyfinResult::Mpris { item, image }) => {
                                // mpv video session: title/artist for the
                                // media controls + the poster written where
                                // the MPRIS bridge can serve it.
                                if ctx.mpv.active
                                    && ctx.mpv.item_id.as_deref() == Some(item.id.as_str())
                                {
                                    ctx.mpv.title = item.name.clone();
                                    ctx.mpv.artist = item
                                        .album_artist
                                        .clone()
                                        .or_else(|| item.artist.clone())
                                        .or_else(|| item.series_name.clone())
                                        .unwrap_or_default();
                                    // Stash the item metadata so the info box
                                    // can show the video's details.
                                    ctx.mpv.item = Some(item.clone());
                                    // The session playlist / queue entry
                                    // still shows the URL-derived title
                                    // ("stream"): use the real name.
                                    crate::core::mpv::update_jellyfin_entry_title(
                                        &ctx,
                                        &item.id,
                                        &item.name,
                                    );
                                    if !image.is_empty() {
                                        let path = crate::ui::modals::paste::mpv_mpris_art_path(
                                            ctx.config.cache_dir.as_deref(),
                                        );
                                        if let Some(parent) = path.parent() {
                                            let _ = std::fs::create_dir_all(parent);
                                        }
                                        if std::fs::write(&path, &image).is_ok() {
                                            ctx.mpv.art_path = Some(path);
                                        }
                                    }
                                } else {
                                    // Still on the same stream? Tag its queue
                                    // entry (title/artist/album) so MPRIS shows
                                    // the episode/movie name, and write the
                                    // thumbnail for the media controls.
                                    let still_current = ctx
                                        .find_current_song_in_queue()
                                        .is_some_and(|(_, song)| {
                                            crate::jellyfin::item_id_from_url(&song.file)
                                                .is_some_and(|id| id == item.id)
                                        });
                                    if still_current {
                                        if let Some(song_id) = ctx.status.songid {
                                            let name = item.name.clone();
                                            let artist = item
                                                .album_artist
                                                .clone()
                                                .or_else(|| item.artist.clone())
                                                .or_else(|| item.series_name.clone())
                                                .unwrap_or_default();
                                            let album =
                                                item.album.clone().unwrap_or_else(|| name.clone());
                                            ctx.command(move |client| {
                                                let _ = client.add_tag_id(song_id, "title", &name);
                                                let _ =
                                                    client.add_tag_id(song_id, "artist", &artist);
                                                let _ = client.add_tag_id(song_id, "album", &album);
                                                Ok(())
                                            });
                                        }
                                        if !image.is_empty() {
                                            crate::core::work::save_mpris_art(
                                                ctx.config.cache_dir.as_deref(),
                                                &image,
                                            );
                                        }
                                    }
                                }
                                render_wanted = true;
                            }
                            (
                                crate::ui::panes::jellyfin::JF_RESUME,
                                JellyfinResult::ResumePosition { seconds, .. },
                            ) => {
                                if ctx.mpv.active && seconds > 10.0 {
                                    if let Some(socket) = ctx.mpv.socket.clone() {
                                        crate::core::mpv::mpv_seek(&socket, seconds);
                                        ctx.mpv.position = seconds;
                                    } else {
                                        // Socket not up yet; the poll applies
                                        // it once reachable.
                                        ctx.mpv.pending_seek = Some(seconds);
                                    }
                                }
                                render_wanted = true;
                            }
                            (
                                crate::ui::panes::jellyfin::JF_SEASON_PLAY,
                                JellyfinResult::SeasonPlaylist { entries, start_index },
                            ) => {
                                use crate::core::mpv::{MpvPlaylistEntry, run_mpv_playlist};
                                let entries: Vec<MpvPlaylistEntry> = entries
                                    .into_iter()
                                    .map(|e| MpvPlaylistEntry::new(e.title, e.url, e.duration))
                                    .collect();
                                if ctx.mpv.active {
                                    // The switch prompt already switched mpv
                                    // to the clicked episode; record its
                                    // season as the Video view's playlist,
                                    // rotated so the clicked episode is first
                                    // (mpv's own playlist must match: the
                                    // prompt used `loadfile … replace`, which
                                    // splices the clicked file into the *old*
                                    // playlist instead of replacing it — a
                                    // same-length old season would then leave
                                    // mpv's `playlist-pos` pointing at a
                                    // stale position that misindexes the
                                    // recorded playlist (+1 title). Rebuild
                                    // mpv's playlist to the rotated season so
                                    // it equals the recorded one).
                                    let mut entries = entries;
                                    if let Some(idx) = start_index
                                        .checked_rem(entries.len())
                                        .filter(|i| *i > 0)
                                    {
                                        entries.rotate_left(idx);
                                    }
                                    if let Some(socket) = ctx.mpv.socket.clone()
                                        && let Some(first) = entries.first()
                                    {
                                        // Clear the old entries; the current
                                        // file survives at position 0.
                                        crate::core::mpv::mpv_playlist_clear(&socket);
                                        // The switch prompt already loaded the
                                        // clicked episode (`loadfile …
                                        // replace`): when it is still the
                                        // current file, only the rest of the
                                        // season needs appending — reloading
                                        // it would restart the episode and
                                        // drop an already-applied resume
                                        // seek. Reload only when the current
                                        // file is not the first entry (the
                                        // prompt's load raced or failed).
                                        // Compare by Jellyfin item id (mpv
                                        // may report the path with
                                        // different query params), falling
                                        // back to the exact URL.
                                        let current_is_first =
                                            crate::core::mpv::read_mpv_path(&socket)
                                                .is_some_and(|p| {
                                                    let a = crate::jellyfin::item_id_from_url(&p);
                                                    let b = crate::jellyfin::item_id_from_url(
                                                        &first.url,
                                                    );
                                                    match (a, b) {
                                                        (Some(a), Some(b)) => a == b,
                                                        _ => p == first.url,
                                                    }
                                                });
                                        if current_is_first {
                                            for entry in entries.iter().skip(1) {
                                                crate::core::mpv::mpv_append_load(
                                                    &socket,
                                                    &entry.url,
                                                );
                                            }
                                        } else {
                                            crate::core::mpv::mpv_loadfile(
                                                &socket,
                                                &first.url,
                                            );
                                            for entry in entries.iter().skip(1) {
                                                crate::core::mpv::mpv_append_load(
                                                    &socket,
                                                    &entry.url,
                                                );
                                            }
                                        }
                                    }
                                    *ctx.mpv.playlist.borrow_mut() = entries;
                                    ctx.mpv.playlist_pos.set(Some(0));
                                } else {
                                    run_mpv_playlist(&ctx, entries, Some(start_index));
                                }
                                render_wanted = true;
                            }
                            (crate::ui::panes::album_art::JF_VIDEO_ART, data) => {
                                // The primary image of the video playing in
                                // mpv, shown as album art by the AlbumArt
                                // pane. Fetch failures are logged, not shown
                                // in the Jellyfin pane.
                                if matches!(data, JellyfinResult::Image { .. }) {
                                    let data = crate::MpdQueryResult::Any(Box::new(data));
                                    if let Err(err) = ui.on_command_finished(
                                        crate::ui::panes::album_art::JF_VIDEO_ART,
                                        Some(crate::config::tabs::PaneType::AlbumArt),
                                        data,
                                        &mut ctx,
                                    ) {
                                        log::error!(error:? = err; "UI failed to handle video art result");
                                    }
                                } else {
                                    log::debug!(data:?; "Video art fetch failed");
                                }
                                render_wanted = true;
                            }
                            (crate::ui::panes::jellyfin::JF_CHAPTERS, data) => {
                                // The chapter markers of the video playing in
                                // mpv (or the current song): route them to
                                // the Jellyfin pane, then let the Queue tab
                                // follow the video into its Chapters list.
                                let data = crate::MpdQueryResult::Any(Box::new(data));
                                if let Err(err) = ui.on_command_finished(
                                    crate::ui::panes::jellyfin::JF_CHAPTERS,
                                    Some(crate::config::tabs::PaneType::Jellyfin { tree: crate::config::tabs::TreeBrowserArgs::default() }),
                                    data,
                                    &mut ctx,
                                ) {
                                    log::error!(error:? = err; "UI failed to handle chapters result");
                                }
                                if let Err(err) = ui.follow_video_session(&ctx) {
                                    log::error!(error:? = err; "Failed to follow the video session in the queue tab");
                                }
                                render_wanted = true;
                            }
                            (id, data) => {
                                let data = crate::MpdQueryResult::Any(Box::new(data));
                                if let Err(err) = ui.on_command_finished(
                                    id,
                                    Some(crate::config::tabs::PaneType::Jellyfin { tree: crate::config::tabs::TreeBrowserArgs::default() }),
                                    data,
                                    &mut ctx,
                                ) {
                                    log::error!(error:? = err; "UI failed to handle jellyfin result");
                                }
                                render_wanted = true;
                            }
                        }
                    }
                    WorkDone::YtDlpPlaylistResolved { urls } => {
                        ctx.ytdlp_manager.queue_download_many(urls);
                        ctx.ytdlp_manager.download_next();
                    }
                    WorkDone::YtDlpDownloaded { id, result, spec } => {
                        match ctx.ytdlp_manager.resolve_download(id, result) {
                            Ok((result, position)) => {
                                let cache_dir = ctx.config.cache_dir.clone();
                                match spec {
                                    // A stream download (the controls'
                                    // Download button or a right-click
                                    // replace): run the spec's replace
                                    // action with every produced file.
                                    Some(spec) => complete_stream_download(
                                        &ctx,
                                        &spec,
                                        &result.file_paths,
                                        position,
                                    ),
                                    None => {
                                        let path = result.file_path;
                                        ctx.command(move |client| {
                                            client.add_downloaded_file_to_queue(
                                                path,
                                                cache_dir.as_deref(),
                                                position,
                                            )?;
                                            Ok(())
                                        });
                                    }
                                }
                            }
                            Err(err) => {
                                status_error!("Yt-dlp resulted in error: {err}");
                            }
                        }
                        ctx.ytdlp_manager.download_next();
                        if let Err(err) = ui.on_event(UiEvent::DownloadsUpdated, &mut ctx) {
                            log::error!(error:? = err; "UI failed to handle DownloadsUpdated event");
                        }
                    }
                    WorkDone::SearchYtResults { items, position, interactive } => {
                        if items.is_empty() {
                            status_warn!("No results found");
                        } else if !interactive {
                            let result = ctx.ytdlp_manager.download_url(&items[0].url, position);
                            match result {
                                Ok(()) => {
                                    if ctx.config.auto_open_downloads {
                                        modal!(ctx, DownloadsModal::new(&ctx));
                                    }
                                }
                                Err(err) => {
                                    status_error!("Failed to download first search result: {err}");
                                }
                            }
                        } else {
                            let labels: Vec<String> = items
                                .iter()
                                .map(|it| it.title.as_deref().unwrap_or("<no title>").to_string())
                                .collect();

                            let modal = SelectModal::builder()
                                .ctx(&ctx)
                                .title("Search results")
                                .confirm_label("Select")
                                .options(labels)
                                .on_confirm(move |ctx, _label, idx| {
                                    let result =
                                        ctx.ytdlp_manager.download_url(&items[idx].url, position);
                                    match result {
                                        Ok(()) => {
                                            if ctx.config.auto_open_downloads {
                                                modal!(ctx, DownloadsModal::new(ctx));
                                            }
                                        }
                                        Err(err) => {
                                            status_error!(
                                                "Failed to download selected item: {err}"
                                            );
                                        }
                                    }
                                    Ok(())
                                })
                                .build();

                            if let Err(err) =
                                ui.on_ui_app_event(UiAppEvent::Modal(Box::new(modal)), &mut ctx)
                            {
                                log::error!(error:? = err; "UI failed to handle modal event");
                            }
                        }

                        render_wanted = true;
                    }
                    WorkDone::ImageResized { data } => {
                        let event = match data {
                            Ok(data) => UiEvent::ImageEncoded { data },
                            Err(err) => UiEvent::ImageEncodeFailed { err },
                        };

                        if let Err(err) = ui.on_event(event, &mut ctx) {
                            log::error!(error:? = err; "UI failed to handle image resized event");
                        }
                        // The encoded image is drawn after the next frame's
                        // buffer flush; make sure a frame actually runs.
                        render_wanted = true;
                    }
                    WorkDone::LyricsIndexed { index } => {
                        ctx.lrc_index = index;
                        if let Err(err) = ui.on_event(UiEvent::LyricsIndexed, &mut ctx) {
                            log::error!(error:? = err; "UI failed to handle lyrics indexed event");
                        }
                    }
                    WorkDone::SingleLrcIndexed { path, metadata } => {
                        if let Some(metadata) = metadata {
                            ctx.lrc_index.add(path, metadata);
                        }
                        if let Err(err) = ui.on_event(UiEvent::LyricsIndexed, &mut ctx) {
                            log::error!(error:? = err; "UI failed to handle single lyrics indexed event");
                        }
                    }
                    WorkDone::MpdCommandFinished { id, target, data } => match (id, target, data) {
                        (GLOBAL_STICKERS_UPDATE, None, MpdQueryResult::SongStickers(stickers)) => {
                            ctx.set_stickers(stickers);
                            render_wanted = true;
                        }
                        (
                            GLOBAL_STATUS_UPDATE,
                            None,
                            MpdQueryResult::Status { data: status, source_event },
                        ) => {
                            let current_song_id =
                                ctx.find_current_song_in_queue().map(|(_, song)| song.id);
                            let previous_state = ctx.status.state;
                            let current_updating_db = ctx.status.updating_db;
                            let current_playlist = ctx.status.lastloadedplaylist.take();
                            let previous_status = std::mem::replace(&mut ctx.status, status);
                            let new_playlist = ctx.status.lastloadedplaylist.as_ref();
                            let mut song_changed = false;

                            if ctx.config.reflect_changes_to_playlist
                                && matches!(source_event, Some(IdleEvent::Playlist))
                            {
                                // Try to reflect changes to saved playlist if any was loaded both
                                // before and after the update
                                if let (Some(current_playlist), Some(new_playlist)) =
                                    (current_playlist, new_playlist)
                                    && &current_playlist == new_playlist
                                {
                                    let playlist_name = current_playlist.clone();
                                    ctx.command(move |client| {
                                        client.save_queue_as_playlist(
                                            &playlist_name,
                                            Some(SaveMode::Replace),
                                        )?;
                                        Ok(())
                                    });
                                }
                            }

                            let mut start_render_loop = || {
                                _update_db_loop_guard = Some(ctx.scheduler.repeated(
                                    Duration::from_millis(250),
                                    |(tx, _)| {
                                        tx.send(AppEvent::RequestRender)?;
                                        Ok(())
                                    },
                                ));
                            };
                            match (current_updating_db, ctx.status.updating_db) {
                                (None, Some(_)) => {
                                    // update of db started
                                    ctx.db_update_start = Some(std::time::Instant::now());
                                    start_render_loop();
                                }
                                (Some(_), Some(_)) if ctx.db_update_start.is_none() => {
                                    // rmpc is opened after db started updating
                                    // beforehand so we reassign
                                    ctx.db_update_start = Some(std::time::Instant::now());
                                    start_render_loop();
                                }
                                (Some(_), None) => {
                                    // update of db ended
                                    ctx.db_update_start = None;
                                    _update_db_loop_guard = None;
                                }
                                _ => {}
                            }

                            if previous_state != ctx.status.state
                                && let Err(err) =
                                    ui.on_event(UiEvent::PlaybackStateChanged, &mut ctx)
                            {
                                status_error!(error:? = err; "UI failed to handle playback state changed event, error: '{}'", err.to_status());
                            }

                            // Starting MPD playback pauses an mpv video
                            // launched from s2udio (they never run together).
                            if previous_state != State::Play
                                && ctx.status.state == State::Play
                                && crate::core::mpv::MPV_RUNNING
                                    .load(std::sync::atomic::Ordering::Relaxed)
                            {
                                log::debug!("MPD playback started; pausing mpv");
                                crate::core::mpv::pause_mpv();
                            }

                            // The mpv video / MPD audio UI-source switch
                            // (music starts or stops while the video is
                            // paused): refresh the album art box so it
                            // follows the active source (the same switch
                            // the mpv poll catches on the pause side).
                            if previous_state != ctx.status.state && ctx.mpv.active {
                                let source_before = !ctx.mpv.paused
                                    || previous_state != State::Play;
                                let source_after =
                                    !ctx.mpv.paused || ctx.status.state != State::Play;
                                if source_before != source_after
                                    && let Err(err) = ui.refresh_album_art(&ctx)
                                {
                                    log::error!(error:? = err; "Failed to refresh album art after the playback source switched");
                                }
                            }

                            match ctx.status.state {
                                State::Play if previous_state == ctx.status.state => {
                                    if let Some(played) = &mut ctx.song_played {
                                        *played += ctx.last_status_update.elapsed();
                                    }
                                }
                                State::Play if previous_state != ctx.status.state => {
                                    _update_loop_guard = ctx
                                        .config
                                        .status_update_interval_ms
                                        .map(Duration::from_millis)
                                        .map(|interval| {
                                            ctx.scheduler.repeated(interval, run_status_update)
                                        });
                                }
                                State::Play => {}
                                State::Pause => {
                                    _update_loop_guard = None;
                                }
                                State::Stop => {
                                    song_changed = true;
                                    ctx.song_played = None;
                                    _update_loop_guard = None;
                                }
                            }

                            if let Some((_, song)) = ctx.find_current_song_in_queue()
                                && Some(song.id) != current_song_id
                            {
                                // Round 38: `lyrics_source: LocalOnly` skips the
                                // hook (the network-fetch vehicle) entirely.
                                if let Some(command) = &ctx.config.on_song_change
                                    && ctx.config.lyrics_source != LyricsSource::LocalOnly
                                {
                                    let mut env = create_env(&ctx, std::iter::empty());

                                    let prev_song_file = (previous_status.state != State::Stop)
                                        .then_some(previous_status.song.and_then(|idx| {
                                            ctx.queue.get(idx).map(|song| song.file.clone())
                                        }))
                                        .flatten();

                                    if let (Some(prev_song), Some(played)) =
                                        (prev_song_file, ctx.song_played)
                                    {
                                        env.push(("PREV_SONG".to_owned(), prev_song));
                                        env.push((
                                            "PREV_ELAPSED".to_owned(),
                                            played.as_secs().to_string(),
                                        ));
                                    }

                                    run_external(command.clone(), env);
                                }
                                song_changed = true;
                                ctx.song_played = Some(Duration::ZERO);
                            }
                            if song_changed
                                && let Err(err) = ui.on_event(UiEvent::SongChanged, &mut ctx)
                            {
                                status_error!(error:? = err; "UI failed to handle idle event, error: '{}'", err.to_status());
                            }
                            if song_changed {
                                // Chapters (YouTube/Jellyfin/local) for the
                                // new track, and MPRIS metadata (title +
                                // thumbnail) for playing streams. A
                                // chaptered track auto-opens the Queue
                                // tab's Chapters list (the active tab is
                                // never switched).
                                crate::ui::modals::paste::ensure_chapters(&ctx);
                                crate::ui::modals::paste::ensure_mpris_metadata(&ctx);
                                ctx.auto_show_chapters();
                                ctx.metadata_processed_song = ctx.status.songid;
                            }

                            ctx.last_status_update = Instant::now();
                            render_wanted = true;
                        }
                        (GLOBAL_VOLUME_UPDATE, None, MpdQueryResult::Volume(volume)) => {
                            let new_volume = *volume.value();
                            ctx.status.volume = volume;
                            // MPRIS clients (the KDE media widget / media
                            // keys, via mpDris2) adjust MPD's volume. While a
                            // video plays in mpv, forward the change to the
                            // mpv session (the actual audio source) and
                            // mirror it into ctx.mpv.volume so the volume bar
                            // updates immediately instead of showing the
                            // stale mpv value the poll read earlier.
                            if crate::core::mpv::mpv_is_ui_source(&ctx)
                                && let Some(socket) = ctx.mpv.socket.clone()
                            {
                                crate::core::mpv::mpv_exchange_volume(
                                    &socket,
                                    f64::from(new_volume),
                                );
                                ctx.mpv.volume = Some(new_volume as u8);
                            }
                            render_wanted = true;
                        }
                        (GLOBAL_QUEUE_UPDATE, None, MpdQueryResult::Queue(queue)) => {
                            ctx.queue = queue.unwrap_or_default();
                            ctx.cached_queue_time_total =
                                ctx.queue.iter().filter_map(|s| s.duration).sum();
                            render_wanted = true;
                            log::debug!(len = ctx.queue.len(); "Queue updated");
                            if let Err(err) = ui.on_event(UiEvent::QueueChanged, &mut ctx) {
                                status_error!(error:? = err; "Ui failed to handle queue changed event, error: '{}'", err.to_status());
                            }
                            // A ReplaceAndPlay re-resolution changes MPD's
                            // song id (delete_id + add_id): the status
                            // update for the new song lands before this
                            // queue refresh, so the song-change check in the
                            // status handler could not find the new id in
                            // the stale queue and skipped the metadata
                            // pipeline. Re-evaluate once the refreshed queue
                            // makes the current song visible; the marker
                            // keeps steady-state queue updates from redoing
                            // it.
                            if let Some(songid) = ctx.status.songid
                                && ctx.metadata_processed_song != Some(songid)
                                && ctx.find_current_song_in_queue().is_some()
                            {
                                ctx.metadata_processed_song = Some(songid);
                                crate::ui::modals::paste::ensure_chapters(&ctx);
                                crate::ui::modals::paste::ensure_mpris_metadata(&ctx);
                                ctx.auto_show_chapters();
                            }
                        }
                        (
                            EXTERNAL_COMMAND,
                            None,
                            MpdQueryResult::ExternalCommand(command, songs),
                        ) => {
                            let songs = songs.iter().map(|s| s.file.as_str());
                            run_external(command, create_env(&ctx, songs));
                        }
                        (id, target, data) => {
                            if let Err(err) = ui.on_command_finished(id, target, data, &mut ctx) {
                                log::error!(error:? = err; "UI failed to handle command finished event");
                            }
                        }
                    },
                    WorkDone::None => {}
                },
                AppEvent::WorkDone(Err(err)) => {
                    status_error!("{}", err);
                }
                AppEvent::Resized { columns, rows } => {
                    ui.set_resizing(true, &ctx);
                    ctx.scheduler.schedule_replace(
                        *ON_RESIZE_SCHEDULE_ID,
                        Duration::from_millis(500),
                        move |(tx, _)| {
                            tx.send(AppEvent::ResizedDebounced { columns, rows })?;
                            Ok(())
                        },
                    );
                    render_wanted = true;
                }
                AppEvent::ResizedDebounced { columns, rows } => {
                    ui.set_resizing(false, &ctx);
                    if let Err(err) = ui.resize(Rect::new(0, 0, columns, rows), &ctx) {
                        log::error!(error:? = err, event:?; "UI failed to handle resize event");
                    }

                    if let Some(cmd) = &ctx.config.on_resize {
                        let cmd = Arc::clone(cmd);
                        let mut env = create_env(&ctx, std::iter::empty::<&str>());
                        env.push(("COLS".to_owned(), columns.to_string()));
                        env.push(("ROWS".to_owned(), rows.to_string()));
                        log::debug!("Executing on resize");
                        run_external(cmd, env);
                    }
                    if let Err(err) = terminal.clear() {
                        log::error!(error:? = err; "Failed to clear terminal after a resize");
                    }
                    // Draw the resized UI twice so the second pass is clean.
                    resize_render_passes = 2;
                    render_wanted = true;
                }
                AppEvent::UiEvent(event) => match ui.on_ui_app_event(event, &mut ctx) {
                    Ok(()) => {}
                    Err(err) => {
                        status_error!(err:?; "Error: {}", err.to_status());
                        render_wanted = true;
                    }
                },
                AppEvent::RemoteSwitchTab { tab_name } => {
                    let target_tab = tab_name.as_str().into();

                    if let Some(tab) =
                        ctx.config.tabs.names.iter().find(|&name| *name == target_tab)
                    {
                        if let Err(err) =
                            ui.on_ui_app_event(UiAppEvent::ChangeTab(tab.clone()), &mut ctx)
                        {
                            status_error!(err:?; "Error switching to tab '{}': {}", tab_name, err.to_status());
                        }
                    } else {
                        let available = ctx
                            .config
                            .tabs
                            .names
                            .iter()
                            .map(|name| name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        status_error!(
                            "Tab '{}' does not exist. Available tabs: {}",
                            tab_name,
                            available
                        );
                    }
                    render_wanted = true;
                }
                AppEvent::IpcQuery { mut stream, targets } => {
                    for target in targets {
                        match target {
                            RemoteCommandQuery::ActiveTab => {
                                stream
                                    .insert_response(target.to_string(), ctx.active_tab.0.as_str());
                            }
                        }
                    }
                }
                AppEvent::BlurCheck => {
                    // Apply the active blur mode's colors only when the
                    // schedule changed since the last check.
                    let mode = crate::core::blur::read_schedule_mode();
                    if mode != last_blur_mode {
                        if let Some(mode) = &mode {
                            let mut config = ctx.config.as_ref().clone();
                            match crate::core::blur::apply_mode_color(&mut config, mode) {
                                Ok(true) => {
                                    log::info!(mode:?; "Applying blur mode colors");
                                    ctx.config = std::sync::Arc::new(config);
                                    last_blur_mode = Some(mode.clone());
                                    if let Err(err) =
                                        ui.on_event(UiEvent::ConfigChanged, &mut ctx)
                                    {
                                        log::error!(
                                            error:? = err; "UI failed to handle blur theme change"
                                        );
                                    }
                                    render_wanted = true;
                                }
                                Ok(false) => {
                                    // Mode has no readable colors (yet); retry
                                    // on the next tick.
                                    log::debug!(mode:?; "No blur colors found, retrying");
                                }
                                Err(err) => {
                                    log::error!(error:? = err, mode:?; "Blur color apply failed");
                                }
                            }
                        } else {
                            last_blur_mode = None;
                        }
                    }
                }
                AppEvent::Reconnected => {
                    for ev in [IdleEvent::Player, IdleEvent::Playlist, IdleEvent::Options] {
                        handle_idle_event(ev, &ctx, &mut additional_evs);
                    }
                    if let Err(err) = ui.on_event(UiEvent::Reconnected, &mut ctx) {
                        log::error!(error:? = err, event:?; "UI failed to handle resize event");
                    }
                    status_warn!("rmpc reconnected to MPD and will reinitialize");
                    connected = true;
                }
                AppEvent::LostConnection => {
                    if ctx.status.state != State::Stop {
                        _update_loop_guard = None;
                        ctx.status.state = State::Stop;
                    }
                    if connected {
                        status_error!("rmpc lost connection to MPD and will try to reconnect");
                    }
                    connected = false;
                }
                AppEvent::TmuxHook { hook } => {
                    if let Some(tmux) = &mut tmux {
                        let old_visible = tmux.visible;
                        if let Err(err) = tmux.update_visible() {
                            log::error!(err:?, hook:?; "Failed to update tmux visibility");
                            continue;
                        }

                        let event = match (tmux.visible, old_visible) {
                            (true, false) => UiEvent::Displayed,
                            (false, true) => UiEvent::Hidden,
                            _ => continue,
                        };

                        match ui.on_event(event, &mut ctx) {
                            Ok(()) => {}
                            Err(err) => {
                                status_error!(err:?; "Error: {}", err.to_status());
                                render_wanted = true;
                            }
                        }
                    }
                }
            }
        }
        if render_wanted {
            let till_next_frame =
                min_frame_duration.saturating_sub(now.duration_since(last_render));
            if till_next_frame != Duration::ZERO {
                continue;
            }
            // The cava row was removed (video playback on the Queue tab, or
            // entering the Jellyfin tab): clear the whole window and draw
            // twice so the visualizer's terminal-side overlay leaves no
            // stale cells.
            if ui.take_cava_refresh() {
                if let Err(err) = terminal.clear() {
                    log::error!(error:? = err; "Failed to clear terminal after hiding cava");
                }
                resize_render_passes = 2;
            }
            let completed_frame = terminal
                .draw(|frame| {
                    if let Err(err) = ui.render(frame, &mut ctx) {
                        log::error!(error:? = err; "Failed to render a frame");
                    }
                })
                .expect("Expected render to succeed");

            // Terminal-side overlays (the album art, the Jellyfin poster)
            // are drawn after the buffer flush: the flush would otherwise
            // overwrite their kitty placeholder cells with the frame's
            // content (e.g. the first frame after the paste popup closes
            // redraws the art-pane area and deletes the transient image).
            // A stale-sized encode is dropped and re-encoded instead of
            // drawn, so the image can never cover the UI. The flushed
            // frame's buffer lets the album art re-place its image exactly
            // when this frame's diff rewrote the art pane area, instead of
            // re-placing after every frame (which strobes the art while
            // playing).
            if let Err(err) = ui.flush_album_art(completed_frame.buffer, &ctx) {
                log::error!(error:? = err; "Failed to flush album art");
            }
            if let Err(err) = ui.flush_pending_overlays(&ctx) {
                log::error!(error:? = err; "Failed to flush pending overlays");
            }
            // The cava bars are a terminal-side overlay too: a Start that
            // was deferred (so the bars never paint before the UI) fires
            // only once the flushed frame is on screen.
            if let Err(err) = ui.maybe_start_cava(&ctx) {
                log::error!(error:? = err; "Failed to start cava after frame");
            }

            ctx.finish_frame();
            last_render = now;
            if resize_render_passes > 0 {
                // One more pass on the next loop iteration.
                resize_render_passes -= 1;
                render_wanted = true;
            } else {
                render_wanted = false;
            }
        }
    }

    terminal
}

/// Start mpv on a torrent's stream entries (round 17; the fresh-engine
/// single-file path and the scanned Play all / Select files… path
/// converge here): keep the engine alive in `Ctx.torrent_engine`, record
/// the session playlist (the Queue tab's Video list, like a Jellyfin
/// season play), insert synthetic yt-info entries (title = file name,
/// channel = torrent name — in memory only, never persisted: the stream
/// URL embeds the rqbit auth token) and — for "Play and Download" — start
/// the 1 s stats-poll job. `download` carries `(torrent_id, file_idx,
/// file_length, file_name, stream_url)` of the picked file.
#[allow(clippy::too_many_arguments)]
fn start_torrent_playback(
    ctx: &mut Ctx,
    engine: std::sync::Arc<crate::core::torrent::TorrentEngine>,
    torrent_id: String,
    torrent_name: String,
    entries: Vec<crate::core::mpv::MpvPlaylistEntry>,
    download: Vec<crate::core::torrent::TorrentDownloadFile>,
    torrent_download_guard: &mut Option<
        crate::core::scheduler::TaskGuard<(Sender<AppEvent>, Sender<ClientRequest>)>,
    >,
) {
    // Keep the engine alive for the whole session (the last Arc clone's
    // Drop kills the rqbit child on app exit). M4 adds the
    // keep_after_play/cleanup policy.
    *ctx.torrent_engine.borrow_mut() = Some(engine.clone());
    status_info!("Streaming {torrent_name}…");
    // Now-playing info: each file's name is its mpv entry title; the
    // torrent name becomes the MPRIS artist. Synthetic yt-info entries
    // keyed by each stream URL feed the info box / MPRIS / queue rows —
    // in memory only, the URL embeds the auth token.
    ctx.mpv.artist = torrent_name.clone();
    crate::ui::modals::paste::remember_torrent_entries(ctx, &torrent_name, &entries);
    crate::core::mpv::play_video_entries(ctx, entries);
    if !download.is_empty() {
        // "Play and Download" / the picker's "Download & Play": keep the
        // engine downloading and move the completed file(s) to
        // s2udio-downloads once done (deferred until mpv stops using a
        // stream).
        start_torrent_download(
            ctx,
            engine,
            torrent_id,
            torrent_name,
            download,
            torrent_download_guard,
        );
    }
}

/// Start a download-only torrent job (round 21): keep the engine running
/// (the popup's "Download" / "Download all", and the fresh
/// `TorrentDownloadPrepared` path), poll its stats once per second and
/// move every kept file to `s2udio-downloads` when the torrent's download
/// is complete. No playback.
fn start_torrent_download(
    ctx: &mut Ctx,
    engine: std::sync::Arc<crate::core::torrent::TorrentEngine>,
    torrent_id: String,
    torrent_name: String,
    files: Vec<crate::core::torrent::TorrentDownloadFile>,
    torrent_download_guard: &mut Option<
        crate::core::scheduler::TaskGuard<(Sender<AppEvent>, Sender<ClientRequest>)>,
    >,
) {
    if files.is_empty() {
        status_warn!("No files to download");
        return;
    }
    // Keep the engine alive for the whole job (the last Arc clone's Drop
    // kills the rqbit child on app exit).
    *ctx.torrent_engine.borrow_mut() = Some(engine.clone());
    let files_n = files.len();
    status_info!(
        "Downloading {} ({} file{})…",
        torrent_name,
        files_n,
        if files_n == 1 { "" } else { "s" }
    );
    *ctx.torrent_download.borrow_mut() = Some(crate::core::torrent::TorrentDownload {
        engine_base_url: engine.base_url().to_owned(),
        torrent_id,
        torrent_name,
        files,
        complete: false,
        deferred: false,
        failures: 0,
    });
    *torrent_download_guard = Some(ctx.scheduler.repeated(
        Duration::from_secs(1),
        move |(tx, _)| {
            let _ = tx.send(AppEvent::TorrentDownloadPoll);
            Ok(())
        },
    ));
}

/// Move a completed "Play and Download" torrent file into
/// `~/Downloads/s2udio-downloads` (outside the MPD library; the browser
/// of the folder and delete the torrent from the engine (which removes
/// the remaining cache files — subtitles, poster, partials). Called when
/// the download's poll reports completion while mpv is not using the
/// stream, or from MpvSessionEnded for a deferred completion.
fn finish_torrent_download(
    ctx: &Ctx,
    engine: &crate::core::torrent::TorrentEngine,
) {
    let (torrent_id, files) = {
        let job = ctx.torrent_download.borrow();
        let Some(job) = job.as_ref() else { return };
        (job.torrent_id.clone(), job.files.clone())
    };
    let Some(dest_dir) = crate::ui::modals::paste::downloads_dir() else {
        status_warn!("Cannot determine the downloads folder (~/Downloads) — the downloaded file stays in the torrent cache");
        return;
    };
    // No MPD update needed: the folder lives outside the MPD library and
    // the browser lists it from disk. Round 21: every kept file moves
    // (a "Download all" job keeps the whole season).
    let mut moved = 0usize;
    for file in &files {
        match crate::core::torrent::move_completed_file(&file.source_path, &dest_dir) {
            Ok(_) => {
                status_info!("Downloaded '{}' to s2udio-downloads", file.file_name);
                moved += 1;
            }
            Err(err) => {
                status_warn!(
                    "Failed to keep downloaded file '{}': {err}",
                    file.file_name
                );
            }
        }
    }
    if moved > 0
        && let Err(err) = crate::core::torrent::delete_torrent(engine, &torrent_id)
    {
        log::warn!(error:? = err; "Failed to delete the completed torrent from the engine");
    }
}

/// Finish a stream download (`s2udio-downloads` save-as): run the spec's
/// replace action with the produced files. The persistent video queue is
/// s2udio-internal state and swaps here; queue/playlist replacements run
/// on the MPD client thread (with the downloads dir indexed first).
fn complete_stream_download(
    ctx: &Ctx,
    spec: &crate::shared::ytdlp::StreamDownloadSpec,
    files: &[std::path::PathBuf],
    _position: Option<crate::mpd::QueuePosition>,
) {
    use crate::shared::ytdlp::ReplaceAction;
    // The persistent video queue holds absolute paths (mpv plays them
    // directly): swap the entry here instead of through MPD.
    if let ReplaceAction::VideoPlaylist { index } = &spec.on_complete
        && let Some(path) = files.first()
    {
        let mut playlist = ctx.video_playlist.borrow_mut();
        if let Some(entry) = playlist.get_mut(*index) {
            entry.url = path.to_string_lossy().into_owned();
            entry.title = path
                .file_stem()
                .map_or_else(|| "Download".to_owned(), |s| s.to_string_lossy().into_owned());
            entry.duration = None;
        }
        drop(playlist);
        crate::ui::modals::paste::save_video_playlist(ctx);
        let _ = ctx.render();
    }
    // Files outside the MPD library (the downloads folder now lives in
    // ~/Downloads/s2udio-downloads) cannot enter the MPD queue or a
    // stored playlist: keep the stream entry and just report the save.
    let files_in_library = files.iter().all(|file| {
        crate::ui::modals::paste::music_directory().is_some_and(|music_dir| {
            file.starts_with(std::path::Path::new(&music_dir))
        })
    });
    match &spec.on_complete {
        ReplaceAction::None => {
            // Just save; the browser lists the folder from disk.
        }
        ReplaceAction::Queue { song_id } => {
            // Outside the library: the stream stays in the queue (MPD
            // cannot play the file) — report the save and leave the
            // entry alone.
            if !files_in_library {
                status_info!(
                    "Saved {} file(s) to s2udio-downloads (outside the MPD library — the stream stays in the queue)",
                    files.len()
                );
                return;
            }
            // The entry's queue position, captured now (it may be deleted
            // once the command runs); None = the entry is already gone,
            // the files are appended instead. When the replaced entry was
            // the one playing, the downloaded file starts playing right
            // away (the delete would otherwise advance the queue).
            let pos = ctx.queue.iter().position(|s| s.id == *song_id);
            let was_current = ctx.status.songid == Some(*song_id);
            let song_id = *song_id;
            let files = files.to_vec();
            ctx.command(move |client| {
                client.replace_downloaded_stream(files, song_id, pos)?;
                if was_current {
                    client.play_pos(pos.unwrap_or(0))?;
                }
                Ok(())
            });
        }
        ReplaceAction::Playlist { name, uri } => {
            // Outside the library: the playlist keeps the stream entry
            // (MPD cannot play the file).
            if !files_in_library {
                status_info!(
                    "Saved {} file(s) to s2udio-downloads (outside the MPD library — the playlist keeps the stream)",
                    files.len()
                );
                return;
            }
            let file = files.first().cloned();
            let name = name.clone();
            let uri = uri.clone();
            ctx.command(move |client| {
                if let Some(file) = file {
                    client.replace_stream_in_playlist(&name, &uri, &file)?;
                }
                Ok(())
            });
        }
        ReplaceAction::VideoPlaylist { .. } => {}
    }
    status_info!("Saved {} file(s) to s2udio-downloads", files.len());
}

fn handle_idle_event(event: IdleEvent, ctx: &Ctx, result_ui_evs: &mut HashSet<IdleEvent>) {
    match event {
        IdleEvent::Mixer if ctx.supported_commands.contains("getvol") => {
            ctx.query()
                .id(GLOBAL_VOLUME_UPDATE)
                .replace_id("volume")
                .query(move |client| Ok(MpdQueryResult::Volume(client.get_volume()?)));
        }
        IdleEvent::Mixer => {
            ctx.query().id(GLOBAL_STATUS_UPDATE).replace_id("status").query(move |client| {
                Ok(MpdQueryResult::Status {
                    data: client.get_status()?,
                    source_event: Some(IdleEvent::Mixer),
                })
            });
        }
        IdleEvent::Options => {
            ctx.query().id(GLOBAL_STATUS_UPDATE).replace_id("status").query(move |client| {
                Ok(MpdQueryResult::Status {
                    data: client.get_status()?,
                    source_event: Some(IdleEvent::Options),
                })
            });
        }
        IdleEvent::Player => {
            ctx.query().id(GLOBAL_STATUS_UPDATE).replace_id("status").query(move |client| {
                Ok(MpdQueryResult::Status {
                    data: client.get_status()?,
                    source_event: Some(IdleEvent::Player),
                })
            });
        }
        IdleEvent::Playlist => {
            ctx.query()
                .id(GLOBAL_QUEUE_UPDATE)
                .replace_id("playlist")
                .query(move |client| Ok(MpdQueryResult::Queue(client.playlist_info()?)));

            // Do not replace because we want to update currently loaded playlist if any
            // Also have to query every time because the current song position may change
            // during queue update (shuffle, move, ...)
            ctx.query().id(GLOBAL_STATUS_UPDATE).replace_id("status_from_playlist").query(
                move |client| {
                    Ok(MpdQueryResult::Status {
                        data: client.get_status()?,
                        source_event: Some(IdleEvent::Playlist),
                    })
                },
            );
        }
        IdleEvent::Sticker => {
            if ctx.stickers_supported.into() {
                let songs: Vec<_> = ctx.stickers().keys().cloned().collect();
                ctx.query().id(GLOBAL_STICKERS_UPDATE).replace_id("global_stickers_update").query(
                    move |client| {
                        Ok(MpdQueryResult::SongStickers(client.fetch_song_stickers(songs)?))
                    },
                );
            }
        }
        IdleEvent::StoredPlaylist => {}
        IdleEvent::Database => {
            ctx.query().id(GLOBAL_STATUS_UPDATE).replace_id("status").query(move |client| {
                Ok(MpdQueryResult::Status {
                    data: client.get_status()?,
                    source_event: Some(IdleEvent::Database),
                })
            });
        }
        IdleEvent::Update => {
            ctx.query().id(GLOBAL_STATUS_UPDATE).replace_id("status").query(move |client| {
                Ok(MpdQueryResult::Status {
                    data: client.get_status()?,
                    source_event: Some(IdleEvent::Update),
                })
            });
        }
        IdleEvent::Output => {}
        IdleEvent::Partition
        | IdleEvent::Subscription
        | IdleEvent::Message
        | IdleEvent::Neighbor
        | IdleEvent::Mount => {
            log::warn!(event:?; "Received unhandled event");
        }
    }

    result_ui_evs.insert(event);
}

mod helpers;
mod mpv;
mod queue_update;
mod status;
mod work_done;

use std::{
    io::Write,
    ops::Sub,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::Duration,
};

use crossterm::{
    queue,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use crossbeam::channel::{Receiver, RecvTimeoutError, Sender};
use ratatui::{Terminal, layout::Rect, prelude::Backend};

use super::command::{create_env, run_external};
use crate::{
    config::{Config, LyricsSource, cli::RemoteCommandQuery},
    ctx::Ctx,
    mpd::commands::State,
    shared::{
        events::{AppEvent, ClientRequest},
        ext::error::ErrorExt,
        id::{self, Id},
        keys::KeyResolver,
        macros::{status_error, status_info},
        terminal::TERMINAL,
    },
    ui::{
        KeyHandleResult, StatusMessage, Ui, UiAppEvent, UiEvent,
        modals::info_modal::InfoModal,
    },
};

/// Round 73.1: RAII bracket for a DEC 2026 synchronized update.
///
/// Everything written between `begin()` and the guard's drop stays in the
/// terminal's update buffer instead of reaching the glass, so the physical
/// clear that the cava-row-drop / album-art-erase repair needs (only a
/// physical clear makes "screen blank" and "diff baseline blank" agree — see
/// the render loop below) never becomes visible. Terminals that do not
/// implement DEC 2026 ignore both escape sequences and simply show the clear,
/// which is exactly the pre-round-71.2 behaviour.
///
/// The `EndSynchronizedUpdate` is not optional: a terminal that does support
/// DEC 2026 keeps the previous frame on the glass until it arrives, so `Drop`
/// emits it on every path, error paths included. Dropping the guard is the
/// only way to end the bracket.
struct SynchronizedUpdate;

impl SynchronizedUpdate {
    /// Emits `BeginSynchronizedUpdate` and flushes it, so the bracket is on
    /// the wire before the clear that follows: nothing may be rendered
    /// between the two. The writer lock is released before returning (the
    /// ratatui backend re-locks it for every write in between).
    fn begin() -> std::io::Result<Self> {
        let tty = TERMINAL.writer();
        let mut writer = tty.lock();
        if let Err(err) = queue!(writer, BeginSynchronizedUpdate).and_then(|()| writer.flush()) {
            // The begin may have reached the terminal (queue! wrote it into
            // the buffer) even though the flush failed: a terminal that did
            // receive it would stay in the synchronized mode and never show
            // another frame, so end it again before reporting the failure.
            // Best effort only — the write path is already broken here.
            let _ = queue!(writer, EndSynchronizedUpdate).and_then(|()| writer.flush());
            return Err(err);
        }
        Ok(Self)
    }
}

impl Drop for SynchronizedUpdate {
    fn drop(&mut self) {
        let tty = TERMINAL.writer();
        let mut writer = tty.lock();
        if let Err(err) = queue!(writer, EndSynchronizedUpdate).and_then(|()| writer.flush())
        {
            log::error!(err:?; "Failed to end the synchronized update");
        }
    }
}

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
    // Round-54 downloader daemon state: re-reads `downloads.json` once
    // per second while the daemon has jobs (or the Downloads modal is
    // open), feeding the Downloads modal's Torrent section and the
    // startup status line.
    let mut dl_state_guard: Option<
        crate::core::scheduler::TaskGuard<(Sender<AppEvent>, Sender<ClientRequest>)>,
    > = None;
    // Round 54: the downloader daemon's jobs may outlive the TUI — show a
    // status line when downloads are in flight at startup, cache the
    // state for the Downloads modal, and keep the 1 s refresh running
    // while the daemon works.
    let initial_dl_state = crate::core::dlctl::read_state();
    let dl_jobs_active = initial_dl_state
        .as_ref()
        .is_some_and(|state| crate::core::dlctl::active_job_count(state) > 0);
    if let Some(state) = &initial_dl_state
        && dl_jobs_active
    {
        let count = crate::core::dlctl::active_job_count(state);
        status_info!(
            "{count} torrent download(s) in progress (Downloads modal / `s2udio dl status`)"
        );
    }
    // Round 56 (56-1) + 56.6 (56.6-4): seed the seen-set with EVERY
    // persisted `Completed` row so a later poll never re-notices them,
    // but surface only the ones that finished recently — a TUI restarted
    // right after a download completed still sees it; old rows must not
    // spam on every launch now that completed rows persist indefinitely.
    if let Some(state) = &initial_dl_state {
        let mut notified = ctx.dl_completed_notified.borrow_mut();
        for job in state
            .jobs
            .iter()
            .filter(|job| job.status == crate::core::dlctl::DlStatus::Completed)
        {
            if notified.insert(job.job_id.clone())
                && crate::core::dlctl::completed_recently(job)
            {
                status_info!("{}", crate::core::dlctl::completion_notice(job));
            }
        }
    }
    ctx.dl_state.replace(initial_dl_state);
    if dl_jobs_active {
        dl_state_guard = Some(ctx.scheduler.repeated(Duration::from_secs(1), move |(tx, _)| {
            let _ = tx.send(AppEvent::DlStatePoll);
            Ok(())
        }));
    }
    ui.before_show(area, &mut ctx).expect("Initial render init to succeed");
    // Round 53: re-apply the persisted replay gain mode on the initial
    // connect. The mode is partition-level server state: it survives client
    // disconnects but MPD does NOT persist it, so it is lost on MPD restarts
    // (AppEvent::Reconnected below covers later reconnects).
    let mut status_state = status::StatusState::new();
    status::apply_replay_gain(&ctx);
    let mut mpv_state = mpv::MpvState::new();
    // mpv video session: poll its IPC socket while active and report
    // playback progress back to Jellyfin (throttled).
    mpv::init(&mut ctx, &mut ui, &mut mpv_state);

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
            status_state.start_update_loop(&ctx);
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
                    mpv::session_started(&mut ctx, &mut ui, &mut mpv_state, url);
                    render_wanted = true;
                }
                AppEvent::MpvSessionEnded => {
                    mpv::session_ended(&mut ctx, &mut ui, &mut mpv_state);
                    render_wanted = true;
                }
                AppEvent::DlStatePoll => {
                    // Refresh the cached `downloads.json` (the Downloads
                    // modal's Torrent section + the status line) and keep
                    // the poll running while the daemon has active jobs.
                    let state = crate::core::dlctl::read_state();
                    let active = state
                        .as_ref()
                        .is_some_and(|s| crate::core::dlctl::active_job_count(s) > 0);
                    ctx.dl_state.replace(state);
                    // Round 56 (56-1): one-shot completion notice for
                    // jobs that transitioned to `Completed` since the
                    // last poll (the per-session seen-set suppresses
                    // duplicates).
                    let mut notified = ctx.dl_completed_notified.borrow_mut();
                    if let Some(state) = ctx.dl_state.borrow().as_ref() {
                        for job in state
                            .jobs
                            .iter()
                            .filter(|job| job.status == crate::core::dlctl::DlStatus::Completed)
                        {
                            if notified.insert(job.job_id.clone()) {
                                status_info!("{}", crate::core::dlctl::completion_notice(job));
                            }
                        }
                    }
                    drop(notified);
                    helpers::open_pending_download_panel(&mut ctx);
                    if let Err(err) = ui.on_event(UiEvent::DownloadsUpdated, &mut ctx) {
                        log::error!(error:? = err; "UI failed to handle DownloadsUpdated event");
                    }
                    if active {
                        if dl_state_guard.is_none() {
                            dl_state_guard = Some(ctx.scheduler.repeated(
                                Duration::from_secs(1),
                                move |(tx, _)| {
                                    let _ = tx.send(AppEvent::DlStatePoll);
                                    Ok(())
                                },
                            ));
                        }
                    } else {
                        dl_state_guard = None;
                    }
                    render_wanted = true;
                }
                AppEvent::YtDlpDownloadsUpdated => {
                    // Round 56.6 (56.6-3): a yt-dlp queue entry was
                    // removed in place (the Downloads modal's "Remove from
                    // list" — the same refresh the download-finished path
                    // sends, so the list + cursor update immediately).
                    if let Err(err) = ui.on_event(UiEvent::DownloadsUpdated, &mut ctx) {
                        log::error!(error:? = err; "UI failed to handle DownloadsUpdated event");
                    }
                    render_wanted = true;
                }
                AppEvent::DlWaitReady { request_id, result } => {
                    crate::ui::modals::paste::on_dl_wait_ready(&mut ctx, &request_id, result);
                    render_wanted = true;
                }
                AppEvent::DlWaitProgress { request_id, elapsed_secs } => {
                    crate::ui::modals::paste::on_dl_wait_progress(
                        &ctx,
                        &request_id,
                        elapsed_secs,
                    );
                    render_wanted = true;
                }
                AppEvent::TorrentScannedPlay { scan, file_indices } => {
                    // Round 17: the paste popup's plain-stream action on
                    // an already-scanned torrent — the engine is running
                    // and the file list is known, so playback starts here
                    // instead of re-scanning on the work thread. Round 20:
                    // the scan map keeps its own `Arc` clone of the engine
                    // (the played scan is NOT consumed), so a repeat paste
                    // of the same torrent reuses the engine instead of
                    // spawning a second rqbit on the same cache dir.
                    // Round 54: committed actions never arrive here — they
                    // run on the `s2udio dl` daemon.
                    let entries = crate::ui::modals::paste::torrent_entries(&scan, &file_indices);
                    let torrent_name = scan.torrent_name.clone();
                    let Some(engine) = scan.engine_or().cloned() else {
                        log::warn!(
                            "Scanned torrent without an engine; cannot start playback"
                        );
                        continue;
                    };
                    helpers::start_torrent_playback(
                        &mut ctx,
                        engine,
                        scan.torrent_id.clone(),
                        torrent_name,
                        entries,
                        scan.info_hash.clone(),
                    );
                    render_wanted = true;
                }
                AppEvent::MpvPoll => match mpv::poll(&mut ctx, &mut ui, &mut mpv_state) {
                    mpv::PollResult::Continue => continue,
                    mpv::PollResult::ContinueRender => {
                        render_wanted = true;
                        continue;
                    }
                    mpv::PollResult::Render => render_wanted = true,
                },
                // Round 107: the queue selection settled while nothing is
                // playing — resolve the highlighted row's description/artwork
                // into the buffer without starting playback.
                AppEvent::WarmQueueSelection => {
                    crate::ui::modals::paste::warm_selected_queue_info(&ctx);
                }
                AppEvent::ActionResolved(mut action) => {
                    match ui.handle_action(&mut action, &mut ctx) {
                        Ok(KeyHandleResult::None) => continue,
                        Ok(KeyHandleResult::Quit) => {
                            // R2.5: the streaming marker goes away with the
                            // TUI — the daemon's stale-marker handling
                            // (mpv /proc probe + bounded wait) takes over.
                            crate::ui::modals::paste::untrack_daemon_streams(&ctx);
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
                    status::handle_idle_event(
                        event,
                        &mut ctx,
                        &mut ui,
                        &mut status_state,
                        &mut render_wanted,
                    );
                }
                AppEvent::RequestRender => {
                    render_wanted = true;
                }
                AppEvent::WorkDone(Ok(result)) => {
                    work_done::handle_work_done(
                        result,
                        &mut ctx,
                        &mut ui,
                        &mut status_state,
                        &mut render_wanted,
                    );
                }
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

                    if let Some(tab) = ctx
                        .config
                        .tabs
                        .names
                        .iter()
                        .find(|&name| *name == target_tab)
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
                AppEvent::RemoteRaise { tab, mut stream } => {
                    // Reuse the tab-switch arm so an unknown tab produces the
                    // same message it does for `remote switchtab`.
                    if let Some(tab_name) = tab {
                        let _ = ctx.app_event_sender.send(AppEvent::RemoteSwitchTab { tab_name });
                    }
                    // Raising spawns window-manager tools and may poll the
                    // journal: never do it on the UI thread.
                    std::thread::spawn(move || match crate::shared::raise::run() {
                        Ok(backend) => {
                            log::info!(backend:% = backend; "Raised the s2udio window");
                            stream.insert_response("backend", backend);
                        }
                        Err(err) => {
                            status_error!("Could not raise the s2udio window: {err}");
                            stream.error(err.to_string());
                        }
                    });
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
                                    if let Err(err) = ui.on_event(UiEvent::ConfigChanged, &mut ctx) {
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
                    status::handle_reconnected(&mut ctx, &mut ui, &mut status_state);
                }
                AppEvent::LostConnection => {
                    status::handle_lost_connection(&mut ctx, &mut status_state);
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
            // entering a tab where the visualizer is hidden): the frame must
            // be repainted in full, because the visualizer paints its bars
            // straight to the terminal (outside the frame buffer) and its
            // band would otherwise leave stale cells behind.
            //
            // Round 73.1: that repair is a physical clear again, bracketed by
            // a DEC 2026 synchronized update so it stays invisible.
            // `swap_buffers()` (round 71.2) could not do it: between frames
            // the *inactive* buffer holds the frame that is on the glass, so
            // the swap threw that record away and left both buffers empty
            // while the terminal kept the old frame — the next diff then had
            // nothing to compare against and every cell the new frame leaves
            // blank (a blank cell equals `Cell::EMPTY`, so no buffer trick can
            // ever write it) kept the previous tab's glyph: the queue ->
            // library switch residue. `terminal.clear()` blanks the physical
            // screen and the diff baseline together, so blank cells are blank
            // on both sides, and the synchronized update hides the blank
            // interval from the user (round 71.2's goal). Terminals without
            // DEC 2026 ignore the bracket and get the pre-71.2 clear +
            // repaint. The cava pane still erases its own band (`cava.clear`)
            // before this, and a config/tabs change keeps its own physical
            // clear below, where the whole layout can change.
            //
            // The guard stays alive until after the terminal-side overlays
            // below: clear, frame and images have to reach the glass in one
            // step, otherwise the user sees the blank, image-less state this
            // bracket exists to hide.
            let mut synchronized_update = None;
            let cava_refresh = ui.take_cava_refresh();
            let album_art_refresh = ui.take_album_art_refresh();
            if cava_refresh || album_art_refresh {
                log::debug!(cava_refresh, album_art_refresh; "Full repaint (cava-row drop / album-art erase repair) in a synchronized update");
                synchronized_update = SynchronizedUpdate::begin()
                    .inspect_err(|err| log::error!(err:?; "Failed to begin the synchronized update"))
                    .ok();
                if let Err(err) = terminal.clear() {
                    log::error!(error:? = err; "Failed to clear terminal after hiding cava");
                }
                resize_render_passes = 2;
                // Round 58: the clear deleted every kitty overlay. The
                // Jellyfin poster only re-draws when its area changes, so it
                // must be told to re-place on the next frame — targeted at
                // the poster only (a global Displayed dispatch caused an
                // album-art re-show/hide feedback loop, reverted). It runs
                // inside the bracket, so the re-placed poster and the frame
                // drawn over it land in the same terminal update.
                if let Err(err) = ui.refresh_overlays_after_clear(&ctx) {
                    log::error!(error:? = err; "Failed to refresh the Jellyfin poster after a full repaint");
                }
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
            // Round 73.1: the frame, its clear and its images are on the wire
            // — end the repair's synchronized update (a dropped `None` when
            // the repair did not fire) so the terminal shows the finished
            // frame. Closed before cava can be started: the bar thread writes
            // its own synchronized update and must not nest in this one.
            drop(synchronized_update);
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

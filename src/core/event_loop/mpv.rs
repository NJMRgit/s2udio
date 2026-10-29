use std::time::{Duration, Instant};

use crossbeam::channel::Sender;

use crate::{
    core::mpv::PendingResume,
    ctx::Ctx,
    jellyfin::JellyfinResult,
    mpd::{
        commands::{State, volume::Bound as _},
        mpd_client::MpdClient,
    },
    shared::events::{AppEvent, ClientRequest, WorkRequest},
    ui::Ui,
};

use super::helpers;

pub(super) struct MpvState {
    poll_guard: Option<crate::core::scheduler::TaskGuard<(Sender<AppEvent>, Sender<ClientRequest>)>>,
    last_report: Instant,
    last_paused: bool,
    prev_paused: Option<bool>,
    stale_ticks: u8,
    session_since: Option<Instant>,
    /// Whether `POST /Sessions/Playing` was already sent for the Jellyfin
    /// item playing now: the server needs a session start to record the
    /// playback, and it is sent once per item from the first progress tick
    /// (a session this instance reattached to already has one from the
    /// launcher/tracker).
    start_reported: bool,
    /// The Jellyfin item the poll is reporting against. A playlist advance
    /// (or a switch to another video) changes it, and the item left behind
    /// is finalized at the last position seen for it — otherwise a season
    /// played through mpv only ever reported the last episode, and a
    /// finished one was never marked watched.
    jellyfin_item: Option<String>,
    /// `PlaySessionId` of the playback of `jellyfin_item` (one per item, so
    /// the server can tell one playback from the next).
    play_session_id: String,
    /// The last position/duration read for `jellyfin_item` while it played:
    /// the final position to report when the item ends.
    item_last_pos: f64,
    item_duration: f64,
}

impl MpvState {
    pub(super) fn new() -> Self {
        Self {
            poll_guard: None,
            last_report: Instant::now() - Duration::from_secs(30),
            last_paused: false,
            prev_paused: None,
            stale_ticks: 0,
            session_since: None,
            start_reported: false,
            jellyfin_item: None,
            play_session_id: String::new(),
            item_last_pos: 0.0,
            item_duration: 0.0,
        }
    }
}

pub(super) enum PollResult {
    Continue,
    ContinueRender,
    Render,
}

pub(super) fn init(ctx: &mut Ctx, ui: &mut Ui, state: &mut MpvState) {
    // A previous s2udio instance may have left mpv playing (mpv survives the
    // app's exit; the standalone tracker daemon keeps the MPRIS state + the
    // Jellyfin tracking alive while the app is closed). Reattach so the
    // controls, seekbar, album art and MPRIS resume working in the TUI.
    if crate::core::mpv::detect_mpv_session(ctx) {
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
        state.last_report = Instant::now();
        // The reattached video's session start was already reported (by the
        // tracker daemon); don't send a second one.
        state.start_reported = true;
        // Poll mpv at 100ms: during playback frames render at this rate,
        // which keeps the controls-bar title carousel, the progress bar and
        // the info-box marquee smooth (a 500ms poll stepped the carousel
        // ~3.75 columns per frame).
        state.poll_guard = Some(ctx.scheduler.repeated(Duration::from_millis(100), move |(tx, _)| {
            let _ = tx.send(AppEvent::MpvPoll);
            Ok(())
        }));
        // A video on the Queue tab pauses MPD, so the cava visualizer goes
        // flat: hide it while the video plays (same as MpvSessionStarted).
        if ctx.cava_hidden_on(ctx.active_tab.as_str())
            && let Err(err) = ui.hide_cava(ctx)
        {
            log::error!(error:? = err; "Failed to hide cava for the reattached video");
        }
        // Make sure the tracker daemon covers this session again: when this
        // s2udio closes, the MPRIS state + Jellyfin tracking must survive.
        // (A session started by an older build may have no tracker; the
        // pid lock makes a duplicate spawn exit immediately.)
        if let Err(err) = {
            let mut tracker = std::process::Command::new("s2u-helper");
            tracker.arg("tracker");
            // Hand over the socket this instance reattached to: on a host
            // with mpvSockets.lua (SVP4's bundled mpv installs it) the
            // fixed /tmp/mpvsocket is dead and only the per-instance path
            // is live.
            if let Some(socket) = crate::core::mpv::mpv_socket() {
                tracker.env("S2U_MPV_SOCKET", socket);
            }
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
        if let Err(err) = ui.refresh_album_art(ctx) {
            log::error!(error:? = err; "Failed to refresh album art for the reattached video");
        }
        // Jellyfin item: fetch the metadata + chapters + art the TUI shows.
        // The saved resume position is *not* re-applied: the video is
        // already playing (the tracker applied it at launch).
        if let Some(item_id) = ctx.mpv.item_id.clone() {
            let _ = ctx
                .work_sender
                .send(WorkRequest::FetchJellyfinMpris { item_id: item_id.clone() });
            let _ = ctx
                .work_sender
                .send(WorkRequest::FetchJellyfinChapters { item_id: item_id.clone() });
            let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinVideoArt { item_id });
        }
        // The Queue tab follows the playing video (Chapters / Video list).
        if let Err(err) = ui.follow_video_session(ctx) {
            log::error!(error:? = err; "Failed to follow the reattached video in the queue tab");
        }
    }
}

pub(super) fn session_started(ctx: &mut Ctx, ui: &mut Ui, state: &mut MpvState, url: String) {
    // Switch the now-playing UI to the mpv video session.
    ctx.mpv.active = true;
    // A video on the Queue tab pauses MPD, so the cava
    // visualizer goes flat: hide it while the video plays.
    if ctx.cava_hidden_on(ctx.active_tab.as_str())
        && let Err(err) = ui.hide_cava(ctx)
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
        playlist.iter().find(|e| e.url == url).map(|e| e.lookup_url().to_owned())
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
    ctx.mpv.resume_seek.set(None);
    *ctx.mpv.resume_prompted_for.borrow_mut() = None;
    *ctx.mpv.resume_fetch_item.borrow_mut() = None;
    *ctx.mpv.pending_loadfile.borrow_mut() = None;
    // The resume question was asked *before* mpv started (so a slow answer
    // cannot leave mpv owning the window): apply the answer to this session.
    apply_pending_resume(ctx, &url);
    state.stale_ticks = 0;
    // A new mpv session is a new Jellyfin playback: the per-item
    // bookkeeping starts clean (the previous session's item was finalized
    // by `session_ended`).
    state.jellyfin_item = None;
    state.play_session_id = crate::jellyfin::new_play_session_id();
    state.item_last_pos = 0.0;
    state.item_duration = 0.0;
    // This is a fresh session: its Jellyfin session start has not been
    // reported yet.
    state.start_reported = false;
    // Don't report progress for the first 10 seconds so a
    // saved resume position is applied before the first
    // progress update (which would otherwise overwrite it).
    state.last_report = Instant::now();
    state.last_paused = false;
    state.prev_paused = None;
    if state.poll_guard.is_none() {
        // 100ms so playback frames (and with them the title
        // carousel / progress bar) stay smooth.
        state.poll_guard = Some(ctx.scheduler.repeated(
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
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinMpris { item_id: item_id.clone() });
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinResume { item_id: item_id.clone() });
        *ctx.mpv.resume_fetch_item.borrow_mut() = Some(item_id.clone());
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinChapters { item_id: item_id.clone() });
        let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinVideoArt { item_id });
    } else if let Some(key) = entry_key_for_url(ctx, &url) {
        // A local file, torrent stream or resolved link: there is no server
        // to ask, so the local store is the only resume source.
        prompt_local_resume(ctx, &key);
    }
    // The album art box belongs to the video now: refresh
    // it so the Queue tab shows the video's thumbnail
    // (Jellyfin primary image / resolved YouTube thumbnail)
    // instead of the stale audio art. The fetch requests go
    // out from the pane's before_show (a no-op on tabs
    // without the album art pane).
    if let Err(err) = ui.refresh_album_art(ctx) {
        log::error!(error:? = err; "Failed to refresh album art for the video session");
    }
    // The Queue tab follows the playing video: its Chapters
    // list when the video has markers (known synchronously
    // for resolved YouTube streams; Jellyfin's arrive with
    // the chapters fetch below), else the mpv playlist.
    if let Err(err) = ui.follow_video_session(ctx) {
        log::error!(error:? = err; "Failed to follow the video session in the queue tab");
    }
    // The mpv session state file feeds the s2udio-mpris
    // bridge (spawned by the tracker), which serves the
    // video through its own MPRIS player.
}

/// Run one Jellyfin playback report on a throwaway thread (the event loop is
/// never allowed to block on HTTP). A failure is logged, not shown: a
/// server hiccup must not interrupt playback.
fn spawn_jellyfin_report(
    ctx: &Ctx,
    report: impl FnOnce(&crate::jellyfin::Jellyfin) -> anyhow::Result<()> + Send + 'static,
) {
    let config_file = ctx.config.jellyfin.config_file.clone();
    std::thread::spawn(move || {
        let sidecar = crate::config::jellyfin::jellyfin_sidecar_path();
        if let Some(jf) = crate::jellyfin::Jellyfin::load(&config_file, Some(&sidecar))
            && let Err(err) = report(&jf)
        {
            log::debug!(error:? = err; "Jellyfin playback report failed");
        }
    });
}

/// Finalize one Jellyfin item's playback: report the stop at the last known
/// position (never below the server's resume threshold, which would wipe
/// the saved position instead of saving it).
fn report_jellyfin_stop(
    ctx: &Ctx,
    item_id: &str,
    position: f64,
    duration: f64,
    play_session_id: &str,
) {
    if !crate::jellyfin::reportable_position(position, duration) {
        log::debug!(
            item_id:? = item_id, position = position, duration = duration;
            "Not reporting a Jellyfin stop below the resume threshold"
        );
        return;
    }
    let item_id = item_id.to_owned();
    let play_session_id = play_session_id.to_owned();
    spawn_jellyfin_report(ctx, move |jf| {
        jf.report_playing_stopped(&item_id, position, &play_session_id)
    });
}

pub(super) fn session_ended(ctx: &mut Ctx, ui: &mut Ui, state: &mut MpvState) {
    // Clear the flag the MPD-start pause guard checks (the
    // launcher thread clears it for its own session, but a
    // reattached one has no launcher thread — without this a
    // dead session kept flagging "mpv running" forever).
    crate::core::mpv::MPV_RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
    // Save the final position for resume (background thread). The position
    // is the last one the poll saw for this item — `ctx.mpv.position` is a
    // fresh 0 on a session that never got past its start, and a 0-tick stop
    // would wipe the resume position the server had.
    if let Some(item_id) = ctx.mpv.item_id.clone() {
        let position = state.item_last_pos.max(ctx.mpv.position).max(0.0);
        report_jellyfin_stop(
            ctx,
            &item_id,
            position,
            ctx.mpv.duration.max(state.item_duration),
            &state.play_session_id,
        );
    }
    state.jellyfin_item = None;
    state.start_reported = false;
    state.item_last_pos = 0.0;
    state.item_duration = 0.0;
    // A pre-launch answer that never reached a session start (its launch
    // failed, or mpv died on the way up) must not seek the next video.
    *ctx.mpv.pending_resume.borrow_mut() = None;
    // Remember the final position locally for every video (Jellyfin keeps
    // its own server-side copy; this covers local files, torrents and
    // resolved links, and is the fallback for a Jellyfin item the server
    // has no position for). A video that ran to its end is cleared, so it
    // does not resume.
    if let Some(key) = current_entry_key(ctx) {
        let position = ctx.mpv.position;
        let duration = ctx.mpv.duration;
        if duration > 0.0 && position >= duration - 5.0 {
            crate::core::video_resume::store(ctx.config.cache_dir.as_deref(), &key, 0.0);
        } else if position > 1.0 {
            crate::core::video_resume::store(ctx.config.cache_dir.as_deref(), &key, position);
        }
    }
    // Drop the mpv state file so mpDris2 falls back to MPD.
    crate::ui::modals::paste::delete_mpv_mpris_state(ctx);
    ctx.mpv = crate::core::mpv::MpvSession::default();
    // R2.5: the daemon may move this job's completed files
    // now ("streams over").
    crate::ui::modals::paste::untrack_daemon_streams(ctx);
    // R2: plain-streamed torrents stop downloading/seeding
    // when the stream ends (forget keeps the partials; a
    // re-stream resumes from them).
    helpers::stop_finished_plain_streams(ctx, None);
    // The album art overlay belongs to the audio source
    // again: restore the current song's art (or the default).
    if let Err(err) = ui.refresh_album_art(ctx) {
        log::error!(error:? = err; "Failed to restore album art");
    }
    // The video ended: bring the cava visualizer back on the
    // tabs that hide it during playback.
    if !ctx.cava_hidden_on(ctx.active_tab.as_str())
        && let Err(err) = ui.show_cava(ctx)
    {
        log::error!(error:? = err; "Failed to restore cava");
    }
    state.poll_guard = None;
}

pub(super) fn poll(ctx: &mut Ctx, ui: &mut Ui, state: &mut MpvState) -> PollResult {
    if !ctx.mpv.active {
        state.session_since = None;
        state.stale_ticks = 0;
        return PollResult::Continue;
    }
    let session_age = state
        .session_since
        .get_or_insert_with(Instant::now)
        .elapsed();
    // Keep the MPRIS bridge in sync (title/art/position);
    // written before the socket is even up so the daemon
    // never sees a missing state file.
    crate::ui::modals::paste::write_mpv_mpris_state(ctx);
    let Some(socket) = crate::core::mpv::mpv_socket() else {
        // No socket at all: mpv exited. For a session this
        // instance reattached to there is no launcher thread
        // to send MpvSessionEnded, so count failures and
        // tear the session down ourselves after a few.
        if session_age >= MPV_START_GRACE {
            state.stale_ticks += 1;
            if state.stale_ticks >= 5 {
                let _ = ctx.app_event_sender.send(AppEvent::MpvSessionEnded);
            }
        }
        return PollResult::ContinueRender;
    };
    ctx.mpv.socket = Some(socket.clone());
    let Some((position, paused, duration, volume, playlist_pos, playlist_count)) =
        crate::core::mpv::read_mpv_state(&socket)
    else {
        // Socket file exists but mpv is unreachable: same as
        // above (the stale file can outlive mpv).
        if session_age >= MPV_START_GRACE {
            state.stale_ticks += 1;
            if state.stale_ticks >= 5 {
                let _ = ctx.app_event_sender.send(AppEvent::MpvSessionEnded);
            }
        }
        return PollResult::ContinueRender;
    };
    state.stale_ticks = 0;
    // The mpv video / MPD audio UI-source switch (MPD
    // playback started and the mutual exclusion paused
    // the video, or the video resumed): the album art
    // box follows whichever source is active, so refresh
    // it when the source flips (nothing else repaints it
    // — a SongChanged may never fire for a resumed
    // track).
    let source_before = ctx.mpv.active
        && (!ctx.mpv.paused || ctx.status.state != crate::mpd::commands::State::Play);
    ctx.mpv.position = position;
    ctx.mpv.paused = paused;
    ctx.mpv.duration = duration;
    ctx.mpv.volume = volume;
    let source_after = ctx.mpv.active
        && (!paused || ctx.status.state != crate::mpd::commands::State::Play);
    if source_before != source_after
        && let Err(err) = ui.refresh_album_art(ctx)
    {
        log::error!(error:? = err; "Failed to refresh album art after the playback source switched");
    }
    // The video resumed while MPD plays: pause the
    // music (the other side of the mutual exclusion;
    // MPD-start -> pause-mpv is handled on the status
    // update). Only on the paused->playing transition:
    // the user unpaused the video, so the music gives
    // way.
    if let Some(prev) = state.prev_paused
        && prev
        && !paused
        && ctx.status.state == State::Play
    {
        log::debug!("mpv resumed while MPD plays; pausing MPD");
        let _ = ctx.client_request_sender.send(ClientRequest::Command(
            crate::MpdCommand {
                callback: Box::new(|client| {
                    client.pause()?;
                    Ok(())
                }),
            },
        ));
    }
    state.prev_paused = Some(paused);
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
            // The previous entry's resume state does not apply to this one.
            ctx.mpv.resume_seek.set(None);
            *ctx.mpv.resume_prompted_for.borrow_mut() = None;
            *ctx.mpv.resume_fetch_item.borrow_mut() = None;
            *ctx.mpv.pending_resume.borrow_mut() = None;
            // Another playlist entry is another Jellyfin playback: its
            // session start has not been reported yet either.
            state.start_reported = false;
            ctx.mpv.playlist_pos.set(playlist_pos);
            ctx.mpv.title = entry.title.clone();
            ctx.mpv.item_id = crate::jellyfin::item_id_from_url(&entry.url);
            ctx.mpv.item = None;
            // A YouTube-style entry: the resolved info
            // supplies the real title/channel, and the
            // MPRIS poster is re-fetched for the new
            // entry.
            if let Some(info) = ctx.yt_info.borrow().get(&entry.lookup_url().to_owned()) {
                ctx.mpv.title = info.title.clone();
                ctx.mpv.artist = info.channel.clone().unwrap_or_default();
            }
            ctx.mpv.art_path = None;
            // Don't serve the previous entry's poster
            // until the new art is fetched.
            crate::ui::modals::paste::clear_mpv_mpris_art(ctx);
            // The new entry is another Jellyfin item: refresh
            // its metadata and chapters.
            if let Some(item_id) = ctx.mpv.item_id.clone() {
                let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinMpris {
                    item_id: item_id.clone(),
                });
                // The new entry's saved position comes from the server (the
                // local store is the fallback when it has none).
                let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinResume {
                    item_id: item_id.clone(),
                });
                *ctx.mpv.resume_fetch_item.borrow_mut() = Some(item_id.clone());
                let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinChapters {
                    item_id: item_id.clone(),
                });
                let _ = ctx.work_sender.send(WorkRequest::FetchJellyfinVideoArt { item_id });
            } else if let Some(key) = crate::core::video_resume::entry_key(&entry) {
                // No server to ask: the local store is the only resume
                // source for the entry mpv advanced to.
                prompt_local_resume(ctx, &key);
            }
            // A YouTube-style entry: refresh the album art
            // thumbnail for the new entry.
            if let Some(info) = crate::ui::modals::paste::mpv_yt_info(ctx)
                && let Some(thumb) = info.thumbnail
            {
                let _ = ctx
                    .work_sender
                    .send(WorkRequest::FetchYtThumbnail { url: thumb });
            }
        }
    }
    // Jellyfin bookkeeping: when the item the poll reports against changes
    // (mpv advanced to the next episode, or the user switched to another
    // video), finalize the item that just ended at the last position seen
    // for it. Every episode of a season then gets its own stop report, and
    // a completed one is marked watched even if the 10 s progress tick never
    // happened to land near its end.
    //
    // This runs AFTER the advance block above on purpose: there
    // `ctx.mpv.item_id` already names the new entry while `item_last_pos`
    // still holds the position the previous item reached, which is exactly
    // the position its stop report needs.
    let current_jellyfin_item = ctx.mpv.item_id.clone();
    if state.jellyfin_item != current_jellyfin_item {
        if let Some(previous) = state.jellyfin_item.clone() {
            report_jellyfin_stop(
                ctx,
                &previous,
                state.item_last_pos,
                state.item_duration,
                &state.play_session_id,
            );
            // Another item is another playback: its session start has not
            // been reported yet.
            state.start_reported = false;
        }
        // None → Some is the first poll of a session (or a reattach): the
        // launcher/tracker already decided `start_reported` for it.
        state.play_session_id = crate::jellyfin::new_play_session_id();
        state.jellyfin_item = current_jellyfin_item;
    }
    state.item_last_pos = position;
    state.item_duration = duration;
    // Apply an armed resume seek now that the socket is reachable; the
    // helper retries until mpv has finished opening the entry (a seek sent
    // while it is still loading is dropped).
    crate::core::mpv::apply_pending_resume_seek(ctx, &socket, position, duration);
    // Round 74 (74-3): a pasted link's start offset for the
    // entry mpv is on (a live switch cannot take `--start=`).
    crate::core::mpv::apply_entry_start_seek(
        ctx,
        &socket,
        position,
        duration,
        playlist_pos,
    );
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
            if let Some(info) = crate::ui::modals::paste::mpv_yt_info(ctx) {
                if !info.title.is_empty() {
                    ctx.mpv.title = info.title.clone();
                }
                if !info.channel.as_deref().unwrap_or("").is_empty() {
                    ctx.mpv.artist = info.channel.clone().unwrap_or_default();
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
        && let Some(info) = crate::ui::modals::paste::mpv_yt_info(ctx)
        && let Some(thumb) = info.thumbnail
    {
        let cache_dir = ctx.config.cache_dir.clone();
        let _ = ctx.work_sender.send(WorkRequest::SaveMpvMprisArt {
            url: thumb,
            cache_dir,
            crop_letterbox: ctx.config.album_art.crop_letterbox,
        });
        ctx.mpv.art_path = Some(crate::ui::modals::paste::mpv_mpris_art_path(
            ctx.config.cache_dir.as_deref(),
        ));
    }
    // Report progress to Jellyfin and remember the position locally: on
    // pause changes and otherwise at most every 10 seconds.
    let changed = ctx.mpv.paused != state.last_paused;
    if changed || state.last_report.elapsed() >= Duration::from_secs(10) {
        state.last_report = Instant::now();
        state.last_paused = ctx.mpv.paused;
        if let Some(item_id) = ctx.mpv.item_id.clone() {
            let session = state.play_session_id.clone();
            // The server needs a session start before progress makes sense
            // (it creates the session and records the playback); sent once
            // per item.
            if !state.start_reported {
                state.start_reported = true;
                let position = ctx.mpv.position;
                let start_item = item_id.clone();
                let start_session = session.clone();
                spawn_jellyfin_report(ctx, move |jf| {
                    jf.report_playing_start(&start_item, position, &start_session)
                });
            }
            // A report below the server's resume threshold is stored as
            // position 0 — it would ERASE the saved resume position, so an
            // early tick reports nothing at all.
            let position = ctx.mpv.position;
            let duration = ctx.mpv.duration;
            if crate::jellyfin::reportable_position(position, duration) {
                let paused = ctx.mpv.paused;
                let session = state.play_session_id.clone();
                spawn_jellyfin_report(ctx, move |jf| {
                    jf.report_playing_progress(&item_id, position, paused, &session)
                });
            }
        }
        // Remember the position of every video (Jellyfin included: the
        // local store is the fallback for an item the server has none for,
        // and the prompt for every other source). A finished video is
        // cleared so it does not resume.
        if let Some(key) = current_entry_key(ctx) {
            let position = ctx.mpv.position;
            let duration = ctx.mpv.duration;
            if duration > 0.0 && position >= duration - 5.0 {
                crate::core::video_resume::store(ctx.config.cache_dir.as_deref(), &key, 0.0);
            } else if position > 1.0 {
                crate::core::video_resume::store(
                    ctx.config.cache_dir.as_deref(),
                    &key,
                    position,
                );
            }
        }
    }
    PollResult::Render
}

// Round 91: when the current mpv session first went active, and how long
// a missing/unreachable socket is tolerated after that. SVP's mpv takes
// seconds to create its IPC socket, while the 100 ms poll reached the
// 5-tick limit in 500 ms - a healthy session was torn down (and with it
// the MPRIS state file) while the video kept playing.
const MPV_START_GRACE: Duration = Duration::from_secs(10);

pub(super) fn forward_volume(ctx: &mut Ctx, volume: crate::mpd::commands::volume::Volume) {
    let new_volume = *volume.value();
    ctx.status.volume = volume;
    // MPRIS clients (the KDE media widget / media
    // keys, via mpDris2) adjust MPD's volume. While a
    // video plays in mpv, forward the change to the
    // mpv session (the actual audio source) and
    // mirror it into ctx.mpv.volume so the volume bar
    // updates immediately instead of showing the
    // stale mpv value the poll read earlier.
    if crate::core::mpv::mpv_is_ui_source(ctx)
        && let Some(socket) = ctx.mpv.socket.clone()
    {
        crate::core::mpv::mpv_exchange_volume(&socket, f64::from(new_volume));
        ctx.mpv.volume = Some(new_volume as u8);
    }
}

pub(super) fn pause_for_mpd_start() {
    log::debug!("MPD playback started; pausing mpv");
    crate::core::mpv::pause_mpv();
}

pub(super) fn handle_jellyfin_fetched(
    id: &'static str,
    data: JellyfinResult,
    ctx: &mut Ctx,
    ui: &mut Ui,
    render_wanted: &mut bool,
) -> Result<(), (&'static str, JellyfinResult)> {
    match (id, data) {
        (crate::ui::panes::jellyfin::JF_ITEM, JellyfinResult::Item(item)) => {
            if ctx.mpv.active && ctx.mpv.item_id.as_deref() == Some(item.id.as_str()) {
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
                crate::core::mpv::update_jellyfin_entry_title(ctx, &item.id, &item.name);
            }
            // The Jellyfin tab's info box shows the full
            // item metadata (identical to the queue
            // tab's): route it to the pane as well.
            let data = crate::MpdQueryResult::Any(Box::new(
                crate::jellyfin::JellyfinResult::Item(item),
            ));
            if let Err(err) = ui.on_command_finished(
                crate::ui::panes::jellyfin::JF_ITEM,
                Some(crate::config::tabs::PaneType::Jellyfin {
                    tree: crate::config::tabs::TreeBrowserArgs::default(),
                }),
                data,
                ctx,
            ) {
                log::error!(error:? = err; "UI failed to handle jellyfin item result");
            }
        }
        (
            crate::ui::panes::jellyfin::JF_MPRIS,
            JellyfinResult::Mpris { item, image },
        ) => {
            // mpv video session: title/artist for the
            // media controls + the poster written where
            // the MPRIS bridge can serve it.
            if ctx.mpv.active && ctx.mpv.item_id.as_deref() == Some(item.id.as_str()) {
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
                crate::core::mpv::update_jellyfin_entry_title(ctx, &item.id, &item.name);
                if !image.is_empty() {
                    let path =
                        crate::ui::modals::paste::mpv_mpris_art_path(ctx.config.cache_dir.as_deref());
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
                let still_current = ctx.find_current_song_in_queue().is_some_and(|(_, song)| {
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
                        let album = item.album.clone().unwrap_or_else(|| name.clone());
                        ctx.command(move |client| {
                            let _ = client.add_tag_id(song_id, "title", &name);
                            let _ = client.add_tag_id(song_id, "artist", &artist);
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
        }
        (
            crate::ui::panes::jellyfin::JF_RESUME,
            JellyfinResult::ResumePosition { seconds, .. },
        ) => {
            // Only act on the response for the item playing now: a fetch
            // that lost a race with a playlist advance must not prompt for
            // the wrong entry.
            let current = ctx.mpv.item_id.clone();
            let pending = ctx.mpv.resume_fetch_item.borrow().clone();
            if ctx.mpv.active && pending == current {
                *ctx.mpv.resume_fetch_item.borrow_mut() = None;
                if let Some(item_id) = current {
                    if should_prompt_resume(seconds) {
                        let title = ctx.mpv.title.clone();
                        prompt_video_resume(ctx, &title, seconds, format!("jf:{item_id}"));
                    } else if let Some(key) = current_entry_key(ctx) {
                        // The server has no position: fall back to the
                        // local store before letting the video start over.
                        prompt_local_resume(ctx, &key);
                    }
                }
            }
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
                if let Some(idx) = start_index.checked_rem(entries.len()).filter(|i| *i > 0) {
                    entries.rotate_left(idx);
                }
                if let Some(socket) = ctx.mpv.socket.clone()
                    && !entries.is_empty()
                {
                    // Round 68: rebuild mpv's whole
                    // playlist from a titled .m3u so
                    // every episode keeps its correct
                    // name. URL-only loadfiles cannot
                    // carry titles over IPC on mpv
                    // v0.41 (appends stay untitled and a
                    // replace inherits the stale title
                    // of the old current entry — that
                    // left the OSD on the wrong episode
                    // name after a mid-play season
                    // switch).
                    let m3u = crate::core::mpv::write_mpv_m3u(&entries);
                    crate::core::mpv::mpv_load_playlist(&socket, &m3u);
                    // The replace restarts the current
                    // file from 0; re-apply the last
                    // known position (same socket, same
                    // thread, so the seek lands after
                    // the new file is loaded) so the
                    // clicked episode continues where
                    // it was.
                    if ctx.mpv.position > 5.0 {
                        crate::core::mpv::mpv_seek(&socket, ctx.mpv.position);
                    }
                }
                *ctx.mpv.playlist.borrow_mut() = entries;
                ctx.mpv.playlist_pos.set(Some(0));
            } else {
                run_mpv_playlist(ctx, entries, Some(start_index));
            }
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
                    ctx,
                ) {
                    log::error!(error:? = err; "UI failed to handle video art result");
                }
            } else {
                log::debug!(data:?; "Video art fetch failed");
            }
        }
        (crate::ui::panes::jellyfin::JF_CHAPTERS, data) => {
            // The chapter markers of the video playing in
            // mpv (or the current song): route them to
            // the Jellyfin pane, then let the Queue tab
            // follow the video into its Chapters list.
            let data = crate::MpdQueryResult::Any(Box::new(data));
            if let Err(err) = ui.on_command_finished(
                crate::ui::panes::jellyfin::JF_CHAPTERS,
                Some(crate::config::tabs::PaneType::Jellyfin {
                    tree: crate::config::tabs::TreeBrowserArgs::default(),
                }),
                data,
                ctx,
            ) {
                log::error!(error:? = err; "UI failed to handle chapters result");
            }
            if let Err(err) = ui.follow_video_session(ctx) {
                log::error!(error:? = err; "Failed to follow the video session in the queue tab");
            }
        }
        (id, data) => return Err((id, data)),
    }
    *render_wanted = true;
    Ok(())
}

/// Whether a saved position is far enough into a video to be worth offering.
/// A Continue / Start over prompt for the first few seconds would be noise
/// (and Jellyfin's own threshold is the same 10 s).
pub(crate) fn should_prompt_resume(seconds: f64) -> bool {
    seconds > 10.0
}

/// Apply a Continue / Start over answer that was collected **before** mpv
/// was spawned (see [`crate::core::mpv::PendingResume`]).
///
/// `Continue` arms the resume seek the poll applies once the entry is open;
/// `Start over` leaves the seek unset. Either way the entry counts as
/// answered, so the server-position fetch that runs for every Jellyfin item
/// does not ask the same question again behind the now-visible video.
pub(crate) fn apply_pending_resume(ctx: &Ctx, url: &str) {
    let Some((item_id, choice)) = ctx.mpv.pending_resume.borrow_mut().take() else {
        return;
    };
    // The answer belongs to the item it was asked about: a session that
    // starts in between must not inherit the position.
    if ctx.mpv.item_id.as_deref() != Some(item_id.as_str()) {
        log::debug!(
            answered_for:? = item_id.as_str(), playing:? = ctx.mpv.item_id.as_deref();
            "Dropped a resume answer asked for another item"
        );
        return;
    }
    if let PendingResume::Continue(seconds) = choice {
        ctx.mpv.resume_seek.set(Some((seconds, 0, None)));
    }
    let key = ctx
        .mpv
        .item_id
        .as_ref()
        .map(|id| format!("jf:{id}"))
        .or_else(|| entry_key_for_url(ctx, url));
    if key.is_some() {
        *ctx.mpv.resume_prompted_for.borrow_mut() = key;
    }
}

/// The resume-store key of the playlist entry that was started at `url`
/// (the session-start lookup: the recorded playlist may not have a position
/// for it yet).
fn entry_key_for_url(ctx: &Ctx, url: &str) -> Option<String> {
    ctx.mpv
        .playlist
        .borrow()
        .iter()
        .find(|entry| entry.url == url)
        .and_then(crate::core::video_resume::entry_key)
}

/// The resume-store key of the entry mpv is on now, falling back to the
/// Jellyfin item id (a Jellyfin stream plays from a session that may have no
/// recorded playlist entry, e.g. right after a reattach).
fn current_entry_key(ctx: &Ctx) -> Option<String> {
    if let Some(pos) = ctx.mpv.playlist_pos.get()
        && let Some(entry) = ctx.mpv.playlist.borrow().get(pos)
        && let Some(key) = crate::core::video_resume::entry_key(entry)
    {
        return Some(key);
    }
    ctx.mpv
        .item_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| format!("jf:{id}"))
}

/// Offer the locally remembered position of `key`, when there is one worth
/// offering.
fn prompt_local_resume(ctx: &mut Ctx, key: &str) {
    let Some(seconds) = crate::core::video_resume::load(ctx.config.cache_dir.as_deref(), key)
    else {
        return;
    };
    let title = ctx.mpv.title.clone();
    prompt_video_resume(ctx, &title, seconds, key.to_owned());
}

/// Ask whether to resume `title` from `seconds` or start it over.
///
/// One prompt per entry (`resume_prompted_for`): "Continue" arms the resume
/// seek the poll applies once mpv has finished opening the entry; "Start
/// over" clears the stored position so the entry plays from the beginning
/// and is not offered again.
fn prompt_video_resume(ctx: &mut Ctx, title: &str, seconds: f64, key: String) {
    if !should_prompt_resume(seconds) {
        return;
    }
    if ctx.mpv.resume_prompted_for.borrow().as_deref() == Some(key.as_str()) {
        return;
    }
    *ctx.mpv.resume_prompted_for.borrow_mut() = Some(key.clone());
    use crate::ui::modals::confirm_modal::{Action, ConfirmModal};
    let formatted = ctx.config.duration_format.format(seconds as u64);
    let message = vec![
        format!("Resume {title}?"),
        format!("Continue from {formatted}."),
    ];
    crate::shared::macros::modal!(
        ctx,
        ConfirmModal::builder()
            .ctx(ctx)
            .message(message)
            .action(Action::CustomButtons {
                buttons: vec![
                    (
                        "Continue",
                        Box::new(move |ctx| {
                            ctx.mpv.resume_seek.set(Some((seconds, 0, None)));
                            Ok(())
                        }),
                    ),
                    (
                        "Start over",
                        Box::new(move |ctx| {
                            ctx.mpv.resume_seek.set(None);
                            crate::core::video_resume::store(
                                ctx.config.cache_dir.as_deref(),
                                &key,
                                0.0,
                            );
                            Ok(())
                        }),
                    ),
                ],
            })
            .size((56, 7))
            .build()
    );
}

#[cfg(test)]
mod tests {
    use super::{apply_pending_resume, should_prompt_resume};
    use crate::core::mpv::PendingResume;

    fn rig() -> crate::ctx::Ctx {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        crate::tests::fixtures::ctx((app_tx, app_rx), (work_tx, work_rx), (client_tx, client_rx))
    }

    /// The answer collected before the launch rides into the session start:
    /// "Continue" arms the seek and marks the entry answered, "Start over"
    /// leaves the seek alone but still counts as answered (so the server
    /// fetch does not ask again), and the answer is consumed once.
    #[test]
    fn a_pre_launch_answer_arms_the_seek_once() {
        let mut ctx = rig();
        ctx.mpv.item_id = Some("ep1".to_owned());
        *ctx.mpv.pending_resume.borrow_mut() =
            Some(("ep1".to_owned(), PendingResume::Continue(1200.0)));
        apply_pending_resume(&ctx, "http://jf:8096/Videos/ep1/stream");
        assert_eq!(
            ctx.mpv.resume_seek.get().map(|(secs, _, _)| secs),
            Some(1200.0),
            "Continue arms the resume seek"
        );
        assert_eq!(
            ctx.mpv.resume_prompted_for.borrow().as_deref(),
            Some("jf:ep1"),
            "the entry counts as answered"
        );
        assert!(ctx.mpv.pending_resume.borrow().is_none(), "the answer is consumed");

        *ctx.mpv.pending_resume.borrow_mut() =
            Some(("ep1".to_owned(), PendingResume::StartOver));
        ctx.mpv.resume_seek.set(None);
        *ctx.mpv.resume_prompted_for.borrow_mut() = None;
        apply_pending_resume(&ctx, "http://jf:8096/Videos/ep1/stream");
        assert!(ctx.mpv.resume_seek.get().is_none(), "Start over seeks nothing");
        assert_eq!(
            ctx.mpv.resume_prompted_for.borrow().as_deref(),
            Some("jf:ep1"),
            "and is not asked again behind the video"
        );
    }

    /// An answer asked about one item must never seek another: a session can
    /// begin between the question and the launch it belongs to.
    #[test]
    fn an_answer_for_another_item_is_dropped() {
        let mut ctx = rig();
        ctx.mpv.item_id = Some("ep2".to_owned());
        *ctx.mpv.pending_resume.borrow_mut() =
            Some(("ep1".to_owned(), PendingResume::Continue(1200.0)));
        apply_pending_resume(&ctx, "http://jf:8096/Videos/ep2/stream");
        assert!(
            ctx.mpv.resume_seek.get().is_none(),
            "another item's position is not applied"
        );
        assert!(
            ctx.mpv.resume_prompted_for.borrow().is_none(),
            "and it does not count as answered for this one"
        );
        assert!(ctx.mpv.pending_resume.borrow().is_none(), "the stale answer is consumed");
    }

    /// Nothing was asked (a fresh item, or a session started by something
    /// other than the play path): the session start must not invent a seek.
    #[test]
    fn no_answer_leaves_the_session_alone() {
        let mut ctx = rig();
        ctx.mpv.item_id = Some("ep2".to_owned());
        apply_pending_resume(&ctx, "http://jf:8096/Videos/ep2/stream");
        assert!(ctx.mpv.resume_seek.get().is_none());
        assert!(ctx.mpv.resume_prompted_for.borrow().is_none());
    }

    /// At/around the threshold: only a position past 10 s is worth a prompt
    /// (the boundary belongs to "start from the beginning").
    #[test]
    fn prompts_only_past_ten_seconds() {
        assert!(!should_prompt_resume(0.0));
        assert!(!should_prompt_resume(10.0));
        assert!(should_prompt_resume(10.001));
        assert!(should_prompt_resume(3600.0));
    }
}

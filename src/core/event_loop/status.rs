use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use crossbeam::channel::Sender;

use crate::{
    ctx::Ctx,
    mpd::{
        commands::{IdleEvent, ReplayGain, State},
        mpd_client::{MpdClient, SaveMode},
    },
    shared::{
        events::{AppEvent, ClientRequest},
        ext::error::ErrorExt,
        macros::{status_error, status_info, status_warn},
        mpd_client_ext::MpdClientExt,
        mpd_query::{
            EXTERNAL_COMMAND, GLOBAL_QUEUE_UPDATE, GLOBAL_STATUS_UPDATE, GLOBAL_STICKERS_UPDATE,
            GLOBAL_VOLUME_UPDATE, MpdQueryResult, run_status_update,
        },
    },
    ui::{Ui, UiEvent},
};

use super::{mpv, queue_update};

type StatusGuard = crate::core::scheduler::TaskGuard<(Sender<AppEvent>, Sender<ClientRequest>)>;

pub(super) struct StatusState {
    additional_evs: HashSet<IdleEvent>,
    connected: bool,
    last_reported_mpd_error: Option<String>,
    update_loop_guard: Option<StatusGuard>,
    update_db_loop_guard: Option<StatusGuard>,
    /// 2026-10-15: bounded MPD decode-error recovery. MPD reports one decode
    /// failure per unresolved row it skips, and each report used to start
    /// another `yt-dlp` resolve for the row it named. A run of unresolved rows
    /// therefore spawned an unbounded chain of resolves — the "resolve storm"
    /// the user saw as the selection hopping down the queue with spinners
    /// before playback finally settled (measured live: three resolves in 1.4 s
    /// and 5.1 s to playback). The window bounds how many recoveries may run
    /// back to back, so a pathological queue degrades to MPD's own skipping
    /// with one explained message instead of hammering yt-dlp.
    decode_recoveries: Vec<std::time::Instant>,
}

/// How many MPD decode-error recoveries may run inside
/// [`DECODE_RECOVERY_WINDOW`] before the app stops resolving and lets MPD skip.
const DECODE_RECOVERY_LIMIT: usize = 4;
/// The window the recovery budget is measured over. Roughly the time a run of
/// skips takes to walk a few unresolved rows, and long enough that an ordinary
/// one-off recovery (a stale URL mid-track) never accumulates.
const DECODE_RECOVERY_WINDOW: Duration = Duration::from_secs(10);

impl StatusState {
    pub(super) fn new() -> Self {
        Self {
            additional_evs: HashSet::new(),
            connected: true,
            last_reported_mpd_error: None,
            update_loop_guard: None,
            update_db_loop_guard: None,
            decode_recoveries: Vec::new(),
        }
    }

    /// Whether another decode-error recovery may start now, recording this one
    /// if so. A `false` answer means the budget is spent: the caller leaves the
    /// row to MPD and says so once, instead of starting yet another resolve.
    fn may_recover_decode_error(&mut self, now: std::time::Instant) -> bool {
        self.decode_recoveries
            .retain(|at| now.saturating_duration_since(*at) < DECODE_RECOVERY_WINDOW);
        if self.decode_recoveries.len() >= DECODE_RECOVERY_LIMIT {
            return false;
        }
        self.decode_recoveries.push(now);
        true
    }

    pub(super) fn start_update_loop(&mut self, ctx: &Ctx) {
        self.update_loop_guard = ctx
            .config
            .status_update_interval_ms
            .map(Duration::from_millis)
            .map(|interval| ctx.scheduler.repeated(interval, run_status_update));
    }

    fn stop_update_loop(&mut self) {
        self.update_loop_guard = None;
    }
}

pub(super) fn apply_replay_gain(ctx: &Ctx) {
    let Some(mode) = crate::config::state::AppStateFile::load().mpd_replay_gain else {
        return;
    };
    let Ok(mode) = mode.parse::<ReplayGain>() else {
        log::warn!(mode = mode.as_str(); "Stored replay gain mode is invalid; ignoring");
        return;
    };
    ctx.command(move |client| {
        client.replay_gain(mode)?;
        Ok(())
    });
}

pub(super) fn handle_idle_event(
    event: IdleEvent,
    ctx: &mut Ctx,
    ui: &mut Ui,
    state: &mut StatusState,
    render_wanted: &mut bool,
) {
    handle_idle_event_query(event, ctx, &mut state.additional_evs);
    for ev in state.additional_evs.drain().filter_map(|ev| UiEvent::try_from(ev).ok()) {
        if let Err(err) = ui.on_event(ev, ctx) {
            status_error!(error:? = err, event:?; "UI failed to handle idle event, event: '{:?}', error: '{}'", event, err.to_status());
        }
    }
    *render_wanted = true;
}

pub(super) fn handle_reconnected(ctx: &mut Ctx, ui: &mut Ui, state: &mut StatusState) {
    // Round 53: the persisted replay gain mode is partition-level
    // server state, lost on MPD restarts only — re-assert it on
    // every reconnect (MPD restart included).
    apply_replay_gain(ctx);
    for ev in [IdleEvent::Player, IdleEvent::Playlist, IdleEvent::Options] {
        handle_idle_event_query(ev, ctx, &mut state.additional_evs);
    }
    if let Err(err) = ui.on_event(UiEvent::Reconnected, ctx) {
        log::error!(error:? = err, event:? = AppEvent::Reconnected; "UI failed to handle resize event");
    }
    status_warn!("rmpc reconnected to MPD and will reinitialize");
    // Round 52: re-surface MPD's error state on reconnect (it
    // may have changed while disconnected; a fresh status
    // poll replaces it right after).
    if let Some(err) = &ctx.status.error {
        status_warn!("MPD: {err} — re-run setup.sh or check the MPD state dir permissions");
        state.last_reported_mpd_error = Some(err.clone());
    }
    state.connected = true;
}

pub(super) fn handle_lost_connection(ctx: &mut Ctx, state: &mut StatusState) {
    if ctx.status.state != State::Stop {
        state.stop_update_loop();
        ctx.status.state = State::Stop;
    }
    if state.connected {
        status_error!("rmpc lost connection to MPD and will try to reconnect");
    }
    state.connected = false;
}

pub(super) fn handle_mpd_command_finished(
    ctx: &mut Ctx,
    ui: &mut Ui,
    state: &mut StatusState,
    id: &'static str,
    target: Option<crate::config::tabs::PaneType>,
    data: MpdQueryResult,
    render_wanted: &mut bool,
) {
    match (id, target, data) {
        (GLOBAL_STICKERS_UPDATE, None, MpdQueryResult::SongStickers(stickers)) => {
            ctx.set_stickers(stickers);
            *render_wanted = true;
        }
        (GLOBAL_STATUS_UPDATE, None, MpdQueryResult::Status { data: status, source_event }) => {
            let current_song_id = ctx.find_current_song_in_queue().map(|(_, song)| song.id);
            let previous_state = ctx.status.state;
            let current_updating_db = ctx.status.updating_db;
            let current_playlist = ctx.status.lastloadedplaylist.take();
            let previous_status = std::mem::replace(&mut ctx.status, status);
            // Round 52 (GitHub issue #1 symptom 3): surface
            // MPD's `error:` field — a broken db / state dir
            // appears verbatim there — in the status bar. One
            // message per distinct error string (a per-poll
            // repeat stays quiet); a recovery line when the
            // error clears.
            if let Some(err) = &ctx.status.error {
                if state.last_reported_mpd_error.as_deref() != Some(err.as_str()) {
                    status_warn!(
                        "MPD: {err} — re-run setup.sh or check the MPD state dir permissions"
                    );
                    state.last_reported_mpd_error = Some(err.clone());
                }
            } else if state.last_reported_mpd_error.take().is_some() {
                status_info!("MPD: error cleared — MPD is ready");
            }
            // Round 91: MPD cannot open a queued stream
            // **link** (the `watch?v=ID#s2u-audio`
            // placeholder a queued web stream starts as): it
            // reports the failed decode in `error:` and leaves
            // NO current song, so the song-id hooks never see
            // that entry and the queue would stall silently.
            // Round 105: the error can also name a **resolved
            // stream whose signed URL expired** — the same
            // silence, except MPD has just played a few
            // seconds of the dead URL and skipped on. Recover
            // here: the entry the error names is resolved (a
            // tagged link) or re-resolved from its original
            // link (an expired stream) and replaced. The
            // generic MPD warning is suppressed for this case
            // (the row was never something MPD was meant to
            // open as-is).
            let error_link = ctx
                .status
                .error
                .as_deref()
                .and_then(|err| err.split('"').nth(1))
                .map(str::to_owned)
                .and_then(|uri| {
                    ctx.queue
                        .iter()
                        .find(|song| song.file == uri)
                        .map(|song| (uri.clone(), song.id))
                });
            if let Some((uri, song_id)) = error_link {
                // 2026-10-15: the recovery budget. Every entry MPD skips spawns
                // a full `yt-dlp` resolve, so a run of unresolved rows turns
                // into an unbounded chain (the resolve storm). Once the budget
                // is spent the row is left to MPD and said once: the queue
                // degrades to skipping instead of stalling on resolves.
                if !state.may_recover_decode_error(std::time::Instant::now()) {
                    log::warn!(
                        song_id, uri = uri.as_str();
                        "Too many stream decode failures in a row; leaving the rest to MPD"
                    );
                    status_warn!(
                        "Several queued streams failed to open; skipping them instead of resolving each one"
                    );
                    state.last_reported_mpd_error = ctx.status.error.clone();
                } else {
                    // Round 98: play the replacement only when MPD
                    // is actually stuck on this entry (it is the
                    // current song, or playback is stopped). A
                    // next-song **prefetch** failure names an
                    // entry MPD has not reached yet; playing it
                    // there cut the current track short, so such
                    // an entry is only swapped in place and MPD
                    // plays it when it gets there.
                    let stuck =
                        ctx.status.songid == Some(song_id) || ctx.status.state == State::Stop;
                    // Round 105: a row the error names that is
                    // neither a tagged link nor an expired stream
                    // (a radio URL, a plain http URL) resolves to
                    // `false` and keeps today's behavior.
                    let resolved = if crate::shared::ytdlp::tagged_stream_entry(&uri).is_some() {
                        if stuck {
                            crate::ui::modals::paste::resolve_tagged_queue_entry(
                                ctx, &uri, song_id,
                            )
                        } else {
                            crate::ui::modals::paste::resolve_tagged_queue_entry_in_place(
                                ctx, &uri, song_id,
                            )
                        }
                    } else {
                        crate::ui::modals::paste::re_resolve_expired_queue_entry(
                            ctx, &uri, song_id, stuck,
                        )
                    };
                    if resolved {
                        state.last_reported_mpd_error = ctx.status.error.clone();
                    }
                }
            }
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
                        client.save_queue_as_playlist(&playlist_name, Some(SaveMode::Replace))?;
                        Ok(())
                    });
                }
            }

            let mut start_render_loop = || {
                state.update_db_loop_guard =
                    Some(ctx.scheduler.repeated(Duration::from_millis(250), |(tx, _)| {
                        tx.send(AppEvent::RequestRender)?;
                        Ok(())
                    }));
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
                    state.update_db_loop_guard = None;
                }
                _ => {}
            }

            if previous_state != ctx.status.state
                && let Err(err) = ui.on_event(UiEvent::PlaybackStateChanged, ctx)
            {
                status_error!(error:? = err; "UI failed to handle playback state changed event, error: '{}'", err.to_status());
            }

            // Starting MPD playback pauses an mpv video
            // launched from s2udio (they never run together).
            if previous_state != State::Play
                && ctx.status.state == State::Play
                && crate::core::mpv::MPV_RUNNING.load(std::sync::atomic::Ordering::Relaxed)
            {
                mpv::pause_for_mpd_start();
                // The Queue tab followed the video session; it
                // follows the music now (a stale Video list has
                // nothing behind it and its wheel did nothing).
                if let Err(err) = ui.follow_mpd_playback(ctx) {
                    log::error!(error:? = err; "Failed to follow MPD playback");
                }
            }

            // Round 74 (74-1): a pasted YouTube-style link
            // that carried a timestamp (`?t=90`) — apply the
            // armed offset now that the status reports the
            // stream playing.
            apply_pending_start_seek(ctx);

            // Round 91: MPD can land on the queue entry of a
            // pasted stream that has not been resolved yet
            // (media keys, `next`, a restored queue); it
            // cannot open the link, so the entry is resolved
            // and replaced once — the replacement plays.
            // Round 105: the same for a current song whose
            // resolved URL expired (re-resolved from the
            // original link).
            resolve_current_song(ctx);

            // The mpv video / MPD audio UI-source switch
            // (music starts or stops while the video is
            // paused): refresh the album art box so it
            // follows the active source (the same switch
            // the mpv poll catches on the pause side).
            if previous_state != ctx.status.state && ctx.mpv.active {
                let source_before = !ctx.mpv.paused || previous_state != State::Play;
                let source_after = !ctx.mpv.paused || ctx.status.state != State::Play;
                if source_before != source_after
                    && let Err(err) = ui.refresh_album_art(ctx)
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
                    state.start_update_loop(ctx);
                }
                State::Play => {}
                State::Pause => {
                    // Round 63.1 (1): KEEP the status-update
                    // loop alive while paused. It is the
                    // frame driver (every status reply sets
                    // `render_wanted`), so the wall-clock
                    // marquee keeps visibly scrolling after
                    // `mpc pause` — the round-63 anchor
                    // advanced but no frames were drawn.
                    // The elapsed accumulation above keys
                    // off `State::Play` only, so
                    // `song_played` cannot drift while
                    // paused; the next Play arm replaces
                    // the guard (dropping it cancels).
                }
                State::Stop => {
                    // Round 75: only the *transition* into
                    // Stop is a song change. MPD keeps
                    // reporting Stop on every status/idle
                    // update while the player is stopped (a
                    // whole-library rescan, an lrcgen pass,
                    // a Jellyfin progress report, ...),
                    // and the unconditional flag re-ran the
                    // entire SongChanged fan-out on each
                    // one: `on_song_change` (the lyrics
                    // fetch), chapters, MPRIS metadata and
                    // the album-art refresh. Off the Queue
                    // tab that refresh re-showed the art
                    // that the very next frame erased
                    // again, forcing a full terminal clear
                    // + repaint: the top-left flicker
                    // reported while a Jellyfin video
                    // played (2026-09-11) — and the reason
                    // `rmpc-fetch-lyrics` ran every few
                    // seconds with nothing playing.
                    if previous_state != State::Stop {
                        song_changed = true;
                    }
                    ctx.song_played = None;
                    state.stop_update_loop();
                }
            }

            if let Some((_, song)) = ctx.find_current_song_in_queue()
                && Some(song.id) != current_song_id
            {
                // Round 38: `lyrics_source: LocalOnly` skips the
                // hook (the network-fetch vehicle) entirely.
                if let Some(command) = &ctx.config.on_song_change
                    && ctx.config.lyrics_source != crate::config::LyricsSource::LocalOnly
                {
                    let mut env = crate::core::command::create_env(ctx, std::iter::empty());

                    let prev_song_file = (previous_status.state != State::Stop)
                        .then_some(
                            previous_status
                                .song
                                .and_then(|idx| ctx.queue.get(idx).map(|song| song.file.clone())),
                        )
                        .flatten();

                    if let (Some(prev_song), Some(played)) = (prev_song_file, ctx.song_played) {
                        env.push(("PREV_SONG".to_owned(), prev_song));
                        env.push(("PREV_ELAPSED".to_owned(), played.as_secs().to_string()));
                    }

                    crate::core::command::run_external(command.clone(), env);
                }
                song_changed = true;
                ctx.song_played = Some(Duration::ZERO);
            }
            if song_changed && let Err(err) = ui.on_event(UiEvent::SongChanged, ctx) {
                status_error!(error:? = err; "UI failed to handle idle event, error: '{}'", err.to_status());
            }
            if song_changed {
                // Chapters (YouTube/Jellyfin/local) for the
                // new track, and MPRIS metadata (title +
                // thumbnail) for playing streams. A
                // chaptered track auto-opens the Queue
                // tab's Chapters list (the active tab is
                // never switched).
                crate::ui::modals::paste::ensure_chapters(ctx);
                crate::ui::modals::paste::ensure_mpris_metadata(ctx);
                // Round 95b (restored 105e): warm the next
                // queue entry's stream while this one plays, so
                // a playlist of stream links plays in order
                // (without it MPD fails 5-15 unresolved rows in
                // ~3 s per transition — the measured storm).
                crate::ui::modals::paste::warm_next_queue_link(ctx);
                ctx.auto_show_chapters();
                ctx.metadata_processed_song = ctx.status.songid;
            }

            // Round 98 (restored 105e): the look ahead is
            // re-evaluated on every status poll. A
            // ReplaceAndPlay (the resolution of the current
            // entry) changes MPD's song id first and refreshes
            // the queue a moment later, so the song-change call
            // above ran while the new entry was not in
            // `ctx.queue` yet and did nothing. The helper is
            // cheap and guards itself (one look ahead per
            // current song).
            crate::ui::modals::paste::warm_next_queue_link(ctx);
            ctx.last_status_update = Instant::now();
            *render_wanted = true;
        }
        (GLOBAL_VOLUME_UPDATE, None, MpdQueryResult::Volume(volume)) => {
            mpv::forward_volume(ctx, volume);
            *render_wanted = true;
        }
        (GLOBAL_QUEUE_UPDATE, None, MpdQueryResult::Queue(queue)) => {
            queue_update::handle_queue_update(ctx, ui, queue, render_wanted);
        }
        (EXTERNAL_COMMAND, None, MpdQueryResult::ExternalCommand(command, songs)) => {
            let songs = songs.iter().map(|s| s.file.as_str());
            crate::core::command::run_external(
                command,
                crate::core::command::create_env(ctx, songs),
            );
        }
        (id, target, data) => {
            if let Err(err) = ui.on_command_finished(id, target, data, ctx) {
                log::error!(error:? = err; "UI failed to handle command finished event");
            }
        }
    }
}

fn handle_idle_event_query(event: IdleEvent, ctx: &Ctx, result_ui_evs: &mut HashSet<IdleEvent>) {
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

/// Round 74 (74-1): status updates to spend on a pasted link's start offset
/// before giving up on a stream that never becomes seekable. At ~1 s per
/// status update this budget is a ~20 s window.
const MAX_START_SEEK_ATTEMPTS: u8 = 20;

/// Round 74 (74-1): minimum gap between start-offset seeks for the same song.
/// `status_update_interval_ms` may be as low as 16 ms, and MPD's `elapsed`
/// only reflects a seek a little after it lands — without this the retry
/// would issue ~20 seeks into the first fraction of a second.
const START_SEEK_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Round 91: resolve the queue entry of a pasted stream link that MPD
/// started on its own (a media-key `next`, `mpc next`, a queue restored from
/// MPD's state): the entry is still `watch?v=ID#s2u-audio`, which MPD cannot
/// open, so the link is resolved and the entry replaced in place, and the
/// replacement plays. Round 105: a current song whose resolved URL expired is
/// re-resolved from its original link the same way (MPD cannot open the dead
/// signed URL either). Runs on every status update because MPD keeps the
/// unplayable row as the current song until the replacement lands;
/// `resolve_tagged_queue_entry` fires once per queue entry and is the same
/// call the Queue tab's Enter makes, so the two paths cannot double-request
/// (Round 91). Round 105d: only while MPD is **playing** — a stopped queue's
/// retained `songid` can point at an unresolved row, and resolving it with
/// `ReplaceAndPlay` would start playback on its own (s2udio startup restoring
/// such a queue was observed auto-starting); the stopped-start belongs to
/// `play_from_stop`.
fn resolve_current_song(ctx: &Ctx) {
    if ctx.status.state != State::Play {
        return;
    }
    let Some(songid) = ctx.status.songid else {
        return;
    };
    let Some(song) = ctx.queue.iter().find(|song| song.id == songid) else {
        return;
    };
    if crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some() {
        crate::ui::modals::paste::resolve_tagged_queue_entry(ctx, &song.file, song.id);
    } else {
        crate::ui::modals::paste::re_resolve_expired_queue_entry(ctx, &song.file, song.id, true);
    }
}

/// Round 74 (74-1): apply the start offset of a pasted link (`?t=90`) once
/// its stream is actually playing.
///
/// MPD cannot seek a stream before it plays, and the offset cannot ride
/// along with `add`/`playid`, so `apply_resolved_streams` arms it on
/// `Ctx::pending_start_seek` (keyed by the stream URL — the song file MPD
/// reports) and this runs on every status update:
///
/// - every update while the song is short of the offset sets it. Repeat
///   attempts are harmless — the same position is set again — and are what
///   makes a seek that MPD was not ready for yet (a stream whose decoder is
///   still opening, one with no duration yet) land on a later update.
///   Measured against MPD 0.24 with a googlevideo stream: `seekcur` is
///   accepted even in the same command batch as `play`, so in practice the
///   first update applies it.
/// - the entry is dropped once `elapsed` reaches the offset (the seek
///   landed, or the user seeked past it), or after the attempt budget.
fn apply_pending_start_seek(ctx: &Ctx) {
    if ctx.status.state != State::Play {
        return;
    }
    let Some((_, song)) = ctx.find_current_song_in_queue() else {
        return;
    };
    let file = song.file.clone();
    let elapsed = ctx.status.elapsed.as_secs_f64();

    let secs = {
        let mut pending = ctx.pending_start_seek.borrow_mut();
        let Some(&(secs, attempts, last_attempt)) = pending.get(&file) else {
            return;
        };
        if elapsed >= secs {
            // Already there: the offset was reached (the seek landed, or the
            // user seeked past it). Nothing left to do.
            log::debug!(file = file.as_str(); "Start offset reached; clearing the pending seek");
            pending.remove(&file);
            return;
        }
        if attempts >= MAX_START_SEEK_ATTEMPTS {
            log::warn!(file = file.as_str(), seconds = secs; "Giving up on the pasted link's start offset");
            pending.remove(&file);
            return;
        }
        // A fast status cadence would otherwise repeat the seek many times
        // per second while MPD catches up; 500 ms apart is enough to cover a
        // stream that was not seekable on the first try.
        if last_attempt.is_some_and(|at| at.elapsed() < START_SEEK_RETRY_INTERVAL) {
            return;
        }
        pending.insert(file.clone(), (secs, attempts + 1, Some(Instant::now())));
        secs
    };

    let seek_to = secs.round().clamp(0.0, f64::from(u32::MAX)) as u32;
    log::debug!(file = file.as_str(), seconds = secs; "Applying the pasted link's start offset");
    ctx.command(move |client| {
        use crate::mpd::mpd_client::ValueChange;
        client.seek_current(ValueChange::Set(seek_to))?;
        Ok(())
    });
}

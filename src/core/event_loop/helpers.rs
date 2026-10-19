use crate::{
    ctx::Ctx,
    mpd::{mpd_client::MpdClient, QueuePosition},
    shared::{macros::status_info, mpd_client_ext::MpdClientExt},
};

/// Start mpv on a torrent's stream entries (round 17; the fresh-engine
/// single-file path and the scanned Play all / Select files… path
/// converge here — round 54: the PLAIN-stream path only; committed
/// "Stream and download" plays route through the `s2udio dl` daemon's
/// engine instead): keep the engine alive in `Ctx.torrent_engine`, record
/// the session playlist (the Queue tab's Video list, like a Jellyfin
/// season play) and insert synthetic yt-info entries (title = file name,
/// channel = torrent name — in memory only, never persisted: the stream
/// URL embeds the rqbit auth token).
pub(super) fn start_torrent_playback(
    ctx: &mut Ctx,
    engine: std::sync::Arc<crate::core::torrent::TorrentEngine>,
    torrent_id: String,
    torrent_name: String,
    entries: Vec<crate::core::mpv::MpvPlaylistEntry>,
    infohash: Option<String>,
) {
    // R2: the outgoing session's plain-streamed torrents stop here — the
    // playlist was replaced (new files added). Their transfers AND
    // seeding stop (forget keeps partials); emptied engines are pruned.
    // The incoming play itself is exempt (`keep`): re-streaming the very
    // torrent that is already playing on the same engine (a repeat paste
    // of the same magnet) must NOT forget its own torrent — the play's
    // ref is pushed below, after the stop.
    stop_finished_plain_streams(ctx, Some((engine.base_url().to_owned(), torrent_id.clone())));
    // Keep the engine alive for the whole session (the last Arc clone's
    // Drop kills the rqbit child on app exit). There is no cache-cleanup
    // policy: the torrent cache dir simply persists.
    *ctx.torrent_engine.borrow_mut() = Some(engine.clone());
    status_info!("Streaming {torrent_name}…");
    // Now-playing info: each file's name is its mpv entry title; the
    // torrent name becomes the MPRIS artist. Synthetic yt-info entries
    // keyed by each stream URL feed the info box / MPRIS / queue rows —
    // in memory only, the URL embeds the auth token.
    ctx.mpv.artist = torrent_name.clone();
    crate::ui::modals::paste::remember_torrent_entries(ctx, &torrent_name, &entries);
    crate::core::mpv::play_video_entries(ctx, entries);
    // R2 bookkeeping: remember the plain stream so it is forgotten
    // (transfer AND seeding stop, partials stay) when the session ends or
    // a later play replaces it. One ref per (engine, torrent): a repeat
    // paste replay dedupes instead of stacking duplicates.
    let mut refs = ctx.plain_stream_torrents.borrow_mut();
    if !refs.iter().any(|stream| {
        stream.engine.base_url() == engine.base_url() && stream.torrent_id == torrent_id
    }) {
        refs.push(crate::core::torrent::PlainTorrentStream {
            engine: engine.clone(),
            torrent_id,
            infohash,
        });
    }
}

/// R2 (round 54): stop every plain-streamed torrent that is no longer
/// playing. Called from `MpvSessionEnded` (the user stopped playback) and
/// from `start_torrent_playback` (a new play replaced the session's
/// streams). Each outgoing torrent is forgotten on its own engine
/// (`POST /torrents/{id}/forget` — transfer AND seeding stop, partials
/// stay in the cache, a re-stream resumes from them) — UNLESS a committed
/// downloader-daemon job for the same infohash is active (the daemon
/// engine owns the cache dir). Engines left with zero torrents are pruned
/// (no idle rqbit lingers).
pub(super) fn stop_finished_plain_streams(ctx: &mut Ctx, keep: Option<(String, String)>) {
    let refs = ctx.plain_stream_torrents.borrow().clone();
    if refs.is_empty() {
        return;
    }
    let daemon_has_job = |infohash: &Option<String>| -> bool {
        infohash.as_deref().is_some_and(|hash| {
            crate::core::dlctl::read_state().as_ref().is_some_and(|state| {
                crate::core::dlctl::job_active_for_infohash(state, hash)
            })
        })
    };
    for stream in &refs {
        // The incoming play re-streams this very torrent on this very
        // engine: it is still playing, nothing stops.
        if keep.as_ref().is_some_and(|(base, id)| {
            *base == stream.engine.base_url() && *id == stream.torrent_id
        }) {
            continue;
        }
        if daemon_has_job(&stream.infohash) {
            log::debug!(
                infohash:? = stream.infohash; "Not forgetting a plain-streamed torrent: an active committed job owns it"
            );
            continue;
        }
        if let Err(err) = crate::core::torrent::forget_torrent(&stream.engine, &stream.torrent_id)
        {
            log::warn!(error:? = err; "Failed to forget a plain-streamed torrent");
        } else {
            log::debug!(torrent_id:? = stream.torrent_id; "Forgot a plain-streamed torrent (stop download/seeding)");
        }
    }
    // Keep the incoming play's ref(s) (the event-loop push below adds the
    // current one); drop every outgoing ref.
    ctx.plain_stream_torrents.borrow_mut().retain(|stream| {
        keep.as_ref().is_some_and(|(base, id)| {
            *base == stream.engine.base_url() && *id == stream.torrent_id
        })
    });
    // Engines that now host nothing are pruned (their torrent was
    // forgotten; the outgoing refs are drained).
    crate::ui::modals::paste::prune_empty_engines(ctx);
}

/// Finish a stream download (`s2udio-downloads` save-as): run the spec's
/// replace action with the produced files. The persistent video queue is
/// s2udio-internal state and swaps here; queue/playlist replacements run
/// on the MPD client thread (with the downloads dir indexed first).
pub(super) fn complete_stream_download(
    ctx: &Ctx,
    spec: &crate::shared::ytdlp::StreamDownloadSpec,
    files: &[std::path::PathBuf],
    _position: Option<QueuePosition>,
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
        crate::ui::modals::paste::music_directory()
            .is_some_and(|music_dir| file.starts_with(std::path::Path::new(&music_dir)))
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

/// Opens the download panel for a just-committed download-only job: the
/// torrent picker's `Download` does not wait for the torrent to be `live`,
/// so there is no "Preparing downloader…" window — the job's own panel
/// (`InfoListModal`, the same one the Downloads modal shows) appears as soon
/// as the daemon registers the job.
///
/// Matching is by `DlJob::source_key`, the picker's own scan key: the
/// daemon generates the job id, so it is not known when the request is
/// written. Same-source commits coalesce onto one job, so the first match
/// wins. The pending key is cleared even when no job shows up: a spooled
/// request that the daemon has not picked up yet must not open a panel
/// later, out of context.
pub(super) fn open_pending_download_panel(ctx: &mut Ctx) {
    let Some(key) = ctx.dl_panel_pending.borrow().clone() else { return };
    let state = ctx.dl_state.borrow();
    let job_id = state
        .as_ref()
        .and_then(|state| {
            state
                .jobs
                .iter()
                .find(|job| job.source_key == key)
                .map(|job| job.job_id.clone())
        });
    drop(state);
    ctx.dl_panel_pending.borrow_mut().take();
    let Some(job_id) = job_id else {
        log::debug!(key:?; "Committed download has no daemon job yet (spooled)");
        return;
    };
    if crate::ui::modals::info_list_modal::torrent_info_rows(ctx, &job_id).is_empty() {
        return;
    }
    log::debug!(job_id:?; "Opening the committed download's progress panel");
    let menu = crate::ui::modals::InfoListModal::builder()
        .rows(crate::ui::modals::info_list_modal::torrent_info_rows(ctx, &job_id))
        .title("Download info")
        .column_widths(crate::ui::modals::info_list_modal::INFO_COLUMN_WIDTHS)
        .build();
    crate::shared::macros::modal!(ctx, menu);
}

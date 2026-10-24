use crate::{
    ctx::Ctx,
    jellyfin::JellyfinResult,
    mpd::mpd_client::MpdClient,
    shared::{
        events::{PlaylistAction, WorkDone},
        macros::{modal, status_error, status_info, status_warn},
        mpd_client_ext::MpdClientExt,
    },
    ui::{
        Ui, UiAppEvent, UiEvent,
        modals::{downloads::DownloadsModal, select_modal::SelectModal},
    },
};

use super::{helpers, mpv, status::StatusState};

/// Whether a resolve result must be **dropped** because a newer play request
/// superseded it (2026-10-25).
///
/// Clears the pending-play record that belonged to it: a cancelled request
/// must not block the next one, and its entry never plays.
fn is_superseded_play(ctx: &Ctx, play_stamp: Option<u64>) -> bool {
    let Some(generation) = play_stamp else {
        // Not a play request: queue appends, the look-ahead swap, metadata
        // warms and the automatic recovery are never superseded.
        return false;
    };
    if crate::shared::ytdlp::play_request::is_current(generation) {
        return false;
    }
    ctx.clear_pending_play(generation);
    true
}

pub(super) fn handle_work_done(
    result: WorkDone,
    ctx: &mut Ctx,
    ui: &mut Ui,
    status_state: &mut StatusState,
    render_wanted: &mut bool,
) {
    match result {
        WorkDone::YtStreamsResolved { info, action, failures, play_stamp } => {
            // 2026-10-25: a play resolve the user has already moved on from
            // is **dropped**: a newer play request superseded it, so applying
            // it would insert or play the entry they left behind (the
            // Playlists tab's "the first resolve is never cancelled"). Its
            // parse markers still come down, so nothing spins forever.
            if is_superseded_play(ctx, play_stamp) {
                log::debug!(generation:? = play_stamp; "Dropping a superseded play resolve");
                let mut keys: Vec<String> = Vec::new();
                for item in &info {
                    keys.push(item.url.clone());
                    if !item.original_url.is_empty() {
                        keys.push(item.original_url.clone());
                    }
                }
                for failure in &failures {
                    if let Some((url, _)) = failure.split_once(": ") {
                        keys.push(url.to_owned());
                    }
                }
                crate::ui::modals::paste::clear_stream_parse_pending(ctx, &keys);
                *render_wanted = true;
                return;
            }
            if let Some(generation) = play_stamp {
                // The play request landed: it is no longer pending (the
                // entry it named is about to play, or it failed below).
                ctx.clear_pending_play(generation);
            }
            // Round 95: the resolve landed (for good or ill) —
            // those rows stop spinning in the queue's Duration
            // column.
            {
                let mut keys: Vec<String> = Vec::new();
                for item in &info {
                    keys.push(item.url.clone());
                    if !item.original_url.is_empty() {
                        keys.push(item.original_url.clone());
                    }
                }
                for failure in &failures {
                    if let Some((url, _)) = failure.split_once(": ") {
                        keys.push(url.to_owned());
                    }
                }
                crate::ui::modals::paste::clear_stream_parse_pending(ctx, &keys);
            }
            for failure in &failures {
                status_warn!("Failed to resolve stream: {failure}");
            }
            if info.is_empty() {
                // Round 103: an entry yt-dlp refuses for good
                // (DRM-protected, private, removed) is dropped from
                // the queue instead of being left for MPD to fail
                // on; a transient failure keeps its row.
                if !crate::ui::modals::paste::drop_unresolvable_queue_entry(
                    ctx, &action, &failures,
                ) {
                    status_warn!("No stream could be resolved");
                }
            } else {
                crate::ui::modals::paste::apply_resolved_streams(
                    ctx, info, action, failures,
                );
                // Round 90.3: a pasted link starts playing before
                // its resolve lands (round 92), so a chaptered
                // video's markers only exist now — run the same
                // chapters fan-out as a song change: take them for
                // the current song, and let the Queue tab follow
                // the playing video into its Chapters list. Without
                // this the list stayed on the mpv playlist until
                // the video was played a second time (the info is
                // then already known when the session starts).
                crate::ui::modals::paste::ensure_chapters(ctx);
                ctx.auto_show_chapters();
                if let Err(err) = ui.follow_video_session(ctx) {
                    log::error!(error:? = err; "Failed to follow the video session after a stream resolve");
                }
            }
            *render_wanted = true;
        }
        WorkDone::TorrentScanFiles { key, name, files } => {
            // Phase 3 of the scan: the file list is known — the torrent
            // picker renders its rows from it and enables Download, while
            // Stream stays gated on phase 4 (`TorrentScanned`).
            crate::ui::modals::paste::on_torrent_scan_files(ctx, key, name, files);
            *render_wanted = true;
        }
        WorkDone::TorrentScanned { key, result, .. } => {
            // Round 17: a popup scan landed — store the engine
            // + file list (or the failure) and refresh the
            // popup's [Torrent] section ("Loading…" → the play
            // actions the scan enables).
            crate::ui::modals::paste::on_torrent_scanned(ctx, key, result);
            *render_wanted = true;
        }
        WorkDone::DlDaemonStarted { result } => {
            // Round 54: the daemon's spawn failed — abort an
            // open "Preparing downloader…" wait with the error
            // (a success is a no-op; the response watcher
            // drives the wait).
            crate::ui::modals::paste::on_dl_daemon_started(ctx, result);
            *render_wanted = true;
        }
        WorkDone::TorrentScanProgress { key, progress } => {
            // Round 18: refresh the paste popup's wait window
            // (elapsed counter + DL-speed / needed-speed check)
            // with the scan's live progress.
            crate::ui::modals::paste::on_torrent_scan_progress(ctx, key, progress);
            *render_wanted = true;
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
            info_hash,
        } => {
            // M2 single-file plain play (the fresh-engine
            // fallback — the scanned path arrives as
            // AppEvent::TorrentScannedPlay). Round 54:
            // committed actions never arrive here (they run on
            // the `s2udio dl` daemon).
            // Round 20: register the prepared play as a
            // single-file scan under the item's canonical key —
            // the engine is shared via `Arc`, so a repeat paste
            // of the same torrent reuses it instead of spawning a
            // second rqbit against the same cache.
            let engine = std::sync::Arc::new(engine);
            ctx.torrent_scans.borrow_mut().insert(
                key,
                Ok(crate::core::torrent::TorrentScan {
                    engine: Some(engine.clone()),
                    torrent_id: torrent_id.clone(),
                    torrent_name: torrent_name.clone(),
                    info_hash: info_hash.clone(),
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
            helpers::start_torrent_playback(
                ctx,
                engine,
                torrent_id,
                torrent_name,
                vec![entry],
                info_hash.clone(),
            );
            *render_wanted = true;
        }
        WorkDone::JellyfinFetched { id, data } => {
            if let Err((id, data)) =
                mpv::handle_jellyfin_fetched(id, data, ctx, ui, render_wanted)
            {
                route_jellyfin_result(id, data, ctx, ui);
                *render_wanted = true;
            }
        }
        WorkDone::YtDlpPlaylistResolved { urls, action } => {
            match action {
                // Round 79: a playlist link's items are known —
                // import them (cache download + queue) or save
                // every track as a file.
                PlaylistAction::ImportToQueue { position, autoplay } => {
                    if urls.is_empty() {
                        status_warn!("The playlist has no tracks to import");
                    } else {
                        status_info!(
                            "Importing {} track(s) from the playlist",
                            urls.len()
                        );
                        ctx.ytdlp_manager.queue_download_many(urls, position, autoplay);
                        ctx.ytdlp_manager.download_next();
                    }
                }
                // Round 82: a picker row — the playlist's items are
                // known now, so open the list of videos and
                // continue from there (audio/video, then the target
                // playlist or the save).
                PlaylistAction::Pick(kind) => {
                    if urls.is_empty() {
                        status_warn!("The playlist has no items");
                    } else {
                        crate::ui::modals::paste::open_playlist_picker(ctx, urls, kind);
                    }
                }
                // Round 95: a pasted playlist link's queue
                // rows — every item lands at once, as a stream
                // link carrying the listing's title, channel
                // and duration (audio: the MPD queue, video:
                // the persistent video playlist).
                PlaylistAction::QueueStreams { audio, autoplay } => {
                    // The wait is over: `queue_playlist_streams` clears
                    // the import's loading row as it adds the rows
                    // (2026-10-24).
                    crate::ui::modals::paste::queue_playlist_streams(
                        ctx, urls, audio, autoplay,
                    );
                }
            }
            *render_wanted = true;
        }
        WorkDone::YtDlpPlaylistFailed { action, error } => {
            // The listing failed, so nothing will land: take the import's
            // loading row down and say why (2026-10-24). Without this the
            // Queue list would spin forever and only a generic work error
            // would have been logged.
            crate::ui::modals::paste::finish_playlist_import(ctx);
            log::warn!(action:? = action, error = error.as_str(); "Could not list the playlist");
            status_error!("Could not load the playlist: {error}");
            *render_wanted = true;
        }
        WorkDone::YtDlpDownloaded { id, result, spec } => {
            match ctx.ytdlp_manager.resolve_download(id, result) {
                Ok((result, position, autoplay)) => {
                    let cache_dir = ctx.config.cache_dir.clone();
                    match spec {
                        // A stream download (the controls'
                        // Download button or a right-click
                        // replace): run the spec's replace
                        // action with every produced file.
                        Some(spec) => helpers::complete_stream_download(
                            ctx,
                            &spec,
                            &result.file_paths,
                            position,
                        ),
                        None => {
                            let path = result.file_path;
                            ctx.command(move |client| {
                                // Round 79: "Import and play"
                                // starts the first track the
                                // moment it lands (it is
                                // appended, so its index is the
                                // queue length right before the
                                // add).
                                let play_at = if autoplay {
                                    // `playlistinfo` is empty
                                    // (`None`) for an empty
                                    // queue — the new entry is
                                    // then at index 0.
                                    Some(
                                        client
                                            .playlist_info()?
                                            .map_or(0, |songs| songs.len()),
                                    )
                                } else {
                                    None
                                };
                                client.add_downloaded_file_to_queue(
                                    path,
                                    cache_dir.as_deref(),
                                    position,
                                )?;
                                if let Some(idx) = play_at {
                                    client.play_position_safe(idx)?;
                                }
                                Ok(())
                            });
                            if autoplay {
                                status_info!("Playing the first track of the playlist");
                            }
                        }
                    }
                }
                Err(err) => {
                    status_error!("Yt-dlp resulted in error: {err}");
                }
            }
            ctx.ytdlp_manager.download_next();
            if let Err(err) = ui.on_event(UiEvent::DownloadsUpdated, ctx) {
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
                            modal!(ctx, DownloadsModal::new(ctx));
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
                    .ctx(ctx)
                    .title("Search results")
                    .confirm_label("Select")
                    .options(labels)
                    .on_confirm(move |ctx, _label, idx| {
                        let result = ctx.ytdlp_manager.download_url(&items[idx].url, position);
                        match result {
                            Ok(()) => {
                                if ctx.config.auto_open_downloads {
                                    modal!(ctx, DownloadsModal::new(ctx));
                                }
                            }
                            Err(err) => {
                                status_error!("Failed to download selected item: {err}");
                            }
                        }
                        Ok(())
                    })
                    .build();

                if let Err(err) = ui.on_ui_app_event(UiAppEvent::Modal(Box::new(modal)), ctx) {
                    log::error!(error:? = err; "UI failed to handle modal event");
                }
            }

            *render_wanted = true;
        }
        // Round 96: hand the search modal its results (or the
        // failure) — the modal is the only listener that cares,
        // and it drops a reply it is no longer waiting for.
        WorkDone::SearchYtModalResults { request_id, items, error } => {
            if let Err(err) = ui.on_event(
                UiEvent::YtSearchResults { request_id, items, error },
                ctx,
            ) {
                log::error!(error:? = err; "UI failed to handle search results");
            }
            *render_wanted = true;
        }
        WorkDone::ImageResized { data } => {
            let event = match data {
                Ok(data) => UiEvent::ImageEncoded { data },
                Err(err) => UiEvent::ImageEncodeFailed { err },
            };

            if let Err(err) = ui.on_event(event, ctx) {
                log::error!(error:? = err; "UI failed to handle image resized event");
            }
            // The encoded image is drawn after the next frame's
            // buffer flush; make sure a frame actually runs.
            *render_wanted = true;
        }
        WorkDone::LyricsIndexed { index } => {
            ctx.lrc_index = index;
            if let Err(err) = ui.on_event(UiEvent::LyricsIndexed, ctx) {
                log::error!(error:? = err; "UI failed to handle lyrics indexed event");
            }
        }
        WorkDone::SingleLrcIndexed { path, metadata } => {
            if let Some(metadata) = metadata {
                ctx.lrc_index.add(path, metadata);
            }
            if let Err(err) = ui.on_event(UiEvent::LyricsIndexed, ctx) {
                log::error!(error:? = err; "UI failed to handle single lyrics indexed event");
            }
        }
        WorkDone::MpdCommandFinished { id, target, data } => {
            super::status::handle_mpd_command_finished(
                ctx,
                ui,
                status_state,
                id,
                target,
                data,
                render_wanted,
            );
        }
        WorkDone::None => {}
    }
}

fn route_jellyfin_result(id: &'static str, data: JellyfinResult, ctx: &mut Ctx, ui: &mut Ui) {
    let data = crate::MpdQueryResult::Any(Box::new(data));
    if let Err(err) = ui.on_command_finished(
        id,
        Some(crate::config::tabs::PaneType::Jellyfin {
            tree: crate::config::tabs::TreeBrowserArgs::default(),
        }),
        data,
        ctx,
    ) {
        log::error!(error:? = err; "UI failed to handle jellyfin result");
    }
}

/// 2026-10-25: a play resolve the user has moved on from never lands.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod superseded_play_tests {
    use super::is_superseded_play;
    use crate::{ctx::Ctx, shared::ytdlp::play_request};

    fn rig() -> Ctx {
        crate::tests::fixtures::ctx(
            (
                crossbeam::channel::unbounded().0,
                crossbeam::channel::unbounded().1,
            ),
            (
                crossbeam::channel::unbounded().0,
                crossbeam::channel::unbounded().1,
            ),
            (
                crossbeam::channel::unbounded().0,
                crossbeam::channel::unbounded().1,
            ),
        )
    }

    /// A newer play request makes the older resolve's result stale: it is
    /// dropped and its pending-play record goes with it.
    #[test]
    fn a_stale_play_resolve_is_dropped() {
        let _guard = crate::tests::fixtures::PLAY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ctx = rig();
        let stale = play_request::next();
        ctx.set_pending_play(stale, "entry-a".to_owned());
        let _newer = play_request::next();

        assert!(is_superseded_play(&ctx, Some(stale)));
        assert_eq!(ctx.pending_play.borrow().clone(), None, "the record is cleared");
    }

    /// The current play request's result is applied.
    #[test]
    fn the_current_play_resolve_is_applied() {
        let _guard = crate::tests::fixtures::PLAY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ctx = rig();
        let current = play_request::next();
        ctx.set_pending_play(current, "entry-a".to_owned());

        assert!(!is_superseded_play(&ctx, Some(current)));
        assert!(ctx.pending_play.borrow().is_some(), "it is still pending");
    }

    /// Only play requests carry a stamp; everything else is never dropped.
    #[test]
    fn an_unstamped_resolve_is_never_dropped() {
        let _guard = crate::tests::fixtures::PLAY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ctx = rig();
        assert!(!is_superseded_play(&ctx, None));
    }
}

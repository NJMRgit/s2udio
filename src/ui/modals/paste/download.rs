use anyhow::Result;
use crate::{
    ctx::Ctx,
    shared::{
        events::{PlaylistPick, WorkRequest},
        macros::{modal, status_error, status_info, status_warn},
        ytdlp::{
            ChapterSection, ReplaceAction, YtDlpContent, YtDlpHost, YtDlpItem, YtStreamInfo,
            yt_video_id,
        },
    },
    ui::modals::menu::{ListSection, modal::MenuModal},
};

use super::{
    PastedItem, close_paste_popup, magnet_infohash_full, pick_playlist_items,
    remember_torrent_entries, stream_info_for, stream_row_meta, torrent_item,
};

/// The downloads folder name (`s2udio-downloads`). Downloads land in
/// `~/Downloads/<name>`, outside the MPD library; the MPD tab shows the
/// folder as "Downloads" at the top of the library from a disk listing
/// (see `directories.rs`).
pub const DOWNLOADS_DIR_NAME: &str = "s2udio-downloads";
/// The downloads folder (`~/Downloads/s2udio-downloads`): stream
/// downloads, torrent "Download" / "Download all" saves and future saved
/// torrents land here. It lives OUTSIDE the MPD library — MPD cannot
/// play files from it (video plays via mpv; the browser lists the folder
/// from disk). `None` when `$HOME` is unset.
pub fn downloads_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join("Downloads").join(DOWNLOADS_DIR_NAME))
}
/// The `Download` row's child list (round 78): audio or video, each with the
/// single-file / all-chapters / chapter-picker choices.
pub(super) fn download_submenu(ctx: &Ctx, url: String, info: Option<YtStreamInfo>) -> ListSection {
    let mut sub = ListSection::new(ctx.config.theme.current_item_style);
    let audio = download_kind_menu(ctx, url.clone(), info.clone(), true);
    let video = download_kind_menu(ctx, url, info, false);
    sub.add_submenu_item("Audio", audio);
    sub.add_submenu_item("Video", video);
    sub.add_item("Cancel", |_ctx| Ok(()));
    sub
}
/// One download kind's options: `Single File`, `All Chapters` (one file per
/// chapter) and `Chapter(s)` (pick the chapters to save) — the chapter rows
/// appear once the link's stream info is known.
fn download_kind_menu(
    ctx: &Ctx,
    url: String,
    info: Option<YtStreamInfo>,
    audio_only: bool,
) -> ListSection {
    let mut sub = ListSection::new(ctx.config.theme.current_item_style);
    let chapters = info
        .as_ref()
        .map(|info| info.chapters.clone())
        .unwrap_or_default();
    let known = info.is_some();
    let has_chapters = chapters.len() > 1;
    let single_url = url.clone();
    sub.add_item("Single File", move |ctx| {
        queue_stream_download(ctx, &single_url, audio_only, false, ReplaceAction::None);
        Ok(())
    });
    if has_chapters {
        let all_url = url.clone();
        sub.add_item("All Chapters", move |ctx| {
            queue_stream_download(ctx, &all_url, audio_only, true, ReplaceAction::None);
            Ok(())
        });
        let mut picker = ListSection::new(ctx.config.theme.current_item_style);
        for chapter in &chapters {
            picker.add_check_item(chapter.title.clone(), false);
        }
        picker.add_check_buttons("Download", "Cancel");
        let picker_url = url;
        let picker_chapters = chapters;
        picker.check_list(move |ctx, selected, _choice| {
            let sections: Vec<ChapterSection> = selected
                .iter()
                .filter_map(|idx| picker_chapters.get(*idx))
                .map(|chapter| ChapterSection {
                    start_secs: chapter.start_secs,
                    end_secs: chapter.end_secs,
                    title: chapter.title.clone(),
                })
                .collect();
            queue_stream_download_sections(ctx, &picker_url, audio_only, sections);
            Ok(())
        });
        sub.add_submenu_item("Chapter(s)", picker);
    } else if !known {
        sub.header("Resolving chapters…");
    }
    sub
}

/// Round 82: a playlist's `Download` row — Audio or Video, each with
/// `All files` (save every track of the playlist) and `Select files` (pick
/// which videos to save). Chapters never apply to a playlist's items, so
/// there is no `Single File` / `All Chapters` / `Chapter(s)` row here.
pub(super) fn playlist_download_menu(ctx: &Ctx, url: String) -> ListSection {
    let mut sub = ListSection::new(ctx.config.theme.current_item_style);
    for (label, audio_only) in [("Audio", true), ("Video", false)] {
        let mut kind = ListSection::new(ctx.config.theme.current_item_style);
        let all_url = url.clone();
        kind.add_item("All files", move |ctx| {
            pick_playlist_items(ctx, &all_url, PlaylistPick::SaveAll { audio_only });
            Ok(())
        });
        let pick_url = url.clone();
        kind.add_item("Select files", move |ctx| {
            pick_playlist_items(ctx, &pick_url, PlaylistPick::SaveSelected { audio_only });
            Ok(())
        });
        kind.add_item("Cancel", |_ctx| Ok(()));
        sub.add_submenu_item(label, kind);
    }
    sub.add_item("Cancel", |_ctx| Ok(()));
    sub
}

// ============================================================================
// Round 54 — downloader daemon client (committed torrent downloads)
// ============================================================================

/// The replacement id of the "Preparing downloader…" wait window: the
/// rebuilt modal (elapsed counter tick) replaces the open one in place.
const DL_WAIT_REPLACEMENT_ID: &str = "dl_wait";
/// An open round-54 download wait ("Preparing downloader…"): a committed
/// enqueue-with-play or a daemon-routed re-stream is waiting for the
/// daemon's response file. The wait shows while the daemon adds the
/// torrent (a cold magnet can take minutes — open-ended, like the scan
/// wait); Esc cancels it (a stop request stops the daemon job).
pub struct DlWaitState {
    /// The request id whose response file the wait polls.
    pub request_id: String,
    /// The open wait modal (popped by the ready/failure path without
    /// running its Esc-cancel close hook).
    pub modal_id: Option<crate::shared::id::Id>,
    /// The wait watcher thread's cancel signal (fired on Esc).
    pub cancel: crossbeam::channel::Sender<()>,
    /// Round 110: this wait is the **one** retry after its response's stream
    /// URLs failed a pre-launch probe. A second failure is reported, not
    /// retried again.
    pub retried: bool,
}

/// The wait window's elapsed counter ("mm:ss", same renderer as the scan
/// wait).
fn dl_wait_elapsed(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Open the "Preparing downloader…" wait window and start its watcher
/// thread (`WorkRequest::WatchDlResponse` polls the daemon's response
/// file, sends `DlWaitProgress` every second and `DlWaitReady` when the
/// response lands).
fn open_dl_wait(ctx: &Ctx, request_id: String) {
    open_dl_wait_retried(ctx, request_id, false);
}

/// [`open_dl_wait`] with the round-110 retry marker.
fn open_dl_wait_retried(ctx: &Ctx, request_id: String, retried: bool) {
    let (cancel_tx, cancel_rx) = crossbeam::channel::unbounded();
    let menu = dl_wait_menu(ctx, request_id.clone(), 0);
    let modal_id = crate::ui::modals::Modal::id(&menu);
    *ctx.dl_wait.borrow_mut() = Some(DlWaitState {
        request_id: request_id.clone(),
        modal_id: Some(modal_id),
        cancel: cancel_tx,
        retried,
    });
    modal!(ctx, menu);
    let _ = ctx.work_sender.send(WorkRequest::WatchDlResponse {
        request_id,
        cancel: cancel_rx,
    });
}

/// The wait window modal: a live "Preparing downloader… mm:ss" header
/// (rebuilt with a `dl_wait` replacement id so the tick replaces it in
/// place), an "esc to cancel" hint and a Cancel item. Its close hook
/// (Esc / Cancel / any destroy) stops the daemon job and cancels the
/// watcher.
fn dl_wait_menu(ctx: &Ctx, request_id: String, elapsed_secs: u64) -> MenuModal<'static> {
    MenuModal::new(ctx)
        .replacement_id(DL_WAIT_REPLACEMENT_ID)
        .list_section(ctx, |mut section| {
            section.header(format!(
                "Preparing downloader… {}",
                dl_wait_elapsed(elapsed_secs)
            ));
            section.header("esc to cancel");
            section.set_on_close(move |ctx| {
                // The user cancelled the wait: stop the daemon job (the
                // daemon matches it by the creating request id).
                let cancel = ctx.dl_wait.borrow().as_ref().map(|w| w.cancel.clone());
                let _ = crate::core::dlctl::write_stop_request(
                    None,
                    Some(&request_id),
                    None,
                );
                if let Some(cancel) = cancel {
                    let _ = cancel.send(());
                }
                ctx.dl_wait.borrow_mut().take();
            });
            section.add_item("Cancel", |_ctx| Ok(()));
            Some(section)
        })
        .build()
}

/// The wait window's 1 s tick: refresh the open modal's elapsed counter
/// (replacement id — the rebuilt modal replaces the open one in place).
pub fn on_dl_wait_progress(ctx: &Ctx, request_id: &str, elapsed_secs: u64) {
    let is_current = ctx
        .dl_wait
        .borrow()
        .as_ref()
        .is_some_and(|w| w.request_id == request_id);
    if !is_current {
        return;
    }
    let menu = dl_wait_menu(ctx, request_id.to_owned(), elapsed_secs);
    let modal_id = crate::ui::modals::Modal::id(&menu);
    if let Some(wait) = ctx.dl_wait.borrow_mut().as_mut() {
        wait.modal_id = Some(modal_id);
    }
    modal!(ctx, menu);
}

/// A downloader wait finished: the daemon response is ready — close the
/// wait modal (without its Esc hook — the job exists, a stop request must
/// NOT fire) and play through the daemon engine (or report the error).
/// Stale events (the wait was cancelled / a newer wait replaced it) are
/// dropped.
pub fn on_dl_wait_ready(
    ctx: &mut Ctx,
    request_id: &str,
    result: Result<crate::core::dlctl::DlJobResponse, String>,
) {
    let is_current = ctx
        .dl_wait
        .borrow()
        .as_ref()
        .is_some_and(|w| w.request_id == request_id);
    if !is_current {
        return;
    }
    let wait = ctx.dl_wait.borrow_mut().take().expect("the current wait exists");
    if let Some(modal_id) = wait.modal_id {
        let _ = ctx
            .app_event_sender
            .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(modal_id)));
    }
    match result {
        Ok(response) if response.error.is_none() => {
            start_daemon_play(ctx, &response, wait.retried);
        }
        Ok(response) => {
            status_error!("{}", response.error.clone().unwrap_or_else(|| "Downloader failed".to_owned()));
        }
        Err(err) => {
            status_error!("{err}");
        }
    }
}

/// The daemon's start finished on the work thread: a failure aborts an
/// open wait (closes the modal, cancels the watcher, removes the spool
/// request so it cannot fire later); a success is a no-op (the watcher
/// handles the rest).
pub fn on_dl_daemon_started(ctx: &Ctx, result: Result<(), String>) {
    if result.is_ok() {
        return;
    }
    let err = result.unwrap_err();
    let Some(wait) = ctx.dl_wait.borrow_mut().take() else {
        // Download-only commits keep their spool request: a later
        // `s2udio dl start` resumes the download (R1 — the commit must
        // not vanish).
        return;
    };
    let _ = wait.cancel.send(());
    if let Some(path) = crate::core::dlctl::request_path(&wait.request_id) {
        let _ = std::fs::remove_file(path);
    }
    if let Some(modal_id) = wait.modal_id {
        let _ = ctx
            .app_event_sender
            .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(modal_id)));
    }
    status_error!("The downloader daemon failed to start: {err}");
}

/// Round-54 hazard guard (§2.2): before a torrent gets a daemon engine on
/// the cache dir, no TUI engine may still download it. Forget it on the
/// scan engine and on every engine currently plain-streaming it (matched
/// by infohash), and drop the scan entry. A plain stream still playing on
/// a TUI engine stalls briefly — the daemon response replays it through
/// the daemon engine.
fn release_torrent_from_tui(ctx: &Ctx, key: &str, infohash: Option<&str>) {
    // Clone first (round 55 F1): a `borrow()` Ref live across the
    // `borrow_mut()` below panicked with `RefCell already borrowed` — the
    // `if let` scrutinee's temporary lives through the whole block.
    let scanned = ctx.torrent_scans.borrow().get(key).cloned();
    if let Some(Ok(scan)) = scanned {
        let _ = crate::core::torrent::forget_torrent(&scan.engine, &scan.torrent_id);
        ctx.torrent_scans.borrow_mut().remove(key);
        log::debug!(
            key:?; "Forgot the scanned torrent on its TUI engine (daemon takes over)"
        );
    }
    if let Some(hash) = infohash {
        let mut refs = ctx.plain_stream_torrents.borrow_mut();
        refs.retain(|stream| {
            if stream.infohash.as_deref() == Some(hash) {
                let _ = crate::core::torrent::forget_torrent(&stream.engine, &stream.torrent_id);
                log::debug!(
                    hash:?; "Forgot a plain-streamed torrent on its TUI engine (daemon takes over)"
                );
                false
            } else {
                true
            }
        });
    }
}

/// Ensure the downloader daemon is running (the work thread spawns
/// `s2udio dl serve` when it is not; the wait watcher covers the spawn
/// window).
fn ensure_daemon_started(ctx: &Ctx) {
    let running = crate::core::dlctl::read_state()
        .as_ref()
        .is_some_and(crate::core::dlctl::daemon_running);
    if !running {
        let _ = ctx.work_sender.send(WorkRequest::StartDlDaemon);
    }
}

/// A committed torrent action (round 54): enqueue a downloader-daemon job
/// (deduped by infohash — a second committed action extends the existing
/// job's kept-file list) and — for "Stream and download" / the picker's
/// "Download & Play" (`play: true`) — wait for the daemon's response,
/// then play through the daemon engine (survives the TUI, R1). For
/// download-only the Downloads modal shows the progress.
pub(super) fn commit_to_daemon(
    ctx: &Ctx,
    item: &PastedItem,
    key: &str,
    indices: Vec<usize>,
    play: bool,
) -> Result<()> {
    if !ctx.config.torrent.enabled {
        status_warn!("Torrent streaming is disabled in the config");
        return Ok(());
    }
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    let (infohash, torrent_name, kept) = match &scan {
        Some(scan) => {
            let file_indices: Vec<usize> = if indices.is_empty() {
                scan.pick_playable().into_iter().map(|f| f.index).collect()
            } else {
                indices.iter().filter(|i| **i < scan.files.len()).copied().collect()
            };
            let kept = file_indices
                .iter()
                .filter_map(|i| scan.files.get(*i))
                .map(|f| crate::core::dlctl::DlKeptFile {
                    index: f.index,
                    name: f.name.clone(),
                    length: f.length,
                })
                .collect::<Vec<_>>();
            (scan.info_hash.clone(), Some(scan.torrent_name.clone()), kept)
        }
        None => (None, None, Vec::new()),
    };
    let infohash = infohash.or_else(|| match item {
        PastedItem::Magnet(magnet) => magnet_infohash_full(magnet),
        _ => None,
    });
    release_torrent_from_tui(ctx, key, infohash.as_deref());
    let request_id = crate::core::dlctl::new_request_id();
    let request = crate::core::dlctl::DlJobRequest::Enqueue {
        id: request_id.clone(),
        infohash,
        source_key: key.to_owned(),
        torrent_item: torrent_item(item).into(),
        torrent_name,
        files: kept,
        play,
    };
    crate::core::dlctl::write_request(&request)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    ensure_daemon_started(ctx);
    // Refresh + keep `Ctx.dl_state` fresh while the job runs (the event
    // loop's `DlStatePoll` arm restarts its 1 s guard while jobs are
    // active).
    let _ = ctx
        .app_event_sender
        .send(crate::AppEvent::DlStatePoll);
    close_paste_popup(ctx);
    if play {
        open_dl_wait(ctx, request_id);
    } else {
        let label = scan
            .as_ref()
            .map(|scan| scan.torrent_name.clone())
            .unwrap_or_else(|| item.label());
        status_info!("Downloading {label}… (progress in the Downloads modal)");
    }
    Ok(())
}

/// A plain re-stream of a torrent that has an ACTIVE committed job
/// (round 54, §2.2): playback routes through the daemon engine — no
/// second TUI engine on the cache dir. The stream request does NOT extend
/// the job's kept-file list.
pub(super) fn stream_via_daemon(ctx: &Ctx, item: &PastedItem, key: &str, indices: Vec<usize>) -> Result<()> {
    let scan = ctx.torrent_scans.borrow().get(key).cloned().and_then(Result::ok);
    let (infohash, torrent_name) = match &scan {
        Some(scan) => (scan.info_hash.clone(), Some(scan.torrent_name.clone())),
        None => (None, None),
    };
    let infohash = infohash.or_else(|| match item {
        PastedItem::Magnet(magnet) => magnet_infohash_full(magnet),
        _ => None,
    });
    release_torrent_from_tui(ctx, key, infohash.as_deref());
    let request_id = crate::core::dlctl::new_request_id();
    let request = crate::core::dlctl::DlJobRequest::Stream {
        id: request_id.clone(),
        infohash,
        source_key: key.to_owned(),
        torrent_item: torrent_item(item).into(),
        torrent_name,
        file_indices: indices,
    };
    crate::core::dlctl::write_request(&request)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    ensure_daemon_started(ctx);
    let _ = ctx
        .app_event_sender
        .send(crate::AppEvent::DlStatePoll);
    close_paste_popup(ctx);
    open_dl_wait(ctx, request_id);
    Ok(())
}

/// Round 64: stream a currently-downloading daemon job from the Downloads
/// modal (same flow as the paste picker's "Stream" — the engine serves
/// the already-downloaded pieces while the job continues). The request
/// carries the job's own fields (`file_indices` empty = the single best
/// playable file) and does NOT extend the job's kept-file list.
pub(crate) fn stream_daemon_job(ctx: &Ctx, job: &crate::core::dlctl::DlJob) -> Result<()> {
    let request_id = crate::core::dlctl::new_request_id();
    let request = crate::core::dlctl::DlJobRequest::Stream {
        id: request_id.clone(),
        infohash: job.infohash.clone(),
        source_key: job.source_key.clone(),
        torrent_item: job.torrent_item.clone(),
        torrent_name: Some(job.torrent_name.clone()),
        file_indices: Vec::new(),
    };
    crate::core::dlctl::write_request(&request)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    ensure_daemon_started(ctx);
    let _ = ctx
        .app_event_sender
        .send(crate::AppEvent::DlStatePoll);
    open_dl_wait(ctx, request_id);
    Ok(())
}

/// Play a downloader-daemon response (round 54, §2.3): build the mpv
/// entries from the response's stream URLs (userinfo auth), insert the
/// synthetic yt-info entries (title = file name, channel = torrent name —
/// exactly like a TUI torrent play), record the job in the streaming
/// marker (R2.5 — the daemon defers moving its files) and prune TUI
/// engines the commit path left idle.
///
/// Round 110: every stream URL is **probed** first (see
/// `torrent::probe_stream_url`). The daemon answers with the engine that was
/// live when it replied; an engine that has since been replaced or stopped
/// answers `401`/`404`/`500`, and mpv then fails inside a second — silently,
/// because it runs `--no-terminal`. That was the "Stream and download never
/// started the stream, but it downloaded" report: the track never played, the
/// download finished, and nothing was said. A failed probe re-requests the
/// stream **once** (`retried`), which re-registers the engine's current URL;
/// a second failure is reported instead of launching a doomed player.
pub fn start_daemon_play(
    ctx: &mut Ctx,
    response: &crate::core::dlctl::DlJobResponse,
    retried: bool,
) {
    let failures: Vec<String> = response
        .files
        .iter()
        .filter_map(|file| {
            crate::core::torrent::probe_stream_url(&file.stream_url)
                .err()
                .map(|err| format!("{}: {err}", file.name))
        })
        .collect();
    if !failures.is_empty() {
        log::warn!(
            job_id:? = response.job_id, retried, failures:?;
            "A downloader-daemon stream URL failed its pre-launch probe"
        );
        if !retried && retry_daemon_stream(ctx, response) {
            return;
        }
        status_error!(
            "Cannot stream {}: {}",
            response.torrent_name,
            failures.first().cloned().unwrap_or_default()
        );
        return;
    }
    let entries: Vec<crate::core::mpv::MpvPlaylistEntry> = response
        .files
        .iter()
        .map(|file| {
            crate::core::mpv::MpvPlaylistEntry::new(
                file.name.clone(),
                file.stream_url.clone(),
                None,
            )
        })
        .collect();
    if entries.is_empty() {
        status_warn!("The downloader response had no playable files");
        return;
    }
    ctx.mpv.artist = response.torrent_name.clone();
    remember_torrent_entries(ctx, &response.torrent_name, &entries);
    crate::core::mpv::play_video_entries(ctx, entries);
    // R2.5: while the TUI streams this job, its completed files must not
    // be moved away from under mpv.
    ctx.dl_streaming_jobs.borrow_mut().insert(response.job_id.clone());
    let jobs: Vec<String> = ctx.dl_streaming_jobs.borrow().iter().cloned().collect();
    crate::core::dlctl::write_streaming_marker(&jobs);
    super::prune_empty_engines(ctx);
    status_info!("Streaming {}…", response.torrent_name);
}

/// Round 110: whether a failed stream probe is retried. Once per response
/// (`retried`), and only while the job is still active — a job that already
/// finished, was stopped or vanished has no engine left to re-register, so the
/// failure is reported instead.
fn should_retry_daemon_stream(retried: bool, job: Option<&crate::core::dlctl::DlJob>) -> bool {
    !retried && job.is_some_and(|job| job.status.active())
}

/// The round-110 retry: ask the daemon for this job's stream URLs again (its
/// engine has been re-registered since) and wait for the fresh response.
/// `false` when the job is no longer known or no longer active — the caller
/// then reports the failure instead.
fn retry_daemon_stream(ctx: &Ctx, response: &crate::core::dlctl::DlJobResponse) -> bool {
    let job = ctx
        .dl_state
        .borrow()
        .as_ref()
        .and_then(|state| state.jobs.iter().find(|job| job.job_id == response.job_id).cloned());
    if !should_retry_daemon_stream(false, job.as_ref()) {
        log::debug!(job_id:? = response.job_id, status:? = job.as_ref().map(|j| j.status);
            "The failed stream's job cannot be retried; reporting the failure");
        return false;
    }
    let Some(job) = job else { return false };
    let request_id = crate::core::dlctl::new_request_id();
    let request = crate::core::dlctl::DlJobRequest::Stream {
        id: request_id.clone(),
        infohash: job.infohash.clone(),
        source_key: job.source_key.clone(),
        torrent_item: job.torrent_item.clone(),
        torrent_name: Some(job.torrent_name.clone()),
        file_indices: Vec::new(),
    };
    if let Err(err) = crate::core::dlctl::write_request(&request) {
        log::warn!(error:? = err; "Failed to re-request the torrent stream after a failed probe");
        return false;
    }
    status_info!("The stream URL was stale — asking the downloader for a fresh one…");
    open_dl_wait_retried(ctx, request_id, true);
    true
}

/// The mpv session ended (R2.5): clear the streaming marker so the daemon
/// stops deferring completed moves ("streams over").
pub fn untrack_daemon_streams(ctx: &Ctx) {
    ctx.dl_streaming_jobs.borrow_mut().clear();
    crate::core::dlctl::clear_streaming_marker();
}

/// Kill TUI engines that no longer host any torrent (round 54, §2.4
/// prune): an engine whose `GET /torrents` list is empty has nothing to
/// do — drop its scan entries and, when it is the current engine, clear
/// `Ctx.torrent_engine`. The last `Arc`'s `Drop` kills rqbit (no idle
/// engine lingers).
pub fn prune_empty_engines(ctx: &Ctx) {
    let bases: std::collections::HashSet<String> = {
        let mut bases: Vec<String> = ctx
            .torrent_scans
            .borrow()
            .values()
            .filter_map(|result| result.as_ref().ok())
            .map(|scan| scan.engine.base_url().to_owned())
            .collect();
        if let Some(engine) = ctx.torrent_engine.borrow().as_ref() {
            bases.push(engine.base_url().to_owned());
        }
        bases.into_iter().collect()
    };
    for base in bases {
        let engine = ctx
            .torrent_scans
            .borrow()
            .values()
            .filter_map(|result| result.as_ref().ok())
            .map(|scan| scan.engine.clone())
            .find(|engine| engine.base_url() == base)
            .or_else(|| ctx.torrent_engine.borrow().clone())
            .filter(|engine| engine.base_url() == base);
        let Some(engine) = engine else { continue };
        let empty = crate::core::torrent::list_torrents(&engine)
            .map(|ids| ids.is_empty())
            .unwrap_or(false);
        if !empty {
            continue;
        }
        log::debug!(base:?; "Pruning an idle TUI torrent engine");
        ctx.torrent_scans.borrow_mut().retain(|_, result| {
            !(result.as_ref().is_ok_and(|scan| scan.engine.base_url() == base))
        });
        if ctx
            .torrent_engine
            .borrow()
            .as_ref()
            .is_some_and(|engine| engine.base_url() == base)
        {
            *ctx.torrent_engine.borrow_mut() = None;
        }
    }
}
/// Queue a `s2udio-downloads` download of a ytdlp stream (yt-dlp needs the
/// original link, not the resolved stream URL). `audio_only` extracts the
/// audio (`-x`), `split_chapters` saves each chapter as its own file named
/// after the chapter title; `replace` is what the downloaded file(s)
/// should take the place of once complete.
pub fn queue_stream_download(
    ctx: &Ctx,
    original_url: &str,
    audio_only: bool,
    split_chapters: bool,
    replace: ReplaceAction,
) {
    queue_stream_download_with(
        ctx,
        original_url,
        audio_only,
        split_chapters,
        replace,
        Vec::new(),
    );
}
/// Queue the download of selected chapter ranges of a ytdlp stream
/// (round 78): one file per range, named after the chapter (the yt-dlp
/// plumbing lives in `YtDlp::download_stream`).
pub fn queue_stream_download_sections(
    ctx: &Ctx,
    original_url: &str,
    audio_only: bool,
    sections: Vec<ChapterSection>,
) {
    if sections.is_empty() {
        status_warn!("No chapters selected");
        return;
    }
    queue_stream_download_with(
        ctx,
        original_url,
        audio_only,
        false,
        ReplaceAction::None,
        sections,
    );
}
/// The yt-dlp item of a downloadable link, or `None` when the link is not one
/// yt-dlp can save: a playlist-only link (nothing to save on its own) or a
/// non-YouTube/Soundcloud link. Round 79: a playlist link that names a video
/// (`watch?v=ID&list=…`) downloads THAT video — the item gets the bare watch
/// id, not the raw link (which would save the whole playlist into one file).
fn ytdlp_item_of(original_url: &str) -> Option<YtDlpItem> {
    match original_url.parse::<YtDlpContent>() {
        Ok(YtDlpContent::Single(item)) => Some(item),
        Ok(YtDlpContent::Playlist(_)) => yt_video_id(original_url).map(|id| YtDlpItem {
            filename: id.clone(),
            id,
            kind: YtDlpHost::Youtube,
            title: None,
            ..Default::default()
        }),
        Err(_) => None,
    }
}
/// The common body of the stream-download requests: yt-dlp needs the original
/// link, not the resolved stream URL. `sections` is empty for a whole-media
/// download.
fn queue_stream_download_with(
    ctx: &Ctx,
    original_url: &str,
    audio_only: bool,
    split_chapters: bool,
    replace: ReplaceAction,
    sections: Vec<ChapterSection>,
) {
    use crate::shared::ytdlp::StreamDownloadSpec;
    let Some(output_dir) = downloads_dir() else {
        status_warn!("Cannot determine the downloads folder (~/Downloads)");
        return;
    };
    let Some(item) = ytdlp_item_of(original_url) else {
        status_warn!(
            "Cannot download '{original_url}': not a YouTube/Soundcloud/NicoVideo link \
             (a playlist-only link needs 'Download > All files')"
        );
        return;
    };
    let chapter_count = sections.len();
    let spec = StreamDownloadSpec {
        output_dir,
        audio_only,
        split_chapters,
        on_complete: replace,
        sections,
    };
    ctx.ytdlp_manager.queue_stream_download(item, spec);
    if chapter_count > 0 {
        status_info!(
            "Downloading {chapter_count} chapter(s) of '{original_url}' to s2udio-downloads"
        );
    } else {
        status_info!("Downloading '{}' to s2udio-downloads", original_url);
    }
}

/// One downloadable row of the queue/playlist menus (round 97): the link
/// yt-dlp saves, the label the picker shows and what the file replaces.
///
/// The stream does **not** have to be resolved yet. A playlist import queues
/// intent-tagged links (`watch?v=ID#s2u-audio`) that only resolve when they
/// are played, so their row has no `yt_info` entry; the link itself is what
/// yt-dlp needs, and the listing metadata (`yt_list_meta`) carries the title.
#[derive(Debug, Clone)]
pub struct StreamDownloadTarget {
    /// The original link, intent tag stripped: what `yt-dlp` is run on.
    pub original_url: String,
    /// The row label for the picker (the cached/resolved title, else the
    /// listing title, else the link).
    pub label: String,
    /// The cached stream info when the row was already resolved (it carries
    /// the chapters, so the single-row menu can offer the chapter options).
    pub info: Option<YtStreamInfo>,
    /// What the downloaded file replaces once it lands.
    pub replace: ReplaceAction,
}

impl StreamDownloadTarget {
    /// The stream info the single-row download menu is built from: the cached
    /// one, or a link + label pair for a row that was not resolved yet (no
    /// chapters, so only `Save as audio` / `Save as video`).
    pub fn info(&self) -> YtStreamInfo {
        self.info.clone().unwrap_or_else(|| YtStreamInfo {
            url: self.original_url.clone(),
            original_url: self.original_url.clone(),
            title: self.label.clone(),
            ..Default::default()
        })
    }
}

/// The download target of a queue row or video-playlist entry, or `None` when
/// yt-dlp cannot save it (a local file, a radio/icecast URL, an unknown host).
/// `fallback_label` is used when neither the resolve cache nor the playlist
/// listing knows a title.
pub fn stream_download_target(
    ctx: &Ctx,
    uri: &str,
    fallback_label: &str,
    replace: ReplaceAction,
) -> Option<StreamDownloadTarget> {
    let (original_url, info, title) = match stream_info_for(ctx, uri).or_else(|| {
        // A tagged row that was resolved under its untagged link (a stored
        // playlist entry, or a queue row an intent tag never left): the
        // resolved info still carries the chapters the menu wants.
        let link = crate::shared::ytdlp::untag_stream_link(uri);
        (link != uri).then(|| stream_info_for(ctx, link)).flatten()
    }) {
        Some(info) => (info.original_url.clone(), Some(info.clone()), info.title),
        None => {
            // Not resolved (yet): a playlist import's tagged link, or a plain
            // link that was never played. Strip the intent tag and check that
            // yt-dlp knows the host before offering the row.
            let link = crate::shared::ytdlp::untag_stream_link(uri);
            if link == uri && !crate::ui::panes::radio::is_stream_url(uri) {
                return None;
            }
            ytdlp_item_of(link)?;
            let title = stream_row_meta(ctx, uri).map(|meta| meta.title).unwrap_or_default();
            (link.to_owned(), None, title)
        }
    };
    let label = if !title.trim().is_empty() {
        title
    } else if !fallback_label.trim().is_empty() {
        fallback_label.to_owned()
    } else {
        original_url.clone()
    };
    Some(StreamDownloadTarget {
        original_url,
        label,
        info,
        replace,
    })
}

/// Queue the downloads of several web streams at once (the queue's
/// multi-selection picker): one file per link, in the order they were listed.
/// Each `ReplaceAction` names what that link's file replaces when it lands
/// (the marked queue rows), and `audio_only` comes from the picker's
/// `Audio` / `Video` button. Links yt-dlp cannot save are skipped and
/// reported once.
pub fn queue_stream_downloads(
    ctx: &Ctx,
    targets: Vec<(String, ReplaceAction)>,
    audio_only: bool,
) {
    use crate::shared::ytdlp::StreamDownloadSpec;
    let Some(output_dir) = downloads_dir() else {
        status_warn!("Cannot determine the downloads folder (~/Downloads)");
        return;
    };
    let asked = targets.len();
    let mut queued = 0usize;
    for (original_url, replace) in targets {
        let Some(item) = ytdlp_item_of(&original_url) else {
            log::warn!(url:% = original_url; "Skipping a link yt-dlp cannot download");
            continue;
        };
        ctx.ytdlp_manager.queue_stream_download(
            item,
            StreamDownloadSpec {
                output_dir: output_dir.clone(),
                audio_only,
                split_chapters: false,
                on_complete: replace,
                sections: Vec::new(),
            },
        );
        queued += 1;
    }
    if queued < asked {
        status_warn!("{} of {asked} link(s) cannot be downloaded", asked - queued);
    }
    if queued > 0 {
        status_info!(
            "Downloading {queued} stream(s) to s2udio-downloads as {}",
            if audio_only { "audio" } else { "video" }
        );
    }
}

/// The multi-stream download picker (the queue's Download row when every
/// selected row is a web stream): one checkbox row per stream — all ticked,
/// so the selection is confirmed as-is — and one `Enter` starts the
/// download. `Up`/`Down` (or `w`/`s`) walk the rows and `Space` ticks them;
/// the output kind is the footer button that is activated (`Audio` = one
/// audio file per stream, `Video` = one video file), `Right`/`d` moves along
/// the buttons and `Esc` closes the picker. Each entry carries the queue row
/// its file replaces.
pub fn open_stream_downloads_picker(ctx: &Ctx, streams: Vec<StreamDownloadTarget>) {
    if streams.is_empty() {
        status_warn!("No streams selected");
        return;
    }
    let count = streams.len();
    modal!(
        ctx,
        MenuModal::new(ctx)
            .width(64)
            .title(format!(" Download {count} stream(s) "))
            .list_section(ctx, move |mut section| {
                for stream in &streams {
                    section.add_check_item(stream.label.clone(), true);
                }
                section.add_choice_buttons(&["Audio", "Video"], "Cancel");
                section.set_confirm_on_enter();
                section.check_list(move |ctx, selected, choice| {
                    let targets: Vec<(String, ReplaceAction)> = selected
                        .iter()
                        .filter_map(|idx| streams.get(*idx))
                        .map(|stream| {
                            (stream.original_url.clone(), stream.replace.clone())
                        })
                        .collect();
                    queue_stream_downloads(ctx, targets, choice == 0);
                    Ok(())
                });
                Some(section)
            })
            // `build()` places the cursor on the first row (`modal!` does not
            // call it), so the picker opens ready to walk and tick.
            .build()
    );
}

/// The save-as menu for a ytdlp stream: audio or video, and — when the
/// media has chapters — one file with chapters or each chapter as its own
/// file. `replace` is what the downloaded file(s) replace in the
/// queue/playlist (the controls' Download button passes
/// `ReplaceAction::None`).
pub fn open_stream_download_menu(
    ctx: &Ctx,
    info: &crate::shared::ytdlp::YtStreamInfo,
    replace: &crate::shared::ytdlp::ReplaceAction,
) {
    let has_chapters = info.chapters.len() > 1;
    let original = info.original_url.clone();
    let menu = MenuModal::new(ctx)
        .width(46)
        .title(" Download ")
        .list_section(
            ctx,
            |mut section| {
                section = section
                    .item(
                        "Save as audio",
                        {
                            let original = original.clone();
                            let replace = replace.clone();
                            move |ctx| {
                                queue_stream_download(ctx, &original, true, false, replace);
                                Ok(())
                            }
                        },
                    );
                section = section
                    .item(
                        "Save as video",
                        {
                            let original = original.clone();
                            let replace = replace.clone();
                            move |ctx| {
                                queue_stream_download(
                                    ctx,
                                    &original,
                                    false,
                                    false,
                                    replace,
                                );
                                Ok(())
                            }
                        },
                    );
                if has_chapters {
                    section = section
                        .item(
                            "Audio — each chapter its own file",
                            {
                                let original = original.clone();
                                let replace = replace.clone();
                                move |ctx| {
                                    queue_stream_download(ctx, &original, true, true, replace);
                                    Ok(())
                                }
                            },
                        );
                    section = section
                        .item(
                            "Video — each chapter its own file",
                            {
                                let original = original.clone();
                                let replace = replace.clone();
                                move |ctx| {
                                    queue_stream_download(ctx, &original, false, true, replace);
                                    Ok(())
                                }
                            },
                        );
                }
                Some(section)
            },
        )
        .list_section(ctx, |section| Some(section.item("Cancel", |_ctx| Ok(()))))
        .build();
    modal!(ctx, menu);
}

use crate::{
    ctx::Ctx,
    mpd::commands::Song,
    shared::{ext::error::ErrorExt, macros::status_error},
    ui::{Ui, UiEvent},
};

pub(super) fn handle_queue_update(
    ctx: &mut Ctx,
    ui: &mut Ui,
    queue: Option<Vec<Song>>,
    render_wanted: &mut bool,
) {
    ctx.queue = queue.unwrap_or_default();
    ctx.cached_queue_time_total = ctx.queue.iter().filter_map(|s| s.duration).sum();
    *render_wanted = true;
    log::debug!(len = ctx.queue.len(); "Queue updated");
    // Round 105: a cleared queue wipes the in-flight audio-row markers. A
    // resolve still on its way for a row that just vanished must not be
    // re-added later (the ReplaceAndPlay guard drops it anyway), and a fresh
    // import of the same link must not trip a stale one-per-entry guard or
    // keep a stale Duration spinner. Video (mpv) resolves carry separate
    // actions and are untouched.
    if ctx.queue.is_empty() {
        crate::ui::modals::paste::clear_queue_resolve_markers(ctx);
    }
    if let Err(err) = ui.on_event(UiEvent::QueueChanged, ctx) {
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
        crate::ui::modals::paste::ensure_chapters(ctx);
        crate::ui::modals::paste::ensure_mpris_metadata(ctx);
        // Round 95b (restored 105e): same look-ahead as the
        // status handler's song change (a ReplaceAndPlay
        // changes the song id, so this path is the one that
        // sees the new current entry).
        crate::ui::modals::paste::warm_next_queue_link(ctx);
        ctx.auto_show_chapters();
        // The album-art box belongs to this fan-out
        // as well: a pasted YouTube stream showed no
        // art until an unrelated tab switch, while
        // MPRIS (restored just above) had it — the
        // skipped SongChanged had also skipped the
        // pane's `before_show`, and that leaves the
        // box collapsed.
        if let Err(err) = ui.rearm_album_art(ctx) {
            log::error!(error:? = err; "Failed to re-arm the album art after the queue refresh");
        }
    }
    // Round 98 (restored 105e): the queue is fresh now, so
    // the look ahead can run for the current song even when
    // the song-change path ran against the previous queue
    // (a ReplaceAndPlay changes the song id before the
    // refreshed queue arrives). The helper is cheap and
    // runs at most once per current song.
    crate::ui::modals::paste::warm_next_queue_link(ctx);
}

use crate::{
    ctx::Ctx,
    mpd::{commands::Song, mpd_client::MpdClient},
    shared::{macros::status_warn, mpd_client_ext::MpdClientExt},
};

/// Whether `file` is a resolved YouTube stream whose signed URL has
/// expired. googlevideo `videoplayback` URLs carry an `expire` epoch; once
/// it passes, MPD cannot open the stream (the YouTube video itself may
/// still exist — only the signed link died).
pub(crate) fn resolved_stream_expired(file: &str) -> bool {
    if !file.contains("googlevideo.com") || !file.contains("videoplayback") {
        return false;
    }
    let Some(expire) = file
        .split("expire=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .and_then(|value| value.parse::<i64>().ok())
    else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    expire < now
}

/// Play a queue song. Round 91: a pasted stream **link**
/// (`watch?v=ID#s2u-audio`) is queued as the link and has no stream URL yet,
/// so playing it resolves the link and replaces the entry in place - the
/// paste itself never waits for yt-dlp. A resolved YouTube stream whose
/// signed URL expired cannot be played as-is either: when the cached info
/// still knows its original link, the link is re-resolved and the dead entry
/// replaced in place ([`YtAction::ReplaceAndPlay`]); otherwise the failure is
/// explained instead of failing silently in MPD.
pub(super) fn play_queue_song(song: &Song, ctx: &Ctx) {
    // Round 91: the entry holds the link, not a stream (the only resolution
    // request for it - the guard inside the helper absorbs a second one from
    // the event loop's MPD-driven path).
    if crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some() {
        crate::ui::modals::paste::resolve_tagged_queue_entry(ctx, &song.file, song.id);
        return;
    }
    // Round 105/107: a **stale resolved row** — an expired googlevideo URL,
    // or a buffered proxy URL the proxy cannot serve (no live upstream
    // registration after a restart, or a failed fill) — must be re-resolved
    // before MPD is asked to play it. MPD fails a dead proxy URL with HTTP
    // 404 and skips on, which reads as "it kept moving to the next item".
    if crate::ui::modals::paste::expired_stream_original_url(ctx, &song.file).is_some() {
        // `false` means a resolve for this entry is already in flight; its
        // replacement plays when it lands, so do not hand MPD the row either.
        crate::ui::modals::paste::re_resolve_expired_queue_entry(ctx, &song.file, song.id, true);
        return;
    }
    if resolved_stream_expired(&song.file) {
        status_warn!("This queue entry's stream URL expired; add the original link again");
    }
    let id = song.id;
    ctx.command(move |client| {
        client.play_id(id)?;
        Ok(())
    });
}

/// Round 104: start playback from a stopped queue without ever handing MPD a
/// raw stream link. MPD's plain `play` starts at the current song (or the
/// first queue entry); for a queue of pasted YouTube links that row is an
/// unresolvable `#s2u-audio` link, MPD fails to open it and advances through
/// the following links faster than the app's per-error recovery can resolve
/// them (four links rejected in two seconds in the user's MPD log, each
/// followed by a `No such song` as the recovery raced a row MPD had already
/// abandoned). Double-clicking a row already resolves it first
/// ([`play_queue_song`]); starting playback now does the same, so the two
/// paths behave consistently. Only a stream link is intercepted - a plain
/// queue keeps MPD's exact `play` semantics (random, consume).
pub(crate) fn play_from_stop(ctx: &Ctx) {
    // The entry MPD's `play` would start at: the current song when one is
    // known (a stopped queue resumes it), else the first queue row.
    let song = ctx
        .status
        .songid
        .and_then(|id| ctx.queue.iter().find(|song| song.id == id))
        .or_else(|| ctx.queue.first())
        .cloned();
    let Some(song) = song else {
        ctx.command(move |client| {
            client.play()?;
            Ok(())
        });
        return;
    };
    let needs_resolve = crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some()
        || crate::ui::modals::paste::expired_stream_original_url(ctx, &song.file).is_some();
    if needs_resolve {
        play_queue_song(&song, ctx);
    } else {
        ctx.command(move |client| {
            client.play()?;
            Ok(())
        });
    }
}

/// Round 112, extended 2026-10-11: the row MPD's plain `next` would land on.
///
/// **MPD is asked, not predicted.** Its `status` carries `nextsong` /
/// `nextsongid` — "playlist song number/songid of the next song to be played"
/// — which the app already parses (`mpd::commands::Status`) and previously
/// never used. That field is authoritative, and it is what makes `random`
/// mode work: MPD keeps its own shuffled sequence, so the app cannot compute
/// the row, but MPD will state it. Using it also removes the guesswork from
/// `repeat` / `single` / `consume` / queue priorities, which MPD folds into
/// the same answer.
///
/// The computed next index is only a **fallback**, for the cases where MPD
/// reports no `nextsong` (a stopped player, or the last row with `repeat`
/// off, where `next` stops instead of moving).
///
/// **2026-10-15: the answer must be re-read, not remembered.** `▶▶` resolves
/// its target before issuing `next`, and that resolve costs ~1.3 s. During
/// that window MPD keeps running its own shuffle, so the row it will play has
/// usually changed by the time `next` is issued — and `next` then lands on a
/// *different* row than the one the app resolved, still an unresolved
/// `#s2u-audio` link. Measured live (412-row random queue): the step resolved
/// song 34, and the `next` 1.28 s later failed to decode a different row
/// entirely (`Mp5CJTrX_tw`), which started the very resolve cascade the step
/// was written to prevent — three yt-dlp resolves in 1.4 s and 5.1 s until
/// playback settled, with the selection hopping down the queue as it went.
///
/// MPD re-derives `nextsongid` when the row it had chosen is replaced in
/// place (verified against MPD 0.24: `deleteid` + `addid` at the same position
/// moves `nextsongid` onto the replacement), so a **fresh** read at step time
/// is authoritative and no app-side shuffle prediction is needed.
fn next_target(ctx: &Ctx) -> Option<Song> {
    // MPD's own answer first. The id is stable across queue edits, and the
    // row is looked up by it so a consumed/removed entry simply misses.
    if let Some(id) = ctx.status.nextsongid
        && let Some(song) = ctx.queue.iter().find(|song| song.id == id)
    {
        return Some(song.clone());
    }
    let current = ctx.status.song?;
    let last = ctx.queue.len().checked_sub(1)?;
    if current >= last {
        if !ctx.status.repeat {
            // `next` past the end stops MPD; there is no row to resolve.
            return None;
        }
        return ctx.queue.first().cloned();
    }
    ctx.queue.get(current + 1).cloned()
}

/// Whether playing this row needs a resolve before MPD may be handed it: an
/// unresolved stream link, or a resolved row whose signed URL has expired
/// (MPD fails both and skips on).
fn row_needs_resolve(ctx: &Ctx, song: &Song) -> bool {
    crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some()
        || crate::ui::modals::paste::expired_stream_original_url(ctx, &song.file).is_some()
}

/// Round 112: `▶▶` (the controls bar) and `>` (`GlobalAction::NextTrack`) step
/// to the next queue entry **without handing MPD a raw stream link**.
///
/// Asking MPD to step was the bug: it lands on the next entry, which for a
/// queue of pasted YouTube links is an unresolved `#s2u-audio` link, fails to
/// decode it (`avformat_open_input() failed: Invalid data found`) and skips on,
/// while the app separately resolved whatever row MPD had reached. The
/// resolves ran per row and completed out of order, so the highlight ran down
/// the list and playback landed on whichever `yt-dlp` call finished first —
/// not the row the user stepped to (reported live 2026-10-11, and the MPD log
/// shows the failed `{next}`).
///
/// The step now mirrors the double-click path (and round 104's
/// [`play_from_stop`]): the target row — **the one MPD reports it will play
/// next** (`nextsongid`), so `random` mode is covered too — is resolved first
/// and `next` is issued once it is playable. A target that is already playable
/// keeps MPD's exact `next` semantics.
///
/// **2026-10-15 (follow-up): the step re-reads its target when the resolve
/// lands.** Remembering the press-time pick (`pending_next_step`) is not
/// enough: the resolve takes ~1.3 s, and in `random` mode MPD re-picks its
/// next song inside that window, so the remembered row is routinely stale by
/// the time `next` is issued. [`issue_next_if_still_target`] re-reads
/// `nextsongid` and only steps when MPD still agrees; otherwise it re-targets
/// the row MPD now reports, which is the row `next` will actually land on.
pub(crate) fn next_track_resolving(ctx: &Ctx) {
    let target = next_target(ctx);
    let needs_resolve = target.as_ref().is_some_and(|song| row_needs_resolve(ctx, song));
    if !needs_resolve {
        // Nothing to resolve ahead of the step: MPD's own `next` is correct.
        ctx.pending_next_step.set(None);
        issue_next(ctx);
        return;
    }
    let Some(song) = target else { return };
    // Coalesce: a step already waiting on this same row is not restarted (its
    // resolve is in flight), and a step to a *different* row supersedes it —
    // the newest press wins, so N rapid presses advance at most N rows in
    // order instead of racing N resolves.
    if ctx.pending_next_step.get() == Some(song.id) {
        log::debug!(song_id = song.id; "A next-track step for this row is already resolving");
        return;
    }
    ctx.pending_next_step.set(Some(song.id));
    log::debug!(
        song_id = song.id, file = song.file.as_str();
        "Resolving the next queue row before stepping to it"
    );
    // `false` = replace in place, do not play: the `next` issued once it lands
    // is what starts it, so MPD's own stepping semantics stay in charge.
    //
    // An unresolved tagged link and an expired resolved row take different
    // helpers (the second re-resolves from its remembered original link); both
    // end in the same `ReplaceInPlace` arm, which fires the pending step.
    let pending = if crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some() {
        crate::ui::modals::paste::resolve_tagged_queue_entry_in_place(ctx, &song.file, song.id)
    } else {
        crate::ui::modals::paste::re_resolve_expired_queue_entry(ctx, &song.file, song.id, false)
    };
    if !pending {
        // The resolve was declined (already in flight, or the row stopped
        // being resoluble). Do not leave the step armed forever: fall through
        // to MPD, whose error recovery is the remaining net.
        ctx.pending_next_step.set(None);
        log::debug!(song_id = song.id; "The step's row needs no resolve after all; stepping");
        issue_next(ctx);
    }
}

/// The step's target has finished resolving: issue `next` **only if MPD still
/// intends to play the row that was resolved**.
///
/// The resolve took time, and `random` mode gives MPD a fresh pick in that
/// window; stepping blindly then lands on whatever MPD has moved on to — an
/// unresolved link, which is exactly the decode failure and resolve cascade
/// this path exists to avoid. So the target is re-read here, at the last
/// moment before the step:
///
/// - MPD still reports the row that was just resolved → step (the common case);
/// - MPD reports a **different** row → re-target it: resolve *that* row and
///   step once it is playable, so the resolve and the step cannot diverge;
/// - MPD reports no next row any more (the queue end, `repeat` off) → step
///   anyway and let MPD stop, which is its own semantics.
///
/// `song_id` is the row whose resolve just landed.
pub(crate) fn issue_next_if_still_target(ctx: &Ctx, song_id: u32) {
    match next_target(ctx) {
        Some(song) if song.id == song_id => {
            ctx.pending_next_step.set(None);
            log::debug!(song_id; "The next-track step's row is playable; stepping now");
            issue_next(ctx);
        }
        Some(song) => {
            // MPD re-picked while the resolve was in flight. Supersede the
            // stale step with one aimed at the row MPD now reports. The marker
            // is cleared first so this is not coalesced away as a repeat of
            // the step being replaced.
            log::debug!(
                resolved = song_id, target = song.id;
                "MPD re-picked the next song while the step resolved; re-targeting it"
            );
            ctx.pending_next_step.set(None);
            next_track_resolving(ctx);
        }
        None => {
            ctx.pending_next_step.set(None);
            log::debug!(song_id; "MPD reports no next song; stepping to let it stop");
            issue_next(ctx);
        }
    }
}

/// Issue MPD's own `next`, keeping the play/pause state as the user set it.
///
/// Round 112: `pub(super)`-and-beyond so the resolve handler can fire the
/// step once the row it was waiting on is playable.
pub(crate) fn issue_next(ctx: &Ctx) {
    let keep_state = ctx.config.keep_state_on_song_change;
    let state = ctx.status.state;
    ctx.command(move |client| {
        client.next_keep_state(keep_state, state)?;
        Ok(())
    });
}

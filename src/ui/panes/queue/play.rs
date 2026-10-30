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

/// Stop MPD so it cannot advance past a row that is still being resolved.
///
/// A play request on an unresolved link is a **wait**, not a skip: MPD would
/// fail the row and move on, and the app's recovery would then resolve wherever
/// it landed. Holding the player gives the resolve time to land, and
/// `ReplaceAndPlay` starts the row by id afterwards. Already-stopped MPD is
/// left alone (nothing to hold); a playing or paused player is stopped, which
/// is what makes the row the user chose the one that plays.
pub(crate) fn hold_mpd_while_resolving(ctx: &Ctx, song_id: u32) {
    if ctx.status.state == crate::mpd::commands::State::Stop {
        return;
    }
    log::debug!(song_id; "Holding MPD while the requested row resolves");
    ctx.command(move |client| {
        client.stop()?;
        Ok(())
    });
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
        // **Hold MPD while the row resolves (2026-10-15).** MPD cannot open an
        // unresolved `#s2u-audio` link, so a running player fails the row and
        // advances — and the status-driven recovery then resolves whichever row
        // MPD landed on, chasing the queue instead of playing the row the user
        // chose ("it skips through multiple entries"). Stopping MPD first means
        // it has nowhere to skip to; `ReplaceAndPlay` starts the resolved row by
        // id when the stream lands, so playback begins exactly once, on the
        // right row.
        hold_mpd_while_resolving(ctx, song.id);
        // A user play request (2026-10-25): a newer one supersedes it, so a
        // slow resolve cannot play a row the user has already left.
        crate::ui::modals::paste::play_tagged_queue_entry(ctx, &song.file, song.id);
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
        crate::ui::modals::paste::play_expired_queue_entry(ctx, &song.file, song.id);
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
/// and started once it is playable. A target that is already playable keeps
/// MPD's exact `next` semantics.
///
/// **2026-10-15 (second report): the resolve must not rely on `next`.**
/// `ReplaceInPlace` rewrites the row with `delete_id` + `add_id`, and in
/// `random` mode **deleting the row MPD had picked makes MPD discard that
/// pick and choose a different row** — verified against MPD 0.24 with real
/// playable media, four trials per mode:
///
/// | swap strategy            | sequential | random |
/// |--------------------------|-----------|--------|
/// | `delete_id` + `add_id`   | 4/4       | **0/4** |
/// | `add_id` + `moveid`      | 4/4       | **0/4** |
/// | `add_id` + `play_id`     | 4/4       | **4/4** |
///
/// So the swap itself moved MPD's next song off the row the app had resolved:
/// the step then landed on a *different*, unresolved link, MPD failed to
/// decode it, and the per-row recovery cascade ran — the second storm the
/// user reported. The step's own guard passed correctly (it
/// had resolved the right row); the divergence came from the swap, not the
/// step. Relying on `next` therefore cannot work: the resolved row must be
/// started **by id**.
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
    // The target is the row MPD says it will play next. If that row is already
    // the one playing, MPD has not caught up with the step the user just made
    // (its `nextsongid` still points at the row that started), and re-resolving
    // it would restart it. Nothing to do.
    if ctx.status.songid == Some(song.id) {
        ctx.pending_next_step.set(None);
        log::debug!(
            song_id = song.id;
            "The step's target is already the playing row; nothing to step to"
        );
        return;
    }
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
    // `play = true`: the resolve **starts** the row it was asked for. The step
    // cannot use MPD's `next` — see this function's docs: the in-place swap
    // moves MPD's own next-song pick in `random` mode, so `next` would land
    // somewhere else. Starting the resolved row by id is the only form that
    // holds in both modes (and is what the double-click path already does).
    //
    // An unresolved tagged link and an expired resolved row take different
    // helpers (the second re-resolves from its remembered original link); both
    // end in the same `ReplaceAndPlay` arm.
    let pending = if crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some() {
        crate::ui::modals::paste::play_tagged_queue_entry(ctx, &song.file, song.id)
    } else {
        crate::ui::modals::paste::play_expired_queue_entry(ctx, &song.file, song.id)
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

#[cfg(test)]
mod play_from_stop_tests {
    use super::{play_from_stop, play_queue_song};
    use crate::mpd::commands::{Song, State};
    use crate::shared::events::{ClientRequest, WorkRequest};
    use crate::ui::modals::paste::YtAction;

    fn rig(
        queue: Vec<Song>,
    ) -> (
        crate::ctx::Ctx,
        crossbeam::channel::Receiver<ClientRequest>,
        crossbeam::channel::Receiver<WorkRequest>,
    ) {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        // Keep clones of the receivers: the fixture takes the originals and
        // the code under test sends on all three channels.
        let client_rx = client.1.clone();
        let work_rx = work.1.clone();
        let mut ctx =
            crate::tests::fixtures::ctx((app.0, app.1), (work.0, work.1), (client.0, client.1));
        ctx.queue = queue;
        (ctx, client_rx, work_rx)
    }

    fn row(id: u32, file: impl Into<String>) -> Song {
        Song { id, file: file.into(), ..Default::default() }
    }

    const FIRST: &str = "https://www.youtube.com/watch?v=gxE4ngu78Ro";
    const SECOND: &str = "https://www.youtube.com/watch?v=L4O2S8R1QBU";
    const THIRD: &str = "https://www.youtube.com/watch?v=Z9QyW1kPm3A";

    fn asks_resolve(
        work_rx: &crossbeam::channel::Receiver<WorkRequest>,
        url: &str,
        id: u32,
    ) -> bool {
        work_rx.try_iter().any(|request| {
            matches!(
                request,
                WorkRequest::ResolveYtStreams { urls, action, .. }
                    if urls == vec![url.to_owned()]
                        && matches!(action, YtAction::ReplaceAndPlay(found) if found == id)
            )
        })
    }

    /// **A play request on an unresolved row must hold MPD still until the
    /// stream is ready.**
    ///
    /// Reported: pressing play on a freshly added playlist of web streams
    /// "skips through multiple" entries instead of playing the one asked for.
    /// MPD is already playing (or is started) and cannot open an unresolved
    /// `#s2u-audio` link, so it fails the row and advances — and the app's
    /// status-driven recovery then resolves whichever row MPD landed on next.
    /// The result is a chase down the queue rather than playback of the row the
    /// user chose.
    ///
    /// The user's requirement: resolve the row **first**, keep MPD where it is,
    /// and start that row once its stream exists.
    #[test]
    fn a_play_request_holds_mpd_while_the_row_resolves() {
        let (mut ctx, client_rx, work_rx) = rig(vec![
            row(4, format!("{FIRST}#s2u-audio")),
            row(5, format!("{SECOND}#s2u-audio")),
        ]);
        // MPD is playing: it is free to fail the unresolved row and skip on.
        ctx.status.state = State::Play;
        ctx.status.songid = Some(4);
        play_queue_song(&row(4, format!("{FIRST}#s2u-audio")), &ctx);
        assert!(asks_resolve(&work_rx, FIRST, 4), "the chosen row is resolved");

        // MPD must be held. The command carries a closure that a test cannot
        // read (the same limitation `2026-10-15-3.md` records for the swap
        // commands), so this pins the two things that *are* observable: a hold
        // command was queued, and it was queued **before** the resolve — i.e.
        // MPD is stopped rather than left free to fail forward through the
        // links while yt-dlp runs.
        let mut holds = 0;
        let mut resolve_index = None;
        for (i, r) in client_rx.try_iter().enumerate() {
            if matches!(r, crate::shared::events::ClientRequest::Command(_)) {
                holds += 1;
                resolve_index.get_or_insert(i);
            }
        }
        assert_eq!(
            holds, 1,
            "exactly one command is queued on the play path: the hold (the \
             resolve itself goes to the work channel, not the client)"
        );
        assert!(
            resolve_index.is_some(),
            "the hold is issued when the play request arrives, not after the \
             stream lands — otherwise MPD has already skipped onward"
        );
    }

    /// A stopped queue whose first row is a stream link starts by resolving
    /// that row (ReplaceAndPlay) instead of handing MPD the link - the
    /// Space-start path matches the double-click path.
    #[test]
    fn stopped_queue_resolves_the_first_link() {
        let (mut ctx, _client_rx, work_rx) =
            rig(vec![row(4, format!("{FIRST}#s2u-audio")), row(5, format!("{SECOND}#s2u-audio"))]);
        ctx.status.songid = None;
        play_from_stop(&ctx);
        assert!(
            asks_resolve(&work_rx, FIRST, 4),
            "the first queue link is resolved, not handed to MPD"
        );
    }

    /// A queue stopped on a later song resolves that song, not the head of
    /// the queue - MPD's `play` would resume the current song.
    #[test]
    fn stopped_queue_resolves_the_current_song() {
        let (mut ctx, _client_rx, work_rx) =
            rig(vec![row(4, format!("{FIRST}#s2u-audio")), row(5, format!("{SECOND}#s2u-audio"))]);
        ctx.status.songid = Some(5);
        play_from_stop(&ctx);
        assert!(asks_resolve(&work_rx, SECOND, 5), "the current (stopped) queue song is resolved");
    }

    /// A plain queue keeps MPD's own `play` semantics - no resolve is
    /// requested and a single play command goes to MPD.
    #[test]
    fn plain_queue_plays_without_resolving() {
        let (mut ctx, client_rx, work_rx) =
            rig(vec![row(4, "/music/a.flac"), row(5, "/music/b.flac")]);
        ctx.status.songid = None;
        play_from_stop(&ctx);
        assert_eq!(work_rx.try_iter().count(), 0, "no resolve for local files");
        assert_eq!(client_rx.try_iter().count(), 1, "MPD receives one play command");
    }

    /// An empty queue sends MPD's plain play (a no-op on MPD's side).
    #[test]
    fn empty_queue_plays_without_resolving() {
        let (mut ctx, client_rx, work_rx) = rig(Vec::new());
        ctx.status.songid = None;
        play_from_stop(&ctx);
        assert_eq!(work_rx.try_iter().count(), 0);
        assert_eq!(client_rx.try_iter().count(), 1);
    }

    /// Round 107: a buffered row the proxy cannot serve (no live upstream
    /// registration, e.g. after a restart) is re-resolved before MPD is handed
    /// the URL — a dead proxy URL fails with HTTP 404 and MPD skips on.
    #[test]
    fn a_buffered_row_the_proxy_cannot_serve_is_resolved_first() {
        use crate::core::stream_cache::{self, StreamClass};
        let dir = std::env::temp_dir();
        stream_cache::init(Some(dir.as_path()));
        let link = "https://www.youtube.com/watch?v=STALE-BUFFER-ROW";
        let proxy = stream_cache::proxy_url(
            Some(dir.as_path()),
            StreamClass::Audio,
            link,
            "https://rr1.googlevideo.com/videoplayback?expire=9999999999",
        );
        // The proxy registered the row, so it is served and not stale.
        let (ctx, _client_rx, _work_rx) = rig(vec![row(4, proxy.clone())]);
        assert!(
            crate::ui::modals::paste::expired_stream_original_url(&ctx, &proxy).is_none(),
            "a freshly registered buffered row is served by the proxy"
        );
        // A restart drops the registration; now the row is stale.
        stream_cache::forget_registration_for_test(&proxy);
        let (mut ctx, _client_rx, work_rx) = rig(vec![row(4, proxy.clone())]);
        ctx.status.songid = None;
        play_from_stop(&ctx);
        assert!(
            asks_resolve(&work_rx, link, 4),
            "the stale buffered row is re-resolved, not handed to MPD"
        );
    }

    // ── Round 112: `▶▶` / `>` step without handing MPD a link ────────────

    /// The step the row order says MPD would land on.
    #[test]
    fn the_step_target_is_the_next_queue_row() {
        let (mut ctx, _client_rx, _work_rx) =
            rig(vec![row(1, FIRST), row(2, SECOND), row(3, "/music/c.flac")]);
        ctx.status.song = Some(0);
        assert_eq!(super::next_target(&ctx).map(|s| s.id), Some(2));

        ctx.status.song = Some(1);
        assert_eq!(super::next_target(&ctx).map(|s| s.id), Some(3));
    }

    /// MPD's own `nextsongid` is the target when it reports one: the row the
    /// app resolves ahead of the step is exactly the row MPD will play.
    #[test]
    fn the_target_follows_mpds_reported_next_song() {
        let (mut ctx, _client_rx, _work_rx) =
            rig(vec![row(1, FIRST), row(2, SECOND), row(3, "/music/c.flac")]);
        ctx.status.song = Some(0);
        // MPD says it will play the third row (id 3), not the row after the
        // current one (id 2).
        ctx.status.nextsongid = Some(3);
        assert_eq!(super::next_target(&ctx).map(|s| s.id), Some(3));
    }

    /// `random` mode: MPD keeps its own shuffled sequence, so the app cannot
    /// compute the row - but MPD reports it, and that row is resolved.
    #[test]
    fn random_mode_resolves_the_row_mpd_reports() {
        let (mut ctx, client_rx, work_rx) = rig(vec![
            row(1, FIRST),
            row(2, "/music/b.flac"),
            row(3, format!("{SECOND}#s2u-audio")),
        ]);
        ctx.status.song = Some(0);
        ctx.status.state = State::Play;
        ctx.status.random = true;
        // The shuffled order picks the third row; the second is a plain file,
        // so a computed `current + 1` would have resolved nothing at all.
        ctx.status.nextsongid = Some(3);
        super::next_track_resolving(&ctx);

        assert!(
            work_rx.try_iter().any(|request| matches!(
                request,
                WorkRequest::ResolveYtStreams { urls, action, .. }
                    if urls == vec![SECOND.to_owned()]
                        && matches!(action, YtAction::ReplaceAndPlay(found) if found == 3)
            )),
            "the row MPD reported is the one resolved (and started by id)"
        );
        assert_eq!(
            client_rx.try_iter().count(),
            0,
            "MPD is not asked to step until that row is playable"
        );
    }

    /// With no `nextsongid` (a stopped player, or the end of the queue) the
    /// computed next index is still used.
    #[test]
    fn the_computed_index_is_the_fallback() {
        let (mut ctx, _client_rx, _work_rx) =
            rig(vec![row(1, FIRST), row(2, SECOND), row(3, "/music/c.flac")]);
        ctx.status.song = Some(1);
        ctx.status.nextsongid = None;
        assert_eq!(super::next_target(&ctx).map(|s| s.id), Some(3));
    }

    /// A `nextsongid` naming a row that is gone (a consumed or removed entry)
    /// falls back to the computed index rather than resolving nothing.
    #[test]
    fn a_missing_reported_row_falls_back_to_the_index() {
        let (mut ctx, _client_rx, _work_rx) = rig(vec![row(1, FIRST), row(2, SECOND)]);
        ctx.status.song = Some(0);
        ctx.status.nextsongid = Some(999);
        assert_eq!(super::next_target(&ctx).map(|s| s.id), Some(2));
    }

    /// The reported bug: stepping onto an unresolved link must resolve **that
    /// row** and not ask MPD to move yet, so playback lands on it rather than
    /// on whichever resolve finishes first.
    #[test]
    fn stepping_onto_an_unresolved_row_resolves_it_instead_of_asking_mpd() {
        let (mut ctx, client_rx, work_rx) =
            rig(vec![row(1, FIRST), row(2, format!("{SECOND}#s2u-audio"))]);
        ctx.status.song = Some(0);
        ctx.status.state = State::Play;
        super::next_track_resolving(&ctx);

        assert!(
            work_rx.try_iter().any(|request| matches!(
                request,
                WorkRequest::ResolveYtStreams { urls, action, .. }
                    if urls == vec![SECOND.to_owned()]
                        && matches!(action, YtAction::ReplaceAndPlay(found) if found == 2)
            )),
            "the row being stepped to is resolved AND started by id: \
             `next` cannot be used, because the in-place swap moves MPD's own \
             next-song pick in random mode (measured 0/4), while starting the \
             row by id lands correctly in both modes (4/4)"
        );
        assert_eq!(
            client_rx.try_iter().count(),
            0,
            "MPD is not asked to step until the target row is playable"
        );
        assert_eq!(ctx.pending_next_step.get(), Some(2), "the step waits on that row");
    }

    // ── 2026-10-15 (second report): the step must not rely on `next` ──

    /// **The reported bug.** In `random` mode the in-place swap moves MPD's
    /// own next-song pick off the row the app resolved (measured 0/4 against
    /// MPD 0.24), so issuing `next` lands on a *different*, unresolved link —
    /// the decode failure and resolve cascade. The step therefore starts the
    /// resolved row by id instead, which holds in both modes (4/4).
    fn assert_step_starts_the_resolved_row(random: bool) {
        let (mut ctx, client_rx, work_rx) = rig(vec![
            row(1, FIRST),
            row(2, format!("{SECOND}#s2u-audio")),
            row(3, format!("{THIRD}#s2u-audio")),
        ]);
        ctx.status.song = Some(0);
        ctx.status.state = State::Play;
        ctx.status.random = random;
        ctx.status.nextsongid = Some(2);
        super::next_track_resolving(&ctx);

        assert!(
            work_rx.try_iter().any(|r| matches!(
                r,
                WorkRequest::ResolveYtStreams { urls, action, .. }
                    if urls == vec![SECOND.to_owned()]
                        && matches!(action, YtAction::ReplaceAndPlay(found) if found == 2)
            )),
            "the step resolves AND starts row 2 by id"
        );
        assert_eq!(
            client_rx.try_iter().count(),
            0,
            "no `next` is issued: the swap would move MPD's pick"
        );
    }

    #[test]
    fn the_step_starts_the_resolved_row_in_random_mode() {
        assert_step_starts_the_resolved_row(true);
    }

    #[test]
    fn the_step_starts_the_resolved_row_sequentially_too() {
        assert_step_starts_the_resolved_row(false);
    }

    /// The streamed variant of the same rule: an **expired** resolved row is
    /// re-resolved and started by id, not stepped onto.
    #[test]
    fn an_expired_target_is_re_resolved_and_started_by_id() {
        const EXPIRED: &str =
            "https://rr2.googlevideo.com/videoplayback?expire=1000000000&id=stale";
        let (mut ctx, client_rx, work_rx) =
            rig(vec![row(1, FIRST), row(2, EXPIRED), row(3, "/music/c.flac")]);
        ctx.status.song = Some(0);
        ctx.status.state = State::Play;
        ctx.status.random = true;
        ctx.status.nextsongid = Some(2);
        ctx.yt_info.borrow_mut().insert(
            EXPIRED.to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                url: EXPIRED.to_owned(),
                original_url: "https://www.youtube.com/watch?v=STALE".to_owned(),
                title: "Stale".to_owned(),
                ..Default::default()
            },
        );
        super::next_track_resolving(&ctx);
        assert!(
            work_rx.try_iter().any(|r| matches!(
                r,
                WorkRequest::ResolveYtStreams { action, .. }
                    if matches!(action, YtAction::ReplaceAndPlay(found) if found == 2)
            )),
            "the expired target is re-resolved and played by id"
        );
        assert_eq!(client_rx.try_iter().count(), 0, "no `next` for a resolved target");
    }

    /// **The reported bug: "the first click of ▶▶ resolves but doesn't
    /// actually progress to the next track until the second press".**
    ///
    /// The step arms `pending_next_step`; the arm that performs the in-place
    /// swap must **start the replacement by its new id**, inside the same MPD
    /// command. It cannot be played from outside afterwards: `delete_id` +
    /// `add_id` gives the row a different id, so a `playid` issued later names
    /// an id that no longer exists (every such step failed with `No such
    /// song`, and the reported second press was the one that worked — by then
    /// the row needs no resolve and MPD's own `next` runs).
    ///
    /// The swap itself is exercised end to end by
    /// `paste::apply_resolved_streams`'s own tests; what this pins is that the
    /// step's marker survives until the swap runs.
    #[test]
    fn a_step_keeps_its_marker_until_the_swap_runs() {
        let (mut ctx, _client_rx, work_rx) =
            rig(vec![row(1, FIRST), row(2, format!("{SECOND}#s2u-audio"))]);
        ctx.status.song = Some(0);
        ctx.status.state = State::Play;
        ctx.status.nextsongid = Some(2);
        super::next_track_resolving(&ctx);
        assert_eq!(
            ctx.pending_next_step.get(),
            Some(2),
            "the marker stays armed so the swap knows to start the row"
        );
        assert!(
            work_rx.try_iter().any(|r| matches!(
                r,
                WorkRequest::ResolveYtStreams { action, .. }
                    if matches!(action, YtAction::ReplaceAndPlay(found) if found == 2)
            )),
            "and the resolve carries the play form"
        );
    }

    /// A press whose target is already the playing row does nothing: MPD has
    /// not caught up yet, and re-resolving would restart the current track.
    #[test]
    fn a_step_onto_the_already_playing_row_is_a_no_op() {
        let (mut ctx, client_rx, work_rx) =
            rig(vec![row(1, FIRST), row(2, format!("{SECOND}#s2u-audio"))]);
        ctx.status.state = State::Play;
        // The resolving row became current; MPD still reports it as next.
        ctx.status.songid = Some(2);
        ctx.status.song = Some(1);
        ctx.status.nextsongid = Some(2);
        super::next_track_resolving(&ctx);
        assert_eq!(
            work_rx.try_iter().count(),
            0,
            "the row is already playing, so it is not resolved again"
        );
        assert_eq!(client_rx.try_iter().count(), 0, "and nothing is sent to MPD");
        assert_eq!(ctx.pending_next_step.get(), None, "no step is left armed");
    }

    /// A target that is already playable keeps MPD's own `next`.
    #[test]
    fn stepping_onto_a_playable_row_asks_mpd_immediately() {
        let (mut ctx, client_rx, work_rx) = rig(vec![row(1, FIRST), row(2, "/music/b.flac")]);
        ctx.status.song = Some(0);
        ctx.status.state = State::Play;
        super::next_track_resolving(&ctx);
        assert_eq!(work_rx.try_iter().count(), 0, "a plain file needs no resolve");
        assert_eq!(client_rx.try_iter().count(), 1, "MPD steps immediately");
    }

    /// Repeated presses for the same target coalesce into one resolve.
    #[test]
    fn repeated_presses_for_the_same_row_do_not_pile_up_resolves() {
        let (mut ctx, _client_rx, work_rx) =
            rig(vec![row(1, FIRST), row(2, format!("{SECOND}#s2u-audio"))]);
        ctx.status.song = Some(0);
        ctx.status.state = State::Play;
        super::next_track_resolving(&ctx);
        let first = work_rx
            .try_iter()
            .filter(|r| matches!(r, WorkRequest::ResolveYtStreams { .. }))
            .count();
        assert_eq!(first, 1, "the first press resolves the target");
        super::next_track_resolving(&ctx);
        let second = work_rx
            .try_iter()
            .filter(|r| matches!(r, WorkRequest::ResolveYtStreams { .. }))
            .count();
        assert_eq!(second, 0, "a second press for the same row does not resolve again");
    }

    /// **The second reported storm, end to end.** A shuffled queue of
    /// unresolved links: pressing `▶▶` must resolve exactly ONE row and start
    /// it by id. The old path resolved the row, swapped it in place and issued
    /// `next`, which in random mode sent MPD to a *different* unresolved row —
    /// whose failure made the app resolve the row MPD then landed on, and so
    /// on (the log shows four `Resolving the stream…` lines in 2.2 s and the
    /// selection walking down the queue).
    #[test]
    fn a_step_in_a_shuffled_stream_queue_resolves_exactly_one_row() {
        let (mut ctx, client_rx, work_rx) = rig(vec![
            row(1, format!("{FIRST}#s2u-audio")),
            row(2, format!("{SECOND}#s2u-audio")),
            row(3, format!("{THIRD}#s2u-audio")),
            row(4, "/music/d.flac"),
        ]);
        ctx.status.song = Some(0);
        ctx.status.state = State::Play;
        ctx.status.random = true;
        ctx.status.nextsongid = Some(2);
        super::next_track_resolving(&ctx);

        let requests: Vec<_> = work_rx.try_iter().collect();
        let resolves = requests
            .iter()
            .filter(|r| matches!(r, WorkRequest::ResolveYtStreams { .. }))
            .count();
        assert_eq!(resolves, 1, "exactly one resolve is requested, not a cascade");
        assert!(
            requests.iter().any(|r| matches!(
                r,
                WorkRequest::ResolveYtStreams { action, .. }
                    if matches!(action, YtAction::ReplaceAndPlay(found) if *found == 2)
            )),
            "and it is the row the step targets, started by id"
        );
        assert_eq!(
            client_rx.try_iter().count(),
            0,
            "no `next` is handed to MPD, so nothing can land on an unresolved row"
        );
    }
}

use crate::{
    ctx::Ctx,
    mpd::{commands::Song, mpd_client::MpdClient},
    shared::macros::status_warn,
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

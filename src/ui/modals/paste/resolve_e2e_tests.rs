//! **End-to-end tests for the resolve landing closures.**
//!
//! Every other test of this path stops at the edge of `Ctx::command`: it
//! asserts on the `WorkRequest` / `ClientRequest` the app *sends*, or on a pure
//! function, while the real decision runs inside a closure on the client
//! thread. Twelve fixes were verified that way and the reported feature stayed
//! broken for eight rounds.
//!
//! These tests close the gap: `Ctx::command`'s channel is drained into a real
//! production `Client` connected to a scripted MPD server
//! ([`crate::tests::fixtures::fake_mpd_server`]), so a test observes the queue
//! MPD would actually end up with — and which row, under which id, is playing.

use std::collections::HashMap;

use crate::{
    ctx::Ctx,
    shared::{events::ClientRequest, ytdlp::YtStreamInfo},
    tests::fixtures::fake_mpd_server::{FakeMpdServer, songs},
    ui::modals::paste::{YtAction, apply_resolved_streams},
};

/// A signed stream URL, as yt-dlp returns one, for the video `tag`.
fn stream_url(tag: &str) -> String {
    format!("https://rr1.googlevideo.com/videoplayback?expire=7258118400&id={tag}")
}

/// A queue row still holding its pasted link — the state a `▶▶` target is in
/// before anything resolves it.
fn link_row(id: u32, link: &str) -> (u32, String) {
    (id, format!("{link}#s2u-audio"))
}

const LINK_B: &str = "https://www.youtube.com/watch?v=bbbbbbbbbbb";
const LINK_C: &str = "https://www.youtube.com/watch?v=ccccccccccc";

/// The resolved info a work thread would report for `link`, playing from `url`.
fn info(link: &str, url: &str) -> YtStreamInfo {
    YtStreamInfo {
        url: url.to_owned(),
        original_url: link.to_owned(),
        title: "A stream".to_owned(),
        ..Default::default()
    }
}

/// A rig holding the app-side `Ctx`, the live MPD server, and the `Client` the
/// queued closures run against.
struct Rig {
    ctx: Ctx,
    server: FakeMpdServer,
    client: crate::mpd::client::Client<'static>,
    client_rx: crossbeam::channel::Receiver<ClientRequest>,
    work_rx: crossbeam::channel::Receiver<crate::shared::events::WorkRequest>,
}

impl Rig {
    /// `rows` is the queue **as MPD has it**; `ctx.queue` starts from the same
    /// list, so the app and MPD agree until a swap lands (after which they
    /// diverge, exactly as they do live).
    fn new(rows: Vec<(u32, String)>) -> Self {
        let server = FakeMpdServer::start(rows.clone());
        let client = server.client();

        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client_channel = crossbeam::channel::unbounded();
        Box::leak(Box::new(app.1.clone()));
        let client_rx = client_channel.1.clone();
        let work_rx = work.1.clone();
        let mut ctx = crate::tests::fixtures::ctx(
            (app.0, app.1),
            (work.0, work.1),
            (client_channel.0, client_channel.1),
        );
        ctx.queue = songs(&rows);
        Self { ctx, server, client, client_rx, work_rx }
    }

    /// Run every `ClientRequest::Command` queued so far against the real
    /// `Client`, exactly as the event loop's client thread does.
    fn run_client_requests(&mut self) -> usize {
        let mut ran = 0;
        while let Ok(request) = self.client_rx.try_recv() {
            if let ClientRequest::Command(command) = request {
                let _ = (command.callback)(&mut self.client);
                ran += 1;
            }
        }
        ran
    }

    /// Refresh `ctx.queue` from MPD, as the event loop's queue update does
    /// ~1.8 ms after a swap.
    fn sync_queue_from_mpd(&mut self) {
        use crate::mpd::mpd_client::MpdClient;
        self.ctx.queue = self
            .client
            .playlist_info()
            .expect("playlistinfo must succeed")
            .unwrap_or_default();
    }

    fn rows_by_id(&self) -> HashMap<u32, String> {
        self.server.snapshot().into_iter().map(|row| (row.id, row.file)).collect()
    }

    /// The files in the fake queue, in order.
    fn files(&self) -> Vec<String> {
        self.server.files()
    }

    fn playing_file(&self) -> Option<String> {
        self.server.playing_file()
    }

    fn commands(&self) -> Vec<String> {
        self.server.commands()
    }

    /// The resolve actions the app asked the work thread for.
    fn pending_resolves(&self) -> Vec<YtAction> {
        self.work_rx
            .try_iter()
            .filter_map(|request| match request {
                crate::shared::events::WorkRequest::ResolveYtStreams { action, .. } => Some(action),
                _ => None,
            })
            .collect()
    }
}

// The rig drives the production `Client` (whose `playlist_info`, `deleteid`,
// `addid` and `playid` are the real code), so a failure here is a failure of the
// shipped path rather than of a stand-in for it.
//
// **The reported bug, replayed from the live log of build `a7ccef07`.**
//
// The step (`▶▶`) resolves the row MPD reports as next — id `814` — at `T`. The
// one-entry look ahead is resolving *the same row* for its in-place swap and
// lands first (`T+1.3 s`), so the row is swapped and now carries a **new** id.
// The step's own `ReplaceAndPlay` then lands (`T+2.7 s`) still naming `814`,
// which no longer exists.
//
// The acceptance requirement is the user's own: the track must start, and it
// must be the track the user stepped to.
#[test]
fn an_unwarmed_step_plays_its_row_after_the_look_ahead_swapped_it() {
    // MPD's queue: 814 is the row being stepped to (still a tagged link), 1206
    // is the current row, 1477 the one after.
    let mut rig = Rig::new(vec![
        (1206, stream_url("now")),
        link_row(814, LINK_B),
        link_row(1477, LINK_C),
    ]);
    rig.ctx.status.state = crate::mpd::commands::State::Play;
    rig.ctx.status.songid = Some(1206);
    rig.ctx.status.nextsongid = Some(814);
    rig.ctx.status.random = true;

    // T: the step arms on row 814 and asks for it to be resolved AND played.
    crate::ui::panes::queue::next_track_resolving(&rig.ctx);
    assert!(
        matches!(rig.pending_resolves().first(), Some(YtAction::ReplaceAndPlay(814))),
        "the step resolves the row it will play"
    );
    assert_eq!(rig.ctx.pending_next_step.get(), Some(814), "the step is armed on 814");

    // T+1.3 s: the **look ahead's** in-place swap of the same row lands first.
    // This is the live order — the `stepping="true"` line at 10:16:54.81 came
    // from the look ahead's landing, not the step's.
    let look_ahead = stream_url("lookahead");
    apply_resolved_streams(
        &rig.ctx,
        vec![info(LINK_B, &look_ahead)],
        YtAction::ReplaceInPlace(814),
        Vec::new(),
    );
    rig.run_client_requests();
    // The app-side queue catches up with the swap, as the event loop's refresh
    // does shortly after the status poll.
    rig.sync_queue_from_mpd();

    let after_swap = rig.rows_by_id();
    assert_eq!(
        after_swap.len(),
        3,
        "the in-place swap keeps the queue length — commands: {:?}, rows: {after_swap:?}",
        rig.commands()
    );
    assert!(
        after_swap.values().any(|file| file.contains("/s2u/audio/")),
        "the look ahead swapped row 814's *link* for a playable stream — \
         commands: {:?}, rows: {after_swap:?}",
        rig.commands()
    );
    assert!(
        !after_swap.contains_key(&814),
        "the swap gave the row a new id, so the step's own id 814 is gone — \
         rows: {after_swap:?}"
    );
    assert!(
        rig.playing_file().is_none(),
        "an in-place swap must not start playback: the look ahead is a warm-up \
         — commands: {:?}",
        rig.commands()
    );

    // T+2.7 s: the step's own landing. It was built for id 814, which the swap
    // just replaced.
    apply_resolved_streams(
        &rig.ctx,
        vec![info(LINK_B, &stream_url("step"))],
        YtAction::ReplaceAndPlay(814),
        Vec::new(),
    );
    rig.run_client_requests();
    rig.sync_queue_from_mpd();

    // The acceptance test: the track started, and it is the track that was
    // stepped to.
    let playing = rig.playing_file();
    let rows = rig.rows_by_id();
    assert!(
        playing.is_some(),
        "the step must start playback even though the look ahead swapped the \
         row out from under it (its id, 814, is gone by the time it lands) — \
         commands: {:?}, rows: {rows:?}",
        rig.commands()
    );
    let playing = playing.unwrap_or_default();
    assert!(
        playing.contains("/s2u/audio/"),
        "the playing row must be the track the user stepped to (row 814's \
         stream), not another row — got {playing:?}, commands: {:?}",
        rig.commands()
    );
    assert!(
        rig.ctx.pending_next_step.get().is_none(),
        "the step is settled, so the ▶▶ spinner must stop"
    );
    assert_eq!(
        rig.files().len(),
        3,
        "no stray row is appended or dropped: {rows:?}"
    );
}

/// The in-place swap keeps the row at **its own position**: it deletes first
/// and only then re-adds at the recorded index, so the shift caused by the
/// deletion is already accounted for. (An earlier reading suspected this
/// ordering was the misplaced selection; driving the real `Client` shows the
/// order is `deleteid` then `addid`, and the row lands back in its own slot.)
#[test]
fn the_in_place_swap_leaves_the_row_at_its_own_position() {
    let mut rig = Rig::new(vec![
        (1206, stream_url("now")),
        link_row(814, LINK_B),
        link_row(1477, LINK_C),
    ]);

    apply_resolved_streams(
        &rig.ctx,
        vec![info(LINK_B, &stream_url("step"))],
        YtAction::ReplaceInPlace(814),
        Vec::new(),
    );
    rig.run_client_requests();

    let rows = rig.server.snapshot();
    let files: Vec<String> = rows.iter().map(|row| row.file.clone()).collect();
    let commands = rig.server.commands();
    assert!(
        commands.iter().any(|c| c.starts_with("deleteid 814")),
        "the swap deletes the old row — commands: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c.ends_with(" 1")),
        "the swap re-adds at the row's own index 1 — commands: {commands:?}"
    );
    assert_eq!(
        rows.len(),
        3,
        "the swap preserves the queue length — actually got {files:?}"
    );
    assert!(
        rows[1].file.contains("/s2u/audio/") || rows[1].file.contains("videoplayback"),
        "the swapped row must stay at index 1 (its own position), but index 1 \
         is {:?} — full queue: {files:?}",
        rows[1].file
    );
}

// ---------------------------------------------------------------------------
// The reported storm: **add a playlist, then press play.**
//
// A pasted playlist lands as N unresolved `#s2u-audio` links in one enqueue
// (`enqueue_stream_links`), and with `play` the first row is resolved on the
// spot. Nothing has warmed the *second* row yet: the one-entry look ahead only
// runs once something is already playing (`warm_next_queue_link` bails on
// `find_current_song_in_queue`), so at the moment the first resolve lands and
// MPD prefetches its next song, row 2 is still a raw link.
//
// MPD fails that prefetch and skips; each skip is a decode error the recovery
// resolves. That is the storm the user sees as the selection running down the
// freshly added playlist with spinners.
#[test]
fn a_freshly_queued_playlist_does_not_storm_the_second_row() {
    // The queue as the playlist enqueue leaves it: every row a raw link.
    let mut rig = Rig::new(vec![
        link_row(1, LINK_B),
        link_row(2, LINK_C),
        link_row(3, "https://www.youtube.com/watch?v=ddddddddddd"),
    ]);
    // MPD is stopped; `play_from_stop` picks the first row (no current song).
    rig.ctx.status.state = crate::mpd::commands::State::Stop;
    rig.ctx.status.songid = None;
    rig.ctx.status.nextsongid = None;

    // The play: the first row is resolved and started.
    crate::ui::panes::queue::play_from_stop(&rig.ctx);
    let asked = rig.pending_resolves();
    assert!(
        matches!(asked.first(), Some(YtAction::ReplaceAndPlay(1))),
        "the first queued row is the one resolved and played — asked: {asked:?}"
    );

    // That resolve lands: row 1 becomes a playable stream and starts.
    apply_resolved_streams(
        &rig.ctx,
        vec![info(LINK_B, &stream_url("first"))],
        YtAction::ReplaceAndPlay(1),
        Vec::new(),
    );
    rig.run_client_requests();
    rig.sync_queue_from_mpd();
    assert!(
        rig.playing_file().is_some_and(|f| f.contains("/s2u/audio/")),
        "the first playlist row must start playing — commands: {:?}",
        rig.commands()
    );

    // Now playback has begun, so the look ahead arms on the row MPD reports
    // next. It must resolve exactly that one row — not walk the playlist.
    //
    // The status is set as the event loop's status poll reports it once the
    // play landed: row 1 is the current song, row 2 is MPD's next. (The fake
    // server answers `status` with a fixed literal, so the poll's values are
    // stood in for here rather than read back.)
    let playing_id = rig
        .rows_by_id()
        .into_iter()
        .find(|(_, file)| file.contains("/s2u/audio/"))
        .map(|(id, _)| id)
        .expect("a playable row exists after the resolve");
    rig.ctx.status.state = crate::mpd::commands::State::Play;
    rig.ctx.status.songid = Some(playing_id);
    rig.ctx.status.nextsongid = rig
        .server
        .snapshot()
        .iter()
        .map(|row| row.id)
        .find(|id| *id != playing_id);
    crate::ui::modals::paste::warm_next_queue_link(&rig.ctx);
    rig.run_client_requests();

    let warmed: Vec<YtAction> = rig.pending_resolves();
    // The window is bounded: a 3-row queue warms at most the rows after the
    // current one, never the whole queue and never in an unbounded chain.
    assert!(
        warmed.len() <= crate::ui::modals::paste::LOOK_AHEAD_WINDOW,
        "the look ahead warms a bounded window, never a chain down the whole          queue — asked: {warmed:?}"
    );
    assert!(
        !warmed.is_empty(),
        "the following rows are warmed so MPD does not reach them unresolved —          asked: {warmed:?}"
    );
    assert!(
        warmed.iter().all(|action| matches!(action, YtAction::ReplaceInPlace(_))),
        "every warm is an in-place swap: the look ahead never starts playback — \
         asked: {warmed:?}"
    );

    // MPD's own pick leads the window: it is the row that actually plays next,
    // so it must be resolved first. The rows after it follow, which is what
    // keeps a batch-imported playlist ahead of playback.
    let next_id = rig.ctx.status.nextsongid.expect("MPD reports a next song");
    assert_eq!(
        warmed.first(),
        Some(&YtAction::ReplaceInPlace(next_id)),
        "MPD's next row is warmed first — nextsongid={next_id:?}, asked: {warmed:?}"
    );
    // A row the user deleted, or one before the current song, is never warmed.
    let queued: Vec<u32> = rig.rows_by_id().keys().copied().collect();
    assert!(
        warmed.iter().all(|action| match action {
            YtAction::ReplaceInPlace(id) => queued.contains(id),
            _ => true,
        }),
        "only rows that exist in the queue are warmed — asked: {warmed:?}"
    );
}

/// **The window already covered the row after the swapped row.**
///
/// This is the property that fixes the reported playlist storm. When the first
/// poll warms MPD's next row and swaps it in place, the row that plays *after*
/// it is warmed in the same pass — so when that swapped row becomes the current
/// song, the following row is already resolved and MPD never reaches an
/// unresolved link at all. Under the old one-row look ahead this test's premise
/// ("the next poll must warm row 30") was the whole problem: row 30 was still a
/// raw link at that moment.
#[test]
fn the_window_covers_the_row_after_the_one_it_swaps() {
    let mut rig = Rig::new(vec![
        link_row(10, LINK_B),
        link_row(20, LINK_C),
        link_row(30, "https://www.youtube.com/watch?v=ddddddddddd"),
        link_row(40, "https://www.youtube.com/watch?v=eeeeeeeeeee"),
    ]);
    // Row 10 is already playable and playing.
    apply_resolved_streams(
        &rig.ctx,
        vec![info(LINK_B, &stream_url("row10"))],
        YtAction::ReplaceInPlace(10),
        Vec::new(),
    );
    rig.run_client_requests();
    rig.sync_queue_from_mpd();
    let row10 = rig
        .rows_by_id()
        .into_iter()
        .find(|(_, file)| !file.contains("#s2u-audio") && !file.contains("youtube.com"))
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("row 10 is playable — rows: {:?}", rig.rows_by_id()));

    rig.ctx.status.state = crate::mpd::commands::State::Play;
    rig.ctx.status.songid = Some(row10);
    rig.ctx.status.nextsongid = Some(20);

    // One poll. It must leave *both* the next row and the row after it
    // resolved, so the transition off the swapped row finds a playable row.
    crate::ui::modals::paste::warm_next_queue_link(&rig.ctx);

    let asked = rig.pending_resolves();
    assert!(
        asked.iter().any(|action| matches!(action, YtAction::ReplaceInPlace(20))),
        "MPD's next row is warmed — asked: {asked:?}"
    );
    assert!(
        asked.iter().any(|action| matches!(action, YtAction::ReplaceInPlace(30))),
        "the row that plays *after* it is warmed in the same pass — this is what \
         stops the storm, because the next transition finds a resolved row \
         instead of a raw link. asked: {asked:?}"
    );
}


/// **A fresh playlist in `random` mode warms MPD's pick, not `current + 1`.**
///
/// This is the case the earlier storm rounds lived in, replayed for the
/// playlist trigger: MPD shuffles its own sequence, so the row after the
/// current one is usually *not* the row that plays next. Warming `current + 1`
/// leaves MPD's real next row unresolved, and MPD prefetches it, fails it, and
/// skips — the walk down the freshly added playlist the user reported.
///
/// The row must come from `status.nextsongid`. Here nextsongid (7) is
/// deliberately *before* the playing row in queue order, so an index-based
/// guess warms the wrong row and the assertion fails.
#[test]
fn a_shuffled_playlist_warms_the_row_mpd_names_not_the_next_index() {
    let mut rig = Rig::new(vec![
        link_row(4, LINK_B),
        link_row(5, "https://www.youtube.com/watch?v=ddddddddddd"),
        link_row(6, LINK_C),
        link_row(7, "https://www.youtube.com/watch?v=eeeeeeeeeee"),
    ]);
    // Row 4 is playing; MPD's shuffle has picked row 7 as next, which sits
    // *after* row 5 in queue order.
    apply_resolved_streams(
        &rig.ctx,
        vec![info(LINK_B, &stream_url("playing"))],
        YtAction::ReplaceInPlace(4),
        Vec::new(),
    );
    rig.run_client_requests();
    rig.sync_queue_from_mpd();
    let playing = rig
        .rows_by_id()
        .into_iter()
        .find(|(_, file)| !file.contains("#s2u-audio") && !file.contains("youtube.com"))
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("row 4 is playable — rows: {:?}", rig.rows_by_id()));

    rig.ctx.status.state = crate::mpd::commands::State::Play;
    rig.ctx.status.random = true;
    rig.ctx.status.songid = Some(playing);
    rig.ctx.status.nextsongid = Some(7);

    crate::ui::modals::paste::warm_next_queue_link(&rig.ctx);

    let asked = rig.pending_resolves();
    assert!(
        matches!(asked.first(), Some(YtAction::ReplaceInPlace(7))),
        "MPD's own pick (7) is the row that must be warmed **first** — warming \
         the next index instead leaves MPD prefetching an unresolved link. \
         asked: {asked:?}"
    );
    assert!(
        asked.len() <= crate::ui::modals::paste::LOOK_AHEAD_WINDOW,
        "the window stays bounded in shuffle too — asked: {asked:?}"
    );
}

// ---------------------------------------------------------------------------
// The playlist storm's fix: the look ahead must run **more than one row deep**.
//
// Measured from the user's live log (`/tmp/s2udio_1000.log`, 2026-10-15):
// 53 warms over 61 s (~24/min) at a median 2.46 s apart — which is exactly
// yt-dlp's resolve latency. One row of look ahead is behind whenever a
// transition is faster than a resolve, so MPD reaches the still-unresolved next
// row, fails it, skips, and the poll warms the row after that: one row per
// resolve, walking down the playlist. Warming a small window keeps the pool fed
// so the row that is about to play is already resolved.
#[test]
fn the_look_ahead_keeps_a_window_of_rows_resolved_ahead() {
    let mut rig = Rig::new(vec![
        link_row(1, LINK_B),
        link_row(2, LINK_C),
        link_row(3, "https://www.youtube.com/watch?v=ddddddddddd"),
        link_row(4, "https://www.youtube.com/watch?v=eeeeeeeeeee"),
        link_row(5, "https://www.youtube.com/watch?v=fffffffffff"),
        link_row(6, "https://www.youtube.com/watch?v=ggggggggggg"),
    ]);
    rig.ctx.status.state = crate::mpd::commands::State::Play;
    rig.ctx.status.songid = Some(1);
    rig.ctx.status.nextsongid = Some(2);

    crate::ui::modals::paste::warm_next_queue_link(&rig.ctx);

    let asked = rig.pending_resolves();
    assert!(
        asked.len() > 1,
        "the look ahead must warm a window of rows ahead, not just one — one \
         row is always behind when the resolve is slower than the transition, \
         which is the measured storm. asked: {asked:?}"
    );
}

// ---------------------------------------------------------------------------
// The poll-cadence gate (2026-10-20).
//
// The look ahead is re-evaluated from every status poll. With the reporter's
// `status_update_interval_ms: 16` that is ~60 evaluations a second, and each
// one walked the queue, rebuilt the window and stated its verdict in the log —
// including the common "nothing here is warmable" verdict, which cannot change
// for a window whose inputs did not change. The live log measured the cost:
// 30 418 look-ahead lines in ~3 minutes, an 8 MiB file rotating every 2-3
// minutes, every other diagnostic line buried.
//
// The rule these two tests pin: a window is decided once per
// (current song, MPD's next pick, queue length); a *new* window is decided at
// once, without waiting anything out.
#[test]
fn an_unchanged_window_is_not_re_decided_on_every_status_poll() {
    let mut rig = Rig::new(vec![
        (1206, stream_url("now")),
        link_row(814, LINK_B),
        link_row(1477, LINK_C),
    ]);
    rig.ctx.status.state = crate::mpd::commands::State::Play;
    rig.ctx.status.songid = Some(1206);
    rig.ctx.status.nextsongid = Some(814);

    assert!(
        crate::ui::modals::paste::warm_next_queue_link(&rig.ctx),
        "the first poll evaluates the window"
    );
    let first = rig.pending_resolves();
    assert!(!first.is_empty(), "the first poll warms what it can — asked: {first:?}");

    // The next status tick, 16 ms later on the reporter's config, sees exactly
    // the same window. It must neither re-walk it nor ask for anything.
    assert!(
        !crate::ui::modals::paste::warm_next_queue_link(&rig.ctx),
        "an unchanged window is throttled, not re-decided"
    );
    assert!(
        rig.pending_resolves().is_empty(),
        "a throttled poll asks the work thread for nothing new"
    );
}

#[test]
fn a_window_whose_inputs_change_is_decided_at_once() {
    let mut rig = Rig::new(vec![
        (1206, stream_url("now")),
        link_row(814, LINK_B),
        link_row(1477, LINK_C),
    ]);
    rig.ctx.status.state = crate::mpd::commands::State::Play;
    rig.ctx.status.songid = Some(1206);
    rig.ctx.status.nextsongid = Some(814);

    assert!(
        crate::ui::modals::paste::warm_next_queue_link(&rig.ctx),
        "the first poll evaluates the window"
    );

    // MPD names another next pick: a different window, so it is evaluated
    // immediately rather than waiting out the throttle interval.
    rig.ctx.status.nextsongid = Some(1477);
    assert!(
        crate::ui::modals::paste::warm_next_queue_link(&rig.ctx),
        "a window with a different MPD pick is new work, not a repeat"
    );

    // A queue that grew is a different window too.
    rig.ctx.queue.push(crate::mpd::commands::Song {
        id: 2000,
        file: "https://www.youtube.com/watch?v=hhhhhhhhhhh#s2u-audio".to_owned(),
        ..Default::default()
    });
    assert!(
        crate::ui::modals::paste::warm_next_queue_link(&rig.ctx),
        "a queue of another length is new work, not a repeat"
    );
}

// ---------------------------------------------------------------------------
// **Reported 2026-10-20: "playback doesn't start after resolve — I have to
// double-click or reselect + play."**
//
// The one-entry look ahead warms the row after the current track, and the user
// clicks **that** row. The app holds MPD and resolves the row for a play while
// the warm-up's own resolve is already in flight; the warm-up wins and swaps the
// row in place, giving it a new id. The play's landing then found neither its id
// nor — because the already-swapped takeover was gated on a waiting `▶▶` step — a
// row to play, so nothing started.
//
// The acceptance requirement: the track starts, on the row the user clicked, and
// no duplicate row is left behind.
#[test]
fn a_plain_play_starts_after_the_look_ahead_swapped_its_row() {
    let mut rig = Rig::new(vec![(1206, stream_url("now")), link_row(814, LINK_B)]);
    rig.ctx.status.state = crate::mpd::commands::State::Play;
    rig.ctx.status.songid = Some(1206);
    rig.ctx.status.nextsongid = Some(814);
    let playing_before = rig.playing_file();

    // The user's play: row 814 is still an unresolved link, so it is resolved
    // for a play. (The real flow holds MPD first so it cannot skip past the row;
    // that only affects MPD's own behaviour, not this landing.)
    assert!(
        crate::ui::modals::paste::resolve_tagged_queue_entry(
            &rig.ctx,
            &format!("{LINK_B}#s2u-audio"),
            814
        ),
        "the play request is admitted for an unresolved row"
    );

    // T+1.3 s: the look ahead's in-place swap of the *same* row lands first.
    apply_resolved_streams(
        &rig.ctx,
        vec![info(LINK_B, &stream_url("lookahead"))],
        YtAction::ReplaceInPlace(814),
        Vec::new(),
    );
    rig.run_client_requests();
    rig.sync_queue_from_mpd();
    assert_eq!(
        rig.playing_file(),
        playing_before,
        "a warm-up swap never starts playback"
    );

    // T+2.7 s: the user's own resolve lands. Its id is gone — the row holds the
    // same stream under a new one — and the play must land on that row.
    apply_resolved_streams(
        &rig.ctx,
        vec![info(LINK_B, &stream_url("play"))],
        YtAction::ReplaceAndPlay(814),
        Vec::new(),
    );
    rig.run_client_requests();

    let playing = rig.playing_file().expect("the resolve must start playback");
    assert!(
        playing.contains("/s2u/audio/"),
        "the row the warm-up wrote is what plays, not a second copy — got {playing}"
    );
    assert_eq!(rig.files().len(), 2, "no duplicate row is added by the takeover");
}

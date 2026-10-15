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

//! **End-to-end tests for "Play" from the paste popup** — the temporary
//! (queue-free) entry the drag & drop popup starts.
//!
//! They drive the production `Client` against the scripted MPD server (the
//! same rig [`super::resolve_e2e_tests`] uses) and hand the result back to the
//! pane that owns the temp entry, so a test asserts on the queue and the
//! playing row MPD really ends up with.
//!
//! **The reported bug.** A file dragged into the TUI and started with
//! `Play > Audio` played nothing, while `Add to queue and play` on the same
//! file worked. The play itself was correct — `addid` + `playid` reached MPD
//! and `playid` started it — and then the app deleted the entry again:
//!
//! ```text
//! addid "…/track.mp3"     (the paste popup's Play)
//! playid 2                (it starts)
//! deleteid 2              (the idle event for that very start)
//! ```
//!
//! `playid` raises MPD's idle `Player` event, the event loop dispatches
//! `UiEvent::Player` to the panes **before** the status query that event
//! issues has answered, and the temp-play cleanup read the stale
//! `ctx.status.state == Stop` (the state from before the play) and dropped
//! the entry it had just started. Deleting the playing row stops MPD, so the
//! net result was neither playback nor a queue row — exactly what the report
//! described. `Add to queue and play` was unaffected because it never records
//! a temp entry, and a play issued while something was already playing was
//! unaffected because the stale status said `Play`.

use crate::{
    ctx::Ctx,
    mpd::commands::State,
    shared::events::ClientRequest,
    tests::fixtures::fake_mpd_server::{FakeMpdServer, songs},
    ui::{UiEvent, panes::Pane},
};

use super::{PASTE_PLAY, PastedItem, parse_paste, play_item};

/// A file dropped on the TUI: an empty file on disk (the paste classifier
/// checks that it exists) plus the `PastedItem` a file-manager drop of it
/// produces. The file is removed when the test ends.
struct DroppedFile {
    path: String,
    item: PastedItem,
}

impl DroppedFile {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .join(format!("s2udio-play-{}-{name}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        std::fs::write(&path, b"").expect("temp file");
        let items = parse_paste(&path);
        assert_eq!(items, vec![PastedItem::File(path.clone())], "the drop must parse");
        let item = items[0].clone();
        Self { path, item }
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn item(&self) -> &PastedItem {
        &self.item
    }
}

impl Drop for DroppedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A rig holding the app-side `Ctx`, the live MPD server, and the `Client` the
/// queued requests run against.
struct Rig {
    ctx: Ctx,
    server: FakeMpdServer,
    client: crate::mpd::client::Client<'static>,
    client_rx: crossbeam::channel::Receiver<ClientRequest>,
    radio: crate::ui::panes::radio::RadioPane,
}

impl Rig {
    /// `rows` is the queue **as MPD has it**; `ctx.queue` starts from the same
    /// list, so the app and MPD agree until a play changes it.
    fn new(rows: Vec<(u32, String)>) -> Self {
        let server = FakeMpdServer::start(rows.clone());
        let client = server.client();
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client_channel = crossbeam::channel::unbounded();
        let client_rx = client_channel.1.clone();
        Box::leak(Box::new(app.1.clone()));
        let ctx = crate::tests::fixtures::ctx(
            (app.0, app.1),
            (work.0, work.1),
            (client_channel.0, client_channel.1),
        );
        let radio = crate::ui::panes::radio::RadioPane::new(&ctx);
        let mut rig = Self { ctx, server, client, client_rx, radio };
        rig.ctx.queue = songs(&rows);
        rig
    }

    /// Run every queued `ClientRequest` against the real `Client`, exactly as
    /// the event loop's client thread does, and return the ids of the queries
    /// that ran.
    fn run_client_requests(&mut self) -> Vec<&'static str> {
        let mut queries = Vec::new();
        while let Ok(request) = self.client_rx.try_recv() {
            match request {
                ClientRequest::Query(query) => {
                    (query.callback)(&mut self.client).expect("the query must succeed");
                    queries.push(query.id);
                }
                ClientRequest::Command(command) => {
                    (command.callback)(&mut self.client).expect("the command must succeed");
                }
                ClientRequest::QuerySync(_) => {}
            }
        }
        queries
    }

    /// The `addid` id the temp entry got: the fake server hands out the next
    /// free id, so the newest row is the one the play just added.
    fn newest_row_id(&self) -> u32 {
        self.server
            .state()
            .rows
            .iter()
            .map(|row| row.id)
            .max()
            .expect("the queue must hold the played row")
    }

    /// Play `item` the way the popup does and hand the query result back to
    /// the Radio pane, which records the temp id (as `Ui::on_command_finished`
    /// does for the `PaneType::Radio` target).
    fn play_and_record(&mut self, item: &PastedItem) -> u32 {
        play_item(&self.ctx, item).expect("play_item must not fail");
        let queries = self.run_client_requests();
        assert_eq!(queries, vec![PASTE_PLAY], "the play must issue one temp-play query");
        let id = self.newest_row_id();
        self.radio
            .on_query_finished(
                PASTE_PLAY,
                crate::MpdQueryResult::Any(Box::new(id)),
                false,
                &self.ctx,
            )
            .expect("the paste play result must be accepted");
        // The result can queue a cleanup of its own (the entry it replaces);
        // the client thread picks it up on its next pass.
        self.run_client_requests();
        id
    }

    /// Dispatch a UI event to the pane that owns the temp entry.
    fn dispatch(&mut self, event: &mut UiEvent) {
        self.radio.on_event(event, false, &self.ctx).expect("the pane must handle the event");
    }

    fn commands(&self) -> Vec<String> {
        self.server.commands()
    }
}

/// The reported bug, replayed: the app must not delete the row its own play
/// started. The idle `Player` event for that start arrives with the status
/// from before it (`Stop` here), which used to read as "playback stopped, drop
/// the temp entry".
#[test]
fn a_play_survives_the_idle_event_it_raises() {
    let mut rig = Rig::new(vec![(1, "existing.mp3".to_owned())]);
    let file = DroppedFile::new("kept.mp3");

    let id = rig.play_and_record(file.item());
    assert_eq!(rig.server.playing_file().as_deref(), Some(file.path()));

    // The idle event MPD raises for the start itself. `ctx.status` has not
    // been refreshed yet, so it still says what it said before the play.
    assert_eq!(rig.ctx.status.state, State::Stop, "the status is stale by design");
    rig.dispatch(&mut UiEvent::Player);
    rig.run_client_requests();

    assert_eq!(
        rig.server.playing_file().as_deref(),
        Some(file.path()),
        "the idle event of the play's own start must not stop it"
    );
    assert!(
        rig.server.files().iter().any(|dropped| dropped == file.path()),
        "the temp row must stay in MPD's queue: {:?}",
        rig.server.files()
    );
    assert!(
        !rig.commands().contains(&format!("deleteid {id}")),
        "nothing may delete the row that just started: {:?}",
        rig.commands()
    );
}

/// The cleanup the stale arm was standing in for still happens: a status
/// update that really reports `Stop` drops the temp entry (streams keep their
/// queue row while playing, so `SongChanged` alone would miss them).
#[test]
fn a_real_stop_still_drops_the_temp_row() {
    let mut rig = Rig::new(vec![(1, "existing.mp3".to_owned())]);
    let file = DroppedFile::new("stopped.mp3");
    let id = rig.play_and_record(file.item());

    // The user stopped playback: MPD reports it and nothing plays any more.
    rig.ctx.status.state = State::Stop;
    rig.dispatch(&mut UiEvent::PlaybackStateChanged);
    rig.run_client_requests();

    assert!(
        rig.commands().contains(&format!("deleteid {id}")),
        "a real stop must still drop the temp row: {:?}",
        rig.commands()
    );
    assert!(
        !rig.server.files().iter().any(|dropped| dropped == file.path()),
        "the temp row must be gone: {:?}",
        rig.server.files()
    );
}

/// Playing a second file must not leave the first temp row behind: the temp
/// row is hidden from the queue list, so an orphan is invisible *and*
/// unremovable by the user, and every play would add one.
#[test]
fn a_second_play_drops_the_first_temp_row() {
    let mut rig = Rig::new(vec![(1, "existing.mp3".to_owned())]);
    let first = DroppedFile::new("first.mp3");
    let first_id = rig.play_and_record(first.item());
    let second = DroppedFile::new("second.mp3");
    let second_id = rig.play_and_record(second.item());

    assert_ne!(first_id, second_id, "each play gets its own queue id");
    assert_eq!(
        rig.server.playing_file().as_deref(),
        Some(second.path()),
        "the second play keeps playing"
    );
    assert!(
        !rig.server.files().iter().any(|dropped| dropped == first.path()),
        "the first temp row must be dropped, not orphaned: {:?}",
        rig.server.files()
    );
    assert_eq!(
        rig.server.files(),
        vec!["existing.mp3".to_owned(), second.path().to_owned()],
        "the queue holds the untouched row and the playing one"
    );
}

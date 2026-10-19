//! The torrent picker — the modal a pasted torrent/magnet opens once its
//! **file list** is known.
//!
//! Two phases of the scan reach this modal (`core::torrent::scan_torrent`):
//!
//! - **phase 3 — the file list** (`WorkDone::TorrentScanFiles`): the rows
//!   appear (every media file, name + size, all ticked) and **Download** is
//!   live immediately.
//! - **phase 4 — the torrent is `live`** (`WorkDone::TorrentScanned`): the
//!   engine will serve streams, so **Stream** ungates.
//!
//! Stream must not run before phase 4: a stream requested while rqbit
//! checksum-validates a re-added torrent errors and mpv exits 2 (the
//! round-18 host finding). So the button renders dim with a spinning glyph
//! until the engine reports the torrent live — the spinner *is* the wait
//! window, and it clears in place (the modal refreshes under the
//! `torrent_picker` replacement id) the moment phase 4 lands.
//!
//! Layout: the list, then one footer row — `[ Cancel ]` glued left, and
//! `[ ⟳ Stream ]  [ Download ]` packed right.

use anyhow::Result;
use ratatui::Frame;

use super::{
    Modal,
    menu::{ButtonRowLayout, ListButton, modal::MenuModal},
};
use crate::{
    ctx::Ctx,
    shared::{id::Id, keys::ActionEvent, mouse_event::MouseEvent},
    ui::input::InputResultEvent,
};

/// The replacement id the picker refreshes in place under: a scan landing
/// (phase 3 or 4) rebuilds it so the rows appear / Stream ungates without
/// stacking a second copy on top.
const TORRENT_PICKER_REPLACEMENT_ID: &str = "torrent_picker";

/// The spinner frames the gated Stream button cycles through (one frame per
/// refresh, see [`refresh_torrent_picker`]).
pub(crate) const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// The label of the Stream button. `None` = live, the plain label; `Some(i)`
/// = still waiting on phase 4, frame `i` of the spinner.
fn stream_label(spinner: Option<usize>) -> String {
    match spinner {
        Some(frame) => format!("Stream {}", SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]),
        None => "Stream".to_owned(),
    }
}

/// Human-readable byte size ("1.2 GB") for the picker rows.
fn format_bytes(len: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = len as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{len} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// What the picker's two buttons do with the ticked file indices.
pub(crate) enum TorrentPickerAction {
    /// Play the picked files through mpv (needs phase 4).
    Stream,
    /// Commit a download job for the picked files (phase 4 not needed).
    Download,
}

/// The state the picker renders from — resolved fresh on every (re)build so
/// an in-place refresh picks up a scan that landed in the meantime.
pub(crate) struct TorrentPickerState {
    /// `(positional index, name, length)` of every media file, scan order.
    pub rows: Vec<(usize, String, u64)>,
    /// `Some(key)` while the file list is not known yet (phase 3 pending) —
    /// the key the scan is tracked under.
    pub pending_key: Option<String>,
    /// `Some(err)` when the scan failed: the picker shows a dead end.
    pub error: Option<String>,
    /// Elapsed seconds of the wait, for the "waiting" line.
    pub elapsed_secs: u64,
    /// `Some(frame)` while Stream is gated (phase 4 pending).
    pub spinner: Option<usize>,
    /// Phase 4: the engine reported the torrent `live`. The ONLY thing that
    /// enables Stream — the spinner is presentation, not the gate.
    pub live: bool,
    /// The torrent's own name once the metainfo landed.
    pub torrent_name: Option<String>,
}

/// Inputs to the picker's state — everything `resolve` reads out of `Ctx`,
/// separated so the state machine is unit-testable without one.
pub(crate) struct TorrentPickerInputs {
    /// Phase 4 done: the engine reported the torrent live. Carries the
    /// torrent's name and file list, which the picker renders either way.
    pub live: Option<(String, Vec<crate::core::torrent::ScannedFile>)>,
    /// Phase 3: the file list landed but the scan has not completed.
    pub listed: Option<(String, Vec<crate::core::torrent::ScannedFile>)>,
    /// The scan failed.
    pub error: Option<String>,
    /// Seconds the scan has been waiting (as last reported).
    pub elapsed_secs: u64,
}

/// The picker's rows from a scan's file list: every media file, scan order,
/// as `(positional index, name, length)`.
fn media_rows(
    files: &[crate::core::torrent::ScannedFile],
) -> Vec<(usize, String, u64)> {
    crate::core::torrent::media_files(files)
        .into_iter()
        .map(|f| (f.index, f.name.clone(), f.length))
        .collect()
}

impl TorrentPickerState {
    /// Resolves the picker's state from the scan bookkeeping. Phase 4 landed
    /// (`live` is `Some`) → Stream is enabled; a phase-3 file list exists but
    /// the scan has not finished → the rows show and only Download is usable
    /// (Stream spins); nothing at all → the metainfo is still being fetched,
    /// so there is nothing to list yet.
    pub fn from_inputs(
        inputs: TorrentPickerInputs,
        spinner: Option<usize>,
    ) -> Self {
        if let Some((name, files)) = inputs.live {
            let rows = media_rows(&files);
            return Self {
                rows,
                pending_key: None,
                error: None,
                elapsed_secs: 0,
                spinner: None,
                live: true,
                torrent_name: Some(name),
            };
        }
        if let Some(err) = inputs.error {
            return Self {
                rows: Vec::new(),
                pending_key: None,
                error: Some(err),
                elapsed_secs: 0,
                spinner: None,
                live: false,
                torrent_name: None,
            };
        }
        let rows = inputs
            .listed
            .as_ref()
            .map(|(_, files)| media_rows(files))
            .unwrap_or_default();
        let torrent_name = inputs.listed.as_ref().map(|(name, _)| name.clone());
        Self {
            rows,
            pending_key: Some(String::new()),
            error: None,
            elapsed_secs: inputs.elapsed_secs,
            spinner,
            live: false,
            torrent_name,
        }
    }

    /// Resolves the picker's state for the torrent `key` out of `Ctx`.
    /// The borrows are all cloned first — a `Ref` held across the next
    /// `borrow()` would panic on the same `RefCell` (the round-55 F1 class
    /// of bug).
    pub fn resolve(ctx: &Ctx, key: &str, spinner: Option<usize>) -> Self {
        let scan = ctx.torrent_scans.borrow().get(key).cloned();
        let listed = ctx.torrent_file_lists.borrow().get(key).cloned();
        let elapsed_secs = ctx
            .torrent_scan_progress
            .borrow()
            .get(key)
            .map(|p| p.elapsed_secs)
            .unwrap_or(0);
        let (live, error): (Option<(String, Vec<crate::core::torrent::ScannedFile>)>, _) =
            match scan {
                Some(Ok(scan)) => (
                    Some((scan.torrent_name.clone(), scan.files.clone())),
                    None,
                ),
                Some(Err(err)) => (None, Some(err)),
                None => (None, None),
            };
        let mut state = Self::from_inputs(
            TorrentPickerInputs { live, listed, error, elapsed_secs },
            spinner,
        );
        // The key is only bookkeeping for the caller (`from_inputs` does not
        // know it); keep it so the modal can address the scan it renders.
        if state.pending_key.is_some() {
            state.pending_key = Some(key.to_owned());
        }
        state
    }

    /// True while the picker has no rows because phase 3 has not landed: it
    /// shows the "Reading torrent…" wait instead of a list.
    pub fn waiting_for_metainfo(&self) -> bool {
        self.pending_key.is_some() && self.rows.is_empty()
    }

    /// The torrent's display name: from the scan or the phase-3 list, else
    /// the pasted label.
    pub fn name(&self, fallback: &str) -> String {
        self.torrent_name.clone().unwrap_or_else(|| fallback.to_owned())
    }

    /// Stream can run only when the engine reported the torrent live (phase
    /// 4). The spinner mirrors that gate for display; it never decides it.
    pub fn stream_enabled(&self) -> bool {
        self.live && !self.rows.is_empty()
    }

    /// Whether the picker has anything to act on. Download follows the
    /// ticked rows (all ticked on open), so this only documents the dead-end
    /// cases — a failed scan or a torrent with no media.
    pub fn has_rows(&self) -> bool {
        !self.rows.is_empty()
    }
}

/// The torrent picker modal.
#[derive(derive_more::Debug)]
pub(crate) struct TorrentPicker<'a> {
    #[debug(skip)]
    inner: MenuModal<'a>,
}

impl<'a> TorrentPicker<'a> {
    /// Builds the picker for the torrent `key`. `fallback_label` is what the
    /// title shows until the torrent's own name is known, `spinner` the
    /// current animation frame.
    ///
    /// The confirm action is NOT a parameter: the picker is rebuilt in place
    /// on every scan event, so a per-build closure would be lost on the next
    /// refresh. It resolves the pasted item from `Ctx.torrent_picker_item`
    /// and calls [`perform_picker_action`] instead, which every rebuild
    /// shares.
    pub(crate) fn new(
        ctx: &Ctx,
        key: String,
        fallback_label: String,
        spinner: usize,
    ) -> Self {
        let state = TorrentPickerState::resolve(ctx, &key, Some(spinner));
        let stream_ready = state.stream_enabled();
        let rows = state.rows.clone();
        let waiting = state.waiting_for_metainfo();
        let has_rows = state.has_rows();
        let error = state.error.clone();
        let elapsed = state.elapsed_secs;
        let spinner_frame = state.spinner;
        let key_for_confirm = key.clone();
        // The picker opens on the pasted label (`magnet:00048b1d`) and
        // switches to the torrent's own name once the metainfo names it —
        // the same window throughout, never a second modal.
        let title = format!(" Torrent: {} ", state.name(&fallback_label));
        let label = fallback_label.clone();
        let inner = MenuModal::new(ctx)
            .width(70)
            .title(title)
            .replacement_id(TORRENT_PICKER_REPLACEMENT_ID)
            .height_cap_fraction(2, 3)
            .list_section(ctx, move |mut section| {
                section.set_on_close(move |ctx| clear_picker(ctx, &key_for_confirm));
                if let Some(err) = error {
                    section.header(err);
                    section.add_item("Close", |_ctx| Ok(()));
                    return Some(section);
                }
                if waiting {
                    // Phase 3 has not landed: the window is the wait, and it
                    // has no buttons — there is nothing to act on yet.
                    section.header(format!("{label} loading… {}", mmss(elapsed)));
                    section.header("esc to cancel");
                    return Some(section);
                }
                if !has_rows {
                    section.header("No playable media in this torrent");
                    section.add_item("Close", |_ctx| Ok(()));
                    return Some(section);
                }
                // The file list, one ticked row per media file (the tick is
                // the row's selection icon). All ticked, so both actions are
                // meaningful the moment the list appears.
                for (_, name, len) in &rows {
                    section.add_check_item(
                        format!("{name}  ({})", format_bytes(*len)),
                        true,
                    );
                }
                section.check_list(move |ctx, selected, choice| {
                    let indices: Vec<usize> = selected
                        .iter()
                        .filter_map(|row| rows.get(*row).map(|(index, _, _)| *index))
                        .collect();
                    let action = if choice == 0 {
                        TorrentPickerAction::Stream
                    } else {
                        TorrentPickerAction::Download
                    };
                    perform_picker_action(ctx, indices, action)
                });
                // The buttons exist only once the list does: Download and
                // Cancel work immediately, Stream waits for the engine.
                // Stream, then Download, packed on the LEFT; Cancel on the
                // RIGHT — and the whole row sits BELOW the window border, so
                // the file list stays inside the outline on its own.
                section.set_buttons(vec![
                    ListButton::confirm(
                        stream_label(if stream_ready { None } else { spinner_frame }),
                        0,
                    ),
                    ListButton::confirm("Download", 1),
                    ListButton::cancel("Cancel"),
                ]);
                section.button_layout(ButtonRowLayout::ActionsLeftCancelRight);
                section.buttons_below_window(true);
                // Choice indices are the buttons' own order: 0 = Stream,
                // 1 = Download.
                section.set_button_enabled(0, stream_ready);
                Some(section)
            })
            .build();
        Self { inner }
    }
}

/// Runs the picker's confirm: what the ticked files do. Both buttons act on
/// the **ticked** set (all files on open), so `Stream` and `Download` always
/// agree on the file set — Download is simply the same files without the
/// playback the Stream gate waits for.
///
/// Resolved from `Ctx` (not from a closure) because the picker refreshes in
/// place: see [`TorrentPicker::new`].
fn perform_picker_action(
    ctx: &Ctx,
    indices: Vec<usize>,
    action: TorrentPickerAction,
) -> Result<()> {
    let Some(key) = ctx.torrent_picker_key.borrow().clone() else {
        return Ok(());
    };
    let item = ctx.torrent_picker_item.borrow().clone();
    let Some(item) = item else {
        // The item vanished with the popup: nothing can be acted on.
        log::warn!(key:?; "The torrent picker lost its item; dropping the action");
        return Ok(());
    };
    match action {
        TorrentPickerAction::Stream if streams_download(ctx) => {
            // "Streaming also downloads": one committed job that plays the
            // picked files AND keeps them (`commit_to_daemon(play = true)`),
            // so the torrent shows up in the Downloads modal and the file
            // lands in `s2udio-downloads`. Playback must run on the daemon's
            // engine — a TUI engine and the daemon cannot share one cache
            // dir (round 54, §2.2).
            crate::ui::modals::paste::commit_to_daemon(ctx, &item, &key, indices, true)
        }
        TorrentPickerAction::Stream => {
            crate::ui::modals::paste::play_torrent_route(ctx, &item, &key, indices, false)
        }
        TorrentPickerAction::Download => {
            // No playback, and no wait for the torrent to be live: the daemon
            // downloads the picked files and the job's own panel opens.
            crate::ui::modals::paste::commit_to_daemon(ctx, &item, &key, indices, false)
        }
    }
}

/// Whether `Stream` also downloads the torrent (`torrent.stream_downloads`,
/// **on** by default): the stream then runs inside a committed download job
/// instead of a throw-away stream engine.
fn streams_download(ctx: &Ctx) -> bool {
    ctx.config.torrent.stream_downloads
}

/// `mm:ss`.
fn mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// The picker's animation frame: bumps once per refresh, so the gated
/// Stream button's glyph moves while phase 4 is pending.
fn next_spinner_frame(ctx: &Ctx) -> usize {
    let frame = (ctx.torrent_picker_spinner.get() + 1) % SPINNER_FRAMES.len();
    ctx.torrent_picker_spinner.set(frame);
    frame
}

/// Rebuild the open picker in place (the `torrent_picker` replacement id
/// swaps it for the new one at the same spot). Called when a scan lands
/// (phase 3 → the rows appear, phase 4 → Stream un-gates) and once a second
/// while phase 4 is pending (the spinner frame + the elapsed counter).
pub(crate) fn refresh_torrent_picker(ctx: &Ctx) {
    let Some(key) = ctx.torrent_picker_key.borrow().clone() else { return };
    let label = ctx
        .paste_modal_items
        .borrow()
        .as_ref()
        .and_then(|items| {
            items
                .iter()
                .find(|item| crate::ui::modals::paste::torrent_item_key(item) == key)
                .map(crate::ui::modals::paste::PastedItem::label)
        })
        .unwrap_or_else(|| key.clone());
    let frame = next_spinner_frame(ctx);
    let picker = TorrentPicker::new(ctx, key, label, frame);
    crate::shared::macros::modal!(ctx, picker);
}

/// The picker closed. Forget which torrent it showed, and cancel that
/// torrent's scan **only if the scan never completed** — a picker closed
/// while the metainfo or the `live` wait is still running would otherwise
/// leave its rqbit child behind with nobody able to use it.
///
/// A completed scan is kept: backing out of the picker into the download
/// panel (or any other modal) must not kill the engine behind a running
/// job, and the scan map also serves a repeat paste.
fn clear_picker(ctx: &Ctx, key: &str) {
    let completed = ctx.torrent_scans.borrow().contains_key(key);
    if !completed {
        if let Some(cancel) = ctx.torrent_scan_cancels.borrow_mut().remove(key) {
            let _ = cancel.send(());
        }
        ctx.torrent_scans_pending.borrow_mut().remove(key);
        ctx.torrent_scan_progress.borrow_mut().remove(key);
        log::debug!(key:?; "Picker closed before the scan finished; scan cancelled");
    }
    ctx.torrent_picker_key.borrow_mut().take();
    ctx.torrent_picker_item.borrow_mut().take();
}

/// [`clear_picker`] for the paste module's hand-off tests (the picker's
/// close hook is not reachable without a live modal).
#[cfg(test)]
pub(crate) fn clear_picker_for_test(ctx: &Ctx, key: &str) {
    clear_picker(ctx, key);
}

impl Modal for TorrentPicker<'_> {
    fn id(&self) -> Id {
        self.inner.id()
    }

    fn replacement_id(&self) -> Option<&std::borrow::Cow<'static, str>> {
        Modal::replacement_id(&self.inner)
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        self.inner.render(frame, ctx)
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &Ctx) -> Result<()> {
        self.inner.handle_insert_mode(kind, ctx)
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        self.inner.handle_key(key, ctx)
    }

    fn handle_raw_key(&mut self, key: crossterm::event::KeyEvent, ctx: &mut Ctx) -> Result<bool> {
        self.inner.handle_raw_key(key, ctx)
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        self.inner.handle_mouse_event(event, ctx)
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::core::torrent::ScannedFile;

    fn files() -> Vec<ScannedFile> {
        vec![
            ScannedFile { index: 0, name: "ep1.mkv".to_owned(), length: 100 },
            ScannedFile { index: 1, name: "ep2.mkv".to_owned(), length: 200 },
            ScannedFile { index: 2, name: "info.nfo".to_owned(), length: 5 },
        ]
    }

    /// Inputs for the state machine. `live` / `listed` choose which phase
    /// landed; the state only reads a scan's file list and name, so the live
    /// case is expressed through the same `ScannedFile` list (no engine is
    /// ever constructed — these tests never touch rqbit).
    fn inputs(live: bool, listed: bool, error: Option<&str>) -> TorrentPickerInputs {
        TorrentPickerInputs {
            live: live.then(|| ("Pack".to_owned(), files())),
            listed: listed.then(|| ("Pack".to_owned(), files())),
            error: error.map(str::to_owned),
            elapsed_secs: 7,
        }
    }

    #[test]
    fn phase_three_lists_the_media_and_gates_only_stream() {
        // The file list landed but the torrent is not live yet: the rows are
        // there, Download works, Stream spins.
        let state = TorrentPickerState::from_inputs(inputs(false, true, None), Some(3));
        assert_eq!(state.rows.len(), 2, "media files only, non-media excluded");
        assert_eq!(state.rows[0], (0, "ep1.mkv".to_owned(), 100));
        assert_eq!(state.rows[1], (1, "ep2.mkv".to_owned(), 200));
        assert!(!state.stream_enabled(), "Stream must wait for phase 4");
        assert!(state.has_rows(), "Download can act on the listed files");
        assert!(!state.waiting_for_metainfo());
        assert_eq!(state.name("magnet:abc"), "Pack");
    }

    #[test]
    fn phase_four_ungates_stream() {
        let state = TorrentPickerState::from_inputs(inputs(true, true, None), Some(3));
        assert_eq!(state.rows.len(), 2);
        assert!(state.stream_enabled(), "live torrents may stream");
        assert!(state.has_rows());
        assert_eq!(state.spinner, None, "no spinner once live");
    }

    #[test]
    fn phase_four_alone_still_lists_the_files() {
        // `TorrentScanned` carries the file list too, so a scan that lands in
        // one poll (a local .torrent: phases 3 and 4 together) is complete.
        let state = TorrentPickerState::from_inputs(inputs(true, false, None), Some(0));
        assert_eq!(state.rows.len(), 2);
        assert!(state.stream_enabled());
    }

    #[test]
    fn before_metainfo_it_waits_without_rows() {
        // Nothing landed: the picker shows "Reading torrent…", no rows, and
        // neither button can do anything.
        let state = TorrentPickerState::from_inputs(inputs(false, false, None), Some(0));
        assert!(state.rows.is_empty());
        assert!(state.waiting_for_metainfo());
        assert!(!state.stream_enabled());
        assert!(!state.has_rows());
        assert_eq!(state.elapsed_secs, 7, "the wait counter is kept");
        assert_eq!(state.name("magnet:abc"), "magnet:abc", "falls back to the label");
    }

    #[test]
    fn a_failed_scan_is_a_dead_end() {
        let state =
            TorrentPickerState::from_inputs(inputs(false, false, Some("No peers found")), Some(0));
        assert_eq!(state.error.as_deref(), Some("No peers found"));
        assert!(!state.stream_enabled());
        assert!(!state.has_rows());
        assert!(!state.waiting_for_metainfo(), "a failure is not a wait");
    }

    #[test_case(false, Some(0), false ; "a pending scan keeps the spinner")]
    #[test_case(false, None, false ; "a list without live still cannot stream")]
    #[test_case(true, Some(0), true ; "live wins over a stale spinner")]
    #[test_case(true, None, true ; "live and settled")]
    fn stream_gate_follows_the_live_phase(live: bool, spinner: Option<usize>, expect: bool) {
        let state = TorrentPickerState::from_inputs(inputs(live, true, None), spinner);
        assert_eq!(state.stream_enabled(), expect);
    }

    #[test]
    fn the_stream_label_carries_the_spinner_while_gated() {
        assert_eq!(stream_label(None), "Stream");
        let gated = stream_label(Some(0));
        assert!(gated.starts_with("Stream "), "the spinner follows the label: {gated}");
        assert_eq!(gated.chars().count(), "Stream".chars().count() + 2);
        // The frames cycle rather than run off the end of the slice.
        let last_frame = SPINNER_FRAMES.len() - 1;
        assert!(stream_label(Some(last_frame)).ends_with(SPINNER_FRAMES[last_frame]));
        assert_eq!(
            stream_label(Some(SPINNER_FRAMES.len())),
            stream_label(Some(0)),
            "the frame index wraps"
        );
    }

    // ---- rendering (headless, through a TestBackend) ----

    fn test_ctx() -> Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    /// Seeds `Ctx` the way the scan handlers do and renders the picker,
    /// returning the screen as text lines.
    fn render_picker(
        ctx: &mut Ctx,
        listed: Option<Vec<ScannedFile>>,
        live: bool,
    ) -> Vec<String> {
        let key = "magnet:?xt=urn:btih:0123456789abcdef";
        if let Some(files) = listed {
            ctx.torrent_file_lists
                .borrow_mut()
                .insert(key.to_owned(), ("Pack".to_owned(), files));
        }
        if live {
            // The picker reads only the listing off a scan, so a scan with no
            // engine is exactly the "live" state as far as rendering goes
            // (a real scan always has one; the picker never touches it).
            ctx.torrent_scans.borrow_mut().insert(
                key.to_owned(),
                Ok(crate::core::torrent::TorrentScan {
                    engine: None,
                    torrent_id: "1".to_owned(),
                    torrent_name: "Pack".to_owned(),
                    info_hash: None,
                    files: files(),
                }),
            );
        }
        *ctx.torrent_picker_key.borrow_mut() = Some(key.to_owned());
        let mut picker = TorrentPicker::new(ctx, key.to_owned(), "magnet:01234567".to_owned(), 0);
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| picker.render(frame, ctx).expect("the picker renders"))
            .expect("draw ok");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_picker_renders_the_rows_and_the_gated_stream_button() {
        let mut ctx = test_ctx();
        let screen = render_picker(&mut ctx, Some(files()), false);
        let joined = screen.join("\n");
        // Media rows only, with sizes, and a mark.
        assert!(joined.contains("ep1.mkv"), "{joined}");
        assert!(joined.contains("ep2.mkv"), "{joined}");
        assert!(!joined.contains("info.nfo"), "non-media files are not rows: {joined}");
        assert!(joined.contains("(100 B)") || joined.contains("100 B"), "{joined}");
        // The buttons: Cancel, the spinning Stream, Download.
        assert!(joined.contains("[ Cancel ]"), "{joined}");
        assert!(joined.contains("[ Download ]"), "{joined}");
        assert!(
            joined.contains(&format!("[ Stream {} ]", SPINNER_FRAMES[0])),
            "Stream shows the spinner while gated: {joined}"
        );
        assert!(joined.contains("Pack"), "the torrent name titles the modal: {joined}");
    }

    #[test]
    fn a_live_torrent_renders_a_plain_stream_button() {
        let mut ctx = test_ctx();
        let screen = render_picker(&mut ctx, None, true);
        let joined = screen.join("\n");
        assert!(joined.contains("[ Stream ]"), "Stream is plain once live: {joined}");
        assert!(
            !joined.contains(&format!("Stream {}", SPINNER_FRAMES[0])),
            "no spinner once live: {joined}"
        );
        assert!(joined.contains("ep1.mkv"), "{joined}");
    }

    #[test]
    fn before_the_metainfo_the_window_shows_the_label_loading_and_no_buttons() {
        let mut ctx = test_ctx();
        let screen = render_picker(&mut ctx, None, false);
        let joined = screen.join("\n");
        // The pasted label is what the user recognises, with its wait state.
        assert!(joined.contains("magnet:01234567"), "{joined}");
        assert!(joined.contains("loading…"), "{joined}");
        assert!(joined.contains("esc to cancel"), "{joined}");
        assert!(!joined.contains("ep1.mkv"), "no rows before phase 3: {joined}");
        // No buttons exist until the file list lands.
        assert!(!joined.contains("[ Stream"), "{joined}");
        assert!(!joined.contains("Download ]"), "{joined}");
        assert!(!joined.contains("[ Cancel ]"), "{joined}");
    }

    #[test]
    fn the_window_grows_to_two_thirds_and_no_further() {
        // 30 rows on a 20-line terminal: the window is capped at 2/3 of the
        // screen and the list scrolls inside it.
        let many: Vec<crate::core::torrent::ScannedFile> = (0..30)
            .map(|i| crate::core::torrent::ScannedFile {
                index: i,
                name: format!("ep{i:02}.mkv"),
                length: 1024,
            })
            .collect();
        let mut ctx = test_ctx();
        let key = "magnet:?xt=urn:btih:0123456789abcdef";
        ctx.torrent_file_lists
            .borrow_mut()
            .insert(key.to_owned(), ("Pack".to_owned(), many));
        *ctx.torrent_picker_key.borrow_mut() = Some(key.to_owned());
        let mut picker = TorrentPicker::new(&ctx, key.to_owned(), "magnet:01234567".to_owned(), 0);
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| picker.render(frame, &mut ctx).expect("renders"))
            .expect("draw ok");
        let buffer = terminal.backend().buffer().clone();
        let lines: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        // Find the window's vertical extent by its rounded border corners.
        let top = lines.iter().position(|l| l.contains('╭')).expect("the window has a top border");
        let bottom =
            lines.iter().rposition(|l| l.contains('╰')).expect("the window has a bottom border");
        let height = bottom - top + 1;
        let two_thirds = 20 * 2 / 3;
        assert_eq!(
            height, two_thirds,
            "a long list fills the cap exactly (2/3 of 20 lines): {height}, screen:\n{}",
            lines.join("\n")
        );
        // The rows scroll inside the cap rather than overflowing it.
        assert!(lines.iter().any(|l| l.contains("ep00.mkv")), "the first row is visible");
        assert!(
            !lines.iter().any(|l| l.contains("ep29.mkv")),
            "the last row is scrolled out of the capped window"
        );
    }

    #[test]
    fn the_list_sits_in_the_outline_and_the_buttons_below_it() {
        // Two files: the window holds ONLY the rows (rows + two border
        // lines), and the button row is drawn underneath the border.
        let mut ctx = test_ctx();
        let screen = render_picker(&mut ctx, Some(files()), false);
        let top = screen.iter().position(|l| l.contains('╭')).expect("top border");
        let bottom = screen.iter().rposition(|l| l.contains('╰')).expect("bottom border");
        let height = bottom - top + 1;
        assert_eq!(height, 4, "two rows + two borders, the buttons are outside");

        // The rows are inside the box…
        for (offset, row) in ["ep1.mkv", "ep2.mkv"].iter().enumerate() {
            let line = &screen[top + 1 + offset];
            assert!(line.contains(row), "row {row} is inside the outline: {line}");
            assert!(line.contains('│'), "the row is bounded by the window: {line}");
        }
        // …and the buttons are on the line BELOW the bottom border, outside it.
        let buttons = &screen[bottom + 1];
        assert!(buttons.contains("[ Stream"), "buttons below the window: {buttons}");
        assert!(buttons.contains("[ Download ]"), "{buttons}");
        assert!(buttons.contains("[ Cancel ]"), "{buttons}");
        assert!(!buttons.contains('│'), "the button row is outside the border: {buttons}");
    }

    #[test]
    fn the_actions_are_left_and_cancel_is_right() {
        let mut ctx = test_ctx();
        let screen = render_picker(&mut ctx, Some(files()), false);
        let bottom = screen.iter().rposition(|l| l.contains('╰')).expect("bottom border");
        let buttons = &screen[bottom + 1];
        let stream_at = buttons.find("[ Stream").expect("Stream present");
        let download_at = buttons.find("[ Download ]").expect("Download present");
        let cancel_at = buttons.find("[ Cancel ]").expect("Cancel present");
        assert!(stream_at < download_at, "Stream is left of Download: {buttons}");
        assert!(download_at < cancel_at, "Download is left of Cancel: {buttons}");
        // Cancel sits at the far right of the row.
        assert_eq!(buttons.trim_end().len(), cancel_at + "[ Cancel ]".len(), "{buttons}");
    }

    #[test]
    fn stream_routes_through_the_downloader_by_default() {
        // `torrent.stream_downloads` defaults to on, so a Stream becomes a
        // committed download and the torrent shows up in Downloads.
        let ctx = test_ctx();
        assert!(ctx.config.torrent.stream_downloads, "on by default");
        assert!(streams_download(&ctx), "Stream commits a download job");
    }

    #[test]
    fn turning_the_option_off_keeps_stream_plain() {
        let mut config = (*test_ctx().config).clone();
        config.torrent.stream_downloads = false;
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let ctx = Ctx { config: std::sync::Arc::new(config), ..ctx };
        assert!(!streams_download(&ctx), "Stream is a plain throw-away stream");
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 3 / 2), "1.5 GB");
    }

    #[test]
    fn the_wait_counter_is_minutes_and_seconds() {
        assert_eq!(mmss(0), "00:00");
        assert_eq!(mmss(12), "00:12");
        assert_eq!(mmss(61), "01:01");
        assert_eq!(mmss(3600), "60:00");
    }
}

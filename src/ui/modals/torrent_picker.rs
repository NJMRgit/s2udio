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
//! until the engine reports the torrent live, and clears in place (the modal
//! refreshes under the `torrent_picker` replacement id) the moment phase 4
//! lands.
//!
//! **The animation is decoupled from the rebuild.** The spinner frame is read
//! off the wall clock (the app's shared braille spinner, `SPINNER_FRAME_MS` a
//! frame) and advanced by a plain 120 ms redraw tick — a repaint, not a new
//! modal. A rebuild happens only when what the window is BUILT from changes:
//! a scan event (the rows appearing, Stream un-gating) or the wait counter
//! advancing by a second. Animating by rebuilding was what made the spinner
//! crawl (two frames a second) and what reset the user's cursor on every
//! frame.
//!
//! Layout: the list, then one footer row — `[ ⟳ Stream ]  [ Download ]`
//! packed left, and `[ Cancel ]` glued right, BELOW the window border.
//!
//! Selection: a click on a row's tick glyph (or anywhere on the row) or
//! `Space` toggles that file; `Enter` leaves the list for the footer buttons,
//! landing on `Stream` (Left/Right walk them, a click runs one). The cursor
//! and the ticks are the user's state, held in `Ctx.torrent_picker_ui` and
//! re-applied on every rebuild — see [`PickerUi`].

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

/// The braille spinner the gated Stream button cycles through — the SAME
/// frames and step as the app's other spinners (the queue's Duration column,
/// the Jellyfin loading indicator): [`SPINNER_FRAME_MS`] per frame, chosen
/// from wall-clock time.
pub(crate) const SPINNER_FRAMES: [&str; 10] =
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The label of the Stream button. `None` = live, the plain label; `Some(f)`
/// = still waiting on phase 4, showing frame `f` of the spinner.
fn stream_label(spinner: Option<usize>) -> String {
    match spinner {
        Some(frame) => format!("Stream {}", SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]),
        None => "Stream".to_owned(),
    }
}

/// The picker's animation frame right now, from the wall clock like every
/// other spinner ([`crate::ui::panes::queue::spinner_frame`]): independent of
/// renders, of scan events and of a dropped tick, so the animation never
/// stutters when a rebuild is skipped or bunched.
fn spinner_frame_now() -> usize {
    let step = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
        / crate::ui::panes::queue::SPINNER_FRAME_MS) as usize;
    step % SPINNER_FRAMES.len()
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

/// What the user has done in the picker — carried across the in-place
/// rebuilds through `Ctx.torrent_picker_ui`, because every rebuild constructs
/// a fresh `MenuModal` whose rows start fully ticked with the cursor on the
/// first row. Without the carriage the once-a-second refresh (and every scan
/// event) handed back a list that re-ticked whatever the user had just
/// unticked: "clicking an item unselects it briefly, then it selects itself
/// again".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickerUi {
    /// The cursor row (`None` = the list's own default).
    pub cursor: Option<usize>,
    /// The labels of the ticked rows. Empty = nothing ticked; the pair with
    /// an untouched `cursor`/`buttons_focused` means "the user has not
    /// touched the picker yet", which keeps the all-ticked default.
    pub ticks: Vec<String>,
    /// Whether the footer buttons hold the focus (`Enter` landed there).
    pub buttons_focused: bool,
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
    pub fn from_inputs(inputs: TorrentPickerInputs) -> Self {
        if let Some((name, files)) = inputs.live {
            let rows = media_rows(&files);
            return Self {
                rows,
                pending_key: None,
                error: None,
                elapsed_secs: 0,
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
            live: false,
            torrent_name,
        }
    }

    /// Resolves the picker's state for the torrent `key` out of `Ctx`.
    /// The borrows are all cloned first — a `Ref` held across the next
    /// `borrow()` would panic on the same `RefCell` (the round-55 F1 class
    /// of bug).
    pub fn resolve(ctx: &Ctx, key: &str) -> Self {
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
        let mut state =
            Self::from_inputs(TorrentPickerInputs { live, listed, error, elapsed_secs });
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
    /// The open picker's user state (cursor, ticks, button focus). `None`
    /// until the user touches the list — the "all files ticked" default is
    /// not a state to carry back.
    vis: Option<PickerUi>,
    /// Phase 4 is pending: the Stream button spins (and, before the file list
    /// lands, the wait window counts up). Kept on the picker because the
    /// spinner's tick is re-armed from `render`, long after `new` returned.
    spins: bool,
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
    pub(crate) fn new(ctx: &Ctx, key: String, fallback_label: String) -> Self {
        let state = TorrentPickerState::resolve(ctx, &key);
        let stream_ready = state.stream_enabled();
        // Phase 4 pending = the Stream button spins (and the wait window
        // counts up): the picker needs its animation tick.
        let spins = !state.live;
        let rows = state.rows.clone();
        let waiting = state.waiting_for_metainfo();
        let has_rows = state.has_rows();
        let error = state.error.clone();
        let elapsed = state.elapsed_secs;
        // The gated Stream button's frame comes from the wall clock here (see
        // `spinner_frame_now`), so a rebuild is never what advances it.
        let spinner_frame = spinner_frame_now();
        let key_for_confirm = key.clone();
        // The user's state from the picker this one replaces (the rebuilds
        // that land on the same replacement id). Held in `Ctx` because this
        // build runs inside a closure the previous instance is already gone
        // from; `None` = nothing touched yet.
        let carried = ctx.torrent_picker_ui.borrow().clone();
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
                        stream_label(if stream_ready { None } else { Some(spinner_frame) }),
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
        let mut picker = Self { inner, vis: carried.clone(), spins };
        // The user's ticks and cursor come back on every rebuild — AFTER
        // `build()`, which runs `section.down()`: restoring inside the section
        // builder put the cursor back and `down()` then moved it off that row
        // (and unselected it outright when the row was the last one), so the
        // cursor still jumped on every refresh.
        if let Some(carried) = &carried
            && let Some(section) = picker.inner.check_section()
        {
            section.restore_picker_ui(carried);
        }
        // The carried state is authoritative for this build — do NOT record
        // the fresh section's state here. `MenuModal::build()` runs
        // `section.down()`, which leaves the cursor on row 0 and the buttons
        // unfocused; capturing that would write the builder's defaults over
        // whatever `restore_picker_ui` just re-applied, and the user's cursor
        // went back to the top on every rebuild.
        if carried.is_none() {
            picker.capture_vis(ctx);
        }
        // The spinner's redraw tick lives exactly as long as the spin: armed
        // on every build while phase 4 is pending, dropped the moment the
        // picker stops spinning (the torrent went live) or closes
        // (`clear_picker`).
        // Arm the spinner's tick for a picker that is still waiting; a live
        // one leaves nothing running.
        request_spinner_frames(ctx, spins);
        picker
    }

    /// Records what the user has done with the rows (cursor, ticks, button
    /// focus) in `Ctx.torrent_picker_ui`, where the next in-place rebuild
    /// reads it. Runs once before every key / click is handled, so the map is
    /// current when a scan event rebuilds the picker in the same frame.
    ///
    /// A picker with no file list (the wait window, a failed scan) has no
    /// state to carry, so it asks for nothing.
    fn capture_vis(&mut self, ctx: &Ctx) {
        // The menu reports the state of its own list section (`None` while the
        // picker is a wait window with plain header rows).
        let Some(vis) = self.inner.rows_vis() else { return };
        self.vis = Some(vis.clone());
        *ctx.torrent_picker_ui.borrow_mut() = Some(vis);
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

/// Rebuild the open picker in place (the `torrent_picker` replacement id
/// swaps it for the new one at the same spot). Called when a scan lands
/// (phase 3 → the rows appear, phase 4 → Stream un-gates) or when the wait
/// counter changes — the state the window is built from.
///
/// The **spinner** does not come through here: it is read off the wall clock
/// at build time and animated by a plain redraw tick
/// ([`schedule_spinner_tick`]), so the animation costs a repaint rather than
/// a rebuilt modal and never resets anything the user is doing.
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
    let picker = TorrentPicker::new(ctx, key, label);
    crate::shared::macros::modal!(ctx, picker);
}

/// Asks the shared frame clock for the picker's spinner cadence while the
/// Stream button spins — the same shape as the Queue pane's Duration spinner,
/// the Controls pane's `▶▶` spinner and the Jellyfin loading indicator, all of
/// which now go through `Ctx::animator` too.
///
/// The clock is armed by the event loop from the shortest request a frame
/// made, so there is no job id to hold here and no `cancel` path: a picker
/// that stops spinning simply stops asking, and `finish_frame` disarms the
/// tick. It is called from `TorrentPicker::render` on every frame.
///
/// Regression (2026-10-23, "the spinner is still too slow"): the tick used to
/// be armed once, when the picker was BUILT, by a surface that never asked
/// again — it fired a single `RequestRender` and stopped, so the glyph advanced
/// about once per scan event (≈1 frame/s, the crawl the report described).
/// Asking per frame is what keeps the cadence alive; the shared clock cannot
/// regress the same way because a frame that asks nothing gets no tick.
fn request_spinner_frames(ctx: &Ctx, spinning: bool) {
    use crate::ui::panes::queue::SPINNER_FRAME_MS;
    if spinning {
        ctx.animator.request_every(std::time::Duration::from_millis(SPINNER_FRAME_MS));
    }
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
    // A picker opened later starts from its own defaults (every file ticked),
    // not from the ticks of the torrent that was on screen a moment ago.
    ctx.torrent_picker_ui.borrow_mut().take();
    // Nothing is on screen to animate any more: the clock is only armed by a
    // frame that asked for one, and the next frame asks nothing (there is no
    // job id to cancel here any more).
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
        // Keep the spinner's tick alive: this frame asks for the next one, so
        // the animation runs at `SPINNER_FRAME_MS` for as long as the picker
        // waits on phase 4 — no rebuild involved.
        request_spinner_frames(ctx, self.spins);
        self.inner.render(frame, ctx)
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &Ctx) -> Result<()> {
        self.inner.handle_insert_mode(kind, ctx)
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        self.inner.handle_key(key, ctx)?;
        // Record what the key did, so a scan event landing right after it
        // rebuilds onto the user's rows and not onto the builder's defaults.
        self.capture_vis(ctx);
        Ok(())
    }

    fn handle_raw_key(&mut self, key: crossterm::event::KeyEvent, ctx: &mut Ctx) -> Result<bool> {
        self.inner.handle_raw_key(key, ctx)
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        // A left click is what toggles a row, and it happens on the PRESS
        // (`ListSection::left_click`): the state is recorded right after it,
        // so a rebuild landing between the press and the release sees the
        // user's ticks rather than the pre-click ones.
        self.inner.handle_mouse_event(event, ctx)?;
        if matches!(event.kind, crate::shared::mouse_event::MouseEventKind::LeftClick) {
            self.capture_vis(ctx);
        }
        Ok(())
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
        let state = TorrentPickerState::from_inputs(inputs(false, true, None));
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
        let state = TorrentPickerState::from_inputs(inputs(true, true, None));
        assert_eq!(state.rows.len(), 2);
        assert!(state.stream_enabled(), "live torrents may stream");
        assert!(state.has_rows());
        assert!(state.live, "phase 4 landed: the picker stops spinning");
    }

    #[test]
    fn phase_four_alone_still_lists_the_files() {
        // `TorrentScanned` carries the file list too, so a scan that lands in
        // one poll (a local .torrent: phases 3 and 4 together) is complete.
        let state = TorrentPickerState::from_inputs(inputs(true, false, None));
        assert_eq!(state.rows.len(), 2);
        assert!(state.stream_enabled());
    }

    #[test]
    fn before_metainfo_it_waits_without_rows() {
        // Nothing landed: the picker shows "Reading torrent…", no rows, and
        // neither button can do anything.
        let state = TorrentPickerState::from_inputs(inputs(false, false, None));
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
            TorrentPickerState::from_inputs(inputs(false, false, Some("No peers found")));
        assert_eq!(state.error.as_deref(), Some("No peers found"));
        assert!(!state.stream_enabled());
        assert!(!state.has_rows());
        assert!(!state.waiting_for_metainfo(), "a failure is not a wait");
    }

    #[test_case(false, Some(0), false ; "a pending scan keeps the spinner")]
    #[test_case(false, None, false ; "a list without live still cannot stream")]
    #[test_case(true, Some(0), true ; "live wins over a stale spinner")]
    #[test_case(true, None, true ; "live and settled")]
    fn stream_gate_follows_the_live_phase(live: bool, _spinner: Option<usize>, expect: bool) {
        let state = TorrentPickerState::from_inputs(inputs(live, true, None));
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

    /// The picker's spinner is the app's own: the same braille frames and
    /// the same step as the queue's Duration spinner and the Jellyfin loading
    /// indicator. It used to have an 8-frame set advanced once per rebuild,
    /// which both dropped glyphs and crawled.
    #[test]
    fn the_picker_spins_like_every_other_spinner() {
        assert_eq!(
            SPINNER_FRAMES.as_slice(),
            crate::ui::panes::queue::SPINNER_FRAMES.as_slice(),
            "the picker shares the house frames"
        );
        assert_eq!(
            crate::ui::panes::queue::SPINNER_FRAME_MS,
            120,
            "the house step the picker's tick re-arms with"
        );
    }

    /// The frame is read from the wall clock, so two labels built within the
    /// same step agree and the animation does not depend on how often a
    /// rebuild happens.
    #[test]
    fn the_frame_follows_the_clock_not_the_rebuild_count() {
        let first = spinner_frame_now();
        let second = spinner_frame_now();
        assert_eq!(first, second, "same clock step, same frame");
        assert!(first < SPINNER_FRAMES.len());
        // A wall-clock frame is the step number modulo the frame count.
        let step = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            / crate::ui::panes::queue::SPINNER_FRAME_MS) as usize;
        assert_eq!(first, step % SPINNER_FRAMES.len());
    }

    /// A rebuild of the picker — whatever triggered it — never moves the
    /// user's cursor: the state is carried, not re-derived from defaults.
    #[test]
    fn a_rebuild_never_moves_the_cursor() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        let mut down = key(crate::config::keys::CommonAction::Down);
        let mut picker = open(&ctx);
        picker.handle_key(&mut down, &mut ctx).expect("down");
        let cursor_before = picker.inner.rows_vis().map(|vis| vis.cursor);
        assert_eq!(cursor_before, Some(Some(1)));

        // Whatever the spinner is doing, a rebuild starts from `Ctx`'s state.
        for _ in 0..3 {
            let rebuilt = open(&ctx);
            assert_eq!(
                rebuilt.inner.rows_vis().map(|vis| vis.cursor),
                cursor_before,
                "the cursor survives the rebuild"
            );
        }
    }

    /// The spinner really repeats: the picker asks the shared frame clock on
    /// every frame, that frame's end arms one scheduler tick, the tick asks
    /// for a render, and the glyph advances every 120 ms for as long as phase 4
    /// is pending.
    ///
    /// Regression (2026-10-23, user report "the spinner is still too slow"):
    /// the tick was armed ONLY when the picker was built. A scheduled job is
    /// `FnOnce`, and its only effect is to request a render, so it fired once
    /// and stopped — the glyph then moved at the rate of scan events (≈1/s)
    /// instead of 8/s. This test drives a REAL, started scheduler and counts
    /// the ticks, so it fails if the picker stops asking from `render` or if
    /// `Ctx::finish_frame` stops arming the clock.
    #[test]
    fn the_spinner_tick_keeps_firing_across_frames() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        seed_listed(&ctx);
        // Start the fixture's scheduler: the tick only repeats if something
        // actually runs the jobs.
        let mut ctx = ctx;
        ctx.scheduler.start();

        let mut picker = open(&ctx);
        assert!(picker.spins, "the picker waits on phase 4");

        // Each pass: the picker asks while it renders, the end of the frame
        // arms the clock, and the clock's job asks for the next render.
        let mut rounds = 0;
        for _ in 0..40 {
            draw(&mut picker, &mut ctx);
            ctx.finish_frame();
            match app_rx.recv_timeout(std::time::Duration::from_millis(400)) {
                Ok(crate::AppEvent::RequestRender) => rounds += 1,
                Ok(_) => continue,
                Err(_) => break,
            }
            if rounds >= 3 {
                break;
            }
        }
        assert!(
            rounds >= 3,
            "the shared clock must keep asking for frames while the spinner \
             spins (got {rounds} rounds); a build-time-only arm stops after one"
        );
    }

    /// Once the torrent goes live there is nothing left to animate: the picker
    /// asks for no frames and the shared clock arms nothing, so the session
    /// stops scheduling redraws on its own (there is no cancel path to forget).
    #[test]
    fn a_live_picker_leaves_no_tick_running() {
        let ctx = test_ctx();
        seed_listed(&ctx);
        seed_live(&ctx);
        let mut picker = open(&ctx);
        assert!(!picker.spins, "a live picker does not spin");
        let mut ctx = ctx;
        // The build-time request is one pass old by now: consume it the way a
        // frame would, then draw the live picker.
        ctx.finish_frame();
        draw(&mut picker, &mut ctx);
        assert!(
            !ctx.animator.wants_frames(),
            "a live picker asks for no frames"
        );
        ctx.finish_frame();
        assert!(
            ctx.animator.idle().is_none(),
            "no tick is armed once the spinner stops"
        );
    }

    /// The reported cursor reset: `Enter` puts the focus on the buttons, then
    /// the spinner repaints (a redraw tick, NOT a rebuild) — and the cursor
    /// must still be where the user left it, with the buttons still focused.
    ///
    /// A repaint never touches the picker's state at all, which is the whole
    /// point of decoupling the animation from the rebuild.
    #[test]
    fn a_spinner_repaint_leaves_the_cursor_and_button_focus_alone() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        seed_live(&ctx);
        let mut picker = open(&ctx);
        // Move down a row, then Enter to the buttons.
        let mut down = key(crate::config::keys::CommonAction::Down);
        picker.handle_key(&mut down, &mut ctx).expect("down");
        let mut enter = key(crate::config::keys::CommonAction::Confirm);
        picker.handle_key(&mut enter, &mut ctx).expect("enter");
        let focused = picker.inner.rows_vis().map(|vis| vis.buttons_focused);
        assert_eq!(focused, Some(true), "Enter focused the buttons");

        let cursor_before = picker.inner.rows_vis().map(|vis| vis.cursor);
        // The spinner's repaints: many frames, no rebuild.
        for _ in 0..10 {
            draw(&mut picker, &mut ctx);
        }
        assert_eq!(
            picker.inner.rows_vis().map(|vis| vis.cursor),
            cursor_before,
            "a repaint never moves the cursor"
        );
        assert_eq!(
            picker.inner.rows_vis().map(|vis| vis.buttons_focused),
            Some(true),
            "a repaint never drops the button focus"
        );
        // And the state the next rebuild would carry is the same.
        assert_eq!(
            ctx.torrent_picker_ui.borrow().as_ref().map(|vis| vis.cursor),
            cursor_before,
            "and the carried state agrees"
        );
    }

    /// A rebuild while the BUTTONS hold the focus must keep both the cursor
    /// row and the focus. This is the case the in-place hand-off used to drop:
    /// the rebuilt modal starts with its section unselected, so a state
    /// captured "from the active section" came back empty and the fresh modal
    /// put the cursor on row 0.
    #[test]
    fn a_rebuild_keeps_the_cursor_while_the_buttons_hold_the_focus() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        seed_live(&ctx);
        let mut picker = open(&ctx);
        let mut down = key(crate::config::keys::CommonAction::Down);
        picker.handle_key(&mut down, &mut ctx).expect("down");
        let mut enter = key(crate::config::keys::CommonAction::Confirm);
        picker.handle_key(&mut enter, &mut ctx).expect("enter");

        let rebuilt = open(&ctx);
        assert_eq!(
            rebuilt.inner.rows_vis().map(|vis| vis.cursor),
            Some(Some(1)),
            "the cursor row survives a rebuild made while the buttons are focused"
        );
        assert_eq!(
            rebuilt.inner.rows_vis().map(|vis| vis.buttons_focused),
            Some(true),
            "and so does the button focus"
        );
    }

    /// The picker hands over the torrent's file indices of the TICKED rows —
    /// for a season where every episode but the first is ticked, all of them
    /// except index 0.
    #[test]
    fn the_confirm_hands_over_the_ticked_torrent_indices() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        let mut picker = open(&ctx);
        // Untick row 0 (ep1.mkv).
        let mut space = key_global(crate::config::keys::GlobalAction::TogglePause);
        picker.handle_key(&mut space, &mut ctx).expect("space");
        let section = picker.inner.check_section().expect("rows");
        let checked = section.checked_indices();
        assert_eq!(checked, vec![1], "only the second row stays ticked");

        // Now the flow the user actually goes through: several rebuilds (the
        // scan re-reporting, the spinner) before the click. Each rebuild must
        // keep the ticks.
        for round in 0..3 {
            let rebuilt = open(&ctx);
            let mut rebuilt = rebuilt;
            let sec = rebuilt.inner.check_section().expect("rows");
            let checked = sec.checked_indices();
            let labels: Vec<String> =
                checked.iter().map(|i| sec.items[*i].label.clone()).collect();
            println!("DBG round {round}: checked={checked:?} labels={labels:?}");
            assert_eq!(checked, vec![1], "round {round} keeps only row 1 ticked");
            // The user's next move happens on this rebuild.
        }
    }

    /// END TO END with EIGHT episodes (the user's torrent): untick the first
    /// row by clicking its glyph, then click `[ Download ]`. What the confirm
    /// hands over is what the download will contain.
    #[test]
    fn clicking_download_with_eight_rows_and_row_zero_unticked() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<Vec<usize>>>> = Arc::new(Mutex::new(Vec::new()));

        let mut ctx = test_ctx();
        let eight: Vec<ScannedFile> = (0..8)
            .map(|i| ScannedFile {
                index: i,
                name: format!("Lanterns.S01E0{}.2160p.mkv", i + 1),
                length: 8_800_000_000,
            })
            .collect();
        ctx.torrent_file_lists
            .borrow_mut()
            .insert(KEY.to_owned(), ("Pack".to_owned(), eight.clone()));
        *ctx.torrent_picker_key.borrow_mut() = Some(KEY.to_owned());

        // Build the picker with an interceptor on the check_list callback so
        // we see EXACTLY what the confirm passes on.
        let seen_cb = seen.clone();
        let rows: Vec<(usize, String, u64)> = eight
            .iter()
            .map(|f| (f.index, f.name.clone(), f.length))
            .collect();
        let mut picker = TorrentPicker::new(&ctx, KEY.to_owned(), "magnet:x".to_owned());
        {
            let section = picker.inner.check_section().expect("rows");
            let rows_cb = rows.clone();
            section.check_list(move |_ctx, selected, _choice| {
                let indices: Vec<usize> = selected
                    .iter()
                    .filter_map(|row| rows_cb.get(*row).map(|(index, _, _)| *index))
                    .collect();
                seen_cb.lock().unwrap().push(indices);
                Ok(())
            });
        }
        draw(&mut picker, &mut ctx);

        // Click row 0's glyph to untick it.
        let row0 = picker.inner.check_section().unwrap().item_area(0).expect("row 0");
        let click = crate::shared::mouse_event::MouseEvent {
            x: row0.x + 1,
            y: row0.y,
            kind: MouseEventKind::LeftClick,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        picker.handle_mouse_event(click, &mut ctx).expect("click row 0");
        draw(&mut picker, &mut ctx);

        let button = picker.inner.check_section().unwrap().button_area_for_test(1).expect("download");
        let click = crate::shared::mouse_event::MouseEvent {
            x: button.x + 1,
            y: button.y,
            kind: MouseEventKind::LeftClick,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        picker.handle_mouse_event(click, &mut ctx).expect("click download");

        let got = seen.lock().unwrap().clone();
        println!("DBG confirm got {got:?}");
        assert_eq!(
            got,
            vec![vec![1, 2, 3, 4, 5, 6, 7]],
            "the confirm hands over episodes 2-8"
        );
    }

    // ---- rendering (headless, through a TestBackend) ----

    /// A `Ctx` whose app-event receiver stays alive: the key handlers call
    /// `ctx.render()`, which sends on it.
    fn test_ctx() -> Ctx {
        let (sender, receiver) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (sender, receiver.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        Box::leak(Box::new(receiver));
        ctx
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
        let mut picker = TorrentPicker::new(ctx, key.to_owned(), "magnet:01234567".to_owned());
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
        // The frame is wall-clock driven, so the render is checked against
        // the frame the clock is on — any of them is a valid spinner frame.
        assert!(
            SPINNER_FRAMES.iter().any(|frame| joined.contains(&format!("[ Stream {frame} ]"))),
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
            !SPINNER_FRAMES.iter().any(|frame| joined.contains(&format!("Stream {frame}"))),
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
        let mut picker = TorrentPicker::new(&ctx, key.to_owned(), "magnet:01234567".to_owned());
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

    // ---- the rows are the user's: ticks, cursor and Enter survive a rebuild ----

    use std::sync::Arc;

    use crate::shared::keys::ActionEvent as Key;
    use crate::shared::mouse_event::MouseEventKind;

    /// The picker's checkbox list (its file rows).
    fn picker_section<'p, 'a>(
        picker: &'p mut TorrentPicker<'a>,
    ) -> &'p mut crate::ui::modals::menu::ListSection {
        picker.inner.check_section().expect("the picker has a file list")
    }

    fn ticks(picker: &mut TorrentPicker<'_>) -> Vec<bool> {
        picker_section(picker)
            .items
            .iter()
            .map(|item| item.checked.expect("a checkbox row"))
            .collect()
    }

    fn key(action: crate::config::keys::CommonAction) -> Key {
        Key::from(Arc::new(vec![crate::shared::keys::Actions::Common(action)]))
    }

    /// `Space`: the menu honours the global TogglePause binding as the tick
    /// key, whatever the keymap calls it.
    fn key_global(action: crate::config::keys::GlobalAction) -> Key {
        Key::from(Arc::new(vec![crate::shared::keys::Actions::Global(action)]))
    }

    /// The picker opens with every file ticked and the cursor on the first
    /// row.
    #[test]
    fn the_picker_opens_with_every_file_ticked() {
        let ctx = test_ctx();
        seed_listed(&ctx);
        let mut picker = open(&ctx);
        assert_eq!(ticks(&mut picker), vec![true, true], "all media ticked");
        assert_eq!(picker.inner.rows_vis().map(|vis| vis.cursor), Some(Some(0)));
    }

    /// Pressing `Space` unticks the cursor row — and the next rebuild (the
    /// once-a-second scan refresh) keeps it unticked. This is the round
    /// report: the tick used to come back on the next refresh.
    #[test]
    fn an_unticked_row_stays_unticked_across_a_rebuild() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        let mut picker = open(&ctx);
        let mut space = key_global(crate::config::keys::GlobalAction::TogglePause);
        picker.handle_key(&mut space, &mut ctx).expect("space toggles");
        assert_eq!(ticks(&mut picker), vec![false, true], "the first row unticks");

        // A scan event rebuilds the picker in place.
        let mut rebuilt = open(&ctx);
        assert_eq!(
            ticks(&mut rebuilt),
            vec![false, true],
            "the rebuild keeps the user's ticks"
        );
    }

    /// A click on the tick glyph toggles the row and stays toggled once the
    /// picker is rebuilt (the press captured the state before it flipped).
    #[test]
    fn a_clicked_tick_survives_the_next_rebuild() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        let mut picker = open(&ctx);
        // Draw once so the list has its geometry, then press the second row's
        // glyph.
        draw(&mut picker, &mut ctx);
        let row = {
            let section = picker_section(&mut picker);
            section.item_area(1).expect("the second row is visible")
        };
        let press = crate::shared::mouse_event::MouseEvent {
            x: row.x + 1,
            y: row.y,
            kind: MouseEventKind::LeftClick,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        picker.handle_mouse_event(press, &mut ctx).expect("the click lands");
        assert_eq!(ticks(&mut picker), vec![true, false], "the clicked row unticks");

        let mut rebuilt = open(&ctx);
        assert_eq!(
            ticks(&mut rebuilt),
            vec![true, false],
            "the rebuild keeps the clicked row unticked"
        );
    }

    /// `Enter` does not tick the row: it moves the focus onto the footer
    /// buttons, starting at `Stream` (the first one).
    #[test]
    fn enter_moves_to_the_buttons_on_stream() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        // A live torrent so Stream is enabled and therefore the focus lands.
        seed_live(&ctx);
        let mut picker = open(&ctx);
        let mut enter = key(crate::config::keys::CommonAction::Confirm);
        picker.handle_key(&mut enter, &mut ctx).expect("enter");
        assert_eq!(ticks(&mut picker), vec![true, true], "Enter never toggles a row");
        assert!(picker_section(&mut picker).buttons_focused(), "the focus is on the buttons");
        assert!(
            picker.vis.as_ref().is_some_and(|vis| vis.buttons_focused),
            "the button focus is carried for the next rebuild"
        );
    }

    /// Down/Space drive the rows: Down moves the cursor without ticking it.
    #[test]
    fn down_moves_the_cursor_and_space_ticks_where_it_lands() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        let mut picker = open(&ctx);
        let mut down = key(crate::config::keys::CommonAction::Down);
        picker.handle_key(&mut down, &mut ctx).expect("down");
        assert_eq!(picker.inner.rows_vis().map(|vis| vis.cursor), Some(Some(1)));
        assert_eq!(ticks(&mut picker), vec![true, true], "moving ticks nothing");
        let mut space = key_global(crate::config::keys::GlobalAction::TogglePause);
        picker.handle_key(&mut space, &mut ctx).expect("space");
        assert_eq!(ticks(&mut picker), vec![true, false]);

        let mut rebuilt = open(&ctx);
        assert_eq!(ticks(&mut rebuilt), vec![true, false], "and it survives the rebuild");
        assert_eq!(
            rebuilt.inner.rows_vis().map(|vis| vis.cursor),
            Some(Some(1)),
            "the cursor stays on the row the user left it on"
        );
    }

    /// Closing the picker forgets its rows: a later torrent starts fully
    /// ticked again.
    #[test]
    fn closing_the_picker_forgets_its_ticks() {
        let mut ctx = test_ctx();
        seed_listed(&ctx);
        let mut space = key_global(crate::config::keys::GlobalAction::TogglePause);
        let mut picker = open(&ctx);
        picker.handle_key(&mut space, &mut ctx).expect("space");
        assert!(ctx.torrent_picker_ui.borrow().is_some());
        clear_picker(&ctx, KEY);
        assert!(
            ctx.torrent_picker_ui.borrow().is_none(),
            "the carried rows are dropped with the picker"
        );
    }

    const KEY: &str = "magnet:?xt=urn:btih:0123456789abcdef";

    /// The phase-3 file list for [`KEY`].
    fn seed_listed(ctx: &Ctx) {
        ctx.torrent_file_lists
            .borrow_mut()
            .insert(KEY.to_owned(), ("Pack".to_owned(), files()));
    }

    /// The phase-4 scan for [`KEY`] (no engine: the picker reads the list).
    fn seed_live(ctx: &Ctx) {
        ctx.torrent_scans.borrow_mut().insert(
            KEY.to_owned(),
            Ok(crate::core::torrent::TorrentScan {
                engine: None,
                torrent_id: "1".to_owned(),
                torrent_name: "Pack".to_owned(),
                info_hash: None,
                files: files(),
            }),
        );
    }

    fn open(ctx: &Ctx) -> TorrentPicker<'static> {
        *ctx.torrent_picker_key.borrow_mut() = Some(KEY.to_owned());
        TorrentPicker::new(ctx, KEY.to_owned(), "magnet:01234567".to_owned())
    }

    fn draw(picker: &mut TorrentPicker<'_>, ctx: &mut Ctx) {
        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| picker.render(frame, ctx).expect("the picker renders"))
            .expect("draw ok");
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

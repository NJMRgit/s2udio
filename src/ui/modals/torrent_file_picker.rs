use anyhow::Result;
use ratatui::Frame;

use super::{ListConfirm, ListModal, Modal};
use crate::{
    ctx::Ctx,
    shared::{id::Id, keys::ActionEvent, mouse_event::MouseEvent},
    ui::input::InputResultEvent,
};

/// The file-selection window's confirm actions (round 20): play the
/// marked files only, or also keep downloading them (the multi-file
/// "Download & Play" variant of the single-file "Play and Download").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentPickerAction {
    Play,
    DownloadAndPlay,
}

/// A multi-select file picker for torrents ("Select files…", round 17;
/// the window title is "▶ files — <name>" since round 22) — a Phase-3
/// thin adapter over `ListModal` (multi-select + marks): lists the
/// torrent's video files (name + size) and lets the user mark the ones
/// to play. Space (`CommonAction::Select`) toggles a mark, Enter
/// (`CommonAction::Confirm`) moves the cursor to the action buttons
/// (Play / Download & Play / Cancel) where a second Enter confirms the
/// focused one — Play and Download & Play play the marked files (all of
/// them when none is marked), Cancel closes.
#[derive(derive_more::Debug)]
pub struct TorrentFilePicker<
    'a,
    Callback: FnOnce(&Ctx, Vec<usize>, TorrentPickerAction) -> Result<()> + Send + Sync + 'a,
> {
    #[debug(skip)]
    inner: ListModal<'a, (usize, String, u64)>,
    #[debug(skip)]
    _callback: std::marker::PhantomData<Callback>,
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

impl<'a, Callback: FnOnce(&Ctx, Vec<usize>, TorrentPickerAction) -> Result<()> + Send + Sync + 'a>
    TorrentFilePicker<'a, Callback>
{
    pub fn new(
        ctx: &Ctx,
        title: impl Into<String>,
        options: Vec<(usize, String, u64)>,
        on_confirm: Callback,
    ) -> Self {
        fn row((_, name, len): &(usize, String, u64), marked: bool, _idx: usize) -> String {
            let mark = if marked { "✔ " } else { "  " };
            format!("{mark}{name}  ({})", format_bytes(*len))
        }
        fn size_fn(len: usize) -> (u16, u16) {
            // List (with its bordered block: +1 top border, +1 bottom
            // title) plus the button row. At least a handful of rows,
            // capped so a large file list scrolls instead of filling the
            // terminal.
            (70, (len as u16 + 5).min(18))
        }
        let inner = ListModal::builder()
            .ctx(ctx)
            .title(title.into())
            .options(options)
            .row_fn(row)
            .size_fn(size_fn)
            // Round 20: Play / Download & Play / Cancel — the middle
            // button also keeps the picked files (download job + move to
            // s2udio-downloads), like the single-file "Play and Download".
            .buttons(vec!["Play", "Download & Play", "Cancel"])
            .confirm_buttons(2)
            .multi_select(true)
            // The marked rows' *positional* file indices (`o.0`) — the
            // options list is name-sorted, so its own indices are NOT the
            // rqbit stream indices.
            .mark_id(|o: &(usize, String, u64)| o.0)
            // Round 22: one blank column between the file rows and the
            // scrollbar (list right padding 1; `scrollbar_area` keeps its
            // column so the thumb stays put).
            .list_right_padding(1)
            .bottom_title(|n| {
                format!(" {} marked · Space toggles · Enter: buttons · Esc cancels ", n)
            })
            .wheel_moves_selection(true)
            .scrollbar_drag(true)
            .on_confirm(move |ctx, confirm: ListConfirm<(usize, String, u64)>| {
                let action = if confirm.button == 0 {
                    TorrentPickerAction::Play
                } else {
                    TorrentPickerAction::DownloadAndPlay
                };
                (on_confirm)(ctx, confirm.marked, action)
            })
            .build();
        Self { inner, _callback: std::marker::PhantomData }
    }
}

impl<Callback: FnOnce(&Ctx, Vec<usize>, TorrentPickerAction) -> Result<()> + Send + Sync> Modal
    for TorrentFilePicker<'_, Callback>
{
    fn id(&self) -> Id {
        self.inner.id()
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

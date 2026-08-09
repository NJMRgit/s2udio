use std::collections::HashSet;

use anyhow::Result;
use ratatui::{
    Frame,
    layout::Rect,
    macros::constraint,
    prelude::{Constraint, Layout},
    style::{Style, Stylize},
    symbols,
    widgets::{Block, Borders, Clear, List, ListState},
};

use super::{BUTTON_GROUP_SYMBOLS, Modal};
use crate::{
    config::keys::CommonAction,
    ctx::Ctx,
    shared::{
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        dirstack::DirState,
        widgets::button::{Button, ButtonGroup, ButtonGroupState},
    },
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
/// the window title is "▶ files — <name>" since round 22): lists the
/// torrent's video files (name + size) and lets the user mark
/// the ones to play. Space (`CommonAction::Select`) toggles a mark, Enter
/// (`CommonAction::Confirm`) moves the cursor to the action buttons
/// (Play / Download & Play / Cancel) where a second Enter confirms the
/// focused one — Play and Download & Play play the marked files (all of
/// them when none is marked), Cancel closes.
#[derive(derive_more::Debug)]
pub struct TorrentFilePicker<'a, Callback: FnOnce(&Ctx, Vec<usize>, TorrentPickerAction) -> Result<()>> {
    id: Id,
    button_group_state: ButtonGroupState,
    button_group: ButtonGroup<'a>,
    scrolling_state: DirState<ListState>,
    focused: FocusedComponent,
    options_area: Rect,
    /// The rendered scrollbar's column (mouse clicks/drags on it scroll
    /// the list via `DirState::scrollbar_drag`).
    scrollbar_area: Rect,
    /// (positional file index, name, length) — the picker keeps the
    /// positional index so the stream URL addresses the right file.
    options: Vec<(usize, String, u64)>,
    /// Indices into `options` the user marked.
    marked: HashSet<usize>,
    #[debug(skip)]
    callback: Option<Callback>,
    title: String,
}

#[derive(Debug, PartialEq, Eq)]
enum FocusedComponent {
    List,
    Buttons,
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
    if unit == 0 {
        format!("{len} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl<'a, Callback: FnOnce(&Ctx, Vec<usize>, TorrentPickerAction) -> Result<()>>
    TorrentFilePicker<'a, Callback>
{
    pub fn new(
        ctx: &Ctx,
        title: impl Into<String>,
        options: Vec<(usize, String, u64)>,
        on_confirm: Callback,
    ) -> Self {
        let mut scrolling_state = DirState::default();
        scrolling_state.select(Some(0), 0);

        let mut button_group_state = ButtonGroupState::default();
        // Round 20: Play / Download & Play / Cancel — the middle button
        // also keeps the picked files (download job + move to
        // s2udio-downloads), like the single-file "Play and Download".
        let buttons = vec![
            Button::default().label("Play"),
            Button::default().label("Download & Play"),
            Button::default().label("Cancel"),
        ];
        button_group_state.set_button_count(buttons.len());

        let button_group = ButtonGroup::default()
            .buttons(buttons)
            .inactive_style(ctx.config.as_text_style())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(BUTTON_GROUP_SYMBOLS)
                    .border_style(ctx.config.as_border_style()),
            );

        Self {
            id: id::new(),
            button_group,
            button_group_state,
            scrolling_state,
            focused: FocusedComponent::List,
            options_area: Rect::default(),
            scrollbar_area: Rect::default(),
            options,
            marked: HashSet::new(),
            callback: Some(on_confirm),
            title: title.into(),
        }
    }

    fn confirm(&mut self, ctx: &Ctx, action: TorrentPickerAction) -> Result<()> {
        // The play order follows the list (name-sorted); nothing marked
        // plays everything. Either way the callback gets the *positional*
        // file indices (`o.0`) — the options list is name-sorted, so its
        // own indices are NOT the rqbit stream indices.
        let play: Vec<usize> = self
            .options
            .iter()
            .enumerate()
            .filter(|(i, _)| self.marked.is_empty() || self.marked.contains(i))
            .map(|(_, o)| o.0)
            .collect();
        if let Some(cb) = self.callback.take() {
            (cb)(ctx, play, action)?;
        }
        self.hide(ctx)?;
        Ok(())
    }
}

impl<'a, Callback: FnOnce(&Ctx, Vec<usize>, TorrentPickerAction) -> Result<()>> Modal
    for TorrentFilePicker<'a, Callback>
{
    fn id(&self) -> Id {
        self.id
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        // List (with its bordered block: +1 top border, +1 bottom title)
        // plus the button row. At least a handful of rows, capped so a
        // large file list scrolls instead of filling the terminal.
        let height = (self.options.len() as u16 + 5).min(18);
        let popup_area = frame.area().centered(constraint!(==70), constraint!(==height));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), popup_area);
        }

        let [list_area, buttons_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(popup_area);

        let options = self.options.iter().enumerate().map(|(idx, (_, name, length))| {
            let mark = if self.marked.contains(&idx) { "✔ " } else { "  " };
            format!("{mark}{name}  ({})", format_bytes(*length))
        });
        // Round 22: one blank column between the file rows and the
        // scrollbar — the list's right padding shrinks the content area
        // (and the click targets / `options_area`) by 1, while
        // `scrollbar_area` keeps its column so the thumb stays put.
        let list_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_set(symbols::border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .padding(ratatui::widgets::Padding::new(0, 1, 0, 0))
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title(self.title.clone().bold())
            .title_bottom(format!(
                " {} marked · Space toggles · Enter: buttons · Esc cancels ",
                self.marked.len()
            ));

        self.options_area = list_block.inner(list_area);
        self.scrolling_state
            .set_content_and_viewport_len(self.options.len(), self.options_area.height.into());

        let list = List::new(options)
            .style(ctx.config.as_text_style())
            .highlight_style(match self.focused {
                FocusedComponent::Buttons => Style::default().reversed(),
                FocusedComponent::List => ctx.config.theme.current_item_style,
            })
            .block(list_block);

        self.button_group.set_active_style(match self.focused {
            FocusedComponent::List => Style::default().reversed(),
            FocusedComponent::Buttons => ctx.config.theme.current_item_style,
        });

        let scrollbar_area =
            Block::default().padding(ratatui::widgets::Padding::new(0, 0, 1, 0)).inner(list_area);
        self.scrollbar_area = scrollbar_area;

        frame.render_stateful_widget(
            list,
            list_area,
            self.scrolling_state.as_render_state_ref(),
        );
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar() {
            frame.render_stateful_widget(
                scrollbar,
                scrollbar_area,
                self.scrolling_state.as_scrollbar_state_ref(),
            );
        }
        self.button_group.render_with_hover(
            buttons_area,
            frame.buffer_mut(),
            &mut self.button_group_state,
            ctx.modal_mouse_pos(),
        );
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::Down => match self.focused {
                    FocusedComponent::List => {
                        if self
                            .scrolling_state
                            .get_selected()
                            .is_some_and(|s| s == self.options.len() - 1)
                        {
                            self.focused = FocusedComponent::Buttons;
                            self.button_group_state.first();
                        } else {
                            self.scrolling_state.next(ctx.config.scrolloff, true);
                        }
                    }
                    FocusedComponent::Buttons => {
                        if self.button_group_state.selected
                            == self.button_group_state.button_count() - 1
                        {
                            self.focused = FocusedComponent::List;
                            self.scrolling_state.first();
                        } else {
                            self.button_group_state.next();
                        }
                    }
                },
                CommonAction::Up => match self.focused {
                    FocusedComponent::List => {
                        if self.scrolling_state.get_selected().is_some_and(|s| s == 0) {
                            self.focused = FocusedComponent::Buttons;
                            self.button_group_state.last();
                        } else {
                            self.scrolling_state.prev(ctx.config.scrolloff, true);
                        }
                    }
                    FocusedComponent::Buttons => {
                        if self.button_group_state.selected == 0 {
                            self.focused = FocusedComponent::List;
                            self.scrolling_state.last();
                        } else {
                            self.button_group_state.prev();
                        }
                    }
                },
                // Space toggles the mark of the highlighted file.
                CommonAction::Select => {
                    if self.focused == FocusedComponent::List
                        && let Some(idx) = self.scrolling_state.get_selected()
                    {
                        if !self.marked.remove(&idx) {
                            self.marked.insert(idx);
                        }
                    }
                    ctx.render()?;
                }
                // a/d / ←/→ move between the buttons (raw keys, claimed in
                // handle_raw_key — the minimal keybind set has no
                // navigation binding for them); configs that DO bind them
                // arrive here as common actions.
                CommonAction::Left => {
                    if self.focused == FocusedComponent::Buttons {
                        self.button_group_state.prev();
                        ctx.render()?;
                    }
                }
                CommonAction::Right => {
                    if self.focused == FocusedComponent::Buttons {
                        self.button_group_state.next();
                        ctx.render()?;
                    }
                }
                // Enter on the list moves the cursor to the action buttons
                // (Play / Download & Play / Cancel) so the user chooses
                // instead of playing immediately (round-21 user note); a
                // second Enter confirms the focused button: Play (0) and
                // Download & Play (1) run their action, Cancel (2)
                // closes. The buttons stay reachable via the wrap-around
                // navigation and the mouse.
                CommonAction::Confirm => match self.focused {
                    FocusedComponent::List => {
                        self.focused = FocusedComponent::Buttons;
                        self.button_group_state.first();
                        ctx.render()?;
                    }
                    FocusedComponent::Buttons => match self.button_group_state.selected {
                        0 => {
                            self.confirm(ctx, TorrentPickerAction::Play)?;
                            ctx.render()?;
                        }
                        1 => {
                            self.confirm(ctx, TorrentPickerAction::DownloadAndPlay)?;
                            ctx.render()?;
                        }
                        _ => {
                            self.hide(ctx)?;
                            ctx.render()?;
                        }
                    },
                },
                CommonAction::Close => {
                    self.hide(ctx)?;
                    ctx.render()?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_raw_key(&mut self, key: crossterm::event::KeyEvent, ctx: &mut Ctx) -> Result<bool> {
        // The app's minimal keybind set binds Space to TogglePause and
        // removed the default `Select` navigation mapping, so the picker
        // claims Space directly to toggle marks (the help row's "Space
        // toggles").
        if key.kind != crossterm::event::KeyEventKind::Release
            && key.modifiers.is_empty()
            && matches!(key.code, crossterm::event::KeyCode::Char(' '))
            && self.focused == FocusedComponent::List
            && let Some(idx) = self.scrolling_state.get_selected()
        {
            if !self.marked.remove(&idx) {
                self.marked.insert(idx);
            }
            ctx.render()?;
            return Ok(true);
        }
        // a/d and ←/→ move between the action buttons (the minimal
        // keybind set binds only w/s/↑/↓ in navigation, so the picker
        // claims the horizontal keys itself — round-21 user note).
        if key.kind != crossterm::event::KeyEventKind::Release
            && key.modifiers.is_empty()
            && self.focused == FocusedComponent::Buttons
        {
            match key.code {
                crossterm::event::KeyCode::Char('a') | crossterm::event::KeyCode::Left => {
                    self.button_group_state.prev();
                    ctx.render()?;
                    return Ok(true);
                }
                crossterm::event::KeyCode::Char('d') | crossterm::event::KeyCode::Right => {
                    self.button_group_state.next();
                    ctx.render()?;
                    return Ok(true);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        // Scrollbar clicks / drags scroll the list (the same thumb-follow
        // contract as the tabs' scrollbars). The wheel over the list is
        // handled below (it moves the selection, not the viewport).
        let content_len = self
            .scrolling_state
            .content_len()
            .unwrap_or(self.options.len())
            .saturating_sub(self.scrolling_state.viewport_len().unwrap_or(0))
            .saturating_add(1)
            .max(1);
        let viewport_len = self
            .scrolling_state
            .viewport_len()
            .unwrap_or(self.scrollbar_area.height as usize);
        let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
        if let Some(perc) = self.scrolling_state.scrollbar_drag.handle(
            event,
            self.scrollbar_area,
            content_len,
            viewport_len,
            self.scrolling_state.offset(),
            begin_len,
            end_len,
        ) {
            self.focused = FocusedComponent::List;
            self.scrolling_state.scroll_to(perc, ctx.config.scrolloff);
            ctx.render()?;
            return Ok(());
        }
        match event.kind {
            MouseEventKind::LeftClick => {
                if self.options_area.contains(event.into()) {
                    let clicked_row: usize =
                        event.y.saturating_sub(self.options_area.y).into();
                    if let Some(idx) = self.scrolling_state.get_at_rendered_row(clicked_row) {
                        self.focused = FocusedComponent::List;
                        self.scrolling_state.select(Some(idx), 0);
                        if !self.marked.remove(&idx) {
                            self.marked.insert(idx);
                        }
                        ctx.render()?;
                    }
                } else if let Some(btn) = self
                    .button_group
                    .get_button_idx_at(ratatui::layout::Position::new(event.x, event.y))
                {
                    // Clicking a button selects it (and shows the focus).
                    self.focused = FocusedComponent::Buttons;
                    self.button_group_state.select(btn);
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                // Double-clicking a button activates it: Play (0),
                // Download & Play (1), Cancel (2) — round-21 user note.
                if let Some(btn) = self
                    .button_group
                    .get_button_idx_at(ratatui::layout::Position::new(event.x, event.y))
                {
                    match btn {
                        0 => self.confirm(ctx, TorrentPickerAction::Play)?,
                        1 => self.confirm(ctx, TorrentPickerAction::DownloadAndPlay)?,
                        _ => self.hide(ctx)?,
                    }
                    ctx.render()?;
                } else if self.options_area.contains(event.into())
                    && let Some(idx) = self.scrolling_state.get_selected()
                    && !self.marked.remove(&idx)
                {
                    self.marked.insert(idx);
                    ctx.render()?;
                }
            }
            MouseEventKind::ScrollUp => {
                // Over the buttons: move the button selection. Over the
                // list (or the scrollbar): move the list selection.
                if self
                    .button_group
                    .get_button_idx_at(ratatui::layout::Position::new(event.x, event.y))
                    .is_some()
                {
                    self.focused = FocusedComponent::Buttons;
                    self.button_group_state.prev();
                    ctx.render()?;
                } else if self.options_area.contains(event.into()) {
                    self.focused = FocusedComponent::List;
                    self.scrolling_state.prev(ctx.config.scrolloff, true);
                    ctx.render()?;
                }
            }
            MouseEventKind::ScrollDown => {
                if self
                    .button_group
                    .get_button_idx_at(ratatui::layout::Position::new(event.x, event.y))
                    .is_some()
                {
                    self.focused = FocusedComponent::Buttons;
                    self.button_group_state.next();
                    ctx.render()?;
                } else if self.options_area.contains(event.into()) {
                    self.focused = FocusedComponent::List;
                    self.scrolling_state.next(ctx.config.scrolloff, true);
                    ctx.render()?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

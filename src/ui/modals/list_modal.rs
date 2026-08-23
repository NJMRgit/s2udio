use std::collections::HashSet;
use anyhow::Result;
use bon::bon;
use ratatui::{
    Frame, layout::{Constraint, Layout, Position, Rect},
    macros::constraint, style::{Style, Stylize},
    symbols, widgets::{Block, Borders, Clear, List, ListState, Padding},
};
use super::{BUTTON_GROUP_SYMBOLS, Modal};
use crate::{
    config::keys::CommonAction, ctx::Ctx,
    shared::{
        id::{self, Id},
        keys::ActionEvent, mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{dirstack::DirState, widgets::button::{Button, ButtonGroup, ButtonGroupState}},
};
/// The payload handed to a `ListModal`'s `on_confirm` callback.
///
/// - Single-select modals (the `SelectModal` shape) fill `value` + `index`
///   (the confirmed option and its list position; the option is moved out
///   of the modal, like the legacy picker did).
/// - Multi-select modals (the torrent file picker) fill `marked` (the
///   marked rows' ids via `mark_id`; empty means "all rows").
///
/// `button` is the 0-based index of the confirmed button.
#[derive(Debug)]
pub struct ListConfirm<V> {
    pub button: usize,
    pub value: Option<V>,
    pub index: usize,
    pub marked: Vec<usize>,
}
#[derive(Debug, PartialEq, Eq)]
enum FocusedComponent {
    List,
    Buttons,
}
/// One master implementation of the "options list + scrollbar + action
/// buttons" modal shape (Phase 3). Absorbs the legacy `SelectModal`
/// (single-select picker) and the torrent file picker (multi-select with
/// marks): the list, selection, scrollbar (incl. drag), hover, the
/// List↔Buttons focus cycle and the Confirm/Close/wheel/click/double-click
/// handling live here once, parameterized by args.
///
/// Args (per-instance differences, never a fork):
///
/// - `row_fn` — how an option renders (`fn(&V, is_marked, list_idx) -> String`).
/// - `size_fn` — popup size for a given option count.
/// - `buttons` — the action-button labels; buttons `0..confirm_buttons`
///   confirm, the rest cancel (like the legacy Cancel).
/// - `multi_select` + `mark_id` — mark toggling (Space/click) and the id
///   the confirm payload carries per marked row.
/// - `bottom_title`, `list_right_padding` — optional bottom help line and
///   a gap before the scrollbar.
/// - `wheel_moves_selection` — wheel over the list moves the selection
///   (torrent picker) or scrolls the viewport (legacy picker).
/// - `scrollbar_drag` — drag the scrollbar thumb (torrent picker).
#[derive(derive_more::Debug)]
pub struct ListModal<'a, V> {
    id: Id,
    title: String,
    options: Vec<V>,
    state: DirState<ListState>,
    focused: FocusedComponent,
    options_area: Rect,
    scrollbar_area: Rect,
    button_group: ButtonGroup<'a>,
    button_group_state: ButtonGroupState,
    multi_select: bool,
    marked: HashSet<usize>,
    row_fn: fn(&V, bool, usize) -> String,
    size_fn: fn(usize) -> (u16, u16),
    list_right_padding: u16,
    bottom_title: Option<fn(usize) -> String>,
    confirm_buttons: usize,
    wheel_moves_selection: bool,
    scrollbar_drag: bool,
    mark_id: Option<fn(&V) -> usize>,
    #[debug(skip)]
    callback: Option<
        Box<dyn FnOnce(&Ctx, ListConfirm<V>) -> Result<()> + Send + Sync + 'a>,
    >,
}
#[bon]
impl<'a, V: std::fmt::Debug> ListModal<'a, V> {
    #[builder]
    pub fn new(
        ctx: &Ctx,
        title: String,
        options: Vec<V>,
        row_fn: fn(&V, bool, usize) -> String,
        size_fn: fn(usize) -> (u16, u16),
        #[builder(default = vec!["Confirm", "Cancel"])]
        buttons: Vec<&'a str>,
        #[builder(default = 1)]
        confirm_buttons: usize,
        #[builder(default)]
        multi_select: bool,
        mark_id: Option<fn(&V) -> usize>,
        bottom_title: Option<fn(usize) -> String>,
        #[builder(default)]
        list_right_padding: u16,
        #[builder(default)]
        wheel_moves_selection: bool,
        #[builder(default)]
        scrollbar_drag: bool,
        on_confirm: impl FnOnce(&Ctx, ListConfirm<V>) -> Result<()> + Send + Sync + 'a,
    ) -> Self {
        let mut state = DirState::default();
        state.select(Some(0), 0);
        let mut button_group_state = ButtonGroupState::default();
        let button_widgets: Vec<Button<'a>> = buttons
            .iter()
            .map(|label| Button::default().label(*label))
            .collect();
        button_group_state.set_button_count(buttons.len());
        let button_group = ButtonGroup::default()
            .buttons(button_widgets)
            .inactive_style(ctx.config.as_text_style())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(BUTTON_GROUP_SYMBOLS)
                    .border_style(ctx.config.as_border_style()),
            );
        Self {
            id: id::new(),
            title,
            options,
            state,
            focused: FocusedComponent::List,
            options_area: Rect::default(),
            scrollbar_area: Rect::default(),
            button_group,
            button_group_state,
            multi_select,
            marked: HashSet::new(),
            row_fn,
            size_fn,
            list_right_padding,
            bottom_title,
            confirm_buttons,
            wheel_moves_selection,
            scrollbar_drag,
            mark_id,
            callback: Some(Box::new(on_confirm)),
        }
    }
}
impl<'a, V: std::fmt::Debug> ListModal<'a, V> {
    /// Run the confirmed button's action: build the `ListConfirm` payload
    /// and hand it to the callback, then close the modal.
    fn confirm(&mut self, ctx: &Ctx, button: usize) -> Result<()> {
        let confirm = if self.multi_select {
            let marked = self
                .options
                .iter()
                .enumerate()
                .filter(|(i, _)| self.marked.is_empty() || self.marked.contains(i))
                .map(|(idx, v)| self.mark_id.map_or(idx, |f| f(v)))
                .collect();
            ListConfirm {
                button,
                value: None,
                index: 0,
                marked,
            }
        } else {
            let Some(idx) = self.state.get_selected() else {
                return self.hide(ctx);
            };
            let value = self.options.remove(idx);
            ListConfirm {
                button,
                value: Some(value),
                index: idx,
                marked: Vec::new(),
            }
        };
        if let Some(cb) = self.callback.take() {
            (cb)(ctx, confirm)?;
        }
        self.hide(ctx)?;
        Ok(())
    }
    /// Toggle the mark of the highlighted row (multi-select only).
    fn toggle_mark(&mut self) {
        if let Some(idx) = self.state.get_selected() {
            if !self.marked.remove(&idx) {
                self.marked.insert(idx);
            }
        }
    }
}
impl<V: std::fmt::Debug> Modal for ListModal<'_, V> {
    fn id(&self) -> Id {
        self.id
    }
    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        let (w, h) = (self.size_fn)(self.options.len());
        let popup_area = frame.area().centered(constraint!(== w), constraint!(== h));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame
                .render_widget(
                    Block::default().style(Style::default().bg(bg_color)),
                    popup_area,
                );
        }
        let [list_area, buttons_area] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .areas(popup_area);
        let options = self
            .options
            .iter()
            .enumerate()
            .map(|(idx, v)| {
                let marked = self.multi_select && self.marked.contains(&idx);
                (self.row_fn)(v, marked, idx)
            });
        let mut list_block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_set(symbols::border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .padding(Padding::new(0, self.list_right_padding, 0, 0))
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title(self.title.clone().bold());
        if let Some(f) = self.bottom_title {
            list_block = list_block.title_bottom(f(self.marked.len()));
        }
        self.options_area = list_block.inner(list_area);
        self.state
            .set_content_and_viewport_len(
                self.options.len(),
                self.options_area.height.into(),
            );
        let list = List::new(options)
            .style(ctx.config.as_text_style())
            .highlight_style(
                match self.focused {
                    FocusedComponent::Buttons => Style::default().reversed(),
                    FocusedComponent::List => ctx.config.theme.current_item_style,
                },
            )
            .block(list_block);
        self.button_group
            .set_active_style(
                match self.focused {
                    FocusedComponent::List => Style::default().reversed(),
                    FocusedComponent::Buttons => ctx.config.theme.current_item_style,
                },
            );
        let scrollbar_area = Block::default()
            .padding(Padding::new(0, 0, 1, 0))
            .inner(list_area);
        self.scrollbar_area = scrollbar_area;
        frame.render_stateful_widget(list, list_area, self.state.as_render_state_ref());
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar() {
            frame
                .render_stateful_widget(
                    scrollbar,
                    scrollbar_area,
                    self.state.as_scrollbar_state_ref(),
                );
        }
        self.button_group
            .render_with_hover(
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
                CommonAction::Down => {
                    match self.focused {
                        FocusedComponent::List => {
                            if self
                                .state
                                .get_selected()
                                .is_some_and(|s| s == self.options.len() - 1)
                            {
                                self.focused = FocusedComponent::Buttons;
                                self.button_group_state.first();
                            } else {
                                self.state.next(ctx.config.scrolloff, true);
                            }
                        }
                        FocusedComponent::Buttons => {
                            if self.button_group_state.selected
                                == self.button_group_state.button_count() - 1
                            {
                                self.focused = FocusedComponent::List;
                                self.state.first();
                            } else {
                                self.button_group_state.next();
                            }
                        }
                    }
                }
                CommonAction::Up => {
                    match self.focused {
                        FocusedComponent::List => {
                            if self.state.get_selected().is_some_and(|s| s == 0) {
                                self.focused = FocusedComponent::Buttons;
                                self.button_group_state.last();
                            } else {
                                self.state.prev(ctx.config.scrolloff, true);
                            }
                        }
                        FocusedComponent::Buttons => {
                            if self.button_group_state.selected == 0 {
                                self.focused = FocusedComponent::List;
                                self.state.last();
                            } else {
                                self.button_group_state.prev();
                            }
                        }
                    }
                }
                CommonAction::Select if self.multi_select => {
                    if self.focused == FocusedComponent::List {
                        self.toggle_mark();
                    }
                    ctx.render()?;
                }
                CommonAction::Left if self.multi_select => {
                    if self.focused == FocusedComponent::Buttons {
                        self.button_group_state.prev();
                        ctx.render()?;
                    }
                }
                CommonAction::Right if self.multi_select => {
                    if self.focused == FocusedComponent::Buttons {
                        self.button_group_state.next();
                        ctx.render()?;
                    }
                }
                CommonAction::Confirm => {
                    match self.focused {
                        FocusedComponent::List => {
                            self.focused = FocusedComponent::Buttons;
                            self.button_group_state.first();
                            ctx.render()?;
                        }
                        FocusedComponent::Buttons if self.button_group_state.selected
                            < self.confirm_buttons => {
                            self.confirm(ctx, self.button_group_state.selected)?;
                            ctx.render()?;
                        }
                        FocusedComponent::Buttons => {
                            self.hide(ctx)?;
                            ctx.render()?;
                        }
                    }
                }
                CommonAction::Close => {
                    self.hide(ctx)?;
                    ctx.render()?;
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn handle_raw_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        ctx: &mut Ctx,
    ) -> Result<bool> {
        if !self.multi_select {
            return Ok(false);
        }
        if key.kind != crossterm::event::KeyEventKind::Release
            && key.modifiers.is_empty()
            && matches!(key.code, crossterm::event::KeyCode::Char(' '))
            && self.focused == FocusedComponent::List
        {
            self.toggle_mark();
            ctx.render()?;
            return Ok(true);
        }
        if key.kind != crossterm::event::KeyEventKind::Release
            && key.modifiers.is_empty() && self.focused == FocusedComponent::Buttons
        {
            match key.code {
                crossterm::event::KeyCode::Char('a')
                | crossterm::event::KeyCode::Left => {
                    self.button_group_state.prev();
                    ctx.render()?;
                    return Ok(true);
                }
                crossterm::event::KeyCode::Char('d')
                | crossterm::event::KeyCode::Right => {
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
        if self.scrollbar_drag {
            let content_len = self
                .state
                .content_len()
                .unwrap_or(self.options.len())
                .saturating_sub(self.state.viewport_len().unwrap_or(0))
                .saturating_add(1)
                .max(1);
            let viewport_len = self
                .state
                .viewport_len()
                .unwrap_or(self.scrollbar_area.height as usize);
            let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
            if let Some(perc) = self
                .state
                .scrollbar_drag
                .handle(
                    event,
                    self.scrollbar_area,
                    content_len,
                    viewport_len,
                    self.state.offset(),
                    begin_len,
                    end_len,
                )
            {
                self.focused = FocusedComponent::List;
                self.state.scroll_to(perc, ctx.config.scrolloff);
                ctx.render()?;
                return Ok(());
            }
        }
        match event.kind {
            MouseEventKind::LeftClick => {
                if self.options_area.contains(event.into()) {
                    let clicked_row: usize = event
                        .y
                        .saturating_sub(self.options_area.y)
                        .into();
                    if let Some(idx) = self.state.get_at_rendered_row(clicked_row) {
                        self.focused = FocusedComponent::List;
                        self.state.select(Some(idx), 0);
                        if self.multi_select {
                            self.marked_toggle(idx);
                        }
                        ctx.render()?;
                    }
                } else if let Some(btn) = self
                    .button_group
                    .get_button_idx_at(Position::new(event.x, event.y))
                {
                    self.focused = FocusedComponent::Buttons;
                    self.button_group_state.select(btn);
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                if let Some(btn) = self
                    .button_group
                    .get_button_idx_at(Position::new(event.x, event.y))
                {
                    if btn < self.confirm_buttons {
                        self.confirm(ctx, btn)?;
                    } else {
                        self.hide(ctx)?;
                    }
                    ctx.render()?;
                } else if self.multi_select && self.options_area.contains(event.into())
                    && self.state.get_selected().is_some()
                {
                    self.marked_toggle(self.state.get_selected().unwrap());
                    ctx.render()?;
                }
            }
            MouseEventKind::ScrollUp => {
                if self
                    .button_group
                    .get_button_idx_at(Position::new(event.x, event.y))
                    .is_some()
                {
                    self.focused = FocusedComponent::Buttons;
                    self.button_group_state.prev();
                    ctx.render()?;
                } else if self.options_area.contains(event.into()) {
                    self.focused = FocusedComponent::List;
                    if self.wheel_moves_selection {
                        self.state.prev(ctx.config.scrolloff, true);
                    } else {
                        self.state
                            .scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
                    }
                    ctx.render()?;
                }
            }
            MouseEventKind::ScrollDown => {
                if self
                    .button_group
                    .get_button_idx_at(Position::new(event.x, event.y))
                    .is_some()
                {
                    self.focused = FocusedComponent::Buttons;
                    self.button_group_state.next();
                    ctx.render()?;
                } else if self.options_area.contains(event.into()) {
                    self.focused = FocusedComponent::List;
                    if self.wheel_moves_selection {
                        self.state.next(ctx.config.scrolloff, true);
                    } else {
                        self.state
                            .scroll_down(ctx.config.scroll_amount, ctx.config.scrolloff);
                    }
                    ctx.render()?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
impl<V> ListModal<'_, V> {
    /// Toggle the mark of `idx` (multi-select only).
    fn marked_toggle(&mut self, idx: usize) {
        if !self.marked.remove(&idx) {
            self.marked.insert(idx);
        }
    }
}

use std::collections::HashSet;

use anyhow::Result;
use bon::bon;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    macros::constraint,
    style::{Style, Stylize},
    symbols,
    widgets::{Block, Borders, Clear, List, ListState, Padding},
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
    callback: Option<Box<dyn FnOnce(&Ctx, ListConfirm<V>) -> Result<()> + Send + Sync + 'a>>,
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
        let button_widgets: Vec<Button<'a>> =
            buttons.iter().map(|label| Button::default().label(*label)).collect();
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
            ListConfirm { button, value: None, index: 0, marked }
        } else {
            let Some(idx) = self.state.get_selected() else {
                return self.hide(ctx);
            };
            let value = self.options.remove(idx);
            ListConfirm { button, value: Some(value), index: idx, marked: Vec::new() }
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
        let popup_area = frame.area().centered(constraint!(==w), constraint!(==h));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), popup_area);
        }

        let [list_area, buttons_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(popup_area);

        let options = self.options.iter().enumerate().map(|(idx, v)| {
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

        let scrollbar_area = Block::default()
            .padding(Padding::new(0, 0, 1, 0))
            .inner(list_area);
        self.scrollbar_area = scrollbar_area;

        frame.render_stateful_widget(list, list_area, self.state.as_render_state_ref());
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar() {
            frame.render_stateful_widget(
                scrollbar,
                scrollbar_area,
                self.state.as_scrollbar_state_ref(),
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
                },
                CommonAction::Up => match self.focused {
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
                },
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
                CommonAction::Confirm => match self.focused {
                    FocusedComponent::List => {
                        self.focused = FocusedComponent::Buttons;
                        self.button_group_state.first();
                        ctx.render()?;
                    }
                    FocusedComponent::Buttons
                        if self.button_group_state.selected < self.confirm_buttons =>
                    {
                        self.confirm(ctx, self.button_group_state.selected)?;
                        ctx.render()?;
                    }
                    FocusedComponent::Buttons => {
                        self.hide(ctx)?;
                        ctx.render()?;
                    }
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
        if !self.multi_select {
            return Ok(false);
        }
        // The minimal keybind set binds Space to TogglePause and removed
        // the default `Select` mapping, so multi-select claims Space
        // directly to toggle marks (the help row's "Space toggles").
        if key.kind != crossterm::event::KeyEventKind::Release
            && key.modifiers.is_empty()
            && matches!(key.code, crossterm::event::KeyCode::Char(' '))
            && self.focused == FocusedComponent::List
        {
            self.toggle_mark();
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
        // handled below (it moves the selection / viewport, not the
        // scrollbar thumb).
        if self.scrollbar_drag {
            let content_len = self
                .state
                .content_len()
                .unwrap_or(self.options.len())
                .saturating_sub(self.state.viewport_len().unwrap_or(0))
                .saturating_add(1)
                .max(1);
            let viewport_len =
                self.state.viewport_len().unwrap_or(self.scrollbar_area.height as usize);
            let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
            if let Some(perc) = self.state.scrollbar_drag.handle(
                event,
                self.scrollbar_area,
                content_len,
                viewport_len,
                self.state.offset(),
                begin_len,
                end_len,
            ) {
                self.focused = FocusedComponent::List;
                self.state.scroll_to(perc, ctx.config.scrolloff);
                ctx.render()?;
                return Ok(());
            }
        }
        match event.kind {
            MouseEventKind::LeftClick => {
                if self.options_area.contains(event.into()) {
                    let clicked_row: usize = event.y.saturating_sub(self.options_area.y).into();
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
                    // Clicking a button selects it (and shows the focus).
                    self.focused = FocusedComponent::Buttons;
                    self.button_group_state.select(btn);
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                // Double-clicking a button activates it: confirm for
                // `button < confirm_buttons`, cancel otherwise (round-21
                // user note). Double-clicking a multi-select row toggles
                // its mark.
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
                } else if self.multi_select
                    && self.options_area.contains(event.into())
                    && self.state.get_selected().is_some()
                {
                    self.marked_toggle(self.state.get_selected().unwrap());
                    ctx.render()?;
                }
            }
            MouseEventKind::ScrollUp => {
                // Over the buttons: move the button selection. Over the
                // list (or the scrollbar): move the list selection.
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
                        self.state.scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
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
                        self.state.scroll_down(ctx.config.scroll_amount, ctx.config.scrolloff);
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        config::keys::CommonAction,
        shared::keys::Actions,
        ui::ActionEvent,
    };

    fn test_ctx() -> (Ctx, crossbeam::channel::Receiver<crate::AppEvent>) {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        (ctx, app_rx)
    }

    fn key(action: CommonAction) -> ActionEvent {
        ActionEvent::from(Arc::new(vec![Actions::Common(action)]))
    }

    /// Render once so `DirState` gets its content/viewport length (the
    /// production flow always renders before any key/mouse event).
    fn render_modal<V: std::fmt::Debug>(modal: &mut ListModal<'_, V>, ctx: &mut Ctx) {
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, ctx).expect("modal renders"))
            .expect("draw ok");
    }

    /// Single-select (the legacy `SelectModal` shape): pressing Down moves
    /// the highlight, Confirm twice delivers the confirmed option's value
    /// and index to the callback and closes the modal.
    #[test]
    fn single_select_confirm_delivers_value_and_index() {
        let (mut ctx, app_rx) = test_ctx();
        let captured = Arc::new(Mutex::new(None));
        let captured2 = captured.clone();
        let mut modal = ListModal::builder()
            .ctx(&ctx)
            .title("Pick a playlist".into())
            .options(vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()])
            .row_fn(|v: &String, _marked, idx| format!("{:>3}: {v}", idx + 1))
            .size_fn(|_| (80, 15))
            .on_confirm(move |ctx, confirm: ListConfirm<String>| {
                *captured2.lock().unwrap() = Some(confirm);
                ctx.app_event_sender
                    .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(
                        crate::shared::id::new(),
                    )))?;
                Ok(())
            })
            .build();

        render_modal(&mut modal, &mut ctx);
        modal.handle_key(&mut key(CommonAction::Down), &mut ctx).unwrap();
        // Enter moves to the buttons, a second Enter confirms the focused
        // (first) button.
        modal.handle_key(&mut key(CommonAction::Confirm), &mut ctx).unwrap();
        modal.handle_key(&mut key(CommonAction::Confirm), &mut ctx).unwrap();

        let confirm = captured.lock().unwrap().take().expect("callback ran");
        assert_eq!(confirm.button, 0);
        assert_eq!(confirm.value, Some("beta".to_owned()));
        assert_eq!(confirm.index, 1);
        assert!(confirm.marked.is_empty());
        // The modal closed itself.
        assert!(
            app_rx.iter().any(|ev| matches!(
                ev,
                crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(id)) if id == modal.id()
            )),
            "expected the modal to close after confirming"
        );
    }

    /// Multi-select (the torrent picker shape): Space toggles marks, the
    /// confirm payload carries the mark_id-mapped indices and the button.
    #[test]
    fn multi_select_confirm_delivers_marked_ids() {
        let (mut ctx, app_rx) = test_ctx();
        let captured = Arc::new(Mutex::new(None));
        let captured2 = captured.clone();
        let mut modal = ListModal::builder()
            .ctx(&ctx)
            .title("▶ files — demo ".into())
            .options(vec![
                (2, "S01E02.mkv".to_owned(), 900),
                (0, "S01E00.mkv".to_owned(), 700),
                (1, "S01E01.mkv".to_owned(), 800),
            ])
            .row_fn(|(_, name, len): &(usize, String, u64), marked, _idx| {
                let mark = if marked { "✔ " } else { "  " };
                format!("{mark}{name}  ({len})")
            })
            .size_fn(|len| (70, (len as u16 + 5).min(18)))
            .buttons(vec!["Play", "Download & Play", "Cancel"])
            .confirm_buttons(2)
            .multi_select(true)
            .mark_id(|o: &(usize, String, u64)| o.0)
            .list_right_padding(1)
            .bottom_title(|n| format!(" {n} marked "))
            .wheel_moves_selection(true)
            .scrollbar_drag(true)
            .on_confirm(move |ctx, confirm: ListConfirm<(usize, String, u64)>| {
                *captured2.lock().unwrap() = Some(confirm);
                ctx.app_event_sender
                    .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(
                        crate::shared::id::new(),
                    )))?;
                Ok(())
            })
            .build();

        render_modal(&mut modal, &mut ctx);
        // Space marks row 0 (positional 2); Down + Space marks row 1
        // (positional 0).
        let space = |ctx: &mut Ctx, modal: &mut ListModal<'_, (usize, String, u64)>| {
            modal
                .handle_raw_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Char(' '),
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    ctx,
                )
                .unwrap()
        };
        space(&mut ctx, &mut modal);
        modal.handle_key(&mut key(CommonAction::Down), &mut ctx).unwrap();
        space(&mut ctx, &mut modal);
        // Confirm twice: list -> buttons, then the focused Play button.
        modal.handle_key(&mut key(CommonAction::Confirm), &mut ctx).unwrap();
        modal.handle_key(&mut key(CommonAction::Confirm), &mut ctx).unwrap();

        let confirm = captured.lock().unwrap().take().expect("callback ran");
        assert_eq!(confirm.button, 0, "Play confirmed");
        assert_eq!(confirm.value, None);
        // Sorted list order: row 0 = S01E02 (positional 2), row 1 =
        // S01E00 (positional 0).
        assert_eq!(confirm.marked, vec![2, 0]);
        assert!(
            app_rx.iter().any(|ev| matches!(
                ev,
                crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(id)) if id == modal.id()
            )),
            "expected the modal to close after confirming"
        );
    }

    /// The cancel button closes the modal without running the callback.
    #[test]
    fn cancel_button_hides_without_callback() {
        let (mut ctx, _app_rx) = test_ctx();
        let ran = Arc::new(Mutex::new(false));
        let ran2 = ran.clone();
        let mut modal = ListModal::builder()
            .ctx(&ctx)
            .title("Pick".into())
            .options(vec!["only".to_owned()])
            .row_fn(|v: &String, _marked, idx| format!("{:>3}: {v}", idx + 1))
            .size_fn(|_| (80, 15))
            .on_confirm(move |ctx, _confirm: ListConfirm<String>| {
                *ran2.lock().unwrap() = true;
                ctx.app_event_sender
                    .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(
                        crate::shared::id::new(),
                    )))?;
                Ok(())
            })
            .build();

        render_modal(&mut modal, &mut ctx);
        // Enter -> buttons (Confirm focused); Down -> the Cancel button;
        // Enter -> cancel: hide, no callback.
        modal.handle_key(&mut key(CommonAction::Confirm), &mut ctx).unwrap();
        modal.handle_key(&mut key(CommonAction::Down), &mut ctx).unwrap();
        modal.handle_key(&mut key(CommonAction::Confirm), &mut ctx).unwrap();
        assert!(!*ran.lock().unwrap(), "cancel must not run the callback");
    }

    /// The unified row-click mapping (torrent-picker math, which the
    /// legacy picker's extra `-1` used to break): clicking the second
    /// visible row selects the second option.
    #[test]
    fn click_on_second_row_selects_second_option() {
        let (mut ctx, _app_rx) = test_ctx();
        let mut modal = ListModal::builder()
            .ctx(&ctx)
            .title("Pick".into())
            .options(vec![
                "alpha".to_owned(),
                "beta".to_owned(),
                "gamma".to_owned(),
                "delta".to_owned(),
            ])
            .row_fn(|v: &String, _marked, idx| format!("{:>3}: {v}", idx + 1))
            .size_fn(|_| (80, 15))
            .on_confirm(|_ctx, _confirm: ListConfirm<String>| Ok(()))
            .build();

        // Render so `options_area` is populated (80x15 popup centered in
        // the 100x30 test backend: x=10, y=7; the list block's inner area
        // starts below the top border + title row, so options_area.y = 9).
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, &mut ctx).expect("modal renders"))
            .expect("draw ok");
        assert_eq!(modal.options_area.y, 9, "layout assumption");

        // Click the second option row (y = options_area.y + 1).
        modal
            .handle_mouse_event(
                MouseEvent {
                    x: 50,
                    y: modal.options_area.y + 1,
                    kind: MouseEventKind::LeftClick,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .unwrap();
        assert_eq!(modal.state.get_selected(), Some(1), "second row selects the second option");
    }
}

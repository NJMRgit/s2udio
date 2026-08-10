use std::collections::HashSet;

use anyhow::Result;
use enum_map::EnumMap;
use itertools::Itertools;
use ratatui::{prelude::Rect, widgets::ListState};

use crate::{
    MpdQueryResult,
    config::keys::{
        CommonAction,
        GlobalAction,
        actions::{AddKind, AutoplayKind, DeleteKind, Position, RateKind, SaveKind},
    },
    ctx::{Ctx, LIKE_STICKER, RATING_STICKER},
    mpd::{client::Client, commands::Song},
    shared::{
        keys::ActionEvent,
        macros::{modal, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt, MpdDelete},
        mpd_query::EXTERNAL_COMMAND,
    },
    ui::{
        dirstack::{DirStack, DirStackItem, WalkDirStackItem},
        input::InputResultEvent,
        modals::{
            confirm_modal::{Action, ConfirmModal},
            input_modal::InputModal,
            menu::{
                add_to_playlist_or_show_modal,
                create_add_modal,
                create_delete_modal,
                create_rating_modal,
                create_save_modal,
                delete_from_playlist_or_show_confirmation,
                modal::MenuModal,
            },
            select_modal::SelectModal,
        },
        panes::Pane,
        widgets::browser::BrowserArea,
    },
};

#[derive(Debug, Clone, Copy)]
pub enum MoveDirection {
    Up,
    Down,
}

#[allow(unused)]
pub(in crate::ui) trait BrowserPane<T>: Pane
where
    T: DirStackItem + std::fmt::Debug + Clone + Send + Sync + 'static,
{
    fn stack(&self) -> &DirStack<T, ListState>;
    fn stack_mut(&mut self) -> &mut DirStack<T, ListState>;
    fn browser_areas(&self) -> EnumMap<BrowserArea, Rect>;
    fn scrollbar_area(&self) -> Option<Rect> {
        let areas = self.browser_areas();
        let scrollbar = areas[BrowserArea::Scrollbar];
        if scrollbar.width > 0 { Some(scrollbar) } else { None }
    }
    fn open(&mut self, autoplay: bool, ctx: &Ctx) -> Result<()> {
        let Some(selected) = self.stack().current().selected() else {
            log::error!("Failed to move deeper inside dir. Current value is None");
            return Ok(());
        };

        if selected.is_file() {
            let (items, hovered_song_idx) = self.enqueue(
                self.stack()
                    .current()
                    .items
                    .iter()
                    // Only add songs here in case the directory contains combination of
                    // directories, playlists and songs to be able to use autoplay from the
                    // hovered song properly.
                    .filter(|item| item.is_file()),
            );
            if !items.is_empty() {
                let (position, autoplay) = if autoplay {
                    (Position::Replace, AutoplayKind::Hovered)
                } else {
                    (Position::EndOfQueue, AutoplayKind::None)
                };

                Client::resolve_and_enqueue(ctx, items, position, autoplay, None, hovered_song_idx);
            }
        } else {
            self.stack_mut().enter();
            ctx.render()?;
        }

        Ok(())
    }
    fn list_songs_in_item(
        &self,
        item: T,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Send + Sync + Clone + 'static;
    fn list_songs_in_items(
        &self,
        all: bool,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Send + Sync + Clone + 'static {
        let list_songs_fns =
            self.items(all).map(|(_, item)| self.list_songs_in_item(item.to_owned())).collect_vec();
        |client| {
            let song_paths: Vec<_> = list_songs_fns
                .into_iter()
                .map(|cb| -> Result<_> { cb(client) })
                .collect::<Result<Vec<Vec<_>>>>()?
                .into_iter()
                .flatten()
                .collect();

            Ok(song_paths)
        }
    }
    fn fetch_data(&self, selected: &T, ctx: &Ctx) -> Result<()>;
    fn fetch_data_internal(&mut self, ctx: &Ctx) -> Result<()> {
        // Only attempt to fetch for empty directories
        if self.stack().next_dir_items().is_none_or(|d| d.is_empty())
            && let Some(selected) = self.stack().current().selected()
            && !selected.is_file()
        {
            self.fetch_data(selected, ctx)
        } else {
            Ok(())
        }
    }
    fn enqueue<'a>(&self, items: impl Iterator<Item = &'a T>) -> (Vec<Enqueue>, Option<usize>) {
        let path = self.stack().path();
        let hovered = self.stack().current().selected();
        let (items, idx) = items
            .flat_map(|item| item.walk(self.stack(), path.clone()))
            .enumerate()
            .fold((Vec::new(), None), |mut acc, (idx, item)| {
                let filename = item.as_path().to_owned();
                if let Some(hovered) = hovered
                    && hovered.is_file()
                    && hovered.as_path() == filename
                {
                    acc.1 = Some(idx);
                }
                acc.0.push(Enqueue::File { path: filename });

                acc
            });

        (items, idx)
    }
    fn show_info(&self, item: &T, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn initial_playlist_name(&self, all: bool) -> Option<String> {
        if all {
            return self.stack().path().current_dir().map(|v| v.to_owned());
        }

        if !self.stack().current().marked().is_empty() {
            None
        } else if let Some(selected) = self.stack().current().selected()
            && !selected.is_file()
        {
            Some(selected.as_path().to_owned())
        } else {
            None
        }
    }

    fn delete<'a>(&self, item: impl Iterator<Item = (usize, &'a T)>) -> Vec<MpdDelete> {
        Vec::new()
    }

    fn can_rename(&self, item: &T) -> bool {
        false
    }
    fn rename(item: &T, ctx: &Ctx) -> Result<()> {
        Ok(())
    }
    fn move_selected(&mut self, direction: MoveDirection, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        let song_format = ctx.config.theme.browser_song_format.0.as_slice();
        let config = &ctx.config;
        match kind {
            InputResultEvent::Push => {
                self.stack_mut().current_mut().jump_first_matching(song_format, ctx);
                self.stack_mut().current_mut().recalculate_matched_items(song_format, ctx);
                self.fetch_data_internal(ctx);
            }
            InputResultEvent::Pop => {
                self.stack_mut().current_mut().recalculate_matched_items(song_format, ctx);
            }
            InputResultEvent::Confirm => {}
            InputResultEvent::NoChange => {}
            InputResultEvent::Cancel => {
                self.stack_mut().current_mut().set_filter_active(false);
                ctx.input.clear_buffer(self.stack().current().filter_buffer_id);
                self.fetch_data_internal(ctx);
            }
        }
        ctx.render()?;
        Ok(())
    }

    fn handle_global_action(&mut self, event: &mut ActionEvent, ctx: &Ctx) -> Result<()> {
        let Some(action) = event.claim_global() else {
            return Ok(());
        };

        let config = &ctx.config;
        match &action {
            GlobalAction::ExternalCommand { command, .. }
                if !self.stack().current().marked().is_empty() =>
            {
                let marked_items: Vec<_> = self
                    .stack()
                    .current()
                    .marked_items()
                    .map(|item| self.list_songs_in_item(item.clone()))
                    .collect();
                let command = std::sync::Arc::clone(command);
                ctx.query().id(EXTERNAL_COMMAND).query(move |client| {
                    let songs: Vec<_> = marked_items
                        .into_iter()
                        .map(|item| (item)(client))
                        .flatten_ok()
                        .try_collect()?;
                    Ok(MpdQueryResult::ExternalCommand(command, songs))
                });
            }
            GlobalAction::ExternalCommand { command, .. } => {
                if let Some(selected) = self.stack().current().selected() {
                    let selected = selected.clone();
                    let songs = self.list_songs_in_item(selected);
                    let command = std::sync::Arc::clone(command);
                    ctx.query().id(EXTERNAL_COMMAND).query(move |client| {
                        let songs = (songs)(client)?;
                        Ok(MpdQueryResult::ExternalCommand(command, songs))
                    });
                }
            }
            _ => {
                event.abandon();
            }
        }

        Ok(())
    }

    /// checks if a mouse click is on the scrollbar area and also handles
    /// scrollbar interactions (track click jumps, thumb drag keeps the
    /// thumb under the pointer 1:1).
    fn handle_scrollbar_interaction(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<bool> {
        let areas = self.browser_areas();
        let Some(scrollbar_area) = self.scrollbar_area() else {
            return Ok(false);
        };

        if !matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }) {
            return Ok(false);
        }

        let current = self.stack().current();
        let items = current.items.len();
        let viewport_len = current.state.viewport_len().unwrap_or(scrollbar_area.height as usize);
        // The rendered scrollbar's content_length is max_offset + 1, so the
        // geometry (thumb size / travel) matches the widget exactly.
        let content_len = items.saturating_sub(viewport_len).saturating_add(1).max(1);
        let position = current.state.inner.offset();
        let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
        // The immutable borrow (`current`) ends here; re-borrow mutably for
        // the drag.
        let state = &mut self.stack_mut().current_mut().state;
        if let Some(perc) = state
            .scrollbar_drag
            .handle(event, scrollbar_area, content_len, viewport_len, position, begin_len, end_len)
        {
            let before = self.stack().current().selected_with_idx().map(|(i, _)| i);
            self.stack_mut().current_mut().scroll_to(perc, ctx.config.scrolloff);
            if before != self.stack().current().selected_with_idx().map(|(i, _)| i) {
                self.fetch_data_internal(ctx);
            }
            ctx.render()?;
            return Ok(true);
        }

        Ok(false)
    }

    fn handle_mouse_action(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if self.handle_scrollbar_interaction(event, ctx)? {
            return Ok(());
        }

        let areas = self.browser_areas();
        let prev_area = areas[BrowserArea::Previous];
        let current_area = areas[BrowserArea::Current];
        let preview_area = areas[BrowserArea::Preview];

        let position = event.into();
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
                if prev_area.contains(position) =>
            {
                let clicked_row: usize = event.y.saturating_sub(prev_area.y).into();
                if let Some(prev_stack) = self.stack_mut().previous_mut() {
                    if let Some(idx_to_select) = prev_stack.state.get_at_rendered_row(clicked_row) {
                        prev_stack.select_idx(idx_to_select, ctx.config.scrolloff);
                    }
                    self.stack_mut().leave();
                    self.fetch_data_internal(ctx);
                }
            }
            MouseEventKind::DoubleClick if current_area.contains(position) => {
                let clicked_row: usize = event.y.saturating_sub(current_area.y).into();

                if let Some(idx_to_select) =
                    self.stack().current().state.get_at_rendered_row(clicked_row)
                {
                    self.open(false, ctx)?;
                    self.fetch_data_internal(ctx);
                }
            }
            MouseEventKind::MiddleClick if current_area.contains(position) => {
                let clicked_row: usize = event.y.saturating_sub(current_area.y).into();

                if let Some(idx_to_select) =
                    self.stack().current().state.get_at_rendered_row(clicked_row)
                {
                    self.stack_mut().current_mut().select_idx(idx_to_select, ctx.config.scrolloff);
                    if let Some(item) = self.stack().current().selected() {
                        let (items, _) = self.enqueue(std::iter::once(item));
                        if !items.is_empty() {
                            ctx.command(move |client| {
                                client.enqueue_multiple(items, None, None, false)?;
                                Ok(())
                            });
                        }
                    }

                    self.fetch_data_internal(ctx);
                }
            }
            MouseEventKind::LeftClick
                if current_area.contains(position)
                    && event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                let clicked_row: usize = event.y.saturating_sub(current_area.y).into();

                if let Some(idx_to_select) =
                    self.stack().current().state.get_at_rendered_row(clicked_row)
                {
                    let current = self.stack_mut().current_mut();
                    current.select_idx(idx_to_select, ctx.config.scrolloff);
                    current.state.toggle_mark(idx_to_select);
                    self.fetch_data_internal(ctx);
                }
            }
            MouseEventKind::LeftClick
                if current_area.contains(position)
                    && event.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                let clicked_row: usize = event.y.saturating_sub(current_area.y).into();

                if let Some(idx_to_select) =
                    self.stack().current().state.get_at_rendered_row(clicked_row)
                {
                    let current = self.stack_mut().current_mut();
                    if current.state.mark_anchor().is_none() {
                        current.state.set_mark_anchor(idx_to_select);
                    }
                    let anchor = current.state.mark_anchor().unwrap_or(idx_to_select);
                    // Replace the previous alt/shift range, so alt+clicking
                    // closer to the anchor deselects the items beyond it,
                    // just like backing up with Shift+Up.
                    if let Some((lo, hi)) = current.state.take_range_mark() {
                        for i in lo..=hi {
                            current.state.marked.remove(&i);
                        }
                    }
                    let (lo, hi) = (anchor.min(idx_to_select), anchor.max(idx_to_select));
                    if lo < hi {
                        current.state.mark_range(lo, hi);
                        current.state.set_range_mark(lo, hi);
                    }
                    // lo == hi means the anchor itself was clicked: the old
                    // range was already unmarked, so everything (including
                    // the anchor) is deselected.
                    current.select_idx(idx_to_select, ctx.config.scrolloff);
                    self.fetch_data_internal(ctx);
                }
            }
            MouseEventKind::LeftClick if current_area.contains(position) => {
                let clicked_row: usize = event.y.saturating_sub(current_area.y).into();

                if let Some(idx_to_select) =
                    self.stack().current().state.get_at_rendered_row(clicked_row)
                {
                    let current = self.stack_mut().current_mut();
                    // A plain click on a different row drops the
                    // multi-selection (ctrl/alt clicks above keep their
                    // marking behavior). Clicking the selected row keeps it.
                    if !current.state.marked.is_empty()
                        && Some(idx_to_select) != current.state.get_selected()
                    {
                        current.state.unmark_all();
                    }
                    current.select_idx(idx_to_select, ctx.config.scrolloff);
                    current.state.set_mark_anchor(idx_to_select);
                    current.state.clear_range_mark();
                    self.fetch_data_internal(ctx);
                }
            }
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
                if preview_area.contains(position) =>
            {
                let clicked_row: usize = event.y.saturating_sub(preview_area.y).into();
                // Offset does not need to be accounted for since it is always
                // scrolled all the way to the top when going
                // deeper
                let idx_to_select = self.stack().next_dir_items().and_then(|preview| {
                    if clicked_row < preview.len() { Some(clicked_row) } else { None }
                });

                self.open(false, ctx)?;
                self.stack_mut().current_mut().select_idx(idx_to_select.unwrap_or_default(), 0);

                self.fetch_data_internal(ctx);
            }
            MouseEventKind::ScrollUp if current_area.contains(position) => {
                self.stack_mut()
                    .current_mut()
                    .scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            MouseEventKind::ScrollDown if current_area.contains(position) => {
                self.stack_mut()
                    .current_mut()
                    .scroll_down(ctx.config.scroll_amount, ctx.config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            MouseEventKind::RightClick => {
                let clicked_row: usize = event.y.saturating_sub(current_area.y).into();

                if let Some(idx_to_select) =
                    self.stack().current().state.get_at_rendered_row(clicked_row)
                {
                    self.stack_mut().current_mut().select_idx(idx_to_select, ctx.config.scrolloff);
                    self.fetch_data_internal(ctx);
                }

                self.open_context_menu(ctx)?;
            }
            MouseEventKind::Drag { .. } => {}
            _ => {}
        }

        Ok(())
    }

    fn handle_common_action(&mut self, event: &mut ActionEvent, ctx: &Ctx) -> Result<()> {
        let Some(action) = event.claim_common() else {
            return Ok(());
        };
        let config = &ctx.config;

        match action.to_owned() {
            CommonAction::Up => {
                self.stack_mut().current_mut().prev(config.scrolloff, config.wrap_navigation);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Down => {
                self.stack_mut().current_mut().next(config.scrolloff, config.wrap_navigation);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::MoveUp => {
                self.move_selected(MoveDirection::Up, ctx);
            }
            CommonAction::MoveDown => {
                self.move_selected(MoveDirection::Down, ctx);
            }
            CommonAction::DownHalf => {
                self.stack_mut().current_mut().next_half_viewport(ctx.config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::UpHalf => {
                self.stack_mut().current_mut().prev_half_viewport(ctx.config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::PageUp => {
                self.stack_mut().current_mut().prev_viewport(ctx.config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::PageDown => {
                self.stack_mut().current_mut().next_viewport(ctx.config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Bottom => {
                self.stack_mut().current_mut().last();
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Top => {
                self.stack_mut().current_mut().first();
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Right => {
                self.open(false, ctx)?;
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Left => {
                self.stack_mut().leave();
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::EnterSearch => {
                ctx.input.insert_mode(self.stack().current().filter_buffer_id);
                ctx.input.clear_buffer(self.stack().current().filter_buffer_id);
                self.stack_mut().current_mut().set_filter_active(true);
                ctx.render()?;
            }
            CommonAction::NextResult => {
                self.stack_mut()
                    .current_mut()
                    .jump_next_matching(ctx.config.theme.browser_song_format.0.as_slice(), ctx);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::PreviousResult => {
                self.stack_mut()
                    .current_mut()
                    .jump_previous_matching(ctx.config.theme.browser_song_format.0.as_slice(), ctx);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::InvertSelection => {
                self.stack_mut().current_mut().invert_marked();

                ctx.render()?;
            }
            CommonAction::Select => {
                self.stack_mut().current_mut().toggle_mark_selected();
                self.stack_mut()
                    .current_mut()
                    .next(ctx.config.scrolloff, ctx.config.wrap_navigation);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::SelectDown | CommonAction::SelectUp => {
                // Range-select from the anchor (set by clicks / the first
                // shift-press) to the current selection, then move. Each
                // press replaces the previous range, so backing up unmarks
                // the items the cursor moved past.
                let dir = if matches!(action, CommonAction::SelectDown) { 1 } else { -1 };
                let current = self.stack_mut().current_mut();
                let start = current.state.get_selected().unwrap_or(0);
                if current.state.mark_anchor().is_none() || current.state.marked.is_empty() {
                    current.state.set_mark_anchor(start);
                }
                let anchor = current.state.mark_anchor().unwrap_or(start);
                // Move first so the newly reached row is included in the
                // range and backing up deselects the row being left.
                if dir > 0 {
                    current.next(ctx.config.scrolloff, ctx.config.wrap_navigation);
                } else {
                    current.prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
                }
                let sel = current.state.get_selected().unwrap_or(start);
                // Replace the previous shift range.
                if let Some((lo, hi)) = current.state.take_range_mark() {
                    for i in lo..=hi {
                        current.state.marked.remove(&i);
                    }
                }
                let (lo, hi) = (anchor.min(sel), anchor.max(sel));
                if lo < hi {
                    current.state.mark_range(lo, hi);
                    current.state.set_range_mark(lo, hi);
                }
                // lo == hi means the cursor reached the anchor: the old range
                // was already unmarked, so everything (including the anchor)
                // is deselected.
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Close if !self.stack().current().marked().is_empty() => {
                let current = self.stack_mut().current_mut();
                current.marked_mut().clear();
                current.state.clear_mark_anchor();
                // Esc is bound to both Close and ShowSettings: clearing a
                // selection consumes the keypress, so the settings panel
                // only opens on a second Esc (when nothing is selected).
                event.consume();
                ctx.render()?;
            }
            CommonAction::Delete => {
                let items = self.delete_items(false);
                if !items.is_empty() {
                    let len = items.len();
                    modal!(
                        ctx,
                        ConfirmModal::builder()
                            .ctx(ctx)
                            .message(vec![
                                format!("Are you sure you want to delete {} items?", len),
                                "This action cannot be undone.".into()
                            ])
                            .action(Action::Single {
                                confirm_label: Some("Delete"),
                                cancel_label: None,
                                on_confirm: Box::new(move |ctx| {
                                    ctx.command(move |client| {
                                        client.delete_multiple(items)?;
                                        Ok(())
                                    });
                                    status_info!("Deleted {} items", len);
                                    Ok(())
                                }),
                            })
                            .size((45, 6))
                            .build()
                    );
                    self.stack_mut().current_mut().marked_mut().clear();
                }
            }
            CommonAction::Rename => {
                if let Some(item) = self.stack().current().selected() {
                    Self::rename(item, ctx);
                }
            }
            CommonAction::FocusInput => {}
            CommonAction::Close => {}
            CommonAction::Confirm if self.stack().current().marked().is_empty() => {
                self.open(true, ctx)?;
                self.fetch_data_internal(ctx)?;
                ctx.render()?;
            }
            CommonAction::ShowInfo => {
                if let Some(item) = self.stack().current().selected() {
                    self.show_info(item, ctx);
                }
            }
            CommonAction::Confirm => {}
            CommonAction::PaneDown => {}
            CommonAction::PaneUp => {}
            CommonAction::PaneRight => {}
            CommonAction::PaneLeft => {}
            CommonAction::AddOptions { kind: AddKind::Action(options) } => {
                let (enqueue, hovered_idx) = self.enqueue_items(options.all);
                if !enqueue.is_empty() {
                    let queue_len = ctx.queue.len();
                    let current_song_idx = ctx.find_current_song_in_queue().map(|(i, _)| i);

                    Client::resolve_and_enqueue(
                        ctx,
                        enqueue,
                        options.position,
                        options.autoplay,
                        current_song_idx,
                        hovered_idx,
                    );
                }
            }
            CommonAction::AddOptions { kind: AddKind::Modal(items) } => {
                let opts = items
                    .iter()
                    .map(|(label, opts)| {
                        let enqueue = self.enqueue_items(opts.all);
                        (label.to_owned(), *opts, enqueue)
                    })
                    .collect_vec();

                modal!(ctx, create_add_modal(opts, ctx));
            }
            CommonAction::ContextMenu => {
                self.open_context_menu(ctx)?;
            }
            CommonAction::Rate {
                kind: RateKind::Value(value),
                current: false,
                min_rating: _,
                max_rating: _,
            } => {
                let items = self.enqueue(self.items(false).map(|(_, i)| i)).0;
                ctx.command(move |client| {
                    client.set_sticker_multiple(RATING_STICKER, value.to_string(), items)?;
                    Ok(())
                });
            }
            CommonAction::Rate {
                kind: RateKind::Modal { values, custom, like },
                current: false,
                min_rating,
                max_rating,
            } => {
                let items = self.enqueue(self.items(false).map(|(_, i)| i)).0;
                modal!(
                    ctx,
                    create_rating_modal(
                        items,
                        values.as_slice(),
                        min_rating,
                        max_rating,
                        custom,
                        like,
                        ctx
                    )
                );
            }
            CommonAction::Rate { kind: RateKind::Like(), current: false, .. } => {
                let items = self.enqueue(self.items(false).map(|(_, i)| i)).0;
                ctx.command(move |client| {
                    client.set_sticker_multiple(LIKE_STICKER, "2".to_string(), items)?;
                    Ok(())
                });
            }
            CommonAction::Rate { kind: RateKind::Neutral(), current: false, .. } => {
                let items = self.enqueue(self.items(false).map(|(_, i)| i)).0;
                ctx.command(move |client| {
                    client.set_sticker_multiple(LIKE_STICKER, "1".to_string(), items)?;
                    Ok(())
                });
            }
            CommonAction::Rate { kind: RateKind::Dislike(), current: false, .. } => {
                let items = self.enqueue(self.items(false).map(|(_, i)| i)).0;
                ctx.command(move |client| {
                    client.set_sticker_multiple(LIKE_STICKER, "0".to_string(), items)?;
                    Ok(())
                });
            }
            CommonAction::Rate { kind: _, current: true, min_rating: _, max_rating: _ } => {
                event.abandon();
            }
            CommonAction::Save { kind: SaveKind::Playlist { name, all, duplicates_strategy } } => {
                let list_songs = self.list_songs_in_items(all);
                let all_songs: Vec<_> = ctx.query_sync(move |client| {
                    Ok(list_songs(client)?.into_iter().map(|s| s.file).collect())
                })?;

                if all_songs.is_empty() {
                    status_warn!("No songs selected to save");
                    return Ok(());
                }

                add_to_playlist_or_show_modal(name, all_songs, duplicates_strategy, ctx);
            }
            CommonAction::Save { kind: SaveKind::Modal { all, duplicates_strategy } } => {
                let list_songs = self.list_songs_in_items(all);
                let song_paths: Vec<_> = ctx.query_sync(move |client| {
                    Ok(list_songs(client)?.into_iter().map(|s| s.file).collect())
                })?;

                if song_paths.is_empty() {
                    status_warn!("No songs selected to save");
                    return Ok(());
                }

                let modal = create_save_modal(
                    song_paths,
                    self.initial_playlist_name(all),
                    duplicates_strategy,
                    ctx,
                )?;
                modal!(ctx, modal);
            }
            CommonAction::DeleteFromPlaylist {
                kind: DeleteKind::Playlist { name, all, confirmation },
            } => {
                let list_songs = self.list_songs_in_items(all);
                let song_paths: HashSet<_> = ctx.query_sync(move |client| {
                    Ok(list_songs(client)?.into_iter().map(|s| s.file).collect())
                })?;

                if song_paths.is_empty() {
                    status_warn!("No songs selected to delete");
                    return Ok(());
                }

                delete_from_playlist_or_show_confirmation(name, &song_paths, confirmation, ctx)?;
            }
            CommonAction::DeleteFromPlaylist { kind: DeleteKind::Modal { all, confirmation } } => {
                let list_songs = self.list_songs_in_items(all);
                let song_paths: HashSet<_> = ctx.query_sync(move |client| {
                    Ok(list_songs(client)?.into_iter().map(|s| s.file).collect())
                })?;

                if song_paths.is_empty() {
                    status_warn!("No songs selected to delete");
                    return Ok(());
                }

                let modal = create_delete_modal(song_paths, confirmation, ctx)?;
                modal!(ctx, modal);
            }
        }

        Ok(())
    }

    fn items<'a>(&'a self, all: bool) -> Box<dyn Iterator<Item = (usize, &'a T)> + 'a> {
        if all {
            Box::new(self.stack().current().items.iter().enumerate())
        } else if self.stack().current().marked().is_empty() {
            if let Some((idx, item)) = self.stack().current().selected_with_idx() {
                Box::new(std::iter::once((idx, item)))
            } else {
                Box::new(std::iter::empty::<(usize, &T)>())
            }
        } else {
            Box::new(
                self.stack()
                    .current()
                    .marked()
                    .iter()
                    .map(|idx| (*idx, &self.stack().current().items[*idx])),
            )
        }
    }

    fn delete_items(&self, all: bool) -> Vec<MpdDelete> {
        self.delete(self.items(all))
    }

    /// If `all` is true, returns `Enqueue` for all items in the current stack
    /// dir. Otherwise returns `Enqueue` for the currently hovered item if no
    /// items are marked or a list of `Enqueue` for all marked items.
    fn enqueue_items(&self, all: bool) -> (Vec<Enqueue>, Option<usize>) {
        self.enqueue(self.items(all).map(|(_, item)| item))
    }

    fn open_context_menu(&mut self, ctx: &Ctx) -> Result<()> {
        let list_songs_in_items = self
            .items(false)
            .map(|(_, item)| self.list_songs_in_item(item.to_owned()))
            .collect_vec();

        let modal = MenuModal::new(ctx)
            .list_section(ctx, |mut section| {
                let (current_items, _) = self.enqueue_items(false);
                if !current_items.is_empty() {
                    let cloned_items = current_items.clone();
                    section.add_item("Add to queue", move |ctx| {
                        ctx.command(move |client| {
                            client.enqueue_multiple(cloned_items, None, None, false)?;
                            Ok(())
                        });
                        Ok(())
                    });
                    let cloned_items = current_items.clone();
                    section.add_item("Replace queue", move |ctx| {
                        ctx.command(move |client| {
                            client.enqueue_multiple(cloned_items, None, None, true)?;
                            Ok(())
                        });
                        Ok(())
                    });
                }

                let songs_in_items_clone = list_songs_in_items.clone();
                let initial_playlist_name = self.initial_playlist_name(false);
                section.add_item("Create playlist", move |ctx| {
                    modal!(
                        ctx,
                        InputModal::new(ctx)
                            .title("Create new playlist")
                            .confirm_label("Save")
                            .input_label("Playlist name:")
                            .initial_value(initial_playlist_name.unwrap_or_default())
                            .on_confirm(move |ctx, value| {
                                let value = value.to_owned();
                                ctx.command(move |client| {
                                    let items: Vec<_> = songs_in_items_clone
                                        .into_iter()
                                        .map(|cb| -> Result<_> { cb(client) })
                                        .collect::<Result<Vec<Vec<_>>>>()?
                                        .into_iter()
                                        .flatten()
                                        .collect();
                                    client.create_playlist(
                                        &value,
                                        items.into_iter().map(|s| s.file).collect(),
                                    )?;

                                    Ok(())
                                });
                                Ok(())
                            })
                    );
                    Ok(())
                });

                section.add_item("Add to playlist", move |ctx| {
                    // The radio favourites playlist is Radio-tab-owned: it
                    // never appears as an add target.
                    let radio_playlist = ctx.config.radio.playlist.clone();
                    let (items, playlists) = ctx.query_sync(move |client| {
                        let items: Vec<_> = list_songs_in_items
                            .into_iter()
                            .map(|cb| -> Result<_> { cb(client) })
                            .collect::<Result<Vec<Vec<_>>>>()?
                            .into_iter()
                            .flatten()
                            .collect();
                        let playlists = client
                            .picker_playlists(&radio_playlist)?
                            .into_iter()
                            .map(|p| p.name)
                            .collect_vec();
                        Ok((items, playlists))
                    })?;

                    modal!(
                        ctx,
                        SelectModal::builder()
                            .ctx(ctx)
                            .options(playlists)
                            .confirm_label("Add")
                            .title("Select a playlist")
                            .on_confirm(move |ctx, selected, _idx| {
                                ctx.command(move |client| {
                                    client.add_to_playlist_multiple(
                                        &selected,
                                        items.into_iter().map(|s| s.file).collect_vec(),
                                    )?;
                                    Ok(())
                                });
                                Ok(())
                            })
                            .build()
                    );
                    Ok(())
                });

                Some(section)
            })
            .list_section(ctx, |mut section| {
                let current_item = self.stack().current().selected().cloned();
                if let Some(item) = current_item {
                    let is_renameable =
                        self.stack().current().selected().is_some_and(|item| self.can_rename(item));
                    if is_renameable {
                        section.add_item("Rename", move |ctx| {
                            Self::rename(&item, ctx)?;
                            Ok(())
                        });
                    }
                }

                if section.items.is_empty() { None } else { Some(section) }
            })
            .list_section(ctx, |mut section| {
                // TODO Deletion cannot be currently done as we need to clear the marked items
                // after the deletion occurs but do not have access to the pane's state in the
                // callback. An event should be dispatched upon deletion to clear the items or
                // better yet, the marked items need to be refactored directly into the
                // `DirStackItem` directly.

                // if !to_delete.is_empty() {
                //     section.add_item("Delete", move |ctx| {
                //         if !to_delete.is_empty() {
                //             ctx.command(move |client| {
                //                 client.delete_multiple(to_delete)?;
                //                 Ok(())
                //             });
                //         }
                //         Ok(())
                //     });
                // }
                //
                // if !all_to_delete.is_empty() {
                //     section.add_item("Delete all", move |ctx| {
                //         if !all_to_delete.is_empty() {
                //             ctx.command(move |client| {
                //                 client.delete_multiple(all_to_delete)?;
                //                 Ok(())
                //             });
                //         }
                //         Ok(())
                //     });
                // }

                if section.items.is_empty() { None } else { Some(section) }
            })
            .list_section(ctx, |section| {
                let section = section.item("Cancel", |_ctx| Ok(()));
                Some(section)
            })
            .build();

        modal!(ctx, modal);
        Ok(())
    }
}

#[cfg(test)]
mod scrollbar_tests {
    use ratatui::layout::Rect;

    use crate::shared::mouse_event::{MouseEvent, MouseEventKind};

    #[test]
    fn test_mouse_event_in_scrollbar_area() {
        let scrollbar_area = Rect::new(29, 1, 1, 8);

        let inside_event = MouseEvent { kind: MouseEventKind::LeftClick, x: 29, y: 3 , ..Default::default() };
        assert!(scrollbar_area.contains(inside_event.into()));

        let outside_event = MouseEvent { kind: MouseEventKind::LeftClick, x: 28, y: 3 , ..Default::default() };
        assert!(!scrollbar_area.contains(outside_event.into()));
    }

    #[test]
    fn test_scrollbar_drag_events() {
        let scrollbar_area = Rect::new(29, 1, 1, 8);
        let drag_start = ratatui::layout::Position { x: 29, y: 1 };
        let drag_event = MouseEvent {
            kind: MouseEventKind::Drag { drag_start_position: drag_start },
            x: 29,
            y: 5,
        modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(scrollbar_area.contains(drag_event.into()));
        assert!(matches!(drag_event.kind, MouseEventKind::Drag { .. }));
    }
}

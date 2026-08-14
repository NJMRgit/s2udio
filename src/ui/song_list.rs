use std::collections::HashSet;

use anyhow::Result;
use itertools::Itertools;
use ratatui::{prelude::Rect, widgets::ListState};

use crate::{
    MpdQueryResult,
    config::{
        keys::{
            CommonAction,
            GlobalAction,
            actions::{AddKind, DeleteKind, RateKind, SaveKind},
        },
        theme::properties::{Property, SongProperty},
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
        dirstack::{Dir, DirStackItem, ScrollingState},
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
    },
};

#[derive(Debug, Clone, Copy)]
pub enum MoveDirection {
    Up,
    Down,
}

/// A generic "list of items" pane core, extracted from `BrowserPane`
/// (browser.rs) by removing the `DirStack` assumption: every list-like
/// pane — the dir-stack browsers (albums / tag browser / playlists), the
/// queue's Audio list, the search results list — shares the same
/// selection / marking / filter / scrollbar / mouse / action mechanics.
///
/// A pane implements this trait over a flat [`Dir`] (`items` + `DirState`
/// + filter); the shared default methods provide all behavior, and
/// per-pane differences are the hooks below (open/leave semantics, item
/// -> songs mapping, enqueue shape, which actions apply, the filter
/// format, the clickable list area, …).
///
/// `BrowserPane<T>` extends this trait with the dir-stack-specific parts
/// (stack navigation, three-column areas, walk-based enqueue). Queue and
/// search panes implement this trait directly.
///
/// Phase-1 consolidation target of docs/design/Rewrite/ui-reuse-rewrite.md.
#[allow(unused)]
pub(in crate::ui) trait SongListCore<T, S = ListState>: Pane
where
    T: DirStackItem + std::fmt::Debug + Clone + Send + Sync + 'static,
    S: ScrollingState + std::fmt::Debug + Default + 'static,
{
    // ── required: the list this core operates on ──────────────────────

    fn list(&self) -> &Dir<T, S>;
    fn list_mut(&mut self) -> &mut Dir<T, S>;

    // ── hooks (per-pane differences) ───────────────────────────────────

    /// The scrollbar strip this list renders in (None = no scrollbar).
    fn scrollbar_area(&self) -> Option<Rect> {
        None
    }

    /// The clickable list area (used for row math and click guards).
    fn list_area(&self) -> Option<Rect> {
        None
    }

    /// The property format used for filter jump-matching. Owned so
    /// adopters can return either a config format or a pane-owned one
    /// (e.g. the queue's column formats).
    fn song_format(&self, ctx: &Ctx) -> Vec<Property<SongProperty>> {
        ctx.config.theme.browser_song_format.0.clone()
    }

    /// Activate the item under the selection (Enter / Right / double
    /// click). Dir-stack panes descend into directories; flat lists play
    /// or open their own menu.
    fn open(&mut self, _autoplay: bool, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    /// Go back / leave (Left arrow). Dir-stack panes pop the stack.
    fn leave(&mut self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    /// Fetch data for a directory that has not been loaded yet.
    fn fetch_data(&self, _selected: &T, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    /// Called after navigation/selection changes; panes that lazily load
    /// data refetch here. Flat lists have nothing to do.
    fn fetch_data_internal(&mut self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    /// Turn the given items into MPD enqueue entries, returning the index
    /// of the hovered item (if any) for autoplay positioning.
    fn enqueue<'a>(&self, items: impl Iterator<Item = &'a T>) -> (Vec<Enqueue>, Option<usize>) {
        let hovered = self.list().selected();
        let (items, idx) = items
            .enumerate()
            .fold((Vec::new(), None), |mut acc, (idx, item)| {
                let path = item.as_path().to_owned();
                if let Some(hovered) = hovered
                    && hovered.is_file()
                    && hovered.as_path() == path
                {
                    acc.1 = Some(idx);
                }
                acc.0.push(Enqueue::File { path });

                acc
            });

        (items, idx)
    }

    /// Resolve one item to its songs (used by save / delete-from-playlist
    /// / external commands / the context menu).
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

    fn initial_playlist_name(&self, _all: bool) -> Option<String> {
        None
    }

    fn delete<'a>(&self, _item: impl Iterator<Item = (usize, &'a T)>) -> Vec<MpdDelete> {
        Vec::new()
    }

    fn can_rename(&self, _item: &T) -> bool {
        false
    }

    fn rename(_item: &T, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn move_selected(&mut self, _direction: MoveDirection, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn show_info(&self, _item: &T, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    // ── shared default behavior ────────────────────────────────────────

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        let song_format = self.song_format(ctx);
        match kind {
            InputResultEvent::Push => {
                self.list_mut().jump_first_matching(&song_format, ctx);
                self.list_mut().recalculate_matched_items(&song_format, ctx);
                self.fetch_data_internal(ctx);
            }
            InputResultEvent::Pop => {
                self.list_mut().recalculate_matched_items(&song_format, ctx);
            }
            InputResultEvent::Confirm => {}
            InputResultEvent::NoChange => {}
            InputResultEvent::Cancel => {
                self.list_mut().set_filter_active(false);
                ctx.input.clear_buffer(self.list().filter_buffer_id);
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
                if !self.list().marked().is_empty() =>
            {
                let marked_items: Vec<_> = self
                    .list()
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
                if let Some(selected) = self.list().selected() {
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
        let Some(scrollbar_area) = self.scrollbar_area() else {
            return Ok(false);
        };

        if !matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }) {
            return Ok(false);
        }

        let current = self.list();
        let items = current.items.len();
        let viewport_len = current.state.viewport_len().unwrap_or(scrollbar_area.height as usize);
        // The rendered scrollbar's content_length is max_offset + 1, so the
        // geometry (thumb size / travel) matches the widget exactly.
        let content_len = items.saturating_sub(viewport_len).saturating_add(1).max(1);
        let position = current.state.inner.offset();
        let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
        // The immutable borrow (`current`) ends here; re-borrow mutably for
        // the drag.
        let state = &mut self.list_mut().state;
        if let Some(perc) = state
            .scrollbar_drag
            .handle(event, scrollbar_area, content_len, viewport_len, position, begin_len, end_len)
        {
            let before = self.list().selected_with_idx().map(|(i, _)| i);
            self.list_mut().scroll_to(perc, ctx.config.scrolloff);
            if before != self.list().selected_with_idx().map(|(i, _)| i) {
                self.fetch_data_internal(ctx);
            }
            ctx.render()?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Mouse handling for the list area itself (click/ctrl/alt/double/
    /// middle/wheel/right-click). Pane-specific arms (dir-stack previous/
    /// preview areas, mode toggles) live in the pane's `handle_mouse_event`
    /// / `handle_mouse_action` override.
    fn handle_list_mouse_action(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let Some(area) = self.list_area() else {
            return Ok(());
        };

        let position = event.into();
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
                if area.contains(position)
                    && event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                let clicked_row: usize = event.y.saturating_sub(area.y).into();

                if let Some(idx_to_select) = self.list().state.get_at_rendered_row(clicked_row) {
                    let current = self.list_mut();
                    current.select_idx(idx_to_select, ctx.config.scrolloff);
                    current.state.toggle_mark(idx_to_select);
                    self.fetch_data_internal(ctx);
                }
            }
            MouseEventKind::LeftClick
                if area.contains(position)
                    && event.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                let clicked_row: usize = event.y.saturating_sub(area.y).into();

                if let Some(idx_to_select) = self.list().state.get_at_rendered_row(clicked_row) {
                    let current = self.list_mut();
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
            MouseEventKind::LeftClick if area.contains(position) => {
                let clicked_row: usize = event.y.saturating_sub(area.y).into();

                if let Some(idx_to_select) = self.list().state.get_at_rendered_row(clicked_row) {
                    let current = self.list_mut();
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
            MouseEventKind::DoubleClick if area.contains(position) => {
                let clicked_row: usize = event.y.saturating_sub(area.y).into();

                if let Some(_) = self.list().state.get_at_rendered_row(clicked_row) {
                    self.open(false, ctx)?;
                    self.fetch_data_internal(ctx);
                }
            }
            MouseEventKind::MiddleClick if area.contains(position) => {
                let clicked_row: usize = event.y.saturating_sub(area.y).into();

                if let Some(idx_to_select) = self.list().state.get_at_rendered_row(clicked_row) {
                    self.list_mut().select_idx(idx_to_select, ctx.config.scrolloff);
                    if let Some(item) = self.list().selected() {
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
            MouseEventKind::ScrollUp if area.contains(position) => {
                self.list_mut().scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            MouseEventKind::ScrollDown if area.contains(position) => {
                self.list_mut().scroll_down(ctx.config.scroll_amount, ctx.config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            MouseEventKind::RightClick => {
                let clicked_row: usize = event.y.saturating_sub(area.y).into();

                if let Some(idx_to_select) = self.list().state.get_at_rendered_row(clicked_row) {
                    self.list_mut().select_idx(idx_to_select, ctx.config.scrolloff);
                    self.fetch_data_internal(ctx);
                }

                self.open_context_menu(ctx)?;
            }
            MouseEventKind::Drag { .. } => {}
            _ => {}
        }

        Ok(())
    }

    fn handle_mouse_action(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if self.handle_scrollbar_interaction(event, ctx)? {
            return Ok(());
        }

        self.handle_list_mouse_action(event, ctx)
    }

    fn handle_common_action(&mut self, event: &mut ActionEvent, ctx: &Ctx) -> Result<()> {
        let Some(action) = event.claim_common() else {
            return Ok(());
        };

        self.handle_claimed_common_action(action.to_owned(), event, ctx)
    }

    /// The shared `CommonAction` arms. Panes with custom arms for some
    /// actions claim the action themselves and delegate the rest here.
    #[allow(clippy::too_many_lines)]
    fn handle_claimed_common_action(
        &mut self,
        action: CommonAction,
        event: &mut ActionEvent,
        ctx: &Ctx,
    ) -> Result<()> {
        let config = &ctx.config;

        match action {
            CommonAction::Up => {
                self.list_mut().prev(config.scrolloff, config.wrap_navigation);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Down => {
                self.list_mut().next(config.scrolloff, config.wrap_navigation);
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
                self.list_mut().next_half_viewport(config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::UpHalf => {
                self.list_mut().prev_half_viewport(config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::PageUp => {
                self.list_mut().prev_viewport(config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::PageDown => {
                self.list_mut().next_viewport(config.scrolloff);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Bottom => {
                self.list_mut().last();
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Top => {
                self.list_mut().first();
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Right => {
                self.open(false, ctx)?;
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::Left => {
                self.leave(ctx)?;
            }
            CommonAction::EnterSearch => {
                ctx.input.insert_mode(self.list().filter_buffer_id);
                ctx.input.clear_buffer(self.list().filter_buffer_id);
                self.list_mut().set_filter_active(true);
                ctx.render()?;
            }
            CommonAction::NextResult => {
                let song_format = self.song_format(ctx);
                self.list_mut().jump_next_matching(&song_format, ctx);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::PreviousResult => {
                let song_format = self.song_format(ctx);
                self.list_mut().jump_previous_matching(&song_format, ctx);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::SelectAll => {
                let len = self.list().len();
                if len > 0 {
                    self.list_mut().state.mark_range(0, len - 1);
                }
                ctx.render()?;
            }
            CommonAction::InvertSelection => {
                self.list_mut().invert_marked();

                ctx.render()?;
            }
            CommonAction::Select => {
                self.list_mut().toggle_mark_selected();
                self.list_mut().next(config.scrolloff, config.wrap_navigation);
                self.fetch_data_internal(ctx);
                ctx.render()?;
            }
            CommonAction::SelectDown | CommonAction::SelectUp => {
                // Range-select from the anchor (set by clicks / the first
                // shift-press) to the current selection, then move. Each
                // press replaces the previous range, so backing up unmarks
                // the items the cursor moved past.
                let dir = if matches!(action, CommonAction::SelectDown) { 1 } else { -1 };
                let current = self.list_mut();
                let start = current.state.get_selected().unwrap_or(0);
                if current.state.mark_anchor().is_none() || current.state.marked.is_empty() {
                    current.state.set_mark_anchor(start);
                }
                let anchor = current.state.mark_anchor().unwrap_or(start);
                // Move first so the newly reached row is included in the
                // range and backing up deselects the row being left.
                if dir > 0 {
                    current.next(config.scrolloff, config.wrap_navigation);
                } else {
                    current.prev(config.scrolloff, config.wrap_navigation);
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
            CommonAction::Close if !self.list().marked().is_empty() => {
                let current = self.list_mut();
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
                    self.list_mut().marked_mut().clear();
                }
            }
            CommonAction::Rename => {
                if let Some(item) = self.list().selected() {
                    Self::rename(item, ctx);
                }
            }
            CommonAction::FocusInput => {}
            CommonAction::Close => {}
            CommonAction::Confirm if self.list().marked().is_empty() => {
                self.open(true, ctx)?;
                self.fetch_data_internal(ctx)?;
                ctx.render()?;
            }
            CommonAction::ShowInfo => {
                if let Some(item) = self.list().selected() {
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
            CommonAction::LyricsNudgeUp | CommonAction::LyricsNudgeDown
            | CommonAction::LyricsSave => {}
        }

        Ok(())
    }

    fn items<'a>(&'a self, all: bool) -> Box<dyn Iterator<Item = (usize, &'a T)> + 'a> {
        if all {
            Box::new(self.list().items.iter().enumerate())
        } else if self.list().marked().is_empty() {
            if let Some((idx, item)) = self.list().selected_with_idx() {
                Box::new(std::iter::once((idx, item)))
            } else {
                Box::new(std::iter::empty::<(usize, &T)>())
            }
        } else {
            Box::new(
                self.list()
                    .marked()
                    .iter()
                    .map(|idx| (*idx, &self.list().items[*idx])),
            )
        }
    }

    fn delete_items(&self, all: bool) -> Vec<MpdDelete> {
        self.delete(self.items(all))
    }

    /// If `all` is true, returns `Enqueue` for all items in the list.
    /// Otherwise returns `Enqueue` for the currently hovered item if no
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
                let current_item = self.list().selected().cloned();
                if let Some(item) = current_item {
                    let is_renameable =
                        self.list().selected().is_some_and(|item| self.can_rename(item));
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

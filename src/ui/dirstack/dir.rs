use std::{collections::BTreeSet, ops::{Bound, RangeBounds}};
use log::error;
use ratatui::{text::Span, widgets::ListItem};
use super::{DirStackItem, state::DirState};
use crate::{
    config::theme::properties::{Property, SongProperty},
    ctx::Ctx, shared::macros::status_warn,
    ui::{FILTER_PREFIX, dirstack::ScrollingState, input::BufferId},
};
#[derive(Debug)]
pub struct Dir<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    pub items: Vec<T>,
    pub state: DirState<S>,
    matched_item_count: usize,
    pub filter_buffer_id: BufferId,
    pub filter_active: bool,
}
impl<T, S> Default for Dir<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    fn default() -> Self {
        Self {
            items: Vec::default(),
            state: DirState::default(),
            matched_item_count: 0,
            filter_buffer_id: BufferId::new(),
            filter_active: false,
        }
    }
}
#[allow(dead_code)]
impl<T, S> Dir<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    pub fn new(root: Vec<T>) -> Self {
        let mut result = Self {
            items: Vec::new(),
            state: DirState::default(),
            matched_item_count: 0,
            filter_buffer_id: BufferId::new(),
            filter_active: false,
        };
        if !root.is_empty() {
            result.state.select(Some(0), 0);
            result.state.set_content_len(Some(root.len()));
            result.items = root;
        }
        result
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn new_with_state(items: Vec<T>, state: DirState<S>) -> Self {
        return Self {
            items,
            state,
            matched_item_count: 0,
            filter_buffer_id: BufferId::new(),
            filter_active: false,
        };
    }
    pub fn set_filter_active(&mut self, active: bool) {
        self.filter_active = active;
    }
    pub fn filter(&self, ctx: &Ctx) -> String {
        ctx.input.value(self.filter_buffer_id)
    }
    pub fn filter_text<'a>(
        &self,
        available_width: u16,
        ctx: &Ctx,
    ) -> Option<Vec<Span<'a>>> {
        self.filter_active
            .then(|| {
                ctx.input
                    .as_spans_prefixed(
                        self.filter_buffer_id,
                        FILTER_PREFIX,
                        available_width,
                        ctx.config.as_border_style(),
                        ctx.input.is_active(self.filter_buffer_id),
                    )
            })
    }
    pub fn recalculate_matched_items(
        &mut self,
        song_format: &[Property<SongProperty>],
        ctx: &Ctx,
    ) {
        let filter = ctx.input.value(self.filter_buffer_id);
        self.matched_item_count = if self.filter_active {
            self.items
                .iter()
                .filter(|item| item.matches(song_format, ctx, &filter))
                .count()
        } else {
            0
        };
    }
    pub fn to_list_items_range<'a>(
        &self,
        range: impl RangeBounds<usize>,
        song_format: &[Property<SongProperty>],
        ctx: &Ctx,
    ) -> Vec<ListItem<'a>> {
        let mut already_matched: u32 = 0;
        let current_item_idx = self.selected_with_idx().map(|(idx, _)| idx);
        let start = match range.start_bound() {
            Bound::Included(&start) => start,
            Bound::Excluded(start) => start + 1,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(end) => end + 1,
            Bound::Excluded(&end) => end,
            Bound::Unbounded => self.items.len(),
        };
        let filter = ctx.input.value(self.filter_buffer_id);
        self.items
            .iter()
            .enumerate()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|(i, item)| {
                let matches = if self.filter_active {
                    item.matches(song_format, ctx, &filter)
                } else {
                    false
                };
                let is_current = current_item_idx.is_some_and(|idx| i == idx);
                if matches {
                    already_matched = already_matched.saturating_add(1);
                }
                let content = if matches && is_current {
                    Some(format!(" [{already_matched}/{}]", self.matched_item_count))
                } else {
                    None
                };
                item.to_list_item(ctx, self.marked().contains(&i), matches, content)
            })
            .collect()
    }
    pub fn to_list_items<'a>(
        &self,
        song_format: &[Property<SongProperty>],
        ctx: &Ctx,
    ) -> Vec<ListItem<'a>> {
        self.to_list_items_range(.., song_format, ctx)
    }
    pub fn selected(&self) -> Option<&T> {
        if let Some(sel) = self.state.get_selected() {
            self.items.get(sel)
        } else {
            None
        }
    }    pub fn selected_with_idx(&self) -> Option<(usize, &T)> {
        if let Some(sel) = self.state.get_selected() {
            self.items.get(sel).map(|v| (sel, v))
        } else {
            None
        }
    }
    pub fn marked_items(&self) -> impl Iterator<Item = &T> {
        self.state.marked.iter().filter_map(|idx| self.items.get(*idx))
    }
    pub fn marked(&self) -> &BTreeSet<usize> {
        &self.state.marked
    }
    pub fn marked_mut(&mut self) -> &mut BTreeSet<usize> {
        &mut self.state.marked
    }
    pub fn unmark_all(&mut self) {
        self.state.unmark_all();
    }
    pub fn invert_marked(&mut self) {
        self.state.invert_marked();
    }
    pub fn toggle_mark_selected(&mut self) -> bool {
        if let Some(sel) = self.state.get_selected() {
            self.state.toggle_mark(sel)
        } else {
            false
        }
    }
    pub fn remove(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.items.remove(idx);
        }
        self.state.remove(idx);
    }
    pub fn next(&mut self, scrolloff: usize, wrap: bool) {
        self.state.next(scrolloff, wrap);
    }
    pub fn prev(&mut self, scrolloff: usize, wrap: bool) {
        self.state.prev(scrolloff, wrap);
    }
    pub fn select_idx_opt(&mut self, idx: Option<usize>, scrolloff: usize) {
        self.state.select(idx, scrolloff);
    }
    pub fn select_idx(&mut self, idx: usize, scrolloff: usize) {
        self.state.select(Some(idx), scrolloff);
    }
    /// Select `idx` and scroll it to the first visible row (see
    /// [`DirState::select_at_top`]).
    pub fn select_at_top(&mut self, idx: usize) {
        self.state.select_at_top(idx);
    }
    pub fn next_half_viewport(&mut self, scrolloff: usize) {
        self.state.next_half_viewport(scrolloff);
    }
    pub fn prev_half_viewport(&mut self, scrolloff: usize) {
        self.state.prev_half_viewport(scrolloff);
    }
    pub fn next_viewport(&mut self, scrolloff: usize) {
        self.state.next_viewport(scrolloff);
    }
    pub fn prev_viewport(&mut self, scrolloff: usize) {
        self.state.prev_viewport(scrolloff);
    }
    pub fn scroll_to(&mut self, perc: f64, scrolloff: usize) {
        self.state.scroll_to(perc, scrolloff);
    }
    pub fn scroll_down(&mut self, amount: usize, scrolloff: usize) {
        self.state.scroll_down(amount, scrolloff);
    }
    pub fn scroll_up(&mut self, amount: usize, scrolloff: usize) {
        self.state.scroll_up(amount, scrolloff);
    }
    /// Scroll the viewport by `dir * amount` rows without moving the
    /// selection (round-32 wheel behavior).
    pub fn scroll_viewport(&mut self, dir: i64, amount: usize) {
        self.state.scroll_viewport(dir, amount);
    }
    pub fn last(&mut self) {
        self.state.last();
    }
    pub fn first(&mut self) {
        self.state.first();
    }
    pub fn jump_next_matching(
        &mut self,
        song_format: &[Property<SongProperty>],
        ctx: &Ctx,
    ) {
        if !self.filter_active {
            status_warn!("No filter set");
            return;
        }
        let Some(selected) = self.state.get_selected() else {
            error!(state:? = self.state; "No song selected");
            return;
        };
        let length = self.items.len();
        let filter = ctx.input.value(self.filter_buffer_id);
        for i in selected + 1..length + selected {
            let i = i % length;
            if self.items[i].matches(song_format, ctx, &filter) {
                self.state.select(Some(i), ctx.config.scrolloff);
                break;
            }
        }
    }
    pub fn jump_previous_matching(
        &mut self,
        song_format: &[Property<SongProperty>],
        ctx: &Ctx,
    ) {
        if !self.filter_active {
            status_warn!("No filter set");
            return;
        }
        let Some(selected) = self.state.get_selected() else {
            error!(state:? = self.state; "No song selected");
            return;
        };
        let length = self.items.len();
        let filter = ctx.input.value(self.filter_buffer_id);
        for i in (0..length).rev() {
            let i = (i + selected) % length;
            if self.items[i].matches(song_format, ctx, &filter) {
                self.state.select(Some(i), ctx.config.scrolloff);
                break;
            }
        }
    }
    pub fn jump_first_matching(
        &mut self,
        song_format: &[Property<SongProperty>],
        ctx: &Ctx,
    ) {
        if !self.filter_active {
            status_warn!("No filter set");
            return;
        }
        let filter = ctx.input.value(self.filter_buffer_id);
        self.items
            .iter()
            .enumerate()
            .find(|(_, item)| item.matches(song_format, ctx, &filter))
            .inspect(|(idx, _)| self.state.select(Some(*idx), ctx.config.scrolloff));
    }
}

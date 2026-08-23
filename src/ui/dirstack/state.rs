use std::{collections::BTreeSet, ops::SubAssign};
use ratatui::widgets::ScrollbarState;
use super::ScrollingState;
#[derive(Debug, Default)]
pub struct DirState<T: ScrollingState> {
    pub scrollbar_state: ScrollbarState,
    pub inner: T,
    pub marked: BTreeSet<usize>,
    mark_anchor: Option<usize>,
    /// The range last marked by shift+arrow selection, so contracting the
    /// range can unmark the items the cursor moved past.
    range_mark: Option<(usize, usize)>,
    content_len: Option<usize>,
    viewport_len: Option<usize>,
    /// Drag state so a scrollbar drag keeps the thumb under the pointer
    /// (1:1) instead of mapping the pointer onto the whole track.
    pub scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
}
#[allow(dead_code)]
impl<T: ScrollingState> DirState<T> {
    pub fn viewport_len(&self) -> Option<usize> {
        self.viewport_len
    }
    pub fn set_viewport_len(&mut self, viewport_len: Option<usize>) {
        match (self.content_len, viewport_len) {
            (Some(content_len), Some(viewport_len)) => {
                self.set_content_and_viewport_len(content_len, viewport_len);
            }
            (None, Some(viewport_len)) => {
                self.viewport_len = Some(viewport_len);
            }
            (_, None) => {
                self.viewport_len = None;
            }
        }
    }
    pub fn set_content_len(&mut self, content_len: Option<usize>) {
        match (content_len, self.viewport_len) {
            (Some(content_len), Some(viewport_len)) => {
                self.set_content_and_viewport_len(content_len, viewport_len);
            }
            (Some(content_len), None) => {
                self.content_len = Some(content_len);
            }
            (None, _) => {
                self.content_len = None;
            }
        }
    }
    pub fn set_content_and_viewport_len(
        &mut self,
        content_len: usize,
        viewport_len: usize,
    ) {
        self.content_len = Some(content_len);
        self.viewport_len = Some(viewport_len);
        if content_len <= viewport_len {
            self.scrollbar_state = self
                .scrollbar_state
                .viewport_content_length(1)
                .content_length(1);
        } else {
            self.scrollbar_state = self
                .scrollbar_state
                .viewport_content_length(viewport_len)
                .content_length(1 + content_len - viewport_len);
        }
    }
    /// Scrolls to a specific percentage of the content.
    /// `perc` must be between 0.0 and 1.0
    pub fn scroll_to(&mut self, perc: f64, scrolloff: usize) {
        debug_assert!(
            (0.0..= 1.0).contains(& perc), "Percentage must be between 0.0 and 1.0"
        );
        let Some(viewport_len) = self.viewport_len else {
            return;
        };
        let Some(content_len) = self.content_len else {
            return;
        };
        let max_offset = content_len.saturating_sub(viewport_len) as f64;
        let new_offset = (perc * max_offset).floor() as usize;
        self.inner.set_offset(new_offset);
        self.scrollbar_state = self.scrollbar_state.position(new_offset);
        self.clamp_to_offset(scrolloff);
    }
    pub fn content_len(&self) -> Option<usize> {
        self.content_len
    }
    pub fn first(&mut self) {
        if self.content_len.is_some_and(|v| v > 0) {
            self.select(Some(0), 0);
        } else {
            self.select(None, 0);
        }
    }
    pub fn last(&mut self) {
        if let Some(item_count) = self.content_len {
            if item_count > 0 {
                self.select(Some(item_count.saturating_sub(1)), 0);
            } else {
                self.select(None, 0);
            }
        } else {
            self.select(None, 0);
        }
    }
    /// Select `idx` and scroll it to the first visible row — as close to
    /// the top as the viewport allows (the offset clamps so the list never
    /// scrolls past its end). Used at app start to land the queue on the
    /// currently playing song, instead of `select`'s keep-visible or
    /// center behavior.
    pub fn select_at_top(&mut self, idx: usize) {
        let Some(content_len) = self.content_len else {
            return;
        };
        if content_len == 0 {
            self.inner.select_scrolling(None);
            self.scrollbar_state = self.scrollbar_state.position(0);
            return;
        }
        let idx = idx.min(content_len.saturating_sub(1));
        self.inner.select_scrolling(Some(idx));
        let max_offset = content_len
            .saturating_sub(self.viewport_len.unwrap_or_default());
        self.inner.set_offset(idx.min(max_offset));
        self.scrollbar_state = self.scrollbar_state.position(self.offset());
    }
    pub fn next(&mut self, scrolloff: usize, wrap: bool) {
        if wrap {
            self.next_wrapping(scrolloff);
        } else {
            self.next_non_wrapping(scrolloff);
        }
    }
    pub fn prev(&mut self, scrolloff: usize, wrap: bool) {
        if wrap {
            self.prev_wrapping(scrolloff);
        } else {
            self.prev_non_wrapping(scrolloff);
        }
    }
    fn prev_non_wrapping(&mut self, scrolloff: usize) {
        if let Some(item_count) = self.content_len {
            match self.get_selected() {
                Some(0) => {
                    self.select(Some(0), scrolloff);
                }
                Some(i) => {
                    self.select(Some(i.saturating_sub(1)), scrolloff);
                }
                None if item_count > 0 => {
                    self.select(Some(item_count.saturating_sub(1)), scrolloff);
                }
                None => self.select(None, scrolloff),
            }
        }
    }
    fn next_non_wrapping(&mut self, scrolloff: usize) {
        if let Some(item_count) = self.content_len {
            match self.get_selected() {
                Some(i) if i == item_count.saturating_sub(1) => {
                    self.select(Some(item_count.saturating_sub(1)), scrolloff);
                }
                Some(i) => {
                    self.select(Some(i + 1), scrolloff);
                }
                None if item_count > 0 => self.select(Some(0), scrolloff),
                None => self.select(None, scrolloff),
            }
        }
    }
    fn next_wrapping(&mut self, scrolloff: usize) {
        if let Some(item_count) = self.content_len {
            let i = match self.get_selected() {
                Some(i) => {
                    if i >= item_count.saturating_sub(1) { Some(0) } else { Some(i + 1) }
                }
                None if item_count > 0 => Some(0),
                None => None,
            };
            self.select(i, scrolloff);
        } else {
            self.select(None, scrolloff);
        }
    }
    fn prev_wrapping(&mut self, scrolloff: usize) {
        if let Some(item_count) = self.content_len {
            let i = match self.get_selected() {
                Some(i) => {
                    if i == 0 { Some(item_count.saturating_sub(1)) } else { Some(i - 1) }
                }
                None if item_count > 0 => Some(item_count.saturating_sub(1)),
                None => None,
            };
            self.select(i, scrolloff);
        } else {
            self.select(None, scrolloff);
        }
    }
    pub fn next_half_viewport(&mut self, scrolloff: usize) {
        if let Some(item_count) = self.content_len {
            if let Some(viewport) = self.viewport_len {
                self.select(
                    self
                        .get_selected()
                        .map(|i| {
                            i
                                .saturating_add(viewport / 2)
                                .min(item_count.saturating_sub(1))
                        }),
                    scrolloff,
                );
            } else {
                self.select(None, scrolloff);
            }
        } else {
            self.select(None, scrolloff);
        }
    }
    pub fn prev_half_viewport(&mut self, scrolloff: usize) {
        if self.content_len.is_some() {
            if let Some(viewport) = self.viewport_len {
                self.select(
                    self.get_selected().map(|i| i.saturating_sub(viewport / 2).max(0)),
                    scrolloff,
                );
            } else {
                self.select(None, scrolloff);
            }
        } else {
            self.select(None, scrolloff);
        }
    }
    pub fn next_viewport(&mut self, scrolloff: usize) {
        if let Some(item_count) = self.content_len {
            if let Some(viewport) = self.viewport_len {
                self.select(
                    self
                        .get_selected()
                        .map(|i| {
                            i.saturating_add(viewport).min(item_count.saturating_sub(1))
                        }),
                    scrolloff,
                );
            } else {
                self.select(None, scrolloff);
            }
        } else {
            self.select(None, scrolloff);
        }
    }
    pub fn prev_viewport(&mut self, scrolloff: usize) {
        if self.content_len.is_some() {
            if let Some(viewport) = self.viewport_len {
                self.select(
                    self.get_selected().map(|i| i.saturating_sub(viewport).max(0)),
                    scrolloff,
                );
            } else {
                self.select(None, scrolloff);
            }
        } else {
            self.select(None, scrolloff);
        }
    }
    /// Scroll the viewport by `dir * amount` rows without moving the
    /// selection (round-32 wheel behavior: the wheel scrolls the
    /// viewport, not the selection — the selection may leave the visible
    /// area). The offset clamps at the list's ends.
    pub fn scroll_viewport(&mut self, dir: i64, amount: usize) {
        let Some(viewport_len) = self.viewport_len else {
            return;
        };
        let Some(content_len) = self.content_len else {
            return;
        };
        let old_offset = self.offset();
        let max_offset = content_len.saturating_sub(viewport_len);
        let new_offset = if dir < 0 {
            old_offset.saturating_sub(amount)
        } else {
            old_offset.saturating_add(amount).min(max_offset)
        };
        if new_offset == old_offset {
            return;
        }
        self.inner.set_offset(new_offset);
        self.scrollbar_state = self.scrollbar_state.position(new_offset);
    }
    pub fn scroll_up(&mut self, amount: usize, scrolloff: usize) {
        let Some(_content_len) = self.content_len else {
            return;
        };
        let old_offset = self.offset();
        let new_offset = self.offset().saturating_sub(amount);
        if old_offset == new_offset && new_offset == 0 {
            return;
        }
        self.inner.set_offset(new_offset);
        self.scrollbar_state = self.scrollbar_state.position(new_offset);
        self.clamp_to_offset(scrolloff);
    }
    pub fn scroll_down(&mut self, amount: usize, scrolloff: usize) {
        let Some(viewport_len) = self.viewport_len else {
            return;
        };
        let Some(content_len) = self.content_len else {
            return;
        };
        let old_offset = self.offset();
        let max_offset = content_len.saturating_sub(viewport_len);
        let new_offset = (old_offset + amount).min(max_offset);
        if new_offset == old_offset {
            return;
        }
        self.inner.set_offset(new_offset);
        self.scrollbar_state = self.scrollbar_state.position(new_offset);
        self.clamp_to_offset(scrolloff);
    }
    pub fn clamp_to_offset(&mut self, scrolloff: usize) {
        let Some(viewport_len) = self.viewport_len else {
            return;
        };
        let offset = self.offset();
        let Some(selected) = self.get_selected() else {
            return;
        };
        if selected > (offset + viewport_len).saturating_sub(scrolloff + 1) {
            self.select(Some(offset + viewport_len - scrolloff - 1), scrolloff);
        } else if selected < offset + scrolloff {
            self.select(Some(offset + scrolloff), scrolloff);
        }
    }
    pub fn select(&mut self, idx: Option<usize>, scrolloff: usize) {
        let content_len = self.content_len.unwrap_or_default();
        let idx = idx.map(|idx| idx.max(0).min(content_len.saturating_sub(1)));
        self.inner.select_scrolling(idx);
        if self.viewport_len.unwrap_or_default() > 0 {
            self.apply_scrolloff(scrolloff);
        }
        self.scrollbar_state = self.scrollbar_state.position(self.offset());
    }
    fn apply_scrolloff(&mut self, scrolloff: usize) {
        let viewport_len = self.viewport_len.unwrap_or_default();
        let offset = self.inner.offset();
        let idx = self.get_selected().unwrap_or_default();
        let content_len = self.content_len.unwrap_or_default();
        let max_offset = content_len.saturating_sub(viewport_len);
        if scrolloff.saturating_mul(2) >= viewport_len {
            self.inner.set_offset(idx.saturating_sub(viewport_len / 2).min(max_offset));
            return;
        }
        let scrolloff_start_down = (offset.saturating_add(viewport_len))
            .saturating_sub(scrolloff.saturating_add(1));
        if idx > scrolloff_start_down {
            let new_offset = (offset
                .saturating_add(idx.saturating_sub(scrolloff_start_down)))
                .min(max_offset);
            self.inner.set_offset(new_offset);
            return;
        }
        if idx < offset.saturating_add(scrolloff) {
            self.inner
                .set_offset(
                    offset
                        .saturating_sub(
                            (offset.saturating_add(scrolloff)).saturating_sub(idx),
                        ),
                );
            return;
        }
    }
    #[allow(clippy::comparison_chain)]
    pub fn remove(&mut self, idx: usize) {
        match self.content_len {
            Some(len) if idx >= len => return,
            None => return,
            Some(ref mut len) => {
                self.marked = std::mem::take(&mut self.marked)
                    .into_iter()
                    .filter_map(|val| {
                        if val < idx {
                            Some(val)
                        } else if val > idx {
                            Some(val - 1)
                        } else {
                            None
                        }
                    })
                    .collect();
                len.sub_assign(1);
                let len: usize = *len;
                if self.get_selected().is_some_and(|selected| selected >= len) {
                    self.last();
                }
            }
        }
    }
    pub fn unmark_all(&mut self) {
        self.marked.clear();
    }
    pub fn mark(&mut self, idx: usize) -> bool {
        self.marked.insert(idx)
    }
    pub fn unmark(&mut self, idx: usize) -> bool {
        self.marked.remove(&idx)
    }
    pub fn toggle_mark(&mut self, idx: usize) -> bool {
        if self.marked.contains(&idx) {
            self.marked.remove(&idx)
        } else {
            self.marked.insert(idx)
        }
    }
    /// Mark every index between `from` and `to` (both inclusive), keeping the
    /// existing marks. Used for shift+click range selection.
    pub fn mark_range(&mut self, from: usize, to: usize) {
        let (lo, hi) = (from.min(to), from.max(to));
        for i in lo..=hi {
            self.marked.insert(i);
        }
    }
    /// Anchor for shift+click range selection; set on plain clicks.
    pub fn set_mark_anchor(&mut self, idx: usize) {
        self.mark_anchor = Some(idx);
    }
    pub fn mark_anchor(&self) -> Option<usize> {
        self.mark_anchor
    }
    /// Drop the anchor (the selection was cleared): the next shift/alt
    /// range-select starts fresh from the cursor.
    pub fn clear_mark_anchor(&mut self) {
        self.mark_anchor = None;
    }
    /// The range last marked by shift+arrow selection (if any).
    pub fn take_range_mark(&mut self) -> Option<(usize, usize)> {
        self.range_mark.take()
    }
    pub fn set_range_mark(&mut self, lo: usize, hi: usize) {
        self.range_mark = Some((lo, hi));
    }
    /// Forget the previous shift-range (called on plain clicks).
    pub fn clear_range_mark(&mut self) {
        self.range_mark = None;
    }
    pub fn invert_marked(&mut self) {
        let Some(content_len) = self.content_len else {
            log::warn!("Failed to invert marked items because content length is None");
            return;
        };
        let all = (0..content_len).collect::<BTreeSet<usize>>();
        self.marked = all.difference(&self.marked).copied().collect();
    }    pub fn get_selected(&self) -> Option<usize> {
        self.inner.get_selected_scrolling()
    }
    pub fn as_render_state_ref(&mut self) -> &mut T {
        &mut self.inner
    }
    pub fn as_scrollbar_state_ref(&mut self) -> &mut ScrollbarState {
        &mut self.scrollbar_state
    }
    pub fn get_at_rendered_row(&self, row: usize) -> Option<usize> {
        let offset = self.inner.offset();
        let idx_to_select = row + offset;
        if self.content_len().is_some_and(|len| idx_to_select < len) {
            Some(idx_to_select)
        } else {
            None
        }
    }
    pub fn offset(&self) -> usize {
        self.inner.offset()
    }
    pub fn set_offset(&mut self, offset: usize) {
        self.inner.set_offset(offset);
        self.scrollbar_state = self.scrollbar_state.position(offset);
    }
}

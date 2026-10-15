use anyhow::Result;
use ratatui::prelude::Rect;

use super::{
    Areas, QueuePane,
    play::play_queue_song,
};
use crate::{
    ctx::Ctx,
    mpd::mpd_client::MpdClient,
    shared::mouse_event::{MouseEvent, MouseEventKind},
    ui::song_list::SongListCore,
};

impl QueuePane {
    pub(super) fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position = event.into();
        if matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick)
            && self.toggle_areas.iter().any(|area| area.contains(position))
        {
            // Segment order (round 62 Q2): 0 Audio, 1 Video, 2 Chapters
            // (only when the track has markers) else Radio, 3 Radio.
            let chapters_visible = Self::chapters_available(ctx);
            let mode = self
                .toggle_areas
                .iter()
                .enumerate()
                .find(|(_, area)| area.contains(position))
                .and_then(|(idx, _)| match idx {
                    0 => Some(crate::ctx::QueueTabMode::Audio),
                    1 => Some(crate::ctx::QueueTabMode::Video),
                    2 if chapters_visible => Some(crate::ctx::QueueTabMode::Chapters),
                    2 => Some(crate::ctx::QueueTabMode::Radio),
                    3 => Some(crate::ctx::QueueTabMode::Radio),
                    _ => None,
                });
            if let Some(mode) = mode {
                self.queue.state.band.cancel();
                self.video_band.cancel();
                Self::set_tab(self, ctx, mode);
                ctx.render()?;
                return Ok(());
            }
        }
        if Self::chapters_available(ctx)
            && ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters
        {
            let table = self.areas[Areas::Table];
            if table.contains(position) {
                match event.kind {
                    MouseEventKind::LeftClick => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.chapters_state.offset() + row;
                        if idx < self.chapters_items_len {
                            self.chapters_state.select(Some(idx));
                            ctx.render()?;
                        }
                        return Ok(());
                    }
                    MouseEventKind::DoubleClick => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.chapters_state.offset() + row;
                        let chapters = Self::current_chapters(ctx);
                        if let Some(chapter) = chapters.get(idx) {
                            self.seek_to(chapter.start_secs, ctx);
                            self.chapters_state.select(None);
                            ctx.render()?;
                        }
                        return Ok(());
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                            -1
                        } else {
                            1
                        };
                        crate::ui::widgets::virtualized_list::scroll_viewport(
                            &mut self.chapters_state,
                            dir,
                            ctx.config.scroll_amount.max(1),
                            self.chapters_items_len,
                            self.areas[Areas::Table].height as usize,
                        );
                        ctx.render()?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Video {
            let table = self.areas[Areas::Table];
            if table.contains(position) {
                match event.kind {
                    MouseEventKind::DoubleClick
                        if !event
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        self.video_band.cancel();
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            self.video_load_entry(idx, ctx);
                            self.video_state.select(None);
                            ctx.render()?;
                            return Ok(());
                        }
                    }
                    MouseEventKind::LeftClick | MouseEventKind::DoubleClick if event
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            // Ctrl+click toggles the row: mark it if it
                            // was not marked, unmark it if it was.
                            let was_marked = self.video_marked.contains(idx);
                            if self.video_marked.is_empty() {
                                if let Some(sel) = self.video_state.selected() {
                                    self.video_marked.add(sel);
                                }
                            }
                            if was_marked {
                                self.video_marked.remove(idx);
                            } else {
                                self.video_marked.add(idx);
                            }
                            // Arm the band so a ctrl+drag from here adds a
                            // range (ctrl semantics keep existing marks).
                            self.video_band.arm(idx, false);
                            self.video_state.select(Some(idx));
                            ctx.render()?;
                            return Ok(());
                        }
                    }
                    MouseEventKind::LeftClick if event
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::ALT) => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            if self.video_marked.anchor().is_none() {
                                self.video_marked.set_anchor(idx);
                            }
                            self.video_marked.select_range(idx);
                            self.video_state.select(Some(idx));
                            ctx.render()?;
                            return Ok(());
                        }
                    }
                    MouseEventKind::LeftClick => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            // A plain press arms the band and defers the
                            // multi-selection drop (click ≠ drag); the
                            // release resolves it (Round 46).
                            let click_on_different_row = !self.video_marked.is_empty()
                                && Some(idx) != self.video_state.selected();
                            self.video_band.arm(idx, click_on_different_row);
                            self.video_state.select(Some(idx));
                            self.video_marked.set_anchor(idx);
                            self.video_marked.clear_range();
                            ctx.render()?;
                            return Ok(());
                        } else if let Some(edge) = crate::ui::band::band_current_row(
                            event.y,
                            table,
                            self.video_state.offset(),
                            self.video_items_len,
                            1,
                        ) {
                            // Round 47: a press in the empty pane space
                            // below the items arms the band at the clamped
                            // edge row, so a drag can select from empty
                            // space into the list, and a plain click there
                            // clears the multi-selection (the release's
                            // deferred plain-click path applies it). The
                            // selection cursor stays put.
                            let clear_marks = !self.video_marked.is_empty();
                            self.video_band.arm(edge, clear_marks);
                            ctx.render()?;
                            return Ok(());
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        self.video_band.cancel();
                        let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                            -1
                        } else {
                            1
                        };
                        self.video_scroll_viewport(dir, ctx)?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        if let Some(scrollbar_area) = self.scrollbar_area()
            && ctx.config.theme.scrollbar.is_some()
            && matches!(
                event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }
            )
        {
            let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
            let mode = ctx.queue_tab.get();
            let viewport = self.areas[Areas::Table].height as usize;
            let (content_len, viewport_len, position) = match mode {
                crate::ctx::QueueTabMode::Video => {
                    (
                        self
                            .video_items_len
                            .saturating_sub(viewport)
                            .saturating_add(1)
                            .max(1),
                        viewport,
                        self.video_state.offset(),
                    )
                }
                crate::ctx::QueueTabMode::Chapters => {
                    (
                        self
                            .chapters_items_len
                            .saturating_sub(viewport)
                            .saturating_add(1)
                            .max(1),
                        viewport,
                        self.chapters_state.offset(),
                    )
                }
                crate::ctx::QueueTabMode::Audio => {
                    let viewport = self
                        .queue
                        .state
                        .viewport_len()
                        .unwrap_or(scrollbar_area.height as usize);
                    (
                        self
                            .queue
                            .items
                            .len()
                            .saturating_sub(viewport)
                            .saturating_add(1)
                            .max(1),
                        viewport,
                        self.queue.state.inner.offset(),
                    )
                }
                // Round 62 (Q2): the queue pane is hidden in Radio mode, so
                // its scrollbar never gets events (kept for exhaustiveness).
                crate::ctx::QueueTabMode::Radio => {
                    (0, self.areas[Areas::Table].height as usize, 0)
                }
            };
            let drag = match mode {
                crate::ctx::QueueTabMode::Video => &mut self.video_scrollbar_drag,
                crate::ctx::QueueTabMode::Chapters => &mut self.chapters_scrollbar_drag,
                crate::ctx::QueueTabMode::Audio => &mut self.queue.state.scrollbar_drag,
                // Round 62 (Q2): the queue pane is hidden in Radio mode, so
                // its scrollbar never gets events; the arm is only for
                // exhaustiveness.
                crate::ctx::QueueTabMode::Radio => &mut self.queue.state.scrollbar_drag,
            };
            if let Some(perc) = drag
                .handle(
                    event,
                    scrollbar_area,
                    content_len,
                    viewport_len,
                    position,
                    begin_len,
                    end_len,
                )
            {
                match mode {
                    crate::ctx::QueueTabMode::Video => self.video_scroll_to(perc, ctx),
                    crate::ctx::QueueTabMode::Chapters => {
                        self.chapters_scroll_to(perc, ctx)
                    }
                    crate::ctx::QueueTabMode::Audio => {
                        // A scrollbar drag counts as reading the queue.
                        self.note_user_navigation();
                        self.queue.state.scroll_to(perc, ctx.config.scrolloff);
                    }
                    // Round 62 (Q2): unreachable (queue pane hidden in
                    // Radio mode); kept for exhaustiveness.
                    crate::ctx::QueueTabMode::Radio => {}
                }
                ctx.render()?;
                return Ok(());
            }
        }
        // Band capture (Round 46): once a band is armed inside the list,
        // drags and releases are accepted even when the pointer left the
        // list area (the row clamps to the visible list). Handled before
        // the area gate below.
        match event.kind {
            MouseEventKind::Drag { .. } if self.band_active_for_mode(ctx) => {
                return match ctx.queue_tab.get() {
                    crate::ctx::QueueTabMode::Video => self.video_band_drag(event, ctx),
                    _ => SongListCore::update_band_drag(self, event, ctx),
                };
            }
            MouseEventKind::LeftRelease if self.band_active_for_mode(ctx) => {
                return match ctx.queue_tab.get() {
                    crate::ctx::QueueTabMode::Video => self.video_band_release(ctx),
                    _ => SongListCore::finish_band_drag(self, ctx),
                };
            }
            _ => {}
        }
        if !self.areas[Areas::Table].contains(position) {
            return Ok(());
        }
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
                if self.areas[Areas::Table].contains(event.into())
                    && event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                let clicked_row: usize = event
                    .y
                    .saturating_sub(self.areas[Areas::Table].y)
                    .into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    // Ctrl+click toggles the row: mark it if it was not
                    // marked, unmark it if it was.
                    let was_marked = self.queue.state.marked.contains(&idx);
                    if self.queue.state.marked.is_empty() {
                        if let Some(sel) = self.queue.state.get_selected() {
                            self.queue.state.mark(sel);
                        }
                    }
                    if was_marked {
                        self.queue.state.unmark(idx);
                    } else {
                        self.queue.state.mark(idx);
                    }
                    // Arm the band so a ctrl+drag from here adds a range.
                    self.queue.state.band.arm(idx, false);
                    self.note_user_navigation();
                    self.queue.select_idx(idx, ctx.config.scrolloff);
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick if self.areas[Areas::Table].contains(event.into())
                && event.modifiers.contains(crossterm::event::KeyModifiers::ALT) => {
                let clicked_row: usize = event
                    .y
                    .saturating_sub(self.areas[Areas::Table].y)
                    .into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    self.queue.state.band.cancel();
                    if self.queue.state.mark_anchor().is_none() {
                        self.queue.state.set_mark_anchor(idx);
                    }
                    let anchor = self.queue.state.mark_anchor().unwrap_or(idx);
                    if let Some((lo, hi)) = self.queue.state.take_range_mark() {
                        for i in lo..=hi {
                            self.queue.state.marked.remove(&i);
                        }
                    }
                    let (lo, hi) = (anchor.min(idx), anchor.max(idx));
                    if lo < hi {
                        self.queue.state.mark_range(lo, hi);
                        self.queue.state.set_range_mark(lo, hi);
                    }
                    self.note_user_navigation();
                    self.queue.select_idx(idx, ctx.config.scrolloff);
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick if self
                .areas[Areas::Table]
                .contains(event.into()) => {
                let clicked_row: usize = event
                    .y
                    .saturating_sub(self.areas[Areas::Table].y)
                    .into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    // A plain press arms the band and defers the
                    // multi-selection drop (click ≠ drag); the release
                    // resolves it (Round 46).
                    let click_on_different_row = !self.queue.state.marked.is_empty()
                        && Some(idx) != self.queue.state.get_selected();
                    self.queue.state.band.arm(idx, click_on_different_row);
                    self.note_user_navigation();
                    self.queue.select_idx(idx, ctx.config.scrolloff);
                    self.queue.state.set_mark_anchor(idx);
                    self.queue.state.clear_range_mark();
                    ctx.render()?;
                } else if let Some(edge) = crate::ui::band::band_current_row(
                    event.y,
                    self.areas[Areas::Table],
                    self.queue.state.offset(),
                    self.queue.len(),
                    1,
                ) {
                    // Round 47: a press in the empty pane space below the
                    // items arms the band at the clamped edge row, so a
                    // drag can select from empty space into the list, and
                    // a plain click there clears the multi-selection (the
                    // release's deferred plain-click path applies it). The
                    // selection cursor stays put. (Host fix 2026-08-27:
                    // this arm was missing from the Round 47 commit.)
                    let clear_marks = !self.queue.state.marked.is_empty();
                    self.queue.state.band.arm(edge, clear_marks);
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick => {}
            MouseEventKind::DoubleClick if self
                .areas[Areas::Table]
                .contains(event.into()) => {
                self.queue.state.band.cancel();
                let clicked_row: usize = event
                    .y
                    .saturating_sub(self.areas[Areas::Table].y)
                    .into();
                if let Some(song) = self
                    .queue
                    .state
                    .get_at_rendered_row(clicked_row)
                    .and_then(|idx| self.queue.items.get(idx))
                {
                    play_queue_song(song, ctx);
                }
            }
            MouseEventKind::DoubleClick => {}
            MouseEventKind::MiddleClick if self
                .areas[Areas::Table]
                .contains(event.into()) => {
                self.queue.state.band.cancel();
                let clicked_row: usize = event
                    .y
                    .saturating_sub(self.areas[Areas::Table].y)
                    .into();
                if let Some(selected_song) = self
                    .queue
                    .state
                    .get_at_rendered_row(clicked_row)
                    .and_then(|idx| self.queue.items.get(idx))
                {
                    let id = selected_song.id;
                    ctx.command(move |client| {
                        client.delete_id(id)?;
                        Ok(())
                    });
                }
            }
            MouseEventKind::MiddleClick => {}
            MouseEventKind::ScrollDown
            | MouseEventKind::ScrollUp if self
                .areas[Areas::Table]
                .contains(event.into()) => {
                self.queue.state.band.cancel();
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                    -1
                } else {
                    1
                };
                let amount = ctx.config.scroll_amount.max(1);
                // The wheel counts as reading the queue: it stands the
                // follow-the-track jump down.
                self.note_user_navigation();
                self.queue.state.scroll_viewport(dir, amount);
                ctx.render()?;
                return Ok(());
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {}
            MouseEventKind::RightClick if self
                .areas[Areas::Table]
                .contains(event.into()) => {
                self.queue.state.band.cancel();
                self.video_band.cancel();
                let clicked_row: usize = event
                    .y
                    .saturating_sub(self.areas[Areas::Table].y)
                    .into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    self.note_user_navigation();
                    self.queue.select_idx(idx, ctx.config.scrolloff);
                    ctx.render()?;
                }
                self.open_context_menu(ctx, Some(position));
            }
            MouseEventKind::RightClick => {
                self.queue.state.band.cancel();
                self.video_band.cancel();
            }
            MouseEventKind::Drag { .. } => {}
            MouseEventKind::LeftRelease => {}
            MouseEventKind::Moved => {}
        }
        Ok(())
    }

    /// Round 63.1 (3): a release anywhere in the app ends every armed
    /// scrollbar grab — a release that ended over another pane never
    /// reached this pane's `handle_mouse_event`, so without this the grab
    /// would survive and hijack the next strip drag with a stale offset.
    pub(super) fn on_global_mouse_release(&mut self, _ctx: &Ctx) -> Result<()> {
        self.queue.state.scrollbar_drag.disarm();
        self.video_scrollbar_drag.disarm();
        self.chapters_scrollbar_drag.disarm();
        Ok(())
    }
}

impl QueuePane {
    fn scrollbar_area(&self) -> Option<Rect> {
        let area = self.areas[Areas::Scrollbar];
        if area.width > 0 { Some(area) } else { None }
    }

    /// Whether a rubber-band is armed/active in the list the current
    /// queue tab shows (Round 46).
    fn band_active_for_mode(&self, ctx: &Ctx) -> bool {
        match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Video => self.video_band.is_active(),
            _ => self.queue.state.band.is_active(),
        }
    }

    /// Rubber-band drag of the Video (mpv playlist) list. The marks live
    /// in `MarkState`; plain drag clears then range-selects from the band
    /// anchor, ctrl+drag keeps the other marks and replaces only the
    /// previous band range.
    fn video_band_drag(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if !self.video_band.is_active() {
            return Ok(());
        }
        let control = event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
        let table = self.areas[Areas::Table];
        let (offset, len) = (self.video_state.offset(), self.video_items_len);
        let Some(current) =
            crate::ui::band::band_current_row(event.y, table, offset, len, 1)
        else {
            return Ok(());
        };
        let anchor = self.video_band.anchor.unwrap_or(current);
        self.video_band.update(current);
        // The band anchor drives the range even after prior keyboard
        // selection set the mark anchor elsewhere.
        self.video_marked.set_anchor(anchor);
        if control {
            self.video_marked.select_range(current);
        } else {
            self.video_marked.clear();
            self.video_marked.set_anchor(anchor);
            self.video_marked.select_range(current);
        }
        ctx.render()?;
        Ok(())
    }

    /// Resolve the Video list's armed band on release.
    fn video_band_release(&mut self, ctx: &Ctx) -> Result<()> {
        let clear_marks = {
            match self.video_band.release() {
                crate::ui::band::BandEnd::Click { clear_marks } => clear_marks,
                _ => false,
            }
        };
        if clear_marks {
            self.video_marked.clear();
        }
        ctx.render()?;
        Ok(())
    }
}

#[cfg(test)]
mod moved_events_tests {
    use crate::shared::mouse_event::MouseEventKind;

    #[test]
    fn moved_events_are_tracked_by_the_mouse_event_tracker() {
        let mut tracker = crate::shared::mouse_event::MouseEventTracker::default();
        let ev = tracker.track_and_get(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 7,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        let ev = ev.expect("moves produce an event");
        assert!(matches!(ev.kind, MouseEventKind::Moved));
        assert_eq!((ev.x, ev.y), (7, 3));
    }
}

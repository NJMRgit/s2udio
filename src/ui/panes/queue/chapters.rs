// Chapters-mode handling of the Queue tab: the chapter list (render,
// navigation, click-seek) plus the seek helper. Split out of the queue
// module root (queue.rs) so the pane keeps its inherent-method surface
// while each focus area lives in its own file.
use anyhow::Result;
use ratatui::{
    Frame,
    layout::Flex,
    prelude::{Constraint, Layout},
    text::Line,
    widgets::{ListItem, StatefulWidget},
};
use unicode_width::UnicodeWidthStr;

use super::{truncate_to_width, Areas, QueuePane, CHAPTER_DURATION_COL, CHAPTER_TIME_COL};
use crate::{
    config::keys::{CommonAction, DirectoriesActions, GlobalAction, QueueActions},
    ctx::Ctx,
    mpd::{
        commands::State,
        mpd_client::{MpdClient, ValueChange},
    },
    shared::keys::ActionEvent,
};

impl QueuePane {
    /// The chapter list (Chapter | start | duration), replacing the song
    /// table in Chapters mode. The values are laid out in the same columns as
    /// the QueueHeaderPane's `Chapter | Time | Duration` labels, so they line
    /// up underneath them. A click highlights a chapter; clicking the
    /// highlighted chapter again seeks to it (MPD or mpv).
    pub(super) fn render_chapters(&mut self, frame: &mut Frame, ctx: &Ctx) -> Result<()> {
        let chapters = Self::current_chapters(ctx);
        let fmt = &ctx.config.duration_format;
        let position = if crate::core::mpv::mpv_is_ui_source(ctx) {
            ctx.mpv.position
        } else {
            ctx.status.elapsed.as_secs_f64()
        };
        let current_idx = chapters
            .iter()
            .rposition(|c| position >= c.start_secs)
            .unwrap_or(0);
        self.chapters_items_len = chapters.len();

        // The chapters table uses its own columns (matching the chapters
        // header): Chapter (flexible) | Time (centered) | Duration
        // (right-aligned at the right edge, like the queue's Duration column).
        let area = self.areas[Areas::Table];
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            area,
            self.chapters_state.offset(),
            chapters.len(),
            1,
        );
        let widths = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(CHAPTER_TIME_COL),
            Constraint::Length(CHAPTER_DURATION_COL),
        ])
        .flex(Flex::Start)
        .spacing(1)
        .split(area);
        // The marker prefix (❯ / two spaces) lives inside the chapter column.
        let title_field = (widths[0].width as usize).saturating_sub(2);
        let time_w = widths[1].width as usize;
        let duration_w = widths[2].width as usize;

        let items: Vec<ListItem> = chapters
            .iter()
            .enumerate()
            .map(|(idx, chapter)| {
                let is_current = idx == current_idx;
                let style = if hover_idx == Some(idx) {
                    ctx.config.theme.hovered_item_style
                } else if is_current {
                    ctx.config.theme.current_item_style
                } else {
                    ctx.config.as_list_text_style()
                };
                let start = fmt.format(chapter.start_secs as u64);
                let duration = fmt.format(chapter.duration() as u64);
                let prefix = if is_current { "❯ " } else { "  " };
                let mut title = chapter.title.clone();
                // Width-safe truncation: keep graphemes until the title
                // column is full.
                truncate_to_width(&mut title, title_field);
                // Pad by display width (not char count), so wide glyphs
                // (CJK etc.) can never push the time/duration columns right.
                let title_pad = title_field.saturating_sub(title.width());
                // Time is centered in its column; the duration is
                // right-aligned at the table's right edge.
                let pad_left = time_w.saturating_sub(start.width()) / 2;
                let pad_right = time_w.saturating_sub(start.width() + pad_left);
                let dur_pad = duration_w.saturating_sub(duration.width());
                ListItem::new(Line::styled(
                    format!(
                        "{prefix}{title}{} {}{start}{} {}{duration}",
                        " ".repeat(title_pad),
                        " ".repeat(pad_left),
                        " ".repeat(pad_right),
                        " ".repeat(dur_pad),
                    ),
                    style,
                ))
            })
            .collect();

        // The click-selected chapter (first click highlights it, the second
        // seeks) gets the accent highlight. Rendered through the
        // virtualized list so the wheel can scroll the viewport without
        // dragging the highlight (round 32).
        let list = crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
            .highlight_style(if hover_idx == self.chapters_state.selected() {
                ctx.config.theme.hovered_item_style
            } else {
                ctx.config.theme.highlighted_item_style
            });
        StatefulWidget::render(list, area, frame.buffer_mut(), &mut self.chapters_state);

        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && self.areas[Areas::Scrollbar].width > 0
        {
            let max = self.chapters_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
            let position = self.chapters_state.offset().min(max);
            // content_length = max + 1 so the bottom position is reachable
            // (ratatui clamps positions to content_length - 1); the viewport
            // length keeps the thumb proportional to the visible rows.
            StatefulWidget::render(
                scrollbar,
                self.areas[Areas::Scrollbar],
                frame.buffer_mut(),
                &mut ratatui::widgets::ScrollbarState::new(max + 1)
                    .position(position)
                    .viewport_content_length(self.areas[Areas::Table].height as usize),
            );
        }
        Ok(())
    }

    /// Seek to a chapter start (MPD or the mpv session): the source whose
    /// chapters the list is showing.
    pub(super) fn seek_to(&self, seconds: f64, ctx: &Ctx) {
        if crate::core::mpv::mpv_is_ui_source(ctx)
            && let Some(socket) = ctx.mpv.socket.clone()
        {
            crate::core::mpv::mpv_seek(&socket, seconds);
            return;
        }
        ctx.command(move |client| {
            use crate::mpd::mpd_client::ValueChange;
            let _ = client.seek_current(ValueChange::Set(seconds.max(0.0) as u32));
            Ok(())
        });
    }

    /// Keyboard handling for Chapters mode: navigate the chapter list
    /// (w/s/↑/↓, PageUp/PageDown, Home/End) and play a chapter with
    /// d/→/Enter. `c` still toggles back to the queue.
    pub(super) fn handle_chapters_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp => self.chapters_move(-1, ctx),
                DirectoriesActions::FolderDown => self.chapters_move(1, ctx),
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    self.chapters_play_selected(ctx)
                }
                DirectoriesActions::FolderCollapse => Ok(()),
            };
        }
        if let Some(action) = event.claim_queue() {
            match action {
                QueueActions::ToggleChapters => {
                    // Cycle back to the Audio view.
                    self.cycle_tab(ctx);
                    ctx.render()?;
                }
                QueueActions::JumpToCurrent => {
                    let chapters = Self::current_chapters(ctx);
                    let position = if crate::core::mpv::mpv_is_ui_source(ctx) {
                        ctx.mpv.position
                    } else {
                        ctx.status.elapsed.as_secs_f64()
                    };
                    let idx = chapters
                        .iter()
                        .rposition(|c| position >= c.start_secs)
                        .unwrap_or(0);
                    self.chapters_jump(idx, ctx)?;
                }
                _ => {}
            }
            return Ok(());
        }
        if let Some(action) = event.claim_common().map(|v| v.to_owned()) {
            match action {
                CommonAction::Up => self.chapters_move(-1, ctx)?,
                CommonAction::Down => self.chapters_move(1, ctx)?,
                CommonAction::PageUp => self.chapters_page(-1, ctx)?,
                CommonAction::PageDown => self.chapters_page(1, ctx)?,
                CommonAction::Top => self.chapters_jump(0, ctx)?,
                CommonAction::Bottom => self.chapters_jump(usize::MAX, ctx)?,
                CommonAction::Confirm => self.chapters_play_selected(ctx)?,
                CommonAction::Select => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.pause_toggle()?;
                            Ok(())
                        });
                    } else {
                        ctx.command(move |client| {
                            client.play()?;
                            Ok(())
                        });
                    }
                }
                _ => event.abandon(),
            }
            return Ok(());
        }
        if let Some(action) = event.claim_global() {
            match action {
                // < / > seek the playing track (like the queue view).
                GlobalAction::PreviousTrack => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.seek_current(ValueChange::Decrease(5))?;
                            Ok(())
                        });
                    }
                }
                GlobalAction::NextTrack => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.seek_current(ValueChange::Increase(5))?;
                            Ok(())
                        });
                    }
                }
                _ => event.abandon(),
            }
            return Ok(());
        }
        Ok(())
    }

    /// Scroll the chapters list to a scrollbar fraction (0.0..=1.0).
    pub(super) fn chapters_scroll_to(&mut self, perc: f64, ctx: &Ctx) {
        let max =
            self.chapters_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
        let new = ((perc.clamp(0.0, 1.0)) * max as f64).floor() as usize;
        let _ = self.chapters_jump(new.min(max), ctx);
    }

    /// Move the chapters highlight by `dir` rows (clamped). The first move
    /// from no selection highlights the first chapter (menu convention).
    /// The list renders from its offset, so the move also scrolls the
    /// selection back into view (the wheel only scrolls the viewport).
    pub(super) fn chapters_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.chapters_items_len;
        if len == 0 {
            return Ok(());
        }
        let Some(current) = self.chapters_state.selected() else {
            self.chapters_state.select(Some(0));
            crate::ui::widgets::virtualized_list::scroll_selection_into_view(
                &mut self.chapters_state,
                len,
                self.areas[Areas::Table].height as usize,
                ctx.config.scrolloff,
            );
            ctx.render()?;
            return Ok(());
        };
        let new = ((current as i64) + dir).clamp(0, len as i64 - 1) as usize;
        if new != current {
            self.chapters_state.select(Some(new));
            crate::ui::widgets::virtualized_list::scroll_selection_into_view(
                &mut self.chapters_state,
                len,
                self.areas[Areas::Table].height as usize,
                ctx.config.scrolloff,
            );
            ctx.render()?;
        }
        Ok(())
    }

    /// Page the chapters list by one viewport in `dir` direction.
    pub(super) fn chapters_page(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let viewport = self.areas[Areas::Table].height.max(1) as i64;
        self.chapters_move(dir * viewport, ctx)
    }

    /// Highlight the chapter at `idx` (clamped to the list) and scroll it
    /// into view (keyboard Home/End; the wheel only scrolls the viewport).
    pub(super) fn chapters_jump(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let len = self.chapters_items_len;
        if len == 0 {
            return Ok(());
        }
        let idx = idx.min(len - 1);
        if self.chapters_state.selected() != Some(idx) {
            self.chapters_state.select(Some(idx));
            crate::ui::widgets::virtualized_list::scroll_selection_into_view(
                &mut self.chapters_state,
                len,
                self.areas[Areas::Table].height as usize,
                ctx.config.scrolloff,
            );
            ctx.render()?;
        }
        Ok(())
    }

    /// Select the chapter currently playing, used when the chapters view
    /// opens (startup, tab re-entry, toggling) so the highlight lands on
    /// the track's current position.
    pub(super) fn chapters_select_current(&mut self, ctx: &Ctx) {
        let chapters = Self::current_chapters(ctx);
        if chapters.is_empty() {
            return;
        }
        let position = if crate::core::mpv::mpv_is_ui_source(ctx) {
            ctx.mpv.position
        } else {
            ctx.status.elapsed.as_secs_f64()
        };
        let idx = chapters
            .iter()
            .rposition(|c| position >= c.start_secs)
            .unwrap_or(0);
        self.chapters_state.select(Some(idx));
        crate::ui::widgets::virtualized_list::scroll_selection_into_view(
            &mut self.chapters_state,
            self.chapters_items_len,
            self.areas[Areas::Table].height as usize,
            ctx.config.scrolloff,
        );
    }

    /// Seek to the highlighted chapter (MPD or mpv). The highlight stays
    /// put so keyboard navigation continues from the played chapter (the
    /// mouse's click-highlight-then-click-again behavior lives in
    /// `handle_mouse_event`).
    pub(super) fn chapters_play_selected(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(idx) = self.chapters_state.selected() else { return Ok(()) };
        let chapters = Self::current_chapters(ctx);
        if let Some(chapter) = chapters.get(idx) {
            self.seek_to(chapter.start_secs, ctx);
        }
        Ok(())
    }
}

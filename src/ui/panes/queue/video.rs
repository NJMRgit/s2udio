// Video-mode handling of the Queue tab: the mpv playlist list (render,
// navigation, marks, load/remove, scrollbar) plus the "follow the playing
// video" mode switch. Split out of the queue module root (queue.rs) so the
// pane keeps its inherent-method surface while each focus area lives in its
// own file.
use anyhow::Result;
use ratatui::{
    Frame,
    layout::Flex,
    prelude::{Constraint, Layout, Rect},
    style::Modifier,
    text::Line,
    widgets::{List, ListItem, StatefulWidget},
};

use unicode_width::UnicodeWidthStr;

use super::{truncate_to_width, Areas, QueuePane, CHAPTER_DURATION_COL};
use crate::{
    config::keys::{CommonAction, DirectoriesActions, QueueActions},
    ctx::Ctx,
    shared::keys::ActionEvent,
};

impl QueuePane {
    /// The list the Queue tab should show while a video plays in mpv: its
    /// Chapters list when the video has markers (and the auto-chapters
    /// setting allows it), else the mpv playlist (Video list). Called when
    /// a video session starts (launch, reattach) and when the video's
    /// chapters arrive, so the tab never keeps showing the stale audio list
    /// after a video was added. A no-op while the video is not the active
    /// UI source (nothing plays in mpv, or MPD playback has taken over and
    /// paused it — the Queue list then belongs to the music).
    pub(crate) fn follow_playing_video(&mut self, ctx: &Ctx) {
        if !crate::core::mpv::mpv_is_ui_source(ctx) {
            return;
        }
        let mode = if ctx.config.ui.auto_show_chapters && Self::chapters_available(ctx) {
            crate::ctx::QueueTabMode::Chapters
        } else {
            crate::ctx::QueueTabMode::Video
        };
        Self::set_tab(self, ctx, mode);
    }

    /// The list shown in the Video view: the Jellyfin session's own
    /// playlist (the season episodes actually playing) while a Jellyfin
    /// item plays, else the persistent video playlist (which is left
    /// untouched during Jellyfin playback and returns when it stops).
    pub(super) fn render_video(&mut self, frame: &mut Frame, ctx: &Ctx) -> Result<()> {
        let area = self.areas[Areas::Table];
        let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
        let playlist: std::cell::Ref<'_, Vec<crate::core::mpv::MpvPlaylistEntry>> = if jellyfin {
            ctx.mpv.playlist.borrow()
        } else {
            ctx.video_playlist.borrow()
        };
        self.video_items_len = playlist.len();
        // The playlist can change under the marks (session switches,
        // removals elsewhere); drop any mark that no longer has a row.
        self.video_marked.clamp(playlist.len());
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            area,
            self.video_state.offset(),
            playlist.len(),
            1,
        );
        if let Some(sel) = self.video_state.selected() {
            if playlist.is_empty() {
                self.video_state.select(None);
            } else if sel >= playlist.len() {
                self.video_state.select(Some(playlist.len() - 1));
            }
        }

        if playlist.is_empty() {
            let style = ctx.config.as_list_text_style().add_modifier(Modifier::DIM);
            frame.render_widget(
                ratatui::widgets::Paragraph::new("No video playing").style(style),
                Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: 1 },
            );
            return Ok(());
        }

        let fmt = &ctx.config.duration_format;
        let current_idx = if jellyfin {
            ctx.mpv.playlist_pos.get().filter(|i| *i < playlist.len())
        } else {
            crate::core::mpv::video_playlist_current_idx(ctx).filter(|i| *i < playlist.len())
        };
        // Title (flexible) | Duration (right-aligned at the right edge,
        // like the queue's Duration column).
        let widths = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(CHAPTER_DURATION_COL),
        ])
        .flex(Flex::Start)
        .spacing(1)
        .split(area);
        let title_field = widths[0].width as usize;
        let duration_w = widths[1].width as usize;

        let items: Vec<ListItem> = playlist
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let is_current = current_idx == Some(idx);
                // Marked rows render with the lighter marked highlight
                // (like the audio queue list); the row under the mouse
                // gets the hover highlight.
                let style = if self.video_marked.contains(idx) {
                    ctx.config.theme.marked_item_style
                } else if hover_idx == Some(idx) {
                    ctx.config.theme.hovered_item_style
                } else if is_current {
                    ctx.config.theme.current_item_style
                } else {
                    ctx.config.as_list_text_style()
                };
                let duration = entry
                    .duration
                    .map(|d| fmt.format(d as u64))
                    .unwrap_or_else(|| "-".to_owned());
                let prefix = if is_current { "❯ " } else { "  " };
                let mut title = entry.title.clone();
                truncate_to_width(&mut title, title_field.saturating_sub(2));
                let title_pad = title_field.saturating_sub(2 + title.width());
                let dur_pad = duration_w.saturating_sub(duration.width());
                ListItem::new(Line::styled(
                    format!(
                        "{prefix}{title}{} {}{duration}",
                        " ".repeat(title_pad),
                        " ".repeat(dur_pad),
                    ),
                    style,
                ))
            })
            .collect();

        let list = List::new(items)
            .highlight_style(if hover_idx == self.video_state.selected() {
                ctx.config.theme.hovered_item_style
            } else {
                ctx.config.theme.highlighted_item_style
            });
        StatefulWidget::render(list, area, frame.buffer_mut(), &mut self.video_state);

        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && self.areas[Areas::Scrollbar].width > 0
        {
            let max = self.video_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
            let position = self.video_state.offset().min(max);
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

    /// Play the visible Video list from `idx` onwards: the entries are
    /// handed to mpv (a fresh instance when none runs, otherwise the
    /// running one is switched to them); neither the Jellyfin session
    /// playlist nor the persistent playlist is mutated.
    pub(super) fn video_load_entry(&self, idx: usize, ctx: &Ctx) {
        let entries: Vec<crate::core::mpv::MpvPlaylistEntry> =
            if crate::core::mpv::session_playlist_shown(ctx) {
                ctx.mpv.playlist.borrow().iter().skip(idx).cloned().collect()
            } else {
                ctx.video_playlist.borrow().iter().skip(idx).cloned().collect()
            };
        if !entries.is_empty() {
            crate::core::mpv::play_video_entries(ctx, entries);
        }
    }

    /// Remove the entries at `indices` from the persistent video playlist
    /// and save it. The selection shifts up past the removed rows and the
    /// marks are dropped (their indices no longer exist).
    pub(super) fn video_remove_entries(&mut self, indices: Vec<usize>, ctx: &Ctx) {
        if indices.is_empty() {
            return;
        }
        {
            let mut playlist = ctx.video_playlist.borrow_mut();
            for idx in indices.iter().rev() {
                if *idx < playlist.len() {
                    playlist.remove(*idx);
                }
            }
        }
        crate::ui::modals::paste::save_video_playlist(ctx);
        let len = ctx.video_playlist.borrow().len();
        self.video_items_len = len;
        self.video_marked.clear();
        self.video_marked.clear_anchor();
        if let Some(sel) = self.video_state.selected() {
            let removed_below = indices.iter().filter(|&&i| i < sel).count();
            let new_sel = sel.saturating_sub(removed_below);
            if len == 0 {
                self.video_state.select(None);
            } else {
                self.video_state.select(Some(new_sel.min(len - 1)));
            }
        }
    }

    /// Keyboard handling for Video mode: navigate the mpv playlist with
    /// w/s/↑/↓, load an entry with d/→/Enter. `c` cycles back to Audio.
    pub(super) fn handle_video_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp => self.video_move(-1, ctx),
                DirectoriesActions::FolderDown => self.video_move(1, ctx),
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    self.video_play_selected(ctx)
                }
                DirectoriesActions::FolderCollapse => Ok(()),
            };
        }
        if let Some(action) = event.claim_queue() {
            match action {
                QueueActions::ToggleChapters => {
                    self.cycle_tab(ctx);
                    ctx.render()?;
                }
                QueueActions::JumpToCurrent => {
                    let idx = if crate::core::mpv::session_playlist_shown(ctx) {
                        ctx.mpv.playlist_pos.get()
                    } else {
                        crate::core::mpv::video_playlist_current_idx(ctx)
                    };
                    if let Some(idx) = idx.filter(|i| *i < self.video_items_len) {
                        self.video_jump(idx, ctx)?;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if let Some(action) = event.claim_common().map(|v| v.to_owned()) {
            match action {
                CommonAction::Up => self.video_move(-1, ctx)?,
                CommonAction::Down => self.video_move(1, ctx)?,
                CommonAction::PageUp => self.video_page(-1, ctx)?,
                CommonAction::PageDown => self.video_page(1, ctx)?,
                CommonAction::Top => self.video_jump(0, ctx)?,
                CommonAction::Bottom => self.video_jump(usize::MAX, ctx)?,
                // Enter opens the context menu (like right-click);
                // `d`/`→` still load the highlighted entry.
                CommonAction::Confirm => self.open_context_menu(ctx),
                CommonAction::SelectUp | CommonAction::SelectDown => {
                    // Shift+Up/Down: range-select from the anchor (set by
                    // plain clicks / the first shift-press), moving first
                    // so the newly reached row is included; each press
                    // replaces the previous range.
                    let dir = if matches!(action, CommonAction::SelectDown) { 1 } else { -1 };
                    let start = self.video_state.selected().unwrap_or(0);
                    if self.video_marked.anchor().is_none() || self.video_marked.is_empty() {
                        self.video_marked.set_anchor(start);
                    }
                    self.video_move(dir, ctx)?;
                    let sel = self.video_state.selected().unwrap_or(start);
                    self.video_marked.select_range(sel);
                    ctx.render()?;
                }
                CommonAction::Delete => {
                    // Remove the marked entries (or the highlighted one)
                    // from the persistent video playlist (a live session
                    // keeps playing them; the queue no longer contains
                    // them). The Jellyfin session's own playlist is live
                    // mpv state — never deletable.
                    if !crate::core::mpv::session_playlist_shown(ctx) {
                        let indices: Vec<usize> = if self.video_marked.is_empty() {
                            self.video_state.selected().into_iter().collect()
                        } else {
                            self.video_marked.iter().collect()
                        };
                        if !indices.is_empty() {
                            self.video_remove_entries(indices, ctx);
                            ctx.render()?;
                        }
                    }
                }
                CommonAction::Select => {
                    // Toggle the video's pause.
                    if let Some(socket) = ctx.mpv.socket.clone() {
                        crate::core::mpv::mpv_toggle_pause(&socket);
                    }
                }
                CommonAction::SelectAll => {
                    // Ctrl+A marks the whole video list (ctrl+a in the
                    // Queue tab applies to the active Audio/Video list).
                    self.video_marked.mark_all(self.video_items_len);
                    ctx.render()?;
                }
                CommonAction::Close if !self.video_marked.is_empty() => {
                    self.video_marked.clear();
                    self.video_marked.clear_anchor();
                    // Esc is bound to both Close and ShowSettings: clearing a
                    // selection consumes the keypress, so the settings panel
                    // only opens on a second Esc (when nothing is selected).
                    event.consume();
                    ctx.render()?;
                }
                _ => event.abandon(),
            }
            return Ok(());
        }
        event.abandon();
        Ok(())
    }

    /// Move the video list highlight by `dir` rows (clamped). The first
    /// move from no selection highlights the first entry (menu convention).
    pub(super) fn video_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.video_items_len;
        if len == 0 {
            return Ok(());
        }
        let Some(current) = self.video_state.selected() else {
            self.video_state.select(Some(0));
            ctx.render()?;
            return Ok(());
        };
        let new = ((current as i64) + dir).clamp(0, len as i64 - 1) as usize;
        if new != current {
            self.video_state.select(Some(new));
            ctx.render()?;
        }
        Ok(())
    }

    /// Scroll the video list to a scrollbar fraction (0.0..=1.0): the
    /// offset lands so the thumb matches the pointer. `max` mirrors the
    /// renderer's `items_len - table_height`.
    pub(super) fn video_scroll_to(&mut self, perc: f64, ctx: &Ctx) {
        let max = self.video_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
        let new = ((perc.clamp(0.0, 1.0)) * max as f64).floor() as usize;
        let _ = self.video_jump(new.min(max), ctx);
    }

    /// Page the video list by one viewport in `dir` direction.
    pub(super) fn video_page(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let viewport = self.areas[Areas::Table].height.max(1) as i64;
        self.video_move(dir * viewport, ctx)
    }

    /// Highlight the playlist entry at `idx` (clamped to the list).
    pub(super) fn video_jump(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let len = self.video_items_len;
        if len == 0 {
            return Ok(());
        }
        let idx = idx.min(len - 1);
        if self.video_state.selected() != Some(idx) {
            self.video_state.select(Some(idx));
            ctx.render()?;
        }
        Ok(())
    }

    /// Load the highlighted playlist entry in mpv (the current view's
    /// equivalent of playing a song).
    pub(super) fn video_play_selected(&mut self, ctx: &Ctx) -> Result<()> {
        if let Some(idx) = self.video_state.selected() {
            self.video_load_entry(idx, ctx);
        }
        Ok(())
    }
}

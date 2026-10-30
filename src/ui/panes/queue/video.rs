// Video-mode handling of the Queue tab: the mpv playlist list (render,
// navigation, marks, load/remove, scrollbar) plus the "follow the playing
// video" mode switch. Split out of the queue module root (queue/mod.rs) so the
// pane keeps its inherent-method surface while each focus area lives in its
// own file.
use anyhow::Result;
use ratatui::{
    Frame,
    layout::Flex,
    prelude::{Constraint, Layout, Rect},
    style::Modifier,
    text::Line,
    widgets::{ListItem, StatefulWidget},
};

use unicode_width::UnicodeWidthStr;

use super::{Areas, CHAPTER_DURATION_COL, QueuePane, truncate_to_width};
use crate::core::mpv::MpvPlaylistEntry;
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
        // Round 90.3: one line per session start / chapters arrival — the
        // decision that a chaptered video must not lose to a late resolve.
        log::debug!(chapters = Self::chapters_available(ctx), mode:? = mode; "The Queue list follows the playing video");
        Self::set_tab(self, ctx, mode);
    }

    /// The list shown in the Video view: the Jellyfin session's own
    /// playlist (the season episodes actually playing) while a Jellyfin
    /// item plays, else the persistent video playlist (which is left
    /// untouched during Jellyfin playback and returns when it stops).
    pub(super) fn render_video(&mut self, frame: &mut Frame, ctx: &Ctx) -> Result<()> {
        let area = self.areas[Areas::Table];
        let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
        let playlist: std::cell::Ref<'_, Vec<crate::core::mpv::MpvPlaylistEntry>> =
            if jellyfin { ctx.mpv.playlist.borrow() } else { ctx.video_playlist.borrow() };
        // 2026-10-24: a pasted playlist import into the Video list paints one
        // extra line below the last entry (`Loading playlist…`) until its rows
        // land. The first frame of the import scrolls it into view, because
        // the import switched the user here to watch it.
        let import_loading = ctx.playlist_import.borrow().is_some_and(|import| import.video);
        if import_loading {
            if !self.video_import_loading_shown {
                self.video_import_loading_shown = true;
                let viewport = area.height as usize;
                let max_offset = (playlist.len() + 1).saturating_sub(viewport);
                *self.video_state.offset_mut() = max_offset;
            }
            self.request_spinner_frames(ctx);
        } else {
            self.video_import_loading_shown = false;
            // Nothing in the Video list spins once the import's line is gone
            // (the audio list's Duration spinners are not drawn in this mode),
            // so this frame asks the clock for nothing and the tick stops
            // instead of repainting at the spinner rate forever.
        }
        self.video_items_len = playlist.len() + usize::from(import_loading);
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

        // 2026-10-20: publish the highlight, so a merely-selected video gets
        // what a merely-selected audio row already gets — its metadata is
        // resolved (debounced `WarmQueueSelection`), and the info box and the
        // album-art box follow it. Driven from the render pass for the same
        // reason the audio selection is (`render.rs`): every selection route
        // (keys, wheel, mouse, the playing-video follow) funnels through here.
        let selected_key = self
            .video_state
            .selected()
            .and_then(|idx| playlist.get(idx))
            .map(|entry| entry.lookup_url().to_owned());
        if ctx.video_selected.borrow().as_deref() != selected_key.as_deref() {
            let resolvable = selected_key.is_some();
            *ctx.video_selected.borrow_mut() = selected_key;
            if resolvable {
                super::schedule_selection_warm(ctx);
            }
        }

        if playlist.is_empty() {
            if import_loading {
                self.render_video_import_loading_row(frame, ctx, 0);
                return Ok(());
            }
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
        let widths =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(CHAPTER_DURATION_COL)])
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
                // Round 111 parity with the audio queue's Duration column: an
                // entry with no duration of its own (a web stream nothing has
                // probed) falls back to the shared stream metadata, so the row
                // shows the real length instead of `-` and the column agrees
                // with the footer total computed from the same lookup.
                let duration = crate::ui::modals::paste::video_entry_duration(ctx, entry)
                    .map(|d| fmt.format(d as u64))
                    .unwrap_or_else(|| "-".to_owned());
                let prefix = video_row_prefix(entry, is_current);
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

        let list = crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
            .highlight_style(if hover_idx == self.video_state.selected() {
                ctx.config.theme.hovered_item_style
            } else {
                ctx.config.theme.highlighted_item_style
            });
        StatefulWidget::render(list, area, frame.buffer_mut(), &mut self.video_state);
        self.render_video_band_marquee(frame, ctx, area);

        if import_loading {
            self.render_video_import_loading_row(frame, ctx, playlist.len());
        }

        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && self.areas[Areas::Scrollbar].width > 0
        {
            let max = self.video_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
            let position = self.video_state.offset().min(max);
            // content_length = max + 1 so the bottom position is reachable
            // (ratatui clamps positions to content_length - 1); the viewport
            // length keeps the thumb proportional to the visible rows.
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                self.areas[Areas::Scrollbar],
                &mut ratatui::widgets::ScrollbarState::new(max + 1)
                    .position(position)
                    .viewport_content_length(self.areas[Areas::Table].height as usize),
            );
        }
        Ok(())
    }

    /// The Video list's `Loading playlist…` line (2026-10-24), painted at
    /// `index` — one past the last entry for an append — and clipped away when
    /// the viewport does not reach it. A painting only, like the audio list's
    /// (see `render_import_loading_row`): the entry list itself is untouched.
    fn render_video_import_loading_row(
        &self,
        frame: &mut ratatui::Frame,
        ctx: &Ctx,
        index: usize,
    ) {
        let area = self.areas[Areas::Table];
        let row = index.saturating_sub(self.video_state.offset());
        if row >= area.height as usize {
            return;
        }
        let line = ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(
                format!("{} ", super::render::spinner_frame()),
                ratatui::style::Style::default(),
            ),
            ratatui::text::Span::styled(
                "Loading playlist…",
                ctx.config.as_list_text_style(),
            ),
        ]);
        frame.render_widget(
            line,
            ratatui::prelude::Rect {
                x: area.x,
                y: area.y.saturating_add(row as u16),
                width: area.width,
                height: 1,
            },
        );
    }

    /// Opt-in (`[theme] band_rect`): the band around the video rows a drag is
    /// selecting — top and bottom rules only, the same shape the audio list
    /// draws (see `QueuePane::render_band_marquee`), from its own band state.
    fn render_video_band_marquee(&self, frame: &mut Frame, ctx: &Ctx, area: Rect) {
        if !ctx.config.theme.band_rect {
            return;
        }
        let band = self.video_band;
        if !band.is_active() {
            return;
        }
        let (Some(anchor), Some(current)) = (band.anchor, band.current) else {
            return;
        };
        let Some(rect) =
            crate::ui::band::marquee_rect(area, self.video_state.offset(), anchor, current, 1)
        else {
            return;
        };
        frame.render_widget(
            ratatui::widgets::Block::default()
                // Top and bottom only: a side border would cover a cell of
                // every row in the band (the lists have no gutter).
                .borders(ratatui::widgets::Borders::TOP | ratatui::widgets::Borders::BOTTOM)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(ctx.config.theme.highlight_border_style),
            rect,
        );
    }

    /// Play the selected row of the Video list: the WHOLE list is handed to
    /// mpv (nothing is trimmed) and playback starts at `idx` — the season
    /// keeps every episode and only the position moves (Round 69, user
    /// feedback: selecting an episode must not remove the ones before it).
    /// A fresh mpv is launched when none runs, otherwise the running one is
    /// kept and switched in place (same list = playlist-pos jump, different
    /// list = titled .m3u `loadlist replace` + position).
    pub(super) fn video_load_entry(&self, idx: usize, ctx: &Ctx) {
        let entries: Vec<crate::core::mpv::MpvPlaylistEntry> =
            if crate::core::mpv::session_playlist_shown(ctx) {
                ctx.mpv.playlist.borrow().clone()
            } else {
                ctx.video_playlist.borrow().clone()
            };
        if !entries.is_empty() {
            let idx = idx.min(entries.len() - 1);
            crate::core::mpv::play_video_entries_at(ctx, entries, idx);
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
    pub(super) fn handle_video_action(
        &mut self,
        event: &mut ActionEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
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
                // Keyboard-opened menus stay centered (Round 46).
                CommonAction::Confirm => self.open_context_menu(ctx, None),
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

    /// Round 32: the wheel scrolls the video list's viewport only — the
    /// highlight stays put and may leave the visible area. The offset
    /// clamps at the list's ends.
    pub(super) fn video_scroll_viewport(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        crate::ui::widgets::virtualized_list::scroll_viewport(
            &mut self.video_state,
            dir,
            ctx.config.scroll_amount.max(1),
            self.video_items_len,
            self.areas[Areas::Table].height as usize,
        );
        ctx.render()?;
        Ok(())
    }

    /// Move the video list highlight by `dir` rows (clamped). The first
    /// move from no selection highlights the first entry (menu convention).
    /// The list renders from its offset, so the move also scrolls the
    /// selection back into view (the wheel only scrolls the viewport).
    pub(super) fn video_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.video_items_len;
        if len == 0 {
            return Ok(());
        }
        let Some(current) = self.video_state.selected() else {
            self.video_state.select(Some(0));
            crate::ui::widgets::virtualized_list::scroll_selection_into_view(
                &mut self.video_state,
                len,
                self.areas[Areas::Table].height as usize,
                ctx.config.scrolloff,
            );
            ctx.render()?;
            return Ok(());
        };
        let new = ((current as i64) + dir).clamp(0, len as i64 - 1) as usize;
        if new != current {
            self.video_state.select(Some(new));
            crate::ui::widgets::virtualized_list::scroll_selection_into_view(
                &mut self.video_state,
                len,
                self.areas[Areas::Table].height as usize,
                ctx.config.scrolloff,
            );
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

    /// Highlight the playlist entry at `idx` (clamped to the list) and
    /// scroll it into view (keyboard Home/End; the wheel only scrolls the
    /// viewport).
    pub(super) fn video_jump(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let len = self.video_items_len;
        if len == 0 {
            return Ok(());
        }
        let idx = idx.min(len - 1);
        if self.video_state.selected() != Some(idx) {
            self.video_state.select(Some(idx));
            crate::ui::widgets::virtualized_list::scroll_selection_into_view(
                &mut self.video_state,
                len,
                self.areas[Areas::Table].height as usize,
                ctx.config.scrolloff,
            );
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

/// The two-column marker before a Video row's title.
///
/// A row a paste inserted before its link was resolved
/// ([`MpvPlaylistEntry::pending`]) shows the resolving spinner there: the
/// title/URL/duration are on their way, and the row is already selectable (its
/// thumbnail and description resolve from the link in the meantime). The
/// playing entry keeps the `❯` marker.
fn video_row_prefix(entry: &MpvPlaylistEntry, is_current: bool) -> String {
    if entry.pending {
        format!("{} ", super::render::spinner_frame())
    } else if is_current {
        "❯ ".to_owned()
    } else {
        "  ".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::core::mpv::MpvPlaylistEntry;
    use crate::shared::ytdlp::YtStreamInfo;
    use crate::ui::modals::paste::{video_entry_duration, video_queue_total_time};

    fn ctx() -> crate::ctx::Ctx {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        Box::leak(Box::new(client.1.clone()));
        crate::tests::fixtures::ctx((app.0, app.1), (work.0, work.1), (client.0, client.1))
    }

    const LINK: &str = "https://www.youtube.com/watch?v=abc123";

    /// A row inserted by a paste before `yt-dlp` answered renders with the
    /// resolving spinner instead of the `❯`/blank marker — the visible half of
    /// "the queue populates immediately, the details follow".
    #[test]
    fn a_pending_row_shows_the_resolving_spinner() {
        let mut entry = MpvPlaylistEntry::pending_link(LINK);
        assert_eq!(
            super::video_row_prefix(&entry, false),
            format!("{} ", crate::ui::panes::queue::render::spinner_frame()),
            "a resolving row is marked with the spinner"
        );
        // A pending row is not "playing" even when it holds the index.
        assert_ne!(
            super::video_row_prefix(&entry, true),
            "❯ ",
            "the spinner wins over the marker"
        );
        entry.pending = false;
        assert_eq!(super::video_row_prefix(&entry, true), "❯ ");
        assert_eq!(super::video_row_prefix(&entry, false), "  ");
    }

    /// The reported bug: five queued YouTube videos read `0:00 total time`
    /// while the Duration cells that had been resolved showed real values.
    /// The entry carries no duration of its own — nothing probes a stream
    /// before playback — so the footer must fall back to the resolved info
    /// the audio queue already reads.
    #[test]
    fn a_stream_entry_without_its_own_duration_takes_the_cached_one() {
        let ctx = ctx();
        ctx.yt_info.borrow_mut().insert(
            LINK.to_string(),
            YtStreamInfo {
                title: "A video".to_string(),
                duration: Some(180.0),
                ..Default::default()
            },
        );
        let entry = MpvPlaylistEntry::new("A video", LINK, None);
        assert_eq!(video_entry_duration(&ctx, &entry), Some(180.0));
    }

    /// The cell and the footer are the same lookup, so they cannot disagree
    /// about one row — the invariant round 111 established for audio.
    #[test]
    fn the_total_is_the_sum_of_the_durations_the_rows_show() {
        let ctx = ctx();
        ctx.yt_info.borrow_mut().insert(
            LINK.to_string(),
            YtStreamInfo {
                title: "A video".to_string(),
                duration: Some(180.0),
                ..Default::default()
            },
        );
        let entries = vec![
            MpvPlaylistEntry::new("A video", LINK, None),
            MpvPlaylistEntry::new("local.mp4", "/videos/local.mp4", Some(120.0)),
        ];
        assert_eq!(video_queue_total_time(&ctx, &entries), Duration::from_secs(300));
    }

    /// An entry whose `original_url` holds the canonical link but whose `url`
    /// is the expired resolved stream URL must still be timed — that is the
    /// persisted shape mpv's playlist export and the session restore produce.
    #[test]
    fn the_original_link_times_an_entry_whose_url_is_the_resolved_stream() {
        let ctx = ctx();
        ctx.yt_info.borrow_mut().insert(
            LINK.to_string(),
            YtStreamInfo {
                title: "A video".to_string(),
                duration: Some(42.0),
                ..Default::default()
            },
        );
        let mut entry = MpvPlaylistEntry::new(
            "A video",
            "https://rr3.googlevideo.com/videoplayback?expire=1",
            None,
        );
        entry.original_url = Some(LINK.to_string());
        assert_eq!(video_entry_duration(&ctx, &entry), Some(42.0));
    }

    /// An entry that already knows its duration keeps it: the fallback is a
    /// fallback, so local files and Jellyfin items are untouched and nothing
    /// is counted twice.
    #[test]
    fn a_known_duration_wins_over_the_cache() {
        let ctx = ctx();
        ctx.yt_info.borrow_mut().insert(
            LINK.to_string(),
            YtStreamInfo {
                title: "A video".to_string(),
                duration: Some(180.0),
                ..Default::default()
            },
        );
        let entry = MpvPlaylistEntry::new("A video", LINK, Some(7.0));
        assert_eq!(video_entry_duration(&ctx, &entry), Some(7.0));
    }

    /// Before a stream's resolve lands nothing is known, so the honest answer
    /// is zero rather than a fabricated number — and no panic.
    #[test]
    fn an_unresolved_stream_contributes_zero() {
        let ctx = ctx();
        let entries = vec![MpvPlaylistEntry::new("A video", LINK, None)];
        assert_eq!(video_queue_total_time(&ctx, &entries), Duration::ZERO);
    }
}

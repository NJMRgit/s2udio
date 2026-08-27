use std::collections::HashSet;
use anyhow::Result;
use enum_map::{Enum, EnumMap, enum_map};
use itertools::Itertools;
use ratatui::{
    Frame, layout::Flex, prelude::{Constraint, Layout, Rect},
    style::Style, text::{Line, Span},
    widgets::{Block, Borders, ListState, Row, TableState},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{
            CommonAction, DirectoriesActions, GlobalAction, QueueActions,
            actions::{AddKind, AutoplayKind},
        },
        theme::{AlbumSeparator, properties::{Property, SongProperty}},
    },
    core::command::{create_env, run_external},
    ctx::Ctx,
    mpd::{
        QueuePosition, client::Client, commands::{Song, State},
        mpd_client::{MpdClient, ValueChange},
    },
    shared::{
        ext::{btreeset_ranges::BTreeSetRanges, rect::RectExt},
        keys::ActionEvent, macros::{modal, status_error, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt},
        song_ext::SongsExt,
    },
    ui::{
        UiEvent, dirstack::{Dir, MarkState},
        input::InputResultEvent,
        modals::{
            confirm_modal::{Action, ConfirmModal},
            info_list_modal::InfoListModal, menu::create_add_modal,
            select_modal::SelectModal,
        },
        panes::queue_header::QueueHeaderPane, song_list::SongListCore,
        widgets::virtualized_table::VirtualizedTable,
    },
};
mod chapters;
mod context_menus;
mod video;
#[derive(Debug)]
pub struct QueuePane {
    queue: Dir<Song, TableState>,
    column_widths: Vec<Constraint>,
    column_formats: Vec<Property<SongProperty>>,
    areas: EnumMap<Areas, Rect>,
    should_center_cursor_on_current: bool,
    /// The app-start jump is one-shot: the first `before_show` lands the
    /// highlight on the currently playing song (scrolled to the top of the
    /// visible list); later shows keep the user's selection.
    startup_jump_done: bool,
    /// Scroll state of the chapter list (Chapters mode).
    chapters_state: ListState,
    chapters_items_len: usize,
    /// Scroll state of the mpv playlist (Video mode).
    video_state: ListState,
    video_items_len: usize,
    /// Marked (multi-selected) entries of the mpv playlist (Video mode),
    /// with the ctrl/alt-click + shift+up/down selection of the audio
    /// queue list.
    video_marked: MarkState,
    /// Drag state of the Video-list scrollbar (thumb follows the pointer).
    video_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
    /// Drag state of the Chapters-list scrollbar (thumb follows the
    /// pointer).
    chapters_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
    /// Rubber-band (drag-rect) selection state of the Video list (Round
    /// 46): armed by a left press, updated by `Drag` events, finalized by
    /// `LeftRelease`.
    video_band: crate::ui::band::BandState,
    /// Click areas of the Audio / Video / Chapters toggle.
    pub(crate) toggle_areas: [Rect; 3],
}
#[derive(Debug, Enum)]
enum Areas {
    Table,
    Scrollbar,
    FilterArea,
}
const ADD_TO_PLAYLIST: &str = "add_to_playlist";
const ADD_TO_PLAYLIST_MULTIPLE: &str = "add_to_playlist_multiple";
/// Result id of a local file's chapter markers (from ffprobe).
pub const FILE_CHAPTERS: &str = "file_chapters";
/// Width (in cells) of the Time / Duration columns in the chapters table.
/// The chapters table uses its own columns — Chapter (flexible) | Time
/// (centered) | Duration (right-aligned at the right edge, matching the
/// queue list's Duration column) — shared with the QueueHeaderPane's
/// chapters header so the labels and values line up.
pub(crate) const CHAPTER_TIME_COL: u16 = 10;
pub(crate) const CHAPTER_DURATION_COL: u16 = 10;
/// Whether `file` is a resolved YouTube stream whose signed URL has
/// expired. googlevideo `videoplayback` URLs carry an `expire` epoch; once
/// it passes, MPD cannot open the stream (the YouTube video itself may
/// still exist — only the signed link died).
fn resolved_stream_expired(file: &str) -> bool {
    if !file.contains("googlevideo.com") || !file.contains("videoplayback") {
        return false;
    }
    let Some(expire) = file
        .split("expire=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .and_then(|value| value.parse::<i64>().ok()) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    expire < now
}
/// Play a queue song. A resolved YouTube stream whose signed URL expired
/// cannot be played as-is: when the cached info still knows its original
/// link, the link is re-resolved and the dead entry replaced in place
/// ([`YtAction::ReplaceAndPlay`]); otherwise the failure is explained
/// instead of failing silently in MPD.
fn play_queue_song(song: &crate::mpd::commands::Song, ctx: &Ctx) {
    if resolved_stream_expired(&song.file) {
        let original = ctx
            .yt_info
            .borrow()
            .get(&song.file)
            .and_then(|yt| {
                let url = yt.original_url.clone();
                (!url.is_empty() && url != song.file).then_some(url)
            });
        if let Some(original) = original {
            let id = song.id;
            let _ = ctx
                .work_sender
                .send(crate::shared::events::WorkRequest::ResolveYtStreams {
                    urls: vec![original],
                    action: crate::ui::modals::paste::YtAction::ReplaceAndPlay(id),
                })
                .map_err(|err| {
                    log::error!(error:? = err; "Failed to request stream re-resolution")
                });
            status_info!("Stream URL expired — re-resolving from the original link");
            return;
        }
        status_warn!(
            "This queue entry's stream URL expired; add the original link again"
        );
    }
    let id = song.id;
    ctx.command(move |client| {
        client.play_id(id)?;
        Ok(())
    });
}
impl QueuePane {
    /// Drop the multi-selected (marked) set, e.g. after the context-menu
    /// Remove deleted the items.
    pub(crate) fn clear_marked(&mut self) {
        self.queue.marked_mut().clear();
        self.queue.state.clear_mark_anchor();
        self.video_marked.clear();
        self.video_marked.clear_anchor();
    }
    /// The queue the pane shows. Radio stations and Jellyfin audio streams
    /// are played through a temporary MPD playlist entry (required to play
    /// a stream) and filtered out here so they never show up in the Queue
    /// tab; the same applies to files played from Directories with the
    /// right arrow / double-click and the paste popup's "Play (don't add
    /// to queue)" — their temporary entry is hidden until it is dropped.
    /// Resolved YouTube-style streams are **queue content** (added via the
    /// paste popup's Add/Append): they stay visible, keyed by their URL in
    /// the yt-info cache.
    fn local_queue(ctx: &Ctx) -> Vec<Song> {
        let temp_play = ctx.temp_play_id.get();
        ctx.queue
            .iter()
            .filter(|song| {
                let hidden_stream = crate::ui::panes::radio::is_stream_url(&song.file)
                    && !ctx.yt_info.borrow().contains_key(&song.file);
                !hidden_stream && Some(song.id) != temp_play
            })
            .cloned()
            .collect()
    }
    /// Whether the current song has chapter markers (shows the
    /// Audio / Video / Chapters toggle).
    fn chapters_available(ctx: &Ctx) -> bool {
        ctx.has_current_chapters()
    }
    /// Chapter markers of the current playback (mpv video or queue song).
    fn current_chapters(ctx: &Ctx) -> Vec<crate::shared::chapters::Chapter> {
        ctx.current_playback_chapters()
    }
    /// Switch the Queue tab's list to `mode`, resetting the list
    /// highlights and landing the Chapters/Video highlight on the currently
    /// playing item.
    fn set_tab(&mut self, ctx: &Ctx, mode: crate::ctx::QueueTabMode) {
        if ctx.queue_tab.get() == mode {
            return;
        }
        ctx.queue_tab.set(mode);
        self.chapters_state = ListState::default();
        self.video_state = ListState::default();
        self.video_marked.clear();
        match mode {
            crate::ctx::QueueTabMode::Chapters => self.chapters_select_current(ctx),
            crate::ctx::QueueTabMode::Video => {
                let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
                let playlist: std::cell::Ref<
                    '_,
                    Vec<crate::core::mpv::MpvPlaylistEntry>,
                > = if jellyfin {
                    ctx.mpv.playlist.borrow()
                } else {
                    ctx.video_playlist.borrow()
                };
                let current = if jellyfin {
                    ctx.mpv.playlist_pos.get()
                } else {
                    crate::core::mpv::video_playlist_current_idx(ctx)
                };
                if let Some(idx) = current.filter(|i| *i < playlist.len()) {
                    self.video_state.select(Some(idx));
                } else if !playlist.is_empty() {
                    self.video_state.select(Some(0));
                }
            }
            crate::ctx::QueueTabMode::Audio => {}
        }
    }
    /// Cycle the list view: Audio -> Video -> Chapters -> Audio (Chapters
    /// only when the track has markers).
    fn cycle_tab(&mut self, ctx: &Ctx) {
        let next = match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Audio => crate::ctx::QueueTabMode::Video,
            crate::ctx::QueueTabMode::Video => {
                if Self::chapters_available(ctx) {
                    crate::ctx::QueueTabMode::Chapters
                } else {
                    crate::ctx::QueueTabMode::Audio
                }
            }
            crate::ctx::QueueTabMode::Chapters => crate::ctx::QueueTabMode::Audio,
        };
        Self::set_tab(self, ctx, next);
    }
    pub fn new(ctx: &Ctx) -> Self {
        let (column_widths, column_formats) = Self::init(ctx);
        Self {
            queue: Dir::new(Self::local_queue(ctx)),
            column_widths,
            column_formats,
            areas: enum_map! {
                _ => Rect::default(),
            },
            should_center_cursor_on_current: ctx.config.center_current_song_on_change,
            startup_jump_done: false,
            chapters_state: ListState::default(),
            chapters_items_len: 0,
            video_state: ListState::default(),
            video_items_len: 0,
            video_marked: MarkState::default(),
            video_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            chapters_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            video_band: crate::ui::band::BandState::default(),
            toggle_areas: [Rect::default(); 3],
        }
    }
    pub fn init(ctx: &Ctx) -> (Vec<Constraint>, Vec<Property<SongProperty>>) {
        (
            ctx
                .config
                .theme
                .song_table_format
                .iter()
                .map(|v| v.width.into_constraint(0))
                .collect_vec(),
            ctx
                .config
                .theme
                .song_table_format
                .iter()
                .map(|v| v.prop.clone())
                .collect_vec(),
        )
    }
    fn enqueue_items(&self, all: bool) -> (Vec<Enqueue>, Option<usize>) {
        let hovered = self.queue.selected().map(|s| s.file.as_str());
        self.items(all)
            .fold(
                (Vec::new(), None),
                |mut acc, (idx, song)| {
                    let path = song.file.clone();
                    if hovered.as_ref().is_some_and(|hovered| hovered == &path) {
                        acc.1 = Some(idx);
                    }
                    acc.0.push(Enqueue::File { path });
                    acc
                },
            )
    }
    fn items<'a>(
        &'a self,
        all: bool,
    ) -> Box<dyn Iterator<Item = (usize, &'a Song)> + 'a> {
        if all {
            Box::new(self.queue.items.iter().enumerate())
        } else if self.queue.marked().is_empty() {
            if let Some((idx, item)) = self.queue.selected_with_idx() {
                Box::new(std::iter::once((idx, item)))
            } else {
                Box::new(std::iter::empty::<(usize, &Song)>())
            }
        } else {
            Box::new(
                self.queue.marked().iter().map(|idx| (*idx, &self.queue.items[*idx])),
            )
        }
    }
}
impl QueuePane {
    /// The `● Audio ○ Video ○ Chapters` toggle, drawn on the row above the
    /// box that contains the queue list (the config reserves a 1-row spacer
    /// there). TabScreen calls this after the box block renders; the active
    /// tab gets the filled dot and clicking a segment switches the mode (`c`
    /// keybind cycles). The ●/○ glyphs are both single-width so the row
    /// never shifts between modes. Audio and Video always show; Chapters
    /// appears when the current track has markers.
    pub(crate) fn render_toggle_on_border(
        &mut self,
        frame: &mut Frame,
        _pane_borders: Borders,
        block_area: Rect,
        ctx: &Ctx,
    ) {
        self.toggle_areas = [Rect::default(); 3];
        if block_area.height == 0 || block_area.y < 2 {
            return;
        }
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters
            && !Self::chapters_available(ctx)
        {
            ctx.queue_tab.set(crate::ctx::QueueTabMode::Audio);
        }
        let corner_x = block_area.x.saturating_sub(2);
        let border_y = {
            let buf = frame.buffer_mut();
            (1..block_area.y)
                .rev()
                .find(|&row| is_box_corner_glyph(buf[(corner_x, row)].symbol()))
        };
        let Some(border_y) = border_y else { return };
        let y = border_y.saturating_sub(1);
        let active = ctx.queue_tab.get();
        let chapters_visible = Self::chapters_available(ctx);
        let mut segments = vec![
            crate ::ui::widgets::sub_tab_bar::Segment { label : "Audio", active : active
            == crate ::ctx::QueueTabMode::Audio, }, crate
            ::ui::widgets::sub_tab_bar::Segment { label : "Video", active : active ==
            crate ::ctx::QueueTabMode::Video, },
        ];
        if chapters_visible {
            segments
                .push(crate::ui::widgets::sub_tab_bar::Segment {
                    label: "Chapters",
                    active: active == crate::ctx::QueueTabMode::Chapters,
                });
        }
        let right = block_area.right().saturating_sub(1);
        let bar = crate::ui::widgets::sub_tab_bar::SubTabBar::new(
            &segments,
            corner_x + 1,
            y,
            right,
        );
        for (idx, area) in bar.render(frame, ctx).into_iter().enumerate() {
            self.toggle_areas[idx] = area;
        }
    }
}
impl Pane for QueuePane {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &Ctx,
    ) -> anyhow::Result<()> {
        let Ctx { config, .. } = ctx;
        self.calculate_areas(area, ctx)?;
        match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Video => return self.render_video(frame, ctx),
            crate::ctx::QueueTabMode::Chapters if Self::chapters_available(ctx) => {
                return self.render_chapters(frame, ctx);
            }
            _ => {}
        }
        let filter_text = self.queue.filter_text(self.areas[Areas::Table].width, ctx);
        let table_block = {
            let border_style = config.as_border_style();
            let mut b = Block::default().border_style(border_style);
            if self.areas[Areas::FilterArea].height == 0
                && let Some(ref title) = filter_text
            {
                b = b.title(title.clone());
            }
            b
        };
        self.queue
            .state
            .set_content_and_viewport_len(
                self.queue.len(),
                self.areas[Areas::Table].height as usize,
            );
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            self.areas[Areas::Table],
            self.queue.state.offset(),
            self.queue.len(),
            1,
        );
        let row_highlight = if hover_idx == self.queue.state.get_selected() {
            config.theme.hovered_item_style
        } else {
            config.theme.current_item_style
        };
        let widths = Layout::horizontal(self.column_widths.as_slice())
            .flex(Flex::Start)
            .spacing(1)
            .split(self.areas[Areas::Table]);
        let formats = &config.theme.song_table_format;
        let new_album_indices: HashSet<usize> = self
            .queue
            .items
            .as_slice()
            .to_album_ranges()
            .map(|range| range.end.saturating_sub(1))
            .collect();
        let current_song_id = ctx.find_current_song_in_queue().map(|(_, song)| song.id);
        let marked = std::mem::take(self.queue.marked_mut());
        let filter = ctx.input.value(self.queue.filter_buffer_id);
        let table = VirtualizedTable::new(&self.queue.items)
            .column_widths(self.column_widths.clone())
            .row_highlight_style(row_highlight)
            .map_fn(|idx, song| {
                let is_current = current_song_id.is_some_and(|v| v == song.id);
                let is_marked = marked.contains(&idx);
                let is_hovered = hover_idx == Some(idx);
                let yt = ctx.yt_info.borrow().get(&song.file).cloned();
                let columns = (0..formats.len())
                    .map(|i| {
                        let mut max_len: usize = widths[i].width.into();
                        let marker = (is_current && i == 0)
                            .then(|| {
                                max_len = max_len.saturating_sub(2);
                                Span::styled("❯ ", Style::default())
                            });
                        let mut line = if let Some(yt) = &yt {
                            stream_column_line(
                                    &formats[i].prop,
                                    yt,
                                    max_len,
                                    &config.theme.symbols,
                                )
                                .unwrap_or_else(|| {
                                    song.as_line_ellipsized(
                                            &formats[i].prop,
                                            max_len,
                                            &config.theme.symbols,
                                            &config.theme.format_tag_separator,
                                            config.theme.multiple_tag_resolution_strategy,
                                            ctx,
                                        )
                                        .unwrap_or_default()
                                })
                        } else {
                            song.as_line_ellipsized(
                                    &formats[i].prop,
                                    max_len,
                                    &config.theme.symbols,
                                    &config.theme.format_tag_separator,
                                    config.theme.multiple_tag_resolution_strategy,
                                    ctx,
                                )
                                .unwrap_or_default()
                        };
                        if let Some(marker) = marker {
                            let mut spans = Vec::with_capacity(line.spans.len() + 1);
                            spans.push(marker);
                            spans.extend(line.spans);
                            line = Line::from(spans);
                        }
                        line.alignment(formats[i].alignment.into())
                    });
                let is_matching_search = is_current
                    || if self.queue.filter_active {
                        song.matches(self.column_formats.as_slice(), &filter, ctx)
                    } else {
                        Default::default()
                    };
                let mut row = QueueRow::default();
                if is_marked {
                    row.cell_style = Some(config.theme.marked_item_style);
                } else if is_hovered {
                    row.cell_style = Some(config.theme.hovered_item_style);
                } else if is_matching_search {
                    row.cell_style = Some(config.theme.highlighted_item_style);
                }
                let sep = ctx.config.theme.song_table_album_separator;
                if new_album_indices.contains(&idx)
                    && matches!(sep, AlbumSeparator::Underline)
                    && idx != self.queue.items.len().saturating_sub(1)
                {
                    row.underlined = true;
                }
                row.into_row(columns)
            });
        frame.render_widget(table_block, self.areas[Areas::Table]);
        frame
            .render_stateful_widget(
                table,
                self.areas[Areas::Table],
                &mut self.queue.state,
            );
        let _ = std::mem::replace(self.queue.marked_mut(), marked);
        ctx.queue_selected_id.set(self.queue.selected().map(|s| s.id));
        if let Some(scrollbar) = config.as_styled_scrollbar()
            && self.areas[Areas::Scrollbar].width > 0
        {
            frame
                .render_stateful_widget(
                    scrollbar,
                    self.areas[Areas::Scrollbar],
                    self.queue.state.as_scrollbar_state_ref(),
                );
        }
        if let Some(filter_text) = filter_text
            && self.areas[Areas::FilterArea].height > 0
        {
            frame
                .render_widget(
                    Line::from(filter_text)
                        .style(
                            config
                                .theme
                                .text_color
                                .map(|c| Style::default().fg(c))
                                .unwrap_or_default(),
                        ),
                    self.areas[Areas::FilterArea],
                );
        }
        Ok(())
    }
    fn calculate_areas(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        let Ctx { config, .. } = ctx;
        let scrollbar_area_width: u16 = config.theme.scrollbar.is_some().into();
        let [table_area, scrollbar_area] = Layout::horizontal([
                Constraint::Percentage(100),
                Constraint::Length(scrollbar_area_width),
            ])
            .areas(area);
        let mut table_area = if self.queue.filter_active {
            self.areas[Areas::FilterArea] = Rect::new(
                table_area.x,
                table_area.y,
                table_area.width,
                1,
            );
            table_area.shrink_from_top(1)
        } else {
            self.areas[Areas::FilterArea] = Rect::default();
            table_area
        };
        table_area.width = table_area.width.saturating_sub(1);
        self.areas[Areas::Table] = table_area;
        self.areas[Areas::Scrollbar] = scrollbar_area;
        ctx.queue_table_width.set(Some(table_area.width));
        Ok(())
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        self.queue
            .state
            .set_content_and_viewport_len(
                self.queue.len(),
                self.areas[Areas::Table].height as usize,
            );
        if !self.startup_jump_done {
            self.startup_jump_done = true;
            self.should_center_cursor_on_current = false;
            let to_select = ctx
                .find_current_song_in_queue()
                .map(|(idx, _)| idx)
                .unwrap_or(0);
            self.queue.select_at_top(to_select);
        } else if self.should_center_cursor_on_current {
            let to_select = ctx
                .find_current_song_in_queue()
                .or(self.queue.selected_with_idx())
                .map(|(idx, _)| idx)
                .or(Some(0));
            self.queue.select_idx_opt(to_select, usize::MAX);
            self.should_center_cursor_on_current = false;
        } else if self
            .queue
            .selected_with_idx()
            .is_none_or(|(sel, _)| sel >= self.queue.items.len())
        {
            let to_select = ctx
                .find_current_song_in_queue()
                .map(|(idx, _)| idx)
                .or(Some(0));
            self.queue.select_idx_opt(to_select, ctx.config.scrolloff);
        }
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters {
            self.chapters_select_current(ctx);
        }
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Video
            && let Some(idx) = if crate::core::mpv::session_playlist_shown(ctx) {
                ctx.mpv.playlist_pos.get()
            } else {
                crate::core::mpv::video_playlist_current_idx(ctx)
            }
                .filter(|i| {
                    let len = if crate::core::mpv::session_playlist_shown(ctx) {
                        ctx.mpv.playlist.borrow().len()
                    } else {
                        ctx.video_playlist.borrow().len()
                    };
                    *i < len
                })
        {
            self.video_state.select(Some(idx));
            crate::ui::widgets::virtualized_list::scroll_selection_into_view(
                &mut self.video_state,
                self.video_items_len,
                self.areas[Areas::Table].height as usize,
                ctx.config.scrolloff,
            );
        }
        Ok(())
    }
    fn resize(&mut self, _area: Rect, ctx: &Ctx) -> Result<()> {
        self.queue
            .state
            .set_content_and_viewport_len(
                self.queue.len(),
                self.areas[Areas::Table].height as usize,
            );
        let to_select = self
            .queue
            .selected_with_idx()
            .or(ctx.find_current_song_in_queue())
            .map(|v| v.0)
            .or(Some(0));
        self.queue.select_idx_opt(to_select, ctx.config.scrolloff);
        ctx.render()?;
        Ok(())
    }
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match event {
            UiEvent::Database => {
                self.queue.filter_active = false;
                self.queue.items.clone_from(&Self::local_queue(ctx));
                self.queue.unmark_all();
            }
            UiEvent::QueueChanged => {
                self.queue.items.clone_from(&Self::local_queue(ctx));
            }
            UiEvent::SongChanged => {
                if let Some((idx, _)) = ctx.find_current_song_in_queue()
                    && ctx.config.select_current_song_on_change
                {
                    match (is_visible, ctx.config.center_current_song_on_change) {
                        (true, true) => {
                            self.queue.select_idx(idx, usize::MAX);
                        }
                        (false, true) => {
                            self.queue.select_idx(idx, usize::MAX);
                            self.should_center_cursor_on_current = true;
                        }
                        (true, false) | (false, false) => {
                            self.queue.select_idx(idx, ctx.config.scrolloff);
                        }
                    }
                    ctx.render()?;
                }
            }
            UiEvent::Reconnected => {
                self.before_show(ctx)?;
            }
            UiEvent::ConfigChanged => {
                let (column_widths, column_formats) = Self::init(ctx);
                self.column_formats = column_formats;
                self.column_widths = column_widths;
            }
            _ => {}
        }
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position = event.into();
        if matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick)
            && self.toggle_areas.iter().any(|area| area.contains(position))
        {
            let mode = if self.toggle_areas[1].contains(position) {
                crate::ctx::QueueTabMode::Video
            } else if self.toggle_areas[2].contains(position) {
                crate::ctx::QueueTabMode::Chapters
            } else {
                crate::ctx::QueueTabMode::Audio
            };
            self.queue.state.band.cancel();
            self.video_band.cancel();
            Self::set_tab(self, ctx, mode);
            ctx.render()?;
            return Ok(());
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
                    MouseEventKind::DoubleClick => {
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
                    MouseEventKind::LeftClick if event
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            if self.video_marked.is_empty() {
                                if let Some(sel) = self.video_state.selected() {
                                    self.video_marked.add(sel);
                                }
                            }
                            self.video_marked.add(idx);
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
            };
            let drag = match mode {
                crate::ctx::QueueTabMode::Video => &mut self.video_scrollbar_drag,
                crate::ctx::QueueTabMode::Chapters => &mut self.chapters_scrollbar_drag,
                crate::ctx::QueueTabMode::Audio => &mut self.queue.state.scrollbar_drag,
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
                        self.queue.state.scroll_to(perc, ctx.config.scrolloff);
                    }
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
                    _ => self.queue_band_drag(event, ctx),
                };
            }
            MouseEventKind::LeftRelease if self.band_active_for_mode(ctx) => {
                return match ctx.queue_tab.get() {
                    crate::ctx::QueueTabMode::Video => self.video_band_release(ctx),
                    _ => self.queue_band_release(ctx),
                };
            }
            _ => {}
        }
        if !self.areas[Areas::Table].contains(position) {
            return Ok(());
        }
        match event.kind {
            MouseEventKind::LeftClick if self.areas[Areas::Table].contains(event.into())
                && event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                let clicked_row: usize = event
                    .y
                    .saturating_sub(self.areas[Areas::Table].y)
                    .into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    if self.queue.state.marked.is_empty() {
                        if let Some(sel) = self.queue.state.get_selected() {
                            self.queue.state.mark(sel);
                        }
                    }
                    self.queue.state.mark(idx);
                    // Arm the band so a ctrl+drag from here adds a range.
                    self.queue.state.band.arm(idx, false);
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
    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: MpdQueryResult,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match (id, data) {
            (FILE_CHAPTERS, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any
                    .downcast::<
                        (String, Result<Vec<crate::shared::chapters::Chapter>, String>),
                    >()
                {
                    let (file, result) = *boxed;
                    if let Ok(chapters) = result && !chapters.is_empty() {
                        ctx.chapters.borrow_mut().insert(file, chapters);
                        ctx.auto_show_chapters();
                    }
                    ctx.render()?;
                }
            }
            (
                ADD_TO_PLAYLIST,
                MpdQueryResult::AddToPlaylist { playlists, song_file },
            ) => {
                modal!(
                    ctx, SelectModal::builder().ctx(ctx).options(playlists)
                    .confirm_label("Add").title("Select a playlist").on_confirm(move |
                    ctx, selected, _idx | { let song_file = song_file.clone(); ctx
                    .command(move | client | { if song_file.starts_with('/') { client
                    .add_to_playlist(& selected, & format!("file://{song_file}"), None,)
                    ?; } else { client.add_to_playlist(& selected, & song_file, None) ?;
                    } status_info!("Song added to playlist {}", selected); Ok(()) });
                    Ok(()) }).build()
                );
            }
            (
                ADD_TO_PLAYLIST_MULTIPLE,
                MpdQueryResult::AddToPlaylistMultiple { playlists, song_files },
            ) => {
                modal!(
                    ctx, SelectModal::builder().ctx(ctx).options(playlists)
                    .confirm_label("Add").title("Select a playlist").on_confirm(move |
                    ctx, selected, _idx | { ctx.command(move | client | { let songs_len =
                    song_files.len(); for song_file in song_files { if song_file
                    .starts_with('/') { client.add_to_playlist(& selected, &
                    format!("file://{song_file}"), None,) ?; } else { client
                    .add_to_playlist(& selected, & song_file, None) ?; } }
                    status_info!("{} songs added to playlist {}", songs_len, selected);
                    Ok(()) }); Ok(()) }).build()
                );
            }
            _ => {}
        }
        Ok(())
    }
    fn handle_insert_mode(
        &mut self,
        kind: InputResultEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        match kind {
            InputResultEvent::Push => {
                self.queue
                    .recalculate_matched_items(self.column_formats.as_slice(), ctx);
                self.queue.jump_first_matching(self.column_formats.as_slice(), ctx);
            }
            InputResultEvent::Pop => {
                self.queue
                    .recalculate_matched_items(self.column_formats.as_slice(), ctx);
            }
            InputResultEvent::Confirm => {}
            InputResultEvent::Cancel => {
                self.queue.set_filter_active(false);
                ctx.input.clear_buffer(self.queue.filter_buffer_id);
            }
            InputResultEvent::NoChange => {}
        }
        ctx.render()?;
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Chapters if Self::chapters_available(ctx) => {
                return self.handle_chapters_action(event, ctx);
            }
            crate::ctx::QueueTabMode::Video => {
                return self.handle_video_action(event, ctx);
            }
            _ => {}
        }
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp => {
                    if !self.queue.is_empty() {
                        self.queue
                            .prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    }
                    ctx.render()?;
                    Ok(())
                }
                DirectoriesActions::FolderDown => {
                    if !self.queue.is_empty() {
                        self.queue
                            .next(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    }
                    ctx.render()?;
                    Ok(())
                }
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if let Some(selected_song) = self.queue.selected() {
                        play_queue_song(selected_song, ctx);
                    }
                    Ok(())
                }
                DirectoriesActions::FolderCollapse => Ok(()),
            };
        }
        if let Some(action) = event.claim_queue() {
            match action {
                QueueActions::Delete if !self.queue.marked().is_empty() => {
                    for range in self.queue.marked().ranges().rev() {
                        ctx.command(move |client| {
                            client.delete_from_queue(range.into())?;
                            Ok(())
                        });
                    }
                    self.queue.marked_mut().clear();
                    self.queue.state.clear_mark_anchor();
                    status_info!("Marked songs removed from queue");
                    ctx.render()?;
                }
                QueueActions::Delete => {
                    if let Some(selected_song) = self.queue.selected() {
                        let id = selected_song.id;
                        ctx.command(move |client| {
                            client.delete_id(id)?;
                            Ok(())
                        });
                    } else {
                        status_error!("No song selected");
                    }
                }
                QueueActions::DeleteAll => {
                    modal!(
                        ctx, ConfirmModal::builder().ctx(ctx)
                        .message(vec!["Are you sure you want to clear the queue?",
                        "This action cannot be undone."]).action(Action::Single {
                        on_confirm : Box::new(| ctx | { ctx.command(| client | Ok(client
                        .clear() ?)); Ok(()) }), confirm_label : Some("Clear"),
                        cancel_label : None, }).size((45, 6)).build()
                    );
                }
                QueueActions::Play => {
                    if let Some(selected_song) = self.queue.selected() {
                        play_queue_song(selected_song, ctx);
                    }
                }
                QueueActions::ToggleChapters => {
                    self.cycle_tab(ctx);
                    ctx.render()?;
                }
                QueueActions::JumpToCurrent => {
                    if let Some((idx, _)) = ctx
                        .status
                        .songid
                        .and_then(|id| {
                            self.queue
                                .items
                                .iter()
                                .enumerate()
                                .find(|(_, song)| song.id == id)
                        })
                    {
                        let scrolloff = if self
                            .queue
                            .selected_with_idx()
                            .is_some_and(|(i, _)| i == idx)
                        {
                            usize::MAX
                        } else {
                            ctx.config.scrolloff
                        };
                        self.queue.select_idx(idx, scrolloff);
                        ctx.render()?;
                    } else {
                        status_info!("No song is currently playing");
                    }
                }
                QueueActions::Shuffle if !self.queue.marked().is_empty() => {
                    for range in self.queue.marked().ranges().rev() {
                        ctx.command(move |client| {
                            client.shuffle(Some(range.into()))?;
                            Ok(())
                        });
                    }
                    status_info!("Shuffled selected songs");
                }
                QueueActions::Shuffle => {
                    ctx.command(move |client| {
                        client.shuffle(None)?;
                        Ok(())
                    });
                    status_info!("Shuffled the queue");
                }
                QueueActions::SortByColumn(idx) => {
                    QueueHeaderPane::sort_by_column(
                        self.column_formats.as_slice(),
                        *idx,
                        ctx,
                    )?;
                    ctx.render()?;
                }
                QueueActions::Unused => {}
            }
        } else if let Some(action) = event.claim_common().map(|v| v.to_owned()) {
            match action {
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
                    return Ok(());
                }
                CommonAction::MoveUp if !self.queue.marked().is_empty() => {
                    if self.queue.is_empty() {
                        return Ok(());
                    }
                    if let Some(0) = self.queue.marked().first() {
                        return Ok(());
                    }
                    let ranges = self.queue.marked().ranges().collect_vec();
                    for range in ranges {
                        for idx in range.clone() {
                            let new_idx = idx.saturating_sub(1);
                            self.queue.items.swap(idx, new_idx);
                        }
                        let new_start_idx = range.start().saturating_sub(1);
                        ctx.command(move |client| {
                            client
                                .move_in_queue(
                                    range.into(),
                                    QueuePosition::Absolute(new_start_idx),
                                )?;
                            Ok(())
                        });
                    }
                    if let Some(start) = self.queue.marked().first() {
                        let new_idx = start.saturating_sub(1);
                        self.queue.select_idx(new_idx, ctx.config.scrolloff);
                    }
                    let mut new_marked = self
                        .queue
                        .marked()
                        .iter()
                        .map(|i| i.saturating_sub(1))
                        .collect();
                    std::mem::swap(self.queue.marked_mut(), &mut new_marked);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::MoveDown if !self.queue.marked().is_empty() => {
                    if self.queue.is_empty() {
                        return Ok(());
                    }
                    if let Some(last_idx) = self.queue.marked().last()
                        && *last_idx == self.queue.len() - 1
                    {
                        return Ok(());
                    }
                    let ranges = self.queue.marked().ranges().rev().collect_vec();
                    for range in ranges {
                        for idx in range.clone().rev() {
                            let new_idx = idx.saturating_add(1);
                            self.queue.items.swap(idx, new_idx);
                        }
                        let new_start_idx = range.start().saturating_add(1);
                        ctx.command(move |client| {
                            client
                                .move_in_queue(
                                    range.into(),
                                    QueuePosition::Absolute(new_start_idx),
                                )?;
                            Ok(())
                        });
                    }
                    if let Some(start) = self.queue.marked().last() {
                        let new_idx = start.saturating_add(1);
                        self.queue.select_idx(new_idx, ctx.config.scrolloff);
                    }
                    let mut new_marked = self
                        .queue
                        .marked()
                        .iter()
                        .map(|i| i.saturating_add(1))
                        .collect();
                    std::mem::swap(self.queue.marked_mut(), &mut new_marked);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::MoveUp => {
                    if self.queue.is_empty() {
                        return Ok(());
                    }
                    let Some((idx, selected)) = self.queue.selected_with_idx() else {
                        return Ok(());
                    };
                    let new_idx = idx.saturating_sub(1);
                    let id = selected.id;
                    ctx.command(move |client| {
                        client.move_id(id, QueuePosition::Absolute(new_idx))?;
                        Ok(())
                    });
                    self.queue.select_idx(new_idx, ctx.config.scrolloff);
                    self.queue.items.swap(idx, new_idx);
                    ctx.render()?;
                }
                CommonAction::MoveDown => {
                    if self.queue.is_empty() {
                        return Ok(());
                    }
                    let Some((idx, selected)) = self.queue.selected_with_idx() else {
                        return Ok(());
                    };
                    let new_idx = (idx + 1).min(self.queue.len() - 1);
                    let id = selected.id;
                    ctx.command(move |client| {
                        client.move_id(id, QueuePosition::Absolute(new_idx))?;
                        Ok(())
                    });
                    self.queue.select_idx(new_idx, ctx.config.scrolloff);
                    self.queue.items.swap(idx, new_idx);
                    ctx.render()?;
                }
                CommonAction::Delete => {
                    if !self.queue.marked().is_empty() {
                        for range in self.queue.marked().ranges().rev() {
                            ctx.command(move |client| {
                                client.delete_from_queue(range.into())?;
                                Ok(())
                            });
                        }
                        self.queue.marked_mut().clear();
                        self.queue.state.clear_mark_anchor();
                        status_info!("Marked songs removed from queue");
                    } else if let Some(selected_song) = self.queue.selected() {
                        let id = selected_song.id;
                        ctx.command(move |client| {
                            client.delete_id(id)?;
                            Ok(())
                        });
                    } else {
                        status_error!("No song selected");
                    }
                    ctx.render()?;
                }
                CommonAction::AddOptions { kind: AddKind::Action(options) } => {
                    let (enqueue, _hovered_song_idx) = self.enqueue_items(options.all);
                    if !enqueue.is_empty() {
                        Client::resolve_and_enqueue(
                            ctx,
                            enqueue,
                            options.position,
                            AutoplayKind::None,
                            None,
                            None,
                        );
                        self.queue.marked_mut().clear();
                    }
                }
                CommonAction::AddOptions { kind: AddKind::Modal(items) } => {
                    let opts = items
                        .into_iter()
                        .map(|(label, mut opts)| {
                            opts.autoplay = AutoplayKind::None;
                            let (enqueue, hovered_song_idx) = self
                                .enqueue_items(opts.all);
                            (label, opts, (enqueue, hovered_song_idx))
                        })
                        .collect_vec();
                    modal!(ctx, create_add_modal(opts, ctx));
                    self.queue.marked_mut().clear();
                }
                CommonAction::ShowInfo => {
                    if let Some(selected_song) = self.queue.selected() {
                        modal!(
                            ctx, InfoListModal::builder().rows(selected_song)
                            .title("Song info").column_widths(& [30, 70]).build()
                        );
                    } else {
                        status_error!("No song selected");
                    }
                }
                CommonAction::Confirm => {
                    self.open_context_menu(ctx, None);
                }
                CommonAction::ContextMenu => {
                    self.open_context_menu(ctx, None);
                }
                CommonAction::Right | CommonAction::Left => {}
                other => self.handle_claimed_common_action(other, event, ctx)?,
            }
        } else if let Some(action) = event.claim_global() {
            match action {
                GlobalAction::PreviousTrack => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.seek_current(ValueChange::Decrease(5))?;
                            Ok(())
                        });
                    }
                    return Ok(());
                }
                GlobalAction::NextTrack => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.seek_current(ValueChange::Increase(5))?;
                            Ok(())
                        });
                    }
                    return Ok(());
                }
                GlobalAction::ExternalCommand { command, .. } => {
                    let songs = create_env(
                        ctx,
                        self.items(false).map(|(_, song)| song.file.as_str()),
                    );
                    run_external(command.clone(), songs);
                }
                _ => {
                    event.abandon();
                }
            }
        }
        Ok(())
    }
}
impl SongListCore<Song, TableState> for QueuePane {
    fn list(&self) -> &Dir<Song, TableState> {
        &self.queue
    }
    fn list_mut(&mut self) -> &mut Dir<Song, TableState> {
        &mut self.queue
    }
    fn list_songs_in_item(
        &self,
        item: Song,
    ) -> impl FnOnce(
        &mut Client<'_>,
    ) -> Result<Vec<Song>> + Send + Sync + Clone + 'static {
        move |_client| Ok(vec![item])
    }
    /// The queue filter jump-matching uses the queue's own column formats
    /// (not the generic browser song format).
    fn song_format(&self, _ctx: &Ctx) -> Vec<Property<SongProperty>> {
        self.column_formats.clone()
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
    /// Rubber-band drag of the audio queue table: plain drag replaces
    /// every mark with the anchor→current range, ctrl+drag adds/contracts
    /// the range keeping the other marks. The row clamps to the visible
    /// list (band capture).
    fn queue_band_drag(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if !self.queue.state.band.is_active() {
            return Ok(());
        }
        let control = event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
        let table = self.areas[Areas::Table];
        let (offset, len) = (self.queue.state.offset(), self.queue.len());
        let Some(current) =
            crate::ui::band::band_current_row(event.y, table, offset, len, 1)
        else {
            return Ok(());
        };
        let state = &mut self.queue.state;
        let anchor = state.band.anchor.unwrap_or(current);
        state.band.update(current);
        if control {
            if let Some((lo, hi)) = state.take_range_mark() {
                for i in lo..=hi {
                    state.marked.remove(&i);
                }
            }
        } else {
            state.unmark_all();
        }
        let (lo, hi) = (anchor.min(current), anchor.max(current));
        if lo <= hi {
            state.mark_range(lo, hi);
        }
        if lo < hi {
            state.set_range_mark(lo, hi);
        } else {
            state.clear_range_mark();
        }
        ctx.render()?;
        Ok(())
    }
    /// Resolve the audio queue's armed band on release: a press that never
    /// moved is a plain click (deferred unmark applies when it landed on
    /// a different row); a moved press finalizes with the band's marks in
    /// place.
    fn queue_band_release(&mut self, ctx: &Ctx) -> Result<()> {
        let clear_marks = {
            let state = &mut self.queue.state;
            match state.band.release() {
                crate::ui::band::BandEnd::Click { clear_marks } => clear_marks,
                _ => false,
            }
        };
        if clear_marks {
            self.queue.state.unmark_all();
        }
        ctx.render()?;
        Ok(())
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
/// Truncate `s` so its display width fits `max_cols`, keeping whole
/// graphemes (wide glyphs take two columns, so a grapheme count is not
/// enough to keep the following columns in place).
fn truncate_to_width(s: &mut String, max_cols: usize) {
    if s.width() <= max_cols {
        return;
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in s.graphemes(true) {
        let w = grapheme.width();
        if used + w > max_cols {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    *s = out;
}
/// The queue-table cell of a resolved YouTube-style stream for the Title /
/// Album / Artist columns: the cached info (title in Title + Album,
/// channel in Artist — matching the MPRIS tags), ellipsized to the column
/// width. `None` for the other columns (duration …), which render normally.
fn stream_column_line(
    prop: &Property<SongProperty>,
    yt: &crate::shared::ytdlp::YtStreamInfo,
    max_len: usize,
    symbols: &crate::config::theme::SymbolsConfig,
) -> Option<Line<'static>> {
    use crate::config::theme::properties::{PropertyKindOrText, SongProperty};
    let text = match &prop.kind {
        PropertyKindOrText::Property(SongProperty::Title) => yt.title.clone(),
        PropertyKindOrText::Property(SongProperty::Album) => yt.title.clone(),
        PropertyKindOrText::Property(SongProperty::Artist) => {
            yt.channel.clone().unwrap_or_default()
        }
        _ => return None,
    };
    let mut text = text;
    if text.width() > max_len {
        let mut out = String::new();
        let mut used = 0;
        let budget = max_len.saturating_sub(symbols.ellipsis.width());
        for grapheme in text.graphemes(true) {
            let w = grapheme.width();
            if used + w > budget {
                break;
            }
            out.push_str(grapheme);
            used += w;
        }
        out.push_str(&symbols.ellipsis);
        text = out;
    }
    Some(Line::from(Span::styled(text, prop.style.unwrap_or_default())))
}
/// Whether a cell holds a box's top-left corner glyph (any of the ratatui
/// border sets), used to locate the box the queue/chapters toggle sits above.
fn is_box_corner_glyph(symbol: &str) -> bool {
    matches!(symbol, "╭" | "┌" | "╒" | "╔" | "╓" | "╥")
}
#[derive(Default)]
struct QueueRow {
    cell_style: Option<Style>,
    underlined: bool,
}
impl QueueRow {
    fn into_row<'a>(self, cells: impl Iterator<Item = Line<'a>>) -> Row<'a> {
        let mut row = if let Some(style) = self.cell_style {
            Row::new(cells.map(|column| column.patch_style(style))).style(style)
        } else {
            Row::new(cells)
        };
        if self.underlined {
            row = row.style(self.cell_style.unwrap_or_default().underlined());
        }
        row
    }
}

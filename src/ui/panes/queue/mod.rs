use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{
            CommonAction, DirectoriesActions, GlobalAction, QueueActions,
            actions::{AddKind, AutoplayKind},
        },
        theme::properties::{Property, SongProperty},
    },
    core::command::{create_env, run_external},
    ctx::Ctx,
    mpd::{
        QueuePosition,
        client::Client,
        commands::{Song, State},
        mpd_client::{MpdClient, ValueChange},
    },
    shared::{
        ext::btreeset_ranges::BTreeSetRanges,
        keys::ActionEvent,
        macros::{modal, status_error, status_info},
        mouse_event::MouseEvent,
        mpd_client_ext::{Enqueue, MpdClientExt},
    },
    ui::{
        UiEvent,
        dirstack::{Dir, MarkState},
        input::InputResultEvent,
        modals::{
            confirm_modal::{Action, ConfirmModal},
            info_list_modal::{INFO_COLUMN_WIDTHS, InfoListModal, song_info},
            menu::create_add_modal,
            select_modal::SelectModal,
        },
        panes::queue_header::QueueHeaderPane,
        song_list::SongListCore,
    },
};
use anyhow::Result;
use enum_map::{Enum, EnumMap, enum_map};
use itertools::Itertools;
use ratatui::{
    Frame,
    prelude::{Constraint, Rect},
    widgets::{ListState, TableState},
};

mod chapters;
mod context_menus;
mod follow;
mod mouse_events;
mod play;
mod render;
mod video;

#[allow(unused_imports)]
pub(crate) use play::play_from_stop;
use play::play_queue_song;
#[allow(unused_imports)]
pub(crate) use play::resolved_stream_expired;
pub(crate) use play::{issue_next_if_still_target, next_track_resolving};
#[allow(unused_imports)]
use render::truncate_to_width;
#[allow(unused_imports)]
pub(crate) use render::{SPINNER_FRAME_MS, SPINNER_FRAMES, spinner_frame};

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
    /// The Chapters view was opened before the track's markers were known: the
    /// highlight lands on the playing chapter as soon as they arrive (a stream
    /// that just started has no chapters for the first seconds).
    chapters_selection_pending: bool,
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
    pub(crate) toggle_areas: [Rect; 4],
    /// Round 95: the redraw tick that keeps the Duration column's spinner
    /// moving while a queued stream resolve is in flight. Armed only while
    /// such a row is on screen, so the animation costs nothing otherwise.
    loading_tick: Option<crate::shared::id::Id>,
    /// Round 111: the queue row the selection-time metadata warm was last
    /// scheduled for. The warm is driven from the render pass — the one
    /// place every selection route funnels through (`w`/`s`, the arrows, a
    /// mouse click, the wheel, a scrollbar drag, a queue restore) — so this
    /// records the id it was scheduled for, and a render that did not
    /// actually move the selection does not restart the debounce forever.
    warmed_selection_id: Option<u32>,
    /// The selection follows the newly playing track, standing down while the
    /// user is reading the queue (see [`follow::FollowGate`]).
    follow: follow::FollowGate,
    /// The track the follow decision was last made for, so a repeated
    /// `SongChanged` for the same track (a re-render, a resume, a second
    /// status update) does not re-run the jump and yank a selection the user
    /// has since moved.
    followed_song_id: Option<u32>,
    /// A follow-the-track jump that landed while this pane was off screen,
    /// where the viewport height is stale. Re-applied by the next
    /// `before_show`, which knows the real area.
    pending_follow_to_top: Option<usize>,
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

impl QueuePane {
    /// Drop the multi-selected (marked) set, e.g. after the context-menu
    /// Remove deleted the items.
    pub(crate) fn clear_marked(&mut self) {
        self.queue.marked_mut().clear();
        self.queue.state.clear_mark_anchor();
        self.video_marked.clear();
        self.video_marked.clear_anchor();
    }
    /// Record that the user just moved the Audio list's selection or scrolled
    /// it, so the automatic "follow the new track" jump stands down while they
    /// are reading the queue ([`follow::FollowGate`]). Every user-driven route
    /// calls this: the movement keys, the mouse wheel, a click or right-click
    /// on a row, and a scrollbar drag. Typing elsewhere in the app does not
    /// count as navigation.
    pub(super) fn note_user_navigation(&mut self) {
        self.follow.note_user_navigation(std::time::Instant::now());
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
                // Round 91: a pasted stream link is queue content from the
                // moment it is queued, before any stream exists - the yt-info
                // cache only knows it once it has been played.
                let tagged_link = crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some();
                let hidden_stream = crate::ui::panes::radio::is_stream_url(&song.file)
                    && !tagged_link
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
    /// The Queue tab follows MPD playback after an mpv video session handed
    /// over (music started, which pauses the video): the Video list inherited
    /// from the video session has nothing behind it any more — mpv is not the
    /// UI source and its playlist is empty — so the tab switches to Chapters
    /// (when the current song has markers and auto-chapters is on) or the audio
    /// queue. Called **once, at the handover** (not per frame): a mode the user
    /// picked with `c` or a click must stay exactly as chosen (round 78
    /// follow-up 8 — cycling to the empty Video list has to work).
    pub(crate) fn follow_mpd_playback(&mut self, ctx: &Ctx) {
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            return;
        }
        let video_len = if crate::core::mpv::session_playlist_shown(ctx) {
            ctx.mpv.playlist.borrow().len()
        } else {
            ctx.video_playlist.borrow().len()
        };
        match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Video if video_len == 0 => {
                let mode = if ctx.config.ui.auto_show_chapters && Self::chapters_available(ctx) {
                    crate::ctx::QueueTabMode::Chapters
                } else {
                    crate::ctx::QueueTabMode::Audio
                };
                Self::set_tab(self, ctx, mode);
            }
            _ => {}
        }
    }

    /// The current song lost its chapter markers (a stream without chapters
    /// replaced a chaptered one): a leftover Chapters view falls back to the
    /// audio list. Called on a song change — a mode the user picked while
    /// markers existed is never overridden by anything else.
    fn follow_mpd_chapters(&mut self, ctx: &Ctx) {
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters
            && !Self::chapters_available(ctx)
        {
            Self::set_tab(self, ctx, crate::ctx::QueueTabMode::Audio);
        }
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
            crate::ctx::QueueTabMode::Chapters => {
                // Markers already known: land the highlight now. Otherwise the
                // render does it the moment they arrive.
                self.chapters_selection_pending = !Self::chapters_available(ctx);
                self.chapters_select_current(ctx);
            }
            crate::ctx::QueueTabMode::Video => {
                let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
                let playlist: std::cell::Ref<'_, Vec<crate::core::mpv::MpvPlaylistEntry>> =
                    if jellyfin { ctx.mpv.playlist.borrow() } else { ctx.video_playlist.borrow() };
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
            crate::ctx::QueueTabMode::Radio => {
                // No list state to reset — the radio browser pane renders
                // the whole body in Radio mode (Round 62 Q2).
            }
        }
    }
    /// Cycle the list view: Audio -> Video -> Chapters -> Audio (Chapters
    /// only when the track has markers).
    fn cycle_tab(&mut self, ctx: &Ctx) {
        // Round 62 (Q2): Audio -> Video -> Chapters (when the track has
        // markers) -> Radio -> Audio, matching the toggle's visual order.
        let next = match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Audio => crate::ctx::QueueTabMode::Video,
            crate::ctx::QueueTabMode::Video => {
                if Self::chapters_available(ctx) {
                    crate::ctx::QueueTabMode::Chapters
                } else {
                    crate::ctx::QueueTabMode::Radio
                }
            }
            crate::ctx::QueueTabMode::Chapters => crate::ctx::QueueTabMode::Radio,
            crate::ctx::QueueTabMode::Radio => crate::ctx::QueueTabMode::Audio,
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
            chapters_selection_pending: false,
            chapters_items_len: 0,
            video_state: ListState::default(),
            video_items_len: 0,
            video_marked: MarkState::default(),
            video_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            chapters_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            video_band: crate::ui::band::BandState::default(),
            toggle_areas: [Rect::default(); 4],
            loading_tick: None,
            warmed_selection_id: None,
            follow: follow::FollowGate::default(),
            followed_song_id: None,
            pending_follow_to_top: None,
        }
    }
    pub fn init(ctx: &Ctx) -> (Vec<Constraint>, Vec<Property<SongProperty>>) {
        (
            ctx.config
                .theme
                .song_table_format
                .iter()
                .map(|v| v.width.into_constraint(0))
                .collect_vec(),
            ctx.config.theme.song_table_format.iter().map(|v| v.prop.clone()).collect_vec(),
        )
    }
    fn enqueue_items(&self, all: bool) -> (Vec<Enqueue>, Option<usize>) {
        let hovered = self.queue.selected().map(|s| s.file.as_str());
        self.items(all).fold((Vec::new(), None), |mut acc, (idx, song)| {
            let path = song.file.clone();
            if hovered.as_ref().is_some_and(|hovered| hovered == &path) {
                acc.1 = Some(idx);
            }
            acc.0.push(Enqueue::File { path });
            acc
        })
    }
    fn items<'a>(&'a self, all: bool) -> Box<dyn Iterator<Item = (usize, &'a Song)> + 'a> {
        if all {
            Box::new(self.queue.items.iter().enumerate())
        } else if self.queue.marked().is_empty() {
            if let Some((idx, item)) = self.queue.selected_with_idx() {
                Box::new(std::iter::once((idx, item)))
            } else {
                Box::new(std::iter::empty::<(usize, &Song)>())
            }
        } else {
            Box::new(self.queue.marked().iter().map(|idx| (*idx, &self.queue.items[*idx])))
        }
    }
}

impl Pane for QueuePane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> anyhow::Result<()> {
        QueuePane::render(self, frame, area, ctx)
    }
    fn calculate_areas(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        QueuePane::calculate_areas(self, area, ctx)
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        QueuePane::handle_mouse_event(self, event, ctx)
    }
    fn on_global_mouse_release(&mut self, ctx: &Ctx) -> Result<()> {
        QueuePane::on_global_mouse_release(self, ctx)
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        self.queue.state.set_content_and_viewport_len(
            self.queue.len(),
            self.areas[Areas::Table].height as usize,
        );
        if !self.startup_jump_done {
            self.startup_jump_done = true;
            self.should_center_cursor_on_current = false;
            let to_select = ctx.find_current_song_in_queue().map(|(idx, _)| idx).unwrap_or(0);
            self.queue.select_at_top(to_select);
        } else if self.should_center_cursor_on_current {
            let to_select = ctx
                .find_current_song_in_queue()
                .or(self.queue.selected_with_idx())
                .map(|(idx, _)| idx)
                .or(Some(0));
            self.queue.select_idx_opt(to_select, usize::MAX);
            self.should_center_cursor_on_current = false;
        } else if let Some(idx) = self.pending_follow_to_top.take() {
            // A follow-the-track jump landed while this pane was off screen,
            // where the viewport height is stale. Now that the real area is
            // known, place the row at the top of the visible list.
            self.queue.select_at_top(idx);
        } else if self
            .queue
            .selected_with_idx()
            .is_none_or(|(sel, _)| sel >= self.queue.items.len())
        {
            let to_select = ctx.find_current_song_in_queue().map(|(idx, _)| idx).or(Some(0));
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
        self.queue.state.set_content_and_viewport_len(
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
    fn on_event(&mut self, event: &mut UiEvent, is_visible: bool, ctx: &Ctx) -> Result<()> {
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
                // A track without markers replaced a chaptered one: the
                // Chapters view falls back to the audio list.
                self.follow_mpd_chapters(ctx);
                if let Some((idx, song)) = ctx.find_current_song_in_queue() {
                    // One decision per track change: a repeated SongChanged
                    // for the same track (a re-render, a resume, a second
                    // status update) must not re-run the jump and yank a
                    // selection the user has since moved.
                    let new_track = self.followed_song_id != Some(song.id);
                    if new_track {
                        self.followed_song_id = Some(song.id);
                        self.follow.begin_track_change();
                    }
                    if new_track
                        && ctx.config.select_current_song_on_change
                        && self.follow.should_follow(std::time::Instant::now())
                    {
                        let to_select = idx;
                        match (is_visible, ctx.config.center_current_song_on_change) {
                            // The default: land the new track at the **top**
                            // of the visible list, as close as the viewport
                            // allows — watching the queue then shows the
                            // playing track with the upcoming rows under it.
                            (true, false) => self.queue.select_at_top(to_select),
                            (false, false) => {
                                self.queue.select_at_top(to_select);
                                // The pane is off screen, so the viewport
                                // height is stale; re-apply on the next show.
                                self.pending_follow_to_top = Some(to_select);
                            }
                            // The older centering placement, kept for anyone
                            // who prefers it.
                            (true, true) => {
                                self.queue.select_idx(to_select, usize::MAX);
                            }
                            (false, true) => {
                                self.queue.select_idx(to_select, usize::MAX);
                                self.should_center_cursor_on_current = true;
                            }
                        }
                        ctx.render()?;
                    }
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
                    .downcast::<(String, Result<Vec<crate::shared::chapters::Chapter>, String>)>()
                {
                    let (file, result) = *boxed;
                    if let Ok(chapters) = result
                        && !chapters.is_empty()
                    {
                        ctx.chapters.borrow_mut().insert(file, chapters);
                        ctx.auto_show_chapters();
                    }
                    ctx.render()?;
                }
            }
            (ADD_TO_PLAYLIST, MpdQueryResult::AddToPlaylist { playlists, song_file }) => {
                modal!(
                    ctx,
                    SelectModal::builder()
                        .ctx(ctx)
                        .options(playlists)
                        .confirm_label("Add")
                        .title("Select a playlist")
                        .on_confirm(move |ctx, selected, _idx| {
                            let song_file = song_file.clone();
                            ctx.command(move |client| {
                                if song_file.starts_with('/') {
                                    client.add_to_playlist(
                                        &selected,
                                        &format!("file://{song_file}"),
                                        None,
                                    )?;
                                } else {
                                    client.add_to_playlist(&selected, &song_file, None)?;
                                }
                                status_info!("Song added to playlist {}", selected);
                                Ok(())
                            });
                            Ok(())
                        })
                        .build()
                );
            }
            (
                ADD_TO_PLAYLIST_MULTIPLE,
                MpdQueryResult::AddToPlaylistMultiple { playlists, song_files },
            ) => {
                modal!(
                    ctx,
                    SelectModal::builder()
                        .ctx(ctx)
                        .options(playlists)
                        .confirm_label("Add")
                        .title("Select a playlist")
                        .on_confirm(move |ctx, selected, _idx| {
                            ctx.command(move |client| {
                                let songs_len = song_files.len();
                                for song_file in song_files {
                                    if song_file.starts_with('/') {
                                        client.add_to_playlist(
                                            &selected,
                                            &format!("file://{song_file}"),
                                            None,
                                        )?;
                                    } else {
                                        client.add_to_playlist(&selected, &song_file, None)?;
                                    }
                                }
                                status_info!("{} songs added to playlist {}", songs_len, selected);
                                Ok(())
                            });
                            Ok(())
                        })
                        .build()
                );
            }
            _ => {}
        }
        Ok(())
    }
    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        match kind {
            InputResultEvent::Push => {
                self.queue.recalculate_matched_items(self.column_formats.as_slice(), ctx);
                self.queue.jump_first_matching(self.column_formats.as_slice(), ctx);
            }
            InputResultEvent::Pop => {
                self.queue.recalculate_matched_items(self.column_formats.as_slice(), ctx);
            }
            InputResultEvent::Confirm => {}
            InputResultEvent::Cancel => {
                self.queue.set_filter_active(false);
                ctx.input.clear_buffer(self.queue.filter_buffer_id);
            }
            InputResultEvent::NoChange => {}
            InputResultEvent::AtStart => {}
            InputResultEvent::CursorLeft => {}
        }
        ctx.render()?;
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        // Round 96: `S` (Shift+S, `GlobalAction::LibrarySearch`) opens the
        // YouTube search popup on the Queue tab — on library tabs the same
        // key jumps to that library's search page, and on the Queue tab it
        // was a no-op before this. Handled before the Audio/Video/Chapters
        // dispatch so it works from all three lists.
        if let Some(action) = event.claim_global() {
            if matches!(action, GlobalAction::LibrarySearch) {
                modal!(ctx, crate::ui::modals::yt_search::YtSearchModal::new(ctx));
                return Ok(());
            }
            event.abandon();
        }
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
                        self.note_user_navigation();
                        self.queue.prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    }
                    ctx.render()?;
                    Ok(())
                }
                DirectoriesActions::FolderDown => {
                    if !self.queue.is_empty() {
                        self.note_user_navigation();
                        self.queue.next(ctx.config.scrolloff, ctx.config.wrap_navigation);
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
                        ctx,
                        ConfirmModal::builder()
                            .ctx(ctx)
                            .message(vec![
                                "Are you sure you want to clear the queue?",
                                "This action cannot be undone."
                            ])
                            .action(Action::Single {
                                on_confirm: Box::new(|ctx| {
                                    ctx.command(|client| Ok(client.clear()?));
                                    Ok(())
                                }),
                                confirm_label: Some("Clear"),
                                cancel_label: None,
                            })
                            .size((45, 6))
                            .build()
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
                    if let Some((idx, _)) = ctx.status.songid.and_then(|id| {
                        self.queue.items.iter().enumerate().find(|(_, song)| song.id == id)
                    }) {
                        let scrolloff =
                            if self.queue.selected_with_idx().is_some_and(|(i, _)| i == idx) {
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
                    QueueHeaderPane::sort_by_column(self.column_formats.as_slice(), *idx, ctx)?;
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
                            client.move_in_queue(
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
                    let mut new_marked =
                        self.queue.marked().iter().map(|i| i.saturating_sub(1)).collect();
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
                            client.move_in_queue(
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
                    let mut new_marked =
                        self.queue.marked().iter().map(|i| i.saturating_add(1)).collect();
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
                            let (enqueue, hovered_song_idx) = self.enqueue_items(opts.all);
                            (label, opts, (enqueue, hovered_song_idx))
                        })
                        .collect_vec();
                    modal!(ctx, create_add_modal(opts, ctx));
                    self.queue.marked_mut().clear();
                }
                CommonAction::ShowInfo => {
                    if let Some(selected_song) = self.queue.selected() {
                        // Round 89: the detailed panel (metadata, audio,
                        // file, queue, stream, chapters, lyrics, stickers).
                        modal!(
                            ctx,
                            InfoListModal::builder()
                                .rows(song_info(ctx, selected_song))
                                .title("Song info")
                                .column_widths(INFO_COLUMN_WIDTHS)
                                .build()
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
                other => {
                    // The generic movement/selection arms (Up/Down, PageUp/
                    // PageDown, Top/Bottom, half-page, SelectUp/Down) live in
                    // `SongListCore`; note the navigation before delegating so
                    // every one of them counts as user activity and stands the
                    // follow-the-track jump down.
                    if matches!(
                        other,
                        CommonAction::Up
                            | CommonAction::Down
                            | CommonAction::PageUp
                            | CommonAction::PageDown
                            | CommonAction::UpHalf
                            | CommonAction::DownHalf
                            | CommonAction::Top
                            | CommonAction::Bottom
                            | CommonAction::Select
                            | CommonAction::SelectUp
                            | CommonAction::SelectDown
                    ) {
                        self.note_user_navigation();
                    }
                    self.handle_claimed_common_action(other, event, ctx)?
                }
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
                    let songs =
                        create_env(ctx, self.items(false).map(|(_, song)| song.file.as_str()));
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
    fn list_area(&self) -> Option<Rect> {
        Some(self.areas[Areas::Table])
    }
    /// Round 111: the selection-time metadata warm is driven from the render
    /// pass (see [`schedule_selection_warm`] and `render.rs`) because this
    /// hook is only reached by `SongListCore`'s `CommonAction` arms, which
    /// the Queue pane's own `w`/`s` and mouse paths never claim.
    fn fetch_data_internal(&mut self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }
    fn list_songs_in_item(
        &self,
        item: Song,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Send + Sync + Clone + 'static {
        move |_client| Ok(vec![item])
    }
    /// The queue filter jump-matching uses the queue's own column formats
    /// (not the generic browser song format).
    fn song_format(&self, _ctx: &Ctx) -> Vec<Property<SongProperty>> {
        self.column_formats.clone()
    }
}

/// Round 107: fire the selection-time metadata warm after the selection has
/// settled. A long scroll would otherwise run yt-dlp once per row;
/// `schedule_replace` with one fixed job id restarts the timer on each
/// change and only the resting selection resolves.
///
/// Round 111: called from the render pass (`render.rs`) on the selected id
/// changing, not from `SongListCore::fetch_data_internal`. The Queue pane
/// claims `DirectoriesActions::FolderUp/FolderDown` itself — which is what
/// `w`/`s` are bound to — and its mouse paths call `select_idx` directly, so
/// neither ever reached `fetch_data_internal` and the warm never ran at all
/// on this tab (the info box and album art stayed empty for a selected
/// stream row). The render pass sees every route.
pub(super) fn schedule_selection_warm(ctx: &Ctx) {
    use std::sync::OnceLock;
    static WARM_JOB: OnceLock<crate::shared::id::Id> = OnceLock::new();
    let id = *WARM_JOB.get_or_init(crate::shared::id::new);
    ctx.scheduler.schedule_replace(
        id,
        std::time::Duration::from_millis(400),
        |(events, _)| {
            let _ = events.send(crate::AppEvent::WarmQueueSelection);
            Ok(())
        },
    );
}
